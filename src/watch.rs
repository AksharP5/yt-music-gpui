use crate::catalog::{Catalog, Track, duration, text, thumbnail, track_details, valid_video_id};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};

#[derive(Clone)]
pub struct Watch {
    pub current: Option<Track>,
    pub current_index: Option<usize>,
    pub queue: Vec<Track>,
    pub lyrics: Option<String>,
    pub related: Option<String>,
}

impl Catalog {
    pub fn watch(&self, video_id: &str, radio: bool) -> Result<Watch> {
        let response = self.request("next", watch_request(video_id, radio)?)?;
        parse_watch(&response, video_id)
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

fn watch_request(video_id: &str, radio: bool) -> Result<Value> {
    ensure!(
        valid_video_id(video_id),
        "This song has an invalid YouTube Music ID"
    );
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

fn parse_watch(response: &Value, video_id: &str) -> Result<Watch> {
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
        let Some(track) = parse_track(row, video_id) else {
            continue;
        };
        if track.id == video_id {
            first_match.get_or_insert(queue.len());
            if row["selected"] == true {
                selected_match.get_or_insert(queue.len());
            }
        }
        queue.push(track);
    }
    ensure!(
        !queue.is_empty(),
        "YouTube Music returned no playable songs"
    );
    let current_index = selected_match.or(first_match);
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
    })
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
    use crate::catalog::Rating;

    #[test]
    fn playback_requests_separate_track_details_from_radio() {
        let details = watch_request("abcdefghijk", false).unwrap();
        assert_eq!(details["videoId"], "abcdefghijk");
        assert_eq!(details["playlistId"], "RDAMVMabcdefghijk");
        assert!(details.get("params").is_none());
        assert_eq!(details.pointer("/watchEndpointMusicSupportedConfigs/watchEndpointMusicConfig/hasPersistentPlaylistPanel"), Some(&json!(true)));
        let radio = watch_request("abcdefghijk", true).unwrap();
        assert_eq!(radio["params"], "wAEB");
        assert!(radio.get("watchEndpointMusicSupportedConfigs").is_none());
        assert!(watch_request("not a video ID", true).is_err());
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
        let watch = parse_watch(&response, "abcdefghijk").unwrap();
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
        let watch = parse_watch(&response, "abcdefghijk").unwrap();
        assert!(watch.current.is_none());
        assert_eq!(watch.current_index, None);
        assert_eq!(watch.queue.len(), 1);
        assert_eq!(watch.queue[0].details.rating, None);
        assert_eq!(watch.queue[0].details.add_library, None);
        assert_eq!(watch.lyrics, None);
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
        assert!(parse_watch(&json!({}), "abcdefghijk").is_err());
    }

    #[test]
    #[ignore = "uses the public YouTube Music next and lyrics endpoints"]
    fn live_public_watch() {
        let catalog = Catalog::new().unwrap();
        let watch = catalog.watch("dQw4w9WgXcQ", false).unwrap();
        assert_eq!(
            watch.current.as_ref().map(|track| track.id.as_str()),
            Some("dQw4w9WgXcQ")
        );
        assert!(!watch.queue.is_empty());
        if let Some(id) = watch.lyrics {
            assert!(!catalog.lyrics(&id).unwrap().trim().is_empty());
        }
    }
}
