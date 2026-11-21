//! The Share dialog: a link to copy and, for songs and videos, a Start at time.

use super::super::*;

#[derive(Clone, Debug, PartialEq)]
pub(in crate::app) struct Share {
    url: String,
    /// Whole seconds that Start at adds to a song or video link.
    start: Option<u64>,
    start_at: bool,
}

impl Share {
    fn link(&self) -> String {
        match self.start.filter(|_| self.start_at) {
            Some(seconds) => format!("{}&t={seconds}", self.url),
            None => self.url.clone(),
        }
    }
}

impl Music {
    /// Start at offers the current position when the song is the one playing, as on the web.
    pub(in crate::app) fn share_track(
        &mut self,
        track: &Track,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let playing = self
            .library
            .queue
            .current()
            .is_some_and(|current| current.id == track.id);
        let start = if playing {
            self.playback_position().max(0.) as u64
        } else {
            0
        };
        let share = Share {
            url: format!("https://music.youtube.com/watch?v={}", track.id),
            start: Some(start),
            start_at: false,
        };
        self.open_overlay(Overlay::Share(share), window, cx);
    }

    pub(in crate::app) fn share_collection(
        &mut self,
        collection: &Collection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let share = Share {
            url: format!("https://music.youtube.com/browse/{}", collection.id),
            start: None,
            start_at: false,
        };
        self.open_overlay(Overlay::Share(share), window, cx);
    }

    /// music.youtube.com's Share dialog, without its row of other apps.
    pub(in crate::app) fn share_dialog(&self, share: &Share, cx: &Context<Self>) -> AnyElement {
        let theme = self.theme;
        let divider = gpui::white().opacity(0.2);
        let secondary = rgb(0xaaaaaa);
        let link = div()
            .mt(px(37.))
            .h(px(58.))
            .pl(px(16.))
            .pr(px(8.))
            .flex()
            .items_center()
            .gap(px(8.))
            .bg(rgb(0x0f0f0f))
            .border_1()
            .border_color(divider)
            .rounded(px(12.))
            .child(div().flex_1().min_w_0().truncate().child(share.link()))
            .child(
                div()
                    .id("share-copy")
                    .keyboard_control(theme)
                    .h(px(40.))
                    .px(px(15.))
                    .flex()
                    .items_center()
                    .rounded_full()
                    .border_1()
                    .border_color(divider)
                    .font_weight(FontWeight::MEDIUM)
                    .cursor_pointer()
                    .hover(|button| button.bg(gpui::white().opacity(0.1)))
                    .child("Copy")
                    .on_click(cx.listener(|view, _, _, cx| {
                        let Some(Overlay::Share(share)) = &view.overlay else {
                            return;
                        };
                        cx.write_to_clipboard(ClipboardItem::new_string(share.link()));
                        view.show_toast("Link copied to clipboard", cx);
                    })),
            );
        let start_at = share.start.map(|seconds| {
            let checkbox = div()
                .size(px(20.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(2.))
                .map(|checkbox| {
                    if share.start_at {
                        checkbox
                            .bg(rgb(0x3ea6ff))
                            .child(icon("check", 18., rgb(0x0f0f0f).into()))
                    } else {
                        checkbox.border_2().border_color(rgb(0xf1f1f1))
                    }
                });
            div()
                .mt(px(24.))
                .pt(px(24.))
                .border_t_1()
                .border_color(divider)
                .flex()
                .items_center()
                .text_color(secondary)
                .child(
                    div()
                        .id("share-start-at")
                        .keyboard_control(theme)
                        .flex()
                        .items_center()
                        .gap(px(16.))
                        .cursor_pointer()
                        .child(checkbox)
                        .child("Start at")
                        .on_click(cx.listener(|view, _, _, cx| {
                            if let Some(Overlay::Share(share)) = &mut view.overlay {
                                share.start_at = !share.start_at;
                                cx.notify();
                            }
                        })),
                )
                .child(
                    div()
                        .ml(px(8.))
                        .w(px(50.))
                        .border_b_1()
                        .border_color(secondary)
                        .child(clock(seconds as f64)),
                )
        });
        let body = div()
            .id("overlay-body")
            .w(px(518.))
            .px(px(24.))
            .pt(px(19.))
            .pb(px(36.))
            .rounded(px(12.))
            .bg(rgb(0x212121))
            .text_color(rgb(0xf1f1f1))
            .text_size(px(14.))
            .line_height(px(20.))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .h(px(40.))
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(div().text_size(px(16.)).line_height(px(22.)).child("Share"))
                    .child(button("close-share", "close", theme).mr(px(-3.)).on_click(
                        cx.listener(|view, _, window, cx| view.close_overlay(window, cx)),
                    )),
            )
            .child(link)
            .children(start_at);
        self.backdrop(body.into_any_element(), cx)
    }
}

#[cfg(test)]
mod tests {
    use super::Share;

    #[test]
    fn start_at_adds_the_time_to_the_copied_link() {
        let mut share = Share {
            url: "https://music.youtube.com/watch?v=kTJczUoc26U".into(),
            start: Some(46),
            start_at: false,
        };
        assert_eq!(
            share.link(),
            "https://music.youtube.com/watch?v=kTJczUoc26U"
        );
        share.start_at = true;
        assert_eq!(
            share.link(),
            "https://music.youtube.com/watch?v=kTJczUoc26U&t=46"
        );
    }
}
