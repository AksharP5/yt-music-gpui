//! Search scopes, category filters, and featured results.

use super::super::*;
use super::artist::soft_edges;
use crate::catalog::CardAction;
use crate::search::{Correction, FilterChip};

/// A top result action: the web fills the first and outlines the second.
fn result_action(
    id: &'static str,
    glyph: &'static str,
    label: &'static str,
    filled: bool,
    size: Size<Pixels>,
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
        .w(size.width)
        .h(size.height)
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
        width: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let index = self.page.shelves[..shelf]
            .iter()
            .map(|shelf| shelf.items.len())
            .sum::<usize>();
        let stacked = width < 800.;
        let item = self.rows[index].clone();
        let artist =
            matches!(&item, Item::Collection(collection) if collection.id.starts_with("UC"));
        // The web fits a video's 16:9 still inside the square, as rows do.
        let art = match &item {
            Item::Track(track) if track.details.wide_thumbnail => self.row_thumbnail(
                item.thumbnail(),
                100.,
                ArtworkShape::Square,
                true,
                "featured-result",
            ),
            _ => self.art(
                item.thumbnail(),
                100.,
                if artist {
                    ArtworkShape::Circle
                } else {
                    ArtworkShape::Square
                },
            ),
        };
        let title = item.title().to_owned();
        let subtitle = match &item {
            Item::Track(track) => track
                .details
                .byline
                .clone()
                .unwrap_or_else(|| track.artist.clone()),
            _ => item.subtitle().to_owned(),
        };
        let heading = self.page.shelves[shelf].title.clone();
        let mut related = Vec::new();
        for offset in 1..self.page.shelves[shelf].items.len().min(4) {
            let id = index + offset;
            let item = self.rows[id].clone();
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
                    .relative()
                    .w_full()
                    .h(px(72.))
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
                            .mr(px(14.))
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
                        // The web floats the menu over the row's end, past the
                        // padding, so titles run to 16px from the row's edge.
                        .absolute()
                        .top(px(18.))
                        .right(px(-18.))
                        .opacity(0.)
                        .group_hover("featured-row", |style| style.opacity(1.))
                        .keyboard_focus_style(self.theme, |style| style.opacity(1.)),
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
        // Without related songs the web lays the card out as one 132px row:
        // art, text, then the actions and the end control.
        let compact = related.is_empty();
        // The web shows each card's own buttons and sizes them per layout.
        // In one row: a radio's lone Play 180px, songs and videos 101 by
        // 40px, and others 137px. Beside related songs: artists 125px, songs
        // and videos 119px, and others 137px.
        let button = match (&item, compact) {
            (Item::Track(_), true) => size(px(101.), px(40.)),
            (Item::Track(_), false) => size(px(119.), px(36.)),
            (Item::Collection(collection), true) if collection.details.card.actions.len() == 1 => {
                size(px(180.), px(36.))
            }
            (_, false) if artist => size(px(125.), px(36.)),
            _ => size(px(137.), px(36.)),
        };
        let mut actions = div().flex().items_center().gap(px(16.));
        match &item {
            Item::Collection(collection) => {
                for (position, action) in collection.details.card.actions.iter().enumerate() {
                    let (id, glyph, label, query) = match action {
                        CardAction::Play => (
                            "featured-play",
                            "play",
                            "Play",
                            PlaybackQuery::collection(collection, PlaybackOrder::Original),
                        ),
                        CardAction::Shuffle => (
                            "featured-shuffle",
                            "shuffle",
                            "Shuffle",
                            PlaybackQuery::collection(collection, PlaybackOrder::Shuffled),
                        ),
                        CardAction::Mix => {
                            ("featured-mix", "mix", "Mix", PlaybackQuery::mix(collection))
                        }
                    };
                    let Some(query) = query else { continue };
                    actions = actions.child(
                        result_action(id, glyph, label, position == 0, button, self.theme)
                            .on_click(cx.listener(move |view, _, window, cx| {
                                cx.stop_propagation();
                                window.focus(&view.focus, cx);
                                view.start_queue(query.clone(), cx);
                            })),
                    );
                }
            }
            Item::Track(track) => {
                let track = track.clone();
                actions = actions
                    .child(
                        result_action("featured-play", "play", "Play", true, button, self.theme)
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
                            button,
                            self.theme,
                        )
                        .on_click(cx.listener(
                            move |view, _, window, cx| {
                                cx.stop_propagation();
                                if view.account.is_none() {
                                    view.connect_account(window, cx);
                                    return;
                                }
                                view.playlist_picker(vec![track.clone()], window, cx);
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
        // Cards with a page end in a chevron to it; songs, videos and radios
        // end in their menu.
        let end = if matches!(&item, Item::Collection(collection) if collection.details.card.chevron)
        {
            details_chevron(("featured-details", shelf), self.theme).on_click(cx.listener(
                move |view, _, _, cx| {
                    cx.stop_propagation();
                    view.play_item(index, cx);
                },
            ))
        } else {
            self.menu_button(("featured-menu", index), cx, move |view, window, cx| {
                view.open_item_menu(index, window, cx);
            })
        };
        let card = div()
            .id(("featured-result", shelf))
            .keyboard_control(self.theme)
            .group("featured-result")
            .relative()
            .flex_1()
            .min_w(px(280.))
            .pl(px(15.))
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
            // The menu matches the buttons' height, 8px past the last one.
            card.h(px(132.))
                .pr(px(7.))
                .child(text)
                .child(actions)
                .child(end.max_w(button.height).max_h(button.height).ml(px(-8.)))
        } else {
            // Beside related songs the card grows with them, as it does when
            // the web heads them "More from YouTube".
            card.when(stacked, |card| card.h(px(164.)))
                .when(!stacked, |card| card.min_h(px(232.)))
                .pr(px(15.))
                .child(text.child(actions.mt(px(4.))))
                .child(div().absolute().top(px(4.)).right(px(8.)).child(end))
        };
        // The web lays the result over its own art, blurred by 60px at 70%
        // under a gradient to black, then lightens the card by 10% and the
        // related songs by 5%. Both halves pad 16px, so they split evenly.
        let backdrop = item.thumbnail().and_then(|url| self.artwork.get_wash(url));
        let panel = div()
            .relative()
            .w_full()
            .rounded(px(8.))
            .overflow_hidden()
            .bg(self.theme.bg)
            .when_some(backdrop, |panel, image| {
                panel
                    .child(
                        img(image)
                            .absolute()
                            .inset_0()
                            .size_full()
                            .object_fit(ObjectFit::Cover)
                            .opacity(0.7),
                    )
                    .children(soft_edges(self.theme.bg, 120.))
            })
            .child(div().absolute().inset_0().bg(linear_gradient(
                180.,
                linear_color_stop(rgba(0x0000000d), 0.),
                linear_color_stop(gpui::black(), 0.8667),
            )))
            .flex()
            .when(stacked, |panel| panel.flex_col())
            .child(
                card.bg(gpui::white().opacity(0.1))
                    .rounded(px(8.))
                    .when(!related.is_empty() && !stacked, |card| {
                        card.rounded_r(px(0.))
                    })
                    .when(stacked, |card| card.flex_none().w_full()),
            )
            .when(!related.is_empty(), |body| {
                body.child(
                    div()
                        .flex_1()
                        .when(stacked, |songs| songs.flex_none().w_full())
                        .min_w(px(280.))
                        .pl(px(15.))
                        .pr(px(17.))
                        .py(px(8.))
                        .rounded_r(px(8.))
                        .bg(gpui::white().opacity(0.05))
                        .when(!heading.is_empty(), |songs| {
                            songs.child(
                                div()
                                    .mt(px(8.))
                                    .mb(px(8.))
                                    .text_size(px(14.))
                                    .line_height(px(17.))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(gpui::white().opacity(0.7))
                                    .child(heading.to_uppercase()),
                            )
                        })
                        .children(related),
                )
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
                                let mut next = Search {
                                    scope,
                                    params: None,
                                    ..search.clone()
                                };
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
                search
                    .scope
                    .supported_filters()
                    .iter()
                    .map(|&filter| FilterChip {
                        filter,
                        params: None,
                    })
                    .collect()
            } else {
                self.page.search_filters.clone()
            }
            .into_iter()
            .filter(|chip| chip.filter != SearchFilter::All)
            .collect::<Vec<_>>();
            let clear = Search::new(search.query.clone(), search.scope);
            content = content.child(
                chip_row("search-filters")
                    .mx(px(inset))
                    .mt(px(6.))
                    .mb(px(6.))
                    .flex_shrink_0()
                    .when(search.filter != SearchFilter::All, |chips| {
                        let clear = clear.clone();
                        chips.child(
                            search_chip("search-filter-clear", None, false, self.theme).on_click(
                                cx.listener(move |view, _, _, cx| {
                                    view.navigate(Route::Search(clear.clone()), cx)
                                }),
                            ),
                        )
                    })
                    .children(filters.into_iter().enumerate().map(|(index, chip)| {
                        let selected = chip.filter == search.filter;
                        let next = if selected {
                            clear.clone()
                        } else {
                            Search {
                                filter: chip.filter,
                                params: chip.params,
                                ..search.clone()
                            }
                        };
                        search_chip(
                            ("search-filter", index),
                            Some(chip.filter.label().into()),
                            selected,
                            self.theme,
                        )
                        .on_click(cx.listener(move |view, _, _, cx| {
                            view.navigate(Route::Search(next.clone()), cx)
                        }))
                    })),
            );
            if let Some(correction) = self.page.correction.clone() {
                content = content.child(self.spelling_correction(correction, inset, cx));
            }
        }
        content
    }

    /// The web's line under the chips: "Showing results for" the corrected
    /// query with a link to the spelling as typed, or "Did you mean".
    fn spelling_correction(
        &self,
        correction: Correction,
        inset: f32,
        cx: &mut Context<Self>,
    ) -> Div {
        let label = |text: &'static str| {
            div()
                .text_size(px(12.))
                .line_height(px(18.))
                .text_color(rgb(0xf1f1f1))
                .child(text)
        };
        let link = |id: &'static str, text: StyledText, target: Search, color: Hsla| {
            div()
                .id(id)
                .keyboard_control(self.theme)
                .text_size(px(14.))
                .line_height(px(20.))
                .font_weight(FontWeight::MEDIUM)
                .text_color(color)
                .cursor_pointer()
                .child(text)
                .on_click(cx.listener(move |view, _, _, cx| {
                    view.navigate(Route::Search(target.clone()), cx)
                }))
        };
        let italic = correction.italic.iter().map(|range| {
            (
                range.clone(),
                HighlightStyle {
                    font_style: Some(FontStyle::Italic),
                    ..Default::default()
                },
            )
        });
        let corrected = link(
            "search-corrected",
            StyledText::new(correction.label.clone()).with_highlights(italic),
            correction.corrected,
            gpui::white(),
        );
        // The web separates the parts with a space at 10px, and the second
        // label with 8px more.
        let line = div().flex().items_center().gap(px(2.5));
        let line = match correction.original {
            Some(original) => line
                .child(label("Showing results for"))
                .child(corrected)
                .child(label("Search instead for").ml(px(8.)))
                .child(link(
                    "search-original",
                    StyledText::new(original.query.clone()),
                    original,
                    rgb(0xf1f1f1).into(),
                )),
            None => line.child(label("Did you mean:")).child(corrected),
        };
        div().mx(px(inset)).py(px(24.)).child(line)
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
