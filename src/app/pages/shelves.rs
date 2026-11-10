//! Virtualized shelves, carousel navigation, and discovery links.

use super::super::*;

impl Music {
    pub(in crate::app) fn navigation(
        &self,
        link: BrowseLink,
        id: impl Into<ElementId>,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        div()
            .id(id)
            .keyboard_control(self.theme)
            .flex()
            .items_center()
            .gap(px(8.))
            .text_size(px(15.))
            .cursor_pointer()
            .hover(|d| d.text_color(self.theme.accent))
            .child(div().flex_1().child(link.title.clone()))
            .child(icon("forward", 17., self.theme.muted))
            .on_click(cx.listener(move |view, _, window, cx| {
                window.focus(&view.focus, cx);
                view.navigate(Route::Browse(link.clone()), cx);
            }))
    }

    pub(in crate::app) fn shelf(
        &mut self,
        index: usize,
        inset: f32,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let shelf = &self.page.shelves[index];
        let title = shelf.title.clone();
        let more = shelf.more.clone();
        let layout = shelf.layout;
        let start = self.page.shelves[..index]
            .iter()
            .map(|shelf| shelf.items.len())
            .sum::<usize>();
        let navigation = shelf
            .items
            .iter()
            .all(|item| matches!(item, Item::Navigation(_)));
        let available = f32::from(window.viewport_size().width)
            - if self.sidebar_collapsed || window.viewport_size().width < px(1150.) {
                72.
            } else {
                240.
            }
            - inset * 2.;
        let width = shelf_card_width(layout, available, navigation);
        let content_height = if navigation {
            4. * 56.
        } else {
            match layout {
                ShelfLayout::Songs => 4. * 64.,
                ShelfLayout::Videos => width * 9. / 16. + 72.,
                ShelfLayout::Cards | ShelfLayout::Featured => width + 72.,
            }
        };
        let rows = if navigation || layout == ShelfLayout::Songs {
            4
        } else {
            1
        };
        let count = shelf.items.len().div_ceil(rows);
        let scroll = self.shelf_scroll.entry(index).or_default().clone();
        let range = visible_cards(
            count,
            (-scroll.offset().x).into(),
            available,
            width + CARD_GAP,
        );
        let stride = width + CARD_GAP;
        let columns = range
            .clone()
            .map(|column| {
                let items = self.page.shelves[index]
                    .items
                    .iter()
                    .skip(column * rows)
                    .take(rows)
                    .cloned()
                    .collect::<Vec<_>>();
                div()
                    .w(px(width))
                    .flex_shrink_0()
                    .flex()
                    .flex_col()
                    .gap(px(if navigation { 12. } else { 0. }))
                    .children(
                        items
                            .into_iter()
                            .enumerate()
                            .map(|(row, item)| {
                                self.card(item, start + column * rows + row, width, layout, cx)
                            })
                            .collect::<Vec<_>>(),
                    )
            })
            .collect::<Vec<_>>();
        let contents = div()
            .id(("carousel", index))
            .w_full()
            .min_w_0()
            .flex()
            .overflow_x_scroll()
            .restrict_scroll_to_axis()
            .track_scroll(&scroll)
            .child(
                div()
                    .relative()
                    .flex_shrink_0()
                    .w(px((count as f32 * stride - CARD_GAP).max(0.)))
                    .h(px(content_height))
                    .child(
                        div()
                            .absolute()
                            .left(px(range.start as f32 * stride))
                            .flex()
                            .gap(px(CARD_GAP))
                            .children(columns),
                    ),
            );
        let previous = scroll.clone();
        let next = scroll.clone();
        div()
            .id(("shelf", index))
            .w_full()
            .min_w_0()
            .h(px(content_height + 108.))
            .px(px(inset))
            .pt(px(8.))
            .flex()
            .flex_col()
            .gap(px(20.))
            .child(
                div()
                    .h(px(40.))
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(32.))
                            .font_weight(FontWeight::BOLD)
                            .child(title),
                    )
                    .child(
                        div()
                            .flex()
                            .gap(px(16.))
                            .children(more.map(|link| {
                                div()
                                    .id(("carousel-more", index))
                                    .keyboard_control(self.theme)
                                    .h(px(36.))
                                    .px(px(16.))
                                    .rounded_full()
                                    .border_1()
                                    .border_color(self.theme.border)
                                    .flex()
                                    .items_center()
                                    .cursor_pointer()
                                    .child(link.title.clone())
                                    .on_click(cx.listener(move |view, _, _, cx| {
                                        view.navigate(Route::Browse(link.clone()), cx)
                                    }))
                            }))
                            .child(
                                button(("shelf-previous", index), "back", self.theme)
                                    .size(px(36.))
                                    .border_1()
                                    .border_color(self.theme.border)
                                    .opacity(if scroll.offset().x < px(-1.) {
                                        1.
                                    } else {
                                        0.35
                                    })
                                    .on_click(cx.listener(move |_, _, _, cx| {
                                        let offset = previous.offset();
                                        previous.set_offset(point(
                                            (offset.x + px(stride * 3.)).min(px(0.)),
                                            offset.y,
                                        ));
                                        cx.notify();
                                    })),
                            )
                            .child(
                                button(("shelf-next", index), "forward", self.theme)
                                    .size(px(36.))
                                    .border_1()
                                    .border_color(self.theme.border)
                                    .on_click(cx.listener(move |_, _, _, cx| {
                                        let offset = next.offset();
                                        next.set_offset(point(
                                            (offset.x - px(stride * 3.)).max(-next.max_offset().x),
                                            offset.y,
                                        ));
                                        cx.notify();
                                    })),
                            ),
                    ),
            )
            .child(contents)
            .into_any_element()
    }
}

pub(in crate::app) fn visible_cards(
    count: usize,
    offset: f32,
    viewport: f32,
    stride: f32,
) -> std::ops::Range<usize> {
    let offset = offset
        .max(0.)
        .min((count as f32 * stride - CARD_GAP - viewport).max(0.));
    let start = (offset / stride) as usize;
    let end = ((offset + viewport) / stride).ceil() as usize + 1;
    start.min(count)..end.min(count)
}

pub(in crate::app) fn shelf_card_width(
    layout: ShelfLayout,
    available: f32,
    navigation: bool,
) -> f32 {
    let target = if navigation {
        240.
    } else {
        match layout {
            ShelfLayout::Songs => 360.,
            ShelfLayout::Videos => 256.,
            ShelfLayout::Cards | ShelfLayout::Featured => CARD_WIDTH,
        }
    };
    let columns = ((available + CARD_GAP) / (target + CARD_GAP))
        .floor()
        .max(1.);
    ((available + CARD_GAP) / columns - CARD_GAP).max(120.)
}
