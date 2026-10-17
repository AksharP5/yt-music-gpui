mod account;
mod app;
mod artwork;
mod assets;
mod auth;
mod catalog;
mod country_picker;
mod history;
mod input;
mod library;
mod player;
mod playlist_form;
mod search;
mod search_box;
mod song_text;
mod theme;
mod watch;

use gpui::{App, AppContext, Bounds, TitlebarOptions, WindowBounds, WindowOptions, px, size};

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
    let mut connect = false;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--connect" => connect = true,
            "--cookies" => {
                eprintln!(
                    "Cookie imports cannot identify the selected Google account. Use --connect."
                );
                std::process::exit(2);
            }
            "--help" | "-h" => {
                println!(
                    "YouTube Music GPUI\n\nUsage: yt-music-gpui [--connect]\n\n--connect        Connect your Google account in a dedicated sign-in window\n\nCookie imports are unsupported because they cannot identify the selected account.\nCtrl K searches. Press ? in the app for all shortcuts."
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
    if connect && !auth::ACCOUNT_LOGIN_ENABLED {
        eprintln!("Account connection is disabled until the security review is complete.");
        std::process::exit(1);
    }
    let _instance = match auth::lock_instance() {
        Ok(lock) => lock,
        Err(error) => {
            eprintln!("{error:#}");
            std::process::exit(1);
        }
    };
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
                |window, cx| cx.new(|cx| app::Music::new(connect, window, cx)),
            )
            .expect("open YouTube Music window");
            cx.activate(true);
        });
}
