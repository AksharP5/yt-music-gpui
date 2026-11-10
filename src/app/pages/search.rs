//! Search scopes, category filters, and featured results.

use super::super::*;

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
            120.,
            if artist {
                ArtworkShape::Circle
            } else {
                ArtworkShape::Square
            },
        );
        let title = item.title().to_owned();
        let subtitle = item.subtitle().to_owned();
        div()
            .id(("featured-result", shelf))
            .w_full()
            .px(px(inset))
            .py(px(20.))
            .flex()
            .items_center()
            .gap(px(24.))
            .child(
                div()
                    .flex_shrink_0()
                    .when(artist, |d| d.rounded_full().overflow_hidden())
                    .child(art),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .child(
                        div()
                            .id("featured-title")
                            .truncate()
                            .text_size(px(28.))
                            .font_weight(FontWeight::BOLD)
                            .cursor_pointer()
                            .child(title)
                            .on_click(cx.listener(move |view, _, _, cx| view.play_item(index, cx))),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_size(px(16.))
                            .text_color(self.theme.muted)
                            .child(subtitle),
                    )
                    .when(matches!(item, Item::Track(_)), |d| {
                        d.child(
                            div()
                                .id("featured-play")
                                .mt(px(8.))
                                .w(px(120.))
                                .h(px(36.))
                                .rounded_full()
                                .bg(self.theme.text)
                                .text_color(self.theme.bg)
                                .flex()
                                .items_center()
                                .justify_center()
                                .gap(px(8.))
                                .cursor_pointer()
                                .child(icon("play", 20., self.theme.bg))
                                .child("Play")
                                .on_click(
                                    cx.listener(move |view, _, _, cx| view.play_item(index, cx)),
                                ),
                        )
                    }),
            )
            .into_any_element()
    }
}

impl Music {
    pub(in crate::app) fn search_filters(
        &mut self,
        mut content: Div,
        inset: f32,
        cx: &mut Context<Self>,
    ) -> Div {
        if let Route::Search(search) = &self.route {
            if self.account.is_some() {
                content = content.child(
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
            content = content.child(
                div()
                    .id("search-filters")
                    .mx(px(inset))
                    .mb(px(24.))
                    .flex_shrink_0()
                    .flex()
                    .gap(px(12.))
                    .overflow_x_scroll()
                    .children(
                        search
                            .scope
                            .supported_filters()
                            .iter()
                            .copied()
                            .enumerate()
                            .map(|(index, filter)| {
                                let next = Search {
                                    filter,
                                    ..search.clone()
                                };
                                filter_chip(
                                    ("search-filter", index),
                                    filter.label(),
                                    filter == search.filter,
                                    self.theme,
                                )
                                .on_click(cx.listener(
                                    move |view, _, _, cx| {
                                        view.navigate(Route::Search(next.clone()), cx)
                                    },
                                ))
                            }),
                    ),
            );
        }
        content
    }
}
