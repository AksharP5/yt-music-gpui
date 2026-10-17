use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    io::{ErrorKind, Read, Write},
    os::unix::net::UnixStream,
    os::unix::process::CommandExt,
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::{Duration, Instant},
};

pub struct PlayerOptions {
    pub cookies: Option<PathBuf>,
    pub volume: f64,
    pub audio_output: Option<String>,
}

impl Default for PlayerOptions {
    fn default() -> Self {
        Self {
            cookies: None,
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

fn run(
    options: PlayerOptions,
    commands: Receiver<PlayerCommand>,
    events: &async_channel::Sender<PlayerEvent>,
) -> Result<()> {
    let runtime = tempfile::Builder::new()
        .prefix("yt-music-gpui-")
        .tempdir()?;
    let socket_path = runtime.path().join("mpv.sock");
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
    let raw_options = if let Some(path) = &options.cookies {
        if !path.is_file() {
            bail!("The selected cookies file no longer exists.");
        }
        // mpv's string-list option uses a length-prefixed value so spaces and commas stay literal.
        let value = path.to_string_lossy();
        format!("ignore-config=,cookies=%{}%{}", value.len(), value)
    } else {
        "ignore-config=".to_owned()
    };
    cmd.arg(format!("--ytdl-raw-options={raw_options}"));
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
    write_command(&mut socket, json!(["request_log_messages", "warn"]), None)?;
    let mut state = PlaybackState::default();
    let mut pending = Vec::new();
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
                    state.last_error.clear();
                    let _ = events.send_blocking(PlayerEvent::Loading { track_id });
                    write_command(
                        &mut socket,
                        json!(["loadfile", url, "replace", -1, {"force-media-title":title}]),
                        Some(track_id),
                    )?;
                    write_command(&mut socket, json!(["set_property", "pause", false]), None)?;
                }
                Ok(PlayerCommand::Json(command)) => write_command(&mut socket, command, None)?,
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
    last_error: String,
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
                    message: format!(
                        "mpv could not load this track: {}",
                        value["error"].as_str().unwrap_or("unknown error")
                    ),
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
                            let detail = if self.last_error.is_empty() {
                                "YouTube could not provide an audio stream. Try another track or update yt-dlp."
                            } else {
                                &self.last_error
                            };
                            let _ = events.send_blocking(PlayerEvent::Error {
                                track_id: Some(track_id),
                                message: detail.to_owned(),
                            });
                        }
                        _ => {}
                    }
                    self.loaded = false;
                }
            }
            Some("log-message") => {
                let prefix = value["prefix"].as_str().unwrap_or("");
                if matches!(prefix, "ytdl_hook" | "ffmpeg" | "cplayer")
                    && let Some(message) = value["text"]
                        .as_str()
                        .filter(|m| !m.contains("https://") && !m.contains("http://"))
                {
                    self.last_error = message.trim().chars().take(500).collect();
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
