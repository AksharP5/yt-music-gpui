use super::native;
use super::ytdlp::{ExtractorCredentials, RESOLVER_TIMEOUT, StreamPreparation, WarmResolver};
use super::{AudioQuality, VideoQuality};
use crate::os::{AsWaitable, Waitable, Waker};
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// YouTube Music's loudness normalization, as its web player computes it
/// (base.js for youtube.player.web_20261110): a track louder than the target
/// is turned down to it, a quieter one is never boosted. The selected format's
/// values take precedence over the response's audioConfig. Stateful
/// normalization (applyStatefulNormalization) is off in the responses we get,
/// so it is not modelled. Returns the gain in dB, never above 0.
pub(super) fn normalization_gain(audio_config: &Value, format: &Value) -> f64 {
    let number = |value: &Value| value.as_f64().filter(|value| value.is_finite());
    let field = |name| number(&format[name]).or_else(|| number(&audio_config[name]));
    let gain = match field("trackAbsoluteLoudnessLkfs") {
        Some(lkfs) => {
            number(&audio_config["loudnessTargetLkfs"]).map_or(0., |target| target - lkfs)
        }
        None => field("loudnessDb").map_or(0., |loudness| -loudness),
    };
    // mpv rejects a volume-gain below its default minimum.
    gain.clamp(-96., 0.)
}

#[derive(Clone, PartialEq, Eq)]
pub(super) struct StreamKey {
    pub(super) url: String,
    pub(super) quality: AudioQuality,
    pub(super) video: Option<VideoQuality>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ResolverLane {
    Fast,
    Full,
}

#[derive(Clone)]
pub(super) struct PreparedTarget {
    pub(super) lane: ResolverLane,
    pub(super) key: StreamKey,
    pub(super) track_id: u64,
}

pub(super) struct ResolvedStream {
    pub(super) key: StreamKey,
    pub(super) url: String,
    pub(super) gain_db: f64,
    pub(super) expires: SystemTime,
    /// A validated timedtext URL. Only video streams keep one: captions draw over the video.
    pub(super) captions: Option<String>,
}

impl ResolvedStream {
    pub(super) fn parse(
        key: StreamKey,
        bytes: &[u8],
        gain_db: f64,
        now: SystemTime,
    ) -> Result<Self> {
        let text = std::str::from_utf8(bytes)?;
        let urls = text
            .lines()
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>();
        ensure!(
            !urls.is_empty() && urls.len() <= 2,
            "Invalid prepared stream"
        );
        let mut expires: Option<SystemTime> = None;
        for url in &urls {
            let parsed = reqwest::Url::parse(url)?;
            ensure!(
                parsed.scheme() == "https"
                    && parsed
                        .host_str()
                        .is_some_and(|host| host.ends_with(".googlevideo.com"))
                    // Tests play prepared streams from local files with this suffix.
                    || cfg!(test)
                        && parsed.scheme() == "file"
                        && parsed.path().ends_with(".test-stream.wav"),
                "Unsupported prepared stream host"
            );
            if let Some(expiry) = parsed.query_pairs().find_map(|(name, value)| {
                (name == "expire")
                    .then(|| value.parse::<u64>().ok())
                    .flatten()
            }) {
                let expiry = UNIX_EPOCH + Duration::from_secs(expiry.saturating_sub(60));
                expires = Some(expires.map_or(expiry, |earlier| earlier.min(expiry)));
            }
        }
        // A link without an expiry is only trusted briefly.
        let expires = expires.unwrap_or(now + Duration::from_secs(5 * 60));
        ensure!(expires > now, "Prepared stream expired");
        let url = if urls.len() == 1 {
            urls[0].to_owned()
        } else {
            format!(
                "edl://%{}%{};!new_stream;%{}%{}",
                urls[0].len(),
                urls[0],
                urls[1].len(),
                urls[1]
            )
        };
        Ok(Self {
            key,
            url,
            gain_db,
            expires,
            captions: None,
        })
    }

    pub(super) fn select(key: StreamKey, selection: native::Selection) -> Result<Self> {
        let captions = key.video.and(selection.captions);
        let stream = Self::parse(
            key,
            selection.urls.join("\n").as_bytes(),
            selection.gain_db,
            SystemTime::now(),
        )?;
        Ok(Self { captions, ..stream })
    }

    pub(super) fn matches(&self, key: &StreamKey) -> bool {
        self.key == *key && SystemTime::now() < self.expires
    }
}

/// When a paused stream needs a fresh URL: five minutes before the earliest
/// `expire` of its googlevideo URLs. Video plays an EDL of two URLs.
pub(super) fn stream_refresh_time(path: &str) -> Option<SystemTime> {
    path.split(['?', '&'])
        .skip(1)
        .filter_map(|parameter| parameter.strip_prefix("expire="))
        .filter_map(|value| {
            value
                .split(|c: char| !c.is_ascii_digit())
                .next()?
                .parse::<u64>()
                .ok()
        })
        .min()
        .and_then(|expiry| {
            UNIX_EPOCH.checked_add(Duration::from_secs(expiry.saturating_sub(5 * 60)))
        })
}

impl StreamKey {
    pub(super) fn lane(&self, preparing: bool, signed_in: bool) -> ResolverLane {
        if self.video.is_some_and(VideoQuality::uses_full_resolver)
            || (preparing && signed_in && self.quality == AudioQuality::High)
        {
            ResolverLane::Full
        } else {
            ResolverLane::Fast
        }
    }

    pub(super) fn selector(&self) -> String {
        self.video
            .map(|video| video.selector(self.quality))
            .unwrap_or_else(|| self.quality.selector().to_owned())
    }
}

pub(super) struct NativeResolver {
    pub(super) lookup: native::Lookup,
    wake: Arc<Waker>,
    sender: Sender<(u64, Result<ResolvedStream>)>,
    receiver: Receiver<(u64, Result<ResolvedStream>)>,
    target: Option<PreparedTarget>,
    generation: u64,
    started: Instant,
}

impl NativeResolver {
    pub(super) fn new(lookup: native::Lookup, wake: Arc<Waker>) -> Self {
        let (sender, receiver) = mpsc::channel();
        Self {
            lookup,
            wake,
            sender,
            receiver,
            target: None,
            generation: 0,
            started: Instant::now(),
        }
    }

    fn start(&mut self, target: PreparedTarget) -> Result<()> {
        if self
            .target
            .as_ref()
            .is_some_and(|current| current.key == target.key)
        {
            self.target = Some(target);
            return Ok(());
        }
        self.target = None;
        self.generation += 1;
        let generation = self.generation;
        let key = target.key.clone();
        let lookup = self.lookup.clone();
        let sender = self.sender.clone();
        let wake = self.wake.clone();
        thread::Builder::new()
            .name("stream-lookup".into())
            .spawn(move || {
                let result = (|| {
                    let url = reqwest::Url::parse(&key.url)?;
                    let id = url
                        .query_pairs()
                        .find_map(|(name, value)| (name == "v").then(|| value.into_owned()))
                        .context("Missing video ID")?;
                    let selection = lookup.lookup(&id, key.quality, key.video)?;
                    ResolvedStream::select(key, selection)
                })();
                if sender.send((generation, result)).is_ok() {
                    wake.wake();
                }
            })?;
        self.started = Instant::now();
        self.target = Some(target);
        Ok(())
    }

    fn finish(&mut self) -> Option<(PreparedTarget, Result<ResolvedStream>, Instant)> {
        while let Ok((generation, result)) = self.receiver.try_recv() {
            if generation == self.generation
                && let Some(target) = self.target.take()
            {
                return Some((target, result, self.started));
            }
        }
        if self.started.elapsed() >= native::BUDGET
            && let Some(target) = self.target.take()
        {
            self.lookup.stalled();
            return Some((
                target,
                Err(anyhow::anyhow!("Native lookup timed out")),
                self.started,
            ));
        }
        None
    }
}

pub(super) struct StreamResolver {
    ytdlp: PathBuf,
    warm: Option<WarmResolver>,
    warm_enabled: bool,
    one_shot: Option<StreamPreparation>,
    target: Option<PreparedTarget>,
    started: Instant,
    pub(super) native: Option<NativeResolver>,
}

impl StreamResolver {
    pub(super) fn new(
        ytdlp: &Path,
        credentials: Option<&ExtractorCredentials>,
        native: Option<NativeResolver>,
    ) -> Self {
        let eager = credentials.is_some() || native.is_none();
        let mut resolver = Self {
            ytdlp: ytdlp.to_owned(),
            warm_enabled: true,
            warm: None,
            one_shot: None,
            target: None,
            started: Instant::now(),
            native,
        };
        if eager {
            resolver.warm_up(credentials);
        }
        resolver
    }

    pub(super) fn warm_up(&mut self, credentials: Option<&ExtractorCredentials>) {
        if self.warm.is_none() && self.warm_enabled {
            self.warm = WarmResolver::start(&self.ytdlp, credentials).ok();
            self.warm_enabled = self.warm.is_some();
        }
    }

    pub(super) fn fd(&self) -> Option<Waitable<'_>> {
        self.warm
            .as_ref()
            .map(|warm| warm.output.as_waitable())
            .or_else(|| self.one_shot.as_ref().map(|job| job.exited.as_waitable()))
    }

    pub(super) fn deadline(&self) -> Option<Instant> {
        let startup = self
            .warm
            .as_ref()
            .filter(|warm| !warm.ready)
            .map(|warm| warm.started + RESOLVER_TIMEOUT);
        let request = self
            .target
            .as_ref()
            .map(|_| self.started + RESOLVER_TIMEOUT);
        let native = self
            .native
            .as_ref()
            .filter(|native| native.target.is_some())
            .map(|native| native.started + native::BUDGET);
        startup.into_iter().chain(request).chain(native).min()
    }

    pub(super) fn stop(&mut self) {
        if let Some(native) = &mut self.native {
            native.target = None;
        }
        self.warm.take();
        self.one_shot.take();
        self.target = None;
    }

    pub(super) fn cancel(&mut self) {
        if let Some(native) = &mut self.native {
            native.target = None;
        }
        if self.target.is_some() {
            self.stop();
        }
    }

    pub(super) fn start(
        &mut self,
        mut target: PreparedTarget,
        credentials: Option<&ExtractorCredentials>,
    ) -> Result<()> {
        if target.lane == ResolverLane::Fast
            && let Some(native) = &mut self.native
        {
            if native.start(target.clone()).is_ok() {
                return Ok(());
            }
            target.lane = ResolverLane::Full;
        }
        if let Some(native) = &mut self.native
            && native
                .target
                .as_ref()
                .is_some_and(|current| current.key == target.key)
        {
            native.target = None;
        }
        // Python only runs full lookups, so its result can satisfy either lane.
        if self
            .target
            .as_ref()
            .is_some_and(|current| current.key == target.key)
        {
            self.target = Some(target);
            return Ok(());
        }
        // Only supersede the full extractor here. Native clicks are independent.
        if self.target.is_some() {
            self.warm.take();
            self.one_shot.take();
            self.target = None;
        }
        self.warm_up(credentials);
        self.started = Instant::now();
        let target = self.target.insert(target);
        if let Some(warm) = &mut self.warm {
            if warm.send(target).is_ok() {
                return Ok(());
            }
            self.warm_enabled = warm.ready;
            self.warm.take();
        }
        match StreamPreparation::start(target.clone(), &self.ytdlp, credentials) {
            Ok(job) => self.one_shot = Some(job),
            Err(error) => {
                self.target = None;
                return Err(error);
            }
        }
        Ok(())
    }

    pub(super) fn retain(&mut self, play: Option<&PreparedTarget>, next: Option<&PreparedTarget>) {
        let wanted = [play, next];
        if let Some(current) = &mut self.target {
            if let Some(target) = wanted
                .into_iter()
                .flatten()
                .find(|target| target.key == current.key)
            {
                current.track_id = target.track_id;
            } else {
                self.warm.take();
                self.one_shot.take();
                self.target = None;
            }
        }
        if let Some(native) = &mut self.native
            && let Some(current) = &mut native.target
        {
            if let Some(target) = wanted
                .into_iter()
                .flatten()
                .find(|target| target.lane == ResolverLane::Fast && target.key == current.key)
            {
                current.track_id = target.track_id;
            } else {
                native.target = None;
            }
        }
    }

    /// `play` is the song waiting to load. A failed native preparation must not
    /// take the full extractor from it; preparation restarts once it loads.
    pub(super) fn finish(
        &mut self,
        play: Option<&PreparedTarget>,
        credentials: Option<&ExtractorCredentials>,
    ) -> Option<(PreparedTarget, Result<ResolvedStream>)> {
        if let Some((mut target, result, started)) =
            self.native.as_mut().and_then(NativeResolver::finish)
        {
            if result.is_ok() {
                return Some((target, result));
            }
            let serving_play = play.is_some_and(|play| {
                play.track_id != target.track_id
                    && self
                        .target
                        .as_ref()
                        .is_some_and(|current| current.key == play.key)
            });
            if !serving_play {
                target.lane = ResolverLane::Full;
                if let Err(error) = self.start(target.clone(), credentials) {
                    return Some((target, Err(error)));
                }
                self.started = started;
            }
        }
        let mut result = None;
        if let Some(warm) = &mut self.warm {
            let response = if !warm.ready && warm.started.elapsed() >= RESOLVER_TIMEOUT {
                Err(anyhow::anyhow!("Resolver did not start"))
            } else {
                warm.read()
            };
            match response {
                Ok(Some(urls)) => {
                    if let Some(target) = &self.target {
                        result = Some(urls.context("Could not resolve the stream").and_then(
                            |selection| ResolvedStream::select(target.key.clone(), selection),
                        ));
                    }
                }
                Err(_) => {
                    // Unsupported installs and servers that never start keep the CLI
                    // path, with the very same account/plugin/cookie arguments. A
                    // server that crashed after starting is respawned on the next request.
                    self.warm_enabled = warm.ready;
                    self.warm.take();
                    if let Some(target) = &self.target {
                        match StreamPreparation::start(target.clone(), &self.ytdlp, credentials) {
                            Ok(job) => {
                                self.started = Instant::now();
                                self.one_shot = Some(job);
                            }
                            Err(error) => result = Some(Err(error)),
                        }
                    }
                }
                Ok(None) => {}
            }
        }
        if result.is_none()
            && let Some(job) = &mut self.one_shot
        {
            match job.finish() {
                Ok(Some(stream)) => result = Some(Ok(stream)),
                Err(error) => result = Some(Err(error)),
                Ok(None) => {}
            }
        }
        if result.is_none() && self.target.is_some() && self.started.elapsed() >= RESOLVER_TIMEOUT {
            self.warm.take();
            result = Some(Err(anyhow::anyhow!("Stream resolution timed out")));
        }
        let result = result?;
        self.one_shot.take();
        Some((self.target.take()?, result))
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    #[cfg(unix)]
    use crate::auth::PlaybackCredentials;
    #[cfg(unix)]
    use crate::os::IpcStream;
    #[cfg(unix)]
    use crate::player::{
        actor::wait_for_player_input,
        command_channel, find_ytdlp,
        ytdlp::{
            tests::{alive, signed_in, synthetic_account, synthetic_credentials},
            write_account_extractor,
        },
    };
    use serde_json::json;
    #[cfg(unix)]
    use std::{fs, io::Write, os::unix::fs::PermissionsExt};

    pub(crate) fn resolver_target(id: &str, track_id: u64) -> PreparedTarget {
        PreparedTarget {
            lane: ResolverLane::Full,
            key: StreamKey {
                url: format!("https://music.youtube.com/watch?v={id}"),
                quality: AudioQuality::Normal,
                video: None,
            },
            track_id,
        }
    }

    #[test]
    fn resolver_lanes_keep_clicks_fast_and_prepare_premium_audio() {
        for quality in AudioQuality::ALL {
            for video in [None].into_iter().chain(VideoQuality::ALL.map(Some)) {
                let key = StreamKey {
                    quality,
                    video,
                    ..resolver_target("abcdefghijk", 1).key
                };
                let above_1080 = matches!(video, Some(VideoQuality::P1440 | VideoQuality::P2160));
                for signed_in in [false, true] {
                    assert_eq!(
                        key.lane(false, signed_in),
                        if above_1080 {
                            ResolverLane::Full
                        } else {
                            ResolverLane::Fast
                        }
                    );
                    assert_eq!(
                        key.lane(true, signed_in),
                        if above_1080 || (signed_in && quality == AudioQuality::High) {
                            ResolverLane::Full
                        } else {
                            ResolverLane::Fast
                        }
                    );
                }
            }
        }
    }

    #[cfg(unix)]
    fn synthetic_resolver_plugin(directory: &Path) -> PathBuf {
        let plugin = write_account_extractor(directory).unwrap();
        let path = plugin.join("account/yt_dlp_plugins/extractor/account.py");
        let mut file = fs::OpenOptions::new().append(true).open(path).unwrap();
        // The actual account plugin is discovered normally. Replace only remote
        // extraction for these process-lifetime tests; Python tests cover identity checks.
        writeln!(file, r#"
import json, os, signal, subprocess, sys
from pathlib import Path

def initialize(self):
    self._account

def extract(self, url):
    self.calls = getattr(self, 'calls', 0) + 1
    if url.endswith('blocked0000'):
        child = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(30)'])
        Path({ready:?}).write_text(json.dumps({{'pid': os.getpid(), 'child': child.pid}}))
        os.kill(os.getpid(), signal.SIGSTOP)
    return {{'id': 'abcdefghijk', 'title': 'Synthetic', 'formats': [{{
        'format_id': 'audio', 'ext': 'webm', 'vcodec': 'none', 'acodec': 'opus',
        'url': 'https://rr1.googlevideo.com/audio?call=' + str(self.calls) + '&identity=' + self._account['data_sync_id']}}]}}

YtMusicAccountIE._real_initialize = initialize
YtMusicAccountIE._real_extract = extract
"#, ready=directory.join("ready").to_str().unwrap()).unwrap();
        plugin
    }

    #[cfg(unix)]
    fn resolved(
        resolver: &mut StreamResolver,
        credentials: Option<&ExtractorCredentials>,
    ) -> ResolvedStream {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some((_, result)) = resolver.finish(None, credentials) {
                return result.unwrap();
            }
            assert!(Instant::now() < deadline, "resolver did not answer");
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "requires Python with yt-dlp 2026.11.23; synthetic account and process group, no network"]
    fn warm_resolver_reuses_requests_and_supersession_kills_the_process_group() {
        let directory = tempfile::tempdir().unwrap();
        let plugin = synthetic_resolver_plugin(directory.path());
        let credentials = signed_in(synthetic_credentials("INVENTED_FIRST"), &plugin);
        let ytdlp = find_ytdlp().unwrap();
        let mut resolver = StreamResolver::new(&ytdlp, Some(&credentials), None);
        let pid = resolver.warm.as_ref().unwrap().child.id();
        let blocked = resolver_target("blocked0000", 1);
        resolver.start(blocked.clone(), Some(&credentials)).unwrap();
        let ready = directory.path().join("ready");
        let deadline = Instant::now() + Duration::from_secs(10);
        while !ready.exists() {
            assert!(
                Instant::now() < deadline,
                "synthetic extractor did not start"
            );
            // Like the actor: the signed-in request goes out once the resolver is ready.
            assert!(resolver.finish(None, Some(&credentials)).is_none());
            thread::sleep(Duration::from_millis(5));
        }
        let child = serde_json::from_slice::<Value>(&fs::read(ready).unwrap()).unwrap()["child"]
            .as_u64()
            .unwrap();
        resolver
            .start(
                PreparedTarget {
                    track_id: 2,
                    ..blocked
                },
                Some(&credentials),
            )
            .unwrap();
        assert_eq!(
            resolver.warm.as_ref().unwrap().child.id(),
            pid,
            "Play of the preparing track must keep its request"
        );
        resolver
            .start(resolver_target("abcdefghijk", 3), Some(&credentials))
            .unwrap();
        assert!(
            !alive(pid),
            "superseded resolver must be reaped synchronously"
        );
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let stat = fs::read_to_string(format!("/proc/{child}/stat")).unwrap_or_default();
            if stat.is_empty() || stat.split_once(") ").unwrap().1.starts_with('Z') {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "JS runtime survived process-group cancellation"
            );
            thread::sleep(Duration::from_millis(5));
        }
        let new_pid = resolver.warm.as_ref().unwrap().child.id();
        assert_ne!(pid, new_pid);
        assert!(
            resolved(&mut resolver, Some(&credentials))
                .url
                .contains("call=1")
        );
        resolver
            .start(resolver_target("dQw4w9WgXcQ", 4), Some(&credentials))
            .unwrap();
        assert!(
            resolved(&mut resolver, Some(&credentials))
                .url
                .contains("call=2")
        );
        assert_eq!(resolver.warm.as_ref().unwrap().child.id(), new_pid);
        // A crash after startup falls back once, then the next request is warm again.
        unsafe { libc::kill(new_pid as i32, libc::SIGKILL) };
        resolver
            .start(resolver_target("abcdefghijk", 5), Some(&credentials))
            .unwrap();
        assert!(
            resolved(&mut resolver, Some(&credentials))
                .url
                .contains("call=1")
        );
        resolver
            .start(resolver_target("dQw4w9WgXcQ", 6), Some(&credentials))
            .unwrap();
        let respawned = resolver
            .warm
            .as_ref()
            .expect("warm resolver respawned")
            .child
            .id();
        assert_ne!(respawned, new_pid);
        resolved(&mut resolver, Some(&credentials));
        drop(resolver);
        assert!(!alive(respawned));
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "requires Python with yt-dlp 2026.11.23; synthetic accounts, no network"]
    fn warm_credential_change_respawns_with_the_new_account() {
        let directory = tempfile::tempdir().unwrap();
        let plugin = synthetic_resolver_plugin(directory.path());
        let credentials = signed_in(synthetic_credentials("INVENTED_FIRST"), &plugin);
        let ytdlp = find_ytdlp().unwrap();
        let mut resolver = StreamResolver::new(&ytdlp, Some(&credentials), None);
        resolver
            .start(resolver_target("abcdefghijk", 1), Some(&credentials))
            .unwrap();
        assert!(
            resolved(&mut resolver, Some(&credentials))
                .url
                .contains("identity=brand||user")
        );
        let pid = resolver.warm.as_ref().unwrap().child.id();
        resolver.stop();
        assert!(!alive(pid));
        let credentials = signed_in(
            synthetic_account("INVENTED_SECOND", "3", "other", "second"),
            &plugin,
        );
        resolver = StreamResolver::new(&ytdlp, Some(&credentials), None);
        resolver
            .start(resolver_target("abcdefghijk", 2), Some(&credentials))
            .unwrap();
        assert!(
            resolved(&mut resolver, Some(&credentials))
                .url
                .contains("identity=other||second")
        );
    }

    #[cfg(unix)]
    pub(crate) fn synthetic_full_extractor(directory: &Path, blocked: bool) -> PathBuf {
        let interpreter = directory.join("python3");
        let response = if blocked {
            format!(
                "echo started > {}; exec sleep 60",
                directory.join("started").display()
            )
        } else {
            // The loudness shape ytdlp_resolver.py reported for fJ9rUzIMcZQ.
            "printf '%s\\n' '{\"urls\":[\"https://rr1.googlevideo.com/full\"],\"loudness\":{\"audioConfig\":{\"trackAbsoluteLoudnessLkfs\":-11.71,\"loudnessTargetLkfs\":-14},\"format\":{\"loudnessDb\":2.29,\"trackAbsoluteLoudnessLkfs\":-11.71}}}'".into()
        };
        fs::write(&interpreter, format!("#!/bin/sh\nprintf '%s\\n' '{{\"ready\":true}}'\nwhile IFS= read -r request; do {response}; done\n")).unwrap();
        fs::set_permissions(&interpreter, fs::Permissions::from_mode(0o700)).unwrap();
        let extractor = directory.join("extractor");
        fs::write(&extractor, format!("#!{}\n", interpreter.display())).unwrap();
        extractor
    }

    #[cfg(unix)]
    #[test]
    fn guest_native_lookup_starts_the_full_resolver_only_on_fallback() {
        let directory = tempfile::tempdir().unwrap();
        let extractor = synthetic_full_extractor(directory.path(), false);
        let (commands, _received) = command_channel().unwrap();
        let (lookup, server) = native::tests::server(vec![
            native::tests::homepage(),
            native::tests::fixture().to_string(),
            "invalid json".into(),
        ]);
        let native = NativeResolver::new(lookup, commands.wake);
        let mut resolver = StreamResolver::new(&extractor, None, Some(native));
        assert!(resolver.fd().is_none());

        let mut track = resolver_target("abcdefghijk", 1);
        track.lane = ResolverLane::Fast;
        resolver.start(track.clone(), None).unwrap();
        let native_stream = resolved(&mut resolver, None);
        assert!(!native_stream.url.ends_with("/full"));
        assert!(resolver.fd().is_none());

        track.track_id = 2;
        resolver.start(track, None).unwrap();
        let full_stream = resolved(&mut resolver, None);
        assert!(full_stream.url.ends_with("/full"));
        assert!(resolver.fd().is_some());
        server.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn native_click_during_full_preparation_does_not_wait_or_restart_python() {
        let directory = tempfile::tempdir().unwrap();
        let extractor = synthetic_full_extractor(directory.path(), true);
        let mut resolver = StreamResolver::new(&extractor, None, None);
        let (commands, _received) = command_channel().unwrap();
        let (lookup, server) = native::tests::server(vec![
            native::tests::homepage(),
            native::tests::fixture().to_string(),
        ]);
        resolver.native = Some(NativeResolver::new(lookup, commands.wake.clone()));
        let preparation = resolver_target("blocked0000", 1);
        resolver.start(preparation.clone(), None).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !directory.path().join("started").exists() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
        let pid = resolver.warm.as_ref().unwrap().child.id();
        let mut click = resolver_target("abcdefghijk", 2);
        click.lane = ResolverLane::Fast;
        let start = Instant::now();
        resolver.start(click.clone(), None).unwrap();
        resolver.retain(Some(&click), Some(&preparation));
        let (socket, _mpv) = IpcStream::pair().unwrap();
        let (target, stream) = loop {
            if let Some(result) = resolver.finish(None, None) {
                break result;
            }
            assert!(start.elapsed() < Duration::from_secs(2));
            wait_for_player_input(
                &socket,
                &commands.wake,
                resolver.fd(),
                Some(start + Duration::from_secs(2)),
            )
            .unwrap();
        };
        assert_eq!(target.track_id, 2);
        assert!(stream.unwrap().url.contains("itag=251"));
        assert_eq!(resolver.warm.as_ref().unwrap().child.id(), pid);
        assert_eq!(resolver.target.as_ref().unwrap().track_id, 1);
        assert!(alive(pid));
        server.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_native_preparation_leaves_the_full_reload_running() {
        let directory = tempfile::tempdir().unwrap();
        let extractor = synthetic_full_extractor(directory.path(), true);
        let mut resolver = StreamResolver::new(&extractor, None, None);
        let (commands, _received) = command_channel().unwrap();
        let (lookup, server) =
            native::tests::server(vec![native::tests::homepage(), "invalid json".into()]);
        resolver.native = Some(NativeResolver::new(lookup, commands.wake.clone()));
        // The next song starts natively, then the current song's automatic
        // reload takes the full extractor while that preparation is in flight.
        let mut preparation = resolver_target("abcdefghijk", 2);
        preparation.lane = ResolverLane::Fast;
        resolver.start(preparation.clone(), None).unwrap();
        let reload = resolver_target("blocked0000", 1);
        resolver.start(reload.clone(), None).unwrap();
        let pid = resolver.warm.as_ref().unwrap().child.id();
        resolver.retain(Some(&reload), Some(&preparation));
        let (socket, _mpv) = IpcStream::pair().unwrap();
        let start = Instant::now();
        loop {
            assert!(resolver.finish(Some(&reload), None).is_none());
            if resolver.native.as_ref().unwrap().target.is_none() {
                break;
            }
            assert!(start.elapsed() < Duration::from_secs(2));
            wait_for_player_input(
                &socket,
                &commands.wake,
                resolver.fd(),
                Some(start + Duration::from_secs(2)),
            )
            .unwrap();
        }
        assert_eq!(resolver.warm.as_ref().unwrap().child.id(), pid);
        assert_eq!(resolver.target.as_ref().unwrap().track_id, 1);
        assert!(resolver.deadline().is_some());
        server.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn native_failures_fall_back_to_the_same_full_resolver() {
        let mut cipher = native::tests::fixture();
        for field in ["formats", "adaptiveFormats"] {
            for format in cipher["streamingData"][field].as_array_mut().unwrap() {
                format.as_object_mut().unwrap().remove("url");
                format["signatureCipher"] = json!("s=encrypted");
            }
        }
        for response in ["invalid json".into(),json!({"videoDetails":{"videoId":"abcdefghijk"},"playabilityStatus":{"status":"UNPLAYABLE"}}).to_string(),cipher.to_string()] {
            let directory = tempfile::tempdir().unwrap();
            let extractor = synthetic_full_extractor(directory.path(), false);
            let mut resolver = StreamResolver::new(&extractor, None, None);
            let pid = resolver.warm.as_ref().unwrap().child.id();
            let (commands,_received) = command_channel().unwrap();
            let (lookup,server) = native::tests::server(vec![native::tests::homepage(),response]);
            resolver.native = Some(NativeResolver::new(lookup,commands.wake.clone()));
            let mut target = resolver_target("abcdefghijk",7);
            target.lane = ResolverLane::Fast;
            resolver.start(target,None).unwrap();
            let stream = resolved(&mut resolver,None);
            assert!(stream.url.ends_with("/full"));
            assert!((stream.gain_db + 2.29).abs() < 1e-9, "{}", stream.gain_db);
            assert_eq!(resolver.warm.as_ref().unwrap().child.id(),pid);
            server.join().unwrap();
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_fast_lookup_in_flight_never_answers_a_full_request() {
        let directory = tempfile::tempdir().unwrap();
        let extractor = synthetic_full_extractor(directory.path(), true);
        let mut resolver = StreamResolver::new(&extractor, None, None);
        let (commands, _received) = command_channel().unwrap();
        let mut native = NativeResolver::new(native::Lookup::new().unwrap(), commands.wake);
        let mut target = resolver_target("abcdefghijk", 1);
        target.lane = ResolverLane::Fast;
        native.target = Some(target.clone());
        let stream = ResolvedStream::parse(
            target.key.clone(),
            b"https://rr1.googlevideo.com/obsolete",
            0.,
            SystemTime::now(),
        )
        .unwrap();
        native.sender.send((native.generation, Ok(stream))).unwrap();
        resolver.native = Some(native);
        target.lane = ResolverLane::Full;
        resolver.start(target, None).unwrap();
        assert!(resolver.finish(None, None).is_none());
        assert!(resolver.native.as_ref().unwrap().target.is_none());
        assert_eq!(resolver.target.as_ref().unwrap().lane, ResolverLane::Full);
    }

    #[cfg(unix)]
    #[test]
    fn resolver_startup_and_request_deadlines_wake_without_output() {
        let directory = tempfile::tempdir().unwrap();
        let interpreter = directory.path().join("python3");
        fs::write(&interpreter, "#!/bin/sh\nexec sleep 60\n").unwrap();
        fs::set_permissions(&interpreter, fs::Permissions::from_mode(0o700)).unwrap();
        let extractor = directory.path().join("extractor");
        fs::write(&extractor, format!("#!{}\n", interpreter.display())).unwrap();
        let (socket, _mpv) = IpcStream::pair().unwrap();
        let (commands, _received) = command_channel().unwrap();
        for ready in [false, true] {
            let mut resolver = StreamResolver::new(&extractor, None, None);
            assert!(resolver.warm.is_some());
            let started = Instant::now().checked_sub(Duration::from_secs(30)).unwrap();
            let warm = resolver.warm.as_mut().unwrap();
            warm.ready = ready;
            warm.started = started;
            if ready {
                resolver.started = started;
                resolver.target = Some(resolver_target("abcdefghijk", 1));
            }
            let deadline = resolver.deadline().unwrap();
            assert!(deadline <= Instant::now());
            assert!(
                !wait_for_player_input(&socket, &commands.wake, resolver.fd(), Some(deadline))
                    .unwrap()
            );
            let result = resolver.finish(None, None);
            if ready {
                assert!(
                    result
                        .unwrap()
                        .1
                        .err()
                        .unwrap()
                        .to_string()
                        .contains("timed out")
                );
            } else {
                assert!(result.is_none());
            }
            assert!(resolver.warm.is_none());
            assert!(resolver.deadline().is_none());
        }
    }

    #[cfg(unix)]
    #[test]
    fn unsupported_interpreters_and_failed_warm_start_use_one_shot_resolution() {
        for fails_start in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let ytdlp = directory.path().join("yt-dlp");
            let interpreter = if fails_start {
                let path = directory.path().join("python3");
                fs::write(
                    &path,
                    "#!/bin/sh\nif [ \"$1\" = -I ]; then exit 23; fi\nexec /bin/sh \"$@\"\n",
                )
                .unwrap();
                fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
                path
            } else {
                PathBuf::from("/bin/sh")
            };
            fs::write(
                &ytdlp,
                format!(
                    "#!{}\nprintf '%s\\n' 'https://rr1.googlevideo.com/audio?mode=oneshot'\n",
                    interpreter.display()
                ),
            )
            .unwrap();
            fs::set_permissions(&ytdlp, fs::Permissions::from_mode(0o700)).unwrap();
            let mut resolver = StreamResolver::new(&ytdlp, None, None);
            resolver
                .start(resolver_target("abcdefghijk", 1), None)
                .unwrap();
            assert!(resolved(&mut resolver, None).url.ends_with("mode=oneshot"));
            assert!(resolver.warm.is_none() && !resolver.warm_enabled);
            resolver
                .start(resolver_target("dQw4w9WgXcQ", 2), None)
                .unwrap();
            assert!(resolved(&mut resolver, None).url.ends_with("mode=oneshot"));
        }
    }

    #[cfg(unix)]
    /// What a stub extractor saw of its own process: descriptors, argv,
    /// environment and the credentials it read from stdin.
    struct HandOff {
        fds: Vec<String>,
        visible: String,
        received: Vec<u8>,
    }

    #[cfg(unix)]
    fn hand_off(warm: bool, credentials: Option<&ExtractorCredentials>) -> HandOff {
        let directory = tempfile::tempdir().unwrap();
        let report = directory.path().join("report");
        // The stub lists its descriptors and copies its argv and environment,
        // then keeps the first line it reads: credentials, or a guest's request.
        let record = format!(
            "ls /dev/fd > {report}.fds\n{{ printf '%s\\n' \"$0\" \"$@\"; env; }} > {report}.visible\n",
            report = report.display()
        );
        let script = if warm {
            let interpreter = directory.path().join("python3");
            fs::write(
                &interpreter,
                format!(
                    "#!/bin/sh\n{record}case \"$*\" in *--use-extractors*) IFS= read -r line; printf '%s\\n' \"$line\" > {report}.stdin;; esac\nprintf '%s\\n' '{{\"ready\":true}}'\nwhile IFS= read -r request; do [ -e {report}.stdin ] || printf '%s\\n' \"$request\" > {report}.stdin; printf '%s\\n' '{{\"urls\":[\"https://rr1.googlevideo.com/warm\"]}}'; done\n",
                    report = report.display()
                ),
            )
            .unwrap();
            fs::set_permissions(&interpreter, fs::Permissions::from_mode(0o700)).unwrap();
            format!("#!{}\n", interpreter.display())
        } else {
            format!(
                "#!/bin/sh\n{record}cat > {report}.stdin\nprintf '%s\\n' 'https://rr1.googlevideo.com/oneshot'\n",
                report = report.display()
            )
        };
        let extractor = directory.path().join("extractor");
        fs::write(&extractor, script).unwrap();
        fs::set_permissions(&extractor, fs::Permissions::from_mode(0o700)).unwrap();
        let mut resolver = StreamResolver::new(&extractor, credentials, None);
        assert_eq!(resolver.warm.is_some(), warm);
        resolver
            .start(resolver_target("abcdefghijk", 1), credentials)
            .unwrap();
        let url = resolved(&mut resolver, credentials).url;
        assert!(url.ends_with(if warm { "warm" } else { "oneshot" }));
        drop(resolver);
        let read = |suffix: &str| fs::read(report.with_extension(suffix)).unwrap_or_default();
        HandOff {
            fds: String::from_utf8(read("fds"))
                .unwrap()
                .lines()
                .map(str::to_owned)
                .collect(),
            visible: String::from_utf8_lossy(&read("visible")).into_owned(),
            received: read("stdin"),
        }
    }

    #[cfg(target_os = "linux")]
    fn credential_writers() -> usize {
        fs::read_dir("/proc/self/task")
            .unwrap()
            .filter(|task| {
                fs::read_to_string(task.as_ref().unwrap().path().join("comm"))
                    .is_ok_and(|name| name.trim() == "credentials")
            })
            .count()
    }

    #[cfg(unix)]
    #[test]
    fn an_unread_oversized_account_never_blocks_the_player_thread() {
        let directory = tempfile::tempdir().unwrap();
        // Far beyond any pipe buffer.
        let credentials = ExtractorCredentials {
            plugin_dir: directory.path().to_owned(),
            source: PlaybackCredentials {
                message: vec![b'x'; 1 << 20],
            },
        };
        for (warm, behaviour) in [
            (true, "exec sleep 60"),
            (false, "exec sleep 60"),
            (false, "exit 0"),
        ] {
            let extractor = directory.path().join("extractor");
            if warm {
                let interpreter = directory.path().join("python3");
                fs::write(&interpreter, format!("#!/bin/sh\n{behaviour}\n")).unwrap();
                fs::set_permissions(&interpreter, fs::Permissions::from_mode(0o700)).unwrap();
                fs::write(&extractor, format!("#!{}\n", interpreter.display())).unwrap();
            } else {
                fs::write(&extractor, format!("#!/bin/sh\n{behaviour}\n")).unwrap();
            }
            fs::set_permissions(&extractor, fs::Permissions::from_mode(0o700)).unwrap();
            let start = Instant::now();
            let mut resolver = StreamResolver::new(&extractor, Some(&credentials), None);
            assert_eq!(resolver.warm.is_some(), warm);
            resolver
                .start(resolver_target("abcdefghijk", 1), Some(&credentials))
                .unwrap();
            assert!(
                start.elapsed() < Duration::from_secs(1),
                "the player thread waited for the extractor to read its account"
            );
            let pid = match &resolver.warm {
                Some(warm) => warm.child.id(),
                None => resolver.one_shot.as_ref().unwrap().child.id(),
            };
            let expired = Instant::now().checked_sub(RESOLVER_TIMEOUT).unwrap();
            resolver.started = expired;
            if let Some(warm) = &mut resolver.warm {
                warm.started = expired;
            }
            // The deadline still runs on this thread and reaps the stalled extractor.
            resolver.finish(None, Some(&credentials));
            assert!(!alive(pid), "{behaviour}: extractor survived its deadline");
            drop(resolver);
        }
        // Each writer ends once its extractor is gone. Only Linux lists thread names.
        #[cfg(target_os = "linux")]
        {
            let deadline = Instant::now() + Duration::from_secs(5);
            while credential_writers() > 0 {
                assert!(Instant::now() < deadline, "a credential writer is stuck");
                thread::sleep(Duration::from_millis(10));
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn credentials_reach_the_extractor_only_through_its_stdin() {
        let directory = tempfile::tempdir().unwrap();
        let credentials = signed_in(
            synthetic_credentials("INVENTED_HANDOFF_SECRET"),
            directory.path(),
        );
        for warm in [false, true] {
            let guest = hand_off(warm, None);
            let account = hand_off(warm, Some(&credentials));
            assert_eq!(account.received, credentials.source.message, "warm: {warm}");
            if warm {
                let request: Value = serde_json::from_slice(&guest.received).unwrap();
                assert_eq!(
                    request["url"],
                    "https://music.youtube.com/watch?v=abcdefghijk"
                );
            } else {
                assert!(guest.received.is_empty());
            }
            for handoff in [&guest, &account] {
                assert!(!handoff.visible.contains("INVENTED_HANDOFF_SECRET"));
                assert!(!handoff.visible.contains("brand||user"));
            }
            assert!(account.visible.contains("--use-extractors"));
            // stdin replaces /dev/null or carries requests; no other descriptor appears.
            assert_eq!(account.fds, guest.fds, "warm: {warm}");
        }
    }

    #[test]
    fn paused_streams_refresh_five_minutes_before_their_earliest_url_expires() {
        let at = |seconds| Some(UNIX_EPOCH + Duration::from_secs(seconds));
        assert_eq!(
            stream_refresh_time(
                "https://rr1.googlevideo.com/videoplayback?expire=1800000000&sparams=expire%2Cei&id=x"
            ),
            at(1_800_000_000 - 300)
        );
        // Video plays separate audio and video URLs; the first to lapse decides.
        let video = "https://rr1.googlevideo.com/v?id=x&expire=1800000600";
        let audio = "https://rr1.googlevideo.com/a?expire=1800000000&id=y";
        assert_eq!(
            stream_refresh_time(&format!(
                "edl://%{}%{video};!new_stream;%{}%{audio}",
                video.len(),
                audio.len()
            )),
            at(1_800_000_000 - 300)
        );
        for path in [
            "/tmp/silence.wav",
            "https://rr1.googlevideo.com/videoplayback?sparams=expire%2Cei&noexpire=1",
            // Past the end of SystemTime.
            "https://rr1.googlevideo.com/videoplayback?expire=18446744073709551615",
        ] {
            assert_eq!(stream_refresh_time(path), None);
        }
    }

    #[test]
    fn prepared_streams_expire_and_are_bound_to_the_recording_and_quality() {
        let key = StreamKey {
            url: "https://music.youtube.com/watch?v=abcdefghijk".into(),
            quality: AudioQuality::Normal,
            video: None,
        };
        let now = SystemTime::now();
        let expiry = now.duration_since(UNIX_EPOCH).unwrap().as_secs() + 120;
        let stream = ResolvedStream::parse(
            key.clone(),
            format!("https://rr1.googlevideo.com/videoplayback?expire={expiry}&id=x\n").as_bytes(),
            0.,
            now,
        )
        .unwrap();
        assert!(stream.matches(&key));
        // A queued song keeps its link for as long as YouTube says it lasts, so a paused app
        // does not look the next song up again every few minutes.
        let hours = now.duration_since(UNIX_EPOCH).unwrap().as_secs() + 6 * 60 * 60;
        let lasting = ResolvedStream::parse(
            key.clone(),
            format!("https://rr1.googlevideo.com/videoplayback?expire={hours}&id=x\n").as_bytes(),
            0.,
            now,
        )
        .unwrap();
        assert_eq!(
            lasting.expires,
            UNIX_EPOCH + Duration::from_secs(hours - 60)
        );
        assert!(!stream.matches(&StreamKey {
            quality: AudioQuality::High,
            ..key.clone()
        }));
        assert!(!stream.matches(&StreamKey {
            url: "other".into(),
            ..key.clone()
        }));
        assert!(
            ResolvedStream::parse(
                key.clone(),
                b"https://rr1.googlevideo.com/videoplayback?expire=1",
                0.,
                now
            )
            .is_err()
        );
        for url in [
            "http://rr1.googlevideo.com/x",
            "https://example.com/x",
            "file:///tmp/private",
        ] {
            assert!(ResolvedStream::parse(key.clone(), url.as_bytes(), 0., now).is_err());
        }
        let video = ResolvedStream::parse(
            key,
            b"https://rr1.googlevideo.com/video\nhttps://rr1.googlevideo.com/audio\n",
            0.,
            now,
        )
        .unwrap();
        assert!(video.url.contains(";!new_stream;"));
    }

    #[test]
    fn loudness_normalization_matches_the_youtube_music_web_player() {
        // audioConfig as a cookie-free player response sends it (fJ9rUzIMcZQ).
        let config = json!({
            "perceptualLoudnessDb": -11.71,
            "enablePerFormatLoudness": true,
            "trackAbsoluteLoudnessLkfs": -11.71,
            "loudnessTargetLkfs": -14,
            "loudnessNormalizationConfig": {
                "applyStatefulNormalization": false,
                "preserveStatefulLoudnessTarget": true,
                "maxStatefulTimeThresholdSec": 300,
                "minimumLoudnessTargetLkfs": -31
            }
        });
        for (config, format, expected) in [
            // A loud track is turned down to the -14 LKFS target.
            (
                &config,
                json!({"itag": 251, "loudnessDb": 2.29, "trackAbsoluteLoudnessLkfs": -11.71}),
                -2.29,
            ),
            // The selected format's measurement wins over the track's.
            (
                &config,
                json!({"itag": 251, "loudnessDb": 3.4, "trackAbsoluteLoudnessLkfs": -10.6}),
                -3.4,
            ),
            // Muxed formats carry no loudness, so the track's applies.
            (&config, json!({"itag": 18}), -2.29),
            // A quiet track is never boosted.
            (
                &config,
                json!({"loudnessDb": -6.5, "trackAbsoluteLoudnessLkfs": -20.5}),
                0.,
            ),
            // Older responses carry only loudnessDb, relative to the target.
            (&json!({"loudnessDb": 5.}), json!({}), -5.),
            (&json!({"loudnessDb": -3.}), json!({}), 0.),
            // An absolute loudness without a target leaves the track alone.
            (&json!({"trackAbsoluteLoudnessLkfs": -8.}), json!({}), 0.),
            (&Value::Null, Value::Null, 0.),
        ] {
            let gain = normalization_gain(config, &format);
            assert!((gain - expected).abs() < 1e-9, "{format}: {gain}");
        }
    }
}
