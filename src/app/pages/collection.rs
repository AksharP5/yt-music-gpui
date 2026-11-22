//! Shared album/playlist headers, metadata, and collection actions.

use super::super::*;
use super::PageLayout;
use crate::input::{InputEvent, SearchInput};

/// Find in playlist: the bar above a playlist's songs and its text.
pub(in crate::app) struct PlaylistFind {
    input: Entity<SearchInput>,
    query: String,
    _subscription: Subscription,
}

impl PlaylistFind {
    pub(in crate::app) fn matches(&self, item: &Item) -> bool {
        find_matches(&self.query, item)
    }

    pub(in crate::app) fn text(&self, cx: &App) -> String {
        self.input.read(cx).value().trim().to_owned()
    }

    pub(in crate::app) fn filtering(&self) -> bool {
        !self.query.is_empty()
    }
}

/// Like the web, a song matches when its title, artists or album contain
/// the lowercased query.
fn find_matches(query: &str, item: &Item) -> bool {
    let Item::Track(track) = item else {
        return false;
    };
    [
        Some(track.title.as_str()),
        Some(track.artist.as_str()),
        track.album.as_deref(),
    ]
    .into_iter()
    .flatten()
    .any(|text| text.to_lowercase().contains(query))
}

impl Music {
    pub(in crate::app) fn open_playlist_find(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(find) = &self.playlist_find {
            window.focus(&find.input.focus_handle(cx), cx);
            return;
        }
        let input = cx.new(|cx| {
            let mut input = SearchInput::new(cx);
            input.set_placeholder("Find on this page", cx);
            input.set_text_size(px(13.33));
            input
        });
        let subscription =
            cx.subscribe_in(
                &input,
                window,
                |view, input, event, window, cx| match event {
                    InputEvent::Changed => {
                        let query = input.read(cx).value().trim().to_lowercase();
                        if let Some(find) = &mut view.playlist_find {
                            find.query = query;
                        }
                        view.rebuild_rows();
                        cx.notify();
                    }
                    InputEvent::Blur => view.close_playlist_find(window, cx),
                    InputEvent::Submit => {}
                },
            );
        window.focus(&input.focus_handle(cx), cx);
        self.playlist_find = Some(PlaylistFind {
            input,
            query: String::new(),
            _subscription: subscription,
        });
        self.rebuild_rows();
        cx.notify();
    }

    fn close_playlist_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.playlist_find.take().is_some() {
            self.rebuild_rows();
            window.focus(&self.focus, cx);
            cx.notify();
        }
    }

    /// The web's Find in playlist bar: a 28px field with a clear button,
    /// and Cancel beside it, 20px above the songs.
    pub(in crate::app) fn playlist_find_bar(&self, cx: &mut Context<Self>) -> Option<Div> {
        let find = self.playlist_find.as_ref()?;
        let text = rgb(0xf1f1f1);
        let clear = find.filtering().then(|| {
            let input = find.input.clone();
            div()
                .id("playlist-find-clear")
                .keyboard_control(self.theme)
                .size(px(24.))
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .child(icon("close", 16., text.into()))
                .on_click(cx.listener(move |_, _, window, cx| {
                    input.update(cx, |input, cx| {
                        input.set_value(String::new(), cx);
                        cx.emit(InputEvent::Changed);
                    });
                    window.focus(&input.focus_handle(cx), cx);
                }))
        });
        Some(
            div()
                .h(px(30.))
                .mb(px(20.))
                .flex()
                .items_center()
                .gap(px(2.))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .h(px(28.))
                        .pl(px(10.))
                        .pr(px(2.))
                        .rounded(px(13.))
                        .bg(gpui::white().opacity(0.15))
                        .flex()
                        .items_center()
                        .child(div().flex_1().min_w_0().child(find.input.clone()))
                        .children(clear),
                )
                .child(
                    div()
                        .id("playlist-find-cancel")
                        .keyboard_control(self.theme)
                        .h(px(24.))
                        .px(px(8.))
                        .flex()
                        .items_center()
                        .text_size(px(12.))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(text)
                        .cursor_pointer()
                        .child("Cancel")
                        .on_click(
                            cx.listener(|view, _, window, cx| view.close_playlist_find(window, cx)),
                        ),
                ),
        )
    }

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

    /// An album, playlist or podcast header: cover, title, metadata and actions.
    pub(in crate::app) fn page_heading(
        &mut self,
        edge: f32,
        layout: PageLayout,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let text_size = layout.text_size();
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
            .w_full()
            .items_center()
            .text_center();
        if let Some(header) = header {
            let avatar = header.artist_thumbnail.as_deref().and_then(|url| {
                self.artwork.get(url, size(px(16.), px(16.))).map(|image| {
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
                cover = Some(match self.artwork.get(&url, size(px(edge), px(edge))) {
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
                    .w(relative(1.04))
                    .text_size(px(layout.heading_size().min(28.)))
                    .line_height(relative(1.2))
                    .font_weight(FontWeight::BOLD)
                    .child(title),
            );
            if !header.owner.is_empty() {
                let avatar = header.owner_thumbnail.as_deref().map(|url| {
                    match self.artwork.get(url, size(px(24.), px(24.))) {
                        Some(image) => img(image)
                            .size(px(24.))
                            .rounded_full()
                            .object_fit(ObjectFit::Cover)
                            .into_any_element(),
                        None => div()
                            .size(px(24.))
                            .rounded_full()
                            .bg(self.theme.raised)
                            .into_any_element(),
                    }
                });
                heading = heading.child(
                    div()
                        .when(avatar.is_some(), |owner| owner.h(px(32.)))
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .text_size(px(12.))
                        .line_height(px(18.))
                        .children(avatar)
                        .child(header.owner),
                );
            }
            if !header.subtitle.is_empty() || !header.stats.is_empty() {
                heading = heading.child(
                    div()
                        .flex()
                        .flex_col()
                        .items_center()
                        .text_size(px(text_size))
                        .line_height(relative(1.2))
                        .text_color(self.theme.muted)
                        .when(!header.subtitle.is_empty(), |d| d.child(header.subtitle))
                        .when(!header.stats.is_empty(), |d| d.child(header.stats)),
                );
            }
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
                    let description = Overlay::Description {
                        title: view.page.title.clone(),
                        text: view.page.description.clone(),
                    };
                    view.open_overlay(description, window, cx);
                }));
            // Extra measured width avoids GPUI wrapping a line that fits on the web.
            heading = heading.child(div().w_full().flex().child(description));
        }
        let mut actions = div()
            .flex()
            .items_center()
            .flex_wrap()
            .gap(px(32.))
            .mt(px(8.))
            .justify_center();
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
            actions = actions.child(self.play_all_button(cx));
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
                        view.open_overlay(Overlay::Chapters(Box::new(track.clone())), window, cx);
                    })),
            );
        }
        if self.account.is_some() && self.page.editable {
            actions = actions
                .child(
                    filter_chip("edit-playlist", "Edit playlist", false, self.theme).on_click(
                        cx.listener(|view, _, window, cx| {
                            view.playlist_form(true, Vec::new(), window, cx)
                        }),
                    ),
                )
                .child(
                    filter_chip("delete-playlist", "Delete", false, self.theme).on_click(
                        cx.listener(|view, _, window, cx| {
                            if let Route::Browse(link) = &view.route {
                                let delete = Overlay::DeletePlaylist {
                                    id: link.id.clone(),
                                    title: view.page.title.clone(),
                                };
                                view.open_overlay(delete, window, cx);
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
                    cx.listener(|view, _, window, cx| {
                        view.playlist_form(false, Vec::new(), window, cx)
                    }),
                ),
            );
        }
        let heading = heading.child(actions);
        match cover {
            Some(cover) => div()
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
            None => div()
                .flex()
                .flex_col()
                .gap(px(16.))
                .children(creator)
                .child(heading)
                .into_any_element(),
        }
    }

    fn play_all_button(&self, cx: &mut Context<Self>) -> Stateful<Div> {
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
            .on_click(cx.listener(|view, _, _, cx| view.replace_page_queue(0, cx)))
    }

    /// The title of a page without a collection header, such as Charts with
    /// its country menu. Explore's pages have no play button on the web.
    pub(in crate::app) fn browse_heading(&mut self, inset: f32, cx: &mut Context<Self>) -> Div {
        let play = !matches!(self.route, Route::Browse(_))
            && self.rows.iter().any(|item| matches!(item, Item::Track(_)));
        let country = self
            .page
            .countries
            .iter()
            .find(|choice| choice.selected)
            .map(|choice| choice.label.clone())
            .or_else(|| (!self.page.countries.is_empty()).then(|| "Country".into()));
        div()
            .px(px(inset))
            .py(px(16.))
            .flex_shrink_0()
            .flex()
            .flex_col()
            .items_start()
            .gap(px(16.))
            .child(
                div()
                    .text_size(px(28.))
                    .line_height(px(38.))
                    .font_weight(FontWeight::BOLD)
                    .child(self.page.title.clone()),
            )
            .when(play, |heading| heading.child(self.play_all_button(cx)))
            .when_some(country, |heading, country| {
                heading.child(
                    menu_chip("chart-country", country, self.theme).on_click(
                        cx.listener(|view, _, window, cx| view.country_picker(window, cx)),
                    ),
                )
            })
    }

    /// A playlist's Sort button above its songs, with the web's menu below
    /// it. Editable playlists leave it out so a reorder never applies to a
    /// sorted view.
    pub(in crate::app) fn playlist_sort(&self, cx: &mut Context<Self>) -> Option<Div> {
        if self.page.editable || self.page.playlist_sort.is_empty() || self.rows.is_empty() {
            return None;
        }
        let theme = self.theme;
        let text = rgb(0xf1f1f1);
        let bounds = Rc::new(Cell::new(Bounds::default()));
        let measured = bounds.clone();
        let button = div()
            .id("playlist-sort")
            .keyboard_control(theme)
            .track_focus(&self.sort_focus)
            .relative()
            .child(
                canvas(move |rect, _, _| measured.set(rect), |_, _, _, _| {})
                    .absolute()
                    .inset_0(),
            )
            .h(px(24.))
            .flex()
            .items_center()
            .gap(px(8.))
            .text_size(px(14.))
            .font_weight(FontWeight::MEDIUM)
            .text_color(text)
            .cursor_pointer()
            .child(icon("sort", 24., text.into()))
            .child("Sort")
            .on_click(cx.listener(|view, _, window, cx| {
                if matches!(view.overlay, Some(Overlay::PlaylistSort)) {
                    view.close_overlay(window, cx);
                } else {
                    view.open_overlay(Overlay::PlaylistSort, window, cx);
                }
            }));
        let mut anchor = div().relative().flex().child(button);
        if matches!(self.overlay, Some(Overlay::PlaylistSort)) {
            let options = self
                .page
                .playlist_sort
                .iter()
                .enumerate()
                .map(|(index, choice)| {
                    let reload = choice.reload.clone();
                    let selected = choice.selected;
                    div()
                        .id(("playlist-sort-option", index))
                        .keyboard_control(theme)
                        .h(px(48.))
                        .px(px(16.))
                        .flex()
                        .items_center()
                        .whitespace_nowrap()
                        .text_size(px(14.))
                        .line_height(px(20.))
                        .text_color(text)
                        .cursor_pointer()
                        .when(selected, |row| row.bg(gpui::white().opacity(0.1)))
                        .hover(|row| row.bg(gpui::white().opacity(0.1)))
                        .child(choice.label.clone())
                        .on_click(cx.listener(move |view, _, window, cx| {
                            view.close_overlay(window, cx);
                            if !selected {
                                view.request(Query::Reload(reload.clone()), cx);
                            }
                        }))
                });
            let menu = div()
                .id("playlist-sort-menu")
                .track_focus(&self.overlay_focus)
                .tab_group()
                .tab_stop(false)
                .absolute()
                .top(px(36.))
                .left_0()
                .py(px(8.))
                .rounded(px(12.))
                .bg(rgb(0x282828))
                .shadow_lg()
                .occlude()
                .children(options)
                .on_mouse_down_out(
                    cx.listener(move |view, event: &MouseDownEvent, window, cx| {
                        if !bounds.get().contains(&event.position) {
                            view.close_overlay(window, cx);
                        }
                    }),
                );
            anchor = anchor.child(deferred(menu).with_priority(1));
        }
        // The web lines the button up with the songs' thumbnails, 18px above them.
        Some(div().pl(px(16.)).pb(px(18.)).child(anchor))
    }
}

#[cfg(test)]
mod tests {
    use super::find_matches;
    use crate::catalog::{Item, Track};

    #[test]
    fn find_in_playlist_matches_title_artist_or_album_ignoring_case() {
        let song = Item::Track(Track {
            id: "abcdefghijk".into(),
            title: "Old Time Rock & Roll".into(),
            artist: "Bob Seger & The Silver Bullet Band".into(),
            album: Some("Stranger In Town".into()),
            duration: None,
            thumbnail: None,
            details: Default::default(),
        });
        for query in ["rock", "seger", "stranger in"] {
            assert!(find_matches(query, &song), "{query}");
        }
        assert!(!find_matches("hurricane", &song));
    }
}
