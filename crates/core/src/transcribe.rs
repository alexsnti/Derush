//! Transcription locale avec whisper.cpp.
//!
//! Le modèle par défaut, `large-v3-turbo` quantifié, est bon en français et tourne vite
//! sur une puce M1 grâce à Metal. Les mots sont horodatés (≈ ±0,1 s), ce qui suffit pour
//! repérer les prises refaites, générer les sous-titres et laisser Claude naviguer dans la vidéo.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde::Serialize;

use crate::model::Word;
use crate::{media, paths, Error, Result};

#[derive(Debug, Clone, Serialize)]
pub struct ModelInfo {
    pub id: &'static str,
    pub label: &'static str,
    pub file: &'static str,
    pub size_mb: u32,
    pub installed: bool,
    pub path: PathBuf,
}

const BASE_URL: &str = "https://huggingface.co/ggerganov/whisper.cpp/resolve/main";

/// (id, libellé, fichier, taille en Mo)
const MODELS: &[(&str, &str, &str, u32)] = &[
    ("turbo", "Précis (recommandé)", "ggml-large-v3-turbo-q5_0.bin", 547),
    ("small", "Rapide", "ggml-small-q5_1.bin", 181),
];

pub const DEFAULT_MODEL: &str = "turbo";

pub fn models() -> Vec<ModelInfo> {
    MODELS
        .iter()
        .map(|&(id, label, file, size_mb)| {
            let path = paths::models_dir().join(file);
            ModelInfo { id, label, file, size_mb, installed: path.exists(), path }
        })
        .collect()
}

pub fn model(id: &str) -> Result<ModelInfo> {
    models()
        .into_iter()
        .find(|m| m.id == id)
        .ok_or_else(|| Error::Invalid(format!("modèle inconnu : {id}")))
}

/// Chemin du premier modèle installé, en commençant par celui demandé.
pub fn installed_model(preferred: &str) -> Option<PathBuf> {
    let all = models();
    all.iter()
        .find(|m| m.id == preferred && m.installed)
        .or_else(|| all.iter().find(|m| m.installed))
        .map(|m| m.path.clone())
}

/// Télécharge un modèle ; `progress` reçoit une valeur entre 0 et 1.
pub fn download_model(id: &str, mut progress: impl FnMut(f64)) -> Result<PathBuf> {
    let m = model(id)?;
    if m.installed {
        return Ok(m.path);
    }
    std::fs::create_dir_all(paths::models_dir())?;
    let url = format!("{BASE_URL}/{}", m.file);
    let resp = ureq::get(&url).call().map_err(|e| Error::Tool(format!("téléchargement de {url} : {e}")))?;
    let body = resp.into_body();
    let total = body.content_length().unwrap_or(m.size_mb as u64 * 1_000_000) as f64;
    let mut reader = body.into_reader();
    let part = m.path.with_extension("part");
    let mut out = std::fs::File::create(&part)?;
    let mut buf = vec![0u8; 256 * 1024];
    let mut done = 0u64;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n])?;
        done += n as u64;
        progress((done as f64 / total).min(1.0));
    }
    out.sync_all()?;
    std::fs::rename(&part, &m.path)?;
    Ok(m.path)
}

/// Audio du fichier en mono 16 kHz, valeurs flottantes, comme l'attend Whisper.
pub fn load_audio(path: &Path) -> Result<Vec<f32>> {
    let mut out = Vec::new();
    media::stream_pcm(path, |s| out.extend(s.iter().map(|&x| x as f32 / 32768.0)))?;
    Ok(out)
}

#[derive(Debug, Clone)]
pub struct Options {
    pub model: PathBuf,
    /// Code langue (« fr »), ou `None` pour la détection automatique.
    pub language: Option<String>,
    /// Vocabulaire qui aide Whisper (noms de tissus, de marques…).
    pub prompt: Option<String>,
}

/// Regroupe les morceaux renvoyés par Whisper en mots (un morceau sans espace
/// en tête prolonge le mot précédent : ponctuation, fin de mot découpée).
pub(crate) fn merge_pieces(pieces: impl IntoIterator<Item = (String, f64, f64)>) -> Vec<Word> {
    let mut words: Vec<Word> = Vec::new();
    for (raw, start, end) in pieces {
        let text = raw.trim();
        if text.is_empty() {
            continue;
        }
        // Annotations non parlées : [Musique], (rires)…
        if (text.starts_with('[') && text.ends_with(']')) || (text.starts_with('(') && text.ends_with(')')) {
            continue;
        }
        let glue = !raw.starts_with(' ') || text.chars().all(|c| c.is_ascii_punctuation() || c == '…');
        match words.last_mut() {
            Some(last) if glue => {
                last.text.push_str(text);
                last.end = last.end.max(end);
            }
            _ => words.push(Word { text: text.to_string(), start, end: end.max(start) }),
        }
    }
    words
}

#[cfg(feature = "transcribe")]
pub fn transcribe_samples(
    samples: &[f32],
    opts: &Options,
    progress: impl FnMut(f64) + 'static,
    cancel: Arc<AtomicBool>,
) -> Result<Vec<Word>> {
    use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

    whisper_rs::install_logging_hooks(); // redirige les journaux de whisper.cpp (sinon très bavards)
    let wcx = WhisperContext::new_with_params(&opts.model, WhisperContextParameters::default())
        .map_err(|e| Error::Tool(format!("chargement du modèle {} : {e}", opts.model.display())))?;
    let mut state = wcx.create_state().map_err(|e| Error::Tool(format!("whisper : {e}")))?;

    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 3 });
    params.set_language(opts.language.as_deref());
    if let Some(p) = &opts.prompt {
        params.set_initial_prompt(p);
    }
    // Un segment par mot, avec horodatage.
    params.set_token_timestamps(true);
    params.set_split_on_word(true);
    params.set_max_len(1);
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_special(false);
    params.set_print_timestamps(false);
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).min(8) as i32;
    params.set_n_threads(threads);
    let mut progress = progress;
    params.set_progress_callback_safe(move |pct: i32| progress(pct as f64 / 100.0));
    params.set_abort_callback_safe(move || cancel.load(Ordering::Relaxed));

    state.full(params, samples).map_err(|e| Error::Tool(format!("transcription : {e}")))?;

    let pieces = state.as_iter().filter_map(|seg| {
        let text = seg.to_str_lossy().ok()?.into_owned();
        Some((text, seg.start_timestamp() as f64 / 100.0, seg.end_timestamp() as f64 / 100.0))
    });
    Ok(merge_pieces(pieces.collect::<Vec<_>>()))
}

#[cfg(not(feature = "transcribe"))]
pub fn transcribe_samples(
    _: &[f32],
    _: &Options,
    _: impl FnMut(f64) + 'static,
    _: Arc<AtomicBool>,
) -> Result<Vec<Word>> {
    Err(Error::Invalid("Derush a été compilé sans la transcription (feature « transcribe »)".into()))
}

/// Transcrit un fichier ; `progress` va de 0 (décodage) à 1.
pub fn transcribe_file(
    path: &Path,
    opts: &Options,
    progress: impl FnMut(f64) + Send + 'static,
    cancel: Arc<AtomicBool>,
) -> Result<Vec<Word>> {
    let shared = Arc::new(std::sync::Mutex::new(progress));
    let p = shared.clone();
    (p.lock().expect("progress"))(0.0);
    let samples = load_audio(path)?;
    let p = shared.clone();
    transcribe_samples(&samples, opts, move |x| (p.lock().expect("progress"))(0.05 + 0.95 * x), cancel)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pieces_are_merged_into_words() {
        let w = merge_pieces([
            (" Bonjour".to_string(), 0.0, 0.4),
            (",".to_string(), 0.4, 0.45),
            (" aujourd".to_string(), 0.5, 0.8),
            ("'hui".to_string(), 0.8, 1.0),
            (" [Musique]".to_string(), 1.0, 2.0),
            (" lin".to_string(), 2.0, 2.3),
            (" .".to_string(), 2.3, 2.35),
        ]);
        let t: Vec<_> = w.iter().map(|w| w.text.as_str()).collect();
        assert_eq!(t, ["Bonjour,", "aujourd'hui", "lin."]);
        assert_eq!(w[1].end, 1.0);
    }

    /// Test avec un vrai modèle : `DERUSH_TEST_MODEL=/chemin/ggml-*.bin DERUSH_TEST_AUDIO=fichier cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn real_model() {
        let (Some(model), Some(audio)) = (std::env::var_os("DERUSH_TEST_MODEL"), std::env::var_os("DERUSH_TEST_AUDIO"))
        else {
            return;
        };
        let opts = Options { model: model.into(), language: Some("fr".into()), prompt: None };
        let words = transcribe_file(Path::new(&audio), &opts, |_| {}, Arc::new(AtomicBool::new(false))).unwrap();
        eprintln!("{} mots : {:?}", words.len(), &words[..words.len().min(20)]);
        assert!(!words.is_empty());
    }
}
