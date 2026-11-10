//! MPRIS state publication and desktop playback commands.

use super::super::*;

impl Music {
    pub(in crate::app) fn publish_desktop(&self) {
        let Some(service) = &self.mpris else { return };
        let queue = &self.library.queue;
        let loaded_next = queue.index.map_or(!queue.tracks.is_empty(), |index| {
            index + 1 < queue.tracks.len()
        });
        let seed_available = !self.autoplay_seeded
            && self.now_playing.as_ref().is_some_and(|watch| {
                watch
                    .current
                    .as_ref()
                    .zip(queue.current())
                    .is_some_and(|(issued, current)| issued.id == current.id)
                    && watch.current_index.is_some_and(|index| {
                        index + 1 < watch.queue.len() || watch.continuation.is_some()
                    })
            });
        let can_go_next = loaded_next
            || matches!(
                &self.watch_pending,
                Some(
                    PlaybackQuery::Playlist(_)
                        | PlaybackQuery::Watch {
                            mode: WatchMode::Issued | WatchMode::Radio,
                            ..
                        }
                )
            )
            || (queue.incomplete && self.queue_source.can_continue())
            || (!queue.incomplete
                && self.library.repeat != Repeat::Off
                && !queue.tracks.is_empty())
            || (self.library.music_autoplay()
                && (!self.autoplay_tracks.is_empty()
                    || self.radio.is_some()
                    || self.watch_pending.is_some()
                    || self.radio_loading
                    || seed_available));
        let waiting = self.queue_waiting.is_some()
            || self.autoplay_waiting.is_some()
            || self.waiting_for_watch();
        service.publish(mpris::State {
            playback: if self.playback == Playback::Stopped {
                mpris::PlaybackStatus::Stopped
            } else if self.playback.is_paused() {
                mpris::PlaybackStatus::Paused
            } else {
                mpris::PlaybackStatus::Playing
            },
            metadata: queue
                .current()
                .map(|track| mpris::Metadata::new(track, self.duration)),
            track_id: self.active_id,
            position: self.position,
            advancing: self.playback.is_advancing(),
            volume: self.library.volume,
            rate: self.library.speed,
            repeat: self.library.repeat,
            can_play: self.player.is_some() && (!queue.tracks.is_empty() || waiting),
            can_pause: self.active_id.is_some() || waiting,
            can_go_next: self.player.is_some() && can_go_next,
            can_go_previous: self.player.is_some()
                && (queue.index.is_some_and(|index| index > 0) || self.position > 3.),
            can_seek: self.seekable
                && self.active_id.is_some()
                && !matches!(self.playback, Playback::Stopped | Playback::Loading { .. }),
        });
    }

    pub(in crate::app) fn desktop_event(&mut self, event: mpris::Event, cx: &mut Context<Self>) {
        match event {
            mpris::Event::Next => self.next(false, cx),
            mpris::Event::Previous => self.previous(cx),
            mpris::Event::Play
                if self.playback == Playback::Stopped || self.playback.is_paused() =>
            {
                self.toggle_play(cx)
            }
            mpris::Event::Pause
                if self.playback != Playback::Stopped && !self.playback.is_paused() =>
            {
                self.toggle_play(cx)
            }
            mpris::Event::Play | mpris::Event::Pause => {}
            mpris::Event::PlayPause => self.toggle_play(cx),
            mpris::Event::Stop => self.stop_playback(cx),
            mpris::Event::Seek { track_id, offset }
                if Some(track_id) == self.active_id && self.seekable =>
            {
                let seconds = (self.playback_position() + offset).max(0.);
                if self.duration > 0. && seconds > self.duration {
                    self.next(false, cx);
                } else {
                    self.seek_current(seconds, cx);
                }
            }
            mpris::Event::SetPosition { track_id, seconds }
                if Some(track_id) == self.active_id
                    && self.seekable
                    && (self.duration <= 0. || seconds <= self.duration) =>
            {
                self.seek_current(seconds, cx)
            }
            mpris::Event::Seek { .. } | mpris::Event::SetPosition { .. } => {}
            mpris::Event::Volume(volume) => {
                if !self.set_volume(volume, cx) {
                    return;
                }
                self.save_preferences();
            }
            mpris::Event::Rate(speed) => self.player_command(|player| player.set_speed(speed)),
            mpris::Event::LoopStatus(repeat) => {
                self.library.repeat = repeat;
                self.save_preferences();
            }
            mpris::Event::Error(error) => self.error = Some(error),
        }
        self.notify_playback(cx);
    }
}
