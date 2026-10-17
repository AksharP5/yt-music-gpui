use crate::auth::{PlaybackCredentials, sealed_file};
use anyhow::{Context, Result, bail, ensure};
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

pub struct PlayerOptions {
    pub credentials: Option<PlaybackCredentials>,
    pub volume: f64,
    pub audio_output: Option<String>,
}

impl Default for PlayerOptions {
    fn default() -> Self {
        Self {
            credentials: None,
            volume: 60.,
            audio_output: None,
        }
    }
}

#[derive(Debug)]
pub enum PlayerEvent {
    Loading {
        track_id: u64,
    },
    Started {
        track_id: u64,
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
}

enum PlayerCommand {
    Play {
        track_id: u64,
        url: String,
        title: String,
    },
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

    pub fn play(&self, video_id: &str, title: &str) -> Result<u64> {
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
        )
    }

    fn play_url(&self, url: String, title: &str) -> Result<u64> {
        let track_id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.send(PlayerCommand::Play {
            track_id,
            url,
            title: title.to_owned(),
        })?;
        Ok(track_id)
    }

    pub fn set_credentials(&self, credentials: PlaybackCredentials) -> Result<()> {
        self.send(PlayerCommand::Credentials(Some(credentials)))
    }
    pub fn clear_cookies(&self) -> Result<()> {
        self.send(PlayerCommand::Credentials(None))
    }
    pub fn toggle_pause(&self) -> Result<()> {
        self.command(json!(["cycle", "pause"]))
    }
    #[cfg(test)]
    pub fn pause(&self, paused: bool) -> Result<()> {
        self.command(json!(["set_property", "pause", paused]))
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

fn write_command(socket: &mut UnixStream, command: Value, request_id: Option<u64>) -> Result<()> {
    let mut message = json!({ "command": command });
    if let Some(id) = request_id {
        message["request_id"] = json!(id);
    }
    let mut bytes = serde_json::to_vec(&message)?;
    bytes.push(b'\n');
    socket
        .write_all(&bytes)
        .context("Could not send the command to mpv")
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
    write_command(socket, command, Some(0))?;
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
        "--ytdl-format=bestaudio/best",
        "--network-timeout=20",
        "--cache=yes",
        "--demuxer-max-bytes=32MiB",
        "--demuxer-max-back-bytes=8MiB",
    ])
    .arg(format!("--volume={}", options.volume.clamp(0., 100.)))
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
                }) => {
                    state.desired = Some(track_id);
                    state.loaded = false;
                    let _ = events.send_blocking(PlayerEvent::Loading { track_id });
                    write_command(
                        &mut socket,
                        json!(["loadfile", url, "replace", -1, {"force-media-title":title}]),
                        Some(track_id),
                    )?;
                    write_command(&mut socket, json!(["set_property", "pause", false]), None)?;
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
}

impl PlaybackState {
    fn message(&mut self, value: Value, events: &async_channel::Sender<PlayerEvent>) {
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
        state.message(
            json!({"event":"end-file","playlist_entry_id":10,"reason":"eof"}),
            &tx,
        );
        assert!(rx.try_recv().is_err());
        state.message(json!({"event":"start-file","playlist_entry_id":11}), &tx);
        state.message(json!({"event":"file-loaded"}), &tx);
        assert!(matches!(
            rx.try_recv(),
            Ok(PlayerEvent::Started { track_id: 2 })
        ));
        state.message(
            json!({"event":"end-file","playlist_entry_id":11,"reason":"eof"}),
            &tx,
        );
        assert!(matches!(
            rx.try_recv(),
            Ok(PlayerEvent::Ended { track_id: 2 })
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
            .play("csILUDfmtqM", "Khruangbin - Hold Me Up")
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(55);
        let mut started = false;
        let mut progressed = false;
        while Instant::now() < deadline {
            if let Some(event) = player.try_recv() {
                println!("{event:?}");
                match event {
                    PlayerEvent::Started { track_id } if track_id == id => started = true,
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
        player.pause(true).unwrap();
        player.seek(30.).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut paused = false;
        let mut sought = false;
        while Instant::now() < deadline && !(paused && sought) {
            match player.try_recv() {
                Some(PlayerEvent::Paused { paused: true, .. }) => paused = true,
                Some(PlayerEvent::Position { seconds, .. }) if seconds >= 29. => sought = true,
                _ => thread::sleep(Duration::from_millis(50)),
            }
        }
        assert!(paused && sought, "pause and seek must reach mpv");
    }
}
