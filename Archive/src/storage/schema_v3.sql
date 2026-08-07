CREATE TABLE favorites (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    provider TEXT NOT NULL,
    video_id TEXT NOT NULL,
    item_json TEXT NOT NULL,
    favorited_at INTEGER NOT NULL,
    UNIQUE(provider, video_id)
);

CREATE INDEX favorites_newest_first
    ON favorites(favorited_at DESC, id DESC);
