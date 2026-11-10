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
mod listening;
mod mpris;
mod player;
mod playlist_form;
mod queue_source;
mod search;
mod search_box;
mod song_text;
mod theme;
mod track_state;
mod video;
mod watch;

use gpui::{App, AppContext, Bounds, TitlebarOptions, WindowBounds, WindowOptions, px, size};

static TERMINATE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

extern "C" fn terminate(_: libc::c_int) {
    TERMINATE.store(true, std::sync::atomic::Ordering::Relaxed);
}

fn shutdown_on_signal(cx: &mut App) {
    // Signal handlers only set a flag. Quit on GPUI's thread so player and video
    // destructors can stop extraction and remove the private playback directory.
    for signal in [libc::SIGTERM, libc::SIGINT] {
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = terminate as *const () as usize;
        unsafe { libc::sigemptyset(&mut action.sa_mask) };
        if unsafe { libc::sigaction(signal, &action, std::ptr::null_mut()) } != 0 {
            eprintln!(
                "Could not install shutdown handler: {}",
                std::io::Error::last_os_error()
            );
        }
    }
    cx.spawn(async |cx| {
        loop {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(100))
                .await;
            if TERMINATE.load(std::sync::atomic::Ordering::Relaxed) {
                cx.update(|cx| cx.quit());
                break;
            }
        }
    })
    .detach();
}

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
                    "YouTube Music GPUI\n\nUsage: yt-music-gpui [--guest | --connect]\n\n--guest          Start without reading a saved account from the keyring\n--connect        Open your default browser and verify a YouTube account\n\nBrowser connection supports Zen, Firefox, Chromium, Google Chrome and Brave. Cookie files are unsupported.\nCtrl K searches. Press ? in the app for all shortcuts."
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
    if startup == app::Startup::Connect && !auth::ACCOUNT_LOGIN_ENABLED {
        eprintln!("Account connection is disabled until the security review is complete.");
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
            shutdown_on_signal(cx);
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
                |window, cx| cx.new(|cx| app::Music::new(startup, window, cx)),
            )
            .expect("open YouTube Music window");
            cx.activate(true);
        });
}
