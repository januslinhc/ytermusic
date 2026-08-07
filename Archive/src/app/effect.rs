use crate::domain::{ArtworkUrl, MediaId, MediaItem, RegionCode};
use crate::{
    auth::Browser,
    notifications::NowPlayingNotification,
    provider::{ChartCacheKey, LibrarySection},
};

use super::{
    ChartCachePayload, Generation, LyricsMediaItem, OpaqueContinuation, PodcastProgressCheckpoint,
    PodcastProviderId, SearchFilter, SessionCheckpoint,
};

#[derive(Clone, Debug, PartialEq)]
pub enum PlayerCommand {
    Pause,
    Resume,
    Volume(u8),
    SeekRelative { seconds: i64 },
}

#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    Search {
        generation: Generation,
        query: String,
        filter: SearchFilter,
    },
    SearchMore {
        generation: Generation,
        query: String,
        filter: SearchFilter,
        continuation: OpaqueContinuation,
    },
    LoadCharts {
        generation: Generation,
        region: RegionCode,
    },
    ReadChartCache {
        generation: Generation,
        region: RegionCode,
        key: ChartCacheKey,
    },
    StoreChartCache {
        key: ChartCacheKey,
        payload: ChartCachePayload,
    },
    LoadPodcast {
        generation: Generation,
        id: PodcastProviderId,
    },
    LoadPodcastRecommendations {
        generation: Generation,
        region: RegionCode,
    },
    ResolvePodcastRecommendation {
        generation: Generation,
        recommendation: crate::podcast_rankings::PodcastRecommendation,
    },
    LoadPodcastProgress {
        generation: Generation,
        media_id: MediaId,
    },
    ConnectAccount {
        browser: Browser,
    },
    LoadLibrary {
        generation: Generation,
        section: LibrarySection,
        continuation: Option<OpaqueContinuation>,
    },
    CheckDependencies,
    LoadHistory {
        generation: Generation,
        limit: usize,
    },
    LoadFavorites {
        generation: Generation,
    },
    AddFavorite {
        generation: Generation,
        item: MediaItem,
    },
    RemoveFavorite {
        generation: Generation,
        media_id: MediaId,
    },
    RecordHistory {
        item: MediaItem,
    },
    Notify(NowPlayingNotification),
    SavePodcastProgress(PodcastProgressCheckpoint),
    Resolve {
        generation: Generation,
        item: MediaItem,
        start_ms: Option<u64>,
    },
    Player(PlayerCommand),
    Persist(SessionCheckpoint),
    FillRadio {
        generation: Generation,
        seed: MediaId,
    },
    FetchArtwork {
        generation: Generation,
        url: ArtworkUrl,
    },
    ClearArtwork,
    LoadLyrics {
        generation: Generation,
        item: LyricsMediaItem,
    },
    ClearLyrics,
}
