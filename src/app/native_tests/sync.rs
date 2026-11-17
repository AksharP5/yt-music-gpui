//! Synthetic account UI checks. Never reads a saved account or sends network writes.
use crate::app::sync::tests::{page, response};
use crate::{
    account::{self, Connection, Effect, Mutation},
    app::{
        CatalogRequests, CatalogResult, CatalogTarget, Job, Music, Overlay, Query, Route, Startup,
    },
    assets::Assets,
    auth::{self, Session},
    catalog::{Account, BrowseLink, Page, Rating},
    track_state,
};
use anyhow::{Result, ensure};
use gpui::{
    AppContext, Bounds, Context, KeyDownEvent, Keystroke, ListOffset, Modifiers, ScrollStrategy,
    TitlebarOptions, WindowBounds, WindowKind, WindowOptions, point, px, size,
};
use serde_json::json;
use std::{
    borrow::Cow,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

fn playlist(rows: usize, continued: bool, stats: &str) -> Page {
    let mut page = page(rows, continued);
    page.header = crate::catalog::parse_page(
        &json!({"header":{"musicResponsiveHeaderRenderer":{
        "title":{"simpleText":"Sync validation playlist"}, "secondSubtitle":{"simpleText":stats},
        "facepile":{"avatarStackViewModel":{"text":{"content":"Fixture owner"}}}
    }},"contents":[]}),
        "Playlist",
    )
    .unwrap()
    .header;
    page
}

fn deliver(view: &mut Music, job: Job, result: Result<Page, String>, cx: &mut Context<Music>) {
    view.catalog_result(
        CatalogResult {
            id: job.id,
            account_revision: job.account_revision,
            account_generation: job.account_generation,
            history_generation: job.history_generation,
            append: matches!(job.query, Query::More { .. } | Query::SidebarMore(_)),
            revalidate: matches!(job.query, Query::Revalidate(_)),
            result,
            session_rejected: false,
            target: job.query.target(),
        },
        cx,
    );
}

fn reload_jobs(view: &mut Music, cx: &mut Context<Music>) -> (Job, Job) {
    view.reload_current_page(cx);
    let mut page = None;
    let mut guide = None;
    while let Ok(job) = view.catalog_requests.pending.try_recv() {
        match job.query.target() {
            CatalogTarget::Page => page = Some(job),
            CatalogTarget::Sidebar => guide = Some(job),
            _ => panic!("Reload must not submit a second metadata read"),
        }
    }
    (
        page.expect("Reload requests the page"),
        guide.expect("Reload requests the guide"),
    )
}

fn guide(id: &str, stats: &str) -> Page {
    let mut page = page(0, false);
    page.shelves[0].items.push(crate::catalog::Item::Collection(
        serde_json::from_value(json!({"id":format!("VL{id}"),"title":id,"subtitle":stats}))
            .unwrap(),
    ));
    page
}

#[test]
#[ignore = "requires desktop, private D-Bus and isolated runtime/data; synthetic accounts only"]
fn playlist_reload_cannot_restore_confirmed_removed_rows() -> Result<()> {
    super::navigation::check_cache_window(async |window, cx| {
        window
            .update(cx, |view, window, cx| {
                for (id, metadata_first) in [("page-first", false), ("metadata-first", true)] {
                    view.navigate(Route::Browse(BrowseLink::new(format!("VL{id}"), id)), cx);
                    super::navigation::reply(view, playlist(1, false, "1 track"), cx);
                    super::navigation::confirm(
                        view,
                        window,
                        Mutation::RemoveFromPlaylist {
                            id: id.into(),
                            video_id: "00000000000".into(),
                            set_video_id: "occurrence-0".into(),
                        },
                        cx,
                    );
                    let mut metadata = Some(view.catalog_requests.pending.try_recv().unwrap());
                    let (page_read, guide_read) = reload_jobs(view, cx);
                    if metadata_first {
                        deliver(
                            view,
                            metadata.take().unwrap(),
                            Ok(playlist(0, false, "0 tracks")),
                            cx,
                        );
                    }
                    deliver(view, page_read, Ok(playlist(1, false, "1 track")), cx);
                    assert!(
                        view.rows.is_empty(),
                        "A lagging Reload must not undo the confirmed removal: {id}"
                    );
                    if let Some(metadata) = metadata {
                        deliver(view, metadata, Ok(playlist(0, false, "0 tracks")), cx);
                    }
                    deliver(view, guide_read, Ok(guide(id, "1 track")), cx);
                    assert!(view.rows.is_empty());
                    assert_eq!(view.page.header.as_ref().unwrap().stats, "0 tracks");
                    assert_eq!(view.sidebar_playlists[0].subtitle, "0 tracks");
                }
            })
            .unwrap();
        super::render::wait_for_presented_frame(window, cx).await;
        window
            .update(cx, |_, window, _| {
                super::render::capture(window, "playlist-empty-after-stale-reload")
            })
            .unwrap();
    })
}

#[test]
#[ignore = "requires desktop, private D-Bus and isolated runtime/data; synthetic accounts only"]
fn playlist_readback_failure_keeps_edits_behind_a_page_error() -> Result<()> {
    super::navigation::check_cache_window(async |window, cx| {
        window
            .update(cx, |view, window, cx| {
                view.navigate(Route::Browse(BrowseLink::new("VLalpha", "Alpha")), cx);
                super::navigation::reply(view, playlist(1, false, "1 track"), cx);
                super::navigation::confirm(
                    view,
                    window,
                    Mutation::RemoveFromPlaylist {
                        id: "alpha".into(),
                        video_id: "00000000000".into(),
                        set_video_id: "occurrence-0".into(),
                    },
                    cx,
                );
                let metadata = view.catalog_requests.pending.try_recv().unwrap();
                let (page_read, guide_read) = reload_jobs(view, cx);
                deliver(view, page_read, Err("Page failed".into()), cx);
                deliver(view, metadata, Err("Readback failed".into()), cx);
                deliver(view, guide_read, Ok(guide("alpha", "1 track")), cx);
                assert_eq!(view.error.as_deref(), Some("Page failed"));
                let (page_read, guide_read) = reload_jobs(view, cx);
                deliver(view, page_read, Ok(playlist(1, false, "1 track")), cx);
                deliver(view, guide_read, Ok(guide("alpha", "1 track")), cx);
                assert!(
                    view.rows.is_empty(),
                    "A different displayed error must not discard the confirmed removal"
                );
                assert!(view.forward_page.as_ref().unwrap().fetched_at.is_none());
                let (page_read, guide_read) = reload_jobs(view, cx);
                deliver(view, page_read, Ok(playlist(0, false, "0 tracks")), cx);
                deliver(view, guide_read, Ok(guide("alpha", "1 track")), cx);
                assert_eq!(view.sidebar_playlists[0].subtitle, "0 tracks");
                assert!(view.error.is_none());

                let (sender, commands) = async_channel::bounded(4);
                view.account_commands = sender;
                view.navigate(Route::Browse(BrowseLink::new("VLbeta", "Beta")), cx);
                super::navigation::reply(view, playlist(2, false, "2 tracks"), cx);
                super::navigation::confirm(
                    view,
                    window,
                    Mutation::RemoveFromPlaylist {
                        id: "beta".into(),
                        video_id: "00000000000".into(),
                        set_video_id: "occurrence-0".into(),
                    },
                    cx,
                );
                let metadata = view.catalog_requests.pending.try_recv().unwrap();
                let removal = Mutation::RemoveFromPlaylist {
                    id: "beta".into(),
                    video_id: "00000000001".into(),
                    set_video_id: "occurrence-1".into(),
                };
                assert!(view.mutate(removal.clone(), cx));
                assert!(matches!(
                    commands.try_recv().unwrap(),
                    account::Command::Apply { .. }
                ));
                deliver(view, metadata, Err("Same read failure".into()), cx);
                view.revalidate_page(cx);
                let page_read = view.catalog_requests.pending.try_recv().unwrap();
                deliver(view, page_read, Err("Same read failure".into()), cx);
                super::navigation::confirm(view, window, removal, cx);
                let metadata = view.catalog_requests.pending.try_recv().unwrap();
                deliver(view, metadata, Ok(playlist(0, false, "0 tracks")), cx);
                assert_eq!(
                    view.error.as_deref(),
                    Some("Same read failure"),
                    "A completed readback cannot hide a page failure with identical text"
                );
                assert_eq!(view.sync.page_error.as_deref(), Some("Same read failure"));
            })
            .unwrap();
    })
}

#[test]
#[ignore = "requires desktop, private D-Bus and isolated runtime/data; synthetic accounts only"]
fn playlist_edits_coalesce_while_one_readback_is_loading() -> Result<()> {
    super::navigation::check_cache_window(async |window, cx| {
        window.update(cx, |view, window, cx| {
            view.navigate(Route::Browse(BrowseLink::new("VLalpha", "Alpha")), cx);
            super::navigation::reply(view, playlist(4, false, "4 tracks"), cx);
            super::navigation::confirm(view, window, Mutation::RemoveFromPlaylist {
                id:"alpha".into(), video_id:"00000000000".into(), set_video_id:"occurrence-0".into(),
            }, cx);
            let initial = view.catalog_requests.pending.try_recv().unwrap();
            for index in [1,2] {
                super::navigation::confirm(view, window, Mutation::RemoveFromPlaylist {
                    id:"alpha".into(), video_id:format!("{index:011}"), set_video_id:format!("occurrence-{index}"),
                }, cx);
            }
            assert!(view.catalog_requests.pending.is_empty(), "Rapid acknowledgements must not start overlapping readbacks");
            view.revalidate_page(cx);
            super::navigation::reply(view, playlist(4, false, "4 tracks"), cx);
            assert_eq!(view.rows.len(), 1, "Foreground page replies retain all three confirmed removals");
            deliver(view, initial, Err("Earlier read failed".into()), cx);
            assert!(view.error.is_none(), "An old read cannot report failure for a newer set of edits");
            let latest = view.catalog_requests.pending.try_recv().unwrap();
            assert!(matches!(&latest.query, Query::PlaylistMetadata(refresh) if refresh.removals.len()==3));
            assert!(view.catalog_requests.pending.is_empty(), "Only one successor is submitted");
            let mut fresh = playlist(4, false, "1 track");
            fresh.shelves[0].items.drain(..3);
            deliver(view, latest, Ok(fresh), cx);
            assert_eq!(view.rows.len(), 1);
            assert_eq!(view.rows[0].id(), "00000000003");
        }).unwrap();
    })
}

#[test]
#[ignore = "requires desktop, private D-Bus and isolated runtime/data; synthetic accounts only"]
fn playlist_reload_retains_a_move_until_its_order_is_visible() -> Result<()> {
    super::navigation::check_cache_window(async |window, cx| {
        window
            .update(cx, |view, window, cx| {
                view.navigate(Route::Browse(BrowseLink::new("VLalpha", "Alpha")), cx);
                super::navigation::reply(view, playlist(3, false, "3 tracks"), cx);
                super::navigation::confirm(
                    view,
                    window,
                    Mutation::MoveInPlaylist {
                        id: "alpha".into(),
                        set_video_id: "occurrence-2".into(),
                        before: Some("occurrence-0".into()),
                    },
                    cx,
                );
                let metadata = view.catalog_requests.pending.try_recv().unwrap();
                let (page_read, guide_read) = reload_jobs(view, cx);
                deliver(view, page_read, Ok(playlist(3, false, "3 tracks")), cx);
                assert!(
                    view.catalog_requests
                        .is_loading(CatalogTarget::PlaylistMetadata),
                    "A visibly old order cannot cancel the move readback"
                );
                assert_eq!(
                    view.rows[0].id(),
                    "00000000002",
                    "Reload must retain the acknowledged order"
                );
                let mut fresh = playlist(3, false, "3 tracks");
                fresh.shelves[0].items.rotate_right(1);
                deliver(view, metadata, Ok(fresh), cx);
                deliver(view, guide_read, Ok(guide("alpha", "3 tracks")), cx);
                assert_eq!(view.rows[0].id(), "00000000002");
                assert!(view.error.is_none());

                view.navigate(Route::Browse(BrowseLink::new("VLbeta", "Beta")), cx);
                let mut initial = playlist(3, false, "3 tracks");
                initial.shelves[0].items.rotate_right(1);
                super::navigation::reply(view, initial.clone(), cx);
                super::navigation::confirm(view, window, Mutation::MoveInPlaylist {
                    id:"beta".into(), set_video_id:"occurrence-0".into(), before:Some("occurrence-1".into()),
                }, cx);
                let initial_read = view.catalog_requests.pending.try_recv().unwrap();
                super::navigation::confirm(view, window, Mutation::MoveInPlaylist {
                    id:"beta".into(), set_video_id:"occurrence-1".into(), before:Some("occurrence-2".into()),
                }, cx);
                assert!(view.catalog_requests.pending.is_empty());
                let track = view.page.tracks()[0].clone();
                super::navigation::confirm(view, window, Mutation::SongLibrary { track, save:true }, cx);
                view.account_event(account::Event::Applied {
                    session:view.account.as_ref().unwrap().session.clone(),
                    mutation:Box::new(Mutation::AddToPlaylist { id:"beta".into(), video_ids:vec!["00000000000".into()], duplicates:true }),
                    result:Ok(Effect::PlaylistAdded { additions:vec![("00000000000".into(),"coalesced-append".into())], toast:None }), session_rejected:false,
                }, window, cx);
                let (page_read, guide_read) = reload_jobs(view, cx);
                deliver(view, page_read, Ok(initial.clone()), cx);
                assert_eq!(view.page.tracks()[0].details.in_library, Some(true), "Retaining order must preserve a newer library edit");
                assert_eq!(view.rows.iter().map(|row| row.id()).collect::<Vec<_>>(), ["00000000001","00000000002","00000000000","00000000000"], "A retained order includes a newer acknowledged append");
                deliver(view, initial_read, Ok(initial), cx);
                let latest = view.catalog_requests.pending.try_recv().unwrap();
                let mut fresh = playlist(3, false, "4 tracks");
                fresh.shelves[0].items.rotate_left(1);
                fresh.shelves[0].items.push(crate::catalog::Item::Track(view.page.tracks().last().unwrap().clone()));
                deliver(view, latest, Ok(fresh), cx);
                deliver(view, guide_read, Ok(guide("beta", "3 tracks")), cx);
                assert_eq!(view.rows.iter().map(|row| row.id()).collect::<Vec<_>>(), ["00000000001","00000000002","00000000000","00000000000"], "Coalesced moves retain their final order rather than replaying intermediate commands");
            })
            .unwrap();
    })
}

#[test]
#[ignore = "requires desktop, private D-Bus and isolated runtime/data; synthetic accounts only"]
fn playlist_revalidation_keeps_pages_loaded_after_an_edit() -> Result<()> {
    super::navigation::check_cache_window(async |window, cx| {
        for failed in [false, true] {
            let id = if failed { "failed" } else { "pending" };
            let metadata = window
                .update(cx, |view, window, cx| {
                    view.navigate(Route::Browse(BrowseLink::new(format!("VL{id}"), id)), cx);
                    super::navigation::reply(view, playlist(50, true, "150 tracks"), cx);
                    view.prefetch_page(50, 50, 0, cx);
                    super::navigation::reply(
                        view,
                        response(&[("Songs", (50..100).collect())], Some("page-three"), true),
                        cx,
                    );
                    super::navigation::confirm(
                        view,
                        window,
                        Mutation::MoveInPlaylist {
                            id: id.into(),
                            set_video_id: "occurrence-75".into(),
                            before: Some("occurrence-60".into()),
                        },
                        cx,
                    );
                    let mut metadata = Some(view.catalog_requests.pending.try_recv().unwrap());
                    if failed {
                        deliver(
                            view,
                            metadata.take().unwrap(),
                            Err("Readback failed; refresh the playlist".into()),
                            cx,
                        );
                    }
                    view.prefetch_page(100, 100, 0, cx);
                    super::navigation::reply(
                        view,
                        response(&[("Songs", (100..150).collect())], None, true),
                        cx,
                    );
                    assert_eq!(view.rows.len(), 150);
                    view.detail_list.state.scroll_to(ListOffset {
                        item_ix: 121,
                        offset_in_item: px(8.),
                    });
                    cx.notify();
                    metadata
                })
                .unwrap();
            super::render::wait_for_presented_frame(window, cx).await;
            let before = window
                .update(cx, |view, window, cx| {
                    let before = view.detail_list.state.logical_scroll_top();
                    assert_eq!(before.item_ix, 121);
                    super::render::capture(window, &format!("playlist-loaded-{id}-before"));
                    view.revalidate_page(cx);
                    let first_page = view.catalog_requests.pending.try_recv().unwrap();
                    deliver(view, first_page, Ok(playlist(50, true, "150 tracks")), cx);
                    assert_eq!(
                        view.rows.len(),
                        150,
                        "Revalidation must keep the page loaded after the edit: {id}"
                    );
                    assert_eq!(view.rows[120].id(), "00000000120");
                    assert!(view.page.continuation.is_none());
                    if let Some(metadata) = metadata {
                        deliver(
                            view,
                            metadata,
                            Err("Readback failed; refresh the playlist".into()),
                            cx,
                        );
                    }
                    before
                })
                .unwrap();
            super::render::wait_for_quiescence(window, cx).await;
            window
                .update(cx, |view, window, _| {
                    let after = view.detail_list.state.logical_scroll_top();
                    assert_eq!(
                        after.item_ix, before.item_ix,
                        "The loaded-page scroll anchor must survive revalidation"
                    );
                    assert_eq!(after.offset_in_item, before.offset_in_item);
                    super::render::capture(window, &format!("playlist-loaded-{id}-after"));
                })
                .unwrap();
        }
    })
}

#[test]
#[ignore = "requires desktop, private D-Bus and isolated runtime/data; synthetic accounts only"]
fn playlist_move_across_a_page_boundary_keeps_the_displaced_song() -> Result<()> {
    super::navigation::check_cache_window(async |window, cx| {
        window
            .update(cx, |view, window, cx| {
                view.navigate(Route::Browse(BrowseLink::new("VLalpha", "Alpha")), cx);
                super::navigation::reply(view, playlist(50, true, "150 tracks"), cx);
                view.prefetch_page(50, 50, 0, cx);
                super::navigation::reply(
                    view,
                    response(&[("Songs", (50..100).collect())], Some("page-three"), true),
                    cx,
                );
                super::navigation::confirm(
                    view,
                    window,
                    Mutation::MoveInPlaylist {
                        id: "alpha".into(),
                        set_video_id: "occurrence-50".into(),
                        before: Some("occurrence-49".into()),
                    },
                    cx,
                );
                let metadata = view.catalog_requests.pending.try_recv().unwrap();
                let mut fresh = playlist(49, true, "150 tracks");
                let moved = view
                    .page
                    .tracks()
                    .into_iter()
                    .find(|track| track.details.set_video_id.as_deref() == Some("occurrence-50"))
                    .unwrap();
                fresh.shelves[0]
                    .items
                    .push(crate::catalog::Item::Track(moved.clone()));
                deliver(view, metadata, Ok(fresh.clone()), cx);
                view.revalidate_page(cx);
                let first_page = view.catalog_requests.pending.try_recv().unwrap();
                deliver(view, first_page, Ok(fresh), cx);
                assert_eq!(
                    view.rows.len(),
                    100,
                    "A first-page refresh must keep the displaced last song of that page"
                );
                assert_eq!(view.rows[49].id(), "00000000050");
                assert_eq!(view.rows[50].id(), "00000000049");
            })
            .unwrap();
    })
}

#[test]
#[ignore = "requires desktop, private D-Bus and isolated runtime/data; synthetic accounts only"]
fn playlist_move_revalidation_keeps_loaded_page_boundaries() -> Result<()> {
    super::navigation::check_cache_window(async |window, cx| {
        window
            .update(cx, |view, window, cx| {
                view.navigate(Route::Browse(BrowseLink::new("VLalpha", "Alpha")), cx);
                super::navigation::reply(view, playlist(50, true, "150 tracks"), cx);
                view.prefetch_page(50, 50, 0, cx);
                super::navigation::reply(
                    view,
                    response(&[("Songs", (50..100).collect())], Some("page-three"), true),
                    cx,
                );
                assert_eq!(view.rows.len(), 100);
                super::navigation::confirm(
                    view,
                    window,
                    Mutation::MoveInPlaylist {
                        id: "alpha".into(),
                        set_video_id: "occurrence-1".into(),
                        before: Some("occurrence-0".into()),
                    },
                    cx,
                );
                let metadata = view.catalog_requests.pending.try_recv().unwrap();
                view.revalidate_page(cx);
                let stale = view.catalog_requests.pending.try_recv().unwrap();
                deliver(view, stale, Ok(playlist(50, true, "150 tracks")), cx);
                assert_eq!(view.rows.len(), 100);
                let mut fresh = playlist(50, true, "150 tracks");
                fresh.shelves[0].items.swap(0, 1);
                deliver(view, metadata, Ok(fresh.clone()), cx);
                view.revalidate_page(cx);
                let latest = view.catalog_requests.pending.try_recv().unwrap();
                deliver(view, latest, Ok(fresh), cx);
                assert_eq!(
                    view.rows.len(),
                    100,
                    "Revalidating the first page must keep the second loaded page"
                );
                assert_eq!(view.rows[50].id(), "00000000050");
                assert!(view.page.continuation.is_some());
            })
            .unwrap();
    })
}

#[test]
#[ignore = "requires desktop, private D-Bus and isolated runtime/data; synthetic accounts only"]
fn playlist_reload_cannot_settle_a_move_beyond_its_first_page() -> Result<()> {
    super::navigation::check_cache_window(async |window, cx| {
        window
            .update(cx, |view, window, cx| {
                view.navigate(Route::Browse(BrowseLink::new("VLalpha", "Alpha")), cx);
                super::navigation::reply(view, playlist(50, true, "150 tracks"), cx);
                view.prefetch_page(50, 50, 0, cx);
                super::navigation::reply(
                    view,
                    response(&[("Songs", (50..100).collect())], Some("page-three"), true),
                    cx,
                );
                view.detail_list.state.scroll_to(ListOffset {
                    item_ix: 76,
                    offset_in_item: px(8.),
                });
                super::navigation::confirm(
                    view,
                    window,
                    Mutation::MoveInPlaylist {
                        id: "alpha".into(),
                        set_video_id: "occurrence-75".into(),
                        before: Some("occurrence-60".into()),
                    },
                    cx,
                );
                let _metadata = view.catalog_requests.pending.try_recv().unwrap();
                let (page_read, guide_read) = reload_jobs(view, cx);
                deliver(view, page_read, Ok(playlist(50, true, "150 tracks")), cx);
                assert!(
                    view.catalog_requests
                        .is_loading(CatalogTarget::PlaylistMetadata),
                    "The unchanged first page cannot settle a move on the second page"
                );
                assert_eq!(
                    view.rows.len(),
                    100,
                    "A short reply must retain the loaded order that includes the acknowledged move"
                );
                let ids: Vec<_> = view.rows.iter().map(|row| row.id()).collect();
                assert!(
                    ids.iter().position(|id| *id == "00000000075")
                        < ids.iter().position(|id| *id == "00000000060")
                );
                deliver(view, guide_read, Ok(guide("alpha", "150 tracks")), cx);
            })
            .unwrap();
    })
}

#[test]
#[ignore = "requires desktop, private D-Bus and isolated runtime/data; synthetic accounts only"]
fn playlist_reload_keeps_a_known_added_row_when_readback_fails() -> Result<()> {
    super::navigation::check_cache_window(async |window, cx| {
        window
            .update(cx, |view, window, cx| {
                for partial in [false, true] {
                    let id = if partial { "beta" } else { "alpha" };
                    view.navigate(Route::Browse(BrowseLink::new(format!("VL{id}"), id)), cx);
                    let original = if partial { 100 } else { 1 };
                    if partial {
                        super::navigation::reply(view, playlist(50, true, "100 tracks"), cx);
                        view.prefetch_page(50, 50, 0, cx);
                        super::navigation::reply(
                            view,
                            response(&[("Songs", (50..100).collect())], None, true),
                            cx,
                        );
                    } else {
                        super::navigation::reply(view, playlist(1, false, "1 track"), cx);
                    }
                    view.account_event(
                        account::Event::Applied {
                            session: view.account.as_ref().unwrap().session.clone(),
                            mutation: Box::new(Mutation::AddToPlaylist {
                                id: id.into(),
                                video_ids: vec!["00000000000".into()],
                                duplicates: true,
                            }),
                            result: Ok(Effect::PlaylistAdded {
                                additions: vec![(
                                    "00000000000".into(),
                                    "acknowledged-append".into(),
                                )],
                                toast: None,
                            }),
                            session_rejected: false,
                        },
                        window,
                        cx,
                    );
                    let metadata = view.catalog_requests.pending.try_recv().unwrap();
                    assert_eq!(view.rows.len(), original + 1);
                    let (page_read, guide_read) = reload_jobs(view, cx);
                    let stale = if partial {
                        playlist(50, true, "100 tracks")
                    } else {
                        playlist(1, false, "1 track")
                    };
                    deliver(view, page_read, Ok(stale), cx);
                    assert_eq!(
                        view.rows.len(),
                        original + 1,
                        "A stale Reload must retain the acknowledged added row, partial={partial}"
                    );
                    deliver(
                        view,
                        metadata,
                        Err("Readback failed; refresh the playlist".into()),
                        cx,
                    );
                    deliver(view, guide_read, Ok(guide(id, "1 track")), cx);
                    assert_eq!(view.rows.len(), original + 1);
                    assert_eq!(
                        view.page
                            .tracks()
                            .last()
                            .unwrap()
                            .details
                            .set_video_id
                            .as_deref(),
                        Some("acknowledged-append")
                    );
                    assert!(view.forward_page.as_ref().unwrap().fetched_at.is_none());
                }
            })
            .unwrap();
    })
}

#[test]
#[ignore = "requires desktop, private D-Bus and isolated runtime/data; synthetic accounts only"]
fn failed_move_readback_can_adopt_a_later_server_order_on_reload() -> Result<()> {
    super::navigation::check_cache_window(async |window, cx| {
        window
            .update(cx, |view, window, cx| {
                view.navigate(Route::Browse(BrowseLink::new("VLalpha", "Alpha")), cx);
                super::navigation::reply(view, playlist(3, false, "3 tracks"), cx);
                super::navigation::confirm(
                    view,
                    window,
                    Mutation::MoveInPlaylist {
                        id: "alpha".into(),
                        set_video_id: "occurrence-2".into(),
                        before: Some("occurrence-0".into()),
                    },
                    cx,
                );
                let metadata = view.catalog_requests.pending.try_recv().unwrap();
                deliver(
                    view,
                    metadata,
                    Err("Readback failed; refresh the playlist".into()),
                    cx,
                );
                assert_eq!(view.rows[0].id(), "00000000002");
                let (page_read, guide_read) = reload_jobs(view, cx);
                // Another client removed the moved song after the failed readback.
                deliver(view, page_read, Ok(playlist(2, false, "2 tracks")), cx);
                deliver(view, guide_read, Ok(guide("alpha", "3 tracks")), cx);
                assert!(
                    view.error.is_none(),
                    "An explicit Reload must recover from a failed move readback"
                );
                assert_eq!(
                    view.rows.iter().map(|row| row.id()).collect::<Vec<_>>(),
                    ["00000000000", "00000000001"],
                    "Reload must adopt the server order instead of restoring the removed song"
                );
                assert_eq!(view.page.header.as_ref().unwrap().stats, "2 tracks");
                assert_eq!(view.sidebar_playlists[0].subtitle, "2 tracks");
                assert!(view.sync.playlist_failures.is_empty());
                assert!(
                    view.forward_page.as_ref().unwrap().fetched_at.is_none(),
                    "A recovery without matching order evidence stays out of the forward cache"
                );
            })
            .unwrap();
        super::render::wait_for_presented_frame(window, cx).await;
        window
            .update(cx, |_, window, _| {
                super::render::capture(window, "playlist-failed-move-reloaded")
            })
            .unwrap();
    })
}

#[test]
#[ignore = "requires desktop, private D-Bus and isolated runtime/data; synthetic accounts only"]
fn failed_long_playlist_readback_can_recover_through_reload() -> Result<()> {
    super::navigation::check_cache_window(async |window, cx| {
        window.update(cx, |view, window, cx| {
            for id in ["removal", "append"] {
                view.navigate(Route::Browse(BrowseLink::new(format!("VL{id}"), id)), cx);
                super::navigation::reply(view, playlist(50, true, "500 tracks"), cx);
                if id=="removal" {
                    super::navigation::confirm(view, window, Mutation::RemoveFromPlaylist {
                        id:id.into(), video_id:"00000000000".into(), set_video_id:"occurrence-0".into(),
                    }, cx);
                } else {
                    view.account_event(account::Event::Applied {
                        session:view.account.as_ref().unwrap().session.clone(),
                        mutation:Box::new(Mutation::AddToPlaylist { id:id.into(), video_ids:vec!["00000000000".into()], duplicates:true }),
                        result:Ok(Effect::PlaylistAdded { additions:vec![("00000000000".into(),"tail-append".into())], toast:None }), session_rejected:false,
                    }, window, cx);
                }
                let metadata = view.catalog_requests.pending.try_recv().unwrap();
                deliver(view, metadata, Err("Readback failed; refresh the playlist".into()), cx);
                let (page_read, guide_read) = reload_jobs(view, cx);
                let count = if id=="removal" { "499 tracks" } else { "501 tracks" };
                let mut fresh = playlist(51, true, count);
                if id=="removal" { fresh.shelves[0].items.remove(0); }
                else { fresh.shelves[0].items.pop(); }
                deliver(view, page_read, Ok(fresh), cx);
                deliver(view, guide_read, Ok(guide(id, "500 tracks")), cx);
                assert!(view.error.is_none(), "A failed long-playlist readback must recover through a post-acknowledgement Reload: {id}");
                assert_eq!(view.page.header.as_ref().unwrap().stats, count);
                assert_eq!(view.sidebar_playlists[0].subtitle, count);
                assert!(view.catalog_requests.pending.is_empty(), "Recovery does not scan the whole playlist");
                assert!(view.forward_page.as_ref().unwrap().fetched_at.is_none(), "Partial recovery cannot certify all rows for caching");
            }
        }).unwrap();
    })
}

#[test]
#[ignore = "requires desktop, private D-Bus and isolated runtime/data; synthetic accounts only"]
fn playlist_metadata_cannot_make_an_older_page_cacheable() -> Result<()> {
    super::navigation::check_cache_window(async |window, cx| {
        window.update(cx, |view, window, cx| {
            view.navigate(Route::Explore, cx);
            super::navigation::reply(view, page(1, false), cx);
            let track = view.page.tracks()[0].clone();
            view.account_event(account::Event::Applied {
                session:view.account.as_ref().unwrap().session.clone(),
                mutation:Box::new(Mutation::AddToPlaylist { id:"alpha".into(), video_ids:vec![track.id.clone()], duplicates:false }),
                result:Ok(Effect::PlaylistAdded { additions:vec![(track.id,"added-occurrence".into())], toast:None }), session_rejected:false,
            }, window, cx);
            let metadata = view.catalog_requests.pending.try_recv().unwrap();
            assert!(metadata.query.target() == CatalogTarget::PlaylistMetadata);
            view.navigate(Route::Browse(BrowseLink::new("VLalpha", "Alpha")), cx);
            let mut fresh = playlist(1, false, "1 track");
            let crate::catalog::Item::Track(added) = &mut fresh.shelves[0].items[0] else { panic!() };
            added.details.set_video_id = Some("added-occurrence".into());
            deliver(view, metadata, Ok(fresh), cx);
            super::navigation::reply(view, playlist(0, false, "0 tracks"), cx);
            assert_eq!(view.page.header.as_ref().unwrap().stats, "1 track");
            assert_eq!(view.rows.len(), 1, "Readback must retain the acknowledged addition over an older empty page");
            view.navigate(Route::Explore, cx);
            if view.catalog_requests.is_loading(CatalogTarget::Page) {
                super::navigation::reply(view, page(1, false), cx);
            }
            view.navigate(Route::Browse(BrowseLink::new("VLalpha", "Alpha")), cx);
            assert!(view.catalog_requests.is_loading(CatalogTarget::Page), "Replaying fresh totals cannot certify an older response's empty rows for forward reuse");
            super::navigation::reply(view, playlist(1, false, "1 track"), cx);
            assert_eq!(view.rows.len(), 1);
        }).unwrap();
    })
}

#[test]
#[ignore = "requires desktop, private D-Bus and isolated runtime/data; synthetic accounts only"]
fn playlist_reload_supersedes_older_readback_failures() -> Result<()> {
    super::navigation::check_cache_window(async |window, cx| {
        window.update(cx, |view, window, cx| {
            for (id, failure_phase, fresh) in [("visible-fresh",0,true),("visible-stale",0,false),("before",1,true),("after",2,true),("stale",2,false),("stale-before",1,false)] {
                view.error = None;
                view.navigate(Route::Browse(BrowseLink::new(format!("VL{id}"), id)), cx);
                super::navigation::reply(view, playlist(1, false, "1 track"), cx);
                view.sidebar_playlists = vec![serde_json::from_value(json!({"id":format!("VL{id}"),"title":id,"subtitle":"1 track"})).unwrap()];
                super::navigation::confirm(view, window, Mutation::RemoveFromPlaylist {
                    id:id.into(), video_id:"00000000000".into(), set_video_id:"occurrence-0".into(),
                }, cx);
                let mut metadata = Some(view.catalog_requests.pending.try_recv().unwrap());
                if failure_phase == 0 {
                    deliver(view, metadata.take().unwrap(), Err("Older playlist details failed".into()), cx);
                }
                view.reload_current_page(cx);
                if failure_phase == 0 {
                    assert!(view.sync.playlist_metadata_error.is_some(), "Reload must retain failed occurrence checks until its response is verified");
                }
                let jobs:Vec<_> = std::iter::from_fn(|| view.catalog_requests.pending.try_recv().ok()).collect();
                let mut page_read = None;
                let mut guide_read = None;
                for job in jobs {
                    match job.query.target() {
                        CatalogTarget::Page => page_read = Some(job),
                        CatalogTarget::Sidebar => guide_read = Some(job),
                        _ => panic!("Unexpected reload job"),
                    }
                }
                if failure_phase == 1 {
                    deliver(view, metadata.take().unwrap(), Err("Older playlist details failed".into()), cx);
                }
                let response = if fresh { playlist(0, false, "0 tracks") } else { playlist(1, false, "1 track") };
                deliver(view, page_read.unwrap(), Ok(response), cx);
                if let Some(metadata) = metadata {
                    deliver(view, metadata, Err("Older playlist details failed".into()), cx);
                }
                let mut guide = page(0, false);
                guide.shelves[0].items.push(crate::catalog::Item::Collection(serde_json::from_value(json!({"id":format!("VL{id}"),"title":id,"subtitle":"1 track"})).unwrap()));
                deliver(view, guide_read.unwrap(), Ok(guide), cx);
                if fresh {
                    assert!(view.error.is_none(), "A successful Reload must supersede older readback feedback: {id}: {:?}", view.error);
                    assert_eq!(view.page.header.as_ref().unwrap().stats, "0 tracks");
                    assert_eq!(view.sidebar_playlists[0].subtitle, "0 tracks", "A verified Reload must update the guide even after readback already failed");
                } else {
                    assert_eq!(view.error.as_deref(), Some("Older playlist details failed"), "A lagging Reload must not hide an unresolved readback failure");
                    view.navigate(Route::Explore, cx);
                    if view.catalog_requests.is_loading(CatalogTarget::Page) { super::navigation::reply(view, page(1, false), cx); }
                    view.navigate(Route::Browse(BrowseLink::new(format!("VL{id}"), id)), cx);
                    assert!(view.catalog_requests.is_loading(CatalogTarget::Page), "A known-lagging Reload must not enter the forward cache");
                    super::navigation::reply(view, playlist(0, false, "0 tracks"), cx);
                }
            }
        }).unwrap();
    })
}

#[test]
#[ignore = "requires desktop, private D-Bus and isolated runtime/data; synthetic accounts only"]
fn playlist_reload_cannot_settle_an_edit_acknowledged_after_it_started() -> Result<()> {
    super::navigation::check_cache_window(async |window, cx| {
        window.update(cx, |view, window, cx| {
            view.navigate(Route::Browse(BrowseLink::new("VLalpha", "Alpha")), cx);
            super::navigation::reply(view, playlist(3, false, "3 tracks"), cx);
            view.reload_current_page(cx);
            let jobs:Vec<_> = std::iter::from_fn(|| view.catalog_requests.pending.try_recv().ok()).collect();
            let mut older_page = None;
            for job in jobs {
                if job.query.target() == CatalogTarget::Page { older_page = Some(job); }
                else { deliver(view, job, Ok(page(0, false)), cx); }
            }
            super::navigation::confirm(view, window, Mutation::MoveInPlaylist {
                id:"alpha".into(), set_video_id:"occurrence-2".into(), before:Some("occurrence-0".into()),
            }, cx);
            let metadata = view.catalog_requests.pending.try_recv().unwrap();
            assert!(metadata.query.target() == CatalogTarget::PlaylistMetadata);
            deliver(view, older_page.unwrap(), Ok(playlist(3, false, "3 tracks")), cx);
            assert!(view.catalog_requests.is_loading(CatalogTarget::PlaylistMetadata), "A pre-edit Reload cannot cancel the post-edit readback, even when the edit has no occurrence checks");
            let mut fresh = playlist(3, false, "3 tracks");
            fresh.header.as_mut().unwrap().thumbnail = Some("synthetic-new-cover".into());
            fresh.shelves[0].items.rotate_right(1);
            deliver(view, metadata, Ok(fresh), cx);
            assert_eq!(view.page.header.as_ref().unwrap().thumbnail.as_deref(), Some("synthetic-new-cover"));
        }).unwrap();
    })
}

#[test]
#[ignore = "requires desktop, private D-Bus and isolated runtime/data; synthetic accounts only"]
fn partial_playlist_reload_keeps_queued_addition_readback() -> Result<()> {
    super::navigation::check_cache_window(async |window, cx| {
        window.update(cx, |view, window, cx| {
            view.navigate(Route::Browse(BrowseLink::new("VLalpha", "Alpha")), cx);
            super::navigation::reply(view, playlist(1, false, "1 track"), cx);
            super::navigation::confirm(view, window, Mutation::RemoveFromPlaylist {
                id:"alpha".into(), video_id:"00000000000".into(), set_video_id:"occurrence-0".into(),
            }, cx);
            let alpha = view.catalog_requests.pending.try_recv().unwrap();
            view.navigate(Route::Browse(BrowseLink::new("VLbeta", "Beta")), cx);
            super::navigation::reply(view, playlist(50, true, "500 tracks"), cx);
            view.account_event(account::Event::Applied {
                session:view.account.as_ref().unwrap().session.clone(),
                mutation:Box::new(Mutation::AddToPlaylist { id:"beta".into(), video_ids:vec!["00000000000".into()], duplicates:true }),
                result:Ok(Effect::PlaylistAdded { additions:vec![("00000000000".into(),"new-occurrence".into())], toast:None }), session_rejected:false,
            }, window, cx);
            view.reload_current_page(cx);
            let jobs:Vec<_> = std::iter::from_fn(|| view.catalog_requests.pending.try_recv().ok()).collect();
            for job in jobs {
                let response = if job.query.target()==CatalogTarget::Page { playlist(50, true, "500 tracks") } else { page(0, false) };
                deliver(view, job, Ok(response), cx);
            }
            assert_eq!(view.sync.playlist_refreshes.iter().map(|refresh| refresh.id.as_str()).collect::<Vec<_>>(), ["alpha","beta"], "A partial Reload missing the new occurrence cannot remove Beta's queued readback");
            deliver(view, alpha, Ok(playlist(0, false, "0 tracks")), cx);
            let beta = view.catalog_requests.pending.try_recv().unwrap();
            assert!(matches!(&beta.query, Query::PlaylistMetadata(refresh) if refresh.id=="beta"));
            deliver(view, beta, Ok(playlist(50, true, "501 tracks")), cx);
            assert_eq!(view.page.header.as_ref().unwrap().stats, "501 tracks");
        }).unwrap();
    })
}

#[test]
#[ignore = "requires desktop, private D-Bus and isolated runtime/data; synthetic accounts only"]
fn playlist_guide_replay_ends_with_its_read() -> Result<()> {
    super::navigation::check_cache_window(async |window, cx| {
        window.update(cx, |view, _, cx| {
            view.navigate(Route::Explore, cx);
            super::navigation::reply(view, page(1, false), cx);
            for terminal in ["continuation", "failure", "superseded"] {
                view.request(Query::SidebarPlaylists, cx);
                let initial = view.catalog_requests.pending.try_recv().unwrap();
                assert!(initial.query.target() == CatalogTarget::Sidebar);
                let mut guide = page(0, true);
                guide.shelves[0].items.push(crate::catalog::Item::Collection(serde_json::from_value(json!({"id":"VLalpha", "title":"Alpha", "subtitle":"3 tracks"})).unwrap()));
                deliver(view, initial, Ok(guide.clone()), cx);
                view.request(Query::SidebarMore(view.sync.sidebar_continuation.clone().unwrap()), cx);
                let older_guide = view.catalog_requests.pending.try_recv().unwrap();
                view.refresh_playlist_metadata("alpha", Vec::new(), None, None, cx);
                let metadata = view.catalog_requests.pending.try_recv().unwrap();
                deliver(view, metadata, Ok(playlist(2, false, "2 tracks")), cx);
                assert_eq!(view.sidebar_playlists[0].subtitle, "2 tracks");
                match terminal {
                    "continuation" => deliver(view, older_guide, Ok(page(0, false)), cx),
                    "failure" => deliver(view, older_guide, Err("Guide read failed".into()), cx),
                    _ => {
                        view.request(Query::SidebarPlaylists, cx);
                        let newer_guide = view.catalog_requests.pending.try_recv().unwrap();
                        deliver(view, older_guide, Err("Superseded guide failure".into()), cx);
                        deliver(view, newer_guide, Ok(guide), cx);
                        assert_eq!(view.sidebar_playlists[0].subtitle, "3 tracks", "A new guide read must not inherit the previous read's replay");
                        continue;
                    }
                }
                view.refresh_playlist_metadata("alpha", Vec::new(), None, None, cx);
                let metadata = view.catalog_requests.pending.try_recv().unwrap();
                deliver(view, metadata, Ok(playlist(1, false, "1 track")), cx);
                view.request(Query::SidebarPlaylists, cx);
                let newer_guide = view.catalog_requests.pending.try_recv().unwrap();
                let crate::catalog::Item::Collection(alpha) = &mut guide.shelves[0].items[0] else { panic!() };
                alpha.subtitle = "1 track".into();
                deliver(view, newer_guide, Ok(guide), cx);
                assert_eq!(view.sidebar_playlists[0].subtitle, "1 track", "A completed {terminal} must not leave an older replay for the next guide read");
            }
        }).unwrap();
    })
}

#[test]
#[ignore = "requires desktop, private D-Bus and isolated runtime/data; synthetic accounts only"]
fn playlist_refreshes_expire_cached_pages_and_cancel_deleted_playlists() -> Result<()> {
    super::navigation::check_cache_window(async |window, cx| {
        window.update(cx, |view, window, cx| {
            view.navigate(Route::Explore, cx);
            super::navigation::reply(view, page(1, false), cx);
            let track = view.page.tracks()[0].clone();
            view.account_event(account::Event::Applied {
                session:view.account.as_ref().unwrap().session.clone(),
                mutation:Box::new(Mutation::AddToPlaylist { id:"alpha".into(), video_ids:vec![track.id.clone()], duplicates:false }),
                result:Ok(Effect::PlaylistAdded { additions:vec![(track.id,"added-occurrence".into())], toast:None }), session_rejected:false,
            }, window, cx);
            let metadata = view.catalog_requests.pending.try_recv().unwrap();
            assert!(matches!(&metadata.query, Query::PlaylistMetadata(refresh) if refresh.id=="alpha"), "The held response must belong to Alpha's metadata read");
            view.navigate(Route::Browse(BrowseLink::new("VLalpha", "Alpha")), cx);
            assert!(view.catalog_requests.is_loading(CatalogTarget::Page), "Opening the destination requests its first page");
            super::navigation::reply(view, playlist(0, false, "0 tracks"), cx);
            view.navigate(Route::Explore, cx);
            if view.catalog_requests.is_loading(CatalogTarget::Page) {
                super::navigation::reply(view, page(1, false), cx);
            }
            assert!(view.page_cache.iter().any(|entry| matches!(&entry.route, Route::Browse(link) if link.id=="VLalpha")), "The lagging destination must have entered the forward cache");
            let mut fresh = playlist(1, false, "1 track");
            let crate::catalog::Item::Track(added) = &mut fresh.shelves[0].items[0] else { panic!() };
            added.details.set_video_id = Some("added-occurrence".into());
            deliver(view, metadata, Ok(fresh.clone()), cx);
            view.navigate(Route::Browse(BrowseLink::new("VLalpha", "Alpha")), cx);
            assert!(view.rows.is_empty(), "Forward must not reuse the stale empty playlist");
            assert!(view.catalog_requests.is_loading(CatalogTarget::Page), "Verified metadata expires the stale forward visit");
            assert!(matches!(super::navigation::reply(view, fresh.clone(), cx), Query::Browse(_)));
            assert_eq!(view.rows.len(), 1);
            assert_eq!(view.page.header.as_ref().unwrap().stats, "1 track");

            view.refresh_playlist_metadata("alpha", Vec::new(), None, None, cx);
            let metadata = view.catalog_requests.pending.try_recv().unwrap();
            deliver(view, metadata, Ok(fresh), cx);
            assert!(view.forward_page.as_ref().unwrap().fetched_at.is_none(), "Metadata alone cannot certify the active page's full rows for caching");

            super::navigation::confirm(view, window, Mutation::RemoveFromPlaylist {
                id:"alpha".into(), video_id:"00000000000".into(), set_video_id:"added-occurrence".into(),
            }, cx);
            let deleted_read = view.catalog_requests.pending.try_recv().unwrap();
            view.refresh_playlist_metadata("beta", Vec::new(), None, None, cx);
            view.refresh_playlist_metadata("gamma", Vec::new(), None, None, cx);
            super::navigation::confirm(view, window, Mutation::DeletePlaylist { id:"beta".into() }, cx);
            assert_eq!(view.sync.playlist_refreshes.iter().map(|refresh| refresh.id.as_str()).collect::<Vec<_>>(), ["alpha", "gamma"], "Deleting a queued playlist removes only that work");
            assert_eq!(view.catalog_requests.generations[CatalogTarget::PlaylistMetadata as usize].load(Ordering::Relaxed), deleted_read.id);
            super::navigation::confirm(view, window, Mutation::DeletePlaylist { id:"alpha".into() }, cx);
            let next = std::iter::from_fn(|| view.catalog_requests.pending.try_recv().ok())
                .find(|job| matches!(&job.query, Query::PlaylistMetadata(refresh) if refresh.id=="gamma"))
                .expect("Deleting the active playlist must start the next retained read");
            deliver(view, deleted_read, Err("Deleted playlist no longer exists".into()), cx);
            assert!(view.error.is_none(), "A deleted playlist's late read must not put an error on Library");
            let guide_generation = view.catalog_requests.generations[CatalogTarget::Sidebar as usize].load(Ordering::Relaxed);
            deliver(view, next, Ok(playlist(0, false, "0 tracks")), cx);
            assert!(view.sync.playlist_refreshes.is_empty());
            assert!(view.error.is_none());
            assert_eq!(view.catalog_requests.generations[CatalogTarget::Sidebar as usize].load(Ordering::Relaxed), guide_generation, "Finishing content readback must not reload guide membership");
        }).unwrap();
    })
}

#[test]
#[ignore = "requires desktop, private D-Bus and isolated runtime/data; synthetic accounts only"]
fn playlist_refreshes_keep_independent_edits_and_scope_failures() -> Result<()> {
    super::navigation::check_cache_window(async |window, cx| {
        window.update(cx, |view, window, cx| {
            view.navigate(Route::Browse(BrowseLink::new("VLalpha", "Alpha")), cx);
            super::navigation::reply(view, playlist(3, false, "3 tracks"), cx);
            super::navigation::confirm(view, window, Mutation::RemoveFromPlaylist {
                id:"alpha".into(), video_id:"00000000000".into(), set_video_id:"occurrence-0".into(),
            }, cx);
            let older_alpha = view.catalog_requests.pending.try_recv().unwrap();
            assert!(matches!(&older_alpha.query, Query::PlaylistMetadata(refresh) if refresh.id=="alpha"));

            view.navigate(Route::Browse(BrowseLink::new("VLbeta", "Beta")), cx);
            super::navigation::reply(view, playlist(1, false, "1 track"), cx);
            super::navigation::confirm(view, window, Mutation::RemoveFromPlaylist {
                id:"beta".into(), video_id:"00000000000".into(), set_video_id:"occurrence-0".into(),
            }, cx);
            let beta = view.page.clone();
            assert!(view.catalog_requests.pending.is_empty(), "Editing Beta must not cancel Alpha's pending readback");
            assert_eq!(view.sync.playlist_refreshes.len(), 2);
            assert_eq!(view.catalog_requests.generations[CatalogTarget::PlaylistMetadata as usize].load(Ordering::Relaxed), older_alpha.id);

            view.go_back(cx);
            let loaded = view.page.clone();
            super::navigation::reply(view, loaded, cx);
            super::navigation::confirm(view, window, Mutation::MoveInPlaylist {
                id:"alpha".into(), set_video_id:"occurrence-2".into(), before:Some("occurrence-1".into()),
            }, cx);
            assert!(view.catalog_requests.pending.is_empty(), "A move coalesces behind the in-flight read");
            view.account_event(account::Event::Applied {
                session:view.account.as_ref().unwrap().session.clone(),
                mutation:Box::new(Mutation::AddToPlaylist { id:"alpha".into(), video_ids:vec!["00000000001".into()], duplicates:true }),
                result:Ok(Effect::PlaylistAdded { additions:vec![("00000000001".into(),"new-occurrence".into())], toast:None }), session_rejected:false,
            }, window, cx);
            assert!(view.catalog_requests.pending.is_empty(), "An addition coalesces behind the in-flight read");
            super::navigation::confirm(view, window, Mutation::RemoveFromPlaylist {
                id:"alpha".into(), video_id:"00000000001".into(), set_video_id:"new-occurrence".into(),
            }, cx);
            assert!(view.catalog_requests.pending.is_empty());
            deliver(view, older_alpha, Err("Superseded metadata failure".into()), cx);
            let alpha = view.catalog_requests.pending.try_recv().unwrap();
            assert!(matches!(&alpha.query, Query::PlaylistMetadata(refresh) if refresh.additions.is_empty() && refresh.removals.contains(&"occurrence-0".into()) && refresh.removals.contains(&"new-occurrence".into()) && refresh.order.is_some()), "The single successor retains the removal, move and undone addition");
            assert!(view.error.is_none(), "An obsolete readback cannot publish an error or consume the current refresh");

            super::navigation::confirm(view, window, Mutation::EditPlaylist {
                id:"alpha".into(), title:"Renamed Alpha".into(), description:"New description".into(), privacy:account::Privacy::Public,
            }, cx);
            let collection = serde_json::from_value(json!({"id":"VLalpha", "title":"Renamed Alpha", "subtitle":"Playlist"})).unwrap();
            super::navigation::confirm(view, window, Mutation::CollectionLibrary { collection, save:true }, cx);
            let mut fresh = playlist(3, false, "2 tracks");
            fresh.shelves[0].items.remove(0);
            fresh.shelves[0].items.rotate_right(1);
            deliver(view, alpha, Ok(fresh), cx);
            assert_eq!(view.page.title, "Renamed Alpha");
            assert_eq!(view.page.privacy.as_deref(), Some("PUBLIC"));
            assert_eq!(view.page.header.as_ref().unwrap().collection.saved, Some(true), "Readback owns totals and cover, not a newer save state");
            assert_eq!(view.page.tracks()[0].details.set_video_id.as_deref(), Some("occurrence-2"));
            let requests:Vec<_> = std::iter::from_fn(|| view.catalog_requests.pending.try_recv().ok()).collect();
            assert!(!requests.iter().any(|job| job.query.target()==CatalogTarget::Sidebar), "Content edits patch guide details without reloading membership");
            let beta_job = requests.into_iter()
                .find(|job| matches!(&job.query, Query::PlaylistMetadata(refresh) if refresh.id=="beta"))
                .expect("Finishing Alpha must start Beta's retained refresh");

            view.navigate(Route::Browse(BrowseLink::new("VLbeta", "Beta")), cx);
            super::navigation::reply(view, beta, cx);
            deliver(view, beta_job, Ok(playlist(0, false, "0 tracks")), cx);
            assert!(view.rows.is_empty());
            assert_eq!(view.page.header.as_ref().unwrap().stats, "0 tracks");
            assert!(view.catalog_requests.pending.is_empty(), "The second edited playlist must not trigger a guide read that can roll back the first");
            view.go_back(cx);
            assert_eq!(view.page.header.as_ref().unwrap().stats, "2 tracks", "Back retains the other playlist's verified totals");
            assert_eq!(view.page.title, "Renamed Alpha");
            let loaded = view.page.clone();
            super::navigation::reply(view, loaded, cx);

            view.refresh_playlist_metadata("alpha", Vec::new(), None, None, cx);
            let job = std::iter::from_fn(|| view.catalog_requests.pending.try_recv().ok()).find(|job| job.query.target()==CatalogTarget::PlaylistMetadata).unwrap();
            view.error = Some("A newer save failed".into());
            deliver(view, job, Err("Older details refresh failed".into()), cx);
            assert_eq!(view.error.as_deref(), Some("A newer save failed"));
            assert!(view.sync.playlist_metadata_error.is_none());

            view.error = None;
            view.refresh_playlist_metadata("alpha", Vec::new(), None, None, cx);
            let job = std::iter::from_fn(|| view.catalog_requests.pending.try_recv().ok()).find(|job| job.query.target()==CatalogTarget::PlaylistMetadata).unwrap();
            deliver(view, job, Err("Details refresh failed".into()), cx);
            assert_eq!(view.error.as_deref(), Some("Details refresh failed"));
            view.reload_current_page(cx);
            assert_eq!(view.error.as_deref(), Some("Details refresh failed"), "Reload retains its warning until the response verifies the edit");
            assert!(view.sync.playlist_metadata_error.is_some());
            super::navigation::reply(view, playlist(2, false, "2 tracks"), cx);
            assert!(view.error.is_none());
            assert!(view.sync.playlist_metadata_error.is_none());
        }).unwrap();
    })
}

#[test]
#[ignore = "requires desktop, private D-Bus and isolated runtime/data; synthetic accounts only"]
fn playlist_edits_refresh_details_without_replacing_loaded_songs() -> Result<()> {
    super::navigation::check_cache_window(async |window, cx| {
        window.update(cx, |view, window, cx| {
            let (sender, commands) = async_channel::bounded(4);
            view.account_commands = sender;
            view.navigate(Route::Browse(BrowseLink::new("VLsynthetic", "Sync validation playlist")), cx);
            let mut initial = page(1, false);
            initial.header = crate::catalog::parse_page(&json!({"header":{"musicResponsiveHeaderRenderer":{
                "title":{"simpleText":"Sync validation playlist"},
                "secondSubtitle":{"simpleText":"1 track • 2 minutes, 17 seconds"}
            }},"contents":[]}), "Playlist").unwrap().header;
            super::navigation::reply(view, initial.clone(), cx);
            view.sidebar_playlists = vec![
                serde_json::from_value(json!({"id":"VLprefix", "title":"First loaded playlist", "subtitle":"10 tracks"})).unwrap(),
                serde_json::from_value(json!({"id":"VLsynthetic", "title":"Sync validation playlist", "subtitle":"1 track"})).unwrap(),
            ];
            view.sync.sidebar_coverage = crate::app::sync::PageCoverage { first_page_len:1, continued:true };
            view.request(Query::Revalidate(Box::new(Query::SidebarPlaylists)), cx);
            let older_guide = view.catalog_requests.pending.try_recv().unwrap();
            view.revalidate_page(cx);
            let older_page = view.catalog_requests.pending.try_recv().unwrap();
            let track = view.page.tracks()[0].clone();
            let removal = Mutation::RemoveFromPlaylist { id:"synthetic".into(), video_id:track.id, set_video_id:"occurrence-0".into() };
            assert!(view.mutate(removal.clone(), cx));
            assert!(matches!(commands.try_recv().unwrap(), account::Command::Apply { .. }));
            view.account_event(account::Event::Applied {
                session:view.account.as_ref().unwrap().session.clone(), mutation:Box::new(removal),
                result:Ok(Effect::Changed), session_rejected:false,
            }, window, cx);
            assert!(view.rows.is_empty(), "Confirmed removal immediately removes the row");
            let job = view.catalog_requests.pending.try_recv().expect("Confirmed removal refreshes the playlist details");
            assert!(matches!(&job.query, Query::PlaylistMetadata(refresh) if refresh.id=="synthetic" && refresh.removals==["occurrence-0"]));
            let mut fresh = initial.clone();
            // Keep an old row in this response to prove that metadata publication
            // cannot undo the confirmed edit, even if service reads disagree.
            fresh.header.as_mut().unwrap().stats = "0 tracks".into();
            view.catalog_result(CatalogResult {
                id:job.id, account_revision:job.account_revision, account_generation:job.account_generation,
                history_generation:job.history_generation, append:false, revalidate:false,
                result:Ok(fresh), session_rejected:false, target:job.query.target(),
            }, cx);
            assert!(view.rows.is_empty());
            assert_eq!(view.page.header.as_ref().unwrap().stats, "0 tracks");
            view.catalog_result(CatalogResult {
                id:older_page.id, account_revision:older_page.account_revision, account_generation:older_page.account_generation,
                history_generation:older_page.history_generation, append:false, revalidate:true,
                result:Ok(initial), session_rejected:false, target:CatalogTarget::Page,
            }, cx);
            assert!(view.rows.is_empty(), "An older page response must keep the confirmed removal");
            assert_eq!(view.page.header.as_ref().unwrap().stats, "0 tracks", "An older page response must keep verified totals");
            let job = older_guide;
            assert!(job.query.target() == CatalogTarget::Sidebar);
            let mut guide = page(0, true);
            guide.shelves[0].items.push(crate::catalog::Item::Collection(view.sidebar_playlists[0].clone()));
            view.catalog_result(CatalogResult {
                id:job.id, account_revision:job.account_revision, account_generation:job.account_generation,
                history_generation:job.history_generation, append:false, revalidate:true,
                result:Ok(guide), session_rejected:false, target:CatalogTarget::Sidebar,
            }, cx);
            assert_eq!(view.sidebar_playlists[1].subtitle, "0 tracks", "A playlist in the loaded guide tail must update without scanning it again");
            assert!(view.error.is_none());
            assert!(view.catalog_requests.pending.is_empty());

            view.page = page(500, true);
            view.page.header = crate::catalog::parse_page(&json!({"header":{"musicResponsiveHeaderRenderer":{
                "title":{"simpleText":"Sync validation playlist"}, "secondSubtitle":{"simpleText":"500 tracks"}
            }},"contents":[]}), "Playlist").unwrap().header;
            view.rebuild_rows();
            view.detail_list.state.scroll_to(ListOffset { item_ix:451, offset_in_item:px(8.) });
            cx.notify();
        }).unwrap();
        super::render::wait_for_presented_frame(window, cx).await;
        window.update(cx, |view, window, cx| {
            let before = view.detail_list.state.logical_scroll_top();
            assert_eq!(before.item_ix, 451);
            let anchor = view.rows[450].id().to_owned();
            view.request(Query::Revalidate(Box::new(Query::SidebarPlaylists)), cx);
            let older_guide = view.catalog_requests.pending.try_recv().unwrap();
            let track = view.page.tracks()[0].clone();
            view.account_event(account::Event::Applied {
                session:view.account.as_ref().unwrap().session.clone(),
                mutation:Box::new(Mutation::RemoveFromPlaylist { id:"synthetic".into(), video_id:track.id, set_video_id:"occurrence-0".into() }),
                result:Ok(Effect::Changed), session_rejected:false,
            }, window, cx);
            let job = view.catalog_requests.pending.try_recv().unwrap();
            let mut fresh = page(50, true);
            fresh.header = crate::catalog::parse_page(&json!({"header":{"musicResponsiveHeaderRenderer":{
                "title":{"simpleText":"Sync validation playlist"}, "secondSubtitle":{"simpleText":"499 tracks"}
            }},"contents":[]}), "Playlist").unwrap().header;
            view.catalog_result(CatalogResult {
                id:job.id, account_revision:job.account_revision, account_generation:job.account_generation,
                history_generation:job.history_generation, append:false, revalidate:false,
                result:Ok(fresh), session_rejected:false, target:job.query.target(),
            }, cx);
            assert_eq!(view.rows.len(), 499, "Refreshing details must retain all loaded songs");
            assert_eq!(view.rows[449].id(), anchor);
            assert_eq!(view.page.header.as_ref().unwrap().stats, "499 tracks");
            assert!(view.page.continuation.is_some());
            let after = view.detail_list.state.logical_scroll_top();
            assert_eq!(after.item_ix, 450, "The visible occurrence stays in place after removing an earlier row");
            assert_eq!(after.offset_in_item, before.offset_in_item);
            let job = older_guide;
            let mut guide = page(0, false);
            guide.shelves[0].items.push(crate::catalog::Item::Collection(serde_json::from_value(json!({"id":"VLsynthetic", "title":"Sync validation playlist", "subtitle":"500 tracks"})).unwrap()));
            view.catalog_result(CatalogResult {
                id:job.id, account_revision:job.account_revision, account_generation:job.account_generation,
                history_generation:job.history_generation, append:false, revalidate:true,
                result:Ok(guide), session_rejected:false, target:job.query.target(),
            }, cx);
            assert_eq!(view.sidebar_playlists[0].subtitle, "499 tracks", "A lagging guide reply must keep verified totals");
            cx.notify();
        }).unwrap();
        super::render::wait_for_presented_frame(window, cx).await;
        window
            .update(cx, |_, window, _| {
                super::render::capture(window, "playlist-details-after-removal");
            })
            .unwrap();
    })
}

#[test]
#[ignore = "requires desktop, private D-Bus and isolated runtime/data; set YT_MUSIC_NATIVE_TESTS=1; synthetic account only"]
fn native_sync_preserves_loaded_pages_and_rolls_back_ratings() -> Result<()> {
    super::isolate()?;
    let passed = Arc::new(AtomicBool::new(false));
    let result = passed.clone();
    gpui_platform::application().with_assets(Assets).run(move |cx| {
        cx.text_system().add_fonts(vec![Cow::Borrowed(include_bytes!("../../../assets/fonts/Roboto.ttf").as_slice())]).unwrap();
        crate::input::init(cx);
        let (sender, commands) = async_channel::bounded(64);
        let window = cx.open_window(WindowOptions {
            window_bounds:Some(WindowBounds::Windowed(Bounds::new(point(px(40.),px(40.)),size(px(1280.),px(700.))))),
            kind:WindowKind::PopUp,
            titlebar:Some(TitlebarOptions { title:Some("Music sync validation".into()), ..Default::default() }),
            focus:true, ..Default::default()
        }, move |window,cx| cx.new(|cx| {
            let mut view = Music::new(Startup::Guest,window,cx);
            view.account_commands.close();
            view.account_worker.take().unwrap().finish().unwrap();
            view.account_commands = sender;
            for target in CatalogTarget::ALL { view.catalog_requests.cancel(target); }
            view.catalog_requests = CatalogRequests::new();
            for generation in view.catalog_requests.generations.iter() { generation.store(1000,Ordering::Relaxed); }
            view.playback_jobs.close();
            let (jobs,pending) = async_channel::bounded(1);
            view.playback_jobs = jobs;
            view.pending_playback_jobs = pending;
            view.track_state = track_state::Worker::idle();
            view.player = None;
            view.media_controls = None;
            view.video = None;
            view.store = None;
            view.library.volume = 0.;
            let session: Session = serde_json::from_value(json!({"cookie":"SAPISID=synthetic","playback_cookie":"# Netscape HTTP Cookie File\n","authuser":"0","delegated_session_id":null,"data_sync_id":"synthetic||","client_version":"test"})).unwrap();
            view.session = auth::shared(Some(session));
            let session = view.session.read().unwrap().clone().unwrap();
            view.account = Some(Connection { session, account:Account { name:"Synthetic account".into(),handle:String::new(),thumbnail:None } });
            view.route = Route::Browse(BrowseLink::new("VLsynthetic","Sync validation playlist"));
            view.page = page(500,true);
            let tail = view.page.shelves[0].items.split_off(50);
            let mut shelf = view.page.shelves[0].clone();
            shelf.items = tail;
            view.page.shelves.push(shelf);
            view.sync.page_coverage = crate::app::sync::PageCoverage { first_page_len: 1, continued: true };
            view.page.title = "Sync validation playlist".into();
            view.rebuild_rows();
            view
        })).unwrap();
        cx.activate(true);
        cx.spawn(async move |cx| {
            window.update(cx,|view,_,cx| {
                view.list_scroll.scroll_to_item(450,ScrollStrategy::Top);
                let track = view.page.tracks()[450].clone();
                view.overlay = Some(Overlay::Track { track, index: Some(450), queue_index: None });
                cx.notify();
            }).unwrap();
            cx.background_executor().timer(Duration::from_millis(600)).await;
            println!("SYNC_SCREENSHOT loaded-menu");
            cx.background_executor().timer(Duration::from_millis(800)).await;
            window.update(cx,|view,window,cx| {
                let revision = view.account_revision;
                let scroll = view.list_scroll.0.borrow().base_handle.offset();
                view.refresh_remote(cx);
                assert_eq!(view.account_revision,revision,"Refresh is not an account switch");
                let job = view.catalog_requests.pending.try_recv().unwrap();
                assert!(matches!(job.query,Query::Revalidate(_)));
                view.catalog_result(CatalogResult { id:job.id, account_revision:job.account_revision,account_generation:view.account_generation,history_generation:view.history_generation, append:false, revalidate:true, result:Ok(page(50,true)),session_rejected:false,target:CatalogTarget::Page },cx);
                assert_eq!(view.rows.len(),500);
                assert!(matches!(view.overlay,Some(Overlay::Track { index:Some(450),.. })));
                assert_eq!(view.list_scroll.0.borrow().base_handle.offset(),scroll);
                view.overlay = None;
                assert!(view.error.is_none(),"Synthetic fixture error: {:?}",view.error);
                view.navigate(Route::Explore,cx);
                let start = Instant::now();
                view.go_back(cx);
                assert_eq!(view.rows.len(),500,"Back renders cached rows before a network reply");
                println!("500-row cached Back: {:?}",start.elapsed());
                let track = view.page.tracks()[450].clone();
                let rate = Mutation::RateSong { track:track.clone(),rating:Rating::Like };
                assert!(view.mutate(rate.clone(),cx));
                assert!(view.is_liked(&track),"Rating must change before a reply");
                assert!(matches!(commands.try_recv().unwrap(),account::Command::Apply { .. }));
                let session = view.account.as_ref().unwrap().session.clone();
                view.account_event(account::Event::Applied { session, mutation:Box::new(rate),result:Err("Synthetic save failure".into()),session_rejected:false }, window, cx);
                assert!(!view.is_liked(&track));
                assert_eq!(view.error.as_deref(),Some("Synthetic save failure"));
                assert_eq!(view.rows.len(),500);
                let disliked = view.page.tracks()[451].clone();
                view.ratings.insert(disliked.id.clone(),Rating::Dislike);
                view.library.queue.replace(vec![track.clone(),disliked,view.page.tracks()[452].clone()],0);
                view.active_id = Some(77);
                view.playback = crate::app::Playback::Loaded { paused:false,buffering:false };
                assert!(view.mutate(Mutation::RateSong { track:track.clone(),rating:Rating::Dislike },cx));
                assert_eq!(view.library.queue.index,Some(2),"Disliking the current song skips it and other disliked songs");
                while commands.try_recv().is_ok() {}
                view.error = None;
                let add = Mutation::AddToPlaylist { id:"VLsynthetic".into(),video_ids:vec![track.id.clone()],duplicates:false };
                assert!(view.mutate(add.clone(),cx));
                let session = view.account.as_ref().unwrap().session.clone();
                view.account_event(account::Event::Applied { session,mutation:Box::new(add),result:Ok(Effect::AlreadyAdded),session_rejected:false },window,cx);
                assert!(matches!(view.overlay,Some(Overlay::AlreadyAdded { .. })));
                while view.catalog_requests.pending.try_recv().is_ok() {}
                view.request(Query::SidebarPlaylists,cx);
                let first = view.catalog_requests.pending.try_recv().unwrap();
                let mut choices = page(0,true);
                choices.shelves[0].items.push(crate::catalog::Item::Collection(serde_json::from_value(json!({"id":"VLfirst","title":"First playlist","subtitle":"Playlist"})).unwrap()));
                view.catalog_result(CatalogResult { id:first.id,account_revision:view.account_revision,account_generation:view.account_generation,history_generation:view.history_generation,append:false,revalidate:false,result:Ok(choices),session_rejected:false,target:CatalogTarget::Sidebar },cx);
                let token = view.sync.sidebar_continuation.clone().expect("Sidebar retains continuation");
                view.request(Query::SidebarMore(token),cx);
                let more = view.catalog_requests.pending.try_recv().unwrap();
                let mut choices = page(0,false);
                choices.shelves[0].items.push(crate::catalog::Item::Collection(serde_json::from_value(json!({"id":"VLsecond","title":"Second playlist","subtitle":"Playlist"})).unwrap()));
                view.catalog_result(CatalogResult { id:more.id,account_revision:view.account_revision,account_generation:view.account_generation,history_generation:view.history_generation,append:true,revalidate:false,result:Ok(choices),session_rejected:false,target:CatalogTarget::Sidebar },cx);
                assert_eq!(view.sidebar_playlists.len(),2);
                assert!(view.sync.sidebar_continuation.is_none());
                let session = view.account.as_ref().unwrap().session.clone();
                view.account_event(account::Event::Applied { session,mutation:Box::new(Mutation::CreatePlaylist { title:"New playlist".into(),description:String::new(),privacy:account::Privacy::Private,initial_video_ids:Vec::new() }),result:Ok(Effect::CreatedPlaylist { id:"created".into() }),session_rejected:false },window,cx);
                assert!(std::iter::from_fn(|| view.catalog_requests.pending.try_recv().ok()).any(|job| job.query.target()==CatalogTarget::Sidebar),"Creating a playlist reloads sidebar membership");
                let session = view.account.as_ref().unwrap().session.clone();
                view.account_event(account::Event::Applied { session,mutation:Box::new(Mutation::DeletePlaylist { id:"first".into() }),result:Ok(Effect::Changed),session_rejected:false },window,cx);
                assert!(!view.sidebar_playlists.iter().any(|playlist| playlist.id=="VLfirst"));
                assert!(std::iter::from_fn(|| view.catalog_requests.pending.try_recv().ok()).any(|job| job.query.target()==CatalogTarget::Sidebar),"Deleting a playlist reloads sidebar membership");
                view.go_back(cx);
                assert_eq!(view.rows.len(),500);
                view.navigate(Route::Browse(BrowseLink::new("VLtarget","Target playlist")),cx);
                view.page = page(0,false);
                view.library.queue = Default::default();
                view.patch_added("target",&[(track.id.clone(),"new-occurrence".into())],cx);
                assert_eq!(view.rows.len(),1,"An addition can resolve metadata from the page left behind");
                assert_eq!(view.page.tracks()[0].details.set_video_id.as_deref(),Some("new-occurrence"));
                let id = view.catalog_requests.generations[CatalogTarget::Page as usize].load(Ordering::Relaxed);
                view.catalog_result(CatalogResult { id,account_revision:view.account_revision,account_generation:view.account_generation,history_generation:view.history_generation,append:false,revalidate:false,result:Ok(page(0,false)),session_rejected:false,target:CatalogTarget::Page },cx);
                assert_eq!(view.rows.len(),1,"An older page reply must preserve the acknowledged addition");
                view.go_back(cx);
                assert_eq!(view.rows.len(),500);
                cx.notify();
            }).unwrap();
            cx.background_executor().timer(Duration::from_millis(600)).await;
            println!("SYNC_SCREENSHOT already-added");
            window.update(cx,|view,_,cx| {
                view.overlay = None;
                view.library.queue = Default::default();
                view.navigate(Route::Liked,cx);
                let job = std::iter::from_fn(|| view.catalog_requests.pending.try_recv().ok()).collect::<Vec<_>>().into_iter().find(|job| job.query.target()==CatalogTarget::Page).unwrap();
                view.catalog_result(CatalogResult { id:job.id,account_revision:view.account_revision,account_generation:view.account_generation,history_generation:view.history_generation,append:false,revalidate:false,result:Ok(response(&[("Songs",(0..50).collect())],Some("page-2"),true)),session_rejected:false,target:CatalogTarget::Page },cx);
                view.error = Some("Synthetic playback failure".into());
                view.prefetch_page(50,50,12,cx);
                let job = view.catalog_requests.pending.try_recv().expect("A playback failure must not block page continuation");
                assert!(matches!(job.query,Query::More { .. }));
                view.catalog_result(CatalogResult { id:job.id,account_revision:view.account_revision,account_generation:view.account_generation,history_generation:view.history_generation,append:true,revalidate:false,result:Ok(response(&[("Songs",(50..500).collect())],Some("page-11"),true)),session_rejected:false,target:CatalogTarget::Page },cx);
                view.prefetch_page(500,500,12,cx);
                let job = view.catalog_requests.pending.try_recv().unwrap();
                view.catalog_result(CatalogResult { id:job.id,account_revision:view.account_revision,account_generation:view.account_generation,history_generation:view.history_generation,append:true,revalidate:false,result:Err("Synthetic page failure".into()),session_rejected:false,target:CatalogTarget::Page },cx);
                view.error = Some("Another playback failure".into());
                assert!(view.mutate(Mutation::RateSong { track:view.page.tracks()[0].clone(), rating:Rating::Like },cx));
                assert_eq!(view.error,view.sync.page_error,"An accepted edit must leave the page failure and its retry path visible");
                view.prefetch_page(500,500,12,cx);
                assert!(view.catalog_requests.pending.try_recv().is_err(),"Page errors block automatic retry even if playback replaces the banner");
                view.list_scroll.scroll_to_item_strict(450,ScrollStrategy::Top);
                cx.notify();
            }).unwrap();
            cx.background_executor().timer(Duration::from_millis(400)).await;
            window.update(cx,|view,window,cx| {
                window.focus(&view.focus,cx);
                let track = view.page.tracks()[450].clone();
                view.overlay = Some(Overlay::Track { track,index:Some(450),queue_index:None });
                // The keyboard ignores refresh while a menu is open; the reload
                // method also supports keeping an already-open menu anchored.
                view.reload_current_page(cx);
                let job = std::iter::from_fn(|| view.catalog_requests.pending.try_recv().ok()).collect::<Vec<_>>().into_iter().find(|job| job.query.target()==CatalogTarget::Page).unwrap();
                let mut first = vec![900];
                first.extend((0..49).rev());
                view.catalog_result(CatalogResult { id:job.id,account_revision:view.account_revision,account_generation:view.account_generation,history_generation:view.history_generation,append:false,revalidate:true,result:Ok(response(&[("Songs",first)],Some("fresh-page-2"),true)),session_rejected:false,target:CatalogTarget::Page },cx);
                assert_eq!(view.rows.len(),50,"Ctrl R replaces stale loaded pages");
                assert!(view.sync.page_error.is_none(),"Reload retries the page error");
                let job = view.catalog_requests.pending.try_recv().expect("Reload follows the fresh continuation to find the visible occurrence");
                assert!(matches!(job.query,Query::More { ref token,.. } if token.token()=="fresh-page-2"));
                view.catalog_result(CatalogResult { id:job.id,account_revision:view.account_revision,account_generation:view.account_generation,history_generation:view.history_generation,append:true,revalidate:false,result:Ok(response(&[("Songs",(50..500).rev().collect())],Some("fresh-tail"),true)),session_rejected:false,target:CatalogTarget::Page },cx);
                assert_eq!(view.rows[0].id(),format!("{:011}",900));
                assert!(matches!(view.overlay,Some(Overlay::Track { index:Some(99),.. })),"Menu follows the occurrence after the server reorder");
                assert!(view.sync.page_reload.is_none());
                view.overlay = None;
                let scroll = view.list_scroll.0.borrow();
                let height = scroll.last_item_size.unwrap().contents.height / 500.;
                assert_eq!(scroll.base_handle.offset().y,-height*99.,"Scroll follows occurrence 450 into row 99");
                drop(scroll);
                let event = KeyDownEvent { keystroke:Keystroke { key:"r".into(),modifiers:Modifiers { control:true,..Default::default() },..Default::default() },is_held:false,prefer_character_input:false };
                view.key_down(&event,window,cx);
                let job = std::iter::from_fn(|| view.catalog_requests.pending.try_recv().ok()).collect::<Vec<_>>().into_iter().find(|job| job.query.target()==CatalogTarget::Page).unwrap();
                let mut fresh = response(&[("Songs",(50..500).rev().collect())],None,true);
                fresh.shelves[0].items.insert(0,view.rows[0].clone());
                view.catalog_result(CatalogResult { id:job.id,account_revision:view.account_revision,account_generation:view.account_generation,history_generation:view.history_generation,append:false,revalidate:true,result:Ok(fresh),session_rejected:false,target:CatalogTarget::Page },cx);
                assert_eq!(view.rows.len(),451);
                assert_eq!(view.rows[50].id(),format!("{:011}",450));
                assert!(view.page.continuation.is_none());
                cx.notify();
            }).unwrap();
            cx.background_executor().timer(Duration::from_millis(400)).await;
            println!("SYNC_SCREENSHOT reload-order-anchor");
            cx.background_executor().timer(Duration::from_millis(800)).await;
            window.update(cx,|view,_,cx| {
                view.route = Route::Browse(BrowseLink::new("VLsynthetic","Collection anchor"));
                view.page = response(&[("Songs",(0..500).collect())],Some("tail"),true);
                view.page.header = crate::catalog::parse_page(&json!({"header":{"musicResponsiveHeaderRenderer":{"title":{"simpleText":"Collection anchor"}}},"contents":[]}),"Collection anchor").unwrap().header;
                view.page.title = "Collection anchor".into();
                view.sync.page_coverage = crate::app::sync::PageCoverage { first_page_len:1,continued:false };
                view.rebuild_rows();
                view.detail_list.state.scroll_to(ListOffset { item_ix:401,offset_in_item:px(8.) });
                let track = view.page.tracks()[400].clone();
                view.overlay = Some(Overlay::Track { track,index:Some(400),queue_index:None });
                cx.notify();
            }).unwrap();
            cx.background_executor().timer(Duration::from_millis(400)).await;
            let expected_collection_top = window.update(cx,|view,_,cx| {
                let before = view.detail_list.state.logical_scroll_top();
                assert_eq!(before.item_ix,401);
                let mut fresh = response(&[("Songs",(0..500).rev().collect())],None,true);
                fresh.header = view.page.header.clone();
                fresh.title = "Collection anchor".into();
                view.revalidate_page(cx);
                let job = std::iter::from_fn(|| view.catalog_requests.pending.try_recv().ok()).collect::<Vec<_>>().into_iter().find(|job| job.query.target()==CatalogTarget::Page).unwrap();
                view.catalog_result(CatalogResult { id:job.id,account_revision:view.account_revision,account_generation:view.account_generation,history_generation:view.history_generation,append:false,revalidate:true,result:Ok(fresh),session_rejected:false,target:CatalogTarget::Page },cx);
                let after = view.detail_list.state.logical_scroll_top();
                assert_eq!(after.item_ix,100,"Collection scroll follows occurrence 400 into row 99, after its heading");
                assert_eq!(after.offset_in_item,before.offset_in_item);
                assert!(matches!(view.overlay,Some(Overlay::Track { index:Some(99),.. })));
                (after.item_ix,after.offset_in_item)
            }).unwrap();
            cx.background_executor().timer(Duration::from_millis(400)).await;
            window.update(cx,|view,_,_| {
                let top = view.detail_list.state.logical_scroll_top();
                assert_eq!((top.item_ix,top.offset_in_item),expected_collection_top,"The rendered collection must preserve the anchor");
            }).unwrap();
            println!("SYNC_SCREENSHOT collection-order-anchor");
            cx.background_executor().timer(Duration::from_millis(800)).await;
            window.update(cx,|view,_,cx| {
                view.overlay = None;
                view.request(Query::SidebarPlaylists,cx);
                let job = view.catalog_requests.pending.try_recv().unwrap();
                let mut choices = page(0,true);
                choices.shelves[0].items.push(crate::catalog::Item::Collection(serde_json::from_value(json!({"id":"VLretry","title":"Retry playlist","subtitle":"Playlist"})).unwrap()));
                view.catalog_result(CatalogResult { id:job.id,account_revision:view.account_revision,account_generation:view.account_generation,history_generation:view.history_generation,append:false,revalidate:false,result:Ok(choices),session_rejected:false,target:CatalogTarget::Sidebar },cx);
                let token = view.sync.sidebar_continuation.clone().unwrap();
                view.request(Query::SidebarMore(token),cx);
                let job = view.catalog_requests.pending.try_recv().unwrap();
                view.catalog_result(CatalogResult { id:job.id,account_revision:view.account_revision,account_generation:view.account_generation,history_generation:view.history_generation,append:true,revalidate:false,result:Err("Synthetic sidebar failure".into()),session_rejected:false,target:CatalogTarget::Sidebar },cx);
                view.error = None;
                cx.notify();
            }).unwrap();
            cx.background_executor().timer(Duration::from_millis(400)).await;
            println!("SYNC_SCREENSHOT sidebar-error-stopped");
            cx.background_executor().timer(Duration::from_millis(800)).await;
            window.update(cx,|view,_,cx| {
                assert!(view.catalog_requests.pending.try_recv().is_err(),"Rendering near the sidebar end must not retry a failed continuation");
                assert_eq!(view.sidebar_error.as_deref(),Some("Synthetic sidebar failure"));
                view.request(Query::SidebarPlaylists,cx);
                let job = view.catalog_requests.pending.try_recv().expect("Explicit sidebar Retry remains available");
                view.catalog_result(CatalogResult { id:job.id,account_revision:view.account_revision,account_generation:view.account_generation,history_generation:view.history_generation,append:false,revalidate:false,result:Ok(page(0,false)),session_rejected:false,target:CatalogTarget::Sidebar },cx);
                assert!(view.sidebar_error.is_none());
                view.route = Route::Home;
                view.overlay = None;
                view.player_open = false;
                view.library.queue = Default::default();
                view.page = page(4,false);
                let shelf = view.page.shelves[0].clone();
                view.page.shelves = (0..8).map(|index| {
                    let mut shelf = shelf.clone();
                    shelf.title = format!("Home shelf {index}");
                    shelf
                }).collect();
                view.rebuild_rows();
                view.detail_list.state.reset(9);
                view.detail_list.state.scroll_to(ListOffset { item_ix:4,offset_in_item:px(0.) });
                cx.notify();
            }).unwrap();
            cx.background_executor().timer(Duration::from_millis(500)).await;
            let expected = window.update(cx,|view,_,cx| {
                let before = view.detail_list.state.logical_scroll_top();
                let before = (before.item_ix,before.offset_in_item);
                assert!(before.0 >= 3,"The Home fixture must be scrolled before refresh");
                let visible_title = view.page.shelves[before.0-1].title.clone();
                let mut fresh = view.page.clone();
                fresh.shelves.reverse();
                view.request(Query::Revalidate(Box::new(Query::Home)),cx);
                let job = std::iter::from_fn(|| view.catalog_requests.pending.try_recv().ok()).collect::<Vec<_>>().into_iter().find(|job| job.query.target()==CatalogTarget::Page).unwrap();
                view.catalog_result(CatalogResult { id:job.id,account_revision:job.account_revision,account_generation:view.account_generation,history_generation:view.history_generation,append:false,revalidate:true,result:Ok(fresh),session_rejected:false,target:CatalogTarget::Page },cx);
                let after = view.detail_list.state.logical_scroll_top();
                assert_eq!(view.page.shelves[after.item_ix-1].title,visible_title,"Home follows the visible shelf after the server reorders it");
                assert_eq!(after.offset_in_item,before.1);
                let expected = (after.item_ix,after.offset_in_item);
                let track = view.page.tracks()[0].clone();
                view.patch_confirmed(&Mutation::RateSong { track,rating:Rating::Like },cx);
                let after = view.detail_list.state.logical_scroll_top();
                assert_eq!((after.item_ix,after.offset_in_item),expected,"A save acknowledgement must keep the visible shelf");
                expected
            }).unwrap();
            cx.background_executor().timer(Duration::from_millis(300)).await;
            window.update(cx,|view,_,_| {
                let after = view.detail_list.state.logical_scroll_top();
                assert_eq!((after.item_ix,after.offset_in_item),expected,"The next frame must preserve Home's scroll position");
            }).unwrap();
            window.update(cx,|view,_,cx| {
                view.navigate(Route::Recent,cx);
                let job = std::iter::from_fn(|| view.catalog_requests.pending.try_recv().ok()).find(|job| job.query.target()==CatalogTarget::Page).unwrap();
                view.catalog_result(CatalogResult { id:job.id,account_revision:job.account_revision,account_generation:view.account_generation,history_generation:view.history_generation,append:false,revalidate:false,result:Ok(response(&[("Today",vec![0]),("Yesterday",vec![1])],Some("older"),false)),session_rejected:false,target:CatalogTarget::Page },cx);
                view.request(Query::More { token:view.page.continuation.clone().unwrap(), search:None },cx);
                let job = view.catalog_requests.pending.try_recv().unwrap();
                view.catalog_result(CatalogResult { id:job.id,account_revision:job.account_revision,account_generation:view.account_generation,history_generation:view.history_generation,append:true,revalidate:false,result:Ok(response(&[("Yesterday",vec![2,5]),("Last week",vec![3])],Some("oldest"),false)),session_rejected:false,target:CatalogTarget::Page },cx);
                let headings = view.list_rows.iter().filter_map(|row| match row { crate::app::ListRow::Heading(index)=>Some(view.page.shelves[*index].title.as_str()),_=>None }).collect::<Vec<_>>();
                assert_eq!(headings,["Today","Yesterday","Last week"],"A continued day gets one heading");
                view.detail_list.state.scroll_to(ListOffset { item_ix:3,offset_in_item:px(0.) });
                view.revalidate_page(cx);
                let job = view.catalog_requests.pending.try_recv().unwrap();
                view.catalog_result(CatalogResult { id:job.id,account_revision:job.account_revision,account_generation:view.account_generation,history_generation:view.history_generation,append:false,revalidate:true,result:Ok(response(&[("Today",vec![2,0]),("Yesterday",vec![4])],Some("fresh"),false)),session_rejected:false,target:CatalogTarget::Page },cx);
                let display = view.list_rows.iter().map(|row| match row {
                    crate::app::ListRow::Heading(index)=>view.page.shelves[*index].title.clone(),
                    crate::app::ListRow::Item(index)=>view.rows[*index].id().to_owned(),
                    _=>panic!("Unexpected History row")
                }).collect::<Vec<_>>();
                assert_eq!(display,vec!["Today".into(),format!("{:011}",2),format!("{:011}",0),"Yesterday".into(),format!("{:011}",4),format!("{:011}",5),"Last week".into(),format!("{:011}",3)]);
                assert_eq!(view.detail_list.state.item_count(),8);
                view.page.title = "History".into();
                view.detail_list.state.scroll_to(ListOffset::default());
                cx.notify();
            }).unwrap();
            cx.background_executor().timer(Duration::from_millis(400)).await;
            window.update(cx,|_,window,_| { super::render::capture(window,"history-day-groups"); }).unwrap();
            println!("HISTORY_GROUPS continuation=passed replay_refresh=passed virtual_rows=8");
            result.store(true,Ordering::Release);
            println!("SYNC_SCREENSHOT home-scroll-preserved");
            cx.update(|cx| cx.quit());
        }).detach();
        cx.spawn(async move |cx| {
            cx.background_executor().timer(Duration::from_secs(20)).await;
            cx.update(|cx| cx.quit());
        }).detach();
    });
    ensure!(
        passed.load(Ordering::Acquire),
        "Native sync checks did not complete"
    );
    Ok(())
}
