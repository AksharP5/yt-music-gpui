//! Guest and account library navigation filters.

use super::super::*;

impl Music {
    pub(in crate::app) fn library_filters(
        &mut self,
        mut content: Div,
        inset: f32,
        cx: &mut Context<Self>,
    ) -> Div {
        let library = matches!(self.route, Route::Saved | Route::Library(_));
        if self.account.is_none()
            && matches!(self.route, Route::Saved | Route::Liked | Route::Recent)
        {
            content = content.child(
                div()
                    .id("local-library-sections")
                    .mx(px(inset))
                    .mb(px(24.))
                    .flex()
                    .gap(px(12.))
                    .children(
                        [
                            (Route::Saved, "Playlists"),
                            (Route::Liked, "Liked Music"),
                            (Route::Recent, "History"),
                        ]
                        .into_iter()
                        .enumerate()
                        .map(|(index, (route, label))| {
                            filter_chip(
                                ("local-library-section", index),
                                label,
                                self.route == route,
                                self.theme,
                            )
                            .on_click(
                                cx.listener(move |view, _, _, cx| view.navigate(route.clone(), cx)),
                            )
                        }),
                    ),
            );
        }
        if self.account.is_some() && (library || matches!(self.route, Route::Liked | Route::Recent))
        {
            let selected = match self.route {
                Route::Liked => Section::Liked,
                Route::Recent => Section::History,
                Route::Library(section) => section,
                _ => Section::Songs,
            };
            content = content.child(
                div()
                    .id("library-sections")
                    .mx(px(inset))
                    .mb(px(28.))
                    .flex_shrink_0()
                    .flex()
                    .gap(px(12.))
                    .overflow_x_scroll()
                    .children(
                        [
                            Section::Playlists,
                            Section::Podcasts,
                            Section::Songs,
                            Section::Albums,
                            Section::Artists,
                            Section::Subscriptions,
                            Section::Liked,
                            Section::History,
                            Section::Episodes,
                            Section::Uploads,
                            Section::UploadAlbums,
                            Section::UploadArtists,
                        ]
                        .into_iter()
                        .enumerate()
                        .map(|(index, section)| {
                            filter_chip(
                                ("library-section", index),
                                section.title(),
                                section == selected,
                                self.theme,
                            )
                            .on_click(cx.listener(
                                move |view, _, _, cx| view.navigate(Route::Library(section), cx),
                            ))
                        }),
                    ),
            );
        }
        content
    }
}
