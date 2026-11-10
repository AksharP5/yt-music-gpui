use super::{CatalogTarget, Music, Playback, Startup};
use crate::player::PlayerEvent;
use anyhow::{Context as _, Result, ensure};
use gpui::{AppContext, Bounds, TitlebarOptions, WindowBounds, WindowOptions, point, px, size};
use std::sync::atomic::AtomicBool;
use std::{
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

#[test]
#[ignore = "requires desktop, private D-Bus and isolated XDG runtime/data; set YT_MUSIC_NATIVE_RENDER_TEST=1"]
fn position_samples_only_render_the_player_bar() -> Result<()> {
    ensure!(
        std::env::var("YT_MUSIC_NATIVE_RENDER_TEST").as_deref() == Ok("1"),
        "Run with an isolated native guest environment"
    );
    let runtime = std::path::PathBuf::from(
        std::env::var_os("XDG_RUNTIME_DIR").context("Set private XDG_RUNTIME_DIR")?,
    );
    let data = std::path::PathBuf::from(
        std::env::var_os("XDG_DATA_HOME").context("Set private XDG_DATA_HOME")?,
    );
    ensure!(runtime.starts_with("/tmp") && data.starts_with(&runtime) && data != runtime);
    crate::video::initialize()?;
    let passed = Arc::new(AtomicBool::new(false));
    let result = passed.clone();
    gpui_platform::application()
        .with_assets(crate::assets::Assets)
        .run(move |cx| {
            cx.text_system()
                .add_fonts(vec![std::borrow::Cow::Borrowed(
                    include_bytes!("../../assets/fonts/Roboto.ttf").as_slice(),
                )])
                .unwrap();
            crate::input::init(cx);
            let window = cx
                .open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                            point(px(40.), px(40.)),
                            size(px(850.), px(560.)),
                        ))),
                        titlebar: Some(TitlebarOptions {
                            title: Some("Music render isolation test".into()),
                            ..Default::default()
                        }),
                        focus: false,
                        ..Default::default()
                    },
                    |window, cx| {
                        cx.new(|cx| {
                            let mut music = Music::new(Startup::Guest, window, cx);
                            for target in CatalogTarget::ALL {
                                music.catalog_requests.cancel(target);
                            }
                            music.player = None;
                            music.mpris = None;
                            music.video = None;
                            music.store = None;
                            music.library.volume = 0.;
                            music.library.queue.replace(
                                vec![serde_json::from_value(serde_json::json!({
                                    "id": "render-test", "title": "Render test", "artist": "Test"
                                })).unwrap()],
                                0,
                            );
                            music.active_id = Some(42);
                            music.playback = Playback::Loaded {
                                paused: false,
                                buffering: false,
                            };
                            music.duration = 120.;
                            music
                        })
                    },
                )
                .unwrap();
            cx.spawn(async move |cx| {
                cx.background_executor()
                    .timer(Duration::from_millis(500))
                    .await;
                let before = window
                    .update(cx, |music, _, cx| {
                        (
                            music.content_view.read(cx).renders,
                            music.player_bar.read(cx).renders,
                        )
                    })
                    .unwrap();
                for seconds in 1..=5 {
                    window
                        .update(cx, |music, _, cx| {
                            music.player_event(
                                PlayerEvent::Position {
                                    track_id: 42,
                                    seconds: seconds as f64,
                                },
                                cx,
                            );
                        })
                        .unwrap();
                    cx.background_executor()
                        .timer(Duration::from_millis(100))
                        .await;
                }
                let after = window
                    .update(cx, |music, _, cx| {
                        (
                            music.content_view.read(cx).renders,
                            music.player_bar.read(cx).renders,
                        )
                    })
                    .unwrap();
                assert_eq!(
                    after.0, before.0,
                    "Position samples must reuse the page body"
                );
                assert!(
                    after.1 >= before.1 + 5,
                    "Each position update must reach the player bar"
                );
                window
                    .update(cx, |music, _, cx| {
                        music.player_event(
                            PlayerEvent::Position {
                                track_id: 41,
                                seconds: 99.,
                            },
                            cx,
                        );
                        assert_eq!(music.position, 5., "An old track must not update the clock");
                        music.player_event(
                            PlayerEvent::Duration {
                                track_id: 42,
                                seconds: 180.,
                            },
                            cx,
                        );
                    })
                    .unwrap();
                cx.background_executor()
                    .timer(Duration::from_millis(100))
                    .await;
                window
                    .update(cx, |music, _, cx| {
                        assert!(
                            music.content_view.read(cx).renders > after.0,
                            "Other playback changes must still update the page"
                        );
                        assert_eq!(music.duration, 180.);
                    })
                    .unwrap();
                result.store(true, Ordering::SeqCst);
                cx.update(|cx| cx.quit());
            })
            .detach();
            cx.spawn(async move |cx| {
                cx.background_executor()
                    .timer(Duration::from_secs(15))
                    .await;
                cx.update(|cx| cx.quit());
            })
            .detach();
        });
    ensure!(
        passed.load(Ordering::SeqCst),
        "Native render test exited before its assertions"
    );
    Ok(())
}
