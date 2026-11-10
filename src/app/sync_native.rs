//! Synthetic account UI checks. Never reads a saved account or sends network writes.
use super::tests::page;
use crate::{
    account::{self, Connection, Effect, Mutation},
    app::{CatalogRequests, CatalogResult, CatalogTarget, Music, Overlay, Query, Route, Startup},
    assets::Assets,
    auth::{self, Session},
    catalog::{Account, BrowseLink, Rating},
    track_state,
};
use anyhow::{Context as _, Result, ensure};
use gpui::{
    AppContext, Bounds, ScrollStrategy, TitlebarOptions, WindowBounds, WindowKind, WindowOptions,
    point, px, size,
};
use serde_json::json;
use std::{
    borrow::Cow,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

#[test]
#[ignore = "requires desktop, private D-Bus and isolated runtime/data; set YT_MUSIC_NATIVE_SYNC_TEST=1; synthetic account only"]
fn native_sync_preserves_loaded_pages_and_rolls_back_ratings() -> Result<()> {
    ensure!(std::env::var("YT_MUSIC_NATIVE_SYNC_TEST").as_deref() == Ok("1"));
    let runtime =
        PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").context("Set private runtime")?);
    let data = PathBuf::from(std::env::var_os("XDG_DATA_HOME").context("Set isolated data")?);
    ensure!(data.starts_with(&runtime) && data != runtime);
    crate::video::initialize()?;
    let passed = Arc::new(AtomicBool::new(false));
    let result = passed.clone();
    gpui_platform::application().with_assets(Assets).run(move |cx| {
        cx.text_system().add_fonts(vec![Cow::Borrowed(include_bytes!("../../assets/fonts/Roboto.ttf").as_slice())]).unwrap();
        crate::input::init(cx);
        let (sender, commands) = async_channel::bounded(64);
        let window = cx.open_window(WindowOptions {
            window_bounds:Some(WindowBounds::Windowed(Bounds::new(point(px(40.),px(40.)),size(px(1050.),px(700.))))),
            kind:WindowKind::PopUp,
            titlebar:Some(TitlebarOptions { title:Some("Music sync validation".into()), ..Default::default() }),
            focus:true, ..Default::default()
        }, move |window,cx| cx.new(|cx| {
            let mut view = Music::new(Startup::Guest,window,cx);
            view.account_commands.close();
            view.account_worker.take().unwrap().finish().unwrap();
            view.account_commands = sender;
            for target in CatalogTarget::ALL { view.catalog_requests.cancel(target); }
            view.catalog_requests = CatalogRequests::new();
            for generation in view.catalog_requests.generations.iter() { generation.store(1000,Ordering::Relaxed); }
            view.playback_jobs.close();
            let (jobs,pending) = async_channel::bounded(1);
            view.playback_jobs = jobs;
            view.pending_playback_jobs = pending;
            view.track_state = track_state::Worker::idle();
            view.player = None;
            view.mpris = None;
            view.video = None;
            view.store = None;
            view.library.volume = 0.;
            let session: Session = serde_json::from_value(json!({"cookie":"SAPISID=synthetic","playback_cookie":"# Netscape HTTP Cookie File\n","authuser":"0","delegated_session_id":null,"data_sync_id":"synthetic||","client_version":"test"})).unwrap();
            view.session = auth::shared(Some(session));
            let session = view.session.read().unwrap().clone().unwrap();
            view.account = Some(Connection { session, account:Account { name:"Synthetic account".into(),handle:String::new(),thumbnail:None } });
            view.route = Route::Browse(BrowseLink::new("VLsynthetic","Sync validation playlist"));
            view.page = page(500,true);
            view.page.title = "Sync validation playlist".into();
            view.rebuild_rows();
            view
        })).unwrap();
        cx.activate(true);
        cx.spawn(async move |cx| {
            window.update(cx,|view,_,cx| {
                view.list_scroll.scroll_to_item(450,ScrollStrategy::Top);
                let track = view.page.tracks()[450].clone();
                view.overlay = Some(Overlay::Track { track, index: Some(450), queue_index: None });
                cx.notify();
            }).unwrap();
            cx.background_executor().timer(Duration::from_millis(600)).await;
            println!("SYNC_SCREENSHOT loaded-menu");
            cx.background_executor().timer(Duration::from_millis(800)).await;
            window.update(cx,|view,window,cx| {
                let revision = view.account_revision;
                let scroll = view.list_scroll.0.borrow().base_handle.offset();
                view.refresh_remote(cx);
                assert_eq!(view.account_revision,revision,"Refresh is not an account switch");
                let job = view.catalog_requests.pending.try_recv().unwrap();
                assert!(matches!(job.query,Query::Revalidate(_)));
                view.catalog_result(CatalogResult { id:job.id, account_revision:job.account_revision, append:false, revalidate:true, result:Ok(page(50,true)),session_rejected:false,target:CatalogTarget::Page },cx);
                assert_eq!(view.rows.len(),500);
                assert!(matches!(view.overlay,Some(Overlay::Track { index:Some(450),.. })));
                assert_eq!(view.list_scroll.0.borrow().base_handle.offset(),scroll);
                view.overlay = None;
                view.navigate(Route::Explore,cx);
                let start = Instant::now();
                view.go_back(cx);
                assert_eq!(view.rows.len(),500,"Back renders cached rows before a network reply");
                println!("500-row cached Back: {:?}",start.elapsed());
                let track = view.page.tracks()[450].clone();
                let rate = Mutation::RateSong { track:track.clone(),rating:Rating::Like };
                assert!(view.mutate(rate.clone(),cx));
                assert!(view.is_liked(&track),"Rating must change before a reply");
                assert!(matches!(commands.try_recv().unwrap(),account::Command::Apply { .. }));
                let session = view.account.as_ref().unwrap().session.clone();
                view.account_event(account::Event::Applied { session, mutation:Box::new(rate),result:Err("Synthetic save failure".into()),session_rejected:false }, window, cx);
                assert!(!view.is_liked(&track));
                assert_eq!(view.error.as_deref(),Some("Synthetic save failure"));
                assert_eq!(view.rows.len(),500);
                view.library.queue.replace(vec![track.clone(),view.page.tracks()[451].clone()],0);
                view.active_id = Some(77);
                view.playback = crate::app::Playback::Loaded { paused:false,buffering:false };
                assert!(view.mutate(Mutation::RateSong { track:track.clone(),rating:Rating::Dislike },cx));
                assert_eq!(view.library.queue.index,Some(1),"Disliking the current song skips immediately");
                while commands.try_recv().is_ok() {}
                view.error = None;
                let add = Mutation::AddToPlaylist { id:"VLsynthetic".into(),video_ids:vec![track.id.clone()],duplicates:false };
                assert!(view.mutate(add.clone(),cx));
                let session = view.account.as_ref().unwrap().session.clone();
                view.account_event(account::Event::Applied { session,mutation:Box::new(add),result:Ok(Effect::AlreadyAdded),session_rejected:false },window,cx);
                assert!(matches!(view.overlay,Some(Overlay::AlreadyAdded { .. })));
                while view.catalog_requests.pending.try_recv().is_ok() {}
                view.request(Query::SidebarPlaylists,cx);
                let first = view.catalog_requests.pending.try_recv().unwrap();
                let mut choices = page(0,true);
                choices.shelves[0].items.push(crate::catalog::Item::Collection(serde_json::from_value(json!({"id":"VLfirst","title":"First playlist","subtitle":"Playlist"})).unwrap()));
                view.catalog_result(CatalogResult { id:first.id,account_revision:view.account_revision,append:false,revalidate:false,result:Ok(choices),session_rejected:false,target:CatalogTarget::Sidebar },cx);
                let token = view.sync.sidebar_continuation.clone().expect("Sidebar retains continuation");
                view.request(Query::SidebarMore(token),cx);
                let more = view.catalog_requests.pending.try_recv().unwrap();
                let mut choices = page(0,false);
                choices.shelves[0].items.push(crate::catalog::Item::Collection(serde_json::from_value(json!({"id":"VLsecond","title":"Second playlist","subtitle":"Playlist"})).unwrap()));
                view.catalog_result(CatalogResult { id:more.id,account_revision:view.account_revision,append:true,revalidate:false,result:Ok(choices),session_rejected:false,target:CatalogTarget::Sidebar },cx);
                assert_eq!(view.sidebar_playlists.len(),2);
                assert!(view.sync.sidebar_continuation.is_none());
                let session = view.account.as_ref().unwrap().session.clone();
                view.account_event(account::Event::Applied { session,mutation:Box::new(Mutation::CreatePlaylist { title:"New playlist".into(),description:String::new(),privacy:account::Privacy::Private,initial_video_id:None }),result:Ok(Effect::CreatedPlaylist { id:"created".into() }),session_rejected:false },window,cx);
                assert!(std::iter::from_fn(|| view.catalog_requests.pending.try_recv().ok()).any(|job| job.query.target()==CatalogTarget::Sidebar),"Creating a playlist reloads sidebar membership");
                let session = view.account.as_ref().unwrap().session.clone();
                view.account_event(account::Event::Applied { session,mutation:Box::new(Mutation::DeletePlaylist { id:"first".into() }),result:Ok(Effect::Changed),session_rejected:false },window,cx);
                assert!(!view.sidebar_playlists.iter().any(|playlist| playlist.id=="VLfirst"));
                assert!(std::iter::from_fn(|| view.catalog_requests.pending.try_recv().ok()).any(|job| job.query.target()==CatalogTarget::Sidebar),"Deleting a playlist reloads sidebar membership");
                view.go_back(cx);
                assert_eq!(view.rows.len(),500);
                view.navigate(Route::Browse(BrowseLink::new("VLtarget","Target playlist")),cx);
                view.page = page(0,false);
                view.library.queue = Default::default();
                view.patch_added("target",&[(track.id.clone(),"new-occurrence".into())],cx);
                assert_eq!(view.rows.len(),1,"An addition can resolve metadata from the page left behind");
                assert_eq!(view.page.tracks()[0].details.set_video_id.as_deref(),Some("new-occurrence"));
                let id = view.catalog_requests.generations[CatalogTarget::Page as usize].load(Ordering::Relaxed);
                view.catalog_result(CatalogResult { id,account_revision:view.account_revision,append:false,revalidate:false,result:Ok(page(0,false)),session_rejected:false,target:CatalogTarget::Page },cx);
                assert_eq!(view.rows.len(),1,"An older page reply must preserve the acknowledged addition");
                view.go_back(cx);
                assert_eq!(view.rows.len(),500);
                cx.notify();
            }).unwrap();
            result.store(true,Ordering::Release);
            cx.background_executor().timer(Duration::from_millis(600)).await;
            println!("SYNC_SCREENSHOT already-added");
            cx.background_executor().timer(Duration::from_secs(5)).await;
            cx.update(|cx| cx.quit());
        }).detach();
        cx.spawn(async move |cx| {
            cx.background_executor().timer(Duration::from_secs(20)).await;
            cx.update(|cx| cx.quit());
        }).detach();
    });
    ensure!(
        passed.load(Ordering::Acquire),
        "Native sync checks did not complete"
    );
    Ok(())
}
