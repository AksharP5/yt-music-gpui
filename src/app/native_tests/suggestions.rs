//! Synthetic Suggestions reads and Add acknowledgments. No account writes leave the test.

use super::navigation::{check_cache_window, reply};
use crate::{
    account::{self, Effect, Mutation},
    app::{CatalogTarget, Job, ListRow, Music, Query, Route, suggestions::SuggestionsResult},
    catalog::{BrowseLink, Continuation, Page, PlaylistSuggestion, PlaylistSuggestions},
};
use anyhow::Result;
use gpui::{Context, ListOffset, px, size};
use serde_json::json;

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
    cx: &mut Context<Music>,
) {
    assert!(matches!(job.query, Query::PlaylistSuggestions { .. }));
    view.suggestions_result(
        SuggestionsResult {
            job,
            result,
            session_rejected: false,
        },
        cx,
    );
}

#[test]
#[ignore = "requires desktop, private D-Bus and isolated runtime/data; synthetic accounts only"]
fn suggestions_refresh_keeps_playlist_rows_and_discards_late_visits() -> Result<()> {
    check_cache_window(async |window, cx| {
        window
            .update(cx, |view, _, cx| {
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
                deliver(view, job, Ok(batch(view, "Original suggestion")), cx);
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
                deliver(view, job, Err("Suggestions read failed".into()), cx);
                assert_eq!(view.suggestions.as_ref().unwrap().tracks.len(), 7);
                assert_eq!(
                    view.suggestions.as_ref().unwrap().error.as_deref(),
                    Some("Suggestions read failed")
                );
                assert!(view.error.is_none());
                view.refresh_suggestions(cx);
                let job = view.catalog_requests.pending.try_recv().unwrap();
                deliver(view, job, Ok(batch(view, "Replacement suggestion")), cx);
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
                let focus = view.suggestions.as_mut().unwrap().focus.handle(1, cx);
                window.focus(&focus, cx);
            })
            .unwrap();
        for _ in 0..13 {
            super::render::key(window, cx, "tab").await;
        }
        window
            .update(cx, |view, window, _| {
                assert_eq!(
                    view.suggestions.as_ref().unwrap().focus.focused(window),
                    Some(14)
                );
            })
            .unwrap();
        super::render::key(window, cx, "enter").await;
        window.update(cx, |view, _, cx| {
            let refresh = view.catalog_requests.pending.try_recv().unwrap();
            assert!(matches!(&refresh.query, Query::PlaylistSuggestions { token: Some(token), .. } if token.token() == "suggestions-reload"));
            deliver(view, refresh, Ok(batch(view, "Keyboard refreshed")), cx);
        }).unwrap();
        window
            .update(cx, |view, _, cx| {
                view.refresh_suggestions(cx);
                let old = view.catalog_requests.pending.try_recv().unwrap();
                view.navigate(Route::Explore, cx);
                deliver(view, old, Ok(batch(view, "Late suggestion")), cx);
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
fn suggestions_add_keeps_clicked_metadata_after_refresh_and_navigation() -> Result<()> {
    check_cache_window(async |window, cx| {
        let (pending, selected) = window
            .update(cx, |view, _, cx| {
                let (commands, pending) = async_channel::bounded(8);
                view.account_commands = commands;
                view.navigate(
                    Route::Browse(BrowseLink::new("VLsuggestions", "Suggestions fixture")),
                    cx,
                );
                reply(view, playlist(view, 0), cx);
                let read = view.catalog_requests.pending.try_recv().unwrap();
                deliver(view, read, Ok(batch(view, "Clicked suggestion")), cx);
                let selected = view.suggestions.as_ref().unwrap().tracks[0].track.clone();
                (pending, selected)
            })
            .unwrap();
        super::render::wait_for_presented_frame(window, cx).await;
        window
            .update(cx, |view, window, cx| {
                let focus = view.suggestions.as_mut().unwrap().focus.handle(1, cx);
                window.focus(&focus, cx);
            })
            .unwrap();
        super::render::key(window, cx, "enter").await;
        window.update(cx, |view, window, cx| {
            let account::Command::Apply { session, mutation } = pending.try_recv().unwrap() else { panic!("Expected Add") };
            assert!(matches!(&*mutation, Mutation::AddToPlaylist { id, video_ids, .. } if id == "suggestions" && video_ids == std::slice::from_ref(&selected.id)));
            view.refresh_suggestions(cx);
            let read = view.catalog_requests.pending.try_recv().unwrap();
            deliver(view, read, Ok(batch(view, "New batch")), cx);
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
