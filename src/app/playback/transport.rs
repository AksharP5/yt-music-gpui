//! The transport controls' state, derived in one place. The player bar, the
//! shortcuts, Up next and the desktop media controls all read it.

use super::super::*;
use media_controls::PlaybackStatus;

/// Previous restarts the current song after this many seconds.
pub(in crate::app) const PREVIOUS_RESTART_SECONDS: f64 = 3.;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(in crate::app) struct Transport {
    pub(in crate::app) status: PlaybackStatus,
    pub(in crate::app) can_go_previous: bool,
    pub(in crate::app) can_go_next: bool,
    /// Seconds of the selected song, or 0 when unknown.
    pub(in crate::app) duration: f64,
}

impl Transport {
    /// `more_songs` covers songs beyond the loaded queue that are already on
    /// their way, such as further pages or a playlist that is starting.
    /// `recommendations` covers autoplay songs, which apply only when the
    /// library's autoplay does.
    pub(in crate::app) fn new(
        library: &Library,
        playback: Playback,
        position: f64,
        measured: Option<&(String, f64)>,
        more_songs: bool,
        recommendations: bool,
        skip: impl Fn(&Track) -> bool,
    ) -> Self {
        let queue = &library.queue;
        Self {
            status: match playback {
                Playback::Stopped => PlaybackStatus::Stopped,
                playback if playback.is_paused() => PlaybackStatus::Paused,
                _ => PlaybackStatus::Playing,
            },
            can_go_previous: queue.index.is_some_and(|index| index > 0)
                || position > PREVIOUS_RESTART_SECONDS,
            can_go_next: queue.next_index(library.repeat, false, skip).is_some()
                || more_songs
                || (library.music_autoplay() && recommendations),
            duration: queue
                .current()
                .map_or(0., |track| track_duration(track, measured)),
        }
    }
}

/// Next and Previous play the song they move to, even from pause, as on
/// music.youtube.com. Restarting the current song keeps its pause, and a stopped
/// player only changes its selection.
pub(in crate::app) fn skip_playback(playback: Playback) -> Playback {
    match playback {
        Playback::Stopped => Playback::Stopped,
        playback => playback.with_pause(false),
    }
}

/// The player's measurement when it belongs to this track, else the catalog length.
pub(in crate::app) fn track_duration(track: &Track, measured: Option<&(String, f64)>) -> f64 {
    measured
        .filter(|(id, seconds)| *id == track.id && *seconds > 0.)
        .map_or_else(
            || track.duration.unwrap_or(0) as f64,
            |(_, seconds)| *seconds,
        )
}

impl Music {
    pub(in crate::app) fn transport(&self) -> Transport {
        let queue = &self.library.queue;
        let skip = |track: &Track| is_disliked(&self.ratings, track);
        let more_songs = matches!(
            &self.watch_pending,
            Some(
                PlaybackQuery::Playlist(_)
                    | PlaybackQuery::Collection(_)
                    | PlaybackQuery::Watch {
                        mode: WatchMode::Issued | WatchMode::Radio,
                        ..
                    }
            )
        ) || matches!(
            &self.account_operation,
            Some(
                AccountOperation::Refreshing { resume, .. }
                | AccountOperation::Activating { resume, .. }
                | AccountOperation::RefreshFailed { resume }
            ) if matches!(
                resume,
                Some(PlaybackIntent::Queue { .. } | PlaybackIntent::Track(PlaybackResume {
                    mode: WatchMode::Issued, ..
                }))
            )
        ) || (queue.incomplete
            && (self.queue_source.can_continue() || queue.source.is_some()));
        let seed_available = !self.autoplay_seeded
            && self.now_playing.as_ref().is_some_and(|watch| {
                watch
                    .current
                    .as_ref()
                    .zip(queue.current())
                    .is_some_and(|(issued, current)| issued.id == current.id)
                    && watch.current_index.is_some_and(|index| {
                        index + 1 < watch.queue.len() || watch.continuation.is_some()
                    })
            });
        let recommendations = self.autoplay_tracks.iter().any(|track| !skip(track))
            || self.radio.is_some()
            || self.watch_pending.is_some()
            || self.radio_loading
            || seed_available;
        let transport = Transport::new(
            &self.library,
            self.playback,
            self.playback_position(),
            self.measured_duration.as_ref(),
            more_songs,
            recommendations,
            skip,
        );
        if self.player.is_some() {
            return transport;
        }
        Transport {
            can_go_previous: false,
            can_go_next: false,
            ..transport
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Playback, Transport, skip_playback};
    use crate::{
        catalog::Track,
        library::{Library, Repeat},
        media_controls::PlaybackStatus::{Paused, Playing, Stopped},
    };

    #[derive(Clone, Copy)]
    struct Case {
        /// One letter per song: `a` 3:52, `b` 1:23, `c` unknown, `v` a 3:11 video.
        queue: &'static str,
        selected: usize,
        removed: Option<usize>,
        incomplete: bool,
        repeat: Repeat,
        autoplay: bool,
        more_songs: bool,
        recommendations: bool,
        playback: Playback,
        position: f64,
        measured: Option<(&'static str, f64)>,
    }

    fn song(id: char) -> Track {
        Track {
            id: id.into(),
            title: id.into(),
            artist: String::new(),
            album: None,
            duration: match id {
                'a' => Some(232),
                'b' => Some(83),
                'v' => Some(191),
                _ => None,
            },
            thumbnail: None,
            details: Default::default(),
        }
    }

    #[test]
    fn next_and_previous_play_from_pause_but_not_from_stop() {
        for paused in [
            Playback::Loaded {
                paused: true,
                buffering: false,
            },
            Playback::Loading {
                paused: true,
                retried: false,
                full_lookup: false,
            },
        ] {
            let playback = skip_playback(paused);
            assert!(!playback.is_paused() && playback != Playback::Stopped);
        }
        assert_eq!(skip_playback(Playback::Stopped), Playback::Stopped);
    }

    #[test]
    fn every_transport_surface_reads_one_derivation() {
        let playing = Case {
            queue: "abc",
            selected: 0,
            removed: None,
            incomplete: false,
            repeat: Repeat::Off,
            autoplay: false,
            more_songs: false,
            recommendations: false,
            playback: Playback::Loaded {
                paused: false,
                buffering: false,
            },
            position: 0.,
            measured: None,
        };
        let stopped = Case {
            playback: Playback::Stopped,
            ..playing
        };
        let paused = Playback::Loaded {
            paused: true,
            buffering: false,
        };
        let waiting = Playback::Loading {
            paused: false,
            retried: false,
            full_lookup: false,
        };
        // Expected: status, Previous, Next, duration.
        #[rustfmt::skip]
        let cases = [
            ("empty queue", Case { queue: "", ..stopped }, (Stopped, false, false, 0.)),
            ("empty queue with autoplay", Case { queue: "", autoplay: true, recommendations: true, ..stopped }, (Stopped, false, false, 0.)),
            ("single song", Case { queue: "a", ..playing }, (Playing, false, false, 232.)),
            ("single song, repeat all", Case { queue: "a", repeat: Repeat::All, ..playing }, (Playing, false, true, 232.)),
            ("single song, repeat one", Case { queue: "a", repeat: Repeat::One, ..playing }, (Playing, false, true, 232.)),
            ("single song, autoplay on", Case { queue: "a", autoplay: true, recommendations: true, ..playing }, (Playing, false, true, 232.)),
            ("single song, autoplay on, nothing recommended yet", Case { queue: "a", autoplay: true, ..playing }, (Playing, false, false, 232.)),
            ("single song, autoplay off", Case { queue: "a", recommendations: true, ..playing }, (Playing, false, false, 232.)),
            ("first song at three seconds", Case { position: 3., ..playing }, (Playing, false, true, 232.)),
            ("first song past three seconds", Case { position: 3.25, ..playing }, (Playing, true, true, 232.)),
            ("first song, stopped at a restored position", Case { position: 20., ..stopped }, (Stopped, true, true, 232.)),
            ("last song", Case { selected: 2, ..playing }, (Playing, true, false, 0.)),
            ("last song, repeat all", Case { selected: 2, repeat: Repeat::All, ..playing }, (Playing, true, true, 0.)),
            ("last loaded song, more pages coming", Case { queue: "ab", selected: 1, incomplete: true, more_songs: true, ..playing }, (Playing, true, true, 83.)),
            ("last loaded song, no autoplay before the rest loads", Case { queue: "ab", selected: 1, incomplete: true, autoplay: true, recommendations: true, ..playing }, (Playing, true, false, 83.)),
            ("paused", Case { playback: paused, ..playing }, (Paused, false, true, 232.)),
            ("waiting to play", Case { playback: waiting, ..playing }, (Playing, false, true, 232.)),
            ("player length of the playing song", Case { measured: Some(("a", 232.5)), ..playing }, (Playing, false, true, 232.5)),
            ("stopped on the next song", Case { selected: 1, measured: Some(("a", 232.5)), ..stopped }, (Stopped, true, true, 83.)),
            ("stopped on a song of unknown length", Case { selected: 2, measured: Some(("a", 232.5)), ..stopped }, (Stopped, true, false, 0.)),
            ("stopped after switching to the video", Case { queue: "vbc", measured: Some(("a", 232.5)), ..stopped }, (Stopped, false, true, 191.)),
            ("playing song removed", Case { selected: 1, removed: Some(1), measured: Some(("b", 83.4)), ..stopped }, (Stopped, true, false, 0.)),
            ("last loaded song removed while more load", Case { queue: "ab", selected: 1, removed: Some(1), incomplete: true, more_songs: true, measured: Some(("b", 83.4)), ..stopped }, (Stopped, false, true, 232.)),
        ];
        for (name, case, expected) in cases {
            let mut library = Library {
                repeat: case.repeat,
                autoplay: case.autoplay,
                ..Library::default()
            };
            library
                .queue
                .replace(case.queue.chars().map(song).collect(), case.selected);
            library.queue.incomplete = case.incomplete;
            if let Some(index) = case.removed {
                library.queue.remove(index);
            }
            let measured = case.measured.map(|(id, seconds)| (id.to_owned(), seconds));
            let transport = Transport::new(
                &library,
                case.playback,
                case.position,
                measured.as_ref(),
                case.more_songs,
                case.recommendations,
                |_| false,
            );
            assert_eq!(
                (
                    transport.status,
                    transport.can_go_previous,
                    transport.can_go_next,
                    transport.duration
                ),
                expected,
                "{name}"
            );
        }
    }
}
