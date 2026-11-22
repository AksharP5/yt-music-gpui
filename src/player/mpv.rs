use super::stream::ResolvedStream;
use super::{PlayOptions, PlaybackSpeed, PlayerEvent, PlayerOptions};
use crate::os::IpcStream;
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    ffi::{CStr, CString, c_char, c_int, c_void},
    io::{ErrorKind, Read, Write},
    path::Path,
    ptr::NonNull,
    sync::atomic::{AtomicI64, Ordering},
    time::{Duration, Instant},
};

#[repr(C)]
struct MpvEvent {
    event_id: c_int,
    error: c_int,
    reply_userdata: u64,
    data: *mut c_void,
}

// Windows binds to the DLL at run time, so no import library is needed.
#[cfg_attr(not(windows), link(name = "mpv"))]
#[cfg_attr(windows, link(name = "libmpv-2", kind = "raw-dylib"))]
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

pub(super) struct MpvCore(pub(super) NonNull<c_void>);

const MPV_ERROR_OPTION_NOT_FOUND: c_int = -5;

impl MpvCore {
    pub(super) fn new(options: &PlayerOptions, socket: &Path) -> Result<Self> {
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
            (c"gapless-audio", "yes"),
            (c"prefetch-playlist", "yes"),
            (c"demuxer-max-bytes", "32MiB"),
            (c"demuxer-max-back-bytes", "8MiB"),
            (c"audio-client-name", "YouTube Music"),
            (c"ytdl", "no"),
        ] {
            core.set_option(name, value)?;
        }
        // mpv also starts a Lua VM for each built-in script. This player is
        // controlled only over IPC, so none of them has any use. Older mpv
        // releases lack some of these scripts, and so their switches.
        for name in [
            c"load-auto-profiles",
            c"load-commands",
            c"load-console",
            c"load-context-menu",
            c"load-positioning",
            c"load-select",
            c"load-stats-overlay",
        ] {
            core.set_optional(name, c"no")?;
        }
        // Captions as the web draws them over video: white Roboto on a 75%
        // black box per line, 2% above the bottom, sized to 16/360 of the
        // player's height (mpv sizes at a 720-pixel-high window). mpv 0.38
        // renamed sub-border-color, so older releases keep their own box color.
        for (name, value) in [
            (c"sub-font", c"Roboto"),
            (c"sub-font-size", c"38"),
            (c"sub-color", c"#FFFFFF"),
            (c"sub-border-style", c"opaque-box"),
            (c"sub-outline-color", c"#BF080808"),
            (c"sub-border-color", c"#BF080808"),
            (c"sub-outline-size", c"2"),
            (c"sub-shadow-offset", c"0"),
            (c"sub-margin-y", c"14"),
        ] {
            core.set_optional(name, value)?;
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

    /// Set an option that older mpv releases may not have.
    fn set_optional(&self, name: &CStr, value: &CStr) -> Result<()> {
        let result =
            unsafe { mpv_set_option_string(self.0.as_ptr(), name.as_ptr(), value.as_ptr()) };
        ensure!(
            result >= 0 || result == MPV_ERROR_OPTION_NOT_FOUND,
            "Could not set mpv startup option {} ({result})",
            name.to_string_lossy()
        );
        Ok(())
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

    pub(super) fn is_shutdown(&self) -> bool {
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

pub(super) fn write_command(
    socket: &mut IpcStream,
    command: Value,
    request_id: Option<Value>,
) -> Result<()> {
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

pub(super) fn write_pause(socket: &mut IpcStream, paused: bool, revision: u64) -> Result<()> {
    write_command(socket, json!(["set_property", "pause", paused]), None)?;
    // A no-op set produces no property-change; always acknowledge the latest intent.
    write_command(
        socket,
        json!(["get_property", "pause"]),
        Some(json!(format!("pause:{revision}"))),
    )
}

pub(super) fn load_command(url: &str, title: &str, gain_db: f64, options: PlayOptions) -> Value {
    // Named arguments, because mpv 0.38 inserted an index argument before the
    // options; positional ones work with only one side of that change.
    json!({"name": "loadfile", "url": url, "flags": "replace", "options": {
        "force-media-title": title,
        "ytdl": "no",
        "start": options.start.to_string(),
        "pause": if options.paused { "yes" } else { "no" },
        // A file-local option: it applies to this entry only, including when
        // mpv reaches a prepared entry on its own, and sits on top of the
        // user's volume instead of changing it. mpv before 0.38 ignores it,
        // so songs play unlevelled there.
        "volume-gain": gain_db.to_string(),
    }})
}

pub(super) fn request(
    socket: &mut IpcStream,
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

pub(super) fn read_message(
    socket: &mut IpcStream,
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

#[derive(Clone, Copy)]
pub(super) struct SeekRequest {
    pub(super) track_id: u64,
    pub(super) revision: u64,
    pub(super) seconds: f64,
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

pub(super) struct ActiveSeek {
    request: SeekRequest,
    phase: SeekPhase,
}

#[derive(Default)]
pub(super) struct PlaybackState {
    pub(super) file_active: bool,
    pub(super) desired: Option<u64>,
    pub(super) upcoming: Option<u64>,
    pub(super) current: Option<u64>,
    pub(super) entries: HashMap<u64, u64>,
    pub(super) loaded: bool,
    pub(super) seekable: bool,
    pub(super) paused: bool,
    pub(super) pause_target: Option<bool>,
    pub(super) pause_after_load: bool,
    pub(super) pause_revision: u64,
    pub(super) restarted: bool,
    pub(super) seek: Option<ActiveSeek>,
    pub(super) next_seek: Option<SeekRequest>,
    pub(super) position_observer: u64,
    /// Caption URLs of the loaded and upcoming tracks, by track ID.
    pub(super) captions: HashMap<u64, String>,
    pub(super) captions_enabled: bool,
    /// Whether the loaded file has its caption track. mpv drops it with the file.
    pub(super) captions_added: bool,
}

impl PlaybackState {
    pub(super) fn read_messages(
        &mut self,
        socket: &mut IpcStream,
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
                    self.captions_added = false;
                    self.apply_captions(socket)?;
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

    /// Remember the captions of a stream mpv is about to load as `track_id`.
    pub(super) fn load_captions(&mut self, track_id: u64, stream: &ResolvedStream) {
        let keep = [self.current, self.desired, self.upcoming];
        self.captions
            .retain(|id, _| *id != track_id && keep.contains(&Some(*id)));
        if let Some(url) = &stream.captions {
            self.captions.insert(track_id, url.clone());
        }
    }

    /// Show or hide captions on the loaded file. mpv fetches the track the
    /// first time they show, so a video watched without them never loads it.
    pub(super) fn apply_captions(&mut self, socket: &mut IpcStream) -> Result<()> {
        if self.captions_enabled
            && !self.captions_added
            && self.loaded
            && let Some(url) = self.current.and_then(|id| self.captions.get(&id))
        {
            write_command(socket, json!(["sub-add", url, "select"]), None)?;
            self.captions_added = true;
        }
        write_command(
            socket,
            json!(["set_property", "sub-visibility", self.captions_enabled]),
            None,
        )
    }

    pub(super) fn discard_upcoming(&mut self) {
        self.upcoming = None;
        // Next can adopt an entry before mpv delivers start-file. Preparing the
        // following song must preserve that entry's event identity as well.
        self.entries
            .retain(|_, id| Some(*id) == self.current || Some(*id) == self.desired);
    }

    pub(super) fn invalidate_seeks(&mut self) {
        self.seek = None;
        self.next_seek = None;
        self.restarted = false;
    }

    pub(super) fn queue_seek(
        &mut self,
        request: SeekRequest,
        events: &async_channel::Sender<PlayerEvent>,
    ) {
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

    pub(super) fn write_seek(&mut self, socket: &mut IpcStream) -> Result<()> {
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

    pub(super) fn message(&mut self, value: Value, events: &async_channel::Sender<PlayerEvent>) {
        if let Some((kind, id)) = value["request_id"]
            .as_str()
            .and_then(|id| id.split_once(':'))
            && let Ok(id) = id.parse::<u64>()
            && self.tagged_reply(kind, id, &value, events)
        {
            return;
        }
        if let Some(id) = value["request_id"].as_u64().filter(|id| *id > 0)
            && self.numbered_reply(id, &value, events)
        {
            return;
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
            Some("file-loaded")
                if let Some(track_id) = self.current.filter(|id| Some(*id) == self.desired) =>
            {
                self.loaded = true;
                let _ = events.send_blocking(PlayerEvent::Started {
                    track_id,
                    paused: self.paused,
                    captions: self.captions.contains_key(&track_id),
                });
                if self.seekable {
                    let _ = events.send_blocking(PlayerEvent::Seekable {
                        track_id,
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
            Some("property-change") => self.property_change(&value, events),
            _ => {}
        }
    }

    /// The reply to a request this player tagged `kind:id`. Returns false for
    /// a tag it never sends.
    fn tagged_reply(
        &mut self,
        kind: &str,
        id: u64,
        value: &Value,
        events: &async_channel::Sender<PlayerEvent>,
    ) -> bool {
        match (kind, id) {
            ("prepare", track_id) => {
                if self.upcoming == Some(track_id)
                    && let Some(entry) = value
                        .pointer("/data/playlist_entry_id")
                        .and_then(Value::as_u64)
                {
                    self.entries.insert(entry, track_id);
                }
            }
            ("loaded-seekable", track_id) => {
                if self.loaded && self.current == Some(track_id) && self.desired == Some(track_id) {
                    self.seekable =
                        value["error"] == "success" && value["data"].as_bool().unwrap_or(false);
                    let _ = events.send_blocking(PlayerEvent::Seekable {
                        track_id,
                        seekable: self.seekable,
                    });
                }
            }
            ("loaded-duration", track_id) => {
                if self.loaded
                    && self.current == Some(track_id)
                    && self.desired == Some(track_id)
                    && value["error"] == "success"
                    && let Some(seconds) =
                        value["data"].as_f64().filter(|seconds| seconds.is_finite())
                {
                    let _ = events.send_blocking(PlayerEvent::Duration { track_id, seconds });
                }
            }
            ("seekable", revision) => {
                if let Some(mut seek) = self.seek.take_if(|seek| {
                    seek.request.revision == revision && seek.phase == SeekPhase::Seekability
                }) {
                    if value["error"] == "success" && value["data"] == true {
                        seek.phase = SeekPhase::Command;
                        self.seek = Some(seek);
                    } else {
                        // mpv accepts a seek before demuxing it, and emits no completion if it cannot seek.
                        Self::seek_finished(
                            seek.request,
                            Err("This stream does not support seeking.".into()),
                            events,
                        );
                    }
                }
            }
            ("seek-position", revision) => {
                if value["error"] != "success"
                    && let Some(seek) = self.seek.take_if(|seek| {
                        seek.request.revision == revision && seek.phase == SeekPhase::Position
                    })
                {
                    Self::seek_finished(
                        seek.request,
                        Err("mpv could not observe the seek position.".into()),
                        events,
                    );
                }
            }
            ("pause", revision) => {
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
            }
            _ => return false,
        }
        true
    }

    /// The reply to a load, tagged with its track ID, or a seek, tagged with
    /// its revision. Returns true when it was the seek's.
    fn numbered_reply(
        &mut self,
        id: u64,
        value: &Value,
        events: &async_channel::Sender<PlayerEvent>,
    ) -> bool {
        if let Some(mut seek) = self
            .seek
            .take_if(|seek| seek.request.revision == id && seek.phase == SeekPhase::Acceptance)
        {
            if value["error"] == "success" {
                seek.phase = SeekPhase::Start;
                self.seek = Some(seek);
            } else {
                Self::seek_finished(
                    seek.request,
                    Err("mpv could not seek in this track.".into()),
                    events,
                );
            }
            return true;
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
        false
    }

    fn property_change(&mut self, value: &Value, events: &async_channel::Sender<PlayerEvent>) {
        if value["name"] == "time-pos"
            && value["id"].as_u64() == Some(self.position_observer.max(1))
            && let Some(seek) = self.seek.take_if(|seek| seek.phase == SeekPhase::Position)
        {
            // observe_property guarantees an initial notification, even for a no-op seek.
            let result = value["data"]
                .as_f64()
                .filter(|seconds| seconds.is_finite() && *seconds >= 0.)
                .ok_or_else(|| "mpv could not confirm the seek position.".into());
            Self::seek_finished(seek.request, result, events);
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
        if value["name"] == "seekable" && self.current.is_some() && self.current == self.desired {
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
            Some("demuxer-cache-time") => value["data"]
                .as_f64()
                .filter(|seconds| seconds.is_finite() && *seconds >= 0.)
                .map(|seconds| PlayerEvent::Buffered { track_id, seconds }),
            _ => None,
        };
        if let Some(event) = event {
            let _ = events.send_blocking(event);
        }
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::player::{
        AudioQuality, VideoQuality,
        stream::{ResolvedStream, StreamKey},
    };
    use std::{thread, time::SystemTime};

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
    fn synchronous_reads_use_the_remaining_deadline() {
        let (mut socket, _mpv) = IpcStream::pair().unwrap();
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

    fn seek_request(revision: u64, seconds: f64) -> SeekRequest {
        SeekRequest {
            track_id: 1,
            revision,
            seconds,
        }
    }

    /// Reads one request as a fake mpv. A fake mpv thread returns its end of
    /// the socket instead of dropping it, so it hangs up only after the test
    /// joins it: macOS refuses to set a read timeout once the peer has hung
    /// up, even with replies still unread, while mpv itself never does.
    pub(crate) fn socket_message(socket: &mut IpcStream) -> Value {
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
        let (mut socket, mut mpv) = IpcStream::pair().unwrap();
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
            (first_id, second_id, mpv)
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
        let (first_id, second_id, _mpv) = worker.join().unwrap();
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
        let (mut socket, mut mpv) = IpcStream::pair().unwrap();
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
        socket: &mut IpcStream,
        mpv: &mut IpcStream,
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
    fn seek_waits_for_restart_then_confirms_position_with_a_fresh_observer() {
        let (events, received) = async_channel::unbounded();
        let (mut socket, mut mpv) = IpcStream::pair().unwrap();
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
        let (mut socket, mut mpv) = IpcStream::pair().unwrap();
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
        let (mut socket, mut mpv) = IpcStream::pair().unwrap();
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
            let (mut socket, mut mpv) = IpcStream::pair().unwrap();
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
        let (mut socket, mut mpv) = IpcStream::pair().unwrap();
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
                Ok(PlayerEvent::Started { track_id: 1, paused, .. }) if paused == expected
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
    fn captions_load_with_their_video_only_once_shown() {
        let (events, received) = async_channel::unbounded();
        let (mut socket, mut mpv) = IpcStream::pair().unwrap();
        mpv.set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let captions = "https://www.youtube.com/api/timedtext?v=dQw4w9WgXcQ&lang=en&fmt=vtt";
        let stream = ResolvedStream {
            key: StreamKey {
                url: "https://music.youtube.com/watch?v=dQw4w9WgXcQ".into(),
                quality: AudioQuality::Normal,
                video: Some(VideoQuality::P720),
            },
            url: "https://rr1.googlevideo.com/video".into(),
            gain_db: 0.,
            expires: SystemTime::now() + Duration::from_secs(60),
            captions: Some(captions.into()),
        };
        let mut state = PlaybackState {
            desired: Some(2),
            pause_after_load: true,
            entries: HashMap::from([(5, 2)]),
            ..Default::default()
        };
        state.load_captions(2, &stream);
        let mut pending =
            b"{\"event\":\"start-file\",\"playlist_entry_id\":5}\n{\"event\":\"file-loaded\"}\n"
                .to_vec();
        state
            .read_messages(&mut socket, &mut pending, &mut [0; 1024], &events)
            .unwrap();
        assert!(matches!(
            received.try_recv(),
            Ok(PlayerEvent::Started {
                track_id: 2,
                captions: true,
                ..
            })
        ));
        // Every command written so far; the read times out once they run out.
        let mut commands = || {
            let (mut bytes, mut chunk) = (Vec::new(), [0; 4096]);
            while let Ok(count @ 1..) = mpv.read(&mut chunk) {
                bytes.extend_from_slice(&chunk[..count]);
            }
            bytes
                .split(|byte| *byte == b'\n')
                .filter(|line| !line.is_empty())
                .map(|line| serde_json::from_slice::<Value>(line).unwrap()["command"].clone())
                .collect::<Vec<_>>()
        };
        let sent = commands();
        // Captions are off: nothing is fetched, only hidden.
        assert!(
            !sent.iter().any(|command| command[0] == "sub-add"),
            "{sent:?}"
        );
        assert_eq!(
            sent.last(),
            Some(&json!(["set_property", "sub-visibility", false]))
        );

        state.captions_enabled = true;
        state.apply_captions(&mut socket).unwrap();
        state.captions_enabled = false;
        state.apply_captions(&mut socket).unwrap();
        state.captions_enabled = true;
        state.apply_captions(&mut socket).unwrap();
        assert_eq!(
            commands(),
            [
                json!(["sub-add", captions, "select"]),
                json!(["set_property", "sub-visibility", true]),
                json!(["set_property", "sub-visibility", false]),
                json!(["set_property", "sub-visibility", true]),
            ]
        );
    }

    #[test]
    fn adopting_a_loaded_stream_refreshes_duration_and_seekability() {
        let (events, received) = async_channel::unbounded();
        let (mut socket, mut mpv) = IpcStream::pair().unwrap();
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
                    paused: false,
                    ..
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
}
