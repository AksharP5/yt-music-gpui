use super::{
    CatalogRequests, CatalogTarget, DetailList, Playback, Query, QueueUpdate,
    acknowledge_song_change, adopt_watch_queue, apply_track_details, playlist_move_targets,
    visible_cards,
};
use crate::{
    account::Mutation,
    catalog::{BrowseLink, Item, Rating, Track},
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
fn replacing_the_list_tail_preserves_scroll_and_the_empty_list_origin() {
    let mut details = DetailList::new();
    details.reset_rows(100);
    details.set_inline_header(true);
    details.state.scroll_to(ListOffset {
        item_ix: 31,
        offset_in_item: px(8.),
    });
    details.replace_tail(100, 100);
    assert_eq!(details.state.item_count(), 201);
    assert_eq!(details.state.logical_scroll_top().item_ix, 31);
    assert_eq!(details.state.logical_scroll_top().offset_in_item, px(8.));
    details.replace_tail(180, 20);
    assert_eq!(details.state.item_count(), 201);
    assert_eq!(details.state.logical_scroll_top().item_ix, 31);

    let empty = DetailList::new();
    empty.state.scroll_to(ListOffset {
        item_ix: 0,
        offset_in_item: px(0.),
    });
    empty.replace_tail(0, 5);
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
        shuffle: None,
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
fn page_refresh_preserves_sidebar_requests() {
    let mut requests = CatalogRequests::new();
    requests.request(Query::Home, 0, 0, 0, None).unwrap();
    let old_page = requests.generations[CatalogTarget::Page as usize]
        .load(std::sync::atomic::Ordering::Relaxed);
    requests
        .request(Query::SidebarPlaylists, 0, 0, 0, None)
        .unwrap();
    requests.request(Query::Explore, 0, 0, 0, None).unwrap();
    let pending = std::iter::from_fn(|| requests.pending.try_recv().ok()).collect::<Vec<_>>();
    assert_eq!(pending.len(), 2);
    for target in [CatalogTarget::Page, CatalogTarget::Sidebar] {
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
fn page_refresh_keeps_the_pending_sidebar_and_rejects_older_pages() {
    let mut requests = CatalogRequests::new();
    requests.request(Query::Home, 0, 0, 0, None).unwrap();
    let old_page = requests.pending.try_recv().unwrap();
    requests
        .request(Query::SidebarPlaylists, 0, 0, 0, None)
        .unwrap();
    requests.request(Query::Explore, 1, 0, 0, None).unwrap();
    let sidebar = requests.pending.try_recv().unwrap();
    let page = requests.pending.try_recv().unwrap();
    assert!(matches!(sidebar.query, Query::SidebarPlaylists));
    assert!(matches!(page.query, Query::Explore));
    assert!(requests.pending.try_recv().is_err());
    assert!(!requests.finish(CatalogTarget::Page, old_page.id));
    assert!(requests.is_loading(CatalogTarget::Page));
    assert!(requests.finish(CatalogTarget::Page, page.id));
    assert!(requests.is_loading(CatalogTarget::Sidebar));
    assert!(requests.finish(CatalogTarget::Sidebar, sidebar.id));
    assert!(!requests.is_loading(CatalogTarget::Page));
    assert!(!requests.is_loading(CatalogTarget::Sidebar));
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
    let mut queue = Queue::default();
    queue.replace(tracks, 0);
    let mut ratings = HashMap::new();
    let like = Mutation::RateSong {
        track: song.clone(),
        rating: Rating::Like,
    };
    acknowledge_song_change(&mut queue, &mut ratings, &like);
    let snapshot = queue.tracks.clone();
    acknowledge_song_change(&mut queue, &mut ratings, &like);
    assert!(std::sync::Arc::ptr_eq(&queue.tracks, &snapshot));
    acknowledge_song_change(
        &mut queue,
        &mut ratings,
        &Mutation::SongLibrary {
            track: song.clone(),
            save: true,
        },
    );
    apply_track_details(queue.tracks_mut(), &mut ratings, &song, false);
    assert_eq!(ratings[&song.id], Rating::Like);
    assert!(
        queue
            .tracks
            .iter()
            .all(|track| track.details.rating == Some(Rating::Like)
                && track.details.in_library == Some(true))
    );

    apply_track_details(queue.tracks_mut(), &mut ratings, &song, true);
    assert_eq!(ratings[&song.id], Rating::Indifferent);
    assert!(
        queue
            .tracks
            .iter()
            .all(|track| track.details.in_library == Some(false))
    );
    assert_eq!(
        queue.tracks[0].details.set_video_id.as_deref(),
        Some("first")
    );
    assert_eq!(
        queue.tracks[1].details.set_video_id.as_deref(),
        Some("second")
    );

    song.details.rating = None;
    song.details.in_library = None;
    apply_track_details(queue.tracks_mut(), &mut ratings, &song, true);
    assert!(!ratings.contains_key(&song.id));
    assert!(
        queue
            .tracks
            .iter()
            .all(|track| track.details.rating.is_none() && track.details.in_library.is_none())
    );
}

#[test]
fn acknowledged_library_changes_update_visible_songs_without_copying_the_queue() {
    let mut song: Track = serde_json::from_value(serde_json::json!({
        "id":"abcdefghijk", "title":"Visible song", "artist":"Artist"
    }))
    .unwrap();
    song.details.in_library = Some(false);
    song.details.add_library = Some("synthetic-add".into());
    song.details.remove_library = Some("synthetic-remove".into());

    let mut page = super::sync::tests::page(1, false);
    let mut queue = Queue::default();
    queue.replace(page.tracks(), 0);
    page.shelves[0].items[0] = Item::Track(song.clone());
    let mut back = page.clone();
    let snapshot = queue.tracks.clone();
    let mut ratings = HashMap::new();
    for save in [true, false] {
        let mutation = Mutation::SongLibrary {
            track: song.clone(),
            save,
        };
        acknowledge_song_change(&mut queue, &mut ratings, &mutation);
        assert!(std::sync::Arc::ptr_eq(&queue.tracks, &snapshot));
        for visible in [&mut page, &mut back] {
            super::sync::patch_page(visible, &super::Route::Home, &mutation);
            let Item::Track(track) = &visible.shelves[0].items[0] else {
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
fn playlist_move_targets_use_occurrences_and_respect_the_loaded_boundary() {
    let track = |occurrence: Option<&str>| {
        let mut track: Track = serde_json::from_value(serde_json::json!({
            "id": "abcdefghijk", "title": "Repeated song", "artist": "Artist"
        }))
        .unwrap();
        track.details.set_video_id = occurrence.map(str::to_owned);
        Item::Track(track)
    };
    let more = || Item::Navigation(BrowseLink::new("VLrelated", "Related"));
    let mut rows = [
        track(Some("first")),
        more(),
        track(Some("second")),
        more(),
        track(Some("third")),
        more(),
    ];
    assert_eq!(
        playlist_move_targets(&rows, 0, true),
        [None, Some(Some("third"))]
    );
    assert_eq!(
        playlist_move_targets(&rows, 2, true),
        [Some(Some("first")), None]
    );
    assert_eq!(
        playlist_move_targets(&rows, 2, false),
        [Some(Some("first")), Some(None)]
    );
    assert_eq!(
        playlist_move_targets(&rows, 4, false),
        [Some(Some("second")), None]
    );
    assert_eq!(playlist_move_targets(&rows, 5, false), [None, None]);

    rows[4] = track(None);
    assert_eq!(playlist_move_targets(&rows, 0, false), [None, None]);
}

#[test]
fn a_load_retries_once_and_pause_changes_do_not_reset_its_budget() {
    for paused in [false, true] {
        let mut playback = Playback::Loading {
            paused,
            retried: false,
            full_lookup: false,
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
        full_lookup: false,
    };
    assert_eq!(
        loading.toggled(),
        Playback::Loading {
            paused: true,
            retried: true,
            full_lookup: false,
        }
    );
    assert_eq!(loading.toggled().toggled(), loading);
}

#[test]
fn a_failed_stream_keeps_its_song_on_the_full_lookup_after_quality_changes() {
    let mut loading = Playback::Loading {
        paused: false,
        retried: false,
        full_lookup: false,
    };
    assert!(loading.retry());
    assert!(!loading.retry());
    // A quality change resets only the retry budget, as the setters do.
    if let Playback::Loading { retried, .. } = &mut loading {
        *retried = false;
    }
    assert!(loading.retry());
    assert!(matches!(
        loading.with_pause(true),
        Playback::Loading {
            paused: true,
            full_lookup: true,
            ..
        }
    ));
}

#[test]
fn displayed_durations_round_partial_seconds_up_like_the_web() {
    for (seconds, expected) in [
        // Levitating's video measures 230.121 s; the web shows 3:51.
        (230.121, "3:51"),
        (337.8, "5:38"),
        (59.5, "1:00"),
        (3599.5, "1:00:00"),
        (266., "4:26"),
        (-1., "0:00"),
    ] {
        assert_eq!(super::clock(seconds), expected);
    }
}

#[test]
fn the_seek_preview_counts_whole_seconds_under_the_pointer() {
    // Hover times music.youtube.com showed across Levitating's 230.121 s video.
    for (at, expected) in [(0.0874, "0:20"), (0.5874, "2:15"), (0.9824, "3:46")] {
        assert_eq!(super::seek_label(at, 230.121), expected);
    }
}

#[test]
fn the_restored_position_applies_once_and_only_to_its_song() {
    let track = |id: &str| Track {
        id: id.into(),
        title: id.into(),
        artist: String::new(),
        album: None,
        duration: Some(200),
        thumbnail: None,
        details: Default::default(),
    };
    let restored = || crate::library::LastPosition {
        track_id: "stopped".into(),
        seconds: 42.,
    };
    let mut listening = super::playback::listening::ListeningState::default();
    listening.restored = Some(restored());
    assert_eq!(listening.take_restored(Some(&track("other"))), None);
    assert_eq!(listening.take_restored(Some(&track("stopped"))), None);
    listening.restored = Some(restored());
    assert_eq!(listening.take_restored(Some(&track("stopped"))), Some(42.));
    assert_eq!(listening.take_restored(Some(&track("stopped"))), None);
}
