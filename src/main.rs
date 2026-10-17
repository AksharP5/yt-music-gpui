mod account;
mod app;
mod artwork;
mod assets;
mod auth;
mod catalog;
mod input;
mod library;
mod player;
mod theme;

use gpui::{App, AppContext, Bounds, TitlebarOptions, WindowBounds, WindowOptions, px, size};
use std::path::PathBuf;

fn main() {
    // A crash dump must not persist reusable account credentials.
    let limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    if unsafe { libc::setrlimit(libc::RLIMIT_CORE, &limit) } != 0 {
        eprintln!("Could not disable credential-bearing crash dumps.");
        std::process::exit(1);
    }
    let mut cookies = None;
    let mut connect = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--connect" => connect = true,
            "--cookies" => {
                cookies = args.next().map(PathBuf::from);
                if cookies.is_none() {
                    eprintln!("--cookies requires a file path");
                    std::process::exit(2);
                }
            }
            "--help" | "-h" => {
                println!(
                    "YouTube Music GPUI\n\nUsage: yt-music-gpui [--connect] [--cookies FILE]\n\n--connect        Connect your Google account in a dedicated sign-in window\n--cookies FILE   Import a Netscape-format YouTube account session\n\nCtrl K searches. Press ? in the app for all shortcuts."
                );
                return;
            }
            "--version" | "-V" => {
                println!("yt-music-gpui {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            _ => {
                eprintln!("Unknown argument: {arg}. Use --help for usage.");
                std::process::exit(2);
            }
        }
    }
    if (connect || cookies.is_some()) && !auth::ACCOUNT_LOGIN_ENABLED {
        eprintln!("Account connection is disabled until the security review is complete.");
        std::process::exit(1);
    }
    if connect {
        eprintln!(
            "Sign in to YouTube Music in the Google account window. It will close when the connection is ready."
        );
        if let Err(error) = auth::connect(&std::sync::atomic::AtomicBool::new(false)) {
            eprintln!("{error:#}");
            std::process::exit(1);
        }
    }
    gpui_platform::application()
        .with_assets(assets::Assets)
        .run(move |cx: &mut App| {
            input::init(cx);
            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();
            let bounds = Bounds::centered(None, size(px(1320.), px(860.)), cx);
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    window_min_size: Some(size(px(850.), px(560.))),
                    titlebar: Some(TitlebarOptions {
                        title: Some("YouTube Music".into()),
                        ..Default::default()
                    }),
                    app_id: Some("yt-music-gpui".into()),
                    ..Default::default()
                },
                |window, cx| cx.new(|cx| app::Music::new(cookies, window, cx)),
            )
            .expect("open YouTube Music window");
            cx.activate(true);
        });
}
