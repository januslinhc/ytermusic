# Country Podcast Recommendations and Stable List Scrolling Design

## Problem

The Podcasts view is empty until the user performs a search, so it provides no useful default discovery experience. The desired behavior is to recommend currently popular podcast shows for the configured country and then open a selected show through YouTube Music. YouTube Music's anonymous Explore response does not reliably expose a country podcast chart, so the ranking source must be independent from the playback provider.

Long lists also have a navigation defect. The renderer derives the first visible row directly from the selected index on every frame. Once the selection reaches the bottom of the viewport, moving upward shifts the entire page on every key press and keeps the highlight pinned near the bottom instead of leaving visible rows stable.

## Approved behavior

- When no show is open, Podcasts displays up to 20 ranked podcast shows for the active country without requiring a search.
- Rankings come from Apple's public, country-specific Top Shows feed. Apple supplies discovery metadata only; YouTube Music remains the only search, detail, queue, and playback provider.
- Each row displays its rank, show title, and publisher. The first row is selected after the initial load.
- Pressing Enter resolves only the selected recommendation against YouTube Music, displays a short matching status, and opens the matched show's existing episode view.
- Only a strong podcast-title match is accepted. A failed or ambiguous match leaves the recommendation list intact and reports that the show is unavailable on YouTube Music.
- Escape from an opened show returns to the country's ranked list. Manual podcast search remains unchanged.
- Podcasts uses the same country selection as Charts. Changing the country refreshes both views, but does not forcibly close a podcast show that is already open.
- The special configured region `ZZ` resolves to the operating-system country when possible and falls back to `US` otherwise.
- Recommendations are cached per effective country for the current process, with entries considered stale after about one hour.
- Up and Down move the highlight within a stable viewport. Rows scroll only when the selection crosses a visible boundary. This behavior applies consistently to Search, Charts, Podcasts, Library, History, Queue, and country or browser pickers.

## Architecture and data flow

A dedicated podcast-ranking boundary will keep third-party discovery separate from the existing `MusicProvider`. Its bounded result model contains only the ranking information needed by the UI: stable source identity, rank, title, publisher, and an optional validated artwork URL. It does not expose or use an Apple playback URL.

Activating an unpopulated Podcasts view requests recommendations for the configured region. The ranking service resolves `ZZ`, checks its in-memory country cache, and otherwise fetches Apple's HTTPS Marketing Tools feed. The completion includes the effective country so the heading can say `Top podcasts in <country>`. Request generations prevent a late response for a previous country from replacing the current list.

Selecting a recommendation starts a separate lazy-resolution operation. The runtime searches YouTube Music's podcast filter with a bounded query derived from the recommendation title and publisher. Matching normalizes titles, strongly prefers an exact normalized title, uses publisher agreement only as a tie-breaker, and rejects weak candidates. A successful provider identifier flows through the existing podcast-detail loading path rather than creating a second episode implementation.

Changing country updates the shared region and starts both the existing charts request and a podcast-recommendation request. If a show is open, its detail remains on screen; the refreshed ranked list is ready when the user returns with Escape.

## Stable viewport state

The renderer will retain a small scroll offset for each independently selectable list because only rendering knows the actual number of rows available after layout. For every frame, the offset is clamped to the current item count and viewport height. It changes only when the selected row lies above the visible start or below the visible end.

Viewport state is keyed by list identity and reset when its underlying dataset is replaced, such as a new search result, chart generation, recommendation country, or picker contents. Terminal resizes clamp the stored offset without losing the selection. Stateless rendering helpers can continue to receive an explicit viewport value, while the mutable terminal renderer owns the memory used between frames.

## Reliability and safety

- The ranking request uses HTTPS, strict connection and total timeouts, a response-size cap, and bounded feed/item/text limits.
- Only the first 20 valid ranked shows are retained. Artwork is optional and must use a permitted HTTPS URL shape before storage.
- Cache size is bounded across countries and contains no credentials or playback data.
- Network, parsing, and matching errors are concise and secret-safe; raw upstream payloads are never displayed or logged.
- A ranking failure leaves the Podcasts view available for manual search. A lazy-resolution failure leaves the ranked list and selection available for another choice.
- Empty or malformed country feeds produce an explicit unavailable state rather than fabricated recommendations.

## Verification

Fixture tests will cover representative US, JP, and HK feeds, malformed and oversized input, effective-country fallback, limits, validation, and cache freshness. State and runtime tests will cover initial activation, country changes, stale generations, preserving an open show, exact and rejected YouTube matches, and failure recovery. Controller tests will cover Enter and Escape behavior while preserving manual search.

Rendering tests will cover the default ranked-show list and prove viewport stability when moving upward and downward through long lists. Additional cases will cover dataset replacement, shorter content, terminal growth and shrinkage, empty lists, and each shared list surface. The full Rust test suite and formatting/lint checks will be run before completion.
