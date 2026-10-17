# YouTube Music parity

The account on music.youtube.com is the source of truth. A local edit is complete only when the service accepts it. Data changed on the web must be reloaded into the native client. Local preferences and the in-progress playback queue remain device state, matching the web player's behavior.

The initial release implements public catalog browsing, search, audio playback and a local queue. Its local likes and saved collections are not account sync. They must not be silently uploaded when an account is connected.

## Required work

Prepared for signed-in verification: account library categories, song ratings and separate library feedback, playlist create/edit/privacy/delete/add/remove/reorder with occurrence IDs, history removal, and account switching/sign-out. The native controls are wired to service requests and refresh after acknowledgement. Account pages also refresh when the window regains focus; Ctrl R refreshes explicitly. None of this is marked complete until tested with an authenticated account against the web client. The reviewed sign-in flow is available so these checks can proceed.

Native radio, current-song account metadata, lyrics and related-music controls use a separate bounded worker. Public next/lyrics requests and two further radio pages have passed live service checks. Radio preserves the server's queue order and repeated occurrences. Continued autoplay keeps recommendations separate from the explicit queue and adds one occurrence when that queue ends. Turning autoplay off drops pending recommendations. Queue arrows preserve the selected occurrence, and Clear stops playback and cancels pending recommendations.

Search has debounced native suggestions with keyboard selection, public category filters, and pagination that retains the query and filter. Live public checks covered all eight public categories, suggestions and song pagination. Library and uploads scopes are wired but unverified. Combined category/scope filters and search-history deletion remain missing.

Connected playback now queues remote history reporting after mpv starts the track. Reports require matching full server account identity, the requested video ID, a validated YouTube history destination and HTTP 204 acknowledgement. Missing account proof fails closed. Reports and their replies are bound to the exact selected session. Guest playback keeps local history; connected plays are not copied into that guest history. A public player inspection returned a valid tracking destination without submitting history. Actual history persistence on a signed-in account remains unverified.

Settings has a saved 0.25× to 2× playback-speed control. A live guest mpv check passed playback, pause, seek and changing speed to 1.5×. Video selection, fullscreen and quality settings remain missing.

Authenticated behavior and the newest search, radio, queue and Settings controls still need an interactive desktop check; the current desktop automation backend refuses input because its Hyprland plugin is unavailable. The user deferred Google sign-in, so real account checks remain pending.

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
