use crate::{
    auth::Session,
    catalog::{
        Catalog, EpisodeContext, PODCAST_EPISODE, Track, duration, episode_context, episode_header,
        text, thumbnail, track_details, valid_service_params, valid_video_id,
    },
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::sync::Arc;

#[derive(Clone)]
pub struct Watch {
    pub current: Option<Track>,
    pub current_index: Option<usize>,
    pub queue: Vec<Track>,
    pub lyrics: Option<String>,
    pub related: Option<String>,
    pub alternatives: Option<Box<MediaPair>>,
    pub continuation: Option<WatchContinuation>,
}

#[derive(Clone)]
pub struct MediaPair {
    pub song: Track,
    pub video: Track,
}

impl MediaPair {
    pub fn target(&self, current: &str, video: bool) -> Option<&Track> {
        if current != self.song.id && current != self.video.id {
            return None;
        }
        Some(if video { &self.video } else { &self.song })
    }
}

#[derive(Clone)]
pub struct WatchContinuation {
    request: Value,
    session: Option<Arc<Session>>,
}

pub struct WatchPage {
    pub queue: Vec<Track>,
    pub continuation: Option<WatchContinuation>,
}

impl Catalog {
    pub fn watch(&self, track: &Track, radio: bool) -> Result<Watch> {
        ensure!(
            valid_video_id(&track.id),
            "This item has an invalid YouTube Music ID"
        );
        ensure!(
            !radio || track.details.episode.is_none(),
            "Radio is not available for podcast episodes"
        );
        let refreshed;
        let track = if track
            .details
            .episode
            .as_ref()
            .is_some_and(|episode| episode.watch_params.is_none())
        {
            let response =
                self.request("browse", json!({"browseId":format!("MPED{}",track.id)}))?;
            refreshed = episode_header(&response, &track.id)?;
            &refreshed
        } else {
            track
        };
        let request = watch_request(&track.id, track.details.episode.as_deref(), radio)?;
        let response = self.request("next", request.clone())?;
        parse_watch(&response, track, request, self.session.clone())
    }

    pub fn watch_more(&self, continuation: &WatchContinuation) -> Result<WatchPage> {
        let same_session = match (&self.session, &continuation.session) {
            (None, None) => true,
            (Some(current), Some(expected)) => Arc::ptr_eq(current, expected),
            _ => false,
        };
        ensure!(
            same_session,
            "The account changed before this radio queue could continue"
        );
        let response = self.request("next", continuation.request.clone())?;
        parse_watch_more(&response, continuation)
    }

    pub fn lyrics(&self, browse_id: &str) -> Result<String> {
        ensure!(
            browse_id.starts_with("MPLY")
                && browse_id.len() <= 256
                && browse_id
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c)),
            "This song has no valid lyrics ID"
        );
        let response = self.request("browse", json!({"browseId": browse_id}))?;
        parse_lyrics(&response)
    }
}

fn watch_request(video_id: &str, episode: Option<&EpisodeContext>, radio: bool) -> Result<Value> {
    ensure!(
        valid_video_id(video_id),
        "This song has an invalid YouTube Music ID"
    );
    if let Some(episode) = episode {
        ensure!(!radio, "Radio is not available for podcast episodes");
        let params = episode
            .watch_params
            .as_deref()
            .filter(|params| !params.is_empty() && valid_service_params(params))
            .context("Refresh this episode before loading its playback details")?;
        return Ok(json!({
            "videoId":video_id,
            "params":params,
            "enablePersistentPlaylistPanel":true,
            "isAudioOnly":true,
            "watchEndpointMusicSupportedConfigs":{
                "watchEndpointMusicConfig":{"musicVideoType":PODCAST_EPISODE}
            }
        }));
    }
    let mut body = json!({
        "videoId": video_id,
        "playlistId": format!("RDAMVM{video_id}"),
        "enablePersistentPlaylistPanel": true,
        "isAudioOnly": true,
        "tunerSettingValue": "AUTOMIX_SETTING_NORMAL"
    });
    if radio {
        body["params"] = json!("wAEB");
    } else {
        body["watchEndpointMusicSupportedConfigs"] = json!({
            "watchEndpointMusicConfig": {
                "hasPersistentPlaylistPanel": true,
                "musicVideoType": "MUSIC_VIDEO_TYPE_ATV"
            }
        });
    }
    Ok(body)
}

fn parse_watch(
    response: &Value,
    seed: &Track,
    request: Value,
    session: Option<Arc<Session>>,
) -> Result<Watch> {
    let tabs = response
        .pointer("/contents/singleColumnMusicWatchNextResultsRenderer/tabbedRenderer/watchNextTabbedResultsRenderer/tabs")
        .and_then(Value::as_array)
        .context("YouTube Music returned no playback details")?;
    let panel = tabs
        .iter()
        .find_map(|tab| {
            tab.pointer("/tabRenderer/content/musicQueueRenderer/content/playlistPanelRenderer")
        })
        .context("YouTube Music returned no playback queue")?;
    let contents = panel["contents"]
        .as_array()
        .context("YouTube Music returned an unreadable playback queue")?;
    let (mut queue, current_index, mut alternatives) = parse_queue(contents, &seed.id);
    ensure!(
        !queue.is_empty(),
        "YouTube Music returned no playable songs"
    );
    if let Some(episode) = &seed.details.episode {
        alternatives = None;
        let current = current_index
            .map(|index| &mut queue[index])
            .context("YouTube Music did not include the requested episode in its playback queue")?;
        current.details.episode = Some(episode.clone());
        if episode.show_id.is_some() {
            current.artist = seed.artist.clone();
        }
        current.duration = current.duration.or(seed.duration);
    }
    let mut lyrics = None;
    let mut related = None;
    for tab in tabs {
        let tab = &tab["tabRenderer"];
        if tab["unselectable"] == true {
            continue;
        }
        let Some(endpoint) = tab.pointer("/endpoint/browseEndpoint") else {
            continue;
        };
        let Some(id) = endpoint["browseId"].as_str().filter(|id| !id.is_empty()) else {
            continue;
        };
        match endpoint
            .pointer(
                "/browseEndpointContextSupportedConfigs/browseEndpointContextMusicConfig/pageType",
            )
            .and_then(Value::as_str)
        {
            Some("MUSIC_PAGE_TYPE_TRACK_LYRICS") => lyrics = Some(id.to_owned()),
            Some("MUSIC_PAGE_TYPE_TRACK_RELATED") => related = Some(id.to_owned()),
            _ => {}
        }
    }
    Ok(Watch {
        current: current_index.map(|index| queue[index].clone()),
        current_index,
        queue,
        lyrics,
        related,
        alternatives,
        continuation: watch_continuation(panel, request, session),
    })
}

fn watch_continuation(
    panel: &Value,
    mut request: Value,
    session: Option<Arc<Session>>,
) -> Option<WatchContinuation> {
    let token = panel["continuations"]
        .as_array()?
        .iter()
        .find_map(|entry| {
            entry
                .get("nextRadioContinuationData")
                .or_else(|| entry.get("nextContinuationData"))?
                .get("continuation")?
                .as_str()
                .filter(|token| !token.is_empty())
        })?;
    request["continuation"] = json!(token);
    Some(WatchContinuation { request, session })
}

fn parse_watch_more(response: &Value, previous: &WatchContinuation) -> Result<WatchPage> {
    let panel = response
        .pointer("/continuationContents/playlistPanelContinuation")
        .context("YouTube Music returned no continued radio queue")?;
    let contents = panel["contents"]
        .as_array()
        .context("YouTube Music returned an unreadable continued radio queue")?;
    // Continuation rows are recommendations, never authoritative metadata for the selected song.
    let (queue, _, _) = parse_queue(contents, "");
    let continuation = if queue.is_empty() {
        None
    } else {
        watch_continuation(panel, previous.request.clone(), previous.session.clone())
    };
    Ok(WatchPage {
        queue,
        continuation,
    })
}

fn parse_queue(
    contents: &[Value],
    current_id: &str,
) -> (Vec<Track>, Option<usize>, Option<Box<MediaPair>>) {
    let mut queue = Vec::new();
    let mut first_match = None;
    let mut selected_match = None;
    for item in contents {
        let row = item.get("playlistPanelVideoRenderer").or_else(|| {
            item.pointer(
                "/playlistPanelVideoWrapperRenderer/primaryRenderer/playlistPanelVideoRenderer",
            )
        });
        let Some(row) = row else { continue };
        let Some(track) = parse_track(row, current_id) else {
            continue;
        };
        let pair = item
            .get("playlistPanelVideoWrapperRenderer")
            .and_then(|wrapper| media_pair(wrapper, &track, current_id));
        if track.id == current_id || pair.is_some() {
            first_match.get_or_insert_with(|| (queue.len(), pair.clone()));
            let selected = row["selected"] == true
                || (pair.is_some()
                    && item
                        .pointer("/playlistPanelVideoWrapperRenderer/counterpart")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(|entry| {
                            entry.pointer("/counterpartRenderer/playlistPanelVideoRenderer")
                        })
                        .any(|row| {
                            row["videoId"].as_str() == Some(current_id) && row["selected"] == true
                        }));
            if selected {
                selected_match.get_or_insert((queue.len(), pair));
            }
        }
        queue.push(track);
    }
    let Some((index, alternatives)) = selected_match.or(first_match) else {
        return (queue, None, None);
    };
    if let Some(pair) = &alternatives
        && queue[index].id != current_id
    {
        queue[index] = if pair.song.id == current_id {
            pair.song.clone()
        } else {
            pair.video.clone()
        };
    }
    (queue, Some(index), alternatives)
}

fn media_pair(wrapper: &Value, primary: &Track, current_id: &str) -> Option<Box<MediaPair>> {
    let primary_row = wrapper.pointer("/primaryRenderer/playlistPanelVideoRenderer")?;
    if primary_row["videoId"].as_str()? != primary.id {
        return None;
    }
    let primary_type = music_video_type(primary_row)?;
    let mut pairs = wrapper["counterpart"]
        .as_array()?
        .iter()
        .filter_map(|entry| entry.pointer("/counterpartRenderer/playlistPanelVideoRenderer"))
        .filter(|row| primary.id == current_id || row["videoId"].as_str() == Some(current_id))
        .filter_map(|row| {
            let counterpart_type = music_video_type(row)?;
            let counterpart = parse_track(row, current_id)?;
            if primary.id == counterpart.id
                || primary.details.episode.is_some()
                || counterpart.details.episode.is_some()
            {
                return None;
            }
            match (primary_type, counterpart_type) {
                ("MUSIC_VIDEO_TYPE_ATV", "MUSIC_VIDEO_TYPE_OMV" | "MUSIC_VIDEO_TYPE_UGC") => {
                    Some(MediaPair {
                        song: primary.clone(),
                        video: counterpart,
                    })
                }
                ("MUSIC_VIDEO_TYPE_OMV" | "MUSIC_VIDEO_TYPE_UGC", "MUSIC_VIDEO_TYPE_ATV") => {
                    Some(MediaPair {
                        song: counterpart,
                        video: primary.clone(),
                    })
                }
                _ => None,
            }
        });
    let pair = pairs.next()?;
    pairs.next().is_none().then(|| Box::new(pair))
}

fn music_video_type(row: &Value) -> Option<&str> {
    let endpoint = row.pointer("/navigationEndpoint/watchEndpoint")?;
    if endpoint["videoId"].as_str()? != row["videoId"].as_str()? {
        return None;
    }
    endpoint
        .pointer("/watchEndpointMusicSupportedConfigs/watchEndpointMusicConfig/musicVideoType")?
        .as_str()
}

fn parse_track(row: &Value, current_id: &str) -> Option<Track> {
    if row.get("unplayableText").is_some() {
        return None;
    }
    let id = row["videoId"].as_str().filter(|id| valid_video_id(id))?;
    let title = text(&row["title"]);
    if title.is_empty() {
        return None;
    }
    let runs: Vec<_> = row["longBylineText"]["runs"]
        .as_array()
        .into_iter()
        .flatten()
        .collect();
    let artists: Vec<_> = runs
        .iter()
        .filter(|run| {
            run.pointer("/navigationEndpoint/browseEndpoint/browseId")
                .and_then(Value::as_str)
                .is_some_and(|id| id.starts_with("UC"))
        })
        .filter_map(|run| run["text"].as_str())
        .collect();
    let artist = if artists.is_empty() {
        runs.first()
            .and_then(|run| run["text"].as_str())
            .unwrap_or("YouTube Music")
            .to_owned()
    } else {
        artists.join(", ")
    };
    let album = runs.iter().find_map(|run| {
        run.pointer("/navigationEndpoint/browseEndpoint/browseId")
            .and_then(Value::as_str)
            .filter(|id| id.starts_with("MPRE"))?;
        run["text"].as_str().map(str::to_owned)
    });
    let mut details = track_details(row, &runs);
    details.episode = episode_context(row, id, &runs).ok()?;
    if id != current_id {
        details.clear_account_state();
    }
    Some(Track {
        id: id.to_owned(),
        title,
        artist,
        album,
        duration: duration(&text(&row["lengthText"])),
        thumbnail: thumbnail(&row["thumbnail"]),
        details,
    })
}

fn parse_lyrics(response: &Value) -> Result<String> {
    let lyrics = response
        .pointer("/contents/sectionListRenderer/contents")
        .and_then(Value::as_array)
        .and_then(|sections| {
            sections
                .iter()
                .find_map(|section| section.pointer("/musicDescriptionShelfRenderer/description"))
        })
        .map(text)
        .filter(|lyrics| !lyrics.trim().is_empty())
        .context("Lyrics are not available for this song")?;
    Ok(lyrics)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{BrowseLink, Rating};

    fn track(id: &str) -> Track {
        Track {
            id: id.into(),
            title: "A song".into(),
            artist: "An artist".into(),
            album: None,
            duration: None,
            thumbnail: None,
            details: Default::default(),
        }
    }

    #[test]
    fn playback_requests_separate_track_details_from_radio() {
        let details = watch_request("abcdefghijk", None, false).unwrap();
        assert_eq!(details["videoId"], "abcdefghijk");
        assert_eq!(details["playlistId"], "RDAMVMabcdefghijk");
        assert!(details.get("params").is_none());
        assert_eq!(details.pointer("/watchEndpointMusicSupportedConfigs/watchEndpointMusicConfig/hasPersistentPlaylistPanel"), Some(&json!(true)));
        let radio = watch_request("abcdefghijk", None, true).unwrap();
        assert_eq!(radio["params"], "wAEB");
        assert!(radio.get("watchEndpointMusicSupportedConfigs").is_none());
        assert!(watch_request("not a video ID", None, true).is_err());
    }

    #[test]
    fn episode_requests_preserve_issued_params_without_music_radio_context() {
        let mut episode = EpisodeContext {
            watch_params: Some("issued-params%3D".into()),
            ..Default::default()
        };
        let request = watch_request("abcdefghijk", Some(&episode), false).unwrap();
        assert_eq!(request["params"], "issued-params%3D");
        assert_eq!(
            request.pointer(
                "/watchEndpointMusicSupportedConfigs/watchEndpointMusicConfig/musicVideoType"
            ),
            Some(&json!(PODCAST_EPISODE))
        );
        assert!(request.get("playlistId").is_none());
        assert!(request.get("tunerSettingValue").is_none());
        assert!(watch_request("abcdefghijk", Some(&episode), true).is_err());
        for params in [
            None,
            Some(String::new()),
            Some("%bad%XX".into()),
            Some("unsafe\nvalue".into()),
        ] {
            episode.watch_params = params;
            assert!(watch_request("abcdefghijk", Some(&episode), false).is_err());
        }

        let mut seed = track("abcdefghijk");
        seed.details.episode = Some(Box::new(episode));
        let error = Catalog::new().unwrap().watch(&seed, true).err().unwrap();
        assert!(error.to_string().contains("Radio is not available"));
    }

    fn watch_response(rows: Value) -> Value {
        json!({"contents":{"singleColumnMusicWatchNextResultsRenderer":{"tabbedRenderer":{
            "watchNextTabbedResultsRenderer":{"tabs":[
                {"tabRenderer":{"endpoint":{"browseEndpoint":{
                    "browseId":"MPTRrelated","browseEndpointContextSupportedConfigs":{
                        "browseEndpointContextMusicConfig":{"pageType":"MUSIC_PAGE_TYPE_TRACK_RELATED"}
                    }
                }}}},
                {"tabRenderer":{"content":{"musicQueueRenderer":{"content":{
                    "playlistPanelRenderer":{"contents":rows}
                }}}}},
                {"tabRenderer":{"endpoint":{"browseEndpoint":{
                    "browseId":"MPLYlyrics","browseEndpointContextSupportedConfigs":{
                        "browseEndpointContextMusicConfig":{"pageType":"MUSIC_PAGE_TYPE_TRACK_LYRICS"}
                    }
                }}}}
            ]}
        }}}})
    }

    fn row(id: &str, selected: bool) -> Value {
        json!({"playlistPanelVideoRenderer":{
            "videoId":id,"selected":selected,"title":{"runs":[{"text":"A song"}]},
            "longBylineText":{"runs":[
                {"text":"First artist","navigationEndpoint":{"browseEndpoint":{"browseId":"UCartist"}}},
                {"text":" & "},
                {"text":"Second artist","navigationEndpoint":{"browseEndpoint":{"browseId":"UCother"}}},
                {"text":" • "},
                {"text":"The album","navigationEndpoint":{"browseEndpoint":{"browseId":"MPREalbum"}}}
            ]},
            "lengthText":{"simpleText":"3:27"},
            "thumbnail":{"thumbnails":[{"url":"https://example.com/cover.jpg","width":320}]},
            "navigationEndpoint":{"watchEndpoint":{"videoId":id,"playlistId":"RDAMVMabcdefghijk"}},
            "menu":{"menuRenderer":{"items":[
                {"toggleMenuServiceItemRenderer":{
                    "defaultServiceEndpoint":{"likeEndpoint":{"status":"INDIFFERENT","target":{"videoId":id}}}
                }},
                {"toggleMenuServiceItemRenderer":{
                    "defaultIcon":{"iconType":"BOOKMARK_BORDER"},"isToggled":true,
                    "defaultServiceEndpoint":{"feedbackEndpoint":{"feedbackToken":format!("add-{id}")}},
                    "toggledServiceEndpoint":{"feedbackEndpoint":{"feedbackToken":format!("remove-{id}")}}
                }}
            ]}}
        }})
    }

    fn media_row(id: &str, selected: bool, kind: &str) -> Value {
        let mut row = row(id, selected);
        row["playlistPanelVideoRenderer"]["navigationEndpoint"]["watchEndpoint"]["watchEndpointMusicSupportedConfigs"] = json!({
            "watchEndpointMusicConfig":{"musicVideoType":kind}
        });
        row
    }

    fn media_wrapper(primary: Value, counterpart: Value) -> Value {
        json!({"playlistPanelVideoWrapperRenderer":{
            "primaryRenderer":primary,
            "counterpart":[{"counterpartRenderer":counterpart}]
        }})
    }

    #[test]
    fn service_media_pairs_keep_each_recordings_own_identity_and_account_state() {
        for video_kind in ["MUSIC_VIDEO_TYPE_OMV", "MUSIC_VIDEO_TYPE_UGC"] {
            let song = media_row("abcdefghijk", true, "MUSIC_VIDEO_TYPE_ATV");
            let video = media_row("lmnopqrstuv", false, video_kind);
            for (primary, counterpart) in [(song.clone(), video.clone()), (video, song)] {
                let response = watch_response(json!([media_wrapper(primary, counterpart)]));
                for current in ["abcdefghijk", "lmnopqrstuv"] {
                    let watch = parse_watch(&response, &track(current), json!({}), None).unwrap();
                    assert_eq!(watch.current.as_ref().unwrap().id, current);
                    assert_eq!(watch.queue.len(), 1);
                    assert_eq!(watch.queue[0].id, current);
                    let pair = watch.alternatives.unwrap();
                    assert_eq!(pair.song.id, "abcdefghijk");
                    assert_eq!(pair.video.id, "lmnopqrstuv");
                    for member in ["abcdefghijk", "lmnopqrstuv"] {
                        assert_eq!(pair.target(member, false).unwrap().id, "abcdefghijk");
                        assert_eq!(pair.target(member, true).unwrap().id, "lmnopqrstuv");
                    }
                    assert!(pair.target("vwxyzABCDEF", false).is_none());
                    assert!(pair.target("vwxyzABCDEF", true).is_none());
                    for member in [&pair.song, &pair.video] {
                        if member.id == current {
                            assert_eq!(member.details.rating, Some(Rating::Like));
                            assert_eq!(
                                member.details.add_library.as_deref(),
                                Some(format!("add-{current}").as_str())
                            );
                        } else {
                            assert_eq!(member.details.rating, None);
                            assert_eq!(member.details.add_library, None);
                            assert_eq!(member.details.in_library, None);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn a_requested_counterpart_replaces_only_the_selected_duplicate_occurrence() {
        for primary_selected in [false, true] {
            let first = media_wrapper(
                media_row("abcdefghijk", false, "MUSIC_VIDEO_TYPE_ATV"),
                media_row("lmnopqrstuv", false, "MUSIC_VIDEO_TYPE_OMV"),
            );
            let mut current_video =
                media_row("lmnopqrstuv", !primary_selected, "MUSIC_VIDEO_TYPE_OMV");
            current_video["playlistPanelVideoRenderer"]["title"] =
                json!({"simpleText":"Selected video"});
            let selected = media_wrapper(
                media_row("abcdefghijk", primary_selected, "MUSIC_VIDEO_TYPE_ATV"),
                current_video,
            );
            let response = watch_response(json!([first, row("vwxyzABCDEF", false), selected]));
            let watch = parse_watch(&response, &track("lmnopqrstuv"), json!({}), None).unwrap();
            assert_eq!(watch.current_index, Some(2));
            assert_eq!(
                watch
                    .queue
                    .iter()
                    .map(|track| track.id.as_str())
                    .collect::<Vec<_>>(),
                ["abcdefghijk", "vwxyzABCDEF", "lmnopqrstuv"]
            );
            assert_eq!(watch.current.unwrap().title, "Selected video");
            assert_eq!(watch.alternatives.unwrap().video.title, "Selected video");
        }
    }

    #[test]
    fn incomplete_ambiguous_and_unrelated_media_rows_do_not_create_switches() {
        let wrapper = media_wrapper(
            media_row("abcdefghijk", true, "MUSIC_VIDEO_TYPE_ATV"),
            media_row("lmnopqrstuv", false, "MUSIC_VIDEO_TYPE_OMV"),
        );
        for (path, value) in [
            (
                "/primaryRenderer/playlistPanelVideoRenderer/navigationEndpoint/watchEndpoint/videoId",
                json!("vwxyzABCDEF"),
            ),
            (
                "/counterpart/0/counterpartRenderer/playlistPanelVideoRenderer/navigationEndpoint/watchEndpoint/videoId",
                json!("vwxyzABCDEF"),
            ),
            (
                "/counterpart/0/counterpartRenderer/playlistPanelVideoRenderer/navigationEndpoint/watchEndpoint/watchEndpointMusicSupportedConfigs/watchEndpointMusicConfig/musicVideoType",
                json!("MUSIC_VIDEO_TYPE_ATV"),
            ),
            (
                "/counterpart/0/counterpartRenderer/playlistPanelVideoRenderer/navigationEndpoint/watchEndpoint/watchEndpointMusicSupportedConfigs/watchEndpointMusicConfig/musicVideoType",
                json!(PODCAST_EPISODE),
            ),
            (
                "/counterpart/0/counterpartRenderer/playlistPanelVideoRenderer/title",
                json!({}),
            ),
            ("/counterpart", json!([])),
        ] {
            let mut invalid = wrapper.clone();
            *invalid["playlistPanelVideoWrapperRenderer"]
                .pointer_mut(path)
                .unwrap() = value;
            let watch = parse_watch(
                &watch_response(json!([invalid])),
                &track("abcdefghijk"),
                json!({}),
                None,
            )
            .unwrap();
            assert!(watch.alternatives.is_none());
            assert_eq!(watch.current.unwrap().id, "abcdefghijk");
        }
        let mut unavailable = wrapper.clone();
        unavailable["playlistPanelVideoWrapperRenderer"]["counterpart"][0]["counterpartRenderer"]
            ["playlistPanelVideoRenderer"]["unplayableText"] = json!({"simpleText":"Unavailable"});
        let mut same_id = wrapper.clone();
        same_id["playlistPanelVideoWrapperRenderer"]["counterpart"][0]["counterpartRenderer"] =
            media_row("abcdefghijk", false, "MUSIC_VIDEO_TYPE_OMV");
        let mut ambiguous = wrapper.clone();
        ambiguous["playlistPanelVideoWrapperRenderer"]["counterpart"]
            .as_array_mut()
            .unwrap()
            .push(json!({
                "counterpartRenderer":media_row("vwxyzABCDEF",false,"MUSIC_VIDEO_TYPE_OMV")
            }));
        for invalid in [unavailable, same_id, ambiguous] {
            assert!(
                parse_watch(
                    &watch_response(json!([invalid])),
                    &track("abcdefghijk"),
                    json!({}),
                    None
                )
                .unwrap()
                .alternatives
                .is_none()
            );
        }
        let response = watch_response(json!([wrapper.clone()]));
        let unrelated = parse_watch(&response, &track("vwxyzABCDEF"), json!({}), None).unwrap();
        assert!(unrelated.current.is_none() && unrelated.alternatives.is_none());
        let response = watch_response(json!([row("abcdefghijk", true), wrapper]));
        let selected = parse_watch(&response, &track("abcdefghijk"), json!({}), None).unwrap();
        assert_eq!(selected.current_index, Some(0));
        assert!(selected.alternatives.is_none());
    }

    #[test]
    fn preserves_queue_order_and_selected_duplicate_metadata() {
        let mut second = row("abcdefghijk", true);
        second["playlistPanelVideoRenderer"]["title"] = json!({"simpleText":"Selected occurrence"});
        let response = watch_response(json!([
            row("abcdefghijk", false),
            row("lmnopqrstuv", false),
            {"playlistPanelVideoWrapperRenderer":{"primaryRenderer":second,"counterpart":[{
                "counterpartRenderer":row("vwxyzABCDEF",false)
            }]}}
        ]));
        let watch = parse_watch(
            &response,
            &track("abcdefghijk"),
            watch_request("abcdefghijk", None, false).unwrap(),
            None,
        )
        .unwrap();
        assert_eq!(
            watch
                .queue
                .iter()
                .map(|track| track.id.as_str())
                .collect::<Vec<_>>(),
            ["abcdefghijk", "lmnopqrstuv", "abcdefghijk"]
        );
        assert_eq!(watch.current_index, Some(2));
        let current = watch.current.unwrap();
        assert_eq!(current.title, "Selected occurrence");
        assert_eq!(current.artist, "First artist, Second artist");
        assert_eq!(current.album.as_deref(), Some("The album"));
        assert_eq!(current.duration, Some(207));
        assert_eq!(current.details.rating, Some(Rating::Like));
        assert_eq!(current.details.in_library, Some(true));
        assert_eq!(
            current.details.add_library.as_deref(),
            Some("add-abcdefghijk")
        );
        assert_eq!(watch.queue[1].details.rating, None);
        assert_eq!(watch.queue[1].details.add_library, None);
        assert_eq!(
            watch.queue[1].details.artist_id.as_deref(),
            Some("UCartist")
        );
        assert_eq!(watch.lyrics.as_deref(), Some("MPLYlyrics"));
        assert_eq!(watch.related.as_deref(), Some("MPTRrelated"));
    }

    #[test]
    fn a_different_selected_song_never_becomes_the_requested_song() {
        let mut unavailable = row("abcdefghijk", true);
        unavailable["playlistPanelVideoRenderer"]["unplayableText"] =
            json!({"simpleText":"Unavailable"});
        let mut response = watch_response(json!([unavailable, row("lmnopqrstuv", true)]));
        let tabs = &mut response["contents"]["singleColumnMusicWatchNextResultsRenderer"]["tabbedRenderer"]
            ["watchNextTabbedResultsRenderer"]["tabs"];
        tabs[2]["tabRenderer"]["unselectable"] = json!(true);
        let watch = parse_watch(
            &response,
            &track("abcdefghijk"),
            watch_request("abcdefghijk", None, false).unwrap(),
            None,
        )
        .unwrap();
        assert!(watch.current.is_none());
        assert_eq!(watch.current_index, None);
        assert_eq!(watch.queue.len(), 1);
        assert_eq!(watch.queue[0].details.rating, None);
        assert_eq!(watch.queue[0].details.add_library, None);
        assert_eq!(watch.lyrics, None);
    }

    #[test]
    fn current_episode_keeps_its_detail_metadata_without_copying_to_other_rows() {
        let mut seed = track("abcdefghijk");
        seed.artist = "The show".into();
        seed.details.episode = Some(Box::new(EpisodeContext {
            show_id: Some("MPSPPLshow".into()),
            description: "Complete episode description".into(),
            subtitle: "Yesterday".into(),
            duration_text: "2 hr 32 min".into(),
            watch_params: Some("issued-params".into()),
            ..Default::default()
        }));
        let mut episode_row = row("lmnopqrstuv", false);
        episode_row["playlistPanelVideoRenderer"]["navigationEndpoint"]["watchEndpoint"] = json!({
            "videoId":"lmnopqrstuv", "params":"other-params",
            "watchEndpointMusicSupportedConfigs":{"watchEndpointMusicConfig":{"musicVideoType":PODCAST_EPISODE}}
        });
        let response = watch_response(json!([
            row("abcdefghijk", false),
            episode_row,
            row("abcdefghijk", true)
        ]));
        let watch = parse_watch(
            &response,
            &seed,
            watch_request(&seed.id, seed.details.episode.as_deref(), false).unwrap(),
            None,
        )
        .unwrap();
        assert_eq!(watch.current_index, Some(2));
        assert_eq!(watch.queue.len(), 3);
        let current = watch.current.unwrap();
        assert_eq!(current.details.episode, seed.details.episode);
        assert_eq!(current.artist, "The show");
        assert_eq!(current.duration, Some(207));
        assert_eq!(current.details.rating, Some(Rating::Like));
        assert!(watch.queue[0].details.episode.is_none());
        let other = watch.queue[1].details.episode.as_ref().unwrap();
        assert!(
            other.show_id.is_none() && other.description.is_empty() && other.watch_params.is_none()
        );
        let mismatch = watch_response(json!([row("lmnopqrstuv", true)]));
        assert!(parse_watch(&mismatch, &seed, json!({}), None).is_err());
    }

    #[test]
    fn continuation_preserves_order_duplicates_and_request_context() {
        let mut response = watch_response(json!([row("abcdefghijk", true)]));
        let panel = response.pointer_mut("/contents/singleColumnMusicWatchNextResultsRenderer/tabbedRenderer/watchNextTabbedResultsRenderer/tabs/1/tabRenderer/content/musicQueueRenderer/content/playlistPanelRenderer").unwrap();
        panel["continuations"] =
            json!([{"nextRadioContinuationData":{"continuation":"first-token"}}]);
        let watch = parse_watch(
            &response,
            &track("abcdefghijk"),
            watch_request("abcdefghijk", None, true).unwrap(),
            None,
        )
        .unwrap();
        let continuation = watch.continuation.unwrap();
        assert_eq!(continuation.request["continuation"], "first-token");
        assert_eq!(continuation.request["playlistId"], "RDAMVMabcdefghijk");
        assert_eq!(continuation.request["params"], "wAEB");

        let mut unavailable = row("lmnopqrstuv", false);
        unavailable["playlistPanelVideoRenderer"]["unplayableText"] =
            json!({"simpleText":"Unavailable"});
        let response = json!({"continuationContents":{"playlistPanelContinuation":{
            "contents":[row("abcdefghijk",true),row("abcdefghijk",false),unavailable,
                {"playlistPanelVideoWrapperRenderer":{"primaryRenderer":row("vwxyzABCDEF",false)}}],
            "continuations":[{"nextRadioContinuationData":{"continuation":"second-token"}}]
        }}});
        let page = parse_watch_more(&response, &continuation).unwrap();
        assert_eq!(
            page.queue
                .iter()
                .map(|track| track.id.as_str())
                .collect::<Vec<_>>(),
            ["abcdefghijk", "abcdefghijk", "vwxyzABCDEF"]
        );
        assert!(
            page.queue
                .iter()
                .all(|track| track.details.rating.is_none() && track.details.add_library.is_none())
        );
        let next = page.continuation.unwrap();
        assert_eq!(next.request["continuation"], "second-token");
        assert_eq!(
            next.request["playlistId"],
            continuation.request["playlistId"]
        );
        assert_eq!(next.request["params"], continuation.request["params"]);
        let terminal = parse_watch_more(&json!({"continuationContents":{"playlistPanelContinuation":{
            "contents":[], "continuations":[{"nextRadioContinuationData":{"continuation":"unusable"}}]
        }}}), &next).unwrap();
        assert!(terminal.queue.is_empty() && terminal.continuation.is_none());
        assert!(parse_watch_more(&json!({}), &next).is_err());
    }

    #[test]
    fn continuation_rejects_another_account_snapshot_before_requesting() {
        let session: Session = serde_json::from_value(json!({
            "cookie":"SAPISID=invented", "playback_cookie":"", "authuser":"0",
            "delegated_session_id":null, "data_sync_id":"synthetic-user||",
            // Invalid headers also keep a regression from issuing any network request.
            "client_version":"invalid\nclient"
        }))
        .unwrap();
        let current = Arc::new(session.clone());
        let catalog = Catalog::with_session(Some(current)).unwrap();
        for expected in [None, Some(Arc::new(session))] {
            let continuation = WatchContinuation {
                request: watch_request("abcdefghijk", None, true).unwrap(),
                session: expected,
            };
            let error = catalog.watch_more(&continuation).err().unwrap();
            assert!(error.to_string().contains("account changed"));
        }
    }

    #[test]
    fn lyric_runs_keep_line_breaks_and_missing_lyrics_are_reported() {
        let response = json!({"contents":{"sectionListRenderer":{"contents":[{
            "musicDescriptionShelfRenderer":{
                "description":{"runs":[{"text":"First line\n"},{"text":"Second line\n"}]},
                "footer":{"runs":[{"text":"Lyrics provider"}]}
            }
        }]}}});
        assert_eq!(
            parse_lyrics(&response).unwrap(),
            "First line\nSecond line\n"
        );
        assert!(parse_lyrics(&json!({"contents":{}})).is_err());
        assert!(
            parse_watch(
                &json!({}),
                &track("abcdefghijk"),
                watch_request("abcdefghijk", None, false).unwrap(),
                None
            )
            .is_err()
        );
    }

    #[test]
    #[ignore = "uses the public YouTube Music next and lyrics endpoints"]
    fn live_public_watch() {
        let catalog = Catalog::new().unwrap();
        let watch = catalog.watch(&track("dQw4w9WgXcQ"), false).unwrap();
        assert_eq!(
            watch.current.as_ref().map(|track| track.id.as_str()),
            Some("dQw4w9WgXcQ")
        );
        assert!(!watch.queue.is_empty());
        if let Some(id) = watch.lyrics {
            assert!(!catalog.lyrics(&id).unwrap().trim().is_empty());
        }
    }

    #[test]
    #[ignore = "uses the public YouTube Music radio continuation endpoint"]
    fn live_public_radio_continuation() {
        let catalog = Catalog::new().unwrap();
        let watch = catalog.watch(&track("dQw4w9WgXcQ"), true).unwrap();
        let first = catalog.watch_more(&watch.continuation.unwrap()).unwrap();
        assert!(!first.queue.is_empty());
        let second = catalog.watch_more(&first.continuation.unwrap()).unwrap();
        assert!(!second.queue.is_empty());
        assert!(second.queue.iter().all(|track| valid_video_id(&track.id)));
    }

    #[test]
    #[ignore = "uses public podcast discovery, episode/show browse and next without an account"]
    fn live_public_podcast_details_and_watch() {
        let catalog = Catalog::new().unwrap();
        let explore = catalog.explore().unwrap();
        let episode = explore
            .tracks()
            .into_iter()
            .find(|track| track.details.episode.is_some())
            .expect("Explore must expose a public episode");
        let page = catalog
            .browse(&BrowseLink::new(format!("MPED{}", episode.id), "Episode"))
            .unwrap();
        let selected = page
            .tracks()
            .pop()
            .expect("Episode header must be playable");
        assert_eq!(selected.id, episode.id);
        let details = selected.details.episode.as_ref().unwrap();
        assert!(!details.duration_text.is_empty() && !page.description.is_empty());
        assert_eq!(details.description, page.description);
        let show_id = details
            .show_id
            .as_ref()
            .expect("Episode must link to its show");
        let show = catalog
            .browse(&BrowseLink::new(show_id, "Podcast"))
            .unwrap();
        assert!(show.tracks().iter().any(|track| {
            track
                .details
                .episode
                .as_ref()
                .and_then(|episode| episode.show_id.as_ref())
                == Some(show_id)
        }));
        if let Some(continuation) = &show.continuation {
            let more = catalog.more(continuation).unwrap();
            let tracks = more.tracks();
            assert!(!tracks.is_empty());
            println!("Podcast show continuation: {} rows", tracks.len());
            assert!(tracks.iter().all(|track| {
                track
                    .details
                    .episode
                    .as_ref()
                    .and_then(|episode| episode.show_id.as_ref())
                    == Some(show_id)
            }));
        }
        let restored: Track =
            serde_json::from_value(serde_json::to_value(&selected).unwrap()).unwrap();
        assert!(
            restored
                .details
                .episode
                .as_ref()
                .unwrap()
                .watch_params
                .is_none()
        );
        let watch = catalog.watch(&restored, false).unwrap();
        let current = watch.current.unwrap();
        assert_eq!(current.id, episode.id);
        assert_eq!(
            current.details.episode.as_ref().unwrap().show_id.as_ref(),
            Some(show_id)
        );
        assert_eq!(
            current.details.episode.as_ref().unwrap().description,
            page.description
        );
        println!(
            "Podcast browse/detail/restore passed; {} watch queue rows",
            watch.queue.len()
        );
    }
}
