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

fn reply_playlist_metadata(view: &mut Music, job: super::super::Job, cx: &mut Context<Music>) {
    assert!(job.query.target() == CatalogTarget::PlaylistMetadata);
    let mut page = view.page.clone();
    page.header = crate::catalog::parse_page(
        &serde_json::json!({"header":{"musicResponsiveHeaderRenderer":{
            "title":{"simpleText":"Destination"},
            "secondSubtitle":{"simpleText":"100 tracks"}
        }},"contents":[]}),
        "Playlist",
    )
    .unwrap()
    .header;
    view.catalog_result(
        CatalogResult {
            id: job.id,
            account_revision: job.account_revision,
            account_generation: job.account_generation,
            history_generation: job.history_generation,
            append: false,
            revalidate: false,
            result: Ok(page),
            session_rejected: false,
            target: CatalogTarget::PlaylistMetadata,
        },
        cx,
    );
}

pub(super) fn confirm(
    view: &mut Music,
    window: &mut Window,
    mutation: Mutation,
    cx: &mut Context<Music>,
) {
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
                            view.media_controls = None;
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
                    panic!()
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
                            toast: None,
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
                    panic!()
                };
                saved.details.in_library = Some(true);
                assert!(matches!(reply(view, fresh, cx), Query::Revalidate(_)));
                assert_eq!(view.page.tracks()[0].details.in_library, Some(true));
                assert!(
                    view.page.tracks().iter().any(|item| item.id == track.id),
                    "The server response must make the successfully added song visible"
                );

                let metadata = view.catalog_requests.pending.try_recv().unwrap();
                reply_playlist_metadata(view, metadata, cx);

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
                assert!(older_read.query.target() == CatalogTarget::Page);
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
                            toast: None,
                        }),
                        session_rejected: false,
                    },
                    window,
                    cx,
                );
                let requests: Vec<_> =
                    std::iter::from_fn(|| view.catalog_requests.pending.try_recv().ok()).collect();
                let pages:Vec<_> = requests.iter().filter(|job| job.query.target()==CatalogTarget::Page).collect();
                assert_eq!(pages.len(), 1, "Missing song metadata must supersede the older page read");
                assert!(pages[0].id > older_read.id);
                assert!(matches!(pages[0].query, Query::Revalidate(_)));
                assert!(requests.iter().any(|job| matches!(&job.query, Query::PlaylistMetadata(refresh) if refresh.id=="destination")));
                view.catalog_requests.cancel(CatalogTarget::Page);
                let metadata = requests.into_iter().find(|job| job.query.target() == CatalogTarget::PlaylistMetadata).unwrap();
                reply_playlist_metadata(view, metadata, cx);

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
                            toast: None,
                        }),
                        session_rejected: false,
                    },
                    window,
                    cx,
                );
                let requests:Vec<_> = std::iter::from_fn(|| view.catalog_requests.pending.try_recv().ok()).collect();
                assert!(!requests.iter().any(|job| job.query.target()==CatalogTarget::Page), "An inactive destination needs no page read");
                assert!(requests.iter().any(|job| matches!(&job.query, Query::PlaylistMetadata(refresh) if refresh.id=="destination")), "Its guide details still refresh");
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
        view.forward_page.as_mut().unwrap().fetched_at = Some(
            Instant::now()
                .checked_sub(Duration::from_secs(181))
                .unwrap(),
        );
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
fn explore_pages_keep_their_titled_shelves() -> Result<()> {
    check_cache(|view, _, cx| {
        use crate::app::ListRow;
        use crate::app::sync::tests::response;
        let sections = response(
            &[
                ("Video charts", (0..4).collect()),
                ("Top songs", (4..8).collect()),
                ("", (8..10).collect()),
            ],
            None,
            false,
        );
        view.navigate(
            Route::Browse(BrowseLink::new("FEmusic_charts", "Charts")),
            cx,
        );
        reply(view, sections, cx);
        assert!(
            matches!(
                view.list_rows.as_slice(),
                [
                    ListRow::Shelf(0),
                    ListRow::Shelf(1),
                    ListRow::Item(8),
                    ListRow::Item(9)
                ]
            ),
            "Titled shelves show as sections and an untitled one as plain rows"
        );
        view.navigate(Route::Browse(BrowseLink::new("FEmusic_more", "More")), cx);
        reply(view, page("More", "more-tail"), cx);
        assert!(
            !view.grouped_rows(),
            "A single shelf keeps the plain list or grid"
        );
        let token = view.page.continuation.clone().unwrap();
        view.request(
            Query::More {
                token,
                search: None,
            },
            cx,
        );
        reply(
            view,
            response(&[("Albums", (100..104).collect())], None, false),
            cx,
        );
        assert!(
            matches!(
                view.list_rows.as_slice(),
                [ListRow::Shelf(0), ListRow::Shelf(1)]
            ),
            "A continuation that adds a second titled shelf regroups the whole page"
        );
        assert_eq!(
            view.detail_list.state.item_count(),
            view.list_rows.len(),
            "The section list must hold every regrouped row"
        );
    })
}

#[test]
#[ignore = "requires desktop, private D-Bus and isolated runtime/data; synthetic accounts only"]
fn history_buttons_restore_visited_pages() -> Result<()> {
    check_cache_window(async |window, cx| {
        let navigate = |direction, window: gpui::WindowHandle<Music>, cx: &mut gpui::AsyncApp| {
            cx.update_window(window.into(), |_, window, cx| {
                let position = point(px(640.), px(400.));
                let button = gpui::MouseButton::Navigate(direction);
                window.dispatch_event(
                    gpui::PlatformInput::MouseDown(gpui::MouseDownEvent {
                        button,
                        position,
                        modifiers: Default::default(),
                        click_count: 1,
                        first_mouse: false,
                    }),
                    cx,
                );
                window.dispatch_event(
                    gpui::PlatformInput::MouseUp(gpui::MouseUpEvent {
                        button,
                        position,
                        modifiers: Default::default(),
                        click_count: 1,
                    }),
                    cx,
                );
            })
            .unwrap();
        };
        let playlist = Route::Browse(BrowseLink::new("VLback", "Back target"));
        window
            .update(cx, |view, _, cx| {
                view.navigate(playlist.clone(), cx);
                reply(view, page("Back target", "back-tail"), cx);
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
            })
            .unwrap();
        wait_for_presented_frame(window, cx).await;
        navigate(gpui::NavigationDirection::Back, window, cx);
        wait_for_presented_frame(window, cx).await;
        window
            .update(cx, |view, window, cx| {
                assert!(matches!(view.route, Route::Explore));
                confirm(
                    view,
                    window,
                    Mutation::RemoveFromPlaylist {
                        id: "back".into(),
                        set_video_id: "occurrence-149".into(),
                        video_id: format!("{:011}", 149),
                    },
                    cx,
                );
            })
            .unwrap();
        window
            .update(cx, |view, _, cx| {
                view.overlay = Some(crate::app::Overlay::Help);
                cx.notify();
            })
            .unwrap();
        wait_for_presented_frame(window, cx).await;
        navigate(gpui::NavigationDirection::Forward, window, cx);
        window
            .update(cx, |view, _, cx| {
                assert!(
                    matches!(view.route, Route::Explore),
                    "The dialog blocks mouse Forward"
                );
                view.overlay = None;
                cx.notify();
            })
            .unwrap();
        wait_for_presented_frame(window, cx).await;
        navigate(gpui::NavigationDirection::Forward, window, cx);
        wait_for_presented_frame(window, cx).await;
        window
            .update(cx, |view, _, _| {
                assert!(view.route == playlist, "Mouse Forward restores the visit");
                assert_eq!(
                    view.rows.len(),
                    199,
                    "Forward keeps the loaded tail with confirmed edits"
                );
                assert_eq!(
                    view.page.continuation.as_ref().unwrap().token(),
                    "loaded-tail"
                );
                assert!(
                    view.page
                        .tracks()
                        .iter()
                        .all(|track| track.id != format!("{:011}", 149))
                );
            })
            .unwrap();

        super::render::key(window, cx, "alt-left").await;
        window
            .update(cx, |view, _, _| {
                assert!(matches!(view.route, Route::Explore))
            })
            .unwrap();
        super::render::key(window, cx, "alt-right").await;
        window
            .update(cx, |view, _, _| assert!(view.route == playlist))
            .unwrap();

        super::render::key(window, cx, "alt-left").await;
        window
            .update(cx, |view, _, cx| {
                view.navigate(Route::Library(Section::Songs), cx);
                reply(view, page("Songs", "songs-tail"), cx);
            })
            .unwrap();
        super::render::key(window, cx, "alt-right").await;
        window
            .update(cx, |view, _, _| {
                assert!(matches!(view.route, Route::Library(Section::Songs)));
            })
            .unwrap();
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
            view.media_controls = None;
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
                assert!(view.catalog_requests.is_loading(CatalogTarget::Page), "Like the web, clicking the current route loads it again");
                reply(view, page("Home recommendations", "home-tail"), cx);
                view.navigate(Route::Explore, cx);
                assert!(view.catalog_requests.pending.is_empty());
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
                let fetched_at = Instant::now().checked_sub(Duration::from_secs(181)).unwrap();
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
                view.forward_page.as_mut().unwrap().fetched_at = Some(Instant::now().checked_sub(Duration::from_secs(181)).unwrap());
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
                view.navigate(Route::Browse(BrowseLink::new("VLspacer", "Spacer")), cx);
                reply(view, page("Spacer", "spacer-tail"), cx);
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
        reply(view, page("Current", "current-tail"), cx);

        // Leaving the refreshed page through Back must not resurrect an older entry either.
        let playlist = Route::Browse(BrowseLink::new("VLrefreshed-back", "Refreshed"));
        view.navigate(playlist.clone(), cx);
        reply(view, page("Original", "original-tail"), cx);
        view.navigate(Route::Explore, cx);
        view.go_back(cx);
        reply(view, page("Refreshed", "refreshed-tail"), cx);
        view.go_back(cx);
        view.navigate(playlist, cx);
        assert_ne!(
            view.page.title, "Original",
            "Back must not leave an entry older than the content last shown"
        );
    })
}

#[test]
#[ignore = "requires desktop, private D-Bus and isolated runtime/data; synthetic accounts only"]
fn route_revalidation_forgets_the_previous_sort() -> Result<()> {
    check_cache(|view, _, cx| {
        view.navigate(Route::Library(Section::Songs), cx);
        reply(view, page("Songs", "sort-request"), cx);
        let sort = view.page.continuation.clone().unwrap();
        view.request(Query::Reload(sort), cx);
        reply(view, page("Sorted songs", "sorted-tail"), cx);
        // Returning to the window after three minutes revalidates the route in its default order.
        view.revalidate_page(cx);
        reply(view, page("Default order", "default-tail"), cx);
        view.navigate(Route::Explore, cx);
        for entry in &mut view.page_cache {
            entry.fetched_at = Instant::now()
                .checked_sub(Duration::from_secs(181))
                .unwrap();
        }
        view.navigate(Route::Library(Section::Songs), cx);
        let job = view.catalog_requests.pending.try_recv().unwrap();
        assert!(
            matches!(job.query, Query::Library(Section::Songs)),
            "A stale revisit must refresh in the order last shown"
        );
    })
}

const COVERS: [(&str, &str, &str); 5] = [
    (
        "Road trip",
        "Synthetic account • 42 songs",
        "https://yt3.googleusercontent.com/xmMiZ8JLzb6np1_Ax5DdLNK9RaSmkQynVbiVApVbmfC-2fb8-9WErSRKOjeE2eDITYYlhaR9Vo9h=s192",
    ),
    (
        "Late night",
        "Synthetic account • 18 songs",
        "https://yt3.googleusercontent.com/utMZxtgiJjr_Gz-dHQv551OQC8L4LqjHT3zY92H28i1sEU0gsMjbLX6A0GAhnK5bSwTFX93tGlw=s192",
    ),
    (
        "Focus",
        "Synthetic account • 7 songs",
        "https://yt3.googleusercontent.com/9E2BdLFXxqJtwG4DvuTUjYVZXB-B4EC-ZcVvLRdRH3ynbYO89O-dAdjXDRmBdo3skeJN_OLD8Zn-=s192",
    ),
    (
        "Gym",
        "Synthetic account • 63 songs",
        "https://yt3.ggpht.com/jzUumXHKi5NJg8DSfKxI1s4WZsglrXlY4Iu-5KmfkuEIx6ZZh1v1aBA-iwEIDuVVm37jvzSqY8Q=s192",
    ),
    (
        "Sunday morning",
        "Synthetic account • 12 songs",
        "https://i.ytimg.com/vi/lRaXPHFEbKA/hqdefault.jpg?sqp=-oaymwEWCJADEOEBIAQqCggAEOADGC0guwJIWg&rs=AMzJL3mXdC-OEXtSBi5e2TMREUfH2T8JxQ",
    ),
];

#[test]
#[ignore = "requires desktop, private D-Bus and isolated runtime/data; synthetic accounts only"]
fn keyboard_reaches_offscreen_save_destinations() -> Result<()> {
    check_cache_window(async |window, cx| {
        let submitted = window
            .update(cx, |view, window, cx| {
                let (commands, submitted) = async_channel::bounded(1);
                view.account_commands = commands;
                view.sidebar_playlists = (0..40)
                    .map(|index| crate::catalog::Collection {
                        id: format!("VLPLkeyboard-{index}"),
                        title: format!("Playlist {index:02}"),
                        subtitle: String::new(),
                        thumbnail: None,
                        details: crate::catalog::CollectionDetails {
                            editable: true,
                            ..Default::default()
                        },
                    })
                    .collect();
                let tracks = view.page.tracks()[..2].to_vec();
                window.focus(&view.overlay_focus, cx);
                view.playlist_picker(tracks, cx);
                submitted
            })
            .unwrap();
        wait_for_presented_frame(window, cx).await;
        // Close is the first stop, then the playlist destinations in order.
        for _ in 0..18 {
            super::render::key(window, cx, "tab").await;
        }
        super::render::key(window, cx, "shift-tab").await;
        super::render::key(window, cx, "enter").await;
        let account::Command::Apply { mutation, .. } = submitted
            .try_recv()
            .expect("Enter must select the destination reached with Tab")
        else {
            panic!("Expected the add-to-playlist command");
        };
        assert!(
            matches!(&*mutation, Mutation::AddToPlaylist { id, .. } if id == "PLkeyboard-15"),
            "Tab and Shift+Tab must reach the expected offscreen destination. Got {:?}",
            mutation.key(),
        );

        window
            .update(cx, |view, window, cx| {
                let tracks = view.page.tracks()[..2].to_vec();
                window.focus(&view.overlay_focus, cx);
                view.playlist_picker(tracks, cx);
            })
            .unwrap();
        wait_for_presented_frame(window, cx).await;
        super::render::key(window, cx, "tab").await;
        super::render::key(window, cx, "tab").await;
        window
            .update(cx, |view, window, cx| {
                let job = playlist_choices_job(view);
                let mut destinations = view.playlist_picker_choices().rows(0..40);
                destinations.rotate_left(1);
                view.playlist_choices_result(
                    crate::app::PlaylistChoicesResult {
                        job,
                        result: Ok(crate::playlist_choices::Choices {
                            destinations,
                            empty: None,
                        }),
                        session_rejected: false,
                    },
                    window,
                    cx,
                );
            })
            .unwrap();
        wait_for_presented_frame(window, cx).await;
        super::render::key(window, cx, "enter").await;
        let account::Command::Apply { mutation, .. } = submitted
            .try_recv()
            .expect("Enter must retain its destination after a reorder")
        else {
            panic!("Expected the add-to-playlist command");
        };
        assert!(
            matches!(&*mutation, Mutation::AddToPlaylist { id, .. } if id == "PLkeyboard-0"),
            "A refreshed list must not move focus to a different playlist"
        );
    })
}

#[test]
#[ignore = "requires desktop, private D-Bus and isolated runtime/data; synthetic accounts only"]
fn refreshing_save_destinations_does_not_retarget_a_mouse_click() -> Result<()> {
    use gpui::{MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PlatformInput};

    check_cache_window(async |window, cx| {
        let submitted = window
            .update(cx, |view, window, cx| {
                let (commands, submitted) = async_channel::bounded(1);
                view.account_commands = commands;
                view.sidebar_playlists = ["First", "Second"]
                    .into_iter()
                    .map(|name| crate::catalog::Collection {
                        id: format!("VLPL{name}"),
                        title: name.into(),
                        subtitle: String::new(),
                        thumbnail: None,
                        details: crate::catalog::CollectionDetails {
                            editable: true,
                            ..Default::default()
                        },
                    })
                    .collect();
                window.focus(&view.overlay_focus, cx);
                view.playlist_picker(view.page.tracks()[..2].to_vec(), cx);
                submitted
            })
            .unwrap();
        wait_for_presented_frame(window, cx).await;
        let position = window
            .update(cx, |view, _, _| {
                let scroll = view.picker_scroll.0.borrow();
                let bounds = scroll.base_handle.bounds();
                let row_height = scroll.last_item_size.unwrap().item.height;
                point(bounds.center().x, bounds.top() + row_height / 2.)
            })
            .unwrap();
        let dispatch = |event, cx: &mut gpui::AsyncApp| {
            cx.update_window(window.into(), |_, window, cx| {
                window.dispatch_event(event, cx)
            })
            .unwrap();
        };
        let down = || {
            PlatformInput::MouseDown(MouseDownEvent {
                button: MouseButton::Left,
                position,
                modifiers: Default::default(),
                click_count: 1,
                first_mouse: false,
            })
        };
        let up = || {
            PlatformInput::MouseUp(MouseUpEvent {
                button: MouseButton::Left,
                position,
                modifiers: Default::default(),
                click_count: 1,
            })
        };
        dispatch(
            PlatformInput::MouseMove(MouseMoveEvent {
                position,
                pressed_button: None,
                modifiers: Default::default(),
            }),
            cx,
        );
        dispatch(down(), cx);
        wait_for_presented_frame(window, cx).await;
        window
            .update(cx, |view, window, cx| {
                let job = playlist_choices_job(view);
                let mut destinations = view.playlist_picker_choices().rows(0..2);
                destinations.swap(0, 1);
                view.playlist_choices_result(
                    crate::app::PlaylistChoicesResult {
                        job,
                        result: Ok(crate::playlist_choices::Choices {
                            destinations,
                            empty: None,
                        }),
                        session_rejected: false,
                    },
                    window,
                    cx,
                );
            })
            .unwrap();
        wait_for_presented_frame(window, cx).await;
        dispatch(up(), cx);
        assert!(
            submitted.is_empty(),
            "Releasing on a different destination must not save"
        );
        wait_for_presented_frame(window, cx).await;
        dispatch(down(), cx);
        dispatch(up(), cx);
        let account::Command::Apply { mutation, .. } = submitted
            .try_recv()
            .expect("A new click must select the refreshed destination")
        else {
            panic!("Expected the add-to-playlist command");
        };
        assert!(matches!(&*mutation, Mutation::AddToPlaylist { id, .. } if id == "PLSecond"));
    })
}

#[test]
#[ignore = "requires desktop, private D-Bus and isolated runtime/data; synthetic accounts only"]
fn playlist_picker_scrolls_its_full_width_without_moving_the_background() -> Result<()> {
    use gpui::{
        MouseButton, MouseDownEvent, PlatformInput, ScrollDelta, ScrollWheelEvent, TouchPhase,
    };

    check_cache_window(async |window, cx| {
        window
            .update(cx, |view, _, cx| {
                view.navigate(Route::Library(Section::Songs), cx);
                reply(view, page("Songs", "songs-tail"), cx);
                view.sidebar_playlists = (0..40)
                    .map(|index| crate::catalog::Collection {
                        id: format!("VLscroll{index}"),
                        title: format!("Playlist {index:02}"),
                        subtitle: "Synthetic account • 30 songs".into(),
                        thumbnail: None,
                        details: crate::catalog::CollectionDetails {
                            editable: true,
                            ..Default::default()
                        },
                    })
                    .collect();
                let track = view.page.tracks()[0].clone();
                view.playlist_picker(vec![track], cx);
            })
            .unwrap();
        wait_for_presented_frame(window, cx).await;
        let background = window
            .update(cx, |view, _, _| {
                assert!(view.list_scroll.0.borrow().base_handle.max_offset().y > px(0.));
                view.list_scroll.0.borrow().base_handle.offset()
            })
            .unwrap();
        let before = window
            .update(cx, |_, window, _| {
                super::render::capture(window, "playlist-scroll-before")
            })
            .unwrap();
        for (name, position) in [
            ("list-right", point(px(800.), px(400.))),
            ("list-left", point(px(490.), px(400.))),
            ("backdrop", point(px(1000.), px(400.))),
        ] {
            cx.update_window(window.into(), |_, window, cx| {
                window.dispatch_event(
                    PlatformInput::ScrollWheel(ScrollWheelEvent {
                        position,
                        delta: ScrollDelta::Pixels(point(px(0.), px(-300.))),
                        modifiers: Default::default(),
                        touch_phase: TouchPhase::Moved,
                    }),
                    cx,
                );
            })
            .unwrap();
            wait_for_presented_frame(window, cx).await;
            let image = window
                .update(cx, |_, window, _| {
                    super::render::capture(window, &format!("playlist-scroll-{name}"))
                })
                .unwrap();
            window
                .update(cx, |view, _, _| {
                    assert_eq!(
                        view.list_scroll.0.borrow().base_handle.offset(),
                        background,
                        "Scrolling {name} must not move the page behind Save to playlist"
                    );
                })
                .unwrap();
            if name == "list-right" {
                let changed = before
                    .enumerate_pixels()
                    .filter(|(x, y, pixel)| {
                        (470..780).contains(x)
                            && (250..550).contains(y)
                            && image.get_pixel(*x, *y) != *pixel
                    })
                    .count();
                assert!(
                    changed > 100,
                    "The right side must scroll the playlist rows"
                );
                assert!(
                    before.enumerate_pixels().all(|(x, y, pixel)| {
                        !(458..822).contains(&x)
                            || !(675..727).contains(&y)
                            || image.get_pixel(x, y) == pixel
                    }),
                    "Scrolling playlist rows must not paint into the New playlist footer"
                );
            }
        }
        cx.update_window(window.into(), |_, window, cx| {
            window.dispatch_event(
                PlatformInput::MouseDown(MouseDownEvent {
                    button: MouseButton::Right,
                    position: point(px(380.), px(400.)),
                    modifiers: Default::default(),
                    click_count: 1,
                    first_mouse: false,
                }),
                cx,
            );
        })
        .unwrap();
        wait_for_presented_frame(window, cx).await;
        window
            .update(cx, |view, _, _| {
                assert!(
                    matches!(
                        view.overlay,
                        Some(crate::app::Overlay::PlaylistPicker { .. })
                    ),
                    "Right-clicking the dimmed page must not replace the dialog with a song menu"
                );
            })
            .unwrap();
        window
            .update(cx, |view, window, cx| {
                window.resize(size(px(1280.), px(560.)));
                view.catalog_requests.playlist_choices_error = Some(
                    "Could not connect to YouTube Music. Check your connection and try again."
                        .into(),
                );
                cx.notify();
            })
            .unwrap();
        wait_for_presented_frame(window, cx).await;
        let compact = window
            .update(cx, |_, window, _| {
                super::render::capture(window, "playlist-scroll-retry-compact")
            })
            .unwrap();
        let panel = compact.get_pixel(450, 280);
        assert_ne!(
            compact.get_pixel(450, 0),
            panel,
            "The dialog must fit above the top edge"
        );
        assert_ne!(
            compact.get_pixel(450, 559),
            panel,
            "Retry and New playlist must fit above the bottom edge"
        );
    })
}

#[test]
#[ignore = "requires desktop, private D-Bus and isolated runtime/data; public artwork; synthetic accounts only"]
fn save_to_playlist_opens_from_loaded_playlists_and_confirms_with_a_toast() -> Result<()> {
    check_cache_window(async |window, cx| {
        let urls: Vec<&str> = COVERS.iter().map(|(_, _, url)| *url).collect();
        window
            .update(cx, |view, _, cx| {
                view.sidebar_playlists = COVERS
                    .iter()
                    .enumerate()
                    .map(
                        |(index, (title, subtitle, url))| crate::catalog::Collection {
                            id: format!("VLsynthetic{index}"),
                            title: (*title).into(),
                            subtitle: (*subtitle).into(),
                            thumbnail: Some((*url).into()),
                            details: crate::catalog::CollectionDetails {
                                editable: true,
                                ..Default::default()
                            },
                        },
                    )
                    .collect();
                let track = view.page.tracks()[0].clone();
                while view.catalog_requests.pending.try_recv().is_ok() {}
                view.playlist_picker(vec![track], cx);
                let requests = std::iter::from_fn(|| view.catalog_requests.pending.try_recv().ok())
                    .collect::<Vec<_>>();
                assert!(
                    requests.len() == 1 && matches!(requests[0].query, Query::PlaylistChoices(_)),
                    "Loaded rows must appear immediately while the selection's destinations are read"
                );
                assert_eq!(view.playlist_picker_choices().rows(1..2)[0].title, "Road trip");
            })
            .unwrap();
        for attempt in 0..250 {
            wait_for_presented_frame(window, cx).await;
            let loaded = window
                .update(cx, |view, _, _| {
                    urls.iter()
                        .all(|url| view.artwork.get(url, size(px(44.), px(44.))).is_some())
                })
                .unwrap();
            if loaded {
                break;
            }
            assert!(attempt < 249, "Playlist covers must load");
            cx.background_executor()
                .timer(Duration::from_millis(20))
                .await;
        }
        wait_for_presented_frame(window, cx).await;
        window
            .update(cx, |_, window, _| {
                super::render::capture(window, "playlist-picker");
            })
            .unwrap();
        let saved = |toast: Option<&'static str>| {
            move |view: &mut Music, window: &mut Window, cx: &mut Context<Music>| {
                let track = view.page.tracks()[0].clone();
                view.overlay = None;
                view.account_event(
                    account::Event::Applied {
                        session: view.account.as_ref().unwrap().session.clone(),
                        mutation: Box::new(Mutation::AddToPlaylist {
                            id: "synthetic0".into(),
                            video_ids: vec![track.id.clone()],
                            duplicates: false,
                        }),
                        result: Ok(Effect::PlaylistAdded {
                            additions: vec![(track.id.clone(), "occurrence".into())],
                            toast: toast.map(str::to_owned),
                        }),
                        session_rejected: false,
                    },
                    window,
                    cx,
                );
                assert_eq!(view.notice, None, "The toast replaces the page notice");
                view.toast.as_ref().unwrap().text.to_string()
            }
        };
        let text = window.update(cx, saved(None)).unwrap();
        assert_eq!(text, "Saved to Road trip");
        let text = window
            .update(cx, saved(Some("Saved to Road trip (from YouTube)")))
            .unwrap();
        assert_eq!(
            text, "Saved to Road trip (from YouTube)",
            "YouTube's own text wins"
        );
        window
            .update(cx, |view, _, cx| {
                // A current song shows the player bar, which the toast sits above.
                let track = view.page.tracks()[0].clone();
                view.library.queue.replace(vec![track], 0);
                cx.notify();
            })
            .unwrap();
        window.update(cx, saved(None)).unwrap();
        cx.background_executor()
            .timer(Duration::from_millis(400))
            .await;
        wait_for_presented_frame(window, cx).await;
        window
            .update(cx, |_, window, _| {
                super::render::capture(window, "playlist-saved");
            })
            .unwrap();
        cx.background_executor()
            .timer(Duration::from_millis(4000))
            .await;
        wait_for_presented_frame(window, cx).await;
        window
            .update(cx, |view, _, _| {
                assert!(view.toast.is_none(), "The toast closes after four seconds")
            })
            .unwrap();
    })
}

#[test]
#[ignore = "requires desktop, private D-Bus and isolated runtime/data; synthetic accounts only"]
fn playlist_choices_keep_fallback_rows_and_never_toggle_an_existing_like() -> Result<()> {
    check_cache_window(async |window, cx| {
        let (commands, submitted) = async_channel::bounded(1);
        window
            .update(cx, |view, window, cx| {
                view.account_commands = commands;
                view.sidebar_playlists = vec![crate::catalog::Collection {
                    id: "VLfallback".into(),
                    title: "Loaded playlist".into(),
                    subtitle: "3 songs".into(),
                    thumbnail: None,
                    details: crate::catalog::CollectionDetails {
                        editable: true,
                        ..Default::default()
                    },
                }];
                let track = view.page.tracks()[0].clone();
                view.ratings.insert(track.id.clone(), Rating::Like);
                window.focus(&view.overlay_focus, cx);
                view.playlist_picker(vec![track], cx);
                let choices = view.playlist_picker_choices();
                assert_eq!(choices.len(), 2);
                let rows = choices.rows(0..2);
                assert_eq!(rows[0].title, "Liked Music");
                assert!(rows[0].byline.is_empty());
                let job = playlist_choices_job(view);
                let mut destinations = rows;
                destinations[0].byline = "4 songs".into();
                destinations[0].pinned = true;
                destinations[1].title = "Road trip".into();
                destinations[1].byline = "8 songs".into();
                destinations[1].privacy = Some(crate::account::Privacy::Private);
                view.playlist_choices_result(
                    crate::app::PlaylistChoicesResult {
                        job,
                        result: Ok(crate::playlist_choices::Choices {
                            destinations,
                            empty: None,
                        }),
                        session_rejected: false,
                    },
                    window,
                    cx,
                );
                assert_eq!(
                    view.playlist_picker_choices().rows(1..2)[0].title,
                    "Road trip"
                );
            })
            .unwrap();
        wait_for_presented_frame(window, cx).await;
        window
            .update(cx, |_, window, _| {
                super::render::capture(window, "playlist-destinations");
            })
            .unwrap();
        window
            .update(cx, |view, window, cx| {
                view.refresh_playlist_choices(cx);
                let job = playlist_choices_job(view);
                view.playlist_choices_result(
                    crate::app::PlaylistChoicesResult {
                        job,
                        result: Err(
                            "Could not reach YouTube Music. Check your connection and try again."
                                .into(),
                        ),
                        session_rejected: false,
                    },
                    window,
                    cx,
                );
                assert!(view.catalog_requests.playlist_choices_error.is_some());
                assert_eq!(
                    view.playlist_picker_choices().rows(1..2)[0].title,
                    "Loaded playlist"
                );
            })
            .unwrap();
        wait_for_presented_frame(window, cx).await;
        window
            .update(cx, |_, window, _| {
                super::render::capture(window, "playlist-destinations-retry");
            })
            .unwrap();
        // Close is followed by Liked Music. Enter must save, even if it is already liked.
        super::render::key(window, cx, "tab").await;
        super::render::key(window, cx, "tab").await;
        super::render::key(window, cx, "enter").await;
        let account::Command::Apply { mutation, .. } = submitted.try_recv().unwrap() else {
            panic!("Expected an explicit Like request")
        };
        assert!(
            matches!(*mutation, Mutation::RateSong { ref track, rating:Rating::Like } if track.id == "00000000000")
        );
        window
            .update(cx, |view, _, _| {
                assert!(view.overlay.is_none());
                assert_eq!(view.ratings.get("00000000000"), Some(&Rating::Like));
                assert!(
                    !view
                        .catalog_requests
                        .is_loading(CatalogTarget::PlaylistChoices)
                );
            })
            .unwrap();
    })
}

fn playlist_choices_job(view: &Music) -> crate::app::Job {
    std::iter::from_fn(|| view.catalog_requests.pending.try_recv().ok())
        .find(|job| job.query.target() == CatalogTarget::PlaylistChoices)
        .expect("Expected a selection-bound playlist read")
}

#[test]
#[ignore = "requires desktop, private D-Bus and isolated runtime/data; synthetic accounts only"]
fn playlist_choices_ignore_closed_reopened_and_changed_account_responses() -> Result<()> {
    use crate::playlist_choices::{Choice, Choices, Destination};

    check_cache(|view, window, cx| {
        let response = |job, title: &str| crate::app::PlaylistChoicesResult {
            job,
            result: Ok(Choices {
                destinations: vec![Choice {
                    target: Destination::Playlist("PLsynthetic".into()),
                    title: title.into(),
                    byline: String::new(),
                    thumbnail: None,
                    privacy: None,
                    pinned: false,
                }],
                empty: None,
            }),
            session_rejected: false,
        };
        let tracks = view.page.tracks()[..2].to_vec();
        view.playlist_picker(tracks.clone(), cx);
        let old = playlist_choices_job(view);
        view.close_overlay(window, cx);
        view.playlist_choices_result(response(old.clone(), "Closed"), window, cx);
        assert!(view.catalog_requests.playlist_choices.is_none());

        view.playlist_picker(tracks.clone(), cx);
        let current = playlist_choices_job(view);
        view.playlist_choices_result(response(old, "Reopened"), window, cx);
        assert!(view.catalog_requests.playlist_choices.is_none());
        assert!(
            view.catalog_requests
                .is_loading(CatalogTarget::PlaylistChoices)
        );

        let mut reversed = current.clone();
        reversed.query =
            Query::PlaylistChoices(tracks.iter().rev().map(|track| track.id.clone()).collect());
        view.playlist_choices_result(response(reversed, "Wrong song order"), window, cx);
        assert!(view.catalog_requests.playlist_choices.is_none());

        let mut wrong_session = current.clone();
        wrong_session.session = Some(Arc::new((**current.session.as_ref().unwrap()).clone()));
        view.playlist_choices_result(response(wrong_session, "Different session"), window, cx);
        assert!(view.catalog_requests.playlist_choices.is_none());

        view.account_generation = view.account_generation.wrapping_add(1);
        view.playlist_choices_result(response(current, "Before another edit"), window, cx);
        assert!(view.catalog_requests.playlist_choices.is_none());
        let refreshed = playlist_choices_job(view);
        view.account.as_mut().unwrap().session =
            Arc::new((**refreshed.session.as_ref().unwrap()).clone());
        view.clear_account_state(cx);
        view.playlist_picker(tracks, cx);
        let new_account = playlist_choices_job(view);
        view.playlist_choices_result(response(refreshed, "Before account refresh"), window, cx);
        assert_eq!(view.playlist_picker_choices().len(), 0);
        view.playlist_choices_result(response(new_account, "Current destinations"), window, cx);
        assert_eq!(
            view.playlist_picker_choices().rows(0..1)[0].title,
            "Current destinations"
        );
        assert!(
            !view
                .catalog_requests
                .is_loading(CatalogTarget::PlaylistChoices)
        );
    })
}

#[test]
#[ignore = "requires desktop, private D-Bus and isolated runtime/data; synthetic accounts only"]
fn playlist_picker_shows_a_rejected_save_inside_the_dialog() -> Result<()> {
    use gpui::{MouseButton, MouseDownEvent, MouseUpEvent, PlatformInput};

    check_cache_window(async |window, cx| {
        window
            .update(cx, |view, _, cx| {
                view.request(
                    Query::More {
                        token: view.page.continuation.clone().unwrap(),
                        search: None,
                    },
                    cx,
                );
                let job = std::iter::from_fn(|| view.catalog_requests.pending.try_recv().ok())
                    .filter(|job| matches!(job.query, Query::More { .. }))
                    .last()
                    .unwrap();
                view.catalog_result(
                    CatalogResult {
                        id: job.id,
                        account_revision: job.account_revision,
                        account_generation: job.account_generation,
                        history_generation: job.history_generation,
                        append: true,
                        revalidate: false,
                        result: Err("Could not load more songs".into()),
                        session_rejected: false,
                        target: CatalogTarget::Page,
                    },
                    cx,
                );
            })
            .unwrap();
        wait_for_presented_frame(window, cx).await;
        let page_error_pixels = |cx: &mut gpui::AsyncApp, name| {
            window
                .update(cx, |_, window, _| {
                    super::render::capture(window, name)
                        .enumerate_pixels()
                        .filter(|(x, y, pixel)| {
                            (260..640).contains(x)
                                && (65..450).contains(y)
                                && pixel.0[0] > 140
                                && pixel.0[0].saturating_sub(pixel.0[1]) > 40
                        })
                        .count()
                })
                .unwrap()
        };
        let page_error_before = page_error_pixels(cx, "page-error-before-picker");
        assert!(
            page_error_before > 100,
            "Show the continuation failure before saving"
        );

        window
            .update(cx, |view, window, cx| {
                let (commands, submitted) = async_channel::bounded(1);
                drop(submitted);
                view.account_commands = commands;
                view.sidebar_playlists.clear();
                let track = view.page.tracks()[0].clone();
                window.focus(&view.overlay_focus, cx);
                view.playlist_picker(vec![track], cx);
            })
            .unwrap();
        super::render::key(window, cx, "tab").await;
        super::render::key(window, cx, "tab").await;
        wait_for_presented_frame(window, cx).await;
        let before = window
            .update(cx, |_, window, _| {
                super::render::capture(window, "playlist-save-before-rejection")
            })
            .unwrap();
        super::render::key(window, cx, "enter").await;
        wait_for_presented_frame(window, cx).await;
        window
            .update(cx, |view, _, _| {
                let Some(crate::app::Overlay::PlaylistPicker { error, .. }) = &view.overlay else {
                    panic!("A rejected save must keep the picker open")
                };
                assert_eq!(
                    error.as_deref(),
                    Some("The account connection stopped. Restart the app to reconnect.")
                );
                assert!(
                    view.error.is_none(),
                    "A picker submission error must not move the background page"
                );
                assert!(view.pending_mutations.is_empty());
            })
            .unwrap();
        let after = window
            .update(cx, |_, window, _| {
                super::render::capture(window, "playlist-save-rejected")
            })
            .unwrap();
        let x = after.width() / 2 - 190;
        let center = after.height() / 2;
        let footer = before.get_pixel(x, center + 90);
        let bottom = |frame: &image::RgbaImage| {
            (center..frame.height())
                .rfind(|&y| frame.get_pixel(x, y) == footer)
                .unwrap()
        };
        assert!(
            bottom(&after) > bottom(&before),
            "A rejected save must visibly expand the picker with its error"
        );
        assert!(
            (0..after.height())
                .all(|y| (0..440).all(|x| before.get_pixel(x, y) == after.get_pixel(x, y))),
            "Submission feedback must leave the page behind the picker unchanged"
        );
        super::render::key(window, cx, "escape").await;
        assert!(
            page_error_pixels(cx, "page-error-after-picker") >= page_error_before,
            "Closing Save to playlist must keep the continuation error visible"
        );

        window
            .update(cx, |view, _, cx| {
                assert!(view.overlay.is_none());
                view.error = Some("Could not start playback".into());
                cx.notify();
            })
            .unwrap();
        wait_for_presented_frame(window, cx).await;
        let dismiss = |cx: &mut gpui::AsyncApp| {
            cx.update_window(window.into(), |_, window, cx| {
                let position = point(px(1154.), px(84.));
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
        };
        dismiss(cx);
        wait_for_presented_frame(window, cx).await;
        assert!(
            page_error_pixels(cx, "page-error-after-dismiss") >= page_error_before,
            "Dismissing another warning must reveal the continuation failure"
        );
        dismiss(cx);
        wait_for_presented_frame(window, cx).await;
        window
            .update(cx, |view, _, _| assert!(view.sync.page_error.is_none()))
            .unwrap();
    })
}

#[test]
#[ignore = "requires desktop, private D-Bus and isolated runtime/data; synthetic accounts only"]
fn already_added_shows_a_rejected_retry_inside_the_dialog() -> Result<()> {
    check_cache_window(async |window, cx| {
        window
            .update(cx, |view, window, cx| {
                let (commands, submitted) = async_channel::bounded(1);
                drop(submitted);
                view.account_commands = commands;
                view.error = None;
                view.notice = None;
                view.account_event(
                    account::Event::Applied {
                        session: view.account.as_ref().unwrap().session.clone(),
                        mutation: Box::new(Mutation::AddToPlaylist {
                            id: "PLduplicate".into(),
                            video_ids: vec!["00000000000".into()],
                            duplicates: false,
                        }),
                        result: Ok(Effect::AlreadyAdded),
                        session_rejected: false,
                    },
                    window,
                    cx,
                );
                window.focus(&view.overlay_focus, cx);
            })
            .unwrap();
        super::render::key(window, cx, "tab").await;
        super::render::key(window, cx, "tab").await;
        let before = window
            .update(cx, |_, window, _| {
                super::render::capture(window, "duplicate-save-before-rejection")
            })
            .unwrap();
        super::render::key(window, cx, "enter").await;
        window
            .update(cx, |view, _, _| {
                let Some(crate::app::Overlay::AlreadyAdded { error, .. }) = &view.overlay else {
                    panic!("A rejected duplicate save must keep the dialog open")
                };
                assert_eq!(
                    error.as_deref(),
                    Some("The account connection stopped. Restart the app to reconnect.")
                );
                assert!(view.error.is_none());
                assert!(view.pending_mutations.is_empty());
            })
            .unwrap();
        let after = window
            .update(cx, |_, window, _| {
                super::render::capture(window, "duplicate-save-rejected")
            })
            .unwrap();
        let left = after.width() / 2 - 240;
        let error_pixels = (0..after.height())
            .flat_map(|y| (left..left + 480).map(move |x| (x, y)))
            .filter(|&(x, y)| {
                let [r, g, b, _] = after.get_pixel(x, y).0;
                r > 100 && r.saturating_sub(g) > 20 && r.saturating_sub(b) > 20
            })
            .count();
        assert!(
            error_pixels > 100,
            "The duplicate save error must be visible"
        );
        assert!(
            (0..after.height())
                .all(|y| (0..left).all(|x| before.get_pixel(x, y) == after.get_pixel(x, y))),
            "Duplicate save feedback must leave the surrounding page unchanged"
        );
        let (commands, submitted) = async_channel::bounded(1);
        window
            .update(cx, |view, _, _| {
                view.account_commands = commands;
                view.account_operation = Some(crate::app::AccountOperation::RestoreFailed);
            })
            .unwrap();
        super::render::key(window, cx, "enter").await;
        window
            .update(cx, |view, _, _| {
                let Some(crate::app::Overlay::AlreadyAdded { error, .. }) = &view.overlay else {
                    panic!("An account change must keep the duplicate dialog open")
                };
                assert_eq!(
                    error.as_deref(),
                    Some("Finish the account change before saving edits.")
                );
                assert!(view.error.is_none());
                assert!(submitted.is_empty());
                view.account_operation = None;
            })
            .unwrap();
        super::render::key(window, cx, "enter").await;
        window
            .update(cx, |view, _, _| {
                assert!(
                    view.overlay.is_none(),
                    "An accepted retry closes the dialog"
                );
                assert!(
                    view.error.is_none(),
                    "An accepted retry clears the prior error"
                );
                let account::Command::Apply { session, mutation } = submitted.try_recv().unwrap()
                else {
                    panic!("Add anyway must submit the duplicate save")
                };
                assert!(Arc::ptr_eq(
                    &session,
                    &view.account.as_ref().unwrap().session
                ));
                assert!(
                    matches!(*mutation, Mutation::AddToPlaylist { id, video_ids, duplicates: true }
                    if id == "PLduplicate" && video_ids == ["00000000000"])
                );
                assert!(view.pending_mutations.contains_key("PLduplicate"));
            })
            .unwrap();
    })
}

#[test]
#[ignore = "requires desktop, private D-Bus and isolated runtime/data; synthetic accounts only"]
fn a_queue_can_create_its_first_playlist_without_losing_songs() -> Result<()> {
    use gpui::{MouseButton, MouseDownEvent, MouseUpEvent, PlatformInput};

    check_cache_window(async |window, cx| {
        let (commands, submitted) = async_channel::bounded(1);
        window
            .update(cx, |view, _, cx| {
                view.account_commands = commands;
                view.sidebar_playlists.clear();
                let tracks = view.page.tracks();
                view.library.queue.replace(
                    vec![tracks[0].clone(), tracks[1].clone(), tracks[0].clone()],
                    0,
                );
                view.player_open = true;
                cx.notify();
            })
            .unwrap();
        wait_for_presented_frame(window, cx).await;
        let save = window
            .update(cx, |view, _, _| {
                let queue = view.player_page.queue_bounds.get();
                // Save is above the 62px autoplay section, centered in its 64px header.
                point(queue.right() - px(45.), queue.top() - px(94.))
            })
            .unwrap();
        for event in [
            PlatformInput::MouseDown(MouseDownEvent {
                button: MouseButton::Left,
                position: save,
                modifiers: Default::default(),
                click_count: 1,
                first_mouse: false,
            }),
            PlatformInput::MouseUp(MouseUpEvent {
                button: MouseButton::Left,
                position: save,
                modifiers: Default::default(),
                click_count: 1,
            }),
        ] {
            cx.update_window(window.into(), |_, window, cx| {
                window.dispatch_event(event, cx)
            })
            .unwrap();
            wait_for_presented_frame(window, cx).await;
        }
        window
            .update(cx, |view, _, _| {
                assert!(matches!(
                    &view.overlay,
                    Some(crate::app::Overlay::PlaylistPicker { tracks, .. }) if tracks.len() == 3
                ));
            })
            .unwrap();
        // With no saved playlists, the dialog has Close and New playlist as its tab stops.
        super::render::key(window, cx, "tab").await;
        super::render::key(window, cx, "tab").await;
        super::render::key(window, cx, "enter").await;
        window
            .update(cx, |view, _, _| {
                assert!(
                    matches!(view.overlay, Some(crate::app::Overlay::PlaylistForm { .. })),
                    "New playlist must be available when saving a queue with no editable playlists"
                );
            })
            .unwrap();
        for key in ["q", "u", "e", "u", "e", "d", "enter"] {
            super::render::key(window, cx, key).await;
        }
        let account::Command::Apply { mutation, .. } = submitted.try_recv().unwrap() else {
            panic!("Expected a playlist creation request")
        };
        let Mutation::CreatePlaylist {
            title,
            initial_video_ids,
            ..
        } = *mutation
        else {
            panic!("Expected New playlist to create the selected queue")
        };
        assert_eq!(title, "queued");
        assert_eq!(
            initial_video_ids,
            ["00000000000", "00000000001", "00000000000"],
            "Saving a queue must preserve every song in order, including repeats"
        );
    })
}

#[test]
#[ignore = "requires desktop, private D-Bus and isolated runtime/data; synthetic accounts only"]
fn clicking_a_queued_row_plays_it_and_dragging_reorders() -> Result<()> {
    use gpui::{MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PlatformInput};
    check_cache_window(async |window, cx| {
        let ids = window
            .update(cx, |view, _, cx| {
                view.account = None;
                view.player = Some(
                    crate::player::Player::new(crate::player::PlayerOptions {
                        volume: 0.,
                        audio_output: Some("null".into()),
                        ..Default::default()
                    })
                    .unwrap(),
                );
                view.library
                    .queue
                    .replace(view.page.tracks()[..6].to_vec(), 0);
                view.player_open = true;
                cx.notify();
                view.library
                    .queue
                    .tracks
                    .iter()
                    .map(|track| track.id.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap();
        wait_for_presented_frame(window, cx).await;
        let queue = window
            .update(cx, |view, _, _| view.player_page.queue_bounds.get())
            .unwrap();
        // A 57 px row whose 32 px artwork starts 8 px in, followed by the title.
        let row = |index: usize, x: f32| {
            point(
                queue.left() + px(x),
                queue.top() + px(57. * index as f32 + 28.),
            )
        };
        let down = |position| {
            PlatformInput::MouseDown(MouseDownEvent {
                button: MouseButton::Left,
                position,
                modifiers: Default::default(),
                click_count: 1,
                first_mouse: false,
            })
        };
        let up = |position| {
            PlatformInput::MouseUp(MouseUpEvent {
                button: MouseButton::Left,
                position,
                modifiers: Default::default(),
                click_count: 1,
            })
        };
        let hover = |position, pressed_button| {
            PlatformInput::MouseMove(MouseMoveEvent {
                position,
                pressed_button,
                modifiers: Default::default(),
            })
        };
        let order = |view: &Music| {
            view.library
                .queue
                .tracks
                .iter()
                .map(|track| track.id.clone())
                .collect::<Vec<_>>()
        };
        for (index, x) in [(2, 24.), (4, 80.)] {
            let position = row(index, x);
            for event in [hover(position, None), down(position), up(position)] {
                cx.update_window(window.into(), |_, window, cx| {
                    window.dispatch_event(event, cx)
                })
                .unwrap();
                wait_for_presented_frame(window, cx).await;
            }
            window
                .update(cx, |view, _, _| {
                    assert_eq!(
                        view.library.queue.index,
                        Some(index),
                        "Clicking row {index} at x={x} must select it"
                    );
                    assert!(
                        view.active_id.is_some() && view.playback != crate::app::Playback::Stopped,
                        "Clicking row {index} at x={x} must start playing it"
                    );
                    assert_eq!(order(view), ids, "A click must not reorder the queue");
                })
                .unwrap();
        }
        let (start, end) = (row(1, 24.), row(3, 24.));
        let held = Some(MouseButton::Left);
        for event in [
            hover(start, None),
            down(start),
            hover(row(2, 24.), held),
            hover(end, held),
            up(end),
        ] {
            cx.update_window(window.into(), |_, window, cx| {
                window.dispatch_event(event, cx)
            })
            .unwrap();
            wait_for_presented_frame(window, cx).await;
        }
        window
            .update(cx, |view, _, _| {
                let dropped = [0, 2, 3, 1, 4, 5].map(|index| ids[index].clone());
                assert_eq!(
                    order(view),
                    dropped,
                    "Dragging row 1 onto row 3 must reorder the queue"
                );
                assert_eq!(
                    view.library.queue.index,
                    Some(4),
                    "A drag must not change the playing song"
                );
            })
            .unwrap();

        let start_drag = async |cx: &mut gpui::AsyncApp| {
            for event in [hover(start, None), down(start), hover(end, held)] {
                cx.update_window(window.into(), |_, window, cx| {
                    window.dispatch_event(event, cx)
                })
                .unwrap();
                wait_for_presented_frame(window, cx).await;
            }
            window
                .update(cx, |view, _, _| {
                    let preview = view.player_page.reorder.unwrap();
                    assert_eq!((preview.from, preview.to), (1, 3));
                })
                .unwrap();
        };
        let release_drag = async |cx: &mut gpui::AsyncApp| {
            cx.update_window(window.into(), |_, window, cx| {
                window.dispatch_event(up(end), cx)
            })
            .unwrap();
            wait_for_presented_frame(window, cx).await;
        };
        let original = window.update(cx, |view, _, _| order(view)).unwrap();
        for shortcut in ["escape", "q", "alt-left"] {
            start_drag(cx).await;

            super::render::key(window, cx, shortcut).await;
            release_drag(cx).await;
            window
                .update(cx, |view, _, _| {
                    assert_eq!(
                        order(view),
                        original,
                        "{shortcut} must cancel the queue move before release"
                    );
                    assert_eq!(view.player_open, shortcut == "escape");
                    assert!(view.player_page.reorder.is_none());
                    assert_eq!(view.library.queue.index, Some(4));
                })
                .unwrap();
            if shortcut == "escape" {
                super::render::key(window, cx, "escape").await;
                window
                    .update(cx, |view, _, _| {
                        assert!(!view.player_open, "Second Escape closes the player");
                    })
                    .unwrap();
            }
            super::render::key(window, cx, "q").await;
        }

        let reply = window
            .update(cx, |view, _, cx| {
                use crate::{
                    app::{PlaybackQuery, PlaybackResult, QueueUpdate},
                    catalog::PlaylistTarget,
                    watch::Watch,
                };
                let (jobs, pending) = async_channel::bounded(1);
                view.playback_jobs = jobs;
                view.pending_playback_jobs = pending;
                let source = PlaylistTarget::from_navigation(&serde_json::json!({
                    "watchPlaylistEndpoint":{"playlistId":"PLreplacement"}
                }))
                .unwrap();
                view.start_queue(PlaybackQuery::Playlist(source.into()), cx);
                let job = view.pending_playback_jobs.try_recv().unwrap();
                let replacement = crate::app::sync::tests::response(
                    &[("Replacement", (20..26).collect())],
                    None,
                    true,
                )
                .tracks();
                PlaybackResult::Watch {
                    id: job.id,
                    account_revision: job.account_revision,
                    update: QueueUpdate::Replace,
                    collection: None,
                    result: Ok(Box::new(Watch {
                        current: Some(replacement[0].clone()),
                        current_index: Some(0),
                        queue: replacement,
                        source: Some("Replacement playlist".into()),
                        lyrics: None,
                        related: None,
                        comments: None,
                        alternatives: None,
                        continuation: None,
                        source_target: None,
                        shuffle: None,
                    })),
                    session_rejected: false,
                }
            })
            .unwrap();
        wait_for_presented_frame(window, cx).await;
        start_drag(cx).await;
        let replacement = window
            .update(cx, |view, _, cx| {
                view.playback_result(reply, cx);
                order(view)
            })
            .unwrap();
        assert_eq!(
            replacement,
            (20..26).map(|i| format!("{i:011}")).collect::<Vec<_>>()
        );
        wait_for_presented_frame(window, cx).await;
        release_drag(cx).await;
        window
            .update(cx, |view, _, _| {
                assert_eq!(
                    order(view),
                    replacement,
                    "A drag from the old queue must not reorder the replacement"
                );
                assert_eq!(view.library.queue.index, Some(0));
                assert!(view.player_page.reorder.is_none());
            })
            .unwrap();

        start_drag(cx).await;
        super::render::key(window, cx, "s").await;
        let shuffled = window
            .update(cx, |view, _, _| {
                assert!(
                    view.library.queue.shuffled,
                    "S must shuffle the local queue"
                );
                order(view)
            })
            .unwrap();
        release_drag(cx).await;
        window
            .update(cx, |view, _, _| {
                assert_eq!(
                    order(view),
                    shuffled,
                    "Release must preserve the shuffled queue"
                );
                assert_eq!(view.library.queue.index, Some(0));
                assert!(view.player_page.reorder.is_none());
            })
            .unwrap();

        start_drag(cx).await;
        window
            .update(cx, |view, _, cx| {
                let song = view.page.tracks()[26].clone();
                view.enqueue(song, cx);
            })
            .unwrap();
        wait_for_presented_frame(window, cx).await;
        release_drag(cx).await;
        window
            .update(cx, |view, _, _| {
                let expected = [0, 2, 3, 1, 4, 5].map(|index| shuffled[index].as_str());
                let mut expected = expected.to_vec();
                expected.push("00000000026");
                assert_eq!(
                    order(view),
                    expected,
                    "Appending a song must preserve the drag"
                );
                assert_eq!(view.library.queue.index, Some(0));
            })
            .unwrap();
    })
}

#[test]
#[ignore = "requires desktop, private D-Bus and isolated runtime/data; synthetic accounts only"]
fn playlist_song_menu_moves_the_selected_occurrence_across_shelves() -> Result<()> {
    use super::render::key;
    use crate::catalog::Item;

    check_cache_window(async |window, cx| {
        let (commands, submitted) = async_channel::bounded(2);
        for (offset, destination) in [(5, "occurrence-0"), (6, "occurrence-3")] {
            window
                .update(cx, |view, window, cx| {
                    view.account_commands = commands.clone();
                    let id = format!("VLmenu-{offset}");
                    view.route = Route::Browse(BrowseLink::new(id, "Editable playlist"));
                    view.page = crate::app::sync::tests::response(
                        &[("Songs", vec![0, 1]), ("More songs", vec![2, 3])],
                        None,
                        true,
                    );
                    view.page.editable = true;
                    // Two occurrences of the same video, each with its own playlist ID.
                    let Item::Track(duplicate) = &mut view.page.shelves[0].items[1] else {
                        panic!("The fixture must contain song rows");
                    };
                    duplicate.id = "00000000000".into();
                    view.rebuild_rows();
                    view.open_item_menu(1, window, cx);
                })
                .unwrap();
            wait_for_presented_frame(window, cx).await;
            key(window, cx, "home").await;
            for _ in 0..offset {
                key(window, cx, "down").await;
            }
            key(window, cx, "enter").await;
            let account::Command::Apply { mutation, .. } = submitted.try_recv().unwrap() else {
                panic!("Expected a playlist move");
            };
            assert!(matches!(*mutation, Mutation::MoveInPlaylist {
                id, set_video_id, before: Some(before)
            } if id == format!("VLmenu-{offset}") && set_video_id == "occurrence-1" && before == destination));
        }
    })
}

#[test]
#[ignore = "requires desktop, private D-Bus and isolated runtime/data; synthetic accounts only"]
fn saving_a_queue_keeps_its_songs_after_playback_changes() -> Result<()> {
    use super::render::key;
    use crate::app::sync::tests::page as playlist;

    check_cache_window(async |window, cx| {
        let (commands, submitted) = async_channel::bounded(1);
        let songs = window
            .update(cx, |view, window, cx| {
                view.account_commands = commands;
                view.navigate(Route::Browse(BrowseLink::new("VLPLtarget", "Target")), cx);
                let mut destination = playlist(0, false);
                destination.editable = true;
                reply(view, destination, cx);
                view.sidebar_playlists = vec![crate::catalog::Collection {
                    id: "VLPLtarget".into(),
                    title: "Target".into(),
                    subtitle: String::new(),
                    thumbnail: None,
                    details: crate::catalog::CollectionDetails {
                        editable: true,
                        ..Default::default()
                    },
                }];
                let mut tracks = playlist(2, false).tracks();
                for (index, track) in tracks.iter_mut().enumerate() {
                    track.id = format!("queueSong0{index}");
                    track.title = format!("Queue song {index}");
                }
                view.library.queue.replace(tracks.clone(), 0);
                window.focus(&view.overlay_focus, cx);
                view.playlist_picker(view.library.queue.tracks.to_vec(), cx);
                let _ = playlist_choices_job(view);
                tracks
            })
            .unwrap();

        key(window, cx, "tab").await;
        key(window, cx, "tab").await;
        key(window, cx, "enter").await;
        let account::Command::Apply { session, mutation } = submitted.try_recv().unwrap() else {
            panic!("Expected the Save to playlist selection");
        };
        let video_ids: Vec<_> = songs.iter().map(|track| track.id.clone()).collect();
        assert!(
            matches!(&*mutation, Mutation::AddToPlaylist { id, video_ids: selected, duplicates: false }
            if id == "PLtarget" && selected == &video_ids)
        );

        window
            .update(cx, |view, window, cx| {
                assert!(view.overlay.is_none());
                view.clear_queue(cx);
                view.library.queue.replace(playlist(500, false).tracks(), 0);
                let saved_queue = view.library.queue.tracks.clone();
                view.account_event(
                    account::Event::Applied {
                        session,
                        mutation,
                        result: Ok(Effect::PlaylistAdded {
                            additions: video_ids
                                .into_iter()
                                .enumerate()
                                .map(|(i, id)| (id, format!("saved-occurrence-{i}")))
                                .collect(),
                            toast: None,
                        }),
                        session_rejected: false,
                    },
                    window,
                    cx,
                );
                assert!(Arc::ptr_eq(&view.library.queue.tracks, &saved_queue));
                let tracks = view.page.tracks();
                assert_eq!(
                    tracks.len(),
                    songs.len(),
                    "Confirmed songs must appear before readback"
                );
                for (index, (actual, source)) in tracks.iter().zip(&songs).enumerate() {
                    assert_eq!(actual.id, source.id);
                    assert_eq!(actual.title, source.title);
                    assert_eq!(actual.artist, source.artist);
                    assert_eq!(
                        actual.details.set_video_id,
                        Some(format!("saved-occurrence-{index}"))
                    );
                }
                assert!(!view.catalog_requests.is_loading(CatalogTarget::Page));
                assert!(
                    view.catalog_requests
                        .is_loading(CatalogTarget::PlaylistMetadata)
                );
            })
            .unwrap();
        wait_for_presented_frame(window, cx).await;
    })
}

#[test]
#[ignore = "requires desktop, private D-Bus and isolated runtime/data; synthetic accounts only"]
fn dragging_a_playlist_song_moves_its_occurrence_across_loaded_shelves() -> Result<()> {
    use gpui::{MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PlatformInput};
    use serde_json::json;

    check_cache_window(async |window, cx| {
        let (commands, submitted) = async_channel::bounded(2);
        window
            .update(cx, |view, _, cx| {
                view.account_commands = commands;
                view.route = Route::Browse(BrowseLink::new("VLPLdrag", "Editable playlist"));
                view.page = crate::app::sync::tests::response(
                    &[("", vec![0, 1]), ("", vec![2, 3])],
                    None,
                    true,
                );
                view.page.header = crate::catalog::parse_page(
                    &json!({"header":{"musicResponsiveHeaderRenderer":{
                        "title":{"simpleText":"Editable playlist"}
                    }},"contents":[]}),
                    "Editable playlist",
                )
                .unwrap()
                .header;
                assert!(view.page.header.is_some());
                view.page.editable = true;
                let crate::catalog::Item::Track(duplicate) = &mut view.page.shelves[0].items[1]
                else {
                    panic!("Expected a song");
                };
                duplicate.id = "00000000000".into();
                view.rebuild_rows();
                cx.notify();
            })
            .unwrap();
        for (cancel, partial) in [(false, false), (true, false), (false, true)] {
            if partial {
                window
                    .update(cx, |view, _, cx| {
                        view.page.continuation = Some(
                            crate::catalog::Continuation::from_response(
                                "playlist-drag-unloaded-tail",
                                &json!({}),
                                view.account.as_ref().map(|account| account.session.clone()),
                            )
                            .unwrap(),
                        );
                        cx.notify();
                    })
                    .unwrap();
            }
            wait_for_presented_frame(window, cx).await;
            let (start, target) = window
                .update(cx, |view, window, _| {
                    let row = |index| {
                        let ix = view.list_rows.iter().position(|row| {
                        matches!(row, crate::app::ListRow::Item(item) if *item == index)
                    }).unwrap() + usize::from(view.detail_list.inline_header);
                        view.detail_list.state.bounds_for_item(ix).unwrap()
                    };
                    let source = row(1);
                    let target = row(if partial { 3 } else { 2 });
                    let inset = if view.detail_list.inline_header {
                        px(view.page_layout(window).inset)
                    } else {
                        px(0.)
                    };
                    (
                        point(source.left() + inset + px(28.), source.top() + px(34.)),
                        point(target.left() + inset + px(180.), target.top() + px(53.)),
                    )
                })
                .unwrap();

            let move_to = |position, pressed_button| {
                PlatformInput::MouseMove(MouseMoveEvent {
                    position,
                    pressed_button,
                    modifiers: Default::default(),
                })
            };
            let press = || {
                PlatformInput::MouseDown(MouseDownEvent {
                    position: start,
                    button: MouseButton::Left,
                    click_count: 1,
                    modifiers: Default::default(),
                    first_mouse: false,
                })
            };
            let release = || {
                PlatformInput::MouseUp(MouseUpEvent {
                    position: target,
                    button: MouseButton::Left,
                    click_count: 1,
                    modifiers: Default::default(),
                })
            };
            for event in [
                move_to(start, None),
                press(),
                move_to(target, Some(MouseButton::Left)),
            ] {
                cx.update_window(window.into(), |_, window, cx| {
                    window.dispatch_event(event, cx)
                })
                .unwrap();
                wait_for_presented_frame(window, cx).await;
            }
            window
                .update(cx, |_, _, cx| {
                    assert!(cx.has_active_drag(), "The grip must start a playlist drag");
                })
                .unwrap();
            if cancel {
                super::render::key(window, cx, "escape").await;
            }
            cx.update_window(window.into(), |_, window, cx| {
                window.dispatch_event(release(), cx)
            })
            .unwrap();
            if cancel || partial {
                assert!(
                    submitted.is_empty(),
                    "An aborted drop must not submit a move"
                );
                window
                    .update(cx, |view, _, cx| {
                        assert!(!cx.has_active_drag());
                        assert!(view.library.queue.tracks.is_empty());
                    })
                    .unwrap();
                continue;
            }

            let account::Command::Apply { session, mutation } = submitted
                .try_recv()
                .expect("Dragging the second occurrence after Song 3 must save a playlist move")
            else {
                panic!("Expected a playlist edit");
            };
            assert!(matches!(
                &*mutation,
                Mutation::MoveInPlaylist { id, set_video_id, before: Some(before) }
                    if id == "VLPLdrag" && set_video_id == "occurrence-1" && before == "occurrence-3"
            ));
            window
                .update(cx, |view, window, cx| {
                    let order = |view: &Music| {
                        view.page
                            .tracks()
                            .into_iter()
                            .map(|song| song.details.set_video_id.unwrap())
                            .collect::<Vec<_>>()
                    };
                    assert_eq!(
                        order(view),
                        [
                            "occurrence-0",
                            "occurrence-1",
                            "occurrence-2",
                            "occurrence-3"
                        ]
                    );
                    assert!(
                        view.library.queue.tracks.is_empty(),
                        "Dragging a song must not play it"
                    );
                    view.account_event(
                        account::Event::Applied {
                            session,
                            mutation,
                            result: Ok(Effect::Changed),
                            session_rejected: false,
                        },
                        window,
                        cx,
                    );
                    assert_eq!(
                        order(view),
                        [
                            "occurrence-0",
                            "occurrence-2",
                            "occurrence-1",
                            "occurrence-3"
                        ]
                    );
                })
                .unwrap();
        }
    })
}

#[test]
#[ignore = "requires desktop, private D-Bus and isolated runtime/data; synthetic accounts only"]
fn playlist_drag_scrolls_to_songs_outside_the_original_viewport() -> Result<()> {
    use gpui::{MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PlatformInput};
    use serde_json::json;

    check_cache_window(async |window, cx| {
        let (commands, submitted) = async_channel::bounded(1);
        window
            .update(cx, |view, _, cx| {
                view.account_commands = commands;
                view.route = Route::Browse(BrowseLink::new("VLPLdrag", "Long playlist"));
                view.page =
                    crate::app::sync::tests::response(&[("", (0..100).collect())], None, true);
                view.page.header = crate::catalog::parse_page(
                    &json!({"header":{"musicResponsiveHeaderRenderer":{
                    "title":{"simpleText":"Long playlist"}
                }},"contents":[]}),
                    "Long playlist",
                )
                .unwrap()
                .header;
                view.page.editable = true;
                view.rebuild_rows();
                cx.notify();
            })
            .unwrap();
        wait_for_presented_frame(window, cx).await;

        let (start, edge, initial_offset) = window
            .update(cx, |view, window, _| {
                let list = &view.detail_list.state;
                let source = list
                    .bounds_for_item(1 + usize::from(view.detail_list.inline_header))
                    .unwrap();
                let inset = if view.detail_list.inline_header {
                    px(view.page_layout(window).inset)
                } else {
                    px(0.)
                };
                let viewport = list.viewport_bounds();
                let edge = point(
                    viewport.left() + inset + px(180.),
                    viewport.bottom() - px(6.),
                );
                (
                    point(source.left() + inset + px(28.), source.top() + px(34.)),
                    edge,
                    list.scroll_px_offset_for_scrollbar().y,
                )
            })
            .unwrap();

        let motion = |position, pressed_button| {
            PlatformInput::MouseMove(MouseMoveEvent {
                position,
                pressed_button,
                modifiers: Default::default(),
            })
        };
        let held = Some(MouseButton::Left);
        for event in [
            motion(start, None),
            PlatformInput::MouseDown(MouseDownEvent {
                position: start,
                button: MouseButton::Left,
                modifiers: Default::default(),
                click_count: 1,
                first_mouse: false,
            }),
            motion(edge, held),
        ] {
            cx.update_window(window.into(), |_, window, cx| {
                window.dispatch_event(event, cx)
            })
            .unwrap();
            wait_for_presented_frame(window, cx).await;
        }
        window
            .update(cx, |_, _, cx| assert!(cx.has_active_drag()))
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(4);
        let (drop_at, successor) = loop {
            let target = window
                .update(cx, |view, _, _| {
                    let list = &view.detail_list.state;
                    let removed = initial_offset - list.scroll_px_offset_for_scrollbar().y;
                    if removed < px(800.) {
                        return None;
                    }
                    let ix = list.logical_scroll_top().item_ix + 2;
                    let rect = list.bounds_for_item(ix).unwrap();
                    let row = &view.list_rows[ix - usize::from(view.detail_list.inline_header)];
                    let crate::app::ListRow::Item(index) = row else {
                        panic!("Expected a song row")
                    };
                    let crate::catalog::Item::Track(song) = &view.rows[*index] else {
                        panic!("Expected a song")
                    };
                    Some((
                        point(edge.x, rect.top() + px(14.)),
                        song.details.set_video_id.clone().unwrap(),
                    ))
                })
                .unwrap();
            if let Some(target) = target {
                break target;
            }
            assert!(
                Instant::now() < deadline,
                "Holding the dragged song near the bottom must scroll the list"
            );
            cx.background_executor()
                .timer(Duration::from_millis(20))
                .await;
        };

        for event in [
            motion(drop_at, held),
            PlatformInput::MouseUp(MouseUpEvent {
                position: drop_at,
                button: MouseButton::Left,
                modifiers: Default::default(),
                click_count: 1,
            }),
        ] {
            cx.update_window(window.into(), |_, window, cx| {
                window.dispatch_event(event, cx)
            })
            .unwrap();
            wait_for_presented_frame(window, cx).await;
        }
        let account::Command::Apply { mutation, .. } = submitted.try_recv().unwrap() else {
            panic!("Expected an edit after dropping the song");
        };
        assert!(matches!(
            &*mutation, Mutation::MoveInPlaylist { id, set_video_id, before:Some(before) }
                if id == "VLPLdrag" && set_video_id == "occurrence-1" && *before == successor
        ));
    })
}
