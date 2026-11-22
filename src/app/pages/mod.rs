//! Page layout, list state, and dispatch to the page-specific renderers.

pub(in crate::app) mod artist;
pub(in crate::app) mod collection;
pub(in crate::app) mod home;
pub(in crate::app) mod items;
pub(in crate::app) mod library;
mod playlist_order;
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
    EmptyPlaylist,
    SuggestionsHeading,
    Suggestion(usize),
    SuggestionsFooter,
    Related(usize),
}

impl ListRow {
    pub(in crate::app) fn is_suggestion(&self) -> bool {
        matches!(
            self,
            Self::EmptyPlaylist
                | Self::SuggestionsHeading
                | Self::Suggestion(_)
                | Self::SuggestionsFooter
                | Self::Related(_)
        )
    }
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
    /// Sizes the list to `rows` plus its inline header, keeping its position.
    pub(in crate::app) fn set_rows(&self, rows: usize) {
        if self.state.item_count() != rows + usize::from(self.inline_header) {
            self.reset_rows(rows);
        }
    }
    pub(in crate::app) fn replace_tail(&self, start: usize, rows: usize) {
        let top = self.state.logical_scroll_top();
        let end = self.state.item_count();
        self.state
            .splice(start + usize::from(self.inline_header)..end, rows);
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
            ShelfLayout::Cards
            | ShelfLayout::SmallCards
            | ShelfLayout::Featured
            | ShelfLayout::Grid => self.card_width(),
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

    /// Left edges of a stacked collection page's header column and of its rows,
    /// which sit side by side, centered in the page.
    pub(in crate::app) fn stacked_columns(self) -> (f32, f32) {
        let (header, tracks, _) = self.collection_columns();
        let header_left =
            ((self.column + 2. * self.inset - header - 40. - tracks) / 2. + 8.).max(0.);
        (header_left, header_left + header + 40.)
    }

    /// The web narrows search results to a centered column.
    pub(in crate::app) fn search_width(self) -> f32 {
        match self.tier {
            Tier::Narrow if self.window < 616. => self.column,
            Tier::Narrow => 560.,
            Tier::Medium => 720.,
            _ => 860.,
        }
        .min(if self.tier < Tier::Wide {
            self.column
        } else {
            self.column + 2. * self.inset - 32.
        })
    }

    /// The small-card grid of the library and of pages that are a single
    /// grid of albums or playlists: columns, gap, card edge, and the layout
    /// its cards use. The web keeps these cards' text at 14px.
    pub(in crate::app) fn small_grid(self) -> (usize, f32, f32, Self) {
        // The web's library column count and small-card margin, for windows
        // at least as wide as the app's 850px minimum.
        let (columns, gap) = match self.tier {
            Tier::Narrow | Tier::Medium => (5, 16.),
            Tier::Wide => (6, 16.),
            Tier::Large => (7, 24.),
            Tier::Huge if self.window < 1800. => (8, 24.),
            Tier::Huge => (9, 24.),
        };
        let width = (self.column - (columns - 1) as f32 * gap) / columns as f32;
        let cards = Self {
            tier: self.tier.min(Tier::Wide),
            ..self
        };
        (columns, gap, width, cards)
    }

    /// Mood and genre buttons per row in the web's Moods & genres grid.
    pub(in crate::app) fn mood_columns(self) -> usize {
        match self.tier {
            Tier::Narrow => 3,
            Tier::Medium | Tier::Wide | Tier::Large => 4,
            Tier::Huge => 5,
        }
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

    /// Explore pages such as Charts and moods arrive as titled shelves, shown like Home's.
    pub(in crate::app) fn browse_sections(&self) -> bool {
        self.page.header.is_none()
            && matches!(self.route, Route::Browse(_))
            && self
                .page
                .shelves
                .iter()
                .filter(|shelf| !shelf.title.is_empty())
                .nth(1)
                .is_some()
    }

    pub(in crate::app) fn grouped_rows(&self) -> bool {
        self.page.header.is_some()
            || self.browse_sections()
            || self.history_groups()
            || matches!(self.route, Route::Search(_))
    }

    /// A page that is one grid of albums or playlists, such as a library
    /// section or New releases' Albums & singles.
    pub(in crate::app) fn collection_grid(&self) -> bool {
        !self.route.is_home()
            && !matches!(self.route, Route::Explore | Route::Search(_))
            && !self.grouped_rows()
            && !self.rows.is_empty()
            && self
                .rows
                .iter()
                .all(|item| matches!(item, Item::Collection(_)))
    }

    pub(in crate::app) fn list_row(
        &mut self,
        index: usize,
        inset: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let playlist_end = self.playlist_rows_end();
        self.prefetch_page(index.min(playlist_end), playlist_end, 8, cx);
        match self.list_rows[index] {
            ListRow::Featured(index) => {
                self.featured_result(index, inset, self.page_layout(window).search_width(), cx)
            }
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
            ListRow::EmptyPlaylist => div()
                .px(px(inset + 16.))
                .py(px(24.))
                .text_color(self.theme.muted)
                .child("No songs here yet.")
                .into_any_element(),
            ListRow::SuggestionsHeading => div()
                .h(px(96.))
                .px(px(inset))
                .flex()
                .items_center()
                .text_size(px(28.))
                .font_weight(FontWeight::BOLD)
                .child("Suggestions")
                .into_any_element(),
            ListRow::Suggestion(index) => self.suggestion_row(index, inset, cx),
            ListRow::SuggestionsFooter => self.suggestions_footer(inset, cx),
            ListRow::Related(index) => {
                self.shelf(self.page.shelves.len() + index, inset, window, cx)
            }
            ListRow::More(index) => {
                let Some(link) = self.page.shelves[index].more.clone() else {
                    return div().into_any_element();
                };
                div()
                    .h(px(64.))
                    .px(px(inset))
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
        let floating_back = stacked
            && self.error.is_none()
            && self.sync.page_error.is_none()
            && self.notice.is_none();
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
        if floating_back && !self.back.is_empty() {
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
        if collection && !artist && !floating_back {
            content = content.child(
                div()
                    .h(px(24.))
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
        // Like the web, Explore pages such as Charts and Moods & genres have no
        // back button, and their title scrolls away with the page.
        let titled = !home && !search && !collection && !library;
        let title_scrolls = titled && (self.grouped_rows() || self.collection_grid());
        if titled && !title_scrolls {
            content = content.child(self.browse_heading(inset, cx));
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
            && let Some(error) = self.error.as_ref().or(self.sync.page_error.as_ref())
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
                                if view.error.is_none() || view.error == view.sync.page_error {
                                    view.sync.page_error = None;
                                }
                                view.error = None;
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
        // Stacked pages scroll from anywhere in the page, like the web's document: the
        // list spans the page and its rows start past the header column.
        let (header_left, rows_left) = layout.stacked_columns();
        let stacked_row = move |row: AnyElement| {
            div()
                .pl(px(rows_left))
                .child(div().w(px(list_width)).child(row))
                .into_any_element()
        };
        let heading = (collection && stacked).then(|| self.page_heading(cover, layout, cx));
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
            let grouped = self.grouped_rows();
            let empty_stacked =
                collection && stacked && self.rows.is_empty() && self.suggestions.is_none();
            let list = if empty_stacked {
                div()
                    .flex_1()
                    .child(stacked_row(
                        div()
                            .px(px(24.))
                            .pt(px(STACKED_TOP + 32.))
                            .pb(px(32.))
                            .text_color(self.theme.muted)
                            .child("No songs here yet.")
                            .into_any_element(),
                    ))
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
                            return stacked_row(if index == 0 {
                                div()
                                    .pt(px(STACKED_TOP))
                                    .children(view.playlist_sort(cx))
                                    .child(row)
                                    .into_any_element()
                            } else {
                                row
                            });
                        }
                        if inline && index == 0 {
                            let cover = if layout.tier >= Tier::Medium {
                                240.
                            } else {
                                200.
                            };
                            let heading = view.page_heading(cover, layout, cx);
                            let header = div()
                                .w_full()
                                .px(px(inset))
                                .pb(px(32.))
                                .flex()
                                .justify_center()
                                .child(div().w_full().max_w(px(500.)).child(heading).when(
                                    view.rows.is_empty() && view.suggestions.is_none(),
                                    |d| {
                                        d.child(
                                            div()
                                                .mt(px(24.))
                                                .text_color(view.theme.muted)
                                                .child("No songs here yet."),
                                        )
                                    },
                                ));
                            return div()
                                .child(header)
                                .children(view.playlist_sort(cx).map(|sort| sort.ml(px(inset))))
                                .into_any_element();
                        }
                        view.list_row(index - usize::from(inline), inset, window, cx)
                    }),
                )
                .flex_1()
                .min_h_0()
                .into_any_element()
            } else if grouped {
                // Search results carry their chips as the first row, and other
                // pages their title.
                self.detail_list.set_inline_header(search || title_scrolls);
                list(
                    self.detail_list.state.clone(),
                    cx.processor(move |view, index, window, cx| {
                        if search && index == 0 {
                            return view
                                .search_filters(div().w_full().flex().flex_col(), inset, cx)
                                .into_any_element();
                        }
                        if title_scrolls && index == 0 {
                            return view.browse_heading(inset, cx).into_any_element();
                        }
                        view.list_row(
                            index - usize::from(search || title_scrolls),
                            inset,
                            window,
                            cx,
                        )
                    }),
                )
                .flex_1()
                .min_h_0()
                .into_any_element()
            } else if library && self.collection_grid() {
                let (columns, gap, card_width, layout) = layout.small_grid();
                let row_height = card_width + layout.card_text_height() + 24.;
                uniform_list(
                    ("collection-grid", self.list_id),
                    self.rows.len().div_ceil(columns),
                    cx.processor(move |view, range: std::ops::Range<usize>, _, cx| {
                        view.prefetch_page(range.end, view.rows.len().div_ceil(columns), 2, cx);
                        range
                            .map(|row| {
                                view.card_row(row, columns, card_width, gap, layout, cx)
                                    .h(px(row_height))
                                    .px(px(inset))
                                    .into_any_element()
                            })
                            .collect::<Vec<_>>()
                    }),
                )
                .track_scroll(&self.list_scroll)
                .flex_1()
                .min_h_0()
                .into_any_element()
            } else if title_scrolls {
                // A grid page such as Albums & singles: its title scrolls with
                // the rows, so one list row holds one row of cards.
                let (columns, gap, card_width, cards) = layout.small_grid();
                let rows = self.rows.len().div_ceil(columns);
                self.detail_list.set_inline_header(true);
                self.detail_list.set_rows(rows);
                list(
                    self.detail_list.state.clone(),
                    cx.processor(move |view, index: usize, _, cx| {
                        let Some(row) = index.checked_sub(1) else {
                            return view.browse_heading(inset, cx).into_any_element();
                        };
                        view.prefetch_page(row + 1, rows, 2, cx);
                        // The web's library grid leaves 40px between rows.
                        view.card_row(row, columns, card_width, gap, cards, cx)
                            .px(px(inset))
                            .pb(px(40.))
                            .into_any_element()
                    }),
                )
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
                // A header taller than the window scrolls on its own; otherwise the
                // wheel over it reaches the list beneath.
                let header_overflows = self.collection_header_scroll.max_offset().y > px(0.);
                content.child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .w_full()
                        .relative()
                        .flex()
                        .child(list)
                        .child(
                            div()
                                .id("collection-header")
                                .absolute()
                                .top_0()
                                .bottom_0()
                                .left(px(header_left))
                                .w(px(header_width))
                                .pt(px(STACKED_TOP))
                                .overflow_y_scroll()
                                .track_scroll(&self.collection_header_scroll)
                                .when(header_overflows, |header| header.occlude())
                                .children(heading),
                        ),
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
    /// One row of a collection grid's cards.
    fn card_row(
        &mut self,
        row: usize,
        columns: usize,
        width: f32,
        gap: f32,
        layout: PageLayout,
        cx: &Context<Self>,
    ) -> Div {
        let first = row * columns;
        let items = self
            .rows
            .iter()
            .skip(first)
            .take(columns)
            .cloned()
            .collect::<Vec<_>>();
        div().flex().items_start().gap(px(gap)).children(
            items
                .into_iter()
                .enumerate()
                .map(|(column, item)| {
                    self.card(item, first + column, width, ShelfLayout::Cards, layout, cx)
                })
                .collect::<Vec<_>>(),
        )
    }

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
