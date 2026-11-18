//! Synthetic Suggestions reads and Add acknowledgments. No account writes leave the test.

use super::navigation::{check_cache_window, reply};
use crate::{
    account::{self, Effect, Mutation},
    app::{
        CatalogTarget, Job, ListRow, Music, Overlay, Query, Route, suggestions::SuggestionsResult,
    },
    catalog::{
        BrowseLink, Collection, Continuation, Item, Page, PlaylistSuggestion, PlaylistSuggestions,
        Shelf, ShelfLayout,
    },
};
use anyhow::Result;
use gpui::{
    AppContext, Context, ListOffset, MouseButton, MouseDownEvent, MouseUpEvent, PlatformInput,
    Window, point, px, size,
};
use serde_json::json;
use std::time::Instant;

fn playlist(view: &Music, rows: usize) -> Page {
    let mut page = crate::app::sync::tests::page(rows, false);
    page.title = "Suggestions fixture".into();
    page.header = crate::catalog::parse_page(
        &json!({
            "header":{"musicResponsiveHeaderRenderer":{
                "title":{"simpleText":"Suggestions fixture"},
                "secondSubtitle":{"simpleText":format!("{rows} tracks")}
            }},"contents":[]
        }),
        "Playlist",
    )
    .unwrap()
    .header;
    page.editable = true;
    page.header.as_mut().unwrap().suggestions = Some(token(view, "suggestions-seed"));
    page
}

fn token(view: &Music, value: &str) -> Continuation {
    Continuation::from_response(
        value,
        &json!({}),
        Some(view.account.as_ref().unwrap().session.clone()),
    )
    .unwrap()
}

fn batch(view: &Music, title: &str) -> PlaylistSuggestions {
    PlaylistSuggestions {
        tracks: crate::app::sync::tests::page(7, false)
            .tracks()
            .into_iter()
            .map(|mut track| {
                track.title = format!("{title} {}", track.id);
                track.details.set_video_id = None;
                PlaylistSuggestion {
                    track,
                    addable: true,
                }
            })
            .collect(),
        reload: Some(token(view, "suggestions-reload")),
        related: Some(Vec::new()),
        auto_reload_when_empty: true,
    }
}

fn deliver(
    view: &mut Music,
    job: Job,
    result: Result<PlaylistSuggestions, String>,
    window: &Window,
    cx: &mut Context<Music>,
) {
    assert!(matches!(job.query, Query::PlaylistSuggestions { .. }));
    view.suggestions_result(
        SuggestionsResult {
            job,
            result,
            session_rejected: false,
        },
        window,
        cx,
    );
}

#[test]
#[ignore = "requires desktop, private D-Bus and isolated runtime/data; synthetic accounts only"]
fn suggestions_refresh_keeps_playlist_rows_and_discards_late_visits() -> Result<()> {
    check_cache_window(async |window, cx| {
        window
            .update(cx, |view, window, cx| {
                view.navigate(
                    Route::Browse(BrowseLink::new("VLsuggestions", "Suggestions fixture")),
                    cx,
                );
                reply(view, playlist(view, 0), cx);
                let job = view.catalog_requests.pending.try_recv().unwrap();
                assert_eq!(
                    job.query.target() as usize,
                    CatalogTarget::Suggestions as usize
                );
                deliver(
                    view,
                    job,
                    Ok(batch(view, "Original suggestion")),
                    window,
                    cx,
                );
                assert!(view.rows.is_empty());
                assert!(view.page.tracks().is_empty());
                assert_eq!(
                    view.list_rows
                        .iter()
                        .filter(|row| matches!(row, ListRow::Suggestion(_)))
                        .count(),
                    7
                );
                assert!(
                    view.list_rows
                        .iter()
                        .any(|row| matches!(row, ListRow::EmptyPlaylist))
                );

                view.refresh_suggestions(cx);
                let job = view.catalog_requests.pending.try_recv().unwrap();
                deliver(view, job, Err("Suggestions read failed".into()), window, cx);
                assert_eq!(view.suggestions.as_ref().unwrap().tracks.len(), 7);
                assert_eq!(
                    view.suggestions.as_ref().unwrap().error.as_deref(),
                    Some("Suggestions read failed")
                );
                assert!(view.error.is_none());
                view.refresh_suggestions(cx);
                let job = view.catalog_requests.pending.try_recv().unwrap();
                deliver(
                    view,
                    job,
                    Ok(batch(view, "Replacement suggestion")),
                    window,
                    cx,
                );
                assert!(
                    view.suggestions.as_ref().unwrap().tracks[0]
                        .track
                        .title
                        .starts_with("Replacement")
                );
                assert!(view.suggestions.as_ref().unwrap().error.is_none());
                assert!(view.page.tracks().is_empty());
            })
            .unwrap();
        super::render::wait_for_presented_frame(window, cx).await;
        window
            .update(cx, |_, window, _| {
                super::render::capture(window, "suggestions-empty-wide")
            })
            .unwrap();
        window
            .update(cx, |view, window, _| {
                window.resize(size(px(850.), px(700.)));
                view.detail_list.state.scroll_to(ListOffset {
                    item_ix: 2,
                    offset_in_item: px(0.),
                });
            })
            .unwrap();
        super::render::wait_for_presented_frame(window, cx).await;
        window
            .update(cx, |_, window, _| {
                super::render::capture(window, "suggestions-empty-narrow")
            })
            .unwrap();
        window
            .update(cx, |view, window, cx| {
                let focus = view.suggestions.as_ref().unwrap().tracks[0].controls[1].clone();
                window.focus(&focus, cx);
            })
            .unwrap();
        for _ in 0..13 {
            super::render::key(window, cx, "tab").await;
        }
        window
            .update(cx, |view, window, _| {
                assert!(view.suggestions.as_ref().unwrap().refresh_focus.is_focused(window));
                let row = view.list_rows.iter().position(|row| matches!(row, ListRow::SuggestionsFooter)).unwrap()
                    + usize::from(view.detail_list.inline_header);
                let bounds = view.detail_list.state.bounds_for_item(row).unwrap();
                assert!(bounds.size.height > px(0.) && bounds.bottom() <= window.viewport_size().height,
                    "Tab must reveal the complete Refresh row, including the inline-header offset: {bounds:?}");
            })
            .unwrap();
        super::render::key(window, cx, "enter").await;
        let refresh = window.update(cx, |view, _, _| {
            let refresh = view.catalog_requests.pending.try_recv().unwrap();
            assert!(matches!(&refresh.query, Query::PlaylistSuggestions { token: Some(token), .. } if token.token() == "suggestions-reload"));
            refresh
        }).unwrap();
        super::render::key(window, cx, "shift-tab").await;
        super::render::key(window, cx, "tab").await;
        window
            .update(cx, |view, window, cx| {
                assert!(
                    !view
                        .suggestions
                        .as_ref()
                        .unwrap()
                        .refresh_focus
                        .is_focused(window),
                    "Loading Refresh must not be a Tab stop"
                );
                let mut smaller = batch(view, "Smaller batch");
                smaller.tracks.pop();
                deliver(view, refresh, Ok(smaller), window, cx);
                let focus = view.suggestions.as_ref().unwrap().refresh_focus.clone();
                window.focus(&focus, cx);
            })
            .unwrap();
        super::render::wait_for_presented_frame(window, cx).await;
        super::render::key(window, cx, "enter").await;
        window
            .update(cx, |view, window, cx| {
                let refresh = view.catalog_requests.pending.try_recv().unwrap();
                deliver(view, refresh, Ok(batch(view, "Larger batch")), window, cx);
            })
            .unwrap();
        super::render::wait_for_presented_frame(window, cx).await;
        super::render::key(window, cx, "enter").await;
        window
            .update(cx, |view, window, cx| {
                let old = view
                    .catalog_requests
                    .pending
                    .try_recv()
                    .expect("Enter after Refresh grows the batch must refresh again");
                assert!(matches!(&old.query, Query::PlaylistSuggestions { .. }));
                assert!(view.library.queue.tracks.is_empty());
                view.navigate(Route::Explore, cx);
                deliver(view, old, Ok(batch(view, "Late suggestion")), window, cx);
                assert!(view.suggestions.is_none());
                assert!(
                    !view
                        .list_rows
                        .iter()
                        .any(|row| matches!(row, ListRow::Suggestion(_)))
                );
            })
            .unwrap();
    })
}

#[test]
#[ignore = "requires desktop, private D-Bus and isolated runtime/data; synthetic accounts only"]
fn related_playlist_cards_open_the_rendered_collection() -> Result<()> {
    check_cache_window(async |window, cx| {
        for keyboard in [false, true] {
            window
                .update(cx, |view, window, cx| {
                    view.navigate(
                        Route::Browse(BrowseLink::new(
                            if keyboard {
                                "VLrelated-keyboard"
                            } else {
                                "VLrelated-pointer"
                            },
                            "Suggestions fixture",
                        )),
                        cx,
                    );
                    reply(view, playlist(view, 3), cx);
                    let read = view.catalog_requests.pending.try_recv().unwrap();
                    let mut suggestions = batch(view, "Suggestion");
                    suggestions.related = Some(vec![Shelf {
                        title: "Related playlists".into(),
                        layout: ShelfLayout::Cards,
                        more: None,
                        items: (0..if keyboard { 1 } else { 6 })
                            .map(|index| {
                                Item::Collection(Collection {
                                    id: if index == 0 {
                                        "VLPLrelated".into()
                                    } else {
                                        format!("VLPLrelated-{index}")
                                    },
                                    title: format!("Related fixture {index}"),
                                    subtitle: "Fixture owner".into(),
                                    thumbnail: None,
                                    details: Default::default(),
                                })
                            })
                            .collect(),
                    }]);
                    deliver(view, read, Ok(suggestions), window, cx);
                    assert_eq!(view.rows.len(), 3);
                    assert_eq!(view.page.tracks().len(), 3);
                    let row = view
                        .list_rows
                        .iter()
                        .position(|row| matches!(row, ListRow::Related(0)))
                        .unwrap();
                    view.detail_list.state.scroll_to(ListOffset {
                        item_ix: row + usize::from(view.detail_list.inline_header),
                        offset_in_item: px(0.),
                    });
                })
                .unwrap();
            super::render::wait_for_presented_frame(window, cx).await;
            if !keyboard {
                window
                    .update(cx, |view, window, cx| {
                        let carousel = &view.suggestions.as_ref().unwrap().related_scroll[&0];
                        let row =
                            view.list_rows.len() - 1 + usize::from(view.detail_list.inline_header);
                        let bounds = view.detail_list.state.bounds_for_item(row).unwrap();
                        assert!(carousel.bounds().size.width <= bounds.size.width + px(8.));
                        assert!(carousel.max_offset().x > px(40.));
                        carousel.set_offset(point(px(-40.), px(0.)));
                        view.refresh_suggestions(cx);
                        let read = view.catalog_requests.pending.try_recv().unwrap();
                        let mut refreshed = batch(view, "Refreshed suggestion");
                        refreshed.related = None;
                        deliver(view, read, Ok(refreshed), window, cx);
                    })
                    .unwrap();
                super::render::wait_for_presented_frame(window, cx).await;
                window
                    .update(cx, |view, _, cx| {
                        let carousel = &view.suggestions.as_ref().unwrap().related_scroll[&0];
                        assert_eq!(carousel.offset().x, px(-40.));
                        view.request(
                            Query::More {
                                token: token(view, "songs-continuation"),
                                search: None,
                            },
                            cx,
                        );
                        reply(
                            view,
                            crate::app::sync::tests::response(&[("", vec![3])], None, true),
                            cx,
                        );
                        assert_eq!(view.page.tracks().len(), 4);
                    })
                    .unwrap();
                super::render::wait_for_presented_frame(window, cx).await;
                window
                    .update(cx, |view, _, cx| {
                        let carousel = &view.suggestions.as_ref().unwrap().related_scroll[&0];
                        assert_eq!(carousel.offset().x, px(-40.));
                        carousel.set_offset(point(px(0.), px(0.)));
                        cx.notify();
                    })
                    .unwrap();
                super::render::wait_for_presented_frame(window, cx).await;
            }
            let target = window
                .update(cx, |view, _, _| {
                    let row = view
                        .list_rows
                        .iter()
                        .position(|row| matches!(row, ListRow::Related(0)))
                        .unwrap()
                        + usize::from(view.detail_list.inline_header);
                    let bounds = view.detail_list.state.bounds_for_item(row).unwrap();
                    point(bounds.left() + px(24.), bounds.top() + px(110.))
                })
                .unwrap();
            cx.update_window(window.into(), |_, window, cx| {
                window.dispatch_event(
                    PlatformInput::MouseDown(MouseDownEvent {
                        button: MouseButton::Right,
                        position: target,
                        modifiers: Default::default(),
                        click_count: 1,
                        first_mouse: false,
                    }),
                    cx,
                )
            })
            .unwrap();
            super::render::wait_for_presented_frame(window, cx).await;
            window.update(cx, |view, _, _| {
                assert!(matches!(&view.overlay, Some(Overlay::Collection(collection)) if collection.id == "VLPLrelated"));
                assert_eq!(view.page.tracks().len(), if keyboard { 3 } else { 4 });
            }).unwrap();
            super::render::key(window, cx, "escape").await;
            if keyboard {
                window
                    .update(cx, |view, window, cx| {
                        let focus = view.suggestions.as_ref().unwrap().refresh_focus.clone();
                        window.focus(&focus, cx);
                    })
                    .unwrap();
                super::render::key(window, cx, "tab").await;
                super::render::key(window, cx, "enter").await;
            } else {
                for event in [
                    PlatformInput::MouseDown(MouseDownEvent {
                        button: MouseButton::Left,
                        position: target,
                        modifiers: Default::default(),
                        click_count: 1,
                        first_mouse: false,
                    }),
                    PlatformInput::MouseUp(MouseUpEvent {
                        button: MouseButton::Left,
                        position: target,
                        modifiers: Default::default(),
                        click_count: 1,
                    }),
                ] {
                    cx.update_window(window.into(), |_, window, cx| {
                        window.dispatch_event(event, cx)
                    })
                    .unwrap();
                }
                super::render::wait_for_presented_frame(window, cx).await;
            }
            window
                .update(cx, |view, _, _| {
                    assert!(
                        matches!(&view.route, Route::Browse(link) if link.id == "VLPLrelated"),
                        "Related card must open on {}",
                        if keyboard { "Enter" } else { "click" }
                    );
                    let original = view.back.last().unwrap().page.as_ref().unwrap();
                    assert_eq!(original.tracks().len(), if keyboard { 3 } else { 4 });
                })
                .unwrap();
        }
    })
}

#[test]
#[ignore = "requires desktop, private D-Bus and isolated runtime/data; run in release mode for timings"]
fn suggestions_refresh_cost_with_loaded_playlist() -> Result<()> {
    check_cache_window(async |window, cx| {
        for rows in [500, 5000] {
            window
                .update(cx, |view, window, cx| {
                    view.navigate(
                        Route::Browse(BrowseLink::new(
                            format!("VLmeasure-{rows}"),
                            "Suggestions fixture",
                        )),
                        cx,
                    );
                    reply(view, playlist(view, rows), cx);
                    let read = view.catalog_requests.pending.try_recv().unwrap();
                    deliver(
                        view,
                        read,
                        Ok(batch(view, "Measured suggestion")),
                        window,
                        cx,
                    );
                    view.detail_list.state.scroll_to(ListOffset {
                        item_ix: rows / 2,
                        offset_in_item: px(0.),
                    });
                })
                .unwrap();
            super::render::wait_for_presented_frame(window, cx).await;
            for sample in 0..9 {
                let started = Instant::now();
                let (job, request_us) = window
                    .update(cx, |view, _, cx| {
                        let cpu = Instant::now();
                        view.refresh_suggestions(cx);
                        let elapsed = cpu.elapsed().as_micros();
                        (view.catalog_requests.pending.try_recv().unwrap(), elapsed)
                    })
                    .unwrap();
                super::render::wait_for_presented_frame(window, cx).await;
                let response_us = window
                    .update(cx, |view, window, cx| {
                        let response = batch(view, "Measured suggestion");
                        let cpu = Instant::now();
                        deliver(view, job, Ok(response), window, cx);
                        let elapsed = cpu.elapsed().as_micros();
                        assert_eq!(view.rows.len(), rows);
                        assert_eq!(
                            view.page
                                .shelves
                                .iter()
                                .map(|shelf| shelf.items.len())
                                .sum::<usize>(),
                            rows
                        );
                        elapsed
                    })
                    .unwrap();
                super::render::wait_for_presented_frame(window, cx).await;
                if sample > 0 {
                    println!(
                        "SUGGESTIONS_REFRESH_PERF {}",
                        json!({"rows":rows,"sample":sample,"request_us":request_us,"response_us":response_us,"two_frames_us":started.elapsed().as_micros()})
                    );
                }
            }
        }
    })
}

#[test]
#[ignore = "requires desktop, private D-Bus and isolated runtime/data; synthetic accounts only"]
fn suggestions_add_keeps_clicked_metadata_after_refresh_and_navigation() -> Result<()> {
    check_cache_window(async |window, cx| {
        let (pending, selected) = window
            .update(cx, |view, window, cx| {
                let (commands, pending) = async_channel::bounded(8);
                view.account_commands = commands;
                view.navigate(
                    Route::Browse(BrowseLink::new("VLsuggestions", "Suggestions fixture")),
                    cx,
                );
                reply(view, playlist(view, 0), cx);
                let read = view.catalog_requests.pending.try_recv().unwrap();
                deliver(
                    view,
                    read,
                    Ok(batch(view, "Clicked suggestion")),
                    window,
                    cx,
                );
                let selected = view.suggestions.as_ref().unwrap().tracks[0].track.clone();
                (pending, selected)
            })
            .unwrap();
        super::render::wait_for_presented_frame(window, cx).await;
        window
            .update(cx, |view, window, cx| {
                let focus = view.suggestions.as_ref().unwrap().tracks[0].controls[1].clone();
                window.focus(&focus, cx);
            })
            .unwrap();
        super::render::key(window, cx, "enter").await;
        super::render::key(window, cx, "tab").await;
        super::render::key(window, cx, "tab").await;
        window
            .update(cx, |view, window, _| {
                assert_eq!(
                    view.suggestions.as_ref().unwrap().focused_control(window),
                    Some(4),
                    "Tab skips Add controls while an edit is pending"
                );
            })
            .unwrap();
        window.update(cx, |view, window, cx| {
            let account::Command::Apply { session, mutation } = pending.try_recv().unwrap() else { panic!("Expected Add") };
            assert!(matches!(&*mutation, Mutation::AddToPlaylist { id, video_ids, .. } if id == "suggestions" && video_ids == std::slice::from_ref(&selected.id)));
            view.refresh_suggestions(cx);
            let read = view.catalog_requests.pending.try_recv().unwrap();
            deliver(view, read, Ok(batch(view, "New batch")), window, cx);
            view.navigate(Route::Explore, cx);
            view.account_event(account::Event::Applied {
                session,
                mutation,
                result: Ok(Effect::PlaylistAdded { additions: vec![(selected.id.clone(), "added-occurrence".into())], toast: None }),
                session_rejected: false,
            }, window, cx);
            let saved = view.back.iter().find_map(|state| {
                matches!(&state.route, Route::Browse(link) if link.id == "VLsuggestions").then_some(state.page.as_ref()).flatten()
            }).unwrap();
            let tracks = saved.tracks();
            assert_eq!(tracks.len(), 1);
            assert_eq!(tracks[0].title, selected.title);
            assert_eq!(tracks[0].artist, selected.artist);
            assert_eq!(tracks[0].details.set_video_id.as_deref(), Some("added-occurrence"));
            assert!(view.pending_mutations.is_empty());
            view.go_back(cx);
            assert_eq!(view.rows.len(), 1);
            view.detail_list.state.scroll_to(ListOffset { item_ix: 0, offset_in_item: px(0.) });
        }).unwrap();
    })
}

#[test]
#[ignore = "requires desktop, private D-Bus and isolated runtime/data; synthetic accounts only"]
fn confirming_suggestion_add_does_not_focus_a_different_song() -> Result<()> {
    check_cache_window(async |window, cx| {
        let (pending, selected) = window
            .update(cx, |view, window, cx| {
                let (commands, pending) = async_channel::bounded(8);
                view.account_commands = commands;
                view.navigate(
                    Route::Browse(BrowseLink::new("VLsuggestions", "Suggestions fixture")),
                    cx,
                );
                reply(view, playlist(view, 0), cx);
                let read = view.catalog_requests.pending.try_recv().unwrap();
                deliver(view, read, Ok(batch(view, "Suggestion")), window, cx);
                let selected = view.suggestions.as_ref().unwrap().tracks[0]
                    .track
                    .id
                    .clone();
                (pending, selected)
            })
            .unwrap();
        super::render::wait_for_presented_frame(window, cx).await;
        window
            .update(cx, |view, window, cx| {
                let focus = view.suggestions.as_ref().unwrap().tracks[0].controls[1].clone();
                window.focus(&focus, cx);
            })
            .unwrap();
        super::render::key(window, cx, "enter").await;
        window.update(cx, |view, window, cx| {
            let account::Command::Apply { session, mutation } = pending.try_recv().expect("Enter should add the focused suggestion") else {
                panic!("Expected Add to playlist");
            };
            assert!(matches!(&*mutation, Mutation::AddToPlaylist { video_ids, .. } if video_ids == std::slice::from_ref(&selected)));
            view.account_event(account::Event::Applied {
                session,
                mutation,
                result: Ok(Effect::PlaylistAdded { additions: vec![(selected.clone(), "added-occurrence".into())], toast: None }),
                session_rejected: false,
            }, window, cx);
            assert_ne!(view.suggestions.as_ref().unwrap().tracks[0].track.id, selected);
        }).unwrap();
        cx.update_window(window.into(), |_, window, cx| {
            let keystroke = gpui::Keystroke::parse("enter").unwrap();
            window.dispatch_keystroke(keystroke.clone(), cx);
            window.dispatch_event(PlatformInput::KeyUp(gpui::KeyUpEvent { keystroke }), cx);
        })
        .unwrap();
        assert!(
            pending.is_empty(),
            "Enter on the previous frame must not add again"
        );
        super::render::wait_for_presented_frame(window, cx).await;
        super::render::key(window, cx, "enter").await;
        assert!(
            pending.is_empty(),
            "Enter must not add the next song without moving focus"
        );
    })
}
