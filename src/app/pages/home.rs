//! Home/Explore filters, discovery links, and feed shelves.

use super::super::*;

impl Music {
    pub(in crate::app) fn home_filters(
        &mut self,
        mut content: Div,
        inset: f32,
        cx: &mut Context<Self>,
    ) -> Div {
        let home = self.route.is_home() || matches!(self.route, Route::Explore);
        if !self.page.chips.is_empty() {
            content = content.child(
                div()
                    .id("home-filters")
                    .mx(px(inset))
                    .mt(px(46.))
                    .mb(px(36.))
                    .flex_shrink_0()
                    .flex()
                    .gap(px(12.))
                    .overflow_x_scroll()
                    .restrict_scroll_to_axis()
                    .children(
                        self.page
                            .chips
                            .iter()
                            .enumerate()
                            .filter(|(_, choice)| choice.label != "All" || !choice.selected)
                            .map(|(index, choice)| {
                                let target = choice.target.clone();
                                filter_chip(
                                    ("home-filter", index),
                                    choice.label.clone(),
                                    choice.selected,
                                    self.theme,
                                )
                                .on_click(cx.listener(
                                    move |view, _, _, cx| {
                                        view.navigate(Route::Browse(target.clone()), cx)
                                    },
                                ))
                            }),
                    ),
            );
        } else if home {
            content = content.child(div().h(px(40.)).flex_shrink_0());
        }
        content
    }
}

impl Music {
    pub(in crate::app) fn home_content(
        &mut self,
        mut content: Div,
        inset: f32,
        cx: &mut Context<Self>,
    ) -> Div {
        let count = self.page.shelves.len() + 1;
        let state = &self.detail_list.state;
        match state.item_count().cmp(&count) {
            std::cmp::Ordering::Less => state.splice(
                state.item_count()..state.item_count(),
                count - state.item_count(),
            ),
            std::cmp::Ordering::Greater => state.reset(count),
            std::cmp::Ordering::Equal => {}
        }
        content = content
            .bg(linear_gradient(
                180.,
                linear_color_stop(rgb(0x14252c), 0.),
                linear_color_stop(self.theme.bg, 0.65),
            ))
            .child(
                list(
                    self.detail_list.state.clone(),
                    cx.processor(move |view, index: usize, window, cx| {
                        if index == 0 {
                            return view.home_filters(div(), inset, cx).into_any_element();
                        }
                        view.prefetch_page(index, view.page.shelves.len(), 2, cx);
                        let shelf = &view.page.shelves[index - 1];
                        if shelf.title.is_empty()
                            && shelf
                                .items
                                .iter()
                                .all(|item| matches!(item, Item::Navigation(_)))
                        {
                            return div()
                                .mx(px(inset))
                                .mb(px(36.))
                                .flex()
                                .flex_wrap()
                                .gap(px(24.))
                                .children(shelf.items.iter().enumerate().filter_map(
                                    |(item, link)| {
                                        let Item::Navigation(link) = link else {
                                            return None;
                                        };
                                        Some(
                                            view.navigation(
                                                link.clone(),
                                                ("discovery-link", (index - 1) * 1000 + item),
                                                cx,
                                            )
                                            .h(px(72.))
                                            .px(px(24.))
                                            .rounded(px(4.))
                                            .bg(view.theme.raised)
                                            .flex_1(),
                                        )
                                    },
                                ))
                                .into_any_element();
                        }
                        view.shelf(index - 1, inset, window, cx)
                    }),
                )
                .flex_1()
                .min_h_0(),
            );
        content
    }
}
