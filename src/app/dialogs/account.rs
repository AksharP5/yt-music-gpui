//! Account menu and sign-in dialog rendering.

use super::super::*;

impl Music {
    pub(in crate::app) fn account_action(
        &self,
        id: &'static str,
        glyph: &'static str,
        label: &'static str,
        cx: &Context<Self>,
        action: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> impl IntoElement {
        div()
            .id(id)
            .h(px(40.))
            .px(px(16.))
            .flex()
            .items_center()
            .gap(px(16.))
            .text_size(px(14.))
            .cursor_pointer()
            .hover(|row| row.bg(gpui::white().opacity(0.1)))
            .child(icon(glyph, 18., self.theme.text))
            .child(label)
            .on_click(cx.listener(move |view, _, window, cx| action(view, window, cx)))
    }

    pub(in crate::app) fn account_popup(
        &mut self,
        window: &Window,
        cx: &Context<Self>,
    ) -> AnyElement {
        let account = self
            .account
            .as_ref()
            .map(|connection| connection.account.clone());
        let separator = gpui::white().opacity(0.2);
        let mut body = div()
            .w(px(300.))
            .rounded(px(12.))
            .overflow_hidden()
            .bg(rgb(0x282828))
            .text_color(rgb(0xf1f1f1))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation());
        if let Some(account) = account {
            let avatar = self.account_avatar(40.);
            body = body
                .child(
                    div()
                        .p(px(16.))
                        .border_b_1()
                        .border_color(separator)
                        .flex()
                        .items_start()
                        .gap(px(16.))
                        .child(avatar)
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_size(px(16.))
                                .line_height(px(22.))
                                .child(
                                    div()
                                        .font_weight(FontWeight::MEDIUM)
                                        .truncate()
                                        .child(account.name),
                                )
                                .child(div().truncate().child(account.handle))
                                .child(
                                    div()
                                        .id("manage-google-account")
                                        .mt(px(8.))
                                        .text_size(px(14.))
                                        .text_color(rgb(0x3ea6ff))
                                        .cursor_pointer()
                                        .child("Manage your Google Account")
                                        .on_click(|_, _, cx| {
                                            cx.open_url("https://myaccount.google.com")
                                        }),
                                ),
                        ),
                )
                .child(
                    div()
                        .py(px(8.))
                        .border_b_1()
                        .border_color(separator)
                        .child(self.account_action(
                            "switch-account",
                            "switch-account",
                            "Switch account",
                            cx,
                            |view, window, cx| view.connect_account(window, cx),
                        ))
                        .child(self.account_action(
                            "sign-out",
                            "logout",
                            "Sign out",
                            cx,
                            |view, _, cx| {
                                view.overlay = None;
                                view.disconnect_account(cx);
                            },
                        )),
                );
        }
        body = body
            .child(div().py(px(8.)).border_b_1().border_color(separator).child(
                self.account_action(
                    "account-settings",
                    "settings",
                    "Settings",
                    cx,
                    |view, _, cx| {
                        view.settings_menu = None;
                        view.overlay = Some(Overlay::Settings);
                        cx.notify();
                    },
                ),
            ))
            .child(div().py(px(8.)).child(self.account_action(
                "account-help",
                "help",
                "Help",
                cx,
                |view, _, cx| {
                    view.overlay = None;
                    cx.open_url("https://support.google.com/youtubemusic");
                    cx.notify();
                },
            )));
        div()
            .absolute()
            .inset_0()
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|view, _, window, cx| {
                    cx.stop_propagation();
                    view.close_overlay(window, cx);
                }),
            )
            .child(
                anchored()
                    .position(
                        self.overlay_anchor.unwrap_or_else(|| {
                            point(window.viewport_size().width - px(400.), px(56.))
                        }),
                    )
                    .snap_to_window_with_margin(px(8.))
                    .child(body),
            )
            .into_any_element()
    }

    pub(in crate::app) fn sign_in_dialog(&self, cx: &Context<Self>) -> AnyElement {
        let checking = matches!(
            self.account_operation,
            Some(AccountOperation::Connecting(
                _,
                BrowserConnection::Reading(_)
            ))
        );
        let ready = matches!(
            self.account_operation,
            Some(AccountOperation::Connecting(_, BrowserConnection::Ready(_)))
        );
        let title = if checking {
            "Signing in…"
        } else {
            "Sign in to YouTube Music"
        };
        let body = div()
            .w(px(480.))
            .max_w(relative(0.9))
            .p(px(24.))
            .rounded(px(3.))
            .bg(self.theme.panel)
            .border_1()
            .border_color(gpui::white().opacity(0.2))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(16.))
                    .child(div().text_size(px(20.)).child(title))
                    .child(button("close-sign-in", "close", self.theme).on_click(
                        cx.listener(|view, _, window, cx| view.close_overlay(window, cx)),
                    )),
            )
            .child(
                div()
                    .mt(px(20.))
                    .text_size(px(14.))
                    .line_height(px(20.))
                    .text_color(self.theme.muted)
                    .child(if checking {
                        "Checking your account."
                    } else {
                        "Finish signing in in your browser, then return to Music."
                    }),
            )
            .when_some(self.error.as_ref(), |body, error| {
                body.child(
                    div()
                        .mt(px(16.))
                        .text_size(px(14.))
                        .text_color(rgb(0xf1a6ae))
                        .child(error.clone()),
                )
            })
            .when(
                (ready || self.account_operation.is_none()) && self.error.is_some(),
                |body| {
                    body.child(
                        div().mt(px(24.)).flex().justify_end().child(
                            div()
                                .id("retry-sign-in")
                                .px(px(16.))
                                .h(px(36.))
                                .flex()
                                .items_center()
                                .rounded_full()
                                .bg(self.theme.text)
                                .text_color(self.theme.bg)
                                .cursor_pointer()
                                .child("Try again")
                                .on_click(cx.listener(|view, _, window, cx| {
                                    view.connect_account(window, cx)
                                })),
                        ),
                    )
                },
            );
        self.backdrop(body.into_any_element(), cx)
    }
}
