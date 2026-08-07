# Country Podcast Recommendations and Stable List Scrolling Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Show country-ranked podcast recommendations by default, lazily open them through YouTube Music, and make selection scrolling stable across every long TUI list.

**Architecture:** Add a bounded, cached Apple Top Shows boundary that returns discovery-only metadata and resolves `ZZ` from the OS locale. Extend the pure app reducer with independent recommendation and lazy-match generations, while the runtime performs external I/O and reuses `MusicProvider::search` plus the existing podcast-detail effect. Retain per-list viewport offsets inside the mutable terminal renderer so scrolling responds to actual layout dimensions without leaking terminal state into the app reducer.

**Tech Stack:** Rust 2024, Tokio, reqwest/rustls, serde/serde_json, sys-locale, ratatui, existing reducer/effect runtime, cargo test/clippy/fmt.

---

Implementation must use @test-driven-development for every behavior change, @systematic-debugging if an expected failure differs from the plan, @requesting-code-review before integration, and @verification-before-completion before any completion claim.

### Task 1: Define and parse bounded podcast-ranking data

**Files:**
- Create: `src/podcast_rankings.rs`
- Modify: `src/lib.rs`
- Test: `src/podcast_rankings.rs` (unit-test module)

**Step 1: Write the failing parser and model tests**

Add minimized Apple feed fixtures and tests for:

```rust
#[test]
fn parses_country_rank_title_publisher_and_https_artwork() {
    let page = parse_apple_top_shows(US_FIXTURE.as_bytes()).unwrap();
    assert_eq!(page.region(), &RegionCode::parse("US").unwrap());
    assert_eq!(page.items().len(), 2);
    assert_eq!(page.items()[0].rank(), 1);
    assert_eq!(page.items()[0].title(), "The Daily");
    assert_eq!(page.items()[0].publisher(), "The New York Times");
    assert!(page.items()[0].artwork_url().is_some());
}

#[test]
fn drops_invalid_rows_and_caps_results_at_twenty() { /* 25 rows + malformed rows */ }

#[test]
fn rejects_empty_invalid_and_oversized_feeds() { /* explicit error variants */ }

#[test]
fn recommendation_debug_redacts_source_identity() { /* no Apple ID in Debug */ }
```

Use fixtures with the real response shape:

```json
{"feed":{"country":"us","results":[{"artistName":"The New York Times","id":"1200361736","name":"The Daily","artworkUrl100":"https://example.test/art.jpg"}]}}
```

**Step 2: Run the tests to verify they fail**

Run: `cargo test podcast_rankings::tests --lib`

Expected: FAIL because `podcast_rankings` and its parser do not exist.

**Step 3: Implement the minimal bounded model and parser**

Add constants such as:

```rust
pub const MAX_PODCAST_RECOMMENDATIONS: usize = 20;
pub const MAX_PODCAST_FEED_BYTES: usize = 512 * 1024;
const MAX_SOURCE_ID_BYTES: usize = 128;
const MAX_PODCAST_TEXT_BYTES: usize = 512;
```

Implement opaque and bounded types with accessors:

```rust
#[derive(Clone, Eq, Hash, PartialEq)]
pub struct PodcastRecommendationId(String);

#[derive(Clone, PartialEq)]
pub struct PodcastRecommendation {
    source_id: PodcastRecommendationId,
    rank: usize,
    title: String,
    publisher: String,
    artwork_url: Option<ArtworkUrl>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PodcastRecommendationPage {
    region: RegionCode,
    items: Vec<PodcastRecommendation>,
}
```

Deserialize only `feed.country` and bounded `results` fields. Normalize country codes through `RegionCode::parse`, trim text, require non-empty ID/title, derive rank from retained feed order, accept artwork only through `ArtworkUrl::try_from`, truncate at 20, and return a typed `PodcastRankingError` without retaining raw response text. Implement a redacted `Debug` for source identities and recommendations.

Expose the module from `src/lib.rs` as `pub mod podcast_rankings;` so runtime and integration tests can use the boundary.

**Step 4: Run the focused tests**

Run: `cargo test podcast_rankings::tests --lib`

Expected: PASS, including malformed, size-limit, and redaction cases.

**Step 5: Commit**

```bash
git add src/lib.rs src/podcast_rankings.rs
git commit -m "feat: parse bounded country podcast rankings"
```

### Task 2: Add effective-country resolution, HTTP transport, and session cache

**Files:**
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Modify: `src/podcast_rankings.rs`
- Test: `src/podcast_rankings.rs` (unit-test module)

**Step 1: Write failing locale, URL, transport, and cache tests**

Cover these exact behaviors with fake locale, clock, and transport boundaries:

```rust
#[test]
fn configured_country_wins_over_locale() {
    assert_eq!(effective_region(&region("JP"), Some("en_US.UTF-8")), region("JP"));
}

#[test]
fn zz_uses_locale_country_then_us_fallback() {
    assert_eq!(effective_region(&region("ZZ"), Some("zh_HK")), region("HK"));
    assert_eq!(effective_region(&region("ZZ"), Some("C")), region("US"));
}

#[tokio::test]
async fn top_shows_uses_lowercase_country_url_and_one_hour_cache() {
    // Two fresh calls cause one transport request; advancing past the TTL causes two.
}

#[tokio::test]
async fn cache_is_bounded_and_transport_errors_are_secret_safe() { /* no body/URL query leak */ }
```

**Step 2: Run the focused tests to verify failure**

Run: `cargo test podcast_rankings::tests --lib`

Expected: FAIL because effective-country and source boundaries are missing.

**Step 3: Add the locale dependency and source abstractions**

Add `sys-locale = "0.3"` to dependencies. Define:

```rust
#[async_trait]
pub trait PodcastRankingSource: Send + Sync {
    async fn top_shows(
        &self,
        requested: &RegionCode,
    ) -> Result<PodcastRecommendationPage, PodcastRankingError>;
}

#[async_trait]
trait PodcastRankingTransport: Send + Sync {
    async fn get(&self, url: Url) -> Result<Vec<u8>, PodcastRankingError>;
}

trait PodcastRankingClock: Send + Sync {
    fn now_millis(&self) -> i64;
}
```

Implement `effective_region` for underscore and hyphen locales, accepting only a valid two-letter country subtag and falling back to US. Production construction obtains the locale through `sys_locale::get_locale()` once.

**Step 4: Implement bounded HTTP and caching**

Build only URLs of the form:

```text
https://rss.marketingtools.apple.com/api/v2/{cc}/podcasts/top/20/podcasts.json
```

Configure a dedicated reqwest client with connect and total timeouts, rustls, redirects disabled or tightly bounded, and streaming body collection that stops beyond `MAX_PODCAST_FEED_BYTES` even when `Content-Length` is absent. `ApplePodcastRankingSource` owns a short-lived mutex-protected cache keyed by effective `RegionCode`, with a one-hour TTL and a maximum of 16 countries; do not hold its lock across network awaits. Cache only successfully parsed pages.

Map every external failure to a small typed variant (`Unavailable`, `InvalidResponse`, `TooLarge`) whose display and debug output contain no response body or dynamic URL.

**Step 5: Run focused tests and lint the module**

Run: `cargo test podcast_rankings::tests --lib`

Expected: PASS.

Run: `cargo clippy --lib --all-features -- -D warnings`

Expected: PASS with no warnings.

**Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock src/podcast_rankings.rs
git commit -m "feat: fetch and cache regional podcast rankings"
```

### Task 3: Model recommendation lifecycle in the pure app reducer

**Files:**
- Modify: `src/app/action.rs`
- Modify: `src/app/effect.rs`
- Modify: `src/app/state.rs`
- Modify: `src/app/reducer.rs`
- Modify: `src/app/mod.rs`
- Test: `tests/reducer.rs`
- Test: `tests/workflows.rs`

**Step 1: Write failing reducer tests**

Add tests proving:

- `AppState::new` seeds the podcast requested region from `Config.region`.
- `PodcastRecommendationsRequested` allocates a generation, clears only recommendation errors, and emits one ranking effect.
- Completion accepts only the matching generation and requested region, selects the first item, and stores the effective country.
- A stale completion after changing JP to US is ignored.
- Selection changes only to an ID present in the current recommendations.
- Opening a recommendation emits a lazy resolve effect without dropping the list.
- A resolved strong match emits the existing `LoadPodcast`; a failed match preserves list and selection.
- `ClosePodcast` clears show/episode state and returns to recommendations.
- A country refresh does not close an already-open show.

Use assertions shaped like:

```rust
assert_eq!(effects, vec![Effect::LoadPodcastRecommendations {
    generation,
    region: jp.clone(),
}]);
assert_eq!(state.podcasts().selected_recommendation(), Some(&daily_id));
```

**Step 2: Run the reducer tests to verify failure**

Run: `cargo test --test reducer podcast_recommendation`

Expected: FAIL because the actions, effects, and state fields are absent.

**Step 3: Add state and message types**

Add these action families (using typed fields, not tuples):

```rust
PodcastRecommendationsRequested { region: RegionCode },
PodcastRecommendationsCompleted {
    generation: Generation,
    requested_region: RegionCode,
    result: Result<PodcastRecommendationPage, AppError>,
},
PodcastRecommendationSelectionChanged { id: PodcastRecommendationId },
OpenSelectedPodcastRecommendation,
PodcastRecommendationResolved {
    generation: Generation,
    result: Result<String, AppError>,
},
ClosePodcast,
```

Add corresponding effects:

```rust
LoadPodcastRecommendations { generation: Generation, region: RegionCode },
ResolvePodcastRecommendation {
    generation: Generation,
    title: String,
    publisher: String,
},
```

Extend `PodcastState` with separate requested/effective region, recommendations, selected recommendation, recommendation loading/generation/error, and match loading/generation/error fields. Keep existing show loading/generation fields independent so a recommendation refresh cannot invalidate an open show request.

**Step 4: Implement minimal reducer transitions**

Bound recommendations again at reducer ingress. Validate both generation and requested region before accepting completion. Preserve recommendation data on source or match error. On match success, allocate a fresh show generation and emit `Effect::LoadPodcast`; do not route through search-state selection. `ClosePodcast` clears the show, selected episode, show error, and pending progress only.

Add public read-only accessors required by controller and renderer; do not expose mutable collections.

**Step 5: Run reducer and workflow tests**

Run: `cargo test --test reducer podcast_recommendation`

Run: `cargo test --test workflows podcast_recommendation`

Expected: PASS.

**Step 6: Commit**

```bash
git add src/app/action.rs src/app/effect.rs src/app/state.rs src/app/reducer.rs src/app/mod.rs tests/reducer.rs tests/workflows.rs
git commit -m "feat: model podcast recommendation workflow"
```

### Task 4: Dispatch ranking fetches and lazy YouTube matches

**Files:**
- Modify: `src/runtime.rs`
- Modify: `src/cli.rs`
- Modify: `src/podcast_rankings.rs`
- Test: `tests/runtime.rs`
- Test: `src/podcast_rankings.rs` (unit-test module)

**Step 1: Write failing match-scoring tests**

Add a pure helper that accepts a recommendation and a bounded podcast search page. Test:

```rust
#[test]
fn exact_normalized_title_wins_and_publisher_breaks_ties() { /* punctuation/case normalized */ }

#[test]
fn weak_or_non_podcast_results_are_rejected() { /* returns None */ }

#[test]
fn provider_id_must_be_present_and_bounded() { /* rejects empty/oversized */ }
```

Normalization should trim, lowercase, collapse whitespace, and ignore common punctuation without fuzzy edit-distance matching. Require exact normalized title equality; use normalized publisher overlap only to choose among exact-title candidates.

**Step 2: Write failing runtime integration tests**

Inject fake `PodcastRankingSource` and `MusicProvider` values and prove:

- `LoadPodcastRecommendations` calls the source once and sends the correct completion action.
- Replacing JP with US cancels or supersedes the prior ranking task.
- `ResolvePodcastRecommendation` calls `search(query, SearchFilter::Podcasts)` with bounded title/publisher text.
- Exact match returns a provider ID; no match returns the safe unavailable error.
- Shutdown cancels both new tasks.

**Step 3: Run tests to verify failure**

Run: `cargo test podcast_rankings::tests::exact_normalized --lib`

Run: `cargo test --test runtime podcast_recommendation`

Expected: FAIL because matching and runtime dispatch are missing.

**Step 4: Extend runtime service injection and dispatcher**

Add `podcast_rankings: Option<Arc<dyn PodcastRankingSource>>` to `RuntimeServices`, a `with_podcast_rankings` builder, and matching field in `EffectDispatcher`. Add two independent replaceable task slots:

```rust
podcast_recommendations_task: Option<ReplaceableTask>,
podcast_match_task: Option<ReplaceableTask>,
```

Map source errors to `AppErrorCategory::Podcast` with concise messages. For lazy resolution, bound the query before calling the existing provider, use `SearchFilter::Podcasts`, evaluate only the bounded returned page with the pure matcher, and send `PodcastRecommendationResolved`.

Ensure both task slots join the existing shutdown cleanup list.

**Step 5: Construct the production source**

In `src/cli.rs`, create the hardened ranking HTTP client/source and inject it with `with_podcast_rankings`. Ranking-source initialization failure should not abort music browsing; omit the boundary and allow the reducer/runtime to show the normal unavailable message.

**Step 6: Run focused tests**

Run: `cargo test podcast_rankings::tests --lib`

Run: `cargo test --test runtime podcast_recommendation`

Expected: PASS.

**Step 7: Commit**

```bash
git add src/runtime.rs src/cli.rs src/podcast_rankings.rs tests/runtime.rs
git commit -m "feat: resolve ranked podcasts through youtube music"
```

### Task 5: Wire country changes and podcast keyboard navigation

**Files:**
- Modify: `src/ui/controller.rs`
- Test: `tests/ui_controller.rs`

**Step 1: Write failing controller tests**

Cover:

- Activating Podcasts requests recommendations only when no list/request/show exists.
- Up and Down select recommendation IDs when no show is open, but episode IDs when a show is open.
- Enter on a recommendation emits `OpenSelectedPodcastRecommendation`.
- Enter on an episode keeps emitting `PlayPodcastEpisode`.
- Escape from an open show emits `ClosePodcast` and does not exit the app.
- Country-picker submit emits both `ChartsRequested` and `PodcastRecommendationsRequested` with the same region in deterministic order.
- Manual podcast search still emits `OpenSelectedPodcast`.

**Step 2: Run tests to verify failure**

Run: `cargo test --test ui_controller podcast_recommendation`

Expected: FAIL because recommendation navigation is not wired.

**Step 3: Implement controller dispatch**

Teach `activate_navigation`, `move_content_selection`, `submit`, and cancel handling to distinguish recommendation-list and opened-show modes. Keep country selection atomic at the UI boundary by returning:

```rust
vec![
    Action::ChartsRequested { region: region.clone() },
    Action::PodcastRecommendationsRequested { region },
]
```

Do not change global search filter or query state when opening a recommendation.

**Step 4: Run controller and input regressions**

Run: `cargo test --test ui_controller`

Run: `cargo test --test ui`

Expected: PASS.

**Step 5: Commit**

```bash
git add src/ui/controller.rs tests/ui_controller.rs
git commit -m "feat: navigate country podcast recommendations"
```

### Task 6: Render the default ranked podcast experience

**Files:**
- Modify: `src/ui/views/podcasts.rs`
- Modify: `src/ui/render.rs`
- Test: `src/ui/render.rs` (unit-test module)
- Test: `tests/ui.rs`

**Step 1: Write failing rendering tests**

Add snapshot/string assertions for:

- Initial state: `Top podcasts in JP` plus loading status after request.
- Loaded rows: `▶ 1. <title>  ·  <publisher>` and subsequent ranks.
- Lazy state: `… Finding on YouTube Music` while list remains visible.
- Source failure: concise error plus `Press / to search podcasts`.
- Match failure: unavailable message plus unchanged selected row.
- Opened show and episodes retain their current presentation.

**Step 2: Run tests to verify failure**

Run: `cargo test ui::render::tests::podcast --lib`

Run: `cargo test --test ui podcast`

Expected: FAIL because the default recommendation presentation is absent.

**Step 3: Implement the recommendation list renderer**

Make `podcasts::lines` branch in this order:

1. Open show and episodes.
2. Recommendation heading/list, retaining rows during match loading/error.
3. Recommendation loading/error.
4. Manual-search hint.

Use `bounded_format_cells` for all external text and preserve the row cap. The effective region labels loaded content; the requested region labels only the loading state. Do not render Apple links or IDs.

**Step 4: Run rendering tests**

Run: `cargo test ui::render::tests::podcast --lib`

Run: `cargo test --test ui podcast`

Expected: PASS.

**Step 5: Commit**

```bash
git add src/ui/views/podcasts.rs src/ui/render.rs tests/ui.rs
git commit -m "feat: render ranked podcasts by country"
```

### Task 7: Replace selection-pinned rendering with persistent viewports

**Files:**
- Modify: `src/ui/render.rs`
- Modify: `src/ui/views/search.rs`
- Modify: `src/ui/views/charts.rs`
- Modify: `src/ui/views/podcasts.rs`
- Modify: `src/ui/views/library.rs`
- Modify: `src/ui/views/history.rs`
- Modify: `src/ui/views/queue.rs`
- Test: `src/ui/render.rs` (unit-test module)
- Test: `tests/ui_controller.rs`

**Step 1: Capture the current bug with a failing viewport test**

Replace the stateless expectation with a stateful sequence:

```rust
#[test]
fn moving_up_inside_visible_window_does_not_move_the_page() {
    let mut viewport = SelectionViewport::default();
    assert_eq!(viewport.visible_range(20, Some(10), 5, 1), 6..11);
    assert_eq!(viewport.visible_range(20, Some(9), 5, 1), 6..11);
    assert_eq!(viewport.visible_range(20, Some(5), 5, 1), 5..10);
}
```

Also test moving down, wraparound, zero rows, selection outside bounds, dataset-key change, item-count shrink, and terminal-height growth/shrink.

**Step 2: Run the focused viewport test to verify failure**

Run: `cargo test ui::render::tests::moving_up_inside_visible_window_does_not_move_the_page --lib -- --exact`

Expected: FAIL with the current `selection_viewport` behavior (`5..10` appears one key too early).

**Step 3: Implement stateful viewport primitives**

Replace `selection_viewport` with a small state object:

```rust
#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct SelectionViewport {
    start: usize,
    dataset_key: u64,
}

impl SelectionViewport {
    fn visible_range(
        &mut self,
        total: usize,
        selected: Option<usize>,
        max_rows: usize,
        dataset_key: u64,
    ) -> Range<usize> { /* clamp, then scroll only across edges */ }
}
```

On dataset-key change, reset start to zero before making the selected row visible. Clamp start to `total.saturating_sub(max_rows)`. If selected is above start, set start to selected; if `selected >= start + max_rows`, set start to `selected + 1 - max_rows`. Return `0..0` for empty/zero-height input.

Add `ViewportMemory` with independent slots for search, charts, podcast recommendations, podcast episodes, library, history, queue, country picker, and browser picker. Compute bounded dataset keys from existing generations/regions and stable item IDs; never hash titles, descriptions, or provider payloads.

**Step 4: Thread viewport memory through rendering**

Add `viewports: ViewportMemory` to `TuiRenderer`. Split mutable borrows when calling `Terminal::draw` and pass memory through `render_with_model_inner`, content/queue/overlay helpers, and list view functions. Keep the public deterministic `render` and `render_with_model` helpers by creating local default memory for a single frame; add a test-only/internal mutable render helper for multi-frame assertions.

Each view receives its already selected viewport slot or visible range. Preserve headings by calculating available list rows before asking for the range. The command palette keeps its existing controller-owned scroll logic; country and browser pickers use renderer-owned slots.

**Step 5: Prove the fix across surfaces**

Add table-driven tests that feed selection sequences through Search, Charts, Podcasts, Library, History, Queue, country picker, and browser picker and assert that the same visible first row remains until a boundary is crossed. Verify the selected marker remains visible after wrapping and resize.

Run: `cargo test ui::render::tests --lib`

Run: `cargo test --test ui_controller`

Expected: PASS with stable visible rows in both directions.

**Step 6: Commit**

```bash
git add src/ui/render.rs src/ui/views/search.rs src/ui/views/charts.rs src/ui/views/podcasts.rs src/ui/views/library.rs src/ui/views/history.rs src/ui/views/queue.rs tests/ui_controller.rs
git commit -m "fix: keep list viewports stable during selection"
```

### Task 8: Add live smoke coverage and user documentation

**Files:**
- Modify: `tests/live/provider_live.rs`
- Modify: `README.md`

**Step 1: Add an ignored live ranking smoke test**

Under the existing `live-tests` feature, add an ignored test that fetches US, JP, and HK sequentially, asserting only bounded invariants: effective region matches, 1-20 non-empty shows, ranks are ordered, and no private/raw payload is printed. Do not require every recommended show to exist on YouTube Music because catalog availability varies.

**Step 2: Update documentation**

Document:

- Podcasts opens with country Top Shows when no show is open.
- `c` changes the country for both Charts and Podcasts.
- `ZZ` uses the OS locale and falls back to US.
- Recommendations originate from Apple's public rankings but playback/search stays on YouTube Music.
- Enter lazily matches a show; `/` remains manual search; Escape returns from episodes to recommendations.
- Rankings are cached only for the current session.

**Step 3: Run documentation and live-test compilation checks**

Run: `cargo test --features live-tests --test provider_live --no-run`

Expected: PASS and produce the live-test executable without making a network request.

Run when network is available:

```bash
YTERMUSIC_LIVE_TESTS=1 cargo test --features live-tests --test provider_live anonymous_country_podcast_rankings_are_bounded -- --ignored --exact
```

Expected: PASS for US, JP, and HK. If external availability prevents the live test, record the precise environment failure and retain fixture-test evidence; do not weaken assertions silently.

**Step 4: Commit**

```bash
git add tests/live/provider_live.rs README.md
git commit -m "docs: explain country podcast discovery"
```

### Task 9: Full regression, review, and integration readiness

**Files:**
- Modify only files required by review findings.

**Step 1: Format and inspect the diff**

Run: `cargo fmt --all -- --check`

Expected: PASS. If it fails, run `cargo fmt --all`, inspect the formatting-only diff, and recommit it with the task that introduced it.

Run: `git diff --check`

Run: `git status --short`

Expected: no whitespace errors; only the user's pre-existing `.DS_Store` may remain untracked.

**Step 2: Run the complete verification suite**

Run: `cargo test --all-targets --all-features`

Expected: PASS for all unit, integration, and feature-gated compiled tests; ignored live tests remain ignored.

Run: `cargo clippy --all-targets --all-features -- -D warnings`

Expected: PASS with no warnings.

Run: `cargo fmt --all -- --check`

Expected: PASS.

**Step 3: Request code review**

Use @requesting-code-review. Review specifically for:

- External-response size and text bounds.
- Secret-safe errors and redacted debug output.
- No Apple playback URL or accidental provider coupling.
- Generation correctness under rapid country changes.
- Exact-title match safety.
- Cache lock not held across awaits.
- Persistent viewport behavior after selection wrap, dataset replacement, and resize.

Apply valid findings with new failing regression tests first, then rerun the focused and full suites.

**Step 4: Verify completion evidence**

Use @verification-before-completion and record fresh output for the full test, clippy, format, and diff checks. Do not claim the live HTTP smoke test passed unless it was actually executed in this session.

**Step 5: Final commit if review caused changes**

```bash
git add <reviewed-files>
git commit -m "fix: address podcast discovery review"
```

Then use @finishing-a-development-branch to offer merge, PR, keep-worktree, or discard choices without modifying the user's unrelated `.DS_Store`.
