use crate::{
    auth::{self, Operation, Session, SharedSession},
    catalog::{
        Account, ArtistSubscription, Catalog, Collection, Page, Rating, Track, find, parse_page,
    },
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
            Self::Podcasts => "FEmusic_library_non_music_audio_list",
            Self::Episodes => "VLSE",
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
        initial_video_id: Option<String>,
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
    CreatedPlaylist { id: String },
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

    pub fn activate(
        self,
        shared: &SharedSession,
        operation: &Operation,
        playback: Result<(), String>,
    ) -> Result<Self> {
        playback.map_err(anyhow::Error::msg)?;
        let mut current = shared
            .write()
            .map_err(|_| anyhow::anyhow!("Account connection is unavailable"))?;
        ensure!(!operation.is_cancelled(), "Account connection cancelled");
        *current = Some(self.session.clone());
        Ok(self)
    }
}

fn restore_connection(
    candidate: Session,
    operation: &Operation,
    verify: impl Fn(Session) -> Result<Connection>,
    read_profile: impl FnOnce(&auth::BrowserProfile) -> Result<Session>,
) -> Result<(Connection, bool)> {
    ensure!(!operation.is_cancelled(), "Account connection cancelled");
    let rejected = match verify(candidate.clone()) {
        Ok(connection) => {
            ensure!(!operation.is_cancelled(), "Account connection cancelled");
            return Ok((connection, false));
        }
        Err(error) if auth::session_was_rejected(&error) => error,
        Err(error) => return Err(error),
    };
    ensure!(!operation.is_cancelled(), "Account connection cancelled");
    let source = candidate.source_profile.as_ref().ok_or_else(|| {
        rejected.context(
            "The saved session expired. Reconnect once to enable automatic refresh from your browser profile.",
        )
    })?;
    let refreshed = read_profile(source)
        .context("Could not refresh the saved account. Your saved connection has been kept.")?;
    ensure!(!operation.is_cancelled(), "Account connection cancelled");
    candidate.require_same_account(&refreshed)?;
    let connection = verify(refreshed)?;
    ensure!(!operation.is_cancelled(), "Account connection cancelled");
    Ok((connection, true))
}

pub enum Command {
    Connect(Operation),
    ReadBrowser {
        operation: Operation,
        profile: auth::BrowserProfile,
    },
    Apply {
        session: Arc<Session>,
        mutation: Box<Mutation>,
    },
    Restore(Operation),
    Disconnect {
        operation: Operation,
        playback: Result<Option<async_channel::Receiver<Result<(), String>>>, String>,
    },
    Persist(Arc<Session>),
    ReportPlayback {
        session: Arc<Session>,
        video_id: String,
    },
}
pub enum Event {
    BrowserReady {
        operation: Operation,
        result: Result<auth::BrowserProfile, String>,
    },
    AccountPreview {
        operation: Operation,
        result: Result<Connection, String>,
    },
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
    /// Finish accepted account changes and cleanup before the process can exit.
    pub fn finish(self) -> Result<()> {
        self.commands.close();
        self.thread
            .join()
            .map_err(|_| anyhow::anyhow!("The account worker stopped unexpectedly"))?
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
                        let result = auth::BrowserProfile::open(&operation).map_err(|error| format!("{error:#}"));
                        Event::BrowserReady { operation, result }
                    }
                    Command::ReadBrowser { operation, profile } => {
                        let result = (|| {
                            let candidate = profile.read_session(&operation)?;
                            let catalog = catalog.as_ref().map_err(|error| anyhow::anyhow!("{error:#}"))?;
                            let connection = Connection::verify(candidate, catalog)?;
                            ensure!(!operation.is_cancelled(), "Account connection cancelled");
                            Ok(connection)
                        })().map_err(|error: anyhow::Error| format!("{error:#}"));
                        Event::AccountPreview { operation, result }
                    }
                    Command::Restore(operation) => {
                        let mut refreshed = false;
                        let result = (|| {
                            ensure!(auth::ACCOUNT_LOGIN_ENABLED, "Account connection is disabled until the security review is complete.");
                            ensure!(!operation.is_cancelled(), "Account connection cancelled");
                            let candidate = Session::load(&operation)?;
                            ensure!(!operation.is_cancelled(), "Account connection cancelled");
                            candidate.map(|candidate| {
                                let catalog = catalog.as_ref().map_err(|e| anyhow::anyhow!("{e:#}"))?;
                                let (connection, renewed) = restore_connection(
                                    candidate,
                                    &operation,
                                    |candidate| Connection::verify(candidate, catalog),
                                    |profile| profile.read_session(&operation),
                                )?;
                                refreshed = renewed;
                                Ok(connection)
                            }).transpose()
                        })();
                        Event::Connected { operation, result: result.map_err(|error| format!("{error:#}")), save_session: refreshed }
                    }
                    Command::Disconnect { operation, playback } => {
                        // End in-memory authority even if keyring removal needs another attempt.
                        if let Ok(mut current) = session.write() { *current = None; }
                        let result = finish_disconnect(playback, auth::Session::disconnect);
                        cleanup_error = result.as_ref().err().cloned();
                        Event::Disconnected { operation, result }
                    }
                    Command::Apply { session: expected, mutation } => {
                        let result = (|| {
                            catalog.as_ref().map_err(|e| anyhow::anyhow!("{e:#}"))?
                                .for_session(Some(expected.clone())).mutate(&mutation, &session)
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

fn finish_disconnect(
    playback: Result<Option<async_channel::Receiver<Result<(), String>>>, String>,
    remove_saved: impl FnOnce() -> Result<()>,
) -> Result<(), String> {
    let saved = remove_saved().map_err(|error| format!("{error:#}"));
    let playback = playback.and_then(|response| match response {
        Some(response) => response.recv_blocking().unwrap_or_else(|_| {
            Err("The player stopped before confirming credential cleanup. Restart the app.".into())
        }),
        None => Ok(()),
    });
    match (saved, playback) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(saved), Err(playback)) => Err(format!("{saved}. {playback}")),
        (Err(error), _) | (_, Err(error)) => Err(error),
    }
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
        let mut page = self.bind_page(parse_page(&data, section.title())?, &data, None)?;
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
    pub fn mutate(&self, mutation: &Mutation, current: &SharedSession) -> Result<Effect> {
        let expected = self
            .session
            .as_ref()
            .context("Connect your Google account first")?;
        let response = send_current_mutation(
            current,
            expected,
            || self.mutation_request(mutation),
            |endpoint, body| self.request(endpoint, body),
        )?;
        let effect = mutation_effect(mutation, &response)?;
        let playlist = match (&effect, mutation) {
            (Effect::CreatedPlaylist { id }, _) => Some(id),
            (
                _,
                Mutation::EditPlaylist { id, .. }
                | Mutation::AddToPlaylist { id, .. }
                | Mutation::RemoveFromPlaylist { id, .. }
                | Mutation::MoveInPlaylist { id, .. },
            ) => Some(id),
            _ => None,
        };
        if let Some(id) = playlist {
            self.wait_for_playlist(id, current, expected, |page| {
                playlist_change_visible(mutation, &response, page)
            })?;
        }
        Ok(effect)
    }

    fn wait_for_playlist(
        &self,
        id: &str,
        current: &SharedSession,
        expected: &Arc<Session>,
        visible: impl Fn(&Page) -> bool,
    ) -> Result<()> {
        // Playlist reads can lag an acknowledged write. Retry reads, never the write.
        for attempt in 0..5 {
            require_current_session(current, expected)?;
            let response = self.request("browse", json!({"browseId":format!("VL{}",playlist_id(id))}))
                .context("YouTube accepted the change, but refreshing the playlist failed. Refresh the playlist.")?;
            require_current_session(current, expected)?;
            if response.get("contents").is_some() && visible(&parse_page(&response, "Playlist")?) {
                return Ok(());
            }
            if attempt < 4 {
                thread::sleep(std::time::Duration::from_millis(500));
            }
        }
        anyhow::bail!(
            "YouTube accepted the change, but the updated playlist is not available yet. Refresh the playlist."
        )
    }

    fn mutation_request(&self, mutation: &Mutation) -> Result<(&'static str, Value)> {
        Ok(match mutation {
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
                    // An artist browse ID can differ from its subscribable channel.
                    let data = self.request("browse", json!({"browseId":collection.id}))?;
                    let subscription = ArtistSubscription::from_response(&data)?;
                    (
                        if *save {
                            "subscription/subscribe"
                        } else {
                            "subscription/unsubscribe"
                        },
                        json!({"channelIds":[subscription.channel_id]}),
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
                initial_video_id,
            } => {
                ensure!(!title.trim().is_empty(), "Enter a playlist name");
                let mut body = json!({"title":title.trim(),"description":description,"privacyStatus":privacy.value()});
                if let Some(video_id) = initial_video_id {
                    body["videoIds"] = json!([video_id]);
                }
                ("playlist/create", body)
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
        })
    }
}

fn playlist_change_visible(mutation: &Mutation, response: &Value, page: &Page) -> bool {
    let tracks = page.tracks();
    match mutation {
        Mutation::CreatePlaylist {
            title,
            description,
            privacy,
            initial_video_id,
        } => {
            page.title == title.trim()
                && page.description == *description
                && page.privacy.as_deref() == Some(privacy.value())
                && initial_video_id
                    .as_ref()
                    .is_none_or(|id| tracks.iter().any(|track| track.id == *id))
        }
        Mutation::EditPlaylist {
            title,
            description,
            privacy,
            ..
        } => {
            page.title == title.trim()
                && page.description == *description
                && page.privacy.as_deref() == Some(privacy.value())
        }
        Mutation::AddToPlaylist { .. } => {
            let added: Vec<_> = response["playlistEditResults"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|result| {
                    result
                        .pointer("/playlistEditVideoAddedResultData/setVideoId")
                        .and_then(Value::as_str)
                })
                .collect();
            !added.is_empty()
                && (page.continuation.is_some()
                    || added.iter().all(|id| {
                        tracks
                            .iter()
                            .any(|track| track.details.set_video_id.as_deref() == Some(id))
                    }))
        }
        Mutation::RemoveFromPlaylist { set_video_id, .. } => !tracks
            .iter()
            .any(|track| track.details.set_video_id.as_ref() == Some(set_video_id)),
        Mutation::MoveInPlaylist {
            set_video_id,
            before,
            ..
        } => {
            let position = |id: &str| {
                tracks
                    .iter()
                    .position(|track| track.details.set_video_id.as_deref() == Some(id))
            };
            let moved = position(set_video_id);
            match before {
                Some(before) => match (moved, position(before)) {
                    (Some(moved), Some(successor)) => moved + 1 == successor,
                    (None, None) => page.continuation.is_some(),
                    _ => false,
                },
                None => match moved {
                    Some(index) => page.continuation.is_none() && index + 1 == tracks.len(),
                    None => page.continuation.is_some(),
                },
            }
        }
        _ => true,
    }
}

fn mutation_effect(mutation: &Mutation, response: &Value) -> Result<Effect> {
    if let Mutation::CreatePlaylist { .. } = mutation {
        return Ok(Effect::CreatedPlaylist {
            id: response["playlistId"]
                .as_str()
                .context("YouTube did not return the new playlist")?
                .to_owned(),
        });
    }
    Ok(Effect::Changed)
}
fn confirm_mutation(endpoint: &str, response: &Value, playlist_id: Option<&str>) -> Result<()> {
    if endpoint == "feedback" {
        let feedback = response["feedbackResponses"]
            .as_array()
            .context("YouTube did not confirm the library change. Refresh and try again.")?;
        ensure!(
            !feedback.is_empty() && feedback.iter().all(|v| v["isProcessed"] == true),
            "YouTube rejected the library change. Refresh and try again."
        );
    }
    if endpoint == "browse/edit_playlist" {
        ensure!(
            response["status"] == "STATUS_SUCCEEDED",
            "YouTube did not confirm the playlist change"
        );
    }
    if endpoint == "playlist/delete" {
        let deleted = response
            .pointer("/command/commandExecutorCommand/commands")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|command| command.pointer("/handlePlaylistDeletionCommand/playlistId"))
            .any(|id| id.as_str().is_some_and(|id| Some(id) == playlist_id));
        ensure!(
            response["status"] == "STATUS_SUCCEEDED" || deleted,
            "YouTube did not confirm the playlist deletion"
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

fn send_current_mutation(
    current: &SharedSession,
    expected: &Arc<Session>,
    prepare: impl FnOnce() -> Result<(&'static str, Value)>,
    send: impl FnOnce(&str, Value) -> Result<Value>,
) -> Result<Value> {
    require_current_session(current, expected)?;
    let (endpoint, body) = prepare()?;
    // Resolving an artist or album can outlive the connected session.
    require_current_session(current, expected)?;
    let playlist_id = body
        .get("playlistId")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let response = send(endpoint, body)?;
    confirm_mutation(endpoint, &response, playlist_id.as_deref())?;
    Ok(response)
}

fn playlist_id(id: &str) -> &str {
    id.strip_prefix("VL").unwrap_or(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn playlist_page(occurrences: &[&str]) -> Page {
        Page {
            header: None,
            title: "Renamed".into(),
            description: "Updated description".into(),
            privacy: Some("PRIVATE".into()),
            shelves: vec![crate::catalog::Shelf {
                layout: crate::catalog::ShelfLayout::Songs,
                title: "Songs".into(),
                more: None,
                items: occurrences
                    .iter()
                    .map(|id| {
                        let mut track: Track = serde_json::from_value(json!({
                            "id":"abcdefghijk", "title":"Repeated song", "artist":"Artist"
                        }))
                        .unwrap();
                        track.details.set_video_id = Some((*id).into());
                        crate::catalog::Item::Track(track)
                    })
                    .collect(),
            }],
            continuation: None,
            editable: true,
            chips: Vec::new(),
            countries: Vec::new(),
        }
    }

    #[test]
    fn playlist_metadata_refresh_waits_for_the_acknowledged_values() {
        let ready = playlist_page(&["first"]);
        for mutation in [
            Mutation::CreatePlaylist {
                title: " Renamed ".into(),
                description: "Updated description".into(),
                privacy: Privacy::Private,
                initial_video_id: Some("abcdefghijk".into()),
            },
            Mutation::EditPlaylist {
                id: "PLedited".into(),
                title: " Renamed ".into(),
                description: "Updated description".into(),
                privacy: Privacy::Private,
            },
        ] {
            assert!(playlist_change_visible(&mutation, &json!({}), &ready));
            for stale in [
                Page {
                    title: "Old name".into(),
                    ..ready.clone()
                },
                Page {
                    description: "Old description".into(),
                    ..ready.clone()
                },
                Page {
                    privacy: Some("PUBLIC".into()),
                    ..ready.clone()
                },
            ] {
                assert!(!playlist_change_visible(&mutation, &json!({}), &stale));
            }
        }
    }

    #[test]
    fn playlist_refresh_matches_added_removed_and_moved_occurrences() {
        let added = Mutation::AddToPlaylist {
            id: "PLedited".into(),
            video_ids: vec!["abcdefghijk".into()],
            duplicates: true,
        };
        let response = json!({"playlistEditResults":[{"playlistEditVideoAddedResultData":{"videoId":"abcdefghijk","setVideoId":"new"}}]});
        assert!(!playlist_change_visible(
            &added,
            &response,
            &playlist_page(&["original"])
        ));
        assert!(playlist_change_visible(
            &added,
            &response,
            &playlist_page(&["original", "new"])
        ));
        let removed = Mutation::RemoveFromPlaylist {
            id: "PLedited".into(),
            video_id: "abcdefghijk".into(),
            set_video_id: "new".into(),
        };
        assert!(!playlist_change_visible(
            &removed,
            &json!({}),
            &playlist_page(&["original", "new"])
        ));
        assert!(playlist_change_visible(
            &removed,
            &json!({}),
            &playlist_page(&["original"])
        ));
        let moved = Mutation::MoveInPlaylist {
            id: "PLedited".into(),
            set_video_id: "third".into(),
            before: Some("second".into()),
        };
        assert!(!playlist_change_visible(
            &moved,
            &json!({}),
            &playlist_page(&["first", "second", "third"])
        ));
        assert!(playlist_change_visible(
            &moved,
            &json!({}),
            &playlist_page(&["first", "third", "second"])
        ));
        let to_end = Mutation::MoveInPlaylist {
            id: "PLedited".into(),
            set_video_id: "first".into(),
            before: None,
        };
        assert!(!playlist_change_visible(
            &to_end,
            &json!({}),
            &playlist_page(&["first", "second", "third"])
        ));
        assert!(playlist_change_visible(
            &to_end,
            &json!({}),
            &playlist_page(&["second", "third", "first"])
        ));
    }

    #[test]
    fn playlist_refresh_does_not_scan_unaffected_pages() {
        let mut partial = playlist_page(&["visible"]);
        partial.continuation = parse_page(
            &json!({"contents":[],"nextContinuationData":{"continuation":"next"}}),
            "Playlist",
        )
        .unwrap()
        .continuation;
        let added = Mutation::AddToPlaylist {
            id: "PLedited".into(),
            video_ids: vec!["abcdefghijk".into()],
            duplicates: true,
        };
        let response = json!({"playlistEditResults":[{"playlistEditVideoAddedResultData":{"setVideoId":"appended"}}]});
        assert!(playlist_change_visible(&added, &response, &partial));
        let moved = Mutation::MoveInPlaylist {
            id: "PLedited".into(),
            set_video_id: "later".into(),
            before: Some("also-later".into()),
        };
        assert!(playlist_change_visible(&moved, &json!({}), &partial));
        let before_visible = Mutation::MoveInPlaylist {
            id: "PLedited".into(),
            set_video_id: "later".into(),
            before: Some("visible".into()),
        };
        assert!(!playlist_change_visible(
            &before_visible,
            &json!({}),
            &partial
        ));
        let to_end = Mutation::MoveInPlaylist {
            id: "PLedited".into(),
            set_video_id: "visible".into(),
            before: None,
        };
        assert!(!playlist_change_visible(&to_end, &json!({}), &partial));
    }

    #[test]
    fn playlist_deletion_confirmation_requires_the_requested_id() {
        let confirmed = json!({"command":{"commandExecutorCommand":{"commands":[
            {"handlePlaylistDeletionCommand":{"playlistId":"PLdeleted"}},
            {"removeFromGuideSectionAction":{"guideEntryId":"unrelated"}}
        ]}}});
        assert!(confirm_mutation("playlist/delete", &confirmed, Some("PLdeleted")).is_ok());
        assert!(confirm_mutation("playlist/delete", &confirmed, Some("PLother")).is_err());
        assert!(confirm_mutation("playlist/delete", &confirmed, None).is_err());
        assert!(confirm_mutation("playlist/delete", &json!({}), Some("PLdeleted")).is_err());
        assert!(
            confirm_mutation(
                "playlist/delete",
                &json!({"status":"STATUS_SUCCEEDED"}),
                Some("PLdeleted")
            )
            .is_ok()
        );
        let mut failed = confirmed;
        failed["status"] = json!("STATUS_FAILED");
        assert!(confirm_mutation("playlist/delete", &failed, Some("PLdeleted")).is_err());
    }

    #[test]
    #[ignore = "requires a public YouTube Music artist read; sends no account mutation"]
    fn live_artist_subscription_request_uses_the_canonical_channel() {
        let catalog = Catalog::new().unwrap();
        let artist = Collection {
            id: "UCmMUZbaYdNH0bEd1PAlAqsA".into(),
            title: "Oasis".into(),
            subtitle: String::new(),
            thumbnail: None,
            details: Default::default(),
        };
        for (save, expected_endpoint) in [
            (true, "subscription/subscribe"),
            (false, "subscription/unsubscribe"),
        ] {
            let (endpoint, body) = catalog
                .mutation_request(&Mutation::CollectionLibrary {
                    collection: artist.clone(),
                    save,
                })
                .unwrap();
            assert_eq!(endpoint, expected_endpoint);
            assert_eq!(body, json!({"channelIds":["UCUDVBtnOQi4c7E8jebpjc9Q"]}));
        }
    }

    #[test]
    fn playlist_creation_includes_the_submitted_song_in_its_request() {
        let catalog = Catalog::new().unwrap();
        for initial_video_id in [None, Some("abcdefghijk".to_owned())] {
            let submitted = Mutation::CreatePlaylist {
                title: " First playlist ".into(),
                description: "Description".into(),
                privacy: Privacy::Unlisted,
                initial_video_id: initial_video_id.clone(),
            };
            let (endpoint, body) = catalog.mutation_request(&submitted).unwrap();
            assert_eq!(endpoint, "playlist/create");
            assert_eq!(body["title"], "First playlist");
            assert_eq!(body["description"], "Description");
            assert_eq!(body["privacyStatus"], "UNLISTED");
            match initial_video_id {
                Some(_) => assert_eq!(body["videoIds"], json!(["abcdefghijk"])),
                None => assert!(body.get("videoIds").is_none()),
            }
            let Effect::CreatedPlaylist { id } =
                mutation_effect(&submitted, &json!({"playlistId":"PLcreated"})).unwrap()
            else {
                panic!("expected the created playlist")
            };
            assert_eq!(id, "PLcreated");
            assert!(mutation_effect(&submitted, &json!({})).is_err());
        }
    }
    fn synthetic_session() -> Session {
        serde_json::from_value(json!({
            "cookie":"SAPISID=synthetic-test-value",
            "playback_cookie":"# Netscape HTTP Cookie File\n",
            "authuser":"0", "delegated_session_id":null, "client_version":"test",
            "data_sync_id":"synthetic-user||"
        }))
        .unwrap()
    }

    fn sourced_session() -> Session {
        let mut saved = synthetic_session();
        saved.source_profile = Some(
            serde_json::from_value(json!({
                "browser":"Zen", "name":"Confirmed profile", "path":"/synthetic/confirmed-profile"
            }))
            .unwrap(),
        );
        saved
    }

    fn synthetic_connection(session: Session) -> Connection {
        Connection {
            session: Arc::new(session),
            account: Account {
                name: "Synthetic account".into(),
                handle: String::new(),
            },
        }
    }

    #[test]
    fn valid_saved_sessions_restore_without_reading_the_browser() {
        let (connection, save_session) = restore_connection(
            sourced_session(),
            &Operation::default(),
            |candidate| Ok(synthetic_connection(candidate)),
            |_| panic!("A valid saved session must not read browser cookies"),
        )
        .unwrap();
        assert!(!save_session);
        assert_eq!(connection.account.name, "Synthetic account");
    }

    #[test]
    fn rejected_sessions_refresh_only_the_confirmed_profile_before_activation() {
        let saved = sourced_session();
        let shared = auth::shared(Some(saved.clone()));
        let previous = shared.read().unwrap().clone().unwrap();
        let checks = std::cell::Cell::new(0);
        let operation = Operation::default();
        let (connection, save_session) = restore_connection(
            saved.clone(),
            &operation,
            |candidate| {
                checks.set(checks.get() + 1);
                if checks.get() == 1 {
                    return Err(auth::RejectedSession.into());
                }
                assert_eq!(candidate.client_version, "refreshed");
                Ok(synthetic_connection(candidate))
            },
            |source| {
                assert_eq!(
                    serde_json::to_value(source).unwrap(),
                    serde_json::to_value(saved.source_profile.as_ref().unwrap()).unwrap()
                );
                let mut refreshed = saved.clone();
                refreshed.client_version = "refreshed".into();
                // Account order can change while the full primary/brand identity stays the same.
                refreshed.authuser = "2".into();
                Ok(refreshed)
            },
        )
        .unwrap();
        assert!(save_session);
        assert_eq!(checks.get(), 2);
        assert!(require_current_session(&shared, &previous).is_ok());
        assert!(
            connection
                .activate(&shared, &operation, Err("Player failed".into()))
                .is_err()
        );
        assert!(require_current_session(&shared, &previous).is_ok());
    }

    #[test]
    fn refresh_rejects_other_primary_and_brand_accounts() {
        for (identity, delegated, other, other_delegated) in [
            ("synthetic-user||", None, "other-user||", None),
            (
                "brand||user",
                Some("brand"),
                "other-brand||user",
                Some("other-brand"),
            ),
            (
                "brand||user",
                Some("brand"),
                "brand||other-user",
                Some("brand"),
            ),
        ] {
            let mut stored = serde_json::to_value(sourced_session()).unwrap();
            stored["data_sync_id"] = json!(identity);
            stored["delegated_session_id"] = json!(delegated);
            let saved: Session = serde_json::from_value(stored.clone()).unwrap();
            let shared = auth::shared(Some(saved.clone()));
            let previous = shared.read().unwrap().clone().unwrap();
            let checks = std::cell::Cell::new(0);
            let result = restore_connection(
                saved,
                &Operation::default(),
                |_| {
                    checks.set(checks.get() + 1);
                    Err(auth::RejectedSession.into())
                },
                |_| {
                    stored["data_sync_id"] = json!(other);
                    stored["delegated_session_id"] = json!(other_delegated);
                    Ok(serde_json::from_value(stored).unwrap())
                },
            );
            assert!(
                result
                    .err()
                    .unwrap()
                    .to_string()
                    .contains("different YouTube account")
            );
            assert_eq!(checks.get(), 1);
            assert!(require_current_session(&shared, &previous).is_ok());
        }
    }

    #[test]
    fn failed_and_cancelled_refreshes_preserve_the_existing_connection() {
        for cancel in [false, true] {
            let saved = sourced_session();
            let shared = auth::shared(Some(saved.clone()));
            let previous = shared.read().unwrap().clone().unwrap();
            let operation = Operation::default();
            let result = restore_connection(
                saved.clone(),
                &operation,
                |_| Err(auth::RejectedSession.into()),
                |_| {
                    if cancel {
                        operation.cancel();
                        return Ok(saved);
                    }
                    anyhow::bail!("Browser profile unavailable")
                },
            );
            assert!(result.is_err());
            assert!(require_current_session(&shared, &previous).is_ok());
        }
        let saved = sourced_session();
        let checks = std::cell::Cell::new(0);
        let reads = std::cell::Cell::new(0);
        let result = restore_connection(
            saved.clone(),
            &Operation::default(),
            |_| {
                checks.set(checks.get() + 1);
                Err(auth::RejectedSession.into())
            },
            |_| {
                reads.set(reads.get() + 1);
                Ok(saved)
            },
        );
        assert!(result.is_err());
        assert_eq!(checks.get(), 2);
        assert_eq!(reads.get(), 1, "A rejected refresh must not loop");
    }

    #[test]
    fn transport_errors_and_legacy_sessions_do_not_guess_a_browser_profile() {
        let error = restore_connection(
            sourced_session(),
            &Operation::default(),
            |_| anyhow::bail!("Network unavailable"),
            |_| panic!("A transport error must not trigger a browser read"),
        )
        .err()
        .unwrap();
        assert_eq!(error.to_string(), "Network unavailable");
        let error = restore_connection(
            synthetic_session(),
            &Operation::default(),
            |_| Err(auth::RejectedSession.into()),
            |_| panic!("An older session has no profile consent"),
        )
        .err()
        .unwrap();
        assert!(error.to_string().contains("Reconnect once"));
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
    fn resolved_mutations_are_not_sent_after_sign_out_or_account_replacement() {
        for replacement in [None, Some(Arc::new(synthetic_session()))] {
            let shared = auth::shared(Some(synthetic_session()));
            let expected = shared.read().unwrap().clone().unwrap();
            let result = send_current_mutation(
                &shared,
                &expected,
                || {
                    *shared.write().unwrap() = replacement;
                    Ok((
                        "subscription/subscribe",
                        json!({"channelIds":["UCresolved"]}),
                    ))
                },
                |_, _| panic!("A revoked account must not send the resolved mutation"),
            );
            assert!(result.is_err());
        }

        let shared = auth::shared(Some(synthetic_session()));
        let expected = shared.read().unwrap().clone().unwrap();
        send_current_mutation(
            &shared,
            &expected,
            || {
                Ok((
                    "subscription/subscribe",
                    json!({"channelIds":["UCresolved"]}),
                ))
            },
            |endpoint, body| {
                assert_eq!(endpoint, "subscription/subscribe");
                assert_eq!(body, json!({"channelIds":["UCresolved"]}));
                Ok(json!({"status":"STATUS_SUCCEEDED"}))
            },
        )
        .unwrap();
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
    fn activation_requires_playback_success_and_preserves_the_previous_session_on_failure() {
        for previous in [None, Some(synthetic_session())] {
            let shared = auth::shared(previous);
            let previous = shared.read().unwrap().clone();
            let candidate = Connection {
                session: Arc::new(synthetic_session()),
                account: Account {
                    name: "Candidate".into(),
                    handle: String::new(),
                },
            };
            let operation = Operation::default();
            assert!(
                candidate
                    .clone()
                    .activate(
                        &shared,
                        &operation,
                        Err("Player installation failed".into())
                    )
                    .is_err()
            );
            match previous {
                Some(previous) => assert!(require_current_session(&shared, &previous).is_ok()),
                None => assert!(shared.read().unwrap().is_none()),
            }
            assert!(require_current_session(&shared, &candidate.session).is_err());
            let activated = candidate.activate(&shared, &operation, Ok(())).unwrap();
            assert!(require_current_session(&shared, &activated.session).is_ok());
        }
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
        assert!(candidate.activate(&shared, &completion, Ok(())).is_err());
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
        for command in [
            Command::Restore(operation.clone()),
            Command::Connect(operation),
        ] {
            commands.send_blocking(command).unwrap();
        }
        worker.finish().unwrap();
        assert!(commands.is_closed());
        assert!(commands.is_empty());
        assert!(require_current_session(&shared, &expected).is_ok());
    }
    #[test]
    fn finishing_worker_waits_for_accepted_persistence() {
        let shared = auth::shared(Some(synthetic_session()));
        let expected = shared.read().unwrap().clone().unwrap();
        let (commands, pending) = async_channel::bounded(1);
        let (saving, save_started) = std::sync::mpsc::channel();
        let (resume, save_resume) = std::sync::mpsc::channel();
        let worker = Worker {
            commands: commands.clone(),
            thread: thread::spawn(move || {
                while let Ok(command) = pending.recv_blocking() {
                    let Command::Persist(session) = command else {
                        panic!("expected persistence")
                    };
                    persist_current_session(
                        &shared,
                        &session,
                        || {
                            saving.send(()).unwrap();
                            save_resume.recv().unwrap();
                            Ok(())
                        },
                        || anyhow::bail!("The current session must not be cleared"),
                    )?;
                }
                Ok(())
            }),
        };
        commands.send_blocking(Command::Persist(expected)).unwrap();
        let finished = thread::spawn(move || worker.finish());
        save_started
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        assert!(!finished.is_finished());
        resume.send(()).unwrap();
        finished.join().unwrap().unwrap();
        assert!(commands.is_closed());
        assert!(commands.is_empty());
    }
    #[test]
    fn sign_out_waits_for_playback_cleanup_and_preserves_failures() {
        for result in [Ok(()), Err("Synthetic player cleanup failure".to_owned())] {
            let expected = result.clone();
            let (reply, response) = async_channel::bounded(1);
            let (removed, removal) = std::sync::mpsc::channel();
            let worker = thread::spawn(move || {
                finish_disconnect(Ok(Some(response)), || {
                    removed.send(()).unwrap();
                    Ok(())
                })
            });
            removal
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap();
            assert!(!worker.is_finished());
            reply.send_blocking(result).unwrap();
            assert_eq!(worker.join().unwrap(), expected);
        }
        let (reply, response) = async_channel::bounded(1);
        drop(reply);
        let error = finish_disconnect(Ok(Some(response)), || {
            anyhow::bail!("Synthetic keyring failure")
        })
        .unwrap_err();
        assert!(error.contains("Synthetic keyring failure"));
        assert!(error.contains("player stopped"));

        let removed = std::cell::Cell::new(false);
        let error = finish_disconnect(Err("Player unavailable".into()), || {
            removed.set(true);
            Ok(())
        })
        .unwrap_err();
        assert!(removed.get());
        assert_eq!(error, "Player unavailable");
        assert!(finish_disconnect(Ok(None), || Ok(())).is_ok());
    }
    #[test]
    fn failed_or_missing_acknowledgements_are_not_success() {
        for response in [
            json!({}),
            json!({"feedbackResponses":[]}),
            json!({"feedbackResponses":[{"isProcessed":false}]}),
        ] {
            assert!(confirm_mutation("feedback", &response, None).is_err());
        }
        assert!(
            confirm_mutation(
                "feedback",
                &json!({"feedbackResponses":[{"isProcessed":true}]}),
                None
            )
            .is_ok()
        );
        assert!(confirm_mutation("browse/edit_playlist", &json!({}), None).is_err());
        assert!(
            confirm_mutation(
                "browse/edit_playlist",
                &json!({"status":"STATUS_SUCCEEDED"}),
                None
            )
            .is_ok()
        );
    }
}
