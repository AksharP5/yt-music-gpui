//! The player page: artwork or video beside the Up next, Lyrics and Related tabs.

use super::*;
use queue_panel::Reorder;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::app) enum PlayerTab {
    UpNext,
    Lyrics,
    Related,
}

impl PlayerTab {
    const ALL: [Self; 3] = [Self::UpNext, Self::Lyrics, Self::Related];

    fn label(self) -> &'static str {
        match self {
            Self::UpNext => "UP NEXT",
            Self::Lyrics => "LYRICS",
            Self::Related => "RELATED",
        }
    }
}

/// Tab details for the browse ID YouTube Music issued with the current song.
struct Details {
    id: String,
    /// The account session the request was made with; another account must not reuse it.
    session: Option<Arc<Session>>,
    request: u64,
    result: Option<Result<SongText, String>>,
}

pub(in crate::app) struct PlayerPage {
    tab: PlayerTab,
    worker: song_text::Worker,
    latest_request: u64,
    lyrics: Option<Details>,
    related: Option<Details>,
    related_scroll: Vec<ScrollHandle>,
    pub(in crate::app) queue_scroll: UniformListScrollHandle,
    pub(in crate::app) queue_bounds: Rc<Cell<Bounds<Pixels>>>,
    pub(in crate::app) reorder: Option<Reorder>,
    pub(in crate::app) hovered_row: Option<usize>,
    pub(in crate::app) video_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
}

impl PlayerPage {
    pub(in crate::app) fn start(window: &mut Window, cx: &mut Context<Music>) -> Self {
        let (worker, events) = song_text::Worker::start();
        cx.spawn_in(window, async move |view, cx| {
            while let Ok(event) = events.recv().await {
                if view
                    .update_in(cx, |view, _, cx| view.player_details_loaded(event, cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        Self::new(worker)
    }

    fn new(worker: song_text::Worker) -> Self {
        Self {
            tab: PlayerTab::UpNext,
            worker,
            latest_request: 0,
            lyrics: None,
            related: None,
            related_scroll: Vec::new(),
            queue_scroll: UniformListScrollHandle::new(),
            queue_bounds: Rc::new(Cell::new(Bounds::default())),
            reorder: None,
            hovered_row: None,
            video_bounds: Rc::new(Cell::new(None)),
        }
    }

    /// Store a finished request when it is still wanted by `session`'s account, returning
    /// whether YouTube Music rejected that session.
    fn complete(
        &mut self,
        event: song_text::Event,
        session: Option<&Arc<Session>>,
    ) -> Option<bool> {
        let slot = [&mut self.lyrics, &mut self.related]
            .into_iter()
            .flatten()
            .find(|slot| slot.request == event.id && slot.result.is_none())?;
        if !same_session(slot.session.as_ref(), session) {
            return None;
        }
        if let Ok(SongText::Related(related)) = &event.result {
            self.related_scroll = related
                .shelves
                .iter()
                .map(|_| ScrollHandle::new())
                .collect();
        }
        slot.result = Some(event.result);
        Some(event.session_rejected)
    }

    /// Drop Lyrics and Related loaded or loading for an account that is no longer current.
    pub(in crate::app) fn reset_account(&mut self) {
        self.worker.cancel();
        self.lyrics = None;
        self.related = None;
        self.related_scroll.clear();
    }

    fn details(&mut self, tab: PlayerTab) -> Option<&mut Option<Details>> {
        match tab {
            PlayerTab::UpNext => None,
            PlayerTab::Lyrics => Some(&mut self.lyrics),
            PlayerTab::Related => Some(&mut self.related),
        }
    }
}

/// Layout constants from music.youtube.com's player page breakpoints.
struct Spacing {
    vertical: f32,
    horizontal: f32,
    gap: f32,
    side: f32,
}

impl Spacing {
    fn for_width(width: f32) -> Self {
        let (vertical, horizontal, gap, side) = match width {
            ..936. => (16., 32., 32., 1.),
            ..1150. => (24., 48., 48., 0.4),
            ..1364. => (32., 56., 56., 0.36),
            ..1578. => (40., 56., 56., 0.36),
            ..1800. => (48., 64., 64., 0.36),
            _ => (64., 96., 96., 0.36),
        };
        Self {
            vertical,
            horizontal,
            gap,
            side,
        }
    }
}

const TOGGLE_HEIGHT: f32 = 56.;

impl Music {
    pub(in crate::app) fn toggle_player_page(&mut self, cx: &mut Context<Self>) {
        if self.player_open {
            self.player_open = false;
        } else if self.library.queue.current().is_some() {
            self.player_open = true;
            if let Some(index) = self.library.queue.index {
                self.player_page
                    .queue_scroll
                    .scroll_to_item(index, ScrollStrategy::Top);
            }
        }
        cx.notify();
    }

    pub(in crate::app) fn show_player_tab(&mut self, tab: PlayerTab, cx: &mut Context<Self>) {
        self.overlay = None;
        self.player_open = self.library.queue.current().is_some();
        self.player_page.tab = tab;
        cx.notify();
    }

    /// Album pages list tracks without artwork, so fall back to the watch response's.
    pub(in crate::app) fn current_thumbnail(&self) -> Option<String> {
        let current = self.library.queue.current()?;
        current
            .thumbnail
            .clone()
            .or_else(|| self.current_watch()?.current.as_ref()?.thumbnail.clone())
    }

    /// The watch details for the current song, once they have loaded.
    fn current_watch(&self) -> Option<&Watch> {
        let current = self.library.queue.current()?;
        self.now_playing
            .as_ref()
            .filter(|watch| watch.current.as_ref().is_some_and(|w| w.id == current.id))
    }

    fn tab_source(&self, tab: PlayerTab) -> Option<String> {
        let watch = self.current_watch()?;
        match tab {
            PlayerTab::UpNext => None,
            PlayerTab::Lyrics => watch.lyrics.clone(),
            PlayerTab::Related => watch.related.clone(),
        }
    }

    /// Request the visible tab's details when the song changes or a request was superseded.
    fn load_visible_tab(&mut self) {
        let tab = self.player_page.tab;
        let Some(id) = self.tab_source(tab) else {
            return;
        };
        let session = self.account.as_ref().map(|account| account.session.clone());
        let latest = self.player_page.latest_request;
        let current = self.player_page.details(tab).and_then(|slot| slot.as_ref());
        if current.is_some_and(|loaded| {
            loaded.id == id
                && same_session(loaded.session.as_ref(), session.as_ref())
                && (loaded.result.is_some() || loaded.request == latest)
        }) {
            return;
        }
        let query = match tab {
            PlayerTab::Lyrics => TextQuery::Lyrics(id.clone()),
            PlayerTab::Related => TextQuery::Related(id.clone()),
            PlayerTab::UpNext => return,
        };
        let details = match self.player_page.worker.load(query, session.clone()) {
            Ok(request) => {
                self.player_page.latest_request = request;
                Details {
                    id,
                    session,
                    request,
                    result: None,
                }
            }
            Err(error) => Details {
                id,
                session,
                request: 0,
                result: Some(Err(format!("{error:#}"))),
            },
        };
        if let Some(slot) = self.player_page.details(tab) {
            *slot = Some(details);
        }
    }

    fn player_details_loaded(&mut self, event: song_text::Event, cx: &mut Context<Self>) {
        let session = self.account.as_ref().map(|account| &account.session);
        let Some(rejected) = self.player_page.complete(event, session) else {
            return;
        };
        if rejected {
            self.refresh_session(false, cx);
        }
        cx.notify();
    }

    pub(in crate::app) fn player_page(
        &mut self,
        width: f32,
        sidebar: f32,
        height: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let video = self.library.video;
        if video && window.is_fullscreen() {
            return self
                .video_viewport(window, cx)
                .size_full()
                .into_any_element();
        }
        self.load_visible_tab();
        let spacing = Spacing::for_width(width);
        let stacked = width < 936.;
        let inner = width - sidebar - spacing.horizontal * 2.;
        let side = if stacked { inner } else { inner * spacing.side };
        let main_width = if stacked {
            inner
        } else {
            inner - side - spacing.gap
        };
        let main_height = if stacked {
            (width * 0.5625).min(height * 0.5)
        } else {
            height - spacing.vertical * 2.
        };
        let media_height = main_height - TOGGLE_HEIGHT;
        let media = if video {
            let video_width = main_width.min(media_height * 16. / 9.).max(0.);
            self.video_viewport(window, cx)
                .w(px(video_width))
                .h(px(video_width * 9. / 16.))
                .flex_shrink_0()
                .into_any_element()
        } else {
            let edge = main_width.min(media_height).clamp(0., 800.);
            self.player_artwork(edge)
        };
        let main = div()
            .flex_shrink_0()
            .w(px(main_width))
            .h(px(main_height))
            .when(!stacked, |main| main.mb(px(spacing.vertical)))
            .flex()
            .flex_col()
            .items_center()
            .child(self.media_toggle(cx))
            .when_some(
                self.error
                    .clone()
                    .filter(|_| !matches!(self.overlay, Some(Overlay::SignIn))),
                |main, error| {
                    main.child(
                        div()
                            .w_full()
                            .pb(px(12.))
                            .flex()
                            .items_center()
                            .gap(px(16.))
                            .text_size(px(14.))
                            .text_color(rgb(0xf1a6ae))
                            .child(div().flex_1().child(error))
                            .child(
                                button("dismiss-player-error", "close", self.theme).on_click(
                                    cx.listener(|view, _, _, cx| {
                                        view.error = None;
                                        cx.notify();
                                    }),
                                ),
                            ),
                    )
                },
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(media),
            );
        let tabs = self.player_tabs(cx);
        let body = match self.player_page.tab {
            PlayerTab::UpNext => self.up_next(cx),
            PlayerTab::Lyrics => self.lyrics_tab(),
            PlayerTab::Related => self.related_tab(side, cx),
        };
        let side_panel = div()
            .w(px(side))
            .flex_shrink_0()
            .min_h_0()
            .when(stacked, |side| side.flex_1())
            .when(!stacked, |side| side.h_full().ml(px(spacing.gap)))
            .flex()
            .flex_col()
            .child(tabs)
            .child(div().flex_1().min_h_0().flex().flex_col().child(body));
        let hole = self.player_page.video_bounds.clone();
        let background = self.theme.bg;
        div()
            .id("player-page")
            .flex_1()
            .min_w_0()
            .h_full()
            .relative()
            .pt(px(spacing.vertical))
            .px(px(spacing.horizontal))
            .flex()
            .when(stacked, |page| page.flex_col())
            .when(video, |page| {
                // Native video shows through a transparent window, so paint the page around it.
                page.child(
                    canvas(
                        |_, _, _| {},
                        move |bounds, _, window, _| {
                            paint_around(bounds, hole.get(), background, window)
                        },
                    )
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full(),
                )
            })
            .when(!video, |page| page.bg(background))
            .child(main)
            .child(side_panel)
            .into_any_element()
    }

    fn player_artwork(&mut self, edge: f32) -> AnyElement {
        self.player_page.video_bounds.set(None);
        let image = self
            .current_thumbnail()
            .map(|url| large_artwork(&url))
            .and_then(|url| self.artwork.get_hero(&url));
        div()
            .size(px(edge))
            .flex_shrink_0()
            .rounded(px(4.))
            .overflow_hidden()
            .bg(self.theme.raised)
            .flex()
            .items_center()
            .justify_center()
            .map(|art| match image {
                Some(image) => art.child(img(image).size_full().object_fit(ObjectFit::Cover)),
                None => art.child(icon("music", 64., self.theme.muted)),
            })
            .into_any_element()
    }

    fn media_toggle(&self, cx: &mut Context<Self>) -> AnyElement {
        let episode = self
            .library
            .queue
            .current()
            .is_some_and(|track| track.details.episode.is_some());
        let video = self.library.video;
        let theme = self.theme;
        let option = move |id: &'static str, label: &'static str, selected: bool| {
            div()
                .id(id)
                .h(px(28.))
                .px(px(16.))
                .rounded_full()
                .flex()
                .items_center()
                .text_size(px(14.))
                .font_weight(FontWeight::MEDIUM)
                .cursor_pointer()
                .when(selected, |option| option.bg(gpui::white().opacity(0.2)))
                .when(!selected, |option| option.text_color(theme.muted))
                .child(label)
        };
        div()
            .h(px(TOGGLE_HEIGHT))
            .flex()
            .items_start()
            .child(
                div()
                    .p(px(4.))
                    .rounded_full()
                    .bg(theme.panel)
                    .flex()
                    .child(
                        option("song-mode", if episode { "Audio" } else { "Song" }, !video)
                            .on_click(cx.listener(|view, _, window, cx| {
                                window.focus(&view.focus, cx);
                                if view.library.video {
                                    view.set_video_mode(false, cx);
                                }
                            })),
                    )
                    .child(option("video-mode", "Video", video).on_click(cx.listener(
                        |view, _, window, cx| {
                            window.focus(&view.focus, cx);
                            if !view.library.video {
                                view.set_video_mode(true, cx);
                            }
                        },
                    ))),
            )
            .into_any_element()
    }

    fn player_tabs(&self, cx: &mut Context<Self>) -> AnyElement {
        let selected = self.player_page.tab;
        let watch = self.current_watch();
        let available = |tab: PlayerTab| match tab {
            PlayerTab::UpNext => true,
            PlayerTab::Lyrics => watch.is_some_and(|watch| watch.lyrics.is_some()),
            PlayerTab::Related => watch.is_some_and(|watch| watch.related.is_some()),
        };
        div()
            .h(px(49.))
            .flex_shrink_0()
            .flex()
            .border_b_1()
            .border_color(gpui::white().opacity(0.1))
            .children(PlayerTab::ALL.into_iter().map(|tab| {
                let active = tab == selected;
                let enabled = available(tab);
                div()
                    .id(tab.label())
                    .flex_auto()
                    .h(px(48.))
                    .px(px(12.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(14.))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(if active {
                        self.theme.text
                    } else if enabled {
                        self.theme.muted
                    } else {
                        gpui::white().opacity(0.3)
                    })
                    .when(active, |tab| tab.border_b_2().border_color(self.theme.text))
                    .when(enabled, |tab| tab.cursor_pointer())
                    .child(tab.label())
                    .on_click(cx.listener(move |view, _, _, cx| {
                        if enabled && view.player_page.tab != tab {
                            view.player_page.tab = tab;
                            cx.notify();
                        }
                    }))
            }))
            .into_any_element()
    }

    fn lyrics_tab(&self) -> AnyElement {
        let loaded = self
            .player_page
            .lyrics
            .as_ref()
            .filter(|loaded| self.tab_source(PlayerTab::Lyrics).as_ref() == Some(&loaded.id));
        let message = |text: &str| {
            div()
                .pt(px(24.))
                .text_size(px(16.))
                .text_color(self.theme.muted)
                .child(text.to_owned())
                .into_any_element()
        };
        let lyrics = match loaded.and_then(|loaded| loaded.result.as_ref()) {
            Some(Ok(SongText::Lyrics(lyrics))) => lyrics,
            Some(Err(error)) => return message(error),
            Some(Ok(_)) => return message("Lyrics are not available for this song"),
            None if self.current_watch().is_some() && loaded.is_none() => {
                return message("Lyrics are not available for this song");
            }
            None => return message("Loading…"),
        };
        div()
            .id(SharedString::from(format!(
                "lyrics-{}",
                loaded.map_or("", |l| &l.id)
            )))
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .pt(px(24.))
            .pb(px(32.))
            .child(
                div()
                    .text_size(px(16.))
                    .line_height(px(19.2))
                    .text_color(gpui::white().opacity(0.7))
                    .child(lyrics.text.clone()),
            )
            .when_some(lyrics.source.clone(), |body, source| {
                body.child(
                    div()
                        .pt(px(28.))
                        .text_size(px(14.))
                        .text_color(self.theme.muted)
                        .child(source),
                )
            })
            .into_any_element()
    }

    fn related_tab(&mut self, width: f32, cx: &mut Context<Self>) -> AnyElement {
        let loaded = self
            .player_page
            .related
            .as_ref()
            .filter(|loaded| self.tab_source(PlayerTab::Related).as_ref() == Some(&loaded.id));
        let page = match loaded.and_then(|loaded| loaded.result.as_ref()) {
            Some(Ok(SongText::Related(page))) => page.clone(),
            Some(Err(error)) => return related_message(error.clone(), self.theme),
            Some(Ok(_)) => return related_message("Nothing related yet".into(), self.theme),
            None => return related_message("Loading…".into(), self.theme),
        };
        let id = loaded.map_or(String::new(), |loaded| loaded.id.clone());
        let shelves = page
            .shelves
            .iter()
            .enumerate()
            .filter(|(_, shelf)| !shelf.items.is_empty())
            .map(|(index, shelf)| self.related_shelf(index, shelf, width, cx))
            .collect::<Vec<_>>();
        div()
            .id(SharedString::from(format!("related-{id}")))
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .pb(px(32.))
            .children(shelves)
            .into_any_element()
    }

    fn related_shelf(
        &mut self,
        index: usize,
        shelf: &Shelf,
        width: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let scroll = self
            .player_page
            .related_scroll
            .get(index)
            .cloned()
            .unwrap_or_default();
        let songs = shelf.layout == ShelfLayout::Songs;
        let column = if songs { width - 56. } else { 160. };
        let gap = if songs { 8. } else { 24. };
        let stride = column + gap;
        let columns = if songs {
            shelf
                .items
                .chunks(4)
                .enumerate()
                .map(|(column_index, items)| {
                    div()
                        .w(px(column))
                        .flex_shrink_0()
                        .flex()
                        .flex_col()
                        .children(items.iter().enumerate().map(|(row, item)| {
                            self.related_song(item, (index, column_index * 4 + row), cx)
                        }))
                        .into_any_element()
                })
                .collect::<Vec<_>>()
        } else {
            shelf
                .items
                .iter()
                .enumerate()
                .map(|(card, item)| self.related_card(item, (index, card), cx))
                .collect()
        };
        let previous = scroll.clone();
        let next = scroll.clone();
        let theme = self.theme;
        let arrow = move |id: (&'static str, usize), name: &'static str, enabled: bool| {
            button(id, name, theme)
                .size(px(36.))
                .border_1()
                .border_color(gpui::white().opacity(0.2))
                .opacity(if enabled { 1. } else { 0.35 })
        };
        div()
            .pt(px(32.))
            .flex()
            .flex_col()
            .gap(px(16.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(16.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(24.))
                            .font_weight(FontWeight::BOLD)
                            .child(shelf.title.clone()),
                    )
                    .child(
                        arrow(
                            ("related-previous", index),
                            "back",
                            scroll.offset().x < px(-1.),
                        )
                        .on_click(cx.listener(move |_, _, _, cx| {
                            let offset = previous.offset();
                            previous
                                .set_offset(point((offset.x + px(stride)).min(px(0.)), offset.y));
                            cx.notify();
                        })),
                    )
                    .child(
                        arrow(
                            ("related-next", index),
                            "forward",
                            scroll.offset().x > -scroll.max_offset().x + px(1.),
                        )
                        .on_click(cx.listener(move |_, _, _, cx| {
                            let offset = next.offset();
                            next.set_offset(point(
                                (offset.x - px(stride)).max(-next.max_offset().x),
                                offset.y,
                            ));
                            cx.notify();
                        })),
                    ),
            )
            .child(
                div()
                    .id(("related-carousel", index))
                    .w_full()
                    .flex()
                    .gap(px(gap))
                    .overflow_x_scroll()
                    .track_scroll(&scroll)
                    .on_scroll_wheel(cx.listener(|_, _, _, cx| cx.notify()))
                    .children(columns),
            )
            .into_any_element()
    }

    fn related_song(
        &mut self,
        item: &Item,
        id: (usize, usize),
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let art = self.art(item.thumbnail(), 48., ArtworkShape::Square);
        let target = item.clone();
        div()
            .id(("related-song", id.0 * 1000 + id.1))
            .h(px(64.))
            .px(px(8.))
            .flex()
            .items_center()
            .gap(px(16.))
            .rounded(px(4.))
            .cursor_pointer()
            .hover(|row| row.bg(gpui::white().opacity(0.1)))
            .child(art)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .line_height(px(19.))
                    .child(
                        div()
                            .truncate()
                            .text_size(px(16.))
                            .font_weight(FontWeight::MEDIUM)
                            .child(item.title().to_owned()),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_size(px(16.))
                            .text_color(self.theme.muted)
                            .child(item.subtitle().to_owned()),
                    ),
            )
            .on_click(cx.listener(move |view, _, _, cx| view.open_related(&target, cx)))
            .into_any_element()
    }

    fn related_card(
        &mut self,
        item: &Item,
        id: (usize, usize),
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let artist =
            matches!(item, Item::Collection(collection) if collection.id.starts_with("UC"));
        let art = self.art(
            item.thumbnail(),
            160.,
            if artist {
                ArtworkShape::Circle
            } else {
                ArtworkShape::Square
            },
        );
        let target = item.clone();
        div()
            .id(("related-card", id.0 * 1000 + id.1))
            .w(px(160.))
            .flex_shrink_0()
            .flex()
            .flex_col()
            .gap(px(8.))
            .cursor_pointer()
            .child(art)
            .child(
                div()
                    .text_size(px(16.))
                    .font_weight(FontWeight::MEDIUM)
                    .line_clamp(2)
                    .when(artist, |title| title.text_center())
                    .child(item.title().to_owned()),
            )
            .child(
                div()
                    .text_size(px(14.))
                    .text_color(self.theme.muted)
                    .line_clamp(2)
                    .when(artist, |subtitle| subtitle.text_center())
                    .child(item.subtitle().to_owned()),
            )
            .on_click(cx.listener(move |view, _, _, cx| view.open_related(&target, cx)))
            .into_any_element()
    }

    fn open_related(&mut self, item: &Item, cx: &mut Context<Self>) {
        match item.clone() {
            Item::Track(track) => self.play_track(track, cx),
            Item::Collection(collection) => self.navigate(
                Route::Browse(BrowseLink::new(collection.id, collection.title)),
                cx,
            ),
            Item::Navigation(link) => self.navigate(Route::Browse(link), cx),
        }
    }
}

fn same_session(a: Option<&Arc<Session>>, b: Option<&Arc<Session>>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => Arc::ptr_eq(a, b),
        (None, None) => true,
        _ => false,
    }
}

fn related_message(text: String, theme: Theme) -> AnyElement {
    div()
        .pt(px(24.))
        .text_size(px(16.))
        .text_color(theme.muted)
        .child(text)
        .into_any_element()
}

/// Ask Google's image service for a player-page sized cover instead of the list thumbnail.
fn large_artwork(url: &str) -> String {
    let resizable = url.starts_with("https://lh3.googleusercontent.com/")
        || url.starts_with("https://yt3.googleusercontent.com/")
        || url.contains(".ggpht.com/");
    match url.rsplit_once('=') {
        Some((base, _)) if resizable => format!("{base}=w900-h900-l90-rj"),
        _ => url.to_owned(),
    }
}

fn paint_around(
    bounds: Bounds<Pixels>,
    hole: Option<Bounds<Pixels>>,
    color: Hsla,
    window: &mut Window,
) {
    let Some(hole) = hole.map(|hole| hole.intersect(&bounds)) else {
        window.paint_quad(fill(bounds, color));
        return;
    };
    for (top_left, bottom_right) in [
        (bounds.origin, point(bounds.right(), hole.top())),
        (point(bounds.left(), hole.bottom()), bounds.bottom_right()),
        (
            point(bounds.left(), hole.top()),
            point(hole.left(), hole.bottom()),
        ),
        (
            point(hole.right(), hole.top()),
            point(bounds.right(), hole.bottom()),
        ),
    ] {
        if bottom_right.x > top_left.x && bottom_right.y > top_left.y {
            window.paint_quad(fill(Bounds::from_corners(top_left, bottom_right), color));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Details, PlayerPage, large_artwork};
    use crate::{
        auth::Session,
        song_text::{self, Content, Lyrics},
    };
    use std::sync::Arc;

    fn session(identity: &str) -> Arc<Session> {
        Arc::new(
            serde_json::from_value(serde_json::json!({
                "cookie":"synthetic", "playback_cookie":"synthetic", "authuser":"0",
                "data_sync_id":identity, "client_version":"synthetic"
            }))
            .unwrap(),
        )
    }

    fn rejected_lyrics(id: u64) -> song_text::Event {
        song_text::Event {
            id,
            result: Ok(Content::Lyrics(Lyrics {
                text: "Another account's lyrics".into(),
                source: None,
            })),
            session_rejected: true,
        }
    }

    #[test]
    fn tab_details_from_a_previous_account_are_dropped_without_renewing_the_new_one() {
        let (worker, _events) = song_text::Worker::start();
        let mut page = PlayerPage::new(worker);
        let first = session("first-account");
        let pending = || Details {
            id: "MPLYsong".into(),
            session: Some(first.clone()),
            request: 7,
            result: None,
        };

        // Renewed or switched session: the old request finishes after the change.
        page.lyrics = Some(pending());
        assert_eq!(
            page.complete(rejected_lyrics(7), Some(&session("second-account"))),
            None
        );
        assert!(page.lyrics.as_ref().unwrap().result.is_none());

        // Account replaced: the hook clears the request, so its completion matches nothing.
        page.reset_account();
        assert_eq!(page.complete(rejected_lyrics(7), Some(&first)), None);
        assert!(page.lyrics.is_none());

        // The same account still receives its own result and rejection.
        page.lyrics = Some(pending());
        assert_eq!(page.complete(rejected_lyrics(7), Some(&first)), Some(true));
        assert!(page.lyrics.as_ref().unwrap().result.is_some());
    }

    #[test]
    fn player_page_artwork_requests_a_large_cover_only_from_resizable_hosts() {
        assert_eq!(
            large_artwork("https://lh3.googleusercontent.com/abc=w120-h120-l90-rj"),
            "https://lh3.googleusercontent.com/abc=w900-h900-l90-rj"
        );
        let video = "https://i.ytimg.com/vi/abc/hqdefault.jpg?sqp=a=b";
        assert_eq!(large_artwork(video), video);
    }
}
