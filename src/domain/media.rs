use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, Hash, PartialEq, Deserialize, Serialize)]
pub struct MediaId {
    pub provider: String,
    pub video_id: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub enum MediaKind {
    Song,
    Video,
    PodcastEpisode,
}

#[derive(Clone, PartialEq, Deserialize, Serialize)]
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

impl fmt::Debug for MediaItem {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MediaItem")
            .field("id", &self.id)
            .field("kind", &self.kind)
            .field("title", &self.title)
            .field("creator_count", &self.creators.len())
            .field("collection_present", &self.collection.is_some())
            .field("duration_ms", &self.duration_ms)
            .field("artwork_present", &self.artwork_url.is_some())
            .field("explicit", &self.explicit)
            .finish_non_exhaustive()
    }
}
