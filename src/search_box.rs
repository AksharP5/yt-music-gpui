use crate::{
    auth::Session,
    catalog::{Catalog, Item},
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
    time::Duration,
};

pub enum SearchBoxEvent {
    Submit(String),
    /// A suggested artist, album or song chosen straight from the dropdown.
    Open(Box<Item>),
    Blur,
}

/// Looks up a cached thumbnail, requesting it when missing.
pub type ArtworkLookup = Box<dyn Fn(&str, &mut App) -> Option<Arc<RenderImage>>>;

/// The web's search box: 480px wide, with the dropdown joined beneath it.
pub(crate) const WIDTH: f32 = 480.;
const HEIGHT: f32 = 42.;

struct SuggestionRequest {
    id: u64,
    query: String,
    session: Option<Arc<Session>>,
}

impl SuggestionRequest {
    fn matches(&self, id: u64, query: &str, session: Option<&Arc<Session>>) -> bool {
        self.id == id && self.query == query && same_session(self.session.as_ref(), session)
    }
}

struct SuggestionResult {
    request: SuggestionRequest,
    result: Result<Vec<Suggestion>, String>,
}

pub struct SearchBox {
    input: Entity<SearchInput>,
    session: Option<Arc<Session>>,
    suggestions: Vec<Suggestion>,
    selected: Option<usize>,
    artwork: Option<ArtworkLookup>,
    error: Option<String>,
    theme: Theme,
    generation: Arc<AtomicU64>,
    jobs: async_channel::Sender<SuggestionRequest>,
    debounce: Option<Task<()>>,
    _subscriptions: [Subscription; 3],
}

impl EventEmitter<SearchBoxEvent> for SearchBox {}

impl SearchBox {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(SearchInput::new);
        let focus = input.focus_handle(cx);
        let generation = Arc::new(AtomicU64::new(0));
        let (jobs, requests) = async_channel::bounded::<SuggestionRequest>(1);
        let (results, completed) = async_channel::unbounded();
        let worker_generation = generation.clone();
        let error = thread::Builder::new()
            .name("search-suggestions".into())
            .spawn(move || {
                let catalog = Catalog::new();
                while let Ok(request) = requests.recv_blocking() {
                    if request.id != worker_generation.load(Ordering::Acquire) {
                        continue;
                    }
                    let result = catalog
                        .as_ref()
                        .map_err(|error| format!("{error:#}"))
                        .and_then(|catalog| {
                            catalog
                                .for_session(request.session.clone())
                                .suggestions(&request.query)
                                .map_err(|error| format!("{error:#}"))
                        });
                    if request.id == worker_generation.load(Ordering::Acquire)
                        && results
                            .send_blocking(SuggestionResult { request, result })
                            .is_err()
                    {
                        break;
                    }
                }
            })
            .err()
            .map(|error| format!("Could not start search suggestions: {error}"));
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
                    let open = !view.suggestions.is_empty() || view.error.is_some();
                    view.invalidate(cx);
                    if !open {
                        cx.emit(SearchBoxEvent::Blur);
                    }
                }
            });
        let blur = cx.on_blur(&focus, window, |view, _, cx| view.invalidate(cx));
        let activation = cx.observe_window_activation(window, |view, window, cx| {
            if !window.is_window_active() {
                view.invalidate(cx);
            }
        });
        Self {
            input,
            session: None,
            suggestions: Vec::new(),
            selected: None,
            artwork: None,
            error,
            theme: Theme::load(),
            generation,
            jobs,
            debounce: None,
            _subscriptions: [subscription, blur, activation],
        }
    }

    pub fn set_query(&mut self, query: String, cx: &mut Context<Self>) {
        self.invalidate(cx);
        self.input
            .update(cx, |input, cx| input.set_value(query, cx));
    }

    pub fn set_artwork(&mut self, lookup: ArtworkLookup) {
        self.artwork = Some(lookup);
    }

    pub fn set_session(&mut self, session: Option<Arc<Session>>, cx: &mut Context<Self>) {
        if same_session(self.session.as_ref(), session.as_ref()) {
            return;
        }
        self.session = session;
        self.invalidate(cx);
    }

    fn invalidate(&mut self, cx: &mut Context<Self>) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.debounce = None;
        self.suggestions.clear();
        self.selected = None;
        self.error = None;
        cx.notify();
    }

    fn changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.invalidate(cx);
        let query = self.input.read(cx).value().trim().to_owned();
        if query.is_empty() {
            return;
        }
        let request = SuggestionRequest {
            id: self.generation.load(Ordering::Acquire),
            query,
            session: self.session.clone(),
        };
        self.debounce = Some(cx.spawn_in(window, async move |view, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(225))
                .await;
            let _ = view.update_in(cx, |view, window, cx| {
                if !request.matches(
                    view.generation.load(Ordering::Acquire),
                    view.input.read(cx).value().trim(),
                    view.session.as_ref(),
                ) || !view.focus_handle(cx).is_focused(window)
                {
                    return;
                }
                if view.jobs.force_send(request).is_err() {
                    view.error = Some("The search suggestions connection stopped".into());
                    cx.notify();
                }
            });
        }));
    }

    fn received(&mut self, result: SuggestionResult, window: &Window, cx: &mut Context<Self>) {
        if !result.request.matches(
            self.generation.load(Ordering::Acquire),
            self.input.read(cx).value().trim(),
            self.session.as_ref(),
        ) || !self.focus_handle(cx).is_focused(window)
        {
            return;
        }
        match result.result {
            Ok(suggestions) => {
                self.suggestions = suggestions;
                self.error = None;
            }
            Err(error) => self.error = Some(error),
        }
        self.selected = None;
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
        window.focus(&self.focus_handle(cx), cx);
    }

    fn suggestion_row(&mut self, index: usize, cx: &mut Context<Self>) -> Stateful<Div> {
        let theme = self.theme;
        let row = div()
            .id(("suggestion", index))
            .w_full()
            .flex()
            .items_center()
            .text_size(px(14.))
            .line_height(px(20.))
            .when(self.selected == Some(index), |row| {
                row.bg(gpui::white().opacity(0.1))
            })
            .cursor_pointer()
            .on_mouse_move(cx.listener(move |view, _, _, cx| {
                if view.selected != Some(index) {
                    view.selected = Some(index);
                    cx.notify();
                }
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |view, _, _, cx| {
                    cx.stop_propagation();
                    view.choose(index, cx);
                }),
            );
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
                let radius = if round { 16. } else { 2. };
                let image = match (item.thumbnail().map(str::to_owned), &self.artwork) {
                    (Some(url), Some(lookup)) => lookup(&url, cx),
                    _ => None,
                };
                row.h(px(56.))
                    .pl(px(12.))
                    .pr(px(16.))
                    .gap(px(16.))
                    .child(
                        div()
                            .size(px(32.))
                            .flex_shrink_0()
                            .rounded(px(radius))
                            .bg(theme.raised)
                            .when_some(image, |frame, image| {
                                frame.child(
                                    img(image)
                                        .size_full()
                                        .rounded(px(radius))
                                        .object_fit(ObjectFit::Cover),
                                )
                            }),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .line_height(px(16.8))
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
            }
        }
    }

    fn key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.suggestions.is_empty() || event.keystroke.modifiers.modified() {
            return;
        }
        self.selected = match event.keystroke.key.as_str() {
            "down" => Some(
                self.selected
                    .map_or(0, |index| (index + 1) % self.suggestions.len()),
            ),
            "up" => Some(self.selected.map_or(self.suggestions.len() - 1, |index| {
                (index + self.suggestions.len() - 1) % self.suggestions.len()
            })),
            _ => return,
        };
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
        let focused = self.focus_handle(cx).is_focused(window);
        let open = focused && (!self.suggestions.is_empty() || self.error.is_some());
        let empty = self.input.read(cx).value().is_empty();
        let edge = gpui::white().opacity(0.15);
        let mut view = div()
            .id("search-box")
            .relative()
            .w(px(WIDTH))
            .min_w_0()
            .h(px(HEIGHT))
            .pl(px(16.))
            .pr(px(4.))
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
            .on_click(cx.listener(|view, _, window, cx| window.focus(&view.focus_handle(cx), cx)))
            .child(
                svg()
                    .path("search")
                    .size(px(20.))
                    .flex_shrink_0()
                    .text_color(gpui::white().opacity(0.5)),
            )
            .child(div().flex_1().min_w_0().child(self.input.clone()))
            .when(!empty, |view| {
                view.child(
                    div()
                        .id("search-clear")
                        .size(px(32.))
                        .flex_shrink_0()
                        .rounded_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .cursor_pointer()
                        .hover(|button| button.bg(gpui::white().opacity(0.1)))
                        .child(svg().path("close").size(px(20.)).text_color(theme.text))
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(cx.listener(|view, _, window, cx| {
                            cx.stop_propagation();
                            view.clear(window, cx);
                        })),
                )
            });
        if open {
            let rows = (0..self.suggestions.len())
                .map(|index| self.suggestion_row(index, cx))
                .collect::<Vec<_>>();
            let dropdown = div()
                .id("search-suggestions")
                .absolute()
                .top(px(HEIGHT - 2.))
                .left(px(-1.))
                .w(px(WIDTH))
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

fn same_session(left: Option<&Arc<Session>>, right: Option<&Arc<Session>>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => Arc::ptr_eq(left, right),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::{Session, SuggestionRequest};
    use serde_json::json;
    use std::sync::Arc;

    #[test]
    fn suggestions_require_the_exact_query_generation_and_account() {
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
        assert!(request.matches(7, "a song", Some(&session)));
        assert!(!request.matches(8, "a song", Some(&session)));
        assert!(!request.matches(7, "another song", Some(&session)));
        assert!(!request.matches(7, "a song", None));
        let restored = Arc::new((*session).clone());
        assert!(!request.matches(7, "a song", Some(&restored)));
        let guest = SuggestionRequest {
            id: 1,
            query: "a song".into(),
            session: None,
        };
        assert!(guest.matches(1, "a song", None));
        assert!(!guest.matches(1, "a song", Some(&session)));
    }
}
