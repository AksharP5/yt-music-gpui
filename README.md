# YouTube Music GPUI

A native YouTube Music client for Linux, built with Rust and GPUI. Audio and video play through libmpv and yt-dlp. It runs on Wayland and X11 with YouTube Music's dark interface.

![Home page with Quick picks and playlist shelves](docs/screenshot.webp)

- Browse Home, Explore, charts, artists, albums and playlists, with search suggestions and category filters.
- Play audio and video with a persistent, drag-to-reorder queue, shuffle, repeat, radio and autoplay. Click the player bar to open the player page with Up next, lyrics, read-only comments and related music.
- Read lyrics and credits, and browse podcasts with episode descriptions and chapters.
- Connect your account to access your library, edit playlists and record playback history.
- Control playback with keyboard shortcuts, media keys and MPRIS desktop controls.

This is an independent client using YouTube Music's unofficial API. Account support is experimental, and service changes can break playback or account features.

## Install

On x86_64 Linux:

```sh
curl -fsSL https://github.com/AksharP5/yt-music-gpui/releases/latest/download/install.sh | sh
```

The download will become available with the first GitHub Release. Requires glibc 2.39 or newer, Linux 5.3 or newer, libmpv.so.2 and a working Vulkan driver. These requirements fit Ubuntu 24.04+, Debian 13+, recent Fedora and Arch. Ubuntu 24.04 and Fedora 43 container checks verify library loading and `--version`; desktop playback is verified on Arch only. Other distributions and desktop sessions have not been tested.

The script checks runtime libraries, Python 3.10+, `xdg-open` and `secret-tool`. It prints the exact missing-package command for pacman, apt, dnf or zypper and asks before running sudo. Sign-in also needs a browser and a running Secret Service keyring.

It installs the app to `~/.local/bin`, and the launcher, icon and licenses under `${XDG_DATA_HOME:-~/.local/share}`. The reviewed yt-dlp zipapp stays in `~/.local/share/yt-music-gpui`. If no supported JavaScript runtime is available, it also installs private Deno there. Neither helper is added to PATH. Re-run the command to upgrade. Existing files from another installation method must be removed first.

Downloads use HTTPS and SHA-256 checks. The app checksum detects corrupted or partial downloads; it does not protect against a compromised GitHub account. yt-dlp and optional Deno additionally have reviewed hashes pinned in this repository. See [SECURITY.md](SECURITY.md).

Uninstall with the same script:

```sh
curl -fsSL https://github.com/AksharP5/yt-music-gpui/releases/latest/download/install.sh | sh -s -- --uninstall
```

Only manifest-listed, unmodified installed files are removed. Library data, settings and saved accounts stay. Use the same `XDG_DATA_HOME` for installing and uninstalling. `sh install.sh --check-deps` checks dependencies without installing packages.

## Build from source

Requires Rust, a C compiler, pkg-config and the development libraries listed in [CONTRIBUTING.md](CONTRIBUTING.md). On Arch:

```sh
sudo pacman -S --needed git rust base-devel mpv yt-dlp deno libglvnd wayland libx11 libxext libxcb libxkbcommon libxkbcommon-x11 fontconfig freetype2 vulkan-driver
git clone https://github.com/AksharP5/yt-music-gpui
cd yt-music-gpui
bin/build
bin/install
```

`bin/install` uses `~/.local/bin` and `~/.local/share`, respecting `XDG_DATA_HOME`, and copies licenses alongside. It does not install dependencies or the reviewed extractor. Build defaults to two jobs; set `CARGO_BUILD_JOBS` to change that. Launch YouTube Music from your app launcher or run `target/release/yt-music-gpui` directly.

On Arch, `packaging/PKGBUILD` builds the `yt-music-gpui-git` package from the latest commit on GitHub. Copy it anywhere and run `makepkg -si`.

## Connect your account

Connection requires Python 3.10+, xdg-utils, libsecret's `secret-tool` and a running Secret Service keyring.

1. Choose **Sign in**, or launch with `--connect`.
2. Sign in to YouTube Music in your default browser. Zen, Firefox, Chromium, Google Chrome and Brave are supported, including Omarchy's default-browser launcher.
3. Return to Music. Your account is verified and saved automatically.

The app reads YouTube cookies from your default browser profile and saves the confirmed session in your desktop keyring. For Chromium, Chrome and Brave that is the profile you used last, and the cookies are decrypted with the browser's own Safe Storage key from the same keyring. The app uses your browser session rather than OAuth, so it has that session's YouTube access. It creates no persistent plaintext cookie export. Private windows, containers and partitioned sessions are unsupported.

Clicked songs first use a native, cookie-free player request that skips the watch page and runs independently of yt-dlp. A background lookup cannot hold up that request. The next song is prepared in the background; when signed in with High audio quality, that preparation uses your account to keep Premium audio. Video above 1080p also uses yt-dlp. If the native request fails or needs JavaScript to decode a stream URL, the same play falls back to yt-dlp. If a stream fails to open, the automatic retry uses yt-dlp.

Account playback uses an extractor reviewed against one yt-dlp release, pinned in `src/ytdlp_account.py` (currently **2026.08.19**). The release installer supplies this reviewed version, preferring its private copy over system yt-dlp. With any other yt-dlp version, the full extractor plays as a guest instead. A weekly CI job tests each new yt-dlp release so the pin can be bumped promptly.

Once connected, the browser can stay closed. The app restores your saved account and can renew rejected sessions from its confirmed profile. Google can still revoke a sign-in. Older saved connections need one reconnect to enable renewal.

Your profile menu shows the connected name and handle. Use **Switch account** to connect another account. Closing the sign-in dialog cancels an unfinished connection.

Use `--guest` to start without restoring the saved account. Guest likes and saved collections stay local and are not uploaded when you connect.

### Staying in sync

- Songs you play show up in your YouTube Music history, and listening time is reported the way the web player reports it.
- Podcast episodes and long videos resume where you left off, including progress saved by YouTube Music.
- Likes and dislikes apply immediately and revert if YouTube Music rejects them. Playlist edits wait for confirmation.
- Pages you visited in the last three minutes open instantly; older ones show right away and refresh in place. After any edit you make, the next visit loads fresh, and History refreshes after each play. Back restores your scroll position and loaded rows.
- When you return to the window after three minutes or more, the current page picks up changes made elsewhere in YouTube Music's order, without losing your scroll position. Ctrl R reloads the page right away.

## Controls

| Key | Action |
| --- | --- |
| / or Ctrl K | Search |
| Ctrl R | Reload the current page |
| Alt Left | Go back |
| Space | Play or pause |
| Ctrl Right / Ctrl Left | Next / previous song |
| Right / Left | Seek 5 seconds |
| Up / Down | Adjust volume |
| L | Like the current song |
| Q | Open or close the player page |
| F | Toggle video fullscreen |
| Escape | Dismiss a dialog or leave fullscreen, the player page or search |
| ? | Keyboard help |
| Ctrl Q | Quit |

Tab and Shift+Tab move through every control, arrow keys move through open menus, and right-clicking a song, card or collection opens its menu.

Settings includes playback speed, audio quality and video resolution limits. The song menu has mix, queue, playlist, sharing and credits actions when available, in the same order as YouTube Music.

## Development

See [CONTRIBUTING.md](CONTRIBUTING.md) for setup, validation, integration tests and running an isolated instance next to your installed app. To sample CPU and memory for a running release build, use `bin/profile APP_PID 30`.

Playback sockets and the account extractor live in a private directory under `$XDG_RUNTIME_DIR`. Normal shutdown, SIGTERM and SIGINT remove it. Repeating the same signal forces termination if cleanup hangs.

Local preferences, queue and guest library data live in `$XDG_DATA_HOME/yt-music-gpui/library.json`, defaulting to `~/.local/share/yt-music-gpui/library.json`.

MIT licensed. Third-party attributions and licenses are listed in [NOTICE](NOTICE).
