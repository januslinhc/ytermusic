# LRCLIB Full-Title Identity Verification Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Prevent artistless complete-title LRCLIB searches from returning lyrics unless an admitted candidate matches the requested artist or album.

**Architecture:** Add an explicit fallback identity policy at matcher completion while leaving candidate admission and ranking unchanged. The client uses the verified-only policy for its complete-title retry and the existing unique-unverified policy for derived multilingual title segments.

**Tech Stack:** Rust 2024, Tokio, Serde streaming deserialization, existing LRCLIB transport/matcher seams, Cargo test and Clippy.

---

### Task 1: Reproduce the Nancy/Sanchez false positive

**Files:**
- Modify: `src/lyrics.rs:1790-2140`
- Modify: `src/lyrics.rs:2430-2530`

**Step 1: Add the real response fixture**

Near the existing `MARCY_FALLBACK_RESPONSE`, add:

```rust
const NANCY_FALLBACK_RESPONSE: &str = r#"[{
  "id":35078027,
  "trackName":"How Could You",
  "artistName":"Sanchez",
  "albumName":"Stays on My Mind",
  "duration":238.0,
  "plainLyrics":"wrong",
  "syncedLyrics":"[00:47.52]Why couldn't you just realize"
}]"#;
```

This is the LRCLIB record displayed in the screenshot. The YouTube Music item
duration is 236 seconds, placing the unrelated record exactly on the existing
two-second boundary.

**Step 2: Add a failing verified-full-title matcher regression**

Add a private matcher entry point named
`match_lrclib_verified_fallback_response` to the test imports before it exists,
then add:

```rust
#[test]
fn lrclib_verified_full_title_rejects_unique_unverified_duration_match()
-> Result<(), Box<dyn Error>> {
    let request = LrclibMatchRequest::new(
        "How could you?",
        "Nancy Kwai",
        &["Nancy Kwai"],
        Some(236_000),
    );

    assert!(
        match_lrclib_verified_fallback_response(
            NANCY_FALLBACK_RESPONSE.as_bytes(),
            "How could you?",
            &request,
        )?
        .is_none()
    );
    Ok(())
}
```

The exact normalized title and duration still admit the Sanchez candidate, but
the verified-full-title policy must return no document because neither artist
nor album agrees.

**Step 3: Add a failing source-client regression**

Use `SequencedSourceTransport` with strict `[]` and the Nancy fixture for the
artistless full-title response. Build the exact item:

```rust
let mut item = source_item("How could you?");
item.id.video_id = "uY69HlDnkic".to_owned();
item.creators = vec!["Nancy Kwai".to_owned()];
item.collection = None;
item.duration_ms = Some(236_000);
```

Require `client.fetch(&item).await?` to be `None`, exactly two requests, and a
second fetch to remain `None` without another network request. This proves the
Sanchez document is neither returned nor cached as lyrics, while the final
no-match result retains normal caching.

**Step 4: Run tests to verify RED**

Run:

```bash
cargo fmt --all
cargo test --lib lrclib_verified_full_title_ --all-features -- --nocapture
cargo test --lib source_rejects_unverified_full_title_nancy_match --all-features -- --nocapture
```

Expected: compilation fails because
`match_lrclib_verified_fallback_response` does not exist. After adding only a
temporary delegating signature if necessary to isolate behavior, both tests
must fail because the Sanchez document is returned.

**Step 5: Commit the failing tests**

```bash
git add src/lyrics.rs
git commit -m "test: reproduce unverified full-title lyric match"
```

### Task 2: Require identity for complete-title fallback

**Files:**
- Modify: `src/lyrics.rs:345-368`
- Modify: `src/lyrics.rs:926-952`
- Modify: `src/lyrics.rs:1196-1222`

**Step 1: Add an explicit policy and shared matcher**

Add:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FallbackIdentityPolicy {
    AllowUniqueUnverified,
    RequireVerified,
}
```

Move the current fallback response body into a shared function that accepts a
policy. Keep `match_lrclib_fallback_response` as the segmented-policy wrapper,
and add the complete-title wrapper:

```rust
fn match_lrclib_fallback_response(
    response: &[u8],
    exact_title_variant: &str,
    request: &LrclibMatchRequest<'_>,
) -> Result<Option<LyricsDocument>, LyricsParseError> {
    match_lrclib_fallback_response_with_policy(
        response,
        exact_title_variant,
        request,
        FallbackIdentityPolicy::AllowUniqueUnverified,
    )
}

fn match_lrclib_verified_fallback_response(
    response: &[u8],
    exact_title: &str,
    request: &LrclibMatchRequest<'_>,
) -> Result<Option<LyricsDocument>, LyricsParseError> {
    match_lrclib_fallback_response_with_policy(
        response,
        exact_title,
        request,
        FallbackIdentityPolicy::RequireVerified,
    )
}
```

The shared function performs the existing bounded parse and calls
`accumulator.finish(policy)`.

**Step 2: Apply the policy at accumulator completion**

Change the specialized accumulator finish method to:

```rust
fn finish(
    self,
    policy: FallbackIdentityPolicy,
) -> Result<Option<LyricsDocument>, LyricsParseError> {
    if self.ranked.failure.is_some() {
        return self.ranked.finish();
    }
    if policy == FallbackIdentityPolicy::RequireVerified && !self.identity_verified {
        return Ok(None);
    }
    if !self.identity_verified && self.conflicting_unverified {
        return Err(LyricsParseError::AmbiguousMatch);
    }
    self.ranked.finish()
}
```

Failure classifications retain priority. A verified-only response with no
verified candidate is a clean no-match. The existing segmented policy retains
its ambiguity behavior.

**Step 3: Wire only the complete-title request to verified identity**

At `LrclibClient::fetch`, change only the first artistless retry:

```rust
document = match_lrclib_verified_fallback_response(
    &fallback_body,
    &item.title,
    &request,
)
.map_err(|_| LrclibSourceError::InvalidResponse)?;
```

Leave calls inside `fallback_title_variants` on
`match_lrclib_fallback_response`.

**Step 4: Add policy-boundary tests**

Add a test that passes an unverified candidate and an artist-verified candidate
to `match_lrclib_verified_fallback_response` in both response orders and proves
the verified document wins. Existing tests must continue proving:

- verified Goodnight Lillie full-title recovery;
- unique unverified `與我無關` segment recovery;
- Marcy conflicting-unverified ambiguity;
- identical unverified document collapse for segmented matching.

**Step 5: Run focused tests to verify GREEN**

Run:

```bash
cargo fmt --all
cargo test --lib lrclib_ --all-features -- --nocapture
cargo test --lib source_ --all-features -- --nocapture
```

Expected: all selected tests pass, including Nancy rejection, Goodnight Lillie
acceptance, and segmented multilingual behavior.

**Step 6: Commit the implementation**

```bash
git add src/lyrics.rs
git commit -m "fix: verify LRCLIB full-title fallback identity"
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
cargo test --all-targets --all-features --quiet
```

Expected: every non-ignored test passes.

**Step 4: Check the patch and worktree**

```bash
git diff --check 8cb16b9..HEAD
git status --short
```

Expected: no whitespace errors and a clean feature worktree.

**Step 5: Request independent code review**

Review the range from `8cb16b9` through `HEAD` against
`docs/plans/2026-08-07-lrclib-full-title-identity-design.md`. Resolve every
Critical or Important finding before branch completion.
