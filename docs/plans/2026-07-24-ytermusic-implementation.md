# Ytermusic Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Build a production-quality, cross-platform YouTube Music TUI that supports anonymous and authenticated discovery, regional charts, podcasts, persistent queue modes, and reliable faded playback.

**Architecture:** A Ratatui front end sends typed actions to a single reducer. Tokio effects call provider, storage, resolver, and player services behind testable traits; `ytmapi-rs` supplies YouTube Music metadata, `yt-dlp` resolves short-lived audio URLs, and `mpv` is controlled through JSON IPC. SQLite persists non-secret state, while browser-session credentials live only in the operating system vault.

**Tech Stack:** Rust 1.97 / edition 2024, Tokio 1.53, Ratatui 0.30, Crossterm 0.29, `ytmapi-rs` 0.3, `yt-dlp`, `mpv`, Rusqlite 0.40, Serde, Clap, Keyring, Proptest, Insta, and GitHub Actions.

---

## Source of Truth and Execution Rules

- Read `docs/plans/2026-07-24-ytermusic-design.md` before starting.
- Use @test-driven-development for every behavior change: write one failing test,
  run it and inspect the expected failure, implement the minimum, then rerun it.
- Use @systematic-debugging for any unexpected failure.
- Use @verification-before-completion before claiming a task or the whole project
  is complete.
- Keep commits limited to one task. Do not fold unrelated cleanup into a task.
- Never put cookies, authorization headers, raw provider responses, or resolved
  stream URLs in fixtures, logs, snapshots, or commits.
- Default tests must not contact YouTube or require `mpv`, `yt-dlp`, or a keyring.
- Run opt-in live tests only when `YTERMUSIC_LIVE_TESTS=1` is explicitly set.

## Target Repository Layout

```text
.
├── .github/workflows/ci.yml
├── Cargo.toml
├── README.md
├── rust-toolchain.toml
├── src
│   ├── app/
│   ├── auth.rs
│   ├── cli.rs
│   ├── config.rs
│   ├── diagnostics.rs
│   ├── domain/
│   ├── fade.rs
│   ├── lib.rs
│   ├── main.rs
│   ├── platform/
│   ├── player/
│   ├── process.rs
│   ├── provider/
│   ├── queue.rs
│   ├── resolver/
│   ├── runtime.rs
│   ├── storage/
│   └── ui/
├── tests
│   ├── fixtures/
│   └── live/
└── packaging/
```

### Task 1: Create the Compiling Crate and CLI Contract

**Files:**
- Create: `Cargo.toml`
- Create: `rust-toolchain.toml`
- Create: `.gitignore`
- Create: `src/lib.rs`
- Create: `src/main.rs`
- Create: `src/cli.rs`
- Create: `tests/cli_help.rs`

**Step 1: Add configuration-only scaffolding**

Create `Cargo.toml` with this dependency boundary:

```toml
[package]
name = "ytermusic"
version = "0.1.0"
edition = "2024"
rust-version = "1.97"
description = "A keyboard-first YouTube Music terminal player"
license = "MIT"

[features]
default = []
live-tests = []

[dependencies]
anyhow = "1"
async-trait = "0.1"
bytes = "1"
clap = { version = "4.6.4", features = ["derive"] }
crossterm = { version = "0.29", features = ["event-stream"] }
directories = "6"
futures = "0.3"
image = { version = "0.25", default-features = false, features = ["jpeg", "png", "webp"] }
keyring = "4.1.5"
rand = "0.9"
rand_chacha = "0.9"
ratatui = "0.30.2"
reqwest = { version = "0.13", default-features = false, features = ["json", "rustls"] }
rusqlite = { version = "0.40.1", features = ["bundled", "serde_json", "uuid"] }
secrecy = "0.10"
semver = "1"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
tempfile = "3"
thiserror = "2.0.19"
time = { version = "0.3", features = ["formatting", "macros", "serde"] }
tokio = { version = "1.53.1", features = ["full", "test-util"] }
tokio-util = "0.7"
toml = "0.9"
tracing = "0.1"
tracing-appender = "0.2"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
unicode-width = "0.2"
url = "2"
uuid = { version = "1", features = ["serde", "v4"] }
which = "8"
ytmapi-rs = { version = "0.3.2", default-features = false, features = ["rustls", "serde_json", "simplified-queries"] }

[dev-dependencies]
assert_cmd = "2"
insta = "1.48.0"
predicates = "3"
proptest = "1.11.0"
serial_test = "3"

[lints.rust]
unsafe_code = "forbid"

[lints.clippy]
all = "warn"
pedantic = "warn"
unwrap_used = "warn"
expect_used = "warn"
```

Set `rust-toolchain.toml` to Rust `1.97` with `rustfmt` and `clippy`. Ignore
`/target`, local database files, logs, `.env`, browser-cookie exports, and
snapshot `.new` files.

Create a minimal `src/lib.rs` exporting `cli`, and a `src/main.rs` that calls
`ytermusic::cli::run()`. This is generated project scaffolding, not product
behavior.

**Step 2: Write the failing CLI test**

```rust
use assert_cmd::Command;
use predicates::str::contains;

#[test]
fn help_explains_the_product_and_support_commands() {
    Command::cargo_bin("ytermusic")
        .unwrap()
        .arg("--help")
        .assert()
        .success()
        .stdout(contains("Music without leaving your terminal"))
        .stdout(contains("doctor"))
        .stdout(contains("auth"));
}
```

**Step 3: Run the test and confirm RED**

Run: `cargo test --test cli_help -- --nocapture`  
Expected: FAIL because the placeholder CLI does not contain the promised help.

**Step 4: Implement the minimum CLI**

Use this public contract in `src/cli.rs`:

```rust
use clap::{Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(name = "ytermusic", about = "Music without leaving your terminal")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    Doctor,
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },
}

#[derive(Clone, Debug, ValueEnum)]
pub enum Browser {
    Brave,
    Chrome,
    Chromium,
    Edge,
    Firefox,
    Opera,
    Safari,
    Vivaldi,
}

#[derive(Debug, Subcommand)]
pub enum AuthCommand {
    Import { #[arg(value_enum)] browser: Browser },
    Status,
    Logout,
}

pub fn run() -> anyhow::Result<()> {
    let _cli = Cli::parse();
    Ok(())
}
```

**Step 5: Verify GREEN**

Run: `cargo test --test cli_help -- --nocapture`  
Expected: PASS.

Run: `cargo fmt --check && cargo clippy --all-targets --all-features -- -D warnings`  
Expected: PASS with no warnings.

**Step 6: Commit**

```bash
git add Cargo.toml rust-toolchain.toml .gitignore src tests/cli_help.rs
git commit -m "chore: scaffold ytermusic crate and cli"
```

### Task 2: Define Domain Models and Validated Configuration

**Files:**
- Create: `src/domain/mod.rs`
- Create: `src/domain/media.rs`
- Create: `src/domain/playback.rs`
- Create: `src/config.rs`
- Modify: `src/lib.rs`
- Create: `tests/config.rs`

**Step 1: Write failing value-object tests**

Cover these behaviors:

```rust
#[test]
fn region_codes_are_normalized_and_validated() {
    assert_eq!(RegionCode::parse("hk").unwrap().as_str(), "HK");
    assert!(RegionCode::parse("hong-kong").is_err());
}

#[test]
fn config_rejects_unsafe_playback_values() {
    let mut config = Config::default();
    config.playback.fade_in_ms = 60_000;
    assert!(config.validate().is_err());
}

#[test]
fn defaults_are_anonymous_and_immediately_usable() {
    let config = Config::default();
    assert_eq!(config.region.as_str(), "ZZ");
    assert_eq!(config.playback.volume, 80);
    assert_eq!(config.podcast.speed, 1.0);
}
```

**Step 2: Run and confirm RED**

Run: `cargo test --test config -- --nocapture`  
Expected: FAIL because `RegionCode` and `Config` do not exist.

**Step 3: Implement the exact domain boundary**

`src/domain/media.rs` must expose:

```rust
#[derive(Clone, Debug, Eq, Hash, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct MediaId {
    pub provider: String,
    pub video_id: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub enum MediaKind { Song, Video, PodcastEpisode }

#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct MediaItem {
    pub id: MediaId,
    pub kind: MediaKind,
    pub title: String,
    pub creators: Vec<String>,
    pub collection: Option<String>,
    pub duration_ms: Option<u64>,
    pub artwork_url: Option<url::Url>,
    pub explicit: bool,
}
```

`src/domain/playback.rs` must define `RepeatMode::{Off, One, All}`,
`PlaybackStatus::{Stopped, Resolving, Buffering, Playing, Paused, Failed}`,
`PlaybackSnapshot`, and a `RegionCode` that accepts only `ZZ` or two ASCII
letters and stores uppercase.

`src/config.rs` must contain:

```rust
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
#[serde(default)]
pub struct Config {
    pub region: RegionCode,
    pub playback: PlaybackConfig,
    pub podcast: PodcastConfig,
    pub behavior: BehaviorConfig,
}
```

Validation bounds:

- volume: `0..=100`;
- fade-in/out: `0..=10_000` milliseconds;
- podcast speed: `0.5..=3.0`;
- podcast skip backward/forward: `1..=600` seconds;
- resolver cache: `0..=300` seconds.

Add `Config::load(path)`, `Config::save(path)`, and `Config::validate()`. A
missing file returns defaults; malformed or invalid files return a typed error.

**Step 4: Verify GREEN**

Run: `cargo test --test config -- --nocapture`  
Expected: PASS.

**Step 5: Commit**

```bash
git add src/domain src/config.rs src/lib.rs tests/config.rs
git commit -m "feat: add domain models and validated configuration"
```

### Task 3: Implement the Persistent Queue Semantics

**Files:**
- Create: `src/queue.rs`
- Modify: `src/lib.rs`
- Create: `tests/queue.rs`

**Step 1: Write failing example tests**

Test sequential advancement, repeat-one, repeat-all wrapping, stable seeded
shuffle, removal before and after the cursor, reorder, and radio de-duplication.
The key test is:

```rust
#[test]
fn enabling_shuffle_preserves_current_and_every_item_once() {
    let mut queue = Queue::from_items(items(["a", "b", "c", "d"]));
    queue.select(QueueItemId::from("b")).unwrap();
    queue.set_shuffle(true, 42);

    assert_eq!(queue.current().unwrap().id, QueueItemId::from("b"));
    let ids = queue.active_ids();
    assert_eq!(ids.len(), 4);
    assert_eq!(ids.iter().collect::<HashSet<_>>().len(), 4);
}
```

Add a Proptest generating unique IDs and arbitrary edit sequences. Assert that
the active order contains exactly the logical items, without loss or duplicate,
after every valid operation.

**Step 2: Run and confirm RED**

Run: `cargo test --test queue -- --nocapture`  
Expected: FAIL because `Queue` is missing.

**Step 3: Implement the queue**

Use this state shape:

```rust
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct Queue {
    logical: Vec<QueueItem>,
    active: Vec<QueueItemId>,
    current: Option<QueueItemId>,
    repeat: RepeatMode,
    shuffle_seed: Option<u64>,
    radio: bool,
}
```

Required operations:

- `from_items`, `items`, `current`, and `active_ids`;
- `append`, `append_unique`, `remove`, `move_before`, and `clear`;
- `select`, `next`, and `previous`;
- `set_repeat`, `set_shuffle(enabled, seed)`, and `set_radio`;
- `needs_radio_fill(threshold)`; and
- `snapshot` / `restore`.

When shuffle is enabled, use `ChaCha8Rng::seed_from_u64`. Keep the current item
first in the remaining active sequence, shuffle only the other items, and
persist the seed. Do not use thread RNG in queue logic.

**Step 4: Verify GREEN**

Run: `cargo test --test queue -- --nocapture`  
Expected: all example and property tests PASS.

**Step 5: Commit**

```bash
git add src/queue.rs src/lib.rs tests/queue.rs
git commit -m "feat: add deterministic queue modes"
```

### Task 4: Build Cancellable Fade Envelopes

**Files:**
- Create: `src/fade.rs`
- Modify: `src/lib.rs`
- Create: `tests/fade.rs`

**Step 1: Write failing clock-free tests**

```rust
#[test]
fn envelope_interpolates_and_clamps() {
    let fade = FadeEnvelope::linear(0.0, 80.0, Duration::from_secs(2));
    assert_eq!(fade.sample(Duration::ZERO), 0.0);
    assert_eq!(fade.sample(Duration::from_secs(1)), 40.0);
    assert_eq!(fade.sample(Duration::from_secs(3)), 80.0);
}

#[test]
fn replacing_a_fade_starts_from_effective_volume() {
    let mut controller = FadeController::new(80.0);
    controller.start(0.0, 80.0, Duration::from_secs(4));
    controller.tick(Duration::from_secs(1));
    controller.start_from_current(0.0, Duration::from_secs(1));
    assert_eq!(controller.effective_volume(), 20.0);
    assert_eq!(controller.target_volume(), 80.0);
}
```

**Step 2: Run and confirm RED**

Run: `cargo test --test fade -- --nocapture`  
Expected: FAIL because fade types do not exist.

**Step 3: Implement**

Keep target and effective volume separate. `FadeEnvelope::sample` must be pure
and clamp elapsed time. `FadeController` owns at most one envelope plus a
monotonic elapsed duration. Starting a newer envelope replaces the old one from
the current sampled value. Cancelling restores the intended target unless the
caller explicitly requests silence.

Expose `FadeIntent::{Play, Resume, Pause, Stop, Replace, NaturalEnd}` and a
function mapping intent plus config to an envelope. Do not sleep inside this
module.

**Step 4: Verify GREEN**

Run: `cargo test --test fade -- --nocapture`  
Expected: PASS.

**Step 5: Commit**

```bash
git add src/fade.rs src/lib.rs tests/fade.rs
git commit -m "feat: add cancellable audio fades"
```

### Task 5: Add the Reducer and Stale-Effect Protection

**Files:**
- Create: `src/app/mod.rs`
- Create: `src/app/action.rs`
- Create: `src/app/effect.rs`
- Create: `src/app/reducer.rs`
- Create: `src/app/state.rs`
- Modify: `src/lib.rs`
- Create: `tests/reducer.rs`

**Step 1: Write failing reducer tests**

Cover:

- starting a search increments its generation and emits `Effect::Search`;
- only results matching the active generation replace visible results;
- selecting a result emits queue and resolve effects;
- player progress changes the now-playing snapshot;
- a failed resolve creates a diagnostic and advances only when configured;
- radio fill is requested once when below the threshold.

Representative stale-response test:

```rust
#[test]
fn stale_search_results_are_ignored() {
    let (state, _) = reduce(AppState::default(), Action::SearchSubmitted("a".into()));
    let (state, _) = reduce(state, Action::SearchSubmitted("ab".into()));
    let active = state.search.generation;

    let (state, _) = reduce(
        state,
        Action::SearchCompleted {
            generation: active - 1,
            result: Ok(page_with("old")),
        },
    );

    assert!(state.search.items.is_empty());
}
```

**Step 2: Run and confirm RED**

Run: `cargo test --test reducer -- --nocapture`  
Expected: FAIL because app state and reducer are absent.

**Step 3: Implement**

`reduce(state, action) -> (AppState, Vec<Effect>)` must be deterministic and
perform no I/O. Use explicit generation newtypes for search, collection load,
artwork, resolution, and radio fill. Store only normalized domain models in
state.

Effects must include:

```rust
pub enum Effect {
    Search { generation: Generation, query: String, filter: SearchFilter },
    LoadCharts { generation: Generation, region: RegionCode },
    Resolve { generation: Generation, item: MediaItem },
    Player(PlayerCommand),
    Persist(SessionCheckpoint),
    FillRadio { generation: Generation, seed: MediaId },
    FetchArtwork { generation: Generation, url: Url },
}
```

**Step 4: Verify GREEN**

Run: `cargo test --test reducer -- --nocapture`  
Expected: PASS.

**Step 5: Commit**

```bash
git add src/app src/lib.rs tests/reducer.rs
git commit -m "feat: add deterministic application reducer"
```

### Task 6: Persist Sessions, Queue, History, Cache, and Podcasts

**Files:**
- Create: `src/storage/mod.rs`
- Create: `src/storage/migrations.rs`
- Create: `src/storage/repository.rs`
- Create: `src/storage/schema_v1.sql`
- Modify: `src/lib.rs`
- Create: `tests/storage.rs`

**Step 1: Write failing repository tests**

Using a temporary database, test:

- opening a new database applies schema version 1;
- a queue checkpoint round-trips logical order, active order, current ID, mode,
  seed, and radio state;
- podcast position uses an upsert and never goes backward when an older
  checkpoint arrives;
- history is newest-first and bounded;
- expired cache entries are not returned;
- no table or column name contains `cookie`, `authorization`, or `secret`.

**Step 2: Run and confirm RED**

Run: `cargo test --test storage -- --nocapture`  
Expected: FAIL because storage is absent.

**Step 3: Create the schema**

`schema_v1.sql` must create:

```sql
CREATE TABLE schema_migrations (
  version INTEGER PRIMARY KEY,
  applied_at INTEGER NOT NULL
);
CREATE TABLE session_state (
  singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
  payload TEXT NOT NULL,
  updated_at INTEGER NOT NULL
);
CREATE TABLE podcast_progress (
  video_id TEXT PRIMARY KEY,
  position_ms INTEGER NOT NULL,
  duration_ms INTEGER,
  played INTEGER NOT NULL DEFAULT 0,
  updated_at INTEGER NOT NULL
);
CREATE TABLE listening_history (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  video_id TEXT NOT NULL,
  item_json TEXT NOT NULL,
  played_at INTEGER NOT NULL
);
CREATE INDEX history_played_at ON listening_history(played_at DESC);
CREATE TABLE metadata_cache (
  cache_key TEXT PRIMARY KEY,
  payload TEXT NOT NULL,
  expires_at INTEGER NOT NULL,
  stored_at INTEGER NOT NULL
);
```

Set WAL mode, foreign keys, and a busy timeout. Run migrations in a transaction.

**Step 4: Implement repositories**

Expose a `Storage` trait used by runtime and a `SqliteStorage` implementation.
Keep Rusqlite calls synchronous inside the module; the runtime will call them
through `spawn_blocking`. Serialize domain snapshots with Serde. Cap history at
5,000 rows after insertion.

**Step 5: Verify GREEN**

Run: `cargo test --test storage -- --nocapture`  
Expected: PASS.

**Step 6: Commit**

```bash
git add src/storage src/lib.rs tests/storage.rs
git commit -m "feat: persist queue history cache and podcast progress"
```

### Task 7: Abstract Child Processes and Implement `doctor`

**Files:**
- Create: `src/process.rs`
- Create: `src/diagnostics.rs`
- Modify: `src/cli.rs`
- Modify: `src/lib.rs`
- Create: `tests/doctor.rs`

**Step 1: Write failing tests with a fake runner**

Define tests proving:

- paths and arguments are passed as separate OS strings, never shell text;
- compatible `mpv`, `yt-dlp`, and `ffmpeg` versions report healthy;
- a missing dependency reports platform-specific install hints;
- unexpected version output is degraded, not a panic;
- diagnostic rendering redacts cookie-like values and stream query strings.

**Step 2: Run and confirm RED**

Run: `cargo test --test doctor -- --nocapture`  
Expected: FAIL because process and diagnostics interfaces are missing.

**Step 3: Implement the process seam**

```rust
#[derive(Clone, Debug)]
pub struct CommandSpec {
    pub program: PathBuf,
    pub args: Vec<OsString>,
}

pub struct ProcessOutput {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

#[async_trait]
pub trait ProcessRunner: Send + Sync {
    async fn output(&self, spec: CommandSpec) -> Result<ProcessOutput, ProcessError>;
}
```

The production runner uses `tokio::process::Command` directly with
`kill_on_drop(true)`. No code may call a shell.

Implement `DependencyChecker` with executable discovery and parsers for the
first semantic-looking version in output. Use capability checks rather than
needlessly strict minimums: `mpv` must accept JSON IPC, `yt-dlp` must support
`-J`, and `ffmpeg` must be executable.

**Step 4: Wire `doctor`**

Make `ytermusic doctor` print a compact table and exit nonzero only when
playback-critical dependencies are missing. Browsing-only capability should be
reported separately.

**Step 5: Verify GREEN**

Run: `cargo test --test doctor -- --nocapture`  
Expected: PASS.

**Step 6: Commit**

```bash
git add src/process.rs src/diagnostics.rs src/cli.rs src/lib.rs tests/doctor.rs
git commit -m "feat: add dependency diagnostics"
```

### Task 8: Resolve Audio Safely Through `yt-dlp`

**Files:**
- Create: `src/resolver/mod.rs`
- Create: `src/resolver/ytdlp.rs`
- Create: `tests/fixtures/ytdlp_song.json`
- Create: `tests/resolver.rs`
- Modify: `src/lib.rs`

**Step 1: Add a scrubbed fixture and failing tests**

The fixture must contain only invented IDs and a localhost media URL. Test:

- the command contains `-J`, `--no-playlist`, `--no-warnings`, and
  `--format ba/b`;
- a media ID becomes `https://music.youtube.com/watch?v=<encoded-id>`;
- authenticated resolution adds a cookie-file argument without exposing its
  contents;
- JSON maps to `ResolvedStream`;
- nonzero exit and missing URL are typed failures;
- an expired cache entry invokes the runner again.

**Step 2: Run and confirm RED**

Run: `cargo test --test resolver -- --nocapture`  
Expected: FAIL because the resolver is absent.

**Step 3: Implement**

```rust
#[derive(Clone, Debug)]
pub struct ResolvedStream {
    pub media_id: MediaId,
    pub url: url::Url,
    pub title: Option<String>,
    pub duration_ms: Option<u64>,
    pub codec: Option<String>,
    pub format_id: Option<String>,
    pub resolved_at: time::OffsetDateTime,
}

#[async_trait]
pub trait Resolver: Send + Sync {
    async fn resolve(
        &self,
        item: &MediaItem,
        auth: Option<&CookieFile>,
        cancel: CancellationToken,
    ) -> Result<ResolvedStream, ResolveError>;
}
```

Parse only stdout JSON. Limit captured stderr, redact it, and retain a stable
error category. Cache successful resolutions by `(video_id, auth_identity)` for
the configured short TTL. Never persist URLs.

**Step 4: Verify GREEN**

Run: `cargo test --test resolver -- --nocapture`  
Expected: PASS.

**Step 5: Commit**

```bash
git add src/resolver src/lib.rs tests/resolver.rs tests/fixtures/ytdlp_song.json
git commit -m "feat: resolve short-lived audio streams"
```

### Task 9: Implement the `mpv` JSON Protocol and Platform Transports

**Files:**
- Create: `src/player/mod.rs`
- Create: `src/player/protocol.rs`
- Create: `src/player/transport.rs`
- Create: `src/platform/mod.rs`
- Create: `src/platform/unix_ipc.rs`
- Create: `src/platform/windows_ipc.rs`
- Create: `tests/fixtures/mpv_events.jsonl`
- Create: `tests/mpv_protocol.rs`
- Modify: `src/lib.rs`

**Step 1: Write failing protocol tests**

Test exact JSON for `loadfile`, pause, seek, volume, observe-property, and quit.
Parse request replies, `property-change`, `file-loaded`, `end-file`, and
`shutdown`. Unknown events must map to `MpvEvent::Unknown`, not fail the stream.

Example:

```rust
#[test]
fn volume_command_is_structured_json() {
    let request = MpvRequest::set_property(7, "volume", json!(42.5));
    assert_eq!(
        serde_json::to_value(request).unwrap(),
        json!({"command":["set_property","volume",42.5],"request_id":7})
    );
}
```

**Step 2: Run and confirm RED**

Run: `cargo test --test mpv_protocol -- --nocapture`  
Expected: FAIL because protocol types are absent.

**Step 3: Implement the protocol codec**

Use newline-delimited JSON and a monotonically increasing `u64` request ID.
Model replies and events separately. Impose a maximum line length and treat
invalid lines as diagnostics while keeping the connection alive where safe.

**Step 4: Implement platform transports**

Expose:

```rust
#[async_trait]
pub trait MpvConnector: Send + Sync {
    async fn connect(&self, endpoint: &IpcEndpoint)
        -> io::Result<Box<dyn AsyncReadWrite>>;
}
```

- On Unix, create a unique socket path inside a private temporary directory and
  use `tokio::net::UnixStream`.
- On Windows, generate `\\.\pipe\ytermusic-<uuid>` and use Tokio named pipes.
- Remove Unix endpoints after disconnect.
- Never accept remote TCP endpoints.

Add native `cfg` tests that start a fake peer and exchange one request/reply.

**Step 5: Verify GREEN**

Run: `cargo test --test mpv_protocol -- --nocapture`  
Run on the current OS: `cargo test player::transport -- --nocapture`  
Expected: PASS.

**Step 6: Commit**

```bash
git add src/player src/platform src/lib.rs tests/mpv_protocol.rs tests/fixtures/mpv_events.jsonl
git commit -m "feat: add cross-platform mpv ipc"
```

### Task 10: Supervise Playback and Apply Fades

**Files:**
- Create: `src/player/backend.rs`
- Create: `src/player/mpv.rs`
- Create: `src/player/supervisor.rs`
- Create: `tests/player.rs`
- Modify: `src/player/mod.rs`

**Step 1: Write failing state-machine tests**

Use fake resolver, clock, and backend. Cover:

- play resolves, loads, starts at zero effective volume, and ramps to target;
- pause fades out then sends pause;
- resume unpauses then fades in;
- replace cancels the previous resolve and fade;
- natural end advances the queue;
- URL rejection causes exactly one fresh resolve;
- backend death causes at most one safe restart;
- podcast seek and speed commands are forwarded;
- no test sleeps in wall-clock time.

**Step 2: Run and confirm RED**

Run: `cargo test --test player -- --nocapture`  
Expected: FAIL because the backend and supervisor are missing.

**Step 3: Implement the backend seam**

```rust
#[async_trait]
pub trait PlayerBackend: Send {
    async fn load(&mut self, url: &Url, start_ms: Option<u64>) -> Result<(), PlayerError>;
    async fn set_paused(&mut self, paused: bool) -> Result<(), PlayerError>;
    async fn seek_relative(&mut self, seconds: i64) -> Result<(), PlayerError>;
    async fn set_volume(&mut self, volume: f64) -> Result<(), PlayerError>;
    async fn set_speed(&mut self, speed: f64) -> Result<(), PlayerError>;
    async fn next_event(&mut self) -> Result<PlayerEvent, PlayerError>;
    async fn shutdown(&mut self) -> Result<(), PlayerError>;
}
```

`MpvBackend` starts `mpv --idle=yes --no-video --terminal=no --no-config
--input-ipc-server=<endpoint>` and observes `time-pos`, `duration`, `pause`,
`volume`, and `speed`.

**Step 4: Implement the supervisor**

The supervisor owns the current generation, resolver cancellation token, fade
controller, and backend. Drive fades from a Tokio interval, but inject a manual
clock/tick stream in tests. Emit typed `PlayerEvent`s to the app channel.

For a transition, complete fade-out before loading the next URL. When fades are
zero, use `loadfile ... replace` immediately. Do not attempt overlapping
dual-process crossfade.

**Step 5: Verify GREEN**

Run: `cargo test --test player -- --nocapture`  
Expected: PASS.

**Step 6: Commit**

```bash
git add src/player tests/player.rs
git commit -m "feat: supervise playback and fade transitions"
```

### Task 11: Define the Music Provider and Fixture Parsers

**Files:**
- Create: `src/provider/mod.rs`
- Create: `src/provider/model.rs`
- Create: `src/provider/charts.rs`
- Create: `tests/fixtures/charts_hk.json`
- Create: `tests/fixtures/podcast_search.json`
- Create: `tests/provider_fixtures.rs`
- Modify: `src/lib.rs`

**Step 1: Write failing provider-contract tests**

Define a fake provider and test the consumer-facing contract. Parse scrubbed
chart and podcast fixtures and assert:

- chart sections become songs with stable video IDs;
- creator runs are joined without navigation noise;
- missing duration stays `None`;
- malformed items are skipped while valid siblings survive;
- country is part of the chart cache key;
- podcast episodes remain distinguishable from songs.

**Step 2: Run and confirm RED**

Run: `cargo test --test provider_fixtures -- --nocapture`  
Expected: FAIL because provider types are missing.

**Step 3: Implement the provider boundary**

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SearchFilter { All, Songs, Albums, Artists, Playlists, Podcasts, Episodes }

pub struct Page<T> {
    pub items: Vec<T>,
    pub continuation: Option<String>,
    pub stale: bool,
}

#[async_trait]
pub trait MusicProvider: Send + Sync {
    async fn search(&self, query: &str, filter: SearchFilter) -> Result<Page<SearchItem>, ProviderError>;
    async fn charts(&self, region: &RegionCode) -> Result<Vec<ChartSection>, ProviderError>;
    async fn playlist(&self, id: &str) -> Result<Vec<MediaItem>, ProviderError>;
    async fn podcast(&self, id: &str) -> Result<Podcast, ProviderError>;
    async fn radio(&self, seed: &MediaId) -> Result<Vec<MediaItem>, ProviderError>;
    async fn library(&self, section: LibrarySection) -> Result<Page<LibraryItem>, ProviderError>;
    fn authentication(&self) -> AuthenticationState;
}
```

Keep fixture traversal in small total functions. Return an error only when the
whole response is unusable. Collect per-item parse warnings for diagnostics.

**Step 4: Verify GREEN**

Run: `cargo test --test provider_fixtures -- --nocapture`  
Expected: PASS.

**Step 5: Commit**

```bash
git add src/provider src/lib.rs tests/provider_fixtures.rs tests/fixtures/charts_hk.json tests/fixtures/podcast_search.json
git commit -m "feat: add normalized music provider contract"
```

### Task 12: Integrate YouTube Music Search, Charts, Podcasts, Radio, and Library

**Files:**
- Create: `src/provider/ytmusic.rs`
- Create: `src/provider/queries.rs`
- Create: `tests/provider_adapter.rs`
- Create: `tests/live/provider_live.rs`
- Modify: `src/provider/mod.rs`

**Step 1: Write failing adapter tests**

Put a narrow `YtMusicApi` trait in front of `ytmapi-rs` and implement a fake in
tests. Verify dispatch:

- each search filter calls the matching typed API method;
- podcast search, episode search, and podcast detail map correctly;
- radio calls the watch-playlist recommendation path and removes the seed;
- library methods return `AuthRequired` in anonymous mode;
- chart requests carry `browseId = "FEmusic_charts"` and
  `formData.selectedValues = [region]`.

**Step 2: Run and confirm RED**

Run: `cargo test --test provider_adapter -- --nocapture`  
Expected: FAIL because the adapter is absent.

**Step 3: Implement anonymous and browser-token clients**

Construct anonymous access with `YtMusic::new_unauthenticated()`. Construct
authenticated access with `YtMusic::from_cookie(cookie.expose_secret())`.
Normalize the crate's search song, album, artist, playlist, podcast, episode,
library, and watch-playlist result types immediately at the adapter boundary.

Implement a local `ChartsQuery` using `ytmapi_rs::query::{Query, PostMethod,
PostQuery}`:

```rust
impl PostQuery for ChartsQuery {
    fn header(&self) -> serde_json::Map<String, serde_json::Value> {
        json!({
            "browseId": "FEmusic_charts",
            "formData": { "selectedValues": [self.region.as_str()] }
        }).as_object().cloned().unwrap_or_default()
    }

    fn params(&self) -> Vec<(&str, Cow<'_, str>)> { Vec::new() }
    fn path(&self) -> &str { "browse" }
}
```

Use `raw_json_query` and the already tested chart fixture parser. The associated
query output type must be a local newtype to satisfy Rust's orphan rules.

**Step 4: Add opt-in live tests**

Gate every test twice:

```rust
#![cfg(feature = "live-tests")]

fn live_enabled() -> bool {
    std::env::var_os("YTERMUSIC_LIVE_TESTS").is_some()
}
```

If not enabled, return without network access. When enabled, test one anonymous
song query, one podcast query, and `HK` plus `US` charts. Authenticated library
tests additionally require a credential explicitly injected by CI or the user.

**Step 5: Verify**

Run: `cargo test --test provider_adapter -- --nocapture`  
Expected: PASS with no network.

Optional run:  
`YTERMUSIC_LIVE_TESTS=1 cargo test --features live-tests --test provider_live -- --ignored --nocapture`  
Expected: PASS when the current YouTube Music API is reachable.

**Step 6: Commit**

```bash
git add src/provider tests/provider_adapter.rs tests/live/provider_live.rs
git commit -m "feat: integrate youtube music discovery"
```

### Task 13: Import Browser Sessions Without Persisting Raw Secrets

**Files:**
- Create: `src/auth.rs`
- Create: `tests/auth.rs`
- Modify: `src/cli.rs`
- Modify: `src/lib.rs`

**Step 1: Write failing security tests**

Test:

- Netscape-cookie lines parse only relevant YouTube domains;
- comments, expired entries, and unrelated domains are ignored;
- `SAPISID` and `__Secure-3PAPISID` survive in the in-memory cookie header;
- Debug and Display never expose cookie values;
- import builds `yt-dlp --cookies-from-browser <browser> --cookies <temp>`;
- the temporary file is absent after success and error;
- logout deletes the vault entry;
- a fake vault supports status/import/logout without the real OS keyring.

**Step 2: Run and confirm RED**

Run: `cargo test --test auth -- --nocapture`  
Expected: FAIL because auth does not exist.

**Step 3: Implement**

Expose:

```rust
#[async_trait]
pub trait SecretVault: Send + Sync {
    async fn load_cookie(&self) -> Result<Option<SecretString>, AuthError>;
    async fn store_cookie(&self, value: SecretString) -> Result<(), AuthError>;
    async fn delete_cookie(&self) -> Result<(), AuthError>;
}
```

The production implementation uses the OS keyring service `ytermusic` and
account `youtube-music-cookie`. Run blocking keyring calls in `spawn_blocking`.

Create the cookie export with `tempfile`; apply user-only permissions before
launch on Unix and rely on the user's protected temp directory/ACL on Windows.
Parse the jar, build only the minimum YouTube cookie header, store it, zeroize
temporary strings where supported, and drop the temporary file before returning.

**Step 4: Wire CLI commands**

- `auth import <browser>` imports and verifies by creating an authenticated
  provider and calling a lightweight authenticated canary.
- `auth status` says connected, expired, or anonymous without printing identity
  details unless available safely.
- `auth logout` deletes only Ytermusic's vault entry.

**Step 5: Verify GREEN**

Run: `cargo test --test auth -- --nocapture`  
Expected: PASS.

**Step 6: Commit**

```bash
git add src/auth.rs src/cli.rs src/lib.rs tests/auth.rs
git commit -m "feat: add secure browser session import"
```

### Task 14: Build the Responsive TUI, Keymap, and Artwork Cells

**Files:**
- Create: `src/ui/mod.rs`
- Create: `src/ui/layout.rs`
- Create: `src/ui/input.rs`
- Create: `src/ui/render.rs`
- Create: `src/ui/theme.rs`
- Create: `src/ui/artwork.rs`
- Create: `tests/ui.rs`
- Modify: `src/lib.rs`

**Step 1: Write failing layout and snapshot tests**

Use `ratatui::backend::TestBackend`. Test:

- wide (`140x40`) renders navigation, content, queue, and player;
- compact (`90x30`) hides the queue behind a tab;
- tiny (`40x10`) renders focused content plus a one-line player;
- selected/focused panes are distinguishable without relying only on color;
- long Unicode titles do not overflow;
- the help dialog and command palette remain inside the viewport;
- every action has a command-palette label;
- standard and Vim keys map to the same semantic actions.

Use reviewed Insta snapshots with fixed test data.

**Step 2: Run and confirm RED**

Run: `cargo test --test ui -- --nocapture`  
Expected: FAIL because UI modules are absent.

**Step 3: Implement layout and input**

Use `LayoutMode::{Wide, Compact, Tiny}` derived only from frame dimensions.
Keep rendering functions side-effect free:

```rust
pub fn render(frame: &mut Frame<'_>, state: &AppState, theme: &Theme);
pub fn map_event(mode: InputMode, event: KeyEvent) -> Option<Action>;
```

Global keys: `q` quit, `?` help, `/` search, `:` palette, `Space` play/pause,
`n`/`p` next/previous, arrows or `hjkl` navigation, `+`/`-` volume, `s`
shuffle, `r` repeat, and `c` country selector. Text-entry modes consume printable
keys before global mapping.

**Step 4: Implement artwork approximation**

Decode downloaded thumbnail bytes with `image`, resize to the requested cell
grid, and combine pairs of vertical pixels into `▀` cells with foreground and
background RGB colors. Cache the decoded cell grid by URL plus dimensions. On
decode, network, or color-capability failure, render a deterministic icon and
metadata instead.

Unit-test pixel-to-cell conversion with an in-memory 2x2 image; network fetches
belong to an injected artwork service, not render code.

**Step 5: Verify GREEN**

Run: `cargo test --test ui -- --nocapture`  
Expected: PASS with reviewed snapshots and no `.snap.new` files.

**Step 6: Commit**

```bash
git add src/ui src/lib.rs tests/ui.rs tests/snapshots
git commit -m "feat: add responsive keyboard-first tui"
```

### Task 15: Add Complete Views and User Workflows

**Files:**
- Create: `src/ui/views/home.rs`
- Create: `src/ui/views/search.rs`
- Create: `src/ui/views/charts.rs`
- Create: `src/ui/views/podcasts.rs`
- Create: `src/ui/views/library.rs`
- Create: `src/ui/views/queue.rs`
- Create: `src/ui/views/history.rs`
- Create: `src/ui/views/settings.rs`
- Create: `src/ui/views/mod.rs`
- Create: `tests/workflows.rs`
- Modify: `src/ui/render.rs`
- Modify: `src/app/*`

**Step 1: Write failing workflow tests**

Drive reducer actions with fake effects and render the result. Cover:

1. anonymous startup -> search -> enqueue -> play;
2. country picker -> HK chart -> switch to US without stale HK overwrite;
3. podcast search -> open show -> play episode -> resume from saved position;
4. toggle shuffle/repeat/radio and reorder queue;
5. authenticated library visible versus anonymous connect prompt;
6. offline cached chart marked stale;
7. dependency failure leaves browsing usable and shows repair action.

**Step 2: Run and confirm RED**

Run: `cargo test --test workflows -- --nocapture`  
Expected: FAIL because full views/workflows are not wired.

**Step 3: Implement views one workflow at a time**

For each workflow, repeat RED/GREEN rather than implementing every screen at
once. Lists use stable item IDs, retain selection across refresh where possible,
show loading/empty/error states, and paginate when the provider returns a
continuation.

The bottom player always shows:

- play/pause/failure state;
- title and creator;
- elapsed and duration;
- volume and effective fade activity;
- shuffle, repeat, and radio mode;
- podcast speed when relevant; and
- resolver quality/codec when known.

**Step 4: Verify GREEN**

Run: `cargo test --test workflows -- --nocapture`  
Expected: PASS.

**Step 5: Commit**

```bash
git add src/ui/views src/ui/render.rs src/app tests/workflows.rs
git commit -m "feat: add discovery podcast library and queue workflows"
```

### Task 16: Compose the Runtime and Guarantee Clean Shutdown

**Files:**
- Create: `src/runtime.rs`
- Create: `src/platform/paths.rs`
- Create: `src/platform/signals.rs`
- Modify: `src/cli.rs`
- Modify: `src/main.rs`
- Modify: `src/lib.rs`
- Create: `tests/runtime.rs`

**Step 1: Write failing runtime tests**

With fake provider/storage/resolver/player and a finite event stream, test:

- effects execute concurrently but actions return through one bounded channel;
- replacing a search cancels the old task;
- storage calls run outside the async executor's blocking path;
- checkpoints are debounced and a final checkpoint occurs on shutdown;
- terminal restoration and player shutdown occur after normal quit, signal, and
  injected panic;
- queue and podcast state restore at startup.

**Step 2: Run and confirm RED**

Run: `cargo test --test runtime -- --nocapture`  
Expected: FAIL because runtime composition is absent.

**Step 3: Implement runtime composition**

`RuntimeServices` owns trait objects for provider, storage, resolver, player,
artwork, vault, clock, and process runner. `Runtime::run(event_source,
renderer)` drives:

1. render current state;
2. select over input, player events, effect completions, ticks, and shutdown;
3. reduce one action;
4. cancel superseded work;
5. dispatch effects;
6. schedule persistence.

Use a terminal guard whose `Drop` restores raw mode, mouse capture, cursor, and
alternate screen. Install the panic hook after constructing the guard. On
shutdown, stop accepting actions, cancel tasks, checkpoint, ask `mpv` to quit,
kill it after a bounded timeout, and restore the terminal.

**Step 4: Wire production startup**

Running `ytermusic` with no subcommand must:

- resolve platform config/data/cache paths;
- initialize secret-safe file logging outside the alternate screen;
- load and validate config;
- migrate SQLite;
- load optional credentials;
- construct anonymous or authenticated provider;
- perform dependency health checks;
- restore session state; and
- enter the TUI.

**Step 5: Verify GREEN**

Run: `cargo test --test runtime -- --nocapture`  
Expected: PASS.

Run: `cargo run -- doctor`  
Expected on the development machine: healthy rows for installed dependencies.

**Step 6: Commit**

```bash
git add src/runtime.rs src/platform src/cli.rs src/main.rs src/lib.rs tests/runtime.rs
git commit -m "feat: compose runtime and clean shutdown"
```

### Task 17: Document, Package, and Test All Three Platforms

**Files:**
- Create: `README.md`
- Create: `LICENSE`
- Create: `config.example.toml`
- Create: `.github/workflows/ci.yml`
- Create: `packaging/homebrew/ytermusic.rb`
- Create: `packaging/scoop/ytermusic.json`
- Create: `packaging/winget/Ytermusic.Ytermusic.yaml`
- Create: `packaging/winget/Ytermusic.Ytermusic.installer.yaml`
- Create: `packaging/winget/Ytermusic.Ytermusic.locale.en-US.yaml`
- Create: `tests/docs.rs`

**Step 1: Write failing documentation assertions**

Test that README contains:

- anonymous quick start;
- macOS, Linux, and Windows dependency installation;
- keyboard reference;
- country chart and podcast usage;
- browser-session privacy explanation;
- config and data paths;
- troubleshooting with `doctor`; and
- explicit statement that Ytermusic is unofficial and does not download media.

Validate example config by deserializing it in `tests/docs.rs`.

**Step 2: Run and confirm RED**

Run: `cargo test --test docs -- --nocapture`  
Expected: FAIL because docs and example config are absent.

**Step 3: Write docs and packaging templates**

Packaging templates must declare `mpv`, `yt-dlp`, and `ffmpeg` dependencies or
print a clear post-install message where a package manager cannot express them.
Use placeholder release checksums only in templates clearly marked for the
release automation to replace; never publish placeholder manifests.

**Step 4: Add CI**

GitHub Actions matrix:

```yaml
strategy:
  fail-fast: false
  matrix:
    os: [ubuntu-latest, macos-latest, windows-latest]
```

Each OS runs `cargo fmt --check`, `cargo clippy --all-targets --all-features --
-D warnings`, `cargo test --all-targets`, and `cargo build --release`. Linux
also runs doc tests and a dependency/security policy job. Cache Cargo registry
and build directories by `Cargo.lock`.

**Step 5: Verify GREEN**

Run: `cargo test --test docs -- --nocapture`  
Expected: PASS.

Run: `cargo fmt --check`  
Run: `cargo clippy --all-targets --all-features -- -D warnings`  
Run: `cargo test --all-targets`  
Run: `cargo build --release`  
Expected: all PASS locally.

**Step 6: Commit**

```bash
git add README.md LICENSE config.example.toml .github packaging tests/docs.rs
git commit -m "docs: add cross-platform install and release setup"
```

### Task 18: Run Live Smoke Tests and Final Release Verification

**Files:**
- Modify only files required by verified defects.
- Create: `docs/release-checklist.md`

**Step 1: Create the release checklist**

Include:

- anonymous song, album, playlist, chart, and podcast smoke tests;
- HK, US, GB, JP, and `ZZ` region checks;
- optional Chrome/Firefox/Safari/Edge credential import;
- play, pause, resume, seek, skip, natural end, shuffle, repeat, and radio;
- fades at 0 ms, 500 ms, and 2,000 ms;
- queue restore and podcast resume;
- dependency missing/dead/restarted scenarios;
- wide, compact, and tiny terminals;
- macOS, Linux, and Windows native verification;
- no secrets in `git grep`, logs, fixtures, or database schema.

**Step 2: Run the offline suite first**

Run:

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
cargo test --doc
cargo build --release
git diff --check
```

Expected: every command exits 0.

**Step 3: Run explicit live smoke tests**

Run only with user-controlled network and credentials:

```bash
YTERMUSIC_LIVE_TESTS=1 cargo test \
  --features live-tests \
  --test provider_live \
  -- --ignored --nocapture
```

Expected: anonymous search, two country charts, and podcast discovery PASS.
Authenticated tests may be skipped when no explicit test credential is
provided.

**Step 4: Perform a local player smoke test**

Run: `cargo run --release`  
Verify one public song and one podcast episode play through `mpv`, transitions
fade, and exit restores the terminal. Do not automate audible output in default
CI.

**Step 5: Audit secret safety**

Run narrowly scoped searches for credential names and ensure every match is
code, documentation, or a scrubbed fixture field—not a value. Inspect the
temporary data directory and SQLite schema. Confirm no resolved media URL is
persisted.

**Step 6: Fix only verified defects using RED/GREEN**

For each defect, first add the smallest failing regression test, confirm the
failure, implement the fix, and rerun the focused plus full suites.

**Step 7: Final commit**

```bash
git add docs/release-checklist.md
git commit -m "test: complete release verification checklist"
```

## Completion Gate

Before declaring completion, use @requesting-code-review and then
@verification-before-completion. Completion requires:

- all offline tests and release builds passing;
- native CI passing on macOS, Linux, and Windows;
- the approved design's eight success criteria demonstrated;
- no raw credentials or stream URLs persisted;
- a clean `git status`; and
- all review findings resolved or explicitly documented.

