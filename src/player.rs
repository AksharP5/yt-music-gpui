use crate::auth::{PlaybackCredentials, sealed_file};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    fs::{self, File},
    io::{ErrorKind, Read, Seek, Write},
    os::fd::{AsRawFd, FromRawFd},
    os::unix::fs::PermissionsExt,
    os::unix::net::UnixStream,
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::{Duration, Instant},
};

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

#[derive(Clone, Copy, Debug, Default)]
pub struct PlayOptions {
    pub quality: AudioQuality,
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
    Error {
        track_id: Option<u64>,
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
    Json(Value),
    Credentials(Option<PlaybackCredentials>),
    Stop,
    Quit,
}

pub struct Player {
    commands: Sender<PlayerCommand>,
    events: async_channel::Receiver<PlayerEvent>,
    next_id: AtomicU64,
}

impl Player {
    pub fn new(options: PlayerOptions) -> Result<Self> {
        let (commands, receiver) = mpsc::channel();
        let (events, event_receiver) = async_channel::unbounded();
        thread::Builder::new()
            .name("music-player".into())
            .spawn(move || {
                if let Err(error) = run(options, receiver, &events) {
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

    pub fn set_credentials(&self, credentials: PlaybackCredentials) -> Result<()> {
        self.send(PlayerCommand::Credentials(Some(credentials)))
    }
    pub fn clear_cookies(&self) -> Result<()> {
        self.send(PlayerCommand::Credentials(None))
    }
    pub fn pause(&self, paused: bool) -> Result<()> {
        self.send(PlayerCommand::Pause(paused))
    }
    pub fn stop(&self) -> Result<()> {
        self.send(PlayerCommand::Stop)
    }
    pub fn seek(&self, seconds: f64) -> Result<()> {
        if !seconds.is_finite() {
            bail!("Invalid seek position.");
        }
        self.command(json!(["seek", seconds.max(0.), "absolute"]))
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
            .map_err(|_| anyhow::anyhow!("The audio player stopped. Restart the app to reconnect."))
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        let _ = self.commands.send(PlayerCommand::Quit);
    }
}

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
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
        "ytdl-format": options.quality.selector(),
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
    // Track requests start at one. Zero is reserved for synchronous control changes.
    write_command(socket, command, Some(json!(0)))?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        while let Some(end) = pending.iter().position(|c| *c == b'\n') {
            let line: Vec<_> = pending.drain(..=end).collect();
            let value: Value = serde_json::from_slice(&line).context("Invalid mpv response")?;
            if value["request_id"] == 0 {
                ensure!(
                    value["error"] == "success",
                    "mpv could not update playback credentials"
                );
                return Ok(value["data"].clone());
            }
            on_event(value);
        }
        ensure!(Instant::now() < deadline, "mpv stopped responding");
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
    mut on_event: impl FnMut(Value),
) -> Result<()> {
    let next = source
        .map(|source| prepare_credentials(source, plugin_dir))
        .transpose()?;
    // Await stop before closing files an active extractor may still be using.
    request(socket, pending, json!(["stop"]), &mut on_event)?;
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
        on_event,
    )?;
    *current = next;
    Ok(())
}

fn run(
    options: PlayerOptions,
    commands: Receiver<PlayerCommand>,
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
    let mut cmd = Command::new("mpv");
    cmd.args([
        "--no-config",
        "--load-scripts=no",
        "--idle=yes",
        "--no-video",
        "--audio-display=no",
        "--no-terminal",
        "--no-osc",
        "--no-input-default-bindings",
        "--no-input-vo-keyboard",
        "--network-timeout=20",
        "--cache=yes",
        "--demuxer-max-bytes=32MiB",
        "--demuxer-max-back-bytes=8MiB",
    ])
    .arg(format!("--volume={}", options.volume.clamp(0., 100.)))
    .arg(format!("--speed={}", f64::from(options.speed)))
    .arg(format!(
        "--script-opts=ytdl_hook-ytdl_path=%{}%{}",
        ytdlp.as_os_str().len(),
        ytdlp.display()
    ))
    .arg(format!("--input-ipc-server={}", socket_path.display()))
    .arg("--audio-client-name=YouTube Music")
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null());
    if let Some(ao) = &options.audio_output {
        cmd.arg(format!("--ao={ao}"));
    }
    if options.audio_output.is_none()
        && std::path::Path::new("/usr/lib/mpv-mpris/mpris.so").exists()
    {
        cmd.arg("--script=/usr/lib/mpv-mpris/mpris.so");
    }
    let mut credentials = None;
    cmd.arg("--ytdl-raw-options=ignore-config=,no-plugin-dirs=");
    let parent = std::process::id() as libc::pid_t;
    // Linux must also reap the audio process when the GUI aborts before Drop can run.
    unsafe {
        cmd.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::getppid() != parent {
                libc::_exit(0);
            }
            Ok(())
        });
    }
    let mut child = Process(
        cmd.spawn()
            .context("Could not start mpv. Install mpv and yt-dlp, then restart the app.")?,
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut socket = loop {
        match UnixStream::connect(&socket_path) {
            Ok(socket) => break socket,
            Err(_) if Instant::now() < deadline => {
                if let Some(status) = child.0.try_wait()? {
                    bail!("mpv exited during startup ({status}).");
                }
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
        options.credentials,
        &plugin_dir,
        |value| state.message(value, events),
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
                Ok(PlayerCommand::Json(command)) => write_command(&mut socket, command, None)?,
                Ok(PlayerCommand::Credentials(next)) => {
                    state.desired = None;
                    state.loaded = false;
                    replace_playback_credentials(
                        &mut socket,
                        &mut pending,
                        &mut credentials,
                        next,
                        &plugin_dir,
                        |value| state.message(value, events),
                    )?;
                }
                Ok(PlayerCommand::Stop) => {
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
        match socket.read(&mut buffer) {
            Ok(0) => bail!("mpv closed its connection. Restart the app to reconnect."),
            Ok(size) => {
                pending.extend_from_slice(&buffer[..size]);
                while let Some(end) = pending.iter().position(|c| *c == b'\n') {
                    let line: Vec<_> = pending.drain(..=end).collect();
                    if let Ok(message) = serde_json::from_slice(&line) {
                        state.message(message, events);
                        if state.loaded && state.pause_after_load {
                            state.pause_after_load = false;
                            if let Some(paused) = state.pause_target {
                                state.pause_revision += 1;
                                write_pause(&mut socket, paused, state.pause_revision)?;
                            }
                        }
                    }
                }
                if pending.len() > 1024 * 1024 {
                    bail!("mpv returned an oversized response.");
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                ) => {}
            Err(error) => return Err(error).context("Lost the connection to mpv"),
        }
        let _ = &credentials;
        if let Some(status) = child.0.try_wait()? {
            bail!("mpv stopped unexpectedly ({status}).");
        }
    }
}

#[derive(Default)]
struct PlaybackState {
    desired: Option<u64>,
    current: Option<u64>,
    entries: HashMap<u64, u64>,
    loaded: bool,
    paused: bool,
    pause_target: Option<bool>,
    pause_after_load: bool,
    pause_revision: u64,
}

impl PlaybackState {
    fn message(&mut self, value: Value, events: &async_channel::Sender<PlayerEvent>) {
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
            Some("start-file") => {
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
            }
            Some("end-file") => {
                let track_id = value["playlist_entry_id"]
                    .as_u64()
                    .and_then(|id| self.entries.remove(&id));
                if let Some(track_id) = track_id.filter(|id| Some(*id) == self.desired) {
                    match value["reason"].as_str() {
                        Some("eof") => {
                            let _ = events.send_blocking(PlayerEvent::Ended { track_id });
                        }
                        Some("error") => {
                            let _ = events.send_blocking(PlayerEvent::Error {
                                track_id: Some(track_id),
                                message: "YouTube could not provide an audio stream. Try another track or update yt-dlp."
                                    .into(),
                            });
                        }
                        _ => {}
                    }
                    self.loaded = false;
                }
            }
            Some("property-change") => {
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
                    Some("time-pos") => value["data"]
                        .as_f64()
                        .map(|seconds| PlayerEvent::Position { track_id, seconds }),
                    Some("duration") => value["data"]
                        .as_f64()
                        .map(|seconds| PlayerEvent::Duration { track_id, seconds }),
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
    fn every_play_has_its_own_validated_options_and_track_id() {
        let (commands, received) = mpsc::channel();
        let (_, events) = async_channel::unbounded();
        let player = Player {
            commands,
            events,
            next_id: AtomicU64::new(1),
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
    fn audio_quality_selectors_use_service_classes_before_bitrate() {
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
"#])
            .arg(serde_json::to_string(&selectors).unwrap())
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

    #[test]
    fn credential_changes_wait_for_stop_and_revoke_old_ram_files() {
        use std::io::{BufRead, BufReader};

        let (mut client, server) = UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(Duration::from_millis(80)))
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
            let mut server = BufReader::new(server);
            let mut active_path = old_path;
            for replacing in [true, false] {
                for stopping in [true, false] {
                    let mut line = String::new();
                    server.read_line(&mut line).unwrap();
                    assert!(!line.contains("INVENTED_"));
                    assert!(!line.contains("brand||user"));
                    let message: Value = serde_json::from_str(&line).unwrap();
                    assert!(std::path::Path::new(&active_path).exists());
                    if stopping {
                        assert_eq!(message["command"], json!(["stop"]));
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
                    server
                        .get_mut()
                        .write_all(b"{\"request_id\":0,\"error\":\"success\"}\n")
                        .unwrap();
                }
            }
        });
        let mut pending = Vec::new();
        replace_playback_credentials(
            &mut client,
            &mut pending,
            &mut current,
            Some(synthetic_credentials("INVENTED_SECOND")),
            plugin_dir,
            |_| {},
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
            |_| {},
        )
        .unwrap();
        assert!(current.is_none());
        for (path, original) in second_files {
            assert_ne!(file_identity(&path), original);
        }
        worker.join().unwrap();
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
        player.seek(30.).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut paused = false;
        let mut sought = false;
        let mut sped_up = false;
        while Instant::now() < deadline && !(paused && sought && sped_up) {
            match player.try_recv() {
                Some(PlayerEvent::Paused { paused: true, .. }) => paused = true,
                Some(PlayerEvent::Position { seconds, .. }) if seconds >= 29. => sought = true,
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
