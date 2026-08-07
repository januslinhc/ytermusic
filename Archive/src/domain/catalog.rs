use std::fmt;

use serde::{Deserialize, Serialize};

use super::MediaItem;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub enum SearchFilter {
    #[default]
    All,
    Songs,
    Albums,
    Artists,
    Playlists,
    Podcasts,
    Episodes,
}

#[derive(Clone, Deserialize, PartialEq, Serialize)]
pub struct ChartSection {
    pub title: String,
    pub items: Vec<MediaItem>,
}

impl ChartSection {
    #[must_use]
    pub fn new(title: impl Into<String>, items: Vec<MediaItem>) -> Self {
        Self {
            title: title.into(),
            items,
        }
    }

    #[must_use]
    pub fn title(&self) -> &str {
        &self.title
    }

    #[must_use]
    pub fn items(&self) -> &[MediaItem] {
        &self.items
    }
}

impl fmt::Debug for ChartSection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ChartSection")
            .field("title", &self.title)
            .field("item_count", &self.items.len())
            .finish_non_exhaustive()
    }
}
