# Player Controls, Notifications, Mouse, and Visual Effects Design

## Scope

Ytermusic already supports keyboard play/pause and previous/next actions, wide-layout static and animated artwork, synchronized lyrics, an audio-reactive spectrum, and persistent list viewports. This change extends those foundations with visible keyboard-operated player controls, media keys, bounded seeking, native song-change notifications, compact artwork, mouse interaction, theme-derived visual effects, and a fix for the remaining Charts viewport jump.

The implementation must preserve keyboard-first operation, bounded resource use, secret-safe diagnostics, and graceful fallback on unsupported terminals or desktop environments.

## Player controls and keyboard input

The player presents button-style labels without turning the TUI into a mouse-dependent interface. The full form is conceptually:

```text
[p Previous] [⇧← −10s] [Space Play/Pause] [⇧→ +10s] [n Next]
```

Wide and compact layouts show the fullest labels that fit. Tiny mode uses abbreviated symbols while retaining its existing playback telemetry.

Bindings are:

- `Space`: play or pause;
- `p` / `n`: previous or next track;
- `Shift+Left` / `Shift+Right`: rewind or fast-forward;
- `F7` / `F8` / `F9`: previous, play/pause, or next;
- dedicated previous, play/pause, and next media-key events map to the same actions.

Music seeks by ten seconds. Podcasts use the existing configured backward and forward intervals. Seeking is clamped to valid media bounds and sent through the existing player-supervisor boundary.

## Mouse interaction

Each rendered frame produces a bounded hit-region map for visible interactive elements. The renderer retains only the latest map so mouse coordinates always correspond to the current layout and terminal size. Mouse events resolve to the same semantic actions as keyboard input.

Supported interactions are:

- click a navigation destination to focus and open it;
- click a list row to focus and select it;
- click an already-selected row, or double-click a row, to activate it;
- use the mouse wheel to move selection through the focused list;
- click player-control labels to invoke their action;
- click the progress bar to seek to the bounded proportional position;
- click popup choices and overlay controls to select or activate them.

Modal overlays suppress hit regions behind them just as they suppress keyboard actions. Zero-sized, clipped, stale, and out-of-bounds regions never dispatch actions.

## Native notifications

A small cross-platform notification service sends one native desktop notification only after a new active media item genuinely begins playback. It contains the title, primary artist/channel/podcast creator, album or show when available, and artwork when a safe decoded image is available. Missing artwork uses a generic application/music icon.

Notifications are deduplicated by playback generation so buffering and repeated status updates cannot produce duplicates. The service runs outside playback and rendering on a bounded worker path. Replacement supersedes stale work. Backend absence, denied permissions, missing artwork, and delivery failures never interrupt audio or input.

Configuration adds:

```toml
[notifications]
enabled = true
```

Notification metadata is normalized and bounded. When a platform API requires an image path, the implementation may use a private short-lived temporary image file and must clean it up. Dynamic artwork URLs, raw provider payloads, and image bytes must not appear in diagnostics.

## Artwork presentation

Wide layout keeps the existing static or genuine video-backed animated artwork. Compact layout gains a bounded static thumbnail while animation remains wide-only. Tiny mode omits artwork to protect controls and telemetry. Missing, malformed, or late artwork falls back to the existing safe presentation without moving unrelated content.

## Spectrum color effect

The spectrum uses a theme-derived gradient:

- low-frequency bands begin at the accent color;
- middle bands blend toward the foreground color;
- high bands use a brighter foreground variant;
- intensity adjusts brightness so stronger levels appear more energetic;
- paused and failed states remain muted.

True-color terminals use deterministic RGB interpolation. Limited-color terminals use a small readable theme-safe palette. The effect is calculated during rendering from the latest bounded spectrum frame and does not add another animation or decode task.

## Timestamp-aware lyric fading

Synchronized lyric styles derive from playback time and adjacent lyric timestamps. Near a boundary, the outgoing line gradually dims while the incoming line brightens and gains emphasis. The transition window is capped by the actual line duration so short lines cannot overlap incorrectly.

Wide mode crossfades the previous, current, and next presentation. Compact and tiny modes fade the active line without changing layout height. Pausing freezes the current appearance. Seeking recomputes it immediately from the new playback position. Plain lyrics remain unaffected in the full overlay.

Terminal color capability controls the fidelity of the fade. True-color terminals interpolate colors; limited-color terminals use deterministic discrete emphasis steps.

## Stable Charts viewport

Charts currently changes the effective list height when its sticky section header appears or disappears. This causes the viewport to move before selection crosses a visible boundary.

Chart content will reserve the section-header row consistently. The logical item viewport therefore has a stable height, while the pinned header updates to describe the selected or visible section. Selection movement changes only the highlight until it crosses the top or bottom of the logical window, then scrolls by the minimum required amount.

The same invariants apply to keyboard and mouse-wheel movement. Resizing clamps the viewport without losing a valid selection. A chart refresh retains the selected media when it still exists and otherwise resets predictably. Other list surfaces retain their existing independent viewport memories.

## Data flow

Key and mouse events enter the runtime as typed events. The input/controller layer maps them to semantic actions, including relative seek and activation. The reducer owns application transitions and emits typed effects. Player operations continue through the supervisor; notifications use their own bounded service; rendering consumes normalized state plus the latest artwork, animation, spectrum, and interaction presentations.

The renderer owns ephemeral frame geometry only. Application state does not persist screen coordinates. Visual interpolation is pure and deterministic from current state, time, theme, and terminal capability.

## Failure handling and privacy

All new integrations degrade independently:

- unavailable native notifications are silent and nonfatal;
- unsupported media keys retain ordinary keyboard shortcuts;
- mouse handling never disables keyboard control;
- absent artwork retains text metadata and fallback art;
- limited terminal color reduces gradient and fade fidelity;
- seek rejection preserves the current playback session;
- visual or notification failures cannot stop audio.

Errors remain static and payload-free. Artwork URLs, provider identifiers, notification bodies, decoded frames, spectrum samples, and lyric text must not be logged through debug or error output.

## Verification

Automated coverage includes:

- F7/F8/F9 and dedicated media-key mappings;
- music and podcast seek intervals, clamping, and supervisor effects;
- player-control labels at wide, compact, and tiny sizes;
- mouse hit testing, wheel movement, modal suppression, stale-map rejection, resizing, list activation, and progress seeking;
- notification deduplication, bounded metadata, artwork fallback, cancellation, disabled configuration, and backend failure;
- compact static artwork and wide animation preservation;
- true-color and limited-color spectrum gradients;
- lyric transitions at boundaries, short lines, pause, and seek;
- stable Chart movement in both directions across sections and terminal resizes;
- regression coverage for every shared list viewport.

Completion requires formatting, strict linting, and all-target/all-feature tests. A manual macOS smoke check verifies F7/F8/F9 behavior and Notification Center delivery; it is reported separately from deterministic automated tests.
