//! Export : traduit le projet en un seul appel ffmpeg.
//!
//! Graphe généré :
//! 1. chaque rush est normalisé (cadence, taille, format, son stéréo 48 kHz) puis dupliqué ;
//! 2. chaque segment gardé est extrait, avec un fondu audio de quelques ms aux coupes ;
//! 3. concaténation, puis titres et images par-dessus ;
//! 4. musique de fond (baissée sous la voix) ;
//! 5. intro + montage + outro, puis loudness normalisée pour YouTube.

use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::media::{self, MediaInfo};
use crate::model::*;
use crate::{Error, Result};

pub struct RenderJob {
    pub args: Vec<String>,
    pub filter: String,
    /// Durée totale attendue de la vidéo, pour la progression.
    pub duration: f64,
    pub warnings: Vec<String>,
    _tmp: tempfile::TempDir,
}

fn f(x: f64) -> String {
    format!("{x:.3}")
}

fn vnorm(e: &ExportSettings) -> String {
    format!(
        "fps={fps},scale={w}:{h}:force_original_aspect_ratio=decrease,\
         pad={w}:{h}:(ow-iw)/2:(oh-ih)/2:color=black,setsar=1,format=yuv420p",
        fps = e.fps,
        w = e.width,
        h = e.height
    )
}

const ANORM: &str = "aresample=48000,aformat=sample_fmts=fltp:channel_layouts=stereo";

fn default_font() -> Option<PathBuf> {
    [
        "/System/Library/Fonts/Supplemental/Arial Bold.ttf",
        "/System/Library/Fonts/Helvetica.ttc",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf",
    ]
    .iter()
    .map(PathBuf::from)
    .find(|p| p.exists())
}

struct Inputs(Vec<Vec<String>>);

impl Inputs {
    fn add(&mut self, pre: &[&str], path: &Path) -> usize {
        let mut a: Vec<String> = pre.iter().map(|s| s.to_string()).collect();
        a.push("-i".into());
        a.push(path.to_string_lossy().into_owned());
        self.0.push(a);
        self.0.len() - 1
    }
}

/// Ajoute l'intro ou l'outro au graphe ; renvoie (vidéo, audio, durée).
fn bumper(
    g: &mut String,
    inputs: &mut Inputs,
    path: &Path,
    name: &str,
    e: &ExportSettings,
) -> Result<(String, String, f64)> {
    let info: MediaInfo = media::probe(path)?;
    if !info.has_video {
        return Err(Error::Invalid(format!("{name} : {} n'a pas de vidéo", path.display())));
    }
    let i = inputs.add(&[], path);
    let _ = writeln!(g, "[{i}:v:0]{}[{name}v];", vnorm(e));
    if info.has_audio {
        let _ = writeln!(g, "[{i}:a:0]{ANORM},atrim=duration={}[{name}a];", f(info.duration));
    } else {
        let _ = writeln!(g, "anullsrc=r=48000:cl=stereo,atrim=duration={}[{name}a];", f(info.duration));
    }
    Ok((format!("{name}v"), format!("{name}a"), info.duration))
}

/// Qualité d'export : finale, ou brouillon rapide pour les aperçus et les images.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Quality {
    Final,
    Draft,
}

pub fn build(project: &Project, output: &Path) -> Result<RenderJob> {
    build_with(project, output, Quality::Final)
}

pub fn build_with(project: &Project, output: &Path, quality: Quality) -> Result<RenderJob> {
    project.validate()?;
    let e = &project.export;
    let kept: Vec<&Segment> = project.kept_segments().collect();
    if kept.is_empty() {
        return Err(Error::Invalid("aucun segment gardé : rien à exporter".into()));
    }
    let tmp = tempfile::tempdir()?;
    let mut inputs = Inputs(vec![]);
    let mut g = String::new();
    let mut warnings = vec![];
    // Encodeur matériel absent de cet ffmpeg : repli sur x264 plutôt qu'un échec.
    let mut encoder = e.video_encoder.clone();
    if encoder.contains("videotoolbox") && !media::has_encoder(&encoder) {
        warnings.push(format!("{encoder} indisponible dans cet ffmpeg : encodage logiciel x264"));
        encoder = "libx264".into();
    }

    // 1–2. Rushes et segments.
    let mut seg_labels = vec![];
    for src in &project.sources {
        let segs: Vec<&&Segment> = kept.iter().filter(|s| s.source == src.id).collect();
        if segs.is_empty() {
            continue;
        }
        let i = inputs.add(&[], &src.path);
        let n = segs.len();
        let vouts: String = (0..n).map(|j| format!("[s{i}v{j}]")).collect();
        let aouts: String = (0..n).map(|j| format!("[s{i}a{j}]")).collect();
        let _ = writeln!(g, "[{i}:v:0]{},split={n}{vouts};", vnorm(e));
        let _ = writeln!(g, "[{i}:a:0]{ANORM},asplit={n}{aouts};");
        for (j, s) in segs.iter().enumerate() {
            let k = seg_labels.len();
            let fade = (e.cut_fade_ms as f64 / 1000.0).min(s.len() / 4.0);
            let _ = writeln!(
                g,
                "[s{i}v{j}]trim=start={a}:end={b},setpts=PTS-STARTPTS[v{k}];",
                a = f(s.start),
                b = f(s.end)
            );
            let _ = writeln!(
                g,
                "[s{i}a{j}]atrim=start={a}:end={b},asetpts=PTS-STARTPTS,\
                 afade=t=in:d={fd},afade=t=out:st={st}:d={fd}[a{k}];",
                a = f(s.start),
                b = f(s.end),
                fd = f(fade),
                st = f((s.len() - fade).max(0.0))
            );
            seg_labels.push(k);
        }
    }
    let pairs: String = seg_labels.iter().map(|k| format!("[v{k}][a{k}]")).collect();
    let _ = writeln!(g, "{pairs}concat=n={}:v=1:a=1[mv0][ma0];", seg_labels.len());
    let prog = project.program_duration();

    // 3. Titres et images.
    let mut v = "mv0".to_string();
    let font = match &e.font {
        Some(p) => Some(p.clone()),
        None => default_font(),
    };
    // Copie de la police dans le dossier temporaire : évite d'échapper des chemins exotiques.
    let font_opt = match font {
        Some(p) if p.exists() => {
            let ext = p.extension().and_then(|x| x.to_str()).unwrap_or("ttf");
            let dst = tmp.path().join(format!("font.{ext}"));
            std::fs::copy(&p, &dst)?;
            format!("fontfile='{}'", dst.display())
        }
        _ => "font=Sans".into(),
    };
    let (w, h) = (e.width as f64, e.height as f64);
    for (n, o) in project.overlays.iter().enumerate() {
        let Some(p) = project.source_to_program(&o.anchor) else {
            warnings.push(format!("{} : ancré dans une partie coupée en fin de vidéo, ignoré", o.id));
            continue;
        };
        let d = o.duration.min(prog - p);
        if d <= 0.05 {
            warnings.push(format!("{} : trop près de la fin, ignoré", o.id));
            continue;
        }
        let enable = format!("enable='between(t,{},{})'", f(p), f(p + d));
        let out = format!("ov{n}");
        match &o.content {
            OverlayContent::Title { text, style } => {
                let file = tmp.path().join(format!("title{n}.txt"));
                std::fs::write(&file, text)?;
                let fade = 0.3f64.min(d / 3.0);
                let alpha = format!(
                    "alpha='if(lt(t,{a}),(t-{p})/{fd},if(gt(t,{b}),({e}-t)/{fd},1))'",
                    a = f(p + fade),
                    p = f(p),
                    b = f(p + d - fade),
                    e = f(p + d),
                    fd = f(fade)
                );
                let layout = match style {
                    TitleStyle::Lower => format!(
                        "fontsize={fs}:x={x}:y={y}:box=1:boxcolor=black@0.55:boxborderw={bb}",
                        fs = (h / 18.0).round(),
                        x = (w * 0.06).round(),
                        y = (h * 0.78).round(),
                        bb = (h / 60.0).round()
                    ),
                    TitleStyle::Center | TitleStyle::Big => format!(
                        "fontsize={fs}:x=(w-text_w)/2:y=(h-text_h)/2:shadowcolor=black@0.6:shadowx=3:shadowy=3",
                        fs = (h / if *style == TitleStyle::Big { 7.0 } else { 11.0 }).round()
                    ),
                };
                let _ = writeln!(
                    g,
                    "[{v}]drawtext=textfile='{tf}':expansion=none:{font_opt}:fontcolor=white:{layout}:{alpha}:{enable}[{out}];",
                    tf = file.display()
                );
            }
            OverlayContent::Image { path, position, scale } => {
                let fps = e.fps.to_string();
                let dur = f(d);
                let i = inputs.add(&["-loop", "1", "-framerate", &fps, "-t", &dur], path);
                let m = (w * 0.04).round();
                let (x, y) = match position {
                    Position::TopLeft => (format!("{m}"), format!("{m}")),
                    Position::TopRight => (format!("W-w-{m}"), format!("{m}")),
                    Position::BottomLeft => (format!("{m}"), format!("H-h-{m}")),
                    Position::BottomRight => (format!("W-w-{m}"), format!("H-h-{m}")),
                    Position::Center => ("(W-w)/2".into(), "(H-h)/2".into()),
                };
                let iw = ((w * scale.clamp(0.02, 1.0)) / 2.0).round() * 2.0;
                let _ = writeln!(
                    g,
                    "[{i}:v:0]scale={iw}:-2,format=rgba,setpts=PTS-STARTPTS+{p}/TB[img{n}];",
                    p = f(p)
                );
                let _ = writeln!(g, "[{v}][img{n}]overlay=x={x}:y={y}:eof_action=pass:{enable}[{out}];");
            }
        }
        v = out;
    }

    // 4. Musique.
    let mut a = "ma0".to_string();
    if let Some(m) = &project.music {
        let i = inputs.add(&["-stream_loop", "-1"], &m.path);
        let fade_out = 2.0f64.min(prog / 4.0);
        let _ = writeln!(
            g,
            "[{i}:a:0]{ANORM},volume={vol}dB,atrim=duration={d},afade=t=out:st={st}:d={fo}[mus];",
            vol = f(m.volume_db),
            d = f(prog),
            st = f(prog - fade_out),
            fo = f(fade_out)
        );
        if m.duck {
            let _ = writeln!(g, "[{a}]asplit=2[voice][sc];");
            let _ = writeln!(g, "[mus][sc]sidechaincompress=threshold=0.02:ratio=6:attack=30:release=500[duck];");
            let _ = writeln!(g, "[voice][duck]amix=inputs=2:duration=first:normalize=0[ma1];");
        } else {
            let _ = writeln!(g, "[{a}][mus]amix=inputs=2:duration=first:normalize=0[ma1];");
        }
        a = "ma1".into();
    }

    // 5. Intro, outro, loudness.
    let mut parts = vec![];
    let mut total = prog;
    if let Some(p) = &project.intro {
        let (iv, ia, d) = bumper(&mut g, &mut inputs, p, "intro", e)?;
        parts.push((iv, ia));
        total += d;
    }
    parts.push((v, a));
    if let Some(p) = &project.outro {
        let (ov, oa, d) = bumper(&mut g, &mut inputs, p, "outro", e)?;
        parts.push((ov, oa));
        total += d;
    }
    let (vout, acat) = if parts.len() == 1 {
        parts.pop().expect("une partie")
    } else {
        let pairs: String = parts.iter().map(|(v, a)| format!("[{v}][{a}]")).collect();
        let _ = writeln!(g, "{pairs}concat=n={}:v=1:a=1[vcat][acat];", parts.len());
        ("vcat".into(), "acat".into())
    };
    let _ = write!(
        g,
        "[{acat}]loudnorm=I={i}:TP=-1.0:LRA=11,aresample=48000[aout]",
        i = f(e.loudness_lufs)
    );

    let script = tmp.path().join("graph.txt");
    std::fs::write(&script, &g)?;

    let mut args: Vec<String> = ["-y", "-hide_banner", "-nostdin", "-progress", "pipe:1", "-nostats"]
        .map(String::from)
        .to_vec();
    for i in inputs.0 {
        args.extend(i);
    }
    let flag = if media::ffmpeg_major_version()? >= 7 { "-/filter_complex" } else { "-filter_complex_script" };
    args.push(flag.into());
    args.push(script.to_string_lossy().into_owned());
    for (k, v) in [("-map", format!("[{vout}]")), ("-map", "[aout]".into())] {
        args.push(k.into());
        args.push(v);
    }
    if quality == Quality::Draft {
        if encoder.contains("videotoolbox") {
            args.extend(["-c:v", "h264_videotoolbox", "-b:v", "2M"].map(String::from));
        } else {
            args.extend(["-c:v", "libx264", "-preset", "ultrafast", "-crf", "30"].map(String::from));
        }
    } else if encoder.contains("videotoolbox") {
        args.extend(["-c:v".into(), encoder.clone(), "-b:v".into(), e.video_bitrate.clone()]);
    } else {
        args.extend(["-c:v".into(), encoder.clone(), "-preset".into(), "medium".into()]);
        args.extend(["-crf".into(), e.crf.to_string()]);
    }
    args.extend(
        ["-pix_fmt", "yuv420p", "-r", &e.fps.to_string(), "-c:a", "aac", "-b:a", "192k", "-movflags", "+faststart"]
            .map(String::from),
    );
    args.push(output.to_string_lossy().into_owned());

    Ok(RenderJob { args, filter: g, duration: total, warnings, _tmp: tmp })
}

/// Lance l'export ; `progress` reçoit une valeur entre 0 et 1.
pub fn run(job: &RenderJob, mut progress: impl FnMut(f64)) -> Result<()> {
    let mut child = Command::new(media::ffmpeg_bin())
        .args(&job.args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| Error::Tool(format!("ffmpeg : {e}")))?;

    // stderr lu dans un fil à part pour ne jamais bloquer ffmpeg.
    let mut stderr = child.stderr.take().expect("stderr piped");
    let err_thread = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr.read_to_string(&mut s);
        s
    });

    let stdout = child.stdout.take().expect("stdout piped");
    for line in BufReader::new(stdout).lines() {
        let line = line?;
        if let Some(us) = line.strip_prefix("out_time_us=") {
            if let Ok(us) = us.trim().parse::<f64>() {
                if job.duration > 0.0 {
                    progress((us / 1e6 / job.duration).clamp(0.0, 1.0));
                }
            }
        } else if line == "progress=end" {
            progress(1.0);
        }
    }
    let status = child.wait()?;
    let err = err_thread.join().unwrap_or_default();
    if !status.success() {
        let tail: String = err.lines().rev().take(15).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n");
        return Err(Error::Tool(format!("export échoué :\n{tail}")));
    }
    Ok(())
}

/// Extrait la plage [from, to] (temps de la vidéo montée) sous forme d'un projet à part :
/// mêmes rushes, seules les parties gardées dans la plage restent, et les titres ou images
/// visibles dans la plage sont recalés. Sert aux aperçus et aux images.
pub fn slice(project: &Project, from: f64, to: f64) -> Project {
    let mut p = project.clone();
    let mut acc = 0.0;
    for seg in p.segments.iter_mut() {
        if !seg.keep {
            continue;
        }
        let (a, b) = (acc, acc + seg.len());
        acc = b;
        let (ka, kb) = (a.max(from), b.min(to));
        if kb - ka < EPS {
            seg.keep = false;
            continue;
        }
        let start = seg.start + (ka - a);
        seg.end = seg.start + (kb - a);
        seg.start = start;
    }
    // Les morceaux recadrés laissent des trous : on les comble par des segments coupés
    // pour que le projet reste valide.
    let mut filled = vec![];
    for src in &project.sources {
        let mut t = 0.0;
        for seg in p.segments.iter().filter(|s| s.source == src.id && s.keep) {
            if seg.start > t + EPS {
                filled.push(Segment { id: String::new(), source: src.id.clone(), start: t, end: seg.start, kind: SegmentKind::Manual, keep: false });
            }
            filled.push(seg.clone());
            t = seg.end;
        }
        if src.duration > t + EPS {
            filled.push(Segment { id: String::new(), source: src.id.clone(), start: t, end: src.duration, kind: SegmentKind::Manual, keep: false });
        }
    }
    p.segments = filled;
    p.normalize();

    p.overlays = project
        .overlays
        .iter()
        .filter_map(|o| {
            let at = project.source_to_program(&o.anchor)?;
            let (a, b) = (at.max(from), (at + o.duration).min(to));
            if b - a < 0.05 {
                return None;
            }
            let anchor = if at >= from { o.anchor.clone() } else { project.program_to_source(from)? };
            Some(Overlay { anchor, duration: b - a, ..o.clone() })
        })
        .collect();
    if from > EPS {
        p.intro = None;
    }
    if to < project.program_duration() - EPS {
        p.outro = None;
    }
    p
}

/// Image du montage à l'instant `t` (temps monté, hors intro), titres et incrustations compris.
/// Écrit un JPEG de `width` pixels de large.
pub fn frame(project: &Project, t: f64, width: u32, output: &Path) -> Result<()> {
    let total = project.program_duration();
    if !(0.0..=total).contains(&t) {
        return Err(Error::Invalid(format!("instant {t:.1} s hors du montage ({total:.1} s)")));
    }
    // Un peu de contexte avant l'instant demandé, pour que les fondus des titres soient terminés.
    let from = (t - 0.4).max(0.0);
    let mut p = slice(project, from, (t + 0.2).min(total));
    p.music = None;
    p.intro = None;
    p.outro = None;
    let ratio = project.export.height as f64 / project.export.width as f64;
    p.export.width = width & !1;
    p.export.height = ((width as f64 * ratio).round() as u32) & !1;
    let dir = tempfile::tempdir()?;
    let clip = dir.path().join("clip.mp4");
    run(&build_with(&p, &clip, Quality::Draft)?, |_| {})?;
    let out = Command::new(media::ffmpeg_bin())
        .args(["-y", "-v", "error", "-ss"])
        .arg(f(t - from))
        .arg("-i")
        .arg(&clip)
        .args(["-frames:v", "1", "-q:v", "4"])
        .arg(output)
        .output()
        .map_err(|e| Error::Tool(format!("ffmpeg : {e}")))?;
    if !out.status.success() || !output.exists() {
        return Err(Error::Tool(format!("extraction d'image : {}", String::from_utf8_lossy(&out.stderr).trim())));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project() -> Project {
        let seg = |start: f64, end: f64, keep: bool| Segment {
            id: String::new(),
            source: "a".into(),
            start,
            end,
            kind: if keep { SegmentKind::Speech } else { SegmentKind::Silence },
            keep,
        };
        let mut p = Project::new(
            vec![Source { id: "a".into(), path: "a.mp4".into(), duration: 30.0, width: None, height: None, fps: None }],
            vec![seg(0.0, 10.0, true), seg(10.0, 12.0, false), seg(12.0, 30.0, true)],
        );
        p.overlays.push(Overlay {
            id: "t".into(),
            anchor: Anchor { source: "a".into(), time: 8.0 },
            duration: 4.0,
            content: OverlayContent::Title { text: "x".into(), style: TitleStyle::Lower },
        });
        p
    }

    #[test]
    fn slice_keeps_only_the_range() {
        let p = project();
        let s = slice(&p, 5.0, 15.0);
        s.validate().unwrap();
        assert!((s.program_duration() - 10.0).abs() < 1e-9);
        let kept: Vec<_> = s.kept_segments().map(|x| (x.start, x.end)).collect();
        assert_eq!(kept, [(5.0, 10.0), (12.0, 17.0)]);
        // Le titre (montage 8–12) est entièrement dans la plage : même ancre.
        assert_eq!(s.overlays[0].anchor.time, 8.0);
        assert_eq!(s.source_to_program(&s.overlays[0].anchor), Some(3.0));
    }

    #[test]
    fn slice_reanchors_overlay_started_before() {
        let p = project();
        let s = slice(&p, 10.0, 20.0); // le titre a commencé à 8 : il en reste 2 s
        assert_eq!(s.overlays.len(), 1);
        assert!((s.overlays[0].duration - 2.0).abs() < 1e-9);
        assert_eq!(s.source_to_program(&s.overlays[0].anchor), Some(0.0));
        assert!(slice(&p, 13.0, 20.0).overlays.is_empty());
    }
}
