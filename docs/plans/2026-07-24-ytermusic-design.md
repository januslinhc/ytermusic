# Ytermusic Product and Technical Design

**Status:** Approved  
**Date:** 2026-07-24  
**Working name:** Ytermusic  

## Purpose

Ytermusic is a polished, keyboard-first YouTube Music terminal player written in
Rust. It starts without an account, can optionally connect to a user's YouTube
Music session, and treats music discovery, regional charts, podcasts, queue
management, and reliable playback as one coherent experience.

The first release supports macOS, Linux, and Windows.

## Product Goals

- Work anonymously without an API key or setup wizard.
- Optionally expose a user's library, playlists, subscriptions, and history
  through browser-cookie authentication.
- Search and browse songs, albums, artists, videos, playlists, podcasts, and
  episodes.
- Show charts and trending music for a user-selected country.
- Support sequential, shuffled, repeat-one, repeat-all, and endless-radio
  playback.
- Fade audio cleanly during play, pause, stop, and track changes.
- Resume queues, sessions, and podcast positions after restart.
- Remain responsive during network calls, stream resolution, and playback.
- Offer a discoverable interface while retaining efficient Vim-style controls.
- Ship and test consistently on macOS, Linux, and Windows.

## Non-Goals for the First Release

- Downloading or permanently saving copyrighted media.
- Video rendering.
- An overlapping, dual-deck DJ crossfade. The first release provides
  configurable fade-in and fade-out transitions and gapless playback when fades
  are disabled.
- Mobile or graphical desktop interfaces.
- Bundling Google credentials or a YouTube Data API key.
- Cloud synchronization beyond the state YouTube Music already stores for an
  authenticated account.

## Key Decisions

### Hybrid application

The application and interface are native Rust. Ratatui and Crossterm provide the
cross-platform terminal UI. YouTube Music metadata is accessed through a
replaceable provider layer based primarily on `ytmapi-rs`. Playback uses
`yt-dlp` for stream resolution and `mpv` for decoding and audio output.

This approach avoids rebuilding rotating YouTube stream extraction, codec
support, seeking, and platform-specific audio output while keeping all product
state and behavior in Rust.

### Anonymous-first authentication

The application starts in anonymous mode. Account connection is optional and
imports an existing browser session; Ytermusic never asks for a Google
password.

Cookie import uses `yt-dlp --cookies-from-browser` support for major browsers.
The temporary cookie jar has restricted permissions and is removed promptly.
The minimum required authentication material is stored through the operating
system credential vault. Secrets are excluded from logs, SQLite, crash
messages, and diagnostics.

If authentication expires or fails, the application explains the lost
capabilities and continues anonymously.

### Replaceable provider boundary

YouTube Music's internal API is unofficial and can change. All remote data
operations therefore sit behind domain-oriented traits. API response types do
not leak into application state or UI code.

The primary provider handles:

- typed search and search suggestions;
- song, album, artist, and playlist details;
- podcasts, shows, episodes, and chapters where available;
- mood and genre collections;
- authenticated library, likes, subscriptions, playlists, and history;
- radio/watch recommendations; and
- country-specific charts.

Regional charts use YouTube Music's browse context with an ISO 3166-1 alpha-2
country selection. An optional official YouTube Data API adapter may be
configured as a fallback for public regional popularity results. Cached content
is used when a transient provider failure occurs.

### External playback process

Ytermusic launches a private `mpv` process with user configuration isolated by
default. It controls the player through JSON IPC:

- Unix-domain sockets on macOS and Linux;
- named pipes on Windows.

`yt-dlp` resolves a selected YouTube ID just before playback. Resolved media
URLs are cached only briefly because they expire. The resolver prefers the best
audio-only format and can pass the configured browser session when required.

## User Experience

### Layout

The normal wide layout contains:

1. A left navigation rail for Home, Search, Charts, Podcasts, Library, History,
   and Settings.
2. A central content pane for results and collections.
3. A right queue pane for upcoming items.
4. A persistent bottom player for title, creator, progress, volume, mode,
   quality, and status.

The layout progressively collapses the queue and navigation rail on narrow
terminals. Tiny terminals show the player and focused content rather than
failing or rendering corrupt borders.

### Interaction

- Arrow keys and Vim movement keys navigate.
- `/` focuses global search.
- `Space` toggles play and pause.
- Dedicated shortcuts skip tracks, seek, change volume, cycle repeat, toggle
  shuffle, and open the queue.
- A command palette exposes every action by name.
- A help overlay shows shortcuts relevant to the current view.
- Mouse selection and scrolling are supported but never required.
- Toasts communicate short-lived results; a diagnostics view preserves useful
  errors and dependency information.

### Queue modes

The queue owns stable item identities and distinguishes the logical queue from
its playback order.

- **Sequential:** advance through the logical order.
- **Shuffle:** create a stable randomized permutation without duplicates,
  preserving the current item.
- **Repeat one:** replay the current item.
- **Repeat all:** wrap at the end of the active order.
- **Endless radio:** request related items before the queue runs dry and avoid
  recently played duplicates.

Manual additions, removals, and reordering update both logical and active orders
without unexpectedly restarting the current track.

### Fades

Fade durations are configurable, including zero.

- Starting or resuming ramps from silence to the user's target volume.
- Pausing, stopping, or intentionally replacing a track ramps down first.
- Automatic track completion uses the configured end fade when duration is
  known.
- A newer playback command cancels an older envelope safely.
- The user's target volume is separate from the transient effective volume, so
  cancellation cannot leave playback permanently quiet or too loud.
- When fades are disabled, `mpv` uses its normal gapless playlist behavior.

### Podcasts

Podcast episodes share the queue and player with songs but add:

- saved progress and automatic resume;
- configurable backward and forward skip intervals;
- playback speed control;
- chapter navigation when chapter metadata is available; and
- played/unplayed state.

## Internal Architecture

Ytermusic uses a unidirectional event architecture:

```text
keyboard/mouse/timer
        |
        v
TUI event loop -> App reducer -> View state -> Ratatui renderer
                       |
            +----------+-----------+
            |          |           |
            v          v           v
       Provider     Queue       SQLite
            |          |
            v          v
         Resolver -> Player state machine -> mpv IPC
                                              |
                                              v
                                  progress/end/error events
```

### Modules

- `app`: application state, actions, reducer, focus, and command dispatch.
- `ui`: responsive layouts, widgets, dialogs, help, palette, and themes.
- `domain`: provider-independent songs, collections, podcasts, regions, and
  playback models.
- `provider`: YouTube Music adapters, authentication, response mapping, and
  cache policy.
- `queue`: ordering, shuffle, repeat, radio fill, and mutation rules.
- `resolver`: cancellable `yt-dlp` jobs and short-lived resolution cache.
- `player`: `mpv` lifecycle, IPC transport, commands, observations, and
  playback state machine.
- `fade`: clock-driven volume envelopes.
- `storage`: SQLite connection, migrations, repositories, and checkpoints.
- `config`: user settings, defaults, validation, and platform paths.
- `platform`: process lookup, Unix sockets, Windows named pipes, credential
  vault, signals, and browser-cookie import.
- `diagnostics`: structured, secret-safe errors and health checks.

### Concurrency

Tokio runs input ticks, provider requests, resolution, storage work, and player
IPC without blocking terminal rendering. Background operations return typed
actions through one bounded application channel.

Every replaceable request, such as search or opening a collection, carries a
generation identifier. Late responses from superseded requests are discarded.
Cancellation tokens stop obsolete work.

The reducer is the only writer of application state. Render functions are pure
with respect to that state.

### Persistence

SQLite stores:

- schema version;
- queue contents and active ordering;
- last session and player settings;
- listening history;
- podcast progress and played state;
- non-secret account metadata;
- cached normalized provider entities; and
- cached regional content with timestamps.

Configuration uses the conventional per-platform config directory. Credentials
remain in the system vault rather than SQLite or the configuration file.

Writes that protect user progress are transactional. Queue/session state is
debounced and checkpointed, while podcast progress is saved periodically and on
important transitions.

## Failure Handling

### Dependency health

Startup checks compatible `mpv`, `yt-dlp`, and `ffmpeg` executables. Missing or
incompatible tools produce platform-specific installation guidance. Browsing
remains available when possible even if playback dependencies are unavailable.

### Remote failures

- All requests have timeouts and cancellation.
- Retries are bounded and limited to transient failures.
- Rate limiting honors server hints and applies backoff.
- Cached results can be displayed with a visible stale marker.
- Provider decode failures preserve a safe response fingerprint for diagnosis,
  never the response body when it may contain credentials.

### Playback failures

- Stream URLs are resolved just in time.
- An expired or rejected URL triggers one clean re-resolution.
- Unavailable items remain visible with a reason and may be skipped
  automatically according to user settings.
- `mpv` termination is detected and restarted once when safe.
- Child processes and IPC endpoints are cleaned up on normal exit, signals, and
  panic.
- Terminal raw mode and the alternate screen are always restored.

## Testing Strategy

### Unit and property tests

- Reducer transitions and stale-result rejection.
- Sequential, shuffle, repeat, radio, and queue mutation behavior.
- Property tests proving queue permutations do not lose or duplicate items.
- Fade envelopes with a controlled clock, including cancellation.
- Configuration validation and migration.
- Provider-to-domain mapping from recorded fixtures.
- Secret redaction.

### Component tests

- UI rendering through Ratatui's test backend at wide, compact, and tiny sizes.
- Storage repositories against temporary SQLite databases.
- Resolver argument construction and output parsing through a fake process
  runner.
- Player commands and observations through fake IPC peers.
- Unix socket and Windows named-pipe transports on their native CI platforms.

### Integration and live tests

Default tests never require YouTube or a personal account. Recorded, scrubbed
fixtures make parser tests deterministic.

Opt-in live smoke tests cover:

- anonymous song and podcast search;
- country chart retrieval;
- authenticated library access when CI secrets are deliberately provided;
- stream resolution; and
- a short muted player lifecycle.

### Continuous integration

The macOS, Linux, and Windows matrix runs:

- formatting checks;
- Clippy with warnings denied;
- unit, property, component, and integration tests;
- documentation tests;
- dependency/security policy checks; and
- release-mode builds.

Release artifacts include checksums and installation guidance for Cargo,
Homebrew, Scoop, and winget.

## Success Criteria

The first release is successful when a new user on each supported operating
system can:

1. launch anonymously;
2. find and play a song;
3. browse country-specific charts;
4. find and resume a podcast episode;
5. use sequential, shuffle, repeat, and endless-radio modes;
6. hear correct fade transitions;
7. restart and recover the queue and podcast progress; and
8. optionally connect a browser session and browse personal library content.

