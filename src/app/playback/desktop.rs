//! Desktop media control publication and commands.

use super::super::*;

impl Music {
    pub(in crate::app) fn publish_desktop(&self) {
        let Some(service) = &self.media_controls else {
            return;
        };
        let queue = &self.library.queue;
        let skip = |track: &Track| is_disliked(&self.ratings, track);
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
        let can_go_next = queue.next_index(self.library.repeat, false, skip).is_some()
            || matches!(
                &self.watch_pending,
                Some(
                    PlaybackQuery::Playlist(_)
                        | PlaybackQuery::Collection(_)
                        | PlaybackQuery::Watch {
                            mode: WatchMode::Issued | WatchMode::Radio,
                            ..
                        }
                )
            )
            || (queue.incomplete && (self.queue_source.can_continue() || queue.source.is_some()))
            || (self.library.music_autoplay()
                && (self.autoplay_tracks.iter().any(|track| !skip(track))
                    || self.radio.is_some()
                    || self.watch_pending.is_some()
                    || self.radio_loading
                    || seed_available));
        let waiting = self.queue_waiting.is_some()
            || self.autoplay_waiting.is_some()
            || self.waiting_for_watch();
        service.publish(media_controls::State {
            playback: if self.playback == Playback::Stopped {
                media_controls::PlaybackStatus::Stopped
            } else if self.playback.is_paused() {
                media_controls::PlaybackStatus::Paused
            } else {
                media_controls::PlaybackStatus::Playing
            },
            metadata: queue
                .current()
                .map(|track| media_controls::Metadata::new(track, self.duration)),
            track_id: self.active_id,
            position: self.position,
            advancing: self.playback.is_advancing(),
            volume: self.library.effective_volume(),
            rate: self.library.speed,
            repeat: self.library.repeat,
            shuffle: self
                .collection_playback()
                .map(|collection| collection.order == queue_source::PlaybackOrder::Shuffled),
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

    pub(in crate::app) fn desktop_event(
        &mut self,
        event: media_controls::Event,
        cx: &mut Context<Self>,
    ) {
        match event {
            media_controls::Event::Next => self.next(false, cx),
            media_controls::Event::Previous => self.previous(cx),
            media_controls::Event::Play
                if self.playback == Playback::Stopped || self.playback.is_paused() =>
            {
                self.toggle_play(cx)
            }
            media_controls::Event::Pause
                if self.playback != Playback::Stopped && !self.playback.is_paused() =>
            {
                self.toggle_play(cx)
            }
            media_controls::Event::Play | media_controls::Event::Pause => {}
            media_controls::Event::PlayPause => self.toggle_play(cx),
            media_controls::Event::Stop => self.stop_playback(cx),
            media_controls::Event::Seek { track_id, offset }
                if Some(track_id) == self.active_id && self.seekable =>
            {
                let seconds = (self.playback_position() + offset).max(0.);
                if self.duration > 0. && seconds > self.duration {
                    self.next(false, cx);
                } else {
                    self.seek_current(seconds, cx);
                }
            }
            media_controls::Event::SetPosition { track_id, seconds }
                if Some(track_id) == self.active_id
                    && self.seekable
                    && (self.duration <= 0. || seconds <= self.duration) =>
            {
                self.seek_current(seconds, cx)
            }
            media_controls::Event::Seek { .. } | media_controls::Event::SetPosition { .. } => {}
            media_controls::Event::Volume(volume) => {
                if !self.set_volume(volume, cx) {
                    return;
                }
                self.save_preferences();
            }
            media_controls::Event::Rate(speed) => {
                self.player_command(|player| player.set_speed(speed))
            }
            media_controls::Event::LoopStatus(repeat) => {
                self.library.repeat = repeat;
                self.save_preferences();
            }
            media_controls::Event::Shuffle(shuffle) => {
                if self.collection_playback().is_some_and(|collection| {
                    (collection.order == queue_source::PlaybackOrder::Shuffled) != shuffle
                }) {
                    self.shuffle_queue(cx);
                }
                return;
            }
            media_controls::Event::Error(error) => self.error = Some(error),
        }
        self.notify_playback(cx);
    }
}
