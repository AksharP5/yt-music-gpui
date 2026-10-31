use crate::{
    auth::Session,
    catalog::Catalog,
    input::{InputEvent, SearchInput},
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
    Blur,
}

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
    result: Result<Vec<String>, String>,
}

pub struct SearchBox {
    input: Entity<SearchInput>,
    session: Option<Arc<Session>>,
    suggestions: Vec<String>,
    selected: Option<usize>,
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
                InputEvent::Submit => {
                    let query = view
                        .selected
                        .and_then(|index| view.suggestions.get(index))
                        .cloned()
                        .unwrap_or_else(|| view.input.read(cx).value().to_owned());
                    view.submit(query, cx);
                }
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
            Ok(mut suggestions) => {
                suggestions.truncate(8);
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
        let open = self.focus_handle(cx).is_focused(window)
            && (!self.suggestions.is_empty() || self.error.is_some());
        let mut view = div()
            .relative()
            .w_full()
            .min_w_0()
            .on_key_down(cx.listener(Self::key_down))
            .child(self.input.clone());
        if open {
            let mut dropdown = div()
                .id("search-suggestions")
                .absolute()
                .top(px(36.))
                .left(px(-10.))
                .right(px(-10.))
                .py(px(4.))
                .rounded(px(7.))
                .bg(theme.panel)
                .border_1()
                .border_color(theme.border)
                .shadow_lg()
                .occlude()
                .text_color(theme.text)
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation());
            for (index, suggestion) in self.suggestions.iter().enumerate() {
                let query = suggestion.clone();
                dropdown = dropdown.child(
                    div()
                        .id(("suggestion", index))
                        .h(px(38.))
                        .px(px(12.))
                        .flex()
                        .items_center()
                        .text_size(px(14.))
                        .when(self.selected == Some(index), |row| row.bg(theme.selection))
                        .hover(|row| row.bg(theme.raised))
                        .cursor_pointer()
                        .child(
                            div()
                                .min_w_0()
                                .overflow_hidden()
                                .text_ellipsis()
                                .child(query.clone()),
                        )
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
                                view.submit(query.clone(), cx);
                            }),
                        ),
                );
            }
            if let Some(error) = &self.error {
                dropdown = dropdown.child(
                    div()
                        .p(px(12.))
                        .text_size(px(12.))
                        .text_color(theme.muted)
                        .child(error.clone()),
                );
            }
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
