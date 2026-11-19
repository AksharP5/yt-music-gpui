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
fn collection_dialogs_return_keyboard_focus_to_their_opening_control() -> anyhow::Result<()> {
    check_cache_window(async |window, cx| {
        window
            .update(cx, |music, window, cx| {
                let mut track = music.page.tracks()[0].clone();
                track.details.episode = Some(Box::new(crate::catalog::EpisodeContext {
                    chapters: vec![crate::catalog::Chapter {
                        title: "Opening chapter".into(),
                        start_seconds: 0,
                    }],
                    ..Default::default()
                }));
                music.route = Route::Browse(BrowseLink::new("VLdialog", "Dialog playlist"));
                music.page.title = "Dialog playlist".into();
                music.page.description = "Read the complete playlist description".into();
                music.page.continuation = None;
                music.page.shelves = vec![crate::catalog::Shelf {
                    title: String::new(),
                    items: vec![crate::catalog::Item::Track(track)],
                    layout: crate::catalog::ShelfLayout::Songs,
                    more: None,
                }];
                music.page.header = crate::catalog::parse_page(
                    &serde_json::json!({"header":{"musicResponsiveHeaderRenderer":{"title":{"simpleText":"Dialog playlist"}}},"contents":[]}),
                    "Dialog playlist",
                ).unwrap().header;
                music.rebuild_rows();
                window.resize(gpui::size(gpui::px(850.), gpui::px(560.)));
                window.focus(&music.focus, cx);
                cx.notify();
            })
            .unwrap();
        wait_for_presented_frame(window, cx).await;
        // Header, compact guide and Back precede the description and collection actions.
        let mut lost_focus = Vec::new();
        for (tabs, chapters) in [(9, false), (13, true)] {
            window
                .update(cx, |music, window, cx| window.focus(&music.focus, cx))
                .unwrap();
            for _ in 0..tabs {
                key(window, cx, "tab").await;
            }
            let opening = window
                .update(cx, |_, window, cx| {
                    super::render::capture(
                        window,
                        if chapters {
                            "chapters-focused"
                        } else {
                            "description-focused"
                        },
                    );
                    window.focused(cx).unwrap()
                })
                .unwrap();
            key(window, cx, "enter").await;
            window
                .update(cx, |music, _, _| {
                    assert!(
                        if chapters {
                            matches!(music.overlay, Some(Overlay::Chapters(_)))
                        } else {
                            matches!(music.overlay, Some(Overlay::Description { .. }))
                        },
                        "Enter must open the focused collection dialog"
                    );
                })
                .unwrap();
            key(window, cx, "escape").await;
            let restored = window
                .update(cx, |music, window, _| {
                    assert!(music.overlay.is_none());
                    super::render::capture(
                        window,
                        if chapters {
                            "chapters-returned"
                        } else {
                            "description-returned"
                        },
                    );
                    opening.is_focused(window)
                })
                .unwrap();
            if !restored {
                lost_focus.push(if chapters { "Chapters" } else { "Description" });
            }
        }
        assert!(
            lost_focus.is_empty(),
            "Closing these dialogs lost their opening control: {lost_focus:?}"
        );
    })
}

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

#[test]
#[ignore = "requires desktop, private D-Bus and isolated runtime/data; synthetic accounts only"]
fn compact_playlist_menu_focus_reveals_the_last_action() -> anyhow::Result<()> {
    use crate::{
        app::Music,
        catalog::{Chapter, EpisodeContext, Item},
    };
    use gpui::{px, size};

    check_cache_window(async |window, cx| {
        let visible = |view: &Music, child| {
            let scroll = &view.menu_scroll;
            let row = scroll.bounds_for_item(child).unwrap();
            let offset = scroll.offset().y;
            row.top() + offset >= scroll.bounds().top()
                && row.bottom() + offset <= scroll.bounds().bottom()
        };

        // The podcast menu has a subtitle above its actions.
        for (episode, first, last) in [(false, 1, 12), (true, 2, 14)] {
            window
                .update(cx, |view, window, cx| {
                    window.resize(size(px(850.), px(560.)));
                    view.route = Route::Browse(BrowseLink::new("VLcompact", "Editable playlist"));
                    view.page = crate::app::sync::tests::response(
                        &[("Songs", vec![0, 1, 2, 3])],
                        None,
                        true,
                    );
                    view.page.editable = true;
                    let Item::Track(song) = &mut view.page.shelves[0].items[1] else {
                        panic!("Expected a song");
                    };
                    song.details.add_library = Some("test-save".into());
                    song.details.album_id = Some("MPREcompact".into());
                    song.details.artist_id = Some("UCcompact".into());
                    if episode {
                        song.details.episode = Some(Box::new(EpisodeContext {
                            show_id: Some("MPSPcompact".into()),
                            subtitle: "Synthetic podcast episode".into(),
                            chapters: vec![Chapter {
                                title: "Opening chapter".into(),
                                start_seconds: 0,
                            }],
                            ..Default::default()
                        }));
                    }

                    view.rebuild_rows();
                    cx.notify();
                })
                .unwrap();
            wait_for_presented_frame(window, cx).await;
            cx.update_window(window.into(), |_, window, cx| {
                window.dispatch_event(
                    PlatformInput::MouseMove(MouseMoveEvent {
                        position: point(px(365.), px(250.)),
                        ..Default::default()
                    }),
                    cx,
                );
            })
            .unwrap();
            window
                .update(cx, |view, window, cx| view.open_item_menu(1, window, cx))
                .unwrap();
            wait_for_presented_frame(window, cx).await;

            window
                .update(cx, |view, window, _| {
                    assert_eq!(window.viewport_size(), size(px(850.), px(560.)));
                    assert!(visible(view, first));
                    assert!(!visible(view, last));
                    assert!(view.menu_scroll.bounds_for_item(last + 1).is_none());
                })
                .unwrap();
            key(window, cx, "end").await;
            window
                .update(cx, |view, _, _| {
                    assert!(visible(view, last), "End must reveal Share");
                })
                .unwrap();
            key(window, cx, "home").await;
            window
                .update(cx, |view, _, _| {
                    assert!(visible(view, first), "Home must reveal the first action");
                })
                .unwrap();
            key(window, cx, "shift-tab").await;
            window
                .update(cx, |view, _, _| {
                    assert!(
                        visible(view, last),
                        "Shift+Tab must reveal the wrapped focus"
                    );
                })
                .unwrap();
            key(window, cx, "escape").await;
            window
                .update(cx, |view, window, cx| view.open_item_menu(1, window, cx))
                .unwrap();
            wait_for_presented_frame(window, cx).await;
            window
                .update(cx, |view, _, _| {
                    assert_eq!(view.menu_scroll.offset().y, px(0.));
                    assert!(visible(view, first));
                })
                .unwrap();
            key(window, cx, "escape").await;
        }
    })
}

#[test]
#[ignore = "requires desktop, private D-Bus and isolated runtime/data; synthetic account only"]
fn queue_menu_actions_restore_the_opening_focus() -> anyhow::Result<()> {
    use gpui::Focusable;

    use crate::library::Repeat;

    check_cache_window(async |window, cx| {
        for downs in [1, 2] {
            let opening_focus = window
                .update(cx, |view, window, cx| {
                    view.route = Route::Browse(BrowseLink::new("VLfocus", "Songs"));
                    view.page = crate::app::sync::tests::page(2, false);
                    view.library
                        .queue
                        .replace(vec![view.page.tracks()[0].clone()], 0);
                    view.library.autoplay = false;
                    view.library.repeat = Repeat::Off;
                    view.rebuild_rows();
                    let opening_focus = if downs == 1 {
                        view.focus.clone()
                    } else {
                        view.search.focus_handle(cx)
                    };
                    window.focus(&opening_focus, cx);
                    view.open_item_menu(1, window, cx);
                    opening_focus
                })
                .unwrap();
            wait_for_presented_frame(window, cx).await;
            key(window, cx, "home").await;
            for _ in 0..downs {
                key(window, cx, "down").await;
            }
            key(window, cx, "enter").await;
            window
                .update(cx, |view, window, _| {
                    assert!(view.overlay.is_none());
                    assert_eq!(view.library.queue.tracks.len(), 2);
                    assert_eq!(view.library.queue.tracks[1].id, view.page.tracks()[1].id);
                    assert!(
                        opening_focus.is_focused(window),
                        "The opener must regain focus after confirming queue action {downs}"
                    );
                })
                .unwrap();
            if downs == 1 {
                key(window, cx, "r").await;
                window
                    .update(cx, |view, _, _| {
                        assert_eq!(view.library.repeat, Repeat::All)
                    })
                    .unwrap();
            }
        }
    })
}

#[test]
#[ignore = "requires desktop, private D-Bus and isolated runtime/data; synthetic account only"]
fn featured_save_picker_restores_focus_to_the_opening_control() -> anyhow::Result<()> {
    use crate::app::Search;
    use crate::catalog::ShelfLayout;
    use crate::search::{SearchFilter, SearchScope};
    use gpui::{px, size};

    check_cache_window(async |window, cx| {
        window
            .update(cx, |view, window, cx| {
                window.resize(size(px(1280.), px(800.)));
                view.sidebar_collapsed = Some(false);
                view.route = Route::Search(Search {
                    query: "Song".into(),
                    filter: SearchFilter::All,
                    scope: SearchScope::Catalog,
                });
                view.page = crate::app::sync::tests::page(2, false);
                view.page.shelves[0].layout = ShelfLayout::Featured;
                view.rebuild_rows();
                window.focus(&view.focus, cx);
                cx.notify();
            })
            .unwrap();
        wait_for_presented_frame(window, cx).await;
        let position = window
            .update(cx, |view, window, _| {
                let bounds = view.detail_list.state.bounds_for_item(1).unwrap();
                let width = view.page_layout(window).search_width();
                let inset = (bounds.size.width - px(width)) / 2.;
                let save_center = if width < 800. { 115. } else { 149. };
                point(
                    bounds.left() + inset + px(326.),
                    bounds.top() + px(save_center),
                )
            })
            .unwrap();
        let click = |event, cx: &mut gpui::AsyncApp| {
            cx.update_window(window.into(), |_, window, cx| {
                window.dispatch_event(event, cx)
            })
            .unwrap();
        };
        click(
            PlatformInput::MouseDown(MouseDownEvent {
                button: MouseButton::Left,
                position,
                modifiers: Default::default(),
                click_count: 1,
                first_mouse: false,
            }),
            cx,
        );
        let opening = window
            .update(cx, |view, window, cx| {
                assert!(
                    !view.focus.is_focused(window),
                    "Save must take focus on mouse down"
                );
                window.focused(cx).unwrap()
            })
            .unwrap();
        click(
            PlatformInput::MouseUp(MouseUpEvent {
                button: MouseButton::Left,
                position,
                modifiers: Default::default(),
                click_count: 1,
            }),
            cx,
        );
        wait_for_presented_frame(window, cx).await;
        let dialog_has_focus = window
            .update(cx, |view, window, cx| {
                assert!(
                    matches!(&view.overlay, Some(Overlay::PlaylistPicker { tracks, .. })
                    if tracks.len() == 1 && tracks[0].id == view.page.tracks()[0].id)
                );
                view.overlay_focus.contains_focused(window, cx)
            })
            .unwrap();
        key(window, cx, "escape").await;
        window
            .update(cx, |view, window, _| {
                assert!(view.overlay.is_none());
                assert!(
                    dialog_has_focus && opening.is_focused(window),
                    "Save must move focus into the picker and restore it after Escape"
                );
            })
            .unwrap();
    })
}

#[test]
#[ignore = "requires desktop, private D-Bus and isolated runtime/data; synthetic account only"]
fn cancelling_playlist_creation_restores_the_sidebar_button() -> anyhow::Result<()> {
    use gpui::{px, size};

    check_cache_window(async |window, cx| {
        window
            .update(cx, |view, window, cx| {
                window.resize(size(px(1280.), px(800.)));
                view.sidebar_collapsed = Some(false);
                window.focus(&view.focus, cx);
                cx.notify();
            })
            .unwrap();
        wait_for_presented_frame(window, cx).await;

        // The wide guide's New playlist button follows its three navigation rows.
        let position = point(px(120.), px(283.));
        cx.update_window(window.into(), |_, window, cx| {
            window.dispatch_event(
                PlatformInput::MouseDown(MouseDownEvent {
                    button: MouseButton::Left,
                    position,
                    modifiers: Default::default(),
                    click_count: 1,
                    first_mouse: false,
                }),
                cx,
            );
        })
        .unwrap();
        let opening = window
            .update(cx, |view, window, cx| {
                assert!(
                    !view.focus.is_focused(window),
                    "New playlist must take focus"
                );
                window.focused(cx).unwrap()
            })
            .unwrap();
        cx.update_window(window.into(), |_, window, cx| {
            window.dispatch_event(
                PlatformInput::MouseUp(MouseUpEvent {
                    button: MouseButton::Left,
                    position,
                    modifiers: Default::default(),
                    click_count: 1,
                }),
                cx,
            );
        })
        .unwrap();
        wait_for_presented_frame(window, cx).await;
        window
            .update(cx, |view, _, _| {
                assert!(matches!(view.overlay, Some(Overlay::PlaylistForm { .. })));
            })
            .unwrap();
        key(window, cx, "escape").await;
        window
            .update(cx, |view, window, _| {
                assert!(view.overlay.is_none());
                assert!(
                    opening.is_focused(window),
                    "New playlist must regain focus after Escape"
                );
            })
            .unwrap();
        key(window, cx, "enter").await;
        window
            .update(cx, |view, _, _| {
                assert!(matches!(view.overlay, Some(Overlay::PlaylistForm { .. })));
            })
            .unwrap();
    })
}
