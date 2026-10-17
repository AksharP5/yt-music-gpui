use crate::{
    auth::{self, SharedSession},
    catalog::{Account, Catalog, Collection, Page, Rating, Track, find, parse_page},
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    sync::{Arc, atomic::AtomicBool},
    thread,
};

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
pub enum Command {
    Connect(Arc<AtomicBool>),
    Apply(Mutation),
    Verify,
    LoadSaved,
    Disconnect,
}
pub enum Event {
    Connected(Result<Account, String>),
    Disconnected(Result<(), String>),
    Applied {
        mutation: Mutation,
        result: Result<Effect, String>,
    },
}

pub fn start(
    session: SharedSession,
) -> (
    async_channel::Sender<Command>,
    async_channel::Receiver<Event>,
) {
    let (requests, commands) = async_channel::bounded(64);
    let (events, completed) = async_channel::unbounded();
    thread::Builder::new()
        .name("account-sync".into())
        .spawn(move || {
            let catalog = Catalog::with_session(session.clone());
            while let Ok(command) = commands.recv_blocking() {
                let event = match command {
                    Command::Connect(cancelled) => Event::Connected(
                        (|| {
                            let connected = auth::connect(&cancelled)?;
                            *session.write().map_err(|_| {
                                anyhow::anyhow!("Account connection is unavailable")
                            })? = Some(connected);
                            catalog
                                .as_ref()
                                .map_err(|e| anyhow::anyhow!("{e:#}"))?
                                .account()
                        })()
                        .map_err(|e| format!("{e:#}")),
                    ),
                    Command::LoadSaved => match auth::Session::load() {
                        Ok(Some(saved)) => {
                            if let Ok(mut current) = session.write() {
                                *current = Some(saved);
                            }
                            Event::Connected(
                                catalog
                                    .as_ref()
                                    .map_err(|e| format!("{e:#}"))
                                    .and_then(|c| c.account().map_err(|e| format!("{e:#}"))),
                            )
                        }
                        Ok(None) => continue,
                        Err(error) => Event::Connected(Err(format!("{error:#}"))),
                    },
                    Command::Disconnect => {
                        let result = auth::Session::disconnect().map_err(|e| format!("{e:#}"));
                        if result.is_ok()
                            && let Ok(mut current) = session.write()
                        {
                            *current = None;
                        }
                        Event::Disconnected(result)
                    }
                    Command::Verify => Event::Connected(
                        catalog
                            .as_ref()
                            .map_err(|e| format!("{e:#}"))
                            .and_then(|c| c.account().map_err(|e| format!("{e:#}"))),
                    ),
                    Command::Apply(mutation) => {
                        let result = catalog
                            .as_ref()
                            .map_err(|e| format!("{e:#}"))
                            .and_then(|c| c.mutate(&mutation).map_err(|e| format!("{e:#}")));
                        Event::Applied { mutation, result }
                    }
                };
                if events.send_blocking(event).is_err() {
                    break;
                }
            }
        })
        .expect("start account sync worker");
    (requests, completed)
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
        ensure!(
            self.session
                .read()
                .map_err(|_| anyhow::anyhow!("Account connection is unavailable"))?
                .is_some(),
            "Connect your Google account first"
        );
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

#[cfg(test)]
mod tests {
    use super::*;
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

fn playlist_id(id: &str) -> &str {
    id.strip_prefix("VL").unwrap_or(id)
}
