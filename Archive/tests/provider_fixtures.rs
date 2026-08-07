use std::{
    error::Error,
    io,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use ytermusic::{
    domain::{MediaId, MediaItem, MediaKind, RegionCode},
    provider::{
        AuthenticationState, BrowseItem, ChartCacheKey, ChartSection, LibraryItem, LibrarySection,
        MusicProvider, Page, ParseError, ParseWarningKind, Podcast, ProviderResult, SearchFilter,
        SearchItem, parse_chart_response, parse_search_response,
    },
};

const CHARTS_FIXTURE: &[u8] = include_bytes!("fixtures/charts_hk.json");
const PODCAST_SEARCH_FIXTURE: &[u8] = include_bytes!("fixtures/podcast_search.json");

fn missing_fixture_value(message: &'static str) -> io::Error {
    io::Error::other(message)
}

#[test]
fn chart_fixture_normalizes_valid_siblings_and_reports_malformed_items()
-> Result<(), Box<dyn Error>> {
    let report = parse_chart_response(CHARTS_FIXTURE)?;
    assert_eq!(report.value.len(), 1);

    let section = report
        .value
        .first()
        .ok_or_else(|| missing_fixture_value("chart section should exist"))?;
    assert_eq!(section.title, "Top songs");
    assert_eq!(section.items.len(), 2);

    let first = section
        .items
        .first()
        .ok_or_else(|| missing_fixture_value("first chart song should exist"))?;
    assert_eq!(first.id.provider, "youtube-music");
    assert_eq!(first.id.video_id, "hk_fixture_01");
    assert_eq!(first.kind, MediaKind::Song);
    assert_eq!(first.title, "Neon Harbour");
    assert_eq!(first.creators, ["Harbour Unit", "Mira Vale"]);
    assert_eq!(first.collection.as_deref(), Some("Midnight Ferries"));
    assert_eq!(first.duration_ms, Some(201_000));
    assert!(first.explicit);

    let second = section
        .items
        .get(1)
        .ok_or_else(|| missing_fixture_value("second chart song should exist"))?;
    assert_eq!(second.id.video_id, "hk_fixture_02");
    assert_eq!(second.kind, MediaKind::Song);
    assert_eq!(second.duration_ms, None);

    assert_eq!(report.warnings.len(), 1);
    assert_eq!(report.warnings[0].kind, ParseWarningKind::MissingVideoId);
    assert_eq!(report.warnings[0].section_index, 0);
    assert_eq!(report.warnings[0].item_index, 2);
    Ok(())
}

#[test]
fn region_is_an_explicit_part_of_the_normalized_chart_cache_key() -> Result<(), Box<dyn Error>> {
    let hk = ChartCacheKey::new(RegionCode::parse("hk")?);
    let us = ChartCacheKey::new(RegionCode::parse("US")?);

    assert_eq!(hk.region().as_str(), "HK");
    assert_eq!(hk.to_string(), "charts:HK");
    assert_eq!(us.to_string(), "charts:US");
    assert_ne!(hk, us);
    Ok(())
}

#[test]
fn podcast_fixture_preserves_episode_and_song_media_kinds() -> Result<(), Box<dyn Error>> {
    let report = parse_search_response(PODCAST_SEARCH_FIXTURE)?;
    assert!(report.warnings.is_empty());
    assert_eq!(report.value.items.len(), 2);
    assert_eq!(report.value.continuation, None);
    assert!(!report.value.stale);

    let Some(SearchItem::Playable(episode)) = report.value.items.first() else {
        return Err(missing_fixture_value("first result should be playable").into());
    };
    assert_eq!(episode.id.video_id, "pod_fixture_ep_07");
    assert_eq!(episode.kind, MediaKind::PodcastEpisode);
    assert_eq!(episode.creators, ["Signal Stories"]);
    assert_eq!(episode.duration_ms, None);

    let Some(SearchItem::Playable(song)) = report.value.items.get(1) else {
        return Err(missing_fixture_value("second result should be playable").into());
    };
    assert_eq!(song.id.video_id, "pod_fixture_song_01");
    assert_eq!(song.kind, MediaKind::Song);
    assert_eq!(song.duration_ms, Some(245_000));
    Ok(())
}

#[test]
fn parsers_reject_unrelated_shapes_and_never_render_raw_payloads() {
    assert!(matches!(
        parse_chart_response(br#"{"contents":{"unrelatedRenderer":{"items":[]}}}"#),
        Err(ParseError::UnsupportedShape { response: "charts" })
    ));

    let secret = "SECRET_COOKIE_SHOULD_NOT_APPEAR";
    let invalid = format!(r#"{{"cookie":"{secret}","contents":"#);
    let Err(error) = parse_search_response(invalid.as_bytes()) else {
        panic!("invalid JSON must fail");
    };
    let rendered = format!("{error:?} {error}");
    assert!(!rendered.contains(secret));
    assert!(matches!(
        error,
        ParseError::InvalidJson { response: "search" }
    ));
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Call {
    Search(String, SearchFilter),
    Charts(RegionCode),
    Playlist(String),
    Podcast(String),
    Radio(MediaId),
    Library(LibrarySection),
    Authentication,
}

#[derive(Clone, Default)]
struct FakeProvider {
    calls: Arc<Mutex<Vec<Call>>>,
}

impl FakeProvider {
    fn record(&self, call: Call) {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(call);
    }

    fn calls(&self) -> Vec<Call> {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

#[async_trait]
impl MusicProvider for FakeProvider {
    async fn search(&self, query: &str, filter: SearchFilter) -> ProviderResult<Page<SearchItem>> {
        self.record(Call::Search(query.to_owned(), filter));
        Ok(Page {
            items: Vec::new(),
            continuation: Some("fixture-continuation".to_owned()),
            stale: false,
        })
    }

    async fn charts(&self, region: &RegionCode) -> ProviderResult<Vec<ChartSection>> {
        self.record(Call::Charts(region.clone()));
        Ok(Vec::new())
    }

    async fn playlist(&self, id: &str) -> ProviderResult<Vec<MediaItem>> {
        self.record(Call::Playlist(id.to_owned()));
        Ok(Vec::new())
    }

    async fn podcast(&self, id: &str) -> ProviderResult<Podcast> {
        self.record(Call::Podcast(id.to_owned()));
        Ok(Podcast {
            id: id.to_owned(),
            title: "Fixture podcast".to_owned(),
            creators: Vec::new(),
            description: None,
            artwork_url: None,
            episodes: Vec::new(),
        })
    }

    async fn radio(&self, seed: &MediaId) -> ProviderResult<Vec<MediaItem>> {
        self.record(Call::Radio(seed.clone()));
        Ok(Vec::new())
    }

    async fn library(&self, section: LibrarySection) -> ProviderResult<Page<LibraryItem>> {
        self.record(Call::Library(section));
        Ok(Page {
            items: vec![LibraryItem::Playlist(BrowseItem {
                id: "VL_FIXTURE".to_owned(),
                title: "Fixture playlist".to_owned(),
                subtitle: None,
                artwork_url: None,
            })],
            continuation: None,
            stale: true,
        })
    }

    fn authentication(&self) -> AuthenticationState {
        self.record(Call::Authentication);
        AuthenticationState::Unauthenticated
    }
}

#[tokio::test]
async fn music_provider_is_object_safe_and_forwards_normalized_inputs() -> Result<(), Box<dyn Error>>
{
    let fake = FakeProvider::default();
    let provider: Box<dyn MusicProvider> = Box::new(fake.clone());
    let region = RegionCode::parse("hk")?;
    let seed = MediaId {
        provider: "youtube-music".to_owned(),
        video_id: "seed_fixture".to_owned(),
    };

    let search = provider
        .search("night train", SearchFilter::Episodes)
        .await?;
    assert_eq!(search.continuation.as_deref(), Some("fixture-continuation"));
    let _ = provider.charts(&region).await?;
    let _ = provider.playlist("VL_FIXTURE").await?;
    let podcast = provider.podcast("MPSP_FIXTURE").await?;
    assert_eq!(podcast.id, "MPSP_FIXTURE");
    let _ = provider.radio(&seed).await?;
    let library = provider.library(LibrarySection::Podcasts).await?;
    assert!(library.stale);
    assert!(matches!(
        provider.authentication(),
        AuthenticationState::Unauthenticated
    ));

    assert_eq!(
        fake.calls(),
        vec![
            Call::Search("night train".to_owned(), SearchFilter::Episodes),
            Call::Charts(region),
            Call::Playlist("VL_FIXTURE".to_owned()),
            Call::Podcast("MPSP_FIXTURE".to_owned()),
            Call::Radio(seed),
            Call::Library(LibrarySection::Podcasts),
            Call::Authentication,
        ]
    );
    Ok(())
}
