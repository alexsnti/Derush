//! Outils MCP. Chaque outil relit le projet, le modifie et le réenregistre (écriture atomique),
//! pour rester cohérent avec l'app qui travaille sur le même fichier.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use derush_core::analysis::{self, SilenceParams};
use derush_core::*;
use serde_json::{json, Value};

type R<T> = std::result::Result<T, String>;

// ------------------------------------------------------------------ contexte et tâches

#[derive(Clone)]
struct Job {
    progress: f64,
    state: JobState,
    detail: String,
    started: Instant,
}

#[derive(Clone, PartialEq)]
enum JobState {
    Running,
    Done(String),
    Failed(String),
}

#[derive(Clone)]
pub struct Ctx {
    default_project: Option<PathBuf>,
    /// Sérialise les lectures/écritures du projet faites par ce serveur (outils et tâches de fond).
    file_lock: Arc<Mutex<()>>,
    jobs: Arc<Mutex<BTreeMap<&'static str, Job>>>,
}

impl Ctx {
    pub fn new(default_project: Option<PathBuf>) -> Self {
        Self { default_project, file_lock: Arc::default(), jobs: Arc::default() }
    }

    fn project_path(&self, args: &Value) -> R<PathBuf> {
        if let Some(p) = args.get("project").and_then(Value::as_str) {
            return Ok(p.into());
        }
        self.default_project.clone().or_else(paths::current_project).ok_or_else(|| {
            "Aucun projet ouvert. Ouvre un projet dans Derush et enregistre-le (⌘S), ou crée-en un avec create_project."
                .to_string()
        })
    }

    fn read(&self, args: &Value) -> R<(PathBuf, Project)> {
        let path = self.project_path(args)?;
        let _g = self.file_lock.lock().map_err(err)?;
        let p = Project::load(&path).map_err(|e| format!("{} : {e}", path.display()))?;
        Ok((path, p))
    }

    /// Relit, modifie, valide, enregistre.
    fn edit<T>(&self, path: &Path, f: impl FnOnce(&mut Project) -> R<T>) -> R<(T, Project)> {
        let _g = self.file_lock.lock().map_err(err)?;
        let mut p = Project::load(path).map_err(err)?;
        let out = f(&mut p)?;
        p.validate().map_err(err)?;
        p.save(path).map_err(err)?;
        Ok((out, p))
    }

    fn edit_args<T>(&self, args: &Value, f: impl FnOnce(&mut Project) -> R<T>) -> R<(T, Project)> {
        let path = self.project_path(args)?;
        self.edit(&path, f)
    }

    fn set_job(&self, kind: &'static str, progress: f64, state: JobState, detail: impl Into<String>) {
        if let Ok(mut jobs) = self.jobs.lock() {
            let started = jobs.get(kind).filter(|j| j.state == JobState::Running).map_or_else(Instant::now, |j| j.started);
            jobs.insert(kind, Job { progress, state, detail: detail.into(), started });
        }
    }

    fn running(&self, kind: &str) -> bool {
        self.jobs.lock().map(|j| j.get(kind).is_some_and(|j| j.state == JobState::Running)).unwrap_or(false)
    }
}

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

// ------------------------------------------------------------------ utilitaires

fn t(x: f64) -> String {
    let x = x.max(0.0);
    let h = (x / 3600.0) as u64;
    let m = (x % 3600.0 / 60.0) as u64;
    let s = x % 60.0;
    if h > 0 {
        format!("{h}:{m:02}:{s:04.1}")
    } else {
        format!("{m}:{s:04.1}")
    }
}

fn name(path: &Path) -> String {
    path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned())
}

fn s<'a>(args: &'a Value, k: &str) -> Option<&'a str> {
    args.get(k).and_then(Value::as_str)
}
fn f(args: &Value, k: &str) -> Option<f64> {
    args.get(k).and_then(Value::as_f64)
}
fn req_s<'a>(args: &'a Value, k: &str) -> R<&'a str> {
    s(args, k).ok_or_else(|| format!("paramètre « {k} » manquant"))
}
fn req_f(args: &Value, k: &str) -> R<f64> {
    f(args, k).ok_or_else(|| format!("paramètre « {k} » manquant (nombre de secondes)"))
}

fn existing_file(p: &str) -> R<PathBuf> {
    let path = PathBuf::from(p);
    if path.is_file() {
        Ok(path)
    } else {
        Err(format!("fichier introuvable : {p}"))
    }
}

/// Instant d'ancrage : `program_time` (vidéo montée) ou `source` + `time` (rush).
fn anchor(p: &Project, args: &Value) -> R<Anchor> {
    if let Some(pt) = f(args, "program_time") {
        return p.program_to_source(pt).ok_or_else(|| {
            format!("program_time {} dépasse la durée du montage ({})", t(pt), t(p.program_duration()))
        });
    }
    let source = req_s(args, "source")?;
    let time = req_f(args, "time")?;
    let src = p.source(source).ok_or_else(|| format!("rush inconnu : {source}"))?;
    if !(0.0..=src.duration).contains(&time) {
        return Err(format!("{source} dure {} : time {} hors du rush", t(src.duration), t(time)));
    }
    Ok(Anchor { source: source.into(), time })
}

fn range(p: &Project, args: &Value) -> R<(String, f64, f64)> {
    let source = req_s(args, "source")?;
    let (a, b) = (req_f(args, "start")?, req_f(args, "end")?);
    let src = p.source(source).ok_or_else(|| format!("rush inconnu : {source}"))?;
    if !(b > a) {
        return Err("end doit être après start".into());
    }
    Ok((source.into(), a.max(0.0), b.min(src.duration)))
}

fn parse_style(v: Option<&str>) -> R<TitleStyle> {
    Ok(match v.unwrap_or("lower") {
        "lower" => TitleStyle::Lower,
        "center" => TitleStyle::Center,
        "big" => TitleStyle::Big,
        o => return Err(format!("style inconnu : {o} (lower, center, big)")),
    })
}

fn parse_position(v: Option<&str>) -> R<Position> {
    Ok(match v.unwrap_or("top-right") {
        "top-left" => Position::TopLeft,
        "top-right" => Position::TopRight,
        "bottom-left" => Position::BottomLeft,
        "bottom-right" => Position::BottomRight,
        "center" => Position::Center,
        o => return Err(format!("position inconnue : {o}")),
    })
}

fn kind_fr(k: SegmentKind) -> &'static str {
    match k {
        SegmentKind::Speech => "parole",
        SegmentKind::Silence => "blanc",
        SegmentKind::Retake => "prise ratée",
        SegmentKind::Manual => "coupé à la main",
    }
}

fn overlay_line(p: &Project, o: &Overlay) -> String {
    let at = p.source_to_program(&o.anchor).map_or("coupé".into(), |x| format!("montage {}", t(x)));
    let what = match &o.content {
        OverlayContent::Title { text, style } => {
            format!("titre « {} » ({})", text.replace('\n', " / "), format!("{style:?}").to_lowercase())
        }
        OverlayContent::Image { path, position, scale } => {
            let pos = serde_json::to_value(position).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default();
            format!("image {} ({pos}, {:.0} % de large)", name(path), scale * 100.0)
        }
    };
    format!("- {} : {what}, {at}, {:.1} s [{} {}]", o.id, o.duration, o.anchor.source, t(o.anchor.time))
}

fn sentence_line(x: &text::SentenceView) -> String {
    let when = x.program.map_or_else(|| "  coupé".to_string(), |p| format!("{:>7}", t(p)));
    let note = if x.kept == 0.0 {
        " (coupé)"
    } else if x.kept < 0.9 {
        " (en partie coupé)"
    } else {
        ""
    };
    format!("{when} | {} {}–{} | {}{note}", x.source, t(x.start), t(x.end), x.text)
}

fn summary_after(p: &Project) -> String {
    format!("Durée du montage : {}.", t(p.program_duration()))
}

// ------------------------------------------------------------------ définitions

fn tool(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": { "type": "object", "properties": properties, "required": required },
    })
}

pub fn definitions() -> Vec<Value> {
    let project = json!({ "type": "string", "description": "Chemin du projet .derush.json (par défaut : celui ouvert dans l'app)" });
    let source = json!({ "type": "string", "description": "Identifiant du rush, ex. src-0" });
    let secs = |d: &str| json!({ "type": "number", "description": d });
    let anchor_props = |extra: Value| {
        let mut m = json!({
            "project": project,
            "source": source,
            "time": secs("Instant dans le rush, en secondes (avec source)"),
            "program_time": secs("Ou : instant dans la vidéo montée, en secondes"),
        });
        m.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        m
    };
    vec![
        tool("get_project", "Vue d'ensemble du montage : rushes, durée, coupes, titres et images, musique, intro/outro, format d'export. À appeler en premier.", json!({ "project": project }), &[]),
        tool(
            "get_transcript",
            "Transcription phrase par phrase avec, pour chacune : temps dans la vidéo montée (ou « coupé »), rush et temps dans le rush. Filtrable par rush ou par plage de temps du montage.",
            json!({
                "project": project,
                "source": source,
                "from": secs("Début de la plage, en temps du montage"),
                "to": secs("Fin de la plage, en temps du montage"),
                "include_cut": { "type": "boolean", "description": "Inclure les phrases coupées (défaut : oui)" },
            }),
            &[],
        ),
        tool(
            "search_transcript",
            "Cherche des mots dans la transcription (sans tenir compte des accents ni de la casse). Pratique pour retrouver « le passage où je parle du lin ».",
            json!({ "project": project, "query": { "type": "string" } }),
            &["query"],
        ),
        tool(
            "cut",
            "Coupe une plage d'un rush (temps du rush, pris dans la transcription).",
            json!({ "project": project, "source": source, "start": secs("Début dans le rush"), "end": secs("Fin dans le rush") }),
            &["source", "start", "end"],
        ),
        tool(
            "keep",
            "Remet une plage d'un rush dans le montage (annule une coupe).",
            json!({ "project": project, "source": source, "start": secs("Début dans le rush"), "end": secs("Fin dans le rush") }),
            &["source", "start", "end"],
        ),
        tool(
            "find_retakes",
            "Repère les prises ratées encore dans le montage (phrase recommencée juste après, « je recommence »…). Avec apply=true, les coupe.",
            json!({ "project": project, "apply": { "type": "boolean" } }),
            &[],
        ),
        tool(
            "add_title",
            "Ajoute un titre à l'écran. Placement : program_time, ou source + time. Styles : lower (bandeau bas), center, big (gros titre). Les titres servent aussi de chapitres YouTube.",
            anchor_props(json!({
                "text": { "type": "string" },
                "style": { "type": "string", "enum": ["lower", "center", "big"] },
                "duration": secs("Durée d'affichage (défaut 3.5 s)"),
            })),
            &["text"],
        ),
        tool(
            "add_image",
            "Incruste une image (logo, photo de tissu…). Placement : program_time, ou source + time.",
            anchor_props(json!({
                "path": { "type": "string", "description": "Chemin de l'image (png, jpg)" },
                "position": { "type": "string", "enum": ["top-left", "top-right", "bottom-left", "bottom-right", "center"] },
                "scale": secs("Largeur en fraction de la vidéo, 0.05–1 (défaut 0.15)"),
                "duration": secs("Durée d'affichage (défaut 4 s)"),
            })),
            &["path"],
        ),
        tool(
            "update_overlay",
            "Modifie un titre ou une image (texte, style, durée, position, taille, ou déplacement avec program_time / source + time).",
            anchor_props(json!({
                "id": { "type": "string" },
                "text": { "type": "string" },
                "style": { "type": "string", "enum": ["lower", "center", "big"] },
                "position": { "type": "string", "enum": ["top-left", "top-right", "bottom-left", "bottom-right", "center"] },
                "scale": secs("Largeur relative"),
                "duration": secs("Durée d'affichage"),
            })),
            &["id"],
        ),
        tool("remove_overlay", "Supprime un titre ou une image.", json!({ "project": project, "id": { "type": "string" } }), &["id"]),
        tool(
            "set_music",
            "Musique de fond. path=null pour la retirer ; sans path, modifie seulement le volume ou la baisse sous la voix.",
            json!({
                "project": project,
                "path": { "type": ["string", "null"] },
                "volume_db": secs("Gain en dB, ex. -20"),
                "duck": { "type": "boolean", "description": "Baisser la musique quand elle parle (défaut : oui)" },
            }),
            &[],
        ),
        tool("set_intro", "Vidéo d'intro (path=null pour la retirer).", json!({ "project": project, "path": { "type": ["string", "null"] } }), &["path"]),
        tool("set_outro", "Vidéo d'outro (path=null pour la retirer).", json!({ "project": project, "path": { "type": ["string", "null"] } }), &["path"]),
        tool(
            "set_export",
            "Réglages d'export.",
            json!({
                "project": project,
                "width": { "type": "integer" },
                "height": { "type": "integer" },
                "fps": { "type": "integer" },
                "encoder": { "type": "string", "enum": ["hardware", "x264"], "description": "hardware = puce Apple (rapide)" },
                "bitrate": { "type": "string", "description": "Débit en mode hardware, ex. 12M" },
            }),
            &[],
        ),
        tool(
            "transcribe",
            "Transcrit les rushes (en tâche de fond, suivre avec job_status). Par défaut, ceux qui n'ont pas encore de transcription.",
            json!({
                "project": project,
                "sources": { "type": "array", "items": { "type": "string" } },
                "prompt": { "type": "string", "description": "Vocabulaire à bien reconnaître (marques, noms de tissus)" },
                "download_model": { "type": "boolean", "description": "Télécharger le modèle s'il manque (~550 Mo)" },
            }),
            &[],
        ),
        tool(
            "export_video",
            "Exporte le MP4 final (en tâche de fond, suivre avec job_status). Confirme d'abord avec l'utilisatrice.",
            json!({ "project": project, "output": { "type": "string", "description": "Chemin du .mp4 à créer" } }),
            &["output"],
        ),
        tool("job_status", "État de l'export, de l'aperçu et de la transcription en cours.", json!({}), &[]),
        tool(
            "list_media",
            "Liste les vidéos, musiques et images d'un dossier, avec durée et date. Sans dossier : survol de Films, Musique, Bureau, Téléchargements et Images, pour trouver les rushes du tournage ou la musique.",
            json!({
                "folder": { "type": "string", "description": "Dossier à lister (~ accepté)" },
                "recursive": { "type": "boolean", "description": "Inclure les sous-dossiers (3 niveaux)" },
                "kind": { "type": "string", "enum": ["video", "audio", "image"] },
            }),
            &[],
        ),
        tool(
            "get_frames",
            "Images de la vidéo montée, telles qu'elles seront exportées (titres et incrustations compris). Donne des instants précis (program_times), ou count pour des images réparties sur tout le montage. Sert à vérifier un titre ou un logo, ou à repérer ce qui est à l'écran.",
            json!({
                "project": project,
                "program_times": { "type": "array", "items": { "type": "number" }, "description": "Instants du montage, en secondes (8 au plus)" },
                "count": { "type": "integer", "description": "Ou : nombre d'images réparties sur le montage (défaut 6, 8 au plus)" },
                "width": { "type": "integer", "description": "Largeur des images (défaut 640)" },
            }),
            &[],
        ),
        tool(
            "preview",
            "Aperçu rapide en basse résolution, de tout le montage ou d'une plage, ouvert dans le lecteur vidéo du Mac pour que l'utilisatrice valide avant l'export final. Une plage courte est prête en quelques secondes ; sinon, suivre avec job_status.",
            json!({
                "project": project,
                "from": secs("Début, en temps du montage (défaut : début)"),
                "to": secs("Fin, en temps du montage (défaut : fin)"),
                "open": { "type": "boolean", "description": "Ouvrir l'aperçu une fois prêt (défaut : oui)" },
            }),
            &[],
        ),
        tool(
            "write_subtitles",
            "Écrit les sous-titres .srt de la vidéo montée (demande une transcription).",
            json!({ "project": project, "output": { "type": "string", "description": "Chemin du .srt" } }),
            &["output"],
        ),
        tool("get_chapters", "Chapitres YouTube (à coller dans la description), générés à partir des titres.", json!({ "project": project }), &[]),
        tool(
            "create_project",
            "Crée un projet à partir de rushes : analyse du son et coupe des blancs. Le projet devient le projet courant.",
            json!({
                "rushes": { "type": "array", "items": { "type": "string" }, "description": "Chemins des vidéos, dans l'ordre" },
                "output": { "type": "string", "description": "Chemin du .derush.json (défaut : à côté du premier rush)" },
                "min_silence": secs("Blanc minimum coupé (défaut 0.6 s)"),
                "padding": secs("Marge gardée autour de la voix (défaut 0.15 s)"),
            }),
            &["rushes"],
        ),
    ]
}

// ------------------------------------------------------------------ appels

/// Contenu MCP renvoyé à Claude : du texte, et des images pour `get_frames`.
pub fn call(ctx: &Ctx, name: &str, args: &Value) -> R<Vec<Value>> {
    match name {
        "get_frames" => get_frames(ctx, args),
        _ => Ok(vec![json!({ "type": "text", "text": call_text(ctx, name, args)? })]),
    }
}

fn call_text(ctx: &Ctx, name: &str, args: &Value) -> R<String> {
    match name {
        "get_project" => get_project(ctx, args),
        "get_transcript" => get_transcript(ctx, args),
        "search_transcript" => search_transcript(ctx, args),
        "cut" | "keep" => {
            let keep = name == "keep";
            let ((src, a, b), p) = ctx.edit_args(args, |p| {
                let (src, a, b) = range(p, args)?;
                p.mark_range(&src, a, b, if keep { SegmentKind::Speech } else { SegmentKind::Manual }, keep);
                Ok((src, a, b))
            })?;
            Ok(format!(
                "{} {src} {}–{} ({:.1} s). {}",
                if keep { "Gardé" } else { "Coupé" },
                t(a),
                t(b),
                b - a,
                summary_after(&p)
            ))
        }
        "find_retakes" => find_retakes(ctx, args),
        "add_title" | "add_image" => add_overlay(ctx, args, name == "add_title"),
        "update_overlay" => update_overlay(ctx, args),
        "remove_overlay" => {
            let id = req_s(args, "id")?.to_string();
            ctx.edit_args(args, |p| {
                let n = p.overlays.len();
                p.overlays.retain(|o| o.id != id);
                if p.overlays.len() == n {
                    return Err(format!("aucun titre ou image « {id} »"));
                }
                Ok(())
            })?;
            Ok(format!("{id} supprimé."))
        }
        "set_music" => set_music(ctx, args),
        "set_intro" | "set_outro" => {
            let intro = name == "set_intro";
            let path = match args.get("path") {
                Some(Value::String(p)) => Some(existing_file(p)?),
                _ => None,
            };
            let label = path.as_deref().map(name_of).unwrap_or_else(|| "retirée".into());
            ctx.edit_args(args, |p| {
                if intro {
                    p.intro = path;
                } else {
                    p.outro = path;
                }
                Ok(())
            })?;
            Ok(format!("{} : {label}.", if intro { "Intro" } else { "Outro" }))
        }
        "set_export" => set_export(ctx, args),
        "transcribe" => transcribe(ctx, args),
        "export_video" => export_video(ctx, args),
        "job_status" => Ok(job_status(ctx)),
        "write_subtitles" => {
            let (_, p) = ctx.read(args)?;
            if p.transcripts.is_empty() {
                return Err("Pas de transcription : lance transcribe d'abord.".into());
            }
            let out = req_s(args, "output")?;
            let intro = p.intro.as_ref().and_then(|i| media::probe(i).ok()).map_or(0.0, |i| i.duration);
            let srt = text::srt(&p, intro);
            std::fs::write(out, &srt).map_err(err)?;
            Ok(format!("{} sous-titres écrits dans {out}.", srt.matches(" --> ").count()))
        }
        "get_chapters" => {
            let (_, p) = ctx.read(args)?;
            let intro = p.intro.as_ref().and_then(|i| media::probe(i).ok()).map_or(0.0, |i| i.duration);
            let c = text::chapters(&p, intro);
            let mut s = c.text;
            for w in c.warnings {
                s += &format!("\n\nAttention : {w}");
            }
            Ok(s)
        }
        "create_project" => create_project(args),
        "list_media" => list_media(args),
        "preview" => preview(ctx, args),
        _ => Err(format!("outil inconnu : {name}")),
    }
}

fn name_of(p: &Path) -> String {
    name(p)
}

fn get_project(ctx: &Ctx, args: &Value) -> R<String> {
    let (path, p) = ctx.read(args)?;
    let raw: f64 = p.sources.iter().map(|s| s.duration).sum();
    let mut by_kind: BTreeMap<&str, usize> = BTreeMap::new();
    for s in p.segments.iter().filter(|s| !s.keep) {
        *by_kind.entry(kind_fr(s.kind)).or_default() += 1;
    }
    let cuts: usize = by_kind.values().sum();
    let detail = by_kind.iter().map(|(k, n)| format!("{k} : {n}")).collect::<Vec<_>>().join(", ");
    let mut o = format!(
        "Projet : {}\nMontage : {} (sur {} de rushes ; {cuts} coupes{})\n\nRushes :\n",
        path.display(),
        t(p.program_duration()),
        t(raw),
        if cuts > 0 { format!(" — {detail}") } else { String::new() }
    );
    for s in &p.sources {
        let tr = p.transcripts.get(&s.id).map_or("non transcrit".into(), |w| format!("transcrit, {} mots", w.len()));
        o += &format!("- {} « {} » {} ({tr})\n", s.id, name(&s.path), t(s.duration));
    }
    o += "\nTitres et images :\n";
    if p.overlays.is_empty() {
        o += "- aucun\n";
    }
    let mut ovs: Vec<&Overlay> = p.overlays.iter().collect();
    ovs.sort_by(|a, b| p.source_to_program(&a.anchor).partial_cmp(&p.source_to_program(&b.anchor)).unwrap_or(std::cmp::Ordering::Equal));
    for ov in ovs {
        o += &overlay_line(&p, ov);
        o += "\n";
    }
    o += &format!(
        "\nMusique : {}\nIntro : {} · Outro : {}\nExport : {}×{} {} i/s, encodeur {}\n",
        p.music.as_ref().map_or("aucune".into(), |m| format!(
            "{}, {} dB{}",
            name(&m.path),
            m.volume_db,
            if m.duck { ", baissée sous la voix" } else { "" }
        )),
        p.intro.as_deref().map_or("—".into(), name),
        p.outro.as_deref().map_or("—".into(), name),
        p.export.width,
        p.export.height,
        p.export.fps,
        p.export.video_encoder
    );
    Ok(o)
}

const MAX_OUTPUT: usize = 60_000;

fn get_transcript(ctx: &Ctx, args: &Value) -> R<String> {
    let (_, p) = ctx.read(args)?;
    if p.transcripts.is_empty() {
        return Err("Aucune transcription. Lance transcribe (ou « Transcrire » dans l'onglet Texte de l'app).".into());
    }
    let include_cut = args.get("include_cut").and_then(Value::as_bool).unwrap_or(true);
    let (from, to) = (f(args, "from"), f(args, "to"));
    let source = s(args, "source");
    let mut out = String::from("montage | rush début–fin | texte\n");
    let mut n = 0;
    for x in text::sentences(&p) {
        if source.is_some_and(|s| s != x.source) || (!include_cut && x.kept == 0.0) {
            continue;
        }
        if from.is_some() || to.is_some() {
            let Some(pt) = x.program else { continue };
            if from.is_some_and(|a| pt < a) || to.is_some_and(|b| pt > b) {
                continue;
            }
        }
        let line = sentence_line(&x);
        if out.len() + line.len() > MAX_OUTPUT {
            out += &format!("… (tronqué : utilise from/to pour lire la suite, dernière phrase à montage {})\n", x.program.map_or("?".into(), t));
            break;
        }
        out += &line;
        out.push('\n');
        n += 1;
    }
    if n == 0 {
        return Ok("Aucune phrase dans cette plage.".into());
    }
    Ok(out)
}

fn norm(x: &str) -> String {
    x.chars()
        .flat_map(char::to_lowercase)
        .map(|c| match c {
            'à' | 'â' | 'ä' => 'a',
            'é' | 'è' | 'ê' | 'ë' => 'e',
            'î' | 'ï' => 'i',
            'ô' | 'ö' => 'o',
            'ù' | 'û' | 'ü' => 'u',
            'ç' => 'c',
            'œ' => 'o',
            c => c,
        })
        .collect()
}

fn search_transcript(ctx: &Ctx, args: &Value) -> R<String> {
    let (_, p) = ctx.read(args)?;
    if p.transcripts.is_empty() {
        return Err("Aucune transcription : lance transcribe d'abord.".into());
    }
    let q = norm(req_s(args, "query")?);
    let terms: Vec<&str> = q.split_whitespace().collect();
    let hits: Vec<String> = text::sentences(&p)
        .iter()
        .filter(|x| {
            let n = norm(&x.text);
            terms.iter().all(|t| n.contains(t))
        })
        .map(sentence_line)
        .collect();
    if hits.is_empty() {
        return Ok(format!("Rien trouvé pour « {} ».", req_s(args, "query")?));
    }
    Ok(format!("{} phrase(s) :\n{}", hits.len(), hits.join("\n")))
}

fn find_retakes(ctx: &Ctx, args: &Value) -> R<String> {
    let apply = args.get("apply").and_then(Value::as_bool).unwrap_or(false);
    let path = ctx.project_path(args)?;
    let (lines, p) = ctx.edit(&path, |p| {
        if p.transcripts.is_empty() {
            return Err("Aucune transcription : lance transcribe d'abord.".into());
        }
        let mut found = vec![];
        for (source, words) in &p.transcripts {
            for r in retakes::find_retakes(words, &Default::default()) {
                if p.kept_segments().any(|s| &s.source == source && s.start < r.end && s.end > r.start) {
                    found.push((source.clone(), r));
                }
            }
        }
        if apply {
            for (src, r) in &found {
                p.mark_range(src, r.start, r.end, SegmentKind::Retake, false);
            }
        }
        Ok(found
            .iter()
            .map(|(src, r)| format!("- {src} {}–{} « {} » — {}", t(r.start), t(r.end), r.text, r.reason))
            .collect::<Vec<_>>())
    })?;
    if lines.is_empty() {
        return Ok("Aucune prise ratée dans le montage.".into());
    }
    Ok(format!(
        "{} prise(s) ratée(s) {} :\n{}\n\n{}",
        lines.len(),
        if apply { "coupée(s)" } else { "repérée(s) (rien n'est coupé : apply=true pour couper)" },
        lines.join("\n"),
        summary_after(&p)
    ))
}

fn new_id(p: &Project, prefix: &str) -> String {
    (1..).map(|i| format!("{prefix}-{i}")).find(|id| p.overlays.iter().all(|o| &o.id != id)).expect("identifiant libre")
}

fn add_overlay(ctx: &Ctx, args: &Value, title: bool) -> R<String> {
    let image = if title { None } else { Some(existing_file(req_s(args, "path")?)?) };
    let (line, _) = ctx.edit_args(args, |p| {
        let anchor = anchor(p, args)?;
        let content = match &image {
            None => OverlayContent::Title { text: req_s(args, "text")?.into(), style: parse_style(s(args, "style"))? },
            Some(path) => OverlayContent::Image {
                path: path.clone(),
                position: parse_position(s(args, "position"))?,
                scale: f(args, "scale").unwrap_or(0.15).clamp(0.05, 1.0),
            },
        };
        let id = new_id(p, if title { "titre" } else { "image" });
        let duration = f(args, "duration").unwrap_or(if title { 3.5 } else { 4.0 }).max(0.5);
        let ov = Overlay { id, anchor, duration, content };
        let line = overlay_line(p, &ov);
        p.overlays.push(ov);
        Ok(line)
    })?;
    Ok(format!("Ajouté :\n{line}"))
}

fn update_overlay(ctx: &Ctx, args: &Value) -> R<String> {
    let id = req_s(args, "id")?;
    let (line, _) = ctx.edit_args(args, |p| {
        let moved = if f(args, "program_time").is_some() || s(args, "source").is_some() { Some(anchor(p, args)?) } else { None };
        let o = p.overlays.iter_mut().find(|o| o.id == id).ok_or_else(|| format!("aucun titre ou image « {id} »"))?;
        if let Some(a) = moved {
            o.anchor = a;
        }
        if let Some(d) = f(args, "duration") {
            o.duration = d.max(0.5);
        }
        match &mut o.content {
            OverlayContent::Title { text, style } => {
                if let Some(x) = s(args, "text") {
                    *text = x.into();
                }
                if let Some(x) = s(args, "style") {
                    *style = parse_style(Some(x))?;
                }
            }
            OverlayContent::Image { position, scale, .. } => {
                if let Some(x) = s(args, "position") {
                    *position = parse_position(Some(x))?;
                }
                if let Some(x) = f(args, "scale") {
                    *scale = x.clamp(0.05, 1.0);
                }
            }
        }
        let o = o.clone();
        Ok(overlay_line(p, &o))
    })?;
    Ok(format!("Modifié :\n{line}"))
}

fn set_music(ctx: &Ctx, args: &Value) -> R<String> {
    let new_path = match args.get("path") {
        Some(Value::String(x)) => Some(Some(existing_file(x)?)),
        Some(Value::Null) => Some(None),
        _ => None,
    };
    let (msg, _) = ctx.edit_args(args, |p| {
        match new_path {
            Some(None) => p.music = None,
            Some(Some(path)) => {
                let old = p.music.take();
                p.music = Some(Music {
                    path,
                    volume_db: old.as_ref().map_or(-20.0, |m| m.volume_db),
                    duck: old.as_ref().map_or(true, |m| m.duck),
                })
            }
            None if p.music.is_none() => return Err("Pas de musique : donne un path.".into()),
            None => {}
        }
        if let Some(m) = p.music.as_mut() {
            if let Some(v) = f(args, "volume_db") {
                m.volume_db = v.clamp(-60.0, 6.0);
            }
            if let Some(d) = args.get("duck").and_then(Value::as_bool) {
                m.duck = d;
            }
        }
        Ok(p.music.as_ref().map_or("Musique retirée.".into(), |m| {
            format!("Musique : {}, {} dB{}.", name(&m.path), m.volume_db, if m.duck { ", baissée sous la voix" } else { "" })
        }))
    })?;
    Ok(msg)
}

fn set_export(ctx: &Ctx, args: &Value) -> R<String> {
    let (msg, _) = ctx.edit_args(args, |p| {
        let e = &mut p.export;
        if let Some(w) = args.get("width").and_then(Value::as_u64) {
            e.width = (w as u32).max(16) & !1;
        }
        if let Some(h) = args.get("height").and_then(Value::as_u64) {
            e.height = (h as u32).max(16) & !1;
        }
        if let Some(r) = args.get("fps").and_then(Value::as_u64) {
            e.fps = (r as u32).clamp(10, 120);
        }
        match s(args, "encoder") {
            Some("hardware") => e.video_encoder = "h264_videotoolbox".into(),
            Some("x264") => e.video_encoder = "libx264".into(),
            Some(o) => return Err(format!("encodeur inconnu : {o}")),
            None => {}
        }
        if let Some(b) = s(args, "bitrate") {
            e.video_bitrate = b.into();
        }
        Ok(format!("Export : {}×{} {} i/s, {}.", e.width, e.height, e.fps, e.video_encoder))
    })?;
    Ok(msg)
}

fn job_status(ctx: &Ctx) -> String {
    let jobs = ctx.jobs.lock().map(|j| j.clone()).unwrap_or_default();
    if jobs.is_empty() {
        return "Aucune tâche lancée.".into();
    }
    jobs.iter()
        .map(|(kind, j)| {
            let el = j.started.elapsed().as_secs_f64();
            let st = match &j.state {
                JobState::Running => {
                    let left = if j.progress > 0.03 { format!(", encore ~{}", t(el / j.progress - el)) } else { String::new() };
                    format!("en cours, {:.0} %{left}", j.progress * 100.0)
                }
                JobState::Done(m) => format!("terminé : {m}"),
                JobState::Failed(e) => format!("échec : {e}"),
            };
            format!("{kind} : {st}{}", if j.detail.is_empty() { String::new() } else { format!(" ({})", j.detail) })
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn export_video(ctx: &Ctx, args: &Value) -> R<String> {
    if ctx.running("export") {
        return Ok(format!("Un export est déjà en cours.\n{}", job_status(ctx)));
    }
    let (_, p) = ctx.read(args)?;
    let output = PathBuf::from(req_s(args, "output")?);
    let job = render::build(&p, &output).map_err(err)?;
    let warnings = job.warnings.clone();
    ctx.set_job("export", 0.0, JobState::Running, name(&output));
    let shown = output.display().to_string();
    let c = ctx.clone();
    std::thread::spawn(move || {
        let c2 = c.clone();
        let out = name(&output);
        let res = render::run(&job, move |x| c2.set_job("export", x, JobState::Running, out.clone()));
        match res {
            Ok(()) => c.set_job("export", 1.0, JobState::Done(output.display().to_string()), ""),
            Err(e) => c.set_job("export", 0.0, JobState::Failed(e.to_string()), ""),
        }
    });
    let mut msg = format!("Export lancé vers {shown} ({} de vidéo). Suis-le avec job_status.", t(job_duration(&p)));
    for w in warnings {
        msg += &format!("\nAttention : {w}");
    }
    Ok(msg)
}

fn job_duration(p: &Project) -> f64 {
    let probe = |x: &Option<PathBuf>| x.as_ref().and_then(|i| media::probe(i).ok()).map_or(0.0, |i| i.duration);
    p.program_duration() + probe(&p.intro) + probe(&p.outro)
}

fn transcribe(ctx: &Ctx, args: &Value) -> R<String> {
    if ctx.running("transcription") {
        return Ok(format!("Une transcription est déjà en cours.\n{}", job_status(ctx)));
    }
    let path = ctx.project_path(args)?;
    let (_, p) = ctx.read(args)?;
    let wanted: Option<Vec<String>> = args
        .get("sources")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect());
    let todo: Vec<Source> = p
        .sources
        .iter()
        .filter(|s| wanted.as_ref().map_or(!p.transcripts.contains_key(&s.id), |w| w.contains(&s.id)))
        .cloned()
        .collect();
    if todo.is_empty() {
        return Ok("Tous les rushes sont déjà transcrits (passe sources pour en refaire).".into());
    }
    let download = args.get("download_model").and_then(Value::as_bool).unwrap_or(false);
    if transcribe::installed_model(transcribe::DEFAULT_MODEL).is_none() && !download {
        let m = transcribe::model(transcribe::DEFAULT_MODEL).map_err(err)?;
        return Err(format!(
            "Le modèle de transcription n'est pas installé ({} Mo). Télécharge-le depuis l'onglet Texte de l'app, ou relance avec download_model=true.",
            m.size_mb
        ));
    }
    let prompt = s(args, "prompt").map(String::from);
    let c = ctx.clone();
    let total = todo.len();
    c.set_job("transcription", 0.0, JobState::Running, "démarrage");
    std::thread::spawn(move || {
        let run = || -> R<String> {
            let model = match transcribe::installed_model(transcribe::DEFAULT_MODEL) {
                Some(m) => m,
                None => {
                    let c2 = c.clone();
                    transcribe::download_model(transcribe::DEFAULT_MODEL, move |x| {
                        c2.set_job("transcription", 0.0, JobState::Running, format!("téléchargement du modèle {:.0} %", x * 100.0))
                    })
                    .map_err(err)?
                }
            };
            let opts = transcribe::Options { model, language: Some("fr".into()), prompt };
            let mut words_total = 0;
            for (i, src) in todo.iter().enumerate() {
                let c2 = c.clone();
                let label = format!("{} ({}/{total})", src.id, i + 1);
                let words = transcribe::transcribe_file(
                    &src.path,
                    &opts,
                    move |x| c2.set_job("transcription", (i as f64 + x) / total as f64, JobState::Running, label.clone()),
                    Arc::new(AtomicBool::new(false)),
                )
                .map_err(err)?;
                words_total += words.len();
                // Enregistré rush par rush : l'app affiche le texte au fur et à mesure.
                let id = src.id.clone();
                c.edit(&path, move |p| {
                    p.transcripts.insert(id, words);
                    Ok(())
                })?;
            }
            Ok(format!("{total} rush(es), {words_total} mots"))
        };
        match run() {
            Ok(m) => c.set_job("transcription", 1.0, JobState::Done(m), ""),
            Err(e) => c.set_job("transcription", 0.0, JobState::Failed(e), ""),
        }
    });
    Ok(format!("Transcription lancée pour {total} rush(es). Suis-la avec job_status."))
}

fn create_project(args: &Value) -> R<String> {
    let rushes: Vec<PathBuf> = args
        .get("rushes")
        .and_then(Value::as_array)
        .ok_or("paramètre « rushes » manquant")?
        .iter()
        .filter_map(Value::as_str)
        .map(existing_file)
        .collect::<R<_>>()?;
    let first = rushes.first().ok_or("aucun rush")?;
    let output = s(args, "output").map(PathBuf::from).unwrap_or_else(|| {
        first.with_file_name(format!("{}.derush.json", first.file_stem().unwrap_or_default().to_string_lossy()))
    });
    let mut params = SilenceParams::default();
    if let Some(x) = f(args, "min_silence") {
        params.min_silence = x;
    }
    if let Some(x) = f(args, "padding") {
        params.padding = x;
    }
    let mut sources = vec![];
    let mut segments = vec![];
    for (i, r) in rushes.iter().enumerate() {
        let a = analysis::analyze_file(r, &format!("src-{i}"), &params).map_err(err)?;
        sources.push(a.source);
        segments.extend(a.segments);
    }
    let p = Project::new(sources, segments);
    p.save(&output).map_err(err)?;
    paths::set_current_project(&output).map_err(err)?;
    let cuts = p.segments.iter().filter(|s| !s.keep).count();
    Ok(format!(
        "Projet créé : {}\n{} rush(es), {cuts} blancs coupés. {}\nOuvre ce fichier dans Derush pour voir le montage.",
        output.display(),
        p.sources.len(),
        summary_after(&p)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Les tests lisent la réponse texte des outils.
    fn call(ctx: &Ctx, name: &str, args: &Value) -> R<String> {
        call_text(ctx, name, args)
    }

    fn setup() -> (tempfile::TempDir, Ctx) {
        let dir = tempfile::tempdir().unwrap();
        let mut p = Project::new(
            vec![Source { id: "src-0".into(), path: dir.path().join("a.mp4"), duration: 20.0, width: None, height: None, fps: None }],
            vec![
                Segment { id: String::new(), source: "src-0".into(), start: 0.0, end: 8.0, kind: SegmentKind::Speech, keep: true },
                Segment { id: String::new(), source: "src-0".into(), start: 8.0, end: 10.0, kind: SegmentKind::Silence, keep: false },
                Segment { id: String::new(), source: "src-0".into(), start: 10.0, end: 20.0, kind: SegmentKind::Speech, keep: true },
            ],
        );
        let w = |text: &str, start: f64| Word { text: text.into(), start, end: start + 0.3 };
        p.transcripts.insert(
            "src-0".into(),
            vec![
                w("Aujourd'hui", 0.5), w("je", 0.9), w("vous", 1.2), w("montre", 1.5), w("du.", 1.8),
                w("Aujourd'hui", 3.0), w("je", 3.4), w("vous", 3.7), w("montre", 4.0), w("du", 4.3), w("lin.", 4.6),
                w("Le", 11.0), w("coton", 11.3), w("bio", 11.6), w("arrive", 11.9), w("ensuite.", 12.2),
            ],
        );
        let path = dir.path().join("p.derush.json");
        p.save(&path).unwrap();
        (dir, Ctx::new(Some(path)))
    }

    #[test]
    fn every_tool_has_a_schema() {
        for d in definitions() {
            assert!(d["name"].is_string() && d["inputSchema"]["type"] == "object", "{d}");
        }
    }

    #[test]
    fn transcript_search_and_cut() {
        let (_d, ctx) = setup();
        let tr = call(&ctx, "get_transcript", &json!({})).unwrap();
        assert!(tr.contains("Le coton bio arrive ensuite."), "{tr}");
        let hit = call(&ctx, "search_transcript", &json!({ "query": "COTON" })).unwrap();
        assert!(hit.starts_with("1 phrase"), "{hit}");
        // « Le coton… » : rush 11.0, montage 11.0 - 2 s de blanc coupé = 9.0
        assert!(hit.contains("0:09.0"), "{hit}");

        let r = call(&ctx, "cut", &json!({ "source": "src-0", "start": 11.0, "end": 12.5 })).unwrap();
        assert!(r.contains("Durée du montage : 0:16.5"), "{r}");
        let tr = call(&ctx, "get_transcript", &json!({ "include_cut": false })).unwrap();
        assert!(!tr.contains("coton"), "{tr}");
    }

    #[test]
    fn retakes_are_found_then_cut() {
        let (_d, ctx) = setup();
        let r = call(&ctx, "find_retakes", &json!({})).unwrap();
        assert!(r.starts_with("1 prise"), "{r}");
        let r = call(&ctx, "find_retakes", &json!({ "apply": true })).unwrap();
        assert!(r.contains("coupée"), "{r}");
        assert_eq!(call(&ctx, "find_retakes", &json!({})).unwrap(), "Aucune prise ratée dans le montage.");
    }

    #[test]
    fn titles_by_program_time_and_chapters() {
        let (_d, ctx) = setup();
        let r = call(&ctx, "add_title", &json!({ "text": "Le coton", "program_time": 9.0 })).unwrap();
        assert!(r.contains("titre-1") && r.contains("[src-0 0:11.0]"), "{r}");
        call(&ctx, "update_overlay", &json!({ "id": "titre-1", "style": "big", "duration": 5 })).unwrap();
        let proj = call(&ctx, "get_project", &json!({})).unwrap();
        assert!(proj.contains("« Le coton » (big)"), "{proj}");
        let ch = call(&ctx, "get_chapters", &json!({})).unwrap();
        assert!(ch.starts_with("0:00 Introduction\n0:09 Le coton"), "{ch}");
        call(&ctx, "remove_overlay", &json!({ "id": "titre-1" })).unwrap();
        assert!(call(&ctx, "remove_overlay", &json!({ "id": "titre-1" })).is_err());
    }

    #[test]
    fn bad_input_is_reported() {
        let (_d, ctx) = setup();
        assert!(call(&ctx, "cut", &json!({ "source": "src-9", "start": 1, "end": 2 })).unwrap_err().contains("inconnu"));
        assert!(call(&ctx, "add_title", &json!({ "text": "x", "program_time": 999 })).unwrap_err().contains("dépasse"));
        assert!(call(&ctx, "set_music", &json!({ "path": "/nope.mp3" })).unwrap_err().contains("introuvable"));
    }
}

// ------------------------------------------------------------------ médias, images, aperçus

const VIDEO_EXT: &[&str] = &["mp4", "mov", "m4v", "mkv", "avi", "mts"];
const AUDIO_EXT: &[&str] = &["mp3", "m4a", "wav", "aac", "flac", "ogg", "aif", "aiff"];
const IMAGE_EXT: &[&str] = &["png", "jpg", "jpeg", "webp"];

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| "/".into())
}

fn expand(p: &str) -> PathBuf {
    match p.strip_prefix("~") {
        Some(rest) => home().join(rest.trim_start_matches('/')),
        None => PathBuf::from(p),
    }
}

fn media_kind(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_lowercase();
    if VIDEO_EXT.contains(&ext.as_str()) {
        Some("video")
    } else if AUDIO_EXT.contains(&ext.as_str()) {
        Some("audio")
    } else if IMAGE_EXT.contains(&ext.as_str()) {
        Some("image")
    } else {
        None
    }
}

fn age(path: &Path) -> String {
    let Some(secs) = std::fs::metadata(path).ok().and_then(|m| m.modified().ok()).and_then(|m| m.elapsed().ok()).map(|d| d.as_secs())
    else {
        return String::new();
    };
    match secs {
        s if s < 3600 => format!("il y a {} min", s / 60),
        s if s < 86_400 => format!("il y a {} h", s / 3600),
        s if s < 60 * 86_400 => format!("il y a {} j", s / 86_400),
        s => format!("il y a {} mois", s / (30 * 86_400)),
    }
}

/// Fichiers médias d'un dossier (récursif jusqu'à `depth`), au plus `limit`.
fn walk(dir: &Path, depth: u32, kind: Option<&str>, out: &mut Vec<PathBuf>, limit: usize) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<PathBuf> = rd.filter_map(|e| e.ok().map(|e| e.path())).collect();
    entries.sort();
    for p in entries {
        if out.len() >= limit {
            return;
        }
        let hidden = p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with('.'));
        if hidden {
            continue;
        }
        if p.is_dir() {
            if depth > 0 {
                walk(&p, depth - 1, kind, out, limit);
            }
        } else if let Some(k) = media_kind(&p) {
            if kind.map_or(true, |w| w == k) {
                out.push(p);
            }
        }
    }
}

fn list_media(args: &Value) -> R<String> {
    let kind = s(args, "kind");
    let Some(folder) = s(args, "folder") else {
        // Survol des dossiers habituels.
        let mut o = String::from("Dossiers habituels (nombre de médias, sous-dossiers compris sur 2 niveaux) :\n");
        for name in ["Movies", "Music", "Desktop", "Downloads", "Pictures"] {
            let dir = home().join(name);
            if !dir.is_dir() {
                continue;
            }
            let mut files = vec![];
            walk(&dir, 2, kind, &mut files, 2000);
            o += &format!("\n{} : {} média(s)\n", dir.display(), files.len());
            let mut subs: BTreeMap<PathBuf, usize> = BTreeMap::new();
            for f in &files {
                if let Some(parent) = f.parent().filter(|p| *p != dir) {
                    *subs.entry(parent.to_path_buf()).or_default() += 1;
                }
            }
            for (sub, n) in subs.iter().take(15) {
                o += &format!("  - {} ({n}, {})\n", sub.display(), age(sub));
            }
        }
        return Ok(o + "\nPrécise folder pour lister les fichiers.");
    };
    let dir = expand(folder);
    if !dir.is_dir() {
        return Err(format!("dossier introuvable : {}", dir.display()));
    }
    let recursive = args.get("recursive").and_then(Value::as_bool).unwrap_or(false);
    let mut files = vec![];
    walk(&dir, if recursive { 3 } else { 0 }, kind, &mut files, 300);
    let mut o = format!("{} :\n", dir.display());
    if files.is_empty() {
        o += "aucun média";
    }
    for (i, f) in files.iter().enumerate() {
        let k = media_kind(f).unwrap_or("");
        let info = if k != "image" && i < 120 {
            media::probe(f).map(|m| {
                let res = m.width.zip(m.height).map(|(w, h)| format!(", {w}×{h}")).unwrap_or_default();
                format!("{}{res}", t(m.duration))
            }).unwrap_or_else(|_| "illisible".into())
        } else {
            String::new()
        };
        let size = std::fs::metadata(f).map(|m| m.len() as f64 / 1e6).unwrap_or(0.0);
        o += &format!("- [{k}] {} — {info}{}{:.0} Mo, {}\n", f.display(), if info.is_empty() { "" } else { ", " }, size, age(f));
    }
    // Sous-dossiers, pour continuer à naviguer.
    if !recursive {
        let subs: Vec<String> = std::fs::read_dir(&dir)
            .map(|rd| {
                rd.filter_map(|e| e.ok().map(|e| e.path()))
                    .filter(|p| p.is_dir() && !p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with('.')))
                    .map(|p| format!("- {}/", p.display()))
                    .collect()
            })
            .unwrap_or_default();
        if !subs.is_empty() {
            o += &format!("\nSous-dossiers :\n{}\n", subs.join("\n"));
        }
    }
    Ok(o)
}

fn get_frames(ctx: &Ctx, args: &Value) -> R<Vec<Value>> {
    use base64::Engine as _;
    let (_, p) = ctx.read(args)?;
    let total = p.program_duration();
    if total <= 0.0 {
        return Err("Le montage est vide.".into());
    }
    let times: Vec<f64> = match args.get("program_times").and_then(Value::as_array) {
        Some(a) if !a.is_empty() => a.iter().filter_map(Value::as_f64).take(8).map(|x| x.clamp(0.0, total - 0.05)).collect(),
        _ => {
            let n = args.get("count").and_then(Value::as_u64).unwrap_or(6).clamp(1, 8) as usize;
            (0..n).map(|i| total * (i as f64 + 0.5) / n as f64).collect()
        }
    };
    let width = args.get("width").and_then(Value::as_u64).unwrap_or(640).clamp(160, 1280) as u32;
    let sentences = text::sentences(&p);
    let dir = tempfile::tempdir().map_err(err)?;
    let mut content = vec![];
    for (i, &pt) in times.iter().enumerate() {
        let jpg = dir.path().join(format!("f{i}.jpg"));
        render::frame(&p, pt, width, &jpg).map_err(err)?;
        let data = base64::engine::general_purpose::STANDARD.encode(std::fs::read(&jpg).map_err(err)?);
        let src = p.program_to_source(pt);
        let mut label = format!("Image {} — montage {}", i + 1, t(pt));
        if let Some(a) = &src {
            label += &format!(" · {} {}", a.source, t(a.time));
            if let Some(x) = sentences.iter().find(|x| x.source == a.source && a.time >= x.start - 0.2 && a.time <= x.end + 0.2) {
                label += &format!(" · « {} »", x.text);
            }
        }
        let visible: Vec<&str> = p
            .overlays
            .iter()
            .filter(|o| p.source_to_program(&o.anchor).is_some_and(|at| pt >= at && pt < at + o.duration))
            .map(|o| o.id.as_str())
            .collect();
        if !visible.is_empty() {
            label += &format!(" · à l'écran : {}", visible.join(", "));
        }
        content.push(json!({ "type": "text", "text": label }));
        content.push(json!({ "type": "image", "data": data, "mimeType": "image/jpeg" }));
    }
    Ok(content)
}

fn open_file(path: &Path) -> bool {
    let cmd = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    std::process::Command::new(cmd).arg(path).spawn().is_ok()
}

fn preview(ctx: &Ctx, args: &Value) -> R<String> {
    if ctx.running("aperçu") {
        return Ok(format!("Un aperçu est déjà en préparation.\n{}", job_status(ctx)));
    }
    let (_, p) = ctx.read(args)?;
    let total = p.program_duration();
    let from = f(args, "from").unwrap_or(0.0).clamp(0.0, total);
    let to = f(args, "to").unwrap_or(total).clamp(from, total);
    if to - from < 0.5 {
        return Err("Plage trop courte pour un aperçu.".into());
    }
    let whole = from < EPS && to > total - EPS;
    let mut q = if whole { p.clone() } else { render::slice(&p, from, to) };
    let ratio = p.export.height as f64 / p.export.width as f64;
    q.export.width = 854;
    q.export.height = ((854.0 * ratio).round() as u32) & !1;
    let dir = paths::data_dir().join("previews");
    std::fs::create_dir_all(&dir).map_err(err)?;
    let out = dir.join(if whole { "apercu.mp4".to_string() } else { format!("apercu-{:.0}-{:.0}.mp4", from, to) });
    let job = render::build_with(&q, &out, render::Quality::Draft).map_err(err)?;
    let open = args.get("open").and_then(Value::as_bool).unwrap_or(true);
    let label = if whole { "montage entier".to_string() } else { format!("{} → {}", t(from), t(to)) };
    ctx.set_job("aperçu", 0.0, JobState::Running, label.clone());
    let c = ctx.clone();
    let out2 = out.clone();
    std::thread::spawn(move || {
        let c2 = c.clone();
        let l2 = label.clone();
        match render::run(&job, move |x| c2.set_job("aperçu", x, JobState::Running, l2.clone())) {
            Ok(()) => {
                let opened = open && open_file(&out2);
                let msg = format!("{}{}", out2.display(), if opened { ", ouvert dans le lecteur" } else { "" });
                c.set_job("aperçu", 1.0, JobState::Done(msg), label);
            }
            Err(e) => c.set_job("aperçu", 0.0, JobState::Failed(e.to_string()), label),
        }
    });
    // Une plage courte est prête vite : on attend un peu pour répondre directement.
    let deadline = Instant::now() + std::time::Duration::from_secs(if to - from <= 120.0 { 45 } else { 3 });
    while Instant::now() < deadline && ctx.running("aperçu") {
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    let state = ctx.jobs.lock().ok().and_then(|j| j.get("aperçu").map(|j| j.state.clone()));
    Ok(match state {
        Some(JobState::Done(m)) => format!("Aperçu prêt : {m}."),
        Some(JobState::Failed(e)) => return Err(format!("Aperçu échoué : {e}")),
        _ => format!("Aperçu en préparation ({} de vidéo) ; suis-le avec job_status.", t(to - from)),
    })
}
