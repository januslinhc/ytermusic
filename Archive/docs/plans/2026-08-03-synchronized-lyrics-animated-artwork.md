# Synchronized Lyrics and Animated Artwork Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add automatically followed synchronized lyrics, a full lyrics overlay, and genuine video-backed animated artwork without allowing either enhancement to disrupt audio playback.

**Architecture:** Normalize YouTube plain lyrics and conservatively matched LRCLIB synchronized lyrics behind a generation-safe runtime boundary with memory-only caching. Extend resolved media with an optional redacted low-resolution video URL and feed cancellable FFmpeg frames through a capacity-one presentation store. Keep timing in pure app state, overlay interaction in the UI controller, and terminal/layout concerns in the renderer.

**Tech Stack:** Rust 2024, Tokio, reqwest/rustls, serde/serde_json, ytmapi-rs lyrics queries, yt-dlp, FFmpeg, ratatui, existing reducer/effect/runtime/artwork architecture.

---

Use @test-driven-development for each production change, @systematic-debugging for unexpected failures, @requesting-code-review after each task, and @verification-before-completion before completion claims.

### Task 1: Add configuration and bounded lyrics domain models

**Files:**
- Modify: `src/config.rs`
- Modify: `config.example.toml`
- Create: `src/lyrics.rs`
- Modify: `src/lib.rs`
- Test: `tests/config.rs`
- Test: `src/lyrics.rs` (unit-test module)

**Step 1: Write failing configuration tests**

Add tests proving defaults and validation:

```rust
assert!(Config::default().lyrics.enabled);
assert!(Config::default().lyrics.external_sync);
assert!(Config::default().artwork.animated);
assert_eq!(Config::default().artwork.max_fps, 8);
```

Reject `max_fps == 0` and values above a conservative cap such as 15. Confirm old configuration files deserialize through `#[serde(default)]` unchanged.

**Step 2: Write failing lyrics-model tests**

Specify opaque, summary-only models:

```rust
pub enum LyricsSource { YouTubeMusic, Lrclib }
pub struct TimedLyricLine { start_ms: u64, end_ms: Option<u64>, text: String }
pub struct LyricsDocument {
    source: LyricsSource,
    plain: Option<String>,
    timed: Vec<TimedLyricLine>,
    instrumental: bool,
}
```

Test constructor limits for total bytes, line count, per-line text, timestamp order, and redacted/summary Debug output. Add `active_line(position_ms)` boundary cases at exact start/end timestamps.

**Step 3: Verify RED**

Run: `cargo test --test config lyrics`

Run: `cargo test lyrics::tests --lib`

Expected: FAIL because the new config sections and lyrics module do not exist.

**Step 4: Implement minimal models/config**

Add `LyricsConfig { enabled, external_sync }` and `ArtworkConfig { animated, max_fps }` to `Config`, with approved defaults and validation. Implement bounded lyrics constructors/accessors; never expose mutable timed-line storage or lyric text through Debug.

**Step 5: Verify GREEN and commit**

Run: `cargo test --test config`

Run: `cargo test lyrics::tests --lib`

Run: `cargo clippy --lib --tests --all-features -- -D warnings`

Commit:

```bash
git add src/config.rs config.example.toml src/lyrics.rs src/lib.rs tests/config.rs
git commit -m "feat: define bounded lyrics configuration and models"
```

### Task 2: Parse LRC and conservatively match LRCLIB records

**Files:**
- Modify: `src/lyrics.rs`
- Test: `src/lyrics.rs` (unit-test module)

**Step 1: Write failing LRC parser tests**

Cover `[mm:ss]`, two- and three-digit fractions, multiple timestamps on one line, blank text, duplicate timestamps, out-of-order input, malformed timestamps, oversized documents, and plain fallback. Assert normalized milliseconds and deterministic duplicate handling.

**Step 2: Write failing LRCLIB match tests**

Deserialize bounded `/api/search` fixtures and require:

- exact normalized title;
- exact normalized primary artist or safe artist-list agreement;
- duration within a small fixed tolerance when both durations exist;
- rejection of ambiguous equal matches;
- preference for synchronized content over plain content only after metadata acceptance.

Use punctuation/case/whitespace normalization, not fuzzy edit distance.

**Step 3: Verify RED**

Run: `cargo test lyrics::tests::lrc --lib`

Run: `cargo test lyrics::tests::lrclib --lib`

Expected: FAIL because parser and matcher are missing.

**Step 4: Implement the parser and matcher**

Parse incrementally within response, result, line, and text caps. Sort accepted timed lines by timestamp, retain one deterministic line for exact duplicate timestamps, and derive `end_ms` from the next line. Reject raw provider IDs and URLs from normalized state. Error variants must be static and payload-free.

**Step 5: Verify GREEN and commit**

Run: `cargo test lyrics::tests --lib`

Run: `cargo clippy --lib --tests --all-features -- -D warnings`

Commit:

```bash
git add src/lyrics.rs
git commit -m "feat: parse and match synchronized lyrics"
```

### Task 3: Add YouTube plain-lyrics and LRCLIB source boundaries

**Files:**
- Modify: `src/provider/model.rs`
- Modify: `src/provider/ytmusic.rs`
- Modify: `src/provider/mod.rs`
- Modify: `src/lyrics.rs`
- Test: `tests/provider_adapter.rs`
- Test: `src/lyrics.rs` (unit-test module)

**Step 1: Write failing provider tests**

Extend the provider contract with a bounded plain-lyrics operation keyed by `MediaId`. Test that valid video IDs call `GetLyricsIDQuery` then `GetLyricsQuery`, unavailable lyrics return a typed unavailable result, invalid IDs fail before dispatch, and returned text is bounded before entering app state.

**Step 2: Write failing LRCLIB transport/cache tests**

Introduce injected transport, clock, and locale-independent metadata request fixtures. Prove:

- HTTPS `/api/search` uses encoded query parameters and required identifying User-Agent;
- redirects, oversized bodies, non-success status, and malformed JSON return safe typed errors;
- successful results are cached by media ID plus metadata fingerprint;
- cache is memory-only, bounded, and expires conservatively;
- concurrent/repeated requests do not hold a mutex across await;
- `external_sync = false` makes no LRCLIB call.

**Step 3: Verify RED**

Run: `cargo test --test provider_adapter lyrics`

Run: `cargo test lyrics::tests::source --lib`

Expected: FAIL because provider/source methods are absent.

**Step 4: Implement source boundaries**

Add `MusicProvider::lyrics(&MediaId)` and implement it in `YtMusicProvider` and all test doubles. Add `LyricsSourceService` that requests YouTube plain lyrics and optionally LRCLIB synchronized results, combines them according to the approved precedence, and identifies the application as required by LRCLIB. Do not persist responses.

**Step 5: Verify GREEN and commit**

Run: `cargo test --test provider_adapter lyrics`

Run: `cargo test lyrics::tests --lib`

Run: `cargo clippy --all-targets --all-features -- -D warnings`

Commit:

```bash
git add src/provider/model.rs src/provider/ytmusic.rs src/provider/mod.rs src/lyrics.rs tests/provider_adapter.rs
git commit -m "feat: retrieve plain and synchronized lyrics"
```

### Task 4: Model and dispatch generation-safe lyrics lifecycle

**Files:**
- Modify: `src/app/action.rs`
- Modify: `src/app/effect.rs`
- Modify: `src/app/state.rs`
- Modify: `src/app/reducer.rs`
- Modify: `src/app/mod.rs`
- Modify: `src/runtime.rs`
- Modify: `src/cli.rs`
- Test: `tests/reducer.rs`
- Test: `tests/runtime.rs`

**Step 1: Write failing reducer tests**

Prove that starting a Song or Video allocates a lyric generation and emits `LoadLyrics`, while podcast episodes and disabled lyrics emit nothing. Completion must require matching generation/media ID. Replacement, stop, and resolve failure invalidate old work. `PlayerProgress` selects the correct active line, including backwards seek. Lyrics failure leaves playback untouched.

**Step 2: Write failing runtime tests**

Inject a fake lyrics service and prove replacement cancels old work, stale completion is ignored, service failure is safe, and shutdown cancels the lyric task. Assert no action/debug output contains lyric content.

**Step 3: Verify RED**

Run: `cargo test --test reducer lyrics`

Run: `cargo test --test runtime lyrics`

Expected: FAIL because lyric state/actions/effects are absent.

**Step 4: Implement lifecycle**

Add `LyricsRequested`, `LyricsCompleted`, and clear/invalidate transitions plus `Effect::LoadLyrics`. Store loading/error/document/active-line identity in a dedicated `LyricsState`; compute active line from authoritative playback position in the reducer. Inject the production source through `RuntimeServices`, add one replaceable lyric task, and include it in shutdown cleanup.

**Step 5: Verify GREEN and commit**

Run: `cargo test --test reducer lyrics`

Run: `cargo test --test runtime lyrics`

Commit:

```bash
git add src/app/action.rs src/app/effect.rs src/app/state.rs src/app/reducer.rs src/app/mod.rs src/runtime.rs src/cli.rs tests/reducer.rs tests/runtime.rs
git commit -m "feat: synchronize lyrics with playback state"
```

### Task 5: Render automatic lyrics and the full lyrics overlay

**Files:**
- Modify: `src/ui/input.rs`
- Modify: `src/ui/controller.rs`
- Modify: `src/ui/render.rs`
- Modify: `src/ui/views/settings.rs`
- Test: `tests/ui.rs`
- Test: `tests/ui_controller.rs`
- Test: `src/ui/render.rs` (unit-test module)

**Step 1: Write failing input/controller tests**

Map plain `L` in normal mode to `ToggleLyrics`. Test open/close, Esc close, overlay suppression of background actions, `j/k` manual scrolling, Enter recenter/follow, and track change resetting follow state. Preserve text-entry behavior so typed `l` remains text.

**Step 2: Write failing renderer tests**

Test wide previous/current/next lines, compact current line, tiny clipping/omission, theme accent on current line, plain lyrics only in overlay, instrumental/unavailable/loading states, manual viewport stability while highlight advances, recenter, and terminal resize.

**Step 3: Verify RED**

Run: `cargo test --test ui lyrics`

Run: `cargo test --test ui_controller lyrics`

Run: `cargo test ui::render::tests::lyrics --lib`

Expected: FAIL because lyrics UI semantics are absent.

**Step 4: Implement UI behavior**

Add a lyrics overlay and controller state `{ follow_active, selected_line/scroll }`. Normal player rendering automatically includes synchronized lines only. Full overlay wraps bounded plain lines or displays timed rows, highlights active timing, and retains manual scrolling until Enter. Settings reports whether lyrics, external sync, and animation are enabled.

**Step 5: Verify GREEN and commit**

Run: `cargo test --test ui`

Run: `cargo test --test ui_controller`

Run: `cargo test ui::render::tests --lib`

Commit:

```bash
git add src/ui/input.rs src/ui/controller.rs src/ui/render.rs src/ui/views/settings.rs tests/ui.rs tests/ui_controller.rs
git commit -m "feat: add followed and full lyrics views"
```

### Task 6: Resolve an optional low-resolution video preview

**Files:**
- Modify: `src/resolver/mod.rs`
- Modify: `src/resolver/ytdlp.rs`
- Modify: `src/player/supervisor.rs`
- Test: `tests/resolver.rs`
- Test: `tests/player.rs`

**Step 1: Write failing resolver tests**

Extend fixtures with audio and video formats. Prove audio selection is unchanged, an optional bounded low-resolution video-only HTTPS URL is selected for Video media, absent/invalid preview data still returns successful audio, and Debug/Display redact both URLs.

**Step 2: Write failing supervisor tests**

Prove mpv receives only the audio URL and existing resolution/fade behavior is unchanged. Expose the optional preview only through a separate generation-scoped presentation action; never add it to queue/session persistence.

**Step 3: Verify RED**

Run: `cargo test --test resolver preview`

Run: `cargo test --test player preview`

Expected: FAIL because `ResolvedStream` lacks preview metadata.

**Step 4: Implement optional preview selection**

Add an opaque/redacted `PreviewStreamUrl` and `preview_url: Option<PreviewStreamUrl>` to `ResolvedStream`. Parse it from the same bounded yt-dlp JSON response where possible, choosing a conservative low-resolution video-only format. Propagate it separately while keeping mpv load parameters identical.

**Step 5: Verify GREEN and commit**

Run: `cargo test --test resolver`

Run: `cargo test --test player`

Commit:

```bash
git add src/resolver/mod.rs src/resolver/ytdlp.rs src/player/supervisor.rs tests/resolver.rs tests/player.rs
git commit -m "feat: resolve optional animated artwork preview"
```

### Task 7: Decode bounded animation frames without backpressure

**Files:**
- Create: `src/ui/animation.rs`
- Modify: `src/ui/mod.rs`
- Modify: `src/ui/artwork.rs`
- Modify: `src/runtime.rs`
- Modify: `src/cli.rs`
- Test: `src/ui/animation.rs` (unit-test module)
- Test: `tests/runtime.rs`
- Test: `tests/ui_artwork_safety.rs`

**Step 1: Write failing pure frame-store tests**

Test capacity-one replacement, generation/media matching, output dimension caps, empty frame rejection, pause/resume state, stale generation rejection, and static fallback after failure.

**Step 2: Write failing worker/runtime tests**

Use fake decoder output and fake clocks to prove max-FPS pacing without real sleeps, newest-frame wins under a slow renderer, replacement cancels old decoding, pause prevents publication, resume continues, compact/tiny layouts do not start work, and shutdown reaps the worker.

**Step 3: Verify RED**

Run: `cargo test ui::animation::tests --lib`

Run: `cargo test --test runtime animation`

Run: `cargo test --test ui_artwork_safety animation`

Expected: FAIL because animation boundary/store do not exist.

**Step 4: Implement the bounded worker**

Define an injected `AnimationDecoder` contract and production FFmpeg process wrapper with direct argv, no shell, strict process/output limits, cancellation, and stderr sanitization. Publish `ArtworkGrid` through a latest-frame store keyed by playback generation and target `CellSize`. Reuse existing terminal-grid rendering; never write frame files.

Runtime starts animation only when enabled, layout-capable, actively playing Video media has a preview URL, and FFmpeg is available. Every error clears animated presentation and leaves static artwork untouched.

**Step 5: Verify GREEN and commit**

Run: `cargo test ui::animation::tests --lib`

Run: `cargo test --test runtime animation`

Run: `cargo test --test ui_artwork_safety`

Run: `cargo clippy --all-targets --all-features -- -D warnings`

Commit:

```bash
git add src/ui/animation.rs src/ui/mod.rs src/ui/artwork.rs src/runtime.rs src/cli.rs tests/runtime.rs tests/ui_artwork_safety.rs
git commit -m "feat: render bounded video-backed artwork animation"
```

### Task 8: Integrate presentation, documentation, and live smoke tests

**Files:**
- Modify: `src/ui/render.rs`
- Modify: `README.md`
- Modify: `tests/docs.rs`
- Modify: `tests/live/provider_live.rs`

**Step 1: Add integration rendering tests**

Render consecutive animation frames in wide mode and prove static artwork remains in compact/tiny mode, paused state retains a stable frame, missing/failed preview uses existing fallback, and animation never covers lyrics/player controls.

**Step 2: Document user behavior and privacy**

Document automatic synchronized lyrics, `L` overlay controls, plain fallback, LRCLIB metadata disclosure and opt-out, animation requirements/fallback, new config keys, and performance caps. Update keyboard-reference tests.

**Step 3: Add ignored live tests**

Add independent feature-gated tests for YouTube plain lyrics, one conservatively matched LRCLIB synchronized result, and one decoded low-resolution preview frame. Each must be explicitly ignored and gated by `YTERMUSIC_LIVE_TESTS=1`; never print lyrics or stream URLs.

**Step 4: Verify focused integration**

Run: `cargo test --test docs`

Run: `cargo test --test ui`

Run: `cargo test --features live-tests --test provider_live --no-run`

Expected: PASS; live tests compile but remain unexecuted.

**Step 5: Commit**

```bash
git add src/ui/render.rs README.md tests/docs.rs tests/live/provider_live.rs
git commit -m "docs: explain lyrics and animated artwork"
```

### Task 9: Full review and verification

**Files:**
- Modify only files required by review findings.

**Step 1: Run offline gates**

Run: `cargo fmt --all -- --check`

Run: `cargo test --all-targets --all-features`

Run: `cargo clippy --all-targets --all-features -- -D warnings`

Run: `git diff --check`

Expected: all pass; only intentionally ignored live/fixture tests remain ignored.

**Step 2: Run live checks when network is available**

Run each new ignored test independently with `YTERMUSIC_LIVE_TESTS=1`. Record exact results; do not convert unavailable external services into silent passes and do not claim execution when only compilation ran.

**Step 3: Request final review**

Use @requesting-code-review for the complete lyrics/animation range. Review generation/cancellation safety, external metadata privacy, lyric matching, copyrighted-text logging, URL redaction, subprocess argv, resource limits, frame dropping, layout fallbacks, and unchanged audio behavior. Fix valid findings test-first and re-review until no Critical/Important issues remain.

**Step 4: Verify completion**

Use @verification-before-completion and capture fresh outputs for format, full tests, clippy, diff check, status, and any live checks actually executed.
