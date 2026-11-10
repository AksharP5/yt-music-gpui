//! Player entity and its progress, transport, track, and volume controls.

use super::*;

pub(super) struct PlayerBar {
    music: WeakEntity<Music>,
    #[cfg(test)]
    pub(super) renders: usize,
}

impl PlayerBar {
    pub(super) fn new(music: WeakEntity<Music>) -> Self {
        Self {
            music,
            #[cfg(test)]
            renders: 0,
        }
    }
}

impl Render for PlayerBar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        #[cfg(test)]
        {
            self.renders += 1;
        }
        let width = window.viewport_size().width.into();
        self.music
            .update(cx, |music, cx| music.player_bar(width, cx))
            .expect("Music owns its player bar")
    }
}

impl Music {
    pub(in crate::app) fn player_bar(&mut self, width: f32, cx: &mut Context<Self>) -> AnyElement {
        let compact = width < 1120.;
        let current = self.library.queue.current().cloned();
        let progress = if self.duration > 0. {
            (self
                .drag_position
                .unwrap_or_else(|| self.playback_position())
                / self.duration)
                .clamp(0., 1.) as f32
        } else {
            0.
        };
        let playing = self.active_id.is_some()
            && self.playback != Playback::Stopped
            && !self.playback.is_paused();
        let mut details = div()
            .flex_1()
            .min_w_0()
            .max_w(px(660.))
            .flex()
            .items_center()
            .gap(px(16.));
        if let Some(track) = current {
            let art = self.art(track.thumbnail.as_deref(), 40., ArtworkShape::Square);
            let liked = self.is_liked(&track);
            let disliked = self
                .ratings
                .get(&track.id)
                .copied()
                .or(track.details.rating)
                == Some(Rating::Dislike);
            let subtitle = match self.playback {
                Playback::Loading { .. } => "Loading track…".into(),
                Playback::Loaded {
                    paused: false,
                    buffering: true,
                } => "Buffering…".into(),
                _ => track.artist.clone(),
            };
            let like_track = track.clone();
            let dislike_track = track.clone();
            let menu_track = track.clone();
            details = details
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
                                .font_weight(FontWeight::MEDIUM)
                                .child(track.title),
                        )
                        .child(
                            div()
                                .truncate()
                                .text_size(px(16.))
                                .text_color(self.theme.muted)
                                .child(subtitle),
                        ),
                )
                .when(self.account.is_some() && !compact, |d| {
                    d.child(
                        button("dislike-current", "thumb-down", self.theme)
                            .opacity(if disliked { 1. } else { 0.65 })
                            .on_click(cx.listener(move |view, _, _, cx| {
                                view.mutate(
                                    Mutation::RateSong {
                                        track: dislike_track.clone(),
                                        rating: if disliked {
                                            Rating::Indifferent
                                        } else {
                                            Rating::Dislike
                                        },
                                    },
                                    cx,
                                );
                            })),
                    )
                })
                .child(
                    button(
                        "like-current",
                        if liked { "thumb-up-filled" } else { "thumb-up" },
                        self.theme,
                    )
                    .on_click(cx.listener(move |view, _, _, cx| view.toggle_like(&like_track, cx))),
                )
                .child(
                    button("current-song-menu", "more", self.theme).on_click(cx.listener(
                        move |view, _, window, cx| {
                            window.focus(&view.focus, cx);
                            view.open_track_menu(menu_track.clone(), None, window, cx);
                        },
                    )),
                );
        }
        let seek_bounds = self.seek_bounds.clone();
        let volume_bounds = self.volume_bounds.clone();
        div()
            .h(px(if width < 936. { 64. } else { 72. }))
            .flex_shrink_0()
            .relative()
            .bg(self.theme.panel)
            .child(
                div()
                    .id("seek-track")
                    .absolute()
                    .top(px(-6.))
                    .left_0()
                    .right_0()
                    .h(px(12.))
                    .cursor_pointer()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|view, event: &MouseDownEvent, _, cx| {
                            view.drag = Some(Drag::Seek);
                            view.drag_to(event.position, cx);
                        }),
                    )
                    .child(slider(progress, seek_bounds, self.theme)),
            )
            .child(
                div()
                    .size_full()
                    .px(px(16.))
                    .flex()
                    .items_center()
                    .gap(px(if compact { 16. } else { 32. }))
                    .child(
                        div()
                            .flex_shrink_0()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .child(
                                button("previous", "previous", self.theme)
                                    .on_click(cx.listener(|view, _, _, cx| view.previous(cx))),
                            )
                            .child(
                                button(
                                    "play-pause",
                                    if playing { "pause" } else { "play" },
                                    self.theme,
                                )
                                .size(px(48.))
                                .on_click(cx.listener(|view, _, _, cx| view.toggle_play(cx))),
                            )
                            .child(
                                button("next", "next", self.theme)
                                    .on_click(cx.listener(|view, _, _, cx| view.next(false, cx))),
                            )
                            .when(!compact, |d| {
                                d.child(
                                    div()
                                        .ml(px(8.))
                                        .text_size(px(12.))
                                        .text_color(self.theme.muted)
                                        .child(format!(
                                            "{} / {}",
                                            clock(
                                                self.drag_position
                                                    .unwrap_or_else(|| self.playback_position())
                                            ),
                                            clock(self.duration)
                                        )),
                                )
                            }),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .justify_center()
                            .child(details),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .flex()
                            .items_center()
                            .gap(px(4.))
                            .when(!compact, |d| {
                                d.child(
                                    div()
                                        .id("volume-track")
                                        .w(px(72.))
                                        .h(px(18.))
                                        .cursor_pointer()
                                        .on_mouse_down(
                                            MouseButton::Left,
                                            cx.listener(|view, event: &MouseDownEvent, _, cx| {
                                                view.drag = Some(Drag::Volume);
                                                view.drag_to(event.position, cx);
                                            }),
                                        )
                                        .child(slider(
                                            (self.library.volume / 100.) as f32,
                                            volume_bounds,
                                            self.theme,
                                        )),
                                )
                            })
                            .child(
                                button(
                                    "mute-toggle",
                                    if self.library.volume == 0. {
                                        "mute"
                                    } else {
                                        "volume"
                                    },
                                    self.theme,
                                )
                                .on_click(cx.listener(
                                    |view, _, _, cx| {
                                        view.set_volume(
                                            if view.library.volume > 0. { 0. } else { 60. },
                                            cx,
                                        );
                                        view.save();
                                    },
                                )),
                            )
                            .child(
                                button("repeat", "repeat", self.theme)
                                    .opacity(if self.library.repeat == Repeat::Off {
                                        0.6
                                    } else {
                                        1.
                                    })
                                    .when(self.library.repeat == Repeat::One, |d| {
                                        d.child(
                                            div()
                                                .absolute()
                                                .top_0()
                                                .right_0()
                                                .text_size(px(9.))
                                                .child("1"),
                                        )
                                    })
                                    .on_click(cx.listener(|view, _, _, cx| {
                                        view.library.repeat = view.library.repeat.next();
                                        view.save();
                                        cx.notify();
                                    })),
                            )
                            .child(
                                button("shuffle", "shuffle", self.theme).on_click(cx.listener(
                                    |view, _, _, cx| {
                                        view.library.queue.shuffle_upcoming();
                                        view.notice = Some("Upcoming songs shuffled".into());
                                        view.prefetch_queue(cx);
                                        view.save();
                                        cx.notify();
                                    },
                                )),
                            )
                            .child(
                                button("video-toggle", "video", self.theme)
                                    .opacity(if self.video_open { 1. } else { 0.65 })
                                    .on_click(cx.listener(|view, _, window, cx| {
                                        window.focus(&view.focus, cx);
                                        if view.video_open {
                                            view.video_open = false;
                                            cx.notify();
                                        } else {
                                            view.set_video_mode(true, cx);
                                        }
                                    })),
                            )
                            .child(
                                button("queue-toggle", "queue", self.theme)
                                    .opacity(if self.show_queue { 1. } else { 0.65 })
                                    .on_click(cx.listener(|view, _, _, cx| {
                                        view.show_queue = !view.show_queue;
                                        cx.notify();
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }
}
