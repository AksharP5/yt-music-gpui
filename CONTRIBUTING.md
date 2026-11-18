# Contributing

Bug reports, fixes and focused features are welcome. For a larger change, open an issue first so we can agree on the approach before you write it.

## Setup

You need stable Rust, a C compiler, pkg-config, and development files for libmpv, EGL, GLES, Wayland, X11, Xext, xcb, xkbcommon, fontconfig and freetype. On Arch:

```sh
sudo pacman -S --needed git rust base-devel mpv yt-dlp deno libglvnd wayland libx11 libxext libxcb libxkbcommon-x11 fontconfig freetype2
```

On Debian and Ubuntu, [the CI setup](.github/actions/setup/action.yml) lists the packages to install. Sign-in also needs Python 3, xdg-utils and libsecret's `secret-tool`, and saves accounts in a running Secret Service keyring.

On macOS, install Xcode's command line tools and `brew install mpv yt-dlp deno`, then build with `cargo build --release --locked`. The macOS build plays signed-out audio only for now: sign-in, video and media keys are Linux-only, and CI checks the build on Apple Silicon with macOS 15.

On Windows, install Rust with the MSVC toolchain, build with `cargo build --release --locked`, and put `libmpv-2.dll` from mpv's release `libmpv-*-x86_64-w64-mingw32-lgpl.zip` next to the executable or on `PATH`; [the CI setup](.github/actions/setup/action.yml) pins the one CI uses. yt-dlp and Deno come from `PATH`. The Windows build plays signed-out audio only for now, like macOS, and CI checks it on Windows Server 2025.

## Build and install from source

```sh
git clone https://github.com/AksharP5/yt-music-gpui
cd yt-music-gpui
bin/build
bin/install
```

`bin/install` copies the app to `~/.local/bin` and the launcher, icon and licenses under `${XDG_DATA_HOME:-~/.local/share}`. It does not install dependencies or the reviewed yt-dlp; with any other yt-dlp version, signed-in lookups fall back to guest extraction. Signed-in playback also needs Deno; Node, Bun and QuickJS are never used (see [SECURITY.md](SECURITY.md)).

`yt-music-gpui --version` prints the app version on the first line, followed by the detected yt-dlp and Deno paths. Use `yt-music-gpui --version | head -n 1` for just the version line.

## How playback works

A clicked song first uses a native, cookie-free player request that skips the watch page and doesn't wait on yt-dlp. The next song is prepared in the background; when signed in with High audio quality, that uses your account through yt-dlp to keep Premium audio. Video above 1080p also uses yt-dlp. If the native request fails or needs JavaScript to decode a stream URL, the same play falls back to yt-dlp, and a stream that fails to open retries through yt-dlp.

Long queues fetch continuation pages while the current songs play. Transport failures and HTTP 408, 429 or 5xx responses get one asynchronous retry per page. A natural page-boundary advance can retry an exhausted transient prefetch; other failures stay visible in Up next with Retry. Recovery preserves paused playback and cannot restart a queue the user stopped. Account rejection and invalid or repeated pages do not retry automatically.

Up next's Save can create a playlist from every loaded queue entry, preserving order and repeated songs. Creation readback checks every submitted song across playlist continuations before reporting success, with session guards around each read. It never repeats the create request when readback fails.

Account playback uses an extractor reviewed against one yt-dlp release, pinned in `src/ytdlp_account.py`. The player checks the installed version alongside saved-account restoration or `--connect` startup. Guest sessions defer the check until account activation. The player caches the result. A weekly CI job tests each new yt-dlp release so the pin can be bumped.

The app hands the account to yt-dlp over the extractor's stdin: one JSON line with the Netscape cookie jar and the account to play as. A short-lived thread writes it, because it can exceed the pipe buffer and the player thread must keep handling pause, seek and deadlines. Killing the extractor ends that write. The extractor reads the line before any request, loads the cookies into yt-dlp's in-memory jar and never gets a cookie file, so nothing is saved back. Credentials never appear in an argument, an environment variable or a file. The resident resolver reads the line before it reports ready, and the app holds a signed-in request until then, so the two never mix on the pipe. The one-shot fallback's stdin closes after the line. `credentials_reach_the_extractor_only_through_its_stdin` checks, for both, that the child receives exactly that line and that its arguments, environment and open descriptors carry nothing else. `an_unread_oversized_account_never_blocks_the_player_thread` gives a stalled or exited extractor a 1 MiB line and checks that the player thread returns at once, the deadline reaps the extractor, and the writer ends.

Playback sockets and the account extractor live in a private directory under `$XDG_RUNTIME_DIR` (`$TMPDIR` on macOS, `%LOCALAPPDATA%\yt-music-gpui\runtime` on Windows, where mpv listens on a randomly named pipe instead), removed on normal shutdown, SIGTERM and SIGINT (Ctrl+C or Ctrl+Break on Windows). The queue, preferences and guest library live in `${XDG_DATA_HOME:-~/.local/share}/yt-music-gpui/library.json` (`%LOCALAPPDATA%\yt-music-gpui\library.json` on Windows). To sample CPU and memory of a running release build, use `bin/profile APP_PID 30`.

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

Playlist Suggestions use their own session-bound continuation and worker. They appear after the playlist songs when the service supplies a seed. Refresh updates that section in place and keeps the previous batch visible on failure. The loaded playlist rows and their measurements survive Refresh. The related carousel keeps its horizontal position when suggestions refresh or more playlist songs load. The song count, pagination and Play all still use playlist data.

The native Suggestions checks use synthetic accounts and intercept Add commands. They cover wide and narrow layouts, Tab and Enter on offscreen Refresh, skipped disabled controls, retained Refresh focus when the batch grows, repeated Enter after an Add, related card navigation and menus, and carousel width and scroll position. They also confirm that the clicked song's metadata survives Refresh or navigation before the Add acknowledgment. The same metadata travels through duplicate confirmation. The confirmation takes keyboard focus when an Add reply opens it. The native check confirms that repeated Enter cannot resubmit the underlying suggestion, Escape restores focus, and **Add anyway** submits the duplicate.

Confirmed playlist additions, removals and moves refresh totals and covers separately from loaded songs. Refreshes are queued per playlist and combine unresolved occurrence checks for repeated edits. Content edits patch loaded guide entries without another membership read, preserving the guide's owner text and selecting the track count separately from views. Verified details survive page and guide reads already in flight and invalidate cached forward pages. A page reply patched with newer totals remains ineligible for forward caching. Guide replay ends when its read completes, fails or is replaced.

The playlist song menu reads the neighboring loaded occurrences for Move up and Move down. The native menu check uses duplicate videos across two shelves and verifies the destination occurrence sent by each action.

Readback retains acknowledged removals, known added rows and the final loaded row order through overlapping reads and failures. Retained rows keep their original first-page coverage and continuation together, so a later revalidation cannot skip a loaded page. Replayed rows keep later confirmed ratings and library changes. Verified continuation pages extend retained snapshots. Cross-page moves keep the page sizes stable so first-page merging preserves displaced rows. An unverified partial first-page read keeps fully loaded rows containing known additions, including when metadata finishes first or a failed readback settles during Reload. Snapshots skip loading placeholders and can use a matching loaded Back page. A matching short prefix does not verify the full loaded order. A dedicated metadata worker keeps readback off the three foreground catalog workers. Rapid edits coalesce behind the active metadata read; its reply either settles the latest edit generation or starts one successor. Failed checks are retained independently of the visible error. Deleting a playlist and renewing cookies clear both pending and failed state.

An explicit Reload must start after acknowledgement. While readback is pending, additions must appear, removals need a complete response, and moves need visible order evidence before Reload can settle it. After readback has failed, Reload adopts the server's order and uses the bounded background read's partial-page policy so long playlists can recover; a response without full occurrence and order evidence still stays out of the forward cache. Background readback retries up to five times when returned occurrences or the visible order contradict the edit. It does not scan continuations or repeat a write; a partial first response cannot verify an append outside that response.

Native checks cover last-song totals, retaining 500 loaded songs and the visible occurrence, overlapping edits and reads, cookie renewal, scoped refresh errors, and clearing the reconnect message when an explicit retry is accepted. Accepting an edit keeps any unresolved page-loading error visible.

A failed page continuation keeps its warning when a dialog clears or replaces general feedback. Dismissing another warning reveals the continuation warning. Dismissing the continuation warning enables automatic paging again.

Tests that need the network, libmpv, yt-dlp, a keyring or a desktop session are marked `#[ignore]`, and each one says what it needs. List them with `bin/test -- --ignored --list` and run one by name:

```sh
bin/test -- --ignored --exact player::tests::embedded_extractor_binds_the_selected_account
```

- **yt-dlp contract tests** (`quality_selectors_respect_audio_classes_and_video_caps`, `embedded_extractor_binds_the_selected_account`) run offline but need a Python with the yt-dlp version pinned in `src/ytdlp_account.py`. Point `YT_DLP_TEST_PYTHON` at that interpreter if `python3` has a different version. CI runs both on every pull request, and a weekly job runs them against the latest yt-dlp release. Set `YT_DLP_TEST_ZIPAPP` to an absolute official zipapp path to run both contracts against its bundled package, including resident subprocesses, instead of a pip installation. The account contract also exercises the resident resolver with synthetic responses, including extractor selection and per-request identity checks. It also runs the one-shot yt-dlp command line with the same stdin hand-off, and rejects missing, truncated or malformed credentials before any request. `warm_resolver_reuses_requests_and_supersession_kills_the_process_group` and `warm_credential_change_respawns_with_the_new_account` additionally test process reuse, cancellation and switching accounts with the installed `yt-dlp` interpreter, without network access.
- **libmpv compatibility** (`plays_with_the_installed_libmpv`) starts mpv with the player's options, loads a short silent file paused partway in, and checks that mpv applied the start position, pause, title, the appended next track and, where mpv supports it, the loudness gain. CI runs it on Ubuntu 24.04 for every pull request, and release builds run it inside each distribution the packages target, against that distribution's libmpv.
- **`live_*` tests** generally call public YouTube Music endpoints as a guest. Follow each test's ignore reason for exceptions. `catalog::tests::live_owned_playlist_suggestions` requires a saved account and `YT_MUSIC_LIVE_PLAYLIST` set to a selected editable playlist's `VLPL…` browse ID. It reads the playlist and refreshes recommendations. If the saved session expired, the test can read the linked browser profile and verify the same account for this run. It sends no playlist edits and does not save refreshed credentials.
- **Keyring and browser tests** (`live_keyring_removal_is_idempotent`, `live_default_browser_profile_resolves_omarchy_launcher`) use your desktop session. They create only a synthetic keyring entry and read only browser profile metadata.

The native window tests live in `src/app/native_tests/` and `src/app/player_page/comments.rs`. They share `YT_MUSIC_NATIVE_TESTS=1` and refuse execution unless the runtime is an owned 0700 directory directly under `/tmp`, with separate data and artwork cache directories inside it and a private D-Bus session. Account, navigation and sync checks use synthetic accounts and intercepted commands. Startup checks hold saved-session Home and sidebar responses until verification and playback activation adopt that exact session. A verified same-account renewal keeps page and sidebar cursors usable, including for visits saved by Back and Forward. They also cover late responses, rejected reads, failed verification or activation, browser refresh, cancellation, guest/connect startup, and the choice to save without a keyring. The navigation checks cover fresh and stale forward visits, account isolation, confirmed edits, route filters, sort and continuation state, cache eviction, and the titled shelves of Explore pages such as Charts. An Up next check clicks a queued row's artwork and then another row's title, and each must play that row; dragging a row must still reorder the queue. Pressing Escape before releasing a dragged row must keep the previous order and leave Up next open. Closing the player with Q or Back must also cancel the move. A playlist reply or a Shuffle shortcut during a drag must preserve the resulting queue and current song through mouse release. Save to playlist shows loaded guide rows immediately while an authenticated `playlist/get_add_to_playlist` read returns destinations for the exact ordered song selection. A failed read keeps those rows with Retry. Single songs provisionally include Liked Music; queues and podcast episodes do not. Choosing Liked Music sends an explicit Like even when already liked. Native checks cover that behavior, cancelled/reopened/account-refreshed reads, public covers and the confirmation toast above the player bar. The search check confirms that Enter searches the typed text when the pointer rests on a suggestion, and opens a suggestion reached with the arrow keys. A second search check presses R to cycle repeat, then types R into the focused search box and confirms repeat did not change. Back and Forward keep an independent snapshot for each visit, including loaded continuations, errors and scroll handles. Opening a different page clears Forward history. Confirmed playlist edits update both directions. The original sync checks cover Back restoration and confirmed edits to stashed pages. The playback check uses public streams and a paginated public playlist. The restart check quits a guest process with 100 collection tracks cached, starts a fresh process against that saved queue, and uses Next from track 100 to play track 101. It also restarts an issued playback queue at its first page boundary. Both use a public classic rock playlist with playable streams at those boundaries. It then restarts an August 10 radio mix with 50 cached tracks and Repeat All enabled, a single song played from search, and a queue already playing an autoplay recommendation. Each restart uses the native Next shortcut and waits for the following track to play. The resume check plays a public song for 20 seconds and quits. The next process must show that position in the player bar without playing, and Play must continue from it. A third process presses Previous before Play and must start from the beginning. The render check uses X11 and ImageMagick's `import` to compare actual window pixels after keyboard focus changes and playback ticks, including a scrolled artist page with decoded artwork while playing and paused. It reopens that artist through a Home link and compares the cached hero photo in the first presented frames, without scrolling or waiting for artwork. It also checks inline artist and album links with Tab, Shift+Tab and Enter, exercises library sorting with a synthetic response, forces a frame before a virtual queue row is revealed, and captures the library and queue at widths of 850 and 1000 pixels. It checks guide dividers and header backgrounds on Home and Explore, before and after scrolling and toggling the hamburger, at widths of 956 and 1920 pixels. Responsive Explore captures check the default guide at 935, 936, 956, 1149, 1150, 1280 and 1920 pixels, including wrapped tile labels.

The X11 player tab check compares frames before and after a failed track-details read. Lyrics, Comments and Related must leave Loading while keeping the selected tab and paused playback.

The Comments check reads public guest comments during muted playback, exercises sorting and keyboard controls, and verifies that a 500-comment list renders only the visible rows. Set `YT_MUSIC_COMMENTS_PROFILE=1` to run `bin/profile PID 30` for two paired Up next/Comments samples. Panel crops are captured at 1280×800 and 1920×1080. Deterministic comments also check that Read more follows four rendered lines after wrapping and resizing.

Dialog checks scroll both sides of a long Save to playlist list and its backdrop, and right-click the dimmed page. The list must move without scrolling or opening a menu in the page underneath. The New playlist footer must stay clear of scrolling rows. Keyboard checks ensure Help captures Enter and Space and restores focus on Escape, and that country-picker arrows make Enter choose the highlighted country.

While Save to playlist waits for the service, it counts the eligible destinations from the loaded guide and copies only the rows requested by the virtual list. The count and row ranges use the same eligibility filter.

Tab and Shift+Tab scroll Save to playlist as focus moves through the destinations. If a service reply moves the focused playlist, the picker scrolls to its new position. A click completes only on the same destination where it started. The native checks cover offscreen selection, a reorder that moves the focused row out of view, and a refresh between mouse-down and mouse-up.

Save to playlist retains the submitted song details until YouTube replies. A batch checks existing playlist occurrences in one pass and preserves the service’s order, including distinct occurrences of the same song. A native check clears the source queue while a save is pending, then verifies that confirmed songs appear in the open playlist without waiting for another page read.

Album and playlist queues retain their independently issued Play and Shuffle actions. Toggling reloads the chosen source order and starts playback, as on the web, including restoring entries removed from Up next. Service-shuffled pages keep the service order and do not enable local eager continuation loading. Saved queues retain only the collection ID and order; restart or account changes require fresh actions. The native `collection_shuffle_toggles_pending_requests_and_ignores_stale_results` check covers rapid toggles, failures, late replies, background details reads and account recovery. `collection_shuffle_refreshes_actions_only_when_the_account_changes` completes synthetic player activation for account switches, guest sign-in and same-account refresh. A separate public guest integration check reloads actions after serialization and verifies both orders without accessing an account or playing media:

```sh
bin/test -- --ignored --exact queue_source::tests::restored_collection_refreshes_its_actions_and_loads_both_playback_orders
```

Artist Shuffle and Mix starts retain the queue's independently issued toggle actions and confirmed order. Their saved state also records which artist action to refresh. Restart and account changes fetch that action again, then obtain fresh queue controls when the requested order differs. Missing controls fall back to ordinary playback. The public guest `artist_queues_refresh_both_orders_for_shuffle_and_mix` integration check covers both starts, toggles and restart without playing media. The native account activation regression covers initial artist actions and accepted queue controls across account switches, guest sign-in and same-account refresh.

MPRIS Shuffle uses the same album, playlist or artist source as the player bar. Repeated requests for the same pending order do not reload it, and a rapid on/off pair keeps the last request. `desktop_shuffle_sets_the_requested_order_without_repeating_pending_requests` checks these commands in a native window. Queues without a reversible collection source still reject enabling persistent shuffle through MPRIS.

`desktop_metadata_follows_stopped_queue_selection` presses J and K in a native window and checks the selected song and its duration through a private D-Bus service, including a song without a known duration.

`account_changes_cancel_pending_shuffle_in_desktop_controls` reads the actual private D-Bus property when sign-out, sign-in, startup restore, refresh cancellation and account switching cancel a synthetic pending request. It checks the immediate state and the state after activation and late replies.

`desktop_previous_follows_the_restart_position` reads the private D-Bus capability as playback crosses three seconds and while a seek is pending or rejected. `desktop_seek_past_end_uses_next_availability` sends Next and Seek on the private D-Bus service and checks that both leave the last song playing when Next is unavailable.

Pixel captures must match the window geometry reported by X11. The capture helper keeps the test window inside the X11 desktop because ImageMagick clips offscreen pixels, including when a popup extends past the last monitor. It preserves the window size and all pixel assertions.

At 956 and 1920 pixels, the render check also verifies search song row spacing, separators and that duration appears only in the byline. Search top-result captures check stacked cards at column widths of 592 and 720 pixels and two-column cards at 860 pixels. At 956, 1280 and 1920 pixels, it captures search and player chrome, including the Autoplay header and hover handles. It opens the compact player controls with a pointer and dismisses them with Escape.

It verifies that track actions appear on keyboard focus without moving the title, remain reachable by Tab, and open their menu with Enter. A 1280px artist fixture checks the 347.8px shelf pitch for two-line titles with one-line subtitles, and scrolled Home captures check that the color wash leaves with the feed.

Render-count measurements wait for idle artwork and playback/catalog requests, then presented frames with stable counters. A window-bounds notification invalidates the measurement interval and starts it again; any redraw during an interval with stable bounds still fails the assertion.

The carousel check measures release-mode shelf construction across 40 presented redraws with 20, 100 and 500 album cards, reporting cold time and warm mean/p50/p95 in microseconds. It also checks horizontal wheel input, offscreen title changes and responsive heights. In the fresh private environment described below, run `bin/test --release -- --ignored --exact app::native_tests::performance::carousel_render_work_and_height_stay_bounded_while_scrolling --nocapture`. These timings cover shelf construction, not total frame time or song startup. Shelf heights reuse their text measurements until content or layout changes.

To measure local guest playback on Linux, run `bin/test --release -- --ignored --exact player::tests::measure_local_guest_playback --nocapture` with the usual libmpv and yt-dlp and a private, RAM-backed `XDG_RUNTIME_DIR`. The test reports five first plays and 200 warm switches as JSON, measured from the play request to the first advancing position. It uses silent local WAV files and null audio. Compare runs on the same machine with the same libmpv and yt-dlp versions.

The Suggestions check compares Refresh work with 500 and 5,000 loaded songs. It reports eight samples per size after one warm-up. Use the same private environment and run `bin/test --release -- --ignored --exact app::native_tests::suggestions::suggestions_refresh_cost_with_loaded_playlist --nocapture`. Request and response times measure local processing with the network intercepted. The window timing includes two frame waits and is useful for comparing runs on the same display.

These tests use Cargo's unoptimized test profile with debug assertions, the same assertions enabled by `cargo run`. The render check includes a Home response with selected and unselected chips and a network error at 956px, then clicks its dismiss button. Run each test in a separate process and fresh environment. GPUI's native application loop cannot be restarted within one test process. This recipe also gives the playback check a synthetic yt-dlp version mismatch, to exercise the guest fallback notice without an account:

```sh
real_ytdlp=$(command -v yt-dlp)
for test in $(bin/test -- --ignored --list | sed -n \
  -e '/^app::native_tests::performance::carousel_render_work_and_height_stay_bounded_while_scrolling: test$/d' \
  -e '/^app::native_tests::suggestions::suggestions_refresh_cost_with_loaded_playlist: test$/d' \
  -e '/^app::native_tests::/s/: test$//p' \
  -e '/^app::player_page::comments::tests::native_/s/: test$//p'); do
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

Search suggestions allow two active reads and keep only the latest queued query. A slow earlier read can finish in the background while the current query shows its suggestions. Results still require the exact query and session. The worker test holds the older read until the new response arrives and checks that the older response is discarded. An explicit session rejection uses the normal reconnect path. After verified renewal, the focused query retries with the updated session. The native search check exercises this with a synthetic account.

At 850 × 560, the native keyboard check opens long song and podcast episode menus, including the episode subtitle. It verifies that End, Home and backward Tab reveal the focused action, and that reopening starts at the top.

The search suggestions panel follows the field's width and scrolls when the rows exceed the available window height. Native X11 checks at 850 × 560 verify clicks inside and outside the panel and that arrow selection and wheel scrolling reveal the last row for pointer input.

## Player waits

The fast lookup runs in Rust with a cookie-free HTTP client and the VISIONOS player context reviewed against yt-dlp 2026.08.19. It skips the watch page and manifests, reuses connections, and caches visitor data from the first Music homepage bootstrap in memory. Short-lived lookup threads deliver completions through the player's eventfd, independently of the sequential Python resolver. A click never waits behind or restarts the Python resolver; preparation the click made obsolete is cancelled. Visitor data and pauses reset when the account changes. Late native results cannot satisfy a full lookup or load an obsolete track.

Preparation uses the native path too, except signed-in High audio uses the full account extractor for Premium formats. Video above 1080p always uses the full extractor. Native format selection follows the existing quality selectors and YouTube's yt-dlp sort order. A selected format that needs signature or `n` deciphering falls back rather than lowering quality. All URLs pass the same HTTPS/googlevideo and expiry validation as prepared streams, including separate video/audio URLs.

Each native HTTP request has a 3-second timeout, within one 5-second lookup budget. Native failures fall back to the Python resolver, leaving most of the player's 30-second request deadline. A bot check refreshes visitor data once. A repeated bot check or unreadable homepage pauses native lookup for 10 minutes; a stall pauses it for one minute. The app's one-time reload after mpv cannot open a stream uses the full extractor and a fresh playback ID. Python now owns only the full yt-dlp instance and its in-memory player-script cache. Unsupported Python installs keep the one-shot CLI fallback.

`resolver_lanes_keep_clicks_fast_and_prepare_premium_audio` covers routing. Native fixture tests cover quality classes, bitrate fallbacks, codec ordering, video caps, missing formats, deciphering, visitor reuse, cookie isolation and cooldowns. `native_click_during_full_preparation_does_not_wait_or_restart_python` blocks a synthetic full extractor and checks native completion through the actor's fd wait without changing the extractor PID. `native_failures_fall_back_to_the_same_full_resolver` and `a_fast_lookup_in_flight_never_answers_a_full_request` cover fallback and reload isolation. The existing ignored `quality_selectors_respect_audio_classes_and_video_caps` contract also compares all 27 native audio/video selector combinations against yt-dlp using the same fixture response, so CI checks native sorting with the pinned release.

The ignored `player::native::tests::live_native_formats_match_ytdlp_and_download` compares all 27 selectors against yt-dlp on four public player responses. It then checks native audio plus video through 1080p with HTTP 206 reads at the start and middle, and downloads every selected byte in bounded ranges. It needs the network and `YT_DLP_TEST_PYTHON`, and never uses an account.

The player actor blocks on mpv IPC, the command eventfd and the resolver's stdout or one-shot extractor pidfd. Native lookup completions and video status changes also signal the eventfd. Video is enabled only after its native render context is ready, since a fast lookup can finish before renderer startup. Resolver startup and requests have 30-second deadlines; native attempts have a 5-second deadline and queued stream URLs wake the actor at expiry. mpv reconnects a dropped stream with its original URL, so a paused song that mpv hasn't buffered to the end is reloaded, still paused at the same position, five minutes before its URL expires. Otherwise a long track or video resumed hours later would end where its buffer ran out. Seek writes and account notices are flushed before sleeping. Synchronous mpv requests use their remaining five-second deadline for reads. Buffered IPC events are processed before waiting, and socket closure reports mpv shutdown. An idle actor with no pending work has no timer; a paused one checks these expiries at least once a minute.

`one_shot_exit_wakes_the_actor_without_mpv_or_commands` and `resolver_startup_and_request_deadlines_wake_without_output` cover extractor notifications and timeouts. The ignored `paused_actor_refreshes_expired_preparation_and_writes_pending_seek` uses synthetic silence and a local extractor to check expiry and seeks without network access or audible output. The ignored `pausing_an_expired_stream_asks_for_a_reload_unless_mpv_buffered_it_to_the_end` checks the paused-song reload the same way.

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

The Linux performance job compares the stripped release binary and the headless local-playback medians with the pull request's base commit. It uses the reviewed yt-dlp from the candidate for both builds and posts the comparison in the Actions summary. The job fails when the binary exceeds the size cap or grows beyond its per-PR budget and flags playback regressions with warnings because shared runners vary. The permitted increases are in `bin/performance-budget.json`. Run `bin/check-performance BASE_COMMIT` on Linux to reproduce the comparison. You can pass the reviewed yt-dlp zipapp as a second argument to avoid downloading it again.

Never commit cookies, account data or keyring contents. Use synthetic values in tests, as the existing ones do.

## Release

Releases are x86_64 Linux binaries built on Ubuntu 24.04. Merging changes only runs CI, without publishing a release or deploying anything. Releases are immutable once published, and only repository admins can create or move `v*` tags; the release build also refuses a tag whose commit is not on `main`.

1. Set the release version in `Cargo.toml`, bumping it for subsequent releases, and refresh `Cargo.lock` with `cargo check`. Merge the reviewed version change.
2. Tag that commit with `git tag vX.Y.Z`, matching `Cargo.toml`, then `git push origin vX.Y.Z`.
3. The Release workflow builds with `--locked`, checks the script-installed artifact in Ubuntu and Fedora containers, checks the `.deb` and `.rpm` with `packaging/test-package.sh` in Ubuntu, Debian, Fedora and openSUSE containers, and publishes the tarball, packages, their SHA-256 checksums, `install.sh` and dependency report to a GitHub Release.

Before tagging, run the Release workflow with `workflow_dispatch` on the branch. It uploads the same files as the `linux-x86_64` workflow artifact and never creates a release. Before the new workflow reaches `main`, dispatch CI on the branch instead; its dispatch-only `release-test` job calls the same read-only release build workflow. Inspect `dependencies.txt` for direct dynamic dependencies, `ldd` and the highest required glibc symbol version.

The manual preflight summary reports stripped Linux binary size against the branch's common ancestor with `main`. Both builds use `cargo build --release --locked` on the same runner. The summary also lists the sizes of the candidate tarball, Debian package and RPM. It records local guest first-play and warm-switch timings using the same null-audio test described above. Those timings measure local player work and exclude YouTube network latency.

Download the artifact and test the installer without touching an existing installation:

```sh
home=$(mktemp -d)
HOME="$home" XDG_DATA_HOME="$home/data" sh install.sh --archive /absolute/path/yt-music-gpui-X.Y.Z-x86_64-linux.tar.gz
HOME="$home" XDG_DATA_HOME="$home/data" sh install.sh --check-deps
HOME="$home" XDG_DATA_HOME="$home/data" sh install.sh --uninstall
rm -r -- "$home"
```

Keep the matching `.tar.gz.sha256` beside the local tarball. The app and helpers still need their normal runtime libraries. The installer reuses an existing private yt-dlp when its hash matches the pin and downloads it over HTTPS otherwise. It downloads private Deno if needed. Use the isolated desktop recipe above to test playback, with the installed binary and a muted library. Container `--version` checks do not establish working graphics, audio or sign-in on those desktops.

`src/ytdlp_account.py` holds the reviewed yt-dlp version and zipapp SHA-256 plus the fallback Deno version, archive SHA-256 and download size. Packaging copies them into `runtime-pins.json`, and the release build uses them to download and verify the yt-dlp and Deno bundled in the `.deb` and `.rpm`; neither the installer nor the packages have independent version pins. When changing them, verify the official release hashes, run `bin/test-install` and both yt-dlp contracts, and test the zipapp's resident resolver and bundled EJS solver. The app never downloads tools or solver scripts.
