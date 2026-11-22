//! Which actions a song's menu offers, in music.youtube.com's order.

use crate::catalog::Track;

/// One row of a song menu. The web also lists Download, Report and Stats for
/// nerds, and Remove from playlist for a mix; the app can't do those, so it
/// leaves them out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::app) enum SongAction {
    Episode,
    Chapters,
    Show,
    Mix,
    PlayNext,
    AddToQueue,
    Library,
    Like,
    SaveToPlaylist,
    RemoveFromPlaylist,
    MoveUp,
    MoveDown,
    RemoveFromQueue,
    RemoveFromHistory,
    Album,
    Artist,
    Credits,
    Share,
}

/// Where the menu opened, beyond the song itself.
#[derive(Clone, Copy, Debug, Default)]
pub(in crate::app) struct MenuPlace {
    pub signed_in: bool,
    /// A row of the page, such as an album track or a search result, rather
    /// than the player bar, Up next or a search suggestion.
    pub page_row: bool,
    /// The song is this entry of the queue: Up next, or the player bar.
    pub queued: bool,
    /// A row of a playlist the account can edit, and which ways it can move.
    pub editable: bool,
    pub move_up: bool,
    pub move_down: bool,
}

pub(in crate::app) fn song_actions(track: &Track, place: MenuPlace) -> Vec<SongAction> {
    use SongAction::*;
    let details = &track.details;
    let mut actions = Vec::new();
    match &details.episode {
        Some(episode) => {
            actions.push(Episode);
            if !episode.chapters.is_empty() {
                actions.push(Chapters);
            }
            if episode.show_id.is_some() {
                actions.push(Show);
            }
        }
        None => actions.push(Mix),
    }
    actions.extend([PlayNext, AddToQueue]);
    // Album rows offer Save to library where search rows and the player offer
    // Add to liked songs. A guest's Save to library asks them to sign in, as
    // Save to playlist does.
    let library = if place.signed_in {
        details.add_library.is_some() || details.remove_library.is_some()
    } else {
        place.page_row && details.in_library.is_some()
    };
    if library {
        actions.push(Library);
    }
    if !place.page_row || details.in_library.is_none() {
        actions.push(Like);
    }
    actions.push(SaveToPlaylist);
    if place.signed_in && place.page_row && place.editable && details.set_video_id.is_some() {
        actions.push(RemoveFromPlaylist);
        if place.move_up {
            actions.push(MoveUp);
        }
        if place.move_down {
            actions.push(MoveDown);
        }
    }
    if place.queued {
        actions.push(RemoveFromQueue);
    }
    if place.signed_in && details.remove_history.is_some() {
        actions.push(RemoveFromHistory);
    }
    if details.album_id.is_some() {
        actions.push(Album);
    }
    if details.artist_id.is_some() {
        actions.push(Artist);
    }
    if track.credits_id().is_some() {
        actions.push(Credits);
    }
    actions.push(Share);
    actions
}

#[cfg(test)]
mod tests {
    use super::{MenuPlace, SongAction::*, song_actions};
    use crate::catalog::Track;

    fn song(id: &str) -> Track {
        Track {
            id: id.into(),
            title: String::new(),
            artist: String::new(),
            album: None,
            duration: None,
            thumbnail: None,
            details: Default::default(),
        }
    }

    /// Each expectation is the menu the signed-out web showed for the same
    /// kind of row, minus Download, Report and Stats for nerds.
    #[test]
    fn guest_menus_follow_the_webs_order() {
        let mut levitating = song("TUVcZfQe-Kw");
        levitating.details.artist_id = Some("UCartist".into());
        // The player bar and Up next.
        assert_eq!(
            song_actions(
                &levitating,
                MenuPlace {
                    queued: true,
                    ..Default::default()
                }
            ),
            [
                Mix,
                PlayNext,
                AddToQueue,
                Like,
                SaveToPlaylist,
                RemoveFromQueue,
                Artist,
                Credits,
                Share
            ]
        );
        // A Future Nostalgia track on its album page offers its library toggle instead of Like.
        // Played from there, the bar still offers Like.
        let mut track = levitating.clone();
        track.details.in_library = Some(false);
        assert_eq!(
            song_actions(
                &track,
                MenuPlace {
                    queued: true,
                    ..Default::default()
                }
            ),
            [
                Mix,
                PlayNext,
                AddToQueue,
                Like,
                SaveToPlaylist,
                RemoveFromQueue,
                Artist,
                Credits,
                Share
            ]
        );
        assert_eq!(
            song_actions(
                &track,
                MenuPlace {
                    page_row: true,
                    ..Default::default()
                }
            ),
            [
                Mix,
                PlayNext,
                AddToQueue,
                Library,
                SaveToPlaylist,
                Artist,
                Credits,
                Share
            ]
        );
        // STAY in search results.
        let mut stay = song("kTJczUoc26U");
        stay.details.album_id = Some("MPREalbum".into());
        stay.details.artist_id = Some("UCartist".into());
        assert_eq!(
            song_actions(
                &stay,
                MenuPlace {
                    page_row: true,
                    ..Default::default()
                }
            ),
            [
                Mix,
                PlayNext,
                AddToQueue,
                Like,
                SaveToPlaylist,
                Album,
                Artist,
                Credits,
                Share
            ]
        );
    }

    #[test]
    fn owned_playlist_rows_keep_their_edits_before_navigation() {
        let mut track = song("TUVcZfQe-Kw");
        track.details.set_video_id = Some("set".into());
        track.details.add_library = Some("add".into());
        track.details.in_library = Some(false);
        track.details.album_id = Some("MPREalbum".into());
        let place = MenuPlace {
            signed_in: true,
            page_row: true,
            editable: true,
            move_up: true,
            move_down: false,
            queued: false,
        };
        assert_eq!(
            song_actions(&track, place),
            [
                Mix,
                PlayNext,
                AddToQueue,
                Library,
                SaveToPlaylist,
                RemoveFromPlaylist,
                MoveUp,
                Album,
                Credits,
                Share
            ]
        );
    }
}
