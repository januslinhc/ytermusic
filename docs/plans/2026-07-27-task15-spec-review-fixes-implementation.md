# Task 15 Spec Review Fixes Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Close the five verified Task 15 integration gaps with pure UI control, effect-backed chart caching, real supervisor telemetry, a complete tiny player, and redacted artwork URLs.

**Architecture:** `UiController` translates physical and palette input into typed reducer actions without owning application state. Chart cache and player integration remain typed action/effect protocols, while rendering consumes immutable app and UI state.

**Tech Stack:** Rust, Ratatui, Crossterm, Tokio, Serde, SQLite traits, Insta snapshots.

---

### Task 1: Executable UI controller and country picker

**Files:**
- Create: `src/ui/controller.rs`
- Modify: `src/ui/mod.rs`
- Modify: `src/ui/render.rs`
- Modify: `src/app/action.rs`
- Modify: `src/app/reducer.rs`
- Test: `tests/ui_controller.rs`
- Test: `tests/workflows.rs`

**Step 1:** Write integration tests that drive real `KeyEvent` values through
the controller, app reducer, fake completions, and `TestBackend`.

**Step 2:** Run the focused tests and confirm RED because no contextual
controller or country-picker overlay exists.

**Step 3:** Implement bounded UI state and a shared semantic dispatcher,
including typed target-volume and generic media enqueue actions.

**Step 4:** Run the focused controller/workflow/UI tests and confirm GREEN.

### Task 2: Effect-backed chart cache

**Files:**
- Modify: `src/app/action.rs`
- Modify: `src/app/effect.rs`
- Modify: `src/app/state.rs`
- Modify: `src/app/reducer.rs`
- Test: `tests/workflows.rs`
- Test: `tests/reducer.rs`
- Test: `tests/storage.rs`

**Step 1:** Write fake-effect tests for cache-first/live-first success, both
failure orders, stale fallback, cache miss, and HK-to-US late-result rejection.

**Step 2:** Run them and confirm RED because chart requests emit no cache read
and live success emits no store.

**Step 3:** Implement a bounded serializable region payload, typed read/store
effects, and deterministic two-source state resolution.

**Step 4:** Run focused workflow/reducer/storage tests and confirm GREEN.

### Task 3: Supervisor presentation telemetry

**Files:**
- Modify: `src/fade.rs`
- Modify: `src/player/supervisor.rs`
- Modify: `tests/fade.rs`
- Modify: `tests/player.rs`
- Modify: `tests/workflows.rs`

**Step 1:** Write supervisor tests for safe format actions, in/out/idle
telemetry, zero-duration clearing, saturation coalescing, and
supervisor-to-reducer rendering.

**Step 2:** Run them and confirm RED because the supervisor does not emit the
new presentation actions and pending storage rejects them.

**Step 3:** Add `FadeDirection`, protected format/telemetry pending slots, and
generation-tagged emissions without stream URLs.

**Step 4:** Run focused fade/player/workflow tests and confirm GREEN.

### Task 4: Complete 40-cell persistent player

**Files:**
- Modify: `src/ui/render.rs`
- Modify: `tests/ui.rs`
- Modify: `tests/snapshots/ui__tiny.snap` only if the reviewed fixture changes

**Step 1:** Add a 40×10 failed long-title podcast test asserting every required
concept by terminal cell width.

**Step 2:** Run it and confirm RED because title-first clipping removes
telemetry and speed.

**Step 3:** Implement fixed telemetry-first fields plus CellWidth-safe leftover
title/creator allocation.

**Step 4:** Run UI tests, review any snapshot delta, and confirm GREEN with no
`.snap.new`.

### Task 5: Redacted artwork URL boundary

**Files:**
- Modify: `src/app/state.rs`
- Modify: `src/app/action.rs`
- Modify: `src/app/effect.rs`
- Modify: `src/app/reducer.rs`
- Modify: `tests/reducer.rs`

**Step 1:** Add sentinel tests formatting the URL wrapper, artwork action,
effect, artwork state, and full app state.

**Step 2:** Run them and confirm RED because derived debug output contains the
signed URL.

**Step 3:** Implement `ArtworkUrl` with private storage, constant redacted
formatting, equality, and explicit fetch accessor; migrate app boundaries.

**Step 4:** Run focused reducer/UI tests and confirm GREEN.

### Task 6: Full verification and follow-up commit

**Files:**
- Review all files changed by Tasks 1–5.

**Step 1:** Run focused workflows, UI/controller, reducer, player, fade, and
storage tests.

**Step 2:** Review snapshots and prove no `.snap.new` exists.

**Step 3:** Run `cargo fmt --all -- --check`.

**Step 4:** Run `cargo clippy --all-targets --all-features -- -D warnings`.

**Step 5:** Run `cargo test --all-features` and
`cargo test --doc --all-features`.

**Step 6:** Run `git diff --check`, review status, and commit separately from
`21e2e20`.

### Task 7: Preserve diagnostic presentation after a matched failure

**Files:**
- Modify: `src/app/reducer.rs`
- Modify: `tests/reducer.rs`
- Modify: `tests/ui.rs`
- Modify: `docs/plans/2026-07-27-task15-spec-review-fixes-design.md`

**Step 1:** Add reducer coverage that records progress, quality, and fade
telemetry, applies a matched failed status, then proves the generation closes,
the last presentation is frozen, late telemetry is ignored, podcast progress
is checkpointed, and the next attempt clears the snapshot.

**Step 2:** Make the 40×10 long-title podcast acceptance path apply
`PlayerStatusChanged(Failed)` last and assert the failure icon plus every
identity, progress, volume/fade, mode, speed, and quality token.

**Step 3:** Run the focused tests and confirm RED because the terminal-status
branch currently clears presentation for both stopped and failed playback.

**Step 4:** Clear presentation only for stopped status. Keep failed
presentation as a frozen diagnostic observation while retaining all existing
terminal generation and checkpoint cleanup.

**Step 5:** Run focused reducer, UI, player, and workflow tests, then the full
format, strict Clippy, all-features, doctest, snapshot-hygiene, and diff gates.

**Step 6:** Commit separately from `7b69423`.
