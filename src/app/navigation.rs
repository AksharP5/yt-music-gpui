//! Routes, catalog workers, request generations, navigation history, and page loading.

use super::*;
use crate::catalog::{BrowseChoice, LibraryFilters};
use crate::playlist_choices::{ChoiceList, Choices, LIKED_MUSIC_ART};
use std::sync::{Mutex, PoisonError, mpsc};

#[derive(Clone, PartialEq)]
pub(in crate::app) enum Route {
    Home,
    Explore,
    Search(Search),
    Browse(BrowseLink),
    Liked,
    Recent,
    Saved,
    Library(Section),
}
impl Route {
    fn same_page(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Home, Self::Browse(link)) | (Self::Browse(link), Self::Home) => {
                link.id == "FEmusic_home" && link.params.is_none() && link.chart_country.is_none()
            }
            (Self::Explore, Self::Browse(link)) | (Self::Browse(link), Self::Explore) => {
                link.id == "FEmusic_explore"
                    && link.params.is_none()
                    && link.chart_country.is_none()
            }
            (Self::Browse(left), Self::Browse(right)) => {
                left.id == right.id
                    && left.params == right.params
                    && left.chart_country == right.chart_country
            }
            _ => self == other,
        }
    }

    /// The account library page behind a library route.
    pub(in crate::app) fn library_section(&self) -> Option<Section> {
        match self {
            Self::Liked => Some(Section::Liked),
            Self::Recent => Some(Section::History),
            Self::Saved => Some(Section::Landing),
            Self::Library(section) => Some(*section),
            _ => None,
        }
    }
    /// Where a library chip leads.
    pub(in crate::app) fn library_chip(link: BrowseLink) -> Self {
        match Section::from_browse_id(&link.id) {
            Some(Section::Landing) => Self::Saved,
            Some(section) => Self::Library(section),
            None => Self::Browse(link),
        }
    }
    pub(in crate::app) fn is_artist(&self) -> bool {
        matches!(self, Self::Browse(link) if link.id.starts_with("UC"))
    }
    pub(in crate::app) fn is_home(&self) -> bool {
        matches!(self, Self::Home)
            || matches!(self, Self::Browse(link) if link.id == "FEmusic_home")
    }
}
#[derive(Clone)]
pub(in crate::app) enum Query {
    Home,
    Explore,
    Search(Search),
    Browse(BrowseLink),
    More {
        token: Continuation,
        search: Option<Search>,
    },
    Library(Section),
    SidebarPlaylists,
    SidebarMore(Continuation),
    PlaylistChoices(Vec<String>),
    PlaylistMetadata(sync::PlaylistRefresh),
    PlaylistSuggestions {
        playlist: BrowseLink,
        token: Option<Continuation>,
    },
    /// The library's first page in the order specified by this reply's sort control.
    Reload(Continuation),
    Revalidate(Box<Query>),
}
impl Query {
    pub(in crate::app) fn target(&self) -> CatalogTarget {
        match self {
            Self::SidebarPlaylists | Self::SidebarMore(_) => CatalogTarget::Sidebar,
            Self::PlaylistChoices(_) => CatalogTarget::PlaylistChoices,
            Self::PlaylistMetadata(_) => CatalogTarget::PlaylistMetadata,
            Self::PlaylistSuggestions { .. } => CatalogTarget::Suggestions,
            Self::Revalidate(query) => query.target(),
            _ => CatalogTarget::Page,
        }
    }
}
#[derive(Clone)]
pub(in crate::app) struct Job {
    pub(in crate::app) id: u64,
    pub(in crate::app) account_revision: u64,
    pub(in crate::app) account_generation: u64,
    pub(in crate::app) history_generation: u64,
    pub(in crate::app) query: Query,
    pub(in crate::app) session: Option<Arc<Session>>,
}
pub(in crate::app) struct CatalogResult {
    pub(in crate::app) id: u64,
    pub(in crate::app) account_revision: u64,
    pub(in crate::app) account_generation: u64,
    pub(in crate::app) history_generation: u64,
    pub(in crate::app) append: bool,
    pub(in crate::app) revalidate: bool,
    pub(in crate::app) result: Result<Page, String>,
    pub(in crate::app) session_rejected: bool,
    pub(in crate::app) target: CatalogTarget,
}
pub(in crate::app) struct PlaylistChoicesResult {
    pub(in crate::app) job: Job,
    pub(in crate::app) result: Result<Choices, String>,
    pub(in crate::app) session_rejected: bool,
}

enum CatalogEvent {
    Page(Box<CatalogResult>),
    PlaylistChoices(Box<PlaylistChoicesResult>),
    Suggestions(Box<suggestions::SuggestionsResult>),
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub(in crate::app) enum CatalogTarget {
    Page,
    Sidebar,
    PlaylistChoices,
    PlaylistMetadata,
    Suggestions,
}
impl CatalogTarget {
    pub(in crate::app) const ALL: [Self; 5] = [
        Self::Page,
        Self::Sidebar,
        Self::PlaylistChoices,
        Self::PlaylistMetadata,
        Self::Suggestions,
    ];
}
struct RestoringRequest {
    id: u64,
    result: Option<CatalogResult>,
}

struct RestoringCatalog {
    session: Arc<Session>,
    account_revision: u64,
    requests: [Option<RestoringRequest>; CatalogTarget::ALL.len()],
}

pub(in crate::app) struct CatalogRequests {
    pub(in crate::app) generations: Arc<[AtomicU64; CatalogTarget::ALL.len()]>,
    pub(in crate::app) loading: [bool; CatalogTarget::ALL.len()],
    pub(in crate::app) jobs: async_channel::Sender<Job>,
    pub(in crate::app) pending: async_channel::Receiver<Job>,
    metadata_queue: Option<(async_channel::Sender<Job>, async_channel::Receiver<Job>)>,
    suggestions_queue: Option<(async_channel::Sender<Job>, async_channel::Receiver<Job>)>,
    pub(in crate::app) playlist_choices: Option<Choices>,
    pub(in crate::app) playlist_choices_error: Option<String>,
    restoring: Option<RestoringCatalog>,
}
impl CatalogRequests {
    pub(in crate::app) fn new() -> Self {
        let (jobs, pending) = async_channel::bounded(CatalogTarget::ALL.len());
        Self {
            generations: Arc::new(std::array::from_fn(|_| AtomicU64::new(0))),
            loading: [false; CatalogTarget::ALL.len()],
            jobs,
            pending,
            metadata_queue: None,
            suggestions_queue: None,
            playlist_choices: None,
            playlist_choices_error: None,
            restoring: None,
        }
    }

    pub(super) fn restore(
        &mut self,
        session: Arc<Session>,
        account_revision: u64,
        account_generation: u64,
        history_generation: u64,
    ) {
        self.discard_restore();
        let mut requests = std::array::from_fn(|_| None);
        for query in [Query::Home, Query::SidebarPlaylists] {
            let target = query.target();
            if self
                .request(
                    query,
                    account_revision,
                    account_generation,
                    history_generation,
                    Some(session.clone()),
                )
                .is_ok()
            {
                requests[target as usize] = Some(RestoringRequest {
                    id: self.generations[target as usize].load(Ordering::Relaxed),
                    result: None,
                });
            }
        }
        self.restoring = Some(RestoringCatalog {
            session,
            account_revision,
            requests,
        });
    }

    pub(super) fn restoring_session_is(&self, session: &Arc<Session>) -> bool {
        self.restoring
            .as_ref()
            .is_some_and(|restoring| Arc::ptr_eq(&restoring.session, session))
    }

    pub(super) fn discard_restore(&mut self) {
        let Some(restoring) = self.restoring.take() else {
            return;
        };
        for target in CatalogTarget::ALL {
            if restoring.requests[target as usize]
                .as_ref()
                .is_some_and(|request| {
                    request.id == self.generations[target as usize].load(Ordering::Relaxed)
                })
            {
                self.cancel(target);
            }
        }
    }

    // Only activation of the exact saved session can carry requests across the
    // account reset. Refreshed cookies, even for the same account, start over.
    pub(super) fn reset_account(&mut self, session: Option<&Arc<Session>>, account_revision: u64) {
        if !self.restoring.as_ref().is_some_and(|restoring| {
            restoring.account_revision == account_revision
                && session.is_some_and(|session| Arc::ptr_eq(session, &restoring.session))
        }) {
            self.discard_restore();
        }
        for target in CatalogTarget::ALL {
            let preserved = self.restoring.as_ref().is_some_and(|restoring| {
                restoring.requests[target as usize]
                    .as_ref()
                    .is_some_and(|request| {
                        request.id == self.generations[target as usize].load(Ordering::Relaxed)
                    })
            });
            if !preserved {
                self.cancel(target);
            }
        }
    }

    fn take_restored(
        &mut self,
        query: &Query,
        session: Option<&Arc<Session>>,
        account_revision: u64,
    ) -> Option<RestoringRequest> {
        if !matches!(query, Query::Home | Query::SidebarPlaylists) {
            return None;
        }
        let restoring = self.restoring.as_mut()?;
        if restoring.account_revision != account_revision
            || !session.is_some_and(|session| Arc::ptr_eq(session, &restoring.session))
        {
            return None;
        }
        let target = query.target();
        let request = restoring.requests[target as usize].take()?;
        if restoring.requests.iter().all(Option::is_none) {
            self.restoring = None;
        }
        (request.id == self.generations[target as usize].load(Ordering::Relaxed)).then_some(request)
    }

    pub(in crate::app) fn request(
        &mut self,
        query: Query,
        account_revision: u64,
        account_generation: u64,
        history_generation: u64,
        session: Option<Arc<Session>>,
    ) -> Result<(), String> {
        let target = query.target();
        self.cancel(target);
        let id = self.generations[target as usize].load(Ordering::Relaxed);
        let (jobs, pending) = match (target, &self.metadata_queue, &self.suggestions_queue) {
            (CatalogTarget::PlaylistMetadata, Some((jobs, pending)), _) => (jobs, pending),
            (CatalogTarget::Suggestions, _, Some((jobs, pending))) => (jobs, pending),
            _ => (&self.jobs, &self.pending),
        };
        let mut other = Vec::with_capacity(CatalogTarget::ALL.len() - 1);
        while let Ok(job) = pending.try_recv() {
            if job.query.target() != target {
                other.push(job);
            }
        }
        // Keep one pending request per destination, including sidebar and picker reads.
        for job in other.into_iter().chain(std::iter::once(Job {
            id,
            account_revision,
            account_generation,
            history_generation,
            query,
            session,
        })) {
            jobs.try_send(job).map_err(|_| {
                "The music connection stopped. Restart the app to reconnect.".to_owned()
            })?;
        }
        self.loading[target as usize] = true;
        Ok(())
    }

    pub(in crate::app) fn cancel(&mut self, target: CatalogTarget) {
        self.generations[target as usize].fetch_add(1, Ordering::Relaxed);
        self.loading[target as usize] = false;
        if target == CatalogTarget::PlaylistChoices {
            self.playlist_choices = None;
            self.playlist_choices_error = None;
        }
    }

    pub(in crate::app) fn finish(&mut self, target: CatalogTarget, id: u64) -> bool {
        if id != self.generations[target as usize].load(Ordering::Relaxed) {
            return false;
        }
        self.loading[target as usize] = false;
        true
    }

    pub(in crate::app) fn is_loading(&self, target: CatalogTarget) -> bool {
        self.loading[target as usize]
    }
}
const PAGE_CACHE_LIMIT: usize = 12;

// Forward visits only. Back owns a separate snapshot for each visit.
pub(in crate::app) struct CachedPage {
    pub(in crate::app) route: Route,
    pub(in crate::app) account_revision: u64,
    pub(in crate::app) account_generation: u64,
    pub(in crate::app) history_generation: u64,
    pub(in crate::app) page: Page,
    pub(in crate::app) fetched_at: Instant,
    pub(in crate::app) coverage: sync::PageCoverage,
}

// A Back restore changes list_id without adopting this forward visit's cache metadata.
pub(in crate::app) struct ForwardPage {
    list_id: usize,
    pub(super) fetched_at: Option<Instant>,
    account_generation: u64,
    history_generation: u64,
}

impl ForwardPage {
    pub(super) fn new(list_id: usize, account_generation: u64, history_generation: u64) -> Self {
        Self {
            list_id,
            fetched_at: None,
            account_generation,
            history_generation,
        }
    }
}

pub(in crate::app) struct NavigationState {
    pub(in crate::app) route: Route,
    pub(in crate::app) page: Option<Page>,
    pub(in crate::app) fetched_at: Option<Instant>,
    pub(in crate::app) coverage: sync::PageCoverage,
    pub(in crate::app) page_error: Option<String>,
    pub(in crate::app) list_scroll: UniformListScrollHandle,
    pub(in crate::app) detail_list: DetailList,
    pub(in crate::app) shelf_scroll: HashMap<usize, ScrollHandle>,
}

impl Music {
    pub(super) fn page_is_current(
        &self,
        account_generation: u64,
        history_generation: u64,
        route: &Route,
    ) -> bool {
        let history = route.library_section() == Some(Section::History)
            || matches!(route, Route::Browse(link) if Section::from_browse_id(&link.id) == Some(Section::History));
        account_generation == self.account_generation
            && (!history || history_generation == self.history_generation)
    }

    pub(in crate::app) fn request(&mut self, query: Query, cx: &mut Context<Self>) {
        let target = query.target();
        if target == CatalogTarget::Sidebar {
            self.sync.sidebar_changes.clear();
        }
        if target == CatalogTarget::Page {
            if let Some(page) = &mut self.forward_page
                && page.list_id == self.list_id
                && matches!(&query, Query::Reload(_))
            {
                page.fetched_at = None;
            }
            self.sync.page_changes.clear();
            self.sync.page_reload = None;
            if self.error == self.sync.page_error {
                self.error = None;
            }
            self.sync.page_error = None;
        }
        let session = self
            .account
            .as_ref()
            .map(|connection| connection.session.clone());
        if let Some(request) =
            self.catalog_requests
                .take_restored(&query, session.as_ref(), self.account_revision)
        {
            if let Some(result) = request.result {
                self.catalog_result(result, cx);
            }
            cx.notify();
            return;
        }
        let result = self.catalog_requests.request(
            query,
            self.account_revision,
            self.account_generation,
            self.history_generation,
            session,
        );
        if target == CatalogTarget::Sidebar {
            self.sidebar_error = result.err();
        } else if target == CatalogTarget::PlaylistChoices {
            self.catalog_requests.playlist_choices_error = result.err();
        } else if target == CatalogTarget::Page {
            self.sync.page_error = result.err();
            if let Some(error) = &self.sync.page_error {
                self.error = Some(error.clone());
            }
        } else if target == CatalogTarget::PlaylistMetadata
            && let Err(error) = result
        {
            self.sync.playlist_refreshes.clear();
            self.error.get_or_insert(error);
        } else if target == CatalogTarget::Suggestions
            && let Err(error) = result
            && let Some(suggestions) = &mut self.suggestions
        {
            suggestions.error = Some(error);
        }
        cx.notify();
    }

    pub(in crate::app) fn navigate(&mut self, route: Route, cx: &mut Context<Self>) {
        self.close_narrow_search(cx);
        // Like the web's drawer, the guide closes once it has taken you somewhere.
        self.guide_drawer = false;
        // A guest's Liked Music is the local list of liked songs.
        let route = match route {
            Route::Browse(link) if self.account.is_none() && link.id == "VLLM" => Route::Liked,
            route => route,
        };
        // Like the web, clicking the page you're on loads it again.
        let reopening = self.route.same_page(&route);
        if let Some(active) = &self.forward_page
            && active.list_id == self.list_id
            && let Some(fetched_at) = active.fetched_at
            && self.sync.page_error.is_none()
            && self.page_is_current(
                active.account_generation,
                active.history_generation,
                &self.route,
            )
        {
            self.page_cache.push_back(CachedPage {
                route: self.route.clone(),
                account_revision: self.account_revision,
                account_generation: active.account_generation,
                history_generation: active.history_generation,
                page: self.page.clone(),
                fetched_at,
                coverage: self.sync.page_coverage,
            });
        }
        self.clear_suggestions();
        if self.route != route {
            self.forward.clear();
            let state = self.take_navigation_state();
            self.back.push(state);
            if self.back.len() > 12 {
                self.back.remove(0);
            }
        }
        let cached = self
            .page_cache
            .iter()
            .position(|entry| {
                entry.account_revision == self.account_revision && entry.route.same_page(&route)
            })
            .and_then(|index| self.page_cache.remove(index))
            .filter(|entry| {
                !reopening
                    && self.page_is_current(
                        entry.account_generation,
                        entry.history_generation,
                        &route,
                    )
            });
        if self.page_cache.len() > PAGE_CACHE_LIMIT {
            self.page_cache.pop_front();
        }
        self.forward_page = Some(ForwardPage::new(
            self.list_id.wrapping_add(1),
            self.account_generation,
            self.history_generation,
        ));
        self.load_route(route, cached, cx);
    }

    fn take_navigation_state(&mut self) -> NavigationState {
        let loaded = !self.rows.is_empty()
            || (!self.catalog_requests.is_loading(CatalogTarget::Page)
                && self.sync.page_error.is_none());
        NavigationState {
            route: self.route.clone(),
            page: loaded.then(|| std::mem::take(&mut self.page)),
            fetched_at: self.sync.page_fetched_at,
            coverage: self.sync.page_coverage,
            page_error: self.sync.page_error.take(),
            list_scroll: std::mem::replace(&mut self.list_scroll, UniformListScrollHandle::new()),
            shelf_scroll: std::mem::take(&mut self.shelf_scroll),
            detail_list: std::mem::replace(&mut self.detail_list, DetailList::new()),
        }
    }

    fn load_route(&mut self, route: Route, cached: Option<CachedPage>, cx: &mut Context<Self>) {
        self.focus_after_navigation(cx);
        self.artwork.cancel_pending();
        self.close_player_page();
        self.list_id = self.list_id.wrapping_add(1);
        self.list_scroll = UniformListScrollHandle::new();
        self.detail_list = DetailList::new();
        self.shelf_scroll.clear();
        self.route = route.clone();
        self.sync.page_fetched_at = None;
        self.sync.page_reload = None;
        self.sync.page_coverage = sync::PageCoverage::default();
        self.sync.page_error = None;
        self.error = None;
        self.notice = None;
        self.page.shelves.clear();
        self.page.header = None;
        self.page.background = None;
        self.page.continuation = None;
        self.page.editable = false;
        self.page.description.clear();
        self.page.privacy = None;
        self.page.chips.clear();
        self.page.countries.clear();
        self.page.correction = None;
        self.page.library = None;
        self.page.playlist_sort.clear();
        if self.overlay.as_ref().is_some_and(Overlay::in_page) {
            self.drop_overlay(cx);
        }
        self.rows.clear();
        self.list_rows.clear();
        if !matches!(route, Route::Search(_)) {
            // Like the web, the search box only keeps a query on its results.
            self.search
                .update(cx, |input, cx| input.set_query(String::new(), cx));
        }
        if let Some(entry) = cached {
            self.catalog_requests.cancel(CatalogTarget::Page);
            if let Route::Search(search) = &route {
                self.search
                    .update(cx, |input, cx| input.set_query(search.query.clone(), cx));
            }
            self.page = entry.page;
            self.sync.page_fetched_at = Some(entry.fetched_at);
            self.sync.page_coverage = entry.coverage;
            self.forward_page = Some(ForwardPage {
                list_id: self.list_id,
                fetched_at: Some(entry.fetched_at),
                account_generation: entry.account_generation,
                history_generation: entry.history_generation,
            });
            self.rebuild_rows();
            self.overlay_ratings();
            if sync::needs_refresh(Some(entry.fetched_at), Instant::now()) {
                // Forward visits start at the top and replace expired loaded tails.
                self.request(self.page_query(), cx);
            } else {
                self.load_suggestions(true, cx);
            }
            cx.notify();
            return;
        }
        match route {
            Route::Home => {
                self.page.title = "Home".into();
                self.request(Query::Home, cx);
            }
            Route::Explore => {
                self.page.title = "Explore".into();
                self.request(Query::Explore, cx);
            }
            Route::Search(search) => {
                self.page.title = format!("Results for \"{}\"", search.query);
                self.search
                    .update(cx, |input, cx| input.set_query(search.query.clone(), cx));
                self.request(Query::Search(search), cx);
            }
            Route::Browse(link) => {
                self.page.title = link.title.clone();
                self.request(Query::Browse(link), cx);
            }
            _ if self.account.is_some()
                && let Some(section) = route.library_section() =>
            {
                self.page.title = section.title().into();
                self.request(Query::Library(section), cx);
            }
            _ => {
                self.catalog_requests.cancel(CatalogTarget::Page);
                self.refresh_local();
                cx.notify();
            }
        }
    }

    pub(in crate::app) fn rebuild_rows(&mut self) {
        self.rows.clear();
        self.list_rows.clear();
        self.append_rows(0);
        if let Some(Overlay::Track { track, index, .. }) = &mut self.overlay {
            *index = self.rows.iter().position(|item| matches!(item, Item::Track(current) if current.id == track.id && current.details.set_video_id == track.details.set_video_id));
        }
        if self.route.is_home() || matches!(self.route, Route::Explore) {
            self.detail_list.state.remeasure();
        } else if self.grouped_rows() {
            self.detail_list.reset_rows(self.list_rows.len());
        } else if self.collection_grid() {
            // The grid sizes its rows from the window when it renders.
            self.detail_list.state.remeasure();
        } else {
            self.detail_list = DetailList::new();
        }
    }

    pub(in crate::app) fn append_rows(&mut self, first_shelf: usize) -> usize {
        let start = self.playlist_rows_end();
        self.list_rows.truncate(start);
        self.shelf_heights.truncate(first_shelf);
        let sectioned = self.page.header.is_some();
        let browse_sections = self.browse_sections();
        let search_all =
            matches!(&self.route, Route::Search(search) if search.filter == SearchFilter::All);
        for (index, shelf) in self.page.shelves.iter().enumerate().skip(first_shelf) {
            if self.history_groups() {
                self.rows.extend(shelf.items.iter().cloned());
                continue;
            }
            let featured = search_all && shelf.layout == ShelfLayout::Featured;
            let carousel = (browse_sections
                && (!shelf.title.is_empty() || shelf.layout == ShelfLayout::Grid))
                || sectioned
                    && matches!(
                        shelf.layout,
                        ShelfLayout::Cards
                            | ShelfLayout::SmallCards
                            | ShelfLayout::Videos
                            | ShelfLayout::Episodes
                            | ShelfLayout::Grid
                    );
            if featured {
                self.list_rows.push(ListRow::Featured(index));
            } else if carousel {
                self.list_rows.push(ListRow::Shelf(index));
            } else if !shelf.title.is_empty() {
                self.list_rows.push(ListRow::Heading(index));
            }
            for (item_index, item) in shelf.items.iter().enumerate() {
                if !carousel && !(featured && item_index <= 3) {
                    self.list_rows.push(ListRow::Item(self.rows.len()));
                }
                self.rows.push(item.clone());
            }
            if sectioned && !carousel && shelf.more.is_some() {
                self.list_rows.push(ListRow::More(index));
            }
        }
        if self.history_groups() {
            self.list_rows.clear();
            let mut groups: Vec<(usize, Vec<usize>)> = Vec::new();
            let mut days = HashMap::new();
            let mut offset = 0;
            for (index, shelf) in self.page.shelves.iter().enumerate() {
                let group = *days.entry(shelf.title.as_str()).or_insert_with(|| {
                    groups.push((index, Vec::new()));
                    groups.len() - 1
                });
                groups[group].1.extend(offset..offset + shelf.items.len());
                offset += shelf.items.len();
            }
            for (shelf, items) in groups {
                if items.is_empty() {
                    continue;
                }
                if !self.page.shelves[shelf].title.is_empty() {
                    self.list_rows.push(ListRow::Heading(shelf));
                }
                self.list_rows.extend(items.into_iter().map(ListRow::Item));
            }
        }
        self.append_suggestion_rows();
        start
    }

    pub(in crate::app) fn refresh_local(&mut self) {
        let (title, rows, library) = match self.route {
            Route::Liked => (
                "Liked songs",
                self.library
                    .liked
                    .iter()
                    .cloned()
                    .map(Item::Track)
                    .collect(),
                None,
            ),
            Route::Recent => (
                "Recently played",
                self.library
                    .recent
                    .iter()
                    .cloned()
                    .map(Item::Track)
                    .collect(),
                None,
            ),
            Route::Saved => (
                "Library",
                std::iter::once(Item::Collection(Collection {
                    id: "VLLM".into(),
                    title: "Liked Music".into(),
                    subtitle: "Auto playlist".into(),
                    thumbnail: Some(LIKED_MUSIC_ART.into()),
                    details: Default::default(),
                }))
                .chain(self.library.saved.iter().cloned().map(Item::Collection))
                .collect(),
                Some(local_library_filters(None)),
            ),
            Route::Library(section) => (
                section.title(),
                self.library
                    .saved
                    .iter()
                    .filter(|collection| local_section(&collection.id) == section)
                    .cloned()
                    .map(Item::Collection)
                    .collect(),
                Some(local_library_filters(Some(section))),
            ),
            _ => return,
        };
        self.page = Page {
            title: title.into(),
            header: None,
            background: None,
            shelves: vec![Shelf {
                strapline: String::new(),
                layout: ShelfLayout::Cards,
                title: String::new(),
                more: None,
                items: rows,
            }],
            continuation: None,
            editable: false,
            description: String::new(),
            privacy: None,
            chips: Vec::new(),
            countries: Vec::new(),
            search_filters: Vec::new(),
            correction: None,
            library,
            playlist_sort: Vec::new(),
        };
        self.rebuild_rows();
    }

    pub(in crate::app) fn go_back(&mut self, cx: &mut Context<Self>) {
        self.navigate_history(NavigationDirection::Back, cx);
    }

    pub(in crate::app) fn navigate_history(
        &mut self,
        direction: NavigationDirection,
        cx: &mut Context<Self>,
    ) {
        if direction == NavigationDirection::Back && self.player_open {
            self.focus_after_navigation(cx);
            self.close_player_page();
            cx.notify();
            return;
        }
        let state = match direction {
            NavigationDirection::Back => self.back.pop(),
            NavigationDirection::Forward => self.forward.pop(),
        };
        let Some(state) = state else { return };
        self.focus_after_navigation(cx);
        self.clear_suggestions();
        let previous = self.take_navigation_state();
        match direction {
            NavigationDirection::Back => self.forward.push(previous),
            NavigationDirection::Forward => self.back.push(previous),
        }
        self.close_player_page();
        self.sync.page_reload = None;
        self.sync.page_changes.clear();
        if let Some(page) = state.page {
            self.catalog_requests.cancel(CatalogTarget::Page);
            self.artwork.cancel_pending();
            self.list_id = self.list_id.wrapping_add(1);
            self.route = state.route;
            let query = match &self.route {
                Route::Search(search) => search.query.clone(),
                _ => String::new(),
            };
            self.search
                .update(cx, |input, cx| input.set_query(query, cx));
            self.page = page;
            self.sync.page_fetched_at = state.fetched_at;
            self.sync.page_coverage = state.coverage;
            self.sync.page_error = state.page_error;
            self.error = self.sync.page_error.clone();
            self.notice = None;
            self.rebuild_rows();
            if self.account.is_some() {
                self.revalidate_page(cx);
            } else {
                self.refresh_local();
            }
        } else {
            self.route = state.route.clone();
            self.load_route(state.route, None, cx);
        }
        self.list_scroll = state.list_scroll;
        self.detail_list = state.detail_list;
        self.shelf_scroll = state.shelf_scroll;
        if self.grouped_rows() {
            self.detail_list.reset_rows(self.list_rows.len());
        }
        cx.notify();
    }

    pub(in crate::app) fn playlist_choices_result(
        &mut self,
        response: PlaylistChoicesResult,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Query::PlaylistChoices(video_ids) = &response.job.query else {
            return;
        };
        let Some(Overlay::PlaylistPicker { tracks, .. }) = &self.overlay else {
            return;
        };
        if response.job.account_revision != self.account_revision
            || !self.account.as_ref().is_some_and(|account| {
                response
                    .job
                    .session
                    .as_ref()
                    .is_some_and(|session| Arc::ptr_eq(session, &account.session))
            })
            || !tracks.iter().map(|track| &track.id).eq(video_ids)
            || !self
                .catalog_requests
                .finish(CatalogTarget::PlaylistChoices, response.job.id)
        {
            return;
        }
        if response.job.account_generation != self.account_generation {
            self.refresh_playlist_choices(cx);
            return;
        }
        if response.session_rejected {
            self.refresh_session(false, cx);
        }
        match response.result {
            Ok(choices) => {
                if let Some(target) = self.picker_focus.focused(window) {
                    let previous = self.playlist_picker_choices().index_of(&target);
                    match ChoiceList::Server(&choices).index_of(&target) {
                        Some(index) if Some(index) != previous => {
                            self.picker_scroll
                                .scroll_to_item(index, ScrollStrategy::Nearest);
                        }
                        None => window.focus(&self.overlay_focus, cx),
                        _ => {}
                    }
                }
                self.catalog_requests.playlist_choices = Some(choices);
                self.catalog_requests.playlist_choices_error = None;
            }
            Err(error) => self.catalog_requests.playlist_choices_error = Some(error),
        }
        cx.notify();
    }

    pub(in crate::app) fn catalog_result(&mut self, result: CatalogResult, cx: &mut Context<Self>) {
        // Quarantine early responses before finish(), refresh, ratings, page/cache,
        // or sidebar state can observe them. Activation consumes each held request.
        if let Some(restoring) = &mut self.catalog_requests.restoring
            && result.account_revision == restoring.account_revision
            && let Some(request) = &mut restoring.requests[result.target as usize]
            && result.id == request.id
        {
            if result.session_rejected {
                self.catalog_requests.discard_restore();
            } else {
                request.result = Some(result);
            }
            return;
        }
        if result.account_revision != self.account_revision
            || !self.catalog_requests.finish(result.target, result.id)
        {
            return;
        }
        if result.session_rejected {
            self.refresh_session(false, cx);
        }
        if result.target == CatalogTarget::PlaylistMetadata {
            self.playlist_metadata_result(result.result, result.account_generation, cx);
            return;
        }
        if result.target == CatalogTarget::Sidebar {
            match result.result {
                Ok(page) => {
                    self.sync.update_sidebar(
                        &mut self.sidebar_playlists,
                        page,
                        result.append,
                        result.revalidate,
                    );
                    self.sidebar_error = None;
                }
                Err(error) => {
                    self.sync.sidebar_changes.clear();
                    self.sidebar_error = Some(error);
                }
            }
            cx.notify();
            return;
        }
        if !self.startup_settled && result.result.is_ok() {
            // Startup is done with the network. Prepare the song Play would start,
            // or with nothing queued, fetch the lookup's visitor ID for the first click.
            self.startup_settled = true;
            if self.library.queue.current().is_none()
                && let Some(player) = &self.player
            {
                player.warm_up();
            }
            self.notify_playback(cx);
        }
        match result.result {
            Ok(mut page) => {
                // A playlist sorted in place arrives without its header.
                if !result.append
                    && page.header.is_none()
                    && !page.playlist_sort.is_empty()
                    && let Some(header) = &self.page.header
                {
                    page.header = Some(header.clone());
                    page.title = self.page.title.clone();
                    page.description = self.page.description.clone();
                    page.background = self.page.background.clone();
                }
                let reload_verified = result.append
                    || self.sync.page_reload.is_none()
                    || self.playlist_reloaded(&page, result.account_generation, cx);
                // The view now has newer content for this route than any cached copy,
                // however the user leaves it (link, Back, or another Back).
                self.page_cache.retain(|entry| {
                    entry.account_revision != self.account_revision
                        || !entry.route.same_page(&self.route)
                });
                let mut replayed_changes = false;
                let mut retained_coverage = None;
                for change in self.sync.page_changes.drain(..) {
                    replayed_changes |= change.apply(
                        &mut page,
                        &self.route,
                        !result.append,
                        &mut retained_coverage,
                    );
                }
                replayed_changes |=
                    self.replay_playlist_edits(&mut page, !result.append, &mut retained_coverage);
                for track in page
                    .shelves
                    .iter()
                    .flat_map(|shelf| &shelf.items)
                    .filter_map(|item| match item {
                        Item::Track(track) => Some(track),
                        _ => None,
                    })
                {
                    if self.account.is_some()
                        && self
                            .library
                            .queue
                            .current()
                            .is_some_and(|current| current.id == track.id)
                    {
                        continue;
                    }
                    if self.sync.rating_overrides.contains(&track.id) {
                        continue;
                    }
                    if let Some(rating) = track.details.rating {
                        self.ratings.insert(track.id.clone(), rating);
                    }
                }
                self.sync.page_fetched_at =
                    (!replayed_changes && reload_verified).then(Instant::now);
                if let Some(active) = &mut self.forward_page
                    && active.list_id == self.list_id
                    && !result.append
                    && (!result.revalidate || self.sync.page_reload.is_some())
                {
                    active.account_generation = result.account_generation;
                    active.history_generation = result.history_generation;
                    active.fetched_at = self.sync.page_fetched_at;
                }
                self.sync.page_error = None;
                if result.revalidate {
                    let anchor = self.page_anchor();
                    if self.sync.page_reload.is_some() || retained_coverage.is_some() {
                        self.sync.page_coverage = retained_coverage.unwrap_or(sync::PageCoverage {
                            first_page_len: page.shelves.len(),
                            continued: false,
                        });
                        self.page = page;
                    } else {
                        sync::merge_page(&mut self.page, page, &mut self.sync.page_coverage);
                    }
                    self.rebuild_rows();
                    if self.sync.page_reload.is_none() {
                        self.restore_page_anchor(&anchor);
                    }
                } else if result.append {
                    self.sync.page_coverage.continued = true;
                    let first_shelf = self.page.shelves.len();
                    let was_grouped = self.grouped_rows();
                    self.page.continuation = page.continuation;
                    self.page.shelves.extend(page.shelves);
                    // A second titled shelf can turn a plain list into sections.
                    if self.history_groups() || self.grouped_rows() != was_grouped {
                        self.rebuild_rows();
                    } else {
                        let start = self.append_rows(first_shelf);
                        if was_grouped {
                            self.detail_list
                                .replace_tail(start, self.list_rows.len() - start);
                        }
                    }
                    self.update_playlist_rows_snapshot();
                } else {
                    self.sync.page_coverage = retained_coverage.unwrap_or(sync::PageCoverage {
                        first_page_len: page.shelves.len(),
                        continued: false,
                    });
                    self.page = page;
                    if matches!(self.overlay, Some(Overlay::Track { .. })) {
                        self.drop_overlay(cx);
                    }
                    self.rebuild_rows();
                }
                if self.sync.page_reload.is_some() {
                    self.rebuild_rows();
                    self.continue_page_reload(cx);
                }
                if !result.append {
                    self.load_suggestions(false, cx);
                }
            }
            Err(error) => {
                if let Some(active) = &mut self.forward_page
                    && active.list_id == self.list_id
                {
                    active.fetched_at = None;
                }
                self.sync.page_error = Some(error.clone());
                self.error = Some(error);
            }
        }
        self.overlay_ratings();
        cx.notify();
    }
}

impl Music {
    fn focus_after_navigation(&self, cx: &mut Context<Self>) {
        let focus = self.focus.clone();
        // The activated row or card can disappear with the old page. Restore a
        // stable focus target so shortcuts keep reaching Music after navigation.
        cx.defer(move |cx| {
            for window in cx.windows() {
                let _ = window.update(cx, |_, window, cx| window.focus(&focus, cx));
            }
        });
    }
}

/// The guest Home, requested while the window opens.
pub struct HomePrefetch(mpsc::Receiver<anyhow::Result<Page>>);

impl HomePrefetch {
    /// Start loading Home now when startup will show the guest Home, so the
    /// request overlaps opening the window instead of following it.
    pub fn start(startup: Startup) -> Option<Self> {
        if startup == Startup::Restore
            && auth::ACCOUNT_LOGIN_ENABLED
            && Session::has_saved_account()
        {
            return None;
        }
        let (sender, receiver) = mpsc::channel();
        thread::Builder::new()
            .name("home-prefetch".into())
            .spawn(move || {
                let _ = sender.send(Catalog::new().and_then(|catalog| catalog.home()));
            })
            .ok()?;
        Some(Self(receiver))
    }
}

impl CatalogRequests {
    #[expect(
        clippy::expect_used,
        reason = "the app cannot run without this worker, and spawning one fails only when the OS is out of threads"
    )]
    fn start_workers(
        mut self,
        run_worker: impl Fn(async_channel::Receiver<Job>) + Clone + Send + 'static,
    ) -> Self {
        let (jobs, pending) = async_channel::bounded(1);
        let (suggestion_jobs, suggestion_pending) = async_channel::bounded(1);
        // Startup sends Home and the sidebar before verification. A third
        // foreground worker can load a replacement even when both are blocked.
        // Metadata readback can poll for an edit, so it has its own worker.
        let workers = (0..3)
            .map(|worker| (format!("catalog-{worker}"), self.pending.clone()))
            .chain([
                ("catalog-metadata".to_owned(), pending.clone()),
                ("catalog-suggestions".to_owned(), suggestion_pending.clone()),
            ]);
        for (name, receiver) in workers {
            let run_worker = run_worker.clone();
            thread::Builder::new()
                .name(name)
                .spawn(move || run_worker(receiver))
                .expect("start catalog worker");
        }
        self.metadata_queue = Some((jobs, pending));
        self.suggestions_queue = Some((suggestion_jobs, suggestion_pending));
        self
    }

    pub(super) fn start(
        window: &mut Window,
        cx: &mut Context<Music>,
        home: Option<HomePrefetch>,
    ) -> Self {
        let catalog_requests = CatalogRequests::new();
        let (results, completed) = async_channel::unbounded::<CatalogEvent>();
        let generations = catalog_requests.generations.clone();
        let prefetch = Arc::new(Mutex::new(home));
        let catalog_requests = catalog_requests.start_workers(move |receiver| {
            let mut catalog = None;
            while let Ok(job) = receiver.recv_blocking() {
                let target = job.query.target();
                if generations[target as usize].load(Ordering::Relaxed) != job.id {
                    continue;
                }
                let catalog = catalog.get_or_insert_with(Catalog::new);
                if let Query::PlaylistSuggestions { playlist, token } = &job.query {
                    let result = catalog
                        .as_ref()
                        .map_err(|error| anyhow::anyhow!("{error:#}"))
                        .and_then(|catalog| {
                            let catalog = catalog.for_session(job.session.clone());
                            let seed = match token {
                                Some(token) => token.clone(),
                                None => catalog
                                    .browse(playlist)?
                                    .header
                                    .and_then(|header| header.suggestions)
                                    .ok_or_else(|| anyhow::anyhow!("No suggestions available"))?,
                            };
                            anyhow::ensure!(
                                generations[target as usize].load(Ordering::Relaxed) == job.id,
                                "Suggestions read was superseded"
                            );
                            catalog.playlist_suggestions(&seed, &playlist.id)
                        });
                    if generations[target as usize].load(Ordering::Relaxed) != job.id {
                        continue;
                    }
                    let session_rejected = result.as_ref().is_err_and(auth::session_was_rejected);
                    if results
                        .send_blocking(CatalogEvent::Suggestions(Box::new(
                            suggestions::SuggestionsResult {
                                job,
                                result: result.map_err(|error| format!("{error:#}")),
                                session_rejected,
                            },
                        )))
                        .is_err()
                    {
                        break;
                    }
                    continue;
                }
                if let Query::PlaylistChoices(video_ids) = &job.query {
                    let result = catalog
                        .as_ref()
                        .map_err(|error| anyhow::anyhow!("{error:#}"))
                        .and_then(|catalog| {
                            catalog
                                .for_session(job.session.clone())
                                .playlist_choices(video_ids)
                        });
                    if generations[target as usize].load(Ordering::Relaxed) != job.id {
                        continue;
                    }
                    let session_rejected = result.as_ref().is_err_and(auth::session_was_rejected);
                    if results
                        .send_blocking(CatalogEvent::PlaylistChoices(Box::new(
                            PlaylistChoicesResult {
                                job,
                                result: result.map_err(|error| format!("{error:#}")),
                                session_rejected,
                            },
                        )))
                        .is_err()
                    {
                        break;
                    }
                    continue;
                }
                let append = matches!(job.query, Query::More { .. } | Query::SidebarMore(_));
                let revalidate = matches!(job.query, Query::Revalidate(_));
                let query = match &job.query {
                    Query::Revalidate(query) => query.as_ref(),
                    query => query,
                };
                // The first Home job takes the prefetch, and uses it only as a guest.
                let prefetched = matches!(query, Query::Home)
                    .then(|| {
                        prefetch
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .take()
                    })
                    .flatten()
                    .filter(|_| job.session.is_none());
                let result = match catalog {
                    Err(e) => Err(anyhow::anyhow!("{e:#}")),
                    Ok(catalog) => {
                        let catalog = catalog.for_session(job.session);
                        match query {
                            Query::Home => prefetched
                                .and_then(|home| home.0.recv().ok())
                                .unwrap_or_else(|| catalog.home()),
                            Query::Explore => catalog.explore(),
                            Query::Library(section) => catalog.library(*section),
                            Query::SidebarPlaylists => catalog.library(Section::Playlists),
                            Query::SidebarMore(token) | Query::Reload(token) => catalog.more(token),
                            Query::Revalidate(_) => Err(anyhow::anyhow!("Nested revalidation")),
                            Query::PlaylistChoices(_) => {
                                Err(anyhow::anyhow!("Invalid playlist destination request"))
                            }
                            Query::PlaylistSuggestions { .. } => {
                                Err(anyhow::anyhow!("Invalid suggestions request"))
                            }
                            Query::PlaylistMetadata(refresh) => catalog.playlist_metadata(
                                &refresh.id,
                                &refresh.additions,
                                &refresh.removals,
                                refresh.order_page(),
                                || generations[target as usize].load(Ordering::Relaxed) == job.id,
                            ),
                            Query::Search(search) => catalog.search(search),
                            Query::Browse(link) => catalog.browse(link),
                            Query::More {
                                token,
                                search: Some(search),
                            } => catalog.search_more(search, token),
                            Query::More {
                                token,
                                search: None,
                            } => catalog.more(token),
                        }
                    }
                };
                let session_rejected = result.as_ref().is_err_and(auth::session_was_rejected);
                let result = result.map_err(|error| format!("{error:#}"));
                if results
                    .send_blocking(CatalogEvent::Page(Box::new(CatalogResult {
                        id: job.id,
                        account_revision: job.account_revision,
                        account_generation: job.account_generation,
                        history_generation: job.history_generation,
                        append,
                        revalidate,
                        result,
                        session_rejected,
                        target,
                    })))
                    .is_err()
                {
                    break;
                }
            }
        });
        cx.spawn_in(window, async move |view, cx| {
            while let Ok(result) = completed.recv().await {
                if view
                    .update_in(cx, |view, window, cx| match result {
                        CatalogEvent::Page(result) => view.catalog_result(*result, cx),
                        CatalogEvent::PlaylistChoices(result) => {
                            view.playlist_choices_result(*result, window, cx)
                        }
                        CatalogEvent::Suggestions(result) => {
                            view.suggestions_result(*result, window, cx)
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        catalog_requests
    }
}

/// The library chips a guest can fill from saved collections.
const LOCAL_SECTIONS: [Section; 4] = [
    Section::Playlists,
    Section::Albums,
    Section::Artists,
    Section::Podcasts,
];

fn local_section(id: &str) -> Section {
    if id.starts_with("MPRE") {
        Section::Albums
    } else if id.starts_with("UC") {
        Section::Artists
    } else if id.starts_with("MPSP") {
        Section::Podcasts
    } else {
        Section::Playlists
    }
}

/// The guest library's chips, shaped like the web's: every section on the
/// landing page, or only the selected one beside a clear chip.
fn local_library_filters(selected: Option<Section>) -> LibraryFilters {
    let landing = Section::Landing.link();
    let chip = |section: Section| BrowseChoice {
        label: section.title().into(),
        selected: Some(section) == selected,
        target: if Some(section) == selected {
            landing.clone()
        } else {
            section.link()
        },
    };
    match selected {
        Some(section) => LibraryFilters {
            clear: Some(landing.clone()),
            chips: vec![chip(section)],
            sort: Vec::new(),
            empty: Some((
                format!("No {} yet", section.title().to_lowercase()),
                format!("{} you save will show here", section.title()),
            )),
        },
        None => LibraryFilters {
            chips: LOCAL_SECTIONS.into_iter().map(chip).collect(),
            ..LibraryFilters::default()
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BrowseLink, CatalogRequests, Duration, LOCAL_SECTIONS, Query, Route, Section,
        local_library_filters, local_section, sync,
    };
    use std::sync::mpsc;

    #[test]
    fn guest_library_chips_stay_on_local_routes() {
        let landing = local_library_filters(None);
        assert!(landing.clear.is_none());
        assert_eq!(landing.chips.len(), LOCAL_SECTIONS.len());
        for (chip, section) in landing.chips.into_iter().zip(LOCAL_SECTIONS) {
            assert!(Route::library_chip(chip.target) == Route::Library(section));
        }
        let albums = local_library_filters(Some(Section::Albums));
        assert!(Route::library_chip(albums.clear.unwrap()) == Route::Saved);
        assert!(albums.chips[0].selected);
        assert!(Route::library_chip(albums.chips[0].target.clone()) == Route::Saved);
        for (id, section) in [
            ("MPREb_album", Section::Albums),
            ("UCartist", Section::Artists),
            ("MPSPPLshow", Section::Podcasts),
            ("VLPLplaylist", Section::Playlists),
        ] {
            assert_eq!(local_section(id), section);
        }
    }

    #[test]
    fn playlist_background_reads_do_not_delay_a_replacement_page() {
        let (started, observed) = mpsc::channel();
        let (release, blocked) = async_channel::bounded::<()>(1);
        let playlist = BrowseLink::new("VLplaylist", "Playlist");
        let mut requests = CatalogRequests::new().start_workers(move |receiver| {
            while let Ok(job) = receiver.recv_blocking() {
                let replacement = matches!(job.query, Query::Revalidate(_));
                if started.send(job).is_err() {
                    break;
                }
                if !replacement {
                    let _ = blocked.recv_blocking();
                }
            }
        });
        for query in [
            Query::PlaylistMetadata(sync::PlaylistRefresh {
                id: "playlist".into(),
                additions: Vec::new(),
                removals: Vec::new(),
                rows: None,
                moved: false,
                account_generation: 0,
            }),
            Query::PlaylistSuggestions {
                playlist: playlist.clone(),
                token: None,
            },
            Query::Browse(playlist.clone()),
            Query::SidebarPlaylists,
        ] {
            let target = query.target();
            requests.request(query, 0, 0, 0, None).unwrap();
            let job = observed
                .recv_timeout(Duration::from_secs(5))
                .expect("each read must enter its worker");
            assert!(job.query.target() == target);
        }
        requests
            .request(
                Query::Revalidate(Box::new(Query::Browse(playlist))),
                0,
                0,
                0,
                None,
            )
            .unwrap();
        let job = observed
            .recv_timeout(Duration::from_secs(5))
            .expect("Reload must start while Suggestions, metadata, the old page, and the sidebar are blocked");
        assert!(matches!(job.query, Query::Revalidate(_)));
        drop(release);
    }
}
