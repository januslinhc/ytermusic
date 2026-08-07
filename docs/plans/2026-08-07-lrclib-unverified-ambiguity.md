# LRCLIB Unverified Fallback Ambiguity Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Prevent artistless LRCLIB fallbacks from choosing between conflicting lyric documents using duration proximity alone.

**Architecture:** Wrap the existing fallback rank accumulator with identity-confidence bookkeeping. Preserve normal ranking whenever an accepted candidate matches the requested artist or album; otherwise accept only one distinct unverified lyric document and fail closed when competing documents exist.

**Tech Stack:** Rust 2024, Tokio, Serde streaming deserialization, existing LRCLIB transport and matcher seams, Cargo test and Clippy.

---

### Task 1: Reproduce the Marcy false positive

**Files:**
- Modify: `src/lyrics.rs:1760-2050`
- Modify: `src/lyrics.rs:2310-2710`

**Step 1: Add a shared response fixture**

Inside `lyrics::tests`, add a string fixture containing both accepted
`ラブソング` records:

```rust
const MARCY_FALLBACK_RESPONSE: &str = r#"[
  {"trackName":"ラブソング","artistName":"OKAMOTO'S","albumName":"OKAMOTO'S","duration":257.114558,"plainLyrics":"wrong","syncedLyrics":"[00:06.03]wrong"},
  {"trackName":"ラブソング","artistName":"マルシィ","albumName":"Marcy -Sweet and Bitter-","duration":256.0,"plainLyrics":"right","syncedLyrics":"[00:18.50]right"}
]"#;
```

**Step 2: Add the failing matcher regression**

Run the fixture in both response orders and require ambiguity:

```rust
#[test]
fn lrclib_fallback_rejects_competing_unverified_documents_despite_duration_rank() {
    let request = LrclibMatchRequest::new(
        "ラブソング - Love Song",
        "Marcy",
        &["Marcy"],
        Some(257_000),
    )
    .with_collection(Some("Love Song"));

    assert_eq!(
        match_lrclib_fallback_response(
            MARCY_FALLBACK_RESPONSE.as_bytes(),
            "ラブソング",
            &request,
        ),
        Err(LyricsParseError::AmbiguousMatch),
    );
}
```

Also construct the reversed JSON array and require the same result, proving
response order cannot affect the safety decision.

**Step 3: Add the failing source-client regression**

Use `SequencedSourceTransport` with strict `[]`, full-title `[]`, and the Marcy
fixture for the Japanese title segment. Build an item with creator `Marcy`,
collection `Love Song`, and duration `257_000`. Require
`LrclibSourceError::InvalidResponse`, exactly three requests, and no request for
the later English title segment.

**Step 4: Preserve valid ranking-test intent**

In the existing synchronization-before-duration and closest-duration tests,
make both candidates use `artistName: "Request Artist"`. Those tests then prove
ranking within an identity-verified candidate set rather than endorsing a
duration-only identity decision.

**Step 5: Run tests to verify RED**

```bash
cargo test --lib lrclib_fallback_ --all-features -- --nocapture
cargo test --lib source_rejects_competing_unverified --all-features -- --nocapture
```

Expected: the new matcher returns the closer OKAMOTO'S document and the client
returns lyrics instead of `InvalidResponse`.

**Step 6: Commit the failing tests**

```bash
git add src/lyrics.rs
git commit -m "test: reproduce unverified LRCLIB duration false positive"
```

### Task 2: Reject conflicting unverified documents

**Files:**
- Modify: `src/lyrics.rs:1020-1170`

**Step 1: Add a specialized fallback accumulator**

```rust
struct FallbackMatchAccumulator {
    ranked: MatchAccumulator<FallbackCandidateRank>,
    identity_verified: bool,
    first_unverified: Option<LyricsDocument>,
    conflicting_unverified: bool,
}

impl Default for FallbackMatchAccumulator {
    fn default() -> Self {
        Self {
            ranked: MatchAccumulator::default(),
            identity_verified: false,
            first_unverified: None,
            conflicting_unverified: false,
        }
    }
}

impl FallbackMatchAccumulator {
    fn consider(&mut self, rank: FallbackCandidateRank, document: LyricsDocument) {
        if rank.artist_match || rank.album_match {
            self.identity_verified = true;
        } else if let Some(first) = &self.first_unverified {
            self.conflicting_unverified |= first != &document;
        } else {
            self.first_unverified = Some(document.clone());
        }
        self.ranked.consider(rank, document);
    }

    fn finish(self) -> Result<Option<LyricsDocument>, LyricsParseError> {
        if !self.identity_verified && self.conflicting_unverified {
            return Err(LyricsParseError::AmbiguousMatch);
        }
        self.ranked.finish()
    }
}
```

**Step 2: Wire only fallback deserialization through it**

Change `LrclibFallbackResponseSeed::Value` and
`LrclibFallbackResponseVisitor::Value` to `FallbackMatchAccumulator`. In the
fallback visitor, retain result-count and failure bookkeeping through
`accumulator.ranked`, while sending accepted `(rank, document)` pairs through
`accumulator.consider`. Leave the strict visitor and `MatchAccumulator`
unchanged.

**Step 3: Run focused tests to verify GREEN**

```bash
cargo test --lib lrclib_fallback_ --all-features -- --nocapture
cargo test --lib source_ --all-features -- --nocapture
```

Expected: all fallback matcher and source orchestration tests pass, including
the Marcy regression, unique alias fallback, identical-document collapse, and
verified ranking tests.

**Step 4: Run lyric integration regressions**

```bash
cargo test lyrics --all-features -- --nocapture
cargo test --test workflows --all-features -- --nocapture
```

Expected: all selected tests pass.

**Step 5: Commit the implementation**

```bash
git add src/lyrics.rs
git commit -m "fix: reject unverified LRCLIB candidate conflicts"
```

### Task 3: Verify and review the complete correction

**Files:**
- Verify only

**Step 1: Check formatting**

```bash
cargo fmt --all -- --check
```

Expected: exit 0.

**Step 2: Run strict Clippy**

```bash
cargo clippy --all-targets --all-features -- -D warnings
```

Expected: exit 0 without warnings.

**Step 3: Run all tests**

```bash
cargo test --all-targets --all-features
```

Expected: every non-ignored test passes.

**Step 4: Check the patch and worktree**

```bash
git diff --check HEAD~2..HEAD
git status --short
```

Expected: no whitespace errors and a clean feature worktree.

**Step 5: Request independent code review**

Review the design and implementation range for false negatives, ambiguity
semantics, identical-document handling, source-service behavior, and test
coverage. Resolve all Critical or Important findings before branch completion.
