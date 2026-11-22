//! Search box input with the real window event path.
use super::navigation::check_cache_window;
use super::render::{capture, key, wait_for_presented_frame};
use crate::app::{Music, Route};
use crate::library::Repeat;
use gpui::{
    AppContext, AsyncApp, Focusable, MouseButton, MouseDownEvent, MouseUpEvent, PlatformInput,
    ScrollDelta, ScrollWheelEvent, TouchPhase, WindowHandle, point, px, size,
};

async fn type_with_suggestions(window: WindowHandle<Music>, cx: &mut AsyncApp) {
    window
        .update(cx, |music, window, cx| {
            let focus = music.search.focus_handle(cx);
            window.focus(&focus, cx);
            music.search.update(cx, |search, cx| {
                search.set_query("typed query".into(), cx);
                search.show_suggestions(&["first suggestion", "second suggestion"], cx);
            });
        })
        .unwrap();
    wait_for_presented_frame(window, cx).await;
}

fn field(window: WindowHandle<Music>, cx: &mut AsyncApp) -> String {
    window
        .update(cx, |music, _, cx| {
            music.search.read(cx).value(cx).to_owned()
        })
        .unwrap()
}

fn search_focused(window: WindowHandle<Music>, cx: &mut AsyncApp) -> bool {
    window
        .update(cx, |music, window, cx| {
            music.search.focus_handle(cx).is_focused(window)
        })
        .unwrap()
}

fn mouse(window: WindowHandle<Music>, cx: &mut AsyncApp, x: f32, y: f32, down: bool) {
    cx.update_window(window.into(), |_, window, cx| {
        let position = point(px(x), px(y));
        window.dispatch_event(
            if down {
                PlatformInput::MouseDown(MouseDownEvent {
                    button: MouseButton::Left,
                    position,
                    modifiers: Default::default(),
                    click_count: 1,
                    first_mouse: false,
                })
            } else {
                PlatformInput::MouseUp(MouseUpEvent {
                    button: MouseButton::Left,
                    position,
                    modifiers: Default::default(),
                    click_count: 1,
                })
            },
            cx,
        );
    })
    .unwrap();
}

/// Open search the way the narrow header's search button does.
async fn open_search(window: WindowHandle<Music>, cx: &mut AsyncApp) {
    window
        .update(cx, |music, window, cx| music.open_search(window, cx))
        .unwrap();
    wait_for_presented_frame(window, cx).await;
}

fn searched(window: WindowHandle<Music>, cx: &mut AsyncApp) -> String {
    window
        .update(cx, |music, _, _| match &music.route {
            Route::Search(search) => search.query.clone(),
            _ => String::new(),
        })
        .unwrap()
}

#[test]
#[ignore = "requires desktop, private D-Bus and isolated runtime/data; synthetic accounts only"]
fn letter_shortcuts_work_outside_search_and_type_inside_it() -> anyhow::Result<()> {
    check_cache_window(async |window, cx| {
        let repeat = |cx: &mut AsyncApp| window.update(cx, |music, _, _| music.library.repeat);
        window
            .update(cx, |music, window, cx| window.focus(&music.focus, cx))
            .unwrap();
        assert_eq!(repeat(cx).unwrap(), Repeat::Off);
        key(window, cx, "r").await;
        assert_eq!(repeat(cx).unwrap(), Repeat::All);
        key(window, cx, "r").await;
        assert_eq!(repeat(cx).unwrap(), Repeat::One);

        window
            .update(cx, |music, window, cx| {
                window.focus(&music.search.focus_handle(cx), cx)
            })
            .unwrap();
        key(window, cx, "r").await;
        key(window, cx, "enter").await;
        assert_eq!(searched(window, cx), "r");
        assert_eq!(repeat(cx).unwrap(), Repeat::One);
    })
}

#[test]
#[ignore = "requires desktop, private D-Bus and isolated runtime/data; synthetic accounts only"]
fn enter_searches_the_typed_text_unless_a_suggestion_was_chosen_with_the_keyboard()
-> anyhow::Result<()> {
    check_cache_window(async |window, cx| {
        type_with_suggestions(window, cx).await;
        // At 1280px the 480px box starts at 340px and its first suggestion row is centered 83px down.
        cx.update_window(window.into(), |_, window, cx| {
            window.dispatch_event(
                PlatformInput::MouseMove(gpui::MouseMoveEvent {
                    position: point(px(580.), px(83.)),
                    pressed_button: None,
                    modifiers: Default::default(),
                }),
                cx,
            );
        })
        .unwrap();
        wait_for_presented_frame(window, cx).await;
        key(window, cx, "enter").await;
        assert_eq!(
            searched(window, cx),
            "typed query",
            "A suggestion under the pointer must not replace the typed text"
        );

        type_with_suggestions(window, cx).await;
        key(window, cx, "down").await;
        key(window, cx, "down").await;
        key(window, cx, "enter").await;
        assert_eq!(searched(window, cx), "second suggestion");
    })
}

#[test]
#[ignore = "requires desktop, private D-Bus and isolated runtime/data; synthetic accounts only"]
fn arrows_preview_suggestions_and_escape_closes_in_one_press() -> anyhow::Result<()> {
    check_cache_window(async |window, cx| {
        type_with_suggestions(window, cx).await;
        // The web cycles through the rows and the typed text, previewing each row's query.
        for (pressed, shown) in [
            ("down", "first suggestion"),
            ("down", "second suggestion"),
            ("down", "typed query"),
            ("up", "second suggestion"),
            ("up", "first suggestion"),
            ("up", "typed query"),
            ("down", "first suggestion"),
        ] {
            key(window, cx, pressed).await;
            assert_eq!(field(window, cx), shown, "after {pressed}");
        }

        key(window, cx, "escape").await;
        assert_eq!(field(window, cx), "typed query");
        assert!(
            !search_focused(window, cx),
            "One Escape must leave the field"
        );

        // Clicking back into the field reopens the same suggestions. At 1280px the box spans
        // 340-820px and its first suggestion row is centered 83px down.
        mouse(window, cx, 580., 32., true);
        mouse(window, cx, 580., 32., false);
        wait_for_presented_frame(window, cx).await;
        assert!(search_focused(window, cx));
        mouse(window, cx, 580., 83., true);
        wait_for_presented_frame(window, cx).await;
        assert!(
            searched(window, cx).is_empty(),
            "A suggestion must open on release, not on press"
        );
        mouse(window, cx, 580., 83., false);
        wait_for_presented_frame(window, cx).await;
        assert_eq!(searched(window, cx), "first suggestion");
    })
}

#[test]
#[ignore = "requires desktop, private D-Bus and isolated runtime/data; synthetic accounts only"]
fn typing_requests_at_once_then_throttles_to_the_newest_text() -> anyhow::Result<()> {
    use std::time::{Duration, Instant};

    check_cache_window(async |window, cx| {
        let requests = window
            .update(cx, |music, window, cx| {
                window.focus(&music.search.focus_handle(cx), cx);
                music
                    .search
                    .update(cx, |search, _| search.intercept_requests())
            })
            .unwrap();
        wait_for_presented_frame(window, cx).await;
        // Each key is its own event, without waiting for a frame in between.
        let typed = |cx: &mut AsyncApp, keys: &[&str]| {
            for key in keys {
                cx.update_window(window.into(), |_, window, cx| {
                    window.dispatch_keystroke(gpui::Keystroke::parse(key).unwrap(), cx);
                })
                .unwrap();
            }
        };
        let collect = async |cx: &mut AsyncApp, start: Instant| {
            let mut sent = Vec::new();
            while start.elapsed() < Duration::from_millis(400) {
                while let Some((query, _)) = requests() {
                    sent.push((query, start.elapsed()));
                }
                cx.background_executor()
                    .timer(Duration::from_millis(5))
                    .await;
            }
            sent
        };

        let start = Instant::now();
        typed(cx, &["l", "o", "f", "i"]);
        let first = requests().map(|(query, _)| query);
        assert_eq!(
            first.as_deref(),
            Some("l"),
            "The first keystroke must be requested without a pause"
        );
        let sent = collect(cx, start).await;
        assert_eq!(
            sent.iter()
                .map(|(query, _)| query.as_str())
                .collect::<Vec<_>>(),
            ["lofi"],
            "Keystrokes within 100ms must collapse into one request for the newest text"
        );
        assert!(sent[0].1 >= Duration::from_millis(100), "{sent:?}");

        // Leaving the field drops a request still waiting for its turn.
        let start = Instant::now();
        typed(cx, &["x", "y", "escape"]);
        let sent = collect(cx, start).await;
        assert_eq!(
            sent.iter()
                .map(|(query, _)| query.as_str())
                .collect::<Vec<_>>(),
            ["lofix"]
        );
    })
}

#[test]
#[ignore = "requires desktop, private D-Bus and isolated runtime/data; synthetic accounts only"]
fn song_suggestion_actions_never_open_sign_in_for_a_guest() -> anyhow::Result<()> {
    use crate::app::Overlay;
    use crate::catalog::{Item, Track};

    check_cache_window(async |window, cx| {
        let track: Track = serde_json::from_value(
            serde_json::json!({"id":"song","title":"Song","artist":"Synthetic"}),
        )
        .unwrap();
        window
            .update(cx, |music, window, cx| {
                music.account = None;
                window.focus(&music.search.focus_handle(cx), cx);
                music.search.update(cx, |search, cx| {
                    search.set_query("song".into(), cx);
                    search.show_items(vec![Item::Track(track)], cx);
                });
            })
            .unwrap();
        wait_for_presented_frame(window, cx).await;
        // At 1280px the only row spans 59-115px, and its actions end 12px inside the 820px edge,
        // where the web puts Like at 682, Dislike at 720 and the menu at 772.
        cx.update_window(window.into(), |_, window, cx| {
            window.dispatch_event(
                PlatformInput::MouseMove(gpui::MouseMoveEvent {
                    position: point(px(580.), px(87.)),
                    pressed_button: None,
                    modifiers: Default::default(),
                }),
                cx,
            );
        })
        .unwrap();
        wait_for_presented_frame(window, cx).await;
        let click = async |cx: &mut AsyncApp, x: f32| {
            mouse(window, cx, x, 87., true);
            mouse(window, cx, x, 87., false);
            wait_for_presented_frame(window, cx).await;
        };

        click(cx, 699.).await;
        window
            .update(cx, |music, _, _| {
                assert!(music.library.liked.iter().any(|liked| liked.id == "song"));
                assert!(music.overlay.is_none());
            })
            .unwrap();

        click(cx, 737.).await;
        window
            .update(cx, |music, _, _| {
                assert_eq!(music.notice.as_deref(), Some("Sign in to dislike songs"));
                assert!(music.overlay.is_none() && music.account_operation.is_none());
            })
            .unwrap();
        assert!(
            searched(window, cx).is_empty(),
            "An action must not open the row"
        );

        click(cx, 789.).await;
        window
            .update(cx, |music, _, _| {
                assert!(matches!(music.overlay, Some(Overlay::Track { .. })));
            })
            .unwrap();
    })
}

#[test]
#[ignore = "requires desktop, private D-Bus and isolated runtime/data; synthetic accounts only"]
fn tab_reaches_the_clear_button() -> anyhow::Result<()> {
    check_cache_window(async |window, cx| {
        type_with_suggestions(window, cx).await;
        key(window, cx, "tab").await;
        assert!(!search_focused(window, cx));
        assert_eq!(field(window, cx), "typed query");
        key(window, cx, "enter").await;
        assert_eq!(field(window, cx), "");
        assert!(
            search_focused(window, cx),
            "Clearing must return to the field"
        );
    })
}

#[test]
#[ignore = "requires X11, private D-Bus and isolated runtime/data; synthetic accounts only"]
fn search_suggestions_fit_the_narrow_field() -> anyhow::Result<()> {
    check_cache_window(async |window, cx| {
        window
            .update(cx, |music, window, cx| {
                music.sidebar_collapsed = false;
                window.resize(size(px(850.), px(560.)));
                cx.notify();
            })
            .unwrap();
        wait_for_presented_frame(window, cx).await;
        // A narrow window shows the field once its search button opens it.
        open_search(window, cx).await;
        type_with_suggestions(window, cx).await;

        let click = |cx: &mut AsyncApp, x: f32| {
            mouse(window, cx, x, 83., true);
            mouse(window, cx, x, 83., false);
        };
        window
            .update(cx, |_, window, _| {
                capture(window, "narrow-search-suggestions")
            })
            .unwrap();

        // This point is under the account controls, past the signed-in search field.
        click(cx, 740.);
        wait_for_presented_frame(window, cx).await;
        assert!(
            searched(window, cx).is_empty(),
            "Clicking outside must not select a suggestion"
        );

        // Clicking outside folds the field, but only once the window has focus, which
        // a test window never takes. Open it again as a user would.
        open_search(window, cx).await;
        type_with_suggestions(window, cx).await;
        click(cx, 400.);
        wait_for_presented_frame(window, cx).await;
        assert_eq!(searched(window, cx), "first suggestion");
    })
}

#[test]
#[ignore = "requires X11, private D-Bus and isolated runtime/data; synthetic accounts only"]
fn last_search_suggestion_is_clickable_in_a_short_window() -> anyhow::Result<()> {
    check_cache_window(async |window, cx| {
        window
            .update(cx, |music, window, cx| {
                music.sidebar_collapsed = false;
                window.resize(size(px(850.), px(560.)));
                cx.notify();
            })
            .unwrap();
        wait_for_presented_frame(window, cx).await;
        let suggestions = (1..=12)
            .map(|i| format!("suggestion {i}"))
            .collect::<Vec<_>>();
        let suggestions = suggestions.iter().map(String::as_str).collect::<Vec<_>>();
        for by_keyboard in [true, false] {
            // Each pass opens the field again, since leaving for a result folds it.
            open_search(window, cx).await;
            window
                .update(cx, |music, _, cx| {
                    music.search.update(cx, |search, cx| {
                        search.set_query("typed query".into(), cx);
                        search.show_suggestions(&suggestions, cx);
                    });
                })
                .unwrap();
            wait_for_presented_frame(window, cx).await;

            if by_keyboard {
                key(window, cx, "up").await;
            } else {
                cx.update_window(window.into(), |_, window, cx| {
                    window.dispatch_event(
                        PlatformInput::ScrollWheel(ScrollWheelEvent {
                            position: point(px(400.), px(200.)),
                            delta: ScrollDelta::Pixels(point(px(0.), px(-200.))),
                            modifiers: Default::default(),
                            touch_phase: TouchPhase::Moved,
                        }),
                        cx,
                    );
                })
                .unwrap();
                wait_for_presented_frame(window, cx).await;
            }

            let tail = cx
                .update_window(window.into(), |_, window, _| {
                    if by_keyboard {
                        capture(window, "compact-suggestion-tail");
                    }
                    f32::from(window.viewport_size().height) - 28.
                })
                .unwrap();
            mouse(window, cx, 400., tail, true);
            mouse(window, cx, 400., tail, false);
            wait_for_presented_frame(window, cx).await;
            assert_eq!(searched(window, cx), "suggestion 12");
            // Use a distinct route so the next click cannot pass by keeping this result.
            window
                .update(cx, |music, _, cx| music.navigate(Route::Home, cx))
                .unwrap();
            wait_for_presented_frame(window, cx).await;
        }
    })
}

#[test]
#[ignore = "requires desktop, private D-Bus and isolated runtime/data; synthetic accounts only"]
fn denied_autocomplete_retries_the_focused_query_after_renewal() -> anyhow::Result<()> {
    use crate::account::{Command, Event};
    use crate::player::{Player, PlayerOptions};
    use std::{
        sync::Arc,
        time::{Duration, Instant},
    };

    check_cache_window(async |window, cx| {
        let next = super::account::session("autocomplete-renewed");
        let (sender, commands) = async_channel::bounded(2);
        let (old, requests) = window
            .update(cx, |view, window, cx| {
                let old = view.account.as_ref().unwrap().session.clone();
                *view.session.write().unwrap() = Some(old.clone());
                view.account_commands = sender;
                view.player = Some(
                    Player::new(PlayerOptions {
                        audio_output: Some("null".into()),
                        volume: 0.,
                        ..Default::default()
                    })
                    .unwrap(),
                );
                window.focus(&view.search.focus_handle(cx), cx);
                let requests = view.search.update(cx, |search, cx| {
                    search.set_query("unfinished quer".into(), cx);
                    search.intercept_requests()
                });
                (old, requests)
            })
            .unwrap();
        key(window, cx, "y").await;
        let deadline = Instant::now() + Duration::from_secs(5);
        let (query, session) = loop {
            if let Some(request) = requests() {
                break request;
            }
            assert!(Instant::now() < deadline, "Typing must request suggestions");
            cx.background_executor()
                .timer(Duration::from_millis(15))
                .await;
        };
        assert_eq!(query, "unfinished query");
        assert!(Arc::ptr_eq(&session.unwrap(), &old));
        window
            .update(cx, |view, window, cx| {
                view.search.update(cx, |search, cx| {
                    let unrelated = super::account::session("stale-suggestion");
                    search.reject_test_suggestion(unrelated, window, cx);
                    search.reject_test_suggestion(old, window, cx);
                });
            })
            .unwrap();
        wait_for_presented_frame(window, cx).await;
        let Command::Refresh {
            operation,
            expected,
        } = commands
            .try_recv()
            .expect("Autocomplete must request a reconnect")
        else {
            panic!("Expected an account refresh");
        };
        key(window, cx, "x").await;
        window
            .update(cx, |view, window, cx| {
                assert!(Arc::ptr_eq(
                    &expected,
                    &view.account.as_ref().unwrap().session
                ));
                view.account_event(
                    Event::Connected {
                        operation,
                        result: Ok(Some(super::account::connection(next.clone()))),
                        save_session: false,
                    },
                    window,
                    cx,
                );
            })
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let query = loop {
            if let Some((query, session)) = requests() {
                let session = session.expect("Autocomplete must remain authenticated");
                if Arc::ptr_eq(&session, &next) {
                    break query;
                }
                assert!(Arc::ptr_eq(&session, &expected));
            }
            assert!(
                Instant::now() < deadline,
                "Reconnection must reissue the focused query"
            );
            cx.background_executor()
                .timer(Duration::from_millis(15))
                .await;
        };
        assert_eq!(query, "unfinished queryx");
        assert!(
            commands.is_empty(),
            "Reconnection must issue one refresh and no mutations"
        );
        window
            .update(cx, |view, window, cx| {
                assert!(Arc::ptr_eq(&view.account.as_ref().unwrap().session, &next));
                view.search.update(cx, |search, cx| {
                    search.reject_test_suggestion(expected, window, cx)
                });
            })
            .unwrap();
        wait_for_presented_frame(window, cx).await;
        assert!(
            commands.is_empty(),
            "A stale rejection must not trigger another renewal"
        );
        window
            .update(cx, |view, window, cx| {
                window.focus(&view.focus, cx);
                let later = super::account::session("autocomplete-background-renewal");
                view.search.update(cx, |search, cx| {
                    search.renew_session(Some(&next), later, window, cx);
                });
            })
            .unwrap();
        cx.background_executor()
            .timer(Duration::from_millis(300))
            .await;
        assert!(
            requests().is_none(),
            "A blurred query must not be sent again"
        );
    })
}
