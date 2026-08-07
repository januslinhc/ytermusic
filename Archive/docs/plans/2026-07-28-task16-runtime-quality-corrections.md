# Task 16 Runtime Quality Corrections Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Make runtime cleanup, internal work, accepted-action draining, artwork identity, and account imports bounded and coherent.

**Architecture:** Keep the reducer and existing service traits as the application boundaries. Add finite ordered lanes, a single absolute cleanup deadline, an accepted-message drain, a one-latest session slot, explicit artwork invalidation, and FIFO account ownership. Lane commands that can answer through the action bus must own reserved reply capacity so a full lane cannot form a send/receive deadlock.

**Tech Stack:** Rust, Tokio bounded `mpsc`, `watch`, `CancellationToken`, `JoinSet`, Ratatui, async-trait, existing reducer/runtime test harnesses.

---

### Task 1: Bound cleanup around hung storage

**Files:**
- Modify: `tests/runtime.rs`
- Modify: `src/runtime.rs`

**Step 1: Write the failing tests**

Add controlled `RuntimeStorage` doubles whose save or load futures announce
entry and remain pending. Cover normal quit, injected signal, and injected
panic. Use paused Tokio time and the existing terminal/player recorders to
assert:

```rust
assert!(runtime_finishes_at_cleanup_deadline);
assert!(calls.contains(&"player_shutdown"));
assert!(calls.contains(&"disable_raw"));
assert!(panic_join.is_panic());
```

**Step 2: Run tests to verify RED**

Run:

```bash
cargo test --test runtime hung_storage -- --nocapture
```

Expected: timeout because ordered storage or session shutdown waits forever.

**Step 3: Implement the minimal deadline**

Introduce one absolute cleanup deadline. Stop producer tasks, close the action
receiver, and deadline-bound storage/session/player worker joins. Abort hung
async workers and always attempt or abort player shutdown before terminal
restoration. Preserve the final checkpoint as best effort.

**Step 4: Run focused tests to verify GREEN**

Run:

```bash
cargo test --test runtime hung_storage -- --nocapture
```

Expected: all hung-storage lifecycle tests pass under paused time.

### Task 2: Bound effect lanes, coalesce sessions, and reap tasks

**Files:**
- Modify: `tests/runtime.rs`
- Modify: `src/runtime.rs`

**Step 1: Write the failing pressure tests**

Add barriers and counters for:

- a full ordered lane whose worker is trying to publish to a saturated action
  bus;
- slow player and storage boundaries under bursts;
- repeated session checkpoints retaining only the newest value;
- many completed provider/effect tasks leaving bounded retained ownership.

Assert explicit capacities rather than relying on yields.

**Step 2: Run tests to verify RED**

Run:

```bash
cargo test --test runtime bounded_internal_work -- --nocapture
cargo test --test runtime saturated_lane_cannot_deadlock_action_bus -- --nocapture
```

Expected: unbounded lane/task/session behavior or timeout.

**Step 3: Implement finite ownership**

Replace player/storage `unbounded_channel` values with finite `mpsc::channel`
values. Make effect dispatch asynchronous and cancellation-aware. Reserve one
action-bus reply permit before awaiting a response-producing lane command so
the worker never blocks behind the sole consumer. Replace session commands with
a watch/single-latest value and replace `Vec<JoinHandle<_>>` with `JoinSet`.

**Step 4: Run focused tests to verify GREEN**

Run the two commands from Step 2 plus:

```bash
cargo test --test runtime -- --nocapture
```

Expected: pressure tests and the complete runtime suite pass.

### Task 3: Drain accepted actions after terminal requests

**Files:**
- Modify: `tests/runtime.rs`
- Modify: `src/runtime.rs`

**Step 1: Write the failing drain tests**

Use coordinated event and player producers to put `PlayerProgress` and another
state-bearing action behind an already accepted quit or signal. Assert the final
session/podcast writes contain the latest accepted position and remain FIFO.
Add accepted key/redraw/panic coverage with the documented post-terminal
behavior.

**Step 2: Run tests to verify RED**

Run:

```bash
cargo test --test runtime accepted_actions_are_drained -- --nocapture
```

Expected: final state reflects the message before quit, not the accepted message
behind it.

**Step 3: Implement close-and-drain**

Use a control-plane terminal request to break blocked lane sends. Once terminal
handling starts, stop producers, close the bounded receiver, reduce all buffered
`Action` messages in order, ignore buffered redraws/keys, remember buffered
panic, and dispatch durable effects before the shared cleanup deadline.

**Step 4: Run focused tests to verify GREEN**

Run the command from Step 2 and the runtime suite. Expected: accepted state is
durable and cleanup remains bounded.

### Task 4: Synchronize every displayed artwork identity

**Files:**
- Modify: `src/app/effect.rs`
- Modify: `src/app/reducer.rs`
- Modify: `src/ui/artwork.rs`
- Modify: `src/runtime.rs`
- Modify: `tests/reducer.rs`
- Modify: `tests/runtime.rs`
- Modify: `tests/workflows.rs`

**Step 1: Write the failing identity tests**

Cover art-to-no-art, no-art-to-art, and same-URL transitions for search
auto-selection, charts, podcast episodes, playable and browse library items,
history entries, and playback/queue transitions. Assert:

```rust
assert_eq!(state.artwork().requested_url(), None);
assert_eq!(effects, vec![Effect::ClearArtwork]);
assert!(stale_completion_has_no_effect);
```

Also assert playback emits `Resolve` before `FetchArtwork`.

**Step 2: Run tests to verify RED**

Run:

```bash
cargo test --test reducer artwork_identity -- --nocapture
cargo test --test workflows artwork_identity -- --nocapture
```

Expected: old artwork remains or auto-selected rows emit no fetch.

**Step 3: Implement centralized synchronization**

Add `sync_artwork(state, Option<Url>)`, `Effect::ClearArtwork`, presentation-store
clear, and runtime cancellation. Invoke the helper from every selection,
auto-selection, and playback identity path. Deduplicate exact URLs.

**Step 4: Run focused tests to verify GREEN**

Run reducer, workflow, UI, and runtime artwork tests. Expected: every identity
transition is immediate and stale-safe.

### Task 5: Serialize account imports

**Files:**
- Modify: `src/runtime.rs`
- Modify: `tests/runtime.rs`
- Modify: `tests/auth.rs`

**Step 1: Write the failing ordering test**

Start two browser imports with controllable prepare/create/commit barriers and
reverse completion pressure. Assert only one attempt enters at a time and the
final committed credential label equals the shared provider label. Add shutdown
coverage before commit and while commit is active, with explicit task-reaping
observation.

**Step 2: Run tests to verify RED**

Run:

```bash
cargo test --test runtime account_imports_are_serialized -- --nocapture
```

Expected: independently spawned imports overlap or finish with mismatched
credential/provider identities.

**Step 3: Implement the account lane**

Add one finite FIFO account lane. Cancel queued and pre-commit work on shutdown.
Represent the commit critical section explicitly so commit and synchronous
provider replacement cannot be interrupted. Keep the worker owned and reaped;
terminal restoration uses the independent cleanup deadline.

**Step 4: Run focused tests to verify GREEN**

Run runtime and auth suites. Expected: imports are ordered and each observed
credential/provider pair is coherent.

### Task 6: Verify and commit

**Files:**
- Modify only files named above and this plan.

**Step 1: Run focused suites**

```bash
cargo test --test runtime --test auth --test reducer --test player --test storage --test ui --test ui_controller --test workflows
```

**Step 2: Run release gates**

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
cargo test --all-targets --all-features
cargo test --doc
cargo build
cargo run -- doctor
git diff --check
find . -name '*.snap.new' -print
```

Expected: every command succeeds; doctor reports the actual local dependency
state; snapshot search is empty.

**Step 3: Self-review and commit**

Review the complete diff for deadlines, cancellation, secret redaction, FIFO
ordering, bounded capacities, artwork staleness, and task ownership. Commit
without amending:

```bash
git add <explicit reviewed files>
git commit -m "fix: bound runtime work and synchronize state"
```
