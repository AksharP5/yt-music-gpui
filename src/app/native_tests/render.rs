use crate::app::{CatalogTarget, Music, Playback, Startup, player_page::PlayerTab};
use crate::player::PlayerEvent;
use anyhow::{Result, ensure};
use gpui::{
    AppContext, AsyncApp, Bounds, TitlebarOptions, WindowBounds, WindowHandle, WindowOptions,
    point, px, size,
};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::sync::atomic::AtomicBool;
use std::{
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

async fn wait_for_presented_frame(window: WindowHandle<Music>, cx: &mut AsyncApp) {
    let (sender, received) = async_channel::bounded(1);
    cx.update_window(window.into(), |_, window, _| {
        // GPUI invokes next-frame callbacks before drawing. The second callback
        // runs after the first frame has finished drawing and presenting.
        window.on_next_frame(move |window, _| {
            window.on_next_frame(move |_, _| {
                let _ = sender.try_send(());
            });
        });
    })
    .unwrap();
    received.recv().await.unwrap();
}

fn capture(window: &gpui::Window, name: &str) -> image::RgbaImage {
    let id = match HasWindowHandle::window_handle(window).unwrap().as_raw() {
        RawWindowHandle::Xcb(handle) => u64::from(handle.window.get()),
        RawWindowHandle::Xlib(handle) => handle.window,
        _ => panic!("Run the pixel regression check on X11, with WAYLAND_DISPLAY unset"),
    };
    let path = std::path::PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").unwrap())
        .join(format!("{name}.png"));
    let output = std::process::Command::new("import")
        .args(["-window", &id.to_string()])
        .arg(&path)
        .output()
        .expect("Install ImageMagick for native pixel checks");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    image::open(path).unwrap().to_rgba8()
}

fn assert_page_retained(baseline: &image::RgbaImage, image: &image::RgbaImage) {
    assert_eq!(image.dimensions(), baseline.dimensions());
    let body_height = baseline.height().saturating_sub(100);
    let changed = baseline
        .enumerate_pixels()
        .filter(|(x, y, pixel)| *y >= 65 && *y < body_height && image.get_pixel(*x, *y) != *pixel)
        .count();
    let fraction = changed as f64 / f64::from(baseline.width() * (body_height - 65));
    let mut visible = 0;
    let mut missing = 0;
    for (x, y, pixel) in baseline.enumerate_pixels() {
        if y < 65 || y >= body_height || pixel.0[..3].iter().all(|channel| *channel < 100) {
            continue;
        }
        visible += 1;
        if image.get_pixel(x, y).0[..3]
            .iter()
            .all(|channel| *channel < 50)
        {
            missing += 1;
        }
    }
    assert!(
        visible > 100,
        "The baseline must contain visible page content"
    );
    let missing = missing as f64 / f64::from(visible);
    println!("TAB_PIXELS changed={fraction:.4} missing_content={missing:.4}");
    assert!(
        missing < 0.02,
        "Tab must retain the sidebar, artwork and tab content, missing {missing:.4}"
    );
    assert!(
        fraction < 0.02,
        "Tab must preserve the painted page body, changed {fraction:.4}"
    );
}

#[test]
#[ignore = "requires desktop, private D-Bus and isolated XDG runtime/data; set YT_MUSIC_NATIVE_TESTS=1"]
fn position_samples_only_render_the_player_bar() -> Result<()> {
    super::isolate()?;
    let passed = Arc::new(AtomicBool::new(false));
    let result = passed.clone();
    gpui_platform::application()
        .with_assets(crate::assets::Assets)
        .run(move |cx| {
            cx.text_system()
                .add_fonts(vec![std::borrow::Cow::Borrowed(
                    include_bytes!("../../../assets/fonts/Roboto.ttf").as_slice(),
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
                            // Deliver Started before the first frame: hidden windows must prepare too.
                            music.library.queue.replace(
                                ["dQw4w9WgXcQ", "fJ9rUzIMcZQ"]
                                    .map(|id| {
                                        serde_json::from_value(
                                            serde_json::json!({"id":id,"title":id,"artist":"Test"}),
                                        )
                                        .unwrap()
                                    })
                                    .to_vec(),
                                0,
                            );
                            music.active_id = Some(42);
                            music.player_event(
                                PlayerEvent::Started {
                                    track_id: 42,
                                    paused: false,
                                },
                                cx,
                            );
                            assert_eq!(music.content_view.read(cx).renders, 0);
                            assert_eq!(
                                music.player.as_ref().unwrap().next_preparation().as_deref(),
                                Some("https://music.youtube.com/watch?v=fJ9rUzIMcZQ")
                            );
                            println!("STARTED_WITHOUT_RENDER prepared=fJ9rUzIMcZQ");
                            music.catalog_requests = crate::app::CatalogRequests::new();
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
                            // Playback opens the player page, so measure with it showing.
                            music.player_open = true;
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
                            music.renders,
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
                            music.renders,
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
                println!(
                    "POSITION_RENDERS body={} bar={} root={}",
                    after.0 - before.0,
                    after.1 - before.1,
                    after.2 - before.2
                );
                // A wheel tick over artwork has no scrolling work to do.
                let before_wheel = window
                    .update(cx, |music, _, cx| {
                        (
                            music.content_view.read(cx).renders,
                            music.player_bar.read(cx).renders,
                            music.renders,
                        )
                    })
                    .unwrap();
                for _ in 0..10 {
                    cx.update_window(window.into(), |_, window, cx| {
                        window.dispatch_event(
                            gpui::PlatformInput::ScrollWheel(gpui::ScrollWheelEvent {
                                position: point(window.viewport_size().width * 0.4, px(300.)),
                                delta: gpui::ScrollDelta::Pixels(point(px(0.), px(-50.))),
                                modifiers: Default::default(),
                                touch_phase: gpui::TouchPhase::Moved,
                            }),
                            cx,
                        );
                    })
                    .unwrap();
                    cx.background_executor()
                        .timer(Duration::from_millis(40))
                        .await;
                }
                let after_wheel = window
                    .update(cx, |music, _, cx| {
                        (
                            music.content_view.read(cx).renders,
                            music.player_bar.read(cx).renders,
                            music.renders,
                        )
                    })
                    .unwrap();
                println!(
                    "IDLE_WHEEL_RENDERS body={} bar={} root={}",
                    after_wheel.0 - before_wheel.0,
                    after_wheel.1 - before_wheel.1,
                    after_wheel.2 - before_wheel.2
                );
                assert_eq!(
                    after_wheel, before_wheel,
                    "A wheel tick outside a scroller must not redraw playback"
                );
                for tab in [PlayerTab::Lyrics, PlayerTab::Related] {
                    window
                        .update(cx, |music, _, cx| music.select_player_tab(tab, cx))
                        .unwrap();
                    wait_for_presented_frame(window, cx).await;
                    let baseline = window
                        .update(cx, |music, window, cx| {
                            println!(
                                "BASELINE requested={tab:?} model={:?} body_renders={}",
                                music.player_page.selected_tab(),
                                music.content_view.read(cx).renders
                            );
                            assert_eq!(music.player_page.selected_tab(), tab);
                            capture(window, &format!("{tab:?}-before"))
                        })
                        .unwrap();
                    for step in 0..16 {
                        cx.update_window(window.into(), |root, window, cx| {
                            let music = root.downcast::<Music>().unwrap();
                            let before_tab = music.read(cx).player_page.selected_tab();
                            let before = window.focused(cx);
                            assert!(
                                window
                                    .dispatch_keystroke(gpui::Keystroke::parse("tab").unwrap(), cx)
                            );
                            assert_ne!(window.focused(cx), before, "Tab must move focus");
                            assert_eq!(
                                music.read(cx).player_page.selected_tab(),
                                before_tab,
                                "Tab must preserve the selected player tab"
                            );
                            assert_eq!(before_tab, tab);
                        })
                        .unwrap();
                        wait_for_presented_frame(window, cx).await;
                        let focused = window
                            .update(cx, |music, window, _| {
                                assert_eq!(
                                    music.player_page.selected_tab(),
                                    tab,
                                    "Focus must preserve the selected player tab"
                                );
                                capture(window, &format!("{tab:?}-focus-{step}"))
                            })
                            .unwrap();
                        assert_page_retained(&baseline, &focused);
                        window
                            .update(cx, |music, _, cx| {
                                music.player_event(
                                    PlayerEvent::Position {
                                        track_id: 42,
                                        seconds: f64::from(10 + step),
                                    },
                                    cx,
                                )
                            })
                            .unwrap();
                        wait_for_presented_frame(window, cx).await;
                        let image = window
                            .update(cx, |music, window, _| {
                                assert_eq!(
                                    music.player_page.selected_tab(),
                                    tab,
                                    "Position redraws must preserve the selected player tab"
                                );
                                capture(window, &format!("{tab:?}-tab-{step}"))
                            })
                            .unwrap();
                        assert_page_retained(&baseline, &image);
                    }
                }
                window
                    .update(cx, |music, _, cx| {
                        music.player_open = false;
                        music.route = crate::app::Route::Browse(crate::catalog::BrowseLink::new(
                            "VLsynthetic",
                            "Scroll test",
                        ));
                        music.page = crate::app::sync::tests::page(500, true);
                        music.rebuild_rows();
                        cx.notify();
                    })
                    .unwrap();
                cx.background_executor()
                    .timer(Duration::from_millis(200))
                    .await;
                for _ in 0..10 {
                    cx.update_window(window.into(), |_, window, cx| {
                        window.dispatch_event(
                            gpui::PlatformInput::ScrollWheel(gpui::ScrollWheelEvent {
                                position: point(window.viewport_size().width * 0.75, px(400.)),
                                delta: gpui::ScrollDelta::Pixels(point(px(0.), px(-5000.))),
                                modifiers: Default::default(),
                                touch_phase: gpui::TouchPhase::Moved,
                            }),
                            cx,
                        );
                    })
                    .unwrap();
                    cx.background_executor()
                        .timer(Duration::from_millis(40))
                        .await;
                }
                window
                    .update(cx, |music, _, _| {
                        assert!(music.list_scroll.0.borrow().base_handle.offset().y < px(-1000.));
                        assert!(
                            !music.catalog_requests.pending.is_empty(),
                            "Scrolling near the end must request the next page"
                        );
                        println!(
                            "SCROLL_PREFETCH offset={:?} pending=true",
                            music.list_scroll.0.borrow().base_handle.offset()
                        );
                    })
                    .unwrap();
                window
                    .update(cx, |music, _, cx| {
                        music.route = crate::app::Route::Home;
                        music.page.continuation = None;
                        cx.notify();
                    })
                    .unwrap();
                cx.background_executor()
                    .timer(Duration::from_millis(200))
                    .await;
                for _ in 0..5 {
                    cx.update_window(window.into(), |_, window, cx| {
                        window.dispatch_event(
                            gpui::PlatformInput::ScrollWheel(gpui::ScrollWheelEvent {
                                position: point(window.viewport_size().width * 0.75, px(300.)),
                                delta: gpui::ScrollDelta::Pixels(point(px(-500.), px(0.))),
                                modifiers: Default::default(),
                                touch_phase: gpui::TouchPhase::Moved,
                            }),
                            cx,
                        );
                    })
                    .unwrap();
                    cx.background_executor()
                        .timer(Duration::from_millis(40))
                        .await;
                }
                window
                    .update(cx, |music, window, _| {
                        assert!(music.shelf_scroll[&0].offset().x < px(-100.));
                        println!(
                            "CAROUSEL_SCROLL offset={:?}",
                            music.shelf_scroll[&0].offset()
                        );
                        capture(window, "carousel-scrolled");
                    })
                    .unwrap();
                result.store(true, Ordering::SeqCst);
                cx.update(|cx| cx.quit());
            })
            .detach();
            cx.spawn(async move |cx| {
                cx.background_executor()
                    .timer(Duration::from_secs(90))
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
