use crate::{
    catalog::{BrowseChoice, BrowseLink},
    input::{InputEvent, SearchInput},
    theme::Theme,
};
use gpui::{prelude::*, *};

pub enum CountryPickerEvent {
    Choose(BrowseLink),
    Cancel,
}

pub struct CountryPicker {
    input: Entity<SearchInput>,
    choices: Vec<BrowseChoice>,
    filtered: Vec<usize>,
    highlighted: Option<usize>,
    scroll: UniformListScrollHandle,
    theme: Theme,
    _subscription: Subscription,
}

impl EventEmitter<CountryPickerEvent> for CountryPicker {}

impl CountryPicker {
    pub fn new(choices: Vec<BrowseChoice>, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| {
            let mut input = SearchInput::new(cx);
            input.set_placeholder("Search countries", cx);
            input
        });
        let subscription = cx.subscribe(&input, |picker, _, event, cx| match event {
            InputEvent::Changed => {
                let query = picker.input.read(cx).value().trim().to_lowercase();
                picker.filtered = picker
                    .choices
                    .iter()
                    .enumerate()
                    .filter(|(_, choice)| choice.label.to_lowercase().contains(&query))
                    .map(|(index, _)| index)
                    .collect();
                picker.highlighted = (!picker.filtered.is_empty()).then_some(0);
                picker.scroll.scroll_to_item(0, ScrollStrategy::Top);
                cx.notify();
            }
            InputEvent::Submit => {
                if let Some(index) = picker.highlighted {
                    picker.choose(index, cx);
                }
            }
            InputEvent::Blur => cx.emit(CountryPickerEvent::Cancel),
        });
        let highlighted = choices.iter().position(|choice| choice.selected);
        let scroll = UniformListScrollHandle::new();
        if let Some(index) = highlighted {
            scroll.scroll_to_item(index, ScrollStrategy::Center);
        }
        Self {
            input,
            filtered: (0..choices.len()).collect(),
            choices,
            highlighted,
            scroll,
            theme: Theme::load(),
            _subscription: subscription,
        }
    }

    fn choose(&self, index: usize, cx: &mut Context<Self>) {
        if let Some(choice) = self.filtered.get(index).and_then(|i| self.choices.get(*i)) {
            cx.emit(CountryPickerEvent::Choose(choice.target.clone()));
        }
    }

    fn key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        if event.keystroke.modifiers.control && event.keystroke.key == "q" {
            return;
        }
        if !self.filtered.is_empty() {
            let next = match event.keystroke.key.as_str() {
                "down" => Some(
                    self.highlighted
                        .map_or(0, |index| (index + 1).min(self.filtered.len() - 1)),
                ),
                "up" => Some(self.highlighted.unwrap_or(0).saturating_sub(1)),
                _ => None,
            };
            if let Some(index) = next {
                self.highlighted = Some(index);
                self.scroll.scroll_to_item(index, ScrollStrategy::Nearest);
                cx.notify();
                cx.stop_propagation();
            }
        }
    }
}

impl Focusable for CountryPicker {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.input.focus_handle(cx)
    }
}

impl Render for CountryPicker {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        div()
            .id("country-picker")
            .w(px(480.))
            .max_w_full()
            .max_h(relative(0.9))
            .p(px(28.))
            .rounded(px(12.))
            .border_1()
            .border_color(theme.border)
            .bg(theme.panel)
            .text_color(theme.text)
            .text_size(px(14.))
            .flex()
            .flex_col()
            .gap(px(20.))
            .key_context("CountryPicker")
            .on_key_down(cx.listener(Self::key_down))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(|_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(23.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Chart country"),
                    )
                    .child(
                        div()
                            .id("close-country-picker")
                            .p(px(6.))
                            .cursor_pointer()
                            .child(svg().path("close").size(px(20.)).text_color(theme.muted))
                            .on_click(
                                cx.listener(|_, _, _, cx| cx.emit(CountryPickerEvent::Cancel)),
                            ),
                    ),
            )
            .child(
                div()
                    .px(px(12.))
                    .py(px(10.))
                    .bg(theme.bg)
                    .border_1()
                    .border_color(theme.border)
                    .rounded(px(6.))
                    .child(self.input.clone()),
            )
            .when(self.filtered.is_empty(), |body| {
                body.child(
                    div()
                        .py(px(20.))
                        .text_color(theme.muted)
                        .child("No countries found"),
                )
            })
            .when(!self.filtered.is_empty(), |body| {
                body.child(
                    uniform_list(
                        "country-choices",
                        self.filtered.len(),
                        cx.processor(|picker, range: std::ops::Range<usize>, _, cx| {
                            range
                                .map(|index| {
                                    let choice = &picker.choices[picker.filtered[index]];
                                    div()
                                        .id(("country", index))
                                        .w_full()
                                        .h(px(44.))
                                        .px(px(12.))
                                        .flex()
                                        .items_center()
                                        .justify_between()
                                        .when(picker.highlighted == Some(index), |row| {
                                            row.bg(picker.theme.selection)
                                        })
                                        .hover(|row| row.bg(picker.theme.raised))
                                        .cursor_pointer()
                                        .child(choice.label.clone())
                                        .when(choice.selected, |row| {
                                            row.child(
                                                div()
                                                    .text_size(px(12.))
                                                    .text_color(picker.theme.muted)
                                                    .child("Current"),
                                            )
                                        })
                                        .on_click(cx.listener(move |picker, _, _, cx| {
                                            picker.choose(index, cx)
                                        }))
                                })
                                .collect::<Vec<_>>()
                        }),
                    )
                    .w_full()
                    .h(px((self.filtered.len() as f32 * 44.).min(352.)))
                    .min_h_0()
                    .track_scroll(&self.scroll),
                )
            })
    }
}
