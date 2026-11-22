use crate::catalog::{Collection, Track};
use crate::player::{AudioQuality, PlaybackSpeed, VideoQuality};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    fs,
    io::{BufWriter, Write},
    path::PathBuf,
    sync::{Arc, Condvar, Mutex, PoisonError},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Copy, Default, Debug, PartialEq, Serialize, Deserialize)]
pub enum Repeat {
    #[default]
    Off,
    All,
    One,
}

impl Repeat {
    pub fn next(self) -> Self {
        match self {
            Self::Off => Self::All,
            Self::All => Self::One,
            Self::One => Self::Off,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Library {
    pub positions: crate::listening::ResumePositions,
    /// Where the current song stopped when listening last ended, for Play after a restart.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_position: Option<LastPosition>,
    pub liked: Vec<Track>,
    pub recent: Vec<Track>,
    pub saved: Vec<Collection>,
    pub queue: Queue,
    pub repeat: Repeat,
    pub autoplay: bool,
    pub volume: f64,
    pub muted: bool,
    pub speed: PlaybackSpeed,
    pub quality: AudioQuality,
    pub video: bool,
    pub video_quality: VideoQuality,
    /// Whether captions show over videos that have them, as the web's CC button sets.
    pub captions: bool,
}

impl Default for Library {
    fn default() -> Self {
        Self {
            positions: crate::listening::ResumePositions::default(),
            last_position: None,
            liked: Vec::new(),
            recent: Vec::new(),
            saved: Vec::new(),
            queue: Queue::default(),
            repeat: Repeat::default(),
            autoplay: true,
            volume: 0.,
            muted: false,
            speed: PlaybackSpeed::default(),
            quality: AudioQuality::default(),
            video: false,
            video_quality: VideoQuality::default(),
            captions: false,
        }
    }
}

impl Library {
    pub fn new() -> Self {
        Self {
            volume: 60.,
            ..Default::default()
        }
    }
    pub fn effective_volume(&self) -> f64 {
        if self.muted { 0. } else { self.volume }
    }
    pub fn music_autoplay(&self) -> bool {
        self.autoplay
            && !self.queue.incomplete
            && self
                .queue
                .current()
                .is_some_and(|track| track.details.episode.is_none())
    }
    pub fn toggle_like(&mut self, track: &Track) {
        if let Some(i) = self.liked.iter().position(|t| t.id == track.id) {
            self.liked.remove(i);
        } else {
            self.liked.insert(0, track.clone());
        }
    }
    pub fn record_play(&mut self, track: &Track) {
        self.recent.retain(|t| t.id != track.id);
        self.recent.insert(0, track.clone());
        self.recent.truncate(100);
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LastPosition {
    pub track_id: String,
    pub seconds: f64,
}

#[derive(Clone, Default, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Queue {
    pub tracks: Arc<Vec<Track>>,
    pub index: Option<usize>,
    /// More explicit tracks exist. Restored partial queues must not enter autoplay.
    pub incomplete: bool,
    /// The album, playlist or mix this queue came from, shown as "Playing from".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub playing_from: Option<String>,
    /// Local shuffle mixes continuation pages. Service ordering lives in `collection`.
    pub(crate) shuffled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) collection: Option<crate::queue_source::CollectionPlayback>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) source: Option<crate::queue_source::SavedSource>,
    #[serde(skip)]
    pub(crate) source_id: Option<Arc<()>>,
}

impl Queue {
    pub fn current(&self) -> Option<&Track> {
        self.tracks.get(self.index?)
    }
    pub fn replace(&mut self, tracks: Vec<Track>, selected: usize) {
        self.index = (selected < tracks.len()).then_some(selected);
        self.tracks = Arc::new(tracks);
        self.incomplete = false;
        self.playing_from = None;
        self.shuffled = false;
        self.collection = None;
        self.source_id = None;
        self.source = None;
    }
    /// Editing detaches from any immutable library snapshot held by the writer.
    pub(crate) fn tracks_mut(&mut self) -> &mut Vec<Track> {
        Arc::make_mut(&mut self.tracks)
    }
    /// Keep saved snapshots shared until a track actually needs editing.
    pub(crate) fn update_tracks(
        &mut self,
        needs_update: impl Fn(&Track) -> bool,
        mut update: impl FnMut(&mut Track),
    ) {
        let Some(first) = self.tracks.iter().position(&needs_update) else {
            return;
        };
        for track in self.tracks_mut().iter_mut().skip(first) {
            if needs_update(track) {
                update(track);
            }
        }
    }

    pub fn select(&mut self, index: usize) -> bool {
        if index >= self.tracks.len() {
            return false;
        }
        self.index = Some(index);
        true
    }
    #[cfg(test)]
    pub fn advance(&mut self, repeat: Repeat, natural_end: bool) -> bool {
        let next = self.next_index(repeat, natural_end, |_| false);
        next.is_some_and(|index| self.select(index))
    }
    /// The track an automatic advance selects, passing over tracks `skip` rejects.
    /// Repeat One replays the current track at its natural end, even a skipped one.
    pub fn next_index(
        &self,
        repeat: Repeat,
        natural_end: bool,
        skip: impl Fn(&Track) -> bool,
    ) -> Option<usize> {
        let Some(current) = self.index else {
            return (0..self.tracks.len()).find(|&i| !skip(&self.tracks[i]));
        };
        if natural_end && repeat == Repeat::One {
            return Some(current);
        }
        let wrap = !self.incomplete && repeat != Repeat::Off;
        (current + 1..self.tracks.len())
            .chain((0..=current).filter(|_| wrap))
            .find(|&i| !skip(&self.tracks[i]))
    }
    pub fn advance_with_autoplay(
        &mut self,
        repeat: Repeat,
        natural_end: bool,
        autoplay: bool,
        recommendations: &mut VecDeque<Track>,
        skip: impl Fn(&Track) -> bool,
    ) -> bool {
        if let Some(index) = self.next_index(repeat, natural_end, &skip) {
            return self.select(index);
        }
        if self.incomplete || !autoplay {
            return false;
        }
        let Some(track) =
            std::iter::from_fn(|| recommendations.pop_front()).find(|track| !skip(track))
        else {
            return false;
        };
        self.index = Some(self.tracks.len());
        self.tracks_mut().push(track);
        true
    }
    pub fn previous(&mut self) -> bool {
        self.index.is_some_and(|i| i > 0 && self.select(i - 1))
    }
    pub fn remove(&mut self, index: usize) -> bool {
        if index >= self.tracks.len() {
            return false;
        }
        let was_current = self.index == Some(index);
        self.tracks_mut().remove(index);
        self.index = match self.index {
            Some(i) if i > index => Some(i - 1),
            Some(_) if self.tracks.is_empty() => None,
            Some(i) => Some(i.min(self.tracks.len() - 1)),
            None => None,
        };
        was_current
    }
    pub fn move_track(&mut self, from: usize, to: usize) -> bool {
        if from == to || from >= self.tracks.len() || to >= self.tracks.len() {
            return false;
        }
        let tracks = self.tracks_mut();
        let track = tracks.remove(from);
        tracks.insert(to, track);
        self.index = self.index.map(|current| match current {
            i if i == from => to,
            i if from < i && i <= to => i - 1,
            i if to <= i && i < from => i + 1,
            i => i,
        });
        true
    }
    pub fn shuffle_upcoming(&mut self) {
        self.shuffled = true;
        self.shuffle_remaining();
    }

    pub(crate) fn append_source(&mut self, tracks: Vec<Track>) {
        if tracks.is_empty() {
            return;
        }
        let first_new = self.tracks.len();
        self.tracks_mut().extend(tracks);
        if self.shuffled {
            self.shuffle_from(first_new);
        }
    }

    fn shuffle_remaining(&mut self) {
        self.shuffle_from(self.index.map_or(0, |i| i + 1));
    }

    fn shuffle_from(&mut self, first: usize) {
        let start = self.index.map_or(0, |i| i + 1);
        let first = first.max(start + 1);
        if first >= self.tracks.len() {
            return;
        }
        let tracks = self.tracks_mut();
        let mut seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
        // Forward Fisher-Yates also inserts new pages into an already shuffled
        // suffix in O(page size), without revisiting the entire loaded queue.
        for i in first..tracks.len() {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let j = start + seed as usize % (i - start + 1);
            tracks.swap(i, j);
        }
    }
}

pub struct Store {
    path: PathBuf,
}

impl Store {
    pub fn load() -> Result<(Self, Library)> {
        let directory = crate::dirs::data().context("Could not find your user data directory")?;
        let store = Self {
            path: directory.join("library.json"),
        };
        let mut library: Library = match fs::read(&store.path) {
            Ok(bytes) => serde_json::from_slice(&bytes).with_context(|| {
                format!(
                    "Could not read {}. The original file has been preserved.",
                    store.path.display()
                )
            })?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Library::new(),
            Err(e) => return Err(e).context("Could not read your saved music"),
        };
        if !library.volume.is_finite() {
            library.volume = 60.;
        }
        library.volume = library.volume.clamp(0., 100.);
        if library.queue.current().is_none() {
            library.queue.index = (!library.queue.tracks.is_empty()).then_some(0);
        }
        library.recent.truncate(100);
        library.video &= crate::video::SUPPORTED;
        Ok((store, library))
    }

    pub fn save(&self, library: &Library) -> Result<()> {
        let directory = self.path.parent().context("Missing data directory")?;
        fs::create_dir_all(directory).context("Could not create the music data directory")?;
        let mut file = tempfile::NamedTempFile::new_in(directory)?;
        {
            let mut writer = BufWriter::new(&mut file);
            serde_json::to_writer(&mut writer, library)?;
            writer.write_all(b"\n")?;
            writer.flush()?;
        }
        file.as_file().sync_all()?;
        file.persist(&self.path)
            .context("Could not save your music library")?;
        Ok(())
    }
}

enum SaveRequest {
    Snapshot(Box<Library>),
    Update(LibraryUpdate),
}

#[derive(Default)]
struct LibraryUpdate {
    preferences: Option<Preferences>,
    positions: Option<(crate::listening::ResumePositions, Option<LastPosition>)>,
}

impl LibraryUpdate {
    fn apply(self, library: &mut Library) {
        if let Some(preferences) = self.preferences {
            preferences.apply(library);
        }
        if let Some((positions, last_position)) = self.positions {
            library.positions = positions;
            library.last_position = last_position;
        }
    }
}

#[derive(Clone, Copy)]
struct Preferences {
    repeat: Repeat,
    autoplay: bool,
    volume: f64,
    muted: bool,
    speed: PlaybackSpeed,
    quality: AudioQuality,
    video: bool,
    video_quality: VideoQuality,
    captions: bool,
}

impl From<&Library> for Preferences {
    fn from(library: &Library) -> Self {
        Self {
            repeat: library.repeat,
            autoplay: library.autoplay,
            volume: library.volume,
            muted: library.muted,
            speed: library.speed,
            quality: library.quality,
            video: library.video,
            video_quality: library.video_quality,
            captions: library.captions,
        }
    }
}

impl Preferences {
    fn apply(self, library: &mut Library) {
        library.repeat = self.repeat;
        library.autoplay = self.autoplay;
        library.volume = self.volume;
        library.muted = self.muted;
        library.speed = self.speed;
        library.quality = self.quality;
        library.video = self.video;
        library.video_quality = self.video_quality;
        library.captions = self.captions;
    }
}

#[derive(Default)]
struct PendingSave {
    request: Option<SaveRequest>,
    closed: bool,
}

#[derive(Default)]
struct SaveQueue {
    pending: Mutex<PendingSave>,
    ready: Condvar,
}

/// Coalesces pending saves without waiting for disk I/O. Drop flushes and joins the worker.
pub struct LibraryWriter {
    queue: Arc<SaveQueue>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl LibraryWriter {
    pub fn new(store: Store) -> (Self, async_channel::Receiver<String>) {
        Self::with_save(store, Store::save)
    }

    #[expect(
        clippy::expect_used,
        reason = "the app cannot run without this worker, and spawning one fails only when the OS is out of threads"
    )]
    fn with_save(
        store: Store,
        mut save: impl FnMut(&Store, &Library) -> Result<()> + Send + 'static,
    ) -> (Self, async_channel::Receiver<String>) {
        let queue = Arc::new(SaveQueue::default());
        let worker_queue = queue.clone();
        let (errors, failed) = async_channel::unbounded();
        let worker = std::thread::Builder::new()
            .name("library-writer".into())
            .spawn(move || {
                let mut current = None;
                loop {
                    let request = {
                        let mut pending = worker_queue
                            .pending
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner);
                        while pending.request.is_none() && !pending.closed {
                            pending = worker_queue
                                .ready
                                .wait(pending)
                                .unwrap_or_else(PoisonError::into_inner);
                        }
                        let Some(request) = pending.request.take() else {
                            break;
                        };
                        request
                    };
                    match request {
                        SaveRequest::Snapshot(library) => current = Some(*library),
                        SaveRequest::Update(update) => {
                            if current.is_none() {
                                let restored = match fs::read(&store.path) {
                                    Ok(bytes) => serde_json::from_slice::<Library>(&bytes)
                                        .map_err(anyhow::Error::from),
                                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                                        Ok(Library::new())
                                    }
                                    Err(error) => Err(error.into()),
                                };
                                match restored {
                                    Ok(library) => current = Some(library),
                                    Err(error) => {
                                        let _ = errors.send_blocking(format!(
                                            "Could not read your saved music: {error}"
                                        ));
                                        continue;
                                    }
                                }
                            }
                            if let Some(library) = current.as_mut() {
                                update.apply(library);
                            }
                        }
                    }
                    if let Some(library) = &current
                        && let Err(error) = save(&store, library)
                    {
                        let _ = errors.send_blocking(format!("{error:#}"));
                    }
                }
            })
            .expect("start library writer");
        (
            Self {
                queue,
                worker: Some(worker),
            },
            failed,
        )
    }

    pub fn save(&self, library: &Library) {
        let snapshot = Box::new(library.clone());
        let mut pending = self
            .queue
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        pending.request = Some(SaveRequest::Snapshot(snapshot));
        self.queue.ready.notify_one();
    }

    /// Save settings without cloning queue or library collections on the UI thread.
    pub fn save_preferences(&self, library: &Library) {
        self.update(LibraryUpdate {
            preferences: Some(Preferences::from(library)),
            ..Default::default()
        });
    }

    pub fn save_positions(&self, library: &Library) {
        self.update(LibraryUpdate {
            positions: Some((library.positions.clone(), library.last_position.clone())),
            ..Default::default()
        });
    }

    fn update(&self, update: LibraryUpdate) {
        // Merge and dequeue use the same lock; the worker never holds it during I/O.
        let mut pending = self
            .queue
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        pending.request = Some(match pending.request.take() {
            Some(SaveRequest::Snapshot(mut snapshot)) => {
                update.apply(&mut snapshot);
                SaveRequest::Snapshot(snapshot)
            }
            Some(SaveRequest::Update(previous)) => SaveRequest::Update(LibraryUpdate {
                preferences: update.preferences.or(previous.preferences),
                positions: update.positions.or(previous.positions),
            }),
            None => SaveRequest::Update(update),
        });
        self.queue.ready.notify_one();
    }
}

impl Drop for LibraryWriter {
    fn drop(&mut self) {
        self.queue
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .closed = true;
        self.queue.ready.notify_one();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn episodes_advance_and_repeat_without_consuming_music_recommendations() {
        let mut episode = track("episode");
        episode.details.episode = Some(Box::default());
        for natural in [false, true] {
            let mut library = Library::default();
            library
                .queue
                .replace(vec![episode.clone(), episode.clone()], 0);
            let mut recommendations = VecDeque::from([track("music")]);
            let autoplay = library.music_autoplay();
            assert!(library.queue.advance_with_autoplay(
                Repeat::Off,
                natural,
                autoplay,
                &mut recommendations,
                |_| false
            ));
            assert_eq!(library.queue.index, Some(1));
            assert!(!library.queue.advance_with_autoplay(
                Repeat::Off,
                natural,
                autoplay,
                &mut recommendations,
                |_| false
            ));
            assert_eq!(library.queue.tracks.len(), 2);
            assert_eq!(recommendations.front().unwrap().id, "music");
            assert!(library.queue.advance_with_autoplay(
                Repeat::All,
                natural,
                autoplay,
                &mut recommendations,
                |_| false
            ));
            assert_eq!(library.queue.index, Some(0));
        }
    }
    fn track(id: &str) -> Track {
        Track {
            id: id.into(),
            title: id.into(),
            artist: String::new(),
            album: None,
            duration: None,
            thumbnail: None,
            details: Default::default(),
        }
    }

    #[test]
    fn preferences_keep_the_latest_queue_snapshot_and_existing_disk_library() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("library.json");
        let mut library = Library::new();
        library
            .queue
            .replace(vec![track("first"), track("second")], 1);
        let store = Store { path: path.clone() };
        store.save(&library).unwrap();
        let (writer, errors) = LibraryWriter::new(store);
        library.volume = 17.;
        library.muted = true;
        writer.save_preferences(&library);
        drop(writer);
        assert!(errors.is_empty());
        let saved: Library = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved.volume, 17.);
        assert!(saved.muted);
        assert_eq!(saved.effective_volume(), 0.);
        assert_eq!(saved.queue.tracks, library.queue.tracks);
        let (writer, errors) = LibraryWriter::new(Store { path: path.clone() });
        library.queue.tracks_mut().push(track("new-tail"));
        writer.save(&library);
        for n in 0..100 {
            library.volume = n as f64;
            library.muted = n % 2 == 0;
            writer.save_preferences(&library);
        }
        drop(writer);
        assert!(errors.is_empty());
        let saved: Library = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(saved.volume, 99.);
        assert!(!saved.muted);
        assert_eq!(saved.queue.tracks, library.queue.tracks);
    }

    #[test]
    fn checkpoints_and_preferences_preserve_each_other_and_pending_queue_edits() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("library.json");
        let mut library = Library::new();
        let mut episode = track("episode");
        episode.details.episode = Some(Box::default());
        episode.duration = Some(3600);
        library.queue.replace(vec![episode.clone()], 0);
        Store { path: path.clone() }.save(&library).unwrap();

        for snapshot in [false, true] {
            let (writer, errors) = LibraryWriter::new(Store { path: path.clone() });
            if snapshot {
                library.queue.tracks_mut().push(track("queued"));
                writer.save(&library);
            }
            for seconds in 100..200 {
                library
                    .positions
                    .set("guest", &episode, f64::from(seconds), 3600., false);
                writer.save_positions(&library);
                library.volume = f64::from(seconds % 100);
                writer.save_preferences(&library);
            }
            drop(writer);
            assert!(errors.is_empty());
            let saved: Library = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            assert_eq!(saved.positions.get("guest", &episode), 199.);
            assert_eq!(saved.volume, 99.);
            assert_eq!(saved.queue.tracks, library.queue.tracks);
        }
    }

    #[test]
    fn interleaved_saves_keep_latest_state_with_a_slow_store() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("library.json");
        let mut episode = track("episode");
        episode.details.episode = Some(Box::default());
        episode.duration = Some(3600);
        let mut library = Library::new();
        for round in 0..2000 {
            let (writer, errors) =
                LibraryWriter::with_save(Store { path: path.clone() }, |store, library| {
                    std::thread::yield_now();
                    store.save(library)
                });
            for n in 0..32 {
                library
                    .queue
                    .replace(vec![track(&format!("{round}-{n}"))], 0);
                writer.save(&library);
                library.volume = f64::from(n);
                library.autoplay = n % 2 == 0;
                writer.save_preferences(&library);
                library
                    .positions
                    .set("guest", &episode, f64::from(100 + n), 3600., false);
                library.last_position = Some(LastPosition {
                    track_id: format!("{round}-{n}"),
                    seconds: f64::from(n),
                });
                writer.save_positions(&library);
            }
            drop(writer);
            assert!(errors.is_empty());
            let saved: Library = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            assert_eq!(
                serde_json::to_value(&saved).unwrap(),
                serde_json::to_value(&library).unwrap(),
                "latest library, preferences and positions must survive release in round {round}"
            );
        }
    }

    #[test]
    #[ignore = "measures caller-thread save cost with a 10,000-song queue"]
    fn large_queue_preference_save_latency() {
        let directory = tempfile::tempdir().unwrap();
        let (writer, errors) = LibraryWriter::new(Store {
            path: directory.path().join("library.json"),
        });
        let mut library = Library::new();
        library.queue.replace(
            (0..10_000).map(|i| track(&format!("song-{i}"))).collect(),
            0,
        );
        let start = std::time::Instant::now();
        for _ in 0..20 {
            writer.save(&library);
        }
        let full = start.elapsed().as_secs_f64() * 1000. / 20.;
        let start = std::time::Instant::now();
        for n in 0..20 {
            library.volume = n as f64;
            writer.save_preferences(&library);
        }
        let preferences = start.elapsed().as_secs_f64() * 1000. / 20.;
        let mut episode = track("episode");
        episode.details.episode = Some(Box::default());
        let start = std::time::Instant::now();
        for seconds in 100..120 {
            library
                .positions
                .set("guest", &episode, f64::from(seconds), 3600., false);
            writer.save_positions(&library);
        }
        let checkpoint = start.elapsed().as_secs_f64() * 1000. / 20.;
        drop(writer);
        assert!(errors.is_empty());
        println!(
            "queue_tracks=10000 snapshot_save_ms={full:.4} preference_save_ms={preferences:.4} checkpoint_save_ms={checkpoint:.4}"
        );
    }

    #[test]
    fn writer_flushes_the_latest_large_snapshot_before_exit() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("library.json");
        let (writer, errors) = LibraryWriter::new(Store { path: path.clone() });
        let mut library = Library::new();
        let mut song = track("saved");
        song.title = "Café \"live\"\n".repeat(1024);
        library.queue.replace(vec![song.clone(), song], 1);
        for n in 0..40 {
            library.volume = n as f64;
            writer.save(&library);
        }
        drop(writer);
        assert!(errors.try_recv().is_err());
        let saved: Library = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(saved.volume, 39.);
        assert_eq!(saved.queue.index, Some(1));
        assert_eq!(saved.queue.tracks, library.queue.tracks);
    }

    #[test]
    fn writing_a_snapshot_preserves_the_queue_as_it_was_submitted() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("library.json");
        let (release, wait) = std::sync::mpsc::channel();
        let (writer, errors) =
            LibraryWriter::with_save(Store { path: path.clone() }, move |store, library| {
                wait.recv()?;
                store.save(library)
            });
        let mut library = Library::new();
        library
            .queue
            .replace(vec![track("first"), track("second")], 1);
        writer.save(&library);
        library.queue.tracks_mut()[1].title = "Edited in the app".into();
        library.queue.move_track(1, 0);
        release.send(()).unwrap();
        drop(writer);

        assert!(errors.is_empty());
        let saved = fs::read(path).unwrap();
        let json: serde_json::Value = serde_json::from_slice(&saved).unwrap();
        assert!(json["queue"]["tracks"].is_array());
        let restored: Library = serde_json::from_slice(&saved).unwrap();
        assert_eq!(restored.queue.tracks[0].id, "first");
        assert_eq!(restored.queue.index, Some(1));
        assert_eq!(restored.queue.current().unwrap().title, "second");
    }

    #[test]
    fn playback_preferences_migrate_and_round_trip() {
        assert!(Library::default().autoplay);
        assert!(Library::new().autoplay);
        assert_eq!(Library::new().effective_volume(), 60.);
        let silent: Library = serde_json::from_value(serde_json::json!({ "volume": 0. })).unwrap();
        assert_eq!(silent.effective_volume(), 0.);
        let migrated: Library = serde_json::from_value(serde_json::json!({
            "volume": 37., "repeat": "All", "liked": [track("saved")]
        }))
        .unwrap();
        assert!(migrated.autoplay);
        assert_eq!(migrated.speed, PlaybackSpeed::default());
        assert_eq!(migrated.quality, AudioQuality::Normal);
        assert!(!migrated.video);
        assert_eq!(migrated.video_quality, VideoQuality::P720);
        assert_eq!(migrated.volume, 37.);
        assert!(!migrated.muted);
        assert_eq!(migrated.effective_volume(), 37.);
        assert_eq!(migrated.repeat, Repeat::All);
        assert_eq!(migrated.liked[0].id, "saved");
        let library = Library {
            autoplay: false,
            volume: 37.,
            muted: true,
            quality: AudioQuality::High,
            video: true,
            video_quality: VideoQuality::P1080,
            ..Library::new()
        };
        let saved = serde_json::to_value(library).unwrap();
        assert_eq!(saved["autoplay"], false);
        assert_eq!(saved["volume"], 37.);
        assert_eq!(saved["muted"], true);
        assert_eq!(saved["quality"], "high");
        assert_eq!(saved["video"], true);
        assert_eq!(saved["video_quality"], "1080p");
        let restored = serde_json::from_value::<Library>(saved).unwrap();
        assert!(!restored.autoplay);
        assert!(restored.muted);
        assert_eq!(restored.volume, 37.);
        assert_eq!(restored.effective_volume(), 0.);
        assert_eq!(restored.quality, AudioQuality::High);
        assert!(restored.video);
        assert_eq!(restored.video_quality, VideoQuality::P1080);
    }

    #[test]
    fn autoplay_off_leaves_prefilled_recommendations_pending_at_queue_end() {
        let mut queue = Queue::default();
        queue.replace(vec![track("explicit")], 0);
        let mut recommendations = VecDeque::from([track("next"), track("later")]);
        let before = queue.clone();
        let pending = recommendations.clone();
        for natural_end in [false, true] {
            assert!(!queue.advance_with_autoplay(
                Repeat::Off,
                natural_end,
                false,
                &mut recommendations,
                |_| false
            ));
            assert_eq!(queue.tracks, before.tracks);
            assert_eq!(queue.index, before.index);
            assert_eq!(recommendations, pending);
        }
    }

    #[test]
    fn explicit_enqueues_advance_before_pending_recommendations() {
        let mut queue = Queue::default();
        queue.replace(vec![track("playing")], 0);
        let mut recommendations = VecDeque::from([track("recommended")]);
        queue.tracks_mut().push(track("enqueued"));
        assert!(
            queue.advance_with_autoplay(Repeat::Off, true, true, &mut recommendations, |_| false)
        );
        assert_eq!(queue.current().unwrap().id, "enqueued");
        assert_eq!(queue.tracks.len(), 2);
        assert_eq!(recommendations.len(), 1);
        assert!(
            queue.advance_with_autoplay(Repeat::Off, true, true, &mut recommendations, |_| false)
        );
        assert_eq!(queue.current().unwrap().id, "recommended");
        assert!(recommendations.is_empty());
    }

    #[test]
    fn autoplay_consumes_one_occurrence_only_after_normal_advance_is_exhausted() {
        let mut queue = Queue::default();
        queue.replace(vec![track("duplicate")], 0);
        let mut recommendations = VecDeque::from([track("duplicate"), track("later")]);
        for (repeat, natural_end) in [(Repeat::One, true), (Repeat::All, false)] {
            assert!(queue.advance_with_autoplay(
                repeat,
                natural_end,
                true,
                &mut recommendations,
                |_| false
            ));
            assert_eq!(queue.index, Some(0));
            assert_eq!(queue.tracks.len(), 1);
            assert_eq!(recommendations.len(), 2);
        }
        assert!(queue.advance_with_autoplay(
            Repeat::Off,
            false,
            true,
            &mut recommendations,
            |_| false
        ));
        assert_eq!(queue.index, Some(1));
        assert_eq!(queue.current().unwrap().id, "duplicate");
        assert_eq!(queue.tracks.len(), 2);
        assert_eq!(recommendations.len(), 1);
        assert!(
            queue.advance_with_autoplay(Repeat::Off, true, true, &mut recommendations, |_| false)
        );
        assert_eq!(queue.index, Some(2));
        assert_eq!(queue.current().unwrap().id, "later");
        assert!(recommendations.is_empty());
        assert!(!queue.advance_with_autoplay(
            Repeat::Off,
            true,
            true,
            &mut recommendations,
            |_| false
        ));
        assert_eq!(queue.index, Some(2));
        assert_eq!(queue.tracks.len(), 3);
    }

    #[test]
    fn moving_tracks_preserves_the_selected_duplicate_occurrence() {
        let tracks: Vec<_> = (0..4)
            .map(|n| {
                let mut song = track("duplicate");
                song.title = format!("Occurrence {n}");
                song
            })
            .collect();
        for from in 0..tracks.len() {
            for to in 0..tracks.len() {
                for selected in [None, Some(0), Some(1), Some(2), Some(3)] {
                    let mut queue = Queue {
                        tracks: Arc::new(tracks.clone()),
                        index: selected,
                        ..Default::default()
                    };
                    let current = queue.current().cloned();
                    assert_eq!(queue.move_track(from, to), from != to);
                    assert_eq!(queue.current(), current.as_ref());
                    assert_eq!(queue.tracks[to], tracks[from]);
                    let remaining: Vec<_> = queue
                        .tracks
                        .iter()
                        .filter(|song| song.title != tracks[from].title)
                        .collect();
                    let expected: Vec<_> = tracks
                        .iter()
                        .enumerate()
                        .filter_map(|(i, song)| (i != from).then_some(song))
                        .collect();
                    assert_eq!(remaining, expected);
                }
            }
        }
    }

    #[test]
    fn invalid_queue_moves_leave_everything_unchanged() {
        let mut queue = Queue::default();
        assert!(!queue.move_track(0, 0));
        assert!(queue.tracks.is_empty());
        assert_eq!(queue.index, None);
        queue.replace(vec![track("a"), track("b")], 1);
        for (from, to) in [(0, 2), (2, 0), (usize::MAX, usize::MAX), (1, 1)] {
            let before = queue.clone();
            assert!(!queue.move_track(from, to));
            assert_eq!(queue.tracks, before.tracks);
            assert_eq!(queue.index, before.index);
        }
    }

    #[test]
    fn removing_before_current_keeps_the_same_song() {
        let mut q = Queue::default();
        q.replace(vec![track("a"), track("b"), track("c")], 1);
        assert!(!q.remove(0));
        assert_eq!(q.current().unwrap().id, "b");
        assert!(q.remove(0));
        assert_eq!(q.current().unwrap().id, "c");
        assert!(q.remove(0));
        assert!(q.current().is_none());
    }

    #[test]
    fn automatic_advances_pass_over_skipped_tracks_and_stop_when_none_remain() {
        let disliked = |track: &Track| track.id.starts_with("disliked");
        let mut queue = Queue::default();
        queue.replace(
            vec![
                track("a"),
                track("disliked 1"),
                track("b"),
                track("disliked 2"),
            ],
            0,
        );
        let mut recommendations = VecDeque::from([track("disliked 3"), track("c")]);
        assert!(queue.advance_with_autoplay(
            Repeat::Off,
            true,
            true,
            &mut recommendations,
            disliked
        ));
        assert_eq!(queue.current().unwrap().id, "b");
        assert!(queue.advance_with_autoplay(
            Repeat::Off,
            false,
            true,
            &mut recommendations,
            disliked
        ));
        assert_eq!(queue.current().unwrap().id, "c");
        assert!(recommendations.is_empty());

        queue.replace(vec![track("a"), track("disliked 1"), track("b")], 2);
        assert!(queue.advance_with_autoplay(
            Repeat::All,
            true,
            false,
            &mut recommendations,
            disliked
        ));
        assert_eq!(queue.current().unwrap().id, "a");

        queue.replace(vec![track("disliked 1"), track("disliked 2")], 0);
        let mut recommendations = VecDeque::from([track("disliked 3")]);
        assert!(!queue.advance_with_autoplay(
            Repeat::All,
            false,
            true,
            &mut recommendations,
            disliked
        ));
        assert_eq!(queue.index, Some(0));
        assert!(recommendations.is_empty());

        // Repeat One replays a disliked song that was clicked explicitly.
        queue.replace(vec![track("disliked 1"), track("b")], 0);
        assert!(queue.advance_with_autoplay(
            Repeat::One,
            true,
            true,
            &mut recommendations,
            disliked
        ));
        assert_eq!(queue.index, Some(0));
    }

    #[test]
    fn repeat_one_only_repeats_natural_end() {
        let mut q = Queue::default();
        q.replace(vec![track("a"), track("b")], 0);
        assert!(q.advance(Repeat::One, true));
        assert_eq!(q.index, Some(0));
        assert!(q.advance(Repeat::One, false));
        assert_eq!(q.index, Some(1));
        assert!(!q.advance(Repeat::Off, true));
        assert!(q.advance(Repeat::All, true));
        assert_eq!(q.index, Some(0));
    }

    #[test]
    fn shuffle_preserves_current_and_all_songs() {
        let mut q = Queue::default();
        q.replace((0..20).map(|n| track(&n.to_string())).collect(), 3);
        let prefix: Vec<_> = q.tracks[..4].to_vec();
        q.shuffle_upcoming();
        assert_eq!(q.tracks[..4], prefix);
        let mut ids: Vec<_> = q
            .tracks
            .iter()
            .map(|t| t.id.parse::<usize>().unwrap())
            .collect();
        ids.sort();
        assert_eq!(ids, (0..20).collect::<Vec<_>>());
    }
}
