//! Search header, account avatar, and top-level controls.

use super::*;

impl Music {
    pub(in crate::app) fn account_avatar(&mut self, edge: f32) -> AnyElement {
        let thumbnail = self
            .account
            .as_ref()
            .and_then(|connection| connection.account.thumbnail.as_deref());
        let image = thumbnail.and_then(|url| self.artwork.get(url, size(px(edge), px(edge))));
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
        compact: bool,
        width: f32,
        cx: &Context<Self>,
    ) -> AnyElement {
        let avatar = self.account.is_some().then(|| self.account_avatar(26.));
        // Below 936px the web folds the search field into a button, except on the
        // search page, and opens it as a field over the left of the header.
        let search = matches!(self.route, Route::Search(_));
        let narrow = width < 936. && !search;
        // The web's account controls keep this padding inside a header that,
        // like the page, stops at the scrollbar.
        let account_inset = pages::SCROLLBAR_WIDTH
            + if width < 936. {
                50.
            } else if search {
                68.
            } else {
                100.
            };
        let scrolled = self.page_scrolled();
        let guide_width = if compact { 72. } else { 240. };
        let layout = pages::PageLayout::new(width, width - guide_width);
        let brand_width = if compact && width < 1150. {
            148.
        } else {
            guide_width
        };
        let search_left = if search {
            guide_width + layout.search_inset() + 1.
        } else if narrow {
            brand_width
        } else if compact && width < 1150. {
            // Below the wide breakpoint, center between the brand and account controls.
            brand_width + ((width - brand_width - 194. - crate::search_box::WIDTH) / 2.).max(0.)
        } else {
            guide_width + layout.inset
        };
        // Only an expanded, unscrolled guide has its own header background.
        div()
            .h(px(HEADER_HEIGHT))
            .w_full()
            .flex_shrink_0()
            .relative()
            .flex()
            .items_center()
            .when(!compact && !scrolled, |header| {
                header.child(
                    div()
                        .absolute()
                        .top_0()
                        .left_0()
                        .h_full()
                        .w(px(240.))
                        .bg(self.theme.bg)
                        .border_r_1()
                        .border_color(self.theme.border),
                )
            })
            .when(scrolled || !self.immersive_header(), |d| {
                d.bg(self.theme.bg)
            })
            .child(
                div()
                    .w(px(brand_width))
                    .flex_shrink_0()
                    .px(px(16.))
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .child(
                        button("guide-toggle", "menu", self.theme)
                            .size(px(40.))
                            .on_click(cx.listener(move |view, _, window, cx| {
                                if f32::from(window.viewport_size().width) < GUIDE_DRAWER_BELOW {
                                    view.guide_drawer = true;
                                } else {
                                    view.sidebar_collapsed = !compact;
                                }
                                cx.notify();
                            })),
                    )
                    .child(self.brand_home("brand-home", cx)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .pl(px((search_left - brand_width).max(0.)))
                    .pr(px(account_inset))
                    .flex()
                    .items_center()
                    .gap(px(24.))
                    .when(!narrow, |header| {
                        header.child(div().flex_1().min_w_0().flex().child(self.search.clone()))
                    })
                    .when(narrow, |header| {
                        header
                            .child(div().flex_1())
                            .when(!self.search_open, |header| {
                                header.child(
                                    div()
                                        .id("search-open")
                                        .w(px(52.))
                                        .h(px(40.))
                                        // The web leaves 40px before the account controls.
                                        .mr(px(16.))
                                        .flex_shrink_0()
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .cursor_pointer()
                                        .child(
                                            div()
                                                .size(px(32.))
                                                .rounded_full()
                                                .flex()
                                                .items_center()
                                                .justify_center()
                                                .hover(|button| button.bg(self.theme.raised))
                                                .child(icon("search", 18., self.theme.text)),
                                        )
                                        .on_click(cx.listener(|view, _, window, cx| {
                                            view.open_search(window, cx)
                                        })),
                                )
                            })
                    })
                    // Signed in, the web keeps History here rather than in the library.
                    .when(avatar.is_some() && !search, |header| {
                        header.child(button("history-nav", "history", self.theme).on_click(
                            cx.listener(|view, _, window, cx| {
                                window.focus(&view.focus, cx);
                                view.navigate(Route::Recent, cx);
                            }),
                        ))
                    })
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
                                .on_click(cx.listener(move |view, _, window, cx| {
                                    view.open_overlay(Overlay::Account, window, cx);
                                    view.overlay_anchor = Some(point(
                                        window.viewport_size().width - px(account_inset + 300.),
                                        px(56.),
                                    ));
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
                                        view.open_overlay(Overlay::Account, window, cx);
                                        // Like the web, the menu hangs from the button's bottom
                                        // edge, its right edge level with the icon's.
                                        view.overlay_anchor = Some(point(
                                            window.viewport_size().width - px(account_inset + 391.),
                                            px(52.),
                                        ));
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
            .when(narrow && self.search_open, |header| {
                header.child(
                    div()
                        .absolute()
                        .left(px(17.))
                        .top(px((HEADER_HEIGHT - 42.) / 2.))
                        // The field covers the guide button and logo.
                        .occlude()
                        .child(self.search.clone()),
                )
            })
            .into_any_element()
    }

    /// Focus the search field, first opening it from its button in a narrow window.
    pub(in crate::app) fn open_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let narrow = f32::from(window.viewport_size().width) < 936.
            && !matches!(self.route, Route::Search(_));
        self.search_open = narrow;
        self.search.update(cx, |search, cx| {
            search.set_back(narrow, cx);
            search.activate(window, cx);
        });
        cx.notify();
    }

    /// Fold the narrow window's open search field back into its button.
    pub(in crate::app) fn close_narrow_search(&mut self, cx: &mut Context<Self>) {
        if !self.search_open {
            return;
        }
        self.search_open = false;
        self.search
            .update(cx, |search, cx| search.set_back(false, cx));
        cx.notify();
    }

    /// The YouTube Music logo, which opens Home.
    pub(in crate::app) fn brand_home(&self, id: &'static str, cx: &Context<Self>) -> Stateful<Div> {
        div()
            .id(id)
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
            .on_click(cx.listener(|view, _, _, cx| view.navigate(Route::Home, cx)))
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
