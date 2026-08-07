# Multilingual LRCLIB Fallback Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Retrieve synchronized LRCLIB lyrics for safely identifiable multilingual YouTube Music tracks such as `與我無關 - Not My Problem` without enabling general fuzzy matching.

**Architecture:** Keep the existing full-title/full-artist lookup and strict matcher unchanged as the primary path. On a clean no-match only, derive a bounded list of exact title segments, issue artistless LRCLIB searches sequentially, and evaluate each response with a separate duration-gated matcher whose unique best candidate may use an artist alias. Cache the final result under the original media metadata fingerprint.

**Tech Stack:** Rust 2024, Tokio, `url`, Serde streaming deserialization, existing LRCLIB transport/cache seams, Cargo test/Clippy.

---

### Task 1: Derive bounded multilingual title variants

**Files:**
- Modify: `src/lyrics.rs:29-36`
- Test: `src/lyrics.rs:1490-2860`

**Step 1: Write the failing tests**

Add focused unit tests for a private `fallback_title_variants` helper:

```rust
#[test]
fn fallback_title_variants_extract_bounded_exact_bilingual_segments() {
    assert_eq!(
        fallback_title_variants("與我無關 - Not My Problem"),
        vec!["與我無關", "Not My Problem"]
    );
    assert_eq!(
        fallback_title_variants("Original — Translation – Alternate"),
        vec!["Original", "Translation", "Alternate"]
    );
}

#[test]
fn fallback_title_variants_reject_empty_tiny_duplicate_and_excess_segments() {
    assert!(fallback_title_variants("Unsplittable title").is_empty());
    assert_eq!(fallback_title_variants("A - Valid - Valid"), vec!["Valid"]);
    assert!(fallback_title_variants(&"x".repeat(MAX_LYRICS_METADATA_BYTES + 1)).is_empty());
    assert!(fallback_title_variants("one - two - three - four").len()
        <= MAX_LRCLIB_FALLBACK_REQUESTS);
}
```

**Step 2: Run the tests to verify RED**

Run:

```bash
cargo test fallback_title_variants --all-features -- --nocapture
```

Expected: compilation fails because `fallback_title_variants` and `MAX_LRCLIB_FALLBACK_REQUESTS` do not exist.

**Step 3: Implement the minimal variant extractor**

Add a fixed cap and recognize only spaced separators, not arbitrary punctuation:

```rust
const MAX_LRCLIB_FALLBACK_REQUESTS: usize = 3;
const LRCLIB_TITLE_SEPARATORS: [&str; 3] = [" - ", " – ", " — "];

fn fallback_title_variants(title: &str) -> Vec<String> {
    if title.len() > MAX_LYRICS_METADATA_BYTES
        || !LRCLIB_TITLE_SEPARATORS.iter().any(|separator| title.contains(separator))
    {
        return Vec::new();
    }
    let mut variants = vec![title.trim().to_owned()];
    for separator in LRCLIB_TITLE_SEPARATORS {
        variants = variants
            .into_iter()
            .flat_map(|part| part.split(separator).map(str::to_owned).collect::<Vec<_>>())
            .collect();
    }
    let normalized_full = normalize_match_text(title);
    let mut accepted = Vec::new();
    for variant in variants {
        let variant = variant.trim();
        let Some(normalized) = normalize_match_text(variant) else { continue };
        if variant.chars().count() < 2
            || Some(&normalized) == normalized_full.as_ref()
            || accepted.iter().any(|prior: &String| normalize_match_text(prior).as_ref() == Some(&normalized))
        {
            continue;
        }
        accepted.push(variant.to_owned());
        if accepted.len() == MAX_LRCLIB_FALLBACK_REQUESTS { break; }
    }
    accepted
}
```

Refine the implementation if needed to keep allocations bounded before collection and satisfy strict Clippy; do not broaden the separator list.

**Step 4: Run the tests to verify GREEN**

Run:

```bash
cargo test fallback_title_variants --all-features -- --nocapture
```

Expected: both tests pass.

**Step 5: Commit**

```bash
git add src/lyrics.rs
git commit -m "feat: derive bounded LRCLIB title variants"
```

### Task 2: Add a conservative fallback response matcher

**Files:**
- Modify: `src/lyrics.rs:799-1120`
- Test: `src/lyrics.rs:2610-2860`

**Step 1: Write the failing track-9979227 regression test**

Construct a request from the YouTube metadata and a response from the supplied LRCLIB record:

```rust
#[test]
fn lrclib_fallback_accepts_unique_exact_segment_and_duration_with_artist_alias()
-> Result<(), Box<dyn Error>> {
    let request = LrclibMatchRequest::new(
        "與我無關 - Not My Problem",
        "MC Cheung Tinfu",
        &["MC Cheung Tinfu"],
        Some(205_000),
    );
    let response = br#"[{
        "id":9979227,
        "trackName":"與我無關",
        "artistName":"MC 張天賦",
        "albumName":"與我無關",
        "duration":205.0,
        "plainLyrics":"你 應該都不再認得 舊年",
        "syncedLyrics":"[00:17.49]你 應該都不再認得 舊年"
    }]"#;

    let document = match_lrclib_fallback_response(response, "與我無關", &request)?
        .ok_or("expected multilingual fallback")?;
    assert_eq!(document.source(), LyricsSource::Lrclib);
    assert_eq!(document.timed()[0].start_ms(), 17_490);
    Ok(())
}
```

Also add separate tests proving fallback rejects:

- a record whose normalized title is only a partial match;
- missing or more-than-two-second duration agreement;
- two distinct equal-ranked synchronized documents;
- oversized metadata, body, result count, or lyric text;
- a lower-metadata-confidence candidate when an exact-artist or exact-album candidate exists.

**Step 2: Run the tests to verify RED**

Run:

```bash
cargo test lrclib_fallback_ --all-features -- --nocapture
```

Expected: compilation fails because `match_lrclib_fallback_response` is absent.

**Step 3: Implement a separate fallback policy**

Do not weaken `match_lrclib_response`. Add a private policy used by the existing streaming response visitor:

```rust
enum LrclibMatchPolicy<'a> {
    Strict,
    ExactTitleFallback { title: &'a str },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct FallbackMetadataRank {
    artist_match: bool,
    album_match: bool,
}
```

`match_lrclib_fallback_response` must:

1. Apply `MAX_LYRICS_RESPONSE_BYTES` before deserialization.
2. Normalize the supplied variant and original request metadata.
3. Require candidate title equality with the exact normalized variant.
4. Require known request and candidate duration within `LRCLIB_DURATION_TOLERANCE_MS`.
5. Permit artist mismatch only in this fallback policy.
6. Rank exact artist, then exact album, then synchronized content, then closest duration.
7. Reuse `MatchAccumulator` ambiguity semantics so distinct equal-ranked documents fail closed.
8. Reuse all record/result/text limits and `parse_lrc` validation.

Use an explicit rank type or add policy-aware fields to `CandidateRank`; strict matching output and ordering must remain unchanged.

**Step 4: Run matcher tests to verify GREEN and strict compatibility**

Run:

```bash
cargo test lrclib_ --all-features -- --nocapture
```

Expected: all strict and fallback matcher tests pass.

**Step 5: Commit**

```bash
git add src/lyrics.rs
git commit -m "feat: match unique multilingual LRCLIB records"
```

### Task 3: Orchestrate strict-first fallback requests

**Files:**
- Modify: `src/lyrics.rs:320-375`
- Test: `src/lyrics.rs:1680-1935`

**Step 1: Add a sequenced transport test seam**

Extend the test transport or add a `SequencedSourceTransport` holding a mutex-protected `VecDeque<LrclibHttpResponse>`. It must record every request and return responses in order without logging URL queries or response bodies.

**Step 2: Write failing end-to-end client tests**

Add async tests for these behaviors:

```rust
#[tokio::test]
async fn source_falls_back_to_exact_title_segment_for_lrclib_9979227()
-> Result<(), Box<dyn Error>> {
    let transport = SequencedSourceTransport::new([
        LrclibHttpResponse::new(200, b"[]".to_vec(), false),
        LrclibHttpResponse::new(200, TRACK_9979227_RESPONSE.to_vec(), false),
    ]);
    let client = test_client(transport.clone());
    let mut item = source_item("與我無關 - Not My Problem");
    item.creators = vec!["MC Cheung Tinfu".to_owned()];
    item.duration_ms = Some(205_000);

    let document = client.fetch(&item).await?.ok_or("fallback lyrics")?;
    assert!(!document.timed().is_empty());
    let requests = transport.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].url().query_pairs().any(|(key, value)|
        key == "artist_name" && value == "MC Cheung Tinfu"));
    assert!(requests[1].url().query_pairs().any(|(key, value)|
        key == "track_name" && value == "與我無關"));
    assert!(!requests[1].url().query_pairs().any(|(key, _)| key == "artist_name"));
    Ok(())
}
```

Add separate tests proving:

- a strict match makes exactly one request;
- unsplittable titles make exactly one request;
- fallback attempts never exceed `MAX_LRCLIB_FALLBACK_REQUESTS` after strict search;
- an accepted fallback is cached and the second fetch makes no request;
- redirect, non-success, oversize, malformed, and transport failures retain payload-free errors and do not cascade into extra requests;
- a valid unmatched fallback response continues to the next variant, while an ambiguous or malformed response fails closed.

**Step 3: Run client tests to verify RED**

Run:

```bash
cargo test source_ --all-features -- --nocapture
```

Expected: the 9979227 test reports one request and no document.

**Step 4: Extract response validation and implement sequential fallback**

Refactor the duplicated response checks into a payload-free helper:

```rust
fn validated_lrclib_body(response: LrclibHttpResponse)
    -> Result<Vec<u8>, LrclibSourceError>;
```

Then structure `LrclibClient::fetch` as:

```rust
let strict_body = self.search(strict_url).await?;
let mut document = match_lrclib_response(&strict_body, &request)
    .map_err(|_| LrclibSourceError::InvalidResponse)?;
if document.is_none() {
    for variant in fallback_title_variants(&item.title) {
        let body = self.search(title_only_url(&variant)?).await?;
        document = match_lrclib_fallback_response(&body, &variant, &request)
            .map_err(|_| LrclibSourceError::InvalidResponse)?;
        if document.is_some() { break; }
    }
}
self.insert_cache(key, expires_at, document.clone());
Ok(document)
```

Keep searches sequential, preserve the cache lock boundary, use the same HTTPS base URL and identifying user-agent, and avoid retaining raw response data after parsing.

**Step 5: Run client and concurrency/cache tests to verify GREEN**

Run:

```bash
cargo test source_ --all-features -- --nocapture
```

Expected: all source, cache, response-hardening, and concurrency tests pass.

**Step 6: Commit**

```bash
git add src/lyrics.rs
git commit -m "feat: fetch multilingual LRCLIB fallbacks"
```

### Task 4: Document privacy behavior and add a gated live regression

**Files:**
- Modify: `README.md:210-225`
- Modify: `tests/docs.rs:760-790`
- Modify: `tests/live/provider_live.rs:205-240`

**Step 1: Write failing documentation and live-test assertions**

Update the docs test to require wording that external sync may send up to three exact title segments without artist metadata after the strict lookup fails. Add an ignored live test that searches YouTube Music for `MC Cheung Tinfu Not My Problem`, chooses the playable 205-second song, loads it through `LyricsSourceService`, and requires a synchronized LRCLIB document.

**Step 2: Run documentation tests to verify RED**

Run:

```bash
cargo test --test docs readme_explains_synchronized_lyrics_controls_sources_and_privacy -- --nocapture
```

Expected: failure because the README does not disclose fallback metadata requests.

**Step 3: Update README and live test**

Document:

- strict full-title/artist/album lookup remains first;
- on no match, up to three bounded exact title segments may be sent without artist metadata;
- duration is used locally to accept only a unique conservative match;
- `lyrics.external_sync = false` disables all LRCLIB requests.

Keep the live test ignored behind the existing explicit gate and avoid asserting exact lyric text.

**Step 4: Run documentation and compile-only live tests to verify GREEN**

Run:

```bash
cargo test --test docs readme_explains_synchronized_lyrics_controls_sources_and_privacy -- --nocapture
cargo test --test provider_live anonymous_lrclib_multilingual_fallback_returns_synchronized_lyrics --no-run
```

Expected: docs test passes and the live test compiles without running the network gate.

**Step 5: Commit**

```bash
git add README.md tests/docs.rs tests/live/provider_live.rs
git commit -m "docs: explain multilingual lyric fallback"
```

### Task 5: Final verification and review

**Files:**
- Review: `src/lyrics.rs`
- Review: `README.md`
- Review: `tests/docs.rs`
- Review: `tests/live/provider_live.rs`

**Step 1: Run formatting and static analysis**

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
git diff --check main...HEAD
```

Expected: all commands exit 0.

**Step 2: Run the complete suite**

```bash
cargo test --all-targets --all-features --quiet
```

Expected: all non-gated tests pass; only explicitly ignored live/fixture tests remain ignored.

**Step 3: Optionally run the live regression when the explicit gate and network are available**

Use the repository’s documented live-test environment gate and run only `anonymous_lrclib_multilingual_fallback_returns_synchronized_lyrics`. Record clearly whether this was run; absence of a live run must not be reported as a pass.

**Step 4: Request independent code review**

Use `superpowers:requesting-code-review` with the design commit as base. Resolve every Critical or Important finding and rerun affected tests.

**Step 5: Commit any review corrections**

```bash
git add src/lyrics.rs README.md tests/docs.rs tests/live/provider_live.rs
git commit -m "fix: address multilingual lyric review"
```
