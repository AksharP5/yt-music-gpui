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
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .when_some(self.library.queue.playing_from.clone(), |header, source| {
                header.child(
                    div()
                        .pt(px(16.))
                        .pb(px(8.))
                        .flex()
                        .flex_col()
                        .gap(px(4.))
                        .child(
                            div()
                                .text_size(px(12.))
                                .text_color(self.theme.muted)
                                .child("Playing from"),
                        )
                        .child(
                            div()
                                .truncate()
                                .text_size(px(14.))
                                .font_weight(FontWeight::MEDIUM)
                                .child(source),
                        ),
                )
            })
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
                            cx.processor(move |view, range: std::ops::Range<usize>, _, cx| {
                                range
                                    .map(|row| match view.up_next_row(row) {
                                        UpNextRow::Queued(row) => view.queue_row(row, cx),
                                        UpNextRow::AutoplayLabel => div()
                                            .w_full()
                                            .h(px(ROW_HEIGHT))
                                            .px(px(12.))
                                            .flex()
                                            .items_center()
                                            .text_size(px(16.))
                                            .text_color(view.theme.muted)
                                            .child("Autoplay is on")
                                            .into_any_element(),
                                        UpNextRow::Autoplay(index) => view.autoplay_row(index, cx),
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
            .px(px(8.))
            .py(px(12.))
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

    fn queue_row(&mut self, row: usize, cx: &mut Context<Self>) -> AnyElement {
        let reorder = self.player_page.reorder;
        let index = reorder.map_or(row, |reorder| reorder.source(row));
        let dragged = reorder.is_some_and(|reorder| reorder.to == row);
        let Some(track) = self.library.queue.tracks.get(index).cloned() else {
            return div().h(px(ROW_HEIGHT)).into_any_element();
        };
        let active = self.library.queue.index == Some(index);
        let playing = active && self.playback != Playback::Stopped && !self.playback.is_paused();
        let menu_track = track.clone();
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
                    cx.listener(move |view, _, _, cx| {
                        cx.stop_propagation();
                        view.player_page.reorder = Some(Reorder {
                            from: index,
                            to: index,
                        });
                        view.drag = Some(Drag::Queue);
                        cx.notify();
                    }),
                )
        });
        let menu = self.row_menu(("queue-menu", index), cx, move |view, window, cx| {
            view.open_track_menu(menu_track.clone(), None, window, cx);
            if let Some(Overlay::Track { queue_index, .. }) = &mut view.overlay {
                *queue_index = Some(index);
            }
        });
        self.track_row(
            ("queue-row", index),
            &track,
            active.then_some(playing),
            dragged,
            handle,
            menu,
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

    fn autoplay_row(&mut self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(track) = self.autoplay_tracks.get(index).cloned() else {
            return div().h(px(ROW_HEIGHT)).into_any_element();
        };
        let menu_track = track.clone();
        let menu = self.row_menu(("autoplay-menu", index), cx, move |view, window, cx| {
            view.open_track_menu(menu_track.clone(), None, window, cx)
        });
        self.track_row(("autoplay-row", index), &track, None, false, None, menu)
            .on_click(cx.listener(move |view, _, _, cx| view.play_recommendation(index, cx)))
            .into_any_element()
    }

    /// The shared row: thumbnail with its play state, title, artist and duration.
    fn track_row(
        &mut self,
        id: (&'static str, usize),
        track: &Track,
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
            .group("up-next-row")
            .w_full()
            .h(px(ROW_HEIGHT))
            .px(px(8.))
            .flex()
            .items_center()
            .border_b_1()
            .border_color(gpui::white().opacity(0.1))
            .cursor_pointer()
            .when(active, |row| row.bg(gpui::white().opacity(0.1)))
            .when(dragged, |row| row.bg(self.theme.raised).shadow_lg())
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
                    .line_height(px(19.))
                    .child(
                        div()
                            .truncate()
                            .text_size(px(16.))
                            .font_weight(FontWeight::MEDIUM)
                            .child(track.title.clone()),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_size(px(16.))
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
                            .text_size(px(16.))
                            .text_color(self.theme.muted)
                            .group_hover("up-next-row", |duration| duration.invisible())
                            .child(duration),
                    )
                    .child(
                        div()
                            .absolute()
                            .top_0()
                            .right_0()
                            .invisible()
                            .group_hover("up-next-row", |menu| menu.visible())
                            .child(menu),
                    ),
            )
    }

    /// The hover menu button that replaces the duration, as on the web.
    fn row_menu(
        &self,
        id: (&'static str, usize),
        cx: &mut Context<Self>,
        open: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> AnyElement {
        button(id, "more", self.theme)
            .size(px(36.))
            .on_click(cx.listener(move |view, _, window, cx| {
                cx.stop_propagation();
                open(view, window, cx);
            }))
            .into_any_element()
    }

    pub(in crate::app) fn drag_queue_row(
        &mut self,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        let Some(reorder) = &mut self.player_page.reorder else {
            return;
        };
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
