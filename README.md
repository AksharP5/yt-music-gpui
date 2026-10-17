# YouTube Music GPUI

A native Linux music client built in Rust with GPUI. It browses YouTube Music's live catalog and plays audio through mpv and yt-dlp. The interface follows the current Omarchy theme and runs on Wayland or X11.

This is an independent client. The public-catalog player is working. Account integration is under development, and sign-in is disabled until its security review and account verification are complete. See [the parity checklist](docs/parity.md) for the remaining work.

## What works

- Home and Explore, search, and artist, album, and playlist browsing.
- Streaming audio, pause, seek, previous/next, volume, and mute.
- A persistent queue with add, remove, shuffle upcoming tracks, and repeat off/all/one.
- Local liked songs, saved collections, and recently played songs.
- Keyboard controls, native text input, and a compact sidebar on narrow windows.
- Media keys and desktop playback controls when `mpv-mpris` is installed.

## Build and run

The development machine uses Omarchy on x86-64, Rust 1.99, mpv 0.41, and yt-dlp 2026.08.19. Cargo.lock pins the dependency graph, including GPUI 0.3.9.

You need a current Rust toolchain, a C toolchain and pkg-config, libxcb, libxkbcommon, libxkbcommon-x11, a working Vulkan driver, mpv, and yt-dlp. Omarchy already supplies most of these. `yt-dlp` is resolved from PATH or `~/.local/bin/yt-dlp`.

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

Account sign-in, saved-session loading and explicit cookie import are currently disabled. The account connection being developed uses an isolated Google login window, the OS keyring, and anonymous RAM files for playback credentials. It does not create persistent plaintext cookie exports or inspect existing browser profiles.

## Controls

| Key | Action |
| --- | --- |
| Ctrl K | Focus search; Enter submits |
| Ctrl R | Refresh the current page |
| Space | Play or pause |
| Ctrl Right / Ctrl Left | Next / previous song |
| Right / Left | Seek 5 seconds |
| Up / Down | Adjust volume |
| L | Like the current song |
| Q | Toggle queue |
| ? | Show keyboard help |
| Escape | Close help or queue; leave search |
| Ctrl Q | Quit |

Click an album, artist, or playlist to open it. Click a song to play from that point in the current results. The plus adds a song to the queue or saves a collection. The song menu offers play next, queue and sharing actions, plus available artist and album links. Shuffle changes upcoming songs and preserves the current track.

## Data and performance

The library is stored at `$XDG_DATA_HOME/yt-music-gpui/library.json`, or `~/.local/share/yt-music-gpui/library.json`. Writes use an atomic replacement on a dedicated worker. Closing the app flushes the last pending write. An unreadable library is preserved and reported instead of overwritten.

Network requests, image decoding, audio extraction, and disk writes run off the UI thread. Lists render visible rows. Artwork is limited to 96 decoded images, each at most 320 pixels per edge, with three download workers. Catalog requests keep only the latest pending navigation. The UI redraws playback progress once per displayed second and has no continuous decorative animation. mpv's forward audio cache is capped at 32 MiB.

To measure a running release build on Linux:

```sh
bin/profile APP_PID 30
```

This samples CPU and resident memory for the app and its existing child processes. CPU is a percentage of one core. PSS apportions shared mappings; GPU memory is not included. Measure after artwork loads, and start playback before sampling playback performance so temporary yt-dlp startup work is excluded.

Measured on the development Omarchy desktop on 2026-10-15, using the optimized build under XWayland at 956 × 1050. Each steady-state sample lasted 30 seconds; playback used muted audio. These are local measurements, not a comparison with the web client.

| State | GUI CPU | mpv CPU | Combined PSS |
| --- | ---: | ---: | ---: |
| Paused | 0.27% | 0.00% | 242.6 MiB |
| Playing | 0.53% | 0.87% | 253.0 MiB |

The stripped release executable is 33.2 MiB. `bin/profile` includes children spawned from worker threads and excludes transient processes that start after sampling begins.

## Validation

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
bin/test
bin/test -- --ignored --nocapture
```

The ordinary tests cover catalog parsing, duration and video ID validation, queue transitions and shuffle, library write flushing, and stale playback events. The ignored tests make real YouTube Music requests and stream audio through mpv's null output, including pause and seek. They need network access, mpv, and yt-dlp.

YouTube's internal API and stream extraction can change. Update yt-dlp if songs stop loading; catalog changes may require a client update. Complete account parity, downloads, lyrics, and other gaps are tracked in docs/parity.md. Account mutations are not yet verified against a signed-in account. GPUI's custom controls currently have limited screen-reader support.

MIT licensed, with the adapted GPUI text input example under Apache-2.0. See NOTICE and LICENSE-APACHE.

The account security design and verification limits are documented in [docs/account-security.md](docs/account-security.md).
