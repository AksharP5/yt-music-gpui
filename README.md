# YouTube Music GPUI

A native YouTube Music client for Linux, built with Rust and GPUI. Audio and video play through libmpv and yt-dlp. It runs on Wayland and X11 with YouTube Music's dark interface.

- Browse Home, Explore, charts, artists, albums and playlists, with search suggestions and category filters.
- Play audio and video with a persistent, drag-to-reorder queue, shuffle, repeat, radio and autoplay. Click the player bar to open the player page with Up next, lyrics and related music.
- Read lyrics and credits, and browse podcasts with episode descriptions and chapters.
- Connect your account to access your library, edit playlists and record playback history.
- Control playback with keyboard shortcuts, media keys and MPRIS desktop controls.

This is an independent client using YouTube Music's unofficial API. Account support is experimental, and service changes can break playback or account features.

## Build and install

Requires Rust, a C compiler and pkg-config. On Arch, runtime dependencies are `libxcb`, `libxkbcommon`, `libxkbcommon-x11`, `vulkan-driver`, `mpv`, `yt-dlp`, `libglvnd`, `wayland`, `libx11` and `libxext`.

```sh
bin/build
bin/install
```

Launch YouTube Music from your app launcher, or run `target/release/yt-music-gpui` directly. The installer uses `~/.local/bin` and `~/.local/share`, respecting `XDG_DATA_HOME`, and copies the license files alongside. It does not install dependencies.

Builds default to two jobs. Set `CARGO_BUILD_JOBS` to change that.

On Arch, `packaging/PKGBUILD` builds the `yt-music-gpui-git` package from the latest commit on GitHub. Copy it anywhere and run `makepkg -si`.

## Connect your account

Connection requires Python 3, xdg-utils, libsecret's `secret-tool` and a running Secret Service keyring.

1. Choose **Sign in**, or launch with `--connect`.
2. Sign in to YouTube Music in your default browser. Zen, Firefox, Chromium, Google Chrome and Brave are supported, including Omarchy's default-browser launcher.
3. Return to Music. Your account is verified and saved automatically.

The app reads YouTube cookies from your default browser profile and saves the confirmed session in your desktop keyring. For Chromium, Chrome and Brave that is the profile you used last, and the cookies are decrypted with the browser's own Safe Storage key from the same keyring. The app uses your browser session rather than OAuth, so it has that session's YouTube access. It creates no persistent plaintext cookie export. Private windows, containers and partitioned sessions are unsupported.

Signed-in playback runs through an extractor reviewed against one yt-dlp release, pinned in `src/ytdlp_account.py` (currently **2026.08.19**). With any other yt-dlp version, songs play as a guest instead. A weekly CI job tests each new yt-dlp release so the pin can be bumped promptly.

Once connected, the browser can stay closed. The app restores your saved account and can renew rejected sessions from its confirmed profile. Google can still revoke a sign-in. Older saved connections need one reconnect to enable renewal.

Your profile menu shows the connected name and handle. Use **Switch account** to connect another account. Closing the sign-in dialog cancels an unfinished connection.

Use `--guest` to start without restoring the saved account. Guest likes and saved collections stay local and are not uploaded when you connect. Press Ctrl R to reload the current page from YouTube Music, including its order and removals. The app replaces loaded rows and reloads continuations to find the visible song again, up to the previous loaded depth.

Podcast episodes and videos with resume information keep a local listening position across restarts. Connected accounts also read the progress supplied by YouTube Music and report playback and listening intervals. Focus refreshes wait three minutes. They replace the first page in the server's order and preserve loaded deeper pages, removing occurrences that moved into the first page. History refreshes after a playback report do the same. A response without a continuation replaces the entire list. Likes and dislikes update immediately and revert if saving fails. Playlist edits wait for confirmation.

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

Settings includes playback speed, audio quality and video resolution limits. The song menu has mix, queue, playlist, sharing and credits actions when available, in the same order as YouTube Music.

## Development

See [CONTRIBUTING.md](CONTRIBUTING.md) for setup, validation, integration tests and running an isolated instance next to your installed app. To sample CPU and memory for a running release build, use `bin/profile APP_PID 30`.

Local preferences, queue and guest library data live in `$XDG_DATA_HOME/yt-music-gpui/library.json`, defaulting to `~/.local/share/yt-music-gpui/library.json`.

MIT licensed. Third-party attributions and licenses are listed in [NOTICE](NOTICE).
