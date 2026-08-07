# Player Controls, Notifications, Mouse, and Visual Effects Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add keyboard/media-key player controls, bounded seeking, cross-platform now-playing notifications, complete mouse interaction, compact artwork, theme-reactive spectrum colors, timestamp-aware lyric fades, and stable Charts scrolling.

**Architecture:** Keep `AppState` and the reducer as the durable behavior boundary. Add typed seek and notification effects, keep native notification work behind an injected bounded service, and let the renderer publish a latest-frame-only interaction map that converts mouse coordinates into existing semantic actions. Keep all visual transitions pure functions of normalized state, playback time, theme, and terminal capability.

**Tech Stack:** Rust 2024, Tokio, Crossterm 0.29, Ratatui 0.30, existing mpv supervisor/artwork/spectrum/lyrics services, target-specific native notification crates, Insta, Proptest.

---

Use `@superpowers:test-driven-development` for every task. Use `@superpowers:systematic-debugging` for any unexpected failure. Do not touch the pre-existing untracked `.DS_Store` in the main checkout.

### Task 1: Retain control and notification configuration in application state

**Files:**
- Modify: `src/config.rs`
- Modify: `src/app/state.rs`
- Modify: `config.example.toml`
- Test: `tests/config.rs`
- Test: `tests/docs.rs`

**Step 1: Write failing configuration tests**

Add assertions that defaults and minimal deserialization produce enabled notifications and retain podcast seek values in read-only state:

```rust
assert!(Config::default().notifications.enabled);

let state = AppState::new(Config::default());
assert_eq!(state.music_seek_seconds(), 10);
assert_eq!(state.podcast_skip_backward_seconds(), 15);
assert_eq!(state.podcast_skip_forward_seconds(), 30);
assert!(state.notifications_enabled());
```

Also assert that `config.example.toml` documents `[notifications] enabled = true`.

**Step 2: Run tests to verify they fail**

Run: `cargo test --test config --test docs config_defaults_include_notifications`

Expected: FAIL because `NotificationsConfig` and state accessors do not exist.

**Step 3: Implement the minimal configuration/state model**

Add:

```rust
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct NotificationsConfig {
    pub enabled: bool,
}

impl Default for NotificationsConfig {
    fn default() -> Self { Self { enabled: true } }
}
```

Add `notifications: NotificationsConfig` to `Config`. Extend `PresentationEnhancements` with the notification flag and the bounded podcast seek intervals. Expose const accessors. Use a private `const MUSIC_SEEK_SECONDS: u64 = 10`; do not add a music-seek configuration field until a user requirement needs one.

**Step 4: Run focused tests**

Run: `cargo test --test config --test docs`

Expected: PASS.

**Step 5: Commit**

```bash
git add src/config.rs src/app/state.rs config.example.toml tests/config.rs tests/docs.rs
git commit -m "feat: configure notifications and seek controls"
```

### Task 2: Add media keys and bounded relative-seek actions

**Files:**
- Modify: `src/ui/input.rs`
- Modify: `src/ui/controller.rs`
- Modify: `src/app/action.rs`
- Modify: `src/app/effect.rs`
- Modify: `src/app/reducer.rs`
- Modify: `src/runtime.rs`
- Test: `tests/ui.rs`
- Test: `tests/ui_controller.rs`
- Test: `tests/reducer.rs`
- Test: `tests/runtime.rs`
- Test: `tests/player.rs`

**Step 1: Write failing input/controller tests**

Cover ordinary function keys and Crossterm media-key variants:

```rust
assert_eq!(map(F(7), NONE), Some(PreviousTrack));
assert_eq!(map(F(8), NONE), Some(TogglePlayback));
assert_eq!(map(F(9), NONE), Some(NextTrack));
assert_eq!(map(Media(TrackPrevious), NONE), Some(PreviousTrack));
assert_eq!(map(Media(PlayPause), NONE), Some(TogglePlayback));
assert_eq!(map(Media(TrackNext), NONE), Some(NextTrack));
assert_eq!(map(Left, SHIFT), Some(SeekBackward));
assert_eq!(map(Right, SHIFT), Some(SeekForward));
```

Verify text entry and modal overlays consume these shortcuts consistently with existing global playback controls.

**Step 2: Run tests and observe failure**

Run: `cargo test --test ui normal_mode_maps_all_global_shortcuts -- --nocapture && cargo test --test ui_controller seek`

Expected: FAIL because seek semantic actions are missing.

**Step 3: Add semantic and application actions**

Add `SeekBackward` and `SeekForward` to `SemanticAction`, palette entries, help metadata, and controller dispatch. Add one reducer action and one player command:

```rust
Action::SeekRelativeRequested { seconds: i64 }
PlayerCommand::SeekRelative { seconds: i64 }
```

The controller selects `10` seconds for music and the retained configured interval for podcasts. Convert `u64` to `i64` with checked conversion and negate only after conversion.

**Step 4: Write reducer/runtime failing tests**

Verify no-current-media is a no-op, the reducer clamps the requested target against `0..=duration`, and the player command contains only the delta from the current position. Verify the ordered player worker calls `RuntimePlayer::seek_relative` and backend failures remain nonfatal.

**Step 5: Implement reducer and dispatcher support**

Use checked millisecond arithmetic:

```rust
let target_ms = if seconds < 0 {
    current_ms.saturating_sub(seconds.unsigned_abs().saturating_mul(1_000))
} else {
    current_ms.saturating_add(seconds as u64 * 1_000)
};
let bounded_ms = duration_ms.map_or(target_ms, |duration| target_ms.min(duration));
let delta_ms = bounded_ms as i128 - current_ms as i128;
```

Convert the bounded delta to whole seconds without overflow. Do not optimistically mutate playback position; accept supervisor progress as authoritative.

**Step 6: Run focused tests**

Run: `cargo test --test ui --test ui_controller --test reducer --test runtime --test player seek`

Expected: PASS.

**Step 7: Commit**

```bash
git add src/ui/input.rs src/ui/controller.rs src/app/action.rs src/app/effect.rs src/app/reducer.rs src/runtime.rs tests/ui.rs tests/ui_controller.rs tests/reducer.rs tests/runtime.rs tests/player.rs
git commit -m "feat: add media keys and relative seeking"
```

### Task 3: Render responsive button-style player controls and progress

**Files:**
- Modify: `src/ui/render.rs`
- Test: `tests/ui.rs`
- Test: `tests/snapshots/ui__wide_layout_snapshot.snap`
- Test: `tests/snapshots/ui__compact_layout_snapshot.snap`
- Test: `tests/snapshots/ui__tiny_layout_snapshot.snap`

**Step 1: Write failing renderer tests**

Render music and podcast states at wide, compact, and tiny boundaries. Assert the widest fitting labels include previous, rewind interval, current play/pause state, forward interval, and next. Assert podcast labels use configured `15s`/`30s`, music uses `10s`, and narrow layouts never overflow or remove essential telemetry.

**Step 2: Run the renderer tests**

Run: `cargo test --test ui player_control_labels -- --nocapture`

Expected: FAIL because the old line only says `[Space] pause [n/p] next/previous`.

**Step 3: Add pure responsive control builders**

Introduce a bounded presentation model:

```rust
struct PlayerControlLabel {
    action: SemanticAction,
    full: String,
    compact: String,
    tiny: &'static str,
}
```

Build the line using `bounded_format_cells` and deterministic width tiers. Use `Play` or `Pause` based on current status; do not use animation state to infer playback.

Add a deterministic text progress bar in wide/compact player rows so a later mouse hit can seek proportionally. Unknown or zero duration renders a disabled bar.

**Step 4: Run tests and review snapshots**

Run: `cargo test --test ui player_control_labels && INSTA_UPDATE=always cargo test --test ui layout_snapshot`

Expected: PASS; inspect every snapshot before accepting it.

**Step 5: Commit**

```bash
git add src/ui/render.rs tests/ui.rs tests/snapshots
git commit -m "feat: render responsive player controls"
```

### Task 4: Introduce a latest-frame interaction map and mouse event pipeline

**Files:**
- Create: `src/ui/interaction.rs`
- Modify: `src/ui/mod.rs`
- Modify: `src/ui/render.rs`
- Modify: `src/runtime.rs`
- Test: `tests/ui.rs`
- Test: `tests/runtime.rs`

**Step 1: Write failing interaction-map tests**

Test bounded region insertion, topmost-last resolution, frame revision replacement, zero-area rejection, stale revision rejection, and coordinate boundaries:

```rust
let mut map = InteractionMap::new(FrameRevision(7));
map.push(Rect::new(2, 3, 4, 2), HitTarget::Semantic(TogglePlayback));
assert_eq!(map.resolve(2, 3, FrameRevision(7)), Some(...));
assert_eq!(map.resolve(6, 3, FrameRevision(7)), None);
assert_eq!(map.resolve(2, 3, FrameRevision(6)), None);
```

**Step 2: Run tests to verify failure**

Run: `cargo test --test ui interaction_map && cargo test --test runtime mouse`

Expected: FAIL because no map or mouse runtime event exists.

**Step 3: Implement interaction types**

Use bounded, identity-safe targets only:

```rust
pub enum HitTarget {
    Semantic(SemanticAction),
    Navigation(NavigationItem),
    ListRow { surface: ListSurface, stable_index: usize },
    Progress { numerator: u16, denominator: u16 },
}
```

Cap regions per frame, never store media IDs, labels, URLs, or screen contents, and make `Debug` summary-only.

**Step 4: Add typed mouse events to the runtime**

Add `RuntimeEvent::Mouse(MouseEvent)` and `RuntimeMessage::Mouse(MouseEvent)`. Accept `CrosstermEvent::Mouse`. Treat mouse movement/scroll/click as lossy UI input in the bounded pending queue. Extend `Renderer` with a default empty `interaction_snapshot()` and let `TuiRenderer` return the current map plus revision after each successful draw.

**Step 5: Run focused tests**

Run: `cargo test --test ui interaction_map && cargo test --test runtime mouse`

Expected: PASS.

**Step 6: Commit**

```bash
git add src/ui/interaction.rs src/ui/mod.rs src/ui/render.rs src/runtime.rs tests/ui.rs tests/runtime.rs
git commit -m "feat: add bounded mouse interaction map"
```

### Task 5: Wire navigation, player controls, progress seeking, and mouse wheel

**Files:**
- Modify: `src/ui/interaction.rs`
- Modify: `src/ui/render.rs`
- Modify: `src/ui/controller.rs`
- Modify: `src/runtime.rs`
- Test: `tests/ui.rs`
- Test: `tests/ui_controller.rs`
- Test: `tests/runtime.rs`

**Step 1: Write failing end-to-end controller tests**

Cover:

- clicking every navigation destination focuses Navigation and opens it;
- clicking visible previous/seek/play/seek/next controls dispatches the matching action;
- clicking progress at the first, middle, and last columns emits bounded absolute-to-relative seek behavior;
- wheel up/down follows the same selection reducer path as `k`/`j`;
- clicks outside the latest frame are ignored;
- modal overlays suppress background controls;
- a resize invalidates the old map before the next draw.

**Step 2: Run tests to verify failure**

Run: `cargo test --test ui_controller mouse -- --nocapture && cargo test --test runtime mouse`

Expected: FAIL because targets are not emitted or dispatched.

**Step 3: Emit hit regions while rendering**

Thread a mutable `InteractionMapBuilder` through layout rendering. Register only the clipped visible cells for each control. Associate the progress bar with its exact interior columns. In overlays, start a modal interaction layer that replaces the background region set.

**Step 4: Resolve mouse gestures through the controller**

Add `reduce_mouse(controller, state, event, snapshot)`. Accept only left-button down/up according to one documented click policy, deduplicate a single physical click, and use Crossterm scroll events for one selection step. An already-selected row click activates it; maintain a bounded `(target, instant)` record only if double-click support needs to distinguish two clicks.

**Step 5: Run focused tests**

Run: `cargo test --test ui --test ui_controller --test runtime mouse`

Expected: PASS.

**Step 6: Commit**

```bash
git add src/ui/interaction.rs src/ui/render.rs src/ui/controller.rs src/runtime.rs tests/ui.rs tests/ui_controller.rs tests/runtime.rs
git commit -m "feat: control the TUI with the mouse"
```

### Task 6: Add mouse selection and activation to every list and picker

**Files:**
- Modify: `src/ui/views/search.rs`
- Modify: `src/ui/views/charts.rs`
- Modify: `src/ui/views/podcasts.rs`
- Modify: `src/ui/views/library.rs`
- Modify: `src/ui/views/history.rs`
- Modify: `src/ui/views/queue.rs`
- Modify: `src/ui/render.rs`
- Modify: `src/ui/controller.rs`
- Test: `tests/ui_controller.rs`
- Test: `tests/ui.rs`

**Step 1: Write failing surface-matrix tests**

For Search, Charts, podcast recommendations, podcast episodes, Library, History, Queue, country picker, browser picker, help, lyrics, and command palette, verify that visible row coordinates resolve to stable logical indices and clipped/non-visible rows do not. Verify click-select then click-activate semantics and overlay isolation.

**Step 2: Run tests to verify failure**

Run: `cargo test --test ui_controller mouse_surface_matrix -- --nocapture`

Expected: FAIL for unregistered list rows.

**Step 3: Return row geometry alongside rendered rows**

Do not reverse-map display strings. Each list renderer already knows the absolute index while slicing its viewport; register that index at the same time it creates the line. Keep the public domain action stable-ID validation in the reducer so stale indices remain harmless.

**Step 4: Implement picker and overlay targets**

Register only active modal choices. Help and plain lyrics wheel events scroll the overlay rather than background content. Command-palette rows dispatch their existing `SemanticAction` entries.

**Step 5: Run focused tests**

Run: `cargo test --test ui_controller mouse_surface_matrix && cargo test --test ui interaction`

Expected: PASS.

**Step 6: Commit**

```bash
git add src/ui/views src/ui/render.rs src/ui/controller.rs tests/ui_controller.rs tests/ui.rs
git commit -m "feat: make lists and overlays mouse-aware"
```

### Task 7: Fix the Charts sticky-header viewport jump

**Files:**
- Modify: `src/ui/views/charts.rs`
- Modify: `src/ui/render.rs`
- Test: `src/ui/render.rs`
- Test: `tests/ui.rs`

**Step 1: Write a failing viewport trace test**

Build at least three chart sections longer than the available window. Render after each one-step selection change down past the bottom, then back up. Capture the absolute visible item indices and assert:

```rust
assert_eq!(start_after_move_inside_window, previous_start);
assert_eq!(start_after_crossing_bottom, previous_start + 1);
assert_eq!(start_after_move_up_inside_window, previous_start);
```

Also cover the section boundary where the pinned header changes and terminal grow/shrink.

**Step 2: Run the test to demonstrate the bug**

Run: `cargo test chart_viewport_moves_only_at_visible_boundary -- --nocapture`

Expected: FAIL because sticky-header presence changes `list_rows` and the viewport start.

**Step 3: Reserve the header row consistently**

Remove the probe/render double calculation with different heights. For nonempty chart sections, reserve exactly one header row and call `visible_range` once with a stable item-row height. Derive the pinned section title from the selected row, or from the first visible item when there is no valid selection. Keep section headers out of the logical selectable-item count.

**Step 4: Run viewport and regression tests**

Run: `cargo test chart_viewport && cargo test --test ui`

Expected: PASS.

**Step 5: Commit**

```bash
git add src/ui/views/charts.rs src/ui/render.rs tests/ui.rs
git commit -m "fix: keep chart viewport stable"
```

### Task 8: Finalize compact artwork and tiny fallback behavior

**Files:**
- Modify: `src/ui/render.rs`
- Modify: `src/ui/artwork.rs`
- Test: `tests/ui.rs`
- Test: `tests/ui_artwork_safety.rs`

**Step 1: Write failing layout tests**

Assert wide uses animated presentation when valid, compact always chooses the static artwork store presentation at a bounded thumbnail size, and tiny never allocates artwork space. Assert content keeps a documented minimum width and no zero-sized decode is requested.

**Step 2: Run tests to verify current gaps**

Run: `cargo test --test ui artwork_layout -- --nocapture`

Expected: at least the tiny omission and compact-size assertions FAIL against current layout behavior.

**Step 3: Implement layout-specific presentation selection**

Split `artwork_presentation_from_stores` into an explicit layout policy. Wide may use animation; compact uses static `ArtworkPresentationStore` only; tiny returns `None`. Use a dedicated compact `CellSize` cap and avoid reusing the wide panel width blindly.

**Step 4: Run artwork tests**

Run: `cargo test --test ui artwork && cargo test --test ui_artwork_safety`

Expected: PASS.

**Step 5: Commit**

```bash
git add src/ui/render.rs src/ui/artwork.rs tests/ui.rs tests/ui_artwork_safety.rs
git commit -m "feat: show bounded compact player artwork"
```

### Task 9: Add theme-derived spectrum gradients

**Files:**
- Modify: `src/ui/theme.rs`
- Modify: `src/ui/render.rs`
- Test: `src/ui/render.rs`
- Test: `tests/ui.rs`

**Step 1: Write failing color tests**

For true color, assert low/middle/high bands differ and louder levels brighten without exceeding byte bounds. For ANSI/basic/monochrome, assert the result belongs to a small deterministic theme palette. Preserve paused/failed muted behavior.

**Step 2: Run tests to verify failure**

Run: `cargo test spectrum_gradient -- --nocapture`

Expected: FAIL because current colors are only accent or foreground.

**Step 3: Implement pure color interpolation**

Add helpers such as:

```rust
fn lerp_channel(start: u8, end: u8, numerator: u16, denominator: u16) -> u8;
fn spectrum_color(theme: &Theme, capability: ColorCapability, band: usize, bands: usize, level: u8) -> Color;
```

Carry the renderer’s actual `ColorCapability` rather than hard-coding true color. Use integer arithmetic for deterministic tests. Monochrome uses modifiers only.

**Step 4: Run focused tests**

Run: `cargo test spectrum_gradient && cargo test --test ui spectrum`

Expected: PASS.

**Step 5: Commit**

```bash
git add src/ui/theme.rs src/ui/render.rs tests/ui.rs
git commit -m "feat: color spectrum with theme gradients"
```

### Task 10: Add timestamp-aware synchronized-lyric fading

**Files:**
- Modify: `src/lyrics.rs`
- Modify: `src/ui/render.rs`
- Test: `src/ui/render.rs`
- Test: `tests/ui.rs`
- Test: `tests/reducer.rs`

**Step 1: Write failing transition tests**

Cover exact start, midpoint of the bounded transition window, settled line, short consecutive lines, open-ended final line, pause, backward/forward seek, true color, and basic/monochrome. Assert line count and text never change during a fade.

**Step 2: Run tests to verify failure**

Run: `cargo test lyric_fade -- --nocapture`

Expected: FAIL because active/non-active styles are currently binary.

**Step 3: Implement a pure lyric transition model**

Expose timestamp boundaries without exposing lyric text:

```rust
struct LyricTransition {
    outgoing: Option<usize>,
    incoming: usize,
    progress_millis: u16, // 0..=1000
}
```

Derive progress from `playback.position_ms` and adjacent starts. Cap the fade window (for example 400 ms) by half of each neighboring line duration. Seek recomputes from position; pause naturally freezes because position freezes.

**Step 4: Apply capability-aware styles**

True color interpolates muted to accent and toggles bold only after the midpoint. ANSI/basic uses a few discrete steps; monochrome uses modifier changes only. Do not add a timer task.

**Step 5: Run focused tests**

Run: `cargo test lyric_fade && cargo test --test ui lyrics && cargo test --test reducer lyrics`

Expected: PASS.

**Step 6: Commit**

```bash
git add src/lyrics.rs src/ui/render.rs tests/ui.rs tests/reducer.rs
git commit -m "feat: fade synchronized lyrics by timestamp"
```

### Task 11: Add bounded cross-platform native now-playing notifications

**Files:**
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Create: `src/notifications.rs`
- Modify: `src/lib.rs`
- Modify: `src/app/effect.rs`
- Modify: `src/app/reducer.rs`
- Modify: `src/runtime.rs`
- Modify: `src/cli.rs`
- Test: `tests/notifications.rs`
- Test: `tests/reducer.rs`
- Test: `tests/runtime.rs`

**Step 1: Select target-specific dependencies and lock them**

Use `notify-rust` 4.18 for Linux/macOS, enabling only the required Linux image and macOS UserNotifications features. Use a maintained WinRT toast crate behind `cfg(target_os = "windows")`. Keep target-specific dependencies in target tables so unsupported platform APIs do not compile on other hosts. Run `cargo tree -e features` and record why every enabled feature is needed in a source comment.

**Step 2: Write failing normalized-model and service tests**

Define bounded, redacted types:

```rust
pub struct NowPlayingNotification {
    generation: Generation,
    title: BoundedNotificationText,
    creator: Option<BoundedNotificationText>,
    collection: Option<BoundedNotificationText>,
    artwork: Option<NotificationArtwork>,
}

#[async_trait]
pub trait RuntimeNotifier: Send + Sync {
    async fn notify(&self, value: NowPlayingNotification) -> Result<(), NotificationError>;
}
```

Test UTF-8-safe title/creator/collection limits, generation deduplication, disabled configuration, missing artwork, backend error, cancellation, replacement, timeout, temp-file permissions/cleanup, and redacted `Debug`/`Display`.

**Step 3: Run tests to verify failure**

Run: `cargo test --test notifications -- --nocapture`

Expected: FAIL because the module does not exist.

**Step 4: Emit one notification effect on genuine playback start**

Extend `player_status_changed`: when the current generation first becomes `Playing`, emit both history and notification effects instead of returning after the first. Track `notification_emitted_generation` separately from history. Build the notification only from the current normalized queue item; never include media/provider IDs or URLs in public errors.

**Step 5: Implement the bounded dispatcher service**

Add one replaceable notification task with a short timeout. A platform backend converts normalized data to the native API. If supported, provide a bounded PNG attachment through a mode-0600 temporary file and remove it after the send call; otherwise omit artwork. Backend errors become one static diagnostic at most and never affect playback.

Because the existing artwork store retains terminal cells rather than encoded bytes, give the notifier a separate bounded artwork fetch/decode path or extend the artwork service with a secret-safe bounded PNG snapshot. Do not reconstruct notification art from terminal cells unless it preserves acceptable quality and alpha behavior.

**Step 6: Wire production and verify tests**

Construct the platform notifier in `ProductionStartup::enter_tui` only when enabled. Inject it through `RuntimeServices::with_notifier`. Ensure shutdown cancels and joins outstanding work within the existing cleanup deadline.

Run: `cargo test --test notifications --test reducer --test runtime`

Expected: PASS.

**Step 7: Verify cross-platform compilation**

Run the repository’s existing cross-platform CI/check commands. At minimum:

```bash
cargo check --all-targets --all-features
```

If the Windows target is installed, also run `cargo check --target x86_64-pc-windows-msvc --all-features`. Expected: PASS or explicitly report the unavailable target without claiming it ran.

**Step 8: Commit**

```bash
git add Cargo.toml Cargo.lock src/notifications.rs src/lib.rs src/app/effect.rs src/app/reducer.rs src/runtime.rs src/cli.rs tests/notifications.rs tests/reducer.rs tests/runtime.rs
git commit -m "feat: notify when playback changes"
```

### Task 12: Documentation, privacy audit, and complete verification

**Files:**
- Modify: `README.md`
- Modify: `config.example.toml`
- Modify: `docs/release-checklist.md`
- Modify: `tests/docs.rs`
- Modify: any focused tests needed to close discovered gaps

**Step 1: Write failing documentation-contract tests**

Require README coverage for button labels, F7/F8/F9 and media keys, Shift+arrow seeking, podcast intervals, mouse behavior, notification configuration/fallback/privacy, compact artwork, spectrum gradient degradation, lyric fades, and Charts viewport behavior.

**Step 2: Update user documentation**

Keep the keymap table and in-app help sourced from the same semantic-action inventory where practical. Document that native notification artwork depends on platform support and that notifications never block playback.

**Step 3: Run formatting and static checks**

Run:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
git diff --check
```

Expected: all exit 0.

**Step 4: Run the complete deterministic suite**

Run:

```bash
cargo test --all-targets --all-features
```

Expected: all non-ignored tests pass. Do not count ignored live tests as executed.

**Step 5: Perform privacy and resource-bound searches**

Run targeted searches for `Debug` derivations on notification/artwork payloads, unbounded interaction vectors, raw URL logging, blocking notification calls on the async runtime, and stale mouse-frame retention. Add sentinel tests for any boundary not already protected.

**Step 6: Run manual macOS smoke checks**

On macOS with permissions enabled:

1. Start a song and verify exactly one Notification Center entry with title and creator.
2. Switch tracks and verify updated metadata and artwork when the backend supports it.
3. Press F7/F8/F9 in both macOS function-key modes.
4. Click every player control and seek at three progress-bar positions.
5. Scroll Charts down and back up across section boundaries.

Report these results separately. Do not claim steps that were not actually observed.

**Step 7: Request final code review**

Use `@superpowers:requesting-code-review`. Resolve Critical and Important findings with `@superpowers:receiving-code-review`, rerun focused tests, then rerun the complete verification gate.

**Step 8: Commit documentation and final corrections**

```bash
git add README.md config.example.toml docs/release-checklist.md tests/docs.rs
git commit -m "docs: explain richer player interaction"
```

**Step 9: Verify the final branch state**

Run:

```bash
git status --short --branch
git log --oneline --decorate -15
```

Expected: clean feature worktree and a sequence of focused commits after the approved design commit.
