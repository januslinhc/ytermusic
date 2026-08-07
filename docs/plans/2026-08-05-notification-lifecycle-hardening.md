# Notification Lifecycle Hardening Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Bound notification cleanup by the runtime deadline while preserving committed native work and honest platform identity.

**Architecture:** Keep capacity-one replacement in `NotificationWorker`, but make shutdown deadline-aware. Move non-cooperative blocking work to detached OS threads, promote artwork into a bounded private cache, and define successful ownership transfer to the detached thread as the pre-cancellable commit followed by owned completion. Keep Windows unavailable by default; enable text-only toasts only when the user supplies an already-registered AppUserModelID.

**Tech Stack:** Rust 2024, Tokio, `notify-rust`, injected notification seams, target-specific cfg.

---

### Task 1: Bound worker and runtime shutdown

**Files:**
- Modify: `src/notifications.rs`
- Modify: `src/runtime.rs`
- Test: `tests/notifications.rs`
- Test: `tests/runtime.rs`

1. Add failing paused-time tests showing `NotificationWorker::shutdown(deadline)` and `EffectDispatcher::shutdown(deadline)` return when a notifier ignores cancellation forever.
2. Run the exact tests and verify they hang/fail under an outer timeout.
3. Add deadline-aware cancellation and bounded detach; thread the existing runtime deadline into notification shutdown.
4. Run the exact tests and existing replacement/cleanup tests to green.

### Task 2: Make blocking ownership runtime-independent

**Files:**
- Modify: `src/notifications.rs`
- Test: `tests/notifications.rs`

1. Add a failing test proving detached non-cooperative blocking work retains its attachment while the caller returns at its deadline.
2. Replace Tokio blocking-pool ownership with a dedicated detached OS thread plus a one-shot result channel.
3. Verify cancellation, normal cleanup, and runtime-drop tests pass.

### Task 3: Model native commit and platform requests

**Files:**
- Modify: `src/notifications.rs`
- Test: `tests/notifications.rs`

1. Add failing injected-backend tests for cancellation before and after the commit seam, including attachment lifetime.
2. Implement pre-commit cancellation and owned committed submission.
3. Add a pure platform-policy/request test proving Linux has no arbitrary ID and Windows does not use PowerShell identity.
4. Implement Linux ID removal, default Windows unavailability, and configured text-only Windows submission without registration side effects.
5. Run notification tests to green.

### Task 4: Verify and commit

**Files:**
- Modify: only files listed above and these plan documents.

1. Run focused notification, reducer, runtime, and workflow tests.
2. Run formatting, all-target/all-feature check, strict Clippy, and full tests.
3. Verify target dependency trees, privacy scans, and `git diff --check`.
4. Commit the scoped correction and report exact platform limitations.
