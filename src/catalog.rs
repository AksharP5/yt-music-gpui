use crate::auth::Session;
use anyhow::{Context, Result, bail, ensure};
use reqwest::{blocking::Client, header::HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    fmt,
    sync::Arc,
    time::Duration,
};

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

impl Track {
    pub fn credits_id(&self) -> Option<String> {
        self.details
            .credits_id
            .clone()
            .or_else(|| valid_video_id(&self.id).then(|| format!("MPTC{}", self.id)))
    }
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
    #[serde(skip)]
    pub rating: Option<Rating>,
    #[serde(skip)]
    pub in_library: Option<bool>,
    #[serde(skip)]
    pub add_library: Option<String>,
    #[serde(skip)]
    pub remove_library: Option<String>,
    #[serde(skip)]
    pub remove_history: Option<String>,
    #[serde(skip)]
    pub set_video_id: Option<String>,
    pub artist_id: Option<String>,
    pub album_id: Option<String>,
    pub credits_id: Option<String>,
}

impl TrackDetails {
    pub fn clear_account_state(&mut self) {
        self.rating = None;
        self.in_library = None;
        self.add_library = None;
        self.remove_library = None;
        self.remove_history = None;
        self.set_video_id = None;
    }
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BrowseLink {
    pub title: String,
    pub id: String,
    pub params: Option<String>,
    pub chart_country: Option<String>,
}

impl BrowseLink {
    pub fn new(id: impl Into<String>, title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            id: id.into(),
            params: None,
            chart_country: None,
        }
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            !self.id.is_empty()
                && self.id.len() <= 256
                && self
                    .id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte)),
            "This page has an invalid YouTube Music ID"
        );
        ensure!(!self.title.trim().is_empty(), "This page has no name");
        if let Some(params) = &self.params {
            ensure!(
                params.len() <= 8192,
                "YouTube Music returned an invalid page link"
            );
            let mut bytes = params.bytes();
            while let Some(byte) = bytes.next() {
                let valid = if byte == b'%' {
                    bytes.next().is_some_and(|byte| byte.is_ascii_hexdigit())
                        && bytes.next().is_some_and(|byte| byte.is_ascii_hexdigit())
                } else {
                    byte.is_ascii_alphanumeric() || b"_-+=/.~".contains(&byte)
                };
                ensure!(valid, "YouTube Music returned an invalid page link");
            }
        }
        if let Some(country) = &self.chart_country {
            ensure!(
                self.id == "FEmusic_charts" && valid_chart_country(country),
                "YouTube Music returned an invalid chart country"
            );
        }
        Ok(())
    }

    fn from_endpoint(endpoint: &Value, title: String) -> Option<Self> {
        let link = Self {
            title,
            id: endpoint["browseId"].as_str()?.to_owned(),
            params: match endpoint.get("params") {
                Some(params) => Some(params.as_str()?.to_owned()),
                None => None,
            },
            chart_country: None,
        };
        link.validate().ok()?;
        Some(link)
    }

    fn body(&self) -> Result<Value> {
        self.validate()?;
        let mut body = json!({"browseId":self.id});
        if let Some(params) = &self.params {
            body["params"] = json!(params);
        }
        if let Some(country) = &self.chart_country {
            body["formData"] = json!({"selectedValues":[country]});
        }
        Ok(body)
    }
}

#[derive(Clone, Debug)]
pub struct BrowseChoice {
    pub label: String,
    pub selected: bool,
    pub target: BrowseLink,
}

#[derive(Clone, Debug)]
pub enum Item {
    Track(Track),
    Collection(Collection),
    Navigation(BrowseLink),
}

impl Item {
    pub fn id(&self) -> &str {
        match self {
            Self::Track(t) => &t.id,
            Self::Collection(c) => &c.id,
            Self::Navigation(link) => &link.id,
        }
    }
    pub fn title(&self) -> &str {
        match self {
            Self::Track(t) => &t.title,
            Self::Collection(c) => &c.title,
            Self::Navigation(link) => &link.title,
        }
    }
    pub fn subtitle(&self) -> &str {
        match self {
            Self::Track(t) => &t.artist,
            Self::Collection(c) => &c.subtitle,
            Self::Navigation(_) => "",
        }
    }
    pub fn thumbnail(&self) -> Option<&str> {
        match self {
            Self::Track(t) => t.thumbnail.as_deref(),
            Self::Collection(c) => c.thumbnail.as_deref(),
            Self::Navigation(_) => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Shelf {
    pub title: String,
    pub items: Vec<Item>,
}

#[derive(Clone)]
pub struct Continuation {
    token: String,
    visitor: Option<HeaderValue>,
    session: Option<Arc<Session>>,
}

impl Continuation {
    fn new(token: &str) -> Self {
        Self {
            token: token.to_owned(),
            visitor: None,
            session: None,
        }
    }

    pub(crate) fn token(&self) -> &str {
        &self.token
    }

    fn require_session(&self, session: Option<&Arc<Session>>) -> Result<()> {
        let matches = match (self.session.as_ref(), session) {
            (None, None) => true,
            (Some(expected), Some(current)) => Arc::ptr_eq(expected, current),
            _ => false,
        };
        ensure!(
            matches,
            "The account changed before this page could continue"
        );
        Ok(())
    }
}

impl fmt::Debug for Continuation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Continuation { .. }")
    }
}

#[derive(Clone, Debug)]
pub struct Page {
    pub title: String,
    pub shelves: Vec<Shelf>,
    pub continuation: Option<Continuation>,
    pub editable: bool,
    pub description: String,
    pub privacy: Option<String>,
    pub chips: Vec<BrowseChoice>,
    pub countries: Vec<BrowseChoice>,
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
    pub(crate) client: Client,
    pub(crate) session: Option<Arc<Session>>,
}

impl Catalog {
    pub fn new() -> Result<Self> {
        Self::with_session(None)
    }

    pub fn with_session(session: Option<Arc<Session>>) -> Result<Self> {
        Ok(Self {
            session, client: Client::builder()
            .user_agent("Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 Chrome/130.0.0.0 Safari/537.36")
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(25))
            .redirect(reqwest::redirect::Policy::none())
            .build()? })
    }

    pub fn for_session(&self, session: Option<Arc<Session>>) -> Self {
        Self {
            client: self.client.clone(),
            session,
        }
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
        self.browse(&BrowseLink::new("FEmusic_home", "Home"))
    }

    pub fn explore(&self) -> Result<Page> {
        self.browse(&BrowseLink::new("FEmusic_explore", "Explore"))
    }

    pub fn browse(&self, link: &BrowseLink) -> Result<Page> {
        let data = self.request("browse", link.body()?)?;
        let mut page = parse_page(&data, &link.title)?;
        if matches!(link.id.as_str(), "FEmusic_home" | "FEmusic_explore") {
            page.continuation = data
                .pointer("/contents/singleColumnBrowseResultsRenderer/tabs/0/tabRenderer/content/sectionListRenderer")
                .and_then(section_continuation);
        }
        match link.id.as_str() {
            "FEmusic_home" => page.chips = home_choices(&data)?,
            "FEmusic_charts" => page.countries = chart_choices(&data, link)?,
            _ => {}
        }
        self.bind_page(page, &data, None)
    }

    pub fn more(&self, continuation: &Continuation) -> Result<Page> {
        let data = self.request_with_continuation(
            "browse",
            json!({"continuation": continuation.token()}),
            Some(continuation),
        )?;
        self.bind_page(parse_page(&data, "More music")?, &data, Some(continuation))
    }

    pub(crate) fn bind_page(
        &self,
        mut page: Page,
        response: &Value,
        previous: Option<&Continuation>,
    ) -> Result<Page> {
        if let Some(continuation) = &mut page.continuation {
            continuation.session = self.session.clone();
            continuation.visitor = match previous.and_then(|previous| previous.visitor.clone()) {
                Some(visitor) => Some(visitor),
                None => response
                    .pointer("/responseContext/visitorData")
                    .map(|value| -> Result<HeaderValue> {
                        let visitor = value
                            .as_str()
                            .context("YouTube Music returned an invalid page visitor")?;
                        let mut visitor = HeaderValue::from_str(visitor)
                            .context("YouTube Music returned an invalid page visitor")?;
                        visitor.set_sensitive(true);
                        Ok(visitor)
                    })
                    .transpose()?,
            };
        }
        Ok(page)
    }

    pub fn request(&self, endpoint: &str, body: Value) -> Result<Value> {
        self.request_with_continuation(endpoint, body, None)
    }

    pub(crate) fn request_with_continuation(
        &self,
        endpoint: &str,
        body: Value,
        continuation: Option<&Continuation>,
    ) -> Result<Value> {
        let response = self
            .page_request(endpoint, body, continuation)?
            .send()
            .map_err(reqwest::Error::without_url)
            .context("Could not reach YouTube Music. Check your connection and try again.")?
            .error_for_status()
            .map_err(reqwest::Error::without_url)
            .context("YouTube Music rejected the request")?;
        let value: Value = response
            .json()
            .map_err(reqwest::Error::without_url)
            .context("YouTube Music returned an unreadable response")?;
        if let Some(error) = value.get("error") {
            bail!(
                "YouTube Music: {}",
                error["message"].as_str().unwrap_or("request failed")
            );
        }
        Ok(value)
    }

    fn page_request(
        &self,
        endpoint: &str,
        mut body: Value,
        continuation: Option<&Continuation>,
    ) -> Result<reqwest::blocking::RequestBuilder> {
        if let Some(continuation) = continuation {
            continuation.require_session(self.session.as_ref())?;
        }
        body["context"] = json!({"client": {
            "clientName": "WEB_REMIX", "clientVersion": CLIENT_VERSION, "hl": "en", "gl": "US"
        }});
        let session = &self.session;
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
        let request = match continuation.and_then(|continuation| continuation.visitor.as_ref()) {
            Some(visitor) => request.header("X-Goog-Visitor-Id", visitor),
            None => request,
        };
        let request = match session.as_ref() {
            Some(session) => session.apply(request)?,
            None => request,
        };
        Ok(request)
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

pub(crate) fn thumbnail(value: &Value) -> Option<String> {
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

pub(crate) fn duration(s: &str) -> Option<u64> {
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

pub(crate) fn valid_video_id(id: &str) -> bool {
    id.len() == 11
        && id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
}

fn parse_item(value: &Value) -> Option<Item> {
    if let Some(button) = value.get("musicNavigationButtonRenderer") {
        let endpoint = button.pointer("/clickCommand/browseEndpoint")?;
        let link = BrowseLink::from_endpoint(endpoint, text(&button["buttonText"]))?;
        return Some(Item::Navigation(link));
    }
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
        details: collection_details(row, id),
    }))
}

fn item_rating(row: &Value) -> Option<Rating> {
    let menu = row.pointer("/menu/menuRenderer")?;
    if let Some(rating) = menu["topLevelButtons"]
        .as_array()
        .into_iter()
        .flatten()
        .find_map(|button| button.pointer("/likeButtonRenderer/likeStatus"))
        .and_then(|value| serde_json::from_value(value.clone()).ok())
    {
        return Some(rating);
    }
    menu["items"]
        .as_array()?
        .iter()
        .filter_map(|item| item.get("toggleMenuServiceItemRenderer"))
        .find_map(|toggle| {
            let status = toggle
                .pointer("/defaultServiceEndpoint/likeEndpoint/status")?
                .as_str()?;
            match status {
                "LIKE" if toggle["isToggled"] == true => Some(Rating::Like),
                "LIKE" => Some(Rating::Indifferent),
                "INDIFFERENT" => Some(Rating::Like),
                _ => None,
            }
        })
}

fn collection_details(row: &Value, browse_id: &str) -> CollectionDetails {
    let playlist_id = browse_id.strip_prefix("VL").or_else(|| {
        row.pointer("/menu/menuRenderer/items")?
            .as_array()?
            .iter()
            .filter_map(|item| item.get("toggleMenuServiceItemRenderer"))
            .find_map(|toggle| {
                toggle
                    .pointer("/defaultServiceEndpoint/likeEndpoint/target/playlistId")?
                    .as_str()
            })
    });
    CollectionDetails {
        playlist_id: playlist_id.map(str::to_owned),
        saved: item_rating(row).map(|rating| rating == Rating::Like),
    }
}

pub(crate) fn track_details(row: &Value, runs: &[&Value]) -> TrackDetails {
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
        rating: item_rating(row),
        set_video_id: row
            .pointer("/playlistItemData/setVideoId")
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
        if let Some(id) = item
            .pointer("/menuNavigationItemRenderer/navigationEndpoint/browseEndpoint/browseId")
            .and_then(Value::as_str)
            .filter(|id| id.starts_with("MPTC"))
        {
            details.credits_id = Some(id.to_owned());
        }
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
                .and_then(|v| v.pointer("/feedbackEndpoint/feedbackToken"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        };
        if let Some(action) = menu
            .pointer("/serviceEndpoint/playlistEditEndpoint/actions")
            .and_then(Value::as_array)
            .and_then(|actions| {
                actions
                    .iter()
                    .find(|action| action["action"] == "ACTION_REMOVE_VIDEO")
            })
        {
            details.set_video_id = action["setVideoId"].as_str().map(str::to_owned);
        }
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
                "gridRenderer",
            ] {
                if let Some(shelf) = map.get(key) {
                    let title = if key == "musicCarouselShelfRenderer" {
                        text(&shelf["header"]["musicCarouselShelfBasicHeaderRenderer"]["title"])
                    } else if key == "gridRenderer" {
                        text(&shelf["header"]["gridHeaderRenderer"]["title"])
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

fn section_continuation(owner: &Value) -> Option<Continuation> {
    owner["continuations"]
        .as_array()?
        .iter()
        .find_map(|entry| {
            entry
                .pointer("/nextContinuationData/continuation")
                .and_then(Value::as_str)
        })
        .filter(|token| !token.is_empty())
        .map(Continuation::new)
}

pub fn parse_page(data: &Value, fallback_title: &str) -> Result<Page> {
    let mut shelves = Vec::new();
    parse_shelves(data, &mut shelves);
    let editable_header = find(data, "musicEditablePlaylistDetailHeaderRenderer");
    let edit_metadata = editable_header
        .and_then(|header| header.pointer("/editHeader/musicPlaylistEditHeaderRenderer"));
    let header = [
        "musicResponsiveHeaderRenderer",
        "musicDetailHeaderRenderer",
        "musicImmersiveHeaderRenderer",
    ]
    .iter()
    .find_map(|key| find(editable_header.unwrap_or(data), key));
    let title = header
        .map(|header| text(&header["title"]))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| fallback_title.to_owned());
    let description = edit_metadata
        .and_then(|header| header.get("description"))
        .or_else(|| {
            let description = header?.get("description")?;
            description
                .pointer("/musicDescriptionShelfRenderer/description")
                .or(Some(description))
        })
        .map(text)
        .unwrap_or_default();
    let continuation = match data
        .pointer("/continuationContents/sectionListContinuation")
        .or_else(|| data.get("sectionListContinuation"))
    {
        Some(owner) => section_continuation(owner),
        None => find(data, "nextContinuationData")
            .and_then(|v| v["continuation"].as_str())
            .or_else(|| find(data, "continuationCommand").and_then(|v| v["token"].as_str()))
            .filter(|token| !token.is_empty())
            .map(Continuation::new),
    };
    if shelves.is_empty() && find(data, "contents").is_none() {
        bail!("YouTube Music returned no music. Try again or open a different collection.");
    }
    Ok(Page {
        title,
        shelves,
        continuation,
        editable: editable_header.is_some(),
        description,
        privacy: edit_metadata
            .and_then(|v| v["privacy"].as_str())
            .filter(|privacy| matches!(*privacy, "PRIVATE" | "UNLISTED" | "PUBLIC"))
            .map(str::to_owned),
        chips: Vec::new(),
        countries: Vec::new(),
    })
}

fn home_choices(data: &Value) -> Result<Vec<BrowseChoice>> {
    let Some(chips) = data.pointer("/contents/singleColumnBrowseResultsRenderer/tabs/0/tabRenderer/content/sectionListRenderer/header/chipCloudRenderer/chips").and_then(Value::as_array) else {
        return Ok(Vec::new());
    };
    let mut choices = Vec::new();
    let mut reset: Option<BrowseLink> = None;
    let mut selected_count = 0;
    for chip in chips
        .iter()
        .filter_map(|chip| chip.get("chipCloudChipRenderer"))
    {
        let label = text(&chip["text"]);
        ensure!(
            !label.trim().is_empty(),
            "YouTube Music returned an unreadable Home filter"
        );
        let selected = chip["isSelected"]
            .as_bool()
            .context("YouTube Music did not identify the selected Home filter")?;
        let target =
            BrowseLink::from_endpoint(&chip["navigationEndpoint"]["browseEndpoint"], "Home".into())
                .context("YouTube Music returned an invalid Home filter")?;
        let deselect = BrowseLink::from_endpoint(
            &chip["onDeselectedCommand"]["browseEndpoint"],
            "Home".into(),
        )
        .context("YouTube Music returned an invalid Home reset filter")?;
        ensure!(
            target.id == "FEmusic_home" && deselect.id == "FEmusic_home",
            "YouTube Music returned an invalid Home filter destination"
        );
        ensure!(
            reset.as_ref().is_none_or(|reset| reset == &deselect),
            "YouTube Music returned conflicting Home reset filters"
        );
        reset = Some(deselect.clone());
        selected_count += usize::from(selected);
        choices.push(BrowseChoice {
            label,
            selected,
            target: if selected { deselect } else { target },
        });
    }
    ensure!(
        selected_count <= 1,
        "YouTube Music returned conflicting selected Home filters"
    );
    if let Some(target) = reset {
        choices.insert(
            0,
            BrowseChoice {
                label: "All".into(),
                selected: selected_count == 0,
                target,
            },
        );
    }
    Ok(choices)
}

fn valid_chart_country(country: &str) -> bool {
    country.len() == 2 && country.bytes().all(|byte| byte.is_ascii_uppercase())
}

fn chart_choices(data: &Value, current: &BrowseLink) -> Result<Vec<BrowseChoice>> {
    let mutations = data
        .pointer("/frameworkUpdates/entityBatchUpdate/mutations")
        .and_then(Value::as_array)
        .context("YouTube Music did not return chart country state")?;
    let entities = |kind: &str| -> HashMap<&str, &Value> {
        mutations
            .iter()
            .filter_map(|mutation| {
                let entity = mutation["payload"].get(kind)?;
                Some((entity["id"].as_str()?, entity))
            })
            .collect()
    };
    let forms = entities("musicForm");
    let states = entities("musicFormBooleanChoice");
    let sections = data.pointer("/contents/singleColumnBrowseResultsRenderer/tabs/0/tabRenderer/content/sectionListRenderer/contents").and_then(Value::as_array)
        .context("YouTube Music did not return chart country choices")?;
    let options = sections
        .iter()
        .filter_map(|section| {
            section
                .pointer("/musicShelfRenderer/subheaders")
                .and_then(Value::as_array)
        })
        .flatten()
        .filter_map(|header| {
            header
                .pointer("/musicSideAlignedItemRenderer/startItems")
                .and_then(Value::as_array)
        })
        .flatten()
        .find_map(|item| {
            item.pointer("/musicSortFilterButtonRenderer/menu/musicMultiSelectMenuRenderer/options")
                .and_then(Value::as_array)
        })
        .context("YouTube Music did not return chart country choices")?;
    let mut choices = Vec::new();
    let mut countries = HashSet::new();
    for option in options
        .iter()
        .filter_map(|option| option.get("musicMultiSelectMenuItemRenderer"))
    {
        let parsed = (|| {
            let key = option["formItemEntityKey"].as_str()?;
            let state = states.get(key)?;
            let parent = state["parentFormEntityKey"].as_str()?;
            let form = forms.get(parent)?;
            if !form["booleanChoiceEntityKeys"]
                .as_array()?
                .iter()
                .any(|value| value.as_str() == Some(key))
            {
                return None;
            }
            let country = state["opaqueToken"].as_str()?;
            if !valid_chart_country(country) {
                return None;
            }
            let selected = state["selected"].as_bool()?;
            let label = text(&option["title"]);
            if label.trim().is_empty() {
                return None;
            }
            let mut target = match option.get("selectedCommand") {
                Some(command) => {
                    let commands = command
                        .pointer("/commandExecutorCommand/commands")?
                        .as_array()?;
                    let mut checked = commands
                        .iter()
                        .filter_map(|command| command.get("musicCheckboxFormItemMutatedCommand"))
                        .filter(|command| command["newCheckedState"] == true);
                    if checked.next()?["formItemEntityKey"].as_str() != Some(key)
                        || checked.next().is_some()
                    {
                        return None;
                    }
                    let mut binders = commands
                        .iter()
                        .filter_map(|command| command.get("musicBrowseFormBinderCommand"));
                    let binder = binders.next()?;
                    if binders.next().is_some() || binder["formEntityKey"].as_str() != Some(parent)
                    {
                        return None;
                    }
                    BrowseLink::from_endpoint(&binder["browseEndpoint"], "Charts".into())?
                }
                None if selected => BrowseLink {
                    title: "Charts".into(),
                    ..current.clone()
                },
                None => return None,
            };
            if target.id != "FEmusic_charts" {
                return None;
            }
            target.chart_country = Some(country.to_owned());
            target.validate().ok()?;
            Some(BrowseChoice {
                label,
                selected,
                target,
            })
        })();
        if let Some(choice) = parsed
            && countries.insert(choice.target.chart_country.clone())
        {
            choices.push(choice);
        }
    }
    let selected: Vec<_> = choices.iter().filter(|choice| choice.selected).collect();
    ensure!(
        selected.len() == 1,
        "YouTube Music did not identify one selected chart country"
    );
    ensure!(
        current
            .chart_country
            .as_ref()
            .is_none_or(|country| selected[0].target.chart_country.as_ref() == Some(country)),
        "YouTube Music did not return the requested chart country"
    );
    Ok(choices)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn continued_page(visitor: Value, token: &str) -> Value {
        json!({
            "responseContext":{"visitorData":visitor},
            "continuationContents":{"sectionListContinuation":{
                "contents":[{"musicCarouselShelfRenderer":{
                    "header":{"musicCarouselShelfBasicHeaderRenderer":{"title":{"simpleText":"More"}}},
                    "contents":[{"musicTwoRowItemRenderer":{
                        "title":{"simpleText":"A collection"},
                        "navigationEndpoint":{"browseEndpoint":{"browseId":"VLcollection"}}
                    }}],
                    "continuations":[{"nextContinuationData":{"continuation":"shelf-token"}}]
                }}],
                "continuations":[{"nextContinuationData":{"continuation":token}}]
            }}
        })
    }

    #[test]
    fn continuation_keeps_its_page_token_and_originating_visitor() {
        let catalog = Catalog::new().unwrap();
        let response = continued_page(json!("original-visitor"), "page-token");
        let page = catalog
            .bind_page(parse_page(&response, "More").unwrap(), &response, None)
            .unwrap();
        assert_eq!(page.shelves.len(), 1);
        assert_eq!(page.shelves[0].items.len(), 1);
        let first = page.continuation.unwrap();
        assert_eq!(first.token(), "page-token");
        let response = continued_page(json!("later-visitor"), "next-page-token");
        let next = catalog
            .bind_page(
                parse_page(&response, "More").unwrap(),
                &response,
                Some(&first),
            )
            .unwrap()
            .continuation
            .unwrap();
        let request = catalog
            .page_request("browse", json!({"continuation":next.token()}), Some(&next))
            .unwrap()
            .build()
            .unwrap();
        let visitor = &request.headers()["X-Goog-Visitor-Id"];
        assert_eq!(visitor, "original-visitor");
        assert!(visitor.is_sensitive());
        let body: Value =
            serde_json::from_slice(request.body().unwrap().as_bytes().unwrap()).unwrap();
        assert_eq!(body["continuation"], "next-page-token");
        assert_eq!(request.url().host_str(), Some("music.youtube.com"));
        assert_eq!(request.url().path(), "/youtubei/v1/browse");
        let debug = format!("{next:?}");
        assert!(!debug.contains("visitor") && !debug.contains("token"));
    }

    #[test]
    fn a_finished_section_list_does_not_use_a_nested_shelf_continuation() {
        let mut response = continued_page(json!("visitor"), "page-token");
        response["continuationContents"]["sectionListContinuation"]
            .as_object_mut()
            .unwrap()
            .remove("continuations");
        assert!(
            parse_page(&response, "More")
                .unwrap()
                .continuation
                .is_none()
        );
        assert!(
            section_continuation(&response["continuationContents"]["sectionListContinuation"])
                .is_none()
        );
        let shelf = json!({"continuationContents":{"musicShelfContinuation":{
            "contents":[],"continuations":[{"nextContinuationData":{"continuation":"library-token"}}]
        }}});
        assert_eq!(
            parse_page(&shelf, "Library")
                .unwrap()
                .continuation
                .as_ref()
                .map(Continuation::token),
            Some("library-token")
        );
    }

    #[test]
    fn continuation_requires_the_exact_account_snapshot_before_preparing_a_request() {
        let session =
            || {
                Arc::new(serde_json::from_value::<Session>(json!({
                "cookie":"SAPISID=synthetic", "playback_cookie":"# Netscape HTTP Cookie File\n",
                "authuser":"2", "delegated_session_id":null, "data_sync_id":"user||",
                "client_version":"test"
            })).unwrap())
            };
        let expected = session();
        let catalog = Catalog::with_session(Some(expected.clone())).unwrap();
        let response = continued_page(json!("visitor"), "token");
        let continuation = catalog
            .bind_page(parse_page(&response, "More").unwrap(), &response, None)
            .unwrap()
            .continuation
            .unwrap();
        assert!(
            catalog
                .page_request("browse", json!({}), Some(&continuation))
                .is_ok()
        );
        for current in [None, Some(session())] {
            let other = catalog.for_session(current);
            assert!(
                other
                    .page_request("browse", json!({}), Some(&continuation))
                    .err()
                    .unwrap()
                    .to_string()
                    .contains("account changed")
            );
        }
        let guest = Catalog::new().unwrap();
        let guest_context = guest
            .bind_page(parse_page(&response, "More").unwrap(), &response, None)
            .unwrap()
            .continuation
            .unwrap();
        assert!(
            catalog
                .page_request("browse", json!({}), Some(&guest_context))
                .is_err()
        );
    }

    #[test]
    fn invalid_page_visitors_fail_without_disclosing_the_value() {
        let catalog = Catalog::new().unwrap();
        for visitor in [
            json!(null),
            json!(42),
            json!("private-visitor\r\ninjected:value"),
        ] {
            let response = continued_page(visitor, "token");
            let error = catalog
                .bind_page(parse_page(&response, "More").unwrap(), &response, None)
                .unwrap_err();
            let message = format!("{error:#}");
            assert!(message.contains("invalid page visitor"));
            assert!(!message.contains("private-visitor") && !message.contains("injected"));
        }
    }

    fn home_filter_fixture(selected: bool) -> Value {
        let chip = |label, selected, params| {
            json!({"chipCloudChipRenderer":{
                "text":{"simpleText":label},"isSelected":selected,
                "navigationEndpoint":{"browseEndpoint":{"browseId":"FEmusic_home","params":params}},
                "onDeselectedCommand":{"browseEndpoint":{"browseId":"FEmusic_home","params":"all-filter"}}
            }})
        };
        json!({"contents":{"singleColumnBrowseResultsRenderer":{"tabs":[{"tabRenderer":{"content":{
            "sectionListRenderer":{"contents":[],"header":{"chipCloudRenderer":{"chips":[
                chip("Relax", selected, "relax-filter"), chip("Focus", false, "focus-filter")
            ]}}}
        }}}]}}})
    }

    fn chart_filter_fixture() -> Value {
        let option = |label, key, selected: bool| {
            let mut option = json!({"title":{"simpleText":label},"formItemEntityKey":key});
            if !selected {
                option["selectedCommand"] = json!({"commandExecutorCommand":{"commands":[
                    {"musicCheckboxFormItemMutatedCommand":{"formItemEntityKey":key,"newCheckedState":true}},
                    {"musicCheckboxFormItemMutatedCommand":{"formItemEntityKey":"us-choice","newCheckedState":false}},
                    {"musicBrowseFormBinderCommand":{"formEntityKey":"country-form","browseEndpoint":{
                        "browseId":"FEmusic_charts","params":"issued-chart-params"
                    }}}
                ]}});
            }
            json!({"musicMultiSelectMenuItemRenderer":option})
        };
        let state = |key, token, selected| {
            json!({"payload":{"musicFormBooleanChoice":{
                "id":key,"parentFormEntityKey":"country-form","opaqueToken":token,"selected":selected
            }}})
        };
        json!({
            "contents":{"singleColumnBrowseResultsRenderer":{"tabs":[{"tabRenderer":{"content":{
                "sectionListRenderer":{"contents":[{"musicShelfRenderer":{"subheaders":[{
                    "musicSideAlignedItemRenderer":{"startItems":[{"musicSortFilterButtonRenderer":{
                        "title":{"simpleText":"Misleading display label"},"menu":{"musicMultiSelectMenuRenderer":{"options":[
                            option("United States","us-choice",true), option("Canada","ca-choice",false),
                            option("Global","global-choice",false), option("United States","us-choice",true)
                        ]}}
                    }}]}
                }]}}]}
            }}}]}},
            "frameworkUpdates":{"entityBatchUpdate":{"mutations":[
                {"payload":{"musicForm":{"id":"country-form","booleanChoiceEntityKeys":["us-choice","ca-choice","global-choice"]}}},
                state("us-choice","US",true), state("ca-choice","CA",false), state("global-choice","ZZ",false)
            ]}}
        })
    }

    #[test]
    fn home_filters_use_service_selection_and_reset_actions() {
        let response = home_filter_fixture(true);
        let choices = home_choices(&response).unwrap();
        assert_eq!(
            choices
                .iter()
                .map(|choice| choice.label.as_str())
                .collect::<Vec<_>>(),
            ["All", "Relax", "Focus"]
        );
        assert!(!choices[0].selected);
        assert!(choices[1].selected);
        assert_eq!(choices[0].target.params.as_deref(), Some("all-filter"));
        assert_eq!(choices[1].target, choices[0].target);
        assert_eq!(choices[2].target.params.as_deref(), Some("focus-filter"));
        assert!(choices.iter().all(|choice| choice.target.title == "Home"));
        let reset = home_choices(&home_filter_fixture(false)).unwrap();
        assert!(reset[0].selected);
        assert!(!reset[1].selected);
        assert_eq!(reset[1].target.params.as_deref(), Some("relax-filter"));
        let parsed = parse_page(&response, "Unscoped parser").unwrap();
        assert!(parsed.chips.is_empty() && parsed.countries.is_empty());

        let chips = "/contents/singleColumnBrowseResultsRenderer/tabs/0/tabRenderer/content/sectionListRenderer/header/chipCloudRenderer/chips";
        for (suffix, value) in [
            ("/0/chipCloudChipRenderer/onDeselectedCommand", Value::Null),
            (
                "/1/chipCloudChipRenderer/onDeselectedCommand/browseEndpoint/params",
                json!("conflicting-reset"),
            ),
            (
                "/1/chipCloudChipRenderer/navigationEndpoint/browseEndpoint/browseId",
                json!("FEmusic_radio_builder"),
            ),
            ("/1/chipCloudChipRenderer/isSelected", json!(true)),
        ] {
            let mut invalid = response.clone();
            *invalid.pointer_mut(&format!("{chips}{suffix}")).unwrap() = value;
            assert!(home_choices(&invalid).is_err());
        }
    }

    #[test]
    fn chart_choices_bind_entities_commands_and_current_request_context() {
        let current = BrowseLink {
            params: Some("current-chart-params".into()),
            ..BrowseLink::new("FEmusic_charts", "Charts")
        };
        let response = chart_filter_fixture();
        let choices = chart_choices(&response, &current).unwrap();
        assert_eq!(choices.len(), 3);
        assert!(choices[0].selected);
        assert_eq!(choices[0].label, "United States");
        assert_eq!(choices[0].target.chart_country.as_deref(), Some("US"));
        assert_eq!(choices[0].target.params, current.params);
        let canada = &choices[1].target;
        assert_eq!(canada.title, "Charts");
        assert_eq!(
            canada.body().unwrap(),
            json!({"browseId":"FEmusic_charts","params":"issued-chart-params","formData":{"selectedValues":["CA"]}})
        );
        assert_ne!(canada, &choices[2].target);
        assert!(chart_choices(&response, canada).is_err());

        let option = "/contents/singleColumnBrowseResultsRenderer/tabs/0/tabRenderer/content/sectionListRenderer/contents/0/musicShelfRenderer/subheaders/0/musicSideAlignedItemRenderer/startItems/0/musicSortFilterButtonRenderer/menu/musicMultiSelectMenuRenderer/options";
        for (path, value) in [
            (format!("{option}/1/musicMultiSelectMenuItemRenderer/selectedCommand/commandExecutorCommand/commands/0/musicCheckboxFormItemMutatedCommand/formItemEntityKey"), json!("global-choice")),
            (format!("{option}/1/musicMultiSelectMenuItemRenderer/selectedCommand/commandExecutorCommand/commands/2/musicBrowseFormBinderCommand/formEntityKey"), json!("other-form")),
            (format!("{option}/1/musicMultiSelectMenuItemRenderer/selectedCommand/commandExecutorCommand/commands/2/musicBrowseFormBinderCommand/browseEndpoint/browseId"), json!("FEmusic_radio_builder")),
            ("/frameworkUpdates/entityBatchUpdate/mutations/2/payload/musicFormBooleanChoice/parentFormEntityKey".into(), json!("other-form")),
            ("/frameworkUpdates/entityBatchUpdate/mutations/2/payload/musicFormBooleanChoice/opaqueToken".into(), json!("../")),
        ] {
            let mut invalid = response.clone();
            *invalid.pointer_mut(&path).unwrap() = value;
            let choices = chart_choices(&invalid, &current).unwrap();
            assert!(choices.iter().all(|choice| choice.label != "Canada"));
        }
        let mut missing_action = response.clone();
        missing_action
            .pointer_mut(&format!("{option}/1/musicMultiSelectMenuItemRenderer"))
            .unwrap()
            .as_object_mut()
            .unwrap()
            .remove("selectedCommand");
        assert!(
            chart_choices(&missing_action, &current)
                .unwrap()
                .iter()
                .all(|choice| choice.label != "Canada")
        );
        let mut malformed_selected = response;
        for index in [0, 3] {
            malformed_selected
                .pointer_mut(&format!(
                    "{option}/{index}/musicMultiSelectMenuItemRenderer"
                ))
                .unwrap()["selectedCommand"] = Value::Null;
        }
        assert!(chart_choices(&malformed_selected, &current).is_err());
    }

    #[test]
    fn chart_form_values_are_narrowly_typed() {
        for id in ["FEmusic_home", "FEmusic_radio_builder"] {
            let target = BrowseLink {
                chart_country: Some("CA".into()),
                ..BrowseLink::new(id, "Not charts")
            };
            assert!(target.body().is_err());
        }
        for country in ["", "ca", "CAN", "C1", "../"] {
            let target = BrowseLink {
                chart_country: Some(country.into()),
                ..BrowseLink::new("FEmusic_charts", "Charts")
            };
            assert!(target.body().is_err());
        }
    }

    fn navigation(title: &str, params: &str) -> Value {
        json!({"musicNavigationButtonRenderer":{
            "buttonText":{"runs":[{"text":title}]},
            "clickCommand":{"browseEndpoint":{
                "browseId":"FEmusic_moods_and_genres_category", "params":params
            }}
        }})
    }

    #[test]
    fn navigation_grids_preserve_parameter_identity_and_server_order() {
        let data = json!({"contents":{"sectionListRenderer":{"contents":[{
            "gridRenderer":{
                "header":{"gridHeaderRenderer":{"title":{"simpleText":"Moods & moments"}}},
                "items":[navigation("Chill", "first%3D%3D"), navigation("Commute", "second_-/+=")]
            }
        }]}}});
        let page = parse_page(&data, "Moods & genres").unwrap();
        assert_eq!(page.title, "Moods & genres");
        assert_eq!(page.shelves.len(), 1);
        assert_eq!(page.shelves[0].title, "Moods & moments");
        let [Item::Navigation(first), Item::Navigation(second)] = page.shelves[0].items.as_slice()
        else {
            panic!("expected two navigation links")
        };
        assert_eq!(first.id, second.id);
        assert_eq!(first.title, "Chill");
        assert_eq!(second.title, "Commute");
        assert_eq!(first.params.as_deref(), Some("first%3D%3D"));
        assert_eq!(second.params.as_deref(), Some("second_-/+="));
        let different_params = BrowseLink {
            title: first.title.clone(),
            ..second.clone()
        };
        assert_ne!(first, &different_params);
        assert!(page.tracks().is_empty());
        assert_eq!(page.shelves[0].items[0].subtitle(), "");
        assert_eq!(page.shelves[0].items[0].thumbnail(), None);
    }

    #[test]
    fn malformed_navigation_is_not_actionable() {
        let valid = navigation("Chill", "filter%3D");
        for (path, value) in [
            ("/musicNavigationButtonRenderer/buttonText", Value::Null),
            (
                "/musicNavigationButtonRenderer/buttonText",
                json!({"simpleText":"  "}),
            ),
            ("/musicNavigationButtonRenderer/clickCommand", Value::Null),
            (
                "/musicNavigationButtonRenderer/clickCommand/browseEndpoint",
                json!({"url":"https://example.invalid"}),
            ),
            (
                "/musicNavigationButtonRenderer/clickCommand/browseEndpoint/browseId",
                json!("https://example.invalid"),
            ),
            (
                "/musicNavigationButtonRenderer/clickCommand/browseEndpoint/params",
                json!({"filter":"not a string"}),
            ),
            (
                "/musicNavigationButtonRenderer/clickCommand/browseEndpoint/params",
                json!("filter&other=value"),
            ),
            (
                "/musicNavigationButtonRenderer/clickCommand/browseEndpoint/params",
                json!("filter%3"),
            ),
        ] {
            let mut invalid = valid.clone();
            *invalid.pointer_mut(path).unwrap() = value;
            assert!(parse_item(&invalid).is_none());
        }
        let without_params = json!({"musicNavigationButtonRenderer":{
            "buttonText":{"simpleText":"New releases"},
            "clickCommand":{"browseEndpoint":{"browseId":"FEmusic_new_releases"}}
        }});
        assert!(
            matches!(parse_item(&without_params), Some(Item::Navigation(link)) if link.params.is_none())
        );

        let catalog = Catalog::new().unwrap();
        for link in [
            BrowseLink::new("https://example.invalid", "Invalid"),
            BrowseLink::new("A".repeat(257), "Invalid"),
            BrowseLink {
                params: Some("A".repeat(8193)),
                ..BrowseLink::new("FEmusic_home", "Invalid")
            },
            BrowseLink {
                params: Some("filter\nvalue".into()),
                ..BrowseLink::new("FEmusic_home", "Invalid")
            },
        ] {
            assert!(
                catalog
                    .browse(&link)
                    .unwrap_err()
                    .to_string()
                    .contains("invalid")
            );
        }
    }

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
            "menu":{"menuRenderer":{"items":[
                {"menuNavigationItemRenderer":{"navigationEndpoint":{"watchEndpoint":{"videoId":"abcdefghijk","playlistId":"RDwrong"}}}},
                {"toggleMenuServiceItemRenderer":{"defaultServiceEndpoint":{"likeEndpoint":{"status":"INDIFFERENT","target":{"playlistId":"playlist"}}}}}
            ]}}
        }});
        let Some(Item::Collection(collection)) = parse_item(&item) else {
            panic!("expected a collection")
        };
        assert_eq!(collection.id, "VLplaylist");
        assert_eq!(collection.details.playlist_id.as_deref(), Some("playlist"));
        assert_eq!(collection.details.saved, Some(true));

        let mut album = item;
        album["musicTwoRowItemRenderer"]["title"]["runs"][0]["navigationEndpoint"]["browseEndpoint"]
            ["browseId"] = json!("MPREalbum");
        album["musicTwoRowItemRenderer"]["menu"]["menuRenderer"]["items"][1]["toggleMenuServiceItemRenderer"]
            ["defaultServiceEndpoint"]["likeEndpoint"]["target"]["playlistId"] = json!("OLAKalbum");
        assert!(
            matches!(parse_item(&album), Some(Item::Collection(c)) if c.details.playlist_id.as_deref() == Some("OLAKalbum"))
        );
    }

    #[test]
    fn duplicate_playlist_occurrences_keep_their_own_ids() {
        let row = |set_id| {
            json!({"musicResponsiveListItemRenderer":{
                "playlistItemData":{"videoId":"abcdefghijk"},
                "flexColumns":[{"musicResponsiveListItemFlexColumnRenderer":{"text":{"runs":[{"text":"Repeated song"}]}}}],
                "menu":{"menuRenderer":{"items":[{"menuServiceItemRenderer":{
                    "serviceEndpoint":{"playlistEditEndpoint":{"playlistId":"playlist","actions":[{
                        "action":"ACTION_REMOVE_VIDEO","removedVideoId":"abcdefghijk","setVideoId":set_id
                    }]}}
                }}]}}
            }})
        };
        let page = parse_page(
            &json!({"contents":{"musicPlaylistShelfRenderer":{
                "contents":[row("occurrence-1"),row("occurrence-2")]
            }}}),
            "Playlist",
        )
        .unwrap();
        let tracks = page.tracks();
        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].id, tracks[1].id);
        assert_eq!(
            tracks[0].details.set_video_id.as_deref(),
            Some("occurrence-1")
        );
        assert_eq!(
            tracks[1].details.set_video_id.as_deref(),
            Some("occurrence-2")
        );
    }

    #[test]
    fn owned_playlist_metadata_comes_from_its_editable_header() {
        let mut response = json!({"contents":{"twoColumnBrowseResultsRenderer":{
            "tabs":[{"tabRenderer":{"content":{"sectionListRenderer":{"contents":[{
                "musicEditablePlaylistDetailHeaderRenderer":{
                    "playlistId":"owned-playlist",
                    "header":{"musicResponsiveHeaderRenderer":{
                        "title":{"runs":[{"text":"My playlist"}]},
                        "description":{"musicDescriptionShelfRenderer":{"description":{"runs":[{"text":"Display description"}]}}}
                    }},
                    "editHeader":{"musicPlaylistEditHeaderRenderer":{
                        "title":{"runs":[{"text":"My playlist"}]},
                        "description":{"runs":[{"text":"Full editable description"}]},
                        "privacy":"UNLISTED"
                    }}
                }
            }]}}}}],
            "secondaryContents":{"sectionListRenderer":{"contents":[]}}
        }}});
        let page = parse_page(&response, "Playlist").unwrap();
        assert!(page.editable);
        assert_eq!(page.title, "My playlist");
        assert_eq!(page.description, "Full editable description");
        assert_eq!(page.privacy.as_deref(), Some("UNLISTED"));

        let section = &mut response["contents"]["twoColumnBrowseResultsRenderer"]["tabs"][0]["tabRenderer"]
            ["content"]["sectionListRenderer"]["contents"][0];
        *section = json!({"musicResponsiveHeaderRenderer":{
            "title":{"runs":[{"text":"Someone else's playlist"}]},
            "description":{"musicDescriptionShelfRenderer":{"description":{"runs":[{"text":"Public description"}]}}},
            "menu":{"editablePlaylistId":"an-unrelated-playlist"}
        }});
        let page = parse_page(&response, "Playlist").unwrap();
        assert!(!page.editable);
        assert_eq!(page.description, "Public description");
        assert_eq!(page.privacy, None);
    }

    #[test]
    fn library_feedback_is_separate_from_song_rating() {
        let mut row = json!({"menu":{"menuRenderer":{
            "topLevelButtons":[{"likeButtonRenderer":{"likeStatus":"DISLIKE"}}],
            "items":[
                {"toggleMenuServiceItemRenderer":{
                    "defaultIcon":{"iconType":"BOOKMARK_BORDER"},"isToggled":true,
                    "defaultServiceEndpoint":{"feedbackEndpoint":{"feedbackToken":"add-song"}},
                    "toggledServiceEndpoint":{"feedbackEndpoint":{"feedbackToken":"remove-song"}}
                }},
                {"menuServiceItemRenderer":{
                    "icon":{"iconType":"REMOVE_FROM_HISTORY"},
                    "serviceEndpoint":{"feedbackEndpoint":{"feedbackToken":"remove-history"}}
                }}
            ]
        }}});
        let details = track_details(&row, &[]);
        assert_eq!(details.rating, Some(Rating::Dislike));
        assert_eq!(details.in_library, Some(true));
        assert_eq!(details.add_library.as_deref(), Some("add-song"));
        assert_eq!(details.remove_library.as_deref(), Some("remove-song"));
        assert_eq!(details.remove_history.as_deref(), Some("remove-history"));

        let toggle = &mut row["menu"]["menuRenderer"]["items"][0]["toggleMenuServiceItemRenderer"];
        toggle["defaultIcon"]["iconType"] = json!("BOOKMARK");
        toggle["defaultServiceEndpoint"]["feedbackEndpoint"]["feedbackToken"] =
            json!("remove-song");
        toggle["toggledServiceEndpoint"]["feedbackEndpoint"]["feedbackToken"] = json!("add-song");
        let details = track_details(&row, &[]);
        assert_eq!(details.add_library.as_deref(), Some("add-song"));
        assert_eq!(details.remove_library.as_deref(), Some("remove-song"));
    }

    #[test]
    fn toggle_actions_are_not_current_ratings() {
        let mut row = json!({"menu":{"menuRenderer":{"items":[{
            "toggleMenuServiceItemRenderer":{
                "defaultIcon":{"iconType":"FAVORITE"},
                "defaultServiceEndpoint":{"likeEndpoint":{"status":"LIKE","target":{"videoId":"abcdefghijk"}}},
                "toggledServiceEndpoint":{"likeEndpoint":{"status":"INDIFFERENT","target":{"videoId":"abcdefghijk"}}}
            }
        }]}}});
        assert_eq!(track_details(&row, &[]).rating, Some(Rating::Indifferent));
        row["menu"]["menuRenderer"]["items"][0]["toggleMenuServiceItemRenderer"]["isToggled"] =
            json!(true);
        assert_eq!(track_details(&row, &[]).rating, Some(Rating::Like));
        let toggle = &mut row["menu"]["menuRenderer"]["items"][0]["toggleMenuServiceItemRenderer"];
        toggle["isToggled"] = json!(false);
        toggle["defaultServiceEndpoint"]["likeEndpoint"]["status"] = json!("INDIFFERENT");
        assert_eq!(track_details(&row, &[]).rating, Some(Rating::Like));
    }

    #[test]
    fn account_state_is_cleared_on_disk_and_in_memory() {
        let mut details = TrackDetails {
            rating: Some(Rating::Like),
            in_library: Some(true),
            add_library: Some("add-song".into()),
            remove_library: Some("remove-song".into()),
            remove_history: Some("remove-history".into()),
            set_video_id: Some("occurrence-1".into()),
            artist_id: Some("UCartist".into()),
            album_id: Some("MPREalbum".into()),
            credits_id: Some("MPTCabcdefghijk".into()),
        };
        assert_eq!(
            serde_json::to_value(&details).unwrap(),
            json!({
                "artist_id":"UCartist","album_id":"MPREalbum","credits_id":"MPTCabcdefghijk"
            })
        );
        let restored: TrackDetails = serde_json::from_value(json!({
            "rating":"LIKE","in_library":true,"add_library":"stale-add-song",
            "remove_library":"stale-remove-song","remove_history":"stale-history",
            "set_video_id":"stale-occurrence","artist_id":"UCartist","album_id":"MPREalbum","credits_id":"MPTCabcdefghijk"
        }))
        .unwrap();
        assert_eq!(
            restored,
            TrackDetails {
                artist_id: Some("UCartist".into()),
                album_id: Some("MPREalbum".into()),
                credits_id: Some("MPTCabcdefghijk".into()),
                ..Default::default()
            }
        );
        details.clear_account_state();
        assert_eq!(details, restored);
    }

    #[test]
    fn parses_current_search_item_sections() {
        let data = json!({"contents":{"sectionListRenderer":{"contents":[{"itemSectionRenderer":{"contents":[{
            "musicResponsiveListItemRenderer":{"title":{"runs":[{"text":"A song","navigationEndpoint":{"watchEndpoint":{"videoId":"abcdefghijk"}}}]},
                "menu":{"menuRenderer":{"items":[
                    {"menuNavigationItemRenderer":{"navigationEndpoint":{"browseEndpoint":{"browseId":"UCartist"}}}},
                    {"menuNavigationItemRenderer":{"navigationEndpoint":{"browseEndpoint":{"browseId":"MPTCcanonical"}}}}
                ]}}}
        }]}}]}}});
        let mut tracks = parse_page(&data, "Search").unwrap().tracks();
        assert_eq!(tracks.len(), 1);
        assert_eq!(
            tracks[0].details.credits_id.as_deref(),
            Some("MPTCcanonical")
        );
        assert_eq!(tracks[0].credits_id().as_deref(), Some("MPTCcanonical"));
        tracks[0].details.credits_id = None;
        assert_eq!(tracks[0].credits_id().as_deref(), Some("MPTCabcdefghijk"));
    }

    #[test]
    #[ignore = "uses the live YouTube Music service"]
    fn live_catalog() {
        let api = Catalog::new().unwrap();
        let home = api.home().unwrap();
        assert!(!home.shelves.is_empty());
        let results = api
            .search_filtered(
                "Khruangbin",
                crate::search::SearchFilter::All,
                crate::search::SearchScope::Catalog,
            )
            .unwrap();
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
        let page = api
            .browse(&BrowseLink::new(
                collection.id.clone(),
                collection.title.clone(),
            ))
            .unwrap();
        assert!(!page.tracks().is_empty());
        println!(
            "Home: {} shelves; search: {} tracks; {}: {} tracks",
            home.shelves.len(),
            results.tracks().len(),
            page.title,
            page.tracks().len()
        );
    }

    #[test]
    #[ignore = "uses public YouTube Music discovery pages without an account"]
    fn live_public_discovery_navigation() {
        let catalog = Catalog::new().unwrap();
        let explore = catalog.explore().unwrap();
        let links: Vec<_> = explore
            .shelves
            .iter()
            .flat_map(|shelf| &shelf.items)
            .filter_map(|item| {
                if let Item::Navigation(link) = item {
                    Some(link)
                } else {
                    None
                }
            })
            .collect();
        for id in [
            "FEmusic_new_releases",
            "FEmusic_charts",
            "FEmusic_moods_and_genres",
        ] {
            let link = links
                .iter()
                .find(|link| link.id == id)
                .expect("Explore must expose the destination");
            let page = catalog.browse(link).unwrap();
            if id != "FEmusic_moods_and_genres" {
                assert!(
                    page.shelves
                        .iter()
                        .flat_map(|shelf| &shelf.items)
                        .any(|item| matches!(item, Item::Track(_) | Item::Collection(_)))
                );
                println!("{}: {} shelves", link.title, page.shelves.len());
                continue;
            }
            let category = page
                .shelves
                .iter()
                .flat_map(|shelf| &shelf.items)
                .find_map(|item| {
                    if let Item::Navigation(link) = item {
                        (link.id == "FEmusic_moods_and_genres_category" && link.params.is_some())
                            .then_some(link)
                    } else {
                        None
                    }
                })
                .expect("Moods must expose a category with its own parameters");
            let playlists = catalog.browse(category).unwrap();
            assert!(
                playlists
                    .shelves
                    .iter()
                    .flat_map(|shelf| &shelf.items)
                    .any(|item| matches!(item, Item::Collection(_)))
            );
            println!(
                "{}: {} category shelves; {}: {} playlist shelves",
                link.title,
                page.shelves.len(),
                category.title,
                playlists.shelves.len()
            );
        }
    }

    #[test]
    #[ignore = "uses two public Home continuation reads without an account"]
    fn live_public_home_continuation() {
        let catalog = Catalog::new().unwrap();
        let mut page = catalog.home().unwrap();
        assert!(!page.shelves.is_empty());
        for _ in 0..2 {
            page = catalog
                .more(page.continuation.as_ref().expect("Home must paginate"))
                .unwrap();
            assert!(
                !page.shelves.is_empty(),
                "Home continuation must contain shelves"
            );
            assert!(page.shelves.iter().all(|shelf| !shelf.items.is_empty()));
        }
    }

    #[test]
    #[ignore = "uses public Home filter and Charts country reads without an account"]
    fn live_public_browse_filters() {
        let catalog = Catalog::new().unwrap();
        let home = catalog.home().unwrap();
        assert!(
            home.chips
                .iter()
                .any(|choice| choice.label == "All" && choice.selected)
        );
        let relax = &home
            .chips
            .iter()
            .find(|choice| choice.label == "Relax")
            .expect("Home must expose Relax")
            .target;
        let filtered = catalog.browse(relax).unwrap();
        assert!(
            filtered
                .chips
                .iter()
                .any(|choice| choice.label == "Relax" && choice.selected)
        );
        assert_eq!(filtered.chips.len(), home.chips.len());
        let deselect = &filtered
            .chips
            .iter()
            .find(|choice| choice.label == "Relax" && choice.selected)
            .unwrap()
            .target;
        let reset = catalog.browse(deselect).unwrap();
        assert!(
            reset
                .chips
                .iter()
                .any(|choice| choice.label == "All" && choice.selected)
        );
        assert!(
            reset
                .chips
                .iter()
                .filter(|choice| choice.label != "All")
                .all(|choice| !choice.selected)
        );
        assert_eq!(reset.title, "Home");

        let explore = catalog.explore().unwrap();
        let charts = explore
            .shelves
            .iter()
            .flat_map(|shelf| &shelf.items)
            .find_map(|item| {
                if let Item::Navigation(link) = item {
                    (link.id == "FEmusic_charts").then_some(link)
                } else {
                    None
                }
            })
            .expect("Explore must expose Charts");
        let countries = catalog.browse(charts).unwrap();
        for label in ["Canada", "United Kingdom", "Global"] {
            let target = &countries
                .countries
                .iter()
                .find(|choice| choice.label == label)
                .expect("Charts must expose the country")
                .target;
            let selected = catalog.browse(target).unwrap();
            assert_eq!(selected.title, "Charts");
            assert_eq!(selected.countries.len(), countries.countries.len());
            assert!(selected.countries.iter().any(
                |choice| choice.selected && choice.target.chart_country == target.chart_country
            ));
            assert!(!selected.shelves.is_empty());
            println!(
                "{label}: service-confirmed selection, {} country choices",
                selected.countries.len()
            );
        }
        println!(
            "Home: {} chips; select and reset confirmed",
            home.chips.len()
        );
    }
}
