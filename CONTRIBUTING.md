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

- **yt-dlp contract tests** (`quality_selectors_respect_audio_classes_and_video_caps`, `embedded_extractor_binds_the_selected_account`) run offline but need a Python with the yt-dlp version pinned in `src/ytdlp_account.py`. Point `YT_DLP_TEST_PYTHON` at that interpreter if `python3` has a different version. CI runs both on every pull request, and a weekly job runs them against the latest yt-dlp release. The account contract also exercises the resident resolver with synthetic responses, including extractor selection and per-request identity checks. `warm_resolver_reuses_requests_and_supersession_kills_the_process_group` and `warm_credential_change_respawns_with_new_identity_and_revokes_old_files` additionally test process reuse, cancellation and credential revocation with the installed `yt-dlp` interpreter, without network access.
- **`live_*` tests** call public YouTube Music endpoints without an account. Some player tests named `live_*` use only a synthetic local extractor, as their ignore reason says.
- **Keyring and browser tests** (`live_keyring_removal_is_idempotent`, `live_default_browser_profile_resolves_omarchy_launcher`) use your desktop session. They create only a synthetic keyring entry and read only browser profile metadata.

The native window tests live in `src/app/native_tests/` and `src/app/player_page/comments.rs`. They share `YT_MUSIC_NATIVE_TESTS=1` and refuse execution unless the runtime is an owned 0700 directory directly under `/tmp`, with separate data and artwork cache directories inside it and a private D-Bus session. Account, navigation and sync checks use synthetic accounts and intercepted commands. Startup checks hold saved-session Home and sidebar responses until verification and playback activation adopt that exact session. They also cover late responses, rejected reads, failed verification or activation, browser refresh, cancellation, and guest/connect startup. The navigation checks cover fresh and stale forward visits, account isolation, confirmed edits, route filters, sort and continuation state, and cache eviction. Back keeps an independent snapshot for each visit, including loaded continuations, errors and scroll handles. The original sync checks cover Back restoration and confirmed edits to stashed pages. The playback check uses public streams and a paginated public playlist. The restart check quits a guest process with 100 collection tracks cached, starts a fresh process against that saved queue, and uses Next from track 100 to play track 101. It also restarts an issued playback queue at its first page boundary. Both use a public classic rock playlist with playable streams at those boundaries. It then restarts an August 10 radio mix with 50 cached tracks and Repeat All enabled, a single song played from search, and a queue already playing an autoplay recommendation. Each restart uses the native Next shortcut and waits for the following track to play. The render check uses X11 and ImageMagick's `import` to compare actual window pixels after keyboard focus changes and playback ticks, including a scrolled artist page with decoded artwork while playing and paused. It reopens that artist through a Home link and compares the cached hero photo in the first presented frames, without scrolling or waiting for artwork. It also checks inline artist and album links with Tab, Shift+Tab and Enter, exercises library sorting with a synthetic response, forces a frame before a virtual queue row is revealed, and captures the library and queue at widths of 850 and 1000 pixels. It checks guide dividers and header backgrounds on Home and Explore, before and after scrolling and toggling the hamburger, at widths of 956 and 1920 pixels. Responsive Explore captures check the default guide at 935, 936, 956, 1149, 1150, 1280 and 1920 pixels, including wrapped tile labels.

The Comments check reads public guest comments during muted playback, exercises sorting and keyboard controls, and verifies that a 500-comment list renders only the visible rows. Set `YT_MUSIC_COMMENTS_PROFILE=1` to run `bin/profile PID 30` for two paired Up next/Comments samples. Panel crops are captured at 1280×800 and 1920×1080. Deterministic comments also check that Read more follows four rendered lines after wrapping and resizing.

Pixel captures must match the window geometry reported by X11. The capture helper keeps the test window inside the X11 desktop because ImageMagick clips offscreen pixels, including when a popup extends past the last monitor. It preserves the window size and all pixel assertions.

At 956 and 1920 pixels, the render check also verifies search song row spacing, separators and that duration appears only in the byline. Search top-result captures check stacked cards at column widths of 592 and 720 pixels and two-column cards at 860 pixels. At 956, 1280 and 1920 pixels, it captures search and player chrome, including the Autoplay header and hover handles. It opens the compact player controls with a pointer and dismisses them with Escape.

It verifies that track actions appear on keyboard focus without moving the title, remain reachable by Tab, and open their menu with Enter. A 1280px artist fixture checks the 347.8px shelf pitch for two-line titles with one-line subtitles, and scrolled Home captures check that the color wash leaves with the feed.

Render-count measurements wait for idle artwork and playback/catalog requests, then presented frames with stable counters. A window-bounds notification invalidates the measurement interval and starts it again; any redraw during an interval with stable bounds still fails the assertion.

These tests use Cargo's unoptimized test profile with debug assertions, the same assertions enabled by `cargo run`. The render check includes a Home response with selected and unselected chips and a network error at 956px, then clicks its dismiss button. Run each test in a separate process and fresh environment. GPUI's native application loop cannot be restarted within one test process. This recipe also gives the playback check a synthetic yt-dlp version mismatch, to exercise the guest fallback notice without an account:

```sh
real_ytdlp=$(command -v yt-dlp)
for test in $(bin/test -- --ignored --list | sed -n -e '/^app::native_tests::/s/: test$//p' -e '/^app::player_page::comments::tests::native_/s/: test$//p'); do
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
    dbus-run-session --config-file=bin/private-bus.conf -- bin/test -- --ignored --exact "$test" --nocapture || break
  # Representative screenshots from the render check stay in $runtime for inspection.
  echo "Native test files: $runtime"
done
```

The wrapper handles `--version` after other flags because the player queries `yt-dlp --ignore-config --no-plugin-dirs --version` before enabling account playback. All other invocations use the real extractor.

No test signs in to a real Google account. Do not add one that does.

## Player waits

The fast lookup runs in Rust with a cookie-free HTTP client and the VISIONOS player context reviewed against yt-dlp 2026.08.19. It skips the watch page and manifests, reuses connections, and caches visitor data from the first Music homepage bootstrap in memory. Short-lived lookup threads deliver completions through the player's eventfd, independently of the sequential Python resolver. A click keeps useful full preparation running; obsolete work can still be cancelled. Late native results cannot satisfy a full lookup or load an obsolete track.

Preparation uses the native path too, except signed-in High audio uses the full account extractor for Premium formats. Video above 1080p always uses the full extractor. Native format selection follows the existing quality selectors and YouTube's yt-dlp sort order. A selected format that needs signature or `n` deciphering falls back rather than lowering quality. All URLs pass the same HTTPS/googlevideo and expiry validation as prepared streams, including separate video/audio URLs.

Each native HTTP request has a 3-second timeout, within one 5-second lookup budget. Native failures fall back to the Python resolver, leaving most of the player's 30-second request deadline. A bot check refreshes visitor data once. A repeated bot check or unreadable homepage pauses native lookup for 10 minutes; a stall pauses it for one minute. The app's one-time reload after mpv cannot open a stream uses the full extractor and a fresh playback ID. Python now owns only the full yt-dlp instance and its in-memory player-script cache. Unsupported Python installs keep the one-shot CLI fallback.

`resolver_lanes_keep_clicks_fast_and_prepare_premium_audio` covers routing. Native fixture tests cover quality classes, bitrate fallbacks, codec ordering, video caps, missing formats, deciphering, visitor reuse, cookie isolation and cooldowns. `native_click_during_full_preparation_does_not_wait_or_restart_python` blocks a synthetic full extractor and checks native completion through the actor's fd wait without changing the extractor PID. `native_failures_fall_back_to_the_same_full_resolver` and `a_fast_lookup_in_flight_never_answers_a_full_request` cover fallback and reload isolation. The existing ignored `quality_selectors_respect_audio_classes_and_video_caps` contract also compares all 27 native audio/video selector combinations against yt-dlp using the same fixture response, so CI checks native sorting with the pinned release.

The ignored `player::native::tests::live_native_formats_match_ytdlp_and_download` compares all 27 selectors against yt-dlp on four public player responses. It then checks native audio plus video through 1080p with HTTP 206 reads at the start and middle, and downloads every selected byte in bounded ranges. It needs the network and `YT_DLP_TEST_PYTHON`, and never uses an account.

The player actor blocks on mpv IPC, the command eventfd and the resolver's stdout or one-shot extractor pidfd. Native lookup completions and video status changes also signal the eventfd. Video is enabled only after its native render context is ready, since a fast lookup can finish before renderer startup. Resolver startup and requests have 30-second deadlines; native attempts have a 5-second deadline and queued stream URLs wake the actor at expiry. Seek writes and account notices are flushed before sleeping. Synchronous mpv requests use their remaining five-second deadline for reads. Buffered IPC events are processed before waiting, and socket closure reports mpv shutdown. An idle or paused actor with no pending work has no timer.

`one_shot_exit_wakes_the_actor_without_mpv_or_commands` and `resolver_startup_and_request_deadlines_wake_without_output` cover extractor notifications and timeouts. The ignored `paused_actor_refreshes_expired_preparation_and_writes_pending_seek` uses synthetic silence and a local extractor to check expiry and seeks without network access or audible output.

To repeat the null-audio wakeup and Play/seek measurements, run `bin/test --release player::tests::local_actor_wakeups_and_latency -- --ignored --nocapture --test-threads=1`. It samples only the `music-player` thread's voluntary context switches over three five-second intervals per mode.

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

`mktemp -d` creates the directory with mode 0700. The initial library mutes playback. The cache stays inside the same directory. The first three variables keep the display and audio connections pointed at your real session. `dbus-run-session` gives the instance a private bus, so it cannot reach your keyring or claim the media keys. `bin/private-bus.conf` turns off service activation on that bus; with the default config, desktop portals start inside the private runtime, crash, and keep running after the instance exits. `--guest` skips the saved account. Remove `$runtime` when you are done.

## Commits and pull requests

Commits follow [Conventional Commits](https://www.conventionalcommits.org/) with a scope when one fits, for example `fix(auth): read signed-in Zen sessions while the browser is open` or `feat(playback): embed video`. Common types are `feat`, `fix`, `perf`, `docs`, `ci` and `build`.

Keep each pull request to one change. Say what was broken or missing from a user's point of view, how you fixed it, and how you checked it in the running app. Include a screenshot or recording for UI changes.

Never commit cookies, account data or keyring contents. Use synthetic values in tests, as the existing ones do.
