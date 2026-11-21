# Contributing

Bug reports, fixes and focused features are welcome. For a larger change, open an issue first so we can agree on the approach before you write it.

## Setup

You need stable Rust, a C compiler, pkg-config, and development files for libmpv, EGL, GLES, Wayland, X11, Xext, xcb, xkbcommon, fontconfig and freetype. On Arch:

```sh
sudo pacman -S --needed git rust base-devel mpv yt-dlp deno libglvnd wayland libx11 libxext libxcb libxkbcommon-x11 fontconfig freetype2
```

On Debian and Ubuntu, [the CI setup](.github/actions/setup/action.yml) lists the packages to install. Sign-in also needs Python 3, xdg-utils and libsecret's `secret-tool`, and saves accounts in a running Secret Service keyring.

On macOS, install Xcode's command line tools and `brew install mpv yt-dlp deno`, then build with `cargo build --release --locked`. The macOS build plays signed-out audio only for now: sign-in, video and media keys are Linux-only, and CI checks the build on Apple Silicon with macOS 15.

On Windows, install Rust with the MSVC toolchain, build with `cargo build --release --locked`, and put `libmpv-2.dll` from mpv's release `libmpv-*-x86_64-w64-mingw32-lgpl.zip` next to the executable or on `PATH`; [the CI setup](.github/actions/setup/action.yml) pins the one CI uses. yt-dlp and Deno come from `PATH`. The Windows build also plays signed-out audio only, and CI checks it on Windows Server 2025.

## Build and install from source

```sh
git clone https://github.com/AksharP5/yt-music-gpui
cd yt-music-gpui
bin/build
bin/install
```

`bin/install` copies the app to `~/.local/bin` and the launcher, icon and licenses under `${XDG_DATA_HOME:-~/.local/share}`. It does not install dependencies or the reviewed yt-dlp; with any other yt-dlp version, signed-in lookups fall back to guest extraction. Signed-in playback also needs Deno; Node, Bun and QuickJS are never used (see [SECURITY.md](SECURITY.md)).

`yt-music-gpui --version` prints the app version on the first line, followed by the detected yt-dlp and Deno paths.

## How playback works

The player runs on its own thread (`src/player.rs`). It blocks on mpv's IPC socket, a command eventfd and the extractor's output. A stopped player blocks without a timer when it has no prepared track or active lookup.

A clicked song uses a native, cookie-free player request (`src/player/native.rs`) that skips the watch page and never waits on yt-dlp. If that request fails, or the chosen format needs JavaScript to decode its URL, the same play falls back to yt-dlp. The next song is prepared in the background. Once the first page has loaded, the app also prepares the song Play would start while nothing plays, such as the one restored at launch, or with an empty queue fetches the visitor ID the first click needs. Signed-in High audio quality and video above 1080p always go through yt-dlp, which uses your account to get Premium formats.

Account playback uses an extractor (`src/ytdlp_account.py`) reviewed against one yt-dlp release, pinned in that file. With any other yt-dlp version, the app falls back to guest streams and says so. A weekly CI job tests each new yt-dlp release so the pin can be bumped. The app hands the account to the extractor as one JSON line on its stdin. Credentials never appear in an argument, an environment variable or a file.

Playback sockets and the extractor live in a private directory under `$XDG_RUNTIME_DIR` (`$TMPDIR` on macOS, `%LOCALAPPDATA%\yt-music-gpui\runtime` on Windows), removed on normal shutdown, SIGTERM and SIGINT. The queue, preferences and guest library live in `${XDG_DATA_HOME:-~/.local/share}/yt-music-gpui/library.json` (`%LOCALAPPDATA%\yt-music-gpui\library.json` on Windows).

### Profiling a packaged build

`bin/profile APP_PID 30` samples a running release build for 30 seconds. It sums proportional set size (PSS) and CPU time across the app and its child processes.

To compare two builds, use a fresh guest profile for each run with the same window size, public track and volume, and turn off autoplay so each run plays the same thing. Check the executable checksum. Let the window and artwork settle, sample idle memory, then start playback and sample again once the position advances. Run each build several times, alternating the order.

## Validate

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
bin/test
bin/test-install
```

Outside tests, Clippy rejects the calls that can crash the app: `unwrap`, `expect`, `panic!`, `unreachable!`, `todo!`, `unimplemented!`, byte-index string slicing and unchecked `Instant`/`SystemTime` subtraction (see `[lints.clippy]` in `Cargo.toml`). Handle the failure instead. For a true invariant, put `#[expect(clippy::..., reason = "...")]` on the narrowest item that needs it.

CI runs the same checks on every pull request. `bin/build` and `bin/test` use two jobs unless `CARGO_BUILD_JOBS` is set.

## Integration tests

Tests that need the network, libmpv, yt-dlp, a keyring or a desktop session are marked `#[ignore]`, and each ignore reason says what the test needs and what it touches. List them and run one by name:

```sh
bin/test -- --ignored --list
bin/test -- --ignored --exact player::tests::embedded_extractor_binds_the_selected_account
```

No test signs in to a real Google account. Do not add one that does. Use synthetic cookies and accounts, as the existing tests do.

- **yt-dlp contracts** (`quality_selectors_respect_audio_classes_and_video_caps`, `embedded_extractor_binds_the_selected_account`) run offline but need a Python with the yt-dlp version pinned in `src/ytdlp_account.py`. Set `YT_DLP_TEST_PYTHON` to that interpreter if `python3` has a different version, or `YT_DLP_TEST_ZIPAPP` to an absolute path of the official zipapp to test that instead of a pip install. CI runs both on every pull request.
- **libmpv** (`plays_with_the_installed_libmpv`) checks the player's options against the installed libmpv with a short silent file.
- **`live_*` tests** call public YouTube Music endpoints as a guest. The one exception is `catalog::tests::live_owned_playlist_suggestions`, which reads with your saved account and needs `YT_MUSIC_LIVE_PLAYLIST` set to an editable playlist's `VLPL…` browse ID. It sends no edits and does not save refreshed credentials.
- **Keyring and browser tests** (`live_keyring_removal_is_idempotent`, `live_default_browser_profile_resolves_omarchy_launcher`) use your desktop session. They create only a synthetic keyring entry and read only browser profile metadata.

### Native window tests

The tests in `src/app/native_tests/` and the `native_` tests in `src/app/player_page/comments.rs` open real windows. They refuse to run unless:

- `YT_MUSIC_NATIVE_TESTS=1` is set,
- `XDG_RUNTIME_DIR` is a directory you own, mode 0700, directly under `/tmp`,
- `XDG_DATA_HOME` and `XDG_CACHE_HOME` are separate directories inside it,
- the session bus comes from `dbus-run-session`.

Together these keep the tests away from your library, saved account, keyring and media keys. GPUI's application loop cannot restart within one process, so each test needs its own process and a fresh runtime. The tests set their own volume to 0.

Some tests compare window pixels through X11 and ImageMagick's `import`, so the recipe unsets `WAYLAND_DISPLAY`.

```sh
for test in $(bin/test -- --ignored --list | sed -n \
  -e '/^app::native_tests::performance::carousel_render_work_and_height_stay_bounded_while_scrolling: test$/d' \
  -e '/^app::native_tests::suggestions::suggestions_refresh_cost_with_loaded_playlist: test$/d' \
  -e '/^app::native_tests::/s/: test$//p' \
  -e '/^app::player_page::comments::tests::native_/s/: test$//p'); do
  runtime=$(mktemp -d /tmp/yt-music-native.XXXXXX)
  mkdir -p "$runtime/data/yt-music-gpui" "$runtime/cache"
  env -u WAYLAND_DISPLAY \
    PIPEWIRE_RUNTIME_DIR="$XDG_RUNTIME_DIR" \
    PULSE_RUNTIME_PATH="$XDG_RUNTIME_DIR/pulse" \
    XDG_RUNTIME_DIR="$runtime" XDG_DATA_HOME="$runtime/data" XDG_CACHE_HOME="$runtime/cache" \
    YT_MUSIC_NATIVE_TESTS=1 \
    dbus-run-session --config-file=bin/private-bus.conf -- bin/test -- --ignored --exact "$test" --nocapture || break
  # Screenshots from the render checks stay in $runtime for inspection.
  echo "Native test files: $runtime"
done
```

To run a single test, replace the loop's list with its name. Remove the runtime directories when you are done.

### Performance checks

Run these with `--release` and `--nocapture`, and compare results on the same machine. The native ones need the same private environment as the native tests above.

```sh
bin/test --release -- --ignored --exact player::tests::measure_local_guest_playback --nocapture
bin/test --release player::tests::local_actor_wakeups_and_latency -- --ignored --nocapture --test-threads=1
bin/test --release -- --ignored --exact library::tests::large_queue_preference_save_latency --nocapture
bin/test --release -- --ignored --exact app::native_tests::performance::carousel_render_work_and_height_stay_bounded_while_scrolling --nocapture
bin/test --release -- --ignored --exact app::native_tests::suggestions::suggestions_refresh_cost_with_loaded_playlist --nocapture
```

The two player measurements use silent local audio and null audio output; give `measure_local_guest_playback` a private, RAM-backed `XDG_RUNTIME_DIR`. Set `YT_MUSIC_COMMENTS_PROFILE=1` when running the native comments test to sample the app with `bin/profile`.

The Linux performance job in CI compares the pull request with its base commit on:

- the stripped release binary size,
- local playback timings and playback-test memory (`measure_local_guest_playback`), with the app's two-arena glibc cap,
- how many requests the fast lookup sends between Play and mpv opening the stream, for a prepared play, the first unprepared play and a later one (`measure_play_requests`, against a local server),
- launch to the first frame and to the first Home frame, and memory on a settled Home. The app is built with `--cfg ytm_perf`, which prints frame timestamps and reads Home from the fixture in `YTM_PERF_HOME`. It runs under a private bus, with every connection sent to a closed port.

It fails when the binary grows past the budget in `bin/performance-budget.json` or a play sends more requests, and warns on playback timing regressions, since shared runners vary. Memory and startup times are reported, not gated: CI renders in software in Xvfb. Reproduce it with `bin/check-performance BASE_COMMIT`, optionally passing the reviewed yt-dlp zipapp as a second argument to skip the download. Startup is measured only when `DISPLAY` is set; the launches open windows on that display.

## Run an isolated instance

The app allows one instance per `XDG_RUNTIME_DIR`, and it keeps its library under `XDG_DATA_HOME`. To run a development build next to your installed app without touching your library, saved account or media keys:

```sh
runtime=$(mktemp -d /tmp/yt-music-dev.XXXXXX)
mkdir -p "$runtime/data/yt-music-gpui" "$runtime/cache"
printf '%s\n' '{"volume":0}' > "$runtime/data/yt-music-gpui/library.json"
WAYLAND_DISPLAY="$XDG_RUNTIME_DIR/${WAYLAND_DISPLAY:-wayland-0}" \
PIPEWIRE_RUNTIME_DIR="$XDG_RUNTIME_DIR" \
PULSE_RUNTIME_PATH="$XDG_RUNTIME_DIR/pulse" \
XDG_RUNTIME_DIR="$runtime" \
XDG_DATA_HOME="$runtime/data" \
XDG_CACHE_HOME="$runtime/cache" \
  dbus-run-session --config-file=bin/private-bus.conf -- cargo run -- --guest
```

`mktemp -d` creates the directory with mode 0700. The initial library mutes playback. The first three variables keep the display and audio connections pointed at your real session. `dbus-run-session` gives the instance a private bus, so it cannot reach your keyring or claim the media keys, and `bin/private-bus.conf` stops that bus from starting desktop portals that would outlive the run. `--guest` skips the saved account. Remove `$runtime` when you are done.

## Commits and pull requests

Commits follow [Conventional Commits](https://www.conventionalcommits.org/) with a scope when one fits, for example `fix(auth): read signed-in Zen sessions while the browser is open` or `feat(playback): embed video`. Common types are `feat`, `fix`, `perf`, `docs`, `ci` and `build`.

Keep each pull request to one change. Say what was broken or missing from a user's point of view, how you fixed it, and how you checked it in the running app. Include a screenshot or recording for UI changes. Test names and code comments should explain what a test checks; this file does not list them.

Never commit cookies, account data or keyring contents.

## Release

Releases are x86_64 Linux binaries built on Ubuntu 24.04. Merging only runs CI; it never publishes a release or deploys anything. Releases are immutable once published, only repository admins can create or move `v*` tags, and the release build refuses a tag whose commit is not on `main`.

1. Set the release version in `Cargo.toml` and refresh `Cargo.lock` with `cargo check`. Merge the reviewed version change.
2. Tag that commit with `git tag vX.Y.Z`, matching `Cargo.toml`, then `git push origin vX.Y.Z`.
3. The Release workflow builds with `--locked`, tests the tarball, `.deb` and `.rpm` in Ubuntu, Debian, Fedora and openSUSE containers, and publishes them with their SHA-256 checksums, `install.sh` and a dependency report.

Before tagging, run the Release workflow with `workflow_dispatch` on the branch. It uploads the same files as the `linux-x86_64` artifact without creating a release. Check `dependencies.txt` for direct dynamic dependencies and the highest required glibc symbol version.

Download the artifact and test the installer without touching an existing installation. Keep the matching `.tar.gz.sha256` beside the tarball:

```sh
home=$(mktemp -d)
HOME="$home" XDG_DATA_HOME="$home/data" sh install.sh --archive /absolute/path/yt-music-gpui-X.Y.Z-x86_64-linux.tar.gz
HOME="$home" XDG_DATA_HOME="$home/data" sh install.sh --check-deps
HOME="$home" XDG_DATA_HOME="$home/data" sh install.sh --uninstall
rm -r -- "$home"
```

Container `--version` checks do not prove that graphics, audio or sign-in work, so test playback with the installed binary using the isolated instance recipe above.

`src/ytdlp_account.py` holds the reviewed yt-dlp version and zipapp SHA-256, plus the fallback Deno version, archive SHA-256 and download size. Packaging copies them into `runtime-pins.json`, which the release build and the installer read; nothing else pins these versions. When changing them, verify the official release hashes, run `bin/test-install` and both yt-dlp contracts, and test the zipapp's resident resolver and bundled EJS solver. The app never downloads tools or solver scripts.
