use crate::{
    account::require_current_session,
    auth::{ORIGIN, Session, SharedSession},
    catalog::{Catalog, valid_video_id},
};
use anyhow::{Context, Result, ensure};
use reqwest::{StatusCode, Url};
use serde_json::{Value, json};
use std::{
    fs::File,
    io::Read,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

// The account-bound query tokens must stay out of logs and persisted models.
struct HistoryUrl(Url);

impl HistoryUrl {
    fn from_player(response: &Value, video_id: &str, expected: &Session) -> Result<Self> {
        expected.verify_server_identity(response)?;
        ensure!(
            response
                .pointer("/videoDetails/videoId")
                .and_then(Value::as_str)
                == Some(video_id),
            "YouTube Music returned playback history for a different song"
        );
        let raw = response
            .pointer("/playbackTracking/videostatsPlaybackUrl/baseUrl")
            .and_then(Value::as_str)
            .context("YouTube Music did not provide a playback history destination")?;
        Self::parse(raw, video_id)
    }

    fn parse(raw: &str, video_id: &str) -> Result<Self> {
        let invalid =
            || anyhow::anyhow!("YouTube Music returned an invalid playback history destination");
        ensure!(
            !raw.chars().any(char::is_whitespace)
                && !raw.bytes().any(|byte| byte.is_ascii_control())
                && !raw.contains('\\'),
            invalid()
        );
        let url = Url::parse(raw).map_err(|_| invalid())?;
        let authority = raw
            .strip_prefix("https://")
            .and_then(|rest| rest.split(['/', '?', '#']).next())
            .filter(|authority| !authority.is_empty())
            .ok_or_else(invalid)?;
        ensure!(
            url.scheme() == "https"
                && matches!(url.host_str(), Some("s.youtube.com" | "www.youtube.com"))
                && url.path() == "/api/stats/playback"
                && url.port_or_known_default() == Some(443)
                && url.username().is_empty()
                && url.password().is_none()
                && !authority.contains('@')
                && url.fragment().is_none(),
            invalid()
        );
        let mut docids = url.query_pairs().filter(|(key, _)| key == "docid");
        ensure!(
            docids.next().is_some_and(|(_, value)| value == video_id) && docids.next().is_none(),
            "YouTube Music returned playback history for a different song"
        );
        Ok(Self(url))
    }
}

impl Catalog {
    pub fn report_playback(&self, video_id: &str, current: &SharedSession) -> Result<()> {
        let expected = self
            .session
            .as_ref()
            .context("Connect your Google account first")?;
        report_current_playback(
            current,
            expected,
            video_id,
            || self.request("player", player_request(video_id)?),
            |url| {
                let cpn = playback_nonce()?;
                let mut request = self
                    .client
                    .get(url.0)
                    .header("Origin", ORIGIN)
                    .header("Referer", format!("{ORIGIN}/"))
                    .query(&[("ver", "2"), ("c", "WEB_REMIX"), ("cpn", &cpn)]);
                if let Some(delegated) = &expected.delegated_session_id {
                    request = request.header("X-Goog-PageId", delegated);
                }
                // apply computes a fresh authorization timestamp for this second request.
                let request = expected
                    .apply(request)?
                    .build()
                    .map_err(reqwest::Error::without_url)?;
                require_current_session(current, expected)?;
                let response = self
                    .client
                    .execute(request)
                    .map_err(reqwest::Error::without_url)
                    .context("Could not report playback to YouTube Music")?;
                Ok(response.status())
            },
        )
    }
}

fn player_request(video_id: &str) -> Result<Value> {
    // ytmusicapi get_song uses yesterday's epoch day as its default signature timestamp.
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("Could not read the playback request time")?
        .as_secs()
        / 86_400;
    Ok(json!({
        "videoId": video_id,
        "playbackContext":{"contentPlaybackContext":{"signatureTimestamp":timestamp.saturating_sub(1)}}
    }))
}

fn report_current_playback(
    current: &SharedSession,
    expected: &Arc<Session>,
    video_id: &str,
    player: impl FnOnce() -> Result<Value>,
    report: impl FnOnce(HistoryUrl) -> Result<StatusCode>,
) -> Result<()> {
    ensure!(
        valid_video_id(video_id),
        "This song has an invalid YouTube Music ID"
    );
    require_current_session(current, expected)?;
    let tracking = HistoryUrl::from_player(&player()?, video_id, expected)?;
    require_current_session(current, expected)?;
    let status = report(tracking)?;
    ensure!(
        status == StatusCode::NO_CONTENT,
        "YouTube Music did not confirm playback history (HTTP {})",
        status.as_u16()
    );
    Ok(())
}

fn playback_nonce() -> Result<String> {
    const ALPHABET: &[u8; 64] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-_";
    let mut bytes = [0; 16];
    File::open("/dev/urandom")
        .and_then(|mut random| random.read_exact(&mut bytes))
        .context("Could not create a playback history request ID")?;
    Ok(bytes
        .into_iter()
        .map(|byte| ALPHABET[(byte & 63) as usize] as char)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth;
    use std::cell::Cell;

    const VIDEO_ID: &str = "abcdefghijk";

    fn session() -> Session {
        serde_json::from_value(json!({
            "cookie":"SAPISID=synthetic","playback_cookie":"# Netscape HTTP Cookie File\n",
            "authuser":"0","delegated_session_id":null,"data_sync_id":"user||","client_version":"test"
        })).unwrap()
    }

    fn response() -> Value {
        json!({
            "videoDetails":{"videoId":VIDEO_ID},
            "responseContext":{"mainAppWebResponseContext":{"loggedOut":false,"datasyncId":"user||"}},
            "playbackTracking":{"videostatsPlaybackUrl":{"baseUrl":format!(
                "https://s.youtube.com/api/stats/playback?docid={VIDEO_ID}&ei=synthetic-secret"
            )}}
        })
    }

    #[test]
    fn tracking_destinations_are_exact_and_errors_do_not_disclose_tokens() {
        for destination in [
            "http://s.youtube.com/api/stats/playback?docid=abcdefghijk",
            "https://evil.invalid/api/stats/playback?docid=abcdefghijk",
            "https://s.youtube.com.evil.invalid/api/stats/playback?docid=abcdefghijk",
            "https://other.s.youtube.com/api/stats/playback?docid=abcdefghijk",
            "https://s.youtube.com:444/api/stats/playback?docid=abcdefghijk",
            "https://user@s.youtube.com/api/stats/playback?docid=abcdefghijk",
            "https://@s.youtube.com/api/stats/playback?docid=abcdefghijk",
            "https:@s.youtube.com/api/stats/playback?docid=abcdefghijk",
            "https:////@s.youtube.com/api/stats/playback?docid=abcdefghijk",
            "https://s.youtube.com/api/stats/watchtime?docid=abcdefghijk",
            "https://s.youtube.com/api/stats/playback/?docid=abcdefghijk",
            "https://s.youtube.com/api/stats/playback?docid=abcdefghijk#fragment",
            "https://s.youtube.com/api/stats/playback?docid=differentid",
            "https://s.youtube.com/api/stats/playback?missing=abcdefghijk",
            "https://s.youtube.com/api/stats/playback?docid=abcdefghijk&docid=abcdefghijk",
            "https://s.youtube.com/api/stats/playback?docid=abcdefghijk&doc%69d=other",
            "https://s.youtube.com\n.evil.invalid/api/stats/playback?docid=abcdefghijk",
            "https://s.youtube.com\\api/stats/playback?docid=abcdefghijk",
            " https://s.youtube.com/api/stats/playback?docid=abcdefghijk",
        ] {
            let raw = format!("{destination}&token=synthetic-secret");
            let error = HistoryUrl::parse(&raw, VIDEO_ID)
                .err()
                .expect("destination must be rejected");
            let message = format!("{error:#}");
            assert!(!message.contains("synthetic-secret"));
            assert!(!message.contains("https://"));
            assert!(!message.contains("evil.invalid"));
        }
        for host in ["s.youtube.com", "www.youtube.com", "www.youtube.com:443"] {
            assert!(
                HistoryUrl::parse(
                    &format!("https://{host}/api/stats/playback?docid={VIDEO_ID}"),
                    VIDEO_ID
                )
                .is_ok()
            );
        }
    }

    #[test]
    fn player_identity_and_explicit_logout_are_checked_before_reporting() {
        let shared = auth::shared(Some(session()));
        let expected = shared.read().unwrap().clone().unwrap();
        for response in [
            {
                let mut value = response();
                value["videoDetails"]["videoId"] = json!("differentid");
                value
            },
            {
                let mut value = response();
                value["videoDetails"] = Value::Null;
                value
            },
            {
                let mut value = response();
                value["LOGGED_IN"] = json!(false);
                value
            },
            {
                let mut value = response();
                value["responseContext"]["mainAppWebResponseContext"]["loggedOut"] = json!(true);
                value
            },
        ] {
            let dispatched = Cell::new(false);
            assert!(
                report_current_playback(
                    &shared,
                    &expected,
                    VIDEO_ID,
                    || Ok(response),
                    |_| {
                        dispatched.set(true);
                        Ok(StatusCode::NO_CONTENT)
                    }
                )
                .is_err()
            );
            assert!(!dispatched.get());
        }
    }

    #[test]
    fn account_replacement_or_sign_out_discards_both_queued_and_prepared_reports() {
        for replacement in [None, Some(Arc::new(session()))] {
            let shared = auth::shared(Some(session()));
            let expected = shared.read().unwrap().clone().unwrap();
            let dispatched = Cell::new(false);
            assert!(
                report_current_playback(
                    &shared,
                    &expected,
                    VIDEO_ID,
                    || {
                        *shared.write().unwrap() = replacement;
                        Ok(response())
                    },
                    |_| {
                        dispatched.set(true);
                        Ok(StatusCode::NO_CONTENT)
                    }
                )
                .is_err()
            );
            assert!(!dispatched.get());
            let fetched = Cell::new(false);
            assert!(
                report_current_playback(
                    &shared,
                    &expected,
                    VIDEO_ID,
                    || {
                        fetched.set(true);
                        Ok(response())
                    },
                    |_| Ok(StatusCode::NO_CONTENT)
                )
                .is_err()
            );
            assert!(!fetched.get());
        }
    }

    #[test]
    fn history_requires_matching_primary_or_brand_identity() {
        for (delegated, identity) in [(None, "user||"), (Some("brand"), "brand||user")] {
            let mut config = serde_json::to_value(session()).unwrap();
            config["authuser"] = json!("2");
            config["delegated_session_id"] = json!(delegated);
            config["data_sync_id"] = json!(identity);
            let shared = auth::shared(Some(serde_json::from_value(config).unwrap()));
            let expected = shared.read().unwrap().clone().unwrap();
            for reported in [None, Some("different||"), Some("brand||different")] {
                let mut response = response();
                response["responseContext"]["mainAppWebResponseContext"]
                    .as_object_mut()
                    .unwrap()
                    .remove("datasyncId");
                if let Some(reported) = reported {
                    response["responseContext"]["mainAppWebResponseContext"]["datasyncId"] =
                        json!(reported);
                }
                let dispatched = Cell::new(false);
                assert!(
                    report_current_playback(
                        &shared,
                        &expected,
                        VIDEO_ID,
                        || Ok(response),
                        |_| {
                            dispatched.set(true);
                            Ok(StatusCode::NO_CONTENT)
                        }
                    )
                    .is_err()
                );
                assert!(!dispatched.get());
            }
            let mut proven = response();
            proven["responseContext"]["mainAppWebResponseContext"]["datasyncId"] = json!(identity);
            proven["SESSION_INDEX"] = json!(2);
            proven["USER_SESSION_ID"] = json!("user");
            proven["DELEGATED_SESSION_ID"] = json!(delegated);
            assert!(
                report_current_playback(
                    &shared,
                    &expected,
                    VIDEO_ID,
                    || Ok(proven.clone()),
                    |_| Ok(StatusCode::NO_CONTENT)
                )
                .is_ok()
            );
            for (key, conflict) in [
                ("DATASYNC_ID", json!("other||")),
                ("SESSION_INDEX", json!(1)),
                ("USER_SESSION_ID", json!("other")),
                ("DELEGATED_SESSION_ID", json!("other-brand")),
            ] {
                let mut conflicting = proven.clone();
                conflicting[key] = conflict;
                assert!(
                    report_current_playback(
                        &shared,
                        &expected,
                        VIDEO_ID,
                        || Ok(conflicting),
                        |_| panic!("conflicting identity must not dispatch")
                    )
                    .is_err()
                );
            }
        }
    }

    #[test]
    fn only_no_content_acknowledges_history() {
        let shared = auth::shared(Some(session()));
        let expected = shared.read().unwrap().clone().unwrap();
        for status in [200, 201, 204, 302, 307, 401, 500] {
            let result = report_current_playback(
                &shared,
                &expected,
                VIDEO_ID,
                || Ok(response()),
                |_| Ok(StatusCode::from_u16(status).unwrap()),
            );
            assert_eq!(result.is_ok(), status == 204);
        }
    }

    #[test]
    #[ignore = "inspects the public player response without sending any playback history"]
    fn live_public_player_tracking() {
        let catalog = Catalog::new().unwrap();
        let video_id = "dQw4w9WgXcQ";
        let response = catalog
            .request("player", player_request(video_id).unwrap())
            .unwrap();
        assert_eq!(
            response
                .pointer("/videoDetails/videoId")
                .and_then(Value::as_str),
            Some(video_id)
        );
        let tracking = response
            .pointer("/playbackTracking/videostatsPlaybackUrl/baseUrl")
            .and_then(Value::as_str);
        println!(
            "Guest player status: {}; playback history destination available: {}",
            response
                .pointer("/playabilityStatus/status")
                .and_then(Value::as_str)
                .unwrap_or("missing"),
            tracking.is_some()
        );
        if let Some(raw) = tracking {
            let tracking = HistoryUrl::parse(raw, video_id).unwrap();
            assert_eq!(tracking.0.path(), "/api/stats/playback");
        }
    }
}
