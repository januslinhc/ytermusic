use crate::domain::{ArtworkUrl, MediaId, MediaItem, PlaybackStatus, RegionCode, RepeatMode};
use crate::resolver::{AnalysisStreamUrl, PreviewStreamUrl};
use crate::{
    auth::Browser,
    diagnostics::DoctorReport,
    lyrics::LyricsDocument,
    podcast_rankings::{PodcastRecommendationId, PodcastRecommendationPage},
    provider::{AuthenticationState, LibraryItem, LibrarySection, Page, Podcast},
    queue::QueueItemId,
    storage::{FavoriteEntry, HistoryEntry, PodcastProgress},
};

use super::{
    AppError, ArtworkSurface, ChartCachePayload, ChartSection, DiagnosticCategory, FadeActivity,
    FavoriteMutation, Generation, LibraryItemId, LyricsMediaId, LyricsMediaItem, PodcastProviderId,
    ResolverQuality, SearchFilter, SearchItemId, SearchPage,
};

#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    SearchSubmitted {
        query: String,
        filter: SearchFilter,
    },
    SearchCompleted {
        generation: Generation,
        result: Result<SearchPage, AppError>,
    },
    SearchSelectionChanged {
        id: SearchItemId,
    },
    SearchMoreRequested,
    ChartsRequested {
        region: RegionCode,
    },
    ChartsCompleted {
        generation: Generation,
        region: RegionCode,
        received_at: i64,
        result: Result<Vec<ChartSection>, AppError>,
    },
    CachedChartsCompleted {
        generation: Generation,
        region: RegionCode,
        observed_at: i64,
        result: Result<Option<ChartCachePayload>, AppError>,
    },
    ChartSelectionChanged {
        media_id: MediaId,
    },
    ChartRowSelectionChanged {
        item_index: usize,
    },
    OpenSelectedPodcast,
    PodcastRecommendationsRequested {
        region: RegionCode,
    },
    PodcastRecommendationsCompleted {
        generation: Generation,
        requested_region: RegionCode,
        result: Result<PodcastRecommendationPage, AppError>,
    },
    PodcastRecommendationSelectionChanged {
        id: PodcastRecommendationId,
    },
    OpenSelectedPodcastRecommendation,
    PodcastRecommendationResolved {
        generation: Generation,
        result: Result<PodcastProviderId, AppError>,
    },
    ClosePodcast,
    PodcastCompleted {
        generation: Generation,
        result: Result<Podcast, AppError>,
    },
    PodcastSelectionChanged {
        media_id: MediaId,
    },
    PlayPodcastEpisode {
        media_id: MediaId,
    },
    PodcastProgressLoaded {
        generation: Generation,
        progress: Option<PodcastProgress>,
    },
    AuthenticationChanged(AuthenticationState),
    ConnectAccountRequested {
        browser: Browser,
    },
    LibraryRequested {
        section: LibrarySection,
    },
    LibraryMoreRequested,
    LibrarySelectionChanged {
        id: LibraryItemId,
    },
    LibraryCompleted {
        generation: Generation,
        result: Result<Page<LibraryItem>, AppError>,
    },
    DependencyCheckRequested,
    DependencyReportLoaded(DoctorReport),
    HistoryRequested,
    HistorySelectionChanged {
        id: i64,
    },
    HistoryCompleted {
        generation: Generation,
        result: Result<Vec<HistoryEntry>, AppError>,
    },
    FavoritesRequested,
    FavoriteSelectionChanged {
        media_id: MediaId,
    },
    FavoriteToggleRequested {
        item: MediaItem,
    },
    FavoritesCompleted {
        generation: Generation,
        result: Result<Vec<FavoriteEntry>, AppError>,
    },
    FavoriteMutationCompleted {
        generation: Generation,
        media_id: MediaId,
        mutation: FavoriteMutation,
        result: Result<Vec<FavoriteEntry>, AppError>,
    },
    ArtworkRequested {
        url: ArtworkUrl,
    },
    ArtworkSurfaceChanged {
        surface: ArtworkSurface,
    },
    ArtworkCompleted {
        generation: Generation,
        result: Result<(), AppError>,
    },
    LyricsRequested {
        item: LyricsMediaItem,
    },
    LyricsCompleted {
        generation: Generation,
        media_id: LyricsMediaId,
        result: Result<Option<LyricsDocument>, AppError>,
    },
    ActivateSearchResult {
        index: usize,
    },
    EnqueueMedia {
        item: MediaItem,
    },
    EnqueueSelectedSearchResult,
    PlayMediaList {
        items: Vec<MediaItem>,
        selected_id: MediaId,
        shuffle_seed: Option<u64>,
    },
    PlayQueueItem {
        id: QueueItemId,
    },
    TogglePlayback,
    NextRequested,
    PreviousRequested,
    SeekRelativeRequested {
        seconds: i64,
    },
    PlayerProgress {
        generation: Generation,
        media_id: MediaId,
        position_ms: u64,
        duration_ms: Option<u64>,
    },
    PlayerStatusChanged {
        generation: Generation,
        status: PlaybackStatus,
    },
    ResolveSucceeded {
        generation: Generation,
    },
    ResolvedFormatUpdated {
        generation: Generation,
        quality: ResolverQuality,
    },
    PreviewStreamUpdated {
        generation: Generation,
        preview_url: Option<PreviewStreamUrl>,
    },
    AnalysisStreamUpdated {
        generation: Generation,
        stream_url: Option<AnalysisStreamUrl>,
    },
    PlaybackTelemetryUpdated {
        generation: Generation,
        effective_volume: f64,
        fade: Option<FadeActivity>,
    },
    ResolveFailed {
        generation: Generation,
        error: AppError,
    },
    PlayerEnded {
        generation: Generation,
    },
    TargetVolumeChanged(u8),
    RepeatModeChanged(RepeatMode),
    ShuffleEnabledChanged {
        enabled: bool,
        seed: u64,
    },
    QueueItemMovedBefore {
        id: QueueItemId,
        before: QueueItemId,
    },
    RadioEnabledChanged(bool),
    CheckRadioFill,
    RadioFillCompleted {
        generation: Generation,
        result: Result<Vec<MediaItem>, AppError>,
    },
    RuntimeDiagnostic {
        category: DiagnosticCategory,
        message: String,
        media_id: Option<MediaId>,
    },
}
