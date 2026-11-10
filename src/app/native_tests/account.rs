use crate::app::{
    AccountOperation, BrowserConnection, CatalogRequests, CatalogTarget, Drag, Music, Overlay,
    Playback, PlaybackQuery, PlaybackResult, QueueUpdate, Route, Startup,
};
use crate::{
    account::{self, Connection, Mutation},
    assets::Assets,
    auth::{BrowserProfile, Operation, Session},
    catalog::{Account, BrowseLink, Rating, Track},
    input,
    library::{LibraryWriter, Repeat, Store},
    mpris,
    player::{Player, PlayerOptions},
    track_state,
    watch::{MediaPair, Watch, WatchMode},
};
use anyhow::{Result, ensure};
use gpui::{
    AppContext, Bounds, Context, TitlebarOptions, Window, WindowBounds, WindowKind, WindowOptions,
    point, px, size,
};
use serde_json::json;
use std::borrow::Cow;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

pub(super) fn session(value: &str) -> Arc<Session> {
    let fields = json!({
        "cookie":format!("SAPISID={value}"),
        "playback_cookie":"# Netscape HTTP Cookie File\n",
        "authuser":"0", "delegated_session_id":null,
        "data_sync_id":"synthetic-user||", "client_version":"test"
    });
    Arc::new(serde_json::from_value(fields).unwrap())
}

pub(super) fn connection(session: Arc<Session>) -> Connection {
    Connection {
        session,
        account: Account {
            name: "Synthetic account".into(),
            handle: String::new(),
            thumbnail: None,
        },
    }
}

fn track(id: &str) -> Track {
    serde_json::from_value(json!({"id":id,"title":id,"artist":"Synthetic"})).unwrap()
}

fn prepare(view: &mut Music, current: Arc<Session>) {
    *view.session.write().unwrap() = Some(current.clone());
    view.account = Some(connection(current));
    view.account_operation = None;
    view.next_session_refresh = None;
    view.route = Route::Browse(BrowseLink::new("synthetic-route", "Synthetic playlist"));
    view.library
        .queue
        .replace(vec![track("invalid-first"), track("invalid-second")], 0);
    view.active_id = Some(900);
    view.playback = Playback::Loaded {
        paused: true,
        buffering: false,
    };
    view.position = 17.;
    view.library.video = false;
    view.player_open = false;
    view.media_pair = None;
    view.error = None;
}

fn refreshing(
    view: &mut Music,
    commands: &async_channel::Receiver<account::Command>,
    cx: &mut Context<Music>,
) -> Operation {
    assert!(view.refresh_session(false, cx));
    match commands.try_recv().unwrap() {
        account::Command::Refresh {
            operation,
            expected,
        } => {
            assert!(Arc::ptr_eq(
                &expected,
                &view.account.as_ref().unwrap().session
            ));
            operation
        }
        _ => panic!("Recovery must only request a current-session refresh"),
    }
}

fn renew(
    view: &mut Music,
    replacement: Arc<Session>,
    commands: &async_channel::Receiver<account::Command>,
    window: &mut Window,
    cx: &mut Context<Music>,
) {
    let old = view.account.as_ref().unwrap().session.clone();
    let operation = refreshing(view, commands, cx);
    view.account_event(
        account::Event::Connected {
            operation,
            result: Ok(Some(connection(replacement))),
            save_session: true,
        },
        window,
        cx,
    );
    assert!(Arc::ptr_eq(&view.account.as_ref().unwrap().session, &old));
    let shared = view.session.read().unwrap();
    assert!(Arc::ptr_eq(shared.as_ref().unwrap(), &old));
    assert!(matches!(
        view.account_operation,
        Some(AccountOperation::Activating { .. })
    ));
    assert!(
        commands.is_empty(),
        "Persist must wait for player acknowledgment"
    );
}

#[test]
#[ignore = "requires desktop, private D-Bus and isolated XDG runtime/data; set YT_MUSIC_NATIVE_TESTS=1"]
fn native_recovery_preserves_intent_and_revokes_cancelled_activation() -> Result<()> {
    super::isolate()?;
    let data = std::path::PathBuf::from(std::env::var_os("XDG_DATA_HOME").unwrap());
    let passed = Arc::new(AtomicBool::new(false));
    let result = passed.clone();
    gpui_platform::application().with_assets(Assets).run(move |cx| {
        cx.text_system().add_fonts(vec![Cow::Borrowed(include_bytes!("../../../assets/fonts/Roboto.ttf").as_slice())]).unwrap();
        input::init(cx);
        let bounds = Bounds::new(point(px(40.), px(40.)), size(px(850.), px(560.)));
        let (sender, commands) = async_channel::bounded(64);
        let window = cx.open_window(WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)), kind: WindowKind::PopUp,
            titlebar: Some(TitlebarOptions { title: Some("Music account recovery test".into()), ..Default::default() }),
            focus: false, ..Default::default()
        }, move |window, cx| cx.new(|cx| {
            let mut view = Music::new(Startup::Guest, window, cx);
            view.account_commands.close();
            view.account_worker.take().unwrap().finish().unwrap();
            view.account_commands = sender;
            for target in CatalogTarget::ALL { view.catalog_requests.cancel(target); }
            view.catalog_requests = CatalogRequests::new();
            for generation in view.catalog_requests.generations.iter() { generation.store(1000, Ordering::Relaxed); }
            view.playback_jobs.close();
            let (jobs, pending) = async_channel::bounded(1);
            view.playback_jobs = jobs;
            view.pending_playback_jobs = pending;
            view.track_state = track_state::Worker::idle();
            view.store = None;
            view.mpris = None;
            view.video = None;
            view.player = Some(Player::new(PlayerOptions { audio_output: Some("null".into()), volume: 0., ..Default::default() }).unwrap());
            view.library.volume = 0.;
            prepare(&mut view, session("first"));
            view
        })).unwrap();
        cx.spawn(async move |cx| {
            window.update(cx, |view, _, cx| {
                let path = data.join("yt-music-gpui/library.json");
                assert!(!path.exists());
                let (store, _) = Store::load().unwrap();
                let (writer, errors) = LibraryWriter::new(store);
                view.store = Some(writer);
                view.desktop_event(mpris::Event::Volume(view.library.volume), cx);
                view.drag = Some(Drag::Seek);
                view.drag_position = Some(42.);
                view.end_drag(cx);
                drop(view.store.take());
                assert!(errors.is_empty());
                assert!(!path.exists(), "Seeking and unchanged volume must not save an identical library");

                let (store, _) = Store::load().unwrap();
                let (writer, errors) = LibraryWriter::new(store);
                view.store = Some(writer);
                view.save();
                assert!(view.set_volume(40., cx));
                view.drag = Some(Drag::Volume);
                view.end_drag(cx);
                drop(view.store.take());
                assert!(errors.is_empty());
                let (_, saved) = Store::load().unwrap();
                assert_eq!(saved.volume, 40., "Changed volume must still be saved on release");
                assert_eq!(saved.queue.index, view.library.queue.index);
                view.pending_seek = None;
                view.set_volume(0., cx);
            }).unwrap();
            let signed_in = window.update(cx, |view, window, cx| {
                let previous = view.account.as_ref().unwrap().session.clone();
                let profile: BrowserProfile = serde_json::from_value(json!({
                    "browser":"Synthetic browser", "name":"Confirmed profile", "path":"/synthetic/profile"
                })).unwrap();
                let cancelled = Operation::default();
                view.account_operation = Some(AccountOperation::Connecting(cancelled.clone(), BrowserConnection::Opening { left_app: false }));
                view.account_event(account::Event::BrowserReady { operation: cancelled.clone(), result: Ok(profile.clone()) }, window, cx);
                assert!(commands.is_empty(), "Opening the browser must not import its existing account before a return");
                assert!(matches!(view.account_operation, Some(AccountOperation::Connecting(_, BrowserConnection::Ready(_)))));
                view.read_browser_account(cx);
                let account::Command::ReadBrowser { operation, profile: requested } = commands.try_recv().unwrap() else { panic!("Returning from sign-in must read the resolved profile"); };
                assert!(operation.matches(&cancelled));
                assert_eq!(serde_json::to_value(requested).unwrap(), serde_json::to_value(&profile).unwrap());
                view.read_browser_account(cx);
                assert!(commands.is_empty(), "Repeated activation must not duplicate an in-flight read");
                view.overlay = Some(Overlay::SignIn);
                view.close_overlay(window, cx);
                assert!(view.overlay.is_none());
                assert!(cancelled.is_cancelled());
                view.account_event(account::Event::AccountPreview { operation: cancelled, result: Ok(connection(session("cancelled-browser"))) }, window, cx);
                assert!(view.account_operation.is_none() && commands.is_empty());
                assert!(Arc::ptr_eq(&view.account.as_ref().unwrap().session, &previous));

                let operation = Operation::default();
                view.account_operation = Some(AccountOperation::Connecting(operation.clone(), BrowserConnection::Opening { left_app: false }));
                view.account_event(account::Event::BrowserReady { operation: operation.clone(), result: Ok(profile.clone()) }, window, cx);
                view.read_browser_account(cx);
                assert!(matches!(commands.try_recv().unwrap(), account::Command::ReadBrowser { .. }));
                view.account_event(account::Event::AccountPreview { operation: Operation::default(), result: Err("Superseded account read".into()) }, window, cx);
                assert!(matches!(view.account_operation, Some(AccountOperation::Connecting(_, BrowserConnection::Reading(_)))));
                view.account_event(account::Event::AccountPreview { operation: operation.clone(), result: Err("Finish signing in".into()) }, window, cx);
                assert!(matches!(view.account_operation, Some(AccountOperation::Connecting(_, BrowserConnection::Ready(_)))));
                assert!(Arc::ptr_eq(&view.account.as_ref().unwrap().session, &previous));
                view.read_browser_account(cx);
                let account::Command::ReadBrowser { operation: retry, profile: requested } = commands.try_recv().unwrap() else { panic!("Returning after a failed read must retry the same profile"); };
                assert!(retry.matches(&operation));
                assert_eq!(serde_json::to_value(requested).unwrap(), serde_json::to_value(&profile).unwrap());
                let mut candidate = session("browser-sign-in");
                Arc::get_mut(&mut candidate).unwrap().source_profile = Some(profile);
                view.account_event(account::Event::AccountPreview { operation, result: Ok(connection(candidate.clone())) }, window, cx);
                assert!(matches!(view.account_operation, Some(AccountOperation::Activating { .. })), "A verified sign-in must activate without another confirmation button");
                assert!(Arc::ptr_eq(&view.account.as_ref().unwrap().session, &previous));
                assert!(commands.is_empty(), "Saving must wait for the actual player acknowledgment");
                candidate
            }).unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                cx.background_executor().timer(Duration::from_millis(10)).await;
                let pending = window.update(cx, |view, _, _| matches!(view.account_operation, Some(AccountOperation::Activating { .. }))).unwrap();
                if !pending { break; }
                assert!(Instant::now() < deadline, "Player did not acknowledge browser sign-in");
            }
            window.update(cx, |view, _, cx| {
                assert!(Arc::ptr_eq(&view.account.as_ref().unwrap().session, &signed_in));
                assert!(Arc::ptr_eq(view.session.read().unwrap().as_ref().unwrap(), &signed_in));
                let account::Command::Persist(saved) = commands.try_recv().unwrap() else { panic!("Successful sign-in must persist without another button"); };
                assert!(Arc::ptr_eq(&saved, &signed_in));
                assert!(saved.source_profile.is_some());
                view.read_browser_account(cx);
                assert!(commands.is_empty());
                view.stop_playback(cx);
                prepare(view, signed_in.clone());
            }).unwrap();
            let original = window.update(cx, |view, window, cx| {
                let current = view.account.as_ref().unwrap().session.clone();
                let mutation = Mutation::RateSong { track: track("invalid-first"), rating: Rating::Like };
                view.pending_mutations.insert(mutation.key());
                view.account_event(account::Event::Applied { session: current.clone(), mutation: Box::new(mutation), result: Err("Synthetic rejected edit".into()), session_rejected: true }, window, cx);
                let account::Command::Refresh { operation, expected } = commands.try_recv().unwrap() else { panic!("Rejected edits must refresh without replay"); };
                assert!(Arc::ptr_eq(&expected, &current));
                assert!(view.pending_mutations.is_empty());
                assert!(!view.refresh_session(false, cx));
                view.account_event(account::Event::Connected { operation, result: Ok(Some(connection(current.clone()))), save_session: false }, window, cx);
                assert_eq!(view.active_id, Some(900));
                assert_eq!(view.position, 17.);
                assert!(view.error.is_some(), "The failed edit must remain visible for explicit retry");
                assert!(!view.refresh_session(false, cx));
                assert!(commands.is_empty());
                view.next_session_refresh = Some(Instant::now() - Duration::from_secs(1));
                let operation = refreshing(view, &commands, cx);
                view.account_event(account::Event::Connected { operation, result: Ok(Some(connection(current.clone()))), save_session: false }, window, cx);
                current
            }).unwrap();
            window.update(cx, |view, window, cx| {
                prepare(view, original.clone());
                view.start_queue(PlaybackQuery::Watch { track: Box::new(track("invalid-radio")), mode: WatchMode::Radio }, cx);
                let job = view.pending_playback_jobs.try_recv().unwrap();
                view.playback_result(PlaybackResult::Watch { id: job.id, account_revision: job.account_revision, update: QueueUpdate::Replace, result: Err("Synthetic rejected Radio read".into()), session_rejected: true }, cx);
                let account::Command::Refresh { operation, .. } = commands.try_recv().unwrap() else { panic!("Radio rejection must refresh"); };
                assert!(view.pending_playback_jobs.is_empty());
                view.account_event(account::Event::Connected { operation, result: Ok(Some(connection(original.clone()))), save_session: false }, window, cx);
                let retried = view.pending_playback_jobs.try_recv().unwrap();
                assert!(matches!(retried.query, PlaybackQuery::Watch { track, mode: WatchMode::Radio } if track.id == "invalid-radio"));
                assert!(retried.session.is_some_and(|session| Arc::ptr_eq(&session, &original)));
                assert!(view.active_id.is_none() && view.error.is_none());
                assert!(commands.is_empty());

                view.stop_playback(cx);
                prepare(view, original.clone());
                view.start_queue(PlaybackQuery::Watch { track: Box::new(track("invalid-radio")), mode: WatchMode::Radio }, cx);
                let superseded = view.pending_playback_jobs.try_recv().unwrap();
                let operation = refreshing(view, &commands, cx);
                view.account_event(account::Event::Connected { operation, result: Ok(Some(connection(original.clone()))), save_session: false }, window, cx);
                let job = view.pending_playback_jobs.try_recv().unwrap();
                assert!(matches!(job.query, PlaybackQuery::Watch { track, mode: WatchMode::Radio } if track.id == "invalid-radio"));
                assert!(job.session.is_some_and(|session| Arc::ptr_eq(&session, &original)));
                view.playback_result(PlaybackResult::Watch { id: superseded.id, account_revision: superseded.account_revision, update: QueueUpdate::Replace, result: Err("Superseded read".into()), session_rejected: true }, cx);
                assert!(view.active_id.is_none() && view.error.is_none() && commands.is_empty());

                view.stop_playback(cx);
                prepare(view, original.clone());
                let operation = refreshing(view, &commands, cx);
                view.start_queue(PlaybackQuery::Watch { track: Box::new(track("invalid-radio")), mode: WatchMode::Radio }, cx);
                view.library.queue.select(1);
                view.play_current_at(13., false, WatchMode::Details, cx);
                view.seek_current(23., cx);
                view.toggle_play(cx);
                assert!(view.pending_playback_jobs.is_empty());
                view.account_event(account::Event::Connected { operation, result: Ok(Some(connection(original.clone()))), save_session: false }, window, cx);
                assert_eq!(view.library.queue.index, Some(1));
                assert_eq!(view.position, 23.);
                assert!(view.error.as_deref().is_some_and(|error| error.contains("invalid YouTube video ID")));
                assert!(view.pending_playback_jobs.is_empty() && commands.is_empty());

                view.stop_playback(cx);
                prepare(view, original.clone());
                view.library.queue.replace(vec![track("invalid-first")], 0);
                view.playback_request(PlaybackQuery::Watch { track: Box::new(track("invalid-first")), mode: WatchMode::Issued }, cx);
                let job = view.pending_playback_jobs.try_recv().unwrap();
                view.next(false, cx);
                assert!(view.queue_waiting.is_some());
                view.playback_result(PlaybackResult::Watch { id: job.id, account_revision: job.account_revision, update: QueueUpdate::Adopt, result: Err("Synthetic rejected playlist read".into()), session_rejected: true }, cx);
                let account::Command::Refresh { operation, .. } = commands.try_recv().unwrap() else { panic!("Issued rejection must retain the advance while refreshing"); };
                view.account_event(account::Event::Connected { operation, result: Err("Synthetic unavailable refresh".into()), save_session: false }, window, cx);
                assert!(matches!(view.account_operation, Some(AccountOperation::RefreshFailed { .. })));
                assert!(view.pending_playback_jobs.is_empty());
                view.toggle_play(cx);
                let account::Command::Refresh { operation, .. } = commands.try_recv().unwrap() else { panic!("Pause during failed recovery must retry the pending read"); };
                view.account_event(account::Event::Connected { operation, result: Ok(Some(connection(original.clone()))), save_session: false }, window, cx);
                let job = view.pending_playback_jobs.try_recv().unwrap();
                assert!(matches!(job.query, PlaybackQuery::Watch { track, mode: WatchMode::Issued } if track.id == "invalid-first"));
                assert!(job.session.is_some_and(|session| Arc::ptr_eq(&session, &original)));
                assert!(view.queue_waiting.is_some() && !view.playback.is_paused());
                assert!(view.error.is_none() && commands.is_empty());
                let watch = Watch { current: Some(track("invalid-first")), current_index: Some(0), queue: vec![track("invalid-first"), track("invalid-second")], source: None, lyrics: None, related: None, alternatives: None, continuation: None, source_target: None };
                view.playback_result(PlaybackResult::Watch { id: job.id, account_revision: job.account_revision, update: QueueUpdate::Adopt, result: Ok(Box::new(watch)), session_rejected: false }, cx);
                assert_eq!(view.library.queue.index, Some(1));
                assert_eq!(view.library.queue.current().unwrap().id, "invalid-second");
                assert!(view.queue_waiting.is_none() && view.active_id.is_none());
                assert!(view.error.as_deref().is_some_and(|error| error.contains("invalid YouTube video ID")));

                view.stop_playback(cx);
                prepare(view, original.clone());
                view.library.repeat = Repeat::One;
                let operation = refreshing(view, &commands, cx);
                view.next(true, cx);
                assert_eq!(view.library.queue.index, Some(0));
                view.account_event(account::Event::Connected { operation, result: Ok(Some(connection(original.clone()))), save_session: false }, window, cx);
                assert_eq!(view.library.queue.index, Some(0), "Natural advance must preserve Repeat one through recovery");
                assert!(view.error.as_deref().is_some_and(|error| error.contains("invalid YouTube video ID")));
                view.library.repeat = Repeat::Off;

                view.stop_playback(cx);
                prepare(view, original.clone());
                let operation = refreshing(view, &commands, cx);
                view.account_event(account::Event::Connected { operation, result: Err("Synthetic unavailable library refresh".into()), save_session: false }, window, cx);
                view.toggle_play(cx);
                assert_eq!(view.active_id, Some(900));
                assert_eq!(view.position, 17.);
                assert_eq!(view.playback, Playback::Loaded { paused: false, buffering: false });
                assert!(commands.is_empty(), "Pause on the existing stream must not restart recovery");
            }).unwrap();
            for (case, value) in ["stopped", "restart", "selection", "radio", "video", "signout"].into_iter().zip(["second", "third", "fourth", "fifth", "sixth", "seventh"]) {
                let replacement = session(value);
                window.update(cx, |view, window, cx| {
                    view.stop_playback(cx);
                    prepare(view, original.clone());
                    if case == "signout" {
                        let operation = Operation::default();
                        let profile: BrowserProfile = serde_json::from_value(json!({
                            "browser":"Synthetic browser", "name":"Confirmed profile", "path":"/synthetic/profile"
                        })).unwrap();
                        view.account_operation = Some(AccountOperation::Connecting(operation.clone(), BrowserConnection::Reading(profile)));
                        view.account_event(account::Event::AccountPreview { operation, result: Ok(connection(replacement.clone())) }, window, cx);
                        assert!(matches!(view.account_operation, Some(AccountOperation::Activating { .. })));
                    } else {
                        renew(view, replacement.clone(), &commands, window, cx);
                    }
                    match case {
                        "stopped" => view.stop_playback(cx),
                        "restart" => view.play_current_at(0., false, WatchMode::Details, cx),
                        "selection" => {
                            view.library.queue.select(1);
                            view.play_current_at(7., true, WatchMode::Issued, cx);
                        }
                        "radio" => {
                            view.start_queue(PlaybackQuery::Watch { track: Box::new(track("invalid-radio")), mode: WatchMode::Radio }, cx);
                            assert!(view.pending_playback_jobs.is_empty(), "Queue requests must wait for credential acknowledgment");
                            view.toggle_play(cx);
                            view.next(false, cx);
                        }
                        "video" => {
                            view.media_pair = Some(Box::new(MediaPair { song: view.library.queue.current().unwrap().clone(), video: track("invalid-video") }));
                            view.set_video_mode(true, cx);
                        }
                        "signout" => view.disconnect_account(cx),
                        _ => unreachable!(),
                    }
                }).unwrap();
                if case == "signout" {
                    let account::Command::Disconnect { playback, .. } = commands.try_recv().unwrap() else { panic!("Expected sign-out cleanup"); };
                    if let Some(response) = playback.unwrap() { assert!(response.recv().await.unwrap().is_ok()); }
                    cx.background_executor().timer(Duration::from_millis(10)).await;
                }
                let deadline = Instant::now() + Duration::from_secs(5);
                loop {
                    cx.background_executor().timer(Duration::from_millis(10)).await;
                    let pending = window.update(cx, |view, _, _| matches!(view.account_operation, Some(AccountOperation::Activating { .. }))).unwrap();
                    if !pending { break; }
                    assert!(Instant::now() < deadline, "Player did not acknowledge synthetic credentials");
                }
                window.update(cx, |view, _, _| {
                    assert_eq!(view.library.queue.tracks.len(), 2);
                    assert!(view.active_id.is_none());
                    if case == "signout" {
                        assert!(view.account.is_none());
                        assert!(view.session.read().unwrap().is_none());
                        assert!(commands.is_empty(), "Cancelled activation must not persist");
                        return;
                    }
                    assert!(Arc::ptr_eq(&view.account.as_ref().unwrap().session, &replacement));
                    assert!(matches!(&view.route, Route::Browse(link) if link.id == "synthetic-route"));
                    match commands.try_recv().unwrap() {
                        account::Command::Persist(saved) => assert!(Arc::ptr_eq(&saved, &replacement)),
                        _ => panic!("Renewal must never replay a change"),
                    }
                    assert!(commands.is_empty());
                    if case == "stopped" { assert!(view.error.is_none()); assert_eq!(view.position, 0.); }
                    else if case == "radio" {
                        assert!(view.error.is_none());
                        assert!(view.playback.is_paused() && view.queue_waiting.is_some());
                        let job = view.pending_playback_jobs.try_recv().unwrap();
                        assert!(job.session.as_ref().is_some_and(|session| Arc::ptr_eq(session, &replacement)));
                        assert!(matches!(job.query, PlaybackQuery::Watch { track, mode: WatchMode::Radio } if track.id == "invalid-radio"));
                        assert!(view.pending_playback_jobs.is_empty());
                    }
                    else if case == "video" {
                        assert!(view.library.video && view.player_open);
                        assert_eq!(view.library.queue.current().unwrap().id, "invalid-video");
                        assert_eq!(view.position, 17.);
                        assert!(view.error.as_deref().is_some_and(|error| error.contains("invalid YouTube video ID")));
                    }
                    else {
                        assert!(view.error.as_deref().is_some_and(|error| error.contains("invalid YouTube video ID")));
                        assert_eq!(view.position, if case == "restart" { 0. } else { 7. });
                        assert_eq!(view.library.queue.index, Some(usize::from(case == "selection")));
                    }
                }).unwrap();
            }
            result.store(true, Ordering::Release);
            cx.update(|cx| cx.quit());
        }).detach();
        cx.spawn(async move |cx| {
            cx.background_executor().timer(Duration::from_secs(25)).await;
            cx.update(|cx| cx.quit());
        }).detach();
    });
    ensure!(
        passed.load(Ordering::Acquire),
        "Native account recovery assertions did not finish"
    );
    Ok(())
}
