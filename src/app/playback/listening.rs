//! Checkpoint resume positions and batch account listening reports off the UI thread.
use super::*;
use crate::{history::Watchtime, listening::ListeningTime};

#[derive(Default)]
pub(in crate::app) struct ListeningState {
    active: Option<ActiveListening>,
    pub(super) resume_candidate: Option<(String, Instant)>,
}

struct ActiveListening {
    track: Track,
    playback_id: u64,
    scope: String,
    session: Option<Arc<Session>>,
    time: ListeningTime,
    position: f64,
    checkpoint: Instant,
}

impl Music {
    fn listening_scope(&self) -> String {
        self.account
            .as_ref()
            .map_or_else(|| "guest".into(), |account| account.session.resume_scope())
    }

    pub(in crate::app) fn play_current_resuming(
        &mut self,
        paused: bool,
        mode: WatchMode,
        cx: &mut Context<Self>,
    ) {
        self.finish_listening(false);
        let start = self.library.queue.current().map_or(0., |track| {
            self.library.positions.get(&self.listening_scope(), track)
        });
        self.play_current_at(start, paused, mode, cx);
        self.listening.resume_candidate = self
            .library
            .queue
            .current()
            .map(|track| (track.id.clone(), Instant::now()));
    }

    pub(in crate::app) fn resume_account_position(&mut self, cx: &mut Context<Self>) {
        let Some(track) = self.library.queue.current().cloned() else {
            return;
        };
        let Some((id, at)) = &self.listening.resume_candidate else {
            return;
        };
        if id != &track.id || at.elapsed() > Duration::from_secs(10) {
            return;
        }
        let start = self.library.positions.get(&self.listening_scope(), &track);
        if start > 0. && (start - self.position).abs() > 2. {
            self.seek_current(start, cx);
        }
        if let Some(active) = &mut self.listening.active
            && active.track.id == track.id
        {
            active.track = track.clone();
        }
    }

    pub(in crate::app) fn begin_listening(&mut self, track: &Track, playback_id: u64) {
        let mut time = ListeningTime::default();
        time.sample(
            self.position,
            self.playback.is_advancing(),
            f64::from(self.library.speed),
            Instant::now(),
        );
        self.listening.active = Some(ActiveListening {
            track: track.clone(),
            playback_id,
            scope: self.listening_scope(),
            session: self.account.as_ref().map(|account| account.session.clone()),
            time,
            position: self.position,
            checkpoint: Instant::now(),
        });
    }

    pub(in crate::app) fn sample_listening(&mut self) {
        let Some(active) = &mut self.listening.active else {
            return;
        };
        active.position = self.position;
        active.time.sample(
            self.position,
            self.playback.is_advancing(),
            f64::from(self.library.speed),
            Instant::now(),
        );
        let due = active.time.due();
        let checkpoint = active.checkpoint.elapsed() >= Duration::from_secs(10);
        if due {
            self.report_listening(false);
        }
        if checkpoint {
            self.checkpoint_listening(false);
        }
    }

    pub(in crate::app) fn listening_discontinuity(&mut self) {
        self.listening.resume_candidate = None;
        if let Some(active) = &mut self.listening.active {
            active.time.discontinuity();
        }
    }

    pub(in crate::app) fn checkpoint_listening(&mut self, completed: bool) {
        let Some(active) = &mut self.listening.active else {
            return;
        };
        let saved = self.library.positions.set(
            &active.scope,
            &active.track,
            active.position,
            self.duration,
            completed,
        );
        active.checkpoint = Instant::now();
        if saved {
            self.save();
        }
    }

    pub(in crate::app) fn report_listening(&mut self, final_report: bool) {
        let Some(active) = &mut self.listening.active else {
            return;
        };
        let intervals = active.time.take();
        if intervals.is_empty() {
            return;
        }
        let Some(session) = &active.session else {
            return;
        };
        if !self
            .account
            .as_ref()
            .is_some_and(|account| Arc::ptr_eq(&account.session, session))
        {
            return;
        }
        if self
            .account_commands
            .try_send(account::Command::ReportWatchtime {
                session: session.clone(),
                report: Watchtime {
                    playback_id: active.playback_id,
                    video_id: active.track.id.clone(),
                    intervals,
                    position: active.position,
                    final_report,
                },
            })
            .is_err()
        {
            self.error =
                Some("Could not queue listening time. Restart the app to reconnect.".into());
        }
    }

    pub(in crate::app) fn finish_listening(&mut self, completed: bool) {
        self.checkpoint_listening(completed);
        self.report_listening(true);
        self.listening.active = None;
        self.listening.resume_candidate = None;
    }
}
