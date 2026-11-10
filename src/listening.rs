//! Local resume positions and the listening intervals sent to YouTube.
use crate::catalog::Track;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::HashMap,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct ResumePositions(HashMap<String, SavedPosition>);

#[derive(Clone, Serialize, Deserialize)]
struct SavedPosition {
    seconds: f64,
    updated_at: u64,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

impl ResumePositions {
    pub fn get(&self, scope: &str, track: &Track) -> f64 {
        let local = self.0.get(&format!("{scope}:{}", track.id));
        if !resumable(track) && local.is_none() {
            return 0.;
        }
        // A fresh local sample wins over the account's eventually consistent read.
        let seconds = match local {
            Some(local) if now().saturating_sub(local.updated_at) < 180 => local.seconds,
            _ => track
                .details
                .resume_seconds
                .or_else(|| local.map(|local| local.seconds))
                .unwrap_or(0.),
        };
        normalize(seconds, track.duration.map(|duration| duration as f64))
    }

    pub fn set(
        &mut self,
        scope: &str,
        track: &Track,
        seconds: f64,
        duration: f64,
        completed: bool,
    ) -> bool {
        if (!resumable(track) && !self.0.contains_key(&format!("{scope}:{}", track.id)))
            || !seconds.is_finite()
        {
            return false;
        }
        self.0.insert(
            format!("{scope}:{}", track.id),
            SavedPosition {
                seconds: if completed {
                    0.
                } else {
                    normalize(seconds, (duration > 0.).then_some(duration))
                },
                updated_at: now(),
            },
        );
        true
    }
}

pub fn resumable(track: &Track) -> bool {
    track.details.episode.is_some() || track.details.resume_seconds.is_some()
}

fn normalize(seconds: f64, duration: Option<f64>) -> f64 {
    if !seconds.is_finite()
        || seconds < 0.
        || duration.is_some_and(|duration| seconds >= duration - 5.)
    {
        0.
    } else {
        seconds
    }
}

/// Only parse progress explicitly issued for this row. Feedback tokens are never retained.
pub fn account_progress(row: &Value) -> Option<f64> {
    let progress = row
        .pointer("/playbackProgress/musicPlaybackProgressRenderer")
        .or_else(|| row.pointer("/progress/musicPlaybackProgressRenderer"));
    let percent = progress
        .and_then(|progress| progress["playbackProgressPercentage"].as_f64())
        .or_else(|| {
            row["thumbnailOverlays"]
                .as_array()?
                .iter()
                .find_map(|overlay| {
                    overlay
                        .pointer("/thumbnailOverlayResumePlaybackRenderer/percentDurationWatched")?
                        .as_f64()
                })
        })?;
    if !percent.is_finite() || !(0. ..=100.).contains(&percent) {
        return None;
    }
    if percent == 100. {
        return Some(0.);
    }
    let total = progress_duration(&crate::catalog::text(&row["lengthText"])).or_else(|| {
        row["fixedColumns"].as_array()?.iter().find_map(|column| {
            progress_duration(&crate::catalog::text(
                &column["musicResponsiveListItemFixedColumnRenderer"]["text"],
            ))
        })
    });
    if let Some(total) = total {
        return Some(normalize(total as f64 * percent / 100., Some(total as f64)));
    }
    let label = crate::catalog::text(&progress?["durationText"]);
    let (label, remaining) = label
        .strip_suffix(" left")
        .map_or((label.as_str(), false), |label| (label, true));
    let seconds = progress_duration(label)? as f64;
    let total = if remaining {
        seconds / (1. - percent / 100.)
    } else {
        seconds
    };
    Some(normalize(total * percent / 100., Some(total)))
}

fn progress_duration(label: &str) -> Option<u64> {
    let label = label.trim_start_matches([' ', '•']);
    if let Some(seconds) = crate::catalog::duration(label) {
        return Some(seconds);
    }
    let mut parts = label.split_whitespace();
    let mut seconds = 0_u64;
    let mut found = false;
    while let Some(number) = parts.next() {
        let number = number.parse::<u64>().ok()?;
        let scale = match parts.next()?.trim_end_matches(',') {
            "hr" | "hrs" | "hour" | "hours" => 3600,
            "min" | "mins" | "minute" | "minutes" => 60,
            "sec" | "secs" | "second" | "seconds" => 1,
            _ => return None,
        };
        seconds = seconds.checked_add(number.checked_mul(scale)?)?;
        found = true;
    }
    found.then_some(seconds)
}

#[derive(Clone, Debug, PartialEq)]
pub struct Interval {
    pub start: f64,
    pub end: f64,
}

#[derive(Default)]
pub struct ListeningTime {
    last: Option<(f64, Instant)>,
    intervals: Vec<Interval>,
    listened: f64,
}

impl ListeningTime {
    pub fn sample(&mut self, position: f64, advancing: bool, speed: f64, now: Instant) {
        if !position.is_finite() || position < 0. {
            return;
        }
        if advancing && let Some((last, at)) = self.last {
            let elapsed = now.saturating_duration_since(at).as_secs_f64();
            let delta = position - last;
            // A seek must never turn the skipped portion into listening time.
            if delta > 0. && elapsed <= 5. && delta <= elapsed * speed + 1. {
                self.listened += delta / speed;
                if let Some(interval) = self
                    .intervals
                    .last_mut()
                    .filter(|interval| (interval.end - last).abs() < 0.01)
                {
                    interval.end = position;
                } else {
                    self.intervals.push(Interval {
                        start: last,
                        end: position,
                    });
                }
            }
        }
        self.last = advancing.then_some((position, now));
    }
    pub fn discontinuity(&mut self) {
        self.last = None;
    }
    pub fn due(&self) -> bool {
        self.listened >= 30. || self.intervals.len() >= 20
    }
    pub fn take(&mut self) -> Vec<Interval> {
        self.listened = 0.;
        std::mem::take(&mut self.intervals)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Duration;
    #[test]
    fn watchtime_excludes_pauses_seeks_and_buffering() {
        let mut time = ListeningTime::default();
        let start = Instant::now();
        for (at, position, playing) in [
            (0, 0., true),
            (1, 1., true),
            (2, 2., false),
            (30, 2., false),
            (31, 2., true),
            (32, 3., true),
            (33, 100., true),
            (34, 101., true),
        ] {
            time.sample(position, playing, 1., start + Duration::from_secs(at));
        }
        assert_eq!(
            time.take(),
            vec![
                Interval { start: 0., end: 1. },
                Interval { start: 2., end: 3. },
                Interval {
                    start: 100.,
                    end: 101.
                }
            ]
        );
    }
    #[test]
    fn episode_progress_and_local_resume_survive_restart() {
        let mut track: Track = serde_json::from_value(
            json!({"id":"abcdefghijk","title":"Episode","artist":"Show","details":{"episode":{}}}),
        )
        .unwrap();
        track.duration = Some(9000);
        track.details.resume_seconds = account_progress(
            &json!({"lengthText":{"simpleText":"2:30:00"},"playbackProgress":{"musicPlaybackProgressRenderer":{"playbackProgressPercentage":37,"videoPlaybackPositionFeedbackToken":"secret"}}}),
        );
        let mut positions = ResumePositions::default();
        assert_eq!(positions.get("account", &track), 3330.);
        positions.set("account", &track, 4000., 9000., false);
        let mut library = crate::library::Library::new();
        library.positions = positions;
        library.queue.replace(vec![track.clone()], 0);
        let serialized = serde_json::to_string(&library).unwrap();
        assert!(!serialized.contains("secret"));
        let mut restored: crate::library::Library = serde_json::from_str(&serialized).unwrap();
        assert_eq!(
            restored
                .positions
                .get("account", restored.queue.current().unwrap()),
            4000.
        );
        assert_eq!(restored.positions.get("guest", &track), 3330.);
        restored
            .positions
            .set("account", &track, 9000., 9000., true);
        assert_eq!(restored.positions.get("account", &track), 0.);
        track.details.episode = None;
        track.details.resume_seconds = None;
        track.id = "ordinary-song".into();
        assert_eq!(restored.positions.get("account", &track), 0.);
    }
    #[test]
    fn resumable_video_progress_is_scoped_to_the_row_and_uses_its_duration() {
        let row = json!({"lengthText":{"simpleText":"2:00:00"},"thumbnailOverlays":[{"thumbnailOverlayResumePlaybackRenderer":{"percentDurationWatched":25}}]});
        assert_eq!(account_progress(&row), Some(1800.));
        assert_eq!(account_progress(&json!({"unrelated":row})), None);
        assert_eq!(
            account_progress(
                &json!({"progress":{"musicPlaybackProgressRenderer":{"durationText":{"runs":[{"text":" • 2 hr 30 min"}]},"playbackProgressPercentage":37}}})
            ),
            Some(3330.)
        );
    }

    #[test]
    fn remaining_time_progress_is_not_mistaken_for_total_duration() {
        assert_eq!(
            account_progress(
                &json!({"progress":{"musicPlaybackProgressRenderer":{"durationText":{"simpleText":"2 min left"},"playbackProgressPercentage":50}}})
            ),
            Some(120.)
        );
        assert_eq!(progress_duration("42K views"), None);
    }

    #[test]
    fn invalid_or_finished_account_progress_does_not_seek() {
        for percent in [json!(-1), json!(101), json!("37"), json!(100)] {
            let seconds = account_progress(
                &json!({"lengthText":{"simpleText":"10:00"},"progress":{"musicPlaybackProgressRenderer":{"playbackProgressPercentage":percent}}}),
            );
            assert!(seconds.is_none_or(|seconds| seconds == 0.));
        }
    }
}
