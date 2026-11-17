//! Keyboard input must act on the visible dialog rather than a background control.

use super::navigation::check_cache_window;
use super::render::{key, wait_for_presented_frame};
use crate::app::{Overlay, Route};
use crate::catalog::{BrowseChoice, BrowseLink};
use crate::media_controls;
use gpui::{
    AppContext, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PlatformInput, point,
};

#[test]
#[ignore = "requires desktop, private D-Bus and isolated runtime/data; synthetic accounts only"]
fn mute_preserves_the_chosen_volume_and_slider_changes_unmute() -> anyhow::Result<()> {
    check_cache_window(async |window, cx| {
        window
            .update(cx, |music, window, cx| {
                let track = music.page.tracks()[0].clone();
                music.library.queue.replace(vec![track], 0);
                assert!(music.set_volume(25., cx));
                window.focus(&music.focus, cx);
            })
            .unwrap();
        wait_for_presented_frame(window, cx).await;
        let slider = window
            .update(cx, |music, _, _| {
                let bounds = music.volume_bounds.get();
                assert!(bounds.size.width > gpui::px(0.));
                point(bounds.left() + bounds.size.width * 0.25, bounds.center().y)
            })
            .unwrap();
        cx.update_window(window.into(), |_, window, cx| {
            window.dispatch_event(
                PlatformInput::MouseMove(MouseMoveEvent {
                    position: slider,
                    ..Default::default()
                }),
                cx,
            );
        })
        .unwrap();
        wait_for_presented_frame(window, cx).await;
        window
            .update(cx, |_, window, _| {
                super::render::capture(window, "volume-before-mute");
            })
            .unwrap();

        key(window, cx, "m").await;
        window
            .update(cx, |music, window, _| {
                assert_eq!(music.library.volume, 25.);
                assert!(music.library.muted);
                assert_eq!(music.library.effective_volume(), 0.);
                super::render::capture(window, "volume-muted");
            })
            .unwrap();
        key(window, cx, "m").await;
        window
            .update(cx, |music, _, _| {
                assert_eq!(music.library.volume, 25.);
                assert!(!music.library.muted);
                assert_eq!(music.library.effective_volume(), 25.);
            })
            .unwrap();

        key(window, cx, "m").await;
        assert!(
            window
                .update(cx, |music, _, _| music.library.muted)
                .unwrap()
        );
        cx.update_window(window.into(), |_, window, cx| {
            window.dispatch_event(
                PlatformInput::MouseMove(MouseMoveEvent {
                    position: slider,
                    ..Default::default()
                }),
                cx,
            );
        })
        .unwrap();
        wait_for_presented_frame(window, cx).await;
        cx.update_window(window.into(), |_, window, cx| {
            window.dispatch_event(
                PlatformInput::MouseDown(MouseDownEvent {
                    button: MouseButton::Left,
                    position: slider,
                    ..Default::default()
                }),
                cx,
            );
            window.dispatch_event(
                PlatformInput::MouseUp(MouseUpEvent {
                    button: MouseButton::Left,
                    position: slider,
                    ..Default::default()
                }),
                cx,
            );
        })
        .unwrap();
        wait_for_presented_frame(window, cx).await;
        window
            .update(cx, |music, window, cx| {
                assert_eq!(music.library.volume, 25.);
                assert!(
                    !music.library.muted,
                    "Clicking the same slider value must unmute"
                );
                music.toggle_mute(cx);
                music.desktop_event(media_controls::Event::Volume(25.), cx);
                assert!(!music.library.muted, "Desktop volume must also unmute");
                assert_eq!(music.library.effective_volume(), 25.);
                super::render::capture(window, "volume-unmuted");
                assert!(music.set_volume(0., cx));
                music.toggle_mute(cx);
                assert!(!music.library.muted);
                assert_eq!(music.library.effective_volume(), 60.);
            })
            .unwrap();
    })
}

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
