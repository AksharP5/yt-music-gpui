//! Up next: the playing-from header, autoplay switch, and the reorderable queue.

use super::*;

/// A 56 px row plus its 1 px divider, as on music.youtube.com.
const ROW_HEIGHT: f32 = 57.;

/// A queue row being dragged from `from`, previewed at `to` until it is dropped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::app) struct Reorder {
    pub(in crate::app) from: usize,
    pub(in crate::app) to: usize,
}

impl Reorder {
    /// The queue index shown at `row` while the dragged row previews at `to`.
    fn source(self, row: usize) -> usize {
        let Self { from, to } = self;
        match row {
            row if row == to => from,
            row if from < to && (from..to).contains(&row) => row + 1,
            row if to < from && (to + 1..=from).contains(&row) => row - 1,
            row => row,
        }
    }
}

enum UpNextRow {
    Queued(usize),
    AutoplayLabel,
    Autoplay(usize),
}

impl Music {
    pub(in crate::app) fn up_next(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let queued = self.library.queue.tracks.len();
        let autoplay = self.autoplay_tracks.len();
        let rows = queued + if autoplay > 0 { autoplay + 1 } else { 0 };
        let bounds = self.player_page.queue_bounds.clone();
        let theme = self.theme;
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(
                div()
                    .pt(px(16.))
                    .flex()
                    .items_center()
                    .gap(px(16.))
                    .child(div().flex_1().min_w_0().flex().flex_col().when_some(
                        self.library.queue.playing_from.clone(),
                        |header, source| {
                            header
                                .child(
                                    div()
                                        .text_size(px(12.))
                                        .line_height(px(18.))
                                        .text_color(gpui::white().opacity(0.7))
                                        .child("Playing from"),
                                )
                                .child(
                                    div()
                                        .truncate()
                                        .text_size(px(14.))
                                        .line_height(px(20.))
                                        .font_weight(FontWeight::MEDIUM)
                                        .child(source),
                                )
                        },
                    ))
                    .child(
                        div()
                            .id("save-queue")
                            .keyboard_control(theme)
                            .h(px(32.))
                            .pl(px(12.))
                            .pr(px(8.))
                            .flex_shrink_0()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .rounded_full()
                            .bg(theme.text)
                            .focus_visible(|pill| pill.border_color(rgb(0x3ea6ff)))
                            .text_color(rgb(0x030303))
                            .text_size(px(14.))
                            .font_weight(FontWeight::MEDIUM)
                            .cursor_pointer()
                            .child(icon("menu-save-playlist", 24., rgb(0x030303).into()))
                            .child("Save")
                            .on_click(cx.listener(|view, _, window, cx| {
                                if view.account.is_none() {
                                    view.connect_account(window, cx);
                                    return;
                                }
                                view.overlay_return_focus = window.focused(cx);
                                window.focus(&view.overlay_focus, cx);
                                let tracks = view.library.queue.tracks.clone();
                                view.playlist_picker(tracks, cx);
                            })),
                    ),
            )
            .child(self.autoplay_switch(cx))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .relative()
                    .child(
                        canvas(move |rect, _, _| bounds.set(rect), |_, _, _, _| {})
                            .absolute()
                            .size_full(),
                    )
                    .child(
                        uniform_list(
                            "up-next",
                            rows,
                            cx.processor(move |view, range: std::ops::Range<usize>, window, cx| {
                                // music.youtube.com enlarges row text at its large breakpoint.
                                let text = if window.viewport_size().width >= px(1364.) {
                                    16.
                                } else {
                                    14.
                                };
                                range
                                    .map(|row| match view.up_next_row(row) {
                                        UpNextRow::Queued(row) => view.queue_row(row, text, cx),
                                        UpNextRow::AutoplayLabel => div()
                                            .w_full()
                                            .h(px(ROW_HEIGHT))
                                            .px(px(12.))
                                            .flex()
                                            .items_center()
                                            .text_size(px(text))
                                            .text_color(view.theme.muted)
                                            .child("Autoplay is on")
                                            .into_any_element(),
                                        UpNextRow::Autoplay(index) => {
                                            view.autoplay_row(index, text, cx)
                                        }
                                    })
                                    .collect::<Vec<_>>()
                            }),
                        )
                        .track_scroll(&self.player_page.queue_scroll)
                        .size_full(),
                    ),
            )
            .into_any_element()
    }

    fn up_next_row(&self, row: usize) -> UpNextRow {
        let queued = self.library.queue.tracks.len();
        match row.checked_sub(queued) {
            None => UpNextRow::Queued(row),
            Some(0) => UpNextRow::AutoplayLabel,
            Some(index) => UpNextRow::Autoplay(index - 1),
        }
    }

    fn autoplay_switch(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let on = self.library.autoplay;
        let blue = rgb(0x3ea6ff);
        div()
            .id("autoplay-toggle")
            .keyboard_control(self.theme)
            .mt(px(8.))
            .px(px(8.))
            .py(px(8.))
            .flex()
            .items_center()
            .gap(px(16.))
            .cursor_pointer()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .child(
                        div()
                            .text_size(px(14.))
                            .font_weight(FontWeight::MEDIUM)
                            .child("Autoplay"),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_size(px(12.))
                            .text_color(self.theme.muted)
                            .child("Add similar content to the end of the queue"),
                    ),
            )
            .child(
                div()
                    .w(px(36.))
                    .h(px(14.))
                    .mr(px(12.))
                    .relative()
                    .rounded_full()
                    .bg(if on {
                        Hsla::from(blue).opacity(0.5)
                    } else {
                        gpui::white().opacity(0.3)
                    })
                    .child(
                        div()
                            .absolute()
                            .top(px(-3.))
                            .left(px(if on { 16. } else { 0. }))
                            .size(px(20.))
                            .rounded_full()
                            .bg(if on { blue } else { rgb(0xf1f1f1) }),
                    ),
            )
            .on_click(cx.listener(|view, _, _, cx| view.toggle_autoplay(cx)))
    }

    fn queue_row(&mut self, row: usize, text: f32, cx: &mut Context<Self>) -> AnyElement {
        let reorder = self.player_page.reorder;
        let index = reorder.map_or(row, |reorder| reorder.source(row));
        let dragged = reorder.is_some_and(|reorder| reorder.to == row);
        let Some(mut track) = self.library.queue.tracks.get(index).cloned() else {
            return div().h(px(ROW_HEIGHT)).into_any_element();
        };
        let active = self.library.queue.index == Some(index);
        if active && track.duration.is_none() {
            track.duration = self
                .current_watch()
                .and_then(|watch| watch.current.as_ref()?.duration);
        }
        let playing = active && self.playback != Playback::Stopped && !self.playback.is_paused();
        // Showing the handle shifts the row, as on the web. GPUI evaluates `display` in both
        // prepaint and paint, so it follows stored hover state rather than `group_hover`.
        let show_handle = dragged || self.player_page.hovered_row == Some(index);
        let handle = show_handle.then(|| {
            div()
                .id(("queue-handle", index))
                .size(px(24.))
                .mr(px(8.))
                .flex()
                .flex_shrink_0()
                .items_center()
                .justify_center()
                .cursor_grab()
                .child(icon("drag-handle", 24., rgb(0xf1f1f1).into()))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |view, event: &MouseDownEvent, _, cx| {
                        cx.stop_propagation();
                        view.player_page.reorder = Some(Reorder {
                            from: index,
                            to: index,
                        });
                        view.player_page.drag_pointer = event.position;
                        view.drag = Some(Drag::Queue);
                        cx.notify();
                    }),
                )
        });
        let menu = self.row_menu(("queue-menu", index), cx, move |view, window, cx| {
            view.open_queue_menu(index, window, cx);
        });
        self.track_row(
            ("queue-row", index),
            &track,
            text,
            active.then_some(playing),
            dragged,
            handle,
            menu,
        )
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(move |view, _, window, cx| {
                cx.stop_propagation();
                view.open_queue_menu(index, window, cx);
            }),
        )
        .on_hover(cx.listener(move |view, hovered: &bool, _, cx| {
            let hovered_row = &mut view.player_page.hovered_row;
            if *hovered && *hovered_row != Some(index) {
                *hovered_row = Some(index);
                cx.notify();
            } else if !*hovered && *hovered_row == Some(index) {
                *hovered_row = None;
                cx.notify();
            }
        }))
        .on_click(cx.listener(move |view, _, _, cx| {
            if view.library.queue.index == Some(index) {
                view.toggle_play(cx);
            } else {
                view.library.queue.select(index);
                view.play_current(cx);
            }
        }))
        .into_any_element()
    }

    fn open_queue_menu(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(track) = self.library.queue.tracks.get(index).cloned() else {
            return;
        };
        self.open_track_menu(track, None, window, cx);
        if let Some(Overlay::Track { queue_index, .. }) = &mut self.overlay {
            *queue_index = Some(index);
        }
    }

    fn autoplay_row(&mut self, index: usize, text: f32, cx: &mut Context<Self>) -> AnyElement {
        let Some(track) = self.autoplay_tracks.get(index).cloned() else {
            return div().h(px(ROW_HEIGHT)).into_any_element();
        };
        let menu_track = track.clone();
        let menu = self.row_menu(("autoplay-menu", index), cx, move |view, window, cx| {
            view.open_track_menu(menu_track.clone(), None, window, cx)
        });
        let context_track = track.clone();
        self.track_row(
            ("autoplay-row", index),
            &track,
            text,
            None,
            false,
            None,
            menu,
        )
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(move |view, _, window, cx| {
                cx.stop_propagation();
                view.open_track_menu(context_track.clone(), None, window, cx);
            }),
        )
        .on_click(cx.listener(move |view, _, _, cx| view.play_recommendation(index, cx)))
        .into_any_element()
    }

    /// The shared row: thumbnail with its play state, title, artist and duration.
    #[allow(clippy::too_many_arguments)]
    fn track_row(
        &mut self,
        id: (&'static str, usize),
        track: &Track,
        text: f32,
        // `Some(playing)` for the current track.
        current: Option<bool>,
        dragged: bool,
        handle: Option<Stateful<Div>>,
        menu: AnyElement,
    ) -> Stateful<Div> {
        let active = current.is_some();
        let playing = current == Some(true);
        let art = self.art(track.thumbnail.as_deref(), 32., ArtworkShape::Square);
        let overlay = |name: &'static str| {
            div()
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(gpui::black().opacity(0.8))
                .child(icon(name, 24., gpui::white()))
        };
        let duration = track
            .duration
            .map(|seconds| clock(seconds as f64))
            .unwrap_or_default();
        div()
            .id(id)
            .keyboard_control(self.theme)
            .group("up-next-row")
            .relative()
            .w_full()
            .h(px(ROW_HEIGHT))
            .px(px(8.))
            .flex()
            .items_center()
            .text_size(px(text))
            .line_height(px(text * 1.2))
            .cursor_pointer()
            .when(active, |row| row.bg(gpui::white().opacity(0.1)))
            .when(dragged, |row| row.bg(self.theme.raised).shadow_lg())
            .child(
                div()
                    .absolute()
                    .left_0()
                    .right_0()
                    .bottom_0()
                    .h(px(1.))
                    .bg(gpui::white().opacity(0.1)),
            )
            .children(handle)
            .child(
                div()
                    .relative()
                    .size(px(32.))
                    .flex_shrink_0()
                    .child(art)
                    .when(active, |thumb| {
                        thumb
                            .child(
                                overlay("volume")
                                    .group_hover("up-next-row", |overlay| overlay.invisible()),
                            )
                            .child(
                                overlay(if playing { "pause" } else { "play" })
                                    .invisible()
                                    .group_hover("up-next-row", |overlay| overlay.visible()),
                            )
                    })
                    .when(!active, |thumb| {
                        thumb.child(
                            overlay("play")
                                .invisible()
                                .group_hover("up-next-row", |overlay| overlay.visible()),
                        )
                    }),
            )
            .child(
                div()
                    .ml(px(16.))
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .child(
                        div()
                            .truncate()
                            .font_weight(FontWeight::MEDIUM)
                            .child(track.title.clone()),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_color(self.theme.muted)
                            .child(track.artist.clone()),
                    ),
            )
            .child(
                div()
                    .ml(px(8.))
                    .w(px(56.))
                    .h(px(36.))
                    .flex_shrink_0()
                    .relative()
                    .child(
                        div()
                            .absolute()
                            .inset_0()
                            .flex()
                            .items_center()
                            .justify_end()
                            .text_color(self.theme.muted)
                            .group_hover("up-next-row", |duration| duration.invisible())
                            .child(duration),
                    )
                    .child(div().absolute().top_0().right_0().child(menu)),
            )
    }

    /// The hover menu button that replaces the duration, as on the web. Keyboard focus
    /// reveals it too, over the duration it would cover.
    fn row_menu(
        &self,
        id: (&'static str, usize),
        cx: &mut Context<Self>,
        open: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> AnyElement {
        let theme = self.theme;
        self.menu_button(id, cx, open)
            .size(px(36.))
            .opacity(0.)
            .group_hover("up-next-row", |menu| menu.opacity(1.))
            .focus_visible(move |menu| menu.opacity(1.).bg(theme.raised).border_color(theme.text))
            .into_any_element()
    }

    /// Follow the pointer while reordering, scrolling the list when it nears either edge.
    pub(in crate::app) fn drag_queue_row(
        &mut self,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        if self.player_page.reorder.is_none() {
            return;
        }
        self.player_page.drag_pointer = position;
        self.retarget_queue_row(cx);
        if self.queue_edge_speed() != 0. && !self.player_page.autoscrolling {
            self.player_page.autoscrolling = true;
            cx.spawn(async move |view, cx| {
                loop {
                    cx.background_executor()
                        .timer(Duration::from_millis(16))
                        .await;
                    let scrolling = view
                        .update(cx, |view, cx| view.autoscroll_queue(cx))
                        .unwrap_or(false);
                    if !scrolling {
                        break;
                    }
                }
            })
            .detach();
        }
    }

    /// Pixels per frame to scroll Up next: negative near the top edge, positive near the bottom.
    fn queue_edge_speed(&self) -> f32 {
        const EDGE: f32 = 48.;
        const FASTEST: f32 = 12.;
        let bounds = self.player_page.queue_bounds.get();
        let y = self.player_page.drag_pointer.y;
        let above = f32::from(y - bounds.top());
        let below = f32::from(bounds.bottom() - y);
        if above < EDGE {
            -FASTEST * (1. - above.max(0.) / EDGE)
        } else if below < EDGE {
            FASTEST * (1. - below.max(0.) / EDGE)
        } else {
            0.
        }
    }

    fn autoscroll_queue(&mut self, cx: &mut Context<Self>) -> bool {
        let speed = self.queue_edge_speed();
        if self.player_page.reorder.is_none() || speed == 0. {
            self.player_page.autoscrolling = false;
            return false;
        }
        {
            let state = self.player_page.queue_scroll.0.borrow();
            let handle = &state.base_handle;
            let offset = handle.offset();
            let bottom = -handle.max_offset().y;
            let y = (offset.y - px(speed)).clamp(bottom.min(px(0.)), px(0.));
            if y == offset.y {
                drop(state);
                self.player_page.autoscrolling = false;
                return false;
            }
            handle.set_offset(point(offset.x, y));
        }
        self.retarget_queue_row(cx);
        cx.notify();
        true
    }

    fn retarget_queue_row(&mut self, cx: &mut Context<Self>) {
        let position = self.player_page.drag_pointer;
        let bounds = self.player_page.queue_bounds.get();
        let scrolled = self
            .player_page
            .queue_scroll
            .0
            .borrow()
            .base_handle
            .offset()
            .y;
        let row = f32::from(position.y - bounds.top() - scrolled) / ROW_HEIGHT;
        let last = self.library.queue.tracks.len().saturating_sub(1);
        let to = (row.max(0.) as usize).min(last);
        let Some(reorder) = &mut self.player_page.reorder else {
            return;
        };
        if reorder.to != to {
            reorder.to = to;
            cx.notify();
        }
    }

    pub(in crate::app) fn drop_queue_row(&mut self, cx: &mut Context<Self>) {
        let Some(Reorder { from, to }) = self.player_page.reorder.take() else {
            return;
        };
        if self.library.queue.move_track(from, to) {
            self.queue_changed(cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Reorder;
    use crate::{catalog::Track, library::Queue};

    #[test]
    fn dragged_rows_preview_exactly_the_order_they_drop_into() {
        let ids = ["a", "b", "c", "d", "e"];
        for from in 0..ids.len() {
            for to in 0..ids.len() {
                let mut queue = Queue {
                    tracks: ids
                        .iter()
                        .map(|id| Track {
                            id: (*id).into(),
                            title: String::new(),
                            artist: String::new(),
                            album: None,
                            duration: None,
                            thumbnail: None,
                            details: Default::default(),
                        })
                        .collect(),
                    ..Default::default()
                };
                queue.move_track(from, to);
                let dropped = queue.tracks.iter().map(|track| track.id.as_str());
                let preview = (0..ids.len()).map(|row| ids[Reorder { from, to }.source(row)]);
                assert!(preview.eq(dropped), "moving {from} to {to}");
            }
        }
    }
}
