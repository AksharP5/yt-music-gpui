//! Routes, catalog workers, request generations, navigation history, and page loading.

use super::*;
use crate::catalog::{BrowseChoice, LibraryFilters};

#[derive(Clone, PartialEq)]
pub(in crate::app) struct Search {
    pub(in crate::app) query: String,
    pub(in crate::app) filter: SearchFilter,
    pub(in crate::app) scope: SearchScope,
}

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
            Self::Browse(link) => Section::from_browse_id(&link.id)
                .or_else(|| (link.id == "LM").then_some(Section::Liked)),
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
    PlaylistChoices,
    SidebarPlaylists,
    PlaylistChoicesMore(Continuation),
    SidebarMore(Continuation),
    /// The current page again in another order, replacing it.
    Reload(Continuation),
    Revalidate(Box<Query>),
}
impl Query {
    pub(in crate::app) fn target(&self) -> CatalogTarget {
        match self {
            Self::PlaylistChoices | Self::PlaylistChoicesMore(_) => CatalogTarget::PlaylistPicker,
            Self::SidebarPlaylists | Self::SidebarMore(_) => CatalogTarget::Sidebar,
            Self::Revalidate(query) => query.target(),
            _ => CatalogTarget::Page,
        }
    }
}
pub(in crate::app) struct Job {
    pub(in crate::app) id: u64,
    pub(in crate::app) account_revision: u64,
    pub(in crate::app) query: Query,
    pub(in crate::app) session: Option<Arc<Session>>,
}
pub(in crate::app) struct CatalogResult {
    pub(in crate::app) id: u64,
    pub(in crate::app) account_revision: u64,
    pub(in crate::app) append: bool,
    pub(in crate::app) revalidate: bool,
    pub(in crate::app) result: Result<Page, String>,
    pub(in crate::app) session_rejected: bool,
    pub(in crate::app) target: CatalogTarget,
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub(in crate::app) enum CatalogTarget {
    Page,
    PlaylistPicker,
    Sidebar,
}
impl CatalogTarget {
    pub(in crate::app) const ALL: [Self; 3] = [Self::Page, Self::PlaylistPicker, Self::Sidebar];
}
pub(in crate::app) struct CatalogRequests {
    pub(in crate::app) generations: Arc<[AtomicU64; CatalogTarget::ALL.len()]>,
    pub(in crate::app) loading: [bool; CatalogTarget::ALL.len()],
    pub(in crate::app) jobs: async_channel::Sender<Job>,
    pub(in crate::app) pending: async_channel::Receiver<Job>,
}
impl CatalogRequests {
    pub(in crate::app) fn new() -> Self {
        let (jobs, pending) = async_channel::bounded(CatalogTarget::ALL.len());
        Self {
            generations: Arc::new(std::array::from_fn(|_| AtomicU64::new(0))),
            loading: [false; CatalogTarget::ALL.len()],
            jobs,
            pending,
        }
    }

    pub(in crate::app) fn request(
        &mut self,
        query: Query,
        account_revision: u64,
        session: Option<Arc<Session>>,
    ) -> Result<(), String> {
        let target = query.target();
        self.cancel(target);
        let id = self.generations[target as usize].load(Ordering::Relaxed);
        let mut other = Vec::with_capacity(CatalogTarget::ALL.len() - 1);
        while let Ok(job) = self.pending.try_recv() {
            if job.query.target() != target {
                other.push(job);
            }
        }
        // Keep one pending request per destination, including sidebar and picker reads.
        for job in other.into_iter().chain(std::iter::once(Job {
            id,
            account_revision,
            query,
            session,
        })) {
            self.jobs.try_send(job).map_err(|_| {
                "The music connection stopped. Restart the app to reconnect.".to_owned()
            })?;
        }
        self.loading[target as usize] = true;
        Ok(())
    }

    pub(in crate::app) fn cancel(&mut self, target: CatalogTarget) {
        self.generations[target as usize].fetch_add(1, Ordering::Relaxed);
        self.loading[target as usize] = false;
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

// Only inactive pages live here. Navigation moves them instead of cloning their tracks.
pub(in crate::app) struct CachedPage {
    pub(in crate::app) route: Route,
    pub(in crate::app) account_revision: u64,
    pub(in crate::app) page: Page,
    pub(in crate::app) fetched_at: Instant,
    pub(in crate::app) coverage: sync::PageCoverage,
    pub(in crate::app) sort: Option<Continuation>,
}

pub(in crate::app) struct NavigationState {
    pub(in crate::app) route: Route,
    pub(in crate::app) list_scroll: UniformListScrollHandle,
    pub(in crate::app) detail_list: DetailList,
    pub(in crate::app) shelf_scroll: HashMap<usize, ScrollHandle>,
}

impl Music {
    pub(in crate::app) fn request(&mut self, query: Query, cx: &mut Context<Self>) {
        let target = query.target();
        if target == CatalogTarget::Page {
            if let Query::Reload(token) = &query {
                self.sync.page_sort = Some(token.clone());
                self.sync.page_fetched_at = None;
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
        let result = self
            .catalog_requests
            .request(query, self.account_revision, session);
        if target == CatalogTarget::PlaylistPicker {
            if let Some(Overlay::PlaylistPicker { error, .. }) = &mut self.overlay {
                *error = result.err();
            }
        } else if target == CatalogTarget::Sidebar {
            self.sidebar_error = result.err();
        } else if target == CatalogTarget::Page {
            self.sync.page_error = result.err();
            if let Some(error) = &self.sync.page_error {
                self.error = Some(error.clone());
            }
        }
        cx.notify();
    }

    pub(in crate::app) fn navigate(&mut self, route: Route, cx: &mut Context<Self>) {
        // A guest's Liked Music is the local list of liked songs.
        let route = match route {
            Route::Browse(link)
                if self.account.is_none() && matches!(link.id.as_str(), "VLLM" | "LM") =>
            {
                Route::Liked
            }
            Route::Browse(link) if link.params.is_none() && link.chart_country.is_none() => {
                match link.id.as_str() {
                    "FEmusic_home" => Route::Home,
                    "FEmusic_explore" => Route::Explore,
                    _ => Route::Browse(link),
                }
            }
            route => route,
        };
        self.focus_after_navigation(cx);
        if !self.route.same_page(&route) {
            self.back.push(NavigationState {
                route: self.route.clone(),
                list_scroll: self.list_scroll.clone(),
                shelf_scroll: std::mem::take(&mut self.shelf_scroll),
                detail_list: std::mem::replace(&mut self.detail_list, DetailList::new()),
            });
            if self.back.len() > PAGE_CACHE_LIMIT {
                self.back.remove(0);
            }
        }
        self.open_route(route, false, cx);
    }

    fn open_route(&mut self, route: Route, restore_scroll: bool, cx: &mut Context<Self>) {
        self.artwork.cancel_pending();
        self.catalog_requests.cancel(CatalogTarget::Page);
        self.player_open = false;
        let page = std::mem::take(&mut self.page);
        if let Some(fetched_at) = self.sync.page_fetched_at.take() {
            self.page_cache
                .retain(|entry| !entry.route.same_page(&self.route));
            self.page_cache.push_back(CachedPage {
                route: self.route.clone(),
                account_revision: self.account_revision,
                page,
                fetched_at,
                coverage: self.sync.page_coverage,
                sort: self.sync.page_sort.take(),
            });
        }
        self.list_id = self.list_id.wrapping_add(1);
        self.list_scroll = UniformListScrollHandle::new();
        self.detail_list = DetailList::new();
        self.shelf_scroll.clear();
        self.route = route.clone();
        self.sync.page_reload = None;
        self.sync.page_sort = None;
        self.sync.page_changes.clear();
        self.sync.page_coverage = sync::PageCoverage::default();
        self.sync.page_error = None;
        self.error = None;
        self.notice = None;
        self.sort_menu = false;
        self.rows.clear();
        self.list_rows.clear();
        let query = match &route {
            Route::Search(search) => search.query.clone(),
            _ => String::new(),
        };
        self.search
            .update(cx, |input, cx| input.set_query(query, cx));

        let cached = self
            .page_cache
            .iter()
            .position(|entry| {
                entry.account_revision == self.account_revision && entry.route.same_page(&route)
            })
            .and_then(|index| self.page_cache.remove(index));
        if self.page_cache.len() > PAGE_CACHE_LIMIT {
            self.page_cache.pop_front();
        }
        if let Some(entry) = cached {
            self.page = entry.page;
            self.sync.page_fetched_at = Some(entry.fetched_at);
            self.sync.page_coverage = entry.coverage;
            self.sync.page_sort = entry.sort;
            self.rebuild_rows();
            self.overlay_ratings();
            if sync::needs_refresh(self.sync.page_fetched_at, Instant::now()) {
                if restore_scroll {
                    self.revalidate_page(cx);
                } else {
                    // Forward visits start at the top, so replace expired loaded tails too.
                    self.request(self.page_query(), cx);
                }
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
                self.request(Query::Search(search), cx);
            }
            Route::Browse(link) => {
                self.page.title = link.title.clone();
                self.request(Query::Browse(link), cx);
            }
            Route::Liked | Route::Saved | Route::Recent | Route::Library(_)
                if self.account.is_some() =>
            {
                let section = route.library_section().expect("library route");
                self.page.title = section.title().into();
                self.request(Query::Library(section), cx);
            }
            _ => {
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
        } else {
            self.detail_list = DetailList::new();
        }
    }

    pub(in crate::app) fn append_rows(&mut self, first_shelf: usize) {
        let sectioned = self.page.header.is_some();
        let search_all =
            matches!(&self.route, Route::Search(search) if search.filter == SearchFilter::All);
        for (index, shelf) in self.page.shelves.iter().enumerate().skip(first_shelf) {
            if self.history_groups() {
                self.rows.extend(shelf.items.iter().cloned());
                continue;
            }
            let featured = search_all && shelf.layout == ShelfLayout::Featured;
            let carousel = sectioned
                && matches!(
                    shelf.layout,
                    ShelfLayout::Cards
                        | ShelfLayout::SmallCards
                        | ShelfLayout::Videos
                        | ShelfLayout::Episodes
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
            shelves: vec![Shelf {
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
            library,
        };
        self.rebuild_rows();
    }

    pub(in crate::app) fn go_back(&mut self, cx: &mut Context<Self>) {
        self.focus_after_navigation(cx);
        if self.player_open {
            self.player_open = false;
            cx.notify();
            return;
        }
        let Some(state) = self.back.pop() else { return };
        self.open_route(state.route, true, cx);
        self.list_scroll = state.list_scroll;
        self.detail_list = state.detail_list;
        self.shelf_scroll = state.shelf_scroll;
        if self.grouped_rows() {
            self.detail_list.reset_rows(self.list_rows.len());
        }
        cx.notify();
    }

    pub(in crate::app) fn catalog_result(&mut self, result: CatalogResult, cx: &mut Context<Self>) {
        if result.account_revision != self.account_revision
            || !self.catalog_requests.finish(result.target, result.id)
        {
            return;
        }
        if result.session_rejected {
            self.refresh_session(false, cx);
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
                Err(error) => self.sidebar_error = Some(error),
            }
            cx.notify();
            return;
        }
        if matches!(result.target, CatalogTarget::PlaylistPicker) {
            if let Some(Overlay::PlaylistPicker { page, error, .. }) = &mut self.overlay {
                match result.result {
                    Ok(choices) => {
                        if result.append
                            && let Some(current) = page
                        {
                            current.continuation = choices.continuation;
                            current.shelves.extend(choices.shelves);
                        } else {
                            *page = Some(Box::new(choices));
                        }
                    }
                    Err(message) => *error = Some(message),
                }
                cx.notify();
            }
            return;
        }
        match result.result {
            Ok(mut page) => {
                let mut invalidated = false;
                for change in self.sync.page_changes.drain(..) {
                    invalidated |=
                        change.apply(&mut page, &self.route, self.sync.page_sort.is_some());
                }
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
                if invalidated {
                    self.sync.page_fetched_at = None;
                } else if !result.append {
                    self.sync.page_fetched_at = Some(Instant::now());
                }
                self.sync.page_error = None;
                if result.revalidate {
                    let anchor = self.page_anchor();
                    if self.sync.page_reload.is_some() {
                        self.sync.page_coverage = sync::PageCoverage {
                            first_page_len: page.shelves.len(),
                            continued: false,
                        };
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
                    let previous_rows = self.list_rows.len();
                    self.page.continuation = page.continuation;
                    self.page.shelves.extend(page.shelves);
                    if self.history_groups() {
                        self.rebuild_rows();
                    } else {
                        self.append_rows(first_shelf);
                    }
                    if self.grouped_rows() && !self.history_groups() {
                        self.detail_list
                            .append_rows(self.list_rows.len() - previous_rows);
                    }
                } else {
                    self.sync.page_coverage = sync::PageCoverage {
                        first_page_len: page.shelves.len(),
                        continued: false,
                    };
                    self.page = page;
                    if matches!(self.overlay, Some(Overlay::Track { .. })) {
                        self.overlay = None;
                    }
                    self.rebuild_rows();
                }
                if self.sync.page_reload.is_some() {
                    self.rebuild_rows();
                    self.continue_page_reload(cx);
                }
            }
            Err(error) => {
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

impl CatalogRequests {
    pub(super) fn start(window: &mut Window, cx: &mut Context<Music>) -> Self {
        let catalog_requests = CatalogRequests::new();
        let (results, completed) = async_channel::unbounded::<CatalogResult>();
        for worker in 0..2 {
            let receiver = catalog_requests.pending.clone();
            let results = results.clone();
            let generations = catalog_requests.generations.clone();
            thread::Builder::new()
                .name(format!("catalog-{worker}"))
                .spawn(move || {
                    let catalog = Catalog::new();
                    while let Ok(job) = receiver.recv_blocking() {
                        let target = job.query.target();
                        if generations[target as usize].load(Ordering::Relaxed) != job.id {
                            continue;
                        }
                        let append = matches!(
                            job.query,
                            Query::More { .. }
                                | Query::PlaylistChoicesMore(_)
                                | Query::SidebarMore(_)
                        );
                        let revalidate = matches!(job.query, Query::Revalidate(_));
                        let query = match &job.query {
                            Query::Revalidate(query) => query.as_ref(),
                            query => query,
                        };
                        let result = match &catalog {
                            Err(e) => Err(anyhow::anyhow!("{e:#}")),
                            Ok(catalog) => {
                                let catalog = catalog.for_session(job.session);
                                match query {
                                    Query::Home => catalog.home(),
                                    Query::Explore => catalog.explore(),
                                    Query::Library(section) => catalog.library(*section),
                                    Query::PlaylistChoices | Query::SidebarPlaylists => {
                                        catalog.library(Section::Playlists)
                                    }
                                    Query::PlaylistChoicesMore(token)
                                    | Query::SidebarMore(token)
                                    | Query::Reload(token) => catalog.more(token),
                                    Query::Revalidate(_) => {
                                        unreachable!("revalidation unwrapped above")
                                    }
                                    Query::Search(search) => catalog.search_filtered(
                                        &search.query,
                                        search.filter,
                                        search.scope,
                                    ),
                                    Query::Browse(link) => catalog.browse(link),
                                    Query::More {
                                        token,
                                        search: Some(search),
                                    } => catalog.search_more(
                                        &search.query,
                                        search.filter,
                                        search.scope,
                                        token,
                                    ),
                                    Query::More {
                                        token,
                                        search: None,
                                    } => catalog.more(token),
                                }
                            }
                        };
                        let session_rejected =
                            result.as_ref().is_err_and(auth::session_was_rejected);
                        let result = result.map_err(|error| format!("{error:#}"));
                        if results
                            .send_blocking(CatalogResult {
                                id: job.id,
                                account_revision: job.account_revision,
                                append,
                                revalidate,
                                result,
                                session_rejected,
                                target,
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                })
                .expect("start catalog worker");
        }
        cx.spawn_in(window, async move |view, cx| {
            while let Ok(result) = completed.recv().await {
                if view
                    .update_in(cx, |view, _, cx| view.catalog_result(result, cx))
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

/// The art the web shows for Liked Music, which guests keep locally.
const LIKED_MUSIC_ART: &str =
    "https://www.gstatic.com/youtube/media/ytm/images/pbg/liked-songs-delhi-192.png";

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
    use super::{LOCAL_SECTIONS, Route, Section, local_library_filters, local_section};

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
}
