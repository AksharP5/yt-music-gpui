//! Account lifecycle, refresh/recovery, remote mutations, and synchronized song state.

use super::*;

pub(in crate::app) enum AccountOperation {
    Restoring(Operation),
    RestoreFailed,
    Refreshing {
        operation: Operation,
        retry_change: bool,
        resume: Option<PlaybackIntent>,
    },
    RefreshFailed {
        resume: Option<PlaybackIntent>,
    },
    Connecting(Operation, BrowserConnection),
    Activating {
        operation: Operation,
        resume: Option<PlaybackIntent>,
    },
    Disconnecting(Operation),
    DisconnectFailed,
}

impl AccountOperation {
    pub(in crate::app) fn pending_playback(&mut self) -> Option<&mut Option<PlaybackIntent>> {
        match self {
            Self::Refreshing { resume, .. }
            | Self::Activating { resume, .. }
            | Self::RefreshFailed { resume } => Some(resume),
            _ => None,
        }
    }
}
pub(in crate::app) enum BrowserConnection {
    Opening { left_app: bool },
    Ready(auth::BrowserProfile),
    Reading(auth::BrowserProfile),
}

impl Music {
    pub(in crate::app) fn clear_account_state(&mut self, cx: &mut Context<Self>) {
        self.finish_listening(false);
        self.next_session_refresh = None;
        self.sync = sync::SyncState::default();
        self.page_cache.clear();
        self.forward_page = None;
        self.catalog_requests.reset_account(
            self.account.as_ref().map(|account| &account.session),
            self.account_revision.wrapping_add(1),
        );
        self.sidebar_playlists.clear();
        self.sidebar_scroll = UniformListScrollHandle::new();
        self.sidebar_error = None;
        self.queue_source.cancel();
        self.queue_waiting = None;
        let session = self
            .account
            .as_ref()
            .map(|connection| connection.session.clone());
        self.search
            .update(cx, |search, cx| search.set_session(session, cx));
        self.ratings.clear();
        self.list_scroll = UniformListScrollHandle::new();
        self.detail_list = DetailList::new();
        self.shelf_scroll.clear();
        self.account_revision = self.account_revision.wrapping_add(1);
        self.track_state.cancel();
        self.track_state_id = None;
        self.history_track = None;
        self.song_text.cancel();
        self.player_page.reset_account();
        self.pending_mutations.clear();
        self.playback_generation.fetch_add(1, Ordering::Relaxed);
        self.watch_pending = None;
        self.reset_radio();
        let _ = self.pending_playback_jobs.try_recv();
        self.now_playing = None;
        self.media_pair = None;
        self.overlay = None;
        self.overlay_subscription = None;
        for collection in &mut self.library.saved {
            collection.details.play = None;
            collection.details.shuffle = None;
            collection.details.mix = None;
        }
        for track in self
            .library
            .queue
            .tracks
            .iter_mut()
            .chain(&mut self.library.recent)
            .chain(&mut self.library.liked)
        {
            track.details.clear_account_state();
        }
    }

    pub(in crate::app) fn finish_account_playback(
        &mut self,
        intent: Option<PlaybackIntent>,
        same_account: bool,
        cx: &mut Context<Self>,
    ) {
        match intent {
            Some(PlaybackIntent::Track(resume))
                if self
                    .library
                    .queue
                    .current()
                    .is_some_and(|track| track.id == resume.video_id) =>
            {
                if same_account && let Some(index) = self.library.queue.index {
                    self.library.queue.tracks[index].details.watch = resume.watch;
                }
                if let Some(mut intent) = resume.advance {
                    intent.playback = intent.playback.with_pause(resume.paused);
                    if same_account && resume.mode == WatchMode::Issued {
                        self.queue_waiting = Some(intent);
                        self.playback = intent.playback;
                        if let Some(track) = self.library.queue.current().cloned() {
                            self.playback_request(
                                PlaybackQuery::Watch {
                                    track: Box::new(track),
                                    mode: WatchMode::Issued,
                                },
                                cx,
                            );
                        }
                    } else {
                        self.finish_account_playback(
                            Some(PlaybackIntent::Advance(intent)),
                            same_account,
                            cx,
                        );
                    }
                } else {
                    self.play_current_at(
                        resume.position,
                        resume.paused,
                        if same_account {
                            resume.mode
                        } else {
                            WatchMode::Details
                        },
                        cx,
                    );
                    if !resume.record_history {
                        self.history_track = self.active_id;
                    }
                }
            }
            Some(PlaybackIntent::Queue {
                query,
                paused,
                advance,
            }) => {
                self.start_queue(query, cx);
                self.playback = self.playback.with_pause(paused);
                if let Some(mut intent) = advance {
                    intent.playback = intent.playback.with_pause(paused);
                    self.advance_queue(intent, cx);
                }
            }
            Some(PlaybackIntent::Advance(intent)) => {
                let at_end = self
                    .library
                    .queue
                    .index
                    .is_none_or(|index| index + 1 >= self.library.queue.tracks.len());
                if at_end
                    && !self.library.queue.incomplete
                    && self.library.repeat == Repeat::Off
                    && self.library.music_autoplay()
                    && self.now_playing.is_none()
                    && let Some(track) = self.library.queue.current().cloned()
                {
                    self.autoplay_waiting = Some(intent);
                    self.playback = Playback::Loading {
                        paused: intent.playback.is_paused(),
                        retried: false,
                        full_lookup: false,
                    };
                    self.playback_request(
                        PlaybackQuery::Watch {
                            track: Box::new(track),
                            mode: WatchMode::Details,
                        },
                        cx,
                    );
                } else {
                    self.advance_queue(intent, cx);
                }
            }
            _ if self.active_id.is_none() => self.playback = Playback::Stopped,
            _ => {}
        }
        self.prefetch_queue(cx);
    }

    pub(in crate::app) fn load_guest_home(&mut self, cx: &mut Context<Self>) {
        if self.account.is_none()
            && self.rows.is_empty()
            && self.route.is_home()
            && !self.catalog_requests.is_loading(CatalogTarget::Page)
            && !matches!(
                self.account_operation,
                Some(AccountOperation::Restoring(_) | AccountOperation::Activating { .. })
            )
        {
            self.request(Query::Home, cx);
        }
    }

    pub(in crate::app) fn refresh_session(
        &mut self,
        retry_change: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        if let Some(AccountOperation::Refreshing {
            retry_change: pending,
            ..
        }) = &mut self.account_operation
        {
            *pending |= retry_change;
            return false;
        }
        if self
            .account_operation
            .as_ref()
            .is_some_and(|operation| !matches!(operation, AccountOperation::RefreshFailed { .. }))
            || self
                .next_session_refresh
                .is_some_and(|deadline| Instant::now() < deadline)
        {
            return false;
        }
        let Some(account) = &self.account else {
            return false;
        };
        let operation = Operation::default();
        let command = account::Command::Refresh {
            operation: operation.clone(),
            expected: account.session.clone(),
        };
        let advance = self.queue_waiting.take().or(self.autoplay_waiting.take());
        let pending_query = self.watch_pending.take();
        if pending_query.is_some() {
            self.playback_generation.fetch_add(1, Ordering::Relaxed);
            self.discard_pending_playback();
        }
        let pending = match pending_query {
            Some(PlaybackQuery::Watch {
                track,
                mode: WatchMode::Issued,
            }) => Some(PlaybackIntent::Track(PlaybackResume {
                video_id: track.id,
                position: self.playback_position(),
                paused: self.playback.is_paused(),
                mode: WatchMode::Issued,
                watch: track.details.watch,
                record_history: false,
                advance,
            })),
            Some(
                query @ (PlaybackQuery::Watch {
                    mode: WatchMode::Radio,
                    ..
                }
                | PlaybackQuery::Playlist(_)),
            ) => Some(PlaybackIntent::Queue {
                query,
                paused: self.playback.is_paused(),
                advance,
            }),
            _ => advance.map(PlaybackIntent::Advance),
        };
        let resume = match &mut self.account_operation {
            Some(AccountOperation::RefreshFailed { resume }) => resume.take(),
            _ => None,
        }
        .or(pending);
        if let Some(intent) = &resume {
            self.playback = Playback::Loading {
                paused: intent.is_paused(),
                retried: false,
                full_lookup: false,
            };
            self.player_command(Player::stop);
            self.active_id = None;
            self.pending_seek = None;
            self.seekable = false;
        }
        self.next_session_refresh = Some(Instant::now() + Duration::from_secs(60));
        match self.account_commands.try_send(command) {
            Ok(()) => {
                self.account_operation = Some(AccountOperation::Refreshing {
                    operation,
                    retry_change,
                    resume,
                });
                self.notice = Some("Reconnecting account…".into());
            }
            Err(_) => {
                self.account_operation = Some(AccountOperation::RefreshFailed { resume });
                self.error =
                    Some("Could not reconnect. Choose Switch account in your profile menu.".into());
            }
        }
        cx.notify();
        true
    }

    pub(in crate::app) fn window_activated(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !window.is_window_active() {
            if let Some(AccountOperation::Connecting(_, BrowserConnection::Opening { left_app })) =
                &mut self.account_operation
            {
                *left_app = true;
            }
            return;
        }
        if matches!(
            self.account_operation,
            Some(AccountOperation::Connecting(_, BrowserConnection::Ready(_)))
        ) {
            self.read_browser_account(cx);
        } else if self.account.is_some()
            && self.account_operation.is_none()
            && sync::needs_refresh(
                self.sync.refreshed_at.max(self.sync.page_fetched_at),
                Instant::now(),
            )
            && !self.catalog_requests.is_loading(CatalogTarget::Page)
        {
            self.refresh_remote(cx);
        }
    }

    pub(in crate::app) fn read_browser_account(&mut self, cx: &mut Context<Self>) {
        let Some(AccountOperation::Connecting(operation, BrowserConnection::Ready(profile))) =
            &self.account_operation
        else {
            return;
        };
        let operation = operation.clone();
        let profile = profile.clone();
        match self
            .account_commands
            .try_send(account::Command::ReadBrowser {
                operation: operation.clone(),
                profile: profile.clone(),
            }) {
            Ok(()) => {
                self.account_operation = Some(AccountOperation::Connecting(
                    operation,
                    BrowserConnection::Reading(profile),
                ));
                self.error = None;
                self.notice = None;
            }
            Err(_) => self.error = Some("Could not check your sign-in. Try again.".into()),
        }
        cx.notify();
    }

    pub(in crate::app) fn connect_account(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if !auth::ACCOUNT_LOGIN_ENABLED {
            self.error = Some("Sign-in is unavailable.".into());
            cx.notify();
            return;
        }
        if matches!(
            self.account_operation,
            Some(AccountOperation::Connecting(_, BrowserConnection::Ready(_)))
        ) {
            self.read_browser_account(cx);
            return;
        }
        if matches!(
            self.account_operation,
            Some(AccountOperation::RestoreFailed | AccountOperation::RefreshFailed { .. })
        ) {
            self.account_operation = None;
        }
        if self.account_operation.is_some() {
            return;
        }
        let operation = Operation::default();
        match self
            .account_commands
            .try_send(account::Command::Connect(operation.clone()))
        {
            Ok(()) => {
                self.account_operation = Some(AccountOperation::Connecting(
                    operation,
                    BrowserConnection::Opening { left_app: false },
                ));
                self.overlay = Some(Overlay::SignIn);
                self.error = None;
                self.notice = None;
            }
            Err(_) => self.error = Some("Could not start sign-in. Try again.".into()),
        }
        cx.notify();
    }

    pub(in crate::app) fn cancel_connection(&mut self, cx: &mut Context<Self>) {
        let refreshing = matches!(
            self.account_operation,
            Some(AccountOperation::Refreshing { .. })
        );
        let Some(
            AccountOperation::Connecting(operation, _)
            | AccountOperation::Restoring(operation)
            | AccountOperation::Refreshing { operation, .. },
        ) = self.account_operation.as_ref()
        else {
            return;
        };
        operation.cancel();
        self.catalog_requests.discard_restore();
        if let Ok(mut session) = self.session.write() {
            *session = self.account.as_ref().map(|account| account.session.clone());
        }
        self.account_operation =
            refreshing.then_some(AccountOperation::RefreshFailed { resume: None });
        self.notice = None;
        self.error = None;
        self.load_guest_home(cx);
        cx.notify();
    }

    pub(in crate::app) fn mutate(&mut self, mutation: Mutation, cx: &mut Context<Self>) -> bool {
        if self.account_operation.is_some() {
            self.error = Some("Finish the account change before saving edits.".into());
            cx.notify();
            return false;
        }
        let Some(connection) = &self.account else {
            return false;
        };
        let key = mutation.key();
        if self.pending_mutations.contains(&key) {
            return false;
        }
        let optimistic = if let Mutation::RateSong { track, rating } = &mutation {
            Some((
                track.clone(),
                *rating,
                self.ratings
                    .get(&track.id)
                    .copied()
                    .or(track.details.rating),
            ))
        } else {
            None
        };
        let submitted = match self.account_commands.try_send(account::Command::Apply {
            session: connection.session.clone(),
            mutation: Box::new(mutation),
        }) {
            Ok(()) => {
                self.pending_mutations.insert(key);
                self.notice = Some("Saving to YouTube Music…".into());
                true
            }
            Err(_) => {
                self.error = Some("Too many pending changes. Wait for them to finish.".into());
                false
            }
        };
        if submitted && let Some((track, rating, previous)) = optimistic {
            self.sync
                .optimistic_ratings
                .insert(track.id.clone(), previous);
            self.sync.rating_overrides.insert(track.id.clone());
            self.set_rating(&track.id, Some(rating));
            if rating == Rating::Dislike
                && self
                    .library
                    .queue
                    .current()
                    .is_some_and(|current| current.id == track.id)
            {
                self.next(false, cx);
            }
        }
        cx.notify();
        submitted
    }

    pub(in crate::app) fn disconnect_account(&mut self, cx: &mut Context<Self>) {
        if matches!(
            self.account_operation,
            Some(AccountOperation::Disconnecting(_))
        ) {
            return;
        }
        if let Some(
            AccountOperation::Connecting(operation, _)
            | AccountOperation::Restoring(operation)
            | AccountOperation::Refreshing { operation, .. }
            | AccountOperation::Activating { operation, .. },
        ) = self.account_operation.take()
        {
            operation.cancel();
        }
        if let Ok(mut session) = self.session.write() {
            *session = None;
        }
        self.account = None;
        self.clear_account_state(cx);
        self.notice = Some("Signing out…".into());
        self.error = None;
        let playback = self
            .player
            .as_ref()
            .map(Player::clear_credentials)
            .transpose()
            .map_err(|error| format!("{error:#}"));
        self.active_id = None;
        self.playback = Playback::Stopped;
        self.navigate(Route::Home, cx);
        self.back.clear();
        self.list_scroll = UniformListScrollHandle::new();
        self.detail_list = DetailList::new();
        self.shelf_scroll.clear();
        let operation = Operation::default();
        if self
            .account_commands
            // Sign-out supersedes a queued edit if the bounded queue is full.
            .force_send(account::Command::Disconnect {
                operation: operation.clone(),
                playback,
            })
            .is_err()
        {
            self.account_operation = Some(AccountOperation::DisconnectFailed);
            self.notice = None;
            self.error = Some("Account access stopped. The account worker could not confirm sign-out cleanup. Restart the app and try again.".into());
        } else {
            self.account_operation = Some(AccountOperation::Disconnecting(operation));
        }
        cx.notify();
    }

    pub(in crate::app) fn refresh_remote(&mut self, cx: &mut Context<Self>) {
        self.sync
            .rating_overrides
            .retain(|id| self.sync.optimistic_ratings.contains_key(id));
        self.sync.refreshed_at = Some(Instant::now());
        self.revalidate_page(cx);
        self.request(Query::Revalidate(Box::new(Query::SidebarPlaylists)), cx);
        self.refresh_current_account(cx);
    }

    pub(in crate::app) fn refresh_current_account(&mut self, cx: &mut Context<Self>) {
        let Some(account) = &self.account else { return };
        let Some(track) = self.library.queue.current().cloned() else {
            return;
        };
        match self.track_state.load(track, account.session.clone()) {
            Ok(id) => self.track_state_id = Some(id),
            Err(error) => {
                self.track_state_id = None;
                self.error = Some(error.to_string());
                cx.notify();
            }
        }
    }

    pub(in crate::app) fn account_event(
        &mut self,
        event: account::Event,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            account::Event::RestoreLoaded { operation, session } => {
                if !operation.is_cancelled()
                    && matches!(&self.account_operation, Some(AccountOperation::Restoring(active)) if active.matches(&operation))
                {
                    self.catalog_requests.restore(
                        session,
                        self.account_revision.wrapping_add(1),
                        self.account_generation,
                        self.history_generation,
                    );
                }
            }
            account::Event::BrowserReady { operation, result } => {
                let returned = match &self.account_operation {
                    Some(AccountOperation::Connecting(
                        active,
                        BrowserConnection::Opening { left_app },
                    )) if active.matches(&operation) => *left_app && window.is_window_active(),
                    _ => return,
                };
                self.notice = None;
                match result {
                    Ok(profile) => {
                        self.account_operation = Some(AccountOperation::Connecting(
                            operation,
                            BrowserConnection::Ready(profile),
                        ));
                        if returned {
                            self.read_browser_account(cx);
                        }
                    }
                    Err(error) => {
                        self.account_operation = None;
                        self.error = Some(error);
                    }
                }
            }
            account::Event::AccountPreview { operation, result } => {
                let Some(AccountOperation::Connecting(active, BrowserConnection::Reading(profile))) =
                    &self.account_operation
                else {
                    return;
                };
                if !active.matches(&operation) {
                    return;
                }
                let profile = profile.clone();
                self.notice = None;
                match result {
                    Ok(connection) => {
                        self.error = None;
                        self.overlay = None;
                        self.account_event(
                            account::Event::Connected {
                                operation,
                                result: Ok(Some(connection)),
                                save_session: true,
                            },
                            window,
                            cx,
                        );
                    }
                    Err(error) => {
                        self.error = Some(error);
                        self.account_operation = Some(AccountOperation::Connecting(
                            operation,
                            BrowserConnection::Ready(profile),
                        ));
                    }
                }
            }
            account::Event::Disconnected { operation, result } => {
                if !matches!(&self.account_operation, Some(AccountOperation::Disconnecting(active)) if active.matches(&operation))
                {
                    return;
                }
                self.account_operation = None;
                self.notice = None;
                match result {
                    Ok(()) => {
                        self.notice = Some("Signed out of YouTube Music".into());
                    }
                    Err(error) => {
                        self.account_operation = Some(AccountOperation::DisconnectFailed);
                        self.error = Some(error);
                    }
                }
            }
            account::Event::Connected {
                operation,
                result,
                save_session,
            } => {
                let (restoring, renewing, retry_change, pending) = match &mut self.account_operation
                {
                    Some(AccountOperation::Restoring(active)) if active.matches(&operation) => {
                        (true, false, false, None)
                    }
                    Some(AccountOperation::Refreshing {
                        operation: active,
                        retry_change,
                        resume,
                    }) if active.matches(&operation) => (false, true, *retry_change, resume.take()),
                    Some(AccountOperation::Connecting(active, BrowserConnection::Reading(_)))
                        if active.matches(&operation) =>
                    {
                        (false, false, false, None)
                    }
                    _ => return,
                };
                if !matches!(&result, Ok(Some(connection)) if self.catalog_requests.restoring_session_is(&connection.session))
                {
                    self.catalog_requests.discard_restore();
                }
                let pending = pending.or_else(|| {
                    renewing
                        .then(|| self.queue_waiting.take().or(self.autoplay_waiting.take()))
                        .flatten()
                        .map(PlaybackIntent::Advance)
                });
                match result {
                    Ok(Some(connection)) => {
                        if renewing
                            && self.account.as_ref().is_some_and(|account| {
                                Arc::ptr_eq(&account.session, &connection.session)
                            })
                        {
                            self.account_operation = None;
                            self.notice = None;
                            self.error = retry_change.then(|| {
                                "Your account is connected. Retry the failed change.".into()
                            });
                            if let Err(error) = self
                                .queue_source
                                .renew_session(&connection.session, connection.session.clone())
                            {
                                self.queue_source.cancel();
                                self.error = Some(error);
                            }
                            self.refresh_remote(cx);
                            self.finish_account_playback(pending, true, cx);
                            if self.active_id.is_some()
                                && self.watch_pending.is_none()
                                && self.now_playing.is_none()
                                && let Some(track) = self.library.queue.current().cloned()
                            {
                                self.playback_request(
                                    PlaybackQuery::Watch {
                                        track: Box::new(track),
                                        mode: WatchMode::Details,
                                    },
                                    cx,
                                );
                            }
                            cx.notify();
                            return;
                        }
                        let installation =
                            connection
                                .session
                                .playback_credentials()
                                .and_then(|credentials| {
                                    self.player
                                        .as_ref()
                                        .ok_or_else(|| {
                                            anyhow::anyhow!(
                                                "The audio player is unavailable. Restart the app."
                                            )
                                        })?
                                        .set_credentials(credentials)
                                });
                        let response = match installation {
                            Ok(response) => response,
                            Err(error) => {
                                self.catalog_requests.discard_restore();
                                self.account_operation = if renewing {
                                    Some(AccountOperation::RefreshFailed { resume: pending })
                                } else {
                                    restoring.then_some(AccountOperation::RestoreFailed)
                                };
                                self.notice = None;
                                self.error = Some(format!(
                                    "Could not activate playback credentials: {error:#}"
                                ));
                                self.load_guest_home(cx);
                                cx.notify();
                                return;
                            }
                        };
                        let resume = pending.or_else(|| {
                            if renewing
                                && self.active_id.is_some()
                                && self.playback != Playback::Stopped
                            {
                                self.library.queue.current().map(|track| {
                                    PlaybackIntent::Track(PlaybackResume {
                                        video_id: track.id.clone(),
                                        position: self.playback_position(),
                                        paused: self.playback.is_paused(),
                                        mode: WatchMode::Details,
                                        watch: track.details.watch.clone(),
                                        record_history: false,
                                        advance: None,
                                    })
                                })
                            } else {
                                None
                            }
                        });
                        self.playback =
                            resume
                                .as_ref()
                                .map_or(Playback::Stopped, |resume| Playback::Loading {
                                    paused: resume.is_paused(),
                                    retried: false,
                                    full_lookup: false,
                                });
                        self.account_operation = Some(AccountOperation::Activating {
                            operation: operation.clone(),
                            resume,
                        });
                        self.notice = Some("Preparing account playback…".into());
                        self.playback_generation.fetch_add(1, Ordering::Relaxed);
                        self.watch_pending = None;
                        self.discard_pending_playback();
                        self.reset_radio();
                        self.queue_waiting = None;
                        self.active_id = None;
                        self.pending_seek = None;
                        self.seekable = false;
                        cx.spawn_in(window, async move |view, cx| {
                            let result = response.recv().await.unwrap_or_else(|_| Err("The player stopped before confirming the account. Restart the app.".into()));
                            let _ = view.update_in(cx, |view, window, cx| {
                                if !matches!(&view.account_operation, Some(AccountOperation::Activating { operation: active, .. }) if active.matches(&operation)) { return; }
                                let resume = match view.account_operation.take() {
                                    Some(AccountOperation::Activating { resume, .. }) => resume,
                                    _ => unreachable!("activation checked above"),
                                };
                                view.notice = None;
                                let previous_session = view.account.as_ref().map(|account| account.session.clone());
                                match connection.activate(&view.session, &operation, result) {
                                    Ok(connection) => {
                                        let session = connection.session.clone();
                                        let label = format!("Connected as {}", connection.account.name);
                                        view.account = Some(connection);
                                        if renewing {
                                            view.account_generation = view.account_generation.wrapping_add(1);
                                            // Read continuations belong to the exact session snapshot.
                                            view.page_cache.clear();
                                            view.forward_page = None;
                                            // Rebind only this verified account's queue; discard other in-flight reads.
                                            view.catalog_requests.cancel(CatalogTarget::Page);
                                            view.catalog_requests.cancel(CatalogTarget::Sidebar);
                                            view.catalog_requests.cancel(CatalogTarget::PlaylistPicker);
                                            if let Some(expected) = &previous_session
                                                && let Err(error) = view.queue_source.renew_session(expected, session.clone())
                                            { view.queue_source.cancel(); view.error = Some(error); }
                                            view.track_state.cancel();
                                            view.track_state_id = None;
                                            view.song_text.cancel();
                                            view.player_page.reset_account();
                                            view.now_playing = None;
                                            view.media_pair = None;
                                            for (id, rating) in std::mem::take(&mut view.sync.optimistic_ratings) { view.set_rating(&id, rating); }
                                            view.sync.rating_overrides.clear();
                                            view.sync.page_changes.clear();
                                            view.ratings.clear();
                                            view.pending_mutations.clear();
                                            for track in &mut view.library.queue.tracks { track.details.clear_account_state(); }
                                            if matches!(view.overlay, Some(Overlay::Track { .. } | Overlay::PlaylistPicker { .. } | Overlay::SongText { .. })) {
                                                view.overlay = None;
                                            }
                                            view.search.update(cx, |search, cx| search.set_session(Some(session.clone()), cx));
                                            view.error = retry_change.then(|| "Your account reconnected. Retry the failed change.".into());
                                            view.refresh_remote(cx);
                                        } else {
                                            view.clear_account_state(cx);
                                            window.focus(&view.focus, cx);
                                            view.navigate(Route::Home, cx);
                                            view.request(Query::SidebarPlaylists, cx);
                                            view.back.clear();
                                            view.notice = (!restoring).then_some(label);
                                            view.refresh_current_account(cx);
                                        }
                                        view.finish_account_playback(resume, renewing, cx);
                                        if retry_change { view.error = Some("Your account is connected. Retry the failed change.".into()); }
                                        if save_session && view.account_commands.try_send(account::Command::Persist(session)).is_err() {
                                            view.error = Some("Connected for this session, but the account could not be queued for keyring storage.".into());
                                        }
                                    }
                                    Err(error) => {
                                        view.catalog_requests.discard_restore();
                                        view.account_operation = if renewing { Some(AccountOperation::RefreshFailed { resume }) }
                                            else { restoring.then_some(AccountOperation::RestoreFailed) };
                                        view.playback = Playback::Stopped;
                                        view.error = Some(format!("Could not activate the account: {error:#}"));
                                    }
                                }
                                view.load_guest_home(cx);
                                cx.notify();
                            });
                        }).detach();
                    }
                    Ok(None) => {
                        self.account_operation = None;
                        self.notice = None;
                    }
                    Err(error) => {
                        self.account_operation = if renewing {
                            Some(AccountOperation::RefreshFailed { resume: pending })
                        } else {
                            restoring.then_some(AccountOperation::RestoreFailed)
                        };
                        self.notice = None;
                        self.error = Some(error);
                    }
                }
            }
            account::Event::Applied {
                session,
                mutation,
                result,
                session_rejected,
            } => {
                if !self
                    .account
                    .as_ref()
                    .is_some_and(|connection| Arc::ptr_eq(&connection.session, &session))
                {
                    return;
                }
                if session_rejected {
                    self.refresh_session(true, cx);
                }
                self.account_generation = self.account_generation.wrapping_add(1);
                let mutation = *mutation;
                self.pending_mutations.remove(&mutation.key());
                let rollback = if let Mutation::RateSong { track, .. } = &mutation {
                    self.sync
                        .optimistic_ratings
                        .remove(&track.id)
                        .map(|rating| (track.id.clone(), rating))
                } else {
                    None
                };
                match result {
                    Ok(effect) => {
                        let deleting = matches!(mutation, Mutation::DeletePlaylist { .. });
                        acknowledge_song_change(
                            self.library.queue.tracks.iter_mut(),
                            &mut self.ratings,
                            &mutation,
                        );
                        self.notice = Some("Saved to YouTube Music".into());
                        self.patch_confirmed(&mutation, cx);
                        match effect {
                            Effect::CreatedPlaylist { id } => {
                                self.request(
                                    Query::Revalidate(Box::new(Query::SidebarPlaylists)),
                                    cx,
                                );
                                if self.overlay.is_none() {
                                    self.navigate(
                                        Route::Browse(BrowseLink::new(
                                            format!("VL{id}"),
                                            "Playlist",
                                        )),
                                        cx,
                                    );
                                }
                            }
                            Effect::AlreadyAdded => {
                                if let Mutation::AddToPlaylist { id, video_ids, .. } = mutation {
                                    self.notice = None;
                                    self.overlay = Some(Overlay::AlreadyAdded { id, video_ids });
                                }
                            }
                            Effect::PlaylistAdded { additions } => {
                                if let Mutation::AddToPlaylist { id, .. } = &mutation {
                                    self.patch_added(id, &additions, cx);
                                }
                            }
                            Effect::Changed => {
                                if deleting {
                                    self.request(
                                        Query::Revalidate(Box::new(Query::SidebarPlaylists)),
                                        cx,
                                    );
                                    self.navigate(Route::Library(Section::Playlists), cx);
                                }
                            }
                        }
                    }
                    Err(error) => {
                        if let Some((id, rating)) = rollback {
                            self.set_rating(&id, rating);
                        }
                        self.notice = None;
                        self.error = Some(error);
                    }
                }
            }
            account::Event::Persisted { session, result } => {
                if self
                    .account
                    .as_ref()
                    .is_some_and(|connection| Arc::ptr_eq(&connection.session, &session))
                    && let Err(error) = result
                {
                    self.error = Some(format!(
                        "Connected for this session, but the account was not saved: {error}"
                    ));
                }
            }
            account::Event::PlaybackReported {
                session,
                video_id,
                result,
                session_rejected,
                watchtime,
                resume_seconds,
            } => {
                if !self
                    .account
                    .as_ref()
                    .is_some_and(|connection| Arc::ptr_eq(&connection.session, &session))
                {
                    return;
                }
                if result.is_ok()
                    && let Some(seconds) = resume_seconds
                    && self
                        .library
                        .queue
                        .current()
                        .is_some_and(|track| track.id == video_id)
                    && let Some(index) = self.library.queue.index
                {
                    self.library.queue.tracks[index].details.resume_seconds = Some(seconds);
                    self.resume_account_position(cx);
                }
                if session_rejected {
                    self.refresh_session(true, cx);
                }
                match result {
                    Ok(()) => {
                        self.history_generation = self.history_generation.wrapping_add(1);
                        if !watchtime
                            && self.overlay.is_none()
                            && !self.catalog_requests.is_loading(CatalogTarget::Page)
                            && matches!(
                                self.route,
                                Route::Recent | Route::Library(Section::History)
                            )
                        {
                            self.revalidate_page(cx);
                        }
                    }
                    Err(error) => {
                        let title = self
                            .library
                            .queue
                            .tracks
                            .iter()
                            .find(|track| track.id == video_id)
                            .map_or("this song", |track| track.title.as_str());
                        self.error = Some(format!(
                            "Could not save {title} to YouTube Music history: {error}"
                        ));
                    }
                }
            }
        }
        self.load_guest_home(cx);
        cx.notify();
    }
}

pub(in crate::app) fn apply_track_details(
    tracks: &mut [Track],
    ratings: &mut HashMap<String, Rating>,
    source: &Track,
    fresh: bool,
) {
    let mut matched = false;
    for track in tracks.iter_mut().filter(|track| track.id == source.id) {
        matched = true;
        let mut details = source.details.clone();
        details.set_video_id = track.details.set_video_id.clone();
        details.credits_id = details
            .credits_id
            .or_else(|| track.details.credits_id.clone());
        if !fresh {
            details.rating = track.details.rating;
            details.in_library = track.details.in_library;
            details.add_library = track.details.add_library.clone();
            details.remove_library = track.details.remove_library.clone();
            details.remove_history = track.details.remove_history.clone();
        }
        details.watch = track.details.watch.clone().or(details.watch);
        track.details = details;
        if track.details.episode.is_some() {
            track.artist = source.artist.clone();
        }
    }
    if matched && fresh {
        if let Some(rating) = source.details.rating {
            ratings.insert(source.id.clone(), rating);
        } else {
            ratings.remove(&source.id);
        }
    }
}

pub(in crate::app) fn acknowledge_song_change<'a>(
    tracks: impl IntoIterator<Item = &'a mut Track>,
    ratings: &mut HashMap<String, Rating>,
    mutation: &Mutation,
) {
    match mutation {
        Mutation::RateSong { track, rating } => {
            ratings.insert(track.id.clone(), *rating);
            for current in tracks.into_iter().filter(|current| current.id == track.id) {
                current.details.rating = Some(*rating);
            }
        }
        Mutation::SongLibrary { track, save } => {
            for current in tracks.into_iter().filter(|current| current.id == track.id) {
                current.details.in_library = Some(*save);
                current.details.add_library = track.details.add_library.clone();
                current.details.remove_library = track.details.remove_library.clone();
            }
        }
        _ => {}
    }
}
