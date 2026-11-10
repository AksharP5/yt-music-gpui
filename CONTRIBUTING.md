# Contributing

Bug reports, fixes and focused features are welcome. For a larger change, open an issue first so we can agree on the approach before you write it.

## Setup

You need stable Rust, a C compiler, pkg-config, and development files for libmpv, EGL, GLES, Wayland, X11, Xext, xcb, xkbcommon, fontconfig and freetype. On Arch:

```sh
sudo pacman -S --needed rust base-devel mpv yt-dlp libglvnd wayland libx11 libxext libxcb libxkbcommon-x11 fontconfig freetype2
```

On Debian and Ubuntu, [the CI setup](.github/actions/setup/action.yml) lists the packages to install. Sign-in also needs Python 3, xdg-utils, libsecret's `secret-tool` and a running Secret Service keyring.

## Validate

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
bin/test
```

CI runs the same checks on every pull request. `bin/build` and `bin/test` use two jobs unless `CARGO_BUILD_JOBS` is set.

## Integration tests

Tests that need the network, libmpv, yt-dlp, a keyring or a desktop session are marked `#[ignore]`, and each one says what it needs. List them with `bin/test -- --ignored --list` and run one by name:

```sh
bin/test -- --ignored --exact player::tests::embedded_extractor_binds_the_selected_account
```

- **yt-dlp contract tests** (`quality_selectors_respect_audio_classes_and_video_caps`, `embedded_extractor_binds_the_selected_account`) run offline but need a Python with the yt-dlp version pinned in `src/ytdlp_account.py`. Point `YT_DLP_TEST_PYTHON` at that interpreter if `python3` has a different version. CI runs both on every pull request, and a weekly job runs them against the latest yt-dlp release.
- **`live_*` tests** call public YouTube Music endpoints without an account. Some player tests named `live_*` use only a synthetic local extractor, as their ignore reason says.
- **Keyring and browser tests** (`live_keyring_removal_is_idempotent`, `live_default_browser_profile_resolves_omarchy_launcher`) use your desktop session. They create only a synthetic keyring entry and read only browser profile metadata.

The five native window tests live in `src/app/native_tests/`. They share `YT_MUSIC_NATIVE_TESTS=1` and refuse execution unless the runtime is an owned 0700 directory directly under `/tmp`, with separate data and artwork cache directories inside it and a private D-Bus session. Account and sync checks use synthetic accounts and intercepted commands. The playback check uses public streams and a paginated public playlist. The restart check quits a guest process with 100 collection tracks cached, starts a fresh process against that saved queue, and uses Next from track 100 to play track 101. It also restarts an issued playback queue at its first page boundary. Both use a public classic rock playlist with playable streams at those boundaries. It then restarts an August 10 radio mix with 50 cached tracks and Repeat All enabled, a single song played from search, and a queue already playing an autoplay recommendation. Each restart uses the native Next shortcut and waits for the following track to play. The render check uses X11 and ImageMagick's `import` to compare actual window pixels after keyboard focus changes and playback ticks, including a scrolled artist page with decoded artwork while playing and paused. It also checks inline artist and album links with Tab, Shift+Tab and Enter, exercises library sorting with a synthetic response, forces a frame before a virtual queue row is revealed, and captures the library and queue at widths of 850 and 1000 pixels. It checks guide dividers and header backgrounds on Home and Explore, before and after scrolling and toggling the hamburger, at widths of 956 and 1920 pixels.

Pixel captures must match the window geometry reported by X11. The capture helper keeps the test window inside the X11 desktop because ImageMagick clips offscreen pixels, including when a popup extends past the last monitor. It preserves the window size and all pixel assertions.

At 956 and 1920 pixels, the render check also verifies search song row spacing, separators and that duration appears only in the byline.

Render-count measurements wait for idle artwork and playback/catalog requests, then presented frames with stable counters. A window-bounds notification invalidates the measurement interval and starts it again; any redraw during an interval with stable bounds still fails the assertion.

These tests use Cargo's unoptimized test profile with debug assertions, the same assertions enabled by `cargo run`. The render check includes a Home response with selected and unselected chips and a network error at 956px, then clicks its dismiss button. Run each test in a separate process and fresh environment. GPUI's native application loop cannot be restarted within one test process. This recipe also gives the playback check a synthetic yt-dlp version mismatch, to exercise the guest fallback notice without an account:

```sh
real_ytdlp=$(command -v yt-dlp)
for test in $(bin/test app::native_tests:: -- --ignored --list | sed -n 's/: test$//p'); do
  runtime=$(mktemp -d /tmp/yt-music-native.XXXXXX)
  mkdir -p "$runtime/data/yt-music-gpui" "$runtime/cache" "$runtime/bin"
  cat > "$runtime/bin/yt-dlp" <<EOF
#!/bin/sh
for arg in "\$@"; do
  if [ "\$arg" = --version ]; then
    echo native-test-mismatch
    exit 0
  fi
done
exec "$real_ytdlp" "\$@"
EOF
  chmod 700 "$runtime/bin/yt-dlp"
  env -u WAYLAND_DISPLAY \
    PIPEWIRE_RUNTIME_DIR="$XDG_RUNTIME_DIR" \
    PULSE_RUNTIME_PATH="$XDG_RUNTIME_DIR/pulse" \
    XDG_RUNTIME_DIR="$runtime" XDG_DATA_HOME="$runtime/data" XDG_CACHE_HOME="$runtime/cache" \
    PATH="$runtime/bin:$PATH" YT_MUSIC_NATIVE_TESTS=1 \
    dbus-run-session -- bin/test -- --ignored --exact "$test" --nocapture || break
  # Representative screenshots from the render check stay in $runtime for inspection.
  echo "Native test files: $runtime"
done
```

The wrapper handles `--version` after other flags because the player queries `yt-dlp --ignore-config --no-plugin-dirs --version` before enabling account playback. All other invocations use the real extractor.

No test signs in to a real Google account. Do not add one that does.

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
  dbus-run-session -- cargo run -- --guest
```

`mktemp -d` creates the directory with mode 0700. The initial library mutes playback. The cache stays inside the same directory. The first three variables keep the display and audio connections pointed at your real session. `dbus-run-session` gives the instance a private bus, so it cannot reach your keyring or claim the media keys. `--guest` skips the saved account. Remove `$runtime` when you are done.

## Commits and pull requests

Commits follow [Conventional Commits](https://www.conventionalcommits.org/) with a scope when one fits, for example `fix(auth): read signed-in Zen sessions while the browser is open` or `feat(playback): embed video`. Common types are `feat`, `fix`, `perf`, `docs`, `ci` and `build`.

Keep each pull request to one change. Say what was broken or missing from a user's point of view, how you fixed it, and how you checked it in the running app. Include a screenshot or recording for UI changes.

Never commit cookies, account data or keyring contents. Use synthetic values in tests, as the existing ones do.
