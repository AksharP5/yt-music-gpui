//! Watch workers, queue changes, autoplay, and track advancement.

use super::super::*;

impl Music {
    pub(in crate::app) fn start_queue(&mut self, query: PlaybackQuery, cx: &mut Context<Self>) {
        self.player_open = true;
        if let Some(resume) = self
            .account_operation
            .as_mut()
            .and_then(AccountOperation::pending_playback)
        {
            *resume = Some(PlaybackIntent::Queue {
                query,
                paused: false,
                advance: None,
            });
            self.playback = Playback::Loading {
                paused: false,
                retried: false,
            };
            self.notice = Some("Loading music…".into());
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

        self.finish_listening(false);
        self.reset_radio();
        self.player_command(Player::stop);
        self.active_id = None;
        self.seekable = false;
        self.pending_seek = None;
        self.position = 0.;
        self.playback = Playback::Loading {
            paused: false,
            retried: false,
        };
        self.now_playing = None;
        self.media_pair = None;
        self.queue_source.cancel();
        self.queue_waiting = None;
        self.notice = Some("Loading music…".into());
        self.playback_request(query, cx);
    }

    pub(in crate::app) fn playback_request(
        &mut self,
        query: PlaybackQuery,
        cx: &mut Context<Self>,
    ) {
        let id = if matches!(query, PlaybackQuery::More(_)) {
            if !self.pending_playback_jobs.is_empty() || self.radio_loading {
                return;
            }
            self.radio_loading = true;
            self.radio_generation.fetch_add(1, Ordering::Relaxed) + 1
        } else {
            self.discard_pending_playback();
            self.watch_pending = Some(query.clone());
            self.playback_generation.fetch_add(1, Ordering::Relaxed) + 1
        };
        let session = self
            .account
            .as_ref()
            .map(|connection| connection.session.clone());
        if self
            .playback_jobs
            .try_send(PlaybackJob {
                id,
                account_revision: self.account_revision,
                query,
                session,
            })
            .is_err()
        {
            self.radio_loading = false;
            self.watch_pending = None;
            self.autoplay_waiting = None;
            if self.active_id.is_none() {
                self.playback = Playback::Stopped;
                self.notice = None;
            }
            self.error = Some(
                "The playback details connection stopped. Restart the app to reconnect.".into(),
            );
        }
        cx.notify();
    }

    pub(in crate::app) fn discard_pending_playback(&mut self) {
        if let Ok(PlaybackJob {
            query: PlaybackQuery::More(_),
            ..
        }) = self.pending_playback_jobs.try_recv()
        {
            self.radio_loading = false;
        }
    }

    pub(in crate::app) fn reset_radio(&mut self) {
        self.radio_generation.fetch_add(1, Ordering::Relaxed);
        self.radio = None;
        self.radio_loading = false;
        self.autoplay_seeded = false;
        self.autoplay_waiting = None;
        self.autoplay_tracks.clear();
    }

    pub(in crate::app) fn extend_autoplay(&mut self, cx: &mut Context<Self>) {
        if !self.library.music_autoplay() {
            return;
        }
        if self.autoplay_tracks.len() > 3 {
            return;
        }
        let Some(index) = self.library.queue.index else {
            return;
        };
        if self.library.queue.tracks.len().saturating_sub(index + 1) > 3 {
            return;
        }
        if let Some(continuation) = self.radio.clone() {
            self.playback_request(PlaybackQuery::More(continuation), cx);
            return;
        }
        if self.autoplay_seeded {
            return;
        }
        let Some(watch) = &self.now_playing else {
            return;
        };
        if !watch.current.as_ref().is_some_and(|track| {
            self.library
                .queue
                .current()
                .is_some_and(|current| current.id == track.id)
        }) {
            return;
        }
        self.autoplay_seeded = true;
        if let Some(current) = watch.current_index {
            self.autoplay_tracks
                .extend(watch.queue.iter().skip(current + 1).cloned());
            self.radio = watch.continuation.clone();
        }
        if self.autoplay_waiting.is_some()
            && self.autoplay_tracks.is_empty()
            && self.library.queue.index == Some(self.library.queue.tracks.len().saturating_sub(1))
            && let Some(continuation) = self.radio.clone()
        {
            self.playback_request(PlaybackQuery::More(continuation), cx);
        }
    }

    pub(in crate::app) fn resume_autoplay(&mut self, cx: &mut Context<Self>) {
        let Some(intent) = self.autoplay_waiting else {
            return;
        };
        let autoplay = self.library.music_autoplay();
        if self.library.queue.advance_with_autoplay(
            self.library.repeat,
            intent.natural,
            autoplay,
            &mut self.autoplay_tracks,
        ) {
            self.autoplay_waiting = None;
            self.follow_queue_selection(intent.playback, cx);
        } else if self.radio_loading || self.watch_pending.is_some() {
            self.playback = Playback::Loading {
                paused: intent.playback.is_paused(),
                retried: false,
            };
        } else {
            self.autoplay_waiting = None;
            self.playback = Playback::Stopped;
        }
    }

    pub(in crate::app) fn playback_result(
        &mut self,
        result: PlaybackResult,
        cx: &mut Context<Self>,
    ) {
        match result {
            PlaybackResult::Watch {
                id,
                account_revision,
                update,
                result,
                session_rejected,
            } => {
                if id != self.playback_generation.load(Ordering::Relaxed) {
                    return;
                }
                if session_rejected && account_revision == self.account_revision {
                    self.refresh_session(false, cx);
                    if matches!(
                        self.account_operation,
                        Some(AccountOperation::Refreshing { .. })
                    ) {
                        self.watch_pending = None;
                        cx.notify();
                        return;
                    }
                }
                self.watch_pending = None;
                let advance = if update != QueueUpdate::Preserve {
                    self.queue_waiting.take()
                } else {
                    None
                };
                match result {
                    Ok(watch) => {
                        let mut watch = *watch;
                        let fresh = account_revision == self.account_revision;
                        if !fresh {
                            for track in watch.current.iter_mut().chain(&mut watch.queue) {
                                track.details.clear_account_state();
                            }
                        }
                        let replaced =
                            match adopt_watch_queue(&mut self.library.queue, &watch, update) {
                                Ok(replaced) => replaced,
                                Err(error) => {
                                    if update == QueueUpdate::Replace || self.active_id.is_none() {
                                        self.playback = Playback::Stopped;
                                    }
                                    self.notice = None;
                                    self.error = Some(error);
                                    cx.notify();
                                    return;
                                }
                            };
                        if replaced {
                            self.reset_radio();
                            let session =
                                self.account.as_ref().map(|account| account.session.clone());
                            self.queue_source.attach_watch(
                                &mut self.library.queue,
                                &watch,
                                session,
                            );
                            self.queue_waiting = None;
                            self.autoplay_seeded = true;
                            self.notice = None;
                        }
                        // A single song plays from its mix, which the watch names as on the web.
                        if let [only] = self.library.queue.tracks.as_slice()
                            && self.library.queue.playing_from.is_none()
                            && watch
                                .current
                                .as_ref()
                                .is_some_and(|issued| issued.id == only.id)
                        {
                            self.library.queue.playing_from = watch.source.clone();
                        }
                        if let Some(track) = &watch.current {
                            apply_track_details(
                                &mut self.library.queue.tracks,
                                &mut self.ratings,
                                track,
                                fresh,
                            );
                        }
                        if self
                            .library
                            .queue
                            .current()
                            .is_some_and(|track| track.details.episode.is_some())
                        {
                            if self.autoplay_waiting.is_some() {
                                self.playback = Playback::Stopped;
                            }
                            self.reset_radio();
                        }
                        self.overlay_ratings();
                        self.resume_account_position(cx);
                        let current = self.library.queue.current();
                        if watch
                            .current
                            .as_ref()
                            .zip(current)
                            .is_some_and(|(issued, current)| issued.id == current.id)
                            && let Some(pair) = &watch.alternatives
                        {
                            let mut pair = pair.clone();
                            pair.song.details.clear_account_state();
                            pair.video.details.clear_account_state();
                            self.media_pair = Some(pair);
                        }
                        let switch_mode = (update == QueueUpdate::Replace
                            || self.active_id.is_some())
                            && current.is_some_and(|current| {
                                self.media_pair
                                    .as_ref()
                                    .and_then(|pair| pair.target(&current.id, self.library.video))
                                    .is_some_and(|target| target.id != current.id)
                            });
                        self.now_playing = Some(watch);
                        let player_open = self.player_open;
                        if let Some(intent) = advance {
                            self.advance_queue(intent, cx);
                        } else if update == QueueUpdate::Replace {
                            let paused = self.playback.is_paused();
                            if switch_mode {
                                self.set_video_mode(self.library.video, cx);
                            }
                            self.play_current_resuming(paused, WatchMode::Details, cx);
                        } else if switch_mode {
                            self.set_video_mode(self.library.video, cx);
                        } else {
                            self.extend_autoplay(cx);
                            self.resume_autoplay(cx);
                            if replaced {
                                self.prefetch_queue(cx);
                                self.save();
                            }
                        }
                        self.player_open = player_open;
                    }
                    Err(error)
                        if update != QueueUpdate::Preserve
                            && (update == QueueUpdate::Replace || self.active_id.is_none()) =>
                    {
                        self.playback = Playback::Stopped;
                        self.notice = None;
                        self.error = Some(error);
                    }
                    Err(error) if update == QueueUpdate::Adopt => {
                        self.notice = Some(format!(
                            "Could not load the playlist. Playing the selected song: {error}"
                        ));
                    }
                    Err(error) => {
                        if self.autoplay_waiting.take().is_some() {
                            self.playback = Playback::Stopped;
                        }
                        self.notice = Some(format!("Playback details are unavailable: {error}"))
                    }
                }
            }
            PlaybackResult::More {
                id,
                result,
                session_rejected,
            } => {
                if id != self.radio_generation.load(Ordering::Relaxed) {
                    return;
                }
                if session_rejected {
                    self.refresh_session(false, cx);
                    if matches!(
                        self.account_operation,
                        Some(AccountOperation::Refreshing { .. })
                    ) {
                        self.radio_loading = false;
                        cx.notify();
                        return;
                    }
                }
                self.radio_loading = false;
                match result {
                    Ok(page) => {
                        self.radio = page.continuation;
                        self.autoplay_tracks.extend(page.queue);
                        self.resume_autoplay(cx);
                    }
                    Err(error) => {
                        if self.autoplay_waiting.take().is_some() {
                            self.playback = Playback::Stopped;
                        }
                        self.error = Some(format!("Could not load more music: {error}"));
                    }
                }
            }
        }
        cx.notify();
    }

    pub(in crate::app) fn replace_page_queue(&mut self, selected: usize, cx: &mut Context<Self>) {
        self.player_open = true;
        self.queue_waiting = None;
        self.reset_radio();
        self.now_playing = None;
        let request = match &self.route {
            Route::Search(search) => PageRequest::Search {
                query: search.query.clone(),
                filter: search.filter,
                scope: search.scope,
            },
            _ => PageRequest::Browse,
        };
        let session = self.account.as_ref().map(|account| account.session.clone());
        self.queue_source.replace_queue(
            &mut self.library.queue,
            &self.page,
            selected,
            request,
            session,
        );
        self.play_current(cx);
    }

    pub(in crate::app) fn prefetch_queue(&mut self, cx: &mut Context<Self>) {
        if !self.library.queue.incomplete {
            return;
        }
        let remaining = self
            .library
            .queue
            .tracks
            .len()
            .saturating_sub(self.library.queue.index.map_or(0, |index| index + 1));
        if remaining > 3 {
            return;
        }
        let session = self.account.as_ref().map(|account| &account.session);
        if let Err(error) = self.queue_source.load(session, false) {
            self.error = Some(error);
            cx.notify();
        }
    }

    pub(in crate::app) fn queue_source_result(
        &mut self,
        event: queue_source::Event,
        cx: &mut Context<Self>,
    ) {
        let session = self.account.as_ref().map(|account| &account.session);
        let session_rejected = event.rejected_for(session);
        match self
            .queue_source
            .apply(event, &mut self.library.queue, session)
        {
            Ok(false) => return,
            Ok(true) => {
                if self.library.queue.tracks.is_empty() && !self.library.queue.incomplete {
                    self.clear_queue(cx);
                    return;
                }
                self.save();
                if let Some(intent) = self.queue_waiting.take() {
                    self.advance_queue(intent, cx);
                } else {
                    self.prefetch_queue(cx);
                    self.extend_autoplay(cx);
                }
            }
            Err(error) => {
                if session_rejected {
                    self.refresh_session(false, cx);
                    if matches!(
                        self.account_operation,
                        Some(AccountOperation::Refreshing { .. })
                    ) {
                        self.notice = Some("Reconnecting account…".into());
                        cx.notify();
                        return;
                    }
                }
                if self.queue_waiting.take().is_some() {
                    self.playback = Playback::Stopped;
                    self.notice = None;
                }
                self.error = Some(error);
            }
        }
        cx.notify();
    }

    pub(in crate::app) fn play_recommendation(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(track) = self.autoplay_tracks.remove(index) else {
            return;
        };
        self.autoplay_tracks.drain(..index);
        let selected = self.library.queue.tracks.len();
        self.library.queue.tracks.push(track);
        self.library.queue.select(selected);
        self.play_current(cx);
    }

    pub(in crate::app) fn clear_queue(&mut self, cx: &mut Context<Self>) {
        self.stop_playback(cx);
        self.queue_source.cancel();
        self.track_state.cancel();
        self.track_state_id = None;
        self.duration = 0.;
        self.library.queue = Default::default();
        self.media_pair = None;
        self.save();
        cx.notify();
    }

    pub(in crate::app) fn toggle_autoplay(&mut self, cx: &mut Context<Self>) {
        self.library.autoplay = !self.library.autoplay;
        if self.library.autoplay {
            self.extend_autoplay(cx);
        } else {
            if self.autoplay_waiting.is_some() {
                self.playback = Playback::Stopped;
            }
            self.reset_radio();
        }
        self.save_preferences();
        cx.notify();
    }

    pub(in crate::app) fn play_item(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(item) = self.rows.get(index).cloned() else {
            return;
        };
        match item {
            Item::Navigation(link) => self.navigate(Route::Browse(link), cx),
            Item::Collection(collection) => self.navigate(
                Route::Browse(BrowseLink::new(collection.id, collection.title)),
                cx,
            ),
            Item::Track(track) => {
                let page_queue = match &self.route {
                    Route::Browse(link) => ["VL", "MPRE", "MPSP", "MPED"]
                        .iter()
                        .any(|prefix| link.id.starts_with(prefix)),
                    Route::Liked | Route::Recent | Route::Saved | Route::Library(_) => true,
                    _ => false,
                };
                if page_queue {
                    if let Some(selected) = self.page.track_index(index) {
                        self.replace_page_queue(selected, cx);
                    }
                    return;
                }
                self.play_track(track, cx);
            }
        }
    }

    /// Play one track as a new queue, keeping its issued playlist when it has one.
    pub(in crate::app) fn play_track(&mut self, track: Track, cx: &mut Context<Self>) {
        let mode = if track
            .details
            .watch
            .as_ref()
            .is_some_and(|target| !target.is_single_video())
        {
            WatchMode::Issued
        } else {
            WatchMode::Details
        };
        self.reset_radio();
        self.queue_source.cancel();
        self.queue_waiting = None;
        self.now_playing = None;
        self.library.queue.replace(vec![track], 0);
        self.player_open = true;
        self.play_current_resuming(false, mode, cx);
    }

    pub(in crate::app) fn waiting_for_watch(&self) -> bool {
        self.watch_pending.is_some()
            && self.active_id.is_none()
            && matches!(self.playback, Playback::Loading { .. })
    }

    pub(in crate::app) fn next(&mut self, natural: bool, cx: &mut Context<Self>) {
        self.finish_listening(natural);
        let playback = self.playback;
        if let Some(resume) = self
            .account_operation
            .as_mut()
            .and_then(AccountOperation::pending_playback)
        {
            let intent = QueueAdvance { natural, playback };
            match resume {
                Some(PlaybackIntent::Queue { advance, .. }) => *advance = Some(intent),
                Some(PlaybackIntent::Track(track)) => track.advance = Some(intent),
                Some(PlaybackIntent::Advance(pending)) => *pending = intent,
                None => *resume = Some(PlaybackIntent::Advance(intent)),
            }
            self.playback = Playback::Loading {
                paused: playback.is_paused(),
                retried: false,
            };
            self.player_command(Player::stop);
            self.active_id = None;
            self.pending_seek = None;
            self.seekable = false;
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
        let playback = self
            .queue_waiting
            .take()
            .or(self.autoplay_waiting.take())
            .map_or(self.playback, |intent| intent.playback);
        self.advance_queue(QueueAdvance { natural, playback }, cx);
    }

    pub(in crate::app) fn advance_queue(&mut self, intent: QueueAdvance, cx: &mut Context<Self>) {
        let autoplay = self.library.music_autoplay();
        let wait_for_queue = matches!(
            &self.watch_pending,
            Some(
                PlaybackQuery::Playlist(_)
                    | PlaybackQuery::Watch {
                        mode: WatchMode::Radio,
                        ..
                    }
            )
        ) || (matches!(
            &self.watch_pending,
            Some(PlaybackQuery::Watch {
                mode: WatchMode::Issued,
                ..
            })
        ) && self
            .library
            .queue
            .index
            .is_none_or(|index| index + 1 >= self.library.queue.tracks.len()));
        if !wait_for_queue
            && self.library.queue.advance_with_autoplay(
                self.library.repeat,
                intent.natural,
                autoplay,
                &mut self.autoplay_tracks,
            )
        {
            self.follow_queue_selection(intent.playback, cx);
        } else {
            self.player_command(Player::stop);
            self.active_id = None;
            self.seekable = false;
            self.pending_seek = None;
            self.playback = Playback::Stopped;
            self.position = self.duration;
            self.notice = None;
            if wait_for_queue {
                self.queue_waiting = Some(intent);
                if intent.playback != Playback::Stopped {
                    self.playback = Playback::Loading {
                        paused: intent.playback.is_paused(),
                        retried: false,
                    };
                }
                self.notice = Some("Loading more queued songs".into());
                cx.notify();
                return;
            }
            if self.library.queue.incomplete {
                self.autoplay_waiting = None;
                let session = self.account.as_ref().map(|account| &account.session);
                match self.queue_source.load(session, !intent.natural) {
                    Ok(true) => {
                        self.queue_waiting = Some(intent);
                        if intent.playback != Playback::Stopped {
                            self.playback = Playback::Loading {
                                paused: intent.playback.is_paused(),
                                retried: false,
                            };
                        }
                        self.notice = Some("Loading more queued songs".into());
                    }
                    Ok(false) => {
                        self.error =
                            Some("Open the playlist again to load the rest of this queue.".into())
                    }
                    Err(error) => self.error = Some(error),
                }
                cx.notify();
                return;
            }
            if autoplay {
                self.autoplay_waiting = Some(intent);
                self.extend_autoplay(cx);
                self.resume_autoplay(cx);
            }
            cx.notify();
        }
    }

    pub(in crate::app) fn previous(&mut self, cx: &mut Context<Self>) {
        let playback = self
            .queue_waiting
            .take()
            .or(self.autoplay_waiting.take())
            .map_or(self.playback, |intent| intent.playback);
        if self.active_id.is_none() {
            if self.playback_position() <= 3. {
                self.library.queue.previous();
            }
            self.follow_queue_selection(playback, cx);
            return;
        }
        if self.playback_position() > 3. {
            self.seek_current(0., cx);
        } else if self.library.queue.previous() {
            self.follow_queue_selection(playback, cx);
        }
    }

    pub(in crate::app) fn follow_queue_selection(
        &mut self,
        playback: Playback,
        cx: &mut Context<Self>,
    ) {
        if playback != Playback::Stopped {
            self.play_current_resuming(playback.is_paused(), WatchMode::Details, cx);
            return;
        }
        self.stop_playback(cx);
        self.duration = self
            .library
            .queue
            .current()
            .and_then(|track| track.duration)
            .unwrap_or(0) as f64;
        self.refresh_current_account(cx);
        self.prefetch_queue(cx);
        self.save();
    }

    pub(in crate::app) fn enqueue(&mut self, track: Track, cx: &mut Context<Self>) {
        self.notice = Some(format!("Added {} to the queue", track.title));
        self.library.queue.tracks.push(track);
        self.queue_changed(cx);
    }

    pub(in crate::app) fn queue_changed(&mut self, cx: &mut Context<Self>) {
        if let Some(intent) = self.queue_waiting.take() {
            self.advance_queue(intent, cx);
        } else {
            self.resume_autoplay(cx);
        }
        self.prefetch_queue(cx);
        self.save();
        cx.notify();
    }

    pub(in crate::app) fn remove_queue_track(&mut self, index: usize, cx: &mut Context<Self>) {
        let playback = self
            .queue_waiting
            .or(self.autoplay_waiting)
            .map_or(self.playback, |intent| intent.playback);
        let unloaded_next =
            self.library.queue.incomplete && index + 1 == self.library.queue.tracks.len();
        if self.library.queue.remove(index) {
            self.stop_playback(cx);
            if unloaded_next {
                self.advance_queue(
                    QueueAdvance {
                        natural: false,
                        playback,
                    },
                    cx,
                );
            } else if self.library.queue.current().is_some() {
                self.follow_queue_selection(playback, cx);
            }
        }
        if self.library.queue.tracks.is_empty() && !self.library.queue.incomplete {
            self.clear_queue(cx);
        }
        self.queue_changed(cx);
    }
}

pub(in crate::app) fn adopt_watch_queue(
    queue: &mut Queue,
    watch: &Watch,
    update: QueueUpdate,
) -> Result<bool, String> {
    if update == QueueUpdate::Preserve {
        return Ok(false);
    }
    if update == QueueUpdate::Adopt
        && (queue.tracks.len() != 1
            || !queue
                .current()
                .zip(watch.current.as_ref())
                .is_some_and(|(current, issued)| current.id == issued.id))
    {
        return Ok(false);
    }
    let index = watch
        .current_index
        .ok_or("YouTube Music did not include the requested song in its playback queue")?;
    queue.replace(watch.queue.clone(), index);
    queue.playing_from = watch.source.clone();
    Ok(true)
}

pub(in crate::app) struct PlaybackChannels {
    pub(in crate::app) playback_jobs: async_channel::Sender<PlaybackJob>,
    pub(in crate::app) playback_receiver: async_channel::Receiver<PlaybackJob>,
    pub(in crate::app) playback_generation: Arc<AtomicU64>,
    pub(in crate::app) radio_generation: Arc<AtomicU64>,
}

impl PlaybackChannels {
    pub(in crate::app) fn start(window: &mut Window, cx: &mut Context<Music>) -> Self {
        let (playback_jobs, playback_receiver) = async_channel::bounded::<PlaybackJob>(1);
        let (playback_results, playback_completed) = async_channel::unbounded();
        let playback_generation = Arc::new(AtomicU64::new(0));
        let radio_generation = Arc::new(AtomicU64::new(0));
        let worker_receiver = playback_receiver.clone();
        let worker_generation = playback_generation.clone();
        let worker_radio_generation = radio_generation.clone();
        thread::Builder::new()
            .name("playback-details".into())
            .spawn(move || {
                let catalog = Catalog::new();
                while let Ok(job) = worker_receiver.recv_blocking() {
                    let generation = if matches!(job.query, PlaybackQuery::More(_)) {
                        &worker_radio_generation
                    } else {
                        &worker_generation
                    };
                    if job.id != generation.load(Ordering::Relaxed) {
                        continue;
                    }
                    let catalog = catalog
                        .as_ref()
                        .map(|catalog| catalog.for_session(job.session))
                        .map_err(|error| anyhow::anyhow!("{error:#}"));
                    let result = match job.query {
                        PlaybackQuery::Watch { track, mode } => {
                            let result = catalog
                                .and_then(|catalog| catalog.watch(&track, mode))
                                .map(Box::new);
                            let session_rejected =
                                result.as_ref().is_err_and(auth::session_was_rejected);
                            PlaybackResult::Watch {
                                id: job.id,
                                account_revision: job.account_revision,
                                update: mode.into(),
                                session_rejected,
                                result: result.map_err(|error| format!("{error:#}")),
                            }
                        }
                        PlaybackQuery::Playlist(target) => {
                            let result = catalog
                                .and_then(|catalog| catalog.watch_playlist(&target))
                                .map(Box::new);
                            let session_rejected =
                                result.as_ref().is_err_and(auth::session_was_rejected);
                            PlaybackResult::Watch {
                                id: job.id,
                                account_revision: job.account_revision,
                                update: QueueUpdate::Replace,
                                session_rejected,
                                result: result.map_err(|error| format!("{error:#}")),
                            }
                        }
                        PlaybackQuery::More(continuation) => {
                            let result =
                                catalog.and_then(|catalog| catalog.watch_more(&continuation));
                            PlaybackResult::More {
                                id: job.id,
                                session_rejected: result
                                    .as_ref()
                                    .is_err_and(auth::session_was_rejected),
                                result: result.map_err(|error| format!("{error:#}")),
                            }
                        }
                    };
                    if playback_results.send_blocking(result).is_err() {
                        break;
                    }
                }
            })
            .expect("start playback details worker");
        cx.spawn_in(window, async move |view, cx| {
            while let Ok(result) = playback_completed.recv().await {
                if view
                    .update_in(cx, |view, _, cx| view.playback_result(result, cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        Self {
            playback_jobs,
            playback_receiver,
            playback_generation,
            radio_generation,
        }
    }
}
