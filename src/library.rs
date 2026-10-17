use crate::catalog::{Collection, Track};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    path::PathBuf,
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

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Library {
    pub liked: Vec<Track>,
    pub recent: Vec<Track>,
    pub saved: Vec<Collection>,
    pub queue: Queue,
    pub repeat: Repeat,
    pub volume: f64,
}

impl Library {
    pub fn new() -> Self {
        Self {
            volume: 60.,
            ..Default::default()
        }
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

#[derive(Clone, Default, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Queue {
    pub tracks: Vec<Track>,
    pub index: Option<usize>,
}

impl Queue {
    pub fn current(&self) -> Option<&Track> {
        self.tracks.get(self.index?)
    }
    pub fn replace(&mut self, tracks: Vec<Track>, selected: usize) {
        self.index = (selected < tracks.len()).then_some(selected);
        self.tracks = tracks;
    }
    pub fn select(&mut self, index: usize) -> bool {
        if index >= self.tracks.len() {
            return false;
        }
        self.index = Some(index);
        true
    }
    pub fn advance(&mut self, repeat: Repeat, natural_end: bool) -> bool {
        let Some(current) = self.index else {
            return self.select(0);
        };
        if natural_end && repeat == Repeat::One {
            return true;
        }
        if self.select(current + 1) {
            return true;
        }
        repeat != Repeat::Off && self.select(0)
    }
    pub fn previous(&mut self) -> bool {
        self.index.is_some_and(|i| i > 0 && self.select(i - 1))
    }
    pub fn remove(&mut self, index: usize) -> bool {
        if index >= self.tracks.len() {
            return false;
        }
        let was_current = self.index == Some(index);
        self.tracks.remove(index);
        self.index = match self.index {
            Some(i) if i > index => Some(i - 1),
            Some(_) if self.tracks.is_empty() => None,
            Some(i) => Some(i.min(self.tracks.len() - 1)),
            None => None,
        };
        was_current
    }
    pub fn shuffle_upcoming(&mut self) {
        let start = self.index.map_or(0, |i| i + 1);
        let mut seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
        for i in (start + 1..self.tracks.len()).rev() {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let j = start + seed as usize % (i - start + 1);
            self.tracks.swap(i, j);
        }
    }
}

pub struct Store {
    path: PathBuf,
}

impl Store {
    pub fn load() -> Result<(Self, Library)> {
        let base = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
            .context("Could not find your user data directory")?;
        let store = Self {
            path: base.join("yt-music-gpui/library.json"),
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
            library.queue.index = None;
        }
        library.recent.truncate(100);
        Ok((store, library))
    }

    pub fn save(&self, library: &Library) -> Result<()> {
        let directory = self.path.parent().context("Missing data directory")?;
        fs::create_dir_all(directory).context("Could not create the music data directory")?;
        let mut file = tempfile::NamedTempFile::new_in(directory)?;
        serde_json::to_writer(&mut file, library)?;
        file.write_all(b"\n")?;
        file.as_file().sync_all()?;
        file.persist(&self.path)
            .context("Could not save your music library")?;
        Ok(())
    }
}

pub struct LibraryWriter {
    requests: async_channel::Sender<Library>,
    pending: async_channel::Receiver<Library>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl LibraryWriter {
    pub fn new(store: Store) -> (Self, async_channel::Receiver<String>) {
        let (requests, pending) = async_channel::bounded::<Library>(1);
        let receiver = pending.clone();
        let (errors, failed) = async_channel::unbounded();
        let worker = std::thread::Builder::new()
            .name("library-writer".into())
            .spawn(move || {
                while let Ok(library) = receiver.recv_blocking() {
                    if let Err(error) = store.save(&library) {
                        let _ = errors.send_blocking(format!("{error:#}"));
                    }
                }
            })
            .expect("start library writer");
        (
            Self {
                requests,
                pending,
                worker: Some(worker),
            },
            failed,
        )
    }

    pub fn save(&self, library: &Library) {
        // Keep only the newest snapshot while a previous atomic write is in flight.
        let _ = self.pending.try_recv();
        let _ = self.requests.try_send(library.clone());
    }
}

impl Drop for LibraryWriter {
    fn drop(&mut self) {
        self.requests.close();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn writer_flushes_the_latest_snapshot_before_exit() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("library.json");
        let (writer, errors) = LibraryWriter::new(Store { path: path.clone() });
        let mut library = Library::new();
        for n in 0..40 {
            library.volume = n as f64;
            writer.save(&library);
        }
        drop(writer);
        assert!(errors.try_recv().is_err());
        let saved: Library = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(saved.volume, 39.);
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
