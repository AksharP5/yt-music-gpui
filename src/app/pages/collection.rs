//! Shared album/playlist headers, metadata, and collection actions.

use super::super::*;

impl Music {
    pub(in crate::app) fn page_heading(
        &mut self,
        stacked: bool,
        edge: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let header = self.page.header.clone();
        let title = self.page.title.clone();
        let has_tracks = self.rows.iter().any(|item| matches!(item, Item::Track(_)));
        let mut cover = None;
        let mut creator = None;
        let mut heading = div()
            .flex()
            .flex_col()
            .gap(px(16.))
            .when(stacked, |d| d.items_center().text_center());
        if let Some(header) = header {
            creator = header.artist.map(|artist| match artist {
                HeaderArtist::Link(link) => div()
                    .id("collection-artist")
                    .text_size(px(14.))
                    .cursor_pointer()
                    .child(link.title.clone())
                    .on_click(cx.listener(move |view, _, _, cx| {
                        view.navigate(Route::Browse(link.clone()), cx)
                    }))
                    .into_any_element(),
                HeaderArtist::Text(text) => div().text_size(px(14.)).child(text).into_any_element(),
            });
            if let Some(url) = header.thumbnail {
                cover = Some(self.art(Some(&url), edge, ArtworkShape::Square));
            }
            heading = heading.child(
                div()
                    .w_full()
                    .text_size(px(if stacked { 24. } else { 32. }))
                    .font_weight(FontWeight::BOLD)
                    .child(title),
            );
            if !header.subtitle.is_empty() || !header.stats.is_empty() {
                heading = heading.child(
                    div()
                        .flex()
                        .flex_col()
                        .when(stacked, |d| d.items_center())
                        .text_size(px(14.))
                        .line_height(px(20.))
                        .text_color(self.theme.muted)
                        .when(!header.subtitle.is_empty(), |d| d.child(header.subtitle))
                        .when(!header.stats.is_empty(), |d| d.child(header.stats)),
                );
            }
        } else {
            heading = heading.child(
                div()
                    .w_full()
                    .text_size(px(32.))
                    .font_weight(FontWeight::BOLD)
                    .child(title),
            );
        }
        if !self.page.description.is_empty() {
            heading = heading.child(
                div()
                    .id("page-description")
                    .w_full()
                    .text_size(px(14.))
                    .text_color(self.theme.muted)
                    .line_height(px(20.))
                    .line_clamp(2)
                    .text_ellipsis()
                    .max_h(px(40.))
                    .overflow_hidden()
                    .cursor_pointer()
                    .child(self.page.description.clone())
                    .on_click(cx.listener(|view, _, window, cx| {
                        window.focus(&view.focus, cx);
                        view.overlay = Some(Overlay::Description {
                            title: view.page.title.clone(),
                            text: view.page.description.clone(),
                        });
                        cx.notify();
                    })),
            );
        }
        let mut actions = div()
            .flex()
            .items_center()
            .flex_wrap()
            .gap(px(12.))
            .when(stacked, |d| d.justify_center());
        if has_tracks {
            actions = actions.child(
                div()
                    .id("play-all")
                    .h(px(40.))
                    .px(px(24.))
                    .rounded_full()
                    .bg(self.theme.text)
                    .text_color(self.theme.bg)
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .font_weight(FontWeight::MEDIUM)
                    .cursor_pointer()
                    .child(icon("play", 24., self.theme.bg))
                    .child("Play")
                    .on_click(cx.listener(|view, _, _, cx| view.replace_page_queue(0, cx))),
            );
        }
        if let [Item::Track(track)] = self.rows.as_slice()
            && track
                .details
                .episode
                .as_ref()
                .is_some_and(|episode| !episode.chapters.is_empty())
        {
            let track = track.clone();
            actions = actions.child(
                div()
                    .id("page-chapters")
                    .px(px(12.))
                    .py(px(10.))
                    .cursor_pointer()
                    .child("Chapters")
                    .on_click(cx.listener(move |view, _, window, cx| {
                        window.focus(&view.focus, cx);
                        view.overlay = Some(Overlay::Chapters(Box::new(track.clone())));
                        cx.notify();
                    })),
            );
        }
        if !self.page.countries.is_empty() {
            let label = self
                .page
                .countries
                .iter()
                .find(|choice| choice.selected)
                .map_or("Country", |choice| choice.label.as_str())
                .to_owned();
            actions = actions.child(
                filter_chip("chart-country", label, false, self.theme)
                    .on_click(cx.listener(|view, _, window, cx| view.country_picker(window, cx))),
            );
        }
        if self.account.is_some() && self.page.editable {
            actions = actions
                .child(
                    filter_chip("edit-playlist", "Edit playlist", false, self.theme).on_click(
                        cx.listener(|view, _, window, cx| {
                            view.playlist_form(true, None, window, cx)
                        }),
                    ),
                )
                .child(
                    filter_chip("delete-playlist", "Delete", false, self.theme).on_click(
                        cx.listener(|view, _, window, cx| {
                            window.focus(&view.focus, cx);
                            if let Route::Browse(link) = &view.route {
                                view.overlay = Some(Overlay::DeletePlaylist {
                                    id: link.id.clone(),
                                    title: view.page.title.clone(),
                                });
                                cx.notify();
                            }
                        }),
                    ),
                );
        }
        if self.account.is_some()
            && matches!(
                self.route,
                Route::Saved | Route::Library(Section::Playlists)
            )
        {
            actions = actions.child(
                filter_chip("new-playlist", "New playlist", false, self.theme).on_click(
                    cx.listener(|view, _, window, cx| view.playlist_form(false, None, window, cx)),
                ),
            );
        }
        let heading = heading.child(actions);
        match cover {
            Some(cover) if stacked => div()
                .flex()
                .flex_col()
                .items_center()
                .gap(px(20.))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .items_center()
                        .gap(px(16.))
                        .children(creator)
                        .child(cover),
                )
                .child(heading)
                .into_any_element(),
            Some(cover) => div()
                .flex()
                .items_start()
                .gap(px(24.))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .items_center()
                        .gap(px(16.))
                        .children(creator)
                        .child(cover),
                )
                .child(heading.flex_1().min_w_0())
                .into_any_element(),
            None => div()
                .flex()
                .flex_col()
                .gap(px(16.))
                .children(creator)
                .child(heading)
                .into_any_element(),
        }
    }
}
