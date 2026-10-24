//! Test de bout en bout : génère un rush avec ffmpeg, l'analyse, l'exporte avec habillage.
//! Ignoré automatiquement si ffmpeg n'est pas installé.

use std::path::Path;
use std::process::Command;

use derush_core::{analysis, media, render, *};

fn ffmpeg(args: &[&str]) {
    let st = Command::new(media::ffmpeg_bin()).args(["-v", "error", "-y"]).args(args).status().unwrap();
    assert!(st.success(), "ffmpeg {args:?}");
}

#[test]
fn analyze_and_render() -> anyhow::Result<()> {
    if Command::new(media::ffmpeg_bin()).arg("-version").output().is_err() {
        eprintln!("ffmpeg absent, test ignoré");
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let p = |n: &str| dir.path().join(n);
    let rush = p("rush.mp4");
    let rush_s = rush.to_str().unwrap();
    // 2,5 s de son / 1,5 s de quasi-silence, trois fois.
    ffmpeg(&[
        "-f", "lavfi", "-i", "testsrc2=size=640x360:rate=30:duration=12",
        "-f", "lavfi", "-i",
        "aevalsrc='0.3*sin(2*PI*220*t)*lt(mod(t\\,4)\\,2.5)+0.002*(random(0)-0.5)':s=48000:d=12",
        "-c:v", "libx264", "-preset", "ultrafast", "-c:a", "aac", "-shortest", rush_s,
    ]);
    ffmpeg(&["-f", "lavfi", "-i", "sine=f=440:d=3", "-c:a", "aac", p("music.m4a").to_str().unwrap()]);

    let a = analysis::analyze_file(&rush, "src-0", &Default::default())?;
    let cut = a.segments.iter().filter(|s| !s.keep).count();
    assert_eq!(cut, 3, "{:?}", a.segments);

    let mut project = Project::new(vec![a.source], a.segments);
    let expected = project.program_duration();
    // 3 × 2,5 s de son + marges de 0,15 s autour.
    assert!((expected - 8.25).abs() < 0.15, "durée montée {expected}");
    project.overlays.push(Overlay {
        id: "t1".into(),
        anchor: Anchor { source: "src-0".into(), time: 1.0 },
        duration: 2.0,
        content: OverlayContent::Title { text: "Test : 100 % l'été".into(), style: TitleStyle::Lower },
    });
    project.music = Some(Music { path: p("music.m4a"), volume_db: -20.0, duck: true });
    project.export.width = 640;
    project.export.height = 360;
    project.export.video_encoder = "libx264".into();

    let out = p("out.mp4");
    let job = render::build(&project, &out)?;
    render::run(&job, |_| {})?;
    let info = media::probe(Path::new(&out))?;
    assert!((info.duration - expected).abs() < 0.2, "{} vs {expected}", info.duration);
    assert_eq!((info.width, info.height), (Some(640), Some(360)));
    Ok(())
}
