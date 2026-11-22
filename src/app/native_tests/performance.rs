//! Release-mode carousel measurements through presented native frames.

use super::navigation::check_cache_window;
use super::render::{capture, wait_for_presented_frame};
use crate::app::Route;
use crate::catalog::{BrowseLink, Item, Shelf, ShelfLayout};
use gpui::{AppContext, PlatformInput, ScrollDelta, ScrollWheelEvent, TouchPhase, point, px, size};
use std::time::Duration;

fn cards(count: usize) -> Shelf {
    Shelf {
        strapline: String::new(),
        title: "Albums".into(),
        layout: ShelfLayout::Cards,
        more: None,
        items: (0..count)
            .map(|i| {
                Item::Collection(
                    serde_json::from_value(serde_json::json!({
                        "id": format!("MPREcarousel{i}"),
                        "title": if i + 1 == count {
                            format!("A longer offscreen album title that wraps onto two lines {i}")
                        } else {
                            format!("Album {i}")
                        },
                        "subtitle": format!("Artist {i} • 2026 • Album")
                    }))
                    .unwrap(),
                )
            })
            .collect(),
    }
}

#[test]
#[ignore = "requires native X11 desktop and private runtime; run --release for timings"]
fn carousel_render_work_and_height_stay_bounded_while_scrolling() -> anyhow::Result<()> {
    check_cache_window(async |window, cx| {
        for count in [20, 100, 500] {
            window
                .update(cx, |view, _, cx| {
                    view.route = Route::Browse(BrowseLink::new("UCcarousel", "Carousel"));
                    view.page.title = "Carousel measurement".into();
                    view.page.header = None;
                    view.page.continuation = None;
                    view.page.shelves = vec![cards(count), cards(1)];
                    view.shelf_scroll.clear();
                    view.shelf_render_times.clear();
                    view.rebuild_rows();
                    cx.notify();
                })
                .unwrap();
            wait_for_presented_frame(window, cx).await;
            let height = window
                .update(cx, |view, window, _| {
                    let height = view
                        .detail_list
                        .state
                        .bounds_for_item(0)
                        .unwrap()
                        .size
                        .height;
                    capture(window, &format!("carousel-{count}-start"));
                    let cold = std::mem::take(&mut view.shelf_render_times);
                    println!(
                        "CAROUSEL_COLD count={count} samples={} max_us={:.1}",
                        cold.len(),
                        cold.iter().max().unwrap().as_secs_f64() * 1e6
                    );
                    height
                })
                .unwrap();

            for frame in 0..40 {
                window
                    .update(cx, |view, _, cx| {
                        let scroll = &view.shelf_scroll[&0];
                        scroll
                            .set_offset(point(px(if frame % 2 == 0 { -300. } else { 0. }), px(0.)));
                        cx.notify();
                    })
                    .unwrap();
                wait_for_presented_frame(window, cx).await;
            }
            window
                .update(cx, |view, _, _| {
                    assert_eq!(view.detail_list.state.bounds_for_item(0).unwrap().size.height, height,
                        "An offscreen wrapped title must keep carousel height stable");
                    let mut samples = std::mem::take(&mut view.shelf_render_times);
                    assert!(samples.len() >= 40, "Measure real shelf renders across presented frames");
                    samples.sort_unstable();
                    let total: Duration = samples.iter().sum();
                    println!("CAROUSEL_WARM count={count} samples={} mean_us={:.1} p50_us={:.1} p95_us={:.1} max_us={:.1}",
                        samples.len(), total.as_secs_f64() * 1e6 / samples.len() as f64,
                        samples[samples.len()/2].as_secs_f64() * 1e6,
                        samples[samples.len()*95/100].as_secs_f64() * 1e6,
                        samples.last().unwrap().as_secs_f64() * 1e6);
                })
                .unwrap();

            cx.update_window(window.into(), |_, window, cx| {
                window.dispatch_event(
                    PlatformInput::ScrollWheel(ScrollWheelEvent {
                        position: point(px(700.), px(350.)),
                        delta: ScrollDelta::Pixels(point(px(-300.), px(0.))),
                        touch_phase: TouchPhase::Moved,
                        modifiers: Default::default(),
                    }),
                    cx,
                );
            })
            .unwrap();
            wait_for_presented_frame(window, cx).await;
            window
                .update(cx, |view, window, _| {
                    assert!(
                        view.shelf_scroll[&0].offset().x < px(0.),
                        "Horizontal wheel input must move the carousel"
                    );
                    assert_eq!(
                        view.detail_list
                            .state
                            .bounds_for_item(0)
                            .unwrap()
                            .size
                            .height,
                        height
                    );
                    capture(window, &format!("carousel-{count}-scrolled"));
                })
                .unwrap();

            window
                .update(cx, |view, _, cx| {
                    let Item::Collection(last) = view.page.shelves[0].items.last_mut().unwrap()
                    else {
                        panic!("Fixture must end with an album")
                    };
                    last.title = "Short title".into();
                    view.rebuild_rows();
                    cx.notify();
                })
                .unwrap();
            wait_for_presented_frame(window, cx).await;
            let edited_height = window
                .update(cx, |view, _, _| {
                    let edited = view
                        .detail_list
                        .state
                        .bounds_for_item(0)
                        .unwrap()
                        .size
                        .height;
                    assert!(
                        edited < height,
                        "Updated offscreen text must resize the shelf"
                    );
                    edited
                })
                .unwrap();
            window
                .update(cx, |_, window, cx| {
                    window.resize(size(px(1000.), px(800.)));
                    cx.notify();
                })
                .unwrap();
            wait_for_presented_frame(window, cx).await;
            window
                .update(cx, |view, window, cx| {
                    assert_ne!(
                        view.detail_list
                            .state
                            .bounds_for_item(0)
                            .unwrap()
                            .size
                            .height,
                        edited_height,
                        "Responsive card widths must remeasure carousel height"
                    );
                    capture(window, &format!("carousel-{count}-narrow"));
                    window.resize(size(px(1280.), px(800.)));
                    cx.notify();
                })
                .unwrap();
            wait_for_presented_frame(window, cx).await;
            window
                .update(cx, |view, _, _| {
                    assert_eq!(
                        view.detail_list
                            .state
                            .bounds_for_item(0)
                            .unwrap()
                            .size
                            .height,
                        edited_height,
                        "Returning to the original width must restore its exact height"
                    );
                })
                .unwrap();
        }
    })
}
