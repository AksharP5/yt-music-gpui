use crate::{
    auth::Session,
    catalog::{Catalog, Continuation, Item, Page},
    library::Queue,
    search::{SearchFilter, SearchScope},
};
use anyhow::Result;
use std::{
    collections::HashSet,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    thread,
};

#[derive(Clone)]
pub enum PageRequest {
    Browse,
    Search {
        query: String,
        filter: SearchFilter,
        scope: SearchScope,
    },
}

struct Request {
    id: u64,
    page: PageRequest,
    continuation: Continuation,
    session: Option<Arc<Session>>,
}

pub struct Event {
    id: u64,
    result: Result<Page, String>,
}

enum State {
    Ready,
    Loading(u64),
    Failed(String),
}

struct Source {
    id: Arc<()>,
    page: PageRequest,
    continuation: Continuation,
    session: Option<Arc<Session>>,
    seen: HashSet<String>,
    state: State,
}

pub struct Worker {
    requests: async_channel::Sender<Request>,
    pending: async_channel::Receiver<Request>,
    generation: Arc<AtomicU64>,
    source: Option<Source>,
}

impl Worker {
    pub fn start() -> (Self, async_channel::Receiver<Event>) {
        let mut catalog = None;
        Self::spawn(move |request| {
            let catalog = catalog
                .get_or_insert_with(Catalog::new)
                .as_ref()
                .map_err(|error| anyhow::anyhow!("{error:#}"))?
                .for_session(request.session.clone());
            match &request.page {
                PageRequest::Browse => catalog.more(&request.continuation),
                PageRequest::Search {
                    query,
                    filter,
                    scope,
                } => catalog.search_more(query, *filter, *scope, &request.continuation),
            }
        })
    }

    fn spawn(
        mut load: impl FnMut(&Request) -> Result<Page> + Send + 'static,
    ) -> (Self, async_channel::Receiver<Event>) {
        let (requests, pending) = async_channel::bounded::<Request>(1);
        let (events, completed) = async_channel::bounded(1);
        let generation = Arc::new(AtomicU64::new(0));
        let current = generation.clone();
        let jobs = pending.clone();
        thread::Builder::new()
            .name("queue-source".into())
            .spawn(move || {
                while let Ok(request) = jobs.recv_blocking() {
                    if request.id != current.load(Ordering::Acquire) {
                        continue;
                    }
                    let result = load(&request).map_err(|error| format!("{error:#}"));
                    if request.id == current.load(Ordering::Acquire)
                        && events
                            .force_send(Event {
                                id: request.id,
                                result,
                            })
                            .is_err()
                    {
                        break;
                    }
                }
            })
            .expect("start queue source worker");
        (
            Self {
                requests,
                pending,
                generation,
                source: None,
            },
            completed,
        )
    }

    /// Capture the page at Play all time, independently of later browsing.
    pub fn replace_queue(
        &mut self,
        queue: &mut Queue,
        page: &Page,
        selected: usize,
        request: PageRequest,
        session: Option<Arc<Session>>,
    ) {
        self.cancel();
        queue.replace(page.tracks(), selected);
        let Some(continuation) = &page.continuation else {
            return;
        };
        let id = Arc::new(());
        queue.incomplete = true;
        queue.source_id = Some(id.clone());
        self.source = Some(Source {
            id,
            page: request,
            continuation: continuation.clone(),
            session,
            seen: HashSet::from([continuation.token().to_owned()]),
            state: State::Ready,
        });
    }

    /// True means a page is pending. Only an explicit retry retries a failed page.
    pub fn load(&mut self, session: Option<&Arc<Session>>, retry: bool) -> Result<bool, String> {
        let Some(source) = &mut self.source else {
            return Ok(false);
        };
        if !same_session(source.session.as_ref(), session) {
            self.cancel();
            return Err(
                "The account changed. Open the playlist again to continue its queue.".into(),
            );
        }
        match &source.state {
            State::Loading(_) => return Ok(true),
            State::Failed(error) if !retry => return Err(error.clone()),
            _ => {}
        }
        let id = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        let request = Request {
            id,
            page: source.page.clone(),
            continuation: source.continuation.clone(),
            session: source.session.clone(),
        };
        if self.requests.force_send(request).is_err() {
            let error = "The queue connection stopped. Restart the app to reconnect.".to_owned();
            source.state = State::Failed(error.clone());
            return Err(error);
        }
        source.state = State::Loading(id);
        Ok(true)
    }

    /// Append occurrences exactly once, rejecting replacements and account changes.
    /// A true result can contain no tracks but still advance the continuation.
    pub fn apply(
        &mut self,
        event: Event,
        queue: &mut Queue,
        session: Option<&Arc<Session>>,
    ) -> Result<bool, String> {
        let Some(source) = &mut self.source else {
            return Ok(false);
        };
        if !matches!(source.state, State::Loading(id) if id == event.id)
            || event.id != self.generation.load(Ordering::Acquire)
        {
            return Ok(false);
        }
        if !queue
            .source_id
            .as_ref()
            .is_some_and(|id| Arc::ptr_eq(id, &source.id))
        {
            self.cancel();
            return Ok(false);
        }
        if !same_session(source.session.as_ref(), session) {
            self.cancel();
            return Err(
                "The account changed. Open the playlist again to continue its queue.".into(),
            );
        }
        let page = match event.result {
            Ok(page) => page,
            Err(error) => {
                source.state = State::Failed(error.clone());
                return Err(error);
            }
        };
        if let Some(next) = &page.continuation
            && !source.seen.insert(next.token().to_owned())
        {
            let error = "YouTube Music repeated a queue page. Open the playlist again to continue."
                .to_owned();
            source.state = State::Failed(error.clone());
            return Err(error);
        }
        queue.tracks.extend(
            page.shelves
                .into_iter()
                .flat_map(|shelf| shelf.items)
                .filter_map(|item| match item {
                    Item::Track(track) => Some(track),
                    _ => None,
                }),
        );
        queue.incomplete = page.continuation.is_some();
        match page.continuation {
            Some(continuation) => {
                source.continuation = continuation;
                source.state = State::Ready;
            }
            None => {
                self.source = None;
                queue.source_id = None;
            }
        }
        Ok(true)
    }

    pub fn can_continue(&self) -> bool {
        self.source.is_some()
    }

    /// Cancel on queue replacement, clear, radio start, and every account transition.
    /// The queue's incomplete marker remains until a new queue explicitly replaces it.
    pub fn cancel(&mut self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        let _ = self.pending.try_recv();
        self.source = None;
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.cancel();
        self.requests.close();
    }
}

fn same_session(expected: Option<&Arc<Session>>, current: Option<&Arc<Session>>) -> bool {
    match (expected, current) {
        (None, None) => true,
        (Some(expected), Some(current)) => Arc::ptr_eq(expected, current),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        catalog::Shelf,
        library::{Library, Repeat},
    };
    use serde_json::json;
    use std::{collections::VecDeque, sync::mpsc, time::Duration};

    fn track(id: &str) -> crate::catalog::Track {
        crate::catalog::Track {
            id: id.into(),
            title: id.into(),
            artist: String::new(),
            album: None,
            duration: None,
            thumbnail: None,
            details: Default::default(),
        }
    }

    fn page(ids: &[&str], token: Option<&str>) -> Page {
        let mut response = json!({"contents": []});
        if let Some(token) = token {
            response["continuations"] = json!([{"nextContinuationData":{"continuation":token}}]);
        }
        let mut page = crate::catalog::parse_page(&response, "Test playlist").unwrap();
        page.shelves = vec![Shelf {
            title: String::new(),
            items: ids.iter().map(|id| Item::Track(track(id))).collect(),
        }];
        page
    }

    fn ids(queue: &Queue) -> Vec<&str> {
        queue.tracks.iter().map(|track| track.id.as_str()).collect()
    }

    fn session() -> Arc<Session> {
        Arc::new(
            serde_json::from_value(json!({
                "cookie":"synthetic", "playback_cookie":"synthetic",
                "authuser":"0", "delegated_session_id":null,
                "data_sync_id":"synthetic", "client_version":"synthetic"
            }))
            .unwrap(),
        )
    }

    #[test]
    fn two_page_queue_preserves_duplicates_before_autoplay_on_or_off() {
        for autoplay in [false, true] {
            for natural in [false, true] {
                let (mut worker, completed) = Worker::spawn(|request| {
                    assert_eq!(request.continuation.token(), "second-page");
                    Ok(page(&["b", "a"], None))
                });
                let mut queue = Queue::default();
                worker.replace_queue(
                    &mut queue,
                    &page(&["a", "a"], Some("second-page")),
                    0,
                    PageRequest::Browse,
                    None,
                );
                let mut recommendations = VecDeque::from([track("recommended")]);
                assert!(
                    !Library {
                        queue: queue.clone(),
                        autoplay,
                        ..Default::default()
                    }
                    .music_autoplay()
                );
                assert!(queue.advance_with_autoplay(
                    Repeat::Off,
                    natural,
                    autoplay,
                    &mut recommendations
                ));
                assert_eq!(queue.index, Some(1));
                for repeat in [Repeat::Off, Repeat::All] {
                    assert!(!queue.advance_with_autoplay(
                        repeat,
                        natural,
                        autoplay,
                        &mut recommendations
                    ));
                    assert_eq!(queue.index, Some(1));
                    assert_eq!(recommendations.len(), 1);
                }
                assert!(queue.advance(Repeat::One, true));
                assert!(worker.load(None, false).unwrap());
                assert!(worker.load(None, false).unwrap());
                assert!(
                    worker
                        .apply(completed.recv_blocking().unwrap(), &mut queue, None)
                        .unwrap()
                );
                assert_eq!(ids(&queue), ["a", "a", "b", "a"]);
                assert!(!queue.incomplete);
                assert_eq!(
                    Library {
                        queue: queue.clone(),
                        autoplay,
                        ..Default::default()
                    }
                    .music_autoplay(),
                    autoplay
                );
                for index in [2, 3] {
                    assert!(queue.advance_with_autoplay(
                        Repeat::Off,
                        natural,
                        autoplay,
                        &mut recommendations
                    ));
                    assert_eq!(queue.index, Some(index));
                    assert_eq!(recommendations.len(), 1);
                }
                assert_eq!(
                    queue.advance_with_autoplay(
                        Repeat::Off,
                        natural,
                        autoplay,
                        &mut recommendations
                    ),
                    autoplay
                );
                assert_eq!(recommendations.is_empty(), autoplay);
                assert!(!worker.load(None, false).unwrap());
                assert!(queue.advance(Repeat::All, natural));
                assert_eq!(queue.index, Some(0));
            }
        }
    }

    #[test]
    fn replacement_rejects_an_event_already_delivered_to_the_ui() {
        let (mut worker, completed) = Worker::spawn(|_| Ok(page(&["old-tail"], None)));
        let mut queue = Queue::default();
        worker.replace_queue(
            &mut queue,
            &page(&["old"], Some("tail")),
            0,
            PageRequest::Browse,
            None,
        );
        worker.load(None, false).unwrap();
        let event = completed.recv_blocking().unwrap();
        queue.replace(vec![track("replacement")], 0);
        assert!(!worker.apply(event, &mut queue, None).unwrap());
        assert_eq!(ids(&queue), ["replacement"]);
        assert!(!worker.load(None, false).unwrap());
    }

    #[test]
    fn cancellation_discards_in_flight_and_queued_requests() {
        let (started, began) = mpsc::channel();
        let (release, resume) = mpsc::channel();
        let (mut worker, completed) = Worker::spawn(move |request| {
            let token = request.continuation.token();
            started.send(token.to_owned()).unwrap();
            if token == "blocked" {
                resume.recv().unwrap();
            }
            Ok(page(&[token], None))
        });
        let mut queue = Queue::default();
        worker.replace_queue(
            &mut queue,
            &page(&["old"], Some("blocked")),
            0,
            PageRequest::Browse,
            None,
        );
        worker.load(None, false).unwrap();
        assert_eq!(
            began.recv_timeout(Duration::from_secs(2)).unwrap(),
            "blocked"
        );
        for token in ["discarded", "latest"] {
            worker.replace_queue(
                &mut queue,
                &page(&["replacement"], Some(token)),
                0,
                PageRequest::Browse,
                None,
            );
            worker.load(None, false).unwrap();
        }
        release.send(()).unwrap();
        assert_eq!(
            began.recv_timeout(Duration::from_secs(2)).unwrap(),
            "latest"
        );
        assert!(
            worker
                .apply(completed.recv_blocking().unwrap(), &mut queue, None)
                .unwrap()
        );
        assert_eq!(ids(&queue), ["replacement", "latest"]);
        assert!(completed.try_recv().is_err());
    }

    #[test]
    fn exact_session_identity_is_required_before_dispatch_and_after_completion() {
        let session = session();
        let other = Arc::new((*session).clone());
        let (mut worker, completed) = Worker::spawn(|_| Ok(page(&["private-tail"], None)));
        let mut queue = Queue::default();
        for current in [None, Some(&other)] {
            worker.replace_queue(
                &mut queue,
                &page(&["loaded"], Some("tail")),
                0,
                PageRequest::Browse,
                Some(session.clone()),
            );
            assert!(worker.load(current, false).is_err());
            assert!(completed.try_recv().is_err());
            worker.replace_queue(
                &mut queue,
                &page(&["loaded"], Some("tail")),
                0,
                PageRequest::Browse,
                Some(session.clone()),
            );
            worker.load(Some(&session), false).unwrap();
            assert!(
                worker
                    .apply(completed.recv_blocking().unwrap(), &mut queue, current)
                    .is_err()
            );
            assert_eq!(ids(&queue), ["loaded"]);
            assert!(queue.incomplete);
            assert!(!worker.load(current, true).unwrap());
        }
    }

    #[test]
    fn failed_pages_block_recommendations_until_an_explicit_retry_succeeds() {
        let mut calls = 0;
        let (mut worker, completed) = Worker::spawn(move |_| {
            calls += 1;
            if calls == 1 {
                anyhow::bail!("synthetic connection failure");
            }
            Ok(page(&["tail"], None))
        });
        let mut queue = Queue::default();
        worker.replace_queue(
            &mut queue,
            &page(&["loaded"], Some("tail")),
            0,
            PageRequest::Browse,
            None,
        );
        worker.load(None, false).unwrap();
        assert!(
            worker
                .apply(completed.recv_blocking().unwrap(), &mut queue, None)
                .is_err()
        );
        let mut recommendations = VecDeque::from([track("recommended")]);
        assert!(!queue.advance_with_autoplay(Repeat::Off, true, true, &mut recommendations));
        assert_eq!(recommendations.len(), 1);
        assert!(worker.load(None, false).is_err());
        worker.load(None, true).unwrap();
        worker
            .apply(completed.recv_blocking().unwrap(), &mut queue, None)
            .unwrap();
        assert!(queue.advance_with_autoplay(Repeat::Off, true, true, &mut recommendations));
        assert_eq!(queue.current().unwrap().id, "tail");
        assert_eq!(recommendations.len(), 1);
    }

    #[test]
    fn empty_pages_continue_and_repeated_tokens_are_errors_not_exhaustion() {
        for last_token in [None, Some("first")] {
            let mut responses = VecDeque::from([page(&[], Some("second")), page(&[], last_token)]);
            let (mut worker, completed) =
                Worker::spawn(move |_| Ok(responses.pop_front().unwrap()));
            let mut queue = Queue::default();
            worker.replace_queue(
                &mut queue,
                &page(&["loaded"], Some("first")),
                0,
                PageRequest::Browse,
                None,
            );
            worker.load(None, false).unwrap();
            worker
                .apply(completed.recv_blocking().unwrap(), &mut queue, None)
                .unwrap();
            assert!(queue.incomplete);
            assert!(!queue.advance(Repeat::All, true));
            worker.load(None, false).unwrap();
            assert_eq!(
                worker
                    .apply(completed.recv_blocking().unwrap(), &mut queue, None)
                    .is_err(),
                last_token.is_some()
            );
            assert_eq!(queue.incomplete, last_token.is_some());
            assert_eq!(ids(&queue), ["loaded"]);
        }
    }

    #[test]
    fn queue_edits_keep_the_source_and_persistence_omits_request_context() {
        let (mut worker, completed) = Worker::spawn(|_| Ok(page(&["a", "tail"], None)));
        let mut queue = Queue::default();
        worker.replace_queue(
            &mut queue,
            &page(&["a", "remove", "b", "c"], Some("private-continuation")),
            0,
            PageRequest::Browse,
            Some(session()),
        );
        queue.tracks.insert(1, track("play-next"));
        assert!(!queue.remove(2));
        queue.shuffle_upcoming();
        let loaded = queue.tracks.clone();
        let saved = serde_json::to_value(&queue).unwrap();
        assert_eq!(saved.as_object().unwrap().len(), 3);
        let mut restored: Queue = serde_json::from_value(saved).unwrap();
        assert!(restored.incomplete);
        assert!(restored.source_id.is_none());
        restored.select(restored.tracks.len() - 1);
        let mut recommendations = VecDeque::from([track("recommended")]);
        assert!(!restored.advance_with_autoplay(Repeat::All, true, true, &mut recommendations));
        let session = worker.source.as_ref().unwrap().session.clone();
        worker.load(session.as_ref(), false).unwrap();
        worker
            .apply(
                completed.recv_blocking().unwrap(),
                &mut queue,
                session.as_ref(),
            )
            .unwrap();
        assert_eq!(queue.tracks[..loaded.len()], loaded);
        assert_eq!(&ids(&queue)[loaded.len()..], ["a", "tail"]);
        assert_eq!(queue.current().unwrap().id, "a");
    }
}
