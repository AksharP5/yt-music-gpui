//! Artist hero background, heading, and artist actions.

use super::super::*;
use super::{PageLayout, Tier};
use crate::catalog::CollectionDetails;

impl Music {
    pub(in crate::app) fn artist_heading(
        &mut self,
        inset: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let header = self.page.header.clone().expect("artist page header");
        let (_, top) = hero_lines(self.page_layout(window).tier);
        let Route::Browse(link) = &self.route else {
            unreachable!("artist pages are browse routes");
        };
        let artist = Collection {
            id: link.id.clone(),
            title: self.page.title.clone(),
            subtitle: header.subtitle.clone(),
            thumbnail: header.thumbnail.clone(),
            details: CollectionDetails {
                shuffle: header.shuffle.clone(),
                mix: header.mix.clone(),
                saved: header.subscribed,
                ..Default::default()
            },
        };
        let expanded = window.use_keyed_state(
            SharedString::from(format!("artist-description-{}", link.id)),
            cx,
            |_, _| false,
        );
        let mut actions = div()
            .mt(px(24.))
            .flex()
            .items_center()
            .flex_wrap()
            .gap(px(8.));
        for (id, label, glyph, target) in [
            ("artist-shuffle", "Shuffle", "shuffle", header.shuffle),
            ("artist-mix", "Mix", "mix", header.mix),
        ] {
            if let Some(target) = target {
                actions = actions.child(
                    div()
                        .id(id)
                        .keyboard_control(self.theme)
                        .w(px(136.))
                        .h(px(36.))
                        .rounded_full()
                        .bg(rgb(0xf1f1f1))
                        .hover(|style| style.bg(rgb(0xd9d9d9)))
                        .text_color(rgb(0x0f0f0f))
                        .flex()
                        .items_center()
                        .justify_center()
                        .gap(px(8.))
                        .font_weight(FontWeight::MEDIUM)
                        .cursor_pointer()
                        .child(icon(glyph, 24., rgb(0x0f0f0f).into()))
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
            let accent: Hsla = rgb(0xff5577).into();
            actions = actions.child(
                div()
                    .id("artist-subscribe")
                    .keyboard_control(self.theme)
                    .h(px(36.))
                    .px(px(16.))
                    .rounded_full()
                    .border_1()
                    .border_color(if subscribed { self.theme.muted } else { accent })
                    .text_color(if subscribed { self.theme.muted } else { accent })
                    .font_weight(FontWeight::MEDIUM)
                    .hover(|style| style.bg(gpui::white().opacity(0.1)))
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
        actions = actions.child(
            self.menu_button("artist-menu", cx, move |view, window, cx| {
                view.open_collection_menu(artist.clone(), window, cx);
            }),
        );
        let open = *expanded.read(cx);
        div()
            .w_full()
            .px(px(inset))
            .pt(px(top))
            .pb(px(32.))
            .flex()
            .flex_col()
            .child(
                // Unlike Home and album pages, the web keeps this title at 28px.
                div()
                    .text_size(px(28.))
                    .line_height(relative(1.2))
                    .font_weight(FontWeight::BOLD)
                    .child(self.page.title.clone()),
            )
            .when(!header.subtitle.is_empty(), |d| {
                d.child(
                    div()
                        .text_size(px(14.))
                        .line_height(relative(1.2))
                        .text_color(self.theme.muted)
                        .child(header.subtitle),
                )
            })
            .when(!self.page.description.is_empty(), |d| {
                d.child(
                    div()
                        .mt(px(16.))
                        .max_w(px(640.))
                        .text_size(px(14.))
                        .line_height(px(20.))
                        .when(!open, |text| text.line_clamp(2).text_ellipsis())
                        .child(self.page.description.clone()),
                )
                .child(
                    div()
                        .id("artist-description-more")
                        .keyboard_control(self.theme)
                        .mt(px(8.))
                        .text_size(px(14.))
                        .line_height(px(20.))
                        .font_weight(FontWeight::MEDIUM)
                        .cursor_pointer()
                        .child(if open { "LESS" } else { "MORE" })
                        .on_click(move |_, _, cx| {
                            expanded.update(cx, |open, cx| {
                                *open = !*open;
                                cx.notify();
                            });
                        }),
                )
            })
            .child(actions)
            .into_any_element()
    }
}

impl Music {
    /// Album, playlist and podcast pages sit on a blurred wash of their art.
    fn wash_url(&self) -> Option<String> {
        if self.route.is_artist() || self.page_collection().is_none() {
            return None;
        }
        self.page.header.as_ref()?.thumbnail.clone()
    }

    /// Artist and collection pages draw behind a transparent header.
    pub(in crate::app) fn immersive_header(&self) -> bool {
        !self.player_open && (self.route.is_artist() || self.wash_url().is_some())
    }

    /// The artist hero or collection wash behind the header and page top. Both
    /// scroll away with the page, as on the web.
    pub(in crate::app) fn page_background(
        &mut self,
        compact: bool,
        width: f32,
        height: f32,
    ) -> Option<Div> {
        if self.player_open {
            return None;
        }
        let left = if compact { 72. } else { 240. };
        let scrolled = f32::from(self.detail_list.state.scroll_px_offset_for_scrollbar().y);
        if let Some(url) = self.wash_url() {
            let image = self.artwork.get_wash(&url)?;
            // The web blurs the art over the top 49vh and fades it out by 50vh.
            return Some(
                div()
                    .absolute()
                    .left(px(left))
                    .right_0()
                    .top_0()
                    .h(px(height * 0.5))
                    .overflow_hidden()
                    .child(
                        img(image)
                            .absolute()
                            .inset_0()
                            .size_full()
                            .object_fit(ObjectFit::Cover),
                    )
                    .child(div().absolute().inset_0().bg(linear_gradient(
                        180.,
                        linear_color_stop(rgba(0x00000099), 0.),
                        linear_color_stop(self.theme.bg, 1.),
                    ))),
            );
        }
        if !self.route.is_artist() {
            return None;
        }
        // The hero spans the header and the artist heading in list row 0.
        let hero = 64. + f32::from(self.detail_list.state.bounds_for_item(0)?.size.height);
        if scrolled <= -hero {
            return None;
        }
        let url = self.page.header.as_ref()?.thumbnail.clone()?;
        let image = self.artwork.get_hero(&url)?;
        // The web lays the photo window-wide from 72px left of the content,
        // keeps its top edge, dims it by 40%, and fades it out behind the heading.
        let size = image.size(0);
        let natural = width * size.height.0 as f32 / size.width.0.max(1) as f32;
        let (fade, _) = hero_lines(PageLayout::new(width, width - left).tier);
        Some(
            div()
                .absolute()
                .left(px(left))
                .right_0()
                .top(px(scrolled))
                .h(px(hero))
                .overflow_hidden()
                .child(
                    img(image)
                        .absolute()
                        .top_0()
                        .left(px(-72.))
                        .w(px(width))
                        .h(px(natural.max(hero)))
                        .object_fit(ObjectFit::Cover),
                )
                .child(div().absolute().inset_0().bg(rgba(0x00000066)))
                .child(
                    div()
                        .absolute()
                        .left_0()
                        .right_0()
                        .top(px(fade))
                        .bottom_0()
                        .bg(linear_gradient(
                            180.,
                            linear_color_stop(rgba(0x03030300), 0.),
                            linear_color_stop(self.theme.bg, 0.91),
                        )),
                ),
        )
    }
}

/// Where the web's artist photo starts fading, from the window top, and how
/// far its title sits below the header, by window tier.
fn hero_lines(tier: Tier) -> (f32, f32) {
    match tier {
        Tier::Narrow => (128., 102.),
        Tier::Medium => (168., 152.),
        Tier::Wide => (228., 228.),
        Tier::Large => (288., 303.),
        Tier::Huge => (368., 405.),
    }
}
