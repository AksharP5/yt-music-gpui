//! Overlay state, menus/dialogs, and picker/form lifecycle.

pub(in crate::app) mod account;
pub(in crate::app) mod settings;

use super::*;

// Music holds a single overlay, so the inline song in `Track` costs nothing worth a box.
#[allow(clippy::large_enum_variant)]
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
        /// One song from its menu, or the whole queue from Up next's Save.
        tracks: Vec<Track>,
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
    /// Sign-in worked, but no keyring can save it.
    NoKeyring(Arc<Session>),
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
        initial_video_ids: Vec<String>,
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
                            initial_video_ids: initial_video_ids.clone(),
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

    /// Save to playlist lists the guide's playlists, which are already loaded and kept in
    /// sync, so it opens at once instead of waiting on another library read.
    pub(in crate::app) fn playlist_picker(&mut self, tracks: Vec<Track>, cx: &mut Context<Self>) {
        self.error = None;
        self.overlay = Some(Overlay::PlaylistPicker { tracks });
        if self.sidebar_playlists.is_empty()
            && !self.catalog_requests.is_loading(CatalogTarget::Sidebar)
        {
            self.request(Query::SidebarPlaylists, cx);
        }
        cx.notify();
    }

    /// The covers the Save to playlist dialog shows first, warmed while its menu is open.
    fn prefetch_playlist_covers(&mut self) {
        for url in self
            .sidebar_playlists
            .iter()
            .filter(|playlist| playlist.details.editable)
            .filter_map(|playlist| playlist.thumbnail.as_deref())
            .take(PICKER_PREFETCH)
        {
            self.artwork
                .get(url, size(px(PICKER_COVER), px(PICKER_COVER)));
        }
    }

    /// The toast YouTube Music shows above the player bar, replacing any toast still showing.
    pub(in crate::app) fn show_toast(
        &mut self,
        text: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        let id = self
            .toast
            .as_ref()
            .map_or(0, |toast| toast.id.wrapping_add(1));
        let dismiss = cx.spawn(async move |view, cx| {
            cx.background_executor().timer(TOAST_DURATION).await;
            let _ = view.update(cx, |view, cx| {
                view.toast = None;
                cx.notify();
            });
        });
        self.toast = Some(Toast {
            id,
            text: text.into(),
            _dismiss: dismiss,
        });
        cx.notify();
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
        self.overlay_button = None;
        if self.account.is_some() {
            self.prefetch_playlist_covers();
        }
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
        self.overlay_button = None;
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
        self.song_text.cancel();
        self.overlay = None;
        self.settings_menu = None;
        self.overlay_anchor = None;
        self.overlay_button = None;
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
            TextQuery::Comments(_) => "Comments",
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
    ) -> AnyElement {
        let id = id.into();
        let glyph = match &id {
            ElementId::Name(name) => match name.as_ref() {
                "menu-radio" => Some("menu-mix"),
                "menu-shuffle" => Some("shuffle"),
                "collection-play" => Some("play"),
                "menu-next" => Some("menu-play-next"),
                "menu-enqueue" => Some("menu-add-queue"),
                "menu-library" => Some("menu-library"),
                "menu-like" => Some("menu-like"),
                "menu-playlist" => Some("menu-save-playlist"),
                "menu-remove-queue" => Some("menu-remove"),
                "menu-history" | "menu-remove-occurrence" => Some("menu-delete"),
                "menu-move-up" => Some("menu-up"),
                "menu-move-down" => Some("menu-down"),
                "menu-album" => Some("menu-album"),
                "menu-artist" | "menu-show" => Some("menu-artist"),
                "menu-credits" => Some("menu-credits"),
                "menu-share" => Some("menu-share"),
                "menu-episode" | "menu-chapters" => Some("description"),
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
            .into_any_element()
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
            // The window renders the account popup itself, with its artwork.
            Overlay::Account => return div().into_any_element(),
            Overlay::SignIn => return self.sign_in_dialog(cx),
            Overlay::Settings => return self.settings_dialog(window, cx),
            Overlay::PlaylistPicker { .. } => {
                return self.playlist_picker_dialog(window, cx);
            }
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
                                Overlay::Settings
                                | Overlay::Account
                                | Overlay::SignIn
                                | Overlay::PlaylistPicker { .. } => {
                                    // Rendered by their own dialogs above.
                                    String::new()
                                }
                                Overlay::Description { .. } => "Description".into(),
                                Overlay::Chapters(_) => "Chapters".into(),
                                Overlay::Track { track, .. } => track.title.clone(),
                                Overlay::Collection(collection) => collection.title.clone(),
                                Overlay::AlreadyAdded { .. } => "Already added".into(),
                                Overlay::DeletePlaylist { .. } => "Delete playlist?".into(),
                                Overlay::NoKeyring(_) => "No keyring found".into(),
                                Overlay::SongText { title, .. } => title.clone(),
                                Overlay::PlaylistForm { .. } | Overlay::CountryPicker(_) => {
                                    String::new()
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
        let mut actions = Vec::new();
        let mut menu_extra = 0.;
        if matches!(kind, Overlay::Help) {
            for &(key, action) in super::interaction::SHORTCUTS {
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
                            .mx(px(16.))
                            .mb(px(16.))
                            .text_color(self.theme.muted)
                            .child(episode.subtitle.clone()),
                    );
                    menu_extra += 36.;
                }
                let target = BrowseLink::new(format!("MPED{}", track.id), track.title.clone());
                actions.push(self.menu_action(
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
                    actions.push(self.menu_action(
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
                    actions.push(self.menu_action(
                        "menu-show",
                        "Go to podcast",
                        cx,
                        move |view, _, cx| {
                            view.overlay = None;
                            view.navigate(Route::Browse(target.clone()), cx);
                        },
                    ));
                }
            } else {
                let song = track.clone();
                actions.push(self.menu_action(
                    "menu-radio",
                    "Start mix",
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
            let song = track.clone();
            actions.push(
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
            actions.push(self.menu_action(
                "menu-enqueue",
                "Add to queue",
                cx,
                move |view, _, cx| {
                    view.overlay = None;
                    view.enqueue(song.clone(), cx);
                },
            ));
            if self.account.is_some()
                && (track.details.add_library.is_some() || track.details.remove_library.is_some())
            {
                let song = track.clone();
                let save = track.details.in_library != Some(true);
                actions.push(self.menu_action(
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
            // Page rows have their own like buttons, so only the player bar and Up next
            // menus offer this, as on the web.
            if index.is_none() {
                let song = track.clone();
                actions.push(self.menu_action(
                    "menu-like",
                    if self.is_liked(track) {
                        "Remove from liked songs"
                    } else {
                        "Add to liked songs"
                    },
                    cx,
                    move |view, _, cx| {
                        view.overlay = None;
                        view.toggle_like(&song, cx);
                    },
                ));
            }
            let song = track.clone();
            actions.push(self.menu_action(
                "menu-playlist",
                "Save to playlist",
                cx,
                move |view, window, cx| {
                    if view.account.is_none() {
                        view.overlay = None;
                        view.connect_account(window, cx);
                        return;
                    }
                    view.playlist_picker(vec![song.clone()], cx);
                },
            ));
            if let Some(queued) = queue_index.filter(|&queued| {
                self.library
                    .queue
                    .tracks
                    .get(queued)
                    .is_some_and(|entry| entry.id == track.id)
            }) {
                actions.push(self.menu_action(
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
                if let Some(token) = &track.details.remove_history {
                    let token = token.clone();
                    actions.push(self.menu_action(
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
                    actions.push(self.menu_action(
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
                                actions.push(self.menu_action(
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
                    "menu-album",
                    "Go to album",
                    "Album",
                    &track.details.album_id,
                ),
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
            ] {
                if let Some(target) = target {
                    let target = target.clone();
                    actions.push(self.menu_action(id, label, cx, move |view, _, cx| {
                        view.overlay = None;
                        view.navigate(Route::Browse(BrowseLink::new(target.clone(), title)), cx);
                    }));
                }
            }
            if let Some(id) = track.credits_id() {
                let title = track.title.clone();
                actions.push(self.menu_action(
                    "menu-credits",
                    "View song credits",
                    cx,
                    move |view, _, cx| {
                        view.show_song_text(TextQuery::Credits(id.clone()), &title, cx)
                    },
                ));
            }
            let url = format!("https://music.youtube.com/watch?v={}", track.id);
            actions.push(
                self.menu_action("menu-share", "Share", cx, move |view, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(url.clone()));
                    view.overlay = None;
                    view.notice = Some("Link copied".into());
                    cx.notify();
                }),
            );
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
                    actions.push(self.menu_action(id, label, cx, move |view, window, cx| {
                        view.close_overlay(window, cx);
                        view.start_queue(PlaybackQuery::Playlist(target.clone()), cx);
                    }));
                }
            }
            if let Some(target) = collection.details.play.clone() {
                actions.push(self.menu_action(
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
            actions.push(self.menu_action(
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
            actions.push(
                self.menu_action("menu-share", "Share", cx, move |view, window, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(url.clone()));
                    view.close_overlay(window, cx);
                    view.notice = Some("Link copied".into());
                }),
            );
        } else if let Overlay::SongText { content, .. } = kind {
            match content {
                None => body = body.child(div().text_color(self.theme.muted).child("Loading…")),
                Some(Err(error)) => {
                    body = body.child(div().text_color(self.theme.muted).child(error.clone()))
                }
                Some(Ok(SongText::Lyrics(lyrics))) => {
                    body = body.child(div().text_size(px(17.)).child(lyrics.text.clone()))
                }
                Some(Ok(SongText::Related(_) | SongText::Comments(_))) => {}
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
        } else if let Overlay::NoKeyring(session) = kind {
            body = body
                .child(div().mb(px(12.)).child(
                    "You're signed in until you quit. To stay signed in, install and start a keyring such as GNOME Keyring, then sign in again.",
                ))
                .child(div().mb(px(18.)).text_color(self.theme.muted).child(
                    "Or save your session unencrypted in a file only your user can read.",
                ));
            let session = session.clone();
            body = body.child(self.menu_action(
                "save-without-keyring",
                "Save without a keyring",
                cx,
                move |view, window, cx| {
                    if view
                        .account_commands
                        .try_send(crate::account::Command::SaveWithoutKeyring(session.clone()))
                        .is_err()
                    {
                        view.error = Some("Could not save the account. Try again.".into());
                    }
                    view.close_overlay(window, cx);
                },
            ));
            body = body.child(self.menu_action(
                "not-now-keyring",
                "Not now",
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
            let viewport = window.viewport_size();
            let menu = size(
                px(MENU_WIDTH),
                px(34. + 48. * actions.len() as f32 + menu_extra),
            );
            let position = match (self.overlay_button, self.overlay_anchor) {
                (Some(button), _) => menu_origin(button, menu, viewport),
                (None, Some(pointer)) => pointer,
                (None, None) => point(viewport.width / 2., viewport.height / 3.),
            };
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
                            body.children(actions)
                                .w(px(MENU_WIDTH))
                                .px(px(0.))
                                .py(px(16.))
                                .rounded(px(2.))
                                .max_h(viewport.height - px(16.)),
                        ),
                )
                .into_any_element();
        }
        self.backdrop(body.into_any_element(), cx)
    }
}

impl Music {
    /// YouTube Music's Save to playlist dialog: covers, titles and bylines under "All
    /// playlists", with New playlist floating over the list's end.
    fn playlist_picker_dialog(&self, window: &Window, cx: &Context<Self>) -> AnyElement {
        let theme = self.theme;
        let divider = gpui::white().opacity(0.1);
        let count = picker_choices(&self.sidebar_playlists).count();
        let viewport: f32 = window.viewport_size().height.into();
        let list_height = (count as f32 * PICKER_ROW + PICKER_LIST_TOP + PICKER_ROW)
            .min(viewport * 0.75 - PICKER_HEADING);
        let mut body = div()
            .id("overlay-body")
            .relative()
            .w(px(386.))
            .flex()
            .flex_col()
            .bg(theme.panel)
            .border_1()
            .border_color(divider)
            .rounded(px(2.))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .min_h(px(64.))
                    .pl(px(24.))
                    .flex()
                    .items_center()
                    .justify_between()
                    .border_b_1()
                    .border_color(divider)
                    .child(
                        div()
                            .text_size(px(24.))
                            .line_height(px(29.))
                            .font_weight(FontWeight::BOLD)
                            .child("Save to playlist"),
                    )
                    .child(
                        tinted_button("close-overlay", "close", rgb(0xaaaaaa), theme)
                            .mr(px(17.))
                            .on_click(
                                cx.listener(|view, _, window, cx| view.close_overlay(window, cx)),
                            ),
                    ),
            );
        if count > 0 {
            body = body
                .child(
                    div()
                        .mt(px(20.))
                        .ml(px(24.))
                        .text_size(px(14.))
                        .line_height(px(17.))
                        .font_weight(FontWeight::MEDIUM)
                        .child("All playlists"),
                )
                .child(
                    uniform_list(
                        "playlist-choices",
                        count,
                        cx.processor(|view, range: std::ops::Range<usize>, _, cx| {
                            let choices: Vec<usize> =
                                picker_choices(&view.sidebar_playlists).collect();
                            if range.end.saturating_add(5) >= choices.len()
                                && !view.catalog_requests.is_loading(CatalogTarget::Sidebar)
                                && view.sidebar_error.is_none()
                                && let Some(token) = view.sync.sidebar_continuation.clone()
                            {
                                view.request(Query::SidebarMore(token), cx);
                            }
                            range
                                .filter_map(|index| {
                                    choices.get(index).map(|&choice| (index, choice))
                                })
                                .map(|(index, choice)| {
                                    let playlist = &view.sidebar_playlists[choice];
                                    let cover = playlist.thumbnail.as_deref().and_then(|url| {
                                        view.artwork
                                            .get(url, size(px(PICKER_COVER), px(PICKER_COVER)))
                                    });
                                    let id = playlist.id.clone();
                                    div()
                                        .id(("playlist-choice", index))
                                        .keyboard_control(view.theme)
                                        .w_full()
                                        .h(px(PICKER_ROW))
                                        .px(px(24.))
                                        .flex()
                                        .items_center()
                                        .cursor_pointer()
                                        .hover(|row| row.bg(gpui::white().opacity(0.05)))
                                        .child(
                                            div()
                                                .size(px(PICKER_COVER))
                                                .mr(px(16.))
                                                .flex_shrink_0()
                                                .rounded(px(2.))
                                                .overflow_hidden()
                                                .bg(view.theme.raised)
                                                .children(cover.map(|image| {
                                                    img(image)
                                                        .size_full()
                                                        .object_fit(ObjectFit::Cover)
                                                })),
                                        )
                                        .child(
                                            div()
                                                .flex_1()
                                                .min_w_0()
                                                .flex()
                                                .flex_col()
                                                .gap(px(4.))
                                                .text_size(px(14.))
                                                .line_height(px(20.))
                                                .child(
                                                    div()
                                                        .truncate()
                                                        .font_weight(FontWeight::MEDIUM)
                                                        .child(playlist.title.clone()),
                                                )
                                                .child(
                                                    div()
                                                        .truncate()
                                                        .text_color(gpui::white().opacity(0.7))
                                                        .child(playlist.subtitle.clone()),
                                                ),
                                        )
                                        .on_click(cx.listener(move |view, _, window, cx| {
                                            let Some(Overlay::PlaylistPicker { tracks }) =
                                                &view.overlay
                                            else {
                                                return;
                                            };
                                            let video_ids = tracks
                                                .iter()
                                                .map(|track| track.id.clone())
                                                .collect();
                                            view.close_overlay(window, cx);
                                            view.mutate(
                                                Mutation::AddToPlaylist {
                                                    id: id.clone(),
                                                    video_ids,
                                                    duplicates: false,
                                                },
                                                cx,
                                            );
                                        }))
                                        .into_any_element()
                                })
                                .collect::<Vec<_>>()
                        }),
                    )
                    .w_full()
                    .h(px(list_height))
                    .pt(px(PICKER_LIST_TOP))
                    .pb(px(PICKER_ROW)),
                );
        } else {
            body = body.child(div().h(px(PICKER_ROW)));
        }
        if let Some(error) = &self.sidebar_error {
            body = body.child(
                div()
                    .px(px(24.))
                    .pb(px(PICKER_ROW))
                    .text_color(theme.muted)
                    .child(error.clone())
                    .child(self.menu_action(
                        "retry-playlist-choices",
                        "Retry",
                        cx,
                        |view, _, cx| view.request(Query::SidebarPlaylists, cx),
                    )),
            );
        }
        body = body.child(
            div()
                .id("create-from-picker")
                .keyboard_control(theme)
                .absolute()
                .bottom(px(17.))
                .right(px(17.))
                .h(px(36.))
                .pl(px(12.))
                .pr(px(16.))
                .flex()
                .items_center()
                .gap(px(6.))
                .rounded_full()
                .bg(rgb(0xf1f1f1))
                .text_color(rgb(0x0f0f0f))
                .text_size(px(14.))
                .font_weight(FontWeight::MEDIUM)
                .cursor_pointer()
                .hover(|button| button.bg(rgb(0xd9d9d9)))
                .child(icon("plus", 24., rgb(0x0f0f0f).into()))
                .child("New playlist")
                .on_click(cx.listener(|view, _, window, cx| {
                    let Some(Overlay::PlaylistPicker { tracks }) = &view.overlay else {
                        return;
                    };
                    let video_ids = tracks.iter().map(|track| track.id.clone()).collect();
                    view.playlist_form(false, video_ids, window, cx);
                })),
        );
        self.backdrop(body.into_any_element(), cx)
    }
}

pub(in crate::app) struct Toast {
    id: u64,
    pub(in crate::app) text: SharedString,
    _dismiss: Task<()>,
}

impl Toast {
    /// YouTube Music's light toast, 16 px from the left and above the player bar, rising
    /// into place over 300 ms like the web's paper toast.
    pub(in crate::app) fn render(&self, bottom: f32, width: f32) -> impl IntoElement {
        div()
            .absolute()
            .left(px(16.))
            .min_w(px(288.))
            .max_w(px(width - 32.))
            .min_h(px(48.))
            .p(px(12.))
            .flex()
            .items_center()
            .rounded(px(8.))
            .bg(rgb(0xf1f1f1))
            .shadow(vec![
                BoxShadow::new(px(0.), px(0.), gpui::black().opacity(0.1)).blur_radius(px(24.)),
            ])
            .text_color(rgb(0x0f0f0f))
            .text_size(px(14.))
            .line_height(px(20.))
            .child(self.text.clone())
            .with_animation(
                ElementId::NamedInteger("toast".into(), self.id),
                Animation::new(Duration::from_millis(300)).with_easing(ease_in_out),
                move |toast, delta| {
                    toast
                        .opacity(delta)
                        .bottom(px(bottom - 100. * (1. - delta)))
                },
            )
    }
}

/// On music.youtube.com a toast without an action button closes after four seconds.
const TOAST_DURATION: Duration = Duration::from_secs(4);
/// The web's `--ytmusic-playlist-item-height`.
const PICKER_ROW: f32 = 64.;
const PICKER_COVER: f32 = 44.;
/// The gap above the first playlist.
const PICKER_LIST_TOP: f32 = 7.;
/// The "All playlists" heading with its top margin.
const PICKER_HEADING: f32 = 37.;
/// Enough covers to fill the dialog's first screen.
const PICKER_PREFETCH: usize = 8;

/// Indices of the guide playlists a song can be saved to.
fn picker_choices(playlists: &[Collection]) -> impl Iterator<Item = usize> + '_ {
    playlists
        .iter()
        .enumerate()
        .filter(|(_, playlist)| playlist.details.editable)
        .map(|(index, _)| index)
}

/// The song menu's width on music.youtube.com, including its 1 px border.
const MENU_WIDTH: f32 = 242.;

/// Place a menu against the button that opened it, as music.youtube.com does: below and
/// left-aligned, aligned to the button's right edge when that would overflow, and above
/// when it does not fit below.
fn menu_origin(
    button: Bounds<Pixels>,
    menu: Size<Pixels>,
    viewport: Size<Pixels>,
) -> Point<Pixels> {
    let margin = px(8.);
    let x = if button.left() + menu.width <= viewport.width - margin {
        button.left()
    } else {
        button.right() - menu.width
    };
    let below = button.bottom() + margin;
    let above = button.top() - margin - menu.height;
    let y = if below + menu.height <= viewport.height - margin || above < margin {
        below
    } else {
        above
    };
    point(x, y)
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

#[cfg(test)]
mod tests {
    use super::{MENU_WIDTH, menu_origin};
    use gpui::{Bounds, point, px, size};

    #[test]
    fn menus_open_against_their_button_like_the_web() {
        let viewport = size(px(1280.), px(800.));
        let menu = size(px(MENU_WIDTH), px(514.));
        let button = |x: f32, y: f32| Bounds::new(point(px(x), px(y)), size(px(36.), px(36.)));
        // Room below and to the right: below, left edges aligned.
        assert_eq!(
            menu_origin(button(400., 100.), menu, viewport),
            point(px(400.), px(144.))
        );
        // A row button near the right edge: right edges aligned, as on album pages.
        assert_eq!(
            menu_origin(button(1091., 220.), menu, viewport),
            point(px(1127. - MENU_WIDTH), px(264.))
        );
        // The player bar's button: above, ending 8 px over it.
        assert_eq!(
            menu_origin(button(825., 746.), menu, viewport),
            point(px(825.), px(746. - 8. - 514.))
        );
    }
}
