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

The credits and scoped-search checkpoint passes 56 ordinary tests and strict all-target Clippy. Song text uses immutable account snapshots on a separate bounded worker; account changes and dialog closure invalidate pending results. Account search rejects unsupported combinations, explicit logout, missing scope tabs and explicitly unselected results. Public credits, search categories and pagination passed live checks. Native guest checks covered suggestions, category search, Back navigation, credits, persisted speed, queue reordering/clearing and autoplay settings. These are feature and boundary checks, not an extension of the sealed scan or a substitute for authenticated verification.

The audio-quality checkpoint passes 60 ordinary tests and strict all-target Clippy. Its bounded source review found two pause-confirmation races, both corrected and covered by focused regressions. Stream replacements retain pause intent until mpv confirms the latest command, including no-op assignments and intervening external controls. New playback IDs isolate old load failures and end events. Changing quality keeps the existing history-reporting marker and does not replace the queue, radio or account session. An offline selector check and live muted guest playback passed, including a Low-quality reload paused at 30 seconds and subsequent external unpause. Authenticated quality selection and real account history remain unverified.

One native High-quality load failed. Public diagnosis found intermittent HTTP 403 failures affecting Normal too, while repeated High loads at the same paused position succeeded. The native failure itself remained unclassified because extractor diagnostics are kept private. The app now makes one fresh extraction attempt after a track-specific failure before Started, preserving playback and history state. Global/process failures and failures after Started are reported immediately. A focused state test and bounded source recheck verified the retry budget, stale IDs and account isolation. mpv's public error event does not distinguish HTTP 403, 404 or 500, so this recovery applies to any track-specific startup failure.

The final native check passed High playback, paused High/Normal changes at 22 seconds, resumption, unchanged queue selection and quality preference restoration after restarting the app. The preceding build also passed a playing switch to Low. These checks used a muted guest fixture; they did not access a real Google account or verify signed-in history.

The discovery checkpoint passes 62 ordinary tests and strict all-target Clippy. Navigation targets keep their validated browse IDs and bounded encoded parameters in the existing fixed-endpoint, session-bound request path. Malformed links are rejected, and navigation items have no collection-save controls. Focused parsing tests, live public destinations and native category/Refresh/Back checks passed. This adds no credential path and does not extend the earlier sealed scan or establish authenticated parity.

The browse-filter checkpoint passes 65 ordinary tests and strict all-target Clippy. Home reset actions and chart country choices are parsed from service state. The client extracts validated browse targets instead of executing service commands, and only Charts accepts a form value, restricted to one uppercase two-letter country token. Country changes require a service-confirmed matching selection. The picker accepts events only from its active entity, and account changes drop it and its subscription. A bounded source review found no concrete issue in this change. Public reads passed Home selection/reset and Canada, United Kingdom and Global charts. Signed-in parser compatibility and cross-client behavior remain unverified.

Native checks caught modal key handling swallowing printable input. The fix consumes only handled navigation keys, and the app's modal guard runs before search/refresh shortcuts. Native country search, selection, empty results, Escape and Refresh/Back passed. A separate local fixture verified playlist title/description typing, Tab and submission without an account. These checks establish input behavior, not successful remote playlist edits.

The pagination checkpoint passes 69 ordinary tests and strict all-target Clippy. Each continuation owns its original visitor header and exact account snapshot; a changed account is rejected before preparing a request. Visitor headers are validated and marked sensitive, and continuation Debug output is redacted. No visitor state is shared globally or persisted. Home and section-list continuation pages use their own page token instead of a nested shelf token. Live public Home checks returned two further pages; public search categories, suggestions and song pagination still pass. Authenticated pagination remains unverified.

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
