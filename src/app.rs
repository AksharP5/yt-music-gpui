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
    library::{LastPosition, Library, LibraryWriter, Queue, Repeat, Store},
    media_controls,
    player::{
        AudioQuality, PlayOptions, PlaybackSpeed, Player, PlayerEvent, PlayerOptions,
        VideoAttachment, VideoQuality, VideoStatus,
    },
    playlist_form::{PlaylistForm, PlaylistFormEvent},
    queue_source::{self, PageRequest, PlaybackOrder},
    search::{Search, SearchFilter, SearchScope},
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
mod player_page;
mod queue_panel;
mod sidebar;
mod suggestions;
mod sync;
mod video;
mod window;

use account_state::*;
use components::*;
use dialogs::settings::{SettingMenu, SettingsSection};
use dialogs::*;
use interaction::Drag;
use library_actions::is_disliked;
pub use navigation::HomePrefetch;
use navigation::*;
#[cfg(test)]
use pages::shelves::{carousel_page, visible_cards};
use pages::{DetailList, ListRow};
#[cfg(test)]
use playback::queue::adopt_watch_queue;
use playback::*;
use player_bar::PlayerBar;
use player_page::PlayerPage;
use window::MusicContent;

pub(crate) const HEADER_HEIGHT: f32 = 64.;
/// The web's breakpoint below which the guide shrinks to its mini form and
/// opens in full only as a drawer.
const GUIDE_DRAWER_BELOW: f32 = 936.;
const CARD_GAP: f32 = 24.;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Startup {
    Restore,
    Connect,
    Guest,
}

pub struct Music {
    #[cfg(test)]
    renders: usize,
    #[cfg(test)]
    focus_restores: usize,
    #[cfg(test)]
    shelf_render_times: Vec<std::time::Duration>,
    content_view: Entity<MusicContent>,
    player_bar: Entity<PlayerBar>,
    theme: Theme,
    session: SharedSession,
    account: Option<Connection>,
    account_commands: async_channel::Sender<account::Command>,
    account_worker: Option<account::Worker>,
    account_operation: Option<AccountOperation>,
    next_session_refresh: Option<Instant>,
    pending_mutations: HashMap<String, Vec<Track>>,
    ratings: HashMap<String, Rating>,
    account_revision: u64,
    account_generation: u64,
    history_generation: u64,
    sync: sync::SyncState,
    track_state: track_state::Worker,
    track_state_id: Option<u64>,
    focus: FocusHandle,
    sidebar_focus: VirtualListFocus,
    queue_focus: VirtualListFocus,
    search: Entity<SearchBox>,
    /// Below 936px, whether the search button has opened its field.
    search_open: bool,
    route: Route,
    page: Page,
    rows: Vec<Item>,
    list_rows: Vec<ListRow>,
    suggestions: Option<suggestions::Suggestions>,
    shelf_scroll: HashMap<usize, ScrollHandle>,
    shelf_heights: Vec<Option<pages::shelves::ShelfHeight>>,
    /// The artist heading height the hero photo was last sized from.
    hero_heading: Option<Pixels>,
    list_scroll: UniformListScrollHandle,
    detail_list: DetailList,
    /// The wide guide's mini state. Below 936px the guide is always mini.
    sidebar_collapsed: bool,
    /// Below 936px, the full guide opens as a drawer over the dimmed window.
    guide_drawer: bool,
    sidebar_playlists: Vec<Collection>,
    sidebar_error: Option<String>,
    sidebar_scroll: UniformListScrollHandle,
    back: Vec<NavigationState>,
    forward: Vec<NavigationState>,
    page_cache: VecDeque<CachedPage>,
    forward_page: Option<ForwardPage>,
    list_id: usize,
    error: Option<String>,
    notice: Option<String>,
    toast: Option<Toast>,
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
    go_chord: interaction::GoChord,
    watch_pending: Option<PlaybackQuery>,
    song_text: song_text::Worker,
    artwork: Artwork,
    library: Library,
    store: Option<LibraryWriter>,
    player: Option<Player>,
    media_controls: Option<media_controls::MediaControls>,
    seekable: bool,
    video: Option<VideoAttachment>,
    /// The full-window player page, showing artwork or video beside its tabs.
    player_open: bool,
    sort_focus: FocusHandle,
    /// A playlist's Find in playlist bar, which filters its loaded rows.
    playlist_find: Option<pages::PlaylistFind>,
    video_fullscreen: bool,
    video_transparent: bool,
    video_error: Option<String>,
    video_click: Option<Task<()>>,
    active_id: Option<u64>,
    /// The first page has loaded, so playback lookups no longer compete with startup.
    startup_settled: bool,
    history_track: Option<u64>,
    listening: playback::listening::ListeningState,
    playback: Playback,
    position: f64,
    pending_seek: Option<SeekIntent>,
    /// The player's length for the video it measured last, by video ID.
    measured_duration: Option<(String, f64)>,
    /// How far mpv has downloaded the playback it names, for the seek bar.
    buffered: Option<(u64, f64)>,
    player_page: PlayerPage,
    overlay: Option<Overlay>,
    settings_section: SettingsSection,
    /// A setting's open choices, with the field that opened them.
    settings_menu: Option<(SettingMenu, Option<FocusHandle>)>,
    overlay_anchor: Option<Point<Pixels>>,
    /// The "More options" button whose menu is open, so the menu can align to it.
    overlay_button: Option<Bounds<Pixels>>,
    overlay_focus: FocusHandle,
    menu_scroll: ScrollHandle,
    collection_header_scroll: ScrollHandle,
    picker_focus: VirtualListFocus<crate::playlist_choices::Destination>,
    picker_scroll: UniformListScrollHandle,
    settings_focus: FocusHandle,
    /// The control focused when the overlay opened, which regains focus when it closes.
    overlay_opener: Option<FocusHandle>,
    overlay_subscription: Option<Subscription>,
    drag: Option<Drag>,
    seek_bounds: Rc<Cell<Bounds<Pixels>>>,
    volume_bounds: Rc<Cell<Bounds<Pixels>>>,
    _subscriptions: Vec<Subscription>,
}

#[cfg(test)]
mod tests;

// These need an isolated Linux desktop session; see CONTRIBUTING.md.
#[cfg(test)]
#[cfg(unix)]
mod native_tests;
