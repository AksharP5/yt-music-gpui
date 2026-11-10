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
            96.,
            if artist {
                ArtworkShape::Circle
            } else {
                ArtworkShape::Square
            },
        );
        let title = item.title().to_owned();
        let subtitle = item.subtitle().to_owned();
        let mut related = Vec::new();
        for offset in 1..self.page.shelves[shelf].items.len().min(4) {
            let id = index + offset;
            let item = self.rows[id].clone();
            let art = self.art(item.thumbnail(), 48., ArtworkShape::Square);
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
                    .hover(|style| style.bg(self.theme.raised))
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
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(item.title().to_owned()),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(14.))
                                    .text_color(self.theme.muted)
                                    .child(item.subtitle().to_owned()),
                            ),
                    )
                    .child(
                        button(("featured-menu", id), "more", self.theme)
                            .opacity(0.)
                            .group_hover("featured-row", |style| style.opacity(1.))
                            .focus_visible(|style| style.opacity(1.).border_color(self.theme.text))
                            .on_click(cx.listener(move |view, _, window, cx| {
                                cx.stop_propagation();
                                view.open_item_menu(id, window, cx);
                            })),
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
            .child(
                div()
                    .id(("featured-result", shelf))
                    .keyboard_control(self.theme)
                    .flex_1()
                    .min_w(px(280.))
                    .h(px(232.))
                    .px(px(20.))
                    .flex()
                    .items_center()
                    .gap(px(16.))
                    .cursor_pointer()
                    .child(art)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(8.))
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(24.))
                                    .font_weight(FontWeight::BOLD)
                                    .child(title),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(14.))
                                    .text_color(self.theme.muted)
                                    .child(subtitle),
                            )
                            .child(
                                filter_chip(
                                    "featured-details",
                                    if matches!(item, Item::Track(_)) {
                                        "Play"
                                    } else {
                                        "See details"
                                    },
                                    true,
                                    self.theme,
                                )
                                .rounded_full()
                                .w(px(120.))
                                .on_click(cx.listener(
                                    move |view, _, _, cx| {
                                        cx.stop_propagation();
                                        view.play_item(index, cx);
                                    },
                                )),
                            ),
                    )
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |view, _, window, cx| {
                            cx.stop_propagation();
                            view.open_item_menu(index, window, cx);
                        }),
                    )
                    .on_click(cx.listener(move |view, _, _, cx| view.play_item(index, cx))),
            )
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
            content = content.child(
                div()
                    .id("search-filters")
                    .mx(px(inset))
                    .mb(px(24.))
                    .flex_shrink_0()
                    .flex()
                    .gap(px(12.))
                    .overflow_x_scroll()
                    .restrict_scroll_to_axis()
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
