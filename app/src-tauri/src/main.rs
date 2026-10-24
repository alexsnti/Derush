// Pas de console à côté de la fenêtre sous Windows en release.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Couche fine entre l'UI et `derush-core` : toute la logique reste dans le cœur,
//! pour que le CLI et le serveur MCP fassent exactement la même chose.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use derush_core::analysis::{self, SilenceParams};
use derush_core::{media, paths, render, retakes, text, transcribe, Project, SegmentKind, Word};
use serde::Serialize;
use tauri::{AppHandle, Emitter, State};

/// Points de forme d'onde par seconde envoyés à l'UI.
const WAVE_PER_SECOND: u32 = 25;

#[derive(Default)]
struct AppState {
    cancel_transcription: Arc<AtomicBool>,
}

#[derive(Serialize)]
struct Waveform {
    per_second: u32,
    db: Vec<f32>,
}

#[derive(Serialize)]
struct Loaded {
    project: Project,
    waveforms: HashMap<String, Waveform>,
}

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

fn waveform(levels: &analysis::Levels) -> Waveform {
    Waveform { per_second: WAVE_PER_SECOND, db: levels.downsample(WAVE_PER_SECOND) }
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T, String> + Send + 'static) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(f).await.map_err(err)?
}

/// Durée de l'intro, pour décaler sous-titres et chapitres.
fn intro_duration(p: &Project) -> f64 {
    p.intro.as_ref().and_then(|i| media::probe(i).ok()).map_or(0.0, |i| i.duration)
}

// ------------------------------------------------------------------ projet

/// Analyse des rushes. `first_id` permet d'en ajouter à un projet existant sans collision d'identifiants.
#[tauri::command]
async fn import_rushes(
    paths: Vec<String>,
    params: Option<SilenceParams>,
    first_id: Option<usize>,
) -> Result<Loaded, String> {
    blocking(move || {
        let params = params.unwrap_or_default();
        let first = first_id.unwrap_or(0);
        let mut sources = vec![];
        let mut segments = vec![];
        let mut waveforms = HashMap::new();
        for (i, path) in paths.iter().enumerate() {
            let id = format!("src-{}", first + i);
            let a = analysis::analyze_file(&PathBuf::from(path), &id, &params).map_err(err)?;
            waveforms.insert(a.source.id.clone(), waveform(&a.levels));
            sources.push(a.source);
            segments.extend(a.segments);
        }
        Ok(Loaded { project: Project::new(sources, segments), waveforms })
    })
    .await
}

/// Ajoute des rushes à la fin d'un projet existant ; renvoie les formes d'onde des nouveaux.
#[tauri::command]
async fn append_rushes(
    mut project: Project,
    paths: Vec<String>,
    params: Option<SilenceParams>,
) -> Result<Loaded, String> {
    let first = project
        .sources
        .iter()
        .filter_map(|s| s.id.strip_prefix("src-")?.parse::<usize>().ok())
        .max()
        .map_or(0, |m| m + 1);
    let added = import_rushes(paths, params, Some(first)).await?;
    project.sources.extend(added.project.sources);
    project.segments.extend(added.project.segments);
    project.normalize();
    Ok(Loaded { project, waveforms: added.waveforms })
}

/// Ouvre un projet enregistré. Les formes d'onde déjà connues (`known`) ne sont pas recalculées,
/// ce qui rend instantané le rechargement après une modification faite par Claude.
#[tauri::command]
async fn load_project(path: String, known: Option<Vec<String>>) -> Result<Loaded, String> {
    blocking(move || {
        let project = Project::load(&PathBuf::from(&path)).map_err(err)?;
        let known = known.unwrap_or_default();
        let mut waveforms = HashMap::new();
        for s in project.sources.iter().filter(|s| !known.contains(&s.id)) {
            let levels = analysis::levels_from_file(&s.path, 10).map_err(err)?;
            waveforms.insert(s.id.clone(), waveform(&levels));
        }
        let _ = paths::set_current_project(&PathBuf::from(&path));
        Ok(Loaded { project, waveforms })
    })
    .await
}

#[tauri::command]
fn save_project(path: String, project: Project) -> Result<(), String> {
    project.validate().map_err(err)?;
    let path = PathBuf::from(path);
    project.save(&path).map_err(err)?;
    let _ = paths::set_current_project(&path);
    Ok(())
}

/// Date de dernière modification du fichier projet (ms), pour repérer les changements faits par Claude.
#[tauri::command]
fn project_mtime(path: String) -> Option<u64> {
    let m = std::fs::metadata(path).ok()?.modified().ok()?;
    Some(m.duration_since(UNIX_EPOCH).ok()?.as_millis() as u64)
}

/// Garde ou coupe une plage d'un rush ; renvoie le projet mis à jour.
#[tauri::command]
fn mark_range(
    mut project: Project,
    source: String,
    start: f64,
    end: f64,
    kind: SegmentKind,
    keep: bool,
) -> Result<Project, String> {
    project.mark_range(&source, start, end, kind, keep);
    project.validate().map_err(err)?;
    Ok(project)
}

// ------------------------------------------------------------------ transcription

#[tauri::command]
fn models_status() -> Vec<transcribe::ModelInfo> {
    transcribe::models()
}

/// Télécharge un modèle Whisper ; progression dans `model-progress` (0–1).
#[tauri::command]
async fn download_model(app: AppHandle, id: String) -> Result<String, String> {
    blocking(move || {
        let path = transcribe::download_model(&id, |p| {
            let _ = app.emit("model-progress", p);
        })
        .map_err(err)?;
        Ok(path.to_string_lossy().into_owned())
    })
    .await
}

#[derive(Clone, Serialize)]
struct TranscribeProgress {
    source: String,
    index: usize,
    total: usize,
    progress: f64,
}

/// Transcrit les rushes demandés (tous ceux sans transcription par défaut).
/// Renvoie les transcriptions ; progression dans `transcribe-progress`.
#[tauri::command]
async fn transcribe_rushes(
    app: AppHandle,
    state: State<'_, AppState>,
    project: Project,
    model: Option<String>,
    sources: Option<Vec<String>>,
    prompt: Option<String>,
) -> Result<BTreeMap<String, Vec<Word>>, String> {
    let cancel = state.cancel_transcription.clone();
    cancel.store(false, Ordering::Relaxed);
    blocking(move || {
        let model_id = model.unwrap_or_else(|| transcribe::DEFAULT_MODEL.into());
        let model_path = transcribe::installed_model(&model_id)
            .ok_or("Aucun modèle de transcription installé : télécharge-le d'abord.")?;
        let opts = transcribe::Options {
            model: model_path,
            language: Some("fr".into()),
            prompt: prompt.filter(|p| !p.trim().is_empty()),
        };
        let todo: Vec<_> = project
            .sources
            .iter()
            .filter(|s| match &sources {
                Some(list) => list.contains(&s.id),
                None => !project.transcripts.contains_key(&s.id),
            })
            .cloned()
            .collect();
        let mut out = BTreeMap::new();
        for (index, src) in todo.iter().enumerate() {
            let (app, id, total) = (app.clone(), src.id.clone(), todo.len());
            let words = transcribe::transcribe_file(
                &src.path,
                &opts,
                move |progress| {
                    let _ = app.emit("transcribe-progress", TranscribeProgress { source: id.clone(), index, total, progress });
                },
                cancel.clone(),
            )
            .map_err(|e| if cancel.load(Ordering::Relaxed) { "Transcription annulée.".to_string() } else { err(e) })?;
            out.insert(src.id.clone(), words);
        }
        Ok(out)
    })
    .await
}

#[tauri::command]
fn cancel_transcription(state: State<'_, AppState>) {
    state.cancel_transcription.store(true, Ordering::Relaxed);
}

/// Transcription découpée en phrases, avec leur place dans le montage.
#[tauri::command]
fn transcript(project: Project) -> Vec<text::SentenceView> {
    text::sentences(&project)
}

#[derive(Serialize)]
struct RetakeOut {
    source: String,
    #[serde(flatten)]
    retake: retakes::Retake,
}

/// Prises ratées encore présentes dans le montage.
#[tauri::command]
fn detect_retakes(project: Project) -> Vec<RetakeOut> {
    let mut out = vec![];
    for (source, words) in &project.transcripts {
        for r in retakes::find_retakes(words, &Default::default()) {
            let still_kept = project
                .kept_segments()
                .any(|s| &s.source == source && s.start < r.end && s.end > r.start);
            if still_kept {
                out.push(RetakeOut { source: source.clone(), retake: r });
            }
        }
    }
    out
}

#[derive(serde::Deserialize)]
struct RetakeIn {
    source: String,
    start: f64,
    end: f64,
}

#[tauri::command]
fn cut_retakes(mut project: Project, retakes: Vec<RetakeIn>) -> Result<Project, String> {
    for r in retakes {
        project.mark_range(&r.source, r.start, r.end, SegmentKind::Retake, false);
    }
    project.validate().map_err(err)?;
    Ok(project)
}

#[tauri::command]
async fn write_srt(project: Project, path: String) -> Result<usize, String> {
    blocking(move || {
        let s = text::srt(&project, intro_duration(&project));
        std::fs::write(&path, &s).map_err(err)?;
        Ok(s.matches(" --> ").count())
    })
    .await
}

#[tauri::command]
async fn chapters(project: Project) -> Result<text::Chapters, String> {
    blocking(move || Ok(text::chapters(&project, intro_duration(&project)))).await
}

// ------------------------------------------------------------------ export

/// Exporte le MP4 ; la progression arrive dans l'événement `export-progress` (0–1).
#[tauri::command]
async fn export_video(app: AppHandle, project: Project, output: String) -> Result<Vec<String>, String> {
    blocking(move || {
        let job = render::build(&project, &PathBuf::from(output)).map_err(err)?;
        render::run(&job, |p| {
            let _ = app.emit("export-progress", p);
        })
        .map_err(err)?;
        Ok(job.warnings)
    })
    .await
}

/// Fichiers passés au lancement (« Ouvrir avec », ligne de commande).
#[tauri::command]
fn startup_paths() -> Vec<String> {
    std::env::args().skip(1).filter(|a| !a.starts_with('-')).collect()
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![
            import_rushes,
            append_rushes,
            load_project,
            save_project,
            project_mtime,
            mark_range,
            models_status,
            download_model,
            transcribe_rushes,
            cancel_transcription,
            transcript,
            detect_retakes,
            cut_retakes,
            write_srt,
            chapters,
            export_video,
            startup_paths
        ])
        .run(tauri::generate_context!())
        .expect("erreur au lancement de Derush");
}
