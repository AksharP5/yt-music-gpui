use crate::{
    account::{self, Connection, Effect, Mutation, Privacy, Section},
    artwork::Artwork,
    auth::{self, Operation, Session, SharedSession},
    catalog::{BrowseLink, Catalog, Collection, Continuation, Item, Page, Rating, Shelf, Track},
    country_picker::{CountryPicker, CountryPickerEvent},
    library::{Library, LibraryWriter, Repeat, Store},
    player::{AudioQuality, PlayOptions, PlaybackSpeed, Player, PlayerEvent, PlayerOptions},
    playlist_form::{PlaylistForm, PlaylistFormEvent},
    search::{SearchFilter, SearchScope},
    search_box::{SearchBox, SearchBoxEvent},
    song_text::{self, Content as SongText, Query as TextQuery},
    theme::Theme,
    watch::{Watch, WatchContinuation, WatchPage},
};
use gpui::{prelude::*, *};
use std::{
    cell::Cell,
    collections::{HashMap, HashSet, VecDeque},
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::Duration,
};

#[derive(Clone, PartialEq)]
struct Search {
    query: String,
    filter: SearchFilter,
    scope: SearchScope,
}

#[derive(Clone, PartialEq)]
enum Route {
    Home,
    Explore,
    Search(Search),
    Browse(BrowseLink),
    Liked,
    Recent,
    Saved,
    Library(Section),
}
impl Route {
    fn is_home(&self) -> bool {
        matches!(self, Self::Home)
            || matches!(self, Self::Browse(link) if link.id == "FEmusic_home")
    }
}
#[derive(Clone)]
enum Query {
    Home,
    Explore,
    Search(Search),
    Browse(BrowseLink),
    More {
        token: Continuation,
        search: Option<Search>,
    },
    Library(Section),
    PlaylistChoices,
    PlaylistChoicesMore(Continuation),
}
struct Job {
    id: u64,
    query: Query,
    session: Option<Arc<Session>>,
}
struct CatalogResult {
    id: u64,
    append: bool,
    result: Result<Page, String>,
    target: CatalogTarget,
}
enum CatalogTarget {
    Page,
    PlaylistPicker,
}
enum PlaybackQuery {
    Watch { track: Box<Track>, radio: bool },
    More(WatchContinuation),
}
struct PlaybackJob {
    id: u64,
    query: PlaybackQuery,
    session: Option<Arc<Session>>,
}
enum PlaybackResult {
    Watch {
        id: u64,
        radio: bool,
        result: Result<Box<Watch>, String>,
    },
    More {
        id: u64,
        result: Result<WatchPage, String>,
    },
}
#[derive(Clone, Copy, PartialEq)]
enum Playback {
    Stopped,
    Loading { paused: bool, retried: bool },
    Playing,
    Paused,
    Buffering,
}
#[derive(Clone, Copy)]
struct SeekIntent {
    track_id: u64,
    revision: u64,
    seconds: f64,
}
impl Playback {
    fn is_paused(self) -> bool {
        matches!(self, Self::Paused | Self::Loading { paused: true, .. })
    }

    fn with_pause(self, paused: bool) -> Self {
        match self {
            Self::Loading { retried, .. } => Self::Loading { paused, retried },
            _ if paused => Self::Paused,
            _ => Self::Playing,
        }
    }

    fn retry(&mut self) -> bool {
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
#[derive(Clone)]
enum Overlay {
    Help,
    Settings,
    CountryPicker(Entity<CountryPicker>),
    Description {
        title: String,
        text: String,
    },
    Chapters(Box<Track>),
    Track {
        track: Track,
        index: Option<usize>,
    },
    PlaylistPicker {
        track: Track,
        page: Option<Box<Page>>,
        duplicates: bool,
    },
    PlaylistForm {
        form: Entity<PlaylistForm>,
        track: Option<Track>,
    },
    DeletePlaylist {
        id: String,
        title: String,
    },
    SongText {
        id: u64,
        title: String,
        content: Option<Result<SongText, String>>,
    },
}
#[derive(Clone, Copy, PartialEq)]
enum Drag {
    Seek,
    Volume,
}
enum AccountOperation {
    Connecting(Operation),
    Disconnecting(Operation),
}
impl AccountOperation {
    fn token(&self) -> &Operation {
        match self {
            Self::Connecting(token) | Self::Disconnecting(token) => token,
        }
    }
}

pub struct Music {
    theme: Theme,
    session: SharedSession,
    account: Option<Connection>,
    account_commands: async_channel::Sender<account::Command>,
    account_worker: Option<account::Worker>,
    account_operation: Option<AccountOperation>,
    pending_mutations: HashSet<String>,
    ratings: HashMap<String, Rating>,
    focus: FocusHandle,
    search: Entity<SearchBox>,
    route: Route,
    page: Page,
    rows: Vec<Item>,
    back: Vec<(Route, Option<Page>)>,
    list_id: usize,
    loading: bool,
    error: Option<String>,
    notice: Option<String>,
    generation: Arc<AtomicU64>,
    jobs: async_channel::Sender<Job>,
    pending_jobs: async_channel::Receiver<Job>,
    playback_jobs: async_channel::Sender<PlaybackJob>,
    pending_playback_jobs: async_channel::Receiver<PlaybackJob>,
    playback_generation: Arc<AtomicU64>,
    now_playing: Option<Watch>,
    radio: Option<WatchContinuation>,
    radio_generation: Arc<AtomicU64>,
    radio_loading: bool,
    autoplay_seeded: bool,
    autoplay_waiting: Option<bool>,
    autoplay_tracks: VecDeque<Track>,
    watch_pending: bool,
    song_text: song_text::Worker,
    artwork: Artwork,
    library: Library,
    store: Option<LibraryWriter>,
    player: Option<Player>,
    active_id: Option<u64>,
    history_track: Option<u64>,
    playback: Playback,
    position: f64,
    pending_seek: Option<SeekIntent>,
    duration: f64,
    show_queue: bool,
    overlay: Option<Overlay>,
    overlay_subscription: Option<Subscription>,
    drag: Option<Drag>,
    seek_bounds: Rc<Cell<Bounds<Pixels>>>,
    volume_bounds: Rc<Cell<Bounds<Pixels>>>,
    drag_position: Option<f64>,
    _subscriptions: Vec<Subscription>,
}

impl Music {
    pub fn new(connect: bool, window: &mut Window, cx: &mut Context<Self>) -> Self {
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
        let account_operation = auth::ACCOUNT_LOGIN_ENABLED.then(|| {
            let operation = Operation::default();
            let command = if connect {
                account::Command::Connect(operation.clone())
            } else {
                account::Command::Restore(operation.clone())
            };
            let _ = account_commands.try_send(command);
            AccountOperation::Connecting(operation)
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
        let (player, error) = match Player::new(PlayerOptions {
            credentials: None,
            volume: library.volume,
            speed: library.speed,
            ..Default::default()
        }) {
            Ok(p) => (Some(p), error),
            Err(e) => (None, Some(format!("{e:#}"))),
        };
        let (artwork, loaded) = Artwork::new();
        let (jobs, receiver) = async_channel::bounded::<Job>(1);
        let (results, completed) = async_channel::unbounded::<CatalogResult>();
        let generation = Arc::new(AtomicU64::new(0));
        for worker in 0..2 {
            let receiver = receiver.clone();
            let results = results.clone();
            let generation = generation.clone();
            thread::Builder::new()
                .name(format!("catalog-{worker}"))
                .spawn(move || {
                    let catalog = Catalog::new();
                    while let Ok(job) = receiver.recv_blocking() {
                        if generation.load(Ordering::Relaxed) != job.id {
                            continue;
                        }
                        let append = matches!(
                            job.query,
                            Query::More { .. } | Query::PlaylistChoicesMore(_)
                        );
                        let target = if matches!(
                            job.query,
                            Query::PlaylistChoices | Query::PlaylistChoicesMore(_)
                        ) {
                            CatalogTarget::PlaylistPicker
                        } else {
                            CatalogTarget::Page
                        };
                        let result = match &catalog {
                            Err(e) => Err(format!("{e:#}")),
                            Ok(catalog) => {
                                let catalog = catalog.for_session(job.session);
                                match &job.query {
                                    Query::Home => catalog.home(),
                                    Query::Explore => catalog.explore(),
                                    Query::Library(section) => catalog.library(*section),
                                    Query::PlaylistChoices => catalog.library(Section::Playlists),
                                    Query::PlaylistChoicesMore(token) => catalog.more(token),
                                    Query::Search(search) => catalog.search_filtered(
                                        &search.query,
                                        search.filter,
                                        search.scope,
                                    ),
                                    Query::Browse(link) => catalog.browse(link),
                                    Query::More {
                                        token,
                                        search: Some(search),
                                    } => catalog.search_more(
                                        &search.query,
                                        search.filter,
                                        search.scope,
                                        token,
                                    ),
                                    Query::More {
                                        token,
                                        search: None,
                                    } => catalog.more(token),
                                }
                                .map_err(|e| format!("{e:#}"))
                            }
                        };
                        if results
                            .send_blocking(CatalogResult {
                                id: job.id,
                                append,
                                result,
                                target,
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                })
                .expect("start catalog worker");
        }
        cx.spawn_in(window, async move |view, cx| {
            while let Ok(result) = completed.recv().await {
                if view
                    .update_in(cx, |view, _, cx| view.catalog_result(result, cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
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
                        .map_err(|error| format!("{error:#}"));
                    let result = match job.query {
                        PlaybackQuery::Watch { track, radio } => PlaybackResult::Watch {
                            id: job.id,
                            radio,
                            result: catalog.and_then(|catalog| {
                                catalog
                                    .watch(&track, radio)
                                    .map(Box::new)
                                    .map_err(|error| format!("{error:#}"))
                            }),
                        },
                        PlaybackQuery::More(continuation) => PlaybackResult::More {
                            id: job.id,
                            result: catalog.and_then(|catalog| {
                                catalog
                                    .watch_more(&continuation)
                                    .map_err(|error| format!("{error:#}"))
                            }),
                        },
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
        let (song_text, text_events) = song_text::Worker::start();
        cx.spawn_in(window, async move |view, cx| {
            while let Ok(event) = text_events.recv().await {
                if view
                    .update_in(cx, |view, _, cx| {
                        if let Some(Overlay::SongText { id, content, .. }) = &mut view.overlay
                            && *id == event.id
                        {
                            *content = Some(event.result);
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
                        if let Some(old) = view.artwork.insert(loaded) {
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
        cx.spawn_in(window, async move |view, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(2)).await;
                if view
                    .update_in(cx, |view, _, cx| {
                        let theme = Theme::load();
                        if theme != view.theme {
                            view.theme = theme;
                            view.search
                                .update(cx, |search, cx| search.set_theme(theme, cx));
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
                SearchBoxEvent::Blur => window.focus(&view.focus, cx),
            });
        let activation = cx.observe_window_activation(window, |view, window, cx| {
            if window.is_window_active()
                && view.account.is_some()
                && view.account_operation.is_none()
                && view.overlay.is_none()
                && !view.loading
            {
                view.refresh_remote(cx);
            }
        });
        let mut view = Self {
            theme: Theme::load(),
            session,
            account: None,
            account_commands,
            account_worker: Some(account_worker),
            account_operation,
            pending_mutations: HashSet::new(),
            ratings: HashMap::new(),
            focus: cx.focus_handle(),
            search,
            route: Route::Home,
            page: Page {
                title: "Home".into(),
                shelves: Vec::new(),
                continuation: None,
                editable: false,
                description: String::new(),
                privacy: None,
                chips: Vec::new(),
                countries: Vec::new(),
            },
            rows: Vec::new(),
            back: Vec::new(),
            list_id: 0,
            loading: false,
            error,
            notice: None,
            generation,
            jobs,
            pending_jobs: receiver,
            playback_jobs,
            pending_playback_jobs: playback_receiver,
            playback_generation,
            now_playing: None,
            radio: None,
            radio_generation,
            radio_loading: false,
            autoplay_seeded: false,
            autoplay_waiting: None,
            autoplay_tracks: VecDeque::new(),
            watch_pending: false,
            song_text,
            artwork,
            library,
            store,
            player,
            active_id: None,
            history_track: None,
            playback: Playback::Stopped,
            position: 0.,
            pending_seek: None,
            duration: 0.,
            show_queue: false,
            overlay: None,
            overlay_subscription: None,
            drag: None,
            seek_bounds: Rc::new(Cell::new(Bounds::default())),
            volume_bounds: Rc::new(Cell::new(Bounds::default())),
            drag_position: None,
            _subscriptions: vec![subscription, activation],
        };
        window.focus(&view.focus, cx);
        view.request(Query::Home, cx);
        view
    }

    fn playback_request(&mut self, query: PlaybackQuery, cx: &mut Context<Self>) {
        let id = if matches!(query, PlaybackQuery::More(_)) {
            if !self.pending_playback_jobs.is_empty() || self.radio_loading {
                return;
            }
            self.radio_loading = true;
            self.radio_generation.fetch_add(1, Ordering::Relaxed) + 1
        } else {
            self.discard_pending_playback();
            self.watch_pending = matches!(query, PlaybackQuery::Watch { .. });
            self.playback_generation.fetch_add(1, Ordering::Relaxed) + 1
        };
        let session = self
            .account
            .as_ref()
            .map(|connection| connection.session.clone());
        if self
            .playback_jobs
            .try_send(PlaybackJob { id, query, session })
            .is_err()
        {
            self.radio_loading = false;
            self.watch_pending = false;
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

    fn discard_pending_playback(&mut self) {
        if let Ok(PlaybackJob {
            query: PlaybackQuery::More(_),
            ..
        }) = self.pending_playback_jobs.try_recv()
        {
            self.radio_loading = false;
        }
    }

    fn reset_radio(&mut self) {
        self.radio_generation.fetch_add(1, Ordering::Relaxed);
        self.radio = None;
        self.radio_loading = false;
        self.autoplay_seeded = false;
        self.autoplay_waiting = None;
        self.autoplay_tracks.clear();
    }

    fn extend_autoplay(&mut self, cx: &mut Context<Self>) {
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

    fn resume_autoplay(&mut self, cx: &mut Context<Self>) {
        let Some(natural) = self.autoplay_waiting else {
            return;
        };
        let autoplay = self.library.music_autoplay();
        if self.library.queue.advance_with_autoplay(
            self.library.repeat,
            natural,
            autoplay,
            &mut self.autoplay_tracks,
        ) {
            self.autoplay_waiting = None;
            self.play_current(cx);
        } else if self.radio_loading || self.watch_pending {
            self.playback = Playback::Loading {
                paused: false,
                retried: false,
            };
        } else {
            self.autoplay_waiting = None;
            self.playback = Playback::Stopped;
        }
    }

    fn playback_result(&mut self, result: PlaybackResult, cx: &mut Context<Self>) {
        match result {
            PlaybackResult::Watch { id, radio, result } => {
                if id != self.playback_generation.load(Ordering::Relaxed) {
                    return;
                }
                self.watch_pending = false;
                match result {
                    Ok(watch) => {
                        let watch = *watch;
                        if radio {
                            let Some(index) = watch.current_index else {
                                self.playback = Playback::Stopped;
                                self.notice = None;
                                self.error = Some("YouTube Music did not include the requested song in its radio.".into());
                                cx.notify();
                                return;
                            };
                            self.reset_radio();
                            self.library.queue.replace(watch.queue.clone(), index);
                            self.radio = watch.continuation.clone();
                            self.autoplay_seeded = true;
                            self.notice = Some("Radio started".into());
                        }
                        if let Some(track) = &watch.current
                            && let Some(index) = self.library.queue.index
                            && let Some(current) = self.library.queue.tracks.get_mut(index)
                            && current.id == track.id
                        {
                            // Keep the selected playlist occurrence when refreshing song metadata.
                            let occurrence = current.details.set_video_id.clone();
                            let credits = current.details.credits_id.clone();
                            current.details = track.details.clone();
                            current.details.set_video_id = occurrence;
                            current.details.credits_id =
                                track.details.credits_id.clone().or(credits);
                            if track.details.episode.is_some() {
                                current.artist = track.artist.clone();
                            }
                            if let Some(rating) = track.details.rating {
                                self.ratings.insert(track.id.clone(), rating);
                            }
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
                        self.now_playing = Some(watch);
                        if radio {
                            self.play_current(cx);
                        } else {
                            self.extend_autoplay(cx);
                            self.resume_autoplay(cx);
                        }
                    }
                    Err(error) if radio => {
                        self.playback = Playback::Stopped;
                        self.notice = None;
                        self.error = Some(error);
                    }
                    Err(error) => {
                        if self.autoplay_waiting.take().is_some() {
                            self.playback = Playback::Stopped;
                        }
                        self.notice = Some(format!("Playback details are unavailable: {error}"))
                    }
                }
            }
            PlaybackResult::More { id, result } => {
                if id != self.radio_generation.load(Ordering::Relaxed) {
                    return;
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

    fn clear_account_state(&mut self, cx: &mut Context<Self>) {
        let session = self
            .account
            .as_ref()
            .map(|connection| connection.session.clone());
        self.search
            .update(cx, |search, cx| search.set_session(session, cx));
        self.ratings.clear();
        self.history_track = None;
        self.song_text.cancel();
        self.pending_mutations.clear();
        self.playback_generation.fetch_add(1, Ordering::Relaxed);
        self.watch_pending = false;
        self.reset_radio();
        let _ = self.pending_playback_jobs.try_recv();
        self.now_playing = None;
        self.overlay = None;
        self.overlay_subscription = None;
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

    fn request(&mut self, query: Query, cx: &mut Context<Self>) {
        let id = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        let _ = self.pending_jobs.try_recv();
        let session = self
            .account
            .as_ref()
            .map(|connection| connection.session.clone());
        if self.jobs.try_send(Job { id, query, session }).is_err() {
            self.error = Some("The music connection stopped. Restart the app to reconnect.".into());
        } else {
            self.loading = true;
        }
        cx.notify();
    }

    fn navigate(&mut self, route: Route, cx: &mut Context<Self>) {
        if self.route != route {
            let cached = (self.account.is_none() && !self.loading && self.error.is_none())
                .then(|| self.page.clone());
            self.back.push((self.route.clone(), cached));
            if self.back.len() > 12 {
                self.back.remove(0);
            }
        }
        self.list_id = self.list_id.wrapping_add(1);
        self.route = route.clone();
        self.error = None;
        self.notice = None;
        self.page.shelves.clear();
        self.page.continuation = None;
        self.page.editable = false;
        self.page.description.clear();
        self.page.privacy = None;
        self.page.chips.clear();
        self.page.countries.clear();
        self.rows.clear();
        match route {
            Route::Home => {
                self.page.title = "Home".into();
                self.request(Query::Home, cx);
            }
            Route::Explore => {
                self.page.title = "Explore".into();
                self.request(Query::Explore, cx);
            }
            Route::Search(search) => {
                self.page.title = format!("Results for \"{}\"", search.query);
                self.search
                    .update(cx, |input, cx| input.set_query(search.query.clone(), cx));
                self.request(Query::Search(search), cx);
            }
            Route::Browse(link) => {
                self.page.title = link.title.clone();
                self.request(Query::Browse(link), cx);
            }
            Route::Library(section) => {
                self.page.title = section.title().into();
                self.request(Query::Library(section), cx);
            }
            Route::Liked | Route::Saved | Route::Recent if self.account.is_some() => {
                let section = match route {
                    Route::Liked => Section::Liked,
                    Route::Recent => Section::History,
                    _ => Section::Songs,
                };
                self.page.title = section.title().into();
                self.request(Query::Library(section), cx);
            }
            _ => {
                self.generation.fetch_add(1, Ordering::Relaxed);
                self.loading = false;
                self.refresh_local();
                cx.notify();
            }
        }
    }

    fn refresh_local(&mut self) {
        let (title, rows) = match self.route {
            Route::Liked => (
                "Liked songs",
                self.library
                    .liked
                    .iter()
                    .cloned()
                    .map(Item::Track)
                    .collect(),
            ),
            Route::Recent => (
                "Recently played",
                self.library
                    .recent
                    .iter()
                    .cloned()
                    .map(Item::Track)
                    .collect(),
            ),
            Route::Saved => (
                "Saved collections",
                self.library
                    .saved
                    .iter()
                    .cloned()
                    .map(Item::Collection)
                    .collect(),
            ),
            _ => return,
        };
        self.page = Page {
            title: title.into(),
            shelves: vec![Shelf {
                title: String::new(),
                items: rows,
            }],
            continuation: None,
            editable: false,
            description: String::new(),
            privacy: None,
            chips: Vec::new(),
            countries: Vec::new(),
        };
        self.rows = self
            .page
            .shelves
            .iter()
            .flat_map(|s| s.items.clone())
            .collect();
    }

    fn go_back(&mut self, cx: &mut Context<Self>) {
        if let Some((route, page)) = self.back.pop() {
            let Some(page) = page.filter(|_| self.account.is_none()) else {
                self.route = route.clone();
                self.navigate(route, cx);
                return;
            };
            self.generation.fetch_add(1, Ordering::Relaxed);
            self.list_id = self.list_id.wrapping_add(1);
            self.route = route;
            if let Route::Search(search) = &self.route {
                self.search
                    .update(cx, |input, cx| input.set_query(search.query.clone(), cx));
            }
            self.page = page;
            self.loading = false;
            self.error = None;
            self.notice = None;
            self.rows = self
                .page
                .shelves
                .iter()
                .flat_map(|s| s.items.clone())
                .collect();
            self.refresh_local();
            cx.notify();
        }
    }

    fn catalog_result(&mut self, result: CatalogResult, cx: &mut Context<Self>) {
        if result.id != self.generation.load(Ordering::Relaxed) {
            return;
        }
        self.loading = false;
        if matches!(result.target, CatalogTarget::PlaylistPicker) {
            if let Some(Overlay::PlaylistPicker { page, .. }) = &mut self.overlay {
                match result.result {
                    Ok(choices) => {
                        if result.append
                            && let Some(current) = page
                        {
                            current.continuation = choices.continuation;
                            current.shelves.extend(choices.shelves);
                        } else {
                            *page = Some(Box::new(choices));
                        }
                    }
                    Err(error) => self.error = Some(error),
                }
                cx.notify();
            }
            return;
        }
        match result.result {
            Ok(page) => {
                if result.append {
                    self.page.continuation = page.continuation;
                    self.page.shelves.extend(page.shelves);
                } else {
                    self.page = page;
                }
                for track in self.page.tracks() {
                    if let Some(rating) = track.details.rating {
                        self.ratings.insert(track.id, rating);
                    }
                }
                self.rows = self
                    .page
                    .shelves
                    .iter()
                    .flat_map(|s| s.items.clone())
                    .collect();
            }
            Err(error) => self.error = Some(error),
        }
        cx.notify();
    }

    fn save(&mut self) {
        if let Some(store) = &self.store {
            store.save(&self.library);
        }
    }
    fn is_liked(&self, track: &Track) -> bool {
        if self.account.is_some() {
            self.ratings
                .get(&track.id)
                .copied()
                .or(track.details.rating)
                == Some(Rating::Like)
        } else {
            self.library.liked.iter().any(|t| t.id == track.id)
        }
    }
    fn connect_account(&mut self, cx: &mut Context<Self>) {
        if !auth::ACCOUNT_LOGIN_ENABLED {
            self.error = Some(
                "Account connection is disabled until the security review is complete.".into(),
            );
            cx.notify();
            return;
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
                self.account_operation = Some(AccountOperation::Connecting(operation));
                self.notice = Some("Sign in to Google in the account window".into());
            }
            Err(_) => self.error = Some("The account connection is busy. Try again.".into()),
        }
        cx.notify();
    }
    fn mutate(&mut self, mutation: Mutation, cx: &mut Context<Self>) {
        if self.account_operation.is_some() {
            self.error = Some("Finish the account change before saving edits.".into());
            cx.notify();
            return;
        }
        let Some(connection) = &self.account else {
            return;
        };
        let key = mutation.key();
        if self.pending_mutations.contains(&key) {
            return;
        }
        match self.account_commands.try_send(account::Command::Apply {
            session: connection.session.clone(),
            mutation: Box::new(mutation),
        }) {
            Ok(()) => {
                self.pending_mutations.insert(key);
                self.notice = Some("Saving to YouTube Music…".into());
            }
            Err(_) => {
                self.error = Some("Too many pending changes. Wait for them to finish.".into())
            }
        }
        cx.notify();
    }
    fn disconnect_account(&mut self, cx: &mut Context<Self>) {
        if matches!(
            self.account_operation,
            Some(AccountOperation::Disconnecting(_))
        ) {
            return;
        }
        if let Some(operation) = self.account_operation.take() {
            operation.token().cancel();
        }
        if let Ok(mut session) = self.session.write() {
            *session = None;
        }
        self.account = None;
        self.clear_account_state(cx);
        self.player_command(Player::clear_cookies);
        self.active_id = None;
        self.playback = Playback::Stopped;
        self.navigate(Route::Home, cx);
        self.back.clear();
        let operation = Operation::default();
        if self
            .account_commands
            // Sign-out supersedes a queued edit if the bounded queue is full.
            .force_send(account::Command::Disconnect(operation.clone()))
            .is_err()
        {
            self.error = Some("Playback and account access stopped. The account worker could not remove the saved keyring entry.".into());
        } else {
            self.account_operation = Some(AccountOperation::Disconnecting(operation));
        }
        cx.notify();
    }
    fn refresh_remote(&mut self, cx: &mut Context<Self>) {
        let query = match &self.route {
            Route::Home => Query::Home,
            Route::Explore => Query::Explore,
            Route::Search(search) => Query::Search(search.clone()),
            Route::Browse(link) => Query::Browse(link.clone()),
            Route::Liked => Query::Library(Section::Liked),
            Route::Recent => Query::Library(Section::History),
            Route::Saved => Query::Library(Section::Songs),
            Route::Library(section) => Query::Library(*section),
        };
        self.request(query, cx);
    }
    fn playlist_form(
        &mut self,
        edit: bool,
        track: Option<Track>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (id, title, description, privacy) = if edit {
            let Route::Browse(link) = &self.route else {
                return;
            };
            if !self.page.editable {
                return;
            }
            let privacy = match self.page.privacy.as_deref() {
                Some("PRIVATE") => Privacy::Private,
                Some("UNLISTED") => Privacy::Unlisted,
                Some("PUBLIC") => Privacy::Public,
                _ => {
                    self.error = Some(
                        "Refresh the playlist to load its current privacy before editing.".into(),
                    );
                    cx.notify();
                    return;
                }
            };
            (
                Some(link.id.clone()),
                self.page.title.clone(),
                self.page.description.clone(),
                privacy,
            )
        } else {
            (None, String::new(), String::new(), Privacy::Private)
        };
        let form = cx.new(|cx| PlaylistForm::new(title, description, privacy, cx));
        let edit_id = id.clone();
        self.overlay_subscription = Some(cx.subscribe_in(
            &form,
            window,
            move |view, _, event, window, cx| match event {
                PlaylistFormEvent::Save {
                    title,
                    description,
                    privacy,
                } => {
                    let mutation = match &edit_id {
                        Some(id) => Mutation::EditPlaylist {
                            id: id.clone(),
                            title: title.clone(),
                            description: description.clone(),
                            privacy: *privacy,
                        },
                        None => Mutation::CreatePlaylist {
                            title: title.clone(),
                            description: description.clone(),
                            privacy: *privacy,
                        },
                    };
                    view.mutate(mutation, cx);
                }
                PlaylistFormEvent::Cancel => {
                    view.close_overlay(window, cx);
                }
            },
        ));
        window.focus(&form.focus_handle(cx), cx);
        self.error = None;
        self.overlay = Some(Overlay::PlaylistForm { form, track });
        cx.notify();
    }
    fn playlist_picker(&mut self, track: Track, cx: &mut Context<Self>) {
        self.error = None;
        self.overlay = Some(Overlay::PlaylistPicker {
            track,
            page: None,
            duplicates: false,
        });
        self.request(Query::PlaylistChoices, cx);
    }
    fn country_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.page.countries.is_empty() {
            return;
        }
        let picker = cx.new(|cx| CountryPicker::new(self.page.countries.clone(), cx));
        self.overlay_subscription = Some(cx.subscribe_in(
            &picker,
            window,
            |view, picker, event, window, cx| {
                if !matches!(&view.overlay, Some(Overlay::CountryPicker(current)) if current == picker) {
                    return;
                }
                view.close_overlay(window, cx);
                if let CountryPickerEvent::Choose(target) = event {
                    view.navigate(Route::Browse(target.clone()), cx);
                }
            },
        ));
        window.focus(&picker.focus_handle(cx), cx);
        self.overlay = Some(Overlay::CountryPicker(picker));
        cx.notify();
    }
    fn close_overlay(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.song_text.cancel();
        self.overlay = None;
        self.overlay_subscription = None;
        window.focus(&self.focus, cx);
        cx.notify();
    }
    fn show_song_text(&mut self, query: TextQuery, title: &str, cx: &mut Context<Self>) {
        let label = match &query {
            TextQuery::Lyrics(_) => "Lyrics",
            TextQuery::Credits(_) => "Credits",
        };
        let title = format!("{label} · {title}");
        let session = self.account.as_ref().map(|account| account.session.clone());
        self.overlay = None;
        match self.song_text.load(query, session) {
            Ok(id) => {
                self.overlay = Some(Overlay::SongText {
                    id,
                    title,
                    content: None,
                })
            }
            Err(error) => self.error = Some(format!("{error:#}")),
        }
        cx.notify();
    }
    fn account_event(
        &mut self,
        event: account::Event,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            account::Event::Disconnected { operation, result } => {
                if !matches!(&self.account_operation, Some(AccountOperation::Disconnecting(active)) if active.matches(&operation))
                {
                    return;
                }
                self.account_operation = None;
                match result {
                    Ok(()) => {
                        self.notice = Some("Signed out of YouTube Music".into());
                    }
                    Err(error) => self.error = Some(error),
                }
            }
            account::Event::Connected {
                operation,
                result,
                save_session,
            } => {
                if !matches!(&self.account_operation, Some(AccountOperation::Connecting(active)) if active.matches(&operation))
                {
                    return;
                }
                self.account_operation = None;
                match result {
                    Ok(Some(connection)) => {
                        let label = format!("Connected as {}", connection.account.name);
                        let credentials = match connection.session.playback_credentials() {
                            Ok(credentials) => credentials,
                            Err(error) => {
                                self.disconnect_account(cx);
                                self.error = Some(format!(
                                    "Could not activate playback credentials: {error:#}"
                                ));
                                cx.notify();
                                return;
                            }
                        };
                        let session = connection.session.clone();
                        self.account = Some(connection);
                        self.clear_account_state(cx);
                        window.focus(&self.focus, cx);
                        self.navigate(Route::Home, cx);
                        self.back.clear();
                        self.player_command(Player::clear_cookies);
                        self.active_id = None;
                        self.playback = Playback::Stopped;
                        self.player_command(|p| p.set_credentials(credentials));
                        if save_session
                            && self
                                .account_commands
                                .try_send(account::Command::Persist(session))
                                .is_err()
                        {
                            self.error = Some("Connected for this session, but the account could not be queued for keyring storage.".into());
                        }
                        self.notice = Some(label);
                    }
                    Ok(None) => {}
                    Err(error) => self.error = Some(error),
                }
            }
            account::Event::Applied {
                session,
                mutation,
                result,
            } => {
                if !self
                    .account
                    .as_ref()
                    .is_some_and(|connection| Arc::ptr_eq(&connection.session, &session))
                {
                    return;
                }
                let mutation = *mutation;
                self.pending_mutations.remove(&mutation.key());
                match result {
                    Ok(effect) => {
                        let deleting = matches!(mutation, Mutation::DeletePlaylist { .. });
                        if let Mutation::RateSong { track, rating } = &mutation {
                            self.ratings.insert(track.id.clone(), *rating);
                        }
                        self.notice = Some("Saved to YouTube Music".into());
                        match effect {
                            Effect::CreatedPlaylist(id) => {
                                let track = match self.overlay.take() {
                                    Some(Overlay::PlaylistForm { track, .. }) => track,
                                    _ => None,
                                };
                                self.close_overlay(window, cx);
                                if let Some(track) = track {
                                    self.mutate(
                                        Mutation::AddToPlaylist {
                                            id: id.clone(),
                                            video_ids: vec![track.id],
                                            duplicates: false,
                                        },
                                        cx,
                                    );
                                }
                                self.navigate(
                                    Route::Browse(BrowseLink::new(format!("VL{id}"), "Playlist")),
                                    cx,
                                )
                            }
                            Effect::Changed => {
                                if matches!(
                                    mutation,
                                    Mutation::EditPlaylist { .. } | Mutation::DeletePlaylist { .. }
                                ) {
                                    self.close_overlay(window, cx);
                                }
                                if deleting {
                                    self.navigate(Route::Library(Section::Playlists), cx);
                                } else {
                                    self.refresh_remote(cx);
                                }
                            }
                        }
                    }
                    Err(error) => {
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
            } => {
                if !self
                    .account
                    .as_ref()
                    .is_some_and(|connection| Arc::ptr_eq(&connection.session, &session))
                {
                    return;
                }
                match result {
                    Ok(())
                        if self.overlay.is_none()
                            && !self.loading
                            && matches!(
                                self.route,
                                Route::Recent | Route::Library(Section::History)
                            ) =>
                    {
                        self.request(Query::Library(Section::History), cx);
                    }
                    Ok(()) => {}
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
        cx.notify();
    }
    fn player_command(&mut self, command: impl FnOnce(&Player) -> anyhow::Result<()>) {
        if let Some(player) = &self.player
            && let Err(error) = command(player)
        {
            self.error = Some(error.to_string());
        }
    }
    fn playback_position(&self) -> f64 {
        self.pending_seek
            .filter(|seek| Some(seek.track_id) == self.active_id)
            .map_or(self.position, |seek| seek.seconds)
    }
    fn seek_current(&mut self, seconds: f64, cx: &mut Context<Self>) {
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
    fn play_current(&mut self, cx: &mut Context<Self>) {
        self.play_current_at(0., cx);
    }
    fn play_current_at(&mut self, start: f64, cx: &mut Context<Self>) {
        let Some(track) = self.library.queue.current().cloned() else {
            return;
        };
        if track.details.episode.is_some() {
            self.reset_radio();
        }
        self.autoplay_waiting = None;
        self.notice = None;
        self.playback_generation.fetch_add(1, Ordering::Relaxed);
        self.watch_pending = false;
        self.discard_pending_playback();
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
                start,
                ..Default::default()
            },
        ) {
            Ok(id) => {
                self.active_id = Some(id);
                self.playback = Playback::Loading {
                    paused: false,
                    retried: false,
                };
                self.position = start;
                self.duration = track.duration.unwrap_or(0) as f64;
                self.error = None;
                if !self
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
                            radio: false,
                        },
                        cx,
                    );
                }
            }
            Err(error) => self.error = Some(error.to_string()),
        }
        self.extend_autoplay(cx);
        self.save();
        cx.notify();
    }
    fn play_chapter(&mut self, track: Track, seconds: u32, cx: &mut Context<Self>) {
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
            self.reset_radio();
            self.now_playing = None;
            self.library.queue.replace(vec![track], 0);
        }
        self.play_current_at(seconds as f64, cx);
    }
    fn set_audio_quality(&mut self, quality: AudioQuality, cx: &mut Context<Self>) {
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
        self.save();
        cx.notify();
    }
    fn reload_stream(&mut self) -> anyhow::Result<()> {
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
                start,
                paused,
            },
        )?;
        if self.active_id.is_some() && self.history_track == self.active_id {
            self.history_track = Some(id);
        }
        self.active_id = Some(id);
        self.position = start;
        self.playback = Playback::Loading { paused, retried };
        self.error = None;
        Ok(())
    }
    fn replace_queue(&mut self, tracks: Vec<Track>, selected: usize, cx: &mut Context<Self>) {
        self.reset_radio();
        self.now_playing = None;
        self.library.queue.replace(tracks, selected);
        self.play_current(cx);
    }
    fn play_recommendation(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(track) = self.autoplay_tracks.remove(index) else {
            return;
        };
        self.autoplay_tracks.drain(..index);
        let selected = self.library.queue.tracks.len();
        self.library.queue.tracks.push(track);
        self.library.queue.select(selected);
        self.play_current(cx);
    }
    fn clear_queue(&mut self, cx: &mut Context<Self>) {
        self.player_command(Player::stop);
        self.active_id = None;
        self.playback = Playback::Stopped;
        self.position = 0.;
        self.duration = 0.;
        self.notice = None;
        self.library.queue = Default::default();
        self.now_playing = None;
        self.reset_radio();
        self.playback_generation.fetch_add(1, Ordering::Relaxed);
        self.watch_pending = false;
        self.discard_pending_playback();
        self.save();
        cx.notify();
    }
    fn toggle_autoplay(&mut self, cx: &mut Context<Self>) {
        self.library.autoplay = !self.library.autoplay;
        if self.library.autoplay {
            self.extend_autoplay(cx);
        } else {
            if self.autoplay_waiting.is_some() {
                self.playback = Playback::Stopped;
            }
            self.reset_radio();
        }
        self.save();
        cx.notify();
    }
    fn play_item(&mut self, item: Item, cx: &mut Context<Self>) {
        match item {
            Item::Navigation(link) => self.navigate(Route::Browse(link), cx),
            Item::Collection(collection) => self.navigate(
                Route::Browse(BrowseLink::new(collection.id, collection.title)),
                cx,
            ),
            Item::Track(track) => {
                let tracks = self.page.tracks();
                if let Some(index) = tracks.iter().position(|t| {
                    t.id == track.id && t.details.set_video_id == track.details.set_video_id
                }) {
                    self.replace_queue(tracks, index, cx);
                } else {
                    self.replace_queue(vec![track], 0, cx);
                }
            }
        }
    }
    fn toggle_play(&mut self, cx: &mut Context<Self>) {
        if self.active_id.is_some() && self.playback != Playback::Stopped {
            let paused = !self.playback.is_paused();
            if let Some(player) = &self.player {
                match player.pause(paused) {
                    Ok(()) => {
                        self.playback = self.playback.with_pause(paused);
                    }
                    Err(error) => self.error = Some(error.to_string()),
                }
            }
            cx.notify();
        } else {
            if self.library.queue.index.is_none() {
                self.library.queue.select(0);
            }
            self.play_current(cx);
        }
    }
    fn next(&mut self, natural: bool, cx: &mut Context<Self>) {
        let autoplay = self.library.music_autoplay();
        if self.library.queue.advance_with_autoplay(
            self.library.repeat,
            natural,
            autoplay,
            &mut self.autoplay_tracks,
        ) {
            self.play_current(cx);
        } else {
            self.player_command(Player::stop);
            self.active_id = None;
            self.playback = Playback::Stopped;
            self.position = self.duration;
            if autoplay {
                self.autoplay_waiting = Some(natural);
                self.extend_autoplay(cx);
                self.resume_autoplay(cx);
            }
            cx.notify();
        }
    }
    fn previous(&mut self, cx: &mut Context<Self>) {
        if self.autoplay_waiting.take().is_some() || self.active_id.is_none() {
            if self.playback_position() <= 3. {
                self.library.queue.previous();
            }
            self.play_current(cx);
            return;
        }
        if self.playback_position() > 3. {
            self.seek_current(0., cx);
        } else if self.library.queue.previous() {
            self.play_current(cx);
        }
    }
    fn player_event(&mut self, event: PlayerEvent, cx: &mut Context<Self>) {
        let mut changed = true;
        match event {
            PlayerEvent::Volume(volume) => {
                changed = self.library.volume.round() != volume.round();
                self.library.volume = volume.clamp(0., 100.);
            }
            PlayerEvent::Speed(speed) => {
                changed = self.library.speed != speed;
                self.library.speed = speed;
                if changed {
                    self.save();
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
                self.active_id = None;
                self.playback = Playback::Stopped;
            }
            PlayerEvent::Started { track_id, paused } if Some(track_id) == self.active_id => {
                self.playback = if paused {
                    Playback::Paused
                } else {
                    Playback::Playing
                };
                if self.history_track != Some(track_id)
                    && let Some(track) = self.library.queue.current().cloned()
                {
                    self.history_track = Some(track_id);
                    if let Some(connection) = &self.account {
                        if self
                            .account_commands
                            .try_send(account::Command::ReportPlayback {
                                session: connection.session.clone(),
                                video_id: track.id,
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
                    Ok(seconds) => self.position = seconds,
                    Err(error) if latest => self.error = Some(error),
                    Err(_) => changed = false,
                }
            }
            PlayerEvent::Duration { track_id, seconds } if Some(track_id) == self.active_id => {
                self.duration = seconds
            }
            PlayerEvent::Paused { track_id, paused } if Some(track_id) == self.active_id => {
                self.playback = self.playback.with_pause(paused)
            }
            PlayerEvent::Buffering {
                track_id,
                buffering,
            } if Some(track_id) == self.active_id => {
                if buffering && !self.playback.is_paused() {
                    self.playback = Playback::Buffering;
                } else if self.playback == Playback::Buffering {
                    self.playback = Playback::Playing;
                }
            }
            PlayerEvent::Ended { track_id } if Some(track_id) == self.active_id => {
                self.next(true, cx);
                return;
            }
            _ => changed = false,
        }
        if changed {
            cx.notify();
        }
    }
    fn toggle_like(&mut self, track: &Track, cx: &mut Context<Self>) {
        if self.account.is_some() {
            let rating = if self.is_liked(track) {
                Rating::Indifferent
            } else {
                Rating::Like
            };
            self.mutate(
                Mutation::RateSong {
                    track: track.clone(),
                    rating,
                },
                cx,
            );
            return;
        }
        self.library.toggle_like(track);
        self.save();
        self.refresh_local();
        cx.notify();
    }
    fn enqueue(&mut self, track: Track, cx: &mut Context<Self>) {
        self.notice = Some(format!("Added {} to the queue", track.title));
        self.library.queue.tracks.push(track);
        self.resume_autoplay(cx);
        self.save();
        cx.notify();
    }
    fn save_collection(&mut self, collection: Collection, cx: &mut Context<Self>) {
        if self.account.is_some() {
            let save = !self.is_saved(&collection);
            self.mutate(Mutation::CollectionLibrary { collection, save }, cx);
            return;
        }
        if let Some(i) = self
            .library
            .saved
            .iter()
            .position(|c| c.id == collection.id)
        {
            self.library.saved.remove(i);
            self.notice = Some("Collection removed from saved music".into());
        } else {
            self.library.saved.insert(0, collection);
            self.notice = Some("Collection saved".into());
        }
        self.save();
        self.refresh_local();
        cx.notify();
    }
    fn is_saved(&self, collection: &Collection) -> bool {
        if self.account.is_none() {
            return self.library.saved.iter().any(|c| c.id == collection.id);
        }
        collection.details.saved.unwrap_or(matches!(
            self.route,
            Route::Library(
                Section::Albums | Section::Playlists | Section::Subscriptions | Section::Podcasts
            )
        ))
    }
    fn set_volume(&mut self, volume: f64, cx: &mut Context<Self>) {
        let volume = volume.clamp(0., 100.).round();
        if self.library.volume != volume {
            self.library.volume = volume;
            self.player_command(|p| p.set_volume(volume));
            cx.notify();
        }
    }
    fn drag_to(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        match self.drag {
            Some(Drag::Seek) if self.duration > 0. => {
                let bounds = self.seek_bounds.get();
                self.drag_position = Some(
                    ((position.x - bounds.left()) / bounds.size.width).clamp(0., 1.) as f64
                        * self.duration,
                );
                cx.notify();
            }
            Some(Drag::Volume) => {
                let bounds = self.volume_bounds.get();
                self.set_volume(
                    ((position.x - bounds.left()) / bounds.size.width).clamp(0., 1.) as f64 * 100.,
                    cx,
                );
            }
            _ => {}
        }
    }
    fn end_drag(&mut self, cx: &mut Context<Self>) {
        if self.drag == Some(Drag::Seek)
            && let Some(seconds) = self.drag_position.take()
        {
            self.seek_current(seconds, cx);
        }
        if self.drag.take().is_some() {
            self.save();
            cx.notify();
        }
    }
    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        let control = event.keystroke.modifiers.control;
        if control && key == "q" {
            cx.quit();
            return;
        }
        if self.overlay.is_some() && key != "escape" {
            return;
        }
        if control && key == "k" {
            window.focus(&self.search.focus_handle(cx), cx);
            return;
        }
        if control && key == "r" {
            self.navigate(self.route.clone(), cx);
            return;
        }
        if self.search.focus_handle(cx).is_focused(window) {
            return;
        }
        match key {
            "space" => self.toggle_play(cx),
            "right" if control => self.next(false, cx),
            "left" if control => self.previous(cx),
            "right" => {
                let seconds = (self.playback_position() + 5.).min(self.duration);
                self.seek_current(seconds, cx);
            }
            "left" => {
                let seconds = (self.playback_position() - 5.).max(0.);
                self.seek_current(seconds, cx);
            }
            "up" => {
                self.set_volume(self.library.volume + 5., cx);
                self.save();
            }
            "down" => {
                self.set_volume(self.library.volume - 5., cx);
                self.save();
            }
            "l" => {
                if let Some(track) = self.library.queue.current().cloned() {
                    self.toggle_like(&track, cx);
                }
            }
            "q" => {
                self.show_queue = !self.show_queue;
                cx.notify();
            }
            "escape" => {
                self.close_overlay(window, cx);
                self.show_queue = false;
                cx.notify();
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
    fn art(&mut self, url: Option<&str>, edge: f32) -> AnyElement {
        let image = url.and_then(|url| self.artwork.get(url));
        let mut element = div()
            .size(px(edge))
            .flex_shrink_0()
            .rounded(px(5.))
            .overflow_hidden()
            .bg(self.theme.raised);
        element = if let Some(image) = image {
            element.child(img(image).size_full().object_fit(ObjectFit::Cover))
        } else {
            element.flex().items_center().justify_center().child(
                icon("music", edge.min(32.) * 0.6, self.theme.muted).text_color(self.theme.muted),
            )
        };
        element.into_any_element()
    }
    fn nav(
        &self,
        id: &'static str,
        glyph: &'static str,
        label: &'static str,
        route: Route,
        compact: bool,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let active = self.route == route
            || (route == Route::Home && self.route.is_home())
            || (route == Route::Saved && matches!(self.route, Route::Library(_)));
        div()
            .id(id)
            .h(px(44.))
            .px(px(13.))
            .flex()
            .items_center()
            .gap(px(14.))
            .rounded(px(7.))
            .text_color(if active {
                self.theme.text
            } else {
                self.theme.muted
            })
            .when(active, |d| d.bg(self.theme.selection))
            .hover(|d| d.bg(self.theme.raised).text_color(self.theme.text))
            .cursor_pointer()
            .tooltip(move |_, cx| {
                cx.new(|_| Hint {
                    text: label,
                    theme: Theme::load(),
                })
                .into()
            })
            .child(icon(
                glyph,
                21.,
                if active {
                    self.theme.text
                } else {
                    self.theme.muted
                },
            ))
            .when(!compact, |d| d.child(label))
            .on_click(cx.listener(move |view, _, window, cx| {
                window.focus(&view.focus, cx);
                view.navigate(route.clone(), cx);
            }))
    }
    fn sidebar(&self, compact: bool, cx: &Context<Self>) -> impl IntoElement {
        div()
            .w(px(if compact { 74. } else { 206. }))
            .h_full()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .px(px(12.))
            .py(px(24.))
            .gap(px(6.))
            .border_r_1()
            .border_color(self.theme.border)
            .bg(self.theme.panel)
            .child(
                div()
                    .h(px(45.))
                    .px(px(12.))
                    .mb(px(25.))
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .child(
                        div()
                            .size(px(29.))
                            .rounded_full()
                            .bg(rgb(0xff344e))
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(icon("play", 18., self.theme.text).text_color(rgb(0xffffff))),
                    )
                    .when(!compact, |d| {
                        d.child(
                            div()
                                .text_size(px(21.))
                                .font_weight(FontWeight::BOLD)
                                .child("Music"),
                        )
                    }),
            )
            .child(self.nav("home-nav", "home", "Home", Route::Home, compact, cx))
            .child(self.nav(
                "explore-nav",
                "explore",
                "Explore",
                Route::Explore,
                compact,
                cx,
            ))
            .child(
                div()
                    .h(px(1.))
                    .mx(px(12.))
                    .my(px(17.))
                    .bg(self.theme.border),
            )
            .child(self.nav(
                "liked-nav",
                "heart",
                "Liked songs",
                Route::Liked,
                compact,
                cx,
            ))
            .child(self.nav(
                "saved-nav",
                "library",
                if self.account.is_some() {
                    "Library"
                } else {
                    "Saved collections"
                },
                Route::Saved,
                compact,
                cx,
            ))
            .child(self.nav(
                "recent-nav",
                "history",
                "Recently played",
                Route::Recent,
                compact,
                cx,
            ))
            .child(div().flex_1())
            .child(
                div()
                    .id("shortcut-help")
                    .h(px(42.))
                    .px(px(13.))
                    .flex()
                    .items_center()
                    .gap(px(14.))
                    .text_color(self.theme.muted)
                    .cursor_pointer()
                    .hover(|d| d.text_color(self.theme.text))
                    .child(icon("help", 20., self.theme.text))
                    .when(!compact, |d| d.child("Keyboard shortcuts"))
                    .on_click(cx.listener(|view, _, window, cx| {
                        window.focus(&view.focus, cx);
                        view.overlay = Some(Overlay::Help);
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .id("settings-nav")
                    .h(px(42.))
                    .px(px(13.))
                    .flex()
                    .items_center()
                    .gap(px(14.))
                    .text_color(self.theme.muted)
                    .cursor_pointer()
                    .hover(|d| d.text_color(self.theme.text))
                    .child(icon("settings", 20., self.theme.text))
                    .when(!compact, |d| d.child("Settings"))
                    .on_click(cx.listener(|view, _, window, cx| {
                        window.focus(&view.focus, cx);
                        view.overlay = Some(Overlay::Settings);
                        cx.notify();
                    })),
            )
    }
    fn header(&self, cx: &Context<Self>) -> impl IntoElement {
        div()
            .h(px(80.))
            .px(px(30.))
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap(px(18.))
            .child(
                button("back", "back", self.theme)
                    .opacity(if self.back.is_empty() { 0.3 } else { 1. })
                    .on_click(cx.listener(|view, _, _, cx| view.go_back(cx))),
            )
            .child(
                div()
                    .id("search-box")
                    .max_w(px(670.))
                    .flex_1()
                    .h(px(46.))
                    .px(px(16.))
                    .rounded(px(8.))
                    .bg(self.theme.raised)
                    .border_1()
                    .border_color(self.theme.border)
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .on_click(cx.listener(|view, _, window, cx| {
                        window.focus(&view.search.focus_handle(cx), cx)
                    }))
                    .child(icon("search", 21., self.theme.text).text_color(self.theme.muted))
                    .child(div().flex_1().min_w_0().child(self.search.clone()))
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(self.theme.muted)
                            .child("Ctrl K"),
                    ),
            )
            .child(div().flex_1())
            .child(
                button("open-web", "external", self.theme)
                    .on_click(|_, _, cx| cx.open_url("https://music.youtube.com")),
            )
            .child(
                div()
                    .id("refresh")
                    .cursor_pointer()
                    .text_color(self.theme.muted)
                    .hover(|d| d.text_color(self.theme.text))
                    .child("Refresh")
                    .on_click(cx.listener(|view, _, _, cx| view.navigate(view.route.clone(), cx))),
            )
    }
    fn card(&mut self, item: Item, id: usize, cx: &Context<Self>) -> AnyElement {
        if let Item::Navigation(link) = item {
            return self
                .navigation(link, ("shelf-link", id), cx)
                .w(px(240.))
                .h(px(44.))
                .into_any_element();
        }
        let art = self.art(item.thumbnail(), 164.);
        let title = item.title().to_owned();
        let subtitle = item.subtitle().to_owned();
        div()
            .id(("card", id))
            .w(px(164.))
            .flex_shrink_0()
            .flex()
            .flex_col()
            .gap(px(9.))
            .cursor_pointer()
            .child(div().rounded(px(5.)).overflow_hidden().child(art))
            .child(
                div()
                    .w_full()
                    .truncate()
                    .text_size(px(15.))
                    .font_weight(FontWeight::MEDIUM)
                    .child(title),
            )
            .child(
                div()
                    .w_full()
                    .truncate()
                    .text_size(px(12.))
                    .text_color(self.theme.muted)
                    .child(subtitle),
            )
            .on_click(cx.listener(move |view, _, window, cx| {
                window.focus(&view.focus, cx);
                view.play_item(item.clone(), cx);
            }))
            .into_any_element()
    }
    fn navigation(
        &self,
        link: BrowseLink,
        id: impl Into<ElementId>,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        div()
            .id(id)
            .flex()
            .items_center()
            .gap(px(8.))
            .text_size(px(15.))
            .cursor_pointer()
            .hover(|d| d.text_color(self.theme.accent))
            .child(div().flex_1().child(link.title.clone()))
            .child(icon("forward", 17., self.theme.muted))
            .on_click(cx.listener(move |view, _, window, cx| {
                window.focus(&view.focus, cx);
                view.navigate(Route::Browse(link.clone()), cx);
            }))
    }
    fn shelf(&mut self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let shelf = self.page.shelves[index].clone();
        let mut contents = div()
            .id(("carousel", index))
            .flex()
            .gap(px(20.))
            .overflow_x_scroll()
            .pb(px(10.));
        if shelf
            .items
            .iter()
            .all(|item| matches!(item, Item::Navigation(_)))
        {
            for (column, items) in shelf.items.chunks(4).enumerate() {
                contents = contents.child(
                    div()
                        .w(px(240.))
                        .flex_shrink_0()
                        .flex()
                        .flex_col()
                        .gap(px(12.))
                        .children(
                            items
                                .iter()
                                .cloned()
                                .enumerate()
                                .map(|(row, item)| {
                                    self.card(item, index * 1000 + column * 4 + row, cx)
                                })
                                .collect::<Vec<_>>(),
                        ),
                );
            }
        } else {
            contents = contents.children(
                shelf
                    .items
                    .into_iter()
                    .enumerate()
                    .map(|(i, item)| self.card(item, index * 1000 + i, cx))
                    .collect::<Vec<_>>(),
            );
        }
        div()
            .id(("shelf", index))
            .h(px(308.))
            .px(px(30.))
            .flex()
            .flex_col()
            .gap(px(17.))
            .child(
                div()
                    .text_size(px(23.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(shelf.title),
            )
            .child(contents)
            .into_any_element()
    }
    fn row(&mut self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let item = self.rows[index].clone();
        let active = self
            .library
            .queue
            .current()
            .is_some_and(|t| t.id == item.id())
            && self.active_id.is_some();
        let art = self.art(item.thumbnail(), 46.);
        let title = item.title().to_owned();
        let subtitle = item.subtitle().to_owned();
        let cloned = item.clone();
        let mut row = div()
            .id(("song", index))
            .w_full()
            .px(px(30.))
            .h(px(68.))
            .flex()
            .items_center()
            .gap(px(14.))
            .rounded(px(6.))
            .when(active, |d| d.bg(self.theme.selection))
            .hover(|d| d.bg(self.theme.raised))
            .cursor_pointer()
            .on_click(cx.listener(move |view, _, window, cx| {
                window.focus(&view.focus, cx);
                view.play_item(cloned.clone(), cx);
            }))
            .child(art)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(5.))
                    .child(
                        div()
                            .truncate()
                            .text_size(px(15.))
                            .when(active, |d| d.text_color(self.theme.accent))
                            .child(title),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_size(px(12.))
                            .text_color(self.theme.muted)
                            .child(subtitle),
                    ),
            );
        match item {
            Item::Navigation(link) => {
                return self
                    .navigation(link, ("browse-link", index), cx)
                    .w_full()
                    .h(px(68.))
                    .px(px(30.))
                    .hover(|d| d.bg(self.theme.raised))
                    .into_any_element();
            }
            Item::Track(track) => {
                let liked = self.is_liked(&track);
                let like_track = track.clone();
                let menu_track = track.clone();
                row = row
                    .child(
                        div()
                            .w(px(if track.details.episode.is_some() {
                                100.
                            } else {
                                48.
                            }))
                            .text_size(px(12.))
                            .text_color(self.theme.muted)
                            .child(track.duration.map(|s| clock(s as f64)).unwrap_or_else(|| {
                                track
                                    .details
                                    .episode
                                    .as_ref()
                                    .map(|episode| episode.duration_text.clone())
                                    .unwrap_or_default()
                            })),
                    )
                    .child(
                        button(
                            ("like", index),
                            if liked { "heart-filled" } else { "heart" },
                            self.theme,
                        )
                        .when(liked, |d| d.text_color(self.theme.accent))
                        .on_click(cx.listener(move |view, _, _, cx| {
                            cx.stop_propagation();
                            view.toggle_like(&like_track, cx);
                        })),
                    )
                    .child(
                        button(("add", index), "plus", self.theme).on_click(cx.listener(
                            move |view, _, _, cx| {
                                cx.stop_propagation();
                                view.enqueue(track.clone(), cx);
                            },
                        )),
                    )
                    .child(
                        div()
                            .id(("track-menu", index))
                            .w(px(32.))
                            .h(px(36.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_size(px(22.))
                            .child("⋯")
                            .on_click(cx.listener(move |view, _, window, cx| {
                                cx.stop_propagation();
                                window.focus(&view.focus, cx);
                                view.overlay = Some(Overlay::Track {
                                    track: menu_track.clone(),
                                    index: Some(index),
                                });
                                cx.notify();
                            })),
                    );
            }
            Item::Collection(collection) => {
                let saved = self.is_saved(&collection);
                row = row
                    .child(div().text_size(px(12.)).text_color(self.theme.muted).child(
                        if collection.id.starts_with("UC") {
                            "Artist"
                        } else {
                            "Collection"
                        },
                    ))
                    .child(
                        button(
                            ("save", index),
                            if saved { "heart-filled" } else { "plus" },
                            self.theme,
                        )
                        .on_click(cx.listener(move |view, _, _, cx| {
                            cx.stop_propagation();
                            view.save_collection(collection.clone(), cx);
                        })),
                    )
                    .child(icon("forward", 17., self.theme.text).text_color(self.theme.muted));
            }
        }
        row.into_any_element()
    }
    fn content(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let home = self.route.is_home() || matches!(self.route, Route::Explore);
        let has_tracks = self.rows.iter().any(|i| matches!(i, Item::Track(_)));
        let single_episode =
            matches!(self.rows.as_slice(), [Item::Track(track)] if track.details.episode.is_some());
        let chapter_track = match self.rows.as_slice() {
            [Item::Track(track)]
                if track
                    .details
                    .episode
                    .as_ref()
                    .is_some_and(|episode| !episode.chapters.is_empty()) =>
            {
                Some(track.clone())
            }
            _ => None,
        };
        let mut content = div()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .child(self.header(cx))
            .child(
                div()
                    .px(px(30.))
                    .pt(px(10.))
                    .pb(px(25.))
                    .flex()
                    .items_center()
                    .gap(px(16.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(31.))
                            .font_weight(FontWeight::BOLD)
                            .child(self.page.title.clone()),
                    )
                    .when(!self.page.description.is_empty(), |heading| {
                        heading.child(
                            div()
                                .id("page-description")
                                .py(px(10.))
                                .text_size(px(13.))
                                .cursor_pointer()
                                .child("Description")
                                .on_click(cx.listener(|view, _, window, cx| {
                                    window.focus(&view.focus, cx);
                                    view.overlay = Some(Overlay::Description {
                                        title: view.page.title.clone(),
                                        text: view.page.description.clone(),
                                    });
                                    cx.notify();
                                })),
                        )
                    })
                    .when_some(chapter_track, |heading, track| {
                        heading.child(
                            div()
                                .id("page-chapters")
                                .py(px(10.))
                                .text_size(px(13.))
                                .cursor_pointer()
                                .child("Chapters")
                                .on_click(cx.listener(move |view, _, window, cx| {
                                    window.focus(&view.focus, cx);
                                    view.overlay = Some(Overlay::Chapters(Box::new(track.clone())));
                                    cx.notify();
                                })),
                        )
                    })
                    .when(!self.page.countries.is_empty(), |heading| {
                        let label = self
                            .page
                            .countries
                            .iter()
                            .find(|choice| choice.selected)
                            .map(|choice| choice.label.clone())
                            .unwrap_or_else(|| "Country".into());
                        heading.child(
                            div()
                                .id("chart-country")
                                .py(px(10.))
                                .flex()
                                .items_center()
                                .gap(px(10.))
                                .text_size(px(14.))
                                .cursor_pointer()
                                .child(label)
                                .child(icon("forward", 14., self.theme.muted))
                                .on_click(cx.listener(|view, _, window, cx| {
                                    view.country_picker(window, cx)
                                })),
                        )
                    })
                    .when(
                        self.account.is_some()
                            && matches!(
                                self.route,
                                Route::Saved | Route::Library(Section::Playlists)
                            ),
                        |d| {
                            d.child(
                                div()
                                    .id("new-playlist")
                                    .cursor_pointer()
                                    .text_size(px(13.))
                                    .child("New playlist")
                                    .on_click(cx.listener(|view, _, window, cx| {
                                        view.playlist_form(false, None, window, cx)
                                    })),
                            )
                        },
                    )
                    .when(self.account.is_some() && self.page.editable, |d| {
                        d.child(
                            div()
                                .id("edit-playlist")
                                .cursor_pointer()
                                .text_size(px(13.))
                                .child("Edit")
                                .on_click(cx.listener(|view, _, window, cx| {
                                    view.playlist_form(true, None, window, cx)
                                })),
                        )
                        .child(
                            div()
                                .id("delete-playlist")
                                .cursor_pointer()
                                .text_size(px(13.))
                                .child("Delete")
                                .on_click(cx.listener(|view, _, _, cx| {
                                    if let Route::Browse(link) = &view.route {
                                        view.overlay = Some(Overlay::DeletePlaylist {
                                            id: link.id.clone(),
                                            title: view.page.title.clone(),
                                        });
                                        cx.notify();
                                    }
                                })),
                        )
                    })
                    .when(!home && has_tracks, |d| {
                        d.child(
                            div()
                                .id("play-all")
                                .h(px(36.))
                                .px(px(17.))
                                .rounded_full()
                                .bg(self.theme.text)
                                .text_color(self.theme.bg)
                                .flex()
                                .items_center()
                                .gap(px(8.))
                                .cursor_pointer()
                                .child(icon("play", 17., self.theme.text))
                                .child(if single_episode { "Play" } else { "Play all" })
                                .on_click(cx.listener(|view, _, _, cx| {
                                    view.replace_queue(view.page.tracks(), 0, cx);
                                })),
                        )
                    }),
            );
        if !self.page.chips.is_empty() {
            content = content.child(
                div()
                    .id("home-filters")
                    .mx(px(30.))
                    .mb(px(20.))
                    .flex()
                    .gap(px(22.))
                    .overflow_x_scroll()
                    .children(self.page.chips.iter().enumerate().map(|(index, choice)| {
                        let target = choice.target.clone();
                        div()
                            .id(("home-filter", index))
                            .flex_shrink_0()
                            .pb(px(9.))
                            .border_b_2()
                            .border_color(if choice.selected {
                                self.theme.accent
                            } else {
                                gpui::transparent_black()
                            })
                            .text_color(if choice.selected {
                                self.theme.text
                            } else {
                                self.theme.muted
                            })
                            .cursor_pointer()
                            .child(choice.label.clone())
                            .on_click(cx.listener(move |view, _, _, cx| {
                                view.navigate(Route::Browse(target.clone()), cx)
                            }))
                    })),
            );
        }
        if let Route::Search(search) = &self.route {
            if self.account.is_some() {
                content = content.child(
                    div()
                        .id("search-scopes")
                        .mx(px(30.))
                        .mb(px(16.))
                        .flex()
                        .gap(px(22.))
                        .children(SearchScope::ALL.into_iter().enumerate().map(
                            |(index, scope)| {
                                let mut next = search.clone();
                                next.scope = scope;
                                if !scope.supports(next.filter) {
                                    next.filter = SearchFilter::All;
                                }
                                div()
                                    .id(("search-scope", index))
                                    .pb(px(8.))
                                    .border_b_2()
                                    .border_color(if scope == search.scope {
                                        self.theme.accent
                                    } else {
                                        gpui::transparent_black()
                                    })
                                    .text_color(if scope == search.scope {
                                        self.theme.text
                                    } else {
                                        self.theme.muted
                                    })
                                    .cursor_pointer()
                                    .child(scope.label())
                                    .on_click(cx.listener(move |view, _, _, cx| {
                                        view.navigate(Route::Search(next.clone()), cx)
                                    }))
                            },
                        )),
                );
            }
            content = content.child(
                div()
                    .id("search-filters")
                    .mx(px(30.))
                    .mb(px(20.))
                    .flex()
                    .gap(px(22.))
                    .overflow_x_scroll()
                    .children(
                        search
                            .scope
                            .supported_filters()
                            .iter()
                            .copied()
                            .enumerate()
                            .map(|(index, filter)| {
                                let next = Search {
                                    filter,
                                    ..search.clone()
                                };
                                div()
                                    .id(("search-filter", index))
                                    .flex_shrink_0()
                                    .pb(px(9.))
                                    .border_b_2()
                                    .border_color(if filter == search.filter {
                                        self.theme.accent
                                    } else {
                                        gpui::transparent_black()
                                    })
                                    .text_color(if filter == search.filter {
                                        self.theme.text
                                    } else {
                                        self.theme.muted
                                    })
                                    .cursor_pointer()
                                    .child(filter.label())
                                    .on_click(cx.listener(move |view, _, _, cx| {
                                        view.navigate(Route::Search(next.clone()), cx)
                                    }))
                            }),
                    ),
            );
        }
        if self.account.is_some()
            && matches!(
                self.route,
                Route::Saved | Route::Liked | Route::Recent | Route::Library(_)
            )
        {
            let selected = match self.route {
                Route::Liked => Section::Liked,
                Route::Recent => Section::History,
                Route::Library(section) => section,
                _ => Section::Songs,
            };
            content = content.child(
                div()
                    .id("library-sections")
                    .mx(px(30.))
                    .mb(px(20.))
                    .flex()
                    .gap(px(22.))
                    .overflow_x_scroll()
                    .children(
                        [
                            Section::Songs,
                            Section::Albums,
                            Section::Artists,
                            Section::Subscriptions,
                            Section::Playlists,
                            Section::Liked,
                            Section::History,
                            Section::Podcasts,
                            Section::Episodes,
                            Section::Uploads,
                            Section::UploadAlbums,
                            Section::UploadArtists,
                        ]
                        .into_iter()
                        .enumerate()
                        .map(|(index, section)| {
                            div()
                                .id(("library-section", index))
                                .flex_shrink_0()
                                .pb(px(9.))
                                .border_b_2()
                                .border_color(if section == selected {
                                    self.theme.accent
                                } else {
                                    gpui::transparent_black()
                                })
                                .text_color(if section == selected {
                                    self.theme.text
                                } else {
                                    self.theme.muted
                                })
                                .cursor_pointer()
                                .child(section.title())
                                .on_click(cx.listener(move |view, _, _, cx| {
                                    view.navigate(Route::Library(section), cx)
                                }))
                        }),
                    ),
            );
        }
        if let Some(error) = &self.error {
            content = content.child(
                div()
                    .mx(px(30.))
                    .mb(px(16.))
                    .p(px(14.))
                    .rounded(px(6.))
                    .bg(self.theme.raised)
                    .text_color(rgb(0xf1a6ae))
                    .text_size(px(13.))
                    .child(error.clone())
                    .child(
                        div()
                            .id("retry")
                            .mt(px(10.))
                            .text_color(self.theme.text)
                            .cursor_pointer()
                            .child("Dismiss")
                            .on_click(cx.listener(|view, _, _, cx| {
                                view.error = None;
                                cx.notify();
                            })),
                    ),
            );
        }
        if let Some(notice) = &self.notice {
            content = content.child(
                div()
                    .px(px(30.))
                    .pb(px(12.))
                    .text_size(px(12.))
                    .text_color(self.theme.accent)
                    .child(notice.clone()),
            );
        }
        if self.loading && self.rows.is_empty() {
            return content
                .child(
                    div()
                        .px(px(30.))
                        .py(px(25.))
                        .text_color(self.theme.muted)
                        .child("Loading music…"),
                )
                .into_any_element();
        }
        if self.rows.is_empty() {
            let message = match self.route {
                Route::Liked => "Tap the heart on a song to save it here.",
                Route::Recent => "Your recently played songs will appear here.",
                Route::Saved => "Save albums, artists, and playlists from search results.",
                Route::Search(..) => "No results. Try a different song or artist.",
                _ => "No music to show yet.",
            };
            return content
                .child(
                    div()
                        .px(px(30.))
                        .py(px(30.))
                        .text_color(self.theme.muted)
                        .child(message),
                )
                .into_any_element();
        }
        content = if home {
            let mut links = Vec::new();
            let mut shelves = Vec::new();
            for (index, shelf) in self.page.shelves.iter().enumerate() {
                if shelf.title.is_empty()
                    && shelf
                        .items
                        .iter()
                        .all(|item| matches!(item, Item::Navigation(_)))
                {
                    for (i, item) in shelf.items.iter().enumerate() {
                        if let Item::Navigation(link) = item {
                            links.push(
                                self.navigation(
                                    link.clone(),
                                    ("discovery-link", index * 1000 + i),
                                    cx,
                                )
                                .py(px(10.)),
                            );
                        }
                    }
                } else {
                    shelves.push(index);
                }
            }
            content
                .when(!links.is_empty(), |content| {
                    content.child(
                        div()
                            .mx(px(30.))
                            .mb(px(12.))
                            .flex()
                            .flex_wrap()
                            .gap(px(28.))
                            .children(links),
                    )
                })
                .child(
                    uniform_list(
                        ("home-shelves", self.list_id),
                        shelves.len(),
                        cx.processor(move |view, range: std::ops::Range<usize>, _, cx| {
                            range
                                .map(|i| view.shelf(shelves[i], cx))
                                .collect::<Vec<_>>()
                        }),
                    )
                    .flex_1()
                    .min_h_0(),
                )
        } else {
            content.child(
                uniform_list(
                    ("music-results", self.list_id),
                    self.rows.len(),
                    cx.processor(|view, range: std::ops::Range<usize>, _, cx| {
                        range.map(|i| view.row(i, cx)).collect::<Vec<_>>()
                    }),
                )
                .flex_1()
                .min_h_0(),
            )
        };
        if self.page.continuation.is_some() {
            content = content.child(
                div()
                    .id("load-more")
                    .h(px(44.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(self.theme.muted)
                    .cursor_pointer()
                    .child(if self.loading {
                        "Loading…"
                    } else {
                        "Load more"
                    })
                    .on_click(cx.listener(|view, _, _, cx| {
                        if !view.loading
                            && let Some(token) = view.page.continuation.clone()
                        {
                            view.request(
                                Query::More {
                                    token,
                                    search: match &view.route {
                                        Route::Search(search) => Some(search.clone()),
                                        _ => None,
                                    },
                                },
                                cx,
                            );
                        }
                    })),
            );
        }
        content.into_any_element()
    }
    fn queue(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let count = self.library.queue.tracks.len();
        let episodes = self
            .library
            .queue
            .tracks
            .iter()
            .filter(|track| track.details.episode.is_some())
            .count();
        let label = match (count, episodes) {
            (1, 1) => "1 episode".into(),
            (1, 0) => "1 song".into(),
            (_, 0) => format!("{count} songs"),
            _ if episodes == count => format!("{count} episodes"),
            _ => format!("{count} items"),
        };
        div()
            .w(px(304.))
            .h_full()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .border_l_1()
            .border_color(self.theme.border)
            .bg(self.theme.panel)
            .child(
                div()
                    .h(px(80.))
                    .px(px(20.))
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(20.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Up next"),
                    )
                    .child(
                        div()
                            .id("clear-queue")
                            .text_size(px(12.))
                            .text_color(self.theme.muted)
                            .cursor_pointer()
                            .child("Clear")
                            .on_click(cx.listener(|view, _, _, cx| view.clear_queue(cx))),
                    )
                    .child(
                        button("close-queue", "close", self.theme).on_click(cx.listener(
                            |view, _, _, cx| {
                                view.show_queue = false;
                                cx.notify();
                            },
                        )),
                    ),
            )
            .child(
                div()
                    .px(px(20.))
                    .pb(px(18.))
                    .text_size(px(12.))
                    .text_color(self.theme.muted)
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(label)
                    .child(
                        div()
                            .id("autoplay-toggle")
                            .cursor_pointer()
                            .text_color(if self.library.autoplay {
                                self.theme.accent
                            } else {
                                self.theme.muted
                            })
                            .child(if self.library.autoplay {
                                "Autoplay on"
                            } else {
                                "Autoplay off"
                            })
                            .on_click(cx.listener(|view, _, _, cx| view.toggle_autoplay(cx))),
                    ),
            )
            .child(
                uniform_list(
                    "queue-list",
                    self.library.queue.tracks.len() + self.autoplay_tracks.len(),
                    cx.processor(|view, range: std::ops::Range<usize>, _, cx| {
                        range
                            .map(|index| {
                                if index >= view.library.queue.tracks.len() {
                                    let index = index - view.library.queue.tracks.len();
                                    let track = view.autoplay_tracks[index].clone();
                                    let art = view.art(track.thumbnail.as_deref(), 40.);
                                    return div()
                                        .id(("autoplay-row", index))
                                        .h(px(64.))
                                        .px(px(18.))
                                        .flex()
                                        .items_center()
                                        .gap(px(10.))
                                        .cursor_pointer()
                                        .hover(|d| d.bg(view.theme.raised))
                                        .on_click(cx.listener(move |view, _, _, cx| {
                                            view.play_recommendation(index, cx)
                                        }))
                                        .child(art)
                                        .child(
                                            div()
                                                .flex_1()
                                                .min_w_0()
                                                .flex()
                                                .flex_col()
                                                .gap(px(4.))
                                                .child(
                                                    div()
                                                        .truncate()
                                                        .text_size(px(13.))
                                                        .child(track.title),
                                                )
                                                .child(
                                                    div()
                                                        .truncate()
                                                        .text_size(px(11.))
                                                        .text_color(view.theme.muted)
                                                        .child(format!(
                                                            "Autoplay · {}",
                                                            track.artist
                                                        )),
                                                ),
                                        )
                                        .child(
                                            button(("remove-autoplay", index), "close", view.theme)
                                                .w(px(26.))
                                                .on_click(cx.listener(move |view, _, _, cx| {
                                                    cx.stop_propagation();
                                                    view.autoplay_tracks.remove(index);
                                                    cx.notify();
                                                })),
                                        )
                                        .into_any_element();
                                }
                                let track = view.library.queue.tracks[index].clone();
                                let active = view.library.queue.index == Some(index);
                                let art = view.art(track.thumbnail.as_deref(), 40.);
                                div()
                                    .id(("queue-row", index))
                                    .w_full()
                                    .h(px(64.))
                                    .px(px(18.))
                                    .rounded(px(5.))
                                    .flex()
                                    .items_center()
                                    .gap(px(10.))
                                    .cursor_pointer()
                                    .when(active, |d| d.bg(view.theme.selection))
                                    .hover(|d| d.bg(view.theme.raised))
                                    .on_click(cx.listener(move |view, _, _, cx| {
                                        view.library.queue.select(index);
                                        view.play_current(cx);
                                    }))
                                    .child(art)
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .flex()
                                            .flex_col()
                                            .gap(px(4.))
                                            .child(
                                                div()
                                                    .truncate()
                                                    .text_size(px(13.))
                                                    .when(active, |d| {
                                                        d.text_color(view.theme.accent)
                                                    })
                                                    .child(track.title),
                                            )
                                            .child(
                                                div()
                                                    .truncate()
                                                    .text_size(px(11.))
                                                    .text_color(view.theme.muted)
                                                    .child(track.artist),
                                            ),
                                    )
                                    .child(div().flex().gap(px(2.)).children(
                                        [(-1isize, "↑"), (1, "↓")].into_iter().enumerate().map(
                                            |(key, (direction, label))| {
                                                let target = index
                                                    .checked_add_signed(direction)
                                                    .filter(|target| {
                                                        *target < view.library.queue.tracks.len()
                                                    });
                                                div()
                                                    .id(("queue-move", index * 2 + key))
                                                    .w(px(22.))
                                                    .h(px(30.))
                                                    .flex()
                                                    .items_center()
                                                    .justify_center()
                                                    .text_size(px(16.))
                                                    .cursor_pointer()
                                                    .child(label)
                                                    .opacity(if target.is_some() {
                                                        1.
                                                    } else {
                                                        0.3
                                                    })
                                                    .on_click(cx.listener(move |view, _, _, cx| {
                                                        cx.stop_propagation();
                                                        if let Some(target) = target
                                                            && view
                                                                .library
                                                                .queue
                                                                .move_track(index, target)
                                                        {
                                                            view.save();
                                                            cx.notify();
                                                        }
                                                    }))
                                            },
                                        ),
                                    ))
                                    .child(
                                        button(("remove-queue", index), "close", view.theme)
                                            .w(px(26.))
                                            .on_click(cx.listener(move |view, _, _, cx| {
                                                cx.stop_propagation();
                                                let playing = matches!(
                                                    view.playback,
                                                    Playback::Playing
                                                        | Playback::Buffering
                                                        | Playback::Loading { paused: false, .. }
                                                );
                                                let current = view.library.queue.remove(index);
                                                if current {
                                                    view.autoplay_waiting = None;
                                                    view.player_command(Player::stop);
                                                    view.active_id = None;
                                                    view.playback = Playback::Stopped;
                                                    view.position = 0.;
                                                    view.duration = 0.;
                                                    if playing {
                                                        view.play_current(cx);
                                                    }
                                                }
                                                if view.library.queue.tracks.is_empty() {
                                                    view.clear_queue(cx);
                                                }
                                                view.save();
                                                cx.notify();
                                            })),
                                    )
                                    .into_any_element()
                            })
                            .collect::<Vec<_>>()
                    }),
                )
                .flex_1()
                .min_h_0(),
            )
            .when(self.library.queue.tracks.is_empty(), |d| {
                d.child(
                    div()
                        .px(px(20.))
                        .text_color(self.theme.muted)
                        .text_size(px(13.))
                        .child("Play a song or add one with +."),
                )
            })
            .into_any_element()
    }
    fn player_bar(&mut self, compact: bool, cx: &mut Context<Self>) -> AnyElement {
        let current = self.library.queue.current().cloned();
        let mut details = div()
            .w(px(if compact { 230. } else { 310. }))
            .min_w_0()
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap(px(12.));
        if let Some(track) = current {
            let art = self.art(track.thumbnail.as_deref(), 52.);
            let menu_track = track.clone();
            let liked = self.is_liked(&track);
            let subtitle = match self.playback {
                Playback::Loading { .. } => "Loading track…".into(),
                Playback::Buffering => "Buffering…".into(),
                _ => track.artist.clone(),
            };
            details = details
                .child(art)
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap(px(6.))
                        .child(
                            div()
                                .truncate()
                                .text_size(px(14.))
                                .font_weight(FontWeight::MEDIUM)
                                .child(track.title.clone()),
                        )
                        .child(
                            div()
                                .truncate()
                                .text_size(px(12.))
                                .text_color(self.theme.muted)
                                .child(subtitle),
                        ),
                )
                .child(
                    button(
                        "like-current",
                        if liked { "heart-filled" } else { "heart" },
                        self.theme,
                    )
                    .when(liked, |d| d.text_color(self.theme.accent))
                    .on_click(cx.listener(move |view, _, _, cx| view.toggle_like(&track, cx))),
                )
                .child(
                    div()
                        .id("current-song-menu")
                        .w(px(26.))
                        .h(px(32.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .cursor_pointer()
                        .text_size(px(22.))
                        .child("⋯")
                        .on_click(cx.listener(move |view, _, window, cx| {
                            window.focus(&view.focus, cx);
                            view.overlay = Some(Overlay::Track {
                                track: menu_track.clone(),
                                index: None,
                            });
                            cx.notify();
                        })),
                );
        } else {
            details = details.child(
                div()
                    .text_color(self.theme.muted)
                    .text_size(px(13.))
                    .child("Choose something to play"),
            );
        }
        let progress = if self.duration > 0. {
            (self
                .drag_position
                .unwrap_or_else(|| self.playback_position())
                / self.duration)
                .clamp(0., 1.) as f32
        } else {
            0.
        };
        let seek_bounds = self.seek_bounds.clone();
        let volume_bounds = self.volume_bounds.clone();
        let playing = self.active_id.is_some()
            && self.playback != Playback::Stopped
            && !self.playback.is_paused();
        div()
            .h(px(106.))
            .flex_shrink_0()
            .px(px(24.))
            .border_t_1()
            .border_color(self.theme.border)
            .bg(self.theme.panel)
            .flex()
            .items_center()
            .gap(px(24.))
            .child(details)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .max_w(px(650.))
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(14.))
                            .child(
                                button("shuffle", "shuffle", self.theme).on_click(cx.listener(
                                    |view, _, _, cx| {
                                        view.library.queue.shuffle_upcoming();
                                        view.notice = Some("Upcoming songs shuffled".into());
                                        view.save();
                                        cx.notify();
                                    },
                                )),
                            )
                            .child(
                                button("previous", "previous", self.theme)
                                    .on_click(cx.listener(|view, _, _, cx| view.previous(cx))),
                            )
                            .child(
                                button(
                                    "play-pause",
                                    if playing { "pause" } else { "play" },
                                    self.theme,
                                )
                                .size(px(40.))
                                .rounded_full()
                                .bg(self.theme.text)
                                .text_color(self.theme.bg)
                                .on_click(cx.listener(|view, _, _, cx| view.toggle_play(cx))),
                            )
                            .child(
                                button("next", "next", self.theme)
                                    .on_click(cx.listener(|view, _, _, cx| view.next(false, cx))),
                            )
                            .child(
                                button("repeat", "repeat", self.theme)
                                    .when(self.library.repeat != Repeat::Off, |d| {
                                        d.text_color(self.theme.accent)
                                    })
                                    .when(self.library.repeat == Repeat::One, |d| {
                                        d.child(
                                            div()
                                                .absolute()
                                                .top_0()
                                                .right_0()
                                                .text_size(px(9.))
                                                .child("1"),
                                        )
                                    })
                                    .on_click(cx.listener(|view, _, _, cx| {
                                        view.library.repeat = view.library.repeat.next();
                                        view.save();
                                        cx.notify();
                                    })),
                            ),
                    )
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .items_center()
                            .gap(px(11.))
                            .text_size(px(11.))
                            .text_color(self.theme.muted)
                            .child(
                                div().w(px(56.)).flex_shrink_0().text_right().child(clock(
                                    self.drag_position
                                        .unwrap_or_else(|| self.playback_position()),
                                )),
                            )
                            .child(
                                div()
                                    .id("seek-track")
                                    .h(px(16.))
                                    .flex_1()
                                    .cursor_pointer()
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(|view, event: &MouseDownEvent, _, cx| {
                                            view.drag = Some(Drag::Seek);
                                            view.drag_to(event.position, cx);
                                        }),
                                    )
                                    .child(slider(progress, seek_bounds, self.theme)),
                            )
                            .child(div().w(px(56.)).flex_shrink_0().child(clock(self.duration))),
                    ),
            )
            .child(div().flex_1().max_w(px(30.)))
            .child(
                div()
                    .w(px(if compact { 158. } else { 208. }))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .child(
                        button("queue-toggle", "queue", self.theme)
                            .when(self.show_queue, |d| d.text_color(self.theme.accent))
                            .on_click(cx.listener(|view, _, _, cx| {
                                view.show_queue = !view.show_queue;
                                cx.notify();
                            })),
                    )
                    .child(
                        button(
                            "mute-toggle",
                            if self.library.volume == 0. {
                                "mute"
                            } else {
                                "volume"
                            },
                            self.theme,
                        )
                        .on_click(cx.listener(|view, _, _, cx| {
                            view.set_volume(if view.library.volume > 0. { 0. } else { 60. }, cx);
                            view.save();
                        })),
                    )
                    .child(
                        div()
                            .id("volume-track")
                            .flex_1()
                            .h(px(18.))
                            .cursor_pointer()
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|view, event: &MouseDownEvent, _, cx| {
                                    view.drag = Some(Drag::Volume);
                                    view.drag_to(event.position, cx);
                                }),
                            )
                            .child(slider(
                                (self.library.volume / 100.) as f32,
                                volume_bounds,
                                self.theme,
                            )),
                    ),
            )
            .into_any_element()
    }
    fn menu_action(
        &self,
        id: impl Into<ElementId>,
        label: impl Into<SharedString>,
        cx: &Context<Self>,
        action: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> impl IntoElement {
        div()
            .id(id)
            .w_full()
            .px(px(10.))
            .py(px(11.))
            .rounded(px(5.))
            .cursor_pointer()
            .hover(|d| d.bg(self.theme.raised))
            .child(label.into())
            .on_click(cx.listener(move |view, _, window, cx| action(view, window, cx)))
    }
    fn backdrop(&self, body: AnyElement, cx: &Context<Self>) -> AnyElement {
        div()
            .absolute()
            .inset_0()
            .bg(gpui::black().opacity(0.65))
            .flex()
            .items_center()
            .justify_center()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|view, _, window, cx| view.close_overlay(window, cx)),
            )
            .child(body)
            .into_any_element()
    }
    fn overlay(&self, kind: &Overlay, cx: &Context<Self>) -> AnyElement {
        if let Overlay::CountryPicker(picker) = kind {
            return self.backdrop(picker.clone().into_any_element(), cx);
        }
        if let Overlay::PlaylistForm { form, .. } = kind {
            let mut body = div().flex().flex_col().gap(px(12.)).child(form.clone());
            if let Some(error) = &self.error {
                body = body.child(
                    div()
                        .max_w(px(480.))
                        .p(px(12.))
                        .bg(self.theme.panel)
                        .text_color(rgb(0xf1a6ae))
                        .child(error.clone()),
                );
            }
            return self.backdrop(body.into_any_element(), cx);
        }
        let mut body = div()
            .id("overlay-body")
            .max_h(relative(0.9))
            .overflow_y_scroll()
            .w(px(480.))
            .p(px(28.))
            .rounded(px(12.))
            .bg(self.theme.panel)
            .border_1()
            .border_color(self.theme.border)
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .mb(px(24.))
                    .gap(px(12.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(23.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(match kind {
                                Overlay::Help => "Keyboard shortcuts".to_owned(),
                                Overlay::Settings => "Settings".into(),
                                Overlay::Description { .. } => "Description".into(),
                                Overlay::Chapters(_) => "Chapters".into(),
                                Overlay::Track { track, .. } => track.title.clone(),
                                Overlay::PlaylistPicker { .. } => "Add to playlist".into(),
                                Overlay::DeletePlaylist { .. } => "Delete playlist?".into(),
                                Overlay::SongText { title, .. } => title.clone(),
                                Overlay::PlaylistForm { .. } | Overlay::CountryPicker(_) => {
                                    unreachable!()
                                }
                            }),
                    )
                    .child(button("close-overlay", "close", self.theme).on_click(
                        cx.listener(|view, _, window, cx| view.close_overlay(window, cx)),
                    )),
            );
        if matches!(kind, Overlay::Help) {
            for (key, action) in [
                ("Ctrl K", "Search"),
                ("Space", "Play or pause"),
                ("Ctrl → / Ctrl ←", "Next / previous song"),
                ("→ / ←", "Seek 5 seconds"),
                ("↑ / ↓", "Adjust volume"),
                ("L", "Like the current song"),
                ("Q", "Show the queue"),
                ("?", "Keyboard shortcuts"),
                ("Ctrl Q", "Quit"),
            ] {
                body = body.child(
                    div()
                        .py(px(9.))
                        .flex()
                        .justify_between()
                        .text_size(px(14.))
                        .child(action)
                        .child(div().text_color(self.theme.muted).child(key)),
                );
            }
        } else if matches!(kind, Overlay::Settings) {
            let mut speeds = div().mt(px(12.)).flex().flex_wrap().gap(px(4.));
            for (index, speed) in PlaybackSpeed::ALL.into_iter().enumerate() {
                speeds = speeds.child(
                    div()
                        .id(("playback-speed", index))
                        .w(px(44.))
                        .py(px(10.))
                        .text_center()
                        .text_size(px(13.))
                        .border_b_2()
                        .border_color(if self.library.speed == speed {
                            self.theme.accent
                        } else {
                            self.theme.panel
                        })
                        .when(self.library.speed == speed, |button| {
                            button.text_color(self.theme.accent)
                        })
                        .hover(|button| button.bg(self.theme.raised))
                        .cursor_pointer()
                        .child(format!("{}×", f64::from(speed)))
                        .on_click(cx.listener(move |view, _, _, cx| {
                            view.library.speed = speed;
                            view.player_command(|player| player.set_speed(speed));
                            view.save();
                            cx.notify();
                        })),
                );
            }
            body = body
                .child(div().text_size(px(15.)).child("Playback speed"))
                .child(speeds);
            let mut qualities = div().mt(px(12.)).flex().gap(px(12.));
            for (index, quality) in AudioQuality::ALL.into_iter().enumerate() {
                qualities = qualities.child(
                    div()
                        .id(("audio-quality", index))
                        .px(px(14.))
                        .py(px(10.))
                        .text_size(px(13.))
                        .border_b_2()
                        .border_color(if self.library.quality == quality {
                            self.theme.accent
                        } else {
                            self.theme.panel
                        })
                        .when(self.library.quality == quality, |button| {
                            button.text_color(self.theme.accent)
                        })
                        .hover(|button| button.bg(self.theme.raised))
                        .cursor_pointer()
                        .child(quality.label())
                        .on_click(cx.listener(move |view, _, _, cx| {
                            view.set_audio_quality(quality, cx);
                        })),
                );
            }
            body = body
                .child(div().mt(px(24.)).text_size(px(15.)).child("Audio quality"))
                .child(qualities)
                .child(
                    div()
                        .mt(px(12.))
                        .text_size(px(13.))
                        .text_color(self.theme.muted)
                        .child("Available quality depends on the song and your membership."),
                );
            let label = self
                .account
                .as_ref()
                .map(|a| format!("{} {}", a.account.name, a.account.handle))
                .unwrap_or_else(|| "Connect your Google account to sync with YouTube Music".into());
            body=body.child(div().mt(px(28.)).text_size(px(15.)).child(label))
                .child(div().id("connect-account").mt(px(22.)).px(px(15.)).py(px(11.)).bg(self.theme.text).text_color(self.theme.bg).rounded(px(5.)).cursor_pointer()
                    .child(if !auth::ACCOUNT_LOGIN_ENABLED {"Sign-in unavailable during security review"}else if matches!(self.account_operation, Some(AccountOperation::Disconnecting(_))) {"Signing out…"}else if self.account_operation.is_some(){"Connecting account…"}else if self.account.is_some(){"Switch or reconnect account"}else{"Connect Google account"})
                    .on_click(cx.listener(|view,_,_,cx|view.connect_account(cx))))
                .when(self.account.is_some(), |body| body.child(div().id("disconnect-account").mt(px(16.)).text_color(self.theme.accent).cursor_pointer().child("Sign out").on_click(cx.listener(|view,_,_,cx|view.disconnect_account(cx)))))
                .child(div().mt(px(16.)).text_size(px(13.)).text_color(self.theme.muted).child(if self.account.is_some(){"Library changes are saved to your Google account. Returning to this window loads changes made on the web."}else{"Guest likes and saved collections stay on this computer. Connecting shows your account library."}))
                .child(div().id("settings-open-web").mt(px(24.)).text_color(self.theme.accent).cursor_pointer().child("Open YouTube Music").on_click(|_,_,cx|cx.open_url("https://music.youtube.com")));
        } else if let Overlay::Description { title, text } = kind {
            body = body
                .child(
                    div()
                        .text_size(px(18.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(title.clone()),
                )
                .child(div().mt(px(18.)).text_size(px(14.)).child(text.clone()));
        } else if let Overlay::Chapters(track) = kind {
            body = body.child(
                div()
                    .mb(px(18.))
                    .text_size(px(18.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(track.title.clone()),
            );
            if let Some(episode) = &track.details.episode {
                let selected = Rc::new((**track).clone());
                for (index, chapter) in episode.chapters.iter().enumerate() {
                    let selected = selected.clone();
                    let seconds = chapter.start_seconds;
                    body = body.child(
                        div()
                            .id(("chapter", index))
                            .py(px(12.))
                            .px(px(8.))
                            .flex()
                            .items_start()
                            .gap(px(14.))
                            .text_size(px(14.))
                            .cursor_pointer()
                            .hover(|style| style.bg(self.theme.raised))
                            .child(
                                div()
                                    .w(px(64.))
                                    .flex_shrink_0()
                                    .text_color(self.theme.muted)
                                    .child(clock(seconds as f64)),
                            )
                            .child(div().flex_1().min_w_0().child(chapter.title.clone()))
                            .on_click(cx.listener(move |view, _, _, cx| {
                                view.play_chapter((*selected).clone(), seconds, cx);
                            })),
                    );
                }
            }
        } else if let Overlay::Track { track, index } = kind {
            if let Some(episode) = &track.details.episode {
                if !episode.subtitle.is_empty() {
                    body = body.child(
                        div()
                            .mb(px(16.))
                            .text_color(self.theme.muted)
                            .child(episode.subtitle.clone()),
                    );
                }
                let target = BrowseLink::new(format!("MPED{}", track.id), track.title.clone());
                body = body.child(self.menu_action(
                    "menu-episode",
                    "Episode details",
                    cx,
                    move |view, _, cx| {
                        view.overlay = None;
                        view.navigate(Route::Browse(target.clone()), cx);
                    },
                ));
                if !episode.chapters.is_empty() {
                    let track = track.clone();
                    body = body.child(self.menu_action(
                        "menu-chapters",
                        "Chapters",
                        cx,
                        move |view, window, cx| {
                            window.focus(&view.focus, cx);
                            view.overlay = Some(Overlay::Chapters(Box::new(track.clone())));
                            cx.notify();
                        },
                    ));
                }
                if let Some(id) = &episode.show_id {
                    let target = BrowseLink::new(id.clone(), track.artist.clone());
                    body = body.child(self.menu_action(
                        "menu-show",
                        "Go to podcast",
                        cx,
                        move |view, _, cx| {
                            view.overlay = None;
                            view.navigate(Route::Browse(target.clone()), cx);
                        },
                    ));
                }
            }
            if track.details.episode.is_none() {
                let song = track.clone();
                body = body.child(self.menu_action(
                    "menu-radio",
                    "Start radio",
                    cx,
                    move |view, _, cx| {
                        view.overlay = None;
                        view.reset_radio();
                        view.player_command(Player::stop);
                        view.active_id = None;
                        view.playback = Playback::Loading {
                            paused: false,
                            retried: false,
                        };
                        view.now_playing = None;
                        view.notice = Some("Loading radio…".into());
                        view.playback_request(
                            PlaybackQuery::Watch {
                                track: Box::new(song.clone()),
                                radio: true,
                            },
                            cx,
                        );
                    },
                ));
            }
            if let Some(watch) = &self.now_playing
                && watch
                    .current
                    .as_ref()
                    .is_some_and(|current| current.id == track.id)
            {
                if let Some(id) = &watch.lyrics {
                    let id = id.clone();
                    let title = track.title.clone();
                    body = body.child(self.menu_action(
                        "menu-lyrics",
                        "Lyrics",
                        cx,
                        move |view, _, cx| {
                            view.show_song_text(TextQuery::Lyrics(id.clone()), &title, cx);
                        },
                    ));
                }
                if let Some(id) = &watch.related {
                    let id = id.clone();
                    body = body.child(self.menu_action(
                        "menu-related",
                        "Related music",
                        cx,
                        move |view, _, cx| {
                            view.overlay = None;
                            view.navigate(
                                Route::Browse(BrowseLink::new(id.clone(), "Related music")),
                                cx,
                            );
                        },
                    ));
                }
            }
            if let Some(id) = track.credits_id() {
                let title = track.title.clone();
                body = body.child(self.menu_action(
                    "menu-credits",
                    "Song credits",
                    cx,
                    move |view, _, cx| {
                        view.show_song_text(TextQuery::Credits(id.clone()), &title, cx)
                    },
                ));
            }
            let song = track.clone();
            body = body.child(self.menu_action(
                "menu-like",
                if self.is_liked(track) {
                    "Remove like"
                } else {
                    "Like"
                },
                cx,
                move |view, _, cx| {
                    view.overlay = None;
                    view.toggle_like(&song, cx);
                },
            ));
            let song = track.clone();
            body =
                body.child(
                    self.menu_action("menu-next", "Play next", cx, move |view, _, cx| {
                        let index = view.library.queue.index.map_or(0, |i| i + 1);
                        view.library.queue.tracks.insert(index, song.clone());
                        view.resume_autoplay(cx);
                        view.overlay = None;
                        view.save();
                        cx.notify();
                    }),
                );
            let song = track.clone();
            body = body.child(self.menu_action(
                "menu-enqueue",
                "Add to queue",
                cx,
                move |view, _, cx| {
                    view.overlay = None;
                    view.enqueue(song.clone(), cx);
                },
            ));
            if self.account.is_some() {
                let song = track.clone();
                let disliked = self
                    .ratings
                    .get(&track.id)
                    .copied()
                    .or(track.details.rating)
                    == Some(Rating::Dislike);
                body = body.child(self.menu_action(
                    "menu-dislike",
                    if disliked {
                        "Remove dislike"
                    } else {
                        "Dislike"
                    },
                    cx,
                    move |view, _, cx| {
                        view.overlay = None;
                        view.mutate(
                            Mutation::RateSong {
                                track: song.clone(),
                                rating: if disliked {
                                    Rating::Indifferent
                                } else {
                                    Rating::Dislike
                                },
                            },
                            cx,
                        );
                    },
                ));
                if track.details.add_library.is_some() || track.details.remove_library.is_some() {
                    let song = track.clone();
                    let save = track.details.in_library != Some(true);
                    body = body.child(self.menu_action(
                        "menu-library",
                        if save {
                            "Save to library"
                        } else {
                            "Remove from library"
                        },
                        cx,
                        move |view, _, cx| {
                            view.overlay = None;
                            view.mutate(
                                Mutation::SongLibrary {
                                    track: song.clone(),
                                    save,
                                },
                                cx,
                            );
                        },
                    ));
                }
                let song = track.clone();
                body = body.child(self.menu_action(
                    "menu-playlist",
                    "Add to playlist",
                    cx,
                    move |view, _, cx| view.playlist_picker(song.clone(), cx),
                ));
                if let Some(token) = &track.details.remove_history {
                    let token = token.clone();
                    body = body.child(self.menu_action(
                        "menu-history",
                        "Remove from history",
                        cx,
                        move |view, _, cx| {
                            view.overlay = None;
                            view.mutate(
                                Mutation::RemoveHistory {
                                    token: token.clone(),
                                },
                                cx,
                            );
                        },
                    ));
                }
                if self.page.editable
                    && let Route::Browse(link) = &self.route
                    && let Some(set_id) = &track.details.set_video_id
                {
                    let playlist = link.id.clone();
                    let set = set_id.clone();
                    let video_id = track.id.clone();
                    body = body.child(self.menu_action(
                        "menu-remove-occurrence",
                        "Remove from playlist",
                        cx,
                        move |view, _, cx| {
                            view.overlay = None;
                            view.mutate(
                                Mutation::RemoveFromPlaylist {
                                    id: playlist.clone(),
                                    video_id: video_id.clone(),
                                    set_video_id: set.clone(),
                                },
                                cx,
                            );
                        },
                    ));
                    if let Some(index) = index {
                        let tracks = self.page.tracks();
                        for (key, label, before) in [
                            (
                                "menu-move-up",
                                "Move up",
                                index
                                    .checked_sub(1)
                                    .and_then(|i| tracks.get(i))
                                    .and_then(|track| track.details.set_video_id.clone())
                                    .map(Some),
                            ),
                            (
                                "menu-move-down",
                                "Move down",
                                if index + 1 < tracks.len() {
                                    Some(
                                        tracks
                                            .get(index + 2)
                                            .and_then(|track| track.details.set_video_id.clone()),
                                    )
                                } else {
                                    None
                                },
                            ),
                        ] {
                            if let Some(before) = before {
                                let playlist = link.id.clone();
                                let set = set_id.clone();
                                body = body.child(self.menu_action(
                                    key,
                                    label,
                                    cx,
                                    move |view, _, cx| {
                                        view.overlay = None;
                                        view.mutate(
                                            Mutation::MoveInPlaylist {
                                                id: playlist.clone(),
                                                set_video_id: set.clone(),
                                                before: before.clone(),
                                            },
                                            cx,
                                        );
                                    },
                                ));
                            }
                        }
                    }
                }
            }
            for (id, label, title, target) in [
                (
                    "menu-artist",
                    if track.details.episode.is_some() {
                        "Go to creator"
                    } else {
                        "Go to artist"
                    },
                    "Artist",
                    &track.details.artist_id,
                ),
                (
                    "menu-album",
                    "Go to album",
                    "Album",
                    &track.details.album_id,
                ),
            ] {
                if let Some(target) = target {
                    let target = target.clone();
                    body = body.child(self.menu_action(id, label, cx, move |view, _, cx| {
                        view.overlay = None;
                        view.navigate(Route::Browse(BrowseLink::new(target.clone(), title)), cx);
                    }));
                }
            }
            let url = format!("https://music.youtube.com/watch?v={}", track.id);
            body = body.child(self.menu_action(
                "menu-share",
                if track.details.episode.is_some() {
                    "Copy episode link"
                } else {
                    "Copy song link"
                },
                cx,
                move |view, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(url.clone()));
                    view.overlay = None;
                    view.notice = Some("Link copied".into());
                    cx.notify();
                },
            ));
        } else if let Overlay::SongText { content, .. } = kind {
            match content {
                None => body = body.child(div().text_color(self.theme.muted).child("Loading…")),
                Some(Err(error)) => {
                    body = body.child(div().text_color(self.theme.muted).child(error.clone()))
                }
                Some(Ok(SongText::Lyrics(text))) => {
                    body = body.child(div().text_size(px(17.)).child(text.clone()))
                }
                Some(Ok(SongText::Credits(credits))) => {
                    for section in credits {
                        body = body.child(
                            div()
                                .mb(px(22.))
                                .child(
                                    div()
                                        .text_size(px(16.))
                                        .font_weight(FontWeight::MEDIUM)
                                        .child(section.title.clone()),
                                )
                                .child(
                                    div()
                                        .mt(px(8.))
                                        .text_size(px(14.))
                                        .text_color(self.theme.muted)
                                        .child(section.names.clone()),
                                ),
                        );
                    }
                }
            }
        } else if let Overlay::PlaylistPicker {
            track,
            page,
            duplicates,
        } = kind
        {
            let song = track.clone();
            body = body.child(self.menu_action(
                "create-from-picker",
                "New playlist",
                cx,
                move |view, window, cx| view.playlist_form(false, Some(song.clone()), window, cx),
            ));
            body = body.child(self.menu_action(
                "allow-duplicates",
                if *duplicates {
                    "Allow duplicates: on"
                } else {
                    "Allow duplicates: off"
                },
                cx,
                |view, _, cx| {
                    if let Some(Overlay::PlaylistPicker { duplicates, .. }) = &mut view.overlay {
                        *duplicates = !*duplicates;
                        cx.notify();
                    }
                },
            ));
            if let Some(page) = page {
                let choices = page
                    .shelves
                    .iter()
                    .flat_map(|shelf| &shelf.items)
                    .filter_map(|item| match item {
                        Item::Collection(collection) if collection.id.starts_with("VL") => {
                            Some(collection)
                        }
                        _ => None,
                    });
                let mut list = div()
                    .id("playlist-choices")
                    .max_h(px(360.))
                    .overflow_y_scroll();
                for (index, playlist) in choices.enumerate() {
                    let id = playlist.id.clone();
                    let song = track.id.clone();
                    let duplicates = *duplicates;
                    list = list.child(self.menu_action(
                        ("playlist-choice", index),
                        playlist.title.clone(),
                        cx,
                        move |view, _, cx| {
                            view.overlay = None;
                            view.mutate(
                                Mutation::AddToPlaylist {
                                    id: id.clone(),
                                    video_ids: vec![song.clone()],
                                    duplicates,
                                },
                                cx,
                            );
                        },
                    ));
                }
                body = body.child(list);
                if let Some(token) = &page.continuation {
                    let token = token.clone();
                    body = body.child(self.menu_action(
                        "more-playlist-choices",
                        if self.loading {
                            "Loading…"
                        } else {
                            "More playlists"
                        },
                        cx,
                        move |view, _, cx| {
                            if !view.loading {
                                view.request(Query::PlaylistChoicesMore(token.clone()), cx);
                            }
                        },
                    ));
                }
            } else {
                body = body.child(
                    div()
                        .p(px(12.))
                        .text_color(self.theme.muted)
                        .child("Loading playlists…"),
                );
            }
            if let Some(error) = &self.error {
                body = body.child(
                    div()
                        .mt(px(12.))
                        .text_color(rgb(0xf1a6ae))
                        .child(error.clone()),
                );
            }
        } else if let Overlay::DeletePlaylist { id, title } = kind {
            body = body.child(div().mb(px(18.)).child(format!(
                "Delete {title}? This removes the playlist from your YouTube Music account."
            )));
            let id = id.clone();
            body = body.child(self.menu_action(
                "confirm-delete",
                "Delete playlist",
                cx,
                move |view, _, cx| view.mutate(Mutation::DeletePlaylist { id: id.clone() }, cx),
            ));
            body = body.child(
                self.menu_action("cancel-delete", "Cancel", cx, |view, _, cx| {
                    view.overlay = None;
                    cx.notify();
                }),
            );
            if let Some(error) = &self.error {
                body = body.child(
                    div()
                        .mt(px(12.))
                        .text_color(rgb(0xf1a6ae))
                        .child(error.clone()),
                );
            }
        }
        self.backdrop(body.into_any_element(), cx)
    }
}

impl Drop for Music {
    fn drop(&mut self) {
        if let Some(AccountOperation::Connecting(operation)) = &self.account_operation {
            operation.cancel();
        }
        if matches!(
            self.account_operation,
            Some(AccountOperation::Disconnecting(_))
        ) && let Some(worker) = self.account_worker.take()
            && let Err(error) = worker.finish()
        {
            eprintln!("Could not finish account sign-out: {error:#}");
        }
    }
}

impl Render for Music {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let compact = window.viewport_size().width < px(1120.);
        let sidebar = self.sidebar(compact, cx).into_any_element();
        let content = self.content(cx);
        let queue = self.show_queue.then(|| self.queue(cx));
        let player = self.player_bar(compact, cx);
        div()
            .size_full()
            .relative()
            .bg(self.theme.bg)
            .text_color(self.theme.text)
            .text_size(px(14.))
            .font_family("Inter")
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
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .child(sidebar)
                    .child(content)
                    .children(queue),
            )
            .child(player)
            .children(self.overlay.as_ref().map(|kind| self.overlay(kind, cx)))
    }
}
fn icon(name: &'static str, edge: f32, color: Hsla) -> Svg {
    svg().path(name).size(px(edge)).text_color(color)
}
struct Hint {
    text: &'static str,
    theme: Theme,
}
impl Render for Hint {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .px(px(10.))
            .py(px(7.))
            .rounded(px(5.))
            .bg(self.theme.raised)
            .text_color(self.theme.text)
            .text_size(px(12.))
            .child(self.text)
    }
}
fn button(id: impl Into<ElementId>, name: &'static str, theme: Theme) -> Stateful<Div> {
    let label = match name {
        "play" => "Play",
        "pause" => "Pause",
        "previous" => "Previous song",
        "next" => "Next song",
        "shuffle" => "Shuffle upcoming songs",
        "repeat" => "Cycle repeat: off, all, one",
        "heart" => "Like song",
        "heart-filled" => "Remove from liked music",
        "queue" => "Show queue",
        "volume" => "Mute",
        "mute" => "Unmute",
        "plus" => "Add",
        "back" => "Go back",
        "help" => "Keyboard shortcuts",
        "settings" => "Settings",
        "close" => "Close or remove",
        _ => "Open",
    };
    div()
        .id(id)
        .relative()
        .size(px(34.))
        .flex_shrink_0()
        .rounded(px(6.))
        .flex()
        .items_center()
        .justify_center()
        .text_color(theme.muted)
        .cursor_pointer()
        .tooltip(move |_, cx| cx.new(|_| Hint { text: label, theme }).into())
        .hover(move |d| {
            let background = if matches!(name, "play" | "pause") {
                theme.text
            } else {
                theme.raised
            };
            d.bg(background)
        })
        .child(icon(
            name,
            19.,
            if matches!(name, "play" | "pause") {
                theme.bg
            } else if name == "heart-filled" {
                theme.accent
            } else {
                theme.text
            },
        ))
}
fn slider(ratio: f32, bounds: Rc<Cell<Bounds<Pixels>>>, theme: Theme) -> impl IntoElement {
    canvas(
        move |rect, _, _| {
            bounds.set(rect);
            rect
        },
        move |rect, _, window, _| {
            let y = rect.top() + rect.size.height / 2. - px(2.);
            window.paint_quad(fill(
                Bounds::new(point(rect.left(), y), size(rect.size.width, px(3.))),
                theme.border,
            ));
            window.paint_quad(fill(
                Bounds::new(point(rect.left(), y), size(rect.size.width * ratio, px(3.))),
                theme.accent,
            ));
            window.paint_quad(fill(
                Bounds::new(
                    point(rect.left() + rect.size.width * ratio - px(3.), y - px(1.5)),
                    size(px(6.), px(6.)),
                ),
                theme.text,
            ));
        },
    )
    .size_full()
}
fn clock(seconds: f64) -> String {
    let seconds = seconds.max(0.) as u64;
    if seconds >= 3600 {
        format!(
            "{}:{:02}:{:02}",
            seconds / 3600,
            (seconds / 60) % 60,
            seconds % 60
        )
    } else {
        format!("{}:{:02}", seconds / 60, seconds % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::Playback;

    #[test]
    fn a_load_retries_once_and_pause_changes_do_not_reset_its_budget() {
        for paused in [false, true] {
            let mut playback = Playback::Loading {
                paused,
                retried: false,
            };
            assert!(playback.retry());
            assert_eq!(playback.is_paused(), paused);
            playback = playback.with_pause(!paused);
            assert_eq!(playback.is_paused(), !paused);
            assert!(!playback.retry());
        }
        for mut playback in [
            Playback::Stopped,
            Playback::Playing,
            Playback::Paused,
            Playback::Buffering,
        ] {
            assert!(!playback.retry());
        }
    }
}
