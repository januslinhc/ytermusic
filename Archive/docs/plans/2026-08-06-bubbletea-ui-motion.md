# Bubble Tea–Style UI Motion Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add a shared, bounded motion system that renders a smooth theme-gradient player progress bar, gliding selection cursors on every selectable list, and Braille spinners on visible loading states.

**Architecture:** A pure `ui::motion` module owns deterministic progress, spinner, and selection interpolation primitives. The runtime drives one coalesced motion clock only while the current frame reports motion demand, injects an immutable `MotionFrame` into transient `RenderModel`, and preserves render-before-effect ordering. Rendering and viewport memory consume that frame without mutating playback or logical selection state.

**Tech Stack:** Rust, Tokio, Ratatui, Crossterm, existing `AppState`/`UiController`/`RenderModel` architecture, insta snapshots, Cargo integration tests.

---

## Invariants for every task

- Follow `@test-driven-development`: add a focused failing test, observe the expected failure, implement the minimum behavior, rerun focused and affected suites, then commit.
- Playback position, duration, status, queue, and logical list selections remain authoritative existing state. Motion is transient presentation only.
- Motion ticks never dispatch provider, player, storage, notification, or other external effects.
- Tick delivery is bounded/coalesced and never builds a backlog.
- Pause freezes progress fill and shimmer; it does not freeze a visible loading spinner or an unfinished list transition.
- Existing viewport scrolling and exact mouse hit geometry remain authoritative.
- All time-dependent tests use explicit timestamps/phases or a controlled timer; do not use sleeps.
- Preserve tiny/compact/wide layout bounds, Unicode safety, render-before-effect ordering, terminal cleanup, and current snapshot intent.

### Task 1: Build deterministic UI motion primitives

**Files:**
- Create: `src/ui/motion.rs`
- Modify: `src/ui/mod.rs`
- Test: unit tests in `src/ui/motion.rs`

**Step 1: Write failing pure unit tests**

Define wished-for APIs and tests for:

```rust
pub const MAX_UI_MOTION_FPS: u8 = 30;
pub const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MotionFrame {
    pub elapsed_ms: u64,
    pub spinner_index: usize,
    pub progress: ProgressPresentation,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ProgressPresentation {
    pub fraction: f64,
    pub shimmer_phase: f64,
}

pub struct ProgressMotion { /* private bounded state */ }
pub struct SelectionMotion { /* private target/current state */ }
```

Cover spinner sequence/wraparound, 0–1 clamping, normal progress easing, faster seek discontinuity convergence, media/generation reset, pause freeze, resume continuation, end-of-track exactness, one-row selection glide, rapid retargeting from the current visual position, capped large moves, and snap/reset.

**Step 2: Run RED**

Run: `cargo test --lib ui::motion -- --nocapture`

Expected: compile failure because `ui::motion` and its types do not exist.

**Step 3: Implement the minimum pure state machines**

- Use integer milliseconds/fixed bounded arithmetic at state boundaries; floating point is allowed only for normalized transient interpolation.
- Use a monotonic easing function such as cubic ease-out.
- Distinguish ordinary progress retargeting from a seek/media discontinuity explicitly; do not infer only from an arbitrary position delta.
- `SelectionMotion::retarget` must discard queued destinations and start from its current visual value.
- Every public presentation clamps finite values and handles zero elapsed time.

**Step 4: Run GREEN and quality checks**

Run:

```bash
cargo test --lib ui::motion -- --nocapture
cargo clippy --lib --all-features -- -D warnings
```

Expected: all new motion tests pass and Clippy is clean.

**Step 5: Commit**

```bash
git add src/ui/motion.rs src/ui/mod.rs
git commit -m "feat: add deterministic UI motion primitives"
```

### Task 2: Add one coalesced runtime motion clock

**Files:**
- Modify: `src/runtime.rs`
- Modify: `src/ui/controller.rs`
- Modify: `src/ui/render.rs`
- Test: `tests/runtime.rs`
- Test: `tests/ui_controller.rs`

**Step 1: Add failing runtime tests**

Cover:

- a playing known-duration state activates ticks at no more than 30 FPS;
- paused/idle state has no progress tick demand;
- visible loading or unfinished selection motion keeps ticks active while progress is paused;
- multiple missed ticks coalesce to one redraw;
- one delivered tick produces one render and no external effect;
- render failure remains safe;
- shutdown retires the ticker;
- `RenderModel` can receive an exact `MotionFrame` for deterministic renderer tests.

Use a controlled timer/watch source following existing animation/spectrum test patterns. Do not wait on wall-clock sleeps.

**Step 2: Run RED**

Run: `cargo test --test runtime ui_motion -- --nocapture`

Expected: failure because no UI motion ticker or frame integration exists.

**Step 3: Implement the clock and transient frame path**

- Add a single runtime-owned ticker driven by a latest-value/watch-style signal.
- Add `MotionDemand { progress, spinner, selection }` or equivalent bounded flags.
- Extend `RenderModel` with a private/default `MotionFrame` and test builder such as `with_motion_frame`.
- Let `TuiRenderer` report whether the just-rendered frame still needs selection motion; combine it with playback/loading demand before scheduling the next tick.
- Reconcile/tick `ProgressMotion` using authoritative state before a redraw.
- Maintain the established synchronous key/mouse ordering: reduce actions, collect effects FIFO, reconcile motion, render one final frame, then dispatch effects.

**Step 4: Run GREEN and affected runtime tests**

Run:

```bash
cargo test --test runtime ui_motion -- --nocapture
cargo test --test runtime --quiet
cargo test --test ui_controller --quiet
```

Expected: all pass with no extra renders in existing keyboard/mouse trace tests.

**Step 5: Commit**

```bash
git add src/runtime.rs src/ui/controller.rs src/ui/render.rs tests/runtime.rs tests/ui_controller.rs
git commit -m "feat: drive UI motion with one coalesced clock"
```

### Task 3: Replace the bracketed progress bar with a theme gradient

**Files:**
- Modify: `src/ui/render.rs`
- Modify: `tests/ui.rs`
- Modify intentional UI snapshots under `tests/snapshots/`

**Step 1: Add failing progress presentation tests**

Test exact cell spans/styles for:

- empty, half, and full progress;
- fractional leading/trailing cells without brackets;
- true-color interpolation between theme accents;
- named/indexed-color fallback ramp;
- shimmer phase moving only inside the filled portion;
- paused exact frame freeze;
- unknown/zero duration static muted track;
- narrow bar clipping;
- start/middle/end mouse targets covering the complete borderless bar.

Update the layout expectation: progress geometry no longer subtracts two bracket cells.

**Step 2: Run RED**

Run: `cargo test --test ui progress_bar -- --nocapture`

Expected: current bracketed text/style assertions fail.

**Step 3: Implement styled progress spans**

- Replace `progress_bar_text` with a styled `Line`/span builder that consumes `ProgressPresentation` and `Theme`.
- Keep layout calculation separate from coloring; return exact offset/width for hit regions.
- Use the whole rendered width as seekable geometry and a denominator of `width - 1` when width permits.
- Render no animated shimmer for unknown duration or paused presentation.
- Preserve control/telemetry priority and the existing minimum progress width.

**Step 4: Review snapshots and run GREEN**

Run:

```bash
INSTA_UPDATE=always cargo test --test ui snapshots -- --nocapture
git diff -- tests/snapshots
cargo test --test ui progress_bar -- --nocapture
cargo test --test ui --quiet
```

Inspect every changed snapshot; accept only the intentional borderless gradient geometry changes.

**Step 5: Commit**

```bash
git add src/ui/render.rs tests/ui.rs tests/snapshots
git commit -m "feat: animate theme-gradient playback progress"
```

### Task 4: Render the shared Braille loading spinner

**Files:**
- Modify: `src/ui/render.rs`
- Modify: `src/ui/views/search.rs`
- Modify: `src/ui/views/charts.rs`
- Modify: `src/ui/views/podcasts.rs`
- Modify: `src/ui/views/library.rs`
- Modify: `src/ui/views/favorites.rs`
- Modify: `src/ui/views/history.rs`
- Modify: relevant view tests in those modules and `tests/ui.rs`
- Modify intentional snapshots under `tests/snapshots/`

**Step 1: Add failing table-driven tests**

For each approved loading surface, pass spinner indices 0, 1, and wraparound and assert the exact Braille prefix. Cover initial loading, retained rows plus loading-more, retained error rows, hidden surfaces, lyrics loading, and artwork loading where the current UI exposes status. Assert Player resolving/buffering does not gain a spinner.

**Step 2: Run RED**

Run: `cargo test --test ui spinner -- --nocapture`

Expected: static `… Loading`/`… Searching` text does not match spinner frames.

**Step 3: Thread one presentation into loading line builders**

- Add a small helper such as `loading_label(frame, text)` returning a bounded `Line`.
- Pass the immutable spinner frame through render/view functions; do not let views compute wall-clock phase.
- Retain row budgets, sticky headers/footers, errors, and hit-target indices.
- Hidden loading surfaces must not contribute motion demand.

**Step 4: Run GREEN and inspect snapshots**

Run:

```bash
cargo test --test ui spinner -- --nocapture
cargo test --test ui --quiet
git diff -- tests/snapshots
```

Expected: all view and geometry tests pass; snapshot changes are limited to visible loading labels.

**Step 5: Commit**

```bash
git add src/ui tests/ui.rs tests/snapshots
git commit -m "feat: animate visible loading indicators"
```

### Task 5: Add bounded selection-motion memory

**Files:**
- Modify: `src/ui/render.rs`
- Modify: `src/ui/interaction.rs` only if a stable surface helper is needed
- Test: unit tests in `src/ui/render.rs`

**Step 1: Add failing viewport/motion tests**

Define a shared API on bounded render memory, for example:

```rust
SelectionPresentation {
    logical_index: Option<usize>,
    cursor_index: Option<usize>,
    transitioning: bool,
}
```

Test one-row glide, rapid retarget, large in-view move, off-screen snap, animation never changing `SelectionViewport::start`, dataset replace/filter reset, identity mismatch, hidden surface reset, prepend/removal/reorder reconciliation, zero rows, and terminal resize.

**Step 2: Run RED**

Run: `cargo test --lib selection_motion -- --nocapture`

Expected: missing selection-motion memory/API.

**Step 3: Implement bounded per-surface motion**

- Store at most one `SelectionMotion` per fixed `ListSurface` in `ViewportMemory` or a sibling bounded structure.
- Key motion by surface plus current dataset key/visible range.
- Retarget only after normal viewport reconciliation.
- If target is outside the visible range, dataset identity changes incompatibly, the surface hides, or dimensions change incompatibly, snap to the logical selected row.
- Report `transitioning` back to `TuiRenderer` so the runtime clock can idle when complete.
- Do not change row hit targets or application/controller selection.

**Step 4: Run GREEN**

Run:

```bash
cargo test --lib selection_motion -- --nocapture
cargo test --lib selection_viewport -- --nocapture
```

Expected: motion and all established viewport tests pass.

**Step 5: Commit**

```bash
git add src/ui/render.rs src/ui/interaction.rs
git commit -m "feat: add bounded list selection motion"
```

### Task 6: Apply the gliding cursor to every selectable surface

**Files:**
- Modify: `src/ui/render.rs`
- Modify: `src/ui/views/search.rs`
- Modify: `src/ui/views/charts.rs`
- Modify: `src/ui/views/podcasts.rs`
- Modify: `src/ui/views/library.rs`
- Modify: `src/ui/views/favorites.rs`
- Modify: `src/ui/views/history.rs`
- Modify: `src/ui/views/queue.rs`
- Modify: `tests/ui.rs`
- Modify: `tests/ui_controller.rs` only for integration behavior
- Modify intentional snapshots under `tests/snapshots/`

**Step 1: Add failing surface matrix tests**

Cover Search, Charts, podcast recommendations, podcast episodes, Library, Favorites, History, Queue, command palette, country picker, browser picker, and selectable lyrics rows/overlay. For each surface assert:

- logical selection changes immediately;
- the cursor begins at the previous visual row and moves toward the new row for intermediate phases;
- the true selected row retains unambiguous selected styling;
- Enter/mouse action targets the logical row, not the animated cursor row;
- rapid moves retarget;
- hidden/off-screen/dataset-change cases snap;
- interaction maps remain byte-for-byte equivalent across motion phases.

**Step 2: Run RED**

Run: `cargo test --test ui selection_motion -- --nocapture`

Expected: current marker jumps immediately and has no transient presentation.

**Step 3: Centralize cursor and selected-row styling**

- Add a shared row-presentation helper that renders the moving accent `▶`/highlight and separately marks the logical selected row.
- Thread `SelectionPresentation` through every surface instead of duplicating interpolation.
- Preserve metadata/playable formatting, pinned chart headers, sticky footers, row clipping, and stable hit targets.
- Ensure overlays use the same motion frame and bounded memory.

**Step 4: Run GREEN and inspect snapshots**

Run:

```bash
cargo test --test ui selection_motion -- --nocapture
cargo test --test ui_controller --quiet
cargo test --test ui --quiet
git diff -- tests/snapshots
```

Expected: all surface matrix, existing viewport, mouse geometry, and controller tests pass.

**Step 5: Commit**

```bash
git add src/ui tests/ui.rs tests/ui_controller.rs tests/snapshots
git commit -m "feat: glide selection across TUI lists"
```

### Task 7: Document motion behavior and manual verification

**Files:**
- Modify: `README.md`
- Modify: `docs/release-checklist.md`
- Modify: `tests/docs.rs`

**Step 1: Add failing scoped documentation tests**

Require the README UI section to state:

- theme-aware borderless animated progress;
- pause freezes progress animation;
- list animation is presentation-only and logical selection is immediate;
- visible loading states use a Braille spinner;
- motion idles when nothing visible needs animation;
- mouse seeking remains supported.

Add a new dated manual audit section without rewriting historical evidence. Mark interactive motion scenarios `NOT RUN` until executed.

**Step 2: Run RED**

Run: `cargo test --test docs -- --nocapture`

Expected: missing motion documentation assertions.

**Step 3: Update concise user documentation**

Document visible behavior, not internal state-machine details. Add manual cases for playback/pause/seek, rapid keyboard and mouse selection, loading completion, terminal resize, tiny layout fallback, and idle redraw behavior.

**Step 4: Run GREEN**

Run: `cargo test --test docs --quiet`

Expected: scoped documentation tests pass.

**Step 5: Commit**

```bash
git add README.md docs/release-checklist.md tests/docs.rs
git commit -m "docs: describe Bubble Tea UI motion"
```

### Task 8: Cross-cutting verification and final review

**Files:**
- Review every file changed since `26f8ee6`.
- Modify only files needed for verified findings.

**Step 1: Run focused invariant suites**

```bash
cargo test --lib ui::motion -- --nocapture
cargo test --test runtime ui_motion -- --nocapture
cargo test --test ui progress_bar -- --nocapture
cargo test --test ui spinner -- --nocapture
cargo test --test ui selection_motion -- --nocapture
cargo test --test ui mouse -- --nocapture
```

**Step 2: Run the complete gate**

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features --quiet
git diff --check 26f8ee6..HEAD
git status --short
```

Record the actual exit code and full test totals; do not treat partial yielded output as completion.

**Step 3: Inspect snapshots and performance bounds**

- Review every changed snapshot manually.
- Verify the runtime cannot exceed 30 FPS or queue missed ticks.
- Verify motion memory is bounded by fixed surfaces.
- Verify paused/idle state produces no motion redraw demand.
- Verify interaction geometry is unchanged except the intentional borderless progress width.

**Step 4: Perform interactive smoke checks when a terminal is available**

- play, pause, resume, seek forward/backward, and change tracks;
- rapidly move selection across every list type;
- click rows and progress cells;
- start/finish visible loading states;
- resize wide/compact/tiny;
- observe idle CPU/redraw behavior.

If not run, leave these accurately marked `NOT RUN`.

**Step 5: Request final whole-feature review**

Review base `26f8ee6` to HEAD for timing nondeterminism, redraw backlog, render/effect ordering, stale list identities, viewport regressions, mouse geometry, Unicode width, theme fallback, shutdown, and documentation accuracy. Fix every Critical/Important finding with a RED/GREEN regression and re-review.
