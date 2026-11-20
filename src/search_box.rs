use crate::{
    app::HEADER_HEIGHT,
    auth::{self, Session},
    catalog::{Catalog, Item, Rating, Track},
    input::{InputEvent, SearchInput},
    search::Suggestion,
    theme::Theme,
};
use gpui::{prelude::*, *};
use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

pub enum SearchBoxEvent {
    Submit(String),
    /// A suggested artist, album or song chosen straight from the dropdown.
    Open(Box<Item>),
    Like(Box<Track>),
    Dislike(Box<Track>),
    Menu(Box<Item>),
    SessionRejected(Arc<Session>),
    Blur,
}

/// Looks up a cached thumbnail, requesting it when missing.
pub type ArtworkLookup = Box<dyn Fn(&str, Size<Pixels>, &mut App) -> Option<Arc<RenderImage>>>;
/// Reads the user's current rating of a song.
pub type RatingLookup = Box<dyn Fn(&Track, &App) -> Rating>;

/// The web's search box: 480px wide, with the dropdown joined beneath it.
pub(crate) const WIDTH: f32 = 480.;
const HEIGHT: f32 = 42.;
/// Like the web, a changed query is requested at once, then at most once per 100ms while typing
/// continues.
const THROTTLE: Duration = Duration::from_millis(100);

struct SuggestionRequest {
    id: u64,
    query: String,
    session: Option<Arc<Session>>,
}

impl SuggestionRequest {
    /// Only the latest request counts, as on the web: its reply may answer a query the user
    /// has since extended, and it stays until the next reply arrives.
    fn matches(&self, id: u64, session: Option<&Arc<Session>>) -> bool {
        self.id == id && same_session(self.session.as_ref(), session)
    }
}

struct SuggestionResult {
    request: SuggestionRequest,
    result: anyhow::Result<Vec<Suggestion>>,
}

pub struct SearchBox {
    input: Entity<SearchInput>,
    session: Option<Arc<Session>>,
    suggestions: Vec<Suggestion>,
    suggestion_scroll: ScrollHandle,
    /// What the user typed. The field shows it unless a row is selected.
    typed: String,
    /// The query the shown suggestions answer.
    answered: String,
    /// The row reached with the arrow keys, previewed in the field. Enter opens it instead
    /// of the typed text; the pointer only highlights rows, like the web.
    selected: Option<usize>,
    /// The row under the pointer. It and the selected row show their actions.
    hovered: Option<usize>,
    /// Like the web, typing or clicking the field opens the suggestions, and Escape, Tab or
    /// leaving the field closes them, keeping them for the next click.
    open: bool,
    clear_focus: FocusHandle,
    artwork: Option<ArtworkLookup>,
    rating: Option<RatingLookup>,
    error: Option<String>,
    theme: Theme,
    generation: Arc<AtomicU64>,
    jobs: async_channel::Sender<SuggestionRequest>,
    /// When the latest request left.
    requested: Option<Instant>,
    /// The request waiting out the rest of THROTTLE; replacing or dropping it cancels it.
    trailing: Option<Task<()>>,
    _subscriptions: [Subscription; 3],
}

impl EventEmitter<SearchBoxEvent> for SearchBox {}

impl SearchBox {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(SearchInput::new);
        let focus = input.focus_handle(cx);
        let generation = Arc::new(AtomicU64::new(0));
        let (jobs, completed, error) = suggestion_workers(&generation, |request| {
            Catalog::with_session(request.session.clone())
                .and_then(|catalog| catalog.suggestions(&request.query))
        });
        cx.spawn_in(window, async move |view, cx| {
            while let Ok(result) = completed.recv().await {
                if view
                    .update_in(cx, |view, window, cx| view.received(result, window, cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        let subscription =
            cx.subscribe_in(&input, window, |view, _, event, window, cx| match event {
                InputEvent::Changed => view.changed(window, cx),
                InputEvent::Submit => match view.selected {
                    Some(index) => view.choose(index, cx),
                    None => view.submit(view.input.read(cx).value().to_owned(), cx),
                },
                InputEvent::Blur => {
                    view.close(cx);
                    cx.emit(SearchBoxEvent::Blur);
                }
            });
        let blur = cx.on_blur(&focus, window, |view, _, cx| view.close(cx));
        let activation = cx.observe_window_activation(window, |view, window, cx| {
            if !window.is_window_active() {
                view.close(cx);
            }
        });
        Self {
            input,
            session: None,
            suggestions: Vec::new(),
            suggestion_scroll: ScrollHandle::new(),
            typed: String::new(),
            answered: String::new(),
            selected: None,
            hovered: None,
            open: false,
            clear_focus: cx.focus_handle().tab_index(0).tab_stop(true),
            artwork: None,
            rating: None,
            error,
            theme: Theme::load(),
            generation,
            jobs,
            requested: None,
            trailing: None,
            _subscriptions: [subscription, blur, activation],
        }
    }

    pub fn set_query(&mut self, query: String, cx: &mut Context<Self>) {
        self.typed.clone_from(&query);
        self.invalidate(cx);
        self.open = false;
    }

    /// Focus the field and show its suggestions, as the web's `/` shortcut does.
    pub fn activate(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus_handle(cx), cx);
        if self.open {
            return;
        }
        self.open = true;
        if self.answered != self.typed.trim() {
            self.request_suggestions(window, cx);
        }
        cx.notify();
    }

    #[cfg(test)]
    #[cfg(unix)]
    pub(crate) fn show_suggestions(&mut self, queries: &[&str], cx: &mut Context<Self>) {
        self.show_test_suggestions(
            queries
                .iter()
                .map(|query| Suggestion::Query {
                    query: (*query).into(),
                    label: (*query).into(),
                    bold: Vec::new(),
                    history: false,
                })
                .collect(),
            cx,
        );
    }

    #[cfg(all(test, unix))]
    pub(crate) fn show_items(&mut self, items: Vec<Item>, cx: &mut Context<Self>) {
        self.show_test_suggestions(
            items
                .into_iter()
                .map(|item| Suggestion::Item(Box::new(item)))
                .collect(),
            cx,
        );
    }

    #[cfg(all(test, unix))]
    fn show_test_suggestions(&mut self, suggestions: Vec<Suggestion>, cx: &mut Context<Self>) {
        self.suggestions = suggestions;
        self.answered = self.typed.trim().to_owned();
        self.open = true;
        cx.notify();
    }

    #[cfg(all(test, unix))]
    pub(crate) fn value<'a>(&self, cx: &'a App) -> &'a str {
        self.input.read(cx).value()
    }

    pub fn set_artwork(&mut self, lookup: ArtworkLookup) {
        self.artwork = Some(lookup);
    }

    pub fn set_rating(&mut self, lookup: RatingLookup) {
        self.rating = Some(lookup);
    }

    pub fn set_session(&mut self, session: Option<Arc<Session>>, cx: &mut Context<Self>) {
        if same_session(self.session.as_ref(), session.as_ref()) {
            return;
        }
        self.session = session;
        self.invalidate(cx);
    }

    /// Retry only the focused query owned by the verified previous session.
    pub(crate) fn renew_session(
        &mut self,
        previous: Option<&Arc<Session>>,
        next: Arc<Session>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let waiting = !self.typed.trim().is_empty() && self.answered != self.typed.trim();
        let retry = waiting
            && previous.is_some_and(|previous| {
                same_session(self.session.as_ref(), Some(previous)) && !Arc::ptr_eq(previous, &next)
            })
            && self.focus_handle(cx).is_focused(window);
        self.set_session(Some(next), cx);
        if retry {
            self.request_suggestions(window, cx);
        }
    }

    #[cfg(all(test, unix))]
    pub(crate) fn intercept_requests(
        &mut self,
    ) -> impl Fn() -> Option<(String, Option<Arc<Session>>)> + 'static + use<> {
        let (jobs, requests) = async_channel::unbounded::<SuggestionRequest>();
        self.jobs.close();
        self.jobs = jobs;
        move || {
            requests
                .try_recv()
                .ok()
                .map(|request| (request.query, request.session))
        }
    }

    #[cfg(all(test, unix))]
    pub(crate) fn reject_test_suggestion(
        &mut self,
        session: Arc<Session>,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        self.received(
            SuggestionResult {
                request: SuggestionRequest {
                    id: self.generation.load(Ordering::Acquire),
                    query: self.input.read(cx).value().trim().to_owned(),
                    session: Some(session),
                },
                result: Err(auth::RejectedSession.into()),
            },
            window,
            cx,
        );
    }

    /// Drop pending requests so no reply for an older query can arrive.
    fn cancel_requests(&mut self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.trailing = None;
    }

    fn invalidate(&mut self, cx: &mut Context<Self>) {
        self.cancel_requests();
        if !self.suggestions.is_empty() || self.error.is_some() {
            self.suggestion_scroll = ScrollHandle::new();
        }
        self.suggestions.clear();
        self.hovered = None;
        self.answered.clear();
        self.error = None;
        self.select(None, cx);
        cx.notify();
    }

    /// Close the suggestions and put the typed text back, keeping the suggestions.
    fn close(&mut self, cx: &mut Context<Self>) {
        self.cancel_requests();
        self.open = false;
        self.select(None, cx);
        cx.notify();
    }

    /// Mark a row and preview its query in the field, or restore the typed text.
    fn select(&mut self, index: Option<usize>, cx: &mut Context<Self>) {
        self.selected = index;
        let shown = match index.and_then(|index| self.suggestions.get(index)) {
            Some(Suggestion::Query { query, .. }) => query,
            _ => &self.typed,
        };
        if self.input.read(cx).value() != shown {
            let shown = shown.clone();
            self.input
                .update(cx, |input, cx| input.set_value(shown, cx));
        }
        if let Some(index) = index {
            self.suggestion_scroll.scroll_to_item(index);
        }
    }

    fn changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.typed = self.input.read(cx).value().to_owned();
        self.selected = None;
        self.error = None;
        self.open = true;
        // Earlier suggestions stay until the new ones arrive, as on the web.
        if self.typed.trim().is_empty() {
            self.cancel_requests();
            self.suggestions.clear();
            self.answered.clear();
        }
        cx.notify();
        self.request_suggestions(window, cx);
    }

    fn request_suggestions(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.typed.trim().is_empty() {
            return;
        }
        let wait = self.requested.map_or(Duration::ZERO, |requested| {
            THROTTLE.saturating_sub(requested.elapsed())
        });
        if wait.is_zero() {
            self.trailing = None;
            self.send_request(window, cx);
            return;
        }
        // Later keystrokes in the same window replace this task, so it sends the newest text.
        self.trailing = Some(cx.spawn_in(window, async move |view, cx| {
            cx.background_executor().timer(wait).await;
            let _ = view.update_in(cx, |view, window, cx| view.send_request(window, cx));
        }));
    }

    fn send_request(&mut self, window: &Window, cx: &mut Context<Self>) {
        let query = self.typed.trim();
        if query.is_empty() || !self.focus_handle(cx).is_focused(window) {
            return;
        }
        // A new request supersedes the older ones: workers skip them if still queued, and
        // their replies are dropped.
        let id = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        self.requested = Some(Instant::now());
        let request = SuggestionRequest {
            id,
            query: query.to_owned(),
            session: self.session.clone(),
        };
        if self.jobs.force_send(request).is_err() {
            self.error = Some("The search suggestions connection stopped".into());
            cx.notify();
        }
    }

    fn received(&mut self, result: SuggestionResult, window: &Window, cx: &mut Context<Self>) {
        if !result.request.matches(
            self.generation.load(Ordering::Acquire),
            self.session.as_ref(),
        ) || !self.focus_handle(cx).is_focused(window)
        {
            return;
        }
        match result.result {
            Ok(suggestions) => {
                self.suggestions = suggestions;
                self.hovered = None;
                self.answered = result.request.query;
                self.error = None;
            }
            Err(error) => {
                if auth::session_was_rejected(&error)
                    && let Some(session) = result.request.session
                {
                    cx.emit(SearchBoxEvent::SessionRejected(session));
                }
                self.error = Some(format!("{error:#}"));
            }
        }
        self.select(None, cx);
        cx.notify();
    }

    fn submit(&mut self, query: String, cx: &mut Context<Self>) {
        let query = query.trim().to_owned();
        if query.is_empty() {
            return;
        }
        self.set_query(query.clone(), cx);
        cx.emit(SearchBoxEvent::Submit(query));
    }

    fn choose(&mut self, index: usize, cx: &mut Context<Self>) {
        match self.suggestions.get(index).cloned() {
            Some(Suggestion::Query { query, .. }) => self.submit(query, cx),
            Some(Suggestion::Item(item)) => {
                self.set_query(String::new(), cx);
                cx.emit(SearchBoxEvent::Open(item));
            }
            None => {}
        }
    }

    fn clear(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.set_query(String::new(), cx);
        self.activate(window, cx);
    }

    fn suggestion_row(
        &mut self,
        index: usize,
        large: bool,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let theme = self.theme;
        let hovered = self.hovered == Some(index);
        let actions = hovered || self.selected == Some(index);
        let row = div()
            .id(("suggestion", index))
            .w_full()
            .flex()
            .items_center()
            .text_size(px(14.))
            .line_height(px(20.))
            .relative()
            .when(self.selected == Some(index), |row| {
                row.bg(gpui::white().opacity(0.1)).child(
                    div()
                        .absolute()
                        .left_0()
                        .top_0()
                        .bottom_0()
                        .w(px(2.))
                        .bg(rgb(0x3ea6ff)),
                )
            })
            .hover(|row| row.bg(gpui::white().opacity(0.1)))
            .on_hover(cx.listener(move |view, hovered: &bool, _, cx| {
                if *hovered {
                    view.hovered = Some(index);
                } else if view.hovered == Some(index) {
                    view.hovered = None;
                }
                cx.notify();
            }))
            .cursor_pointer()
            // Like the web, a row opens on release.
            .on_click(cx.listener(move |view, _, _, cx| {
                cx.stop_propagation();
                view.choose(index, cx);
            }));
        match &self.suggestions[index] {
            Suggestion::Query {
                label,
                bold,
                history,
                ..
            } => {
                let dim = gpui::white().opacity(0.5);
                let text =
                    StyledText::new(label.clone()).with_highlights(bold.iter().map(|range| {
                        (
                            range.clone(),
                            HighlightStyle {
                                font_weight: Some(FontWeight::MEDIUM),
                                ..Default::default()
                            },
                        )
                    }));
                row.h(px(48.))
                    .child(
                        div()
                            .w(px(50.))
                            .flex_shrink_0()
                            .flex()
                            .justify_center()
                            .child(
                                svg()
                                    .path(if *history { "history" } else { "search" })
                                    .size(px(20.))
                                    .text_color(dim),
                            ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .pr(px(16.))
                            .truncate()
                            .text_color(dim)
                            .child(text),
                    )
            }
            Suggestion::Item(item) => {
                let title = item.title().to_owned();
                let subtitle = match item.as_ref() {
                    Item::Track(track) => track
                        .details
                        .byline
                        .clone()
                        .unwrap_or_else(|| track.artist.clone()),
                    _ => item.subtitle().to_owned(),
                };
                let round = matches!(item.as_ref(), Item::Collection(collection) if collection.id.starts_with("UC"));
                let playable = !round && !matches!(item.as_ref(), Item::Navigation(_));
                let edge = 32.;
                // A video's 16:9 still sits inside the square, as on the web.
                let height = match item.as_ref() {
                    Item::Track(track) if track.details.wide_thumbnail => edge * 9. / 16.,
                    _ => edge,
                };
                let radius = if round { edge / 2. } else { 2. };
                let image = match (item.thumbnail().map(str::to_owned), &self.artwork) {
                    (Some(url), Some(lookup)) => lookup(&url, size(px(edge), px(height)), cx),
                    _ => None,
                };
                let thumbnail = div()
                    .relative()
                    .size(px(edge))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(match image {
                        Some(image) => img(image)
                            .w(px(edge))
                            .h(px(height))
                            .rounded(px(radius))
                            .object_fit(ObjectFit::Cover)
                            .into_any_element(),
                        None => div()
                            .size_full()
                            .rounded(px(radius))
                            .bg(theme.raised)
                            .into_any_element(),
                    })
                    .when(hovered && playable, |thumbnail| {
                        thumbnail.child(
                            div()
                                .absolute()
                                .inset_0()
                                .rounded(px(radius))
                                .bg(rgba(0x000000cc))
                                .flex()
                                .items_center()
                                .justify_center()
                                .child(svg().path("play").size(px(24.)).text_color(theme.text)),
                        )
                    });
                let item = item.as_ref().clone();
                row.h(px(56.))
                    .pl(px(12.))
                    .pr(px(if actions { 12. } else { 16. }))
                    .gap(px(16.))
                    .child(thumbnail)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .text_size(px(if large { 16. } else { 14. }))
                            .line_height(relative(1.2))
                            .gap(px(3.))
                            .child(
                                div()
                                    .truncate()
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(theme.text)
                                    .child(title),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .text_color(gpui::white().opacity(0.7))
                                    .child(subtitle),
                            ),
                    )
                    .when(actions, |row| row.child(self.row_actions(index, item, cx)))
            }
        }
    }

    /// The web's row actions: Like and Dislike for songs and videos, then a menu.
    fn row_actions(&self, index: usize, item: Item, cx: &mut Context<Self>) -> Div {
        let action = |id: &'static str, icon: &'static str| {
            div()
                .id((id, index))
                .size(px(36.))
                .flex_shrink_0()
                .rounded_full()
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .hover(|button| button.bg(gpui::white().opacity(0.1)))
                .child(svg().path(icon).size(px(24.)).text_color(rgb(0xf1f1f1)))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        };
        let rated = match (&item, &self.rating) {
            (Item::Track(track), Some(rating)) => Some((track.clone(), rating(track, cx))),
            _ => None,
        };
        div()
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap(px(16.))
            .when_some(rated, |actions, (track, rating)| {
                let disliked = track.clone();
                let like = if rating == Rating::Like {
                    "thumb-up-filled"
                } else {
                    "thumb-up"
                };
                let dislike = if rating == Rating::Dislike {
                    "thumb-down-filled"
                } else {
                    "thumb-down"
                };
                actions.child(
                    div()
                        .flex()
                        .gap(px(2.))
                        .child(action("suggestion-like", like).on_click(cx.listener(
                            move |_, _, _, cx| {
                                cx.stop_propagation();
                                cx.emit(SearchBoxEvent::Like(Box::new(track.clone())));
                                cx.notify();
                            },
                        )))
                        .child(action("suggestion-dislike", dislike).on_click(cx.listener(
                            move |_, _, _, cx| {
                                cx.stop_propagation();
                                cx.emit(SearchBoxEvent::Dislike(Box::new(disliked.clone())));
                                cx.notify();
                            },
                        ))),
                )
            })
            .child(
                action("suggestion-menu", "more").on_click(cx.listener(move |_, _, _, cx| {
                    cx.stop_propagation();
                    cx.emit(SearchBoxEvent::Menu(Box::new(item.clone())));
                })),
            )
    }

    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let rows = self.suggestions.len();
        if !self.open
            || rows == 0
            || event.keystroke.modifiers.modified()
            || !self.focus_handle(cx).is_focused(window)
        {
            return;
        }
        // The typed text sits between the last row and the first, as on the web.
        let index = match (event.keystroke.key.as_str(), self.selected) {
            ("down", None) => Some(0),
            ("down", Some(index)) => Some(index + 1).filter(|next| *next < rows),
            ("up", None) => Some(rows - 1),
            ("up", Some(index)) => index.checked_sub(1),
            _ => return,
        };
        self.select(index, cx);
        cx.stop_propagation();
        cx.notify();
    }
}

impl Focusable for SearchBox {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.input.focus_handle(cx)
    }
}

impl Drop for SearchBox {
    fn drop(&mut self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.jobs.close();
    }
}

impl Render for SearchBox {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        // Song, video and collection rows use 16px text from the web's 1364px breakpoint.
        let large = window.viewport_size().width >= px(1364.);
        let typing = self.focus_handle(cx).is_focused(window);
        let open = self.open && typing;
        let focused = typing || self.clear_focus.is_focused(window);
        let empty = self.input.read(cx).value().is_empty();
        let edge = gpui::white().opacity(0.15);
        // The web brightens the icons while the box has focus, or on hover once it holds a query.
        let icon = |icon: Svg| {
            icon.text_color(gpui::white().opacity(if focused { 1. } else { 0.5 }))
                .when(!empty, |icon| {
                    icon.group_hover("search-box", |style| style.text_color(gpui::white()))
                })
        };
        let mut view = div()
            .id("search-box")
            .group("search-box")
            .relative()
            .w(px(WIDTH))
            .min_w_0()
            .h(px(HEIGHT))
            .pl(px(16.))
            .pr(px(9.))
            .rounded(px(8.))
            .when(open, |view| view.rounded_b_none())
            // Focused, the web darkens the box rather than underlining the text.
            .bg(if focused { rgb(0x030303).into() } else { edge })
            .border_1()
            .border_color(if focused { edge } else { rgb(0x484848).into() })
            .flex()
            .items_center()
            .gap(px(16.))
            .on_key_down(cx.listener(Self::key_down))
            .on_click(cx.listener(|view, _, window, cx| view.activate(window, cx)))
            .child(icon(svg().path("search").size(px(20.)).flex_shrink_0()))
            .child(div().flex_1().min_w_0().child(self.input.clone()))
            .when(!empty, |view| {
                view.child(
                    div()
                        .id("search-clear")
                        .track_focus(&self.clear_focus)
                        .size(px(32.))
                        .flex_shrink_0()
                        .rounded_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .cursor_pointer()
                        .hover(|button| button.bg(gpui::white().opacity(0.1)))
                        .focus_visible(|button| button.bg(gpui::white().opacity(0.1)))
                        .child(icon(svg().path("search-clear").size(px(18.))))
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(cx.listener(|view, _, window, cx| {
                            cx.stop_propagation();
                            view.clear(window, cx);
                        })),
                )
            });
        if open {
            let rows = (0..self.suggestions.len())
                .map(|index| self.suggestion_row(index, large, cx))
                .collect::<Vec<_>>();
            let dropdown = div()
                .id("search-suggestions")
                .absolute()
                .top(px(HEIGHT - 2.))
                .left(px(-1.))
                .right(px(-1.))
                // The dropdown starts within the header, leaving a bottom inset.
                .max_h((window.viewport_size().height - px(HEADER_HEIGHT)).max(px(0.)))
                .overflow_y_scroll()
                .track_scroll(&self.suggestion_scroll)
                .py(px(8.))
                .rounded_b(px(8.))
                .bg(rgb(0x030303))
                .border_1()
                .border_color(edge)
                .occlude()
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .children(rows)
                .when_some(self.error.clone(), |dropdown, error| {
                    dropdown.child(
                        div()
                            .p(px(12.))
                            .text_size(px(12.))
                            .text_color(theme.muted)
                            .child(error),
                    )
                });
            view = view.child(deferred(dropdown).with_priority(1));
        }
        view
    }
}

fn suggestion_workers(
    generation: &Arc<AtomicU64>,
    lookup: impl Fn(&SuggestionRequest) -> anyhow::Result<Vec<Suggestion>> + Clone + Send + 'static,
) -> (
    async_channel::Sender<SuggestionRequest>,
    async_channel::Receiver<SuggestionResult>,
    Option<String>,
) {
    // Keep one pending query and allow a new query to bypass a slow obsolete read.
    let (jobs, requests) = async_channel::bounded::<SuggestionRequest>(1);
    let (results, completed) = async_channel::unbounded();
    let mut running = false;
    let mut error = None;
    for name in ["search-suggestions", "search-suggestions-spare"] {
        let requests = requests.clone();
        let results = results.clone();
        let generation = generation.clone();
        let lookup = lookup.clone();
        match thread::Builder::new().name(name.into()).spawn(move || {
            while let Ok(request) = requests.recv_blocking() {
                if request.id != generation.load(Ordering::Acquire) {
                    continue;
                }
                let result = lookup(&request);
                if request.id == generation.load(Ordering::Acquire)
                    && results
                        .send_blocking(SuggestionResult { request, result })
                        .is_err()
                {
                    break;
                }
            }
        }) {
            Ok(_) => running = true,
            Err(failure) => error = Some(format!("Could not start search suggestions: {failure}")),
        }
    }
    (jobs, completed, error.filter(|_| !running))
}

fn same_session(left: Option<&Arc<Session>>, right: Option<&Arc<Session>>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => Arc::ptr_eq(left, right),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::{Session, SuggestionRequest, suggestion_workers};
    use serde_json::json;
    use std::{
        sync::{
            Arc,
            atomic::{AtomicU64, Ordering},
            mpsc,
        },
        time::Duration,
    };

    #[test]
    fn suggestions_for_the_latest_query_bypass_a_slow_old_request() {
        let generation = Arc::new(AtomicU64::new(1));
        let (started, old_started) = mpsc::channel();
        let (release, old_release) = async_channel::bounded::<()>(1);
        let (jobs, completed, error) = suggestion_workers(&generation, move |request| {
            if request.id == 1 {
                started.send(()).unwrap();
                let _ = old_release.recv_blocking();
            }
            Ok(Vec::new())
        });
        assert!(error.is_none(), "{error:?}");
        let (observed, results) = mpsc::channel();
        let forward = std::thread::spawn(move || {
            while let Ok(result) = completed.recv_blocking() {
                if observed.send(result).is_err() {
                    break;
                }
            }
        });
        let timeout = Duration::from_secs(3);
        jobs.send_blocking(SuggestionRequest {
            id: 1,
            query: "older query".into(),
            session: None,
        })
        .unwrap();
        old_started.recv_timeout(timeout).unwrap();
        generation.store(2, Ordering::Release);
        jobs.send_blocking(SuggestionRequest {
            id: 2,
            query: "new query".into(),
            session: None,
        })
        .unwrap();
        let latest = results.recv_timeout(timeout);
        drop(release);
        drop(jobs);
        let latest = latest.unwrap();
        assert_eq!(latest.request.query, "new query");
        assert!(latest.result.is_ok());
        assert!(matches!(
            results.recv_timeout(timeout),
            Err(mpsc::RecvTimeoutError::Disconnected)
        ));
        forward.join().unwrap();
    }

    #[test]
    fn suggestions_require_the_latest_request_and_account() {
        let session: Session = serde_json::from_value(json!({
            "cookie":"SAPISID=synthetic","playback_cookie":"# Netscape HTTP Cookie File\n",
            "authuser":"0","delegated_session_id":null,"data_sync_id":"user||","client_version":"test"
        })).unwrap();
        let session = Arc::new(session);
        let request = SuggestionRequest {
            id: 7,
            query: "a song".into(),
            session: Some(session.clone()),
        };
        assert!(request.matches(7, Some(&session)));
        assert!(!request.matches(8, Some(&session)));
        assert!(!request.matches(7, None));
        let restored = Arc::new((*session).clone());
        assert!(!request.matches(7, Some(&restored)));
        let guest = SuggestionRequest {
            id: 1,
            query: "a song".into(),
            session: None,
        };
        assert!(guest.matches(1, None));
        assert!(!guest.matches(1, Some(&session)));
    }
}
