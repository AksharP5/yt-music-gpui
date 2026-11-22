use crate::auth::PlaybackCredentials;
#[cfg(not(target_os = "macos"))]
use crate::auth::private_runtime;
#[cfg(target_os = "macos")]
use crate::auth::validate_private_runtime;
use crate::os::{AsWaitable, IpcStream, Waitable, Waker, ipc_path, wait_readable};
use crate::video::NativeVideo;
use anyhow::{Context, Result, bail, ensure};
use gpui::{AnyWindowHandle, App, BackgroundExecutor, Bounds, Pixels, Subscription, Window};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::{
    cell::RefCell,
    fs,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::{Duration, Instant, SystemTime},
};

mod mpv;
mod native;
mod stream;
mod ytdlp;

use mpv::{
    MpvCore, PlaybackState, SeekRequest, load_command, read_message, request, write_command,
    write_pause,
};
use stream::{
    NativeResolver, PreparedTarget, ResolvedStream, ResolverLane, StreamKey, StreamResolver,
    stream_refresh_time,
};
#[cfg(all(test, unix))]
pub(crate) use ytdlp::TEST_YTDLP;
use ytdlp::{
    ExtractorCredentials, account_credentials, account_playback_supported, write_account_extractor,
};
pub(crate) use ytdlp::{find_ytdlp, private_tool};

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "f64", into = "f64")]
pub struct PlaybackSpeed(f64);

impl PlaybackSpeed {
    pub const ALL: [Self; 8] = [
        Self(0.25),
        Self(0.5),
        Self(0.75),
        Self(1.),
        Self(1.25),
        Self(1.5),
        Self(1.75),
        Self(2.),
    ];
}

impl Default for PlaybackSpeed {
    fn default() -> Self {
        Self(1.)
    }
}

impl TryFrom<f64> for PlaybackSpeed {
    type Error = &'static str;

    fn try_from(value: f64) -> Result<Self, Self::Error> {
        if !value.is_finite() || !(0.25..=2.).contains(&value) {
            return Err("Playback speed must be between 0.25 and 2");
        }
        Ok(Self(value))
    }
}

impl From<PlaybackSpeed> for f64 {
    fn from(value: PlaybackSpeed) -> Self {
        value.0
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioQuality {
    Low,
    #[default]
    Normal,
    High,
}

impl AudioQuality {
    pub const ALL: [Self; 3] = [Self::Low, Self::Normal, Self::High];

    pub fn label(self) -> &'static str {
        match self {
            Self::Low => "Low",
            Self::Normal => "Normal",
            Self::High => "High",
        }
    }

    fn selector(self) -> &'static str {
        // yt-dlp exposes YouTube's audioQuality in format_note. Its VBR streams
        // can exceed the nominal bitrate, so prefer the service's quality class.
        match self {
            Self::Low => {
                "bestaudio[format_note~='(^|, )(ultralow|low)(,|$)']/bestaudio[abr<=64]/worstaudio/best"
            }
            Self::Normal => {
                "bestaudio[format_note~='(^|, )(ultralow|low|medium)(,|$)']/bestaudio[abr<=128]/bestaudio/best"
            }
            Self::High => "bestaudio/best",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum VideoQuality {
    #[serde(rename = "144p")]
    P144,
    #[serde(rename = "240p")]
    P240,
    #[serde(rename = "360p")]
    P360,
    #[serde(rename = "480p")]
    P480,
    #[default]
    #[serde(rename = "720p")]
    P720,
    #[serde(rename = "1080p")]
    P1080,
    #[serde(rename = "1440p")]
    P1440,
    #[serde(rename = "2160p")]
    P2160,
}

impl VideoQuality {
    pub const ALL: [Self; 8] = [
        Self::P144,
        Self::P240,
        Self::P360,
        Self::P480,
        Self::P720,
        Self::P1080,
        Self::P1440,
        Self::P2160,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::P144 => "144p",
            Self::P240 => "240p",
            Self::P360 => "360p",
            Self::P480 => "480p",
            Self::P720 => "720p",
            Self::P1080 => "1080p",
            Self::P1440 => "1440p",
            Self::P2160 => "2160p",
        }
    }

    fn height(self) -> u16 {
        match self {
            Self::P144 => 144,
            Self::P240 => 240,
            Self::P360 => 360,
            Self::P480 => 480,
            Self::P720 => 720,
            Self::P1080 => 1080,
            Self::P1440 => 1440,
            Self::P2160 => 2160,
        }
    }

    fn selector(self, audio: AudioQuality) -> String {
        let height = self.height();
        let mut branches: Vec<_> = audio
            .selector()
            .split('/')
            // Progressive `best` belongs only in the bounded fallback below.
            .filter(|audio| *audio != "best")
            .map(|audio| format!("bestvideo[height<={height}]+{audio}"))
            .collect();
        branches.push(format!("best[height<={height}]"));
        branches.join("/")
    }

    fn uses_full_resolver(self) -> bool {
        self.height() > 1080
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct PlayOptions {
    pub quality: AudioQuality,
    pub video: Option<VideoQuality>,
    pub start: f64,
    pub paused: bool,
    /// Skip the fast lookup. The automatic reload after a stream fails to open
    /// sets this, in case YouTube rejected the fast lookup's stream.
    pub full_lookup: bool,
}

pub struct PlayerOptions {
    pub credentials: Option<PlaybackCredentials>,
    pub check_account_at_startup: bool,
    pub initial_video: Option<VideoQuality>,
    pub volume: f64,
    pub speed: PlaybackSpeed,
    pub audio_output: Option<String>,
    pub captions: bool,
}

impl Default for PlayerOptions {
    fn default() -> Self {
        Self {
            credentials: None,
            check_account_at_startup: false,
            initial_video: None,
            volume: 60.,
            speed: PlaybackSpeed::default(),
            audio_output: None,
            captions: false,
        }
    }
}

#[derive(Debug)]
pub enum PlayerEvent {
    Started {
        track_id: u64,
        paused: bool,
        /// Whether the loaded video has captions to show.
        captions: bool,
    },
    Position {
        track_id: u64,
        seconds: f64,
    },
    SeekFinished {
        track_id: u64,
        revision: u64,
        result: Result<f64, String>,
    },
    Duration {
        track_id: u64,
        seconds: f64,
    },
    Paused {
        track_id: u64,
        paused: bool,
    },
    Buffering {
        track_id: u64,
        buffering: bool,
    },
    /// How far into the song mpv has downloaded, for the seek bar's buffered range.
    Buffered {
        track_id: u64,
        seconds: f64,
    },
    Ended {
        track_id: u64,
    },
    /// The paused song's stream URL is about to expire before mpv buffered
    /// the rest of it. Reload it in place before playback resumes.
    StreamExpiring {
        track_id: u64,
    },
    Stopped {
        track_id: u64,
    },
    Seekable {
        track_id: u64,
        seekable: bool,
    },
    Error {
        track_id: Option<u64>,
        message: String,
    },
    Notice(String),
    #[cfg(test)]
    Prepared {
        video_id: String,
    },
    VideoChanged,
    VideoError {
        message: String,
    },
    Speed(PlaybackSpeed),
}

enum PlayerCommand {
    Prepare(Option<PreparedTarget>),
    Play {
        track_id: u64,
        url: String,
        title: String,
        options: PlayOptions,
    },
    Pause(bool),
    Captions(bool),
    Seek(SeekRequest),
    Json(Value),
    Credentials {
        next: Option<PlaybackCredentials>,
        reply: async_channel::Sender<Result<(), String>>,
    },
    AttachVideo {
        video: Arc<NativeVideo>,
    },
    DetachVideo {
        video: Arc<NativeVideo>,
        reply: async_channel::Sender<()>,
    },
    Stop,
    Quit,
}

#[derive(Clone)]
struct CommandSender {
    sender: Sender<PlayerCommand>,
    wake: Arc<Waker>,
}

fn command_channel() -> Result<(CommandSender, Receiver<PlayerCommand>)> {
    let wake = Arc::new(Waker::new().context("Could not create the player command wakeup")?);
    let (sender, receiver) = mpsc::channel();
    Ok((CommandSender { sender, wake }, receiver))
}

impl CommandSender {
    fn send(&self, command: PlayerCommand) -> Result<(), mpsc::SendError<PlayerCommand>> {
        self.sender.send(command)?;
        self.wake.wake();
        Ok(())
    }
}

fn wait_for_player_input(
    socket: &IpcStream,
    wake: &Waker,
    resolver: Option<Waitable>,
    deadline: Option<Instant>,
) -> Result<bool> {
    let [socket_ready, woken, _] = wait_readable(
        [
            Some(socket.as_waitable()),
            Some(wake.as_waitable()),
            resolver,
        ],
        deadline,
    )
    .context("Could not wait for the player")?;
    if woken {
        wake.drain()
            .context("Could not read the player command wakeup")?;
    }
    Ok(socket_ready)
}

pub struct Player {
    commands: CommandSender,
    events: async_channel::Receiver<PlayerEvent>,
    next_id: AtomicU64,
    next: Mutex<Option<PreparedTarget>>,
    lookup: Option<native::Lookup>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Player {
    pub fn new(options: PlayerOptions) -> Result<Self> {
        Self::with_lookup(options, find_ytdlp()?, native::Lookup::new().ok())
    }

    #[cfg(test)]
    #[cfg(unix)]
    fn with_extractor(options: PlayerOptions, ytdlp: PathBuf) -> Result<Self> {
        Self::with_lookup(options, ytdlp, None)
    }

    fn with_lookup(
        options: PlayerOptions,
        ytdlp: PathBuf,
        lookup: Option<native::Lookup>,
    ) -> Result<Self> {
        let (commands, receiver) = command_channel()?;
        let wake = commands.wake.clone();
        let (events, event_receiver) = async_channel::unbounded();
        let warm_up = lookup.clone();
        let worker = thread::Builder::new()
            .name("music-player".into())
            .spawn(move || {
                if let Err(error) = run(options, receiver, &wake, &events, &ytdlp, lookup) {
                    let _ = events.send_blocking(PlayerEvent::Error {
                        track_id: None,
                        message: format!("{error:#}"),
                    });
                }
            })?;
        Ok(Self {
            commands,
            events: event_receiver,
            next_id: AtomicU64::new(1),
            next: Mutex::new(None),
            lookup: warm_up,
            worker: Some(worker),
        })
    }

    /// Fetch the fast lookup's visitor ID in the background, so the first
    /// click on an unprepared song skips that page load.
    pub fn warm_up(&self) {
        if let Some(lookup) = self.lookup.clone() {
            let _ = thread::Builder::new()
                .name("lookup-warm-up".into())
                .spawn(move || lookup.warm_up());
        }
    }

    pub fn play(&self, video_id: &str, title: &str, options: PlayOptions) -> Result<u64> {
        if video_id.len() != 11
            || !video_id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
        {
            bail!("This song has an invalid YouTube video ID.");
        }
        self.play_url(
            format!("https://music.youtube.com/watch?v={video_id}"),
            title,
            options,
        )
    }

    /// Resolve only the next candidate. Replacing/clearing it cancels old work.
    /// Pass the natural successor, respecting repeat/autoplay. Call after queue edits
    /// and quality changes. mpv can transition to it at EOF without reopening its stream.
    pub fn prepare_next(&self, video_id: Option<&str>, options: PlayOptions) -> Result<()> {
        if let Some(id) = video_id {
            ensure!(
                crate::catalog::valid_video_id(id),
                "Invalid next YouTube video ID"
            );
        }
        let mut next = self.next.lock().unwrap_or_else(PoisonError::into_inner);
        let unchanged = match (next.as_ref(), video_id) {
            (None, None) => true,
            (Some(next), Some(id)) => {
                next.key
                    .url
                    .strip_prefix("https://music.youtube.com/watch?v=")
                    == Some(id)
                    && next.key.quality == options.quality
                    && next.key.video == options.video
            }
            _ => false,
        };
        if unchanged {
            return Ok(());
        }
        let key = video_id.map(|id| StreamKey {
            url: format!("https://music.youtube.com/watch?v={id}"),
            quality: options.quality,
            video: options.video,
        });
        *next = key.map(|key| PreparedTarget {
            lane: ResolverLane::Fast,
            key,
            track_id: self.next_id.fetch_add(1, Ordering::Relaxed),
        });
        self.send(PlayerCommand::Prepare(next.clone()))
    }

    #[cfg(test)]
    #[cfg(unix)]
    pub(crate) fn next_preparation(&self) -> Option<String> {
        self.next
            .lock()
            .unwrap()
            .as_ref()
            .map(|next| next.key.url.clone())
    }

    fn play_url(&self, url: String, title: &str, options: PlayOptions) -> Result<u64> {
        ensure!(
            options.start.is_finite() && options.start >= 0.,
            "Invalid playback start position."
        );
        let next = self
            .next
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        // A full lookup reload gets a fresh ID, so late events from a prepared
        // occurrence of the same song can never be mistaken for it.
        let track_id = next
            .filter(|next| {
                !options.full_lookup
                    && options.start == 0.
                    && next.key
                        == StreamKey {
                            url: url.clone(),
                            quality: options.quality,
                            video: options.video,
                        }
            })
            .map_or_else(
                || self.next_id.fetch_add(1, Ordering::Relaxed),
                |next| next.track_id,
            );
        self.send(PlayerCommand::Play {
            track_id,
            url,
            title: title.to_owned(),
            options,
        })?;
        Ok(track_id)
    }

    /// A successful response confirms the old extractor stopped and the new credentials are installed.
    pub fn set_credentials(
        &self,
        credentials: PlaybackCredentials,
    ) -> Result<async_channel::Receiver<Result<(), String>>> {
        self.next
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let (reply, response) = async_channel::bounded(1);
        self.send(PlayerCommand::Credentials {
            next: Some(credentials),
            reply,
        })?;
        Ok(response)
    }
    /// A successful response confirms extractor termination and credential revocation.
    /// A closed response channel means the player exited without confirming cleanup.
    pub fn clear_credentials(&self) -> Result<async_channel::Receiver<Result<(), String>>> {
        self.next
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let (reply, response) = async_channel::bounded(1);
        self.send(PlayerCommand::Credentials { next: None, reply })?;
        Ok(response)
    }
    pub fn pause(&self, paused: bool) -> Result<()> {
        self.send(PlayerCommand::Pause(paused))
    }
    pub fn stop(&self) -> Result<()> {
        self.next
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        self.send(PlayerCommand::Stop)
    }
    pub fn seek(&self, track_id: u64, seconds: f64) -> Result<u64> {
        if !seconds.is_finite() {
            bail!("Invalid seek position.");
        }
        let revision = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.send(PlayerCommand::Seek(SeekRequest {
            track_id,
            revision,
            seconds: seconds.max(0.),
        }))?;
        Ok(revision)
    }
    pub fn set_volume(&self, percent: f64) -> Result<()> {
        if !percent.is_finite() {
            bail!("Invalid volume.");
        }
        self.command(json!(["set_property", "volume", percent.clamp(0., 100.)]))
    }
    pub fn set_captions(&self, enabled: bool) -> Result<()> {
        self.send(PlayerCommand::Captions(enabled))
    }
    pub fn set_speed(&self, speed: PlaybackSpeed) -> Result<()> {
        self.command(json!(["set_property", "speed", f64::from(speed)]))
    }
    /// Create with the view, before paint/input callbacks borrow the platform window.
    pub fn video_attachment(&self, window: &mut Window, cx: &mut App) -> VideoAttachment {
        let binding: Rc<RefCell<VideoState>> = Rc::new(RefCell::new(VideoState::Detached));
        let cleanup = Rc::new(RefCell::new(Vec::<async_channel::Receiver<()>>::new()));
        let weak = Rc::downgrade(&binding);
        let closing = Rc::new(std::cell::Cell::new(false));
        let close_pending = closing.clone();
        let pending = cleanup.clone();
        window.on_window_should_close(cx, move |window, cx| {
            if close_pending.replace(true) {
                return false;
            }
            if let Some(binding) = weak.upgrade() {
                *binding.borrow_mut() = VideoState::Detached;
            }
            let completions = pending.borrow().clone();
            if completions.iter().all(async_channel::Receiver::is_closed) {
                return true;
            }
            window
                .spawn(cx, async move |cx| {
                    for completion in completions {
                        let _ = completion.recv().await;
                    }
                    let _ = cx.update(|window, _| window.remove_window());
                })
                .detach();
            false
        });
        let weak = Rc::downgrade(&binding);
        let pending = cleanup.clone();
        let quit = cx.on_app_quit(move |_| {
            if let Some(binding) = weak.upgrade() {
                *binding.borrow_mut() = VideoState::Detached;
            }
            // GPUI destroys windows before awaiting quit futures. Drain here while
            // the borrowed Wayland parent/display and X11 parent are still live.
            for completion in pending.borrow().iter() {
                let _ = completion.recv_blocking();
            }
            async {}
        });
        VideoAttachment {
            binding,
            commands: self.commands.clone(),
            window: window.window_handle(),
            cleanup,
            executor: cx.background_executor().clone(),
            closing,
            _quit: quit,
        }
    }

    pub fn shutdown(&mut self) -> Result<()> {
        if let Some(worker) = self.worker.take() {
            let _ = self.commands.send(PlayerCommand::Quit);
            worker
                .join()
                .map_err(|_| anyhow::anyhow!("The player worker panicked during shutdown"))?;
        }
        Ok(())
    }
    #[cfg(test)]
    pub fn try_recv(&self) -> Option<PlayerEvent> {
        self.events.try_recv().ok()
    }
    pub fn events(&self) -> async_channel::Receiver<PlayerEvent> {
        self.events.clone()
    }
    fn command(&self, command: Value) -> Result<()> {
        self.send(PlayerCommand::Json(command))
    }
    fn send(&self, command: PlayerCommand) -> Result<()> {
        self.commands
            .send(command)
            .map_err(|_| anyhow::anyhow!("The player stopped. Restart the app to reconnect."))
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

pub struct VideoAttachment {
    binding: Rc<RefCell<VideoState>>,
    commands: CommandSender,
    window: AnyWindowHandle,
    _quit: Subscription,
    cleanup: Rc<RefCell<Vec<async_channel::Receiver<()>>>>,
    executor: BackgroundExecutor,
    closing: Rc<std::cell::Cell<bool>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoStatus {
    Detached,
    Loading,
    Ready,
    Failed,
}

enum VideoState {
    Detached,
    Attached(VideoBinding),
    Failed(String),
}

impl VideoAttachment {
    pub fn status(&self) -> VideoStatus {
        match &*self.binding.borrow() {
            VideoState::Detached => VideoStatus::Detached,
            VideoState::Failed(_) => VideoStatus::Failed,
            VideoState::Attached(binding) => match binding.video.ready() {
                Ok(false) => VideoStatus::Loading,
                Ok(true) => VideoStatus::Ready,
                Err(_) => VideoStatus::Failed,
            },
        }
    }

    /// Attach if absent; otherwise update geometry at the window's current scale.
    /// A failure stays latched until detach is called, so paint cannot retry it.
    pub fn attach(&self, bounds: Bounds<Pixels>, window: &Window) -> Result<()> {
        ensure!(
            self.window == window.window_handle(),
            "The video attachment belongs to another window"
        );
        if self.closing.get() {
            return Ok(());
        }
        let mut state = self.binding.borrow_mut();
        if let VideoState::Failed(message) = &*state {
            bail!("{message}");
        }
        if let VideoState::Attached(binding) = &*state {
            let result = binding
                .video
                .ready()
                .and_then(|_| binding.video.set_bounds(bounds, window.scale_factor()));
            if let Err(error) = &result {
                *state = VideoState::Failed(error.to_string());
            }
            return result;
        }
        let result: Result<VideoBinding> = (|| {
            let video = Arc::new(NativeVideo::new(bounds, window)?);
            self.commands
                .send(PlayerCommand::AttachVideo {
                    video: video.clone(),
                })
                .context("The player stopped")?;
            Ok(VideoBinding {
                video,
                commands: self.commands.clone(),
                cleanup: self.cleanup.clone(),
                executor: self.executor.clone(),
            })
        })();
        match result {
            Ok(binding) => {
                *state = VideoState::Attached(binding);
                Ok(())
            }
            Err(error) => {
                *state = VideoState::Failed(error.to_string());
                Err(error)
            }
        }
    }

    /// Leave the window rectangles of dialogs and menus out of the video, for the
    /// platforms that draw it above the window.
    pub fn set_cutouts(&self, cutouts: &[Bounds<Pixels>], window: &Window) {
        if let VideoState::Attached(binding) = &*self.binding.borrow() {
            binding.video.set_cutouts(cutouts, window.scale_factor());
        }
    }

    /// Retire the renderer off the UI thread. Window close and app quit drain cleanup.
    /// Also clears a previous failure to allow an explicit user retry.
    pub fn detach(&self) {
        *self.binding.borrow_mut() = VideoState::Detached;
    }
}

impl Drop for VideoAttachment {
    fn drop(&mut self) {
        self.detach();
    }
}

struct VideoBinding {
    video: Arc<NativeVideo>,
    commands: CommandSender,
    cleanup: Rc<RefCell<Vec<async_channel::Receiver<()>>>>,
    executor: BackgroundExecutor,
}

impl Drop for VideoBinding {
    fn drop(&mut self) {
        let (reply, received) = async_channel::bounded(1);
        let (done, completion) = async_channel::bounded::<()>(1);
        let mut cleanup = self.cleanup.borrow_mut();
        cleanup.retain(|completion| !completion.is_closed());
        cleanup.push(completion);
        let _ = self.commands.send(PlayerCommand::DetachVideo {
            video: self.video.clone(),
            reply,
        });
        let video = self.video.clone();
        self.executor
            .spawn(async move {
                // A closed channel also fences actor failure: its renderer guard is
                // dropped before the command receiver and before the mpv core.
                let _ = received.recv().await;
                video.destroy();
                drop(done);
            })
            .detach();
    }
}

struct AttachedVideo {
    video: Arc<NativeVideo>,
    ready: bool,
}

impl Drop for AttachedVideo {
    fn drop(&mut self) {
        self.video.stop();
    }
}

fn replace_playback_credentials(
    socket: &mut IpcStream,
    pending: &mut Vec<u8>,
    current: &mut Option<ExtractorCredentials>,
    source: Option<PlaybackCredentials>,
    plugin_dir: &Path,
    state: &mut PlaybackState,
    events: &async_channel::Sender<PlayerEvent>,
) -> Result<()> {
    let next = source.map(|source| ExtractorCredentials {
        plugin_dir: plugin_dir.to_owned(),
        source,
    });
    request(socket, pending, json!(["stop"]), |value| {
        state.message(value, events)
    })?;
    let client = request(socket, pending, json!(["client_name"]), |value| {
        state.message(value, events)
    })?;
    let client = client
        .as_str()
        .context("mpv did not identify its IPC client")?;
    let marker = json!(["yt-music-credentials-stopped"]);
    let mut marker_seen = false;
    // mpv's stop clears the playlist on the core thread. A targeted message then
    // joins the same FIFO as start-file/end-file (mpv player/client.c), unlike IPC
    // replies. Consume it before trusting file_active: a start may still be queued.
    request(
        socket,
        pending,
        json!(["script-message-to", client, marker[0]]),
        |value| {
            marker_seen |= value["event"] == "client-message" && value["args"] == marker;
            state.message(value, events);
        },
    )?;
    // Drain the old file's events before accepting loads under the new identity.
    let deadline = Instant::now() + Duration::from_secs(5);
    while !marker_seen || state.file_active {
        let value = read_message(socket, pending, deadline)?;
        marker_seen |= value["event"] == "client-message" && value["args"] == marker;
        state.message(value, events);
    }
    *current = next;
    Ok(())
}

struct PendingPlay {
    target: PreparedTarget,
    title: String,
    options: PlayOptions,
}

fn playback_runtime(parent: &Path) -> Result<tempfile::TempDir> {
    let mut builder = tempfile::Builder::new();
    builder.prefix("yt-music-gpui-");
    // On Windows the folder inherits the private runtime's access instead.
    #[cfg(unix)]
    builder.permissions(fs::Permissions::from_mode(0o700));
    builder
        .tempdir_in(parent)
        .context("Could not create the private playback directory")
}

fn run(
    mut options: PlayerOptions,
    commands: Receiver<PlayerCommand>,
    wake: &Arc<Waker>,
    events: &async_channel::Sender<PlayerEvent>,
    ytdlp: &Path,
    lookup: Option<native::Lookup>,
) -> Result<()> {
    // mpv cannot bind a control socket path longer than 102 bytes on macOS,
    // and $TMPDIR can be far longer. /tmp is short and shared, so the folder
    // created there is checked to be ours alone before anything goes in it.
    #[cfg(target_os = "macos")]
    let runtime = {
        let runtime = playback_runtime(Path::new("/tmp"))?;
        validate_private_runtime(runtime.path())
            .context("Could not create a private playback directory in /tmp")?;
        runtime
    };
    #[cfg(not(target_os = "macos"))]
    let runtime = playback_runtime(&private_runtime()?)?;
    let socket_path = ipc_path(runtime.path())?;
    let plugin_dir = write_account_extractor(runtime.path())?;
    let mut account_supported = options
        .check_account_at_startup
        .then(|| account_playback_supported(ytdlp));
    let mut supports_account =
        || *account_supported.get_or_insert_with(|| account_playback_supported(ytdlp));
    let mut account_warned = false;
    let mut account_notice = None;
    let mut credentials = account_credentials(
        options.credentials.take(),
        &mut supports_account,
        account_warned,
        &mut account_notice,
    )
    .map(|source| ExtractorCredentials {
        plugin_dir: plugin_dir.clone(),
        source,
    });
    // Destroy the core before removing the private runtime.
    let core = MpvCore::new(&options, &socket_path)?;
    // Cancel/reap the resolver before credentials or the core can drop.
    let native = lookup.map(|lookup| NativeResolver::new(lookup, wake.clone()));
    let mut resolver = StreamResolver::new(ytdlp, credentials.as_ref(), native);
    if options
        .initial_video
        .is_some_and(VideoQuality::uses_full_resolver)
    {
        resolver.warm_up(credentials.as_ref());
    }
    let mut pending_play: Option<PendingPlay> = None;
    let mut next_target: Option<PreparedTarget> = None;
    let mut prepared: Option<(PreparedTarget, ResolvedStream)> = None;
    // The paused song and when its stream must be refreshed, once per pause.
    let mut paused_stream: Option<(u64, Option<SystemTime>)> = None;
    // Every actor exit frees the renderer before destroying the core.
    let mut video: Option<AttachedVideo> = None;
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut socket = loop {
        match IpcStream::connect(&socket_path) {
            Ok(socket) => break socket,
            Err(_) if Instant::now() < deadline => {
                ensure!(!core.is_shutdown(), "mpv exited during startup");
                thread::sleep(Duration::from_millis(25));
            }
            Err(error) => return Err(error).context("mpv did not open its control socket"),
        }
    };
    socket.set_write_timeout(Some(Duration::from_secs(2)))?;
    let mut state = PlaybackState {
        captions_enabled: options.captions,
        ..PlaybackState::default()
    };
    let mut pending = Vec::new();
    // UI and MPRIS own volume. Delayed mpv acknowledgments must not
    // replace the retained level with an earlier mute's zero.
    for (id, name) in [
        "time-pos",
        "duration",
        "pause",
        "paused-for-cache",
        "speed",
        "demuxer-cache-time",
    ]
    .iter()
    .enumerate()
    {
        write_command(&mut socket, json!(["observe_property", id + 1, name]), None)?;
    }
    // Position observers use 1 + 6n; reserve zero for seekability.
    write_command(
        &mut socket,
        json!(["observe_property", 0, "seekable"]),
        None,
    )?;
    let mut buffer = [0_u8; 16384];
    loop {
        loop {
            match commands.try_recv() {
                Ok(PlayerCommand::Prepare(target)) => {
                    if target.as_ref().is_some_and(|target| {
                        prepared.as_ref().is_some_and(|(current, stream)| {
                            current.track_id == target.track_id && stream.matches(&target.key)
                        })
                    }) {
                        continue;
                    }
                    next_target = target.map(|mut target| {
                        target.lane = target.key.lane(true, credentials.is_some());
                        target
                    });
                    prepared = None;
                    state.discard_upcoming();
                    write_command(&mut socket, json!(["playlist-clear"]), None)?;
                    // A queue render can prepare the following song while Play is
                    // resolving. Interactive playback keeps the single request slot.
                    if pending_play.is_none() {
                        if let Some(target) = next_target.clone() {
                            let _ = resolver.start(target, credentials.as_ref());
                        } else {
                            resolver.cancel();
                        }
                    }
                }
                Ok(PlayerCommand::Play {
                    track_id,
                    url,
                    title,
                    options,
                }) => {
                    let key = StreamKey {
                        url: url.clone(),
                        quality: options.quality,
                        video: options.video,
                    };
                    // A reload that skips the fast lookup must not get its stream back.
                    // Any other play of the prepared song, such as resuming the restored
                    // song partway in, loads the prepared stream under its own ID.
                    let resolved = prepared.take().filter(|(target, stream)| {
                        stream.matches(&key)
                            && (target.track_id == track_id || !options.full_lookup)
                    });
                    // mpv moves to an appended entry only from a file it is playing.
                    // An idle player, as at startup, loads the prepared stream instead.
                    let prefetched = resolved
                        .as_ref()
                        .is_some_and(|(target, _)| target.track_id == track_id)
                        && state.file_active
                        && state.entries.values().any(|entry| *entry == track_id);
                    next_target = None;
                    pending_play = None;
                    let already_restarted = state.restarted && state.current == Some(track_id);
                    state.invalidate_seeks();
                    state.restarted = already_restarted;
                    state.desired = Some(track_id);
                    state.paused = options.paused;
                    state.pause_target = Some(options.paused);
                    // Adoption must refresh metadata even if file-loaded preceded Play.
                    state.pause_after_load = true;
                    state.upcoming = None;
                    if prefetched {
                        resolver.cancel();
                        if state.current != Some(track_id) {
                            state.loaded = false;
                            state.pause_after_load = true;
                            // mpv may have reached this last prepared entry before
                            // its start-file event reaches us. Weak leaves it playing.
                            write_command(&mut socket, json!(["playlist-next", "weak"]), None)?;
                        } else if state.loaded {
                            // EOF may have transitioned before the UI consumed Ended.
                            let _ = events.send_blocking(PlayerEvent::Started {
                                track_id,
                                paused: options.paused,
                                captions: state.captions.contains_key(&track_id),
                            });
                        }
                        state.pause_revision += 1;
                        write_pause(&mut socket, options.paused, state.pause_revision)?;
                    } else {
                        state.loaded = false;
                        #[cfg(test)]
                        if Path::new(&url).is_absolute() {
                            resolver.cancel();
                            write_command(
                                &mut socket,
                                load_command(&url, &title, 0., options),
                                Some(json!(track_id)),
                            )?;
                            continue;
                        }
                        write_command(&mut socket, json!(["stop"]), None)?;
                        // The append acknowledgement can still be queued when the
                        // user clicks Next. Reuse its resolved URL in that case.
                        if let Some((_, stream)) = resolved {
                            resolver.cancel();
                            state.load_captions(track_id, &stream);
                            write_command(
                                &mut socket,
                                load_command(&stream.url, &title, stream.gain_db, options),
                                Some(json!(track_id)),
                            )?;
                            continue;
                        }
                        let target = PreparedTarget {
                            lane: if options.full_lookup {
                                ResolverLane::Full
                            } else {
                                key.lane(false, credentials.is_some())
                            },
                            key,
                            track_id,
                        };
                        pending_play = Some(PendingPlay {
                            target: target.clone(),
                            title,
                            options,
                        });
                        if resolver.start(target, credentials.as_ref()).is_err() {
                            pending_play = None;
                            stream_error(track_id, events);
                        }
                    }
                }
                Ok(PlayerCommand::Pause(paused)) => {
                    state.pause_revision += 1;
                    state.pause_target = Some(paused);
                    state.paused = paused;
                    state.pause_after_load = !state.loaded;
                    write_pause(&mut socket, paused, state.pause_revision)?;
                }
                Ok(PlayerCommand::Captions(enabled)) => {
                    state.captions_enabled = enabled;
                    state.apply_captions(&mut socket)?;
                }
                Ok(PlayerCommand::Seek(request)) => state.queue_seek(request, events),
                Ok(PlayerCommand::Json(command)) => write_command(&mut socket, command, None)?,
                Ok(PlayerCommand::Credentials { next, reply }) => {
                    // Reap even an idle resolver: it holds the old account in memory.
                    resolver.stop();
                    pending_play = None;
                    next_target = None;
                    prepared = None;
                    state.upcoming = None;
                    let next = account_credentials(
                        next,
                        &mut supports_account,
                        account_warned,
                        &mut account_notice,
                    );
                    state.invalidate_seeks();
                    state.desired = None;
                    state.loaded = false;
                    let result = replace_playback_credentials(
                        &mut socket,
                        &mut pending,
                        &mut credentials,
                        next,
                        &plugin_dir,
                        &mut state,
                        events,
                    );
                    if result.is_ok() {
                        let native = resolver.native.take();
                        if let Some(native) = &native {
                            native.lookup.reset();
                        }
                        resolver = StreamResolver::new(ytdlp, credentials.as_ref(), native);
                    }
                    let _ = reply.send_blocking(
                        result
                            .as_ref()
                            .map(|_| ())
                            .map_err(|error| format!("{error:#}")),
                    );
                    result?;
                }
                Ok(PlayerCommand::AttachVideo { video: next }) => {
                    let result: Result<()> = (|| {
                        ensure!(video.is_none(), "A video surface is already attached");
                        next.start(core.0.as_ptr(), wake)?;
                        let attached = AttachedVideo {
                            video: next,
                            ready: false,
                        };
                        video = Some(attached);
                        Ok(())
                    })();
                    if let Err(error) = result {
                        let _ = events.send_blocking(PlayerEvent::VideoError {
                            message: format!("{error:#}"),
                        });
                    }
                    let _ = events.send_blocking(PlayerEvent::VideoChanged);
                }
                Ok(PlayerCommand::DetachVideo {
                    video: current,
                    reply,
                }) => {
                    if video
                        .as_ref()
                        .is_some_and(|video| Arc::ptr_eq(&video.video, &current))
                    {
                        let result = request(
                            &mut socket,
                            &mut pending,
                            json!(["set_property", "vid", "no"]),
                            |value| state.message(value, events),
                        );
                        video.take();
                        if result.is_err() {
                            let _ = events.send_blocking(PlayerEvent::VideoError {
                                message: "Could not disable the video track".into(),
                            });
                        }
                    }
                    let _ = reply.send_blocking(());
                }
                Ok(PlayerCommand::Stop) => {
                    resolver.cancel();
                    pending_play = None;
                    next_target = None;
                    prepared = None;
                    state.upcoming = None;
                    state.invalidate_seeks();
                    state.desired = None;
                    state.loaded = false;
                    write_command(&mut socket, json!(["stop"]), None)?;
                }
                Ok(PlayerCommand::Quit) | Err(mpsc::TryRecvError::Disconnected) => {
                    resolver.stop();
                    let _ = write_command(&mut socket, json!(["quit"]), None);
                    return Ok(());
                }
                Err(mpsc::TryRecvError::Empty) => break,
            }
        }
        resolver.retain(
            pending_play.as_ref().map(|play| &play.target),
            next_target.as_ref(),
        );
        if let Some((target, result)) = resolver.finish(
            pending_play.as_ref().map(|play| &play.target),
            credentials.as_ref(),
        ) {
            if let Some(play) = pending_play.take_if(|play| {
                play.target.track_id == target.track_id && play.target.key == target.key
            }) {
                match result {
                    Ok(stream) => {
                        let options = PlayOptions {
                            paused: state.paused,
                            ..play.options
                        };
                        state.load_captions(target.track_id, &stream);
                        write_command(
                            &mut socket,
                            load_command(&stream.url, &play.title, stream.gain_db, options),
                            Some(json!(target.track_id)),
                        )?;
                        if let Some(target) = next_target.clone()
                            && !prepared.as_ref().is_some_and(|(current, stream)| {
                                current.track_id == target.track_id && stream.matches(&target.key)
                            })
                        {
                            let _ = resolver.start(target, credentials.as_ref());
                        }
                    }
                    Err(_) => {
                        next_target = None;
                        stream_error(target.track_id, events);
                    }
                }
            } else if next_target
                .as_ref()
                .is_some_and(|next| next.track_id == target.track_id && next.key == target.key)
                && let Ok(stream) = result
            {
                prepared = Some((target, stream));
            }
        }
        // A full preparation can finish while a native click is resolving. Hold
        // it until that click loads, so append cannot replace its pending load.
        if pending_play.is_none()
            && let Some((target, stream)) = &prepared
            && state.upcoming != Some(target.track_id)
            // After a natural transition the prepared song is already playing.
            && state.current != Some(target.track_id)
        {
            let mut command = load_command(
                &stream.url,
                "",
                stream.gain_db,
                PlayOptions {
                    quality: target.key.quality,
                    video: target.key.video,
                    ..Default::default()
                },
            );
            command["flags"] = json!("append");
            state.upcoming = Some(target.track_id);
            state.load_captions(target.track_id, stream);
            write_command(
                &mut socket,
                command,
                Some(json!(format!("prepare:{}", target.track_id))),
            )?;
            #[cfg(test)]
            let _ = events.send_blocking(PlayerEvent::Prepared {
                video_id: stream.key.url.rsplit_once("v=").unwrap().1.to_owned(),
            });
        }
        if let Some((target, _)) = prepared.take_if(|(target, stream)| {
            state.current != Some(target.track_id) && SystemTime::now() >= stream.expires
        }) {
            state.discard_upcoming();
            write_command(&mut socket, json!(["playlist-clear"]), None)?;
            let _ = resolver.start(target, credentials.as_ref());
        }
        if let Some(attached) = &mut video {
            let result = attached.video.ready_for_player().and_then(|ready| {
                if ready && !attached.ready {
                    // A native lookup can load video before EGL/libmpv finishes
                    // starting. Enabling it sooner leaves mpv with no renderer.
                    request(
                        &mut socket,
                        &mut pending,
                        json!(["set_property", "vid", "auto"]),
                        |value| state.message(value, events),
                    )
                    .map_err(|_| anyhow::anyhow!("Could not enable the video track"))?;
                    attached.ready = true;
                    let _ = events.send_blocking(PlayerEvent::VideoChanged);
                }
                Ok(())
            });
            if let Err(error) = result {
                let _ = request(
                    &mut socket,
                    &mut pending,
                    json!(["set_property", "vid", "no"]),
                    |value| state.message(value, events),
                );
                video.take();
                let _ = events.send_blocking(PlayerEvent::VideoError {
                    message: error.to_string(),
                });
                let _ = events.send_blocking(PlayerEvent::VideoChanged);
            }
        }
        if state.loaded
            && state.current == state.desired
            && let Some(message) = account_notice.take()
        {
            // Activation and stream replacement clear transient UI notices.
            // Deliver this after Started, once guest playback is actually ready.
            account_warned = true;
            let _ = events.send_blocking(PlayerEvent::Notice(message));
        }
        let _ = &credentials;
        state.write_seek(&mut socket)?;
        ensure!(!core.is_shutdown(), "mpv stopped unexpectedly");
        // mpv reconnects a dropped stream with the same URL, which fails once
        // the URL expires, and the song then ends where its buffer runs out.
        // A paused song can outlive its URL, so before that ask the app to
        // reload it, still paused, at the same position.
        let paused_track = state.current.filter(|id| {
            state.paused && state.loaded && state.desired == Some(*id) && pending_play.is_none()
        });
        if paused_track != paused_stream.map(|(id, _)| id) {
            paused_stream = paused_track.map(|id| {
                let path = request(
                    &mut socket,
                    &mut pending,
                    json!(["get_property", "path"]),
                    |value| state.message(value, events),
                );
                let refresh = path
                    .ok()
                    .as_ref()
                    .and_then(Value::as_str)
                    .and_then(stream_refresh_time);
                (id, refresh)
            });
        }
        if let Some((track_id, Some(refresh))) = paused_stream
            && SystemTime::now() >= refresh
        {
            paused_stream = Some((track_id, None));
            let buffered_to_end = request(
                &mut socket,
                &mut pending,
                json!(["get_property", "demuxer-cache-state"]),
                |value| state.message(value, events),
            )
            .is_ok_and(|cache| cache["eof"] == true);
            if !buffered_to_end {
                let _ = events.send_blocking(PlayerEvent::StreamExpiring { track_id });
            }
        }
        // Expiry is wall-clock time, which a monotonic wait doesn't follow across
        // clock changes or suspend, so recheck it at least once a minute.
        let expiry = prepared
            .as_ref()
            .filter(|(target, _)| state.current != Some(target.track_id))
            .map(|(_, stream)| stream.expires)
            .into_iter()
            .chain(paused_stream.and_then(|(_, refresh)| refresh))
            .min()
            .map(|expires| {
                Instant::now()
                    + expires
                        .duration_since(SystemTime::now())
                        .unwrap_or_default()
                        .min(Duration::from_secs(60))
            });
        let deadline = resolver.deadline().into_iter().chain(expiry).min();
        if pending.contains(&b'\n')
            || wait_for_player_input(&socket, wake, resolver.fd(), deadline)?
        {
            state.read_messages(&mut socket, &mut pending, &mut buffer, events)?;
        }
    }
}

fn stream_error(track_id: u64, events: &async_channel::Sender<PlayerEvent>) {
    let _ = events.send_blocking(PlayerEvent::Error {
        track_id: Some(track_id),
        message: "YouTube could not provide this stream. Try another track or update yt-dlp."
            .into(),
    });
}

#[cfg(test)]
mod tests {
    use super::mpv::tests::socket_message;
    #[cfg(unix)]
    use super::stream::tests::resolver_target;
    #[cfg(target_os = "linux")]
    use super::stream::tests::synthetic_full_extractor;
    use super::ytdlp::tests::{signed_in, synthetic_credentials};
    #[cfg(unix)]
    use super::ytdlp::{REVIEWED_YTDLP, tests::alive};
    use super::*;
    use std::io::{Read, Write};
    #[cfg(unix)]
    use std::os::fd::{AsFd, AsRawFd};
    use std::{
        fs::File,
        process::{Command, Stdio},
    };

    #[cfg(unix)]
    #[test]
    fn playback_runtime_stays_private_under_the_desktop_runtime_and_cleans_up() {
        use std::os::unix::fs::MetadataExt;
        let parent = tempfile::Builder::new()
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let runtime = playback_runtime(parent.path()).unwrap();
        let path = runtime.path().to_owned();
        assert_eq!(path.parent(), Some(parent.path()));
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o700);
        write_account_extractor(&path).unwrap();
        drop(runtime);
        assert!(!path.exists());
    }

    #[test]
    fn resolver_output_wakes_the_actor_without_mpv_or_commands() {
        let (socket, _mpv) = IpcStream::pair().unwrap();
        let (resolver, mut output) = IpcStream::pair().unwrap();
        let (commands, _received) = command_channel().unwrap();
        output.write_all(b"{\"ready\":true}\n").unwrap();
        let start = Instant::now();
        assert!(
            !wait_for_player_input(&socket, &commands.wake, Some(resolver.as_waitable()), None)
                .unwrap()
        );
        assert!(
            start.elapsed() < Duration::from_millis(60),
            "resolver output waited for the actor's timer"
        );
    }

    #[cfg(unix)]
    #[test]
    fn one_shot_exit_wakes_the_actor_without_mpv_or_commands() {
        let directory = tempfile::tempdir().unwrap();
        let extractor = directory.path().join("extractor");
        fs::write(
            &extractor,
            "#!/bin/sh\nsleep 0.15\nprintf '%s\\n' 'https://rr1.googlevideo.com/audio'\n",
        )
        .unwrap();
        fs::set_permissions(&extractor, fs::Permissions::from_mode(0o700)).unwrap();
        let mut resolver = StreamResolver::new(&extractor, None, None);
        resolver
            .start(resolver_target("abcdefghijk", 1), None)
            .unwrap();
        let (socket, _mpv) = IpcStream::pair().unwrap();
        let (commands, _received) = command_channel().unwrap();
        let start = Instant::now();
        assert!(
            !wait_for_player_input(
                &socket,
                &commands.wake,
                resolver.fd(),
                Some(start + Duration::from_secs(2))
            )
            .unwrap()
        );
        let (_, stream) = resolver
            .finish(None, None)
            .expect("child exit must wake the actor");
        assert!(stream.is_ok());
        assert!(start.elapsed() < Duration::from_secs(1));
        assert!(resolver.fd().is_none() && resolver.deadline().is_none());
    }

    #[test]
    fn prepared_occurrences_keep_their_reserved_identity_only_for_matching_plays() {
        let (commands, received) = command_channel().unwrap();
        let (_, events) = async_channel::unbounded();
        let player = Player {
            commands,
            events,
            next_id: AtomicU64::new(1),
            next: Mutex::new(None),
            lookup: None,
            worker: None,
        };
        player
            .prepare_next(Some("abcdefghijk"), PlayOptions::default())
            .unwrap();
        let PlayerCommand::Prepare(Some(target)) = received.try_recv().unwrap() else {
            panic!("missing preparation")
        };
        assert_eq!(
            player
                .play("abcdefghijk", "Next", PlayOptions::default())
                .unwrap(),
            target.track_id
        );
        assert_eq!(
            player
                .play("abcdefghijk", "Replay", PlayOptions::default())
                .unwrap(),
            target.track_id + 1
        );
        received.try_recv().unwrap();
        received.try_recv().unwrap();
        player
            .prepare_next(Some("abcdefghijk"), PlayOptions::default())
            .unwrap();
        let PlayerCommand::Prepare(Some(target)) = received.try_recv().unwrap() else {
            panic!("missing preparation")
        };
        assert_ne!(
            player
                .play(
                    "abcdefghijk",
                    "New quality",
                    PlayOptions {
                        quality: AudioQuality::High,
                        ..Default::default()
                    }
                )
                .unwrap(),
            target.track_id
        );
        // Repeat One prepares the song that just failed to open. The app's reload
        // must not inherit that occurrence's ID, or its late stop would end it.
        received.try_recv().unwrap();
        player
            .prepare_next(Some("abcdefghijk"), PlayOptions::default())
            .unwrap();
        let PlayerCommand::Prepare(Some(target)) = received.try_recv().unwrap() else {
            panic!("missing preparation")
        };
        let reload = player
            .play(
                "abcdefghijk",
                "Reload",
                PlayOptions {
                    full_lookup: true,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_ne!(reload, target.track_id);
    }

    #[test]
    #[ignore = "requires libmpv, yt-dlp and network; uses null audio and no account"]
    fn live_prepared_next_latency() {
        fn event(player: &Player, deadline: Instant) -> PlayerEvent {
            loop {
                if let Some(event) = player.try_recv() {
                    return event;
                }
                assert!(Instant::now() < deadline, "player stopped producing events");
                thread::sleep(Duration::from_millis(5));
            }
        }
        fn wait_for_audio(player: &Player, id: u64, start: Instant) -> Duration {
            while start.elapsed() < Duration::from_secs(55) {
                match event(player, start + Duration::from_secs(55)) {
                    PlayerEvent::Position { track_id, seconds }
                        if track_id == id && seconds > 0. =>
                    {
                        return start.elapsed();
                    }
                    PlayerEvent::Error { track_id, message } => {
                        panic!("track {track_id:?}: {message}")
                    }
                    _ => {}
                }
            }
            panic!("audio did not start")
        }
        let player = Player::new(PlayerOptions {
            volume: 0.,
            audio_output: Some("null".into()),
            ..Default::default()
        })
        .unwrap();
        let start = Instant::now();
        let initial = player
            .play("csILUDfmtqM", "Initial", PlayOptions::default())
            .unwrap();
        wait_for_audio(&player, initial, start);
        let start = Instant::now();
        let id = player
            .play("csILUDfmtqM", "Baseline", PlayOptions::default())
            .unwrap();
        let cold = wait_for_audio(&player, id, start);
        player
            .prepare_next(Some("csILUDfmtqM"), PlayOptions::default())
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(55);
        loop {
            assert!(Instant::now() < deadline, "next stream did not prepare");
            match event(&player, deadline) {
                PlayerEvent::Prepared { video_id } if video_id == "csILUDfmtqM" => break,
                PlayerEvent::Error { track_id, message } => panic!("track {track_id:?}: {message}"),
                _ => {}
            }
        }
        thread::sleep(Duration::from_secs(1));
        while player.events.try_recv().is_ok() {}
        let start = Instant::now();
        let id = player
            .play("csILUDfmtqM", "Prepared", PlayOptions::default())
            .unwrap();
        // Music prepares the following entry when the current selection changes.
        player
            .prepare_next(Some("dQw4w9WgXcQ"), PlayOptions::default())
            .unwrap();
        let warm = wait_for_audio(&player, id, start);
        println!(
            "cold_next_ms={:.1} prepared_next_ms={:.1}",
            cold.as_secs_f64() * 1000.,
            warm.as_secs_f64() * 1000.
        );
        player
            .prepare_next(Some("csILUDfmtqM"), PlayOptions::default())
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(55);
        loop {
            if matches!(event(&player, deadline), PlayerEvent::Prepared { video_id } if video_id == "csILUDfmtqM")
            {
                break;
            }
        }
        // Exercise the real EOF handoff quickly, after the queued URL can prebuffer.
        thread::sleep(Duration::from_secs(1));
        player.seek(id, 225.).unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut ended = None;
        let mut next_id = None;
        let mut started = false;
        let mut seekable = false;
        let mut positioned = false;
        loop {
            assert!(
                Instant::now() < deadline,
                "natural transition did not finish"
            );
            match event(&player, deadline) {
                PlayerEvent::Ended { track_id } if track_id == id => {
                    ended = Some(Instant::now());
                    next_id = Some(
                        player
                            .play("csILUDfmtqM", "Natural", PlayOptions::default())
                            .unwrap(),
                    );
                    player
                        .prepare_next(Some("dQw4w9WgXcQ"), PlayOptions::default())
                        .unwrap();
                }
                PlayerEvent::Started { track_id, .. } if Some(track_id) == next_id => {
                    println!(
                        "natural_eof_to_started_ms={:.3}",
                        ended.unwrap().elapsed().as_secs_f64() * 1000.
                    );
                    started = true;
                }
                PlayerEvent::Seekable {
                    track_id,
                    seekable: true,
                } if Some(track_id) == next_id => seekable = true,
                PlayerEvent::Position { track_id, seconds }
                    if Some(track_id) == next_id && seconds > 0. =>
                {
                    positioned = true
                }
                PlayerEvent::Error { track_id, message } => panic!("track {track_id:?}: {message}"),
                _ => {}
            }
            if started && seekable && positioned {
                break;
            }
        }
    }

    #[test]
    fn idle_wait_blocks_until_a_command_and_deadline_wait_expires() {
        let (socket, _mpv) = IpcStream::pair().unwrap();
        let (commands, _received) = command_channel().unwrap();
        thread::scope(|scope| {
            let (completed, result) = mpsc::channel();
            let wake = &commands.wake;
            let socket = &socket;
            scope.spawn(move || {
                completed
                    .send(wait_for_player_input(socket, wake, None, None))
                    .unwrap();
            });
            let premature = result.recv_timeout(Duration::from_millis(200));
            commands.send(PlayerCommand::Stop).unwrap();
            assert!(premature.is_err(), "an idle actor must not wake on a timer");
            assert!(
                !result
                    .recv_timeout(Duration::from_secs(1))
                    .unwrap()
                    .unwrap()
            );
        });
        let start = Instant::now();
        assert!(
            !wait_for_player_input(
                &socket,
                &commands.wake,
                None,
                Some(start + Duration::from_millis(25))
            )
            .unwrap()
        );
        assert!(start.elapsed() >= Duration::from_millis(25));
        assert!(start.elapsed() < Duration::from_millis(500));
    }

    #[cfg(unix)]
    #[test]
    fn command_wakeups_coalesce_without_losing_commands_or_socket_readiness() {
        let (commands, received) = command_channel().unwrap();
        let (mut mpv, socket) = IpcStream::pair().unwrap();
        commands.send(PlayerCommand::Stop).unwrap();
        commands.clone().send(PlayerCommand::Pause(true)).unwrap();
        assert!(!wait_for_player_input(&socket, &commands.wake, None, None).unwrap());
        assert!(matches!(received.try_recv(), Ok(PlayerCommand::Stop)));
        assert!(matches!(
            received.try_recv(),
            Ok(PlayerCommand::Pause(true))
        ));
        let mut wake = libc::pollfd {
            fd: commands.wake.as_fd().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        assert_eq!(unsafe { libc::poll(&mut wake, 1, 0) }, 0);
        assert_ne!(
            unsafe { libc::fcntl(wake.fd, libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );

        mpv.write_all(b"mpv reply\n").unwrap();
        commands.send(PlayerCommand::Quit).unwrap();
        assert!(wait_for_player_input(&socket, &commands.wake, None, None).unwrap());
        let mut reply = [0; 10];
        (&socket).read_exact(&mut reply).unwrap();
        assert_eq!(&reply, b"mpv reply\n");
        assert!(matches!(received.try_recv(), Ok(PlayerCommand::Quit)));
        drop(received);
        assert!(commands.send(PlayerCommand::Stop).is_err());
        assert_eq!(unsafe { libc::poll(&mut wake, 1, 0) }, 0);
    }

    #[test]
    fn seek_has_a_validated_track_bound_revision() {
        let (commands, received) = command_channel().unwrap();
        let (_, events) = async_channel::unbounded();
        let player = Player {
            commands,
            events,
            next_id: AtomicU64::new(2),
            next: Mutex::new(None),
            lookup: None,
            worker: None,
        };
        for seconds in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(player.seek(1, seconds).is_err());
        }
        assert!(received.try_recv().is_err());
        let revision = player.seek(1, -10.).unwrap();
        assert_eq!(revision, 2);
        assert!(matches!(
            received.try_recv(),
            Ok(PlayerCommand::Seek(SeekRequest {
                track_id: 1,
                revision: 2,
                seconds: 0.
            }))
        ));
    }

    #[test]
    #[ignore = "requires libmpv and yt-dlp; no network or display"]
    fn headless_worker_shuts_down_after_ipc_controls() {
        let (commands, receiver) = command_channel().unwrap();
        let wake = commands.wake.clone();
        let (events, received) = async_channel::unbounded();
        let (finished, completion) = mpsc::channel();
        let worker = thread::spawn(move || {
            let result = run(
                PlayerOptions::default(),
                receiver,
                &wake,
                &events,
                &find_ytdlp().unwrap(),
                None,
            );
            finished.send(result).unwrap();
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut ready = false;
        while Instant::now() < deadline && !ready {
            match received.try_recv() {
                Ok(PlayerEvent::Speed(value)) if value == PlaybackSpeed::default() => ready = true,
                _ => thread::sleep(Duration::from_millis(10)),
            }
        }
        commands.send(PlayerCommand::Quit).unwrap();
        completion
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        worker.join().unwrap();
        assert!(ready, "headless IPC must initialize before clean shutdown");
    }

    #[test]
    fn every_play_has_its_own_validated_options_and_track_id() {
        let (commands, received) = command_channel().unwrap();
        let (_, events) = async_channel::unbounded();
        let player = Player {
            commands,
            events,
            next_id: AtomicU64::new(1),
            next: Mutex::new(None),
            lookup: None,
            worker: None,
        };
        for start in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.] {
            assert!(
                player
                    .play(
                        "csILUDfmtqM",
                        "Track",
                        PlayOptions {
                            start,
                            ..Default::default()
                        }
                    )
                    .is_err()
            );
        }
        assert!(received.try_recv().is_err());
        let options = PlayOptions {
            video: None,
            quality: AudioQuality::Low,
            start: 32.5,
            paused: true,
            full_lookup: false,
        };
        let first = player.play("csILUDfmtqM", "Track", options).unwrap();
        let second = player.play("csILUDfmtqM", "Track", options).unwrap();
        assert_ne!(first, second);
        let PlayerCommand::Play {
            track_id,
            url,
            title,
            options,
        } = received.try_recv().unwrap()
        else {
            panic!("play must enqueue a single load with its options");
        };
        assert_eq!(track_id, first);
        let command = load_command(&url, &title, -2.29, options);
        assert_eq!(command["name"], "loadfile");
        assert_eq!(
            command["url"],
            "https://music.youtube.com/watch?v=csILUDfmtqM"
        );
        assert_eq!(command["flags"], "replace");
        assert_eq!(command["options"]["start"], "32.5");
        assert_eq!(command["options"]["pause"], "yes");
        assert_eq!(command["options"]["ytdl"], "no");
        assert_eq!(command["options"]["volume-gain"], "-2.29");
        let defaults = load_command("unused", "Track", 0., PlayOptions::default());
        assert_eq!(defaults["options"]["start"], "0");
        assert_eq!(defaults["options"]["pause"], "no");
        assert_eq!(defaults["options"]["ytdl"], "no");
        let video = load_command(
            "unused",
            "Track",
            0.,
            PlayOptions {
                video: Some(VideoQuality::P720),
                ..options
            },
        );
        assert_eq!(video["options"]["ytdl"], "no");
        assert_eq!(video["options"]["start"], "32.5");
        assert_eq!(video["options"]["pause"], "yes");
    }

    #[test]
    #[ignore = "requires Python with yt-dlp; set YT_DLP_TEST_PYTHON if needed; no network"]
    fn quality_selectors_respect_audio_classes_and_video_caps() {
        let python = std::env::var_os("YT_DLP_TEST_PYTHON").unwrap_or_else(|| "python3".into());
        let selectors = AudioQuality::ALL.map(AudioQuality::selector);
        let output = Command::new(python)
            .args(["-B", "-c", r#"
import json, os, sys
if zipapp := os.environ.get('YT_DLP_TEST_ZIPAPP'):
    sys.path.insert(0, zipapp)
from yt_dlp import YoutubeDL

selectors = json.loads(sys.argv[1])
def select(selector, formats):
    with YoutubeDL({'quiet': True, 'no_warnings': True}) as ydl:
        formats = [dict(url='https://example.invalid/audio', ext='webm', acodec='opus', vcodec='none', **f) for f in formats]
        result = list(ydl.build_format_selector(selector)({'formats': formats, 'has_merged_format': False, 'incomplete_formats': False}))
        return result[0]['format_id']

# Guest service metadata observed on csILUDfmtqM, plus a synthetic high format.
formats = [
    dict(format_id='249', format_note='low', quality=2, abr=50.193),
    dict(format_id='250', format_note='low', quality=2, abr=66.012),
    dict(format_id='251', format_note='medium', quality=3, abr=131.297),
    dict(format_id='high', format_note='high', quality=4, abr=256),
]
assert [select(s, formats) for s in selectors] == ['250', '251', 'high']
assert select(selectors[0], [dict(format_id='fallback', abr=48), dict(format_id='larger', abr=192)]) == 'fallback'
assert select(selectors[1], [dict(format_id='fallback', abr=96), dict(format_id='larger', abr=192)]) == 'fallback'
assert all(select(s, [dict(format_id='only')]) == 'only' for s in selectors)

video_selectors = json.loads(sys.argv[2])
cap_selectors = json.loads(sys.argv[3])
def media(kind, id, **values):
    base = dict(url='https://example.invalid/media', ext='mp4', format_id=id)
    base.update(acodec='mp4a' if kind != 'video' else 'none',
                vcodec='h264' if kind != 'audio' else 'none')
    return dict(base, **values)
def choose(selector, formats):
    with YoutubeDL({'quiet': True, 'no_warnings': True}) as ydl:
        result = list(ydl.build_format_selector(selector)({'formats': formats,
            'has_merged_format': False, 'incomplete_formats': False}))
        return result[0]['format_id'] if result else None

heights = [144, 240, 360, 480, 720, 1080, 1440, 2160]
videos = [media('video', 'v'+str(h), height=h) for h in heights]
audio = [media('audio', f.pop('format_id'), **f) for f in formats]
assert [choose(s, audio + videos) for s in video_selectors] == ['v720+250', 'v720+251', 'v720+high']
assert [choose(s, audio + videos) for s in cap_selectors] == ['v'+str(h)+'+high' for h in heights]
assert choose(video_selectors[0], [media('audio', 'fallback', abr=48), media('audio', 'larger', abr=192)] + videos) == 'v720+fallback'
assert choose(video_selectors[1], [media('audio', 'fallback', abr=96), media('audio', 'larger', abr=192)] + videos) == 'v720+fallback'
progressive = [media('both', 'p720', height=720), media('both', 'p1080', height=1080)]
assert all(choose(s, progressive) == 'p720' for s in video_selectors)
assert all(choose(s, [videos[4], progressive[1]]) is None for s in video_selectors)
assert all(choose(s, audio) is None for s in video_selectors)
assert all(choose(s, [progressive[1]]) is None for s in video_selectors)
"#])
            .arg(serde_json::to_string(&selectors).unwrap())
            .arg(serde_json::to_string(&AudioQuality::ALL.map(|audio| VideoQuality::P720.selector(audio))).unwrap())
            .arg(serde_json::to_string(&VideoQuality::ALL.map(|video| video.selector(AudioQuality::High))).unwrap())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        native::tests::compare_ytdlp(&native::tests::fixture());
    }

    fn credential_barrier_command(server: &mut IpcStream) -> Value {
        let name = socket_message(server);
        assert_eq!(name["command"], json!(["client_name"]));
        writeln!(
            server,
            "{}",
            json!({
                "request_id":name["request_id"], "error":"success", "data":"test-ipc",
            })
        )
        .unwrap();
        let marker = socket_message(server);
        assert_eq!(marker["command"][0], "script-message-to");
        assert_eq!(marker["command"][1], "test-ipc");
        marker
    }

    fn reply_to_credential_barrier(server: &mut IpcStream) {
        let marker = credential_barrier_command(server);
        // Also cover the event arriving before its command reply.
        writeln!(
            server,
            "{}",
            json!({
                "event":"client-message", "args":[marker["command"][2]],
            })
        )
        .unwrap();
        writeln!(
            server,
            "{}",
            json!({
                "request_id":marker["request_id"], "error":"success",
            })
        )
        .unwrap();
    }

    #[test]
    fn credential_changes_wait_for_old_file_events() {
        let (mut client, mut server) = IpcStream::pair().unwrap();
        client
            .set_read_timeout(Some(Duration::from_millis(80)))
            .unwrap();
        server
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let plugin_dir = Path::new("/private/synthetic-plugin-root");
        let mut current = Some(signed_in(
            synthetic_credentials("INVENTED_FIRST"),
            plugin_dir,
        ));
        let worker = thread::spawn(move || {
            for start_after_marker_reply in [true, false] {
                let stop = socket_message(&mut server);
                assert_eq!(stop["command"], json!(["stop"]));
                writeln!(
                    server,
                    "{}",
                    json!({"request_id":stop["request_id"], "error":"success"})
                )
                .unwrap();
                if start_after_marker_reply {
                    let marker = credential_barrier_command(&mut server);
                    writeln!(
                        server,
                        "{}",
                        json!({"request_id":marker["request_id"], "error":"success"})
                    )
                    .unwrap();
                    writeln!(server, "{}", json!({"event":"start-file"})).unwrap();
                    writeln!(
                        server,
                        "{}",
                        json!({"event":"client-message", "args":[marker["command"][2]]})
                    )
                    .unwrap();
                } else {
                    writeln!(server, "{}", json!({"event":"start-file"})).unwrap();
                    reply_to_credential_barrier(&mut server);
                }
                server
                    .set_read_timeout(Some(Duration::from_millis(25)))
                    .unwrap();
                assert!(server.read(&mut [0]).is_err());
                server
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                writeln!(server, "{}", json!({"event":"end-file"})).unwrap();
            }
            server
        });
        let mut pending = Vec::new();
        let mut state = PlaybackState::default();
        let (events, _) = async_channel::unbounded();
        replace_playback_credentials(
            &mut client,
            &mut pending,
            &mut current,
            Some(synthetic_credentials("INVENTED_SECOND")),
            plugin_dir,
            &mut state,
            &events,
        )
        .unwrap();
        let installed = &current.as_ref().unwrap().source.message;
        assert!(String::from_utf8_lossy(installed).contains("INVENTED_SECOND"));
        replace_playback_credentials(
            &mut client,
            &mut pending,
            &mut current,
            None,
            plugin_dir,
            &mut state,
            &events,
        )
        .unwrap();
        assert!(current.is_none());
        worker.join().unwrap();
    }

    #[test]
    fn credential_clear_handles_idle_or_already_ended_files() {
        for queued in [
            vec![],
            vec![json!({"event":"start-file"}), json!({"event":"end-file"})],
        ] {
            let (mut client, mut server) = IpcStream::pair().unwrap();
            client
                .set_read_timeout(Some(Duration::from_millis(80)))
                .unwrap();
            let worker = thread::spawn(move || {
                let stop = socket_message(&mut server);
                writeln!(
                    server,
                    "{}",
                    json!({"request_id":stop["request_id"], "error":"success"})
                )
                .unwrap();
                for message in queued {
                    writeln!(server, "{message}").unwrap();
                }
                reply_to_credential_barrier(&mut server);
                server
            });
            let (events, _) = async_channel::unbounded();
            replace_playback_credentials(
                &mut client,
                &mut Vec::new(),
                &mut None,
                None,
                Path::new("/private/plugins"),
                &mut PlaybackState::default(),
                &events,
            )
            .unwrap();
            worker.join().unwrap();
        }
    }

    #[test]
    fn credential_responses_close_if_the_worker_cannot_handle_them() {
        let (commands, received) = command_channel().unwrap();
        let (_, events) = async_channel::unbounded();
        let player = Player {
            commands,
            events,
            next_id: AtomicU64::new(1),
            next: Mutex::new(None),
            lookup: None,
            worker: None,
        };
        let cleared = player.clear_credentials().unwrap();
        let installed = player
            .set_credentials(synthetic_credentials("INVENTED_UNHANDLED"))
            .unwrap();
        assert!(cleared.is_empty() && installed.is_empty());
        drop(received);
        assert!(cleared.recv_blocking().is_err());
        assert!(installed.recv_blocking().is_err());
        assert!(player.clear_credentials().is_err());
        assert!(
            player
                .set_credentials(synthetic_credentials("INVENTED_UNHANDLED"))
                .is_err()
        );
    }

    #[test]
    fn failed_credential_cleanup_does_not_report_success() {
        for failure in ["stop", "marker", "end-file"] {
            let (mut client, mut server) = IpcStream::pair().unwrap();
            client
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            server
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            let plugin_dir = Path::new("/private/synthetic-plugin-root");
            let mut current = Some(signed_in(
                synthetic_credentials("INVENTED_FIRST"),
                plugin_dir,
            ));
            let worker = thread::spawn(move || {
                let stop = socket_message(&mut server);
                assert_eq!(stop["command"], json!(["stop"]));
                if failure == "end-file" {
                    writeln!(server, "{}", json!({"event":"start-file"})).unwrap();
                }
                writeln!(server, "{}", json!({
                    "request_id":stop["request_id"],
                    "error":if failure == "stop" { "INVENTED_PRIVATE_DIAGNOSTIC" } else { "success" },
                })).unwrap();
                if failure == "marker" {
                    let marker = credential_barrier_command(&mut server);
                    writeln!(
                        server,
                        "{}",
                        json!({
                            "request_id":marker["request_id"], "error":"success",
                        })
                    )
                    .unwrap();
                }
                if failure == "end-file" {
                    reply_to_credential_barrier(&mut server);
                }
            });
            let (events, _) = async_channel::unbounded();
            let result = replace_playback_credentials(
                &mut client,
                &mut Vec::new(),
                &mut current,
                None,
                plugin_dir,
                &mut PlaybackState::default(),
                &events,
            );
            assert!(
                !result
                    .unwrap_err()
                    .to_string()
                    .contains("INVENTED_PRIVATE_DIAGNOSTIC")
            );
            // The actor keeps the old account until core teardown on this fatal error.
            assert!(current.is_some());
            worker.join().unwrap();
        }
    }

    #[cfg(unix)]
    fn local_extractor_player(directory: &Path) -> Player {
        let extractor = directory.join("extractor");
        fs::write(
            &extractor,
            r#"#!/bin/sh
if [ "$3" = --version ]; then echo 2026.08.19; exit; fi
exec python3 -c '
import json, os, signal, sys
from pathlib import Path

assert "^ytmusicaccount$" in sys.argv
assert b"INVENTED_" in sys.stdin.buffer.read()
ready = Path("READY_PATH")
ready.with_suffix(".tmp").write_text(json.dumps({"pid": os.getpid()}))
ready.with_suffix(".tmp").replace(ready)
os.kill(os.getpid(), signal.SIGSTOP)
' "$@"
"#
            .replace("READY_PATH", directory.join("ready").to_str().unwrap()),
        )
        .unwrap();
        fs::set_permissions(&extractor, fs::Permissions::from_mode(0o700)).unwrap();
        Player::with_extractor(
            PlayerOptions {
                credentials: Some(synthetic_credentials("INVENTED_FIRST")),
                audio_output: Some("null".into()),
                ..Default::default()
            },
            extractor,
        )
        .unwrap()
    }

    #[cfg(unix)]
    struct ExtractorProcess {
        pid: u64,
    }

    #[cfg(unix)]
    impl ExtractorProcess {
        fn start(player: &Player, directory: &Path) -> Self {
            let ready = directory.join("ready");
            if ready.exists() {
                fs::remove_file(&ready).unwrap();
            }
            player
                .play_url(
                    "https://music.youtube.com/watch?v=abcdefghijk".into(),
                    "Synthetic local extractor",
                    PlayOptions::default(),
                )
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            while !ready.exists() {
                assert!(Instant::now() < deadline, "local extractor did not start");
                if let Some(PlayerEvent::Error { message, .. }) = player.try_recv() {
                    panic!("{message}");
                }
                thread::sleep(Duration::from_millis(5));
            }
            let data: Value = serde_json::from_slice(&fs::read(ready).unwrap()).unwrap();
            let pid = data["pid"].as_u64().unwrap();
            assert!(alive(pid));
            Self { pid }
        }

        fn assert_revoked(&self) {
            assert!(
                !alive(self.pid),
                "extractor must be reaped before cleanup completes"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "requires libmpv, yt-dlp and python3; synthetic local extractor, no network or audio"]
    fn live_credential_replacement_and_clear_reap_extractors_before_completion() {
        let directory = tempfile::tempdir().unwrap();
        let player = local_extractor_player(directory.path());
        let first = ExtractorProcess::start(&player, directory.path());
        player
            .set_credentials(synthetic_credentials("INVENTED_SECOND"))
            .unwrap()
            .recv_blocking()
            .unwrap()
            .unwrap();
        first.assert_revoked();
        let second = ExtractorProcess::start(&player, directory.path());
        let response = player.clear_credentials().unwrap();
        response.recv_blocking().unwrap().unwrap();
        second.assert_revoked();
        player
            .clear_credentials()
            .unwrap()
            .recv_blocking()
            .unwrap()
            .unwrap();
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "requires libmpv, yt-dlp and python3; synthetic local extractor, no network or audio"]
    fn live_immediate_play_then_clear_credentials_does_not_leave_an_extractor() {
        let directory = tempfile::tempdir().unwrap();
        let player = local_extractor_player(directory.path());
        let ready = directory.path().join("ready");
        let extractor = directory.path().join("extractor");
        let extractor = extractor.to_str().unwrap();
        let mut started = 0;
        for _ in 0..100 {
            player
                .set_credentials(synthetic_credentials("INVENTED_IMMEDIATE"))
                .unwrap()
                .recv_blocking()
                .unwrap()
                .unwrap();
            player
                .play_url(
                    "https://music.youtube.com/watch?v=abcdefghijk".into(),
                    "Synthetic immediate clear",
                    PlayOptions::default(),
                )
                .unwrap();
            // Deliberately no readiness wait or event read between Play and clear.
            player
                .clear_credentials()
                .unwrap()
                .recv_blocking()
                .unwrap()
                .unwrap();
            if ready.exists() {
                let data: Value = serde_json::from_slice(&fs::read(&ready).unwrap()).unwrap();
                let pid = data["pid"].as_u64().unwrap();
                assert!(!alive(pid), "extractor survived immediate credential clear");
                started += 1;
                fs::remove_file(&ready).unwrap();
            }
            // A process may be running before Python writes its ready marker.
            // Only inspect this test process's children, identified by our script.
            for task in fs::read_dir("/proc/self/task").unwrap() {
                let task = task.unwrap();
                let children = fs::read_to_string(task.path().join("children")).unwrap_or_default();
                for pid in children.split_whitespace() {
                    let args = fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
                    assert!(
                        !args
                            .split(|byte| *byte == 0)
                            .any(|arg| arg == extractor.as_bytes()),
                        "extractor {pid} survived before writing its ready marker"
                    );
                }
            }
        }
        eprintln!(
            "100 immediate Play/clear cycles passed; {started} extractors reached the ready marker"
        );
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "requires libmpv, yt-dlp and python3; synthetic local extractor, no network or audio"]
    fn live_player_drop_and_fatal_exit_revoke_extractor_credentials() {
        for fatal in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let player = local_extractor_player(directory.path());
            let extractor = ExtractorProcess::start(&player, directory.path());
            if fatal {
                player.command(json!(["quit"])).unwrap();
                let deadline = Instant::now() + Duration::from_secs(5);
                loop {
                    if let Some(PlayerEvent::Error { track_id: None, .. }) = player.try_recv() {
                        break;
                    }
                    assert!(
                        Instant::now() < deadline,
                        "player did not report its fatal exit"
                    );
                    thread::sleep(Duration::from_millis(5));
                }
                assert!(player.clear_credentials().is_err());
                extractor.assert_revoked();
            }
            drop(player);
            extractor.assert_revoked();
        }
    }

    #[test]
    #[ignore = "requires Python with yt-dlp 2026.08.19; set YT_DLP_TEST_PYTHON if needed; no network"]
    fn embedded_extractor_binds_the_selected_account() {
        let runtime = tempfile::tempdir().unwrap();
        let plugin_dir = write_account_extractor(runtime.path()).unwrap();
        let python = std::env::var_os("YT_DLP_TEST_PYTHON").unwrap_or_else(|| "python3".into());
        let server_script = runtime.path().join("ytdlp_resolver.py");
        fs::write(&server_script, include_str!("ytdlp_resolver.py")).unwrap();
        let mut child = Command::new(python)
            .args(["-B", "-"])
            .arg(plugin_dir)
            .arg(server_script)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(include_bytes!("ytdlp_account_tests.py"))
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "measures /proc actor wakeups and latency with libmpv, null audio and synthetic silence"]
    fn local_actor_wakeups_and_latency() {
        let directory = tempfile::tempdir().unwrap();
        let path = silent_audio(directory.path(), 120);
        let player = Player::new(PlayerOptions {
            volume: 0.,
            audio_output: Some("null".into()),
            ..Default::default()
        })
        .unwrap();
        let wait_for = |predicate: &dyn Fn(&PlayerEvent) -> bool| {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                assert!(
                    Instant::now() < deadline,
                    "player did not produce the expected event"
                );
                match player.try_recv() {
                    Some(PlayerEvent::Error { message, .. }) => panic!("{message}"),
                    Some(event) if predicate(&event) => break,
                    Some(_) => {}
                    None => thread::sleep(Duration::from_millis(1)),
                }
            }
        };
        wait_for(&|event| matches!(event, PlayerEvent::Speed(_)));
        let status = fs::read_dir("/proc/self/task")
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                fs::read_to_string(path.join("comm"))
                    .is_ok_and(|name| name.trim() == "music-player")
            })
            .unwrap()
            .join("status");
        let switches = || -> u64 {
            fs::read_to_string(&status)
                .unwrap()
                .lines()
                .find_map(|line| {
                    line.strip_prefix("voluntary_ctxt_switches:")
                        .map(|n| n.trim().parse().unwrap())
                })
                .unwrap()
        };
        let measure = |mode: &str| {
            thread::sleep(Duration::from_secs(2));
            for sample in 1..=3 {
                let before = switches();
                let start = Instant::now();
                thread::sleep(Duration::from_secs(5));
                println!(
                    "mode={mode} sample={sample} wakeups_per_s={:.2}",
                    (switches() - before) as f64 / start.elapsed().as_secs_f64()
                );
            }
        };
        measure("idle");
        for trial in 1..=3 {
            while player.try_recv().is_some() {}
            let start = Instant::now();
            let id = player
                .play_url(
                    path.to_string_lossy().into_owned(),
                    "Synthetic silence",
                    PlayOptions::default(),
                )
                .unwrap();
            wait_for(
                &|event| matches!(event, PlayerEvent::Position { track_id, seconds } if *track_id == id && *seconds > 0.),
            );
            println!(
                "trial={trial} play_to_audio_ms={:.3}",
                start.elapsed().as_secs_f64() * 1000.
            );
            player.pause(true).unwrap();
            wait_for(
                &|event| matches!(event, PlayerEvent::Paused { track_id, paused: true } if *track_id == id),
            );
            if trial == 1 {
                measure("paused");
            }
            for seconds in [10., 30., 50.] {
                let start = Instant::now();
                let revision = player.seek(id, seconds).unwrap();
                wait_for(
                    &|event| matches!(event, PlayerEvent::SeekFinished { revision: current, result: Ok(actual), .. } if *current == revision && (*actual - seconds).abs() < 0.5),
                );
                println!(
                    "trial={trial} seek_to={seconds} seek_ms={:.3}",
                    start.elapsed().as_secs_f64() * 1000.
                );
            }
            player.pause(false).unwrap();
            wait_for(
                &|event| matches!(event, PlayerEvent::Paused { track_id, paused: false } if *track_id == id),
            );
            if trial == 1 {
                measure("playing");
            }
        }
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "requires libmpv and a synthetic local extractor, no network or audio output"]
    fn playback_checks_the_account_version_once_when_needed() {
        for check_account_at_startup in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let extractor = directory.path().join("extractor");
            let probes = directory.path().join("version-probes");
            let script = format!(
                concat!(
                    "#!/bin/sh\n",
                    "[ \"$3\" = --version ] || exit 1\n",
                    "echo probe >> \"$(dirname \"$0\")/version-probes\"\n",
                    "echo {}\n",
                ),
                REVIEWED_YTDLP,
            );
            fs::write(&extractor, script).unwrap();
            fs::set_permissions(&extractor, fs::Permissions::from_mode(0o700)).unwrap();
            let path = silent_audio(directory.path(), 10);
            let player = Player::with_extractor(
                PlayerOptions {
                    check_account_at_startup,
                    volume: 0.,
                    audio_output: Some("null".into()),
                    ..Default::default()
                },
                extractor,
            )
            .unwrap();
            let id = player
                .play_url(
                    path.to_string_lossy().into_owned(),
                    "Synthetic guest",
                    PlayOptions::default(),
                )
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                assert!(Instant::now() < deadline, "Guest playback did not advance");
                match player.try_recv() {
                    Some(PlayerEvent::Error { message, .. }) => panic!("{message}"),
                    Some(PlayerEvent::Position { track_id, seconds })
                        if track_id == id && seconds > 0. =>
                    {
                        break;
                    }
                    Some(_) => {}
                    None => thread::sleep(Duration::from_millis(1)),
                }
            }
            assert_eq!(probes.exists(), check_account_at_startup);
            for value in ["INVENTED_FIRST", "INVENTED_SECOND"] {
                player
                    .set_credentials(synthetic_credentials(value))
                    .unwrap()
                    .recv_blocking()
                    .unwrap()
                    .unwrap();
                player
                    .clear_credentials()
                    .unwrap()
                    .recv_blocking()
                    .unwrap()
                    .unwrap();
            }
            assert_eq!(fs::read_to_string(probes).unwrap(), "probe\n");
        }
    }

    fn silent_audio(directory: &Path, seconds: u32) -> PathBuf {
        let path = directory.join("silence.wav");
        let sample_bytes = seconds * 8000 * 2;
        let mut file = File::create(&path).unwrap();
        file.write_all(b"RIFF").unwrap();
        file.write_all(&(36 + sample_bytes).to_le_bytes()).unwrap();
        file.write_all(b"WAVEfmt ").unwrap();
        file.write_all(&16_u32.to_le_bytes()).unwrap();
        file.write_all(&1_u16.to_le_bytes()).unwrap();
        file.write_all(&1_u16.to_le_bytes()).unwrap();
        file.write_all(&8000_u32.to_le_bytes()).unwrap();
        file.write_all(&16000_u32.to_le_bytes()).unwrap();
        file.write_all(&2_u16.to_le_bytes()).unwrap();
        file.write_all(&16_u16.to_le_bytes()).unwrap();
        file.write_all(b"data").unwrap();
        file.write_all(&sample_bytes.to_le_bytes()).unwrap();
        file.write_all(&vec![0; sample_bytes as usize]).unwrap();
        drop(file);
        path
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "timing sample with libmpv, yt-dlp, local silent audio and no network"]
    fn measure_local_guest_playback() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let tracks = [
            silent_audio(first.path(), 10),
            silent_audio(second.path(), 10),
        ];
        let mut first_play = Vec::new();
        let mut switches = Vec::new();
        let mut playing_pss = Vec::new();
        for _ in 0..5 {
            let player = Player::new(PlayerOptions {
                volume: 0.,
                audio_output: Some("null".into()),
                ..Default::default()
            })
            .unwrap();
            for trial in 0..=40 {
                let file = &tracks[trial % tracks.len()];
                let start = Instant::now();
                let id = player
                    .play_url(
                        file.to_string_lossy().into_owned(),
                        "Synthetic silence",
                        PlayOptions::default(),
                    )
                    .unwrap();
                let elapsed = loop {
                    assert!(
                        start.elapsed() < Duration::from_secs(5),
                        "Track never advanced"
                    );
                    match player.try_recv() {
                        Some(PlayerEvent::Position { track_id, seconds })
                            if track_id == id && seconds > 0. =>
                        {
                            break start.elapsed().as_secs_f64() * 1000.;
                        }
                        Some(PlayerEvent::Error { message, .. }) => panic!("{message}"),
                        Some(_) => {}
                        None => thread::sleep(Duration::from_millis(1)),
                    }
                };
                if trial == 0 {
                    first_play.push(elapsed);
                } else {
                    switches.push(elapsed);
                }
            }
            // Memory once the last track has played for a second.
            thread::sleep(Duration::from_secs(1));
            let rollup = fs::read_to_string("/proc/self/smaps_rollup").unwrap();
            let pss_kib: f64 = rollup
                .lines()
                .find_map(|line| line.strip_prefix("Pss:"))
                .and_then(|value| value.trim().strip_suffix("kB")?.trim().parse().ok())
                .unwrap();
            playing_pss.push(pss_kib / 1024.);
        }
        println!(
            "playback_samples {}",
            json!({
                "first_position_ms": first_play,
                "warm_switch_ms": switches,
                "playing_pss_mib": playing_pss,
            })
        );
    }

    /// A lookup server that answers every request: the page with visitor data,
    /// and a player response for the requested video.
    #[cfg(target_os = "linux")]
    fn counting_lookup() -> (native::Lookup, Arc<AtomicU64>) {
        use std::io::{BufRead, BufReader};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let lookup =
            native::tests::lookup_at(&format!("http://{}", listener.local_addr().unwrap()));
        let requests = Arc::new(AtomicU64::new(0));
        let served = requests.clone();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                served.fetch_add(1, Ordering::SeqCst);
                let mut input = BufReader::new(&stream);
                let mut line = String::new();
                input.read_line(&mut line).unwrap();
                let mut length = 0;
                loop {
                    let mut header = String::new();
                    input.read_line(&mut header).unwrap();
                    if header == "\r\n" {
                        break;
                    }
                    if let Some(value) = header.to_lowercase().strip_prefix("content-length:") {
                        length = value.trim().parse().unwrap();
                    }
                }
                let mut body = vec![0; length];
                input.read_exact(&mut body).unwrap();
                let response = if line.starts_with("GET ") {
                    native::tests::homepage()
                } else {
                    let request: Value = serde_json::from_slice(&body).unwrap();
                    let mut player = native::tests::fixture();
                    player["videoDetails"]["videoId"] = request["videoId"].clone();
                    player.to_string()
                };
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                    response.len()
                )
                .unwrap();
            }
        });
        (lookup, requests)
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "count sample with libmpv and a local lookup server; streams fail to open without network"]
    fn measure_play_requests() {
        let directory = tempfile::tempdir().unwrap();
        let extractor = synthetic_full_extractor(directory.path(), false);
        let (lookup, requests) = counting_lookup();
        let player = Player::with_lookup(
            PlayerOptions {
                volume: 0.,
                audio_output: Some("null".into()),
                ..Default::default()
            },
            extractor,
            Some(lookup),
        )
        .unwrap();
        // mpv refuses every stream URL at once, so no request leaves the machine.
        player
            .command(json!([
                "set_property",
                "stream-lavf-o",
                "protocol_whitelist=file"
            ]))
            .unwrap();
        let wait_for = |step: &str, predicate: &dyn Fn(&PlayerEvent) -> bool| {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                assert!(
                    Instant::now() < deadline,
                    "{step}: the player never got there"
                );
                match player.try_recv() {
                    Some(event) if predicate(&event) => break,
                    Some(_) => {}
                    None => thread::sleep(Duration::from_millis(5)),
                }
            }
        };
        // Requests from Play until mpv was asked to open the stream and failed.
        let count_play = |video_id: &str| {
            let before = requests.load(Ordering::SeqCst);
            let id = player
                .play(video_id, "Requests", PlayOptions::default())
                .unwrap();
            wait_for(
                video_id,
                &|event| matches!(event, PlayerEvent::Error { track_id: Some(track), .. } if *track == id),
            );
            requests.load(Ordering::SeqCst) - before
        };
        let unprepared_cold = count_play("aaaaaaaaaaa");
        let unprepared_warm = count_play("bbbbbbbbbbb");
        // As for Next: the following song is prepared while one plays.
        let path = silent_audio(directory.path(), 30);
        let playing = player
            .play_url(
                path.to_string_lossy().into_owned(),
                "Silence",
                PlayOptions::default(),
            )
            .unwrap();
        wait_for(
            "local file",
            &|event| matches!(event, PlayerEvent::Started { track_id, .. } if *track_id == playing),
        );
        player
            .prepare_next(Some("ccccccccccc"), PlayOptions::default())
            .unwrap();
        wait_for(
            "preparation",
            &|event| matches!(event, PlayerEvent::Prepared { video_id } if video_id == "ccccccccccc"),
        );
        let prepared = count_play("ccccccccccc");
        println!(
            "play_requests {}",
            json!({
                "prepared_play_requests": prepared,
                "cold_play_requests": unprepared_cold,
                "warm_play_requests": unprepared_warm,
            })
        );
    }

    #[test]
    #[ignore = "requires libmpv; uses local silent audio, null audio and no network"]
    fn plays_with_the_installed_libmpv() {
        // Release builds run this in each distribution the packages target,
        // against its own libmpv, so options and commands from newer mpv
        // releases cannot break playback there. It uses the player's own
        // startup options and load commands, then reads back what mpv applied.
        let directory = tempfile::tempdir().unwrap();
        let path = silent_audio(directory.path(), 5)
            .to_string_lossy()
            .into_owned();
        let socket_path = ipc_path(directory.path()).unwrap();
        let _core = MpvCore::new(
            &PlayerOptions {
                volume: 0.,
                audio_output: Some("null".into()),
                ..Default::default()
            },
            &socket_path,
        )
        .unwrap();
        // mpv creates its IPC socket on a thread of its own.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut socket = loop {
            match IpcStream::connect(&socket_path) {
                Ok(socket) => break socket,
                Err(error) if Instant::now() < deadline => {
                    use std::io::ErrorKind::{ConnectionRefused, NotFound};
                    assert!(
                        matches!(error.kind(), NotFound | ConnectionRefused),
                        "{error}"
                    );
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("{error}"),
            }
        };
        let mut pending = Vec::new();
        let mut next_id = 0;
        let mut request = |command: Value| {
            next_id += 1;
            write_command(&mut socket, command, Some(json!(next_id))).unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let reply = read_message(&mut socket, &mut pending, deadline).unwrap();
                if reply["request_id"] == next_id {
                    break reply;
                }
            }
        };
        let options = PlayOptions {
            start: 2.5,
            paused: true,
            ..Default::default()
        };
        let reply = request(load_command(&path, "Synthetic silence", -2.29, options));
        assert_eq!(reply["error"], "success", "{reply}");
        let mut next = load_command(&path, "Prepared silence", 0., PlayOptions::default());
        next["flags"] = json!("append");
        let reply = request(next);
        assert_eq!(reply["error"], "success", "{reply}");

        let deadline = Instant::now() + Duration::from_secs(10);
        let position = loop {
            let reply = request(json!(["get_property", "time-pos"]));
            if let Some(seconds) = reply["data"].as_f64() {
                break seconds;
            }
            assert!(Instant::now() < deadline, "the local file never loaded");
            thread::sleep(Duration::from_millis(20));
        };
        let version = request(json!(["get_property", "mpv-version"]))["data"].clone();
        assert!(
            (position - 2.5).abs() < 0.1,
            "{version}: started at {position}"
        );
        assert_eq!(
            request(json!(["get_property", "pause"]))["data"],
            true,
            "{version}"
        );
        assert_eq!(
            request(json!(["get_property", "media-title"]))["data"],
            "Synthetic silence",
            "{version}"
        );
        assert_eq!(
            request(json!(["get_property", "playlist-count"]))["data"],
            2,
            "{version}"
        );
        // mpv 0.37 has no volume-gain and plays unlevelled; any release that
        // has it must apply the per-file value.
        let levelled =
            request(json!(["get_property", "option-info/volume-gain/name"]))["error"] == "success";
        if levelled {
            let gain = request(json!(["get_property", "volume-gain"]))["data"].clone();
            assert!(
                gain.as_f64().is_some_and(|gain| (gain + 2.29).abs() < 1e-6),
                "{version}: volume-gain {gain}"
            );
        }
        let loudness = if levelled { "applied" } else { "unsupported" };
        eprintln!("{version}: start, pause, title and append applied; loudness {loudness}");
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "requires libmpv; uses local silent audio, null audio and no network"]
    fn pausing_an_expired_stream_asks_for_a_reload_unless_mpv_buffered_it_to_the_end() {
        let directory = tempfile::tempdir().unwrap();
        // mpv reports the path it opened, so a local name can carry a lapsed expiry.
        let path = directory.path().join("silence?expire=1.wav");
        fs::rename(silent_audio(directory.path(), 120), &path).unwrap();
        let extractor = directory.path().join("extractor");
        fs::write(&extractor, "#!/bin/sh\necho 2026.08.19\n").unwrap();
        fs::set_permissions(&extractor, fs::Permissions::from_mode(0o700)).unwrap();
        let player = Player::with_extractor(
            PlayerOptions {
                volume: 0.,
                audio_output: Some("null".into()),
                ..Default::default()
            },
            extractor,
        )
        .unwrap();
        // The 1.9 MB file fits the default buffer but not a 64 KiB one.
        for (buffer, reload) in [("32MiB", false), ("64KiB", true)] {
            player
                .command(json!(["set_property", "demuxer-max-bytes", buffer]))
                .unwrap();
            let track_id = player
                .play_url(
                    path.to_string_lossy().into_owned(),
                    "Synthetic silence",
                    PlayOptions::default(),
                )
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            while !matches!(player.try_recv(), Some(PlayerEvent::Started { track_id: id, .. }) if id == track_id)
            {
                assert!(Instant::now() < deadline, "the local file did not start");
                thread::sleep(Duration::from_millis(5));
            }
            // Let mpv read ahead as far as its buffer allows before pausing.
            thread::sleep(Duration::from_millis(500));
            player.pause(true).unwrap();
            let deadline = Instant::now() + Duration::from_secs(2);
            let mut requested = false;
            while Instant::now() < deadline {
                match player.try_recv() {
                    Some(PlayerEvent::StreamExpiring { track_id: id }) if id == track_id => {
                        requested = true;
                    }
                    Some(PlayerEvent::Error { message, .. }) => panic!("{message}"),
                    Some(_) => {}
                    None => thread::sleep(Duration::from_millis(5)),
                }
            }
            assert_eq!(requested, reload, "buffer {buffer}");
        }
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "requires libmpv; uses a synthetic extractor, local silent audio, null audio and no network"]
    fn idle_player_plays_the_restored_song_from_its_preparation() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("restored.test-stream.wav");
        fs::rename(silent_audio(directory.path(), 60), &path).unwrap();
        // Each resolution appends a line, so a play that resolves again is caught.
        let calls = directory.path().join("calls");
        let extractor = directory.path().join("extractor");
        fs::write(
            &extractor,
            format!(
                "#!/bin/sh\nif [ \"$3\" = --version ]; then echo 2026.08.19; exit; fi\necho >> '{}'\nprintf 'file://%s\\n' '{}'\n",
                calls.display(),
                path.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&extractor, fs::Permissions::from_mode(0o700)).unwrap();
        let resolutions = || fs::read_to_string(&calls).map_or(0, |calls| calls.lines().count());
        // A new player each time, idle as at launch: the restored song from the
        // start, then one restored partway in.
        for (start, step) in [(0., "from the start"), (20., "resumed at 0:20")] {
            let player = Player::with_extractor(
                PlayerOptions {
                    volume: 0.,
                    audio_output: Some("null".into()),
                    ..Default::default()
                },
                extractor.clone(),
            )
            .unwrap();
            let wait_for = |stage: &str, predicate: &dyn Fn(&PlayerEvent) -> bool| {
                let deadline = Instant::now() + Duration::from_secs(10);
                loop {
                    assert!(
                        Instant::now() < deadline,
                        "{step}, {stage}: the player never got there"
                    );
                    match player.try_recv() {
                        Some(PlayerEvent::Error { message, .. }) => {
                            panic!("{step}, {stage}: {message}")
                        }
                        Some(event) if predicate(&event) => break,
                        Some(_) => {}
                        None => thread::sleep(Duration::from_millis(5)),
                    }
                }
            };
            player
                .prepare_next(Some("abcdefghijk"), PlayOptions::default())
                .unwrap();
            wait_for("preparing", &|event| {
                matches!(event, PlayerEvent::Prepared { .. })
            });
            let prepared = resolutions();
            let id = player
                .play(
                    "abcdefghijk",
                    "Restored",
                    PlayOptions {
                        start,
                        ..Default::default()
                    },
                )
                .unwrap();
            wait_for("playing", &|event| {
                matches!(event, PlayerEvent::Position { track_id, seconds }
                    if *track_id == id && *seconds > start + 0.2)
            });
            assert_eq!(
                resolutions(),
                prepared,
                "{step}: Play resolved the song again"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "requires libmpv; uses a synthetic extractor, null audio and no network"]
    fn paused_actor_refreshes_expired_preparation_and_writes_pending_seek() {
        let directory = tempfile::tempdir().unwrap();
        let path = silent_audio(directory.path(), 120);
        let extractor = directory.path().join("extractor");
        fs::write(&extractor, "#!/bin/sh\nif [ \"$3\" = --version ]; then echo 2026.08.19; exit; fi\nprintf 'https://rr1.googlevideo.com/audio?expire=%s\\n' \"$(($(date +%s) + 62))\"\n").unwrap();
        fs::set_permissions(&extractor, fs::Permissions::from_mode(0o700)).unwrap();
        let player = Player::with_extractor(
            PlayerOptions {
                volume: 0.,
                audio_output: Some("null".into()),
                ..Default::default()
            },
            extractor,
        )
        .unwrap();
        player
            .command(json!(["set_property", "prefetch-playlist", false]))
            .unwrap();
        let track_id = player
            .play_url(
                path.to_string_lossy().into_owned(),
                "Synthetic silence",
                PlayOptions {
                    paused: true,
                    ..Default::default()
                },
            )
            .unwrap();
        let wait_for = |predicate: &dyn Fn(&PlayerEvent) -> bool| {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                assert!(
                    Instant::now() < deadline,
                    "paused actor did not finish pending work"
                );
                match player.try_recv() {
                    Some(PlayerEvent::Error { message, .. }) => panic!("{message}"),
                    Some(event) if predicate(&event) => break,
                    Some(_) => {}
                    None => thread::sleep(Duration::from_millis(5)),
                }
            }
        };
        wait_for(
            &|event| matches!(event, PlayerEvent::Started { track_id: id, paused: true, .. } if *id == track_id),
        );
        player
            .prepare_next(Some("abcdefghijk"), PlayOptions::default())
            .unwrap();
        wait_for(&|event| matches!(event, PlayerEvent::Prepared { .. }));
        let start = Instant::now();
        wait_for(&|event| matches!(event, PlayerEvent::Prepared { .. }));
        assert!(
            start.elapsed() > Duration::from_millis(800),
            "unexpired streams must stay prepared"
        );
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "expiry must trigger re-preparation"
        );
        let revision = player.seek(track_id, 30.).unwrap();
        wait_for(&|event| {
            matches!(event, PlayerEvent::SeekFinished { revision: current, result: Ok(seconds), .. }
            if *current == revision && (29.9..30.1).contains(seconds))
        });
    }

    #[test]
    #[ignore = "requires installed mpv and yt-dlp; uses only private synthetic local audio"]
    fn local_seek_completion_handles_rapid_no_op_and_end_of_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = silent_audio(directory.path(), 120);
        let player = Player::new(PlayerOptions {
            audio_output: Some("null".into()),
            ..Default::default()
        })
        .unwrap();
        let track_id = player
            .play_url(
                path.to_string_lossy().into_owned(),
                "Synthetic silence",
                PlayOptions {
                    paused: true,
                    ..Default::default()
                },
            )
            .unwrap();
        let wait_for = |predicate: &dyn Fn(&PlayerEvent) -> bool| {
            let deadline = Instant::now() + Duration::from_secs(8);
            while Instant::now() < deadline {
                if let Some(event) = player.try_recv() {
                    if predicate(&event) {
                        return event;
                    }
                    if let PlayerEvent::Error { message, .. } = event {
                        panic!("{message}");
                    }
                } else {
                    thread::sleep(Duration::from_millis(10));
                }
            }
            panic!("mpv did not complete the synthetic seek boundary");
        };
        wait_for(
            &|event| matches!(event,PlayerEvent::Started{track_id:id,paused:true,..} if *id == track_id),
        );
        player.seek(track_id, 10.).unwrap();
        let revision = player.seek(track_id, 50.).unwrap();
        let event = wait_for(
            &|event| matches!(event,PlayerEvent::SeekFinished{revision:id,..} if *id == revision),
        );
        assert!(
            matches!(event,PlayerEvent::SeekFinished{result:Ok(seconds),..} if (49.9..50.1).contains(&seconds))
        );
        let revision = player.seek(track_id, 50.).unwrap();
        let event = wait_for(
            &|event| matches!(event,PlayerEvent::SeekFinished{revision:id,..} if *id == revision),
        );
        assert!(
            matches!(event,PlayerEvent::SeekFinished{result:Ok(seconds),..} if (49.9..50.1).contains(&seconds))
        );
        let revision = player.seek(track_id, 500.).unwrap();
        wait_for(
            &|event| matches!(event,PlayerEvent::SeekFinished{revision:id,..} if *id == revision),
        );
        wait_for(&|event| matches!(event,PlayerEvent::Ended{track_id:id} if *id == track_id));
        let revision = player.seek(track_id, 10.).unwrap();
        let event = wait_for(
            &|event| matches!(event,PlayerEvent::SeekFinished{revision:id,..} if *id == revision),
        );
        assert!(matches!(
            event,
            PlayerEvent::SeekFinished { result: Err(_), .. }
        ));
    }

    #[test]
    #[ignore = "requires mpv, yt-dlp, and network access"]
    fn live_playback() {
        let player = Player::new(PlayerOptions {
            audio_output: Some("null".into()),
            ..Default::default()
        })
        .unwrap();
        let id = player
            .play(
                "csILUDfmtqM",
                "Khruangbin - Hold Me Up",
                PlayOptions::default(),
            )
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(55);
        let mut started = false;
        let mut progressed = false;
        while Instant::now() < deadline {
            if let Some(event) = player.try_recv() {
                println!("{event:?}");
                match event {
                    PlayerEvent::Started {
                        track_id,
                        paused: false,
                        ..
                    } if track_id == id => started = true,
                    PlayerEvent::Position { track_id, seconds }
                        if track_id == id && seconds > 1. =>
                    {
                        progressed = true;
                        break;
                    }
                    PlayerEvent::Error { message, .. } => panic!("{message}"),
                    _ => {}
                }
            } else {
                thread::sleep(Duration::from_millis(50));
            }
        }
        assert!(started && progressed, "stream must begin and progress");
        let speed = PlaybackSpeed::try_from(1.5).unwrap();
        player.set_speed(speed).unwrap();
        player.pause(true).unwrap();
        let seek_revision = player.seek(id, 30.).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut paused = false;
        let mut sought = false;
        let mut sped_up = false;
        while Instant::now() < deadline && !(paused && sought && sped_up) {
            match player.try_recv() {
                Some(PlayerEvent::Paused { paused: true, .. }) => paused = true,
                Some(PlayerEvent::SeekFinished {
                    track_id,
                    revision,
                    result,
                }) if track_id == id && revision == seek_revision => {
                    assert!(result.unwrap() >= 29.);
                    sought = true;
                }
                Some(PlayerEvent::Speed(value)) if value == speed => sped_up = true,
                _ => thread::sleep(Duration::from_millis(50)),
            }
        }
        assert!(
            paused && sought && sped_up,
            "pause, seek and playback speed must reach mpv"
        );

        let replacement = player
            .play(
                "csILUDfmtqM",
                "Khruangbin - Hold Me Up",
                PlayOptions {
                    video: None,
                    quality: AudioQuality::Low,
                    start: 30.,
                    paused: true,
                    full_lookup: false,
                },
            )
            .unwrap();
        assert_ne!(replacement, id);
        let deadline = Instant::now() + Duration::from_secs(55);
        let mut resumed_paused = false;
        let mut resumed_position = false;
        while Instant::now() < deadline && !(resumed_paused && resumed_position) {
            match player.try_recv() {
                Some(event) => {
                    println!("{event:?}");
                    match event {
                        PlayerEvent::Started {
                            track_id, paused, ..
                        } if track_id == replacement => {
                            assert!(paused, "quality reload must retain pause");
                            resumed_paused = true;
                        }
                        PlayerEvent::Position { track_id, seconds } if track_id == replacement => {
                            assert!(
                                (29.5..30.5).contains(&seconds),
                                "reload must resume at 30 seconds"
                            );
                            resumed_position = true;
                        }
                        PlayerEvent::Ended { track_id } if track_id == replacement => {
                            panic!("quality reload ended prematurely")
                        }
                        PlayerEvent::Error { message, .. } => panic!("{message}"),
                        _ => {}
                    }
                }
                None => thread::sleep(Duration::from_millis(50)),
            }
        }
        assert!(
            resumed_paused && resumed_position,
            "quality reload must start paused at its saved position"
        );
        thread::sleep(Duration::from_millis(600));
        while let Some(event) = player.try_recv() {
            match event {
                PlayerEvent::Position { track_id, seconds } if track_id == replacement => {
                    assert!(
                        (29.5..30.5).contains(&seconds),
                        "paused reload must not advance"
                    );
                }
                PlayerEvent::Paused { track_id, paused } if track_id == replacement => {
                    assert!(paused)
                }
                PlayerEvent::Error { message, .. } => panic!("{message}"),
                _ => {}
            }
        }
        // A no-op pause still confirms, then external media controls can unpause.
        player.pause(true).unwrap();
        player
            .command(json!(["set_property", "pause", false]))
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut unpaused = false;
        let mut resumed_progress = false;
        while Instant::now() < deadline && !(unpaused && resumed_progress) {
            match player.try_recv() {
                Some(PlayerEvent::Paused {
                    track_id,
                    paused: false,
                }) if track_id == replacement => unpaused = true,
                Some(PlayerEvent::Position { track_id, seconds })
                    if track_id == replacement && seconds > 31. =>
                {
                    resumed_progress = true
                }
                Some(PlayerEvent::Error { message, .. }) => panic!("{message}"),
                _ => thread::sleep(Duration::from_millis(50)),
            }
        }
        assert!(
            unpaused && resumed_progress,
            "quality reload must continue after unpausing"
        );
    }
}
