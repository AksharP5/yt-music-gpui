//! Constructs the application, connects worker events, and shuts down account work.

use super::*;

impl Music {
    pub fn new(startup: Startup, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| SearchBox::new(window, cx));
        let (store, library, error) = match Store::load() {
            Ok((s, l)) => (Some(s), l, None),
            Err(e) => (None, Library::new(), Some(format!("{e:#}"))),
        };
        let store = store.map(|store| {
            let (writer, errors) = LibraryWriter::new(store);
            cx.spawn_in(window, async move |view, cx| {
                while let Ok(error) = errors.recv().await {
                    if view
                        .update_in(cx, |view, _, cx| {
                            view.error = Some(error);
                            cx.notify();
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .detach();
            writer
        });
        let session = auth::shared(None);
        let (account_commands, account_events, account_worker) = account::start(session.clone());
        let account_operation =
            (auth::ACCOUNT_LOGIN_ENABLED && startup != Startup::Guest).then(|| {
                let operation = Operation::default();
                let command = if startup == Startup::Connect {
                    account::Command::Connect(operation.clone())
                } else {
                    account::Command::Restore(operation.clone())
                };
                let _ = account_commands.try_send(command);
                if startup == Startup::Connect {
                    AccountOperation::Connecting(
                        operation,
                        BrowserConnection::Opening { left_app: false },
                    )
                } else {
                    AccountOperation::Restoring(operation)
                }
            });
        cx.spawn_in(window, async move |view, cx| {
            while let Ok(event) = account_events.recv().await {
                if view
                    .update_in(cx, |view, window, cx| view.account_event(event, window, cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        let (player, mut error) = match Player::new(PlayerOptions {
            credentials: None,
            volume: library.volume,
            speed: library.speed,
            ..Default::default()
        }) {
            Ok(p) => (Some(p), error),
            Err(e) => (None, Some(format!("{e:#}"))),
        };
        let video = player
            .as_ref()
            .map(|player| player.video_attachment(window, cx));
        let (artwork, loaded) = Artwork::new();
        let catalog_requests = CatalogRequests::start(window, cx);
        let PlaybackChannels {
            playback_jobs,
            playback_receiver,
            playback_generation,
            radio_generation,
        } = PlaybackChannels::start(window, cx);
        let (queue_source, queue_events) = queue_source::Worker::start();
        cx.spawn_in(window, async move |view, cx| {
            while let Ok(event) = queue_events.recv().await {
                if view
                    .update_in(cx, |view, _, cx| view.queue_source_result(event, cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        let (track_state, track_events) = track_state::Worker::start();
        cx.spawn_in(window, async move |view, cx| {
            while let Ok(event) = track_events.recv().await {
                if view
                    .update_in(cx, |view, _, cx| {
                        if view.track_state_id != Some(event.id) {
                            return;
                        }
                        view.track_state_id = None;
                        match event.result {
                            Ok(track)
                                if view
                                    .library
                                    .queue
                                    .current()
                                    .is_some_and(|current| current.id == track.id) =>
                            {
                                apply_track_details(
                                    &mut view.library.queue.tracks,
                                    &mut view.ratings,
                                    &track,
                                    !view.sync.rating_overrides.contains(&track.id),
                                );
                                view.overlay_ratings();
                            }
                            Ok(_) => {}
                            Err(error) => {
                                if event.session_rejected {
                                    view.refresh_session(false, cx);
                                }
                                view.error =
                                    Some(format!("Could not refresh the current song: {error}"))
                            }
                        }
                        view.notify_playback(cx);
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        let (song_text, text_events) = song_text::Worker::start();
        cx.spawn_in(window, async move |view, cx| {
            while let Ok(event) = text_events.recv().await {
                if view
                    .update_in(cx, |view, _, cx| {
                        if matches!(&view.overlay, Some(Overlay::SongText { id, .. }) if *id == event.id) {
                            if event.session_rejected { view.refresh_session(false, cx); }
                            if let Some(Overlay::SongText { content, .. }) = &mut view.overlay {
                                *content = Some(event.result);
                            }
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        cx.spawn_in(window, async move |view, cx| {
            while let Ok(loaded) = loaded.recv().await {
                if view
                    .update_in(cx, |view, window, cx| {
                        for old in view.artwork.insert(loaded) {
                            cx.drop_image(old, Some(window));
                        }
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        if let Some(player) = &player {
            let events = player.events();
            cx.spawn_in(window, async move |view, cx| {
                while let Ok(event) = events.recv().await {
                    if view
                        .update_in(cx, |view, _, cx| view.player_event(event, cx))
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .detach();
        }
        let mpris = match mpris::Mpris::new() {
            Ok(service) => Some(service),
            Err(failure) => {
                error.get_or_insert_with(|| failure.to_string());
                None
            }
        };
        if let Some(service) = &mpris {
            let events = service.events();
            cx.spawn_in(window, async move |view, cx| {
                while let Ok(event) = events.recv().await {
                    if view
                        .update_in(cx, |view, _, cx| view.desktop_event(event, cx))
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .detach();
        }
        let subscription =
            cx.subscribe_in(&search, window, |view, _, event, window, cx| match event {
                SearchBoxEvent::Submit(query) => {
                    if !query.is_empty() {
                        let search = match &view.route {
                            Route::Search(search) => Search {
                                query: query.clone(),
                                ..search.clone()
                            },
                            _ => Search {
                                query: query.clone(),
                                filter: SearchFilter::All,
                                scope: SearchScope::Catalog,
                            },
                        };
                        view.navigate(Route::Search(search), cx);
                        window.focus(&view.focus, cx);
                    }
                }
                SearchBoxEvent::Open(item) => {
                    window.focus(&view.focus, cx);
                    match item.as_ref().clone() {
                        Item::Track(track) => view.play_track(track, cx),
                        Item::Collection(collection) => view.navigate(
                            Route::Browse(BrowseLink::new(collection.id, collection.title)),
                            cx,
                        ),
                        Item::Navigation(link) => view.navigate(Route::Browse(link), cx),
                    }
                }
                SearchBoxEvent::Blur => window.focus(&view.focus, cx),
            });
        let activation = cx.observe_window_activation(window, Self::window_activated);
        let focus_lost = cx.on_focus_lost(window, Self::restore_keyboard_focus);
        let music = cx.entity();
        let weak = music.downgrade();
        search.update(cx, |search, _| {
            search.set_artwork(Box::new(move |url, cx| {
                weak.update(cx, |music, _| music.artwork.get(url))
                    .ok()
                    .flatten()
            }))
        });
        let content_view = cx.new(|cx| MusicContent::new(music.clone(), cx));
        let player_bar = cx.new(|cx| PlayerBar::new(music.downgrade(), cx));
        let mut view = Self {
            #[cfg(test)]
            renders: 0,
            #[cfg(test)]
            focus_restores: 0,
            content_view,
            player_bar,
            theme: Theme::load(),
            session,
            account: None,
            account_commands,
            account_worker: Some(account_worker),
            account_operation,
            next_session_refresh: None,
            pending_mutations: HashSet::new(),
            ratings: HashMap::new(),
            account_revision: 0,
            account_generation: 0,
            history_generation: 0,
            sync: sync::SyncState::default(),
            track_state,
            track_state_id: None,
            focus: cx.focus_handle(),
            sidebar_focus: VirtualListFocus::default(),
            queue_focus: VirtualListFocus::default(),
            search,
            route: Route::Home,
            page: Page {
                title: "Home".into(),
                header: None,
                shelves: Vec::new(),
                continuation: None,
                editable: false,
                description: String::new(),
                privacy: None,
                chips: Vec::new(),
                countries: Vec::new(),
                search_filters: Vec::new(),
                library: None,
            },
            rows: Vec::new(),
            list_rows: Vec::new(),
            shelf_scroll: HashMap::new(),
            list_scroll: UniformListScrollHandle::new(),
            detail_list: DetailList::new(),
            sidebar_collapsed: None,
            sidebar_playlists: Vec::new(),
            sidebar_error: None,
            sidebar_scroll: UniformListScrollHandle::new(),
            back: Vec::new(),
            page_cache: VecDeque::new(),
            forward_page: Some(ForwardPage::new(0, 0, 0)),
            list_id: 0,
            error,
            notice: None,
            catalog_requests,
            playback_jobs,
            pending_playback_jobs: playback_receiver,
            playback_generation,
            now_playing: None,
            media_pair: None,
            radio: None,
            radio_generation,
            radio_loading: false,
            autoplay_seeded: false,
            autoplay_waiting: None,
            queue_source,
            queue_waiting: None,
            autoplay_tracks: VecDeque::new(),
            watch_pending: None,
            song_text,
            artwork,
            library,
            store,
            player,
            mpris,
            seekable: false,
            video,
            player_open: false,
            sort_menu: false,
            sort_focus: cx.focus_handle().tab_index(0).tab_stop(true),
            video_fullscreen: false,
            video_transparent: false,
            video_error: None,
            video_click: None,
            active_id: None,
            history_track: None,
            listening: playback::listening::ListeningState::default(),
            playback: Playback::Stopped,
            position: 0.,
            pending_seek: None,
            duration: 0.,
            player_page: PlayerPage::start(window, cx),
            overlay: (startup == Startup::Connect).then_some(Overlay::SignIn),
            settings_section: SettingsSection::General,
            settings_menu: None,
            overlay_anchor: None,
            overlay_button: None,
            overlay_focus: cx.focus_handle(),
            settings_focus: cx.focus_handle(),
            overlay_return_focus: None,
            overlay_subscription: None,
            drag: None,
            seek_bounds: Rc::new(Cell::new(Bounds::default())),
            volume_bounds: Rc::new(Cell::new(Bounds::default())),
            drag_position: None,
            _subscriptions: vec![subscription, activation, focus_lost],
        };
        view.notify_playback(cx);
        window.focus(&view.focus, cx);
        if startup != Startup::Restore || !auth::ACCOUNT_LOGIN_ENABLED {
            view.request(Query::Home, cx);
        }
        view
    }
}

impl Drop for Music {
    fn drop(&mut self) {
        self.finish_listening(false);
        if let Some(
            AccountOperation::Connecting(operation, _)
            | AccountOperation::Restoring(operation)
            | AccountOperation::Refreshing { operation, .. }
            | AccountOperation::Activating { operation, .. },
        ) = &self.account_operation
        {
            operation.cancel();
        }
        if let Some(worker) = self.account_worker.take()
            && let Err(error) = worker.finish()
        {
            eprintln!("Could not finish account cleanup: {error:#}");
        }
    }
}
