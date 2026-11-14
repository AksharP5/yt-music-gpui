# YouTube Music GPUI

A fast, native YouTube Music client for Linux, built with Rust and GPUI. Audio and video play through libmpv.

![Home page with Quick picks and playlist shelves](docs/screenshot.webp)

- Home, Explore, charts, artists, albums, playlists, podcasts and search, with YouTube Music's layout.
- Audio and video playback with a drag-to-reorder queue, shuffle, repeat, radio and autoplay.
- Player page with Up next, lyrics, credits, comments and related music.
- Sign in to use your library, edit playlists and keep your YouTube Music history in sync.
- Volume levelled between songs like the YouTube Music website (needs mpv 0.38 or newer).
- Autoplay and Next skip songs the app knows you disliked.
- Picks up the last song where you left off after a restart.
- Keyboard shortcuts, media keys and MPRIS desktop controls.

Songs start about 0.2 s after a click, and playback uses around 1% CPU (measured on Arch).

This is an independent client using YouTube Music's unofficial API. YouTube changes can break playback or account features.

## Install

Requires x86_64 Linux with glibc 2.39 or newer and a working Vulkan driver: Ubuntu 24.04+, Debian 13+, Fedora, openSUSE Tumbleweed or Arch.

**Ubuntu, Debian, Fedora, openSUSE:** download the `.deb` or `.rpm` from the [latest release](https://github.com/AksharP5/yt-music-gpui/releases/latest) and install it:

```sh
sudo apt install ./yt-music-gpui_*_amd64.deb                         # Ubuntu, Debian
sudo dnf install ./yt-music-gpui-*.x86_64.rpm                         # Fedora
sudo zypper install --allow-unsigned-rpm ./yt-music-gpui-*.x86_64.rpm # openSUSE
```

To upgrade, install the newer package the same way. Remove it with your package manager.

**Any distribution:**

```sh
curl -fsSL https://github.com/AksharP5/yt-music-gpui/releases/latest/download/install.sh | sh
```

The script installs into your home folder, tells you which system packages are missing and asks before using sudo. Run it again to upgrade. To remove the app:

```sh
curl -fsSL https://github.com/AksharP5/yt-music-gpui/releases/latest/download/install.sh | sh -s -- --uninstall
```

Use either a package or the script, not both. Uninstalling keeps your library, settings and saved account. See [SECURITY.md](SECURITY.md) for what each method checks and trusts.

**Arch, from source:** `packaging/PKGBUILD` builds the latest commit. Copy it anywhere and run `makepkg -si`.

## Sign in

Sign-in needs a running Secret Service keyring (GNOME Keyring or KWallet) and a supported default browser: Zen, Firefox, Chromium, Google Chrome or Brave.

1. Choose **Sign in**.
2. Sign in to YouTube Music in your browser.
3. Return to the app. Your account is verified and saved automatically.

The app copies your browser's YouTube session into your desktop keyring, so the browser can stay closed afterwards. It has the same access to YouTube as that browser session. Private windows and browser containers aren't supported. Start with `--guest` to skip the saved account.

## Keyboard shortcuts

| Key | Action |
| --- | --- |
| / or Ctrl K | Search |
| Space | Play or pause |
| Ctrl Right / Ctrl Left | Next / previous song |
| Right / Left | Seek 5 seconds |
| Up / Down | Volume |
| L | Like the current song |
| Q | Open or close the player page |
| F | Video fullscreen |
| Ctrl R | Reload the page |
| Alt Left | Back |
| Escape | Close a dialog, fullscreen, the player page or search |
| ? | All shortcuts |
| Ctrl Q | Quit |

Right-click a song, card or collection for its menu.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for building from source, tests and running a development build next to your installed app. Report security issues privately as described in [SECURITY.md](SECURITY.md).

## License

MIT. Third-party attributions are in [NOTICE](NOTICE).
