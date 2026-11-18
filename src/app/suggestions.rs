//! Playlist recommendations have their own read and display state, never queue rows.

use super::*;
use crate::catalog::{PlaylistSuggestion, PlaylistSuggestions};

pub(super) struct Suggestions {
    playlist: BrowseLink,
    visit: usize,
    pub tracks: Vec<PlaylistSuggestion>,
    reload: Option<Continuation>,
    pub related: Vec<Shelf>,
    pub(super) related_scroll: HashMap<usize, ScrollHandle>,
    pub error: Option<String>,
    pub(super) focus: VirtualListFocus,
    pub(super) refresh_focus: FocusHandle,
    auto_reload_when_empty: bool,
    added_during_read: HashSet<String>,
}

pub(super) struct SuggestionsResult {
    pub job: Job,
    pub result: Result<PlaylistSuggestions, String>,
    pub session_rejected: bool,
}

impl Music {
    pub(super) fn playlist_rows_end(&self) -> usize {
        self.list_rows
            .iter()
            .rposition(|row| !row.is_suggestion())
            .map_or(0, |index| index + 1)
    }

    pub(super) fn append_suggestion_rows(&mut self) {
        if let Some(suggestions) = &self.suggestions {
            if self.rows.is_empty() {
                self.list_rows.push(ListRow::EmptyPlaylist);
            }
            self.list_rows.push(ListRow::SuggestionsHeading);
            self.list_rows
                .extend((0..suggestions.tracks.len()).map(ListRow::Suggestion));
            self.list_rows.push(ListRow::SuggestionsFooter);
            self.list_rows
                .extend((0..suggestions.related.len()).map(ListRow::Related));
        }
    }

    fn refresh_suggestion_rows(&mut self) {
        let start = self.playlist_rows_end();
        self.list_rows.truncate(start);
        self.append_suggestion_rows();
        self.detail_list
            .replace_tail(start, self.list_rows.len() - start);
    }

    pub(super) fn clear_suggestions(&mut self) {
        self.catalog_requests.cancel(CatalogTarget::Suggestions);
        if self.suggestions.take().is_some() {
            self.shelf_heights.truncate(self.page.shelves.len());
            self.refresh_suggestion_rows();
        }
    }

    pub(super) fn load_suggestions(&mut self, fresh_seed: bool, cx: &mut Context<Self>) {
        if self.account.is_none() || !self.page.editable {
            self.clear_suggestions();
            return;
        }
        if self.suggestions.is_some() {
            return;
        }
        let Route::Browse(playlist) = &self.route else {
            return;
        };
        if !playlist.id.starts_with("VL") {
            return;
        }
        let token = self
            .page
            .header
            .as_ref()
            .and_then(|header| header.suggestions.clone());
        if token.is_none() {
            return;
        }
        self.suggestions = Some(Suggestions {
            playlist: playlist.clone(),
            visit: self.list_id,
            tracks: Vec::new(),
            reload: None,
            related: Vec::new(),
            related_scroll: HashMap::new(),
            error: None,
            focus: VirtualListFocus::default(),
            refresh_focus: cx.focus_handle().tab_index(0).tab_stop(true),
            auto_reload_when_empty: false,
            added_during_read: HashSet::new(),
        });
        self.request_suggestions(if fresh_seed { None } else { token }, cx);
    }

    fn request_suggestions(&mut self, token: Option<Continuation>, cx: &mut Context<Self>) {
        let Some(suggestions) = &mut self.suggestions else {
            return;
        };
        suggestions.error = None;
        suggestions.added_during_read.clear();
        let playlist = suggestions.playlist.clone();
        self.request(Query::PlaylistSuggestions { playlist, token }, cx);
        self.refresh_suggestion_rows();
    }

    pub(super) fn refresh_suggestions(&mut self, cx: &mut Context<Self>) {
        if self.catalog_requests.is_loading(CatalogTarget::Suggestions) {
            return;
        }
        let Some(suggestions) = &self.suggestions else {
            return;
        };
        let token = suggestions.reload.clone();
        self.request_suggestions(token, cx);
    }

    pub(super) fn suggestions_result(
        &mut self,
        response: SuggestionsResult,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let Query::PlaylistSuggestions { playlist, .. } = &response.job.query else {
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
            || !self.suggestions.as_ref().is_some_and(|suggestions| {
                suggestions.visit == self.list_id && suggestions.playlist.id == playlist.id
            })
            || !self
                .catalog_requests
                .finish(CatalogTarget::Suggestions, response.job.id)
        {
            return;
        }
        if response.session_rejected {
            self.refresh_session(false, cx);
            return;
        }
        if let Some(suggestions) = &mut self.suggestions {
            match response.result {
                Ok(mut batch) => {
                    batch.tracks.retain(|suggestion| {
                        !suggestions.added_during_read.contains(&suggestion.track.id)
                    });
                    suggestions.tracks = batch.tracks;
                    suggestions.reload = batch.reload;
                    suggestions.auto_reload_when_empty = batch.auto_reload_when_empty;
                    if let Some(related) = batch.related {
                        suggestions.related = related;
                        suggestions.related_scroll.clear();
                        self.shelf_heights.truncate(self.page.shelves.len());
                    }
                    suggestions.error = None;
                }
                Err(error) => suggestions.error = Some(error),
            }
            suggestions.added_during_read.clear();
        }
        self.refresh_suggestion_rows();
        if let Some(suggestions) = &self.suggestions
            && suggestions.refresh_focus.is_focused(window)
        {
            self.reveal_suggestion_control(None);
        }
        cx.notify();
    }

    pub(super) fn add_suggestion(&mut self, track: Track, cx: &mut Context<Self>) {
        let Some(suggestions) = &self.suggestions else {
            return;
        };
        let mutation = Mutation::AddToPlaylist {
            id: suggestions.playlist.id.trim_start_matches("VL").to_owned(),
            video_ids: vec![track.id.clone()],
            duplicates: false,
        };
        let key = mutation.key();
        if self.mutate(mutation, cx)
            && let Some(sources) = self.pending_mutations.get_mut(&key)
        {
            sources.push(track);
        }
    }

    pub(super) fn acknowledge_suggestions(
        &mut self,
        id: &str,
        additions: &[(String, String)],
        cx: &mut Context<Self>,
    ) {
        let loading = self.catalog_requests.is_loading(CatalogTarget::Suggestions);
        let Some(suggestions) = &mut self.suggestions else {
            return;
        };
        if suggestions.playlist.id.trim_start_matches("VL") != id.trim_start_matches("VL") {
            return;
        }
        for (video, _) in additions {
            suggestions
                .tracks
                .retain(|suggestion| suggestion.track.id != *video);
            if loading {
                suggestions.added_during_read.insert(video.clone());
            }
        }
        let reload = suggestions.tracks.is_empty() && suggestions.auto_reload_when_empty;
        self.refresh_suggestion_rows();
        if reload && !loading {
            self.refresh_suggestions(cx);
        }
    }

    pub(super) fn suggestion_row(
        &mut self,
        index: usize,
        inset: f32,
        cx: &Context<Self>,
    ) -> AnyElement {
        let enabled = self.suggestion_control_enabled(index * 2 + 1);
        let Some(suggestions) = &mut self.suggestions else {
            return div().into_any_element();
        };
        let Some(suggestion) = suggestions.tracks.get(index) else {
            return div().into_any_element();
        };
        let track = suggestion.track.clone();
        let play_focus = suggestions.focus.handle(index * 2, cx);
        let add_focus = suggestions
            .focus
            .handle(index * 2 + 1, cx)
            .tab_stop(enabled);
        let subtitle = track
            .details
            .byline
            .clone()
            .unwrap_or_else(|| match &track.album {
                Some(album) => format!("{} • {album}", track.artist),
                None => track.artist.clone(),
            });
        let play = track.clone();
        let play_row = track.clone();
        let add = track.clone();
        let visit = self.list_id;
        let thumbnail = self.row_thumbnail(
            track.thumbnail.as_deref(),
            48.,
            ArtworkShape::Square,
            track.details.wide_thumbnail,
            "suggestion-row",
        );
        div()
            .id(("suggestion-row", index))
            .group("suggestion-row")
            .w_full()
            .min_w_0()
            .h(px(68.))
            .px(px(inset + 16.))
            .flex()
            .items_center()
            .gap(px(16.))
            .child(
                div()
                    .id(("suggestion-play", index))
                    .keyboard_control(self.theme)
                    .track_focus(&play_focus)
                    .cursor_pointer()
                    .flex_shrink_0()
                    .child(thumbnail)
                    .on_click(cx.listener(move |view, _, _, cx| {
                        cx.stop_propagation();
                        if view.list_id == visit {
                            view.play_track(play.clone(), cx);
                        }
                    })),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .child(
                        div()
                            .truncate()
                            .text_size(px(14.))
                            .font_weight(FontWeight::MEDIUM)
                            .child(track.title),
                    )
                    .child(self.byline(
                        track.details.explicit,
                        self.linked_text(subtitle, &track.details.links, cx),
                        14.,
                    )),
            )
            .child(
                div()
                    .w(px(44.))
                    .flex_shrink_0()
                    .text_size(px(14.))
                    .text_color(self.theme.muted)
                    .child(
                        track
                            .duration
                            .map(|seconds| clock(seconds as f64))
                            .unwrap_or_default(),
                    ),
            )
            .child(
                button(("suggestion-add", index), "menu-save-playlist", self.theme)
                    .track_focus(&add_focus)
                    .opacity(if enabled { 1. } else { 0.35 })
                    .on_click(cx.listener(move |view, _, _, cx| {
                        cx.stop_propagation();
                        if enabled && view.list_id == visit {
                            view.add_suggestion(add.clone(), cx);
                        }
                    })),
            )
            .cursor_pointer()
            .on_click(cx.listener(move |view, _, _, cx| {
                if view.list_id == visit {
                    view.play_track(play_row.clone(), cx);
                }
            }))
            .into_any_element()
    }

    pub(super) fn suggestions_footer(&mut self, inset: f32, cx: &Context<Self>) -> AnyElement {
        let loading = self.catalog_requests.is_loading(CatalogTarget::Suggestions);
        let Some(suggestions) = &self.suggestions else {
            return div().into_any_element();
        };
        let focus = suggestions.refresh_focus.clone().tab_stop(!loading);
        let error = suggestions.error.clone();
        div()
            .w_full()
            .px(px(inset + 16.))
            .pt(px(16.))
            .pb(px(32.))
            .flex()
            .flex_col()
            .gap(px(16.))
            .children(error.map(|error| {
                div()
                    .text_size(px(14.))
                    .text_color(self.theme.muted)
                    .child(error)
            }))
            .child(
                div().w_full().flex().justify_center().child(
                    div()
                        .id("suggestions-refresh")
                        .keyboard_control(self.theme)
                        .track_focus(&focus)
                        .px(px(20.))
                        .h(px(36.))
                        .rounded_full()
                        .border_1()
                        .border_color(self.theme.border)
                        .flex()
                        .items_center()
                        .cursor_pointer()
                        .text_size(px(14.))
                        .child(if loading { "Loading…" } else { "Refresh" })
                        .on_click(cx.listener(|view, _, _, cx| view.refresh_suggestions(cx))),
                ),
            )
            .into_any_element()
    }

    pub(super) fn move_suggestions_focus(
        &mut self,
        reverse: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(suggestions) = &self.suggestions else {
            return false;
        };
        let refresh = suggestions.tracks.len() * 2;
        let control = if suggestions.refresh_focus.is_focused(window) {
            refresh
        } else if let Some(control) = suggestions.focus.focused(window) {
            control
        } else {
            return false;
        };
        let next = if reverse {
            (0..control)
                .rev()
                .find(|index| self.suggestion_control_enabled(*index))
        } else {
            (control + 1..=refresh).find(|index| self.suggestion_control_enabled(*index))
        };
        let Some(control) = next else { return false };
        if let Some(suggestions) = &mut self.suggestions {
            if control == refresh {
                window.focus(&suggestions.refresh_focus, cx);
            } else {
                suggestions.focus.focus(control, window, cx);
            }
        }
        self.reveal_suggestion_control((control != refresh).then_some(control / 2));
        cx.notify();
        true
    }

    fn reveal_suggestion_control(&self, track: Option<usize>) {
        let row = self.list_rows.iter().rposition(|row| match row {
            ListRow::Suggestion(index) => track == Some(*index),
            ListRow::SuggestionsFooter => track.is_none(),
            _ => false,
        });
        if let Some(row) = row {
            let row = row + usize::from(self.detail_list.inline_header);
            if self.detail_list.state.bounds_for_item(row).is_some() {
                self.detail_list.state.scroll_to_reveal_item(row);
            } else {
                self.detail_list.state.scroll_to(ListOffset {
                    item_ix: row,
                    offset_in_item: px(0.),
                });
            }
        }
    }

    fn suggestion_control_enabled(&self, control: usize) -> bool {
        let Some(suggestions) = &self.suggestions else {
            return false;
        };
        if control == suggestions.tracks.len() * 2 {
            return !self.catalog_requests.is_loading(CatalogTarget::Suggestions);
        }
        let Some(suggestion) = suggestions.tracks.get(control / 2) else {
            return false;
        };
        control.is_multiple_of(2)
            || suggestion.addable
                && self.account_operation.is_none()
                && !self
                    .pending_mutations
                    .contains_key(suggestions.playlist.id.trim_start_matches("VL"))
    }
}
