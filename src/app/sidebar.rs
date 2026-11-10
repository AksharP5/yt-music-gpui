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
            .h(px(if compact { 68. } else { 48. }))
            .flex_shrink_0()
            .px(px(if compact { 4. } else { 15. }))
            .flex()
            .items_center()
            .when(compact, |d| d.flex_col().justify_center())
            .gap(px(if compact { 6. } else { 20. }))
            .rounded(px(8.))
            .text_size(px(if compact { 10. } else { 16. }))
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

    pub(in crate::app) fn sidebar(&self, compact: bool, cx: &Context<Self>) -> impl IntoElement {
        let mut sidebar = div()
            .id("sidebar")
            .w(px(if compact { 72. } else { 240. }))
            .h_full()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .px(px(if compact { 4. } else { 8. }))
            .pt(px(8.))
            .border_r_1()
            .border_color(self.theme.border)
            .bg(self.theme.bg)
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
        if compact {
            return sidebar.into_any_element();
        }
        sidebar = sidebar
            .child(
                div()
                    .h(px(1.))
                    .mx(px(16.))
                    .my(px(24.))
                    .bg(self.theme.border),
            )
            .child(
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
                        window.focus(&view.focus, cx);
                        if view.account.is_some() {
                            view.playlist_form(false, None, window, cx);
                        } else {
                            view.connect_account(window, cx);
                        }
                    })),
            );
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
                    .gap(px(4.))
                    .cursor_pointer()
                    .when(self.route == route, |d| d.bg(self.theme.selection))
                    .hover(|d| d.bg(gpui::white().opacity(0.1)))
                    .child(
                        div()
                            .text_size(px(14.))
                            .font_weight(FontWeight::MEDIUM)
                            .child(title),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(self.theme.muted)
                            .child(subtitle),
                    )
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
                    .gap(px(4.))
                    .cursor_pointer()
                    .hover(|d| d.bg(gpui::white().opacity(0.1)))
                    .child(
                        div()
                            .text_size(px(14.))
                            .font_weight(FontWeight::MEDIUM)
                            .child("Episodes for Later"),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(self.theme.muted)
                            .child("Auto playlist"),
                    )
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
                                .gap(px(4.))
                                .cursor_pointer()
                                .hover(|d| d.bg(view.theme.raised))
                                .child(
                                    div()
                                        .truncate()
                                        .text_size(px(14.))
                                        .font_weight(FontWeight::MEDIUM)
                                        .child(playlist.title.clone()),
                                )
                                .child(
                                    div()
                                        .truncate()
                                        .text_size(px(12.))
                                        .text_color(view.theme.muted)
                                        .child(playlist.subtitle.clone()),
                                )
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
