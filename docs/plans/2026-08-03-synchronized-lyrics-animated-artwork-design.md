# Synchronized Lyrics and Animated Artwork Design

## Problem

The player currently shows playback metadata and one static artwork image. It does not fetch or display lyrics, follow the current lyric line, or render genuine motion from playable video media. The desired experience is an automatic, compact synchronized-lyrics presentation plus a full lyrics view, with real provider-backed animation when it is available and safe static fallback everywhere else.

YouTube Music's current provider library exposes plain lyrics but not dependable line timestamps. Synchronized lyrics therefore need an optional external source with conservative matching. YouTube does not expose a reliable animated-cover contract through the current library; real motion will come only from a low-resolution video stream associated with the current playable media, never from synthetic effects.

## Approved behavior

- Songs and music videos request lyrics when they become the active playback item. Podcasts do not.
- YouTube Music supplies plain lyrics. LRCLIB may supply synchronized lyrics when title, artist, and duration form a strong match.
- Synchronized lyrics appear automatically in the normal player presentation. Wide layout shows previous, current, and next lines; compact layout shows the current line; tiny layout keeps controls readable and shows at most one clipped line.
- The active synchronized line uses the theme accent and advances from authoritative player position updates, including after seeks.
- `L` opens or closes a full lyrics overlay. `Esc` also closes it.
- The full overlay centers and follows the active line by default. Arrow keys or `j`/`k` scroll manually while the active highlight continues to update; `Enter` returns to the active line and resumes following.
- Plain lyrics are available in the full overlay but do not automatically occupy compact player space.
- Instrumental, unavailable, loading, and failed lyric states are represented concisely without interrupting playback.
- In wide layout, a usable low-resolution YouTube video stream replaces static artwork with actual decoded frames. Audio-only media, unsupported layouts, missing decoders, slow systems, and all failures retain static artwork.
- Animation is enabled by default, capped near eight frames per second, pauses with playback, resumes without losing audio position, and is cancelled on track replacement.

## Lyrics architecture and data flow

Add an independent lyrics boundary rather than coupling lyrics to search or artwork. A request is keyed by the active media ID plus a bounded metadata fingerprint. The runtime starts YouTube plain-lyrics retrieval and, when enabled, an LRCLIB lookup using bounded title, artist, album, and duration fields. Both operations are generation-scoped so late results cannot replace a newer track.

The normalized result contains a source classification, optional plain text, and optional timed lines. LRCLIB synchronized lyrics take precedence only after exact normalized title and artist agreement and close duration agreement. YouTube plain lyrics are the preferred plain fallback; an LRCLIB plain result may fill the gap when no YouTube text is available. Weak or ambiguous external matches are treated as unavailable.

LRC parsing accepts common minute/second/fraction timestamps, bounds every line and the full document, sorts safely when necessary, coalesces exact duplicate timestamps deterministically, and derives each line's visible range from the next timestamp. Malformed timed lines are skipped. If no usable timed lines remain, bounded plain text remains available when present.

Lyrics are cached only in memory for the process lifetime. The cache is bounded and keyed by media identity and metadata fingerprint so changed metadata cannot reuse a mismatched result. Lyrics payloads are not stored in SQLite, history, diagnostics, or logs.

## Animated artwork architecture and data flow

Extend resolution output with an optional redacted, expiring low-resolution video URL while retaining the existing audio URL as the sole mpv playback input. The resolver selects a conservative video-only format suitable for a small terminal preview; failure to obtain it does not change resolution success.

A cancellable animation worker passes the video URL directly to FFmpeg and requests frames at the current rendered artwork dimensions, bounded by a configured maximum and frame-rate cap. The worker publishes through a capacity-one latest-frame channel. When decoding or terminal drawing falls behind, stale frames are replaced rather than queued. The existing artwork presentation store exposes either the newest animation frame or the existing static/fallback presentation.

Animation follows playback state: it begins only for the active generation, pauses frame publication while audio is paused, and is torn down on stop, replacement, layout loss, decoder failure, or shutdown. It does not control mpv, audio timing, queue state, or fades. No video or frame file is persisted.

## UI behavior

Wide player layout uses the existing artwork area for animation and reserves a small adjacent lyric region for three synchronized lines. Compact mode adds one synchronized line beneath essential track and progress data. Tiny mode may omit the lyric when necessary to preserve controls.

The full lyrics overlay is independently scrollable. Its renderer retains a viewport and follow/manual mode. Progress changes update the active-line identity without forcibly moving a manually scrolled viewport. Enter restores follow mode and reveals the active line. Track changes reset the overlay viewport and follow mode.

Plain lyrics render as bounded wrapped lines without a false active-line marker. Source attribution is concise and shown in the overlay. The normal player never displays a large error; it falls back to its pre-lyrics layout.

## Configuration and privacy

Add settings equivalent to:

```toml
[lyrics]
enabled = true
external_sync = true

[artwork]
animated = true
max_fps = 8
```

Disabling `external_sync` prevents track metadata from being sent to LRCLIB while retaining YouTube Music plain lyrics. LRCLIB requests identify the application with the required user-agent header and use its documented unauthenticated read API only. Dynamic URLs, provider identifiers, response bodies, lyrics text, and decoded frames are never included in errors or debug output.

All HTTP inputs are HTTPS-only with strict timeout, redirect, response-size, item, and text limits. Animation has strict input-byte, output-dimension, frame-rate, process, and queue limits. Any lyric or animation boundary failure is non-fatal and never delays or stops audio.

## Verification

Pure tests will cover metadata normalization and match rejection, LRC fractions and malformed lines, monotonically derived ranges, duplicate timestamps, document limits, plain fallback, and redacted debug output. Reducer/runtime tests will cover initial track activation, podcasts being excluded, stale generations, cache identity, seek-driven line changes, cancellation, and non-fatal failures.

Renderer/controller tests will cover automatic wide/compact/tiny presentations, full-overlay toggling, follow mode, manual scrolling, Enter recentering, track replacement, resize behavior, plain lyrics, and unavailable states. Animation tests will use fake frame streams and clocks to cover pause/resume, capacity-one frame replacement, decoder overload, cancellation, missing FFmpeg, dimension limits, and static fallback without depending on real-time sleeps.

Feature-gated ignored live tests will separately exercise YouTube plain lyrics, one strong LRCLIB synchronized match, and one low-resolution video frame. Offline fixture tests and the full Rust test/lint/format suite remain the completion gate; live tests are reported separately and are never claimed unless actually executed.
