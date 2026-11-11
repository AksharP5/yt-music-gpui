//! Cached navigation with intercepted catalog reads and synthetic account state only.
use super::account::{connection, session};
use super::render::wait_for_presented_frame;
use crate::{
    account::{self, Effect, Mutation, Section},
    app::{CatalogRequests, CatalogResult, CatalogTarget, Music, Query, Route, Search, Startup},
    assets::Assets,
    catalog::{BrowseLink, Page, Rating},
    search::{SearchFilter, SearchScope},
    track_state,
};
use anyhow::{Result, ensure};
use gpui::{
    AppContext, Bounds, Context, ScrollStrategy, Window, WindowBounds, WindowKind, WindowOptions,
    point, px, size,
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

pub(super) fn reply(view: &mut Music, page: Page, cx: &mut Context<Music>) -> Query {
    let job = std::iter::from_fn(|| view.catalog_requests.pending.try_recv().ok())
        .find(|job| job.query.target() == CatalogTarget::Page)
        .expect("Expected a page request");
    let append = matches!(job.query, Query::More { .. });
    let revalidate = matches!(job.query, Query::Revalidate(_));
    view.catalog_result(
        CatalogResult {
            id: job.id,
            account_revision: job.account_revision,
            account_generation: job.account_generation,
            history_generation: job.history_generation,
            append,
            revalidate,
            result: Ok(page),
            session_rejected: false,
            target: CatalogTarget::Page,
        },
        cx,
    );
    job.query
}

fn confirm(view: &mut Music, window: &mut Window, mutation: Mutation, cx: &mut Context<Music>) {
    view.account_event(
        account::Event::Applied {
            session: view.account.as_ref().unwrap().session.clone(),
            mutation: Box::new(mutation),
            result: Ok(Effect::Changed),
            session_rejected: false,
        },
        window,
        cx,
    );
}

fn page(title: &str, token: &str) -> Page {
    let mut page =
        crate::app::sync::tests::response(&[("Songs", (0..100).collect())], Some(token), true);
    page.title = title.into();
    page
}

fn check_cache(check: fn(&mut Music, &mut Window, &mut Context<Music>)) -> Result<()> {
    check_cache_window(async move |window, cx| {
        window.update(cx, check).unwrap();
    })
}

pub(super) fn check_cache_window(
    check: impl AsyncFnOnce(gpui::WindowHandle<Music>, &mut gpui::AsyncApp) + 'static,
) -> Result<()> {
    super::isolate()?;
    let passed = Arc::new(AtomicBool::new(false));
    let result = passed.clone();
    gpui_platform::application()
        .with_assets(Assets)
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
                            point(px(40.), px(40.)),
                            size(px(1280.), px(800.)),
                        ))),
                        kind: WindowKind::PopUp,
                        focus: true,
                        ..Default::default()
                    },
                    |window, cx| {
                        cx.new(|cx| {
                            let mut view = Music::new(Startup::Guest, window, cx);
                            view.account_commands.close();
                            view.account_worker.take().unwrap().finish().unwrap();
                            for target in CatalogTarget::ALL {
                                view.catalog_requests.cancel(target);
                            }
                            view.catalog_requests = CatalogRequests::new();
                            for generation in view.catalog_requests.generations.iter() {
                                generation.store(1000, Ordering::Relaxed);
                            }
                            view.track_state = track_state::Worker::idle();
                            view.player = None;
                            view.video = None;
                            view.mpris = None;
                            view.store = None;
                            view.library.volume = 0.;
                            view.account = Some(connection(session("cache-regression")));
                            view.clear_account_state(cx);
                            view.navigate(Route::Explore, cx);
                            reply(&mut view, page("Explore", "explore-tail"), cx);
                            view
                        })
                    },
                )
                .unwrap();
            cx.activate(true);
            cx.spawn(async move |cx| {
                wait_for_presented_frame(window, cx).await;
                check(window, cx).await;
                wait_for_presented_frame(window, cx).await;
                result.store(true, Ordering::Relaxed);
                cx.update(|cx| cx.quit());
            })
            .detach();
        });
    ensure!(
        passed.load(Ordering::Relaxed),
        "Cache regression check did not complete"
    );
    Ok(())
}

#[test]
#[ignore = "requires desktop, private D-Bus and isolated runtime/data; synthetic accounts only"]
fn playlist_add_does_not_copy_a_saved_song_from_an_invalidated_page() -> Result<()> {
    check_cache_window(async |window, cx| {
        let track = window
            .update(cx, |view, window, cx| {
                view.navigate(
                    Route::Search(Search {
                        query: "song to save".into(),
                        filter: SearchFilter::Songs,
                        scope: SearchScope::Catalog,
                    }),
                    cx,
                );
                let mut unsaved = page("Unsaved song", "search-tail");
                let crate::catalog::Item::Track(track) = &mut unsaved.shelves[0].items[0] else {
                    unreachable!()
                };
                track.details.in_library = Some(false);
                let track = track.clone();
                reply(view, unsaved, cx);
                view.navigate(
                    Route::Browse(BrowseLink::new("VLdestination", "Destination")),
                    cx,
                );
                reply(
                    view,
                    crate::app::sync::tests::response(&[("Songs", (100..200).collect())], None, true),
                    cx,
                );
                confirm(
                    view,
                    window,
                    Mutation::SongLibrary {
                        track: track.clone(),
                        save: true,
                    },
                    cx,
                );
                view.back.clear();
                view.account_event(
                    account::Event::Applied {
                        session: view.account.as_ref().unwrap().session.clone(),
                        mutation: Box::new(Mutation::AddToPlaylist {
                            id: "destination".into(),
                            video_ids: vec![track.id.clone()],
                            duplicates: false,
                        }),
                        result: Ok(Effect::PlaylistAdded {
                            additions: vec![(track.id.clone(), "added-occurrence".into())],
                        }),
                        session_rejected: false,
                    },
                    window,
                    cx,
                );
                assert!(
                    view.page
                        .tracks()
                        .iter()
                        .all(|item| item.id != track.id || item.details.in_library == Some(true)),
                    "Adding a song must not resurrect the cached pre-save library state"
                );
                assert!(
                    view.catalog_requests.is_loading(CatalogTarget::Page),
                    "A successful addition without current metadata must revalidate the active destination"
                );
                let mut fresh = page("Authoritative playlist", "fresh-tail");
                let crate::catalog::Item::Track(saved) = &mut fresh.shelves[0].items[0] else {
                    unreachable!()
                };
                saved.details.in_library = Some(true);
                assert!(matches!(reply(view, fresh, cx), Query::Revalidate(_)));
                assert_eq!(view.page.tracks()[0].details.in_library, Some(true));
                assert!(
                    view.page.tracks().iter().any(|item| item.id == track.id),
                    "The server response must make the successfully added song visible"
                );

                track
            })
            .unwrap();
        wait_for_presented_frame(window, cx).await;
        window
            .update(cx, |view, window, cx| {
                super::render::capture(window, "playlist-added-revalidated");
                assert!(
                    view.page.tracks().iter().any(|item| item.id == track.id),
                    "The added song must remain in the presented destination"
                );

                // A partial metadata match must also refresh, even with a read already in flight.
                view.revalidate_page(cx);
                let older_read = view.catalog_requests.pending.try_recv().unwrap();
                view.account_event(
                    account::Event::Applied {
                        session: view.account.as_ref().unwrap().session.clone(),
                        mutation: Box::new(Mutation::AddToPlaylist {
                            id: "destination".into(),
                            video_ids: vec![track.id.clone(), "missing-song".into()],
                            duplicates: false,
                        }),
                        result: Ok(Effect::PlaylistAdded {
                            additions: vec![
                                (track.id.clone(), "another-occurrence".into()),
                                ("missing-song".into(), "missing-occurrence".into()),
                            ],
                        }),
                        session_rejected: false,
                    },
                    window,
                    cx,
                );
                let requests: Vec<_> =
                    std::iter::from_fn(|| view.catalog_requests.pending.try_recv().ok()).collect();
                assert_eq!(
                    requests.len(),
                    1,
                    "Missing any metadata must supersede the older read"
                );
                assert!(requests[0].id > older_read.id);
                assert!(matches!(requests[0].query, Query::Revalidate(_)));
                view.catalog_requests.cancel(CatalogTarget::Page);

                view.navigate(Route::Explore, cx);
                reply(view, page("Explore", "explore-tail"), cx);
                view.account_event(
                    account::Event::Applied {
                        session: view.account.as_ref().unwrap().session.clone(),
                        mutation: Box::new(Mutation::AddToPlaylist {
                            id: "destination".into(),
                            video_ids: vec!["missing-song".into()],
                            duplicates: false,
                        }),
                        result: Ok(Effect::PlaylistAdded {
                            additions: vec![("missing-song".into(), "inactive-occurrence".into())],
                        }),
                        session_rejected: false,
                    },
                    window,
                    cx,
                );
                assert!(
                    view.catalog_requests.pending.is_empty(),
                    "An inactive destination needs no immediate read"
                );
                view.navigate(
                    Route::Browse(BrowseLink::new("VLdestination", "Destination")),
                    cx,
                );
                assert!(
                    view.rows.is_empty(),
                    "The inactive destination must fail the forward generation check"
                );
                assert!(matches!(
                    reply(view, page("Updated destination", "new-tail"), cx),
                    Query::Browse(_)
                ));
            })
            .unwrap();
    })
}

#[test]
#[ignore = "requires desktop, private D-Bus and isolated runtime/data; synthetic accounts only"]
fn stale_forward_revisit_replaces_loaded_tail_and_continuation() -> Result<()> {
    check_cache(|view, _, cx| {
        let route = Route::Browse(BrowseLink::new("VLexpired", "Expired playlist"));
        view.navigate(route.clone(), cx);
        reply(view, page("Playlist", "first-tail"), cx);
        view.prefetch_page(100, 100, 0, cx);
        reply(
            view,
            crate::app::sync::tests::response(
                &[("Songs", (100..200).collect())],
                Some("old-tail"),
                true,
            ),
            cx,
        );
        assert_eq!(view.rows.len(), 200);
        view.forward_page.as_mut().unwrap().fetched_at =
            Some(Instant::now() - Duration::from_secs(181));
        view.navigate(Route::Explore, cx);
        view.navigate(route, cx);
        assert_eq!(
            view.rows.len(),
            200,
            "The expired page is visible until its refresh arrives"
        );
        reply(view, page("Refreshed playlist", "fresh-tail"), cx);
        assert_eq!(
            view.rows.len(),
            100,
            "Forward refresh must discard loaded tails outside the fresh response"
        );
        assert_eq!(
            view.page.continuation.as_ref().unwrap().token(),
            "fresh-tail"
        );
        assert!(!view.sync.page_coverage.continued);
        view.prefetch_page(100, 100, 0, cx);
        reply(
            view,
            crate::app::sync::tests::response(
                &[("Songs", (100..200).filter(|index| *index != 149).collect())],
                None,
                true,
            ),
            cx,
        );
        assert_eq!(view.rows.len(), 199);
        assert!(
            view.page
                .tracks()
                .iter()
                .all(|track| track.id != format!("{:011}", 149))
        );
    })
}

#[test]
#[ignore = "requires desktop, private D-Bus and isolated runtime/data; synthetic accounts only"]
fn liking_a_song_refetches_cached_library_songs() -> Result<()> {
    check_cache(|view, window, cx| {
        let songs = Route::Library(Section::Songs);
        view.navigate(songs.clone(), cx);
        reply(view, page("Library Songs", "songs-tail"), cx);
        view.navigate(Route::Explore, cx);
        let track = view.page.tracks()[0].clone();
        confirm(
            view,
            window,
            Mutation::RateSong {
                track,
                rating: Rating::Like,
            },
            cx,
        );
        view.navigate(songs, cx);
        assert!(
            view.rows.is_empty(),
            "A like adds to Library Songs, so its old response must not be served"
        );
        assert!(matches!(
            reply(view, page("Updated Library Songs", "fresh-tail"), cx),
            Query::Library(Section::Songs)
        ));
    })
}

#[test]
#[ignore = "requires desktop, private D-Bus and isolated runtime/data; synthetic accounts only"]
fn rejected_optimistic_like_does_not_survive_navigation() -> Result<()> {
    check_cache(|view, window, cx| {
        let (commands, pending) = async_channel::unbounded();
        view.account_commands = commands;
        let route = Route::Browse(BrowseLink::new("VLrejected", "Rejected like"));
        view.navigate(route.clone(), cx);
        reply(view, page("Playlist", "playlist-tail"), cx);
        let track = view.page.tracks()[0].clone();
        view.toggle_like(&track, cx);
        assert_eq!(view.page.tracks()[0].details.rating, Some(Rating::Like));
        let account::Command::Apply { mutation, .. } = pending.try_recv().unwrap() else {
            panic!("Expected an intercepted rating")
        };
        view.navigate(Route::Explore, cx);
        view.account_event(
            account::Event::Applied {
                session: view.account.as_ref().unwrap().session.clone(),
                mutation,
                result: Err("Synthetic rejection".into()),
                session_rejected: false,
            },
            window,
            cx,
        );
        view.navigate(route, cx);
        assert!(
            view.rows.is_empty(),
            "The cached optimistic rating must be discarded after rollback"
        );
        reply(view, page("Playlist after rejection", "fresh-tail"), cx);
        assert_ne!(view.page.tracks()[0].details.rating, Some(Rating::Like));
    })
}

#[test]
#[ignore = "requires desktop, private D-Bus and isolated runtime/data; synthetic accounts only"]
fn guest_likes_and_saves_invalidate_cached_catalog_pages() -> Result<()> {
    check_cache(|view, _, cx| {
        view.account = None;
        view.clear_account_state(cx);
        for save in [false, true] {
            view.navigate(Route::Home, cx);
            reply(view, page("Guest Home", "home-tail"), cx);
            let track = view.page.tracks()[0].clone();
            view.navigate(Route::Explore, cx);
            reply(view, page("Guest Explore", "explore-tail"), cx);
            if save {
                view.save_collection(serde_json::from_value(serde_json::json!({"id":"MPREguest", "title":"Guest album", "subtitle":"Album"})).unwrap(), cx);
            } else {
                view.toggle_like(&track, cx);
            }
            view.navigate(Route::Home, cx);
            assert!(
                view.rows.is_empty(),
                "A guest library edit must discard older catalog pages too"
            );
            reply(view, page("Updated guest Home", "fresh-tail"), cx);
            view.forward_page.as_mut().unwrap().fetched_at = None;
            view.navigate(Route::Explore, cx);
        }
    })
}

#[test]
#[ignore = "requires desktop, private D-Bus and isolated runtime/data; synthetic accounts only"]
fn saved_song_refetches_cached_library_search_and_playlist() -> Result<()> {
    check_cache(|view, window, cx| {
        let search = Route::Search(Search {
            query: "saved songs".into(),
            filter: SearchFilter::Songs,
            scope: SearchScope::Library,
        });
        let playlist = Route::Browse(BrowseLink::new("VLunrelated", "Unrelated playlist"));
        for route in [&search, &playlist] {
            view.navigate(route.clone(), cx);
            reply(view, page("Before save", "old-tail"), cx);
        }
        view.navigate(
            Route::Search(Search {
                query: "new song".into(),
                filter: SearchFilter::Songs,
                scope: SearchScope::Catalog,
            }),
            cx,
        );
        reply(view, page("Search result", "search-tail"), cx);
        let track = view.page.tracks()[0].clone();
        assert!(view.page_cache.iter().any(|entry| entry.route == search));
        assert!(view.page_cache.iter().any(|entry| entry.route == playlist));
        view.account_event(
            account::Event::Applied {
                session: view.account.as_ref().unwrap().session.clone(),
                mutation: Box::new(Mutation::SongLibrary { track, save: true }),
                result: Ok(Effect::Changed),
                session_rejected: false,
            },
            window,
            cx,
        );
        assert_eq!(
            view.rows.len(),
            100,
            "Saving must keep the active search visible"
        );
        assert_eq!(view.page.tracks()[0].details.in_library, Some(true));
        assert!(view.catalog_requests.pending.is_empty());
        for route in [search, playlist] {
            view.navigate(route, cx);
            assert!(
                view.rows.is_empty(),
                "Every cached route must fetch after a save"
            );
            assert!(view.catalog_requests.is_loading(CatalogTarget::Page));
            reply(view, page("After save", "fresh-tail"), cx);
        }
    })
}

#[test]
#[ignore = "requires desktop, private D-Bus and isolated runtime/data; synthetic accounts only"]
fn playback_report_invalidates_cached_and_inflight_history_only() -> Result<()> {
    check_cache(|view, window, cx| {
        let playlist = Route::Browse(BrowseLink::new("VLretained", "Retained playlist"));
        view.navigate(playlist.clone(), cx);
        reply(view, page("Playlist", "playlist-tail"), cx);
        for watchtime in [false, true] {
            for route in [
                Route::Recent,
                Route::Library(Section::History),
                Route::Browse(Section::History.link()),
            ] {
                // Exercise both a loaded History and a reply issued before the report.
                for inflight in [false, true] {
                    view.navigate(route.clone(), cx);
                    if !inflight {
                        reply(view, page("History", "history-tail"), cx);
                    }
                    let generation = view.history_generation;
                    let current = view.account.as_ref().unwrap().session.clone();
                    for (session, result) in [
                        (current.clone(), Err("Synthetic report failure".into())),
                        (session("other-account"), Ok(())),
                    ] {
                        view.account_event(
                            account::Event::PlaybackReported {
                                session,
                                video_id: "00000000000".into(),
                                result,
                                session_rejected: false,
                                watchtime,
                                resume_seconds: None,
                            },
                            window,
                            cx,
                        );
                        assert_eq!(view.history_generation, generation);
                    }
                    // Keep active History's normal automatic revalidation out of this ordering test.
                    view.navigate(playlist.clone(), cx);
                    if inflight {
                        view.navigate(route.clone(), cx);
                    }
                    view.account_event(
                        account::Event::PlaybackReported {
                            session: current,
                            video_id: "00000000000".into(),
                            result: Ok(()),
                            session_rejected: false,
                            watchtime,
                            resume_seconds: None,
                        },
                        window,
                        cx,
                    );
                    if inflight {
                        reply(view, page("History issued before report", "old-tail"), cx);
                        view.navigate(playlist.clone(), cx);
                    }
                    assert_eq!(view.page.title, "Playlist");
                    assert!(
                        view.catalog_requests.pending.is_empty(),
                        "Playback reports must retain unrelated cached pages"
                    );
                    view.navigate(route.clone(), cx);
                    assert!(
                        view.rows.is_empty(),
                        "History from before a successful report must fetch on revisit"
                    );
                    reply(view, page("Fresh History", "fresh-tail"), cx);
                    view.forward_page.as_mut().unwrap().fetched_at = None;
                    view.navigate(playlist.clone(), cx);
                }
            }
        }
    })
}

#[test]
#[ignore = "requires desktop, private D-Bus and isolated runtime/data; synthetic accounts only"]
fn continuation_errors_are_not_forward_cached() -> Result<()> {
    check_cache(|view, _, cx| {
        let route = Route::Browse(BrowseLink::new("VLretry", "Retry playlist"));
        view.navigate(route.clone(), cx);
        reply(view, page("Playlist", "retry-tail"), cx);
        view.prefetch_page(100, 100, 0, cx);
        let job = view.catalog_requests.pending.try_recv().unwrap();
        view.catalog_result(
            CatalogResult {
                id: job.id,
                account_revision: job.account_revision,
                account_generation: job.account_generation,
                history_generation: job.history_generation,
                append: true,
                revalidate: false,
                result: Err("Synthetic continuation failure".into()),
                session_rejected: false,
                target: CatalogTarget::Page,
            },
            cx,
        );
        assert!(view.sync.page_error.is_some());
        view.navigate(Route::Explore, cx);
        view.navigate(route, cx);
        assert!(
            view.rows.is_empty(),
            "A page with a continuation error must fetch on forward navigation"
        );
        assert!(view.sync.page_error.is_none());
        assert!(view.error.is_none());
        if view.catalog_requests.is_loading(CatalogTarget::Page) {
            reply(view, page("Retry succeeded", "retry-tail"), cx);
        }
        view.prefetch_page(100, 100, 0, cx);
        reply(
            view,
            crate::app::sync::tests::response(&[("Songs", (100..200).collect())], None, true),
            cx,
        );
        assert_eq!(
            view.rows.len(),
            200,
            "Continuation loading must work again without an explicit reload"
        );
    })
}

#[test]
#[ignore = "requires desktop, private D-Bus and isolated runtime/data; synthetic accounts only"]
fn edited_loaded_and_inflight_pages_are_not_recached() -> Result<()> {
    check_cache(|view, window, cx| {
        let songs = Route::Browse(Section::Songs.link());
        for inflight in [false, true] {
            view.navigate(songs.clone(), cx);
            if !inflight {
                reply(view, page("Songs", "songs-tail"), cx);
            }
            let track = crate::app::sync::tests::page(1, false).tracks().remove(0);
            confirm(
                view,
                window,
                Mutation::SongLibrary { track, save: true },
                cx,
            );
            if inflight {
                reply(view, page("Old songs response", "songs-tail"), cx);
            }
            view.navigate(Route::Explore, cx);
            view.go_back(cx);
            assert_eq!(
                view.rows.len(),
                100,
                "Back must retain even an unpatchable loaded response"
            );
            assert!(matches!(
                reply(view, page("Revalidated songs", "songs-tail"), cx),
                Query::Revalidate(_)
            ));
            view.navigate(Route::Explore, cx);
            view.navigate(songs.clone(), cx);
            assert!(
                view.rows.is_empty(),
                "An unpatchable active page must not become a forward cached hit after leaving"
            );
            reply(view, page("Songs after save", "songs-tail"), cx);
            // Remove this valid entry before exercising the initial response in flight.
            view.forward_page.as_mut().unwrap().fetched_at = None;
            view.navigate(Route::Explore, cx);
        }

        let playlist = Route::Browse(BrowseLink::new("VLmoving", "Moving playlist"));
        view.navigate(playlist.clone(), cx);
        reply(view, page("Playlist", "playlist-tail"), cx);
        confirm(
            view,
            window,
            Mutation::MoveInPlaylist {
                id: "moving".into(),
                set_video_id: "occurrence-0".into(),
                before: Some("unloaded-occurrence".into()),
            },
            cx,
        );
        view.prefetch_page(100, 100, 0, cx);
        reply(
            view,
            crate::app::sync::tests::response(
                &[("Songs", (100..200).collect())],
                Some("deeper-tail"),
                true,
            ),
            cx,
        );
        view.navigate(Route::Explore, cx);
        view.navigate(playlist, cx);
        assert!(
            view.rows.is_empty(),
            "A continuation reply must not make an unpatchable playlist cacheable again"
        );
        reply(view, page("Playlist after move", "fresh-tail"), cx);
    })
}

#[test]
#[ignore = "requires desktop, private D-Bus and isolated runtime/data; set YT_MUSIC_NATIVE_TESTS=1; synthetic accounts only"]
fn native_navigation_reuses_pages_and_preserves_account_isolation() -> Result<()> {
    super::isolate()?;
    let passed = Arc::new(AtomicBool::new(false));
    let result = passed.clone();
    gpui_platform::application().with_assets(Assets).run(move |cx| {
        cx.text_system().add_fonts(vec![std::borrow::Cow::Borrowed(include_bytes!("../../../assets/fonts/Roboto.ttf").as_slice())]).unwrap();
        crate::input::init(cx);
        let window = cx.open_window(WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::new(point(px(40.), px(40.)), size(px(1280.), px(800.))))),
            kind: WindowKind::PopUp, focus: true, ..Default::default()
        }, |window, cx| cx.new(|cx| {
            let mut view = Music::new(Startup::Guest, window, cx);
            view.account_commands.close();
            view.account_worker.take().unwrap().finish().unwrap();
            for target in CatalogTarget::ALL { view.catalog_requests.cancel(target); }
            view.catalog_requests = CatalogRequests::new();
            for generation in view.catalog_requests.generations.iter() { generation.store(1000, Ordering::Relaxed); }
            view.track_state = track_state::Worker::idle();
            view.player = None;
            view.video = None;
            view.mpris = None;
            view.store = None;
            view.library.volume = 0.;
            view.navigate(Route::Home, cx);
            reply(&mut view, page("Home recommendations", "home-tail"), cx);
            view
        })).unwrap();
        cx.activate(true);
        cx.spawn(async move |cx| {
            let playlist = Route::Browse(BrowseLink::new("VLcached", "Cached playlist"));
            wait_for_presented_frame(window, cx).await;
            window.update(cx, |view, _, cx| {
                let fetched_at = view.sync.page_fetched_at;
                view.navigate(Route::Explore, cx);
                reply(view, page("Explore", "explore-tail"), cx);
                view.navigate(Route::Home, cx);
                assert_eq!(view.page.title, "Home recommendations");
                assert_eq!(view.rows.len(), 100, "Fresh pages must be available synchronously");
                assert_eq!(view.sync.page_fetched_at, fetched_at);
                assert!(view.catalog_requests.pending.is_empty(), "Fresh Home must not reshuffle recommendations");
                assert!(!view.catalog_requests.is_loading(CatalogTarget::Page));
                view.navigate(Route::Home, cx);
                assert!(view.catalog_requests.pending.is_empty(), "Clicking the current route also uses its response");
                view.navigate(Route::Browse(BrowseLink::new("FEmusic_home", "Home link")), cx);
                assert_eq!(view.page.title, "Home recommendations");
                assert!(view.catalog_requests.pending.is_empty(), "An unfiltered Home link shares the primary Home cache entry");
                assert!(matches!(&view.route, Route::Browse(link) if link.id == "FEmusic_home"), "Cache aliases must not rewrite the route recorded for Back");
            }).unwrap();
            wait_for_presented_frame(window, cx).await;
            window.update(cx, |view, window, cx| {
                assert_eq!(view.page.title, "Home recommendations");
                super::render::capture(window, "cached-home-first-frame");
                view.account = Some(connection(session("first")));
                view.clear_account_state(cx);
                view.navigate(playlist.clone(), cx);
                reply(view, page("Cached playlist", "playlist-tail"), cx);
                let fetched_at = Instant::now() - Duration::from_secs(181);
                view.forward_page.as_mut().unwrap().fetched_at = Some(fetched_at);
                let token = view.page.continuation.clone().unwrap();
                view.request(Query::More { token, search: None }, cx);
                reply(view, crate::app::sync::tests::response(&[("Songs", (100..150).collect())], Some("deeper-tail"), false), cx);
                assert_eq!(view.forward_page.as_ref().unwrap().fetched_at, Some(fetched_at), "Loading a continuation must not renew the first page's cache age");
                view.list_scroll.scroll_to_item_strict(80, ScrollStrategy::Top);
                cx.notify();
            }).unwrap();
            wait_for_presented_frame(window, cx).await;
            window.update(cx, |view, window, cx| {
                let track = view.page.tracks()[0].clone();
                confirm(view, window, Mutation::RateSong { track, rating: Rating::Like }, cx);
                view.navigate(Route::Explore, cx);
                view.navigate(playlist.clone(), cx);
                assert!(view.rows.is_empty(), "The reply issued before the rating must not be cached");
                reply(view, page("Fresh playlist", "fresh-tail"), cx);
                assert_eq!(view.list_scroll.0.borrow().base_handle.offset().y, px(0.), "Forward navigation resets scroll");
                view.forward_page.as_mut().unwrap().fetched_at = Some(Instant::now() - Duration::from_secs(181));
                view.navigate(Route::Explore, cx);
                view.navigate(playlist.clone(), cx);
                assert_eq!(view.rows.len(), 100, "Stale forward navigation shows its page synchronously");
                reply(view, page("Revisited playlist", "fresh-tail"), cx);
                assert_eq!(view.rows.len(), 100, "Forward refresh replaces expired continuation pages");
                view.navigate(Route::Explore, cx);
                confirm(view, window, Mutation::RemoveFromPlaylist { id: "VLcached".into(), set_video_id: "occurrence-0".into(), video_id: "00000000000".into() }, cx);
                view.navigate(playlist.clone(), cx);
                assert!(view.rows.is_empty(), "A playlist with invalidated order or continuations must be evicted");
                assert!(view.catalog_requests.is_loading(CatalogTarget::Page));
                reply(view, page("After edit", "edit-tail"), cx);
                view.navigate(Route::Explore, cx);
                reply(view, page("Explore", "explore-tail"), cx);
                confirm(view, window, Mutation::DeletePlaylist { id: "VLcached".into() }, cx);
                view.navigate(playlist, cx);
                assert!(view.rows.is_empty(), "Deleted playlists must be evicted");
                reply(view, page("Replacement", "replacement-tail"), cx);

                let mut filtered = BrowseLink::new("FEmusic_charts", "Filtered charts");
                filtered.params = Some("workout".into());
                filtered.chart_country = Some("US".into());
                let mut other = filtered.clone();
                other.params = Some("relax".into());
                for (link, title, token) in [(&filtered, "Workout", "workout-tail"), (&other, "Relax", "relax-tail")] {
                    view.navigate(Route::Browse(link.clone()), cx);
                    assert!(view.rows.is_empty(), "Different browse params must not share content");
                    reply(view, page(title, token), cx);
                }
                view.navigate(Route::Browse(filtered.clone()), cx);
                assert_eq!(view.page.title, "Workout");
                assert_eq!(view.page.continuation.as_ref().unwrap().token(), "workout-tail");
                assert!(view.catalog_requests.pending.is_empty());
                let mut renamed = filtered.clone();
                renamed.title = "Another label for Workout".into();
                renamed.accent = Some(0xff0000);
                view.navigate(Route::Browse(renamed), cx);
                assert_eq!(view.page.title, "Workout");
                assert!(view.catalog_requests.pending.is_empty(), "Display labels and colors must not change the request's cache identity");
                filtered.chart_country = Some("CA".into());
                view.navigate(Route::Browse(filtered), cx);
                assert!(view.rows.is_empty(), "Country filters are part of the route");
                reply(view, page("Canada", "canada-tail"), cx);
                for filter in [SearchFilter::Songs, SearchFilter::Albums] {
                    view.navigate(Route::Search(Search { query: "same query".into(), filter, scope: SearchScope::Catalog }), cx);
                    assert!(view.rows.is_empty(), "Search filters must not collide");
                    reply(view, page("Search", "search-tail"), cx);
                }
                view.navigate(Route::Search(Search { query: "same query".into(), filter: SearchFilter::Songs, scope: SearchScope::Library }), cx);
                assert!(view.rows.is_empty(), "Search scopes must not collide");
                reply(view, page("Library search", "library-tail"), cx);

                view.navigate(Route::Library(Section::Songs), cx);
                reply(view, page("Songs", "sort-request"), cx);
                let sort = view.page.continuation.clone().unwrap();
                view.request(Query::Reload(sort), cx);
                reply(view, page("Sorted songs", "sorted-tail"), cx);
                view.navigate(Route::Explore, cx);
                reply(view, page("Explore", "explore-tail"), cx);
                view.navigate(Route::Library(Section::Songs), cx);
                assert_eq!(view.page.title, "Sorted songs");
                assert_eq!(view.page.continuation.as_ref().unwrap().token(), "sorted-tail");
                assert!(view.catalog_requests.pending.is_empty());
                view.reload_current_page(cx);
                let job = view.catalog_requests.pending.try_recv().unwrap();
                assert!(matches!(job.query, Query::Revalidate(query) if matches!(&*query, Query::Library(Section::Songs))), "Explicit reload keeps main's route revalidation behavior");
                while view.catalog_requests.pending.try_recv().is_ok() {}
                view.navigate(Route::Saved, cx);
                reply(view, page("Saved collections", "saved-tail"), cx);
                view.navigate(Route::Explore, cx);
                let collection = serde_json::from_value(serde_json::json!({"id":"MPREnew", "title":"New album", "subtitle":"Album"})).unwrap();
                confirm(view, window, Mutation::CollectionLibrary { collection, save: true }, cx);
                view.navigate(Route::Saved, cx);
                assert!(view.rows.is_empty(), "Unpatchable library membership must be evicted before display");
                assert!(view.catalog_requests.is_loading(CatalogTarget::Page), "Saving a new collection must invalidate cached library membership");
                reply(view, page("Updated collections", "updated-tail"), cx);

                view.account = Some(connection(session("second")));
                view.clear_account_state(cx);
                assert!(view.page_cache.is_empty());
                view.navigate(Route::Home, cx);
                assert!(view.rows.is_empty(), "Another account must not see the guest or first account's cached Home");
                reply(view, page("Second account", "second-tail"), cx);
                view.account = None;
                view.clear_account_state(cx);
                view.navigate(Route::Home, cx);
                assert!(view.rows.is_empty(), "Sign-out must not restore the account's Home");
                reply(view, page("Guest again", "guest-tail"), cx);

                for index in 0..14 {
                    view.navigate(Route::Browse(BrowseLink::new(format!("VLlru{index}"), "LRU")), cx);
                    reply(view, page(&format!("Page {index}"), "lru-tail"), cx);
                    assert!(view.page_cache.len() <= 12);
                }
                view.navigate(Route::Browse(BrowseLink::new("VLlru1", "LRU")), cx);
                assert_eq!(view.page.title, "Page 1", "The least recent cached page must survive the outgoing page's insertion");
                assert!(view.catalog_requests.pending.is_empty());
                view.navigate(Route::Browse(BrowseLink::new("VLlru0", "LRU")), cx);
                assert!(view.rows.is_empty(), "The oldest page must be evicted at the cache limit");
                reply(view, page("Evicted page reloaded", "new-tail"), cx);
            }).unwrap();
            wait_for_presented_frame(window, cx).await;
            result.store(true, Ordering::Relaxed);
            cx.update(|cx| cx.quit());
        }).detach();
    });
    ensure!(
        passed.load(Ordering::Relaxed),
        "Navigation checks did not complete"
    );
    Ok(())
}

#[test]
#[ignore = "requires desktop, private D-Bus and isolated runtime/data; synthetic accounts only"]
fn back_owns_each_visits_loaded_page_and_ignores_forward_cache() -> Result<()> {
    check_cache(|view, window, cx| {
        let route = Route::Browse(BrowseLink::new("VLsnapshots", "Snapshots"));
        view.navigate(route.clone(), cx);
        reply(view, page("First visit", "first-tail"), cx);
        view.prefetch_page(100, 100, 0, cx);
        reply(
            view,
            crate::app::sync::tests::response(
                &[("Songs", (100..200).collect())],
                Some("loaded-tail"),
                true,
            ),
            cx,
        );
        view.navigate(Route::Explore, cx);
        confirm(
            view,
            window,
            Mutation::RemoveFromPlaylist {
                id: "snapshots".into(),
                set_video_id: "occurrence-149".into(),
                video_id: format!("{:011}", 149),
            },
            cx,
        );
        view.navigate(route, cx);
        let mut second = crate::app::sync::tests::page(50, true);
        second.title = "Second visit".into();
        reply(view, second, cx);
        view.navigate(Route::Explore, cx);
        view.page_cache.clear();
        view.go_back(cx);
        assert_eq!(view.page.title, "Second visit");
        assert_eq!(view.rows.len(), 50);
        assert!(view.catalog_requests.is_loading(CatalogTarget::Page));
        view.go_back(cx);
        view.go_back(cx);
        assert_eq!(view.page.title, "First visit");
        assert_eq!(
            view.rows.len(),
            199,
            "Each visit retains its own loaded tail"
        );
        assert!(
            view.page
                .tracks()
                .iter()
                .all(|track| track.id != format!("{:011}", 149)),
            "Confirmed removals patch stashed tails"
        );
        assert_eq!(
            view.page.continuation.as_ref().unwrap().token(),
            "loaded-tail"
        );
        assert!(
            view.page_cache.is_empty(),
            "Back never populates the forward cache"
        );
        let job = view.catalog_requests.pending.try_recv().unwrap();
        assert!(
            matches!(job.query, Query::Revalidate(_)),
            "Signed-in Back always revalidates as on main"
        );
        view.catalog_result(
            CatalogResult {
                id: job.id,
                account_revision: job.account_revision,
                account_generation: job.account_generation,
                history_generation: job.history_generation,
                append: false,
                revalidate: true,
                result: Err("Synthetic revalidation error".into()),
                session_rejected: false,
                target: CatalogTarget::Page,
            },
            cx,
        );
        view.navigate(Route::Explore, cx);
        view.go_back(cx);
        assert_eq!(
            view.rows.len(),
            199,
            "A catalog error must not discard Back content"
        );

        let incomplete = Route::Browse(BrowseLink::new("VLincomplete", "Incomplete"));
        view.navigate(incomplete.clone(), cx);
        view.navigate(Route::Explore, cx);
        view.page_cache.push_back(crate::app::CachedPage {
            route: incomplete,
            account_revision: view.account_revision,
            account_generation: view.account_generation,
            history_generation: view.history_generation,
            page: page("Must not restore from forward cache", "wrong-tail"),
            fetched_at: Instant::now(),
            coverage: Default::default(),
            sort: None,
        });
        let cache_len = view.page_cache.len();
        view.go_back(cx);
        assert!(
            view.rows.is_empty(),
            "Back without a snapshot fetches fresh as on main"
        );
        assert_eq!(
            view.page_cache.len(),
            cache_len,
            "Back never consumes forward cache entries"
        );
        assert!(matches!(
            reply(view, page("Fetched incomplete", "fresh-tail"), cx),
            Query::Browse(_)
        ));
    })
}

#[test]
#[ignore = "requires desktop, private D-Bus and isolated runtime/data; synthetic accounts only"]
fn refresh_after_back_supersedes_the_forward_cache_entry() -> Result<()> {
    check_cache(|view, _, cx| {
        let route = Route::Browse(BrowseLink::new("VLrefreshed", "Refreshed"));
        view.navigate(route.clone(), cx);
        reply(view, page("Original", "original-tail"), cx);
        view.navigate(Route::Home, cx);
        reply(view, page("Home", "home-tail"), cx);
        view.go_back(cx);
        reply(view, page("Refreshed", "refreshed-tail"), cx);
        assert_eq!(view.page.title, "Refreshed");
        view.navigate(Route::Home, cx);
        view.navigate(route, cx);
        assert_ne!(
            view.page.title, "Original",
            "An older forward entry must not undo content refreshed after Back"
        );
        assert!(view.catalog_requests.is_loading(CatalogTarget::Page));
    })
}
