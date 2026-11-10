//! Local persistence, likes, and saved collections.

use super::*;

impl Music {
    pub(super) fn set_rating(&mut self, id: &str, rating: Option<Rating>) {
        if let Some(rating) = rating {
            self.ratings.insert(id.into(), rating);
        } else {
            self.ratings.remove(id);
        }
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
            if track.id == id {
                track.details.rating = rating;
            }
        }
    }

    pub(in crate::app) fn save(&mut self) {
        if let Some(store) = &self.store {
            store.save(&self.library);
        }
    }

    pub(in crate::app) fn save_preferences(&mut self) {
        if let Some(store) = &self.store {
            store.save_preferences(&self.library);
        }
    }

    pub(in crate::app) fn is_liked(&self, track: &Track) -> bool {
        if self.account.is_some() {
            self.ratings
                .get(&track.id)
                .copied()
                .or(track.details.rating)
                == Some(Rating::Like)
        } else {
            self.library.liked.iter().any(|t| t.id == track.id)
        }
    }

    pub(in crate::app) fn toggle_like(&mut self, track: &Track, cx: &mut Context<Self>) {
        if self.account.is_some() {
            let rating = if self.is_liked(track) {
                Rating::Indifferent
            } else {
                Rating::Like
            };
            self.mutate(
                Mutation::RateSong {
                    track: track.clone(),
                    rating,
                },
                cx,
            );
            return;
        }
        self.library.toggle_like(track);
        self.save();
        self.refresh_local();
        cx.notify();
    }

    pub(in crate::app) fn save_collection(
        &mut self,
        collection: Collection,
        cx: &mut Context<Self>,
    ) {
        if self.account.is_some() {
            let save = !self.is_saved(&collection);
            self.mutate(Mutation::CollectionLibrary { collection, save }, cx);
            return;
        }
        if let Some(i) = self
            .library
            .saved
            .iter()
            .position(|c| c.id == collection.id)
        {
            self.library.saved.remove(i);
            self.notice = Some("Collection removed from saved music".into());
        } else {
            self.library.saved.insert(0, collection);
            self.notice = Some("Collection saved".into());
        }
        self.save();
        self.refresh_local();
        cx.notify();
    }

    pub(in crate::app) fn is_saved(&self, collection: &Collection) -> bool {
        if self.account.is_none() {
            return self.library.saved.iter().any(|c| c.id == collection.id);
        }
        collection.details.saved.unwrap_or(matches!(
            self.route,
            Route::Saved
                | Route::Library(
                    Section::Albums
                        | Section::Playlists
                        | Section::Subscriptions
                        | Section::Podcasts
                )
        ))
    }
}
