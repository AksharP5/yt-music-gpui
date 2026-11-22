//! Virtualized shelves, carousel navigation, and discovery links.

use super::super::*;
use super::PageLayout;
use super::Tier;

#[derive(Clone, Copy, PartialEq)]
struct ShelfSize {
    width: f32,
    text_size: f32,
    subtitle_lines: usize,
    video: bool,
}

pub(in crate::app) struct ShelfHeight {
    size: ShelfSize,
    height: f32,
}

fn measure_shelf_height(shelf: &Shelf, size: ShelfSize, window: &Window) -> f32 {
    let line_height = size.text_size * 1.2;
    let text_height = |text: &str, width: f32, weight, clamp| {
        let run = TextRun {
            len: text.len(),
            font: Font {
                weight,
                ..font("Roboto")
            },
            ..Default::default()
        };
        match window.text_system().shape_text(
            text.to_owned().into(),
            px(size.text_size),
            &[run],
            Some(px(width)),
            Some(clamp),
        ) {
            Ok(lines) => lines
                .iter()
                .map(|line| f32::from(line.size(px(line_height)).height))
                .sum::<f32>(),
            Err(error) => {
                eprintln!("Could not measure shelf text: {error:#}");
                line_height * clamp as f32
            }
        }
    };
    shelf
        .items
        .iter()
        .map(|item| {
            let explicit = match item {
                Item::Track(track) => track.details.explicit,
                Item::Collection(collection) => collection.details.explicit,
                Item::Navigation(_) => false,
            };
            let title = text_height(item.title(), size.width, FontWeight::MEDIUM, 2);
            let subtitle = text_height(
                item.card_subtitle(),
                size.width - if explicit { 20. } else { 0. },
                FontWeight::NORMAL,
                size.subtitle_lines,
            );
            let art = if size.video {
                size.width * 9. / 16.
            } else {
                size.width
            };
            art + 19. + title + subtitle
        })
        .fold(0., f32::max)
}

impl Music {
    pub(in crate::app) fn display_shelf(&self, index: usize) -> Option<&Shelf> {
        self.page.shelves.get(index).or_else(|| {
            self.suggestions
                .as_ref()?
                .related
                .get(index.checked_sub(self.page.shelves.len())?)
        })
    }
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
            .hover(|d| d.text_color(self.theme.accent).bg(self.theme.raised))
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
        #[cfg(test)]
        let started = Instant::now();
        let Some(shelf) = self.display_shelf(index) else {
            return div().into_any_element();
        };
        if shelf.layout == ShelfLayout::Grid {
            return self.grid_shelf(index, inset, window, cx);
        }
        let title = shelf.title.clone();
        let more = shelf.more.clone();
        let layout = shelf.layout;
        let start = self.first_item(index);
        let navigation = shelf
            .items
            .iter()
            .all(|item| matches!(item, Item::Navigation(_)));
        let page = self.page_layout(window);
        let album = matches!(&self.route, Route::Browse(link) if link.id.starts_with("MPRE"));
        let related = index >= self.page.shelves.len();
        let available = if (album || related) && page.tier >= Tier::Wide {
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
        let rows = match layout {
            _ if navigation => 4,
            ShelfLayout::Songs => 4,
            ShelfLayout::Episodes => 3,
            _ => 1,
        };
        // Include offscreen text when sizing the carousel so its height stays
        // steady when a longer title scrolls out of the rendered columns.
        let count = shelf.items.len().div_ceil(rows);
        let card_height = if rows == 1 {
            let size = ShelfSize {
                width,
                text_size: page.text_size(),
                subtitle_lines: if album && layout == ShelfLayout::Cards {
                    1
                } else {
                    2
                },
                video: layout == ShelfLayout::Videos,
            };
            if let Some(cached) = self.shelf_heights.get(index).and_then(Option::as_ref)
                && cached.size == size
            {
                cached.height
            } else {
                let height = measure_shelf_height(shelf, size, window);
                if self.shelf_heights.len() <= index {
                    self.shelf_heights.resize_with(index + 1, || None);
                }
                self.shelf_heights[index] = Some(ShelfHeight { size, height });
                height
            }
        } else {
            0.
        };
        let scroll = if let Some(index) = index.checked_sub(self.page.shelves.len())
            && let Some(suggestions) = &mut self.suggestions
        {
            suggestions.related_scroll.entry(index).or_default().clone()
        } else {
            self.shelf_scroll.entry(index).or_default().clone()
        };
        // Below 936px the web lets cards scroll under the page margins, out to the
        // window's edge, so four whole cards fit at 900px.
        let lead = if page.tier == Tier::Narrow && !related {
            inset
        } else {
            0.
        };
        let viewport = available + 2. * lead;
        let stride = width + gap;
        let row_width = (count as f32 * stride - gap).max(0.) + 2. * lead;
        let offset = -f32::from(scroll.offset().x);
        let range = visible_cards(count, offset - lead, viewport, stride);
        let overflows = count as f32 * stride - gap > available + 1.;
        let at_start = offset < 1.;
        let at_end = offset + viewport + 1. >= row_width;
        let page_step = px(carousel_page(viewport, width, gap));
        let end = px((row_width - viewport).max(0.));
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
                let items = self
                    .display_shelf(index)
                    .into_iter()
                    .flat_map(|shelf| &shelf.items)
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
            .w(px(viewport + 8.))
            .min_w_0()
            .p(px(4.))
            .m(px(-4.))
            .ml(px(-4. - lead))
            .flex()
            .overflow_x_scroll()
            .restrict_scroll_to_axis()
            .track_scroll(&scroll)
            .child(
                div()
                    .flex()
                    .gap(px(gap))
                    .flex_shrink_0()
                    .w(px(row_width))
                    .px(px(lead))
                    .min_h(px(card_height))
                    .when(range.start > 0, |row| {
                        row.child(
                            div()
                                .w(px(range.start as f32 * stride - gap))
                                .flex_shrink_0(),
                        )
                    })
                    .children(columns),
            );
        let header_height = (heading_size * 1.2).max(36.);
        let previous = scroll.clone();
        let next = scroll.clone();
        let element = div()
            .id(("shelf", index))
            .w_full()
            .min_w_0()
            .px(px(inset))
            .pt(px(if page.tier < Tier::Wide { 16. } else { 32. }))
            .pb(px(48.))
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
                                    .mr(px(8.))
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
                                        .tab_stop(!at_start)
                                        .border_1()
                                        .border_color(self.theme.border)
                                        .opacity(if at_start { 0.35 } else { 1. })
                                        .on_click(cx.listener(move |_, _, _, cx| {
                                            let offset = previous.offset();
                                            previous.set_offset(point(
                                                (offset.x + page_step).min(px(0.)),
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
                                        .tab_stop(!at_end)
                                        .border_1()
                                        .border_color(self.theme.border)
                                        .opacity(if at_end { 0.35 } else { 1. })
                                        .on_click(cx.listener(move |_, _, _, cx| {
                                            let offset = next.offset();
                                            next.set_offset(point(
                                                (offset.x - page_step).max(-end),
                                                offset.y,
                                            ));
                                            cx.notify();
                                        })),
                                )
                            }),
                    ),
            )
            .child(contents)
            .into_any_element();
        #[cfg(test)]
        if index == 0 {
            self.shelf_render_times.push(started.elapsed());
        }
        element
    }
}

impl Music {
    /// The page-wide index of a shelf's first item.
    fn first_item(&self, index: usize) -> usize {
        self.page.shelves[..index.min(self.page.shelves.len())]
            .iter()
            .map(|shelf| shelf.items.len())
            .sum()
    }

    /// A shelf the web lays out in rows that wrap: Moods & genres' buttons,
    /// or the cards that open New releases.
    fn grid_shelf(
        &mut self,
        index: usize,
        inset: f32,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(shelf) = self.display_shelf(index) else {
            return div().into_any_element();
        };
        let title = shelf.title.clone();
        let items = shelf.items.clone();
        let start = self.first_item(index);
        let page = self.page_layout(window);
        let (columns, width, gap) = if items.iter().all(|item| matches!(item, Item::Navigation(_)))
        {
            let columns = page.mood_columns();
            let width = (page.column - (columns - 1) as f32 * 16.) / columns as f32;
            (columns, width, 16.)
        } else {
            let (width, gap) = (page.card_width(), page.card_gap());
            let columns = ((page.column + gap) / (width + gap)).floor().max(1.) as usize;
            (columns, width, gap)
        };
        let narrow = page.tier == Tier::Narrow;
        div()
            .id(("grid-shelf", index))
            .w_full()
            .px(px(inset))
            .pb(px(if narrow { 38. } else { 56. }))
            .when(!title.is_empty(), |grid| {
                grid.child(
                    div()
                        .mb(px(24.))
                        .text_size(px(if narrow { 20. } else { 24. }))
                        .line_height(relative(1.2))
                        .font_weight(FontWeight::BOLD)
                        .child(title),
                )
            })
            .child(
                div().flex().flex_col().gap(px(gap)).children(
                    items
                        .chunks(columns)
                        .enumerate()
                        .map(|(row, items)| {
                            div().flex().items_start().gap(px(gap)).children(
                                items
                                    .iter()
                                    .enumerate()
                                    .map(|(column, item)| {
                                        let id = start + row * columns + column;
                                        let item = item.clone();
                                        self.card(item, id, width, ShelfLayout::Cards, page, cx)
                                    })
                                    .collect::<Vec<_>>(),
                            )
                        })
                        .collect::<Vec<_>>(),
                ),
            )
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
            .and_then(|url| self.artwork.get(url, size(px(120.), px(68.))));
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

/// How far Previous and Next move a carousel: every card that fits in its
/// viewport, as the web pages its shelves.
pub(in crate::app) fn carousel_page(viewport: f32, width: f32, gap: f32) -> f32 {
    let stride = width + gap;
    // A viewport holding exactly five cards and their gaps can land a hair under
    // five strides in floating point.
    ((viewport + gap) / stride + 0.001).floor().max(1.) * stride
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
