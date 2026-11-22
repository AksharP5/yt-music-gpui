//! The Song credits dialog, laid out as music.youtube.com's.

use super::*;
use crate::song_text::Credits;

/// The web's dialog: 398 px wide, 24 px side padding, #212121.
const WIDTH: f32 = 398.;
const COVER: f32 = 72.;

impl Music {
    /// Song text opens only as this dialog: "Song credits" with the song's cover,
    /// title, artist and type, then the roles. It reads "Loading…" or the error
    /// until the credits arrive.
    pub(in crate::app) fn credits_dialog(
        &mut self,
        content: Option<&Result<SongText, String>>,
        cx: &Context<Self>,
    ) -> AnyElement {
        let theme = self.theme;
        let muted = gpui::white().opacity(0.7);
        let credits = match content {
            Some(Ok(SongText::Credits(credits))) => Some(credits),
            _ => None,
        };
        let title = credits.map_or("Song credits", |credits| credits.title.as_str());
        let mut body = div()
            .id("overlay-body")
            .w(px(WIDTH))
            .max_h(relative(0.9))
            .overflow_y_scroll()
            .px(px(24.))
            .pb(px(24.))
            .rounded(px(12.))
            .bg(rgb(0x212121))
            .text_size(px(14.))
            .line_height(px(16.8))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .h(px(60.))
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(16.))
                            .line_height(px(22.))
                            .text_color(rgb(0xaaaaaa))
                            .child(title.to_owned()),
                    )
                    .child(
                        button("close-overlay", "close", theme)
                            .mr(px(-12.))
                            .on_click(
                                cx.listener(|view, _, window, cx| view.close_overlay(window, cx)),
                            ),
                    ),
            );
        match content {
            None => body = body.child(div().text_color(muted).child("Loading…")),
            Some(Err(error)) => body = body.child(div().text_color(muted).child(error.clone())),
            Some(Ok(_)) => {}
        }
        if let Some(Credits { song, sections, .. }) = credits {
            if let Some(song) = song {
                let cover = self.art(song.thumbnail.as_deref(), COVER, ArtworkShape::Square);
                body = body.child(
                    div()
                        .mt(px(13.))
                        .mb(px(27.))
                        .flex()
                        .items_center()
                        .gap(px(16.))
                        .child(cover)
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .gap(px(1.5))
                                .child(
                                    div()
                                        .truncate()
                                        .font_weight(FontWeight::MEDIUM)
                                        .child(song.title.clone()),
                                )
                                .child(
                                    div()
                                        .truncate()
                                        .text_color(muted)
                                        .child(song.artist.clone()),
                                )
                                .child(
                                    div()
                                        .truncate()
                                        .text_color(muted)
                                        .child(song.detail.clone()),
                                ),
                        ),
                );
            }
            for (index, section) in sections.iter().enumerate() {
                body = body.child(
                    div()
                        .when(index > 0, |section| section.mt(px(27.)))
                        .child(
                            div()
                                .font_weight(FontWeight::MEDIUM)
                                .child(section.title.clone()),
                        )
                        .child(
                            div()
                                .mt(px(4.))
                                .line_height(px(20.))
                                .text_color(muted)
                                .child(section.names.clone()),
                        ),
                );
            }
        }
        self.backdrop(body.into_any_element(), cx)
    }
}
