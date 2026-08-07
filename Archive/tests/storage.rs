use std::{error::Error, path::Path};

use rusqlite::Connection;
use tempfile::TempDir;
use ytermusic::{
    app::SessionCheckpoint,
    domain::{MediaId, MediaItem, MediaKind, PlaybackSnapshot, PlaybackStatus, RepeatMode},
    queue::{Queue, QueueItem},
    storage::{
        FAVORITES_LIMIT, FavoriteInsertOutcome, HISTORY_LIMIT, PodcastProgress, SqliteStorage,
        Storage, StorageError,
    },
};

fn database_path(directory: &TempDir) -> std::path::PathBuf {
    directory.path().join("ytermusic.sqlite3")
}

fn media(video_id: &str) -> MediaItem {
    MediaItem {
        id: MediaId {
            provider: "youtube-music".to_owned(),
            video_id: video_id.to_owned(),
        },
        kind: MediaKind::Song,
        title: format!("Song {video_id}"),
        creators: vec!["Artist".to_owned()],
        collection: Some("Collection".to_owned()),
        duration_ms: Some(180_000),
        artwork_url: None,
        explicit: false,
    }
}

fn provider_media(provider: &str, video_id: &str) -> MediaItem {
    let mut item = media(video_id);
    provider.clone_into(&mut item.id.provider);
    item
}

fn checkpoint() -> Result<SessionCheckpoint, Box<dyn Error>> {
    let mut queue = Queue::from_items(vec![
        QueueItem::new("one", media("video-one")),
        QueueItem::new("two", media("video-two")),
        QueueItem::new("three", media("video-three")),
    ])?;
    queue.select(&"two".into())?;
    queue.set_repeat(RepeatMode::All);
    queue.set_shuffle(true, 0x00C0_FFEE);
    queue.set_radio(true);

    Ok(SessionCheckpoint {
        queue: queue.snapshot(),
        playback: PlaybackSnapshot {
            current: Some(media("video-two").id),
            status: PlaybackStatus::Paused,
            position_ms: 42_000,
            duration_ms: Some(180_000),
            target_volume: 73,
            playback_speed: 1.25,
        },
    })
}

fn replacement_checkpoint() -> Result<SessionCheckpoint, Box<dyn Error>> {
    let mut queue = Queue::from_items(vec![
        QueueItem::new("replacement-one", media("replacement-one")),
        QueueItem::new("replacement-two", media("replacement-two")),
    ])?;
    queue.select(&"replacement-two".into())?;
    queue.set_repeat(RepeatMode::One);

    Ok(SessionCheckpoint {
        queue: queue.snapshot(),
        playback: PlaybackSnapshot {
            current: Some(media("replacement-two").id),
            status: PlaybackStatus::Playing,
            position_ms: 99_000,
            duration_ms: Some(240_000),
            target_volume: 41,
            playback_speed: 1.75,
        },
    })
}

fn table_names(path: &Path) -> Result<Vec<String>, Box<dyn Error>> {
    let connection = Connection::open(path)?;
    let mut statement = connection.prepare(
        "SELECT name
         FROM sqlite_schema
         WHERE type = 'table' AND name NOT LIKE 'sqlite_%'
         ORDER BY name",
    )?;
    let rows = statement.query_map([], |row| row.get(0))?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

fn assert_schema_v3_corrupt(path: &Path) {
    match SqliteStorage::open(path) {
        Err(StorageError::CorruptData {
            entity: "schema_v3",
            ..
        }) => {}
        Err(other) => panic!("unexpected storage error: {other}"),
        Ok(_) => panic!("a malformed stamped-v3 schema must be rejected"),
    }
}

#[test]
fn favorite_migration_upgrades_v2_and_preserves_existing_data() -> Result<(), Box<dyn Error>> {
    let directory = TempDir::new()?;
    let path = database_path(&directory);
    let connection = Connection::open(&path)?;
    connection.execute_batch(include_str!("../src/storage/schema_v1.sql"))?;
    connection.execute(
        "INSERT INTO schema_migrations(version, applied_at) VALUES (1, 100)",
        [],
    )?;
    connection.execute_batch(include_str!("../src/storage/schema_v2.sql"))?;
    connection.execute(
        "INSERT INTO schema_migrations(version, applied_at) VALUES (2, 200)",
        [],
    )?;
    connection.execute(
        "INSERT INTO metadata_cache(cache_key, payload, expires_at, stored_at)
         VALUES ('kept', 'payload', 500, 300)",
        [],
    )?;
    drop(connection);

    let storage = SqliteStorage::open(&path)?;
    assert_eq!(storage.schema_version()?, 3);
    assert!(storage.load_favorites()?.is_empty());
    assert_eq!(
        storage.get_metadata("kept", 400)?,
        Some("payload".to_owned())
    );

    let connection = Connection::open(path)?;
    let versions = connection
        .prepare("SELECT version FROM schema_migrations ORDER BY version")?
        .query_map([], |row| row.get::<_, i64>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(versions, vec![1, 2, 3]);
    Ok(())
}

#[test]
fn favorite_v3_strictly_validates_table_columns_and_index() -> Result<(), Box<dyn Error>> {
    for mutation in [
        "DROP TABLE favorites",
        "DROP INDEX favorites_newest_first",
        "DROP INDEX favorites_newest_first;
         CREATE INDEX favorites_newest_first ON favorites(favorited_at ASC, id DESC)",
        "ALTER TABLE favorites ADD COLUMN unexpected TEXT",
    ] {
        let directory = TempDir::new()?;
        let path = database_path(&directory);
        drop(SqliteStorage::open(&path)?);
        let connection = Connection::open(&path)?;
        connection.execute_batch(mutation)?;
        drop(connection);
        assert_schema_v3_corrupt(&path);
    }
    Ok(())
}

#[test]
fn favorite_repository_adds_loads_and_removes_full_media_identity() -> Result<(), Box<dyn Error>> {
    let directory = TempDir::new()?;
    let path = database_path(&directory);
    let mut storage = SqliteStorage::open(&path)?;
    let youtube = provider_media("youtube-music", "shared-id");
    let other = provider_media("other-provider", "shared-id");

    assert_eq!(
        storage.add_favorite(&youtube, 100)?,
        FavoriteInsertOutcome::Added
    );
    assert_eq!(
        storage.add_favorite(&other, 200)?,
        FavoriteInsertOutcome::Added
    );
    assert_eq!(
        storage
            .load_favorites()?
            .into_iter()
            .map(|entry| (entry.item.id, entry.favorited_at))
            .collect::<Vec<_>>(),
        vec![(other.id.clone(), 200), (youtube.id.clone(), 100)]
    );

    assert!(storage.remove_favorite(&youtube.id)?);
    assert!(!storage.remove_favorite(&youtube.id)?);
    assert_eq!(storage.load_favorites()?.len(), 1);
    assert_eq!(storage.load_favorites()?[0].item.id, other.id);
    Ok(())
}

#[test]
fn favorite_order_is_deterministic_newest_first_when_timestamps_tie() -> Result<(), Box<dyn Error>>
{
    let directory = TempDir::new()?;
    let path = database_path(&directory);
    let mut storage = SqliteStorage::open(&path)?;

    for video_id in ["first", "second", "third"] {
        assert_eq!(
            storage.add_favorite(&media(video_id), 500)?,
            FavoriteInsertOutcome::Added
        );
    }

    let entries = storage.load_favorites()?;
    assert_eq!(
        entries
            .iter()
            .map(|entry| entry.item.id.video_id.as_str())
            .collect::<Vec<_>>(),
        vec!["third", "second", "first"]
    );
    assert!(entries[0].id > entries[1].id && entries[1].id > entries[2].id);
    Ok(())
}

#[test]
fn favorite_readd_is_idempotent_without_reordering() -> Result<(), Box<dyn Error>> {
    let directory = TempDir::new()?;
    let path = database_path(&directory);
    let mut storage = SqliteStorage::open(&path)?;
    let first = media("first");
    let second = media("second");
    storage.add_favorite(&first, 100)?;
    storage.add_favorite(&second, 200)?;
    let before = storage.load_favorites()?;

    assert_eq!(
        storage.add_favorite(&first, 999)?,
        FavoriteInsertOutcome::AlreadyPresent
    );
    assert_eq!(storage.load_favorites()?, before);
    Ok(())
}

#[test]
fn favorites_persist_across_reopen_and_session_replacement() -> Result<(), Box<dyn Error>> {
    let directory = TempDir::new()?;
    let path = database_path(&directory);
    let favorite = media("independent");
    {
        let mut storage = SqliteStorage::open(&path)?;
        storage.add_favorite(&favorite, 100)?;
        storage.save_session(&checkpoint()?, 200)?;
        storage.save_session(&replacement_checkpoint()?, 300)?;
    }

    let storage = SqliteStorage::open(&path)?;
    assert_eq!(storage.load_favorites()?.len(), 1);
    assert_eq!(storage.load_favorites()?[0].item, favorite);
    assert_eq!(storage.load_session()?, Some(replacement_checkpoint()?));
    Ok(())
}

#[test]
fn favorite_capacity_is_transactional_and_never_evicts() -> Result<(), Box<dyn Error>> {
    let directory = TempDir::new()?;
    let path = database_path(&directory);
    let mut storage = SqliteStorage::open(&path)?;
    for index in 0..FAVORITES_LIMIT {
        assert_eq!(
            storage.add_favorite(&media(&format!("favorite-{index}")), i64::try_from(index)?,)?,
            FavoriteInsertOutcome::Added
        );
    }
    let before = storage.load_favorites()?;

    assert_eq!(
        storage.add_favorite(&media("overflow"), 99_999)?,
        FavoriteInsertOutcome::Full
    );
    assert_eq!(storage.load_favorites()?, before);
    assert_eq!(storage.load_favorites()?.len(), FAVORITES_LIMIT);
    Ok(())
}

#[test]
fn load_favorites_rejects_malformed_json() -> Result<(), Box<dyn Error>> {
    let directory = TempDir::new()?;
    let path = database_path(&directory);
    drop(SqliteStorage::open(&path)?);
    let connection = Connection::open(&path)?;
    connection.execute(
        "INSERT INTO favorites(provider, video_id, item_json, favorited_at)
         VALUES ('youtube-music', 'malformed', '{', 1)",
        [],
    )?;
    drop(connection);

    assert!(matches!(
        SqliteStorage::open(&path)?.load_favorites(),
        Err(StorageError::Json(_))
    ));
    Ok(())
}

#[test]
fn load_favorites_rejects_relational_and_serialized_identity_disagreement()
-> Result<(), Box<dyn Error>> {
    let directory = TempDir::new()?;
    let path = database_path(&directory);
    let item_json = serde_json::to_string(&provider_media("serialized-provider", "serialized-id"))?;
    drop(SqliteStorage::open(&path)?);
    let connection = Connection::open(&path)?;
    connection.execute(
        "INSERT INTO favorites(provider, video_id, item_json, favorited_at)
         VALUES ('relational-provider', 'relational-id', ?1, 1)",
        [item_json],
    )?;
    drop(connection);

    assert!(matches!(
        SqliteStorage::open(&path)?.load_favorites(),
        Err(StorageError::CorruptData {
            entity: "favorites",
            ..
        })
    ));
    Ok(())
}

fn assert_invalid_checkpoint_preserves_saved_session(
    invalid: &SessionCheckpoint,
) -> Result<(), Box<dyn Error>> {
    let directory = TempDir::new()?;
    let path = database_path(&directory);
    let expected = checkpoint()?;
    let mut storage = SqliteStorage::open(&path)?;
    storage.save_session(&expected, 1_000)?;

    match storage.save_session(invalid, 2_000) {
        Err(StorageError::CorruptData {
            entity: "session_state",
            ..
        }) => {}
        Err(other) => panic!("unexpected storage error: {other}"),
        Ok(()) => panic!("invalid checkpoint must be rejected before persistence"),
    }
    assert_eq!(storage.load_session()?, Some(expected.clone()));

    let connection = Connection::open(path)?;
    let (payload, updated_at): (String, i64) = connection.query_row(
        "SELECT payload, updated_at FROM session_state WHERE singleton = 1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    assert_eq!(
        serde_json::from_str::<SessionCheckpoint>(&payload)?,
        expected
    );
    assert_eq!(updated_at, 1_000);
    Ok(())
}

#[test]
fn malformed_partial_schema_is_rejected_without_recording_v1() -> Result<(), Box<dyn Error>> {
    let directory = TempDir::new()?;
    let path = database_path(&directory);
    let connection = Connection::open(&path)?;
    connection.execute(
        "CREATE TABLE session_state (
            payload TEXT NOT NULL
        )",
        [],
    )?;
    drop(connection);

    match SqliteStorage::open(&path) {
        Err(StorageError::Database(_)) => {}
        Err(other) => panic!("unexpected storage error: {other}"),
        Ok(_) => panic!("a partial schema must not be stamped as version 1"),
    }

    let connection = Connection::open(&path)?;
    let migration_table_exists: bool = connection.query_row(
        "SELECT EXISTS(
            SELECT 1
            FROM sqlite_schema
            WHERE type = 'table' AND name = 'schema_migrations'
        )",
        [],
        |row| row.get(0),
    )?;
    assert!(
        !migration_table_exists,
        "the failed migration transaction must not leave a version table"
    );
    Ok(())
}

#[test]
fn empty_well_shaped_migration_ledger_is_typed_corruption() -> Result<(), Box<dyn Error>> {
    let directory = TempDir::new()?;
    let path = database_path(&directory);
    let connection = Connection::open(&path)?;
    connection.execute_batch(
        "CREATE TABLE schema_migrations (
            version INTEGER PRIMARY KEY,
            applied_at INTEGER NOT NULL
        );",
    )?;
    drop(connection);

    match SqliteStorage::open(&path) {
        Err(StorageError::CorruptData {
            entity: "schema_migrations",
            ..
        }) => {}
        Err(other) => panic!("unexpected storage error: {other}"),
        Ok(_) => panic!("an empty migration ledger must be rejected"),
    }
    Ok(())
}

#[test]
fn malformed_migration_table_with_v1_record_is_rejected() -> Result<(), Box<dyn Error>> {
    let directory = TempDir::new()?;
    let path = database_path(&directory);
    let connection = Connection::open(&path)?;
    connection.execute_batch(
        "CREATE TABLE schema_migrations (
            version INTEGER PRIMARY KEY
        );
        INSERT INTO schema_migrations(version) VALUES (1);",
    )?;
    drop(connection);

    match SqliteStorage::open(&path) {
        Err(StorageError::CorruptData {
            entity: "schema_migrations",
            ..
        }) => {}
        Err(other) => panic!("unexpected storage error: {other}"),
        Ok(_) => panic!("a malformed migration table must not be accepted"),
    }
    Ok(())
}

#[test]
fn stamped_v3_missing_required_table_is_rejected_on_open() -> Result<(), Box<dyn Error>> {
    let directory = TempDir::new()?;
    let path = database_path(&directory);
    drop(SqliteStorage::open(&path)?);
    let connection = Connection::open(&path)?;
    connection.execute("DROP TABLE metadata_cache", [])?;
    drop(connection);

    assert_schema_v3_corrupt(&path);
    Ok(())
}

#[test]
fn stamped_v3_wrong_required_columns_are_rejected_on_open() -> Result<(), Box<dyn Error>> {
    let directory = TempDir::new()?;
    let path = database_path(&directory);
    drop(SqliteStorage::open(&path)?);
    let connection = Connection::open(&path)?;
    connection.execute_batch(
        "DROP TABLE metadata_cache;
         CREATE TABLE metadata_cache (
             cache_key TEXT PRIMARY KEY,
             payload TEXT NOT NULL,
             expires_at INTEGER NOT NULL
         );",
    )?;
    drop(connection);

    assert_schema_v3_corrupt(&path);
    Ok(())
}

#[test]
fn stamped_v3_missing_singleton_check_is_rejected_on_open() -> Result<(), Box<dyn Error>> {
    let directory = TempDir::new()?;
    let path = database_path(&directory);
    drop(SqliteStorage::open(&path)?);
    let connection = Connection::open(&path)?;
    connection.execute_batch(
        "DROP TABLE session_state;
         CREATE TABLE session_state (
             singleton INTEGER PRIMARY KEY,
             payload TEXT NOT NULL,
             updated_at INTEGER NOT NULL
         );",
    )?;
    drop(connection);

    assert_schema_v3_corrupt(&path);
    Ok(())
}

#[test]
fn stamped_v3_missing_history_autoincrement_is_rejected_on_open() -> Result<(), Box<dyn Error>> {
    let directory = TempDir::new()?;
    let path = database_path(&directory);
    drop(SqliteStorage::open(&path)?);
    let connection = Connection::open(&path)?;
    connection.execute_batch(
        "DROP TABLE listening_history;
         CREATE TABLE listening_history (
             id INTEGER PRIMARY KEY,
             video_id TEXT NOT NULL,
             item_json TEXT NOT NULL,
             played_at INTEGER NOT NULL
         );
         CREATE INDEX history_played_at
             ON listening_history(played_at DESC);",
    )?;
    drop(connection);

    assert_schema_v3_corrupt(&path);
    Ok(())
}

#[test]
fn stamped_v3_wrong_history_index_direction_is_rejected_on_open() -> Result<(), Box<dyn Error>> {
    let directory = TempDir::new()?;
    let path = database_path(&directory);
    drop(SqliteStorage::open(&path)?);
    let connection = Connection::open(&path)?;
    connection.execute_batch(
        "DROP INDEX history_played_at;
         CREATE INDEX history_played_at
             ON listening_history(played_at ASC);",
    )?;
    drop(connection);

    assert_schema_v3_corrupt(&path);
    Ok(())
}

#[test]
fn stamped_v3_unknown_trigger_is_rejected_on_open() -> Result<(), Box<dyn Error>> {
    let directory = TempDir::new()?;
    let path = database_path(&directory);
    drop(SqliteStorage::open(&path)?);
    let connection = Connection::open(&path)?;
    connection.execute_batch(
        "CREATE TRIGGER alter_session_timestamp
         AFTER INSERT ON session_state
         BEGIN
             UPDATE session_state SET updated_at = 0;
         END;",
    )?;
    drop(connection);

    assert_schema_v3_corrupt(&path);
    Ok(())
}

#[test]
fn new_database_has_schema_v3_connection_pragmas_and_empty_reads() -> Result<(), Box<dyn Error>> {
    let directory = TempDir::new()?;
    let path = database_path(&directory);
    let mut storage = SqliteStorage::open(&path)?;

    assert_eq!(storage.schema_version()?, 3);
    let settings = storage.connection_settings()?;
    assert_eq!(settings.journal_mode.to_ascii_lowercase(), "wal");
    assert!(settings.foreign_keys);
    assert!(settings.busy_timeout_ms > 0);
    assert_eq!(
        table_names(&path)?,
        vec![
            "favorites",
            "listening_history",
            "metadata_cache",
            "podcast_progress",
            "schema_migrations",
            "session_state",
        ]
    );

    assert_eq!(storage.load_session()?, None);
    assert_eq!(storage.load_podcast_progress("missing")?, None);
    assert!(storage.recent_history(10)?.is_empty());
    assert!(storage.load_favorites()?.is_empty());
    assert_eq!(storage.get_metadata("missing", 100)?, None);

    let object_safe_storage: &mut dyn Storage = &mut storage;
    assert_eq!(object_safe_storage.load_session()?, None);
    Ok(())
}

#[test]
fn invalid_queue_checkpoint_is_rejected_without_replacing_saved_session()
-> Result<(), Box<dyn Error>> {
    let mut invalid = checkpoint()?;
    invalid.queue.logical.push(invalid.queue.logical[0].clone());

    assert_invalid_checkpoint_preserves_saved_session(&invalid)
}

#[test]
fn mismatched_playback_current_is_rejected_without_replacing_saved_session()
-> Result<(), Box<dyn Error>> {
    let mut invalid = checkpoint()?;
    invalid.playback.current = Some(media("video-one").id);

    assert_invalid_checkpoint_preserves_saved_session(&invalid)
}

#[test]
fn invalid_target_volume_is_rejected_without_replacing_saved_session() -> Result<(), Box<dyn Error>>
{
    let mut invalid = checkpoint()?;
    invalid.playback.target_volume = 101;

    assert_invalid_checkpoint_preserves_saved_session(&invalid)
}

#[test]
fn invalid_playback_speeds_are_rejected_without_replacing_saved_session()
-> Result<(), Box<dyn Error>> {
    for speed in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 0.49, 3.01] {
        let mut invalid = checkpoint()?;
        invalid.playback.playback_speed = speed;
        assert_invalid_checkpoint_preserves_saved_session(&invalid)?;
    }
    Ok(())
}

#[test]
fn decoded_checkpoint_is_validated_when_loaded() -> Result<(), Box<dyn Error>> {
    let directory = TempDir::new()?;
    let path = database_path(&directory);
    let mut storage = SqliteStorage::open(&path)?;
    storage.save_session(&checkpoint()?, 1_000)?;

    let mut corrupt = checkpoint()?;
    corrupt.playback.target_volume = 101;
    let payload = serde_json::to_string(&corrupt)?;
    let connection = Connection::open(path)?;
    connection.execute(
        "UPDATE session_state SET payload = ?1 WHERE singleton = 1",
        [payload],
    )?;

    match storage.load_session() {
        Err(StorageError::CorruptData {
            entity: "session_state",
            ..
        }) => {}
        Err(other) => panic!("unexpected storage error: {other}"),
        Ok(_) => panic!("decoded checkpoint invariants must be validated"),
    }
    Ok(())
}

#[test]
fn saving_session_twice_replaces_the_singleton_payload_and_timestamp() -> Result<(), Box<dyn Error>>
{
    let directory = TempDir::new()?;
    let path = database_path(&directory);
    let mut storage = SqliteStorage::open(&path)?;
    let first = checkpoint()?;
    let replacement = replacement_checkpoint()?;

    storage.save_session(&first, 1_000)?;
    storage.save_session(&replacement, 2_000)?;
    assert_eq!(storage.load_session()?, Some(replacement));

    let connection = Connection::open(path)?;
    let (row_count, updated_at): (i64, i64) = connection.query_row(
        "SELECT COUNT(*), MAX(updated_at) FROM session_state",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    assert_eq!(row_count, 1);
    assert_eq!(updated_at, 2_000);
    Ok(())
}

#[test]
fn reopen_is_idempotent_and_session_checkpoint_round_trips_completely() -> Result<(), Box<dyn Error>>
{
    let directory = TempDir::new()?;
    let path = database_path(&directory);
    let expected = checkpoint()?;

    {
        let mut storage = SqliteStorage::open(&path)?;
        storage.save_session(&expected, 1_000)?;
    }

    let storage = SqliteStorage::open(&path)?;
    assert_eq!(storage.schema_version()?, 3);
    assert_eq!(storage.load_session()?, Some(expected));
    let reopened_settings = storage.connection_settings()?;
    assert!(reopened_settings.foreign_keys);
    assert!(reopened_settings.busy_timeout_ms > 0);

    let connection = Connection::open(&path)?;
    let migration_count: i64 =
        connection.query_row("SELECT COUNT(*) FROM schema_migrations", [], |row| {
            row.get(0)
        })?;
    assert_eq!(migration_count, 3);
    Ok(())
}

#[test]
fn newer_schema_version_is_rejected_safely() -> Result<(), Box<dyn Error>> {
    let directory = TempDir::new()?;
    let path = database_path(&directory);
    let connection = Connection::open(&path)?;
    connection.execute_batch(
        "CREATE TABLE schema_migrations (
            version INTEGER PRIMARY KEY,
            applied_at INTEGER NOT NULL
        );
        INSERT INTO schema_migrations(version, applied_at) VALUES (4, 0);",
    )?;
    drop(connection);

    match SqliteStorage::open(&path) {
        Err(StorageError::UnsupportedSchemaVersion {
            found: 4,
            supported: 3,
        }) => {}
        Err(other) => panic!("unexpected storage error: {other}"),
        Ok(_) => panic!("newer schema must be rejected"),
    }
    Ok(())
}

#[test]
fn v1_podcast_progress_migrates_to_epoch_zero() -> Result<(), Box<dyn Error>> {
    let directory = TempDir::new()?;
    let path = database_path(&directory);
    let connection = Connection::open(&path)?;
    connection.execute_batch(include_str!("../src/storage/schema_v1.sql"))?;
    connection.execute(
        "INSERT INTO schema_migrations(version, applied_at) VALUES (1, 100)",
        [],
    )?;
    connection.execute(
        "INSERT INTO podcast_progress(
             video_id, position_ms, duration_ms, played, updated_at
         ) VALUES ('migrated-episode', 90000, 90000, 1, 200)",
        [],
    )?;
    drop(connection);

    let storage = SqliteStorage::open(&path)?;
    assert_eq!(storage.schema_version()?, 3);
    assert_eq!(
        storage.load_podcast_progress("migrated-episode")?,
        Some(PodcastProgress {
            video_id: "migrated-episode".to_owned(),
            playback_epoch: 0,
            position_ms: 90_000,
            duration_ms: Some(90_000),
            played: true,
            updated_at: 200,
        })
    );

    let connection = Connection::open(path)?;
    let versions = connection
        .prepare("SELECT version FROM schema_migrations ORDER BY version")?
        .query_map([], |row| row.get::<_, i64>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(versions, vec![1, 2, 3]);
    Ok(())
}

#[test]
fn newer_epoch_resets_completed_progress_and_ignores_delayed_old_attempts()
-> Result<(), Box<dyn Error>> {
    let directory = TempDir::new()?;
    let path = database_path(&directory);
    {
        let mut storage = SqliteStorage::open(&path)?;
        storage.save_podcast_progress(&PodcastProgress {
            video_id: "replayed-episode".to_owned(),
            playback_epoch: 7,
            position_ms: 100_000,
            duration_ms: Some(100_000),
            played: true,
            updated_at: 100,
        })?;
        storage.save_podcast_progress(&PodcastProgress {
            video_id: "replayed-episode".to_owned(),
            playback_epoch: 8,
            position_ms: 0,
            duration_ms: Some(100_000),
            played: false,
            updated_at: 200,
        })?;
        storage.save_podcast_progress(&PodcastProgress {
            video_id: "replayed-episode".to_owned(),
            playback_epoch: 8,
            position_ms: 40_000,
            duration_ms: Some(100_000),
            played: false,
            updated_at: 300,
        })?;
        storage.save_podcast_progress(&PodcastProgress {
            video_id: "replayed-episode".to_owned(),
            playback_epoch: 7,
            position_ms: 100_000,
            duration_ms: Some(100_000),
            played: true,
            updated_at: 999,
        })?;
        storage.save_podcast_progress(&PodcastProgress {
            video_id: "replayed-episode".to_owned(),
            playback_epoch: 8,
            position_ms: 10_000,
            duration_ms: Some(90_000),
            played: false,
            updated_at: 250,
        })?;
    }

    let storage = SqliteStorage::open(path)?;
    assert_eq!(
        storage.load_podcast_progress("replayed-episode")?,
        Some(PodcastProgress {
            video_id: "replayed-episode".to_owned(),
            playback_epoch: 8,
            position_ms: 40_000,
            duration_ms: Some(100_000),
            played: false,
            updated_at: 300,
        })
    );
    Ok(())
}

#[test]
fn podcast_progress_upsert_is_monotonic_for_delayed_updates() -> Result<(), Box<dyn Error>> {
    let directory = TempDir::new()?;
    let mut storage = SqliteStorage::open(database_path(&directory))?;

    storage.save_podcast_progress(&PodcastProgress {
        video_id: "episode".to_owned(),
        playback_epoch: 1,
        position_ms: 50_000,
        duration_ms: Some(100_000),
        played: false,
        updated_at: 1_000,
    })?;
    storage.save_podcast_progress(&PodcastProgress {
        video_id: "episode".to_owned(),
        playback_epoch: 1,
        position_ms: 20_000,
        duration_ms: Some(90_000),
        played: false,
        updated_at: 900,
    })?;
    assert_eq!(
        storage.load_podcast_progress("episode")?,
        Some(PodcastProgress {
            video_id: "episode".to_owned(),
            playback_epoch: 1,
            position_ms: 50_000,
            duration_ms: Some(100_000),
            played: false,
            updated_at: 1_000,
        })
    );

    storage.save_podcast_progress(&PodcastProgress {
        video_id: "episode".to_owned(),
        playback_epoch: 1,
        position_ms: 40_000,
        duration_ms: Some(120_000),
        played: true,
        updated_at: 1_100,
    })?;
    assert_eq!(
        storage.load_podcast_progress("episode")?,
        Some(PodcastProgress {
            video_id: "episode".to_owned(),
            playback_epoch: 1,
            position_ms: 50_000,
            duration_ms: Some(120_000),
            played: true,
            updated_at: 1_100,
        })
    );

    storage.save_podcast_progress(&PodcastProgress {
        video_id: "episode".to_owned(),
        playback_epoch: 1,
        position_ms: 10,
        duration_ms: None,
        played: false,
        updated_at: 800,
    })?;
    assert_eq!(
        storage.load_podcast_progress("episode")?,
        Some(PodcastProgress {
            video_id: "episode".to_owned(),
            playback_epoch: 1,
            position_ms: 50_000,
            duration_ms: Some(120_000),
            played: true,
            updated_at: 1_100,
        })
    );
    Ok(())
}

#[test]
fn history_is_newest_first_deterministic_and_capped() -> Result<(), Box<dyn Error>> {
    let directory = TempDir::new()?;
    let mut storage = SqliteStorage::open(database_path(&directory))?;

    storage.record_history(&media("older"), 100)?;
    storage.record_history(&media("same-time-first"), 200)?;
    storage.record_history(&media("same-time-second"), 200)?;
    let recent = storage.recent_history(2)?;
    assert_eq!(recent.len(), 2);
    assert_eq!(recent[0].item.id.video_id, "same-time-second");
    assert_eq!(recent[1].item.id.video_id, "same-time-first");

    for index in 0..=HISTORY_LIMIT {
        storage.record_history(
            &media(&format!("bulk-{index}")),
            1_000 + i64::try_from(index)?,
        )?;
    }
    let all = storage.recent_history(HISTORY_LIMIT + 10)?;
    assert_eq!(all.len(), HISTORY_LIMIT);
    assert_eq!(
        all.first().map(|entry| entry.item.id.video_id.as_str()),
        Some(format!("bulk-{HISTORY_LIMIT}").as_str())
    );
    assert_eq!(
        all.last().map(|entry| entry.item.id.video_id.as_str()),
        Some("bulk-1")
    );
    Ok(())
}

#[test]
fn metadata_cache_returns_live_payload_and_misses_at_expiry() -> Result<(), Box<dyn Error>> {
    let directory = TempDir::new()?;
    let mut storage = SqliteStorage::open(database_path(&directory))?;
    let payload = r#"{"country":"JP","items":[1,2,3]}"#;

    storage.put_metadata("charts:JP", payload, 2_000, 1_000)?;
    assert_eq!(
        storage.get_metadata("charts:JP", 1_999)?,
        Some(payload.to_owned())
    );
    assert_eq!(storage.get_metadata("charts:JP", 2_000)?, None);
    assert_eq!(storage.get_metadata("charts:JP", 2_001)?, None);
    Ok(())
}

#[test]
fn metadata_cache_entry_preserves_expired_payload_and_provenance() -> Result<(), Box<dyn Error>> {
    let directory = TempDir::new()?;
    let mut storage = SqliteStorage::open(database_path(&directory))?;
    let payload = r#"{"country":"US","items":[1]}"#;
    storage.put_metadata("charts:US", payload, 2_000, 1_000)?;

    let Some(entry) = storage.get_metadata_entry("charts:US")? else {
        panic!("stored metadata entry");
    };
    assert_eq!(entry.payload(), payload);
    assert_eq!(entry.expires_at(), 2_000);
    assert_eq!(entry.stored_at(), 1_000);
    assert!(!entry.stale_at(1_999));
    assert!(entry.stale_at(2_000));
    assert!(entry.stale_at(2_001));
    assert_eq!(storage.get_metadata_entry("missing")?, None);
    Ok(())
}

#[test]
fn metadata_cache_rejects_text_expiry_as_typed_corruption() -> Result<(), Box<dyn Error>> {
    let directory = TempDir::new()?;
    let path = database_path(&directory);
    let storage = SqliteStorage::open(&path)?;
    let connection = Connection::open(path)?;
    connection.execute(
        "INSERT INTO metadata_cache(cache_key, payload, expires_at, stored_at)
         VALUES ('bad-expiry', 'payload', 'not-an-integer', 100)",
        [],
    )?;

    match storage.get_metadata("bad-expiry", 1_000) {
        Err(StorageError::CorruptData {
            entity: "metadata_cache",
            ..
        }) => {}
        Err(other) => panic!("unexpected storage error: {other}"),
        Ok(_) => panic!("a non-integer cache expiry must be rejected"),
    }
    Ok(())
}

#[test]
fn metadata_cache_rejects_text_storage_time_as_typed_corruption() -> Result<(), Box<dyn Error>> {
    let directory = TempDir::new()?;
    let path = database_path(&directory);
    let storage = SqliteStorage::open(&path)?;
    let connection = Connection::open(path)?;
    connection.execute(
        "INSERT INTO metadata_cache(cache_key, payload, expires_at, stored_at)
         VALUES ('bad-stored-at', 'payload', 2000, 'not-an-integer')",
        [],
    )?;

    match storage.get_metadata("bad-stored-at", 1_000) {
        Err(StorageError::CorruptData {
            entity: "metadata_cache",
            ..
        }) => {}
        Err(other) => panic!("unexpected storage error: {other}"),
        Ok(_) => panic!("a non-integer cache storage time must be rejected"),
    }
    Ok(())
}

#[test]
fn schema_identifiers_do_not_contain_sensitive_names() -> Result<(), Box<dyn Error>> {
    let directory = TempDir::new()?;
    let path = database_path(&directory);
    let _storage = SqliteStorage::open(&path)?;
    let connection = Connection::open(path)?;

    let mut statement = connection.prepare(
        "SELECT name
         FROM sqlite_schema
         WHERE type IN ('table', 'index') AND name NOT LIKE 'sqlite_%'",
    )?;
    let names = statement.query_map([], |row| row.get::<_, String>(0))?;
    let mut identifiers = names.collect::<Result<Vec<_>, _>>()?;

    for table in table_names(directory.path().join("ytermusic.sqlite3").as_path())? {
        let quoted_table = format!("'{}'", table.replace('\'', "''"));
        let mut columns = connection.prepare(&format!("PRAGMA table_info({quoted_table})"))?;
        identifiers.extend(
            columns
                .query_map([], |row| row.get::<_, String>(1))?
                .collect::<Result<Vec<_>, _>>()?,
        );
    }

    for identifier in identifiers {
        let normalized = identifier.to_ascii_lowercase();
        assert!(
            !["cookie", "authorization", "secret"]
                .iter()
                .any(|word| normalized.contains(word)),
            "sensitive identifier found: {identifier}"
        );
    }
    Ok(())
}

#[test]
fn out_of_range_inputs_and_corrupt_rows_return_typed_errors() -> Result<(), Box<dyn Error>> {
    let directory = TempDir::new()?;
    let path = database_path(&directory);
    let mut storage = SqliteStorage::open(&path)?;

    match storage.save_podcast_progress(&PodcastProgress {
        video_id: "too-large".to_owned(),
        playback_epoch: 0,
        position_ms: u64::MAX,
        duration_ms: None,
        played: false,
        updated_at: 1,
    }) {
        Err(StorageError::IntegerOutOfRange {
            field: "podcast position_ms",
            ..
        }) => {}
        Err(other) => panic!("unexpected storage error: {other}"),
        Ok(()) => panic!("out-of-range progress must be rejected"),
    }
    match storage.save_podcast_progress(&PodcastProgress {
        video_id: "epoch-too-large".to_owned(),
        playback_epoch: (i64::MAX as u64) + 1,
        position_ms: 0,
        duration_ms: None,
        played: false,
        updated_at: 1,
    }) {
        Err(StorageError::IntegerOutOfRange {
            field: "podcast playback_epoch",
            ..
        }) => {}
        Err(other) => panic!("unexpected storage error: {other}"),
        Ok(()) => panic!("out-of-range playback epoch must be rejected"),
    }

    let connection = Connection::open(&path)?;
    connection.execute(
        "INSERT INTO listening_history(video_id, item_json, played_at)
         VALUES ('broken', '{not-json', 1)",
        [],
    )?;
    connection.execute(
        "INSERT INTO podcast_progress(
             video_id, position_ms, duration_ms, played, updated_at
         ) VALUES ('negative', -1, NULL, 0, 1)",
        [],
    )?;
    connection.execute(
        "INSERT INTO podcast_progress(
             video_id, playback_epoch, position_ms, duration_ms, played, updated_at
         ) VALUES ('negative-epoch', -1, 0, NULL, 0, 1)",
        [],
    )?;

    match storage.recent_history(1) {
        Err(StorageError::Json(_)) => {}
        Err(other) => panic!("unexpected storage error: {other}"),
        Ok(_) => panic!("corrupt history JSON must be rejected"),
    }
    match storage.load_podcast_progress("negative") {
        Err(StorageError::CorruptData { .. }) => {}
        Err(other) => panic!("unexpected storage error: {other}"),
        Ok(_) => panic!("negative progress must be rejected"),
    }
    match storage.load_podcast_progress("negative-epoch") {
        Err(StorageError::CorruptData { .. }) => {}
        Err(other) => panic!("unexpected storage error: {other}"),
        Ok(_) => panic!("negative playback epoch must be rejected"),
    }
    Ok(())
}
