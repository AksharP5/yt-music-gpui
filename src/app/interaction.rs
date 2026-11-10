//! Keyboard shortcuts and seek/volume drag interactions.

use super::*;

#[derive(Clone, Copy, PartialEq)]
pub(in crate::app) enum Drag {
    Seek,
    Volume,
    /// Reordering Up next; the rows track the pointer in `PlayerPage`.
    Queue,
}

impl Music {
    pub(in crate::app) fn drag_to(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        match self.drag {
            Some(Drag::Seek) if self.duration > 0. => {
                let bounds = self.seek_bounds.get();
                self.drag_position = Some(
                    ((position.x - bounds.left()) / bounds.size.width).clamp(0., 1.) as f64
                        * self.duration,
                );
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
        match self.drag.take() {
            Some(Drag::Seek) => {
                if let Some(seconds) = self.drag_position.take() {
                    self.seek_current(seconds, cx);
                }
            }
            Some(Drag::Volume) => self.save_preferences(),
            Some(Drag::Queue) => self.drop_queue_row(cx),
            None => return,
        }
        cx.notify();
    }

    pub(in crate::app) fn key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = event.keystroke.key.as_str();
        let modifiers = event.keystroke.modifiers;
        let control = modifiers.control
            && !modifiers.alt
            && !modifiers.platform
            && !modifiers.function
            && !modifiers.shift;
        if control && key == "q" {
            cx.quit();
            return;
        }
        if self.overlay.is_some() && key != "escape" {
            let menu = self.settings_menu.is_some()
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
            if modifiers.shift {
                window.focus_prev(cx);
            } else {
                window.focus_next(cx);
            }
            cx.stop_propagation();
            return;
        }
        if (control && key == "k")
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
        if control && key == "r" {
            self.reload_current_page(cx);
            return;
        }
        if modifiers.alt
            && !modifiers.control
            && !modifiers.platform
            && !modifiers.function
            && !modifiers.shift
            && key == "left"
        {
            self.go_back(cx);
            return;
        }
        if self.search.focus_handle(cx).is_focused(window) {
            return;
        }
        if key == "space" && !self.focus.is_focused(window) {
            return;
        }
        if control {
            match key {
                "right" => self.next(false, cx),
                "left" => self.previous(cx),
                _ => {}
            }
            return;
        }
        if modifiers.control
            || modifiers.alt
            || modifiers.platform
            || modifiers.function
            || (modifiers.shift && !matches!(key, "?" | "/"))
        {
            return;
        }
        match key {
            "space" => self.toggle_play(cx),
            "f" => self.toggle_video_fullscreen(window, cx),
            "right" => {
                let seconds = (self.playback_position() + 5.).min(self.duration);
                self.seek_current(seconds, cx);
            }
            "left" => {
                let seconds = (self.playback_position() - 5.).max(0.);
                self.seek_current(seconds, cx);
            }
            "up" => {
                if self.set_volume(self.library.volume + 5., cx) {
                    self.save_preferences();
                }
            }
            "down" => {
                if self.set_volume(self.library.volume - 5., cx) {
                    self.save_preferences();
                }
            }
            "l" => {
                if let Some(track) = self.library.queue.current().cloned() {
                    self.toggle_like(&track, cx);
                }
            }
            "q" => self.toggle_player_page(cx),
            "escape" => {
                if self.settings_menu.take().is_some() {
                    window.focus(&self.overlay_focus, cx);
                    cx.notify();
                    return;
                }
                if self.overlay.is_some() {
                    self.close_overlay(window, cx);
                    return;
                }
                if self.player_open && window.is_fullscreen() {
                    self.toggle_video_fullscreen(window, cx);
                    return;
                }
                if self.player_open {
                    self.player_open = false;
                    cx.notify();
                }
            }
            "?" => {
                self.overlay = Some(Overlay::Help);
                cx.notify();
            }
            "/" if event.keystroke.modifiers.shift => {
                self.overlay = Some(Overlay::Help);
                cx.notify();
            }
            _ => {}
        }
    }
}
