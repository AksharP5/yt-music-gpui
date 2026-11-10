//! Video view, media mode controls, and fullscreen presentation.

use super::*;

impl Music {
    pub(in crate::app) fn video_content(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let fullscreen = window.is_fullscreen();
        let current = self.library.queue.current();
        let title = current
            .map_or("Now playing", |track| track.title.as_str())
            .to_owned();
        let native = self.overlay.is_none()
            && self.active_id.is_some()
            && self.playback != Playback::Stopped
            && self.video_error.is_none()
            && self.video.is_some();
        let ready = native
            && self
                .video
                .as_ref()
                .is_some_and(|video| video.status() == VideoStatus::Ready);
        let view = cx.entity().downgrade();
        let mut viewport = div()
            .id("video-viewport")
            .relative()
            .flex_1()
            .min_h_0()
            .when(!ready, |viewport| viewport.bg(gpui::black()))
            .child(
                canvas(
                    move |bounds, window, cx| {
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
                    // GPUI's Linux backends recognize double-clicks within 400 ms.
                    cx.background_executor()
                        .timer(Duration::from_millis(400))
                        .await;
                    let _ = view.update(cx, |view, cx| {
                        if view.video_open
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
        let content = div().flex_1().min_w_0().flex().flex_col();
        content
            .child(
                div()
                    .h(px(64.))
                    .flex_shrink_0()
                    .px(px(24.))
                    .bg(self.theme.bg)
                    .flex()
                    .items_center()
                    .gap(px(14.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(title),
                    )
                    .child(
                        div()
                            .id("audio-mode")
                            .px(px(10.))
                            .py(px(12.))
                            .text_color(self.theme.muted)
                            .cursor_pointer()
                            .child("Audio")
                            .on_click(cx.listener(|view, _, _, cx| view.set_video_mode(false, cx))),
                    )
                    .child(div().text_color(self.theme.accent).child("Video"))
                    .child(
                        div()
                            .id("video-quality-picker")
                            .px(px(10.))
                            .py(px(12.))
                            .cursor_pointer()
                            .child(self.library.video_quality.label())
                            .on_click(cx.listener(|view, _, window, cx| {
                                window.focus(&view.focus, cx);
                                view.settings_section = SettingsSection::Playback;
                                view.overlay = Some(Overlay::Settings);
                                cx.notify();
                            })),
                    )
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
                            view.toggle_video_fullscreen(window, cx)
                        })),
                    ),
            )
            .when_some(
                self.error
                    .as_ref()
                    .filter(|_| !matches!(self.overlay, Some(Overlay::SignIn))),
                |content, error| {
                    content.child(
                        div()
                            .px(px(24.))
                            .py(px(12.))
                            .bg(self.theme.panel)
                            .text_color(rgb(0xf1a6ae))
                            .child(error.clone()),
                    )
                },
            )
            .child(viewport)
            .into_any_element()
    }
}
