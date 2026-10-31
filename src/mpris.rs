use crate::{catalog::Track, library::Repeat, player::PlaybackSpeed};
use anyhow::{Context, Result};
use std::{
    collections::HashMap,
    fmt::Write,
    sync::{Arc, Mutex, mpsc},
    thread,
    time::Instant,
};
use zbus::{
    blocking::{Connection, connection::Builder},
    fdo,
    zvariant::{ObjectPath, OwnedValue, Value},
};

const PATH: &str = "/org/mpris/MediaPlayer2";
const PLAYER: &str = "org.mpris.MediaPlayer2.Player";
const NAME: &str = "org.mpris.MediaPlayer2.yt_music_gpui";

type Properties = HashMap<&'static str, OwnedValue>;

#[derive(Debug, PartialEq)]
pub enum Event {
    Next,
    Previous,
    Play,
    Pause,
    PlayPause,
    Stop,
    Seek { track_id: u64, offset: f64 },
    SetPosition { track_id: u64, seconds: f64 },
    Volume(f64),
    Rate(PlaybackSpeed),
    LoopStatus(Repeat),
    Error(String),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PlaybackStatus {
    #[default]
    Stopped,
    Playing,
    Paused,
}

impl PlaybackStatus {
    fn name(self) -> &'static str {
        match self {
            Self::Stopped => "Stopped",
            Self::Playing => "Playing",
            Self::Paused => "Paused",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Metadata {
    path: ObjectPath<'static>,
    title: String,
    artist: String,
    album: Option<String>,
    url: String,
    art_url: Option<String>,
    length: Option<i64>,
}

impl Metadata {
    pub fn new(track: &Track, duration: f64) -> Self {
        let duration = if duration.is_finite() && duration > 0. {
            Some(duration)
        } else {
            track.duration.map(|seconds| seconds as f64)
        };
        let prefix = "/yt_music_gpui/track/v";
        let mut path = String::with_capacity(prefix.len() + track.id.len() * 2);
        path.push_str(prefix);
        for byte in track.id.bytes() {
            write!(path, "{byte:02x}").expect("writing to a String cannot fail");
        }
        Self {
            path: ObjectPath::try_from(path).expect("hex-encoded track IDs are valid object paths"),
            title: track.title.clone(),
            artist: track.artist.clone(),
            album: track.album.clone(),
            url: format!("https://music.youtube.com/watch?v={}", track.id),
            art_url: track.thumbnail.clone(),
            length: duration.map(microseconds),
        }
    }

    fn values(&self) -> Properties {
        let mut values = HashMap::from([
            ("mpris:trackid", self.path.clone().into()),
            ("xesam:title", owned(self.title.as_str())),
            ("xesam:artist", owned(vec![self.artist.as_str()])),
            ("xesam:url", owned(self.url.as_str())),
        ]);
        if let Some(length) = self.length {
            values.insert("mpris:length", length.into());
        }
        if let Some(album) = &self.album {
            values.insert("xesam:album", owned(album.as_str()));
        }
        if let Some(url) = &self.art_url {
            values.insert("mpris:artUrl", owned(url.as_str()));
        }
        values
    }
}

/// Confirmed app state. Position is seconds, volume is 0..=100.
/// `advancing` is false during loading, buffering, pause, and stop.
#[derive(Clone, Debug, PartialEq)]
pub struct State {
    pub playback: PlaybackStatus,
    pub metadata: Option<Metadata>,
    pub track_id: Option<u64>,
    pub position: f64,
    pub advancing: bool,
    pub volume: f64,
    pub rate: PlaybackSpeed,
    pub repeat: Repeat,
    pub can_play: bool,
    pub can_pause: bool,
    pub can_go_next: bool,
    pub can_go_previous: bool,
    pub can_seek: bool,
}

impl Default for State {
    fn default() -> Self {
        Self {
            playback: PlaybackStatus::Stopped,
            metadata: None,
            track_id: None,
            position: 0.,
            advancing: false,
            volume: 60.,
            rate: PlaybackSpeed::default(),
            repeat: Repeat::Off,
            can_play: false,
            can_pause: false,
            can_go_next: false,
            can_go_previous: false,
            can_seek: false,
        }
    }
}

impl State {
    fn properties(&self) -> Properties {
        HashMap::from([
            ("PlaybackStatus", owned(self.playback.name())),
            (
                "Metadata",
                owned(
                    self.metadata
                        .as_ref()
                        .map(Metadata::values)
                        .unwrap_or_default(),
                ),
            ),
            ("Volume", (self.volume / 100.).into()),
            ("Rate", f64::from(self.rate).into()),
            ("LoopStatus", owned(repeat_name(self.repeat))),
            ("CanPlay", self.can_play.into()),
            ("CanPause", self.can_pause.into()),
            ("CanGoNext", self.can_go_next.into()),
            ("CanGoPrevious", self.can_go_previous.into()),
            ("CanSeek", self.can_seek.into()),
        ])
    }

    fn same_properties(&self, other: &Self) -> bool {
        self.playback == other.playback
            && self.metadata == other.metadata
            && self.volume == other.volume
            && self.rate == other.rate
            && self.repeat == other.repeat
            && self.can_play == other.can_play
            && self.can_pause == other.can_pause
            && self.can_go_next == other.can_go_next
            && self.can_go_previous == other.can_go_previous
            && self.can_seek == other.can_seek
    }
}

struct Published {
    state: State,
    sampled_at: Instant,
    seeked: Option<(u64, i64)>,
}

impl Published {
    fn position_at(&self, now: Instant) -> i64 {
        let elapsed = if self.state.advancing && self.state.playback == PlaybackStatus::Playing {
            now.saturating_duration_since(self.sampled_at).as_secs_f64()
                * f64::from(self.state.rate)
        } else {
            0.
        };
        let position = microseconds(self.state.position + elapsed);
        self.state
            .metadata
            .as_ref()
            .and_then(|track| track.length)
            .map_or(position, |length| position.min(length))
    }
}

/// Owns the desktop service, not playback. Keep it on the app and consume events
/// on the app thread. Dropping it closes the service without blocking the UI.
pub struct Mpris {
    published: Arc<Mutex<Published>>,
    wake: mpsc::SyncSender<()>,
    events: async_channel::Receiver<Event>,
}

impl Mpris {
    pub fn new() -> Result<Self> {
        let published = Arc::new(Mutex::new(Published {
            state: State::default(),
            sampled_at: Instant::now(),
            seeked: None,
        }));
        let (wake, updates) = mpsc::sync_channel(1);
        let (events, receiver) = async_channel::bounded(64);
        let player = DesktopPlayer {
            published: published.clone(),
            events: events.clone(),
        };
        thread::Builder::new()
            .name("music-mpris".into())
            .spawn(move || {
                if let Err(error) = serve(player, updates) {
                    let _ = events.try_send(Event::Error(format!(
                        "Desktop media controls unavailable: {error:#}"
                    )));
                }
            })
            .context("Could not start desktop media controls")?;
        Ok(Self {
            published,
            wake,
            events: receiver,
        })
    }

    pub fn events(&self) -> async_channel::Receiver<Event> {
        self.events.clone()
    }

    /// Frequent clock samples do not allocate metadata or wake the D-Bus worker.
    pub fn position_sample(&self, track_id: u64, seconds: f64) {
        if !seconds.is_finite() || seconds < 0. {
            return;
        }
        let mut published = self.published.lock().unwrap();
        if published.state.track_id == Some(track_id) && published.state.position != seconds {
            published.state.position = seconds;
            published.sampled_at = Instant::now();
        }
    }

    pub fn publish(&self, mut state: State) {
        state.position = if state.position.is_finite() {
            state.position.max(0.)
        } else {
            0.
        };
        state.volume = if state.volume.is_finite() {
            state.volume.clamp(0., 100.)
        } else {
            0.
        };
        let mut published = self.published.lock().unwrap();
        let old = &published.state;
        let properties_changed = !old.same_properties(&state);
        if old.position != state.position
            || old.advancing != state.advancing
            || old.playback != state.playback
            || old.rate != state.rate
            || old.track_id != state.track_id
        {
            published.sampled_at = Instant::now();
        }
        published.state = state;
        drop(published);
        if properties_changed {
            let _ = self.wake.try_send(());
        }
    }

    /// Call after a confirmed seek, after publishing its resulting state.
    pub fn seeked(&self, track_id: u64, seconds: f64) {
        if !seconds.is_finite() || seconds < 0. {
            return;
        }
        let mut published = self.published.lock().unwrap();
        if published.state.track_id != Some(track_id) {
            return;
        }
        published.seeked = Some((track_id, microseconds(seconds)));
        drop(published);
        let _ = self.wake.try_send(());
    }
}

fn serve(player: DesktopPlayer, updates: mpsc::Receiver<()>) -> Result<()> {
    let published = player.published.clone();
    let connection = Builder::session()?
        .serve_at(PATH, DesktopRoot)?
        .serve_at(PATH, AppPlayer(player))?
        .name(NAME)?
        .build()?;
    let mut previous = State::default().properties();
    while updates.recv().is_ok() {
        let (state, seeked) = {
            let mut published = published.lock().unwrap();
            (published.state.clone(), published.seeked.take())
        };
        let properties = state.properties();
        let changed: HashMap<_, _> = properties
            .iter()
            .filter(|(key, value)| previous.get(*key) != Some(*value))
            .map(|(key, value)| (*key, value))
            .collect();
        if !changed.is_empty() {
            connection.emit_signal(
                None::<&str>,
                PATH,
                "org.freedesktop.DBus.Properties",
                "PropertiesChanged",
                &(PLAYER, changed, Vec::<&str>::new()),
            )?;
        }
        previous = properties;
        if let Some((track_id, position)) = seeked
            && state.track_id == Some(track_id)
        {
            emit_seeked(&connection, position)?;
        }
    }
    Ok(())
}

fn emit_seeked(connection: &Connection, position: i64) -> zbus::Result<()> {
    connection.emit_signal(None::<&str>, PATH, PLAYER, "Seeked", &(position,))
}

fn owned<'a>(value: impl Into<Value<'a>>) -> OwnedValue {
    value
        .into()
        .try_into()
        .expect("MPRIS values contain no file descriptors")
}

fn microseconds(seconds: f64) -> i64 {
    (seconds.max(0.) * 1_000_000.) as i64
}

fn repeat_name(repeat: Repeat) -> &'static str {
    match repeat {
        Repeat::Off => "None",
        Repeat::One => "Track",
        Repeat::All => "Playlist",
    }
}

fn unsupported(operation: &str) -> fdo::Error {
    fdo::Error::NotSupported(format!("{operation} is not supported"))
}

struct DesktopRoot;

#[zbus::interface(name = "org.mpris.MediaPlayer2")]
impl DesktopRoot {
    fn raise(&self) -> fdo::Result<()> {
        Err(unsupported("Raise"))
    }
    fn quit(&self) -> fdo::Result<()> {
        Err(unsupported("Quit"))
    }
    #[zbus(property(emits_changed_signal = "const"))]
    fn can_quit(&self) -> bool {
        false
    }
    #[zbus(property(emits_changed_signal = "const"))]
    fn can_raise(&self) -> bool {
        false
    }
    #[zbus(property(emits_changed_signal = "const"))]
    fn has_track_list(&self) -> bool {
        false
    }
    #[zbus(property(emits_changed_signal = "const"))]
    fn identity(&self) -> &str {
        "YouTube Music"
    }
    #[zbus(property(emits_changed_signal = "const"))]
    fn desktop_entry(&self) -> &str {
        "yt-music-gpui"
    }
    #[zbus(property(emits_changed_signal = "const"))]
    fn supported_uri_schemes(&self) -> Vec<&str> {
        Vec::new()
    }
    #[zbus(property(emits_changed_signal = "const"))]
    fn supported_mime_types(&self) -> Vec<&str> {
        Vec::new()
    }
}

struct DesktopPlayer {
    published: Arc<Mutex<Published>>,
    events: async_channel::Sender<Event>,
}

impl DesktopPlayer {
    fn send(&self, event: Event) -> fdo::Result<()> {
        self.events.try_send(event).map_err(|error| match error {
            async_channel::TrySendError::Full(_) => {
                fdo::Error::LimitsExceeded("Too many pending media commands".into())
            }
            async_channel::TrySendError::Closed(_) => {
                fdo::Error::Failed("The music player has stopped".into())
            }
        })
    }
}

#[zbus::interface(name = "org.mpris.MediaPlayer2.Player")]
impl DesktopPlayer {
    fn next(&self) -> fdo::Result<()> {
        if !self.can_go_next() {
            return Ok(());
        }
        self.send(Event::Next)
    }
    fn previous(&self) -> fdo::Result<()> {
        if !self.can_go_previous() {
            return Ok(());
        }
        self.send(Event::Previous)
    }
    fn play(&self) -> fdo::Result<()> {
        if !self.can_play() {
            return Ok(());
        }
        self.send(Event::Play)
    }
    fn pause(&self) -> fdo::Result<()> {
        if !self.can_pause() {
            return Ok(());
        }
        self.send(Event::Pause)
    }
    fn play_pause(&self) -> fdo::Result<()> {
        let state = &self.published.lock().unwrap().state;
        let allowed = if state.playback == PlaybackStatus::Playing {
            state.can_pause
        } else {
            state.can_play
        };
        if !allowed {
            return Ok(());
        }
        self.send(Event::PlayPause)
    }
    fn stop(&self) -> fdo::Result<()> {
        self.send(Event::Stop)
    }
    fn seek(&self, offset: i64) -> fdo::Result<()> {
        let state = &self.published.lock().unwrap().state;
        if !state.can_seek {
            return Ok(());
        }
        let Some(track_id) = state.track_id else {
            return Ok(());
        };
        self.send(Event::Seek {
            track_id,
            offset: offset as f64 / 1_000_000.,
        })
    }
    fn set_position(&self, track_id: ObjectPath<'_>, position: i64) -> fdo::Result<()> {
        let state = &self.published.lock().unwrap().state;
        if !state.can_seek || position < 0 {
            return Ok(());
        }
        let Some(track) = &state.metadata else {
            return Ok(());
        };
        if track.path != track_id || track.length.is_some_and(|length| position > length) {
            return Ok(());
        }
        let Some(track_id) = state.track_id else {
            return Ok(());
        };
        self.send(Event::SetPosition {
            track_id,
            seconds: position as f64 / 1_000_000.,
        })
    }
    fn open_uri(&self, _uri: &str) -> fdo::Result<()> {
        Err(unsupported("OpenUri"))
    }
    #[zbus(signal)]
    async fn seeked(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        position: i64,
    ) -> zbus::Result<()>;

    #[zbus(property)]
    fn playback_status(&self) -> &'static str {
        self.published.lock().unwrap().state.playback.name()
    }
    #[zbus(property)]
    fn metadata(&self) -> Properties {
        self.published
            .lock()
            .unwrap()
            .state
            .metadata
            .as_ref()
            .map(Metadata::values)
            .unwrap_or_default()
    }
    // AppPlayer dispatches setters without zbus emitting the previous property value.
    #[zbus(property)]
    fn volume(&self) -> f64 {
        self.published.lock().unwrap().state.volume / 100.
    }
    #[zbus(property)]
    fn set_volume(&self, volume: f64) -> fdo::Result<()> {
        if !volume.is_finite() {
            return Err(fdo::Error::InvalidArgs("Volume must be finite".into()));
        }
        self.send(Event::Volume(volume.clamp(0., 1.) * 100.))
    }
    #[zbus(property)]
    fn rate(&self) -> f64 {
        f64::from(self.published.lock().unwrap().state.rate)
    }
    #[zbus(property)]
    fn set_rate(&self, rate: f64) -> fdo::Result<()> {
        if rate == 0. {
            return self.pause();
        }
        let rate =
            PlaybackSpeed::try_from(rate).map_err(|error| fdo::Error::InvalidArgs(error.into()))?;
        self.send(Event::Rate(rate))
    }
    #[zbus(property)]
    fn loop_status(&self) -> &'static str {
        repeat_name(self.published.lock().unwrap().state.repeat)
    }
    #[zbus(property)]
    fn set_loop_status(&self, status: &str) -> fdo::Result<()> {
        let repeat = match status {
            "None" => Repeat::Off,
            "Track" => Repeat::One,
            "Playlist" => Repeat::All,
            _ => {
                return Err(fdo::Error::InvalidArgs(
                    "LoopStatus must be None, Track, or Playlist".into(),
                ));
            }
        };
        self.send(Event::LoopStatus(repeat))
    }
    #[zbus(property(emits_changed_signal = "false"))]
    fn shuffle(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn set_shuffle(&self, shuffle: bool) -> fdo::Result<()> {
        if shuffle {
            return Err(unsupported("Persistent shuffle"));
        }
        Ok(())
    }
    #[zbus(property(emits_changed_signal = "false"))]
    fn position(&self) -> i64 {
        self.published.lock().unwrap().position_at(Instant::now())
    }
    #[zbus(property(emits_changed_signal = "const"))]
    fn minimum_rate(&self) -> f64 {
        0.25
    }
    #[zbus(property(emits_changed_signal = "const"))]
    fn maximum_rate(&self) -> f64 {
        2.
    }
    #[zbus(property(emits_changed_signal = "const"))]
    fn can_control(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn can_play(&self) -> bool {
        self.published.lock().unwrap().state.can_play
    }
    #[zbus(property)]
    fn can_pause(&self) -> bool {
        self.published.lock().unwrap().state.can_pause
    }
    #[zbus(property)]
    fn can_go_next(&self) -> bool {
        self.published.lock().unwrap().state.can_go_next
    }
    #[zbus(property)]
    fn can_go_previous(&self) -> bool {
        self.published.lock().unwrap().state.can_go_previous
    }
    #[zbus(property)]
    fn can_seek(&self) -> bool {
        self.published.lock().unwrap().state.can_seek
    }
}

// zbus automatically emits a property signal after each setter. These setters
// only enqueue intent. Bypass that automatic emission so publish() remains the
// single owner of confirmed, deduplicated signals, with truthful introspection.
struct AppPlayer(DesktopPlayer);

#[zbus::export::async_trait::async_trait]
impl zbus::object_server::Interface for AppPlayer {
    fn name() -> zbus::names::InterfaceName<'static> {
        <DesktopPlayer as zbus::object_server::Interface>::name()
    }

    fn spawn_tasks_for_methods(&self) -> bool {
        false
    }

    async fn get(
        &self,
        name: &str,
        server: &zbus::ObjectServer,
        connection: &zbus::Connection,
        header: Option<&zbus::message::Header<'_>>,
        emitter: &zbus::object_server::SignalEmitter<'_>,
    ) -> Option<fdo::Result<OwnedValue>> {
        self.0.get(name, server, connection, header, emitter).await
    }

    async fn get_all(
        &self,
        server: &zbus::ObjectServer,
        connection: &zbus::Connection,
        header: Option<&zbus::message::Header<'_>>,
        emitter: &zbus::object_server::SignalEmitter<'_>,
    ) -> fdo::Result<HashMap<String, OwnedValue>> {
        self.0.get_all(server, connection, header, emitter).await
    }

    fn set<'call>(
        &'call self,
        name: &'call str,
        value: &'call Value<'_>,
        server: &'call zbus::ObjectServer,
        connection: &'call zbus::Connection,
        header: Option<&'call zbus::message::Header<'_>>,
        emitter: &'call zbus::object_server::SignalEmitter<'_>,
    ) -> zbus::object_server::DispatchResult2<'call> {
        if !matches!(name, "Volume" | "Rate" | "LoopStatus" | "Shuffle") {
            return self.0.set(name, value, server, connection, header, emitter);
        }
        zbus::object_server::DispatchResult2::Async(Box::pin(async move {
            let invalid = |error: zbus::zvariant::Error| fdo::Error::InvalidArgs(error.to_string());
            match name {
                "Volume" => self.0.set_volume(f64::try_from(value).map_err(invalid)?),
                "Rate" => self.0.set_rate(f64::try_from(value).map_err(invalid)?),
                "LoopStatus" => self
                    .0
                    .set_loop_status(<&str>::try_from(value).map_err(invalid)?),
                "Shuffle" => self.0.set_shuffle(bool::try_from(value).map_err(invalid)?),
                _ => unreachable!(),
            }
        }))
    }

    async fn set_mut(
        &mut self,
        _name: &str,
        _value: &Value<'_>,
        _server: &zbus::ObjectServer,
        _connection: &zbus::Connection,
        _header: Option<&zbus::message::Header<'_>>,
        _emitter: &zbus::object_server::SignalEmitter<'_>,
    ) -> Option<fdo::Result<()>> {
        None
    }

    fn call<'call>(
        &'call self,
        server: &'call zbus::ObjectServer,
        connection: &'call zbus::Connection,
        message: &'call zbus::message::Message,
        name: zbus::names::MemberName<'call>,
    ) -> zbus::object_server::DispatchResult2<'call> {
        self.0.call(server, connection, message, name)
    }

    fn call_mut<'call>(
        &'call mut self,
        _server: &'call zbus::ObjectServer,
        _connection: &'call zbus::Connection,
        _message: &'call zbus::message::Message,
        _name: zbus::names::MemberName<'call>,
    ) -> zbus::object_server::DispatchResult2<'call> {
        zbus::object_server::DispatchResult2::NotFound
    }

    fn introspect_to_writer(&self, writer: &mut dyn std::fmt::Write, level: usize) {
        self.0.introspect_to_writer(writer, level);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn fixture() -> (Mpris, DesktopPlayer, mpsc::Receiver<()>) {
        let published = Arc::new(Mutex::new(Published {
            state: State::default(),
            sampled_at: Instant::now(),
            seeked: None,
        }));
        let (wake, updates) = mpsc::sync_channel(1);
        let (events, receiver) = async_channel::bounded(64);
        let mpris = Mpris {
            published: published.clone(),
            wake,
            events: receiver,
        };
        let player = DesktopPlayer { published, events };
        (mpris, player, updates)
    }

    fn playing() -> State {
        let track = Track {
            id: "abcdefghijk".into(),
            title: "Song".into(),
            artist: "Artist".into(),
            album: Some("Album".into()),
            duration: Some(60),
            thumbnail: None,
            details: Default::default(),
        };
        State {
            playback: PlaybackStatus::Playing,
            metadata: Some(Metadata::new(&track, 60.)),
            track_id: Some(7),
            position: 10.,
            advancing: true,
            can_play: true,
            can_pause: true,
            can_go_next: true,
            can_go_previous: true,
            can_seek: true,
            ..Default::default()
        }
    }

    #[test]
    fn media_commands_reach_the_app_without_mutating_its_state() {
        let (mpris, player, _) = fixture();
        player.next().unwrap();
        assert!(mpris.events.try_recv().is_err());
        let state = playing();
        mpris.publish(state.clone());
        player.next().unwrap();
        player.previous().unwrap();
        player.pause().unwrap();
        player.play().unwrap();
        player.play_pause().unwrap();
        player.stop().unwrap();
        for expected in [
            Event::Next,
            Event::Previous,
            Event::Pause,
            Event::Play,
            Event::PlayPause,
            Event::Stop,
        ] {
            assert_eq!(mpris.events.try_recv().unwrap(), expected);
        }
        assert_eq!(mpris.published.lock().unwrap().state, state);
    }

    #[test]
    fn seek_requests_are_track_bound_and_stale_or_out_of_range_positions_are_ignored() {
        let (mpris, player, _) = fixture();
        let state = playing();
        let path = state.metadata.as_ref().unwrap().path.clone();
        mpris.publish(state.clone());
        player.seek(-3_500_000).unwrap();
        assert_eq!(
            mpris.events.try_recv().unwrap(),
            Event::Seek {
                track_id: 7,
                offset: -3.5
            }
        );
        player
            .set_position(ObjectPath::try_from("/old_track").unwrap(), 10_000_000)
            .unwrap();
        player.set_position(path.clone(), -1).unwrap();
        player.set_position(path.clone(), 60_000_001).unwrap();
        assert!(mpris.events.try_recv().is_err());
        player.set_position(path.clone(), 25_000_000).unwrap();
        assert_eq!(
            mpris.events.try_recv().unwrap(),
            Event::SetPosition {
                track_id: 7,
                seconds: 25.
            }
        );
        mpris.publish(State {
            can_seek: false,
            ..state
        });
        player.seek(1_000_000).unwrap();
        player.set_position(path, 0).unwrap();
        assert!(mpris.events.try_recv().is_err());
    }

    #[test]
    fn property_intents_are_validated_and_unsupported_modes_are_explicit() {
        let (mpris, player, _) = fixture();
        mpris.publish(playing());
        player.set_volume(-1.).unwrap();
        player.set_volume(2.).unwrap();
        player.set_rate(0.).unwrap();
        player.set_rate(1.5).unwrap();
        player.set_loop_status("Track").unwrap();
        for expected in [
            Event::Volume(0.),
            Event::Volume(100.),
            Event::Pause,
            Event::Rate(PlaybackSpeed::try_from(1.5).unwrap()),
            Event::LoopStatus(Repeat::One),
        ] {
            assert_eq!(mpris.events.try_recv().unwrap(), expected);
        }
        for invalid in [f64::NAN, f64::INFINITY] {
            assert!(player.set_volume(invalid).is_err());
        }
        for invalid in [f64::NAN, f64::INFINITY, -1., 2.1] {
            assert!(player.set_rate(invalid).is_err());
        }
        assert!(player.set_loop_status("invalid").is_err());
        assert!(matches!(
            player.set_shuffle(true),
            Err(fdo::Error::NotSupported(_))
        ));
        assert!(matches!(
            player.open_uri("file:///unused"),
            Err(fdo::Error::NotSupported(_))
        ));
        player.set_shuffle(false).unwrap();
        assert!(mpris.events.try_recv().is_err());
        assert_eq!(player.volume(), 0.6);
        assert_eq!(player.rate(), 1.);
        assert_eq!(player.loop_status(), "None");
    }

    #[test]
    fn position_projects_at_rate_and_freezes_during_pause_buffering_and_stop() {
        let (mpris, _, updates) = fixture();
        let mut state = playing();
        state.rate = PlaybackSpeed::try_from(1.5).unwrap();
        mpris.publish(state.clone());
        updates.try_recv().unwrap();
        let sampled_at = mpris.published.lock().unwrap().sampled_at;
        assert_eq!(
            mpris
                .published
                .lock()
                .unwrap()
                .position_at(sampled_at + Duration::from_secs(2)),
            13_000_000
        );
        mpris.publish(state.clone());
        assert_eq!(mpris.published.lock().unwrap().sampled_at, sampled_at);
        assert!(updates.try_recv().is_err());
        state.position = 11.;
        mpris.publish(state.clone());
        assert!(
            updates.try_recv().is_err(),
            "position samples must not emit PropertiesChanged"
        );
        assert!(!state.properties().contains_key("Position"));
        for playback in [
            PlaybackStatus::Playing,
            PlaybackStatus::Paused,
            PlaybackStatus::Stopped,
        ] {
            state.playback = playback;
            state.advancing = false;
            mpris.publish(state.clone());
            let published = mpris.published.lock().unwrap();
            assert_eq!(
                published.position_at(published.sampled_at + Duration::from_secs(20)),
                11_000_000
            );
        }
        state.playback = PlaybackStatus::Playing;
        state.advancing = true;
        mpris.publish(state);
        let published = mpris.published.lock().unwrap();
        assert_eq!(
            published.position_at(published.sampled_at + Duration::from_secs(60)),
            60_000_000
        );
    }

    #[test]
    fn metadata_survives_backend_reload_and_stop_and_seeked_requires_current_load() {
        let (mpris, player, updates) = fixture();
        let mut state = playing();
        mpris.publish(state.clone());
        updates.try_recv().unwrap();
        let metadata = player.metadata();
        assert!(<&ObjectPath<'_>>::try_from(metadata.get("mpris:trackid").unwrap()).is_ok());
        assert_eq!(
            i64::try_from(metadata.get("mpris:length").unwrap()).unwrap(),
            60_000_000
        );
        state.track_id = Some(8);
        mpris.publish(state.clone());
        assert_eq!(player.metadata(), metadata);
        assert!(updates.try_recv().is_err());
        mpris.seeked(7, 20.);
        assert!(updates.try_recv().is_err());
        state.position = 20.;
        mpris.publish(state.clone());
        mpris.seeked(8, 20.);
        updates.try_recv().unwrap();
        assert_eq!(
            mpris.published.lock().unwrap().seeked.take(),
            Some((8, 20_000_000))
        );
        state.playback = PlaybackStatus::Stopped;
        state.track_id = None;
        state.advancing = false;
        state.position = 0.;
        state.can_seek = false;
        mpris.publish(state);
        assert_eq!(player.metadata(), metadata);
        assert_eq!(player.position(), 0);
        player.play().unwrap();
        assert_eq!(mpris.events.try_recv().unwrap(), Event::Play);
    }
}
