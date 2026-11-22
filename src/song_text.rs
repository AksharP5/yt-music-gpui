//! Song details loaded on demand: lyrics, comments, credits and related music.

use crate::{
    auth::{self, Session},
    catalog::{BrowseLink, Catalog, Page, text, thumbnail},
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    thread,
};

pub enum Query {
    Lyrics(String),
    Credits(String),
    Related(String),
    Comments(crate::catalog::Continuation),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Lyrics {
    pub text: String,
    /// The provider line YouTube Music shows under the lyrics, such as "Source: Musixmatch".
    pub source: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CreditSection {
    pub title: String,
    pub names: String,
}

/// A song's credits dialog, as YouTube Music sends it: its title ("Song
/// credits"), the song it is about, and the roles.
#[derive(Clone, Debug, PartialEq)]
pub struct Credits {
    pub title: String,
    pub song: Option<CreditedSong>,
    pub sections: Vec<CreditSection>,
}

/// The cover, title, artist and "Song • 2020" line above the credits.
#[derive(Clone, Debug, PartialEq)]
pub struct CreditedSong {
    pub title: String,
    pub artist: String,
    pub detail: String,
    pub thumbnail: Option<String>,
}

#[derive(Clone)]
pub enum Content {
    Lyrics(Lyrics),
    Credits(Credits),
    Related(Box<Page>),
    Comments(Box<crate::comments::Page>),
}

struct Request {
    id: u64,
    query: Query,
    session: Option<Arc<Session>>,
}

pub struct Event {
    pub id: u64,
    pub result: Result<Content, String>,
    pub session_rejected: bool,
}

pub struct Worker {
    requests: async_channel::Sender<Request>,
    pending: async_channel::Receiver<Request>,
    generation: Arc<AtomicU64>,
}

impl Worker {
    pub fn start() -> (Self, async_channel::Receiver<Event>) {
        let mut catalog = None;
        Self::spawn(move |request| {
            let catalog = catalog
                .get_or_insert_with(Catalog::new)
                .as_ref()
                .map_err(|error| anyhow::anyhow!("{error:#}"))?
                .for_session(request.session.clone());
            match &request.query {
                Query::Comments(token) => catalog
                    .comments(token)
                    .map(|page| Content::Comments(Box::new(page))),
                Query::Lyrics(id) => catalog.lyrics(id).map(Content::Lyrics),
                Query::Credits(id) => catalog.credits(id).map(Content::Credits),
                Query::Related(id) => catalog
                    .browse(&BrowseLink::new(id.clone(), "Related"))
                    .map(|page| Content::Related(Box::new(page))),
            }
        })
    }

    #[expect(
        clippy::expect_used,
        reason = "the app cannot run without this worker, and spawning one fails only when the OS is out of threads"
    )]
    fn spawn(
        mut load: impl FnMut(&Request) -> Result<Content> + Send + 'static,
    ) -> (Self, async_channel::Receiver<Event>) {
        let (requests, pending) = async_channel::bounded::<Request>(1);
        let (events, completed) = async_channel::unbounded();
        let generation = Arc::new(AtomicU64::new(0));
        let current = generation.clone();
        let jobs = pending.clone();
        thread::Builder::new()
            .name("song-text".into())
            .spawn(move || {
                while let Ok(request) = jobs.recv_blocking() {
                    if request.id != current.load(Ordering::Acquire) {
                        continue;
                    }
                    let result = load(&request);
                    let session_rejected = result.as_ref().is_err_and(auth::session_was_rejected);
                    let result = result.map_err(|error| format!("{error:#}"));
                    if request.id == current.load(Ordering::Acquire)
                        && events
                            .send_blocking(Event {
                                id: request.id,
                                result,
                                session_rejected,
                            })
                            .is_err()
                    {
                        break;
                    }
                }
            })
            .expect("start song text worker");
        (
            Self {
                requests,
                pending,
                generation,
            },
            completed,
        )
    }

    pub fn load(&self, query: Query, session: Option<Arc<Session>>) -> Result<u64> {
        let id = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        self.requests
            .force_send(Request { id, query, session })
            .map_err(|_| {
                anyhow::anyhow!(
                    "The song details connection stopped. Restart the app to reconnect."
                )
            })?;
        Ok(id)
    }

    pub fn cancel(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        let _ = self.pending.try_recv();
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.cancel();
        self.requests.close();
    }
}

impl Catalog {
    pub fn lyrics(&self, browse_id: &str) -> Result<Lyrics> {
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

    pub fn credits(&self, browse_id: &str) -> Result<Credits> {
        ensure!(
            browse_id.starts_with("MPTC")
                && browse_id.len() > 4
                && browse_id.len() <= 256
                && browse_id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte)),
            "This song has no valid credits ID"
        );
        let response = self.request("browse", json!({"browseId": browse_id}))?;
        parse_credits(&response)
    }
}

fn parse_lyrics(response: &Value) -> Result<Lyrics> {
    let shelf = response
        .pointer("/contents/sectionListRenderer/contents")
        .and_then(Value::as_array)
        .and_then(|sections| {
            sections
                .iter()
                .find_map(|section| section.get("musicDescriptionShelfRenderer"))
        })
        .context("Lyrics are not available for this song")?;
    let lyrics = text(&shelf["description"]);
    ensure!(
        !lyrics.trim().is_empty(),
        "Lyrics are not available for this song"
    );
    let source = Some(text(&shelf["footer"])).filter(|line| !line.trim().is_empty());
    Ok(Lyrics {
        text: lyrics,
        source,
    })
}

fn parse_credits(response: &Value) -> Result<Credits> {
    let dialog = response
        .get("onResponseReceivedActions")
        .and_then(Value::as_array)
        .and_then(|actions| {
            actions.iter().find_map(|action| {
                action.pointer("/openPopupAction/popup/dismissableDialogRenderer")
            })
        })
        .context("Credits are not available for this song")?;
    let sections = dialog["sections"]
        .as_array()
        .context("Credits are not available for this song")?;
    let mut credits = Vec::new();
    for section in sections {
        let Some(section) = section.get("dismissableDialogContentSectionRenderer") else {
            continue;
        };
        let title = text(&section["title"]);
        let names = text(&section["subtitle"]);
        if !title.trim().is_empty() && !names.trim().is_empty() {
            credits.push(CreditSection { title, names });
        }
    }
    ensure!(
        !credits.is_empty(),
        "Credits are not available for this song"
    );
    let title = dialog["title"]
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| text(&dialog["title"]));
    let song = dialog
        .pointer("/metadata/musicMultiRowListItemRenderer")
        .map(|song| CreditedSong {
            title: text(&song["title"]),
            artist: text(&song["subtitle"]),
            detail: text(&song["secondTitle"]),
            thumbnail: thumbnail(&song["thumbnail"]),
        })
        .filter(|song| !song.title.trim().is_empty());
    Ok(Credits {
        title: if title.trim().is_empty() {
            "Song credits".into()
        } else {
            title
        },
        song,
        sections: credits,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::mpsc, time::Duration};

    #[test]
    fn credits_keep_the_song_header_and_localized_roles_in_server_order() {
        let response = json!({"onResponseReceivedActions":[{}, {"openPopupAction":{"popup":{
            "dismissableDialogRenderer":{"title":"Song credits",
                "metadata":{"musicMultiRowListItemRenderer":{
                    "thumbnail":{"musicThumbnailRenderer":{"thumbnail":{"thumbnails":[
                        {"url":"https://example.com/w60","width":60,"height":60},
                        {"url":"https://example.com/w226","width":226,"height":226}]}}},
                    "title":{"runs":[{"text":"Future Nostalgia"}]},
                    "subtitle":{"runs":[{"text":"Dua Lipa"}]},
                    "secondTitle":{"runs":[{"text":"Song"},{"text":" • "},{"text":"2020"}]}}},
                "sections":[
                {"dismissableDialogContentSectionRenderer":{"title":{"simpleText":"Performed by"},
                    "subtitle":{"runs":[{"text":"First artist"},{"text":"\n"},{"text":"Second artist"}]}}},
                {"dismissableDialogContentSectionRenderer":{"title":{"simpleText":"Piano"},
                    "subtitle":{"simpleText":"Another performer"}}},
                {"dismissableDialogContentSectionRenderer":{"title":{"simpleText":"Empty role"}}}
            ]}
        }}}]});
        assert_eq!(
            parse_credits(&response).unwrap(),
            Credits {
                title: "Song credits".into(),
                song: Some(CreditedSong {
                    title: "Future Nostalgia".into(),
                    artist: "Dua Lipa".into(),
                    detail: "Song • 2020".into(),
                    thumbnail: Some("https://example.com/w226".into()),
                }),
                sections: vec![
                    CreditSection {
                        title: "Performed by".into(),
                        names: "First artist\nSecond artist".into()
                    },
                    CreditSection {
                        title: "Piano".into(),
                        names: "Another performer".into()
                    },
                ],
            }
        );
        assert!(parse_credits(&json!({})).is_err());
        let catalog = Catalog::new().unwrap();
        for invalid in [
            "MPTC",
            "VLplaylist",
            "MPTC../../private",
            "MPTCbad?token=private",
        ] {
            assert!(catalog.credits(invalid).is_err());
        }
    }

    #[test]
    fn lyrics_keep_line_breaks_and_their_source_line() {
        let response = json!({"contents":{"sectionListRenderer":{"contents":[{
            "musicDescriptionShelfRenderer":{
                "description":{"runs":[{"text":"First line\n"},{"text":"Second line\n"}]},
                "footer":{"runs":[{"text":"Source: Musixmatch"}]}
            }
        }]}}});
        assert_eq!(
            parse_lyrics(&response).unwrap(),
            Lyrics {
                text: "First line\nSecond line\n".into(),
                source: Some("Source: Musixmatch".into()),
            }
        );
        assert!(parse_lyrics(&json!({"contents":{}})).is_err());
    }

    #[test]
    fn cancelled_and_superseded_text_never_replace_the_latest_request() {
        let (started, began) = mpsc::channel();
        let (release, resume) = mpsc::channel();
        let (worker, completed) = Worker::spawn(move |request| {
            let Query::Lyrics(id) = &request.query else {
                panic!()
            };
            started.send(id.clone()).unwrap();
            if id == "blocked" {
                resume.recv().unwrap();
            }
            Ok(Content::Lyrics(Lyrics {
                text: id.clone(),
                source: None,
            }))
        });
        worker.load(Query::Lyrics("blocked".into()), None).unwrap();
        assert_eq!(
            began.recv_timeout(Duration::from_secs(2)).unwrap(),
            "blocked"
        );
        worker
            .load(Query::Lyrics("discarded".into()), None)
            .unwrap();
        worker.cancel();
        let latest = worker.load(Query::Lyrics("latest".into()), None).unwrap();
        release.send(()).unwrap();
        assert_eq!(
            began.recv_timeout(Duration::from_secs(2)).unwrap(),
            "latest"
        );
        let event = completed.recv_blocking().unwrap();
        assert_eq!(event.id, latest);
        assert!(matches!(event.result, Ok(Content::Lyrics(value)) if value.text == "latest"));
        assert!(completed.try_recv().is_err());
    }

    #[test]
    #[ignore = "uses the public YouTube Music credits endpoint"]
    fn live_public_credits() {
        let catalog = Catalog::new().unwrap();
        let credits = catalog.credits("MPTCiKLU7z_xdYQ").unwrap();
        assert!(!credits.sections.is_empty() && credits.song.is_some());
        let page = catalog
            .search_filtered(
                "Coldplay Yellow",
                crate::search::SearchFilter::Songs,
                crate::search::SearchScope::Catalog,
            )
            .unwrap();
        let tracks = page.tracks();
        let track = tracks
            .first()
            .expect("public song search must return a track");
        let id = track
            .credits_id()
            .expect("a valid song must have a credits endpoint");
        assert!(!catalog.credits(&id).unwrap().sections.is_empty());
    }
}
