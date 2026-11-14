# Security policy

YouTube Music GPUI signs in with your browser's YouTube session, so a bug that leaks or misuses that session is a security issue.

## Reporting a vulnerability

Report it privately through [GitHub's private vulnerability reporting](https://github.com/AksharP5/yt-music-gpui/security/advisories/new), not in a public issue. Include the version or commit, your distribution and desktop, and the steps to reproduce. Don't include real cookies, tokens or account details; a description of where they appear is enough.

You'll get a reply within a week. Fixes land on `main`; there are no maintained release branches.

## What the app does with your session

- It reads YouTube cookies from your default browser profile only when you connect an account, and saves the confirmed session in your desktop keyring through Secret Service.
- It never writes cookies to disk in plaintext. Signed-in playback hands credentials to yt-dlp through sealed in-memory files.
- Playback runtime files (the mpv socket and the extractor script) live in a private directory under `$XDG_RUNTIME_DIR`, and core dumps are disabled so credentials can't end up in one.
- Signing out or switching accounts removes the saved session from the keyring.
- An empty `account-saved` file in the app's data folder records that the keyring holds an account, so a keyring that can't be read is reported only to people who signed in. It holds no account data, and signing out removes it.

In scope: anything that exposes, persists or sends the session somewhere other than YouTube; local privilege or file-permission problems; code execution through crafted service responses. Out of scope: YouTube's own services, and anything that requires an already-compromised user account on your machine.

## Release installer

The installer trusts this repository's release build, its build dependencies, GitHub's HTTPS service and your distribution's package repositories. The app tarball and checksum come from the same GitHub Release. That checksum detects corrupted or partial downloads, not a compromised maintainer account or release.

The official yt-dlp zipapp must match both its upstream checksum and the hash reviewed in `src/ytdlp_account.py`. The optional private Deno archive must match its reviewed hash too. These pins reject changed upstream assets; they cannot protect against a compromised app release that changes the pins. Updating helpers requires a new reviewed app release and re-running the installer. The app never downloads tools or JavaScript solver scripts.

Signed-in and some guest playback runs YouTube's player JavaScript through Deno with yt-dlp's flags, which grant no file, process, network or environment permissions; only Deno's own cache and storage area are writable. This is Deno's permission sandbox, not an operating-system sandbox: a bug in Deno's JavaScript engine could give that code your user's access. Keep a system Deno updated; the private copy updates when you re-run the installer.

The installer refuses root, asks before running any package command with sudo, rejects archive paths and links, and anchors destination writes to checked directory descriptors. The app only prefers private tools whose file and parent directories through `HOME` belong to the current user and are not writable by group or others. This does not defend against root or another process already running as your user. Uninstall checks its manifest and file hashes; it keeps modified files, library data, settings and saved accounts.

## Packages

The `.deb` and `.rpm` come from the same release build and binary as the tarball and trust the same sources, plus the nFPM packaging tool, pinned by version and SHA-256. The build downloads the yt-dlp zipapp and Deno and fails unless both match the reviewed hashes in `src/ytdlp_account.py`, then ships them in `/usr/lib/yt-music-gpui`.

The packages are not signed. Their `.sha256` files come from the same GitHub Release, so they detect corrupted downloads, not a compromised maintainer account or release. Installing a local package as root runs no package scripts from this project. The bundled yt-dlp and Deno change only when you install a newer package; your distribution does not update them, so Deno security fixes reach packaged installs only through a new app release.

The app uses the packaged helpers only when each file and every directory up to `/` belongs to root and is not writable by group or others. The packaged app prefers them over installer copies in your home folder; the script-installed app and source builds prefer their own.

Each release build installs the `.deb` in clean Ubuntu 24.04 and Debian 13 containers and the `.rpm` in clean Fedora 43 and openSUSE Tumbleweed containers. It checks that dependencies resolve, the packaged app prefers the bundled helpers over a trusted copy in HOME while a copy outside `/usr/bin` prefers the HOME copy, the app refuses the bundled helpers once their directory is writable by others, yt-dlp accepts the bundled Deno, and removal leaves none of the package's files. It does not test graphics, audio, sign-in or playback.
