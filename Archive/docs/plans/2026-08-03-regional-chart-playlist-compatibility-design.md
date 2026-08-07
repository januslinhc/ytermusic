# Regional Chart Playlist Compatibility Design

## Problem

The charts endpoint no longer always returns playable songs directly. Current regional responses can contain several carousel types: album cards, a regional chart playlist card, and artist rows. The existing parser treats every carousel as a playable-song shelf, skips all of those unsupported cards, and reports an invalid response when no direct song survives. This produces `charts failed: provider returned an invalid response` for regions such as Japan.

## Approved behavior

- Preserve support for legacy chart responses containing direct playable rows.
- Recognize regional chart playlist cards whose title navigation browse ID starts with `VL`.
- Strip only the leading `VL` browse marker and hydrate the resulting playlist ID through the playable watch-playlist query.
- Render the hydrated tracks as an ordinary chart section, preserving the existing queue and playback behavior.
- Ignore unrelated album and artist carousels.
- Keep response parsing, identifiers, request count, and the complete operation bounded; never expose raw provider payloads in errors or debug output.

## Data flow

`ChartsQuery` will normalize a response into a small internal result containing either legacy playable sections or bounded chart-playlist references. Legacy playable sections take precedence and require no additional request. When only playlist references are present, `RealYtMusicApi::charts` hydrates them sequentially in response order with `GetWatchPlaylistQuery::new_from_playlist_id` and constructs playable `ChartSection` values. This endpoint accepts the normalized bare playlist ID and returns a consistent playable-track model for both song and video chart playlists.

Hydration is best effort across recognized references: successful non-empty playlists are retained; an individual failed or empty playlist does not hide another usable chart. If no reference produces a usable section, the first provider error is returned when available, otherwise the response is classified as invalid. The existing top-level charts timeout covers both discovery and hydration.

## Parsing and limits

The parser inspects at most the existing section and item limits. A chart playlist card must be a `musicTwoRowItemRenderer` with a non-empty title and a title-run `browseEndpoint.browseId` beginning with `VL`; the stripped identifier must pass the existing opaque-ID constraints. At most the existing section limit of references is retained. Album IDs such as `MPRE...`, artist rows, malformed cards, and cards without the exact marker are ignored.

## Verification

A minimized JP-shaped fixture will contain a country selector, an unrelated album carousel, a `Trending 20 Japan` playlist card, and an artist carousel. Tests will prove that the chart playlist is recognized with the marker removed, unrelated cards are ignored, hydration returns playable tracks under the playlist title, legacy direct-song fixtures remain supported, and malformed or excessive data remains safely rejected or bounded.
