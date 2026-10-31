use crate::{
    account::{self, Connection, Effect, Mutation, Privacy, Section},
    artwork::Artwork,
    auth::{self, Operation, Session, SharedSession},
    catalog::{
        BrowseLink, Catalog, Collection, Continuation, Item, Page, PlaylistTarget, Rating, Shelf,
        ShelfLayout, Track,
    },
    country_picker::{CountryPicker, CountryPickerEvent},
    library::{Library, LibraryWriter, Queue, Repeat, Store},
    mpris,
    player::{
        AudioQuality, PlayOptions, PlaybackSpeed, Player, PlayerEvent, PlayerOptions,
        VideoAttachment, VideoQuality, VideoStatus,
    },
    playlist_form::{PlaylistForm, PlaylistFormEvent},
    queue_source::{self, PageRequest},
    search::{SearchFilter, SearchScope},
    search_box::{SearchBox, SearchBoxEvent},
    song_text::{self, Content as SongText, Query as TextQuery},
    theme::Theme,
    track_state,
    watch::{MediaPair, Watch, WatchContinuation, WatchMode, WatchPage},
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

const CARD_WIDTH: f32 = 180.;
const CARD_GAP: f32 = 24.;
const SHELF_HEIGHT: f32 = 350.;

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
    fn is_artist(&self) -> bool {
        matches!(self, Self::Browse(link) if link.id.starts_with("UC"))
    }
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
    SidebarPlaylists,
    PlaylistChoicesMore(Continuation),
}
impl Query {
    fn target(&self) -> CatalogTarget {
        match self {
            Self::PlaylistChoices | Self::PlaylistChoicesMore(_) => CatalogTarget::PlaylistPicker,
            Self::SidebarPlaylists => CatalogTarget::Sidebar,
            _ => CatalogTarget::Page,
        }
    }
}
struct Job {
    id: u64,
    account_revision: u64,
    query: Query,
    session: Option<Arc<Session>>,
}
struct CatalogResult {
    id: u64,
    account_revision: u64,
    append: bool,
    result: Result<Page, String>,
    target: CatalogTarget,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum CatalogTarget {
    Page,
    PlaylistPicker,
    Sidebar,
}
impl CatalogTarget {
    const ALL: [Self; 3] = [Self::Page, Self::PlaylistPicker, Self::Sidebar];
}
struct CatalogRequests {
    generations: Arc<[AtomicU64; CatalogTarget::ALL.len()]>,
    loading: [bool; CatalogTarget::ALL.len()],
    jobs: async_channel::Sender<Job>,
    pending: async_channel::Receiver<Job>,
}
impl CatalogRequests {
    fn new() -> Self {
        let (jobs, pending) = async_channel::bounded(CatalogTarget::ALL.len());
        Self {
            generations: Arc::new(std::array::from_fn(|_| AtomicU64::new(0))),
            loading: [false; CatalogTarget::ALL.len()],
            jobs,
            pending,
        }
    }

    fn request(
        &mut self,
        query: Query,
        account_revision: u64,
        session: Option<Arc<Session>>,
    ) -> Result<(), String> {
        let target = query.target();
        self.cancel(target);
        let id = self.generations[target as usize].load(Ordering::Relaxed);
        let mut other = Vec::with_capacity(CatalogTarget::ALL.len() - 1);
        while let Ok(job) = self.pending.try_recv() {
            if job.query.target() != target {
                other.push(job);
            }
        }
        // Keep one pending request per destination, including sidebar and picker reads.
        for job in other.into_iter().chain(std::iter::once(Job {
            id,
            account_revision,
            query,
            session,
        })) {
            self.jobs.try_send(job).map_err(|_| {
                "The music connection stopped. Restart the app to reconnect.".to_owned()
            })?;
        }
        self.loading[target as usize] = true;
        Ok(())
    }

    fn cancel(&mut self, target: CatalogTarget) {
        self.generations[target as usize].fetch_add(1, Ordering::Relaxed);
        self.loading[target as usize] = false;
    }

    fn finish(&mut self, target: CatalogTarget, id: u64) -> bool {
        if id != self.generations[target as usize].load(Ordering::Relaxed) {
            return false;
        }
        self.loading[target as usize] = false;
        true
    }

    fn is_loading(&self, target: CatalogTarget) -> bool {
        self.loading[target as usize]
    }
}
enum PlaybackQuery {
    Watch { track: Box<Track>, mode: WatchMode },
    Playlist(Arc<PlaylistTarget>),
    More(WatchContinuation),
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum QueueUpdate {
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
struct PlaybackJob {
    id: u64,
    account_revision: u64,
    query: PlaybackQuery,
    session: Option<Arc<Session>>,
}
enum PlaybackResult {
    Watch {
        id: u64,
        account_revision: u64,
        update: QueueUpdate,
        result: Result<Box<Watch>, String>,
    },
    More {
        id: u64,
        result: Result<WatchPage, String>,
    },
}
#[derive(Clone, Copy, Debug, PartialEq)]
enum Playback {
    Stopped,
    Loading { paused: bool, retried: bool },
    Loaded { paused: bool, buffering: bool },
}
#[derive(Clone, Copy)]
struct SeekIntent {
    track_id: u64,
    revision: u64,
    seconds: f64,
}

#[derive(Clone, Copy)]
struct QueueAdvance {
    natural: bool,
    playback: Playback,
}
impl Playback {
    fn is_paused(self) -> bool {
        matches!(
            self,
            Self::Loaded { paused: true, .. } | Self::Loading { paused: true, .. }
        )
    }

    fn with_pause(self, paused: bool) -> Self {
        match self {
            Self::Loading { retried, .. } => Self::Loading { paused, retried },
            Self::Loaded { buffering, .. } => Self::Loaded { paused, buffering },
            Self::Stopped => Self::Loaded {
                paused,
                buffering: false,
            },
        }
    }

    fn with_buffering(self, buffering: bool) -> Self {
        match self {
            Self::Loaded { paused, .. } => Self::Loaded { paused, buffering },
            _ => self,
        }
    }

    fn is_advancing(self) -> bool {
        matches!(
            self,
            Self::Loaded {
                paused: false,
                buffering: false
            }
        )
    }

    fn toggled(self) -> Self {
        self.with_pause(self != Self::Stopped && !self.is_paused())
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
        error: Option<String>,
    },
    PlaylistForm {
        form: Entity<PlaylistForm>,
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
    Restoring(Operation),
    RestoreFailed,
    Connecting(Operation, BrowserConnection),
    Activating(Operation),
    Disconnecting(Operation),
    DisconnectFailed,
}

enum BrowserConnection {
    Opening,
    Ready(auth::BrowserProfile),
    Reading(auth::BrowserProfile),
    Review {
        profile: auth::BrowserProfile,
        connection: Connection,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Startup {
    Restore,
    Connect,
    Guest,
}

enum ListRow {
    Heading(usize),
    Item(usize),
    Shelf(usize),
    More(usize),
}

struct DetailList {
    state: ListState,
    inline_header: bool,
}
impl DetailList {
    fn new() -> Self {
        Self {
            state: ListState::new(0, ListAlignment::Top, px(128.)),
            inline_header: false,
        }
    }
    fn reset_rows(&self, rows: usize) {
        let top = self.state.logical_scroll_top();
        self.state.reset(rows + usize::from(self.inline_header));
        self.state.scroll_to(top);
    }
    fn set_inline_header(&mut self, inline: bool) {
        if self.inline_header == inline {
            return;
        }
        let top = self.state.logical_scroll_top();
        if inline {
            self.state.splice(0..0, 1);
        } else {
            self.state.splice(0..1, 0);
        }
        self.inline_header = inline;
        if top.item_ix == 0 && top.offset_in_item == px(0.) {
            self.state.scroll_to(ListOffset {
                item_ix: 0,
                offset_in_item: px(0.),
            });
        }
        self.state.remeasure();
    }
}

struct NavigationState {
    route: Route,
    page: Option<Page>,
    list_scroll: UniformListScrollHandle,
    detail_list: DetailList,
    shelf_scroll: HashMap<usize, ScrollHandle>,
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
    account_revision: u64,
    track_state: track_state::Worker,
    track_state_id: Option<u64>,
    focus: FocusHandle,
    search: Entity<SearchBox>,
    route: Route,
    page: Page,
    rows: Vec<Item>,
    list_rows: Vec<ListRow>,
    shelf_scroll: HashMap<usize, ScrollHandle>,
    list_scroll: UniformListScrollHandle,
    detail_list: DetailList,
    sidebar_collapsed: bool,
    sidebar_playlists: Vec<Collection>,
    sidebar_error: Option<String>,
    sidebar_scroll: UniformListScrollHandle,
    back: Vec<NavigationState>,
    list_id: usize,
    error: Option<String>,
    notice: Option<String>,
    catalog_requests: CatalogRequests,
    playback_jobs: async_channel::Sender<PlaybackJob>,
    pending_playback_jobs: async_channel::Receiver<PlaybackJob>,
    playback_generation: Arc<AtomicU64>,
    now_playing: Option<Watch>,
    media_pair: Option<Box<MediaPair>>,
    radio: Option<WatchContinuation>,
    radio_generation: Arc<AtomicU64>,
    radio_loading: bool,
    autoplay_seeded: bool,
    autoplay_waiting: Option<QueueAdvance>,
    queue_source: queue_source::Worker,
    queue_waiting: Option<QueueAdvance>,
    autoplay_tracks: VecDeque<Track>,
    watch_pending: Option<QueueUpdate>,
    song_text: song_text::Worker,
    artwork: Artwork,
    library: Library,
    store: Option<LibraryWriter>,
    player: Option<Player>,
    mpris: Option<mpris::Mpris>,
    seekable: bool,
    video: Option<VideoAttachment>,
    video_open: bool,
    video_fullscreen: bool,
    video_transparent: bool,
    video_error: Option<String>,
    video_click: Option<Task<()>>,
    active_id: Option<u64>,
    history_track: Option<u64>,
    playback: Playback,
    position: f64,
    pending_seek: Option<SeekIntent>,
    duration: f64,
    show_queue: bool,
    overlay: Option<Overlay>,
    overlay_anchor: Option<Point<Pixels>>,
    overlay_subscription: Option<Subscription>,
    drag: Option<Drag>,
    seek_bounds: Rc<Cell<Bounds<Pixels>>>,
    volume_bounds: Rc<Cell<Bounds<Pixels>>>,
    drag_position: Option<f64>,
    _subscriptions: Vec<Subscription>,
}

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
                    AccountOperation::Connecting(operation, BrowserConnection::Opening)
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
        let catalog_requests = CatalogRequests::new();
        let (results, completed) = async_channel::unbounded::<CatalogResult>();
        for worker in 0..2 {
            let receiver = catalog_requests.pending.clone();
            let results = results.clone();
            let generations = catalog_requests.generations.clone();
            thread::Builder::new()
                .name(format!("catalog-{worker}"))
                .spawn(move || {
                    let catalog = Catalog::new();
                    while let Ok(job) = receiver.recv_blocking() {
                        let target = job.query.target();
                        if generations[target as usize].load(Ordering::Relaxed) != job.id {
                            continue;
                        }
                        let append = matches!(
                            job.query,
                            Query::More { .. } | Query::PlaylistChoicesMore(_)
                        );
                        let result = match &catalog {
                            Err(e) => Err(format!("{e:#}")),
                            Ok(catalog) => {
                                let catalog = catalog.for_session(job.session);
                                match &job.query {
                                    Query::Home => catalog.home(),
                                    Query::Explore => catalog.explore(),
                                    Query::Library(section) => catalog.library(*section),
                                    Query::PlaylistChoices | Query::SidebarPlaylists => {
                                        catalog.library(Section::Playlists)
                                    }
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
                                account_revision: job.account_revision,
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
                        PlaybackQuery::Watch { track, mode } => PlaybackResult::Watch {
                            id: job.id,
                            account_revision: job.account_revision,
                            update: mode.into(),
                            result: catalog.and_then(|catalog| {
                                catalog
                                    .watch(&track, mode)
                                    .map(Box::new)
                                    .map_err(|error| format!("{error:#}"))
                            }),
                        },
                        PlaybackQuery::Playlist(target) => PlaybackResult::Watch {
                            id: job.id,
                            account_revision: job.account_revision,
                            update: QueueUpdate::Replace,
                            result: catalog.and_then(|catalog| {
                                catalog
                                    .watch_playlist(&target)
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
                                    true,
                                );
                            }
                            Ok(_) => {}
                            Err(error) => {
                                view.error =
                                    Some(format!("Could not refresh the current song: {error}"))
                            }
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
                SearchBoxEvent::Blur => window.focus(&view.focus, cx),
            });
        let activation = cx.observe_window_activation(window, |view, window, cx| {
            if window.is_window_active()
                && view.account.is_some()
                && view.account_operation.is_none()
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
            account_revision: 0,
            track_state,
            track_state_id: None,
            focus: cx.focus_handle(),
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
            },
            rows: Vec::new(),
            list_rows: Vec::new(),
            shelf_scroll: HashMap::new(),
            list_scroll: UniformListScrollHandle::new(),
            detail_list: DetailList::new(),
            sidebar_collapsed: false,
            sidebar_playlists: Vec::new(),
            sidebar_error: None,
            sidebar_scroll: UniformListScrollHandle::new(),
            back: Vec::new(),
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
            video_open: false,
            video_fullscreen: false,
            video_transparent: false,
            video_error: None,
            video_click: None,
            active_id: None,
            history_track: None,
            playback: Playback::Stopped,
            position: 0.,
            pending_seek: None,
            duration: 0.,
            show_queue: false,
            overlay: (startup == Startup::Connect).then_some(Overlay::Settings),
            overlay_anchor: None,
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

    fn start_queue(&mut self, query: PlaybackQuery, cx: &mut Context<Self>) {
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

    fn playback_request(&mut self, query: PlaybackQuery, cx: &mut Context<Self>) {
        let id = if matches!(query, PlaybackQuery::More(_)) {
            if !self.pending_playback_jobs.is_empty() || self.radio_loading {
                return;
            }
            self.radio_loading = true;
            self.radio_generation.fetch_add(1, Ordering::Relaxed) + 1
        } else {
            self.discard_pending_playback();
            self.watch_pending = match &query {
                PlaybackQuery::Watch { mode, .. } => Some((*mode).into()),
                PlaybackQuery::Playlist(_) => Some(QueueUpdate::Replace),
                PlaybackQuery::More(_) => None,
            };
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

    fn playback_result(&mut self, result: PlaybackResult, cx: &mut Context<Self>) {
        match result {
            PlaybackResult::Watch {
                id,
                account_revision,
                update,
                result,
            } => {
                if id != self.playback_generation.load(Ordering::Relaxed) {
                    return;
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
                        let video_open = self.video_open;
                        if let Some(intent) = advance {
                            self.advance_queue(intent, cx);
                        } else if update == QueueUpdate::Replace {
                            let paused = self.playback.is_paused();
                            if switch_mode {
                                self.set_video_mode(self.library.video, cx);
                            }
                            self.play_current_at(0., paused, WatchMode::Details, cx);
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
                        self.video_open = video_open;
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
        self.catalog_requests.cancel(CatalogTarget::Page);
        self.catalog_requests.cancel(CatalogTarget::PlaylistPicker);
        self.catalog_requests.cancel(CatalogTarget::Sidebar);
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

    fn request(&mut self, query: Query, cx: &mut Context<Self>) {
        let target = query.target();
        let session = self
            .account
            .as_ref()
            .map(|connection| connection.session.clone());
        let result = self
            .catalog_requests
            .request(query, self.account_revision, session);
        if target == CatalogTarget::PlaylistPicker {
            if let Some(Overlay::PlaylistPicker { error, .. }) = &mut self.overlay {
                *error = result.err();
            }
        } else if target == CatalogTarget::Sidebar {
            self.sidebar_error = result.err();
        } else if target == CatalogTarget::Page
            && let Err(error) = result
        {
            self.error = Some(error);
        }
        cx.notify();
    }

    fn navigate(&mut self, route: Route, cx: &mut Context<Self>) {
        self.artwork.cancel_pending();
        self.video_open = false;
        if self.route != route {
            let cached = (self.account.is_none()
                && !self.catalog_requests.is_loading(CatalogTarget::Page)
                && self.error.is_none())
            .then(|| self.page.clone());
            self.back.push(NavigationState {
                route: self.route.clone(),
                page: cached,
                list_scroll: std::mem::replace(
                    &mut self.list_scroll,
                    UniformListScrollHandle::new(),
                ),
                shelf_scroll: std::mem::take(&mut self.shelf_scroll),
                detail_list: std::mem::replace(&mut self.detail_list, DetailList::new()),
            });
            if self.back.len() > 12 {
                self.back.remove(0);
            }
        }
        self.list_id = self.list_id.wrapping_add(1);
        self.route = route.clone();
        self.error = None;
        self.notice = None;
        self.page.shelves.clear();
        self.page.header = None;
        self.page.continuation = None;
        self.page.editable = false;
        self.page.description.clear();
        self.page.privacy = None;
        self.page.chips.clear();
        self.page.countries.clear();
        self.rows.clear();
        self.list_rows.clear();
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
                self.catalog_requests.cancel(CatalogTarget::Page);
                self.refresh_local();
                cx.notify();
            }
        }
    }

    fn rebuild_rows(&mut self) {
        self.rows.clear();
        self.list_rows.clear();
        let artist = self.route.is_artist();
        for (index, shelf) in self.page.shelves.iter().enumerate() {
            let carousel = artist && shelf.layout != ShelfLayout::Songs;
            if carousel {
                self.list_rows.push(ListRow::Shelf(index));
            } else if !shelf.title.is_empty() {
                self.list_rows.push(ListRow::Heading(index));
            }
            for item in &shelf.items {
                if !carousel {
                    self.list_rows.push(ListRow::Item(self.rows.len()));
                }
                self.rows.push(item.clone());
            }
            if artist && !carousel && shelf.more.is_some() {
                self.list_rows.push(ListRow::More(index));
            }
        }
        if self.page.header.is_some() {
            self.detail_list.reset_rows(if artist {
                self.list_rows.len()
            } else {
                self.rows.len()
            });
        } else {
            self.detail_list = DetailList::new();
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
            header: None,
            shelves: vec![Shelf {
                layout: ShelfLayout::Cards,
                title: String::new(),
                more: None,
                items: rows,
            }],
            continuation: None,
            editable: false,
            description: String::new(),
            privacy: None,
            chips: Vec::new(),
            countries: Vec::new(),
        };
        self.rebuild_rows();
    }

    fn go_back(&mut self, cx: &mut Context<Self>) {
        if self.video_open {
            self.video_open = false;
            cx.notify();
            return;
        }
        let Some(state) = self.back.pop() else { return };
        if let Some(page) = state.page.filter(|_| self.account.is_none()) {
            self.catalog_requests.cancel(CatalogTarget::Page);
            self.artwork.cancel_pending();
            self.list_id = self.list_id.wrapping_add(1);
            self.route = state.route;
            if let Route::Search(search) = &self.route {
                self.search
                    .update(cx, |input, cx| input.set_query(search.query.clone(), cx));
            }
            self.page = page;
            self.error = None;
            self.notice = None;
            self.rebuild_rows();
            self.refresh_local();
        } else {
            self.route = state.route.clone();
            self.navigate(state.route, cx);
        }
        self.list_scroll = state.list_scroll;
        self.detail_list = state.detail_list;
        self.shelf_scroll = state.shelf_scroll;
        cx.notify();
    }

    fn catalog_result(&mut self, result: CatalogResult, cx: &mut Context<Self>) {
        if !self.catalog_requests.finish(result.target, result.id) {
            return;
        }
        if result.target == CatalogTarget::Sidebar {
            match result.result {
                Ok(page) => {
                    self.sidebar_playlists = page
                        .shelves
                        .into_iter()
                        .flat_map(|shelf| shelf.items)
                        .filter_map(|item| match item {
                            Item::Collection(collection)
                                if !matches!(collection.id.as_str(), "VLLM" | "VLSE") =>
                            {
                                Some(collection)
                            }
                            _ => None,
                        })
                        .collect();
                    self.sidebar_error = None;
                }
                Err(error) => self.sidebar_error = Some(error),
            }
            cx.notify();
            return;
        }
        if matches!(result.target, CatalogTarget::PlaylistPicker) {
            if let Some(Overlay::PlaylistPicker { page, error, .. }) = &mut self.overlay {
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
                    Err(message) => *error = Some(message),
                }
                cx.notify();
            }
            return;
        }
        match result.result {
            Ok(mut page) => {
                if result.account_revision != self.account_revision {
                    for track in page
                        .shelves
                        .iter_mut()
                        .flat_map(|shelf| &mut shelf.items)
                        .filter_map(|item| {
                            if let Item::Track(track) = item {
                                Some(track)
                            } else {
                                None
                            }
                        })
                    {
                        track.details.clear_account_state();
                    }
                }
                if result.append {
                    self.page.continuation = page.continuation;
                    self.page.shelves.extend(page.shelves);
                } else {
                    self.page = page;
                    if matches!(self.overlay, Some(Overlay::Track { .. })) {
                        self.overlay = None;
                    }
                }
                for track in self
                    .page
                    .shelves
                    .iter()
                    .flat_map(|shelf| &shelf.items)
                    .filter_map(|item| match item {
                        Item::Track(track) => Some(track),
                        _ => None,
                    })
                {
                    if self.account.is_some()
                        && self
                            .library
                            .queue
                            .current()
                            .is_some_and(|current| current.id == track.id)
                    {
                        continue;
                    }
                    if let Some(rating) = track.details.rating {
                        self.ratings.insert(track.id.clone(), rating);
                    }
                }
                self.rebuild_rows();
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
    fn connect_account(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !auth::ACCOUNT_LOGIN_ENABLED {
            self.error = Some("Account connection is unavailable.".into());
            cx.notify();
            return;
        }
        if matches!(
            self.account_operation,
            Some(AccountOperation::RestoreFailed)
        ) {
            let operation = Operation::default();
            match self
                .account_commands
                .try_send(account::Command::Restore(operation.clone()))
            {
                Ok(()) => {
                    self.account_operation = Some(AccountOperation::Restoring(operation));
                    self.error = None;
                    self.notice = Some("Restoring your saved account…".into());
                }
                Err(_) => self.error = Some("The account connection is busy. Try again.".into()),
            }
            cx.notify();
            return;
        }
        if let Some(AccountOperation::Connecting(operation, stage)) = &self.account_operation {
            match stage {
                BrowserConnection::Ready(profile) => {
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
                            self.notice = Some("Checking the browser's YouTube account…".into());
                        }
                        Err(_) => {
                            self.error = Some("The account connection is busy. Try again.".into())
                        }
                    }
                }
                BrowserConnection::Review { connection, .. } => {
                    let event = account::Event::Connected {
                        operation: operation.clone(),
                        result: Ok(Some(connection.clone())),
                        save_session: true,
                    };
                    self.error = None;
                    self.account_event(event, window, cx);
                }
                BrowserConnection::Opening | BrowserConnection::Reading(_) => {}
            }
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
                self.account_operation = Some(AccountOperation::Connecting(
                    operation,
                    BrowserConnection::Opening,
                ));
                self.error = None;
                self.notice = Some("Opening your default browser…".into());
            }
            Err(_) => self.error = Some("The account connection is busy. Try again.".into()),
        }
        cx.notify();
    }
    fn retry_browser_account(&mut self, cx: &mut Context<Self>) {
        if let Some(AccountOperation::Connecting(
            operation,
            BrowserConnection::Review { profile, .. },
        )) = &self.account_operation
        {
            self.account_operation = Some(AccountOperation::Connecting(
                operation.clone(),
                BrowserConnection::Ready(profile.clone()),
            ));
            self.error = None;
            self.notice = None;
            cx.notify();
        }
    }
    fn cancel_connection(&mut self, cx: &mut Context<Self>) {
        let Some(
            AccountOperation::Connecting(operation, _) | AccountOperation::Restoring(operation),
        ) = self.account_operation.as_ref()
        else {
            return;
        };
        operation.cancel();
        if let Ok(mut session) = self.session.write() {
            *session = self.account.as_ref().map(|account| account.session.clone());
        }
        self.account_operation = None;
        self.notice = None;
        self.error = None;
        cx.notify();
    }
    fn mutate(&mut self, mutation: Mutation, cx: &mut Context<Self>) -> bool {
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
        cx.notify();
        submitted
    }
    fn disconnect_account(&mut self, cx: &mut Context<Self>) {
        if matches!(
            self.account_operation,
            Some(AccountOperation::Disconnecting(_))
        ) {
            return;
        }
        if let Some(
            AccountOperation::Connecting(operation, _)
            | AccountOperation::Restoring(operation)
            | AccountOperation::Activating(operation),
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
    fn refresh_remote(&mut self, cx: &mut Context<Self>) {
        self.account_revision = self.account_revision.wrapping_add(1);
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
        self.request(Query::SidebarPlaylists, cx);
        self.refresh_current_account(cx);
    }

    fn refresh_current_account(&mut self, cx: &mut Context<Self>) {
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
        let initial_video_id = track.as_ref().map(|track| track.id.clone());
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
                            initial_video_id: initial_video_id.clone(),
                        },
                    };
                    if view.mutate(mutation, cx) {
                        view.close_overlay(window, cx);
                    }
                }
                PlaylistFormEvent::Cancel => {
                    view.close_overlay(window, cx);
                }
            },
        ));
        window.focus(&form.focus_handle(cx), cx);
        self.error = None;
        self.overlay = Some(Overlay::PlaylistForm { form });
        cx.notify();
    }
    fn playlist_picker(&mut self, track: Track, cx: &mut Context<Self>) {
        self.error = None;
        self.overlay = Some(Overlay::PlaylistPicker {
            track,
            page: None,
            duplicates: false,
            error: None,
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
    fn open_track_menu(
        &mut self,
        track: Track,
        index: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus, cx);
        self.overlay_anchor = Some(window.mouse_position());
        self.overlay = Some(Overlay::Track { track, index });
        cx.notify();
    }

    fn close_overlay(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.catalog_requests.cancel(CatalogTarget::PlaylistPicker);
        self.song_text.cancel();
        self.overlay = None;
        self.overlay_anchor = None;
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
            account::Event::BrowserReady { operation, result } => {
                if !matches!(&self.account_operation, Some(AccountOperation::Connecting(active, BrowserConnection::Opening)) if active.matches(&operation))
                {
                    return;
                }
                self.notice = None;
                match result {
                    Ok(profile) => {
                        self.account_operation = Some(AccountOperation::Connecting(
                            operation,
                            BrowserConnection::Ready(profile),
                        ))
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
                let stage = match result {
                    Ok(connection) => BrowserConnection::Review {
                        profile,
                        connection,
                    },
                    Err(error) => {
                        self.error = Some(error);
                        BrowserConnection::Ready(profile)
                    }
                };
                self.account_operation = Some(AccountOperation::Connecting(operation, stage));
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
                if !matches!(&self.account_operation, Some(AccountOperation::Connecting(active, BrowserConnection::Review { .. }) | AccountOperation::Restoring(active)) if active.matches(&operation))
                {
                    return;
                }
                let restoring =
                    matches!(self.account_operation, Some(AccountOperation::Restoring(_)));
                match result {
                    Ok(Some(connection)) => {
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
                                self.account_operation =
                                    restoring.then_some(AccountOperation::RestoreFailed);
                                self.notice = None;
                                self.error = Some(format!(
                                    "Could not activate playback credentials: {error:#}"
                                ));
                                cx.notify();
                                return;
                            }
                        };
                        self.account_operation =
                            Some(AccountOperation::Activating(operation.clone()));
                        self.notice = Some("Preparing account playback…".into());
                        self.playback_generation.fetch_add(1, Ordering::Relaxed);
                        self.watch_pending = None;
                        self.discard_pending_playback();
                        self.reset_radio();
                        self.queue_waiting = None;
                        self.active_id = None;
                        self.pending_seek = None;
                        self.seekable = false;
                        self.playback = Playback::Stopped;
                        cx.spawn_in(window, async move |view, cx| {
                            let result = response.recv().await.unwrap_or_else(|_| {
                                Err("The player stopped before confirming the account. Restart the app.".into())
                            });
                            let _ = view.update_in(cx, |view, window, cx| {
                                if !matches!(&view.account_operation, Some(AccountOperation::Activating(active)) if active.matches(&operation)) {
                                    return;
                                }
                                view.account_operation = None;
                                view.notice = None;
                                match connection.activate(&view.session, &operation, result) {
                                    Ok(connection) => {
                                        let label = format!("Connected as {}", connection.account.name);
                                        let session = connection.session.clone();
                                        view.account = Some(connection);
                                        view.clear_account_state(cx);
                                        window.focus(&view.focus, cx);
                                        view.navigate(Route::Home, cx);
                                        view.request(Query::SidebarPlaylists,cx);
                                        view.back.clear();
                                        if save_session && view.account_commands.try_send(account::Command::Persist(session)).is_err() {
                                            view.error = Some("Connected for this session, but the account could not be queued for keyring storage.".into());
                                        }
                                        view.notice = (!restoring).then_some(label);
                                        view.refresh_current_account(cx);
                                    }
                                    Err(error) => {
                                        view.account_operation=restoring.then_some(AccountOperation::RestoreFailed);
                                        view.error=Some(format!("Could not activate the account: {error:#}"));
                                    }
                                }
                                cx.notify();
                            });
                        }).detach();
                    }
                    Ok(None) => {
                        self.account_operation = None;
                        self.notice = None;
                    }
                    Err(error) => {
                        self.account_operation =
                            restoring.then_some(AccountOperation::RestoreFailed);
                        self.notice = None;
                        self.error = Some(error);
                    }
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
                        acknowledge_song_change(
                            self.library.queue.tracks.iter_mut().chain(
                                self.page
                                    .shelves
                                    .iter_mut()
                                    .flat_map(|shelf| &mut shelf.items)
                                    .chain(&mut self.rows)
                                    .filter_map(|item| match item {
                                        Item::Track(track) => Some(track),
                                        _ => None,
                                    }),
                            ),
                            &mut self.ratings,
                            &mutation,
                        );
                        if let Mutation::CollectionLibrary { collection, save } = &mutation {
                            if matches!(&self.route, Route::Browse(link) if link.id == collection.id)
                                && let Some(header) = &mut self.page.header
                                && collection.id.starts_with("UC")
                            {
                                header.subscribed = Some(*save);
                                header.subscription_label =
                                    if *save { "Subscribed" } else { "Subscribe" }.into();
                            }
                            for item in self
                                .page
                                .shelves
                                .iter_mut()
                                .flat_map(|shelf| &mut shelf.items)
                                .chain(&mut self.rows)
                            {
                                if let Item::Collection(current) = item
                                    && current.id == collection.id
                                {
                                    current.details.saved = Some(*save);
                                }
                            }
                        }
                        self.notice = Some("Saved to YouTube Music".into());
                        match effect {
                            Effect::CreatedPlaylist { id } => {
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
                            Effect::Changed => {
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
                            && !self.catalog_requests.is_loading(CatalogTarget::Page)
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
        self.play_current_at(0., false, WatchMode::Details, cx);
    }
    fn play_current_at(
        &mut self,
        start: f64,
        paused: bool,
        watch_mode: WatchMode,
        cx: &mut Context<Self>,
    ) {
        if matches!(
            self.account_operation,
            Some(AccountOperation::Activating(_))
        ) {
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
            self.queue_source.cancel();
            self.queue_waiting = None;
            self.reset_radio();
            self.now_playing = None;
            self.library.queue.replace(vec![track], 0);
        }
        self.play_current_at(seconds as f64, false, WatchMode::Details, cx);
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
    fn set_video_mode(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.video_open = enabled;
        self.video_error = None;
        let changed = self.library.video != enabled;
        let position = self.playback_position();
        let paused = self.playback.is_paused();
        self.library.video = enabled;
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
            if self.active_id.is_some() {
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
    fn set_video_quality(&mut self, quality: VideoQuality, cx: &mut Context<Self>) {
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
        self.save();
        cx.notify();
    }
    fn toggle_video_fullscreen(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.video_open {
            return;
        }
        self.video_fullscreen = !window.is_fullscreen();
        window.toggle_fullscreen();
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
    fn replace_page_queue(&mut self, selected: usize, cx: &mut Context<Self>) {
        self.video_open = self.library.video;
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

    fn prefetch_queue(&mut self, cx: &mut Context<Self>) {
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

    fn queue_source_result(&mut self, event: queue_source::Event, cx: &mut Context<Self>) {
        let session = self.account.as_ref().map(|account| &account.session);
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
                if self.queue_waiting.take().is_some() {
                    self.playback = Playback::Stopped;
                    self.notice = None;
                }
                self.error = Some(error);
            }
        }
        cx.notify();
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
        self.queue_source.cancel();
        self.queue_waiting = None;
        self.track_state.cancel();
        self.track_state_id = None;
        self.player_command(Player::stop);
        self.active_id = None;
        self.seekable = false;
        self.playback = Playback::Stopped;
        self.position = 0.;
        self.duration = 0.;
        self.notice = None;
        self.library.queue = Default::default();
        self.now_playing = None;
        self.media_pair = None;
        self.reset_radio();
        self.playback_generation.fetch_add(1, Ordering::Relaxed);
        self.watch_pending = None;
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
    fn play_item(&mut self, index: usize, cx: &mut Context<Self>) {
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
                self.video_open = self.library.video;
                self.play_current_at(0., false, mode, cx);
            }
        }
    }
    fn toggle_play(&mut self, cx: &mut Context<Self>) {
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
            self.video_open = self.library.video;
            self.play_current(cx);
        }
    }
    fn waiting_for_watch(&self) -> bool {
        self.watch_pending.is_some()
            && self.active_id.is_none()
            && matches!(self.playback, Playback::Loading { .. })
    }
    fn next(&mut self, natural: bool, cx: &mut Context<Self>) {
        let playback = self
            .queue_waiting
            .take()
            .or(self.autoplay_waiting.take())
            .map_or(self.playback, |intent| intent.playback);
        self.advance_queue(QueueAdvance { natural, playback }, cx);
    }

    fn advance_queue(&mut self, intent: QueueAdvance, cx: &mut Context<Self>) {
        let autoplay = self.library.music_autoplay();
        let wait_for_queue = self.watch_pending == Some(QueueUpdate::Replace)
            || (self.watch_pending == Some(QueueUpdate::Adopt)
                && self
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
    fn previous(&mut self, cx: &mut Context<Self>) {
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

    fn follow_queue_selection(&mut self, playback: Playback, cx: &mut Context<Self>) {
        if playback != Playback::Stopped {
            self.play_current_at(0., playback.is_paused(), WatchMode::Details, cx);
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

    fn stop_playback(&mut self, cx: &mut Context<Self>) {
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
    fn player_event(&mut self, event: PlayerEvent, cx: &mut Context<Self>) {
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
                if let Some(service) = &self.mpris {
                    service.position_sample(track_id, seconds);
                }
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
                self.playback = self.playback.with_pause(paused)
            }
            PlayerEvent::Buffering {
                track_id,
                buffering,
            } if Some(track_id) == self.active_id => {
                self.playback = self.playback.with_buffering(buffering);
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

    fn publish_desktop(&self) {
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
                self.watch_pending,
                Some(QueueUpdate::Adopt | QueueUpdate::Replace)
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

    fn desktop_event(&mut self, event: mpris::Event, cx: &mut Context<Self>) {
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
                self.set_volume(volume, cx);
                self.save();
            }
            mpris::Event::Rate(speed) => self.player_command(|player| player.set_speed(speed)),
            mpris::Event::LoopStatus(repeat) => {
                self.library.repeat = repeat;
                self.save();
            }
            mpris::Event::Error(error) => self.error = Some(error),
        }
        self.publish_desktop();
        cx.notify();
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
        self.queue_changed(cx);
    }

    fn queue_changed(&mut self, cx: &mut Context<Self>) {
        if let Some(intent) = self.queue_waiting.take() {
            self.advance_queue(intent, cx);
        } else {
            self.resume_autoplay(cx);
        }
        self.prefetch_queue(cx);
        self.save();
        cx.notify();
    }

    fn remove_queue_track(&mut self, index: usize, cx: &mut Context<Self>) {
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
            return;
        }
        if control && key == "k" {
            if self.video_open && window.is_fullscreen() {
                self.toggle_video_fullscreen(window, cx);
            }
            window.focus(&self.search.focus_handle(cx), cx);
            return;
        }
        if control && key == "r" {
            if self.account.is_some() {
                self.refresh_remote(cx);
            } else {
                self.navigate(self.route.clone(), cx);
            }
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
            "f" if self.video_open => self.toggle_video_fullscreen(window, cx),
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
                if self.overlay.is_some() {
                    self.close_overlay(window, cx);
                    return;
                }
                if self.video_open && window.is_fullscreen() {
                    self.toggle_video_fullscreen(window, cx);
                    return;
                }
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
            .h(px(if compact { 68. } else { 48. }))
            .flex_shrink_0()
            .px(px(if compact { 4. } else { 16. }))
            .flex()
            .items_center()
            .when(compact, |d| d.flex_col().justify_center())
            .gap(px(if compact { 6. } else { 24. }))
            .rounded(px(8.))
            .text_size(px(if compact { 10. } else { 16. }))
            .text_color(self.theme.text)
            .when(active, |d| d.bg(self.theme.selection))
            .hover(|d| d.bg(self.theme.raised))
            .cursor_pointer()
            .child(icon(glyph, 24., self.theme.text))
            .child(label)
            .on_click(cx.listener(move |view, _, window, cx| {
                window.focus(&view.focus, cx);
                view.navigate(route.clone(), cx);
            }))
    }
    fn sidebar(&self, compact: bool, cx: &Context<Self>) -> impl IntoElement {
        let mut sidebar = div()
            .id("sidebar")
            .w(px(if compact { 72. } else { 240. }))
            .h_full()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .px(px(if compact { 4. } else { 8. }))
            .pt(px(8.))
            .border_r_1()
            .border_color(self.theme.border)
            .bg(self.theme.bg)
            .child(self.nav("home-nav", "home", "Home", Route::Home, compact, cx))
            .child(self.nav(
                "explore-nav",
                "explore",
                "Explore",
                Route::Explore,
                compact,
                cx,
            ))
            .child(self.nav("saved-nav", "library", "Library", Route::Saved, compact, cx));
        if compact {
            return sidebar.into_any_element();
        }
        sidebar = sidebar
            .child(
                div()
                    .h(px(1.))
                    .mx(px(16.))
                    .my(px(24.))
                    .bg(self.theme.border),
            )
            .child(
                div()
                    .id("sidebar-new-playlist")
                    .mx(px(12.))
                    .h(px(36.))
                    .mb(px(16.))
                    .rounded_full()
                    .bg(self.theme.selection)
                    .flex()
                    .items_center()
                    .justify_center()
                    .gap(px(8.))
                    .text_size(px(14.))
                    .font_weight(FontWeight::MEDIUM)
                    .cursor_pointer()
                    .hover(|d| d.bg(self.theme.raised))
                    .child(icon("plus", 20., self.theme.text))
                    .child("New playlist")
                    .on_click(cx.listener(|view, _, window, cx| {
                        window.focus(&view.focus, cx);
                        if view.account.is_some() {
                            view.playlist_form(false, None, window, cx);
                        } else {
                            view.overlay = Some(Overlay::Settings);
                            cx.notify();
                        }
                    })),
            );
        for (id, title, subtitle, route) in [
            ("liked-nav", "Liked Music", "Auto playlist", Route::Liked),
            ("recent-nav", "History", "Recently played", Route::Recent),
        ]
        .into_iter()
        .filter(|(id, _, _, _)| self.account.is_none() || *id != "recent-nav")
        {
            sidebar = sidebar.child(
                div()
                    .id(id)
                    .h(px(56.))
                    .px(px(16.))
                    .rounded(px(8.))
                    .flex()
                    .flex_col()
                    .justify_center()
                    .gap(px(4.))
                    .cursor_pointer()
                    .when(self.route == route, |d| d.bg(self.theme.selection))
                    .hover(|d| d.bg(self.theme.raised))
                    .child(
                        div()
                            .text_size(px(14.))
                            .font_weight(FontWeight::MEDIUM)
                            .child(title),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(self.theme.muted)
                            .child(subtitle),
                    )
                    .on_click(cx.listener(move |view, _, window, cx| {
                        window.focus(&view.focus, cx);
                        view.navigate(route.clone(), cx);
                    })),
            );
        }
        if self.account.is_some() {
            sidebar = sidebar.child(
                div()
                    .id("saved-episodes-nav")
                    .h(px(56.))
                    .px(px(16.))
                    .rounded(px(8.))
                    .flex()
                    .flex_col()
                    .justify_center()
                    .gap(px(4.))
                    .cursor_pointer()
                    .hover(|d| d.bg(self.theme.raised))
                    .child(
                        div()
                            .text_size(px(14.))
                            .font_weight(FontWeight::MEDIUM)
                            .child("Episodes for Later"),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(self.theme.muted)
                            .child("Auto playlist"),
                    )
                    .on_click(cx.listener(|view, _, _, cx| {
                        view.navigate(Route::Library(Section::Episodes), cx)
                    })),
            );
        }
        let playlists = if self.account.is_some() {
            &self.sidebar_playlists
        } else {
            &self.library.saved
        };
        sidebar = sidebar.child(
            uniform_list(
                "sidebar-playlists",
                playlists.len(),
                cx.processor(|view, range: std::ops::Range<usize>, _, cx| {
                    let playlists = if view.account.is_some() {
                        &view.sidebar_playlists
                    } else {
                        &view.library.saved
                    };
                    range
                        .map(|index| {
                            let playlist = &playlists[index];
                            let link = BrowseLink::new(playlist.id.clone(), playlist.title.clone());
                            div()
                                .id(("sidebar-playlist", index))
                                .h(px(56.))
                                .px(px(16.))
                                .rounded(px(8.))
                                .flex()
                                .flex_col()
                                .justify_center()
                                .gap(px(4.))
                                .cursor_pointer()
                                .hover(|d| d.bg(view.theme.raised))
                                .child(
                                    div()
                                        .truncate()
                                        .text_size(px(14.))
                                        .font_weight(FontWeight::MEDIUM)
                                        .child(playlist.title.clone()),
                                )
                                .child(
                                    div()
                                        .truncate()
                                        .text_size(px(12.))
                                        .text_color(view.theme.muted)
                                        .child(playlist.subtitle.clone()),
                                )
                                .on_click(cx.listener(move |view, _, window, cx| {
                                    window.focus(&view.focus, cx);
                                    view.navigate(Route::Browse(link.clone()), cx);
                                }))
                                .into_any_element()
                        })
                        .collect::<Vec<_>>()
                }),
            )
            .track_scroll(&self.sidebar_scroll)
            .flex_1()
            .min_h_0(),
        );
        if let Some(error) = &self.sidebar_error {
            sidebar = sidebar.child(
                div()
                    .px(px(16.))
                    .py(px(12.))
                    .text_size(px(12.))
                    .text_color(self.theme.muted)
                    .child(error.clone())
                    .child(
                        div()
                            .id("retry-sidebar")
                            .mt(px(8.))
                            .text_color(self.theme.text)
                            .cursor_pointer()
                            .child("Retry playlists")
                            .on_click(cx.listener(|view, _, _, cx| {
                                view.request(Query::SidebarPlaylists, cx)
                            })),
                    ),
            );
        }
        sidebar.into_any_element()
    }
    fn header(&self, inset: f32, cx: &Context<Self>) -> impl IntoElement {
        let initial = self
            .account
            .as_ref()
            .and_then(|connection| connection.account.name.chars().next())
            .map(|initial| initial.to_uppercase().to_string());
        div()
            .h(px(64.))
            .w_full()
            .flex_shrink_0()
            .flex()
            .items_center()
            .when(!self.route.is_artist() || self.video_open, |d| {
                d.bg(self.theme.bg)
            })
            .child(
                div()
                    .w(px(240.))
                    .flex_shrink_0()
                    .px(px(16.))
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .child(
                        button("guide-toggle", "menu", self.theme)
                            .size(px(40.))
                            .on_click(cx.listener(|view, _, _, cx| {
                                view.sidebar_collapsed = !view.sidebar_collapsed;
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .id("brand-home")
                            .flex()
                            .items_center()
                            .gap(px(4.))
                            .cursor_pointer()
                            .child(
                                div()
                                    .size(px(26.))
                                    .rounded_full()
                                    .bg(self.theme.accent)
                                    .child(icon("brand", 26., self.theme.text)),
                            )
                            .child(
                                svg()
                                    .path("wordmark")
                                    .w(px(47.))
                                    .h(px(26.))
                                    .text_color(self.theme.text),
                            )
                            .on_click(cx.listener(|view, _, _, cx| view.navigate(Route::Home, cx))),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .pl(px(inset))
                    .pr(px(32.))
                    .flex()
                    .items_center()
                    .gap(px(24.))
                    .child(
                        div()
                            .id("search-box")
                            .w(px(480.))
                            .min_w_0()
                            .h(px(42.))
                            .px(px(12.))
                            .rounded(px(8.))
                            .bg(self.theme.raised)
                            .border_1()
                            .border_color(rgb(0x484848))
                            .flex()
                            .items_center()
                            .gap(px(16.))
                            .on_click(cx.listener(|view, _, window, cx| {
                                window.focus(&view.search.focus_handle(cx), cx)
                            }))
                            .child(icon("search", 24., self.theme.text))
                            .child(div().flex_1().min_w_0().child(self.search.clone())),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .id("account-menu")
                            .size(px(32.))
                            .flex_shrink_0()
                            .rounded_full()
                            .bg(rgb(0x3f3f3f))
                            .flex()
                            .items_center()
                            .justify_center()
                            .cursor_pointer()
                            .when_some(initial, |d, initial| d.child(initial))
                            .when(self.account.is_none(), |d| {
                                d.child(icon("person", 24., self.theme.text))
                            })
                            .tooltip(|_, cx| {
                                cx.new(|_| Hint {
                                    text: "Account and settings",
                                    theme: Theme::load(),
                                })
                                .into()
                            })
                            .on_click(cx.listener(|view, _, window, cx| {
                                window.focus(&view.focus, cx);
                                view.overlay = Some(Overlay::Settings);
                                cx.notify();
                            })),
                    ),
            )
    }
    fn card(
        &mut self,
        item: Item,
        id: usize,
        width: f32,
        layout: ShelfLayout,
        cx: &Context<Self>,
    ) -> AnyElement {
        if let Item::Navigation(link) = item {
            return self
                .navigation(link, ("shelf-link", id), cx)
                .w(px(width))
                .h(px(44.))
                .into_any_element();
        }
        let title = item.title().to_owned();
        let subtitle = item.subtitle().to_owned();
        if layout == ShelfLayout::Songs {
            let art = self.art(item.thumbnail(), 48.);
            return div()
                .id(("quick-pick", id))
                .w(px(width))
                .h(px(64.))
                .flex()
                .items_center()
                .gap(px(16.))
                .rounded(px(4.))
                .cursor_pointer()
                .hover(|d| d.bg(self.theme.raised))
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
                                .text_size(px(16.))
                                .font_weight(FontWeight::MEDIUM)
                                .child(title),
                        )
                        .child(
                            div()
                                .truncate()
                                .text_size(px(14.))
                                .text_color(self.theme.muted)
                                .child(subtitle),
                        ),
                )
                .child(
                    button(("quick-menu", id), "more", self.theme).on_click(cx.listener(
                        move |view, _, window, cx| {
                            cx.stop_propagation();
                            window.focus(&view.focus, cx);
                            if let Some(Item::Track(track)) = view.rows.get(id) {
                                view.open_track_menu(track.clone(), Some(id), window, cx);
                            }
                        },
                    )),
                )
                .on_click(cx.listener(move |view, _, window, cx| {
                    window.focus(&view.focus, cx);
                    view.play_item(id, cx);
                }))
                .into_any_element();
        }
        let image = item.thumbnail().and_then(|url| self.artwork.get(url));
        let artist =
            matches!(&item, Item::Collection(collection) if collection.id.starts_with("UC"));
        let play = match &item {
            Item::Collection(collection) => collection.details.play.clone(),
            _ => None,
        };
        let mut art = div()
            .relative()
            .w(px(width))
            .h(px(if layout == ShelfLayout::Videos {
                width * 9. / 16.
            } else {
                width
            }))
            .rounded(px(4.))
            .when(artist, |d| d.rounded_full())
            .overflow_hidden()
            .bg(self.theme.raised);
        art = if let Some(image) = image {
            art.child(img(image).size_full().object_fit(ObjectFit::Cover))
        } else {
            art.flex()
                .items_center()
                .justify_center()
                .child(icon("music", 32., self.theme.muted))
        };
        if let Some(target) = play {
            art = art.child(
                div()
                    .id(("collection-play", id))
                    .invisible()
                    .group_hover("music-card", |d| d.visible())
                    .absolute()
                    .right(px(12.))
                    .bottom(px(12.))
                    .size(px(48.))
                    .rounded_full()
                    .bg(rgba(0x030303dd))
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .child(icon("play", 28., self.theme.text))
                    .on_click(cx.listener(move |view, _, window, cx| {
                        cx.stop_propagation();
                        window.focus(&view.focus, cx);
                        view.start_queue(PlaybackQuery::Playlist(target.clone()), cx);
                    })),
            );
        }
        div()
            .id(("card", id))
            .group("music-card")
            .w(px(width))
            .flex_shrink_0()
            .flex()
            .flex_col()
            .gap(px(8.))
            .cursor_pointer()
            .child(art)
            .child(
                div()
                    .w_full()
                    .truncate()
                    .text_size(px(16.))
                    .font_weight(FontWeight::MEDIUM)
                    .child(title),
            )
            .child(
                div()
                    .w_full()
                    .truncate()
                    .text_size(px(16.))
                    .text_color(self.theme.muted)
                    .child(subtitle),
            )
            .on_click(cx.listener(move |view, _, window, cx| {
                window.focus(&view.focus, cx);
                view.play_item(id, cx);
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
    fn shelf(
        &mut self,
        index: usize,
        inset: f32,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let shelf = &self.page.shelves[index];
        let title = shelf.title.clone();
        let more = shelf.more.clone();
        let layout = shelf.layout;
        let start = self.page.shelves[..index]
            .iter()
            .map(|shelf| shelf.items.len())
            .sum::<usize>();
        let navigation = shelf
            .items
            .iter()
            .all(|item| matches!(item, Item::Navigation(_)));
        let width = if navigation {
            240.
        } else {
            match layout {
                ShelfLayout::Cards => CARD_WIDTH,
                ShelfLayout::Videos => 256.,
                ShelfLayout::Songs => 360.,
            }
        };
        let rows = if navigation || layout == ShelfLayout::Songs {
            4
        } else {
            1
        };
        let count = shelf.items.len().div_ceil(rows);
        let scroll = self.shelf_scroll.entry(index).or_default().clone();
        let range = visible_cards(
            count,
            (-scroll.offset().x).into(),
            window.viewport_size().width.into(),
            width + CARD_GAP,
        );
        let stride = width + CARD_GAP;
        let columns = range
            .clone()
            .map(|column| {
                let items = self.page.shelves[index]
                    .items
                    .iter()
                    .skip(column * rows)
                    .take(rows)
                    .cloned()
                    .collect::<Vec<_>>();
                div()
                    .w(px(width))
                    .flex_shrink_0()
                    .flex()
                    .flex_col()
                    .gap(px(if navigation { 12. } else { 0. }))
                    .children(
                        items
                            .into_iter()
                            .enumerate()
                            .map(|(row, item)| {
                                self.card(item, start + column * rows + row, width, layout, cx)
                            })
                            .collect::<Vec<_>>(),
                    )
            })
            .collect::<Vec<_>>();
        let contents = div()
            .id(("carousel", index))
            .w_full()
            .min_w_0()
            .flex()
            .overflow_x_scroll()
            .track_scroll(&scroll)
            .on_scroll_wheel(cx.listener(|_, _, _, cx| cx.notify()))
            .child(
                div()
                    .relative()
                    .flex_shrink_0()
                    .w(px((count as f32 * stride - CARD_GAP).max(0.)))
                    .h(px(260.))
                    .child(
                        div()
                            .absolute()
                            .left(px(range.start as f32 * stride))
                            .flex()
                            .gap(px(CARD_GAP))
                            .children(columns),
                    ),
            );
        let previous = scroll.clone();
        let next = scroll.clone();
        div()
            .id(("shelf", index))
            .w_full()
            .min_w_0()
            .h(px(SHELF_HEIGHT))
            .px(px(inset))
            .pt(px(8.))
            .flex()
            .flex_col()
            .gap(px(20.))
            .child(
                div()
                    .h(px(40.))
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(32.))
                            .font_weight(FontWeight::BOLD)
                            .child(title),
                    )
                    .child(
                        div()
                            .flex()
                            .gap(px(16.))
                            .children(more.map(|link| {
                                div()
                                    .id(("carousel-more", index))
                                    .h(px(36.))
                                    .px(px(16.))
                                    .rounded_full()
                                    .border_1()
                                    .border_color(self.theme.border)
                                    .flex()
                                    .items_center()
                                    .cursor_pointer()
                                    .child(link.title.clone())
                                    .on_click(cx.listener(move |view, _, _, cx| {
                                        view.navigate(Route::Browse(link.clone()), cx)
                                    }))
                            }))
                            .child(
                                button(("shelf-previous", index), "back", self.theme)
                                    .size(px(36.))
                                    .border_1()
                                    .border_color(self.theme.border)
                                    .opacity(if scroll.offset().x < px(-1.) {
                                        1.
                                    } else {
                                        0.35
                                    })
                                    .on_click(cx.listener(move |_, _, _, cx| {
                                        let offset = previous.offset();
                                        previous.set_offset(point(
                                            (offset.x + px(stride * 3.)).min(px(0.)),
                                            offset.y,
                                        ));
                                        cx.notify();
                                    })),
                            )
                            .child(
                                button(("shelf-next", index), "forward", self.theme)
                                    .size(px(36.))
                                    .border_1()
                                    .border_color(self.theme.border)
                                    .on_click(cx.listener(move |_, _, _, cx| {
                                        let offset = next.offset();
                                        next.set_offset(point(
                                            (offset.x - px(stride * 3.)).max(-next.max_offset().x),
                                            offset.y,
                                        ));
                                        cx.notify();
                                    })),
                            ),
                    ),
            )
            .child(contents)
            .into_any_element()
    }
    fn row(
        &mut self,
        index: usize,
        inset: f32,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let item = self.rows[index].clone();
        let artist =
            self.route.is_artist() && window.viewport_size().width >= px(1200.) && !self.show_queue;
        let active = self.library.queue.current().is_some_and(|current| {
            current.id == item.id()
                && match &item {
                    Item::Track(track) if track.details.set_video_id.is_some() => {
                        current.details.set_video_id == track.details.set_video_id
                    }
                    _ => true,
                }
        }) && self.active_id.is_some();
        let numbered = matches!(&self.route,Route::Browse(link) if link.id.starts_with("MPRE"))
            && matches!(item, Item::Track(_));
        let art = if numbered {
            div()
                .w(px(32.))
                .h(px(48.))
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .text_color(self.theme.muted)
                .when(active, |d| d.child(icon("volume", 20., self.theme.text)))
                .when(!active, |d| d.child((index + 1).to_string()))
                .into_any_element()
        } else {
            let thumbnail = item.thumbnail().map(str::to_owned).or_else(|| {
                self.page
                    .header
                    .as_ref()
                    .and_then(|header| header.thumbnail.clone())
            });
            self.art(thumbnail.as_deref(), if artist { 40. } else { 48. })
        };
        let title = item.title().to_owned();
        let subtitle = item.subtitle().to_owned();
        let mut row = div()
            .id(("song", index))
            .group("music-row")
            .w_full()
            .h(px(if artist { 50. } else { 64. }))
            .flex()
            .items_center()
            .gap(px(if artist { 24. } else { 14. }))
            .rounded(px(4.))
            .when(active, |d| d.bg(self.theme.selection))
            .hover(|d| d.bg(self.theme.raised))
            .cursor_pointer()
            .on_click(cx.listener(move |view, _, window, cx| {
                window.focus(&view.focus, cx);
                view.play_item(index, cx);
            }))
            .child(art)
            .child(if artist {
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap(px(24.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(16.))
                            .child(title),
                    )
                    .child(
                        div()
                            .w(px(180.))
                            .min_w_0()
                            .truncate()
                            .text_size(px(16.))
                            .text_color(self.theme.muted)
                            .child(subtitle),
                    )
                    .when_some(
                        match &item {
                            Item::Track(track) => track.album.clone(),
                            _ => None,
                        },
                        |d, album| {
                            d.child(
                                div()
                                    .w(px(180.))
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(16.))
                                    .text_color(self.theme.muted)
                                    .child(album),
                            )
                        },
                    )
                    .into_any_element()
            } else {
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(5.))
                    .child(
                        div()
                            .truncate()
                            .text_size(px(16.))
                            .when(active, |d| d.text_color(self.theme.accent))
                            .child(title),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_size(px(14.))
                            .text_color(self.theme.muted)
                            .child(subtitle),
                    )
                    .into_any_element()
            });
        match item {
            Item::Navigation(link) => {
                return self
                    .navigation(link, ("browse-link", index), cx)
                    .w_full()
                    .h(px(64.))
                    .px(px(inset))
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
                            .text_size(px(14.))
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
                            if liked { "thumb-up-filled" } else { "thumb-up" },
                            self.theme,
                        )
                        .when(!liked, |d| {
                            d.invisible().group_hover("music-row", |d| d.visible())
                        })
                        .on_click(cx.listener(move |view, _, _, cx| {
                            cx.stop_propagation();
                            view.toggle_like(&like_track, cx);
                        })),
                    )
                    .child(
                        button(("add", index), "plus", self.theme)
                            .invisible()
                            .group_hover("music-row", |d| d.visible())
                            .on_click(cx.listener(move |view, _, _, cx| {
                                cx.stop_propagation();
                                view.enqueue(track.clone(), cx);
                            })),
                    )
                    .child(
                        div()
                            .id(("track-menu", index))
                            .invisible()
                            .group_hover("music-row", |d| d.visible())
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
                                view.open_track_menu(menu_track.clone(), Some(index), window, cx);
                            })),
                    );
            }
            Item::Collection(collection) => {
                let saved = self.is_saved(&collection);
                row = row
                    .child(div().text_size(px(14.)).text_color(self.theme.muted).child(
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
        div().w_full().px(px(inset)).child(row).into_any_element()
    }
    fn artist_heading(&mut self, inset: f32, cx: &mut Context<Self>) -> AnyElement {
        let header = self.page.header.clone().expect("artist page header");
        let mut actions = div().flex().items_center().flex_wrap().gap(px(8.));
        for (id, label, target) in [
            ("artist-shuffle", "Shuffle", header.shuffle),
            ("artist-mix", "Mix", header.mix),
        ] {
            if let Some(target) = target {
                actions = actions.child(
                    div()
                        .id(id)
                        .w(px(136.))
                        .h(px(36.))
                        .rounded_full()
                        .bg(self.theme.text)
                        .text_color(self.theme.bg)
                        .flex()
                        .items_center()
                        .justify_center()
                        .font_weight(FontWeight::MEDIUM)
                        .cursor_pointer()
                        .child(label)
                        .on_click(cx.listener(move |view, _, _, cx| {
                            view.start_queue(PlaybackQuery::Playlist(target.clone()), cx);
                        })),
                );
            }
        }
        if let Some(subscribed) = header.subscribed {
            let label = if header.subscription_label.is_empty() {
                if subscribed {
                    "Subscribed"
                } else {
                    "Subscribe"
                }
                .to_owned()
            } else {
                header.subscription_label
            };
            let label = if header.subscriber_count.is_empty() {
                label
            } else {
                format!("{label} {}", header.subscriber_count)
            };
            actions = actions.child(
                div()
                    .id("artist-subscribe")
                    .h(px(36.))
                    .px(px(16.))
                    .rounded_full()
                    .border_1()
                    .border_color(self.theme.accent)
                    .text_color(self.theme.accent)
                    .flex()
                    .items_center()
                    .cursor_pointer()
                    .child(label)
                    .on_click(cx.listener(move |view, _, window, cx| {
                        window.focus(&view.focus, cx);
                        if view.account.is_none() {
                            view.overlay = Some(Overlay::Settings);
                            cx.notify();
                            return;
                        }
                        let Route::Browse(link) = &view.route else {
                            return;
                        };
                        let collection = Collection {
                            id: link.id.clone(),
                            title: view.page.title.clone(),
                            subtitle: String::new(),
                            thumbnail: None,
                            details: Default::default(),
                        };
                        view.mutate(
                            Mutation::CollectionLibrary {
                                collection,
                                save: !subscribed,
                            },
                            cx,
                        );
                    })),
            );
        }
        div()
            .w_full()
            .h(px(500.))
            .px(px(inset))
            .pb(px(32.))
            .flex()
            .flex_col()
            .justify_end()
            .gap(px(16.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .child(
                        div()
                            .text_size(px(32.))
                            .line_height(relative(1.2))
                            .font_weight(FontWeight::BOLD)
                            .child(self.page.title.clone()),
                    )
                    .when(!header.subtitle.is_empty(), |d| {
                        d.child(
                            div()
                                .text_size(px(16.))
                                .line_height(relative(1.25))
                                .text_color(self.theme.muted)
                                .child(header.subtitle),
                        )
                    }),
            )
            .when(!self.page.description.is_empty(), |d| {
                d.child(
                    div()
                        .id("artist-description")
                        .max_w(px(700.))
                        .line_clamp(2)
                        .max_h(px(40.))
                        .overflow_hidden()
                        .text_size(px(14.))
                        .line_height(px(20.))
                        .cursor_pointer()
                        .child(self.page.description.clone())
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
            .child(actions)
            .into_any_element()
    }
    fn list_row(
        &mut self,
        index: usize,
        inset: f32,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match self.list_rows[index] {
            ListRow::Heading(shelf) => div()
                .h(px(64.))
                .px(px(inset))
                .flex()
                .items_center()
                .text_size(px(24.))
                .font_weight(FontWeight::BOLD)
                .child(self.page.shelves[shelf].title.clone())
                .into_any_element(),
            ListRow::Item(index) => self.row(index, inset, window, cx),
            ListRow::Shelf(index) => self.shelf(index, inset, window, cx),
            ListRow::More(index) => {
                let link = self.page.shelves[index]
                    .more
                    .clone()
                    .expect("shelf navigation row");
                div()
                    .h(px(72.))
                    .px(px(inset))
                    .pt(px(8.))
                    .flex()
                    .items_start()
                    .child(
                        div()
                            .id(("shelf-more", index))
                            .h(px(36.))
                            .px(px(16.))
                            .rounded_full()
                            .border_1()
                            .border_color(self.theme.border)
                            .flex()
                            .items_center()
                            .cursor_pointer()
                            .child(link.title.clone())
                            .on_click(cx.listener(move |view, _, _, cx| {
                                view.navigate(Route::Browse(link.clone()), cx);
                            })),
                    )
                    .into_any_element()
            }
        }
    }
    fn page_heading(&mut self, stacked: bool, edge: f32, cx: &mut Context<Self>) -> AnyElement {
        let header = self.page.header.clone();
        let title = self.page.title.clone();
        let has_tracks = self.rows.iter().any(|item| matches!(item, Item::Track(_)));
        let mut cover = None;
        let mut heading = div()
            .flex()
            .flex_col()
            .gap(px(16.))
            .when(stacked, |d| d.items_center().text_center());
        if let Some(header) = header {
            if let Some(url) = header.thumbnail {
                cover = Some(self.art(Some(&url), edge));
            }
            heading = heading.child(
                div()
                    .w_full()
                    .text_size(px(if stacked { 24. } else { 32. }))
                    .font_weight(FontWeight::BOLD)
                    .child(title),
            );
            if !header.subtitle.is_empty() {
                heading = heading.child(
                    div()
                        .text_size(px(14.))
                        .text_color(self.theme.muted)
                        .child(header.subtitle),
                );
            }
        } else {
            heading = heading.child(
                div()
                    .w_full()
                    .text_size(px(32.))
                    .font_weight(FontWeight::BOLD)
                    .child(title),
            );
        }
        if !self.page.description.is_empty() {
            heading = heading.child(
                div()
                    .id("page-description")
                    .w_full()
                    .text_size(px(14.))
                    .text_color(self.theme.muted)
                    .max_h(px(40.))
                    .overflow_hidden()
                    .cursor_pointer()
                    .child(self.page.description.clone())
                    .on_click(cx.listener(|view, _, window, cx| {
                        window.focus(&view.focus, cx);
                        view.overlay = Some(Overlay::Description {
                            title: view.page.title.clone(),
                            text: view.page.description.clone(),
                        });
                        cx.notify();
                    })),
            );
        }
        let mut actions = div()
            .flex()
            .items_center()
            .flex_wrap()
            .gap(px(12.))
            .when(stacked, |d| d.justify_center());
        if has_tracks {
            actions = actions.child(
                div()
                    .id("play-all")
                    .h(px(40.))
                    .px(px(24.))
                    .rounded_full()
                    .bg(self.theme.text)
                    .text_color(self.theme.bg)
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .font_weight(FontWeight::MEDIUM)
                    .cursor_pointer()
                    .child(icon("play", 24., self.theme.bg))
                    .child("Play")
                    .on_click(cx.listener(|view, _, _, cx| view.replace_page_queue(0, cx))),
            );
        }
        if let [Item::Track(track)] = self.rows.as_slice()
            && track
                .details
                .episode
                .as_ref()
                .is_some_and(|episode| !episode.chapters.is_empty())
        {
            let track = track.clone();
            actions = actions.child(
                div()
                    .id("page-chapters")
                    .px(px(12.))
                    .py(px(10.))
                    .cursor_pointer()
                    .child("Chapters")
                    .on_click(cx.listener(move |view, _, window, cx| {
                        window.focus(&view.focus, cx);
                        view.overlay = Some(Overlay::Chapters(Box::new(track.clone())));
                        cx.notify();
                    })),
            );
        }
        if !self.page.countries.is_empty() {
            let label = self
                .page
                .countries
                .iter()
                .find(|choice| choice.selected)
                .map_or("Country", |choice| choice.label.as_str())
                .to_owned();
            actions = actions.child(
                filter_chip("chart-country", label, false, self.theme)
                    .on_click(cx.listener(|view, _, window, cx| view.country_picker(window, cx))),
            );
        }
        if self.account.is_some() && self.page.editable {
            actions = actions
                .child(
                    filter_chip("edit-playlist", "Edit playlist", false, self.theme).on_click(
                        cx.listener(|view, _, window, cx| {
                            view.playlist_form(true, None, window, cx)
                        }),
                    ),
                )
                .child(
                    filter_chip("delete-playlist", "Delete", false, self.theme).on_click(
                        cx.listener(|view, _, window, cx| {
                            window.focus(&view.focus, cx);
                            if let Route::Browse(link) = &view.route {
                                view.overlay = Some(Overlay::DeletePlaylist {
                                    id: link.id.clone(),
                                    title: view.page.title.clone(),
                                });
                                cx.notify();
                            }
                        }),
                    ),
                );
        }
        if self.account.is_some()
            && matches!(
                self.route,
                Route::Saved | Route::Library(Section::Playlists)
            )
        {
            actions = actions.child(
                filter_chip("new-playlist", "New playlist", false, self.theme).on_click(
                    cx.listener(|view, _, window, cx| view.playlist_form(false, None, window, cx)),
                ),
            );
        }
        let heading = heading.child(actions);
        match cover {
            Some(cover) if stacked => div()
                .flex()
                .flex_col()
                .items_center()
                .gap(px(20.))
                .child(cover)
                .child(heading)
                .into_any_element(),
            Some(cover) => div()
                .flex()
                .items_start()
                .gap(px(24.))
                .child(cover)
                .child(heading.flex_1().min_w_0())
                .into_any_element(),
            None => heading.into_any_element(),
        }
    }
    fn content(&mut self, inset: f32, available: f32, cx: &mut Context<Self>) -> AnyElement {
        let home = self.route.is_home() || matches!(self.route, Route::Explore);
        let search = matches!(self.route, Route::Search(_));
        let library = matches!(self.route, Route::Saved | Route::Library(_));
        let collection = !home && self.page.header.is_some();
        let artist = collection && self.route.is_artist();
        let stacked = collection && !artist && available >= 980.;
        let mut content = div().flex_1().min_w_0().min_h_0().flex().flex_col();
        if !home && !artist {
            content = content.child(
                div()
                    .h(px(48.))
                    .px(px(inset))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .when(!self.back.is_empty(), |d| {
                        d.child(
                            button("back", "back", self.theme)
                                .on_click(cx.listener(|view, _, _, cx| view.go_back(cx))),
                        )
                    }),
            );
        }
        if !home && !search && !collection {
            let heading = self.page_heading(false, 128., cx);
            content = content.child(div().px(px(inset)).pb(px(28.)).child(heading));
        }
        if !self.page.chips.is_empty() {
            content = content.child(
                div()
                    .id("home-filters")
                    .mx(px(inset))
                    .mt(px(46.))
                    .mb(px(36.))
                    .flex_shrink_0()
                    .flex()
                    .gap(px(12.))
                    .overflow_x_scroll()
                    .children(
                        self.page
                            .chips
                            .iter()
                            .enumerate()
                            .filter(|(_, choice)| choice.label != "All" || !choice.selected)
                            .map(|(index, choice)| {
                                let target = choice.target.clone();
                                filter_chip(
                                    ("home-filter", index),
                                    choice.label.clone(),
                                    choice.selected,
                                    self.theme,
                                )
                                .on_click(cx.listener(
                                    move |view, _, _, cx| {
                                        view.navigate(Route::Browse(target.clone()), cx)
                                    },
                                ))
                            }),
                    ),
            );
        } else if home {
            content = content.child(div().h(px(40.)).flex_shrink_0());
        }
        if let Route::Search(search) = &self.route {
            if self.account.is_some() {
                content = content.child(
                    div()
                        .id("search-scopes")
                        .mx(px(inset))
                        .mb(px(24.))
                        .flex()
                        .gap(px(32.))
                        .children(SearchScope::ALL.into_iter().enumerate().map(
                            |(index, scope)| {
                                let mut next = search.clone();
                                next.scope = scope;
                                if !scope.supports(next.filter) {
                                    next.filter = SearchFilter::All;
                                }
                                div()
                                    .id(("search-scope", index))
                                    .pb(px(12.))
                                    .border_b_2()
                                    .border_color(if scope == search.scope {
                                        self.theme.text
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
                    .mx(px(inset))
                    .mb(px(24.))
                    .flex_shrink_0()
                    .flex()
                    .gap(px(12.))
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
                                filter_chip(
                                    ("search-filter", index),
                                    filter.label(),
                                    filter == search.filter,
                                    self.theme,
                                )
                                .on_click(cx.listener(
                                    move |view, _, _, cx| {
                                        view.navigate(Route::Search(next.clone()), cx)
                                    },
                                ))
                            }),
                    ),
            );
        }
        if self.account.is_none()
            && matches!(self.route, Route::Saved | Route::Liked | Route::Recent)
        {
            content = content.child(
                div()
                    .id("local-library-sections")
                    .mx(px(inset))
                    .mb(px(24.))
                    .flex()
                    .gap(px(12.))
                    .children(
                        [
                            (Route::Saved, "Playlists"),
                            (Route::Liked, "Liked Music"),
                            (Route::Recent, "History"),
                        ]
                        .into_iter()
                        .enumerate()
                        .map(|(index, (route, label))| {
                            filter_chip(
                                ("local-library-section", index),
                                label,
                                self.route == route,
                                self.theme,
                            )
                            .on_click(
                                cx.listener(move |view, _, _, cx| view.navigate(route.clone(), cx)),
                            )
                        }),
                    ),
            );
        }
        if self.account.is_some() && (library || matches!(self.route, Route::Liked | Route::Recent))
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
                    .mx(px(inset))
                    .mb(px(28.))
                    .flex_shrink_0()
                    .flex()
                    .gap(px(12.))
                    .overflow_x_scroll()
                    .children(
                        [
                            Section::Playlists,
                            Section::Podcasts,
                            Section::Songs,
                            Section::Albums,
                            Section::Artists,
                            Section::Subscriptions,
                            Section::Liked,
                            Section::History,
                            Section::Episodes,
                            Section::Uploads,
                            Section::UploadAlbums,
                            Section::UploadArtists,
                        ]
                        .into_iter()
                        .enumerate()
                        .map(|(index, section)| {
                            filter_chip(
                                ("library-section", index),
                                section.title(),
                                section == selected,
                                self.theme,
                            )
                            .on_click(cx.listener(
                                move |view, _, _, cx| view.navigate(Route::Library(section), cx),
                            ))
                        }),
                    ),
            );
        }
        if let Some(error) = &self.error {
            content = content.child(
                div()
                    .mx(px(inset))
                    .mb(px(16.))
                    .flex()
                    .items_center()
                    .gap(px(16.))
                    .text_color(rgb(0xf1a6ae))
                    .text_size(px(14.))
                    .child(div().flex_1().child(error.clone()))
                    .child(
                        button("dismiss-error", "close", self.theme).on_click(cx.listener(
                            |view, _, _, cx| {
                                view.error = None;
                                cx.notify();
                            },
                        )),
                    ),
            );
        }
        if let Some(notice) = &self.notice {
            content = content.child(
                div()
                    .px(px(inset))
                    .pb(px(16.))
                    .text_size(px(14.))
                    .text_color(self.theme.muted)
                    .child(notice.clone()),
            );
        }
        if self.catalog_requests.is_loading(CatalogTarget::Page) && self.rows.is_empty() {
            return content
                .child(
                    div()
                        .px(px(inset))
                        .py(px(24.))
                        .text_color(self.theme.muted)
                        .child("Loading music…"),
                )
                .into_any_element();
        }
        let heading = (collection && stacked)
            .then(|| self.page_heading(true, if stacked { 264. } else { 200. }, cx));
        if self.rows.is_empty() && !collection {
            let message = match self.route {
                Route::Liked => "Music you like will appear here.",
                Route::Recent => "Your listening history will appear here.",
                Route::Saved | Route::Library(_) => "Nothing saved yet.",
                Route::Search(_) => "No results. Try a different search.",
                _ => "No music to show yet.",
            };
            return content
                .children(heading.map(|heading| div().px(px(inset)).pb(px(24.)).child(heading)))
                .child(
                    div()
                        .px(px(inset))
                        .py(px(24.))
                        .text_color(self.theme.muted)
                        .child(message),
                )
                .into_any_element();
        }
        if home {
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
                                .h(px(72.))
                                .px(px(24.))
                                .rounded(px(4.))
                                .bg(self.theme.raised)
                                .flex_1(),
                            );
                        }
                    }
                } else {
                    shelves.push(index);
                }
            }
            content = content
                .when(!links.is_empty(), |d| {
                    d.child(
                        div()
                            .mx(px(inset))
                            .mb(px(36.))
                            .flex()
                            .gap(px(24.))
                            .children(links),
                    )
                })
                .child(
                    uniform_list(
                        ("home-shelves", self.list_id),
                        shelves.len(),
                        cx.processor(move |view, range: std::ops::Range<usize>, window, cx| {
                            range
                                .map(|i| view.shelf(shelves[i], inset, window, cx))
                                .collect::<Vec<_>>()
                        }),
                    )
                    .track_scroll(&self.list_scroll)
                    .flex_1()
                    .min_h_0(),
                );
        } else {
            let grid = !collection
                && self
                    .rows
                    .iter()
                    .all(|item| matches!(item, Item::Collection(_)));
            let grouped =
                matches!(&self.route,Route::Search(search) if search.filter==SearchFilter::All);
            let list = if collection && stacked && self.rows.is_empty() {
                div()
                    .flex_1()
                    .px(px(24.))
                    .py(px(32.))
                    .text_color(self.theme.muted)
                    .child("No songs here yet.")
                    .into_any_element()
            } else if collection {
                self.detail_list.set_inline_header(!stacked);
                let inline = self.detail_list.inline_header;
                list(
                    self.detail_list.state.clone(),
                    cx.processor(move |view, index: usize, window, cx| {
                        if artist {
                            return if index == 0 {
                                view.artist_heading(inset, cx)
                            } else {
                                view.list_row(index - 1, inset, window, cx)
                            };
                        }
                        if inline && index == 0 {
                            let heading = view.page_heading(true, 200., cx);
                            return div()
                                .w_full()
                                .px(px(inset))
                                .pb(px(32.))
                                .flex()
                                .justify_center()
                                .child(div().w_full().max_w(px(500.)).child(heading).when(
                                    view.rows.is_empty(),
                                    |d| {
                                        d.child(
                                            div()
                                                .mt(px(24.))
                                                .text_color(view.theme.muted)
                                                .child("No songs here yet."),
                                        )
                                    },
                                ))
                                .into_any_element();
                        }
                        view.row(
                            index - usize::from(inline),
                            if stacked { 0. } else { inset },
                            window,
                            cx,
                        )
                    }),
                )
                .flex_1()
                .min_h_0()
                .into_any_element()
            } else if grouped {
                uniform_list(
                    ("search-results", self.list_id),
                    self.list_rows.len(),
                    cx.processor(move |view, range: std::ops::Range<usize>, window, cx| {
                        range
                            .map(|index| view.list_row(index, inset, window, cx))
                            .collect::<Vec<_>>()
                    }),
                )
                .track_scroll(&self.list_scroll)
                .flex_1()
                .min_h_0()
                .into_any_element()
            } else if grid {
                let columns = ((available - inset * 2. + CARD_GAP) / (CARD_WIDTH + CARD_GAP))
                    .floor()
                    .max(1.) as usize;
                uniform_list(
                    ("collection-grid", self.list_id),
                    self.rows.len().div_ceil(columns),
                    cx.processor(move |view, range: std::ops::Range<usize>, _, cx| {
                        range
                            .map(|row| {
                                let items = view
                                    .rows
                                    .iter()
                                    .skip(row * columns)
                                    .take(columns)
                                    .cloned()
                                    .collect::<Vec<_>>();
                                div()
                                    .h(px(274.))
                                    .px(px(inset))
                                    .flex()
                                    .gap(px(CARD_GAP))
                                    .children(
                                        items
                                            .into_iter()
                                            .enumerate()
                                            .map(|(column, item)| {
                                                view.card(
                                                    item,
                                                    row * columns + column,
                                                    CARD_WIDTH,
                                                    ShelfLayout::Cards,
                                                    cx,
                                                )
                                            })
                                            .collect::<Vec<_>>(),
                                    )
                                    .into_any_element()
                            })
                            .collect::<Vec<_>>()
                    }),
                )
                .track_scroll(&self.list_scroll)
                .flex_1()
                .min_h_0()
                .into_any_element()
            } else {
                let row_inset = if stacked { 0. } else { inset };
                uniform_list(
                    ("music-results", self.list_id),
                    self.rows.len(),
                    cx.processor(move |view, range: std::ops::Range<usize>, window, cx| {
                        range
                            .map(|i| view.row(i, row_inset, window, cx))
                            .collect::<Vec<_>>()
                    }),
                )
                .track_scroll(&self.list_scroll)
                .flex_1()
                .min_h_0()
                .into_any_element()
            };
            content = if stacked {
                content.child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .px(px(60.))
                        .flex()
                        .gap(px(56.))
                        .child(
                            div()
                                .id("collection-header")
                                .w(px(280.))
                                .h_full()
                                .flex_shrink_0()
                                .overflow_y_scroll()
                                .children(heading),
                        )
                        .child(list),
                )
            } else {
                content
                    .children(heading.map(|heading| div().px(px(inset)).pb(px(28.)).child(heading)))
                    .child(list)
            };
        }
        if self.page.continuation.is_some() {
            content = content.child(
                div()
                    .id("load-more")
                    .h(px(40.))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(self.theme.muted)
                    .cursor_pointer()
                    .child(if self.catalog_requests.is_loading(CatalogTarget::Page) {
                        "Loading…"
                    } else {
                        "Load more"
                    })
                    .on_click(cx.listener(|view, _, _, cx| {
                        if !view.catalog_requests.is_loading(CatalogTarget::Page)
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
    fn queue(&mut self, compact: bool, cx: &mut Context<Self>) -> AnyElement {
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
            .w(px(if compact { 320. } else { 400. }))
            .h_full()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .border_l_1()
            .border_color(self.theme.border)
            .bg(self.theme.bg)
            .child(
                div()
                    .h(px(64.))
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
                                                        .text_size(px(16.))
                                                        .child(track.title),
                                                )
                                                .child(
                                                    div()
                                                        .truncate()
                                                        .text_size(px(14.))
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
                                                    .text_size(px(16.))
                                                    .when(active, |d| {
                                                        d.text_color(view.theme.accent)
                                                    })
                                                    .child(track.title),
                                            )
                                            .child(
                                                div()
                                                    .truncate()
                                                    .text_size(px(14.))
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
                                                            view.queue_changed(cx);
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
                                                view.remove_queue_track(index, cx);
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
                        .text_size(px(16.))
                        .child("Play a song or add one with +."),
                )
            })
            .into_any_element()
    }
    fn video_content(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let fullscreen = window.is_fullscreen();
        let current = self.library.queue.current();
        let title = current
            .map_or("Now playing", |track| track.title.as_str())
            .to_owned();
        let native = self.overlay.is_none()
            && self.active_id.is_some()
            && self.playback != Playback::Stopped
            && self.video_error.is_none()
            && self.video.is_some();
        let ready = native
            && self
                .video
                .as_ref()
                .is_some_and(|video| video.status() == VideoStatus::Ready);
        let view = cx.entity().downgrade();
        let mut viewport = div()
            .id("video-viewport")
            .relative()
            .flex_1()
            .min_h_0()
            .when(!ready, |viewport| viewport.bg(gpui::black()))
            .child(
                canvas(
                    move |bounds, window, cx| {
                        view.update(cx, |view, cx| {
                            if !native {
                                return false;
                            }
                            let Some(video) = &view.video else {
                                return false;
                            };
                            if let Err(error) = video.attach(bounds, window) {
                                view.video_error = Some(error.to_string());
                                cx.notify();
                                return false;
                            }
                            video.status() == VideoStatus::Ready
                        })
                        .unwrap_or(false)
                    },
                    |bounds, ready, window, _| {
                        if !ready {
                            window.paint_quad(fill(bounds, gpui::black()));
                        }
                    },
                )
                .absolute()
                .size_full(),
            )
            .on_click(cx.listener(|view, event: &ClickEvent, window, cx| {
                window.focus(&view.focus, cx);
                view.video_click = None;
                if event.click_count() == 2 {
                    view.toggle_video_fullscreen(window, cx);
                    return;
                }
                if event.click_count() != 1 {
                    return;
                }
                let active = view.active_id;
                let playback = view.playback;
                view.video_click = Some(cx.spawn(async move |view, cx| {
                    // GPUI's Linux backends recognize double-clicks within 400 ms.
                    cx.background_executor()
                        .timer(Duration::from_millis(400))
                        .await;
                    let _ = view.update(cx, |view, cx| {
                        if view.video_open
                            && view.overlay.is_none()
                            && view.active_id == active
                            && view.playback == playback
                        {
                            view.toggle_play(cx);
                        }
                    });
                }));
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|view, event: &MouseDownEvent, _, _| {
                    if event.click_count >= 2 {
                        view.video_click = None;
                    }
                }),
            );
        if !native {
            let message = self.video_error.clone().unwrap_or_else(|| {
                if current.is_some() {
                    "Press play to watch".into()
                } else {
                    "Choose something to play".into()
                }
            });
            viewport = viewport.child(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap(px(18.))
                    .px(px(40.))
                    .text_color(self.theme.muted)
                    .child(message)
                    .when(self.video_error.is_some(), |body| {
                        body.child(
                            div()
                                .id("retry-video")
                                .px(px(16.))
                                .py(px(10.))
                                .text_color(self.theme.text)
                                .cursor_pointer()
                                .child("Retry video")
                                .on_click(cx.listener(|view, _, _, cx| {
                                    cx.stop_propagation();
                                    if let Some(video) = &view.video {
                                        video.detach();
                                    }
                                    view.video_error = None;
                                    cx.notify();
                                })),
                        )
                    }),
            );
        }
        let content = div().flex_1().min_w_0().flex().flex_col();
        content
            .child(
                div()
                    .h(px(64.))
                    .flex_shrink_0()
                    .px(px(24.))
                    .bg(self.theme.bg)
                    .flex()
                    .items_center()
                    .gap(px(14.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(title),
                    )
                    .child(
                        div()
                            .id("audio-mode")
                            .px(px(10.))
                            .py(px(12.))
                            .text_color(self.theme.muted)
                            .cursor_pointer()
                            .child("Audio")
                            .on_click(cx.listener(|view, _, _, cx| view.set_video_mode(false, cx))),
                    )
                    .child(div().text_color(self.theme.accent).child("Video"))
                    .child(
                        div()
                            .id("video-quality-picker")
                            .px(px(10.))
                            .py(px(12.))
                            .cursor_pointer()
                            .child(self.library.video_quality.label())
                            .on_click(cx.listener(|view, _, window, cx| {
                                window.focus(&view.focus, cx);
                                view.overlay = Some(Overlay::Settings);
                                cx.notify();
                            })),
                    )
                    .child(
                        button(
                            "video-fullscreen",
                            if fullscreen {
                                "exit-fullscreen"
                            } else {
                                "fullscreen"
                            },
                            self.theme,
                        )
                        .on_click(cx.listener(|view, _, window, cx| {
                            view.toggle_video_fullscreen(window, cx)
                        })),
                    ),
            )
            .when_some(self.error.as_ref(), |content, error| {
                content.child(
                    div()
                        .px(px(24.))
                        .py(px(12.))
                        .bg(self.theme.panel)
                        .text_color(rgb(0xf1a6ae))
                        .child(error.clone()),
                )
            })
            .child(viewport)
            .into_any_element()
    }
    fn player_bar(&mut self, compact: bool, cx: &mut Context<Self>) -> AnyElement {
        let current = self.library.queue.current().cloned();
        let progress = if self.duration > 0. {
            (self
                .drag_position
                .unwrap_or_else(|| self.playback_position())
                / self.duration)
                .clamp(0., 1.) as f32
        } else {
            0.
        };
        let playing = self.active_id.is_some()
            && self.playback != Playback::Stopped
            && !self.playback.is_paused();
        let mut details = div()
            .flex_1()
            .min_w_0()
            .max_w(px(660.))
            .flex()
            .items_center()
            .gap(px(16.));
        if let Some(track) = current {
            let art = self.art(track.thumbnail.as_deref(), 40.);
            let liked = self.is_liked(&track);
            let disliked = self
                .ratings
                .get(&track.id)
                .copied()
                .or(track.details.rating)
                == Some(Rating::Dislike);
            let subtitle = match self.playback {
                Playback::Loading { .. } => "Loading track…".into(),
                Playback::Loaded {
                    paused: false,
                    buffering: true,
                } => "Buffering…".into(),
                _ => track.artist.clone(),
            };
            let like_track = track.clone();
            let dislike_track = track.clone();
            let menu_track = track.clone();
            details = details
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
                                .text_size(px(16.))
                                .font_weight(FontWeight::MEDIUM)
                                .child(track.title),
                        )
                        .child(
                            div()
                                .truncate()
                                .text_size(px(16.))
                                .text_color(self.theme.muted)
                                .child(subtitle),
                        ),
                )
                .when(self.account.is_some() && !compact, |d| {
                    d.child(
                        button("dislike-current", "thumb-down", self.theme)
                            .opacity(if disliked { 1. } else { 0.65 })
                            .on_click(cx.listener(move |view, _, _, cx| {
                                view.mutate(
                                    Mutation::RateSong {
                                        track: dislike_track.clone(),
                                        rating: if disliked {
                                            Rating::Indifferent
                                        } else {
                                            Rating::Dislike
                                        },
                                    },
                                    cx,
                                );
                            })),
                    )
                })
                .child(
                    button(
                        "like-current",
                        if liked { "thumb-up-filled" } else { "thumb-up" },
                        self.theme,
                    )
                    .on_click(cx.listener(move |view, _, _, cx| view.toggle_like(&like_track, cx))),
                )
                .child(
                    button("current-song-menu", "more", self.theme).on_click(cx.listener(
                        move |view, _, window, cx| {
                            window.focus(&view.focus, cx);
                            view.open_track_menu(menu_track.clone(), None, window, cx);
                        },
                    )),
                );
        }
        let seek_bounds = self.seek_bounds.clone();
        let volume_bounds = self.volume_bounds.clone();
        div()
            .h(px(74.))
            .flex_shrink_0()
            .relative()
            .bg(self.theme.panel)
            .child(
                div()
                    .id("seek-track")
                    .absolute()
                    .top(px(-6.))
                    .left_0()
                    .right_0()
                    .h(px(12.))
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
            .child(
                div()
                    .size_full()
                    .px(px(16.))
                    .flex()
                    .items_center()
                    .gap(px(if compact { 16. } else { 32. }))
                    .child(
                        div()
                            .flex_shrink_0()
                            .flex()
                            .items_center()
                            .gap(px(8.))
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
                                .size(px(48.))
                                .on_click(cx.listener(|view, _, _, cx| view.toggle_play(cx))),
                            )
                            .child(
                                button("next", "next", self.theme)
                                    .on_click(cx.listener(|view, _, _, cx| view.next(false, cx))),
                            )
                            .when(!compact, |d| {
                                d.child(
                                    div()
                                        .ml(px(8.))
                                        .text_size(px(12.))
                                        .text_color(self.theme.muted)
                                        .child(format!(
                                            "{} / {}",
                                            clock(
                                                self.drag_position
                                                    .unwrap_or_else(|| self.playback_position())
                                            ),
                                            clock(self.duration)
                                        )),
                                )
                            }),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .justify_center()
                            .child(details),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .flex()
                            .items_center()
                            .gap(px(4.))
                            .when(!compact, |d| {
                                d.child(
                                    div()
                                        .id("volume-track")
                                        .w(px(72.))
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
                                )
                            })
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
                                .on_click(cx.listener(
                                    |view, _, _, cx| {
                                        view.set_volume(
                                            if view.library.volume > 0. { 0. } else { 60. },
                                            cx,
                                        );
                                        view.save();
                                    },
                                )),
                            )
                            .child(
                                button("repeat", "repeat", self.theme)
                                    .opacity(if self.library.repeat == Repeat::Off {
                                        0.6
                                    } else {
                                        1.
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
                            )
                            .child(
                                button("shuffle", "shuffle", self.theme).on_click(cx.listener(
                                    |view, _, _, cx| {
                                        view.library.queue.shuffle_upcoming();
                                        view.notice = Some("Upcoming songs shuffled".into());
                                        view.prefetch_queue(cx);
                                        view.save();
                                        cx.notify();
                                    },
                                )),
                            )
                            .child(
                                button("video-toggle", "video", self.theme)
                                    .opacity(if self.video_open { 1. } else { 0.65 })
                                    .on_click(cx.listener(|view, _, window, cx| {
                                        window.focus(&view.focus, cx);
                                        if view.video_open {
                                            view.video_open = false;
                                            cx.notify();
                                        } else {
                                            view.set_video_mode(true, cx);
                                        }
                                    })),
                            )
                            .child(
                                button("queue-toggle", "queue", self.theme)
                                    .opacity(if self.show_queue { 1. } else { 0.65 })
                                    .on_click(cx.listener(|view, _, _, cx| {
                                        view.show_queue = !view.show_queue;
                                        cx.notify();
                                    })),
                            ),
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
                cx.listener(|view, _, window, cx| {
                    cx.stop_propagation();
                    view.close_overlay(window, cx);
                }),
            )
            .child(body)
            .into_any_element()
    }
    fn overlay(&self, kind: &Overlay, window: &Window, cx: &Context<Self>) -> AnyElement {
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
                    .when(matches!(kind, Overlay::Track { .. }), |d| {
                        d.h(px(0.)).mb(px(0.)).overflow_hidden()
                    })
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
                ("Alt ←", "Go back"),
                ("Space", "Play or pause"),
                ("Ctrl → / Ctrl ←", "Next / previous song"),
                ("→ / ←", "Seek 5 seconds"),
                ("↑ / ↓", "Adjust volume"),
                ("L", "Like the current song"),
                ("Q", "Show the queue"),
                ("F", "Toggle video fullscreen"),
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
            let label = self
                .account
                .as_ref()
                .map(|a| format!("{} {}", a.account.name, a.account.handle))
                .unwrap_or_else(|| "Connect your Google account to sync with YouTube Music".into());
            let disconnect_failed = matches!(
                self.account_operation,
                Some(AccountOperation::DisconnectFailed)
            );
            let connecting = matches!(
                self.account_operation,
                Some(AccountOperation::Connecting(_, _))
            );
            let restoring = matches!(self.account_operation, Some(AccountOperation::Restoring(_)));
            let activating = matches!(
                self.account_operation,
                Some(AccountOperation::Activating(_))
            );
            let (connect_label, instructions) = match &self.account_operation {
                Some(AccountOperation::Connecting(_, BrowserConnection::Opening)) => {
                    ("Opening browser…".to_owned(), None)
                }
                Some(AccountOperation::Connecting(_, BrowserConnection::Ready(profile))) => (
                    "Read YouTube session".to_owned(),
                    Some(format!(
                        "Reads your YouTube sign-in session from {} · {}. After you confirm, Music can refresh this account from that profile when restoring an expired saved session.",
                        profile.browser, profile.name
                    )),
                ),
                Some(AccountOperation::Connecting(_, BrowserConnection::Reading(_))) => {
                    ("Checking account…".to_owned(), None)
                }
                Some(AccountOperation::Connecting(
                    _,
                    BrowserConnection::Review { connection, .. },
                )) => (
                    format!("Connect as {}", connection.account.name),
                    Some(format!(
                        "{} {}",
                        connection.account.name, connection.account.handle
                    )),
                ),
                Some(AccountOperation::Restoring(_)) => ("Restoring account…".to_owned(), None),
                Some(AccountOperation::RestoreFailed) => ("Retry saved account".to_owned(), None),
                Some(AccountOperation::Activating(_)) => {
                    ("Preparing account playback…".to_owned(), None)
                }
                Some(AccountOperation::Disconnecting(_)) => ("Signing out…".to_owned(), None),
                Some(AccountOperation::DisconnectFailed) => {
                    ("Sign-out incomplete".to_owned(), None)
                }
                None if self.account.is_some() => ("Switch or reconnect account".to_owned(), None),
                None => ("Connect in browser".to_owned(), None),
            };
            let reviewing = matches!(
                self.account_operation,
                Some(AccountOperation::Connecting(
                    _,
                    BrowserConnection::Review { .. }
                ))
            );
            body=body.child(div().text_size(px(15.)).child(label))
                .child(div().id("connect-account").mt(px(22.)).px(px(15.)).py(px(11.)).bg(self.theme.text).text_color(self.theme.bg).rounded(px(5.)).cursor_pointer()
                    .child(connect_label)
                    .on_click(cx.listener(|view,_,window,cx|view.connect_account(window,cx))))
                .when_some(instructions, |body, text| body.child(div().mt(px(12.)).text_size(px(13.)).text_color(self.theme.muted).child(text)))
                .when(matches!(self.account_operation,Some(AccountOperation::RestoreFailed)),|body|body.child(
                    div().id("reconnect-browser-account").mt(px(16.)).cursor_pointer().child("Connect in browser")
                        .on_click(cx.listener(|view,_,window,cx|{
                            view.account_operation=None;
                            view.connect_account(window,cx);
                        }))))
                .when(reviewing, |body| body.child(div().id("retry-browser-account").mt(px(16.)).text_color(self.theme.accent).cursor_pointer().child("Check another account").on_click(cx.listener(|view,_,_,cx|view.retry_browser_account(cx)))))
                .when(connecting || restoring, |body| body.child(div().id("cancel-connection").mt(px(16.)).text_color(self.theme.accent).cursor_pointer().child("Cancel connection").on_click(cx.listener(|view,_,_,cx|view.cancel_connection(cx)))))
                .when_some(self.error.as_ref(), |body, error| body.child(div().mt(px(12.)).text_size(px(13.)).text_color(rgb(0xf1a6ae)).child(error.clone())))
                .when(self.account.is_some() || disconnect_failed || activating, |body| body.child(div().id("disconnect-account").mt(px(16.)).text_color(self.theme.accent).cursor_pointer().child(if disconnect_failed {"Retry sign-out"} else {"Sign out"}).on_click(cx.listener(|view,_,_,cx|view.disconnect_account(cx)))))
                .child(div().mt(px(16.)).text_size(px(13.)).text_color(self.theme.muted).child(if self.account.is_some(){"Library changes are saved to your Google account. Returning to this window loads changes made on the web."}else{"Guest likes and saved collections stay on this computer. Connecting shows your account library."}))
                .child(div().id("settings-open-web").mt(px(24.)).text_color(self.theme.accent).cursor_pointer().child("Open YouTube Music").on_click(|_,_,cx|cx.open_url("https://music.youtube.com")));
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
                .child(div().mt(px(32.)).text_size(px(15.)).child("Playback speed"))
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
            let mut video_qualities = div().mt(px(12.)).flex().flex_wrap().gap(px(4.));
            for (index, quality) in VideoQuality::ALL.into_iter().enumerate() {
                video_qualities = video_qualities.child(
                    div()
                        .id(("video-quality", index))
                        .px(px(12.))
                        .py(px(10.))
                        .text_size(px(13.))
                        .border_b_2()
                        .border_color(if self.library.video_quality == quality {
                            self.theme.accent
                        } else {
                            self.theme.panel
                        })
                        .when(self.library.video_quality == quality, |button| {
                            button.text_color(self.theme.accent)
                        })
                        .hover(|button| button.bg(self.theme.raised))
                        .cursor_pointer()
                        .child(quality.label())
                        .on_click(cx.listener(move |view, _, _, cx| {
                            view.set_video_quality(quality, cx);
                        })),
                );
            }
            body = body
                .child(div().mt(px(24.)).text_size(px(15.)).child("Video quality"))
                .child(video_qualities);
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
            let mut track = track.clone();
            if self.account.is_some()
                && let Some(current) = self
                    .library
                    .queue
                    .current()
                    .filter(|current| current.id == track.id)
            {
                track.details.rating = current.details.rating;
                track.details.in_library = current.details.in_library;
                track.details.add_library = current.details.add_library.clone();
                track.details.remove_library = current.details.remove_library.clone();
            }
            let track = &track;
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
                        view.start_queue(
                            PlaybackQuery::Watch {
                                track: Box::new(song.clone()),
                                mode: WatchMode::Radio,
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
                        view.queue_changed(cx);
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
                if index.is_some()
                    && self.page.editable
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
                    if let Some(index) = index.and_then(|index| self.page.track_index(index)) {
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
                                playlist_move_down(
                                    &tracks,
                                    index,
                                    self.page.continuation.is_some(),
                                ),
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
            error,
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
                        Item::Collection(collection) if collection.details.editable => {
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
                    body = body.child(
                        self.menu_action(
                            "more-playlist-choices",
                            if self
                                .catalog_requests
                                .is_loading(CatalogTarget::PlaylistPicker)
                            {
                                "Loading…"
                            } else {
                                "More playlists"
                            },
                            cx,
                            move |view, _, cx| {
                                if !view
                                    .catalog_requests
                                    .is_loading(CatalogTarget::PlaylistPicker)
                                {
                                    view.request(Query::PlaylistChoicesMore(token.clone()), cx);
                                }
                            },
                        ),
                    );
                }
            } else if self
                .catalog_requests
                .is_loading(CatalogTarget::PlaylistPicker)
            {
                body = body.child(
                    div()
                        .p(px(12.))
                        .text_color(self.theme.muted)
                        .child("Loading playlists…"),
                );
            } else {
                body = body.child(self.menu_action(
                    "retry-playlist-choices",
                    "Retry",
                    cx,
                    |view, _, cx| view.request(Query::PlaylistChoices, cx),
                ));
            }
            if let Some(error) = error {
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
                move |view, window, cx| {
                    if view.mutate(Mutation::DeletePlaylist { id: id.clone() }, cx) {
                        view.close_overlay(window, cx);
                    }
                },
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
        if matches!(kind, Overlay::Track { .. }) {
            let position = self.overlay_anchor.unwrap_or_else(|| {
                point(
                    window.viewport_size().width / 2.,
                    window.viewport_size().height / 3.,
                )
            });
            return div()
                .absolute()
                .inset_0()
                .occlude()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|view, _, window, cx| {
                        cx.stop_propagation();
                        view.close_overlay(window, cx);
                    }),
                )
                .child(
                    anchored()
                        .position(position)
                        .snap_to_window_with_margin(px(8.))
                        .child(
                            body.w(px(320.))
                                .p(px(8.))
                                .max_h(window.viewport_size().height - px(16.)),
                        ),
                )
                .into_any_element();
        }
        self.backdrop(body.into_any_element(), cx)
    }
}

impl Drop for Music {
    fn drop(&mut self) {
        if let Some(
            AccountOperation::Connecting(operation, _)
            | AccountOperation::Restoring(operation)
            | AccountOperation::Activating(operation),
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

impl Render for Music {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.publish_desktop();
        if self.video_fullscreen && !self.video_open {
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
        let native_video = self.video_open
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
        let fullscreen = self.video_open && window.is_fullscreen();
        let width: f32 = window.viewport_size().width.into();
        let compact = self.sidebar_collapsed || width < 1090. || (self.show_queue && width < 1440.);
        let queue_compact = width < 1200.;
        let available = width
            - if compact { 72. } else { 240. }
            - if self.show_queue && !fullscreen {
                if queue_compact { 320. } else { 400. }
            } else {
                0.
            };
        let inset = if available >= 1000. { 100. } else { 32. };
        let artist_top = self.detail_list.state.logical_scroll_top();
        let artist_art = if self.route.is_artist() && !self.video_open && artist_top.item_ix == 0 {
            let url = self
                .page
                .header
                .as_ref()
                .and_then(|header| header.thumbnail.as_deref());
            url.and_then(|url| self.artwork.get_hero(url))
        } else {
            None
        };
        let artist_background = artist_art.map(|image| {
            let offset = -artist_top.offset_in_item;
            div()
                .absolute()
                .left(px(if compact { 72. } else { 240. }))
                .right_0()
                .top(offset)
                .h(px(564.))
                .overflow_hidden()
                .child(img(image).size_full().object_fit(ObjectFit::Cover))
                .child(div().absolute().inset_0().bg(linear_gradient(
                    180.,
                    linear_color_stop(rgba(0x03030388), 0.),
                    linear_color_stop(self.theme.bg, 1.),
                )))
        });
        let header = (!fullscreen).then(|| self.header(inset, cx).into_any_element());
        let sidebar = (!fullscreen).then(|| self.sidebar(compact, cx).into_any_element());
        let content = if self.video_open {
            self.video_content(window, cx)
        } else {
            self.content(inset, available, cx)
        };
        let queue = (self.show_queue && !fullscreen).then(|| self.queue(queue_compact, cx));
        let player = self
            .library
            .queue
            .current()
            .is_some()
            .then(|| self.player_bar(width < 1120., cx));
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
            .on_scroll_wheel(cx.listener(|_, _, _, cx| cx.notify()))
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
            .children(artist_background)
            .children(header)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .children(sidebar)
                    .child(content)
                    .children(queue),
            )
            .children(player)
            .children(
                self.overlay
                    .as_ref()
                    .map(|kind| self.overlay(kind, window, cx)),
            )
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
        "menu" => "Toggle sidebar",
        "more" => "More options",
        "thumb-up" => "Like song",
        "thumb-up-filled" => "Remove from liked music",
        "thumb-down" => "Dislike song",
        "forward" => "Next items",
        "play" => "Play",
        "pause" => "Pause",
        "previous" => "Previous song",
        "next" => "Next song",
        "shuffle" => "Shuffle upcoming songs",
        "repeat" => "Cycle repeat: off, all, one",
        "heart" => "Like song",
        "heart-filled" => "Remove from liked music",
        "queue" => "Show queue",
        "video" => "Show or hide video",
        "fullscreen" => "Fullscreen",
        "exit-fullscreen" => "Exit fullscreen",
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
        .size(px(40.))
        .flex_shrink_0()
        .rounded_full()
        .flex()
        .items_center()
        .justify_center()
        .text_color(theme.text)
        .cursor_pointer()
        .tooltip(move |_, cx| cx.new(|_| Hint { text: label, theme }).into())
        .hover(move |d| d.bg(theme.raised))
        .child(icon(name, 24., theme.text))
}
fn filter_chip(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    selected: bool,
    theme: Theme,
) -> Stateful<Div> {
    div()
        .id(id)
        .h(px(36.))
        .px(px(12.))
        .flex_shrink_0()
        .rounded(px(8.))
        .flex()
        .items_center()
        .bg(if selected { theme.text } else { theme.raised })
        .text_color(if selected { theme.bg } else { theme.text })
        .text_size(px(14.))
        .cursor_pointer()
        .child(label.into())
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
fn apply_track_details(
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

fn acknowledge_song_change<'a>(
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

fn adopt_watch_queue(
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
    Ok(true)
}

fn playlist_move_down(tracks: &[Track], index: usize, has_more: bool) -> Option<Option<String>> {
    tracks.get(index + 1)?;
    match tracks.get(index + 2) {
        Some(successor) => successor.details.set_video_id.clone().map(Some),
        None if !has_more => Some(None),
        None => None,
    }
}

fn visible_cards(count: usize, offset: f32, viewport: f32, stride: f32) -> std::ops::Range<usize> {
    let offset = offset
        .max(0.)
        .min((count as f32 * stride - CARD_GAP - viewport).max(0.));
    let start = (offset / stride) as usize;
    let end = ((offset + viewport) / stride).ceil() as usize + 1;
    start.min(count)..end.min(count)
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
    use super::{
        CatalogRequests, CatalogTarget, DetailList, Playback, Query, QueueUpdate,
        acknowledge_song_change, adopt_watch_queue, apply_track_details, playlist_move_down,
        visible_cards,
    };
    use crate::{
        account::Mutation,
        catalog::{Rating, Track},
        library::Queue,
        watch::Watch,
    };
    use gpui::{ListOffset, px};
    use std::collections::HashMap;

    #[test]
    fn collection_layout_preserves_scroll_when_the_header_moves_between_columns() {
        let mut details = DetailList::new();
        details.reset_rows(100);
        details.set_inline_header(true);
        assert_eq!(details.state.item_count(), 101);
        assert_eq!(details.state.logical_scroll_top().item_ix, 0);
        details.state.scroll_to(ListOffset {
            item_ix: 31,
            offset_in_item: px(8.),
        });
        details.set_inline_header(false);
        assert_eq!(details.state.item_count(), 100);
        assert_eq!(details.state.logical_scroll_top().item_ix, 30);
        assert_eq!(details.state.logical_scroll_top().offset_in_item, px(8.));
        details.set_inline_header(true);
        assert_eq!(details.state.logical_scroll_top().item_ix, 31);
        details.reset_rows(200);
        assert_eq!(details.state.item_count(), 201);
        assert_eq!(details.state.logical_scroll_top().item_ix, 31);
    }

    #[test]
    fn issued_queue_adoption_preserves_manual_edits_and_the_selected_occurrence() {
        let track = |id: &str| Track {
            id: id.into(),
            title: id.into(),
            artist: "Artist".into(),
            album: None,
            duration: None,
            thumbnail: None,
            details: Default::default(),
        };
        let seed = track("abcdefghijk");
        let next = track("lmnopqrstuv");
        let watch = Watch {
            current: Some(seed.clone()),
            current_index: Some(1),
            queue: vec![next.clone(), seed.clone(), next.clone()],
            lyrics: None,
            related: None,
            alternatives: None,
            continuation: None,
        };
        let mut queue = Queue::default();
        queue.replace(vec![seed.clone()], 0);
        assert!(adopt_watch_queue(&mut queue, &watch, QueueUpdate::Adopt).unwrap());
        assert_eq!(queue.index, Some(1));
        assert_eq!(queue.current().unwrap().id, seed.id);
        assert_eq!(
            queue
                .tracks
                .iter()
                .map(|track| track.id.as_str())
                .collect::<Vec<_>>(),
            [next.id.as_str(), seed.id.as_str(), next.id.as_str()]
        );

        queue.replace(vec![seed.clone(), track("manual-edit")], 0);
        assert!(!adopt_watch_queue(&mut queue, &watch, QueueUpdate::Adopt).unwrap());
        assert_eq!(queue.tracks[1].id, "manual-edit");
        assert_eq!(queue.index, Some(0));
        assert!(!adopt_watch_queue(&mut queue, &watch, QueueUpdate::Preserve).unwrap());
        assert_eq!(queue.tracks[1].id, "manual-edit");

        queue.replace(vec![track("new-selection")], 0);
        assert!(!adopt_watch_queue(&mut queue, &watch, QueueUpdate::Adopt).unwrap());
        assert_eq!(queue.current().unwrap().id, "new-selection");
        assert!(adopt_watch_queue(&mut queue, &watch, QueueUpdate::Replace).unwrap());
        assert_eq!(queue.current().unwrap().id, seed.id);
    }

    #[test]
    fn page_refresh_preserves_sidebar_and_picker_requests() {
        let mut requests = CatalogRequests::new();
        requests.request(Query::Home, 0, None).unwrap();
        let old_page = requests.generations[CatalogTarget::Page as usize]
            .load(std::sync::atomic::Ordering::Relaxed);
        requests.request(Query::PlaylistChoices, 0, None).unwrap();
        requests.request(Query::SidebarPlaylists, 0, None).unwrap();
        requests.request(Query::Explore, 0, None).unwrap();
        let pending = std::iter::from_fn(|| requests.pending.try_recv().ok()).collect::<Vec<_>>();
        assert_eq!(pending.len(), 3);
        for target in CatalogTarget::ALL {
            assert_eq!(
                pending
                    .iter()
                    .filter(|job| job.query.target() == target)
                    .count(),
                1
            );
        }
        assert!(
            pending
                .iter()
                .any(|job| matches!(job.query, Query::Explore))
        );
        assert!(!requests.finish(CatalogTarget::Page, old_page));
    }

    #[test]
    fn carousel_only_renders_nearby_cards_and_clamps_after_a_shorter_refresh() {
        assert_eq!(visible_cards(100, 0., 612., 204.), 0..4);
        assert_eq!(visible_cards(100, 2040., 612., 204.), 10..14);
        assert_eq!(visible_cards(100, 100000., 612., 204.), 96..100);
        assert_eq!(visible_cards(2, 100000., 612., 204.), 0..2);
        assert_eq!(visible_cards(0, 0., 612., 204.), 0..0);
    }

    #[test]
    fn page_refresh_keeps_the_pending_playlist_picker_and_rejects_older_pages() {
        let mut requests = CatalogRequests::new();
        requests.request(Query::Home, 0, None).unwrap();
        let old_page = requests.pending.try_recv().unwrap();
        requests.request(Query::PlaylistChoices, 0, None).unwrap();
        requests.request(Query::Explore, 1, None).unwrap();
        let picker = requests.pending.try_recv().unwrap();
        let page = requests.pending.try_recv().unwrap();
        assert!(matches!(picker.query, Query::PlaylistChoices));
        assert!(matches!(page.query, Query::Explore));
        assert!(requests.pending.try_recv().is_err());
        assert!(!requests.finish(CatalogTarget::Page, old_page.id));
        assert!(requests.is_loading(CatalogTarget::Page));
        assert!(requests.finish(CatalogTarget::Page, page.id));
        assert!(requests.is_loading(CatalogTarget::PlaylistPicker));
        assert!(requests.finish(CatalogTarget::PlaylistPicker, picker.id));
        assert!(!requests.is_loading(CatalogTarget::Page));
        assert!(!requests.is_loading(CatalogTarget::PlaylistPicker));
    }

    #[test]
    fn opening_or_closing_a_picker_does_not_cancel_navigation() {
        let mut requests = CatalogRequests::new();
        requests.request(Query::Home, 0, None).unwrap();
        requests.request(Query::PlaylistChoices, 0, None).unwrap();
        requests.request(Query::PlaylistChoices, 0, None).unwrap();
        let page = requests.pending.try_recv().unwrap();
        let picker = requests.pending.try_recv().unwrap();
        assert!(matches!(page.query, Query::Home));
        assert!(matches!(picker.query, Query::PlaylistChoices));
        assert!(requests.pending.try_recv().is_err());
        requests.cancel(CatalogTarget::PlaylistPicker);
        assert!(!requests.finish(CatalogTarget::PlaylistPicker, picker.id));
        assert!(requests.is_loading(CatalogTarget::Page));
        assert!(requests.finish(CatalogTarget::Page, page.id));
    }

    #[test]
    fn pause_and_resume_keep_the_clock_frozen_until_buffering_finishes() {
        let playing = Playback::Loaded {
            paused: false,
            buffering: false,
        };
        let buffering = playing.with_buffering(true);
        assert!(!buffering.is_advancing());
        let paused = buffering.with_pause(true);
        assert!(paused.is_paused());
        assert!(!paused.is_advancing());
        let resumed = paused.with_pause(false);
        assert!(!resumed.is_paused());
        assert!(!resumed.is_advancing());
        assert!(resumed.with_buffering(false).is_advancing());
        assert!(!paused.with_buffering(false).is_advancing());
    }

    #[test]
    fn acknowledged_song_state_survives_old_reads_and_fresh_reads_can_remove_it() {
        let mut song: Track = serde_json::from_value(serde_json::json!({
            "id": "abcdefghijk", "title": "Song", "artist": "Artist"
        }))
        .unwrap();
        song.details.rating = Some(Rating::Indifferent);
        song.details.in_library = Some(false);
        song.details.add_library = Some("synthetic-add".into());
        song.details.remove_library = Some("synthetic-remove".into());
        let mut tracks = vec![song.clone(), song.clone()];
        tracks[0].details.set_video_id = Some("first".into());
        tracks[1].details.set_video_id = Some("second".into());
        let mut ratings = HashMap::new();
        acknowledge_song_change(
            &mut tracks,
            &mut ratings,
            &Mutation::RateSong {
                track: song.clone(),
                rating: Rating::Like,
            },
        );
        acknowledge_song_change(
            &mut tracks,
            &mut ratings,
            &Mutation::SongLibrary {
                track: song.clone(),
                save: true,
            },
        );
        apply_track_details(&mut tracks, &mut ratings, &song, false);
        assert_eq!(ratings[&song.id], Rating::Like);
        assert!(
            tracks
                .iter()
                .all(|track| track.details.rating == Some(Rating::Like)
                    && track.details.in_library == Some(true))
        );

        apply_track_details(&mut tracks, &mut ratings, &song, true);
        assert_eq!(ratings[&song.id], Rating::Indifferent);
        assert!(
            tracks
                .iter()
                .all(|track| track.details.in_library == Some(false))
        );
        assert_eq!(tracks[0].details.set_video_id.as_deref(), Some("first"));
        assert_eq!(tracks[1].details.set_video_id.as_deref(), Some("second"));

        song.details.rating = None;
        song.details.in_library = None;
        apply_track_details(&mut tracks, &mut ratings, &song, true);
        assert!(!ratings.contains_key(&song.id));
        assert!(
            tracks
                .iter()
                .all(|track| track.details.rating.is_none() && track.details.in_library.is_none())
        );
    }

    #[test]
    fn acknowledged_library_changes_update_both_visible_copies_without_a_queue_match() {
        let mut song: Track = serde_json::from_value(serde_json::json!({
            "id":"abcdefghijk", "title":"Visible song", "artist":"Artist"
        }))
        .unwrap();
        song.details.in_library = Some(false);
        song.details.add_library = Some("synthetic-add".into());
        song.details.remove_library = Some("synthetic-remove".into());
        let mut page = vec![crate::catalog::Item::Track(song.clone())];
        let mut rows = page.clone();
        let mut ratings = HashMap::new();
        for save in [true, false] {
            acknowledge_song_change(
                page.iter_mut()
                    .chain(&mut rows)
                    .filter_map(|item| match item {
                        crate::catalog::Item::Track(track) => Some(track),
                        _ => None,
                    }),
                &mut ratings,
                &Mutation::SongLibrary {
                    track: song.clone(),
                    save,
                },
            );
            for item in page.iter().chain(&rows) {
                let crate::catalog::Item::Track(track) = item else {
                    panic!("expected song")
                };
                assert_eq!(track.details.in_library, Some(save));
                assert_eq!(track.details.add_library.as_deref(), Some("synthetic-add"));
                assert_eq!(
                    track.details.remove_library.as_deref(),
                    Some("synthetic-remove")
                );
            }
        }
    }

    #[test]
    fn moving_down_requires_the_successor_or_a_confirmed_playlist_end() {
        let mut tracks: Vec<Track> = ["first", "second", "third"]
            .into_iter()
            .map(|occurrence| {
                let mut track: Track = serde_json::from_value(serde_json::json!({
                    "id": "abcdefghijk", "title": "Repeated song", "artist": "Artist"
                }))
                .unwrap();
                track.details.set_video_id = Some(occurrence.into());
                track
            })
            .collect();
        assert_eq!(
            playlist_move_down(&tracks, 0, true),
            Some(Some("third".into()))
        );
        assert_eq!(playlist_move_down(&tracks, 1, true), None);
        assert_eq!(playlist_move_down(&tracks, 1, false), Some(None));
        assert_eq!(playlist_move_down(&tracks, 2, false), None);
        tracks[2].details.set_video_id = None;
        assert_eq!(playlist_move_down(&tracks, 0, false), None);
    }

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
            Playback::Loaded {
                paused: false,
                buffering: false,
            },
            Playback::Loaded {
                paused: true,
                buffering: false,
            },
            Playback::Loaded {
                paused: false,
                buffering: true,
            },
        ] {
            assert!(!playback.retry());
        }
    }

    #[test]
    fn play_starts_a_stopped_wait_and_loading_toggles_preserve_the_retry_budget() {
        let playing = Playback::Loaded {
            paused: false,
            buffering: false,
        };
        let paused = Playback::Loaded {
            paused: true,
            buffering: false,
        };
        assert_eq!(Playback::Stopped.toggled(), playing);
        assert_eq!(paused.toggled(), playing);
        assert_eq!(playing.toggled(), paused);
        let loading = Playback::Loading {
            paused: false,
            retried: true,
        };
        assert_eq!(
            loading.toggled(),
            Playback::Loading {
                paused: true,
                retried: true,
            }
        );
        assert_eq!(loading.toggled().toggled(), loading);
    }
}
