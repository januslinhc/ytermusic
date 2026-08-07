# Task 15 Spec Review Fixes Design

## Scope

This follow-up closes five Task 15 integration gaps without composing the
Task 16 runtime. The application reducer remains the sole writer of
`AppState`, rendering remains pure, and all I/O remains represented as typed
effects.

## Executable UI controller

Add a pure `ui::controller` module. `UiController` owns the render model,
bounded search draft, input mode, palette state, country picker, and stable
queue selection. A key event first passes through the existing physical-key
mapper and then through one contextual semantic dispatcher. Command-palette
submission invokes that same dispatcher, so keyboard and palette behavior
cannot diverge.

Help and country-picker overlays are modal: they accept only their own
navigation, submission, cancellation, and global quit controls, preventing
playback or queue mutations behind an overlay.

The dispatcher returns typed application actions but never mutates
`AppState`. It handles search entry and submission, stable list movement and
activation, queue playback and reordering, playback controls, volume and queue
modes, account connection, pagination, dependency repair, navigation, overlay
cancellation, and compact queue focus.

The country picker is a bounded static list with a stable `RegionCode`
selection. `c` or the palette command opens it, arrows or Vim keys move it,
Enter emits `ChartsRequested`, and Escape closes it without an app action.

## Effect-backed chart cache

`ChartsRequested` allocates one generation and emits both a region-keyed cache
read and live chart load. A bounded typed payload contains the region,
normalized sections, and stored/expiry provenance. Its storage encoding is
size checked before use. Public deserialization routes through a private raw
document and revalidates view bounds, provenance, and encoded size.

Chart state tracks cache and live completion independently. A fresh live result
always wins, closes the generation against late cache results, and emits a
bounded store effect. A live error and cache miss/error resolve deterministically
in either arrival order: stale matching cache is shown when available, otherwise
the live error is shown once both outcomes are known. Generation and region
checks reject late results from an older country.

## Supervisor presentation telemetry

`FadeController` gains a layer-independent `FadeDirection`. The player
supervisor maps it to application `FadeActivity` and emits generation-tagged,
coalescible playback telemetry at fade starts, ticks, completion, cancellation,
zero-duration transitions, and target-volume changes. Equal fade endpoints
report no audible direction.

Successful resolution emits bounded format and codec metadata only. The signed
stream URL never enters an action. Pending action storage gets independent
format and telemetry slots. Resolution, status, and terminal actions retain
priority over telemetry and progress; format remains protected and eventually
delivered.

A matched failed-status transition closes the playback generation but freezes
the last observed presentation for diagnostics. In that state, effective
volume, quality, and the fade glyph describe the final observation; they do
not claim that a fade is still advancing. Late telemetry remains
generation-rejected, and the next resolution clears the frozen snapshot.
Stopped and natural-end paths continue to clear presentation immediately.

## Tiny player

The 40-cell player line uses a telemetry-first fixed encoding for status,
elapsed/duration, target/effective volume and fade, shuffle/repeat/radio,
podcast speed, and known format/codec. Only the remaining display cells are
used for a separately truncated title and creator. Cell-width calculations,
not byte length, reserve at least one visible cell for each when both exist,
including wide graphemes.

## Artwork URL secrecy

Introduce `ArtworkUrl`, a private `Url` newtype with full equality, a deliberate
`as_url()` executor accessor, and constant redacted `Debug` and `Display`.
Actions, effects, and artwork application state use this type so their derived
debug output cannot reveal a signed artwork URL.

## Verification

Each section is implemented with a witnessed RED then focused GREEN. Final
verification covers workflows, UI/controller, reducer, player, fade, storage,
snapshots, formatting, strict Clippy, the full all-feature suite, doctests, and
clean-tree/snapshot hygiene.
