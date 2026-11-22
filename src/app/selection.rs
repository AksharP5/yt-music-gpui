//! Selected album and playlist songs and the web's toolbar that acts on them.

use super::*;

/// The toolbar's size and colors, measured from music.youtube.com.
const BAR_WIDTH: f32 = 398.;
const BAR_EDGE: u32 = 0x333333;
const BAR_FILL: u32 = 0x212121;
const CHECKBOX: u32 = 0xaaaaaa;

fn same_song(a: &Track, b: &Track) -> bool {
    a.id == b.id && a.details.set_video_id == b.details.set_video_id
}

impl Music {
    pub(in crate::app) fn is_selected(&self, track: &Track) -> bool {
        self.selected_songs
            .iter()
            .any(|selected| same_song(selected, track))
    }

    pub(in crate::app) fn toggle_selected(&mut self, track: &Track, cx: &mut Context<Self>) {
        let before = self.selected_songs.len();
        self.selected_songs
            .retain(|selected| !same_song(selected, track));
        if self.selected_songs.len() == before {
            self.selected_songs.push(track.clone());
        }
        cx.notify();
    }

    pub(in crate::app) fn clear_selection(&mut self, cx: &mut Context<Self>) {
        if !self.selected_songs.is_empty() {
            self.selected_songs.clear();
            cx.notify();
        }
    }

    /// A 24px checkbox like the web's: an outlined square, or a filled one
    /// with a check when selected.
    pub(in crate::app) fn checkbox(&self, checked: bool) -> Div {
        let color = rgb(CHECKBOX);
        div()
            .size(px(18.))
            .rounded(px(2.))
            .flex()
            .items_center()
            .justify_center()
            .when(checked, |checkbox| {
                checkbox.bg(color).child(icon("check", 16., self.theme.bg))
            })
            .when(!checked, |checkbox| checkbox.border_2().border_color(color))
    }

    /// Takes the selection for an action, which ends selecting like the web.
    fn take_selection(&mut self) -> Vec<Track> {
        std::mem::take(&mut self.selected_songs)
    }

    fn songs(count: usize) -> String {
        if count == 1 {
            "1 song".into()
        } else {
            format!("{count} songs")
        }
    }

    fn save_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.account.is_none() {
            self.connect_account(window, cx);
            return;
        }
        let tracks = self.take_selection();
        self.playlist_picker(tracks, window, cx);
    }

    fn play_selection_next(&mut self, cx: &mut Context<Self>) {
        let tracks = self.take_selection();
        let toast = format!("{} will play next", Self::songs(tracks.len()));
        self.play_next(tracks, cx);
        self.show_toast(toast, cx);
    }

    fn queue_selection(&mut self, cx: &mut Context<Self>) {
        let tracks = self.take_selection();
        let toast = format!("{} added to queue", Self::songs(tracks.len()));
        self.library.queue.tracks_mut().extend(tracks);
        self.queue_changed(cx);
        self.show_toast(toast, cx);
    }

    /// The toolbar's More actions menu.
    pub(in crate::app) fn selection_actions(&self, cx: &Context<Self>) -> [AnyElement; 3] {
        [
            self.menu_action(
                "menu-playlist",
                "Save to playlist",
                cx,
                |view, window, cx| view.save_selection(window, cx),
            ),
            self.menu_action("menu-next", "Play next", cx, |view, _, cx| {
                view.play_selection_next(cx)
            }),
            self.menu_action("menu-enqueue", "Add to queue", cx, |view, _, cx| {
                view.queue_selection(cx)
            }),
        ]
    }

    /// The web's toolbar, centered 32px above the bottom of the page:
    /// dismiss, the count, Save to playlist, Play next and More actions.
    pub(in crate::app) fn selection_bar(
        &self,
        bottom: f32,
        width: f32,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if self.selected_songs.is_empty() {
            return None;
        }
        let theme = self.theme;
        let action = |id: &'static str, glyph: &'static str| {
            tinted_button(id, glyph, theme.text, theme).size(px(40.))
        };
        Some(
            div()
                .id("selection-bar")
                .absolute()
                .bottom(px(bottom + 32.))
                .left(px(((width - BAR_WIDTH) / 2.).max(0.)))
                .w(px(BAR_WIDTH))
                .p(px(1.))
                .rounded(px(3.))
                .bg(rgb(BAR_EDGE))
                .shadow(vec![
                    BoxShadow::new(px(0.), px(16.), gpui::black().opacity(0.14))
                        .blur_radius(px(24.))
                        .spread_radius(px(2.)),
                ])
                .occlude()
                .child(
                    div()
                        .h(px(70.))
                        .px(px(20.))
                        .rounded(px(2.))
                        .bg(rgb(BAR_FILL))
                        .flex()
                        .items_center()
                        .child(
                            tinted_button("selection-dismiss", "close", rgb(CHECKBOX), theme)
                                .size(px(40.))
                                .on_click(cx.listener(|view, _, _, cx| view.clear_selection(cx))),
                        )
                        .child(
                            div()
                                .ml(px(9.))
                                .flex_1()
                                .text_size(px(16.))
                                .line_height(px(22.))
                                .child(format!("{} selected", self.selected_songs.len())),
                        )
                        .child(action("selection-save", "playlist-add").on_click(
                            cx.listener(|view, _, window, cx| view.save_selection(window, cx)),
                        ))
                        .child(
                            action("selection-next", "menu-play-next")
                                .ml(px(8.))
                                .on_click(
                                    cx.listener(|view, _, _, cx| view.play_selection_next(cx)),
                                ),
                        )
                        .child(
                            self.menu_button("selection-more", cx, |view, window, cx| {
                                view.open_overlay(Overlay::Selection, window, cx);
                            })
                            .size(px(36.))
                            .ml(px(28.)),
                        ),
                )
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::same_song;
    use crate::catalog::Track;

    #[test]
    fn a_playlists_repeated_song_is_selected_once_per_occurrence() {
        let occurrence = |set: &str| {
            let mut track = Track {
                id: "abcdefghijk".into(),
                title: "Dreams".into(),
                artist: "Fleetwood Mac".into(),
                album: None,
                duration: None,
                thumbnail: None,
                details: Default::default(),
            };
            track.details.set_video_id = Some(set.into());
            track
        };
        assert!(same_song(&occurrence("first"), &occurrence("first")));
        assert!(!same_song(&occurrence("first"), &occurrence("second")));
    }
}
