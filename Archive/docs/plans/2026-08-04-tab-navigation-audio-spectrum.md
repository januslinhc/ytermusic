# Tab Navigation and Audio-Reactive Spectrum Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add a three-region Tab focus cycle, wrapping horizontal navigation, and a bounded FFmpeg-backed frequency spectrum that genuinely reacts to the currently playing audio.

**Architecture:** Keep focus and navigation changes inside the existing semantic input/controller boundary. Propagate a redacted, generation-scoped audio-analysis URL through transient player presentation, decode low-rate mono PCM with a cancellable FFmpeg worker, compute bounded FFT bands in Rust, and publish only the newest lease-scoped spectrum frame to the renderer.

**Tech Stack:** Rust 2024, Tokio, crossterm, ratatui, FFmpeg, rustfft, existing resolver/player/reducer/runtime presentation architecture.

---

Use @test-driven-development for every production change, @systematic-debugging for unexpected failures, @requesting-code-review after every task, and @verification-before-completion before completion claims. Work in an isolated Git worktree and preserve unrelated `.DS_Store` state in the main checkout.

### Task 1: Add the three-region Tab focus cycle

**Files:**
- Modify: `src/ui/input.rs`
- Modify: `src/ui/controller.rs`
- Test: `tests/ui.rs`
- Test: `tests/ui_controller.rs`

**Step 1: Write failing input tests**

Add tests that specify:

```rust
assert_eq!(
    map_event(InputMode::Normal, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
    Some(InputAction::Semantic(SemanticAction::CycleFocusForward)),
);
assert_eq!(
    map_event(InputMode::Normal, KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT)),
    Some(InputAction::Semantic(SemanticAction::CycleFocusBackward)),
);
```

Also prove Tab and BackTab are ignored while editing Search or Command Palette text, and modal overlays do not leak focus movement.

**Step 2: Run the input tests to verify RED**

Run:

```bash
cargo test --test ui tab_focus
```

Expected: FAIL because the semantic actions and mappings do not exist.

**Step 3: Write failing controller tests**

Specify the exact stored-focus cycle:

```text
Navigation -> Content -> Player -> Navigation
Navigation <- Content <- Player <- Navigation
```

Start from Queue and prove forward/backward cycling normalizes to the approved three-region cycle rather than retaining Queue. Prove `Q` still toggles Queue independently.

**Step 4: Implement minimal focus actions and dispatch**

Add:

```rust
SemanticAction::CycleFocusForward
SemanticAction::CycleFocusBackward
```

Map `KeyCode::Tab` and `KeyCode::BackTab`, and explicitly consume them in `InputMode::TextEntry`. Implement explicit controller helpers rather than relying on enum order:

```rust
const fn cycle_focus(focus: FocusRegion, forward: bool) -> FocusRegion {
    match (focus, forward) {
        (FocusRegion::Navigation, true) => FocusRegion::Content,
        (FocusRegion::Content | FocusRegion::Queue, true) => FocusRegion::Player,
        (FocusRegion::Player, true) => FocusRegion::Navigation,
        (FocusRegion::Navigation, false) => FocusRegion::Player,
        (FocusRegion::Content | FocusRegion::Queue, false) => FocusRegion::Navigation,
        (FocusRegion::Player, false) => FocusRegion::Content,
    }
}
```

Add command-palette entries with accurate shortcuts. Do not change the existing `Q`, arrow, or `h/l` mappings in this task.

**Step 5: Verify GREEN and commit**

Run:

```bash
cargo test --test ui tab_focus
cargo test --test ui_controller tab_focus
cargo clippy --lib --tests --all-features -- -D warnings
```

Commit:

```bash
git add src/ui/input.rs src/ui/controller.rs tests/ui.rs tests/ui_controller.rs
git commit -m "feat: cycle primary focus with tab"
```

### Task 2: Switch wrapping navigation with horizontal keys

**Files:**
- Modify: `src/ui/controller.rs`
- Test: `tests/ui_controller.rs`

**Step 1: Write failing navigation tests**

With Navigation focused, prove both semantic horizontal actions immediately change views:

```text
Home --Right/l--> Search
Search --Left/h--> Home
Settings --Right/l--> Home
Home --Left/h--> Settings
```

Prove lazy view actions still dispatch when needed: entering Podcasts requests recommendations when empty, authenticated Library requests its active section, and History requests entries. Focus must remain Navigation after the view changes.

**Step 2: Run the tests to verify RED**

Run:

```bash
cargo test --test ui_controller horizontal_navigation
```

Expected: FAIL because `MoveLeft` currently stays on Navigation and `MoveRight` activates Content instead of cycling views.

**Step 3: Implement one ordered navigation helper**

Use `NavigationItem::ALL` and the existing bounded `moved_value` behavior. Extract the lazy action logic from `activate_navigation` so horizontal view switching can request data without moving focus:

```rust
fn switch_navigation_view(
    mut controller: UiController,
    state: &AppState,
    delta: isize,
) -> (UiController, Vec<Action>) {
    controller.model.view = moved_value(
        &NavigationItem::ALL,
        Some(&controller.model.view),
        delta,
    )
    .copied()
    .unwrap_or_default();
    let action = navigation_load_action(controller.model.view, state);
    (controller, action.into_iter().collect())
}
```

Route `MoveLeft`/`MoveRight` through this helper only when Navigation has focus. Preserve current focus movement behavior in Content, Queue, and Player.

**Step 4: Verify GREEN and commit**

Run:

```bash
cargo test --test ui_controller horizontal_navigation
cargo test --test ui_controller
```

Commit:

```bash
git add src/ui/controller.rs tests/ui_controller.rs
git commit -m "feat: browse navigation views horizontally"
```

### Task 3: Add visualizer configuration and bounded spectrum presentation models

**Files:**
- Modify: `src/config.rs`
- Modify: `config.example.toml`
- Modify: `src/app/state.rs`
- Create: `src/ui/spectrum.rs`
- Modify: `src/ui/mod.rs`
- Test: `tests/config.rs`
- Test: `src/ui/spectrum.rs` (unit-test module)

**Step 1: Write failing configuration tests**

Specify defaults and validation:

```rust
assert!(Config::default().visualizer.enabled);
assert_eq!(Config::default().visualizer.max_fps, 15);
```

Accept `1..=30`, reject `0` and values above `30` with `ConfigError::InvalidValue`, and prove old/minimal TOML receives defaults. Add save/load and example-config coverage.

**Step 2: Write failing spectrum model/store tests**

Specify opaque bounded models:

```rust
pub struct SpectrumTarget { bands: u16, rows: u8 }
pub struct SpectrumKey { generation: Generation, media_id: MediaId, target: SpectrumTarget }
pub struct SpectrumFrame { levels: Box<[u8]> }
pub struct SpectrumPresentation { frame: Option<Arc<SpectrumFrame>>, paused: bool, failed: bool }
pub struct SpectrumFrameStore { /* capacity-one slot + redraw revision + lease */ }
```

Test:

- band count `1..=64` and rows `1..=3`;
- levels bounded to `0..=24`;
- exact target size;
- empty/oversized/nonconforming frame rejection;
- capacity-one newest-frame replacement;
- generation/media/target matching;
- pause retains frame, resume allocates a fresh lease;
- old same-key lease cannot publish or fail;
- failure produces quiet fallback state;
- redraw revision coalesces;
- Debug exposes counts/status only, never IDs or levels.

**Step 3: Verify RED**

Run:

```bash
cargo test --test config visualizer
cargo test ui::spectrum::tests --lib
```

Expected: FAIL because config and spectrum models are absent.

**Step 4: Implement minimal config/models**

Add `VisualizerConfig { enabled, max_fps }` to `Config` with `#[serde(default)]`. Store `visualizer_enabled` in `PresentationEnhancements` and expose a read-only `AppState::visualizer_enabled()` accessor. Keep `max_fps` in runtime construction rather than durable state.

Implement private frame storage and monotonic leases modeled on `AnimationFrameStore`; do not expose mutable level storage.

**Step 5: Verify GREEN and commit**

Run:

```bash
cargo test --test config visualizer
cargo test ui::spectrum::tests --lib
cargo clippy --lib --tests --all-features -- -D warnings
```

Commit:

```bash
git add src/config.rs config.example.toml src/app/state.rs src/ui/spectrum.rs src/ui/mod.rs tests/config.rs
git commit -m "feat: define bounded spectrum presentation"
```

### Task 4: Compute deterministic FFT frequency bands

**Files:**
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Modify: `src/ui/spectrum.rs`
- Test: `src/ui/spectrum.rs` (unit-test module)

**Step 1: Write failing FFT tests**

Use generated sample fixtures, never copyrighted media:

- silence produces all-zero levels;
- 80 Hz emphasizes bass bands;
- 1 kHz emphasizes middle bands;
- 3 kHz emphasizes upper bands;
- NaN/infinite samples are rejected or normalized safely;
- input must contain exactly `FFT_SIZE` samples;
- normalization never exceeds level `24`;
- fast attack and slower decay are deterministic;
- Unicode/rendering concerns do not enter the analyzer model.

Use constants such as:

```rust
const ANALYSIS_SAMPLE_RATE: u32 = 8_000;
const FFT_SIZE: usize = 512;
const MAX_SPECTRUM_BANDS: usize = 64;
const MAX_SPECTRUM_LEVEL: u8 = 24;
```

**Step 2: Run tests to verify RED**

Run:

```bash
cargo test ui::spectrum::tests::fft --lib
```

Expected: FAIL because `SpectrumProcessor` is absent.

**Step 3: Add rustfft and implement minimal processing**

Add `rustfft` to dependencies. Implement a reusable `SpectrumProcessor` that:

1. applies a Hann window;
2. performs a fixed-size forward FFT;
3. discards DC and bins above Nyquist;
4. groups magnitudes into bounded logarithmic bands;
5. normalizes with a fixed floor/ceiling;
6. applies fast attack and slower decay against its previous frame;
7. returns a validated `SpectrumFrame`.

Preallocate working buffers and avoid logging samples, bins, or levels.

**Step 4: Verify GREEN and commit**

Run:

```bash
cargo test ui::spectrum::tests --lib
cargo clippy --lib --all-features -- -D warnings
```

Commit:

```bash
git add Cargo.toml Cargo.lock src/ui/spectrum.rs
git commit -m "feat: analyze bounded audio frequency bands"
```

### Task 5: Propagate a transient redacted audio-analysis source

**Files:**
- Modify: `src/resolver/mod.rs`
- Modify: `src/app/action.rs`
- Modify: `src/app/state.rs`
- Modify: `src/app/reducer.rs`
- Modify: `src/player/supervisor.rs`
- Test: `tests/resolver.rs`
- Test: `tests/player.rs`
- Test: `tests/reducer.rs`

**Step 1: Write failing URL/privacy tests**

Define an opaque `AnalysisStreamUrl` created only from a bounded HTTPS resolved audio URL. Test pre/post-parse 8 KiB limits, scheme/host validation, and redacted Debug/Display. Extend the existing `ResolvedStream` sentinel test to ensure analysis access does not expose URL, provider ID, video ID, or title.

**Step 2: Write failing supervisor/reducer tests**

Add a generation-scoped action:

```rust
Action::AnalysisStreamUpdated {
    generation: Generation,
    stream_url: Option<AnalysisStreamUrl>,
}
```

Prove:

- successful resolve emits it separately after format/preview presentation;
- mpv still receives only the original audio URL once;
- a rejected analysis URL is nonfatal to playback;
- same-generation refresh emits `None` to clear stale analysis;
- stale generation is ignored;
- replacement, stop, failure, and resolve failure clear it;
- `PlayerPresentation::Debug` and `Action::Debug` remain redacted;
- session/queue/database serialization never contains it.

**Step 3: Verify RED**

Run:

```bash
cargo test --test resolver analysis_stream
cargo test --test player analysis_stream
cargo test --test reducer analysis_stream
```

**Step 4: Implement transient propagation**

Add `analysis_url: Option<AnalysisStreamUrl>` to `PlayerPresentation` with a read-only accessor. Construct it from `ResolvedStream::url`; do not add another yt-dlp request. Always emit `Some` or `None`, just like preview refresh clearing. Update every lifecycle clearing path.

Do not add this field to `SessionCheckpoint`, queue snapshots, SQLite, cache keys, or logs.

**Step 5: Verify GREEN and commit**

Run:

```bash
cargo test --test resolver analysis_stream
cargo test --test player analysis_stream
cargo test --test reducer analysis_stream
cargo test --test player
```

Commit:

```bash
git add src/resolver/mod.rs src/app/action.rs src/app/state.rs src/app/reducer.rs src/player/supervisor.rs tests/resolver.rs tests/player.rs tests/reducer.rs
git commit -m "feat: expose transient audio analysis source"
```

### Task 6: Decode bounded PCM and publish spectrum frames

**Files:**
- Modify: `src/ui/spectrum.rs`
- Test: `src/ui/spectrum.rs` (unit-test module)

**Step 1: Write failing decoder/worker tests**

Specify injected boundaries:

```rust
#[async_trait]
pub trait SpectrumDecoder: Send + Sync {
    async fn decode(
        &self,
        request: SpectrumRequest,
        output: watch::Sender<Option<Result<Arc<SpectrumFrame>, SpectrumError>>>,
        cancel: CancellationToken,
    ) -> Result<(), SpectrumError>;
}

#[async_trait]
pub trait SpectrumPacer: Send + Sync {
    async fn wait(&self, duration: Duration);
}
```

Test with fake processes and paused Tokio time:

- direct argv contains `-readrate 1`, bounded `-ss`, `-vn`, mono, 8 kHz, `pcm_f32le`, and `pipe:1`;
- the opaque URL is one argv value and no `sh`/`-c` appears;
- stderr is null/sanitized and no files are created;
- exact 512-sample frames are parsed from little-endian f32;
- partial, oversized, NaN-heavy, or malformed output returns a static error;
- max-FPS pacing uses fake time;
- newest output wins under a slow publisher;
- pause cancels/reaps decoder and retains the frame;
- resume uses current `start_ms` and a new lease;
- replacement/seek/clear/shutdown retire old work;
- stalled stdout, wall timeout, cancellation, and non-cooperative post-kill wait return within a bound and retain child ownership for reaping;
- Debug/errors never contain URL, IDs, samples, bins, or levels.

**Step 2: Run tests to verify RED**

Run:

```bash
cargo test ui::spectrum::tests::worker --lib
cargo test ui::spectrum::tests::ffmpeg --lib
```

**Step 3: Implement the bounded worker**

Implement `SpectrumRequest`, `SpectrumDecoder`, `SpectrumPacer`, `SpectrumWorker`, and `FfmpegSpectrumDecoder`. Model lifecycle ownership after the hardened `AnimationWorker`, including per-run leases, bounded retirement, and detached reaper ownership when an OS wait exceeds the synchronous grace period.

Use conservative FFmpeg bounds:

- direct process argv;
- network I/O timeout;
- bounded probe/analyze sizes;
- low-rate mono output;
- fixed sample chunk size;
- bounded process runtime;
- null stderr;
- explicit kill plus wait on cancel/timeout.

The worker processes frames with `SpectrumProcessor` and publishes through the capacity-one store. It must not block the runtime event loop.

**Step 4: Verify GREEN and commit**

Run:

```bash
cargo test ui::spectrum::tests --lib
cargo clippy --lib --all-features -- -D warnings
```

Commit:

```bash
git add src/ui/spectrum.rs
git commit -m "feat: decode bounded audio spectrum frames"
```

### Task 7: Integrate spectrum lifecycle and redraw into runtime

**Files:**
- Modify: `src/runtime.rs`
- Modify: `src/cli.rs`
- Test: `tests/runtime.rs`
- Test: `tests/ui_artwork_safety.rs`

**Step 1: Write failing runtime eligibility tests**

Inject a fake decoder and prove:

- Playing plus enabled config, analysis URL, FFmpeg worker, and Wide layout starts analysis;
- Compact also starts with a one-row target;
- Tiny never starts and clears active analysis;
- disabled config or missing analysis URL never starts;
- pause cancels/reaps decode and freezes the last frame;
- resume restarts at authoritative playback position;
- large forward/backward progress discontinuity restarts at the new position;
- replacement and same-key restart allocate distinct leases;
- stop/failure/shutdown clear and reap;
- analyzer errors preserve playback and other presentations.

**Step 2: Write failing redraw tests**

Prove a spectrum frame causes `Renderer::render_with_model` without a terminal/player action, 256 rapid frames coalesce, closed redraw receivers are removed, terminal cancellation stays highest priority, and simultaneous animation/spectrum redraw channels do not busy-loop or starve the action bus.

**Step 3: Verify RED**

Run:

```bash
cargo test --test runtime spectrum
cargo test --test ui_artwork_safety spectrum
```

**Step 4: Implement runtime composition**

Add `Option<SpectrumWorker>` and `Arc<SpectrumFrameStore>` to runtime/renderer composition. Implement:

```rust
fn spectrum_request_for_state(state: &AppState, area: Option<Rect>) -> Option<SpectrumRequest>;
fn reconcile_spectrum(worker: &mut Option<SpectrumWorker>, state: &AppState, area: Option<Rect>);
```

Compute targets from layout and bounded terminal width: Wide rows `3`, Compact rows `1`, Tiny `None`, at most 64 bands. Track position discontinuities for seek restart without restarting on normal progress ticks.

Extend the biased runtime select with a separate coalesced spectrum redraw receiver. Shutdown the worker before terminal teardown. In `ProductionStartup`, find FFmpeg once and construct animation and spectrum workers independently according to their respective config flags.

**Step 5: Verify GREEN and commit**

Run:

```bash
cargo test --test runtime spectrum
cargo test --test runtime animation
cargo test --test ui_artwork_safety
cargo clippy --all-targets --all-features -- -D warnings
```

Commit:

```bash
git add src/runtime.rs src/cli.rs tests/runtime.rs tests/ui_artwork_safety.rs
git commit -m "feat: synchronize spectrum analysis with playback"
```

### Task 8: Render spectrum strips and document the keymap

**Files:**
- Modify: `src/runtime.rs`
- Modify: `src/ui/render.rs`
- Modify: `src/ui/views/settings.rs`
- Modify: `README.md`
- Modify: `tests/ui.rs`
- Modify: `tests/docs.rs`
- Test: `src/ui/render.rs` (unit-test module)

**Step 1: Write failing renderer tests**

Use a real `SpectrumFrameStore` and actual terminal buffers. Prove:

- Wide reserves three spectrum rows and renders multiple frequency bands;
- Compact reserves one row;
- Tiny contains no spectrum and retains its one-row player;
- bass bands use `theme.accent`;
- paused presentation freezes and dims the latest frame;
- missing/failed analysis renders a quiet baseline;
- consecutive frames change Wide/Compact buffers;
- spectrum never covers timed lyrics, artwork, progress, volume, modes, or controls;
- resize preserves bounded list viewports and player visibility;
- disabled visualizer preserves pre-feature layout heights exactly.

**Step 2: Verify RED**

Run:

```bash
cargo test ui::render::tests::spectrum --lib
cargo test --test ui spectrum
```

**Step 3: Implement rendering**

Pass one already-bounded `SpectrumPresentation` from `TuiRenderer` into the render boundary. Preserve existing wrapper functions for tests without spectrum.

Increase player height only when visualizer presentation is eligible:

```text
Wide: base player + 3 spectrum rows + optional 3 lyric rows
Compact: base player + 1 spectrum row + optional 1 lyric row
Tiny: unchanged
```

Insert spectrum lines between core player metadata/controls and automatic lyrics. Resample levels to the available band count, use Unicode block cells with bounded fallback, apply accent to low-frequency bands, and dim paused output. Never render raw numeric samples/bins.

Update Settings to show visualizer enabled/max FPS. Update README and keyboard reference with Tab, Shift-Tab, horizontal navigation wrapping, layouts, FFmpeg requirement, extra low-bandwidth decode, privacy, config, and fallback behavior. Update docs tests from palette entries without brittle whole-table matching.

**Step 4: Verify GREEN and commit**

Run:

```bash
cargo test ui::render::tests::spectrum --lib
cargo test --test ui
cargo test --test ui_controller
cargo test --test docs
cargo test viewport --all-targets --all-features
```

Commit:

```bash
git add src/runtime.rs src/ui/render.rs src/ui/views/settings.rs README.md tests/ui.rs tests/docs.rs
git commit -m "feat: render audio-reactive player spectrum"
```

### Task 9: Add a safe live smoke and complete final verification

**Files:**
- Modify: `tests/live/provider_live.rs`
- Modify only files required by valid review findings.

**Step 1: Write the ignored live smoke**

Add an independent feature-gated test:

```rust
#[tokio::test]
#[ignore = "requires explicit live-test gate, network access, yt-dlp, and FFmpeg"]
async fn anonymous_audio_stream_produces_one_bounded_spectrum_frame() -> Result<(), Box<dyn Error>> {
    require_explicit_live_gate()?;
    // Search a stable playable item, resolve it, analyze one frame, and assert
    // only band count/range. Never print IDs, URLs, samples, bins, or levels.
    Ok(())
}
```

Require exact `YTERMUSIC_LIVE_TESTS=1`; external unavailability must fail explicitly with a safe static message rather than silently pass.

**Step 2: Compile focused live/docs targets**

Run:

```bash
cargo test --features live-tests --test provider_live --no-run
cargo test --test docs
```

Expected: PASS; the live smoke compiles and remains ignored.

**Step 3: Run fresh offline gates**

Run:

```bash
cargo fmt --all -- --check
cargo test --all-targets --all-features
cargo clippy --all-targets --all-features -- -D warnings
git diff --check
git status --short --branch
```

Expected: all offline targets pass; only intentional live/fixture tests are ignored, and the feature worktree is clean except for committed work.

**Step 4: Run the new live smoke when network is available**

Run independently:

```bash
YTERMUSIC_LIVE_TESTS=1 cargo test --features live-tests --test provider_live anonymous_audio_stream_produces_one_bounded_spectrum_frame -- --ignored --exact
```

Record the actual pass/failure and duration. Do not claim execution when only compilation ran.

**Step 5: Request final review and fix findings**

Use @requesting-code-review across the complete feature range. Audit:

- Tab/text-entry/overlay behavior;
- navigation wrapping and lazy loads;
- FFT math/bounds and non-finite input;
- URL/ID/title/sample/bin/level privacy;
- transient non-persistence;
- FFmpeg argv, pacing, I/O/wall limits, and kill/reap ownership;
- pause/resume/seek/replacement leases;
- dual redraw channel fairness/no busy loop;
- playback/fades unchanged;
- renderer overlap and viewport stability;
- live-test gating and content safety.

Fix every valid Critical, Important, and Minor finding test-first and re-review until PASS.

**Step 6: Commit any final verified fixes**

Use focused commit messages describing actual findings. Rerun all completion gates after the final commit.

