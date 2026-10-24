//! Détection des silences.
//!
//! V1 volontairement simple et robuste : niveau RMS par trame de 10 ms, seuil adaptatif
//! calé entre le bruit de fond et le niveau de parole du rush, hystérésis par durées
//! minimales. Le VAD Silero (plus robuste à la machine à coudre ou à la musique) viendra
//! remplacer `is_voiced` sans toucher au reste.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::media::{self, ANALYSIS_SAMPLE_RATE};
use crate::model::{Segment, SegmentKind, Source};
use crate::Result;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SilenceParams {
    /// Taille d'une trame d'analyse.
    pub frame_ms: u32,
    /// Seuil fixe en dBFS. `None` : seuil adaptatif.
    pub threshold_db: Option<f32>,
    /// Position du seuil adaptatif entre bruit de fond (0) et parole (1).
    pub adaptive_ratio: f32,
    /// Un blanc plus court n'est pas coupé (respirations, pauses naturelles).
    pub min_silence: f64,
    /// Marge gardée autour de la parole, pour ne pas manger les attaques et fins de mots.
    pub padding: f64,
    /// Un bruit isolé plus court est traité comme du silence (clic, toux brève).
    pub min_speech: f64,
}

impl Default for SilenceParams {
    fn default() -> Self {
        Self {
            frame_ms: 10,
            threshold_db: None,
            adaptive_ratio: 0.3,
            min_silence: 0.6,
            padding: 0.15,
            min_speech: 0.2,
        }
    }
}

/// Niveau sonore du rush, une valeur en dBFS par trame. Sert aussi à dessiner la forme d'onde.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Levels {
    pub frame_s: f64,
    pub db: Vec<f32>,
}

impl Levels {
    /// Version réduite pour l'affichage (`per_second` points par seconde, maximum par bloc).
    pub fn downsample(&self, per_second: u32) -> Vec<f32> {
        let step = ((1.0 / per_second as f64) / self.frame_s).round().max(1.0) as usize;
        self.db
            .chunks(step)
            .map(|c| c.iter().copied().fold(f32::NEG_INFINITY, f32::max))
            .collect()
    }
}

pub const SILENCE_DB: f32 = -90.0;

fn rms_db(frame: &[i16]) -> f32 {
    if frame.is_empty() {
        return SILENCE_DB;
    }
    let sum: f64 = frame.iter().map(|&s| (s as f64 / 32768.0).powi(2)).sum();
    let rms = (sum / frame.len() as f64).sqrt();
    if rms <= 0.0 {
        SILENCE_DB
    } else {
        (20.0 * rms.log10()).max(SILENCE_DB as f64) as f32
    }
}

pub fn levels_from_samples(samples: &[i16], sample_rate: u32, frame_ms: u32) -> Levels {
    let frame = (sample_rate as usize * frame_ms as usize / 1000).max(1);
    Levels {
        frame_s: frame as f64 / sample_rate as f64,
        db: samples.chunks(frame).map(rms_db).collect(),
    }
}

/// Calcule les niveaux d'un fichier en flux.
pub fn levels_from_file(path: &Path, frame_ms: u32) -> Result<Levels> {
    let frame = (ANALYSIS_SAMPLE_RATE as usize * frame_ms as usize / 1000).max(1);
    let mut db = Vec::new();
    let mut pending: Vec<i16> = Vec::with_capacity(frame);
    media::stream_pcm(path, |samples| {
        let mut rest = samples;
        while !rest.is_empty() {
            let take = (frame - pending.len()).min(rest.len());
            pending.extend_from_slice(&rest[..take]);
            rest = &rest[take..];
            if pending.len() == frame {
                db.push(rms_db(&pending));
                pending.clear();
            }
        }
    })?;
    if !pending.is_empty() {
        db.push(rms_db(&pending));
    }
    Ok(Levels { frame_s: frame as f64 / ANALYSIS_SAMPLE_RATE as f64, db })
}

fn percentile(values: &[f32], p: f32) -> f32 {
    if values.is_empty() {
        return SILENCE_DB;
    }
    let mut v = values.to_vec();
    v.sort_by(|a, b| a.total_cmp(b));
    v[((v.len() - 1) as f32 * p).round() as usize]
}

/// Seuil utilisé : fixe si demandé, sinon entre le bruit de fond (10e centile)
/// et la parole (95e centile).
pub fn threshold(levels: &Levels, params: &SilenceParams) -> f32 {
    if let Some(t) = params.threshold_db {
        return t;
    }
    let floor = percentile(&levels.db, 0.10);
    let speech = percentile(&levels.db, 0.95);
    // Au moins 6 dB au-dessus du bruit, même sur un rush très comprimé.
    (floor + (speech - floor) * params.adaptive_ratio).max(floor + 6.0)
}

/// Plages de silence à couper, en secondes, triées.
pub fn detect_silences(levels: &Levels, duration: f64, params: &SilenceParams) -> Vec<(f64, f64)> {
    let thr = threshold(levels, params);
    let mut voiced: Vec<bool> = levels.db.iter().map(|&d| d > thr).collect();

    // 1. Les bruits trop courts deviennent du silence.
    let min_speech = (params.min_speech / levels.frame_s).round() as usize;
    for (start, end) in runs(&voiced, true) {
        if end - start < min_speech {
            voiced[start..end].iter_mut().for_each(|v| *v = false);
        }
    }

    // 2. Les blancs assez longs sont coupés, moins la marge autour de la parole.
    let min_silence = (params.min_silence / levels.frame_s).round() as usize;
    let n = voiced.len();
    let mut out = Vec::new();
    for (start, end) in runs(&voiced, false) {
        if end - start < min_silence {
            continue;
        }
        let mut a = start as f64 * levels.frame_s;
        let mut b = (end as f64 * levels.frame_s).min(duration);
        if start > 0 {
            a += params.padding;
        }
        if end < n {
            b -= params.padding;
        } else {
            b = duration;
        }
        if b - a > 0.05 {
            out.push((a, b));
        }
    }
    out
}

/// Plages consécutives où `values[i] == value` (indices de trames, fin exclue).
fn runs(values: &[bool], value: bool) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut start = None;
    for (i, &v) in values.iter().enumerate() {
        match (v == value, start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                out.push((s, i));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        out.push((s, values.len()));
    }
    out
}

/// Découpe un rush en segments parole / silence (les silences sont coupés par défaut).
pub fn segments_for(source: &Source, silences: &[(f64, f64)]) -> Vec<Segment> {
    let mut out = Vec::new();
    let mut t = 0.0;
    let mk = |start: f64, end: f64, kind: SegmentKind| Segment {
        id: String::new(),
        source: source.id.clone(),
        start,
        end,
        kind,
        keep: kind == SegmentKind::Speech,
    };
    for &(a, b) in silences {
        if a > t {
            out.push(mk(t, a, SegmentKind::Speech));
        }
        out.push(mk(a, b, SegmentKind::Silence));
        t = b;
    }
    if source.duration > t {
        out.push(mk(t, source.duration, SegmentKind::Speech));
    }
    out
}

/// Résultat de l'analyse d'un rush.
pub struct Analysis {
    pub source: Source,
    pub levels: Levels,
    pub segments: Vec<Segment>,
}

/// Sonde un fichier, calcule ses niveaux et propose les coupes.
pub fn analyze_file(path: &Path, id: &str, params: &SilenceParams) -> Result<Analysis> {
    let info = media::probe(path)?;
    if !info.has_audio {
        return Err(crate::Error::Invalid(format!("{} n'a pas de piste audio", path.display())));
    }
    let source = Source {
        id: id.into(),
        path: path.to_path_buf(),
        duration: info.duration,
        width: info.width,
        height: info.height,
        fps: info.fps,
    };
    let levels = levels_from_file(path, params.frame_ms)?;
    let silences = detect_silences(&levels, source.duration, params);
    let segments = segments_for(&source, &silences);
    Ok(Analysis { source, levels, segments })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 1 s de parole, 1 s de blanc, 1 s de parole, 0,3 s de blanc, 1 s de parole.
    fn synthetic() -> Levels {
        let mut db = vec![];
        let mut push = |secs: f64, level: f32| db.extend(std::iter::repeat(level).take((secs * 100.0) as usize));
        push(1.0, -20.0);
        push(1.0, -60.0);
        push(1.0, -20.0);
        push(0.3, -60.0);
        push(1.0, -20.0);
        Levels { frame_s: 0.01, db }
    }

    #[test]
    fn cuts_long_silence_only() {
        let s = detect_silences(&synthetic(), 4.3, &SilenceParams::default());
        assert_eq!(s.len(), 1);
        let (a, b) = s[0];
        assert!((a - 1.15).abs() < 1e-6 && (b - 1.85).abs() < 1e-6, "{s:?}");
    }

    #[test]
    fn ignores_short_noise_inside_silence() {
        let mut l = synthetic();
        l.db[150] = -10.0; // clic de 10 ms au milieu du blanc
        let s = detect_silences(&l, 4.3, &SilenceParams::default());
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn edges_have_no_padding() {
        let mut l = synthetic();
        l.db.splice(0..0, std::iter::repeat(-60.0).take(100));
        let s = detect_silences(&l, 5.3, &SilenceParams::default());
        assert_eq!(s[0], (0.0, 0.85));
    }

    #[test]
    fn segments_cover_source() {
        let src = Source { id: "a".into(), path: "a.mp4".into(), duration: 4.3, width: None, height: None, fps: None };
        let segs = segments_for(&src, &[(1.15, 1.85)]);
        assert_eq!(segs.len(), 3);
        assert!(!segs[1].keep);
        assert_eq!(segs.last().unwrap().end, 4.3);
    }

    #[test]
    fn rms_of_full_scale_square_is_zero_db() {
        let l = levels_from_samples(&[32767, -32767, 32767, -32767], 4, 1000);
        assert!(l.db[0].abs() < 0.01);
    }
}
