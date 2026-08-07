use std::{
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params, types::Value};

use crate::{
    app::SessionCheckpoint,
    config::Config,
    domain::{MediaId, MediaItem},
    queue::Queue,
};

use super::{StorageError, migrations};

pub const HISTORY_LIMIT: usize = 5_000;
pub const FAVORITES_LIMIT: usize = 1_024;
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PodcastProgress {
    pub video_id: String,
    pub playback_epoch: u64,
    pub position_ms: u64,
    pub duration_ms: Option<u64>,
    pub played: bool,
    pub updated_at: i64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct HistoryEntry {
    pub id: i64,
    pub item: MediaItem,
    pub played_at: i64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FavoriteEntry {
    pub id: i64,
    pub item: MediaItem,
    pub favorited_at: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FavoriteInsertOutcome {
    Added,
    AlreadyPresent,
    Full,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MetadataCacheEntry {
    payload: String,
    expires_at: i64,
    stored_at: i64,
}

impl MetadataCacheEntry {
    #[must_use]
    pub fn payload(&self) -> &str {
        &self.payload
    }

    #[must_use]
    pub const fn expires_at(&self) -> i64 {
        self.expires_at
    }

    #[must_use]
    pub const fn stored_at(&self) -> i64 {
        self.stored_at
    }

    #[must_use]
    pub const fn stale_at(&self, now: i64) -> bool {
        self.expires_at <= now
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConnectionSettings {
    pub journal_mode: String,
    pub foreign_keys: bool,
    pub busy_timeout_ms: u64,
}

/// Provider-neutral persistence operations used by the application runtime.
pub trait Storage: Send {
    /// Saves the latest queue and playback checkpoint as the singleton session.
    ///
    /// # Errors
    ///
    /// Returns a typed error if the checkpoint cannot be serialized or saved.
    fn save_session(
        &mut self,
        checkpoint: &SessionCheckpoint,
        updated_at: i64,
    ) -> Result<(), StorageError>;

    /// Loads the saved session, or `None` when no checkpoint exists.
    ///
    /// # Errors
    ///
    /// Returns a typed error for database failures or corrupt persisted state.
    fn load_session(&self) -> Result<Option<SessionCheckpoint>, StorageError>;

    /// Merges progress for one podcast episode using playback-attempt ordering.
    ///
    /// A newer playback epoch starts a fresh position and played state. Within
    /// one epoch, position, played state, and update timestamp never move
    /// backward. Updates from older epochs are ignored. Duration retains the
    /// greatest non-`None` observation accepted by epoch ordering.
    ///
    /// # Errors
    ///
    /// Returns a typed error if an integer is out of range or the row cannot be
    /// saved.
    fn save_podcast_progress(&mut self, progress: &PodcastProgress) -> Result<(), StorageError>;

    /// Loads progress for one podcast episode.
    ///
    /// # Errors
    ///
    /// Returns a typed error for database failures or invalid stored values.
    fn load_podcast_progress(
        &self,
        video_id: &str,
    ) -> Result<Option<PodcastProgress>, StorageError>;

    /// Adds a normalized media item to listening history and enforces the cap.
    ///
    /// # Errors
    ///
    /// Returns a typed error if the item cannot be serialized or the
    /// transactional update fails.
    fn record_history(&mut self, item: &MediaItem, played_at: i64) -> Result<(), StorageError>;

    /// Returns at most `limit` history entries in newest-first order.
    ///
    /// # Errors
    ///
    /// Returns a typed error for an out-of-range limit, a database failure, or
    /// corrupt stored media.
    fn recent_history(&self, limit: usize) -> Result<Vec<HistoryEntry>, StorageError>;

    /// Returns all favorites in deterministic newest-first order.
    ///
    /// # Errors
    ///
    /// Returns a typed error for database failures or corrupt stored media.
    fn load_favorites(&self) -> Result<Vec<FavoriteEntry>, StorageError>;

    /// Adds a favorite without exceeding the fixed local capacity.
    ///
    /// The identity check, capacity check, and insertion are performed in one
    /// transaction. Re-adding an existing identity is a no-op.
    ///
    /// # Errors
    ///
    /// Returns a typed error if the item cannot be serialized or the
    /// transaction fails. Capacity is reported as a normal outcome.
    fn add_favorite(
        &mut self,
        item: &MediaItem,
        favorited_at: i64,
    ) -> Result<FavoriteInsertOutcome, StorageError>;

    /// Removes the favorite with the complete provider/media identity.
    ///
    /// Returns whether a row was removed.
    ///
    /// # Errors
    ///
    /// Returns a typed error if the delete cannot be completed.
    fn remove_favorite(&mut self, id: &MediaId) -> Result<bool, StorageError>;

    /// Inserts or replaces a provider-neutral metadata cache value.
    ///
    /// # Errors
    ///
    /// Returns a typed error if the cache row cannot be saved.
    fn put_metadata(
        &mut self,
        cache_key: &str,
        payload: &str,
        expires_at: i64,
        stored_at: i64,
    ) -> Result<(), StorageError>;

    /// Returns cache data with its freshness timestamps, including expired
    /// entries that may be used as an explicitly stale offline fallback.
    ///
    /// # Errors
    ///
    /// Returns a typed error if the cache row cannot be queried.
    fn get_metadata_entry(
        &self,
        _cache_key: &str,
    ) -> Result<Option<MetadataCacheEntry>, StorageError> {
        Ok(None)
    }

    /// Returns a live cached payload, treating its expiry instant as expired.
    ///
    /// # Errors
    ///
    /// Returns a typed error if the cache row cannot be queried.
    fn get_metadata(&self, cache_key: &str, now: i64) -> Result<Option<String>, StorageError>;
}

pub struct SqliteStorage {
    connection: Connection,
}

impl SqliteStorage {
    /// Opens a `SQLite` database, configures the connection, and applies all
    /// supported migrations.
    ///
    /// # Errors
    ///
    /// Returns a typed storage error when the database cannot be opened or
    /// configured, a migration fails, or the database was written by a newer
    /// version of the application.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        let mut connection = Connection::open(path)?;
        configure_connection(&connection)?;
        migrations::run(&mut connection, current_timestamp_ms()?)?;
        Ok(Self { connection })
    }

    /// Returns the highest applied migration version.
    ///
    /// # Errors
    ///
    /// Returns a database error if migration metadata cannot be read.
    pub fn schema_version(&self) -> Result<i64, StorageError> {
        Ok(self.connection.query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
            [],
            |row| row.get(0),
        )?)
    }

    /// Reports settings for this connection, primarily for diagnostics.
    ///
    /// # Errors
    ///
    /// Returns a typed error if a pragma cannot be queried or contains an
    /// invalid value.
    pub fn connection_settings(&self) -> Result<ConnectionSettings, StorageError> {
        let journal_mode = self
            .connection
            .pragma_query_value(None, "journal_mode", |row| row.get::<_, String>(0))?;
        let foreign_keys_value =
            self.connection
                .pragma_query_value(None, "foreign_keys", |row| row.get::<_, i64>(0))?;
        let foreign_keys = sqlite_boolean(foreign_keys_value, "PRAGMA foreign_keys")?;
        let busy_timeout = self
            .connection
            .pragma_query_value(None, "busy_timeout", |row| row.get::<_, i64>(0))?;

        Ok(ConnectionSettings {
            journal_mode,
            foreign_keys,
            busy_timeout_ms: nonnegative_i64_to_u64(busy_timeout, "busy_timeout")?,
        })
    }

    fn add_favorite_with_capacity_observer(
        &mut self,
        item: &MediaItem,
        favorited_at: i64,
        after_capacity_check: impl FnOnce(),
    ) -> Result<FavoriteInsertOutcome, StorageError> {
        let item_json = serde_json::to_string(item)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let already_present = transaction.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM favorites WHERE provider = ?1 AND video_id = ?2
             )",
            params![item.id.provider, item.id.video_id],
            |row| row.get::<_, bool>(0),
        )?;
        if already_present {
            transaction.commit()?;
            return Ok(FavoriteInsertOutcome::AlreadyPresent);
        }

        let favorite_count =
            transaction.query_row("SELECT COUNT(*) FROM favorites", [], |row| {
                row.get::<_, i64>(0)
            })?;
        let limit =
            i64::try_from(FAVORITES_LIMIT).map_err(|_| StorageError::IntegerOutOfRange {
                field: "favorites limit",
                value: FAVORITES_LIMIT as u128,
            })?;
        if favorite_count >= limit {
            transaction.commit()?;
            return Ok(FavoriteInsertOutcome::Full);
        }

        after_capacity_check();
        transaction.execute(
            "INSERT INTO favorites(provider, video_id, item_json, favorited_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![item.id.provider, item.id.video_id, item_json, favorited_at],
        )?;
        transaction.commit()?;
        Ok(FavoriteInsertOutcome::Added)
    }
}

impl Storage for SqliteStorage {
    fn save_session(
        &mut self,
        checkpoint: &SessionCheckpoint,
        updated_at: i64,
    ) -> Result<(), StorageError> {
        validate_checkpoint(checkpoint)?;
        let payload = serde_json::to_string(checkpoint)?;
        self.connection.execute(
            "INSERT INTO session_state(singleton, payload, updated_at)
             VALUES (1, ?1, ?2)
             ON CONFLICT(singleton) DO UPDATE SET
                 payload = excluded.payload,
                 updated_at = excluded.updated_at",
            params![payload, updated_at],
        )?;
        Ok(())
    }

    fn load_session(&self) -> Result<Option<SessionCheckpoint>, StorageError> {
        let payload = self
            .connection
            .query_row(
                "SELECT payload FROM session_state WHERE singleton = 1",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()?;

        payload
            .map(|payload| {
                let checkpoint: SessionCheckpoint = serde_json::from_str(&payload)?;
                validate_checkpoint(&checkpoint)?;
                Ok(checkpoint)
            })
            .transpose()
    }

    fn save_podcast_progress(&mut self, progress: &PodcastProgress) -> Result<(), StorageError> {
        let playback_epoch = u64_to_i64(progress.playback_epoch, "podcast playback_epoch")?;
        let position_ms = u64_to_i64(progress.position_ms, "podcast position_ms")?;
        let duration_ms = progress
            .duration_ms
            .map(|value| u64_to_i64(value, "podcast duration_ms"))
            .transpose()?;
        let played = i64::from(progress.played);

        self.connection.execute(
            "INSERT INTO podcast_progress(
                 video_id, playback_epoch, position_ms, duration_ms, played, updated_at
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(video_id) DO UPDATE SET
                 playback_epoch = excluded.playback_epoch,
                 position_ms = CASE
                     WHEN excluded.playback_epoch > podcast_progress.playback_epoch
                         THEN excluded.position_ms
                     ELSE MAX(podcast_progress.position_ms, excluded.position_ms)
                 END,
                 duration_ms = CASE
                     WHEN podcast_progress.duration_ms IS NULL THEN excluded.duration_ms
                     WHEN excluded.duration_ms IS NULL THEN podcast_progress.duration_ms
                     ELSE MAX(podcast_progress.duration_ms, excluded.duration_ms)
                 END,
                 played = CASE
                     WHEN excluded.playback_epoch > podcast_progress.playback_epoch
                         THEN excluded.played
                     ELSE MAX(podcast_progress.played, excluded.played)
                 END,
                 updated_at = CASE
                     WHEN excluded.playback_epoch > podcast_progress.playback_epoch
                         THEN excluded.updated_at
                     ELSE MAX(podcast_progress.updated_at, excluded.updated_at)
                 END
             WHERE excluded.playback_epoch >= podcast_progress.playback_epoch",
            params![
                progress.video_id,
                playback_epoch,
                position_ms,
                duration_ms,
                played,
                progress.updated_at
            ],
        )?;
        Ok(())
    }

    fn load_podcast_progress(
        &self,
        video_id: &str,
    ) -> Result<Option<PodcastProgress>, StorageError> {
        let row = self
            .connection
            .query_row(
                "SELECT video_id, playback_epoch, position_ms, duration_ms, played, updated_at
                 FROM podcast_progress
                 WHERE video_id = ?1",
                [video_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, Option<i64>>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, i64>(5)?,
                    ))
                },
            )
            .optional()?;

        row.map(
            |(video_id, playback_epoch, position_ms, duration_ms, played, updated_at)| {
                Ok(PodcastProgress {
                    video_id,
                    playback_epoch: nonnegative_i64_to_u64(
                        playback_epoch,
                        "podcast playback_epoch",
                    )?,
                    position_ms: nonnegative_i64_to_u64(position_ms, "podcast position_ms")?,
                    duration_ms: duration_ms
                        .map(|value| nonnegative_i64_to_u64(value, "podcast duration_ms"))
                        .transpose()?,
                    played: sqlite_boolean(played, "podcast played")?,
                    updated_at,
                })
            },
        )
        .transpose()
    }

    fn record_history(&mut self, item: &MediaItem, played_at: i64) -> Result<(), StorageError> {
        let item_json = serde_json::to_string(item)?;
        let transaction = self.connection.transaction()?;
        transaction.execute(
            "INSERT INTO listening_history(video_id, item_json, played_at)
             VALUES (?1, ?2, ?3)",
            params![item.id.video_id, item_json, played_at],
        )?;
        transaction.execute(
            "DELETE FROM listening_history
             WHERE id IN (
                 SELECT id
                 FROM listening_history
                 ORDER BY played_at DESC, id DESC
                 LIMIT -1 OFFSET ?1
             )",
            [
                i64::try_from(HISTORY_LIMIT).map_err(|_| StorageError::IntegerOutOfRange {
                    field: "history limit",
                    value: HISTORY_LIMIT as u128,
                })?,
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    fn recent_history(&self, limit: usize) -> Result<Vec<HistoryEntry>, StorageError> {
        let limit = i64::try_from(limit).map_err(|_| StorageError::IntegerOutOfRange {
            field: "history query limit",
            value: limit as u128,
        })?;
        let mut statement = self.connection.prepare(
            "SELECT id, video_id, item_json, played_at
             FROM listening_history
             ORDER BY played_at DESC, id DESC
             LIMIT ?1",
        )?;
        let rows = statement.query_map([limit], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })?;

        rows.map(|row_result| {
            let (id, video_id, item_json, played_at) = row_result?;
            let item: MediaItem = serde_json::from_str(&item_json)?;
            if item.id.video_id != video_id {
                return Err(StorageError::CorruptData {
                    entity: "listening_history",
                    reason: format!(
                        "row video ID `{video_id}` does not match serialized media ID `{}`",
                        item.id.video_id
                    ),
                });
            }
            Ok(HistoryEntry {
                id,
                item,
                played_at,
            })
        })
        .collect()
    }

    fn load_favorites(&self) -> Result<Vec<FavoriteEntry>, StorageError> {
        let mut statement = self.connection.prepare(
            "SELECT id, provider, video_id, item_json, favorited_at
             FROM favorites
             ORDER BY favorited_at DESC, id DESC",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })?;

        rows.map(|row_result| {
            let (id, provider, video_id, item_json, favorited_at) = row_result?;
            let item: MediaItem = serde_json::from_str(&item_json)?;
            if item.id.provider != provider || item.id.video_id != video_id {
                return Err(StorageError::CorruptData {
                    entity: "favorites",
                    reason: format!(
                        "row identity `{provider}:{video_id}` does not match serialized media identity `{}:{}`",
                        item.id.provider, item.id.video_id
                    ),
                });
            }
            Ok(FavoriteEntry {
                id,
                item,
                favorited_at,
            })
        })
        .collect()
    }

    fn add_favorite(
        &mut self,
        item: &MediaItem,
        favorited_at: i64,
    ) -> Result<FavoriteInsertOutcome, StorageError> {
        self.add_favorite_with_capacity_observer(item, favorited_at, || {})
    }

    fn remove_favorite(&mut self, id: &MediaId) -> Result<bool, StorageError> {
        Ok(self.connection.execute(
            "DELETE FROM favorites WHERE provider = ?1 AND video_id = ?2",
            params![id.provider, id.video_id],
        )? != 0)
    }

    fn put_metadata(
        &mut self,
        cache_key: &str,
        payload: &str,
        expires_at: i64,
        stored_at: i64,
    ) -> Result<(), StorageError> {
        self.connection.execute(
            "INSERT INTO metadata_cache(cache_key, payload, expires_at, stored_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(cache_key) DO UPDATE SET
                 payload = excluded.payload,
                 expires_at = excluded.expires_at,
                 stored_at = excluded.stored_at",
            params![cache_key, payload, expires_at, stored_at],
        )?;
        Ok(())
    }

    fn get_metadata_entry(
        &self,
        cache_key: &str,
    ) -> Result<Option<MetadataCacheEntry>, StorageError> {
        let row = self
            .connection
            .query_row(
                "SELECT payload, expires_at, stored_at
                 FROM metadata_cache
                 WHERE cache_key = ?1",
                [cache_key],
                |row| {
                    Ok((
                        row.get::<_, Value>(0)?,
                        row.get::<_, Value>(1)?,
                        row.get::<_, Value>(2)?,
                    ))
                },
            )
            .optional()?;
        let Some((payload, expires_at, stored_at)) = row else {
            return Ok(None);
        };
        let payload = cache_text(payload, "payload")?;
        let expires_at = cache_integer(expires_at, "expires_at")?;
        let stored_at = cache_integer(stored_at, "stored_at")?;

        Ok(Some(MetadataCacheEntry {
            payload,
            expires_at,
            stored_at,
        }))
    }

    fn get_metadata(&self, cache_key: &str, now: i64) -> Result<Option<String>, StorageError> {
        Ok(self
            .get_metadata_entry(cache_key)?
            .filter(|entry| !entry.stale_at(now))
            .map(|entry| entry.payload))
    }
}

fn validate_checkpoint(checkpoint: &SessionCheckpoint) -> Result<(), StorageError> {
    let queue =
        Queue::restore(checkpoint.queue.clone()).map_err(|error| StorageError::CorruptData {
            entity: "session_state",
            reason: error.to_string(),
        })?;
    let queue_current = queue.current().map(|item| &item.media().id);
    if queue_current != checkpoint.playback.current.as_ref() {
        return Err(StorageError::CorruptData {
            entity: "session_state",
            reason: "queue current media does not match playback current media".to_owned(),
        });
    }

    let mut config = Config::default();
    config.playback.volume = checkpoint.playback.target_volume;
    config.podcast.speed = checkpoint.playback.playback_speed;
    config
        .validate()
        .map_err(|error| StorageError::CorruptData {
            entity: "session_state",
            reason: error.to_string(),
        })
}

fn cache_text(value: Value, field: &'static str) -> Result<String, StorageError> {
    match value {
        Value::Text(value) => Ok(value),
        other => Err(invalid_cache_type(field, "TEXT", &other)),
    }
}

fn cache_integer(value: Value, field: &'static str) -> Result<i64, StorageError> {
    match value {
        Value::Integer(value) => Ok(value),
        other => Err(invalid_cache_type(field, "INTEGER", &other)),
    }
}

fn invalid_cache_type(field: &'static str, expected: &str, value: &Value) -> StorageError {
    StorageError::CorruptData {
        entity: "metadata_cache",
        reason: format!(
            "column `{field}` must use SQLite storage class {expected}, found {}",
            sqlite_value_kind(value)
        ),
    }
}

fn sqlite_value_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "NULL",
        Value::Integer(_) => "INTEGER",
        Value::Real(_) => "REAL",
        Value::Text(_) => "TEXT",
        Value::Blob(_) => "BLOB",
    }
}

fn configure_connection(connection: &Connection) -> Result<(), StorageError> {
    connection.pragma_update(None, "journal_mode", "WAL")?;
    connection.pragma_update(None, "foreign_keys", true)?;
    connection.busy_timeout(BUSY_TIMEOUT)?;
    Ok(())
}

fn current_timestamp_ms() -> Result<i64, StorageError> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| StorageError::Clock {
            reason: error.to_string(),
        })?;
    i64::try_from(duration.as_millis()).map_err(|_| StorageError::IntegerOutOfRange {
        field: "migration timestamp",
        value: duration.as_millis(),
    })
}

fn u64_to_i64(value: u64, field: &'static str) -> Result<i64, StorageError> {
    i64::try_from(value).map_err(|_| StorageError::IntegerOutOfRange {
        field,
        value: u128::from(value),
    })
}

fn nonnegative_i64_to_u64(value: i64, field: &'static str) -> Result<u64, StorageError> {
    u64::try_from(value).map_err(|_| StorageError::CorruptData {
        entity: field,
        reason: format!("expected a nonnegative integer, found {value}"),
    })
}

fn sqlite_boolean(value: i64, field: &'static str) -> Result<bool, StorageError> {
    match value {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(StorageError::CorruptData {
            entity: field,
            reason: format!("expected 0 or 1, found {value}"),
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        error::Error,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
            mpsc,
        },
        thread,
        time::Duration,
    };

    use rusqlite::ErrorCode;
    use tempfile::TempDir;

    use super::*;
    use crate::domain::{MediaItem, MediaKind};

    const SYNC_HANG_TIMEOUT: Duration = Duration::from_secs(2);

    fn favorite(video_id: &str) -> MediaItem {
        MediaItem {
            id: MediaId {
                provider: "youtube-music".to_owned(),
                video_id: video_id.to_owned(),
            },
            kind: MediaKind::Song,
            title: video_id.to_owned(),
            creators: Vec::new(),
            collection: None,
            duration_ms: None,
            artwork_url: None,
            explicit: false,
        }
    }

    #[test]
    fn immediate_favorite_transaction_locks_before_the_capacity_check() -> Result<(), Box<dyn Error>>
    {
        let directory = TempDir::new()?;
        let path = directory.path().join("favorites.sqlite3");
        let mut seed = SqliteStorage::open(&path)?;
        for index in 0..(FAVORITES_LIMIT - 1) {
            assert_eq!(
                seed.add_favorite(&favorite(&format!("seed-{index}")), i64::try_from(index)?)?,
                FavoriteInsertOutcome::Added
            );
        }
        drop(seed);

        let first_storage = SqliteStorage::open(&path)?;
        let mut contender = SqliteStorage::open(&path)?;
        contender.connection.busy_timeout(Duration::ZERO)?;
        let (at_capacity_boundary_tx, at_capacity_boundary_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let first = thread::spawn(move || -> Result<FavoriteInsertOutcome, StorageError> {
            let mut storage = first_storage;
            storage.add_favorite_with_capacity_observer(&favorite("first"), 10_000, || {
                let _ = at_capacity_boundary_tx.send(());
                assert!(
                    release_rx.recv_timeout(SYNC_HANG_TIMEOUT).is_ok(),
                    "timed out waiting to release the first favorite transaction"
                );
            })
        });
        if let Err(error) = at_capacity_boundary_rx.recv_timeout(SYNC_HANG_TIMEOUT) {
            let _ = release_tx.send(());
            let _ = first.join();
            return Err(std::io::Error::other(format!(
                "timed out waiting for the first transaction capacity boundary: {error}"
            ))
            .into());
        }

        let contender_reached_capacity = Arc::new(AtomicBool::new(false));
        let observed = Arc::clone(&contender_reached_capacity);
        let contention =
            contender.add_favorite_with_capacity_observer(&favorite("second"), 10_001, move || {
                observed.store(true, Ordering::SeqCst);
            });

        release_tx.send(())?;
        assert!(
            !contender_reached_capacity.load(Ordering::SeqCst),
            "a competing transaction reached COUNT before the first writer committed"
        );
        let first_outcome = first
            .join()
            .map_err(|_| std::io::Error::other("first favorite insert thread panicked"))??;
        assert_eq!(first_outcome, FavoriteInsertOutcome::Added);
        assert!(matches!(
            contention,
            Err(StorageError::Database(error))
                if error.sqlite_error_code() == Some(ErrorCode::DatabaseBusy)
        ));
        contender.connection.busy_timeout(BUSY_TIMEOUT)?;
        assert_eq!(
            contender.add_favorite(&favorite("second"), 10_001)?,
            FavoriteInsertOutcome::Full
        );
        assert_eq!(contender.load_favorites()?.len(), FAVORITES_LIMIT);
        Ok(())
    }
}
