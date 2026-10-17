use crate::catalog::{Catalog, Page, parse_page};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};

#[derive(Clone, Copy, Default, PartialEq, Eq)]
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
    Library,
    Uploads,
}

impl SearchFilter {
    pub const ALL: [Self; 10] = [
        Self::All,
        Self::Songs,
        Self::Videos,
        Self::Albums,
        Self::Artists,
        Self::Playlists,
        Self::Podcasts,
        Self::Episodes,
        Self::Library,
        Self::Uploads,
    ];

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
            Self::Library => "Library",
            Self::Uploads => "Uploads",
        }
    }

    pub fn requires_account(self) -> bool {
        matches!(self, Self::Library | Self::Uploads)
    }

    fn params(self) -> Option<&'static str> {
        match self {
            Self::All => None,
            Self::Songs => Some("EgWKAQIIAWoMEA4QChADEAQQCRAF"),
            Self::Videos => Some("EgWKAQIQAWoMEA4QChADEAQQCRAF"),
            Self::Albums => Some("EgWKAQIYAWoMEA4QChADEAQQCRAF"),
            Self::Artists => Some("EgWKAQIgAWoMEA4QChADEAQQCRAF"),
            Self::Playlists => Some("Eg-KAQwIABAAGAAgACgBMABqChAEEAMQCRAFEAo%3D"),
            Self::Podcasts => Some("EgWKAQJQAWoMEA4QChADEAQQCRAF"),
            Self::Episodes => Some("EgWKAQJIAWoMEA4QChADEAQQCRAF"),
            Self::Library => Some("agIYBA%3D%3D"),
            Self::Uploads => Some("agIYAw%3D%3D"),
        }
    }

    fn tab_index(self) -> usize {
        match self {
            Self::Library => 1,
            Self::Uploads => 2,
            _ => 0,
        }
    }
}

impl Catalog {
    pub fn search_filtered(&self, query: &str, filter: SearchFilter) -> Result<Page> {
        self.search_results(query, filter, None)
    }

    pub fn search_more(
        &self,
        query: &str,
        filter: SearchFilter,
        continuation: &str,
    ) -> Result<Page> {
        ensure!(
            !continuation.is_empty(),
            "No more search results are available"
        );
        self.search_results(query, filter, Some(continuation))
    }

    fn search_results(
        &self,
        query: &str,
        filter: SearchFilter,
        continuation: Option<&str>,
    ) -> Result<Page> {
        let query = query.trim();
        ensure!(
            !query.is_empty(),
            "Enter a song, artist, album, or playlist"
        );
        ensure!(
            !filter.requires_account() || self.session.is_some(),
            "Connect your Google account to search your library or uploads"
        );
        let mut body = json!({"query": query});
        if let Some(params) = filter.params() {
            body["params"] = json!(params);
        }
        if let Some(continuation) = continuation {
            body["continuation"] = json!(continuation);
        }
        let response = self.request("search", body)?;
        ensure!(
            !filter.requires_account()
                || response.pointer("/responseContext/mainAppWebResponseContext/loggedOut")
                    != Some(&json!(true)),
            "YouTube Music did not accept the account session. Connect again."
        );
        parse_search_page(&response, query, filter)
    }

    pub fn suggestions(&self, query: &str) -> Result<Vec<String>> {
        let response = self.request(
            "music/get_search_suggestions",
            json!({"input": query.trim()}),
        )?;
        parse_suggestions(&response)
    }
}

fn parse_search_page(response: &Value, query: &str, filter: SearchFilter) -> Result<Page> {
    let title = format!("Results for \"{query}\"");
    if let Some(contents) = response.get("continuationContents") {
        return parse_page(contents, &title);
    }
    let Some(contents) = response.get("contents") else {
        return parse_page(&json!({"contents": []}), &title);
    };
    let results = if let Some(tabbed) = contents.get("tabbedSearchResultsRenderer") {
        tabbed["tabs"]
            .as_array()
            .and_then(|tabs| tabs.get(filter.tab_index()))
            .and_then(|tab| tab.pointer("/tabRenderer/content"))
            .context("YouTube Music did not return the requested search scope")?
    } else {
        contents
    };
    parse_page(results, &title)
}

fn parse_suggestions(response: &Value) -> Result<Vec<String>> {
    let mut suggestions = Vec::new();
    for section in response["contents"].as_array().into_iter().flatten() {
        for item in section
            .pointer("/searchSuggestionsSectionRenderer/contents")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let renderer = item
                .get("historySuggestionRenderer")
                .or_else(|| item.get("searchSuggestionRenderer"));
            let Some(renderer) = renderer else { continue };
            let query = renderer
                .pointer("/navigationEndpoint/searchEndpoint/query")
                .and_then(Value::as_str)
                .filter(|query| !query.trim().is_empty())
                .context("YouTube Music returned an unreadable search suggestion")?;
            suggestions.push(query.to_owned());
        }
    }
    Ok(suggestions)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_scopes_never_mix_public_results_or_continuations() {
        let scope = |name, token| {
            json!({"tabRenderer":{"content":{"sectionListRenderer":{"contents":[{
                "musicShelfRenderer":{"title":{"simpleText":name},"contents":[{
                    "musicResponsiveListItemRenderer":{
                        "title":{"simpleText":name},
                        "navigationEndpoint":{"browseEndpoint":{"browseId":format!("VL{name}")}}
                    }
                }],"continuations":[{"nextContinuationData":{"continuation":token}}]}
            }]}}}})
        };
        let response = json!({"contents":{"tabbedSearchResultsRenderer":{"tabs":[
            scope("Public", "public-token"), scope("Library", "library-token"), scope("Uploads", "uploads-token")
        ]}}});
        let library = parse_search_page(&response, "test", SearchFilter::Library).unwrap();
        assert_eq!(library.shelves.len(), 1);
        assert_eq!(library.shelves[0].items[0].title(), "Library");
        assert_eq!(library.continuation.as_deref(), Some("library-token"));
        let uploads = parse_search_page(&response, "test", SearchFilter::Uploads).unwrap();
        assert_eq!(uploads.shelves[0].items[0].title(), "Uploads");
        assert_eq!(uploads.continuation.as_deref(), Some("uploads-token"));
        assert!(
            parse_search_page(
                &json!({"contents":{"tabbedSearchResultsRenderer":{"tabs":[]}}}),
                "test",
                SearchFilter::Library
            )
            .is_err()
        );
        let continuation = json!({"continuationContents":{"musicShelfContinuation":{
            "contents":[{"musicResponsiveListItemRenderer":{
                "title":{"simpleText":"More library results"},
                "navigationEndpoint":{"browseEndpoint":{"browseId":"VLmore"}}
            }}],
            "continuations":[{"nextContinuationData":{"continuation":"next-library-token"}}]
        }}});
        let more = parse_search_page(&continuation, "test", SearchFilter::Library).unwrap();
        assert_eq!(more.shelves[0].items[0].title(), "More library results");
        assert_eq!(more.continuation.as_deref(), Some("next-library-token"));
    }

    #[test]
    fn suggestions_use_canonical_queries_and_keep_server_order() {
        let response = json!({"contents":[{"searchSuggestionsSectionRenderer":{"contents":[
            {"historySuggestionRenderer":{
                "navigationEndpoint":{"searchEndpoint":{"query":"A previous search"}},
                "suggestion":{"runs":[{"text":"Different display formatting"}]},
                "serviceEndpoint":{"feedbackEndpoint":{"feedbackToken":"not-a-query"}}
            }},
            {"searchSuggestionRenderer":{"navigationEndpoint":{"searchEndpoint":{"query":"A suggested search"}}}},
            {"musicResponsiveListItemRenderer":{"title":{"simpleText":"A direct song result"}}}
        ]}}]});
        assert_eq!(
            parse_suggestions(&response).unwrap(),
            ["A previous search", "A suggested search"]
        );
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
    fn guest_search_rejects_account_scopes_before_any_request() {
        let catalog = Catalog::new().unwrap();
        for filter in [SearchFilter::Library, SearchFilter::Uploads] {
            assert!(
                catalog
                    .search_filtered("a song", filter)
                    .unwrap_err()
                    .to_string()
                    .contains("Connect your Google account")
            );
        }
        assert!(catalog.search_filtered("  ", SearchFilter::All).is_err());
        let empty = parse_search_page(&json!({}), "no results", SearchFilter::All).unwrap();
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
            let page = catalog.search_filtered(query, filter).unwrap();
            assert!(
                !page.shelves.is_empty(),
                "{} returned no results",
                filter.label()
            );
            if filter == SearchFilter::Songs {
                let continuation = page
                    .continuation
                    .as_deref()
                    .expect("song search should paginate");
                let more = catalog.search_more(query, filter, continuation).unwrap();
                assert!(!more.tracks().is_empty());
            }
        }
        assert!(!catalog.suggestions("Khru").unwrap().is_empty());
    }
}
