//! Preserve loaded pages while account reads and confirmed edits arrive.
use super::*;

#[derive(Default)]
pub(in crate::app) struct SyncState {
    pub refreshed_at: Option<Instant>,
    pub page_fetched_at: Option<Instant>,
    pub sidebar_continuation: Option<Continuation>,
    pub sidebar_coverage: PageCoverage,
    pub rating_overrides: HashSet<String>,
    pub page_changes: Vec<PageChange>,
    pub optimistic_ratings: HashMap<String, Option<Rating>>,
    pub page_coverage: PageCoverage,
    pub page_error: Option<String>,
    pub page_reload: Option<PageReload>,
    pub playlist_refreshes: VecDeque<PlaylistRefresh>,
    pub playlist_failures: HashMap<String, PlaylistRefresh>,
    pub playlist_metadata_error: Option<(String, String)>,
    pub sidebar_changes: Vec<PlaylistMetadata>,
}

#[derive(Clone, Copy, Default)]
pub(in crate::app) struct PageCoverage {
    pub first_page_len: usize,
    pub continued: bool,
}

pub(super) enum PageChange {
    Confirmed(Box<Mutation>),
    PlaylistAdded { id: String, tracks: Vec<Track> },
    PlaylistMetadata(PlaylistMetadata),
    PlaylistEdits(PlaylistRefresh),
}

impl PageChange {
    pub(super) fn apply(
        self,
        page: &mut Page,
        route: &Route,
        first_page: bool,
        retained: &mut Option<PageCoverage>,
    ) -> bool {
        match self {
            Self::Confirmed(mutation) => patch_page(page, route, &mutation),
            Self::PlaylistAdded { id, tracks } => return append_added(page, route, &id, &tracks),
            Self::PlaylistMetadata(metadata) => return metadata.apply(page, route),
            Self::PlaylistEdits(refresh) => {
                return refresh.apply(page, route, first_page, retained);
            }
        }
        false
    }
}

#[derive(Clone)]
pub(in crate::app) struct PlaylistRefresh {
    pub id: String,
    pub additions: Vec<String>,
    pub removals: Vec<String>,
    // Replay owns its rows even after a metadata result retires the active record.
    pub rows: Option<Arc<PlaylistRows>>,
    pub moved: bool,
    pub account_generation: u64,
}

#[derive(Clone)]
pub(in crate::app) struct PlaylistRows {
    pub page: Page,
    coverage: PageCoverage,
}

impl PlaylistRefresh {
    fn merge(
        &mut self,
        additions: Vec<String>,
        removal: Option<String>,
        rows: Option<PlaylistRows>,
        moved: bool,
    ) {
        if let Some(rows) = rows
            && self.rows.as_ref().is_none_or(|previous| {
                crate::catalog::playlist_order_visible(&rows.page, &previous.page) == Some(true)
            })
        {
            self.rows = Some(Arc::new(rows));
        }
        self.moved |= moved;
        self.removals.retain(|id| !additions.contains(id));
        self.additions.extend(additions);
        self.additions.sort_unstable();
        self.additions.dedup();
        if let Some(removal) = removal {
            if let Some(order) = &mut self.rows {
                for shelf in &mut Arc::make_mut(order).page.shelves {
                    shelf.items.retain(|item| !matches!(item, Item::Track(track) if track.details.set_video_id.as_ref() == Some(&removal)));
                }
            }
            self.additions.retain(|id| id != &removal);
            if !self.removals.contains(&removal) {
                self.removals.push(removal);
            }
        }
    }

    pub(super) fn order_page(&self) -> Option<&Page> {
        self.rows
            .as_ref()
            .filter(|_| self.moved)
            .map(|rows| &rows.page)
    }

    fn apply(
        &self,
        page: &mut Page,
        route: &Route,
        first_page: bool,
        retained: &mut Option<PageCoverage>,
    ) -> bool {
        if !matches!(route, Route::Browse(link) if link.id.trim_start_matches("VL") == self.id) {
            return false;
        }
        let mut patched = false;
        if let Some(rows) = &self.rows {
            let additions: Vec<_> = rows
                .page
                .shelves
                .iter()
                .flat_map(|shelf| &shelf.items)
                .filter_map(|item| match item {
                    Item::Track(track)
                        if track
                            .details
                            .set_video_id
                            .as_ref()
                            .is_some_and(|id| self.additions.contains(id)) =>
                    {
                        Some(track.clone())
                    }
                    _ => None,
                })
                .collect();
            let missing_addition = additions.iter().any(|source| {
                !page.shelves.iter().flat_map(|shelf| &shelf.items).any(|item| matches!(item, Item::Track(track) if track.details.set_video_id == source.details.set_video_id))
            });
            let retain_order = self.moved
                && crate::catalog::playlist_order_visible(page, &rows.page) != Some(true);
            let retain_addition =
                page.continuation.is_some() && rows.page.continuation.is_none() && missing_addition;
            if first_page && (retain_order || retain_addition) {
                page.shelves = rows.page.shelves.clone();
                page.continuation = rows.page.continuation.clone();
                *retained = Some(rows.coverage);
                patched = true;
            }
            patched |= append_added(page, route, &self.id, &additions);
        }
        for shelf in &mut page.shelves {
            let count = shelf.items.len();
            shelf.items.retain(|item| !matches!(item, Item::Track(track) if track.details.set_video_id.as_ref().is_some_and(|id| self.removals.contains(id))));
            patched |= count != shelf.items.len();
        }
        patched
    }
}

#[derive(Clone)]
pub(super) struct PlaylistMetadata {
    id: String,
    stats: String,
    count: Option<String>,
    thumbnail: Option<String>,
    background: Option<String>,
}

impl PlaylistMetadata {
    fn from_page(id: String, page: &Page) -> Result<Self, String> {
        let header = page.header.as_ref().ok_or_else(|| {
            "Playlist saved, but its details are unavailable. Refresh the playlist.".to_owned()
        })?;
        let count = header.track_count().map(str::to_owned);
        Ok(Self {
            id,
            stats: header.stats.clone(),
            count,
            thumbnail: header.thumbnail.clone(),
            background: page.background.clone(),
        })
    }

    fn apply(&self, page: &mut Page, route: &Route) -> bool {
        if matches!(route, Route::Browse(link) if link.id.trim_start_matches("VL") == self.id) {
            if let Some(header) = &mut page.header {
                header.stats = self.stats.clone();
                header.thumbnail = self.thumbnail.clone();
            }
            page.background = self.background.clone();
            return true;
        }
        false
    }

    fn apply_sidebar(&self, playlists: &mut [Collection]) {
        for playlist in playlists {
            if playlist.id.trim_start_matches("VL") != self.id {
                continue;
            }
            if let Some(count) = &self.count {
                playlist.subtitle = playlist.subtitle.rsplit_once(" • ").map_or_else(
                    || count.clone(),
                    |(prefix, _)| format!("{prefix} • {count}"),
                );
            }
            playlist.thumbnail = self.thumbnail.clone();
        }
    }
}

pub(super) const FOCUS_REFRESH_AGE: Duration = Duration::from_secs(180);

pub(super) fn needs_refresh(last: Option<Instant>, now: Instant) -> bool {
    last.is_none_or(|last| now.saturating_duration_since(last) >= FOCUS_REFRESH_AGE)
}

fn item_key(item: &Item) -> (&str, Option<&str>, u8) {
    match item {
        Item::Track(track) => (&track.id, track.details.set_video_id.as_deref(), 0),
        Item::Collection(collection) => (&collection.id, None, 1),
        Item::Navigation(link) => (&link.id, None, 2),
    }
}

type ItemKey = (String, Option<String>, u8);

fn owned_item_key(item: &Item) -> ItemKey {
    let (id, occurrence, kind) = item_key(item);
    (id.to_owned(), occurrence.map(str::to_owned), kind)
}

enum RowAnchor {
    Item(ItemKey),
    Shelf(String, ShelfLayout),
    Suggestion(String),
    SuggestionsHeading,
    SuggestionsFooter,
    EmptyPlaylist,
}

pub(super) struct PageAnchor {
    detail: Option<(RowAnchor, ListOffset)>,
    uniform: Option<(ItemKey, Pixels, Pixels)>,
    shelves: Vec<(String, ShelfLayout, ScrollHandle)>,
}

pub(super) struct PageReload {
    anchor: PageAnchor,
    rows: usize,
}

/// Replace the authoritative first response and filter moved entries from loaded continuations.
fn merge_first_page<T>(
    loaded: &mut Vec<T>,
    mut fresh: Vec<T>,
    coverage: &mut PageCoverage,
    paginated: bool,
    filter_tail: impl FnOnce(&mut Vec<T>, &[T]),
) -> bool {
    let first_page_len = fresh.len();
    let preserve_tail = paginated && coverage.continued;
    if preserve_tail {
        let mut tail = loaded.split_off(coverage.first_page_len.min(loaded.len()));
        filter_tail(&mut tail, &fresh);
        fresh.extend(tail);
    } else {
        coverage.continued = false;
    }
    coverage.first_page_len = first_page_len;
    *loaded = fresh;
    preserve_tail
}

pub(super) fn merge_page(loaded: &mut Page, mut fresh: Page, coverage: &mut PageCoverage) {
    if merge_first_page(
        &mut loaded.shelves,
        std::mem::take(&mut fresh.shelves),
        coverage,
        fresh.continuation.is_some(),
        |tail, fresh| {
            let keys: HashSet<_> = fresh
                .iter()
                .flat_map(|shelf| &shelf.items)
                .map(item_key)
                .collect();
            // A replay moves a History song out of its old day bucket. Playlist
            // occurrence IDs still distinguish repeated songs.
            tail.retain_mut(|shelf| {
                shelf.items.retain(|item| !keys.contains(&item_key(item)));
                !shelf.items.is_empty()
            });
        },
    ) {
        fresh.continuation = loaded.continuation.take();
    }
    fresh.shelves = std::mem::take(&mut loaded.shelves);
    *loaded = fresh;
}

impl SyncState {
    pub(super) fn cancel_playlist_refreshes(&mut self) {
        self.playlist_refreshes.clear();
        self.playlist_failures.clear();
        self.playlist_metadata_error = None;
        self.sidebar_changes.clear();
    }

    pub(super) fn update_sidebar(
        &mut self,
        playlists: &mut Vec<Collection>,
        page: Page,
        append: bool,
        revalidate: bool,
    ) {
        let mut seen = HashSet::new();
        let choices = page
            .shelves
            .into_iter()
            .flat_map(|shelf| shelf.items)
            .filter_map(|item| match item {
                Item::Collection(collection)
                    if !matches!(collection.id.as_str(), "VLLM" | "VLSE")
                        && seen.insert(collection.id.clone()) =>
                {
                    Some(collection)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        if append {
            let mut loaded: HashSet<_> = playlists.iter().map(|item| item.id.clone()).collect();
            playlists.extend(
                choices
                    .into_iter()
                    .filter(|item| loaded.insert(item.id.clone())),
            );
            self.sidebar_coverage.continued = true;
            self.sidebar_continuation = page.continuation;
            for change in self.sidebar_changes.drain(..) {
                change.apply_sidebar(playlists);
            }
            return;
        }
        if !revalidate {
            self.sidebar_coverage = PageCoverage::default();
        }
        let preserve_tail = merge_first_page(
            playlists,
            choices,
            &mut self.sidebar_coverage,
            page.continuation.is_some(),
            |tail, _| tail.retain(|collection| !seen.contains(&collection.id)),
        );
        if !preserve_tail {
            self.sidebar_continuation = page.continuation;
        }
        for change in self.sidebar_changes.drain(..) {
            change.apply_sidebar(playlists);
        }
    }
}

pub(super) fn patch_page(page: &mut Page, route: &Route, mutation: &Mutation) {
    let playlist = |id: &str| matches!(route, Route::Browse(link) if link.id.trim_start_matches("VL") == id.trim_start_matches("VL"));
    let liked = matches!(route, Route::Liked | Route::Library(Section::Liked));
    let songs = matches!(route, Route::Library(Section::Songs));
    for shelf in &mut page.shelves {
        shelf.items.retain(|item| match (item, mutation) {
            (Item::Track(current), Mutation::RateSong { track, rating }) => {
                !(liked && current.id == track.id && *rating != Rating::Like)
            }
            (Item::Track(current), Mutation::SongLibrary { track, save }) => {
                !(songs && current.id == track.id && !save)
            }
            (
                Item::Track(current),
                Mutation::RemoveFromPlaylist {
                    id, set_video_id, ..
                },
            ) => !(playlist(id) && current.details.set_video_id.as_ref() == Some(set_video_id)),
            (Item::Track(current), Mutation::RemoveHistory { token }) => {
                current.details.remove_history.as_ref() != Some(token)
            }
            (
                Item::Collection(current),
                Mutation::CollectionLibrary {
                    collection,
                    save: false,
                },
            ) if matches!(
                route,
                Route::Saved
                    | Route::Library(
                        Section::Albums
                            | Section::Playlists
                            | Section::Subscriptions
                            | Section::Podcasts
                    )
            ) =>
            {
                current.id != collection.id
            }
            (Item::Collection(current), Mutation::DeletePlaylist { id }) => {
                current.id.trim_start_matches("VL") != id.trim_start_matches("VL")
            }
            _ => true,
        });
        for item in &mut shelf.items {
            match item {
                Item::Track(current) => match mutation {
                    Mutation::RateSong { track, rating } if current.id == track.id => {
                        current.details.rating = Some(*rating)
                    }
                    Mutation::SongLibrary { track, save } if current.id == track.id => {
                        current.details.in_library = Some(*save);
                        current.details.add_library = track.details.add_library.clone();
                        current.details.remove_library = track.details.remove_library.clone();
                    }
                    _ => {}
                },
                Item::Collection(current) => match mutation {
                    Mutation::CollectionLibrary { collection, save }
                        if current.id == collection.id =>
                    {
                        current.details.saved = Some(*save)
                    }
                    Mutation::EditPlaylist { id, title, .. }
                        if current.id.trim_start_matches("VL") == id.trim_start_matches("VL") =>
                    {
                        current.title = title.trim().into()
                    }
                    _ => {}
                },
                _ => {}
            }
        }
    }
    let added_song = match mutation {
        Mutation::RateSong {
            track,
            rating: Rating::Like,
        } if liked => Some(track),
        Mutation::SongLibrary { track, save: true } if songs => Some(track),
        _ => None,
    };
    if let Some(track) = added_song
        && !page
            .shelves
            .iter()
            .flat_map(|shelf| &shelf.items)
            .any(|item| matches!(item, Item::Track(current) if current.id == track.id))
    {
        if page.shelves.is_empty() {
            page.shelves.push(Shelf {
                title: String::new(),
                layout: ShelfLayout::Songs,
                more: None,
                items: Vec::new(),
            });
        }
        let mut track = track.clone();
        match mutation {
            Mutation::RateSong { rating, .. } => track.details.rating = Some(*rating),
            Mutation::SongLibrary { save, .. } => track.details.in_library = Some(*save),
            _ => {}
        }
        page.shelves[0].items.insert(0, Item::Track(track));
    }
    if let Mutation::CollectionLibrary { collection, save } = mutation
        && matches!(route, Route::Browse(link) if link.id == collection.id)
        && let Some(header) = &mut page.header
    {
        header.collection.saved = Some(*save);
        if collection.id.starts_with("UC") {
            header.subscribed = Some(*save);
            header.subscription_label = if *save { "Subscribed" } else { "Subscribe" }.into();
        }
    }
    if let Mutation::EditPlaylist {
        id,
        title,
        description,
        privacy,
    } = mutation
        && playlist(id)
    {
        page.title = title.trim().into();
        page.description = description.clone();
        page.privacy = Some(privacy.value().into());
    }
    if let Mutation::MoveInPlaylist {
        id,
        set_video_id,
        before,
    } = mutation
        && playlist(id)
    {
        let position = |id: &str| {
            page.shelves.iter().flat_map(|shelf| &shelf.items).position(|item| matches!(item, Item::Track(track) if track.details.set_video_id.as_deref() == Some(id)))
        };
        let Some(from) = position(set_video_id) else {
            return;
        };
        let target = match before {
            Some(before) => position(before),
            None if page.continuation.is_none() => {
                Some(page.shelves.iter().map(|shelf| shelf.items.len()).sum())
            }
            None => None,
        };
        let Some(to) = target else { return };
        let sizes: Vec<_> = page.shelves.iter().map(|shelf| shelf.items.len()).collect();
        let mut items: Vec<_> = page
            .shelves
            .iter_mut()
            .flat_map(|shelf| shelf.items.drain(..))
            .collect();
        let moved = items.remove(from);
        items.insert(to - usize::from(to > from), moved);
        let mut items = items.into_iter();
        // Shelves are page boundaries. A cross-page move displaces a row into the next page.
        for (shelf, count) in page.shelves.iter_mut().zip(sizes) {
            shelf.items.extend(items.by_ref().take(count));
        }
    }
}

fn append_added(page: &mut Page, route: &Route, id: &str, sources: &[Track]) -> bool {
    if !matches!(route, Route::Browse(link) if link.id.strip_prefix("VL").unwrap_or(&link.id) == id.strip_prefix("VL").unwrap_or(id))
        || page.continuation.is_some()
        || sources.is_empty()
    {
        return false;
    }
    if page.shelves.is_empty() {
        page.shelves.push(Shelf {
            title: String::new(),
            layout: ShelfLayout::Songs,
            more: None,
            items: Vec::new(),
        });
    }
    let mut added = false;
    for track in sources {
        if !page.shelves.iter().flat_map(|shelf| &shelf.items).any(|item| matches!(item, Item::Track(current) if current.details.set_video_id == track.details.set_video_id))
            && let Some(shelf) = page.shelves.last_mut()
        {
            shelf.items.push(Item::Track(track.clone()));
            added = true;
        }
    }
    added
}

impl Music {
    fn playlist_pages<'a>(
        &'a self,
        id: &'a str,
    ) -> impl Iterator<Item = (&'a Page, PageCoverage)> + 'a {
        let id = id.trim_start_matches("VL");
        std::iter::once((&self.page, &self.route, self.sync.page_coverage))
            .chain(
                self.back
                    .iter()
                    .rev()
                    .filter_map(|state| Some((state.page.as_ref()?, &state.route, state.coverage))),
            )
            .filter_map(move |(page, route, coverage)| {
                matches!(route, Route::Browse(link) if link.id.trim_start_matches("VL") == id)
                    .then_some((page, coverage))
            })
    }

    pub(super) fn replay_playlist_edits(
        &self,
        page: &mut Page,
        first_page: bool,
        retained: &mut Option<PageCoverage>,
    ) -> bool {
        let mut patched = false;
        for refresh in self
            .sync
            .playlist_refreshes
            .iter()
            .chain(self.sync.playlist_failures.values())
        {
            patched |= refresh.apply(page, &self.route, first_page, retained);
        }
        patched
    }

    pub(super) fn update_playlist_rows_snapshot(&mut self) {
        let Route::Browse(link) = &self.route else {
            return;
        };
        let coverage = self.sync.page_coverage;
        for refresh in self
            .sync
            .playlist_refreshes
            .iter_mut()
            .chain(self.sync.playlist_failures.values_mut())
        {
            if refresh.id == link.id.trim_start_matches("VL")
                && let Some(order) = &mut refresh.rows
                && crate::catalog::playlist_order_visible(&self.page, &order.page) == Some(true)
            {
                *order = Arc::new(PlaylistRows {
                    page: self.page.clone(),
                    coverage,
                });
            }
        }
    }

    pub(super) fn page_anchor(&self) -> PageAnchor {
        let home = self.route.is_home() || matches!(self.route, Route::Explore);
        let detailed = home || self.grouped_rows();
        let top = self.detail_list.state.logical_scroll_top();
        let detail = if home {
            top.item_ix
                .checked_sub(1)
                .and_then(|index| self.page.shelves.get(index))
                .map(|shelf| RowAnchor::Shelf(shelf.title.clone(), shelf.layout))
        } else if detailed {
            top.item_ix
                .checked_sub(usize::from(self.detail_list.inline_header))
                .and_then(|index| self.list_rows.get(index))
                .map(|row| match *row {
                    ListRow::Item(index) => RowAnchor::Item(owned_item_key(&self.rows[index])),
                    ListRow::EmptyPlaylist => RowAnchor::EmptyPlaylist,
                    ListRow::SuggestionsHeading => RowAnchor::SuggestionsHeading,
                    ListRow::SuggestionsFooter => RowAnchor::SuggestionsFooter,
                    ListRow::Suggestion(index) => RowAnchor::Suggestion(
                        self.suggestions
                            .as_ref()
                            .map(|suggestions| suggestions.tracks[index].track.id.clone())
                            .unwrap_or_default(),
                    ),
                    ListRow::Related(index) => self.suggestions.as_ref().map_or(
                        RowAnchor::SuggestionsHeading,
                        |suggestions| {
                            let shelf = &suggestions.related[index];
                            RowAnchor::Shelf(shelf.title.clone(), shelf.layout)
                        },
                    ),
                    ListRow::Featured(index)
                    | ListRow::Shelf(index)
                    | ListRow::Heading(index)
                    | ListRow::More(index) => {
                        let shelf = &self.page.shelves[index];
                        RowAnchor::Shelf(shelf.title.clone(), shelf.layout)
                    }
                })
        } else {
            None
        }
        .map(|anchor| (anchor, top));
        let scroll = self.list_scroll.0.borrow();
        let uniform = scroll.last_item_size.and_then(|size| {
            // Collection grids use one scroll item per grid row, not per item.
            if detailed
                || self
                    .rows
                    .iter()
                    .all(|item| matches!(item, Item::Collection(_)))
                || self.rows.is_empty()
                || size.contents.height <= px(0.)
            {
                return None;
            }
            let height = size.contents.height / self.rows.len() as f32;
            let offset = -scroll.base_handle.offset().y;
            let index = (f32::from(offset) / f32::from(height)).floor().max(0.) as usize;
            self.rows
                .get(index)
                .map(|item| (owned_item_key(item), offset - height * index as f32, height))
        });
        let shelves = self
            .shelf_scroll
            .iter()
            .filter_map(|(index, scroll)| {
                let shelf = self.page.shelves.get(*index)?;
                Some((shelf.title.clone(), shelf.layout, scroll.clone()))
            })
            .collect();
        PageAnchor {
            detail,
            uniform,
            shelves,
        }
    }

    pub(super) fn restore_page_anchor(&mut self, anchor: &PageAnchor) -> bool {
        self.shelf_scroll = anchor
            .shelves
            .iter()
            .filter_map(|(title, layout, scroll)| {
                self.page
                    .shelves
                    .iter()
                    .position(|shelf| shelf.title == *title && shelf.layout == *layout)
                    .map(|index| (index, scroll.clone()))
            })
            .collect();
        let mut found = true;
        if let Some((key, offset_in_item, height)) = &anchor.uniform {
            found = false;
            if let Some(index) = self
                .rows
                .iter()
                .position(|item| item_key(item) == (key.0.as_str(), key.1.as_deref(), key.2))
            {
                let scroll = self.list_scroll.0.borrow();
                let mut offset = scroll.base_handle.offset();
                offset.y = -(*height * index as f32 + *offset_in_item);
                scroll.base_handle.set_offset(offset);
                found = true;
            }
        }
        let Some((anchor, old_top)) = &anchor.detail else {
            return found;
        };
        let mut top = *old_top;
        let home = self.route.is_home() || matches!(self.route, Route::Explore);
        let index = if home {
            self.detail_list.state.reset(self.page.shelves.len() + 1);
            self.page.shelves.iter().position(|shelf| matches!(&anchor, RowAnchor::Shelf(title, layout) if shelf.title == *title && shelf.layout == *layout)).map(|index| index + 1)
        } else {
            self.list_rows
                .iter()
                .position(|row| match (anchor, row) {
                    (RowAnchor::EmptyPlaylist, ListRow::EmptyPlaylist)
                    | (RowAnchor::SuggestionsHeading, ListRow::SuggestionsHeading)
                    | (RowAnchor::SuggestionsFooter, ListRow::SuggestionsFooter) => true,
                    (RowAnchor::Suggestion(id), ListRow::Suggestion(index)) => self
                        .suggestions
                        .as_ref()
                        .is_some_and(|suggestions| suggestions.tracks[*index].track.id == *id),
                    (RowAnchor::Shelf(title, layout), ListRow::Related(index)) => {
                        self.suggestions.as_ref().is_some_and(|suggestions| {
                            let shelf = &suggestions.related[*index];
                            shelf.title == *title && shelf.layout == *layout
                        })
                    }
                    (RowAnchor::Item(key), ListRow::Item(index)) => {
                        item_key(&self.rows[*index]) == (key.0.as_str(), key.1.as_deref(), key.2)
                    }
                    (
                        RowAnchor::Shelf(title, layout),
                        ListRow::Featured(index)
                        | ListRow::Shelf(index)
                        | ListRow::Heading(index)
                        | ListRow::More(index),
                    ) => {
                        let shelf = &self.page.shelves[*index];
                        shelf.title == *title && shelf.layout == *layout
                    }
                    _ => false,
                })
                .map(|index| index + usize::from(self.detail_list.inline_header))
        };
        if let Some(index) = index {
            top.item_ix = index;
        }
        top.item_ix = top
            .item_ix
            .min(self.detail_list.state.item_count().saturating_sub(1));
        self.detail_list.state.scroll_to(top);
        index.is_some()
    }

    pub(in crate::app) fn reload_current_page(&mut self, cx: &mut Context<Self>) {
        if self.account.is_none() && self.route.library_section().is_some() {
            let anchor = self.page_anchor();
            self.refresh_local();
            self.restore_page_anchor(&anchor);
            cx.notify();
            return;
        }
        if let Some(active) = &mut self.forward_page {
            active.fetched_at = None;
        }
        if self.account.is_some() {
            self.refresh_remote(cx);
        } else {
            self.revalidate_page(cx);
        }
        self.sync.page_reload = Some(PageReload {
            anchor: self.page_anchor(),
            rows: self.rows.len(),
        });
    }

    pub(super) fn continue_page_reload(&mut self, cx: &mut Context<Self>) {
        let Some(reload) = self.sync.page_reload.take() else {
            return;
        };
        let found = self.restore_page_anchor(&reload.anchor);
        // Reload enough continuations to find the old visible occurrence. If it
        // was removed, stop at the previous depth rather than reading the whole list.
        if !found
            && self.rows.len() < reload.rows
            && let Some(token) = self.page.continuation.clone()
        {
            self.request(
                Query::More {
                    token,
                    search: match &self.route {
                        Route::Search(search) => Some(search.clone()),
                        _ => None,
                    },
                },
                cx,
            );
            self.sync.page_reload = Some(reload);
        }
    }

    pub(super) fn overlay_ratings(&mut self) {
        for track in self.library.queue.tracks.iter_mut().chain(
            self.page
                .shelves
                .iter_mut()
                .flat_map(|shelf| &mut shelf.items)
                .chain(&mut self.rows)
                .filter_map(|item| match item {
                    Item::Track(track) => Some(track),
                    _ => None,
                }),
        ) {
            if self.sync.rating_overrides.contains(&track.id) {
                track.details.rating = self.ratings.get(&track.id).copied();
            }
        }
    }

    pub(super) fn patch_added(
        &mut self,
        id: &str,
        additions: &[(String, String)],
        preferred: &[Track],
        cx: &mut Context<Self>,
    ) {
        let sources = self.playlist_added_sources(additions, preferred);
        self.append_playlist_tracks(id, &sources);
        if sources.len() < additions.len()
            && matches!(&self.route, Route::Browse(link) if link.id.trim_start_matches("VL") == id.trim_start_matches("VL"))
        {
            self.revalidate_page(cx);
        }
        cx.notify();
    }

    fn playlist_added_sources(
        &self,
        additions: &[(String, String)],
        preferred: &[Track],
    ) -> Vec<Track> {
        additions
            .iter()
            .filter_map(|(video, occurrence)| {
                let source = preferred
                    .iter()
                    .chain(
                        self.library
                            .queue
                            .tracks
                            .iter()
                            .chain(self.rows.iter().filter_map(|item| match item {
                                Item::Track(track) => Some(track),
                                _ => None,
                            }))
                            .chain(
                                self.back
                                    .iter()
                                    .filter_map(|state| state.page.as_ref())
                                    .flat_map(|page| &page.shelves)
                                    .flat_map(|shelf| &shelf.items)
                                    .filter_map(|item| match item {
                                        Item::Track(track) => Some(track),
                                        _ => None,
                                    }),
                            ),
                    )
                    .find(|track| &track.id == video)?;
                let mut track = source.clone();
                track.details.set_video_id = Some(occurrence.clone());
                Some(track)
            })
            .collect()
    }

    fn append_playlist_tracks(&mut self, id: &str, sources: &[Track]) {
        if sources.is_empty() {
            return;
        }
        if self.catalog_requests.is_loading(CatalogTarget::Page) {
            self.sync.page_changes.push(PageChange::PlaylistAdded {
                id: id.into(),
                tracks: sources.to_vec(),
            });
        }
        let anchor = self.page_anchor();
        let changed = append_added(&mut self.page, &self.route, id, sources);
        for state in &mut self.back {
            if let Some(page) = &mut state.page {
                append_added(page, &state.route, id, sources);
            }
        }
        for refresh in self
            .sync
            .playlist_refreshes
            .iter_mut()
            .chain(self.sync.playlist_failures.values_mut())
        {
            if refresh.id == id.trim_start_matches("VL")
                && let Some(rows) = &mut refresh.rows
            {
                let route = Route::Browse(BrowseLink::new(format!("VL{}", refresh.id), "Playlist"));
                append_added(&mut Arc::make_mut(rows).page, &route, id, sources);
            }
        }
        if changed {
            self.rebuild_rows();
            self.restore_page_anchor(&anchor);
            self.overlay_ratings();
        }
    }

    fn playlist_rows_snapshot(&self, id: &str, moved: Option<&str>) -> Option<PlaylistRows> {
        self.playlist_pages(id)
            .filter(|(page, _)| page.header.is_some())
            .filter(|(page, _)| moved.is_none_or(|id| {
                page.shelves.iter().flat_map(|shelf| &shelf.items).any(|item| {
                    matches!(item, Item::Track(track) if track.details.set_video_id.as_deref() == Some(id))
                })
            }))
            .max_by_key(|(page, _)| page.shelves.iter().map(|shelf| shelf.items.len()).sum::<usize>())
            .map(|(page, coverage)| PlaylistRows { page: page.clone(), coverage })
    }

    pub(super) fn refresh_playlist_metadata(
        &mut self,
        id: &str,
        additions: Vec<String>,
        removal: Option<String>,
        moved: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        let id = id.trim_start_matches("VL");
        let retaining_rows = !additions.is_empty()
            || moved.is_some()
            || self
                .sync
                .playlist_refreshes
                .iter()
                .chain(self.sync.playlist_failures.values())
                .any(|refresh| refresh.id == id && refresh.rows.is_some());
        let rows = retaining_rows
            .then(|| self.playlist_rows_snapshot(id, moved))
            .flatten();
        if let Some((index, refresh)) = self
            .sync
            .playlist_refreshes
            .iter_mut()
            .enumerate()
            .find(|(_, refresh)| refresh.id == id)
        {
            refresh.account_generation = self.account_generation;
            refresh.merge(additions, removal, rows, moved.is_some());
            if index > 0 {
                return;
            }
        } else {
            let mut refresh =
                self.sync
                    .playlist_failures
                    .remove(id)
                    .unwrap_or_else(|| PlaylistRefresh {
                        id: id.to_owned(),
                        additions: Vec::new(),
                        removals: Vec::new(),
                        rows: None,
                        moved: false,
                        account_generation: self.account_generation,
                    });
            refresh.account_generation = self.account_generation;
            refresh.merge(additions, removal, rows, moved.is_some());
            self.sync.playlist_refreshes.push_back(refresh);
            if self.sync.playlist_refreshes.len() > 1 {
                return;
            }
        }
        if !self
            .catalog_requests
            .is_loading(CatalogTarget::PlaylistMetadata)
        {
            self.request_playlist_metadata(cx);
        }
    }

    fn request_playlist_metadata(&mut self, cx: &mut Context<Self>) {
        if let Some(refresh) = self.sync.playlist_refreshes.front() {
            self.request(Query::PlaylistMetadata(refresh.clone()), cx);
        }
    }

    pub(super) fn playlist_metadata_result(
        &mut self,
        result: Result<Page, String>,
        account_generation: u64,
        cx: &mut Context<Self>,
    ) {
        if self
            .sync
            .playlist_refreshes
            .front()
            .is_some_and(|refresh| refresh.account_generation > account_generation)
        {
            self.request_playlist_metadata(cx);
            return;
        }
        let Some(mut refresh) = self.sync.playlist_refreshes.pop_front() else {
            return;
        };
        match result.and_then(|page| {
            let metadata = PlaylistMetadata::from_page(refresh.id.clone(), &page)?;
            let additions: Vec<_> = page
                .shelves
                .iter()
                .flat_map(|shelf| &shelf.items)
                .filter_map(|item| match item {
                    Item::Track(track)
                        if track
                            .details
                            .set_video_id
                            .as_ref()
                            .is_some_and(|id| refresh.additions.contains(id)) =>
                    {
                        Some(track.clone())
                    }
                    _ => None,
                })
                .collect();
            Ok((metadata, additions))
        }) {
            Ok((metadata, returned)) => {
                let additions: Vec<_> = returned
                    .iter()
                    .filter_map(|track| {
                        Some((track.id.clone(), track.details.set_video_id.clone()?))
                    })
                    .collect();
                let mut sources = self.playlist_added_sources(&additions, &[]);
                for track in returned {
                    if !sources
                        .iter()
                        .any(|source| source.details.set_video_id == track.details.set_video_id)
                    {
                        sources.push(track);
                    }
                }
                self.append_playlist_tracks(&refresh.id, &sources);
                if let Some(order) = &mut refresh.rows {
                    let route =
                        Route::Browse(BrowseLink::new(format!("VL{}", refresh.id), "Playlist"));
                    append_added(
                        &mut Arc::make_mut(order).page,
                        &route,
                        &refresh.id,
                        &sources,
                    );
                }
                self.clear_playlist_failure(&refresh.id);
                self.publish_playlist_metadata(metadata, &refresh);
            }
            Err(error) => {
                if self.error.is_none() {
                    self.sync.playlist_metadata_error = Some((refresh.id.clone(), error.clone()));
                    self.error = Some(error);
                }
                self.sync
                    .playlist_failures
                    .insert(refresh.id.clone(), refresh);
            }
        }
        self.request_playlist_metadata(cx);
        cx.notify();
    }

    fn publish_playlist_metadata(&mut self, metadata: PlaylistMetadata, refresh: &PlaylistRefresh) {
        if self.catalog_requests.is_loading(CatalogTarget::Page) {
            self.sync
                .page_changes
                .push(PageChange::PlaylistEdits(refresh.clone()));
            self.sync
                .page_changes
                .push(PageChange::PlaylistMetadata(metadata.clone()));
        }
        for (page, route) in std::iter::once((&mut self.page, &self.route)).chain(
            self.back
                .iter_mut()
                .filter_map(|state| Some((state.page.as_mut()?, &state.route))),
        ) {
            metadata.apply(page, route);
        }
        self.page_cache.retain(|entry| !matches!(&entry.route, Route::Browse(link) if link.id.trim_start_matches("VL") == metadata.id));
        if matches!(&self.route, Route::Browse(link) if link.id.trim_start_matches("VL") == metadata.id)
            && let Some(active) = &mut self.forward_page
        {
            active.fetched_at = None;
        }
        metadata.apply_sidebar(&mut self.sidebar_playlists);
        if self.catalog_requests.is_loading(CatalogTarget::Sidebar) {
            self.sync
                .sidebar_changes
                .retain(|change| change.id != metadata.id);
            self.sync.sidebar_changes.push(metadata);
        }
    }

    pub(super) fn playlist_reloaded(
        &mut self,
        page: &Page,
        account_generation: u64,
        cx: &mut Context<Self>,
    ) -> bool {
        let Route::Browse(link) = &self.route else {
            return true;
        };
        let id = link.id.trim_start_matches("VL");
        let Some((refresh, pending)) = self
            .sync
            .playlist_refreshes
            .iter()
            .find(|refresh| refresh.id == id)
            .map(|refresh| (refresh, true))
            .or_else(|| {
                self.sync
                    .playlist_failures
                    .get(id)
                    .map(|refresh| (refresh, false))
            })
        else {
            return true;
        };
        // Check the unpatched response. Replayed local removals cannot prove
        // that the service has caught up with the acknowledged edit.
        let order = refresh.order_page().map_or(Some(true), |order| {
            crate::catalog::playlist_order_visible(page, order)
        });
        let strict = !(page.continuation.is_some() && !refresh.removals.is_empty())
            && order == Some(true)
            && crate::catalog::playlist_occurrences_visible(
                page,
                &refresh.additions,
                &refresh.removals,
            );
        let reflected = if pending {
            strict
        } else {
            crate::catalog::playlist_metadata_visible(page, &refresh.additions, &refresh.removals)
        };
        if account_generation < refresh.account_generation || !reflected {
            return false;
        }
        let mut refresh = refresh.clone();
        if let Ok(metadata) = PlaylistMetadata::from_page(refresh.id.clone(), page) {
            self.cancel_playlist_refresh(&metadata.id, cx);
            // Failed readback recovers to the server order. Known additions still
            // need their rows through this same reply, before the record is retired.
            if !pending {
                refresh.moved = false;
            }
            self.sync
                .page_changes
                .push(PageChange::PlaylistEdits(refresh.clone()));
            self.publish_playlist_metadata(metadata, &refresh);
            return strict;
        }
        false
    }

    fn cancel_playlist_refresh(&mut self, id: &str, cx: &mut Context<Self>) {
        let id = id.trim_start_matches("VL");
        let active = self
            .sync
            .playlist_refreshes
            .front()
            .is_some_and(|refresh| refresh.id == id);
        self.sync
            .playlist_refreshes
            .retain(|refresh| refresh.id != id);
        self.sync.sidebar_changes.retain(|change| change.id != id);
        self.sync.page_changes.retain(|change| {
            !matches!(change, PageChange::PlaylistMetadata(metadata) if metadata.id == id)
                && !matches!(change, PageChange::PlaylistEdits(refresh) if refresh.id == id)
        });
        self.clear_playlist_failure(id);
        if active {
            self.catalog_requests
                .cancel(CatalogTarget::PlaylistMetadata);
            self.request_playlist_metadata(cx);
        }
    }

    fn clear_playlist_failure(&mut self, id: &str) {
        self.sync.playlist_failures.remove(id);
        if self
            .sync
            .playlist_metadata_error
            .as_ref()
            .is_some_and(|(playlist, _)| playlist == id)
            && let Some((_, error)) = self.sync.playlist_metadata_error.take()
            && self.error.as_ref() == Some(&error)
            && self.sync.page_error.as_ref() != Some(&error)
        {
            self.error = None;
        }
    }

    #[expect(clippy::expect_used, reason = "every library route has a section")]
    pub(super) fn page_query(&self) -> Query {
        match &self.route {
            Route::Home => Query::Home,
            Route::Explore => Query::Explore,
            Route::Search(search) => Query::Search(search.clone()),
            Route::Browse(link) => Query::Browse(link.clone()),
            Route::Liked | Route::Recent | Route::Saved | Route::Library(_) => {
                Query::Library(self.route.library_section().expect("library route"))
            }
        }
    }

    pub(super) fn revalidate_page(&mut self, cx: &mut Context<Self>) {
        self.request(Query::Revalidate(Box::new(self.page_query())), cx);
    }

    pub(super) fn patch_confirmed(&mut self, mutation: &Mutation, cx: &mut Context<Self>) {
        let anchor = self.page_anchor();
        if self.catalog_requests.is_loading(CatalogTarget::Page) {
            self.sync
                .page_changes
                .push(PageChange::Confirmed(Box::new(mutation.clone())));
        }
        patch_page(&mut self.page, &self.route, mutation);
        for state in &mut self.back {
            if let Some(page) = &mut state.page {
                patch_page(page, &state.route, mutation);
            }
        }
        for refresh in self
            .sync
            .playlist_refreshes
            .iter_mut()
            .chain(self.sync.playlist_failures.values_mut())
        {
            if let Some(order) = &mut refresh.rows {
                let route = Route::Browse(BrowseLink::new(format!("VL{}", refresh.id), "Playlist"));
                patch_page(&mut Arc::make_mut(order).page, &route, mutation);
            }
        }
        if let Mutation::DeletePlaylist { id } = mutation {
            self.cancel_playlist_refresh(id, cx);
            let mut index = 0;
            let first_page_len = &mut self.sync.sidebar_coverage.first_page_len;
            let boundary = *first_page_len;
            self.sidebar_playlists.retain(|collection| {
                let keep = collection.id.trim_start_matches("VL") != id.trim_start_matches("VL");
                if !keep && index < boundary {
                    *first_page_len -= 1;
                }
                index += 1;
                keep
            });
        }
        if let Mutation::EditPlaylist { id, title, .. } = mutation {
            for collection in &mut self.sidebar_playlists {
                if collection.id.trim_start_matches("VL") == id.trim_start_matches("VL") {
                    collection.title = title.trim().into();
                }
            }
        }
        self.rebuild_rows();
        self.restore_page_anchor(&anchor);
        cx.notify();
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::{PageCoverage, PlaylistMetadata, SyncState, merge_page, needs_refresh, patch_page};
    use crate::{
        account::Mutation,
        app::Route,
        catalog::{BrowseLink, Collection, Item, Page, Shelf, ShelfLayout, Track},
    };
    use serde_json::json;
    use std::time::{Duration, Instant};

    #[test]
    fn playlist_guide_keeps_its_owner_and_uses_tracks_instead_of_views() {
        for (stats, expected) in [
            ("3 views • 2 tracks • 5 minutes", "2 tracks"),
            ("3 views • 0 tracks", "0 tracks"),
            ("2 songs • 5 minutes", "2 songs"),
        ] {
            let page = crate::catalog::parse_page(
                &json!({"header":{"musicResponsiveHeaderRenderer":{
                "title":{"simpleText":"Playlist"}, "secondSubtitle":{"simpleText":stats},
                "facepile":{"avatarStackViewModel":{"text":{"content":"Different header owner"}}}
            }},"contents":[]}),
                "Playlist",
            )
            .unwrap();
            let metadata = PlaylistMetadata::from_page("playlist".into(), &page).unwrap();
            let mut playlists = vec![Collection {
                id: "VLplaylist".into(),
                title: "Playlist".into(),
                subtitle: "Original service owner • 3 tracks".into(),
                thumbnail: None,
                details: Default::default(),
            }];
            metadata.apply_sidebar(&mut playlists);
            assert_eq!(
                playlists[0].subtitle,
                format!("Original service owner • {expected}")
            );
        }
    }

    pub(in crate::app) fn page(count: usize, continuation: bool) -> Page {
        let mut page = crate::catalog::parse_page(&json!({"contents":[],"nextContinuationData":if continuation { json!({"continuation":"next"}) } else { serde_json::Value::Null }}), "Playlist").unwrap();
        page.shelves.push(Shelf { title: "Songs".into(), layout: ShelfLayout::Songs, more: None, items:(0..count).map(|index| {
            let mut track: Track = serde_json::from_value(json!({"id":format!("{index:011}"),"title":format!("Song {}",index+1),"artist":"Synthetic artist"})).unwrap();
            track.details.set_video_id = Some(format!("occurrence-{index}")); Item::Track(track)
        }).collect() });
        page
    }

    #[test]
    fn focus_refresh_waits_three_minutes() {
        let now = Instant::now();
        assert!(!needs_refresh(
            Some(now.checked_sub(Duration::from_secs(179)).unwrap()),
            now
        ));
        assert!(needs_refresh(
            Some(now.checked_sub(Duration::from_secs(180)).unwrap()),
            now
        ));
        assert!(needs_refresh(None, now));
    }

    #[test]
    fn confirmed_collection_save_updates_header_without_changing_subscriptions() {
        let mut page = crate::catalog::parse_page(
            &json!({"header":{"musicResponsiveHeaderRenderer":{
                "title":{"simpleText":"Album"},
                "buttons":[{"toggleButtonRenderer":{
                    "defaultIcon":{"iconType":"BOOKMARK_BORDER"},"isToggled":false
                }}]
            }},"contents":[]}),
            "Album",
        )
        .unwrap();
        let collection = Collection {
            id: "MPREalbum".into(),
            title: "Album".into(),
            subtitle: String::new(),
            thumbnail: None,
            details: Default::default(),
        };
        let route = Route::Browse(BrowseLink::new(&collection.id, &collection.title));
        for save in [true, false] {
            patch_page(
                &mut page,
                &route,
                &Mutation::CollectionLibrary {
                    collection: collection.clone(),
                    save,
                },
            );
            let header = page.header.as_ref().unwrap();
            assert_eq!(header.collection.saved, Some(save));
            assert_eq!(header.subscribed, None);
        }
    }

    pub(in crate::app) fn response(
        shelves: &[(&str, Vec<usize>)],
        token: Option<&str>,
        occurrences: bool,
    ) -> Page {
        let contents: Vec<_> = shelves.iter().map(|(title, rows)| json!({
            "musicShelfRenderer": {
                "title": {"runs": [{"text": title}]},
                "contents": rows.iter().map(|index| json!({
                    "musicResponsiveListItemRenderer": {
                        "playlistItemData": {
                            "videoId": format!("{index:011}"),
                            "setVideoId": occurrences.then(|| format!("occurrence-{index}"))
                        },
                        "flexColumns": [
                            {"musicResponsiveListItemFlexColumnRenderer": {"text": {"runs": [{"text": format!("Song {}", index+1)}]}}},
                            {"musicResponsiveListItemFlexColumnRenderer": {"text": {"runs": [{"text": "Synthetic artist"}]}}}
                        ]
                    }
                })).collect::<Vec<_>>()
            }
        })).collect();
        crate::catalog::parse_page(&json!({
            "continuationContents": {"sectionListContinuation": {
                "contents": contents,
                "continuations": token.map(|token| vec![json!({"nextContinuationData": {"continuation": token}})]).unwrap_or_default()
            }}
        }), "Synthetic library").unwrap()
    }

    fn sidebar_page(ids: &[&str], token: Option<&str>) -> Page {
        let mut page = response(&[("Playlists", vec![0])], token, false);
        page.shelves[0].items = ids
            .iter()
            .map(|id| {
                Item::Collection(
                    serde_json::from_value(
                        json!({"id":id,"title":id,"subtitle":"Synthetic playlist"}),
                    )
                    .unwrap(),
                )
            })
            .collect();
        page
    }

    #[test]
    fn sidebar_refresh_replaces_first_page_order_and_deduplicates_loaded_continuations() {
        let mut sync = SyncState::default();
        let mut playlists = Vec::new();
        sync.update_sidebar(
            &mut playlists,
            sidebar_page(&["VLa", "VLb", "VLc"], Some("first-tail")),
            false,
            false,
        );
        sync.update_sidebar(
            &mut playlists,
            sidebar_page(&["VLc", "VLd", "VLe"], Some("loaded-tail")),
            true,
            false,
        );
        assert_eq!(
            playlists
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            ["VLa", "VLb", "VLc", "VLd", "VLe"]
        );
        sync.update_sidebar(
            &mut playlists,
            sidebar_page(&["VLd", "VLb", "VLd", "VLLM", "VLSE"], Some("fresh-tail")),
            false,
            true,
        );
        assert_eq!(
            playlists
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            ["VLd", "VLb", "VLe"]
        );
        assert_eq!(
            sync.sidebar_continuation.as_ref().unwrap().token(),
            "loaded-tail"
        );
        sync.update_sidebar(
            &mut playlists,
            sidebar_page(&["VLb"], Some("fresh-tail-2")),
            false,
            true,
        );
        assert_eq!(
            playlists
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            ["VLb", "VLe"]
        );
    }

    #[test]
    fn complete_sidebar_refresh_adopts_server_order_and_removals() {
        let mut sync = SyncState::default();
        let mut playlists = Vec::new();
        sync.update_sidebar(
            &mut playlists,
            sidebar_page(&["VLa", "VLb", "VLc"], Some("tail")),
            false,
            false,
        );
        sync.update_sidebar(
            &mut playlists,
            sidebar_page(&["VLc", "VLb"], None),
            false,
            true,
        );
        assert_eq!(
            playlists
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            ["VLc", "VLb"]
        );
        assert!(sync.sidebar_continuation.is_none());
        sync.update_sidebar(&mut playlists, sidebar_page(&[], None), false, true);
        assert!(playlists.is_empty());
    }

    #[test]
    fn sidebar_refresh_without_loaded_continuations_replaces_the_whole_old_first_page() {
        let mut sync = SyncState::default();
        let mut playlists = Vec::new();
        sync.update_sidebar(
            &mut playlists,
            sidebar_page(&["VLa", "VLb"], Some("stale-tail")),
            false,
            false,
        );
        sync.update_sidebar(
            &mut playlists,
            sidebar_page(&["VLb"], Some("new-tail")),
            false,
            true,
        );
        assert_eq!(
            playlists
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            ["VLb"]
        );
        assert_eq!(
            sync.sidebar_continuation.as_ref().unwrap().token(),
            "new-tail"
        );
    }

    fn ids(page: &Page) -> Vec<String> {
        page.shelves
            .iter()
            .flat_map(|shelf| &shelf.items)
            .map(|item| item.id().to_owned())
            .collect()
    }

    #[test]
    fn first_page_refresh_adopts_order_removals_and_new_likes_preserving_450_deeper_rows() {
        let mut loaded = response(&[("Songs", (0..50).collect())], Some("page-2"), true);
        for start in (50..500).step_by(50) {
            loaded.shelves.extend(
                response(
                    &[("Songs", (start..start + 50).collect())],
                    Some("page-11"),
                    true,
                )
                .shelves,
            );
        }
        loaded.continuation = response(&[], Some("page-11"), true).continuation;
        let mut coverage = PageCoverage {
            first_page_len: 1,
            continued: true,
        };
        // The old last row is removed, a new like arrives first, and all other
        // first-page songs are reversed. None of the loaded tail is covered.
        let mut fresh_rows = vec![900];
        fresh_rows.extend((0..49).rev());
        let fresh = response(&[("Songs", fresh_rows)], Some("fresh-page-2"), true);
        let expected_prefix = ids(&fresh);
        let expected_tail = ids(&response(&[("Songs", (50..500).collect())], None, true));
        merge_page(&mut loaded, fresh, &mut coverage);
        let actual = ids(&loaded);
        assert_eq!(&actual[..50], expected_prefix);
        assert_eq!(&actual[50..], expected_tail);
        assert!(!actual.contains(&format!("{:011}", 49)));
        assert_eq!(loaded.continuation.as_ref().unwrap().token(), "page-11");

        // A shorter first response still replaces the entire previous span.
        merge_page(
            &mut loaded,
            response(&[("Songs", vec![900, 48])], Some("next"), true),
            &mut coverage,
        );
        assert_eq!(loaded.tracks().len(), 452);
        assert_eq!(&ids(&loaded)[2..], expected_tail);
    }

    #[test]
    fn refreshed_occurrences_are_removed_from_deeper_pages_without_removing_repeated_songs() {
        let mut loaded = response(
            &[("Songs", vec![0, 1]), ("Songs", vec![2, 3, 4])],
            Some("tail"),
            true,
        );
        let Item::Track(repeated) = &mut loaded.shelves[1].items[2] else {
            panic!("track fixture")
        };
        repeated.id = format!("{:011}", 3);
        let mut coverage = PageCoverage {
            first_page_len: 1,
            continued: true,
        };
        merge_page(
            &mut loaded,
            response(&[("Songs", vec![3, 1])], Some("next"), true),
            &mut coverage,
        );
        assert_eq!(ids(&loaded), [3, 1, 2, 3].map(|id| format!("{id:011}")));
        assert_eq!(
            loaded.tracks()[3].details.set_video_id.as_deref(),
            Some("occurrence-4")
        );
    }

    #[test]
    fn history_replay_moves_into_today_and_drops_its_old_day_bucket_occurrence() {
        let mut loaded = response(
            &[
                ("Today", vec![0, 1]),
                ("Yesterday", vec![2, 3]),
                ("Last week", vec![4, 5]),
            ],
            Some("older"),
            false,
        );
        let mut coverage = PageCoverage {
            first_page_len: 1,
            continued: true,
        };
        merge_page(
            &mut loaded,
            response(
                &[("Today", vec![2, 0, 1]), ("Yesterday", vec![3])],
                Some("next"),
                false,
            ),
            &mut coverage,
        );
        assert_eq!(
            loaded
                .shelves
                .iter()
                .map(|shelf| shelf.title.as_str())
                .collect::<Vec<_>>(),
            ["Today", "Yesterday", "Last week"]
        );
        assert_eq!(
            ids(&loaded),
            [2, 0, 1, 3, 4, 5].map(|id| format!("{id:011}"))
        );
        assert_eq!(loaded.continuation.as_ref().unwrap().token(), "older");
        assert_eq!(coverage.first_page_len, 2);
    }

    #[test]
    fn complete_responses_are_authoritative_without_a_collection_header() {
        let mut loaded = response(
            &[("Today", vec![0, 1]), ("Yesterday", vec![2, 3])],
            Some("next"),
            false,
        );
        let mut coverage = PageCoverage {
            first_page_len: 1,
            continued: true,
        };
        merge_page(
            &mut loaded,
            response(&[("Today", vec![3, 0])], None, false),
            &mut coverage,
        );
        assert_eq!(ids(&loaded), [3, 0].map(|id| format!("{id:011}")));
        assert!(loaded.continuation.is_none());
        assert!(!coverage.continued);
        merge_page(&mut loaded, response(&[], None, false), &mut coverage);
        assert!(loaded.shelves.is_empty());
    }

    #[test]
    fn a_first_page_without_loaded_continuations_adopts_the_new_continuation() {
        let mut loaded = response(&[("Songs", vec![0, 1])], Some("stale"), true);
        let mut coverage = PageCoverage {
            first_page_len: 1,
            continued: false,
        };
        merge_page(
            &mut loaded,
            response(&[("Songs", vec![1, 2])], Some("fresh"), true),
            &mut coverage,
        );
        assert_eq!(ids(&loaded), [1, 2].map(|id| format!("{id:011}")));
        assert_eq!(loaded.continuation.as_ref().unwrap().token(), "fresh");
    }

    #[test]
    fn confirmed_removal_and_move_patch_only_the_requested_occurrence() {
        let mut loaded = page(500, false);
        let route = Route::Browse(BrowseLink::new("VLplaylist", "Playlist"));
        patch_page(
            &mut loaded,
            &route,
            &Mutation::RemoveFromPlaylist {
                id: "playlist".into(),
                video_id: "00000000000".into(),
                set_video_id: "occurrence-400".into(),
            },
        );
        assert_eq!(loaded.tracks().len(), 499);
        assert!(
            !loaded
                .tracks()
                .iter()
                .any(|track| track.details.set_video_id.as_deref() == Some("occurrence-400"))
        );
        patch_page(
            &mut loaded,
            &route,
            &Mutation::MoveInPlaylist {
                id: "playlist".into(),
                set_video_id: "occurrence-499".into(),
                before: Some("occurrence-1".into()),
            },
        );
        assert_eq!(
            loaded.tracks()[1].details.set_video_id.as_deref(),
            Some("occurrence-499")
        );
        assert_eq!(
            loaded.tracks()[2].details.set_video_id.as_deref(),
            Some("occurrence-1")
        );
    }
}
