//! Window composition and cached page body, kept separate from the playback bar.

use super::*;

// Music owns commands and state. This cached sibling of PlayerBar only redraws
// when Music notifies, rather than on playback position samples.
pub(super) struct MusicContent {
    music: WeakEntity<Music>,
    _subscription: Subscription,
    #[cfg(test)]
    pub(super) renders: usize,
}

impl MusicContent {
    pub(super) fn new(music: Entity<Music>, cx: &mut Context<Self>) -> Self {
        let subscription = cx.observe(&music, |_, _, cx| cx.notify());
        Self {
            music: music.downgrade(),
            _subscription: subscription,
            #[cfg(test)]
            renders: 0,
        }
    }
}

impl Render for MusicContent {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        #[cfg(test)]
        {
            self.renders += 1;
        }
        self.music
            .update(cx, |music, cx| music.render_content(window, cx))
            .expect("Music owns its content view")
    }
}

impl Music {
    pub(super) fn restore_keyboard_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // A scroll request can land between frames. The target handle is already focused,
        // but its virtual row has not entered the dispatch tree yet.
        if self.queue_focus.reveal_pending(window)
            || self.sidebar_focus.reveal_pending(window)
            || self.comments_reveal_pending(window)
        {
            #[cfg(test)]
            eprintln!(
                "FOCUS_LOST pending_reveal queue={:?} sidebar={:?}",
                self.queue_focus.focused(window),
                self.sidebar_focus.focused(window)
            );
            cx.notify();
            return;
        }
        #[cfg(test)]
        {
            self.focus_restores += 1;
        }
        let focus = if self.settings_menu.is_some() {
            &self.settings_focus
        } else if self.overlay.is_some() || self.sort_menu {
            &self.overlay_focus
        } else {
            &self.focus
        };
        window.focus(focus, cx);
    }

    pub(in crate::app) fn sidebar_compact(&self, width: f32) -> bool {
        self.sidebar_collapsed.unwrap_or(width < 1150.)
    }

    fn render_content(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let player_page = self.player_open && self.library.queue.current().is_some();
        let fullscreen = player_page && self.library.video && window.is_fullscreen();
        let viewport = window.viewport_size();
        let width: f32 = viewport.width.into();
        let height: f32 = viewport.height.into();
        let compact = self.sidebar_compact(width);
        let sidebar_width = if compact { 72. } else { 240. };
        let available = width - sidebar_width;
        let inset = if available >= 1000. { 100. } else { 32. };
        let background = self.page_background(compact, width, height, window, cx);
        let header = (!fullscreen).then(|| self.header(inset, compact, width, cx));
        let sidebar = (!fullscreen).then(|| self.sidebar(compact, cx).into_any_element());
        let divider = (!fullscreen).then(|| self.header_divider()).flatten();
        let content = if player_page {
            let body = height - player_bar_height(width) - if fullscreen { 0. } else { 64. };
            self.player_page(width, sidebar_width, body, window, cx)
        } else {
            self.content(width, available, cx)
        };
        div()
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .children(background)
            .children(header)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .children(sidebar)
                    .child(content),
            )
            .children(divider)
            .into_any_element()
    }
}

fn player_bar_height(width: f32) -> f32 {
    if width < 936. { 64. } else { 72. }
}

impl Render for Music {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        #[cfg(test)]
        {
            self.renders += 1;
        }
        if self.video_fullscreen && !(self.player_open && self.library.video) {
            if window.is_fullscreen() {
                window.toggle_fullscreen();
            }
            self.video_fullscreen = false;
        }
        if self.video_error.is_none()
            && self
                .video
                .as_ref()
                .is_some_and(|video| video.status() == VideoStatus::Failed)
        {
            self.video_error = Some("Video is unavailable. Try again or switch to audio.".into());
        }
        let native_video = self.player_open
            && self.library.video
            && self.overlay.is_none()
            && self.active_id.is_some()
            && self.playback != Playback::Stopped
            && self.video_error.is_none();
        if !native_video && let Some(video) = &self.video {
            video.detach();
        }
        if self.video_transparent != native_video {
            window.set_background_appearance(if native_video {
                WindowBackgroundAppearance::Transparent
            } else {
                WindowBackgroundAppearance::Opaque
            });
            self.video_transparent = native_video;
        }
        let width: f32 = window.viewport_size().width.into();
        let height: f32 = window.viewport_size().height.into();
        let has_player = self.library.queue.current().is_some();
        let player_height = if has_player {
            player_bar_height(width)
        } else {
            0.
        };
        let content = self.content_view.clone().cached(StyleRefinement {
            size: SizeRefinement {
                width: Some(px(width).into()),
                height: Some(px((height - player_height).max(0.)).into()),
            },
            flex_shrink: Some(0.),
            ..Default::default()
        });
        let player = has_player.then(|| self.player_bar.clone());
        let overlay = if matches!(self.overlay, Some(Overlay::Account)) {
            Some(self.account_popup(window, cx))
        } else {
            self.overlay
                .as_ref()
                .map(|kind| self.overlay(kind, window, cx))
        }
        .map(|overlay| {
            div()
                .id("overlay-focus")
                .track_focus(&self.overlay_focus)
                .tab_group()
                .tab_stop(false)
                .absolute()
                .inset_0()
                .child(overlay)
        });
        div()
            .size_full()
            .relative()
            .when(!native_video, |view| view.bg(self.theme.bg))
            .text_color(self.theme.text)
            .text_size(px(14.))
            .font_family("Roboto")
            .flex()
            .flex_col()
            .key_context("Music")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::key_down))
            .on_mouse_move(cx.listener(|view, event: &MouseMoveEvent, _, cx| {
                if view.drag.is_some() {
                    view.drag_to(event.position, cx);
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|view, _, _, cx| view.end_drag(cx)),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|view, _, _, cx| view.end_drag(cx)),
            )
            .child(content)
            .children(player)
            .children(overlay)
    }
}
