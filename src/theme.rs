use gpui::{Hsla, rgb};

#[derive(Clone, Copy, PartialEq)]
pub struct Theme {
    pub bg: Hsla,
    pub panel: Hsla,
    pub raised: Hsla,
    pub text: Hsla,
    pub muted: Hsla,
    pub border: Hsla,
    pub accent: Hsla,
    pub selection: Hsla,
    pub text_selection: Hsla,
}

impl Theme {
    pub fn load() -> Self {
        Self {
            bg: rgb(0x030303).into(),
            panel: rgb(0x212121).into(),
            raised: rgb(0x292929).into(),
            text: rgb(0xffffff).into(),
            muted: rgb(0xb3b3b3).into(),
            border: rgb(0x292929).into(),
            accent: rgb(0xff0033).into(),
            selection: rgb(0x1d1d1d).into(),
            text_selection: rgb(0x264f78).into(),
        }
    }
}
