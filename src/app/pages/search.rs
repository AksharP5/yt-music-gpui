//! Search scopes, category filters, and featured results.

use super::super::*;

/// A top result action: the web fills the first and outlines the second.
fn result_action(
    id: &'static str,
    glyph: &'static str,
    label: &'static str,
    filled: bool,
    width: f32,
    theme: Theme,
) -> Stateful<Div> {
    let ink: Hsla = if filled {
        rgb(0x0f0f0f).into()
    } else {
        rgb(0xf1f1f1).into()
    };
    div()
        .id(id)
        .keyboard_control(theme)
        .w(px(width))
        .h(px(36.))
        .flex_shrink_0()
        .rounded_full()
        .flex()
        .items_center()
        .justify_center()
        .gap(px(8.))
        .text_size(px(14.))
        .font_weight(FontWeight::MEDIUM)
        .text_color(ink)
        .cursor_pointer()
        .when(filled, |action| {
            action
                .bg(rgb(0xf1f1f1))
                .hover(|style| style.bg(rgb(0xd9d9d9)))
        })
        .when(!filled, |action| {
            action
                .border_1()
                .border_color(gpui::white().opacity(0.2))
                .hover(|style| style.bg(gpui::white().opacity(0.1)))
        })
        .child(icon(glyph, 24., ink))
        .child(label)
}

impl Music {
    pub(in crate::app) fn featured_result(
        &mut self,
        shelf: usize,
        inset: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let index = self.page.shelves[..shelf]
            .iter()
            .map(|shelf| shelf.items.len())
            .sum::<usize>();
        let item = self.rows[index].clone();
        let artist =
            matches!(&item, Item::Collection(collection) if collection.id.starts_with("UC"));
        let art = self.art(
            item.thumbnail(),
            100.,
            if artist {
                ArtworkShape::Circle
            } else {
                ArtworkShape::Square
            },
        );
        let title = item.title().to_owned();
        let subtitle = match &item {
            Item::Track(track) => track
                .details
                .byline
                .clone()
                .unwrap_or_else(|| track.artist.clone()),
            _ => item.subtitle().to_owned(),
        };
        let mut related = Vec::new();
        for offset in 1..self.page.shelves[shelf].items.len().min(4) {
            let id = index + offset;
            let item = self.rows[id].clone();
            let (byline, explicit, wide) = match &item {
                Item::Track(track) => (
                    track
                        .details
                        .byline
                        .clone()
                        .unwrap_or_else(|| track.artist.clone()),
                    track.details.explicit,
                    track.details.wide_thumbnail,
                ),
                _ => (item.subtitle().to_owned(), false, false),
            };
            let art = self.row_thumbnail(
                item.thumbnail(),
                56.,
                ArtworkShape::Square,
                wide,
                "featured-row",
            );
            related.push(
                div()
                    .id(("featured-related", id))
                    .keyboard_control(self.theme)
                    .group("featured-row")
                    .w_full()
                    .h(px(72.))
                    .px(px(16.))
                    .flex()
                    .items_center()
                    .gap(px(16.))
                    .cursor_pointer()
                    .hover(|style| style.bg(gpui::white().opacity(0.1)))
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
                                    .text_size(px(14.))
                                    .line_height(relative(1.2))
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(item.title().to_owned()),
                            )
                            .child(self.byline(explicit, byline, 14.)),
                    )
                    .child(
                        self.menu_button(("featured-menu", id), cx, move |view, window, cx| {
                            view.open_item_menu(id, window, cx);
                        })
                        .opacity(0.)
                        .group_hover("featured-row", |style| style.opacity(1.))
                        .focus_visible(|style| style.opacity(1.).border_color(self.theme.text)),
                    )
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |view, _, window, cx| {
                            cx.stop_propagation();
                            view.open_item_menu(id, window, cx);
                        }),
                    )
                    .on_click(cx.listener(move |view, _, _, cx| view.play_item(id, cx))),
            );
        }
        // The web sizes these buttons per card: albums and playlists 137px,
        // artists 125px and songs 119px.
        let mut actions = div().flex().items_center().gap(px(16.));
        match &item {
            Item::Collection(collection) => {
                let play = if artist {
                    None
                } else {
                    Some((
                        "featured-play",
                        "play",
                        "Play",
                        collection.details.play.clone(),
                    ))
                };
                let shuffle = Some((
                    "featured-shuffle",
                    "shuffle",
                    "Shuffle",
                    collection.details.shuffle.clone(),
                ));
                let mix =
                    artist.then(|| ("featured-mix", "mix", "Mix", collection.details.mix.clone()));
                // Playlists lead with Shuffle on the web; albums with Play.
                let order = if collection.id.starts_with("VL") {
                    [shuffle, play, mix]
                } else {
                    [play, shuffle, mix]
                };
                let width = if artist { 125. } else { 137. };
                for (position, (id, glyph, label, target)) in
                    order.into_iter().flatten().enumerate()
                {
                    let Some(target) = target else { continue };
                    actions = actions.child(
                        result_action(id, glyph, label, position == 0, width, self.theme).on_click(
                            cx.listener(move |view, _, window, cx| {
                                cx.stop_propagation();
                                window.focus(&view.focus, cx);
                                view.start_queue(PlaybackQuery::Playlist(target.clone()), cx);
                            }),
                        ),
                    );
                }
            }
            Item::Track(track) => {
                let track = track.clone();
                actions = actions
                    .child(
                        result_action("featured-play", "play", "Play", true, 119., self.theme)
                            .on_click(cx.listener(move |view, _, window, cx| {
                                cx.stop_propagation();
                                window.focus(&view.focus, cx);
                                view.play_item(index, cx);
                            })),
                    )
                    .child(
                        result_action(
                            "featured-save",
                            "playlist-add",
                            "Save",
                            false,
                            119.,
                            self.theme,
                        )
                        .on_click(cx.listener(
                            move |view, _, window, cx| {
                                cx.stop_propagation();
                                if view.account.is_none() {
                                    view.connect_account(window, cx);
                                    return;
                                }
                                view.playlist_picker(vec![track.clone()], cx);
                            },
                        )),
                    );
            }
            Item::Navigation(_) => {}
        }
        let text = div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(
                div()
                    .truncate()
                    .text_size(px(24.))
                    .line_height(relative(1.2))
                    .font_weight(FontWeight::BOLD)
                    .child(title),
            )
            .child(
                div()
                    .truncate()
                    .text_size(px(14.))
                    .line_height(relative(1.2))
                    .text_color(self.theme.muted)
                    .child(subtitle),
            );
        let details = details_chevron(("featured-details", shelf), self.theme).on_click(
            cx.listener(move |view, _, _, cx| {
                cx.stop_propagation();
                view.play_item(index, cx);
            }),
        );
        // Albums and playlists without related songs use the web's single
        // 132px row: art, text, then the actions and a details chevron.
        let compact = matches!(&item, Item::Collection(_)) && !artist && related.is_empty();
        let card = div()
            .id(("featured-result", shelf))
            .keyboard_control(self.theme)
            .relative()
            .flex_1()
            .min_w(px(280.))
            .pl(px(16.))
            .flex()
            .items_center()
            .gap(px(16.))
            .cursor_pointer()
            .child(art)
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |view, _, window, cx| {
                    cx.stop_propagation();
                    view.open_item_menu(index, window, cx);
                }),
            )
            .on_click(cx.listener(move |view, _, _, cx| view.play_item(index, cx)));
        let card = if compact {
            card.h(px(132.))
                .pr(px(8.))
                .child(text)
                .child(actions)
                .child(details)
        } else {
            let corner = if matches!(&item, Item::Track(_)) {
                self.menu_button(("featured-menu", index), cx, move |view, window, cx| {
                    view.open_item_menu(index, window, cx);
                })
                .into_any_element()
            } else {
                details.into_any_element()
            };
            card.h(px(232.))
                .pr(px(16.))
                .child(text.child(actions.mt(px(12.))))
                .child(div().absolute().top(px(4.)).right(px(8.)).child(corner))
        };
        let panel = div()
            .w_full()
            .rounded(px(8.))
            .overflow_hidden()
            .bg(linear_gradient(
                180.,
                linear_color_stop(rgb(0x292929), 0.),
                linear_color_stop(rgb(0x101010), 1.),
            ))
            .flex()
            .flex_wrap()
            .child(card)
            .when(!related.is_empty(), |body| {
                body.child(div().flex_1().min_w(px(280.)).py(px(8.)).children(related))
            });
        div()
            .w_full()
            .px(px(inset))
            .pb(px(32.))
            .child(panel)
            .into_any_element()
    }
}

/// The 24px chevron the web puts on a top result to open its page.
fn details_chevron(id: impl Into<ElementId>, theme: Theme) -> Stateful<Div> {
    div()
        .id(id)
        .keyboard_control(theme)
        .size(px(24.))
        .flex_shrink_0()
        .rounded_full()
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .hover(|style| style.bg(gpui::white().opacity(0.1)))
        .child(icon("forward", 20., rgb(0xf1f1f1).into()))
}

impl Music {
    /// The signed-in YT Music and Library tabs, which stay pinned on the web.
    pub(in crate::app) fn search_scopes(
        &mut self,
        mut content: Div,
        inset: f32,
        cx: &mut Context<Self>,
    ) -> Div {
        if let Route::Search(search) = &self.route
            && self.account.is_some()
        {
            content =
                content.child(
                    div()
                        .id("search-scopes")
                        .mx(px(inset))
                        .mb(px(24.))
                        .flex()
                        .gap(px(32.))
                        .children(SearchScope::ALL.into_iter().enumerate().map(
                            |(index, scope)| {
                                let mut next = search.clone();
                                next.scope = scope;
                                if !scope.supports(next.filter) {
                                    next.filter = SearchFilter::All;
                                }
                                div()
                                    .id(("search-scope", index))
                                    .keyboard_control(self.theme)
                                    .pb(px(12.))
                                    .border_b_2()
                                    .border_color(if scope == search.scope {
                                        self.theme.text
                                    } else {
                                        gpui::transparent_black()
                                    })
                                    .text_color(if scope == search.scope {
                                        self.theme.text
                                    } else {
                                        self.theme.muted
                                    })
                                    .cursor_pointer()
                                    .child(scope.label())
                                    .on_click(cx.listener(move |view, _, _, cx| {
                                        view.navigate(Route::Search(next.clone()), cx)
                                    }))
                            },
                        )),
                );
        }
        content
    }

    /// The category chips, which scroll with the results on the web.
    pub(in crate::app) fn search_filters(
        &mut self,
        mut content: Div,
        inset: f32,
        cx: &mut Context<Self>,
    ) -> Div {
        if let Route::Search(search) = &self.route {
            let filters = if self.page.search_filters.is_empty() {
                search.scope.supported_filters()
            } else {
                self.page.search_filters.as_slice()
            }
            .iter()
            .copied()
            .filter(|filter| *filter != SearchFilter::All)
            .collect::<Vec<_>>();
            let clear = Search {
                filter: SearchFilter::All,
                ..search.clone()
            };
            content = content.child(
                div()
                    .id("search-filters")
                    .mx(px(inset))
                    .mt(px(23.))
                    .mb(px(6.))
                    .flex_shrink_0()
                    .flex()
                    .gap(px(12.))
                    .overflow_x_scroll()
                    .restrict_scroll_to_axis()
                    .when(search.filter != SearchFilter::All, |chips| {
                        chips.child(
                            search_chip("search-filter-clear", None, false, self.theme).on_click(
                                cx.listener(move |view, _, _, cx| {
                                    view.navigate(Route::Search(clear.clone()), cx)
                                }),
                            ),
                        )
                    })
                    .children(filters.into_iter().enumerate().map(|(index, filter)| {
                        let selected = filter == search.filter;
                        let next = Search {
                            filter: if selected { SearchFilter::All } else { filter },
                            ..search.clone()
                        };
                        search_chip(
                            ("search-filter", index),
                            Some(filter.label().into()),
                            selected,
                            self.theme,
                        )
                        .on_click(cx.listener(move |view, _, _, cx| {
                            view.navigate(Route::Search(next.clone()), cx)
                        }))
                    })),
            );
        }
        content
    }
}

/// The web's 32px search and library chip; with no label it is the clear chip.
pub(in crate::app) fn search_chip(
    id: impl Into<ElementId>,
    label: Option<SharedString>,
    selected: bool,
    theme: Theme,
) -> Stateful<Div> {
    div()
        .id(id)
        .keyboard_control(theme)
        .h(px(32.))
        .px(px(if label.is_some() { 12. } else { 6. }))
        .flex_shrink_0()
        .rounded(px(8.))
        .flex()
        .items_center()
        .text_size(px(14.))
        .cursor_pointer()
        .when(selected || label.is_none(), |chip| {
            chip.bg(theme.text).text_color(theme.bg)
        })
        .when(!selected && label.is_some(), |chip| {
            chip.bg(gpui::white().opacity(0.1))
                .hover(|style| style.bg(gpui::white().opacity(0.2)))
        })
        .map(|chip| match label {
            Some(label) => chip.child(label),
            None => chip.child(icon("close", 20., theme.bg)),
        })
}
