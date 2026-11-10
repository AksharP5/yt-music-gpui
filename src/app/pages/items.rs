//! Catalog cards and track/collection rows shared across pages.

use super::super::*;
use super::PageLayout;

/// Web row thumbnails dim to this on hover behind the play glyph.
const THUMBNAIL_SCRIM: u32 = 0x000000cc;

impl Music {
    /// A row thumbnail that swaps to a play glyph while its row is hovered.
    /// Video and episode frames keep 16:9 instead of being cropped square.
    pub(in crate::app) fn row_thumbnail(
        &mut self,
        url: Option<&str>,
        edge: f32,
        shape: ArtworkShape,
        wide: bool,
        group: &'static str,
    ) -> AnyElement {
        let image = url.and_then(|url| self.artwork.get(url));
        let radius = match shape {
            ArtworkShape::Square => 4.,
            ArtworkShape::Circle => edge / 2.,
        };
        let frame = div()
            .size(px(edge))
            .flex_shrink_0()
            .relative()
            .flex()
            .items_center()
            .justify_center();
        let art = match image {
            Some(image) => img(image)
                .w(px(edge))
                .h(px(if wide { edge * 9. / 16. } else { edge }))
                .rounded(px(radius))
                .object_fit(ObjectFit::Cover)
                .into_any_element(),
            None => div()
                .size(px(edge))
                .rounded(px(radius))
                .bg(self.theme.raised)
                .flex()
                .items_center()
                .justify_center()
                .child(icon("music", edge.min(32.) * 0.6, self.theme.muted))
                .into_any_element(),
        };
        frame
            .child(art)
            .child(
                div()
                    .invisible()
                    .group_hover(group, |style| style.visible())
                    .absolute()
                    .inset_0()
                    .rounded(px(radius))
                    .bg(rgba(THUMBNAIL_SCRIM))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(icon("play", 24., self.theme.text)),
            )
            .into_any_element()
    }

    /// The explicit badge and secondary line shared by rows and cards.
    pub(in crate::app) fn byline(&self, explicit: bool, text: impl IntoElement, size: f32) -> Div {
        let line = div()
            .min_w_0()
            .text_size(px(size))
            .line_height(relative(1.2))
            .text_color(self.theme.muted);
        if !explicit {
            return line.truncate().child(text);
        }
        line.flex()
            .items_center()
            .gap(px(4.))
            .child(icon("explicit", 16., self.theme.muted).flex_shrink_0())
            .child(div().flex_1().min_w_0().truncate().child(text))
    }

    /// Text whose artist, album and show names open their pages, underlined
    /// on hover like the web's links. Clicking elsewhere on the text still
    /// reaches the row.
    pub(in crate::app) fn linked_text<'a>(
        &self,
        text: String,
        links: impl IntoIterator<Item = &'a BrowseLink>,
        cx: &Context<Self>,
    ) -> AnyElement {
        let spans = link_spans(&text, links);
        if spans.iter().all(|(_, link)| link.is_none()) {
            return text.into_any_element();
        }
        let last = spans.len() - 1;
        div()
            .flex()
            .min_w_0()
            .overflow_hidden()
            .whitespace_nowrap()
            .children(spans.into_iter().enumerate().map(|(index, (span, link))| {
                let span = div().child(span);
                // Only the last span shrinks, so the line ends in an ellipsis.
                let span = if index == last {
                    span.min_w_0().truncate()
                } else {
                    span.flex_shrink_0()
                };
                let Some(link) = link else {
                    return span.into_any_element();
                };
                span.id(SharedString::from(format!("text-link-{}-{index}", link.id)))
                    .keyboard_control(self.theme)
                    .cursor_pointer()
                    .hover(|style| style.underline())
                    .on_click(cx.listener(move |view, _, window, cx| {
                        cx.stop_propagation();
                        window.focus(&view.focus, cx);
                        view.navigate(Route::Browse(link.clone()), cx);
                    }))
                    .into_any_element()
            }))
            .into_any_element()
    }

    fn mood_button(
        &self,
        link: BrowseLink,
        id: usize,
        width: f32,
        cx: &Context<Self>,
    ) -> AnyElement {
        let accent = link
            .accent
            .map_or(self.theme.muted, |color| rgb(color).into());
        div()
            .id(("mood", id))
            .keyboard_control(self.theme)
            .w(px(width))
            .h(px(48.))
            .flex_shrink_0()
            .rounded(px(8.))
            .overflow_hidden()
            .bg(gpui::white().opacity(0.15))
            .hover(|style| style.bg(gpui::white().opacity(0.2)))
            .flex()
            .items_center()
            .cursor_pointer()
            .child(
                div()
                    .w(px(6.))
                    .h_full()
                    .flex_shrink_0()
                    .rounded_l(px(7.))
                    .bg(accent),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .px(px(12.))
                    .text_size(px(14.))
                    .line_height(relative(1.2))
                    .font_weight(FontWeight::MEDIUM)
                    .line_clamp(2)
                    .text_ellipsis()
                    .child(link.title.clone()),
            )
            .on_click(cx.listener(move |view, _, window, cx| {
                window.focus(&view.focus, cx);
                view.navigate(Route::Browse(link.clone()), cx);
            }))
            .into_any_element()
    }

    pub(in crate::app) fn card(
        &mut self,
        item: Item,
        id: usize,
        width: f32,
        layout: ShelfLayout,
        page: PageLayout,
        cx: &Context<Self>,
    ) -> AnyElement {
        if let Item::Navigation(link) = item {
            return self.mood_button(link, id, width, cx);
        }
        let text_size = page.text_size();
        let title = item.title().to_owned();
        if layout == ShelfLayout::Songs {
            let (byline, explicit, wide) = match &item {
                Item::Track(track) => (
                    self.linked_text(
                        track
                            .details
                            .byline
                            .clone()
                            .unwrap_or_else(|| track.artist.clone()),
                        &track.details.links,
                        cx,
                    ),
                    track.details.explicit,
                    track.details.wide_thumbnail,
                ),
                _ => (item.subtitle().to_owned().into_any_element(), false, false),
            };
            let art = self.row_thumbnail(
                item.thumbnail(),
                48.,
                ArtworkShape::Square,
                wide,
                "music-row",
            );
            return div()
                .id(("quick-pick", id))
                .keyboard_control(self.theme)
                .group("music-row")
                .w(px(width))
                .h(px(48.))
                .pl(px(8.))
                .flex()
                .items_center()
                .gap(px(16.))
                .rounded(px(4.))
                .cursor_pointer()
                .child(art)
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap(px(4.))
                        .child(
                            div()
                                .truncate()
                                .text_size(px(text_size))
                                .line_height(relative(1.2))
                                .font_weight(FontWeight::MEDIUM)
                                .child(title),
                        )
                        .child(self.byline(explicit, byline, text_size)),
                )
                .child(
                    self.menu_button(("quick-menu", id), cx, move |view, window, cx| {
                        view.open_item_menu(id, window, cx);
                    })
                    .opacity(0.)
                    .group_hover("music-row", |style| style.opacity(1.))
                    .focus_visible(|style| style.opacity(1.).border_color(self.theme.text)),
                )
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |view, _, window, cx| {
                        cx.stop_propagation();
                        view.open_item_menu(id, window, cx);
                    }),
                )
                .on_click(cx.listener(move |view, _, window, cx| {
                    window.focus(&view.focus, cx);
                    view.play_item(id, cx);
                }))
                .into_any_element();
        }
        let subtitle_lines = if layout == ShelfLayout::Cards
            && matches!(&self.route, Route::Browse(link) if link.id.starts_with("MPRE"))
        {
            1
        } else {
            2
        };
        let subtitle = item.subtitle().to_owned();
        let explicit = match &item {
            Item::Track(track) => track.details.explicit,
            Item::Collection(collection) => collection.details.explicit,
            Item::Navigation(_) => false,
        };
        let image = item.thumbnail().and_then(|url| self.artwork.get(url));
        let artist =
            matches!(&item, Item::Collection(collection) if collection.id.starts_with("UC"));
        let play = match &item {
            Item::Collection(collection) => collection.details.play.clone(),
            _ => None,
        };
        let playable = play.is_some() || matches!(&item, Item::Track(_));
        let mut art = div()
            .relative()
            .w(px(width))
            .h(px(if layout == ShelfLayout::Videos {
                width * 9. / 16.
            } else {
                width
            }))
            .rounded(px(8.))
            .when(artist, |d| d.rounded_full())
            .overflow_hidden()
            .bg(self.theme.raised);
        // GPUI clips children to rectangles, so the image carries its own corners.
        let radius = if artist { width / 2. } else { 8. };
        art = if let Some(image) = image {
            art.child(
                img(image)
                    .size_full()
                    .rounded(px(radius))
                    .object_fit(ObjectFit::Cover),
            )
        } else {
            art.flex()
                .items_center()
                .justify_center()
                .child(icon("music", 32., self.theme.muted))
        };
        // One hover layer holds the web's top scrim and play button.
        let mut hover = div()
            .invisible()
            .group_hover("music-card", |style| style.visible())
            .absolute()
            .inset_0()
            .rounded(px(radius))
            .when(!artist, |layer| {
                layer.bg(linear_gradient(
                    180.,
                    linear_color_stop(rgba(0x00000080), 0.),
                    linear_color_stop(rgba(0x00000000), 0.5),
                ))
            });
        if playable {
            let play_button = div()
                .id(("card-play", id))
                .absolute()
                .right(px(12.))
                .bottom(px(12.))
                .size(px(40.))
                .rounded_full()
                .bg(rgba(0x00000099))
                .hover(|style| style.bg(rgba(0x000000cc)))
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .child(icon("play", 24., self.theme.text));
            hover = hover.child(match play {
                Some(target) => play_button.on_click(cx.listener(move |view, _, window, cx| {
                    cx.stop_propagation();
                    window.focus(&view.focus, cx);
                    view.start_queue(PlaybackQuery::Playlist(target.clone()), cx);
                })),
                None => play_button.on_click(cx.listener(move |view, _, window, cx| {
                    cx.stop_propagation();
                    window.focus(&view.focus, cx);
                    view.play_item(id, cx);
                })),
            });
        }
        if playable || !artist {
            art = art.child(hover);
        }
        div()
            .id(("card", id))
            .keyboard_control(self.theme)
            .group("music-card")
            .w(px(width))
            .flex_shrink_0()
            .flex()
            .flex_col()
            .cursor_pointer()
            .when(artist, |card| card.items_center())
            .child(art.when(!artist, |art| {
                art.child(
                    self.menu_button(("card-menu", id), cx, move |view, window, cx| {
                        view.open_item_menu(id, window, cx);
                    })
                    .absolute()
                    .right(px(4.))
                    .top(px(8.))
                    .size(px(36.))
                    .opacity(0.)
                    .group_hover("music-card", |style| style.opacity(1.))
                    .focus_visible(|style| style.opacity(1.).border_color(self.theme.text)),
                )
            }))
            .child(
                div()
                    .mt(px(16.))
                    .w_full()
                    .text_size(px(text_size))
                    .line_height(relative(1.2))
                    .font_weight(FontWeight::MEDIUM)
                    .line_clamp(2)
                    .text_ellipsis()
                    .when(artist, |title| title.text_center())
                    .child(title),
            )
            .child({
                let line = div()
                    .mt(px(3.))
                    .w_full()
                    .text_size(px(text_size))
                    .line_height(relative(1.2))
                    .text_color(self.theme.muted)
                    .when(artist, |line| line.text_center());
                if explicit {
                    line.flex()
                        .items_start()
                        .gap(px(4.))
                        .child(
                            div()
                                .h(px(text_size * 1.2))
                                .flex()
                                .items_center()
                                .flex_shrink_0()
                                .child(icon("explicit", 16., self.theme.muted)),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .line_clamp(subtitle_lines)
                                .text_ellipsis()
                                .child(subtitle),
                        )
                } else {
                    line.line_clamp(subtitle_lines)
                        .text_ellipsis()
                        .child(subtitle)
                }
            })
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |view, _, window, cx| {
                    cx.stop_propagation();
                    view.open_item_menu(id, window, cx);
                }),
            )
            .on_click(cx.listener(move |view, _, window, cx| {
                window.focus(&view.focus, cx);
                view.play_item(id, cx);
            }))
            .into_any_element()
    }

    pub(in crate::app) fn row(
        &mut self,
        index: usize,
        inset: f32,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let item = self.rows[index].clone();
        if let Item::Navigation(link) = item {
            return self
                .navigation(link, ("browse-link", index), cx)
                .w_full()
                .h(px(64.))
                .px(px(inset))
                .into_any_element();
        }
        let page = self.page_layout(window);
        let text_size = page.text_size();
        let style = if self.route.is_artist() {
            if window.viewport_size().width >= px(1200.) {
                RowStyle::ArtistTable
            } else {
                RowStyle::List
            }
        } else if matches!(&self.route, Route::Browse(link) if link.id.starts_with("MPRE"))
            && matches!(item, Item::Track(_))
        {
            RowStyle::Album
        } else if self.page.header.is_some() {
            RowStyle::Playlist
        } else if matches!(self.route, Route::Search(_)) {
            RowStyle::Search
        } else {
            RowStyle::List
        };
        let active = self.library.queue.current().is_some_and(|current| {
            current.id == item.id()
                && match &item {
                    Item::Track(track) if track.details.set_video_id.is_some() => {
                        current.details.set_video_id == track.details.set_video_id
                    }
                    _ => true,
                }
        }) && self.active_id.is_some();
        let track = match &item {
            Item::Track(track) => Some(track),
            _ => None,
        };
        let explicit = match &item {
            Item::Track(track) => track.details.explicit,
            Item::Collection(collection) => collection.details.explicit,
            Item::Navigation(_) => false,
        };
        let wide = track.is_some_and(|track| track.details.wide_thumbnail);
        let art = if style == RowStyle::Album {
            div()
                .size(px(48.))
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(text_size))
                .font_weight(FontWeight::MEDIUM)
                .text_color(rgb(0xaaaaaa))
                .when(active, |d| d.child(icon("volume", 20., self.theme.text)))
                .when(!active, |d| {
                    d.child(
                        div()
                            .group_hover("music-row", |d| d.invisible())
                            .child((index + 1).to_string()),
                    )
                    .child(
                        div()
                            .invisible()
                            .group_hover("music-row", |d| d.visible())
                            .absolute()
                            .child(icon("play", 24., self.theme.text)),
                    )
                })
                .relative()
                .into_any_element()
        } else {
            let thumbnail = item.thumbnail().map(str::to_owned).or_else(|| {
                self.page
                    .header
                    .as_ref()
                    .and_then(|header| header.thumbnail.clone())
            });
            let shape = if matches!(&item, Item::Collection(collection) if collection.id.starts_with("UC"))
            {
                ArtworkShape::Circle
            } else {
                ArtworkShape::Square
            };
            self.row_thumbnail(
                thumbnail.as_deref(),
                style.thumbnail(),
                shape,
                wide,
                "music-row",
            )
        };
        let title = item.title().to_owned();
        let plays = track.and_then(|track| track.details.plays.clone());
        let joined = |parts: &[Option<&str>]| {
            parts
                .iter()
                .flatten()
                .filter(|part| !part.is_empty())
                .copied()
                .collect::<Vec<_>>()
                .join(" • ")
        };
        let subtitle = match (style, track) {
            (RowStyle::Search, Some(track)) => track
                .details
                .byline
                .clone()
                .unwrap_or_else(|| joined(&[Some(&track.artist), plays.as_deref()])),
            (RowStyle::Playlist, Some(track)) => joined(&[
                Some(&track.artist),
                plays.as_deref(),
                track.album.as_deref(),
            ]),
            (_, Some(track)) => joined(&[Some(&track.artist), plays.as_deref()]),
            _ => item.subtitle().to_owned(),
        };
        let menu_index = index;
        let mut row = div()
            .id(("song", index))
            .keyboard_control(self.theme)
            .group("music-row")
            .relative()
            .w_full()
            .h(px(style.height()))
            .pl(px(style.padding()))
            .pr(px(16.))
            .flex()
            .items_center()
            .gap(px(if style == RowStyle::ArtistTable {
                24.
            } else {
                16.
            }))
            .rounded(px(4.))
            .when(style == RowStyle::ArtistTable, |row| row.rounded_none())
            .when(active, |d| d.bg(self.theme.selection))
            .cursor_pointer()
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |view, _, window, cx| {
                    cx.stop_propagation();
                    view.open_item_menu(menu_index, window, cx);
                }),
            )
            .on_click(cx.listener(move |view, _, window, cx| {
                window.focus(&view.focus, cx);
                view.play_item(index, cx);
            }))
            .child(art);
        row = if style == RowStyle::ArtistTable {
            // The web gives the title two fifths of the columns and each other
            // column a fifth less its 16px gap, leaving 150px for the controls.
            let columns = (page.column - 214.).max(0.);
            let column = |text: AnyElement| {
                div()
                    .w(px((columns / 5. - 16.).max(0.)))
                    .flex_shrink_0()
                    .truncate()
                    .text_color(self.theme.muted)
                    .child(text)
            };
            let links = track.map_or(&[][..], |track| track.details.links.as_slice());
            let is_album = |link: &&BrowseLink| link.id.starts_with("MPRE");
            let artist = self.linked_text(
                track.map(|track| track.artist.clone()).unwrap_or_default(),
                links.iter().filter(|link| !is_album(link)),
                cx,
            );
            let album = self.linked_text(
                track
                    .and_then(|track| track.album.clone())
                    .unwrap_or_default(),
                links.iter().filter(is_album),
                cx,
            );
            row.child(
                div()
                    .w(px(columns))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap(px(16.))
                    .text_size(px(text_size))
                    .child(
                        div()
                            .w(px(columns * 0.4))
                            .flex_shrink_0()
                            .truncate()
                            .font_weight(FontWeight::MEDIUM)
                            .child(title),
                    )
                    .child(column(artist))
                    .child(column(plays.unwrap_or_default().into_any_element()))
                    .child(column(album)),
            )
        } else {
            row.child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(if style == RowStyle::Search { 4. } else { 8. }))
                    .child(
                        div()
                            .truncate()
                            .text_size(px(text_size))
                            .line_height(relative(1.2))
                            .font_weight(FontWeight::MEDIUM)
                            .child(title),
                    )
                    .when(!subtitle.is_empty() || explicit, |d| {
                        // Album bylines stay 14px when the web enlarges titles.
                        let size = if style == RowStyle::Album {
                            14.
                        } else {
                            text_size
                        };
                        let links = track.map_or(&[][..], |track| track.details.links.as_slice());
                        d.child(self.byline(explicit, self.linked_text(subtitle, links, cx), size))
                    }),
            )
        };
        let Some(track) = track.cloned() else {
            row = row.child(
                self.menu_button(("collection-menu", index), cx, move |view, window, cx| {
                    view.open_item_menu(index, window, cx);
                })
                .opacity(0.)
                .group_hover("music-row", |style| style.opacity(1.))
                .focus_visible(|style| style.opacity(1.).border_color(self.theme.text)),
            );
            return div().w_full().px(px(inset)).child(row).into_any_element();
        };
        let liked = self.is_liked(&track);
        let like_track = track.clone();
        let menu_track = track.clone();
        if style != RowStyle::ArtistTable {
            let duration = track.duration.map(|s| clock(s as f64)).unwrap_or_else(|| {
                track
                    .details
                    .episode
                    .as_ref()
                    .map(|episode| episode.duration_text.clone())
                    .unwrap_or_default()
            });
            // Like, dislike and menu replace the duration on hover. GPUI hover
            // styles cannot move layout, so the row keeps their room clear.
            row = row.child(
                div()
                    .w(px(104.))
                    .flex_shrink_0()
                    .flex()
                    .justify_end()
                    .text_size(px(text_size))
                    .text_color(self.theme.muted)
                    .group_hover("music-row", |style| style.invisible())
                    .child(duration),
            );
        }
        let artist_table = style == RowStyle::ArtistTable;
        // Three 36px buttons, 2px apart, fill the 104px zone plus the row's padding.
        let controls = div()
            .flex()
            .items_center()
            .gap(px(2.))
            .pr(px(8.))
            .child(
                button(
                    ("like", index),
                    if liked { "thumb-up-filled" } else { "thumb-up" },
                    self.theme,
                )
                .size(px(36.))
                .when(!liked, |d| {
                    d.opacity(0.)
                        .group_hover("music-row", |d| d.opacity(1.))
                        .focus_visible(|style| style.opacity(1.).border_color(self.theme.text))
                })
                .on_click(cx.listener(move |view, _, _, cx| {
                    cx.stop_propagation();
                    view.toggle_like(&like_track, cx);
                })),
            )
            .when(!artist_table, |d| {
                d.child(
                    button(("dislike", index), "thumb-down", self.theme)
                        .size(px(36.))
                        .opacity(0.)
                        .group_hover("music-row", |d| d.opacity(1.))
                        .focus_visible(|style| style.opacity(1.).border_color(self.theme.text))
                        .on_click(cx.listener(move |view, _, window, cx| {
                            cx.stop_propagation();
                            if view.account.is_none() {
                                view.connect_account(window, cx);
                                return;
                            }
                            let disliked = view
                                .ratings
                                .get(&track.id)
                                .copied()
                                .or(track.details.rating)
                                == Some(Rating::Dislike);
                            view.mutate(
                                Mutation::RateSong {
                                    track: track.clone(),
                                    rating: if disliked {
                                        Rating::Indifferent
                                    } else {
                                        Rating::Dislike
                                    },
                                },
                                cx,
                            );
                        })),
                )
            })
            .child(
                self.menu_button(("track-menu", index), cx, move |view, window, cx| {
                    view.open_track_menu(menu_track.clone(), Some(index), window, cx);
                })
                .size(px(36.))
                .opacity(0.)
                .group_hover("music-row", |d| d.opacity(1.))
                .focus_visible(|style| style.opacity(1.).border_color(self.theme.text)),
            );
        row = row.child(controls.absolute().right_0().top_0().h_full());
        div()
            .w_full()
            .px(px(inset))
            .when(style.gap() > 0., |d| d.pb(px(style.gap())))
            .child(row)
            .when(artist_table, |d| {
                d.child(div().h(px(1.)).bg(gpui::white().opacity(0.1)))
            })
            .into_any_element()
    }
}

/// Row geometry per page, measured from music.youtube.com.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RowStyle {
    /// Search results: 56px art on an 80px pitch.
    Search,
    /// Album tracks: an index column instead of art.
    Album,
    /// Playlist and other collection tracks.
    Playlist,
    /// The artist page's Top songs table.
    ArtistTable,
    /// Library, history, charts and other plain lists.
    List,
}

impl RowStyle {
    fn height(self) -> f32 {
        match self {
            Self::Search => 80.,
            Self::Album | Self::Playlist => 68.,
            Self::ArtistTable => 48.,
            Self::List => 64.,
        }
    }

    /// Space below each row; uniform lists need the gap inside the row slot.
    fn gap(self) -> f32 {
        match self {
            Self::Album | Self::Playlist => 8.,
            _ => 0.,
        }
    }

    fn padding(self) -> f32 {
        match self {
            Self::Album | Self::Playlist => 16.,
            _ => 8.,
        }
    }

    fn thumbnail(self) -> f32 {
        match self {
            Self::Search => 56.,
            Self::ArtistTable => 32.,
            _ => 48.,
        }
    }
}

/// Splits text into plain spans and the names that `links` open. Links come in
/// the text's order, so each is looked for after the previous match; one the
/// text does not contain is skipped.
fn link_spans<'a>(
    text: &str,
    links: impl IntoIterator<Item = &'a BrowseLink>,
) -> Vec<(String, Option<BrowseLink>)> {
    let mut spans = Vec::new();
    let mut rest = 0;
    for link in links {
        if link.title.is_empty() {
            continue;
        }
        let Some(start) = text[rest..].find(&link.title).map(|start| rest + start) else {
            continue;
        };
        if start > rest {
            spans.push((text[rest..start].to_owned(), None));
        }
        rest = start + link.title.len();
        spans.push((link.title.clone(), Some(link.clone())));
    }
    if rest < text.len() || spans.is_empty() {
        spans.push((text[rest..].to_owned(), None));
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::link_spans;
    use crate::catalog::BrowseLink;

    #[test]
    fn byline_links_follow_the_text_order_and_skip_missing_names() {
        let links = [
            BrowseLink::new("UCweezer", "Weezer"),
            BrowseLink::new("UCother", "Not in this line"),
            BrowseLink::new("MPREweezer", "Weezer"),
        ];
        let spans = link_spans("Song • Weezer • Weezer • 1994", &links);
        let shape: Vec<_> = spans
            .iter()
            .map(|(text, link)| (text.as_str(), link.as_ref().map(|link| link.id.as_str())))
            .collect();
        assert_eq!(
            shape,
            [
                ("Song • ", None),
                ("Weezer", Some("UCweezer")),
                (" • ", None),
                ("Weezer", Some("MPREweezer")),
                (" • 1994", None),
            ]
        );
    }
}
