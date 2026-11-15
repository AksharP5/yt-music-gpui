//! MPRIS over the D-Bus session bus.

use super::{Event, Metadata, PlaybackStatus, Published, State};
use crate::{library::Repeat, player::PlaybackSpeed};
use anyhow::Result;
use std::{
    collections::HashMap,
    fmt::Write,
    sync::{Arc, Mutex, MutexGuard, PoisonError, mpsc},
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

fn status_name(status: PlaybackStatus) -> &'static str {
    match status {
        PlaybackStatus::Stopped => "Stopped",
        PlaybackStatus::Playing => "Playing",
        PlaybackStatus::Paused => "Paused",
    }
}

#[expect(
    clippy::expect_used,
    reason = "writing to a String cannot fail, and hex digits are valid object path characters"
)]
fn track_path(id: &str) -> ObjectPath<'static> {
    let prefix = "/yt_music_gpui/track/v";
    let mut path = String::with_capacity(prefix.len() + id.len() * 2);
    path.push_str(prefix);
    for byte in id.bytes() {
        write!(path, "{byte:02x}").expect("writing to a String cannot fail");
    }
    ObjectPath::try_from(path).expect("hex-encoded track IDs are valid object paths")
}

fn metadata_values(track: &Metadata) -> Properties {
    let mut values = HashMap::from([
        ("mpris:trackid", track_path(&track.id).into()),
        ("xesam:title", owned(track.title.as_str())),
        ("xesam:artist", owned(vec![track.artist.as_str()])),
        (
            "xesam:url",
            owned(format!("https://music.youtube.com/watch?v={}", track.id)),
        ),
    ]);
    if let Some(length) = track.length {
        values.insert("mpris:length", length.into());
    }
    if let Some(album) = &track.album {
        values.insert("xesam:album", owned(album.as_str()));
    }
    if let Some(url) = &track.art_url {
        values.insert("mpris:artUrl", owned(url.as_str()));
    }
    values
}

fn properties(state: &State) -> Properties {
    HashMap::from([
        ("PlaybackStatus", owned(status_name(state.playback))),
        (
            "Metadata",
            owned(
                state
                    .metadata
                    .as_ref()
                    .map(metadata_values)
                    .unwrap_or_default(),
            ),
        ),
        ("Volume", (state.volume / 100.).into()),
        ("Rate", f64::from(state.rate).into()),
        ("LoopStatus", owned(repeat_name(state.repeat))),
        ("CanPlay", state.can_play.into()),
        ("CanPause", state.can_pause.into()),
        ("CanGoNext", state.can_go_next.into()),
        ("CanGoPrevious", state.can_go_previous.into()),
        ("CanSeek", state.can_seek.into()),
    ])
}

/// Serves MPRIS until the app drops its `MediaControls`.
pub(super) fn serve(
    published: Arc<Mutex<Published>>,
    events: async_channel::Sender<Event>,
    updates: mpsc::Receiver<()>,
) -> Result<()> {
    let player = DesktopPlayer {
        published: published.clone(),
        events,
    };
    let connection = Builder::session()?
        .serve_at(PATH, DesktopRoot)?
        .serve_at(PATH, AppPlayer(player))?
        .name(NAME)?
        .build()?;
    let mut previous = properties(&State::default());
    while updates.recv().is_ok() {
        let (state, seeked) = {
            let mut published = published.lock().unwrap_or_else(PoisonError::into_inner);
            (published.state.clone(), published.seeked.take())
        };
        let properties = properties(&state);
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

#[expect(
    clippy::expect_used,
    reason = "only file descriptors fail to convert, and MPRIS values never contain them"
)]
fn owned<'a>(value: impl Into<Value<'a>>) -> OwnedValue {
    value
        .into()
        .try_into()
        .expect("MPRIS values contain no file descriptors")
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
    // A poisoned lock still holds the last published state, which is safe to read.
    fn published(&self) -> MutexGuard<'_, Published> {
        self.published
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

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
        let state = &self.published().state;
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
        let state = &self.published().state;
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
        let state = &self.published().state;
        if !state.can_seek || position < 0 {
            return Ok(());
        }
        let Some(track) = &state.metadata else {
            return Ok(());
        };
        if track_path(&track.id) != track_id || track.length.is_some_and(|length| position > length)
        {
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
        status_name(self.published().state.playback)
    }
    #[zbus(property)]
    fn metadata(&self) -> Properties {
        self.published()
            .state
            .metadata
            .as_ref()
            .map(metadata_values)
            .unwrap_or_default()
    }
    // AppPlayer dispatches setters without zbus emitting the previous property value.
    #[zbus(property)]
    fn volume(&self) -> f64 {
        self.published().state.volume / 100.
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
        f64::from(self.published().state.rate)
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
        repeat_name(self.published().state.repeat)
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
        self.published().position_at(Instant::now())
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
        self.published().state.can_play
    }
    #[zbus(property)]
    fn can_pause(&self) -> bool {
        self.published().state.can_pause
    }
    #[zbus(property)]
    fn can_go_next(&self) -> bool {
        self.published().state.can_go_next
    }
    #[zbus(property)]
    fn can_go_previous(&self) -> bool {
        self.published().state.can_go_previous
    }
    #[zbus(property)]
    fn can_seek(&self) -> bool {
        self.published().state.can_seek
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
                _ => Err(fdo::Error::UnknownProperty(name.to_owned())),
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
    use crate::media_controls::{MediaControls, tests::playing};

    fn fixture() -> (MediaControls, DesktopPlayer, mpsc::Receiver<()>) {
        let (mpris, events, updates) = MediaControls::unserved();
        let player = DesktopPlayer {
            published: mpris.published.clone(),
            events,
        };
        (mpris, player, updates)
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
        let path = track_path(&state.metadata.as_ref().unwrap().id);
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
    fn metadata_survives_backend_reload_and_stop_and_seeked_requires_current_load() {
        let (mpris, player, updates) = fixture();
        let mut state = playing();
        mpris.publish(state.clone());
        updates.try_recv().unwrap();
        let metadata = player.metadata();
        assert!(
            !properties(&state).contains_key("Position"),
            "position samples must not emit PropertiesChanged"
        );
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
