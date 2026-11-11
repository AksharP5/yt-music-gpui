use super::{
    CatalogRequests, CatalogTarget, DetailList, Playback, Query, QueueUpdate,
    acknowledge_song_change, adopt_watch_queue, apply_track_details, playlist_move_down,
    visible_cards,
};
use crate::{
    account::Mutation,
    catalog::{Rating, Track},
    library::Queue,
    watch::Watch,
};
use gpui::{ListOffset, px};
use std::collections::{HashMap, VecDeque};

#[test]
fn regenerated_autoplay_skips_cached_and_pending_recommendations() {
    let track = |id: &str| Track {
        id: id.into(),
        title: id.into(),
        artist: String::new(),
        album: None,
        duration: None,
        thumbnail: None,
        details: Default::default(),
    };
    let mut queue = Queue::default();
    queue.replace(vec![track("seed"), track("played-autoplay")], 1);
    let mut recommendations = VecDeque::from([track("pending")]);
    super::playback::queue::append_recommendations(
        &queue,
        &mut recommendations,
        [
            track("played-autoplay"),
            track("seed"),
            track("fresh"),
            track("pending"),
            track("fresh"),
            track("next"),
        ],
    );
    assert_eq!(
        recommendations
            .iter()
            .map(|track| track.id.as_str())
            .collect::<Vec<_>>(),
        ["pending", "fresh", "next"]
    );
    assert_eq!(queue.index, Some(1));
}

#[test]
fn errors_show_a_short_summary_with_the_diagnostic_left_intact() {
    let detail = "Could not load Home: error sending request for url (https://music.youtube.com/youtubei/v1/browse): client error (Connect): dns error: failed to lookup address information";
    assert_eq!(
        super::pages::error_summary(detail),
        "Could not connect to YouTube Music. Check your connection and try again."
    );
    assert_eq!(
        super::pages::error_summary("Could not save your music: disk full"),
        "Could not save your music"
    );
    assert!(
        super::pages::error_summary(&"long diagnostic ".repeat(100))
            .chars()
            .count()
            <= 161
    );
}

#[test]
fn collection_layout_preserves_scroll_when_the_header_moves_between_columns() {
    let mut details = DetailList::new();
    details.reset_rows(100);
    details.set_inline_header(true);
    assert_eq!(details.state.item_count(), 101);
    assert_eq!(details.state.logical_scroll_top().item_ix, 0);
    details.state.scroll_to(ListOffset {
        item_ix: 31,
        offset_in_item: px(8.),
    });
    details.set_inline_header(false);
    assert_eq!(details.state.item_count(), 100);
    assert_eq!(details.state.logical_scroll_top().item_ix, 30);
    assert_eq!(details.state.logical_scroll_top().offset_in_item, px(8.));
    details.set_inline_header(true);
    assert_eq!(details.state.logical_scroll_top().item_ix, 31);
    details.reset_rows(200);
    assert_eq!(details.state.item_count(), 201);
    assert_eq!(details.state.logical_scroll_top().item_ix, 31);
}

#[test]
fn appended_rows_preserve_scroll_and_the_empty_list_origin() {
    let mut details = DetailList::new();
    details.reset_rows(100);
    details.set_inline_header(true);
    details.state.scroll_to(ListOffset {
        item_ix: 31,
        offset_in_item: px(8.),
    });
    details.append_rows(100);
    assert_eq!(details.state.item_count(), 201);
    assert_eq!(details.state.logical_scroll_top().item_ix, 31);
    assert_eq!(details.state.logical_scroll_top().offset_in_item, px(8.));
    details.append_rows(0);
    assert_eq!(details.state.item_count(), 201);
    assert_eq!(details.state.logical_scroll_top().item_ix, 31);

    let empty = DetailList::new();
    empty.state.scroll_to(ListOffset {
        item_ix: 0,
        offset_in_item: px(0.),
    });
    empty.append_rows(5);
    assert_eq!(empty.state.item_count(), 5);
    assert_eq!(empty.state.logical_scroll_top().item_ix, 0);
    assert_eq!(empty.state.logical_scroll_top().offset_in_item, px(0.));
}

#[test]
fn issued_queue_adoption_preserves_manual_edits_and_the_selected_occurrence() {
    let track = |id: &str| Track {
        id: id.into(),
        title: id.into(),
        artist: "Artist".into(),
        album: None,
        duration: None,
        thumbnail: None,
        details: Default::default(),
    };
    let seed = track("abcdefghijk");
    let next = track("lmnopqrstuv");
    let watch = Watch {
        source_target: None,
        current: Some(seed.clone()),
        current_index: Some(1),
        queue: vec![next.clone(), seed.clone(), next.clone()],
        source: None,
        lyrics: None,
        related: None,
        comments: None,
        alternatives: None,
        continuation: None,
    };
    let mut queue = Queue::default();
    queue.replace(vec![seed.clone()], 0);
    assert!(adopt_watch_queue(&mut queue, &watch, QueueUpdate::Adopt).unwrap());
    assert_eq!(queue.index, Some(1));
    assert_eq!(queue.current().unwrap().id, seed.id);
    assert_eq!(
        queue
            .tracks
            .iter()
            .map(|track| track.id.as_str())
            .collect::<Vec<_>>(),
        [next.id.as_str(), seed.id.as_str(), next.id.as_str()]
    );

    queue.replace(vec![seed.clone(), track("manual-edit")], 0);
    assert!(!adopt_watch_queue(&mut queue, &watch, QueueUpdate::Adopt).unwrap());
    assert_eq!(queue.tracks[1].id, "manual-edit");
    assert_eq!(queue.index, Some(0));
    assert!(!adopt_watch_queue(&mut queue, &watch, QueueUpdate::Preserve).unwrap());
    assert_eq!(queue.tracks[1].id, "manual-edit");

    queue.replace(vec![track("new-selection")], 0);
    assert!(!adopt_watch_queue(&mut queue, &watch, QueueUpdate::Adopt).unwrap());
    assert_eq!(queue.current().unwrap().id, "new-selection");
    assert!(adopt_watch_queue(&mut queue, &watch, QueueUpdate::Replace).unwrap());
    assert_eq!(queue.current().unwrap().id, seed.id);
}

#[test]
fn page_refresh_preserves_sidebar_and_picker_requests() {
    let mut requests = CatalogRequests::new();
    requests.request(Query::Home, 0, 0, 0, None).unwrap();
    let old_page = requests.generations[CatalogTarget::Page as usize]
        .load(std::sync::atomic::Ordering::Relaxed);
    requests
        .request(Query::PlaylistChoices, 0, 0, 0, None)
        .unwrap();
    requests
        .request(Query::SidebarPlaylists, 0, 0, 0, None)
        .unwrap();
    requests.request(Query::Explore, 0, 0, 0, None).unwrap();
    let pending = std::iter::from_fn(|| requests.pending.try_recv().ok()).collect::<Vec<_>>();
    assert_eq!(pending.len(), 3);
    for target in CatalogTarget::ALL {
        assert_eq!(
            pending
                .iter()
                .filter(|job| job.query.target() == target)
                .count(),
            1
        );
    }
    assert!(
        pending
            .iter()
            .any(|job| matches!(job.query, Query::Explore))
    );
    assert!(!requests.finish(CatalogTarget::Page, old_page));
}

#[test]
fn carousel_only_renders_nearby_cards_and_clamps_after_a_shorter_refresh() {
    assert_eq!(visible_cards(100, 0., 612., 204.), 0..4);
    assert_eq!(visible_cards(100, 2040., 612., 204.), 10..14);
    assert_eq!(visible_cards(100, 100000., 612., 204.), 96..100);
    assert_eq!(visible_cards(2, 100000., 612., 204.), 0..2);
    assert_eq!(visible_cards(0, 0., 612., 204.), 0..0);
}

#[test]
fn page_refresh_keeps_the_pending_playlist_picker_and_rejects_older_pages() {
    let mut requests = CatalogRequests::new();
    requests.request(Query::Home, 0, 0, 0, None).unwrap();
    let old_page = requests.pending.try_recv().unwrap();
    requests
        .request(Query::PlaylistChoices, 0, 0, 0, None)
        .unwrap();
    requests.request(Query::Explore, 1, 0, 0, None).unwrap();
    let picker = requests.pending.try_recv().unwrap();
    let page = requests.pending.try_recv().unwrap();
    assert!(matches!(picker.query, Query::PlaylistChoices));
    assert!(matches!(page.query, Query::Explore));
    assert!(requests.pending.try_recv().is_err());
    assert!(!requests.finish(CatalogTarget::Page, old_page.id));
    assert!(requests.is_loading(CatalogTarget::Page));
    assert!(requests.finish(CatalogTarget::Page, page.id));
    assert!(requests.is_loading(CatalogTarget::PlaylistPicker));
    assert!(requests.finish(CatalogTarget::PlaylistPicker, picker.id));
    assert!(!requests.is_loading(CatalogTarget::Page));
    assert!(!requests.is_loading(CatalogTarget::PlaylistPicker));
}

#[test]
fn opening_or_closing_a_picker_does_not_cancel_navigation() {
    let mut requests = CatalogRequests::new();
    requests.request(Query::Home, 0, 0, 0, None).unwrap();
    requests
        .request(Query::PlaylistChoices, 0, 0, 0, None)
        .unwrap();
    requests
        .request(Query::PlaylistChoices, 0, 0, 0, None)
        .unwrap();
    let page = requests.pending.try_recv().unwrap();
    let picker = requests.pending.try_recv().unwrap();
    assert!(matches!(page.query, Query::Home));
    assert!(matches!(picker.query, Query::PlaylistChoices));
    assert!(requests.pending.try_recv().is_err());
    requests.cancel(CatalogTarget::PlaylistPicker);
    assert!(!requests.finish(CatalogTarget::PlaylistPicker, picker.id));
    assert!(requests.is_loading(CatalogTarget::Page));
    assert!(requests.finish(CatalogTarget::Page, page.id));
}

#[test]
fn pause_and_resume_keep_the_clock_frozen_until_buffering_finishes() {
    let playing = Playback::Loaded {
        paused: false,
        buffering: false,
    };
    let buffering = playing.with_buffering(true);
    assert!(!buffering.is_advancing());
    let paused = buffering.with_pause(true);
    assert!(paused.is_paused());
    assert!(!paused.is_advancing());
    let resumed = paused.with_pause(false);
    assert!(!resumed.is_paused());
    assert!(!resumed.is_advancing());
    assert!(resumed.with_buffering(false).is_advancing());
    assert!(!paused.with_buffering(false).is_advancing());
}

#[test]
fn acknowledged_song_state_survives_old_reads_and_fresh_reads_can_remove_it() {
    let mut song: Track = serde_json::from_value(serde_json::json!({
        "id": "abcdefghijk", "title": "Song", "artist": "Artist"
    }))
    .unwrap();
    song.details.rating = Some(Rating::Indifferent);
    song.details.in_library = Some(false);
    song.details.add_library = Some("synthetic-add".into());
    song.details.remove_library = Some("synthetic-remove".into());
    let mut tracks = vec![song.clone(), song.clone()];
    tracks[0].details.set_video_id = Some("first".into());
    tracks[1].details.set_video_id = Some("second".into());
    let mut ratings = HashMap::new();
    acknowledge_song_change(
        &mut tracks,
        &mut ratings,
        &Mutation::RateSong {
            track: song.clone(),
            rating: Rating::Like,
        },
    );
    acknowledge_song_change(
        &mut tracks,
        &mut ratings,
        &Mutation::SongLibrary {
            track: song.clone(),
            save: true,
        },
    );
    apply_track_details(&mut tracks, &mut ratings, &song, false);
    assert_eq!(ratings[&song.id], Rating::Like);
    assert!(
        tracks
            .iter()
            .all(|track| track.details.rating == Some(Rating::Like)
                && track.details.in_library == Some(true))
    );

    apply_track_details(&mut tracks, &mut ratings, &song, true);
    assert_eq!(ratings[&song.id], Rating::Indifferent);
    assert!(
        tracks
            .iter()
            .all(|track| track.details.in_library == Some(false))
    );
    assert_eq!(tracks[0].details.set_video_id.as_deref(), Some("first"));
    assert_eq!(tracks[1].details.set_video_id.as_deref(), Some("second"));

    song.details.rating = None;
    song.details.in_library = None;
    apply_track_details(&mut tracks, &mut ratings, &song, true);
    assert!(!ratings.contains_key(&song.id));
    assert!(
        tracks
            .iter()
            .all(|track| track.details.rating.is_none() && track.details.in_library.is_none())
    );
}

#[test]
fn acknowledged_library_changes_update_both_visible_copies_without_a_queue_match() {
    let mut song: Track = serde_json::from_value(serde_json::json!({
        "id":"abcdefghijk", "title":"Visible song", "artist":"Artist"
    }))
    .unwrap();
    song.details.in_library = Some(false);
    song.details.add_library = Some("synthetic-add".into());
    song.details.remove_library = Some("synthetic-remove".into());
    let mut page = vec![crate::catalog::Item::Track(song.clone())];
    let mut rows = page.clone();
    let mut ratings = HashMap::new();
    for save in [true, false] {
        acknowledge_song_change(
            page.iter_mut()
                .chain(&mut rows)
                .filter_map(|item| match item {
                    crate::catalog::Item::Track(track) => Some(track),
                    _ => None,
                }),
            &mut ratings,
            &Mutation::SongLibrary {
                track: song.clone(),
                save,
            },
        );
        for item in page.iter().chain(&rows) {
            let crate::catalog::Item::Track(track) = item else {
                panic!("expected song")
            };
            assert_eq!(track.details.in_library, Some(save));
            assert_eq!(track.details.add_library.as_deref(), Some("synthetic-add"));
            assert_eq!(
                track.details.remove_library.as_deref(),
                Some("synthetic-remove")
            );
        }
    }
}

#[test]
fn moving_down_requires_the_successor_or_a_confirmed_playlist_end() {
    let mut tracks: Vec<Track> = ["first", "second", "third"]
        .into_iter()
        .map(|occurrence| {
            let mut track: Track = serde_json::from_value(serde_json::json!({
                "id": "abcdefghijk", "title": "Repeated song", "artist": "Artist"
            }))
            .unwrap();
            track.details.set_video_id = Some(occurrence.into());
            track
        })
        .collect();
    assert_eq!(
        playlist_move_down(&tracks, 0, true),
        Some(Some("third".into()))
    );
    assert_eq!(playlist_move_down(&tracks, 1, true), None);
    assert_eq!(playlist_move_down(&tracks, 1, false), Some(None));
    assert_eq!(playlist_move_down(&tracks, 2, false), None);
    tracks[2].details.set_video_id = None;
    assert_eq!(playlist_move_down(&tracks, 0, false), None);
}

#[test]
fn a_load_retries_once_and_pause_changes_do_not_reset_its_budget() {
    for paused in [false, true] {
        let mut playback = Playback::Loading {
            paused,
            retried: false,
        };
        assert!(playback.retry());
        assert_eq!(playback.is_paused(), paused);
        playback = playback.with_pause(!paused);
        assert_eq!(playback.is_paused(), !paused);
        assert!(!playback.retry());
    }
    for mut playback in [
        Playback::Stopped,
        Playback::Loaded {
            paused: false,
            buffering: false,
        },
        Playback::Loaded {
            paused: true,
            buffering: false,
        },
        Playback::Loaded {
            paused: false,
            buffering: true,
        },
    ] {
        assert!(!playback.retry());
    }
}

#[test]
fn play_starts_a_stopped_wait_and_loading_toggles_preserve_the_retry_budget() {
    let playing = Playback::Loaded {
        paused: false,
        buffering: false,
    };
    let paused = Playback::Loaded {
        paused: true,
        buffering: false,
    };
    assert_eq!(Playback::Stopped.toggled(), playing);
    assert_eq!(paused.toggled(), playing);
    assert_eq!(playing.toggled(), paused);
    let loading = Playback::Loading {
        paused: false,
        retried: true,
    };
    assert_eq!(
        loading.toggled(),
        Playback::Loading {
            paused: true,
            retried: true,
        }
    );
    assert_eq!(loading.toggled().toggled(), loading);
}

#[test]
fn displayed_durations_round_to_the_nearest_second() {
    for (seconds, expected) in [
        (337.8, "5:38"),
        (59.5, "1:00"),
        (3599.5, "1:00:00"),
        (266., "4:26"),
        (-1., "0:00"),
    ] {
        assert_eq!(super::clock(seconds), expected);
    }
}
