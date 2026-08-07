# Tab Navigation and Audio-Reactive Spectrum Design

**Date:** 2026-08-04

## Goal

Make focus movement predictable with a three-region Tab cycle, make the navigation rail directly browsable with horizontal keys, and add a genuine sound-reactive spectrum strip to the player without coupling playback correctness to visualization work.

## Approved Interaction

### Focus cycle

`Tab` cycles forward through:

1. Navigation
2. Content (the Home or current view's main panel)
3. Player
4. Navigation

`Shift-Tab` cycles backward through the same regions. Queue is intentionally excluded from this cycle and remains accessible through the existing `Q` compact-queue behavior. Existing directional focus behavior remains available outside the navigation rail.

### Navigation rail

While Navigation has focus, `Left`/`Right` and `h`/`l` immediately switch among Home, Search, Charts, Podcasts, Library, History, and Settings. Navigation wraps in both directions: moving right from Settings opens Home, and moving left from Home opens Settings.

`Tab` from Navigation enters Content for the currently selected view. A second `Tab` focuses Player.

### Spectrum presentation

The audio visualizer is enabled by default and uses a dedicated player strip:

- Wide layout: a three-row frequency spectrum.
- Compact layout: a one-row mini spectrum.
- Tiny layout: no spectrum.

The spectrum coexists with artwork, synchronized lyrics, progress, modes, volume, and player controls. It owns bounded layout space and must never overlap those elements. Bass-weighted bars use the theme accent, with the remaining bands using compatible theme colors. Fast attack and slower decay smooth the display and limit terminal flicker.

While playing, visualization updates at a bounded default of 15 frames per second. Pausing freezes and dims the latest frame. Missing dependencies, unsupported media, malformed analyzer output, or analyzer failure show a quiet baseline and never alter audio playback.

## Configuration

Add an enabled-by-default configuration section:

```toml
[visualizer]
enabled = true
max_fps = 15
```

`max_fps` is validated within a conservative range. Old configuration files continue to deserialize through defaults. The visualizer is independently configurable from animated artwork.

## Architecture

### Focus navigation

Add semantic actions for cycling focus forward and backward. Map `Tab` and `BackTab`/`Shift-Tab` before global directional dispatch. The controller computes the three-region cycle explicitly rather than relying on enum ordering, so Queue cannot enter it accidentally.

When focus is Navigation, horizontal movement changes the selected `NavigationItem` and activates it immediately. The navigation-item order is defined once and used for forward/backward wrapping. Vertical list selection remains unchanged.

Text-entry modes and modal overlays retain priority. Tab focus movement must not leak through a modal interaction that owns the key.

### Audio analysis

The current player exposes status, position, and effective volume but no amplitude or frequency telemetry. Genuine sound reaction therefore uses a separate, bounded FFmpeg analyzer rather than a synthetic progress animation or fragile mpv log parsing.

The resolved audio URL is passed transiently and opaquely to the analyzer boundary. FFmpeg reads it in real time, seeks to the authoritative playback position on start or resume, downsamples to low-rate mono PCM, and emits raw samples over stdout. It runs through direct arguments with no shell and no frame or sample files.

Rust applies a fixed-size window and FFT, groups bins into a bounded number of perceptual frequency bands, normalizes levels, and applies deterministic attack/decay smoothing. A capacity-one latest-frame store publishes normalized bar levels keyed by playback generation, media identity, target layout size, and a monotonic run lease.

The TUI runtime listens to a coalesced spectrum revision signal. New frames request redraw without blocking the analyzer or creating a busy loop. The renderer reads only the latest valid spectrum frame.

## Lifecycle

The analyzer starts only when all of these are true:

- `visualizer.enabled` is true.
- Playback is actively Playing.
- A playable audio stream URL is available.
- The current layout supports a spectrum.
- FFmpeg is available and meets dependency checks.

Replacement, seek, pause, stop, playback failure, disabling visualization, layout downgrade, or shutdown cancels and reaps current work. Pause retains the last safe frame but stops decode/network work. Resume starts a new run from the current playback position. Same-track restarts receive a new lease so retired workers cannot publish or clear current presentation.

Errors clear or freeze only visualization presentation as appropriate. They never modify playback, fades, lyrics, artwork, queue, history, or persistence.

## Bounds and Privacy

- Direct FFmpeg argv; no shell.
- No temporary files or persistent spectrum cache.
- Audio URLs, provider identifiers, media titles, samples, FFT bins, and spectrum frames are redacted from Debug, Display, errors, and logs.
- Low sample rate and mono input.
- Fixed upper bounds for sample chunks, FFT size, band count, player dimensions, stdout reads, process lifetime, network I/O, update rate, and pending redraws.
- Capacity-one newest-frame presentation drops obsolete frames instead of applying backpressure.
- Cancellation and timeout paths kill and explicitly reap the child within bounded runtime ownership, with safe detached ownership if the operating-system wait outlives the synchronous grace period.
- Analyzer output is memory-only and discarded on replacement or process exit.

The separate low-bandwidth decode is an accepted tradeoff for genuine frequency response without unstable mpv-specific metadata parsing.

## Rendering and Fallback

Wide rendering reserves three rows in the player only when the visualizer is enabled and presentation is eligible. Compact rendering reserves one row. Tiny rendering reserves none. Layout calculations preserve the existing bounded list viewports and player-control visibility.

If no fresh frame exists, the renderer shows a stable quiet baseline in the reserved strip. During pause, it shows a dimmed last frame. Animated artwork can continue in Wide while the spectrum renders in its separate strip. Compact/tiny static-artwork rules remain unchanged.

## Testing Strategy

### Input and controller

- Tab and Shift-Tab forward/reverse cycles.
- Queue exclusion from the cycle.
- Navigation Left/Right and h/l immediate switching.
- Both-direction navigation wraparound.
- Text-entry and overlay suppression.
- Existing Q queue behavior and directional selection regressions.

### Spectrum math and storage

- Silence and deterministic bass, mid, and treble fixtures.
- FFT and band normalization bounds.
- Attack/decay smoothing.
- Capacity-one newest-frame behavior.
- Generation, media, size, and lease matching.
- Empty, oversized, malformed, and non-finite input rejection.
- Summary-only Debug output.

### Worker and runtime

- Real-time FFmpeg arguments and playback-position seek.
- FPS pacing with fake clocks and no real sleeps.
- Pause stops decode work and retains the last frame.
- Resume starts a new lease at current position.
- Seek, replacement, layout downgrade, failure, and shutdown cancellation/reaping.
- Non-cooperative process timeout behavior.
- Coalesced redraw notification and closed-channel handling without busy loops.
- Stale same-key workers cannot publish or clear current state.

### Rendering and integration

- Wide three-row and compact one-row spectrum.
- Tiny omission.
- Theme colors, bass accent, dimmed pause, and quiet fallback.
- Spectrum never covers lyrics, artwork, progress, modes, or controls.
- Resize safety and existing list viewport tests.
- Missing FFmpeg and analyzer failures leave playback untouched.
- Privacy sentinels for URL, IDs, titles, samples, bins, and frames.

### Verification

- Focused input/controller, analyzer, runtime, and renderer tests.
- Full all-target/all-feature suite.
- Formatting, strict Clippy, and diff checks.
- One explicitly ignored, environment-gated live smoke that produces a single bounded spectrum frame without printing media data.

