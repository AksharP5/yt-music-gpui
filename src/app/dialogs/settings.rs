//! Settings sections, controls, and selection menus.

use super::super::*;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(in crate::app) enum SettingsSection {
    General,
    Playback,
    About,
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub(in crate::app) enum SettingMenu {
    Speed,
    AudioQuality,
    VideoQuality,
}

impl Music {
    pub(in crate::app) fn setting_row(
        &self,
        title: &'static str,
        kind: SettingMenu,
        field_width: f32,
        cx: &Context<Self>,
    ) -> AnyElement {
        let field_bounds = Rc::new(Cell::new(Bounds::default()));
        let measured_bounds = field_bounds.clone();
        let value = match kind {
            SettingMenu::Speed => format!("{}×", f64::from(self.library.speed)),
            SettingMenu::AudioQuality => self.library.quality.label().to_owned(),
            SettingMenu::VideoQuality => self.library.video_quality.label().to_owned(),
        };
        div()
            .py(px(20.))
            .flex()
            .items_center()
            .justify_between()
            .gap(px(20.))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(div().text_size(px(16.)).child(title)),
            )
            .child(
                div()
                    .id(("setting-value", kind as usize))
                    .relative()
                    .w(px(field_width))
                    .min_w(px(100.))
                    .flex_shrink_0()
                    .py(px(8.))
                    .border_b_1()
                    .border_color(gpui::white().opacity(0.2))
                    .flex()
                    .items_center()
                    .justify_between()
                    .text_size(px(14.))
                    .cursor_pointer()
                    .child(
                        canvas(
                            move |bounds, _, _| measured_bounds.set(bounds),
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .inset_0(),
                    )
                    .child(value)
                    .child(icon("back", 18., self.theme.muted).with_transformation(
                        Transformation::rotate(Radians(-std::f32::consts::FRAC_PI_2)),
                    ))
                    .on_click(cx.listener(move |view, _, _, cx| {
                        view.settings_menu = Some(kind);
                        let bounds = field_bounds.get();
                        view.overlay_anchor = Some(point(bounds.left(), bounds.bottom()));
                        cx.notify();
                    })),
            )
            .into_any_element()
    }

    pub(in crate::app) fn settings_dialog(
        &self,
        window: &Window,
        cx: &Context<Self>,
    ) -> AnyElement {
        let field_width = if window.viewport_size().width < px(936.) {
            128.
        } else {
            186.
        };
        let mut categories = div().w(px(234.)).py(px(24.));
        for (section, label) in [
            (SettingsSection::General, "General"),
            (SettingsSection::Playback, "Playback"),
            (SettingsSection::About, "About"),
        ] {
            categories = categories.child(
                div()
                    .id(("settings-category", section as usize))
                    .h(px(48.))
                    .px(px(24.))
                    .flex()
                    .items_center()
                    .text_size(px(14.))
                    .cursor_pointer()
                    .when(self.settings_section == section, |row| {
                        row.bg(rgb(0x4e4e4e))
                    })
                    .hover(|row| row.bg(gpui::white().opacity(0.1)))
                    .child(label)
                    .on_click(cx.listener(move |view, _, _, cx| {
                        view.settings_section = section;
                        view.settings_menu = None;
                        cx.notify();
                    })),
            );
        }
        let mut content = div()
            .id("settings-content")
            .flex_1()
            .h_full()
            .overflow_y_scroll()
            .border_l_1()
            .border_color(rgb(0x4e4e4e))
            .pl(px(48.))
            .pr(px(24.))
            .py(px(24.));
        match self.settings_section {
            SettingsSection::General => {
                content = content.child(
                    div()
                        .py(px(20.))
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap(px(20.))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_size(px(16.))
                                .child("Don't play music videos"),
                        )
                        .child(
                            div()
                                .id("audio-only-setting")
                                .w(px(36.))
                                .h(px(24.))
                                .flex_shrink_0()
                                .flex()
                                .items_center()
                                .cursor_pointer()
                                .child(
                                    div()
                                        .relative()
                                        .w(px(36.))
                                        .h(px(14.))
                                        .rounded_full()
                                        .bg(if self.library.video {
                                            rgb(0x606060)
                                        } else {
                                            rgb(0x7d2436)
                                        })
                                        .child(
                                            div()
                                                .absolute()
                                                .top(px(-3.))
                                                .left(px(if self.library.video { 0. } else { 16. }))
                                                .size(px(20.))
                                                .rounded_full()
                                                .bg(if self.library.video {
                                                    rgb(0xaaaaaa)
                                                } else {
                                                    rgb(0xff0033)
                                                }),
                                        ),
                                )
                                .on_click(cx.listener(|view, _, _, cx| {
                                    view.set_video_mode(!view.library.video, cx)
                                })),
                        ),
                );
            }
            SettingsSection::Playback => {
                content = content
                    .child(self.setting_row(
                        "Audio quality",
                        SettingMenu::AudioQuality,
                        field_width,
                        cx,
                    ))
                    .child(self.setting_row(
                        "Video quality",
                        SettingMenu::VideoQuality,
                        field_width,
                        cx,
                    ))
                    .child(self.setting_row("Playback speed", SettingMenu::Speed, field_width, cx));
            }
            SettingsSection::About => {
                content = content
                    .child(div().py(px(20.)).text_size(px(16.)).child("YouTube Music"))
                    .child(
                        div()
                            .text_size(px(14.))
                            .text_color(self.theme.muted)
                            .child(format!("Version {}", env!("CARGO_PKG_VERSION"))),
                    )
                    .child(
                        div()
                            .id("music-project")
                            .mt(px(24.))
                            .text_size(px(14.))
                            .text_color(rgb(0x3ea6ff))
                            .cursor_pointer()
                            .child("Project website")
                            .on_click(|_, _, cx| {
                                cx.open_url("https://github.com/AksharP5/yt-music-gpui")
                            }),
                    );
            }
        }
        let body = div()
            .w(px(if window.viewport_size().width >= px(936.) {
                900.
            } else {
                568.
            }))
            .max_w(window.viewport_size().width - px(48.))
            .h(px(583.))
            .max_h(window.viewport_size().height - px(48.))
            .flex()
            .flex_col()
            .rounded(px(3.))
            .bg(self.theme.panel)
            .border_1()
            .border_color(gpui::white().opacity(0.2))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .px(px(24.))
                    .py(px(19.))
                    .border_b_1()
                    .border_color(gpui::white().opacity(0.2))
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(div().text_size(px(20.)).child("Settings"))
                    .child(
                        button("close-settings", "close", self.theme)
                            .size(px(24.))
                            .on_click(
                                cx.listener(|view, _, window, cx| view.close_overlay(window, cx)),
                            ),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .child(categories)
                    .child(content),
            );
        let mut overlay = div()
            .absolute()
            .inset_0()
            .child(self.backdrop(body.into_any_element(), cx));
        if let Some(kind) = self.settings_menu {
            let (choices, selected): (Vec<String>, Option<usize>) = match kind {
                SettingMenu::Speed => (
                    PlaybackSpeed::ALL
                        .map(|speed| format!("{}×", f64::from(speed)))
                        .to_vec(),
                    PlaybackSpeed::ALL
                        .iter()
                        .position(|speed| *speed == self.library.speed),
                ),
                SettingMenu::AudioQuality => (
                    AudioQuality::ALL
                        .map(|quality| quality.label().to_owned())
                        .to_vec(),
                    AudioQuality::ALL
                        .iter()
                        .position(|quality| *quality == self.library.quality),
                ),
                SettingMenu::VideoQuality => (
                    VideoQuality::ALL
                        .map(|quality| quality.label().to_owned())
                        .to_vec(),
                    VideoQuality::ALL
                        .iter()
                        .position(|quality| *quality == self.library.video_quality),
                ),
            };
            let menu = div()
                .id("settings-options")
                .w(px(186.))
                .max_h(px(320.))
                .overflow_y_scroll()
                .py(px(8.))
                .bg(rgb(0x4e4e4e))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .children(choices.into_iter().enumerate().map(|(index, label)| {
                    div()
                        .id(("setting-option", index))
                        .px(px(16.))
                        .h(px(40.))
                        .flex()
                        .items_center()
                        .text_size(px(14.))
                        .cursor_pointer()
                        .when(selected == Some(index), |row| {
                            row.bg(gpui::white().opacity(0.1))
                        })
                        .hover(|row| row.bg(gpui::white().opacity(0.1)))
                        .child(label)
                        .on_click(cx.listener(move |view, _, _, cx| {
                            view.settings_menu = None;
                            match kind {
                                SettingMenu::Speed => {
                                    let speed = PlaybackSpeed::ALL[index];
                                    view.library.speed = speed;
                                    view.player_command(|player| player.set_speed(speed));
                                    view.save();
                                    cx.notify();
                                }
                                SettingMenu::AudioQuality => {
                                    view.set_audio_quality(AudioQuality::ALL[index], cx)
                                }
                                SettingMenu::VideoQuality => {
                                    view.set_video_quality(VideoQuality::ALL[index], cx)
                                }
                            }
                        }))
                }));
            overlay = overlay.child(
                div()
                    .absolute()
                    .inset_0()
                    .occlude()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|view, _, _, cx| {
                            cx.stop_propagation();
                            view.settings_menu = None;
                            cx.notify();
                        }),
                    )
                    .child(
                        anchored()
                            .position(self.overlay_anchor.unwrap_or_else(|| {
                                point(window.viewport_size().width / 2., px(200.))
                            }))
                            .snap_to_window_with_margin(px(24.))
                            .child(menu),
                    ),
            );
        }
        overlay.into_any_element()
    }
}
