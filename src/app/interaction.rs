//! Keyboard shortcuts and seek/volume drag interactions.

use super::*;

/// The `?` dialog and the README table both list these, in this order.
pub(in crate::app) const SHORTCUTS: &[(&str, &str)] = &[
    ("/ or Ctrl K", "Search"),
    ("Space or ;", "Play or pause"),
    ("J, Shift N or Ctrl →", "Next song"),
    ("K, Shift P or Ctrl ←", "Previous song"),
    ("→ / ←", "Seek 5 seconds"),
    ("L / H or Shift → / Shift ←", "Seek 10 seconds"),
    ("Shift L / Shift H or Ctrl Shift → / ←", "Seek 1 second"),
    ("↑ / ↓ or = / -", "Volume up / down"),
    ("M", "Mute"),
    ("R", "Repeat: off, all, one"),
    ("S", "Shuffle"),
    ("+", "Like the current song"),
    ("_", "Dislike the current song"),
    ("Q", "Open or close the player page"),
    ("F", "Video fullscreen"),
    ("Ctrl R", "Reload the page"),
    ("Alt ← or mouse Back", "Back"),
    ("Alt → or mouse Forward", "Forward"),
    (
        "Escape",
        "Close a dialog, fullscreen, the player page or search",
    ),
    ("?", "Keyboard shortcuts"),
    ("Ctrl Q", "Quit"),
];

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
        if (self.overlay.is_some() || self.sort_menu) && key != "escape" {
            let menu = self.sort_menu
                || self.settings_menu.is_some()
                || matches!(
                    self.overlay,
                    Some(Overlay::Track { .. } | Overlay::Collection(_) | Overlay::Account)
                );
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
            window.focus(&self.search.focus_handle(cx), cx);
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
                if self.sort_menu {
                    self.dismiss_sort_menu(window, cx);
                    return;
                }
                if self.settings_menu.take().is_some() {
                    window.focus(&self.overlay_focus, cx);
                    cx.notify();
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
            ("?", _) | ("/", true) => {
                self.overlay_return_focus = window.focused(cx);
                window.focus(&self.overlay_focus, cx);
                self.overlay = Some(Overlay::Help);
                cx.notify();
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn readme_table_matches_the_help_dialog() {
        let rows: String = super::SHORTCUTS
            .iter()
            .map(|(key, action)| format!("| {key} | {action} |\n"))
            .collect();
        let table = format!("| Key | Action |\n| --- | --- |\n{rows}\n");
        assert!(
            include_str!("../../README.md").contains(&table),
            "Update the README shortcut table to:\n{table}"
        );
    }
}
