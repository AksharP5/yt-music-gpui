//! Synthetic account UI checks. Never reads a saved account or sends network writes.
use crate::app::sync::tests::{page, response};
use crate::{
    account::{self, Connection, Effect, Mutation},
    app::{CatalogRequests, CatalogResult, CatalogTarget, Music, Overlay, Query, Route, Startup},
    assets::Assets,
    auth::{self, Session},
    catalog::{Account, BrowseLink, Rating},
    track_state,
};
use anyhow::{Result, ensure};
use gpui::{
    AppContext, Bounds, KeyDownEvent, Keystroke, ListOffset, Modifiers, ScrollStrategy,
    TitlebarOptions, WindowBounds, WindowKind, WindowOptions, point, px, size,
};
use serde_json::json;
use std::{
    borrow::Cow,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

#[test]
#[ignore = "requires desktop, private D-Bus and isolated runtime/data; set YT_MUSIC_NATIVE_TESTS=1; synthetic account only"]
fn native_sync_preserves_loaded_pages_and_rolls_back_ratings() -> Result<()> {
    super::isolate()?;
    let passed = Arc::new(AtomicBool::new(false));
    let result = passed.clone();
    gpui_platform::application().with_assets(Assets).run(move |cx| {
        cx.text_system().add_fonts(vec![Cow::Borrowed(include_bytes!("../../../assets/fonts/Roboto.ttf").as_slice())]).unwrap();
        crate::input::init(cx);
        let (sender, commands) = async_channel::bounded(64);
        let window = cx.open_window(WindowOptions {
            window_bounds:Some(WindowBounds::Windowed(Bounds::new(point(px(40.),px(40.)),size(px(1280.),px(700.))))),
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
            let tail = view.page.shelves[0].items.split_off(50);
            let mut shelf = view.page.shelves[0].clone();
            shelf.items = tail;
            view.page.shelves.push(shelf);
            view.sync.page_coverage = crate::app::sync::PageCoverage { first_page_len: 1, continued: true };
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
                view.catalog_result(CatalogResult { id:job.id, account_revision:job.account_revision,account_generation:job.account_generation,history_generation:job.history_generation, append:false, revalidate:true, result:Ok(page(50,true)),session_rejected:false,target:CatalogTarget::Page },cx);
                assert_eq!(view.rows.len(),500);
                assert!(matches!(view.overlay,Some(Overlay::Track { index:Some(450),.. })));
                assert_eq!(view.list_scroll.0.borrow().base_handle.offset(),scroll);
                view.overlay = None;
                assert!(view.error.is_none(),"Synthetic fixture error: {:?}",view.error);
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
                view.catalog_result(CatalogResult { id:first.id,account_revision:view.account_revision,account_generation:first.account_generation,history_generation:first.history_generation,append:false,revalidate:false,result:Ok(choices),session_rejected:false,target:CatalogTarget::Sidebar },cx);
                let token = view.sync.sidebar_continuation.clone().expect("Sidebar retains continuation");
                view.request(Query::SidebarMore(token),cx);
                let more = view.catalog_requests.pending.try_recv().unwrap();
                let mut choices = page(0,false);
                choices.shelves[0].items.push(crate::catalog::Item::Collection(serde_json::from_value(json!({"id":"VLsecond","title":"Second playlist","subtitle":"Playlist"})).unwrap()));
                view.catalog_result(CatalogResult { id:more.id,account_revision:view.account_revision,account_generation:more.account_generation,history_generation:more.history_generation,append:true,revalidate:false,result:Ok(choices),session_rejected:false,target:CatalogTarget::Sidebar },cx);
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
                assert!(view.rows.is_empty(),"The earlier playlist edit must prevent restoring a page whose continuation may be wrong");
                let job = view.catalog_requests.pending.try_recv().expect("An evicted playlist must be fetched again");
                assert!(job.query.target()==CatalogTarget::Page);
                view.catalog_result(CatalogResult { id:job.id,account_revision:job.account_revision,account_generation:job.account_generation,history_generation:job.history_generation,append:false,revalidate:false,result:Ok(page(500,true)),session_rejected:false,target:CatalogTarget::Page },cx);
                assert_eq!(view.rows.len(),500);
                view.navigate(Route::Browse(BrowseLink::new("VLtarget","Target playlist")),cx);
                let job = view.catalog_requests.pending.try_recv().unwrap();
                view.page = page(0,false);
                view.library.queue = Default::default();
                let session = view.account.as_ref().unwrap().session.clone();
                view.account_event(account::Event::Applied { session,mutation:Box::new(Mutation::AddToPlaylist { id:"target".into(),video_ids:vec![track.id.clone()],duplicates:false }),result:Ok(Effect::PlaylistAdded { additions:vec![(track.id.clone(),"new-occurrence".into())] }),session_rejected:false },window,cx);
                assert_eq!(view.rows.len(),1,"An addition can resolve metadata from the page left behind");
                assert_eq!(view.page.tracks()[0].details.set_video_id.as_deref(),Some("new-occurrence"));
                view.catalog_result(CatalogResult { id:job.id,account_revision:job.account_revision,account_generation:job.account_generation,history_generation:job.history_generation,append:false,revalidate:false,result:Ok(page(0,false)),session_rejected:false,target:CatalogTarget::Page },cx);
                assert_eq!(view.rows.len(),1,"An older page reply must preserve the acknowledged addition");
                view.go_back(cx);
                assert!(view.rows.is_empty(),"Back must fetch the playlist discarded by the addition");
                let job = view.catalog_requests.pending.try_recv().unwrap();
                view.catalog_result(CatalogResult { id:job.id,account_revision:job.account_revision,account_generation:job.account_generation,history_generation:job.history_generation,append:false,revalidate:false,result:Ok(page(500,true)),session_rejected:false,target:CatalogTarget::Page },cx);
                assert_eq!(view.rows.len(),500);
                cx.notify();
            }).unwrap();
            cx.background_executor().timer(Duration::from_millis(600)).await;
            println!("SYNC_SCREENSHOT already-added");
            window.update(cx,|view,_,cx| {
                view.overlay = None;
                view.library.queue = Default::default();
                view.navigate(Route::Liked,cx);
                let job = std::iter::from_fn(|| view.catalog_requests.pending.try_recv().ok()).collect::<Vec<_>>().into_iter().find(|job| job.query.target()==CatalogTarget::Page).unwrap();
                view.catalog_result(CatalogResult { id:job.id,account_revision:view.account_revision,account_generation:job.account_generation,history_generation:job.history_generation,append:false,revalidate:false,result:Ok(response(&[("Songs",(0..50).collect())],Some("page-2"),true)),session_rejected:false,target:CatalogTarget::Page },cx);
                view.error = Some("Synthetic playback failure".into());
                view.prefetch_page(50,50,12,cx);
                let job = view.catalog_requests.pending.try_recv().expect("A playback failure must not block page continuation");
                assert!(matches!(job.query,Query::More { .. }));
                view.catalog_result(CatalogResult { id:job.id,account_revision:view.account_revision,account_generation:job.account_generation,history_generation:job.history_generation,append:true,revalidate:false,result:Ok(response(&[("Songs",(50..500).collect())],Some("page-11"),true)),session_rejected:false,target:CatalogTarget::Page },cx);
                view.prefetch_page(500,500,12,cx);
                let job = view.catalog_requests.pending.try_recv().unwrap();
                view.catalog_result(CatalogResult { id:job.id,account_revision:view.account_revision,account_generation:job.account_generation,history_generation:job.history_generation,append:true,revalidate:false,result:Err("Synthetic page failure".into()),session_rejected:false,target:CatalogTarget::Page },cx);
                view.error = Some("Another playback failure".into());
                view.prefetch_page(500,500,12,cx);
                assert!(view.catalog_requests.pending.try_recv().is_err(),"Page errors block automatic retry even if playback replaces the banner");
                view.list_scroll.scroll_to_item_strict(450,ScrollStrategy::Top);
                cx.notify();
            }).unwrap();
            cx.background_executor().timer(Duration::from_millis(400)).await;
            window.update(cx,|view,window,cx| {
                window.focus(&view.focus,cx);
                let track = view.page.tracks()[450].clone();
                view.overlay = Some(Overlay::Track { track,index:Some(450),queue_index:None });
                // The keyboard ignores refresh while a menu is open; the reload
                // method also supports keeping an already-open menu anchored.
                view.reload_current_page(cx);
                let job = std::iter::from_fn(|| view.catalog_requests.pending.try_recv().ok()).collect::<Vec<_>>().into_iter().find(|job| job.query.target()==CatalogTarget::Page).unwrap();
                let mut first = vec![900];
                first.extend((0..49).rev());
                view.catalog_result(CatalogResult { id:job.id,account_revision:view.account_revision,account_generation:job.account_generation,history_generation:job.history_generation,append:false,revalidate:true,result:Ok(response(&[("Songs",first)],Some("fresh-page-2"),true)),session_rejected:false,target:CatalogTarget::Page },cx);
                assert_eq!(view.rows.len(),50,"Ctrl R replaces stale loaded pages");
                assert!(view.sync.page_error.is_none(),"Reload retries the page error");
                let job = view.catalog_requests.pending.try_recv().expect("Reload follows the fresh continuation to find the visible occurrence");
                assert!(matches!(job.query,Query::More { ref token,.. } if token.token()=="fresh-page-2"));
                view.catalog_result(CatalogResult { id:job.id,account_revision:view.account_revision,account_generation:job.account_generation,history_generation:job.history_generation,append:true,revalidate:false,result:Ok(response(&[("Songs",(50..500).rev().collect())],Some("fresh-tail"),true)),session_rejected:false,target:CatalogTarget::Page },cx);
                assert_eq!(view.rows[0].id(),format!("{:011}",900));
                assert!(matches!(view.overlay,Some(Overlay::Track { index:Some(99),.. })),"Menu follows the occurrence after the server reorder");
                assert!(view.sync.page_reload.is_none());
                view.overlay = None;
                let scroll = view.list_scroll.0.borrow();
                let height = scroll.last_item_size.unwrap().contents.height / 500.;
                assert_eq!(scroll.base_handle.offset().y,-height*99.,"Scroll follows occurrence 450 into row 99");
                drop(scroll);
                let event = KeyDownEvent { keystroke:Keystroke { key:"r".into(),modifiers:Modifiers { control:true,..Default::default() },..Default::default() },is_held:false,prefer_character_input:false };
                view.key_down(&event,window,cx);
                let job = std::iter::from_fn(|| view.catalog_requests.pending.try_recv().ok()).collect::<Vec<_>>().into_iter().find(|job| job.query.target()==CatalogTarget::Page).unwrap();
                let mut fresh = response(&[("Songs",(50..500).rev().collect())],None,true);
                fresh.shelves[0].items.insert(0,view.rows[0].clone());
                view.catalog_result(CatalogResult { id:job.id,account_revision:view.account_revision,account_generation:job.account_generation,history_generation:job.history_generation,append:false,revalidate:true,result:Ok(fresh),session_rejected:false,target:CatalogTarget::Page },cx);
                assert_eq!(view.rows.len(),451);
                assert_eq!(view.rows[50].id(),format!("{:011}",450));
                assert!(view.page.continuation.is_none());
                cx.notify();
            }).unwrap();
            cx.background_executor().timer(Duration::from_millis(400)).await;
            println!("SYNC_SCREENSHOT reload-order-anchor");
            cx.background_executor().timer(Duration::from_millis(800)).await;
            window.update(cx,|view,_,cx| {
                view.route = Route::Browse(BrowseLink::new("VLsynthetic","Collection anchor"));
                view.page = response(&[("Songs",(0..500).collect())],Some("tail"),true);
                view.page.header = crate::catalog::parse_page(&json!({"header":{"musicResponsiveHeaderRenderer":{"title":{"simpleText":"Collection anchor"}}},"contents":[]}),"Collection anchor").unwrap().header;
                view.page.title = "Collection anchor".into();
                view.sync.page_coverage = crate::app::sync::PageCoverage { first_page_len:1,continued:false };
                view.rebuild_rows();
                view.detail_list.state.scroll_to(ListOffset { item_ix:401,offset_in_item:px(8.) });
                let track = view.page.tracks()[400].clone();
                view.overlay = Some(Overlay::Track { track,index:Some(400),queue_index:None });
                cx.notify();
            }).unwrap();
            cx.background_executor().timer(Duration::from_millis(400)).await;
            let expected_collection_top = window.update(cx,|view,_,cx| {
                let before = view.detail_list.state.logical_scroll_top();
                assert_eq!(before.item_ix,401);
                let mut fresh = response(&[("Songs",(0..500).rev().collect())],None,true);
                fresh.header = view.page.header.clone();
                fresh.title = "Collection anchor".into();
                view.revalidate_page(cx);
                let job = std::iter::from_fn(|| view.catalog_requests.pending.try_recv().ok()).collect::<Vec<_>>().into_iter().find(|job| job.query.target()==CatalogTarget::Page).unwrap();
                view.catalog_result(CatalogResult { id:job.id,account_revision:view.account_revision,account_generation:job.account_generation,history_generation:job.history_generation,append:false,revalidate:true,result:Ok(fresh),session_rejected:false,target:CatalogTarget::Page },cx);
                let after = view.detail_list.state.logical_scroll_top();
                assert_eq!(after.item_ix,100,"Collection scroll follows occurrence 400 into row 99, after its heading");
                assert_eq!(after.offset_in_item,before.offset_in_item);
                assert!(matches!(view.overlay,Some(Overlay::Track { index:Some(99),.. })));
                (after.item_ix,after.offset_in_item)
            }).unwrap();
            cx.background_executor().timer(Duration::from_millis(400)).await;
            window.update(cx,|view,_,_| {
                let top = view.detail_list.state.logical_scroll_top();
                assert_eq!((top.item_ix,top.offset_in_item),expected_collection_top,"The rendered collection must preserve the anchor");
            }).unwrap();
            println!("SYNC_SCREENSHOT collection-order-anchor");
            cx.background_executor().timer(Duration::from_millis(800)).await;
            window.update(cx,|view,_,cx| {
                view.overlay = None;
                view.request(Query::SidebarPlaylists,cx);
                let job = view.catalog_requests.pending.try_recv().unwrap();
                let mut choices = page(0,true);
                choices.shelves[0].items.push(crate::catalog::Item::Collection(serde_json::from_value(json!({"id":"VLretry","title":"Retry playlist","subtitle":"Playlist"})).unwrap()));
                view.catalog_result(CatalogResult { id:job.id,account_revision:view.account_revision,account_generation:job.account_generation,history_generation:job.history_generation,append:false,revalidate:false,result:Ok(choices),session_rejected:false,target:CatalogTarget::Sidebar },cx);
                let token = view.sync.sidebar_continuation.clone().unwrap();
                view.request(Query::SidebarMore(token),cx);
                let job = view.catalog_requests.pending.try_recv().unwrap();
                view.catalog_result(CatalogResult { id:job.id,account_revision:view.account_revision,account_generation:job.account_generation,history_generation:job.history_generation,append:true,revalidate:false,result:Err("Synthetic sidebar failure".into()),session_rejected:false,target:CatalogTarget::Sidebar },cx);
                view.error = None;
                cx.notify();
            }).unwrap();
            cx.background_executor().timer(Duration::from_millis(400)).await;
            println!("SYNC_SCREENSHOT sidebar-error-stopped");
            cx.background_executor().timer(Duration::from_millis(800)).await;
            window.update(cx,|view,_,cx| {
                assert!(view.catalog_requests.pending.try_recv().is_err(),"Rendering near the sidebar end must not retry a failed continuation");
                assert_eq!(view.sidebar_error.as_deref(),Some("Synthetic sidebar failure"));
                view.request(Query::SidebarPlaylists,cx);
                let job = view.catalog_requests.pending.try_recv().expect("Explicit sidebar Retry remains available");
                view.catalog_result(CatalogResult { id:job.id,account_revision:view.account_revision,account_generation:job.account_generation,history_generation:job.history_generation,append:false,revalidate:false,result:Ok(page(0,false)),session_rejected:false,target:CatalogTarget::Sidebar },cx);
                assert!(view.sidebar_error.is_none());
                view.route = Route::Home;
                view.overlay = None;
                view.player_open = false;
                view.library.queue = Default::default();
                view.page = page(4,false);
                let shelf = view.page.shelves[0].clone();
                view.page.shelves = (0..8).map(|index| {
                    let mut shelf = shelf.clone();
                    shelf.title = format!("Home shelf {index}");
                    shelf
                }).collect();
                view.rebuild_rows();
                view.detail_list.state.reset(9);
                view.detail_list.state.scroll_to(ListOffset { item_ix:4,offset_in_item:px(0.) });
                cx.notify();
            }).unwrap();
            cx.background_executor().timer(Duration::from_millis(500)).await;
            let expected = window.update(cx,|view,_,cx| {
                let before = view.detail_list.state.logical_scroll_top();
                let before = (before.item_ix,before.offset_in_item);
                assert!(before.0 >= 3,"The Home fixture must be scrolled before refresh");
                let visible_title = view.page.shelves[before.0-1].title.clone();
                let mut fresh = view.page.clone();
                fresh.shelves.reverse();
                view.request(Query::Revalidate(Box::new(Query::Home)),cx);
                let job = std::iter::from_fn(|| view.catalog_requests.pending.try_recv().ok()).collect::<Vec<_>>().into_iter().find(|job| job.query.target()==CatalogTarget::Page).unwrap();
                view.catalog_result(CatalogResult { id:job.id,account_revision:job.account_revision,account_generation:job.account_generation,history_generation:job.history_generation,append:false,revalidate:true,result:Ok(fresh),session_rejected:false,target:CatalogTarget::Page },cx);
                let after = view.detail_list.state.logical_scroll_top();
                assert_eq!(view.page.shelves[after.item_ix-1].title,visible_title,"Home follows the visible shelf after the server reorders it");
                assert_eq!(after.offset_in_item,before.1);
                let expected = (after.item_ix,after.offset_in_item);
                let track = view.page.tracks()[0].clone();
                view.patch_confirmed(&Mutation::RateSong { track,rating:Rating::Like },cx);
                let after = view.detail_list.state.logical_scroll_top();
                assert_eq!((after.item_ix,after.offset_in_item),expected,"A save acknowledgement must keep the visible shelf");
                expected
            }).unwrap();
            cx.background_executor().timer(Duration::from_millis(300)).await;
            window.update(cx,|view,_,_| {
                let after = view.detail_list.state.logical_scroll_top();
                assert_eq!((after.item_ix,after.offset_in_item),expected,"The next frame must preserve Home's scroll position");
            }).unwrap();
            window.update(cx,|view,_,cx| {
                view.navigate(Route::Recent,cx);
                let job = std::iter::from_fn(|| view.catalog_requests.pending.try_recv().ok()).find(|job| job.query.target()==CatalogTarget::Page).unwrap();
                view.catalog_result(CatalogResult { id:job.id,account_revision:job.account_revision,account_generation:job.account_generation,history_generation:job.history_generation,append:false,revalidate:false,result:Ok(response(&[("Today",vec![0]),("Yesterday",vec![1])],Some("older"),false)),session_rejected:false,target:CatalogTarget::Page },cx);
                view.request(Query::More { token:view.page.continuation.clone().unwrap(), search:None },cx);
                let job = view.catalog_requests.pending.try_recv().unwrap();
                view.catalog_result(CatalogResult { id:job.id,account_revision:job.account_revision,account_generation:job.account_generation,history_generation:job.history_generation,append:true,revalidate:false,result:Ok(response(&[("Yesterday",vec![2,5]),("Last week",vec![3])],Some("oldest"),false)),session_rejected:false,target:CatalogTarget::Page },cx);
                let headings = view.list_rows.iter().filter_map(|row| match row { crate::app::ListRow::Heading(index)=>Some(view.page.shelves[*index].title.as_str()),_=>None }).collect::<Vec<_>>();
                assert_eq!(headings,["Today","Yesterday","Last week"],"A continued day gets one heading");
                view.detail_list.state.scroll_to(ListOffset { item_ix:3,offset_in_item:px(0.) });
                view.revalidate_page(cx);
                let job = view.catalog_requests.pending.try_recv().unwrap();
                view.catalog_result(CatalogResult { id:job.id,account_revision:job.account_revision,account_generation:job.account_generation,history_generation:job.history_generation,append:false,revalidate:true,result:Ok(response(&[("Today",vec![2,0]),("Yesterday",vec![4])],Some("fresh"),false)),session_rejected:false,target:CatalogTarget::Page },cx);
                let display = view.list_rows.iter().map(|row| match row {
                    crate::app::ListRow::Heading(index)=>view.page.shelves[*index].title.clone(),
                    crate::app::ListRow::Item(index)=>view.rows[*index].id().to_owned(),
                    _=>panic!("Unexpected History row")
                }).collect::<Vec<_>>();
                assert_eq!(display,vec!["Today".into(),format!("{:011}",2),format!("{:011}",0),"Yesterday".into(),format!("{:011}",4),format!("{:011}",5),"Last week".into(),format!("{:011}",3)]);
                assert_eq!(view.detail_list.state.item_count(),8);
                view.page.title = "History".into();
                view.detail_list.state.scroll_to(ListOffset::default());
                cx.notify();
            }).unwrap();
            cx.background_executor().timer(Duration::from_millis(400)).await;
            window.update(cx,|_,window,_| { super::render::capture(window,"history-day-groups"); }).unwrap();
            println!("HISTORY_GROUPS continuation=passed replay_refresh=passed virtual_rows=8");
            result.store(true,Ordering::Release);
            println!("SYNC_SCREENSHOT home-scroll-preserved");
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
