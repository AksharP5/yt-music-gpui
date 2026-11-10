//! Comments UI. Only the song-details worker can perform its read continuations.

use super::*;
use crate::comments::{Page, Sort, Thread};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Load {
    Initial,
    Sort(usize),
    More,
    Replies(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Control {
    Sort,
    SortOption(usize),
    Read(usize),
    Toggle(usize),
    More,
    Replies(usize),
}

fn comment_preview(text: &str) -> Option<String> {
    let mut lines = 1;
    for (count, (index, character)) in text.char_indices().enumerate() {
        if count == 180 || (character == '\n' && lines == 4) {
            return Some(format!("{}…", text[..index].trim_end()));
        }
        if character == '\n' {
            lines += 1;
        }
    }
    None
}

struct Pending {
    id: u64,
    load: Load,
}

struct Entry {
    thread: Thread,
    expanded: bool,
    preview: Option<String>,
    replies_open: bool,
    replies: Vec<usize>,
    depth: usize,
    error: Option<String>,
}

#[derive(Clone, Copy)]
enum Row {
    Comment(usize),
    Replies(usize),
    More,
}

pub(super) struct State {
    track: String,
    session: Option<Arc<Session>>,
    pending: Option<Pending>,
    loaded: bool,
    retry: Option<(Load, Continuation)>,
    count: String,
    sorts: Vec<Sort>,
    menu: bool,
    entries: Vec<Entry>,
    roots: Vec<usize>,
    continuation: Option<Continuation>,
    message: Option<String>,
    error: Option<String>,
    rows: Vec<Row>,
    list: ListState,
    controls: Vec<(usize, Control)>,
    focus: HashMap<Control, FocusHandle>,
    revealing: Option<(Control, Rc<Cell<bool>>)>,
    #[cfg(test)]
    rendered: HashSet<usize>,
}

impl State {
    pub(super) fn new() -> Self {
        Self {
            track: String::new(),
            session: None,
            pending: None,
            loaded: false,
            retry: None,
            count: "Comments".into(),
            sorts: Vec::new(),
            menu: false,
            entries: Vec::new(),
            roots: Vec::new(),
            continuation: None,
            message: None,
            error: None,
            rows: Vec::new(),
            list: ListState::new(0, ListAlignment::Top, px(200.)),
            controls: Vec::new(),
            focus: HashMap::new(),
            revealing: None,
            #[cfg(test)]
            rendered: HashSet::new(),
        }
    }

    pub(super) fn pending_id(&self) -> Option<u64> {
        self.pending.as_ref().map(|pending| pending.id)
    }

    pub(super) fn close_menu(&mut self) {
        self.menu = false;
    }

    pub(super) fn cancel(&mut self) -> bool {
        self.pending.take().is_some()
    }

    fn rebuild(&mut self, reset: bool) {
        self.rows.clear();
        let mut pending = self
            .roots
            .iter()
            .rev()
            .map(|index| Row::Comment(*index))
            .collect::<Vec<_>>();
        while let Some(row) = pending.pop() {
            self.rows.push(row);
            if let Row::Comment(index) = row {
                let entry = &self.entries[index];
                if entry.replies_open {
                    if entry.thread.replies.is_some() || entry.error.is_some() {
                        pending.push(Row::Replies(index));
                    }
                    pending.extend(entry.replies.iter().rev().map(|reply| Row::Comment(*reply)));
                }
            }
        }
        if self.continuation.is_some() || self.error.is_some() {
            self.rows.push(Row::More);
        }
        self.controls.clear();
        for (row, item) in self.rows.iter().enumerate() {
            match *item {
                Row::Comment(index) => {
                    let entry = &self.entries[index];
                    if entry.preview.is_some() {
                        self.controls.push((row, Control::Read(index)));
                    }
                    if entry.thread.replies.is_some() || !entry.replies.is_empty() {
                        self.controls.push((row, Control::Toggle(index)));
                    }
                }
                Row::More => self.controls.push((row, Control::More)),
                Row::Replies(parent) => self.controls.push((row, Control::Replies(parent))),
            }
        }
        self.focus.retain(|control, _| {
            matches!(control, Control::Sort | Control::SortOption(_))
                || self.controls.iter().any(|(_, wanted)| wanted == control)
        });
        let top = self.list.logical_scroll_top();
        self.list.reset(self.rows.len());
        if !reset {
            self.list.scroll_to(top);
        }
    }

    fn handle(&mut self, control: Control, cx: &App) -> FocusHandle {
        self.focus
            .entry(control)
            .or_insert_with(|| cx.focus_handle().tab_index(0).tab_stop(true))
            .clone()
    }

    fn focus_control(&mut self, control: Control, window: &mut Window, cx: &mut App) {
        let pending = Rc::new(Cell::new(true));
        self.revealing = Some((control, pending.clone()));
        window.focus(&self.handle(control, cx), cx);
        if let Some((row, _)) = self.controls.iter().find(|(_, item)| *item == control) {
            if self.list.bounds_for_item(*row).is_some() {
                self.list.scroll_to_reveal_item(*row);
            } else {
                // ListState gives unmeasured rows zero height. Reveal by index first.
                self.list.scroll_to(ListOffset {
                    item_ix: *row,
                    offset_in_item: px(0.),
                });
            }
        }
        window.on_next_frame(move |window, _| {
            window.on_next_frame(move |_, _| pending.set(false));
        });
    }

    fn reveal_pending(&self, window: &Window) -> bool {
        self.revealing.as_ref().is_some_and(|(control, pending)| {
            pending.get()
                && self
                    .focus
                    .get(control)
                    .is_some_and(|focus| focus.is_focused(window))
        })
    }

    fn complete(
        &mut self,
        event: song_text::Event,
        session: Option<&Arc<Session>>,
        track: Option<&str>,
    ) -> Option<bool> {
        if self.pending_id() != Some(event.id) {
            return None;
        }
        let pending = self.pending.take()?;
        if !same_session(self.session.as_ref(), session) || track != Some(self.track.as_str()) {
            return None;
        }
        match event.result {
            Ok(SongText::Comments(page)) => {
                self.retry = None;
                self.apply(*page, pending.load);
            }
            Err(error) => {
                if let Load::Replies(index) = pending.load {
                    self.entries[index].error = Some(error);
                } else {
                    self.error = Some(error);
                }
                self.rebuild(false);
            }
            Ok(_) => return None,
        }
        Some(event.session_rejected)
    }

    fn insert(&mut self, mut thread: Thread, depth: usize) -> usize {
        let children = std::mem::take(&mut thread.children);
        let index = self.entries.len();
        let preview = comment_preview(&thread.comment.text);
        self.entries.push(Entry {
            thread,
            preview,
            expanded: false,
            replies_open: false,
            replies: Vec::new(),
            depth,
            error: None,
        });
        for child in children {
            let reply = self.insert(child, depth + 1);
            self.entries[index].replies.push(reply);
        }
        index
    }

    fn apply(&mut self, page: Page, load: Load) {
        let previous_count = self.entries.len();
        if let Load::Replies(index) = load {
            let depth = self.entries[index].depth + 1;
            for thread in page.comments {
                if !self.entries[index]
                    .replies
                    .iter()
                    .any(|reply| self.entries[*reply].thread.comment.id == thread.comment.id)
                {
                    let reply = self.insert(thread, depth);
                    self.entries[index].replies.push(reply);
                }
            }
            self.entries[index].thread.replies = page
                .continuation
                .filter(|_| self.entries.len() > previous_count);
            self.entries[index].error = page.message;
        } else {
            if matches!(load, Load::Initial | Load::Sort(_)) {
                self.entries.clear();
                self.roots.clear();
            }
            if let Some(count) = page.count {
                self.count = count;
            }
            if !page.sorts.is_empty() {
                self.sorts = page.sorts;
            }
            if let Load::Sort(selected) = load {
                for (index, sort) in self.sorts.iter_mut().enumerate() {
                    sort.selected = index == selected;
                }
            }
            for thread in page.comments {
                if !self
                    .roots
                    .iter()
                    .any(|root| self.entries[*root].thread.comment.id == thread.comment.id)
                {
                    let root = self.insert(thread, 0);
                    self.roots.push(root);
                }
            }
            self.continuation = page
                .continuation
                .filter(|_| load != Load::More || self.entries.len() > previous_count);
            self.message = page.message;
            self.loaded = true;
        }
        self.rebuild(matches!(load, Load::Initial | Load::Sort(_)));
    }
}

impl Music {
    #[cfg(test)]
    pub(in crate::app) fn seed_comments_for_render(&mut self) {
        let mut page = crate::comments::parse_page(
            &serde_json::from_str(include_str!("../../fixtures/comments/initial.json")).unwrap(),
            None,
        )
        .unwrap();
        page.continuation = None;
        for thread in &mut page.comments {
            thread.comment.avatar = None;
        }
        self.player_page.comments = State::new();
        self.player_page.comments.track = self.library.queue.current().unwrap().id.clone();
        self.player_page.comments.apply(page, Load::Initial);
    }

    pub(in crate::app) fn comments_reveal_pending(&self, window: &Window) -> bool {
        self.player_open
            && self.player_page.tab == PlayerTab::Comments
            && self.player_page.comments.reveal_pending(window)
    }

    pub(in crate::app) fn move_comments_focus(
        &mut self,
        reverse: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.player_open || self.player_page.tab != PlayerTab::Comments {
            return false;
        }
        let state = &mut self.player_page.comments;
        let Some(current) = state.controls.iter().position(|(_, control)| {
            state
                .focus
                .get(control)
                .is_some_and(|focus| focus.is_focused(window))
        }) else {
            return false;
        };
        let next = if reverse {
            current.checked_sub(1)
        } else {
            current
                .checked_add(1)
                .filter(|next| *next < state.controls.len())
        };
        let Some((_, control)) = next.map(|next| state.controls[next]) else {
            return false;
        };
        state.focus_control(control, window, cx);
        cx.notify();
        true
    }

    pub(super) fn load_comments(&mut self) {
        let Some(current) = self.library.queue.current() else {
            return;
        };
        let track = current.id.clone();
        let session = self.account.as_ref().map(|account| account.session.clone());
        let state = &mut self.player_page.comments;
        if state.track != track || !same_session(state.session.as_ref(), session.as_ref()) {
            if state.cancel() {
                self.player_page.worker.cancel();
            }
            *state = State::new();
            state.track = track;
            state.session = session;
        }
        if state.pending.is_none()
            && state.error.is_none()
            && let Some((load, token)) = state.retry.clone().filter(|(load, _)| {
                !matches!(load, Load::Replies(index) if state.entries[*index].error.is_some())
            })
        {
            self.request_comments(load, token);
            return;
        }
        if state.loaded || state.pending.is_some() || state.error.is_some() {
            return;
        }
        if let Some(token) = self
            .current_watch()
            .and_then(|watch| watch.comments.clone())
        {
            self.request_comments(Load::Initial, token);
        }
    }

    fn request_comments(&mut self, load: Load, token: Continuation) {
        let state = &mut self.player_page.comments;
        if state.pending.is_some() {
            return;
        }
        state.error = None;
        state.retry = Some((load, token.clone()));
        if let Load::Replies(index) = load {
            state.entries[index].error = None;
        }
        match self
            .player_page
            .worker
            .load(TextQuery::Comments(token), state.session.clone())
        {
            Ok(id) => {
                self.player_page.latest_request = id;
                state.pending = Some(Pending { id, load });
            }
            Err(error) => state.error = Some(format!("{error:#}")),
        }
    }

    pub(super) fn comments_loaded(&mut self, event: song_text::Event, cx: &mut Context<Self>) {
        let session = self.account.as_ref().map(|account| &account.session);
        let track = self.library.queue.current().map(|track| track.id.as_str());
        let Some(rejected) = self.player_page.comments.complete(event, session, track) else {
            return;
        };
        if rejected {
            self.refresh_session(false, cx);
        }
        cx.notify();
    }

    pub(super) fn comments_tab(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let state = &self.player_page.comments;
        if !state.loaded {
            if let Some(error) = &state.error {
                return div()
                    .pt(px(16.))
                    .child(error.clone())
                    .child(
                        div()
                            .id("retry-comments")
                            .keyboard_control(self.theme)
                            .cursor_pointer()
                            .py(px(8.))
                            .child("Retry")
                            .on_click(cx.listener(|view, _, _, cx| {
                                if let Some((load, token)) = view.player_page.comments.retry.clone()
                                {
                                    view.request_comments(load, token);
                                } else {
                                    view.player_page.comments.error = None;
                                }
                                cx.notify();
                            })),
                    )
                    .into_any_element();
            }
            let message = if self
                .current_watch()
                .is_some_and(|watch| watch.comments.is_none())
            {
                "Comments are turned off"
            } else {
                "Loading…"
            };
            return related_message(message.into(), self.theme);
        }
        let count = state.count.clone();
        let sorts = state
            .sorts
            .iter()
            .enumerate()
            .map(|(index, sort)| (index, sort.label.clone(), sort.selected))
            .collect::<Vec<_>>();
        let selected = state
            .sorts
            .iter()
            .find(|sort| sort.selected)
            .map(|sort| sort.label.clone())
            .unwrap_or_else(|| "Top comments".into());
        let menu = state.menu;
        let loading = state.pending.is_some();
        let message = state
            .message
            .clone()
            .or_else(|| (state.entries.is_empty() && !loading).then(|| "No comments yet".into()));
        let rows = state.list.clone();
        let sort_focus = self.player_page.comments.handle(Control::Sort, cx);
        let option_focus = sorts
            .iter()
            .map(|(index, _, _)| {
                self.player_page
                    .comments
                    .handle(Control::SortOption(*index), cx)
            })
            .collect::<Vec<_>>();
        div()
            .id("comments-tab")
            .on_key_down(cx.listener(|view, event: &KeyDownEvent, window, cx| {
                let state = &mut view.player_page.comments;
                if !state.menu || event.keystroke.modifiers.modified() {
                    return;
                }
                let count = state.sorts.len();
                if count == 0 {
                    return;
                }
                let current = (0..count).find(|index| {
                    state
                        .focus
                        .get(&Control::SortOption(*index))
                        .is_some_and(|focus| focus.is_focused(window))
                });
                let next = match event.keystroke.key.as_str() {
                    "escape" => {
                        state.menu = false;
                        window.focus(&state.handle(Control::Sort, cx), cx);
                        None
                    }
                    "down" => Some(current.map_or(0, |index| (index + 1) % count)),
                    "up" => Some(current.map_or(count - 1, |index| (index + count - 1) % count)),
                    "home" => Some(0),
                    "end" => Some(count - 1),
                    _ => return,
                };
                if let Some(index) = next {
                    window.focus(&state.handle(Control::SortOption(index), cx), cx);
                }
                cx.stop_propagation();
                cx.notify();
            }))
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(
                div()
                    .py(px(16.))
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .child(
                        div()
                            .flex_1()
                            .font_weight(FontWeight::BOLD)
                            .text_size(px(16.))
                            .child(count),
                    )
                    .when(!sorts.is_empty(), |header| {
                        header.child(
                            div()
                                .id("comment-sort")
                                .track_focus(&sort_focus)
                                .keyboard_control(self.theme)
                                .cursor_pointer()
                                .px(px(4.))
                                .py(px(6.))
                                .text_size(px(13.))
                                .child(format!("{selected} ▾"))
                                .on_click(cx.listener(|view, _, _, cx| {
                                    view.player_page.comments.menu =
                                        !view.player_page.comments.menu;
                                    cx.notify();
                                })),
                        )
                    }),
            )
            .when(menu, |body| {
                body.child(
                    div()
                        .pb(px(12.))
                        .flex()
                        .flex_col()
                        .children(sorts.into_iter().map(|(index, label, selected)| {
                            div()
                                .id(("comment-sort-option", index))
                                .track_focus(&option_focus[index])
                                .keyboard_control(self.theme)
                                .cursor_pointer()
                                .px(px(12.))
                                .py(px(10.))
                                .bg(if selected {
                                    self.theme.raised
                                } else {
                                    self.theme.bg
                                })
                                .text_size(px(14.))
                                .child(label)
                                .on_click(cx.listener(move |view, _, _, cx| {
                                    view.player_page.comments.menu = false;
                                    if let Some(sort) = view.player_page.comments.sorts.get(index) {
                                        let token = sort.continuation.clone();
                                        if view.player_page.comments.cancel() {
                                            view.player_page.worker.cancel();
                                        }
                                        view.request_comments(Load::Sort(index), token);
                                    }
                                    cx.notify();
                                }))
                        })),
                )
            })
            .when_some(message, |body, message| {
                body.child(
                    div()
                        .py(px(16.))
                        .text_size(px(14.))
                        .text_color(self.theme.muted)
                        .child(message),
                )
            })
            .child(
                list(
                    rows,
                    cx.processor(|view, index, _, cx| view.comment_list_row(index, cx)),
                )
                .flex_1()
                .min_h_0(),
            )
            .into_any_element()
    }

    fn comment_list_row(&mut self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        #[cfg(test)]
        self.player_page.comments.rendered.insert(index);
        let Some(row) = self.player_page.comments.rows.get(index).copied() else {
            return div().into_any_element();
        };
        match row {
            Row::Comment(index) => self.comment_row(index, cx),
            Row::More | Row::Replies(_) => {
                let state = &self.player_page.comments;
                let (load, token, error) = match row {
                    Row::Replies(parent) => (
                        Load::Replies(parent),
                        state.entries[parent].thread.replies.clone(),
                        state.entries[parent].error.clone(),
                    ),
                    _ => {
                        let (load, token) = state
                            .retry
                            .clone()
                            .map(|(load, token)| (load, Some(token)))
                            .unwrap_or((Load::More, state.continuation.clone()));
                        (load, token, state.error.clone())
                    }
                };
                if error.is_none()
                    && state.pending.is_none()
                    && let Some(token) = token.clone()
                {
                    // Queue after layout; the list's measurement pass must not perform IO.
                    let view = cx.weak_entity();
                    let track = state.track.clone();
                    cx.defer(move |cx| {
                        let _ = view.update(cx, |view, cx| {
                            if view.player_open
                                && view.player_page.tab == PlayerTab::Comments
                                && view.player_page.comments.track == track
                            {
                                view.request_comments(load, token);
                                cx.notify();
                            }
                        });
                    });
                }
                let control = match row {
                    Row::Replies(parent) => Control::Replies(parent),
                    _ => Control::More,
                };
                let focus = self.player_page.comments.handle(control, cx);
                div()
                    .id(("comments-more", index))
                    .track_focus(&focus)
                    .keyboard_control(self.theme)
                    .cursor_pointer()
                    .py(px(16.))
                    .text_size(px(13.))
                    .text_color(self.theme.muted)
                    .child(
                        error
                            .as_ref()
                            .map(|error| format!("{error}. Retry"))
                            .unwrap_or_else(|| "Loading…".into()),
                    )
                    .on_click(cx.listener(move |view, _, _, cx| {
                        if let Some(token) = token.clone() {
                            view.request_comments(load, token);
                            cx.notify();
                        }
                    }))
                    .into_any_element()
            }
        }
    }

    fn comment_row(&mut self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let entry = &self.player_page.comments.entries[index];
        let comment = entry.thread.comment.clone();
        let expanded = entry.expanded;
        let depth = entry.depth;
        let replies_open = entry.replies_open;
        let replies_label = entry.thread.replies_label.clone();
        let has_replies = entry.thread.replies.is_some() || !entry.replies.is_empty();
        let long = entry.preview.is_some();
        let content = if expanded {
            comment.text.clone()
        } else {
            entry
                .preview
                .clone()
                .unwrap_or_else(|| comment.text.clone())
        };
        let read_focus = long.then(|| self.player_page.comments.handle(Control::Read(index), cx));
        let reply_focus =
            has_replies.then(|| self.player_page.comments.handle(Control::Toggle(index), cx));
        let id = SharedString::from(comment.id.clone());
        let art = self.art(
            comment.avatar.as_deref(),
            if depth > 0 { 24. } else { 32. },
            ArtworkShape::Circle,
        );
        div()
            .w_full()
            .py(px(12.))
            .when(depth > 0, |row| row.pl(px((depth.min(3) * 32) as f32)))
            .flex()
            .items_start()
            .gap(px(16.))
            .child(art)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(5.))
                    .text_size(px(13.))
                    .line_height(px(18.))
                    .when_some(comment.pinned, |body, pinned| {
                        body.child(
                            div()
                                .text_size(px(12.))
                                .text_color(self.theme.muted)
                                .child(pinned),
                        )
                    })
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap(px(6.))
                            .child(div().font_weight(FontWeight::MEDIUM).child(comment.author))
                            .when(comment.creator, |line| {
                                line.child(div().text_color(self.theme.muted).child("Creator"))
                            })
                            .when(comment.verified, |line| {
                                line.child(div().text_color(self.theme.muted).child("✓"))
                            })
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(self.theme.muted)
                                    .child(comment.published),
                            ),
                    )
                    .child(
                        div()
                            .whitespace_normal()
                            .when(long && !expanded, |text| {
                                text.line_clamp(4).max_h(px(72.)).overflow_hidden()
                            })
                            .child(content),
                    )
                    .when(long, |body| {
                        body.child(
                            div()
                                .id((ElementId::Name("comment-expand".into()), id.clone()))
                                .when_some(read_focus, |body, focus| body.track_focus(&focus))
                                .keyboard_control(self.theme)
                                .cursor_pointer()
                                .text_color(self.theme.muted)
                                .child(if expanded { "Show less" } else { "Read more" })
                                .on_click(cx.listener(move |view, _, _, cx| {
                                    let state = &mut view.player_page.comments;
                                    state.entries[index].expanded = !expanded;
                                    state.list.remeasure();
                                    cx.notify();
                                })),
                        )
                    })
                    .when(!comment.likes.is_empty(), |body| {
                        body.child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(8.))
                                .py(px(3.))
                                .child(icon(
                                    if comment.liked {
                                        "thumb-up-filled"
                                    } else {
                                        "thumb-up"
                                    },
                                    16.,
                                    if comment.liked {
                                        self.theme.text
                                    } else {
                                        self.theme.muted
                                    },
                                ))
                                .child(comment.likes),
                        )
                    })
                    .when(has_replies, |body| {
                        body.child(
                            div()
                                .id((ElementId::Name("comment-replies".into()), id))
                                .when_some(reply_focus, |body, focus| body.track_focus(&focus))
                                .keyboard_control(self.theme)
                                .cursor_pointer()
                                .text_color(rgb(0x3ea6ff))
                                .py(px(6.))
                                .child(if replies_open {
                                    "▴ Hide replies".to_owned()
                                } else {
                                    format!(
                                        "▾ {}",
                                        if replies_label.is_empty() {
                                            "Replies"
                                        } else {
                                            &replies_label
                                        }
                                    )
                                })
                                .on_click(cx.listener(move |view, _, _, cx| {
                                    view.player_page.comments.entries[index].replies_open =
                                        !replies_open;
                                    view.player_page.comments.rebuild(false);
                                    cx.notify();
                                })),
                        )
                    }),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::{Control, Load, Pending, PlayerPage, PlayerTab, State};
    use crate::app::native_tests::{
        isolate,
        render::{capture, wait_for_presented_frame},
    };
    use crate::{
        app::{CatalogTarget, Music, Startup},
        auth::Session,
        song_text::{self, Content as SongText},
    };
    use anyhow::{Context as _, Result, ensure};
    use gpui::{
        AppContext, AsyncApp, Bounds, KeyUpEvent, Keystroke, PlatformInput, TitlebarOptions,
        WindowBounds, WindowHandle, WindowKind, WindowOptions, point, px, size,
    };
    use std::{
        sync::Arc,
        time::{Duration, Instant},
    };

    fn session(identity: &str) -> Arc<Session> {
        Arc::new(
            serde_json::from_value(serde_json::json!({
                "cookie":"synthetic", "playback_cookie":"synthetic", "authuser":"0",
                "data_sync_id":identity, "client_version":"synthetic"
            }))
            .unwrap(),
        )
    }

    #[test]
    fn account_reset_cancels_comments_and_discards_their_controls_and_tokens() {
        let (worker, _events) = song_text::Worker::start();
        let mut page = PlayerPage::new(worker);
        page.comments.session = Some(session("old"));
        page.comments.pending = Some(Pending {
            id: 8,
            load: Load::Initial,
        });
        page.comments.apply(
            crate::comments::parse_page(
                &serde_json::from_str(include_str!("../../fixtures/comments/initial.json"))
                    .unwrap(),
                None,
            )
            .unwrap(),
            Load::Initial,
        );
        page.reset_account();
        assert!(page.comments.pending.is_none());
        assert!(page.comments.entries.is_empty() && page.comments.sorts.is_empty());
        assert!(page.comments.continuation.is_none() && page.comments.controls.is_empty());
        assert!(page.comments.session.is_none());
    }

    #[test]
    fn superseded_track_and_account_completions_cannot_show_comments_or_refresh_the_new_session() {
        let original = session("first");
        let second = session("second");
        let pending = || {
            let mut state = State::new();
            state.track = "first-track".into();
            state.session = Some(original.clone());
            state.pending = Some(Pending {
                id: 7,
                load: Load::Initial,
            });
            state
        };
        let rejected = || song_text::Event {
            id: 7,
            result: Err("Rejected old account".into()),
            session_rejected: true,
        };
        let mut state = pending();
        assert_eq!(
            state.complete(rejected(), Some(&second), Some("first-track")),
            None
        );
        assert!(state.error.is_none());
        let mut state = pending();
        assert_eq!(
            state.complete(rejected(), Some(&original), Some("new-track")),
            None
        );
        assert!(state.error.is_none());
        let mut state = pending();
        assert_eq!(
            state.complete(rejected(), Some(&original), Some("first-track")),
            Some(true)
        );
        assert!(state.error.is_some());
    }

    #[test]
    fn collapsed_comments_bound_text_without_splitting_unicode() {
        assert_eq!(
            super::comment_preview("one\ntwo\nthree\nfour\nfive").as_deref(),
            Some("one\ntwo\nthree\nfour…")
        );
        assert!(super::comment_preview("A short comment.").is_none());
        let preview = super::comment_preview(&"é".repeat(1000)).unwrap();
        assert_eq!(preview.chars().count(), 181);
        assert!(preview.ends_with('…'));
    }

    #[test]
    fn repeated_pages_stop_paging_instead_of_refetching_forever() {
        let page = || {
            crate::comments::parse_page(
                &serde_json::from_str(include_str!("../../fixtures/comments/initial.json"))
                    .unwrap(),
                None,
            )
            .unwrap()
        };
        let mut state = State::new();
        state.apply(page(), Load::Initial);
        assert!(state.continuation.is_some());
        state.apply(page(), Load::More);
        assert_eq!(state.roots.len(), 2);
        assert!(state.continuation.is_none());
    }

    fn cpu_seconds() -> f64 {
        let mut time = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        assert_eq!(
            unsafe { libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut time) },
            0
        );
        time.tv_sec as f64 + time.tv_nsec as f64 / 1e9
    }

    async fn key(window: WindowHandle<Music>, cx: &mut AsyncApp, key: &str) {
        cx.update_window(window.into(), |_, window, cx| {
            let keystroke = Keystroke::parse(key).unwrap();
            window.dispatch_keystroke(keystroke.clone(), cx);
            window.dispatch_event(PlatformInput::KeyUp(KeyUpEvent { keystroke }), cx);
        })
        .unwrap();
        wait_for_presented_frame(window, cx).await;
    }

    #[test]
    #[ignore = "requires isolated X11 desktop/private D-Bus, yt-dlp and public playback; set YT_MUSIC_NATIVE_TESTS=1"]
    fn native_comments_read_controls_and_playback_cost() -> Result<()> {
        isolate()?;
        let (finished, result) = async_channel::bounded::<Result<()>>(1);
        gpui_platform::application().with_assets(crate::assets::Assets).run(move |cx| {
            cx.text_system().add_fonts(vec![std::borrow::Cow::Borrowed(include_bytes!("../../../assets/fonts/Roboto.ttf").as_slice())]).unwrap();
            crate::input::init(cx);
            let window = cx.open_window(WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::new(point(px(20.), px(20.)), size(px(1320.), px(860.))))),
                titlebar: Some(TitlebarOptions { title: Some("Music comments integration test".into()), ..Default::default() }),
                focus: false,
                kind: WindowKind::PopUp,
                ..Default::default()
            }, |window, cx| cx.new(|cx| {
                let mut music = Music::new(Startup::Guest, window, cx);
                for target in CatalogTarget::ALL { music.catalog_requests.cancel(target); }
                music.library.volume = 0.;
                music.library.video = false;
                music.library.autoplay = false;
                music.player.as_ref().unwrap().set_volume(0.).unwrap();
                music.play_track(serde_json::from_value(serde_json::json!({"id":"dQw4w9WgXcQ", "title":"Never Gonna Give You Up", "artist":"Rick Astley"})).unwrap(), cx);
                music
            })).unwrap();
            cx.spawn(async move |cx| {
                let outcome: Result<()> = async {
                    let deadline = Instant::now() + Duration::from_secs(90);
                    loop {
                        let ready = window.update(cx, |music, _, _| music.position > 0. && music.current_watch().is_some())?;
                        if ready { break; }
                        ensure!(Instant::now() < deadline, "Guest playback/watch did not start");
                        cx.background_executor().timer(Duration::from_millis(50)).await;
                    }
                    for tab in [PlayerTab::UpNext, PlayerTab::Comments, PlayerTab::UpNext, PlayerTab::Comments] {
                        window.update(cx, |music, _, cx| music.select_player_tab(tab, cx))?;
                        let deadline = Instant::now() + Duration::from_secs(30);
                        loop {
                            let ready = window.update(cx, |music, _, _| tab != PlayerTab::Comments || (music.player_page.comments.loaded && music.player_page.comments.pending.is_none()))?;
                            if ready { break; }
                            let error = window.update(cx, |music, _, _| music.player_page.comments.error.clone())?;
                            ensure!(error.is_none(), "Comments failed: {error:?}");
                            ensure!(Instant::now() < deadline, "Comments did not load");
                            cx.background_executor().timer(Duration::from_millis(50)).await;
                        }
                        // Artwork decoding and catalog completions are outside the steady sample.
                        cx.background_executor().timer(Duration::from_secs(2)).await;
                        let start = Instant::now();
                        let cpu = cpu_seconds();
                        let before = window.update(cx, |music, _, cx| music.content_view.read(cx).renders)?;
                        cx.background_executor().timer(Duration::from_secs(5)).await;
                        let cost = 100. * (cpu_seconds() - cpu) / start.elapsed().as_secs_f64();
                        let after = window.update(cx, |music, _, cx| music.content_view.read(cx).renders)?;
                        println!("COMMENTS_PLAYING_CPU tab={tab:?} percent_one_core={cost:.2} body_renders={}", after-before);
                    }
                    wait_for_presented_frame(window, cx).await;
                    window.update(cx, |music, window, _| {
                        assert!(!music.player_page.comments.entries.is_empty());
                        let image = capture(window, "comments-live");
                        let width = image.width();
                        image::imageops::crop_imm(&image, width.saturating_sub(485), 65, 460.min(width), image.height().saturating_sub(160)).to_image()
                            .save(std::path::PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").unwrap()).join("comments-native-crop.png")).unwrap();
                    })?;
                    window.update(cx, |music, window, cx| {
                        let focus = music.player_page.comments.handle(Control::Sort, cx);
                        window.focus(&focus, cx);
                    })?;
                    key(window, cx, "enter").await;
                    window.update(cx, |music, window, cx| {
                        assert!(music.player_page.comments.menu);
                        let focus = music.player_page.comments.handle(Control::SortOption(1), cx);
                        window.focus(&focus, cx);
                    })?;
                    key(window, cx, "enter").await;
                    let deadline = Instant::now() + Duration::from_secs(30);
                    loop {
                        let sorted = window.update(cx, |music, _, _| {
                            assert!(music.player_page.comments.error.is_none());
                            music.player_page.comments.pending.is_none() && music.player_page.comments.sorts[1].selected
                        })?;
                        if sorted { break; }
                        ensure!(Instant::now() < deadline, "Newest sort did not complete");
                        cx.background_executor().timer(Duration::from_millis(50)).await;
                    }
                    // Exercise read-only controls against deterministic content without any writes.
                    window.update(cx, |music, window, cx| {
                        music.stop_playback(cx);
                        music.seed_comments_for_render();
                        let state = &mut music.player_page.comments;
                        let focus = state.handle(Control::Read(0), cx);
                        window.focus(&focus, cx);
                        cx.notify();
                    })?;
                    wait_for_presented_frame(window, cx).await;
                    key(window, cx, "enter").await;
                    window.update(cx, |music, _, _| assert!(music.player_page.comments.entries[0].expanded))?;
                    key(window, cx, "enter").await;
                    window.update(cx, |music, _, _| assert!(!music.player_page.comments.entries[0].expanded))?;
                    key(window, cx, "tab").await;
                    window.update(cx, |music, window, _| {
                        assert!(music.player_page.comments.focus[&Control::Toggle(0)].is_focused(window));
                    })?;
                    // Prevent the synthetic reply continuation from reaching the real service.
                    window.update(cx, |music, _, _| music.player_page.comments.pending = Some(Pending { id: u64::MAX, load: Load::Replies(0) }))?;
                    key(window, cx, "enter").await;
                    window.update(cx, |music, _, cx| {
                        assert!(music.player_page.comments.entries[0].replies_open);
                        music.comments_loaded(song_text::Event { id: u64::MAX, result: Ok(SongText::Comments(Box::new(crate::comments::parse_page(&serde_json::from_str(include_str!("../../fixtures/comments/replies.json")).unwrap(), None).unwrap()))), session_rejected: false }, cx);
                        // The fixture's next page is deliberately not fetched in this render check.
                        music.player_page.comments.entries[0].thread.replies = None;
                        music.player_page.comments.rebuild(false);
                        cx.notify();
                    })?;
                    wait_for_presented_frame(window, cx).await;
                    window.update(cx, |music, window, _| {
                        assert_eq!(music.player_page.comments.entries[0].replies.len(), 1);
                        capture(window, "comments-expanded-replies");
                    })?;
                    key(window, cx, "enter").await;
                    window.update(cx, |music, _, _| assert!(!music.player_page.comments.entries[0].replies_open))?;
                    window.update(cx, |music, _, cx| {
                        let state = &mut music.player_page.comments;
                        let template = state.entries[0].thread.comment.clone();
                        state.entries = (0..500).map(|index| {
                            let mut comment = template.clone(); comment.id = format!("virtual-{index}");
                            let preview = super::comment_preview(&comment.text);
                            super::Entry { preview, thread: crate::comments::Thread { comment, replies_label: String::new(), replies: None, children: Vec::new() }, expanded: false, replies_open: false, replies: Vec::new(), depth: 0, error: None }
                        }).collect();
                        state.roots = (0..500).collect();
                        state.continuation = None;
                        state.rebuild(true);
                        state.rendered.clear();
                        cx.notify();
                    })?;
                    wait_for_presented_frame(window, cx).await;
                    window.update(cx, |music, window, cx| {
                        let state = &mut music.player_page.comments;
                        assert!(state.rendered.len() < 25, "Rendered {} of 500 comments", state.rendered.len());
                        state.focus_control(Control::Read(300), window, cx);
                        cx.notify();
                    })?;
                    wait_for_presented_frame(window, cx).await;
                    window.update(cx, |music, window, _| {
                        let state = &music.player_page.comments;
                        assert!(state.focus[&Control::Read(300)].is_focused(window));
                        assert!(state.rendered.contains(&300), "Focused comment must enter the rendered list");
                    })?;
                    key(window, cx, "tab").await;
                    window.update(cx, |music, window, _| {
                        assert!(music.player_page.comments.focus.get(&Control::Read(301)).is_some_and(|focus| focus.is_focused(window)));
                        assert!(music.player_page.comments.list.logical_scroll_top().item_ix >= 299);
                    })?;
                    println!("COMMENTS_NATIVE read_more=passed replies=passed sort=passed keyboard=passed virtual_500=passed guest=passed");
                    Ok(())
                }.await;
                let _ = finished.send(outcome).await;
                cx.update(|cx| cx.quit());
            }).detach();
        });
        result
            .recv_blocking()
            .context("Native comments check did not finish")?
    }
}
