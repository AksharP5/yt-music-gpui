//! Shared album/playlist headers, metadata, and collection actions.

use super::super::*;

impl Music {
    pub(in crate::app) fn page_collection(&self) -> Option<Collection> {
        let Route::Browse(link) = &self.route else {
            return None;
        };
        if !["MPRE", "VL", "MPSP"]
            .iter()
            .any(|prefix| link.id.starts_with(prefix))
        {
            return None;
        }
        let header = self.page.header.as_ref()?;
        Some(Collection {
            id: link.id.clone(),
            title: self.page.title.clone(),
            subtitle: header.subtitle.clone(),
            thumbnail: header.thumbnail.clone(),
            details: header.collection.clone(),
        })
    }

    pub(in crate::app) fn page_heading(
        &mut self,
        stacked: bool,
        edge: f32,
        text_size: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let header = self.page.header.clone();
        let title = self.page.title.clone();
        let has_tracks = self.rows.iter().any(|item| matches!(item, Item::Track(_)));
        let collection = self.page_collection();
        let podcast = matches!(&self.route, Route::Browse(link) if link.id.starts_with("MPSP"));
        let mut cover = None;
        let mut creator = None;
        let mut heading = div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .when(stacked, |d| d.w_full().items_center().text_center());
        if let Some(header) = header {
            let avatar = header.artist_thumbnail.as_deref().and_then(|url| {
                self.artwork.get(url).map(|image| {
                    img(image)
                        .size(px(16.))
                        .flex_shrink_0()
                        .rounded_full()
                        .object_fit(ObjectFit::Cover)
                })
            });
            creator = header.artist.map(|artist| {
                let strapline = div()
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    .text_size(px(text_size))
                    .line_height(relative(1.2))
                    .children(avatar);
                match artist {
                    HeaderArtist::Link(link) => strapline
                        .id("collection-artist")
                        .keyboard_control(self.theme)
                        .cursor_pointer()
                        .child(link.title.clone())
                        .on_click(cx.listener(move |view, _, _, cx| {
                            view.navigate(Route::Browse(link.clone()), cx)
                        }))
                        .into_any_element(),
                    HeaderArtist::Text(text) => strapline.child(text).into_any_element(),
                }
            });
            if let Some(url) = header.thumbnail {
                cover = Some(match self.artwork.get(&url) {
                    Some(image) => img(image)
                        .size(px(edge))
                        .flex_shrink_0()
                        .rounded(px(12.))
                        .object_fit(ObjectFit::Cover)
                        .into_any_element(),
                    None => div()
                        .size(px(edge))
                        .rounded(px(12.))
                        .bg(self.theme.raised)
                        .into_any_element(),
                });
            }
            heading = heading.child(
                div()
                    .w(relative(if stacked { 1.04 } else { 1. }))
                    .text_size(px(if stacked { 28. } else { 32. }))
                    .line_height(relative(1.2))
                    .font_weight(FontWeight::BOLD)
                    .child(title),
            );
            if !header.owner.is_empty() {
                heading = heading.child(
                    div()
                        .text_size(px(12.))
                        .line_height(px(18.))
                        .child(header.owner),
                );
            }
            if !header.subtitle.is_empty() || !header.stats.is_empty() {
                heading = heading.child(
                    div()
                        .flex()
                        .flex_col()
                        .when(stacked, |d| d.items_center())
                        .text_size(px(text_size))
                        .line_height(relative(1.2))
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
            let description = div()
                .id("page-description")
                .keyboard_control(self.theme)
                .flex_1()
                .min_w_0()
                .mx(px(-4.))
                .text_size(px(text_size))
                .text_color(self.theme.muted)
                .line_height(relative(1.2))
                .line_clamp(2)
                .text_ellipsis()
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
                }));
            // Extra measured width avoids GPUI wrapping a line that fits on the web.
            heading = heading.child(div().w_full().flex().child(description));
        }
        let mut actions = div()
            .flex()
            .items_center()
            .flex_wrap()
            .gap(px(if stacked { 32. } else { 12. }))
            .when(stacked, |d| d.mt(px(8.)).justify_center());
        if let Some(collection) = collection.as_ref() {
            let selected = collection.clone();
            let saved = self.is_saved(collection);
            actions = actions.child(
                button(
                    "save-collection",
                    if saved { "bookmark-filled" } else { "bookmark" },
                    self.theme,
                )
                .bg(self.theme.panel)
                .when(podcast, |button| {
                    button.w_auto().px(px(16.)).gap(px(8.)).child(if saved {
                        "Saved to library"
                    } else {
                        "Save to library"
                    })
                })
                .on_click(
                    cx.listener(move |view, _, _, cx| view.save_collection(selected.clone(), cx)),
                ),
            );
        }
        if has_tracks && !podcast {
            actions = actions.child(
                div()
                    .id("play-all")
                    .keyboard_control(self.theme)
                    .size(px(64.))
                    .rounded_full()
                    .bg(self.theme.text)
                    .text_color(self.theme.bg)
                    .flex()
                    .items_center()
                    .justify_center()
                    .gap(px(8.))
                    .font_weight(FontWeight::MEDIUM)
                    .cursor_pointer()
                    .hover(|style| style.bg(rgb(0xe0e0e0)))
                    .tooltip(|_, cx| {
                        cx.new(|_| Hint {
                            text: "Play",
                            theme: Theme::load(),
                        })
                        .into()
                    })
                    .child(icon("play", 32., self.theme.bg))
                    .on_click(cx.listener(|view, _, _, cx| view.replace_page_queue(0, cx))),
            );
        }
        if let Some(collection) = collection {
            actions = actions.child(
                self.menu_button("collection-header-menu", cx, move |view, window, cx| {
                    view.open_collection_menu(collection.clone(), window, cx);
                })
                .bg(self.theme.panel),
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
                    .keyboard_control(self.theme)
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
                .gap(px(16.))
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
