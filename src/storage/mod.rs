mod migrations;
mod repository;

use thiserror::Error;

pub use repository::{
    ConnectionSettings, FAVORITES_LIMIT, FavoriteEntry, FavoriteInsertOutcome, HISTORY_LIMIT,
    HistoryEntry, MetadataCacheEntry, PodcastProgress, SqliteStorage, Storage,
};

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("SQLite storage failed: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("stored JSON could not be encoded or decoded: {0}")]
    Json(#[from] serde_json::Error),
    #[error("database schema version {found} is newer than supported version {supported}")]
    UnsupportedSchemaVersion { found: i64, supported: i64 },
    #[error("database contains invalid schema version {found}")]
    InvalidSchemaVersion { found: i64 },
    #[error("{field} value {value} cannot be represented safely")]
    IntegerOutOfRange { field: &'static str, value: u128 },
    #[error("stored {entity} data is corrupt: {reason}")]
    CorruptData {
        entity: &'static str,
        reason: String,
    },
    #[error("system clock could not supply a migration timestamp: {reason}")]
    Clock { reason: String },
}
