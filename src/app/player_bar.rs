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

/// A bar button that keeps its click from also toggling the player page.
fn control(
    id: impl Into<ElementId>,
    name: &'static str,
    theme: Theme,
    cx: &mut Context<Music>,
    action: impl Fn(&mut Music, &mut Window, &mut Context<Music>) + 'static,
) -> Stateful<Div> {
    button(id, name, theme)
        .size(px(36.))
        .on_click(cx.listener(move |view, _, window, cx| {
            cx.stop_propagation();
            action(view, window, cx);
        }))
}

impl Music {
    pub(in crate::app) fn player_bar(&mut self, width: f32, cx: &mut Context<Self>) -> AnyElement {
        let compact = width < 1120.;
        let theme = self.theme;
        let current = self.library.queue.current().cloned();
        let position = self
            .drag_position
            .unwrap_or_else(|| self.playback_position());
        let progress = if self.duration > 0. {
            (position / self.duration).clamp(0., 1.) as f32
        } else {
            0.
        };
        let playing = self.active_id.is_some()
            && self.playback != Playback::Stopped
            && !self.playback.is_paused();
        // Sized to its content and centered, so the rating buttons follow the title as on the web.
        let mut details = div().min_w_0().max_w(px(660.)).flex().items_center();
        if let Some(track) = current {
            let thumbnail = self.current_thumbnail();
            let art = self.art(thumbnail.as_deref(), 40., ArtworkShape::Square);
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
                _ => match &track.album {
                    Some(album) if !album.is_empty() => format!("{} • {album}", track.artist),
                    _ => track.artist.clone(),
                },
            };
            let like_track = track.clone();
            let dislike_track = track.clone();
            let menu_track = track.clone();
            details = details
                .child(art)
                .child(
                    div()
                        .ml(px(16.))
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .line_height(px(19.))
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
                                .text_color(gpui::white().opacity(0.7))
                                .child(subtitle),
                        ),
                )
                .child(
                    div()
                        .ml(px(8.))
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .gap(px(3.))
                        .child(control(
                            "like-current",
                            if liked { "thumb-up-filled" } else { "thumb-up" },
                            theme,
                            cx,
                            move |view, _, cx| view.toggle_like(&like_track, cx),
                        ))
                        .when(self.account.is_some() && !compact, |d| {
                            d.child(
                                control(
                                    "dislike-current",
                                    "thumb-down",
                                    theme,
                                    cx,
                                    move |view, _, cx| {
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
                                    },
                                )
                                .opacity(if disliked {
                                    1.
                                } else {
                                    0.7
                                }),
                            )
                        })
                        .child(
                            control(
                                "current-song-menu",
                                "more",
                                theme,
                                cx,
                                move |view, window, cx| {
                                    window.focus(&view.focus, cx);
                                    view.open_track_menu(menu_track.clone(), None, window, cx);
                                },
                            )
                            .ml(px(13.)),
                        ),
                );
        }
        let seek_bounds = self.seek_bounds.clone();
        let volume_bounds = self.volume_bounds.clone();
        let adjusting_volume = self.drag == Some(Drag::Volume);
        let repeat = self.library.repeat;
        div()
            .id("player-bar")
            .h(px(if width < 936. { 64. } else { 72. }))
            .flex_shrink_0()
            .relative()
            .bg(theme.panel)
            .cursor_pointer()
            .on_click(cx.listener(|view, _, window, cx| {
                window.focus(&view.focus, cx);
                view.toggle_player_page(cx);
            }))
            .child(
                div()
                    .id("seek-track")
                    .absolute()
                    .top(px(-6.))
                    .left_0()
                    .right_0()
                    .h(px(12.))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|view, event: &MouseDownEvent, _, cx| {
                            cx.stop_propagation();
                            view.drag = Some(Drag::Seek);
                            view.drag_to(event.position, cx);
                        }),
                    )
                    .child(slider(progress, seek_bounds, theme)),
            )
            .child(
                div()
                    .size_full()
                    .pl(px(8.))
                    .pr(px(16.))
                    .flex()
                    .items_center()
                    .gap(px(if compact { 16. } else { 32. }))
                    .child(
                        div()
                            .flex_shrink_0()
                            .flex()
                            .items_center()
                            .gap(px(12.))
                            .child(control("previous", "previous", theme, cx, |view, _, cx| {
                                view.previous(cx)
                            }))
                            .child(
                                div()
                                    .id("play-pause")
                                    .size(px(52.))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded_full()
                                    .hover(|d| d.bg(theme.raised))
                                    .child(icon(
                                        if playing { "pause" } else { "play" },
                                        36.,
                                        theme.text,
                                    ))
                                    .on_click(cx.listener(|view, _, _, cx| {
                                        cx.stop_propagation();
                                        view.toggle_play(cx);
                                    })),
                            )
                            .child(control("next", "next", theme, cx, |view, _, cx| {
                                view.next(false, cx)
                            }))
                            .when(!compact, |d| {
                                d.child(div().text_size(px(12.)).text_color(theme.muted).child(
                                    format!("{} / {}", clock(position), clock(self.duration)),
                                ))
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
                            // Taffy under-measures this group's content, so size it from
                            // its fixed controls: four 36 px buttons, gaps, and the slider.
                            .w(px(if compact { 176. } else { 280. }))
                            .flex_shrink_0()
                            .flex()
                            .items_center()
                            .gap(px(12.))
                            .child(
                                div()
                                    .id("volume")
                                    .group("volume")
                                    .flex_shrink_0()
                                    .flex()
                                    .items_center()
                                    .gap(px(4.))
                                    .when(!compact, |d| {
                                        d.child(
                                            div()
                                                .id("volume-track")
                                                .w(px(100.))
                                                .h(px(32.))
                                                .when(!adjusting_volume, |d| {
                                                    d.invisible()
                                                        .group_hover("volume", |d| d.visible())
                                                })
                                                .on_mouse_down(
                                                    MouseButton::Left,
                                                    cx.listener(
                                                        |view, event: &MouseDownEvent, _, cx| {
                                                            cx.stop_propagation();
                                                            view.drag = Some(Drag::Volume);
                                                            view.drag_to(event.position, cx);
                                                        },
                                                    ),
                                                )
                                                .child(slider(
                                                    (self.library.volume / 100.) as f32,
                                                    volume_bounds,
                                                    theme,
                                                )),
                                        )
                                    })
                                    .child(control(
                                        "mute-toggle",
                                        if self.library.volume == 0. {
                                            "mute"
                                        } else {
                                            "volume"
                                        },
                                        theme,
                                        cx,
                                        |view, _, cx| {
                                            view.set_volume(
                                                if view.library.volume > 0. { 0. } else { 60. },
                                                cx,
                                            );
                                            view.save();
                                        },
                                    )),
                            )
                            .child(
                                control("repeat", "repeat", theme, cx, |view, _, cx| {
                                    view.library.repeat = view.library.repeat.next();
                                    view.save();
                                    cx.notify();
                                })
                                .opacity(if repeat == Repeat::Off { 0.6 } else { 1. })
                                .when(
                                    repeat == Repeat::One,
                                    |d| {
                                        d.child(
                                            div()
                                                .absolute()
                                                .top_0()
                                                .right_0()
                                                .text_size(px(9.))
                                                .child("1"),
                                        )
                                    },
                                ),
                            )
                            .child(control("shuffle", "shuffle", theme, cx, |view, _, cx| {
                                view.library.queue.shuffle_upcoming();
                                view.notice = Some("Upcoming songs shuffled".into());
                                view.prefetch_queue(cx);
                                view.save();
                                cx.notify();
                            }))
                            .child(
                                control(
                                    "player-page-toggle",
                                    if self.player_open {
                                        "collapse"
                                    } else {
                                        "expand"
                                    },
                                    theme,
                                    cx,
                                    |view, window, cx| {
                                        window.focus(&view.focus, cx);
                                        view.toggle_player_page(cx);
                                    },
                                )
                                .ml(px(-4.)),
                            ),
                    ),
            )
            .into_any_element()
    }
}
