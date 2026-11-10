use crate::auth::{PlaybackCredentials, sealed_file};
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
    path::{Path, PathBuf},
    ptr::NonNull,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicI64, AtomicU64, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::{Duration, Instant},
};

#[cfg(test)]
use std::process::{Command, Stdio};

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
    #[allow(dead_code)] // Consumed by the app integration after its module split.
    Notice(String),
    VideoChanged,
    VideoError {
        message: String,
    },
    Volume(f64),
    Speed(PlaybackSpeed),
}

enum PlayerCommand {
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
        let increment = 1_u64.to_ne_bytes();
        loop {
            let result = unsafe {
                libc::write(
                    self.wake.as_raw_fd(),
                    increment.as_ptr().cast(),
                    increment.len(),
                )
            };
            if result == increment.len() as isize {
                return Ok(());
            }
            let error = io::Error::last_os_error();
            match error.kind() {
                ErrorKind::Interrupted => continue,
                // A saturated eventfd is already readable, so no wakeup is lost.
                ErrorKind::WouldBlock => return Ok(()),
                _ => unreachable!("The owned player wakeup must remain writable: {error}"),
            }
        }
    }
}

fn wait_for_player_input(socket: &UnixStream, wake: &OwnedFd) -> Result<bool> {
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
    ];
    loop {
        let result = unsafe { libc::poll(descriptors.as_mut_ptr(), descriptors.len() as _, 80) };
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
    worker: Option<thread::JoinHandle<()>>,
}

impl Player {
    pub fn new(options: PlayerOptions) -> Result<Self> {
        let (commands, receiver) = command_channel()?;
        let wake = commands.wake.clone();
        let (events, event_receiver) = async_channel::unbounded();
        let worker = thread::Builder::new()
            .name("music-player".into())
            .spawn(move || {
                if let Err(error) = run(options, receiver, &wake, &events) {
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

    fn play_url(&self, url: String, title: &str, options: PlayOptions) -> Result<u64> {
        ensure!(
            options.start.is_finite() && options.start >= 0.,
            "Invalid playback start position."
        );
        let track_id = self.next_id.fetch_add(1, Ordering::Relaxed);
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
        let (reply, response) = async_channel::bounded(1);
        self.send(PlayerCommand::Credentials {
            next: Some(credentials),
            reply,
        })?;
        Ok(response)
    }
    /// A successful response confirms extractor termination, option reset, and credential revocation.
    /// A closed response channel means the player exited without confirming cleanup.
    pub fn clear_credentials(&self) -> Result<async_channel::Receiver<Result<(), String>>> {
        let (reply, response) = async_channel::bounded(1);
        self.send(PlayerCommand::Credentials { next: None, reply })?;
        Ok(response)
    }
    pub fn pause(&self, paused: bool) -> Result<()> {
        self.send(PlayerCommand::Pause(paused))
    }
    pub fn stop(&self) -> Result<()> {
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
    fn new(options: &PlayerOptions, ytdlp: &Path, socket: &Path) -> Result<Self> {
        let core = Self(NonNull::new(unsafe { mpv_create() }).context("Could not create mpv")?);
        for (name, value) in [
            (c"terminal", "no"),
            (c"config", "no"),
            (c"load-scripts", "no"),
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
            (c"demuxer-max-bytes", "32MiB"),
            (c"demuxer-max-back-bytes", "8MiB"),
            (c"audio-client-name", "YouTube Music"),
            (c"ytdl-raw-options", "ignore-config=,no-plugin-dirs="),
        ] {
            core.set_option(name, value)?;
        }
        core.set_option(c"volume", &options.volume.clamp(0., 100.).to_string())?;
        core.set_option(c"speed", &f64::from(options.speed).to_string())?;
        core.set_option(
            c"script-opts",
            &format!(
                "ytdl_hook-ytdl_path=%{}%{}",
                ytdlp.as_os_str().len(),
                ytdlp.display()
            ),
        )?;
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
    let selector = options
        .video
        .map(|video| video.selector(options.quality))
        .unwrap_or_else(|| options.quality.selector().to_owned());
    json!(["loadfile", url, "replace", -1, {
        "force-media-title": title,
        "ytdl-format": selector,
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
        ensure!(Instant::now() < deadline, "mpv stopped responding");
        if let Some(end) = pending.iter().position(|c| *c == b'\n') {
            let line: Vec<_> = pending.drain(..=end).collect();
            return serde_json::from_slice(&line).context("Invalid mpv response");
        }
        let mut bytes = [0; 16384];
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
    identity: File,
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
    // Keep ordered plugin switches in a RAM config; mpv's option map has no CLI ordering contract.
    let config = format!(
        "--no-plugin-dirs\n--plugin-dirs {quoted_directory}\n--use-extractors '^ytmusicaccount$'\n--extractor-args ytmusicaccount:context=/proc/{}/fd/{}\n",
        std::process::id(),
        identity.as_raw_fd()
    );
    Ok(ExtractorCredentials {
        cookies,
        identity,
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
    // The stop reply and idle-active can precede the extractor's exit. end-file
    // follows the synchronous ytdl hook, including subprocess termination/reaping.
    let deadline = Instant::now() + Duration::from_secs(5);
    while !marker_seen || state.file_active {
        let value = read_message(socket, pending, deadline)?;
        marker_seen |= value["event"] == "client-message" && value["args"] == marker;
        state.message(value, events);
    }
    let mut options = json!({"ignore-config":""});
    if let Some(credentials) = &next {
        options["cookies"] = json!(format!(
            "/proc/{}/fd/{}",
            std::process::id(),
            credentials.cookies.as_raw_fd()
        ));
        options["config-locations"] = json!(format!(
            "/proc/{}/fd/{}",
            std::process::id(),
            credentials.config.as_raw_fd()
        ));
        // The identity descriptor must remain open alongside the RAM config that names it.
        let _ = &credentials.identity;
    } else {
        options["no-plugin-dirs"] = json!("");
    }
    request(
        socket,
        pending,
        json!(["set_property", "options/ytdl-raw-options", options]),
        |value| state.message(value, events),
    )?;
    *current = next;
    Ok(())
}

const REVIEWED_YTDLP: &str = "2026.08.19";

fn account_playback_supported(ytdlp: &Path) -> bool {
    std::process::Command::new(ytdlp)
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
    warned: &mut bool,
    events: &async_channel::Sender<PlayerEvent>,
) -> Option<PlaybackCredentials> {
    if supported || source.is_none() {
        return source;
    }
    if !*warned {
        *warned = true;
        let _ = events.send_blocking(PlayerEvent::Notice(format!(
            "Signed-in playback requires yt-dlp {REVIEWED_YTDLP}. Playing public streams as a guest with the installed version. Account-only songs may be unavailable."
        )));
    }
    None
}

fn run(
    options: PlayerOptions,
    commands: Receiver<PlayerCommand>,
    wake: &OwnedFd,
    events: &async_channel::Sender<PlayerEvent>,
) -> Result<()> {
    let runtime = tempfile::Builder::new()
        .prefix("yt-music-gpui-")
        .tempdir()?;
    let socket_path = runtime.path().join("mpv.sock");
    let plugin_dir = write_account_extractor(runtime.path())?;
    let ytdlp = std::env::var_os("PATH")
        .into_iter()
        .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
        .map(|path| path.join("yt-dlp"))
        .chain(std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/bin/yt-dlp")))
        .find(|path| path.is_file())
        .context("Could not find yt-dlp. Install it, then restart the app.")?;
    let account_supported = account_playback_supported(&ytdlp);
    let mut account_warned = false;
    let mut credentials = None;
    // Destroy the core before dropping the RAM credentials or private runtime.
    let core = MpvCore::new(&options, &ytdlp, &socket_path)?;
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
    socket.set_read_timeout(Some(Duration::from_millis(80)))?;
    socket.set_write_timeout(Some(Duration::from_secs(2)))?;
    let mut state = PlaybackState::default();
    let mut pending = Vec::new();
    replace_playback_credentials(
        &mut socket,
        &mut pending,
        &mut credentials,
        account_credentials(
            options.credentials,
            account_supported,
            &mut account_warned,
            events,
        ),
        &plugin_dir,
        &mut state,
        events,
    )?;
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
                Ok(PlayerCommand::Play {
                    track_id,
                    url,
                    title,
                    options,
                }) => {
                    state.invalidate_seeks();
                    state.desired = Some(track_id);
                    state.loaded = false;
                    state.paused = options.paused;
                    state.pause_target = Some(options.paused);
                    state.pause_after_load = true;
                    write_command(
                        &mut socket,
                        load_command(&url, &title, options),
                        Some(json!(track_id)),
                    )?;
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
                    let next =
                        account_credentials(next, account_supported, &mut account_warned, events);
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
                    let _ = reply.send_blocking(
                        result
                            .as_ref()
                            .map(|_| ())
                            .map_err(|error| format!("{error:#}")),
                    );
                    result?;
                }
                Ok(PlayerCommand::AttachVideo { video: next }) => {
                    let result = (|| {
                        ensure!(video.is_none(), "A video surface is already attached");
                        next.start(core.0.as_ptr())?;
                        let attached = AttachedVideo {
                            video: next,
                            ready: false,
                        };
                        request(
                            &mut socket,
                            &mut pending,
                            json!(["set_property", "vid", "auto"]),
                            |value| state.message(value, events),
                        )
                        .map_err(|_| anyhow::anyhow!("Could not enable the video track"))?;
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
                    state.invalidate_seeks();
                    state.desired = None;
                    state.loaded = false;
                    write_command(&mut socket, json!(["stop"]), None)?;
                }
                Ok(PlayerCommand::Quit) | Err(mpsc::TryRecvError::Disconnected) => {
                    let _ = write_command(&mut socket, json!(["quit"]), None);
                    return Ok(());
                }
                Err(mpsc::TryRecvError::Empty) => break,
            }
        }
        if let Some(attached) = &mut video {
            match attached.video.ready() {
                Ok(true) if !attached.ready => {
                    attached.ready = true;
                    let _ = events.send_blocking(PlayerEvent::VideoChanged);
                }
                Ok(_) => {}
                Err(error) => {
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
        }
        state.write_seek(&mut socket)?;
        if pending.contains(&b'\n') || wait_for_player_input(&socket, wake)? {
            state.read_messages(&mut socket, &mut pending, &mut buffer, events)?;
        }
        let _ = &credentials;
        ensure!(!core.is_shutdown(), "mpv stopped unexpectedly");
    }
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
    fn mismatched_extractor_uses_guest_streams_and_notifies_only_once() {
        let (events, received) = async_channel::unbounded();
        let mut warned = false;
        for _ in 0..2 {
            assert!(
                account_credentials(
                    Some(synthetic_credentials("INVENTED")),
                    false,
                    &mut warned,
                    &events
                )
                .is_none()
            );
        }
        let PlayerEvent::Notice(message) = received.try_recv().unwrap() else {
            panic!("missing fallback notice")
        };
        assert!(message.contains("2026.08.19"));
        assert!(message.contains("guest"));
        assert!(!message.contains("update yt-dlp"));
        assert!(received.is_empty());
        assert!(
            account_credentials(
                Some(synthetic_credentials("INVENTED")),
                true,
                &mut warned,
                &events
            )
            .is_some()
        );
    }

    #[test]
    fn command_wakeups_coalesce_without_losing_commands_or_socket_readiness() {
        let (commands, received) = command_channel().unwrap();
        let (mut mpv, socket) = UnixStream::pair().unwrap();
        commands.send(PlayerCommand::Stop).unwrap();
        commands.clone().send(PlayerCommand::Pause(true)).unwrap();
        assert!(!wait_for_player_input(&socket, &commands.wake).unwrap());
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
        assert!(wait_for_player_input(&socket, &commands.wake).unwrap());
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
            let result = run(PlayerOptions::default(), receiver, &wake, &events);
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
        assert_eq!(command[4]["ytdl-format"], AudioQuality::Low.selector());
        let defaults = load_command("unused", "Track", PlayOptions::default());
        assert_eq!(defaults[4]["start"], "0");
        assert_eq!(defaults[4]["pause"], "no");
        assert_eq!(defaults[4]["ytdl-format"], AudioQuality::Normal.selector());
        let video = load_command(
            "unused",
            "Track",
            PlayOptions {
                video: Some(VideoQuality::P720),
                ..options
            },
        );
        assert_eq!(
            video[4]["ytdl-format"],
            VideoQuality::P720.selector(AudioQuality::Low)
        );
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
    fn credential_changes_wait_for_stop_and_revoke_old_ram_files() {
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
        let first_path = format!(
            "/proc/self/fd/{}",
            current.as_ref().unwrap().cookies.as_raw_fd()
        );
        let first_identity = format!(
            "/proc/self/fd/{}",
            current.as_ref().unwrap().identity.as_raw_fd()
        );
        let first_config = format!(
            "/proc/self/fd/{}",
            current.as_ref().unwrap().config.as_raw_fd()
        );
        let first_files = [&first_path, &first_identity, &first_config]
            .map(|path| (path.clone(), file_identity(path)));
        let old_path = first_path.clone();
        let worker = thread::spawn(move || {
            let mut active_path = old_path;
            for replacing in [true, false] {
                for stopping in [true, false] {
                    let message = socket_message(&mut server);
                    let line = message.to_string();
                    assert!(!line.contains("INVENTED_"));
                    assert!(!line.contains("brand||user"));
                    assert!(std::path::Path::new(&active_path).exists());
                    if stopping {
                        assert_eq!(message["command"], json!(["stop"]));
                        writeln!(server, "{}", json!({"event":"start-file"})).unwrap();
                    } else {
                        assert_eq!(message["command"][0], "set_property");
                        assert_eq!(message["command"][1], "options/ytdl-raw-options");
                        let options = &message["command"][2];
                        assert_eq!(options["ignore-config"], "");
                        if replacing {
                            let config =
                                fs::read_to_string(options["config-locations"].as_str().unwrap())
                                    .unwrap();
                            assert!(config.starts_with("--no-plugin-dirs\n--plugin-dirs "));
                            assert!(config.contains("--use-extractors '^ytmusicaccount$'"));
                            assert!(!config.contains("brand||user"));
                            let path = options["cookies"].as_str().unwrap();
                            assert!(
                                fs::read_to_string(path)
                                    .unwrap()
                                    .contains("INVENTED_SECOND")
                            );
                            active_path = path.to_owned();
                        } else {
                            assert!(options.get("cookies").is_none());
                        }
                    }
                    writeln!(
                        server,
                        "{}",
                        json!({"request_id":message["request_id"],"error":"success"})
                    )
                    .unwrap();
                    if stopping {
                        reply_to_credential_barrier(&mut server);
                        server
                            .set_read_timeout(Some(Duration::from_millis(25)))
                            .unwrap();
                        let error = server.read(&mut [0]).unwrap_err();
                        assert!(matches!(
                            error.kind(),
                            ErrorKind::WouldBlock | ErrorKind::TimedOut
                        ));
                        server
                            .set_read_timeout(Some(Duration::from_secs(1)))
                            .unwrap();
                        assert!(Path::new(&active_path).exists());
                        writeln!(server, "{}", json!({"event":"end-file"})).unwrap();
                    }
                }
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
        for (path, original) in first_files {
            assert_ne!(file_identity(&path), original);
        }
        let second_path = format!(
            "/proc/self/fd/{}",
            current.as_ref().unwrap().cookies.as_raw_fd()
        );
        let second_identity = format!(
            "/proc/self/fd/{}",
            current.as_ref().unwrap().identity.as_raw_fd()
        );
        let second_config = format!(
            "/proc/self/fd/{}",
            current.as_ref().unwrap().config.as_raw_fd()
        );
        let second_files = [&second_path, &second_identity, &second_config]
            .map(|path| (path.clone(), file_identity(path)));
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
        for (path, original) in second_files {
            assert_ne!(file_identity(&path), original);
        }
        worker.join().unwrap();
    }

    #[test]
    fn credential_clear_waits_for_start_queued_after_stop_reply() {
        for start_after_marker_reply in [false, true] {
            let (mut client, mut server) = UnixStream::pair().unwrap();
            client
                .set_read_timeout(Some(Duration::from_millis(80)))
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
            let (completed, response) = mpsc::channel();
            let worker = thread::spawn(move || {
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
                completed
                    .send(result.map_err(|error| error.to_string()))
                    .unwrap();
            });
            let stop = socket_message(&mut server);
            assert_eq!(stop["command"], json!(["stop"]));
            writeln!(
                server,
                "{}",
                json!({"request_id":stop["request_id"],"error":"success"})
            )
            .unwrap();
            if !start_after_marker_reply {
                writeln!(server, "{}", json!({"event":"start-file"})).unwrap();
            }
            // Accept the old stop/options protocol too, so removing the barrier
            // reproduces premature success instead of just a protocol mismatch.
            let options_sent = loop {
                let command = socket_message(&mut server);
                let reply = json!({
                    "request_id":command["request_id"], "error":"success", "data":"test-ipc",
                });
                writeln!(server, "{reply}").unwrap();
                match command["command"][0].as_str().unwrap() {
                    "client_name" => continue,
                    "script-message-to" => {
                        if start_after_marker_reply {
                            writeln!(server, "{}", json!({"event":"start-file"})).unwrap();
                        }
                        writeln!(
                            server,
                            "{}",
                            json!({
                                "event":"client-message", "args":[command["command"][2]],
                            })
                        )
                        .unwrap();
                        break false;
                    }
                    "set_property" => {
                        assert_eq!(command["command"][1], "options/ytdl-raw-options");
                        break true;
                    }
                    other => panic!("unexpected command: {other}"),
                }
            };
            let early = response.recv_timeout(Duration::from_millis(25));
            let preserved = file_identity(&path) == original;
            let mut byte = 0_u8;
            // Check for an option write without consuming it or replying early.
            let options_waiting = unsafe {
                libc::recv(
                    server.as_raw_fd(),
                    (&mut byte as *mut u8).cast(),
                    1,
                    libc::MSG_PEEK | libc::MSG_DONTWAIT,
                )
            } > 0;
            // Always release the worker before asserting, including on the failing implementation.
            let _ = writeln!(server, "{}", json!({"event":"end-file"}));
            if !options_sent {
                let options = socket_message(&mut server);
                assert_eq!(options["command"][1], "options/ytdl-raw-options");
                writeln!(
                    server,
                    "{}",
                    json!({"request_id":options["request_id"],"error":"success"})
                )
                .unwrap();
            }
            if early.is_err() {
                response
                    .recv_timeout(Duration::from_secs(1))
                    .unwrap()
                    .unwrap();
            }
            worker.join().unwrap();
            assert!(
                matches!(early, Err(mpsc::RecvTimeoutError::Timeout)),
                "cleanup completed before the queued start-file's end-file: {early:?}"
            );
            assert!(preserved, "credentials were revoked before end-file");
            assert!(
                !options_sent && !options_waiting,
                "options changed before end-file"
            );
            assert_ne!(file_identity(&path), original);
        }
    }

    #[test]
    fn credential_clear_does_not_wait_for_a_file_that_never_started_or_already_ended() {
        for queued in [
            vec![],
            vec![json!({"request_id":1,"error":"success","data":{"playlist_entry_id":1}})],
            vec![json!({"request_id":1,"error":"invalid parameter"})],
            vec![json!({"event":"start-file"}), json!({"event":"end-file"})],
        ] {
            let (mut client, mut server) = UnixStream::pair().unwrap();
            client
                .set_read_timeout(Some(Duration::from_millis(80)))
                .unwrap();
            server
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            let worker = thread::spawn(move || {
                for messages in [queued, vec![]] {
                    let stop = socket_message(&mut server);
                    assert_eq!(stop["command"], json!(["stop"]));
                    writeln!(
                        server,
                        "{}",
                        json!({"request_id":stop["request_id"],"error":"success"})
                    )
                    .unwrap();
                    for message in messages {
                        writeln!(server, "{message}").unwrap();
                    }
                    reply_to_credential_barrier(&mut server);
                    let options = socket_message(&mut server);
                    assert_eq!(options["command"][1], "options/ytdl-raw-options");
                    writeln!(
                        server,
                        "{}",
                        json!({"request_id":options["request_id"],"error":"success"})
                    )
                    .unwrap();
                }
            });
            let (events, _) = async_channel::unbounded();
            let mut state = PlaybackState::default();
            let mut pending = Vec::new();
            let mut current = None;
            for _ in 0..2 {
                replace_playback_credentials(
                    &mut client,
                    &mut pending,
                    &mut current,
                    None,
                    Path::new("/private/synthetic-plugin-root"),
                    &mut state,
                    &events,
                )
                .unwrap();
            }
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
        for failure in ["stop", "marker", "end-file", "options"] {
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
                if matches!(failure, "end-file" | "options") {
                    reply_to_credential_barrier(&mut server);
                }
                if failure == "options" {
                    let options = socket_message(&mut server);
                    assert_eq!(options["command"][1], "options/ytdl-raw-options");
                    writeln!(server, "{}", json!({
                        "request_id":options["request_id"], "error":"INVENTED_PRIVATE_DIAGNOSTIC",
                    })).unwrap();
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
            r#"#!/usr/bin/python3
import json, os, re, signal, sys
from pathlib import Path

args = sys.argv[1:]
cookies = args[args.index('--cookies') + 1]
config = args[args.index('--config-locations') + 1]
identity = re.search(r'context=(/proc/\d+/fd/\d+)', Path(config).read_text())[1]
paths = [cookies, config, identity]
files = [open(path, 'rb') for path in paths]
assert b'INVENTED_' in files[0].read()
ready = Path(__file__).with_name('ready')
ready.with_suffix('.tmp').write_text(json.dumps({'pid': os.getpid(), 'paths': paths}))
ready.with_suffix('.tmp').replace(ready)
os.kill(os.getpid(), signal.SIGSTOP)
"#,
        )
        .unwrap();
        fs::set_permissions(&extractor, fs::Permissions::from_mode(0o700)).unwrap();
        let player = Player::new(PlayerOptions {
            credentials: Some(synthetic_credentials("INVENTED_FIRST")),
            audio_output: Some("null".into()),
            ..Default::default()
        })
        .unwrap();
        player
            .command(json!([
                "set_property", "options/script-opts", {"ytdl_hook-ytdl_path": extractor},
            ]))
            .unwrap();
        player
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
                    format!("ytdl://{}", directory.join("missing.media").display()),
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
                    format!(
                        "ytdl://{}",
                        directory.path().join("missing.media").display()
                    ),
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
        let mut child = Command::new(python)
            .args(["-B", "-"])
            .arg(plugin_dir)
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
    #[ignore = "requires installed mpv and yt-dlp; uses only private synthetic local audio"]
    fn local_seek_completion_handles_rapid_no_op_and_end_of_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("silence.wav");
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
