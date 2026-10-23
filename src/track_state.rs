use crate::{
    auth::Session,
    catalog::{Catalog, Track},
};
use anyhow::{Context, Result};
use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    thread,
};

struct Request {
    id: u64,
    track: Track,
    session: Arc<Session>,
}

pub struct Event {
    pub id: u64,
    pub result: Result<Track, String>,
}

/// Refresh the current song without replacing radio or text requests.
pub struct Worker {
    requests: async_channel::Sender<Request>,
    pending: async_channel::Receiver<Request>,
    generation: Arc<AtomicU64>,
}

impl Worker {
    pub fn start() -> (Self, async_channel::Receiver<Event>) {
        let (requests, pending) = async_channel::bounded::<Request>(1);
        let (events, completed) = async_channel::unbounded();
        let generation = Arc::new(AtomicU64::new(0));
        let current = generation.clone();
        let jobs = pending.clone();
        thread::Builder::new()
            .name("current-song".into())
            .spawn(move || {
                let catalog = Catalog::new();
                while let Ok(request) = jobs.recv_blocking() {
                    if request.id != current.load(Ordering::Acquire) {
                        continue;
                    }
                    let result = (|| {
                        catalog
                            .as_ref()
                            .map_err(|error| anyhow::anyhow!("{error:#}"))?
                            .for_session(Some(request.session))
                            .watch(&request.track, false)?
                            .current
                            .filter(|track| track.id == request.track.id)
                            .context(
                                "YouTube Music did not return the current song's account state",
                            )
                    })()
                    .map_err(|error| format!("{error:#}"));
                    if request.id == current.load(Ordering::Acquire)
                        && events
                            .send_blocking(Event {
                                id: request.id,
                                result,
                            })
                            .is_err()
                    {
                        break;
                    }
                }
            })
            .expect("start current song worker");
        (
            Self {
                requests,
                pending,
                generation,
            },
            completed,
        )
    }

    pub fn load(&self, track: Track, session: Arc<Session>) -> Result<u64> {
        let id = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        self.requests
            .force_send(Request { id, track, session })
            .map_err(|_| {
                anyhow::anyhow!("The current song connection stopped. Restart the app.")
            })?;
        Ok(id)
    }

    pub fn cancel(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        let _ = self.pending.try_recv();
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.cancel();
        self.requests.close();
    }
}
