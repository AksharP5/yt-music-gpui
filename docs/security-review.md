# Account review, 2026-10-17

The source review covered every changed source file between e9cfcdf and 5899fb5. It found no reportable vulnerability in that gated build. This is a bounded review, not a guarantee that the application has no vulnerabilities.

The subsequent hardening and playback checkpoint is 460b17f. The account gate is still off in that revision.

Subsequent hardening binds queued edits, persistence, catalog and song-detail jobs to a verified session. Failed verification preserves the previous session. Connection completion must match the active, uncancelled operation in both the worker and UI. Persistence starts only after UI acceptance and playback credential allocation; revoked writes are removed. Closing during pending sign-out drains account cleanup, with bounded keyring child processes. Sign-out clears navigation history, overlays and in-memory action tokens. Account feedback tokens, occurrence IDs and rating snapshots are excluded from device-state serialization.

Playback now carries the selected account index and full primary/brand identity through sealed RAM. The embedded extractor requires matching server identity before returning formats, rejects logout and conflicting player responses, and uses a single HTTP handler that refuses redirects before forwarding authorization. A missing plugin or different yt-dlp version fails closed. External plugins and user configuration are disabled. Playback uses an acknowledged stop before replacing or closing its private RAM files. Raw extractor diagnostics cannot reach user-facing playback errors. These later changes received a separate bounded source review; they are not covered by the earlier sealed scan.

Validation passed:

- 35 ordinary tests, including session binding, cancelled publication, failed verification, disabled imports, revoked storage, cleanup after UI closure, request redaction, RAM file permissions/lifetimes and account metadata parsing.
- Nine offline extractor cases against installed yt-dlp 2026.08.19, covering multiple Google/brand accounts, returned identity, explicit logout, missing proof, redirects, private context, plugin failure and unsupported versions.
- A live isolated Chromium transport check with no debugging TCP port or inherited extensions.
- Live public guest mpv playback, pause and seek. Invented credentials no longer have a live streaming test because authenticated playback must reject an unconfirmed account rather than fall back to guest playback.
- A synthetic Secret Service store/read/remove round trip under unique test attributes. No existing credential entry was accessed.

No real Google account was used by the checks above. Authenticated compatibility and cross-client mutations remain unverified. Sign-in and saved-session loading are now available for controlled live verification; cookie imports are unsupported. A private runtime lock prevents concurrent app processes from racing shared account state. Its exclusivity, release and symlink refusal have a focused test.

The search and autoplay checkpoint passes 47 ordinary tests and strict all-target Clippy. Suggestions reject replies from a previous query, generation or exact account session. Radio continuation carries its original account identity and rejects another session before making a request. These checks supplement the earlier review; they do not extend the sealed security scan to newer commits. The user deferred sign-in, and no authenticated cross-client check has run.

The history and playback-speed checkpoint passes 53 ordinary tests and strict all-target Clippy, including five history contracts and a validated speed boundary. A separate source review found that the first history implementation checked the video but lacked server account proof. A regression reproduced that gap before it was fixed. Reporting now requires the full selected primary/brand identity and rejects conflicting account hints before accepting the tracking URL. The bounded recheck found no remaining concrete defect in that fix. Public player inspection validated a history destination without submitting it; live guest playback passed pause, seek and speed changes. Actual signed-in history remains unverified.

## Dependency maintenance

687 locked packages were checked against RustSec advisory database commit 999ec89d5d3f10f20be14750ec8c2d8b44e0ac9b. Version requirements were matched using Rust's semver implementation, excluding withdrawn advisories. No current vulnerability or unsoundness advisory matched. Six active transitive dependencies matched unmaintained advisories, with no patched versions listed:

- [zvariant 5.15.0, RUSTSEC-2026-0370](https://rustsec.org/advisories/RUSTSEC-2026-0370.html)
- [zvariant_derive 5.15.0, RUSTSEC-2026-0372](https://rustsec.org/advisories/RUSTSEC-2026-0372.html)
- [zbus_names 4.3.4, RUSTSEC-2026-0371](https://rustsec.org/advisories/RUSTSEC-2026-0371.html)
- [ttf-parser 0.25.1, RUSTSEC-2026-0192](https://rustsec.org/advisories/RUSTSEC-2026-0192.html)
- [rustybuzz 0.20.1, RUSTSEC-2026-0206](https://rustsec.org/advisories/RUSTSEC-2026-0206.html)
- [paste 1.0.15, RUSTSEC-2024-0436](https://rustsec.org/advisories/RUSTSEC-2024-0436.html)

These enter through GPUI's Linux, font, SVG and image dependencies. Their maintenance status needs upstream migration; it is not evidence of a particular exploit. The check excludes the browser, mpv, yt-dlp, OS packages and unpublished vulnerabilities.

Browser-cookie authentication gives the application broader YouTube authority than a narrow OAuth grant. An unlocked desktop keyring or compromised user session remains outside the application's isolation guarantees. See [the account design and limits](account-security.md).
