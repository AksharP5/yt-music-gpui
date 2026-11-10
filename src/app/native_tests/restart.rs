//! A real process restart, using only a public playlist and isolated guest data.

use crate::{
    app::{CatalogTarget, Music, Playback, Startup},
    catalog::{BrowseLink, Catalog},
    library::Library,
    queue_source::PageRequest,
    watch::WatchMode,
};
use anyhow::{Context as _, Result, ensure};
use gpui::{
    AppContext, Bounds, TitlebarOptions, WindowBounds, WindowKind, WindowOptions, point, px, size,
};
use std::{
    process::Command,
    time::{Duration, Instant},
};

const TEST: &str = "app::native_tests::restart::partial_playlist_continues_after_process_restart";
const PLAYLIST: &str = "VLPL0GvsLQil0MmYC96KEs_7dTNsLm1PS6JX";

#[test]
#[ignore = "requires isolated desktop/D-Bus, network and yt-dlp; set YT_MUSIC_NATIVE_TESTS=1"]
fn partial_playlist_continues_after_process_restart() -> Result<()> {
    super::isolate()?;
    let Ok(phase) = std::env::var("YT_MUSIC_RESTART_PHASE") else {
        for phase in [
            "save-browse",
            "restore-browse",
            "save-issued",
            "restore-issued",
        ] {
            let output = Command::new(std::env::current_exe()?)
                .args(["--ignored", "--exact", TEST, "--nocapture"])
                .env("YT_MUSIC_RESTART_PHASE", phase)
                .output()?;
            println!("{}", String::from_utf8_lossy(&output.stdout));
            ensure!(
                output.status.success(),
                "Restart {phase} process failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        return Ok(());
    };
    ensure!(
        matches!(
            phase.as_str(),
            "save-browse" | "restore-browse" | "save-issued" | "restore-issued"
        ),
        "Unknown restart phase"
    );
    let (completed, result) = async_channel::bounded::<Result<()>>(1);
    gpui_platform::application().with_assets(crate::assets::Assets).run(move |cx| {
        cx.text_system().add_fonts(vec![std::borrow::Cow::Borrowed(include_bytes!("../../../assets/fonts/Roboto.ttf").as_slice())]).unwrap();
        crate::input::init(cx);
        let window = cx.open_window(WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::new(point(px(2.), px(2.)), size(px(956.), px(1050.))))),
            titlebar: Some(TitlebarOptions { title: Some("Music queue restart integration test".into()), ..Default::default() }),
            kind: WindowKind::PopUp,
            focus: false,
            ..Default::default()
        }, |window, cx| cx.new(|cx| {
            let mut music = Music::new(Startup::Guest, window, cx);
            for target in CatalogTarget::ALL { music.catalog_requests.cancel(target); }
            music.library.volume = 0.;
            music.library.autoplay = false;
            music.player.as_ref().unwrap().set_volume(0.).unwrap();
            music.player_open = true;
            music
        })).unwrap();
        cx.spawn(async move |cx| {
            let outcome: Result<()> = async {
                super::render::wait_for_presented_frame(window, cx).await;
                let path = std::path::PathBuf::from(std::env::var_os("XDG_DATA_HOME").context("private data directory")?).join("yt-music-gpui/library.json");
                if phase.starts_with("save-") {
                    let issued = phase.ends_with("-issued");
                    let (page, watch) = cx.background_executor().spawn(async move {
                        let catalog = Catalog::new()?;
                        let page = catalog.browse(&BrowseLink::new(PLAYLIST, "Top 500 Classic Rock songs"))?;
                        let watch = if issued {
                            let seed = page.tracks().into_iter().next().context("Public playlist must have a seed")?;
                            let watch = catalog.watch(&seed, WatchMode::Issued)?;
                            ensure!(!watch.queue.is_empty() && watch.continuation.is_some(), "Issued restart fixture must have a continued first page");
                            Some(watch)
                        } else { None };
                        Ok::<_, anyhow::Error>((page, watch))
                    }).await?;
                    ensure!(page.tracks().len() == 100 && page.continuation.is_some(), "Public restart fixture must have 100 cached rows and a continuation");
                    let cached_count = watch.as_ref().map_or(100, |watch| watch.queue.len());
                    window.update(cx, |music, _, cx| {
                        if let Some(watch) = &watch {
                            music.library.queue.replace(watch.queue.clone(), watch.current_index.unwrap());
                            music.library.queue.playing_from = watch.source.clone();
                            music.queue_source.attach_watch(&mut music.library.queue, watch, None);
                        } else {
                            music.queue_source.replace_queue(&mut music.library.queue, &page, 0, PageRequest::Collection(PLAYLIST.into()), None);
                        }
                        music.play_current(cx);
                        music.save();
                    })?;
                    let deadline = Instant::now() + Duration::from_secs(45);
                    loop {
                        let playing = window.update(cx, |music, _, _| music.position > 0. && matches!(music.playback, Playback::Loaded { .. }))?;
                        let saved = std::fs::read(&path).ok().and_then(|data| serde_json::from_slice::<Library>(&data).ok()).is_some_and(|library| library.queue.tracks.len() == cached_count && library.queue.source.is_some() && library.volume == 0.);
                        if playing && saved { break; }
                        ensure!(Instant::now() < deadline, "Could not play and persist the first {cached_count} rows before quit");
                        cx.background_executor().timer(Duration::from_millis(50)).await;
                    }
                    println!("RESTART_SAVED phase={phase} cached={cached_count} playing=true source_persisted=true pid={}", std::process::id());
                } else {
                    let cached_count = window.update(cx, |music, _, cx| -> Result<usize> {
                        let cached_count = music.library.queue.tracks.len();
                        ensure!(cached_count > 0 && music.library.queue.incomplete, "Restart did not restore the partial queue");
                        ensure!(phase.ends_with("-issued") || cached_count == 100, "Browse restart must restore exactly 100 cached tracks");
                        ensure!(!music.queue_source.can_continue(), "The source must stay lazy on startup");
                        music.library.queue.select(cached_count - 1);
                        music.play_current(cx);
                        Ok(cached_count)
                    })??;
                    // Use the same Next shortcut as the native UI, before prefetch can finish.
                    cx.update_window(window.into(), |_, window, cx| {
                        window.dispatch_keystroke(gpui::Keystroke::parse("ctrl-right").unwrap(), cx);
                    })?;
                    let deadline = Instant::now() + Duration::from_secs(60);
                    loop {
                        let (index, count, playing, error, current) = window.update(cx, |music, _, _| (music.library.queue.index, music.library.queue.tracks.len(), music.position > 0. && matches!(music.playback, Playback::Loaded { .. }), music.error.clone(), music.library.queue.current().map(|track| (track.id.clone(), track.title.clone()))))?;
                        ensure!(error.is_none(), "Restart continuation failed: {error:?}; index={index:?}, loaded={count}, current={current:?}");
                        if index == Some(cached_count) && playing {
                            println!("RESTART_NEXT phase={phase} cached={cached_count} selected={cached_count} next={} loaded={count} playing=true pid={}", cached_count + 1, std::process::id());
                            break;
                        }
                        ensure!(Instant::now() < deadline, "Next from row {cached_count} did not play the following row: index={index:?}, count={count}");
                        cx.background_executor().timer(Duration::from_millis(50)).await;
                    }
                    cx.background_executor().timer(Duration::from_millis(300)).await;
                    window.update(cx, |_, window, _| { super::render::capture(window, &format!("{phase}-track-101")); })?;
                }
                Ok(())
            }.await;
            if let Err(error) = &outcome { eprintln!("Native restart check: {error:#}"); }
            let _ = completed.send_blocking(outcome);
            cx.update(|cx| cx.quit());
        }).detach();
        cx.spawn(async move |cx| {
            cx.background_executor().timer(Duration::from_secs(120)).await;
            cx.update(|cx| cx.quit());
        }).detach();
    });
    result
        .try_recv()
        .context("Native restart check did not finish")?
}
