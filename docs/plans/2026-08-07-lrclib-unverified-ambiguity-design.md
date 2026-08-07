# LRCLIB Unverified Fallback Ambiguity Design

## Problem

Artistless LRCLIB fallback searches can return unrelated songs whose normalized
titles and durations happen to agree. For the YouTube Music item
`ラブソング - Love Song` by `Marcy`, the provider supplies collection
`Love Song` and duration `257s`. The `ラブソング` fallback returns both:

- LRCLIB 24990028 by `OKAMOTO'S`, duration `257.115s`;
- LRCLIB 28980117 by `マルシィ`, duration `256s`.

Neither candidate's artist or album matches the localized provider metadata.
Both contain synchronized lyrics and pass the existing two-second duration
gate. The current ranking therefore selects the unrelated OKAMOTO'S record
because its duration is closer, displaying confidently wrong lyrics.

## Goals

- Never use duration proximity alone to choose between conflicting lyric
  documents from identity-unverified candidates.
- Preserve strict matching and verified artist or album fallback ranking.
- Preserve unique translated-alias fallbacks such as `Goodnight, Lillie` and
  `MC Cheung Tinfu` when only one duration-qualified document exists.
- Preserve identical-document collapse, request bounds, caching, privacy, and
  payload hardening.
- Avoid transliteration tables, fuzzy artist matching, or delayed mpv-driven
  lyric reloads in this correction.

## Approaches Considered

### Reject competing unverified documents (recommended)

Treat artist or album agreement as identity verification. If no accepted
candidate has either form of verification, accept only one distinct lyric
document; conflicting documents fail closed as ambiguous. This prevents wrong
lyrics while retaining unique alias recovery.

### Retry with the runtime mpv duration

The player later reports a duration closer to the correct Marcy record, but
lyrics begin loading before mpv has loaded the stream. Retrying would add state,
network work, and timing-dependent selection, while duration would still be
used as identity evidence.

### Add artist transliteration aliases

Mapping `Marcy` to `マルシィ` could select this record, but a reliable generic
cross-script identity system is outside the current metadata and dependency
boundaries. A small manual alias table would be incomplete and unsafe.

## Design

The fallback response accumulator will separately observe accepted candidates
that have neither an exact normalized artist match nor an exact normalized album
match. It will retain the first such lyric document and mark a conflict when a
later unverified candidate contains a different document.

Existing rank selection continues unchanged. If any accepted candidate has an
artist or album match, the normal rank and equal-rank ambiguity rules decide the
result; verified candidates already outrank unverified ones. If no verified
candidate exists and conflicting unverified documents were observed, finishing
the response returns `AmbiguousMatch` regardless of synchronization or duration
ranking. Repeated records containing the same document remain safe to collapse.

The client continues mapping matcher ambiguity to its existing payload-free
external-source error. `LyricsSourceService` therefore retains provider plain
lyrics when available; otherwise the UI reports lyric unavailability rather
than showing unrelated synchronized text.

## Data Flow

1. Apply existing exact-title and two-second duration admission checks.
2. Compute existing artist, album, synchronization, and duration rank fields.
3. Record whether the accepted document has artist or album verification.
4. Run existing rank selection.
5. At response completion, reject conflicting documents only when the entire
   accepted set lacks identity verification.
6. Return the verified or uniquely unverified document through the existing
   cache and source-selection path.

## Testing

- Reproduce the Marcy/OKAMOTO'S response and require `AmbiguousMatch` in either
  response order.
- Prove a single translated-artist candidate still succeeds.
- Prove identical unverified documents still collapse.
- Prove verified artist or album candidates still outrank unrelated candidates.
- Update synchronization and closest-duration ranking tests so those ranking
  dimensions operate only after identity verification.
- Run focused LRCLIB, source-service, workflow, and complete project gates.
