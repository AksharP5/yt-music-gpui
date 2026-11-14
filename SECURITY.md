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

In scope: anything that exposes, persists or sends the session somewhere other than YouTube; local privilege or file-permission problems; code execution through crafted service responses. Out of scope: YouTube's own services, and anything that requires an already-compromised user account on your machine.

## Release installer

The installer trusts this repository's release build, its build dependencies, GitHub's HTTPS service and your distribution's package repositories. The app tarball and checksum come from the same GitHub Release. That checksum detects corrupted or partial downloads, not a compromised maintainer account or release.

The official yt-dlp zipapp must match both its upstream checksum and the hash reviewed in `src/ytdlp_account.py`. The optional private Deno archive must match its reviewed hash too. These pins reject changed upstream assets; they cannot protect against a compromised app release that changes the pins. Updating helpers requires a new reviewed app release and re-running the installer. The app never downloads tools or JavaScript solver scripts.

The installer refuses root, asks before running any package command with sudo, rejects archive paths and links, and anchors destination writes to checked directory descriptors. The app only prefers private tools whose file and parent directories through `HOME` belong to the current user and are not writable by group or others. This does not defend against root or another process already running as your user. Uninstall checks its manifest and file hashes; it keeps modified files, library data, settings and saved accounts.
