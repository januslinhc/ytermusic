use std::{
    borrow::Cow,
    error::Error,
    future::{self, Future},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    task::Poll,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use serde_json::json;
use ytermusic::{
    domain::{MediaId, MediaItem, MediaKind, RegionCode, SearchFilter},
    lyrics::MAX_LYRICS_TEXT_BYTES,
    provider::{
        AuthenticationState, BrowseItem, ChartSection, ChartsQuery, LibraryItem, LibrarySection,
        MusicProvider, Page, Podcast, ProviderError, ProviderErrorKind, ProviderOperation,
        ProviderResult, SearchItem, YtMusicApi, YtMusicProvider,
    },
};
use ytmapi_rs::query::PostQuery;

#[derive(Clone, Debug, Eq, PartialEq)]
enum Call {
    Search(SearchFilter, String),
    Charts(RegionCode),
    Playlist(String),
    Podcast(String),
    WatchPlaylist(String),
    LyricsId(String),
    Lyrics(String),
    Library(LibrarySection),
}

#[derive(Clone, Default)]
struct FakeApi {
    calls: Arc<Mutex<Vec<Call>>>,
    search_failures: Arc<Mutex<Vec<SearchFilter>>>,
    pending_searches: Arc<Mutex<Vec<SearchFilter>>>,
    search_time_advances: Arc<Mutex<Vec<(SearchFilter, Duration)>>>,
    duplicate_search_results: Arc<AtomicBool>,
    searches_in_flight: Arc<AtomicUsize>,
    max_searches_in_flight: Arc<AtomicUsize>,
    lyrics_id_available: Arc<AtomicBool>,
    lyrics_text_available: Arc<AtomicBool>,
    lyrics_text: Arc<Mutex<String>>,
}

struct SearchInFlight(Arc<AtomicUsize>);

impl Drop for SearchInFlight {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl FakeApi {
    fn with_lyrics(text: &str) -> Self {
        let fake = Self::default();
        fake.lyrics_id_available.store(true, Ordering::SeqCst);
        fake.lyrics_text_available.store(true, Ordering::SeqCst);
        text.clone_into(
            &mut fake
                .lyrics_text
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        fake
    }
    fn with_search_failures(failures: impl IntoIterator<Item = SearchFilter>) -> Self {
        Self {
            search_failures: Arc::new(Mutex::new(failures.into_iter().collect())),
            ..Self::default()
        }
    }

    fn with_pending_search(filter: SearchFilter) -> Self {
        Self::default().and_pending_search(filter)
    }

    fn and_pending_search(self, filter: SearchFilter) -> Self {
        {
            self.pending_searches
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(filter);
        }
        self
    }

    fn and_advance_time_during_search(self, filter: SearchFilter, duration: Duration) -> Self {
        {
            self.search_time_advances
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((filter, duration));
        }
        self
    }

    fn with_duplicate_search_results() -> Self {
        let fake = Self::default();
        fake.duplicate_search_results.store(true, Ordering::SeqCst);
        fake
    }

    fn calls(&self) -> Vec<Call> {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn record(&self, call: Call) {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(call);
    }

    fn max_searches_in_flight(&self) -> usize {
        self.max_searches_in_flight.load(Ordering::SeqCst)
    }

    fn searches_in_flight(&self) -> usize {
        self.searches_in_flight.load(Ordering::SeqCst)
    }

    async fn typed_search(
        &self,
        query: &str,
        filter: SearchFilter,
    ) -> ProviderResult<Page<SearchItem>> {
        self.record(Call::Search(filter, query.to_owned()));
        let active = self.searches_in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_searches_in_flight
            .fetch_max(active, Ordering::SeqCst);
        let _in_flight = SearchInFlight(Arc::clone(&self.searches_in_flight));
        tokio::task::yield_now().await;
        let advance_by = self
            .search_time_advances
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .find_map(|(candidate, duration)| (*candidate == filter).then_some(*duration));
        if let Some(duration) = advance_by {
            let mut advance = Box::pin(tokio::time::advance(duration));
            future::poll_fn(|context| match advance.as_mut().poll(context) {
                Poll::Ready(()) | Poll::Pending => Poll::Ready(()),
            })
            .await;
        }
        let pending = self
            .pending_searches
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(&filter);
        if pending {
            future::pending::<()>().await;
        }

        if self
            .search_failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(&filter)
        {
            let kind = if filter == SearchFilter::Songs {
                ProviderErrorKind::NotFound
            } else {
                ProviderErrorKind::Unavailable
            };
            return Err(ProviderError::new(ProviderOperation::Search, kind));
        }
        if self.duplicate_search_results.load(Ordering::SeqCst) {
            return Ok(Page {
                items: vec![SearchItem::Playable(media("duplicate", MediaKind::Song))],
                continuation: None,
                stale: false,
            });
        }
        Ok(Self::search_page(filter))
    }

    fn search_page(filter: SearchFilter) -> Page<SearchItem> {
        let item = match filter {
            SearchFilter::Songs | SearchFilter::Episodes => SearchItem::Playable(media(
                &format!("{filter:?}"),
                if filter == SearchFilter::Episodes {
                    MediaKind::PodcastEpisode
                } else {
                    MediaKind::Song
                },
            )),
            SearchFilter::Albums => SearchItem::Album(browse("album")),
            SearchFilter::Artists => SearchItem::Artist(browse("artist")),
            SearchFilter::Playlists => SearchItem::Playlist(browse("playlist")),
            SearchFilter::Podcasts => SearchItem::Podcast(browse("podcast")),
            SearchFilter::All => SearchItem::Playable(media("all", MediaKind::Song)),
        };
        Page {
            items: vec![item],
            continuation: Some(format!("{filter:?}-continuation")),
            stale: false,
        }
    }

    fn library_page(section: LibrarySection) -> Page<LibraryItem> {
        let item = match section {
            LibrarySection::Songs => {
                LibraryItem::Playable(media(&format!("{section:?}"), MediaKind::Song))
            }
            LibrarySection::Albums => LibraryItem::Album(browse("library-album")),
            LibrarySection::Artists => LibraryItem::Artist(browse("library-artist")),
            LibrarySection::Playlists => LibraryItem::Playlist(browse("library-playlist")),
            LibrarySection::Podcasts => LibraryItem::Podcast(browse("library-podcast")),
            LibrarySection::Episodes => panic!("fake API has no saved-library episodes route"),
        };
        Page {
            items: vec![item],
            continuation: Some(format!("{section:?}-continuation")),
            stale: section == LibrarySection::Podcasts,
        }
    }
}

#[async_trait]
impl YtMusicApi for FakeApi {
    async fn lyrics_id(&self, video_id: &str) -> ProviderResult<Option<String>> {
        self.record(Call::LyricsId(video_id.to_owned()));
        Ok(self
            .lyrics_id_available
            .load(Ordering::SeqCst)
            .then(|| "lyrics-browse-id".to_owned()))
    }

    async fn lyrics(&self, lyrics_id: &str) -> ProviderResult<Option<String>> {
        self.record(Call::Lyrics(lyrics_id.to_owned()));
        Ok(self.lyrics_text_available.load(Ordering::SeqCst).then(|| {
            self.lyrics_text
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }))
    }

    async fn search_songs(&self, query: &str) -> ProviderResult<Page<SearchItem>> {
        self.typed_search(query, SearchFilter::Songs).await
    }

    async fn search_albums(&self, query: &str) -> ProviderResult<Page<SearchItem>> {
        self.typed_search(query, SearchFilter::Albums).await
    }

    async fn search_artists(&self, query: &str) -> ProviderResult<Page<SearchItem>> {
        self.typed_search(query, SearchFilter::Artists).await
    }

    async fn search_playlists(&self, query: &str) -> ProviderResult<Page<SearchItem>> {
        self.typed_search(query, SearchFilter::Playlists).await
    }

    async fn search_podcasts(&self, query: &str) -> ProviderResult<Page<SearchItem>> {
        self.typed_search(query, SearchFilter::Podcasts).await
    }

    async fn search_episodes(&self, query: &str) -> ProviderResult<Page<SearchItem>> {
        self.typed_search(query, SearchFilter::Episodes).await
    }

    async fn charts(&self, query: ChartsQuery) -> ProviderResult<Vec<ChartSection>> {
        self.record(Call::Charts(query.region().clone()));
        Ok(vec![ChartSection::new(
            format!("{} charts", query.region()),
            Vec::new(),
        )])
    }

    async fn playlist(&self, id: &str) -> ProviderResult<Vec<MediaItem>> {
        self.record(Call::Playlist(id.to_owned()));
        Ok(vec![media("playlist-track", MediaKind::Song)])
    }

    async fn podcast(&self, id: &str) -> ProviderResult<Podcast> {
        self.record(Call::Podcast(id.to_owned()));
        Ok(Podcast {
            id: id.to_owned(),
            title: "Mapped podcast".to_owned(),
            creators: vec!["Mapped creator".to_owned()],
            description: Some("Mapped description".to_owned()),
            artwork_url: None,
            episodes: vec![media("mapped-episode", MediaKind::PodcastEpisode)],
        })
    }

    async fn watch_playlist(&self, video_id: &str) -> ProviderResult<Vec<MediaItem>> {
        self.record(Call::WatchPlaylist(video_id.to_owned()));
        Ok(vec![
            media("before", MediaKind::Song),
            media(video_id, MediaKind::Song),
            media("after", MediaKind::Song),
            media(video_id, MediaKind::Song),
            MediaItem {
                id: MediaId {
                    provider: "other-provider".to_owned(),
                    video_id: video_id.to_owned(),
                },
                ..media("other", MediaKind::Song)
            },
        ])
    }

    async fn library_songs(&self) -> ProviderResult<Page<LibraryItem>> {
        self.record(Call::Library(LibrarySection::Songs));
        Ok(Self::library_page(LibrarySection::Songs))
    }

    async fn library_albums(&self) -> ProviderResult<Page<LibraryItem>> {
        self.record(Call::Library(LibrarySection::Albums));
        Ok(Self::library_page(LibrarySection::Albums))
    }

    async fn library_artists(&self) -> ProviderResult<Page<LibraryItem>> {
        self.record(Call::Library(LibrarySection::Artists));
        Ok(Self::library_page(LibrarySection::Artists))
    }

    async fn library_playlists(&self) -> ProviderResult<Page<LibraryItem>> {
        self.record(Call::Library(LibrarySection::Playlists));
        Ok(Self::library_page(LibrarySection::Playlists))
    }

    async fn library_podcasts(&self) -> ProviderResult<Page<LibraryItem>> {
        self.record(Call::Library(LibrarySection::Podcasts));
        Ok(Self::library_page(LibrarySection::Podcasts))
    }
}

fn browse(id: &str) -> BrowseItem {
    BrowseItem {
        id: id.to_owned(),
        title: format!("{id} title"),
        subtitle: Some(format!("{id} subtitle")),
        artwork_url: None,
    }
}

fn media(video_id: &str, kind: MediaKind) -> MediaItem {
    MediaItem {
        id: MediaId {
            provider: "youtube-music".to_owned(),
            video_id: video_id.to_owned(),
        },
        kind,
        title: format!("{video_id} title"),
        creators: vec!["Fixture creator".to_owned()],
        collection: None,
        duration_ms: Some(42_000),
        artwork_url: None,
        explicit: false,
    }
}

#[tokio::test]
async fn every_search_filter_dispatches_to_its_typed_api_method() -> Result<(), Box<dyn Error>> {
    let fake = FakeApi::default();
    let provider = YtMusicProvider::with_api(fake.clone(), AuthenticationState::Unauthenticated);
    let filters = [
        SearchFilter::Songs,
        SearchFilter::Albums,
        SearchFilter::Artists,
        SearchFilter::Playlists,
        SearchFilter::Podcasts,
        SearchFilter::Episodes,
    ];

    for filter in filters {
        let page = provider.search("fixture query", filter).await?;
        assert_eq!(
            page.continuation.as_deref(),
            Some(format!("{filter:?}-continuation").as_str())
        );
        match (filter, page.items.first()) {
            (SearchFilter::Podcasts, Some(SearchItem::Podcast(_)))
            | (
                SearchFilter::Episodes,
                Some(SearchItem::Playable(MediaItem {
                    kind: MediaKind::PodcastEpisode,
                    ..
                })),
            )
            | (SearchFilter::Songs, Some(SearchItem::Playable(_)))
            | (SearchFilter::Albums, Some(SearchItem::Album(_)))
            | (SearchFilter::Artists, Some(SearchItem::Artist(_)))
            | (SearchFilter::Playlists, Some(SearchItem::Playlist(_))) => {}
            other => panic!("unexpected normalized search result: {other:?}"),
        }
    }

    assert_eq!(
        fake.calls(),
        filters
            .into_iter()
            .map(|filter| Call::Search(filter, "fixture query".to_owned()))
            .collect::<Vec<_>>()
    );
    Ok(())
}

fn search_item_id(item: &SearchItem) -> &str {
    match item {
        SearchItem::Playable(item) => &item.id.video_id,
        SearchItem::Album(item)
        | SearchItem::Artist(item)
        | SearchItem::Playlist(item)
        | SearchItem::Podcast(item) => &item.id,
    }
}

#[tokio::test]
async fn all_search_is_sequential_best_effort_in_fixed_category_order() -> Result<(), Box<dyn Error>>
{
    for (failures, expected_ids) in [
        (
            vec![SearchFilter::Albums],
            vec!["Songs", "artist", "playlist", "podcast", "Episodes"],
        ),
        (
            vec![SearchFilter::Albums, SearchFilter::Playlists],
            vec!["Songs", "artist", "podcast", "Episodes"],
        ),
    ] {
        let fake = FakeApi::with_search_failures(failures);
        let provider =
            YtMusicProvider::with_api(fake.clone(), AuthenticationState::Unauthenticated);

        let page = provider.search("fixture query", SearchFilter::All).await?;

        assert_eq!(
            page.items.iter().map(search_item_id).collect::<Vec<_>>(),
            expected_ids
        );
        assert_eq!(
            fake.calls(),
            [
                SearchFilter::Songs,
                SearchFilter::Albums,
                SearchFilter::Artists,
                SearchFilter::Playlists,
                SearchFilter::Podcasts,
                SearchFilter::Episodes,
            ]
            .into_iter()
            .map(|filter| Call::Search(filter, "fixture query".to_owned()))
            .collect::<Vec<_>>()
        );
        assert_eq!(fake.max_searches_in_flight(), 1);
    }
    Ok(())
}

#[tokio::test]
async fn all_search_keeps_cross_category_duplicates_in_category_order() -> Result<(), Box<dyn Error>>
{
    let fake = FakeApi::with_duplicate_search_results();
    let provider = YtMusicProvider::with_api(fake.clone(), AuthenticationState::Unauthenticated);

    let page = provider.search("fixture query", SearchFilter::All).await?;

    assert_eq!(page.items.len(), 6);
    assert!(
        page.items
            .iter()
            .all(|item| search_item_id(item) == "duplicate")
    );
    assert_eq!(fake.max_searches_in_flight(), 1);
    Ok(())
}

#[tokio::test]
async fn all_search_returns_the_first_typed_error_when_every_category_fails() {
    let filters = [
        SearchFilter::Songs,
        SearchFilter::Albums,
        SearchFilter::Artists,
        SearchFilter::Playlists,
        SearchFilter::Podcasts,
        SearchFilter::Episodes,
    ];
    let fake = FakeApi::with_search_failures(filters);
    let provider = YtMusicProvider::with_api(fake.clone(), AuthenticationState::Unauthenticated);

    let Err(error) = provider.search("fixture query", SearchFilter::All).await else {
        panic!("all failed categories must return a typed error");
    };

    assert_eq!(
        error,
        ProviderError::new(ProviderOperation::Search, ProviderErrorKind::NotFound)
    );
    assert_eq!(
        fake.calls(),
        filters
            .into_iter()
            .map(|filter| Call::Search(filter, "fixture query".to_owned()))
            .collect::<Vec<_>>()
    );
    assert_eq!(fake.max_searches_in_flight(), 1);
}

#[tokio::test(start_paused = true)]
async fn all_search_returns_early_success_when_a_later_category_reaches_the_deadline()
-> Result<(), Box<dyn Error>> {
    let fake = FakeApi::with_pending_search(SearchFilter::Albums);
    let provider = YtMusicProvider::with_api(fake.clone(), AuthenticationState::Unauthenticated);
    let wall_start = Instant::now();

    let page = tokio::time::timeout(
        Duration::from_secs(31),
        provider.search("fixture query", SearchFilter::All),
    )
    .await
    .map_err(|_| "provider deadline did not fire before the test guard")??;

    assert_eq!(
        page.items.iter().map(search_item_id).collect::<Vec<_>>(),
        ["Songs"]
    );
    assert!(wall_start.elapsed() < Duration::from_secs(1));
    assert_eq!(fake.searches_in_flight(), 0);
    assert_eq!(
        fake.calls(),
        [
            Call::Search(SearchFilter::Songs, "fixture query".to_owned()),
            Call::Search(SearchFilter::Albums, "fixture query".to_owned()),
        ]
    );
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn all_search_does_not_start_another_category_after_the_deadline_has_elapsed()
-> Result<(), Box<dyn Error>> {
    let fake = FakeApi::default()
        .and_advance_time_during_search(SearchFilter::Songs, Duration::from_secs(31));
    let provider = YtMusicProvider::with_api(fake.clone(), AuthenticationState::Unauthenticated);
    let wall_start = Instant::now();

    let page = provider.search("fixture query", SearchFilter::All).await?;

    assert_eq!(
        page.items.iter().map(search_item_id).collect::<Vec<_>>(),
        ["Songs"]
    );
    assert!(wall_start.elapsed() < Duration::from_secs(1));
    assert_eq!(fake.searches_in_flight(), 0);
    assert_eq!(
        fake.calls(),
        [Call::Search(
            SearchFilter::Songs,
            "fixture query".to_owned()
        )]
    );
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn all_search_deadline_without_success_returns_an_error_and_stops_dispatch() {
    for (fake, expected_kind, expected_filters) in [
        (
            FakeApi::with_pending_search(SearchFilter::Songs),
            ProviderErrorKind::Unavailable,
            vec![SearchFilter::Songs],
        ),
        (
            FakeApi::with_search_failures([SearchFilter::Songs])
                .and_pending_search(SearchFilter::Albums),
            ProviderErrorKind::NotFound,
            vec![SearchFilter::Songs, SearchFilter::Albums],
        ),
    ] {
        let provider =
            YtMusicProvider::with_api(fake.clone(), AuthenticationState::Unauthenticated);

        let guarded = tokio::time::timeout(
            Duration::from_secs(31),
            provider.search("fixture query", SearchFilter::All),
        )
        .await;
        let Ok(result) = guarded else {
            panic!("provider deadline did not fire before the test guard");
        };
        let Err(error) = result else {
            panic!("deadline without a successful category must return an error");
        };

        assert_eq!(
            error,
            ProviderError::new(ProviderOperation::Search, expected_kind)
        );
        assert_eq!(fake.searches_in_flight(), 0);
        assert_eq!(
            fake.calls(),
            expected_filters
                .into_iter()
                .map(|filter| Call::Search(filter, "fixture query".to_owned()))
                .collect::<Vec<_>>()
        );
    }
}

#[tokio::test(start_paused = true)]
async fn pending_provider_operation_hits_a_bounded_deadline_without_wall_sleep() {
    let fake = FakeApi::with_pending_search(SearchFilter::Songs);
    let provider = YtMusicProvider::with_api(fake.clone(), AuthenticationState::Unauthenticated);
    let wall_start = Instant::now();

    let guarded = tokio::time::timeout(
        Duration::from_secs(31),
        provider.search("fixture query", SearchFilter::Songs),
    )
    .await;
    let Ok(result) = guarded else {
        panic!("provider deadline did not fire before the test guard");
    };
    let Err(error) = result else {
        panic!("pending provider operation must time out");
    };

    assert_eq!(
        error,
        ProviderError::new(ProviderOperation::Search, ProviderErrorKind::Unavailable)
    );
    assert!(wall_start.elapsed() < Duration::from_secs(1));
    assert_eq!(
        fake.calls(),
        [Call::Search(
            SearchFilter::Songs,
            "fixture query".to_owned()
        )]
    );
}

#[test]
fn charts_query_has_exact_region_scoped_innertube_shape() -> Result<(), Box<dyn Error>> {
    let query = ChartsQuery::new(RegionCode::parse("hk")?);

    assert_eq!(
        query.header(),
        json!({
            "browseId": "FEmusic_charts",
            "formData": {"selectedValues": ["HK"]}
        })
        .as_object()
        .cloned()
        .ok_or("chart header fixture must be an object")?
    );
    assert_eq!(query.path(), "browse");
    assert_eq!(query.params(), Vec::<(&str, Cow<'_, str>)>::new());
    Ok(())
}

#[tokio::test]
async fn charts_and_podcast_detail_preserve_normalized_results() -> Result<(), Box<dyn Error>> {
    let fake = FakeApi::default();
    let provider = YtMusicProvider::with_api(fake.clone(), AuthenticationState::Unauthenticated);
    let region = RegionCode::parse("us")?;

    let charts = provider.charts(&region).await?;
    assert_eq!(charts.first().map(ChartSection::title), Some("US charts"));
    let podcast = provider.podcast("MPSP_FIXTURE").await?;
    assert_eq!(podcast.id, "MPSP_FIXTURE");
    assert_eq!(podcast.title, "Mapped podcast");
    assert_eq!(podcast.creators, ["Mapped creator"]);
    assert_eq!(podcast.episodes[0].kind, MediaKind::PodcastEpisode);
    assert_eq!(
        fake.calls(),
        [
            Call::Charts(region),
            Call::Podcast("MPSP_FIXTURE".to_owned())
        ]
    );
    Ok(())
}

#[tokio::test]
async fn radio_removes_every_exact_seed_and_preserves_other_duplicates_in_response_order()
-> Result<(), Box<dyn Error>> {
    let fake = FakeApi::default();
    let provider = YtMusicProvider::with_api(fake.clone(), AuthenticationState::Unauthenticated);
    let seed = MediaId {
        provider: "youtube-music".to_owned(),
        video_id: "seed".to_owned(),
    };

    let radio = provider.radio(&seed).await?;
    assert_eq!(
        radio
            .iter()
            .map(|item| (item.id.provider.as_str(), item.id.video_id.as_str()))
            .collect::<Vec<_>>(),
        [
            ("youtube-music", "before"),
            ("youtube-music", "after"),
            ("other-provider", "seed"),
        ]
    );
    assert_eq!(fake.calls(), [Call::WatchPlaylist(seed.video_id.clone())]);
    Ok(())
}

#[tokio::test]
async fn anonymous_library_fails_before_capability_checks_or_api_calls() {
    let fake = FakeApi::default();
    let provider = YtMusicProvider::with_api(fake.clone(), AuthenticationState::Unauthenticated);

    let Err(songs_error) = provider.library(LibrarySection::Songs).await else {
        panic!("anonymous library must require authentication");
    };
    let Err(episodes_error) = provider.library(LibrarySection::Episodes).await else {
        panic!("anonymous library episodes must require authentication");
    };
    assert_eq!(
        songs_error,
        ProviderError::new(
            ProviderOperation::Library,
            ProviderErrorKind::AuthenticationRequired
        )
    );
    assert_eq!(
        episodes_error,
        ProviderError::new(
            ProviderOperation::Library,
            ProviderErrorKind::AuthenticationRequired
        )
    );
    assert!(fake.calls().is_empty());
}

#[tokio::test]
async fn authenticated_library_dispatches_supported_sections_and_rejects_episodes_without_a_call()
-> Result<(), Box<dyn Error>> {
    let fake = FakeApi::default();
    let provider = YtMusicProvider::with_api(fake.clone(), AuthenticationState::Authenticated);
    let sections = [
        LibrarySection::Songs,
        LibrarySection::Albums,
        LibrarySection::Artists,
        LibrarySection::Playlists,
        LibrarySection::Podcasts,
    ];

    for section in sections {
        let page = provider.library(section).await?;
        assert_eq!(
            page.continuation.as_deref(),
            Some(format!("{section:?}-continuation").as_str())
        );
        assert_eq!(page.stale, section == LibrarySection::Podcasts);
    }
    assert_eq!(
        fake.calls(),
        sections.into_iter().map(Call::Library).collect::<Vec<_>>()
    );
    let calls_before_unsupported = fake.calls();
    let Err(error) = provider.library(LibrarySection::Episodes).await else {
        panic!("saved library episodes must be reported as unsupported");
    };
    assert_eq!(
        error,
        ProviderError::new(ProviderOperation::Library, ProviderErrorKind::Unsupported)
    );
    assert_eq!(fake.calls(), calls_before_unsupported);
    Ok(())
}

#[tokio::test]
async fn invalid_opaque_ids_are_rejected_before_api_dispatch() {
    let fake = FakeApi::default();
    let provider = YtMusicProvider::with_api(fake.clone(), AuthenticationState::Unauthenticated);

    let Err(playlist_error) = provider.playlist("contains whitespace").await else {
        panic!("invalid playlist id must fail");
    };
    let Err(podcast_error) = provider.podcast("").await else {
        panic!("empty podcast id must fail");
    };
    let Err(radio_error) = provider
        .radio(&MediaId {
            provider: "another-provider".to_owned(),
            video_id: "otherwise-valid".to_owned(),
        })
        .await
    else {
        panic!("foreign provider seed must fail");
    };
    let Err(long_radio_error) = provider
        .radio(&MediaId {
            provider: "youtube-music".to_owned(),
            video_id: "x".repeat(129),
        })
        .await
    else {
        panic!("overlong media id must fail");
    };

    assert_eq!(playlist_error.kind, ProviderErrorKind::InvalidResponse);
    assert_eq!(playlist_error.operation, ProviderOperation::Playlist);
    assert_eq!(podcast_error.kind, ProviderErrorKind::InvalidResponse);
    assert_eq!(podcast_error.operation, ProviderOperation::Podcast);
    assert_eq!(radio_error.kind, ProviderErrorKind::InvalidResponse);
    assert_eq!(radio_error.operation, ProviderOperation::Radio);
    assert_eq!(long_radio_error.kind, ProviderErrorKind::InvalidResponse);
    assert_eq!(long_radio_error.operation, ProviderOperation::Radio);
    assert!(fake.calls().is_empty());
}

#[tokio::test]
async fn lyrics_resolves_browse_id_then_returns_bounded_plain_text() -> Result<(), Box<dyn Error>> {
    let fake = FakeApi::with_lyrics("line one\r\nline\ttwo\x1b\x07");
    let provider = YtMusicProvider::with_api(fake.clone(), AuthenticationState::Unauthenticated);
    let id = MediaId {
        provider: "youtube-music".to_owned(),
        video_id: "valid-video-id".to_owned(),
    };

    let lyrics = provider.lyrics(&id).await?;

    assert_eq!(lyrics.text(), "line one\nline two");
    assert_eq!(
        fake.calls(),
        [
            Call::LyricsId("valid-video-id".to_owned()),
            Call::Lyrics("lyrics-browse-id".to_owned()),
        ]
    );
    Ok(())
}

#[tokio::test]
async fn lyrics_unavailable_is_typed_and_invalid_ids_never_dispatch() {
    let fake = FakeApi::default();
    let provider = YtMusicProvider::with_api(fake.clone(), AuthenticationState::Unauthenticated);
    let unavailable = MediaId {
        provider: "youtube-music".to_owned(),
        video_id: "no-lyrics".to_owned(),
    };

    let Err(unavailable_error) = provider.lyrics(&unavailable).await else {
        panic!("missing lyrics must be typed");
    };
    assert_eq!(unavailable_error.operation, ProviderOperation::Lyrics);
    assert_eq!(unavailable_error.kind, ProviderErrorKind::NotFound);
    assert_eq!(fake.calls(), [Call::LyricsId("no-lyrics".to_owned())]);

    fake.lyrics_id_available.store(true, Ordering::SeqCst);
    let Err(unavailable_body_error) = provider.lyrics(&unavailable).await else {
        panic!("an unavailable lyrics body must be typed");
    };
    assert_eq!(unavailable_body_error.operation, ProviderOperation::Lyrics);
    assert_eq!(unavailable_body_error.kind, ProviderErrorKind::NotFound);
    assert_eq!(
        fake.calls(),
        [
            Call::LyricsId("no-lyrics".to_owned()),
            Call::LyricsId("no-lyrics".to_owned()),
            Call::Lyrics("lyrics-browse-id".to_owned()),
        ]
    );

    let calls_before_invalid = fake.calls();
    for invalid in [
        MediaId {
            provider: "other".to_owned(),
            video_id: "valid".to_owned(),
        },
        MediaId {
            provider: "youtube-music".to_owned(),
            video_id: "has whitespace".to_owned(),
        },
    ] {
        let Err(error) = provider.lyrics(&invalid).await else {
            panic!("invalid id must fail");
        };
        assert_eq!(error.operation, ProviderOperation::Lyrics);
        assert_eq!(error.kind, ProviderErrorKind::InvalidResponse);
    }
    assert_eq!(fake.calls(), calls_before_invalid);
}

#[tokio::test]
async fn lyrics_text_is_bounded_before_returning_from_provider() {
    let fake = FakeApi::with_lyrics(&"x".repeat(MAX_LYRICS_TEXT_BYTES + 1));
    let provider = YtMusicProvider::with_api(fake, AuthenticationState::Unauthenticated);
    let id = MediaId {
        provider: "youtube-music".to_owned(),
        video_id: "valid-video-id".to_owned(),
    };

    let Err(error) = provider.lyrics(&id).await else {
        panic!("oversized lyrics must fail");
    };
    assert_eq!(error.operation, ProviderOperation::Lyrics);
    assert_eq!(error.kind, ProviderErrorKind::InvalidResponse);
}

#[test]
fn adapter_and_query_debug_output_are_summary_only() -> Result<(), Box<dyn Error>> {
    let sentinel = "COOKIE_CONTINUATION_RAW_JSON_SIGNED_URL_SENTINEL";
    let fake = FakeApi::default();
    let provider = YtMusicProvider::with_api(fake, AuthenticationState::Authenticated);
    let query = ChartsQuery::new(RegionCode::parse("hk")?);

    for rendered in [format!("{provider:?}"), format!("{query:?}")] {
        assert!(!rendered.contains(sentinel));
        assert!(!rendered.contains("ytmapi"));
        assert!(!rendered.contains("cookie"));
    }
    Ok(())
}
