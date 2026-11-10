//! Overlay state, menus/dialogs, and picker/form lifecycle.

pub(in crate::app) mod account;
pub(in crate::app) mod settings;

use super::*;

#[derive(Clone)]
pub(in crate::app) enum Overlay {
    Help,
    Settings,
    SignIn,
    Account,
    CountryPicker(Entity<CountryPicker>),
    Description {
        title: String,
        text: String,
    },
    Chapters(Box<Track>),
    Track {
        track: Track,
        index: Option<usize>,
        /// Set when opened from Up next, so the menu can remove that queue entry.
        queue_index: Option<usize>,
    },
    Collection(Collection),
    PlaylistPicker {
        track: Track,
        page: Option<Box<Page>>,
        duplicates: bool,
        error: Option<String>,
    },
    AlreadyAdded {
        id: String,
        video_ids: Vec<String>,
    },
    PlaylistForm {
        form: Entity<PlaylistForm>,
    },
    DeletePlaylist {
        id: String,
        title: String,
    },
    SongText {
        id: u64,
        title: String,
        content: Option<Result<SongText, String>>,
    },
}

impl Music {
    pub(in crate::app) fn playlist_form(
        &mut self,
        edit: bool,
        track: Option<Track>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (id, title, description, privacy) = if edit {
            let Route::Browse(link) = &self.route else {
                return;
            };
            if !self.page.editable {
                return;
            }
            let privacy = match self.page.privacy.as_deref() {
                Some("PRIVATE") => Privacy::Private,
                Some("UNLISTED") => Privacy::Unlisted,
                Some("PUBLIC") => Privacy::Public,
                _ => {
                    self.error = Some(
                        "Refresh the playlist to load its current privacy before editing.".into(),
                    );
                    cx.notify();
                    return;
                }
            };
            (
                Some(link.id.clone()),
                self.page.title.clone(),
                self.page.description.clone(),
                privacy,
            )
        } else {
            (None, String::new(), String::new(), Privacy::Private)
        };
        let form = cx.new(|cx| PlaylistForm::new(title, description, privacy, cx));
        let edit_id = id.clone();
        let initial_video_id = track.as_ref().map(|track| track.id.clone());
        self.overlay_subscription = Some(cx.subscribe_in(
            &form,
            window,
            move |view, _, event, window, cx| match event {
                PlaylistFormEvent::Save {
                    title,
                    description,
                    privacy,
                } => {
                    let mutation = match &edit_id {
                        Some(id) => Mutation::EditPlaylist {
                            id: id.clone(),
                            title: title.clone(),
                            description: description.clone(),
                            privacy: *privacy,
                        },
                        None => Mutation::CreatePlaylist {
                            title: title.clone(),
                            description: description.clone(),
                            privacy: *privacy,
                            initial_video_id: initial_video_id.clone(),
                        },
                    };
                    if view.mutate(mutation, cx) {
                        view.close_overlay(window, cx);
                    }
                }
                PlaylistFormEvent::Cancel => {
                    view.close_overlay(window, cx);
                }
            },
        ));
        window.focus(&form.focus_handle(cx), cx);
        self.error = None;
        self.overlay = Some(Overlay::PlaylistForm { form });
        cx.notify();
    }

    pub(in crate::app) fn playlist_picker(&mut self, track: Track, cx: &mut Context<Self>) {
        self.error = None;
        self.overlay = Some(Overlay::PlaylistPicker {
            track,
            page: None,
            duplicates: false,
            error: None,
        });
        self.request(Query::PlaylistChoices, cx);
    }

    pub(in crate::app) fn country_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.page.countries.is_empty() {
            return;
        }
        let picker = cx.new(|cx| CountryPicker::new(self.page.countries.clone(), cx));
        self.overlay_subscription = Some(cx.subscribe_in(
            &picker,
            window,
            |view, picker, event, window, cx| {
                if !matches!(&view.overlay, Some(Overlay::CountryPicker(current)) if current == picker) {
                    return;
                }
                view.close_overlay(window, cx);
                if let CountryPickerEvent::Choose(target) = event {
                    view.navigate(Route::Browse(target.clone()), cx);
                }
            },
        ));
        window.focus(&picker.focus_handle(cx), cx);
        self.overlay = Some(Overlay::CountryPicker(picker));
        cx.notify();
    }

    pub(in crate::app) fn open_track_menu(
        &mut self,
        track: Track,
        index: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.overlay_return_focus = window.focused(cx);
        window.focus(&self.overlay_focus, cx);
        self.overlay_anchor = Some(window.mouse_position());
        self.overlay = Some(Overlay::Track {
            track,
            index,
            queue_index: None,
        });
        let view = cx.weak_entity();
        window.on_next_frame(move |window, cx| {
            let _ = view.update(cx, |view, cx| {
                if view.overlay_focus.is_focused(window) {
                    view.move_overlay_focus(false, window, cx);
                }
            });
        });
        cx.notify();
    }

    pub(in crate::app) fn open_item_menu(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.rows.get(index).cloned() {
            Some(Item::Track(track)) => self.open_track_menu(track, Some(index), window, cx),
            Some(Item::Collection(collection)) => self.open_collection_menu(collection, window, cx),
            _ => {}
        }
    }

    pub(in crate::app) fn open_collection_menu(
        &mut self,
        collection: Collection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.overlay_return_focus = window.focused(cx);
        window.focus(&self.overlay_focus, cx);
        self.overlay_anchor = Some(window.mouse_position());
        self.overlay = Some(Overlay::Collection(collection));
        let view = cx.weak_entity();
        window.on_next_frame(move |window, cx| {
            let _ = view.update(cx, |view, cx| {
                if view.overlay_focus.is_focused(window) {
                    view.move_overlay_focus(false, window, cx);
                }
            });
        });
        cx.notify();
    }

    pub(in crate::app) fn move_overlay_focus(
        &self,
        backward: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let root = if self.settings_menu.is_some() {
            &self.settings_focus
        } else {
            &self.overlay_focus
        };
        let current = window.focused(cx);
        // Only tab stops inside the dialog participate. Keep this derived from the
        // rendered controls so conditional menu actions cannot leave stale indices.
        window.focus(root, cx);
        window.focus_next(cx);
        let mut controls = Vec::new();
        while root.contains_focused(window, cx) {
            let Some(focus) = window.focused(cx) else {
                break;
            };
            if controls.first() == Some(&focus) {
                break;
            }
            controls.push(focus);
            window.focus_next(cx);
        }
        if controls.is_empty() {
            window.focus(root, cx);
            return;
        }
        let index = current.and_then(|focus| controls.iter().position(|item| *item == focus));
        let next = if backward {
            index.map_or(controls.len() - 1, |index| {
                (index + controls.len() - 1) % controls.len()
            })
        } else {
            index.map_or(0, |index| (index + 1) % controls.len())
        };
        window.focus(&controls[next], cx);
    }

    pub(in crate::app) fn close_overlay(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.overlay, Some(Overlay::SignIn)) {
            self.cancel_connection(cx);
        }
        self.catalog_requests.cancel(CatalogTarget::PlaylistPicker);
        self.song_text.cancel();
        self.overlay = None;
        self.settings_menu = None;
        self.overlay_anchor = None;
        self.overlay_subscription = None;
        window.focus(
            self.overlay_return_focus
                .take()
                .as_ref()
                .unwrap_or(&self.focus),
            cx,
        );
        cx.notify();
    }

    pub(in crate::app) fn show_song_text(
        &mut self,
        query: TextQuery,
        title: &str,
        cx: &mut Context<Self>,
    ) {
        let label = match &query {
            TextQuery::Lyrics(_) => "Lyrics",
            TextQuery::Credits(_) => "Credits",
            TextQuery::Related(_) => "Related",
        };
        let title = format!("{label} · {title}");
        let session = self.account.as_ref().map(|account| account.session.clone());
        self.overlay = None;
        match self.song_text.load(query, session) {
            Ok(id) => {
                self.overlay = Some(Overlay::SongText {
                    id,
                    title,
                    content: None,
                })
            }
            Err(error) => self.error = Some(format!("{error:#}")),
        }
        cx.notify();
    }

    pub(in crate::app) fn menu_action(
        &self,
        id: impl Into<ElementId>,
        label: impl Into<SharedString>,
        cx: &Context<Self>,
        action: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> impl IntoElement {
        let id = id.into();
        let glyph = match &id {
            ElementId::Name(name) => match name.as_ref() {
                "menu-radio" => Some("radio"),
                "menu-next" => Some("play-next"),
                "menu-enqueue" => Some("queue"),
                "menu-playlist" | "menu-library" => Some("playlist-add"),
                "menu-artist" | "menu-show" => Some("person"),
                "menu-album" => Some("album"),
                "menu-like" => Some("thumb-up"),
                "menu-dislike" => Some("thumb-down"),
                "menu-history" => Some("history"),
                "menu-remove-occurrence" | "menu-remove-queue" => Some("close"),
                "menu-share" => Some("link"),
                "menu-credits" | "menu-lyrics" | "menu-episode" | "menu-chapters" => {
                    Some("description")
                }
                "menu-related" => Some("music"),
                _ => None,
            },
            _ => None,
        };
        div()
            .id(id)
            .keyboard_control(self.theme)
            .w_full()
            .h(px(48.))
            .px(px(8.))
            .flex()
            .items_center()
            .cursor_pointer()
            .text_size(px(14.))
            .hover(|d| d.bg(self.theme.raised))
            .children(glyph.map(|glyph| div().mx(px(8.)).child(icon(glyph, 18., self.theme.text))))
            .child(div().mx(px(8.)).flex_1().min_w_0().child(label.into()))
            .on_click(cx.listener(move |view, _, window, cx| {
                let previous = view.overlay.as_ref().map(std::mem::discriminant);
                action(view, window, cx);
                view.finish_menu_action(previous, window, cx);
            }))
    }

    pub(in crate::app) fn finish_menu_action(
        &mut self,
        previous: Option<std::mem::Discriminant<Overlay>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.overlay.is_none() {
            self.close_overlay(window, cx);
            window.focus(&self.focus, cx);
            return;
        }
        if previous != self.overlay.as_ref().map(std::mem::discriminant)
            && !matches!(
                self.overlay,
                Some(Overlay::CountryPicker(_) | Overlay::PlaylistForm { .. })
            )
        {
            window.focus(&self.overlay_focus, cx);
        }
    }

    pub(in crate::app) fn backdrop(&self, body: AnyElement, cx: &Context<Self>) -> AnyElement {
        div()
            .id("dialog-focus")
            .absolute()
            .inset_0()
            .bg(gpui::black().opacity(0.65))
            .flex()
            .items_center()
            .justify_center()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|view, _, window, cx| {
                    cx.stop_propagation();
                    view.close_overlay(window, cx);
                }),
            )
            .child(body)
            .into_any_element()
    }

    pub(in crate::app) fn overlay(
        &self,
        kind: &Overlay,
        window: &Window,
        cx: &Context<Self>,
    ) -> AnyElement {
        match kind {
            Overlay::Account => unreachable!("Account artwork is prepared by the renderer"),
            Overlay::SignIn => return self.sign_in_dialog(cx),
            Overlay::Settings => return self.settings_dialog(window, cx),
            _ => {}
        }
        if let Overlay::CountryPicker(picker) = kind {
            return self.backdrop(picker.clone().into_any_element(), cx);
        }
        if let Overlay::PlaylistForm { form, .. } = kind {
            let mut body = div().flex().flex_col().gap(px(12.)).child(form.clone());
            if let Some(error) = &self.error {
                body = body.child(
                    div()
                        .max_w(px(480.))
                        .p(px(12.))
                        .bg(self.theme.panel)
                        .text_color(rgb(0xf1a6ae))
                        .child(error.clone()),
                );
            }
            return self.backdrop(body.into_any_element(), cx);
        }
        let mut body = div()
            .id("overlay-body")
            .max_h(relative(0.9))
            .overflow_y_scroll()
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
                    .gap(px(12.))
                    .when(
                        matches!(kind, Overlay::Track { .. } | Overlay::Collection(_)),
                        |d| d.h(px(0.)).mb(px(0.)).overflow_hidden(),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(23.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(match kind {
                                Overlay::Help => "Keyboard shortcuts".to_owned(),
                                Overlay::Settings | Overlay::Account | Overlay::SignIn => {
                                    unreachable!()
                                }
                                Overlay::Description { .. } => "Description".into(),
                                Overlay::Chapters(_) => "Chapters".into(),
                                Overlay::Track { track, .. } => track.title.clone(),
                                Overlay::Collection(collection) => collection.title.clone(),
                                Overlay::PlaylistPicker { .. } => "Add to playlist".into(),
                                Overlay::AlreadyAdded { .. } => "Already added".into(),
                                Overlay::DeletePlaylist { .. } => "Delete playlist?".into(),
                                Overlay::SongText { title, .. } => title.clone(),
                                Overlay::PlaylistForm { .. } | Overlay::CountryPicker(_) => {
                                    unreachable!()
                                }
                            }),
                    )
                    .child(
                        button("close-overlay", "close", self.theme)
                            .when(
                                matches!(kind, Overlay::Track { .. } | Overlay::Collection(_)),
                                |button| button.tab_stop(false),
                            )
                            .on_click(
                                cx.listener(|view, _, window, cx| view.close_overlay(window, cx)),
                            ),
                    ),
            );
        if matches!(kind, Overlay::Help) {
            for (key, action) in [
                ("/ or Ctrl K", "Search"),
                ("Alt ←", "Go back"),
                ("Space", "Play or pause"),
                ("Ctrl → / Ctrl ←", "Next / previous song"),
                ("→ / ←", "Seek 5 seconds"),
                ("↑ / ↓", "Adjust volume"),
                ("L", "Like the current song"),
                ("Q", "Open or close the player page"),
                ("F", "Toggle video fullscreen"),
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
        } else if let Overlay::Description { title, text } = kind {
            body = body
                .child(
                    div()
                        .text_size(px(18.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(title.clone()),
                )
                .child(div().mt(px(18.)).text_size(px(14.)).child(text.clone()));
        } else if let Overlay::Chapters(track) = kind {
            body = body.child(
                div()
                    .mb(px(18.))
                    .text_size(px(18.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(track.title.clone()),
            );
            if let Some(episode) = &track.details.episode {
                let selected = Rc::new((**track).clone());
                for (index, chapter) in episode.chapters.iter().enumerate() {
                    let selected = selected.clone();
                    let seconds = chapter.start_seconds;
                    body = body.child(
                        div()
                            .id(("chapter", index))
                            .keyboard_control(self.theme)
                            .py(px(12.))
                            .px(px(8.))
                            .flex()
                            .items_start()
                            .gap(px(14.))
                            .text_size(px(14.))
                            .cursor_pointer()
                            .hover(|style| style.bg(self.theme.raised))
                            .child(
                                div()
                                    .w(px(64.))
                                    .flex_shrink_0()
                                    .text_color(self.theme.muted)
                                    .child(clock(seconds as f64)),
                            )
                            .child(div().flex_1().min_w_0().child(chapter.title.clone()))
                            .on_click(cx.listener(move |view, _, window, cx| {
                                view.play_chapter((*selected).clone(), seconds, cx);
                                view.close_overlay(window, cx);
                            })),
                    );
                }
            }
        } else if let Overlay::Track {
            track,
            index,
            queue_index,
        } = kind
        {
            let mut track = track.clone();
            if self.account.is_some()
                && let Some(current) = self
                    .library
                    .queue
                    .current()
                    .filter(|current| current.id == track.id)
            {
                track.details.rating = current.details.rating;
                track.details.in_library = current.details.in_library;
                track.details.add_library = current.details.add_library.clone();
                track.details.remove_library = current.details.remove_library.clone();
            }
            let track = &track;
            if let Some(episode) = &track.details.episode {
                if !episode.subtitle.is_empty() {
                    body = body.child(
                        div()
                            .mb(px(16.))
                            .text_color(self.theme.muted)
                            .child(episode.subtitle.clone()),
                    );
                }
                let target = BrowseLink::new(format!("MPED{}", track.id), track.title.clone());
                body = body.child(self.menu_action(
                    "menu-episode",
                    "Episode details",
                    cx,
                    move |view, _, cx| {
                        view.overlay = None;
                        view.navigate(Route::Browse(target.clone()), cx);
                    },
                ));
                if !episode.chapters.is_empty() {
                    let track = track.clone();
                    body = body.child(self.menu_action(
                        "menu-chapters",
                        "Chapters",
                        cx,
                        move |view, window, cx| {
                            window.focus(&view.focus, cx);
                            view.overlay = Some(Overlay::Chapters(Box::new(track.clone())));
                            cx.notify();
                        },
                    ));
                }
                if let Some(id) = &episode.show_id {
                    let target = BrowseLink::new(id.clone(), track.artist.clone());
                    body = body.child(self.menu_action(
                        "menu-show",
                        "Go to podcast",
                        cx,
                        move |view, _, cx| {
                            view.overlay = None;
                            view.navigate(Route::Browse(target.clone()), cx);
                        },
                    ));
                }
            }
            if track.details.episode.is_none() {
                let song = track.clone();
                body = body.child(self.menu_action(
                    "menu-radio",
                    "Start radio",
                    cx,
                    move |view, _, cx| {
                        view.overlay = None;
                        view.start_queue(
                            PlaybackQuery::Watch {
                                track: Box::new(song.clone()),
                                mode: WatchMode::Radio,
                            },
                            cx,
                        );
                    },
                ));
            }
            if let Some(watch) = &self.now_playing
                && watch
                    .current
                    .as_ref()
                    .is_some_and(|current| current.id == track.id)
            {
                if watch.lyrics.is_some() {
                    body = body.child(self.menu_action(
                        "menu-lyrics",
                        "Lyrics",
                        cx,
                        |view, _, cx| view.show_player_tab(PlayerTab::Lyrics, cx),
                    ));
                }
                if watch.related.is_some() {
                    body = body.child(self.menu_action(
                        "menu-related",
                        "Related music",
                        cx,
                        |view, _, cx| view.show_player_tab(PlayerTab::Related, cx),
                    ));
                }
            }
            if let Some(id) = track.credits_id() {
                let title = track.title.clone();
                body = body.child(self.menu_action(
                    "menu-credits",
                    "Song credits",
                    cx,
                    move |view, _, cx| {
                        view.show_song_text(TextQuery::Credits(id.clone()), &title, cx)
                    },
                ));
            }
            let song = track.clone();
            body = body.child(self.menu_action(
                "menu-like",
                if self.is_liked(track) {
                    "Remove like"
                } else {
                    "Like"
                },
                cx,
                move |view, _, cx| {
                    view.overlay = None;
                    view.toggle_like(&song, cx);
                },
            ));
            let song = track.clone();
            body =
                body.child(
                    self.menu_action("menu-next", "Play next", cx, move |view, _, cx| {
                        let index = view.library.queue.index.map_or(0, |i| i + 1);
                        view.library.queue.tracks.insert(index, song.clone());
                        view.queue_changed(cx);
                        view.overlay = None;
                        view.save();
                        cx.notify();
                    }),
                );
            let song = track.clone();
            body = body.child(self.menu_action(
                "menu-enqueue",
                "Add to queue",
                cx,
                move |view, _, cx| {
                    view.overlay = None;
                    view.enqueue(song.clone(), cx);
                },
            ));
            if let Some(queued) = queue_index.filter(|&queued| {
                self.library
                    .queue
                    .tracks
                    .get(queued)
                    .is_some_and(|entry| entry.id == track.id)
            }) {
                body = body.child(self.menu_action(
                    "menu-remove-queue",
                    "Remove from queue",
                    cx,
                    move |view, _, cx| {
                        view.overlay = None;
                        view.remove_queue_track(queued, cx);
                    },
                ));
            }
            if self.account.is_some() {
                let song = track.clone();
                let disliked = self
                    .ratings
                    .get(&track.id)
                    .copied()
                    .or(track.details.rating)
                    == Some(Rating::Dislike);
                body = body.child(self.menu_action(
                    "menu-dislike",
                    if disliked {
                        "Remove dislike"
                    } else {
                        "Dislike"
                    },
                    cx,
                    move |view, _, cx| {
                        view.overlay = None;
                        view.mutate(
                            Mutation::RateSong {
                                track: song.clone(),
                                rating: if disliked {
                                    Rating::Indifferent
                                } else {
                                    Rating::Dislike
                                },
                            },
                            cx,
                        );
                    },
                ));
                if track.details.add_library.is_some() || track.details.remove_library.is_some() {
                    let song = track.clone();
                    let save = track.details.in_library != Some(true);
                    body = body.child(self.menu_action(
                        "menu-library",
                        if save {
                            "Save to library"
                        } else {
                            "Remove from library"
                        },
                        cx,
                        move |view, _, cx| {
                            view.overlay = None;
                            view.mutate(
                                Mutation::SongLibrary {
                                    track: song.clone(),
                                    save,
                                },
                                cx,
                            );
                        },
                    ));
                }
                let song = track.clone();
                body = body.child(self.menu_action(
                    "menu-playlist",
                    "Add to playlist",
                    cx,
                    move |view, _, cx| view.playlist_picker(song.clone(), cx),
                ));
                if let Some(token) = &track.details.remove_history {
                    let token = token.clone();
                    body = body.child(self.menu_action(
                        "menu-history",
                        "Remove from history",
                        cx,
                        move |view, _, cx| {
                            view.overlay = None;
                            view.mutate(
                                Mutation::RemoveHistory {
                                    token: token.clone(),
                                },
                                cx,
                            );
                        },
                    ));
                }
                if index.is_some()
                    && self.page.editable
                    && let Route::Browse(link) = &self.route
                    && let Some(set_id) = &track.details.set_video_id
                {
                    let playlist = link.id.clone();
                    let set = set_id.clone();
                    let video_id = track.id.clone();
                    body = body.child(self.menu_action(
                        "menu-remove-occurrence",
                        "Remove from playlist",
                        cx,
                        move |view, _, cx| {
                            view.overlay = None;
                            view.mutate(
                                Mutation::RemoveFromPlaylist {
                                    id: playlist.clone(),
                                    video_id: video_id.clone(),
                                    set_video_id: set.clone(),
                                },
                                cx,
                            );
                        },
                    ));
                    if let Some(index) = index.and_then(|index| self.page.track_index(index)) {
                        let tracks = self.page.tracks();
                        for (key, label, before) in [
                            (
                                "menu-move-up",
                                "Move up",
                                index
                                    .checked_sub(1)
                                    .and_then(|i| tracks.get(i))
                                    .and_then(|track| track.details.set_video_id.clone())
                                    .map(Some),
                            ),
                            (
                                "menu-move-down",
                                "Move down",
                                playlist_move_down(
                                    &tracks,
                                    index,
                                    self.page.continuation.is_some(),
                                ),
                            ),
                        ] {
                            if let Some(before) = before {
                                let playlist = link.id.clone();
                                let set = set_id.clone();
                                body = body.child(self.menu_action(
                                    key,
                                    label,
                                    cx,
                                    move |view, _, cx| {
                                        view.overlay = None;
                                        view.mutate(
                                            Mutation::MoveInPlaylist {
                                                id: playlist.clone(),
                                                set_video_id: set.clone(),
                                                before: before.clone(),
                                            },
                                            cx,
                                        );
                                    },
                                ));
                            }
                        }
                    }
                }
            }
            for (id, label, title, target) in [
                (
                    "menu-artist",
                    if track.details.episode.is_some() {
                        "Go to creator"
                    } else {
                        "Go to artist"
                    },
                    "Artist",
                    &track.details.artist_id,
                ),
                (
                    "menu-album",
                    "Go to album",
                    "Album",
                    &track.details.album_id,
                ),
            ] {
                if let Some(target) = target {
                    let target = target.clone();
                    body = body.child(self.menu_action(id, label, cx, move |view, _, cx| {
                        view.overlay = None;
                        view.navigate(Route::Browse(BrowseLink::new(target.clone(), title)), cx);
                    }));
                }
            }
            let url = format!("https://music.youtube.com/watch?v={}", track.id);
            body = body.child(self.menu_action(
                "menu-share",
                if track.details.episode.is_some() {
                    "Copy episode link"
                } else {
                    "Copy song link"
                },
                cx,
                move |view, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(url.clone()));
                    view.overlay = None;
                    view.notice = Some("Link copied".into());
                    cx.notify();
                },
            ));
        } else if let Overlay::Collection(collection) = kind {
            for (id, label, target) in [
                (
                    "menu-shuffle",
                    "Shuffle play",
                    collection.details.shuffle.clone(),
                ),
                ("menu-radio", "Start mix", collection.details.mix.clone()),
            ] {
                if let Some(target) = target {
                    body = body.child(self.menu_action(id, label, cx, move |view, window, cx| {
                        view.close_overlay(window, cx);
                        view.start_queue(PlaybackQuery::Playlist(target.clone()), cx);
                    }));
                }
            }
            if let Some(target) = collection.details.play.clone() {
                body = body.child(self.menu_action(
                    "collection-play",
                    "Play",
                    cx,
                    move |view, window, cx| {
                        view.close_overlay(window, cx);
                        view.start_queue(PlaybackQuery::Playlist(target.clone()), cx);
                    },
                ));
            }
            let saved = self.is_saved(collection);
            let selected = collection.clone();
            body = body.child(self.menu_action(
                "menu-library",
                if collection.id.starts_with("UC") {
                    if saved { "Unsubscribe" } else { "Subscribe" }
                } else if saved {
                    "Remove from library"
                } else {
                    "Save to library"
                },
                cx,
                move |view, window, cx| {
                    view.close_overlay(window, cx);
                    if selected.id.starts_with("UC") && view.account.is_none() {
                        view.connect_account(window, cx);
                        return;
                    }
                    view.save_collection(selected.clone(), cx);
                },
            ));
            let url = format!("https://music.youtube.com/browse/{}", collection.id);
            body = body.child(self.menu_action(
                "menu-share",
                "Copy link",
                cx,
                move |view, window, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(url.clone()));
                    view.close_overlay(window, cx);
                    view.notice = Some("Link copied".into());
                },
            ));
        } else if let Overlay::SongText { content, .. } = kind {
            match content {
                None => body = body.child(div().text_color(self.theme.muted).child("Loading…")),
                Some(Err(error)) => {
                    body = body.child(div().text_color(self.theme.muted).child(error.clone()))
                }
                Some(Ok(SongText::Lyrics(lyrics))) => {
                    body = body.child(div().text_size(px(17.)).child(lyrics.text.clone()))
                }
                Some(Ok(SongText::Related(_))) => {}
                Some(Ok(SongText::Credits(credits))) => {
                    for section in credits {
                        body = body.child(
                            div()
                                .mb(px(22.))
                                .child(
                                    div()
                                        .text_size(px(16.))
                                        .font_weight(FontWeight::MEDIUM)
                                        .child(section.title.clone()),
                                )
                                .child(
                                    div()
                                        .mt(px(8.))
                                        .text_size(px(14.))
                                        .text_color(self.theme.muted)
                                        .child(section.names.clone()),
                                ),
                        );
                    }
                }
            }
        } else if let Overlay::PlaylistPicker {
            track,
            page,
            duplicates,
            error,
        } = kind
        {
            let song = track.clone();
            body = body.child(self.menu_action(
                "create-from-picker",
                "New playlist",
                cx,
                move |view, window, cx| view.playlist_form(false, Some(song.clone()), window, cx),
            ));
            body = body.child(self.menu_action(
                "allow-duplicates",
                if *duplicates {
                    "Allow duplicates: on"
                } else {
                    "Allow duplicates: off"
                },
                cx,
                |view, _, cx| {
                    if let Some(Overlay::PlaylistPicker { duplicates, .. }) = &mut view.overlay {
                        *duplicates = !*duplicates;
                        cx.notify();
                    }
                },
            ));
            if let Some(page) = page {
                let choices = page
                    .shelves
                    .iter()
                    .flat_map(|shelf| &shelf.items)
                    .filter_map(|item| match item {
                        Item::Collection(collection) if collection.details.editable => {
                            Some(collection)
                        }
                        _ => None,
                    });
                let mut list = div()
                    .id("playlist-choices")
                    .max_h(px(360.))
                    .overflow_y_scroll();
                for (index, playlist) in choices.enumerate() {
                    let id = playlist.id.clone();
                    let song = track.id.clone();
                    let duplicates = *duplicates;
                    list = list.child(self.menu_action(
                        ("playlist-choice", index),
                        playlist.title.clone(),
                        cx,
                        move |view, _, cx| {
                            view.overlay = None;
                            view.mutate(
                                Mutation::AddToPlaylist {
                                    id: id.clone(),
                                    video_ids: vec![song.clone()],
                                    duplicates,
                                },
                                cx,
                            );
                        },
                    ));
                }
                body = body.child(list);
                if let Some(token) = &page.continuation {
                    let token = token.clone();
                    body = body.child(
                        self.menu_action(
                            "more-playlist-choices",
                            if self
                                .catalog_requests
                                .is_loading(CatalogTarget::PlaylistPicker)
                            {
                                "Loading…"
                            } else {
                                "More playlists"
                            },
                            cx,
                            move |view, _, cx| {
                                if !view
                                    .catalog_requests
                                    .is_loading(CatalogTarget::PlaylistPicker)
                                {
                                    view.request(Query::PlaylistChoicesMore(token.clone()), cx);
                                }
                            },
                        ),
                    );
                }
            } else if self
                .catalog_requests
                .is_loading(CatalogTarget::PlaylistPicker)
            {
                body = body.child(
                    div()
                        .p(px(12.))
                        .text_color(self.theme.muted)
                        .child("Loading playlists…"),
                );
            } else {
                body = body.child(self.menu_action(
                    "retry-playlist-choices",
                    "Retry",
                    cx,
                    |view, _, cx| view.request(Query::PlaylistChoices, cx),
                ));
            }
            if let Some(error) = error {
                body = body.child(
                    div()
                        .mt(px(12.))
                        .text_color(rgb(0xf1a6ae))
                        .child(error.clone()),
                );
            }
        } else if let Overlay::AlreadyAdded { id, video_ids } = kind {
            body = body.child(
                div()
                    .p(px(12.))
                    .child("This song is already in the playlist."),
            );
            let id = id.clone();
            let video_ids = video_ids.clone();
            body = body.child(self.menu_action(
                "add-anyway",
                "Add anyway",
                cx,
                move |view, window, cx| {
                    if view.mutate(
                        Mutation::AddToPlaylist {
                            id: id.clone(),
                            video_ids: video_ids.clone(),
                            duplicates: true,
                        },
                        cx,
                    ) {
                        view.close_overlay(window, cx);
                    }
                },
            ));
            body = body.child(self.menu_action(
                "cancel-duplicate",
                "Cancel",
                cx,
                |view, window, cx| view.close_overlay(window, cx),
            ));
        } else if let Overlay::DeletePlaylist { id, title } = kind {
            body = body.child(div().mb(px(18.)).child(format!(
                "Delete {title}? This removes the playlist from your YouTube Music account."
            )));
            let id = id.clone();
            body = body.child(self.menu_action(
                "confirm-delete",
                "Delete playlist",
                cx,
                move |view, window, cx| {
                    if view.mutate(Mutation::DeletePlaylist { id: id.clone() }, cx) {
                        view.close_overlay(window, cx);
                    }
                },
            ));
            body = body.child(
                self.menu_action("cancel-delete", "Cancel", cx, |view, _, cx| {
                    view.overlay = None;
                    cx.notify();
                }),
            );
            if let Some(error) = &self.error {
                body = body.child(
                    div()
                        .mt(px(12.))
                        .text_color(rgb(0xf1a6ae))
                        .child(error.clone()),
                );
            }
        }
        if matches!(kind, Overlay::Track { .. } | Overlay::Collection(_)) {
            let position = self.overlay_anchor.unwrap_or_else(|| {
                point(
                    window.viewport_size().width / 2.,
                    window.viewport_size().height / 3.,
                )
            });
            return div()
                .id("menu-focus")
                .absolute()
                .inset_0()
                .occlude()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|view, _, window, cx| {
                        cx.stop_propagation();
                        view.close_overlay(window, cx);
                    }),
                )
                .child(
                    anchored()
                        .position(position)
                        .snap_to_window_with_margin(px(8.))
                        .child(
                            body.w(px(240.))
                                .px(px(0.))
                                .py(px(16.))
                                .rounded(px(2.))
                                .max_h(window.viewport_size().height - px(16.)),
                        ),
                )
                .into_any_element();
        }
        self.backdrop(body.into_any_element(), cx)
    }
}

pub(in crate::app) fn playlist_move_down(
    tracks: &[Track],
    index: usize,
    has_more: bool,
) -> Option<Option<String>> {
    tracks.get(index + 1)?;
    match tracks.get(index + 2) {
        Some(successor) => successor.details.set_video_id.clone().map(Some),
        None if !has_more => Some(None),
        None => None,
    }
}
