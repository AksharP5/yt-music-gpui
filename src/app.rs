use crate::{
    artwork::Artwork,
    catalog::{Catalog, Collection, Item, Page, Shelf, Track},
    input::{InputEvent, SearchInput},
    library::{Library, LibraryWriter, Repeat, Store},
    player::{Player, PlayerEvent, PlayerOptions},
    theme::Theme,
};
use gpui::{prelude::*, *};
use std::{
    cell::Cell,
    path::PathBuf,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::Duration,
};

#[derive(Clone, PartialEq)]
enum Route {
    Home,
    Explore,
    Search(String),
    Browse(String),
    Liked,
    Recent,
    Saved,
}
#[derive(Clone)]
enum Query {
    Home,
    Explore,
    Search(String),
    Browse(String),
    More { token: String, search: bool },
}
struct Job {
    id: u64,
    query: Query,
}
struct CatalogResult {
    id: u64,
    append: bool,
    result: Result<Page, String>,
}
#[derive(Clone, Copy, PartialEq)]
enum Playback {
    Stopped,
    Loading,
    Playing,
    Paused,
    Buffering,
}
#[derive(Clone, Copy, PartialEq)]
enum Overlay {
    Help,
    Settings,
}
#[derive(Clone, Copy, PartialEq)]
enum Drag {
    Seek,
    Volume,
}

pub struct Music {
    theme: Theme,
    focus: FocusHandle,
    search: Entity<SearchInput>,
    route: Route,
    page: Page,
    rows: Vec<Item>,
    back: Vec<(Route, Page)>,
    list_id: usize,
    loading: bool,
    error: Option<String>,
    notice: Option<String>,
    generation: Arc<AtomicU64>,
    jobs: async_channel::Sender<Job>,
    pending_jobs: async_channel::Receiver<Job>,
    artwork: Artwork,
    library: Library,
    store: Option<LibraryWriter>,
    player: Option<Player>,
    active_id: Option<u64>,
    playback: Playback,
    position: f64,
    duration: f64,
    show_queue: bool,
    overlay: Option<Overlay>,
    drag: Option<Drag>,
    seek_bounds: Rc<Cell<Bounds<Pixels>>>,
    volume_bounds: Rc<Cell<Bounds<Pixels>>>,
    drag_position: Option<f64>,
    _subscriptions: Vec<Subscription>,
}

impl Music {
    pub fn new(cookies: Option<PathBuf>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(SearchInput::new);
        let (store, library, error) = match Store::load() {
            Ok((s, l)) => (Some(s), l, None),
            Err(e) => (None, Library::new(), Some(format!("{e:#}"))),
        };
        let store = store.map(|store| {
            let (writer, errors) = LibraryWriter::new(store);
            cx.spawn_in(window, async move |view, cx| {
                while let Ok(error) = errors.recv().await {
                    if view
                        .update_in(cx, |view, _, cx| {
                            view.error = Some(error);
                            cx.notify();
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .detach();
            writer
        });
        let (player, error) = match Player::new(PlayerOptions {
            cookies,
            volume: library.volume,
            ..Default::default()
        }) {
            Ok(p) => (Some(p), error),
            Err(e) => (None, Some(format!("{e:#}"))),
        };
        let (artwork, loaded) = Artwork::new();
        let (jobs, receiver) = async_channel::bounded::<Job>(1);
        let (results, completed) = async_channel::unbounded::<CatalogResult>();
        let generation = Arc::new(AtomicU64::new(0));
        for worker in 0..2 {
            let receiver = receiver.clone();
            let results = results.clone();
            let generation = generation.clone();
            thread::Builder::new()
                .name(format!("catalog-{worker}"))
                .spawn(move || {
                    let catalog = Catalog::new();
                    while let Ok(job) = receiver.recv_blocking() {
                        if generation.load(Ordering::Relaxed) != job.id {
                            continue;
                        }
                        let append = matches!(job.query, Query::More { .. });
                        let result = match &catalog {
                            Err(e) => Err(format!("{e:#}")),
                            Ok(catalog) => match &job.query {
                                Query::Home => catalog.home(),
                                Query::Explore => catalog.explore(),
                                Query::Search(q) => catalog.search(q),
                                Query::Browse(id) => catalog.browse(id),
                                Query::More { token, search } => catalog.more(token, *search),
                            }
                            .map_err(|e| format!("{e:#}")),
                        };
                        if results
                            .send_blocking(CatalogResult {
                                id: job.id,
                                append,
                                result,
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                })
                .expect("start catalog worker");
        }
        cx.spawn_in(window, async move |view, cx| {
            while let Ok(result) = completed.recv().await {
                if view
                    .update_in(cx, |view, _, cx| view.catalog_result(result, cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        cx.spawn_in(window, async move |view, cx| {
            while let Ok(loaded) = loaded.recv().await {
                if view
                    .update_in(cx, |view, window, cx| {
                        if let Some(old) = view.artwork.insert(loaded) {
                            cx.drop_image(old, Some(window));
                        }
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        if let Some(player) = &player {
            let events = player.events();
            cx.spawn_in(window, async move |view, cx| {
                while let Ok(event) = events.recv().await {
                    if view
                        .update_in(cx, |view, _, cx| view.player_event(event, cx))
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .detach();
        }
        cx.spawn_in(window, async move |view, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(2)).await;
                if view
                    .update_in(cx, |view, _, cx| {
                        let theme = Theme::load();
                        if theme != view.theme {
                            view.theme = theme;
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        let subscription =
            cx.subscribe_in(&search, window, |view, _, event, window, cx| match event {
                InputEvent::Submit => {
                    let query = view.search.read(cx).value().trim().to_owned();
                    if !query.is_empty() {
                        view.navigate(Route::Search(query), cx);
                        window.focus(&view.focus, cx);
                    }
                }
                InputEvent::Blur => window.focus(&view.focus, cx),
                InputEvent::Changed => {}
            });
        let mut view = Self {
            theme: Theme::load(),
            focus: cx.focus_handle(),
            search,
            route: Route::Home,
            page: Page {
                title: "Home".into(),
                shelves: Vec::new(),
                continuation: None,
            },
            rows: Vec::new(),
            back: Vec::new(),
            list_id: 0,
            loading: false,
            error,
            notice: None,
            generation,
            jobs,
            pending_jobs: receiver,
            artwork,
            library,
            store,
            player,
            active_id: None,
            playback: Playback::Stopped,
            position: 0.,
            duration: 0.,
            show_queue: false,
            overlay: None,
            drag: None,
            seek_bounds: Rc::new(Cell::new(Bounds::default())),
            volume_bounds: Rc::new(Cell::new(Bounds::default())),
            drag_position: None,
            _subscriptions: vec![subscription],
        };
        window.focus(&view.focus, cx);
        view.request(Query::Home, cx);
        view
    }

    fn request(&mut self, query: Query, cx: &mut Context<Self>) {
        let id = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        let _ = self.pending_jobs.try_recv();
        if self.jobs.try_send(Job { id, query }).is_err() {
            self.error = Some("The music connection stopped. Restart the app to reconnect.".into());
        } else {
            self.loading = true;
        }
        cx.notify();
    }

    fn navigate(&mut self, route: Route, cx: &mut Context<Self>) {
        if self.route != route {
            self.back.push((self.route.clone(), self.page.clone()));
            if self.back.len() > 12 {
                self.back.remove(0);
            }
        }
        self.list_id = self.list_id.wrapping_add(1);
        self.route = route.clone();
        self.error = None;
        self.notice = None;
        self.page.shelves.clear();
        self.page.continuation = None;
        self.rows.clear();
        match route {
            Route::Home => {
                self.page.title = "Home".into();
                self.request(Query::Home, cx);
            }
            Route::Explore => {
                self.page.title = "Explore".into();
                self.request(Query::Explore, cx);
            }
            Route::Search(query) => {
                self.page.title = format!("Results for \"{query}\"");
                self.search
                    .update(cx, |input, cx| input.set_value(query.clone(), cx));
                self.request(Query::Search(query), cx);
            }
            Route::Browse(id) => {
                self.page.title = "Loading collection".into();
                self.request(Query::Browse(id), cx);
            }
            _ => {
                self.generation.fetch_add(1, Ordering::Relaxed);
                self.loading = false;
                self.refresh_local();
                cx.notify();
            }
        }
    }

    fn refresh_local(&mut self) {
        let (title, rows) = match self.route {
            Route::Liked => (
                "Liked songs",
                self.library
                    .liked
                    .iter()
                    .cloned()
                    .map(Item::Track)
                    .collect(),
            ),
            Route::Recent => (
                "Recently played",
                self.library
                    .recent
                    .iter()
                    .cloned()
                    .map(Item::Track)
                    .collect(),
            ),
            Route::Saved => (
                "Saved collections",
                self.library
                    .saved
                    .iter()
                    .cloned()
                    .map(Item::Collection)
                    .collect(),
            ),
            _ => return,
        };
        self.page = Page {
            title: title.into(),
            shelves: vec![Shelf {
                title: String::new(),
                items: rows,
            }],
            continuation: None,
        };
        self.rows = self
            .page
            .shelves
            .iter()
            .flat_map(|s| s.items.clone())
            .collect();
    }

    fn go_back(&mut self, cx: &mut Context<Self>) {
        if let Some((route, page)) = self.back.pop() {
            self.generation.fetch_add(1, Ordering::Relaxed);
            self.list_id = self.list_id.wrapping_add(1);
            self.route = route;
            self.page = page;
            self.loading = false;
            self.error = None;
            self.rows = self
                .page
                .shelves
                .iter()
                .flat_map(|s| s.items.clone())
                .collect();
            self.refresh_local();
            cx.notify();
        }
    }

    fn catalog_result(&mut self, result: CatalogResult, cx: &mut Context<Self>) {
        if result.id != self.generation.load(Ordering::Relaxed) {
            return;
        }
        self.loading = false;
        match result.result {
            Ok(page) => {
                if result.append {
                    self.page.continuation = page.continuation;
                    self.page.shelves.extend(page.shelves);
                } else {
                    self.page = page;
                }
                self.rows = self
                    .page
                    .shelves
                    .iter()
                    .flat_map(|s| s.items.clone())
                    .collect();
            }
            Err(error) => self.error = Some(error),
        }
        cx.notify();
    }

    fn save(&mut self) {
        if let Some(store) = &self.store {
            store.save(&self.library);
        }
    }
    fn player_command(&mut self, command: impl FnOnce(&Player) -> anyhow::Result<()>) {
        if let Some(player) = &self.player
            && let Err(error) = command(player)
        {
            self.error = Some(error.to_string());
        }
    }
    fn play_current(&mut self, cx: &mut Context<Self>) {
        let Some(track) = self.library.queue.current().cloned() else {
            return;
        };
        let Some(player) = &self.player else {
            self.error =
                Some("The audio player is unavailable. Restart the app to try again.".into());
            cx.notify();
            return;
        };
        match player.play(&track.id, &format!("{} - {}", track.artist, track.title)) {
            Ok(id) => {
                self.active_id = Some(id);
                self.playback = Playback::Loading;
                self.position = 0.;
                self.duration = track.duration.unwrap_or(0) as f64;
                self.error = None;
            }
            Err(error) => self.error = Some(error.to_string()),
        }
        self.save();
        cx.notify();
    }
    fn play_item(&mut self, item: Item, cx: &mut Context<Self>) {
        match item {
            Item::Collection(collection) => self.navigate(Route::Browse(collection.id), cx),
            Item::Track(track) => {
                let tracks = self.page.tracks();
                if let Some(index) = tracks.iter().position(|t| t.id == track.id) {
                    self.library.queue.replace(tracks, index);
                } else {
                    self.library.queue.replace(vec![track], 0);
                }
                self.play_current(cx);
            }
        }
    }
    fn toggle_play(&mut self, cx: &mut Context<Self>) {
        if self.active_id.is_some() && self.playback != Playback::Stopped {
            self.player_command(Player::toggle_pause);
        } else {
            if self.library.queue.index.is_none() {
                self.library.queue.select(0);
            }
            self.play_current(cx);
        }
    }
    fn next(&mut self, natural: bool, cx: &mut Context<Self>) {
        if self.library.queue.advance(self.library.repeat, natural) {
            self.play_current(cx);
        } else {
            self.player_command(Player::stop);
            self.active_id = None;
            self.playback = Playback::Stopped;
            self.position = self.duration;
            cx.notify();
        }
    }
    fn previous(&mut self, cx: &mut Context<Self>) {
        if self.position > 3. {
            self.player_command(|p| p.seek(0.));
        } else if self.library.queue.previous() {
            self.play_current(cx);
        }
    }
    fn player_event(&mut self, event: PlayerEvent, cx: &mut Context<Self>) {
        let mut changed = true;
        match event {
            PlayerEvent::Volume(volume) => {
                changed = self.library.volume.round() != volume.round();
                self.library.volume = volume.clamp(0., 100.);
            }
            PlayerEvent::Error { track_id, message }
                if track_id.is_none() || track_id == self.active_id =>
            {
                self.error = Some(message);
                self.active_id = None;
                self.playback = Playback::Stopped;
            }
            PlayerEvent::Started { track_id } if Some(track_id) == self.active_id => {
                self.playback = Playback::Playing;
                if let Some(track) = self.library.queue.current().cloned() {
                    self.library.record_play(&track);
                    self.save();
                }
            }
            PlayerEvent::Loading { track_id } if Some(track_id) == self.active_id => {
                self.playback = Playback::Loading
            }
            PlayerEvent::Position { track_id, seconds } if Some(track_id) == self.active_id => {
                changed = self.position.floor() != seconds.floor();
                self.position = seconds;
            }
            PlayerEvent::Duration { track_id, seconds } if Some(track_id) == self.active_id => {
                self.duration = seconds
            }
            PlayerEvent::Paused { track_id, paused } if Some(track_id) == self.active_id => {
                self.playback = if paused {
                    Playback::Paused
                } else {
                    Playback::Playing
                }
            }
            PlayerEvent::Buffering {
                track_id,
                buffering,
            } if Some(track_id) == self.active_id => {
                if buffering {
                    self.playback = Playback::Buffering;
                } else if self.playback == Playback::Buffering {
                    self.playback = Playback::Playing;
                }
            }
            PlayerEvent::Ended { track_id } if Some(track_id) == self.active_id => {
                self.next(true, cx);
                return;
            }
            _ => changed = false,
        }
        if changed {
            cx.notify();
        }
    }
    fn toggle_like(&mut self, track: &Track, cx: &mut Context<Self>) {
        self.library.toggle_like(track);
        self.save();
        self.refresh_local();
        cx.notify();
    }
    fn enqueue(&mut self, track: Track, cx: &mut Context<Self>) {
        self.notice = Some(format!("Added {} to the queue", track.title));
        self.library.queue.tracks.push(track);
        self.save();
        cx.notify();
    }
    fn save_collection(&mut self, collection: Collection, cx: &mut Context<Self>) {
        if let Some(i) = self
            .library
            .saved
            .iter()
            .position(|c| c.id == collection.id)
        {
            self.library.saved.remove(i);
            self.notice = Some("Collection removed from saved music".into());
        } else {
            self.library.saved.insert(0, collection);
            self.notice = Some("Collection saved".into());
        }
        self.save();
        self.refresh_local();
        cx.notify();
    }
    fn set_volume(&mut self, volume: f64, cx: &mut Context<Self>) {
        let volume = volume.clamp(0., 100.).round();
        if self.library.volume != volume {
            self.library.volume = volume;
            self.player_command(|p| p.set_volume(volume));
            cx.notify();
        }
    }
    fn drag_to(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        match self.drag {
            Some(Drag::Seek) if self.duration > 0. => {
                let bounds = self.seek_bounds.get();
                self.drag_position = Some(
                    ((position.x - bounds.left()) / bounds.size.width).clamp(0., 1.) as f64
                        * self.duration,
                );
                cx.notify();
            }
            Some(Drag::Volume) => {
                let bounds = self.volume_bounds.get();
                self.set_volume(
                    ((position.x - bounds.left()) / bounds.size.width).clamp(0., 1.) as f64 * 100.,
                    cx,
                );
            }
            _ => {}
        }
    }
    fn end_drag(&mut self, cx: &mut Context<Self>) {
        if self.drag == Some(Drag::Seek)
            && let Some(seconds) = self.drag_position.take()
        {
            self.player_command(|p| p.seek(seconds));
        }
        if self.drag.take().is_some() {
            self.save();
            cx.notify();
        }
    }
    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        let control = event.keystroke.modifiers.control;
        if control && key == "k" {
            window.focus(&self.search.focus_handle(cx), cx);
            return;
        }
        if control && key == "q" {
            cx.quit();
            return;
        }
        if self.search.focus_handle(cx).is_focused(window) {
            return;
        }
        match key {
            "space" => self.toggle_play(cx),
            "right" if control => self.next(false, cx),
            "left" if control => self.previous(cx),
            "right" => {
                let seconds = (self.position + 5.).min(self.duration);
                self.player_command(|p| p.seek(seconds));
            }
            "left" => {
                let seconds = (self.position - 5.).max(0.);
                self.player_command(|p| p.seek(seconds));
            }
            "up" => {
                self.set_volume(self.library.volume + 5., cx);
                self.save();
            }
            "down" => {
                self.set_volume(self.library.volume - 5., cx);
                self.save();
            }
            "l" => {
                if let Some(track) = self.library.queue.current().cloned() {
                    self.toggle_like(&track, cx);
                }
            }
            "q" => {
                self.show_queue = !self.show_queue;
                cx.notify();
            }
            "escape" => {
                self.overlay = None;
                self.show_queue = false;
                cx.notify();
            }
            "?" => {
                self.overlay = Some(Overlay::Help);
                cx.notify();
            }
            "/" if event.keystroke.modifiers.shift => {
                self.overlay = Some(Overlay::Help);
                cx.notify();
            }
            _ => {}
        }
    }
    fn art(&mut self, url: Option<&str>, edge: f32) -> AnyElement {
        let image = url.and_then(|url| self.artwork.get(url));
        let mut element = div()
            .size(px(edge))
            .flex_shrink_0()
            .rounded(px(5.))
            .overflow_hidden()
            .bg(self.theme.raised);
        element = if let Some(image) = image {
            element.child(img(image).size_full().object_fit(ObjectFit::Cover))
        } else {
            element.flex().items_center().justify_center().child(
                icon("music", edge.min(32.) * 0.6, self.theme.muted).text_color(self.theme.muted),
            )
        };
        element.into_any_element()
    }
    fn nav(
        &self,
        id: &'static str,
        glyph: &'static str,
        label: &'static str,
        route: Route,
        compact: bool,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let active = self.route == route;
        div()
            .id(id)
            .h(px(44.))
            .px(px(13.))
            .flex()
            .items_center()
            .gap(px(14.))
            .rounded(px(7.))
            .text_color(if active {
                self.theme.text
            } else {
                self.theme.muted
            })
            .when(active, |d| d.bg(self.theme.selection))
            .hover(|d| d.bg(self.theme.raised).text_color(self.theme.text))
            .cursor_pointer()
            .tooltip(move |_, cx| {
                cx.new(|_| Hint {
                    text: label,
                    theme: Theme::load(),
                })
                .into()
            })
            .child(icon(
                glyph,
                21.,
                if active {
                    self.theme.text
                } else {
                    self.theme.muted
                },
            ))
            .when(!compact, |d| d.child(label))
            .on_click(cx.listener(move |view, _, window, cx| {
                window.focus(&view.focus, cx);
                view.navigate(route.clone(), cx);
            }))
    }
    fn sidebar(&self, compact: bool, cx: &Context<Self>) -> impl IntoElement {
        div()
            .w(px(if compact { 74. } else { 206. }))
            .h_full()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .px(px(12.))
            .py(px(24.))
            .gap(px(6.))
            .border_r_1()
            .border_color(self.theme.border)
            .bg(self.theme.panel)
            .child(
                div()
                    .h(px(45.))
                    .px(px(12.))
                    .mb(px(25.))
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .child(
                        div()
                            .size(px(29.))
                            .rounded_full()
                            .bg(rgb(0xff344e))
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(icon("play", 18., self.theme.text).text_color(rgb(0xffffff))),
                    )
                    .when(!compact, |d| {
                        d.child(
                            div()
                                .text_size(px(21.))
                                .font_weight(FontWeight::BOLD)
                                .child("Music"),
                        )
                    }),
            )
            .child(self.nav("home-nav", "home", "Home", Route::Home, compact, cx))
            .child(self.nav(
                "explore-nav",
                "explore",
                "Explore",
                Route::Explore,
                compact,
                cx,
            ))
            .child(
                div()
                    .h(px(1.))
                    .mx(px(12.))
                    .my(px(17.))
                    .bg(self.theme.border),
            )
            .child(self.nav(
                "liked-nav",
                "heart",
                "Liked songs",
                Route::Liked,
                compact,
                cx,
            ))
            .child(self.nav(
                "saved-nav",
                "library",
                "Saved collections",
                Route::Saved,
                compact,
                cx,
            ))
            .child(self.nav(
                "recent-nav",
                "history",
                "Recently played",
                Route::Recent,
                compact,
                cx,
            ))
            .child(div().flex_1())
            .child(
                div()
                    .id("shortcut-help")
                    .h(px(42.))
                    .px(px(13.))
                    .flex()
                    .items_center()
                    .gap(px(14.))
                    .text_color(self.theme.muted)
                    .cursor_pointer()
                    .hover(|d| d.text_color(self.theme.text))
                    .child(icon("help", 20., self.theme.text))
                    .when(!compact, |d| d.child("Keyboard shortcuts"))
                    .on_click(cx.listener(|view, _, _, cx| {
                        view.overlay = Some(Overlay::Help);
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .id("settings-nav")
                    .h(px(42.))
                    .px(px(13.))
                    .flex()
                    .items_center()
                    .gap(px(14.))
                    .text_color(self.theme.muted)
                    .cursor_pointer()
                    .hover(|d| d.text_color(self.theme.text))
                    .child(icon("settings", 20., self.theme.text))
                    .when(!compact, |d| d.child("Settings"))
                    .on_click(cx.listener(|view, _, _, cx| {
                        view.overlay = Some(Overlay::Settings);
                        cx.notify();
                    })),
            )
    }
    fn header(&self, cx: &Context<Self>) -> impl IntoElement {
        div()
            .h(px(80.))
            .px(px(30.))
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap(px(18.))
            .child(
                button("back", "back", self.theme)
                    .opacity(if self.back.is_empty() { 0.3 } else { 1. })
                    .on_click(cx.listener(|view, _, _, cx| view.go_back(cx))),
            )
            .child(
                div()
                    .id("search-box")
                    .max_w(px(670.))
                    .flex_1()
                    .h(px(46.))
                    .px(px(16.))
                    .rounded(px(8.))
                    .bg(self.theme.raised)
                    .border_1()
                    .border_color(self.theme.border)
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .on_click(cx.listener(|view, _, window, cx| {
                        window.focus(&view.search.focus_handle(cx), cx)
                    }))
                    .child(icon("search", 21., self.theme.text).text_color(self.theme.muted))
                    .child(div().flex_1().min_w_0().child(self.search.clone()))
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(self.theme.muted)
                            .child("Ctrl K"),
                    ),
            )
            .child(div().flex_1())
            .child(
                button("open-web", "external", self.theme)
                    .on_click(|_, _, cx| cx.open_url("https://music.youtube.com")),
            )
    }
    fn card(&mut self, item: Item, id: usize, cx: &Context<Self>) -> AnyElement {
        let art = self.art(item.thumbnail(), 164.);
        let title = item.title().to_owned();
        let subtitle = item.subtitle().to_owned();
        div()
            .id(("card", id))
            .w(px(164.))
            .flex_shrink_0()
            .flex()
            .flex_col()
            .gap(px(9.))
            .cursor_pointer()
            .child(div().rounded(px(5.)).overflow_hidden().child(art))
            .child(
                div()
                    .w_full()
                    .truncate()
                    .text_size(px(15.))
                    .font_weight(FontWeight::MEDIUM)
                    .child(title),
            )
            .child(
                div()
                    .w_full()
                    .truncate()
                    .text_size(px(12.))
                    .text_color(self.theme.muted)
                    .child(subtitle),
            )
            .on_click(cx.listener(move |view, _, window, cx| {
                window.focus(&view.focus, cx);
                view.play_item(item.clone(), cx);
            }))
            .into_any_element()
    }
    fn shelf(&mut self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let shelf = self.page.shelves[index].clone();
        div()
            .id(("shelf", index))
            .h(px(308.))
            .px(px(30.))
            .flex()
            .flex_col()
            .gap(px(17.))
            .child(
                div()
                    .text_size(px(23.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(shelf.title),
            )
            .child(
                div()
                    .id(("carousel", index))
                    .flex()
                    .gap(px(20.))
                    .overflow_x_scroll()
                    .pb(px(10.))
                    .children(
                        shelf
                            .items
                            .into_iter()
                            .enumerate()
                            .map(|(i, item)| self.card(item, index * 1000 + i, cx))
                            .collect::<Vec<_>>(),
                    ),
            )
            .into_any_element()
    }
    fn row(&mut self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let item = self.rows[index].clone();
        let active = self
            .library
            .queue
            .current()
            .is_some_and(|t| t.id == item.id())
            && self.active_id.is_some();
        let art = self.art(item.thumbnail(), 46.);
        let title = item.title().to_owned();
        let subtitle = item.subtitle().to_owned();
        let cloned = item.clone();
        let mut row = div()
            .id(("song", index))
            .w_full()
            .px(px(30.))
            .h(px(68.))
            .flex()
            .items_center()
            .gap(px(14.))
            .rounded(px(6.))
            .when(active, |d| d.bg(self.theme.selection))
            .hover(|d| d.bg(self.theme.raised))
            .cursor_pointer()
            .on_click(cx.listener(move |view, _, window, cx| {
                window.focus(&view.focus, cx);
                view.play_item(cloned.clone(), cx);
            }))
            .child(art)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(5.))
                    .child(
                        div()
                            .truncate()
                            .text_size(px(15.))
                            .when(active, |d| d.text_color(self.theme.accent))
                            .child(title),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_size(px(12.))
                            .text_color(self.theme.muted)
                            .child(subtitle),
                    ),
            );
        match item {
            Item::Track(track) => {
                let liked = self.library.liked.iter().any(|t| t.id == track.id);
                let like_track = track.clone();
                row = row
                    .child(
                        div()
                            .w(px(48.))
                            .text_size(px(12.))
                            .text_color(self.theme.muted)
                            .child(track.duration.map(|s| clock(s as f64)).unwrap_or_default()),
                    )
                    .child(
                        button(
                            ("like", index),
                            if liked { "heart-filled" } else { "heart" },
                            self.theme,
                        )
                        .when(liked, |d| d.text_color(self.theme.accent))
                        .on_click(cx.listener(move |view, _, _, cx| {
                            cx.stop_propagation();
                            view.toggle_like(&like_track, cx);
                        })),
                    )
                    .child(
                        button(("add", index), "plus", self.theme).on_click(cx.listener(
                            move |view, _, _, cx| {
                                cx.stop_propagation();
                                view.enqueue(track.clone(), cx);
                            },
                        )),
                    );
            }
            Item::Collection(collection) => {
                let saved = self.library.saved.iter().any(|c| c.id == collection.id);
                row = row
                    .child(div().text_size(px(12.)).text_color(self.theme.muted).child(
                        if collection.id.starts_with("UC") {
                            "Artist"
                        } else {
                            "Collection"
                        },
                    ))
                    .child(
                        button(
                            ("save", index),
                            if saved { "heart-filled" } else { "plus" },
                            self.theme,
                        )
                        .on_click(cx.listener(move |view, _, _, cx| {
                            cx.stop_propagation();
                            view.save_collection(collection.clone(), cx);
                        })),
                    )
                    .child(icon("forward", 17., self.theme.text).text_color(self.theme.muted));
            }
        }
        row.into_any_element()
    }
    fn content(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let home = matches!(self.route, Route::Home | Route::Explore);
        let has_tracks = self.rows.iter().any(|i| matches!(i, Item::Track(_)));
        let mut content = div()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .child(self.header(cx))
            .child(
                div()
                    .px(px(30.))
                    .pt(px(10.))
                    .pb(px(25.))
                    .flex()
                    .items_center()
                    .gap(px(16.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(31.))
                            .font_weight(FontWeight::BOLD)
                            .child(self.page.title.clone()),
                    )
                    .when(!home && has_tracks, |d| {
                        d.child(
                            div()
                                .id("play-all")
                                .h(px(36.))
                                .px(px(17.))
                                .rounded_full()
                                .bg(self.theme.text)
                                .text_color(self.theme.bg)
                                .flex()
                                .items_center()
                                .gap(px(8.))
                                .cursor_pointer()
                                .child(icon("play", 17., self.theme.text))
                                .child("Play all")
                                .on_click(cx.listener(|view, _, _, cx| {
                                    view.library.queue.replace(view.page.tracks(), 0);
                                    view.play_current(cx);
                                })),
                        )
                    }),
            );
        if let Some(error) = &self.error {
            content = content.child(
                div()
                    .mx(px(30.))
                    .mb(px(16.))
                    .p(px(14.))
                    .rounded(px(6.))
                    .bg(self.theme.raised)
                    .text_color(rgb(0xf1a6ae))
                    .text_size(px(13.))
                    .child(error.clone())
                    .child(
                        div()
                            .id("retry")
                            .mt(px(10.))
                            .text_color(self.theme.text)
                            .cursor_pointer()
                            .child("Dismiss")
                            .on_click(cx.listener(|view, _, _, cx| {
                                view.error = None;
                                cx.notify();
                            })),
                    ),
            );
        }
        if let Some(notice) = &self.notice {
            content = content.child(
                div()
                    .px(px(30.))
                    .pb(px(12.))
                    .text_size(px(12.))
                    .text_color(self.theme.accent)
                    .child(notice.clone()),
            );
        }
        if self.loading && self.rows.is_empty() {
            return content
                .child(
                    div()
                        .px(px(30.))
                        .py(px(25.))
                        .text_color(self.theme.muted)
                        .child("Loading music…"),
                )
                .into_any_element();
        }
        if self.rows.is_empty() {
            let message = match self.route {
                Route::Liked => "Tap the heart on a song to save it here.",
                Route::Recent => "Your recently played songs will appear here.",
                Route::Saved => "Save albums, artists, and playlists from search results.",
                Route::Search(_) => "No results. Try a different song or artist.",
                _ => "No music to show yet.",
            };
            return content
                .child(
                    div()
                        .px(px(30.))
                        .py(px(30.))
                        .text_color(self.theme.muted)
                        .child(message),
                )
                .into_any_element();
        }
        content = if home {
            content.child(
                uniform_list(
                    ("home-shelves", self.list_id),
                    self.page.shelves.len(),
                    cx.processor(|view, range: std::ops::Range<usize>, _, cx| {
                        range.map(|i| view.shelf(i, cx)).collect::<Vec<_>>()
                    }),
                )
                .flex_1()
                .min_h_0(),
            )
        } else {
            content.child(
                uniform_list(
                    ("music-results", self.list_id),
                    self.rows.len(),
                    cx.processor(|view, range: std::ops::Range<usize>, _, cx| {
                        range.map(|i| view.row(i, cx)).collect::<Vec<_>>()
                    }),
                )
                .flex_1()
                .min_h_0(),
            )
        };
        if self.page.continuation.is_some() {
            content = content.child(
                div()
                    .id("load-more")
                    .h(px(44.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(self.theme.muted)
                    .cursor_pointer()
                    .child(if self.loading {
                        "Loading…"
                    } else {
                        "Load more"
                    })
                    .on_click(cx.listener(|view, _, _, cx| {
                        if !view.loading
                            && let Some(token) = view.page.continuation.clone()
                        {
                            view.request(
                                Query::More {
                                    token,
                                    search: matches!(view.route, Route::Search(_)),
                                },
                                cx,
                            );
                        }
                    })),
            );
        }
        content.into_any_element()
    }
    fn queue(&mut self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .w(px(304.))
            .h_full()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .border_l_1()
            .border_color(self.theme.border)
            .bg(self.theme.panel)
            .child(
                div()
                    .h(px(80.))
                    .px(px(20.))
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(20.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Up next"),
                    )
                    .child(
                        button("close-queue", "close", self.theme).on_click(cx.listener(
                            |view, _, _, cx| {
                                view.show_queue = false;
                                cx.notify();
                            },
                        )),
                    ),
            )
            .child(
                div()
                    .px(px(20.))
                    .pb(px(18.))
                    .text_size(px(12.))
                    .text_color(self.theme.muted)
                    .child(match self.library.queue.tracks.len() {
                        1 => "1 song".to_owned(),
                        count => format!("{count} songs"),
                    }),
            )
            .child(
                uniform_list(
                    "queue-list",
                    self.library.queue.tracks.len(),
                    cx.processor(|view, range: std::ops::Range<usize>, _, cx| {
                        range
                            .map(|index| {
                                let track = view.library.queue.tracks[index].clone();
                                let active = view.library.queue.index == Some(index);
                                let art = view.art(track.thumbnail.as_deref(), 40.);
                                div()
                                    .id(("queue-row", index))
                                    .w_full()
                                    .h(px(64.))
                                    .px(px(18.))
                                    .rounded(px(5.))
                                    .flex()
                                    .items_center()
                                    .gap(px(10.))
                                    .cursor_pointer()
                                    .when(active, |d| d.bg(view.theme.selection))
                                    .hover(|d| d.bg(view.theme.raised))
                                    .on_click(cx.listener(move |view, _, _, cx| {
                                        view.library.queue.select(index);
                                        view.play_current(cx);
                                    }))
                                    .child(art)
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .flex()
                                            .flex_col()
                                            .gap(px(4.))
                                            .child(
                                                div()
                                                    .truncate()
                                                    .text_size(px(13.))
                                                    .when(active, |d| {
                                                        d.text_color(view.theme.accent)
                                                    })
                                                    .child(track.title),
                                            )
                                            .child(
                                                div()
                                                    .truncate()
                                                    .text_size(px(11.))
                                                    .text_color(view.theme.muted)
                                                    .child(track.artist),
                                            ),
                                    )
                                    .child(
                                        button(("remove-queue", index), "close", view.theme)
                                            .w(px(26.))
                                            .on_click(cx.listener(move |view, _, _, cx| {
                                                cx.stop_propagation();
                                                let playing = matches!(
                                                    view.playback,
                                                    Playback::Playing
                                                        | Playback::Buffering
                                                        | Playback::Loading
                                                );
                                                let current = view.library.queue.remove(index);
                                                if current {
                                                    view.player_command(Player::stop);
                                                    view.active_id = None;
                                                    view.playback = Playback::Stopped;
                                                    view.position = 0.;
                                                    view.duration = 0.;
                                                    if playing {
                                                        view.play_current(cx);
                                                    }
                                                }
                                                view.save();
                                                cx.notify();
                                            })),
                                    )
                                    .into_any_element()
                            })
                            .collect::<Vec<_>>()
                    }),
                )
                .flex_1()
                .min_h_0(),
            )
            .when(self.library.queue.tracks.is_empty(), |d| {
                d.child(
                    div()
                        .px(px(20.))
                        .text_color(self.theme.muted)
                        .text_size(px(13.))
                        .child("Play a song or add one with +."),
                )
            })
            .into_any_element()
    }
    fn player_bar(&mut self, compact: bool, cx: &mut Context<Self>) -> AnyElement {
        let current = self.library.queue.current().cloned();
        let mut details = div()
            .w(px(if compact { 230. } else { 310. }))
            .min_w_0()
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap(px(12.));
        if let Some(track) = current {
            let art = self.art(track.thumbnail.as_deref(), 52.);
            let liked = self.library.liked.iter().any(|t| t.id == track.id);
            let subtitle = match self.playback {
                Playback::Loading => "Loading track…".into(),
                Playback::Buffering => "Buffering…".into(),
                _ => track.artist.clone(),
            };
            details = details
                .child(art)
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap(px(6.))
                        .child(
                            div()
                                .truncate()
                                .text_size(px(14.))
                                .font_weight(FontWeight::MEDIUM)
                                .child(track.title.clone()),
                        )
                        .child(
                            div()
                                .truncate()
                                .text_size(px(12.))
                                .text_color(self.theme.muted)
                                .child(subtitle),
                        ),
                )
                .child(
                    button(
                        "like-current",
                        if liked { "heart-filled" } else { "heart" },
                        self.theme,
                    )
                    .when(liked, |d| d.text_color(self.theme.accent))
                    .on_click(cx.listener(move |view, _, _, cx| view.toggle_like(&track, cx))),
                );
        } else {
            details = details.child(
                div()
                    .text_color(self.theme.muted)
                    .text_size(px(13.))
                    .child("Choose something to play"),
            );
        }
        let progress = if self.duration > 0. {
            (self.drag_position.unwrap_or(self.position) / self.duration).clamp(0., 1.) as f32
        } else {
            0.
        };
        let seek_bounds = self.seek_bounds.clone();
        let volume_bounds = self.volume_bounds.clone();
        let playing = matches!(self.playback, Playback::Playing | Playback::Buffering);
        div()
            .h(px(106.))
            .flex_shrink_0()
            .px(px(24.))
            .border_t_1()
            .border_color(self.theme.border)
            .bg(self.theme.panel)
            .flex()
            .items_center()
            .gap(px(24.))
            .child(details)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .max_w(px(650.))
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(14.))
                            .child(
                                button("shuffle", "shuffle", self.theme).on_click(cx.listener(
                                    |view, _, _, cx| {
                                        view.library.queue.shuffle_upcoming();
                                        view.notice = Some("Upcoming songs shuffled".into());
                                        view.save();
                                        cx.notify();
                                    },
                                )),
                            )
                            .child(
                                button("previous", "previous", self.theme)
                                    .on_click(cx.listener(|view, _, _, cx| view.previous(cx))),
                            )
                            .child(
                                button(
                                    "play-pause",
                                    if playing { "pause" } else { "play" },
                                    self.theme,
                                )
                                .size(px(40.))
                                .rounded_full()
                                .bg(self.theme.text)
                                .text_color(self.theme.bg)
                                .on_click(cx.listener(|view, _, _, cx| view.toggle_play(cx))),
                            )
                            .child(
                                button("next", "next", self.theme)
                                    .on_click(cx.listener(|view, _, _, cx| view.next(false, cx))),
                            )
                            .child(
                                button("repeat", "repeat", self.theme)
                                    .when(self.library.repeat != Repeat::Off, |d| {
                                        d.text_color(self.theme.accent)
                                    })
                                    .when(self.library.repeat == Repeat::One, |d| {
                                        d.child(
                                            div()
                                                .absolute()
                                                .top_0()
                                                .right_0()
                                                .text_size(px(9.))
                                                .child("1"),
                                        )
                                    })
                                    .on_click(cx.listener(|view, _, _, cx| {
                                        view.library.repeat = view.library.repeat.next();
                                        view.save();
                                        cx.notify();
                                    })),
                            ),
                    )
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .items_center()
                            .gap(px(11.))
                            .text_size(px(11.))
                            .text_color(self.theme.muted)
                            .child(
                                div()
                                    .w(px(38.))
                                    .text_right()
                                    .child(clock(self.drag_position.unwrap_or(self.position))),
                            )
                            .child(
                                div()
                                    .id("seek-track")
                                    .h(px(16.))
                                    .flex_1()
                                    .cursor_pointer()
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(|view, event: &MouseDownEvent, _, cx| {
                                            view.drag = Some(Drag::Seek);
                                            view.drag_to(event.position, cx);
                                        }),
                                    )
                                    .child(slider(progress, seek_bounds, self.theme)),
                            )
                            .child(div().w(px(38.)).child(clock(self.duration))),
                    ),
            )
            .child(div().flex_1().max_w(px(30.)))
            .child(
                div()
                    .w(px(if compact { 158. } else { 208. }))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .child(
                        button("queue-toggle", "queue", self.theme)
                            .when(self.show_queue, |d| d.text_color(self.theme.accent))
                            .on_click(cx.listener(|view, _, _, cx| {
                                view.show_queue = !view.show_queue;
                                cx.notify();
                            })),
                    )
                    .child(
                        button(
                            "mute-toggle",
                            if self.library.volume == 0. {
                                "mute"
                            } else {
                                "volume"
                            },
                            self.theme,
                        )
                        .on_click(cx.listener(|view, _, _, cx| {
                            view.set_volume(if view.library.volume > 0. { 0. } else { 60. }, cx);
                            view.save();
                        })),
                    )
                    .child(
                        div()
                            .id("volume-track")
                            .flex_1()
                            .h(px(18.))
                            .cursor_pointer()
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|view, event: &MouseDownEvent, _, cx| {
                                    view.drag = Some(Drag::Volume);
                                    view.drag_to(event.position, cx);
                                }),
                            )
                            .child(slider(
                                (self.library.volume / 100.) as f32,
                                volume_bounds,
                                self.theme,
                            )),
                    ),
            )
            .into_any_element()
    }
    fn overlay(&self, kind: Overlay, cx: &Context<Self>) -> AnyElement {
        let mut body = div()
            .w(px(480.))
            .p(px(28.))
            .rounded(px(12.))
            .bg(self.theme.panel)
            .border_1()
            .border_color(self.theme.border)
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .mb(px(24.))
                    .child(
                        div()
                            .text_size(px(23.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(if kind == Overlay::Help {
                                "Keyboard shortcuts"
                            } else {
                                "Settings"
                            }),
                    )
                    .child(
                        button("close-overlay", "close", self.theme).on_click(cx.listener(
                            |view, _, _, cx| {
                                view.overlay = None;
                                cx.notify();
                            },
                        )),
                    ),
            );
        if kind == Overlay::Help {
            for (key, action) in [
                ("Ctrl K", "Search"),
                ("Space", "Play or pause"),
                ("Ctrl → / Ctrl ←", "Next / previous song"),
                ("→ / ←", "Seek 5 seconds"),
                ("↑ / ↓", "Adjust volume"),
                ("L", "Like the current song"),
                ("Q", "Show the queue"),
                ("?", "Keyboard shortcuts"),
                ("Ctrl Q", "Quit"),
            ] {
                body = body.child(
                    div()
                        .py(px(9.))
                        .flex()
                        .justify_between()
                        .text_size(px(14.))
                        .child(action)
                        .child(div().text_color(self.theme.muted).child(key)),
                );
            }
        } else {
            body=body.child(div().text_size(px(15.)).child("YouTube Music for your desktop"))
                .child(div().mt(px(12.)).text_size(px(13.)).text_color(self.theme.muted).child("Your liked songs, saved collections, and queue are stored on this computer. The colors follow your Omarchy theme."))
                .child(div().id("settings-open-web").mt(px(24.)).text_color(self.theme.accent).cursor_pointer().child("Open YouTube Music in your browser").on_click(|_,_,cx|cx.open_url("https://music.youtube.com")))
                .child(div().mt(px(24.)).text_size(px(12.)).text_color(self.theme.muted).child("Version 0.1.0"));
        }
        div()
            .absolute()
            .inset_0()
            .bg(gpui::black().opacity(0.65))
            .flex()
            .items_center()
            .justify_center()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|view, _, _, cx| {
                    view.overlay = None;
                    cx.notify();
                }),
            )
            .child(body)
            .into_any_element()
    }
}

impl Render for Music {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let compact = window.viewport_size().width < px(1120.);
        let sidebar = self.sidebar(compact, cx).into_any_element();
        let content = self.content(cx);
        let queue = self.show_queue.then(|| self.queue(cx));
        let player = self.player_bar(compact, cx);
        div()
            .size_full()
            .relative()
            .bg(self.theme.bg)
            .text_color(self.theme.text)
            .text_size(px(14.))
            .font_family("Inter")
            .flex()
            .flex_col()
            .key_context("Music")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::key_down))
            .on_mouse_move(cx.listener(|view, event: &MouseMoveEvent, _, cx| {
                if view.drag.is_some() {
                    view.drag_to(event.position, cx);
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|view, _, _, cx| view.end_drag(cx)),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|view, _, _, cx| view.end_drag(cx)),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .child(sidebar)
                    .child(content)
                    .children(queue),
            )
            .child(player)
            .children(self.overlay.map(|kind| self.overlay(kind, cx)))
    }
}
fn icon(name: &'static str, edge: f32, color: Hsla) -> Svg {
    svg().path(name).size(px(edge)).text_color(color)
}
struct Hint {
    text: &'static str,
    theme: Theme,
}
impl Render for Hint {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .px(px(10.))
            .py(px(7.))
            .rounded(px(5.))
            .bg(self.theme.raised)
            .text_color(self.theme.text)
            .text_size(px(12.))
            .child(self.text)
    }
}
fn button(id: impl Into<ElementId>, name: &'static str, theme: Theme) -> Stateful<Div> {
    let label = match name {
        "play" => "Play",
        "pause" => "Pause",
        "previous" => "Previous song",
        "next" => "Next song",
        "shuffle" => "Shuffle upcoming songs",
        "repeat" => "Cycle repeat: off, all, one",
        "heart" => "Like song",
        "heart-filled" => "Remove from liked music",
        "queue" => "Show queue",
        "volume" => "Mute",
        "mute" => "Unmute",
        "plus" => "Add",
        "back" => "Go back",
        "help" => "Keyboard shortcuts",
        "settings" => "Settings",
        "close" => "Close or remove",
        _ => "Open",
    };
    div()
        .id(id)
        .relative()
        .size(px(34.))
        .flex_shrink_0()
        .rounded(px(6.))
        .flex()
        .items_center()
        .justify_center()
        .text_color(theme.muted)
        .cursor_pointer()
        .tooltip(move |_, cx| cx.new(|_| Hint { text: label, theme }).into())
        .hover(move |d| {
            let background = if matches!(name, "play" | "pause") {
                theme.text
            } else {
                theme.raised
            };
            d.bg(background)
        })
        .child(icon(
            name,
            19.,
            if matches!(name, "play" | "pause") {
                theme.bg
            } else if name == "heart-filled" {
                theme.accent
            } else {
                theme.text
            },
        ))
}
fn slider(ratio: f32, bounds: Rc<Cell<Bounds<Pixels>>>, theme: Theme) -> impl IntoElement {
    canvas(
        move |rect, _, _| {
            bounds.set(rect);
            rect
        },
        move |rect, _, window, _| {
            let y = rect.top() + rect.size.height / 2. - px(2.);
            window.paint_quad(fill(
                Bounds::new(point(rect.left(), y), size(rect.size.width, px(3.))),
                theme.border,
            ));
            window.paint_quad(fill(
                Bounds::new(point(rect.left(), y), size(rect.size.width * ratio, px(3.))),
                theme.accent,
            ));
            window.paint_quad(fill(
                Bounds::new(
                    point(rect.left() + rect.size.width * ratio - px(3.), y - px(1.5)),
                    size(px(6.), px(6.)),
                ),
                theme.text,
            ));
        },
    )
    .size_full()
}
fn clock(seconds: f64) -> String {
    let seconds = seconds.max(0.) as u64;
    if seconds >= 3600 {
        format!(
            "{}:{:02}:{:02}",
            seconds / 3600,
            (seconds / 60) % 60,
            seconds % 60
        )
    } else {
        format!("{}:{:02}", seconds / 60, seconds % 60)
    }
}
