use crate::{
    catalog::{BrowseChoice, BrowseLink},
    theme::Theme,
};
use gpui::{prelude::*, *};

pub enum CountryPickerEvent {
    Choose(BrowseLink),
}

/// The web's chart country dropdown, measured from music.youtube.com: 307px
/// wide, a "Select a country" heading, then 48px rows with a check on the
/// current country and a divider after the pinned ones. Up, Down and Enter
/// choose from the keyboard; Escape closes it through the app's overlays.
pub struct CountryPicker {
    choices: Vec<BrowseChoice>,
    /// How many countries sit above the divider.
    divider: Option<usize>,
    highlighted: usize,
    focus: FocusHandle,
    scroll: ScrollHandle,
    theme: Theme,
}

pub const PICKER_WIDTH: f32 = 307.;
const ROW: f32 = 48.;
/// The web's list, padding included, before it scrolls.
const LIST_HEIGHT: f32 = 438.;

impl EventEmitter<CountryPickerEvent> for CountryPicker {}

impl CountryPicker {
    pub fn new(choices: Vec<BrowseChoice>, divider: Option<usize>, cx: &mut Context<Self>) -> Self {
        let highlighted = choices
            .iter()
            .position(|choice| choice.selected)
            .unwrap_or(0);
        let scroll = ScrollHandle::new();
        scroll.scroll_to_item(highlighted);
        Self {
            choices,
            divider,
            highlighted,
            focus: cx.focus_handle(),
            scroll,
            theme: Theme::load(),
        }
    }

    fn choose(&self, index: usize, cx: &mut Context<Self>) {
        if let Some(choice) = self.choices.get(index) {
            cx.emit(CountryPickerEvent::Choose(choice.target.clone()));
        }
    }

    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if event.keystroke.modifiers.modified() || self.choices.is_empty() {
            return;
        }
        let last = self.choices.len() - 1;
        self.highlighted = match event.keystroke.key.as_str() {
            "down" => (self.highlighted + 1).min(last),
            "up" => self.highlighted.saturating_sub(1),
            "home" => 0,
            "end" => last,
            "enter" => {
                self.choose(self.highlighted, cx);
                cx.stop_propagation();
                return;
            }
            _ => return,
        };
        self.scroll.scroll_to_item(self.highlighted);
        window.focus(&self.focus, cx);
        cx.stop_propagation();
        cx.notify();
    }
}

impl Focusable for CountryPicker {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for CountryPicker {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let line = gpui::white().opacity(0.1);
        let rows = self.choices.iter().enumerate().map(|(index, choice)| {
            let row = div()
                .id(("country", index))
                .tab_index(0)
                .h(px(ROW))
                .pl(px(6.))
                .flex()
                .items_center()
                .gap(px(8.))
                .border_1()
                .border_color(gpui::transparent_black())
                .focus_visible(|style| style.border_color(theme.text))
                .when(self.highlighted == index, |row| row.bg(line))
                .hover(|row| row.bg(line))
                .cursor_pointer()
                .child(
                    div()
                        .size(px(40.))
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .when(choice.selected, |column| {
                            column.child(svg().path("check").size(px(24.)).text_color(theme.text))
                        }),
                )
                .child(choice.label.clone())
                .on_key_down(
                    cx.listener(move |picker, event: &KeyDownEvent, window, cx| {
                        if matches!(
                            event.keystroke.key.as_str(),
                            "up" | "down" | "home" | "end" | "enter"
                        ) {
                            picker.highlighted = index;
                            picker.key_down(event, window, cx);
                        }
                    }),
                )
                .on_click(cx.listener(move |picker, _, _, cx| picker.choose(index, cx)));
            div()
                .child(row)
                .when(self.divider == Some(index + 1), |slot| {
                    slot.child(div().mx(px(16.)).w(px(260.)).h(px(1.)).bg(line))
                })
        });
        div()
            .id("country-picker")
            .track_focus(&self.focus)
            .key_context("CountryPicker")
            .on_key_down(cx.listener(Self::key_down))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .w(px(PICKER_WIDTH))
            .rounded(px(2.))
            .border_1()
            .border_color(line)
            .bg(rgb(0x212121))
            .text_color(theme.text)
            .text_size(px(14.))
            .shadow_lg()
            .child(
                div()
                    .h(px(64.))
                    .pl(px(28.))
                    .flex()
                    .items_center()
                    .border_b_1()
                    .border_color(line)
                    .child("Select a country"),
            )
            .child(
                div()
                    .id("country-choices")
                    .max_h(px(LIST_HEIGHT))
                    .py(px(8.))
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .children(rows),
            )
    }
}
