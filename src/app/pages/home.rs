//! Home/Explore filters, discovery links, and feed shelves.

use super::super::*;
use super::{PageLayout, Tier};

impl Music {
    pub(in crate::app) fn home_filters(
        &mut self,
        mut content: Div,
        layout: PageLayout,
        cx: &mut Context<Self>,
    ) -> Div {
        let home = self.route.is_home() || matches!(self.route, Route::Explore);
        // The web pads its chip cloud 40px above and 8px below, 48/16 from 1800px.
        let (above, below) = if layout.window >= 1800. {
            (54., 22.)
        } else {
            (46., 14.)
        };
        if !self.page.chips.is_empty() {
            content = content.child(
                div()
                    .id("home-filters")
                    .mx(px(layout.inset))
                    .mt(px(above))
                    .mb(px(below))
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
                                let selected = choice.selected;
                                filter_chip(
                                    ("home-filter", index),
                                    choice.label.clone(),
                                    selected,
                                    self.theme,
                                )
                                .when(!selected, |chip| {
                                    chip.bg(gpui::white().opacity(0.1))
                                        .hover(|style| style.bg(gpui::white().opacity(0.2)))
                                })
                                .on_click(cx.listener(
                                    move |view, _, _, cx| {
                                        view.navigate(Route::Browse(target.clone()), cx)
                                    },
                                ))
                            }),
                    ),
            );
        } else if home {
            content = content.child(div().h(px(32.)).flex_shrink_0());
        }
        content
    }

    /// Explore's New releases, Charts, Moods & genres and Podcasts tiles.
    fn explore_tiles(&self, shelf: usize, layout: PageLayout, cx: &Context<Self>) -> AnyElement {
        let gap = if layout.tier >= Tier::Wide { 24. } else { 16. };
        let tiles = div().flex().gap(px(gap)).children(
            self.page.shelves[shelf]
                .items
                .iter()
                .enumerate()
                .filter_map(|(item, link)| {
                    let Item::Navigation(link) = link else {
                        return None;
                    };
                    let glyph = match link.id.as_str() {
                        "FEmusic_new_releases" => "new-releases",
                        "FEmusic_charts" => "trending-up",
                        "FEmusic_moods_and_genres" => "mood",
                        "FEmusic_non_music_audio" | "FEmusic_podcasts" => "podcasts",
                        _ => "explore",
                    };
                    let link = link.clone();
                    Some(
                        div()
                            .id(("discovery-link", shelf * 1000 + item))
                            .keyboard_control(self.theme)
                            .flex_grow(1.)
                            .flex_basis(px(0.))
                            .min_w_0()
                            .h(px(110.))
                            .p(px(20.))
                            .rounded(px(16.))
                            .bg(gpui::white().opacity(0.15))
                            .hover(|style| style.bg(gpui::white().opacity(0.2)))
                            .flex()
                            .flex_col()
                            .justify_between()
                            .cursor_pointer()
                            .child(icon(glyph, 26., self.theme.text))
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(20.))
                                    .line_height(px(24.))
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(link.title.clone()),
                            )
                            .on_click(cx.listener(move |view, _, window, cx| {
                                window.focus(&view.focus, cx);
                                view.navigate(Route::Browse(link.clone()), cx);
                            })),
                    )
                }),
        );
        // List items ignore their own margins, so space the grid with padding.
        div()
            .w_full()
            .px(px(layout.inset))
            .pb(px(56.))
            .child(tiles)
            .into_any_element()
    }

    pub(in crate::app) fn home_content(&mut self, mut content: Div, cx: &mut Context<Self>) -> Div {
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
        if self.route.is_home() {
            content = content.bg(linear_gradient(
                180.,
                linear_color_stop(rgb(0x14252c), 0.),
                linear_color_stop(self.theme.bg, 0.65),
            ));
        }
        content.child(
            list(
                self.detail_list.state.clone(),
                cx.processor(move |view, index: usize, window, cx| {
                    let layout = view.page_layout(window);
                    if index == 0 {
                        return view.home_filters(div(), layout, cx).into_any_element();
                    }
                    view.prefetch_page(index, view.page.shelves.len(), 2, cx);
                    let shelf = &view.page.shelves[index - 1];
                    if shelf.title.is_empty()
                        && shelf
                            .items
                            .iter()
                            .all(|item| matches!(item, Item::Navigation(_)))
                    {
                        return view.explore_tiles(index - 1, layout, cx);
                    }
                    view.shelf(index - 1, layout.inset, window, cx)
                }),
            )
            .flex_1()
            .min_h_0(),
        )
    }
}
