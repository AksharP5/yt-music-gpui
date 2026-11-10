use crate::catalog::{Catalog, Continuation, Item, Page, parse_item, parse_page};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::ops::Range;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SearchFilter {
    #[default]
    All,
    Songs,
    Videos,
    Albums,
    Artists,
    Playlists,
    Podcasts,
    Episodes,
    CommunityPlaylists,
    FeaturedPlaylists,
    Profiles,
}

impl SearchFilter {
    pub const ALL: [Self; 8] = [
        Self::All,
        Self::Songs,
        Self::Videos,
        Self::Albums,
        Self::Artists,
        Self::Playlists,
        Self::Podcasts,
        Self::Episodes,
    ];

    /// Catalog categories, used when a response names none of its own.
    pub const CATALOG: [Self; 10] = [
        Self::All,
        Self::Songs,
        Self::Videos,
        Self::Albums,
        Self::Artists,
        Self::CommunityPlaylists,
        Self::FeaturedPlaylists,
        Self::Episodes,
        Self::Profiles,
        Self::Podcasts,
    ];

    fn from_label(label: &str) -> Option<Self> {
        Self::CATALOG
            .into_iter()
            .find(|filter| filter.label() == label)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Songs => "Songs",
            Self::Videos => "Videos",
            Self::Albums => "Albums",
            Self::Artists => "Artists",
            Self::Playlists => "Playlists",
            Self::Podcasts => "Podcasts",
            Self::Episodes => "Episodes",
            Self::CommunityPlaylists => "Community playlists",
            Self::FeaturedPlaylists => "Featured playlists",
            Self::Profiles => "Profiles",
        }
    }

    fn param(self) -> Option<&'static str> {
        match self {
            Self::All => None,
            Self::Songs => Some("II"),
            Self::Videos => Some("IQ"),
            Self::Albums => Some("IY"),
            Self::Artists => Some("Ig"),
            Self::Playlists => Some("Io"),
            Self::Podcasts => Some("JQ"),
            Self::Episodes => Some("JI"),
            Self::Profiles => Some("JY"),
            Self::CommunityPlaylists | Self::FeaturedPlaylists => None,
        }
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub enum SearchScope {
    #[default]
    Catalog,
    Library,
    Uploads,
}

impl SearchScope {
    pub const ALL: [Self; 3] = [Self::Catalog, Self::Library, Self::Uploads];

    pub fn label(self) -> &'static str {
        match self {
            Self::Catalog => "Catalog",
            Self::Library => "Library",
            Self::Uploads => "Uploads",
        }
    }

    pub fn requires_account(self) -> bool {
        self != Self::Catalog
    }

    pub fn supported_filters(self) -> &'static [SearchFilter] {
        match self {
            Self::Catalog => &SearchFilter::CATALOG,
            Self::Library => &SearchFilter::ALL,
            Self::Uploads => &[SearchFilter::All],
        }
    }

    pub fn supports(self, filter: SearchFilter) -> bool {
        // The catalog still answers the combined Playlists category.
        self == Self::Catalog || self.supported_filters().contains(&filter)
    }

    fn tab_index(self, filter: SearchFilter) -> usize {
        // Older responses without tab identifiers put filtered results first.
        if filter != SearchFilter::All {
            return 0;
        }
        match self {
            Self::Catalog => 0,
            Self::Library => 1,
            Self::Uploads => 2,
        }
    }

    fn tab_id(self) -> &'static str {
        match self {
            Self::Catalog => "music_search_catalog",
            Self::Library => "music_search_library",
            Self::Uploads => "music_search_upload",
        }
    }
}

impl Catalog {
    pub fn search_filtered(
        &self,
        query: &str,
        filter: SearchFilter,
        scope: SearchScope,
    ) -> Result<Page> {
        self.search_results(query, filter, scope, None)
    }

    pub fn search_more(
        &self,
        query: &str,
        filter: SearchFilter,
        scope: SearchScope,
        continuation: &Continuation,
    ) -> Result<Page> {
        self.search_results(query, filter, scope, Some(continuation))
    }

    fn search_results(
        &self,
        query: &str,
        filter: SearchFilter,
        scope: SearchScope,
        continuation: Option<&Continuation>,
    ) -> Result<Page> {
        let body = search_request(query, filter, scope, continuation.map(Continuation::token))?;
        ensure!(
            !scope.requires_account() || self.session.is_some(),
            "Connect your Google account to search your library or uploads"
        );
        let response = self.request_with_continuation("search", body, continuation)?;
        self.bind_page(
            parse_search_page(&response, query.trim(), filter, scope)?,
            &response,
            continuation,
        )
    }

    pub fn suggestions(&self, query: &str) -> Result<Vec<Suggestion>> {
        let response = self.request(
            "music/get_search_suggestions",
            json!({"input": query.trim()}),
        )?;
        parse_suggestions(&response)
    }
}

fn search_request(
    query: &str,
    filter: SearchFilter,
    scope: SearchScope,
    continuation: Option<&str>,
) -> Result<Value> {
    let query = query.trim();
    ensure!(
        !query.is_empty(),
        "Enter a song, artist, album, or playlist"
    );
    ensure!(
        scope.supports(filter),
        "Uploads search does not support category filters"
    );
    let params = match (scope, filter.param()) {
        (SearchScope::Catalog, _) if filter == SearchFilter::Playlists => {
            Some("Eg-KAQwIABAAGAAgACgBMABqChAEEAMQCRAFEAo%3D".into())
        }
        (SearchScope::Catalog, _) if filter == SearchFilter::CommunityPlaylists => {
            Some("EgeKAQQoAEABagwQDhAKEAMQBBAJEAU%3D".into())
        }
        (SearchScope::Catalog, _) if filter == SearchFilter::FeaturedPlaylists => {
            Some("EgeKAQQoADgBagwQDhAKEAMQBBAJEAU%3D".into())
        }
        (SearchScope::Catalog, None) => None,
        (SearchScope::Catalog, Some(param)) => Some(format!("EgWKAQ{param}AWoMEA4QChADEAQQCRAF")),
        (SearchScope::Library, None) => Some("agIYBA%3D%3D".into()),
        (SearchScope::Library, Some(param)) => {
            Some(format!("EgWKAQ{param}AWoKEAUQCRADEAoYBA%3D%3D"))
        }
        (SearchScope::Uploads, _) => Some("agIYAw%3D%3D".into()),
    };
    let mut body = json!({"query": query});
    if let Some(params) = params {
        body["params"] = json!(params);
    }
    if let Some(continuation) = continuation {
        body["continuation"] = json!(continuation);
    }
    Ok(body)
}

fn parse_search_page(
    response: &Value,
    query: &str,
    filter: SearchFilter,
    scope: SearchScope,
) -> Result<Page> {
    ensure!(
        !scope.requires_account()
            || (response["LOGGED_IN"] != false
                && response.pointer("/responseContext/mainAppWebResponseContext/loggedOut")
                    != Some(&json!(true))),
        "YouTube Music did not accept the account session. Connect again."
    );
    let title = format!("Results for \"{query}\"");
    if let Some(contents) = response.get("continuationContents") {
        return parse_page(contents, &title);
    }
    let Some(contents) = response.get("contents") else {
        return parse_page(&json!({"contents": []}), &title);
    };
    let results = if let Some(tabbed) = contents.get("tabbedSearchResultsRenderer") {
        let tabs = tabbed["tabs"]
            .as_array()
            .context("YouTube Music did not return search tabs")?;
        let tab = if tabs
            .iter()
            .any(|tab| tab["tabRenderer"].get("tabIdentifier").is_some())
        {
            tabs.iter()
                .find(|tab| tab["tabRenderer"]["tabIdentifier"] == scope.tab_id())
        } else {
            tabs.get(scope.tab_index(filter))
        };
        tab.and_then(|tab| tab.get("tabRenderer"))
            .filter(|tab| tab["selected"] != false)
            .and_then(|tab| tab.get("content"))
            .context("YouTube Music did not return the requested search scope")?
    } else {
        ensure!(
            !scope.requires_account(),
            "YouTube Music did not return the requested search scope"
        );
        contents
    };
    let mut page = parse_page(results, &title)?;
    // The service orders categories by relevance to each query.
    page.search_filters = results
        .pointer("/sectionListRenderer/header/chipCloudRenderer/chips")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|chip| {
            SearchFilter::from_label(&crate::catalog::text(
                &chip["chipCloudChipRenderer"]["text"],
            ))
        })
        .collect();
    Ok(page)
}

/// One row of the search box dropdown, in the service's order.
#[derive(Clone, Debug)]
pub enum Suggestion {
    /// A query to search for, shown with the service's bold ranges.
    Query {
        query: String,
        label: String,
        bold: Vec<Range<usize>>,
        history: bool,
    },
    /// An artist, album, song or video to open directly.
    Item(Box<Item>),
}

fn parse_suggestions(response: &Value) -> Result<Vec<Suggestion>> {
    let mut suggestions = Vec::new();
    for section in response["contents"].as_array().into_iter().flatten() {
        for item in section
            .pointer("/searchSuggestionsSectionRenderer/contents")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let (renderer, history) = match item
                .get("historySuggestionRenderer")
                .map(|renderer| (renderer, true))
                .or_else(|| {
                    item.get("searchSuggestionRenderer")
                        .map(|renderer| (renderer, false))
                }) {
                Some(found) => found,
                None => {
                    suggestions
                        .extend(parse_item(item).map(|item| Suggestion::Item(Box::new(item))));
                    continue;
                }
            };
            let query = renderer
                .pointer("/navigationEndpoint/searchEndpoint/query")
                .and_then(Value::as_str)
                .filter(|query| !query.trim().is_empty())
                .context("YouTube Music returned an unreadable search suggestion")?;
            let mut label = String::new();
            let mut bold = Vec::new();
            for run in renderer
                .pointer("/suggestion/runs")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let text = run["text"].as_str().unwrap_or_default();
                if run["bold"] == true {
                    bold.push(label.len()..label.len() + text.len());
                }
                label.push_str(text);
            }
            if label.trim().is_empty() {
                label = query.to_owned();
                bold.clear();
            }
            suggestions.push(Suggestion::Query {
                query: query.to_owned(),
                label,
                bold,
                history,
            });
        }
    }
    Ok(suggestions)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tab(name: &str, token: &str, selected: bool) -> Value {
        json!({"tabRenderer":{"selected":selected,"content":{"sectionListRenderer":{"contents":[{
            "musicShelfRenderer":{"title":{"simpleText":name},"contents":[{
                "musicResponsiveListItemRenderer":{
                    "title":{"simpleText":name},
                    "navigationEndpoint":{"browseEndpoint":{"browseId":format!("VL{name}")}}
                }
            }],"continuations":[{"nextContinuationData":{"continuation":token}}]}
        }]}}}})
    }

    #[test]
    fn library_category_and_scope_params_survive_pagination() {
        for (filter, params) in [
            (SearchFilter::All, "agIYBA%3D%3D"),
            (SearchFilter::Songs, "EgWKAQIIAWoKEAUQCRADEAoYBA%3D%3D"),
            (SearchFilter::Videos, "EgWKAQIQAWoKEAUQCRADEAoYBA%3D%3D"),
            (SearchFilter::Albums, "EgWKAQIYAWoKEAUQCRADEAoYBA%3D%3D"),
            (SearchFilter::Artists, "EgWKAQIgAWoKEAUQCRADEAoYBA%3D%3D"),
            (SearchFilter::Playlists, "EgWKAQIoAWoKEAUQCRADEAoYBA%3D%3D"),
            (SearchFilter::Podcasts, "EgWKAQJQAWoKEAUQCRADEAoYBA%3D%3D"),
            (SearchFilter::Episodes, "EgWKAQJIAWoKEAUQCRADEAoYBA%3D%3D"),
        ] {
            let first = search_request(" a song ", filter, SearchScope::Library, None).unwrap();
            assert_eq!(first, json!({"query":"a song","params":params}));
            let more = search_request(
                " a song ",
                filter,
                SearchScope::Library,
                Some("library-token"),
            )
            .unwrap();
            assert_eq!(
                more,
                json!({"query":"a song","params":params,"continuation":"library-token"})
            );
        }
        assert_eq!(
            search_request(
                "a playlist",
                SearchFilter::Playlists,
                SearchScope::Catalog,
                None
            )
            .unwrap()["params"],
            "Eg-KAQwIABAAGAAgACgBMABqChAEEAMQCRAFEAo%3D"
        );
        assert_eq!(
            search_request("an upload", SearchFilter::All, SearchScope::Uploads, None).unwrap()["params"],
            "agIYAw%3D%3D"
        );
    }

    #[test]
    fn catalog_categories_keep_the_services_order_and_skip_the_clear_chip() {
        let chip =
            |label: &str| json!({"chipCloudChipRenderer":{"text":{"runs":[{"text":label}]}}});
        let response = json!({"contents":{"tabbedSearchResultsRenderer":{"tabs":[{"tabRenderer":{
            "tabIdentifier":"music_search_catalog",
            "content":{"sectionListRenderer":{
                "header":{"chipCloudRenderer":{"chips":[
                    {"chipCloudChipRenderer":{"icon":{"iconType":"CLOSE"}}},
                    chip("Videos"), chip("Community playlists"), chip("Songs"), chip("Profiles")
                ]}},
                "contents":[]
            }}
        }}]}}});
        let page = parse_search_page(
            &response,
            "a song",
            SearchFilter::Videos,
            SearchScope::Catalog,
        )
        .unwrap();
        assert_eq!(
            page.search_filters,
            [
                SearchFilter::Videos,
                SearchFilter::CommunityPlaylists,
                SearchFilter::Songs,
                SearchFilter::Profiles
            ]
        );
        assert_eq!(
            search_request("a song", SearchFilter::Profiles, SearchScope::Catalog, None).unwrap()["params"],
            "EgWKAQJYAWoMEA4QChADEAQQCRAF"
        );
    }

    #[test]
    fn account_tabs_never_fall_back_to_public_or_stale_scope_contents() {
        let mut response = json!({"contents":{"tabbedSearchResultsRenderer":{"tabs":[
            tab("Public", "public-token", false),
            tab("Library", "library-token", true),
            tab("Uploads", "uploads-token", false)
        ]}}});
        let library =
            parse_search_page(&response, "test", SearchFilter::All, SearchScope::Library).unwrap();
        assert_eq!(library.shelves.len(), 1);
        assert_eq!(library.shelves[0].items[0].title(), "Library");
        assert_eq!(
            library.continuation.as_ref().map(Continuation::token),
            Some("library-token")
        );
        assert!(
            parse_search_page(&response, "test", SearchFilter::All, SearchScope::Catalog).is_err()
        );
        assert!(
            parse_search_page(&response, "test", SearchFilter::Songs, SearchScope::Library)
                .is_err()
        );

        let tabs = &mut response["contents"]["tabbedSearchResultsRenderer"]["tabs"];
        tabs[1]["tabRenderer"]["selected"] = json!(false);
        tabs[2]["tabRenderer"]["selected"] = json!(true);
        let uploads =
            parse_search_page(&response, "test", SearchFilter::All, SearchScope::Uploads).unwrap();
        assert_eq!(uploads.shelves[0].items[0].title(), "Uploads");
        assert_eq!(
            uploads.continuation.as_ref().map(Continuation::token),
            Some("uploads-token")
        );

        let filtered = json!({"contents":{"tabbedSearchResultsRenderer":{"tabs":[
            tab("Library songs", "library-songs-token", true),
            tab("Stale library", "stale-library-token", false)
        ]}}});
        let songs = parse_search_page(&filtered, "test", SearchFilter::Songs, SearchScope::Library)
            .unwrap();
        assert_eq!(songs.shelves[0].items[0].title(), "Library songs");
        assert_eq!(
            songs.continuation.as_ref().map(Continuation::token),
            Some("library-songs-token")
        );

        let missing = json!({"contents":{"tabbedSearchResultsRenderer":{"tabs":[
            tab("Public", "public-token", true)
        ]}}});
        assert!(
            parse_search_page(&missing, "test", SearchFilter::All, SearchScope::Library).is_err()
        );
        for scope in [SearchScope::Library, SearchScope::Uploads] {
            assert!(
                parse_search_page(
                    &json!({"contents":{"musicShelfRenderer":{}}}),
                    "test",
                    SearchFilter::All,
                    scope
                )
                .is_err()
            );
        }

        let continuation = json!({"continuationContents":{"musicShelfContinuation":{
            "contents":[{"musicResponsiveListItemRenderer":{
                "title":{"simpleText":"More library results"},
                "navigationEndpoint":{"browseEndpoint":{"browseId":"VLmore"}}
            }}],
            "continuations":[{"nextContinuationData":{"continuation":"next-library-token"}}]
        }}});
        let more = parse_search_page(
            &continuation,
            "test",
            SearchFilter::Songs,
            SearchScope::Library,
        )
        .unwrap();
        assert_eq!(more.shelves[0].items[0].title(), "More library results");
        assert_eq!(
            more.continuation.as_ref().map(Continuation::token),
            Some("next-library-token")
        );
        let mut logged_out = continuation.clone();
        logged_out["responseContext"] = json!({"mainAppWebResponseContext":{"loggedOut":true}});
        let mut not_logged_in = continuation;
        not_logged_in["LOGGED_IN"] = json!(false);
        for rejected in [logged_out, not_logged_in] {
            assert!(
                parse_search_page(&rejected, "test", SearchFilter::Songs, SearchScope::Library)
                    .is_err()
            );
        }
    }

    #[test]
    fn identified_search_tabs_select_scope_independently_of_category_and_position() {
        let mut public = tab("Catalog", "public-token", false);
        public["tabRenderer"]["tabIdentifier"] = json!("music_search_catalog");
        let mut library = tab("Library songs", "library-token", true);
        library["tabRenderer"]["tabIdentifier"] = json!("music_search_library");
        for tabs in [vec![public.clone(), library.clone()], vec![library, public]] {
            let mut response = json!({"contents":{"tabbedSearchResultsRenderer":{"tabs":tabs}}});
            let songs =
                parse_search_page(&response, "test", SearchFilter::Songs, SearchScope::Library)
                    .unwrap();
            assert_eq!(songs.shelves[0].items[0].title(), "Library songs");
            assert_eq!(
                songs.continuation.as_ref().map(Continuation::token),
                Some("library-token")
            );
            for scope in [SearchScope::Catalog, SearchScope::Uploads] {
                assert!(parse_search_page(&response, "test", SearchFilter::All, scope).is_err());
            }
            for tab in response["contents"]["tabbedSearchResultsRenderer"]["tabs"]
                .as_array_mut()
                .unwrap()
            {
                tab["tabRenderer"]["tabIdentifier"] = json!("unknown_scope");
            }
            assert!(
                parse_search_page(&response, "test", SearchFilter::Songs, SearchScope::Library)
                    .is_err()
            );
        }
    }

    #[test]
    fn suggestions_use_canonical_queries_and_keep_server_order() {
        let response = json!({"contents":[
            {"searchSuggestionsSectionRenderer":{"contents":[
                {"historySuggestionRenderer":{
                    "navigationEndpoint":{"searchEndpoint":{"query":"a previous search"}},
                    "suggestion":{"runs":[{"text":"a ","bold":true},{"text":"previous search"}]},
                    "serviceEndpoint":{"feedbackEndpoint":{"feedbackToken":"not-a-query"}}
                }},
                {"searchSuggestionRenderer":{"navigationEndpoint":{"searchEndpoint":{"query":"A suggested search"}}}}
            ]}},
            {"searchSuggestionsSectionRenderer":{"contents":[
                {"musicResponsiveListItemRenderer":{
                    "flexColumns":[
                        {"musicResponsiveListItemFlexColumnRenderer":{"text":{"runs":[{"text":"An artist"}]}}},
                        {"musicResponsiveListItemFlexColumnRenderer":{"text":{"runs":[{"text":"1M monthly audience"}]}}}
                    ],
                    "navigationEndpoint":{"browseEndpoint":{"browseId":"UCartist"}}
                }},
                {"musicResponsiveListItemRenderer":{
                    "flexColumns":[
                        {"musicResponsiveListItemFlexColumnRenderer":{"text":{"runs":[{"text":"A song"}]}}},
                        {"musicResponsiveListItemFlexColumnRenderer":{"text":{"runs":[
                            {"text":"Song"},{"text":" • "},
                            {"text":"An artist","navigationEndpoint":{"browseEndpoint":{"browseId":"UCartist"}}}
                        ]}}}
                    ],
                    "playlistItemData":{"videoId":"abcdefghijk"}
                }}
            ]}}
        ]});
        let suggestions = parse_suggestions(&response).unwrap();
        let Suggestion::Query {
            query,
            label,
            bold,
            history: true,
        } = &suggestions[0]
        else {
            panic!("expected a history query first");
        };
        assert_eq!(query, "a previous search");
        assert_eq!(&label[bold[0].clone()], "a ");
        assert!(matches!(
            &suggestions[1],
            Suggestion::Query { query, label, history: false, .. }
                if query == "A suggested search" && label == query
        ));
        assert!(matches!(
            &suggestions[2],
            Suggestion::Item(item) if matches!(item.as_ref(), Item::Collection(artist) if artist.id == "UCartist")
        ));
        assert!(matches!(
            &suggestions[3],
            Suggestion::Item(item) if matches!(item.as_ref(), Item::Track(song) if song.id == "abcdefghijk" && song.artist == "An artist")
        ));
        assert!(
            parse_suggestions(&json!({"contents":[]}))
                .unwrap()
                .is_empty()
        );
        assert!(parse_suggestions(&json!({"contents":[{"searchSuggestionsSectionRenderer":{"contents":[
            {"searchSuggestionRenderer":{"suggestion":{"simpleText":"Missing canonical query"}}}
        ]}}]})).is_err());
    }

    #[test]
    fn invalid_combinations_and_guest_account_scopes_fail_before_http() {
        let catalog = Catalog::new().unwrap();
        for scope in [SearchScope::Library, SearchScope::Uploads] {
            assert!(
                catalog
                    .search_filtered("a song", SearchFilter::All, scope)
                    .unwrap_err()
                    .to_string()
                    .contains("Connect your Google account")
            );
        }
        let continuation = parse_page(
            &json!({"contents":[],"nextContinuationData":{"continuation":"token"}}),
            "Search",
        )
        .unwrap()
        .continuation
        .unwrap();
        for filter in SearchFilter::ALL
            .into_iter()
            .filter(|filter| *filter != SearchFilter::All)
        {
            assert!(!SearchScope::Uploads.supports(filter));
            assert!(
                catalog
                    .search_filtered("a song", filter, SearchScope::Uploads)
                    .unwrap_err()
                    .to_string()
                    .contains("does not support category filters")
            );
            assert!(
                catalog
                    .search_more("a song", filter, SearchScope::Uploads, &continuation)
                    .is_err()
            );
        }
        assert!(
            catalog
                .search_filtered("  ", SearchFilter::All, SearchScope::Catalog)
                .is_err()
        );
        let empty = parse_search_page(
            &json!({}),
            "no results",
            SearchFilter::All,
            SearchScope::Catalog,
        )
        .unwrap();
        assert!(empty.shelves.is_empty());
    }

    #[test]
    #[ignore = "uses the public YouTube Music search and suggestions endpoints"]
    fn live_public_search_and_suggestions() {
        let catalog = Catalog::new().unwrap();
        for (filter, query) in [
            (SearchFilter::All, "Khruangbin"),
            (SearchFilter::Songs, "Khruangbin"),
            (SearchFilter::Videos, "Khruangbin"),
            (SearchFilter::Albums, "Khruangbin"),
            (SearchFilter::Artists, "Khruangbin"),
            (SearchFilter::Playlists, "Khruangbin"),
            (SearchFilter::Podcasts, "science"),
            (SearchFilter::Episodes, "science"),
        ] {
            let page = catalog
                .search_filtered(query, filter, SearchScope::Catalog)
                .unwrap();
            assert!(
                !page.shelves.is_empty(),
                "{} returned no results",
                filter.label()
            );
            if filter == SearchFilter::Songs {
                let continuation = page
                    .continuation
                    .as_ref()
                    .expect("song search should paginate");
                let more = catalog
                    .search_more(query, filter, SearchScope::Catalog, continuation)
                    .unwrap();
                assert!(!more.tracks().is_empty());
            }
        }
        assert!(!catalog.suggestions("Khru").unwrap().is_empty());
    }
}
