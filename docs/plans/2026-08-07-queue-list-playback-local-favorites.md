# Queue-List Playback and Local Favorites Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Make list activation replace and start a bounded queue atomically, and add durable local Favorites with keyboard and mouse-aware TUI integration.

**Architecture:** Controllers translate the currently loaded playable rows into one explicit-list action. The reducer validates and constructs a complete replacement `Queue` before committing it, preserving repeat and shuffle semantics while disabling radio. Favorites use a schema-v3 SQLite table and the existing ordered storage worker; reducer state is updated only by generation-matched completion actions. A dedicated top-level view consumes the shared list viewport and exposes the same playable-list activation behavior.

**Tech Stack:** Rust, Ratatui/Crossterm, Tokio, rusqlite/SQLite, serde/JSON, insta snapshots, Cargo integration tests.

---

## Invariants to preserve throughout

- A rejected explicit list never mutates queue, playback, repeat, shuffle, or radio state.
- Explicit-list normalization retains first occurrence order, removes duplicate full `MediaId`s, rejects an absent selected ID, and caps the final list at 1,024 items.
- Shuffle retains the activated item as current and randomizes only the remainder with the controller-provided seed.
- Favorites are keyed by full provider/media identity, newest first, capped at 1,024 without eviction, and excluded from `SessionCheckpoint`.
- A favorite mutation never adds, removes, stops, or reorders playback by itself.
- Every task starts with a failing focused test, makes the smallest production change, reruns the focused test, then runs the affected suite before commit.

### Task 1: Add an atomic explicit-list queue constructor

**Files:**
- Modify: `src/queue.rs`
- Modify: `tests/queue.rs`

1. Add failing queue tests for source-order de-duplication, selected-item validation, selected-first shuffled order, preserved repeat mode, disabled radio, and a 1,024-item upper bound.
2. Run `cargo test --test queue explicit_list -- --nocapture` and confirm the new tests fail because the constructor does not exist.
3. Add a typed `QueueReplacementError` and a constructor/helper that builds a fresh `Queue` from `Vec<MediaItem>`, selected `MediaId`, preserved `RepeatMode`, and optional shuffle seed. Validate all invariants before returning the candidate queue.
4. Keep shuffle deterministic by using the existing queue shuffle path after selecting the requested item; never mutate an existing queue in this helper.
5. Rerun `cargo test --test queue explicit_list -- --nocapture`, then `cargo test --test queue --quiet`.
6. Commit with `git add src/queue.rs tests/queue.rs && git commit -m "feat: build atomic explicit-list queues"`.

### Task 2: Make explicit-list playback a single reducer transition

**Files:**
- Modify: `src/app/action.rs`
- Modify: `src/app/reducer.rs`
- Modify: `src/app/state.rs`
- Modify: `src/app/mod.rs`
- Modify: `tests/reducer.rs`
- Modify: `tests/workflows.rs`

1. Add failing reducer tests for `Action::PlayMediaList`: successful replacement starts the selected item, duplicates disappear, repeat survives, shuffle keeps selected current, radio is disabled, and invalid/over-limit/missing-selection inputs preserve the complete old playback state and emit no playback effects.
2. Add failing workflow coverage proving both music and podcast selections enter the existing resolution/progress-aware playback paths after a valid atomic commit.
3. Run `cargo test --test reducer play_media_list -- --nocapture` and `cargo test --test workflows explicit_list -- --nocapture` to record RED.
4. Add `Action::PlayMediaList { items, selected_id, shuffle_seed }`. In the reducer, derive the candidate via Task 1, swap it into state only after successful construction, and call the existing selected-item playback path. Map preparation failures to a safe `State` diagnostic without touching the old queue or playback.
5. Ensure ordinary resolver/device failures after a valid commit retain current behavior; do not treat them as queue-preparation rollback cases.
6. Run the focused tests, followed by `cargo test --test reducer --test workflows --quiet`.
7. Commit with `git add src/app src/queue.rs tests/reducer.rs tests/workflows.rs && git commit -m "feat: replace queue on explicit list playback"`.

### Task 3: Persist bounded Favorites in SQLite schema v3

**Files:**
- Create: `src/storage/schema_v3.sql`
- Modify: `src/storage/migrations.rs`
- Modify: `src/storage/repository.rs`
- Modify: `src/storage/mod.rs`
- Modify: `tests/storage.rs`

1. Add failing migration tests for upgrading v2 to v3 and for strict v3 table/index/column inventory validation.
2. Add failing repository tests for add/load/remove, full provider-plus-media identity, deterministic newest-first ordering when timestamps tie, persistence across reopen/session replacement, idempotent re-add, and `FavoriteInsertOutcome::Full` at 1,024 without eviction.
3. Run `cargo test --test storage favorite -- --nocapture` and confirm RED.
4. Add `favorites` with provider identity, media identity, serialized `MediaItem`, `favorited_at`, and monotonic row ID. Add a unique identity constraint plus newest-first index, and advance migration validation to schema version 3.
5. Add `FavoriteEntry` and typed insert outcome exports. Extend `Storage`/`SqliteStorage` with load, transactional capacity-checked insert, and remove operations. Keep capacity overflow a normal typed outcome rather than a redacted database error.
6. Run `cargo test --test storage favorite -- --nocapture`, then `cargo test --test storage --quiet`.
7. Commit with `git add src/storage tests/storage.rs && git commit -m "feat: persist local favorites in sqlite"`.

### Task 4: Add generation-safe Favorites state and ordered storage effects

**Files:**
- Modify: `src/app/action.rs`
- Modify: `src/app/effect.rs`
- Modify: `src/app/state.rs`
- Modify: `src/app/reducer.rs`
- Modify: `src/app/mod.rs`
- Modify: `src/runtime.rs`
- Modify: `src/cli.rs`
- Modify: `tests/reducer.rs`
- Modify: `tests/runtime.rs`
- Modify: `tests/workflows.rs`

1. Add failing reducer tests for startup load, selection preservation, add/remove completion, stale generation rejection, visible overflow, storage error display, and removal of the playing item leaving playback unchanged.
2. Add failing runtime tests proving favorite commands travel through the FIFO storage worker in order and map `Full` to a safe Favorites-category completion rather than a generic storage failure.
3. Add a failing startup test proving `FavoritesRequested` is dispatched with authentication and dependency initialization.
4. Run focused commands: `cargo test --test reducer favorites -- --nocapture`, `cargo test --test runtime favorites -- --nocapture`, and `cargo test --test workflows favorites -- --nocapture`.
5. Add `FavoritesState` with entries, stable selection, loading/loaded/error, generation, and pending mutation identity. Add `AppErrorCategory::Favorites`, request/toggle/completion actions, and load/add/remove effects.
6. Extend `RuntimeStorage`, `FifoStorage`, `StorageCommand`, and the ordered-storage loop for Favorites. Use the runtime clock for `favorited_at`; route every completion back through generation-bearing actions.
7. Dispatch initial `FavoritesRequested` from `src/cli.rs`. Do not add Favorites to `SessionCheckpoint` or session restore/reset code.
8. Run the focused tests and then `cargo test --test reducer --test runtime --test workflows --quiet`.
9. Commit with `git add src/app src/runtime.rs src/cli.rs tests/reducer.rs tests/runtime.rs tests/workflows.rs && git commit -m "feat: load and mutate favorites through runtime"`.

### Task 5: Convert playable UI activation to explicit-list playback

**Files:**
- Modify: `src/ui/controller.rs`
- Modify: `tests/ui_controller.rs`

1. Add table-driven failing controller tests for Search, Charts, History, Library playable rows, open-podcast episodes, and Favorites. Each test must assert one `PlayMediaList` action containing every currently loaded playable row, selected ID, and the next deterministic shuffle seed.
2. Add cases proving metadata rows are excluded, duplicate media IDs retain only their first row, Queue-panel activation remains direct queue playback, explicit enqueue behavior remains append-only, and no action is emitted for nonplayable selections.
3. Run `cargo test --test ui_controller explicit_list -- --nocapture` and confirm RED.
4. Replace the existing `enqueue_and_play`, search enqueue/play pair, and podcast-only activation with shared surface collectors that construct `PlayMediaList`. Increment the controller seed only when it is consumed for an enabled-shuffle list activation.
5. Rerun the focused tests, then `cargo test --test ui_controller --quiet`.
6. Commit with `git add src/ui/controller.rs tests/ui_controller.rs && git commit -m "feat: play loaded rows as replacement queues"`.

### Task 6: Bind `f` to focus-aware favorite toggling

**Files:**
- Modify: `src/ui/input.rs`
- Modify: `src/ui/controller.rs`
- Modify: `tests/ui_controller.rs`
- Modify: `tests/ui.rs`

1. Add failing input tests that `f` maps to `ToggleFavorite` only in normal mode and remains text input in Search/Palette entry modes.
2. Add failing controller tests for Content targets on Search, Charts, podcast episodes, Library, History, and Favorites; Queue targets the selected queue item; Player targets current playback; navigation, metadata, recommendations, settings, and empty selections do nothing.
3. Run `cargo test --test ui_controller toggle_favorite -- --nocapture` and the relevant `cargo test --test ui input -- --nocapture` filter.
4. Add `SemanticAction::ToggleFavorite`, its `f` binding/palette entry, and a single focus-aware media-target resolver that emits `Action::ToggleFavorite` without changing playback.
5. Run focused tests, then `cargo test --test ui_controller --test ui --quiet`.
6. Commit with `git add src/ui/input.rs src/ui/controller.rs tests/ui_controller.rs tests/ui.rs && git commit -m "feat: toggle favorites from focused media"`.

### Task 7: Add Favorites as a complete top-level TUI destination

**Files:**
- Create: `src/ui/views/favorites.rs`
- Modify: `src/ui/views/mod.rs`
- Modify: `src/ui/render.rs`
- Modify: `src/ui/interaction.rs`
- Modify: `src/ui/controller.rs`
- Modify: `tests/ui.rs`
- Modify: `tests/ui_controller.rs`
- Modify: `tests/snapshots/ui__wide.snap`
- Modify other snapshots only when their intentional output changes.

1. Add failing navigation/render tests for Favorites positioned after Library, keyboard left/right cycling, mouse navigation, list-row hit targets, shared viewport scrolling, selected artwork, and loading/empty/error/populated states.
2. Add a failing activation test showing a Favorites row uses the same explicit-list queue replacement path.
3. Run `cargo test --test ui favorites -- --nocapture` and `cargo test --test ui_controller favorites -- --nocapture` to record RED.
4. Add `NavigationItem::Favorites`, `ListSurface::Favorites`, `views::favorites`, controller selection/mouse handling, render routing, artwork selection, and navigation labels. Render newest-first entries directly from `FavoritesState` and keep visible overflow/storage errors in this view.
5. Review snapshot diffs with `INSTA_UPDATE=always cargo test --test ui snapshots -- --nocapture`, then rerun without update and inspect every changed `.snap` file.
6. Run `cargo test --test ui --test ui_controller --quiet`.
7. Commit with `git add src/ui tests/ui.rs tests/ui_controller.rs tests/snapshots && git commit -m "feat: add top-level favorites view"`.

### Task 8: Update user-facing keymap documentation

**Files:**
- Modify: `README.md`
- Modify: `docs/release-checklist.md` if its manual TUI checklist enumerates navigation/key actions.
- Modify: `tests/docs.rs`
- Modify: `tests/cli_help.rs` only if CLI/help output is affected.

1. Add or update failing documentation assertions for top-level Favorites, `f`, replacement-list playback, the 1,024 limits, shuffle/current behavior, and local SQLite durability.
2. Run `cargo test --test docs --test cli_help -- --nocapture` and confirm any new assertions fail before documentation changes.
3. Update the README keymap and behavior sections plus the applicable release checklist entries. Avoid promising network synchronization or playlist editing.
4. Rerun `cargo test --test docs --test cli_help --quiet`.
5. Commit with `git add README.md docs tests/docs.rs tests/cli_help.rs && git commit -m "docs: describe list playback and local favorites"`.

### Task 9: Cross-cutting verification and review

**Files:**
- Review all files changed since `0077c36`.
- Modify only files required to correct findings.

1. Run formatting and static checks: `cargo fmt --all -- --check` and `cargo clippy --all-targets --all-features -- -D warnings`.
2. Run the full suite: `cargo test --all-targets --all-features --quiet`.
3. Run targeted invariant tests again: `cargo test --test queue explicit_list -- --nocapture`, `cargo test --test storage favorite -- --nocapture`, and `cargo test --test ui_controller favorites -- --nocapture`.
4. Inspect `git diff --check`, `git status --short`, `git log --oneline 0077c36..HEAD`, and `git diff --stat 0077c36..HEAD`.
5. Request code review against the committed design, correct any verified findings with new RED/GREEN tests, and rerun all checks.
6. Commit review fixes separately if needed; do not amend implementation commits.
