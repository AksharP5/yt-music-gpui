//! Page layout, list state, and dispatch to the page-specific renderers.

pub(in crate::app) mod artist;
pub(in crate::app) mod collection;
pub(in crate::app) mod home;
pub(in crate::app) mod items;
pub(in crate::app) mod library;
pub(in crate::app) mod search;
pub(in crate::app) mod shelves;

use super::*;

pub(super) fn error_summary(error: &str) -> String {
    if [
        "error sending request",
        "dns error",
        "Connection refused",
        "operation timed out",
        "network is unreachable",
    ]
    .iter()
    .any(|cause| error.contains(cause))
    {
        return "Could not connect to YouTube Music. Check your connection and try again.".into();
    }
    let message = error.lines().next().unwrap_or("Could not load music");
    let message = message.split(": ").next().unwrap_or(message);
    let mut summary: String = message.chars().take(160).collect();
    if message.chars().count() > 160 {
        summary.push('…');
    }
    summary
}

struct ErrorDetails {
    text: SharedString,
    theme: Theme,
}

impl Render for ErrorDetails {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .p(px(12.))
            .max_w(px(480.))
            .max_h(px(280.))
            .overflow_hidden()
            .bg(self.theme.raised)
            .text_color(self.theme.text)
            .text_size(px(12.))
            .whitespace_normal()
            .child(self.text.clone())
    }
}

pub(in crate::app) enum ListRow {
    Featured(usize),
    Heading(usize),
    Item(usize),
    Shelf(usize),
    More(usize),
}

/// Space above stacked album and playlist columns: the old back-button row
/// plus the web's 40px top padding.
const STACKED_TOP: f32 = 64.;

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
            // 112px page margins plus the web's 12px scrollbar reservation.
            available - 124.
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
            ShelfLayout::Cards | ShelfLayout::SmallCards | ShelfLayout::Featured => {
                self.card_width()
            }
        }
    }

    /// Art, track column and cover widths shared by collection headers and shelves.
    fn collection_columns(self) -> (f32, f32, f32) {
        let (header, tracks, cover): (f32, f32, f32) = match self.tier {
            Tier::Huge => (333., 921., 264.),
            Tier::Large => (261., 779., 245.),
            _ => (280., 546., 240.),
        };
        (
            header,
            tracks.min(self.column + 2. * self.inset - header - 56.),
            cover,
        )
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

    /// The library's small-card grid: columns, gap, card edge, and the
    /// layout its cards use. The web keeps these cards' text at 14px.
    pub(in crate::app) fn small_grid(self) -> (usize, f32, f32, Self) {
        let (columns, gap) = match self.tier {
            Tier::Narrow | Tier::Medium => (5, 16.),
            Tier::Wide => (6, 16.),
            Tier::Large => (7, 24.),
            Tier::Huge if self.window < 1792. => (8, 24.),
            Tier::Huge => (9, 24.),
        };
        let width = (self.column - (columns - 1) as f32 * gap) / columns as f32;
        let cards = Self {
            tier: self.tier.min(Tier::Wide),
            ..self
        };
        (columns, gap, width, cards)
    }

    /// Card title and subtitle each reserve two lines, like the web's line clamp.
    pub(in crate::app) fn card_text_height(self) -> f32 {
        16. + 3. + 4. * self.text_size() * 1.2
    }
}

impl Music {
    pub(in crate::app) fn page_layout(&self, window: &Window) -> PageLayout {
        let width = f32::from(window.viewport_size().width);
        let sidebar = if self.sidebar_compact(width) {
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
            || matches!(self.route, Route::Search(_))
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
        let stacked = collection && !artist && layout.tier >= Tier::Wide;
        // Library pages open on their chips, without a back button or heading.
        let library = !collection && matches!(self.route, Route::Saved | Route::Library(_));
        let mut content = div()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .relative()
            .flex()
            .flex_col();
        // Stacked album and playlist rows scroll up to the header, so their
        // back button floats over the top of the art column instead.
        if stacked && !self.back.is_empty() {
            content = content.child(
                div()
                    .absolute()
                    .top_0()
                    .left(px(inset))
                    .h(px(24.))
                    .flex()
                    .items_center()
                    .child(
                        button("back", "back", self.theme)
                            .on_click(cx.listener(|view, _, _, cx| view.go_back(cx))),
                    ),
            );
        }
        if !home && !artist && !search && !stacked && !library {
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
        if !home && !search && !collection && !library {
            let heading = self.page_heading(false, 128., layout, cx);
            content = content.child(div().px(px(inset)).pb(px(28.)).child(heading));
        }
        if !home {
            content = self.home_filters(content, layout, cx);
        }
        // With results, the search chips scroll away as the list's first row.
        content = self.search_scopes(content, inset, cx);
        if self.rows.is_empty() {
            content = self.search_filters(content, inset, cx);
        }
        content = self.library_filters(content, inset, cx);
        if !matches!(self.overlay, Some(Overlay::SignIn))
            && let Some(error) = &self.error
        {
            let detail = SharedString::from(error.clone());
            let theme = self.theme;
            content = content.child(
                div()
                    .mx(px(inset))
                    .mb(px(16.))
                    .min_w_0()
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap(px(16.))
                    .text_color(rgb(0xf1a6ae))
                    .text_size(px(14.))
                    .child(
                        div()
                            .id("error-message")
                            .flex_1()
                            .min_w_0()
                            .whitespace_normal()
                            .child(error_summary(error))
                            .tooltip(move |_, cx| {
                                cx.new(|_| ErrorDetails {
                                    text: detail.clone(),
                                    theme,
                                })
                                .into()
                            }),
                    )
                    .child(
                        button("dismiss-error", "close", self.theme)
                            .flex_shrink_0()
                            .on_click(cx.listener(|view, _, _, cx| {
                                view.error = None;
                                view.sync.page_error = None;
                                cx.notify();
                            })),
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
        let (header_width, list_width, cover) = layout.collection_columns();
        let heading = (collection && stacked).then(|| self.page_heading(true, cover, layout, cx));
        if self.rows.is_empty()
            && !collection
            && let Some(empty) = self.library_empty()
        {
            return content.child(empty).into_any_element();
        }
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
                && !search
                && self
                    .rows
                    .iter()
                    .all(|item| matches!(item, Item::Collection(_)));
            let grouped = self.grouped_rows();
            let list = if collection && stacked && self.rows.is_empty() {
                div()
                    .flex_1()
                    .px(px(24.))
                    .pt(px(STACKED_TOP + 32.))
                    .pb(px(32.))
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
                        // Stacked rows scroll up to the header, so the space
                        // above them scrolls with the first row.
                        if stacked {
                            let row = view.list_row(index, 0., window, cx);
                            return if index == 0 {
                                div().pt(px(STACKED_TOP)).child(row).into_any_element()
                            } else {
                                row
                            };
                        }
                        if inline && index == 0 {
                            let cover = if layout.tier >= Tier::Medium {
                                240.
                            } else {
                                200.
                            };
                            let heading = view.page_heading(true, cover, layout, cx);
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
                        view.list_row(index - usize::from(inline), inset, window, cx)
                    }),
                )
                .flex_1()
                .min_h_0()
                .into_any_element()
            } else if grouped {
                // Search results carry their chips as the first row.
                self.detail_list.set_inline_header(search);
                list(
                    self.detail_list.state.clone(),
                    cx.processor(move |view, index, window, cx| {
                        if search && index == 0 {
                            return view
                                .search_filters(div().w_full().flex().flex_col(), inset, cx)
                                .into_any_element();
                        }
                        view.list_row(index - usize::from(search), inset, window, cx)
                    }),
                )
                .flex_1()
                .min_h_0()
                .into_any_element()
            } else if grid {
                let (columns, gap, card_width, layout) = if library {
                    layout.small_grid()
                } else {
                    let gap = layout.card_gap();
                    let width = layout.card_width();
                    let columns = ((layout.column + gap) / (width + gap)).floor().max(1.) as usize;
                    (columns, gap, width, layout)
                };
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
                                    .items_start()
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
                        .pl(px(((available - header_width - 40. - list_width) / 2.
                            + 8.)
                            .max(0.)))
                        .flex()
                        .gap(px(40.))
                        .child(
                            div()
                                .id("collection-header")
                                .w(px(header_width))
                                .h_full()
                                .flex_shrink_0()
                                .pt(px(STACKED_TOP))
                                .overflow_y_scroll()
                                .children(heading),
                        )
                        .child(div().w(px(list_width)).h_full().flex().child(list)),
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
