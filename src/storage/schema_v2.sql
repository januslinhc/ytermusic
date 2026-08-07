ALTER TABLE podcast_progress RENAME TO podcast_progress_v1;

CREATE TABLE podcast_progress (
    video_id TEXT PRIMARY KEY,
    playback_epoch INTEGER NOT NULL DEFAULT 0,
    position_ms INTEGER NOT NULL,
    duration_ms INTEGER,
    played INTEGER NOT NULL DEFAULT 0,
    updated_at INTEGER NOT NULL
);

INSERT INTO podcast_progress(
    video_id,
    playback_epoch,
    position_ms,
    duration_ms,
    played,
    updated_at
)
SELECT
    video_id,
    0,
    position_ms,
    duration_ms,
    played,
    updated_at
FROM podcast_progress_v1;

DROP TABLE podcast_progress_v1;
