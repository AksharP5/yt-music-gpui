# Security policy

YouTube Music GPUI signs in with your browser's YouTube session, so a bug that leaks or misuses that session is a security issue.

## Reporting a vulnerability

Report it privately through [GitHub's private vulnerability reporting](https://github.com/AksharP5/yt-music-gpui/security/advisories/new), not in a public issue. Include the version or commit, your distribution and desktop, and the steps to reproduce. Don't include real cookies, tokens or account details; a description of where they appear is enough.

You'll get a reply within a week. Fixes land on `main`; there are no maintained release branches.

## What the app does with your session

- It reads YouTube cookies from your default browser profile only when you connect an account, and saves the confirmed session in your desktop keyring through Secret Service on Linux, or in your login keychain on macOS.
- On macOS the session is a generic password item named `yt-music-gpui`. Its access list holds only the app build that saved it, so another program, or a newer build of the app, has to ask you before reading it. Saving replaces any existing item rather than updating it, so an item another program created first cannot keep a wider access list. For Chromium-family browsers the app itself reads the browser's Safe Storage key from your keychain, so macOS asks on the app's behalf, and passes it to the browser reader on its stdin. The reader runs on the app's own Python, or Apple's `/usr/bin/python3` for source builds, never a Python in your home folder. Choosing **Always Allow** trusts only that build of the app, never Python or the `security` tool. Safari is not supported: its cookies are readable only by apps with Full Disk Access.
- On Linux, if no Secret Service is running, it saves nothing until you choose **Save without a keyring**. That stores the session unencrypted in `account-session` in the app's data folder (`$XDG_DATA_HOME/yt-music-gpui`, usually `~/.local/share/yt-music-gpui`), with mode 0600. The file is written to a new private temporary file and renamed into place, and the app refuses to read it if it is a link, belongs to another user or is readable by anyone else. Firefox, and Chromium-family browsers without a keyring, keep the same cookies unencrypted in your home folder. While the file exists, a renewed session is written back to it; once a keyring saves the account, the file is deleted.
- Without that choice it never writes cookies to disk in plaintext, except the copy of a locked browser database on macOS described below. The browser reader hands the session back through a pipe on macOS and an in-memory file on Linux. Signed-in playback writes the cookies and the account to play as to yt-dlp's stdin. They never appear in its command line, its environment or a file, and yt-dlp keeps them in memory with no cookie file to save them back to.
- Playback runtime files (the mpv socket and the extractor script) live in a private directory under `$XDG_RUNTIME_DIR` (on macOS, a directory under `/tmp` that the app checks is owned by you and closed to everyone else), and core dumps are disabled so credentials can't end up in one.
- When the browser holds its cookie database locked, the app reads a copy instead. On Linux the copy is in RAM. macOS has no RAM-backed folder, so the copy goes to a new folder only you can open in your per-user temporary directory and is deleted as soon as it is read. If the app crashes during that read, the `yt-music-session-*` folder stays in the temporary directory until macOS clears it. Firefox and Zen keep that database unencrypted in your home folder anyway; Chromium-family values stay encrypted in the copy.
- Signing out removes the saved session from the keyring and deletes the session file. Switching accounts replaces it.
- An empty `account-saved` file in the app's data folder records that the keyring holds an account, so a keyring that can't be read is reported only to people who signed in. It holds no account data, and signing out removes it.

In scope: anything that exposes, persists or sends the session somewhere other than YouTube; local privilege or file-permission problems; code execution through crafted service responses. Out of scope: YouTube's own services, and anything that requires an already-compromised user account on your machine.

## Release installer

The installer trusts this repository's release build, its build dependencies, GitHub's HTTPS service and your distribution's package repositories. The app tarball and checksum come from the same GitHub Release. That checksum detects corrupted or partial downloads, not a compromised maintainer account or release. Each release file also has a GitHub build provenance attestation that records the workflow run, tag and commit that built it. To check that a download came from this repository's release workflow, run `gh attestation verify FILE --repo AksharP5/yt-music-gpui`. It shows where a file was built, not that the source is safe.

A downloaded yt-dlp zipapp must match its upstream checksum and the hash reviewed in `src/ytdlp_account.py`. During upgrades, the installer can reuse an existing private copy if its ownership, permissions and reviewed hash all match. The optional private Deno archive must match its reviewed hash too. These pins reject changed upstream assets; they cannot protect against a compromised app release that changes the pins. Updating helpers requires a new reviewed app release and re-running the installer. The app never downloads tools or JavaScript solver scripts.

Signed-in and some guest playback runs YouTube's player JavaScript through Deno with yt-dlp's flags, which grant no file, process, network or environment permissions; only Deno's own cache and storage area are writable. This is Deno's permission sandbox, not an operating-system sandbox: a bug in Deno's JavaScript engine could give that code your user's access. Keep a system Deno updated; the private copy updates when you re-run the installer.

The installer refuses root, asks before running any package command with sudo, rejects archive paths and links, and anchors destination writes to checked directory descriptors. The app only prefers private tools whose file and parent directories through `HOME` belong to the current user and are not writable by group or others. This does not defend against root or another process already running as your user. Uninstall checks its manifest and file hashes; it keeps modified files, library data, settings and saved accounts.

## Packages

The `.deb` and `.rpm` come from the same release build and binary as the tarball and trust the same sources, plus the nFPM packaging tool, pinned by version and SHA-256. The build downloads the yt-dlp zipapp and Deno and fails unless both match the reviewed hashes in `src/ytdlp_account.py`, then ships them in `/usr/lib/yt-music-gpui`.

The packages are not signed. Their `.sha256` files come from the same GitHub Release, so they detect corrupted downloads, not a compromised maintainer account or release. Installing a local package as root runs no package scripts from this project. The bundled yt-dlp and Deno change only when you install a newer package; your distribution does not update them, so Deno security fixes reach packaged installs only through a new app release.

The app uses the packaged helpers only when each file and every directory up to `/` belongs to root and is not writable by group or others. The packaged app prefers them over installer copies in your home folder; the script-installed app and source builds prefer their own.

Each release build installs the `.deb` in clean Ubuntu 24.04 and Debian 13 containers and the `.rpm` in clean Fedora 43 and openSUSE Tumbleweed containers. It checks that dependencies resolve, the packaged app prefers the bundled helpers over a trusted copy in HOME while a copy outside `/usr/bin` prefers the HOME copy, the app refuses the bundled helpers once their directory is writable by others or owned by another user, yt-dlp accepts the bundled Deno, and removal leaves none of the package's files. It does not test graphics, audio, sign-in or playback.

## macOS app

The `.dmg` comes from the same release workflow, built on GitHub's macOS 15 runner. The app is signed ad hoc, not with an Apple Developer ID, and is not notarized, so macOS asks you to approve it once. Its `.sha256` detects corrupted downloads, not a compromised maintainer account or release. Like every release file, it has a build provenance attestation you can check with `gh attestation verify`, as described under [Release installer](#release-installer).

Its libmpv is built in the release workflow from mpv, FFmpeg, libplacebo, libass, dav1d, FreeType, FriBidi and HarfBuzz sources pinned by SHA-256 in `packaging/macos-libmpv.sh`, and loads only macOS frameworks. `Contents/Resources/licenses/LIBRARIES.txt` lists their versions, licenses and sources. The yt-dlp zipapp and Deno must match the reviewed hashes in `src/ytdlp_account.py`, and the Python that runs yt-dlp must match the hash in `packaging/macos-dmg.py`. All of these update only with a new app release.

The app uses the yt-dlp, Deno and Python in its own `Contents/Resources` only when each file and every directory up to the `.app` belongs to the owner of the app's executable and is not writable by group or others. Anyone who could change them could change the app itself. Otherwise it falls back to a trusted copy in your home folder, then `PATH`.
