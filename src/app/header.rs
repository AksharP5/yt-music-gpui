//! Search header, account avatar, and top-level controls.

use super::*;

impl Music {
    pub(in crate::app) fn account_avatar(&mut self, edge: f32) -> AnyElement {
        let thumbnail = self
            .account
            .as_ref()
            .and_then(|connection| connection.account.thumbnail.as_deref());
        let image = thumbnail.and_then(|url| self.artwork.get(url));
        let initial = self
            .account
            .as_ref()
            .and_then(|connection| connection.account.name.chars().next())
            .map(|initial| initial.to_uppercase().to_string())
            .unwrap_or_default();
        div()
            .size(px(edge))
            .flex_shrink_0()
            .rounded_full()
            .overflow_hidden()
            .bg(rgb(0x3f3f3f))
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(edge * 0.5))
            .when_some(image.clone(), |avatar, image| {
                avatar.child(
                    img(image)
                        .size_full()
                        .object_fit(ObjectFit::Cover)
                        .rounded_full(),
                )
            })
            .when(image.is_none(), |avatar| avatar.child(initial))
            .into_any_element()
    }

    pub(in crate::app) fn header(
        &mut self,
        inset: f32,
        compact: bool,
        width: f32,
        cx: &Context<Self>,
    ) -> AnyElement {
        let avatar = self.account.is_some().then(|| self.account_avatar(26.));
        let account_inset = if matches!(self.route, Route::Search(_)) {
            if width < 936. { 50. } else { 68. }
        } else {
            100.
        };
        let scrolled = self.page_scrolled();
        // Home tints the content header; the guide keeps its own background.
        let home_band = self.route.is_home() && !self.player_open && !scrolled;
        div()
            .h(px(64.))
            .w_full()
            .flex_shrink_0()
            .relative()
            .flex()
            .items_center()
            .when(home_band, |d| d.bg(rgb(0x14252c)))
            .child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .h_full()
                    .w(px(if compact { 72. } else { 240. }))
                    .bg(self.theme.bg)
                    .border_r_1()
                    .border_color(self.theme.border),
            )
            .when(!home_band && (scrolled || !self.immersive_header()), |d| {
                d.bg(self.theme.bg)
            })
            .child(
                div()
                    .w(px(if compact { 148. } else { 240. }))
                    .flex_shrink_0()
                    .px(px(16.))
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .child(
                        button("guide-toggle", "menu", self.theme)
                            .size(px(40.))
                            .on_click(cx.listener(|view, _, _, cx| {
                                view.sidebar_collapsed = !view.sidebar_collapsed;
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .id("brand-home")
                            .keyboard_control(self.theme)
                            // The focus shadow supplies the ring without a 1px layout inset.
                            .border_0()
                            .flex_shrink_0()
                            .cursor_pointer()
                            .child(
                                img(|window: &mut Window, cx: &mut App| {
                                    let scale = window.scale_factor();
                                    window.use_asset::<crate::assets::HeaderLogo>(
                                        &size(
                                            DevicePixels((71. * scale).round() as i32),
                                            DevicePixels((24. * scale).round() as i32),
                                        ),
                                        cx,
                                    )
                                })
                                .w(px(71.))
                                .h(px(24.)),
                            )
                            .on_click(cx.listener(|view, _, _, cx| view.navigate(Route::Home, cx))),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .pl(px(if compact {
                        0.
                    } else {
                        inset
                            + if matches!(self.route, Route::Search(_)) {
                                ((width - 240. - 1060.) / 2.).max(0.)
                            } else {
                                0.
                            }
                    }))
                    .pr(px(account_inset))
                    .flex()
                    .items_center()
                    .gap(px(24.))
                    .child(self.search.clone())
                    .child(div().flex_1())
                    // Signed in, the web keeps History here rather than in the library.
                    .when(
                        avatar.is_some() && !matches!(self.route, Route::Search(_)),
                        |header| {
                            header.child(button("history-nav", "history", self.theme).on_click(
                                cx.listener(|view, _, window, cx| {
                                    window.focus(&view.focus, cx);
                                    view.navigate(Route::Recent, cx);
                                }),
                            ))
                        },
                    )
                    .when_some(avatar, |header, avatar| {
                        header.child(
                            div()
                                .id("account-menu")
                                .keyboard_control(self.theme)
                                .size(px(40.))
                                .flex_shrink_0()
                                .flex()
                                .items_center()
                                .justify_center()
                                .cursor_pointer()
                                .child(avatar)
                                .on_click(cx.listener(|view, _, window, cx| {
                                    view.overlay_return_focus = window.focused(cx);
                                    window.focus(&view.overlay_focus, cx);
                                    view.overlay_anchor = Some(point(
                                        window.viewport_size().width
                                            - px(if matches!(view.route, Route::Search(_)) {
                                                68.
                                            } else {
                                                100.
                                            })
                                            - px(300.),
                                        px(56.),
                                    ));
                                    view.overlay = Some(Overlay::Account);
                                    cx.notify();
                                })),
                        )
                    })
                    .when(self.account.is_none(), |header| {
                        header.child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(8.))
                                .child(button("guest-account-menu", "more", self.theme).on_click(
                                    cx.listener(move |view, _, window, cx| {
                                        view.overlay_return_focus = window.focused(cx);
                                        window.focus(&view.overlay_focus, cx);
                                        view.overlay_anchor = Some(point(
                                            window.viewport_size().width - px(account_inset + 380.),
                                            px(56.),
                                        ));
                                        view.overlay = Some(Overlay::Account);
                                        cx.notify();
                                    }),
                                ))
                                .child(
                                    div()
                                        .id("sign-in")
                                        .keyboard_control(self.theme)
                                        .h(px(32.))
                                        .px(px(16.))
                                        .flex_shrink_0()
                                        .rounded_full()
                                        .bg(rgb(0xf1f1f1))
                                        .text_color(rgb(0x0f0f0f))
                                        .text_size(px(14.))
                                        .font_weight(FontWeight::MEDIUM)
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .cursor_pointer()
                                        .hover(|button| button.bg(rgb(0xd9d9d9)))
                                        .child("Sign in")
                                        .on_click(cx.listener(|view, _, window, cx| {
                                            view.connect_account(window, cx)
                                        })),
                                ),
                        )
                    }),
            )
            .into_any_element()
    }

    /// Whether the page has scrolled away from its top, where the web turns
    /// the header solid and draws a divider under it.
    pub(in crate::app) fn page_scrolled(&self) -> bool {
        if self.player_open {
            return false;
        }
        let top = self.detail_list.state.logical_scroll_top();
        top.item_ix > 0
            || top.offset_in_item > px(0.)
            || self.list_scroll.0.borrow().base_handle.offset().y < px(0.)
    }

    /// The 1px rule under a scrolled header, drawn over the page below it.
    pub(in crate::app) fn header_divider(&self) -> Option<Div> {
        self.page_scrolled().then(|| {
            div()
                .absolute()
                .top(px(64.))
                .left_0()
                .right_0()
                .h(px(1.))
                .bg(self.theme.border)
        })
    }
}
