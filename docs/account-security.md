# Account security

Account connection is disabled while the authentication changes are reviewed. Do not use an older prototype sign-in build.

The intended connection flow opens Google's real sign-in page in a dedicated Chromium process. It launches the browser binary directly, disables extensions and sync, and uses a private inherited pipe. There is no TCP debugging listener. The temporary browser profile must be inside the current user's private RAM-backed desktop runtime directory. The browser is terminated and reaped on success, cancellation, timeout, or failure; its temporary profile is owned by that process guard.

Only YouTube-domain cookies are exported from that temporary profile. The API verifies the account with YouTube Music before storing credentials. Credentials are saved through the desktop Secret Service keyring using libsecret's `secret-tool`. Keyring failure has no plaintext-file fallback. Existing browser profiles and saved passwords are not inspected.

Playback receives cookies through a sealed, owner-readable anonymous RAM file. HTTP cookie and authorization headers are marked sensitive so request debug formatting redacts them. The app disables core dumps to prevent a crash from persisting credentials. Credentials are never passed on command lines, written to application logs, or committed to Git.

These controls do not establish immunity from vulnerabilities. A compromised user session or privileged process can still access credentials in memory or through an unlocked keyring. Google browser sessions grant broader YouTube access than a narrowly scoped OAuth grant. The internal API is unofficial and account operations require real-service verification. The app must report rejected writes rather than silently saving a local substitute.

Automated checks cover exact cookie names and domain filtering, fresh authorization, redacted request diagnostics, anonymous sealed credential files, and fragmented private-pipe messages. A live Chromium test verifies the private transport with extensions disabled and no debugging port. A completed security review and cross-client account verification are separate release requirements.
