//! The player page: artwork or video beside the Up next, Lyrics, Comments and Related tabs.

use super::*;
use pages::shelves::visible_cards;
use queue_panel::Reorder;

mod comments;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::app) enum PlayerTab {
    UpNext,
    Lyrics,
    Comments,
    Related,
}

impl PlayerTab {
    const ALL: [Self; 4] = [Self::UpNext, Self::Lyrics, Self::Comments, Self::Related];

    fn label(self) -> &'static str {
        match self {
            Self::UpNext => "UP NEXT",
            Self::Lyrics => "LYRICS",
            Self::Comments => "COMMENTS",
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
    comments: comments::State,
    related_list: ListState,
    related_scroll: Vec<ScrollHandle>,
    pub(in crate::app) tabs_scroll: ScrollHandle,
    /// Focus handles for each shelf's items, so keyboard focus can scroll its carousel.
    related_focus: Vec<Vec<FocusHandle>>,
    pub(in crate::app) queue_scroll: UniformListScrollHandle,
    pub(in crate::app) queue_bounds: Rc<Cell<Bounds<Pixels>>>,
    pub(in crate::app) reorder: Option<Reorder>,
    pub(in crate::app) hovered_row: Option<usize>,
    /// The pointer while reordering Up next, kept for edge auto-scroll between moves.
    pub(in crate::app) drag_pointer: Point<Pixels>,
    pub(in crate::app) autoscrolling: bool,
    pub(in crate::app) video_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
}

impl PlayerPage {
    #[cfg(test)]
    pub(in crate::app) fn selected_tab(&self) -> PlayerTab {
        self.tab
    }

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
            comments: comments::State::new(),
            related_list: ListState::new(0, ListAlignment::Top, px(400.)),
            related_scroll: Vec::new(),
            tabs_scroll: ScrollHandle::new(),
            related_focus: Vec::new(),
            queue_scroll: UniformListScrollHandle::new(),
            queue_bounds: Rc::new(Cell::new(Bounds::default())),
            reorder: None,
            hovered_row: None,
            drag_pointer: Point::default(),
            autoscrolling: false,
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
            self.related_list.reset(related.shelves.len());
        }
        slot.result = Some(event.result);
        Some(event.session_rejected)
    }

    /// Drop Lyrics, Comments and Related loaded or loading for an account that is no longer current.
    pub(in crate::app) fn reset_account(&mut self) {
        self.worker.cancel();
        self.comments = comments::State::new();
        self.lyrics = None;
        self.related = None;
        self.related_scroll.clear();
        self.related_focus.clear();
        self.related_list.reset(0);
    }

    fn details(&mut self, tab: PlayerTab) -> Option<&mut Option<Details>> {
        match tab {
            PlayerTab::UpNext | PlayerTab::Comments => None,
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
    pub(in crate::app) fn select_player_tab(&mut self, tab: PlayerTab, cx: &mut Context<Self>) {
        if self.player_page.tab != tab {
            if self.player_page.tab == PlayerTab::Comments && self.player_page.comments.cancel() {
                self.player_page.worker.cancel();
            }
            self.player_page.comments.close_menu();
            self.player_page.tab = tab;
            cx.notify();
        }
    }

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

    /// Album pages list tracks without artwork, so fall back to the watch response's.
    pub(in crate::app) fn current_thumbnail(&self) -> Option<String> {
        let current = self.library.queue.current()?;
        current
            .thumbnail
            .clone()
            .or_else(|| self.current_watch()?.current.as_ref()?.thumbnail.clone())
    }

    /// The watch details for the current song, once they have loaded.
    pub(in crate::app) fn current_watch(&self) -> Option<&Watch> {
        let current = self.library.queue.current()?;
        self.now_playing
            .as_ref()
            .filter(|watch| watch.current.as_ref().is_some_and(|w| w.id == current.id))
    }

    fn tab_source(&self, tab: PlayerTab) -> Option<String> {
        let watch = self.current_watch()?;
        match tab {
            PlayerTab::UpNext | PlayerTab::Comments => None,
            PlayerTab::Lyrics => watch.lyrics.clone(),
            PlayerTab::Related => watch.related.clone(),
        }
    }

    /// Request the visible tab's details when the song changes or a request was superseded.
    fn load_visible_tab(&mut self) {
        let tab = self.player_page.tab;
        if tab == PlayerTab::Comments {
            self.load_comments();
            return;
        }
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
            PlayerTab::UpNext | PlayerTab::Comments => return,
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
        if self.player_page.comments.pending_id() == Some(event.id) {
            self.comments_loaded(event, cx);
            return;
        }
        let session = self.account.as_ref().map(|account| &account.session);
        let related =
            (self.player_page.related.as_ref()).is_some_and(|slot| slot.request == event.id);
        let Some(rejected) = self.player_page.complete(event, session) else {
            return;
        };
        if related
            && let Some(Ok(SongText::Related(page))) = self
                .player_page
                .related
                .as_ref()
                .and_then(|slot| slot.result.as_ref())
        {
            self.player_page.related_focus = page
                .shelves
                .iter()
                .map(|shelf| {
                    shelf
                        .items
                        .iter()
                        .map(|_| cx.focus_handle().tab_index(0).tab_stop(true))
                        .collect()
                })
                .collect();
        }
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
            // Leave room for the tabs, queue header, Autoplay and one complete row.
            (width * 0.5625)
                .min(height * 0.5)
                .min((height - spacing.vertical - 256.).max(TOGGLE_HEIGHT))
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
        let tabs = self.player_tabs(side, window, cx);
        let body = match self.player_page.tab {
            PlayerTab::UpNext => self.up_next(cx),
            PlayerTab::Lyrics => self.lyrics_tab(),
            PlayerTab::Comments => self.comments_tab(side, window, cx),
            PlayerTab::Related => self.related_tab(width, side, cx),
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
                .keyboard_control(theme)
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
                .on_click(cx.listener(move |view, event: &ClickEvent, window, cx| {
                    // Keep keyboard focus on the choice; a click hands it back to the app.
                    if !matches!(event, ClickEvent::Keyboard(_)) {
                        window.focus(&view.focus, cx);
                    }
                    if view.library.video == selected {
                        view.set_video_mode(!selected, cx);
                    }
                }))
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
                    .child(option(
                        "song-mode",
                        if episode { "Audio" } else { "Song" },
                        !video,
                    ))
                    .child(option("video-mode", "Video", video)),
            )
            .into_any_element()
    }

    fn player_tabs(&self, width: f32, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let selected = self.player_page.tab;
        let watch = self.current_watch();
        let available = |tab: PlayerTab| match tab {
            PlayerTab::UpNext => true,
            PlayerTab::Comments => watch.is_some_and(|watch| watch.comments.is_some()),
            PlayerTab::Lyrics => watch.is_some_and(|watch| watch.lyrics.is_some()),
            PlayerTab::Related => watch.is_some_and(|watch| watch.related.is_some()),
        };
        let scroll = self.player_page.tabs_scroll.clone();
        let tabs = div()
            .id("player-tabs")
            .flex_1()
            .min_w_0()
            .overflow_x_scroll()
            .track_scroll(&scroll)
            .h(px(48.))
            .flex()
            .children(PlayerTab::ALL.into_iter().map(|tab| {
                let active = tab == selected;
                let enabled = available(tab);
                // Tabs share spare width on top of their label, like the web's `flex: 1 0 auto`.
                div()
                    .id(tab.label())
                    .when(enabled, |tab| tab.keyboard_control(self.theme))
                    .flex_auto()
                    .flex_shrink_0()
                    .whitespace_nowrap()
                    .h(px(48.))
                    .px(px(12.))
                    .relative()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(14.))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(if active {
                        self.theme.text
                    } else if enabled {
                        gpui::white().opacity(0.7)
                    } else {
                        gpui::white().opacity(0.3)
                    })
                    .when(enabled, |tab| tab.cursor_pointer())
                    .child(tab.label())
                    .when(active, |tab| {
                        tab.child(
                            div()
                                .absolute()
                                .left_0()
                                .right_0()
                                .bottom_0()
                                .h(px(1.))
                                .bg(self.theme.text),
                        )
                    })
                    .on_click(cx.listener(move |view, _, _, cx| {
                        if enabled {
                            view.select_player_tab(tab, cx);
                        }
                    }))
            }));
        let minimum = PlayerTab::ALL
            .into_iter()
            .map(|tab| {
                let label: SharedString = tab.label().into();
                let run = TextRun {
                    len: label.len(),
                    font: Font {
                        weight: FontWeight::MEDIUM,
                        ..font("Roboto")
                    },
                    ..Default::default()
                };
                window
                    .text_system()
                    .shape_line(label, px(14.), &[run], None)
                    .width
                    + px(24.)
            })
            .sum::<Pixels>();
        let overflow = minimum > px(width);
        let back = scroll.offset().x < px(0.);
        let mut control = overflow.then(|| {
            button(
                "player-tabs-scroll",
                if back { "back" } else { "forward" },
                self.theme,
            )
            .size(px(48.))
            .flex_shrink_0()
            .bg(self.theme.bg)
            .on_click(cx.listener(move |_, _, _, cx| {
                scroll.set_offset(point(
                    if back { px(0.) } else { -scroll.max_offset().x },
                    px(0.),
                ));
                cx.notify();
            }))
        });
        div()
            .h(px(49.))
            .flex_shrink_0()
            .flex()
            .border_b_1()
            .border_color(gpui::white().opacity(0.1))
            .children(if back { control.take() } else { None })
            .child(tabs)
            .children(control)
            .into_any_element()
    }

    fn lyrics_tab(&self) -> AnyElement {
        let loaded = self
            .player_page
            .lyrics
            .as_ref()
            .filter(|loaded| self.tab_source(PlayerTab::Lyrics).as_ref() == Some(&loaded.id));
        let message = |text: &str| related_message(text.to_owned(), self.theme);
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
            .pt(px(16.))
            .pb(px(32.))
            .text_size(px(14.))
            .line_height(px(19.6))
            .child(lyrics.text.clone())
            .when_some(lyrics.source.clone(), |body, source| {
                body.child(div().my(px(16.)).text_color(self.theme.muted).child(source))
            })
            .into_any_element()
    }

    /// Related shelves, rendered like the web's player-page carousels. Only shelves in
    /// view, and only the columns in view within each, are built.
    fn related_tab(&mut self, width: f32, side: f32, cx: &mut Context<Self>) -> AnyElement {
        let loaded = self
            .player_page
            .related
            .as_ref()
            .filter(|loaded| self.tab_source(PlayerTab::Related).as_ref() == Some(&loaded.id));
        match loaded.and_then(|loaded| loaded.result.as_ref()) {
            Some(Ok(SongText::Related(_))) => {}
            Some(Err(error)) => return related_message(error.clone(), self.theme),
            Some(Ok(_)) => return related_message("Nothing related yet".into(), self.theme),
            None => return related_message("Loading…".into(), self.theme),
        }
        list(
            self.player_page.related_list.clone(),
            cx.processor(move |view, index: usize, window, cx| {
                view.related_shelf(index, width, side, window, cx)
            }),
        )
        .flex_1()
        .min_h_0()
        .into_any_element()
    }

    fn related_shelf(
        &mut self,
        index: usize,
        width: f32,
        side: f32,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(Ok(SongText::Related(page))) = self
            .player_page
            .related
            .as_ref()
            .and_then(|loaded| loaded.result.as_ref())
        else {
            return div().into_any_element();
        };
        let Some(shelf) = page
            .shelves
            .get(index)
            .filter(|shelf| !shelf.items.is_empty())
        else {
            return div().into_any_element();
        };
        let scroll = self
            .player_page
            .related_scroll
            .get(index)
            .cloned()
            .unwrap_or_default();
        let songs = shelf.layout == ShelfLayout::Songs;
        let (column, gap) = related_columns(songs, width, side);
        let stride = column + gap;
        let rows = if songs { 4 } else { 1 };
        let count = shelf.items.len().div_ceil(rows);
        let focus = self.player_page.related_focus.get(index).map(Vec::as_slice);
        // Tabbing onto a clipped column scrolls it into view, as a browser does.
        if window.last_input_was_keyboard()
            && let Some(item) =
                focus.and_then(|items| items.iter().position(|item| item.is_focused(window)))
        {
            let left = (item / rows) as f32 * stride;
            let shown = -f32::from(scroll.offset().x);
            let target = if left < shown {
                left
            } else if left + column > shown + side {
                left + column - side
            } else {
                shown
            };
            if target != shown {
                scroll.set_offset(point(px(-target), scroll.offset().y));
            }
        }
        let range = visible_cards(count, (-scroll.offset().x).into(), side, stride);
        let title = shelf.title.clone();
        let visible = shelf
            .items
            .iter()
            .skip(range.start * rows)
            .take(range.len() * rows)
            .cloned()
            .collect::<Vec<_>>();
        let handles = focus
            .unwrap_or_default()
            .iter()
            .skip(range.start * rows)
            .take(range.len() * rows)
            .cloned()
            .collect::<Vec<_>>();
        let columns = visible
            .chunks(rows)
            .enumerate()
            .map(|(offset, items)| {
                let column_index = range.start + offset;
                div()
                    .w(px(column))
                    .flex_shrink_0()
                    .flex()
                    .flex_col()
                    .gap(px(16.))
                    .children(items.iter().enumerate().map(|(row, item)| {
                        let id = (index, column_index * rows + row);
                        let focus = handles.get(offset * rows + row).cloned();
                        if songs {
                            self.related_song(item, id, focus, cx)
                        } else {
                            self.related_card(item, id, focus, cx)
                        }
                    }))
                    .into_any_element()
            })
            .collect::<Vec<_>>();
        let previous = scroll.clone();
        let next = scroll.clone();
        let theme = self.theme;
        let arrow = move |id: (&'static str, usize), name: &'static str, enabled: bool| {
            button(id, name, theme)
                .tab_stop(enabled)
                .size(px(36.))
                .border_1()
                .border_color(gpui::white().opacity(0.2))
                .opacity(if enabled { 1. } else { 0.35 })
        };
        div()
            .w_full()
            .pt(px(32.))
            .pb(px(48.))
            .flex()
            .flex_col()
            .gap(px(16.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(24.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(24.))
                            .line_height(px(28.8))
                            .font_weight(FontWeight::BOLD)
                            .line_clamp(2)
                            .child(title),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .flex()
                            .gap(px(16.))
                            .child(
                                arrow(
                                    ("related-previous", index),
                                    "back",
                                    scroll.offset().x < px(-1.),
                                )
                                .on_click(cx.listener(
                                    move |_, _, _, cx| {
                                        let offset = previous.offset();
                                        previous.set_offset(point(
                                            (offset.x + px(stride)).min(px(0.)),
                                            offset.y,
                                        ));
                                        cx.notify();
                                    },
                                )),
                            )
                            .child(
                                arrow(
                                    ("related-next", index),
                                    "forward",
                                    -f32::from(scroll.offset().x) + side + 1.
                                        < count as f32 * stride - gap,
                                )
                                .on_click(cx.listener(
                                    move |_, _, _, cx| {
                                        let offset = next.offset();
                                        next.set_offset(point(
                                            (offset.x - px(stride)).max(-next.max_offset().x),
                                            offset.y,
                                        ));
                                        cx.notify();
                                    },
                                )),
                            ),
                    ),
            )
            .child(
                div()
                    .id(("related-carousel", index))
                    .w_full()
                    .overflow_x_scroll()
                    .restrict_scroll_to_axis()
                    .track_scroll(&scroll)
                    .child(
                        div()
                            .relative()
                            .w(px((count as f32 * stride - gap).max(0.)))
                            .h(px(if songs {
                                4. * 48. + 3. * 16.
                            } else {
                                RELATED_CARD + 8. + 4. * 16.8 + 4.
                            }))
                            .child(
                                div()
                                    .absolute()
                                    .left(px(range.start as f32 * stride))
                                    .flex()
                                    .gap(px(gap))
                                    .children(columns),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn related_song(
        &mut self,
        item: &Item,
        id: (usize, usize),
        focus: Option<FocusHandle>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let art = self.art(item.thumbnail(), 48., ArtworkShape::Square);
        let target = item.clone();
        let (artist, album) = match item {
            Item::Track(track) => (track.artist.clone(), track.album.clone()),
            _ => (item.subtitle().to_owned(), None),
        };
        div()
            .id(("related-song", id.0 * 1000 + id.1))
            .when_some(focus, |row, focus| row.track_focus(&focus))
            .keyboard_control(self.theme)
            .h(px(48.))
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
                    .gap(px(3.))
                    .text_size(px(14.))
                    .line_height(px(16.8))
                    .child(
                        div()
                            .truncate()
                            .font_weight(FontWeight::MEDIUM)
                            .child(item.title().to_owned()),
                    )
                    .child(
                        div()
                            .flex()
                            .min_w_0()
                            .text_color(gpui::white().opacity(0.7))
                            .child(div().min_w_0().truncate().child(artist))
                            .when_some(album.filter(|album| !album.is_empty()), |line, album| {
                                line.child(div().flex_shrink_0().child("\u{a0}•\u{a0}"))
                                    .child(div().min_w_0().truncate().child(album))
                            }),
                    ),
            )
            .on_click(cx.listener(move |view, _, window, cx| {
                window.focus(&view.focus, cx);
                view.open_related(&target, cx)
            }))
            .into_any_element()
    }

    fn related_card(
        &mut self,
        item: &Item,
        id: (usize, usize),
        focus: Option<FocusHandle>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let artist =
            matches!(item, Item::Collection(collection) if collection.id.starts_with("UC"));
        let art = self.art(
            item.thumbnail(),
            RELATED_CARD,
            if artist {
                ArtworkShape::Circle
            } else {
                ArtworkShape::Square
            },
        );
        let target = item.clone();
        div()
            .id(("related-card", id.0 * 1000 + id.1))
            .when_some(focus, |card, focus| card.track_focus(&focus))
            .keyboard_control(self.theme)
            .w(px(RELATED_CARD))
            .flex_shrink_0()
            .flex()
            .flex_col()
            .gap(px(8.))
            .cursor_pointer()
            .text_size(px(14.))
            .line_height(px(16.8))
            .child(art)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(3.))
                    .when(artist, |text| text.items_center().text_center())
                    .child(
                        div()
                            .font_weight(FontWeight::MEDIUM)
                            .line_clamp(2)
                            .child(item.title().to_owned()),
                    )
                    .child(
                        div()
                            .text_color(gpui::white().opacity(0.7))
                            .line_clamp(2)
                            .child(item.subtitle().to_owned()),
                    ),
            )
            .on_click(cx.listener(move |view, _, window, cx| {
                window.focus(&view.focus, cx);
                view.open_related(&target, cx)
            }))
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

/// Player-page card artwork edge, as on music.youtube.com.
const RELATED_CARD: f32 = 160.;

/// Column width and gap for a Related carousel. Song lists follow the web's responsive
/// list item width, capped at 200 px below its large breakpoint, plus 8 px row padding.
fn related_columns(songs: bool, width: f32, side: f32) -> (f32, f32) {
    let large = width >= 1364.;
    let gap = if large { 24. } else { 16. };
    let column = match (songs, width) {
        (false, _) => RELATED_CARD,
        (true, ..936.) => side.min(436.),
        (true, ..1364.) => 216.,
        (true, _) => ((width - 452.).min(1478.) - 96.) / 3. + 16.,
    };
    (column, gap)
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
        .pt(px(16.))
        .text_size(px(14.))
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
