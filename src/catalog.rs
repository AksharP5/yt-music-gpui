use crate::auth::{self, Session};
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

impl Account {
    fn from_response(data: &Value) -> Result<Self> {
        let header = find(data, "activeAccountHeaderRenderer")
            .context("Google did not accept the account session. Connect again.")?;
        let name = text(&header["accountName"]);
        ensure!(!name.is_empty(), "Google returned no signed-in account");
        Ok(Self {
            name,
            handle: text(&header["channelHandle"]),
        })
    }
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
        if self.details.episode.is_some() {
            return None;
        }
        self.details
            .credits_id
            .clone()
            .or_else(|| valid_video_id(&self.id).then(|| format!("MPTC{}", self.id)))
    }
}

#[derive(Clone, PartialEq)]
pub struct WatchTarget {
    video_id: Option<String>,
    playlist_id: Option<String>,
    params: Option<String>,
    index: Option<u32>,
    music_video_type: Option<String>,
}

impl WatchTarget {
    pub fn is_single_video(&self) -> bool {
        self.playlist_id.is_none() && self.params.is_none() && self.index.is_none()
    }

    fn from_row(row: &Value, video_id: &str) -> Option<Self> {
        let endpoints = [
            row.pointer("/flexColumns/0/musicResponsiveListItemFlexColumnRenderer/text/runs/0/navigationEndpoint/watchEndpoint"),
            row.pointer("/title/runs/0/navigationEndpoint/watchEndpoint"),
            row.pointer("/navigationEndpoint/watchEndpoint"),
            row.pointer("/onTap/watchEndpoint"),
            row.pointer("/overlay/musicItemThumbnailOverlayRenderer/content/musicPlayButtonRenderer/playNavigationEndpoint/watchEndpoint"),
            row.pointer("/thumbnailOverlay/musicItemThumbnailOverlayRenderer/content/musicPlayButtonRenderer/playNavigationEndpoint/watchEndpoint"),
        ];
        if endpoints
            .iter()
            .flatten()
            .any(|endpoint| endpoint["videoId"].as_str() != Some(video_id))
        {
            return None;
        }
        let endpoint = endpoints.into_iter().flatten().next()?;
        Self::from_endpoint(endpoint)
    }

    fn from_endpoint(endpoint: &Value) -> Option<Self> {
        let video_id = match endpoint.get("videoId") {
            Some(value) => Some(value.as_str().filter(|id| valid_video_id(id))?.to_owned()),
            None => None,
        };
        let playlist_id = match endpoint.get("playlistId") {
            Some(value) => Some(value.as_str()?.to_owned()),
            None => None,
        };
        if playlist_id.as_ref().is_some_and(|id| {
            id.is_empty()
                || id.len() > 256
                || !id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
        }) {
            return None;
        }
        if video_id.is_none() && playlist_id.is_none() {
            return None;
        }
        let params = match endpoint.get("params") {
            Some(value) => Some(value.as_str()?.to_owned()),
            None => None,
        };
        if params
            .as_ref()
            .is_some_and(|params| params.is_empty() || !valid_service_params(params))
        {
            return None;
        }
        let index = match endpoint.get("index") {
            Some(value) => Some(u32::try_from(value.as_u64()?).ok()?),
            None => None,
        };
        let music_video_type = match endpoint
            .pointer("/watchEndpointMusicSupportedConfigs/watchEndpointMusicConfig/musicVideoType")
        {
            Some(value) => Some(value.as_str()?.to_owned()),
            None => None,
        };
        if music_video_type.as_ref().is_some_and(|kind| {
            !kind.starts_with("MUSIC_VIDEO_TYPE_")
                || kind.len() <= "MUSIC_VIDEO_TYPE_".len()
                || kind.len() > 80
                || !kind
                    .bytes()
                    .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
        }) {
            return None;
        }
        Some(Self {
            video_id,
            playlist_id,
            params,
            index,
            music_video_type,
        })
    }

    pub(crate) fn request(&self, video_id: &str) -> Result<Value> {
        ensure!(
            self.video_id.as_deref() == Some(video_id),
            "The playback destination belongs to a different song. Refresh and try again."
        );
        Ok(self.body())
    }

    fn body(&self) -> Value {
        let mut body = json!({});
        if let Some(id) = &self.video_id {
            body["videoId"] = json!(id);
        }
        if let Some(id) = &self.playlist_id {
            body["playlistId"] = json!(id);
        }
        if let Some(params) = &self.params {
            body["params"] = json!(params);
        }
        if let Some(index) = self.index {
            body["index"] = json!(index);
        }
        if let Some(kind) = &self.music_video_type {
            body["watchEndpointMusicSupportedConfigs"] =
                json!({"watchEndpointMusicConfig":{"musicVideoType":kind}});
        }
        body
    }
}

impl fmt::Debug for WatchTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("WatchTarget { .. }")
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct PlaylistTarget(WatchTarget);

impl PlaylistTarget {
    fn from_navigation(navigation: &Value) -> Option<Self> {
        let endpoint = match (
            navigation.get("watchPlaylistEndpoint"),
            navigation.get("watchEndpoint"),
        ) {
            (Some(endpoint), None) | (None, Some(endpoint)) => endpoint,
            _ => return None,
        };
        let target = WatchTarget::from_endpoint(endpoint)?;
        target.playlist_id.as_ref()?;
        Some(Self(target))
    }

    pub(crate) fn request(&self) -> Value {
        self.0.body()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Chapter {
    pub title: String,
    pub start_seconds: u32,
}

#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EpisodeContext {
    pub show_id: Option<String>,
    pub description: String,
    pub subtitle: String,
    pub duration_text: String,
    pub chapters: Vec<Chapter>,
    #[serde(skip)]
    pub(crate) watch_params: Option<String>,
}

impl fmt::Debug for EpisodeContext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EpisodeContext")
            .field("show_id", &self.show_id)
            .field("description", &self.description)
            .field("subtitle", &self.subtitle)
            .field("duration_text", &self.duration_text)
            .field("chapters", &self.chapters)
            .finish_non_exhaustive()
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
    pub plays: Option<String>,
    #[serde(skip)]
    pub watch: Option<Arc<WatchTarget>>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub episode: Option<Box<EpisodeContext>>,
}

impl TrackDetails {
    pub fn clear_account_state(&mut self) {
        self.watch = None;
        self.rating = None;
        self.in_library = None;
        self.add_library = None;
        self.remove_library = None;
        self.remove_history = None;
        self.set_video_id = None;
        if let Some(episode) = &mut self.episode {
            episode.watch_params = None;
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CollectionDetails {
    #[serde(skip)]
    pub play: Option<Arc<PlaylistTarget>>,
    pub playlist_id: Option<String>,
    pub saved: Option<bool>,
    #[serde(skip)]
    pub editable: bool,
}

pub(crate) struct ArtistSubscription {
    pub channel_id: String,
    pub subscribed: bool,
}

impl ArtistSubscription {
    pub(crate) fn from_response(data: &Value) -> Result<Self> {
        let button = data
            .pointer(
                "/header/musicImmersiveHeaderRenderer/subscriptionButton/subscribeButtonRenderer",
            )
            .context("YouTube did not provide this artist's subscription")?;
        Self::from_button(button)
    }

    fn from_button(button: &Value) -> Result<Self> {
        let channel_id = button["channelId"]
            .as_str()
            .filter(|id| {
                id.starts_with("UC")
                    && id.len() == 24
                    && id
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
            })
            .context("YouTube returned an invalid artist subscription channel")?;
        let subscribed = button["subscribed"]
            .as_bool()
            .context("YouTube did not provide this artist's subscription state")?;
        Ok(Self {
            channel_id: channel_id.to_owned(),
            subscribed,
        })
    }
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
        if let Some(video_id) = self.id.strip_prefix("MPED") {
            ensure!(
                valid_video_id(video_id),
                "This episode has an invalid video ID"
            );
        }
        if let Some(params) = &self.params {
            ensure!(
                valid_service_params(params),
                "YouTube Music returned an invalid page link"
            );
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
#[expect(
    clippy::large_enum_variant,
    reason = "Song lists dominate; keep tracks inline to avoid an allocation per row."
)]
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
    pub layout: ShelfLayout,
    pub more: Option<BrowseLink>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ShelfLayout {
    #[default]
    Cards,
    Videos,
    Songs,
    Featured,
}

#[derive(Clone, Debug)]
pub enum HeaderArtist {
    Link(BrowseLink),
    Text(String),
}

#[derive(Clone, Debug)]
pub struct PageHeader {
    pub thumbnail: Option<String>,
    pub artist: Option<HeaderArtist>,
    pub subtitle: String,
    pub stats: String,
    pub shuffle: Option<Arc<PlaylistTarget>>,
    pub mix: Option<Arc<PlaylistTarget>>,
    pub subscribed: Option<bool>,
    pub subscription_label: String,
    pub subscriber_count: String,
}

#[derive(Clone)]
pub struct Continuation {
    token: String,
    visitor: Option<HeaderValue>,
    session: Option<Arc<Session>>,
    show: Option<(String, String)>,
}

impl Continuation {
    fn new(token: &str) -> Self {
        Self {
            token: token.to_owned(),
            visitor: None,
            session: None,
            show: None,
        }
    }

    pub(crate) fn token(&self) -> &str {
        &self.token
    }

    pub(crate) fn renew_session(
        &mut self,
        expected: &Arc<Session>,
        next: &Arc<Session>,
    ) -> Result<()> {
        self.require_session(Some(expected))?;
        expected.require_same_account(next)?;
        self.session = Some(next.clone());
        Ok(())
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
    pub header: Option<PageHeader>,
    pub shelves: Vec<Shelf>,
    pub continuation: Option<Continuation>,
    pub editable: bool,
    pub description: String,
    pub privacy: Option<String>,
    pub chips: Vec<BrowseChoice>,
    pub countries: Vec<BrowseChoice>,
}

impl Page {
    pub fn track_index(&self, item_index: usize) -> Option<usize> {
        let mut items = self.shelves.iter().flat_map(|shelf| &shelf.items);
        let preceding = items
            .by_ref()
            .take(item_index)
            .filter(|item| matches!(item, Item::Track(_)))
            .count();
        matches!(items.next(), Some(Item::Track(_))).then_some(preceding)
    }

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
        let session = self
            .session
            .as_ref()
            .context("Connect your Google account first")?;
        let data = self.request("account/account_menu", json!({}))?;
        session.verify_account_identity(&data, &self.client)?;
        Account::from_response(&data)
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
                .and_then(owned_continuation);
        }
        match link.id.as_str() {
            "FEmusic_home" => page.chips = home_choices(&data)?,
            "FEmusic_charts" => page.countries = chart_choices(&data, link)?,
            _ => {}
        }
        apply_episode_page(&mut page, &data, &link.id)?;
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
        if let Some((id, title)) = previous.and_then(|continuation| continuation.show.as_ref()) {
            associate_show(&mut page, id, title);
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
        if let Some(session) = &self.session {
            session.check_response_identity(&value)?;
            if matches!(
                value.pointer("/error/code").and_then(Value::as_u64),
                Some(401 | 403)
            ) {
                return Err(auth::RejectedSession.into());
            }
        }
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
        let request = if endpoint == "music/get_search_suggestions" {
            request.timeout(Duration::from_secs(4))
        } else {
            request
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

pub(crate) fn valid_service_params(params: &str) -> bool {
    if params.len() > 8192 {
        return false;
    }
    let mut bytes = params.bytes();
    while let Some(byte) = bytes.next() {
        let valid = if byte == b'%' {
            bytes.next().is_some_and(|byte| byte.is_ascii_hexdigit())
                && bytes.next().is_some_and(|byte| byte.is_ascii_hexdigit())
        } else {
            byte.is_ascii_alphanumeric() || b"_-+=/.~".contains(&byte)
        };
        if !valid {
            return false;
        }
    }
    true
}

pub(crate) const PODCAST_EPISODE: &str = "MUSIC_VIDEO_TYPE_PODCAST_EPISODE";

fn valid_show_id(id: &str) -> bool {
    id.starts_with("MPSP")
        && id.len() > 4
        && id.len() <= 256
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
}

pub(crate) fn episode_context(
    row: &Value,
    video_id: &str,
    runs: &[&Value],
) -> Result<Option<Box<EpisodeContext>>> {
    let endpoint = [
        row.pointer("/onTap/watchEndpoint"),
        row.pointer("/navigationEndpoint/watchEndpoint"),
        row.pointer("/overlay/musicItemThumbnailOverlayRenderer/content/musicPlayButtonRenderer/playNavigationEndpoint/watchEndpoint"),
    ]
    .into_iter()
    .flatten()
    .chain(
        row["buttons"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|button| button.pointer("/musicPlayButtonRenderer/playNavigationEndpoint/watchEndpoint")),
    )
    .find(|endpoint| {
        endpoint.pointer("/watchEndpointMusicSupportedConfigs/watchEndpointMusicConfig/musicVideoType")
            .and_then(Value::as_str) == Some(PODCAST_EPISODE)
    });
    let Some(endpoint) = endpoint else {
        return Ok(None);
    };
    ensure!(
        valid_video_id(video_id) && endpoint["videoId"].as_str() == Some(video_id),
        "YouTube Music returned playback details for a different episode"
    );
    let watch_params = endpoint
        .get("params")
        .map(|params| {
            params
                .as_str()
                .filter(|params| !params.is_empty() && valid_service_params(params))
                .map(str::to_owned)
                .context("YouTube Music returned invalid episode playback parameters")
        })
        .transpose()?;
    let show_id = runs.iter().find_map(|run| {
        run.pointer("/navigationEndpoint/browseEndpoint/browseId")
            .and_then(Value::as_str)
            .filter(|id| valid_show_id(id))
            .map(str::to_owned)
    });
    let progress = row
        .pointer("/playbackProgress/musicPlaybackProgressRenderer")
        .or_else(|| row.pointer("/progress/musicPlaybackProgressRenderer"));
    let duration_text = progress
        .map(|progress| text(&progress["durationText"]))
        .filter(|duration| !duration.is_empty())
        .unwrap_or_else(|| text(&row["lengthText"]))
        .trim_start_matches([' ', '•'])
        .to_owned();
    Ok(Some(Box::new(EpisodeContext {
        show_id,
        description: text(&row["description"]),
        subtitle: text(&row["subtitle"]),
        duration_text,
        chapters: Vec::new(),
        watch_params,
    })))
}

pub(crate) fn episode_header(data: &Value, video_id: &str) -> Result<Track> {
    ensure!(
        valid_video_id(video_id),
        "This episode has an invalid video ID"
    );
    let header = data.pointer("/contents/twoColumnBrowseResultsRenderer/tabs/0/tabRenderer/content/sectionListRenderer/contents/0/musicResponsiveHeaderRenderer")
        .context("YouTube Music returned no episode header")?;
    let runs: Vec<_> = header["straplineTextOne"]["runs"]
        .as_array()
        .into_iter()
        .flatten()
        .collect();
    let mut episode = episode_context(header, video_id, &runs)?
        .context("YouTube Music returned no podcast episode playback details")?;
    let description = data
        .pointer("/contents/twoColumnBrowseResultsRenderer/secondaryContents/sectionListRenderer/contents")
        .and_then(Value::as_array)
        .and_then(|sections| sections.iter().find_map(|section| {
            section.pointer("/musicDescriptionShelfRenderer/description")
        }));
    episode.description = description.map(text).unwrap_or_default();
    episode.chapters = description
        .map(|description| episode_chapters(description, video_id))
        .unwrap_or_default();
    let title = text(&header["title"]);
    ensure!(
        !title.trim().is_empty(),
        "YouTube Music returned an unnamed episode"
    );
    let artist = text(&header["straplineTextOne"]);
    let mut details = track_details(header, &runs, video_id);
    details.episode = Some(episode);
    Ok(Track {
        id: video_id.to_owned(),
        title,
        artist: if artist.is_empty() {
            "Podcast".into()
        } else {
            artist
        },
        album: None,
        duration: details
            .episode
            .as_ref()
            .and_then(|episode| duration(&episode.duration_text)),
        thumbnail: thumbnail(&header["thumbnail"]),
        details,
    })
}

fn episode_chapters(description: &Value, video_id: &str) -> Vec<Chapter> {
    let Some(runs) = description["runs"].as_array() else {
        return Vec::new();
    };
    let mut chapters = Vec::new();
    let mut starts = HashSet::new();
    for (index, run) in runs.iter().enumerate() {
        let Some(endpoint) = run.pointer("/navigationEndpoint/watchEndpoint") else {
            continue;
        };
        let label = run["text"].as_str().unwrap_or_default();
        if endpoint["videoId"].as_str() != Some(video_id) || label.contains(['\n', '\r']) {
            continue;
        }
        let start = match endpoint.get("startTimeSeconds") {
            Some(value) => value.as_u64().and_then(|value| u32::try_from(value).ok()),
            None if matches!(label, "0:00" | "00:00" | "0:00:00") => Some(0),
            None => None,
        };
        let Some(start_seconds) = start else {
            continue;
        };
        let mut title = String::new();
        for following in &runs[index + 1..] {
            if following.get("navigationEndpoint").is_some() {
                break;
            }
            let Some(text) = following["text"].as_str() else {
                break;
            };
            if let Some((line, _)) = text.split_once(['\n', '\r']) {
                title.push_str(line);
                break;
            }
            title.push_str(text);
        }
        let title = title.trim().trim_start_matches('-').trim();
        if !title.is_empty() && starts.insert(start_seconds) {
            chapters.push(Chapter {
                title: title.to_owned(),
                start_seconds,
            });
        }
    }
    chapters
}

fn apply_episode_page(page: &mut Page, data: &Value, browse_id: &str) -> Result<()> {
    if let Some(video_id) = browse_id.strip_prefix("MPED") {
        let track = episode_header(data, video_id)?;
        page.title = track.title.clone();
        page.description = track.details.episode.as_ref().unwrap().description.clone();
        page.shelves = vec![Shelf {
            title: String::new(),
            items: vec![Item::Track(track)],
            layout: ShelfLayout::Songs,
            more: None,
        }];
        page.continuation = None;
        page.editable = false;
        return Ok(());
    }
    if valid_show_id(browse_id) {
        associate_show(page, browse_id, &page.title.clone());
    }
    Ok(())
}

fn associate_show(page: &mut Page, show_id: &str, title: &str) {
    for item in page.shelves.iter_mut().flat_map(|shelf| &mut shelf.items) {
        if let Item::Track(track) = item
            && let Some(episode) = &mut track.details.episode
            && episode.show_id.is_none()
        {
            episode.show_id = Some(show_id.to_owned());
            track.artist = title.to_owned();
        }
    }
    if let Some(continuation) = &mut page.continuation {
        continuation.show = Some((show_id.to_owned(), title.to_owned()));
    }
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
        .or_else(|| row.pointer("/onTap/watchEndpoint/videoId")?.as_str())
        .or_else(|| row.pointer("/overlay/musicItemThumbnailOverlayRenderer/content/musicPlayButtonRenderer/playNavigationEndpoint/watchEndpoint/videoId")?.as_str());
    let metadata: Vec<&Value> = columns
        .iter()
        .skip(1)
        .copied()
        .chain(std::iter::once(&row["subtitle"]))
        .chain(std::iter::once(&row["secondTitle"]))
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
        let episode = episode_context(row, id, &runs).ok()?;
        let show = episode.as_ref().and_then(|episode| {
            runs.iter().find(|run| {
                run.pointer("/navigationEndpoint/browseEndpoint/browseId")
                    .and_then(Value::as_str)
                    == episode.show_id.as_deref()
                    && episode.show_id.is_some()
            })
        });
        let show_id = show.and_then(|run| {
            run.pointer("/navigationEndpoint/browseEndpoint/browseId")
                .and_then(Value::as_str)
        });
        let is_creator = |run: &Value| {
            run.pointer("/navigationEndpoint/browseEndpoint/browseId")
                .and_then(Value::as_str)
                .is_some_and(|id| show_id.map_or_else(|| id.starts_with("UC"), |show| id == show))
        };
        let artist = metadata
            .iter()
            .find_map(|column| {
                let runs = column["runs"].as_array()?;
                let first = runs.iter().position(&is_creator)?;
                let last = runs.iter().rposition(&is_creator)?;
                let creators = &runs[first..=last];
                creators
                    .iter()
                    .all(|run| run.get("navigationEndpoint").is_none() || is_creator(run))
                    .then(|| {
                        creators
                            .iter()
                            .filter_map(|run| run["text"].as_str())
                            .collect::<String>()
                    })
            })
            .or_else(|| {
                if episode.is_some() {
                    return None;
                }
                metadata
                    .iter()
                    .map(|v| text(v))
                    .find(|s| !s.is_empty() && !s.starts_with("Song") && !s.starts_with("Video"))
            })
            .unwrap_or_else(|| {
                if episode.is_some() {
                    "Podcast".into()
                } else {
                    "YouTube Music".into()
                }
            });
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
            .next()
            .or_else(|| metadata.iter().find_map(|value| duration(&text(value))))
            .or_else(|| duration(&text(&row["lengthText"])))
            .or_else(|| {
                episode
                    .as_ref()
                    .and_then(|episode| duration(&episode.duration_text))
            });
        let mut details = track_details(row, &runs, id);
        details.episode = episode;
        return Some(Item::Track(Track {
            id: id.to_owned(),
            title,
            artist,
            album,
            duration,
            thumbnail: thumbnail(row),
            details,
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
    let subscribed = if browse_id.starts_with("UC") {
        row.pointer("/subscriptionButton/subscribeButtonRenderer")
            .into_iter()
            .chain(
                row.pointer("/menu/menuRenderer/topLevelButtons")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|button| button.get("subscribeButtonRenderer")),
            )
            .find_map(|button| ArtistSubscription::from_button(button).ok())
            .map(|subscription| subscription.subscribed)
    } else {
        None
    };
    CollectionDetails {
        play: row.pointer("/thumbnailOverlay/musicItemThumbnailOverlayRenderer/content/musicPlayButtonRenderer/playNavigationEndpoint")
            .or_else(|| row.pointer("/overlay/musicItemThumbnailOverlayRenderer/content/musicPlayButtonRenderer/playNavigationEndpoint"))
            .and_then(PlaylistTarget::from_navigation)
            .map(Arc::new),
        playlist_id: playlist_id.map(str::to_owned),
        saved: subscribed.or_else(|| item_rating(row).map(|rating| rating == Rating::Like)),
        editable: browse_id.strip_prefix("VL").filter(|id| !id.is_empty()).is_some_and(|id| {
            row.pointer("/menu/menuRenderer/items")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .any(|item| {
                    item.pointer("/menuNavigationItemRenderer/navigationEndpoint/playlistEditorEndpoint/playlistId")
                        .and_then(Value::as_str) == Some(id)
                })
        }),
    }
}

pub(crate) fn track_details(row: &Value, runs: &[&Value], video_id: &str) -> TrackDetails {
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
        plays: row["flexColumns"]
            .as_array()
            .into_iter()
            .flatten()
            .skip(1)
            .filter_map(|column| column.pointer("/musicResponsiveListItemFlexColumnRenderer/text"))
            .map(text)
            .find(|label| {
                label.starts_with(|c: char| c.is_ascii_digit())
                    && (label.ends_with(" plays") || label.ends_with(" views"))
            })
            .or_else(|| {
                ["viewCountText", "shortViewCountText"]
                    .into_iter()
                    .map(|key| text(&row[key]))
                    .find(|label| !label.is_empty())
            }),
        watch: WatchTarget::from_row(row, video_id).map(Arc::new),
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
                        let more = shelf.get("bottomEndpoint")
                            .and_then(|endpoint| BrowseLink::from_endpoint(&endpoint["browseEndpoint"], text(&shelf["bottomText"])))
                            .or_else(|| {
                                let button = shelf.pointer("/header/musicCarouselShelfBasicHeaderRenderer/moreContentButton/buttonRenderer")?;
                                BrowseLink::from_endpoint(&button["navigationEndpoint"]["browseEndpoint"], text(&button["text"]))
                            });
                        shelves.push(Shelf {
                            title,
                            items,
                            layout: if key == "musicCardShelfRenderer" {
                                ShelfLayout::Featured
                            } else {
                                contents
                                    .and_then(Value::as_array)
                                    .map_or(ShelfLayout::Cards, |items| shelf_layout(items))
                            },
                            more,
                        });
                    }
                    return;
                }
            }
            for child in map.values() {
                parse_shelves(child, shelves);
            }
        }
        Value::Array(items) => {
            let mut rows = Vec::new();
            let mut start = 0;
            for (index, item) in items.iter().enumerate() {
                if item.get("musicCardShelfRenderer").is_none()
                    && let Some(row) = parse_item(item)
                {
                    rows.push(row);
                    continue;
                }
                if !rows.is_empty() {
                    shelves.push(Shelf {
                        title: String::new(),
                        items: std::mem::take(&mut rows),
                        layout: shelf_layout(&items[start..index]),
                        more: None,
                    });
                }
                parse_shelves(item, shelves);
                start = index + 1;
            }
            if !rows.is_empty() {
                shelves.push(Shelf {
                    title: String::new(),
                    items: rows,
                    layout: shelf_layout(&items[start..]),
                    more: None,
                });
            }
        }
        _ => {}
    }
}

fn shelf_layout(items: &[Value]) -> ShelfLayout {
    if items.iter().any(|item| {
        item.pointer("/musicTwoRowItemRenderer/aspectRatio")
            .and_then(Value::as_str)
            == Some("MUSIC_TWO_ROW_ITEM_THUMBNAIL_ASPECT_RATIO_RECTANGLE_16_9")
    }) {
        return ShelfLayout::Videos;
    }
    if items
        .iter()
        .any(|item| item.get("musicResponsiveListItemRenderer").is_some())
    {
        return ShelfLayout::Songs;
    }
    ShelfLayout::Cards
}

fn owned_continuation(owner: &Value) -> Option<Continuation> {
    owner["continuations"]
        .as_array()
        .into_iter()
        .flatten()
        .find_map(|entry| {
            entry
                .pointer("/nextContinuationData/continuation")
                .and_then(Value::as_str)
        })
        .or_else(|| {
            owner["contents"].as_array()?.iter().find_map(|entry| {
                entry
                    .pointer(
                        "/continuationItemRenderer/continuationEndpoint/continuationCommand/token",
                    )
                    .and_then(Value::as_str)
            })
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
    let artist_header = data.pointer("/header/musicImmersiveHeaderRenderer");
    let subscription_button = artist_header
        .and_then(|header| header.pointer("/subscriptionButton/subscribeButtonRenderer"))
        .filter(|button| ArtistSubscription::from_button(button).is_ok());
    let subscribed = subscription_button.and_then(|button| button["subscribed"].as_bool());
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
    let continuation = match find(data, "musicPlaylistShelfRenderer")
        .or_else(|| find(data, "musicPlaylistShelfContinuation"))
        .or_else(|| data.pointer("/continuationContents/sectionListContinuation"))
        .or_else(|| data.get("sectionListContinuation"))
    {
        Some(owner) => owned_continuation(owner),
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
        header: header.map(|header| PageHeader {
            thumbnail: thumbnail(&header["thumbnail"]),
            artist: {
                let artist = text(&header["straplineTextOne"]);
                (!artist.is_empty()).then(|| {
                    header["straplineTextOne"]["runs"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .find_map(|run| {
                            BrowseLink::from_endpoint(
                                &run["navigationEndpoint"]["browseEndpoint"],
                                artist.clone(),
                            )
                        })
                        .map_or(HeaderArtist::Text(artist), HeaderArtist::Link)
                })
            },
            stats: text(&header["secondSubtitle"]),
            subtitle: ["subtitle", "monthlyListenerCount"]
                .into_iter()
                .map(|key| text(&header[key]))
                .filter(|part| !part.trim().is_empty())
                .collect::<Vec<_>>()
                .join(" • "),
            shuffle: artist_header
                .and_then(|header| header.pointer("/playButton/buttonRenderer"))
                .filter(|button| {
                    button.pointer("/icon/iconType").and_then(Value::as_str)
                        == Some("MUSIC_SHUFFLE")
                })
                .and_then(|button| PlaylistTarget::from_navigation(&button["navigationEndpoint"]))
                .map(Arc::new),
            mix: artist_header
                .and_then(|header| header.pointer("/startRadioButton/buttonRenderer"))
                .filter(|button| {
                    button.pointer("/icon/iconType").and_then(Value::as_str) == Some("MIX")
                })
                .and_then(|button| PlaylistTarget::from_navigation(&button["navigationEndpoint"]))
                .map(Arc::new),
            subscribed,
            subscription_label: subscription_button
                .map(|button| {
                    text(
                        &button[if subscribed == Some(true) {
                            "subscribedButtonText"
                        } else {
                            "unsubscribedButtonText"
                        }],
                    )
                })
                .unwrap_or_default(),
            subscriber_count: subscription_button
                .and_then(|button| {
                    [
                        "shortSubscriberCountText",
                        "longSubscriberCountText",
                        "subscriberCountText",
                    ]
                    .into_iter()
                    .map(|key| text(&button[key]))
                    .find(|text| !text.trim().is_empty())
                })
                .unwrap_or_default(),
        }),
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

    #[test]
    fn page_heroes_use_the_page_header_and_never_a_song_cover() {
        for renderer in [
            "musicResponsiveHeaderRenderer",
            "musicDetailHeaderRenderer",
            "musicImmersiveHeaderRenderer",
        ] {
            let response = json!({
                "header": {renderer: {
                    "title":{"simpleText":"Collection"},
                    "straplineTextOne":{"simpleText":"Artist"},
                    "subtitle":{"runs":[{"text":"Album"},{"text":" • 2026"}]},
                    "secondSubtitle":{"simpleText":"10 songs"},
                    "thumbnail":{"musicThumbnailRenderer":{"thumbnail":{"thumbnails":[{
                        "url":"https://lh3.googleusercontent.com/collection","width":360,"height":360
                    }]}}}
                }},
                "contents":[]
            });
            let page = parse_page(&response, "Fallback").unwrap();
            assert_eq!(page.title, "Collection");
            let header = page.header.unwrap();
            assert_eq!(
                header.thumbnail.as_deref(),
                Some("https://lh3.googleusercontent.com/collection")
            );
            assert_eq!(header.subtitle, "Album • 2026");
            assert_eq!(header.stats, "10 songs");
            assert!(
                matches!(header.artist, Some(HeaderArtist::Text(artist)) if artist == "Artist")
            );
        }
        let page = parse_page(
            &json!({"contents":[{"musicShelfRenderer":{"contents":[{
                "musicResponsiveListItemRenderer":{
                    "title":{"simpleText":"Song"},
                    "playlistItemData":{"videoId":"abcdefghijk"},
                    "thumbnail":{"thumbnails":[{"url":"https://i.ytimg.com/song","width":360}]}
                }
            }]}}]}),
            "Playlist",
        )
        .unwrap();
        assert_eq!(
            page.tracks()[0].thumbnail.as_deref(),
            Some("https://i.ytimg.com/song")
        );
        assert!(page.header.is_none());
    }

    #[test]
    fn artist_header_uses_issued_shuffle_mix_and_subscription_state() {
        let mut response = json!({"header":{"musicImmersiveHeaderRenderer":{
            "title":{"simpleText":"Artist"},
            "monthlyListenerCount":{"simpleText":"1.2M monthly listeners"},
            "playButton":{"buttonRenderer":{"icon":{"iconType":"MUSIC_SHUFFLE"},"navigationEndpoint":{"watchEndpoint":{
                "videoId":"abcdefghijk", "playlistId":"RDAOartist", "params":"wAEB8gECGAE%3D", "index":2
            }}}},
            "startRadioButton":{"buttonRenderer":{"icon":{"iconType":"MIX"},"navigationEndpoint":{"watchEndpoint":{
                "videoId":"lmnopqrstuv", "playlistId":"RDEMartist", "params":"wAEB"
            }}}},
            "subscriptionButton":{"subscribeButtonRenderer":{
                "channelId":"UCUDVBtnOQi4c7E8jebpjc9Q", "subscribed":false,
                "subscribedButtonText":{"simpleText":"Subscribed"},
                "unsubscribedButtonText":{"simpleText":"Subscribe"},
                "shortSubscriberCountText":{"simpleText":"7M"},
                "longSubscriberCountText":{"simpleText":"7M subscribers"}
            }}
        }},"contents":[]});
        for subscribed in [false, true] {
            response["header"]["musicImmersiveHeaderRenderer"]["subscriptionButton"]["subscribeButtonRenderer"]
                ["subscribed"] = json!(subscribed);
            let header = parse_page(&response, "Artist").unwrap().header.unwrap();
            assert_eq!(header.subtitle, "1.2M monthly listeners");
            assert_eq!(header.subscribed, Some(subscribed));
            assert_eq!(
                header.subscription_label,
                if subscribed {
                    "Subscribed"
                } else {
                    "Subscribe"
                }
            );
            assert_eq!(header.subscriber_count, "7M");
            let shuffle = header.shuffle.unwrap().request();
            assert_eq!(
                shuffle,
                json!({"videoId":"abcdefghijk", "playlistId":"RDAOartist", "params":"wAEB8gECGAE%3D", "index":2})
            );
            let mix = header.mix.unwrap().request();
            assert_eq!(mix["playlistId"], "RDEMartist");
            assert_eq!(mix["videoId"], "lmnopqrstuv");
        }
        let header = response["header"].take();
        response["frameworkUpdates"] = header;
        let header = parse_page(&response, "Artist").unwrap().header.unwrap();
        assert!(header.shuffle.is_none() && header.mix.is_none() && header.subscribed.is_none());
    }

    #[test]
    fn collection_play_uses_only_its_thumbnail_control_and_is_not_persisted() {
        let mut row = json!({"musicTwoRowItemRenderer":{
            "title":{"simpleText":"Playlist"},
            "navigationEndpoint":{"browseEndpoint":{"browseId":"VLplaylist"}},
            "thumbnailOverlay":{"musicItemThumbnailOverlayRenderer":{"content":{"musicPlayButtonRenderer":{
                "playNavigationEndpoint":{"watchPlaylistEndpoint":{"playlistId":"playlist", "params":"wAEB", "index":3}}
            }}}},
            "menu":{"watchPlaylistEndpoint":{"playlistId":"unrelated-menu-playlist"}}
        }});
        let Some(Item::Collection(collection)) = parse_item(&row) else {
            panic!("expected a collection")
        };
        let play = collection.details.play.as_ref().unwrap();
        assert_eq!(
            play.request(),
            json!({"playlistId":"playlist", "params":"wAEB", "index":3})
        );
        assert!(!format!("{play:?}").contains("wAEB"));
        let restored: Collection =
            serde_json::from_value(serde_json::to_value(collection).unwrap()).unwrap();
        assert!(restored.details.play.is_none());
        row["musicTwoRowItemRenderer"]["thumbnailOverlay"] = Value::Null;
        let Some(Item::Collection(collection)) = parse_item(&row) else {
            panic!("expected a collection")
        };
        assert!(collection.details.play.is_none());
    }

    #[test]
    fn playlist_targets_require_an_unambiguous_valid_issued_playlist() {
        for navigation in [
            json!({"watchPlaylistEndpoint":{}}),
            json!({"watchPlaylistEndpoint":{"playlistId":""}}),
            json!({"watchPlaylistEndpoint":{"playlistId":"../other"}}),
            json!({"watchPlaylistEndpoint":{"playlistId":"valid", "params":"bad%XX"}}),
            json!({"watchPlaylistEndpoint":{"playlistId":"valid", "index":-1}}),
            json!({"watchEndpoint":{"videoId":"invalid", "playlistId":"valid"}}),
            json!({"watchEndpoint":{"videoId":"abcdefghijk"}}),
            json!({"watchEndpoint":{"videoId":"abcdefghijk", "playlistId":"valid"}, "watchPlaylistEndpoint":{"playlistId":"another"}}),
        ] {
            assert!(PlaylistTarget::from_navigation(&navigation).is_none());
        }
    }

    #[test]
    fn shelf_layouts_keep_responsive_songs_and_wide_video_carousels() {
        let cards = json!({"musicTwoRowItemRenderer":{
            "title":{"simpleText":"Album"},
            "navigationEndpoint":{"browseEndpoint":{"browseId":"MPREalbum"}},
            "aspectRatio":"MUSIC_TWO_ROW_ITEM_THUMBNAIL_ASPECT_RATIO_SQUARE"
        }});
        let videos = json!({"musicTwoRowItemRenderer":{
            "title":{"simpleText":"Video"},
            "navigationEndpoint":{"watchEndpoint":{"videoId":"abcdefghijk"}},
            "aspectRatio":"MUSIC_TWO_ROW_ITEM_THUMBNAIL_ASPECT_RATIO_RECTANGLE_16_9"
        }});
        let songs = json!({"musicResponsiveListItemRenderer":{
            "title":{"simpleText":"Song"},
            "playlistItemData":{"videoId":"abcdefghijk"}
        }});
        for (item, layout) in [
            (cards, ShelfLayout::Cards),
            (videos, ShelfLayout::Videos),
            (songs.clone(), ShelfLayout::Songs),
        ] {
            let page = parse_page(&json!({"contents":[{"musicCarouselShelfRenderer":{
                "header":{"musicCarouselShelfBasicHeaderRenderer":{"title":{"simpleText":"Shelf"}}},
                "contents":[item]
            }}]}), "Home").unwrap();
            assert_eq!(page.shelves.len(), 1);
            assert_eq!(page.shelves[0].layout, layout);
            assert_eq!(page.shelves[0].items.len(), 1);
        }
        let list = parse_page(
            &json!({"contents":[{"musicShelfRenderer":{"contents":[songs]}}]}),
            "Songs",
        )
        .unwrap();
        assert_eq!(list.shelves[0].layout, ShelfLayout::Songs);
    }

    #[test]
    fn shelf_more_controls_preserve_scoped_labels_and_parameterized_destinations() {
        let mut response = json!({"contents":[
            {"musicShelfRenderer":{
                "title":{"simpleText":"Top songs"},
                "bottomText":{"simpleText":"Show all"},
                "bottomEndpoint":{"browseEndpoint":{"browseId":"VLOLAKartist", "params":"ggMCCAI%3D"}},
                "contents":[{"musicResponsiveListItemRenderer":{"title":{"simpleText":"Song"},"playlistItemData":{"videoId":"abcdefghijk"}}}]
            }},
            {"musicCarouselShelfRenderer":{
                "header":{"musicCarouselShelfBasicHeaderRenderer":{
                    "title":{"runs":[{"text":"Albums", "navigationEndpoint":{"browseEndpoint":{"browseId":"MPADartist", "params":"title-only"}}}]},
                    "moreContentButton":{"buttonRenderer":{"text":{"simpleText":"More"},"navigationEndpoint":{"browseEndpoint":{"browseId":"MPADartist", "params":"ggMIegYIARoCAQI%3D"}}}}
                }},
                "contents":[{"musicTwoRowItemRenderer":{"title":{"simpleText":"Album"}, "navigationEndpoint":{"browseEndpoint":{"browseId":"MPREalbum"}}}}]
            }}
        ]});
        let page = parse_page(&response, "Artist").unwrap();
        let songs = page.shelves[0].more.as_ref().unwrap();
        assert_eq!(songs.title, "Show all");
        assert_eq!(songs.id, "VLOLAKartist");
        assert_eq!(songs.params.as_deref(), Some("ggMCCAI%3D"));
        let albums = page.shelves[1].more.as_ref().unwrap();
        assert_eq!(albums.title, "More");
        assert_eq!(albums.id, "MPADartist");
        assert_eq!(albums.params.as_deref(), Some("ggMIegYIARoCAQI%3D"));

        response["contents"][0]["musicShelfRenderer"]["bottomEndpoint"]["browseEndpoint"]["params"] =
            json!("bad%XX");
        response["contents"][1]["musicCarouselShelfRenderer"]["header"]["musicCarouselShelfBasicHeaderRenderer"]
            ["moreContentButton"] = Value::Null;
        let page = parse_page(&response, "Artist").unwrap();
        assert!(page.shelves[0].more.is_none());
        assert!(
            page.shelves[1].more.is_none(),
            "A title link must not fabricate a More control"
        );
        response["contents"][0]["musicShelfRenderer"]["bottomEndpoint"]["browseEndpoint"]["params"] =
            json!("valid");
        response["contents"][0]["musicShelfRenderer"]["bottomText"] = json!({"simpleText":""});
        assert!(
            parse_page(&response, "Artist").unwrap().shelves[0]
                .more
                .is_none()
        );
    }

    #[test]
    fn creator_columns_keep_collaborators_and_separate_play_counts() {
        let row = json!({"musicResponsiveListItemRenderer":{
            "flexColumns":[
                {"musicResponsiveListItemFlexColumnRenderer":{"text":{"runs":[{"text":"A song","navigationEndpoint":{"watchEndpoint":{"videoId":"abcdefghijk"}}}]}}},
                {"musicResponsiveListItemFlexColumnRenderer":{"text":{"runs":[
                    {"text":"Artist A","navigationEndpoint":{"browseEndpoint":{"browseId":"UCfirst"}}},
                    {"text":" & "},
                    {"text":"Artist B","navigationEndpoint":{"browseEndpoint":{"browseId":"UCsecond"}}}
                ]}}},
                {"musicResponsiveListItemFlexColumnRenderer":{"text":{"runs":[{"text":"1.2B plays"}]}}},
                {"musicResponsiveListItemFlexColumnRenderer":{"text":{"runs":[{"text":"The album","navigationEndpoint":{"browseEndpoint":{"browseId":"MPREalbum"}}}]}}}
            ]
        }});
        let Some(Item::Track(track)) = parse_item(&row) else {
            panic!("expected a song");
        };
        assert_eq!(track.artist, "Artist A & Artist B");
        assert_eq!(track.details.plays.as_deref(), Some("1.2B plays"));
        assert_eq!(track.album.as_deref(), Some("The album"));
        assert_eq!(track.details.artist_id.as_deref(), Some("UCfirst"));
        let restored: Track =
            serde_json::from_value(serde_json::to_value(&track).unwrap()).unwrap();
        assert!(restored.details.plays.is_none());
        assert_eq!(restored.artist, track.artist);

        let mixed = json!({"musicResponsiveListItemRenderer":{
            "title":{"runs":[{"text":"A song","navigationEndpoint":{"watchEndpoint":{"videoId":"abcdefghijk"}}}]},
            "subtitle":{"runs":[
                {"text":"Song • "},
                {"text":"Artist A","navigationEndpoint":{"browseEndpoint":{"browseId":"UCfirst"}}},
                {"text":" & "},
                {"text":"Artist B","navigationEndpoint":{"browseEndpoint":{"browseId":"UCsecond"}}},
                {"text":" • "},
                {"text":"The album","navigationEndpoint":{"browseEndpoint":{"browseId":"MPREalbum"}}},
                {"text":" • 1.2B plays"}
            ]}
        }});
        let Some(Item::Track(track)) = parse_item(&mixed) else {
            panic!("expected a song");
        };
        assert_eq!(track.artist, "Artist A & Artist B");
        assert!(
            track.details.plays.is_none(),
            "A mixed subtitle is not a dedicated play-count column"
        );
    }

    #[test]
    fn responsive_header_keeps_linked_artist_and_stats_on_separate_lines() {
        let page = parse_page(&json!({
            "header":{"musicResponsiveHeaderRenderer":{
                "title":{"simpleText":"An album"},
                "straplineTextOne":{"runs":[{"text":"The artist","navigationEndpoint":{"browseEndpoint":{"browseId":"UCartist", "params":"issued%3D"}}}]},
                "subtitle":{"simpleText":"Album • 2013"},
                "secondSubtitle":{"simpleText":"13 songs • 1 hour, 14 minutes"}
            }},
            "contents":[]
        }), "Album").unwrap();
        let header = page.header.unwrap();
        assert_eq!(header.subtitle, "Album • 2013");
        assert_eq!(header.stats, "13 songs • 1 hour, 14 minutes");
        let Some(HeaderArtist::Link(artist)) = header.artist else {
            panic!("expected linked artist");
        };
        assert_eq!(artist.title, "The artist");
        assert_eq!(artist.id, "UCartist");
        assert_eq!(artist.params.as_deref(), Some("issued%3D"));
    }

    #[test]
    fn featured_search_result_keeps_its_own_item_before_associated_tracks() {
        let page = parse_page(&json!({"contents":[
            {"musicCardShelfRenderer":{
                "title":{"runs":[{"text":"The artist","navigationEndpoint":{"browseEndpoint":{"browseId":"UCartist"}}}]},
                "subtitle":{"simpleText":"Artist • 67M monthly audience"},
                "contents":[
                    {"musicResponsiveListItemRenderer":{"title":{"simpleText":"First song"},"playlistItemData":{"videoId":"abcdefghijk"}}},
                    {"musicResponsiveListItemRenderer":{"title":{"simpleText":"Second song"},"playlistItemData":{"videoId":"lmnopqrstuv"}}}
                ]
            }},
            {"itemSectionRenderer":{"contents":[
                {"musicResponsiveListItemRenderer":{"title":{"runs":[{"text":"Another artist","navigationEndpoint":{"browseEndpoint":{"browseId":"UCanother"}}}]}}}
            ]}}
        ]}), "Search").unwrap();
        assert_eq!(page.shelves[0].layout, ShelfLayout::Featured);
        assert_eq!(
            page.shelves[0]
                .items
                .iter()
                .map(Item::title)
                .collect::<Vec<_>>(),
            ["The artist", "First song", "Second song"]
        );
        assert_eq!(
            page.shelves[0].items[0].subtitle(),
            "Artist • 67M monthly audience"
        );
        assert_eq!(page.shelves[1].items[0].title(), "Another artist");
        assert!(
            page.shelves[1].title.is_empty(),
            "Untitled service sections must not invent headings"
        );
    }

    #[test]
    fn suggestions_have_a_short_deadline_without_limiting_catalog_pages() {
        let catalog = Catalog::new().unwrap();
        let suggestions = catalog
            .page_request(
                "music/get_search_suggestions",
                json!({"input":"Song"}),
                None,
            )
            .unwrap()
            .build()
            .unwrap();
        assert_eq!(suggestions.timeout(), Some(&Duration::from_secs(4)));
        let browse = catalog
            .page_request("browse", json!({"browseId":"FEmusic_home"}), None)
            .unwrap()
            .build()
            .unwrap();
        assert!(browse.timeout().is_none());
    }

    #[test]
    fn page_selection_keeps_duplicate_occurrences_across_mixed_shelves() {
        let track = |id: &str| {
            Item::Track(
                serde_json::from_value(json!({
                    "id":id, "title":id, "artist":"Artist"
                }))
                .unwrap(),
            )
        };
        let mut page = Page {
            title: "Mixed results".into(),
            header: None,
            shelves: Vec::new(),
            continuation: None,
            editable: false,
            description: String::new(),
            privacy: None,
            chips: Vec::new(),
            countries: Vec::new(),
        };
        page.shelves = vec![
            Shelf {
                title: "First".into(),
                items: vec![track("A"), track("B")],
                layout: ShelfLayout::Songs,
                more: None,
            },
            Shelf {
                title: "Second".into(),
                items: vec![
                    Item::Navigation(BrowseLink::new("FEexplore", "Explore")),
                    track("A"),
                    track("C"),
                ],
                layout: ShelfLayout::Songs,
                more: None,
            },
        ];
        assert_eq!(page.track_index(2), None);
        assert_eq!(page.track_index(5), None);
        assert_eq!(page.track_index(0), Some(0));
        assert_eq!(page.track_index(3), Some(2));
        let mut queue = crate::library::Queue::default();
        queue.replace(page.tracks(), page.track_index(3).unwrap());
        assert_eq!(queue.current().unwrap().id, "A");
        assert!(queue.advance(crate::library::Repeat::Off, false));
        assert_eq!(queue.current().unwrap().id, "C");
    }

    #[test]
    fn artist_subscription_uses_its_header_channel_and_current_state() {
        let browse_id = "UCmMUZbaYdNH0bEd1PAlAqsA";
        let channel_id = "UCUDVBtnOQi4c7E8jebpjc9Q";
        for subscribed in [false, true] {
            let button = json!({"channelId":channel_id,"subscribed":subscribed});
            let mut response = json!({
                "header":{"musicImmersiveHeaderRenderer":{
                    "title":{"simpleText":"Oasis"},
                    "subscriptionButton":{"subscribeButtonRenderer":button}
                }},
                "contents":{"subscriptionButton":{"subscribeButtonRenderer":{
                    "channelId":browse_id,"subscribed":!subscribed
                }}}
            });
            let subscription = ArtistSubscription::from_response(&response).unwrap();
            assert_eq!(subscription.channel_id, channel_id);
            assert_ne!(subscription.channel_id, browse_id);
            assert_eq!(subscription.subscribed, subscribed);

            for row in [
                json!({"subscriptionButton":{"subscribeButtonRenderer":button}}),
                json!({"menu":{"menuRenderer":{"topLevelButtons":[{
                    "subscribeButtonRenderer":button
                }]}}}),
            ] {
                assert_eq!(collection_details(&row, browse_id).saved, Some(subscribed));
            }
            response.as_object_mut().unwrap().remove("header");
            assert!(ArtistSubscription::from_response(&response).is_err());
        }
    }

    #[test]
    fn artist_subscription_rejects_invalid_channels_and_missing_state() {
        for button in [
            json!({"channelId":"UCshort","subscribed":false}),
            json!({"channelId":"UCUDVBtnOQi4c7E8jebpjc9/","subscribed":false}),
            json!({"channelId":"UCUDVBtnOQi4c7E8jebpjc9Q"}),
            json!({"channelId":"UCUDVBtnOQi4c7E8jebpjc9Q","subscribed":"false"}),
        ] {
            let response = json!({"header":{"musicImmersiveHeaderRenderer":{
                "subscriptionButton":{"subscribeButtonRenderer":button}
            }}});
            assert!(ArtistSubscription::from_response(&response).is_err());
        }
    }

    #[test]
    fn account_name_requires_a_signed_in_header() {
        let mut response = json!({"activeAccountHeaderRenderer":{
            "accountName":{"simpleText":"Test account"},
            "channelHandle":{"simpleText":"@test"}
        }});
        let account = Account::from_response(&response).unwrap();
        assert_eq!(account.name, "Test account");
        assert_eq!(account.handle, "@test");
        response["activeAccountHeaderRenderer"]["accountName"] = json!({});
        assert!(Account::from_response(&response).is_err());
        assert!(Account::from_response(&json!({})).is_err());
    }

    fn episode_row() -> Value {
        json!({"musicMultiRowListItemRenderer":{
            "title":{"runs":[{"text":"An episode","navigationEndpoint":{
                "browseEndpoint":{"browseId":"MPEDabcdefghijk"}
            }}]},
            "onTap":{"watchEndpoint":{
                "videoId":"abcdefghijk","params":"issued-params%3D",
                "watchEndpointMusicSupportedConfigs":{"watchEndpointMusicConfig":{"musicVideoType":PODCAST_EPISODE}}
            }},
            "subtitle":{"runs":[{"text":"42K views"},{"text":" • "},{"text":"2d ago"}]},
            "description":{"runs":[{"text":"Episode summary"}]},
            "playbackProgress":{"musicPlaybackProgressRenderer":{
                "durationText":{"runs":[{"text":" • "},{"text":"2 hr 32 min"}]},
                "playbackProgressPercentage":37,
                "videoPlaybackPositionFeedbackToken":"synthetic-progress-secret"
            }}
        }})
    }

    fn episode_response() -> Value {
        json!({"contents":{"twoColumnBrowseResultsRenderer":{
            "tabs":[{"tabRenderer":{"content":{"sectionListRenderer":{"contents":[{
                "musicResponsiveHeaderRenderer":{
                    "title":{"runs":[{"text":"An episode"}]},
                    "straplineTextOne":{"runs":[{"text":"The show","navigationEndpoint":{
                        "browseEndpoint":{"browseId":"MPSPPLshow"}
                    }}]},
                    "subtitle":{"runs":[{"text":"42K views • 2d ago"}]},
                    "progress":{"musicPlaybackProgressRenderer":{
                        "durationText":{"runs":[{"text":" • "},{"text":"2 hr 32 min"}]},
                        "playbackProgressPercentage":37
                    }},
                    "buttons":[{"musicPlayButtonRenderer":{"playNavigationEndpoint":{
                        "watchEndpoint":{
                            "videoId":"abcdefghijk","params":"issued-params%3D",
                            "watchEndpointMusicSupportedConfigs":{"watchEndpointMusicConfig":{"musicVideoType":PODCAST_EPISODE}}
                        }
                    }}}]
                }
            }]}}}}],
            "secondaryContents":{"sectionListRenderer":{"contents":[{
                "musicDescriptionShelfRenderer":{"description":{"runs":[
                    {"text":"The full episode description.\n"},
                    {"text":"04:05","navigationEndpoint":{"watchEndpoint":{"videoId":"abcdefghijk","startTimeSeconds":245}}},
                    {"text":" First topic"}
                ]}}
            }]}}
        }},"frameworkUpdates":{"musicDescriptionShelfRenderer":{"description":{"runs":[
            {"text":"00:00","navigationEndpoint":{"watchEndpoint":{"videoId":"abcdefghijk"}}},
            {"text":" Unrelated description"}
        ]}}}})
    }

    #[test]
    fn episodes_keep_public_metadata_and_known_show_association() {
        let row = episode_row();
        let mut page = parse_page(
            &json!({"contents":[{"musicShelfRenderer":{"contents":[row.clone()]}}]}),
            "The show",
        )
        .unwrap();
        apply_episode_page(&mut page, &json!({}), "MPSPPLshow").unwrap();
        let track = page.tracks().pop().unwrap();
        let episode = track.details.episode.as_ref().unwrap();
        assert_eq!(track.id, "abcdefghijk");
        assert_eq!(track.artist, "The show");
        assert_eq!(track.duration, None);
        assert_eq!(episode.show_id.as_deref(), Some("MPSPPLshow"));
        assert_eq!(episode.description, "Episode summary");
        assert_eq!(episode.subtitle, "42K views • 2d ago");
        assert_eq!(episode.duration_text, "2 hr 32 min");
        assert_eq!(episode.watch_params.as_deref(), Some("issued-params%3D"));
        assert_eq!(track.credits_id(), None);

        let mut linked = row;
        linked["musicMultiRowListItemRenderer"]["secondTitle"] = json!({"runs":[{
            "text":"A different show","navigationEndpoint":{"browseEndpoint":{"browseId":"MPSPPLother"}}
        }]});
        let Some(Item::Track(track)) = parse_item(&linked) else {
            panic!("expected an episode")
        };
        assert_eq!(track.artist, "A different show");
        assert_eq!(
            track.details.episode.unwrap().show_id.as_deref(),
            Some("MPSPPLother")
        );
    }

    #[test]
    fn episode_pages_use_the_matching_header_and_scoped_description() {
        let response = episode_response();
        let mut page = parse_page(&response, "Episode").unwrap();
        apply_episode_page(&mut page, &response, "MPEDabcdefghijk").unwrap();
        assert_eq!(page.title, "An episode");
        assert_eq!(page.tracks().len(), 1);
        assert_eq!(page.tracks()[0].artist, "The show");
        assert_eq!(
            page.description,
            "The full episode description.\n04:05 First topic"
        );
        assert_eq!(
            page.tracks()[0]
                .details
                .episode
                .as_ref()
                .unwrap()
                .description,
            page.description
        );
        assert_eq!(
            page.tracks()[0].details.episode.as_ref().unwrap().chapters,
            [Chapter {
                title: "First topic".into(),
                start_seconds: 245,
            }]
        );
        assert!(!page.editable && page.continuation.is_none());
        assert!(episode_header(&response, "lmnopqrstuv").is_err());

        let mut unrelated = response.clone();
        unrelated["contents"]["twoColumnBrowseResultsRenderer"]["secondaryContents"] = json!({});
        assert!(
            episode_header(&unrelated, "abcdefghijk")
                .unwrap()
                .details
                .episode
                .unwrap()
                .description
                .is_empty()
        );
        assert!(
            episode_header(&unrelated, "abcdefghijk")
                .unwrap()
                .details
                .episode
                .unwrap()
                .chapters
                .is_empty()
        );
        assert!(BrowseLink::new("MPEDinvalid", "Episode").body().is_err());
    }

    fn chapter_link(label: &str, start: Option<Value>, video_id: &str) -> Value {
        let mut endpoint = json!({"videoId":video_id});
        if let Some(start) = start {
            endpoint["startTimeSeconds"] = start;
        }
        json!({"text":label,"navigationEndpoint":{"watchEndpoint":endpoint}})
    }

    #[test]
    fn chapter_offsets_are_authoritative_with_narrow_zero_defaults_and_stable_deduplication() {
        let video = "abcdefghijk";
        let description = json!({"runs":[
            chapter_link("00:00",None,video), {"text":" - Introduction\n"},
            chapter_link("99:59",Some(json!(15)),video), {"text":" - "}, {"text":"A service offset\n"},
            chapter_link("0:00",None,video), {"text":" Duplicate introduction\n"},
            chapter_link("00:03",Some(json!(3)),video), {"text":" Earlier in service order\n"},
            chapter_link("00:15",Some(json!(15)),video), {"text":" Duplicate offset\n"}
        ]});
        assert_eq!(
            episode_chapters(&description, video),
            [
                Chapter {
                    title: "Introduction".into(),
                    start_seconds: 0
                },
                Chapter {
                    title: "A service offset".into(),
                    start_seconds: 15
                },
                Chapter {
                    title: "Earlier in service order".into(),
                    start_seconds: 3
                },
            ]
        );
        for label in ["0:00", "00:00", "0:00:00"] {
            let description =
                json!({"runs":[chapter_link(label,None,video),{"text":" Beginning"}]});
            assert_eq!(episode_chapters(&description, video)[0].start_seconds, 0);
        }
    }

    #[test]
    fn chapter_links_require_matching_video_valid_offsets_and_plain_same_line_titles() {
        let video = "abcdefghijk";
        for offset in [
            json!(-1),
            json!(1.5),
            json!("12"),
            json!(true),
            Value::Null,
            json!(u64::from(u32::MAX) + 1),
        ] {
            let description = json!({"runs":[chapter_link("00:00",Some(offset),video),{"text":" Invalid offset"}]});
            assert!(episode_chapters(&description, video).is_empty());
        }
        for label in ["00:15", "00:00:00", "Watch this episode"] {
            let description =
                json!({"runs":[chapter_link(label,None,video),{"text":" No issued position"}]});
            assert!(episode_chapters(&description, video).is_empty());
        }
        let description = json!({"runs":[
            chapter_link("00:15",Some(json!(15)),"lmnopqrstuv"),{"text":" Wrong video\n"},
            chapter_link("00:20",Some(json!(20)),video),{"text":"\nTitle on another line\n"},
            chapter_link("00:25\n",Some(json!(25)),video),{"text":" Split timestamp line\n"},
            chapter_link("00:30",Some(json!(30)),video),{"text":"Linked title","navigationEndpoint":{"urlEndpoint":{"url":"https://example.invalid"}}},
            {"text":"00:45 - Plain text is not a chapter\n"},
            chapter_link("An explicit zero",Some(json!(0)),video),{"text":" A valid title"}
        ]});
        assert_eq!(
            episode_chapters(&description, video),
            [Chapter {
                title: "A valid title".into(),
                start_seconds: 0
            }]
        );
        assert!(episode_chapters(&json!({"nested":{"runs":[chapter_link("00:00",None,video),{"text":" Unrelated"}]}}),video).is_empty());
    }

    #[test]
    fn malformed_episode_playback_is_not_downgraded_to_music() {
        for params in [
            json!("untrusted\nparams"),
            json!("%XY"),
            json!(42),
            json!("A".repeat(8193)),
        ] {
            let mut row = episode_row();
            row["musicMultiRowListItemRenderer"]["onTap"]["watchEndpoint"]["params"] = params;
            assert!(parse_item(&row).is_none());
        }
        let mut row = episode_row();
        row["musicMultiRowListItemRenderer"]["playlistItemData"] = json!({"videoId":"lmnopqrstuv"});
        assert!(parse_item(&row).is_none());
        let mut response = episode_response();
        *response.pointer_mut("/contents/twoColumnBrowseResultsRenderer/tabs/0/tabRenderer/content/sectionListRenderer/contents/0/musicResponsiveHeaderRenderer/buttons/0/musicPlayButtonRenderer/playNavigationEndpoint/watchEndpoint/watchEndpointMusicSupportedConfigs/watchEndpointMusicConfig/musicVideoType").unwrap() = json!("MUSIC_VIDEO_TYPE_ATV");
        assert!(episode_header(&response, "abcdefghijk").is_err());
    }

    #[test]
    fn episode_metadata_persists_without_playback_params_or_account_progress() {
        let Some(Item::Track(mut track)) = parse_item(&episode_row()) else {
            panic!("expected an episode")
        };
        let serialized = serde_json::to_value(&track).unwrap();
        let encoded = serialized.to_string();
        assert!(
            !encoded.contains("issued-params") && !encoded.contains("synthetic-progress-secret")
        );
        assert!(!encoded.contains("playbackProgressPercentage"));
        assert!(!format!("{:?}", track).contains("issued-params"));
        let restored: Track = serde_json::from_value(serialized).unwrap();
        assert!(
            restored
                .details
                .episode
                .as_ref()
                .unwrap()
                .watch_params
                .is_none()
        );
        assert_eq!(
            restored.details.episode.as_ref().unwrap().description,
            "Episode summary"
        );
        track.details.clear_account_state();
        assert_eq!(track, restored);
    }

    #[test]
    fn show_continuations_inherit_association_without_overwriting_linked_shows() {
        let catalog = Catalog::new().unwrap();
        let first = json!({"contents":[{"musicShelfRenderer":{
            "contents":[episode_row()],
            "continuations":[{"nextContinuationData":{"continuation":"first-show-page"}}]
        }}]});
        let mut page = parse_page(&first, "The show").unwrap();
        apply_episode_page(&mut page, &first, "MPSPPLshow").unwrap();
        let first = catalog
            .bind_page(page, &first, None)
            .unwrap()
            .continuation
            .unwrap();
        assert_eq!(first.show, Some(("MPSPPLshow".into(), "The show".into())));
        let mut linked = episode_row();
        linked["musicMultiRowListItemRenderer"]["secondTitle"] = json!({"runs":[{
            "text":"A linked show","navigationEndpoint":{"browseEndpoint":{"browseId":"MPSPPLother"}}
        }]});
        let response = json!({"continuationContents":{"musicShelfContinuation":{
            "contents":[episode_row(), linked],
            "continuations":[{"nextContinuationData":{"continuation":"next-show-page"}}]
        }}});
        let continued = catalog
            .bind_page(
                parse_page(&response, "More music").unwrap(),
                &response,
                Some(&first),
            )
            .unwrap();
        let tracks = continued.tracks();
        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].artist, "The show");
        assert_eq!(
            tracks[0]
                .details
                .episode
                .as_ref()
                .unwrap()
                .show_id
                .as_deref(),
            Some("MPSPPLshow")
        );
        assert_eq!(tracks[1].artist, "A linked show");
        assert_eq!(
            tracks[1]
                .details
                .episode
                .as_ref()
                .unwrap()
                .show_id
                .as_deref(),
            Some("MPSPPLother")
        );
        let next = continued.continuation.unwrap();
        assert_eq!(next.show, first.show);
        assert_eq!(next.token(), "next-show-page");
        let terminal = json!({"continuationContents":{"musicShelfContinuation":{
            "contents":[episode_row()]
        }}});
        let final_page = catalog
            .bind_page(
                parse_page(&terminal, "More music").unwrap(),
                &terminal,
                Some(&next),
            )
            .unwrap();
        assert!(final_page.continuation.is_none());
        assert_eq!(final_page.tracks()[0].artist, "The show");
        assert_eq!(
            final_page.tracks()[0]
                .details
                .episode
                .as_ref()
                .unwrap()
                .show_id
                .as_deref(),
            Some("MPSPPLshow")
        );
    }

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
    fn playlist_pages_follow_the_track_list_instead_of_the_recommendation_feed() {
        let mut response = json!({"contents":{"twoColumnBrowseResultsRenderer":{
            "secondaryContents":{"sectionListRenderer":{
                "contents":[{"musicPlaylistShelfRenderer":{"contents":[
                    {"musicResponsiveListItemRenderer":{
                        "playlistItemData":{"videoId":"abcdefghijk"},
                        "flexColumns":[{"musicResponsiveListItemFlexColumnRenderer":{
                            "text":{"runs":[{"text":"A song"}]}
                        }}]
                    }},
                    {"continuationItemRenderer":{"continuationEndpoint":{
                        "continuationCommand":{"token":"playlist-tail"}
                    }}}
                ]}}],
                "continuations":[{"nextContinuationData":{"continuation":"recommendations"}}]
            }}
        }}});
        let page = parse_page(&response, "Playlist").unwrap();
        assert_eq!(page.tracks().len(), 1);
        assert_eq!(page.continuation.unwrap().token(), "playlist-tail");

        response.pointer_mut("/contents/twoColumnBrowseResultsRenderer/secondaryContents/sectionListRenderer/contents/0/musicPlaylistShelfRenderer/contents")
            .unwrap().as_array_mut().unwrap().pop();
        let page = parse_page(&response, "Playlist").unwrap();
        assert_eq!(page.tracks().len(), 1);
        assert!(page.continuation.is_none());

        let continued = json!({"continuationContents":{"musicPlaylistShelfContinuation":{
            "contents":[],
            "continuations":[{"nextContinuationData":{"continuation":"later-tracks"}}]
        }}});
        assert_eq!(
            parse_page(&continued, "More music")
                .unwrap()
                .continuation
                .unwrap()
                .token(),
            "later-tracks"
        );
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
            owned_continuation(&response["continuationContents"]["sectionListContinuation"])
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
    fn song_durations_accept_simple_text_columns_and_length_labels() {
        let mut row = json!({"musicResponsiveListItemRenderer":{
            "title":{"simpleText":"Song"},
            "playlistItemData":{"videoId":"abcdefghijk"},
            "fixedColumns":[{"musicResponsiveListItemFixedColumnRenderer":{"text":{"simpleText":"3:42"}}}]
        }});
        let Some(Item::Track(track)) = parse_item(&row) else {
            panic!("expected a song")
        };
        assert_eq!(track.duration, Some(222));
        row["musicResponsiveListItemRenderer"]["fixedColumns"] = json!([]);
        row["musicResponsiveListItemRenderer"]["lengthText"] = json!({"simpleText":"4:20"});
        let Some(Item::Track(track)) = parse_item(&row) else {
            panic!("expected a song")
        };
        assert_eq!(track.duration, Some(260));
        row["musicResponsiveListItemRenderer"]["lengthText"] = json!({"simpleText":"2M views"});
        let Some(Item::Track(track)) = parse_item(&row) else {
            panic!("expected a song")
        };
        assert_eq!(track.duration, None);
    }

    #[test]
    fn issued_watch_targets_reject_malformed_and_conflicting_clicks() {
        let endpoint = json!({
            "videoId":"abcdefghijk", "playlistId":"OLAKplaylist", "params":"issued%3D", "index":2,
            "watchEndpointMusicSupportedConfigs":{"watchEndpointMusicConfig":{"musicVideoType":"MUSIC_VIDEO_TYPE_ATV"}}
        });
        let row = json!({"musicResponsiveListItemRenderer":{
            "title":{"runs":[{"text":"Song","navigationEndpoint":{"watchEndpoint":endpoint}}]},
            "playlistItemData":{"videoId":"abcdefghijk"},
            "menu":{"menuRenderer":{"items":[{"menuNavigationItemRenderer":{"navigationEndpoint":{
                "watchEndpoint":{"videoId":"lmnopqrstuv", "playlistId":"RDAMVMlmnopqrstuv", "params":"wAEB"}
            }}}]}}
        }});
        let Some(Item::Track(valid)) = parse_item(&row) else {
            panic!("expected a song")
        };
        assert!(
            valid.details.watch.is_some(),
            "Menu destinations are not the row's click target"
        );
        for (field, value) in [
            ("videoId", json!("lmnopqrstuv")),
            ("playlistId", json!("https://example.invalid/playlist")),
            ("playlistId", json!("x".repeat(257))),
            ("playlistId", json!("")),
            ("playlistId", json!(7)),
            ("params", json!("invalid%XX")),
            ("params", json!("unsafe\nvalue")),
            ("params", json!("x".repeat(8193))),
            ("params", json!(7)),
            ("index", json!(-1)),
            ("index", json!(u64::from(u32::MAX) + 1)),
            ("index", json!("2")),
            (
                "watchEndpointMusicSupportedConfigs",
                json!({"watchEndpointMusicConfig":{"musicVideoType":"MUSIC_VIDEO_TYPE_ATV\n"}}),
            ),
        ] {
            let mut invalid = row.clone();
            invalid["musicResponsiveListItemRenderer"]["title"]["runs"][0]["navigationEndpoint"]
                ["watchEndpoint"][field] = value;
            let Some(Item::Track(track)) = parse_item(&invalid) else {
                panic!("expected a song")
            };
            assert!(
                track.details.watch.is_none(),
                "Invalid {field} must not become a destination"
            );
        }
        let mut conflicting = row.clone();
        conflicting["musicResponsiveListItemRenderer"]["overlay"] = json!({"musicItemThumbnailOverlayRenderer":{"content":{"musicPlayButtonRenderer":{
            "playNavigationEndpoint":{"watchEndpoint":{"videoId":"lmnopqrstuv"}}
        }}}});
        let Some(Item::Track(track)) = parse_item(&conflicting) else {
            panic!("expected a song")
        };
        assert!(track.details.watch.is_none());
        let mut menu_only = row;
        menu_only["musicResponsiveListItemRenderer"]["title"] = json!({"simpleText":"Song"});
        let Some(Item::Track(track)) = parse_item(&menu_only) else {
            panic!("expected a song")
        };
        assert!(
            track.details.watch.is_none(),
            "An unrelated menu must never supply a click destination"
        );
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
    fn playlist_editability_requires_its_own_editor_and_is_not_persisted() {
        let row = |editor_id| {
            json!({"musicTwoRowItemRenderer":{
                "title":{"runs":[{"text":"Saved playlist","navigationEndpoint":{
                    "browseEndpoint":{"browseId":"VLPLsaved"}
                }}]},
                "menu":{"menuRenderer":{"items":[{
                    "menuNavigationItemRenderer":{"navigationEndpoint":{
                        "playlistEditorEndpoint":{"playlistId":editor_id}
                    }}
                }]}}
            }})
        };
        for (editor_id, editable) in [
            (Some("PLsaved"), true),
            (Some("PLother"), false),
            (None, false),
        ] {
            let Some(Item::Collection(collection)) = parse_item(&row(editor_id)) else {
                panic!("expected a playlist");
            };
            assert_eq!(collection.details.editable, editable);
            let mut stored = serde_json::to_value(&collection).unwrap();
            assert!(stored["details"].get("editable").is_none());
            stored["details"]["editable"] = json!(true);
            let restored: Collection = serde_json::from_value(stored).unwrap();
            assert!(!restored.details.editable);
        }
        let mut unrelated = row(Some("PLsaved"));
        unrelated["musicTwoRowItemRenderer"]["title"]["runs"][0]["navigationEndpoint"]["browseEndpoint"]
            ["browseId"] = json!("MPREalbum");
        assert!(
            matches!(parse_item(&unrelated), Some(Item::Collection(collection)) if !collection.details.editable)
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
        let details = track_details(&row, &[], "abcdefghijk");
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
        let details = track_details(&row, &[], "abcdefghijk");
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
        assert_eq!(
            track_details(&row, &[], "abcdefghijk").rating,
            Some(Rating::Indifferent)
        );
        row["menu"]["menuRenderer"]["items"][0]["toggleMenuServiceItemRenderer"]["isToggled"] =
            json!(true);
        assert_eq!(
            track_details(&row, &[], "abcdefghijk").rating,
            Some(Rating::Like)
        );
        let toggle = &mut row["menu"]["menuRenderer"]["items"][0]["toggleMenuServiceItemRenderer"];
        toggle["isToggled"] = json!(false);
        toggle["defaultServiceEndpoint"]["likeEndpoint"]["status"] = json!("INDIFFERENT");
        assert_eq!(
            track_details(&row, &[], "abcdefghijk").rating,
            Some(Rating::Like)
        );
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
            ..Default::default()
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
    #[ignore = "reads public artist, album and search display metadata without an account"]
    fn live_public_visual_metadata() {
        let catalog = Catalog::new().unwrap();
        let artist = catalog
            .browse(&BrowseLink::new("UCRr1xG_2WIDs18a6cIiCxeA", "Daft Punk"))
            .unwrap();
        assert!(
            artist
                .tracks()
                .iter()
                .any(|track| track.details.plays.is_some())
        );
        let album = catalog
            .browse(&BrowseLink::new(
                "MPREb_K8qWMWVqXGi",
                "Random Access Memories",
            ))
            .unwrap();
        let header = album.header.unwrap();
        assert!(
            matches!(header.artist, Some(HeaderArtist::Link(artist)) if artist.id == "UCRr1xG_2WIDs18a6cIiCxeA")
        );
        assert!(header.subtitle.contains("Album"));
        assert!(!header.stats.is_empty());
        let search = catalog
            .search_filtered(
                "Daft Punk",
                crate::search::SearchFilter::All,
                crate::search::SearchScope::Catalog,
            )
            .unwrap();
        let featured = search
            .shelves
            .iter()
            .find(|shelf| shelf.layout == ShelfLayout::Featured)
            .expect("Public search must retain its featured result");
        assert!(
            matches!(featured.items.first(), Some(Item::Collection(collection)) if collection.id == "UCRr1xG_2WIDs18a6cIiCxeA")
        );
        assert!(
            featured
                .items
                .iter()
                .skip(1)
                .any(|item| matches!(item, Item::Track(_)))
        );
        println!("Public artist counts, album artist/stats and featured-search metadata passed");
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
    #[ignore = "reads a public playlist and its continuation without an account"]
    fn live_public_playlist_continuation() {
        let api = Catalog::new().unwrap();
        let first = api
            .browse(&BrowseLink::new(
                "VLPL_yH9Q5syvqggJSHEZsd4itCiA42Q6vJr",
                "LAS 1000 MEJORES CANCIONES DE LA HISTORIA",
            ))
            .unwrap();
        let second = api.more(first.continuation.as_ref().unwrap()).unwrap();
        assert!(!first.tracks().is_empty());
        assert!(!second.tracks().is_empty());
        assert!(second.continuation.is_some());
        println!(
            "Playlist: {} initial tracks, {} continued tracks with another page",
            first.tracks().len(),
            second.tracks().len()
        );
    }

    #[test]
    #[ignore = "reads a public podcast episode description with issued chapter links"]
    fn live_public_episode_chapters() {
        let catalog = Catalog::new().unwrap();
        let page = catalog
            .browse(&BrowseLink::new("MPEDK5LhnIlxN4g", "Episode"))
            .unwrap();
        let track = page.tracks().pop().unwrap();
        let chapters = &track.details.episode.as_ref().unwrap().chapters;
        assert_eq!(track.id, "K5LhnIlxN4g");
        assert!(chapters.len() >= 2);
        assert_eq!(chapters[0].start_seconds, 0);
        assert!(chapters.iter().all(|chapter| !chapter.title.is_empty()));
        let starts: Vec<_> = chapters
            .iter()
            .map(|chapter| chapter.start_seconds)
            .collect();
        assert_eq!(starts.iter().collect::<HashSet<_>>().len(), starts.len());
        println!("Public episode chapter offsets: {starts:?}");
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
