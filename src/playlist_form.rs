use crate::{
    account::Privacy,
    input::{InputEvent, SearchInput},
    theme::Theme,
};
use gpui::{prelude::*, *};

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
        if event.keystroke.modifiers.control && event.keystroke.key == "q" {
            return;
        }
        if event.keystroke.key == "tab" {
            let title = self.title.focus_handle(cx);
            let description = self.description.focus_handle(cx);
            window.focus(
                if title.is_focused(window) {
                    &description
                } else {
                    &title
                },
                cx,
            );
        }
        cx.stop_propagation();
    }
}

impl Focusable for PlaylistForm {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.title.focus_handle(cx)
    }
}

impl Render for PlaylistForm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
                    .child(
                        div().flex().gap(px(8.)).children(
                            [
                                ("private", "Private", Privacy::Private),
                                ("unlisted", "Unlisted", Privacy::Unlisted),
                                ("public", "Public", Privacy::Public),
                            ]
                            .map(|(id, label, privacy)| {
                                let selected = self.privacy == privacy;
                                div()
                                    .id(id)
                                    .flex_1()
                                    .py(px(10.))
                                    .text_center()
                                    .rounded(px(6.))
                                    .border_1()
                                    .border_color(if selected {
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
                                    .on_click(cx.listener(move |form, _, _, cx| {
                                        form.privacy = privacy;
                                        cx.notify();
                                    }))
                            }),
                        ),
                    ),
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
                            .px(px(18.))
                            .py(px(10.))
                            .rounded(px(6.))
                            .cursor_pointer()
                            .hover(|style| style.bg(theme.raised))
                            .child("Cancel")
                            .on_click(
                                cx.listener(|_, _, _, cx| cx.emit(PlaylistFormEvent::Cancel)),
                            ),
                    )
                    .child(
                        div()
                            .id("save-playlist")
                            .px(px(22.))
                            .py(px(10.))
                            .rounded(px(6.))
                            .bg(if can_save { theme.accent } else { theme.raised })
                            .text_color(if can_save { theme.bg } else { theme.muted })
                            .font_weight(FontWeight::SEMIBOLD)
                            .when(can_save, |button| button.cursor_pointer())
                            .child("Save")
                            .on_click(cx.listener(|form, _, _, cx| form.save(cx))),
                    ),
            )
    }
}
