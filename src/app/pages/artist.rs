//! Artist hero background, heading, and artist actions.

use super::super::*;

impl Music {
    pub(in crate::app) fn artist_heading(
        &mut self,
        inset: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let header = self.page.header.clone().expect("artist page header");
        let mut actions = div().flex().items_center().flex_wrap().gap(px(8.));
        for (id, label, glyph, target) in [
            ("artist-shuffle", "Shuffle", "shuffle", header.shuffle),
            ("artist-mix", "Mix", "radio", header.mix),
        ] {
            if let Some(target) = target {
                actions = actions.child(
                    div()
                        .id(id)
                        .keyboard_control(self.theme)
                        .w(px(136.))
                        .h(px(36.))
                        .rounded_full()
                        .bg(self.theme.text)
                        .text_color(self.theme.bg)
                        .flex()
                        .items_center()
                        .justify_center()
                        .gap(px(8.))
                        .when(id == "artist-mix", |style| {
                            style
                                .bg(gpui::transparent_black())
                                .text_color(self.theme.text)
                                .border_color(self.theme.muted)
                        })
                        .font_weight(FontWeight::MEDIUM)
                        .cursor_pointer()
                        .child(icon(
                            glyph,
                            20.,
                            if id == "artist-mix" {
                                self.theme.text
                            } else {
                                self.theme.bg
                            },
                        ))
                        .child(label)
                        .on_click(cx.listener(move |view, _, _, cx| {
                            view.start_queue(PlaybackQuery::Playlist(target.clone()), cx);
                        })),
                );
            }
        }
        if let Some(subscribed) = header.subscribed {
            let label = if header.subscription_label.is_empty() {
                if subscribed {
                    "Subscribed"
                } else {
                    "Subscribe"
                }
                .to_owned()
            } else {
                header.subscription_label
            };
            let label = if header.subscriber_count.is_empty() {
                label
            } else {
                format!("{label} {}", header.subscriber_count)
            };
            actions = actions.child(
                div()
                    .id("artist-subscribe")
                    .keyboard_control(self.theme)
                    .h(px(36.))
                    .px(px(16.))
                    .rounded_full()
                    .border_1()
                    .border_color(self.theme.border)
                    .text_color(self.theme.text)
                    .flex()
                    .items_center()
                    .cursor_pointer()
                    .child(label)
                    .on_click(cx.listener(move |view, _, window, cx| {
                        window.focus(&view.focus, cx);
                        if view.account.is_none() {
                            view.connect_account(window, cx);
                            return;
                        }
                        let Route::Browse(link) = &view.route else {
                            return;
                        };
                        let collection = Collection {
                            id: link.id.clone(),
                            title: view.page.title.clone(),
                            subtitle: String::new(),
                            thumbnail: None,
                            details: Default::default(),
                        };
                        view.mutate(
                            Mutation::CollectionLibrary {
                                collection,
                                save: !subscribed,
                            },
                            cx,
                        );
                    })),
            );
        }
        div()
            .w_full()
            .h(px(500.))
            .px(px(inset))
            .pb(px(32.))
            .flex()
            .flex_col()
            .justify_end()
            .gap(px(16.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .child(
                        div()
                            .text_size(px(34.))
                            .line_height(px(38.))
                            .font_weight(FontWeight::BOLD)
                            .child(self.page.title.clone()),
                    )
                    .when(!header.subtitle.is_empty(), |d| {
                        d.child(
                            div()
                                .text_size(px(16.))
                                .line_height(relative(1.25))
                                .text_color(self.theme.muted)
                                .child(header.subtitle),
                        )
                    }),
            )
            .when(!self.page.description.is_empty(), |d| {
                d.child(
                    div()
                        .id("artist-description")
                        .keyboard_control(self.theme)
                        .max_w(px(640.))
                        .line_clamp(2)
                        .text_ellipsis()
                        .max_h(px(40.))
                        .overflow_hidden()
                        .text_size(px(14.))
                        .line_height(px(20.))
                        .cursor_pointer()
                        .child(self.page.description.clone())
                        .on_click(cx.listener(|view, _, window, cx| {
                            window.focus(&view.focus, cx);
                            view.overlay = Some(Overlay::Description {
                                title: view.page.title.clone(),
                                text: view.page.description.clone(),
                            });
                            cx.notify();
                        })),
                )
            })
            .child(actions)
            .into_any_element()
    }
}

impl Music {
    pub(in crate::app) fn artist_background(&mut self, compact: bool) -> Option<Div> {
        let artist_top = self.detail_list.state.logical_scroll_top();
        let artist_art = if self.route.is_artist() && !self.player_open && artist_top.item_ix == 0 {
            let url = self
                .page
                .header
                .as_ref()
                .and_then(|header| header.thumbnail.as_deref());
            url.and_then(|url| self.artwork.get_hero(url))
        } else {
            None
        };
        artist_art.map(|image| {
            let offset = -artist_top.offset_in_item;
            div()
                .absolute()
                .left(px(if compact { 72. } else { 240. }))
                .right_0()
                .top(offset)
                .h(px(564.))
                .overflow_hidden()
                .child(img(image).size_full().object_fit(ObjectFit::Cover))
                .child(div().absolute().inset_0().bg(linear_gradient(
                    180.,
                    linear_color_stop(rgba(0x03030388), 0.),
                    linear_color_stop(self.theme.bg, 1.),
                )))
        })
    }
}
