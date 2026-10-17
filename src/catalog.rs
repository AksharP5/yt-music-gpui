use crate::auth::{self, SharedSession};
use anyhow::{Context, Result, bail, ensure};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::Duration;

const ORIGIN: &str = "https://music.youtube.com";
const CLIENT_VERSION: &str = "1.20261014.01.00";

#[derive(Clone)]
pub struct Account {
    pub name: String,
    pub handle: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Track {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub album: Option<String>,
    pub duration: Option<u64>,
    pub thumbnail: Option<String>,
    #[serde(default)]
    pub details: TrackDetails,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Rating {
    Like,
    Dislike,
    #[default]
    Indifferent,
}
impl Rating {
    pub fn endpoint(self) -> &'static str {
        match self {
            Self::Like => "like/like",
            Self::Dislike => "like/dislike",
            Self::Indifferent => "like/removelike",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TrackDetails {
    pub rating: Option<Rating>,
    pub in_library: Option<bool>,
    pub add_library: Option<String>,
    pub remove_library: Option<String>,
    pub remove_history: Option<String>,
    pub set_video_id: Option<String>,
    pub artist_id: Option<String>,
    pub album_id: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CollectionDetails {
    pub playlist_id: Option<String>,
    pub saved: Option<bool>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Collection {
    pub id: String,
    pub title: String,
    pub subtitle: String,
    pub thumbnail: Option<String>,
    #[serde(default)]
    pub details: CollectionDetails,
}

#[derive(Clone, Debug)]
pub enum Item {
    Track(Track),
    Collection(Collection),
}

impl Item {
    pub fn id(&self) -> &str {
        match self {
            Self::Track(t) => &t.id,
            Self::Collection(c) => &c.id,
        }
    }
    pub fn title(&self) -> &str {
        match self {
            Self::Track(t) => &t.title,
            Self::Collection(c) => &c.title,
        }
    }
    pub fn subtitle(&self) -> &str {
        match self {
            Self::Track(t) => &t.artist,
            Self::Collection(c) => &c.subtitle,
        }
    }
    pub fn thumbnail(&self) -> Option<&str> {
        match self {
            Self::Track(t) => t.thumbnail.as_deref(),
            Self::Collection(c) => c.thumbnail.as_deref(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Shelf {
    pub title: String,
    pub items: Vec<Item>,
}

#[derive(Clone, Debug)]
pub struct Page {
    pub title: String,
    pub shelves: Vec<Shelf>,
    pub continuation: Option<String>,
    pub editable: bool,
    pub description: String,
    pub privacy: Option<String>,
}

impl Page {
    pub fn tracks(&self) -> Vec<Track> {
        self.shelves
            .iter()
            .flat_map(|s| &s.items)
            .filter_map(|item| {
                if let Item::Track(track) = item {
                    Some(track.clone())
                } else {
                    None
                }
            })
            .collect()
    }
}

#[derive(Clone)]
pub struct Catalog {
    client: Client,
    pub(crate) session: SharedSession,
}

impl Catalog {
    pub fn new() -> Result<Self> {
        Self::with_session(auth::shared(None))
    }

    pub fn with_session(session: SharedSession) -> Result<Self> {
        Ok(Self {
            session, client: Client::builder()
            .user_agent("Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 Chrome/130.0.0.0 Safari/537.36")
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(25))
            .build()? })
    }

    pub fn account(&self) -> Result<Account> {
        let data = self.request("account/account_menu", json!({}))?;
        let header = find(&data, "activeAccountHeaderRenderer")
            .context("Google did not accept the account session. Connect again.")?;
        let name = text(&header["accountName"]);
        ensure!(!name.is_empty(), "Google returned no signed-in account");
        Ok(Account {
            name,
            handle: text(&header["channelHandle"]),
        })
    }

    pub fn home(&self) -> Result<Page> {
        self.browse_named("FEmusic_home", "Home")
    }

    pub fn explore(&self) -> Result<Page> {
        self.browse_named("FEmusic_explore", "Explore")
    }

    pub fn search(&self, query: &str) -> Result<Page> {
        let query = query.trim();
        if query.is_empty() {
            bail!("Enter a song, artist, album, or playlist.");
        }
        let data = self.request("search", json!({"query": query}))?;
        parse_page(&data, &format!("Results for \"{query}\""))
    }

    pub fn browse(&self, id: &str) -> Result<Page> {
        if id.is_empty()
            || id.len() > 256
            || !id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
        {
            bail!("This collection has an invalid YouTube Music ID.");
        }
        self.browse_named(id, "Playlist")
    }

    fn browse_named(&self, id: &str, title: &str) -> Result<Page> {
        let data = self.request("browse", json!({"browseId": id}))?;
        parse_page(&data, title)
    }

    pub fn more(&self, continuation: &str, search: bool) -> Result<Page> {
        let data = self.request(
            if search { "search" } else { "browse" },
            json!({"continuation": continuation}),
        )?;
        parse_page(&data, "More music")
    }

    pub fn request(&self, endpoint: &str, mut body: Value) -> Result<Value> {
        body["context"] = json!({"client": {
            "clientName": "WEB_REMIX", "clientVersion": CLIENT_VERSION, "hl": "en", "gl": "US"
        }});
        let session = self
            .session
            .read()
            .map_err(|_| anyhow::anyhow!("Account connection is unavailable"))?;
        if let Some(session) = session.as_ref() {
            body["context"]["client"]["clientVersion"] = json!(session.client_version);
            if let Some(delegated) = &session.delegated_session_id {
                body["context"]["user"] = json!({"onBehalfOfUser":delegated});
            }
        }
        let request = self
            .client
            .post(format!("{ORIGIN}/youtubei/v1/{endpoint}?prettyPrint=false"))
            .header("Origin", ORIGIN)
            .header("Referer", format!("{ORIGIN}/"))
            .header("X-YouTube-Client-Name", "67")
            .header(
                "X-YouTube-Client-Version",
                session
                    .as_ref()
                    .map_or(CLIENT_VERSION, |s| s.client_version.as_str()),
            )
            .json(&body);
        let request = match session.as_ref() {
            Some(session) => session.apply(request)?,
            None => request,
        };
        drop(session);
        let response = request
            .send()
            .context("Could not reach YouTube Music. Check your connection and try again.")?
            .error_for_status()
            .context("YouTube Music rejected the request")?;
        let value: Value = response
            .json()
            .context("YouTube Music returned an unreadable response")?;
        if let Some(error) = value.get("error") {
            bail!(
                "YouTube Music: {}",
                error["message"].as_str().unwrap_or("request failed")
            );
        }
        Ok(value)
    }
}

pub fn text(value: &Value) -> String {
    if let Some(s) = value["simpleText"].as_str() {
        return s.to_owned();
    }
    value["runs"]
        .as_array()
        .map(|runs| runs.iter().filter_map(|r| r["text"].as_str()).collect())
        .unwrap_or_default()
}

pub fn find<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    match value {
        Value::Object(map) => map
            .get(key)
            .or_else(|| map.values().find_map(|v| find(v, key))),
        Value::Array(items) => items.iter().find_map(|v| find(v, key)),
        _ => None,
    }
}

fn thumbnail(value: &Value) -> Option<String> {
    find(value, "thumbnails")?
        .as_array()?
        .iter()
        .filter(|t| {
            t["url"]
                .as_str()
                .is_some_and(|url| url.starts_with("https://"))
        })
        .min_by_key(|t| t["width"].as_u64().unwrap_or(0).abs_diff(360))?["url"]
        .as_str()
        .map(str::to_owned)
}

fn duration(s: &str) -> Option<u64> {
    let parts: Vec<_> = s.trim().split(':').collect();
    if !(2..=3).contains(&parts.len()) {
        return None;
    }
    let mut seconds = 0_u64;
    for (i, part) in parts.iter().enumerate() {
        let n = part.parse::<u64>().ok()?;
        if i > 0 && n >= 60 {
            return None;
        }
        seconds = seconds.checked_mul(60)?.checked_add(n)?;
    }
    Some(seconds)
}

fn valid_video_id(id: &str) -> bool {
    id.len() == 11
        && id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
}

fn parse_item(value: &Value) -> Option<Item> {
    let row = value
        .get("musicResponsiveListItemRenderer")
        .or_else(|| value.get("musicTwoRowItemRenderer"))
        .or_else(|| value.get("musicCardShelfRenderer"))
        .or_else(|| value.get("musicMultiRowListItemRenderer"))?;
    let columns: Vec<&Value> = row["flexColumns"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|col| col.pointer("/musicResponsiveListItemFlexColumnRenderer/text"))
        .collect();
    let title_value = columns.first().copied().unwrap_or(&row["title"]);
    let title = text(title_value);
    if title.is_empty() {
        return None;
    }
    let endpoint = title_value["runs"]
        .as_array()
        .and_then(|runs| runs.first())
        .and_then(|run| run.get("navigationEndpoint"))
        .or_else(|| row.get("navigationEndpoint"));
    let video_id = row.pointer("/playlistItemData/videoId").and_then(Value::as_str)
        .or_else(|| endpoint?.pointer("/watchEndpoint/videoId")?.as_str())
        .or_else(|| row.pointer("/overlay/musicItemThumbnailOverlayRenderer/content/musicPlayButtonRenderer/playNavigationEndpoint/watchEndpoint/videoId")?.as_str());
    let metadata: Vec<&Value> = columns
        .iter()
        .skip(1)
        .copied()
        .chain(std::iter::once(&row["subtitle"]))
        .chain(
            row["fixedColumns"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|v| v.pointer("/musicResponsiveListItemFixedColumnRenderer/text")),
        )
        .collect();
    if let Some(id) = video_id.filter(|id| valid_video_id(id)) {
        let runs: Vec<&Value> = metadata
            .iter()
            .flat_map(|v| v["runs"].as_array().into_iter().flatten())
            .collect();
        let artist =
            runs.iter()
                .find(|r| {
                    r.pointer("/navigationEndpoint/browseEndpoint/browseId")
                        .and_then(Value::as_str)
                        .is_some_and(|id| id.starts_with("UC"))
                })
                .and_then(|r| r["text"].as_str())
                .map(str::to_owned)
                .or_else(|| {
                    metadata.iter().map(|v| text(v)).find(|s| {
                        !s.is_empty() && !s.starts_with("Song") && !s.starts_with("Video")
                    })
                })
                .unwrap_or_else(|| "YouTube Music".to_owned());
        let album = runs
            .iter()
            .find(|r| {
                r.pointer("/navigationEndpoint/browseEndpoint/browseId")
                    .and_then(Value::as_str)
                    .is_some_and(|id| id.starts_with("MPRE"))
            })
            .and_then(|r| r["text"].as_str())
            .map(str::to_owned);
        let duration = runs
            .iter()
            .filter_map(|r| r["text"].as_str().and_then(duration))
            .next();
        return Some(Item::Track(Track {
            id: id.to_owned(),
            title,
            artist,
            album,
            duration,
            thumbnail: thumbnail(row),
            details: track_details(row, &runs),
        }));
    }
    let id = endpoint?.pointer("/browseEndpoint/browseId")?.as_str()?;
    Some(Item::Collection(Collection {
        id: id.to_owned(),
        title,
        subtitle: metadata
            .iter()
            .map(|v| text(v))
            .find(|s| !s.is_empty())
            .unwrap_or_default(),
        thumbnail: thumbnail(row),
        details: CollectionDetails {
            playlist_id: id.strip_prefix("VL").map(str::to_owned),
            saved: find(row, "likeStatus")
                .and_then(Value::as_str)
                .map(|s| s == "LIKE"),
        },
    }))
}

fn track_details(row: &Value, runs: &[&Value]) -> TrackDetails {
    let browse_id = |prefix: &str| {
        runs.iter()
            .filter_map(|r| {
                r.pointer("/navigationEndpoint/browseEndpoint/browseId")
                    .and_then(Value::as_str)
            })
            .find(|id| id.starts_with(prefix))
            .map(str::to_owned)
    };
    let mut details = TrackDetails {
        rating: find(row, "likeStatus").and_then(|v| serde_json::from_value(v.clone()).ok()),
        set_video_id: find(row, "setVideoId")
            .and_then(Value::as_str)
            .map(str::to_owned),
        artist_id: browse_id("UC"),
        album_id: browse_id("MPRE"),
        ..Default::default()
    };
    for item in row
        .pointer("/menu/menuRenderer/items")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(menu) = item
            .get("toggleMenuServiceItemRenderer")
            .or_else(|| item.get("menuServiceItemRenderer"))
        else {
            continue;
        };
        let icon = menu
            .pointer("/defaultIcon/iconType")
            .or_else(|| menu.pointer("/icon/iconType"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let token = |key: &str| {
            menu.get(key)
                .and_then(|v| find(v, "feedbackToken"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        };
        match icon {
            "BOOKMARK_BORDER" => {
                details.in_library = Some(menu["isToggled"].as_bool().unwrap_or(false));
                details.add_library = token("defaultServiceEndpoint");
                details.remove_library = token("toggledServiceEndpoint");
            }
            "BOOKMARK" => {
                details.in_library = Some(true);
                details.add_library = token("toggledServiceEndpoint");
                details.remove_library = token("defaultServiceEndpoint");
            }
            "REMOVE_FROM_HISTORY" => details.remove_history = token("serviceEndpoint"),
            _ => {}
        }
    }
    details
}

fn parse_shelves(value: &Value, shelves: &mut Vec<Shelf>) {
    match value {
        Value::Object(map) => {
            for key in [
                "musicCarouselShelfRenderer",
                "musicShelfRenderer",
                "musicPlaylistShelfRenderer",
                "musicCardShelfRenderer",
                "itemSectionRenderer",
                "musicPlaylistShelfContinuation",
                "musicShelfContinuation",
                "musicGridRenderer",
            ] {
                if let Some(shelf) = map.get(key) {
                    let title = if key == "musicCarouselShelfRenderer" {
                        text(&shelf["header"]["musicCarouselShelfBasicHeaderRenderer"]["title"])
                    } else {
                        text(&shelf["title"])
                    };
                    let contents = shelf.get("contents").or_else(|| shelf.get("items"));
                    let mut items: Vec<Item> = if key == "musicCardShelfRenderer" {
                        parse_item(value).into_iter().collect()
                    } else {
                        Vec::new()
                    };
                    items.extend(
                        contents
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten()
                            .filter_map(parse_item),
                    );
                    if key == "musicCardShelfRenderer" {
                        for item in &mut items {
                            if let Item::Track(track) = item
                                && (track.artist == "YouTube Music"
                                    || track.artist.ends_with(" plays"))
                            {
                                track.artist = title.clone();
                            }
                        }
                    }
                    if !items.is_empty() {
                        shelves.push(Shelf { title, items });
                    }
                    return;
                }
            }
            for child in map.values() {
                parse_shelves(child, shelves);
            }
        }
        Value::Array(items) => {
            let rows: Vec<_> = items.iter().filter_map(parse_item).collect();
            if !rows.is_empty() {
                shelves.push(Shelf {
                    title: String::new(),
                    items: rows,
                });
            }
            for item in items {
                if parse_item(item).is_none() {
                    parse_shelves(item, shelves);
                }
            }
        }
        _ => {}
    }
}

pub fn parse_page(data: &Value, fallback_title: &str) -> Result<Page> {
    let mut shelves = Vec::new();
    parse_shelves(data, &mut shelves);
    let title = [
        "musicResponsiveHeaderRenderer",
        "musicDetailHeaderRenderer",
        "musicImmersiveHeaderRenderer",
    ]
    .iter()
    .find_map(|key| find(data, key).map(|header| text(&header["title"])))
    .filter(|s| !s.is_empty())
    .unwrap_or_else(|| fallback_title.to_owned());
    let continuation = find(data, "nextContinuationData")
        .and_then(|v| v["continuation"].as_str())
        .or_else(|| find(data, "continuationCommand").and_then(|v| v["token"].as_str()))
        .map(str::to_owned);
    if shelves.is_empty() && find(data, "contents").is_none() {
        bail!("YouTube Music returned no music. Try again or open a different collection.");
    }
    Ok(Page {
        title,
        shelves,
        continuation,
        editable: find(data, "musicEditablePlaylistDetailHeaderRenderer").is_some()
            || find(data, "editablePlaylistId").is_some(),
        description: find(data, "description").map(text).unwrap_or_default(),
        privacy: find(data, "musicPlaylistEditHeaderRenderer")
            .and_then(|v| v["privacy"].as_str())
            .map(str::to_owned),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_durations_without_accepting_metadata() {
        assert_eq!(duration("4:27"), Some(267));
        assert_eq!(duration("1:02:03"), Some(3723));
        assert_eq!(duration("17M views"), None);
        assert_eq!(duration("2:75"), None);
    }

    #[test]
    fn song_requires_a_real_video_id_and_title() {
        let row = json!({"musicResponsiveListItemRenderer": {
            "playlistItemData":{"videoId":"abcdefghijk"},
            "flexColumns":[
                {"musicResponsiveListItemFlexColumnRenderer":{"text":{"runs":[{"text":"A song"}]}}},
                {"musicResponsiveListItemFlexColumnRenderer":{"text":{"runs":[{"text":"An artist","navigationEndpoint":{"browseEndpoint":{"browseId":"UCartist"}}},{"text":" • "},{"text":"3:42"}]}}}
            ]
        }});
        let Some(Item::Track(track)) = parse_item(&row) else {
            panic!("expected a track")
        };
        assert_eq!(track.artist, "An artist");
        assert_eq!(track.duration, Some(222));
        let mut invalid = row.clone();
        invalid["musicResponsiveListItemRenderer"]["playlistItemData"]["videoId"] =
            json!("invalid-video-id");
        assert!(parse_item(&invalid).is_none());
        assert!(parse_item(&json!({})).is_none());
    }

    #[test]
    fn playlist_navigation_is_not_its_shuffle_endpoint() {
        let item = json!({"musicTwoRowItemRenderer": {
            "title":{"runs":[{"text":"My mix","navigationEndpoint":{"browseEndpoint":{"browseId":"VLplaylist"}}}]},
            "menu":{"watchEndpoint":{"videoId":"abcdefghijk"}}
        }});
        assert!(matches!(parse_item(&item), Some(Item::Collection(c)) if c.id == "VLplaylist"));
    }

    #[test]
    fn parses_current_search_item_sections() {
        let data = json!({"contents":{"sectionListRenderer":{"contents":[{"itemSectionRenderer":{"contents":[{
            "musicResponsiveListItemRenderer":{"title":{"runs":[{"text":"A song","navigationEndpoint":{"watchEndpoint":{"videoId":"abcdefghijk"}}}]}}
        }]}}]}}});
        assert_eq!(parse_page(&data, "Search").unwrap().tracks().len(), 1);
    }

    #[test]
    #[ignore = "uses the live YouTube Music service"]
    fn live_catalog() {
        let api = Catalog::new().unwrap();
        let home = api.home().unwrap();
        assert!(!home.shelves.is_empty());
        let results = api.search("Khruangbin").unwrap();
        assert!(!results.tracks().is_empty());
        let collection = home
            .shelves
            .iter()
            .flat_map(|s| &s.items)
            .find_map(|i| {
                if let Item::Collection(c) = i {
                    Some(c)
                } else {
                    None
                }
            })
            .unwrap();
        let page = api.browse(&collection.id).unwrap();
        assert!(!page.tracks().is_empty());
        println!(
            "Home: {} shelves; search: {} tracks; {}: {} tracks",
            home.shelves.len(),
            results.tracks().len(),
            page.title,
            page.tracks().len()
        );
    }
}
