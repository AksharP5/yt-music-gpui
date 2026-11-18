//! Save destinations returned by YouTube Music, reduced to actions the app owns.

use crate::{
    account::Privacy,
    catalog::{Catalog, Collection, Track, find, text, thumbnail, valid_video_id},
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::ops::Range;

pub(crate) const LIKED_MUSIC_ART: &str =
    "https://www.gstatic.com/youtube/media/ytm/images/pbg/liked-songs-delhi-192.png";

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub(crate) enum Destination {
    Playlist(String),
    LikedMusic,
}

#[derive(Clone)]
pub(crate) struct Choice {
    pub target: Destination,
    pub title: String,
    pub byline: String,
    pub thumbnail: Option<String>,
    pub privacy: Option<Privacy>,
    pub pinned: bool,
}

#[derive(Clone)]
pub(crate) struct Empty {
    pub title: String,
    pub detail: String,
}

#[derive(Clone)]
pub(crate) struct Choices {
    pub destinations: Vec<Choice>,
    pub empty: Option<Empty>,
}

pub(crate) enum ChoiceList<'a> {
    Server(&'a Choices),
    Library {
        playlists: &'a [Collection],
        tracks: &'a [Track],
    },
}

impl ChoiceList<'_> {
    pub fn len(&self) -> usize {
        match self {
            Self::Server(choices) => choices.destinations.len(),
            Self::Library { playlists, tracks } => provisional(playlists, tracks).count(),
        }
    }

    pub fn empty_message(&self) -> Option<&Empty> {
        match self {
            Self::Server(choices) => choices.empty.as_ref(),
            Self::Library { .. } => None,
        }
    }

    pub fn index_of(&self, target: &Destination) -> Option<usize> {
        match self {
            Self::Server(choices) => choices
                .destinations
                .iter()
                .position(|choice| &choice.target == target),
            Self::Library { playlists, tracks } => {
                provisional(playlists, tracks).position(|choice| match (choice, target) {
                    (ProvisionalChoice::LikedMusic, Destination::LikedMusic) => true,
                    (ProvisionalChoice::Playlist(_, id), Destination::Playlist(target)) => {
                        id == target
                    }
                    _ => false,
                })
            }
        }
    }

    pub fn rows(&self, range: Range<usize>) -> Vec<Choice> {
        match self {
            Self::Server(choices) => choices
                .destinations
                .iter()
                .skip(range.start)
                .take(range.len())
                .cloned()
                .collect(),
            Self::Library { playlists, tracks } => provisional(playlists, tracks)
                .skip(range.start)
                .take(range.len())
                .map(Choice::from)
                .collect(),
        }
    }
}

impl Catalog {
    pub(crate) fn playlist_choices(&self, video_ids: &[String]) -> Result<Choices> {
        ensure!(self.session.is_some(), "Sign in to load your playlists");
        let response = self.request("playlist/get_add_to_playlist", request_body(video_ids)?)?;
        parse(&response, video_ids)
    }
}

fn request_body(video_ids: &[String]) -> Result<Value> {
    ensure!(
        !video_ids.is_empty() && video_ids.iter().all(|id| valid_video_id(id)),
        "Refresh the songs before choosing a playlist"
    );
    Ok(json!({"videoIds":video_ids,"excludeWatchLater":false}))
}

fn playlist_id(id: &str) -> Option<&str> {
    let id = id.strip_prefix("VL").unwrap_or(id);
    (!id.is_empty()
        && id.len() <= 256
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte)))
    .then_some(id)
}

enum ProvisionalChoice<'a> {
    LikedMusic,
    Playlist(&'a Collection, &'a str),
}

impl From<ProvisionalChoice<'_>> for Choice {
    fn from(row: ProvisionalChoice<'_>) -> Self {
        match row {
            ProvisionalChoice::LikedMusic => Self {
                target: Destination::LikedMusic,
                title: "Liked Music".into(),
                byline: String::new(),
                thumbnail: Some(LIKED_MUSIC_ART.into()),
                privacy: None,
                pinned: false,
            },
            ProvisionalChoice::Playlist(playlist, id) => Self {
                target: Destination::Playlist(id.into()),
                title: playlist.title.clone(),
                byline: playlist.subtitle.clone(),
                thumbnail: playlist.thumbnail.clone(),
                privacy: None,
                pinned: false,
            },
        }
    }
}

fn provisional<'a>(
    playlists: &'a [Collection],
    tracks: &[Track],
) -> impl Iterator<Item = ProvisionalChoice<'a>> + use<'a> {
    let liked = matches!(tracks, [track] if track.details.episode.is_none())
        .then_some(ProvisionalChoice::LikedMusic);
    liked.into_iter().chain(
        playlists
            .iter()
            .filter(|playlist| playlist.details.editable)
            .filter_map(|playlist| {
                let id = playlist_id(&playlist.id).filter(|id| *id != "LM")?;
                Some(ProvisionalChoice::Playlist(playlist, id))
            }),
    )
}

pub(crate) fn parse(response: &Value, video_ids: &[String]) -> Result<Choices> {
    let renderer = find(response, "addToPlaylistRenderer")
        .or_else(|| {
            (response.get("playlists").is_some() || response.get("topShelf").is_some())
                .then_some(response)
        })
        .context("YouTube Music did not return playlist choices")?;
    let rows = match renderer.get("playlists") {
        Some(rows) => rows
            .as_array()
            .context("YouTube Music returned unreadable playlist choices")?
            .as_slice(),
        None if renderer.get("topShelf").is_some() && renderer["actions"].is_array() => &[],
        None => anyhow::bail!("YouTube Music did not return playlist choices"),
    };
    let destinations = rows
        .iter()
        .map(|row| {
            let row = row["playlistAddToOptionRenderer"]
                .as_object()
                .context("YouTube Music returned an unreadable playlist")?;
            let id = row
                .get("playlistId")
                .and_then(Value::as_str)
                .and_then(playlist_id)
                .context("YouTube Music returned an invalid playlist")?;
            let target = if id == "LM" {
                let [video_id] = video_ids else {
                    anyhow::bail!("YouTube Music returned Liked Music for multiple songs")
                };
                let like = &row
                    .get("addToPlaylistServiceEndpoint")
                    .unwrap_or(&Value::Null)["likeEndpoint"];
                ensure!(
                    like["status"] == "LIKE"
                        && like.pointer("/target/videoId").and_then(Value::as_str)
                            == Some(video_id.as_str()),
                    "YouTube Music returned a mismatched Liked Music action"
                );
                Destination::LikedMusic
            } else {
                Destination::Playlist(id.into())
            };
            let title = text(row.get("title").unwrap_or(&Value::Null));
            ensure!(
                !title.is_empty(),
                "YouTube Music returned an unnamed playlist"
            );
            let privacy = row
                .get("privacy")
                .map(|privacy| match privacy.as_str() {
                    Some("PRIVATE") => Ok(Privacy::Private),
                    Some("PUBLIC") => Ok(Privacy::Public),
                    Some("UNLISTED") => Ok(Privacy::Unlisted),
                    _ => anyhow::bail!("YouTube Music returned unreadable playlist privacy"),
                })
                .transpose()?;
            Ok(Choice {
                target,
                title,
                byline: text(row.get("shortBylineText").unwrap_or(&Value::Null)),
                thumbnail: row.get("thumbnailRenderer").and_then(thumbnail),
                privacy,
                pinned: row
                    .get("additionalIcons")
                    .and_then(Value::as_array)
                    .is_some_and(|icons| icons.iter().any(|icon| icon["iconType"] == "KEEP")),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let empty = if destinations.is_empty() {
        find(&renderer["topShelf"], "messageRenderer").map(|message| Empty {
            title: text(&message["text"]),
            detail: text(&message["subtext"]["messageSubtextRenderer"]["text"]),
        })
    } else {
        None
    };
    Ok(Choices {
        destinations,
        empty,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response() -> Value {
        json!({"contents":[{"addToPlaylistRenderer":{
            "playlists":[{"playlistAddToOptionRenderer":{
                "playlistId":"LM", "title":{"runs":[{"text":"Liked Music"}]},
                "shortBylineText":{"simpleText":"4 songs"},
                "thumbnailRenderer":{"musicThumbnailRenderer":{"thumbnail":{"thumbnails":[{"url":LIKED_MUSIC_ART}]}}},
                "additionalIcons":[{"iconType":"KEEP"}],
                "addToPlaylistServiceEndpoint":{"likeEndpoint":{"status":"LIKE","target":{"videoId":"abcdefghijk"}}}
            }}]
        }}]})
    }

    #[test]
    fn requests_preserve_the_selected_songs_and_include_watch_later() {
        let ids = ["abcdefghijk", "lmnopqrstuv", "abcdefghijk"].map(str::to_owned);
        assert_eq!(
            request_body(&ids).unwrap(),
            json!({
                "videoIds":["abcdefghijk","lmnopqrstuv","abcdefghijk"],"excludeWatchLater":false
            })
        );
        assert!(request_body(&[]).is_err());
        assert!(request_body(&["abcdefghijk".into(), "invalid".into()]).is_err());
    }

    #[test]
    fn provisional_liked_music_is_only_for_one_song_and_has_no_invented_count() {
        let mut song: Track = serde_json::from_value(json!({
            "id":"abcdefghijk", "title":"A song", "artist":"An artist"
        }))
        .unwrap();
        let choice = Choice::from(provisional(&[], &[song.clone()]).next().unwrap());
        assert_eq!(choice.target, Destination::LikedMusic);
        assert!(choice.byline.is_empty());
        assert!(!choice.pinned);
        assert!(
            provisional(&[], &[song.clone(), song.clone()])
                .next()
                .is_none()
        );
        song.details.episode = Some(Box::default());
        assert!(provisional(&[], &[song]).next().is_none());
    }

    #[test]
    fn provisional_ranges_count_only_editable_destinations() {
        let playlist = |id: &str, editable| Collection {
            id: id.into(),
            title: id.into(),
            subtitle: String::new(),
            thumbnail: None,
            details: crate::catalog::CollectionDetails {
                editable,
                ..Default::default()
            },
        };
        let playlists = [
            playlist("VLfirst", true),
            playlist("VLnot-editable", false),
            playlist("VL/invalid", true),
            playlist("VLLM", true),
            playlist("VLlast", true),
        ];
        let choices = ChoiceList::Library {
            playlists: &playlists,
            tracks: &[],
        };
        assert_eq!(choices.len(), 2);
        let rows = choices.rows(1..3);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].target, Destination::Playlist("last".into()));
    }

    #[test]
    fn parses_liked_music_and_typed_playlists_without_storing_endpoints() {
        let ids = ["abcdefghijk".into()];
        let mut response = response();
        response["contents"][0]["addToPlaylistRenderer"]["playlists"]
            .as_array_mut().unwrap().push(json!({"playlistAddToOptionRenderer":{
                "playlistId":"PLsynthetic", "title":{"simpleText":"Road trip"},
                "shortBylineText":{"simpleText":"8 songs"}, "privacy":"PRIVATE",
                "addToPlaylistServiceEndpoint":{"playlistEditEndpoint":{"playlistId":"PLsynthetic","actions":[{"action":"ACTION_ADD_VIDEO","addedVideoId":"abcdefghijk"}]}}
            }}));
        let choices = parse(&response, &ids).unwrap();
        assert_eq!(choices.destinations.len(), 2);
        assert_eq!(choices.destinations[0].target, Destination::LikedMusic);
        assert_eq!(choices.destinations[0].byline, "4 songs");
        assert!(choices.destinations[0].pinned);
        assert_eq!(
            choices.destinations[0].thumbnail.as_deref(),
            Some(LIKED_MUSIC_ART)
        );
        assert_eq!(
            choices.destinations[1].target,
            Destination::Playlist("PLsynthetic".into())
        );
        assert!(choices.destinations[1].privacy == Some(Privacy::Private));
        let minimal = parse(
            &json!({"playlists":[{"playlistAddToOptionRenderer":{
                "playlistId":"PLminimal", "title":{"simpleText":"Another playlist"}
            }}]}),
            &ids,
        )
        .unwrap();
        assert_eq!(
            minimal.destinations[0].target,
            Destination::Playlist("PLminimal".into())
        );
        let empty = parse(&json!({"topShelf":{"messageRenderer":{
            "text":{"simpleText":"No playlists yet"},
            "subtext":{"messageSubtextRenderer":{"text":{"simpleText":"Your playlists appear here"}}}
        }},"actions":[]}), &ids).unwrap();
        assert!(empty.destinations.is_empty());
        assert_eq!(empty.empty.unwrap().title, "No playlists yet");
    }

    #[test]
    fn rejects_mismatched_liked_music_and_unreadable_responses() {
        for endpoint in [
            json!({"likeEndpoint":{"status":"INDIFFERENT","target":{"videoId":"abcdefghijk"}}}),
            json!({"likeEndpoint":{"status":"LIKE","target":{"videoId":"lmnopqrstuv"}}}),
            json!({}),
        ] {
            let mut response = response();
            response["contents"][0]["addToPlaylistRenderer"]["playlists"][0]["playlistAddToOptionRenderer"]
                ["addToPlaylistServiceEndpoint"] = endpoint;
            assert!(parse(&response, &["abcdefghijk".into()]).is_err());
        }
        assert!(parse(&response(), &["abcdefghijk".into(), "lmnopqrstuv".into()]).is_err());
        assert!(parse(&json!({}), &["abcdefghijk".into()]).is_err());
        assert!(parse(&json!({"playlists":[{}]}), &["abcdefghijk".into()]).is_err());
    }
}
