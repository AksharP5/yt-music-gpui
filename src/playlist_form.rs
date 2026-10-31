use crate::{
    account::Privacy,
    input::{InputEvent, SearchInput},
    theme::Theme,
};
use gpui::{prelude::*, *};

const PRIVACY_CHOICES: [(&str, &str, Privacy); 3] = [
    ("private", "Private", Privacy::Private),
    ("unlisted", "Unlisted", Privacy::Unlisted),
    ("public", "Public", Privacy::Public),
];

pub enum PlaylistFormEvent {
    Save {
        title: String,
        description: String,
        privacy: Privacy,
    },
    Cancel,
}

pub struct PlaylistForm {
    title: Entity<SearchInput>,
    description: Entity<SearchInput>,
    privacy: Privacy,
    privacy_focus: [FocusHandle; PRIVACY_CHOICES.len()],
    cancel_focus: FocusHandle,
    save_focus: FocusHandle,
    theme: Theme,
    _subscriptions: [Subscription; 2],
}

impl EventEmitter<PlaylistFormEvent> for PlaylistForm {}

impl PlaylistForm {
    pub fn new(
        title: String,
        description: String,
        privacy: Privacy,
        cx: &mut Context<Self>,
    ) -> Self {
        let title = cx.new(|cx| {
            let mut input = SearchInput::new(cx);
            input.set_placeholder("Playlist title", cx);
            input.set_value(title, cx);
            input
        });
        let description = cx.new(|cx| {
            let mut input = SearchInput::new(cx);
            input.set_placeholder("Description (optional)", cx);
            input.set_value(description, cx);
            input
        });
        let subscriptions = [&title, &description].map(|input| {
            cx.subscribe(input, |form, _, event, cx| match event {
                InputEvent::Submit => form.save(cx),
                InputEvent::Blur => cx.emit(PlaylistFormEvent::Cancel),
                InputEvent::Changed => cx.notify(),
            })
        });
        Self {
            title,
            description,
            privacy,
            privacy_focus: std::array::from_fn(|_| cx.focus_handle()),
            cancel_focus: cx.focus_handle(),
            save_focus: cx.focus_handle(),
            theme: Theme::load(),
            _subscriptions: subscriptions,
        }
    }

    fn save(&self, cx: &mut Context<Self>) {
        let title = self.title.read(cx).value().trim().to_owned();
        if title.is_empty() {
            return;
        }
        cx.emit(PlaylistFormEvent::Save {
            title,
            description: self.description.read(cx).value().to_owned(),
            privacy: self.privacy,
        });
    }

    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let modifiers = event.keystroke.modifiers;
        if modifiers.control || modifiers.alt || modifiers.platform || modifiers.function {
            return;
        }
        let key = event.keystroke.key.as_str();
        if key == "escape" {
            cx.emit(PlaylistFormEvent::Cancel);
            cx.stop_propagation();
            return;
        }
        if key == "tab" {
            let handles = [
                self.title.focus_handle(cx),
                self.description.focus_handle(cx),
                self.privacy_focus[0].clone(),
                self.privacy_focus[1].clone(),
                self.privacy_focus[2].clone(),
                self.cancel_focus.clone(),
                self.save_focus.clone(),
            ];
            let count = handles.len() - usize::from(self.title.read(cx).value().trim().is_empty());
            let focused = handles[..count]
                .iter()
                .position(|handle| handle.is_focused(window));
            let next = if modifiers.shift {
                focused.map_or(count - 1, |index| (index + count - 1) % count)
            } else {
                focused.map_or(0, |index| (index + 1) % count)
            };
            window.focus(&handles[next], cx);
            cx.stop_propagation();
            return;
        }
        if modifiers.shift || !matches!(key, "enter" | "space") {
            return;
        }
        if let Some(index) = self
            .privacy_focus
            .iter()
            .position(|handle| handle.is_focused(window))
        {
            self.privacy = PRIVACY_CHOICES[index].2;
        } else if self.cancel_focus.is_focused(window) {
            cx.emit(PlaylistFormEvent::Cancel);
        } else if self.save_focus.is_focused(window) {
            self.save(cx);
        } else {
            return;
        }
        cx.stop_propagation();
        cx.notify();
    }
}

impl Focusable for PlaylistForm {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.title.focus_handle(cx)
    }
}

impl Render for PlaylistForm {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let can_save = !self.title.read(cx).value().trim().is_empty();
        div()
            .id("playlist-form")
            .w(px(480.))
            .max_w_full()
            .p(px(28.))
            .rounded(px(12.))
            .border_1()
            .border_color(theme.border)
            .bg(theme.panel)
            .text_color(theme.text)
            .text_size(px(14.))
            .flex()
            .flex_col()
            .gap(px(20.))
            .key_context("PlaylistForm")
            .on_key_down(cx.listener(Self::key_down))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(|_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .text_size(px(23.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Playlist details"),
            )
            .children(
                [
                    ("Title", self.title.clone()),
                    ("Description", self.description.clone()),
                ]
                .map(|(label, input)| {
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(8.))
                        .child(div().text_color(theme.muted).child(label))
                        .child(
                            div()
                                .px(px(12.))
                                .py(px(10.))
                                .bg(theme.bg)
                                .border_1()
                                .border_color(theme.border)
                                .rounded(px(6.))
                                .child(input),
                        )
                }),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .child(div().text_color(theme.muted).child("Privacy"))
                    .child(div().flex().gap(px(8.)).children(
                        PRIVACY_CHOICES.into_iter().enumerate().map(
                            |(index, (id, label, privacy))| {
                                let selected = self.privacy == privacy;
                                let focused = self.privacy_focus[index].is_focused(window);
                                div()
                                    .id(id)
                                    .track_focus(&self.privacy_focus[index])
                                    .flex_1()
                                    .py(px(10.))
                                    .text_center()
                                    .rounded(px(6.))
                                    .border_1()
                                    .border_color(if focused {
                                        theme.text
                                    } else if selected {
                                        theme.accent
                                    } else {
                                        theme.border
                                    })
                                    .bg(if selected {
                                        theme.selection
                                    } else {
                                        theme.panel
                                    })
                                    .cursor_pointer()
                                    .hover(|style| style.bg(theme.raised))
                                    .child(label)
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(move |form, _, window, cx| {
                                            window.focus(&form.privacy_focus[index], cx);
                                        }),
                                    )
                                    .on_click(cx.listener(move |form, _, _, cx| {
                                        form.privacy = privacy;
                                        cx.notify();
                                    }))
                            },
                        ),
                    )),
            )
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap(px(10.))
                    .mt(px(4.))
                    .child(
                        div()
                            .id("cancel-playlist")
                            .track_focus(&self.cancel_focus)
                            .px(px(18.))
                            .py(px(10.))
                            .rounded(px(6.))
                            .border_1()
                            .border_color(if self.cancel_focus.is_focused(window) {
                                theme.text
                            } else {
                                gpui::transparent_black()
                            })
                            .cursor_pointer()
                            .hover(|style| style.bg(theme.raised))
                            .child("Cancel")
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|form, _, window, cx| {
                                    window.focus(&form.cancel_focus, cx);
                                }),
                            )
                            .on_click(
                                cx.listener(|_, _, _, cx| cx.emit(PlaylistFormEvent::Cancel)),
                            ),
                    )
                    .child(
                        div()
                            .id("save-playlist")
                            .track_focus(&self.save_focus)
                            .px(px(22.))
                            .py(px(10.))
                            .rounded(px(6.))
                            .border_1()
                            .border_color(if self.save_focus.is_focused(window) {
                                theme.text
                            } else {
                                gpui::transparent_black()
                            })
                            .bg(if can_save { theme.accent } else { theme.raised })
                            .text_color(if can_save { theme.bg } else { theme.muted })
                            .font_weight(FontWeight::SEMIBOLD)
                            .when(can_save, |button| button.cursor_pointer())
                            .child("Save")
                            .when(can_save, |button| {
                                button.on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|form, _, window, cx| {
                                        window.focus(&form.save_focus, cx);
                                    }),
                                )
                            })
                            .on_click(cx.listener(|form, _, _, cx| form.save(cx))),
                    ),
            )
    }
}
