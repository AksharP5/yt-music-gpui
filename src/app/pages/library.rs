//! The library's filter chips, sort menu and empty state, shaped like the web's.

use super::super::*;
use super::search::search_chip;

impl Music {
    pub(in crate::app) fn library_filters(
        &mut self,
        mut content: Div,
        inset: f32,
        cx: &mut Context<Self>,
    ) -> Div {
        let Some(filters) = self.page.library.clone() else {
            return content;
        };
        let chips = div()
            .id("library-chips")
            .flex_1()
            .min_w_0()
            .flex()
            .gap(px(12.))
            .overflow_x_scroll()
            .restrict_scroll_to_axis()
            .when_some(filters.clear, |chips, landing| {
                chips.child(
                    search_chip("library-clear", None, false, self.theme).on_click(cx.listener(
                        move |view, _, _, cx| {
                            view.navigate(Route::library_chip(landing.clone()), cx)
                        },
                    )),
                )
            })
            .children(
                filters
                    .chips
                    .into_iter()
                    .enumerate()
                    .map(|(index, choice)| {
                        let target = choice.target;
                        search_chip(
                            ("library-chip", index),
                            Some(choice.label.into()),
                            choice.selected,
                            self.theme,
                        )
                        .on_click(cx.listener(move |view, _, _, cx| {
                            view.navigate(Route::library_chip(target.clone()), cx)
                        }))
                    }),
            );
        let sort = filters
            .sort
            .iter()
            .find(|choice| choice.selected)
            .map(|current| self.sort_button(current.label.clone(), &filters.sort, cx));
        // Measured on the web: chips sit 32px under the header and the grid
        // starts 42px below them.
        content = content.child(
            div()
                .mx(px(inset))
                .mt(px(29.))
                .mb(px(39.))
                .h(px(38.))
                .flex_shrink_0()
                .flex()
                .items_center()
                .gap(px(24.))
                .child(chips)
                .children(sort),
        );
        content
    }

    fn sort_button(
        &self,
        label: String,
        choices: &[crate::catalog::SortChoice],
        cx: &mut Context<Self>,
    ) -> Div {
        let theme = self.theme;
        let button = div()
            .id("library-sort")
            .keyboard_control(theme)
            .h(px(38.))
            .pl(px(16.))
            .pr(px(12.))
            .flex_shrink_0()
            .rounded_full()
            .border_1()
            .border_color(gpui::white().opacity(0.1))
            .bg(gpui::white().opacity(0.1))
            .hover(|button| button.bg(gpui::white().opacity(0.2)))
            .flex()
            .items_center()
            .gap(px(8.))
            .text_size(px(14.))
            .font_weight(FontWeight::MEDIUM)
            .cursor_pointer()
            .child(label)
            .child(icon("expand-more", 24., theme.text))
            .on_click(cx.listener(|view, _, _, cx| {
                view.sort_menu = !view.sort_menu;
                cx.notify();
            }));
        let mut anchor = div().relative().flex_shrink_0().child(button);
        if self.sort_menu {
            let options = choices.iter().enumerate().map(|(index, choice)| {
                let reload = choice.reload.clone();
                let selected = choice.selected;
                div()
                    .id(("library-sort-option", index))
                    .keyboard_control(theme)
                    .h(px(48.))
                    .px(px(16.))
                    .flex()
                    .items_center()
                    .gap(px(14.))
                    .text_size(px(14.))
                    .cursor_pointer()
                    .hover(|row| row.bg(gpui::white().opacity(0.1)))
                    .child(
                        div()
                            .size(px(24.))
                            .flex_shrink_0()
                            .when(selected, |mark| mark.child(icon("check", 24., theme.text))),
                    )
                    .child(choice.label.clone())
                    .on_click(cx.listener(move |view, _, _, cx| {
                        view.sort_menu = false;
                        if !selected {
                            view.request(Query::Reload(reload.clone()), cx);
                        }
                        cx.notify();
                    }))
            });
            let menu = div()
                .id("library-sort-menu")
                .absolute()
                .top(px(46.))
                .right_0()
                .w(px(307.))
                .rounded(px(2.))
                .bg(rgb(0x212121))
                .border_1()
                .border_color(gpui::white().opacity(0.1))
                .shadow_lg()
                .occlude()
                .child(
                    div()
                        .h(px(64.))
                        .px(px(28.))
                        .flex()
                        .items_center()
                        .text_size(px(14.))
                        .border_b_1()
                        .border_color(gpui::white().opacity(0.1))
                        .child("Sort by"),
                )
                .child(div().py(px(8.)).children(options))
                .on_mouse_down_out(cx.listener(|view, _, _, cx| {
                    view.sort_menu = false;
                    cx.notify();
                }));
            anchor = anchor.child(deferred(menu).with_priority(1));
        }
        anchor
    }

    /// The web's centered empty library message.
    pub(in crate::app) fn library_empty(&self) -> Option<AnyElement> {
        let (title, detail) = self.page.library.as_ref()?.empty.clone()?;
        Some(
            div()
                .flex_1()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(8.))
                .pb(px(96.))
                .child(icon("music", 100., self.theme.muted))
                .child(
                    div()
                        .mt(px(32.))
                        .text_size(px(20.))
                        .font_weight(FontWeight::MEDIUM)
                        .child(title),
                )
                .child(
                    div()
                        .text_size(px(14.))
                        .text_color(self.theme.muted)
                        .child(detail),
                )
                .into_any_element(),
        )
    }
}
