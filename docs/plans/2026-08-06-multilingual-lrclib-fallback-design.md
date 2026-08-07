# Multilingual LRCLIB Fallback Design

## Problem

YouTube Music can expose localized and translated metadata in one title while
LRCLIB stores only the original-language title and artist. For example, YouTube
returns `與我無關 - Not My Problem` by `MC Cheung Tinfu`, while LRCLIB track
9979227 is `與我無關` by `MC 張天賦`. The current client searches with the full
YouTube title and primary artist and then requires exact normalized title and
artist matches. LRCLIB consequently returns no candidates, even though the
205-second record has synchronized lyrics.

## Goals

- Preserve the existing strict search and matching path as the first choice.
- Recover multilingual records when one bounded title segment matches exactly.
- Require duration agreement and reject ambiguous candidates.
- Avoid general fuzzy title or artist matching.
- Keep network, response-size, metadata-size, cache, and privacy boundaries.

## Design

The LRCLIB client will perform its current strict metadata search first. If that
response contains no accepted document, it may derive bounded title variants by
splitting a bilingual title only at recognized separator forms. Empty, duplicate,
oversized, and one-character variants are discarded. Each remaining variant is
queried by exact `track_name` without an artist constraint, one at a time and
under a small fixed request cap.

Fallback candidates must have an exactly normalized title equal to the queried
variant and a duration within the existing tolerance. An exact normalized artist
or album match remains preferred when available. A candidate with a translated
artist alias may be accepted only when the best duration-qualified result is
unique; equal-ranked distinct lyric documents remain ambiguous and are rejected.
Synchronized lyrics continue to rank ahead of plain lyrics only after metadata
acceptance.

The first accepted fallback result is cached under the original media identity
and metadata fingerprint. Transport failures remain non-fatal and fall through to
the existing provider/plain-lyrics behavior. Responses receive the same redirect,
status, byte-size, result-count, and text-size validation as strict responses.

## Data Flow

1. Build and issue the existing full-title/full-artist search.
2. Apply the existing strict matcher.
3. If unmatched, derive safe title segments from the original title.
4. Query each segment with `track_name` only, up to the fixed cap.
5. Rank only exact-segment, duration-qualified records using strict metadata as a
   preference and uniqueness as the acceptance gate.
6. Cache and return the accepted document, or return no external lyrics.

## Testing

- Reproduce LRCLIB track 9979227 metadata against the YouTube title, artist, and
  205-second duration.
- Prove strict search remains first and avoids fallback when it succeeds.
- Prove fallback requests are bounded, encoded, and omit the mismatching artist.
- Reject partial titles, excessive variants, duration mismatch, and ambiguous
  equal-ranked candidates.
- Preserve existing response hardening, cache, concurrency, and source-selection
  tests.

