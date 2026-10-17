use gpui::{Hsla, rgb};
use std::{fs, path::PathBuf};

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
}

impl Theme {
    pub fn load() -> Self {
        let colors = std::env::var_os("HOME")
            .map(PathBuf::from)
            .and_then(|home| {
                fs::read_to_string(home.join(".local/state/omarchy/current/theme/colors.toml")).ok()
            })
            .and_then(|s| toml::from_str::<toml::Table>(&s).ok());
        let color = |name: &str, fallback| {
            colors
                .as_ref()
                .and_then(|t| t.get(name))
                .and_then(toml::Value::as_str)
                .and_then(|s| u32::from_str_radix(s.trim_start_matches('#'), 16).ok())
                .unwrap_or(fallback)
        };
        let bg = color("background", 0x101014);
        let fg = color("foreground", 0xe8e8ed);
        let accent = color("accent", 0xff5367);
        Self {
            bg: rgb(bg).into(),
            panel: rgb(mix(bg, fg, 0.025)).into(),
            raised: rgb(mix(bg, fg, 0.07)).into(),
            text: rgb(mix(fg, 0xffffff, 0.25)).into(),
            muted: rgb(mix(bg, fg, 0.65)).into(),
            border: rgb(mix(bg, fg, 0.12)).into(),
            accent: rgb(mix(accent, fg, 0.18)).into(),
            selection: rgb(mix(bg, accent, 0.18)).into(),
        }
    }
}

fn mix(a: u32, b: u32, t: f32) -> u32 {
    [16, 8, 0].iter().fold(0, |v, s| {
        v | (((((a >> s) & 255) as f32 * (1. - t) + ((b >> s) & 255) as f32 * t) as u32) << s)
    })
}
