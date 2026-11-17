mod account;
mod app;
mod artwork;
mod assets;
mod auth;
mod catalog;
mod comments;
mod country_picker;
mod dirs;
mod history;
mod input;
mod library;
mod listening;
mod media_controls;
#[cfg_attr(windows, path = "os_windows.rs")]
mod os;
mod player;
mod playlist_choices;
mod playlist_form;
mod queue_source;
mod search;
mod search_box;
mod shutdown;
mod song_text;
mod theme;
mod track_state;
#[cfg_attr(not(target_os = "linux"), path = "video_unsupported.rs")]
mod video;
mod watch;

use gpui::{App, AppContext, Bounds, TitlebarOptions, WindowBounds, WindowOptions, px, size};

fn shutdown_on_signal(cx: &mut App) -> std::io::Result<()> {
    let signal = shutdown::listen()?;
    cx.spawn(async move |cx| {
        if signal.recv().await.is_ok() {
            // Quit on GPUI's thread so player and video destructors can clean up.
            cx.update(|cx| cx.quit());
        }
    })
    .detach();
    Ok(())
}

fn main() {
    if os::disable_core_dumps().is_err() {
        eprintln!("Could not disable credential-bearing crash dumps.");
        std::process::exit(1);
    }
    let mut startup = app::Startup::Restore;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--connect" if startup != app::Startup::Guest => startup = app::Startup::Connect,
            "--guest" if startup != app::Startup::Connect => startup = app::Startup::Guest,
            "--connect" | "--guest" => {
                eprintln!("Choose either --guest or --connect.");
                std::process::exit(2);
            }
            "--cookies" => {
                eprintln!(
                    "Cookie files are unsupported. Use --connect to verify your browser account."
                );
                std::process::exit(2);
            }
            "--help" | "-h" => {
                println!(
                    "YouTube Music GPUI\n\nUsage: yt-music-gpui [--guest | --connect]\n\n--guest          Start without reading the saved account\n--connect        Open your default browser and verify a YouTube account\n\nBrowser connection supports Zen, Firefox, Chromium, Google Chrome, Brave and Helium. Cookie files are unsupported.\nCtrl K searches. Press ? in the app for all shortcuts."
                );
                return;
            }
            "--version" | "-V" => {
                println!("yt-music-gpui {}", env!("CARGO_PKG_VERSION"));
                // Which helpers passed the trust checks, for bug reports and package tests.
                let ytdlp = player::find_ytdlp().map(|path| path.display().to_string());
                // yt-dlp looks for Deno on PATH unless the app passes a trusted copy.
                let deno = player::private_tool("deno").or_else(|| {
                    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
                        .map(|directory| {
                            directory.join(format!("deno{}", std::env::consts::EXE_SUFFIX))
                        })
                        .find(|path| path.is_file())
                });
                println!("yt-dlp: {}", ytdlp.as_deref().unwrap_or("not found"));
                match deno {
                    Some(path) => println!("deno: {}", path.display()),
                    None => println!("deno: not found"),
                }
                return;
            }
            _ => {
                eprintln!("Unknown argument: {arg}. Use --help for usage.");
                std::process::exit(2);
            }
        }
    }
    if startup == app::Startup::Connect && !auth::ACCOUNT_LOGIN_ENABLED {
        eprintln!("Sign-in on this system is coming soon.");
        std::process::exit(1);
    }
    if let Err(error) = video::initialize() {
        eprintln!("{error:#}");
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
            if let Err(error) = cx.text_system().add_fonts(vec![std::borrow::Cow::Borrowed(
                include_bytes!("../assets/fonts/Roboto.ttf").as_slice(),
            )]) {
                eprintln!("Could not load the interface font: {error:#}");
            }
            input::init(cx);
            if let Err(error) = shutdown_on_signal(cx) {
                eprintln!("Could not install shutdown signal handlers: {error}");
                std::process::exit(1);
            }
            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();
            let bounds = Bounds::centered(None, size(px(1320.), px(860.)), cx);
            let window = cx.open_window(
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
                |window, cx| cx.new(|cx| app::Music::new(startup, window, cx)),
            );
            if let Err(error) = window {
                eprintln!("Could not open the window: {error:#}");
                cx.quit();
                return;
            }
            cx.activate(true);
        });
}
