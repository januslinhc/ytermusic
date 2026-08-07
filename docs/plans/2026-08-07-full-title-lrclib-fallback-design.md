# Full-Title LRCLIB Fallback Design

## Problem

The multilingual LRCLIB fallback only issues artistless searches for title
segments separated by ` - `, ` – `, or ` — `. It cannot recover a record when
the complete title already agrees but the provider and LRCLIB use translated
artist names.

For `我看見今晚的月色很美，你呢？`, YouTube Music identifies the artist as
`Goodnight, Lillie`, while LRCLIB record 36174150 identifies the artist as
`晚安莉莉`. A strict title/artist search returns no candidates. A title-only
search returns a unique 259-second record with plain lyrics, but the current
client never makes that request because the title has no recognized segment
separator.

## Goals

- Preserve strict title, artist, and album matching as the primary path.
- Recover exact full-title records whose artist metadata is a translated alias.
- Preserve duration, ambiguity, payload-size, privacy, and cache boundaries.
- Keep plain-only records in the full lyrics overlay rather than the player.
- Avoid fuzzy title matching and permanent artist-alias tables.

## Design

After a clean strict no-match, the client will issue one artistless search for
the complete title. The response will use the existing fallback matcher: a
candidate must have the exact normalized queried title and a known duration
within the existing two-second tolerance. Exact artist and album matches remain
ranking preferences, followed by synchronized content and closest duration.
Distinct equal-ranked documents remain ambiguous and fail closed.

If the full-title response yields no document, the existing bounded segmented
fallbacks run in their current order. The complete title is not added to the
segment derivation helper, which keeps request orchestration explicit and avoids
changing that helper's established contract. The new lookup consumes one
additional bounded request only after strict matching fails.

An accepted plain-only record becomes a normal LRCLIB `LyricsDocument`: it is
available in the `L` overlay and remains absent from the inline player. An
accepted synchronized record follows the existing inline and overlay behavior.
The final result, including no match, is cached under the original media ID and
metadata fingerprint.

## Alternatives Considered

- Artist alias mappings would require an incomplete, continuously maintained
  translation database and could introduce incorrect cross-language matches.
- Direct LRCLIB record IDs are unavailable in current YouTube Music metadata.
- Broad fuzzy title matching would weaken the existing false-positive boundary.

## Testing

- Reproduce record 36174150 with the complete title, translated artist alias,
  259-second duration, and plain-only lyrics.
- Prove request ordering is strict, full-title-only, then segmented fallbacks.
- Prove strict success still makes one request.
- Reject full-title candidates with mismatched duration or ambiguous equal rank.
- Prove a plain-only accepted record remains an overlay-only document.
- Preserve existing LRCLIB matcher, cache, response-hardening, and source-service
  tests.
