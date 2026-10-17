use crate::{
    auth::{self, Operation, Session, SharedSession},
    catalog::{Account, Catalog, Collection, Page, Rating, Track, find, parse_page},
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{sync::Arc, thread};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Songs,
    Albums,
    Artists,
    Subscriptions,
    Playlists,
    Liked,
    History,
    Podcasts,
    Episodes,
    Uploads,
    UploadAlbums,
    UploadArtists,
}
impl Section {
    pub fn title(self) -> &'static str {
        match self {
            Self::Songs => "Songs",
            Self::Albums => "Albums",
            Self::Artists => "Artists",
            Self::Subscriptions => "Subscriptions",
            Self::Playlists => "Playlists",
            Self::Liked => "Liked songs",
            Self::History => "History",
            Self::Podcasts => "Podcasts",
            Self::Episodes => "Episodes",
            Self::Uploads => "Uploaded songs",
            Self::UploadAlbums => "Uploaded albums",
            Self::UploadArtists => "Uploaded artists",
        }
    }
    fn browse_id(self) -> &'static str {
        match self {
            Self::Songs => "FEmusic_liked_videos",
            Self::Albums => "FEmusic_liked_albums",
            Self::Artists => "FEmusic_library_corpus_track_artists",
            Self::Subscriptions => "FEmusic_library_corpus_artists",
            Self::Playlists => "FEmusic_liked_playlists",
            Self::Liked => "VLLM",
            Self::History => "FEmusic_history",
            Self::Podcasts => "FEmusic_library_non_music_audio_channels_list",
            Self::Episodes => "FEmusic_library_non_music_audio_list",
            Self::Uploads => "FEmusic_library_privately_owned_tracks",
            Self::UploadAlbums => "FEmusic_library_privately_owned_releases",
            Self::UploadArtists => "FEmusic_library_privately_owned_artists",
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
pub enum Privacy {
    Private,
    Unlisted,
    Public,
}
impl Privacy {
    pub fn value(self) -> &'static str {
        match self {
            Self::Private => "PRIVATE",
            Self::Unlisted => "UNLISTED",
            Self::Public => "PUBLIC",
        }
    }
}

#[derive(Clone)]
pub enum Mutation {
    RateSong {
        track: Track,
        rating: Rating,
    },
    SongLibrary {
        track: Track,
        save: bool,
    },
    CollectionLibrary {
        collection: Collection,
        save: bool,
    },
    CreatePlaylist {
        title: String,
        description: String,
        privacy: Privacy,
    },
    EditPlaylist {
        id: String,
        title: String,
        description: String,
        privacy: Privacy,
    },
    DeletePlaylist {
        id: String,
    },
    AddToPlaylist {
        id: String,
        video_ids: Vec<String>,
        duplicates: bool,
    },
    RemoveFromPlaylist {
        id: String,
        video_id: String,
        set_video_id: String,
    },
    MoveInPlaylist {
        id: String,
        set_video_id: String,
        before: Option<String>,
    },
    RemoveHistory {
        token: String,
    },
}
impl Mutation {
    pub fn key(&self) -> String {
        match self {
            Self::RateSong { track, .. } | Self::SongLibrary { track, .. } => track.id.clone(),
            Self::CollectionLibrary { collection, .. } => collection.id.clone(),
            Self::CreatePlaylist { .. } => "create-playlist".into(),
            Self::EditPlaylist { id, .. }
            | Self::DeletePlaylist { id }
            | Self::AddToPlaylist { id, .. }
            | Self::RemoveFromPlaylist { id, .. }
            | Self::MoveInPlaylist { id, .. } => id.clone(),
            Self::RemoveHistory { token } => token.clone(),
        }
    }
}

pub enum Effect {
    Changed,
    CreatedPlaylist(String),
}
#[derive(Clone)]
pub struct Connection {
    pub account: Account,
    pub session: Arc<Session>,
}

impl Connection {
    fn verify(session: Session, catalog: &Catalog) -> Result<Self> {
        session.validate_playback_identity()?;
        let session = Arc::new(session);
        let account = catalog.for_session(Some(session.clone())).account()?;
        Ok(Self { account, session })
    }

    fn publish(self, shared: &SharedSession, operation: &Operation) -> Result<Self> {
        let mut current = shared
            .write()
            .map_err(|_| anyhow::anyhow!("Account connection is unavailable"))?;
        ensure!(!operation.is_cancelled(), "Account connection cancelled");
        *current = Some(self.session.clone());
        Ok(self)
    }
}

pub enum Command {
    Connect(Operation),
    Apply {
        session: Arc<Session>,
        mutation: Box<Mutation>,
    },
    Restore(Operation),
    Disconnect(Operation),
    Persist(Arc<Session>),
    ReportPlayback {
        session: Arc<Session>,
        video_id: String,
    },
}
pub enum Event {
    Connected {
        operation: Operation,
        result: Result<Option<Connection>, String>,
        save_session: bool,
    },
    Disconnected {
        operation: Operation,
        result: Result<(), String>,
    },
    Applied {
        session: Arc<Session>,
        mutation: Box<Mutation>,
        result: Result<Effect, String>,
    },
    Persisted {
        session: Arc<Session>,
        result: Result<(), String>,
    },
    PlaybackReported {
        session: Arc<Session>,
        video_id: String,
        result: Result<(), String>,
    },
}

pub struct Worker {
    commands: async_channel::Sender<Command>,
    thread: thread::JoinHandle<Result<()>>,
}

impl Worker {
    /// Finish pending sign-out work before the process can exit.
    pub fn finish(self) -> Result<()> {
        self.commands.close();
        self.thread
            .join()
            .map_err(|_| anyhow::anyhow!("The account cleanup worker stopped unexpectedly"))?
    }
}

pub fn start(
    session: SharedSession,
) -> (
    async_channel::Sender<Command>,
    async_channel::Receiver<Event>,
    Worker,
) {
    let (requests, commands) = async_channel::bounded(64);
    let (events, completed) = async_channel::unbounded();
    let worker = thread::Builder::new()
        .name("account-sync".into())
        .spawn(move || -> Result<()> {
            let catalog = Catalog::new();
            let mut cleanup_error = None;
            while let Ok(command) = commands.recv_blocking() {
                let event = match command {
                    Command::Connect(operation) => {
                        let result = (|| {
                            let (candidate, account) = auth::connect(&operation)?;
                            Connection { session: Arc::new(candidate), account }
                                .publish(&session, &operation).map(Some)
                        })().map_err(|e| format!("{e:#}"));
                        Event::Connected { operation, result, save_session: true }
                    }
                    Command::Restore(operation) => {
                        let result = (|| {
                            ensure!(auth::ACCOUNT_LOGIN_ENABLED, "Account connection is disabled until the security review is complete.");
                            ensure!(!operation.is_cancelled(), "Account connection cancelled");
                            let candidate = Session::load()?;
                            candidate.map(|candidate| {
                                let catalog = catalog.as_ref().map_err(|e| anyhow::anyhow!("{e:#}"))?;
                                Connection::verify(candidate, catalog)?.publish(&session, &operation)
                            }).transpose()
                        })();
                        Event::Connected { operation, result: result.map_err(|error| format!("{error:#}")), save_session: false }
                    }
                    Command::Disconnect(operation) => {
                        // End in-memory authority even if keyring removal needs another attempt.
                        if let Ok(mut current) = session.write() { *current = None; }
                        let result = auth::Session::disconnect().map_err(|e| format!("{e:#}"));
                        cleanup_error = result.as_ref().err().cloned();
                        Event::Disconnected { operation, result }
                    }
                    Command::Apply { session: expected, mutation } => {
                        let result = (|| {
                            require_current_session(&session, &expected)?;
                            catalog.as_ref().map_err(|e| anyhow::anyhow!("{e:#}"))?
                                .for_session(Some(expected.clone())).mutate(&mutation)
                        })().map_err(|e| format!("{e:#}"));
                        Event::Applied { session: expected, mutation, result }
                    }
                    Command::Persist(expected) => {
                        let result = (|| {
                            ensure!(auth::ACCOUNT_LOGIN_ENABLED, "Account connection is disabled until the security review is complete.");
                            persist_current_session(&session, &expected, || expected.save(), Session::disconnect)
                        })().map_err(|error| format!("{error:#}"));
                        Event::Persisted { session: expected, result }
                    }
                    Command::ReportPlayback { session: expected, video_id } => {
                        let result = (|| {
                            catalog.as_ref().map_err(|error| anyhow::anyhow!("{error:#}"))?
                                .for_session(Some(expected.clone())).report_playback(&video_id, &session)
                        })().map_err(|error| format!("{error:#}"));
                        Event::PlaybackReported { session: expected, video_id, result }
                    }
                };
                // Closing the UI must not discard a queued sign-out behind this event.
                let _ = events.send_blocking(event);
            }
            if let Some(error) = cleanup_error {
                anyhow::bail!(error);
            }
            Ok(())
        })
        .expect("start account sync worker");
    let worker = Worker {
        commands: requests.clone(),
        thread: worker,
    };
    (requests, completed, worker)
}

fn persist_current_session(
    shared: &SharedSession,
    expected: &Arc<Session>,
    save: impl FnOnce() -> Result<()>,
    clear: impl FnOnce() -> Result<()>,
) -> Result<()> {
    require_current_session(shared, expected)?;
    let saved = save();
    if let Err(revoked) = require_current_session(shared, expected) {
        clear().context("Could not remove a revoked account from the keyring")?;
        return Err(revoked);
    }
    saved
}

pub(crate) fn require_current_session(
    shared: &SharedSession,
    expected: &Arc<Session>,
) -> Result<()> {
    let current = shared
        .read()
        .map_err(|_| anyhow::anyhow!("Account connection is unavailable"))?;
    ensure!(
        current
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, expected)),
        "The connected account changed. Refresh and try again."
    );
    Ok(())
}

impl Catalog {
    pub fn library(&self, section: Section) -> Result<Page> {
        self.require_account()?;
        let data = self.request("browse", json!({"browseId":section.browse_id()}))?;
        let mut page = parse_page(&data, section.title())?;
        if section == Section::Liked {
            for shelf in &mut page.shelves {
                for item in &mut shelf.items {
                    if let crate::catalog::Item::Track(track) = item {
                        track.details.rating = Some(Rating::Like);
                    }
                }
            }
        }
        Ok(page)
    }
    fn require_account(&self) -> Result<()> {
        ensure!(self.session.is_some(), "Connect your Google account first");
        Ok(())
    }
    pub fn mutate(&self, mutation: &Mutation) -> Result<Effect> {
        self.require_account()?;
        let (endpoint, body) = match mutation {
            Mutation::RateSong { track, rating } => {
                (rating.endpoint(), json!({"target":{"videoId":track.id}}))
            }
            Mutation::SongLibrary { track, save } => {
                let token = if *save {
                    &track.details.add_library
                } else {
                    &track.details.remove_library
                };
                let token = token
                    .as_ref()
                    .context("Refresh this song before changing its library status")?;
                ("feedback", json!({"feedbackTokens":[token]}))
            }
            Mutation::CollectionLibrary { collection, save } => {
                if collection.id.starts_with("UC") {
                    (
                        if *save {
                            "subscription/subscribe"
                        } else {
                            "subscription/unsubscribe"
                        },
                        json!({"channelIds":[collection.id]}),
                    )
                } else {
                    let canonical = collection.id.strip_prefix("VL").map(str::to_owned);
                    let playlist = match canonical
                        .as_ref()
                        .or(collection.details.playlist_id.as_ref())
                    {
                        Some(id) => id.clone(),
                        None if collection.id.starts_with("VL") => {
                            collection.id.trim_start_matches("VL").to_owned()
                        }
                        None => {
                            let data = self.request("browse", json!({"browseId":collection.id}))?;
                            find(&data, "musicResponsiveHeaderRenderer")
                                .or_else(|| find(&data, "musicDetailHeaderRenderer"))
                                .and_then(|header| {
                                    find(&header["buttons"], "musicPlayButtonRenderer")
                                })
                                .and_then(|button| {
                                    button
                                        .pointer("/playNavigationEndpoint/watchEndpoint/playlistId")
                                })
                                .and_then(Value::as_str)
                                .context("YouTube did not provide this collection's library ID")?
                                .to_owned()
                        }
                    };
                    (
                        if *save {
                            "like/like"
                        } else {
                            "like/removelike"
                        },
                        json!({"target":{"playlistId":playlist}}),
                    )
                }
            }
            Mutation::CreatePlaylist {
                title,
                description,
                privacy,
            } => {
                ensure!(!title.trim().is_empty(), "Enter a playlist name");
                (
                    "playlist/create",
                    json!({"title":title.trim(),"description":description,"privacyStatus":privacy.value()}),
                )
            }
            Mutation::EditPlaylist {
                id,
                title,
                description,
                privacy,
            } => {
                ensure!(!title.trim().is_empty(), "Enter a playlist name");
                (
                    "browse/edit_playlist",
                    json!({"playlistId":playlist_id(id),"actions":[{"action":"ACTION_SET_PLAYLIST_NAME","playlistName":title.trim()},{"action":"ACTION_SET_PLAYLIST_DESCRIPTION","playlistDescription":description},{"action":"ACTION_SET_PLAYLIST_PRIVACY","playlistPrivacy":privacy.value()}]}),
                )
            }
            Mutation::DeletePlaylist { id } => {
                ("playlist/delete", json!({"playlistId":playlist_id(id)}))
            }
            Mutation::AddToPlaylist {
                id,
                video_ids,
                duplicates,
            } => {
                let actions: Vec<_> = video_ids
                    .iter()
                    .map(|id| {
                        let mut action = json!({"action":"ACTION_ADD_VIDEO","addedVideoId":id});
                        if *duplicates {
                            action["dedupeOption"] = json!("DEDUPE_OPTION_SKIP");
                        }
                        action
                    })
                    .collect();
                (
                    "browse/edit_playlist",
                    json!({"playlistId":playlist_id(id),"actions":actions}),
                )
            }
            Mutation::RemoveFromPlaylist {
                id,
                video_id,
                set_video_id,
            } => (
                "browse/edit_playlist",
                json!({"playlistId":playlist_id(id),"actions":[{"action":"ACTION_REMOVE_VIDEO","removedVideoId":video_id,"setVideoId":set_video_id}]}),
            ),
            Mutation::MoveInPlaylist {
                id,
                set_video_id,
                before,
            } => {
                let mut action =
                    json!({"action":"ACTION_MOVE_VIDEO_BEFORE","setVideoId":set_video_id});
                if let Some(before) = before {
                    action["movedSetVideoIdSuccessor"] = json!(before);
                }
                (
                    "browse/edit_playlist",
                    json!({"playlistId":playlist_id(id),"actions":[action]}),
                )
            }
            Mutation::RemoveHistory { token } => ("feedback", json!({"feedbackTokens":[token]})),
        };
        let response = self.request(endpoint, body)?;
        confirm_mutation(endpoint, &response)?;
        if matches!(mutation, Mutation::CreatePlaylist { .. }) {
            return Ok(Effect::CreatedPlaylist(
                response["playlistId"]
                    .as_str()
                    .context("YouTube did not return the new playlist")?
                    .to_owned(),
            ));
        }
        Ok(Effect::Changed)
    }
}
fn confirm_mutation(endpoint: &str, response: &Value) -> Result<()> {
    if endpoint == "feedback" {
        let feedback = response["feedbackResponses"]
            .as_array()
            .context("YouTube did not confirm the library change. Refresh and try again.")?;
        ensure!(
            !feedback.is_empty() && feedback.iter().all(|v| v["isProcessed"] == true),
            "YouTube rejected the library change. Refresh and try again."
        );
    }
    if matches!(endpoint, "browse/edit_playlist" | "playlist/delete") {
        ensure!(
            response["status"] == "STATUS_SUCCEEDED",
            "YouTube did not confirm the playlist change"
        );
    }
    if let Some(status) = response["status"].as_str() {
        ensure!(
            status == "STATUS_SUCCEEDED",
            "YouTube did not save the change ({status})"
        );
    }
    Ok(())
}

fn playlist_id(id: &str) -> &str {
    id.strip_prefix("VL").unwrap_or(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn synthetic_session() -> Session {
        serde_json::from_value(json!({
            "cookie":"SAPISID=synthetic-test-value",
            "playback_cookie":"# Netscape HTTP Cookie File\n",
            "authuser":"0", "delegated_session_id":null, "client_version":"test",
            "data_sync_id":"synthetic-user||"
        }))
        .unwrap()
    }
    #[test]
    fn queued_writes_require_the_exact_verified_session() {
        let shared = auth::shared(Some(synthetic_session()));
        let expected = shared.read().unwrap().clone().unwrap();
        assert!(require_current_session(&shared, &expected).is_ok());
        // Even equal cookie bytes restored later represent a different connection.
        *shared.write().unwrap() = Some(Arc::new(synthetic_session()));
        assert!(require_current_session(&shared, &expected).is_err());
        *shared.write().unwrap() = None;
        assert!(require_current_session(&shared, &expected).is_err());
    }
    #[test]
    fn rejected_candidate_preserves_the_previous_verified_session() {
        let shared = auth::shared(Some(synthetic_session()));
        let expected = shared.read().unwrap().clone().unwrap();
        let catalog = Catalog::new().unwrap();
        let mut candidate = synthetic_session();
        // An invalid request header fails before any network connection is made.
        candidate.client_version = "invalid\nclient".into();
        assert!(Connection::verify(candidate, &catalog).is_err());
        assert!(require_current_session(&shared, &expected).is_ok());
    }
    #[test]
    fn cancelled_connection_cannot_publish_after_sign_out() {
        let shared = auth::shared(Some(synthetic_session()));
        let operation = Operation::default();
        let completion = operation.clone();
        let old_session = shared.read().unwrap().clone().unwrap();
        let candidate = Connection {
            session: Arc::new(synthetic_session()),
            account: Account {
                name: "Synthetic account".into(),
                handle: String::new(),
            },
        };
        operation.cancel();
        *shared.write().unwrap() = None;
        assert!(candidate.publish(&shared, &completion).is_err());
        assert!(shared.read().unwrap().is_none());
        assert!(!operation.matches(&completion));
        assert!(require_current_session(&shared, &old_session).is_err());
        let fresh = Operation::default();
        assert!(!fresh.matches(&completion));
        assert!(fresh.matches(&fresh.clone()));
    }
    #[test]
    fn revoked_persistence_is_cleared_even_after_a_partial_store_failure() {
        use std::cell::Cell;
        for store_succeeds in [true, false] {
            let shared = auth::shared(Some(synthetic_session()));
            let expected = shared.read().unwrap().clone().unwrap();
            let cleared = Cell::new(false);
            let result = persist_current_session(
                &shared,
                &expected,
                || {
                    *shared.write().unwrap() = None;
                    ensure!(store_succeeds, "Synthetic partial store failure");
                    Ok(())
                },
                || {
                    cleared.set(true);
                    Ok(())
                },
            );
            assert!(result.is_err());
            assert!(cleared.get());
        }
    }
    #[test]
    fn finishing_worker_drains_commands_after_the_event_receiver_closes() {
        let shared = auth::shared(Some(synthetic_session()));
        let expected = shared.read().unwrap().clone().unwrap();
        let (commands, events, worker) = start(shared.clone());
        drop(events);
        let operation = Operation::default();
        operation.cancel();
        for _ in 0..2 {
            commands
                .send_blocking(Command::Restore(operation.clone()))
                .unwrap();
        }
        worker.finish().unwrap();
        assert!(commands.is_closed());
        assert!(commands.is_empty());
        assert!(require_current_session(&shared, &expected).is_ok());
    }
    #[test]
    fn failed_or_missing_acknowledgements_are_not_success() {
        for response in [
            json!({}),
            json!({"feedbackResponses":[]}),
            json!({"feedbackResponses":[{"isProcessed":false}]}),
        ] {
            assert!(confirm_mutation("feedback", &response).is_err());
        }
        assert!(
            confirm_mutation(
                "feedback",
                &json!({"feedbackResponses":[{"isProcessed":true}]})
            )
            .is_ok()
        );
        assert!(confirm_mutation("browse/edit_playlist", &json!({})).is_err());
        assert!(
            confirm_mutation(
                "browse/edit_playlist",
                &json!({"status":"STATUS_SUCCEEDED"})
            )
            .is_ok()
        );
    }
}
