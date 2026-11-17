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

fn continuation_page(indices: std::ops::Range<usize>, token: Option<&str>) -> crate::catalog::Page {
    crate::app::sync::tests::response(&[("Songs", indices.collect())], token, true)
}

#[test]
#[ignore = "requires desktop, private D-Bus and isolated runtime/data; synthetic playlist responses only"]
fn collection_shuffle_toggles_pending_requests_and_ignores_stale_results() -> Result<()> {
    use crate::app::{
        AccountOperation, PlaybackIntent, PlaybackQuery, PlaybackResult, QueueUpdate,
    };
    use crate::queue_source::{CollectionPlayback, PlaybackOrder};
    use std::sync::atomic::Ordering;

    super::navigation::check_cache_window(async |window, cx| {
        let details = crate::catalog::parse_page(&serde_json::json!({
            "header":{"musicResponsiveHeaderRenderer":{"title":{"simpleText":"Synthetic playlist"},"buttons":[
                {"musicPlayButtonRenderer":{"playNavigationEndpoint":{"watchPlaylistEndpoint":{"playlistId":"PLsynthetic","params":"original%3D"}}}},
                {"menuRenderer":{"items":[{"menuNavigationItemRenderer":{"icon":{"iconType":"MUSIC_SHUFFLE"},"navigationEndpoint":{"watchPlaylistEndpoint":{"playlistId":"PLsynthetic","params":"shuffle%3D"}}}}]}}
            ]}},"contents":[]
        }), "Synthetic playlist").unwrap().header.unwrap().collection;
        let source =
            CollectionPlayback::new("VLPLsynthetic", &details, PlaybackOrder::Original).unwrap();
        let tracks = continuation_page(0..3, None).tracks();
        let watch = |shuffled| {
            let queue = if shuffled {
                vec![tracks[1].clone(), tracks[2].clone(), tracks[0].clone()]
            } else {
                tracks.clone()
            };
            crate::watch::Watch {
                current: Some(queue[0].clone()),
                current_index: Some(0),
                queue,
                source: Some("Synthetic playlist".into()),
                lyrics: None,
                related: None,
                comments: None,
                alternatives: None,
                continuation: None,
                source_target: None,
            }
        };
        window
            .update(cx, |view, window, cx| {
                let (jobs, pending) = async_channel::bounded(1);
                view.playback_jobs = jobs;
                view.pending_playback_jobs = pending;
                view.account = None;
                view.account_operation = None;
                view.library.autoplay = false;
                view.library.queue.replace(tracks.clone(), 2);
                view.library.queue.collection = Some(source.clone());
                view.library.queue.playing_from = Some("Synthetic playlist".into());
                view.player_open = true;
                view.position = 14.7;
                view.playback = Playback::Loaded {
                    paused: true,
                    buffering: false,
                };
                view.error = None;
                window.focus(&view.focus, cx);
                cx.notify();
            })
            .unwrap();
        super::render::wait_for_presented_frame(window, cx).await;
        window
            .update(cx, |_, window, _| {
                super::render::capture(window, "collection-shuffle-before");
            })
            .unwrap();
        super::render::key(window, cx, "s").await;
        let first = window.update(cx, |view, _, _| {
            assert_eq!(view.position, 0.);
            assert!(matches!(view.playback, Playback::Loading { paused: false, .. }));
            let job = view.pending_playback_jobs.try_recv().unwrap();
            assert!(matches!(&job.query, PlaybackQuery::Collection(source) if source.order == PlaybackOrder::Shuffled));
            job
        }).unwrap();
        super::render::key(window, cx, "s").await;
        window.update(cx, |view, _, cx| {
            let latest = view.pending_playback_jobs.try_recv().unwrap();
            assert!(matches!(&latest.query, PlaybackQuery::Collection(source) if source.order == PlaybackOrder::Original));
            assert!(latest.id > first.id);
            view.playback_result(PlaybackResult::Watch {
                id:first.id, account_revision:first.account_revision, update:QueueUpdate::Replace,
                collection:Some(source.toggled()), result:Ok(Box::new(watch(true))), session_rejected:false,
            },cx);
            assert_eq!(view.library.queue.index, Some(2));
            assert_eq!(view.library.queue.collection.as_ref().unwrap().order, PlaybackOrder::Original);
            view.playback_result(PlaybackResult::Watch {
                id:latest.id, account_revision:latest.account_revision, update:QueueUpdate::Replace,
                collection:Some(source.clone()), result:Err("Synthetic unavailable playlist".into()), session_rejected:false,
            },cx);
            assert_eq!(view.library.queue.index, Some(2));
            assert_eq!(view.error.as_deref(), Some("Synthetic unavailable playlist"));
            view.error = None;
        }).unwrap();
        super::render::key(window, cx, "s").await;
        window
            .update(cx, |view, _, cx| {
                let job = view.pending_playback_jobs.try_recv().unwrap();
                let PlaybackQuery::Collection(collection) = job.query else {
                    panic!("Shuffle needs its collection source")
                };
                view.playback_result(
                    PlaybackResult::Watch {
                        id: job.id,
                        account_revision: job.account_revision,
                        update: QueueUpdate::Replace,
                        collection: Some(collection),
                        result: Ok(Box::new(watch(true))),
                        session_rejected: false,
                    },
                    cx,
                );
                assert_eq!(view.library.queue.tracks, watch(true).queue);
                assert_eq!(view.library.queue.index, Some(0));
                assert!(
                    !view.library.queue.shuffled,
                    "Service queues must not enable local reshuffling or eager continuation loading"
                );
                assert_eq!(
                    view.library.queue.collection.as_ref().unwrap().order,
                    PlaybackOrder::Shuffled
                );
            })
            .unwrap();
        super::render::wait_for_presented_frame(window, cx).await;
        window
            .update(cx, |view, window, cx| {
                super::render::capture(window, "collection-shuffle-active");
                view.remove_queue_track(1, cx);
                assert_eq!(view.library.queue.tracks.len(), 2);
                // A current-song details read must not hide or replace the queue's source.
                view.playback_request(
                    PlaybackQuery::Watch {
                        track: Box::new(view.library.queue.current().unwrap().clone()),
                        mode: crate::watch::WatchMode::Details,
                    },
                    cx,
                );
                view.pending_playback_jobs.try_recv().unwrap();
            })
            .unwrap();
        super::render::key(window, cx, "s").await;
        window
            .update(cx, |view, _, cx| {
                let job = view.pending_playback_jobs.try_recv().unwrap();
                let PlaybackQuery::Collection(collection) = job.query else {
                    panic!("Unshuffle needs its collection source")
                };
                assert_eq!(collection.order, PlaybackOrder::Original);
                view.playback_result(
                    PlaybackResult::Watch {
                        id: job.id,
                        account_revision: job.account_revision,
                        update: QueueUpdate::Replace,
                        collection: Some(collection),
                        result: Ok(Box::new(watch(false))),
                        session_rejected: false,
                    },
                    cx,
                );
                assert_eq!(view.library.queue.tracks, tracks);
                assert_eq!(view.library.queue.index, Some(0));
                view.library.queue =
                    serde_json::from_value(serde_json::to_value(&view.library.queue).unwrap())
                        .unwrap();
                view.account_operation = Some(AccountOperation::Refreshing {
                    operation: auth::Operation::default(),
                    retry_change: false,
                    resume: None,
                });
                view.shuffle_queue(cx);
                view.shuffle_queue(cx);
                let Some(AccountOperation::Refreshing {
                    resume:
                        Some(PlaybackIntent::Queue {
                            query: PlaybackQuery::Collection(collection),
                            ..
                        }),
                    ..
                }) = &view.account_operation
                else {
                    panic!("Account recovery must retain the latest shuffle action")
                };
                assert_eq!(collection.order, PlaybackOrder::Original);
                assert!(view.pending_playback_jobs.is_empty());
                assert!(view.playback_generation.load(Ordering::Relaxed) > job.id);
                view.account_operation = None;
            })
            .unwrap();
    })
}

#[test]
#[ignore = "requires desktop, private D-Bus and isolated runtime/data; synthetic queue and local connection failures only"]
fn natural_advancement_retries_a_failed_prefetch_and_continues_the_queue() -> Result<()> {
    super::navigation::check_cache_window(async |window, cx| {
        let client = reqwest::blocking::Client::builder()
            .no_proxy()
            .build()
            .unwrap();
        let mut calls = 0;
        let (source, events) = crate::queue_source::Worker::with_page_loader(move || {
            calls += 1;
            if calls <= 2 {
                return Err(client
                    .get("http://127.0.0.1:0")
                    .send()
                    .unwrap_err()
                    .without_url()
                    .into());
            }
            Ok(continuation_page(100..102, None))
        });
        window
            .update(cx, move |view, _, cx| {
                view.account = None;
                view.library.autoplay = false;
                view.queue_source = source;
                view.queue_source.replace_queue(
                    &mut view.library.queue,
                    &continuation_page(0..100, Some("tail")),
                    99,
                    PageRequest::Browse,
                    None,
                );
                view.playback = Playback::Stopped;
                view.error = None;
                view.prefetch_queue(cx);
            })
            .unwrap();
        for _ in 0..2 {
            let event = events.recv().await.unwrap();
            window
                .update(cx, move |view, _, cx| view.queue_source_result(event, cx))
                .unwrap();
        }
        window
            .update(cx, |view, _, cx| {
                assert!(view.queue_source.error().is_some());
                assert_eq!(view.library.queue.tracks.len(), 100);
                assert_eq!(view.library.queue.index, Some(99));
                view.next(true, cx);
                assert!(
                    view.queue_waiting.is_some(),
                    "A natural end must retry the failed prefetch"
                );
                assert!(view.queue_source.error().is_none());
            })
            .unwrap();
        let event = events.recv().await.unwrap();
        window
            .update(cx, move |view, _, cx| {
                view.queue_source_result(event, cx);
                assert_eq!(view.library.queue.index, Some(100));
                assert_eq!(view.library.queue.current().unwrap().id, "00000000100");
                assert_eq!(view.library.queue.tracks.len(), 102);
                assert!(!view.library.queue.incomplete);
                assert!(view.queue_waiting.is_none());
                assert!(view.error.is_none());
            })
            .unwrap();
    })
}

#[test]
#[ignore = "requires desktop, private D-Bus and isolated runtime/data; synthetic queue and local connection failures only"]
fn queue_retry_button_preserves_pause_and_a_stopped_retry_cannot_start_playback() -> Result<()> {
    use gpui::{MouseButton, MouseDownEvent, MouseUpEvent, PlatformInput};
    super::navigation::check_cache_window(async |window, cx| {
        let client = reqwest::blocking::Client::builder()
            .no_proxy()
            .build()
            .unwrap();
        let mut calls = 0;
        let (source, events) = crate::queue_source::Worker::with_page_loader(move || {
            calls += 1;
            if calls <= 3 {
                return Err(client
                    .get("http://127.0.0.1:0")
                    .send()
                    .unwrap_err()
                    .without_url()
                    .into());
            }
            Ok(continuation_page(1..2, None))
        });
        window
            .update(cx, move |view, _, cx| {
                view.account = None;
                view.library.autoplay = false;
                view.queue_source = source;
                view.queue_source.replace_queue(
                    &mut view.library.queue,
                    &continuation_page(0..1, Some("tail")),
                    0,
                    PageRequest::Browse,
                    None,
                );
                view.playback = Playback::Loaded {
                    paused: true,
                    buffering: false,
                };
                view.player_open = true;
                view.error = None;
                view.prefetch_queue(cx);
            })
            .unwrap();
        for _ in 0..2 {
            let event = events.recv().await.unwrap();
            window
                .update(cx, move |view, _, cx| view.queue_source_result(event, cx))
                .unwrap();
        }
        window
            .update(cx, |view, _, cx| {
                assert!(
                    view.playback.is_paused(),
                    "Failed prefetch must not change the current song's pause state"
                );
                view.next(false, cx);
                assert!(view.queue_waiting.unwrap().playback.is_paused());
            })
            .unwrap();
        let event = events.recv().await.unwrap();
        window
            .update(cx, move |view, _, cx| {
                view.queue_source_result(event, cx);
                assert_eq!(view.playback, Playback::Stopped);
                assert!(view.queue_waiting.unwrap().playback.is_paused());
                assert!(view.queue_source.error().is_some());
                assert!(view.library.queue.incomplete);
                assert_eq!(view.library.queue.index, Some(0));
            })
            .unwrap();
        super::render::wait_for_presented_frame(window, cx).await;
        let position = window
            .update(cx, |view, window, _| {
                super::render::capture(window, "queue-continuation-retry");
                let bounds = view.player_page.queue_bounds.get();
                point(bounds.right() - px(36.), bounds.bottom() + px(28.))
            })
            .unwrap();
        for event in [
            PlatformInput::MouseDown(MouseDownEvent {
                button: MouseButton::Left,
                position,
                modifiers: Default::default(),
                click_count: 1,
                first_mouse: false,
            }),
            PlatformInput::MouseUp(MouseUpEvent {
                button: MouseButton::Left,
                position,
                modifiers: Default::default(),
                click_count: 1,
            }),
        ] {
            cx.update_window(window.into(), |_, window, cx| {
                window.dispatch_event(event, cx)
            })
            .unwrap();
            super::render::wait_for_presented_frame(window, cx).await;
        }
        window
            .update(cx, |view, _, cx| {
                assert!(
                    view.queue_source.error().is_none(),
                    "Clicking Retry must dispatch a new continuation request"
                );
                assert!(view.queue_waiting.unwrap().playback.is_paused());
                assert!(
                    view.playback.is_paused(),
                    "Retry must preserve the paused queue's intent"
                );
                view.stop_playback(cx);
            })
            .unwrap();
        let event = events.recv().await.unwrap();
        window
            .update(cx, move |view, _, cx| {
                view.queue_source_result(event, cx);
                assert_eq!(view.library.queue.tracks.len(), 2);
                assert_eq!(view.library.queue.index, Some(0));
                assert_eq!(view.playback, Playback::Stopped);
                assert!(view.active_id.is_none());
                assert!(view.queue_waiting.is_none());
                assert!(view.error.is_none());
            })
            .unwrap();
    })
}

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
            let player = music.player.as_ref().unwrap();
            player.set_volume(0.).unwrap_or_else(|error| panic!("{error:#}; startup events: {:?}", std::iter::from_fn(|| player.try_recv()).collect::<Vec<_>>()));
            music.player_open = true;
            music
        })).unwrap();
        cx.activate(true);
        cx.spawn(async move |cx| {
            let outcome: Result<()> = async {
                let response = window.update(cx, |music, _, _| {
                    music.player.as_ref().unwrap().set_credentials(auth::PlaybackCredentials {
                        message: [br##"{"cookies":"# Netscape HTTP Cookie File\n","account":{"authuser":"0","delegated_session_id":null,"data_sync_id":"synthetic||"}}"##.as_slice(), b"\n"].concat(),
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
