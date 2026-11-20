//! Desktop media control publication and commands.

use super::super::*;

impl Music {
    pub(in crate::app) fn publish_desktop(&self) {
        let Some(service) = &self.media_controls else {
            return;
        };
        let queue = &self.library.queue;
        let transport = self.transport();
        let waiting = self.queue_waiting.is_some()
            || self.autoplay_waiting.is_some()
            || self.waiting_for_watch();
        service.publish(media_controls::State {
            playback: transport.status,
            metadata: queue
                .current()
                .map(|track| media_controls::Metadata::new(track, transport.duration)),
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
            can_go_next: transport.can_go_next,
            can_go_previous: transport.can_go_previous,
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
            media_controls::Event::Next => return self.skip_next(cx),
            media_controls::Event::Previous => self.previous(cx),
            media_controls::Event::Play
                if self.transport().status != media_controls::PlaybackStatus::Playing =>
            {
                self.toggle_play(cx)
            }
            media_controls::Event::Pause
                if self.transport().status == media_controls::PlaybackStatus::Playing =>
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
                let duration = self.transport().duration;
                if duration > 0. && seconds > duration {
                    return self.skip_next(cx);
                } else {
                    self.seek_current(seconds, cx);
                }
            }
            media_controls::Event::SetPosition { track_id, seconds }
                if Some(track_id) == self.active_id && self.seekable =>
            {
                let duration = self.transport().duration;
                if duration <= 0. || seconds <= duration {
                    self.seek_current(seconds, cx);
                }
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
