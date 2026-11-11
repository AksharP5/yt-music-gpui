//! Player entity and its progress, transport, track, and volume controls.

use super::*;

pub(super) struct PlayerBar {
    music: WeakEntity<Music>,
    pub(super) controls_expanded: bool,
    controls_focus: FocusHandle,
    #[cfg(test)]
    pub(super) renders: usize,
}

impl PlayerBar {
    pub(super) fn new(music: WeakEntity<Music>, cx: &Context<Self>) -> Self {
        Self {
            music,
            controls_expanded: false,
            controls_focus: cx.focus_handle(),
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
        if width >= 1120. {
            self.controls_expanded = false;
        }
        let expanded = self.controls_expanded && width < 1120.;
        let focus = self.controls_focus.clone();
        self.music
            .update(cx, |music, cx| music.player_bar(width, expanded, focus, cx))
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
    tinted_control(id, name, theme.text, theme, cx, action)
}

/// The web draws the volume, repeat and shuffle buttons grey until they are active.
const INACTIVE: u32 = 0x909090;

fn tinted_control(
    id: impl Into<ElementId>,
    name: &'static str,
    color: impl Into<Hsla>,
    theme: Theme,
    cx: &mut Context<Music>,
    action: impl Fn(&mut Music, &mut Window, &mut Context<Music>) + 'static,
) -> Stateful<Div> {
    tinted_button(id, name, color, theme)
        .size(px(36.))
        .on_click(cx.listener(move |view, _, window, cx| {
            cx.stop_propagation();
            action(view, window, cx);
        }))
}

impl Music {
    pub(in crate::app) fn shuffle_queue(&mut self, cx: &mut Context<Self>) {
        let session = self.account.as_ref().map(|account| &account.session);
        match self.queue_source.shuffle(&mut self.library.queue, session) {
            Ok(()) => self.notice = Some("Upcoming songs shuffled".into()),
            Err(error) => self.error = Some(error),
        }
        self.save();
        self.notify_playback(cx);
    }

    pub(in crate::app) fn player_bar(
        &mut self,
        width: f32,
        expanded: bool,
        controls_focus: FocusHandle,
        cx: &mut Context<Self>,
    ) -> AnyElement {
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
            // The web rounds the bar's art by only 2px.
            let art = match self
                .current_thumbnail()
                .and_then(|url| self.artwork.get(&url))
            {
                Some(image) => img(image)
                    .size(px(40.))
                    .flex_shrink_0()
                    .rounded(px(2.))
                    .object_fit(ObjectFit::Cover)
                    .into_any_element(),
                None => div()
                    .size(px(40.))
                    .flex_shrink_0()
                    .rounded(px(2.))
                    .bg(theme.raised)
                    .into_any_element(),
            };
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
                _ => {
                    // Search and chart rows can omit the album; the watch response has it.
                    let issued = self
                        .current_watch()
                        .and_then(|watch| watch.current.as_ref());
                    let album = track.album.as_deref().or_else(|| issued?.album.as_deref());
                    let year = issued.and_then(|issued| issued.details.year.as_deref());
                    [Some(track.artist.as_str()), album, year]
                        .into_iter()
                        .flatten()
                        .filter(|part| !part.is_empty())
                        .collect::<Vec<_>>()
                        .join(" • ")
                }
            };
            // music.youtube.com switches the bar text to 16 px at its large breakpoint.
            let text_size = if width >= 1364. { 16. } else { 14. };
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
                        .text_size(px(text_size))
                        .line_height(px(text_size * 1.2))
                        .child(
                            div()
                                .truncate()
                                .font_weight(FontWeight::MEDIUM)
                                .child(track.title),
                        )
                        .child(
                            div()
                                .truncate()
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
                        .when(width >= 936., |d| {
                            d.child(control(
                                "like-current",
                                if liked { "thumb-up-filled" } else { "thumb-up" },
                                theme,
                                cx,
                                move |view, _, cx| view.toggle_like(&like_track, cx),
                            ))
                            .child(control(
                                "dislike-current",
                                if disliked {
                                    "thumb-down-filled"
                                } else {
                                    "thumb-down"
                                },
                                theme,
                                cx,
                                move |view, window, cx| {
                                    // Guests are asked to sign in, as on the web.
                                    if view.account.is_none() {
                                        view.connect_account(window, cx);
                                        return;
                                    }
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
                            ))
                        })
                        .child(
                            self.menu_button("current-song-menu", cx, move |view, window, cx| {
                                view.open_track_menu(menu_track.clone(), None, window, cx);
                            })
                            .size(px(36.))
                            .ml(px(13.))
                            .mr(px(16.)),
                        ),
                );
        }
        let seek_bounds = self.seek_bounds.clone();
        let seeking = self.drag == Some(Drag::Seek);
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
                    .top(px(-8.))
                    .left_0()
                    .right_0()
                    .h(px(16.))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|view, event: &MouseDownEvent, _, cx| {
                            cx.stop_propagation();
                            view.drag = Some(Drag::Seek);
                            view.drag_to(event.position, cx);
                        }),
                    )
                    .child(seek_bar(progress, seek_bounds, seeking, theme)),
            )
            .child(
                div()
                    .size_full()
                    .pr(px(16.))
                    .flex()
                    .items_center()
                    .gap(px(if compact { 0. } else { 32. }))
                    .child(
                        div()
                            .w(px(if compact { 241. } else { 280. }))
                            .pl(px(8.))
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
                                        40.,
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
                            .when(width >= 936., |d| {
                                d.child(div().text_size(px(12.)).text_color(theme.muted).child(
                                    format!(
                                        "{} / {}",
                                        clock(position.floor()),
                                        clock(self.duration)
                                    ),
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
                            .w(px(if compact { 241. } else { 280. }))
                            .relative()
                            .flex_shrink_0()
                            .flex()
                            .items_center()
                            .justify_end()
                            .gap(px(12.))
                            .on_key_down(cx.listener(
                                move |view, event: &KeyDownEvent, window, cx| {
                                    if expanded && event.keystroke.key == "escape" {
                                        view.player_bar.update(cx, |bar, cx| {
                                            bar.controls_expanded = false;
                                            window.focus(&bar.controls_focus, cx);
                                            cx.notify();
                                        });
                                        cx.stop_propagation();
                                    }
                                },
                            ))
                            .when(!compact, |group| {
                                group.child(self.player_controls(false, cx))
                            })
                            .when(compact, |group| {
                                group.child(
                                    tinted_control(
                                        "more-player-controls",
                                        "player-controls",
                                        rgb(INACTIVE),
                                        theme,
                                        cx,
                                        |view, window, cx| {
                                            view.player_bar.update(cx, |bar, cx| {
                                                bar.controls_expanded = !bar.controls_expanded;
                                                window.focus(&bar.controls_focus, cx);
                                                cx.notify();
                                            });
                                        },
                                    )
                                    .track_focus(&controls_focus),
                                )
                            })
                            .when(expanded, |group| {
                                group.child(
                                    div()
                                        .id("expanded-player-controls")
                                        .absolute()
                                        .right(px(36.))
                                        .w(px(260.))
                                        .h(px(36.))
                                        .px(px(12.))
                                        .bg(theme.panel)
                                        .child(self.player_controls(true, cx))
                                        .occlude()
                                        .on_click(|_, _, cx| cx.stop_propagation())
                                        .on_mouse_down_out(cx.listener(|view, _, _, cx| {
                                            view.player_bar.update(cx, |bar, cx| {
                                                bar.controls_expanded = false;
                                                cx.notify();
                                            });
                                        })),
                                )
                            })
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

    fn player_controls(&self, expanded: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let volume_bounds = self.volume_bounds.clone();
        let adjusting_volume = self.drag == Some(Drag::Volume);
        let repeat = self.library.repeat;
        div()
            .w(px(236.))
            .h(px(36.))
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
                    .child(
                        div()
                            .id("volume-track")
                            .w(px(100.))
                            .h(px(32.))
                            .when(!expanded && !adjusting_volume, |d| {
                                d.invisible().group_hover("volume", |d| d.visible())
                            })
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|view, event: &MouseDownEvent, _, cx| {
                                    cx.stop_propagation();
                                    view.drag = Some(Drag::Volume);
                                    view.drag_to(event.position, cx);
                                }),
                            )
                            .child(slider(
                                (self.library.volume / 100.) as f32,
                                volume_bounds,
                                theme,
                            )),
                    )
                    .child(tinted_control(
                        "mute-toggle",
                        if self.library.volume == 0. {
                            "mute"
                        } else {
                            "volume"
                        },
                        rgb(INACTIVE),
                        theme,
                        cx,
                        |view, _, cx| {
                            view.set_volume(if view.library.volume > 0. { 0. } else { 60. }, cx);
                            view.save_preferences();
                        },
                    )),
            )
            .child(
                tinted_control(
                    "repeat",
                    "repeat",
                    if repeat == Repeat::Off {
                        rgb(INACTIVE).into()
                    } else {
                        theme.text
                    },
                    theme,
                    cx,
                    |view, _, cx| {
                        view.library.repeat = view.library.repeat.next();
                        view.save_preferences();
                        view.notify_playback(cx);
                    },
                )
                .when(repeat == Repeat::One, |d| {
                    d.child(
                        div()
                            .absolute()
                            .top_0()
                            .right_0()
                            .text_size(px(9.))
                            .child("1"),
                    )
                }),
            )
            .child(tinted_control(
                "shuffle",
                "shuffle",
                rgb(INACTIVE),
                theme,
                cx,
                |view, _, cx| view.shuffle_queue(cx),
            ))
    }
}
