//! Tout ce qui part de la transcription : vue par phrases (pour l'UI et Claude),
//! sous-titres SRT et chapitres YouTube.

use serde::Serialize;

use crate::model::*;
use crate::retakes;

/// Une phrase de la transcription, avec sa place dans le montage.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SentenceView {
    pub source: String,
    /// Temps du rush.
    pub start: f64,
    pub end: f64,
    pub text: String,
    /// Temps dans la vidéo montée (hors intro) ; `None` si la phrase est entièrement coupée.
    pub program: Option<f64>,
    /// Part de la phrase gardée au montage (0 = coupée, 1 = gardée en entier).
    pub kept: f64,
}

fn kept_overlap(project: &Project, source: &str, start: f64, end: f64) -> f64 {
    project
        .kept_segments()
        .filter(|s| s.source == source)
        .map(|s| (s.end.min(end) - s.start.max(start)).max(0.0))
        .sum()
}

pub fn sentences(project: &Project) -> Vec<SentenceView> {
    let mut out = Vec::new();
    for src in &project.sources {
        let Some(words) = project.transcripts.get(&src.id) else { continue };
        for s in retakes::sentences(words, 0.8) {
            let len = (s.end - s.start).max(1e-3);
            let kept = (kept_overlap(project, &src.id, s.start, s.end) / len).clamp(0.0, 1.0);
            let program = (kept > 0.0)
                .then(|| project.source_to_program(&Anchor { source: src.id.clone(), time: s.start }))
                .flatten();
            out.push(SentenceView { source: src.id.clone(), start: s.start, end: s.end, text: s.text, program, kept });
        }
    }
    out
}

/// Mots gardés au montage, en temps de la vidéo montée.
pub fn program_words(project: &Project) -> Vec<Word> {
    let mut out = Vec::new();
    let mut acc = 0.0;
    for seg in project.kept_segments() {
        if let Some(words) = project.transcripts.get(&seg.source) {
            for w in words {
                let mid = (w.start + w.end) / 2.0;
                if mid >= seg.start && mid < seg.end {
                    out.push(Word {
                        text: w.text.clone(),
                        start: acc + (w.start.max(seg.start) - seg.start),
                        end: acc + (w.end.min(seg.end) - seg.start),
                    });
                }
            }
        }
        acc += seg.len();
    }
    out
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Cue {
    pub start: f64,
    pub end: f64,
    pub text: String,
}

/// Regroupe les mots en sous-titres : au plus `max_chars` caractères et `max_dur` secondes,
/// coupure aux fins de phrase et aux pauses.
pub fn cues(words: &[Word], max_chars: usize, max_dur: f64) -> Vec<Cue> {
    let mut out: Vec<Cue> = Vec::new();
    let mut cur: Option<Cue> = None;
    let mut last_end = f64::NEG_INFINITY;
    let mut ended_sentence = false;
    for w in words {
        let start_new = match &cur {
            None => true,
            Some(c) => {
                ended_sentence
                    || w.start - last_end > 0.6
                    || c.text.chars().count() + 1 + w.text.chars().count() > max_chars
                    || w.end - c.start > max_dur
            }
        };
        if start_new {
            if let Some(c) = cur.take() {
                out.push(c);
            }
            cur = Some(Cue { start: w.start, end: w.end, text: w.text.clone() });
        } else if let Some(c) = cur.as_mut() {
            c.text.push(' ');
            c.text.push_str(&w.text);
            c.end = w.end;
        }
        last_end = w.end;
        ended_sentence = w.text.ends_with(['.', '?', '!', '…']);
    }
    out.extend(cur);
    // Un sous-titre reste affiché au moins 0,8 s, sans empiéter sur le suivant.
    for i in 0..out.len() {
        let next = out.get(i + 1).map_or(f64::INFINITY, |n| n.start);
        let c = &mut out[i];
        c.end = c.end.max(c.start + 0.8).min(next - 0.02).max(c.start + 0.1);
    }
    out
}

fn srt_time(t: f64) -> String {
    let ms = (t.max(0.0) * 1000.0).round() as u64;
    format!("{:02}:{:02}:{:02},{:03}", ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000)
}

/// Sous-titres de la vidéo exportée. `offset` = durée de l'intro.
pub fn srt(project: &Project, offset: f64) -> String {
    let mut s = String::new();
    for (i, c) in cues(&program_words(project), 42, 4.0).iter().enumerate() {
        s += &format!("{}\n{} --> {}\n{}\n\n", i + 1, srt_time(c.start + offset), srt_time(c.end + offset), c.text);
    }
    s
}

fn yt_time(t: f64) -> String {
    let t = t.max(0.0) as u64;
    if t >= 3600 {
        format!("{}:{:02}:{:02}", t / 3600, t / 60 % 60, t % 60)
    } else {
        format!("{}:{:02}", t / 60, t % 60)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Chapters {
    /// Texte à coller dans la description YouTube.
    pub text: String,
    /// Règles YouTube non respectées (les chapitres ne s'afficheront pas).
    pub warnings: Vec<String>,
}

/// Chapitres YouTube à partir des titres du montage. `intro` = durée de l'intro.
pub fn chapters(project: &Project, intro: f64) -> Chapters {
    let mut list: Vec<(f64, String)> = project
        .overlays
        .iter()
        .filter_map(|o| match &o.content {
            OverlayContent::Title { text, .. } => Some((
                project.source_to_program(&o.anchor)? + intro,
                text.split_whitespace().collect::<Vec<_>>().join(" "),
            )),
            _ => None,
        })
        .collect();
    list.sort_by(|a, b| a.0.total_cmp(&b.0));
    if list.first().map_or(true, |(t, _)| *t >= 1.0) {
        let name = if intro > 0.0 { "Intro" } else { "Introduction" };
        list.insert(0, (0.0, name.into()));
    } else {
        list[0].0 = 0.0;
    }
    let total = project.program_duration() + intro;
    let mut warnings = vec![];
    if list.len() < 3 {
        warnings.push("YouTube demande au moins 3 chapitres : ajoute des titres.".into());
    }
    for (i, (t, name)) in list.iter().enumerate() {
        let next = list.get(i + 1).map_or(total, |n| n.0);
        if next - t < 10.0 {
            warnings.push(format!("« {name} » dure moins de 10 s : YouTube l'ignorera."));
        }
    }
    let text = list.iter().map(|(t, n)| format!("{} {n}", yt_time(*t))).collect::<Vec<_>>().join("\n");
    Chapters { text, warnings }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(text: &str, start: f64, end: f64) -> Word {
        Word { text: text.into(), start, end }
    }

    fn project() -> Project {
        let mut p = Project::new(
            vec![Source { id: "a".into(), path: "a.mp4".into(), duration: 10.0, width: None, height: None, fps: None }],
            vec![
                Segment { id: String::new(), source: "a".into(), start: 0.0, end: 3.0, kind: SegmentKind::Speech, keep: true },
                Segment { id: String::new(), source: "a".into(), start: 3.0, end: 5.0, kind: SegmentKind::Retake, keep: false },
                Segment { id: String::new(), source: "a".into(), start: 5.0, end: 10.0, kind: SegmentKind::Speech, keep: true },
            ],
        );
        p.transcripts.insert(
            "a".into(),
            vec![
                w("Bonjour", 0.2, 0.6),
                w("à", 0.7, 0.8),
                w("tous.", 0.9, 1.3),
                w("Raté", 3.2, 3.6),
                w("ici.", 3.7, 4.0),
                w("Voici", 5.2, 5.6),
                w("le", 5.7, 5.8),
                w("lin.", 5.9, 6.4),
            ],
        );
        p
    }

    #[test]
    fn program_words_skip_cuts() {
        let words = program_words(&project());
        let t: Vec<_> = words.iter().map(|w| w.text.as_str()).collect();
        assert_eq!(t, ["Bonjour", "à", "tous.", "Voici", "le", "lin."]);
        assert!((words[3].start - 3.2).abs() < 1e-9); // 3 s gardées avant, puis 0,2 s
    }

    #[test]
    fn sentence_view_reports_cut() {
        let v = sentences(&project());
        assert_eq!(v.len(), 3);
        assert_eq!(v[1].kept, 0.0);
        assert_eq!(v[1].program, None);
        assert_eq!(v[2].program, Some(3.2));
    }

    #[test]
    fn srt_format_and_offset() {
        let s = srt(&project(), 2.0);
        assert!(s.starts_with("1\n00:00:02,200 --> "), "{s}");
        assert!(s.contains("Voici le lin."));
        assert!(!s.contains("Raté"));
    }

    #[test]
    fn cues_split_on_length() {
        let words: Vec<Word> = (0..20).map(|i| w("mot", i as f64 * 0.3, i as f64 * 0.3 + 0.25)).collect();
        let c = cues(&words, 12, 10.0);
        assert!(c.iter().all(|c| c.text.len() <= 12));
        assert_eq!(c.len(), 7);
    }

    #[test]
    fn chapters_from_titles() {
        let mut p = project();
        p.sources[0].duration = 100.0;
        p.segments.last_mut().unwrap().end = 100.0;
        for (i, (t, name)) in [(30.0, "Le lin"), (60.0, "La viscose")].iter().enumerate() {
            p.overlays.push(Overlay {
                id: format!("t{i}"),
                anchor: Anchor { source: "a".into(), time: *t },
                duration: 3.0,
                content: OverlayContent::Title { text: (*name).into(), style: TitleStyle::Lower },
            });
        }
        let c = chapters(&p, 0.0);
        assert_eq!(c.text, "0:00 Introduction\n0:28 Le lin\n0:58 La viscose");
        assert!(c.warnings.is_empty(), "{:?}", c.warnings);
    }
}
