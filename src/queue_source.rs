use crate::{
    auth::{self, Session},
    catalog::{BrowseLink, Catalog, Continuation, Item, Page, Track, WatchTarget},
    library::Queue,
    search::{SearchFilter, SearchScope},
    watch::{Watch, WatchContinuation},
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
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
    Collection(String),
    Search {
        query: String,
        filter: SearchFilter,
        scope: SearchScope,
    },
}

/// Only public request fields and consumed track IDs, never continuation tokens,
/// visitor data or credentials. Queue edits do not change the consumed source.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct SavedSource {
    target: SavedTarget,
    loaded: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
enum SavedTarget {
    Browse(String),
    Watch(WatchTarget),
}

impl SavedTarget {
    fn is_generated(&self) -> bool {
        matches!(self, Self::Watch(target) if target.is_generated())
    }

    fn load(&self, catalog: &Catalog) -> Result<QueuePage> {
        match self {
            Self::Browse(id) => {
                ensure!(
                    id.starts_with("VLPL")
                        || id.starts_with("VLOLAK5uy")
                        || id.starts_with("MPREb"),
                    "This saved queue has no reloadable collection"
                );
                let page = catalog.browse(&BrowseLink::new(id, "Saved queue"))?;
                Ok(QueuePage {
                    tracks: page.tracks(),
                    continuation: page
                        .continuation
                        .map(|continuation| SourceContinuation::Page {
                            page: PageRequest::Browse,
                            continuation,
                        }),
                })
            }
            Self::Watch(target) => {
                ensure!(
                    target.restorable(),
                    "This saved queue has no reloadable playlist"
                );
                let watch = catalog.watch_target(target)?;
                Ok(QueuePage {
                    tracks: watch.queue,
                    continuation: watch.continuation.map(SourceContinuation::Watch),
                })
            }
        }
    }
}

#[derive(Clone)]
enum SourceContinuation {
    Restore(SavedSource),
    Page {
        page: PageRequest,
        continuation: Continuation,
    },
    Watch(WatchContinuation),
}

impl SourceContinuation {
    fn token(&self) -> &str {
        match self {
            Self::Restore(_) => "",
            Self::Page { continuation, .. } => continuation.token(),
            Self::Watch(continuation) => continuation.token(),
        }
    }

    fn renew_session(&mut self, expected: &Arc<Session>, next: &Arc<Session>) -> Result<()> {
        match self {
            Self::Restore(_) => expected.require_same_account(next),
            Self::Page { continuation, .. } => continuation.renew_session(expected, next),
            Self::Watch(continuation) => continuation.renew_session(expected, next),
        }
    }
}

fn load_page(catalog: &Catalog, continuation: &SourceContinuation) -> Result<QueuePage> {
    match continuation {
        SourceContinuation::Restore(saved) => {
            let first = saved.target.load(catalog)?;
            if saved.target.is_generated() {
                // Mixes regenerate under the current session. The append boundary
                // deduplicates them instead of requiring a static playlist prefix.
                Ok(first)
            } else {
                resume_prefix(saved, first, |next| load_page(catalog, next))
            }
        }
        SourceContinuation::Page { page, continuation } => {
            let response = match page {
                PageRequest::Browse | PageRequest::Collection(_) => catalog.more(continuation),
                PageRequest::Search {
                    query,
                    filter,
                    scope,
                } => catalog.search_more(query, *filter, *scope, continuation),
            }?;
            Ok(QueuePage {
                tracks: response
                    .shelves
                    .into_iter()
                    .flat_map(|shelf| shelf.items)
                    .filter_map(|item| match item {
                        Item::Track(track) => Some(track),
                        _ => None,
                    })
                    .collect(),
                continuation: response
                    .continuation
                    .map(|continuation| SourceContinuation::Page {
                        page: page.clone(),
                        continuation,
                    }),
            })
        }
        SourceContinuation::Watch(continuation) => {
            let response = catalog.watch_more(continuation)?;
            Ok(QueuePage {
                tracks: response.queue,
                continuation: response.continuation.map(SourceContinuation::Watch),
            })
        }
    }
}

fn resume_prefix(
    saved: &SavedSource,
    mut page: QueuePage,
    mut more: impl FnMut(&SourceContinuation) -> Result<QueuePage>,
) -> Result<QueuePage> {
    let mut consumed = 0;
    let mut seen = HashSet::new();
    loop {
        let skip = page.tracks.len().min(saved.loaded.len() - consumed);
        ensure!(
            page.tracks
                .iter()
                .take(skip)
                .map(|track| &track.id)
                .eq(&saved.loaded[consumed..consumed + skip]),
            "The playlist or account changed since this queue was saved. Open the playlist again to continue."
        );
        consumed += skip;
        if consumed == saved.loaded.len() {
            page.tracks.drain(..skip);
            return Ok(page);
        }
        let next = page.continuation.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "The saved playlist is now shorter. Open the playlist again to continue."
            )
        })?;
        ensure!(
            seen.insert(next.token().to_owned()),
            "YouTube Music repeated a saved queue page. Open the playlist again to continue."
        );
        page = more(next)?;
    }
}

struct QueuePage {
    tracks: Vec<Track>,
    continuation: Option<SourceContinuation>,
}

struct Request {
    id: u64,
    continuation: SourceContinuation,
    session: Option<Arc<Session>>,
}

pub struct Event {
    id: u64,
    result: Result<QueuePage, String>,
    session_rejected: bool,
    session: Option<Arc<Session>>,
}
impl Event {
    pub fn rejected_for(&self, session: Option<&Arc<Session>>) -> bool {
        self.session_rejected && same_session(self.session.as_ref(), session)
    }
}

enum State {
    Ready,
    Loading(u64),
    Failed(String),
}

struct Source {
    id: Arc<()>,
    continuation: SourceContinuation,
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
            load_page(&catalog, &request.continuation)
        })
    }

    fn spawn(
        mut load: impl FnMut(&Request) -> Result<QueuePage> + Send + 'static,
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
                    let result = load(&request);
                    let session_rejected = result.as_ref().is_err_and(auth::session_was_rejected);
                    let result = result.map_err(|error| format!("{error:#}"));
                    if request.id == current.load(Ordering::Acquire)
                        && events
                            .force_send(Event {
                                id: request.id,
                                result,
                                session_rejected,
                                session: request.session,
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
        queue.replace(page.tracks(), selected);
        queue.playing_from = Some(page.title.clone()).filter(|title| !title.is_empty());
        if let PageRequest::Collection(id) = &request {
            queue.source = Some(SavedSource {
                target: SavedTarget::Browse(id.clone()),
                loaded: queue.tracks.iter().map(|track| track.id.clone()).collect(),
            });
        }
        self.set_source(
            queue,
            page.continuation
                .as_ref()
                .map(|continuation| SourceContinuation::Page {
                    page: request,
                    continuation: continuation.clone(),
                }),
            session,
        );
    }

    /// An issued playback queue continues independently of optional recommendations.
    pub fn attach_watch(
        &mut self,
        queue: &mut Queue,
        watch: &Watch,
        session: Option<Arc<Session>>,
    ) {
        queue.source = watch.source_target.clone().map(|target| SavedSource {
            target: SavedTarget::Watch(target),
            loaded: watch.queue.iter().map(|track| track.id.clone()).collect(),
        });
        self.set_source(
            queue,
            watch
                .continuation
                .as_ref()
                .map(|continuation| SourceContinuation::Watch(continuation.clone())),
            session,
        );
    }

    /// Reacquire under the current session only when playback needs more tracks.
    /// Static collections verify their prefix; mixes append fresh recommendations.
    pub fn restore(&mut self, queue: &mut Queue, session: Option<&Arc<Session>>) {
        if self.source.is_none()
            && queue.incomplete
            && let Some(saved) = queue.source.clone()
        {
            self.set_source(
                queue,
                Some(SourceContinuation::Restore(saved)),
                session.cloned(),
            );
        }
    }

    fn set_source(
        &mut self,
        queue: &mut Queue,
        continuation: Option<SourceContinuation>,
        session: Option<Arc<Session>>,
    ) {
        self.cancel();
        queue.source_id = None;
        queue.incomplete = continuation.is_some();
        let Some(continuation) = continuation else {
            queue.source = None;
            return;
        };
        let id = Arc::new(());
        queue.source_id = Some(id.clone());
        self.source = Some(Source {
            id,
            seen: HashSet::from([continuation.token().to_owned()]),
            continuation,
            session,
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
        let mut page = match event.result {
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
        let generated = queue
            .source
            .as_ref()
            .is_some_and(|saved| saved.target.is_generated());
        if let Some(saved) = &mut queue.source {
            if generated {
                let mut seen: HashSet<_> = saved
                    .loaded
                    .iter()
                    .chain(queue.tracks.iter().map(|track| &track.id))
                    .cloned()
                    .collect();
                page.tracks.retain(|track| seen.insert(track.id.clone()));
            }
            saved
                .loaded
                .extend(page.tracks.iter().map(|track| track.id.clone()));
        }
        queue.append_source(page.tracks);
        queue.incomplete = page.continuation.is_some();
        if !queue.incomplete {
            queue.source = None;
        }
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
        if queue.shuffled && queue.incomplete && !generated {
            self.load(session, false)?;
        }
        Ok(true)
    }

    /// Shuffle the entire source, including pages that have not arrived yet.
    pub fn shuffle(
        &mut self,
        queue: &mut Queue,
        session: Option<&Arc<Session>>,
    ) -> Result<(), String> {
        self.restore(queue, session);
        queue.shuffle_upcoming();
        self.load(session, true)?;
        Ok(())
    }

    pub fn can_continue(&self) -> bool {
        self.source.is_some()
    }

    /// Rebind only after verified same-account renewal and player activation.
    pub fn renew_session(
        &mut self,
        expected: &Arc<Session>,
        next: Arc<Session>,
    ) -> Result<bool, String> {
        let Some(source) = &mut self.source else {
            return Ok(false);
        };
        if !same_session(source.session.as_ref(), Some(expected)) {
            return Err(
                "The account changed. Open the playlist again to continue its queue.".into(),
            );
        }
        source
            .continuation
            .renew_session(expected, &next)
            .map_err(|error| format!("{error:#}"))?;
        self.generation.fetch_add(1, Ordering::AcqRel);
        let _ = self.pending.try_recv();
        source.session = Some(next);
        source.state = State::Ready;
        Ok(true)
    }

    /// Cancel on queue replacement, clear, radio start, and account switches.
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
        catalog::{Item, Shelf},
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
            layout: crate::catalog::ShelfLayout::Songs,
            more: None,
            title: String::new(),
            items: ids.iter().map(|id| Item::Track(track(id))).collect(),
        }];
        page
    }

    fn next_page(ids: &[&str], token: Option<&str>) -> QueuePage {
        let page = page(ids, token);
        QueuePage {
            tracks: page.tracks(),
            continuation: page
                .continuation
                .map(|continuation| SourceContinuation::Page {
                    page: PageRequest::Browse,
                    continuation,
                }),
        }
    }

    fn watch(ids: &[&str], token: Option<&str>, session: Option<Arc<Session>>) -> Watch {
        let mut panel = json!({
            "contents": ids.iter().enumerate().map(|(index, id)| json!({
                "playlistPanelVideoRenderer": {
                    "videoId": id,
                    "title": {"simpleText": id},
                    "selected": index == 0
                }
            })).collect::<Vec<_>>()
        });
        if let Some(token) = token {
            panel["continuations"] = json!([{"nextRadioContinuationData":{"continuation":token}}]);
        }
        let response = json!({"contents":{"singleColumnMusicWatchNextResultsRenderer":{
            "tabbedRenderer":{"watchNextTabbedResultsRenderer":{"tabs":[{
                "tabRenderer":{"content":{"musicQueueRenderer":{"content":{
                    "playlistPanelRenderer":panel
                }}}}
            }]}}
        }}});
        crate::watch::parse_playlist_watch(&response, json!({"playlistId":"PLsynthetic"}), session)
            .unwrap()
    }

    fn watch_page(ids: &[&str], token: Option<&str>, session: Option<Arc<Session>>) -> QueuePage {
        let watch = watch(ids, token, session);
        QueuePage {
            tracks: watch.queue,
            continuation: watch.continuation.map(SourceContinuation::Watch),
        }
    }

    fn ids(queue: &Queue) -> Vec<&str> {
        queue.tracks.iter().map(|track| track.id.as_str()).collect()
    }

    fn session() -> Arc<Session> {
        account("synthetic-user||", None)
    }

    fn account(identity: &str, delegated: Option<&str>) -> Arc<Session> {
        Arc::new(
            serde_json::from_value(json!({
                "cookie":"synthetic", "playback_cookie":"synthetic",
                "authuser":"0", "delegated_session_id":delegated,
                "data_sync_id":identity, "client_version":"synthetic"
            }))
            .unwrap(),
        )
    }

    fn continuation(
        watch_source: bool,
        token: &str,
        session: Option<Arc<Session>>,
    ) -> SourceContinuation {
        if watch_source {
            return SourceContinuation::Watch(
                watch(&["abcdefghijk"], Some(token), session)
                    .continuation
                    .unwrap(),
            );
        }
        let continuation = Catalog::with_session(session)
            .unwrap()
            .bind_page(page(&[], Some(token)), &json!({}), None)
            .unwrap()
            .continuation
            .unwrap();
        SourceContinuation::Page {
            page: PageRequest::Browse,
            continuation,
        }
    }

    #[test]
    fn renewal_preserves_the_source_and_queue_but_rejects_a_delivered_old_reply() {
        for watch_source in [false, true] {
            let expected = session();
            let mut next = (*expected).clone();
            next.client_version = "renewed".into();
            let next = Arc::new(next);
            let renewed = next.clone();
            let mut calls = 0;
            let (mut worker, completed) = Worker::spawn(move |request| {
                calls += 1;
                if calls == 1 {
                    return Ok(next_page(&["stale-tail"], None));
                }
                assert!(same_session(request.session.as_ref(), Some(&renewed)));
                request
                    .continuation
                    .clone()
                    .renew_session(&renewed, &renewed)?;
                Ok(QueuePage {
                    tracks: vec![track("b"), track("a")],
                    continuation: Some(continuation(watch_source, "more", Some(renewed.clone()))),
                })
            });
            let mut queue = Queue::default();
            queue.replace(vec![track("a"), track("a"), track("b")], 1);
            worker.set_source(
                &mut queue,
                Some(continuation(watch_source, "tail", Some(expected.clone()))),
                Some(expected.clone()),
            );
            worker.load(Some(&expected), false).unwrap();
            let old_reply = completed.recv_blocking().unwrap();
            let source_id = queue.source_id.clone().unwrap();
            assert!(worker.renew_session(&expected, next.clone()).unwrap());
            let source = worker.source.as_ref().unwrap();
            assert!(Arc::ptr_eq(&source.id, &source_id));
            assert_eq!(source.continuation.token(), "tail");
            assert_eq!(source.seen, HashSet::from(["tail".to_owned()]));
            assert!(!worker.apply(old_reply, &mut queue, Some(&next)).unwrap());
            assert_eq!(ids(&queue), ["a", "a", "b"]);
            assert_eq!(queue.index, Some(1));
            worker.load(Some(&next), false).unwrap();
            worker
                .apply(completed.recv_blocking().unwrap(), &mut queue, Some(&next))
                .unwrap();
            assert_eq!(ids(&queue), ["a", "a", "b", "b", "a"]);
            assert_eq!(queue.index, Some(1));
            assert!(queue.incomplete);
            assert!(Arc::ptr_eq(queue.source_id.as_ref().unwrap(), &source_id));
            assert_eq!(
                worker.source.as_ref().unwrap().seen,
                HashSet::from(["tail".to_owned(), "more".to_owned()])
            );
        }
    }

    #[test]
    fn renewal_discards_in_flight_and_queued_requests() {
        for watch_source in [false, true] {
            let expected = session();
            let next = Arc::new((*expected).clone());
            let renewed = next.clone();
            let (started, began) = mpsc::channel();
            let (release, resume) = mpsc::channel();
            let mut calls = 0;
            let (mut worker, completed) = Worker::spawn(move |request| {
                calls += 1;
                started.send(request.id).unwrap();
                if calls == 1 {
                    resume.recv().unwrap();
                } else {
                    assert!(same_session(request.session.as_ref(), Some(&renewed)));
                    request
                        .continuation
                        .clone()
                        .renew_session(&renewed, &renewed)?;
                }
                Ok(next_page(
                    &[if calls == 1 { "stale" } else { "tail" }],
                    None,
                ))
            });
            let mut queue = Queue::default();
            queue.replace(vec![track("loaded")], 0);
            worker.set_source(
                &mut queue,
                Some(continuation(watch_source, "tail", Some(expected.clone()))),
                Some(expected.clone()),
            );
            worker.load(Some(&expected), false).unwrap();
            began.recv_timeout(Duration::from_secs(2)).unwrap();
            worker.renew_session(&expected, next.clone()).unwrap();
            worker.load(Some(&next), false).unwrap();
            worker.renew_session(&next, next.clone()).unwrap();
            worker.load(Some(&next), false).unwrap();
            let latest = worker.generation.load(Ordering::Acquire);
            release.send(()).unwrap();
            assert_eq!(began.recv_timeout(Duration::from_secs(2)).unwrap(), latest);
            worker
                .apply(completed.recv_blocking().unwrap(), &mut queue, Some(&next))
                .unwrap();
            assert_eq!(ids(&queue), ["loaded", "tail"]);
            assert!(!queue.incomplete);
            assert!(completed.try_recv().is_err());
            assert!(began.try_recv().is_err());
        }
    }

    #[test]
    fn renewal_rejects_unrelated_bindings_without_changing_failed_source_state() {
        let expected = session();
        let equivalent = Arc::new((*expected).clone());
        let brand = account("brand-one||synthetic-user", Some("brand-one"));
        for watch_source in [false, true] {
            for (bound, source_session, expected, next) in [
                (None, None, expected.clone(), expected.clone()),
                (
                    None,
                    Some(expected.clone()),
                    expected.clone(),
                    expected.clone(),
                ),
                (
                    Some(equivalent.clone()),
                    Some(expected.clone()),
                    expected.clone(),
                    expected.clone(),
                ),
                (
                    Some(equivalent.clone()),
                    Some(equivalent.clone()),
                    expected.clone(),
                    expected.clone(),
                ),
                (
                    Some(expected.clone()),
                    Some(expected.clone()),
                    expected.clone(),
                    account("other-primary||", None),
                ),
                (
                    Some(brand.clone()),
                    Some(brand.clone()),
                    brand.clone(),
                    account("brand-two||synthetic-user", Some("brand-two")),
                ),
                (
                    Some(brand.clone()),
                    Some(brand.clone()),
                    brand.clone(),
                    account("brand-one||other-primary", Some("brand-one")),
                ),
            ] {
                let (mut worker, completed) =
                    Worker::spawn(|_| anyhow::bail!("synthetic page failure"));
                let mut queue = Queue::default();
                queue.replace(vec![track("loaded")], 0);
                worker.set_source(
                    &mut queue,
                    Some(continuation(watch_source, "tail", bound.clone())),
                    source_session.clone(),
                );
                worker.load(source_session.as_ref(), false).unwrap();
                assert!(
                    worker
                        .apply(
                            completed.recv_blocking().unwrap(),
                            &mut queue,
                            source_session.as_ref()
                        )
                        .is_err()
                );
                let generation = worker.generation.load(Ordering::Acquire);
                let source_id = queue.source_id.clone().unwrap();
                assert!(worker.renew_session(&expected, next).is_err());
                let source = worker.source.as_ref().unwrap();
                assert!(Arc::ptr_eq(&source.id, &source_id));
                assert!(same_session(
                    source.session.as_ref(),
                    source_session.as_ref()
                ));
                assert_eq!(source.continuation.token(), "tail");
                assert_eq!(source.seen, HashSet::from(["tail".to_owned()]));
                if let Some(bound) = bound {
                    source
                        .continuation
                        .clone()
                        .renew_session(&bound, &bound)
                        .unwrap();
                }
                assert_eq!(worker.generation.load(Ordering::Acquire), generation);
                assert_eq!(
                    worker.load(source_session.as_ref(), false).unwrap_err(),
                    "synthetic page failure"
                );
                assert_eq!(ids(&queue), ["loaded"]);
                assert!(queue.incomplete);
            }
        }
    }

    #[test]
    fn healthy_same_snapshot_renewal_retries_a_failed_page() {
        for watch_source in [false, true] {
            let expected = session();
            let mut calls = 0;
            let (mut worker, completed) = Worker::spawn(move |_| {
                calls += 1;
                if calls == 1 {
                    return Err(auth::RejectedSession.into());
                }
                Ok(next_page(&["tail"], None))
            });
            let mut queue = Queue::default();
            queue.replace(vec![track("loaded")], 0);
            worker.set_source(
                &mut queue,
                Some(continuation(watch_source, "tail", Some(expected.clone()))),
                Some(expected.clone()),
            );
            worker.load(Some(&expected), false).unwrap();
            let rejected = completed.recv_blocking().unwrap();
            assert!(rejected.rejected_for(Some(&expected)));
            assert!(worker.apply(rejected, &mut queue, Some(&expected)).is_err());
            assert!(worker.load(Some(&expected), false).is_err());
            assert!(worker.renew_session(&expected, expected.clone()).unwrap());
            worker.load(Some(&expected), false).unwrap();
            worker
                .apply(
                    completed.recv_blocking().unwrap(),
                    &mut queue,
                    Some(&expected),
                )
                .unwrap();
            assert_eq!(ids(&queue), ["loaded", "tail"]);
            assert!(!queue.incomplete);
            assert!(!worker.renew_session(&expected, expected.clone()).unwrap());
        }
    }

    #[test]
    fn two_page_queue_preserves_duplicates_before_autoplay_on_or_off() {
        for autoplay in [false, true] {
            for natural in [false, true] {
                let (mut worker, completed) = Worker::spawn(|request| {
                    assert_eq!(request.continuation.token(), "second-page");
                    Ok(next_page(&["b", "a"], None))
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
    fn issued_watch_queue_continues_with_autoplay_off_before_repeating() {
        let (mut worker, completed) = Worker::spawn(|request| {
            assert!(matches!(request.continuation, SourceContinuation::Watch(_)));
            assert_eq!(request.continuation.token(), "watch-tail");
            Ok(watch_page(&["lmnopqrstuv", "abcdefghijk"], None, None))
        });
        let watch = watch(&["abcdefghijk", "abcdefghijk"], Some("watch-tail"), None);
        let mut library = Library {
            autoplay: false,
            repeat: Repeat::All,
            ..Default::default()
        };
        library
            .queue
            .replace(watch.queue.clone(), watch.current_index.unwrap());
        worker.attach_watch(&mut library.queue, &watch, None);
        assert!(library.queue.incomplete);
        assert!(!library.music_autoplay());
        assert!(library.queue.advance(library.repeat, true));
        let mut recommendations = VecDeque::from([track("recommendation")]);
        assert!(!library.queue.advance_with_autoplay(
            library.repeat,
            true,
            library.autoplay,
            &mut recommendations
        ));
        assert_eq!(library.queue.index, Some(1));
        worker.load(None, false).unwrap();
        worker
            .apply(completed.recv_blocking().unwrap(), &mut library.queue, None)
            .unwrap();
        assert_eq!(
            ids(&library.queue),
            ["abcdefghijk", "abcdefghijk", "lmnopqrstuv", "abcdefghijk"]
        );
        assert!(!library.queue.incomplete);
        assert!(!worker.can_continue());
        for index in [2, 3, 0] {
            assert!(library.queue.advance_with_autoplay(
                library.repeat,
                true,
                library.autoplay,
                &mut recommendations
            ));
            assert_eq!(library.queue.index, Some(index));
        }
        assert_eq!(recommendations.len(), 1);
    }

    #[test]
    fn issued_watch_queue_rejects_replacements_and_changed_account_sessions() {
        let session = session();
        let other = Arc::new((*session).clone());
        for replace in [false, true] {
            let (mut worker, completed) =
                Worker::spawn(|_| Ok(watch_page(&["lmnopqrstuv"], None, None)));
            let watch = watch(&["abcdefghijk"], Some("watch-tail"), Some(session.clone()));
            let mut queue = Queue::default();
            queue.replace(watch.queue.clone(), 0);
            worker.attach_watch(&mut queue, &watch, Some(session.clone()));
            worker.load(Some(&session), false).unwrap();
            let event = completed.recv_blocking().unwrap();
            if replace {
                queue.replace(vec![track("replacement")], 0);
                assert!(!worker.apply(event, &mut queue, Some(&session)).unwrap());
                assert_eq!(ids(&queue), ["replacement"]);
            } else {
                assert!(worker.apply(event, &mut queue, Some(&other)).is_err());
                assert_eq!(ids(&queue), ["abcdefghijk"]);
                assert!(queue.incomplete);
            }
            assert!(!worker.can_continue());
            assert!(!worker.load(Some(&session), true).unwrap());
        }
    }

    #[test]
    fn issued_watch_queue_rejects_a_repeated_token_before_appending_occurrences() {
        let (mut worker, completed) =
            Worker::spawn(|_| Ok(watch_page(&["lmnopqrstuv"], Some("same-page"), None)));
        let watch = watch(&["abcdefghijk"], Some("same-page"), None);
        let mut queue = Queue::default();
        queue.replace(watch.queue.clone(), 0);
        worker.attach_watch(&mut queue, &watch, None);
        worker.load(None, false).unwrap();
        assert!(
            worker
                .apply(completed.recv_blocking().unwrap(), &mut queue, None)
                .is_err()
        );
        assert_eq!(ids(&queue), ["abcdefghijk"]);
        assert!(queue.incomplete);
    }

    #[test]
    fn replacement_rejects_an_event_already_delivered_to_the_ui() {
        let (mut worker, completed) = Worker::spawn(|_| Ok(next_page(&["old-tail"], None)));
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
            Ok(next_page(&[token], None))
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
        let (mut worker, completed) = Worker::spawn(|_| Ok(next_page(&["private-tail"], None)));
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
            Ok(next_page(&["tail"], None))
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
            let mut responses =
                VecDeque::from([next_page(&[], Some("second")), next_page(&[], last_token)]);
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
    fn shuffle_fetches_all_continuations_and_keeps_played_tracks_fixed() {
        let (mut worker, completed) = Worker::spawn(|request| {
            Ok(match request.continuation.token() {
                "first" => next_page(&["d", "e"], Some("second")),
                "second" => next_page(&["f", "g"], None),
                _ => panic!("unexpected continuation"),
            })
        });
        let mut queue = Queue::default();
        worker.replace_queue(
            &mut queue,
            &page(&["a", "b", "c"], Some("first")),
            1,
            PageRequest::Browse,
            None,
        );
        worker.shuffle(&mut queue, None).unwrap();
        for _ in 0..2 {
            worker
                .apply(completed.recv_blocking().unwrap(), &mut queue, None)
                .unwrap();
        }
        assert!(!queue.incomplete);
        assert!(!worker.can_continue());
        assert_eq!(queue.index, Some(1));
        assert_eq!(&ids(&queue)[..2], ["a", "b"]);
        let mut all = ids(&queue);
        all.sort_unstable();
        assert_eq!(all, ["a", "b", "c", "d", "e", "f", "g"]);
        assert!(queue.shuffled);
    }

    #[test]
    fn queue_edits_keep_the_source_and_persistence_omits_request_context() {
        let (mut worker, completed) = Worker::spawn(|_| Ok(next_page(&["a", "tail"], None)));
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
        let mut keys = saved.as_object().unwrap().keys().collect::<Vec<_>>();
        keys.sort();
        assert_eq!(
            keys,
            ["incomplete", "index", "playing_from", "shuffled", "tracks"]
        );
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
        assert_eq!(queue.tracks[0], loaded[0]);
        let mut all = ids(&queue);
        all.sort_unstable();
        assert_eq!(all, ["a", "a", "b", "c", "play-next", "tail"]);
        assert_eq!(queue.current().unwrap().id, "a");
    }

    #[test]
    fn restart_verifies_source_occurrences_without_replacing_edited_or_shuffled_tracks() {
        for issued in [false, true] {
            for shuffled in [false, true] {
                for signed_in in [false, true] {
                    let original: Vec<String> = (0..177)
                        .map(|i| format!("{:011}", if i == 7 || i == 80 { 0 } else { i % 173 }))
                        .collect();
                    let initial: Vec<&str> = original[..100].iter().map(String::as_str).collect();
                    let (mut worker, _) = Worker::spawn(|_| unreachable!());
                    let mut queue = Queue::default();
                    let old_session = signed_in.then(session);
                    if issued {
                        let watch = watch(&initial, Some("secret-old-token"), old_session.clone());
                        queue.replace(watch.queue.clone(), 7);
                        worker.attach_watch(&mut queue, &watch, old_session);
                    } else {
                        worker.replace_queue(
                            &mut queue,
                            &page(&initial, Some("secret-old-token")),
                            7,
                            PageRequest::Collection("VLPLsynthetic".into()),
                            old_session,
                        );
                    }
                    queue.tracks.insert(8, track("manual-next"));
                    queue.remove(15);
                    queue.move_track(35, 9);
                    if shuffled {
                        queue.shuffle_upcoming();
                    }
                    let before = queue.tracks.clone();
                    let selected = queue.current().unwrap().clone();
                    let serialized = serde_json::to_string(&queue).unwrap();
                    for secret in [
                        "secret-old-token",
                        "cookie",
                        "visitor",
                        "authuser",
                        "data_sync_id",
                        "synthetic-user",
                    ] {
                        assert!(
                            !serialized.contains(secret),
                            "Saved queue contains {secret}"
                        );
                    }
                    drop(worker);
                    let mut queue: Queue = serde_json::from_str(&serialized).unwrap();
                    let renewed = signed_in.then(|| account("synthetic-user||", None));
                    let current = renewed.clone();
                    let source_tracks = original.clone();
                    let (mut worker, events) = Worker::spawn(move |request| {
                        assert!(same_session(request.session.as_ref(), current.as_ref()));
                        if let SourceContinuation::Restore(saved) = &request.continuation {
                            assert_eq!(saved.loaded, source_tracks[..100]);
                            // A fresh session may use different page boundaries.
                            let first: Vec<&str> =
                                source_tracks[..60].iter().map(String::as_str).collect();
                            let rest: Vec<&str> =
                                source_tracks[60..].iter().map(String::as_str).collect();
                            return resume_prefix(
                                saved,
                                next_page(&first, Some("fresh-token")),
                                |_| Ok(next_page(&rest, None)),
                            );
                        }
                        panic!("Restoration must reacquire the source, not replay an old token");
                    });
                    assert!(!worker.can_continue());
                    assert!(events.try_recv().is_err());
                    worker.restore(&mut queue, renewed.as_ref());
                    worker.load(renewed.as_ref(), false).unwrap();
                    worker
                        .apply(
                            events.recv_blocking().unwrap(),
                            &mut queue,
                            renewed.as_ref(),
                        )
                        .unwrap();
                    assert_eq!(queue.index, Some(7));
                    assert_eq!(queue.current(), Some(&selected));
                    assert_eq!(queue.shuffled, shuffled);
                    assert!(!queue.incomplete);
                    if !shuffled {
                        assert_eq!(&queue.tracks[..100], &before);
                        queue.select(99);
                        assert!(queue.advance(Repeat::Off, false));
                        assert_eq!(queue.current().unwrap().id, original[100]);
                    }
                    let mut expected: Vec<_> = before
                        .iter()
                        .map(|track| track.id.as_str())
                        .chain(original[100..].iter().map(String::as_str))
                        .collect();
                    let mut actual = ids(&queue);
                    expected.sort_unstable();
                    actual.sort_unstable();
                    assert_eq!(actual, expected);
                }
            }
        }
    }

    #[test]
    fn changed_collection_or_account_keeps_the_cached_queue_and_blocks_autoplay() {
        for changed in [true, false] {
            let saved = SavedSource {
                target: SavedTarget::Browse("VLPLsynthetic".into()),
                loaded: vec!["a".into(), "a".into(), "b".into()],
            };
            let mut queue = Queue::default();
            queue.replace(vec![track("a"), track("manual"), track("b")], 2);
            queue.incomplete = true;
            queue.source = Some(saved);
            let current = account("different-account||", None);
            let expected = current.clone();
            let (mut worker, events) = Worker::spawn(move |request| {
                assert!(same_session(request.session.as_ref(), Some(&expected)));
                let SourceContinuation::Restore(saved) = &request.continuation else {
                    panic!()
                };
                resume_prefix(
                    saved,
                    next_page(
                        if changed {
                            &["a", "different", "b"]
                        } else {
                            &["a"]
                        },
                        None,
                    ),
                    |_| unreachable!(),
                )
            });
            worker.restore(&mut queue, Some(&current));
            worker.load(Some(&current), false).unwrap();
            assert!(
                worker
                    .apply(events.recv_blocking().unwrap(), &mut queue, Some(&current))
                    .is_err()
            );
            assert_eq!(ids(&queue), ["a", "manual", "b"]);
            assert_eq!(queue.index, Some(2));
            assert!(queue.incomplete);
            assert!(!queue.advance_with_autoplay(
                Repeat::All,
                false,
                true,
                &mut VecDeque::from([track("radio")])
            ));
        }
    }

    #[test]
    fn restarted_mixes_keep_edits_and_append_unique_fresh_recommendations() {
        for endpoint in [
            json!({"videoId":"abcdefghijk", "playlistId":"RDAMVMabcdefghijk", "params":"wAEB"}),
            json!({"videoId":"abcdefghijk"}),
        ] {
            for shuffled in [false, true] {
                let mut watch = watch(
                    &["abcdefghijk", "removed0000", "lmnopqrstuv"],
                    Some("private-old-token"),
                    Some(session()),
                );
                watch.source_target = Some(WatchTarget::try_from(endpoint.clone()).unwrap());
                let mut queue = Queue::default();
                queue.replace(watch.queue.clone(), 0);
                let (mut worker, _) = Worker::spawn(|_| unreachable!());
                worker.attach_watch(&mut queue, &watch, Some(session()));
                queue.remove(1);
                queue.tracks.insert(1, track("manual-next"));
                queue.move_track(2, 1);
                if shuffled {
                    queue.shuffle_upcoming();
                }
                let before = queue.tracks.clone();
                let saved = serde_json::to_string(&queue).unwrap();
                for secret in [
                    "private-old-token",
                    "cookie",
                    "authuser",
                    "synthetic-user",
                    "data_sync_id",
                ] {
                    assert!(!saved.contains(secret));
                }
                drop(worker);
                let mut queue: Queue = serde_json::from_str(&saved).unwrap();
                queue.select(queue.tracks.len() - 1);
                assert!(
                    !queue.advance(Repeat::All, false),
                    "Repeat All must wait for more mix tracks"
                );
                // Recommendations can change completely and include removed tracks,
                // manual additions and duplicates, including on later pages.
                let fresh_session = account("synthetic-user||", None);
                let expected_session = fresh_session.clone();
                let (mut worker, events) = Worker::spawn(move |request| {
                    assert!(same_session(
                        request.session.as_ref(),
                        Some(&expected_session)
                    ));
                    match &request.continuation {
                        SourceContinuation::Restore(saved) => {
                            assert!(saved.target.is_generated());
                            Ok(next_page(
                                &[
                                    "lmnopqrstuv",
                                    "removed0000",
                                    "manual-next",
                                    "fresh-first",
                                    "fresh-first",
                                ],
                                Some("fresh-token"),
                            ))
                        }
                        _ => Ok(next_page(
                            &["fresh-first", "fresh-next"],
                            Some("next-token"),
                        )),
                    }
                });
                assert!(!worker.can_continue());
                worker.restore(&mut queue, Some(&fresh_session));
                for _ in 0..2 {
                    worker.load(Some(&fresh_session), false).unwrap();
                    worker
                        .apply(
                            events.recv_blocking().unwrap(),
                            &mut queue,
                            Some(&fresh_session),
                        )
                        .unwrap();
                    assert!(
                        matches!(worker.source.as_ref().unwrap().state, State::Ready),
                        "Shuffling an endless mix must not eagerly load every page"
                    );
                }
                assert_eq!(&queue.tracks[..before.len()], &before);
                let mut appended = ids(&queue)[before.len()..].to_vec();
                appended.sort_unstable();
                assert_eq!(appended, ["fresh-first", "fresh-next"]);
                assert!(queue.advance(Repeat::All, false));
                assert!(matches!(
                    queue.current().unwrap().id.as_str(),
                    "fresh-first" | "fresh-next"
                ));
                assert!(queue.incomplete);
                assert!(queue.source.is_some());
                let saved_again: Queue =
                    serde_json::from_str(&serde_json::to_string(&queue).unwrap()).unwrap();
                assert!(
                    saved_again
                        .source
                        .unwrap()
                        .loaded
                        .iter()
                        .any(|id| id == "fresh-next")
                );
            }
        }
    }

    #[test]
    #[ignore = "reacquires public radio and single-song mix queues under a fresh guest session"]
    fn live_public_mix_restore() {
        let catalog = Catalog::new().unwrap();
        let seed = catalog
            .search_filtered(
                "Khruangbin August 10",
                SearchFilter::Songs,
                SearchScope::Catalog,
            )
            .unwrap()
            .tracks()
            .into_iter()
            .find(|track| {
                track
                    .details
                    .watch
                    .as_ref()
                    .is_some_and(|target| target.is_single_video())
            })
            .expect("Search must provide a single-song destination");
        for mode in [
            crate::watch::WatchMode::Radio,
            crate::watch::WatchMode::Details,
        ] {
            let watch = catalog.watch(&seed, mode).unwrap();
            assert!(watch.continuation.is_some());
            let (mut worker, _) = Worker::start();
            let mut queue = Queue::default();
            queue.replace(watch.queue.clone(), 0);
            worker.attach_watch(&mut queue, &watch, None);
            let saved = serde_json::to_string(&queue).unwrap();
            drop(worker);
            let mut queue: Queue = serde_json::from_str(&saved).unwrap();
            let cached = queue.tracks.clone();
            let (mut worker, events) = Worker::start();
            queue.select(cached.len() - 1);
            assert!(!queue.advance(Repeat::All, false));
            worker.restore(&mut queue, None);
            for _ in 0..8 {
                worker.load(None, false).unwrap();
                worker
                    .apply(events.recv_blocking().unwrap(), &mut queue, None)
                    .unwrap();
                if queue.tracks.len() > cached.len() {
                    break;
                }
                assert!(
                    queue.incomplete,
                    "Public mix ended without fresh recommendations"
                );
            }
            assert_eq!(&queue.tracks[..cached.len()], &cached);
            let mut seen: HashSet<_> = cached.iter().map(|track| &track.id).collect();
            assert!(
                queue.tracks[cached.len()..]
                    .iter()
                    .all(|track| seen.insert(&track.id))
            );
            assert!(queue.advance(Repeat::All, false));
            assert_eq!(queue.index, Some(cached.len()));
            println!(
                "PUBLIC_MIX_RESTORE mode={mode:?} seed={} cached={} loaded={} next={}",
                seed.title,
                cached.len(),
                queue.tracks.len(),
                queue.current().unwrap().title
            );
        }
    }
}
