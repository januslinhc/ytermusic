# Release verification checklist

Verification date: 2026-07-28

Reviewed revision: `335590edfeea6a4276d6417feee799d27b82db5d`

Native host: macOS 26.5.1 (25F80), arm64

Toolchain: `rustc 1.97.1`, `cargo 1.97.1`

## Release decision

**PENDING — release blocked.** The offline macOS gate passes, but anonymous
network smoke tests could not reach the provider in the sandbox and both
requests for approved network execution were aborted before a result. Native
Linux and Windows CI evidence, authenticated smoke tests, and manual audible
song/podcast playback with fade observation are also outstanding.

Statuses in this document are evidence states, not tasks:

- **PASS** — the dated command or observation completed successfully.
- **PENDING** — required release evidence was not obtained; the release remains
  blocked where stated.
- **SKIP** — an optional credential-dependent check was deliberately not run
  because no dedicated test credential was supplied.
- **NOT RUN** — a manual scenario was deliberately not launched and has no
  observation evidence.

## Richer player interaction verification

Supplemental audit date: 2026-08-05

Reviewed implementation revision: `c13747ad3c37ffce0ea9ffa35d2f214ce8322240`

The documentation-evidence commit follows this implementation revision and
contains no production code.

### Privacy and resource audit

| Status | Scope | Evidence |
| --- | --- | --- |
| PASS | Notification and artwork diagnostics | Notification request `Debug`/`Display` implementations expose presence flags only or explicit redaction. Source logging contains only the generic `native notification is unavailable` warning; artwork URLs, provider IDs, titles, creators, collection names, image bytes, and cache paths are not logged. |
| PASS | Lyrics diagnostics | Lyric request/document/line debug output uses bounded counts, timing, source, and presence information rather than lyric text. No lyric text logging call was found. |
| PASS | Mouse target resources | Interaction maps stop at 512 regions, rendered row-target vectors stop at 128, inactive overlays avoid target collection, each new frame invalidates prior geometry, and stale or failed publications resolve no clicks. |
| PASS | Notification admission and blocking work | Cache creation/pruning has a 100 ms startup bound on an owned OS thread. Artwork decode and native submission each have a one-permit admission semaphore. Network reads and worker replacement are bounded and cancellable; image decoding, cache file operations, and native submission do not block Tokio workers. Cache failure falls back to text-only notifications where the platform remains available, without constructing an HTTP artwork client. The artwork client is constructed only when a prepared cache and private-path artwork mode are both available. The latest request replaces pending work, errors are reported once, and shutdown is deadline-bounded. |
| PASS | Notification artwork cache | macOS/Linux artwork is bounded to 4 MiB input, validated before decode, resized to at most 512×512, stored in a private `0700` directory with `0600` files on Unix, and retains at most two PNG files. Startup pruning runs before the notification enabled/platform gate, including disabled and Windows-unavailable starts. Windows does not attach artwork. |
| PASS | Terminal color capability | Production captures stdout terminal status plus `NO_COLOR`, `TERM`, and `COLORTERM` once. Non-terminal, opted-out, or dumb output is monochrome; ordinary ANSI is Basic, `256color` is ANSI 256, and declared truecolor/24bit/direct output is TrueColor. The same detected value configures artwork and the renderer theme. |

### Platform and dependency inspection

| Status | Platform | Evidence |
| --- | --- | --- |
| PASS | macOS arm64 host | Native dependency tree selects `notify-rust` 4.18 with the UserNotifications preview backend and private-path artwork support. Host all-target/all-feature verification is recorded below. |
| PASS | Linux cfg/dependencies | Manifest and cfg inspection select `notify-rust` 4.18 with Tokio-backed zbus and private-path artwork support. No native Linux executable was run. |
| PASS | Windows cfg/dependencies | Manifest and cfg inspection select `winrt-toast-reborn` 0.3.8. Notifications require an optional validated, already-registered `windows_aum_id`, are text-only, and perform no PowerShell or registry mutation. |
| PENDING | Windows cross-compile | Target not installed: `rustup target list --installed` returned only `aarch64-apple-darwin`; therefore no Windows compile result is claimed. |
| PASS | Unsupported Unix cfg | The fallback is cfg-isolated, supplies no artwork, and returns a redacted unavailable result without affecting playback. No native unsupported-Unix executable was run. |

### Supplemental automated gate

Host: macOS 26.5.1 (25F80), arm64; `rustc 1.97.1`; `cargo 1.97.1`.

| Status | Command | Evidence |
| --- | --- | --- |
| PASS | `cargo fmt --all -- --check` | Exit 0; no formatting differences. |
| PASS | `cargo clippy --all-targets --all-features -- -D warnings` | Exit 0; strict Clippy completed with no warnings. |
| PASS | `git diff --check` | Exit 0; no whitespace errors. |
| PASS | `cargo test --all-targets --all-features -- --quiet` | Exit 0; 1,022 passed, 0 failed, 13 ignored. |

The 13 ignored tests were not executed: four `doctor` child-process fixtures
and nine opt-in live provider tests. The one test that ran in the
`provider_live` target is a local decoder-cleanup invariant; no live provider
request, native notification, or interactive playback was executed.

### Manual macOS smoke

No real TUI or native notification was launched: this run had no interactive
environment or user consent for visible OS notifications and media-key/audio
control. Automated evidence does not replace these manual observations.

| Status | Scenario | Reason |
| --- | --- | --- |
| NOT RUN | Notification Center entry and artwork | No interactive environment or user consent for a native notification. |
| NOT RUN | F7/F8/F9 in playing, paused, and loading modes | No interactive terminal/media-key environment. |
| NOT RUN | All player mouse controls and progress seeking | No interactive TUI or audio session. |
| NOT RUN | Charts section scrolling and stable selection viewport | No interactive TUI session. |

## Favorites and explicit-list playback verification

Supplemental audit date: 2026-08-06

Reviewed implementation revision: `d35ed465cfb7764fa93d680b06efb02f91da8400`

The documentation-evidence commits follow this implementation revision and
contain no production code.

### Automated contract evidence

| Status | Scope | Evidence |
| --- | --- | --- |
| PASS | Local Favorites storage | `favorite_capacity_is_transactional_and_never_evicts`, `favorite_order_is_deterministic_newest_first_when_timestamps_tie`, and `favorites_persist_across_reopen_and_session_replacement`. |
| PASS | Favorites runtime and reducer behavior | `favorites_commands_use_ordered_storage_and_runtime_clock`, `favorites_full_is_safe_category_completion`, and `favorites_removing_playing_item_leaves_playback_and_queue_unchanged`. |
| PASS | Explicit-list Queue behavior | `explicit_list_accepts_1024_unique_items_and_rejects_1025`, `explicit_list_applies_the_item_cap_after_full_media_id_deduplication`, `explicit_list_shuffle_keeps_selected_first_and_randomizes_only_the_remainder`, and `explicit_list_always_disables_endless_radio`. |
| PASS | Favorites and list input | `favorite_shortcut_targets_selected_playable_content_without_other_ui_changes`, `favorite_shortcut_targets_selected_queue_item_and_current_player_item`, `explicit_list_activation_replaces_queue_from_every_loaded_playable_surface`, and `favorites_content_moves_toggles_and_activates_the_explicit_list`. |

### Manual macOS smoke

No interactive TUI, isolated application database, or audio session was
launched for this audit. Automated evidence does not replace these manual
observations.

| Status | Scenario | Reason |
| --- | --- | --- |
| NOT RUN | Favorites startup load and newest-first order | No isolated interactive TUI/database session. |
| NOT RUN | Favorite toggling from Content, Queue, and Player focus; Favorites overflow rejection without eviction; removing the playing favorite leaves playback and Queue unchanged | No isolated interactive TUI/database and audio session. |
| NOT RUN | Explicit-list Queue replacement, selected/current shuffle behavior, and Endless Radio disablement | No interactive TUI/audio session. |
| NOT RUN | Explicit-list playback accepts 1,024 unique playable rows; 1,025-row rejection preserves the old Queue, playback, and modes | No interactive oversized provider-list fixture. |

## Bubble Tea UI motion verification

Supplemental audit date: 2026-08-07

Reviewed implementation revision: `67d2e34`

### Automated contract evidence

| Status | Scope | Evidence |
| --- | --- | --- |
| PASS | Theme-aware borderless progress | Deterministic UI tests cover empty, fractional, full, narrow, true-color, fallback-color, paused, unknown-duration, shimmer, and complete-bar mouse-seek geometry. |
| PASS | Presentation-only selection motion | Pure and rendered tests cover bounded gliding, rapid retargeting, off-screen and incompatible-data snaps, hidden and resized resets, immediate logical selection, selected-row styling, timed lyrics, and phase-invariant hit maps. |
| PASS | Braille loading spinner | UI tests cover initial and retained loading states, errors, wraparound phases, visible lyrics loading, hidden surfaces, and unchanged Player resolving/buffering labels. |
| PASS | Bounded coalesced motion clock | Runtime tests cover the 30 FPS ceiling, latest-value tick coalescing, paused demand, idle shutdown, render failure, no external effects, and exact injected frames. |

### Manual macOS smoke

No interactive TUI or audio session was launched for this audit. Automated
evidence does not replace these visual, audible, input, resize, and idle-resource
observations.

| Status | Scenario | Reason |
| --- | --- | --- |
| NOT RUN | Play, pause, resume, and seek while observing progress fill and shimmer | No interactive TUI/audio session. |
| NOT RUN | Rapid keyboard and mouse selection across every list | No interactive TUI/input session. |
| NOT RUN | Visible loading start and completion, including retained rows and errors | No controlled interactive provider-loading session. |
| NOT RUN | Resize wide, compact, and tiny layouts during progress, selection, and loading motion | No interactive terminal resize session. |
| NOT RUN | Idle CPU and redraw behavior after all visible motion completes | No interactive process observation session. |

## Offline gate

All commands ran in the repository root on the native macOS host before any live
provider attempt.

| Status | Date | Platform | Command | Evidence |
| --- | --- | --- | --- | --- |
| PASS | 2026-07-28 | macOS arm64 | `cargo fmt --check` | Exit 0; no formatting differences. |
| PASS | 2026-07-28 | macOS arm64 | `cargo clippy --all-targets --all-features -- -D warnings` | Exit 0; strict Clippy completed with no warnings. |
| PASS | 2026-07-28 | macOS arm64 | `cargo test --all-targets` | Exit 0; 586 passed, 0 failed, 4 ignored child-process fixtures. |
| PASS | 2026-07-28 | macOS arm64 | `cargo test --doc` | Exit 0; 0 doctests registered, 0 failed. |
| PASS | 2026-07-28 | macOS arm64 | `cargo build --release` | Exit 0; optimized release target built. |
| PASS | 2026-07-28 | macOS arm64 | `git diff --check` | Exit 0; no whitespace errors before checklist creation. |

## Explicit provider smoke

The required command was:

```text
YTERMUSIC_LIVE_TESTS=1 cargo test --features live-tests --test provider_live -- --ignored --nocapture
```

The sandboxed run exited 101: 1 test passed and 3 failed. The passing test was
the authenticated test's no-credential self-skip path. Anonymous song search,
HK/US charts, and podcast discovery each returned the secret-safe category
`ProviderError { operation: Authentication, kind: Unavailable }`. This is not a
provider PASS. Two escalated reruns requesting network access were aborted
before a test result, after approximately 187 seconds and 266 seconds
respectively. No further network attempt was made.

| Status | Date | Platform | Scenario | Evidence |
| --- | --- | --- | --- | --- |
| PENDING | 2026-07-28 | macOS arm64, sandboxed network | Anonymous song search | `anonymous_song_search_returns_normalized_results` failed with `Authentication/Unavailable`; approved-network rerun aborted. |
| PENDING | 2026-07-28 | macOS arm64, sandboxed network | Anonymous album discovery | The explicit live target has no album smoke; no manual provider observation was made. |
| PENDING | 2026-07-28 | macOS arm64, sandboxed network | Anonymous playlist discovery | The explicit live target has no playlist smoke; no manual provider observation was made. |
| PENDING | 2026-07-28 | macOS arm64, sandboxed network | Anonymous chart discovery | `anonymous_hk_and_us_charts_return_normalized_sections` failed with `Authentication/Unavailable`; approved-network rerun aborted. |
| PENDING | 2026-07-28 | macOS arm64, sandboxed network | Anonymous podcast discovery | `anonymous_podcast_search_returns_normalized_results` failed with `Authentication/Unavailable`; approved-network rerun aborted. |

### Region matrix

| Status | Date | Platform | Region | Evidence |
| --- | --- | --- | --- | --- |
| PENDING | 2026-07-28 | macOS arm64 | HK | Offline normalization and country-switch tests passed, but the required live HK chart request was network-blocked. |
| PENDING | 2026-07-28 | macOS arm64 | US | Offline normalization and country-switch tests passed, but the required live US chart request was network-blocked. |
| PENDING | 2026-07-28 | macOS arm64 | GB | Region validation is covered by `cargo test --all-targets`; no live GB chart was observed. |
| PENDING | 2026-07-28 | macOS arm64 | JP | Region validation is covered by `cargo test --all-targets`; no live JP chart was observed. |
| PENDING | 2026-07-28 | macOS arm64 | ZZ | Default-region behavior is covered by `config_defaults_match_the_documented_baseline`; no live `ZZ` chart was observed. |

### Optional authenticated smoke

No dedicated `YTERMUSIC_LIVE_COOKIE` was supplied. No browser store, browser
profile, existing Ytermusic vault entry, or user credential was read.

| Status | Date | Platform | Import source | Evidence |
| --- | --- | --- | --- | --- |
| SKIP | 2026-07-28 | macOS arm64 | Chrome | No dedicated test credential supplied; optional authenticated smoke deliberately skipped. |
| SKIP | 2026-07-28 | macOS arm64 | Firefox | No dedicated test credential supplied; optional authenticated smoke deliberately skipped. |
| SKIP | 2026-07-28 | macOS arm64 | Safari | No dedicated test credential supplied; optional authenticated smoke deliberately skipped. |
| SKIP | 2026-07-28 | macOS arm64 | Edge | No dedicated test credential supplied; optional authenticated smoke deliberately skipped. |

## Playback, persistence, and resilience evidence

These PASS rows are automated contract evidence from the 586-test offline run.
They do not replace the pending native audible playback gate below.

| Status | Date | Platform | Scenario | Automated evidence |
| --- | --- | --- | --- | --- |
| PASS | 2026-07-28 | macOS arm64 | Play | `play_resolves_loads_at_silence_and_ramps_to_target` and `anonymous_startup_search_enqueue_and_play`. |
| PASS | 2026-07-28 | macOS arm64 | Pause and resume | `pause_fades_before_pausing_and_resume_unpauses_before_fading` and `resume_interrupts_a_pause_fade_without_repausing_at_fade_completion`. |
| PASS | 2026-07-28 | macOS arm64 | Seek and podcast speed | `podcast_seek_and_speed_are_forwarded_without_wall_clock_waits`. |
| PASS | 2026-07-28 | macOS arm64 | Skip/next/previous | Queue navigation and reducer tests, including `queue_next_and_previous_load_podcast_progress_before_resolution`. |
| PASS | 2026-07-28 | macOS arm64 | Natural end | `natural_end_emits_the_current_attempt_generation` and `natural_end_resolves_next_and_respects_repeat_modes`. |
| PASS | 2026-07-28 | macOS arm64 | Shuffle | `seeded_shuffle_is_deterministic_complete_and_keeps_current_first` and the combined workflow test. |
| PASS | 2026-07-28 | macOS arm64 | Repeat | `repeat_one_replays_the_current_item_in_both_directions`, `repeat_all_wraps_at_both_edges`, and the combined workflow test. |
| PASS | 2026-07-28 | macOS arm64 | Radio | `radio_fill_is_based_on_the_strict_count_after_current` and `shuffle_repeat_radio_and_queue_reorder_preserve_current_and_active_ids`. |
| PASS | 2026-07-28 | macOS arm64 | Queue restore | `startup_restores_queue_and_podcast_playback` and queue snapshot round-trip tests. |
| PASS | 2026-07-28 | macOS arm64 | Podcast resume | `podcast_search_open_episode_play_and_resume_saved_position` and completed replay/restart tests. |
| PASS | 2026-07-28 | macOS arm64 | Dependency missing | `missing_dependency_uses_the_injected_platform_hint` and `dependency_failure_keeps_browsing_usable_and_exposes_repair_action`. |
| PASS | 2026-07-28 | macOS arm64 | Player dead/failure | `terminal_backend_failure_stops_polling_and_emits_failure_once`. |
| PASS | 2026-07-28 | macOS arm64 | Player restart | `backend_shutdown_before_file_loaded_restarts_the_staged_attempt`, paused restart, and restart-budget tests. |

### Fade matrix

| Status | Date | Platform | Duration | Evidence |
| --- | --- | --- | --- | --- |
| PASS | 2026-07-28 | macOS arm64, automated | 0 ms | Zero-duration envelope, mapped-intent, load, and replace tests passed. |
| PASS | 2026-07-28 | macOS arm64, automated | 500 ms point | Linear fade sampling at 500 ms passed; exact native audible 500 ms observation remains pending below. |
| PASS | 2026-07-28 | macOS arm64, automated | 2,000 ms | `mapped_resume_interrupts_fade_out_without_a_volume_jump` uses a configured 2,000 ms fade and passed. |
| PENDING | 2026-07-28 | macOS arm64, manual audio | 0/500/2,000 ms audible transitions | No audible output was automated or manually observed in this run. |

### Terminal layouts

| Status | Date | Platform | Layout | Evidence |
| --- | --- | --- | --- | --- |
| PASS | 2026-07-28 | macOS arm64, automated | Wide | `wide_layout_snapshot` and wide artwork-panel tests passed. |
| PASS | 2026-07-28 | macOS arm64, automated | Compact | `compact_layout_snapshot` and compact queue-focus tests passed. |
| PASS | 2026-07-28 | macOS arm64, automated | Tiny | `tiny_layout_snapshot`, tiny player, and clamped-overlay tests passed. |

## Local player smoke

| Status | Date | Platform | Scenario | Evidence |
| --- | --- | --- | --- | --- |
| PENDING | 2026-07-28 | macOS arm64, PTY | Release startup | Not run: production startup traverses the credential-vault stage, and no isolated test vault was available. Avoiding access to any existing user credential took precedence over a native TUI observation. |
| PENDING | 2026-07-28 | macOS arm64, PTY | Clean `q` exit and terminal restoration | No native PTY startup was performed. Automated `normal_quit_checkpoints_shuts_down_player_and_restores_terminal` and terminal-guard tests passed, but do not satisfy this manual gate. |
| PENDING | 2026-07-28 | macOS arm64, manual audio | One public song plays | Not manually observed; no audible output was automated. |
| PENDING | 2026-07-28 | macOS arm64, manual audio | One public podcast episode plays | Not manually observed; no audible output was automated. |

## Native platform matrix

| Status | Date | Platform | Evidence |
| --- | --- | --- | --- |
| PASS | 2026-07-28 | macOS 26.5.1 arm64 | Native offline suite and release build passed. Startup/exit and audible integration are tracked separately above. |
| PENDING | 2026-07-28 | Linux | No native Linux execution or current remote CI result was available in this run. |
| PENDING | 2026-07-28 | Windows | No native Windows execution or current remote CI result was available in this run. |
| PENDING | 2026-07-28 | GitHub Actions | Current macOS/Linux/Windows matrix and dependency/security jobs were not observed remotely. |

## Secret and persistence audit

| Status | Date | Platform | Scope | Evidence |
| --- | --- | --- | --- | --- |
| PASS | 2026-07-28 | macOS arm64 | Tracked files and fixtures | Narrow `git grep -l` classification found credential terms only in source, tests, documentation, CI, and packaging placeholders. A second filename-only search for private-key headers and common Google/OAuth/JWT value prefixes returned no files, so no candidate value was printed or retained. |
| PASS | 2026-07-28 | macOS arm64 | Logs and test workspace | `git ls-files -- '*.log' '*.db' '*.sqlite' '*.sqlite3'` and matching `find target`/`find /private/tmp` searches returned no retained log or database artifacts. Test `TempDir` data had been removed. No real application data path was inspected. |
| PASS | 2026-07-28 | macOS arm64 | SQLite schema/test databases | `cargo test --test storage schema_identifiers_do_not_contain_sensitive_names -- --exact --nocapture` passed 1/1. `src/storage/schema_v1.sql` contains only migration, session, podcast-progress, history, and metadata-cache fields; no credential or authorization column exists. |
| PASS | 2026-07-28 | macOS arm64 | Resolved media URL persistence | Storage-boundary inspection found `ResolvedStream.url` confined to the runtime resolver/player path. Persisted `SessionCheckpoint` contains only queue and `PlaybackSnapshot`; the latter stores media identity/status/timing/volume/speed. SQLite tables have no resolved-stream URL field, and player presentation retains only bounded codec/format labels. |

## Blocking release evidence

The release must not be declared complete until all of the following are
recorded as PASS:

- anonymous live song, album, playlist, chart, and podcast discovery;
- HK, US, GB, JP, and `ZZ` live region checks;
- one public song and one public podcast episode through native `mpv`;
- audible fades at 0 ms, 500 ms, and 2,000 ms;
- native macOS startup/clean exit plus Linux and Windows verification;
- the remote native CI and dependency/security jobs.
