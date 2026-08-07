# Regional Chart Playlist Compatibility Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Make current JP and other regional chart responses resolve their chart playlist cards into playable tracks while preserving legacy direct-song charts.

**Architecture:** Add a bounded parser for chart-playlist references and let `ChartsQueryOutput` carry either already-playable sections or references. The real adapter will resolve references sequentially through its existing playlist-track request, retaining successful non-empty sections while the existing top-level charts timeout bounds the entire operation.

**Tech Stack:** Rust 2024, serde_json, ytmapi-rs, Tokio, existing provider parser and adapter test suites

---

### Task 1: Parse the new chart-playlist response shape

**Files:**
- Create: `tests/fixtures/charts_playlist_carousel_jp.json`
- Modify: `src/provider/charts.rs`
- Modify: `src/provider/queries.rs`

**Step 1: Write the failing fixture test**

Create a minimized response with a country `musicShelfRenderer`, an album `musicTwoRowItemRenderer` whose browse ID starts with `MPRE`, a `Trending 20 Japan` two-row item whose title-run browse ID is `VLJP_CHART_FIXTURE`, and an artist responsive item. In `src/provider/queries.rs`, add a unit test that normalizes the fixture and asserts:

```rust
let (sections, references) = output.into_parts();
assert!(sections.is_empty());
assert_eq!(references.len(), 1);
assert_eq!(references[0].title(), "Trending 20 Japan");
assert_eq!(references[0].playlist_id(), "JP_CHART_FIXTURE");
```

Also retain the existing legacy fixture test and assert that it produces sections with no references.

**Step 2: Run the focused test and verify RED**

Run: `cargo test --lib provider::queries::tests::processed_jp_chart_playlist_is_recognized -- --exact`

Expected: FAIL because `ChartsQueryOutput` cannot represent playlist references and the current parser rejects the fixture.

**Step 3: Implement bounded reference parsing**

In `src/provider/charts.rs`, add a crate-visible `ChartPlaylistReference` and `parse_chart_playlist_references`. Reuse `parse_document`, `section_list`, `MAX_SECTIONS`, and `MAX_ITEMS_PER_SHELF`. Inspect only carousel contents; accept only `musicTwoRowItemRenderer.title.runs` entries with a non-empty text title and a `browseEndpoint.browseId` beginning with exact `VL`. Strip that prefix, reject empty, whitespace/control-containing, or over-512-byte identifiers, ignore all unrelated cards, and report `UnusableResponse` when no usable reference exists.

In `src/provider/queries.rs`, change the output to:

```rust
pub struct ChartsQueryOutput {
    sections: Vec<ChartSection>,
    playlist_references: Vec<ChartPlaylistReference>,
}
```

Try the legacy playable parser first; only on `UnusableResponse` parse playlist references. Keep all errors redacted and make `Debug` print counts only.

**Step 4: Run focused and legacy parser tests and verify GREEN**

Run: `cargo test --lib provider::queries::tests -- --nocapture`

Expected: all query normalization tests PASS, including the new JP-shaped fixture and the existing direct-song fixture.

**Step 5: Commit**

```bash
git add tests/fixtures/charts_playlist_carousel_jp.json src/provider/charts.rs src/provider/queries.rs
git commit -m "fix: recognize regional chart playlists"
```

### Task 2: Hydrate chart playlists into playable sections

**Files:**
- Modify: `src/provider/ytmusic.rs`

**Step 1: Write failing adapter-resolution tests**

Add Tokio unit tests around a small internal `resolve_chart_output` helper. Supply an output containing two references and an async loader closure. Assert that the helper calls the loader with stripped playlist IDs in response order and returns `ChartSection` values with the reference titles and playable items. Add cases proving legacy sections bypass the loader and that one failed reference does not hide a later successful reference.

**Step 2: Run focused tests and verify RED**

Run: `cargo test --lib provider::ytmusic::normalization_tests::chart -- --nocapture`

Expected: FAIL because the resolver does not exist and real charts return only `into_sections()`.

**Step 3: Implement sequential hydration**

Add an internal async resolver accepting an owned-ID async loader. Return legacy sections immediately. Otherwise load each bounded bare playlist ID through `GetWatchPlaylistQuery::new_from_playlist_id`, retain successful non-empty playlists as `ChartSection::new(reference.title, items)`, remember the first error, and return that error only if nothing succeeds; if every load is empty, return a redacted charts `InvalidResponse`.

Change `RealYtMusicApi::charts` to pass the parsed output to the resolver and use its existing `playlist` method as the loader. This remains inside `YtMusicProvider::charts`' existing 30-second operation timeout.

**Step 4: Run focused provider tests and verify GREEN**

Run: `cargo test --lib provider::ytmusic::normalization_tests -- --nocapture`

Expected: all normalization and new chart-resolution tests PASS.

**Step 5: Commit**

```bash
git add src/provider/ytmusic.rs
git commit -m "fix: hydrate regional chart playlists"
```

### Task 3: Regression and quality verification

**Files:**
- Modify only if a regression reveals a defect in the files above.

**Step 1: Run provider regression suites**

Run: `cargo test --test provider_fixtures --test provider_hardening --test provider_adapter`

Expected: all tests PASS, including legacy chart normalization, parser resource limits, query shape, and adapter behavior.

**Step 2: Run formatting and lint checks**

Run: `cargo fmt --all -- --check`

Expected: exit 0.

Run: `cargo clippy --all-targets --all-features -- -D warnings`

Expected: exit 0 with no warnings.

**Step 3: Run the complete test suite**

Run: `cargo test --all-targets --all-features`

Expected: all tests PASS.

**Step 4: Review the diff for safety and scope**

Run: `git diff HEAD~2 --check && git status --short`

Expected: no whitespace errors and no unrelated changes. Verify no raw response data or playlist contents appear in `Debug` or errors.

**Step 5: Commit any verification-only correction**

If verification required a source correction, commit only that focused correction after rerunning its failing test. Otherwise make no empty commit.
