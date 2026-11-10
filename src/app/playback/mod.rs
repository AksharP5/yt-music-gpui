//! Playback state, libmpv commands/events, seeking, and media quality.

pub(in crate::app) mod desktop;
pub(in crate::app) mod listening;
pub(in crate::app) mod queue;

use super::*;
pub(in crate::app) use queue::PlaybackChannels;

#[derive(Clone)]
pub(in crate::app) enum PlaybackQuery {
    Watch { track: Box<Track>, mode: WatchMode },
    Playlist(Arc<PlaylistTarget>),
    More(WatchContinuation),
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub(in crate::app) enum QueueUpdate {
    Preserve,
    Adopt,
    Replace,
}
impl From<WatchMode> for QueueUpdate {
    fn from(mode: WatchMode) -> Self {
        match mode {
            WatchMode::Details => Self::Preserve,
            WatchMode::Issued => Self::Adopt,
            WatchMode::Radio => Self::Replace,
        }
    }
}
pub(in crate::app) struct PlaybackJob {
    pub(in crate::app) id: u64,
    pub(in crate::app) account_revision: u64,
    pub(in crate::app) query: PlaybackQuery,
    pub(in crate::app) session: Option<Arc<Session>>,
}
pub(in crate::app) enum PlaybackResult {
    Watch {
        id: u64,
        account_revision: u64,
        update: QueueUpdate,
        result: Result<Box<Watch>, String>,
        session_rejected: bool,
    },
    More {
        id: u64,
        result: Result<WatchPage, String>,
        session_rejected: bool,
    },
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub(in crate::app) enum Playback {
    Stopped,
    Loading { paused: bool, retried: bool },
    Loaded { paused: bool, buffering: bool },
}
#[derive(Clone, Copy)]
pub(in crate::app) struct SeekIntent {
    pub(in crate::app) track_id: u64,
    pub(in crate::app) revision: u64,
    pub(in crate::app) seconds: f64,
}

#[derive(Clone, Copy)]
pub(in crate::app) struct QueueAdvance {
    pub(in crate::app) natural: bool,
    pub(in crate::app) playback: Playback,
}
impl Playback {
    pub(in crate::app) fn is_paused(self) -> bool {
        matches!(
            self,
            Self::Loaded { paused: true, .. } | Self::Loading { paused: true, .. }
        )
    }

    pub(in crate::app) fn with_pause(self, paused: bool) -> Self {
        match self {
            Self::Loading { retried, .. } => Self::Loading { paused, retried },
            Self::Loaded { buffering, .. } => Self::Loaded { paused, buffering },
            Self::Stopped => Self::Loaded {
                paused,
                buffering: false,
            },
        }
    }

    pub(in crate::app) fn with_buffering(self, buffering: bool) -> Self {
        match self {
            Self::Loaded { paused, .. } => Self::Loaded { paused, buffering },
            _ => self,
        }
    }

    pub(in crate::app) fn is_advancing(self) -> bool {
        matches!(
            self,
            Self::Loaded {
                paused: false,
                buffering: false
            }
        )
    }

    pub(in crate::app) fn toggled(self) -> Self {
        self.with_pause(self != Self::Stopped && !self.is_paused())
    }

    pub(in crate::app) fn retry(&mut self) -> bool {
        let Self::Loading { retried, .. } = self else {
            return false;
        };
        if *retried {
            return false;
        }
        *retried = true;
        true
    }
}
pub(in crate::app) enum PlaybackIntent {
    Track(PlaybackResume),
    Advance(QueueAdvance),
    Queue {
        query: PlaybackQuery,
        paused: bool,
        advance: Option<QueueAdvance>,
    },
}
impl PlaybackIntent {
    pub(in crate::app) fn is_paused(&self) -> bool {
        match self {
            Self::Track(track) => track.paused,
            Self::Advance(intent) => intent.playback.is_paused(),
            Self::Queue { paused, .. } => *paused,
        }
    }
    pub(in crate::app) fn toggle_pause(&mut self) {
        match self {
            Self::Advance(intent) => intent.playback = intent.playback.toggled(),
            Self::Track(track) => track.paused = !track.paused,
            Self::Queue { paused, .. } => *paused = !*paused,
        }
    }
}

pub(in crate::app) struct PlaybackResume {
    pub(in crate::app) video_id: String,
    pub(in crate::app) position: f64,
    pub(in crate::app) paused: bool,
    pub(in crate::app) mode: WatchMode,
    pub(in crate::app) watch: Option<Arc<WatchTarget>>,
    pub(in crate::app) record_history: bool,
    pub(in crate::app) advance: Option<QueueAdvance>,
}

impl Music {
    pub(in crate::app) fn player_command(
        &mut self,
        command: impl FnOnce(&Player) -> anyhow::Result<()>,
    ) {
        if let Some(player) = &self.player
            && let Err(error) = command(player)
        {
            self.error = Some(error.to_string());
        }
    }

    pub(in crate::app) fn playback_position(&self) -> f64 {
        self.pending_seek
            .filter(|seek| Some(seek.track_id) == self.active_id)
            .map_or(self.position, |seek| seek.seconds)
    }

    pub(in crate::app) fn seek_current(&mut self, seconds: f64, cx: &mut Context<Self>) {
        self.report_listening(false);
        self.listening_discontinuity();
        if let Some(Some(PlaybackIntent::Track(resume))) = self
            .account_operation
            .as_mut()
            .and_then(AccountOperation::pending_playback)
        {
            resume.position = seconds.max(0.);
            resume.advance = None;
            self.position = resume.position;
            cx.notify();
            return;
        }
        let Some(track_id) = self.active_id else {
            return;
        };
        if matches!(self.playback, Playback::Loading { .. }) {
            let previous = self.position;
            let pending = self.pending_seek.take();
            self.position = seconds;
            if let Err(error) = self.reload_stream() {
                self.position = previous;
                self.pending_seek = pending;
                self.error = Some(error.to_string());
            }
        } else if let Some(player) = &self.player {
            match player.seek(track_id, seconds) {
                Ok(revision) => {
                    self.pending_seek = Some(SeekIntent {
                        track_id,
                        revision,
                        seconds,
                    });
                }
                Err(error) => self.error = Some(error.to_string()),
            }
        }
        cx.notify();
    }

    pub(in crate::app) fn play_current(&mut self, cx: &mut Context<Self>) {
        self.play_current_resuming(false, WatchMode::Details, cx);
    }

    pub(in crate::app) fn play_current_at(
        &mut self,
        start: f64,
        paused: bool,
        watch_mode: WatchMode,
        cx: &mut Context<Self>,
    ) {
        self.finish_listening(false);
        if let Some(resume) = self
            .account_operation
            .as_mut()
            .and_then(AccountOperation::pending_playback)
        {
            *resume = self.library.queue.current().map(|track| {
                PlaybackIntent::Track(PlaybackResume {
                    video_id: track.id.clone(),
                    position: start,
                    paused,
                    mode: watch_mode,
                    watch: track.details.watch.clone(),
                    record_history: true,
                    advance: None,
                })
            });
            self.position = start;
            self.playback = Playback::Loading {
                paused,
                retried: false,
            };
            self.player_command(Player::stop);
            self.active_id = None;
            self.seekable = false;
            self.pending_seek = None;
            self.playback_generation.fetch_add(1, Ordering::Relaxed);
            self.watch_pending = None;
            self.discard_pending_playback();
            if matches!(
                self.account_operation,
                Some(AccountOperation::RefreshFailed { .. })
            ) {
                self.next_session_refresh = None;
                self.refresh_session(false, cx);
            }
            cx.notify();
            return;
        }
        self.queue_waiting = None;
        let Some(track) = self.library.queue.current().cloned() else {
            return;
        };
        self.track_state.cancel();
        self.track_state_id = None;
        if !self
            .media_pair
            .as_ref()
            .is_some_and(|pair| pair.target(&track.id, self.library.video).is_some())
        {
            self.media_pair = None;
        }
        if track.details.episode.is_some() {
            self.reset_radio();
        }
        self.autoplay_waiting = None;
        self.notice = None;
        self.playback_generation.fetch_add(1, Ordering::Relaxed);
        self.watch_pending = None;
        self.discard_pending_playback();
        // The queue may already name a different recording. Retire the old load
        // before enqueueing, including when the player has stopped accepting work.
        self.active_id = None;
        self.seekable = false;
        self.pending_seek = None;
        self.playback = Playback::Stopped;
        self.position = start;
        let Some(player) = &self.player else {
            self.playback = Playback::Stopped;
            self.error =
                Some("The audio player is unavailable. Restart the app to try again.".into());
            cx.notify();
            return;
        };
        match player.play(
            &track.id,
            &format!("{} - {}", track.artist, track.title),
            PlayOptions {
                quality: self.library.quality,
                video: self.library.video.then_some(self.library.video_quality),
                start,
                paused,
            },
        ) {
            Ok(id) => {
                self.active_id = Some(id);
                self.playback = Playback::Loading {
                    paused,
                    retried: false,
                };
                self.position = start;
                self.duration = track.duration.unwrap_or(0) as f64;
                self.error = None;
                if watch_mode != WatchMode::Details
                    || self.account.is_some()
                    || !self
                        .now_playing
                        .as_ref()
                        .and_then(|watch| watch.current.as_ref())
                        .is_some_and(|current| current.id == track.id)
                {
                    self.now_playing = None;
                    self.autoplay_seeded = false;
                    if matches!(self.overlay, Some(Overlay::SongText { .. })) {
                        self.song_text.cancel();
                        self.overlay = None;
                    }
                    self.playback_request(
                        PlaybackQuery::Watch {
                            track: Box::new(track),
                            mode: watch_mode,
                        },
                        cx,
                    );
                }
            }
            Err(error) => self.error = Some(error.to_string()),
        }
        self.extend_autoplay(cx);
        self.prefetch_queue(cx);
        self.save();
        cx.notify();
    }

    pub(in crate::app) fn play_chapter(
        &mut self,
        track: Track,
        seconds: u32,
        cx: &mut Context<Self>,
    ) {
        self.overlay = None;
        let current = self
            .library
            .queue
            .current()
            .is_some_and(|current| current.id == track.id);
        if current && self.active_id.is_some() && self.playback != Playback::Stopped {
            self.seek_current(seconds as f64, cx);
            return;
        }
        if !current {
            self.queue_source.cancel();
            self.queue_waiting = None;
            self.reset_radio();
            self.now_playing = None;
            self.library.queue.replace(vec![track], 0);
        }
        self.play_current_at(seconds as f64, false, WatchMode::Details, cx);
    }

    /// Lets the player resolve the natural successor ahead of time. Cheap to call on
    /// every render: the player ignores an unchanged candidate.
    pub(in crate::app) fn prepare_upcoming(&mut self) {
        let queue = &self.library.queue;
        let next = (self.active_id.is_some() && self.playback != Playback::Stopped)
            .then(|| {
                let index = queue.index?;
                if self.library.repeat == Repeat::One {
                    return queue.current();
                }
                queue.tracks.get(index + 1).or_else(|| {
                    if queue.incomplete {
                        return None;
                    }
                    if self.library.repeat == Repeat::All {
                        return queue.tracks.first();
                    }
                    self.library
                        .music_autoplay()
                        .then(|| self.autoplay_tracks.front())
                        .flatten()
                })
            })
            .flatten();
        if let Some(player) = &self.player
            && let Err(error) = player.prepare_next(
                next.map(|track| track.id.as_str()),
                PlayOptions {
                    quality: self.library.quality,
                    video: self.library.video.then_some(self.library.video_quality),
                    ..Default::default()
                },
            )
        {
            self.error = Some(error.to_string());
        }
    }

    pub(in crate::app) fn set_audio_quality(
        &mut self,
        quality: AudioQuality,
        cx: &mut Context<Self>,
    ) {
        if self.library.quality == quality {
            return;
        }
        self.library.quality = quality;
        if self.active_id.is_some() && self.playback != Playback::Stopped {
            if let Playback::Loading { retried, .. } = &mut self.playback {
                *retried = false;
            }
            if let Err(error) = self.reload_stream() {
                self.error = Some(error.to_string());
            }
        }
        self.save_preferences();
        cx.notify();
    }

    pub(in crate::app) fn set_video_mode(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.player_open |= enabled;
        self.video_error = None;
        let changed = self.library.video != enabled;
        let position = self.playback_position();
        let paused = self.playback.is_paused();
        self.library.video = enabled;
        let pending = self
            .account_operation
            .as_mut()
            .and_then(AccountOperation::pending_playback)
            .and_then(|resume| resume.as_ref());
        let deferred_source = matches!(
            pending,
            Some(PlaybackIntent::Queue { .. } | PlaybackIntent::Advance(_))
                | Some(PlaybackIntent::Track(PlaybackResume {
                    mode: WatchMode::Issued,
                    ..
                }))
        );
        let pending_track = matches!(pending, Some(PlaybackIntent::Track(_)));
        if deferred_source {
            self.save();
            cx.notify();
            return;
        }
        let alternative = self
            .media_pair
            .as_ref()
            .and_then(|pair| {
                let current = self.library.queue.current()?;
                pair.target(&current.id, enabled)
                    .filter(|target| target.id != current.id)
            })
            .cloned();
        if let Some(track) = alternative
            && let Some(current) = self
                .library
                .queue
                .index
                .and_then(|index| self.library.queue.tracks.get_mut(index))
        {
            *current = track;
            self.reset_radio();
            self.now_playing = None;
            if self.active_id.is_some() || pending_track {
                self.play_current_at(position, paused, WatchMode::Details, cx);
            }
        } else if changed
            && self.active_id.is_some()
            && self.playback != Playback::Stopped
            && let Err(error) = self.reload_stream()
        {
            self.error = Some(error.to_string());
        }
        self.save();
        cx.notify();
    }

    pub(in crate::app) fn set_video_quality(
        &mut self,
        quality: VideoQuality,
        cx: &mut Context<Self>,
    ) {
        if self.library.video_quality == quality {
            return;
        }
        self.library.video_quality = quality;
        if self.library.video && self.active_id.is_some() && self.playback != Playback::Stopped {
            if let Playback::Loading { retried, .. } = &mut self.playback {
                *retried = false;
            }
            if let Err(error) = self.reload_stream() {
                self.error = Some(error.to_string());
            }
        }
        self.save_preferences();
        cx.notify();
    }

    pub(in crate::app) fn toggle_video_fullscreen(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !(self.player_open && self.library.video) {
            return;
        }
        self.video_fullscreen = !window.is_fullscreen();
        window.toggle_fullscreen();
        cx.notify();
    }

    pub(in crate::app) fn reload_stream(&mut self) -> anyhow::Result<()> {
        let track = self
            .library
            .queue
            .current()
            .ok_or_else(|| anyhow::anyhow!("There is no current song to reload."))?;
        let player = self
            .player
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("The audio player is unavailable. Restart the app."))?;
        let paused = self.playback.is_paused();
        let retried = matches!(self.playback, Playback::Loading { retried: true, .. });
        let start = self.playback_position();
        let id = player.play(
            &track.id,
            &format!("{} - {}", track.artist, track.title),
            PlayOptions {
                quality: self.library.quality,
                video: self.library.video.then_some(self.library.video_quality),
                start,
                paused,
            },
        )?;
        if self.active_id.is_some() && self.history_track == self.active_id {
            self.history_track = Some(id);
        }
        self.active_id = Some(id);
        self.seekable = false;
        self.position = start;
        self.playback = Playback::Loading { paused, retried };
        self.error = None;
        Ok(())
    }

    pub(in crate::app) fn toggle_play(&mut self, cx: &mut Context<Self>) {
        let pending = match &self.account_operation {
            Some(AccountOperation::Activating { .. }) => true,
            Some(AccountOperation::RefreshFailed { resume }) => {
                resume.is_some() || self.active_id.is_none()
            }
            Some(AccountOperation::Refreshing { resume, .. }) => resume.is_some(),
            _ => false,
        };
        if pending
            && let Some(resume) = self
                .account_operation
                .as_mut()
                .and_then(AccountOperation::pending_playback)
        {
            if let Some(intent) = resume {
                intent.toggle_pause();
            } else {
                *resume = self.library.queue.current().map(|track| {
                    PlaybackIntent::Track(PlaybackResume {
                        video_id: track.id.clone(),
                        position: 0.,
                        paused: false,
                        mode: WatchMode::Details,
                        watch: track.details.watch.clone(),
                        record_history: true,
                        advance: None,
                    })
                });
            }
            self.playback = resume
                .as_ref()
                .map_or(Playback::Stopped, |intent| Playback::Loading {
                    paused: intent.is_paused(),
                    retried: false,
                });
            if matches!(
                self.account_operation,
                Some(AccountOperation::RefreshFailed { .. })
            ) {
                self.next_session_refresh = None;
                self.refresh_session(false, cx);
            }
            cx.notify();
            return;
        }

        if let Some(intent) = self
            .queue_waiting
            .as_mut()
            .or(self.autoplay_waiting.as_mut())
        {
            intent.playback = intent.playback.toggled();
            self.playback = Playback::Loading {
                paused: intent.playback.is_paused(),
                retried: false,
            };
            cx.notify();
            return;
        }
        if self.waiting_for_watch() {
            self.playback = self.playback.toggled();
            cx.notify();
            return;
        }
        if self.active_id.is_some() && self.playback != Playback::Stopped {
            let playback = self.playback.toggled();
            if let Some(player) = &self.player {
                match player.pause(playback.is_paused()) {
                    Ok(()) => {
                        self.playback = playback;
                    }
                    Err(error) => self.error = Some(error.to_string()),
                }
            }
            cx.notify();
        } else {
            if self.library.queue.index.is_none() {
                self.library.queue.select(0);
            }
            self.player_open |= self.library.video;
            self.play_current(cx);
        }
    }

    pub(in crate::app) fn stop_playback(&mut self, cx: &mut Context<Self>) {
        self.finish_listening(false);
        if let Some(resume) = self
            .account_operation
            .as_mut()
            .and_then(AccountOperation::pending_playback)
        {
            *resume = None;
        }

        self.player_command(Player::stop);
        self.active_id = None;
        self.seekable = false;
        self.pending_seek = None;
        self.playback = Playback::Stopped;
        self.position = 0.;
        self.notice = None;
        self.queue_waiting = None;
        self.reset_radio();
        self.playback_generation.fetch_add(1, Ordering::Relaxed);
        self.watch_pending = None;
        self.discard_pending_playback();
        self.now_playing = None;
        self.video_click = None;
        cx.notify();
    }

    pub(in crate::app) fn player_event(&mut self, event: PlayerEvent, cx: &mut Context<Self>) {
        let mut changed = true;
        let mut seeked = None;
        match event {
            PlayerEvent::Stopped { track_id } if Some(track_id) == self.active_id => {
                self.stop_playback(cx);
            }
            PlayerEvent::Seekable { track_id, seekable } if Some(track_id) == self.active_id => {
                changed = self.seekable != seekable;
                self.seekable = seekable;
            }
            PlayerEvent::Notice(message) => self.notice = Some(message),
            PlayerEvent::VideoChanged => {}
            PlayerEvent::VideoError { message } => {
                if self
                    .video
                    .as_ref()
                    .is_some_and(|video| video.status() == VideoStatus::Failed)
                {
                    self.video_error = Some(message);
                } else {
                    changed = false;
                }
            }
            PlayerEvent::Volume(volume) => {
                changed = self.library.volume.round() != volume.round();
                self.library.volume = volume.clamp(0., 100.);
            }
            PlayerEvent::Speed(speed) => {
                changed = self.library.speed != speed;
                self.library.speed = speed;
                if changed {
                    self.save_preferences();
                }
            }
            PlayerEvent::Error {
                track_id,
                mut message,
            } if track_id.is_none() || track_id == self.active_id => {
                if track_id.is_some() && self.playback.retry() {
                    match self.reload_stream() {
                        Ok(()) => {
                            cx.notify();
                            return;
                        }
                        Err(error) => message = format!("{message} Retry failed: {error}"),
                    }
                }
                self.error = Some(message);
                self.autoplay_waiting = None;
                self.queue_waiting = None;
                self.active_id = None;
                self.seekable = false;
                self.playback = Playback::Stopped;
            }
            PlayerEvent::Started { track_id, paused } if Some(track_id) == self.active_id => {
                self.playback = Playback::Loaded {
                    paused,
                    buffering: false,
                };
                if self.history_track != Some(track_id)
                    && let Some(track) = self.library.queue.current().cloned()
                {
                    self.history_track = Some(track_id);
                    self.begin_listening(&track, track_id);
                    if let Some(connection) = &self.account {
                        if self
                            .account_commands
                            .try_send(account::Command::ReportPlayback {
                                session: connection.session.clone(),
                                video_id: track.id,
                                playback_id: track_id,
                            })
                            .is_err()
                        {
                            self.error = Some(
                                "Could not queue playback history. Restart the app to reconnect."
                                    .into(),
                            );
                        }
                    } else {
                        self.library.record_play(&track);
                        self.save();
                    }
                }
            }
            PlayerEvent::Position { track_id, seconds }
                if Some(track_id) == self.active_id
                    && !self
                        .pending_seek
                        .is_some_and(|seek| seek.track_id == track_id) =>
            {
                changed = self.position.floor() != seconds.floor();
                self.position = seconds;
                self.sample_listening();
                if let Some(service) = &self.mpris {
                    service.position_sample(track_id, seconds);
                }
                if changed {
                    self.publish_desktop();
                    self.player_bar.update(cx, |_, cx| cx.notify());
                }
                return;
            }
            PlayerEvent::SeekFinished {
                track_id,
                revision,
                result,
            } if Some(track_id) == self.active_id => {
                let latest = self
                    .pending_seek
                    .is_some_and(|seek| seek.track_id == track_id && seek.revision == revision);
                if latest {
                    self.pending_seek = None;
                }
                match result {
                    Ok(seconds) => {
                        self.position = seconds;
                        self.sample_listening();
                        self.checkpoint_listening(false);
                        seeked = Some((track_id, seconds));
                    }
                    Err(error) if latest => self.error = Some(error),
                    Err(_) => changed = false,
                }
            }
            PlayerEvent::Duration { track_id, seconds } if Some(track_id) == self.active_id => {
                self.duration = seconds
            }
            PlayerEvent::Paused { track_id, paused } if Some(track_id) == self.active_id => {
                self.playback = self.playback.with_pause(paused);
                self.sample_listening();
                self.checkpoint_listening(false);
                if paused {
                    self.report_listening(false);
                }
            }
            PlayerEvent::Buffering {
                track_id,
                buffering,
            } if Some(track_id) == self.active_id => {
                self.playback = self.playback.with_buffering(buffering);
                self.sample_listening();
            }
            PlayerEvent::Ended { track_id } if Some(track_id) == self.active_id => {
                self.next(true, cx);
                return;
            }
            _ => changed = false,
        }
        if changed {
            self.publish_desktop();
            cx.notify();
        }
        if let Some((track_id, seconds)) = seeked
            && let Some(service) = &self.mpris
        {
            service.seeked(track_id, seconds);
        }
    }

    pub(in crate::app) fn set_volume(&mut self, volume: f64, cx: &mut Context<Self>) -> bool {
        let volume = volume.clamp(0., 100.).round();
        if self.library.volume == volume {
            return false;
        }
        self.library.volume = volume;
        self.player_command(|player| player.set_volume(volume));
        cx.notify();
        true
    }
}
