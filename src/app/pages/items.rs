//! Catalog cards and track/collection rows shared across pages.

use super::super::*;

impl Music {
    pub(in crate::app) fn card(
        &mut self,
        item: Item,
        id: usize,
        width: f32,
        layout: ShelfLayout,
        cx: &Context<Self>,
    ) -> AnyElement {
        if let Item::Navigation(link) = item {
            return self
                .navigation(link, ("shelf-link", id), cx)
                .w(px(width))
                .h(px(44.))
                .into_any_element();
        }
        let title = item.title().to_owned();
        let subtitle = item.subtitle().to_owned();
        if layout == ShelfLayout::Songs {
            let art = self.art(item.thumbnail(), 48., ArtworkShape::Square);
            return div()
                .id(("quick-pick", id))
                .w(px(width))
                .h(px(64.))
                .flex()
                .items_center()
                .gap(px(16.))
                .rounded(px(4.))
                .cursor_pointer()
                .hover(|d| d.bg(self.theme.raised))
                .child(art)
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap(px(5.))
                        .child(
                            div()
                                .truncate()
                                .text_size(px(16.))
                                .font_weight(FontWeight::MEDIUM)
                                .child(title),
                        )
                        .child(
                            div()
                                .truncate()
                                .text_size(px(14.))
                                .text_color(self.theme.muted)
                                .child(subtitle),
                        ),
                )
                .child(
                    button(("quick-menu", id), "more", self.theme).on_click(cx.listener(
                        move |view, _, window, cx| {
                            cx.stop_propagation();
                            window.focus(&view.focus, cx);
                            if let Some(Item::Track(track)) = view.rows.get(id) {
                                view.open_track_menu(track.clone(), Some(id), window, cx);
                            }
                        },
                    )),
                )
                .on_click(cx.listener(move |view, _, window, cx| {
                    window.focus(&view.focus, cx);
                    view.play_item(id, cx);
                }))
                .into_any_element();
        }
        let image = item.thumbnail().and_then(|url| self.artwork.get(url));
        let artist =
            matches!(&item, Item::Collection(collection) if collection.id.starts_with("UC"));
        let play = match &item {
            Item::Collection(collection) => collection.details.play.clone(),
            _ => None,
        };
        let mut art = div()
            .relative()
            .w(px(width))
            .h(px(if layout == ShelfLayout::Videos {
                width * 9. / 16.
            } else {
                width
            }))
            .rounded(px(4.))
            .when(artist, |d| d.rounded_full())
            .overflow_hidden()
            .bg(self.theme.raised);
        art = if let Some(image) = image {
            art.child(
                img(image)
                    .size_full()
                    .object_fit(ObjectFit::Cover)
                    .when(artist, |image| image.rounded_full()),
            )
        } else {
            art.flex()
                .items_center()
                .justify_center()
                .child(icon("music", 32., self.theme.muted))
        };
        if let Some(target) = play {
            art = art.child(
                div()
                    .id(("collection-play", id))
                    .invisible()
                    .group_hover("music-card", |d| d.visible())
                    .absolute()
                    .right(px(12.))
                    .bottom(px(12.))
                    .size(px(48.))
                    .rounded_full()
                    .bg(rgba(0x030303dd))
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .child(icon("play", 28., self.theme.text))
                    .on_click(cx.listener(move |view, _, window, cx| {
                        cx.stop_propagation();
                        window.focus(&view.focus, cx);
                        view.start_queue(PlaybackQuery::Playlist(target.clone()), cx);
                    })),
            );
        }
        div()
            .id(("card", id))
            .group("music-card")
            .w(px(width))
            .flex_shrink_0()
            .flex()
            .flex_col()
            .gap(px(8.))
            .cursor_pointer()
            .child(art)
            .child(
                div()
                    .w_full()
                    .truncate()
                    .text_size(px(16.))
                    .font_weight(FontWeight::MEDIUM)
                    .child(title),
            )
            .child(
                div()
                    .w_full()
                    .truncate()
                    .text_size(px(16.))
                    .text_color(self.theme.muted)
                    .child(subtitle),
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
        let artist =
            self.route.is_artist() && window.viewport_size().width >= px(1200.) && !self.show_queue;
        let active = self.library.queue.current().is_some_and(|current| {
            current.id == item.id()
                && match &item {
                    Item::Track(track) if track.details.set_video_id.is_some() => {
                        current.details.set_video_id == track.details.set_video_id
                    }
                    _ => true,
                }
        }) && self.active_id.is_some();
        let numbered = matches!(&self.route,Route::Browse(link) if link.id.starts_with("MPRE"))
            && matches!(item, Item::Track(_));
        let art = if numbered {
            div()
                .w(px(32.))
                .h(px(48.))
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .text_color(self.theme.muted)
                .when(active, |d| d.child(icon("volume", 20., self.theme.text)))
                .when(!active, |d| d.child((index + 1).to_string()))
                .into_any_element()
        } else {
            let thumbnail = item.thumbnail().map(str::to_owned).or_else(|| {
                self.page
                    .header
                    .as_ref()
                    .and_then(|header| header.thumbnail.clone())
            });
            self.art(
                thumbnail.as_deref(),
                if artist { 40. } else { 48. },
                if matches!(&item, Item::Collection(collection) if collection.id.starts_with("UC"))
                {
                    ArtworkShape::Circle
                } else {
                    ArtworkShape::Square
                },
            )
        };
        let title = item.title().to_owned();
        let subtitle = match &item {
            Item::Track(track) if !artist => match &track.details.plays {
                Some(plays) => format!("{} • {plays}", track.artist),
                None => track.artist.clone(),
            },
            _ => item.subtitle().to_owned(),
        };
        let mut row = div()
            .id(("song", index))
            .group("music-row")
            .relative()
            .w_full()
            .h(px(if artist || numbered { 50. } else { 64. }))
            .flex()
            .items_center()
            .gap(px(if artist { 24. } else { 14. }))
            .rounded(px(4.))
            .when(active, |d| d.bg(self.theme.selection))
            .hover(|d| d.bg(self.theme.raised))
            .cursor_pointer()
            .on_click(cx.listener(move |view, _, window, cx| {
                window.focus(&view.focus, cx);
                view.play_item(index, cx);
            }))
            .child(art)
            .child(if artist {
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap(px(24.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(16.))
                            .child(title),
                    )
                    .child(
                        div()
                            .w(px(150.))
                            .min_w_0()
                            .truncate()
                            .text_size(px(16.))
                            .text_color(self.theme.muted)
                            .child(subtitle),
                    )
                    .when_some(
                        match &item {
                            Item::Track(track) => track.details.plays.clone(),
                            _ => None,
                        },
                        |d, plays| {
                            d.child(
                                div()
                                    .w(px(130.))
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(16.))
                                    .text_color(self.theme.muted)
                                    .child(plays),
                            )
                        },
                    )
                    .when_some(
                        match &item {
                            Item::Track(track) => track.album.clone(),
                            _ => None,
                        },
                        |d, album| {
                            d.child(
                                div()
                                    .w(px(180.))
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(16.))
                                    .text_color(self.theme.muted)
                                    .child(album),
                            )
                        },
                    )
                    .into_any_element()
            } else {
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(5.))
                    .child(
                        div()
                            .truncate()
                            .text_size(px(16.))
                            .when(active, |d| d.text_color(self.theme.accent))
                            .child(title),
                    )
                    .when(!numbered, |d| {
                        d.child(
                            div()
                                .truncate()
                                .text_size(px(14.))
                                .text_color(self.theme.muted)
                                .child(subtitle),
                        )
                    })
                    .into_any_element()
            });
        match item {
            Item::Navigation(link) => {
                return self
                    .navigation(link, ("browse-link", index), cx)
                    .w_full()
                    .h(px(64.))
                    .px(px(inset))
                    .hover(|d| d.bg(self.theme.raised))
                    .into_any_element();
            }
            Item::Track(track) => {
                let liked = self.is_liked(&track);
                let like_track = track.clone();
                let menu_track = track.clone();
                if !artist {
                    row = row.child(
                        div()
                            .w(px(if track.details.episode.is_some() {
                                100.
                            } else {
                                48.
                            }))
                            .text_size(px(14.))
                            .text_color(self.theme.muted)
                            .child(track.duration.map(|s| clock(s as f64)).unwrap_or_else(|| {
                                track
                                    .details
                                    .episode
                                    .as_ref()
                                    .map(|episode| episode.duration_text.clone())
                                    .unwrap_or_default()
                            })),
                    );
                }
                let controls = div()
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    .child(
                        button(
                            ("like", index),
                            if liked { "thumb-up-filled" } else { "thumb-up" },
                            self.theme,
                        )
                        .when(!liked && !artist, |d| {
                            d.invisible().group_hover("music-row", |d| d.visible())
                        })
                        .on_click(cx.listener(move |view, _, _, cx| {
                            cx.stop_propagation();
                            view.toggle_like(&like_track, cx);
                        })),
                    )
                    .when(!artist, |d| {
                        d.child(
                            button(("add", index), "plus", self.theme)
                                .invisible()
                                .group_hover("music-row", |d| d.visible())
                                .on_click(cx.listener(move |view, _, _, cx| {
                                    cx.stop_propagation();
                                    view.enqueue(track.clone(), cx);
                                })),
                        )
                    })
                    .child(
                        button(("track-menu", index), "more", self.theme)
                            .when(!artist, |d| {
                                d.invisible().group_hover("music-row", |d| d.visible())
                            })
                            .on_click(cx.listener(move |view, _, window, cx| {
                                cx.stop_propagation();
                                view.open_track_menu(menu_track.clone(), Some(index), window, cx);
                            })),
                    );
                row = row.child(if artist {
                    controls
                        .absolute()
                        .right_0()
                        .top_0()
                        .h_full()
                        .bg(self.theme.raised)
                        .invisible()
                        .group_hover("music-row", |d| d.visible())
                } else {
                    controls
                });
            }
            Item::Collection(collection) => {
                let saved = self.is_saved(&collection);
                row = row
                    .child(div().text_size(px(14.)).text_color(self.theme.muted).child(
                        if collection.id.starts_with("UC") {
                            "Artist"
                        } else {
                            "Collection"
                        },
                    ))
                    .child(
                        button(
                            ("save", index),
                            if saved { "heart-filled" } else { "plus" },
                            self.theme,
                        )
                        .on_click(cx.listener(move |view, _, _, cx| {
                            cx.stop_propagation();
                            view.save_collection(collection.clone(), cx);
                        })),
                    )
                    .child(icon("forward", 17., self.theme.text).text_color(self.theme.muted));
            }
        }
        div().w_full().px(px(inset)).child(row).into_any_element()
    }
}
