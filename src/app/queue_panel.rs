//! Queue panel, recommendations, and queue rows.

use super::*;

impl Music {
    pub(in crate::app) fn queue(&mut self, compact: bool, cx: &mut Context<Self>) -> AnyElement {
        let count = self.library.queue.tracks.len();
        let episodes = self
            .library
            .queue
            .tracks
            .iter()
            .filter(|track| track.details.episode.is_some())
            .count();
        let label = match (count, episodes) {
            (1, 1) => "1 episode".into(),
            (1, 0) => "1 song".into(),
            (_, 0) => format!("{count} songs"),
            _ if episodes == count => format!("{count} episodes"),
            _ => format!("{count} items"),
        };
        div()
            .w(px(if compact { 320. } else { 400. }))
            .h_full()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .border_l_1()
            .border_color(self.theme.border)
            .bg(self.theme.bg)
            .child(
                div()
                    .h(px(64.))
                    .px(px(20.))
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(20.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Up next"),
                    )
                    .child(
                        div()
                            .id("clear-queue")
                            .text_size(px(12.))
                            .text_color(self.theme.muted)
                            .cursor_pointer()
                            .child("Clear")
                            .on_click(cx.listener(|view, _, _, cx| view.clear_queue(cx))),
                    )
                    .child(
                        button("close-queue", "close", self.theme).on_click(cx.listener(
                            |view, _, _, cx| {
                                view.show_queue = false;
                                cx.notify();
                            },
                        )),
                    ),
            )
            .child(
                div()
                    .px(px(20.))
                    .pb(px(18.))
                    .text_size(px(12.))
                    .text_color(self.theme.muted)
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(label)
                    .child(
                        div()
                            .id("autoplay-toggle")
                            .cursor_pointer()
                            .text_color(if self.library.autoplay {
                                self.theme.accent
                            } else {
                                self.theme.muted
                            })
                            .child(if self.library.autoplay {
                                "Autoplay on"
                            } else {
                                "Autoplay off"
                            })
                            .on_click(cx.listener(|view, _, _, cx| view.toggle_autoplay(cx))),
                    ),
            )
            .child(
                uniform_list(
                    "queue-list",
                    self.library.queue.tracks.len() + self.autoplay_tracks.len(),
                    cx.processor(|view, range: std::ops::Range<usize>, _, cx| {
                        range
                            .map(|index| {
                                if index >= view.library.queue.tracks.len() {
                                    let index = index - view.library.queue.tracks.len();
                                    let track = view.autoplay_tracks[index].clone();
                                    let art = view.art(
                                        track.thumbnail.as_deref(),
                                        40.,
                                        ArtworkShape::Square,
                                    );
                                    return div()
                                        .id(("autoplay-row", index))
                                        .h(px(64.))
                                        .px(px(18.))
                                        .flex()
                                        .items_center()
                                        .gap(px(10.))
                                        .cursor_pointer()
                                        .hover(|d| d.bg(view.theme.raised))
                                        .on_click(cx.listener(move |view, _, _, cx| {
                                            view.play_recommendation(index, cx)
                                        }))
                                        .child(art)
                                        .child(
                                            div()
                                                .flex_1()
                                                .min_w_0()
                                                .flex()
                                                .flex_col()
                                                .gap(px(4.))
                                                .child(
                                                    div()
                                                        .truncate()
                                                        .text_size(px(16.))
                                                        .child(track.title),
                                                )
                                                .child(
                                                    div()
                                                        .truncate()
                                                        .text_size(px(14.))
                                                        .text_color(view.theme.muted)
                                                        .child(format!(
                                                            "Autoplay · {}",
                                                            track.artist
                                                        )),
                                                ),
                                        )
                                        .child(
                                            button(("remove-autoplay", index), "close", view.theme)
                                                .w(px(26.))
                                                .on_click(cx.listener(move |view, _, _, cx| {
                                                    cx.stop_propagation();
                                                    view.autoplay_tracks.remove(index);
                                                    cx.notify();
                                                })),
                                        )
                                        .into_any_element();
                                }
                                let track = view.library.queue.tracks[index].clone();
                                let active = view.library.queue.index == Some(index);
                                let art =
                                    view.art(track.thumbnail.as_deref(), 40., ArtworkShape::Square);
                                div()
                                    .id(("queue-row", index))
                                    .w_full()
                                    .h(px(64.))
                                    .px(px(18.))
                                    .rounded(px(5.))
                                    .flex()
                                    .items_center()
                                    .gap(px(10.))
                                    .cursor_pointer()
                                    .when(active, |d| d.bg(view.theme.selection))
                                    .hover(|d| d.bg(view.theme.raised))
                                    .on_click(cx.listener(move |view, _, _, cx| {
                                        view.library.queue.select(index);
                                        view.play_current(cx);
                                    }))
                                    .child(art)
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .flex()
                                            .flex_col()
                                            .gap(px(4.))
                                            .child(
                                                div()
                                                    .truncate()
                                                    .text_size(px(16.))
                                                    .when(active, |d| {
                                                        d.text_color(view.theme.accent)
                                                    })
                                                    .child(track.title),
                                            )
                                            .child(
                                                div()
                                                    .truncate()
                                                    .text_size(px(14.))
                                                    .text_color(view.theme.muted)
                                                    .child(track.artist),
                                            ),
                                    )
                                    .child(div().flex().gap(px(2.)).children(
                                        [(-1isize, "↑"), (1, "↓")].into_iter().enumerate().map(
                                            |(key, (direction, label))| {
                                                let target = index
                                                    .checked_add_signed(direction)
                                                    .filter(|target| {
                                                        *target < view.library.queue.tracks.len()
                                                    });
                                                div()
                                                    .id(("queue-move", index * 2 + key))
                                                    .w(px(22.))
                                                    .h(px(30.))
                                                    .flex()
                                                    .items_center()
                                                    .justify_center()
                                                    .text_size(px(16.))
                                                    .cursor_pointer()
                                                    .child(label)
                                                    .opacity(if target.is_some() {
                                                        1.
                                                    } else {
                                                        0.3
                                                    })
                                                    .on_click(cx.listener(move |view, _, _, cx| {
                                                        cx.stop_propagation();
                                                        if let Some(target) = target
                                                            && view
                                                                .library
                                                                .queue
                                                                .move_track(index, target)
                                                        {
                                                            view.queue_changed(cx);
                                                        }
                                                    }))
                                            },
                                        ),
                                    ))
                                    .child(
                                        button(("remove-queue", index), "close", view.theme)
                                            .w(px(26.))
                                            .on_click(cx.listener(move |view, _, _, cx| {
                                                cx.stop_propagation();
                                                view.remove_queue_track(index, cx);
                                            })),
                                    )
                                    .into_any_element()
                            })
                            .collect::<Vec<_>>()
                    }),
                )
                .flex_1()
                .min_h_0(),
            )
            .when(self.library.queue.tracks.is_empty(), |d| {
                d.child(
                    div()
                        .px(px(20.))
                        .text_color(self.theme.muted)
                        .text_size(px(16.))
                        .child("Play a song or add one with +."),
                )
            })
            .into_any_element()
    }
}
