//! Native video viewport for the player page and fullscreen presentation.

use super::*;

impl Music {
    /// Collects the bounds of the children of the element it is attached to: a dialog or
    /// menu that can sit over the video. X11 draws the video above the app, so the window
    /// cuts these out of it.
    pub(in crate::app) fn covers_video(
        &self,
    ) -> impl Fn(Vec<Bounds<Pixels>>, &mut Window, &mut App) + 'static {
        let covers = self.player_page.video_covers.clone();
        move |bounds, _, _| covers.borrow_mut().extend(bounds)
    }

    /// The video surface. libmpv draws into its bounds through the transparent window.
    pub(in crate::app) fn video_viewport(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let fullscreen = window.is_fullscreen();
        let current = self.library.queue.current();
        let native = self.active_id.is_some()
            && self.playback != Playback::Stopped
            && self.video_error.is_none()
            && self.video.is_some();
        let ready = native
            && self
                .video
                .as_ref()
                .is_some_and(|video| video.status() == VideoStatus::Ready);
        let view = cx.entity().downgrade();
        let hole = self.player_page.video_bounds.clone();
        let mut viewport = div()
            .id("video-viewport")
            .group("video-viewport")
            .relative()
            .when(!ready, |viewport| viewport.bg(gpui::black()))
            .child(
                canvas(
                    move |bounds, window, cx| {
                        hole.set(Some(bounds));
                        view.update(cx, |view, cx| {
                            if !native {
                                return false;
                            }
                            let Some(video) = &view.video else {
                                return false;
                            };
                            if let Err(error) = video.attach(bounds, window) {
                                view.video_error = Some(error.to_string());
                                cx.notify();
                                return false;
                            }
                            video.status() == VideoStatus::Ready
                        })
                        .unwrap_or(false)
                    },
                    |bounds, ready, window, _| {
                        if !ready {
                            window.paint_quad(fill(bounds, gpui::black()));
                        }
                    },
                )
                .absolute()
                .size_full(),
            )
            .on_click(cx.listener(|view, event: &ClickEvent, window, cx| {
                window.focus(&view.focus, cx);
                view.video_click = None;
                if event.click_count() == 2 {
                    view.toggle_video_fullscreen(window, cx);
                    return;
                }
                if event.click_count() != 1 {
                    return;
                }
                let active = view.active_id;
                let playback = view.playback;
                view.video_click = Some(cx.spawn(async move |view, cx| {
                    cx.background_executor()
                        .timer(double_click_interval())
                        .await;
                    let _ = view.update(cx, |view, cx| {
                        if view.player_open
                            && view.overlay.is_none()
                            && view.active_id == active
                            && view.playback == playback
                        {
                            view.toggle_play(cx);
                        }
                    });
                }));
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|view, event: &MouseDownEvent, _, _| {
                    if event.click_count >= 2 {
                        view.video_click = None;
                    }
                }),
            )
            .child(
                div()
                    .absolute()
                    .top(px(12.))
                    .right(px(12.))
                    .invisible()
                    .group_hover("video-viewport", |button| button.visible())
                    .child(
                        button(
                            "video-fullscreen",
                            if fullscreen {
                                "exit-fullscreen"
                            } else {
                                "fullscreen"
                            },
                            self.theme,
                        )
                        .on_click(cx.listener(|view, _, window, cx| {
                            cx.stop_propagation();
                            view.toggle_video_fullscreen(window, cx)
                        })),
                    ),
            );
        if !native {
            let message = self.video_error.clone().unwrap_or_else(|| {
                if current.is_some() {
                    "Press play to watch".into()
                } else {
                    "Choose something to play".into()
                }
            });
            viewport = viewport.child(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap(px(18.))
                    .px(px(40.))
                    .text_color(self.theme.muted)
                    .child(message)
                    .when(self.video_error.is_some(), |body| {
                        body.child(
                            div()
                                .id("retry-video")
                                .px(px(16.))
                                .py(px(10.))
                                .text_color(self.theme.text)
                                .cursor_pointer()
                                .child("Retry video")
                                .on_click(cx.listener(|view, _, _, cx| {
                                    cx.stop_propagation();
                                    if let Some(video) = &view.video {
                                        video.detach();
                                    }
                                    view.video_error = None;
                                    cx.notify();
                                })),
                        )
                    }),
            );
        }
        viewport
    }
}

/// How long a click on the video waits to see whether it starts a double-click.
fn double_click_interval() -> Duration {
    // The user's setting, which AppKit counts double-clicks with.
    #[cfg(target_os = "macos")]
    {
        if let Ok(interval) =
            Duration::try_from_secs_f64(objc2_app_kit::NSEvent::doubleClickInterval())
        {
            return interval;
        }
    }
    // GPUI's Linux backends recognize double-clicks within 400 ms.
    Duration::from_millis(400)
}
