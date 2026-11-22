use super::mpv::{
    MpvCore, PlaybackState, load_command, read_message, request, write_command, write_pause,
};
use super::native;
use super::stream::{
    NativeResolver, PreparedTarget, ResolvedStream, ResolverLane, StreamKey, StreamResolver,
    stream_refresh_time,
};
use super::ytdlp::{
    ExtractorCredentials, account_credentials, account_playback_supported, write_account_extractor,
};
use super::{PlayOptions, PlayerCommand, PlayerEvent, PlayerOptions, VideoQuality};
use crate::auth::PlaybackCredentials;
#[cfg(not(target_os = "macos"))]
use crate::auth::private_runtime;
#[cfg(target_os = "macos")]
use crate::auth::validate_private_runtime;
use crate::os::{AsWaitable, IpcStream, Waitable, Waker, ipc_path, wait_readable};
use crate::video::NativeVideo;
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
#[cfg(unix)]
use std::{fs, os::unix::fs::PermissionsExt};
use std::{
    path::Path,
    sync::{
        Arc,
        mpsc::{self, Receiver},
    },
    thread,
    time::{Duration, Instant, SystemTime},
};

pub(super) fn wait_for_player_input(
    socket: &IpcStream,
    wake: &Waker,
    resolver: Option<Waitable>,
    deadline: Option<Instant>,
) -> Result<bool> {
    let [socket_ready, woken, _] = wait_readable(
        [
            Some(socket.as_waitable()),
            Some(wake.as_waitable()),
            resolver,
        ],
        deadline,
    )
    .context("Could not wait for the player")?;
    if woken {
        wake.drain()
            .context("Could not read the player command wakeup")?;
    }
    Ok(socket_ready)
}

struct AttachedVideo {
    video: Arc<NativeVideo>,
    ready: bool,
}

impl Drop for AttachedVideo {
    fn drop(&mut self) {
        self.video.stop();
    }
}

fn replace_playback_credentials(
    socket: &mut IpcStream,
    pending: &mut Vec<u8>,
    current: &mut Option<ExtractorCredentials>,
    source: Option<PlaybackCredentials>,
    plugin_dir: &Path,
    state: &mut PlaybackState,
    events: &async_channel::Sender<PlayerEvent>,
) -> Result<()> {
    let next = source.map(|source| ExtractorCredentials {
        plugin_dir: plugin_dir.to_owned(),
        source,
    });
    request(socket, pending, json!(["stop"]), |value| {
        state.message(value, events)
    })?;
    let client = request(socket, pending, json!(["client_name"]), |value| {
        state.message(value, events)
    })?;
    let client = client
        .as_str()
        .context("mpv did not identify its IPC client")?;
    let marker = json!(["yt-music-credentials-stopped"]);
    let mut marker_seen = false;
    // mpv's stop clears the playlist on the core thread. A targeted message then
    // joins the same FIFO as start-file/end-file (mpv player/client.c), unlike IPC
    // replies. Consume it before trusting file_active: a start may still be queued.
    request(
        socket,
        pending,
        json!(["script-message-to", client, marker[0]]),
        |value| {
            marker_seen |= value["event"] == "client-message" && value["args"] == marker;
            state.message(value, events);
        },
    )?;
    // Drain the old file's events before accepting loads under the new identity.
    let deadline = Instant::now() + Duration::from_secs(5);
    while !marker_seen || state.file_active {
        let value = read_message(socket, pending, deadline)?;
        marker_seen |= value["event"] == "client-message" && value["args"] == marker;
        state.message(value, events);
    }
    *current = next;
    Ok(())
}

struct PendingPlay {
    target: PreparedTarget,
    title: String,
    options: PlayOptions,
}

fn playback_runtime(parent: &Path) -> Result<tempfile::TempDir> {
    let mut builder = tempfile::Builder::new();
    builder.prefix("yt-music-gpui-");
    // On Windows the folder inherits the private runtime's access instead.
    #[cfg(unix)]
    builder.permissions(fs::Permissions::from_mode(0o700));
    builder
        .tempdir_in(parent)
        .context("Could not create the private playback directory")
}

pub(super) fn run(
    mut options: PlayerOptions,
    commands: Receiver<PlayerCommand>,
    wake: &Arc<Waker>,
    events: &async_channel::Sender<PlayerEvent>,
    ytdlp: &Path,
    lookup: Option<native::Lookup>,
) -> Result<()> {
    // mpv cannot bind a control socket path longer than 102 bytes on macOS,
    // and $TMPDIR can be far longer. /tmp is short and shared, so the folder
    // created there is checked to be ours alone before anything goes in it.
    #[cfg(target_os = "macos")]
    let runtime = {
        let runtime = playback_runtime(Path::new("/tmp"))?;
        validate_private_runtime(runtime.path())
            .context("Could not create a private playback directory in /tmp")?;
        runtime
    };
    #[cfg(not(target_os = "macos"))]
    let runtime = playback_runtime(&private_runtime()?)?;
    let socket_path = ipc_path(runtime.path())?;
    let plugin_dir = write_account_extractor(runtime.path())?;
    let mut account_supported = options
        .check_account_at_startup
        .then(|| account_playback_supported(ytdlp));
    let mut supports_account =
        || *account_supported.get_or_insert_with(|| account_playback_supported(ytdlp));
    let mut account_warned = false;
    let mut account_notice = None;
    let mut credentials = account_credentials(
        options.credentials.take(),
        &mut supports_account,
        account_warned,
        &mut account_notice,
    )
    .map(|source| ExtractorCredentials {
        plugin_dir: plugin_dir.clone(),
        source,
    });
    // Destroy the core before removing the private runtime.
    let core = MpvCore::new(&options, &socket_path)?;
    // Cancel/reap the resolver before credentials or the core can drop.
    let native = lookup.map(|lookup| NativeResolver::new(lookup, wake.clone()));
    let mut resolver = StreamResolver::new(ytdlp, credentials.as_ref(), native);
    if options
        .initial_video
        .is_some_and(VideoQuality::uses_full_resolver)
    {
        resolver.warm_up(credentials.as_ref());
    }
    let mut pending_play: Option<PendingPlay> = None;
    let mut next_target: Option<PreparedTarget> = None;
    let mut prepared: Option<(PreparedTarget, ResolvedStream)> = None;
    // The paused song and when its stream must be refreshed, once per pause.
    let mut paused_stream: Option<(u64, Option<SystemTime>)> = None;
    // Every actor exit frees the renderer before destroying the core.
    let mut video: Option<AttachedVideo> = None;
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut socket = loop {
        match IpcStream::connect(&socket_path) {
            Ok(socket) => break socket,
            Err(_) if Instant::now() < deadline => {
                ensure!(!core.is_shutdown(), "mpv exited during startup");
                thread::sleep(Duration::from_millis(25));
            }
            Err(error) => return Err(error).context("mpv did not open its control socket"),
        }
    };
    socket.set_write_timeout(Some(Duration::from_secs(2)))?;
    let mut state = PlaybackState {
        captions_enabled: options.captions,
        ..PlaybackState::default()
    };
    let mut pending = Vec::new();
    // UI and MPRIS own volume. Delayed mpv acknowledgments must not
    // replace the retained level with an earlier mute's zero.
    for (id, name) in [
        "time-pos",
        "duration",
        "pause",
        "paused-for-cache",
        "speed",
        "demuxer-cache-time",
    ]
    .iter()
    .enumerate()
    {
        write_command(&mut socket, json!(["observe_property", id + 1, name]), None)?;
    }
    // Position observers use 1 + 6n; reserve zero for seekability.
    write_command(
        &mut socket,
        json!(["observe_property", 0, "seekable"]),
        None,
    )?;
    let mut buffer = [0_u8; 16384];
    loop {
        loop {
            match commands.try_recv() {
                Ok(PlayerCommand::Prepare(target)) => {
                    if target.as_ref().is_some_and(|target| {
                        prepared.as_ref().is_some_and(|(current, stream)| {
                            current.track_id == target.track_id && stream.matches(&target.key)
                        })
                    }) {
                        continue;
                    }
                    next_target = target.map(|mut target| {
                        target.lane = target.key.lane(true, credentials.is_some());
                        target
                    });
                    prepared = None;
                    state.discard_upcoming();
                    write_command(&mut socket, json!(["playlist-clear"]), None)?;
                    // A queue render can prepare the following song while Play is
                    // resolving. Interactive playback keeps the single request slot.
                    if pending_play.is_none() {
                        if let Some(target) = next_target.clone() {
                            let _ = resolver.start(target, credentials.as_ref());
                        } else {
                            resolver.cancel();
                        }
                    }
                }
                Ok(PlayerCommand::Play {
                    track_id,
                    url,
                    title,
                    options,
                }) => {
                    let key = StreamKey {
                        url: url.clone(),
                        quality: options.quality,
                        video: options.video,
                    };
                    // A reload that skips the fast lookup must not get its stream back.
                    // Any other play of the prepared song, such as resuming the restored
                    // song partway in, loads the prepared stream under its own ID.
                    let resolved = prepared.take().filter(|(target, stream)| {
                        stream.matches(&key)
                            && (target.track_id == track_id || !options.full_lookup)
                    });
                    // mpv moves to an appended entry only from a file it is playing.
                    // An idle player, as at startup, loads the prepared stream instead.
                    let prefetched = resolved
                        .as_ref()
                        .is_some_and(|(target, _)| target.track_id == track_id)
                        && state.file_active
                        && state.entries.values().any(|entry| *entry == track_id);
                    next_target = None;
                    pending_play = None;
                    let already_restarted = state.restarted && state.current == Some(track_id);
                    state.invalidate_seeks();
                    state.restarted = already_restarted;
                    state.desired = Some(track_id);
                    state.paused = options.paused;
                    state.pause_target = Some(options.paused);
                    // Adoption must refresh metadata even if file-loaded preceded Play.
                    state.pause_after_load = true;
                    state.upcoming = None;
                    if prefetched {
                        resolver.cancel();
                        if state.current != Some(track_id) {
                            state.loaded = false;
                            state.pause_after_load = true;
                            // mpv may have reached this last prepared entry before
                            // its start-file event reaches us. Weak leaves it playing.
                            write_command(&mut socket, json!(["playlist-next", "weak"]), None)?;
                        } else if state.loaded {
                            // EOF may have transitioned before the UI consumed Ended.
                            let _ = events.send_blocking(PlayerEvent::Started {
                                track_id,
                                paused: options.paused,
                                captions: state.captions.contains_key(&track_id),
                            });
                        }
                        state.pause_revision += 1;
                        write_pause(&mut socket, options.paused, state.pause_revision)?;
                    } else {
                        state.loaded = false;
                        #[cfg(test)]
                        if Path::new(&url).is_absolute() {
                            resolver.cancel();
                            write_command(
                                &mut socket,
                                load_command(&url, &title, 0., options),
                                Some(json!(track_id)),
                            )?;
                            continue;
                        }
                        write_command(&mut socket, json!(["stop"]), None)?;
                        // The append acknowledgement can still be queued when the
                        // user clicks Next. Reuse its resolved URL in that case.
                        if let Some((_, stream)) = resolved {
                            resolver.cancel();
                            state.load_captions(track_id, &stream);
                            write_command(
                                &mut socket,
                                load_command(&stream.url, &title, stream.gain_db, options),
                                Some(json!(track_id)),
                            )?;
                            continue;
                        }
                        let target = PreparedTarget {
                            lane: if options.full_lookup {
                                ResolverLane::Full
                            } else {
                                key.lane(false, credentials.is_some())
                            },
                            key,
                            track_id,
                        };
                        pending_play = Some(PendingPlay {
                            target: target.clone(),
                            title,
                            options,
                        });
                        if resolver.start(target, credentials.as_ref()).is_err() {
                            pending_play = None;
                            stream_error(track_id, events);
                        }
                    }
                }
                Ok(PlayerCommand::Pause(paused)) => {
                    state.pause_revision += 1;
                    state.pause_target = Some(paused);
                    state.paused = paused;
                    state.pause_after_load = !state.loaded;
                    write_pause(&mut socket, paused, state.pause_revision)?;
                }
                Ok(PlayerCommand::Captions(enabled)) => {
                    state.captions_enabled = enabled;
                    state.apply_captions(&mut socket)?;
                }
                Ok(PlayerCommand::Seek(request)) => state.queue_seek(request, events),
                Ok(PlayerCommand::Json(command)) => write_command(&mut socket, command, None)?,
                Ok(PlayerCommand::Credentials { next, reply }) => {
                    // Reap even an idle resolver: it holds the old account in memory.
                    resolver.stop();
                    pending_play = None;
                    next_target = None;
                    prepared = None;
                    state.upcoming = None;
                    let next = account_credentials(
                        next,
                        &mut supports_account,
                        account_warned,
                        &mut account_notice,
                    );
                    state.invalidate_seeks();
                    state.desired = None;
                    state.loaded = false;
                    let result = replace_playback_credentials(
                        &mut socket,
                        &mut pending,
                        &mut credentials,
                        next,
                        &plugin_dir,
                        &mut state,
                        events,
                    );
                    if result.is_ok() {
                        let native = resolver.native.take();
                        if let Some(native) = &native {
                            native.lookup.reset();
                        }
                        resolver = StreamResolver::new(ytdlp, credentials.as_ref(), native);
                    }
                    let _ = reply.send_blocking(
                        result
                            .as_ref()
                            .map(|_| ())
                            .map_err(|error| format!("{error:#}")),
                    );
                    result?;
                }
                Ok(PlayerCommand::AttachVideo { video: next }) => {
                    let result: Result<()> = (|| {
                        ensure!(video.is_none(), "A video surface is already attached");
                        next.start(core.0.as_ptr(), wake)?;
                        let attached = AttachedVideo {
                            video: next,
                            ready: false,
                        };
                        video = Some(attached);
                        Ok(())
                    })();
                    if let Err(error) = result {
                        let _ = events.send_blocking(PlayerEvent::VideoError {
                            message: format!("{error:#}"),
                        });
                    }
                    let _ = events.send_blocking(PlayerEvent::VideoChanged);
                }
                Ok(PlayerCommand::DetachVideo {
                    video: current,
                    reply,
                }) => {
                    if video
                        .as_ref()
                        .is_some_and(|video| Arc::ptr_eq(&video.video, &current))
                    {
                        let result = request(
                            &mut socket,
                            &mut pending,
                            json!(["set_property", "vid", "no"]),
                            |value| state.message(value, events),
                        );
                        video.take();
                        if result.is_err() {
                            let _ = events.send_blocking(PlayerEvent::VideoError {
                                message: "Could not disable the video track".into(),
                            });
                        }
                    }
                    let _ = reply.send_blocking(());
                }
                Ok(PlayerCommand::Stop) => {
                    resolver.cancel();
                    pending_play = None;
                    next_target = None;
                    prepared = None;
                    state.upcoming = None;
                    state.invalidate_seeks();
                    state.desired = None;
                    state.loaded = false;
                    write_command(&mut socket, json!(["stop"]), None)?;
                }
                Ok(PlayerCommand::Quit) | Err(mpsc::TryRecvError::Disconnected) => {
                    resolver.stop();
                    let _ = write_command(&mut socket, json!(["quit"]), None);
                    return Ok(());
                }
                Err(mpsc::TryRecvError::Empty) => break,
            }
        }
        resolver.retain(
            pending_play.as_ref().map(|play| &play.target),
            next_target.as_ref(),
        );
        if let Some((target, result)) = resolver.finish(
            pending_play.as_ref().map(|play| &play.target),
            credentials.as_ref(),
        ) {
            if let Some(play) = pending_play.take_if(|play| {
                play.target.track_id == target.track_id && play.target.key == target.key
            }) {
                match result {
                    Ok(stream) => {
                        let options = PlayOptions {
                            paused: state.paused,
                            ..play.options
                        };
                        state.load_captions(target.track_id, &stream);
                        write_command(
                            &mut socket,
                            load_command(&stream.url, &play.title, stream.gain_db, options),
                            Some(json!(target.track_id)),
                        )?;
                        if let Some(target) = next_target.clone()
                            && !prepared.as_ref().is_some_and(|(current, stream)| {
                                current.track_id == target.track_id && stream.matches(&target.key)
                            })
                        {
                            let _ = resolver.start(target, credentials.as_ref());
                        }
                    }
                    Err(_) => {
                        next_target = None;
                        stream_error(target.track_id, events);
                    }
                }
            } else if next_target
                .as_ref()
                .is_some_and(|next| next.track_id == target.track_id && next.key == target.key)
                && let Ok(stream) = result
            {
                prepared = Some((target, stream));
            }
        }
        // A full preparation can finish while a native click is resolving. Hold
        // it until that click loads, so append cannot replace its pending load.
        if pending_play.is_none()
            && let Some((target, stream)) = &prepared
            && state.upcoming != Some(target.track_id)
            // After a natural transition the prepared song is already playing.
            && state.current != Some(target.track_id)
        {
            let mut command = load_command(
                &stream.url,
                "",
                stream.gain_db,
                PlayOptions {
                    quality: target.key.quality,
                    video: target.key.video,
                    ..Default::default()
                },
            );
            command["flags"] = json!("append");
            state.upcoming = Some(target.track_id);
            state.load_captions(target.track_id, stream);
            write_command(
                &mut socket,
                command,
                Some(json!(format!("prepare:{}", target.track_id))),
            )?;
            #[cfg(test)]
            let _ = events.send_blocking(PlayerEvent::Prepared {
                video_id: stream.key.url.rsplit_once("v=").unwrap().1.to_owned(),
            });
        }
        if let Some((target, _)) = prepared.take_if(|(target, stream)| {
            state.current != Some(target.track_id) && SystemTime::now() >= stream.expires
        }) {
            state.discard_upcoming();
            write_command(&mut socket, json!(["playlist-clear"]), None)?;
            let _ = resolver.start(target, credentials.as_ref());
        }
        if let Some(attached) = &mut video {
            let result = attached.video.ready_for_player().and_then(|ready| {
                if ready && !attached.ready {
                    // A native lookup can load video before EGL/libmpv finishes
                    // starting. Enabling it sooner leaves mpv with no renderer.
                    request(
                        &mut socket,
                        &mut pending,
                        json!(["set_property", "vid", "auto"]),
                        |value| state.message(value, events),
                    )
                    .map_err(|_| anyhow::anyhow!("Could not enable the video track"))?;
                    attached.ready = true;
                    let _ = events.send_blocking(PlayerEvent::VideoChanged);
                }
                Ok(())
            });
            if let Err(error) = result {
                let _ = request(
                    &mut socket,
                    &mut pending,
                    json!(["set_property", "vid", "no"]),
                    |value| state.message(value, events),
                );
                video.take();
                let _ = events.send_blocking(PlayerEvent::VideoError {
                    message: error.to_string(),
                });
                let _ = events.send_blocking(PlayerEvent::VideoChanged);
            }
        }
        if state.loaded
            && state.current == state.desired
            && let Some(message) = account_notice.take()
        {
            // Activation and stream replacement clear transient UI notices.
            // Deliver this after Started, once guest playback is actually ready.
            account_warned = true;
            let _ = events.send_blocking(PlayerEvent::Notice(message));
        }
        let _ = &credentials;
        state.write_seek(&mut socket)?;
        ensure!(!core.is_shutdown(), "mpv stopped unexpectedly");
        // mpv reconnects a dropped stream with the same URL, which fails once
        // the URL expires, and the song then ends where its buffer runs out.
        // A paused song can outlive its URL, so before that ask the app to
        // reload it, still paused, at the same position.
        let paused_track = state.current.filter(|id| {
            state.paused && state.loaded && state.desired == Some(*id) && pending_play.is_none()
        });
        if paused_track != paused_stream.map(|(id, _)| id) {
            paused_stream = paused_track.map(|id| {
                let path = request(
                    &mut socket,
                    &mut pending,
                    json!(["get_property", "path"]),
                    |value| state.message(value, events),
                );
                let refresh = path
                    .ok()
                    .as_ref()
                    .and_then(Value::as_str)
                    .and_then(stream_refresh_time);
                (id, refresh)
            });
        }
        if let Some((track_id, Some(refresh))) = paused_stream
            && SystemTime::now() >= refresh
        {
            paused_stream = Some((track_id, None));
            let buffered_to_end = request(
                &mut socket,
                &mut pending,
                json!(["get_property", "demuxer-cache-state"]),
                |value| state.message(value, events),
            )
            .is_ok_and(|cache| cache["eof"] == true);
            if !buffered_to_end {
                let _ = events.send_blocking(PlayerEvent::StreamExpiring { track_id });
            }
        }
        // Expiry is wall-clock time, which a monotonic wait doesn't follow across
        // clock changes or suspend, so recheck it at least once a minute.
        let expiry = prepared
            .as_ref()
            .filter(|(target, _)| state.current != Some(target.track_id))
            .map(|(_, stream)| stream.expires)
            .into_iter()
            .chain(paused_stream.and_then(|(_, refresh)| refresh))
            .min()
            .map(|expires| {
                Instant::now()
                    + expires
                        .duration_since(SystemTime::now())
                        .unwrap_or_default()
                        .min(Duration::from_secs(60))
            });
        let deadline = resolver.deadline().into_iter().chain(expiry).min();
        if pending.contains(&b'\n')
            || wait_for_player_input(&socket, wake, resolver.fd(), deadline)?
        {
            state.read_messages(&mut socket, &mut pending, &mut buffer, events)?;
        }
    }
}

fn stream_error(track_id: u64, events: &async_channel::Sender<PlayerEvent>) {
    let _ = events.send_blocking(PlayerEvent::Error {
        track_id: Some(track_id),
        message: "YouTube could not provide this stream. Try another track or update yt-dlp."
            .into(),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use crate::player::stream::tests::resolver_target;
    use crate::player::{
        PlaybackSpeed, command_channel, find_ytdlp,
        mpv::tests::socket_message,
        ytdlp::tests::{signed_in, synthetic_credentials},
    };
    use std::io::{Read, Write};
    #[cfg(unix)]
    use std::os::fd::{AsFd, AsRawFd};

    #[cfg(unix)]
    #[test]
    fn playback_runtime_stays_private_under_the_desktop_runtime_and_cleans_up() {
        use std::os::unix::fs::MetadataExt;
        let parent = tempfile::Builder::new()
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let runtime = playback_runtime(parent.path()).unwrap();
        let path = runtime.path().to_owned();
        assert_eq!(path.parent(), Some(parent.path()));
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o700);
        write_account_extractor(&path).unwrap();
        drop(runtime);
        assert!(!path.exists());
    }

    #[test]
    fn resolver_output_wakes_the_actor_without_mpv_or_commands() {
        let (socket, _mpv) = IpcStream::pair().unwrap();
        let (resolver, mut output) = IpcStream::pair().unwrap();
        let (commands, _received) = command_channel().unwrap();
        output.write_all(b"{\"ready\":true}\n").unwrap();
        let start = Instant::now();
        assert!(
            !wait_for_player_input(&socket, &commands.wake, Some(resolver.as_waitable()), None)
                .unwrap()
        );
        assert!(
            start.elapsed() < Duration::from_millis(60),
            "resolver output waited for the actor's timer"
        );
    }

    #[cfg(unix)]
    #[test]
    fn one_shot_exit_wakes_the_actor_without_mpv_or_commands() {
        let directory = tempfile::tempdir().unwrap();
        let extractor = directory.path().join("extractor");
        fs::write(
            &extractor,
            "#!/bin/sh\nsleep 0.15\nprintf '%s\\n' 'https://rr1.googlevideo.com/audio'\n",
        )
        .unwrap();
        fs::set_permissions(&extractor, fs::Permissions::from_mode(0o700)).unwrap();
        let mut resolver = StreamResolver::new(&extractor, None, None);
        resolver
            .start(resolver_target("abcdefghijk", 1), None)
            .unwrap();
        let (socket, _mpv) = IpcStream::pair().unwrap();
        let (commands, _received) = command_channel().unwrap();
        let start = Instant::now();
        assert!(
            !wait_for_player_input(
                &socket,
                &commands.wake,
                resolver.fd(),
                Some(start + Duration::from_secs(2))
            )
            .unwrap()
        );
        let (_, stream) = resolver
            .finish(None, None)
            .expect("child exit must wake the actor");
        assert!(stream.is_ok());
        assert!(start.elapsed() < Duration::from_secs(1));
        assert!(resolver.fd().is_none() && resolver.deadline().is_none());
    }

    #[test]
    fn idle_wait_blocks_until_a_command_and_deadline_wait_expires() {
        let (socket, _mpv) = IpcStream::pair().unwrap();
        let (commands, _received) = command_channel().unwrap();
        thread::scope(|scope| {
            let (completed, result) = mpsc::channel();
            let wake = &commands.wake;
            let socket = &socket;
            scope.spawn(move || {
                completed
                    .send(wait_for_player_input(socket, wake, None, None))
                    .unwrap();
            });
            let premature = result.recv_timeout(Duration::from_millis(200));
            commands.send(PlayerCommand::Stop).unwrap();
            assert!(premature.is_err(), "an idle actor must not wake on a timer");
            assert!(
                !result
                    .recv_timeout(Duration::from_secs(1))
                    .unwrap()
                    .unwrap()
            );
        });
        let start = Instant::now();
        assert!(
            !wait_for_player_input(
                &socket,
                &commands.wake,
                None,
                Some(start + Duration::from_millis(25))
            )
            .unwrap()
        );
        assert!(start.elapsed() >= Duration::from_millis(25));
        assert!(start.elapsed() < Duration::from_millis(500));
    }

    #[cfg(unix)]
    #[test]
    fn command_wakeups_coalesce_without_losing_commands_or_socket_readiness() {
        let (commands, received) = command_channel().unwrap();
        let (mut mpv, socket) = IpcStream::pair().unwrap();
        commands.send(PlayerCommand::Stop).unwrap();
        commands.clone().send(PlayerCommand::Pause(true)).unwrap();
        assert!(!wait_for_player_input(&socket, &commands.wake, None, None).unwrap());
        assert!(matches!(received.try_recv(), Ok(PlayerCommand::Stop)));
        assert!(matches!(
            received.try_recv(),
            Ok(PlayerCommand::Pause(true))
        ));
        let mut wake = libc::pollfd {
            fd: commands.wake.as_fd().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        assert_eq!(unsafe { libc::poll(&mut wake, 1, 0) }, 0);
        assert_ne!(
            unsafe { libc::fcntl(wake.fd, libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );

        mpv.write_all(b"mpv reply\n").unwrap();
        commands.send(PlayerCommand::Quit).unwrap();
        assert!(wait_for_player_input(&socket, &commands.wake, None, None).unwrap());
        let mut reply = [0; 10];
        (&socket).read_exact(&mut reply).unwrap();
        assert_eq!(&reply, b"mpv reply\n");
        assert!(matches!(received.try_recv(), Ok(PlayerCommand::Quit)));
        drop(received);
        assert!(commands.send(PlayerCommand::Stop).is_err());
        assert_eq!(unsafe { libc::poll(&mut wake, 1, 0) }, 0);
    }

    #[test]
    #[ignore = "requires libmpv and yt-dlp; no network or display"]
    fn headless_worker_shuts_down_after_ipc_controls() {
        let (commands, receiver) = command_channel().unwrap();
        let wake = commands.wake.clone();
        let (events, received) = async_channel::unbounded();
        let (finished, completion) = mpsc::channel();
        let worker = thread::spawn(move || {
            let result = run(
                PlayerOptions::default(),
                receiver,
                &wake,
                &events,
                &find_ytdlp().unwrap(),
                None,
            );
            finished.send(result).unwrap();
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut ready = false;
        while Instant::now() < deadline && !ready {
            match received.try_recv() {
                Ok(PlayerEvent::Speed(value)) if value == PlaybackSpeed::default() => ready = true,
                _ => thread::sleep(Duration::from_millis(10)),
            }
        }
        commands.send(PlayerCommand::Quit).unwrap();
        completion
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        worker.join().unwrap();
        assert!(ready, "headless IPC must initialize before clean shutdown");
    }

    fn credential_barrier_command(server: &mut IpcStream) -> Value {
        let name = socket_message(server);
        assert_eq!(name["command"], json!(["client_name"]));
        writeln!(
            server,
            "{}",
            json!({
                "request_id":name["request_id"], "error":"success", "data":"test-ipc",
            })
        )
        .unwrap();
        let marker = socket_message(server);
        assert_eq!(marker["command"][0], "script-message-to");
        assert_eq!(marker["command"][1], "test-ipc");
        marker
    }

    fn reply_to_credential_barrier(server: &mut IpcStream) {
        let marker = credential_barrier_command(server);
        // Also cover the event arriving before its command reply.
        writeln!(
            server,
            "{}",
            json!({
                "event":"client-message", "args":[marker["command"][2]],
            })
        )
        .unwrap();
        writeln!(
            server,
            "{}",
            json!({
                "request_id":marker["request_id"], "error":"success",
            })
        )
        .unwrap();
    }

    #[test]
    fn credential_changes_wait_for_old_file_events() {
        let (mut client, mut server) = IpcStream::pair().unwrap();
        client
            .set_read_timeout(Some(Duration::from_millis(80)))
            .unwrap();
        server
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let plugin_dir = Path::new("/private/synthetic-plugin-root");
        let mut current = Some(signed_in(
            synthetic_credentials("INVENTED_FIRST"),
            plugin_dir,
        ));
        let worker = thread::spawn(move || {
            for start_after_marker_reply in [true, false] {
                let stop = socket_message(&mut server);
                assert_eq!(stop["command"], json!(["stop"]));
                writeln!(
                    server,
                    "{}",
                    json!({"request_id":stop["request_id"], "error":"success"})
                )
                .unwrap();
                if start_after_marker_reply {
                    let marker = credential_barrier_command(&mut server);
                    writeln!(
                        server,
                        "{}",
                        json!({"request_id":marker["request_id"], "error":"success"})
                    )
                    .unwrap();
                    writeln!(server, "{}", json!({"event":"start-file"})).unwrap();
                    writeln!(
                        server,
                        "{}",
                        json!({"event":"client-message", "args":[marker["command"][2]]})
                    )
                    .unwrap();
                } else {
                    writeln!(server, "{}", json!({"event":"start-file"})).unwrap();
                    reply_to_credential_barrier(&mut server);
                }
                server
                    .set_read_timeout(Some(Duration::from_millis(25)))
                    .unwrap();
                assert!(server.read(&mut [0]).is_err());
                server
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                writeln!(server, "{}", json!({"event":"end-file"})).unwrap();
            }
            server
        });
        let mut pending = Vec::new();
        let mut state = PlaybackState::default();
        let (events, _) = async_channel::unbounded();
        replace_playback_credentials(
            &mut client,
            &mut pending,
            &mut current,
            Some(synthetic_credentials("INVENTED_SECOND")),
            plugin_dir,
            &mut state,
            &events,
        )
        .unwrap();
        let installed = &current.as_ref().unwrap().source.message;
        assert!(String::from_utf8_lossy(installed).contains("INVENTED_SECOND"));
        replace_playback_credentials(
            &mut client,
            &mut pending,
            &mut current,
            None,
            plugin_dir,
            &mut state,
            &events,
        )
        .unwrap();
        assert!(current.is_none());
        worker.join().unwrap();
    }

    #[test]
    fn credential_clear_handles_idle_or_already_ended_files() {
        for queued in [
            vec![],
            vec![json!({"event":"start-file"}), json!({"event":"end-file"})],
        ] {
            let (mut client, mut server) = IpcStream::pair().unwrap();
            client
                .set_read_timeout(Some(Duration::from_millis(80)))
                .unwrap();
            let worker = thread::spawn(move || {
                let stop = socket_message(&mut server);
                writeln!(
                    server,
                    "{}",
                    json!({"request_id":stop["request_id"], "error":"success"})
                )
                .unwrap();
                for message in queued {
                    writeln!(server, "{message}").unwrap();
                }
                reply_to_credential_barrier(&mut server);
                server
            });
            let (events, _) = async_channel::unbounded();
            replace_playback_credentials(
                &mut client,
                &mut Vec::new(),
                &mut None,
                None,
                Path::new("/private/plugins"),
                &mut PlaybackState::default(),
                &events,
            )
            .unwrap();
            worker.join().unwrap();
        }
    }

    #[test]
    fn failed_credential_cleanup_does_not_report_success() {
        for failure in ["stop", "marker", "end-file"] {
            let (mut client, mut server) = IpcStream::pair().unwrap();
            client
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            server
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            let plugin_dir = Path::new("/private/synthetic-plugin-root");
            let mut current = Some(signed_in(
                synthetic_credentials("INVENTED_FIRST"),
                plugin_dir,
            ));
            let worker = thread::spawn(move || {
                let stop = socket_message(&mut server);
                assert_eq!(stop["command"], json!(["stop"]));
                if failure == "end-file" {
                    writeln!(server, "{}", json!({"event":"start-file"})).unwrap();
                }
                writeln!(server, "{}", json!({
                    "request_id":stop["request_id"],
                    "error":if failure == "stop" { "INVENTED_PRIVATE_DIAGNOSTIC" } else { "success" },
                })).unwrap();
                if failure == "marker" {
                    let marker = credential_barrier_command(&mut server);
                    writeln!(
                        server,
                        "{}",
                        json!({
                            "request_id":marker["request_id"], "error":"success",
                        })
                    )
                    .unwrap();
                }
                if failure == "end-file" {
                    reply_to_credential_barrier(&mut server);
                }
            });
            let (events, _) = async_channel::unbounded();
            let result = replace_playback_credentials(
                &mut client,
                &mut Vec::new(),
                &mut current,
                None,
                plugin_dir,
                &mut PlaybackState::default(),
                &events,
            );
            assert!(
                !result
                    .unwrap_err()
                    .to_string()
                    .contains("INVENTED_PRIVATE_DIAGNOSTIC")
            );
            // The actor keeps the old account until core teardown on this fatal error.
            assert!(current.is_some());
            worker.join().unwrap();
        }
    }
}
