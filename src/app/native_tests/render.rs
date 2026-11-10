use crate::app::{CatalogTarget, Music, Playback, Startup, player_page::PlayerTab};
use crate::player::PlayerEvent;
use anyhow::{Result, ensure};
use gpui::{
    App, AppContext, AsyncApp, Bounds, Focusable, TitlebarOptions, WindowBounds, WindowHandle,
    WindowKind, WindowOptions, point, px, size,
};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::sync::atomic::AtomicBool;
use std::{
    cell::Cell,
    rc::Rc,
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant},
};

pub(super) async fn wait_for_presented_frame(window: WindowHandle<Music>, cx: &mut AsyncApp) {
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

fn render_counts(music: &Music, cx: &App) -> (usize, usize, usize) {
    (
        music.content_view.read(cx).renders,
        music.player_bar.read(cx).renders,
        music.renders,
    )
}

async fn wait_for_quiescence(
    window: WindowHandle<Music>,
    cx: &mut AsyncApp,
) -> (usize, usize, usize) {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut previous = None;
    loop {
        wait_for_presented_frame(window, cx).await;
        let (counts, idle) = window
            .update(cx, |music, _, cx| {
                let idle = music.artwork.is_idle()
                    && CatalogTarget::ALL
                        .into_iter()
                        .all(|target| !music.catalog_requests.is_loading(target))
                    && music.watch_pending.is_none()
                    && !music.radio_loading
                    && !music.queue_source.can_continue();
                (render_counts(music, cx), idle)
            })
            .unwrap();
        if idle && previous == Some(counts) {
            return counts;
        }
        assert!(
            Instant::now() < deadline,
            "Render measurement did not settle: counts={counts:?}, workers_idle={idle}"
        );
        previous = idle.then_some(counts);
    }
}

async fn key(window: WindowHandle<Music>, cx: &mut AsyncApp, key: &str) {
    cx.update_window(window.into(), |_, window, cx| {
        let keystroke = gpui::Keystroke::parse(key).unwrap();
        window.dispatch_keystroke(keystroke.clone(), cx);
        window.dispatch_event(
            gpui::PlatformInput::KeyUp(gpui::KeyUpEvent { keystroke }),
            cx,
        );
    })
    .unwrap();
    wait_for_presented_frame(window, cx).await;
}

fn assert_row_visible(scroll: &gpui::UniformListScrollHandle, row: usize, count: usize) {
    let state = scroll.0.borrow();
    let size = state.last_item_size.unwrap();
    let height = size.contents.height / count as f32;
    let top = -state.base_handle.offset().y;
    assert!(
        height * row as f32 >= top - px(1.),
        "Focused row {row} is clipped above"
    );
    assert!(
        height * (row + 1) as f32 <= top + size.item.height + px(1.),
        "Focused row {row} is clipped below"
    );
}

async fn keyboard_survives_replacement_and_scrolls_lists(
    window: WindowHandle<Music>,
    cx: &mut AsyncApp,
) {
    use crate::app::{CatalogResult, Route, sync::tests::page};
    use crate::catalog::ShelfLayout;
    window
        .update(cx, |music, window, cx| {
            music.player_open = false;
            music.route = Route::Home;
            music.page = page(1, false);
            music.page.shelves[0].layout = ShelfLayout::Cards;
            music.page.shelves[0].title = "Removed Home shelf".into();
            music.rebuild_rows();
            music
                .detail_list
                .state
                .scroll_to(gpui::ListOffset::default());
            music.shelf_scroll.clear();
            music.error = None;
            music.notice = None;
            music.player = Some(
                crate::player::Player::new(crate::player::PlayerOptions {
                    volume: 0.,
                    audio_output: Some("null".into()),
                    ..Default::default()
                })
                .unwrap(),
            );
            window.focus(&music.focus, cx);
            cx.notify();
        })
        .unwrap();
    wait_for_presented_frame(window, cx).await;
    // At this narrow width: four header tab stops, three primary navigation
    // controls, then the card. Both carousel arrows are disabled for one card.
    for _ in 0..8 {
        key(window, cx, "tab").await;
    }
    window
        .update(cx, |music, window, _| {
            assert!(!music.focus.is_focused(window));
            capture(window, "focused-home-card");
        })
        .unwrap();
    key(window, cx, "ctrl-r").await;
    window
        .update(cx, |music, _, cx| {
            let job = std::iter::from_fn(|| music.catalog_requests.pending.try_recv().ok())
                .find(|job| job.query.target() == CatalogTarget::Page)
                .unwrap();
            music.catalog_result(
                CatalogResult {
                    id: job.id,
                    account_revision: job.account_revision,
                    append: false,
                    revalidate: true,
                    result: Ok(page(0, false)),
                    session_rejected: false,
                    target: CatalogTarget::Page,
                },
                cx,
            );
        })
        .unwrap();
    wait_for_presented_frame(window, cx).await;
    window
        .update(cx, |music, window, _| {
            assert!(
                music.focus.is_focused(window),
                "Removing a focused card must restore root shortcuts"
            );
        })
        .unwrap();
    key(window, cx, "space").await;
    window
        .update(cx, |music, _, _| {
            assert!(
                music.playback.is_paused(),
                "Space must pause after the focused card disappears"
            )
        })
        .unwrap();
    key(window, cx, "tab").await;
    window
        .update(cx, |music, window, _| {
            assert!(
                !music.focus.is_focused(window),
                "Tab must still traverse after replacement"
            )
        })
        .unwrap();
    key(window, cx, "/").await;
    window
        .update(cx, |music, window, cx| {
            assert!(
                music.search.focus_handle(cx).is_focused(window),
                "Search must survive replacement"
            );
            window.focus(&music.focus, cx);
            window.resize(size(px(1280.), px(700.)));
            music.library.queue.replace(page(30, false).tracks(), 0);
            music.autoplay_tracks = page(3, false).tracks().into();
            music.player_open = true;
            music.select_player_tab(PlayerTab::UpNext, cx);
            music
                .player_page
                .queue_scroll
                .scroll_to_item(0, gpui::ScrollStrategy::Top);
            cx.notify();
        })
        .unwrap();
    wait_for_presented_frame(window, cx).await;
    // Enter by Tab, then pass through every row and menu, including the unrendered
    // queue tail and the autoplay separator. Shift Tab takes the same path back.
    for _ in 0..30 {
        if window
            .update(cx, |music, window, _| {
                music.queue_focus.focused(window) == Some(0)
            })
            .unwrap()
        {
            break;
        }
        key(window, cx, "tab").await;
    }
    let restores_before_queue = window
        .update(cx, |music, _, _| music.focus_restores)
        .unwrap();
    for slot in 0..66 {
        window
            .update(cx, |music, window, _| {
                assert_eq!(music.queue_focus.focused(window), Some(slot));
                let row = slot / 2;
                assert_row_visible(
                    &music.player_page.queue_scroll,
                    row + usize::from(row >= 30),
                    34,
                );
            })
            .unwrap();
        if slot == 49 {
            // Force a frame between focusing an undrawn row and revealing it. This
            // reproduced the app's focus-lost fallback stealing the queue target.
            window
                .update(cx, |music, window, cx| {
                    music.queue_focus.focus(50, window, cx);
                    cx.notify();
                })
                .unwrap();
            wait_for_presented_frame(window, cx).await;
            window
                .update(cx, |music, window, cx| {
                    assert_eq!(music.queue_focus.focused(window), Some(50));
                    assert_eq!(music.focus_restores, restores_before_queue);
                    music
                        .player_page
                        .queue_scroll
                        .scroll_to_item(25, gpui::ScrollStrategy::Nearest);
                    cx.notify();
                })
                .unwrap();
            wait_for_presented_frame(window, cx).await;
        } else if slot < 65 {
            key(window, cx, "tab").await;
        }
    }
    window
        .update(cx, |_, window, _| {
            capture(window, "queue-keyboard-tail");
        })
        .unwrap();
    key(window, cx, "tab").await;
    window
        .update(cx, |music, window, _| {
            assert_eq!(
                music.queue_focus.focused(window),
                None,
                "The final queue menu exits to the player bar"
            )
        })
        .unwrap();
    window
        .update(cx, |music, _, cx| {
            music
                .player_page
                .queue_scroll
                .scroll_to_item_strict(0, gpui::ScrollStrategy::Top);
            cx.notify();
        })
        .unwrap();
    wait_for_presented_frame(window, cx).await;
    key(window, cx, "shift-tab").await;
    for slot in (0..66).rev() {
        window
            .update(cx, |music, window, _| {
                assert_eq!(music.queue_focus.focused(window), Some(slot));
                let row = slot / 2;
                assert_row_visible(
                    &music.player_page.queue_scroll,
                    row + usize::from(row >= 30),
                    34,
                );
            })
            .unwrap();
        if slot > 0 {
            key(window, cx, "shift-tab").await;
        }
    }
    window
        .update(cx, |music, _, _| {
            assert_eq!(
                music.focus_restores, restores_before_queue,
                "Queue traversal must never restore root focus"
            );
            println!("QUEUE_FOCUS_RESTORES forward_reverse=0 forced_undrawn_target=passed");
        })
        .unwrap();
    // Scrolling a focused virtual row away must recover the same root shortcuts.
    window
        .update(cx, |music, _, cx| {
            music
                .player_page
                .queue_scroll
                .scroll_to_item_strict(29, gpui::ScrollStrategy::Top);
            cx.notify();
        })
        .unwrap();
    wait_for_presented_frame(window, cx).await;
    window.update(cx, |music, window, cx| {
        assert!(music.focus.is_focused(window), "Virtualizing the focused row must restore root focus");
        music.player_open = false;
        music.library.saved = (0..30).map(|index| serde_json::from_value(serde_json::json!({
            "id": format!("VLsynthetic-{index}"), "title": format!("Playlist {index}"), "subtitle": "Synthetic playlist"
        })).unwrap()).collect();
        window.resize(size(px(1280.), px(700.)));
        window.focus(&music.focus, cx);
        cx.notify();
    }).unwrap();
    wait_for_presented_frame(window, cx).await;
    for _ in 0..30 {
        if window
            .update(cx, |music, window, _| {
                music.sidebar_focus.focused(window) == Some(0)
            })
            .unwrap()
        {
            break;
        }
        key(window, cx, "tab").await;
    }
    for index in 0..30 {
        window
            .update(cx, |music, window, _| {
                assert_eq!(music.sidebar_focus.focused(window), Some(index));
                assert_row_visible(&music.sidebar_scroll, index, 30);
            })
            .unwrap();
        if index < 29 {
            key(window, cx, "tab").await;
        }
    }
    window
        .update(cx, |_, window, _| {
            capture(window, "sidebar-keyboard-tail");
        })
        .unwrap();
    key(window, cx, "tab").await;
    window
        .update(cx, |music, window, cx| {
            assert_eq!(music.sidebar_focus.focused(window), None);
            music
                .sidebar_scroll
                .scroll_to_item_strict(0, gpui::ScrollStrategy::Top);
            cx.notify();
        })
        .unwrap();
    wait_for_presented_frame(window, cx).await;
    key(window, cx, "shift-tab").await;
    window
        .update(cx, |music, window, _| {
            assert_eq!(
                music.sidebar_focus.focused(window),
                Some(29),
                "Shift Tab enters the final item even when it is not rendered"
            );
            assert_row_visible(&music.sidebar_scroll, 29, 30);
        })
        .unwrap();
    for index in (0..29).rev() {
        key(window, cx, "shift-tab").await;
        window
            .update(cx, |music, window, _| {
                assert_eq!(music.sidebar_focus.focused(window), Some(index));
                assert_row_visible(&music.sidebar_scroll, index, 30);
            })
            .unwrap();
    }
    println!("KEYBOARD_RECOVERY card=passed virtual_row=passed queue_slots=66 sidebar_rows=30");
}

async fn library_sort_and_compact_layouts(window: WindowHandle<Music>, cx: &mut AsyncApp) {
    use crate::app::{CatalogRequests, Query, Route};
    use serde_json::json;
    let option = |label: &str, token: &str| {
        json!({"musicMultiSelectMenuItemRenderer":{
            "title":{"runs":[{"text":label}]},
            "selectedCommand":{"commandExecutorCommand":{"commands":[{"browseSectionListReloadEndpoint":{"continuation":{"reloadContinuationData":{"continuation":token}}}}]}}
        }})
    };
    let response = json!({"contents":{"singleColumnBrowseResultsRenderer":{"tabs":[{"tabRenderer":{"content":{"sectionListRenderer":{
        "header":{"musicSideAlignedItemRenderer":{
            "startItems":[{"chipCloudRenderer":{"chips":[]}}],
            "endItems":[{"musicSortFilterButtonRenderer":{
            "title":{"runs":[{"text":"Recent activity"}]},
            "menu":{"musicMultiSelectMenuRenderer":{"options":[option("Recent activity","activity"),option("Recently saved","saved")]}}
        }}]}},
        "contents":[{"gridRenderer":{"items":(0..10).map(|index| json!({"musicTwoRowItemRenderer":{
            "title":{"runs":[{"text":format!("Playlist {}",index+1)}]},
            "subtitle":{"runs":[{"text":"Synthetic playlist"}]},
            "navigationEndpoint":{"browseEndpoint":{"browseId":format!("VLsynthetic-{index}")}}
        }})).collect::<Vec<_>>(),"itemSize":"COLLECTION_STYLE_ITEM_SIZE_SMALL"}}]
    }}}}]}}});
    window
        .update(cx, |music, window, cx| {
            music.player_open = false;
            music.account = Some(super::account::connection(super::account::session(
                "keyboard-test",
            )));
            music.library.saved.clear();
            music.catalog_requests = CatalogRequests::new();
            music.route = Route::Saved;
            music.page = crate::catalog::parse_page(&response, "Library").unwrap();
            assert_eq!(music.page.library.as_ref().unwrap().sort.len(), 2);
            music.rebuild_rows();
            music
                .list_scroll
                .scroll_to_item_strict(0, gpui::ScrollStrategy::Top);
            music.library.volume = 0.;
            music.error = None;
            window.resize(size(px(850.), px(560.)));
            window.focus(&music.focus, cx);
            cx.notify();
        })
        .unwrap();
    wait_for_presented_frame(window, cx).await;
    window
        .update(cx, |music, window, cx| window.focus(&music.sort_focus, cx))
        .unwrap();
    wait_for_presented_frame(window, cx).await;
    key(window, cx, "enter").await;
    let first = window
        .update(cx, |music, window, cx| {
            assert!(music.sort_menu);
            assert!(music.overlay_focus.contains_focused(window, cx));
            window.focused(cx).unwrap()
        })
        .unwrap();
    key(window, cx, "down").await;
    let second = window
        .update(cx, |music, window, cx| {
            assert_eq!(music.library.volume, 0.);
            let focused = window.focused(cx).unwrap();
            assert_ne!(focused, first);
            assert!(music.overlay_focus.contains_focused(window, cx));
            focused
        })
        .unwrap();
    key(window, cx, "up").await;
    window
        .update(cx, |music, window, cx| {
            assert_eq!(
                music.library.volume, 0.,
                "Sort arrows must never change volume"
            );
            assert_eq!(window.focused(cx), Some(first.clone()));
            capture(window, "library-sort-focused");
        })
        .unwrap();
    for (key_name, expected) in [
        ("end", second.clone()),
        ("home", first.clone()),
        ("shift-tab", second.clone()),
        ("tab", first),
    ] {
        key(window, cx, key_name).await;
        window
            .update(cx, |_, window, cx| {
                assert_eq!(window.focused(cx), Some(expected))
            })
            .unwrap();
    }
    key(window, cx, "escape").await;
    window
        .update(cx, |music, window, _| {
            assert!(!music.sort_menu);
            assert!(music.sort_focus.is_focused(window));
        })
        .unwrap();
    key(window, cx, "enter").await;
    key(window, cx, "down").await;
    key(window, cx, "enter").await;
    window.update(cx,|music,window,_| {
        assert!(!music.sort_menu);
        assert!(music.sort_focus.is_focused(window));
        assert!(matches!(music.catalog_requests.pending.try_recv().unwrap().query,Query::Reload(ref reload) if reload.token()=="saved"));
    }).unwrap();
    for width in [850., 1000.] {
        window
            .update(cx, |music, window, cx| {
                window.resize(size(px(width), px(560.)));
                music.catalog_requests = CatalogRequests::new();
                music
                    .list_scroll
                    .scroll_to_item_strict(0, gpui::ScrollStrategy::Top);
                window.focus(&music.sort_focus, cx);
                cx.notify();
            })
            .unwrap();
        wait_for_presented_frame(window, cx).await;
        key(window, cx, "tab").await;
        window
            .update(cx, |_, window, _| {
                capture(window, &format!("library-card-{width}"));
            })
            .unwrap();
        window
            .update(cx, |music, window, cx| {
                music.account = None;
                music.autoplay_tracks = crate::app::sync::tests::page(3, false).tracks().into();
                music.player_open = true;
                music.library.queue.playing_from = Some("Synthetic playlist".into());
                music.select_player_tab(PlayerTab::UpNext, cx);
                music
                    .player_page
                    .queue_scroll
                    .scroll_to_item_strict(0, gpui::ScrollStrategy::Top);
                music.playback = Playback::Loaded {
                    paused: true,
                    buffering: false,
                };
                music.player_page.hovered_row = None;
                window.focus(&music.focus, cx);
                cx.notify();
            })
            .unwrap();
        wait_for_presented_frame(window, cx).await;
        window
            .update(cx, |music, window, cx| {
                capture(window, &format!("queue-paused-{width}"));
                music.player_page.hovered_row = Some(0);
                cx.notify();
            })
            .unwrap();
        wait_for_presented_frame(window, cx).await;
        window
            .update(cx, |_, window, _| {
                capture(window, &format!("queue-hover-{width}"));
            })
            .unwrap();
        // Reach the filled Save pill with actual Tab input and capture its ring.
        for _ in 0..30 {
            key(window, cx, "tab").await;
            if window
                .update(cx, |music, window, _| {
                    music.queue_focus.focused(window) == Some(0)
                })
                .unwrap()
            {
                break;
            }
        }
        window
            .update(cx, |music, window, _| {
                assert_eq!(music.queue_focus.focused(window), Some(0))
            })
            .unwrap();
        key(window, cx, "shift-tab").await;
        key(window, cx, "shift-tab").await;
        window
            .update(cx, |_, window, _| {
                capture(window, &format!("queue-save-focused-{width}"));
            })
            .unwrap();
        key(window, cx, "tab").await;
        key(window, cx, "tab").await;
        for slot in 0..66 {
            window
                .update(cx, |music, window, cx| {
                    assert_eq!(music.queue_focus.focused(window), Some(slot), "width={width} queued={} autoplay={} root={} restores={} raw={:?} scroll={:?}",music.library.queue.tracks.len(),music.autoplay_tracks.len(),music.focus.is_focused(window),music.focus_restores,window.focused(cx),music.player_page.queue_scroll.0.borrow().base_handle.offset());
                    let row = slot / 2;
                    assert_row_visible(
                        &music.player_page.queue_scroll,
                        row + usize::from(row >= 30),
                        34,
                    );
                })
                .unwrap();
            if slot < 65 {
                key(window, cx, "tab").await;
            }
        }
        window
            .update(cx, |_, window, _| {
                capture(window, &format!("queue-tail-{width}"));
            })
            .unwrap();
        key(window, cx, "tab").await;
        window
            .update(cx, |music, window, _| {
                assert_eq!(music.queue_focus.focused(window), None)
            })
            .unwrap();
        window
            .update(cx, |music, _, cx| {
                music
                    .player_page
                    .queue_scroll
                    .scroll_to_item_strict(0, gpui::ScrollStrategy::Top);
                cx.notify();
            })
            .unwrap();
        wait_for_presented_frame(window, cx).await;
        window
            .update(cx, |music, window, cx| {
                music.queue_focus.focus(0, window, cx)
            })
            .unwrap();
        key(window, cx, "shift-tab").await;
        key(window, cx, "space").await;
        window
            .update(cx, |music, _, cx| {
                assert!(
                    !music.library.autoplay,
                    "The autoplay control must remain reachable at compact widths"
                );
                music.library.autoplay = true;
                music.player_open = false;
                cx.notify();
            })
            .unwrap();
    }
    println!(
        "LIBRARY_SORT arrows=passed volume_unchanged=passed enter_reload=passed escape_restore=passed tab_trap=passed compact_widths=850,1000"
    );
}

async fn inline_links_follow_row_order(window: WindowHandle<Music>, cx: &mut AsyncApp) {
    use crate::app::{CatalogRequests, Route};
    use crate::catalog::{BrowseLink, Item};
    let links = [
        BrowseLink::new("UCfirst", "First artist"),
        BrowseLink::new("UCsecond", "Second artist"),
        BrowseLink::new("MPREalbum", "Album"),
    ];
    for reverse in [false, true] {
        for (index, expected) in links.iter().enumerate() {
            let queue = window.update(cx, |music, window, cx| {
                music.account = None;
                music.library.saved.clear();
                music.player_open = false;
                music.overlay = None;
                music.back.clear();
                music.catalog_requests = CatalogRequests::new();
                music.route = Route::Browse(BrowseLink::new("VLkeyboard", "Keyboard playlist"));
                music.page = crate::app::sync::tests::page(2, false);
                music.page.header = crate::catalog::parse_page(&serde_json::json!({"header":{"musicResponsiveHeaderRenderer":{"title":{"simpleText":"Keyboard playlist"}}},"contents":[]}), "Keyboard playlist").unwrap().header;
                for item in &mut music.page.shelves[0].items {
                    let Item::Track(track) = item else { unreachable!() };
                    track.artist = "First artist & Second artist".into();
                    track.album = Some("Album".into());
                    track.details.links = links.to_vec();
                    track.duration = Some(719);
                }
                music.error = None;
                music.notice = None;
                music.rebuild_rows();
                music.detail_list = crate::app::DetailList::new();
                music.detail_list.reset_rows(music.list_rows.len());
                window.resize(size(px(850.), px(560.)));
                window.focus(&music.focus, cx);
                cx.notify();
                (music.library.queue.current().unwrap().id.clone(), music.active_id)
            }).unwrap();
            wait_for_presented_frame(window, cx).await;
            let idle = (!reverse && index == 0).then(|| {
                window
                    .update(cx, |_, window, _| capture(window, "track-controls-idle"))
                    .unwrap()
            });
            if idle.is_some() {
                for (name, position) in [
                    ("track-controls-hover", point(px(500.), px(330.))),
                    ("track-controls-away", point(px(1.), px(70.))),
                ] {
                    cx.update_window(window.into(), |_, window, cx| {
                        window.dispatch_event(
                            gpui::PlatformInput::MouseMove(gpui::MouseMoveEvent {
                                position,
                                ..Default::default()
                            }),
                            cx,
                        );
                    })
                    .unwrap();
                    wait_for_presented_frame(window, cx).await;
                    window
                        .update(cx, |_, window, _| capture(window, name))
                        .unwrap();
                }
            }
            // Four header controls, three sidebar controls, three playlist
            // actions, then the row, its two artists, its album and row actions.
            for step in 0..if reverse { 14 } else { 12 + index } {
                key(window, cx, "tab").await;
                if idle.is_some() && step == 3 {
                    window
                        .update(cx, |_, window, _| capture(window, "focus-white-button"))
                        .unwrap();
                }
                if let Some(idle) = &idle
                    && step == 10
                {
                    let focused = window
                        .update(cx, |_, window, _| capture(window, "track-controls-focused"))
                        .unwrap();
                    let title = |image: &image::RgbaImage| {
                        image::imageops::crop_imm(image, 210, 308, 400, 20).to_image()
                    };
                    let actions = |image: &image::RgbaImage| {
                        image::imageops::crop_imm(image, 675, 310, 110, 40).to_image()
                    };
                    assert!(
                        title(idle) == title(&focused),
                        "Revealing row controls must not move the title"
                    );
                    assert!(
                        actions(idle) != actions(&focused),
                        "Keyboard row focus must reveal the action overlay"
                    );
                }
            }
            if reverse {
                for _ in index..2 {
                    key(window, cx, "shift-tab").await;
                }
            }
            if !reverse {
                window
                    .update(cx, |_, window, _| {
                        capture(window, &format!("inline-link-{index}"));
                    })
                    .unwrap();
            }
            if !reverse && index == 0 {
                for _ in 0..5 {
                    key(window, cx, "tab").await;
                }
                key(window, cx, "enter").await;
                window
                    .update(cx, |music, window, _| {
                        assert!(
                            matches!(music.overlay, Some(crate::app::Overlay::Track { .. })),
                            "The overlay menu must remain keyboard reachable"
                        );
                        capture(window, "track-controls-menu");
                    })
                    .unwrap();
                key(window, cx, "escape").await;
                for _ in 0..5 {
                    key(window, cx, "shift-tab").await;
                }
            }
            key(window, cx, "enter").await;
            window.update(cx, |music, _, _| {
                assert!(music.route == Route::Browse(expected.clone()), "Enter must open the inline link in text order, reverse={reverse} index={index}");
                assert_eq!((music.library.queue.current().unwrap().id.clone(), music.active_id), queue, "Activating an inline link must not play its row");
                assert!(music.overlay.is_none(), "Inline activation must not open the row menu");
            }).unwrap();
        }
    }
    println!("INLINE_LINKS forward=passed reverse=passed enter=passed row_playback=unchanged");
}

fn prepare_x11_capture(id: u64) -> (u32, u32) {
    unsafe extern "C" {
        fn XOpenDisplay(name: *const libc::c_char) -> *mut libc::c_void;
        fn XGetGeometry(
            display: *mut libc::c_void,
            window: libc::c_ulong,
            root: *mut libc::c_ulong,
            x: *mut libc::c_int,
            y: *mut libc::c_int,
            width: *mut libc::c_uint,
            height: *mut libc::c_uint,
            border: *mut libc::c_uint,
            depth: *mut libc::c_uint,
        ) -> libc::c_int;
        fn XTranslateCoordinates(
            display: *mut libc::c_void,
            source: libc::c_ulong,
            destination: libc::c_ulong,
            source_x: libc::c_int,
            source_y: libc::c_int,
            x: *mut libc::c_int,
            y: *mut libc::c_int,
            child: *mut libc::c_ulong,
        ) -> libc::c_int;
        fn XMoveWindow(
            display: *mut libc::c_void,
            window: libc::c_ulong,
            x: libc::c_int,
            y: libc::c_int,
        ) -> libc::c_int;
        fn XSync(display: *mut libc::c_void, discard: libc::c_int) -> libc::c_int;
        fn XCloseDisplay(display: *mut libc::c_void) -> libc::c_int;
    }
    // The private connection touches only this app's live window and root geometry.
    // Every output pointer refers to a local, and we close the connection on error.
    unsafe {
        let display = XOpenDisplay(std::ptr::null());
        assert!(!display.is_null(), "Connect to X11 for native pixel checks");
        let geometry = |window| {
            let (mut root, mut x, mut y, mut width, mut height, mut border, mut depth) =
                (0, 0, 0, 0, 0, 0, 0);
            let status = XGetGeometry(
                display,
                window,
                &mut root,
                &mut x,
                &mut y,
                &mut width,
                &mut height,
                &mut border,
                &mut depth,
            );
            if status == 0 {
                XCloseDisplay(display);
                panic!("Read X11 geometry for window {window}");
            }
            (root, width, height)
        };
        let (root, width, height) = geometry(id);
        let (_, desktop_width, desktop_height) = geometry(root);
        let (mut x, mut y, mut child) = (0, 0, 0);
        let translated = XTranslateCoordinates(display, id, root, 0, 0, &mut x, &mut y, &mut child);
        if translated == 0 || width > desktop_width || height > desktop_height {
            XCloseDisplay(display);
            panic!(
                "App window {id} ({width}x{height}) must fit the X11 desktop ({desktop_width}x{desktop_height})"
            );
        }
        // ImageMagick clips to the root even for an override-redirect popup.
        // Keep the entire test drawable onscreen without changing its size.
        let left = x.clamp(0, (desktop_width - width) as i32);
        let top = y.clamp(0, (desktop_height - height) as i32);
        if (x, y) != (left, top) {
            XMoveWindow(display, id, left, top);
            XSync(display, 0);
            println!(
                "CAPTURE_X11_POSITION from=({x},{y}) to=({left},{top}) size=({width},{height}) desktop=({desktop_width},{desktop_height})"
            );
        }
        XCloseDisplay(display);
        (width, height)
    }
}

pub(super) fn capture(window: &gpui::Window, name: &str) -> image::RgbaImage {
    let id = match HasWindowHandle::window_handle(window).unwrap().as_raw() {
        RawWindowHandle::Xcb(handle) => u64::from(handle.window.get()),
        RawWindowHandle::Xlib(handle) => handle.window,
        _ => panic!("Run the pixel regression check on X11, with WAYLAND_DISPLAY unset"),
    };
    let path = std::path::PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").unwrap())
        .join(format!("{name}.png"));
    let actual = prepare_x11_capture(id);
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
    let image = image::open(&path).unwrap().to_rgba8();
    let viewport = window.viewport_size();
    let scale = window.scale_factor();
    assert_eq!(
        image.dimensions(),
        actual,
        "Capture must contain the app window at its actual X11 dimensions"
    );
    let gpui_size = (
        (f32::from(viewport.width) * scale).round() as u32,
        (f32::from(viewport.height) * scale).round() as u32,
    );
    if actual != gpui_size {
        println!("CAPTURE_X11_GEOMETRY name={name} gpui={gpui_size:?} x11={actual:?}");
    }
    // Comparison loops use the pixels immediately. Keep representative states,
    // rather than dozens of nearly identical PNGs in a quota-limited runtime.
    if (name.starts_with("Lyrics-") || name.starts_with("Related-") || name.starts_with("artist-"))
        && (name.contains("-tab-") || name.contains("-focus-") || name.contains("-tick-"))
        && !(name.starts_with("artist-") && name.ends_with("-focus-0"))
    {
        std::fs::remove_file(path).unwrap();
    }
    image
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

async fn scrolled_artist_survives_tab(
    window: WindowHandle<Music>,
    cx: &mut AsyncApp,
    bounds_changes: &Cell<u64>,
) {
    use crate::app::{ListRow, Route};
    use crate::catalog::{BrowseLink, Collection, Item, PageHeader, Shelf, ShelfLayout};
    use sha1::{Digest, Sha1};

    // Seed the real artwork loader's private disk cache, keeping the pixel test
    // offline while exercising decoded textures as well as text and SVG glyphs.
    let cache = std::path::PathBuf::from(std::env::var_os("XDG_CACHE_HOME").unwrap())
        .join("yt-music-gpui/artwork");
    std::fs::create_dir_all(&cache).unwrap();
    let urls = (0..6)
        .map(|index| {
            let url = format!("https://i.ytimg.com/vi/native-tab-{index}/cover.png");
            let image = image::RgbaImage::from_fn(96, 96, |x, y| {
                image::Rgba([80 + index * 25, (x * 2) as u8, (y * 2) as u8, 255])
            });
            image
                .save_with_format(
                    cache.join(format!("{:x}", Sha1::digest(url.as_bytes()))),
                    image::ImageFormat::Png,
                )
                .unwrap();
            url
        })
        .collect::<Vec<_>>();
    let covers = urls.clone();
    let albums_row = window
        .update(cx, |music, window, cx| {
            music.player_open = false;
            music.route = Route::Browse(BrowseLink::new("UCnative-artist", "Synthetic artist"));
            music.page = crate::app::sync::tests::page(5, false);
            music.page.title = "Synthetic artist".into();
            music.page.header = Some(PageHeader {
                collection: Default::default(),
                thumbnail: Some(covers[0].clone()),
                artist: None,
                artist_thumbnail: None,
                owner: String::new(),
                subtitle: "Artist".into(),
                stats: String::new(),
                shuffle: None,
                mix: None,
                subscribed: None,
                subscription_label: String::new(),
                subscriber_count: String::new(),
            });
            for title in ["Albums", "Singles & EPs", "Videos"] {
                music.page.shelves.push(Shelf {
                    title: title.into(),
                    layout: ShelfLayout::Cards,
                    more: None,
                    items: covers
                        .iter()
                        .enumerate()
                        .map(|(index, url)| {
                            Item::Collection(Collection {
                                id: format!("MPREnative-{index}"),
                                title: format!("Album {}", index + 1),
                                subtitle: "2026".into(),
                                thumbnail: Some(url.clone()),
                                details: Default::default(),
                            })
                        })
                        .collect(),
                });
            }
            music.error = None;
            music.notice = None;
            music.rebuild_rows();
            music.detail_list = crate::app::DetailList::new();
            music.detail_list.reset_rows(music.list_rows.len());
            music.shelf_scroll.clear();
            window.resize(size(px(1916.), px(1050.)));
            window.focus(&music.focus, cx);
            cx.notify();
            music
                .list_rows
                .iter()
                .position(|row| matches!(row, ListRow::Shelf(1)))
                .unwrap()
                + 1
        })
        .unwrap();
    wait_for_presented_frame(window, cx).await;
    window
        .update(cx, |music, _, cx| {
            music.detail_list.state.scroll_to(gpui::ListOffset {
                item_ix: albums_row,
                offset_in_item: px(0.),
            });
            cx.notify();
        })
        .unwrap();
    wait_for_presented_frame(window, cx).await;
    for attempt in 0..100 {
        let loaded = window
            .update(cx, |music, _, _| {
                urls.iter().all(|url| music.artwork.get(url).is_some())
                    && music.artwork.get_hero(&urls[0]).is_some()
            })
            .unwrap();
        if loaded {
            break;
        }
        assert!(
            attempt < 99,
            "Synthetic covers must finish loading before pixel comparisons"
        );
        cx.background_executor()
            .timer(Duration::from_millis(20))
            .await;
    }
    wait_for_presented_frame(window, cx).await;
    for paused in [false, true] {
        window
            .update(cx, |music, window, cx| {
                music.playback = Playback::Loaded {
                    paused,
                    buffering: false,
                };
                window.focus(&music.focus, cx);
                cx.notify();
            })
            .unwrap();
        wait_for_presented_frame(window, cx).await;
        let baseline = window
            .update(cx, |music, window, _| {
                assert_eq!(
                    music.detail_list.state.logical_scroll_top().item_ix,
                    albums_row
                );
                capture(window, &format!("artist-{paused}-before"))
            })
            .unwrap();
        if !paused {
            let deadline = Instant::now() + Duration::from_secs(10);
            let (before, after) = loop {
                let before = wait_for_quiescence(window, cx).await;
                let bounds_before = bounds_changes.get();
                for seconds in 20..25 {
                    window
                        .update(cx, |music, _, cx| {
                            music.player_event(
                                PlayerEvent::Position {
                                    track_id: 42,
                                    seconds: f64::from(seconds),
                                },
                                cx,
                            );
                        })
                        .unwrap();
                    wait_for_presented_frame(window, cx).await;
                }
                let after = window
                    .update(cx, |music, _, cx| render_counts(music, cx))
                    .unwrap();
                if bounds_changes.get() == bounds_before {
                    break (before, after);
                }
                assert!(
                    Instant::now() < deadline,
                    "Window bounds kept changing during artist position measurement"
                );
                println!("BROWSE_POSITION_MEASUREMENT_RETRY window_bounds_changed=true");
            };
            assert_eq!(
                after.0, before.0,
                "Position samples must reuse the artist page body"
            );
            assert!(after.1 >= before.1 + 5);
            println!(
                "BROWSE_POSITION_RENDERS body=0 bar={} root={}",
                after.1 - before.1,
                after.2 - before.2
            );
        }
        for step in 0..24 {
            let before = cx
                .update_window(window.into(), |_, window, cx| window.focused(cx))
                .unwrap();
            key(window, cx, "tab").await;
            let focused = window
                .update(cx, |_, window, cx| {
                    assert_ne!(
                        window.focused(cx),
                        before,
                        "Tab must advance on a scrolled artist page"
                    );
                    capture(window, &format!("artist-{paused}-focus-{step}"))
                })
                .unwrap();
            assert_page_retained(&baseline, &focused);
            for (region, x0, y0, x1, y1) in [
                ("header", 0, 0, baseline.width(), 64),
                ("sidebar", 0, 64, 240, 400),
            ] {
                let bright = |image: &image::RgbaImage| {
                    (y0..y1)
                        .flat_map(|y| (x0..x1).map(move |x| image.get_pixel(x, y)))
                        .filter(|pixel| pixel.0[..3].iter().any(|channel| *channel >= 100))
                        .count()
                };
                let before = bright(&baseline);
                let after = bright(&focused);
                let area = f64::from((x1 - x0) * (y1 - y0));
                println!(
                    "ARTIST_TAB paused={paused} step={step} {region}_bright before={:.6} after={:.6}",
                    before as f64 / area,
                    after as f64 / area
                );
                assert!(
                    before > 100 && after as f64 >= before as f64 * 0.98,
                    "Tab erased {region}"
                );
            }
            if !paused {
                window
                    .update(cx, |music, _, cx| {
                        music.player_event(
                            PlayerEvent::Position {
                                track_id: 42,
                                seconds: f64::from(30 + step),
                            },
                            cx,
                        )
                    })
                    .unwrap();
                wait_for_presented_frame(window, cx).await;
                window
                    .update(cx, |_, window, _| {
                        assert_page_retained(
                            &baseline,
                            &capture(window, &format!("artist-{paused}-tick-{step}")),
                        );
                    })
                    .unwrap();
            }
        }
    }
    let sidebar = window
        .update(cx, |music, window, cx| {
            window.resize(size(px(1280.), px(900.)));
            let sidebar = music.sidebar_collapsed;
            music.sidebar_collapsed = Some(false);
            for shelf in &mut music.page.shelves[1..] {
                let Item::Collection(album) = &mut shelf.items[0] else {
                    unreachable!()
                };
                album.title = "Random Access Memories (Drumless Edition)".into();
            }
            music.detail_list.state.remeasure();
            music.detail_list.state.scroll_to(gpui::ListOffset {
                item_ix: albums_row,
                offset_in_item: px(0.),
            });
            window.focus(&music.focus, cx);
            cx.notify();
            sidebar
        })
        .unwrap();
    wait_for_presented_frame(window, cx).await;
    let shelf_height = window
        .update(cx, |music, window, _| {
            let height = f32::from(music.detail_list.state.bounds_for_item(albums_row).unwrap().size.height);
            assert!((height - 347.8).abs() < 1., "A two-line title and one-line subtitle shelf must match the web's 347.8px pitch, got {height}");
            println!("SHELF_GEOMETRY width=1280 pitch={height:.2} web=347.8");
            capture(window, "shelf-geometry-1280");
            let scroll = &music.shelf_scroll[&1];
            scroll.set_offset(point(-scroll.max_offset().x, px(0.)));
            window.refresh();
            height
        })
        .unwrap();
    wait_for_presented_frame(window, cx).await;
    window
        .update(cx, |music, _, _| {
            assert_eq!(
                f32::from(
                    music
                        .detail_list
                        .state
                        .bounds_for_item(albums_row)
                        .unwrap()
                        .size
                        .height
                ),
                shelf_height,
                "Scrolling past a wrapped title must not change shelf height"
            );
        })
        .unwrap();
    artist_revisit_shows_hero(window, cx).await;
    window
        .update(cx, |music, window, cx| {
            music.playback = Playback::Loaded {
                paused: false,
                buffering: false,
            };
            music.sidebar_collapsed = sidebar;
            window.resize(size(px(850.), px(560.)));
            window.focus(&music.focus, cx);
            cx.notify();
        })
        .unwrap();
    wait_for_presented_frame(window, cx).await;
}

async fn artist_revisit_shows_hero(window: WindowHandle<Music>, cx: &mut AsyncApp) {
    use crate::app::{CatalogRequests, CatalogResult, Query, Route};
    use crate::catalog::{Item, ShelfLayout};

    let (artist, page) = window
        .update(cx, |music, window, cx| {
            music.catalog_requests = CatalogRequests::new();
            music.account = None;
            music.library.saved.clear();
            music
                .detail_list
                .state
                .scroll_to(gpui::ListOffset::default());
            window.focus(&music.focus, cx);
            cx.notify();
            let Route::Browse(artist) = music.route.clone() else {
                unreachable!()
            };
            (artist, music.page.clone())
        })
        .unwrap();
    wait_for_quiescence(window, cx).await;
    let baseline = window
        .update(cx, |_, window, _| {
            capture(window, "artist-first-visit-hero")
        })
        .unwrap();
    // This patch of the decoded photo is clear of the header controls and artist text.
    let hero = image::imageops::crop_imm(&baseline, 1000, 90, 500, 120).to_image();
    let colorful = hero
        .pixels()
        .filter(|pixel| {
            let [r, g, b, _] = pixel.0;
            r.max(g).max(b) - r.min(g).min(b) > 20
        })
        .count();
    assert!(
        colorful > 30_000,
        "First visit must contain the seeded hero photo: {colorful}"
    );
    window
        .update(cx, |music, _, cx| {
            music.navigate(Route::Home, cx);
            let job = music.catalog_requests.pending.try_recv().unwrap();
            assert!(matches!(job.query, Query::Home));
            let mut home = crate::app::sync::tests::page(0, false);
            home.shelves[0].layout = ShelfLayout::Cards;
            home.shelves[0].items.push(Item::Navigation(artist.clone()));
            music.catalog_result(
                CatalogResult {
                    id: job.id,
                    account_revision: job.account_revision,
                    append: false,
                    revalidate: false,
                    result: Ok(home),
                    session_rejected: false,
                    target: CatalogTarget::Page,
                },
                cx,
            );
        })
        .unwrap();
    wait_for_presented_frame(window, cx).await;
    // Four header controls and six expanded sidebar controls precede the artist link.
    for _ in 0..11 {
        key(window, cx, "tab").await;
    }
    window
        .update(cx, |_, window, _| {
            capture(window, "artist-revisit-link-focused");
        })
        .unwrap();
    key(window, cx, "enter").await;
    window
        .update(cx, |music, _, cx| {
            assert!(
                music.route == Route::Browse(artist.clone()),
                "Enter must reopen the artist through the Home link"
            );
            let job = music.catalog_requests.pending.try_recv().unwrap();
            assert!(matches!(&job.query, Query::Browse(link) if link == &artist));
            assert!(
                music.artwork.is_idle(),
                "Revisit must use decoded artwork without a late image completion"
            );
            assert!(
                music
                    .artwork
                    .get_hero(page.header.as_ref().unwrap().thumbnail.as_ref().unwrap())
                    .is_some()
            );
            music.catalog_result(
                CatalogResult {
                    id: job.id,
                    account_revision: job.account_revision,
                    append: false,
                    revalidate: false,
                    result: Ok(page),
                    session_rejected: false,
                    target: CatalogTarget::Page,
                },
                cx,
            );
        })
        .unwrap();
    // No scroll, input, resize, sleep or manual notify after the response. The
    // heading measurement must itself arrange the first redraw with cached art.
    wait_for_presented_frame(window, cx).await;
    let revisit = window
        .update(cx, |_, window, _| {
            capture(window, "artist-revisit-first-frames")
        })
        .unwrap();
    let revisit = image::imageops::crop_imm(&revisit, 1000, 90, 500, 120).to_image();
    let missing = hero
        .pixels()
        .zip(revisit.pixels())
        .filter(|(before, after)| {
            before.0[..3]
                .iter()
                .zip(&after.0[..3])
                .any(|(a, b)| a.abs_diff(*b) > 6)
        })
        .count();
    println!("ARTIST_REVISIT_HERO changed_pixels={missing}/60000 colorful_baseline={colorful}");
    assert!(
        missing < 600,
        "Cached hero photo must be present in the first presented frames after reopening through a link: {missing}/60000 changed pixels"
    );
}

async fn home_chips_and_narrow_error(window: WindowHandle<Music>, cx: &mut AsyncApp) {
    let chip = |label, selected, params| {
        serde_json::json!({"chipCloudChipRenderer": {
            "text":{"simpleText":label}, "isSelected":selected,
            "navigationEndpoint":{"browseEndpoint":{"browseId":"FEmusic_home","params":params}},
            "onDeselectedCommand":{"browseEndpoint":{"browseId":"FEmusic_home","params":"all-filter"}}
        }})
    };
    let response = serde_json::json!({"contents":{"singleColumnBrowseResultsRenderer":{"tabs":[{"tabRenderer":{"content":{
        "sectionListRenderer":{"contents":[],"header":{"chipCloudRenderer":{"chips":[
            chip("Relax", true, "relax-filter"), chip("Focus", false, "focus-filter")
        ]}}}
    }}}]}}});
    window.update(cx, |music, window, cx| {
        window.resize(size(px(956.), px(1050.)));
        music.player_open = false;
        music.playback = Playback::Stopped;
        music.route = crate::app::Route::Home;
        music.catalog_requests.cancel(CatalogTarget::Page);
        music.page = crate::catalog::parse_home(&response, "Home").unwrap();
        assert_eq!(music.page.chips.len(), 3);
        music.page.shelves = crate::app::sync::tests::page(1, false).shelves;
        music.rebuild_rows();
        music.detail_list.state.scroll_to(gpui::ListOffset::default());
        music.notice = None;
        music.error = Some("Could not load Home: error sending request for url (https://music.youtube.com/youtubei/v1/browse): client error (Connect): dns error: failed to lookup address information".into());
        cx.notify();
    }).unwrap();
    wait_for_presented_frame(window, cx).await;
    window
        .update(cx, |_, window, _| {
            capture(window, "home-chips-network-error-956");
        })
        .unwrap();
    // The dismiss control stays at the right of the constrained content column.
    let position = window
        .update(cx, |music, window, _| {
            let layout = music.page_layout(window);
            point(
                window.viewport_size().width - px(layout.inset + 20.),
                px(84.),
            )
        })
        .unwrap();
    cx.update_window(window.into(), |_, window, cx| {
        window.dispatch_event(
            gpui::PlatformInput::MouseDown(gpui::MouseDownEvent {
                button: gpui::MouseButton::Left,
                position,
                modifiers: Default::default(),
                click_count: 1,
                first_mouse: false,
            }),
            cx,
        );
        window.dispatch_event(
            gpui::PlatformInput::MouseUp(gpui::MouseUpEvent {
                button: gpui::MouseButton::Left,
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
        .update(cx, |music, window, cx| {
            assert!(
                music.error.is_none(),
                "The visible dismiss button must accept a click at 956px"
            );
            capture(window, "home-chips-dismissed-956");
            // This browse row used to add a second hover style to navigation().
            music.route = crate::app::Route::Browse(crate::catalog::BrowseLink::new(
                "FEmusic_moods_and_genres",
                "Moods",
            ));
            music.page = crate::app::sync::tests::page(0, false);
            music.page.shelves[0]
                .items
                .push(crate::catalog::Item::Navigation(
                    crate::catalog::BrowseLink::new("FEmusic_home", "Home"),
                ));
            music.rebuild_rows();
            cx.notify();
            println!("DEBUG_HOME_CHIPS rendered=true narrow_error_dismissed=true");
        })
        .unwrap();
    wait_for_presented_frame(window, cx).await;
}

async fn search_song_rows(window: WindowHandle<Music>, cx: &mut AsyncApp) {
    for (width, left, column) in [(956., 154, 720), (1920., 650, 860)] {
        window
            .update(cx, |music, window, cx| {
                window.resize(size(px(width), px(1050.)));
                music.sidebar_collapsed = None;
                music.account = None;
                music.catalog_requests = crate::app::CatalogRequests::new();
                music.player_open = false;
                music.library.queue = Default::default();
                music.route = crate::app::Route::Search(crate::app::Search {
                    query: "Synthetic search".into(),
                    filter: crate::search::SearchFilter::Songs,
                    scope: crate::search::SearchScope::Catalog,
                });
                music.page = crate::app::sync::tests::page(3, false);
                for item in &mut music.page.shelves[0].items {
                    if let crate::catalog::Item::Track(track) = item {
                        track.duration = Some(266);
                        track.details.byline = Some("Synthetic artist • Album • 4:26".into());
                    }
                }
                music.error = None;
                music.notice = None;
                music.detail_list = crate::app::DetailList::new();
                music.rebuild_rows();
                window.focus(&music.focus, cx);
                cx.notify();
            })
            .unwrap();
        wait_for_presented_frame(window, cx).await;
        let baseline = window
            .update(cx, |_, window, _| {
                capture(window, &format!("search-songs-{width}"))
            })
            .unwrap();
        // The web starts its first 80px row at y=172, then adds a 1px divider.
        for y in [252, 333, 414] {
            let separator = baseline.get_pixel(left, y);
            assert!(separator.0[0] > baseline.get_pixel(left, y - 1).0[0] + 15);
            assert!((left..left + column).all(|x| baseline.get_pixel(x, y) == separator));
        }
        window
            .update(cx, |music, _, cx| {
                let crate::catalog::Item::Track(track) = &mut music.rows[0] else {
                    panic!("Expected a song search result");
                };
                // Only the byline should display duration in search results.
                track.duration = Some(600);
                cx.notify();
            })
            .unwrap();
        wait_for_presented_frame(window, cx).await;
        let changed = window
            .update(cx, |_, window, _| {
                capture(window, &format!("search-duration-{width}"))
            })
            .unwrap();
        assert!(
            baseline == changed,
            "Search must not repeat duration in a column"
        );
    }
    println!("SEARCH_ROWS widths=956,1920 first_row=172 pitch=81 duration=byline_only");
}

async fn guide_chrome(window: WindowHandle<Music>, cx: &mut AsyncApp) {
    for width in [956., 1920.] {
        for home in [true, false] {
            for scrolled in [false, true] {
                window
                    .update(cx, |music, window, cx| {
                        window.resize(size(px(width), px(1050.)));
                        music.sidebar_collapsed = None;
                        music.catalog_requests = crate::app::CatalogRequests::new();
                        music.player_open = false;
                        music.library.queue = Default::default();
                        music.route = if home {
                            crate::app::Route::Home
                        } else {
                            crate::app::Route::Explore
                        };
                        music.page = crate::app::sync::tests::page(6, false);
                        music.page.shelves = vec![music.page.shelves[0].clone(); 8];
                        music.error = None;
                        music.notice = None;
                        music.rebuild_rows();
                        music.detail_list = crate::app::DetailList::new();
                        cx.notify();
                    })
                    .unwrap();
                wait_for_presented_frame(window, cx).await;
                if scrolled {
                    window
                        .update(cx, |music, _, cx| {
                            music.detail_list.state.scroll_to(gpui::ListOffset {
                                item_ix: 2,
                                offset_in_item: px(0.),
                            });
                            cx.notify();
                        })
                        .unwrap();
                    wait_for_presented_frame(window, cx).await;
                }
                let automatic = width < 1150.;
                for compact in [automatic, !automatic] {
                    if compact != automatic {
                        cx.update_window(window.into(), |_, window, cx| {
                            let position = point(px(36.), px(32.));
                            window.dispatch_event(
                                gpui::PlatformInput::MouseDown(gpui::MouseDownEvent {
                                    button: gpui::MouseButton::Left,
                                    position,
                                    modifiers: Default::default(),
                                    click_count: 1,
                                    first_mouse: false,
                                }),
                                cx,
                            );
                            window.dispatch_event(
                                gpui::PlatformInput::MouseUp(gpui::MouseUpEvent {
                                    button: gpui::MouseButton::Left,
                                    position,
                                    modifiers: Default::default(),
                                    click_count: 1,
                                }),
                                cx,
                            );
                        })
                        .unwrap();
                        wait_for_presented_frame(window, cx).await;
                    }
                    window
                        .update(cx, |music, window, _| {
                            assert_eq!(
                                music.sidebar_compact(width),
                                compact,
                                "Hamburger must override the automatic guide size"
                            );
                            assert_eq!(music.page_scrolled(), scrolled);
                            let name = format!(
                                "guide-{}-{}-{}-{}",
                                if compact { "mini" } else { "expanded" },
                                if home { "home" } else { "explore" },
                                if scrolled { "scroll" } else { "top" },
                                width as u32
                            );
                            let image = capture(window, &name);
                            let pixel = |x, y| *image.get_pixel(x, y);
                            let bg = image::Rgba([3, 3, 3, 255]);
                            let border = image::Rgba([41, 41, 41, 255]);
                            if compact || scrolled {
                                assert_eq!(
                                    pixel(8, 4),
                                    pixel(180, 4),
                                    "Header must not split at the mini guide: {name}"
                                );
                                assert_eq!(
                                    pixel(72, 4),
                                    pixel(300, 4),
                                    "No guide rule may cross the mini or scrolled header: {name}"
                                );
                            } else {
                                assert_eq!(pixel(238, 4), bg);
                                assert_eq!(
                                    pixel(239, 4),
                                    border,
                                    "Expanded rule starts at the top: {name}"
                                );
                            }
                            if scrolled {
                                if home {
                                    assert_eq!(
                                        pixel(width as u32 - 2, 200),
                                        bg,
                                        "The Home wash must scroll out with the feed"
                                    );
                                }
                                assert_eq!(pixel(8, 4), bg);
                                assert_eq!(
                                    pixel(300, 64),
                                    border,
                                    "Scrolled header has a bottom rule: {name}"
                                );
                                let edge = if compact { 72 } else { 240 };
                                assert_eq!(
                                    pixel(edge, 68),
                                    image::Rgba([28, 28, 28, 255]),
                                    "Scrolled guide has an outer 10% rule: {name}"
                                );
                                assert_eq!(pixel(edge, image.height() - 1), pixel(edge, 68));
                            } else if compact {
                                assert_eq!(
                                    pixel(71, 68),
                                    pixel(72, 68),
                                    "Unscrolled mini guide has no rule: {name}"
                                );
                                assert_eq!(pixel(72, 68), pixel(73, 68));
                            }
                            if !compact {
                                assert_eq!(pixel(239, 68), border);
                                assert_eq!(pixel(239, image.height() - 1), border);
                            }
                        })
                        .unwrap();
                }
            }
        }
    }
    println!(
        "GUIDE_CHROME home_explore=passed top_scrolled=passed widths=956,1920 hamburger=passed"
    );
}

async fn player_chrome(window: WindowHandle<Music>, cx: &mut AsyncApp) {
    use crate::app::{Route, Search};
    use crate::search::{SearchFilter, SearchScope};
    window
        .update(cx, |music, window, cx| {
            music.account = None;
            music.catalog_requests = crate::app::CatalogRequests::new();
            music.player = None;
            music.now_playing = None;
            music.autoplay_tracks.clear();
            music.duration = 337.8;
            music.position = 3.;
            music.playback = Playback::Loaded {
                paused: true,
                buffering: false,
            };
            music.error = None;
            music.notice = None;
            music.route = Route::Home;
            let mut track: crate::catalog::Track = serde_json::from_value(serde_json::json!({
                "id": "khnokW3Mw24",
                "title": "Instant Crush (feat. Julian Casablancas)",
                "artist": "Daft Punk & Julian Casablancas",
                "album": "Random Access Memories",
                "duration": 338
            }))
            .unwrap();
            track.details.year = Some("2013".into());
            music.library.queue.replace(vec![track.clone(); 12], 0);
            music.library.queue.playing_from = Some("Daft Punk".into());
            music.now_playing = Some(crate::watch::Watch {
                current: Some(track),
                current_index: Some(0),
                queue: Vec::new(),
                source: None,
                lyrics: None,
                related: None,
                alternatives: None,
                continuation: None,
                source_target: None,
            });
            music.select_player_tab(PlayerTab::UpNext, cx);
            music
                .player_page
                .queue_scroll
                .scroll_to_item_strict(0, gpui::ScrollStrategy::Top);
            window.focus(&music.focus, cx);
            cx.notify();
        })
        .unwrap();
    for width in [1280., 956.] {
        window
            .update(cx, |music, window, cx| {
                window.resize(size(px(width), px(800.)));
                music.sidebar_collapsed = Some(width < 1150.);
                music.player_open = true;
                music.player_page.hovered_row = None;
                cx.notify();
            })
            .unwrap();
        wait_for_presented_frame(window, cx).await;
        window
            .update(cx, |music, window, cx| {
                capture(window, &format!("chrome-queue-{width}"));
                let bounds = music.player_page.queue_bounds.get();
                let (left, top) = if width == 1280. {
                    (890., 277.)
                } else {
                    (593., 269.)
                };
                assert_eq!(
                    bounds.origin,
                    point(px(left), px(top)),
                    "Queue must align with the web's tabs and Autoplay header"
                );
                println!("PLAYER_CHROME width={width} queue={bounds:?}");
                music.player_page.hovered_row = Some(0);
                cx.notify();
            })
            .unwrap();
        wait_for_presented_frame(window, cx).await;
        window
            .update(cx, |_, window, _| {
                capture(window, &format!("chrome-queue-hover-{width}"));
            })
            .unwrap();
        if width == 956. {
            // Open the compact controls with a real pointer event, then dismiss with Escape.
            cx.update_window(window.into(), |_, window, cx| {
                let position = point(px(878.), px(764.));
                window.dispatch_event(
                    gpui::PlatformInput::MouseDown(gpui::MouseDownEvent {
                        button: gpui::MouseButton::Left,
                        position,
                        modifiers: Default::default(),
                        click_count: 1,
                        first_mouse: false,
                    }),
                    cx,
                );
                window.dispatch_event(
                    gpui::PlatformInput::MouseUp(gpui::MouseUpEvent {
                        button: gpui::MouseButton::Left,
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
                .update(cx, |music, window, cx| {
                    assert!(music.player_bar.read(cx).controls_expanded);
                    capture(window, "chrome-controls-expanded-956");
                })
                .unwrap();
            key(window, cx, "escape").await;
            window
                .update(cx, |music, _, cx| {
                    assert!(!music.player_bar.read(cx).controls_expanded)
                })
                .unwrap();
        }
    }
    for (width, compact, search) in [
        (1280., true, false),
        (956., true, false),
        (1280., false, true),
    ] {
        window
            .update(cx, |music, window, cx| {
                window.resize(size(px(width), px(800.)));
                music.player_open = false;
                music.sidebar_collapsed = Some(compact);
                music
                    .detail_list
                    .state
                    .scroll_to(gpui::ListOffset::default());
                music
                    .list_scroll
                    .0
                    .borrow()
                    .base_handle
                    .set_offset(point(px(0.), px(0.)));
                music.route = if search {
                    Route::Search(Search {
                        query: "daft punk".into(),
                        filter: SearchFilter::All,
                        scope: SearchScope::Catalog,
                    })
                } else {
                    Route::Home
                };
                window.focus(&music.focus, cx);
                cx.notify();
            })
            .unwrap();
        wait_for_presented_frame(window, cx).await;
        window
            .update(cx, |_, window, _| {
                let image = capture(window, &format!("chrome-header-{width}-{search}"));
                let left = if search {
                    325
                } else if width == 1280. {
                    172
                } else {
                    215
                };
                assert_ne!(
                    image.get_pixel(left, 32),
                    image.get_pixel(left - 1, 32),
                    "Search must start at the web's horizontal position"
                );
                assert_ne!(image.get_pixel(left, 32), image.get_pixel(left + 1, 32));
                assert_ne!(
                    image.get_pixel(left + 479, 32),
                    image.get_pixel(left + 480, 32),
                    "Search must retain its 480px width"
                );
            })
            .unwrap();
    }
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
                            point(px(2.), px(2.)),
                            size(px(850.), px(560.)),
                        ))),
                        titlebar: Some(TitlebarOptions {
                            title: Some("Music render isolation test".into()),
                            ..Default::default()
                        }),
                        kind: WindowKind::PopUp,
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
                            // These startup workers are replaced by synthetic state below.
                            // Close their subscriptions so late startup events cannot redraw
                            // the body during the position-only measurement.
                            music.player.as_ref().unwrap().events().close();
                            if let Some(service) = &music.mpris {
                                service.events().close();
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
                            // Playback opens the player page, so measure with it showing.
                            music.player_open = true;
                            music
                        })
                    },
                )
                .unwrap();
            let bounds_changes = Rc::new(Cell::new(0_u64));
            let observed_changes = bounds_changes.clone();
            let bounds_subscription = window
                .update(cx, |_, window, cx| {
                    cx.observe_window_bounds(window, move |_, window, _| {
                        observed_changes.set(observed_changes.get() + 1);
                        println!("MEASUREMENT_BOUNDS_CHANGED bounds={:?}", window.bounds());
                    })
                })
                .unwrap();
            cx.spawn(async move |cx| {
                let _bounds_subscription = bounds_subscription;
                // X11 ConfigureNotify can arrive after the first frame. It refreshes
                // every view, independently of playback samples or wheel events.
                let deadline = Instant::now() + Duration::from_secs(10);
                let (before, after) = loop {
                    let before = wait_for_quiescence(window, cx).await;
                    let bounds_before = bounds_changes.get();
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
                    wait_for_presented_frame(window, cx).await;
                    let after = window
                        .update(cx, |music, _, cx| render_counts(music, cx))
                        .unwrap();
                    if bounds_changes.get() == bounds_before {
                        break (before, after);
                    }
                    assert!(
                        Instant::now() < deadline,
                        "Window bounds kept changing during position measurement"
                    );
                    println!("POSITION_MEASUREMENT_RETRY window_bounds_changed=true");
                };
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
                wait_for_quiescence(window, cx).await;
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
                // Settle pointer hover before measuring stationary wheel input.
                let deadline = Instant::now() + Duration::from_secs(10);
                let (before_wheel, after_wheel) = loop {
                    cx.update_window(window.into(), |_, window, cx| {
                        window.dispatch_event(
                            gpui::PlatformInput::MouseMove(gpui::MouseMoveEvent {
                                position: point(window.viewport_size().width * 0.4, px(300.)),
                                pressed_button: None,
                                modifiers: Default::default(),
                            }),
                            cx,
                        );
                    })
                    .unwrap();
                    let before = wait_for_quiescence(window, cx).await;
                    let bounds_before = bounds_changes.get();
                    // A wheel tick over artwork has no scrolling work to do.
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
                    wait_for_presented_frame(window, cx).await;
                    let after = window
                        .update(cx, |music, _, cx| render_counts(music, cx))
                        .unwrap();
                    if bounds_changes.get() == bounds_before {
                        break (before, after);
                    }
                    assert!(
                        Instant::now() < deadline,
                        "Window bounds kept changing during wheel measurement"
                    );
                    println!("WHEEL_MEASUREMENT_RETRY window_bounds_changed=true");
                };
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
                scrolled_artist_survives_tab(window, cx, &bounds_changes).await;
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
                keyboard_survives_replacement_and_scrolls_lists(window, cx).await;
                library_sort_and_compact_layouts(window, cx).await;
                inline_links_follow_row_order(window, cx).await;
                home_chips_and_narrow_error(window, cx).await;
                search_song_rows(window, cx).await;
                guide_chrome(window, cx).await;
                player_chrome(window, cx).await;
                result.store(true, Ordering::SeqCst);
                cx.update(|cx| cx.quit());
            })
            .detach();
            cx.spawn(async move |cx| {
                cx.background_executor()
                    .timer(Duration::from_secs(150))
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
