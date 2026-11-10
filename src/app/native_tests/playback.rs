use crate::{
    app::{CatalogTarget, Music, Playback, Route, Startup},
    auth,
    catalog::{BrowseLink, Catalog, Track},
    queue_source::PageRequest,
};
use anyhow::{Context as _, Result, ensure};
use gpui::{AppContext, Bounds, TitlebarOptions, WindowBounds, WindowOptions, point, px, size};
use std::time::{Duration, Instant};
use std::{collections::HashSet, process::Command};

#[test]
#[ignore = "requires isolated desktop/D-Bus, network and a yt-dlp version-mismatch wrapper; set YT_MUSIC_NATIVE_TESTS=1"]
fn fallback_notice_survives_playback_and_shuffle_loads_the_whole_playlist() -> Result<()> {
    super::isolate()?;
    let version = Command::new("yt-dlp")
        .args(["--ignore-config", "--no-plugin-dirs", "--version"])
        .output()?;
    ensure!(
        version.status.success() && String::from_utf8_lossy(&version.stdout).trim() != "2026.08.19",
        "Use a synthetic version-mismatch wrapper that also handles --version after other flags; got {:?}",
        String::from_utf8_lossy(&version.stdout).trim()
    );

    let (completed, result) = async_channel::bounded::<Result<()>>(1);
    gpui_platform::application().with_assets(crate::assets::Assets).run(move |cx| {
        cx.text_system().add_fonts(vec![std::borrow::Cow::Borrowed(include_bytes!("../../../assets/fonts/Roboto.ttf").as_slice())]).unwrap();
        crate::input::init(cx);
        let window = cx.open_window(WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::new(point(px(40.), px(40.)), size(px(1320.), px(860.))))),
            titlebar: Some(TitlebarOptions { title: Some("Music playback integration test".into()), ..Default::default() }),
            focus: true,
            ..Default::default()
        }, |window, cx| cx.new(|cx| {
            let mut music = Music::new(Startup::Guest, window, cx);
            for target in CatalogTarget::ALL { music.catalog_requests.cancel(target); }
            music.library.queue.replace(vec![Track {
                id: "dQw4w9WgXcQ".into(), title: "Never Gonna Give You Up".into(), artist: "Rick Astley".into(),
                album: None, duration: None, thumbnail: None, details: Default::default(),
            }], 0);
            music.library.autoplay = false;
            music.library.video = false;
            music.library.volume = 0.;
            music.player.as_ref().unwrap().set_volume(0.).unwrap();
            music.player_open = true;
            music
        })).unwrap();
        cx.activate(true);
        cx.spawn(async move |cx| {
            let outcome: Result<()> = async {
                let response = window.update(cx, |music, _, _| {
                    music.player.as_ref().unwrap().set_credentials(auth::PlaybackCredentials {
                        cookies: auth::sealed_file(b"# Netscape HTTP Cookie File\n")?,
                        identity: auth::sealed_file(br#"{"authuser":"0","delegated_session_id":null,"data_sync_id":"synthetic||"}"#)?,
                    })
                })??;
                response.recv().await.context("Player closed credential response")?.map_err(anyhow::Error::msg)?;
                // Account activation and play_current both replace transient notices.
                cx.background_executor().timer(Duration::from_millis(200)).await;
                window.update(cx, |music, _, cx| music.play_current(cx))?;
                let deadline = Instant::now() + Duration::from_secs(40);
                loop {
                    if window.update(cx, |music, _, _| music.position > 0. && matches!(music.playback, Playback::Loaded { .. }))? { break; }
                    ensure!(Instant::now() < deadline, "Guest fallback did not start audio");
                    cx.background_executor().timer(Duration::from_millis(20)).await;
                }
                let deadline = Instant::now() + Duration::from_secs(2);
                loop {
                    let notice = window.update(cx, |music, _, _| music.notice.clone())?;
                    if notice.as_deref().is_some_and(|notice| notice.contains("Playing public streams as a guest")) { break; }
                    ensure!(Instant::now() < deadline, "Version mismatch notice was erased before playback: {notice:?}");
                    cx.background_executor().timer(Duration::from_millis(20)).await;
                }
                cx.background_executor().timer(Duration::from_millis(500)).await;
                println!("UI_NOTICE_READY pid={}", std::process::id());
                cx.background_executor().timer(Duration::from_secs(3)).await;
                window.update(cx, |music, _, cx| music.stop_playback(cx))?;
                let page = cx.background_executor().spawn(async {
                    Catalog::new()?.browse(&BrowseLink::new("VLPL979721712CED57BB", "The 200 Greatest Rock & Pop Songs Of All Time"))
                }).await?;
                let initial = page.tracks();
                ensure!(!initial.is_empty() && page.continuation.is_some(), "Use a playlist with continuation pages");
                let first = initial[0].id.clone();
                let initial_count = initial.len();
                let initial_ids: HashSet<_> = initial.into_iter().map(|track| track.id).collect();
                window.update(cx, |music, _, cx| {
                    music.queue_source.replace_queue(&mut music.library.queue, &page, 0, PageRequest::Browse, None);
                    music.route = Route::Browse(BrowseLink::new("VLPL979721712CED57BB", page.title.clone()));
                    music.page = page;
                    music.rebuild_rows();
                    music.error = None;
                    music.shuffle_queue(cx);
                })?;
                let deadline = Instant::now() + Duration::from_secs(60);
                loop {
                    let (incomplete, error) = window.update(cx, |music, _, _| (music.library.queue.incomplete, music.error.clone()))?;
                    ensure!(error.is_none(), "Playlist shuffle failed: {error:?}");
                    if !incomplete { break; }
                    ensure!(Instant::now() < deadline, "Shuffle did not finish all continuation pages");
                    cx.background_executor().timer(Duration::from_millis(50)).await;
                }
                window.update(cx, |music, _, _| -> Result<()> {
                    let queue = &music.library.queue;
                    ensure!(queue.tracks.len() > initial_count, "Shuffle did not fetch continuation tracks");
                    ensure!(queue.current().is_some_and(|track| track.id == first) && queue.index == Some(0), "Shuffle moved the current track");
                    ensure!(queue.tracks.iter().skip(1).take(initial_count - 1).any(|track| !initial_ids.contains(&track.id)), "Continuation pages were not mixed into the upcoming queue");
                    ensure!(music.notice.as_deref() == Some("Upcoming songs shuffled"));
                    println!("SHUFFLE_LOADED initial={initial_count} total={} current_preserved=true", queue.tracks.len());
                    Ok(())
                })??;
                cx.background_executor().timer(Duration::from_millis(500)).await;
                println!("UI_SHUFFLE_READY pid={}", std::process::id());
                cx.background_executor().timer(Duration::from_secs(3)).await;
                Ok(())
            }.await;
            if let Err(error) = &outcome { eprintln!("Native playback check: {error:#}"); }
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
        .context("Native playback check did not finish")?
}
