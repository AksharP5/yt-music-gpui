# YouTube Music parity

The account on music.youtube.com is the source of truth. A local edit is complete only when the service accepts it. Data changed on the web must be reloaded into the native client. Local preferences and the in-progress playback queue remain device state, matching the web player's behavior.

The initial release implements public catalog browsing, search, audio playback and a local queue. Its local likes and saved collections are not account sync. They must not be silently uploaded when an account is connected.

## Required work

Prepared for signed-in verification: account library categories, song ratings and separate library feedback, playlist create/edit/privacy/delete/add/remove/reorder with occurrence IDs, history removal, and account switching/sign-out. The native controls are wired to service requests and refresh after acknowledgement. Account pages also refresh when the window regains focus; Ctrl R refreshes explicitly. None of this is marked complete until tested with an authenticated account against the web client. The reviewed sign-in flow is available so these checks can proceed.

Native radio, current-song account metadata and related-music controls use a separate bounded worker. Lyrics and credits have their own bounded worker, so opening either cannot cancel a radio request. Closing the dialog or changing accounts discards pending text and stale results. Public next/lyrics requests, song credits and two further radio pages have passed live service checks. Radio preserves the server's queue order and repeated occurrences. Continued autoplay keeps recommendations separate from the explicit queue and adds one occurrence when that queue ends. Turning autoplay off drops pending recommendations. Queue arrows preserve the selected occurrence, and Clear stops playback and cancels pending recommendations.

Search has debounced native suggestions with keyboard selection, separate category and scope tabs, and pagination that retains the query, category and scope. Library supports category filters; uploads supports All only. Unsupported combinations fail before a request. Initial account results must contain the expected selected scope tab. Live public checks covered all eight public categories, suggestions and song pagination. Library and uploads scopes remain unverified with a real account. Search-history deletion remains missing.

Discovery preserves service navigation buttons, grid headings and browse parameters. New releases, Charts, Moods & genres and a parameterized mood category passed live guest checks. Explore keeps its four main links above the content and its titled mood section in service order. Navigation links are separate from savable collections. Home filter chips and chart country selection remain missing. Browse continuation beyond the tested search/radio paths remains unverified.

Native discovery checks passed New releases, Charts, the Moods & genres list, distinct Chill and Commute categories, category Refresh, Back and the grouped Explore layout. The ordinary suite passes 62 tests, with 11 optional integration tests ignored. These guest checks do not verify personalized discovery or connected library actions.

Connected playback now queues remote history reporting after mpv starts the track. Reports require matching full server account identity, the requested video ID, a validated YouTube history destination and HTTP 204 acknowledgement. Missing account proof fails closed. Reports and their replies are bound to the exact selected session. Guest playback keeps local history; connected plays are not copied into that guest history. A public player inspection returned a valid tracking destination without submitting history. Actual history persistence on a signed-in account remains unverified.

Settings has saved playback-speed and Low/Normal/High audio-quality controls. Quality changes replace only the stream, preserving its position, pause state, queue and history-reporting marker. Formats follow the service's quality classes, with bitrate fallbacks when those labels are absent. High selects the best stream available for the song and membership. Video selection, fullscreen and video-quality settings remain missing.

The native quality check passed a playing switch to Low, paused High/Normal changes that retained 22 seconds, preference restoration after restart, and resumed High playback with the same two queue entries. A startup failure receives one fresh extraction attempt with the same playback state; a second failure is reported. The retry cannot run after Started, and pausing cannot reset its budget. Public diagnosis observed intermittent stream HTTP 403 responses, so these checks do not establish uninterrupted service availability.

An interactive XWayland check passed native suggestions, category search, navigation to an album, Back restoring the query and category, the credits dialog, speed selection and persistence, queue add/reorder/clear, and autoplay preference persistence. Completed guest pages remain cached; Back restarts unfinished requests instead of restoring a permanently empty page. Signed-in controls, radio/lyrics interactions and natural autoplay still need desktop verification. The user deferred Google sign-in, so real account checks remain pending.

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
