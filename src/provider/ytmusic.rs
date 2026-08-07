use std::{fmt, future::Future, sync::Arc, time::Duration};

use async_trait::async_trait;
use secrecy::{ExposeSecret, SecretString};
use url::Url;
use ytmapi_rs::{
    YtMusic, YtMusicBuilder,
    auth::{BrowserToken, noauth::NoAuthToken},
    client::Client as YtMusicClient,
    common::{Explicit, LyricsID, PlaylistID, PodcastID, Thumbnail, VideoID, YoutubeID},
    error::ErrorKind,
    parse::{
        Episode, EpisodeDuration, GetPodcast, LibraryArtist, LibraryPlaylist, LibraryPodcast,
        Lyrics, PlaylistItem, SearchResultAlbum, SearchResultArtist, SearchResultEpisode,
        SearchResultPlaylist, SearchResultPodcast, SearchResultSong, TableListSong,
        WatchPlaylistTrack,
    },
    query::{
        GetLibraryAlbumsQuery, GetLibraryArtistsQuery, GetLibraryPlaylistsQuery,
        GetLibraryPodcastsQuery, GetLibrarySongsQuery, GetLyricsIDQuery, GetLyricsQuery,
        GetPlaylistTracksQuery, GetPodcastQuery, GetWatchPlaylistQuery, SearchQuery,
        search::filteredsearch::{
            AlbumsFilter, ArtistsFilter, EpisodesFilter, PlaylistsFilter, PodcastsFilter,
            SongsFilter,
        },
    },
};

use crate::domain::{ChartSection, MediaId, MediaItem, MediaKind, RegionCode, SearchFilter};

use super::{
    AuthenticationState, ChartsQuery, LibraryItem, LibrarySection, MusicProvider, Page,
    PlainLyrics, Podcast, ProviderError, ProviderErrorKind, ProviderOperation, ProviderResult,
    SearchItem,
};

const YOUTUBE_MUSIC_PROVIDER: &str = "youtube-music";
const MAX_OPAQUE_ID_BYTES: usize = 512;
const PROVIDER_OPERATION_TIMEOUT: Duration = Duration::from_secs(30);
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const HTTP_READ_TIMEOUT: Duration = Duration::from_secs(15);
const HTTP_REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

#[async_trait]
pub trait YtMusicApi: Send + Sync {
    async fn lyrics_id(&self, video_id: &str) -> ProviderResult<Option<String>>;
    async fn lyrics(&self, lyrics_id: &str) -> ProviderResult<Option<String>>;

    async fn search_songs(&self, query: &str) -> ProviderResult<Page<SearchItem>>;
    async fn search_albums(&self, query: &str) -> ProviderResult<Page<SearchItem>>;
    async fn search_artists(&self, query: &str) -> ProviderResult<Page<SearchItem>>;
    async fn search_playlists(&self, query: &str) -> ProviderResult<Page<SearchItem>>;
    async fn search_podcasts(&self, query: &str) -> ProviderResult<Page<SearchItem>>;
    async fn search_episodes(&self, query: &str) -> ProviderResult<Page<SearchItem>>;

    async fn charts(&self, query: ChartsQuery) -> ProviderResult<Vec<ChartSection>>;
    async fn playlist(&self, id: &str) -> ProviderResult<Vec<MediaItem>>;
    async fn podcast(&self, id: &str) -> ProviderResult<Podcast>;
    async fn watch_playlist(&self, video_id: &str) -> ProviderResult<Vec<MediaItem>>;

    async fn library_songs(&self) -> ProviderResult<Page<LibraryItem>>;
    async fn library_albums(&self) -> ProviderResult<Page<LibraryItem>>;
    async fn library_artists(&self) -> ProviderResult<Page<LibraryItem>>;
    async fn library_playlists(&self) -> ProviderResult<Page<LibraryItem>>;
    async fn library_podcasts(&self) -> ProviderResult<Page<LibraryItem>>;
}

pub struct YtMusicProvider {
    api: Arc<dyn YtMusicApi>,
    authentication: AuthenticationState,
}

impl YtMusicProvider {
    #[must_use]
    pub fn with_api<A>(api: A, authentication: AuthenticationState) -> Self
    where
        A: YtMusicApi + 'static,
    {
        Self {
            api: Arc::new(api),
            authentication,
        }
    }

    /// Creates a provider backed by an anonymous `YouTube` Music session.
    ///
    /// # Errors
    ///
    /// Returns a redacted provider error if session initialization fails.
    pub async fn new_unauthenticated() -> ProviderResult<Self> {
        let client = with_operation_timeout(ProviderOperation::Authentication, async move {
            let builder = YtMusicBuilder::new_with_client(build_http_client()?);
            builder
                .build()
                .await
                .map_err(|error| map_upstream_error(error, ProviderOperation::Authentication))
        })
        .await?;
        Ok(Self::with_api(
            RealYtMusicApi::anonymous(client),
            AuthenticationState::Unauthenticated,
        ))
    }

    /// Creates a provider backed by an authenticated browser session.
    ///
    /// # Errors
    ///
    /// Returns a redacted provider error if the credential is rejected or
    /// session initialization fails.
    pub async fn from_cookie(cookie: SecretString) -> ProviderResult<Self> {
        let client = with_operation_timeout(ProviderOperation::Authentication, async move {
            let builder = YtMusicBuilder::new_with_client(build_http_client()?)
                .with_browser_token_cookie(cookie.expose_secret().to_owned());
            builder
                .build()
                .await
                .map_err(|error| map_upstream_error(error, ProviderOperation::Authentication))
        })
        .await?;
        Ok(Self::with_api(
            RealYtMusicApi::authenticated(client),
            AuthenticationState::Authenticated,
        ))
    }
}

impl fmt::Debug for YtMusicProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("YtMusicProvider")
            .field("authentication", &self.authentication)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl MusicProvider for YtMusicProvider {
    async fn search(&self, query: &str, filter: SearchFilter) -> ProviderResult<Page<SearchItem>> {
        match filter {
            SearchFilter::All => search_all_best_effort(self.api.as_ref(), query).await,
            SearchFilter::Songs => {
                with_operation_timeout(ProviderOperation::Search, self.api.search_songs(query))
                    .await
            }
            SearchFilter::Albums => {
                with_operation_timeout(ProviderOperation::Search, self.api.search_albums(query))
                    .await
            }
            SearchFilter::Artists => {
                with_operation_timeout(ProviderOperation::Search, self.api.search_artists(query))
                    .await
            }
            SearchFilter::Playlists => {
                with_operation_timeout(ProviderOperation::Search, self.api.search_playlists(query))
                    .await
            }
            SearchFilter::Podcasts => {
                with_operation_timeout(ProviderOperation::Search, self.api.search_podcasts(query))
                    .await
            }
            SearchFilter::Episodes => {
                with_operation_timeout(ProviderOperation::Search, self.api.search_episodes(query))
                    .await
            }
        }
    }

    async fn charts(&self, region: &RegionCode) -> ProviderResult<Vec<ChartSection>> {
        with_operation_timeout(
            ProviderOperation::Charts,
            self.api.charts(ChartsQuery::new(region.clone())),
        )
        .await
    }

    async fn playlist(&self, id: &str) -> ProviderResult<Vec<MediaItem>> {
        validate_opaque_id(id, ProviderOperation::Playlist)?;
        with_operation_timeout(ProviderOperation::Playlist, self.api.playlist(id)).await
    }

    async fn podcast(&self, id: &str) -> ProviderResult<Podcast> {
        validate_opaque_id(id, ProviderOperation::Podcast)?;
        with_operation_timeout(ProviderOperation::Podcast, self.api.podcast(id)).await
    }

    async fn radio(&self, seed: &MediaId) -> ProviderResult<Vec<MediaItem>> {
        if seed.provider != YOUTUBE_MUSIC_PROVIDER {
            return Err(invalid_response(ProviderOperation::Radio));
        }
        validate_media_id(&seed.video_id, ProviderOperation::Radio)?;
        let mut items = with_operation_timeout(
            ProviderOperation::Radio,
            self.api.watch_playlist(&seed.video_id),
        )
        .await?;
        items.retain(|item| item.id != *seed);
        Ok(items)
    }

    async fn lyrics(&self, id: &MediaId) -> ProviderResult<PlainLyrics> {
        if id.provider != YOUTUBE_MUSIC_PROVIDER {
            return Err(invalid_response(ProviderOperation::Lyrics));
        }
        validate_media_id(&id.video_id, ProviderOperation::Lyrics)?;
        let Some(lyrics_id) =
            with_operation_timeout(ProviderOperation::Lyrics, self.api.lyrics_id(&id.video_id))
                .await?
        else {
            return Err(ProviderError::new(
                ProviderOperation::Lyrics,
                ProviderErrorKind::NotFound,
            ));
        };
        validate_opaque_id(&lyrics_id, ProviderOperation::Lyrics)?;
        let Some(text) =
            with_operation_timeout(ProviderOperation::Lyrics, self.api.lyrics(&lyrics_id)).await?
        else {
            return Err(ProviderError::new(
                ProviderOperation::Lyrics,
                ProviderErrorKind::NotFound,
            ));
        };
        PlainLyrics::new(&text)
    }

    async fn library(&self, section: LibrarySection) -> ProviderResult<Page<LibraryItem>> {
        if self.authentication == AuthenticationState::Unauthenticated {
            return Err(ProviderError::new(
                ProviderOperation::Library,
                ProviderErrorKind::AuthenticationRequired,
            ));
        }
        with_operation_timeout(ProviderOperation::Library, async {
            match section {
                LibrarySection::Songs => self.api.library_songs().await,
                LibrarySection::Albums => self.api.library_albums().await,
                LibrarySection::Artists => self.api.library_artists().await,
                LibrarySection::Playlists => self.api.library_playlists().await,
                LibrarySection::Podcasts => self.api.library_podcasts().await,
                // ytmapi-rs 0.3.2 only exposes the "New Episodes" auto-playlist;
                // it is not the user's saved episode library.
                LibrarySection::Episodes => Err(unsupported(ProviderOperation::Library)),
            }
        })
        .await
    }

    fn authentication(&self) -> AuthenticationState {
        self.authentication
    }
}

enum RealClient {
    Anonymous(YtMusic<NoAuthToken>),
    Authenticated(YtMusic<BrowserToken>),
}

struct RealYtMusicApi {
    client: RealClient,
}

impl RealYtMusicApi {
    const fn anonymous(client: YtMusic<NoAuthToken>) -> Self {
        Self {
            client: RealClient::Anonymous(client),
        }
    }

    const fn authenticated(client: YtMusic<BrowserToken>) -> Self {
        Self {
            client: RealClient::Authenticated(client),
        }
    }

    async fn chart_playlist(&self, playlist_id: &str) -> ProviderResult<Vec<MediaItem>> {
        let query =
            || GetWatchPlaylistQuery::new_from_playlist_id(PlaylistID::from_raw(playlist_id));
        let result = match &self.client {
            RealClient::Anonymous(client) => client.query(query()).await,
            RealClient::Authenticated(client) => client.query(query()).await,
        }
        .map_err(|error| map_upstream_error(error, ProviderOperation::Charts))?;
        normalize_watch_tracks(result)
            .map_err(|error| ProviderError::new(ProviderOperation::Charts, error.kind))
    }
}

impl fmt::Debug for RealYtMusicApi {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let authentication = match self.client {
            RealClient::Anonymous(_) => AuthenticationState::Unauthenticated,
            RealClient::Authenticated(_) => AuthenticationState::Authenticated,
        };
        formatter
            .debug_struct("RealYtMusicApi")
            .field("authentication", &authentication)
            .finish_non_exhaustive()
    }
}

async fn resolve_chart_output<F, Request>(
    output: super::queries::ChartsQueryOutput,
    mut load_playlist: F,
) -> ProviderResult<Vec<ChartSection>>
where
    F: FnMut(String) -> Request,
    Request: Future<Output = ProviderResult<Vec<MediaItem>>>,
{
    let (sections, playlist_references) = output.into_parts();
    if !sections.is_empty() {
        return Ok(sections);
    }

    let mut hydrated = Vec::new();
    let mut first_error = None;
    for reference in playlist_references {
        let (title, playlist_id) = reference.into_parts();
        match load_playlist(playlist_id).await {
            Ok(items) if !items.is_empty() => hydrated.push(ChartSection::new(title, items)),
            Ok(_) => {}
            Err(error) => {
                first_error
                    .get_or_insert(ProviderError::new(ProviderOperation::Charts, error.kind));
            }
        }
    }

    if !hydrated.is_empty() {
        return Ok(hydrated);
    }
    Err(first_error.unwrap_or_else(|| invalid_response(ProviderOperation::Charts)))
}

#[async_trait]
impl YtMusicApi for RealYtMusicApi {
    async fn lyrics_id(&self, video_id: &str) -> ProviderResult<Option<String>> {
        let query = || GetLyricsIDQuery::new(VideoID::from_raw(video_id));
        let result = match &self.client {
            RealClient::Anonymous(client) => client.query(query()).await,
            RealClient::Authenticated(client) => client.query(query()).await,
        };
        match result {
            Ok(id) => Ok(Some(id.get_raw().to_owned())),
            Err(error) => {
                let error = map_upstream_error(error, ProviderOperation::Lyrics);
                if matches!(
                    error.kind,
                    ProviderErrorKind::InvalidResponse | ProviderErrorKind::NotFound
                ) {
                    Ok(None)
                } else {
                    Err(error)
                }
            }
        }
    }

    async fn lyrics(&self, lyrics_id: &str) -> ProviderResult<Option<String>> {
        let query = || GetLyricsQuery::new(LyricsID::from_raw(lyrics_id));
        let result: Result<Lyrics, _> = match &self.client {
            RealClient::Anonymous(client) => client.query(query()).await,
            RealClient::Authenticated(client) => client.query(query()).await,
        };
        match result {
            Ok(result) => Ok(Some(result.lyrics)),
            Err(error) => {
                let error = map_upstream_error(error, ProviderOperation::Lyrics);
                if matches!(
                    error.kind,
                    ProviderErrorKind::InvalidResponse | ProviderErrorKind::NotFound
                ) {
                    Ok(None)
                } else {
                    Err(error)
                }
            }
        }
    }

    async fn search_songs(&self, query: &str) -> ProviderResult<Page<SearchItem>> {
        let result = match &self.client {
            RealClient::Anonymous(client) => {
                client
                    .query(SearchQuery::new_filtered(query, SongsFilter))
                    .await
            }
            RealClient::Authenticated(client) => {
                client
                    .query(SearchQuery::new_filtered(query, SongsFilter))
                    .await
            }
        }
        .map_err(|error| map_upstream_error(error, ProviderOperation::Search))?;
        normalize_song_results(result)
    }

    async fn search_albums(&self, query: &str) -> ProviderResult<Page<SearchItem>> {
        let result = match &self.client {
            RealClient::Anonymous(client) => {
                client
                    .query(SearchQuery::new_filtered(query, AlbumsFilter))
                    .await
            }
            RealClient::Authenticated(client) => {
                client
                    .query(SearchQuery::new_filtered(query, AlbumsFilter))
                    .await
            }
        }
        .map_err(|error| map_upstream_error(error, ProviderOperation::Search))?;
        normalize_album_results(result)
    }

    async fn search_artists(&self, query: &str) -> ProviderResult<Page<SearchItem>> {
        let result = match &self.client {
            RealClient::Anonymous(client) => {
                client
                    .query(SearchQuery::new_filtered(query, ArtistsFilter))
                    .await
            }
            RealClient::Authenticated(client) => {
                client
                    .query(SearchQuery::new_filtered(query, ArtistsFilter))
                    .await
            }
        }
        .map_err(|error| map_upstream_error(error, ProviderOperation::Search))?;
        normalize_artist_results(result)
    }

    async fn search_playlists(&self, query: &str) -> ProviderResult<Page<SearchItem>> {
        let result = match &self.client {
            RealClient::Anonymous(client) => {
                client
                    .query(SearchQuery::new_filtered(query, PlaylistsFilter))
                    .await
            }
            RealClient::Authenticated(client) => {
                client
                    .query(SearchQuery::new_filtered(query, PlaylistsFilter))
                    .await
            }
        }
        .map_err(|error| map_upstream_error(error, ProviderOperation::Search))?;
        normalize_playlist_results(result)
    }

    async fn search_podcasts(&self, query: &str) -> ProviderResult<Page<SearchItem>> {
        let result = match &self.client {
            RealClient::Anonymous(client) => {
                client
                    .query(SearchQuery::new_filtered(query, PodcastsFilter))
                    .await
            }
            RealClient::Authenticated(client) => {
                client
                    .query(SearchQuery::new_filtered(query, PodcastsFilter))
                    .await
            }
        }
        .map_err(|error| map_upstream_error(error, ProviderOperation::Search))?;
        normalize_podcast_results(result)
    }

    async fn search_episodes(&self, query: &str) -> ProviderResult<Page<SearchItem>> {
        let result = match &self.client {
            RealClient::Anonymous(client) => {
                client
                    .query(SearchQuery::new_filtered(query, EpisodesFilter))
                    .await
            }
            RealClient::Authenticated(client) => {
                client
                    .query(SearchQuery::new_filtered(query, EpisodesFilter))
                    .await
            }
        }
        .map_err(|error| map_upstream_error(error, ProviderOperation::Search))?;
        normalize_episode_results(result)
    }

    async fn charts(&self, query: ChartsQuery) -> ProviderResult<Vec<ChartSection>> {
        let output = match &self.client {
            RealClient::Anonymous(client) => client.query(query).await,
            RealClient::Authenticated(client) => client.query(query).await,
        }
        .map_err(|error| map_upstream_error(error, ProviderOperation::Charts))?;
        resolve_chart_output(output, |playlist_id| async move {
            self.chart_playlist(&playlist_id).await
        })
        .await
    }

    async fn playlist(&self, id: &str) -> ProviderResult<Vec<MediaItem>> {
        let query = || GetPlaylistTracksQuery::new(PlaylistID::from_raw(id));
        let result = match &self.client {
            RealClient::Anonymous(client) => client.query(query()).await,
            RealClient::Authenticated(client) => client.query(query()).await,
        }
        .map_err(|error| map_upstream_error(error, ProviderOperation::Playlist))?;
        normalize_playlist_items(result)
    }

    async fn podcast(&self, id: &str) -> ProviderResult<Podcast> {
        let query = || GetPodcastQuery::new(PodcastID::from_raw(id));
        let result = match &self.client {
            RealClient::Anonymous(client) => client.query(query()).await,
            RealClient::Authenticated(client) => client.query(query()).await,
        }
        .map_err(|error| map_upstream_error(error, ProviderOperation::Podcast))?;
        normalize_podcast(id, result)
    }

    async fn watch_playlist(&self, video_id: &str) -> ProviderResult<Vec<MediaItem>> {
        let query = || GetWatchPlaylistQuery::new_from_video_id(VideoID::from_raw(video_id));
        let result = match &self.client {
            RealClient::Anonymous(client) => client.query(query()).await,
            RealClient::Authenticated(client) => client.query(query()).await,
        }
        .map_err(|error| map_upstream_error(error, ProviderOperation::Radio))?;
        normalize_watch_tracks(result)
    }

    async fn library_songs(&self) -> ProviderResult<Page<LibraryItem>> {
        let RealClient::Authenticated(client) = &self.client else {
            return Err(authentication_required());
        };
        let result = client
            .query(GetLibrarySongsQuery::default())
            .await
            .map_err(|error| map_upstream_error(error, ProviderOperation::Library))?;
        normalize_library_songs(result)
    }

    async fn library_albums(&self) -> ProviderResult<Page<LibraryItem>> {
        let RealClient::Authenticated(client) = &self.client else {
            return Err(authentication_required());
        };
        let result = client
            .query(GetLibraryAlbumsQuery::default())
            .await
            .map_err(|error| map_upstream_error(error, ProviderOperation::Library))?;
        normalize_library_albums(result)
    }

    async fn library_artists(&self) -> ProviderResult<Page<LibraryItem>> {
        let RealClient::Authenticated(client) = &self.client else {
            return Err(authentication_required());
        };
        let result = client
            .query(GetLibraryArtistsQuery::default())
            .await
            .map_err(|error| map_upstream_error(error, ProviderOperation::Library))?;
        normalize_library_artists(result)
    }

    async fn library_playlists(&self) -> ProviderResult<Page<LibraryItem>> {
        let RealClient::Authenticated(client) = &self.client else {
            return Err(authentication_required());
        };
        let result = client
            .query(GetLibraryPlaylistsQuery)
            .await
            .map_err(|error| map_upstream_error(error, ProviderOperation::Library))?;
        normalize_library_playlists(result)
    }

    async fn library_podcasts(&self) -> ProviderResult<Page<LibraryItem>> {
        let RealClient::Authenticated(client) = &self.client else {
            return Err(authentication_required());
        };
        let result = client
            .query(GetLibraryPodcastsQuery::default())
            .await
            .map_err(|error| map_upstream_error(error, ProviderOperation::Library))?;
        normalize_library_podcasts(result)
    }
}

/// Aggregates filtered search calls instead of using ytmapi-rs 0.3.2's
/// incomplete unfiltered parser. Categories are queried sequentially in this
/// fixed order: songs, albums, artists, playlists, podcasts, episodes.
/// Successful results, including cross-category duplicates, are appended in
/// that order. Category-specific continuation tokens cannot be merged and are
/// therefore discarded. The first error is returned only when every category
/// fails. One absolute operation deadline covers the sequence; reaching it
/// preserves earlier successes or the first earlier error and starts no later
/// category.
async fn search_all_best_effort(
    api: &dyn YtMusicApi,
    query: &str,
) -> ProviderResult<Page<SearchItem>> {
    let deadline = tokio::time::Instant::now() + PROVIDER_OPERATION_TIMEOUT;
    let searches = [
        api.search_songs(query),
        api.search_albums(query),
        api.search_artists(query),
        api.search_playlists(query),
        api.search_podcasts(query),
        api.search_episodes(query),
    ];
    let mut items = Vec::new();
    let mut stale = false;
    let mut first_error = None;
    let mut succeeded = false;

    for search in searches {
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        let Ok(result) = tokio::time::timeout_at(deadline, search).await else {
            break;
        };
        match result {
            Ok(page) => {
                succeeded = true;
                stale |= page.stale;
                items.extend(page.items);
            }
            Err(error) => {
                first_error.get_or_insert(error);
            }
        }
    }

    if succeeded {
        Ok(Page {
            items,
            continuation: None,
            stale,
        })
    } else {
        Err(first_error.unwrap_or_else(|| unavailable(ProviderOperation::Search)))
    }
}

async fn with_operation_timeout<T>(
    operation: ProviderOperation,
    future: impl Future<Output = ProviderResult<T>>,
) -> ProviderResult<T> {
    tokio::time::timeout(PROVIDER_OPERATION_TIMEOUT, future)
        .await
        .unwrap_or_else(|_| Err(unavailable(operation)))
}

fn build_http_client() -> ProviderResult<YtMusicClient> {
    let client = reqwest::Client::builder()
        .use_rustls_tls()
        .connect_timeout(HTTP_CONNECT_TIMEOUT)
        .read_timeout(HTTP_READ_TIMEOUT)
        .timeout(HTTP_REQUEST_TIMEOUT)
        .build()
        .map_err(|_| unavailable(ProviderOperation::Authentication))?;
    Ok(YtMusicClient::new_from_reqwest_client(client))
}

fn normalize_song_results(results: Vec<SearchResultSong>) -> ProviderResult<Page<SearchItem>> {
    results
        .into_iter()
        .map(normalize_search_song)
        .map(|result| result.map(SearchItem::Playable))
        .collect::<ProviderResult<Vec<_>>>()
        .map(page)
}

fn normalize_album_results(results: Vec<SearchResultAlbum>) -> ProviderResult<Page<SearchItem>> {
    results
        .into_iter()
        .map(|album| {
            normalize_search_album(album, ProviderOperation::Search).map(SearchItem::Album)
        })
        .collect::<ProviderResult<Vec<_>>>()
        .map(page)
}

fn normalize_artist_results(results: Vec<SearchResultArtist>) -> ProviderResult<Page<SearchItem>> {
    results
        .into_iter()
        .map(normalize_search_artist)
        .map(|result| result.map(SearchItem::Artist))
        .collect::<ProviderResult<Vec<_>>>()
        .map(page)
}

fn normalize_playlist_results(
    results: Vec<SearchResultPlaylist>,
) -> ProviderResult<Page<SearchItem>> {
    results
        .into_iter()
        .map(normalize_search_playlist)
        .collect::<ProviderResult<Vec<_>>>()
        .map(page)
}

fn normalize_podcast_results(
    results: Vec<SearchResultPodcast>,
) -> ProviderResult<Page<SearchItem>> {
    results
        .into_iter()
        .map(|podcast| {
            normalize_search_podcast(podcast, ProviderOperation::Search).map(SearchItem::Podcast)
        })
        .collect::<ProviderResult<Vec<_>>>()
        .map(page)
}

fn normalize_episode_results(
    results: Vec<SearchResultEpisode>,
) -> ProviderResult<Page<SearchItem>> {
    results
        .into_iter()
        .map(normalize_search_episode)
        .map(|result| result.map(SearchItem::Playable))
        .collect::<ProviderResult<Vec<_>>>()
        .map(page)
}

fn normalize_search_song(song: SearchResultSong) -> ProviderResult<MediaItem> {
    media_item(
        song.video_id.get_raw(),
        MediaKind::Song,
        song.title,
        nonempty_values([song.artist]),
        song.album.and_then(|album| nonempty(album.name)),
        duration_ms(&song.duration),
        artwork_url(&song.thumbnails),
        matches!(song.explicit, Explicit::IsExplicit),
        ProviderOperation::Search,
    )
}

fn normalize_search_album(
    album: SearchResultAlbum,
    operation: ProviderOperation,
) -> ProviderResult<super::BrowseItem> {
    browse_item(
        album.album_id.get_raw(),
        album.title,
        nonempty(album.artist),
        artwork_url(&album.thumbnails),
        operation,
    )
}

fn normalize_search_artist(artist: SearchResultArtist) -> ProviderResult<super::BrowseItem> {
    browse_item(
        artist.browse_id.get_raw(),
        artist.artist,
        artist.subscribers.and_then(nonempty),
        artwork_url(&artist.thumbnails),
        ProviderOperation::Search,
    )
}

fn normalize_search_playlist(playlist: SearchResultPlaylist) -> ProviderResult<SearchItem> {
    match playlist {
        SearchResultPlaylist::Featured(playlist) => browse_item(
            playlist.playlist_id.get_raw(),
            playlist.title,
            nonempty(playlist.author),
            artwork_url(&playlist.thumbnails),
            ProviderOperation::Search,
        )
        .map(SearchItem::Playlist),
        SearchResultPlaylist::Community(playlist) => browse_item(
            playlist.playlist_id.get_raw(),
            playlist.title,
            nonempty(playlist.author),
            artwork_url(&playlist.thumbnails),
            ProviderOperation::Search,
        )
        .map(SearchItem::Playlist),
        SearchResultPlaylist::Podcast(podcast) => {
            normalize_search_podcast(podcast, ProviderOperation::Search).map(SearchItem::Podcast)
        }
        _ => Err(invalid_response(ProviderOperation::Search)),
    }
}

fn normalize_search_podcast(
    podcast: SearchResultPodcast,
    operation: ProviderOperation,
) -> ProviderResult<super::BrowseItem> {
    browse_item(
        podcast.podcast_id.get_raw(),
        podcast.title,
        nonempty(podcast.publisher),
        artwork_url(&podcast.thumbnails),
        operation,
    )
}

fn normalize_search_episode(episode: SearchResultEpisode) -> ProviderResult<MediaItem> {
    media_item(
        episode.episode_id.get_raw(),
        MediaKind::PodcastEpisode,
        episode.title,
        nonempty_values([episode.channel_name]),
        None,
        None,
        artwork_url(&episode.thumbnails),
        false,
        ProviderOperation::Search,
    )
}

fn normalize_podcast(id: &str, detail: GetPodcast) -> ProviderResult<Podcast> {
    validate_opaque_id(id, ProviderOperation::Podcast)?;
    let creators = nonempty_values(detail.channels.iter().map(|channel| channel.name.clone()));
    let title = detail.title;
    let episodes = detail
        .episodes
        .into_iter()
        .map(|episode| {
            normalize_podcast_episode(
                episode,
                creators.clone(),
                Some(title.clone()),
                ProviderOperation::Podcast,
            )
        })
        .collect::<ProviderResult<Vec<_>>>()?;
    Ok(Podcast {
        id: id.to_owned(),
        title,
        creators,
        description: nonempty(detail.description),
        artwork_url: None,
        episodes,
    })
}

fn normalize_podcast_episode(
    episode: Episode,
    creators: Vec<String>,
    collection: Option<String>,
    operation: ProviderOperation,
) -> ProviderResult<MediaItem> {
    media_item(
        episode.episode_id.get_raw(),
        MediaKind::PodcastEpisode,
        episode.title,
        creators,
        collection.and_then(nonempty),
        duration_ms(&episode.total_duration),
        artwork_url(&episode.thumbnails),
        false,
        operation,
    )
}

fn normalize_watch_tracks(results: Vec<WatchPlaylistTrack>) -> ProviderResult<Vec<MediaItem>> {
    results
        .into_iter()
        .map(|track| {
            media_item(
                track.video_id.get_raw(),
                MediaKind::Song,
                track.title,
                nonempty_values([track.author]),
                None,
                duration_ms(&track.duration),
                artwork_url(&track.thumbnails),
                false,
                ProviderOperation::Radio,
            )
        })
        .collect()
}

fn normalize_playlist_items(results: Vec<PlaylistItem>) -> ProviderResult<Vec<MediaItem>> {
    let mut items = Vec::with_capacity(results.len());
    for result in results {
        let item = match result {
            PlaylistItem::Song(song) => {
                if song.is_available {
                    Some(media_item(
                        song.video_id.get_raw(),
                        MediaKind::Song,
                        song.title,
                        nonempty_values(song.artists.into_iter().map(|artist| artist.name)),
                        nonempty(song.album.name),
                        duration_ms(&song.duration),
                        artwork_url(&song.thumbnails),
                        matches!(song.explicit, Explicit::IsExplicit),
                        ProviderOperation::Playlist,
                    )?)
                } else {
                    None
                }
            }
            PlaylistItem::Video(video) => {
                if video.is_available {
                    Some(media_item(
                        video.video_id.get_raw(),
                        MediaKind::Video,
                        video.title,
                        nonempty_values([video.channel_name]),
                        None,
                        duration_ms(&video.duration),
                        artwork_url(&video.thumbnails),
                        false,
                        ProviderOperation::Playlist,
                    )?)
                } else {
                    None
                }
            }
            PlaylistItem::Episode(episode) => {
                if episode.is_available {
                    let duration = match episode.duration {
                        EpisodeDuration::Live => None,
                        EpisodeDuration::Recorded { duration } => duration_ms(&duration),
                    };
                    Some(media_item(
                        episode.episode_id.get_raw(),
                        MediaKind::PodcastEpisode,
                        episode.title,
                        nonempty_values([episode.podcast_name]),
                        None,
                        duration,
                        artwork_url(&episode.thumbnails),
                        false,
                        ProviderOperation::Playlist,
                    )?)
                } else {
                    None
                }
            }
            PlaylistItem::UploadSong(song) => Some(media_item(
                song.video_id.get_raw(),
                MediaKind::Song,
                song.title,
                nonempty_values(song.artists.into_iter().map(|artist| artist.name)),
                song.album.and_then(|album| nonempty(album.name)),
                duration_ms(&song.duration),
                artwork_url(&song.thumbnails),
                false,
                ProviderOperation::Playlist,
            )?),
        };
        items.extend(item);
    }
    Ok(items)
}

fn normalize_library_songs(results: Vec<TableListSong>) -> ProviderResult<Page<LibraryItem>> {
    let mut items = Vec::with_capacity(results.len());
    for song in results {
        if song.is_available {
            items.push(LibraryItem::Playable(media_item(
                song.video_id.get_raw(),
                MediaKind::Song,
                song.title,
                nonempty_values(song.artists.into_iter().map(|artist| artist.name)),
                nonempty(song.album.name),
                duration_ms(&song.duration),
                artwork_url(&song.thumbnails),
                matches!(song.explicit, Explicit::IsExplicit),
                ProviderOperation::Library,
            )?));
        }
    }
    Ok(page(items))
}

fn normalize_library_albums(results: Vec<SearchResultAlbum>) -> ProviderResult<Page<LibraryItem>> {
    results
        .into_iter()
        .map(|album| {
            normalize_search_album(album, ProviderOperation::Library).map(LibraryItem::Album)
        })
        .collect::<ProviderResult<Vec<_>>>()
        .map(page)
}

fn normalize_library_artists(results: Vec<LibraryArtist>) -> ProviderResult<Page<LibraryItem>> {
    results
        .into_iter()
        .map(|artist| {
            browse_item(
                artist.channel_id.get_raw(),
                artist.artist,
                nonempty(artist.byline),
                None,
                ProviderOperation::Library,
            )
            .map(LibraryItem::Artist)
        })
        .collect::<ProviderResult<Vec<_>>>()
        .map(page)
}

fn normalize_library_playlists(results: Vec<LibraryPlaylist>) -> ProviderResult<Page<LibraryItem>> {
    results
        .into_iter()
        .map(|playlist| {
            browse_item(
                playlist.playlist_id.get_raw(),
                playlist.title,
                nonempty(playlist.author),
                artwork_url(&playlist.thumbnails),
                ProviderOperation::Library,
            )
            .map(LibraryItem::Playlist)
        })
        .collect::<ProviderResult<Vec<_>>>()
        .map(page)
}

fn normalize_library_podcasts(results: Vec<LibraryPodcast>) -> ProviderResult<Page<LibraryItem>> {
    results
        .into_iter()
        .map(|podcast| {
            let creators =
                nonempty_values(podcast.channels.into_iter().map(|channel| channel.name));
            browse_item(
                podcast.podcast_id.get_raw(),
                podcast.title,
                if creators.is_empty() {
                    None
                } else {
                    Some(creators.join(", "))
                },
                artwork_url(&podcast.thumbnails),
                ProviderOperation::Library,
            )
            .map(LibraryItem::Podcast)
        })
        .collect::<ProviderResult<Vec<_>>>()
        .map(page)
}

#[allow(clippy::too_many_arguments)]
fn media_item(
    raw_id: &str,
    kind: MediaKind,
    title: String,
    creators: Vec<String>,
    collection: Option<String>,
    duration_ms: Option<u64>,
    artwork_url: Option<Url>,
    explicit: bool,
    operation: ProviderOperation,
) -> ProviderResult<MediaItem> {
    validate_media_id(raw_id, operation)?;
    Ok(MediaItem {
        id: MediaId {
            provider: YOUTUBE_MUSIC_PROVIDER.to_owned(),
            video_id: raw_id.to_owned(),
        },
        kind,
        title,
        creators,
        collection,
        duration_ms,
        artwork_url,
        explicit,
    })
}

fn browse_item(
    raw_id: &str,
    title: String,
    subtitle: Option<String>,
    artwork_url: Option<Url>,
    operation: ProviderOperation,
) -> ProviderResult<super::BrowseItem> {
    validate_opaque_id(raw_id, operation)?;
    Ok(super::BrowseItem {
        id: raw_id.to_owned(),
        title,
        subtitle,
        artwork_url,
    })
}

fn page<T>(items: Vec<T>) -> Page<T> {
    // ytmapi-rs 0.3.2's typed `query` API returns the parsed page items but
    // discards its continuation token. Real normalized pages therefore use
    // `continuation: None` and `stale: false` instead of inventing state.
    Page {
        items,
        continuation: None,
        stale: false,
    }
}

fn nonempty(value: String) -> Option<String> {
    if value.trim().is_empty() {
        None
    } else {
        Some(value)
    }
}

fn nonempty_values(values: impl IntoIterator<Item = String>) -> Vec<String> {
    values.into_iter().filter_map(nonempty).collect()
}

fn duration_ms(value: &str) -> Option<u64> {
    let parts = value.trim().split(':').collect::<Vec<_>>();
    if parts.is_empty() || parts.len() > 3 {
        return None;
    }
    let mut total_seconds = 0_u64;
    for (index, part) in parts.iter().enumerate() {
        let component = part.parse::<u64>().ok()?;
        if index > 0 && component >= 60 {
            return None;
        }
        total_seconds = total_seconds.checked_mul(60)?.checked_add(component)?;
    }
    total_seconds.checked_mul(1_000)
}

fn artwork_url(thumbnails: &[Thumbnail]) -> Option<Url> {
    thumbnails
        .iter()
        .filter_map(|thumbnail| {
            Url::parse(&thumbnail.url)
                .ok()
                .filter(|url| matches!(url.scheme(), "http" | "https") && url.has_host())
                .map(|url| (thumbnail.width.saturating_mul(thumbnail.height), url))
        })
        .max_by_key(|(area, _)| *area)
        .map(|(_, url)| url)
}

fn map_upstream_error(error: ytmapi_rs::Error, operation: ProviderOperation) -> ProviderError {
    let kind = match error.into_kind() {
        ErrorKind::Header | ErrorKind::OAuthTokenExpired { .. } => {
            ProviderErrorKind::AuthenticationRequired
        }
        ErrorKind::OtherErrorCodeInResponse {
            code: 401 | 403, ..
        } => ProviderErrorKind::AuthenticationRequired,
        ErrorKind::OtherErrorCodeInResponse { code: 404, .. } => ProviderErrorKind::NotFound,
        ErrorKind::JsonParsing(_)
        | ErrorKind::InvalidResponse { .. }
        | ErrorKind::UnableToSerializeGoogleOAuthToken { .. }
        | ErrorKind::UnableToParseYtCfg { .. }
        | ErrorKind::ApiStatusFailed => ProviderErrorKind::InvalidResponse,
        _ => ProviderErrorKind::Unavailable,
    };
    ProviderError::new(operation, kind)
}

const fn authentication_required() -> ProviderError {
    ProviderError::new(
        ProviderOperation::Library,
        ProviderErrorKind::AuthenticationRequired,
    )
}

const fn unsupported(operation: ProviderOperation) -> ProviderError {
    ProviderError::new(operation, ProviderErrorKind::Unsupported)
}

const fn unavailable(operation: ProviderOperation) -> ProviderError {
    ProviderError::new(operation, ProviderErrorKind::Unavailable)
}

fn validate_opaque_id(value: &str, operation: ProviderOperation) -> ProviderResult<()> {
    validate_id(value, MAX_OPAQUE_ID_BYTES, operation)
}

fn validate_media_id(value: &str, operation: ProviderOperation) -> ProviderResult<()> {
    validate_id(value, super::MAX_VIDEO_ID_BYTES, operation)
}

fn validate_id(value: &str, max_bytes: usize, operation: ProviderOperation) -> ProviderResult<()> {
    if value.is_empty()
        || value.len() > max_bytes
        || value
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
    {
        return Err(invalid_response(operation));
    }
    Ok(())
}

const fn invalid_response(operation: ProviderOperation) -> ProviderError {
    ProviderError::new(operation, ProviderErrorKind::InvalidResponse)
}

#[cfg(test)]
mod normalization_tests {
    use std::sync::{Arc, Mutex};

    use serde::de::DeserializeOwned;
    use serde_json::{Value, json};
    use ytmapi_rs::parse::{
        GetPodcast, PlaylistItem, SearchResultAlbum, SearchResultArtist, SearchResultEpisode,
        SearchResultPlaylist, SearchResultPodcast, SearchResultSong, WatchPlaylistTrack,
    };

    use crate::{
        domain::{ChartSection, MediaId, MediaItem, MediaKind},
        provider::{ProviderError, ProviderErrorKind, ProviderOperation},
    };

    use super::super::{charts::ChartPlaylistReference, queries::ChartsQueryOutput};

    use super::{
        build_http_client, normalize_album_results, normalize_artist_results,
        normalize_episode_results, normalize_playlist_items, normalize_playlist_results,
        normalize_podcast, normalize_podcast_results, normalize_song_results,
        normalize_watch_tracks, resolve_chart_output,
    };

    fn chart_track(video_id: &str) -> MediaItem {
        MediaItem {
            id: MediaId {
                provider: "youtube-music".to_owned(),
                video_id: video_id.to_owned(),
            },
            kind: MediaKind::Song,
            title: format!("Track {video_id}"),
            creators: vec!["Fixture artist".to_owned()],
            collection: None,
            duration_ms: None,
            artwork_url: None,
            explicit: false,
        }
    }

    #[tokio::test]
    async fn chart_playlist_references_are_hydrated_in_order_and_partial_failure_is_tolerated()
    -> Result<(), ProviderError> {
        let output = ChartsQueryOutput::from_playlist_references(vec![
            ChartPlaylistReference::new("Unavailable chart", "FAILED_CHART"),
            ChartPlaylistReference::new("Trending 20 Japan", "JP_CHART_FIXTURE"),
        ]);
        let calls = Arc::new(Mutex::new(Vec::new()));
        let observed_calls = Arc::clone(&calls);

        let sections = resolve_chart_output(output, move |playlist_id| {
            let observed_calls = Arc::clone(&observed_calls);
            async move {
                observed_calls
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(playlist_id.clone());
                if playlist_id == "FAILED_CHART" {
                    Err(ProviderError::new(
                        ProviderOperation::Playlist,
                        ProviderErrorKind::Unavailable,
                    ))
                } else {
                    Ok(vec![chart_track("jp_track_01")])
                }
            }
        })
        .await?;

        assert_eq!(
            *calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            ["FAILED_CHART", "JP_CHART_FIXTURE"]
        );
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].title(), "Trending 20 Japan");
        assert_eq!(sections[0].items()[0].id.video_id, "jp_track_01");
        Ok(())
    }

    #[tokio::test]
    async fn chart_legacy_sections_bypass_playlist_hydration() -> Result<(), ProviderError> {
        let legacy = ChartSection::new("Top songs", vec![chart_track("legacy_track")]);
        let output = ChartsQueryOutput::from_sections(vec![legacy]);

        let sections = resolve_chart_output(output, |_playlist_id| async {
            panic!("legacy chart output must not hydrate playlists");
        })
        .await?;

        assert_eq!(sections[0].title(), "Top songs");
        assert_eq!(sections[0].items()[0].id.video_id, "legacy_track");
        Ok(())
    }

    fn typed<T: DeserializeOwned>(value: Value) -> Result<T, serde_json::Error> {
        serde_json::from_value(value)
    }

    fn thumbnail() -> Value {
        json!({
            "height": 128,
            "width": 128,
            "url": "https://signed.invalid/art.jpg?signature=ARTWORK_SENTINEL"
        })
    }

    #[test]
    fn bounded_http_client_construction_is_fallible_and_offline()
    -> Result<(), Box<dyn std::error::Error>> {
        let _client = build_http_client()?;
        Ok(())
    }

    #[test]
    fn typed_search_results_map_to_normalized_contract_models()
    -> Result<(), Box<dyn std::error::Error>> {
        let song: SearchResultSong = typed(json!({
            "title": "Neon Harbour",
            "artist": "Harbour Unit",
            "album": {"name": "Midnight Ferries", "id": "MPREb_album"},
            "duration": "3:21",
            "plays": "42",
            "explicit": "IsExplicit",
            "video_id": "song_01",
            "thumbnails": [thumbnail()]
        }))?;
        let album: SearchResultAlbum = typed(json!({
            "title": "Midnight Ferries",
            "artist": "Harbour Unit",
            "year": "2026",
            "explicit": "NotExplicit",
            "album_id": "MPREb_album",
            "album_type": "Album",
            "thumbnails": [thumbnail()]
        }))?;
        let artist: SearchResultArtist = typed(json!({
            "artist": "Harbour Unit",
            "subscribers": null,
            "browse_id": "UC_harbour",
            "thumbnails": [thumbnail()]
        }))?;
        let playlist: SearchResultPlaylist = typed(json!({
            "Featured": {
                "title": "Night Ferries",
                "author": "YouTube Music",
                "songs": "50 songs",
                "playlist_id": "VL_night",
                "thumbnails": [thumbnail()]
            }
        }))?;
        let podcast: SearchResultPodcast = typed(json!({
            "title": "Signal Stories",
            "publisher": "Signal Network",
            "podcast_id": "MPSP_signal",
            "thumbnails": [thumbnail()]
        }))?;
        let episode: SearchResultEpisode = typed(json!({
            "title": "The Rust Terminal",
            "date": {"Recorded": {"date": "Jul 25, 2026"}},
            "channel_name": "Signal Stories",
            "episode_id": "MPSPE_terminal",
            "thumbnails": [thumbnail()]
        }))?;

        let songs = normalize_song_results(vec![song])?;
        let albums = normalize_album_results(vec![album])?;
        let artists = normalize_artist_results(vec![artist])?;
        let playlists = normalize_playlist_results(vec![playlist])?;
        let podcasts = normalize_podcast_results(vec![podcast])?;
        let episodes = normalize_episode_results(vec![episode])?;

        let super::SearchItem::Playable(song) = &songs.items[0] else {
            return Err("song must be playable".into());
        };
        assert_eq!(song.kind, MediaKind::Song);
        assert_eq!(song.duration_ms, Some(201_000));
        assert_eq!(song.collection.as_deref(), Some("Midnight Ferries"));
        assert!(song.explicit);
        assert!(matches!(albums.items[0], super::SearchItem::Album(_)));
        assert!(matches!(artists.items[0], super::SearchItem::Artist(_)));
        assert!(matches!(playlists.items[0], super::SearchItem::Playlist(_)));
        assert!(matches!(podcasts.items[0], super::SearchItem::Podcast(_)));
        let super::SearchItem::Playable(episode) = &episodes.items[0] else {
            return Err("episode must be playable".into());
        };
        assert_eq!(episode.kind, MediaKind::PodcastEpisode);
        assert_eq!(episode.creators, ["Signal Stories"]);

        for page in [&songs, &albums, &artists, &playlists, &podcasts, &episodes] {
            assert_eq!(page.continuation, None);
            assert!(!page.stale);
        }
        Ok(())
    }

    #[test]
    fn typed_normalization_rejects_invalid_opaque_ids() -> Result<(), Box<dyn std::error::Error>> {
        let song: SearchResultSong = typed(json!({
            "title": "Invalid",
            "artist": "Fixture",
            "album": null,
            "duration": "1:00",
            "plays": "0",
            "explicit": "NotExplicit",
            "video_id": "contains whitespace",
            "thumbnails": []
        }))?;

        let Err(error) = normalize_song_results(vec![song]) else {
            panic!("whitespace in an opaque media id must fail");
        };
        assert_eq!(error.kind, super::ProviderErrorKind::InvalidResponse);
        assert_eq!(error.operation, super::ProviderOperation::Search);
        Ok(())
    }

    #[test]
    fn typed_podcast_detail_retains_requested_id_and_maps_episodes()
    -> Result<(), Box<dyn std::error::Error>> {
        let detail: GetPodcast = typed(json!({
            "channels": [{"name": "Signal Network", "id": "UC_signal"}],
            "title": "Signal Stories",
            "description": "Stories from the edge",
            "library_status": "BOOKMARK_BORDER",
            "episodes": [{
                "title": "The Rust Terminal",
                "description": "An episode",
                "total_duration": "1:02:03",
                "remaining_duration": "1:02:03",
                "date": "Jul 25, 2026",
                "episode_id": "MPSPE_terminal",
                "thumbnails": [thumbnail()]
            }]
        }))?;

        let podcast = normalize_podcast("MPSP_signal", detail)?;
        assert_eq!(podcast.id, "MPSP_signal");
        assert_eq!(podcast.creators, ["Signal Network"]);
        assert_eq!(
            podcast.description.as_deref(),
            Some("Stories from the edge")
        );
        assert_eq!(podcast.episodes.len(), 1);
        assert_eq!(podcast.episodes[0].kind, MediaKind::PodcastEpisode);
        assert_eq!(podcast.episodes[0].duration_ms, Some(3_723_000));
        Ok(())
    }

    #[test]
    fn typed_watch_and_playlist_results_preserve_media_kinds()
    -> Result<(), Box<dyn std::error::Error>> {
        let watch: WatchPlaylistTrack = typed(json!({
            "title": "Radio song",
            "author": "Radio artist",
            "duration": "4:05",
            "thumbnails": [],
            "video_id": "radio_song"
        }))?;
        let episode: PlaylistItem = typed(json!({
            "Episode": {
                "episode_id": "MPSPE_queue",
                "track_no": 1,
                "date": {"Recorded": {"date": "Jul 25, 2026"}},
                "duration": {"Recorded": {"duration": "42:10"}},
                "title": "Queued episode",
                "podcast_name": "Signal Stories",
                "podcast_id": "MPSP_signal",
                "like_status": "INDIFFERENT",
                "thumbnails": [],
                "is_available": true
            }
        }))?;

        let watch = normalize_watch_tracks(vec![watch])?;
        let playlist = normalize_playlist_items(vec![episode])?;
        assert_eq!(watch[0].kind, MediaKind::Song);
        assert_eq!(watch[0].duration_ms, Some(245_000));
        assert_eq!(playlist[0].kind, MediaKind::PodcastEpisode);
        assert_eq!(playlist[0].duration_ms, Some(2_530_000));
        Ok(())
    }
}
