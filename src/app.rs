//! Shared Music state and module wiring; public startup API stays unchanged.

use crate::{
    account::{self, Connection, Effect, Mutation, Privacy, Section},
    artwork::Artwork,
    auth::{self, Operation, Session, SharedSession},
    catalog::{
        BrowseLink, Catalog, Collection, Continuation, HeaderArtist, Item, Page, PlaylistTarget,
        Rating, Shelf, ShelfLayout, Track, WatchTarget,
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
    time::{Duration, Instant},
};

mod account_state;
mod bootstrap;
mod components;
mod dialogs;
mod header;
mod interaction;
mod library_actions;
mod navigation;
mod pages;
mod playback;
mod player_bar;
mod queue_panel;
mod sidebar;
mod video;
mod window;

use account_state::*;
use components::*;
use dialogs::settings::{SettingMenu, SettingsSection};
use dialogs::*;
use interaction::Drag;
use navigation::*;
#[cfg(test)]
use pages::shelves::visible_cards;
use pages::{DetailList, ListRow};
#[cfg(test)]
use playback::queue::adopt_watch_queue;
use playback::*;
use player_bar::PlayerBar;
use window::MusicContent;

const CARD_WIDTH: f32 = 180.;
const CARD_GAP: f32 = 24.;
const SHELF_HEIGHT: f32 = 350.;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Startup {
    Restore,
    Connect,
    Guest,
}

pub struct Music {
    content_view: Entity<MusicContent>,
    player_bar: Entity<PlayerBar>,
    theme: Theme,
    session: SharedSession,
    account: Option<Connection>,
    account_commands: async_channel::Sender<account::Command>,
    account_worker: Option<account::Worker>,
    account_operation: Option<AccountOperation>,
    next_session_refresh: Option<Instant>,
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
    watch_pending: Option<PlaybackQuery>,
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
    settings_section: SettingsSection,
    settings_menu: Option<SettingMenu>,
    overlay_anchor: Option<Point<Pixels>>,
    overlay_subscription: Option<Subscription>,
    drag: Option<Drag>,
    seek_bounds: Rc<Cell<Bounds<Pixels>>>,
    volume_bounds: Rc<Cell<Bounds<Pixels>>>,
    drag_position: Option<f64>,
    _subscriptions: Vec<Subscription>,
}

#[cfg(test)]
mod tests;

#[cfg(test)]
#[path = "native_account_tests.rs"]
mod native_account_tests;

#[cfg(test)]
mod render_tests;
