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

impl Music {
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
        inset: f32,
        available: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let home = self.route.is_home() || matches!(self.route, Route::Explore);
        let search = matches!(self.route, Route::Search(_));
        let collection = !home && self.page.header.is_some();
        let artist = collection && self.route.is_artist();
        let stacked = collection && !artist && available >= 980.;
        let mut content = div()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .when(search, |content| content.max_w(px(1060.)).mx_auto());
        if !home && !artist {
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
            content = self.home_filters(content, inset, cx);
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
            content = self.home_content(content, inset, cx);
        } else {
            let grid = !collection
                && self
                    .rows
                    .iter()
                    .all(|item| matches!(item, Item::Collection(_)));
            let grouped =
                matches!(&self.route,Route::Search(search) if search.filter==SearchFilter::All);
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
                                view.artist_heading(inset, cx)
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
                let columns = ((available - inset * 2. + CARD_GAP) / (CARD_WIDTH + CARD_GAP))
                    .floor()
                    .max(1.) as usize;
                let card_width =
                    shelves::shelf_card_width(ShelfLayout::Cards, available - inset * 2., false);
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
                                    .h(px(card_width + 94.))
                                    .px(px(inset))
                                    .flex()
                                    .gap(px(CARD_GAP))
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
                        .px(px(inset + 6.))
                        .pt(px(40.))
                        .flex()
                        .gap(px(56.))
                        .child(
                            div()
                                .id("collection-header")
                                .w(px(280.))
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
            || self.error.is_some()
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
