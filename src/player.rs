use crate::auth::{PlaybackCredentials, private_runtime, sealed_file};
use crate::video::NativeVideo;
use anyhow::{Context, Result, bail, ensure};
use gpui::{AnyWindowHandle, App, BackgroundExecutor, Bounds, Pixels, Subscription, Window};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    cell::RefCell,
    collections::HashMap,
    ffi::{CStr, CString, c_char, c_int, c_void},
    fs::{self, File},
    io::{self, ErrorKind, Read, Seek, Write},
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    os::unix::fs::PermissionsExt,
    os::unix::net::UnixStream,
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    ptr::NonNull,
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicI64, AtomicU64, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

mod native;

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
    pub volume: f64,
    pub speed: PlaybackSpeed,
    pub audio_output: Option<String>,
}

impl Default for PlayerOptions {
    fn default() -> Self {
        Self {
            credentials: None,
            volume: 60.,
            speed: PlaybackSpeed::default(),
            audio_output: None,
        }
    }
}

#[derive(Debug)]
pub enum PlayerEvent {
    Started {
        track_id: u64,
        paused: bool,
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
    Ended {
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
    Volume(f64),
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
    wake: Arc<OwnedFd>,
}

fn command_channel() -> Result<(CommandSender, Receiver<PlayerCommand>)> {
    let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
    if fd < 0 {
        return Err(io::Error::last_os_error())
            .context("Could not create the player command wakeup");
    }
    let wake = Arc::new(unsafe { OwnedFd::from_raw_fd(fd) });
    let (sender, receiver) = mpsc::channel();
    Ok((CommandSender { sender, wake }, receiver))
}

impl CommandSender {
    fn send(&self, command: PlayerCommand) -> Result<(), mpsc::SendError<PlayerCommand>> {
        self.sender.send(command)?;
        signal_player(&self.wake);
        Ok(())
    }
}

fn signal_player(wake: &OwnedFd) {
    let increment = 1_u64.to_ne_bytes();
    loop {
        let result =
            unsafe { libc::write(wake.as_raw_fd(), increment.as_ptr().cast(), increment.len()) };
        if result == increment.len() as isize {
            return;
        }
        let error = io::Error::last_os_error();
        match error.kind() {
            ErrorKind::Interrupted => continue,
            // A saturated eventfd is already readable, so no wakeup is lost.
            ErrorKind::WouldBlock => return,
            _ => unreachable!("The owned player wakeup must remain writable: {error}"),
        }
    }
}

fn wait_for_player_input(
    socket: &UnixStream,
    wake: &OwnedFd,
    resolver_fd: Option<c_int>,
    deadline: Option<Instant>,
) -> Result<bool> {
    let mut descriptors = [
        libc::pollfd {
            fd: socket.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: wake.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: resolver_fd.unwrap_or(-1),
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    loop {
        let timeout = deadline.map(|deadline| {
            let remaining = deadline.saturating_duration_since(Instant::now());
            libc::timespec {
                tv_sec: remaining.as_secs() as _,
                tv_nsec: remaining.subsec_nanos() as _,
            }
        });
        let result = unsafe {
            libc::ppoll(
                descriptors.as_mut_ptr(),
                descriptors.len() as _,
                timeout.as_ref().map_or(std::ptr::null(), |timeout| timeout),
                std::ptr::null(),
            )
        };
        if result >= 0 {
            break;
        }
        let error = io::Error::last_os_error();
        if error.kind() != ErrorKind::Interrupted {
            return Err(error).context("Could not wait for the player");
        }
    }
    ensure!(
        descriptors[1].revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) == 0,
        "The player command wakeup stopped"
    );
    if descriptors[1].revents & libc::POLLIN != 0 {
        let mut count = [0_u8; 8];
        loop {
            let result =
                unsafe { libc::read(wake.as_raw_fd(), count.as_mut_ptr().cast(), count.len()) };
            if result == count.len() as isize {
                break;
            }
            let error = io::Error::last_os_error();
            match error.kind() {
                ErrorKind::Interrupted => continue,
                ErrorKind::WouldBlock => break,
                _ => return Err(error).context("Could not read the player command wakeup"),
            }
        }
    }
    Ok(descriptors[0].revents != 0)
}

pub struct Player {
    commands: CommandSender,
    events: async_channel::Receiver<PlayerEvent>,
    next_id: AtomicU64,
    next: Mutex<Option<PreparedTarget>>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Player {
    pub fn new(options: PlayerOptions) -> Result<Self> {
        Self::with_lookup(options, find_ytdlp()?, native::Lookup::new().ok())
    }

    #[cfg(test)]
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
            worker: Some(worker),
        })
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
        let mut next = self.next.lock().unwrap();
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
        let next = self.next.lock().unwrap().take();
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
        self.next.lock().unwrap().take();
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
        self.next.lock().unwrap().take();
        let (reply, response) = async_channel::bounded(1);
        self.send(PlayerCommand::Credentials { next: None, reply })?;
        Ok(response)
    }
    pub fn pause(&self, paused: bool) -> Result<()> {
        self.send(PlayerCommand::Pause(paused))
    }
    pub fn stop(&self) -> Result<()> {
        self.next.lock().unwrap().take();
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

#[repr(C)]
struct MpvEvent {
    event_id: c_int,
    error: c_int,
    reply_userdata: u64,
    data: *mut c_void,
}

#[link(name = "mpv")]
unsafe extern "C" {
    fn mpv_create() -> *mut c_void;
    fn mpv_set_option_string(
        handle: *mut c_void,
        name: *const c_char,
        value: *const c_char,
    ) -> c_int;
    fn mpv_initialize(handle: *mut c_void) -> c_int;
    fn mpv_wait_event(handle: *mut c_void, timeout: f64) -> *const MpvEvent;
    fn mpv_terminate_destroy(handle: *mut c_void);
}

struct MpvCore(NonNull<c_void>);

impl MpvCore {
    fn new(options: &PlayerOptions, socket: &Path) -> Result<Self> {
        let core = Self(NonNull::new(unsafe { mpv_create() }).context("Could not create mpv")?);
        for (name, value) in [
            (c"terminal", "no"),
            (c"config", "no"),
            (c"load-scripts", "no"),
            // mpv also starts a Lua VM for each built-in script. This player is
            // controlled only over IPC, so none of them has any use.
            (c"load-auto-profiles", "no"),
            (c"load-commands", "no"),
            (c"load-console", "no"),
            (c"load-context-menu", "no"),
            (c"load-positioning", "no"),
            (c"load-select", "no"),
            (c"load-stats-overlay", "no"),
            (c"idle", "yes"),
            (c"vid", "no"),
            (c"vo", "libmpv"),
            (c"hwdec", "auto-safe"),
            (c"audio-display", "no"),
            (c"osc", "no"),
            (c"input-default-bindings", "no"),
            (c"input-vo-keyboard", "no"),
            (c"network-timeout", "20"),
            (c"cache", "yes"),
            (c"gapless-audio", "yes"),
            (c"prefetch-playlist", "yes"),
            (c"demuxer-max-bytes", "32MiB"),
            (c"demuxer-max-back-bytes", "8MiB"),
            (c"audio-client-name", "YouTube Music"),
            (c"ytdl", "no"),
        ] {
            core.set_option(name, value)?;
        }
        core.set_option(c"volume", &options.volume.clamp(0., 100.).to_string())?;
        core.set_option(c"speed", &f64::from(options.speed).to_string())?;
        core.set_option(
            c"input-ipc-server",
            socket
                .to_str()
                .context("Invalid private playback directory")?,
        )?;
        if let Some(ao) = &options.audio_output {
            core.set_option(c"ao", ao)?;
        }
        ensure!(
            unsafe { mpv_initialize(core.0.as_ptr()) } >= 0,
            "Could not initialize mpv"
        );
        Ok(core)
    }

    fn set_option(&self, name: &CStr, value: &str) -> Result<()> {
        let value = CString::new(value).context("Invalid mpv startup option")?;
        let result =
            unsafe { mpv_set_option_string(self.0.as_ptr(), name.as_ptr(), value.as_ptr()) };
        ensure!(
            result >= 0,
            "Could not set mpv startup option {} ({result})",
            name.to_string_lossy()
        );
        Ok(())
    }

    fn is_shutdown(&self) -> bool {
        loop {
            let event = unsafe { mpv_wait_event(self.0.as_ptr(), 0.) };
            match unsafe { (*event).event_id } {
                0 => return false,
                1 => return true,
                _ => {}
            }
        }
    }
}

impl Drop for MpvCore {
    fn drop(&mut self) {
        unsafe { mpv_terminate_destroy(self.0.as_ptr()) };
    }
}

fn write_command(socket: &mut UnixStream, command: Value, request_id: Option<Value>) -> Result<()> {
    let mut message = json!({ "command": command });
    if let Some(id) = request_id {
        message["request_id"] = id;
    }
    let mut bytes = serde_json::to_vec(&message)?;
    bytes.push(b'\n');
    socket
        .write_all(&bytes)
        .context("Could not send the command to mpv")
}

fn write_pause(socket: &mut UnixStream, paused: bool, revision: u64) -> Result<()> {
    write_command(socket, json!(["set_property", "pause", paused]), None)?;
    // A no-op set produces no property-change; always acknowledge the latest intent.
    write_command(
        socket,
        json!(["get_property", "pause"]),
        Some(json!(format!("pause:{revision}"))),
    )
}

fn load_command(url: &str, title: &str, options: PlayOptions) -> Value {
    json!(["loadfile", url, "replace", -1, {
        "force-media-title": title,
        "ytdl": "no",
        "start": options.start.to_string(),
        "pause": if options.paused { "yes" } else { "no" },
    }])
}

fn writable_playback_cookies(mut source: File) -> Result<File> {
    // yt-dlp rewrites its cookie jar on exit, so give it a disposable RAM copy.
    let fd = unsafe { libc::memfd_create(c"yt-music-playback".as_ptr(), libc::MFD_CLOEXEC) };
    ensure!(fd >= 0, "Could not create private playback credentials");
    let mut file = unsafe { File::from_raw_fd(fd) };
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    source.rewind()?;
    std::io::copy(&mut source, &mut file)?;
    Ok(file)
}

fn request(
    socket: &mut UnixStream,
    pending: &mut Vec<u8>,
    command: Value,
    mut on_event: impl FnMut(Value),
) -> Result<Value> {
    // Positive IDs identify tracks and seeks; mpv uses zero for ID-less commands.
    static NEXT_REQUEST_ID: AtomicI64 = AtomicI64::new(-1);
    let request_id = NEXT_REQUEST_ID.fetch_sub(1, Ordering::Relaxed);
    write_command(socket, command, Some(json!(request_id)))?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let value = read_message(socket, pending, deadline)?;
        if value["request_id"].as_i64() == Some(request_id) {
            ensure!(
                value["error"] == "success",
                "mpv could not update playback credentials"
            );
            return Ok(value["data"].clone());
        }
        on_event(value);
    }
}

fn read_message(
    socket: &mut UnixStream,
    pending: &mut Vec<u8>,
    deadline: Instant,
) -> Result<Value> {
    loop {
        let timeout = deadline
            .checked_duration_since(Instant::now())
            .filter(|timeout| !timeout.is_zero())
            .context("mpv stopped responding")?;
        if let Some(end) = pending.iter().position(|c| *c == b'\n') {
            let line: Vec<_> = pending.drain(..=end).collect();
            return serde_json::from_slice(&line).context("Invalid mpv response");
        }
        let mut bytes = [0; 16384];
        socket.set_read_timeout(Some(timeout))?;
        match socket.read(&mut bytes) {
            Ok(0) => bail!("mpv closed its connection"),
            Ok(size) => {
                pending.extend_from_slice(&bytes[..size]);
                ensure!(pending.len() <= 1024 * 1024, "Oversized mpv response");
            }
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                ) => {}
            Err(error) => return Err(error).context("Lost the connection to mpv"),
        }
    }
}

const ACCOUNT_EXTRACTOR: &str = include_str!("ytdlp_account.py");

struct ExtractorCredentials {
    cookies: File,
    _identity: File,
    config: File,
}

fn write_account_extractor(runtime: &Path) -> Result<PathBuf> {
    let root = runtime.join("plugins");
    let directory = root.join("account/yt_dlp_plugins/extractor");
    fs::create_dir_all(&directory)?;
    fs::write(directory.join("account.py"), ACCOUNT_EXTRACTOR)?;
    Ok(root)
}

fn prepare_credentials(
    source: PlaybackCredentials,
    plugin_dir: &Path,
) -> Result<ExtractorCredentials> {
    let cookies = writable_playback_cookies(source.cookies)?;
    let identity = source.identity;
    let plugin_dir = plugin_dir
        .to_str()
        .context("Invalid private playback directory")?;
    let quoted_directory = format!("'{}'", plugin_dir.replace('\'', "'\"'\"'"));
    // Keep the account-only extractor and its sealed identity together in RAM.
    let config = format!(
        "--no-plugin-dirs\n--plugin-dirs {quoted_directory}\n--use-extractors '^ytmusicaccount$'\n--extractor-args ytmusicaccount:context=/proc/{}/fd/{}\n",
        std::process::id(),
        identity.as_raw_fd()
    );
    Ok(ExtractorCredentials {
        cookies,
        _identity: identity,
        config: sealed_file(config.as_bytes())?,
    })
}

fn replace_playback_credentials(
    socket: &mut UnixStream,
    pending: &mut Vec<u8>,
    current: &mut Option<ExtractorCredentials>,
    source: Option<PlaybackCredentials>,
    plugin_dir: &Path,
    state: &mut PlaybackState,
    events: &async_channel::Sender<PlayerEvent>,
) -> Result<()> {
    let next = source
        .map(|source| prepare_credentials(source, plugin_dir))
        .transpose()?;
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

const REVIEWED_YTDLP: &str = "2026.08.19";

fn account_playback_supported(ytdlp: &Path) -> bool {
    Command::new(ytdlp)
        .args(["--ignore-config", "--no-plugin-dirs", "--version"])
        .output()
        .is_ok_and(|output| {
            output.status.success()
                && String::from_utf8_lossy(&output.stdout).trim() == REVIEWED_YTDLP
        })
}

fn account_credentials(
    source: Option<PlaybackCredentials>,
    supported: bool,
    warned: bool,
    notice: &mut Option<String>,
) -> Option<PlaybackCredentials> {
    if supported || source.is_none() {
        *notice = None;
        return source;
    }
    if !warned {
        *notice = Some(format!(
            "Signed-in playback requires yt-dlp {REVIEWED_YTDLP}. Playing public streams as a guest with the installed version. Account-only songs may be unavailable."
        ));
    }
    None
}

#[derive(Clone, PartialEq, Eq)]
struct StreamKey {
    url: String,
    quality: AudioQuality,
    video: Option<VideoQuality>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResolverLane {
    Fast,
    Full,
}

#[derive(Clone)]
struct PreparedTarget {
    lane: ResolverLane,
    key: StreamKey,
    track_id: u64,
}

struct ResolvedStream {
    key: StreamKey,
    url: String,
    expires: SystemTime,
}

impl ResolvedStream {
    fn parse(key: StreamKey, bytes: &[u8], now: SystemTime) -> Result<Self> {
        let text = std::str::from_utf8(bytes)?;
        let urls = text
            .lines()
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>();
        ensure!(
            !urls.is_empty() && urls.len() <= 2,
            "Invalid prepared stream"
        );
        let mut expires = now + Duration::from_secs(5 * 60);
        for url in &urls {
            let parsed = reqwest::Url::parse(url)?;
            ensure!(
                parsed.scheme() == "https"
                    && parsed
                        .host_str()
                        .is_some_and(|host| host.ends_with(".googlevideo.com")),
                "Unsupported prepared stream host"
            );
            if let Some(expiry) = parsed.query_pairs().find_map(|(name, value)| {
                (name == "expire")
                    .then(|| value.parse::<u64>().ok())
                    .flatten()
            }) {
                expires = expires.min(UNIX_EPOCH + Duration::from_secs(expiry.saturating_sub(60)));
            }
        }
        ensure!(expires > now, "Prepared stream expired");
        let url = if urls.len() == 1 {
            urls[0].to_owned()
        } else {
            format!(
                "edl://%{}%{};!new_stream;%{}%{}",
                urls[0].len(),
                urls[0],
                urls[1].len(),
                urls[1]
            )
        };
        Ok(Self { key, url, expires })
    }

    fn matches(&self, key: &StreamKey) -> bool {
        self.key == *key && SystemTime::now() < self.expires
    }
}

impl StreamKey {
    fn lane(&self, preparing: bool, signed_in: bool) -> ResolverLane {
        if self.video.is_some_and(|video| video.height() > 1080)
            || (preparing && signed_in && self.quality == AudioQuality::High)
        {
            ResolverLane::Full
        } else {
            ResolverLane::Fast
        }
    }

    fn selector(&self) -> String {
        self.video
            .map(|video| video.selector(self.quality))
            .unwrap_or_else(|| self.quality.selector().to_owned())
    }
}

fn extractor_arguments(
    command: &mut Command,
    credentials: Option<&ExtractorCredentials>,
) -> Result<Option<File>> {
    command.args([
        "--ignore-config",
        "--no-playlist",
        "--no-warnings",
        "--quiet",
        "--get-url",
        // Extraction must not persist account data or signed URLs in a disk cache.
        "--no-cache-dir",
    ]);
    let cookies = credentials
        .map(|credentials| writable_playback_cookies(credentials.cookies.try_clone()?))
        .transpose()?;
    if let Some(credentials) = credentials {
        command
            .arg("--cookies")
            .arg(format!(
                "/proc/{}/fd/{}",
                std::process::id(),
                cookies.as_ref().unwrap().as_raw_fd()
            ))
            .arg("--config-locations")
            .arg(format!(
                "/proc/{}/fd/{}",
                std::process::id(),
                credentials.config.as_raw_fd()
            ));
    } else {
        command.arg("--no-plugin-dirs");
    }
    Ok(cookies)
}

fn python_interpreter(ytdlp: &Path) -> Result<Command> {
    let mut header = [0; 1024];
    let count = File::open(ytdlp)?.read(&mut header)?;
    let line = header[..count].split(|byte| *byte == b'\n').next().unwrap();
    let line = std::str::from_utf8(line)?
        .strip_prefix("#!")
        .context("Not a Python script")?;
    let words: Vec<_> = line.split_whitespace().collect();
    let executable = *words.first().context("Missing interpreter")?;
    let mut interpreter = 0;
    if Path::new(executable)
        .file_name()
        .is_some_and(|name| name == "env")
    {
        interpreter = if words.get(1) == Some(&"-S") { 2 } else { 1 };
    }
    let name = Path::new(
        words
            .get(interpreter)
            .context("Missing Python interpreter")?,
    )
    .file_name()
    .and_then(|name| name.to_str())
    .unwrap_or("");
    ensure!(
        name.strip_prefix("python").is_some_and(|version| {
            version
                .bytes()
                .all(|byte| byte.is_ascii_digit() || byte == b'.')
        }),
        "Not a Python interpreter"
    );
    let mut command = Command::new(executable);
    command.args(&words[1..]);
    Ok(command)
}

/// An extractor holds account cookies in memory. Its own process group keeps it
/// out of terminal signals, so ask the kernel to kill it if the player thread dies
/// without running its destructors (for example when the app is killed).
fn exit_with_parent(command: &mut Command) {
    let parent = std::process::id() as libc::pid_t;
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 || libc::getppid() != parent
            {
                return Err(io::Error::other(
                    "The player exited before its extractor started",
                ));
            }
            Ok(())
        });
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ResolverResponse {
    Ready { ready: bool },
    Stream { urls: Option<Vec<String>> },
}

const RESOLVER_TIMEOUT: Duration = Duration::from_secs(30);

struct WarmResolver {
    child: Child,
    input: ChildStdin,
    output: ChildStdout,
    bytes: Vec<u8>,
    ready: bool,
    started: Instant,
    _cookies: Option<File>,
}

impl WarmResolver {
    fn start(ytdlp: &Path, credentials: Option<&ExtractorCredentials>) -> Result<Self> {
        let mut command = python_interpreter(ytdlp)?;
        command
            .args(["-I", "-B", "-u", "-c", include_str!("ytdlp_resolver.py")])
            .arg(ytdlp);
        let cookies = extractor_arguments(&mut command, credentials)?;
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0);
        exit_with_parent(&mut command);
        let mut child = command.spawn()?;
        let resolver = Self {
            input: child.stdin.take().unwrap(),
            output: child.stdout.take().unwrap(),
            child,
            bytes: Vec::new(),
            ready: false,
            started: Instant::now(),
            _cookies: cookies,
        };
        let fd = resolver.output.as_raw_fd();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        ensure!(
            flags >= 0 && unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } >= 0,
            "Could not read resolver output asynchronously"
        );
        Ok(resolver)
    }

    fn send(&mut self, target: &PreparedTarget) -> Result<()> {
        serde_json::to_writer(
            &mut self.input,
            &json!({"url": target.key.url, "format": target.key.selector()}),
        )?;
        self.input.write_all(b"\n")?;
        Ok(())
    }

    fn read(&mut self) -> Result<Option<Option<Vec<String>>>> {
        loop {
            if let Some(end) = self.bytes.iter().position(|byte| *byte == b'\n') {
                let line: Vec<_> = self.bytes.drain(..=end).collect();
                match serde_json::from_slice(&line)? {
                    ResolverResponse::Ready { ready: true } if !self.ready => self.ready = true,
                    ResolverResponse::Stream { urls } if self.ready => return Ok(Some(urls)),
                    _ => bail!("Invalid resolver response"),
                }
            } else {
                let mut bytes = [0; 8192];
                match self.output.read(&mut bytes) {
                    Ok(0) => bail!("Resolver closed its output"),
                    Ok(count) => self.bytes.extend_from_slice(&bytes[..count]),
                    Err(error) if error.kind() == ErrorKind::WouldBlock => return Ok(None),
                    Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                    Err(error) => return Err(error.into()),
                }
                ensure!(self.bytes.len() <= 32 * 1024, "Oversized resolver response");
            }
        }
    }
}

impl Drop for WarmResolver {
    fn drop(&mut self) {
        // The group can still contain a JS runtime after Python has exited.
        unsafe { libc::kill(-(self.child.id() as i32), libc::SIGKILL) };
        let _ = self.child.wait();
    }
}

struct NativeResolver {
    lookup: native::Lookup,
    wake: Arc<OwnedFd>,
    sender: Sender<(u64, Result<ResolvedStream>)>,
    receiver: Receiver<(u64, Result<ResolvedStream>)>,
    target: Option<PreparedTarget>,
    generation: u64,
    started: Instant,
}

impl NativeResolver {
    fn new(lookup: native::Lookup, wake: Arc<OwnedFd>) -> Self {
        let (sender, receiver) = mpsc::channel();
        Self {
            lookup,
            wake,
            sender,
            receiver,
            target: None,
            generation: 0,
            started: Instant::now(),
        }
    }

    fn start(&mut self, target: PreparedTarget) -> Result<()> {
        if self
            .target
            .as_ref()
            .is_some_and(|current| current.key == target.key)
        {
            self.target = Some(target);
            return Ok(());
        }
        self.target = None;
        self.generation += 1;
        let generation = self.generation;
        let key = target.key.clone();
        let lookup = self.lookup.clone();
        let sender = self.sender.clone();
        let wake = self.wake.clone();
        thread::Builder::new()
            .name("stream-lookup".into())
            .spawn(move || {
                let result = (|| {
                    let url = reqwest::Url::parse(&key.url)?;
                    let id = url
                        .query_pairs()
                        .find_map(|(name, value)| (name == "v").then(|| value.into_owned()))
                        .context("Missing video ID")?;
                    let urls = lookup.lookup(&id, key.quality, key.video)?;
                    ResolvedStream::parse(key, urls.join("\n").as_bytes(), SystemTime::now())
                })();
                if sender.send((generation, result)).is_ok() {
                    signal_player(&wake);
                }
            })?;
        self.started = Instant::now();
        self.target = Some(target);
        Ok(())
    }

    fn finish(&mut self) -> Option<(PreparedTarget, Result<ResolvedStream>, Instant)> {
        while let Ok((generation, result)) = self.receiver.try_recv() {
            if generation == self.generation
                && let Some(target) = self.target.take()
            {
                return Some((target, result, self.started));
            }
        }
        if self.target.is_some() && self.started.elapsed() >= native::BUDGET {
            self.lookup.stalled();
            return Some((
                self.target.take().unwrap(),
                Err(anyhow::anyhow!("Native lookup timed out")),
                self.started,
            ));
        }
        None
    }
}

struct StreamResolver {
    ytdlp: PathBuf,
    warm: Option<WarmResolver>,
    warm_enabled: bool,
    one_shot: Option<StreamPreparation>,
    target: Option<PreparedTarget>,
    started: Instant,
    native: Option<NativeResolver>,
}

impl StreamResolver {
    fn new(ytdlp: &Path, credentials: Option<&ExtractorCredentials>) -> Self {
        let warm = WarmResolver::start(ytdlp, credentials).ok();
        Self {
            ytdlp: ytdlp.to_owned(),
            warm_enabled: warm.is_some(),
            warm,
            one_shot: None,
            target: None,
            started: Instant::now(),
            native: None,
        }
    }

    fn fd(&self) -> Option<c_int> {
        self.warm
            .as_ref()
            .map(|warm| warm.output.as_raw_fd())
            .or_else(|| self.one_shot.as_ref().map(|job| job.exited.as_raw_fd()))
    }

    fn deadline(&self) -> Option<Instant> {
        let startup = self
            .warm
            .as_ref()
            .filter(|warm| !warm.ready)
            .map(|warm| warm.started + RESOLVER_TIMEOUT);
        let request = self
            .target
            .as_ref()
            .map(|_| self.started + RESOLVER_TIMEOUT);
        let native = self
            .native
            .as_ref()
            .filter(|native| native.target.is_some())
            .map(|native| native.started + native::BUDGET);
        startup.into_iter().chain(request).chain(native).min()
    }

    fn stop(&mut self) {
        if let Some(native) = &mut self.native {
            native.target = None;
        }
        self.warm.take();
        self.one_shot.take();
        self.target = None;
    }

    fn cancel(&mut self) {
        if let Some(native) = &mut self.native {
            native.target = None;
        }
        if self.target.is_some() {
            self.stop();
        }
    }

    fn start(
        &mut self,
        mut target: PreparedTarget,
        credentials: Option<&ExtractorCredentials>,
    ) -> Result<()> {
        if target.lane == ResolverLane::Fast
            && let Some(native) = &mut self.native
        {
            if native.start(target.clone()).is_ok() {
                return Ok(());
            }
            target.lane = ResolverLane::Full;
        }
        if let Some(native) = &mut self.native
            && native
                .target
                .as_ref()
                .is_some_and(|current| current.key == target.key)
        {
            native.target = None;
        }
        // Python only runs full lookups, so its result can satisfy either lane.
        if self
            .target
            .as_ref()
            .is_some_and(|current| current.key == target.key)
        {
            self.target = Some(target);
            return Ok(());
        }
        // Only supersede the full extractor here. Native clicks are independent.
        if self.target.is_some() {
            self.warm.take();
            self.one_shot.take();
            self.target = None;
        }
        if self.warm.is_none() && self.warm_enabled {
            self.warm = WarmResolver::start(&self.ytdlp, credentials).ok();
            self.warm_enabled = self.warm.is_some();
        }
        self.started = Instant::now();
        self.target = Some(target);
        if let Some(warm) = &mut self.warm {
            if warm.send(self.target.as_ref().unwrap()).is_ok() {
                return Ok(());
            }
            self.warm_enabled = warm.ready;
            self.warm.take();
        }
        match StreamPreparation::start(
            self.target.as_ref().unwrap().clone(),
            &self.ytdlp,
            credentials,
        ) {
            Ok(job) => self.one_shot = Some(job),
            Err(error) => {
                self.target = None;
                return Err(error);
            }
        }
        Ok(())
    }

    fn retain(&mut self, play: Option<&PreparedTarget>, next: Option<&PreparedTarget>) {
        let wanted = [play, next];
        if let Some(current) = &mut self.target {
            if let Some(target) = wanted
                .into_iter()
                .flatten()
                .find(|target| target.key == current.key)
            {
                current.track_id = target.track_id;
            } else {
                self.warm.take();
                self.one_shot.take();
                self.target = None;
            }
        }
        if let Some(native) = &mut self.native
            && let Some(current) = &mut native.target
        {
            if let Some(target) = wanted
                .into_iter()
                .flatten()
                .find(|target| target.lane == ResolverLane::Fast && target.key == current.key)
            {
                current.track_id = target.track_id;
            } else {
                native.target = None;
            }
        }
    }

    /// `play` is the song waiting to load. A failed native preparation must not
    /// take the full extractor from it; preparation restarts once it loads.
    fn finish(
        &mut self,
        play: Option<&PreparedTarget>,
        credentials: Option<&ExtractorCredentials>,
    ) -> Option<(PreparedTarget, Result<ResolvedStream>)> {
        if let Some((mut target, result, started)) =
            self.native.as_mut().and_then(NativeResolver::finish)
        {
            if result.is_ok() {
                return Some((target, result));
            }
            let serving_play = play.is_some_and(|play| {
                play.track_id != target.track_id
                    && self
                        .target
                        .as_ref()
                        .is_some_and(|current| current.key == play.key)
            });
            if !serving_play {
                target.lane = ResolverLane::Full;
                if let Err(error) = self.start(target.clone(), credentials) {
                    return Some((target, Err(error)));
                }
                self.started = started;
            }
        }
        let mut result = None;
        if let Some(warm) = &mut self.warm {
            let response = if !warm.ready && warm.started.elapsed() >= RESOLVER_TIMEOUT {
                Err(anyhow::anyhow!("Resolver did not start"))
            } else {
                warm.read()
            };
            match response {
                Ok(Some(urls)) => {
                    if let Some(target) = &self.target {
                        result = Some(urls.context("Could not resolve the stream").and_then(
                            |urls| {
                                ResolvedStream::parse(
                                    target.key.clone(),
                                    urls.join("\n").as_bytes(),
                                    SystemTime::now(),
                                )
                            },
                        ));
                    }
                }
                Err(_) => {
                    // Unsupported installs and servers that never start keep the CLI
                    // path, with the very same account/plugin/cookie arguments. A
                    // server that crashed after starting is respawned on the next request.
                    self.warm_enabled = warm.ready;
                    self.warm.take();
                    if let Some(target) = &self.target {
                        match StreamPreparation::start(target.clone(), &self.ytdlp, credentials) {
                            Ok(job) => {
                                self.started = Instant::now();
                                self.one_shot = Some(job);
                            }
                            Err(error) => result = Some(Err(error)),
                        }
                    }
                }
                Ok(None) => {}
            }
        }
        if result.is_none()
            && let Some(job) = &mut self.one_shot
        {
            match job.finish() {
                Ok(Some(stream)) => result = Some(Ok(stream)),
                Err(error) => result = Some(Err(error)),
                Ok(None) => {}
            }
        }
        if result.is_none() && self.target.is_some() && self.started.elapsed() >= RESOLVER_TIMEOUT {
            self.warm.take();
            result = Some(Err(anyhow::anyhow!("Stream resolution timed out")));
        }
        let result = result?;
        self.one_shot.take();
        Some((self.target.take().unwrap(), result))
    }
}

struct PendingPlay {
    target: PreparedTarget,
    title: String,
    options: PlayOptions,
}

struct StreamPreparation {
    target: PreparedTarget,
    child: Child,
    output: File,
    exited: OwnedFd,
    _cookies: Option<File>,
}

impl StreamPreparation {
    fn start(
        target: PreparedTarget,
        ytdlp: &Path,
        credentials: Option<&ExtractorCredentials>,
    ) -> Result<Self> {
        // Keep signed URLs and any extractor output in anonymous RAM, never disk.
        let output = writable_playback_cookies(sealed_file(b"")?)?;
        let mut command = Command::new(ytdlp);
        let cookies = extractor_arguments(&mut command, credentials)?;
        command
            .arg("--format")
            .arg(target.key.selector())
            .stdin(Stdio::null())
            .stdout(output.try_clone()?)
            .stderr(Stdio::null());
        command.process_group(0).arg(&target.key.url);
        exit_with_parent(&mut command);
        let mut child = command.spawn()?;
        // stdout is a memfd, so watch process exit instead of polling try_wait.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, child.id(), 0) };
        if fd < 0 {
            let error = io::Error::last_os_error();
            unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
            let _ = child.wait();
            return Err(error).context("Could not watch the stream extractor");
        }
        Ok(Self {
            target,
            child,
            output,
            exited: unsafe { OwnedFd::from_raw_fd(fd as _) },
            _cookies: cookies,
        })
    }

    fn finish(&mut self) -> Result<Option<ResolvedStream>> {
        let Some(status) = self.child.try_wait()? else {
            return Ok(None);
        };
        ensure!(status.success(), "Could not prepare the next stream");
        self.output.rewind()?;
        let mut bytes = Vec::new();
        Read::by_ref(&mut self.output)
            .take(16 * 1024 + 1)
            .read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= 16 * 1024, "Oversized prepared stream");
        ResolvedStream::parse(self.target.key.clone(), &bytes, SystemTime::now()).map(Some)
    }
}

impl Drop for StreamPreparation {
    fn drop(&mut self) {
        // yt-dlp can start a JS runtime. Cancel its process group as well.
        unsafe { libc::kill(-(self.child.id() as i32), libc::SIGKILL) };
        let _ = self.child.wait();
    }
}

fn playback_runtime(parent: &Path) -> Result<tempfile::TempDir> {
    tempfile::Builder::new()
        .prefix("yt-music-gpui-")
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir_in(parent)
        .context("Could not create the private playback directory")
}

fn find_ytdlp() -> Result<PathBuf> {
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
        .map(|path| path.join("yt-dlp"))
        .chain(std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/bin/yt-dlp")))
        .find(|path| path.is_file())
        .context("Could not find yt-dlp. Install it, then restart the app.")
}

fn run(
    mut options: PlayerOptions,
    commands: Receiver<PlayerCommand>,
    wake: &Arc<OwnedFd>,
    events: &async_channel::Sender<PlayerEvent>,
    ytdlp: &Path,
    lookup: Option<native::Lookup>,
) -> Result<()> {
    let runtime = playback_runtime(&private_runtime()?)?;
    let socket_path = runtime.path().join("mpv.sock");
    let plugin_dir = write_account_extractor(runtime.path())?;
    let account_supported = account_playback_supported(ytdlp);
    let mut account_warned = false;
    let mut account_notice = None;
    let mut credentials = account_credentials(
        options.credentials.take(),
        account_supported,
        account_warned,
        &mut account_notice,
    )
    .map(|source| prepare_credentials(source, &plugin_dir))
    .transpose()?;
    // Destroy the core before dropping the RAM credentials or private runtime.
    let core = MpvCore::new(&options, &socket_path)?;
    // Cancel/reap the resolver before credentials or the core can drop.
    let mut resolver = StreamResolver::new(ytdlp, credentials.as_ref());
    resolver.native = lookup.map(|lookup| NativeResolver::new(lookup, wake.clone()));
    let mut pending_play: Option<PendingPlay> = None;
    let mut next_target: Option<PreparedTarget> = None;
    let mut prepared: Option<(PreparedTarget, ResolvedStream)> = None;
    // Every actor exit frees the renderer before destroying the core.
    let mut video: Option<AttachedVideo> = None;
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut socket = loop {
        match UnixStream::connect(&socket_path) {
            Ok(socket) => break socket,
            Err(_) if Instant::now() < deadline => {
                ensure!(!core.is_shutdown(), "mpv exited during startup");
                thread::sleep(Duration::from_millis(25));
            }
            Err(error) => return Err(error).context("mpv did not open its control socket"),
        }
    };
    socket.set_write_timeout(Some(Duration::from_secs(2)))?;
    let mut state = PlaybackState::default();
    let mut pending = Vec::new();
    for (id, name) in [
        "time-pos",
        "duration",
        "pause",
        "paused-for-cache",
        "volume",
        "speed",
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
                    let resolved = prepared.take().filter(|(target, stream)| {
                        target.track_id == track_id && stream.matches(&key)
                    });
                    let prefetched = resolved.is_some()
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
                                load_command(&url, &title, options),
                                Some(json!(track_id)),
                            )?;
                            continue;
                        }
                        write_command(&mut socket, json!(["stop"]), None)?;
                        // The append acknowledgement can still be queued when the
                        // user clicks Next. Reuse its resolved URL in that case.
                        if let Some((_, stream)) = resolved {
                            resolver.cancel();
                            write_command(
                                &mut socket,
                                load_command(&stream.url, &title, options),
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
                Ok(PlayerCommand::Seek(request)) => state.queue_seek(request, events),
                Ok(PlayerCommand::Json(command)) => write_command(&mut socket, command, None)?,
                Ok(PlayerCommand::Credentials { next, reply }) => {
                    // Reap even an idle resolver before revoking its cookie copy.
                    resolver.stop();
                    pending_play = None;
                    next_target = None;
                    prepared = None;
                    state.upcoming = None;
                    let next = account_credentials(
                        next,
                        account_supported,
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
                        resolver = StreamResolver::new(ytdlp, credentials.as_ref());
                        resolver.native = native;
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
            if pending_play.as_ref().is_some_and(|play| {
                play.target.track_id == target.track_id && play.target.key == target.key
            }) {
                let play = pending_play.take().unwrap();
                match result {
                    Ok(stream) => {
                        let options = PlayOptions {
                            paused: state.paused,
                            ..play.options
                        };
                        write_command(
                            &mut socket,
                            load_command(&stream.url, &play.title, options),
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
                PlayOptions {
                    quality: target.key.quality,
                    video: target.key.video,
                    ..Default::default()
                },
            );
            command[2] = json!("append");
            state.upcoming = Some(target.track_id);
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
        if prepared.as_ref().is_some_and(|(target, stream)| {
            state.current != Some(target.track_id) && SystemTime::now() >= stream.expires
        }) {
            let (target, _) = prepared.take().unwrap();
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
        // Only queued streams need refreshing; the current stream is already open.
        // Expiry is wall-clock time, which a monotonic wait doesn't follow across
        // clock changes or suspend, so recheck it at least once a minute.
        let expiry = prepared
            .as_ref()
            .filter(|(target, _)| state.current != Some(target.track_id))
            .map(|(_, stream)| {
                Instant::now()
                    + stream
                        .expires
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

#[derive(Clone, Copy)]
struct SeekRequest {
    track_id: u64,
    revision: u64,
    seconds: f64,
}

#[derive(PartialEq)]
enum SeekPhase {
    Seekability,
    Command,
    Acceptance,
    Start,
    Restart,
    PositionRequest,
    Position,
}

struct ActiveSeek {
    request: SeekRequest,
    phase: SeekPhase,
}

#[derive(Default)]
struct PlaybackState {
    file_active: bool,
    desired: Option<u64>,
    upcoming: Option<u64>,
    current: Option<u64>,
    entries: HashMap<u64, u64>,
    loaded: bool,
    seekable: bool,
    paused: bool,
    pause_target: Option<bool>,
    pause_after_load: bool,
    pause_revision: u64,
    restarted: bool,
    seek: Option<ActiveSeek>,
    next_seek: Option<SeekRequest>,
    position_observer: u64,
}

impl PlaybackState {
    fn read_messages(
        &mut self,
        socket: &mut UnixStream,
        pending: &mut Vec<u8>,
        buffer: &mut [u8],
        events: &async_channel::Sender<PlayerEvent>,
    ) -> Result<()> {
        // A synchronous request can leave complete events after its reply.
        if !pending.contains(&b'\n') {
            match socket.read(buffer) {
                Ok(0) => bail!("mpv closed its connection. Restart the app to reconnect."),
                Ok(size) => pending.extend_from_slice(&buffer[..size]),
                Err(error)
                    if matches!(
                        error.kind(),
                        ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                    ) =>
                {
                    return Ok(());
                }
                Err(error) => return Err(error).context("Lost the connection to mpv"),
            }
        }
        while let Some(end) = pending.iter().position(|c| *c == b'\n') {
            let line: Vec<_> = pending.drain(..=end).collect();
            if let Ok(message) = serde_json::from_slice(&line) {
                self.message(message, events);
                if self.loaded && self.pause_after_load {
                    self.pause_after_load = false;
                    if let Some(track_id) = self.current {
                        write_command(
                            socket,
                            json!(["get_property", "seekable"]),
                            Some(json!(format!("loaded-seekable:{track_id}"))),
                        )?;
                        write_command(
                            socket,
                            json!(["get_property", "duration"]),
                            Some(json!(format!("loaded-duration:{track_id}"))),
                        )?;
                    }
                    if let Some(paused) = self.pause_target {
                        self.pause_revision += 1;
                        write_pause(socket, paused, self.pause_revision)?;
                    }
                }
                self.write_seek(socket)?;
            }
        }
        ensure!(
            pending.len() <= 1024 * 1024,
            "mpv returned an oversized response."
        );
        Ok(())
    }

    fn discard_upcoming(&mut self) {
        self.upcoming = None;
        // Next can adopt an entry before mpv delivers start-file. Preparing the
        // following song must preserve that entry's event identity as well.
        self.entries
            .retain(|_, id| Some(*id) == self.current || Some(*id) == self.desired);
    }

    fn invalidate_seeks(&mut self) {
        self.seek = None;
        self.next_seek = None;
        self.restarted = false;
    }

    fn queue_seek(&mut self, request: SeekRequest, events: &async_channel::Sender<PlayerEvent>) {
        if self.loaded && self.current == Some(request.track_id) && self.current == self.desired {
            self.next_seek = Some(request);
        } else {
            Self::seek_finished(
                request,
                Err("This track is no longer ready to seek.".into()),
                events,
            );
        }
    }

    fn seek_finished(
        request: SeekRequest,
        result: Result<f64, String>,
        events: &async_channel::Sender<PlayerEvent>,
    ) {
        let _ = events.send_blocking(PlayerEvent::SeekFinished {
            track_id: request.track_id,
            revision: request.revision,
            result,
        });
    }

    fn fail_seeks(&mut self, events: &async_channel::Sender<PlayerEvent>) {
        for request in [
            self.seek.take().map(|seek| seek.request),
            self.next_seek.take(),
        ]
        .into_iter()
        .flatten()
        {
            Self::seek_finished(
                request,
                Err("Playback ended before the seek completed.".into()),
                events,
            );
        }
        self.restarted = false;
    }

    fn write_seek(&mut self, socket: &mut UnixStream) -> Result<()> {
        if self.seek.is_none()
            && self.restarted
            && let Some(request) = self.next_seek.take()
        {
            write_command(
                socket,
                json!(["get_property", "seekable"]),
                Some(json!(format!("seekable:{}", request.revision))),
            )?;
            self.seek = Some(ActiveSeek {
                request,
                phase: SeekPhase::Seekability,
            });
        }
        if let Some(seek) = &mut self.seek
            && seek.phase == SeekPhase::Command
        {
            write_command(
                socket,
                json!(["seek", seek.request.seconds, "absolute"]),
                Some(json!(seek.request.revision)),
            )?;
            seek.phase = SeekPhase::Acceptance;
        }
        if let Some(seek) = &mut self.seek
            && seek.phase == SeekPhase::PositionRequest
        {
            // A fresh observer fences out cached position notifications from before the seek.
            let old_observer = self.position_observer.max(1);
            self.position_observer = old_observer + 6;
            write_command(socket, json!(["unobserve_property", old_observer]), None)?;
            write_command(
                socket,
                json!(["observe_property", self.position_observer, "time-pos"]),
                Some(json!(format!("seek-position:{}", seek.request.revision))),
            )?;
            seek.phase = SeekPhase::Position;
        }
        Ok(())
    }

    fn message(&mut self, value: Value, events: &async_channel::Sender<PlayerEvent>) {
        if let Some(track_id) = value["request_id"]
            .as_str()
            .and_then(|id| id.strip_prefix("prepare:"))
            .and_then(|id| id.parse::<u64>().ok())
        {
            if self.upcoming == Some(track_id)
                && let Some(entry) = value
                    .pointer("/data/playlist_entry_id")
                    .and_then(Value::as_u64)
            {
                self.entries.insert(entry, track_id);
            }
            return;
        }
        if let Some(track_id) = value["request_id"]
            .as_str()
            .and_then(|id| id.strip_prefix("loaded-seekable:"))
            .and_then(|id| id.parse::<u64>().ok())
        {
            if self.loaded && self.current == Some(track_id) && self.desired == Some(track_id) {
                self.seekable =
                    value["error"] == "success" && value["data"].as_bool().unwrap_or(false);
                let _ = events.send_blocking(PlayerEvent::Seekable {
                    track_id,
                    seekable: self.seekable,
                });
            }
            return;
        }
        if let Some(track_id) = value["request_id"]
            .as_str()
            .and_then(|id| id.strip_prefix("loaded-duration:"))
            .and_then(|id| id.parse::<u64>().ok())
        {
            if self.loaded
                && self.current == Some(track_id)
                && self.desired == Some(track_id)
                && value["error"] == "success"
                && let Some(seconds) = value["data"].as_f64().filter(|seconds| seconds.is_finite())
            {
                let _ = events.send_blocking(PlayerEvent::Duration { track_id, seconds });
            }
            return;
        }
        if let Some(revision) = value["request_id"]
            .as_str()
            .and_then(|id| id.strip_prefix("seekable:"))
            .and_then(|revision| revision.parse::<u64>().ok())
        {
            if self.seek.as_ref().is_some_and(|seek| {
                seek.request.revision == revision && seek.phase == SeekPhase::Seekability
            }) {
                if value["error"] == "success" && value["data"] == true {
                    self.seek.as_mut().unwrap().phase = SeekPhase::Command;
                } else {
                    // mpv accepts a seek before demuxing it, and emits no completion if it cannot seek.
                    let seek = self.seek.take().unwrap();
                    Self::seek_finished(
                        seek.request,
                        Err("This stream does not support seeking.".into()),
                        events,
                    );
                }
            }
            return;
        }
        if let Some(revision) = value["request_id"]
            .as_str()
            .and_then(|id| id.strip_prefix("seek-position:"))
            .and_then(|revision| revision.parse::<u64>().ok())
        {
            if self.seek.as_ref().is_some_and(|seek| {
                seek.request.revision == revision && seek.phase == SeekPhase::Position
            }) && value["error"] != "success"
            {
                let seek = self.seek.take().unwrap();
                Self::seek_finished(
                    seek.request,
                    Err("mpv could not observe the seek position.".into()),
                    events,
                );
            }
            return;
        }
        if let Some(revision) = value["request_id"]
            .as_str()
            .and_then(|id| id.strip_prefix("pause:"))
            .and_then(|revision| revision.parse::<u64>().ok())
        {
            if revision == self.pause_revision
                && self.loaded
                && !self.pause_after_load
                && value["error"] == "success"
                && let Some(paused) = value["data"].as_bool()
                && self.pause_target.is_some()
            {
                self.pause_target = None;
                self.paused = paused;
                if let Some(track_id) = self.current.filter(|id| Some(*id) == self.desired) {
                    let _ = events.send_blocking(PlayerEvent::Paused { track_id, paused });
                }
            }
            return;
        }
        if let Some(id) = value["request_id"].as_u64().filter(|id| *id > 0) {
            if self.seek.as_ref().is_some_and(|seek| {
                seek.request.revision == id && seek.phase == SeekPhase::Acceptance
            }) {
                if value["error"] == "success" {
                    self.seek.as_mut().unwrap().phase = SeekPhase::Start;
                } else {
                    let seek = self.seek.take().unwrap();
                    Self::seek_finished(
                        seek.request,
                        Err("mpv could not seek in this track.".into()),
                        events,
                    );
                }
                return;
            }
            if let Some(entry) = value
                .pointer("/data/playlist_entry_id")
                .and_then(Value::as_u64)
            {
                self.entries.insert(entry, id);
            } else if value["error"].as_str() != Some("success") && self.desired == Some(id) {
                let _ = events.send_blocking(PlayerEvent::Error {
                    track_id: Some(id),
                    message: "mpv could not load this track. Try another track or update yt-dlp."
                        .into(),
                });
            }
        }
        match value["event"].as_str() {
            Some("seek") if self.loaded && self.current == self.desired => {
                self.restarted = false;
                if let Some(seek) = &mut self.seek
                    && seek.phase == SeekPhase::Start
                {
                    seek.phase = SeekPhase::Restart;
                }
            }
            Some("playback-restart") if self.loaded && self.current == self.desired => {
                self.restarted = true;
                if let Some(seek) = &mut self.seek
                    && seek.phase == SeekPhase::Restart
                {
                    seek.phase = SeekPhase::PositionRequest;
                }
            }
            Some("start-file") => {
                self.file_active = true;
                self.seekable = false;
                self.current = value["playlist_entry_id"]
                    .as_u64()
                    .and_then(|id| self.entries.get(&id).copied());
                if self.current.is_some() && self.current == self.upcoming {
                    self.desired = self.current;
                    self.upcoming = None;
                    self.loaded = false;
                    self.paused = false;
                    self.pause_target = Some(false);
                    self.pause_after_load = true;
                    self.invalidate_seeks();
                }
            }
            Some("file-loaded") if self.current.is_some() && self.current == self.desired => {
                self.loaded = true;
                let _ = events.send_blocking(PlayerEvent::Started {
                    track_id: self.current.unwrap(),
                    paused: self.paused,
                });
                if self.seekable {
                    let _ = events.send_blocking(PlayerEvent::Seekable {
                        track_id: self.current.unwrap(),
                        seekable: true,
                    });
                }
            }
            Some("end-file") => {
                self.file_active = false;
                let track_id = value["playlist_entry_id"]
                    .as_u64()
                    .and_then(|id| self.entries.remove(&id));
                if let Some(track_id) = track_id.filter(|id| Some(*id) == self.desired) {
                    self.fail_seeks(events);
                    match value["reason"].as_str() {
                        Some("eof") => {
                            let _ = events.send_blocking(PlayerEvent::Ended { track_id });
                        }
                        Some("stop") => {
                            self.desired = None;
                            let _ = events.send_blocking(PlayerEvent::Stopped { track_id });
                        }
                        Some("error") => {
                            let _ = events.send_blocking(PlayerEvent::Error {
                                track_id: Some(track_id),
                                message: "YouTube could not provide this stream. Try another track or update yt-dlp."
                                    .into(),
                            });
                        }
                        _ => {}
                    }
                    self.loaded = false;
                }
            }
            Some("property-change") => {
                if value["name"] == "time-pos"
                    && value["id"].as_u64() == Some(self.position_observer.max(1))
                    && self
                        .seek
                        .as_ref()
                        .is_some_and(|seek| seek.phase == SeekPhase::Position)
                {
                    // observe_property guarantees an initial notification, even for a no-op seek.
                    let seek = self.seek.take().unwrap();
                    let result = value["data"]
                        .as_f64()
                        .filter(|seconds| seconds.is_finite() && *seconds >= 0.)
                        .ok_or_else(|| "mpv could not confirm the seek position.".into());
                    Self::seek_finished(seek.request, result, events);
                    return;
                }
                if value["name"] == "volume" {
                    if let Some(volume) = value["data"].as_f64() {
                        let _ = events.send_blocking(PlayerEvent::Volume(volume));
                    }
                    return;
                }
                if value["name"] == "speed" {
                    if let Some(speed) = value["data"]
                        .as_f64()
                        .and_then(|value| PlaybackSpeed::try_from(value).ok())
                    {
                        let _ = events.send_blocking(PlayerEvent::Speed(speed));
                    }
                    return;
                }
                if value["name"] == "seekable"
                    && self.current.is_some()
                    && self.current == self.desired
                {
                    self.seekable = value["data"].as_bool().unwrap_or(false);
                }
                if value["name"] == "pause"
                    && self.current.is_some()
                    && self.current == self.desired
                    && let Some(paused) = value["data"].as_bool()
                {
                    if self.pause_target.is_some_and(|target| target != paused) {
                        return;
                    }
                    self.paused = paused;
                }
                let Some(track_id) = self
                    .current
                    .filter(|id| self.loaded && Some(*id) == self.desired)
                else {
                    return;
                };
                let event = match value["name"].as_str() {
                    Some("time-pos")
                        if self.seek.is_none()
                            && self.next_seek.is_none()
                            && value["id"].as_u64() == Some(self.position_observer.max(1)) =>
                    {
                        value["data"]
                            .as_f64()
                            .filter(|seconds| seconds.is_finite() && *seconds >= 0.)
                            .map(|seconds| PlayerEvent::Position { track_id, seconds })
                    }
                    Some("duration") => value["data"]
                        .as_f64()
                        .map(|seconds| PlayerEvent::Duration { track_id, seconds }),
                    Some("seekable") => Some(PlayerEvent::Seekable {
                        track_id,
                        seekable: self.seekable,
                    }),
                    Some("pause") => value["data"]
                        .as_bool()
                        .map(|paused| PlayerEvent::Paused { track_id, paused }),
                    Some("paused-for-cache") => {
                        value["data"]
                            .as_bool()
                            .map(|buffering| PlayerEvent::Buffering {
                                track_id,
                                buffering,
                            })
                    }
                    _ => None,
                };
                if let Some(event) = event {
                    let _ = events.send_blocking(event);
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn mismatched_extractor_uses_guest_streams_and_notifies_only_once() {
        let mut notice = None;
        for _ in 0..2 {
            assert!(
                account_credentials(
                    Some(synthetic_credentials("INVENTED")),
                    false,
                    false,
                    &mut notice
                )
                .is_none()
            );
        }
        let message = notice
            .take()
            .expect("fallback notice must wait for playback");
        assert!(message.contains("2026.08.19"));
        assert!(message.contains("guest"));
        assert!(!message.contains("update yt-dlp"));
        assert!(
            account_credentials(
                Some(synthetic_credentials("INVENTED")),
                false,
                true,
                &mut notice
            )
            .is_none()
        );
        assert!(
            notice.is_none(),
            "a displayed fallback notice must not repeat"
        );
        assert!(
            account_credentials(
                Some(synthetic_credentials("INVENTED")),
                true,
                false,
                &mut notice
            )
            .is_some()
        );
        account_credentials(
            Some(synthetic_credentials("INVENTED")),
            false,
            false,
            &mut notice,
        );
        account_credentials(None, false, false, &mut notice);
        assert!(
            notice.is_none(),
            "sign-out must cancel an undisplayed account notice"
        );
    }

    fn resolver_target(id: &str, track_id: u64) -> PreparedTarget {
        PreparedTarget {
            lane: ResolverLane::Full,
            key: StreamKey {
                url: format!("https://music.youtube.com/watch?v={id}"),
                quality: AudioQuality::Normal,
                video: None,
            },
            track_id,
        }
    }

    #[test]
    fn resolver_lanes_keep_clicks_fast_and_prepare_premium_audio() {
        for quality in AudioQuality::ALL {
            for video in [None].into_iter().chain(VideoQuality::ALL.map(Some)) {
                let key = StreamKey {
                    quality,
                    video,
                    ..resolver_target("abcdefghijk", 1).key
                };
                let above_1080 = matches!(video, Some(VideoQuality::P1440 | VideoQuality::P2160));
                for signed_in in [false, true] {
                    assert_eq!(
                        key.lane(false, signed_in),
                        if above_1080 {
                            ResolverLane::Full
                        } else {
                            ResolverLane::Fast
                        }
                    );
                    assert_eq!(
                        key.lane(true, signed_in),
                        if above_1080 || (signed_in && quality == AudioQuality::High) {
                            ResolverLane::Full
                        } else {
                            ResolverLane::Fast
                        }
                    );
                }
            }
        }
    }

    fn synthetic_resolver_plugin(directory: &Path) -> PathBuf {
        let plugin = write_account_extractor(directory).unwrap();
        let path = plugin.join("account/yt_dlp_plugins/extractor/account.py");
        let mut file = fs::OpenOptions::new().append(true).open(path).unwrap();
        // The actual account plugin is discovered normally. Replace only remote
        // extraction for these process-lifetime tests; Python tests cover identity checks.
        writeln!(file, r#"
import signal, subprocess, sys
from pathlib import Path

def initialize(self):
    self._account

def extract(self, url):
    self.calls = getattr(self, 'calls', 0) + 1
    if url.endswith('blocked0000'):
        child = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(30)'])
        Path({ready:?}).write_text(json.dumps({{'pid': os.getpid(), 'child': child.pid}}))
        os.kill(os.getpid(), signal.SIGSTOP)
    return {{'id': 'abcdefghijk', 'title': 'Synthetic', 'formats': [{{
        'format_id': 'audio', 'ext': 'webm', 'vcodec': 'none', 'acodec': 'opus',
        'url': 'https://rr1.googlevideo.com/audio?call=' + str(self.calls) + '&identity=' + self._account['data_sync_id']}}]}}

YtMusicAccountIE._real_initialize = initialize
YtMusicAccountIE._real_extract = extract
"#, ready=directory.join("ready").to_str().unwrap()).unwrap();
        plugin
    }

    fn resolved(
        resolver: &mut StreamResolver,
        credentials: Option<&ExtractorCredentials>,
    ) -> ResolvedStream {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some((_, result)) = resolver.finish(None, credentials) {
                return result.unwrap();
            }
            assert!(Instant::now() < deadline, "resolver did not answer");
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    #[ignore = "requires Python with yt-dlp 2026.08.19; synthetic account and process group, no network"]
    fn warm_resolver_reuses_requests_and_supersession_kills_the_process_group() {
        let directory = tempfile::tempdir().unwrap();
        let plugin = synthetic_resolver_plugin(directory.path());
        let credentials =
            prepare_credentials(synthetic_credentials("INVENTED_FIRST"), &plugin).unwrap();
        let ytdlp = find_ytdlp().unwrap();
        let mut resolver = StreamResolver::new(&ytdlp, Some(&credentials));
        let old = resolver.warm.as_ref().unwrap();
        let pid = old.child.id();
        let cookie = format!(
            "/proc/self/fd/{}",
            old._cookies.as_ref().unwrap().as_raw_fd()
        );
        let cookie_identity = file_identity(&cookie);
        let blocked = resolver_target("blocked0000", 1);
        resolver.start(blocked.clone(), Some(&credentials)).unwrap();
        let ready = directory.path().join("ready");
        let deadline = Instant::now() + Duration::from_secs(10);
        while !ready.exists() {
            assert!(
                Instant::now() < deadline,
                "synthetic extractor did not start"
            );
            thread::sleep(Duration::from_millis(5));
        }
        let child = serde_json::from_slice::<Value>(&fs::read(ready).unwrap()).unwrap()["child"]
            .as_u64()
            .unwrap();
        resolver
            .start(
                PreparedTarget {
                    track_id: 2,
                    ..blocked
                },
                Some(&credentials),
            )
            .unwrap();
        assert_eq!(
            resolver.warm.as_ref().unwrap().child.id(),
            pid,
            "Play of the preparing track must keep its request"
        );
        resolver
            .start(resolver_target("abcdefghijk", 3), Some(&credentials))
            .unwrap();
        assert!(
            !Path::new(&format!("/proc/{pid}")).exists(),
            "superseded resolver must be reaped synchronously"
        );
        assert_ne!(file_identity(&cookie), cookie_identity);
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let stat = fs::read_to_string(format!("/proc/{child}/stat")).unwrap_or_default();
            if stat.is_empty() || stat.split_once(") ").unwrap().1.starts_with('Z') {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "JS runtime survived process-group cancellation"
            );
            thread::sleep(Duration::from_millis(5));
        }
        let new_pid = resolver.warm.as_ref().unwrap().child.id();
        assert_ne!(pid, new_pid);
        assert!(
            resolved(&mut resolver, Some(&credentials))
                .url
                .contains("call=1")
        );
        resolver
            .start(resolver_target("dQw4w9WgXcQ", 4), Some(&credentials))
            .unwrap();
        assert!(
            resolved(&mut resolver, Some(&credentials))
                .url
                .contains("call=2")
        );
        assert_eq!(resolver.warm.as_ref().unwrap().child.id(), new_pid);
        // A crash after startup falls back once, then the next request is warm again.
        unsafe { libc::kill(new_pid as i32, libc::SIGKILL) };
        resolver
            .start(resolver_target("abcdefghijk", 5), Some(&credentials))
            .unwrap();
        assert!(
            resolved(&mut resolver, Some(&credentials))
                .url
                .contains("call=1")
        );
        resolver
            .start(resolver_target("dQw4w9WgXcQ", 6), Some(&credentials))
            .unwrap();
        let respawned = resolver
            .warm
            .as_ref()
            .expect("warm resolver respawned")
            .child
            .id();
        assert_ne!(respawned, new_pid);
        resolved(&mut resolver, Some(&credentials));
        drop(resolver);
        assert!(!Path::new(&format!("/proc/{respawned}")).exists());
    }

    #[test]
    #[ignore = "requires Python with yt-dlp 2026.08.19; synthetic accounts, no network"]
    fn warm_credential_change_respawns_with_new_identity_and_revokes_old_files() {
        let directory = tempfile::tempdir().unwrap();
        let plugin = synthetic_resolver_plugin(directory.path());
        let credentials =
            prepare_credentials(synthetic_credentials("INVENTED_FIRST"), &plugin).unwrap();
        let ytdlp = find_ytdlp().unwrap();
        let mut resolver = StreamResolver::new(&ytdlp, Some(&credentials));
        resolver
            .start(resolver_target("abcdefghijk", 1), Some(&credentials))
            .unwrap();
        assert!(
            resolved(&mut resolver, Some(&credentials))
                .url
                .contains("identity=brand||user")
        );
        let old = resolver.warm.as_ref().unwrap();
        let pid = old.child.id();
        let paths = [
            old._cookies.as_ref().unwrap().as_raw_fd(),
            credentials._identity.as_raw_fd(),
            credentials.config.as_raw_fd(),
        ]
        .map(|fd| {
            let path = format!("/proc/self/fd/{fd}");
            let identity = file_identity(&path);
            (path, identity)
        });
        resolver.stop();
        drop(credentials);
        for (path, identity) in paths {
            assert_ne!(file_identity(&path), identity);
        }
        assert!(!Path::new(&format!("/proc/{pid}")).exists());
        let mut source = synthetic_credentials("INVENTED_SECOND");
        source.identity = sealed_file(
            br#"{"authuser":"3","delegated_session_id":"other","data_sync_id":"other||second"}"#,
        )
        .unwrap();
        let credentials = prepare_credentials(source, &plugin).unwrap();
        resolver = StreamResolver::new(&ytdlp, Some(&credentials));
        resolver
            .start(resolver_target("abcdefghijk", 2), Some(&credentials))
            .unwrap();
        assert!(
            resolved(&mut resolver, Some(&credentials))
                .url
                .contains("identity=other||second")
        );
    }

    #[test]
    fn resolver_output_wakes_the_actor_without_mpv_or_commands() {
        let (socket, _mpv) = UnixStream::pair().unwrap();
        let (resolver, mut output) = UnixStream::pair().unwrap();
        let (commands, _received) = command_channel().unwrap();
        output.write_all(b"{\"ready\":true}\n").unwrap();
        let start = Instant::now();
        assert!(
            !wait_for_player_input(&socket, &commands.wake, Some(resolver.as_raw_fd()), None)
                .unwrap()
        );
        assert!(
            start.elapsed() < Duration::from_millis(60),
            "resolver output waited for the actor's timer"
        );
    }

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
        let mut resolver = StreamResolver::new(&extractor, None);
        resolver
            .start(resolver_target("abcdefghijk", 1), None)
            .unwrap();
        let (socket, _mpv) = UnixStream::pair().unwrap();
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

    fn synthetic_full_resolver(directory: &Path, blocked: bool) -> StreamResolver {
        let interpreter = directory.join("python3");
        let response = if blocked {
            format!(
                "echo started > {}; exec sleep 60",
                directory.join("started").display()
            )
        } else {
            "printf '%s\\n' '{\"urls\":[\"https://rr1.googlevideo.com/full\"]}'".into()
        };
        fs::write(&interpreter, format!("#!/bin/sh\nprintf '%s\\n' '{{\"ready\":true}}'\nwhile IFS= read -r request; do {response}; done\n")).unwrap();
        fs::set_permissions(&interpreter, fs::Permissions::from_mode(0o700)).unwrap();
        let extractor = directory.join("extractor");
        fs::write(&extractor, format!("#!{}\n", interpreter.display())).unwrap();
        StreamResolver::new(&extractor, None)
    }

    #[test]
    fn native_click_during_full_preparation_does_not_wait_or_restart_python() {
        let directory = tempfile::tempdir().unwrap();
        let mut resolver = synthetic_full_resolver(directory.path(), true);
        let (commands, _received) = command_channel().unwrap();
        let (lookup, server) = native::tests::server(vec![
            native::tests::homepage(),
            native::tests::fixture().to_string(),
        ]);
        resolver.native = Some(NativeResolver::new(lookup, commands.wake.clone()));
        let preparation = resolver_target("blocked0000", 1);
        resolver.start(preparation.clone(), None).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !directory.path().join("started").exists() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
        let pid = resolver.warm.as_ref().unwrap().child.id();
        let mut click = resolver_target("abcdefghijk", 2);
        click.lane = ResolverLane::Fast;
        let start = Instant::now();
        resolver.start(click.clone(), None).unwrap();
        resolver.retain(Some(&click), Some(&preparation));
        let (socket, _mpv) = UnixStream::pair().unwrap();
        let (target, stream) = loop {
            if let Some(result) = resolver.finish(None, None) {
                break result;
            }
            assert!(start.elapsed() < Duration::from_secs(2));
            wait_for_player_input(
                &socket,
                &commands.wake,
                resolver.fd(),
                Some(start + Duration::from_secs(2)),
            )
            .unwrap();
        };
        assert_eq!(target.track_id, 2);
        assert!(stream.unwrap().url.contains("itag=251"));
        assert_eq!(resolver.warm.as_ref().unwrap().child.id(), pid);
        assert_eq!(resolver.target.as_ref().unwrap().track_id, 1);
        assert!(Path::new(&format!("/proc/{pid}")).exists());
        server.join().unwrap();
    }

    #[test]
    fn a_failed_native_preparation_leaves_the_full_reload_running() {
        let directory = tempfile::tempdir().unwrap();
        let mut resolver = synthetic_full_resolver(directory.path(), true);
        let (commands, _received) = command_channel().unwrap();
        let (lookup, server) =
            native::tests::server(vec![native::tests::homepage(), "invalid json".into()]);
        resolver.native = Some(NativeResolver::new(lookup, commands.wake.clone()));
        // The next song starts natively, then the current song's automatic
        // reload takes the full extractor while that preparation is in flight.
        let mut preparation = resolver_target("abcdefghijk", 2);
        preparation.lane = ResolverLane::Fast;
        resolver.start(preparation.clone(), None).unwrap();
        let reload = resolver_target("blocked0000", 1);
        resolver.start(reload.clone(), None).unwrap();
        let pid = resolver.warm.as_ref().unwrap().child.id();
        resolver.retain(Some(&reload), Some(&preparation));
        let (socket, _mpv) = UnixStream::pair().unwrap();
        let start = Instant::now();
        loop {
            assert!(resolver.finish(Some(&reload), None).is_none());
            if resolver.native.as_ref().unwrap().target.is_none() {
                break;
            }
            assert!(start.elapsed() < Duration::from_secs(2));
            wait_for_player_input(
                &socket,
                &commands.wake,
                resolver.fd(),
                Some(start + Duration::from_secs(2)),
            )
            .unwrap();
        }
        assert_eq!(resolver.warm.as_ref().unwrap().child.id(), pid);
        assert_eq!(resolver.target.as_ref().unwrap().track_id, 1);
        assert!(resolver.deadline().is_some());
        server.join().unwrap();
    }

    #[test]
    fn native_failures_fall_back_to_the_same_full_resolver() {
        let mut cipher = native::tests::fixture();
        for field in ["formats", "adaptiveFormats"] {
            for format in cipher["streamingData"][field].as_array_mut().unwrap() {
                format.as_object_mut().unwrap().remove("url");
                format["signatureCipher"] = json!("s=encrypted");
            }
        }
        for response in ["invalid json".into(),json!({"videoDetails":{"videoId":"abcdefghijk"},"playabilityStatus":{"status":"UNPLAYABLE"}}).to_string(),cipher.to_string()] {
            let directory = tempfile::tempdir().unwrap();
            let mut resolver = synthetic_full_resolver(directory.path(),false);
            let pid = resolver.warm.as_ref().unwrap().child.id();
            let (commands,_received) = command_channel().unwrap();
            let (lookup,server) = native::tests::server(vec![native::tests::homepage(),response]);
            resolver.native = Some(NativeResolver::new(lookup,commands.wake.clone()));
            let mut target = resolver_target("abcdefghijk",7);
            target.lane = ResolverLane::Fast;
            resolver.start(target,None).unwrap();
            let stream = resolved(&mut resolver,None);
            assert!(stream.url.ends_with("/full"));
            assert_eq!(resolver.warm.as_ref().unwrap().child.id(),pid);
            server.join().unwrap();
        }
    }

    #[test]
    fn a_fast_lookup_in_flight_never_answers_a_full_request() {
        let directory = tempfile::tempdir().unwrap();
        let mut resolver = synthetic_full_resolver(directory.path(), true);
        let (commands, _received) = command_channel().unwrap();
        let mut native = NativeResolver::new(native::Lookup::new().unwrap(), commands.wake);
        let mut target = resolver_target("abcdefghijk", 1);
        target.lane = ResolverLane::Fast;
        native.target = Some(target.clone());
        let stream = ResolvedStream::parse(
            target.key.clone(),
            b"https://rr1.googlevideo.com/obsolete",
            SystemTime::now(),
        )
        .unwrap();
        native.sender.send((native.generation, Ok(stream))).unwrap();
        resolver.native = Some(native);
        target.lane = ResolverLane::Full;
        resolver.start(target, None).unwrap();
        assert!(resolver.finish(None, None).is_none());
        assert!(resolver.native.as_ref().unwrap().target.is_none());
        assert_eq!(resolver.target.as_ref().unwrap().lane, ResolverLane::Full);
    }

    #[test]
    fn resolver_startup_and_request_deadlines_wake_without_output() {
        let directory = tempfile::tempdir().unwrap();
        let interpreter = directory.path().join("python3");
        fs::write(&interpreter, "#!/bin/sh\nexec sleep 60\n").unwrap();
        fs::set_permissions(&interpreter, fs::Permissions::from_mode(0o700)).unwrap();
        let extractor = directory.path().join("extractor");
        fs::write(&extractor, format!("#!{}\n", interpreter.display())).unwrap();
        let (socket, _mpv) = UnixStream::pair().unwrap();
        let (commands, _received) = command_channel().unwrap();
        for ready in [false, true] {
            let mut resolver = StreamResolver::new(&extractor, None);
            assert!(resolver.warm.is_some());
            let started = Instant::now() - Duration::from_secs(30);
            let warm = resolver.warm.as_mut().unwrap();
            warm.ready = ready;
            warm.started = started;
            if ready {
                resolver.started = started;
                resolver.target = Some(resolver_target("abcdefghijk", 1));
            }
            let deadline = resolver.deadline().unwrap();
            assert!(deadline <= Instant::now());
            assert!(
                !wait_for_player_input(&socket, &commands.wake, resolver.fd(), Some(deadline))
                    .unwrap()
            );
            let result = resolver.finish(None, None);
            if ready {
                assert!(
                    result
                        .unwrap()
                        .1
                        .err()
                        .unwrap()
                        .to_string()
                        .contains("timed out")
                );
            } else {
                assert!(result.is_none());
            }
            assert!(resolver.warm.is_none());
            assert!(resolver.deadline().is_none());
        }
    }

    #[test]
    fn unsupported_interpreters_and_failed_warm_start_use_one_shot_resolution() {
        for fails_start in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let ytdlp = directory.path().join("yt-dlp");
            let interpreter = if fails_start {
                let path = directory.path().join("python3");
                fs::write(
                    &path,
                    "#!/bin/sh\nif [ \"$1\" = -I ]; then exit 23; fi\nexec /bin/sh \"$@\"\n",
                )
                .unwrap();
                fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
                path
            } else {
                PathBuf::from("/bin/sh")
            };
            fs::write(
                &ytdlp,
                format!(
                    "#!{}\nprintf '%s\\n' 'https://rr1.googlevideo.com/audio?mode=oneshot'\n",
                    interpreter.display()
                ),
            )
            .unwrap();
            fs::set_permissions(&ytdlp, fs::Permissions::from_mode(0o700)).unwrap();
            let mut resolver = StreamResolver::new(&ytdlp, None);
            resolver
                .start(resolver_target("abcdefghijk", 1), None)
                .unwrap();
            assert!(resolved(&mut resolver, None).url.ends_with("mode=oneshot"));
            assert!(resolver.warm.is_none() && !resolver.warm_enabled);
            resolver
                .start(resolver_target("dQw4w9WgXcQ", 2), None)
                .unwrap();
            assert!(resolved(&mut resolver, None).url.ends_with("mode=oneshot"));
        }
    }

    #[test]
    fn cancelling_preparation_reaps_the_extractor_and_revokes_its_cookie_copy() {
        let directory = tempfile::tempdir().unwrap();
        let extractor = directory.path().join("extractor");
        fs::write(
            &extractor,
            "#!/usr/bin/python3\nimport time\ntime.sleep(30)\n",
        )
        .unwrap();
        fs::set_permissions(&extractor, fs::Permissions::from_mode(0o700)).unwrap();
        let credentials =
            prepare_credentials(synthetic_credentials("INVENTED"), directory.path()).unwrap();
        let target = PreparedTarget {
            lane: ResolverLane::Full,
            key: StreamKey {
                url: "https://music.youtube.com/watch?v=abcdefghijk".into(),
                quality: AudioQuality::Normal,
                video: None,
            },
            track_id: 1,
        };
        let job = StreamPreparation::start(target, &extractor, Some(&credentials)).unwrap();
        let pid = job.child.id();
        let copy = format!(
            "/proc/{}/fd/{}",
            std::process::id(),
            job._cookies.as_ref().unwrap().as_raw_fd()
        );
        let original = format!(
            "/proc/{}/fd/{}",
            std::process::id(),
            credentials.cookies.as_raw_fd()
        );
        let copied = file_identity(&copy);
        assert_ne!(copied, file_identity(&original));
        assert!(fs::read_to_string(&copy).unwrap().contains("INVENTED"));
        drop(job);
        assert!(!Path::new(&format!("/proc/{pid}")).exists());
        // Parallel tests can reuse the closed descriptor number for an unrelated file.
        assert_ne!(file_identity(&copy), copied);
        assert!(Path::new(&original).exists());
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
    fn preparing_another_song_during_adoption_keeps_start_and_position_events() {
        let (events, received) = async_channel::unbounded();
        // Next has adopted entry 11, but mpv has not delivered its start-file yet.
        let mut state = PlaybackState {
            desired: Some(2),
            current: Some(1),
            entries: HashMap::from([(10, 1), (11, 2), (12, 3)]),
            ..Default::default()
        };
        // The app's next render immediately prepares the following queue entry.
        state.discard_upcoming();
        state.message(
            json!({"event":"end-file","playlist_entry_id":10,"reason":"stop"}),
            &events,
        );
        state.message(
            json!({"event":"start-file","playlist_entry_id":11}),
            &events,
        );
        state.message(json!({"event":"file-loaded"}), &events);
        state.message(
            json!({"event":"property-change","name":"time-pos","id":1,"data":0.25}),
            &events,
        );
        assert!(matches!(
            received.try_recv(),
            Ok(PlayerEvent::Started { track_id: 2, .. })
        ));
        assert!(matches!(
            received.try_recv(),
            Ok(PlayerEvent::Position {
                track_id: 2,
                seconds: 0.25
            })
        ));
        assert!(
            !state.entries.contains_key(&12),
            "cancelled preparations must still be discarded"
        );
    }

    #[test]
    fn natural_prefetched_transition_reports_the_old_end_and_new_start_in_order() {
        let (events, received) = async_channel::unbounded();
        let mut state = PlaybackState {
            desired: Some(1),
            current: Some(1),
            loaded: true,
            upcoming: Some(2),
            entries: HashMap::from([(10, 1)]),
            ..Default::default()
        };
        state.message(
            json!({"request_id":"prepare:2","error":"success","data":{"playlist_entry_id":11}}),
            &events,
        );
        state.message(
            json!({"event":"end-file","playlist_entry_id":10,"reason":"eof"}),
            &events,
        );
        state.message(
            json!({"event":"start-file","playlist_entry_id":11}),
            &events,
        );
        state.message(json!({"event":"file-loaded"}), &events);
        assert!(matches!(
            received.try_recv().unwrap(),
            PlayerEvent::Ended { track_id: 1 }
        ));
        assert!(matches!(
            received.try_recv().unwrap(),
            PlayerEvent::Started { track_id: 2, .. }
        ));
        assert_eq!(state.desired, Some(2));
        assert!(state.loaded);
        // A replaced preparation cannot reinsert a removed playlist entry.
        state.message(
            json!({"request_id":"prepare:3","error":"success","data":{"playlist_entry_id":12}}),
            &events,
        );
        assert!(!state.entries.contains_key(&12));
    }

    #[test]
    fn prepared_streams_expire_and_are_bound_to_the_recording_and_quality() {
        let key = StreamKey {
            url: "https://music.youtube.com/watch?v=abcdefghijk".into(),
            quality: AudioQuality::Normal,
            video: None,
        };
        let now = SystemTime::now();
        let expiry = now.duration_since(UNIX_EPOCH).unwrap().as_secs() + 120;
        let stream = ResolvedStream::parse(
            key.clone(),
            format!("https://rr1.googlevideo.com/videoplayback?expire={expiry}&id=x\n").as_bytes(),
            now,
        )
        .unwrap();
        assert!(stream.matches(&key));
        assert!(!stream.matches(&StreamKey {
            quality: AudioQuality::High,
            ..key.clone()
        }));
        assert!(!stream.matches(&StreamKey {
            url: "other".into(),
            ..key.clone()
        }));
        assert!(
            ResolvedStream::parse(
                key.clone(),
                b"https://rr1.googlevideo.com/videoplayback?expire=1",
                now
            )
            .is_err()
        );
        for url in [
            "http://rr1.googlevideo.com/x",
            "https://example.com/x",
            "file:///tmp/private",
        ] {
            assert!(ResolvedStream::parse(key.clone(), url.as_bytes(), now).is_err());
        }
        let video = ResolvedStream::parse(
            key,
            b"https://rr1.googlevideo.com/video\nhttps://rr1.googlevideo.com/audio\n",
            now,
        )
        .unwrap();
        assert!(video.url.contains(";!new_stream;"));
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
        let (socket, _mpv) = UnixStream::pair().unwrap();
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

    #[test]
    fn synchronous_reads_use_the_remaining_deadline() {
        let (mut socket, _mpv) = UnixStream::pair().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_millis(500)))
            .unwrap();
        let start = Instant::now();
        let result = read_message(
            &mut socket,
            &mut Vec::new(),
            start + Duration::from_millis(25),
        );
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("stopped responding")
        );
        assert!(start.elapsed() < Duration::from_millis(250));
    }

    #[test]
    fn command_wakeups_coalesce_without_losing_commands_or_socket_readiness() {
        let (commands, received) = command_channel().unwrap();
        let (mut mpv, socket) = UnixStream::pair().unwrap();
        commands.send(PlayerCommand::Stop).unwrap();
        commands.clone().send(PlayerCommand::Pause(true)).unwrap();
        assert!(!wait_for_player_input(&socket, &commands.wake, None, None).unwrap());
        assert!(matches!(received.try_recv(), Ok(PlayerCommand::Stop)));
        assert!(matches!(
            received.try_recv(),
            Ok(PlayerCommand::Pause(true))
        ));
        let mut wake = libc::pollfd {
            fd: commands.wake.as_raw_fd(),
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

    fn seek_request(revision: u64, seconds: f64) -> SeekRequest {
        SeekRequest {
            track_id: 1,
            revision,
            seconds,
        }
    }

    fn socket_message(socket: &mut UnixStream) -> Value {
        let mut line = Vec::new();
        loop {
            let mut byte = [0];
            socket.read_exact(&mut byte).unwrap();
            if byte[0] == b'\n' {
                break;
            }
            line.push(byte[0]);
        }
        serde_json::from_slice(&line).unwrap()
    }

    #[test]
    fn synchronous_requests_ignore_zero_and_previous_request_replies() {
        let (mut socket, mut mpv) = UnixStream::pair().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_millis(80)))
            .unwrap();
        mpv.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        let worker = thread::spawn(move || {
            let first_id = socket_message(&mut mpv)["request_id"].clone();
            let replies = format!(
                "{}\n{}\n{}\n",
                json!({"request_id":0,"error":"success"}),
                json!({"request_id":first_id,"error":"property unavailable"}),
                json!({"request_id":first_id,"error":"success"}),
            );
            mpv.write_all(replies.as_bytes()).unwrap();
            let second_id = socket_message(&mut mpv)["request_id"].clone();
            writeln!(
                mpv,
                "{}",
                json!({"request_id":second_id,"error":"property unavailable"})
            )
            .unwrap();
            (first_id, second_id)
        });
        let mut pending = Vec::new();
        let first = request(
            &mut socket,
            &mut pending,
            json!(["set_property", "vid", "auto"]),
            |_| {},
        );
        let second = request(
            &mut socket,
            &mut pending,
            json!(["set_property", "vid", "no"]),
            |_| {},
        );
        let (first_id, second_id) = worker.join().unwrap();
        for result in [first, second] {
            assert_eq!(
                result.unwrap_err().to_string(),
                "mpv could not update playback credentials"
            );
        }
        assert!(first_id.as_i64().unwrap() < 0);
        assert!(second_id.as_i64().unwrap() < 0);
        assert_ne!(first_id, second_id);
    }

    #[test]
    fn buffered_seek_completion_is_delivered_while_mpv_is_silent() {
        let (events, received) = async_channel::unbounded();
        let (mut socket, mut mpv) = UnixStream::pair().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_millis(80)))
            .unwrap();
        mpv.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        let worker = thread::spawn(move || {
            let command = socket_message(&mut mpv);
            let replies = format!(
                "{}\n{}\n",
                json!({"request_id":command["request_id"],"error":"success"}),
                json!({"event":"property-change","id":7,"name":"time-pos","data":20.}),
            );
            mpv.write_all(replies.as_bytes()).unwrap();
            mpv
        });
        let mut state = PlaybackState {
            desired: Some(1),
            current: Some(1),
            loaded: true,
            position_observer: 7,
            seek: Some(ActiveSeek {
                request: seek_request(2, 20.),
                phase: SeekPhase::Position,
            }),
            ..Default::default()
        };
        let mut pending = Vec::new();
        request(
            &mut socket,
            &mut pending,
            json!(["set_property", "vid", "auto"]),
            |value| state.message(value, &events),
        )
        .unwrap();
        let _mpv = worker.join().unwrap();
        assert!(pending.contains(&b'\n'));
        assert!(received.try_recv().is_err());

        state
            .read_messages(&mut socket, &mut pending, &mut [0; 16384], &events)
            .unwrap();
        assert!(matches!(
            received.try_recv(),
            Ok(PlayerEvent::SeekFinished {
                track_id: 1,
                revision: 2,
                result: Ok(20.),
            })
        ));
    }

    fn accepted_seek_command(
        state: &mut PlaybackState,
        socket: &mut UnixStream,
        mpv: &mut UnixStream,
        events: &async_channel::Sender<PlayerEvent>,
    ) -> Value {
        state.write_seek(socket).unwrap();
        let check = socket_message(mpv);
        assert_eq!(check["command"], json!(["get_property", "seekable"]));
        state.message(
            json!({"request_id":check["request_id"],"error":"success","data":true}),
            events,
        );
        state.write_seek(socket).unwrap();
        socket_message(mpv)
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
    fn seek_waits_for_restart_then_confirms_position_with_a_fresh_observer() {
        let (events, received) = async_channel::unbounded();
        let (mut socket, mut mpv) = UnixStream::pair().unwrap();
        mpv.set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        let mut state = PlaybackState {
            desired: Some(1),
            current: Some(1),
            loaded: true,
            ..Default::default()
        };
        state.queue_seek(seek_request(2, 10.), &events);
        state.queue_seek(seek_request(3, 20.), &events);
        state.write_seek(&mut socket).unwrap();
        assert!(
            state.seek.is_none(),
            "initial loading restart is a separate boundary"
        );
        state.message(json!({"event":"playback-restart"}), &events);
        assert_eq!(
            accepted_seek_command(&mut state, &mut socket, &mut mpv, &events),
            json!({"command":["seek",20.,"absolute"],"request_id":3})
        );
        state.message(json!({"request_id":3,"error":"success"}), &events);
        state.message(json!({"event":"playback-restart"}), &events);
        assert!(matches!(
            state.seek.as_ref().map(|seek| &seek.phase),
            Some(SeekPhase::Start)
        ));
        state.message(json!({"event":"seek"}), &events);
        state.message(
            json!({"event":"property-change","id":1,"name":"time-pos","data":1.}),
            &events,
        );
        state.message(json!({"event":"playback-restart"}), &events);
        assert!(received.try_recv().is_err());
        state.write_seek(&mut socket).unwrap();
        assert_eq!(
            socket_message(&mut mpv)["command"],
            json!(["unobserve_property", 1])
        );
        assert_eq!(
            socket_message(&mut mpv),
            json!({"command":["observe_property",7,"time-pos"],"request_id":"seek-position:3"})
        );
        state.message(
            json!({"request_id":"seek-position:2","error":"success","data":10.}),
            &events,
        );
        assert!(received.try_recv().is_err());
        state.message(
            json!({"event":"property-change","id":7,"name":"time-pos","data":19.999}),
            &events,
        );
        assert!(
            matches!(received.try_recv(), Ok(PlayerEvent::SeekFinished { track_id:1, revision:3, result:Ok(seconds) }) if seconds == 19.999)
        );
        state.message(
            json!({"request_id":"seek-position:3","error":"success"}),
            &events,
        );
        assert!(
            received.try_recv().is_err(),
            "the initial sample can precede registration acknowledgment"
        );
        state.message(
            json!({"event":"property-change","id":1,"name":"time-pos","data":1.}),
            &events,
        );
        assert!(received.try_recv().is_err());
        state.message(
            json!({"event":"property-change","id":7,"name":"time-pos","data":20.1}),
            &events,
        );
        assert!(matches!(
            received.try_recv(),
            Ok(PlayerEvent::Position {
                track_id: 1,
                seconds: 20.1
            })
        ));
    }

    #[test]
    fn rapid_seeks_coalesce_behind_one_in_flight_seek_and_no_op_is_acknowledged() {
        let (events, received) = async_channel::unbounded();
        let (mut socket, mut mpv) = UnixStream::pair().unwrap();
        let mut state = PlaybackState {
            desired: Some(1),
            current: Some(1),
            loaded: true,
            restarted: true,
            ..Default::default()
        };
        state.queue_seek(seek_request(2, 10.), &events);
        assert_eq!(
            accepted_seek_command(&mut state, &mut socket, &mut mpv, &events)["request_id"],
            2
        );
        state.queue_seek(seek_request(3, 20.), &events);
        state.queue_seek(seek_request(4, 10.), &events);
        state.write_seek(&mut socket).unwrap();
        assert_eq!(state.seek.as_ref().unwrap().request.revision, 2);
        assert_eq!(state.next_seek.unwrap().revision, 4);
        for value in [
            json!({"request_id":2,"error":"success"}),
            json!({"event":"seek"}),
            json!({"event":"playback-restart"}),
        ] {
            state.message(value, &events);
        }
        state.write_seek(&mut socket).unwrap();
        for _ in 0..2 {
            socket_message(&mut mpv);
        }
        state.message(
            json!({"request_id":"seek-position:2","error":"success"}),
            &events,
        );
        assert!(
            received.try_recv().is_err(),
            "registration alone does not confirm a position"
        );
        state.message(
            json!({"event":"property-change","id":7,"name":"time-pos","data":10.}),
            &events,
        );
        assert!(matches!(
            received.try_recv(),
            Ok(PlayerEvent::SeekFinished {
                revision: 2,
                result: Ok(10.),
                ..
            })
        ));
        assert_eq!(
            accepted_seek_command(&mut state, &mut socket, &mut mpv, &events),
            json!({"command":["seek",10.,"absolute"],"request_id":4})
        );
        for value in [
            json!({"request_id":4,"error":"success"}),
            json!({"event":"seek"}),
            json!({"event":"playback-restart"}),
        ] {
            state.message(value, &events);
        }
        state.write_seek(&mut socket).unwrap();
        for _ in 0..2 {
            socket_message(&mut mpv);
        }
        state.message(
            json!({"event":"property-change","id":13,"name":"time-pos","data":10.}),
            &events,
        );
        assert!(matches!(
            received.try_recv(),
            Ok(PlayerEvent::SeekFinished {
                revision: 4,
                result: Ok(10.),
                ..
            })
        ));
        assert!(state.seek.is_none() && state.next_seek.is_none());
    }

    #[test]
    fn seek_errors_end_of_file_and_invalidation_do_not_leave_pending_work() {
        let (events, received) = async_channel::unbounded();
        let (mut socket, mut mpv) = UnixStream::pair().unwrap();
        let mut state = PlaybackState {
            desired: Some(1),
            current: Some(1),
            loaded: true,
            restarted: true,
            entries: HashMap::from([(10, 1)]),
            ..Default::default()
        };
        state.queue_seek(seek_request(2, 10.), &events);
        accepted_seek_command(&mut state, &mut socket, &mut mpv, &events);
        state.message(
            json!({"request_id":2,"error":"INVENTED_PRIVATE_DIAGNOSTIC"}),
            &events,
        );
        assert!(
            matches!(received.try_recv(),Ok(PlayerEvent::SeekFinished{revision:2,result:Err(message),..}) if !message.contains("INVENTED_PRIVATE_DIAGNOSTIC"))
        );
        state.queue_seek(seek_request(3, 10.), &events);
        accepted_seek_command(&mut state, &mut socket, &mut mpv, &events);
        state.queue_seek(seek_request(4, 20.), &events);
        state.message(
            json!({"event":"end-file","playlist_entry_id":10,"reason":"eof"}),
            &events,
        );
        for revision in [3, 4] {
            assert!(
                matches!(received.try_recv(),Ok(PlayerEvent::SeekFinished{revision:id,result:Err(_),..}) if id == revision)
            );
        }
        assert!(matches!(
            received.try_recv(),
            Ok(PlayerEvent::Ended { track_id: 1 })
        ));
        assert!(state.seek.is_none() && state.next_seek.is_none() && !state.restarted);

        state.loaded = true;
        state.restarted = true;
        state.queue_seek(seek_request(5, 10.), &events);
        accepted_seek_command(&mut state, &mut socket, &mut mpv, &events);
        state.invalidate_seeks();
        state.desired = Some(6);
        state.message(json!({"request_id":5,"error":"error"}), &events);
        state.message(
            json!({"request_id":"seek-position:5","error":"success","data":10.}),
            &events,
        );
        assert!(received.try_recv().is_err());
        assert!(state.seek.is_none() && state.next_seek.is_none());
        state.queue_seek(seek_request(7, 10.), &events);
        assert!(matches!(
            received.try_recv(),
            Ok(PlayerEvent::SeekFinished {
                revision: 7,
                result: Err(_),
                ..
            })
        ));
    }

    #[test]
    fn seek_observer_registration_and_unavailable_initial_sample_fail_explicitly() {
        for response in [
            json!({"request_id":"seek-position:2","error":"INVENTED_PRIVATE_DIAGNOSTIC"}),
            json!({"event":"property-change","id":7,"name":"time-pos","data":null}),
            json!({"event":"property-change","id":7,"name":"time-pos","data":-1.}),
        ] {
            let (events, received) = async_channel::unbounded();
            let mut state = PlaybackState {
                desired: Some(1),
                current: Some(1),
                loaded: true,
                restarted: true,
                position_observer: 7,
                seek: Some(ActiveSeek {
                    request: seek_request(2, 10.),
                    phase: SeekPhase::Position,
                }),
                ..Default::default()
            };
            state.message(response, &events);
            assert!(
                matches!(received.try_recv(),Ok(PlayerEvent::SeekFinished{revision:2,result:Err(message),..}) if !message.contains("INVENTED_PRIVATE_DIAGNOSTIC"))
            );
            assert!(state.seek.is_none());
            state.message(
                json!({"event":"property-change","id":7,"name":"time-pos","data":10.}),
                &events,
            );
            assert!(!matches!(
                received.try_recv(),
                Ok(PlayerEvent::SeekFinished { .. })
            ));
        }
    }

    #[test]
    fn unseekable_or_unknown_streams_fail_before_seeking_and_cancelled_checks_are_ignored() {
        for response in [
            json!({"error":"success","data":false}),
            json!({"error":"property unavailable"}),
        ] {
            let (events, received) = async_channel::unbounded();
            let (mut socket, mut mpv) = UnixStream::pair().unwrap();
            let mut state = PlaybackState {
                desired: Some(1),
                current: Some(1),
                loaded: true,
                restarted: true,
                ..Default::default()
            };
            state.queue_seek(seek_request(2, 10.), &events);
            state.write_seek(&mut socket).unwrap();
            assert_eq!(
                socket_message(&mut mpv)["command"],
                json!(["get_property", "seekable"])
            );
            let mut response = response;
            response["request_id"] = json!("seekable:2");
            state.message(response, &events);
            assert!(matches!(
                received.try_recv(),
                Ok(PlayerEvent::SeekFinished {
                    revision: 2,
                    result: Err(_),
                    ..
                })
            ));
            assert!(state.seek.is_none());
        }
        let (events, received) = async_channel::unbounded();
        let (mut socket, mut mpv) = UnixStream::pair().unwrap();
        let mut state = PlaybackState {
            desired: Some(1),
            current: Some(1),
            loaded: true,
            restarted: true,
            ..Default::default()
        };
        state.queue_seek(seek_request(2, 10.), &events);
        state.write_seek(&mut socket).unwrap();
        socket_message(&mut mpv);
        state.invalidate_seeks();
        state.desired = Some(3);
        state.message(
            json!({"request_id":"seekable:2","error":"success","data":true}),
            &events,
        );
        assert!(state.seek.is_none() && state.next_seek.is_none());
        assert!(received.try_recv().is_err());
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
        let command = load_command(&url, &title, options);
        assert_eq!(command[0], "loadfile");
        assert_eq!(command[1], "https://music.youtube.com/watch?v=csILUDfmtqM");
        assert_eq!(command[2], "replace");
        assert_eq!(command[4]["start"], "32.5");
        assert_eq!(command[4]["pause"], "yes");
        assert_eq!(command[4]["ytdl"], "no");
        let defaults = load_command("unused", "Track", PlayOptions::default());
        assert_eq!(defaults[4]["start"], "0");
        assert_eq!(defaults[4]["pause"], "no");
        assert_eq!(defaults[4]["ytdl"], "no");
        let video = load_command(
            "unused",
            "Track",
            PlayOptions {
                video: Some(VideoQuality::P720),
                ..options
            },
        );
        assert_eq!(video[4]["ytdl"], "no");
        assert_eq!(video[4]["start"], "32.5");
        assert_eq!(video[4]["pause"], "yes");
    }

    #[test]
    fn started_preserves_pause_intent_across_loading_notifications() {
        for (requested, observed, override_pause, expected) in [
            (false, Some(true), None, false),
            (true, Some(false), None, true),
            (true, Some(true), None, true),
            (true, None, None, true),
            (true, Some(true), Some(false), false),
        ] {
            let (events, received) = async_channel::unbounded();
            let mut state = PlaybackState {
                current: Some(1),
                desired: Some(1),
                paused: override_pause.unwrap_or(requested),
                pause_target: Some(override_pause.unwrap_or(requested)),
                pause_after_load: true,
                ..Default::default()
            };
            if let Some(paused) = observed {
                state.message(
                    json!({"event":"property-change","name":"pause","data":paused}),
                    &events,
                );
            }
            assert!(received.try_recv().is_err());
            state.message(json!({"event":"file-loaded"}), &events);
            assert!(matches!(
                received.try_recv(),
                Ok(PlayerEvent::Started { track_id: 1, paused }) if paused == expected
            ));
        }
    }

    #[test]
    fn pending_pause_survives_stale_events_until_the_latest_write_is_confirmed() {
        let (events, received) = async_channel::unbounded();
        let mut state = PlaybackState {
            current: Some(1),
            desired: Some(1),
            paused: true,
            pause_target: Some(true),
            pause_after_load: true,
            pause_revision: 1,
            ..Default::default()
        };
        state.message(
            json!({"event":"property-change","name":"pause","data":false}),
            &events,
        );
        assert!(state.paused);
        assert!(received.try_recv().is_err());
        state.message(
            json!({"event":"property-change","name":"pause","data":true}),
            &events,
        );
        state.message(
            json!({"request_id":"pause:1","error":"success","data":true}),
            &events,
        );
        assert_eq!(state.pause_target, Some(true));
        state.message(json!({"event":"file-loaded"}), &events);
        assert!(matches!(
            received.try_recv(),
            Ok(PlayerEvent::Started { paused: true, .. })
        ));

        // The worker reapplies the target after per-file options, even for a no-op.
        state.pause_after_load = false;
        state.pause_revision = 2;
        state.message(
            json!({"event":"property-change","name":"pause","data":false}),
            &events,
        );
        state.message(
            json!({"request_id":"pause:1","error":"success","data":true}),
            &events,
        );
        assert!(received.try_recv().is_err());
        assert_eq!(state.pause_target, Some(true));
        state.message(
            json!({"request_id":"pause:2","error":"success","data":true}),
            &events,
        );
        assert_eq!(state.pause_target, None);
        assert!(matches!(
            received.try_recv(),
            Ok(PlayerEvent::Paused { paused: true, .. })
        ));

        // A newer opposite request supersedes both old replies and observations.
        state.pause_revision = 3;
        state.pause_target = Some(false);
        state.paused = false;
        state.message(
            json!({"request_id":"pause:2","error":"success","data":true}),
            &events,
        );
        state.message(
            json!({"event":"property-change","name":"pause","data":true}),
            &events,
        );
        assert!(received.try_recv().is_err());
        assert!(!state.paused);
        state.message(
            json!({"request_id":"pause:3","error":"success","data":false}),
            &events,
        );
        assert_eq!(state.pause_target, None);
        assert!(matches!(
            received.try_recv(),
            Ok(PlayerEvent::Paused { paused: false, .. })
        ));

        // Once acknowledged, ordinary external media controls remain authoritative.
        state.message(
            json!({"event":"property-change","name":"pause","data":true}),
            &events,
        );
        assert!(matches!(
            received.try_recv(),
            Ok(PlayerEvent::Paused { paused: true, .. })
        ));

        // An external pause can win between the set and its confirmation query.
        state.pause_revision = 4;
        state.pause_target = Some(true);
        state.message(
            json!({"request_id":"pause:4","error":"success","data":false}),
            &events,
        );
        assert_eq!(state.pause_target, None);
        assert!(!state.paused);
        assert!(matches!(
            received.try_recv(),
            Ok(PlayerEvent::Paused { paused: false, .. })
        ));
    }

    #[test]
    #[ignore = "requires Python with yt-dlp; set YT_DLP_TEST_PYTHON if needed; no network"]
    fn quality_selectors_respect_audio_classes_and_video_caps() {
        let python = std::env::var_os("YT_DLP_TEST_PYTHON").unwrap_or_else(|| "python3".into());
        let selectors = AudioQuality::ALL.map(AudioQuality::selector);
        let output = Command::new(python)
            .args(["-B", "-c", r#"
import json, sys
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

    #[test]
    fn playback_speed_is_validated_before_it_can_reach_mpv() {
        assert_eq!(f64::from(PlaybackSpeed::default()), 1.);
        for invalid in [f64::NAN, f64::INFINITY, -1., 0., 0.24, 2.01] {
            assert!(PlaybackSpeed::try_from(invalid).is_err());
        }
        assert!(serde_json::from_value::<PlaybackSpeed>(json!(0.1)).is_err());
        let speed = serde_json::from_value::<PlaybackSpeed>(json!(1.5)).unwrap();
        assert_eq!(serde_json::to_value(speed).unwrap(), json!(1.5));
        let (events, received) = async_channel::unbounded();
        let mut state = PlaybackState::default();
        state.message(
            json!({"event":"property-change","name":"speed","data":1.5}),
            &events,
        );
        assert!(matches!(received.try_recv(), Ok(PlayerEvent::Speed(value)) if value == speed));
        state.message(
            json!({"event":"property-change","name":"speed","data":0}),
            &events,
        );
        assert!(received.try_recv().is_err());
    }

    fn file_identity(path: &str) -> Option<(u64, u64)> {
        use std::os::unix::fs::MetadataExt;
        match fs::metadata(path) {
            Ok(metadata) => Some((metadata.dev(), metadata.ino())),
            Err(error) if error.kind() == ErrorKind::NotFound => None,
            Err(error) => panic!("could not inspect synthetic playback file: {error}"),
        }
    }

    fn sealed_cookies(value: &str) -> File {
        let fd = unsafe {
            libc::memfd_create(
                c"yt-music-synthetic".as_ptr(),
                libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
            )
        };
        assert!(fd >= 0);
        let mut file = unsafe { File::from_raw_fd(fd) };
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .unwrap();
        write!(
            file,
            "# Netscape HTTP Cookie File\n.youtube.com\tTRUE\t/\tTRUE\t0\tYT_MUSIC_SYNTHETIC\t{value}\n"
        )
        .unwrap();
        let seals =
            libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL;
        assert_eq!(unsafe { libc::fcntl(fd, libc::F_ADD_SEALS, seals) }, 0);
        file
    }

    fn synthetic_credentials(value: &str) -> PlaybackCredentials {
        PlaybackCredentials {
            cookies: sealed_cookies(value),
            identity: sealed_file(
                br#"{"authuser":"2","delegated_session_id":"brand","data_sync_id":"brand||user"}"#,
            )
            .unwrap(),
        }
    }

    #[test]
    fn extractor_cookie_copy_is_private_writable_ram() {
        let mut source = sealed_cookies("INVENTED_FIRST");
        let mut file = writable_playback_cookies(source.try_clone().unwrap()).unwrap();
        let path = format!("/proc/self/fd/{}", file.as_raw_fd());
        let original = file_identity(&path);
        assert!(
            fs::read_to_string(&path)
                .unwrap()
                .contains("INVENTED_FIRST")
        );
        assert!(
            fs::read_link(&path)
                .unwrap()
                .to_string_lossy()
                .contains("memfd:")
        );
        assert_eq!(file.metadata().unwrap().permissions().mode() & 0o777, 0o600);
        assert_ne!(
            unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
        file.set_len(0).unwrap();
        file.rewind().unwrap();
        file.write_all(b"extractor rewrote its jar").unwrap();
        assert!(source.write_all(b"source remains sealed").is_err());
        drop(file);
        assert_ne!(file_identity(&path), original);
    }

    fn credential_barrier_command(server: &mut UnixStream) -> Value {
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

    fn reply_to_credential_barrier(server: &mut UnixStream) {
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
    fn credential_changes_wait_for_old_file_events_and_revoke_ram_files() {
        let (mut client, mut server) = UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(Duration::from_millis(80)))
            .unwrap();
        server
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let plugin_dir = Path::new("/private/synthetic-plugin-root");
        let mut current =
            Some(prepare_credentials(synthetic_credentials("INVENTED_FIRST"), plugin_dir).unwrap());
        let files = [
            current.as_ref().unwrap().cookies.as_raw_fd(),
            current.as_ref().unwrap()._identity.as_raw_fd(),
            current.as_ref().unwrap().config.as_raw_fd(),
        ]
        .map(|fd| {
            let path = format!("/proc/self/fd/{fd}");
            let identity = file_identity(&path);
            (path, identity)
        });
        let old_files = files.clone();
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
                if start_after_marker_reply {
                    for (path, identity) in &old_files {
                        assert_eq!(file_identity(path), *identity);
                    }
                }
                server
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                writeln!(server, "{}", json!({"event":"end-file"})).unwrap();
            }
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
        for (path, identity) in files {
            assert_ne!(file_identity(&path), identity);
        }
        assert!(
            fs::read_to_string(format!(
                "/proc/self/fd/{}",
                current.as_ref().unwrap().cookies.as_raw_fd()
            ))
            .unwrap()
            .contains("INVENTED_SECOND")
        );
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
            let (mut client, mut server) = UnixStream::pair().unwrap();
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
            let (mut client, mut server) = UnixStream::pair().unwrap();
            client
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            server
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            let plugin_dir = Path::new("/private/synthetic-plugin-root");
            let mut current = Some(
                prepare_credentials(synthetic_credentials("INVENTED_FIRST"), plugin_dir).unwrap(),
            );
            let path = format!(
                "/proc/self/fd/{}",
                current.as_ref().unwrap().cookies.as_raw_fd()
            );
            let original = file_identity(&path);
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
            // The actor keeps the old files until core teardown on this fatal error.
            assert_eq!(file_identity(&path), original);
            drop(current);
            assert_ne!(file_identity(&path), original);
            worker.join().unwrap();
        }
    }

    fn local_extractor_player(directory: &Path) -> Player {
        let extractor = directory.join("extractor");
        fs::write(
            &extractor,
            r#"#!/bin/sh
if [ "$3" = --version ]; then echo 2026.08.19; exit; fi
exec python3 - "$@" <<'PYTHON'
import json, os, re, signal, sys
from pathlib import Path

args = sys.argv[1:]
cookies = args[args.index('--cookies') + 1]
config = args[args.index('--config-locations') + 1]
identity = re.search(r'context=(/proc/\d+/fd/\d+)', Path(config).read_text())[1]
paths = [cookies, config, identity]
files = [open(path, 'rb') for path in paths]
assert b'INVENTED_' in files[0].read()
ready = Path('READY_PATH')
ready.with_suffix('.tmp').write_text(json.dumps({'pid': os.getpid(), 'paths': paths}))
ready.with_suffix('.tmp').replace(ready)
os.kill(os.getpid(), signal.SIGSTOP)
PYTHON
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

    struct ExtractorProcess {
        pid: u64,
        files: Vec<(String, Option<(u64, u64)>)>,
    }

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
            assert!(Path::new(&format!("/proc/{pid}")).exists());
            let files = data["paths"]
                .as_array()
                .unwrap()
                .iter()
                .map(|path| {
                    let path = path.as_str().unwrap().to_owned();
                    let original = file_identity(&path);
                    assert!(original.is_some());
                    (path, original)
                })
                .collect();
            Self { pid, files }
        }

        fn assert_revoked(&self) {
            assert!(
                !Path::new(&format!("/proc/{}", self.pid)).exists(),
                "extractor must be reaped before cleanup completes"
            );
            for (path, original) in &self.files {
                assert_ne!(
                    &file_identity(path),
                    original,
                    "credential descriptor still open"
                );
            }
        }
    }

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

    #[test]
    #[ignore = "requires libmpv and yt-dlp; no network, display, or audio"]
    fn credential_installation_reports_preparation_failure() {
        let directory = tempfile::tempdir().unwrap();
        let mut player = Player::new(PlayerOptions {
            audio_output: Some("null".into()),
            ..Default::default()
        })
        .unwrap();
        player
            .clear_credentials()
            .unwrap()
            .recv_blocking()
            .unwrap()
            .unwrap();
        let mut credentials = synthetic_credentials("INVENTED_INVALID_SOURCE");
        credentials.cookies = File::open(directory.path()).unwrap();
        let result = player
            .set_credentials(credentials)
            .unwrap()
            .recv_blocking()
            .unwrap();
        assert!(
            result.is_err(),
            "failed installation must not acknowledge success"
        );
        player.shutdown().unwrap();
        assert!(player.clear_credentials().is_err());
    }

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
                assert!(
                    !Path::new(&format!("/proc/{pid}")).exists(),
                    "extractor survived immediate credential clear"
                );
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
        let server_script = sealed_file(include_bytes!("ytdlp_resolver.py")).unwrap();
        let mut child = Command::new(python)
            .args(["-B", "-"])
            .arg(plugin_dir)
            .arg(format!(
                "/proc/{}/fd/{}",
                std::process::id(),
                server_script.as_raw_fd()
            ))
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

    #[test]
    fn subprocess_diagnostics_cannot_enter_playback_errors() {
        let (tx, rx) = async_channel::unbounded();
        let mut state = PlaybackState {
            desired: Some(1),
            ..Default::default()
        };
        state.message(
            json!({"request_id":1,"error":"success","data":{"playlist_entry_id":10}}),
            &tx,
        );
        state.message(json!({"event":"log-message","prefix":"ytdl_hook","text":"Cookie: SAPISID=INVENTED_SECRET"}), &tx);
        state.message(
            json!({"event":"end-file","playlist_entry_id":10,"reason":"error"}),
            &tx,
        );
        match rx.try_recv().unwrap() {
            PlayerEvent::Error { message, .. } => assert!(!message.contains("INVENTED_SECRET")),
            event => panic!("unexpected event: {event:?}"),
        }
        state.message(json!({"request_id":1,"error":"INVENTED_SECRET"}), &tx);
        match rx.try_recv().unwrap() {
            PlayerEvent::Error { message, .. } => assert!(!message.contains("INVENTED_SECRET")),
            event => panic!("unexpected event: {event:?}"),
        }
    }

    #[test]
    fn external_stop_reports_the_current_load_without_finishing_or_stopping_a_replacement() {
        for desired in [Some(1), Some(2), None] {
            let (events, received) = async_channel::unbounded();
            let mut state = PlaybackState {
                desired,
                current: Some(1),
                entries: HashMap::from([(10, 1)]),
                loaded: true,
                restarted: true,
                ..Default::default()
            };
            state.message(
                json!({"event":"end-file","playlist_entry_id":10,"reason":"stop"}),
                &events,
            );
            if desired == Some(1) {
                assert!(matches!(
                    received.try_recv(),
                    Ok(PlayerEvent::Stopped { track_id: 1 })
                ));
                assert!(!state.loaded && !state.restarted);
                assert_eq!(state.desired, None);
            } else {
                assert!(state.loaded);
                assert_eq!(state.desired, desired);
            }
            assert!(received.try_recv().is_err());
            state.message(
                json!({"event":"property-change","id":1,"name":"time-pos","data":30.}),
                &events,
            );
            assert!(received.try_recv().is_err());
        }
    }

    #[test]
    fn seekability_is_published_for_the_loaded_track_and_resets_for_replacement() {
        let (events, received) = async_channel::unbounded();
        let mut state = PlaybackState {
            desired: Some(1),
            entries: HashMap::from([(10, 1), (11, 2)]),
            ..Default::default()
        };
        state.message(
            json!({"event":"start-file","playlist_entry_id":10}),
            &events,
        );
        state.message(
            json!({"event":"property-change","id":0,"name":"seekable","data":true}),
            &events,
        );
        assert!(received.try_recv().is_err());
        state.message(json!({"event":"file-loaded"}), &events);
        assert!(matches!(
            received.try_recv(),
            Ok(PlayerEvent::Started { track_id: 1, .. })
        ));
        assert!(matches!(
            received.try_recv(),
            Ok(PlayerEvent::Seekable {
                track_id: 1,
                seekable: true
            })
        ));
        state.message(
            json!({"event":"property-change","id":0,"name":"seekable","data":false}),
            &events,
        );
        assert!(matches!(
            received.try_recv(),
            Ok(PlayerEvent::Seekable {
                track_id: 1,
                seekable: false
            })
        ));
        state.desired = Some(2);
        state.loaded = false;
        state.message(
            json!({"event":"start-file","playlist_entry_id":11}),
            &events,
        );
        assert!(!state.seekable);
        state.message(json!({"event":"file-loaded"}), &events);
        assert!(matches!(
            received.try_recv(),
            Ok(PlayerEvent::Started { track_id: 2, .. })
        ));
        assert!(received.try_recv().is_err());
        state.message(
            json!({"request_id":"loaded-seekable:1","error":"success","data":true}),
            &events,
        );
        assert!(received.try_recv().is_err());
        state.message(
            json!({"request_id":"loaded-seekable:2","error":"success","data":true}),
            &events,
        );
        assert!(matches!(
            received.try_recv(),
            Ok(PlayerEvent::Seekable {
                track_id: 2,
                seekable: true
            })
        ));
    }

    #[test]
    fn adopting_a_loaded_stream_refreshes_duration_and_seekability() {
        let (events, received) = async_channel::unbounded();
        let (mut socket, mut mpv) = UnixStream::pair().unwrap();
        mpv.set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        // mpv finished loading before the UI adopted this occurrence. Its first
        // duration/seekability notifications belonged to the old UI track.
        let mut state = PlaybackState {
            current: Some(2),
            desired: Some(2),
            loaded: true,
            pause_after_load: true,
            ..Default::default()
        };
        let mut pending = b"{\"event\":\"playback-restart\"}\n".to_vec();
        state
            .read_messages(&mut socket, &mut pending, &mut [0; 1024], &events)
            .unwrap();
        for (property, data) in [("seekable", json!(true)), ("duration", json!(281.521))] {
            let command = socket_message(&mut mpv);
            assert_eq!(command["command"], json!(["get_property", property]));
            state.message(
                json!({"request_id":command["request_id"],"error":"success","data":data}),
                &events,
            );
        }
        assert!(matches!(
            received.try_recv(),
            Ok(PlayerEvent::Seekable {
                track_id: 2,
                seekable: true
            })
        ));
        assert!(matches!(
            received.try_recv(),
            Ok(PlayerEvent::Duration {
                track_id: 2,
                seconds: 281.521
            })
        ));
        state.desired = Some(3);
        state.message(
            json!({"request_id":"loaded-duration:2","error":"success","data":281.521}),
            &events,
        );
        assert!(
            received.is_empty(),
            "late duration must not overwrite a replacement track"
        );
    }

    #[test]
    fn replacing_track_does_not_finish_the_new_track() {
        for reason in ["eof", "error", "stop"] {
            let (tx, rx) = async_channel::unbounded();
            let mut state = PlaybackState {
                desired: Some(2),
                ..Default::default()
            };
            state.message(
                json!({"request_id":1,"error":"success","data":{"playlist_entry_id":10}}),
                &tx,
            );
            state.message(
                json!({"request_id":2,"error":"success","data":{"playlist_entry_id":11}}),
                &tx,
            );
            state.message(json!({"event":"start-file","playlist_entry_id":11}), &tx);
            state.message(json!({"event":"file-loaded"}), &tx);
            assert!(matches!(
                rx.try_recv(),
                Ok(PlayerEvent::Started {
                    track_id: 2,
                    paused: false
                })
            ));
            state.message(
                json!({"event":"end-file","playlist_entry_id":10,"reason":reason}),
                &tx,
            );
            state.message(json!({"request_id":1,"error":"load failed"}), &tx);
            assert!(rx.try_recv().is_err());
            assert!(state.loaded);
            state.message(
                json!({"event":"end-file","playlist_entry_id":11,"reason":"eof"}),
                &tx,
            );
            assert!(matches!(
                rx.try_recv(),
                Ok(PlayerEvent::Ended { track_id: 2 })
            ));
        }
    }

    #[test]
    #[ignore = "measures /proc actor wakeups and latency with libmpv, null audio and synthetic silence"]
    fn local_actor_wakeups_and_latency() {
        let directory = tempfile::tempdir().unwrap();
        let path = silent_audio(directory.path());
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

    fn silent_audio(directory: &Path) -> PathBuf {
        let path = directory.join("silence.wav");
        let sample_bytes = 120_u32 * 8000 * 2;
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

    #[test]
    #[ignore = "requires libmpv; uses a synthetic extractor, null audio and no network"]
    fn paused_actor_refreshes_expired_preparation_and_writes_pending_seek() {
        let directory = tempfile::tempdir().unwrap();
        let path = silent_audio(directory.path());
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
            &|event| matches!(event, PlayerEvent::Started { track_id: id, paused: true } if *id == track_id),
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
        let path = silent_audio(directory.path());
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
            &|event| matches!(event,PlayerEvent::Started{track_id:id,paused:true} if *id == track_id),
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
                        PlayerEvent::Started { track_id, paused } if track_id == replacement => {
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
