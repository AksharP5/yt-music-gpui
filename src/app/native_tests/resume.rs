//! Real process restarts that continue the last song where the previous process quit.

use crate::{
    app::{Music, Playback, Startup},
    catalog::Track,
};
use anyhow::{Context as _, Result, ensure};
use gpui::{
    AppContext, Bounds, TitlebarOptions, WindowBounds, WindowKind, WindowOptions, point, px, size,
};
use std::{
    process::Command,
    time::{Duration, Instant},
};

const TEST: &str = "app::native_tests::resume::the_last_song_continues_after_process_restart";
const SONG: &str = "dQw4w9WgXcQ";

#[test]
#[ignore = "requires isolated desktop/D-Bus, network and yt-dlp; set YT_MUSIC_NATIVE_TESTS=1"]
fn the_last_song_continues_after_process_restart() -> Result<()> {
    super::isolate()?;
    let Ok(phase) = std::env::var("YT_MUSIC_RESUME_PHASE") else {
        for phase in ["play", "restore", "previous"] {
            let output = Command::new(std::env::current_exe()?)
                .args(["--ignored", "--exact", TEST, "--nocapture"])
                .env("YT_MUSIC_RESUME_PHASE", phase)
                .output()?;
            println!("{}", String::from_utf8_lossy(&output.stdout));
            ensure!(
                output.status.success(),
                "Resume {phase} process failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        return Ok(());
    };
    ensure!(
        matches!(phase.as_str(), "play" | "restore" | "previous"),
        "Unknown resume phase"
    );
    let (completed, result) = async_channel::bounded::<Result<()>>(1);
    gpui_platform::application().with_assets(crate::assets::Assets).run(move |cx| {
        cx.text_system().add_fonts(vec![std::borrow::Cow::Borrowed(include_bytes!("../../../assets/fonts/Roboto.ttf").as_slice())]).unwrap();
        crate::input::init(cx);
        let window = cx.open_window(WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::new(point(px(2.), px(2.)), size(px(956.), px(700.))))),
            titlebar: Some(TitlebarOptions { title: Some("Music resume integration test".into()), ..Default::default() }),
            kind: WindowKind::PopUp,
            focus: false,
            ..Default::default()
        }, |window, cx| cx.new(|cx| {
            let mut music = Music::new(Startup::Guest, window, cx);
            music.library.volume = 0.;
            music.player.as_ref().unwrap().set_volume(0.).unwrap();
            music
        })).unwrap();
        cx.spawn(async move |cx| {
            let outcome: Result<()> = async {
                super::render::wait_for_presented_frame(window, cx).await;
                // Where the bar starts and where Play is expected to continue.
                let expected = window.update(cx, |music, _, cx| -> Result<f64> {
                    if phase == "play" {
                        let song: Track = serde_json::from_value(serde_json::json!({
                            "id": SONG, "title": "Never Gonna Give You Up", "artist": "Rick Astley", "duration": 213,
                        }))?;
                        music.library.queue.replace(vec![song], 0);
                        music.toggle_play(cx);
                        return Ok(0.);
                    }
                    let last = music.library.last_position.clone().context("Quit must save the last song position")?;
                    ensure!(last.track_id == SONG && last.seconds >= 19., "Unexpected saved position {last:?}");
                    ensure!(music.playback == Playback::Stopped && music.active_id.is_none(), "A restart must not autoplay");
                    ensure!((music.playback_position() - last.seconds).abs() < 0.01 && music.transport().duration == 213., "The player bar must show the saved position before Play");
                    if phase == "previous" {
                        music.previous(cx);
                        ensure!(music.playback_position() == 0., "Previous before Play returns the bar to the start");
                        music.toggle_play(cx);
                        return Ok(0.);
                    }
                    music.toggle_play(cx);
                    Ok(last.seconds)
                })??;
                let deadline = Instant::now() + Duration::from_secs(60);
                let started = loop {
                    let (playing, position, error) = window.update(cx, |music, _, _| (
                        music.position > 0. && matches!(music.playback, Playback::Loaded { paused: false, .. }),
                        music.position, music.error.clone(),
                    ))?;
                    ensure!(error.is_none(), "Playback failed: {error:?}");
                    if playing {
                        break position;
                    }
                    ensure!(Instant::now() < deadline, "{phase}: Play did not start");
                    cx.background_executor().timer(Duration::from_millis(50)).await;
                };
                ensure!((started - expected).abs() < 3., "{phase}: Play started at {started:.1}s, expected {expected:.1}s");
                // Leave enough listening for the next phase to resume from.
                let until = expected + 20.;
                loop {
                    let position = window.update(cx, |music, _, _| music.position)?;
                    if position >= until {
                        println!("RESUME phase={phase} started={started:.1} quit_at={position:.1}");
                        break;
                    }
                    ensure!(Instant::now() < deadline, "{phase}: playback stalled at {position:.1}s");
                    cx.background_executor().timer(Duration::from_millis(100)).await;
                }
                Ok(())
            }.await;
            if let Err(error) = &outcome { eprintln!("Native resume check: {error:#}"); }
            let _ = completed.send_blocking(outcome);
            // Quitting drops the view, which saves the position the next phase reads.
            cx.update(|cx| cx.quit());
        }).detach();
        cx.spawn(async move |cx| {
            cx.background_executor().timer(Duration::from_secs(120)).await;
            cx.update(|cx| cx.quit());
        }).detach();
    });
    result
        .try_recv()
        .context("Native resume check did not finish")?
}
