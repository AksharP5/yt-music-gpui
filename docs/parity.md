# YouTube Music parity

The account on music.youtube.com is the source of truth. A local edit is complete only when the service accepts it. Data changed on the web must be reloaded into the native client. Local preferences and the in-progress playback queue remain device state, matching the web player's behavior.

The initial release implements public catalog browsing, search, audio playback and a local queue. Its local likes and saved collections are not account sync. They must not be silently uploaded when an account is connected.

## Required work

Prepared for signed-in verification: account library categories, song ratings and separate library feedback, playlist create/edit/privacy/delete/add/remove/reorder with occurrence IDs, history removal, and account switching/sign-out. The native controls are wired to service requests and refresh after acknowledgement. Account pages also refresh when the window regains focus; Ctrl R refreshes explicitly. None of this is marked complete until tested with an authenticated account against the web client. Sign-in remains gated.

Native radio, current-song account metadata, lyrics and related-music controls are wired to a separate bounded worker. Public next/lyrics requests have passed a live service check. Radio preserves the server's queue order and repeated occurrences. Authenticated behavior and the new controls still need an interactive desktop check; the current desktop automation backend refuses input because its Hyprland plugin is unavailable.

- [ ] Google account connection, reconnect, account identity and account switching.
- [ ] Personalized home, mixes and discovery, moods, genres, charts and new releases.
- [ ] Account library with songs, albums, artists, subscriptions, playlists and liked music.
- [ ] Song likes, dislikes and unrated state; separate add/remove from library.
- [ ] Save/remove albums and playlists; subscribe/unsubscribe from artists and shows.
- [ ] Playlist create, rename, description, privacy, delete, add/remove/reorder tracks and duplicate handling.
- [ ] Library changes visible on the web and web changes visible in the app.
- [ ] Remote history, playback reporting and history removal.
- [ ] Search suggestions, category filters and pagination.
- [ ] Radio, autoplay, queue add-next, reorder, clear, shuffle and repeat.
- [ ] Lyrics, related music, credits and links to artists and albums.
- [ ] Audio/video selection, fullscreen, playback quality and speed.
- [ ] Podcasts, episodes, resume position and episode controls.
- [ ] Personal uploads, upload management and downloads where the service permits them.
- [ ] Sharing, collaborative playlists and account/settings actions.
- [ ] Cast/device playback capabilities supported by the service.
- [ ] Cross-client verification and native performance measurements for the connected account.

Service-specific features need evidence of working behavior. Tests of request JSON alone do not prove account synchronization or complete feature parity.
