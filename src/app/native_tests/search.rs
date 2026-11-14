//! Search box input with the real window event path.
use super::navigation::check_cache_window;
use super::render::wait_for_presented_frame;
use crate::app::{Music, Route};
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
