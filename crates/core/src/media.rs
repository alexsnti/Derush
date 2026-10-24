//! Accès à ffmpeg / ffprobe.
//!
//! Les binaires sont cherchés dans `DERUSH_FFMPEG` / `DERUSH_FFPROBE`, sinon dans le PATH.
//! L'app Tauri les fournira en « sidecar » pour que ta compagne n'ait rien à installer.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::Deserialize;

use crate::{Error, Result};

/// Cherche un outil : variable d'environnement, à côté de l'exécutable (ffmpeg embarqué
/// dans Derush.app/Contents/MacOS), PATH, puis emplacements Homebrew.
/// Une app lancée depuis le Finder, ou un serveur MCP lancé par Claude Desktop,
/// ne voit pas le PATH du terminal : sans ces replis, ffmpeg serait introuvable.
fn find_tool(env: &str, name: &str) -> PathBuf {
    if let Some(p) = std::env::var_os(env) {
        return p.into();
    }
    if let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)) {
        let bundled = dir.join(name);
        if bundled.is_file() {
            return bundled;
        }
    }
    let in_path = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).map(|d| d.join(name)).any(|p| p.is_file()))
        .unwrap_or(false);
    if in_path {
        return name.into();
    }
    ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"]
        .iter()
        .map(|d| Path::new(d).join(name))
        .find(|p| p.is_file())
        .unwrap_or_else(|| name.into())
}

pub fn ffmpeg_bin() -> PathBuf {
    find_tool("DERUSH_FFMPEG", "ffmpeg")
}

pub fn ffprobe_bin() -> PathBuf {
    find_tool("DERUSH_FFPROBE", "ffprobe")
}

/// Version majeure d'ffmpeg (ex. 7), utile car certaines options ont changé.
pub fn ffmpeg_major_version() -> Result<u32> {
    let out = Command::new(ffmpeg_bin())
        .arg("-version")
        .output()
        .map_err(|e| Error::Tool(format!("ffmpeg : {e}")))?;
    let text = String::from_utf8_lossy(&out.stdout);
    // "ffmpeg version 6.1.1-3ubuntu5 ..." ou "ffmpeg version n7.1 ..."
    let v = text
        .split_whitespace()
        .nth(2)
        .ok_or_else(|| Error::Tool("sortie de -version inattendue".into()))?;
    let digits: String = v.trim_start_matches('n').chars().take_while(|c| c.is_ascii_digit()).collect();
    // Les builds de développement ("N-12345-g…") sont récents.
    Ok(digits.parse().unwrap_or(99))
}

#[derive(Debug, Clone, PartialEq)]
pub struct MediaInfo {
    pub duration: f64,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fps: Option<f64>,
    pub has_audio: bool,
    pub has_video: bool,
}

#[derive(Deserialize)]
struct ProbeOut {
    format: ProbeFormat,
    #[serde(default)]
    streams: Vec<ProbeStream>,
}
#[derive(Deserialize)]
struct ProbeFormat {
    duration: Option<String>,
}
#[derive(Deserialize)]
struct ProbeStream {
    codec_type: String,
    width: Option<u32>,
    height: Option<u32>,
    avg_frame_rate: Option<String>,
    disposition: Option<ProbeDisposition>,
}
#[derive(Deserialize)]
struct ProbeDisposition {
    #[serde(default)]
    attached_pic: u8,
}

fn parse_rate(r: &str) -> Option<f64> {
    let (n, d) = r.split_once('/')?;
    let (n, d): (f64, f64) = (n.parse().ok()?, d.parse().ok()?);
    (d > 0.0 && n > 0.0).then_some(n / d)
}

pub fn probe(path: &Path) -> Result<MediaInfo> {
    let out = Command::new(ffprobe_bin())
        .args(["-v", "error", "-show_entries"])
        .arg("format=duration:stream=codec_type,width,height,avg_frame_rate:stream_disposition=attached_pic")
        .args(["-of", "json"])
        .arg(path)
        .output()
        .map_err(|e| Error::Tool(format!("ffprobe : {e}")))?;
    if !out.status.success() {
        return Err(Error::Tool(format!(
            "ffprobe {} : {}",
            path.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let p: ProbeOut = serde_json::from_slice(&out.stdout)?;
    let video = p.streams.iter().find(|s| {
        s.codec_type == "video" && s.disposition.as_ref().map_or(true, |d| d.attached_pic == 0)
    });
    Ok(MediaInfo {
        duration: p.format.duration.as_deref().and_then(|d| d.parse().ok()).unwrap_or(0.0),
        width: video.and_then(|v| v.width),
        height: video.and_then(|v| v.height),
        fps: video.and_then(|v| v.avg_frame_rate.as_deref()).and_then(parse_rate),
        has_audio: p.streams.iter().any(|s| s.codec_type == "audio"),
        has_video: video.is_some(),
    })
}

/// Fréquence d'échantillonnage utilisée pour l'analyse (celle attendue par Whisper et Silero).
pub const ANALYSIS_SAMPLE_RATE: u32 = 16_000;

/// Décode la piste audio en mono 16 kHz et appelle `on_samples` au fil de l'eau,
/// pour ne pas garder une heure de PCM en mémoire.
pub fn stream_pcm(path: &Path, mut on_samples: impl FnMut(&[i16])) -> Result<()> {
    let mut child = Command::new(ffmpeg_bin())
        .args(["-nostdin", "-v", "error", "-i"])
        .arg(path)
        .args(["-vn", "-ac", "1", "-ar"])
        .arg(ANALYSIS_SAMPLE_RATE.to_string())
        .args(["-f", "s16le", "pipe:1"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| Error::Tool(format!("ffmpeg : {e}")))?;

    let mut stdout = child.stdout.take().expect("stdout piped");
    let mut buf = vec![0u8; 64 * 1024];
    let mut carry: Option<u8> = None;
    let mut samples = Vec::with_capacity(buf.len() / 2 + 1);
    loop {
        let n = stdout.read(&mut buf)?;
        if n == 0 {
            break;
        }
        samples.clear();
        let mut bytes = &buf[..n];
        if let Some(lo) = carry.take() {
            samples.push(i16::from_le_bytes([lo, bytes[0]]));
            bytes = &bytes[1..];
        }
        let mut chunks = bytes.chunks_exact(2);
        samples.extend(chunks.by_ref().map(|c| i16::from_le_bytes([c[0], c[1]])));
        carry = chunks.remainder().first().copied();
        on_samples(&samples);
    }

    let out = child.wait_with_output()?;
    if !out.status.success() {
        return Err(Error::Tool(format!(
            "décodage audio de {} : {}",
            path.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(())
}

/// L'ffmpeg utilisé sait-il encoder avec cet encodeur ? (résultat mis en cache)
pub fn has_encoder(name: &str) -> bool {
    use std::sync::OnceLock;
    static LIST: OnceLock<String> = OnceLock::new();
    LIST.get_or_init(|| {
        Command::new(ffmpeg_bin())
            .args(["-hide_banner", "-encoders"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default()
    })
    .split_whitespace()
    .any(|w| w == name)
}
