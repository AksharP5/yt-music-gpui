//! Keyboard shortcuts and seek/volume drag interactions.

use super::*;

/// The `?` sheet's sections, worded as music.youtube.com's, with the app's
/// extra keys added to their rows. The README lists the same tables.
pub(in crate::app) const SHORTCUTS: &[(&str, &[(&str, &str)])] = &[
    (
        "Playback",
        &[
            ("Play/Pause", "SPACE or ;"),
            ("Next song", "j or SHIFT + n or CTRL + right arrow"),
            ("Previous song", "k or SHIFT + p or CTRL + left arrow"),
            ("Forward 10s", "l or SHIFT + right arrow"),
            ("Back 10s", "h or SHIFT + left arrow"),
            ("Forward or back 5s", "right or left arrow"),
            ("Forward 1s", "SHIFT + l or CTRL + SHIFT + right arrow"),
            ("Back 1s", "SHIFT + h or CTRL + SHIFT + left arrow"),
            ("Shuffle queue", "s"),
            ("Toggle repeat", "r"),
        ],
    ),
    (
        "Navigation",
        &[
            ("Go to Home", "gh"),
            ("Go to Explore", "ge"),
            ("Go to Library", "gl"),
            ("Go to Settings", "g,"),
            ("Search", "/ or CTRL + k"),
            ("Back", "ALT + left arrow or mouse back"),
            ("Forward", "ALT + right arrow or mouse forward"),
        ],
    ),
    (
        "General",
        &[
            ("Increase volume", "= or up arrow"),
            ("Decrease volume", "- or down arrow"),
            ("Toggle mute", "m"),
            ("Toggle queue/expanded player", "q or ESC"),
            ("Toggle full screen", "f"),
            ("Like current song", "+"),
            ("Dislike current song", "_"),
            ("Keyboard shortcuts", "?"),
            ("Reload", "CTRL + r"),
            ("Quit", "CTRL + q"),
        ],
    ),
];

/// Where the web's two-key shortcuts go: G, then H, E, L or comma.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::app) enum GoTo {
    Home,
    Explore,
    Library,
    Settings,
}

/// The pending G of a two-key shortcut. As on the web, the second key must
/// follow within about 1.5 s; any other key, or a late one, acts on its own,
/// so a bare H or L still seeks.
#[derive(Default)]
pub(in crate::app) struct GoChord(Option<Instant>);

impl GoChord {
    const TIMEOUT: Duration = Duration::from_millis(1500);

    /// `None` lets the key act as usual. `Some(None)` means G started a chord.
    fn press(&mut self, key: &str, now: Instant) -> Option<Option<GoTo>> {
        let pending = self
            .0
            .take()
            .is_some_and(|pressed| now.saturating_duration_since(pressed) < Self::TIMEOUT);
        let target = match key {
            "h" => Some(GoTo::Home),
            "e" => Some(GoTo::Explore),
            "l" => Some(GoTo::Library),
            "," => Some(GoTo::Settings),
            _ => None,
        };
        if pending && target.is_some() {
            return Some(target);
        }
        if key == "g" {
            self.0 = Some(now);
            return Some(None);
        }
        None
    }
}

#[derive(Clone, Copy, PartialEq)]
pub(in crate::app) enum Drag {
    Seek {
        track_id: Option<u64>,
        generation: u64,
        seconds: f64,
    },
    Volume,
    /// Reordering Up next; the rows track the pointer in `PlayerPage`.
    Queue,
}

impl Music {
    pub(in crate::app) fn cancel_queue_drag(&mut self) -> bool {
        if self.drag != Some(Drag::Queue) {
            return false;
        }
        self.drag = None;
        self.player_page.reorder = None;
        self.player_page.press = None;
        true
    }

    pub(in crate::app) fn start_seek_drag(
        &mut self,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        if self.transport().duration <= 0. {
            return;
        }
        self.drag = Some(Drag::Seek {
            track_id: self.active_id,
            generation: self.playback_generation.load(Ordering::Relaxed),
            seconds: self.playback_position(),
        });
        self.drag_to(position, cx);
    }

    pub(in crate::app) fn seek_drag_position(&self) -> Option<f64> {
        let Some(Drag::Seek {
            track_id,
            generation,
            seconds,
        }) = self.drag
        else {
            return None;
        };
        // Stream reloads change the playback ID. Stopped queue selections
        // change the generation even though they do not have a playback ID.
        (track_id == self.active_id
            && generation == self.playback_generation.load(Ordering::Relaxed))
        .then_some(seconds)
    }

    pub(in crate::app) fn drag_to(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        match self.drag {
            Some(Drag::Seek {
                track_id,
                generation,
                ..
            }) => {
                if self.seek_drag_position().is_none() {
                    self.drag = None;
                    return;
                }
                let bounds = self.seek_bounds.get();
                let fraction = ((position.x - bounds.left()) / bounds.size.width).clamp(0., 1.);
                self.drag = Some(Drag::Seek {
                    track_id,
                    generation,
                    seconds: f64::from(fraction) * self.transport().duration,
                });
                self.player_bar.update(cx, |_, cx| cx.notify());
            }
            Some(Drag::Volume) => {
                let bounds = self.volume_bounds.get();
                self.set_volume(
                    ((position.x - bounds.left()) / bounds.size.width).clamp(0., 1.) as f64 * 100.,
                    cx,
                );
            }
            Some(Drag::Queue) => self.drag_queue_row(position, cx),
            _ => {}
        }
    }

    pub(in crate::app) fn end_drag(&mut self, cx: &mut Context<Self>) {
        let seek_position = self.seek_drag_position();
        match self.drag.take() {
            Some(Drag::Seek { .. }) => {
                if let Some(seconds) = seek_position {
                    self.seek_current(seconds, cx);
                }
            }
            Some(Drag::Volume) => self.save_preferences(),
            Some(Drag::Queue) => self.drop_queue_row(cx),
            None => return,
        }
        cx.notify();
    }

    fn seek_by(&mut self, seconds: f64, cx: &mut Context<Self>) {
        let target = (self.playback_position() + seconds)
            .min(self.transport().duration)
            .max(0.);
        self.seek_current(target, cx);
    }

    pub(in crate::app) fn key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = event.keystroke.key.as_str();
        let modifiers = event.keystroke.modifiers;
        let secondary = modifiers == Modifiers::secondary_key();
        if secondary && key == "q" {
            cx.quit();
            return;
        }
        if let Some(overlay) = &self.overlay
            && key != "escape"
        {
            let menu = self.settings_menu.is_some() || overlay.is_menu();
            if !modifiers.control
                && !modifiers.alt
                && !modifiers.platform
                && (key == "tab" || (menu && matches!(key, "up" | "down" | "home" | "end")))
            {
                if matches!(key, "home" | "end") {
                    window.focus(
                        if self.settings_menu.is_some() {
                            &self.settings_focus
                        } else {
                            &self.overlay_focus
                        },
                        cx,
                    );
                }
                self.move_overlay_focus(modifiers.shift || matches!(key, "up" | "end"), window, cx);
                cx.stop_propagation();
            }
            return;
        }
        if key == "tab" && !modifiers.control && !modifiers.alt && !modifiers.platform {
            self.move_keyboard_focus(modifiers.shift, window, cx);
            cx.stop_propagation();
            return;
        }
        if (secondary && key == "k")
            || (key == "/"
                && !modifiers.modified()
                && !self.search.focus_handle(cx).is_focused(window))
        {
            if self.player_open && window.is_fullscreen() {
                self.toggle_video_fullscreen(window, cx);
            }
            self.open_search(window, cx);
            cx.stop_propagation();
            return;
        }
        if secondary && key == "r" {
            self.reload_current_page(cx);
            return;
        }
        if modifiers.alt
            && !modifiers.control
            && !modifiers.platform
            && !modifiers.function
            && !modifiers.shift
            && matches!(key, "left" | "right")
        {
            let direction = if key == "left" {
                NavigationDirection::Back
            } else {
                NavigationDirection::Forward
            };
            self.navigate_history(direction, cx);
            return;
        }
        if self.search.focus_handle(cx).is_focused(window) {
            return;
        }
        if key == "space" && !self.focus.is_focused(window) {
            return;
        }
        if secondary {
            match key {
                "right" => self.skip_next(cx),
                "left" => self.previous(cx),
                _ => {}
            }
            return;
        }
        let shift = modifiers.shift;
        if modifiers.alt || modifiers.platform || modifiers.function {
            return;
        }
        if modifiers.control {
            match (key, shift) {
                ("right", true) => self.seek_by(1., cx),
                ("left", true) => self.seek_by(-1., cx),
                _ => {}
            }
            return;
        }
        if !shift && let Some(go) = self.go_chord.press(key, Instant::now()) {
            match go {
                Some(GoTo::Home) => self.navigate(Route::Home, cx),
                Some(GoTo::Explore) => self.navigate(Route::Explore, cx),
                Some(GoTo::Library) => self.navigate(Route::Saved, cx),
                Some(GoTo::Settings) => self.open_overlay(Overlay::Settings, window, cx),
                None => {}
            }
            return;
        }
        // Linux reports Shift+= as "+" and Shift+/ as "?" without Shift; other
        // platforms may keep the base key and the modifier.
        match (key, shift) {
            ("space" | ";", false) => self.toggle_play(cx),
            ("j", false) | ("n", true) => self.skip_next(cx),
            ("k", false) | ("p", true) => self.previous(cx),
            ("right", false) => self.seek_by(5., cx),
            ("left", false) => self.seek_by(-5., cx),
            ("right", true) | ("l", false) => self.seek_by(10., cx),
            ("left", true) | ("h", false) => self.seek_by(-10., cx),
            ("l", true) => self.seek_by(1., cx),
            ("h", true) => self.seek_by(-1., cx),
            ("up" | "=", false) => {
                if self.set_volume(self.library.volume + 5., cx) {
                    self.save_preferences();
                }
            }
            ("down" | "-", false) => {
                if self.set_volume(self.library.volume - 5., cx) {
                    self.save_preferences();
                }
            }
            ("m", false) => self.toggle_mute(cx),
            ("r", false) => self.cycle_repeat(cx),
            ("s", false) => self.shuffle_queue(cx),
            ("+", _) | ("=", true) => {
                if let Some(track) = self.library.queue.current().cloned() {
                    self.toggle_like(&track, cx);
                }
            }
            ("_", _) | ("-", true) => {
                if let Some(track) = self.library.queue.current().cloned() {
                    self.toggle_dislike(&track, window, cx);
                }
            }
            ("f", false) => self.toggle_video_fullscreen(window, cx),
            ("q", false) => self.toggle_player_page(cx),
            ("escape", false) => {
                if self.guide_drawer {
                    self.guide_drawer = false;
                    cx.notify();
                    return;
                }
                if self.settings_menu.is_some() {
                    self.close_settings_menu(window, cx);
                    return;
                }
                if self.overlay.is_some() {
                    self.close_overlay(window, cx);
                    return;
                }
                if cx.stop_active_drag(window) || self.cancel_queue_drag() {
                    cx.notify();
                    return;
                }
                if self.player_open && window.is_fullscreen() {
                    self.toggle_video_fullscreen(window, cx);
                    return;
                }
                if self.player_open {
                    self.close_player_page();
                    cx.notify();
                }
            }
            ("?", _) | ("/", true) => self.open_overlay(Overlay::Help, window, cx),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{GoChord, GoTo};
    use std::time::{Duration, Instant};

    #[test]
    fn readme_tables_match_the_shortcut_sheet() {
        for (section, rows) in super::SHORTCUTS {
            let rows: String = rows
                .iter()
                .map(|(action, keys)| format!("| {keys} | {action} |\n"))
                .collect();
            let table = format!("### {section}\n\n| Key | Action |\n| --- | --- |\n{rows}\n");
            assert!(
                include_str!("../../README.md").contains(&table),
                "Update the README shortcut table to:\n{table}"
            );
        }
    }

    #[test]
    fn g_then_a_key_navigates_only_within_the_webs_window() {
        let start = Instant::now();
        let mut chord = GoChord::default();
        assert_eq!(chord.press("h", start), None, "a bare H still seeks");
        for (key, target) in [
            ("h", GoTo::Home),
            ("e", GoTo::Explore),
            ("l", GoTo::Library),
            (",", GoTo::Settings),
        ] {
            assert_eq!(chord.press("g", start), Some(None));
            let soon = start + Duration::from_millis(1400);
            assert_eq!(chord.press(key, soon), Some(Some(target)));
            assert_eq!(chord.press(key, soon), None, "the chord is used up");
        }
        // The web dropped a G held for 1.6 s before L.
        assert_eq!(chord.press("g", start), Some(None));
        assert_eq!(chord.press("l", start + Duration::from_millis(1600)), None);
        // Any other key cancels the chord and acts on its own.
        assert_eq!(chord.press("g", start), Some(None));
        assert_eq!(chord.press("j", start), None);
        assert_eq!(chord.press("h", start), None);
    }
}
