//! Keyboard input must act on the visible dialog rather than a background control.

use super::navigation::check_cache_window;
use super::render::{key, wait_for_presented_frame};
use crate::app::{Overlay, Route};
use crate::catalog::{BrowseChoice, BrowseLink};

#[test]
#[ignore = "requires desktop, private D-Bus and isolated runtime/data; synthetic accounts only"]
fn help_blocks_background_keyboard_clicks_and_restores_focus() -> anyhow::Result<()> {
    check_cache_window(async |window, cx| {
        let return_focus = window
            .update(cx, |music, _, cx| {
                music.sidebar_playlists = vec![
                    serde_json::from_value(serde_json::json!({
                        "id": "VLkeyboard",
                        "title": "Keyboard playlist",
                        "subtitle": "Playlist"
                    }))
                    .unwrap(),
                ];
                music.sidebar_collapsed = Some(false);
                cx.notify();
                music.sidebar_focus.handle(0, cx)
            })
            .unwrap();
        wait_for_presented_frame(window, cx).await;
        window
            .update(cx, |_, window, cx| window.focus(&return_focus, cx))
            .unwrap();
        wait_for_presented_frame(window, cx).await;

        key(window, cx, "?").await;
        for activation in ["enter", "space"] {
            key(window, cx, activation).await;
            window
                .update(cx, |music, _, _| {
                    assert!(matches!(music.overlay, Some(Overlay::Help)));
                    assert!(
                        music.route == Route::Explore,
                        "Keyboard activation inside Help must not open the background playlist"
                    );
                })
                .unwrap();
        }

        key(window, cx, "escape").await;
        window
            .update(cx, |music, window, _| {
                assert!(music.overlay.is_none());
                assert!(return_focus.is_focused(window));
            })
            .unwrap();
        key(window, cx, "enter").await;
        window
            .update(cx, |music, _, _| {
                assert!(matches!(&music.route, Route::Browse(link) if link.id == "VLkeyboard"));
            })
            .unwrap();
    })
}

#[test]
#[ignore = "requires desktop, private D-Bus and isolated runtime/data; synthetic accounts only"]
fn country_arrows_choose_the_highlighted_country_after_tabbing_into_rows() -> anyhow::Result<()> {
    check_cache_window(async |window, cx| {
        window
            .update(cx, |music, window, cx| {
                music.page.countries = (0..4)
                    .map(|index| BrowseChoice {
                        label: format!("Country {index}"),
                        selected: index == 0,
                        target: BrowseLink::new(
                            format!("keyboard-country-{index}"),
                            format!("Country {index}"),
                        ),
                    })
                    .collect();
                music.country_picker(window, cx);
            })
            .unwrap();
        wait_for_presented_frame(window, cx).await;

        // Move through the input and onto the second country row.
        key(window, cx, "tab").await;
        key(window, cx, "tab").await;
        key(window, cx, "tab").await;
        window
            .update(cx, |_, window, _| {
                super::render::capture(window, "country-before-arrow");
            })
            .unwrap();
        key(window, cx, "down").await;
        window
            .update(cx, |_, window, _| {
                super::render::capture(window, "country-after-arrow");
            })
            .unwrap();
        key(window, cx, "enter").await;
        window
            .update(cx, |music, _, _| {
                assert!(music.overlay.is_none());
                assert_eq!(
                    match &music.route {
                        Route::Browse(link) => Some(link.id.as_str()),
                        _ => None,
                    },
                    Some("keyboard-country-2"),
                    "Down from the second country must make Enter choose the third country"
                );
            })
            .unwrap();
    })
}
