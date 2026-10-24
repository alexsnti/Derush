//! CLI de Derush : sert à régler l'analyse sur de vrais rushes et à déboguer l'export,
//! sans passer par l'interface.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use derush_core::{analysis, render, retakes, text, transcribe, Project};

#[derive(Parser)]
#[command(name = "derush", about = "Dérushage automatique de vidéos YouTube")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Analyse des rushes et crée un projet avec les silences coupés.
    Analyze {
        #[arg(required = true)]
        inputs: Vec<PathBuf>,
        #[arg(short, long, default_value = "projet.json")]
        output: PathBuf,
        /// Seuil fixe en dBFS (sinon adaptatif).
        #[arg(long)]
        threshold_db: Option<f32>,
        /// Durée minimale d'un blanc coupé, en secondes.
        #[arg(long, default_value_t = 0.6)]
        min_silence: f64,
        /// Marge gardée autour de la parole, en secondes.
        #[arg(long, default_value_t = 0.15)]
        padding: f64,
    },
    /// Applique une transcription (JSON : [{text,start,end}]) pour couper les prises refaites.
    Retakes {
        project: PathBuf,
        /// Identifiant du rush transcrit (ex. src-0).
        #[arg(long)]
        source: String,
        #[arg(long)]
        words: PathBuf,
    },
    /// Transcrit les rushes avec Whisper (télécharge le modèle au besoin).
    Transcribe {
        project: PathBuf,
        /// turbo (précis) ou small (rapide).
        #[arg(long, default_value = "turbo")]
        model: String,
        /// Vocabulaire à bien reconnaître.
        #[arg(long)]
        prompt: Option<String>,
        /// Coupe aussi les prises ratées détectées.
        #[arg(long)]
        cut_retakes: bool,
    },
    /// Écrit les sous-titres .srt de la vidéo montée.
    Srt {
        project: PathBuf,
        #[arg(short, long, default_value = "sous-titres.srt")]
        output: PathBuf,
    },
    /// Résumé du projet.
    Info { project: PathBuf },
    /// Affiche la commande et le graphe ffmpeg sans lancer l'export.
    Plan {
        project: PathBuf,
        #[arg(short, long, default_value = "sortie.mp4")]
        output: PathBuf,
    },
    /// Exporte le MP4 final.
    Render {
        project: PathBuf,
        #[arg(short, long, default_value = "sortie.mp4")]
        output: PathBuf,
    },
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Analyze { inputs, output, threshold_db, min_silence, padding } => {
            let params = analysis::SilenceParams { threshold_db, min_silence, padding, ..Default::default() };
            let mut sources = vec![];
            let mut segments = vec![];
            for (i, path) in inputs.iter().enumerate() {
                let a = analysis::analyze_file(path, &format!("src-{i}"), &params)
                    .with_context(|| format!("analyse de {}", path.display()))?;
                let cut: f64 = a.segments.iter().filter(|s| !s.keep).map(|s| s.len()).sum();
                eprintln!(
                    "{} : {:.1} s, seuil {:.1} dB, {} blancs coupés ({:.1} s)",
                    path.display(),
                    a.source.duration,
                    analysis::threshold(&a.levels, &params),
                    a.segments.iter().filter(|s| !s.keep).count(),
                    cut
                );
                sources.push(a.source);
                segments.extend(a.segments);
            }
            let project = Project::new(sources, segments);
            project.save(&output)?;
            eprintln!("→ {} (durée montée : {:.1} s)", output.display(), project.program_duration());
        }
        Cmd::Retakes { project, source, words } => {
            let mut p = Project::load(&project)?;
            let words: Vec<retakes::Word> = serde_json::from_str(&std::fs::read_to_string(&words)?)?;
            let found = retakes::find_retakes(&words, &Default::default());
            for r in &found {
                eprintln!("{:>7.2}–{:<7.2} {}  [{}]", r.start, r.end, r.text, r.reason);
            }
            retakes::apply(&mut p, &source, &found);
            p.save(&project)?;
            eprintln!("{} prises coupées, durée montée : {:.1} s", found.len(), p.program_duration());
        }
        Cmd::Transcribe { project, model, prompt, cut_retakes } => {
            let mut p = Project::load(&project)?;
            let model_path = match transcribe::installed_model(&model) {
                Some(m) => m,
                None => {
                    eprintln!("téléchargement du modèle {model}…");
                    let mut last = -1;
                    transcribe::download_model(&model, |x| {
                        let pct = (x * 100.0) as i32;
                        if pct / 10 != last / 10 {
                            eprintln!("  {pct} %");
                            last = pct;
                        }
                    })?
                }
            };
            let opts = transcribe::Options { model: model_path, language: Some("fr".into()), prompt };
            for src in p.sources.clone() {
                let started = std::time::Instant::now();
                eprintln!("{} : transcription…", src.path.display());
                let words = transcribe::transcribe_file(&src.path, &opts, |_| {}, Default::default())?;
                eprintln!(
                    "  {} mots en {:.0} s (rush de {:.0} s)",
                    words.len(),
                    started.elapsed().as_secs_f64(),
                    src.duration
                );
                if cut_retakes {
                    let found = retakes::find_retakes(&words, &Default::default());
                    for r in &found {
                        eprintln!("  prise coupée {:.1}–{:.1} : {}", r.start, r.end, r.text);
                    }
                    retakes::apply(&mut p, &src.id, &found);
                }
                p.transcripts.insert(src.id.clone(), words);
            }
            p.save(&project)?;
            for s in text::sentences(&p).iter().take(15) {
                println!("{:>7.1} {}", s.start, s.text);
            }
        }
        Cmd::Srt { project, output } => {
            let p = Project::load(&project)?;
            let srt = text::srt(&p, 0.0);
            std::fs::write(&output, &srt)?;
            eprintln!("{} sous-titres → {}", srt.matches(" --> ").count(), output.display());
        }
        Cmd::Info { project } => {
            let p = Project::load(&project)?;
            for s in &p.segments {
                println!(
                    "{:<8} {:<6} {:>8.2} → {:<8.2} {:<8} {}",
                    s.id,
                    s.source,
                    s.start,
                    s.end,
                    format!("{:?}", s.kind).to_lowercase(),
                    if s.keep { "gardé" } else { "coupé" }
                );
            }
            println!("durée montée : {:.1} s", p.program_duration());
        }
        Cmd::Plan { project, output } => {
            let job = render::build(&Project::load(&project)?, &output)?;
            println!("ffmpeg {}\n\n{}", job.args.join(" "), job.filter);
            for w in &job.warnings {
                eprintln!("attention : {w}");
            }
        }
        Cmd::Render { project, output } => {
            let job = render::build(&Project::load(&project)?, &output)?;
            for w in &job.warnings {
                eprintln!("attention : {w}");
            }
            let mut last = -1i32;
            render::run(&job, |p| {
                let pct = (p * 100.0) as i32;
                if pct / 5 != last / 5 {
                    eprintln!("export {pct} %");
                    last = pct;
                }
            })?;
            eprintln!("→ {}", output.display());
        }
    }
    Ok(())
}
