# Queue-List Playback and Local Favorites Design

## Scope

This design changes activation of loaded playable lists from append-and-play to
replace-and-play, and adds a provider-independent Favorites destination backed
by the local SQLite database.

## List Playback

Activating a playable row in Search, Charts, listening History, the playable
Songs section of Library, an opened podcast's episode list, or Favorites emits
one typed list-playback action. The controller derives that action from the
current application state, not from rendered text. It includes the currently
loaded playable media, the selected media ID, and a fresh deterministic shuffle
seed.

The reducer prepares the replacement queue before mutating application state:

1. retain only playable media supplied by the typed surface;
2. preserve source order while removing duplicate `MediaId` values;
3. reject an empty list, a missing selected ID, or more than 1,024 retained
   items;
4. build and validate a complete candidate `QueueSnapshot`;
5. carry forward the old Repeat mode;
6. select the activated media;
7. when Shuffle was enabled, place the selected item first in active order and
   shuffle the remainder with the supplied seed;
8. disable Endless Radio.

Only after every validation succeeds does the reducer replace the old queue and
start the selected media through the existing music or podcast playback path.
Preparation failures record a safe diagnostic and preserve the prior queue,
playback, Repeat, Shuffle, and Radio state. Resolution or device failures after
a valid replacement continue to use the existing playback error behavior.

Queue-panel activation remains play-one-existing-queue-item. Explicit enqueue
commands remain append operations. Metadata rows in Search and Library remain
non-playable and never enter replacement queues.

## Favorites Domain and Persistence

Favorites are local `MediaItem` snapshots keyed by the complete provider/media
identity, not by title and not by YouTube video ID alone. A favorite record also
stores a local insertion timestamp and a monotonic SQLite row ID so newest-first
ordering is deterministic when timestamps tie.

Schema migration v3 adds a `favorites` table with a unique provider/media key
and a newest-first index. The repository exposes bounded list, add, and remove
operations. Add runs in an immediate transaction: it checks the 1,024-item cap
and inserts only when capacity exists. A full collection returns a typed
capacity outcome; it never evicts an older favorite. Remove deletes only the
favorite row and does not touch session, queue, playback, history, or podcast
progress tables.

Favorites load during application startup so the `f` shortcut has complete
membership and capacity information from the first interactive frame. Storage
operations run through the existing FIFO storage owner and ordered effect loop.
Each toggle is generation-checked; the UI updates only from the corresponding
successful completion. Storage errors and overflow retain the prior in-memory
list and expose a bounded, payload-free error in the Favorites view.

Favorites are excluded from `SessionCheckpoint`. Queue clearing, replacement,
session restore, and session reset therefore cannot clear them.

## Navigation and Interaction

`Favorites` is a top-level `NavigationItem`, placed after Library. Its content
view renders newest-first playable rows with the shared selection viewport,
mouse row targets, loading/empty/error states, and selected artwork. Enter or a
second mouse click activates the entire currently loaded Favorites list through
the same list-playback action.

Normal-mode `f` toggles one media item:

- Content focus: the selected playable row in Search, Charts, an opened podcast
  episode list, Library Songs, History, or Favorites.
- Queue focus: the selected queue row.
- Player focus: the currently playing item.
- Navigation focus, metadata rows, podcast recommendations, Settings, and other
  non-playable content: no action.

Adding places the item first after completion. Removing the selected favorite
chooses the next row, then the previous row at the end. Removing the currently
playing item changes only Favorites; queue selection, playback generation,
position, and status remain unchanged.

The shortcut is documented in Help and the command palette. It is consumed only
in normal mode and does not insert `f` into Search or palette text entry.

## Failure and Resource Boundaries

- Replacement queues and Favorites are independently capped at 1,024 entries.
- Deduplication and candidate queue construction are bounded before commit.
- SQLite writes are transactional and serialized by the existing storage owner.
- Corrupt favorite JSON or identity mismatches produce typed, payload-free
  storage errors rather than partial lists.
- Stale load/toggle completions are ignored by generation.
- No favorite metadata is sent to a new network service.

## Test Strategy

- Queue unit tests cover atomic replacement, selected-first shuffle, Repeat
  preservation, Radio disablement, deduplication, cap rejection, and rollback.
- Reducer/controller tests cover every supported list surface, metadata
  exclusion, queue/player `f` targeting, and playback start behavior.
- Storage and migration tests cover v1/v2 upgrades to v3, newest-first ties,
  uniqueness, the 1,024 boundary, no eviction, corruption, and independence
  from session resets.
- Runtime tests cover startup load, FIFO ordering, stale completions, typed
  overflow, and payload-free failures.
- UI tests cover top-level navigation, keyboard/mouse selection, artwork,
  loading/empty/error states, shortcut help, and removing the playing favorite
  without interrupting playback.

