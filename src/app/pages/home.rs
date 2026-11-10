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
        let mut links = Vec::new();
        let mut shelves = Vec::new();
        for (index, shelf) in self.page.shelves.iter().enumerate() {
            if shelf.title.is_empty()
                && shelf
                    .items
                    .iter()
                    .all(|item| matches!(item, Item::Navigation(_)))
            {
                for (i, item) in shelf.items.iter().enumerate() {
                    if let Item::Navigation(link) = item {
                        links.push(
                            self.navigation(link.clone(), ("discovery-link", index * 1000 + i), cx)
                                .h(px(72.))
                                .px(px(24.))
                                .rounded(px(4.))
                                .bg(self.theme.raised)
                                .flex_1(),
                        );
                    }
                }
            } else {
                shelves.push(index);
            }
        }
        content = content
            .when(!links.is_empty(), |d| {
                d.child(
                    div()
                        .mx(px(inset))
                        .mb(px(36.))
                        .flex()
                        .gap(px(24.))
                        .children(links),
                )
            })
            .child(
                uniform_list(
                    ("home-shelves", self.list_id),
                    shelves.len(),
                    cx.processor(move |view, range: std::ops::Range<usize>, window, cx| {
                        range
                            .map(|i| view.shelf(shelves[i], inset, window, cx))
                            .collect::<Vec<_>>()
                    }),
                )
                .track_scroll(&self.list_scroll)
                .flex_1()
                .min_h_0(),
            );
        content
    }
}
