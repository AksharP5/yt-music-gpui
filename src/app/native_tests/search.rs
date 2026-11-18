//! Search box input with the real window event path.
use super::navigation::check_cache_window;
use super::render::wait_for_presented_frame;
use crate::app::{Music, Route};
use crate::library::Repeat;
use gpui::{AppContext, AsyncApp, Focusable, PlatformInput, WindowHandle, point, px};

async fn press(window: WindowHandle<Music>, cx: &mut AsyncApp, key: &str) {
    cx.update_window(window.into(), |_, window, cx| {
        window.dispatch_keystroke(gpui::Keystroke::parse(key).unwrap(), cx);
    })
    .unwrap();
    wait_for_presented_frame(window, cx).await;
}

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
        press(window, cx, "r").await;
        assert_eq!(repeat(cx).unwrap(), Repeat::All);
        press(window, cx, "r").await;
        assert_eq!(repeat(cx).unwrap(), Repeat::One);

        window
            .update(cx, |music, window, cx| {
                window.focus(&music.search.focus_handle(cx), cx)
            })
            .unwrap();
        press(window, cx, "r").await;
        press(window, cx, "enter").await;
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
        press(window, cx, "enter").await;
        assert_eq!(
            searched(window, cx),
            "typed query",
            "A suggestion under the pointer must not replace the typed text"
        );

        type_with_suggestions(window, cx).await;
        press(window, cx, "down").await;
        press(window, cx, "down").await;
        press(window, cx, "enter").await;
        assert_eq!(searched(window, cx), "second suggestion");
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
        press(window, cx, "y").await;
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
        press(window, cx, "x").await;
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
