//! macOS Now Playing: `MPNowPlayingInfoCenter` shows the track in Control
//! Center and the menu bar widget, and `MPRemoteCommandCenter` delivers media
//! keys and their buttons. The system extrapolates the position from the
//! elapsed time and rate published with each update.

use super::{Event, PlaybackStatus, Published, State};
use anyhow::Result;
use block2::RcBlock;
use objc2::{AnyThread, rc::Retained, runtime::AnyObject};
use objc2_app_kit::NSImage;
use objc2_core_foundation::CGSize;
use objc2_foundation::{NSData, NSMutableDictionary, NSNumber, NSString};
use objc2_media_player::{
    MPChangePlaybackPositionCommandEvent, MPMediaItemArtwork, MPMediaItemPropertyAlbumTitle,
    MPMediaItemPropertyArtist, MPMediaItemPropertyArtwork, MPMediaItemPropertyPlaybackDuration,
    MPMediaItemPropertyTitle, MPNowPlayingInfoCenter, MPNowPlayingInfoPropertyElapsedPlaybackTime,
    MPNowPlayingInfoPropertyPlaybackRate, MPNowPlayingPlaybackState, MPRemoteCommand,
    MPRemoteCommandCenter, MPRemoteCommandEvent, MPRemoteCommandHandlerStatus,
};
use std::{
    ptr::NonNull,
    sync::{Arc, Mutex, MutexGuard, PoisonError, mpsc},
    time::{Duration, Instant},
};

/// Control Center draws art at up to 256 points on Retina displays.
const ARTWORK_SIDE: u32 = 512;

fn lock(published: &Mutex<Published>) -> MutexGuard<'_, Published> {
    published.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Serves Now Playing until the app drops its `MediaControls`.
pub(super) fn serve(
    published: Arc<Mutex<Published>>,
    events: async_channel::Sender<Event>,
    updates: mpsc::Receiver<()>,
) -> Result<()> {
    // SAFETY: both are process-wide singletons that Apple documents for use
    // from any thread.
    let center = unsafe { MPNowPlayingInfoCenter::defaultCenter() };
    let commands = unsafe { MPRemoteCommandCenter::sharedCommandCenter() };
    let handle = |command: &MPRemoteCommand,
                  request: fn(&State, &MPRemoteCommandEvent) -> Option<Event>| {
        let published = published.clone();
        let events = events.clone();
        let handler = RcBlock::new(move |event: NonNull<MPRemoteCommandEvent>| {
            // SAFETY: the system passes a valid event for the handler's duration.
            let event = unsafe { event.as_ref() };
            let Some(event) = request(&lock(&published).state, event) else {
                return MPRemoteCommandHandlerStatus::CommandFailed;
            };
            match events.try_send(event) {
                Ok(()) => MPRemoteCommandHandlerStatus::Success,
                Err(_) => MPRemoteCommandHandlerStatus::CommandFailed,
            }
        });
        // SAFETY: the command center keeps the handler alive until removeTarget.
        unsafe { command.addTargetWithHandler(&handler) };
    };
    // SAFETY: the accessors return the center's long-lived commands.
    let (play, pause, toggle, next, previous, position) = unsafe {
        (
            commands.playCommand(),
            commands.pauseCommand(),
            commands.togglePlayPauseCommand(),
            commands.nextTrackCommand(),
            commands.previousTrackCommand(),
            commands.changePlaybackPositionCommand(),
        )
    };
    handle(&play, |state, _| state.can_play.then_some(Event::Play));
    handle(&pause, |state, _| state.can_pause.then_some(Event::Pause));
    handle(&toggle, |state, _| {
        let allowed = if state.playback == PlaybackStatus::Playing {
            state.can_pause
        } else {
            state.can_play
        };
        allowed.then_some(Event::PlayPause)
    });
    handle(&next, |state, _| state.can_go_next.then_some(Event::Next));
    handle(&previous, |state, _| {
        state.can_go_previous.then_some(Event::Previous)
    });
    handle(&position, |state, event| {
        let event = event.downcast_ref::<MPChangePlaybackPositionCommandEvent>()?;
        // SAFETY: a change-position event always carries its target time.
        let seconds = unsafe { event.positionTime() };
        let length = state.metadata.as_ref()?.length;
        let in_range =
            seconds >= 0. && length.is_none_or(|length| super::microseconds(seconds) <= length);
        (state.can_seek && in_range).then_some(Event::SetPosition {
            track_id: state.track_id?,
            seconds,
        })
    });

    let client = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(3))
        .timeout(Duration::from_secs(6))
        .build()
        .ok();
    let mut artwork: Option<(String, Retained<MPMediaItemArtwork>)> = None;
    // Set even when the download failed, so later updates do not retry it.
    let mut tried: Option<String> = None;
    while updates.recv().is_ok() {
        let state = update(&center, &published, artwork.as_ref());
        // SAFETY: toggling availability is documented for any thread.
        unsafe {
            play.setEnabled(state.can_play);
            pause.setEnabled(state.can_pause);
            toggle.setEnabled(state.can_play || state.can_pause);
            next.setEnabled(state.can_go_next);
            previous.setEnabled(state.can_go_previous);
            position.setEnabled(state.can_seek);
        }
        let url = state.metadata.and_then(|track| track.art_url);
        if url == tried {
            continue;
        }
        tried.clone_from(&url);
        artwork = url.zip(client.as_ref()).and_then(|(url, client)| {
            let bytes = crate::artwork::download_square(client, &url, ARTWORK_SIDE).ok()?;
            Some((url, item_artwork(&bytes)?))
        });
        if artwork.is_some() {
            update(&center, &published, artwork.as_ref());
        }
    }
    // SAFETY: removing every target releases the handlers registered above.
    unsafe {
        for command in [&*play, &*pause, &*toggle, &*next, &*previous, &position] {
            command.removeTarget(None);
        }
        center.setNowPlayingInfo(None);
        center.setPlaybackState(MPNowPlayingPlaybackState::Stopped);
    }
    Ok(())
}

/// Publishes the current state, with `artwork` when it belongs to the track.
fn update(
    center: &MPNowPlayingInfoCenter,
    published: &Mutex<Published>,
    artwork: Option<&(String, Retained<MPMediaItemArtwork>)>,
) -> State {
    let (state, elapsed) = {
        let mut published = lock(published);
        published.seeked = None;
        (
            published.state.clone(),
            published.position_at(Instant::now()) as f64 / 1_000_000.,
        )
    };
    let playback = match state.playback {
        PlaybackStatus::Stopped => MPNowPlayingPlaybackState::Stopped,
        PlaybackStatus::Playing => MPNowPlayingPlaybackState::Playing,
        PlaybackStatus::Paused => MPNowPlayingPlaybackState::Paused,
    };
    let info = state.metadata.as_ref().map(|track| {
        let info = NSMutableDictionary::<NSString, AnyObject>::new();
        let rate = if state.advancing && state.playback == PlaybackStatus::Playing {
            f64::from(state.rate)
        } else {
            0.
        };
        // SAFETY: the keys are framework constants, and every value has the
        // type the framework documents for its key.
        unsafe {
            info.insert(MPMediaItemPropertyTitle, &*NSString::from_str(&track.title));
            info.insert(
                MPMediaItemPropertyArtist,
                &*NSString::from_str(&track.artist),
            );
            if let Some(album) = &track.album {
                info.insert(MPMediaItemPropertyAlbumTitle, &*NSString::from_str(album));
            }
            if let Some(length) = track.length {
                let seconds = NSNumber::new_f64(length as f64 / 1_000_000.);
                info.insert(MPMediaItemPropertyPlaybackDuration, &*seconds);
            }
            if let Some((url, artwork)) = artwork
                && track.art_url.as_ref() == Some(url)
            {
                info.insert(MPMediaItemPropertyArtwork, &**artwork);
            }
            let elapsed = NSNumber::new_f64(elapsed);
            info.insert(MPNowPlayingInfoPropertyElapsedPlaybackTime, &*elapsed);
            info.insert(
                MPNowPlayingInfoPropertyPlaybackRate,
                &*NSNumber::new_f64(rate),
            );
        }
        info
    });
    // SAFETY: the info dictionary holds only the documented value types.
    unsafe {
        center.setNowPlayingInfo(info.as_deref().map(|info| &**info));
        center.setPlaybackState(playback);
    }
    state
}

fn item_artwork(bytes: &[u8]) -> Option<Retained<MPMediaItemArtwork>> {
    let image = NSImage::initWithData(NSImage::alloc(), &NSData::with_bytes(bytes))?;
    let size = image.size();
    let handler = RcBlock::new(move |_: CGSize| NonNull::from(&*image));
    // SAFETY: the block owns the image, so the pointer it returns stays valid
    // for as long as the artwork keeps the block.
    Some(unsafe {
        MPMediaItemArtwork::initWithBoundsSize_requestHandler(
            MPMediaItemArtwork::alloc(),
            size,
            &handler,
        )
    })
}
