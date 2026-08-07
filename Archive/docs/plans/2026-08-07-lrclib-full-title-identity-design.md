# LRCLIB Full-Title Identity Verification Design

## Problem

The artistless full-title LRCLIB fallback can accept an unrelated song when its
normalized title and duration happen to agree with the requested item. YouTube
Music reports `How could you?` by Nancy Kwai with a duration of 236 seconds.
LRCLIB has no Nancy Kwai result, but its title-only search returns record
35078027, `How Could You` by Sanchez, at 238 seconds. That record is exactly on
the existing two-second admission boundary, so the unique-unverified fallback
rule accepts and displays its synchronized lyrics.

Duration agreement is useful for rejecting impossible candidates, but it is not
strong enough to establish song identity for a complete common title.

## Goals

- Never accept an artistless full-title result without verified artist or album
  identity.
- Preserve strict title, artist, and album matching as the primary path.
- Preserve verified translated-artist full-title recovery, including the
  Goodnight Lillie case whose album matches exactly.
- Preserve the existing unique-candidate policy for segmented multilingual
  title fallbacks.
- Preserve request bounds, caching, privacy, response hardening, and payload-free
  error behavior.

## Approaches Considered

### Require identity only for full-title fallback (recommended)

Use the existing exact normalized artist or album match as a hard requirement
when evaluating the artistless complete-title response. Duration remains an
admission filter, not identity evidence. Segmented title variants retain their
current ambiguity policy because their purpose is to recover cross-language
metadata aliases.

### Reject every unverified artistless fallback

This is simpler and stricter, but it removes useful unique translated-alias
segment recovery that was an explicit feature requirement.

### Reduce the duration tolerance

A smaller tolerance would happen to reject the Sanchez record but would not fix
the identity problem. Unrelated recordings can share closer durations, while
legitimate provider and LRCLIB durations can differ because of intros, silence,
or rounding.

## Design

The fallback matcher will receive an explicit identity policy. Its existing
candidate admission and ranking remain unchanged. When the policy requires
identity verification, finishing the response returns no match unless at least
one accepted candidate has an exact normalized artist or album match. If a
verified candidate exists, the current ranking and equal-rank ambiguity rules
select among accepted records.

The client will use the verified-identity policy only for the artistless search
of the complete original title. It will use the current unique-unverified policy
for derived title segments. This keeps the trust boundary at the orchestration
site where full-title and segmented requests are distinguishable.

An unverified full-title response is a clean no-match rather than a parsing
error. The client may continue to segmented variants when the title actually
contains safe variants. If no later match exists, the source service retains
provider plain lyrics when available or reports lyrics unavailable. The
unrelated document is never cached.

## Data Flow

1. Run the existing strict title-and-artist LRCLIB search.
2. On no match, run the artistless complete-title search.
3. Apply exact title and two-second duration admission as today.
4. Require at least one admitted candidate to match the requested artist or
   album before selecting a full-title document.
5. If full-title identity is unverified, treat it as no match and continue to
   safe segmented variants, if any.
6. Apply the existing unique-unverified ambiguity policy to segmented variants.
7. Cache only the final accepted document or final no-match result.

## Testing

- Reproduce the Nancy Kwai request and Sanchez LRCLIB record at the two-second
  boundary; require the full-title matcher policy to return no document.
- Prove the client does not return or cache the Sanchez lyrics.
- Prove the result is independent of response order when unverified candidates
  accompany a verified candidate.
- Preserve the verified Goodnight Lillie full-title fallback.
- Preserve unique translated-artist segmented fallback behavior.
- Run focused lyrics and source tests, formatting, strict Clippy, and the full
  all-target/all-feature suite.
