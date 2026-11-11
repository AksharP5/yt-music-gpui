//! Cached navigation with intercepted catalog reads and synthetic account state only.
use super::account::{connection, session};
use super::render::wait_for_presented_frame;
use crate::{
    account::{Mutation, Section},
    app::{CatalogRequests, CatalogResult, CatalogTarget, Music, Query, Route, Search, Startup},
    assets::Assets,
    catalog::{BrowseLink, Page, Rating},
    search::{SearchFilter, SearchScope},
    track_state,
};
use anyhow::{Result, ensure};
use gpui::{
    AppContext, Bounds, Context, ListOffset, ScrollStrategy, WindowBounds, WindowKind,
    WindowOptions, point, px, size,
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

fn reply(view: &mut Music, page: Page, cx: &mut Context<Music>) {
    let job = view
        .catalog_requests
        .pending
        .try_recv()
        .expect("Expected one catalog request");
    assert!(job.query.target() == CatalogTarget::Page);
    let append = matches!(job.query, Query::More { .. });
    let revalidate = matches!(job.query, Query::Revalidate(_));
    view.catalog_result(
        CatalogResult {
            id: job.id,
            account_revision: job.account_revision,
            append,
            revalidate,
            result: Ok(page),
            session_rejected: false,
            target: CatalogTarget::Page,
        },
        cx,
    );
}

fn page(title: &str, token: &str) -> Page {
    let mut page =
        crate::app::sync::tests::response(&[("Songs", (0..100).collect())], Some(token), true);
    page.title = title.into();
    page
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
                assert!(view.catalog_requests.pending.is_empty(), "An unfiltered Home link shares the primary Home route");
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
                view.sync.page_fetched_at = Some(fetched_at);
                let token = view.page.continuation.clone().unwrap();
                view.request(Query::More { token, search: None }, cx);
                reply(view, crate::app::sync::tests::response(&[("Songs", (100..150).collect())], Some("deeper-tail"), false), cx);
                assert_eq!(view.sync.page_fetched_at, Some(fetched_at), "Loading a continuation must not renew the first page's age");
                view.list_scroll.scroll_to_item_strict(80, ScrollStrategy::Top);
                cx.notify();
            }).unwrap();
            wait_for_presented_frame(window, cx).await;
            let old_scroll = window.update(cx, |view, _, cx| {
                let old_scroll = view.list_scroll.0.borrow().base_handle.offset();
                assert!(old_scroll.y < px(-1000.), "The playlist must actually be scrolled before testing Back and revalidation");
                view.navigate(Route::Explore, cx);
                reply(view, page("Explore", "explore-tail"), cx);
                view.go_back(cx);
                assert_eq!(view.rows.len(), 150, "Stale Back must render all cached continuations before refreshing");
                assert_eq!(view.list_scroll.0.borrow().base_handle.offset(), old_scroll);
                assert_eq!(view.page.continuation.as_ref().unwrap().token(), "deeper-tail");
                assert!(view.catalog_requests.is_loading(CatalogTarget::Page));
                old_scroll
            }).unwrap();
            wait_for_presented_frame(window, cx).await;
            window.update(cx, |view, window, cx| {
                super::render::capture(window, "stale-playlist-first-frame");
                assert!(matches!(view.catalog_requests.pending.try_recv().unwrap().query, Query::Revalidate(_)));
                // Put the intercepted request back through the same path, then mutate while it is in flight.
                view.revalidate_page(cx);
                let track = view.page.tracks()[0].clone();
                view.patch_confirmed(&Mutation::RateSong { track, rating: Rating::Like }, cx);
                window.focus(&view.focus, cx);
                reply(view, page("Updated playlist", "fresh-tail"), cx);
                assert_eq!(view.page.title, "Updated playlist");
                assert_eq!(view.rows.len(), 150, "Refresh must preserve loaded continuations");
                assert_eq!(view.page.tracks()[0].details.rating, Some(Rating::Like), "An in-flight reply must retain confirmed local changes");
                assert_eq!(view.list_scroll.0.borrow().base_handle.offset(), old_scroll, "Refresh must keep the visible occurrence");
                assert!(view.focus.is_focused(window), "Background refresh must keep focus");
                view.navigate(Route::Explore, cx);
                assert!(view.catalog_requests.pending.is_empty());
                let track = crate::app::sync::tests::page(1, false).tracks().remove(0);
                view.patch_confirmed(&Mutation::RateSong { track, rating: Rating::Dislike }, cx);
                view.navigate(playlist.clone(), cx);
                assert_eq!(view.page.tracks()[0].details.rating, Some(Rating::Dislike), "Confirmed ratings overlay cached pages");
                assert!(view.catalog_requests.pending.is_empty());
                assert_eq!(view.list_scroll.0.borrow().base_handle.offset().y, px(0.), "Forward navigation resets scroll");
                view.sync.page_fetched_at = Some(Instant::now() - Duration::from_secs(181));
                view.navigate(Route::Explore, cx);
                view.navigate(playlist.clone(), cx);
                assert_eq!(view.rows.len(), 150, "Stale forward navigation shows its page synchronously");
                assert!(matches!(view.catalog_requests.pending.try_recv().unwrap().query, Query::Revalidate(_)));
                view.revalidate_page(cx);
                reply(view, page("Revisited playlist", "fresh-tail"), cx);
                view.navigate(Route::Explore, cx);
                view.go_back(cx);
                assert_eq!(view.page.title, "Revisited playlist");
                assert!(view.catalog_requests.pending.is_empty(), "Fresh signed-in Back must not revalidate");
                view.navigate(Route::Explore, cx);
                view.patch_confirmed(&Mutation::RemoveFromPlaylist { id: "VLcached".into(), set_video_id: "occurrence-0".into(), video_id: "00000000000".into() }, cx);
                view.navigate(playlist.clone(), cx);
                assert_eq!(view.rows.len(), 149, "The cached playlist must lose the confirmed removal immediately");
                assert!(view.page.tracks().iter().all(|track| track.details.set_video_id.as_deref() != Some("occurrence-0")));
                assert!(view.catalog_requests.is_loading(CatalogTarget::Page), "Playlist edits also invalidate remote order and continuations");
                reply(view, page("After edit", "edit-tail"), cx);
                view.navigate(Route::Explore, cx);
                view.patch_confirmed(&Mutation::DeletePlaylist { id: "VLcached".into() }, cx);
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
                view.navigate(Route::Library(Section::Songs), cx);
                assert_eq!(view.page.title, "Sorted songs");
                assert_eq!(view.page.continuation.as_ref().unwrap().token(), "sorted-tail");
                assert!(view.catalog_requests.pending.is_empty());
                view.reload_current_page(cx);
                let job = view.catalog_requests.pending.try_recv().unwrap();
                assert!(matches!(job.query, Query::Revalidate(query) if matches!(&*query, Query::Reload(token) if token.token() == "sort-request")), "Reload must retain the selected sort's request");
                while view.catalog_requests.pending.try_recv().is_ok() {}
                view.navigate(Route::Saved, cx);
                reply(view, page("Saved collections", "saved-tail"), cx);
                view.navigate(Route::Explore, cx);
                let collection = serde_json::from_value(serde_json::json!({"id":"MPREnew", "title":"New album", "subtitle":"Album"})).unwrap();
                view.patch_confirmed(&Mutation::CollectionLibrary { collection, save: true }, cx);
                view.navigate(Route::Saved, cx);
                assert_eq!(view.page.title, "Saved collections");
                assert!(view.catalog_requests.is_loading(CatalogTarget::Page), "Saving a new collection must invalidate cached library membership");
                reply(view, page("Updated collections", "updated-tail"), cx);

                let previous_revision = view.account_revision;
                view.account = Some(connection(session("second")));
                view.clear_account_state(cx);
                assert!(view.back.is_empty());
                assert!(view.page_cache.is_empty());
                view.navigate(Route::Home, cx);
                assert!(view.rows.is_empty(), "Another account must not see the guest or first account's cached Home");
                let generation = view.catalog_requests.generations[CatalogTarget::Page as usize].load(Ordering::Relaxed);
                view.catalog_result(CatalogResult { id: generation, account_revision: previous_revision, append: false, revalidate: false, result: Ok(page("Wrong account", "wrong-tail")), session_rejected: false, target: CatalogTarget::Page }, cx);
                assert!(view.rows.is_empty());
                assert!(view.catalog_requests.is_loading(CatalogTarget::Page));
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
                view.navigate(Route::Search(Search { query: "back rows".into(), filter: SearchFilter::Songs, scope: SearchScope::Catalog }), cx);
                reply(view, page("Grouped rows", "grouped-tail"), cx);
                view.navigate(Route::Explore, cx);
                reply(view, page("Explore", "explore-again"), cx);
                for entry in &mut view.page_cache {
                    if matches!(&entry.route, Route::Search(search) if search.query == "back rows") {
                        entry.page.shelves[0].items.truncate(2);
                    }
                }
                view.go_back(cx);
                assert_eq!(view.rows.len(), 2);
                assert_eq!(view.detail_list.state.item_count(), view.list_rows.len(), "Back must update its old list state when cached rows changed");
                view.detail_list.state.scroll_to(ListOffset::default());
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
