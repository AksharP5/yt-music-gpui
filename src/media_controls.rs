//! Desktop media controls: the app publishes now-playing state and receives
//! commands from media keys and the desktop. Each platform serves them its own
//! way; Linux uses MPRIS over D-Bus. macOS has no backend yet, so only
//! Linux reads the published state.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use crate::{catalog::Track, library::Repeat, player::PlaybackSpeed};
use anyhow::{Context, Result};
use std::{
    sync::{Arc, Mutex, PoisonError, mpsc},
    thread,
    time::Instant,
};

#[cfg(target_os = "linux")]
mod mpris;
#[cfg(target_os = "linux")]
use mpris::serve;

#[cfg(not(target_os = "linux"))]
fn serve(
    _: Arc<Mutex<Published>>,
    _: async_channel::Sender<Event>,
    _: mpsc::Receiver<()>,
) -> Result<()> {
    Ok(())
}

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
    Shuffle(bool),
    Error(String),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PlaybackStatus {
    #[default]
    Stopped,
    Playing,
    Paused,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Metadata {
    id: String,
    title: String,
    artist: String,
    album: Option<String>,
    art_url: Option<String>,
    /// Microseconds.
    length: Option<i64>,
}

impl Metadata {
    /// `duration` is in seconds; zero or less means unknown.
    pub fn new(track: &Track, duration: f64) -> Self {
        Self {
            id: track.id.clone(),
            title: track.title.clone(),
            artist: track.artist.clone(),
            album: track.album.clone(),
            art_url: track.thumbnail.clone(),
            length: (duration.is_finite() && duration > 0.).then(|| microseconds(duration)),
        }
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
    /// None when the current queue cannot restore its original order.
    pub shuffle: Option<bool>,
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
            shuffle: None,
            can_play: false,
            can_pause: false,
            can_go_next: false,
            can_go_previous: false,
            can_seek: false,
        }
    }
}

impl State {
    /// Everything except the position clock, which the desktop reads on demand.
    fn same_properties(&self, other: &Self) -> bool {
        self.playback == other.playback
            && self.metadata == other.metadata
            && self.volume == other.volume
            && self.rate == other.rate
            && self.repeat == other.repeat
            && self.shuffle == other.shuffle
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
pub struct MediaControls {
    published: Arc<Mutex<Published>>,
    wake: mpsc::SyncSender<()>,
    events: async_channel::Receiver<Event>,
}

impl MediaControls {
    pub fn new() -> Result<Self> {
        let (controls, events, updates) = Self::unserved();
        let published = controls.published.clone();
        thread::Builder::new()
            .name("music-media".into())
            .spawn(move || {
                if let Err(error) = serve(published, events.clone(), updates) {
                    let _ = events.try_send(Event::Error(format!(
                        "Desktop media controls unavailable: {error:#}"
                    )));
                }
            })
            .context("Could not start desktop media controls")?;
        Ok(controls)
    }

    /// The controls and the ends a desktop backend serves from.
    fn unserved() -> (Self, async_channel::Sender<Event>, mpsc::Receiver<()>) {
        let (wake, updates) = mpsc::sync_channel(1);
        let (events, receiver) = async_channel::bounded(64);
        let controls = Self {
            published: Arc::new(Mutex::new(Published {
                state: State::default(),
                sampled_at: Instant::now(),
                seeked: None,
            })),
            wake,
            events: receiver,
        };
        (controls, events, updates)
    }

    pub fn events(&self) -> async_channel::Receiver<Event> {
        self.events.clone()
    }

    /// Frequent clock samples do not allocate metadata or wake the desktop worker.
    pub fn position_sample(&self, track_id: u64, seconds: f64) {
        if !seconds.is_finite() || seconds < 0. {
            return;
        }
        let mut published = self
            .published
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
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
        let mut published = self
            .published
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
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
        let mut published = self
            .published
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if published.state.track_id != Some(track_id) {
            return;
        }
        published.seeked = Some((track_id, microseconds(seconds)));
        drop(published);
        let _ = self.wake.try_send(());
    }
}

fn microseconds(seconds: f64) -> i64 {
    (seconds.max(0.) * 1_000_000.) as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    pub(super) fn playing() -> State {
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
    fn position_projects_at_rate_and_freezes_during_pause_buffering_and_stop() {
        let (mpris, _, updates) = MediaControls::unserved();
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
}
