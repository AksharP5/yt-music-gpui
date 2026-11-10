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
        let page = self.page_layout(window);
        let available = page.column;
        let gap = page.card_gap();
        let width = page.shelf_width(layout);
        let content_height = if navigation {
            4. * 48. + 3. * 16.
        } else {
            match layout {
                ShelfLayout::Songs => 4. * 48. + 3. * 16.,
                ShelfLayout::Videos => width * 9. / 16. + page.card_text_height(),
                ShelfLayout::Cards | ShelfLayout::Featured => width + page.card_text_height(),
            }
        };
        let rows = if navigation || layout == ShelfLayout::Songs {
            4
        } else {
            1
        };
        let count = shelf.items.len().div_ceil(rows);
        let scroll = self.shelf_scroll.entry(index).or_default().clone();
        let range = visible_cards(count, (-scroll.offset().x).into(), available, width + gap);
        let stride = width + gap;
        let heading_size = page.heading_size();
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
                    .gap(px(if navigation || layout == ShelfLayout::Songs {
                        16.
                    } else {
                        0.
                    }))
                    .children(
                        items
                            .into_iter()
                            .enumerate()
                            .map(|(row, item)| {
                                self.card(
                                    item,
                                    start + column * rows + row,
                                    width,
                                    layout,
                                    page,
                                    cx,
                                )
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
                    .w(px((count as f32 * stride - gap).max(0.)))
                    .h(px(content_height))
                    .child(
                        div()
                            .absolute()
                            .left(px(range.start as f32 * stride))
                            .flex()
                            .gap(px(gap))
                            .children(columns),
                    ),
            );
        let header_height = (heading_size * 1.2).max(36.);
        let previous = scroll.clone();
        let next = scroll.clone();
        div()
            .id(("shelf", index))
            .w_full()
            .min_w_0()
            .h(px(32. + header_height + 16. + content_height + 48.))
            .px(px(inset))
            .pt(px(32.))
            .flex()
            .flex_col()
            .gap(px(16.))
            .child(
                div()
                    .h(px(header_height))
                    .flex()
                    .items_end()
                    .justify_between()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(heading_size))
                            .line_height(relative(1.2))
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
                                    .tab_stop(scroll.offset().x < px(-1.))
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
                                    .tab_stop(
                                        -f32::from(scroll.offset().x) + available + 1.
                                            < count as f32 * stride - gap,
                                    )
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
