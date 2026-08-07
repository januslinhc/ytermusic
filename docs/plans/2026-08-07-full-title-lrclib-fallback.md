# Full-Title LRCLIB Fallback Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Retrieve a uniquely identifiable LRCLIB record when the complete title and duration agree but provider and LRCLIB artist names are translated aliases.

**Architecture:** Keep the strict metadata search and matcher unchanged. After a clean strict no-match, issue one complete-title-only search and evaluate it with the existing exact-title, duration-gated fallback matcher; only then try the existing bounded title segments. Cache the final result under the original media fingerprint.

**Tech Stack:** Rust 2024, Tokio, `url`, Serde streaming deserialization, existing LRCLIB transport/cache seams, Cargo test and Clippy.

---

### Task 1: Add the full-title request regression

**Files:**
- Modify: `src/lyrics.rs:2310-2650`

**Step 1: Write the failing source-client test**

Add a test using the existing `SequencedSourceTransport` and the reported LRCLIB metadata:

```rust
#[tokio::test]
async fn source_falls_back_to_full_title_for_translated_artist_plain_lyrics()
-> Result<(), Box<dyn Error>> {
    let record = br#"[{"trackName":"我看見今晚的月色很美，你呢？","artistName":"晚安莉莉","albumName":"Goodnight, Lillie.","duration":259.0,"plainLyrics":"第一行\n第二行","syncedLyrics":null}]"#;
    let transport = SequencedSourceTransport::from_responses([
        LrclibHttpResponse::new(200, b"[]".to_vec(), false),
        LrclibHttpResponse::new(200, record.to_vec(), false),
    ]);
    let client = LrclibClient::with_dependencies(
        Arc::new(transport.clone()),
        Arc::new(SourceClock::default()),
        8,
        60_000,
    );
    let mut item = source_item("我看見今晚的月色很美，你呢？");
    item.creators = vec!["Goodnight, Lillie".to_owned()];
    item.collection = Some("Goodnight, Lillie.".to_owned());
    item.duration_ms = Some(259_000);

    let document = client.fetch(&item).await?.ok_or("full-title lyrics")?;

    assert_eq!(document.source(), LyricsSource::Lrclib);
    assert_eq!(document.plain(), Some("第一行\n第二行"));
    assert!(document.timed().is_empty());
    let requests = transport.requests();
    assert_eq!(requests.len(), 2);
    let fallback = requests[1].url().query_pairs().collect::<Vec<_>>();
    assert!(fallback.iter().any(|(key, value)| {
        key == "track_name" && value == "我看見今晚的月色很美，你呢？"
    }));
    assert!(!fallback.iter().any(|(key, _)| key == "artist_name"));
    assert!(!fallback.iter().any(|(key, _)| key == "album_name"));
    Ok(())
}
```

**Step 2: Update orchestration expectations without changing production code**

Adjust the existing source tests to describe the intended request order:

- rename the unsplittable no-match test and expect two requests;
- insert an empty full-title response before segmented fallback responses;
- expect the bounded maximum to be `2 + MAX_LRCLIB_FALLBACK_REQUESTS`;
- keep strict success at one request;
- make fallback failure fixtures target the complete title so they still prove immediate fail-closed behavior.

**Step 3: Run tests to verify RED**

Run:

```bash
cargo test source_ --all-features -- --nocapture
```

Expected: the new regression and updated request-order tests fail because a strict no-match skips the complete-title-only request.

**Step 4: Commit the red tests**

```bash
git add src/lyrics.rs
git commit -m "test: reproduce full-title LRCLIB alias miss"
```

### Task 2: Orchestrate the bounded full-title fallback

**Files:**
- Modify: `src/lyrics.rs:341-365`

**Step 1: Add the minimal full-title lookup**

Between strict matching and segmented fallback iteration, add:

```rust
if document.is_none() {
    let fallback_body = self.search(&item.title, None, None).await?;
    document = match_lrclib_fallback_response(&fallback_body, &item.title, &request)
        .map_err(|_| LrclibSourceError::InvalidResponse)?;
}
```

Keep the existing matcher, response validation, sequential requests, and cache insertion unchanged.

**Step 2: Run the focused tests to verify GREEN**

Run:

```bash
cargo test source_ --all-features -- --nocapture
cargo test lrclib_fallback_ --all-features -- --nocapture
```

Expected: all source orchestration and fallback matcher tests pass.

**Step 3: Run lyrics and workflow regression tests**

Run:

```bash
cargo test lyrics --all-features -- --nocapture
cargo test --test workflows --all-features -- --nocapture
```

Expected: all selected tests pass.

**Step 4: Commit the implementation**

```bash
git add src/lyrics.rs
git commit -m "fix: fetch full-title LRCLIB fallbacks"
```

### Task 3: Document the additional external lookup

**Files:**
- Modify: `README.md:230-238`
- Modify: `tests/docs.rs:872-905`

**Step 1: Strengthen the failing documentation assertion**

Require the lyrics privacy guidance to say that a strict no-match may trigger
one complete-title request without artist or album metadata before up to three
segmented-title requests.

**Step 2: Run the documentation test to verify RED**

Run:

```bash
cargo test --test docs readme_explains_synchronized_lyrics_controls_sources_and_privacy -- --nocapture
```

Expected: FAIL because the README only describes segmented fallback requests.

**Step 3: Update the README**

Document the strict-first sequence, the full-title artistless request, the
bounded segmented requests, and local duration/ambiguity validation without
claiming that lyric content or duration is sent.

**Step 4: Run the documentation test to verify GREEN**

Run the command from Step 2.

Expected: PASS.

**Step 5: Commit the documentation**

```bash
git add README.md tests/docs.rs
git commit -m "docs: explain full-title LRCLIB fallback"
```

### Task 4: Verify the complete change

**Files:**
- Verify only

**Step 1: Format**

```bash
cargo fmt --all -- --check
```

Expected: exit 0.

**Step 2: Lint all targets**

```bash
cargo clippy --all-targets --all-features -- -D warnings
```

Expected: exit 0 with no warnings.

**Step 3: Run the complete suite**

```bash
cargo test --all-targets --all-features
```

Expected: all non-ignored tests pass.

**Step 4: Check the patch**

```bash
git diff --check HEAD~3..HEAD
git status --short
```

Expected: no whitespace errors; only the user's pre-existing untracked files remain.
