//! The `?` sheet, laid out as music.youtube.com's: Playback and Navigation in
//! the left column, General in the right.

use super::*;
use crate::app::interaction::SHORTCUTS;

const COLUMN: f32 = 422.;
const GAP: f32 = 32.;

impl Music {
    pub(in crate::app) fn shortcuts_dialog(&self, cx: &Context<Self>) -> AnyElement {
        let section = |(title, rows): &(&str, &[(&str, &str)])| {
            div()
                .child(
                    div()
                        .mb(px(16.))
                        .text_size(px(20.))
                        .line_height(px(28.))
                        .font_weight(FontWeight::BOLD)
                        .child(title.to_string()),
                )
                .children(rows.iter().map(|(action, keys)| {
                    div()
                        .py(px(8.))
                        .border_t_1()
                        .border_color(rgb(0x4e4e4e))
                        .flex()
                        .items_center()
                        .text_size(px(12.))
                        .child(
                            div()
                                .w(px(127.))
                                .mr(px(16.))
                                .flex_shrink_0()
                                .line_height(px(14.4))
                                .text_color(rgb(0xaaaaaa))
                                .child(action.to_string()),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .line_height(px(16.))
                                .font_weight(FontWeight::BOLD)
                                .child(keys.to_string()),
                        )
                }))
        };
        let (left, right) = SHORTCUTS.split_at(SHORTCUTS.len() - 1);
        let column = |sections: &[(&str, &[(&str, &str)])]| {
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(27.))
                .children(sections.iter().map(section))
        };
        let body = div()
            .id("overlay-body")
            .w(px(2. * COLUMN + GAP + 48.))
            .max_w(relative(0.95))
            .max_h(relative(0.9))
            .overflow_y_scroll()
            .p(px(24.))
            .rounded(px(3.))
            .bg(rgb(0x333333))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .mb(px(23.))
                    .text_size(px(24.))
                    .line_height(px(28.8))
                    .font_weight(FontWeight::BOLD)
                    .child("Keyboard shortcuts"),
            )
            .child(
                div()
                    .flex()
                    .items_start()
                    .gap(px(GAP))
                    .child(column(left))
                    .child(column(right)),
            );
        self.backdrop(body.into_any_element(), cx)
    }
}
