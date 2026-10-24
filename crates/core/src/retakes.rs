//! Détection des prises refaites à partir d'une transcription horodatée au mot.
//!
//! Deux signaux :
//! - **phrase recommencée** : une phrase dont le début ressemble au début d'une des phrases
//!   qui suivent de près est une prise abandonnée ; on garde la dernière ;
//! - **marqueur explicite** : « je recommence », « non attends »… dans la phrase.
//!
//! La transcription viendra de Whisper (whisper.cpp, accéléré Metal sur M1). Ce module est
//! indépendant de la source : il prend une liste de mots horodatés.

use serde::{Deserialize, Serialize};

use crate::model::{Project, SegmentKind};

pub use crate::model::Word;

#[derive(Debug, Clone, PartialEq)]
pub struct Sentence {
    pub start: f64,
    pub end: f64,
    pub text: String,
    pub(crate) tokens: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Retake {
    pub start: f64,
    pub end: f64,
    pub text: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RetakeParams {
    /// Pause au-delà de laquelle on considère qu'une nouvelle phrase commence.
    pub sentence_gap: f64,
    /// Nombre de phrases suivantes où chercher la reprise.
    pub lookahead: usize,
    /// Similarité minimale des débuts de phrase (0–1).
    pub similarity: f64,
    /// Nombre de mots comparés en début de phrase.
    pub prefix_words: usize,
    pub markers: Vec<String>,
}

impl Default for RetakeParams {
    fn default() -> Self {
        Self {
            sentence_gap: 0.8,
            lookahead: 3,
            similarity: 0.7,
            prefix_words: 8,
            markers: [
                "je recommence",
                "on recommence",
                "je reprends",
                "on reprend",
                "je la refais",
                "on la refait",
                "non attends",
                "attends je",
                "coupe ça",
                "coupez",
            ]
            .map(String::from)
            .to_vec(),
        }
    }
}

fn tokenize(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_lowercase)
        .collect()
}

pub fn sentences(words: &[Word], gap: f64) -> Vec<Sentence> {
    let mut out: Vec<Sentence> = Vec::new();
    let mut current: Vec<&Word> = Vec::new();
    let flush = |current: &mut Vec<&Word>, out: &mut Vec<Sentence>| {
        if let (Some(first), Some(last)) = (current.first(), current.last()) {
            let text = current.iter().map(|w| w.text.trim()).collect::<Vec<_>>().join(" ");
            out.push(Sentence { start: first.start, end: last.end, tokens: tokenize(&text), text });
        }
        current.clear();
    };
    for w in words {
        if let Some(prev) = current.last() {
            if w.start - prev.end > gap {
                flush(&mut current, &mut out);
            }
        }
        current.push(w);
        if w.text.trim_end().ends_with(['.', '?', '!', '…']) {
            flush(&mut current, &mut out);
        }
    }
    flush(&mut current, &mut out);
    out
}

fn levenshtein(a: &[String], b: &[String]) -> usize {
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, x) in a.iter().enumerate() {
        let mut cur = vec![i + 1; b.len() + 1];
        for (j, y) in b.iter().enumerate() {
            cur[j + 1] = (prev[j] + usize::from(x != y)).min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        prev = cur;
    }
    prev[b.len()]
}

/// Ressemblance des débuts de deux phrases, et nombre de mots comparés.
fn prefix_similarity(a: &[String], b: &[String], max: usize) -> (f64, usize) {
    let k = a.len().min(b.len()).min(max);
    if k == 0 {
        return (0.0, 0);
    }
    let d = levenshtein(&a[..k], &b[..k]);
    (1.0 - d as f64 / k as f64, k)
}

pub fn find_retakes(words: &[Word], params: &RetakeParams) -> Vec<Retake> {
    let sents = sentences(words, params.sentence_gap);
    let markers: Vec<Vec<String>> = params.markers.iter().map(|m| tokenize(m)).collect();
    let mut out = Vec::new();

    for (i, s) in sents.iter().enumerate() {
        if let Some(m) = markers
            .iter()
            .find(|m| !m.is_empty() && s.tokens.windows(m.len()).any(|w| w == m.as_slice()))
        {
            out.push(Retake {
                start: s.start,
                end: s.end,
                text: s.text.clone(),
                reason: format!("marqueur « {} »", m.join(" ")),
            });
            continue;
        }
        for next in sents.iter().skip(i + 1).take(params.lookahead) {
            let (sim, k) = prefix_similarity(&s.tokens, &next.tokens, params.prefix_words);
            // Début identique d'une phrase plus courte : phrase abandonnée en route.
            let abandoned = k >= 2 && s.tokens.len() < next.tokens.len() && sim == 1.0;
            if (k >= 3 && sim >= params.similarity) || abandoned {
                out.push(Retake {
                    start: s.start,
                    end: s.end,
                    text: s.text.clone(),
                    reason: format!("reprise plus loin : « {} »", next.text),
                });
                break;
            }
        }
    }
    out
}

/// Marque les prises refaites comme coupées dans le projet.
pub fn apply(project: &mut Project, source: &str, retakes: &[Retake]) {
    for r in retakes {
        project.mark_range(source, r.start, r.end, SegmentKind::Retake, false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fabrique des mots espacés de 0,3 s ; `|` force une pause longue.
    fn words(text: &str) -> Vec<Word> {
        let mut t = 0.0;
        let mut out = vec![];
        for w in text.split(' ') {
            if w == "|" {
                t += 1.5;
                continue;
            }
            out.push(Word { text: w.into(), start: t, end: t + 0.25 });
            t += 0.3;
        }
        out
    }

    #[test]
    fn restarted_sentence_is_cut_and_last_take_kept() {
        let w = words(
            "Alors aujourd'hui je vous montre mes tissus de. | Alors aujourd'hui je vous montre mes tissus de printemps.",
        );
        let r = find_retakes(&w, &RetakeParams::default());
        assert_eq!(r.len(), 1);
        assert!(r[0].text.ends_with("tissus de."));
    }

    #[test]
    fn abandoned_fragment_is_cut() {
        let w = words("Ce lin est | Ce lin est vraiment doux au toucher.");
        let r = find_retakes(&w, &RetakeParams::default());
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].text, "Ce lin est");
    }

    #[test]
    fn explicit_marker() {
        let w = words("Et là je couds le. Non attends je recommence. Et là je couds la bordure.");
        let r = find_retakes(&w, &RetakeParams::default());
        assert_eq!(r.len(), 2, "{r:?}");
        assert!(r.iter().any(|x| x.reason.contains("marqueur")));
    }

    #[test]
    fn different_sentences_are_kept() {
        let w = words("Voici le premier tissu. Il vient de chez Mondial. Le deuxième est un coton bio.");
        assert!(find_retakes(&w, &RetakeParams::default()).is_empty());
    }

    #[test]
    fn common_opening_word_alone_is_not_a_retake() {
        let w = words("Alors. | Alors on passe au patron maintenant.");
        assert!(find_retakes(&w, &RetakeParams::default()).is_empty());
    }
}
