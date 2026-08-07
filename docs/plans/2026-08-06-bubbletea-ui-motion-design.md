# Bubble Tea–Style UI Motion Design

## Goal

Give the TUI a cohesive Bubble Tea–style motion language without weakening playback, selection, viewport, mouse, or runtime guarantees.

The feature has three coordinated parts:

- a borderless, theme-aware gradient playback progress bar with smooth fill and shimmer;
- a gliding cursor/highlight for every selectable list;
- a compact Braille dot-orbit spinner for visible loading states.

All three share one bounded redraw clock. Playback and logical selection remain authoritative application state; animation is presentation only.

## User-visible behavior

### Playback progress

- Replace the current bracketed `[████░░░░]` display with a clean borderless bar in the same player control row.
- Interpolate the displayed fill smoothly toward the authoritative playback position.
- Draw the filled portion with a gradient derived from the configured theme accents.
- Move a subtle highlight through the filled portion while playback is active.
- Freeze fill interpolation and shimmer immediately while paused.
- Resume from the frozen presentation rather than restarting the animation.
- Retarget quickly after seeks. Large forward or backward jumps must converge promptly and must not drift slowly toward the new position.
- Reset safely when the current media or playback generation changes.
- For unknown or zero duration, show a static muted track and disable progress seeking.
- Preserve mouse seeking across the complete visible bar width.

### List selection

Apply the same motion language to all selectable list surfaces:

- Search
- Charts
- podcast recommendations and opened episode lists
- Library
- Favorites
- History
- Queue
- command palette
- country picker
- browser picker
- lyrics list/overlay where selectable

Logical selection changes immediately. Activation, artwork, mouse behavior, queue actions, and application state always use the real selected row.

Only the visual cursor/highlight glides toward the selected row, normally over 80–120 ms. Rapid key presses retarget from the current visual position instead of queueing animations. The real selected row retains unambiguous selected styling while motion is in progress.

Animation must never scroll the viewport. Existing viewport rules remain authoritative. Dataset replacement, filtering, view changes, invalid targets, off-screen targets, and terminal resize snap the presentation to the real selected row. This prevents the cursor from appearing over stale or unrelated data.

### Loading spinner

Use the compact Braille dot-orbit sequence `⠋ ⠙ ⠹ ⠸ ⠼ ⠴ ⠦ ⠧ ⠇ ⠏` for visible loading states in:

- Search
- Charts
- podcast recommendations and opened podcasts
- Library
- Favorites
- History
- artwork and lyrics states where the UI already exposes loading feedback

The spinner replaces static loading punctuation or prefixes; it does not add a Player buffering spinner in this scope. It disappears immediately when loading finishes or the loading surface is hidden.

## Architecture

### Shared motion clock

Add one runtime-owned `UiMotionClock`. Do not create a worker or timer per component.

The clock is active only while at least one visible presentation needs motion:

- playback is playing with a known duration;
- a visible list selection is transitioning;
- a visible loading state needs a spinner.

The maximum rate is 30 FPS. Tick delivery coalesces through a latest-value/watch-style signal so slow terminal rendering cannot accumulate a redraw backlog. When paused and no spinner or selection transition is active, the clock idles completely.

### Deterministic motion presentation

Each redraw receives an immutable UI motion presentation containing the values needed for deterministic rendering:

- displayed progress fraction;
- progress shimmer phase;
- spinner frame index;
- elapsed transition time or normalized selection motion phase.

Playback position and duration remain in `AppState`. The motion layer observes them but never mutates them. The runtime detects media/generation changes, seeks, pause/resume, and playback progress updates and retargets the presentation.

Tests inject exact time/tick values. Rendering must not read wall-clock time directly.

### Render integration

Extend the existing transient rendering/enhancement boundary rather than adding animation state to session checkpoints or persistent app state.

The renderer derives progress gradient cells from the immutable motion presentation and `Theme`. Selection presentation uses bounded per-surface visual state in the existing viewport/render memory. It may retain only stable bounded identities and numeric offsets, never unbounded provider content.

The progress control layout exposes the bar’s actual offset and width. Mouse hit regions cover every seekable cell directly; they do not depend on fill position or shimmer phase.

### Theme behavior

Gradient endpoints come from the configured theme’s accent colors. True-color endpoints use RGB interpolation. Indexed or named colors use a small deterministic fallback ramp based on the existing accent and highlight colors. Empty-track cells use the theme’s muted styling.

No fixed pink/purple palette is introduced, so custom themes remain coherent.

## State transitions and edge cases

### Progress interpolation

- Normal progress updates retarget from the current displayed fraction.
- Seek discontinuities use a faster bounded easing window.
- A new media ID or playback generation snaps/reset-initializes to the authoritative starting position.
- Pausing freezes both fraction and shimmer at their last rendered values.
- Resuming continues from that frozen presentation.
- End-of-track clamps exactly to 100%; invalid/unknown durations never divide by zero.

### Selection motion

- A one-row move glides over the normal 80–120 ms window.
- Larger in-viewport moves use a capped duration; rapid retargeting never queues old destinations.
- Off-screen selection changes first obey viewport reconciliation, then snap to the correct visible row.
- Dataset replacement, identity mismatch, filtering, hidden surfaces, and resize clear stale motion and snap.
- Mouse selection uses the same motion behavior after the logical row changes.

### Spinner lifecycle

- Loading start activates the shared clock only if the loading surface is visible.
- Loading completion or navigation away removes that spinner demand immediately.
- Multiple visible loading indications share the same phase.
- Static/tiny fallbacks render safely without requiring animation.

## Performance and safety

- Hard cap redraw demand at 30 FPS.
- Coalesce ticks; never enqueue one runtime action per missed frame.
- Keep all transient motion memory bounded by the fixed list-surface set.
- Do not alter queue, playback, selection, history, Favorites, session, or storage state.
- Do not dispatch external effects from animation ticks.
- Preserve runtime render-before-effect ordering and terminal cleanup behavior.
- Respect terminal width and clipping limits, including tiny layouts and wide Unicode text.

## Testing strategy

Use TDD with a fake/injected clock and exact phases. No sleep-based animation tests.

### Progress tests

- start, middle, end, unknown duration, and zero duration;
- smooth interpolation between playback observations;
- pause freeze and resume continuation;
- forward/backward seek convergence;
- media/generation reset;
- shimmer start/freeze/resume;
- theme-aware true-color and fallback gradients;
- narrow/tiny layouts;
- exact mouse seek geometry across the borderless bar.

### Selection tests

- one-row glide and rapid retargeting;
- large in-view moves and off-screen snapping;
- viewport never scrolls from animation alone;
- dataset replacement/filtering/identity mismatch reset;
- prepend/removal/reorder/resize reconciliation;
- keyboard and mouse selection;
- every selectable surface listed above;
- selected target remains logically unambiguous during motion.

### Spinner tests

- exact Braille frame sequence and wraparound;
- start/stop for each loading surface;
- hidden loading state does not keep ticking;
- shared phase for simultaneous visible indicators;
- static/tiny fallback.

### Runtime tests

- clock activates and idles on the correct demands;
- 30 FPS cap and coalesced redraws;
- paused progress does not request redraws unless another motion demand exists;
- one render per delivered tick;
- animation ticks emit no external effects;
- render failure and shutdown remain safe.

Finish with formatting, strict all-target/all-feature Clippy, the complete all-target/all-feature test suite, snapshot inspection, and interactive smoke checks for progress, rapid list movement, mouse seeking, loading transitions, pause/resume, and terminal resize.

## Non-goals

- Player buffering/resolving spinner.
- Audio-reactive progress colors.
- Fixed Bubble Tea pink/purple colors.
- Persistent animation settings or session state.
- Changes to queue, playback, Favorites, or storage semantics.
- Continuous animation while paused and otherwise idle.
