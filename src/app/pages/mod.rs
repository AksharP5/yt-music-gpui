//! Page layout, list state, and dispatch to the page-specific renderers.

pub(in crate::app) mod artist;
pub(in crate::app) mod collection;
pub(in crate::app) mod home;
pub(in crate::app) mod items;
pub(in crate::app) mod library;
pub(in crate::app) mod search;
pub(in crate::app) mod shelves;

use super::*;

pub(in crate::app) enum ListRow {
    Featured(usize),
    Heading(usize),
    Item(usize),
    Shelf(usize),
    More(usize),
}

pub(in crate::app) struct DetailList {
    pub(in crate::app) state: ListState,
    pub(in crate::app) inline_header: bool,
}
impl DetailList {
    pub(in crate::app) fn new() -> Self {
        Self {
            state: ListState::new(0, ListAlignment::Top, px(128.)),
            inline_header: false,
        }
    }
    pub(in crate::app) fn reset_rows(&self, rows: usize) {
        let top = self.state.logical_scroll_top();
        self.state.reset(rows + usize::from(self.inline_header));
        self.state.scroll_to(top);
    }
    pub(in crate::app) fn append_rows(&self, rows: usize) {
        if rows == 0 {
            return;
        }
        let top = self.state.logical_scroll_top();
        let end = self.state.item_count();
        self.state.splice(end..end, rows);
        self.state.scroll_to(top);
    }
    pub(in crate::app) fn set_inline_header(&mut self, inline: bool) {
        if self.inline_header == inline {
            return;
        }
        let top = self.state.logical_scroll_top();
        if inline {
            self.state.splice(0..0, 1);
        } else {
            self.state.splice(0..1, 0);
        }
        self.inline_header = inline;
        if top.item_ix == 0 && top.offset_in_item == px(0.) {
            self.state.scroll_to(ListOffset {
                item_ix: 0,
                offset_in_item: px(0.),
            });
        }
        self.state.remeasure();
    }
}

/// Window-width tiers from music.youtube.com's media queries. The web keys its
/// breakpoints on the whole window, not the content area beside the guide.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(in crate::app) enum Tier {
    Narrow,
    Medium,
    Wide,
    Large,
    Huge,
}

/// The centered content column every browse page shares, and the type and
/// card sizes the web derives from it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(in crate::app) struct PageLayout {
    pub(in crate::app) window: f32,
    pub(in crate::app) tier: Tier,
    pub(in crate::app) inset: f32,
    pub(in crate::app) column: f32,
}

impl PageLayout {
    pub(in crate::app) fn new(window: f32, available: f32) -> Self {
        let tier = match window {
            width if width < 936. => Tier::Narrow,
            width if width < 1150. => Tier::Medium,
            width if width < 1364. => Tier::Wide,
            width if width < 1578. => Tier::Large,
            _ => Tier::Huge,
        };
        let column = if tier >= Tier::Wide {
            (available - 212.).min(1478.)
        } else if window < 616. {
            available - 32.
        } else {
            available - 112.
        }
        .max(0.);
        Self {
            window,
            tier,
            inset: (available - column) / 2.,
            column,
        }
    }

    pub(in crate::app) fn heading_size(self) -> f32 {
        match self.tier {
            Tier::Narrow | Tier::Medium => 24.,
            Tier::Wide => 28.,
            Tier::Large => 34.,
            Tier::Huge => 45.,
        }
    }

    pub(in crate::app) fn text_size(self) -> f32 {
        if self.tier >= Tier::Large { 16. } else { 14. }
    }

    pub(in crate::app) fn card_gap(self) -> f32 {
        if self.tier >= Tier::Wide { 24. } else { 16. }
    }

    /// Square card edge: five across on wide windows, six on large ones.
    pub(in crate::app) fn card_width(self) -> f32 {
        let gap = self.card_gap();
        match self.tier {
            Tier::Narrow | Tier::Medium => 180.,
            Tier::Wide => (self.column - 4. * gap) / 5.,
            Tier::Large | Tier::Huge => (self.column - 5. * gap) / 6.,
        }
    }

    /// Quick picks column: fixed below the large breakpoint, three across above.
    pub(in crate::app) fn song_width(self) -> f32 {
        if self.tier >= Tier::Large {
            (self.column - 2. * self.card_gap()) / 3.
        } else {
            self.column.min(436.)
        }
    }

    /// Width of one carousel column for a shelf layout.
    pub(in crate::app) fn shelf_width(self, layout: ShelfLayout) -> f32 {
        match layout {
            ShelfLayout::Songs | ShelfLayout::Episodes => self.song_width(),
            ShelfLayout::Videos => self.card_width() * 16. / 9.,
            ShelfLayout::Cards | ShelfLayout::Featured => self.card_width(),
        }
    }

    /// The web narrows search results to a centered column.
    pub(in crate::app) fn search_width(self) -> f32 {
        match self.tier {
            Tier::Narrow if self.window < 616. => self.column,
            Tier::Narrow => 560.,
            Tier::Medium => 720.,
            _ => 860.,
        }
        .min(self.column + 2. * self.inset - 32.)
    }

    /// Card title and subtitle each reserve two lines, like the web's line clamp.
    pub(in crate::app) fn card_text_height(self) -> f32 {
        16. + 3. + 4. * self.text_size() * 1.2
    }
}

impl Music {
    pub(in crate::app) fn page_layout(&self, window: &Window) -> PageLayout {
        let width = f32::from(window.viewport_size().width);
        let sidebar = if self.sidebar_collapsed || width < 1150. {
            72.
        } else {
            240.
        };
        PageLayout::new(width, width - sidebar)
    }

    pub(in crate::app) fn history_groups(&self) -> bool {
        matches!(self.route, Route::Recent | Route::Library(Section::History))
    }

    pub(in crate::app) fn grouped_rows(&self) -> bool {
        self.page.header.is_some()
            || self.history_groups()
            || matches!(&self.route, Route::Search(search) if search.filter == SearchFilter::All)
    }

    pub(in crate::app) fn list_row(
        &mut self,
        index: usize,
        inset: f32,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.prefetch_page(index, self.list_rows.len(), 8, cx);
        match self.list_rows[index] {
            ListRow::Featured(index) => self.featured_result(index, inset, cx),
            ListRow::Heading(shelf) => div()
                .h(px(64.))
                .px(px(inset))
                .flex()
                .items_center()
                .text_size(px(24.))
                .when(self.history_groups(), |heading| {
                    heading.line_height(px(32.))
                })
                .font_weight(FontWeight::BOLD)
                .child(self.page.shelves[shelf].title.clone())
                .into_any_element(),
            ListRow::Item(index) => self.row(index, inset, window, cx),
            ListRow::Shelf(index) => self.shelf(index, inset, window, cx),
            ListRow::More(index) => {
                let link = self.page.shelves[index]
                    .more
                    .clone()
                    .expect("shelf navigation row");
                div()
                    .h(px(72.))
                    .px(px(inset))
                    .pt(px(8.))
                    .flex()
                    .items_start()
                    .child(
                        div()
                            .id(("shelf-more", index))
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
                                view.navigate(Route::Browse(link.clone()), cx);
                            })),
                    )
                    .into_any_element()
            }
        }
    }

    pub(in crate::app) fn content(
        &mut self,
        window: f32,
        available: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let layout = PageLayout::new(window, available);
        let search = matches!(self.route, Route::Search(_));
        let inset = if search {
            (available - layout.search_width()) / 2.
        } else {
            layout.inset
        };
        let home = self.route.is_home() || matches!(self.route, Route::Explore);
        let collection = !home && self.page.header.is_some();
        let artist = collection && self.route.is_artist();
        let stacked = collection && !artist && available >= 980.;
        let mut content = div().flex_1().min_w_0().min_h_0().flex().flex_col();
        if !home && !artist && !search {
            content = content.child(
                div()
                    .h(px(if collection { 24. } else { 48. }))
                    .px(px(inset))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .when(!self.back.is_empty(), |d| {
                        d.child(
                            button("back", "back", self.theme)
                                .on_click(cx.listener(|view, _, _, cx| view.go_back(cx))),
                        )
                    }),
            );
        }
        if !home && !search && !collection {
            let heading = self.page_heading(false, 128., cx);
            content = content.child(div().px(px(inset)).pb(px(28.)).child(heading));
        }
        if !home {
            content = self.home_filters(content, layout, cx);
        }
        content = self.search_filters(content, inset, cx);
        content = self.library_filters(content, inset, cx);
        if !matches!(self.overlay, Some(Overlay::SignIn))
            && let Some(error) = &self.error
        {
            content = content.child(
                div()
                    .mx(px(inset))
                    .mb(px(16.))
                    .flex()
                    .items_center()
                    .gap(px(16.))
                    .text_color(rgb(0xf1a6ae))
                    .text_size(px(14.))
                    .child(div().flex_1().child(error.clone()))
                    .child(
                        button("dismiss-error", "close", self.theme).on_click(cx.listener(
                            |view, _, _, cx| {
                                view.error = None;
                                view.sync.page_error = None;
                                cx.notify();
                            },
                        )),
                    ),
            );
        }
        if let Some(notice) = &self.notice {
            content = content.child(
                div()
                    .px(px(inset))
                    .pb(px(16.))
                    .text_size(px(14.))
                    .text_color(self.theme.muted)
                    .child(notice.clone()),
            );
        }
        if self.rows.is_empty()
            && (self.catalog_requests.is_loading(CatalogTarget::Page)
                || matches!(
                    self.account_operation,
                    Some(AccountOperation::Restoring(_) | AccountOperation::Activating { .. })
                ))
        {
            return content
                .child(
                    div()
                        .px(px(inset))
                        .py(px(24.))
                        .text_color(self.theme.muted)
                        .child("Loading music…"),
                )
                .into_any_element();
        }
        let heading = (collection && stacked)
            .then(|| self.page_heading(true, if available < 1200. { 240. } else { 264. }, cx));
        if self.rows.is_empty() && !collection {
            let message = match self.route {
                Route::Liked => "Music you like will appear here.",
                Route::Recent => "Your listening history will appear here.",
                Route::Saved | Route::Library(_) => "Nothing saved yet.",
                Route::Search(_) => "No results. Try a different search.",
                _ => "No music to show yet.",
            };
            return content
                .children(heading.map(|heading| div().px(px(inset)).pb(px(24.)).child(heading)))
                .child(
                    div()
                        .px(px(inset))
                        .py(px(24.))
                        .text_color(self.theme.muted)
                        .child(message),
                )
                .into_any_element();
        }
        if home {
            content = self.home_content(content, cx);
        } else {
            let grid = !collection
                && self
                    .rows
                    .iter()
                    .all(|item| matches!(item, Item::Collection(_)));
            let grouped = self.grouped_rows();
            let list = if collection && stacked && self.rows.is_empty() {
                div()
                    .flex_1()
                    .px(px(24.))
                    .py(px(32.))
                    .text_color(self.theme.muted)
                    .child("No songs here yet.")
                    .into_any_element()
            } else if collection {
                self.detail_list.set_inline_header(!stacked);
                let inline = self.detail_list.inline_header;
                list(
                    self.detail_list.state.clone(),
                    cx.processor(move |view, index: usize, window, cx| {
                        if artist {
                            return if index == 0 {
                                view.artist_heading(inset, window, cx)
                            } else {
                                view.list_row(index - 1, inset, window, cx)
                            };
                        }
                        if inline && index == 0 {
                            let heading = view.page_heading(true, 200., cx);
                            return div()
                                .w_full()
                                .px(px(inset))
                                .pb(px(32.))
                                .flex()
                                .justify_center()
                                .child(div().w_full().max_w(px(500.)).child(heading).when(
                                    view.rows.is_empty(),
                                    |d| {
                                        d.child(
                                            div()
                                                .mt(px(24.))
                                                .text_color(view.theme.muted)
                                                .child("No songs here yet."),
                                        )
                                    },
                                ))
                                .into_any_element();
                        }
                        view.list_row(
                            index - usize::from(inline),
                            if stacked { 0. } else { inset },
                            window,
                            cx,
                        )
                    }),
                )
                .flex_1()
                .min_h_0()
                .into_any_element()
            } else if grouped {
                self.detail_list.set_inline_header(false);
                list(
                    self.detail_list.state.clone(),
                    cx.processor(move |view, index, window, cx| {
                        view.list_row(index, inset, window, cx)
                    }),
                )
                .flex_1()
                .min_h_0()
                .into_any_element()
            } else if grid {
                let gap = layout.card_gap();
                let card_width = layout.card_width();
                let columns = ((layout.column + gap) / (card_width + gap)).floor().max(1.) as usize;
                let row_height = card_width + layout.card_text_height() + 24.;
                uniform_list(
                    ("collection-grid", self.list_id),
                    self.rows.len().div_ceil(columns),
                    cx.processor(move |view, range: std::ops::Range<usize>, _, cx| {
                        view.prefetch_page(range.end, view.rows.len().div_ceil(columns), 2, cx);
                        range
                            .map(|row| {
                                let items = view
                                    .rows
                                    .iter()
                                    .skip(row * columns)
                                    .take(columns)
                                    .cloned()
                                    .collect::<Vec<_>>();
                                div()
                                    .h(px(row_height))
                                    .px(px(inset))
                                    .flex()
                                    .gap(px(gap))
                                    .children(
                                        items
                                            .into_iter()
                                            .enumerate()
                                            .map(|(column, item)| {
                                                view.card(
                                                    item,
                                                    row * columns + column,
                                                    card_width,
                                                    ShelfLayout::Cards,
                                                    layout,
                                                    cx,
                                                )
                                            })
                                            .collect::<Vec<_>>(),
                                    )
                                    .into_any_element()
                            })
                            .collect::<Vec<_>>()
                    }),
                )
                .track_scroll(&self.list_scroll)
                .flex_1()
                .min_h_0()
                .into_any_element()
            } else {
                let row_inset = if stacked { 0. } else { inset };
                uniform_list(
                    ("music-results", self.list_id),
                    self.rows.len(),
                    cx.processor(move |view, range: std::ops::Range<usize>, window, cx| {
                        view.prefetch_page(range.end, view.rows.len(), 12, cx);
                        range
                            .map(|i| view.row(i, row_inset, window, cx))
                            .collect::<Vec<_>>()
                    }),
                )
                .track_scroll(&self.list_scroll)
                .flex_1()
                .min_h_0()
                .into_any_element()
            };
            content = if stacked {
                content.child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .w_full()
                        .max_w(px(1492.))
                        .mx_auto()
                        // Measured on the web at 1280: art column at 335, list
                        // 40px later, durations ending 79px from the edge.
                        .when(available < 1200., |d| {
                            d.pl(px(inset - 11.)).pr(px(inset - 27.))
                        })
                        .when(available >= 1200., |d| d.px(px(inset)))
                        .pt(px(40.))
                        .flex()
                        .gap(px(if available < 1200. { 40. } else { 72. }))
                        .child(
                            div()
                                .id("collection-header")
                                .w(px(if available < 1200. { 280. } else { 330. }))
                                .h_full()
                                .flex_shrink_0()
                                .overflow_y_scroll()
                                .children(heading),
                        )
                        .child(list),
                )
            } else {
                content
                    .children(heading.map(|heading| div().px(px(inset)).pb(px(28.)).child(heading)))
                    .child(list)
            };
        }
        if self.page.continuation.is_some() && self.catalog_requests.is_loading(CatalogTarget::Page)
        {
            content = content.child(
                div()
                    .h(px(32.))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(self.theme.muted)
                    .child("Loading…"),
            );
        }
        content.into_any_element()
    }
}

impl Music {
    pub(in crate::app) fn prefetch_page(
        &mut self,
        visible_end: usize,
        count: usize,
        ahead: usize,
        cx: &mut Context<Self>,
    ) {
        if visible_end.saturating_add(ahead) < count
            || self.catalog_requests.is_loading(CatalogTarget::Page)
            || self.sync.page_error.is_some()
        {
            return;
        }
        if let Some(token) = self.page.continuation.clone() {
            self.request(
                Query::More {
                    token,
                    search: match &self.route {
                        Route::Search(search) => Some(search.clone()),
                        _ => None,
                    },
                },
                cx,
            );
        }
    }
}
