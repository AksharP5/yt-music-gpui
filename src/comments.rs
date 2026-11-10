//! Read-only comment continuations. Mutation endpoints and commands never leave this boundary.

use crate::{
    auth::Session,
    catalog::{Catalog, Continuation, find, text, thumbnail},
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{collections::HashMap, sync::Arc};

#[derive(Clone, Debug)]
pub struct Comment {
    pub id: String,
    pub author: String,
    pub avatar: Option<String>,
    pub published: String,
    pub text: String,
    pub likes: String,
    pub liked: bool,
    pub creator: bool,
    pub verified: bool,
    pub pinned: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Thread {
    pub comment: Comment,
    pub replies_label: String,
    pub replies: Option<Continuation>,
    pub children: Vec<Thread>,
}

#[derive(Clone, Debug)]
pub struct Sort {
    pub label: String,
    pub selected: bool,
    pub continuation: Continuation,
}

#[derive(Clone, Debug, Default)]
pub struct Page {
    pub count: Option<String>,
    pub sorts: Vec<Sort>,
    pub comments: Vec<Thread>,
    pub continuation: Option<Continuation>,
    pub message: Option<String>,
}

pub(crate) fn watch_comments(
    response: &Value,
    session: Option<Arc<Session>>,
) -> Result<Option<Continuation>> {
    let tabs = response.pointer("/contents/singleColumnMusicWatchNextResultsRenderer/tabbedRenderer/watchNextTabbedResultsRenderer/tabs")
        .and_then(Value::as_array);
    let token = tabs.into_iter().flatten().find_map(|tab| {
        let tab = &tab["tabRenderer"];
        if tab["unselectable"] == true {
            return None;
        }
        tab.pointer("/content/sectionListRenderer/continuations")?
            .as_array()?
            .iter()
            .find_map(|entry| {
                entry
                    .pointer("/reloadContinuationData/continuation")?
                    .as_str()
            })
    });
    token
        .map(|token| Continuation::from_response(token, response, session))
        .transpose()
}

impl Catalog {
    pub fn comments(&self, continuation: &Continuation) -> Result<Page> {
        let response = self.request_with_continuation(
            "next",
            json!({"continuation":continuation.token()}),
            Some(continuation),
        )?;
        parse_page(&response, self.session.clone())
    }
}

fn continuation(
    value: &Value,
    response: &Value,
    session: Option<Arc<Session>>,
) -> Result<Option<Continuation>> {
    let token = value
        .pointer("/continuationItemRenderer/continuationEndpoint/continuationCommand/token")
        .or_else(|| {
            value.pointer(
                "/continuationItemRenderer/button/buttonRenderer/command/continuationCommand/token",
            )
        })
        .or_else(|| value.pointer("/reloadContinuationData/continuation"))
        .or_else(|| value.pointer("/nextContinuationData/continuation"))
        .and_then(Value::as_str);
    token
        .map(|token| Continuation::from_response(token, response, session))
        .transpose()
}

/// Join modern entity payloads to ordered thread renderers. Older guest responses may
/// still carry commentRenderer inline; both produce the same read-only values.
pub(crate) fn parse_page(response: &Value, session: Option<Arc<Session>>) -> Result<Page> {
    let entities: HashMap<_, _> = response
        .pointer("/frameworkUpdates/entityBatchUpdate/mutations")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|mutation| Some((mutation["entityKey"].as_str()?, &mutation["payload"])))
        .collect();
    let mut page = Page::default();
    let mut recognized = false;
    for key in ["onResponseReceivedEndpoints", "onResponseReceivedActions"] {
        for action in response[key].as_array().into_iter().flatten() {
            for command in [
                "reloadContinuationItemsCommand",
                "appendContinuationItemsAction",
            ] {
                if let Some(items) = action
                    .pointer(&format!("/{command}/continuationItems"))
                    .and_then(Value::as_array)
                {
                    recognized = true;
                    parse_items(items, response, &session, &entities, &mut page)?;
                }
            }
        }
    }
    for kind in ["itemSectionContinuation", "sectionListContinuation"] {
        if let Some(owner) = response.pointer(&format!("/continuationContents/{kind}")) {
            recognized = true;
            if let Some(items) = owner["contents"].as_array() {
                parse_items(items, response, &session, &entities, &mut page)?;
            }
            for item in owner["continuations"].as_array().into_iter().flatten() {
                page.continuation =
                    continuation(item, response, session.clone())?.or(page.continuation);
            }
        }
    }
    ensure!(recognized, "YouTube Music returned no readable comments");
    Ok(page)
}

fn parse_items(
    items: &[Value],
    response: &Value,
    session: &Option<Arc<Session>>,
    entities: &HashMap<&str, &Value>,
    page: &mut Page,
) -> Result<()> {
    for item in items {
        if let Some(header) = item.get("commentsHeaderRenderer") {
            let count = text(&header["countText"]);
            if !count.is_empty() {
                page.count = Some(count);
            }
            for sort in header
                .pointer("/sortMenu/sortFilterSubMenuRenderer/subMenuItems")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(token) = continuation(&sort["continuation"], response, session.clone())?
                {
                    page.sorts.push(Sort {
                        label: sort["title"].as_str().unwrap_or("Top comments").to_owned(),
                        selected: sort["selected"] == true,
                        continuation: token,
                    });
                }
            }
        } else if let Some(thread) = item.get("commentThreadRenderer") {
            let comment = parse_comment(thread, entities)?;
            let replies = &thread["replies"]["commentRepliesRenderer"];
            let mut children = Page::default();
            if let Some(items) = replies["contents"]
                .as_array()
                .or_else(|| replies["subThreads"].as_array())
            {
                parse_items(items, response, session, entities, &mut children)?;
            }
            page.comments.push(Thread {
                comment,
                replies_label: text(&replies["viewReplies"]["buttonRenderer"]["text"]),
                replies: children.continuation,
                children: children.comments,
            });
        } else if item.get("commentViewModel").is_some() || item.get("commentRenderer").is_some() {
            page.comments.push(Thread {
                comment: parse_comment(item, entities)?,
                replies_label: String::new(),
                replies: None,
                children: Vec::new(),
            });
        } else if let Some(token) = continuation(item, response, session.clone())? {
            page.continuation = Some(token);
        } else if let Some(message) = item.get("messageRenderer") {
            page.message = Some(text(&message["text"]));
        } else if let Some(contents) = item
            .pointer("/itemSectionRenderer/contents")
            .and_then(Value::as_array)
        {
            parse_items(contents, response, session, entities, page)?;
        }
    }
    Ok(())
}

fn parse_comment(value: &Value, entities: &HashMap<&str, &Value>) -> Result<Comment> {
    if let Some(old) = value
        .pointer("/comment/commentRenderer")
        .or_else(|| value.get("commentRenderer"))
    {
        return Ok(Comment {
            id: old["commentId"]
                .as_str()
                .context("Comment has no ID")?
                .to_owned(),
            author: text(&old["authorText"]),
            avatar: thumbnail(&old["authorThumbnail"]),
            published: text(&old["publishedTimeText"]),
            text: text(&old["contentText"]),
            likes: text(&old["voteCount"]),
            liked: old["isLiked"] == true,
            creator: old["authorIsChannelOwner"] == true,
            verified: old.get("authorCommentBadge").is_some(),
            pinned: old
                .pointer("/pinnedCommentBadge/pinnedCommentBadgeRenderer/label")
                .map(text),
        });
    }
    let model = value
        .pointer("/commentViewModel/commentViewModel")
        .or_else(|| value.get("commentViewModel"))
        .context("YouTube Music returned an unreadable comment")?;
    let key = model["commentKey"]
        .as_str()
        .context("Comment has no entity key")?;
    let entity = entities
        .get(key)
        .and_then(|payload| payload.get("commentEntityPayload"))
        .context("YouTube Music omitted a comment entity")?;
    let properties = &entity["properties"];
    let author = &entity["author"];
    let toolbar = &entity["toolbar"];
    let state = properties["toolbarStateKey"]
        .as_str()
        .and_then(|key| entities.get(key))
        .map(|payload| &payload["engagementToolbarStateEntityPayload"]);
    let pinned = model
        .pointer("/pinnedText")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| find(model, "pinnedCommentBadgeRenderer").map(|badge| text(&badge["label"])))
        .filter(|text| !text.is_empty())
        .or_else(|| (properties["isPinned"] == true).then(|| "Pinned".to_owned()));
    let liked = state.is_some_and(|state| state["likeState"] == "TOOLBAR_LIKE_STATE_LIKED");
    Ok(Comment {
        id: properties["commentId"]
            .as_str()
            .context("Comment has no ID")?
            .to_owned(),
        author: author["displayName"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        avatar: author["avatarThumbnailUrl"]
            .as_str()
            .filter(|url| url.starts_with("https://"))
            .map(str::to_owned),
        published: properties["publishedTime"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        text: properties
            .pointer("/content/content")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        likes: toolbar[if liked {
            "likeCountLiked"
        } else {
            "likeCountNotliked"
        }]
        .as_str()
        .unwrap_or_default()
        .to_owned(),
        liked,
        creator: author["isCreator"] == true,
        verified: author["isVerified"] == true,
        pinned,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Value {
        serde_json::from_str(match name {
            "initial" => include_str!("fixtures/comments/initial.json"),
            "replies" => include_str!("fixtures/comments/replies.json"),
            "continuation" => include_str!("fixtures/comments/continuation.json"),
            "disabled" => include_str!("fixtures/comments/disabled.json"),
            _ => unreachable!(),
        })
        .unwrap()
    }

    #[test]
    fn comments_join_entities_in_thread_order_and_keep_only_read_data() {
        let page = parse_page(&fixture("initial"), None).unwrap();
        assert_eq!(page.count.as_deref(), Some("1,234 Comments"));
        assert_eq!(
            page.comments
                .iter()
                .map(|thread| thread.comment.id.as_str())
                .collect::<Vec<_>>(),
            ["top-comment", "second-comment"]
        );
        let first = &page.comments[0];
        assert_eq!(first.comment.author, "@artist");
        assert!(
            first
                .comment
                .text
                .starts_with("A song for everyone.\nThanks")
        );
        assert!(first.comment.liked && first.comment.creator && first.comment.verified);
        assert_eq!(first.comment.pinned.as_deref(), Some("Pinned by @artist"));
        assert_eq!(first.comment.likes, "1.2K");
        assert_eq!(first.replies_label, "2 replies");
        assert_eq!(first.replies.as_ref().unwrap().token(), "replies-page");
        assert_eq!(page.continuation.unwrap().token(), "comments-page-2");
        assert_eq!(page.sorts[1].label, "Newest");
        assert_eq!(page.sorts[1].continuation.token(), "new-sort");
        assert!(page.sorts[0].selected);
    }

    #[test]
    fn replies_and_paging_keep_their_own_continuations() {
        let replies = parse_page(&fixture("replies"), None).unwrap();
        assert_eq!(
            replies.comments[0].comment.text,
            "Thank you for sharing this."
        );
        assert_eq!(replies.continuation.unwrap().token(), "replies-page-2");
        assert!(replies.count.is_none() && replies.sorts.is_empty());
        let nested = &replies.comments[0].children[0];
        assert_eq!(nested.comment.text, "A reply to the reply.");
        assert_eq!(nested.replies.as_ref().unwrap().token(), "nested-tail");
        let more = parse_page(&fixture("continuation"), None).unwrap();
        assert_eq!(more.comments[0].comment.author, "@guest");
        assert_eq!(more.comments[0].comment.text, "A classic.");
        assert_eq!(more.continuation.unwrap().token(), "comments-page-3");
    }

    #[test]
    fn disabled_and_empty_comments_are_valid_but_missing_entities_are_errors() {
        let page = parse_page(&fixture("disabled"), None).unwrap();
        assert_eq!(page.message.as_deref(), Some("Comments are turned off."));
        assert!(page.comments.is_empty() && page.continuation.is_none());
        assert!(parse_page(&json!({"onResponseReceivedEndpoints":[{"reloadContinuationItemsCommand":{"continuationItems":[]}}]}), None).unwrap().comments.is_empty());
        let mut broken = fixture("initial");
        broken["frameworkUpdates"] = Value::Null;
        assert!(parse_page(&broken, None).is_err());
        assert!(parse_page(&json!({}), None).is_err());
    }

    #[test]
    fn watch_uses_the_comments_section_token_and_honors_unselectable() {
        let response = |disabled| {
            json!({"contents":{"singleColumnMusicWatchNextResultsRenderer":{"tabbedRenderer":{"watchNextTabbedResultsRenderer":{"tabs":[
                {"tabRenderer":{"title":"Localized comments", "unselectable":disabled,"content":{"sectionListRenderer":{"targetId":"abcdefghijk","continuations":[{"reloadContinuationData":{"continuation":"watch-comments"}}]}}}}
            ]}}}}})
        };
        assert_eq!(
            watch_comments(&response(false), None)
                .unwrap()
                .unwrap()
                .token(),
            "watch-comments"
        );
        assert!(watch_comments(&response(true), None).unwrap().is_none());
    }

    #[test]
    fn comments_tokens_cannot_cross_accounts_even_before_an_http_request() {
        let session = Arc::new(serde_json::from_value(json!({"cookie":"synthetic","playback_cookie":"synthetic","authuser":"0","data_sync_id":"first","client_version":"synthetic"})).unwrap());
        let page = parse_page(&fixture("initial"), Some(session)).unwrap();
        let error = Catalog::new()
            .unwrap()
            .comments(page.continuation.as_ref().unwrap())
            .unwrap_err();
        assert!(format!("{error:#}").contains("account changed"));
    }

    #[test]
    #[ignore = "reads public guest comments, sort, replies and pagination from YouTube Music"]
    fn live_public_comments() {
        let catalog = Catalog::new().unwrap();
        let response = catalog
            .request("next", json!({"videoId":"dQw4w9WgXcQ","isAudioOnly":true}))
            .unwrap();
        let token = watch_comments(&response, None)
            .unwrap()
            .expect("comments tab");
        let first = catalog.comments(&token).unwrap();
        assert!(!first.comments.is_empty());
        let more = catalog
            .comments(first.continuation.as_ref().unwrap())
            .unwrap();
        assert!(!more.comments.is_empty());
        assert_ne!(first.comments[0].comment.id, more.comments[0].comment.id);
        let reply = first
            .comments
            .iter()
            .find_map(|thread| thread.replies.as_ref())
            .unwrap();
        assert!(!catalog.comments(reply).unwrap().comments.is_empty());
        let newest = first
            .sorts
            .iter()
            .find(|sort| sort.label.contains("New"))
            .unwrap();
        assert!(
            !catalog
                .comments(&newest.continuation)
                .unwrap()
                .comments
                .is_empty()
        );
        let disabled = catalog
            .request("next", json!({"videoId":"XqZsoesa55w","isAudioOnly":true}))
            .unwrap();
        assert!(watch_comments(&disabled, None).unwrap().is_none());
    }
}
