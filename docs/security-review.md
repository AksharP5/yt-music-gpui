# Account review, 2026-10-17

The source review covered every changed source file between e9cfcdf and 5899fb5. It found no reportable vulnerability in that gated build. This is a bounded review, not a guarantee that the application has no vulnerabilities.

Subsequent hardening binds queued edits and catalog jobs to a verified session, preserves the previous session when verification fails, revokes playback credentials during sign-out, blocks authenticated redirects, and applies the disabled account gate to imports and saved sessions. Account feedback tokens, occurrence IDs and rating snapshots are excluded from device-state serialization. Playback uses an acknowledged stop before replacing or closing its private RAM cookie jar. Raw extractor diagnostics cannot reach user-facing playback errors.

Validation passed:

- 26 ordinary tests, including session binding, failed verification, disabled imports, cookie filtering, request redaction, RAM file permissions/lifetimes and account metadata parsing.
- A live isolated Chromium transport check with no debugging TCP port or inherited extensions.
- Live mpv playback with invented initial cookies, replacement cookies and cleared credentials.
- A synthetic Secret Service store/read/remove round trip under unique test attributes. No existing credential entry was accessed.

No real Google account was used. Account identity selection, authenticated playback and cross-client mutations remain unverified. New account sign-in, import and saved-session loading remain disabled.

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
