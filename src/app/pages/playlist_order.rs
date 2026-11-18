//! Drag playlist occurrences through the same confirmed edit path as row menus.

use super::*;

#[derive(Clone, Copy, PartialEq, Eq)]
struct PlaylistVisit {
    list_id: usize,
    account_revision: u64,
    account_generation: u64,
}

impl PlaylistVisit {
    fn current(view: &Music) -> Self {
        Self {
            list_id: view.list_id,
            account_revision: view.account_revision,
            account_generation: view.account_generation,
        }
    }
}

struct PlaylistDrag {
    visit: PlaylistVisit,
    occurrence: Arc<str>,
    title: SharedString,
}

struct DraggedSong {
    title: SharedString,
    theme: Theme,
}

impl Render for DraggedSong {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .max_w(px(260.))
            .px(px(12.))
            .py(px(8.))
            .rounded(px(6.))
            .bg(self.theme.raised)
            .text_color(self.theme.text)
            .text_size(px(14.))
            .truncate()
            .child(self.title.clone())
    }
}

impl Music {
    pub(super) fn playlist_order_controls(
        &self,
        row: Stateful<Div>,
        index: usize,
        track: &Track,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let Some(occurrence) = track.details.set_video_id.as_deref() else {
            return row;
        };
        if self.editable_playlist().is_none() {
            return row;
        }

        let theme = self.theme;
        let visit = PlaylistVisit::current(self);
        let occurrence: Arc<str> = occurrence.into();
        let drag = PlaylistDrag {
            visit,
            occurrence: occurrence.clone(),
            title: track.title.clone().into(),
        };
        row.child(
            div()
                .id("playlist-drag-grip")
                .w(px(24.))
                .h(px(32.))
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .cursor_grab()
                .opacity(0.55)
                .group_hover("music-row", |style| style.opacity(1.))
                .child(icon("drag-handle", 18., theme.text))
                .on_click(|_, _, cx| cx.stop_propagation())
                .on_drag(drag, move |song, _, _, cx| {
                    cx.new(|_| DraggedSong {
                        title: song.title.clone(),
                        theme,
                    })
                }),
        )
        .children(
            [false, true]
                .into_iter()
                .filter(|&after| !after || self.playlist_successor(index).is_some())
                .map(|after| {
                    let target = occurrence.clone();
                    let highlight = target.clone();
                    div()
                        .absolute()
                        .left_0()
                        .right_0()
                        .h(relative(0.5))
                        .when(after, |zone| zone.bottom_0())
                        .when(!after, |zone| zone.top_0())
                        .drag_over::<PlaylistDrag>(move |style, song, _, _| {
                            if song.visit != visit || song.occurrence == highlight {
                                return style;
                            }
                            let line = if after {
                                style.border_b_2()
                            } else {
                                style.border_t_2()
                            };
                            line.border_color(theme.text)
                        })
                        .on_drop(cx.listener(move |view, song: &PlaylistDrag, _, cx| {
                            view.drop_playlist_song(song, &target, after, cx);
                        }))
                }),
        )
    }

    fn editable_playlist(&self) -> Option<&BrowseLink> {
        match &self.route {
            Route::Browse(link)
                if link.id.starts_with("VL")
                    && self.page.editable
                    && self.account.is_some()
                    && self.account_operation.is_none() =>
            {
                Some(link)
            }
            _ => None,
        }
    }

    fn drop_playlist_song(
        &mut self,
        source: &PlaylistDrag,
        target: &str,
        after: bool,
        cx: &mut Context<Self>,
    ) {
        if source.visit != PlaylistVisit::current(self)
            || source.occurrence.as_ref() == target
            || self.overlay.is_some()
            || self.player_open
        {
            return;
        }
        let Some(playlist) = self.editable_playlist() else {
            return;
        };
        let position = |id: &str| {
            self.rows.iter().position(
                |row| matches!(row, Item::Track(track) if track.details.set_video_id.as_deref() == Some(id)),
            )
        };
        let (Some(from), Some(to)) = (position(&source.occurrence), position(target)) else {
            return;
        };
        let before = if after {
            let Some(before) = self.playlist_successor(to) else {
                return;
            };
            before
        } else {
            Some(target)
        };
        if before == Some(source.occurrence.as_ref())
            || self.playlist_successor(from) == Some(before)
        {
            return;
        }
        self.mutate(
            Mutation::MoveInPlaylist {
                id: playlist.id.clone(),
                set_video_id: source.occurrence.to_string(),
                before: before.map(str::to_owned),
            },
            cx,
        );
    }

    // No loaded successor is usable when the next row has no occurrence ID
    // or the response may have more songs. A complete list can append at the end.
    fn playlist_successor(&self, index: usize) -> Option<Option<&str>> {
        for item in &self.rows[index + 1..] {
            if let Item::Track(track) = item {
                return track.details.set_video_id.as_deref().map(Some);
            }
        }
        self.page.continuation.is_none().then_some(None)
    }
}
