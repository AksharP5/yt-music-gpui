//! Virtualized shelves, carousel navigation, and discovery links.

use super::super::*;
use super::PageLayout;
use super::Tier;

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
        let album = matches!(&self.route, Route::Browse(link) if link.id.starts_with("MPRE"));
        let available = if album && page.column + 2. * page.inset >= 980. {
            page.collection_columns().1
        } else {
            page.column
        };
        let (width, gap) = if album && layout == ShelfLayout::SmallCards {
            (160., if page.tier >= Tier::Large { 24. } else { 16. })
        } else if album && layout == ShelfLayout::Cards {
            let cards = if page.tier >= Tier::Large { 5. } else { 4. };
            ((available - (cards - 1.) * 24.) / cards, 24.)
        } else {
            (page.shelf_width(layout), page.card_gap())
        };
        let content_height = if navigation {
            4. * 48. + 3. * 16.
        } else {
            match layout {
                ShelfLayout::Songs => 4. * 48. + 3. * 16.,
                ShelfLayout::Episodes => 3. * EPISODE_ROW + 2. * 16.,
                ShelfLayout::Videos => width * 9. / 16. + page.card_text_height(),
                ShelfLayout::Cards if album => width + 19. + 3. * page.text_size() * 1.2,
                ShelfLayout::Cards | ShelfLayout::SmallCards | ShelfLayout::Featured => {
                    width + page.card_text_height()
                }
            }
        };
        let rows = match layout {
            _ if navigation => 4,
            ShelfLayout::Songs => 4,
            ShelfLayout::Episodes => 3,
            _ => 1,
        };
        let count = shelf.items.len().div_ceil(rows);
        let scroll = self.shelf_scroll.entry(index).or_default().clone();
        let range = visible_cards(count, (-scroll.offset().x).into(), available, width + gap);
        let stride = width + gap;
        let overflows = count as f32 * stride - gap > available + 1.;
        // The web scales shelf titles with the window on Home, Explore and
        // album pages; artist titles stay small and playlists stop at 28px.
        let heading_size = match &self.route {
            route if route.is_artist() => {
                if page.tier >= Tier::Wide {
                    24.
                } else {
                    20.
                }
            }
            Route::Browse(_) if self.page.header.is_some() && !album => {
                page.heading_size().min(28.)
            }
            _ => page.heading_size(),
        };
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
                    .gap(px(if rows > 1 { 16. } else { 0. }))
                    .children(
                        items
                            .into_iter()
                            .enumerate()
                            .map(|(row, item)| {
                                let id = start + column * rows + row;
                                if layout == ShelfLayout::Episodes {
                                    return self.episode_row(item, id, width, page, cx);
                                }
                                self.card(item, id, width, layout, page, cx)
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
                                    .border_color(rgb(0x353535))
                                    .flex()
                                    .items_center()
                                    .cursor_pointer()
                                    .child(link.title.clone())
                                    .on_click(cx.listener(move |view, _, _, cx| {
                                        view.navigate(Route::Browse(link.clone()), cx)
                                    }))
                            }))
                            .when(overflows, |controls| {
                                controls.child(
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
                            })
                            .when(overflows, |controls| {
                                controls.child(
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
                                                (offset.x - px(stride * 3.))
                                                    .max(-next.max_offset().x),
                                                offset.y,
                                            ));
                                            cx.notify();
                                        })),
                                )
                            }),
                    ),
            )
            .child(contents)
            .into_any_element()
    }
}

/// Explore's episode rows: a 120x68 frame beside three lines of text.
const EPISODE_ROW: f32 = 120.;

impl Music {
    fn episode_row(
        &mut self,
        item: Item,
        id: usize,
        width: f32,
        page: PageLayout,
        cx: &Context<Self>,
    ) -> AnyElement {
        let Item::Track(track) = item else {
            return self.card(item, id, width, ShelfLayout::Cards, page, cx);
        };
        let stats = [
            track.details.byline.clone().unwrap_or_default(),
            track
                .details
                .episode
                .as_ref()
                .map(|episode| episode.duration_text.clone())
                .unwrap_or_default(),
        ]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" • ");
        let image = track
            .thumbnail
            .as_deref()
            .and_then(|url| self.artwork.get(url));
        let frame = div()
            .relative()
            .w(px(120.))
            .h(px(68.))
            .flex_shrink_0()
            .rounded(px(4.))
            .overflow_hidden()
            .bg(self.theme.raised)
            .when_some(image, |frame, image| {
                frame.child(
                    img(image)
                        .size_full()
                        .rounded(px(4.))
                        .object_fit(ObjectFit::Cover),
                )
            })
            .child(
                div()
                    .invisible()
                    .group_hover("episode-row", |style| style.visible())
                    .absolute()
                    .inset_0()
                    .rounded(px(4.))
                    .bg(gpui::black().opacity(0.8))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(icon("play", 24., self.theme.text)),
            );
        div()
            .id(("episode-row", id))
            .keyboard_control(self.theme)
            .group("episode-row")
            .w(px(width))
            .h(px(EPISODE_ROW))
            .px(px(8.))
            .flex()
            .items_center()
            .gap(px(16.))
            .rounded(px(4.))
            .cursor_pointer()
            .hover(|row| row.bg(self.theme.raised))
            .child(frame)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .text_size(px(14.))
                    .line_height(px(16.8))
                    .child(div().truncate().text_color(self.theme.muted).child(stats))
                    .child(
                        div()
                            .font_weight(FontWeight::MEDIUM)
                            .line_clamp(2)
                            .text_ellipsis()
                            .child(track.title.clone()),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_color(self.theme.muted)
                            .child(track.artist.clone()),
                    ),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |view, _, window, cx| {
                    cx.stop_propagation();
                    view.open_item_menu(id, window, cx);
                }),
            )
            .on_click(cx.listener(move |view, _, window, cx| {
                window.focus(&view.focus, cx);
                view.play_item(id, cx);
            }))
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
