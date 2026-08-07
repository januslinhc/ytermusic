use std::fmt;

use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

use crate::{
    config::{BehaviorConfig, Config, ConfigError},
    diagnostics::{DiagnosticRow, DoctorReport},
    domain::{
        ArtworkUrl, ChartSection, MediaId, MediaItem, PlaybackSnapshot, PlaybackStatus, RegionCode,
        SearchFilter,
    },
    lyrics::LyricsDocument,
    podcast_rankings::{PodcastRecommendation, PodcastRecommendationId},
    provider::{AuthenticationState, LibraryItem, LibrarySection, Podcast},
    queue::{Queue, QueueError, QueueSnapshot},
    storage::{FavoriteEntry, HistoryEntry, MetadataCacheEntry},
};

pub const RADIO_FILL_THRESHOLD: usize = 2;
pub const MAX_VIEW_ITEMS: usize = 1_024;
const MAX_PODCAST_PROVIDER_ID_BYTES: usize = 512;
const MUSIC_SEEK_SECONDS: u64 = 10;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ArtworkSurface {
    #[default]
    Home,
    Search,
    Charts,
    Podcasts,
    Library,
    Favorites,
    History,
    Settings,
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Generation(u64);

impl Generation {
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn value(self) -> u64 {
        self.0
    }

    pub(super) const fn checked_next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }
}

#[derive(Clone, Eq, Hash, PartialEq)]
pub struct PodcastProviderId(String);

impl PodcastProviderId {
    #[must_use]
    pub fn new(value: String) -> Option<Self> {
        (!value.is_empty()
            && value.len() <= MAX_PODCAST_PROVIDER_ID_BYTES
            && !value
                .chars()
                .any(|character| character.is_whitespace() || character.is_control()))
        .then_some(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for PodcastProviderId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PodcastProviderId([REDACTED])")
    }
}

impl fmt::Display for PodcastProviderId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[REDACTED]")
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SearchMetadataKind {
    Album,
    Artist,
    Playlist,
    Podcast,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchMetadata {
    kind: SearchMetadataKind,
    provider_id: Option<String>,
    title: String,
    subtitle: Option<String>,
    artwork_url: Option<ArtworkUrl>,
}

impl SearchMetadata {
    #[must_use]
    pub fn new(kind: SearchMetadataKind, title: impl Into<String>) -> Self {
        Self {
            kind,
            provider_id: None,
            title: title.into(),
            subtitle: None,
            artwork_url: None,
        }
    }

    #[must_use]
    pub const fn kind(&self) -> SearchMetadataKind {
        self.kind
    }

    #[must_use]
    pub fn provider_id(&self) -> Option<&str> {
        self.provider_id.as_deref()
    }

    #[must_use]
    pub fn title(&self) -> &str {
        &self.title
    }

    #[must_use]
    pub fn subtitle(&self) -> Option<&str> {
        self.subtitle.as_deref()
    }

    #[must_use]
    pub const fn artwork_url(&self) -> Option<&ArtworkUrl> {
        self.artwork_url.as_ref()
    }

    #[must_use]
    pub fn with_subtitle(mut self, subtitle: impl Into<String>) -> Self {
        self.subtitle = Some(subtitle.into());
        self
    }

    #[must_use]
    pub fn with_provider_id(mut self, provider_id: impl Into<String>) -> Self {
        self.provider_id = Some(provider_id.into());
        self
    }

    #[must_use]
    pub fn with_artwork_url(mut self, artwork_url: ArtworkUrl) -> Self {
        self.artwork_url = Some(artwork_url);
        self
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum SearchItemId {
    Media(MediaId),
    Metadata {
        kind: SearchMetadataKind,
        provider_id: String,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub enum SearchItem {
    Playable(MediaItem),
    Metadata(SearchMetadata),
}

impl SearchItem {
    #[must_use]
    pub fn stable_id(&self) -> SearchItemId {
        match self {
            Self::Playable(item) => SearchItemId::Media(item.id.clone()),
            Self::Metadata(metadata) => SearchItemId::Metadata {
                kind: metadata.kind,
                provider_id: metadata
                    .provider_id
                    .clone()
                    .unwrap_or_else(|| metadata.title.clone()),
            },
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SearchPage {
    items: Vec<SearchItem>,
    continuation: Option<OpaqueContinuation>,
    stale: bool,
}

impl SearchPage {
    #[must_use]
    pub const fn new(items: Vec<SearchItem>) -> Self {
        Self {
            items,
            continuation: None,
            stale: false,
        }
    }

    #[must_use]
    pub fn from_provider(page: crate::provider::Page<crate::provider::SearchItem>) -> Self {
        let items = page
            .items
            .into_iter()
            .map(|item| match item {
                crate::provider::SearchItem::Playable(item) => SearchItem::Playable(item),
                crate::provider::SearchItem::Album(item) => {
                    SearchItem::Metadata(provider_metadata(SearchMetadataKind::Album, item))
                }
                crate::provider::SearchItem::Artist(item) => {
                    SearchItem::Metadata(provider_metadata(SearchMetadataKind::Artist, item))
                }
                crate::provider::SearchItem::Playlist(item) => {
                    SearchItem::Metadata(provider_metadata(SearchMetadataKind::Playlist, item))
                }
                crate::provider::SearchItem::Podcast(item) => {
                    SearchItem::Metadata(provider_metadata(SearchMetadataKind::Podcast, item))
                }
            })
            .collect();
        Self {
            items,
            continuation: page.continuation.and_then(OpaqueContinuation::new),
            stale: page.stale,
        }
    }

    #[must_use]
    pub fn with_continuation(mut self, continuation: impl Into<String>) -> Self {
        self.continuation = OpaqueContinuation::new(continuation.into());
        self
    }

    #[must_use]
    pub const fn with_stale(mut self, stale: bool) -> Self {
        self.stale = stale;
        self
    }

    #[must_use]
    pub fn items(&self) -> &[SearchItem] {
        &self.items
    }

    #[must_use]
    pub const fn continuation(&self) -> Option<&OpaqueContinuation> {
        self.continuation.as_ref()
    }

    #[must_use]
    pub const fn stale(&self) -> bool {
        self.stale
    }

    pub(super) fn into_parts(self) -> (Vec<SearchItem>, Option<OpaqueContinuation>, bool) {
        (self.items, self.continuation, self.stale)
    }
}

fn provider_metadata(
    kind: SearchMetadataKind,
    item: crate::provider::BrowseItem,
) -> SearchMetadata {
    let crate::provider::BrowseItem {
        id,
        title,
        subtitle,
        artwork_url,
    } = item;
    let mut metadata = SearchMetadata::new(kind, title).with_provider_id(id);
    if let Some(subtitle) = subtitle {
        metadata = metadata.with_subtitle(subtitle);
    }
    if let Some(artwork_url) = artwork_url.and_then(|url| ArtworkUrl::try_from(url).ok()) {
        metadata = metadata.with_artwork_url(artwork_url);
    }
    metadata
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AppErrorCategory {
    Search,
    Resolve,
    Charts,
    Podcast,
    Library,
    Authentication,
    History,
    Favorites,
    PlaybackUnavailable,
    Artwork,
    Lyrics,
    Radio,
    State,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FavoriteMutation {
    Add,
    Remove,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingFavoriteMutation {
    pub(super) media_id: MediaId,
    pub(super) mutation: FavoriteMutation,
}

impl PendingFavoriteMutation {
    #[must_use]
    pub const fn media_id(&self) -> &MediaId {
        &self.media_id
    }

    #[must_use]
    pub const fn mutation(&self) -> FavoriteMutation {
        self.mutation
    }
}

#[derive(Clone, Eq, Hash, PartialEq)]
pub struct LyricsMediaId(MediaId);

impl LyricsMediaId {
    #[must_use]
    pub const fn media_id(&self) -> &MediaId {
        &self.0
    }

    #[must_use]
    pub fn into_media_id(self) -> MediaId {
        self.0
    }
}

impl From<MediaId> for LyricsMediaId {
    fn from(value: MediaId) -> Self {
        Self(value)
    }
}

impl fmt::Debug for LyricsMediaId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("LyricsMediaId([REDACTED])")
    }
}

#[derive(Clone, PartialEq)]
pub struct LyricsMediaItem(MediaItem);

impl LyricsMediaItem {
    #[must_use]
    pub const fn media(&self) -> &MediaItem {
        &self.0
    }

    #[must_use]
    pub fn into_media(self) -> MediaItem {
        self.0
    }
}

impl From<MediaItem> for LyricsMediaItem {
    fn from(value: MediaItem) -> Self {
        Self(value)
    }
}

impl fmt::Debug for LyricsMediaItem {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LyricsMediaItem")
            .field("kind", &self.0.kind)
            .field("duration_ms", &self.0.duration_ms)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Default, PartialEq)]
pub struct LyricsState {
    pub(super) active_generation: Option<Generation>,
    pub(super) media_id: Option<MediaId>,
    pub(super) loading: bool,
    pub(super) error: Option<AppError>,
    pub(super) document: Option<LyricsDocument>,
    pub(super) active_line_index: Option<usize>,
}

impl fmt::Debug for LyricsState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LyricsState")
            .field("active_generation", &self.active_generation)
            .field("has_media", &self.media_id.is_some())
            .field("loading", &self.loading)
            .field("error", &self.error)
            .field("document", &self.document)
            .field("active_line_index", &self.active_line_index)
            .finish()
    }
}

impl LyricsState {
    #[must_use]
    pub const fn active_generation(&self) -> Option<Generation> {
        self.active_generation
    }
    #[must_use]
    pub const fn media_id(&self) -> Option<&MediaId> {
        self.media_id.as_ref()
    }
    #[must_use]
    pub const fn loading(&self) -> bool {
        self.loading
    }
    #[must_use]
    pub const fn error(&self) -> Option<&AppError> {
        self.error.as_ref()
    }
    #[must_use]
    pub const fn document(&self) -> Option<&LyricsDocument> {
        self.document.as_ref()
    }
    #[must_use]
    pub const fn active_line_index(&self) -> Option<usize> {
        self.active_line_index
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct AppError {
    category: AppErrorCategory,
    message: String,
}

impl fmt::Debug for AppError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AppError")
            .field("category", &self.category)
            .field("message_redacted", &true)
            .finish_non_exhaustive()
    }
}

impl AppError {
    #[must_use]
    pub fn new(category: AppErrorCategory, message: impl Into<String>) -> Self {
        Self {
            category,
            message: message.into(),
        }
    }

    #[must_use]
    pub const fn category(&self) -> AppErrorCategory {
        self.category
    }

    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiagnosticCategory {
    Selection,
    Resolve,
    Radio,
    State,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Diagnostic {
    category: DiagnosticCategory,
    message: String,
    media_id: Option<MediaId>,
}

impl Diagnostic {
    #[must_use]
    pub fn new(
        category: DiagnosticCategory,
        message: impl Into<String>,
        media_id: Option<MediaId>,
    ) -> Self {
        Self {
            category,
            message: message.into(),
            media_id,
        }
    }

    #[must_use]
    pub const fn category(&self) -> DiagnosticCategory {
        self.category
    }

    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    #[must_use]
    pub const fn media_id(&self) -> Option<&MediaId> {
        self.media_id.as_ref()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SearchState {
    pub(super) query: String,
    pub(super) filter: SearchFilter,
    pub(super) generation: Generation,
    pub(super) active_generation: Option<Generation>,
    pub(super) loading: bool,
    pub(super) loading_more: bool,
    pub(super) items: Vec<SearchItem>,
    pub(super) selected_id: Option<SearchItemId>,
    pub(super) continuation: Option<OpaqueContinuation>,
    pub(super) stale: bool,
    pub(super) error: Option<AppError>,
}

impl Default for SearchState {
    fn default() -> Self {
        Self {
            query: String::new(),
            filter: SearchFilter::All,
            generation: Generation::default(),
            active_generation: None,
            loading: false,
            loading_more: false,
            items: Vec::new(),
            selected_id: None,
            continuation: None,
            stale: false,
            error: None,
        }
    }
}

impl SearchState {
    #[must_use]
    pub fn query(&self) -> &str {
        &self.query
    }

    #[must_use]
    pub const fn filter(&self) -> SearchFilter {
        self.filter
    }

    #[must_use]
    pub const fn generation(&self) -> Generation {
        self.generation
    }

    #[must_use]
    pub const fn active_generation(&self) -> Option<Generation> {
        self.active_generation
    }

    #[must_use]
    pub const fn loading(&self) -> bool {
        self.loading
    }

    #[must_use]
    pub const fn loading_more(&self) -> bool {
        self.loading_more
    }

    #[must_use]
    pub fn items(&self) -> &[SearchItem] {
        &self.items
    }

    #[must_use]
    pub const fn selected_id(&self) -> Option<&SearchItemId> {
        self.selected_id.as_ref()
    }

    #[must_use]
    pub const fn continuation(&self) -> Option<&OpaqueContinuation> {
        self.continuation.as_ref()
    }

    #[must_use]
    pub const fn stale(&self) -> bool {
        self.stale
    }

    #[must_use]
    pub const fn error(&self) -> Option<&AppError> {
        self.error.as_ref()
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct PodcastState {
    pub(super) requested_region: RegionCode,
    pub(super) effective_region: Option<RegionCode>,
    pub(super) recommendations: Vec<PodcastRecommendation>,
    pub(super) selected_recommendation: Option<PodcastRecommendationId>,
    pub(super) recommendation_generation: Generation,
    pub(super) active_recommendation_generation: Option<Generation>,
    pub(super) recommendations_loading: bool,
    pub(super) recommendation_error: Option<AppError>,
    pub(super) resolve_generation: Generation,
    pub(super) active_resolve_generation: Option<Generation>,
    pub(super) resolve_loading: bool,
    pub(super) resolve_error: Option<AppError>,
    pub(super) generation: Generation,
    pub(super) active_generation: Option<Generation>,
    pub(super) loading: bool,
    pub(super) show: Option<Podcast>,
    pub(super) selected_episode: Option<MediaId>,
    pub(super) pending_progress_generation: Option<Generation>,
    pub(super) pending_media: Option<MediaItem>,
    pub(super) error: Option<AppError>,
}

impl PodcastState {
    #[must_use]
    pub const fn requested_region(&self) -> &RegionCode {
        &self.requested_region
    }

    #[must_use]
    pub const fn effective_region(&self) -> Option<&RegionCode> {
        self.effective_region.as_ref()
    }

    #[must_use]
    pub fn recommendations(&self) -> &[PodcastRecommendation] {
        &self.recommendations
    }

    #[must_use]
    pub const fn selected_recommendation(&self) -> Option<&PodcastRecommendationId> {
        self.selected_recommendation.as_ref()
    }

    #[must_use]
    pub const fn recommendation_generation(&self) -> Generation {
        self.recommendation_generation
    }

    #[must_use]
    pub const fn active_recommendation_generation(&self) -> Option<Generation> {
        self.active_recommendation_generation
    }

    #[must_use]
    pub const fn recommendations_loading(&self) -> bool {
        self.recommendations_loading
    }

    #[must_use]
    pub const fn recommendation_error(&self) -> Option<&AppError> {
        self.recommendation_error.as_ref()
    }

    #[must_use]
    pub const fn resolve_generation(&self) -> Generation {
        self.resolve_generation
    }

    #[must_use]
    pub const fn active_resolve_generation(&self) -> Option<Generation> {
        self.active_resolve_generation
    }

    #[must_use]
    pub const fn resolve_loading(&self) -> bool {
        self.resolve_loading
    }

    #[must_use]
    pub const fn resolve_error(&self) -> Option<&AppError> {
        self.resolve_error.as_ref()
    }

    #[must_use]
    pub const fn generation(&self) -> Generation {
        self.generation
    }

    #[must_use]
    pub const fn active_generation(&self) -> Option<Generation> {
        self.active_generation
    }

    #[must_use]
    pub const fn loading(&self) -> bool {
        self.loading
    }

    #[must_use]
    pub const fn show(&self) -> Option<&Podcast> {
        self.show.as_ref()
    }

    #[must_use]
    pub const fn selected_episode(&self) -> Option<&MediaId> {
        self.selected_episode.as_ref()
    }

    #[must_use]
    pub const fn pending_progress_generation(&self) -> Option<Generation> {
        self.pending_progress_generation
    }

    #[must_use]
    pub const fn error(&self) -> Option<&AppError> {
        self.error.as_ref()
    }
}

const MAX_CONTINUATION_BYTES: usize = 4 * 1024;

#[derive(Clone, Eq, PartialEq)]
pub struct OpaqueContinuation(String);

impl OpaqueContinuation {
    #[must_use]
    pub fn new(value: String) -> Option<Self> {
        (!value.trim().is_empty() && value.len() <= MAX_CONTINUATION_BYTES).then_some(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for OpaqueContinuation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("OpaqueContinuation([REDACTED])")
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum LibraryItemId {
    Media(MediaId),
    Browse {
        section: LibrarySection,
        provider_id: String,
    },
}

#[must_use]
pub fn stable_library_item_id(item: &LibraryItem) -> LibraryItemId {
    match item {
        LibraryItem::Playable(media) => LibraryItemId::Media(media.id.clone()),
        LibraryItem::Album(item) => LibraryItemId::Browse {
            section: LibrarySection::Albums,
            provider_id: item.id.clone(),
        },
        LibraryItem::Artist(item) => LibraryItemId::Browse {
            section: LibrarySection::Artists,
            provider_id: item.id.clone(),
        },
        LibraryItem::Playlist(item) => LibraryItemId::Browse {
            section: LibrarySection::Playlists,
            provider_id: item.id.clone(),
        },
        LibraryItem::Podcast(item) => LibraryItemId::Browse {
            section: LibrarySection::Podcasts,
            provider_id: item.id.clone(),
        },
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct LibraryState {
    pub(super) authentication: AuthenticationState,
    pub(super) section: LibrarySection,
    pub(super) generation: Generation,
    pub(super) active_generation: Option<Generation>,
    pub(super) loading: bool,
    pub(super) loading_more: bool,
    pub(super) items: Vec<LibraryItem>,
    pub(super) selected_id: Option<LibraryItemId>,
    pub(super) continuation: Option<OpaqueContinuation>,
    pub(super) stale: bool,
    pub(super) error: Option<AppError>,
}

impl Default for LibraryState {
    fn default() -> Self {
        Self {
            authentication: AuthenticationState::Unauthenticated,
            section: LibrarySection::Songs,
            generation: Generation::default(),
            active_generation: None,
            loading: false,
            loading_more: false,
            items: Vec::new(),
            selected_id: None,
            continuation: None,
            stale: false,
            error: None,
        }
    }
}

impl LibraryState {
    #[must_use]
    pub const fn authentication(&self) -> AuthenticationState {
        self.authentication
    }

    #[must_use]
    pub const fn section(&self) -> LibrarySection {
        self.section
    }

    #[must_use]
    pub const fn generation(&self) -> Generation {
        self.generation
    }

    #[must_use]
    pub const fn active_generation(&self) -> Option<Generation> {
        self.active_generation
    }

    #[must_use]
    pub const fn loading(&self) -> bool {
        self.loading
    }

    #[must_use]
    pub const fn loading_more(&self) -> bool {
        self.loading_more
    }

    #[must_use]
    pub fn items(&self) -> &[LibraryItem] {
        &self.items
    }

    #[must_use]
    pub const fn selected_id(&self) -> Option<&LibraryItemId> {
        self.selected_id.as_ref()
    }

    #[must_use]
    pub const fn continuation(&self) -> Option<&OpaqueContinuation> {
        self.continuation.as_ref()
    }

    #[must_use]
    pub const fn stale(&self) -> bool {
        self.stale
    }

    #[must_use]
    pub const fn error(&self) -> Option<&AppError> {
        self.error.as_ref()
    }
}

pub const CHART_CACHE_TTL_SECONDS: i64 = 3_600;
pub const MAX_CHART_CACHE_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ChartCachePayload {
    region: RegionCode,
    sections: Vec<ChartSection>,
    stored_at: i64,
    expires_at: i64,
}

#[derive(Deserialize)]
struct RawChartCachePayload {
    region: RegionCode,
    sections: Vec<ChartSection>,
    stored_at: i64,
    expires_at: i64,
}

impl<'de> Deserialize<'de> for ChartCachePayload {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = RawChartCachePayload::deserialize(deserializer)?;
        if raw.sections.len() > crate::provider::MAX_SECTIONS
            || raw
                .sections
                .iter()
                .any(|section| section.items.len() > crate::provider::MAX_ITEMS_PER_SHELF)
        {
            return Err(serde::de::Error::custom(
                "chart cache payload exceeds view limits",
            ));
        }
        Self::try_new(raw.region, raw.sections, raw.stored_at, raw.expires_at)
            .map_err(|error| serde::de::Error::custom(error.message()))
    }
}

impl ChartCachePayload {
    /// Creates a normalized cache payload whose encoded form fits the cache
    /// boundary.
    ///
    /// # Errors
    ///
    /// Returns a chart error for invalid provenance or an oversized payload.
    pub fn try_new(
        region: RegionCode,
        mut sections: Vec<ChartSection>,
        stored_at: i64,
        expires_at: i64,
    ) -> Result<Self, AppError> {
        if expires_at < stored_at {
            return Err(chart_cache_error(
                "chart cache expiry precedes its storage time",
            ));
        }
        sections.truncate(crate::provider::MAX_SECTIONS);
        for section in &mut sections {
            section.items.truncate(crate::provider::MAX_ITEMS_PER_SHELF);
        }
        let payload = Self {
            region,
            sections,
            stored_at,
            expires_at,
        };
        let _ = payload.encoded()?;
        Ok(payload)
    }

    /// Decodes a cache row while validating its key, provenance, and bounds.
    ///
    /// # Errors
    ///
    /// Returns a chart error when the stored document is corrupt, oversized,
    /// belongs to another region, or disagrees with the row timestamps.
    pub fn from_metadata_entry(
        expected_region: &RegionCode,
        entry: &MetadataCacheEntry,
    ) -> Result<Self, AppError> {
        if entry.payload().len() > MAX_CHART_CACHE_BYTES {
            return Err(chart_cache_error(
                "chart cache payload exceeds the input limit",
            ));
        }
        let payload = serde_json::from_str::<Self>(entry.payload())
            .map_err(|_| chart_cache_error("chart cache payload is invalid"))?;
        if &payload.region != expected_region {
            return Err(chart_cache_error(
                "chart cache region does not match its key",
            ));
        }
        if payload.stored_at != entry.stored_at() || payload.expires_at != entry.expires_at() {
            return Err(chart_cache_error(
                "chart cache provenance does not match its storage row",
            ));
        }
        if payload.sections.len() > crate::provider::MAX_SECTIONS
            || payload
                .sections
                .iter()
                .any(|section| section.items.len() > crate::provider::MAX_ITEMS_PER_SHELF)
        {
            return Err(chart_cache_error("chart cache payload exceeds view limits"));
        }
        Ok(payload)
    }

    /// Encodes the validated payload for [`crate::storage::Storage`].
    ///
    /// # Errors
    ///
    /// Returns a chart error if serialization fails or exceeds the byte cap.
    pub fn encoded(&self) -> Result<String, AppError> {
        let encoded = serde_json::to_string(self)
            .map_err(|_| chart_cache_error("chart cache payload could not be encoded"))?;
        if encoded.len() > MAX_CHART_CACHE_BYTES {
            return Err(chart_cache_error(
                "chart cache payload exceeds the encoded limit",
            ));
        }
        Ok(encoded)
    }

    #[must_use]
    pub const fn region(&self) -> &RegionCode {
        &self.region
    }

    #[must_use]
    pub fn sections(&self) -> &[ChartSection] {
        &self.sections
    }

    #[must_use]
    pub const fn stored_at(&self) -> i64 {
        self.stored_at
    }

    #[must_use]
    pub const fn expires_at(&self) -> i64 {
        self.expires_at
    }

    #[must_use]
    pub const fn stale_at(&self, observed_at: i64) -> bool {
        self.expires_at <= observed_at
    }

    pub(super) fn into_parts(self) -> (Vec<ChartSection>, i64, i64) {
        (self.sections, self.stored_at, self.expires_at)
    }
}

fn chart_cache_error(message: &'static str) -> AppError {
    AppError::new(AppErrorCategory::Charts, message)
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct ChartSelectionAnchor {
    pub(super) media_id: MediaId,
    pub(super) section_title: String,
    pub(super) section_ordinal: usize,
    pub(super) occurrence_in_section: usize,
    pub(super) global_occurrence: usize,
}

#[derive(Clone, Debug, Default, PartialEq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "public chart presentation flags and independent cache/live request legs are distinct protocol state"
)]
pub struct ChartState {
    pub(super) region: Option<RegionCode>,
    pub(super) generation: Generation,
    pub(super) active_generation: Option<Generation>,
    pub(super) loading: bool,
    pub(super) sections: Vec<ChartSection>,
    pub(super) selected_id: Option<MediaId>,
    pub(super) selected_index: Option<usize>,
    pub(super) selected_anchor: Option<ChartSelectionAnchor>,
    pub(super) stale: bool,
    pub(super) cached_at: Option<i64>,
    pub(super) error: Option<AppError>,
    pub(super) cache_pending: bool,
    pub(super) live_pending: bool,
    pub(super) cached_candidate: Option<ChartCachePayload>,
    pub(super) cache_observed_at: Option<i64>,
    pub(super) live_error: Option<AppError>,
}

impl ChartState {
    #[must_use]
    pub const fn region(&self) -> Option<&RegionCode> {
        self.region.as_ref()
    }

    #[must_use]
    pub const fn generation(&self) -> Generation {
        self.generation
    }

    #[must_use]
    pub const fn active_generation(&self) -> Option<Generation> {
        self.active_generation
    }

    #[must_use]
    pub const fn loading(&self) -> bool {
        self.loading
    }

    #[must_use]
    pub fn sections(&self) -> &[ChartSection] {
        &self.sections
    }

    #[must_use]
    pub const fn selected_id(&self) -> Option<&MediaId> {
        self.selected_id.as_ref()
    }

    #[must_use]
    pub const fn selected_index(&self) -> Option<usize> {
        self.selected_index
    }

    #[must_use]
    pub const fn stale(&self) -> bool {
        self.stale
    }

    #[must_use]
    pub const fn cached_at(&self) -> Option<i64> {
        self.cached_at
    }

    #[must_use]
    pub const fn error(&self) -> Option<&AppError> {
        self.error.as_ref()
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ArtworkState {
    pub(super) requested_url: Option<ArtworkUrl>,
    pub(super) ready_url: Option<ArtworkUrl>,
    pub(super) generation: Generation,
    pub(super) active_generation: Option<Generation>,
    pub(super) loading: bool,
    pub(super) error: Option<AppError>,
}

impl ArtworkState {
    #[must_use]
    pub const fn requested_url(&self) -> Option<&ArtworkUrl> {
        self.requested_url.as_ref()
    }

    #[must_use]
    pub const fn ready_url(&self) -> Option<&ArtworkUrl> {
        self.ready_url.as_ref()
    }

    #[must_use]
    pub const fn generation(&self) -> Generation {
        self.generation
    }

    #[must_use]
    pub const fn active_generation(&self) -> Option<Generation> {
        self.active_generation
    }

    #[must_use]
    pub const fn loading(&self) -> bool {
        self.loading
    }

    #[must_use]
    pub const fn error(&self) -> Option<&AppError> {
        self.error.as_ref()
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct SessionCheckpoint {
    pub queue: QueueSnapshot,
    pub playback: PlaybackSnapshot,
}

#[derive(Debug, Error)]
pub enum RestoreError {
    #[error("saved queue is invalid: {0}")]
    Queue(#[from] QueueError),
    #[error("saved queue selection does not match saved playback")]
    PlaybackMismatch,
    #[error("saved playback settings are invalid: {0}")]
    PlaybackSettings(#[source] ConfigError),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PodcastProgressCheckpoint {
    media_id: MediaId,
    playback_epoch: u64,
    position_ms: u64,
    duration_ms: Option<u64>,
    played: bool,
}

impl PodcastProgressCheckpoint {
    #[must_use]
    pub const fn new(
        media_id: MediaId,
        playback_epoch: u64,
        position_ms: u64,
        duration_ms: Option<u64>,
        played: bool,
    ) -> Self {
        Self {
            media_id,
            playback_epoch,
            position_ms,
            duration_ms,
            played,
        }
    }

    #[must_use]
    pub const fn media_id(&self) -> &MediaId {
        &self.media_id
    }

    #[must_use]
    pub const fn playback_epoch(&self) -> u64 {
        self.playback_epoch
    }

    #[must_use]
    pub const fn position_ms(&self) -> u64 {
        self.position_ms
    }

    #[must_use]
    pub const fn duration_ms(&self) -> Option<u64> {
        self.duration_ms
    }

    #[must_use]
    pub const fn played(&self) -> bool {
        self.played
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FadeActivity {
    In,
    Out,
}

impl FadeActivity {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::In => "in",
            Self::Out => "out",
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ResolverQuality {
    codec: Option<String>,
    format_id: Option<String>,
}

impl ResolverQuality {
    #[must_use]
    pub fn new(codec: Option<&str>, format_id: Option<&str>) -> Self {
        Self {
            codec: codec.and_then(bounded_media_label),
            format_id: format_id.and_then(bounded_media_label),
        }
    }

    #[must_use]
    pub fn from_resolved_stream(stream: &crate::resolver::ResolvedStream) -> Self {
        Self::new(stream.codec.as_deref(), stream.format_id.as_deref())
    }

    #[must_use]
    pub fn codec(&self) -> Option<&str> {
        self.codec.as_deref()
    }

    #[must_use]
    pub fn format_id(&self) -> Option<&str> {
        self.format_id.as_deref()
    }

    #[must_use]
    pub fn known(&self) -> bool {
        self.codec.is_some() || self.format_id.is_some()
    }
}

fn bounded_media_label(value: &str) -> Option<String> {
    let value = value
        .chars()
        .filter(|character| !character.is_control())
        .take(64)
        .collect::<String>();
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

#[derive(Clone, Debug, PartialEq)]
pub struct PlayerPresentation {
    pub(super) effective_volume: f64,
    pub(super) fade: Option<FadeActivity>,
    pub(super) quality: ResolverQuality,
    pub(super) preview_url: Option<crate::resolver::PreviewStreamUrl>,
    pub(super) analysis_url: Option<crate::resolver::AnalysisStreamUrl>,
    fade_in_ms: u64,
    fade_out_ms: u64,
}

impl PlayerPresentation {
    #[must_use]
    pub fn effective_volume(&self) -> f64 {
        self.effective_volume
    }

    #[must_use]
    pub const fn fade(&self) -> Option<FadeActivity> {
        self.fade
    }

    #[must_use]
    pub const fn quality(&self) -> &ResolverQuality {
        &self.quality
    }

    #[must_use]
    pub const fn preview_url(&self) -> Option<&crate::resolver::PreviewStreamUrl> {
        self.preview_url.as_ref()
    }

    #[must_use]
    pub const fn analysis_url(&self) -> Option<&crate::resolver::AnalysisStreamUrl> {
        self.analysis_url.as_ref()
    }

    #[must_use]
    pub const fn fade_in_ms(&self) -> u64 {
        self.fade_in_ms
    }

    #[must_use]
    pub const fn fade_out_ms(&self) -> u64 {
        self.fade_out_ms
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DependencyState {
    pub(super) report: Option<DoctorReport>,
    pub(super) checking: bool,
}

impl DependencyState {
    #[must_use]
    pub fn browsing_available(&self) -> bool {
        self.report
            .as_ref()
            .is_none_or(DoctorReport::browsing_available)
    }

    #[must_use]
    pub fn playback_available(&self) -> bool {
        self.report
            .as_ref()
            .is_none_or(DoctorReport::playback_available)
    }

    #[must_use]
    pub const fn checking(&self) -> bool {
        self.checking
    }

    #[must_use]
    pub fn rows(&self) -> &[DiagnosticRow] {
        self.report.as_ref().map_or(&[], |report| report.rows())
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct HistoryState {
    pub(super) generation: Generation,
    pub(super) active_generation: Option<Generation>,
    pub(super) loading: bool,
    pub(super) entries: Vec<HistoryEntry>,
    pub(super) selected_id: Option<i64>,
    pub(super) error: Option<AppError>,
}

impl HistoryState {
    #[must_use]
    pub const fn generation(&self) -> Generation {
        self.generation
    }

    #[must_use]
    pub const fn active_generation(&self) -> Option<Generation> {
        self.active_generation
    }

    #[must_use]
    pub const fn loading(&self) -> bool {
        self.loading
    }

    #[must_use]
    pub fn entries(&self) -> &[HistoryEntry] {
        &self.entries
    }

    #[must_use]
    pub const fn selected_id(&self) -> Option<i64> {
        self.selected_id
    }

    #[must_use]
    pub const fn error(&self) -> Option<&AppError> {
        self.error.as_ref()
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct FavoritesState {
    pub(super) generation: Generation,
    pub(super) active_generation: Option<Generation>,
    pub(super) loading: bool,
    pub(super) loaded: bool,
    pub(super) entries: Vec<FavoriteEntry>,
    pub(super) selected_id: Option<MediaId>,
    pub(super) pending_mutation: Option<PendingFavoriteMutation>,
    pub(super) error: Option<AppError>,
}

impl FavoritesState {
    #[must_use]
    pub const fn generation(&self) -> Generation {
        self.generation
    }
    #[must_use]
    pub const fn active_generation(&self) -> Option<Generation> {
        self.active_generation
    }
    #[must_use]
    pub const fn loading(&self) -> bool {
        self.loading
    }
    #[must_use]
    pub const fn loaded(&self) -> bool {
        self.loaded
    }
    #[must_use]
    pub fn entries(&self) -> &[FavoriteEntry] {
        &self.entries
    }
    #[must_use]
    pub const fn selected_id(&self) -> Option<&MediaId> {
        self.selected_id.as_ref()
    }
    #[must_use]
    pub const fn pending_mutation(&self) -> Option<&PendingFavoriteMutation> {
        self.pending_mutation.as_ref()
    }
    #[must_use]
    pub const fn error(&self) -> Option<&AppError> {
        self.error.as_ref()
    }
}

#[derive(Clone, Debug, PartialEq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "these flags represent independent presentation capabilities, not mutually exclusive states"
)]
pub(super) struct PresentationEnhancements {
    lyrics_external_sync_enabled: bool,
    animated_artwork_enabled: bool,
    visualizer_enabled: bool,
    notifications_enabled: bool,
    podcast_skip_backward_seconds: u64,
    podcast_skip_forward_seconds: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AppState {
    pub(super) search: SearchState,
    pub(super) charts: ChartState,
    pub(super) podcasts: PodcastState,
    pub(super) library: LibraryState,
    pub(super) artwork: ArtworkState,
    pub(super) dependencies: DependencyState,
    pub(super) history: HistoryState,
    pub(super) favorites: FavoritesState,
    pub(super) lyrics: LyricsState,
    pub(super) artwork_surface: ArtworkSurface,
    pub(super) queue: Queue,
    pub(super) playback: PlaybackSnapshot,
    pub(super) player_presentation: PlayerPresentation,
    pub(super) behavior: BehaviorConfig,
    pub(super) lyrics_enabled: bool,
    pub(super) presentation_enhancements: PresentationEnhancements,
    pub(super) diagnostics: Vec<Diagnostic>,
    pub(super) last_generation: Generation,
    pub(super) active_resolve_generation: Option<Generation>,
    pub(super) current_attempt_generation: Option<Generation>,
    pub(super) current_podcast_epoch: Option<u64>,
    pub(super) pending_radio_generation: Option<Generation>,
    pub(super) resume_radio_after_fill: bool,
    pub(super) history_recorded_generation: Option<Generation>,
    pub(super) notification_emitted_generation: Option<Generation>,
}

impl Default for AppState {
    fn default() -> Self {
        Self::new(Config::default())
    }
}

impl AppState {
    #[must_use]
    pub fn new(config: Config) -> Self {
        Self {
            search: SearchState::default(),
            charts: ChartState {
                region: Some(config.region.clone()),
                ..ChartState::default()
            },
            podcasts: PodcastState {
                requested_region: config.region.clone(),
                ..PodcastState::default()
            },
            library: LibraryState::default(),
            artwork: ArtworkState::default(),
            dependencies: DependencyState::default(),
            history: HistoryState::default(),
            favorites: FavoritesState::default(),
            lyrics: LyricsState::default(),
            artwork_surface: ArtworkSurface::default(),
            queue: Queue::default(),
            playback: PlaybackSnapshot {
                current: None,
                status: PlaybackStatus::Stopped,
                position_ms: 0,
                duration_ms: None,
                target_volume: config.playback.volume,
                playback_speed: config.podcast.speed,
            },
            player_presentation: PlayerPresentation {
                effective_volume: f64::from(config.playback.volume),
                fade: None,
                quality: ResolverQuality::default(),
                preview_url: None,
                analysis_url: None,
                fade_in_ms: config.playback.fade_in_ms,
                fade_out_ms: config.playback.fade_out_ms,
            },
            lyrics_enabled: config.lyrics.enabled,
            presentation_enhancements: PresentationEnhancements {
                lyrics_external_sync_enabled: config.lyrics.external_sync,
                animated_artwork_enabled: config.artwork.animated,
                visualizer_enabled: config.visualizer.enabled,
                notifications_enabled: config.notifications.enabled,
                podcast_skip_backward_seconds: config.podcast.skip_backward_seconds,
                podcast_skip_forward_seconds: config.podcast.skip_forward_seconds,
            },
            behavior: config.behavior,
            diagnostics: Vec::new(),
            last_generation: Generation::default(),
            active_resolve_generation: None,
            current_attempt_generation: None,
            current_podcast_epoch: None,
            pending_radio_generation: None,
            resume_radio_after_fill: false,
            history_recorded_generation: None,
            notification_emitted_generation: None,
        }
    }

    /// Restores validated durable session state without claiming that an old
    /// player process is still active.
    ///
    /// # Errors
    ///
    /// Returns a typed restore error when queue invariants, playback identity,
    /// or durable playback settings are invalid.
    pub fn restore_session(
        mut config: Config,
        checkpoint: SessionCheckpoint,
    ) -> Result<Self, RestoreError> {
        let queue = Queue::restore(checkpoint.queue)?;
        let queue_current = queue.current().map(|item| &item.media().id);
        if queue_current != checkpoint.playback.current.as_ref() {
            return Err(RestoreError::PlaybackMismatch);
        }

        config.playback.volume = checkpoint.playback.target_volume;
        config.podcast.speed = checkpoint.playback.playback_speed;
        config.validate().map_err(RestoreError::PlaybackSettings)?;

        let mut state = Self::new(config);
        state.queue = queue;
        state.playback = checkpoint.playback;
        if matches!(
            state.playback.status,
            PlaybackStatus::Resolving
                | PlaybackStatus::Buffering
                | PlaybackStatus::Playing
                | PlaybackStatus::Paused
        ) {
            state.playback.status = PlaybackStatus::Stopped;
        }
        state.player_presentation.effective_volume = f64::from(state.playback.target_volume);
        Ok(state)
    }

    #[must_use]
    pub const fn search(&self) -> &SearchState {
        &self.search
    }

    #[must_use]
    pub const fn charts(&self) -> &ChartState {
        &self.charts
    }

    #[must_use]
    pub const fn podcasts(&self) -> &PodcastState {
        &self.podcasts
    }

    #[must_use]
    pub const fn library(&self) -> &LibraryState {
        &self.library
    }

    #[must_use]
    pub const fn artwork(&self) -> &ArtworkState {
        &self.artwork
    }

    #[must_use]
    pub const fn artwork_surface(&self) -> ArtworkSurface {
        self.artwork_surface
    }

    #[must_use]
    pub const fn dependencies(&self) -> &DependencyState {
        &self.dependencies
    }

    #[must_use]
    pub const fn history(&self) -> &HistoryState {
        &self.history
    }

    #[must_use]
    pub const fn favorites(&self) -> &FavoritesState {
        &self.favorites
    }

    #[must_use]
    pub const fn lyrics(&self) -> &LyricsState {
        &self.lyrics
    }

    #[must_use]
    pub const fn lyrics_enabled(&self) -> bool {
        self.lyrics_enabled
    }

    #[must_use]
    pub const fn lyrics_external_sync_enabled(&self) -> bool {
        self.presentation_enhancements.lyrics_external_sync_enabled
    }

    #[must_use]
    pub const fn animated_artwork_enabled(&self) -> bool {
        self.presentation_enhancements.animated_artwork_enabled
    }

    #[must_use]
    pub const fn visualizer_enabled(&self) -> bool {
        self.presentation_enhancements.visualizer_enabled
    }

    #[must_use]
    pub const fn music_seek_seconds(&self) -> u64 {
        MUSIC_SEEK_SECONDS
    }

    #[must_use]
    pub const fn podcast_skip_backward_seconds(&self) -> u64 {
        self.presentation_enhancements.podcast_skip_backward_seconds
    }

    #[must_use]
    pub const fn podcast_skip_forward_seconds(&self) -> u64 {
        self.presentation_enhancements.podcast_skip_forward_seconds
    }

    #[must_use]
    pub const fn notifications_enabled(&self) -> bool {
        self.presentation_enhancements.notifications_enabled
    }

    #[must_use]
    pub const fn queue(&self) -> &Queue {
        &self.queue
    }

    #[must_use]
    pub const fn playback(&self) -> &PlaybackSnapshot {
        &self.playback
    }

    #[must_use]
    pub const fn player_presentation(&self) -> &PlayerPresentation {
        &self.player_presentation
    }

    #[must_use]
    pub const fn behavior(&self) -> &BehaviorConfig {
        &self.behavior
    }

    #[must_use]
    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    #[must_use]
    pub const fn current_resolve_generation(&self) -> Option<Generation> {
        self.active_resolve_generation
    }

    #[must_use]
    pub const fn current_attempt_generation(&self) -> Option<Generation> {
        self.current_attempt_generation
    }

    #[must_use]
    pub const fn current_podcast_epoch(&self) -> Option<u64> {
        self.current_podcast_epoch
    }

    #[must_use]
    pub const fn active_chart_generation(&self) -> Option<Generation> {
        self.charts.active_generation
    }

    #[must_use]
    pub const fn active_artwork_generation(&self) -> Option<Generation> {
        self.artwork.active_generation
    }

    #[must_use]
    pub const fn pending_radio_generation(&self) -> Option<Generation> {
        self.pending_radio_generation
    }
}
