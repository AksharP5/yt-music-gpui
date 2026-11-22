//! Primary navigation and sidebar playlists.

use super::*;

impl Music {
    pub(in crate::app) fn nav(
        &self,
        id: &'static str,
        glyphs: (&'static str, &'static str),
        label: &'static str,
        route: Route,
        compact: bool,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let active = self.route == route
            || (route == Route::Home && self.route.is_home())
            || (route == Route::Saved && matches!(self.route, Route::Library(_)));
        div()
            .id(id)
            .keyboard_control(self.theme)
            .h(px(if compact { 65. } else { 48. }))
            .flex_shrink_0()
            .px(px(if compact { 4. } else { 15. }))
            .flex()
            .items_center()
            .when(compact, |d| d.flex_col().justify_center())
            .gap(px(if compact { 5. } else { 20. }))
            .rounded(px(8.))
            .text_size(px(if compact { 10. } else { 16. }))
            .when(compact, |d| d.line_height(px(12.)))
            .text_color(self.theme.text)
            .when(active, |d| d.bg(self.theme.selection))
            .hover(|d| d.bg(gpui::white().opacity(0.1)))
            .cursor_pointer()
            .when(active, |d| d.font_weight(FontWeight::MEDIUM))
            .child(icon(
                if active { glyphs.1 } else { glyphs.0 },
                24.,
                self.theme.text,
            ))
            .child(label)
            .on_click(cx.listener(move |view, _, window, cx| {
                window.focus(&view.focus, cx);
                view.navigate(route.clone(), cx);
            }))
    }

    /// The full guide drawn over the dimmed window, as the web opens it below 936px.
    pub(in crate::app) fn guide_drawer(&self, cx: &Context<Self>) -> AnyElement {
        div()
            .id("guide-drawer")
            .absolute()
            .inset_0()
            .occlude()
            .bg(gpui::black().opacity(0.5))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|view, _, _, cx| {
                    cx.stop_propagation();
                    view.guide_drawer = false;
                    cx.notify();
                }),
            )
            .child(
                div()
                    .id("guide-drawer-panel")
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left_0()
                    .w(px(240.))
                    .flex()
                    .flex_col()
                    .bg(self.theme.bg)
                    .border_r_1()
                    .border_color(self.theme.border)
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(
                        div()
                            .h(px(HEADER_HEIGHT))
                            .flex_shrink_0()
                            .px(px(16.))
                            .flex()
                            .items_center()
                            .gap(px(12.))
                            .child(button("guide-close", "close", self.theme).on_click(
                                cx.listener(|view, _, _, cx| {
                                    view.guide_drawer = false;
                                    cx.notify();
                                }),
                            ))
                            .child(self.brand_home("drawer-brand-home", cx)),
                    )
                    .child(self.sidebar(false, true, cx)),
            )
            .into_any_element()
    }

    pub(in crate::app) fn sidebar(
        &self,
        compact: bool,
        drawer: bool,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let width = if compact { 72. } else { 240. };
        // The drawer draws its own background and edge.
        let scrolled = !drawer && self.page_scrolled();
        let mut sidebar = div()
            .id("sidebar")
            .relative()
            .w(px(width))
            .when(drawer, |sidebar| sidebar.flex_1().min_h_0())
            .when(!drawer, |sidebar| sidebar.h_full())
            .flex_shrink_0()
            .flex()
            .flex_col()
            .px(px(8.))
            .when(!drawer, |sidebar| sidebar.pt(px(8.)))
            .when(!compact && !drawer, |sidebar| {
                sidebar.border_r_1().border_color(self.theme.border)
            })
            .when(!compact || scrolled, |sidebar| sidebar.bg(self.theme.bg))
            .when(scrolled, |sidebar| {
                sidebar.child(
                    div()
                        .absolute()
                        .left(px(width))
                        .top_0()
                        .bottom_0()
                        .w(px(1.))
                        .bg(self.theme.bg.blend(gpui::white().opacity(0.1))),
                )
            })
            .child(self.nav(
                "home-nav",
                ("home-outlined", "home-filled"),
                "Home",
                Route::Home,
                compact,
                cx,
            ))
            .child(self.nav(
                "explore-nav",
                ("explore-outlined", "explore-filled"),
                "Explore",
                Route::Explore,
                compact,
                cx,
            ))
            .child(self.nav(
                "saved-nav",
                ("bookmark", "bookmark-filled"),
                "Library",
                Route::Saved,
                compact,
                cx,
            ));
        let guest = self.account.is_none();
        if compact {
            // The web's rail offers Sign in below a divider.
            return sidebar
                .when(guest, |sidebar| {
                    sidebar.child(
                        div()
                            .id("guide-sign-in")
                            .keyboard_control(self.theme)
                            .mx(px(-8.))
                            .pt(px(12.))
                            .pb(px(12.))
                            .border_t_1()
                            .border_color(gpui::white().opacity(0.15))
                            .flex()
                            .flex_col()
                            .items_center()
                            .gap(px(5.))
                            .text_size(px(10.))
                            .line_height(px(16.))
                            .text_color(self.theme.text)
                            .cursor_pointer()
                            .child(icon("sign-in", 24., self.theme.text))
                            .child("Sign in")
                            .on_click(
                                cx.listener(|view, _, window, cx| view.connect_account(window, cx)),
                            ),
                    )
                })
                .into_any_element();
        }
        sidebar = sidebar.child(
            div()
                .h(px(1.))
                .mx(px(16.))
                .my(px(24.))
                .bg(self.theme.border),
        );
        // Guests get the web's Sign in prompt where the account's New playlist
        // button sits; their local likes, history and saved collections follow.
        sidebar = if guest {
            sidebar.child(
                div()
                    .ml(px(16.))
                    .w(px(192.))
                    .mb(px(16.))
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .child(
                        div()
                            .id("guide-sign-in")
                            .keyboard_control(self.theme)
                            .h(px(36.))
                            .rounded_full()
                            .bg(gpui::white().opacity(0.1))
                            .hover(|d| d.bg(gpui::white().opacity(0.2)))
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_size(px(14.))
                            .font_weight(FontWeight::MEDIUM)
                            .cursor_pointer()
                            .child("Sign in")
                            .on_click(
                                cx.listener(|view, _, window, cx| view.connect_account(window, cx)),
                            ),
                    )
                    .child(
                        div()
                            .whitespace_normal()
                            .text_size(px(12.))
                            .line_height(px(18.))
                            .text_color(rgb(0x909090))
                            .child(
                                "Sign in to create & share playlists, get personalized \
                                 recommendations, and more.",
                            ),
                    ),
            )
        } else {
            sidebar.child(
                div()
                    .id("sidebar-new-playlist")
                    .keyboard_control(self.theme)
                    .mx(px(12.))
                    .h(px(36.))
                    .mb(px(16.))
                    .rounded_full()
                    .bg(self.theme.selection)
                    .flex()
                    .items_center()
                    .justify_center()
                    .gap(px(8.))
                    .text_size(px(14.))
                    .font_weight(FontWeight::MEDIUM)
                    .cursor_pointer()
                    .hover(|d| d.bg(gpui::white().opacity(0.1)))
                    .child(icon("plus", 20., self.theme.text))
                    .child("New playlist")
                    .on_click(cx.listener(|view, _, window, cx| {
                        if view.account.is_some() {
                            view.playlist_form(false, Vec::new(), window, cx);
                        } else {
                            view.connect_account(window, cx);
                        }
                    })),
            )
        };
        for (id, title, subtitle, route) in [
            ("liked-nav", "Liked Music", "Auto playlist", Route::Liked),
            ("recent-nav", "History", "Recently played", Route::Recent),
        ]
        .into_iter()
        .filter(|(id, _, _, _)| self.account.is_none() || *id != "recent-nav")
        {
            sidebar = sidebar.child(
                div()
                    .id(id)
                    .keyboard_control(self.theme)
                    .h(px(56.))
                    .px(px(16.))
                    .rounded(px(8.))
                    .flex()
                    .flex_col()
                    .justify_center()
                    .cursor_pointer()
                    .when(self.route == route, |d| d.bg(self.theme.selection))
                    .hover(|d| d.bg(gpui::white().opacity(0.1)))
                    .child(entry_lines(title, subtitle, self.theme))
                    .on_click(cx.listener(move |view, _, window, cx| {
                        window.focus(&view.focus, cx);
                        view.navigate(route.clone(), cx);
                    })),
            );
        }
        if self.account.is_some() {
            sidebar = sidebar.child(
                div()
                    .id("saved-episodes-nav")
                    .keyboard_control(self.theme)
                    .h(px(56.))
                    .px(px(16.))
                    .rounded(px(8.))
                    .flex()
                    .flex_col()
                    .justify_center()
                    .cursor_pointer()
                    .hover(|d| d.bg(gpui::white().opacity(0.1)))
                    .child(entry_lines(
                        "Episodes for Later",
                        "Auto playlist",
                        self.theme,
                    ))
                    .on_click(cx.listener(|view, _, _, cx| {
                        view.navigate(Route::Library(Section::Episodes), cx)
                    })),
            );
        }
        let playlists = if self.account.is_some() {
            &self.sidebar_playlists
        } else {
            &self.library.saved
        };
        sidebar = sidebar.child(
            uniform_list(
                "sidebar-playlists",
                playlists.len(),
                cx.processor(|view, range: std::ops::Range<usize>, window, cx| {
                    if view.account.is_some()
                        && range.end.saturating_add(5) >= view.sidebar_playlists.len()
                        && !view.catalog_requests.is_loading(CatalogTarget::Sidebar)
                        && view.sidebar_error.is_none()
                        && let Some(token) = view.sync.sidebar_continuation.clone()
                    {
                        view.request(Query::SidebarMore(token), cx);
                    }
                    view.sidebar_focus
                        .retain(range.clone(), window, |index| index);
                    let playlists = if view.account.is_some() {
                        &view.sidebar_playlists
                    } else {
                        &view.library.saved
                    };
                    range
                        .map(|index| {
                            let playlist = &playlists[index];
                            let link = BrowseLink::new(playlist.id.clone(), playlist.title.clone());
                            div()
                                .id(("sidebar-playlist", index))
                                .keyboard_control(view.theme)
                                .track_focus(&view.sidebar_focus.handle(index, cx))
                                .h(px(56.))
                                .px(px(16.))
                                .rounded(px(8.))
                                .flex()
                                .flex_col()
                                .justify_center()
                                .cursor_pointer()
                                .hover(|d| d.bg(view.theme.raised))
                                .child(entry_lines(
                                    playlist.title.clone(),
                                    playlist.subtitle.clone(),
                                    view.theme,
                                ))
                                .on_click(cx.listener(move |view, _, window, cx| {
                                    window.focus(&view.focus, cx);
                                    view.navigate(Route::Browse(link.clone()), cx);
                                }))
                                .into_any_element()
                        })
                        .collect::<Vec<_>>()
                }),
            )
            .track_scroll(&self.sidebar_scroll)
            .flex_1()
            .min_h_0(),
        );
        if let Some(error) = &self.sidebar_error {
            sidebar = sidebar.child(
                div()
                    .px(px(16.))
                    .py(px(12.))
                    .text_size(px(12.))
                    .text_color(self.theme.muted)
                    .child(error.clone())
                    .child(
                        div()
                            .id("retry-sidebar")
                            .keyboard_control(self.theme)
                            .mt(px(8.))
                            .text_color(self.theme.text)
                            .cursor_pointer()
                            .child("Retry playlists")
                            .on_click(cx.listener(|view, _, _, cx| {
                                view.request(Query::SidebarPlaylists, cx)
                            })),
                    ),
            );
        }
        sidebar.into_any_element()
    }
}

/// A guide entry's title and subtitle, 20px apart like the web's.
fn entry_lines(
    title: impl Into<SharedString>,
    subtitle: impl Into<SharedString>,
    theme: Theme,
) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(2.))
        .child(
            div()
                .truncate()
                .text_size(px(14.))
                .line_height(px(18.))
                .font_weight(FontWeight::MEDIUM)
                .child(title.into()),
        )
        .child(
            div()
                .truncate()
                .text_size(px(12.))
                .line_height(px(16.))
                .text_color(theme.muted)
                .child(subtitle.into()),
        )
}
