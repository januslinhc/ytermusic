# Task 16 Runtime Quality Corrections Design

## Scope

This correction keeps the reducer as the sole `AppState` writer and preserves
the existing provider, player, storage, account, artwork, and terminal
boundaries. It addresses only bounded cleanup, bounded internal work, accepted
action draining, artwork identity synchronization, and serialized account
imports.

## Bounded shutdown and accepted-action drain

Runtime cleanup uses one absolute deadline derived from the injectable shutdown
timeout. Quit, signal, injected panic, and controller quit first stop producer
tasks and close the bounded action receiver. Messages already accepted by the
receiver are then drained in channel order. Accepted `Action` messages pass
through the reducer and their durable effects retain FIFO ordering. Redraws and
keys after the terminal message are ignored; another accepted panic upgrades the
final outcome to panic after cleanup.

The latest coherent session checkpoint is offered before lane shutdown. Storage
and session workers are aborted when the shared deadline expires. Player
shutdown is always attempted and the player is aborted when it does not
acknowledge before the deadline. Terminal restoration and panic resumption occur
regardless of hung asynchronous storage futures. A synchronous storage owner
blocked in operating-system code may remain detached, but no asynchronous
runtime waiter may keep the terminal lifecycle open indefinitely.

## Finite effect lanes and task ownership

Ordered player and storage commands use explicit finite Tokio channels.
Scheduling is cancellation-aware and preserves semantic work. The runtime must
not await a full lane while it is the only consumer capable of draining a lane
worker's result from the bounded action bus. Dispatch therefore pumps accepted
runtime messages while waiting for lane capacity, with a deterministic
saturation test covering the send-to-receive cycle.

Session persistence uses a one-latest watch or single-slot value. Updates
coalesce while retaining the existing debounce and an explicit best-effort final
flush. Provider and effect tasks live in a `JoinSet` and are opportunistically
reaped so completed handles do not accumulate.

## Artwork identity synchronization

One reducer helper synchronizes the displayed identity with an optional artwork
URL. A new valid URL allocates one generation and emits one fetch. The same URL
deduplicates. A missing or invalid URL immediately clears artwork state and
emits an invalidation effect that cancels the old fetch and clears the shared
presentation slot. Old completions are generation-rejected.

Search, chart, podcast, library, and history auto-selection and selection
changes use the helper, as do queue and playback identity changes. Playback
resolution remains the first effect and artwork fetch follows it.

## Serialized account transactions

Account imports use one finite FIFO lane rather than independently spawned
tasks. Each prepare, provider construction, credential commit, and provider swap
runs in request order. Failures before commit retain the previous coherent
credential/provider pair.

Cancellation may stop queued work or an active attempt before the credential
commit critical section. Once commit starts, the lane preserves the
commit-to-provider-swap sequence. Terminal restoration remains governed by the
independent cleanup deadline, while task ownership remains explicit and
reapable; no import task is silently detached.

## Rejected alternatives

Semaphores around the existing unbounded senders do not bound queued values,
propagate pressure, or serialize account transactions. A single monolithic
runtime executor would strengthen ordering but would unnecessarily replace the
established architecture and increase regression risk.

## Verification

Each correction group begins with deterministic failing tests and receives a
focused green run. Tests use barriers, counters, bounded capacities, and paused
time where appropriate; they do not depend on arbitrary task yields. Final
verification covers the requested focused suites, formatting, strict Clippy,
two normal-scheduling all-target/all-feature test passes, doc tests, build,
doctor, diff hygiene, snapshot artifacts, and a full self-review.
