//! Modèle de projet.
//!
//! Principe : la vidéo finale est la suite des segments `keep == true`, dans l'ordre
//! des rushes puis du temps. Les calques (titres, images) sont ancrés sur un instant
//! d'un rush (et non sur un temps de la vidéo finale) : ils restent au bon endroit
//! quand on ajoute ou retire des coupes ailleurs.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::{Error, Result};

pub const PROJECT_VERSION: u32 = 1;
/// En dessous, un segment est considéré comme vide.
pub const EPS: f64 = 1e-3;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Project {
    pub version: u32,
    pub sources: Vec<Source>,
    pub segments: Vec<Segment>,
    #[serde(default)]
    pub overlays: Vec<Overlay>,
    #[serde(default)]
    pub music: Option<Music>,
    #[serde(default)]
    pub intro: Option<PathBuf>,
    #[serde(default)]
    pub outro: Option<PathBuf>,
    #[serde(default)]
    pub export: ExportSettings,
    /// Transcription horodatée au mot, par rush (temps du rush).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub transcripts: BTreeMap<String, Vec<Word>>,
}

/// Un mot transcrit, en temps du rush.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Word {
    pub text: String,
    pub start: f64,
    pub end: f64,
}

/// Un rush importé.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Source {
    pub id: String,
    pub path: PathBuf,
    /// Durée en secondes.
    pub duration: f64,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
    #[serde(default)]
    pub fps: Option<f64>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SegmentKind {
    /// Parole détectée.
    Speech,
    /// Blanc détecté.
    Silence,
    /// Prise ratée ou refaite.
    Retake,
    /// Coupe faite à la main.
    Manual,
}

/// Une plage d'un rush, gardée ou coupée.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Segment {
    pub id: String,
    pub source: String,
    pub start: f64,
    pub end: f64,
    pub kind: SegmentKind,
    pub keep: bool,
}

impl Segment {
    pub fn len(&self) -> f64 {
        self.end - self.start
    }
    pub fn is_empty(&self) -> bool {
        self.len() < EPS
    }
}

/// Instant d'un rush auquel un calque est accroché.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Anchor {
    pub source: String,
    pub time: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Overlay {
    pub id: String,
    pub anchor: Anchor,
    /// Durée d'affichage en secondes (temps de la vidéo finale).
    pub duration: f64,
    pub content: OverlayContent,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum OverlayContent {
    Title { text: String, style: TitleStyle },
    Image {
        path: PathBuf,
        position: Position,
        /// Largeur de l'image en fraction de la largeur de la vidéo (0.0–1.0).
        scale: f64,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TitleStyle {
    /// Bandeau en bas à gauche.
    Lower,
    /// Texte centré.
    Center,
    /// Gros titre centré.
    Big,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Position {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
    Center,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Music {
    pub path: PathBuf,
    /// Gain appliqué à la musique, en dB (négatif = plus bas).
    pub volume_db: f64,
    /// Baisse automatique de la musique quand elle parle.
    pub duck: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ExportSettings {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// `h264_videotoolbox` sur Mac (encodeur matériel), `libx264` ailleurs.
    pub video_encoder: String,
    /// Débit pour les encodeurs matériels, ex. "12M".
    pub video_bitrate: String,
    /// Qualité pour libx264.
    pub crf: u8,
    /// Cible de loudness YouTube.
    pub loudness_lufs: f64,
    /// Fondu audio à chaque coupe, pour éviter les clics.
    pub cut_fade_ms: u32,
    /// Police des titres. Par défaut : police système.
    pub font: Option<PathBuf>,
}

impl Default for ExportSettings {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            fps: 30,
            video_encoder: if cfg!(target_os = "macos") {
                "h264_videotoolbox".into()
            } else {
                "libx264".into()
            },
            video_bitrate: "12M".into(),
            crf: 20,
            loudness_lufs: -14.0,
            cut_fade_ms: 10,
            font: None,
        }
    }
}

impl Project {
    pub fn new(sources: Vec<Source>, segments: Vec<Segment>) -> Self {
        let mut p = Self {
            version: PROJECT_VERSION,
            sources,
            segments,
            overlays: vec![],
            music: None,
            intro: None,
            outro: None,
            export: ExportSettings::default(),
            transcripts: BTreeMap::new(),
        };
        p.normalize();
        p
    }

    pub fn load(path: &std::path::Path) -> Result<Self> {
        let p: Project = serde_json::from_str(&std::fs::read_to_string(path)?)?;
        p.validate()?;
        Ok(p)
    }

    /// Écriture atomique (fichier temporaire puis renommage) : l'app et le serveur MCP
    /// peuvent lire le même projet sans jamais tomber sur un fichier à moitié écrit.
    pub fn save(&self, path: &std::path::Path) -> Result<()> {
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn source(&self, id: &str) -> Option<&Source> {
        self.sources.iter().find(|s| s.id == id)
    }

    fn source_index(&self, id: &str) -> usize {
        self.sources
            .iter()
            .position(|s| s.id == id)
            .unwrap_or(usize::MAX)
    }

    /// Trie les segments (ordre des rushes puis du temps), retire les vides,
    /// fusionne les voisins identiques et renumérote les identifiants.
    pub fn normalize(&mut self) {
        let mut segs = std::mem::take(&mut self.segments);
        segs.retain(|s| !s.is_empty());
        segs.sort_by(|a, b| {
            (self.source_index(&a.source), a.start)
                .partial_cmp(&(self.source_index(&b.source), b.start))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let mut out: Vec<Segment> = Vec::with_capacity(segs.len());
        for s in segs {
            if let Some(last) = out.last_mut() {
                if last.source == s.source
                    && last.kind == s.kind
                    && last.keep == s.keep
                    && (s.start - last.end).abs() < EPS
                {
                    last.end = s.end;
                    continue;
                }
            }
            out.push(s);
        }
        for (i, s) in out.iter_mut().enumerate() {
            s.id = format!("seg-{i}");
        }
        self.segments = out;
    }

    /// Applique `kind`/`keep` à la plage [start, end] d'un rush, en découpant
    /// les segments existants si besoin.
    pub fn mark_range(&mut self, source: &str, start: f64, end: f64, kind: SegmentKind, keep: bool) {
        if end - start < EPS {
            return;
        }
        let mut out = Vec::with_capacity(self.segments.len() + 2);
        for seg in self.segments.drain(..) {
            if seg.source != source || seg.end <= start || seg.start >= end {
                out.push(seg);
                continue;
            }
            if seg.start < start {
                out.push(Segment { end: start, ..seg.clone() });
            }
            out.push(Segment {
                start: seg.start.max(start),
                end: seg.end.min(end),
                kind,
                keep,
                ..seg.clone()
            });
            if seg.end > end {
                out.push(Segment { start: end, ..seg });
            }
        }
        self.segments = out;
        self.normalize();
    }

    pub fn kept_segments(&self) -> impl Iterator<Item = &Segment> {
        self.segments.iter().filter(|s| s.keep)
    }

    /// Durée de la partie montée (sans intro ni outro).
    pub fn program_duration(&self) -> f64 {
        self.kept_segments().map(Segment::len).sum()
    }

    /// Position d'un instant de rush dans la vidéo montée (sans intro).
    /// Si l'instant tombe dans une partie coupée, renvoie le début de la partie gardée suivante.
    pub fn source_to_program(&self, anchor: &Anchor) -> Option<f64> {
        let target = self.source_index(&anchor.source);
        let mut acc = 0.0;
        for seg in self.kept_segments() {
            let idx = self.source_index(&seg.source);
            if idx > target || (idx == target && seg.start > anchor.time) {
                return Some(acc);
            }
            if idx == target && anchor.time < seg.end {
                return Some(acc + (anchor.time - seg.start));
            }
            acc += seg.len();
        }
        None
    }

    /// Inverse de [`Self::source_to_program`] : instant du rush affiché à `t` dans la vidéo montée.
    pub fn program_to_source(&self, t: f64) -> Option<Anchor> {
        let mut acc = 0.0;
        let mut last = None;
        for seg in self.kept_segments() {
            if t < acc + seg.len() {
                return Some(Anchor { source: seg.source.clone(), time: seg.start + (t - acc).max(0.0) });
            }
            acc += seg.len();
            last = Some(seg);
        }
        // Tout à la fin : dernier instant gardé.
        last.filter(|_| t - acc < 0.5).map(|s| Anchor { source: s.source.clone(), time: s.end - 0.01 })
    }

    pub fn validate(&self) -> Result<()> {
        if self.version != PROJECT_VERSION {
            return Err(Error::Invalid(format!("version {} non gérée", self.version)));
        }
        for (i, s) in self.sources.iter().enumerate() {
            if self.sources[..i].iter().any(|o| o.id == s.id) {
                return Err(Error::Invalid(format!("rush en double : {}", s.id)));
            }
        }
        for seg in &self.segments {
            let src = self
                .source(&seg.source)
                .ok_or_else(|| Error::Invalid(format!("{} : rush inconnu {}", seg.id, seg.source)))?;
            if seg.start < -EPS || seg.end > src.duration + 0.05 || seg.start >= seg.end {
                return Err(Error::Invalid(format!(
                    "{} : plage {:.3}–{:.3} hors du rush ({:.3} s)",
                    seg.id, seg.start, seg.end, src.duration
                )));
            }
        }
        for w in self.segments.windows(2) {
            if w[0].source == w[1].source && w[1].start < w[0].end - EPS {
                return Err(Error::Invalid(format!("{} et {} se chevauchent", w[0].id, w[1].id)));
            }
        }
        for o in &self.overlays {
            if self.source(&o.anchor.source).is_none() {
                return Err(Error::Invalid(format!("{} : rush inconnu", o.id)));
            }
            if o.duration <= 0.0 {
                return Err(Error::Invalid(format!("{} : durée nulle", o.id)));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(source: &str, start: f64, end: f64, kind: SegmentKind, keep: bool) -> Segment {
        Segment { id: String::new(), source: source.into(), start, end, kind, keep }
    }

    fn project() -> Project {
        let src = |id: &str, d: f64| Source {
            id: id.into(),
            path: format!("{id}.mp4").into(),
            duration: d,
            width: None,
            height: None,
            fps: None,
        };
        Project::new(
            vec![src("a", 10.0), src("b", 5.0)],
            vec![
                seg("a", 0.0, 2.0, SegmentKind::Silence, false),
                seg("a", 2.0, 6.0, SegmentKind::Speech, true),
                seg("a", 6.0, 10.0, SegmentKind::Speech, true),
                seg("b", 0.0, 5.0, SegmentKind::Speech, true),
            ],
        )
    }

    #[test]
    fn normalize_merges_neighbours() {
        let p = project();
        assert_eq!(p.segments.len(), 3);
        assert_eq!(p.segments[1].end, 10.0);
        assert_eq!(p.program_duration(), 13.0);
        p.validate().unwrap();
    }

    #[test]
    fn mark_range_splits() {
        let mut p = project();
        p.mark_range("a", 4.0, 5.0, SegmentKind::Retake, false);
        let a: Vec<_> = p.segments.iter().filter(|s| s.source == "a").collect();
        assert_eq!(a.len(), 4);
        assert_eq!((a[2].start, a[2].end, a[2].keep), (4.0, 5.0, false));
        assert_eq!(p.program_duration(), 12.0);
        p.validate().unwrap();
    }

    #[test]
    fn program_time_mapping() {
        let mut p = project();
        p.mark_range("a", 4.0, 5.0, SegmentKind::Manual, false);
        let at = |s: &str, t: f64| p.source_to_program(&Anchor { source: s.into(), time: t });
        assert_eq!(at("a", 1.0), Some(0.0)); // dans un blanc coupé → début du segment suivant
        assert_eq!(at("a", 3.0), Some(1.0));
        assert_eq!(at("a", 4.5), Some(2.0)); // dans la coupe
        assert_eq!(at("a", 6.0), Some(3.0));
        assert_eq!(at("b", 1.0), Some(8.0));
        assert_eq!(at("b", 9.0), None);

        let back = |t: f64| p.program_to_source(t).map(|a| (a.source, a.time));
        assert_eq!(back(1.0), Some(("a".into(), 3.0)));
        assert_eq!(back(2.5), Some(("a".into(), 5.5)));
        assert_eq!(back(8.0), Some(("b".into(), 1.0)));
        assert_eq!(back(30.0), None);
    }

    #[test]
    fn json_roundtrip() {
        let mut p = project();
        p.overlays.push(Overlay {
            id: "t1".into(),
            anchor: Anchor { source: "a".into(), time: 3.0 },
            duration: 3.0,
            content: OverlayContent::Title { text: "Mon haul de lin".into(), style: TitleStyle::Lower },
        });
        let json = serde_json::to_string(&p).unwrap();
        assert!(json.contains("\"type\":\"title\""));
        let back: Project = serde_json::from_str(&json).unwrap();
        assert_eq!(back, p);
    }
}
