use std::fmt;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use url::Url;

use crate::domain::{ChartSection, MediaId, MediaItem, RegionCode, SearchFilter};
use crate::lyrics::{MAX_LYRICS_TEXT_BYTES, normalize_lyrics_text};

#[derive(Clone, Default, Deserialize, PartialEq, Serialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    /// Opaque token retained for the runtime to pass back to the provider.
    pub continuation: Option<String>,
    pub stale: bool,
}

impl<T> fmt::Debug for Page<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Page")
            .field("item_count", &self.items.len())
            .field("continuation_present", &self.continuation.is_some())
            .field("stale", &self.stale)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
pub struct BrowseItem {
    pub id: String,
    pub title: String,
    pub subtitle: Option<String>,
    pub artwork_url: Option<Url>,
}

impl fmt::Debug for BrowseItem {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BrowseItem")
            .field("id", &self.id)
            .field("title", &self.title)
            .field("subtitle", &self.subtitle)
            .field("artwork_present", &self.artwork_url.is_some())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Deserialize, PartialEq, Serialize)]
pub enum SearchItem {
    Playable(MediaItem),
    Album(BrowseItem),
    Artist(BrowseItem),
    Playlist(BrowseItem),
    Podcast(BrowseItem),
}

impl fmt::Debug for SearchItem {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Playable(item) => formatter
                .debug_struct("SearchItem::Playable")
                .field("kind", &item.kind)
                .field("artwork_present", &item.artwork_url.is_some())
                .finish_non_exhaustive(),
            Self::Album(item) => formatter
                .debug_tuple("SearchItem::Album")
                .field(item)
                .finish(),
            Self::Artist(item) => formatter
                .debug_tuple("SearchItem::Artist")
                .field(item)
                .finish(),
            Self::Playlist(item) => formatter
                .debug_tuple("SearchItem::Playlist")
                .field(item)
                .finish(),
            Self::Podcast(item) => formatter
                .debug_tuple("SearchItem::Podcast")
                .field(item)
                .finish(),
        }
    }
}

#[derive(Clone, Deserialize, PartialEq, Serialize)]
pub struct Podcast {
    pub id: String,
    pub title: String,
    pub creators: Vec<String>,
    pub description: Option<String>,
    pub artwork_url: Option<Url>,
    pub episodes: Vec<MediaItem>,
}

impl fmt::Debug for Podcast {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Podcast")
            .field("id", &self.id)
            .field("title", &self.title)
            .field("creator_count", &self.creators.len())
            .field("description_present", &self.description.is_some())
            .field("artwork_present", &self.artwork_url.is_some())
            .field("episode_count", &self.episodes.len())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub enum LibrarySection {
    Songs,
    Albums,
    Artists,
    Playlists,
    Podcasts,
    Episodes,
}

#[derive(Clone, Deserialize, PartialEq, Serialize)]
pub enum LibraryItem {
    Playable(MediaItem),
    Album(BrowseItem),
    Artist(BrowseItem),
    Playlist(BrowseItem),
    Podcast(BrowseItem),
}

impl fmt::Debug for LibraryItem {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Playable(item) => formatter
                .debug_struct("LibraryItem::Playable")
                .field("kind", &item.kind)
                .field("artwork_present", &item.artwork_url.is_some())
                .finish_non_exhaustive(),
            Self::Album(item) => formatter
                .debug_tuple("LibraryItem::Album")
                .field(item)
                .finish(),
            Self::Artist(item) => formatter
                .debug_tuple("LibraryItem::Artist")
                .field(item)
                .finish(),
            Self::Playlist(item) => formatter
                .debug_tuple("LibraryItem::Playlist")
                .field(item)
                .finish(),
            Self::Podcast(item) => formatter
                .debug_tuple("LibraryItem::Podcast")
                .field(item)
                .finish(),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum AuthenticationState {
    Unauthenticated,
    Authenticated,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ProviderErrorKind {
    #[error("provider is unavailable")]
    Unavailable,
    #[error("authentication is required")]
    AuthenticationRequired,
    #[error("requested item was not found")]
    NotFound,
    #[error("provider returned an invalid response")]
    InvalidResponse,
    #[error("provider does not support this operation")]
    Unsupported,
    #[error("operation was cancelled")]
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderOperation {
    Search,
    Charts,
    Playlist,
    Podcast,
    Radio,
    Library,
    Authentication,
    Lyrics,
}

impl fmt::Display for ProviderOperation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Search => "search",
            Self::Charts => "charts",
            Self::Playlist => "playlist",
            Self::Podcast => "podcast",
            Self::Radio => "radio",
            Self::Library => "library",
            Self::Authentication => "authentication",
            Self::Lyrics => "lyrics",
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("{operation} failed: {kind}")]
pub struct ProviderError {
    pub operation: ProviderOperation,
    pub kind: ProviderErrorKind,
}

impl ProviderError {
    #[must_use]
    pub const fn new(operation: ProviderOperation, kind: ProviderErrorKind) -> Self {
        Self { operation, kind }
    }
}

pub type ProviderResult<T> = Result<T, ProviderError>;

#[derive(Clone, Eq, PartialEq)]
pub struct PlainLyrics {
    text: String,
}

impl PlainLyrics {
    /// Creates bounded, non-empty plain lyrics.
    ///
    /// # Errors
    ///
    /// Returns a payload-free provider error for empty or oversized text.
    pub fn new(text: &str) -> ProviderResult<Self> {
        let text = text.trim();
        if text.len() > MAX_LYRICS_TEXT_BYTES {
            return Err(ProviderError::new(
                ProviderOperation::Lyrics,
                ProviderErrorKind::InvalidResponse,
            ));
        }
        let text = normalize_lyrics_text(text);
        let text = text.trim();
        if text.is_empty() {
            return Err(ProviderError::new(
                ProviderOperation::Lyrics,
                ProviderErrorKind::InvalidResponse,
            ));
        }
        Ok(Self {
            text: text.to_owned(),
        })
    }

    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }
}

impl fmt::Debug for PlainLyrics {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PlainLyrics")
            .field("text_bytes", &self.text.len())
            .finish_non_exhaustive()
    }
}

#[async_trait]
pub trait MusicProvider: Send + Sync {
    async fn search(&self, query: &str, filter: SearchFilter) -> ProviderResult<Page<SearchItem>>;

    /// Continues a search using the provider's opaque page token.
    ///
    /// Providers that cannot preserve continuation semantics report
    /// [`ProviderErrorKind::Unsupported`] instead of replaying page one.
    async fn search_more(
        &self,
        _query: &str,
        _filter: SearchFilter,
        _continuation: &str,
    ) -> ProviderResult<Page<SearchItem>> {
        Err(ProviderError::new(
            ProviderOperation::Search,
            ProviderErrorKind::Unsupported,
        ))
    }

    /// Returns chart sections; cache freshness remains owned by the runtime.
    async fn charts(&self, region: &RegionCode) -> ProviderResult<Vec<ChartSection>>;

    async fn playlist(&self, id: &str) -> ProviderResult<Vec<MediaItem>>;

    async fn podcast(&self, id: &str) -> ProviderResult<Podcast>;

    async fn radio(&self, seed: &MediaId) -> ProviderResult<Vec<MediaItem>>;

    async fn lyrics(&self, _id: &MediaId) -> ProviderResult<PlainLyrics> {
        Err(ProviderError::new(
            ProviderOperation::Lyrics,
            ProviderErrorKind::Unsupported,
        ))
    }

    async fn library(&self, section: LibrarySection) -> ProviderResult<Page<LibraryItem>>;

    /// Continues a library request using an opaque provider token.
    async fn library_more(
        &self,
        _section: LibrarySection,
        _continuation: &str,
    ) -> ProviderResult<Page<LibraryItem>> {
        Err(ProviderError::new(
            ProviderOperation::Library,
            ProviderErrorKind::Unsupported,
        ))
    }

    fn authentication(&self) -> AuthenticationState;
}
