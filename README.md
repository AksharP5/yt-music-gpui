# YouTube Music GPUI

A native Linux music client built in Rust with GPUI. It browses YouTube Music's live catalog and plays audio and video through libmpv and yt-dlp. The interface follows the current Omarchy theme and runs on Wayland or X11.

This is an independent client. The public-catalog player is working. Account integration is under development. The reviewed Google connection is available for live verification, but account sync and authenticated playback have not yet passed the cross-client checks. See [the parity checklist](docs/parity.md) for the remaining work.

## What works

- Home filters and Explore, new releases, charts with country selection, mood and genre pages, search suggestions and category filters, and artist, album, and playlist browsing.
- Streaming audio, pause, seek, previous/next, volume, mute, audio quality and playback speed.
- Native video playback, fullscreen, saved video quality limits and service-provided Song/Video alternatives.
- A persistent queue with add next, remove, reorder, clear, shuffle upcoming tracks, and repeat off/all/one.
- Radio, continued autoplay, lyrics, song credits and related music when the service provides them.
- Podcast show and episode pages, full descriptions, chapter navigation, show navigation and episode duration labels.
- Local liked songs, saved collections, and recently played songs.
- Keyboard controls, native text input, and a compact sidebar on narrow windows.
- Media keys and desktop playback controls through the app's MPRIS service.

## Build and run

The development machine uses Omarchy on x86-64, Rust 1.99, mpv 0.41, and yt-dlp 2026.08.19. Cargo.lock pins the dependency graph, including GPUI 0.3.9.

You need a current Rust toolchain, a C toolchain and pkg-config, libxcb, libxkbcommon, libxkbcommon-x11, a working Vulkan driver, libmpv, EGL/GLES, Wayland client/EGL libraries, Xlib, Xext and yt-dlp. On Arch, mpv supplies libmpv and libglvnd supplies EGL/GLES. Omarchy already supplies most of these. `yt-dlp` is resolved from PATH or `~/.local/bin/yt-dlp`.

```sh
bin/build
target/release/yt-music-gpui
```

Builds default to two jobs to limit peak memory. Set `CARGO_BUILD_JOBS` to change that. The first build compiles GPUI and its graphics dependencies and takes several minutes.

To install the built binary, icon, and app launcher entry for your user:

```sh
bin/install
```

The installer writes to `~/.local/bin` and `$XDG_DATA_HOME`, defaulting to `~/.local/share`. It does not install system dependencies. Alternatively, build an Arch package by running `makepkg` inside `packaging/`, then install that package with your package manager.

Connect from Settings or launch with `--connect`. Sign in only in the dedicated Google window, select your Google or brand account, then return to Settings and choose **Use selected Google account**. A rejected verification keeps that window open so you can retry. **Cancel connection** closes it and preserves the previous connection. Cookie imports are unsupported because they cannot identify the selected Google or brand account. The connection uses the OS keyring and anonymous RAM files for playback credentials. It does not create persistent plaintext cookie exports or inspect existing browser profiles. Authenticated playback requires the reviewed yt-dlp version 2026.08.19 and matching account identity from the service. The desktop runtime directory must be private and RAM-backed. Only one app instance may run at a time to protect shared account and library state.

Launch with `--guest` to skip restoring a saved account from the keyring. This does not delete the saved connection.

## Controls

| Key | Action |
| --- | --- |
| Ctrl K | Focus search; Up/Down selects suggestions; Enter submits |
| Ctrl R | Refresh the current page |
| Space | Play or pause |
| Ctrl Right / Ctrl Left | Next / previous song |
| Right / Left | Seek 5 seconds |
| Up / Down | Adjust volume |
| L | Like the current song |
| Q | Toggle queue |
| F | Toggle fullscreen in the video view |
| ? | Show keyboard help |
| Escape | Close a dialog, exit video fullscreen, close queue or leave search |
| Ctrl Q | Quit |

Escape dismisses the current dialog without closing the queue behind it.

Click an album, artist, or playlist to open it. Click a song to play from that point in the current results. The plus adds a song to the queue or saves a collection. Search keeps its query, category and scope while loading more results or navigating back. Library search supports category filters; uploads search covers all uploads. Both scopes require a connected account.

The video button in the playback bar opens the native video view. Click the picture to pause or resume; double-click it or press F for fullscreen. Audio returns to audio playback. The quality button opens Settings, with limits from 144p through 2160p and a default of 720p. These are maximum heights, not a promise that every recording offers that resolution. Video formats retain the selected audio quality and never fall back to an above-limit stream. Quality and Audio/Video changes preserve position and pause state. When YouTube Music provides an exact Song/Video pair, switching selects that recording; otherwise video plays the current recording. Closing the view keeps playback running. Dialogs temporarily detach the video so their controls remain accessible.

Explore keeps the service's main navigation and mood section. Mood and genre links retain their exact category parameters through navigation, Back and Refresh. Navigation links open pages; collection rows retain their separate save controls.

The song menu offers credits, radio, play next, queue and sharing actions, plus available artist and album links. The current song's menu also exposes lyrics and related music when the service provides them. Credits retain the service's roles and names. Lyrics and credits load independently of radio and autoplay. Pausing while radio loads keeps the returned selection paused. Queue arrows move individual occurrences without changing the selected song. Shuffle changes upcoming songs and preserves the current track. Autoplay recommendations follow your explicit queue; switching autoplay off drops those pending recommendations. Its preference is saved on this device.

Playing a playlist or search page loads its remaining pages as the queue approaches the loaded end. Repeated songs retain their order. A failed page can be retried with Next; recommendations and repeat-all wait until the source ends. Shuffle affects loaded upcoming songs. After restarting or changing accounts, reopen an unfinished playlist to load its remaining pages.

Desktop Next and Previous use the same queue as the app and preserve pause or stop. Stop retains the selected song; Play starts it again. The desktop position, speed, volume and repeat settings follow confirmed app state.

Episode details expose the service's chapter links in a native Chapters dialog. Choosing a chapter starts that episode at its timestamp or seeks the current episode, preserving pause when it is already loaded. Podcast episodes advance through the explicit queue without adding music recommendations.

Settings changes playback speed from 0.25× to 2× and audio quality between Low, Normal and High. Both preferences are saved on this device. Normal is the default quality. Changing quality reloads the current stream at its playback position, keeping its pause state, queue and existing history report.

A stream that fails before playback starts gets one fresh extraction attempt at the same position and pause state. A second failure is reported. Playback errors after a song has started are reported immediately.

Selection uses YouTube's quality labels before bitrate because its variable-bitrate streams can exceed nominal rates. High selects the best stream the service makes available to the account. Available quality depends on the song and membership.

## Data and performance

The library is stored at `$XDG_DATA_HOME/yt-music-gpui/library.json`, or `~/.local/share/yt-music-gpui/library.json`. Writes use an atomic replacement on a dedicated worker. Closing the app flushes the last pending write. An unreadable library is preserved and reported instead of overwritten.

Network requests, image decoding, audio extraction, and disk writes run off the UI thread. Lists render visible rows; horizontal artwork carousels render their viewport with a small overscan. Artwork is limited to 96 decoded images, each at most 320 pixels per edge, with three download workers. Navigation drops unstarted artwork requests from the previous page. Failed artwork can retry on access after 30 seconds. Catalog requests retain the latest pending navigation and playlist-picker request independently, with two workers and two pending slots. The UI redraws playback progress once per displayed second and has no continuous decorative animation. mpv's forward audio cache is capped at 32 MiB. Normal shutdown drains accepted account writes as well as device-state writes.

To measure a running release build on Linux:

```sh
bin/profile APP_PID 30
```

This samples CPU and resident memory for the app and its existing child processes. CPU is a percentage of one core. PSS apportions shared mappings; GPU memory is not included. Measure after artwork loads, and start playback before sampling playback performance so temporary yt-dlp startup work is excluded.

Measured on the development Omarchy desktop on 2026-10-23 with release `4191ff3`, under XWayland at 1920 × 1080 fullscreen. Public guest playback used Muse's "Uprising" at 1280 × 720 with NVDEC, Low audio quality, 1× speed, muted volume and autoplay off. The queue contained 200 tracks after the playlist continuation check. Each steady sample lasted 30 seconds after loading finished.

| State | App CPU | App PSS |
| --- | ---: | ---: |
| Playing 720p video | 3.87% | 522.7 MiB |
| Paused 720p video | 0.37% | 526.3 MiB |

Both samples recorded zero dropped frames; the paused position stayed unchanged. PSS excludes GPU memory. Matched web-client comparisons and connected-account measurements remain pending. An earlier rendering stall and the current verification limits are recorded in [docs/parity.md](docs/parity.md). `bin/profile` includes children spawned from worker threads and excludes transient processes that start after sampling begins.

A separate native Wayland guest session at 956 × 523 recorded 2.87% of one CPU core and 579.0 MiB PSS during 30 seconds of the same 720p stream, with zero dropped frames. That session also passed Stop, renderer detachment, Play and renderer reattachment.

## Validation

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
bin/test
bin/test -- --ignored --nocapture
```

The ordinary tests cover account isolation, catalog and search parsing, stale suggestions and song text, radio continuation, queue reordering and autoplay transitions, library write flushing, and stale playback events. Player checks also cover pause confirmation and stream replacement. An ignored offline test checks quality selectors using yt-dlp. The live ignored tests make real YouTube Music requests and stream audio through mpv's null output, including pause, seek, speed changes and a paused quality reload at 30 seconds. They need network access, mpv, and yt-dlp. Public search categories, suggestions, credits, discovery links and radio continuation have passed live service checks. Interactive XWayland checks passed New releases, Charts, mood categories, category Refresh, suggestions, category search, Back navigation, credits, speed and quality selection, quality preference restoration after restart, queue add/reorder/clear and autoplay settings. Signed-in search scopes and account sync remain unverified.

YouTube's internal API and stream extraction can change. Public guest playback may need an updated yt-dlp; authenticated playback deliberately rejects versions that have not been reviewed. Catalog changes may require a client update. Complete account parity and remaining verification are tracked in docs/parity.md. Account mutations are not yet verified against a signed-in account. GPUI's custom controls currently have limited screen-reader support.

MIT licensed, with the adapted GPUI text input example under Apache-2.0. See NOTICE and LICENSE-APACHE.

The account security design and verification limits are documented in [docs/account-security.md](docs/account-security.md).

The October 23 security pass fixed cleanup when quitting during sign-in, bounded and made keyring lookup cancellable, and made sign-out wait for player and keyring cleanup. Account activation rejects missing or mismatched stable identity. Patched TLS dependencies address two certificate-validation advisories. Real Google sign-in and account sync remain unverified; see [the review](docs/security-review.md#2026-10-23).
