//! Cookie-free VISIONOS player lookup, reviewed against yt-dlp 2026.08.19.
use super::{AudioQuality, VideoQuality};
use anyhow::{Context, Result, bail, ensure};
use reqwest::blocking::{Client, RequestBuilder};
use serde_json::{Value, json};
use std::{
    cmp::Ordering,
    io::Read,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const TIMEOUT: Duration = Duration::from_secs(3);
pub(super) const BUDGET: Duration = Duration::from_secs(5);
const COOLDOWN: Duration = Duration::from_secs(600);
const STALL_COOLDOWN: Duration = Duration::from_secs(60);
const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 15_7_3) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/26.0 Safari/605.1.15";

#[derive(Default)]
struct Visitor {
    data: Option<String>,
    paused_until: Option<Instant>,
    // Bumped by reset(). A lookup from an earlier account session may still
    // be running; it must not write its visitor ID or pause into this one.
    epoch: u64,
}

#[derive(Clone)]
pub(super) struct Lookup {
    client: Client,
    visitor: Arc<Mutex<Visitor>>,
    homepage: String,
    player: String,
}

impl Lookup {
    pub(super) fn new() -> Result<Self> {
        // No cookie store, account headers, extractor options or browser state.
        Ok(Self {
            client: Client::builder()
                .gzip(true)
                .connect_timeout(TIMEOUT)
                .timeout(TIMEOUT)
                .user_agent(USER_AGENT)
                .build()?,
            visitor: Arc::default(),
            homepage: "https://music.youtube.com/".into(),
            player: "https://www.youtube.com/youtubei/v1/player?prettyPrint=false".into(),
        })
    }

    pub(super) fn lookup(
        &self,
        video_id: &str,
        audio: AudioQuality,
        video: Option<VideoQuality>,
    ) -> Result<Vec<String>> {
        let deadline = Instant::now() + BUDGET;
        let epoch = {
            let visitor = self.visitor.lock().unwrap();
            ensure!(
                !visitor
                    .paused_until
                    .is_some_and(|until| until > Instant::now()),
                "Native lookup is paused"
            );
            visitor.epoch
        };
        let result = self
            .player_response(video_id, deadline, epoch)
            .and_then(|player| select(&player, audio, video));
        if result.as_ref().err().is_some_and(|error| {
            Instant::now() >= deadline
                || error.chain().any(|cause| {
                    cause
                        .downcast_ref::<reqwest::Error>()
                        .is_some_and(|error| error.is_timeout())
                })
        }) {
            self.pause(STALL_COOLDOWN, epoch);
        }
        result
    }

    fn pause(&self, duration: Duration, epoch: u64) {
        let mut visitor = self.visitor.lock().unwrap();
        if visitor.epoch != epoch {
            return;
        }
        let until = Instant::now() + duration;
        visitor.paused_until = Some(
            visitor
                .paused_until
                .map_or(until, |current| current.max(until)),
        );
    }

    pub(super) fn stalled(&self) {
        let epoch = self.visitor.lock().unwrap().epoch;
        self.pause(STALL_COOLDOWN, epoch);
    }

    /// Forget the guest visitor ID and any pause, so a new account session
    /// cannot be linked to the previous one through cookie-free lookups.
    pub(super) fn reset(&self) {
        let mut visitor = self.visitor.lock().unwrap();
        *visitor = Visitor {
            epoch: visitor.epoch + 1,
            ..Visitor::default()
        };
    }

    fn send(
        &self,
        request: RequestBuilder,
        deadline: Instant,
    ) -> Result<reqwest::blocking::Response> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        ensure!(!remaining.is_zero(), "Native lookup exceeded its budget");
        Ok(request
            .timeout(remaining.min(TIMEOUT))
            .send()?
            .error_for_status()?)
    }

    fn visitor_data(&self, refresh: bool, deadline: Instant, epoch: u64) -> Result<String> {
        if !refresh && let Some(data) = self.visitor.lock().unwrap().data.clone() {
            return Ok(data);
        }
        // Do not hold the cache lock across IO: an obsolete preparation must
        // never make a click wait for its request to finish.
        let result = self
            .send(self.client.get(&self.homepage), deadline)
            .and_then(visitor_data);
        match result {
            Ok(data) => {
                let mut visitor = self.visitor.lock().unwrap();
                if visitor.epoch == epoch {
                    visitor.data = Some(data.clone());
                }
                Ok(data)
            }
            Err(error) => {
                // Timeouts get the shorter pause in lookup().
                if !error.chain().any(|cause| {
                    cause
                        .downcast_ref::<reqwest::Error>()
                        .is_some_and(|error| error.is_timeout())
                }) && Instant::now() < deadline
                {
                    self.pause(COOLDOWN, epoch);
                }
                Err(error)
            }
        }
    }

    fn player_response(&self, video_id: &str, deadline: Instant, epoch: u64) -> Result<Value> {
        let mut visitor = self.visitor_data(false, deadline, epoch)?;
        for attempt in 0..2 {
            let response = self.send(
                self.client.post(&self.player)
                    .header("X-YouTube-Client-Name", "101")
                    .header("X-YouTube-Client-Version", "1.02")
                    .header("X-Goog-Visitor-Id", &visitor)
                    .header("Origin", "https://www.youtube.com")
                    .json(&json!({
                        "context": {"client": {
                            "clientName": "VISIONOS", "clientVersion": "1.02",
                            "deviceMake": "Apple", "deviceModel": "RealityDevice17,1",
                            "osName": "visionOS", "osVersion": "26.5.23O471",
                            "userAgent": USER_AGENT,
                            "hl": "en", "timeZone": "UTC", "utcOffsetMinutes": 0
                        }},
                        "videoId": video_id, "contentCheckOk": true, "racyCheckOk": true,
                        "playbackContext": {"contentPlaybackContext": {"html5Preference": "HTML5_PREF_WANTS"}}
                    })), deadline)?;
            let mut bytes = Vec::new();
            response.take(4 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
            ensure!(
                bytes.len() <= 4 * 1024 * 1024,
                "Oversized native player response"
            );
            let player: Value = serde_json::from_slice(&bytes)?;
            if player["playabilityStatus"]
                .to_string()
                .to_lowercase()
                .contains("not a bot")
            {
                if attempt == 0 {
                    visitor = self.visitor_data(true, deadline, epoch)?;
                    continue;
                }
                self.pause(COOLDOWN, epoch);
                bail!("Native player requires a bot check");
            }
            ensure!(
                player["videoDetails"]["videoId"] == video_id,
                "Native player returned another video"
            );
            return Ok(player);
        }
        unreachable!()
    }
}

fn visitor_data(mut page: impl Read) -> Result<String> {
    // Like auth::page_config, stop at the first complete ytcfg bootstrap. That
    // parser also validates a signed-in account, which this path must not use.
    let mut bytes = Vec::new();
    let mut chunk = [0; 16 * 1024];
    loop {
        let count = page.read(&mut chunk)?;
        bytes.extend_from_slice(&chunk[..count]);
        ensure!(bytes.len() <= 4 * 1024 * 1024, "Oversized visitor page");
        let text = String::from_utf8_lossy(&bytes);
        // yt-dlp's ytcfg pattern requires an object; `ytcfg.set("KEY", value)`
        // calls are skipped.
        if let Some(start) = text.find("ytcfg.set({") {
            let bootstrap = &text[start + "ytcfg.set(".len()..];
            let mut values = serde_json::Deserializer::from_str(bootstrap).into_iter::<Value>();
            match values.next() {
                Some(Ok(config))
                    if bootstrap[values.byte_offset()..]
                        .trim_start()
                        .starts_with(')') =>
                {
                    return config["VISITOR_DATA"]
                        .as_str()
                        .filter(|data| !data.is_empty())
                        .map(str::to_owned)
                        .context("Missing visitor data");
                }
                Some(Err(error)) if !error.is_eof() => return Err(error.into()),
                _ => {}
            }
        }
        ensure!(count != 0, "Unreadable visitor page");
    }
}

struct Format<'a> {
    url: Option<&'a str>,
    id: String,
    audio: bool,
    video: bool,
    height: Option<u64>,
    class: Option<u8>,
    abr: Option<f64>,
    score: [f64; 17],
}

fn number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str()?.parse().ok())
        .filter(|value| value.is_finite())
}

fn codec_rank(codec: &str, video: bool) -> Option<f64> {
    let prefixes: &[&[&str]] = if video {
        &[
            &["av01", "av1"],
            &["vp9.2", "vp09.02"],
            &["vp9", "vp09"],
            &["hev", "hvc", "h265"],
            &["avc", "h264"],
            &["vp8", "vp08"],
            &["mp4v", "h263"],
            &["theora"],
        ]
    } else {
        &[
            &["flac", "alac"],
            &["wav", "aiff"],
            &["opus"],
            &["vorbis", "ogg"],
            &["aac"],
            &["mp4a"],
            &["mp3"],
            &["ac4", "ac-4"],
            &["ec-3", "eac3"],
            &["ac-3", "ac3"],
            &["dts"],
        ]
    };
    prefixes
        .iter()
        .position(|prefixes| prefixes.iter().any(|prefix| codec.starts_with(prefix)))
        .map(|index| (prefixes.len() - index) as f64)
        // Dolby Vision is a video codec outside yt-dlp's codec order, which
        // ranks it below every listed codec rather than dropping the format.
        .or_else(|| {
            (video
                && ["dvh1", "dvhe"]
                    .iter()
                    .any(|prefix| codec.starts_with(prefix)))
            .then_some(0.)
        })
}

/// yt-dlp's `hdr:12` rank, from its codec parsing: HDR10 above SDR, and Dolby
/// Vision, which sits past the limit, below both.
fn dynamic_range(codec: &str) -> f64 {
    // yt-dlp first drops leading zeros: re.sub(r'0+(?=\d)', '', codec).
    let mut normalized = String::new();
    let mut chars = codec.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '0' {
            normalized.push(c);
            continue;
        }
        while chars.peek() == Some(&'0') {
            chars.next();
        }
        if !chars.peek().is_some_and(char::is_ascii_digit) {
            normalized.push('0');
        }
    }
    let parts: Vec<_> = normalized.split('.').collect();
    match parts.as_slice() {
        ["dvh1" | "dvhe", ..] => -8.,
        ["av1", _, _, "10", ..] | ["vp9", "2", ..] => 5.,
        _ => 2.,
    }
}

impl<'a> Format<'a> {
    fn parse(raw: &'a Value, duration: Option<f64>) -> Option<Self> {
        if raw["drmFamilies"]
            .as_array()
            .is_some_and(|drm| !drm.is_empty())
            || raw.get("targetDurationSec").is_some()
            || raw["type"] == "FORMAT_STREAM_TYPE_OTF"
        {
            return None;
        }
        let url = raw["url"].as_str().filter(|url| {
            raw.get("signatureCipher").is_none()
                && raw.get("cipher").is_none()
                && reqwest::Url::parse(url).is_ok_and(|url| {
                    url.scheme() == "https"
                        && url
                            .host_str()
                            .is_some_and(|host| host.ends_with(".googlevideo.com"))
                        && !url.query_pairs().any(|(name, _)| name == "n")
                })
        });
        let mime = raw["mimeType"].as_str()?;
        let (kind, codecs) = mime.split_once(';')?;
        let codecs = codecs.trim().strip_prefix("codecs=\"")?.strip_suffix('"')?;
        let (mut acodec, mut vcodec) = (None, None);
        let mut hdr = f64::NEG_INFINITY;
        for codec in codecs.split(',').map(str::trim) {
            if let Some(rank) = codec_rank(codec, true) {
                vcodec = Some(rank);
                hdr = dynamic_range(codec);
            } else {
                acodec = Some(codec_rank(codec, false)?);
            }
        }
        let audio = acodec.is_some();
        let video = vcodec.is_some();
        if !audio && !video {
            return None;
        }
        let itag = raw["itag"].as_u64()?;
        let drc = raw["isDrc"] == true;
        let mut quality = raw["quality"].as_str().unwrap_or("");
        if quality.is_empty() || quality == "tiny" {
            quality = raw["audioQuality"].as_str().unwrap_or(quality);
        }
        if itag == 17 {
            quality = "tiny";
        }
        let quality = [
            "tiny",
            "AUDIO_QUALITY_ULTRALOW",
            "AUDIO_QUALITY_LOW",
            "AUDIO_QUALITY_MEDIUM",
            "AUDIO_QUALITY_HIGH",
            "small",
            "medium",
            "large",
            "hd720",
            "hd1080",
            "hd1440",
            "hd2160",
            "hd2880",
            "highres",
        ]
        .iter()
        .position(|value| *value == quality)
        .map_or(-1., |rank| rank as f64)
            - if drc { 0.5 } else { 0. };
        let class = match raw["audioQuality"].as_str() {
            Some("AUDIO_QUALITY_ULTRALOW") => Some(0),
            Some("AUDIO_QUALITY_LOW") => Some(1),
            Some("AUDIO_QUALITY_MEDIUM") => Some(2),
            Some("AUDIO_QUALITY_HIGH") => Some(3),
            _ => None,
        };
        let bitrate = number(&raw["averageBitrate"])
            .or_else(|| number(&raw["bitrate"]))
            .map(|rate| rate / 1000.);
        let format_duration = number(&raw["approxDurationMs"]).map(|ms| ms / 1000.);
        let damaged = duration
            .zip(format_duration)
            .is_some_and(|(duration, format)| format < (duration / 2.).floor());
        let height = raw["height"].as_u64();
        let resolution = height
            .into_iter()
            .chain(raw["width"].as_u64())
            .min()
            .unwrap_or(0) as f64;
        let display = raw["audioTrack"]["displayName"]
            .as_str()
            .unwrap_or("")
            .to_lowercase();
        let language = if display.contains("descriptive") {
            -10.
        } else if display.contains("original") {
            10.
        } else if raw["audioTrack"]["audioIsDefault"] == true {
            5.
        } else {
            -1.
        };
        let size = number(&raw["contentLength"]).or_else(|| {
            bitrate
                .zip(format_duration)
                .map(|(rate, duration)| (rate * duration * 125.).floor())
        });
        let (vext, aext) = match kind {
            "video/mp4" => (4., 0.),
            "video/webm" => (2., 0.),
            "audio/mp4" => (0., 6.),
            "audio/webm" => (0., 1.),
            _ => return None,
        };
        // yt-dlp's YouTube order: hasvid, preference, quality, res, fps, hdr,
        // source, vcodec, channels, acodec, lang, proto, size, br, asr, ext,
        // hasaud, id. Protocol is identical for these direct HTTPS formats.
        let unknown = f64::NEG_INFINITY;
        Some(Self {
            url,
            id: format!("{itag}{}", if drc { "-drc" } else { "" }),
            audio,
            video,
            height,
            class,
            abr: if audio && !video { bitrate } else { None },
            score: [
                f64::from(video),
                if damaged {
                    -10.
                } else if itag == 17 {
                    -2.
                } else {
                    -1.
                },
                quality,
                resolution,
                number(&raw["fps"])
                    .filter(|fps| *fps > 1.)
                    .unwrap_or(unknown),
                hdr,
                (if itag == 22 { -5. } else { -1. })
                    + if raw["qualityLabel"]
                        .as_str()
                        .is_some_and(|label| label.contains("Premium"))
                    {
                        100.
                    } else {
                        0.
                    },
                vcodec.unwrap_or(0.),
                number(&raw["audioChannels"]).unwrap_or(unknown),
                acodec.unwrap_or(0.),
                language,
                size.unwrap_or(unknown),
                bitrate.unwrap_or(unknown),
                number(&raw["audioSampleRate"]).unwrap_or(unknown),
                vext,
                aext,
                f64::from(audio),
            ],
        })
    }

    fn compare(&self, other: &Self) -> Ordering {
        self.score
            .iter()
            .zip(other.score)
            .map(|(a, b)| a.total_cmp(&b))
            .find(|order| *order != Ordering::Equal)
            .unwrap_or_else(|| self.id.cmp(&other.id))
    }

    fn url(&self) -> Result<String> {
        self.url
            .map(str::to_owned)
            .context("Selected native format needs the full extractor")
    }
}

pub(super) fn select(
    player: &Value,
    audio: AudioQuality,
    video: Option<VideoQuality>,
) -> Result<Vec<String>> {
    ensure!(
        player["playabilityStatus"]["status"] == "OK",
        "Native player is unplayable"
    );
    ensure!(
        player["videoDetails"]["isLive"] != true,
        "Live player needs the full extractor"
    );
    let duration = number(&player["videoDetails"]["lengthSeconds"]);
    let data = &player["streamingData"];
    let mut formats: Vec<_> = ["formats", "adaptiveFormats"]
        .into_iter()
        .flat_map(|field| data[field].as_array().into_iter().flatten())
        .filter_map(|raw| Format::parse(raw, duration))
        .collect();
    formats.sort_by(Format::compare);
    let audio_only: Vec<_> = formats
        .iter()
        .filter(|format| format.audio && !format.video)
        .collect();
    let selected_audio = match audio {
        AudioQuality::Low | AudioQuality::Normal => {
            let class = if audio == AudioQuality::Low { 1 } else { 2 };
            let abr = if audio == AudioQuality::Low {
                64.
            } else {
                128.
            };
            audio_only
                .iter()
                .rev()
                .find(|format| format.class.is_some_and(|value| value <= class))
                .or_else(|| {
                    audio_only
                        .iter()
                        .rev()
                        .find(|format| format.abr.is_some_and(|rate| rate <= abr))
                })
                .or_else(|| {
                    if audio == AudioQuality::Low {
                        audio_only.first()
                    } else {
                        audio_only.last()
                    }
                })
                .copied()
        }
        AudioQuality::High => audio_only.last().copied(),
    };
    if let Some(video) = video {
        if let Some(video) = formats.iter().rev().find(|format| {
            format.video
                && !format.audio
                && format
                    .height
                    .is_some_and(|height| height <= u64::from(video.height()))
        }) && let Some(audio) = selected_audio
        {
            return Ok(vec![video.url()?, audio.url()?]);
        }
    } else if let Some(audio) = selected_audio {
        return Ok(vec![audio.url()?]);
    }
    let progressive = formats.iter().rev().find(|format| {
        format.audio
            && format.video
            && video.is_none_or(|video| {
                format
                    .height
                    .is_some_and(|height| height <= u64::from(video.height()))
            })
    });
    Ok(vec![
        progressive
            .context("No native format matches this quality")?
            .url()?,
    ])
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use std::{
        io::{BufRead, BufReader, Write},
        net::TcpListener,
        thread,
    };

    pub(in crate::player) fn fixture() -> Value {
        serde_json::from_str(include_str!("native_formats.json")).unwrap()
    }

    fn itags(urls: &[String]) -> Vec<String> {
        urls.iter()
            .map(|url| {
                reqwest::Url::parse(url)
                    .unwrap()
                    .query_pairs()
                    .find_map(|(name, value)| (name == "itag").then(|| value.into_owned()))
                    .unwrap()
            })
            .collect()
    }

    pub(in crate::player) fn server(
        responses: Vec<String>,
    ) -> (Lookup, thread::JoinHandle<Vec<Value>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut lookup = Lookup::new().unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        lookup.homepage = format!("{base}/");
        lookup.player = format!("{base}/player");
        let server = thread::spawn(move || {
            responses.into_iter().map(|response| {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut input = BufReader::new(&stream);
            let mut line = String::new();
            input.read_line(&mut line).unwrap();
            let mut headers = serde_json::Map::new();
            loop {
                let mut header = String::new();
                input.read_line(&mut header).unwrap();
                if header == "\r\n" { break; }
                let (name, value) = header.trim().split_once(':').unwrap();
                headers.insert(name.to_lowercase(), json!(value.trim()));
            }
            assert!(!headers.contains_key("cookie"));
            let count = headers.get("content-length").and_then(Value::as_str).map_or(0, |length| length.parse().unwrap());
            let mut body = vec![0; count];
            input.read_exact(&mut body).unwrap();
            let body = serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null);
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nSet-Cookie: INVISIBLE_SECRET=value\r\nConnection: close\r\n\r\n{response}", response.len()).unwrap();
            json!({"request": line, "headers": headers, "body": body})
        }).collect()
        });
        (lookup, server)
    }

    pub(in crate::player) fn homepage() -> String {
        "<script>ytcfg.set({\"VISITOR_DATA\":\"SYNTHETIC_VISITOR\",\"LOGGED_IN\":false});</script>"
            .into()
    }

    #[test]
    fn quality_classes_codecs_and_video_caps_match_selectors() {
        let player = fixture();
        for (audio, expected) in [
            (AudioQuality::Low, "249"),
            (AudioQuality::Normal, "251"),
            (AudioQuality::High, "141"),
        ] {
            assert_eq!(itags(&select(&player, audio, None).unwrap()), [expected]);
            for (video, expected_video) in VideoQuality::ALL
                .into_iter()
                // 480p prefers the HDR VP9.2 format, as yt-dlp does.
                .zip(["160", "133", "134", "333", "298", "303", "271", "313"])
            {
                assert_eq!(
                    itags(&select(&player, audio, Some(video)).unwrap()),
                    [expected_video, expected]
                );
            }
        }
        let mut player = player;
        player["streamingData"]["adaptiveFormats"]
            .as_array_mut()
            .unwrap()
            .retain(|format| format["mimeType"].as_str().unwrap().starts_with("video/"));
        for audio in AudioQuality::ALL {
            assert_eq!(itags(&select(&player, audio, None).unwrap()), ["18"]);
            assert!(select(&player, audio, Some(VideoQuality::P240)).is_err());
            assert_eq!(
                itags(&select(&player, audio, Some(VideoQuality::P360)).unwrap()),
                ["18"]
            );
        }
    }

    #[test]
    fn bitrate_fallbacks_and_unusable_formats() {
        let mut player = fixture();
        let formats = player["streamingData"]["adaptiveFormats"]
            .as_array_mut()
            .unwrap();
        formats.retain(|format| format["mimeType"].as_str().unwrap().starts_with("audio/"));
        for format in &mut *formats {
            format.as_object_mut().unwrap().remove("audioQuality");
        }
        assert_eq!(
            itags(&select(&player, AudioQuality::Low, None).unwrap()),
            ["600"]
        );
        assert_eq!(
            itags(&select(&player, AudioQuality::Normal, None).unwrap()),
            ["249"]
        );
        let formats = player["streamingData"]["adaptiveFormats"]
            .as_array_mut()
            .unwrap();
        for format in &mut *formats {
            format["averageBitrate"] = json!(200000);
        }
        assert_eq!(
            itags(&select(&player, AudioQuality::Low, None).unwrap()),
            ["139"]
        );
        assert_eq!(
            itags(&select(&player, AudioQuality::Normal, None).unwrap()),
            ["600"]
        );
        player["streamingData"]["formats"] = json!([]);
        let formats = player["streamingData"]["adaptiveFormats"]
            .as_array_mut()
            .unwrap();
        for format in &mut *formats {
            format["signatureCipher"] = json!("s=encrypted&url=https://rr1.googlevideo.com/x");
            format.as_object_mut().unwrap().remove("url");
        }
        assert!(select(&player, AudioQuality::High, None).is_err());
        for url in [
            "https://rr1.googlevideo.com/x?n=challenge",
            "https://evil.example/x",
            "http://rr1.googlevideo.com/x",
        ] {
            let mut format = fixture()["streamingData"]["adaptiveFormats"][0].clone();
            format["url"] = json!(url);
            player["streamingData"]["adaptiveFormats"] = json!([format]);
            assert!(select(&player, AudioQuality::Low, None).is_err());
        }
        assert!(select(&json!({}), AudioQuality::High, None).is_err());
    }

    #[test]
    fn visitor_reuse_refresh_and_cookie_isolation() {
        let bot = json!({"playabilityStatus":{"status":"LOGIN_REQUIRED","reason":"Sign in to confirm you're not a bot"}}).to_string();
        let (lookup, server) = server(vec![
            homepage(),
            fixture().to_string(),
            bot.clone(),
            homepage(),
            fixture().to_string(),
            bot.clone(),
            homepage(),
            bot,
        ]);
        for _ in 0..2 {
            lookup
                .lookup("abcdefghijk", AudioQuality::Normal, None)
                .unwrap();
        }
        assert!(
            lookup
                .lookup("abcdefghijk", AudioQuality::Normal, None)
                .is_err()
        );
        assert!(
            lookup
                .lookup("abcdefghijk", AudioQuality::Normal, None)
                .is_err()
        );
        assert!(
            lookup
                .visitor
                .lock()
                .unwrap()
                .paused_until
                .unwrap()
                .saturating_duration_since(Instant::now())
                > Duration::from_secs(590)
        );
        let requests = server.join().unwrap();
        assert_eq!(
            requests
                .iter()
                .filter(|request| request["body"].is_null())
                .count(),
            3
        );
        for request in requests.iter().filter(|request| !request["body"].is_null()) {
            assert_eq!(request["headers"]["x-goog-visitor-id"], "SYNTHETIC_VISITOR");
            assert_eq!(request["headers"]["x-youtube-client-name"], "101");
            assert_eq!(request["headers"]["x-youtube-client-version"], "1.02");
            assert_eq!(
                request["body"]["context"]["client"]["clientName"],
                "VISIONOS"
            );
            assert_eq!(request["body"]["videoId"], "abcdefghijk");
        }
        let (lookup, server) = self::server(vec!["ytcfg.set({});".into()]);
        assert!(
            lookup
                .lookup("abcdefghijk", AudioQuality::Normal, None)
                .is_err()
        );
        assert!(
            lookup
                .lookup("abcdefghijk", AudioQuality::Normal, None)
                .is_err()
        );
        assert!(
            lookup
                .visitor
                .lock()
                .unwrap()
                .paused_until
                .unwrap()
                .saturating_duration_since(Instant::now())
                > Duration::from_secs(590)
        );
        server.join().unwrap();
        // A new account session starts with no visitor ID and no pause.
        lookup.reset();
        let visitor = lookup.visitor.lock().unwrap();
        assert!(visitor.data.is_none() && visitor.paused_until.is_none());
        drop(visitor);
        // A lookup that began before the reset cannot write into the new session.
        lookup.pause(COOLDOWN, 0);
        assert!(lookup.visitor.lock().unwrap().paused_until.is_none());
    }

    #[test]
    fn visitor_data_skips_key_value_ytcfg_calls_and_dolby_vision_is_kept() {
        let page = "ytcfg.set(\"EXPERIMENT\", true);ytcfg.set({\"VISITOR_DATA\":\"VD\"});";
        assert_eq!(visitor_data(page.as_bytes()).unwrap(), "VD");
        // yt-dlp ranks an unlisted video codec below listed ones, not out.
        assert_eq!(codec_rank("dvh1.05.06", true), Some(0.));
        assert!(codec_rank("theora", true) > codec_rank("dvh1.05.06", true));
        assert_eq!(dynamic_range("dvh1.05.06"), -8.);
    }

    #[test]
    fn trickling_response_has_one_budget_and_short_cooldown() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut lookup = Lookup::new().unwrap();
        lookup.player = format!("http://{}", listener.local_addr().unwrap());
        lookup.visitor.lock().unwrap().data = Some("SYNTHETIC_VISITOR".into());
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 8192];
            assert!(stream.read(&mut request).unwrap() > 0);
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: 100000\r\n\r\n").unwrap();
            for _ in 0..200 {
                if stream.write_all(b" ").is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
        });
        let start = Instant::now();
        assert!(
            lookup
                .lookup("abcdefghijk", AudioQuality::High, None)
                .is_err()
        );
        assert!(start.elapsed() < BUDGET);
        let pause = lookup
            .visitor
            .lock()
            .unwrap()
            .paused_until
            .unwrap()
            .saturating_duration_since(Instant::now());
        assert!(
            pause > Duration::from_secs(55) && pause <= STALL_COOLDOWN,
            "{pause:?}"
        );
        server.join().unwrap();
    }

    pub(in crate::player) fn compare_ytdlp(player: &Value) {
        use std::process::{Command, Stdio};
        let cases: Vec<_> = AudioQuality::ALL.into_iter().flat_map(|audio| {
            [None].into_iter().chain(VideoQuality::ALL.map(Some)).map(move |video| {
                json!({
                    "selector": video.map_or_else(|| audio.selector().to_owned(), |video| video.selector(audio)),
                    "itags": select(player,audio,video).ok().map(|urls| itags(&urls))
                })
            })
        }).collect();
        let python = std::env::var_os("YT_DLP_TEST_PYTHON").unwrap_or_else(|| "python3".into());
        let mut child = Command::new(python).args(["-I", "-B", "-c", r#"
import copy, json, sys
from yt_dlp import YoutubeDL
from yt_dlp.extractor.youtube import YoutubeIE
data = json.load(sys.stdin)
player = data['player']
YoutubeIE._extract_player_response = lambda *args, **kwargs: copy.deepcopy(player)
opts = {
    'quiet': True, 'no_warnings': True, 'cachedir': False,
    'skip_download': True,
    'extractor_args': {'youtube': {
        'player_client': ['visionos'],
        'player_skip': ['webpage', 'initial_data', 'configs'],
        'skip': ['hls', 'dash'], 'visitor_data': ['SYNTHETIC_VISITOR'],
    }},
}
with YoutubeDL(opts, auto_init=False) as ydl:
    def no_network(*args, **kwargs):
        raise AssertionError('contract attempted a network request')
    ydl.urlopen = no_network
    ydl.add_info_extractor(YoutubeIE())
    info = ydl.extract_info('https://www.youtube.com/watch?v=' + player['videoDetails']['videoId'], download=False)
    formats = info['formats']
    context = {'formats': formats, 'has_merged_format': any(f.get('acodec') != 'none' and f.get('vcodec') != 'none' for f in formats), 'incomplete_formats': False}
    for case in data['cases']:
        chosen = list(ydl.build_format_selector(case['selector'])(context))
        itags = [f['format_id'].split('-')[0] for f in chosen[0].get('requested_formats', chosen)] if chosen else None
        assert itags == case['itags'], (case['selector'], itags, case['itags'])
print('27 selectors matched')
"#]).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        serde_json::to_writer(
            child.stdin.take().unwrap(),
            &json!({"player":player,"cases":cases}),
        )
        .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        eprintln!(
            "{}: {}",
            player["videoDetails"]["videoId"],
            String::from_utf8_lossy(&output.stdout).trim()
        );
    }

    #[test]
    #[ignore = "requires public YouTube endpoints and reviewed yt-dlp; no account"]
    fn live_native_formats_match_ytdlp_and_download() {
        let lookup = Lookup::new().unwrap();
        for id in ["dQw4w9WgXcQ", "csILUDfmtqM", "fJ9rUzIMcZQ", "jNQXAC9IVRw"] {
            let player = lookup
                .player_response(id, Instant::now() + BUDGET, 0)
                .unwrap();
            compare_ytdlp(&player);
            let mut checked = std::collections::HashSet::new();
            for audio in AudioQuality::ALL {
                let urls = select(&player, audio, Some(VideoQuality::P1080)).unwrap();
                eprintln!("{id} {} 1080p: {:?}", audio.label(), itags(&urls));
                for url in urls {
                    let parsed = reqwest::Url::parse(&url).unwrap();
                    let itag = parsed
                        .query_pairs()
                        .find_map(|(name, value)| (name == "itag").then(|| value.into_owned()))
                        .unwrap();
                    if !checked.insert(itag.clone()) {
                        continue;
                    }
                    let length = parsed
                        .query_pairs()
                        .find_map(|(name, value)| {
                            (name == "clen").then(|| value.parse::<u64>().unwrap())
                        })
                        .unwrap();
                    for start in [0, length / 2] {
                        let response = lookup
                            .client
                            .get(&url)
                            .header("Range", format!("bytes={start}-{}", start + 1023))
                            .timeout(Duration::from_secs(20))
                            .send()
                            .unwrap();
                        assert_eq!(response.status(), 206);
                        assert_eq!(response.bytes().unwrap().len(), 1024);
                    }
                    // YouTube throttles some large, single-range transfers.
                    // yt-dlp also downloads these formats in bounded chunks.
                    let mut bytes = 0;
                    for start in (0..length).step_by(1024 * 1024) {
                        let end = (start + 1024 * 1024 - 1).min(length - 1);
                        let mut response = lookup
                            .client
                            .get(&url)
                            .header("Range", format!("bytes={start}-{end}"))
                            .timeout(Duration::from_secs(20))
                            .send()
                            .unwrap();
                        assert_eq!(response.status(), 206);
                        let count = std::io::copy(&mut response, &mut std::io::sink()).unwrap();
                        assert_eq!(count, end - start + 1);
                        bytes += count;
                    }
                    assert_eq!(bytes, length);
                    eprintln!("{id} itag {itag}: start/mid HTTP 206, complete {bytes} bytes");
                }
            }
        }
    }
}
