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
fn settings_dropdown_arrows_start_at_the_selected_option() -> anyhow::Result<()> {
    use crate::app::{Music, SettingsSection};
    use crate::player::{AudioQuality, PlaybackSpeed, VideoQuality};

    type SelectedIndex = fn(&Music) -> usize;
    check_cache_window(async |window, cx| {
        let choices: [(_, _, _, _, SelectedIndex); 3] = [
            ("Audio quality", 5, 1, 2, |view| {
                AudioQuality::ALL
                    .iter()
                    .position(|choice| *choice == view.library.quality)
                    .unwrap()
            }),
            ("Video quality", 6, 4, 7, |view| {
                VideoQuality::ALL
                    .iter()
                    .position(|choice| *choice == view.library.video_quality)
                    .unwrap()
            }),
            ("Playback speed", 7, 3, 7, |view| {
                PlaybackSpeed::ALL
                    .iter()
                    .position(|choice| *choice == view.library.speed)
                    .unwrap()
            }),
        ];
        for (label, tabs, selected, last, current) in choices {
            window
                .update(cx, |view, window, cx| {
                    view.settings_section = SettingsSection::Playback;
                    view.open_overlay(Overlay::Settings, window, cx);
                })
                .unwrap();
            wait_for_presented_frame(window, cx).await;
            for _ in 0..tabs {
                key(window, cx, "tab").await;
            }
            let opener = window
                .update(cx, |view, window, cx| {
                    assert_eq!(current(view), selected, "{label} initial value");
                    window.focused(cx).unwrap()
                })
                .unwrap();

            for (keys, before, after) in [
                (&["down"][..], selected, selected + 1),
                (&["end", "down"][..], selected + 1, 0),
                (&["end", "home", "up"][..], 0, last),
                (&[][..], last, last),
            ] {
                key(window, cx, "enter").await;
                for name in keys {
                    key(window, cx, name).await;
                }
                window
                    .update(cx, |view, _, _| {
                        assert!(view.settings_menu.is_some(), "{label} menu is open");
                        assert_eq!(current(view), before, "{label} before Enter");
                    })
                    .unwrap();
                key(window, cx, "enter").await;
                window
                    .update(cx, |view, window, _| {
                        assert_eq!(current(view), after, "{label} after {keys:?} and Enter");
                        assert!(view.settings_menu.is_none(), "{label} menu closed");
                        assert!(opener.is_focused(window), "{label} focus returned");
                    })
                    .unwrap();
            }
        }
    })
}

mod focus_return {
    use super::super::navigation::{check_cache_window, reply};
    use super::super::render::{key, wait_for_presented_frame};
    use crate::app::{Music, Overlay, Route, Search, SettingsSection, sync::tests::page};
    use crate::catalog::{
        BrowseChoice, BrowseLink, Chapter, EpisodeContext, Item, Shelf, ShelfLayout,
    };
    use crate::search::{SearchFilter, SearchScope};
    use gpui::{
        AppContext, AsyncApp, Context, FocusHandle, Focusable, MouseButton, MouseDownEvent,
        MouseUpEvent, Pixels, PlatformInput, Point, Window, WindowHandle, point, px, size,
    };

    enum Input {
        Keys(&'static [&'static str]),
        Click(fn(&Music, &Window) -> Point<Pixels>),
        Call(fn(&mut Music, &mut Window, &mut Context<Music>)),
    }

    enum Back {
        Opener,
        /// The opener is gone, so focus falls back to the page.
        Page,
    }

    struct Row {
        name: &'static str,
        prepare: fn(&mut Music, &mut Window, &mut Context<Music>),
        tabs: usize,
        open: Input,
        opened: fn(&Music) -> bool,
        close: &'static [Input],
        back: Back,
    }

    /// Returns where a click put focus on mouse down, which makes the clicked control the opener.
    async fn send(
        input: &Input,
        window: WindowHandle<Music>,
        cx: &mut AsyncApp,
    ) -> Option<FocusHandle> {
        match input {
            Input::Keys(keys) => {
                for name in *keys {
                    key(window, cx, name).await;
                }
                None
            }
            Input::Call(call) => {
                window.update(cx, call).unwrap();
                wait_for_presented_frame(window, cx).await;
                None
            }
            Input::Click(at) => {
                let position = window
                    .update(cx, |view, window, _| at(view, window))
                    .unwrap();
                // The handle form leaves Music free for the handlers the event reaches.
                let focused = cx
                    .update_window(window.into(), |_, window, cx| {
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
                        window.focused(cx)
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
                focused
            }
        }
    }

    /// One episode with a chapter on an editable playlist, so its header offers every dialog.
    fn collection(view: &mut Music, window: &mut Window, cx: &mut Context<Music>) {
        let mut track = page(1, false).tracks()[0].clone();
        track.details.episode = Some(Box::new(EpisodeContext {
            chapters: vec![Chapter {
                title: "Opening chapter".into(),
                start_seconds: 0,
            }],
            ..Default::default()
        }));
        // The default compact guide keeps the header tab order short.
        view.sidebar_collapsed = false;
        view.sidebar_playlists.clear();
        view.route = Route::Browse(BrowseLink::new("VLdialog", "Dialog playlist"));
        view.page = page(0, false);
        view.page.title = "Dialog playlist".into();
        view.page.description = "Read the complete playlist description".into();
        view.page.editable = true;
        view.page.privacy = Some("PRIVATE".into());
        view.page.shelves = vec![Shelf {
            strapline: String::new(),
            title: String::new(),
            items: vec![Item::Track(track)],
            layout: ShelfLayout::Songs,
            more: None,
        }];
        view.page.header = crate::catalog::parse_page(
            &serde_json::json!({"header":{"musicResponsiveHeaderRenderer":{"title":{"simpleText":"Dialog playlist"}}},"contents":[]}),
            "Dialog playlist",
        )
        .unwrap()
        .header;
        view.rebuild_rows();
        window.resize(size(px(850.), px(560.)));
        window.focus(&view.focus, cx);
    }

    fn countries(view: &mut Music, window: &mut Window, cx: &mut Context<Music>) {
        view.sidebar_collapsed = false;
        view.sidebar_playlists.clear();
        view.route = Route::Browse(BrowseLink::new("FEmusic_charts", "Charts"));
        view.page = page(1, false);
        view.page.title = "Charts".into();
        view.rebuild_rows();
        window.resize(size(px(850.), px(560.)));
        window.focus(&view.focus, cx);
        view.page.countries = (0..4)
            .map(|index| BrowseChoice {
                label: format!("Country {index}"),
                selected: index == 0,
                target: BrowseLink::new(format!("country-{index}"), format!("Country {index}")),
            })
            .collect();
    }

    fn songs(view: &mut Music, window: &mut Window, cx: &mut Context<Music>) {
        window.resize(size(px(1280.), px(800.)));
        view.sidebar_collapsed = false;
        view.route = Route::Browse(BrowseLink::new("VLfocus", "Songs"));
        view.page = page(2, false);
        view.library
            .queue
            .replace(vec![view.page.tracks()[0].clone()], 0);
        view.library.autoplay = false;
        view.rebuild_rows();
        window.focus(&view.search.focus_handle(cx), cx);
    }

    fn sidebar_row(view: &mut Music, window: &mut Window, cx: &mut Context<Music>) {
        songs(view, window, cx);
        view.sidebar_playlists = vec![
            serde_json::from_value(serde_json::json!({
                "id": "VLkeyboard",
                "title": "Keyboard playlist",
                "subtitle": "Playlist"
            }))
            .unwrap(),
        ];
        window.focus(&view.sidebar_focus.handle(0, cx), cx);
    }

    fn featured(view: &mut Music, window: &mut Window, cx: &mut Context<Music>) {
        songs(view, window, cx);
        view.route = Route::Search(Search {
            query: "Song".into(),
            filter: SearchFilter::All,
            scope: SearchScope::Catalog,
            params: None,
        });
        view.page.shelves[0].layout = ShelfLayout::Featured;
        view.rebuild_rows();
        window.focus(&view.focus, cx);
    }

    fn featured_save(view: &Music, window: &Window) -> Point<Pixels> {
        let bounds = view.detail_list.state.bounds_for_item(1).unwrap();
        let width = view.page_layout(window).search_width();
        let inset = (bounds.size.width - px(width)) / 2.;
        let save_center = if width < 800. { 115. } else { 149. };
        point(
            bounds.left() + inset + px(326.),
            bounds.top() + px(save_center),
        )
    }

    fn arriving_page(view: &mut Music, window: &mut Window, cx: &mut Context<Music>) {
        songs(view, window, cx);
        view.navigate(Route::Browse(BrowseLink::new("VLarriving", "Arriving")), cx);
    }

    fn open_menu(view: &mut Music, window: &mut Window, cx: &mut Context<Music>) {
        view.open_item_menu(1, window, cx);
    }

    fn open_account(view: &mut Music, window: &mut Window, cx: &mut Context<Music>) {
        view.open_overlay(Overlay::Account, window, cx);
    }

    const ESCAPE: &[Input] = &[Input::Keys(&["escape"])];
    const BACKDROP: &[Input] = &[Input::Click(|_, _| point(px(8.), px(8.)))];

    /// Every kind of overlay, opened from the real control where one is reachable and closed
    /// by Escape, a click outside, an action or code, must hand focus back to its opener.
    #[test]
    #[ignore = "requires desktop, private D-Bus and isolated runtime/data; synthetic accounts only"]
    fn overlays_return_focus_to_their_opener() -> anyhow::Result<()> {
        let rows = [
            Row {
                name: "Help, Escape",
                prepare: sidebar_row,
                tabs: 0,
                open: Input::Keys(&["?"]),
                opened: |view| matches!(view.overlay, Some(Overlay::Help)),
                close: ESCAPE,
                back: Back::Opener,
            },
            Row {
                name: "Description, Escape",
                prepare: collection,
                tabs: 9,
                open: Input::Keys(&["enter"]),
                opened: |view| matches!(view.overlay, Some(Overlay::Description { .. })),
                close: ESCAPE,
                back: Back::Opener,
            },
            Row {
                name: "Chapters, click outside",
                prepare: collection,
                tabs: 13,
                open: Input::Keys(&["enter"]),
                opened: |view| matches!(view.overlay, Some(Overlay::Chapters(_))),
                close: BACKDROP,
                back: Back::Opener,
            },
            Row {
                name: "Playlist menu, Escape",
                prepare: collection,
                tabs: 12,
                open: Input::Keys(&["enter"]),
                opened: |view| matches!(view.overlay, Some(Overlay::Collection(_))),
                close: ESCAPE,
                back: Back::Opener,
            },
            Row {
                name: "Edit playlist form, Escape",
                prepare: collection,
                tabs: 14,
                open: Input::Keys(&["enter"]),
                opened: |view| matches!(view.overlay, Some(Overlay::PlaylistForm { .. })),
                close: ESCAPE,
                back: Back::Opener,
            },
            Row {
                name: "Delete playlist, Cancel",
                prepare: collection,
                tabs: 15,
                open: Input::Keys(&["enter"]),
                opened: |view| matches!(view.overlay, Some(Overlay::DeletePlaylist { .. })),
                close: &[Input::Keys(&["shift-tab", "enter"])],
                back: Back::Opener,
            },
            Row {
                name: "Country picker, Escape",
                prepare: countries,
                tabs: 8,
                open: Input::Keys(&["enter"]),
                opened: |view| matches!(view.overlay, Some(Overlay::CountryPicker(_))),
                close: ESCAPE,
                back: Back::Opener,
            },
            Row {
                name: "Song menu, Add to queue",
                prepare: songs,
                tabs: 0,
                open: Input::Call(open_menu),
                opened: |view| matches!(view.overlay, Some(Overlay::Track { .. })),
                close: &[Input::Keys(&["home", "down", "down", "enter"])],
                back: Back::Opener,
            },
            Row {
                name: "Song menu, Chapters, Escape",
                prepare: collection,
                tabs: 0,
                open: Input::Call(|view, window, cx| view.open_item_menu(0, window, cx)),
                opened: |view| matches!(view.overlay, Some(Overlay::Track { .. })),
                close: &[Input::Keys(&["home", "down", "enter", "escape"])],
                back: Back::Opener,
            },
            Row {
                name: "Song menu, closed by a page arriving",
                prepare: arriving_page,
                tabs: 1,
                open: Input::Call(|view, window, cx| {
                    view.open_track_menu(page(1, false).tracks()[0].clone(), None, window, cx)
                }),
                opened: |view| matches!(view.overlay, Some(Overlay::Track { .. })),
                close: &[Input::Call(|view, _, cx| {
                    reply(view, page(2, false), cx);
                })],
                back: Back::Opener,
            },
            Row {
                name: "Account menu, click outside",
                prepare: songs,
                tabs: 0,
                open: Input::Call(open_account),
                opened: |view| matches!(view.overlay, Some(Overlay::Account)),
                close: BACKDROP,
                back: Back::Opener,
            },
            Row {
                name: "Account menu, Settings, Escape",
                prepare: songs,
                tabs: 0,
                open: Input::Call(open_account),
                opened: |view| matches!(view.overlay, Some(Overlay::Account)),
                close: &[Input::Keys(&[
                    "down", "down", "down", "down", "enter", "escape",
                ])],
                back: Back::Opener,
            },
            Row {
                name: "Setting choices, Escape",
                prepare: |view, window, cx| {
                    songs(view, window, cx);
                    view.settings_section = SettingsSection::Playback;
                    view.open_overlay(Overlay::Settings, window, cx);
                },
                tabs: 5,
                open: Input::Keys(&["enter"]),
                opened: |view| view.settings_menu.is_some(),
                close: ESCAPE,
                back: Back::Opener,
            },
            Row {
                name: "Save to playlist, New playlist, Escape",
                prepare: songs,
                tabs: 0,
                open: Input::Call(|view, window, cx| {
                    view.playlist_picker(vec![view.page.tracks()[0].clone()], window, cx)
                }),
                opened: |view| matches!(view.overlay, Some(Overlay::PlaylistPicker { .. })),
                close: &[Input::Keys(&["shift-tab", "enter", "escape"])],
                back: Back::Opener,
            },
            Row {
                name: "Featured Save click, Escape",
                prepare: featured,
                tabs: 0,
                open: Input::Click(featured_save),
                opened: |view| matches!(view.overlay, Some(Overlay::PlaylistPicker { .. })),
                close: ESCAPE,
                back: Back::Opener,
            },
            Row {
                name: "Sidebar New playlist click, Escape",
                prepare: |view, window, cx| {
                    songs(view, window, cx);
                    window.focus(&view.focus, cx);
                },
                tabs: 0,
                // The wide guide's New playlist button follows its three navigation rows.
                open: Input::Click(|_, _| point(px(120.), px(283.))),
                opened: |view| matches!(view.overlay, Some(Overlay::PlaylistForm { .. })),
                close: ESCAPE,
                back: Back::Opener,
            },
            Row {
                name: "Help after its opener is removed, Escape",
                prepare: sidebar_row,
                tabs: 0,
                open: Input::Keys(&["?"]),
                opened: |view| matches!(view.overlay, Some(Overlay::Help)),
                close: &[
                    Input::Call(|view, _, cx| {
                        view.sidebar_playlists.clear();
                        cx.notify();
                    }),
                    Input::Keys(&["escape"]),
                ],
                back: Back::Page,
            },
        ];
        check_cache_window(async move |window, cx| {
            let mut failures = Vec::new();
            for row in &rows {
                window
                    .update(cx, |view, window, cx| {
                        view.close_overlay(window, cx);
                        (row.prepare)(view, window, cx);
                        cx.notify();
                    })
                    .unwrap();
                wait_for_presented_frame(window, cx).await;
                for _ in 0..row.tabs {
                    key(window, cx, "tab").await;
                }
                let focused = window
                    .update(cx, |_, window, cx| window.focused(cx))
                    .unwrap();
                let opener = send(&row.open, window, cx).await.or(focused);
                let opened = window
                    .update(cx, |view, _, _| {
                        (row.opened)(view)
                            .then_some(())
                            .ok_or_else(|| view.overlay.as_ref().map(std::mem::discriminant))
                    })
                    .unwrap();
                if let Err(overlay) = opened {
                    failures.push(format!("{}: did not open; open: {overlay:?}", row.name));
                    continue;
                }
                for input in row.close {
                    send(input, window, cx).await;
                }
                let returned = window
                    .update(cx, |view, window, _| {
                        assert!(!(row.opened)(view), "{}: did not close", row.name);
                        match row.back {
                            Back::Opener => opener.as_ref().is_some_and(|o| o.is_focused(window)),
                            Back::Page => view.focus.is_focused(window),
                        }
                    })
                    .unwrap();
                println!("FOCUS_RETURN {}: {returned}", row.name);
                if !returned {
                    failures.push(format!("{}: focus did not return", row.name));
                }
            }
            assert!(failures.is_empty(), "{failures:#?}");
        })
    }
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
                music.sidebar_collapsed = false;
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

        // Tab onto the second country row.
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
        for (episode, first, last) in [(false, 1, 13), (true, 2, 15)] {
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
#[ignore = "requires desktop, private D-Bus and isolated runtime/data; synthetic tracks only"]
fn queue_menu_actions_show_the_first_song_without_starting_playback() -> anyhow::Result<()> {
    use crate::app::Playback;

    check_cache_window(async |window, cx| {
        let tracks = window
            .update(cx, |view, _, _| {
                let mut tracks = view.page.tracks()[..2].to_vec();
                tracks[0].duration = Some(120);
                tracks
            })
            .unwrap();
        let mut failed = Vec::new();
        for (first_action, second_action) in [(2, 1), (1, 2)] {
            window
                .update(cx, |view, window, cx| {
                    view.clear_queue(cx);
                    view.close_player_page();
                    window.focus(&view.focus, cx);
                })
                .unwrap();
            for (index, (track, steps)) in
                tracks.iter().zip([first_action, second_action]).enumerate()
            {
                window
                    .update(cx, |view, window, cx| {
                        view.open_track_menu(track.clone(), None, window, cx);
                    })
                    .unwrap();
                wait_for_presented_frame(window, cx).await;
                key(window, cx, "home").await;
                for _ in 0..steps {
                    key(window, cx, "down").await;
                }
                key(window, cx, "enter").await;
                let confirmation = if steps == 1 {
                    "Will play next"
                } else {
                    "Added to queue"
                };
                window
                    .update(cx, |view, _, _| {
                        assert_eq!(
                            view.toast.as_ref().map(|toast| toast.text.as_ref()),
                            Some(confirmation)
                        );
                        assert_eq!(view.player_open, index == 1);
                    })
                    .unwrap();
                if index == 0 {
                    key(window, cx, "q").await;
                }
            }
            window
                .update(cx, |view, _, _| {
                    let queue = &view.library.queue;
                    assert_eq!(queue.tracks.len(), 2);
                    if queue.current().map(|track| track.id.as_str()) != Some(&tracks[0].id)
                        || queue.tracks[1].id != tracks[1].id
                        || view.transport().duration != 120.
                        || !view.player_open
                    {
                        failed.push(first_action);
                    }
                    assert_eq!(view.playback, Playback::Stopped);
                    assert_eq!(view.position, 0.);
                })
                .unwrap();
        }
        assert!(
            failed.is_empty(),
            "Both queue actions must select the first song and make Up next reachable. Failed: {failed:?}"
        );

        let mut library = crate::library::Library::new();
        library.queue.tracks_mut().extend(tracks);
        let path = crate::dirs::data().unwrap().join("library.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, serde_json::to_vec(&library).unwrap()).unwrap();
        let (_, restored) = crate::library::Store::load().unwrap();
        assert_eq!(
            restored.queue.current().map(|track| track.id.as_str()),
            library.queue.tracks.first().map(|track| track.id.as_str()),
            "Older queues saved without a selection must open on their first song"
        );
    })
}
