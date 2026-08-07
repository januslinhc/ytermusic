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

CREATE INDEX history_played_at
    ON listening_history(played_at DESC);

CREATE TABLE metadata_cache (
    cache_key TEXT PRIMARY KEY,
    payload TEXT NOT NULL,
    expires_at INTEGER NOT NULL,
    stored_at INTEGER NOT NULL
);
