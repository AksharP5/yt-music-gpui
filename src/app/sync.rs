//! Preserve loaded pages while account reads and confirmed edits arrive.
use super::*;

#[derive(Default)]
pub(in crate::app) struct SyncState {
    pub refreshed_at: Option<Instant>,
    pub page_fetched_at: Option<Instant>,
    pub sidebar_continuation: Option<Continuation>,
    pub rating_overrides: HashSet<String>,
    pub page_changes: Vec<PageChange>,
    pub optimistic_ratings: HashMap<String, Option<Rating>>,
}

pub(super) enum PageChange {
    Confirmed(Box<Mutation>),
    PlaylistAdded { id: String, tracks: Vec<Track> },
}

impl PageChange {
    pub(super) fn apply(self, page: &mut Page, route: &Route) {
        match self {
            Self::Confirmed(mutation) => patch_page(page, route, &mutation),
            Self::PlaylistAdded { id, tracks } => append_added(page, route, &id, &tracks),
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

/// A first page cannot prove that an unseen row was removed. Keep loaded order,
/// occurrences and the continuation already reached; refresh metadata in place.
pub(super) fn merge_page(loaded: &mut Page, fresh: Page) {
    loaded.title = fresh.title;
    loaded.header = fresh.header;
    loaded.description = fresh.description;
    loaded.privacy = fresh.privacy;
    loaded.editable = fresh.editable;
    loaded.chips = fresh.chips;
    loaded.countries = fresh.countries;
    if loaded.shelves.is_empty() {
        loaded.shelves = fresh.shelves;
        loaded.continuation = fresh.continuation;
        return;
    }
    let complete_list = fresh.continuation.is_none() && loaded.header.is_some();
    if complete_list {
        let keys: HashSet<_> = fresh
            .shelves
            .iter()
            .flat_map(|shelf| &shelf.items)
            .map(|item| {
                let (id, occurrence, kind) = item_key(item);
                (id.to_owned(), occurrence.map(str::to_owned), kind)
            })
            .collect();
        for shelf in &mut loaded.shelves {
            shelf.items.retain(|item| {
                let (id, occurrence, kind) = item_key(item);
                keys.contains(&(id.to_owned(), occurrence.map(str::to_owned), kind))
            });
        }
        loaded.continuation = None;
    }
    for shelf in fresh.shelves {
        // Continuations can split one playlist over several same-named shelves.
        let matching: Vec<_> = loaded
            .shelves
            .iter()
            .enumerate()
            .filter(|(_, old)| old.title == shelf.title && old.layout == shelf.layout)
            .map(|(index, _)| index)
            .collect();
        let Some(&first) = matching.first() else {
            loaded.shelves.push(shelf);
            continue;
        };
        for item in shelf.items {
            let existing = matching.iter().find_map(|&index| {
                loaded.shelves[index]
                    .items
                    .iter()
                    .position(|old| item_key(old) == item_key(&item))
                    .map(|row| (index, row))
            });
            if let Some((index, row)) = existing {
                loaded.shelves[index].items[row] = item;
            } else {
                loaded.shelves[first].items.push(item);
            }
        }
    }
}

pub(super) fn patch_page(page: &mut Page, route: &Route, mutation: &Mutation) {
    let playlist = |id: &str| matches!(route, Route::Browse(link) if link.id.trim_start_matches("VL") == id.trim_start_matches("VL"));
    let liked = matches!(route, Route::Liked | Route::Library(Section::Liked));
    let songs = matches!(route, Route::Saved | Route::Library(Section::Songs));
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
                Route::Library(
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
        let found = page.shelves.iter().enumerate().find_map(|(s, shelf)| shelf.items.iter().position(|item| matches!(item, Item::Track(track) if track.details.set_video_id.as_ref() == Some(set_video_id))).map(|i| (s,i)));
        if let Some((s, i)) = found {
            let item = page.shelves[s].items.remove(i);
            let target = before.as_ref().and_then(|before| page.shelves.iter().enumerate().find_map(|(s, shelf)| shelf.items.iter().position(|item| matches!(item, Item::Track(track) if track.details.set_video_id.as_ref() == Some(before))).map(|i| (s,i))));
            if let Some((s, i)) = target {
                page.shelves[s].items.insert(i, item);
            } else if before.is_none() && page.continuation.is_none() {
                if let Some(shelf) = page.shelves.last_mut() {
                    shelf.items.push(item);
                }
            } else {
                page.shelves[s].items.insert(i, item);
            }
        }
    }
}

fn append_added(page: &mut Page, route: &Route, id: &str, sources: &[Track]) {
    if !matches!(route, Route::Browse(link) if link.id.strip_prefix("VL").unwrap_or(&link.id) == id.strip_prefix("VL").unwrap_or(id))
        || page.continuation.is_some()
    {
        return;
    }
    if page.shelves.is_empty() {
        page.shelves.push(Shelf {
            title: String::new(),
            layout: ShelfLayout::Songs,
            more: None,
            items: Vec::new(),
        });
    }
    for track in sources {
        if !page.shelves.iter().flat_map(|shelf| &shelf.items).any(|item| matches!(item, Item::Track(current) if current.details.set_video_id == track.details.set_video_id))
        {
            page.shelves
                .last_mut()
                .unwrap()
                .items
                .push(Item::Track(track.clone()));
        }
    }
}

impl Music {
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
        cx: &mut Context<Self>,
    ) {
        let sources: Vec<_> = additions
            .iter()
            .filter_map(|(video, occurrence)| {
                let source = self
                    .library
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
                    )
                    .find(|track| &track.id == video)?;
                let mut track = source.clone();
                track.details.set_video_id = Some(occurrence.clone());
                Some(track)
            })
            .collect();
        if self.catalog_requests.is_loading(CatalogTarget::Page) {
            self.sync.page_changes.push(PageChange::PlaylistAdded {
                id: id.into(),
                tracks: sources.clone(),
            });
        }
        for (page, route) in std::iter::once((&mut self.page, &self.route)).chain(
            self.back
                .iter_mut()
                .filter_map(|state| Some((state.page.as_mut()?, &state.route))),
        ) {
            append_added(page, route, id, &sources);
        }
        self.rebuild_rows();
        cx.notify();
    }

    pub(super) fn revalidate_page(&mut self, cx: &mut Context<Self>) {
        let query = match &self.route {
            Route::Home => Query::Home,
            Route::Explore => Query::Explore,
            Route::Search(search) => Query::Search(search.clone()),
            Route::Browse(link) => Query::Browse(link.clone()),
            Route::Liked => Query::Library(Section::Liked),
            Route::Recent => Query::Library(Section::History),
            Route::Saved => Query::Library(Section::Songs),
            Route::Library(section) => Query::Library(*section),
        };
        self.request(Query::Revalidate(Box::new(query)), cx);
    }

    pub(super) fn patch_confirmed(&mut self, mutation: &Mutation, cx: &mut Context<Self>) {
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
        if let Mutation::DeletePlaylist { id } = mutation {
            self.sidebar_playlists.retain(|collection| {
                collection.id.trim_start_matches("VL") != id.trim_start_matches("VL")
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
        cx.notify();
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::{merge_page, needs_refresh, patch_page};
    use crate::{
        account::Mutation,
        app::Route,
        catalog::{BrowseLink, Collection, Item, Page, Rating, Shelf, ShelfLayout, Track},
    };
    use serde_json::json;
    use std::time::{Duration, Instant};

    pub(super) fn page(count: usize, continuation: bool) -> Page {
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
        assert!(!needs_refresh(Some(now - Duration::from_secs(179)), now));
        assert!(needs_refresh(Some(now - Duration::from_secs(180)), now));
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

    #[test]
    fn refreshing_five_hundred_rows_preserves_tail_occurrences_and_continuation() {
        let mut loaded = page(500, true);
        let mut fresh = page(50, true);
        if let Item::Track(track) = &mut fresh.shelves[0].items[0] {
            track.details.rating = Some(Rating::Like);
        }
        fresh.shelves[0].items.reverse();
        merge_page(&mut loaded, fresh);
        assert_eq!(loaded.tracks().len(), 500);
        assert_eq!(loaded.tracks()[0].details.rating, Some(Rating::Like));
        assert_eq!(
            loaded.tracks()[499].details.set_video_id.as_deref(),
            Some("occurrence-499")
        );
        assert!(loaded.continuation.is_some());
    }

    #[test]
    fn home_refresh_keeps_shelves_and_cards_under_the_cursor() {
        let mut loaded = page(5, false);
        loaded.shelves[0].layout = ShelfLayout::Cards;
        let mut fresh = loaded.clone();
        fresh.shelves[0].items.reverse();
        let before: Vec<_> = loaded.shelves[0]
            .items
            .iter()
            .map(|item| item.id().to_owned())
            .collect();
        merge_page(&mut loaded, fresh);
        assert_eq!(
            loaded.shelves[0]
                .items
                .iter()
                .map(|item| item.id().to_owned())
                .collect::<Vec<_>>(),
            before
        );
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

#[cfg(test)]
#[path = "sync_native.rs"]
mod native;
