//! Shared artwork, icons, buttons, sliders, and time formatting.

use super::*;

pub(in crate::app) trait KeyboardControl:
    StatefulInteractiveElement + Styled + Sized
{
    fn keyboard_control(self, theme: Theme) -> Self {
        self.tab_index(0)
            .border_1()
            .border_color(gpui::transparent_black())
            .focus_visible(move |style| style.border_color(theme.text))
    }
}

impl<T: StatefulInteractiveElement + Styled> KeyboardControl for T {}

#[derive(Clone, Copy)]
pub(in crate::app) enum ArtworkShape {
    Square,
    Circle,
}

impl Music {
    pub(in crate::app) fn art(
        &mut self,
        url: Option<&str>,
        edge: f32,
        shape: ArtworkShape,
    ) -> AnyElement {
        let radius = match shape {
            ArtworkShape::Square => 5.,
            ArtworkShape::Circle => edge / 2.,
        };
        let image = url.and_then(|url| self.artwork.get(url));
        let mut element = div()
            .size(px(edge))
            .flex_shrink_0()
            .rounded(px(radius))
            .overflow_hidden()
            .bg(self.theme.raised);
        element = if let Some(image) = image {
            element.child(
                img(image)
                    .size_full()
                    .object_fit(ObjectFit::Cover)
                    .rounded(px(radius)),
            )
        } else {
            element.flex().items_center().justify_center().child(
                icon("music", edge.min(32.) * 0.6, self.theme.muted).text_color(self.theme.muted),
            )
        };
        element.into_any_element()
    }
}

pub(in crate::app) fn icon(name: &'static str, edge: f32, color: Hsla) -> Svg {
    svg().path(name).size(px(edge)).text_color(color)
}
pub(in crate::app) struct Hint {
    pub(in crate::app) text: &'static str,
    pub(in crate::app) theme: Theme,
}
impl Render for Hint {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .px(px(10.))
            .py(px(7.))
            .rounded(px(5.))
            .bg(self.theme.raised)
            .text_color(self.theme.text)
            .text_size(px(12.))
            .child(self.text)
    }
}
pub(in crate::app) fn button(
    id: impl Into<ElementId>,
    name: &'static str,
    theme: Theme,
) -> Stateful<Div> {
    tinted_button(id, name, theme.text, theme)
}
/// An icon button drawn in `color`, such as the web's grey for inactive player controls.
pub(in crate::app) fn tinted_button(
    id: impl Into<ElementId>,
    name: &'static str,
    color: impl Into<Hsla>,
    theme: Theme,
) -> Stateful<Div> {
    let label = match name {
        "menu" => "Toggle sidebar",
        "more" => "More options",
        "thumb-up" => "Like song",
        "thumb-up-filled" => "Remove from liked music",
        "thumb-down" => "Dislike song",
        "thumb-down-filled" => "Remove dislike",
        "forward" => "Next items",
        "play" => "Play",
        "pause" => "Pause",
        "previous" => "Previous song",
        "next" => "Next song",
        "shuffle" => "Shuffle upcoming songs",
        "repeat" => "Cycle repeat: off, all, one",
        "heart" => "Like song",
        "heart-filled" => "Remove from liked music",
        "expand" => "Open player page",
        "collapse" => "Close player page",
        "fullscreen" => "Fullscreen",
        "exit-fullscreen" => "Exit fullscreen",
        "volume" => "Mute",
        "mute" => "Unmute",
        "plus" => "Add",
        "bookmark" => "Save to library",
        "bookmark-filled" => "Remove from library",
        "back" => "Go back",
        "help" => "Keyboard shortcuts",
        "settings" => "Settings",
        "close" => "Close or remove",
        _ => "Open",
    };
    icon_button(id, name, color, theme)
        .tooltip(move |_, cx| cx.new(|_| Hint { text: label, theme }).into())
}
/// A round icon button without a tooltip.
fn icon_button(
    id: impl Into<ElementId>,
    name: &'static str,
    color: impl Into<Hsla>,
    theme: Theme,
) -> Stateful<Div> {
    div()
        .id(id)
        .keyboard_control(theme)
        .relative()
        .size(px(40.))
        .flex_shrink_0()
        .rounded_full()
        .flex()
        .items_center()
        .justify_center()
        .text_color(theme.text)
        .cursor_pointer()
        .hover(move |d| d.bg(theme.raised))
        .child(icon(name, 24., color.into()))
}
impl Music {
    /// A "More options" button whose menu opens against the button, as on the web. Its
    /// tooltip is withheld while a menu is open, since GPUI keeps an occluded tooltip visible.
    pub(in crate::app) fn menu_button(
        &self,
        id: impl Into<ElementId>,
        cx: &Context<Self>,
        open: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> Stateful<Div> {
        let bounds = Rc::new(Cell::new(Bounds::default()));
        let measured = bounds.clone();
        let theme = self.theme;
        let menu_open = matches!(
            self.overlay,
            Some(Overlay::Track { .. } | Overlay::Collection(_))
        );
        icon_button(id, "more", theme.text, theme)
            .when(!menu_open, |button| {
                button.tooltip(move |_, cx| {
                    cx.new(|_| Hint {
                        text: "More options",
                        theme,
                    })
                    .into()
                })
            })
            .child(
                canvas(move |rect, _, _| measured.set(rect), |_, _, _, _| {})
                    .absolute()
                    .inset_0(),
            )
            .on_click(cx.listener(move |view, event: &ClickEvent, window, cx| {
                cx.stop_propagation();
                // A pointer click returns focus to the app when the menu closes; a keyboard
                // activation returns it to this button.
                if !matches!(event, ClickEvent::Keyboard(_)) {
                    window.focus(&view.focus, cx);
                }
                open(view, window, cx);
                if view.overlay.is_some() {
                    view.overlay_button = Some(bounds.get());
                }
            }))
    }
}
pub(in crate::app) fn filter_chip(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    selected: bool,
    theme: Theme,
) -> Stateful<Div> {
    div()
        .id(id)
        .keyboard_control(theme)
        .h(px(36.))
        .px(px(12.))
        .flex_shrink_0()
        .rounded(px(8.))
        .flex()
        .items_center()
        .bg(if selected { theme.text } else { theme.raised })
        .text_color(if selected { theme.bg } else { theme.text })
        .text_size(px(14.))
        .cursor_pointer()
        .hover(move |style| {
            style.bg(if selected {
                theme.text
            } else {
                rgb(0x3f3f3f).into()
            })
        })
        .child(label.into())
}
pub(in crate::app) fn slider(
    ratio: f32,
    bounds: Rc<Cell<Bounds<Pixels>>>,
    theme: Theme,
) -> impl IntoElement {
    canvas(
        move |rect, _, _| {
            bounds.set(rect);
            rect
        },
        move |rect, _, window, _| {
            let y = rect.top() + rect.size.height / 2. - px(2.);
            window.paint_quad(fill(
                Bounds::new(point(rect.left(), y), size(rect.size.width, px(3.))),
                theme.border,
            ));
            window.paint_quad(fill(
                Bounds::new(point(rect.left(), y), size(rect.size.width * ratio, px(3.))),
                theme.accent,
            ));
            window.paint_quad(fill(
                Bounds::new(
                    point(rect.left() + rect.size.width * ratio - px(3.), y - px(1.5)),
                    size(px(6.), px(6.)),
                ),
                theme.text,
            ));
        },
    )
    .size_full()
}
/// The player bar's progress line: 2 px at rest, 4 px with a knob while hovered or dragged.
pub(in crate::app) fn seek_bar(
    ratio: f32,
    bounds: Rc<Cell<Bounds<Pixels>>>,
    dragging: bool,
    theme: Theme,
) -> impl IntoElement {
    div()
        .size_full()
        .group("seek")
        .relative()
        .child(
            canvas(move |rect, _, _| bounds.set(rect), |_, _, _, _| {})
                .absolute()
                .inset_0(),
        )
        .child(
            div()
                .absolute()
                .left_0()
                .right_0()
                .top(px(8.))
                .h(px(2.))
                .bg(gpui::white().opacity(0.1))
                .when(dragging, |track| track.top(px(7.)).h(px(4.)))
                .group_hover("seek", |track| track.top(px(7.)).h(px(4.)))
                .child(div().h_full().w(relative(ratio)).bg(linear_gradient(
                    90.,
                    linear_color_stop(theme.accent, 0.8),
                    linear_color_stop(rgb(0xff2791), 1.),
                ))),
        )
        .child(
            div()
                .absolute()
                .top(px(2.))
                .left(relative(ratio))
                .ml(px(-7.))
                .size(px(14.))
                .rounded_full()
                .bg(theme.accent)
                .invisible()
                .when(dragging, |knob| knob.visible())
                .group_hover("seek", |knob| knob.visible()),
        )
}
pub(in crate::app) fn clock(seconds: f64) -> String {
    let seconds = seconds.max(0.) as u64;
    if seconds >= 3600 {
        format!(
            "{}:{:02}:{:02}",
            seconds / 3600,
            (seconds / 60) % 60,
            seconds % 60
        )
    } else {
        format!("{}:{:02}", seconds / 60, seconds % 60)
    }
}
