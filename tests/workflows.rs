use std::{collections::HashSet, error::Error};

use ratatui::{Terminal, backend::TestBackend, buffer::Buffer};
use tempfile::TempDir;
use ytermusic::{
    app::{
        Action, AppState, ChartCachePayload, ChartSection, Effect, FadeActivity, Generation,
        MAX_VIEW_ITEMS, PlayerCommand, PodcastProgressCheckpoint, PodcastProviderId,
        ResolverQuality, SearchFilter, SearchItem, SearchItemId, SearchMetadata,
        SearchMetadataKind, SearchPage, reduce, stable_library_item_id, stable_queue_item_id,
    },
    auth::Browser,
    config::Config,
    diagnostics::{DiagnosticRow, DiagnosticStatus, DoctorReport},
    domain::{MediaId, MediaItem, MediaKind, PlaybackStatus, RegionCode, RepeatMode},
    notifications::NowPlayingNotification,
    podcast_rankings::{PodcastRecommendationPage, parse_apple_top_shows},
    provider::{
        AuthenticationState, BrowseItem, LibraryItem, LibrarySection, MAX_ITEMS_PER_SHELF,
        MAX_SECTIONS, Page, Podcast,
    },
    queue::QueueItem,
    runtime::startup_actions,
    storage::{HistoryEntry, PodcastProgress, SqliteStorage, Storage},
    ui::{
        render::{NavigationItem, RenderModel, render_with_model},
        theme::Theme,
    },
};

#[test]
fn favorites_startup_is_dispatched_with_authentication_and_dependencies() {
    let dependencies = DoctorReport::new(Vec::new());
    let actions = startup_actions(AuthenticationState::Authenticated, dependencies.clone());
    assert_eq!(
        actions,
        vec![
            Action::AuthenticationChanged(AuthenticationState::Authenticated),
            Action::DependencyReportLoaded(dependencies),
            Action::FavoritesRequested,
        ]
    );
}

fn song(video_id: &str, title: &str, creator: &str) -> MediaItem {
    MediaItem {
        id: MediaId {
            provider: "youtube-music".to_owned(),
            video_id: video_id.to_owned(),
        },
        kind: MediaKind::Song,
        title: title.to_owned(),
        creators: vec![creator.to_owned()],
        collection: None,
        duration_ms: Some(180_000),
        artwork_url: None,
        explicit: false,
    }
}

fn config_without_lyrics() -> Config {
    let mut config = Config::default();
    config.lyrics.enabled = false;
    config
}

#[test]
fn explicit_list_music_selection_enters_resolution_after_atomic_commit() {
    let first = song("explicit-first", "First", "Artist");
    let selected = song("explicit-selected", "Selected", "Artist");

    let (state, effects) = reduce(
        AppState::new(config_without_lyrics()),
        Action::PlayMediaList {
            items: vec![first, selected.clone()],
            selected_id: selected.id.clone(),
            shuffle_seed: None,
        },
    );

    assert_eq!(
        state.queue().current().map(QueueItem::media),
        Some(&selected)
    );
    assert_eq!(state.playback().current.as_ref(), Some(&selected.id));
    assert_eq!(state.playback().status, PlaybackStatus::Resolving);
    assert!(effects.iter().any(|effect| matches!(
        effect,
        Effect::Resolve {
            item,
            start_ms: None,
            ..
        } if item == &selected
    )));
}

#[test]
fn explicit_list_podcast_selection_loads_progress_before_resolution() {
    let episode = MediaItem {
        kind: MediaKind::PodcastEpisode,
        duration_ms: Some(240_000),
        ..song("explicit-episode", "Episode", "Host")
    };

    let (state, effects) = reduce(
        AppState::new(config_without_lyrics()),
        Action::PlayMediaList {
            items: vec![song("explicit-song", "Song", "Artist"), episode.clone()],
            selected_id: episode.id.clone(),
            shuffle_seed: None,
        },
    );
    let Some(progress_generation) = effects.iter().find_map(|effect| match effect {
        Effect::LoadPodcastProgress {
            generation,
            media_id,
        } if media_id == &episode.id => Some(*generation),
        _ => None,
    }) else {
        panic!("explicit podcast playback must load progress first");
    };
    assert!(
        effects
            .iter()
            .all(|effect| !matches!(effect, Effect::Resolve { .. }))
    );
    assert_eq!(
        state.queue().current().map(QueueItem::media),
        Some(&episode)
    );

    let (state, effects) = reduce(
        state,
        Action::PodcastProgressLoaded {
            generation: progress_generation,
            progress: Some(PodcastProgress {
                video_id: episode.id.video_id.clone(),
                playback_epoch: 7,
                position_ms: 45_000,
                duration_ms: episode.duration_ms,
                played: false,
                updated_at: 1,
            }),
        },
    );

    assert_eq!(
        state.queue().current().map(QueueItem::media),
        Some(&episode)
    );
    assert_eq!(state.playback().current.as_ref(), Some(&episode.id));
    assert_eq!(state.playback().position_ms, 45_000);
    assert!(effects.iter().any(|effect| matches!(
        effect,
        Effect::Resolve {
            item,
            start_ms: Some(45_000),
            ..
        } if item == &episode
    )));
}

fn region(value: &str) -> RegionCode {
    RegionCode::parse(value).unwrap_or_else(|error| panic!("valid test region: {error}"))
}

fn podcast_recommendations(
    country: &str,
    rows: &[(&str, &str, &str)],
) -> PodcastRecommendationPage {
    let results = rows
        .iter()
        .map(|(id, title, publisher)| {
            serde_json::json!({"id": id, "name": title, "artistName": publisher})
        })
        .collect::<Vec<_>>();
    let bytes = serde_json::to_vec(&serde_json::json!({
        "feed": {"country": country, "results": results}
    }))
    .unwrap_or_else(|error| panic!("podcast fixture must encode: {error}"));
    parse_apple_top_shows(&bytes)
        .unwrap_or_else(|error| panic!("podcast fixture must parse: {error}"))
}

fn podcast_provider_id(value: &str) -> PodcastProviderId {
    PodcastProviderId::new(value.to_owned()).unwrap_or_else(|| panic!("valid provider id"))
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "the end-to-end reducer workflow keeps lifecycle assertions in protocol order"
)]
fn podcast_recommendation_resolve_open_refresh_failure_and_close_preserve_discovery() {
    let us = region("US");
    let page = podcast_recommendations(
        "US",
        &[
            ("daily", "The Daily", "NYT"),
            ("up-first", "Up First", "NPR"),
        ],
    );
    let selected_id = page.items()[0].source_id().clone();
    let (state, effects) = reduce(
        AppState::default(),
        Action::PodcastRecommendationsRequested { region: us.clone() },
    );
    let [Effect::LoadPodcastRecommendations { generation, .. }] = effects.as_slice() else {
        panic!("recommendation source effect");
    };
    let (state, _) = reduce(
        state,
        Action::PodcastRecommendationsCompleted {
            generation: *generation,
            requested_region: us.clone(),
            result: Ok(page),
        },
    );
    assert_eq!(
        state.podcasts().selected_recommendation(),
        Some(&selected_id)
    );

    let (state, effects) = reduce(state, Action::OpenSelectedPodcastRecommendation);
    let [
        Effect::ResolvePodcastRecommendation {
            generation: resolve_generation,
            recommendation,
        },
    ] = effects.as_slice()
    else {
        panic!("recommendation match effect");
    };
    assert_eq!(recommendation.title(), "The Daily");
    assert_eq!(recommendation.publisher(), "NYT");
    assert!(state.podcasts().resolve_loading());
    assert_eq!(state.podcasts().recommendations().len(), 2);

    let before_stale = state.clone();
    let (state, effects) = reduce(
        state,
        Action::PodcastRecommendationResolved {
            generation: Generation::new(resolve_generation.value().saturating_sub(1)),
            result: Ok(podcast_provider_id("stale-provider-id")),
        },
    );
    assert!(effects.is_empty());
    assert_eq!(state, before_stale);

    let (state, effects) = reduce(
        state,
        Action::PodcastRecommendationResolved {
            generation: *resolve_generation,
            result: Ok(podcast_provider_id("daily-provider-id")),
        },
    );
    let [Effect::LoadPodcast { generation, id }] = effects.as_slice() else {
        panic!("successful match must load the existing podcast detail path");
    };
    assert_eq!(id.as_str(), "daily-provider-id");
    assert_ne!(generation, resolve_generation);
    assert_eq!(state.podcasts().recommendations().len(), 2);

    let show = Podcast {
        id: "daily-provider-id".to_owned(),
        title: "The Daily".to_owned(),
        creators: vec!["NYT".to_owned()],
        description: None,
        artwork_url: None,
        episodes: vec![song("episode", "Episode", "NYT")],
    };
    let (state, _) = reduce(
        state,
        Action::PodcastCompleted {
            generation: *generation,
            result: Ok(show),
        },
    );
    let (state, effects) = reduce(
        state,
        Action::PodcastRecommendationsRequested { region: us.clone() },
    );
    let [Effect::LoadPodcastRecommendations { generation, .. }] = effects.as_slice() else {
        panic!("refresh source effect");
    };
    assert!(state.podcasts().show().is_some());
    let (state, _) = reduce(
        state,
        Action::PodcastRecommendationsCompleted {
            generation: *generation,
            requested_region: us,
            result: Err(ytermusic::app::AppError::new(
                ytermusic::app::AppErrorCategory::Podcast,
                "Rankings unavailable",
            )),
        },
    );
    assert_eq!(state.podcasts().recommendations().len(), 2);
    assert_eq!(
        state.podcasts().selected_recommendation(),
        Some(&selected_id)
    );
    assert!(state.podcasts().show().is_some());

    let (state, effects) = reduce(state, Action::ClosePodcast);
    assert!(effects.is_empty());
    assert!(state.podcasts().show().is_none());
    assert!(state.podcasts().selected_episode().is_none());
    assert_eq!(state.podcasts().recommendations().len(), 2);
    assert_eq!(
        state.podcasts().selected_recommendation(),
        Some(&selected_id)
    );
}

#[test]
fn podcast_recommendation_match_failure_preserves_list_and_selection() {
    let us = region("US");
    let page = podcast_recommendations("US", &[("daily", "The Daily", "NYT")]);
    let selected_id = page.items()[0].source_id().clone();
    let (state, effects) = reduce(
        AppState::default(),
        Action::PodcastRecommendationsRequested { region: us.clone() },
    );
    let [Effect::LoadPodcastRecommendations { generation, .. }] = effects.as_slice() else {
        panic!("source effect");
    };
    let (state, _) = reduce(
        state,
        Action::PodcastRecommendationsCompleted {
            generation: *generation,
            requested_region: us,
            result: Ok(page),
        },
    );
    let (state, effects) = reduce(state, Action::OpenSelectedPodcastRecommendation);
    let [Effect::ResolvePodcastRecommendation { generation, .. }] = effects.as_slice() else {
        panic!("match effect");
    };
    let (state, effects) = reduce(
        state,
        Action::PodcastRecommendationResolved {
            generation: *generation,
            result: Err(ytermusic::app::AppError::new(
                ytermusic::app::AppErrorCategory::Search,
                "Sensitive invalid match",
            )),
        },
    );
    assert!(effects.is_empty());
    assert_eq!(
        state.podcasts().selected_recommendation(),
        Some(&selected_id)
    );
    assert_eq!(state.podcasts().recommendations().len(), 1);
    let error = state
        .podcasts()
        .resolve_error()
        .unwrap_or_else(|| panic!("invalid match must be visible"));
    assert_eq!(error.category(), ytermusic::app::AppErrorCategory::Podcast);
    assert!(!error.message().contains("daily"));
}

fn buffer_text(buffer: &Buffer) -> String {
    buffer
        .content()
        .chunks(usize::from(buffer.area.width))
        .map(|row| {
            row.iter()
                .map(ratatui::buffer::Cell::symbol)
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn row_text(buffer: &Buffer, row: u16) -> String {
    (0..buffer.area.width)
        .filter_map(|column| buffer.cell((column, row)))
        .map(ratatui::buffer::Cell::symbol)
        .collect()
}

fn chart_request(effects: &[Effect]) -> (Generation, RegionCode) {
    let [
        Effect::ReadChartCache {
            generation,
            region,
            key,
        },
        Effect::LoadCharts {
            generation: live_generation,
            region: live_region,
        },
    ] = effects
    else {
        panic!("chart request must read cache and load live charts");
    };
    assert_eq!(generation, live_generation);
    assert_eq!(region, live_region);
    assert_eq!(key.region(), region);
    (*generation, region.clone())
}

fn state_with_podcast_episode(episode: MediaItem) -> AppState {
    let metadata = SearchMetadata::new(SearchMetadataKind::Podcast, "Replay Show")
        .with_provider_id("replay-show");
    let (state, effects) = reduce(
        AppState::default(),
        Action::SearchSubmitted {
            query: "replay show".to_owned(),
            filter: SearchFilter::Podcasts,
        },
    );
    let [Effect::Search { generation, .. }] = effects.as_slice() else {
        panic!("podcast fixture search must load");
    };
    let (state, _) = reduce(
        state,
        Action::SearchCompleted {
            generation: *generation,
            result: Ok(SearchPage::new(vec![SearchItem::Metadata(metadata)])),
        },
    );
    let (state, effects) = reduce(state, Action::OpenSelectedPodcast);
    let [Effect::LoadPodcast { generation, .. }] = effects.as_slice() else {
        panic!("podcast fixture must load its show");
    };
    reduce(
        state,
        Action::PodcastCompleted {
            generation: *generation,
            result: Ok(Podcast {
                id: "replay-show".to_owned(),
                title: "Replay Show".to_owned(),
                creators: vec!["Replay Host".to_owned()],
                description: None,
                artwork_url: None,
                episodes: vec![episode],
            }),
        },
    )
    .0
}

fn request_podcast_progress(state: AppState, media_id: &MediaId) -> (AppState, Generation) {
    let (state, effects) = reduce(
        state,
        Action::PlayPodcastEpisode {
            media_id: media_id.clone(),
        },
    );
    let [Effect::LoadPodcastProgress { generation, .. }] = effects.as_slice() else {
        panic!("podcast fixture must request persisted progress");
    };
    (state, *generation)
}

fn saved_checkpoint(effects: &[Effect]) -> &PodcastProgressCheckpoint {
    effects
        .iter()
        .find_map(|effect| match effect {
            Effect::SavePodcastProgress(checkpoint) => Some(checkpoint),
            _ => None,
        })
        .unwrap_or_else(|| panic!("expected a podcast progress checkpoint"))
}

fn persisted_session(effects: &[Effect]) -> &ytermusic::app::SessionCheckpoint {
    effects
        .iter()
        .find_map(|effect| match effect {
            Effect::Persist(checkpoint) => Some(checkpoint),
            _ => None,
        })
        .unwrap_or_else(|| panic!("expected a session checkpoint"))
}

fn apply_actions_and_persist_sessions(
    mut state: AppState,
    actions: impl IntoIterator<Item = Action>,
    storage: &mut dyn Storage,
    mut updated_at: i64,
) -> Result<(AppState, bool), ytermusic::storage::StorageError> {
    let mut saw_volume_command = false;
    for action in actions {
        let (next, effects) = reduce(state, action);
        state = next;
        saw_volume_command |= effects
            .iter()
            .any(|effect| matches!(effect, Effect::Player(PlayerCommand::Volume(63))));
        for effect in &effects {
            if let Effect::Persist(checkpoint) = effect {
                storage.save_session(checkpoint, updated_at)?;
                updated_at += 1;
            }
        }
    }
    Ok((state, saw_volume_command))
}

fn persist_podcast_checkpoint(
    storage: &mut dyn Storage,
    checkpoint: &PodcastProgressCheckpoint,
    updated_at: i64,
) -> Result<(), ytermusic::storage::StorageError> {
    storage.save_podcast_progress(&PodcastProgress {
        video_id: checkpoint.media_id().video_id.clone(),
        playback_epoch: checkpoint.playback_epoch(),
        position_ms: checkpoint.position_ms(),
        duration_ms: checkpoint.duration_ms(),
        played: checkpoint.played(),
        updated_at,
    })
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "this end-to-end workflow intentionally keeps its state transitions together"
)]
fn anonymous_startup_search_enqueue_and_play() -> Result<(), Box<dyn Error>> {
    let item = song("anon-song", "Midnight Terminal", "Artist One");
    let (state, search_effects) = reduce(
        AppState::new(config_without_lyrics()),
        Action::SearchSubmitted {
            query: "midnight".to_owned(),
            filter: SearchFilter::Songs,
        },
    );
    let [Effect::Search { generation, .. }] = search_effects.as_slice() else {
        panic!("anonymous search must issue one provider effect");
    };

    let (state, completion_effects) = reduce(
        state,
        Action::SearchCompleted {
            generation: *generation,
            result: Ok(SearchPage::new(vec![SearchItem::Playable(item.clone())])),
        },
    );
    assert!(completion_effects.is_empty());

    let search_id = SearchItemId::Media(item.id.clone());
    let (state, selection_effects) = reduce(
        state,
        Action::SearchSelectionChanged {
            id: search_id.clone(),
        },
    );
    assert!(selection_effects.is_empty());
    assert_eq!(state.search().selected_id(), Some(&search_id));

    let (state, enqueue_effects) = reduce(state, Action::EnqueueSelectedSearchResult);
    assert_eq!(enqueue_effects.len(), 1);
    assert!(matches!(enqueue_effects[0], Effect::Persist(_)));
    assert_eq!(state.queue().items().len(), 1);
    assert_eq!(state.playback().current.as_ref(), Some(&item.id));
    assert_eq!(state.playback().status, PlaybackStatus::Stopped);
    assert_eq!(state.playback().position_ms, 0);
    assert_eq!(state.playback().duration_ms, item.duration_ms);

    let queue_id = stable_queue_item_id(&item.id);
    let (state, activation_effects) = reduce(
        state,
        Action::PlayQueueItem {
            id: queue_id.clone(),
        },
    );
    let Some(playback_generation) = state.current_attempt_generation() else {
        panic!("playing the queued result must begin playback");
    };
    assert_eq!(state.queue().current().map(QueueItem::id), Some(&queue_id));
    assert_eq!(state.queue().current().map(QueueItem::media), Some(&item));
    assert!(matches!(
        activation_effects.as_slice(),
        [Effect::Resolve { .. }, Effect::Persist(_)]
    ));

    let (state, quality_effects) = reduce(
        state,
        Action::ResolvedFormatUpdated {
            generation: playback_generation,
            quality: ResolverQuality::new(Some("opus"), Some("251")),
        },
    );
    assert!(quality_effects.is_empty());
    let (state, resolve_effects) = reduce(
        state,
        Action::ResolveSucceeded {
            generation: playback_generation,
        },
    );
    assert!(resolve_effects.is_empty());
    let (state, player_effects) = reduce(
        state,
        Action::PlayerStatusChanged {
            generation: playback_generation,
            status: PlaybackStatus::Playing,
        },
    );
    assert_eq!(
        player_effects,
        vec![
            Effect::RecordHistory { item: item.clone() },
            Effect::Notify(NowPlayingNotification::from_media(
                playback_generation,
                &item,
            )),
        ]
    );
    let (state, telemetry_effects) = reduce(
        state,
        Action::PlaybackTelemetryUpdated {
            generation: playback_generation,
            effective_volume: 64.0,
            fade: Some(FadeActivity::In),
        },
    );
    assert!(telemetry_effects.is_empty());
    assert_eq!(state.playback().status, PlaybackStatus::Playing);

    let backend = TestBackend::new(140, 40);
    let mut terminal = Terminal::new(backend)?;
    terminal.draw(|frame| {
        render_with_model(
            frame,
            &state,
            &Theme::default(),
            &RenderModel::default().with_view(NavigationItem::Search),
        );
    })?;
    let buffer = terminal.backend().buffer();
    let rendered = buffer_text(buffer);
    assert!(rendered.contains("Midnight Terminal"));
    assert!(rendered.contains("Queue · 1"));
    let player = (35..40)
        .map(|row| row_text(buffer, row))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        player.contains("Artist One"),
        "persistent player must show the current creator:\n{player}"
    );
    assert!(player.contains("Effective 64%"));
    assert!(player.contains("Fade in"));
    assert!(player.contains("Quality 251/opus"));
    assert!(
        !player.contains("Speed"),
        "music tracks must not show podcast-only speed:\n{player}"
    );

    let backend = TestBackend::new(90, 30);
    let mut terminal = Terminal::new(backend)?;
    terminal.draw(|frame| {
        render_with_model(
            frame,
            &state,
            &Theme::default(),
            &RenderModel::default().with_view(NavigationItem::Search),
        );
    })?;
    let compact_player = (26..30)
        .map(|row| row_text(terminal.backend().buffer(), row))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(compact_player.contains("Q:251/opus"));
    assert!(compact_player.contains("S:off"));
    assert!(compact_player.contains("R:Off"));
    assert!(compact_player.contains("Ra:off"));

    Ok(())
}

#[test]
fn first_anonymous_enqueue_persists_a_coherent_restorable_session() -> Result<(), Box<dyn Error>> {
    let directory = TempDir::new()?;
    let path = directory.path().join("first-anonymous-enqueue.sqlite3");
    let item = song("first-enqueue", "First Enqueue", "Anonymous Artist");
    let (state, effects) = reduce(
        AppState::default(),
        Action::SearchSubmitted {
            query: "first enqueue".to_owned(),
            filter: SearchFilter::Songs,
        },
    );
    let [Effect::Search { generation, .. }] = effects.as_slice() else {
        panic!("search fixture must issue one search");
    };
    let (state, _) = reduce(
        state,
        Action::SearchCompleted {
            generation: *generation,
            result: Ok(SearchPage::new(vec![SearchItem::Playable(item.clone())])),
        },
    );

    let (state, enqueue_effects) = reduce(state, Action::EnqueueSelectedSearchResult);
    let checkpoint = persisted_session(&enqueue_effects).clone();
    let mut storage = SqliteStorage::open(&path)?;
    storage.save_session(&checkpoint, 100)?;
    drop(storage);

    let storage = SqliteStorage::open(&path)?;
    assert_eq!(storage.load_session()?, Some(checkpoint));
    assert_eq!(state.playback().current.as_ref(), Some(&item.id));
    assert_eq!(state.playback().status, PlaybackStatus::Stopped);
    assert_eq!(state.playback().position_ms, 0);
    assert_eq!(state.playback().duration_ms, item.duration_ms);

    let (state, play_effects) = reduce(
        state,
        Action::PlayQueueItem {
            id: stable_queue_item_id(&item.id),
        },
    );
    assert!(state.current_attempt_generation().is_some());
    assert!(play_effects.iter().any(
        |effect| matches!(effect, Effect::Resolve { item: resolved, .. } if resolved == &item)
    ));
    Ok(())
}

#[test]
fn country_picker_switches_hk_to_us_without_late_hk_overwrite() -> Result<(), Box<dyn Error>> {
    let hk = region("hk");
    let us = region("us");
    let config = Config {
        region: hk.clone(),
        ..Config::default()
    };
    let state = AppState::new(config);
    assert_eq!(
        state.charts().region(),
        Some(&hk),
        "configured country should seed the picker"
    );

    let (state, hk_effects) = reduce(state, Action::ChartsRequested { region: hk.clone() });
    let (hk_generation, hk_effect_region) = chart_request(&hk_effects);
    assert_eq!(hk_effect_region, hk);

    let (state, us_effects) = reduce(state, Action::ChartsRequested { region: us.clone() });
    let (us_generation, us_effect_region) = chart_request(&us_effects);
    assert_eq!(us_effect_region, us);

    let old_hk = song("hk-old", "Late HK number one", "HK Artist");
    let (state, stale_effects) = reduce(
        state,
        Action::ChartsCompleted {
            generation: hk_generation,
            region: hk.clone(),
            received_at: 1_000,
            result: Ok(vec![ChartSection::new("HK Top songs", vec![old_hk])]),
        },
    );
    assert!(stale_effects.is_empty());
    assert!(state.charts().loading());
    assert!(state.charts().sections().is_empty());
    assert_eq!(state.charts().region(), Some(&us));

    let us_item = song("us-current", "US number one", "US Artist");
    let (state, current_effects) = reduce(
        state,
        Action::ChartsCompleted {
            generation: us_generation,
            region: us.clone(),
            received_at: 1_001,
            result: Ok(vec![ChartSection::new(
                "US Top songs",
                vec![us_item.clone()],
            )]),
        },
    );
    assert!(matches!(
        current_effects.as_slice(),
        [Effect::StoreChartCache { .. }]
    ));
    assert_eq!(state.charts().sections()[0].items(), &[us_item]);

    let backend = TestBackend::new(140, 40);
    let mut terminal = Terminal::new(backend)?;
    terminal.draw(|frame| {
        render_with_model(
            frame,
            &state,
            &Theme::default(),
            &RenderModel::default().with_view(NavigationItem::Charts),
        );
    })?;
    let rendered = buffer_text(terminal.backend().buffer());
    assert!(rendered.contains("Trending in US"));
    assert!(rendered.contains("US number one"));
    assert!(!rendered.contains("Late HK number one"));

    Ok(())
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "this end-to-end workflow intentionally keeps resume and checkpoint transitions together"
)]
fn podcast_search_open_episode_play_and_resume_saved_position() -> Result<(), Box<dyn Error>> {
    let episode = MediaItem {
        kind: MediaKind::PodcastEpisode,
        ..song("episode-7", "Episode Seven", "Terminal Stories")
    };
    let show_result = SearchMetadata::new(SearchMetadataKind::Podcast, "Terminal Stories")
        .with_provider_id("show-terminal-stories");
    let (state, search_effects) = reduce(
        AppState::default(),
        Action::SearchSubmitted {
            query: "terminal stories".to_owned(),
            filter: SearchFilter::Podcasts,
        },
    );
    let [Effect::Search { generation, .. }] = search_effects.as_slice() else {
        panic!("podcast search must issue one provider effect");
    };
    let (state, _) = reduce(
        state,
        Action::SearchCompleted {
            generation: *generation,
            result: Ok(SearchPage::new(vec![SearchItem::Metadata(show_result)])),
        },
    );

    let (state, open_effects) = reduce(state, Action::OpenSelectedPodcast);
    let [
        Effect::LoadPodcast {
            generation: show_generation,
            id,
        },
    ] = open_effects.as_slice()
    else {
        panic!("opening a podcast result must load the selected show");
    };
    assert_eq!(id.as_str(), "show-terminal-stories");
    let show = Podcast {
        id: id.as_str().to_owned(),
        title: "Terminal Stories".to_owned(),
        creators: vec!["Story Network".to_owned()],
        description: Some("Stories about command lines.".to_owned()),
        artwork_url: None,
        episodes: vec![episode.clone()],
    };
    let (state, show_effects) = reduce(
        state,
        Action::PodcastCompleted {
            generation: *show_generation,
            result: Ok(show.clone()),
        },
    );
    assert!(show_effects.is_empty());
    assert_eq!(state.podcasts().show(), Some(&show));
    assert_eq!(state.podcasts().selected_episode(), Some(&episode.id));

    let (state, progress_effects) = reduce(
        state,
        Action::PlayPodcastEpisode {
            media_id: episode.id.clone(),
        },
    );
    let [
        Effect::LoadPodcastProgress {
            generation: progress_generation,
            media_id,
        },
    ] = progress_effects.as_slice()
    else {
        panic!("playing an episode must load its saved position first");
    };
    assert_eq!(media_id, &episode.id);
    let saved = PodcastProgress {
        video_id: episode.id.video_id.clone(),
        playback_epoch: 7,
        position_ms: 65_000,
        duration_ms: episode.duration_ms,
        played: false,
        updated_at: 1_700_000_000,
    };
    let (state, resume_effects) = reduce(
        state,
        Action::PodcastProgressLoaded {
            generation: *progress_generation,
            progress: Some(saved),
        },
    );
    assert!(resume_effects.iter().any(|effect| {
        matches!(
            effect,
            Effect::Resolve {
                item,
                start_ms: Some(65_000),
                ..
            } if item == &episode
        )
    }));
    assert!(resume_effects.iter().any(|effect| {
        matches!(
            effect,
            Effect::SavePodcastProgress(checkpoint)
                if checkpoint.media_id() == &episode.id
                    && checkpoint.playback_epoch() == 8
                    && checkpoint.position_ms() == 65_000
                    && !checkpoint.played()
        )
    }));
    let Some(playback_generation) = state.current_attempt_generation() else {
        panic!("loaded podcast progress must begin resolution");
    };
    assert_eq!(state.playback().position_ms, 65_000);

    let (state, post_load_effects) = reduce(
        state,
        Action::ResolveSucceeded {
            generation: playback_generation,
        },
    );
    assert!(post_load_effects.is_empty());
    let (state, _) = reduce(
        state,
        Action::PlayerStatusChanged {
            generation: playback_generation,
            status: PlaybackStatus::Playing,
        },
    );

    let backend = TestBackend::new(140, 40);
    let mut terminal = Terminal::new(backend)?;
    terminal.draw(|frame| {
        render_with_model(
            frame,
            &state,
            &Theme::default(),
            &RenderModel::default().with_view(NavigationItem::Podcasts),
        );
    })?;
    let rendered = buffer_text(terminal.backend().buffer());
    assert!(rendered.contains("Terminal Stories"));
    assert!(rendered.contains("Episode Seven"));
    assert!(rendered.contains("Resume 01:05"));
    let player = (35..40)
        .map(|row| row_text(terminal.backend().buffer(), row))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(player.contains("Speed 1.00×"));

    let (state, progress_effects) = reduce(
        state,
        Action::PlayerProgress {
            generation: playback_generation,
            media_id: episode.id.clone(),
            position_ms: 70_000,
            duration_ms: episode.duration_ms,
        },
    );
    assert_eq!(
        progress_effects,
        vec![Effect::SavePodcastProgress(PodcastProgressCheckpoint::new(
            episode.id.clone(),
            8,
            70_000,
            episode.duration_ms,
            false
        ))]
    );
    let (_, stopped_effects) = reduce(
        state.clone(),
        Action::PlayerStatusChanged {
            generation: playback_generation,
            status: PlaybackStatus::Stopped,
        },
    );
    assert!(stopped_effects.iter().any(|effect| {
        matches!(
            effect,
            Effect::SavePodcastProgress(checkpoint)
                if checkpoint.media_id() == &episode.id
                    && checkpoint.playback_epoch() == 8
                    && checkpoint.position_ms() == 70_000
                    && !checkpoint.played()
        )
    }));
    let (_, failed_effects) = reduce(
        state.clone(),
        Action::PlayerStatusChanged {
            generation: playback_generation,
            status: PlaybackStatus::Failed,
        },
    );
    assert!(failed_effects.iter().any(|effect| {
        matches!(
            effect,
            Effect::SavePodcastProgress(checkpoint)
                if checkpoint.media_id() == &episode.id
                    && checkpoint.playback_epoch() == 8
                    && checkpoint.position_ms() == 70_000
                    && !checkpoint.played()
        )
    }));
    let (_, ended_effects) = reduce(
        state,
        Action::PlayerEnded {
            generation: playback_generation,
        },
    );
    assert!(ended_effects.iter().any(|effect| {
        matches!(
            effect,
            Effect::SavePodcastProgress(checkpoint)
                if checkpoint.media_id() == &episode.id
                    && checkpoint.playback_epoch() == 8
                    && checkpoint.played()
        )
    }));

    Ok(())
}

#[test]
fn unavailable_podcast_preflight_preserves_completed_progress_row() -> Result<(), Box<dyn Error>> {
    let directory = TempDir::new()?;
    let path = directory
        .path()
        .join("podcast-unavailable-preflight.sqlite3");
    let episode = MediaItem {
        kind: MediaKind::PodcastEpisode,
        duration_ms: Some(100_000),
        ..song(
            "unavailable-preflight",
            "Unavailable Preflight",
            "Replay Host",
        )
    };
    let prior = PodcastProgress {
        video_id: episode.id.video_id.clone(),
        playback_epoch: 7,
        position_ms: 100_000,
        duration_ms: episode.duration_ms,
        played: true,
        updated_at: 100,
    };
    let mut storage = SqliteStorage::open(&path)?;
    storage.save_podcast_progress(&prior)?;

    let unavailable = DoctorReport::new(vec![
        DiagnosticRow::new(
            "browsing",
            DiagnosticStatus::Healthy,
            "metadata browsing available",
        ),
        DiagnosticRow::new(
            "playback",
            DiagnosticStatus::Unhealthy,
            "unavailable; browsing still works",
        ),
    ]);
    let (state, _) = reduce(
        AppState::default(),
        Action::DependencyReportLoaded(unavailable),
    );
    let (state, _) = reduce(
        state,
        Action::EnqueueMedia {
            item: episode.clone(),
        },
    );
    let (state, start_effects) = reduce(
        state,
        Action::PlayQueueItem {
            id: stable_queue_item_id(&episode.id),
        },
    );
    let Some(progress_generation) = start_effects.iter().find_map(|effect| match effect {
        Effect::LoadPodcastProgress {
            generation,
            media_id,
        } if media_id == &episode.id => Some(*generation),
        _ => None,
    }) else {
        panic!("preflight fixture must request stored podcast progress");
    };
    let loaded = storage.load_podcast_progress(&episode.id.video_id)?;

    let (state, effects) = reduce(
        state,
        Action::PodcastProgressLoaded {
            generation: progress_generation,
            progress: loaded,
        },
    );
    let mut progress_save_count = 0;
    for effect in &effects {
        match effect {
            Effect::SavePodcastProgress(checkpoint) => {
                progress_save_count += 1;
                persist_podcast_checkpoint(&mut storage, checkpoint, 200)?;
            }
            Effect::Persist(checkpoint) => storage.save_session(checkpoint, 200)?,
            _ => {}
        }
    }
    drop(storage);

    let storage = SqliteStorage::open(&path)?;
    assert_eq!(
        storage.load_podcast_progress(&episode.id.video_id)?,
        Some(prior)
    );
    assert_eq!(progress_save_count, 0);
    assert!(state.current_attempt_generation().is_none());
    assert!(state.current_resolve_generation().is_none());
    assert!(state.current_podcast_epoch().is_none());
    assert!(state.podcasts().pending_progress_generation().is_none());
    assert_eq!(state.playback().status, PlaybackStatus::Failed);
    assert!(state.diagnostics().iter().any(|diagnostic| {
        diagnostic.category() == ytermusic::app::DiagnosticCategory::Resolve
            && diagnostic.media_id() == Some(&episode.id)
    }));
    Ok(())
}

#[test]
fn completed_podcast_replay_survives_delayed_old_checkpoint_and_restart()
-> Result<(), Box<dyn Error>> {
    let directory = TempDir::new()?;
    let path = directory.path().join("podcast-replay.sqlite3");
    let episode = MediaItem {
        kind: MediaKind::PodcastEpisode,
        duration_ms: Some(100_000),
        ..song("replayed-episode", "Replayed Episode", "Replay Host")
    };
    let mut storage = SqliteStorage::open(&path)?;
    storage.save_podcast_progress(&PodcastProgress {
        video_id: episode.id.video_id.clone(),
        playback_epoch: 7,
        position_ms: 100_000,
        duration_ms: episode.duration_ms,
        played: true,
        updated_at: 100,
    })?;

    let state = state_with_podcast_episode(episode.clone());
    let (state, progress_generation) = request_podcast_progress(state, &episode.id);
    let finished = storage.load_podcast_progress(&episode.id.video_id)?;
    let (state, replay_effects) = reduce(
        state,
        Action::PodcastProgressLoaded {
            generation: progress_generation,
            progress: finished,
        },
    );
    assert_eq!(state.playback().position_ms, 0);
    let replay_baseline = saved_checkpoint(&replay_effects);
    assert_eq!(replay_baseline.playback_epoch(), 8);
    assert!(!replay_baseline.played());
    persist_podcast_checkpoint(&mut storage, replay_baseline, 200)?;

    let Some(playback_generation) = state.current_attempt_generation() else {
        panic!("the fresh replay must begin a playback attempt");
    };
    let (_, midpoint_effects) = reduce(
        state,
        Action::PlayerProgress {
            generation: playback_generation,
            media_id: episode.id.clone(),
            position_ms: 40_000,
            duration_ms: episode.duration_ms,
        },
    );
    persist_podcast_checkpoint(&mut storage, saved_checkpoint(&midpoint_effects), 300)?;
    storage.save_podcast_progress(&PodcastProgress {
        video_id: episode.id.video_id.clone(),
        playback_epoch: 7,
        position_ms: 100_000,
        duration_ms: episode.duration_ms,
        played: true,
        updated_at: 999,
    })?;
    drop(storage);

    let storage = SqliteStorage::open(&path)?;
    let persisted_midpoint = storage.load_podcast_progress(&episode.id.video_id)?;
    let state = state_with_podcast_episode(episode.clone());
    let (state, progress_generation) = request_podcast_progress(state, &episode.id);
    let (state, restarted_effects) = reduce(
        state,
        Action::PodcastProgressLoaded {
            generation: progress_generation,
            progress: persisted_midpoint,
        },
    );

    assert_eq!(state.playback().position_ms, 40_000);
    let restarted_baseline = saved_checkpoint(&restarted_effects);
    assert_eq!(restarted_baseline.playback_epoch(), 9);
    assert_eq!(restarted_baseline.position_ms(), 40_000);
    assert!(!restarted_baseline.played());
    Ok(())
}

#[test]
fn queued_podcast_replay_loads_progress_before_resolving_without_an_open_show()
-> Result<(), Box<dyn Error>> {
    let directory = TempDir::new()?;
    let path = directory.path().join("queued-podcast-replay.sqlite3");
    let episode = MediaItem {
        kind: MediaKind::PodcastEpisode,
        duration_ms: Some(100_000),
        ..song("queued-replay", "Queued Replay", "Replay Host")
    };
    let mut storage = SqliteStorage::open(&path)?;
    storage.save_podcast_progress(&PodcastProgress {
        video_id: episode.id.video_id.clone(),
        playback_epoch: 7,
        position_ms: 40_000,
        duration_ms: episode.duration_ms,
        played: false,
        updated_at: 100,
    })?;

    let (state, _) = reduce(
        AppState::default(),
        Action::EnqueueMedia {
            item: episode.clone(),
        },
    );
    assert!(state.podcasts().show().is_none());
    let queue_id = stable_queue_item_id(&episode.id);
    let (state, start_effects) = reduce(state, Action::PlayQueueItem { id: queue_id });
    let Some(progress_generation) = start_effects.iter().find_map(|effect| match effect {
        Effect::LoadPodcastProgress {
            generation,
            media_id,
        } if media_id == &episode.id => Some(*generation),
        _ => None,
    }) else {
        panic!("queued podcast playback must load persisted progress first");
    };
    assert!(
        !start_effects
            .iter()
            .any(|effect| matches!(effect, Effect::Resolve { .. }))
    );
    assert!(
        !start_effects
            .iter()
            .any(|effect| matches!(effect, Effect::SavePodcastProgress(_)))
    );

    let saved = storage.load_podcast_progress(&episode.id.video_id)?;
    let (state, resume_effects) = reduce(
        state,
        Action::PodcastProgressLoaded {
            generation: progress_generation,
            progress: saved,
        },
    );
    assert_eq!(state.playback().position_ms, 40_000);
    assert!(
        resume_effects
            .iter()
            .any(|effect| matches!(effect, Effect::Resolve { item, .. } if item == &episode))
    );
    let baseline = saved_checkpoint(&resume_effects);
    assert_eq!(baseline.playback_epoch(), 8);
    assert_eq!(baseline.position_ms(), 40_000);
    assert!(!baseline.played());

    let Some(playback_generation) = state.current_attempt_generation() else {
        panic!("loaded queued podcast progress must begin resolution");
    };
    let (_, progress_effects) = reduce(
        state,
        Action::PlayerProgress {
            generation: playback_generation,
            media_id: episode.id.clone(),
            position_ms: 45_000,
            duration_ms: episode.duration_ms,
        },
    );
    let checkpoint = saved_checkpoint(&progress_effects);
    assert_eq!(checkpoint.playback_epoch(), 8);
    persist_podcast_checkpoint(&mut storage, checkpoint, 200)?;
    drop(storage);

    let storage = SqliteStorage::open(&path)?;
    let persisted = storage
        .load_podcast_progress(&episode.id.video_id)?
        .unwrap_or_else(|| panic!("queued replay checkpoint must persist"));
    assert_eq!(persisted.playback_epoch, 8);
    assert_eq!(persisted.position_ms, 45_000);
    assert!(!persisted.played);
    Ok(())
}

#[test]
fn queued_podcast_switch_defers_session_persist_until_progress_is_loaded()
-> Result<(), Box<dyn Error>> {
    let directory = TempDir::new()?;
    let path = directory.path().join("queued-podcast-switch.sqlite3");
    let track = song("switch-song", "Switch Song", "Artist");
    let episode = MediaItem {
        kind: MediaKind::PodcastEpisode,
        ..song("switch-episode", "Switch Episode", "Host")
    };
    let (state, _) = reduce(
        AppState::default(),
        Action::EnqueueMedia {
            item: track.clone(),
        },
    );
    let (state, _) = reduce(
        state,
        Action::PlayQueueItem {
            id: stable_queue_item_id(&track.id),
        },
    );
    let song_generation = state
        .current_attempt_generation()
        .unwrap_or_else(|| panic!("song fixture must start an attempt"));
    let (state, _) = reduce(
        state,
        Action::ResolveSucceeded {
            generation: song_generation,
        },
    );
    let (state, _) = reduce(
        state,
        Action::PlayerStatusChanged {
            generation: song_generation,
            status: PlaybackStatus::Playing,
        },
    );
    let (state, enqueue_effects) = reduce(
        state,
        Action::EnqueueMedia {
            item: episode.clone(),
        },
    );
    let mut storage = SqliteStorage::open(&path)?;
    storage.save_session(persisted_session(&enqueue_effects), 100)?;
    let saved_song_session = storage.load_session()?;

    let (state, start_effects) = reduce(
        state,
        Action::PlayQueueItem {
            id: stable_queue_item_id(&episode.id),
        },
    );
    let Some(progress_generation) = start_effects.iter().find_map(|effect| match effect {
        Effect::LoadPodcastProgress {
            generation,
            media_id,
        } if media_id == &episode.id => Some(*generation),
        _ => None,
    }) else {
        panic!("podcast switch must load progress");
    };
    assert!(
        !start_effects
            .iter()
            .any(|effect| matches!(effect, Effect::Persist(_)))
    );
    assert_eq!(storage.load_session()?, saved_song_session);

    let (state, loaded_effects) = reduce(
        state,
        Action::PodcastProgressLoaded {
            generation: progress_generation,
            progress: None,
        },
    );
    let coherent = persisted_session(&loaded_effects);
    assert_eq!(
        coherent.queue.current.as_ref(),
        state.queue().snapshot().current.as_ref()
    );
    assert_eq!(coherent.playback.current.as_ref(), Some(&episode.id));
    storage.save_session(coherent, 200)?;
    let expected = coherent.clone();
    drop(storage);

    let storage = SqliteStorage::open(&path)?;
    assert_eq!(storage.load_session()?, Some(expected));
    Ok(())
}

#[test]
fn pending_podcast_defers_every_incoherent_session_persist_until_progress_load()
-> Result<(), Box<dyn Error>> {
    let directory = TempDir::new()?;
    let path = directory.path().join("pending-podcast-gate.sqlite3");
    let outgoing = song("persist-outgoing", "Persist Outgoing", "Artist");
    let episode = MediaItem {
        kind: MediaKind::PodcastEpisode,
        ..song("persist-episode", "Persist Episode", "Host")
    };
    let moved = song("persist-moved", "Persist Moved", "Artist");
    let appended = song("persist-appended", "Persist Appended", "Artist");
    let mut state = AppState::default();
    for item in [outgoing.clone(), episode.clone(), moved.clone()] {
        (state, _) = reduce(state, Action::EnqueueMedia { item });
    }
    let (state, song_effects) = reduce(
        state,
        Action::PlayQueueItem {
            id: stable_queue_item_id(&outgoing.id),
        },
    );
    let outgoing_generation = state
        .current_attempt_generation()
        .unwrap_or_else(|| panic!("outgoing song must start playback"));
    let mut storage = SqliteStorage::open(&path)?;
    let saved_song_session = persisted_session(&song_effects).clone();
    storage.save_session(&saved_song_session, 100)?;

    let (state, pending_effects) = reduce(
        state,
        Action::PlayQueueItem {
            id: stable_queue_item_id(&episode.id),
        },
    );
    let Some(progress_generation) = pending_effects.iter().find_map(|effect| match effect {
        Effect::LoadPodcastProgress {
            generation,
            media_id,
        } if media_id == &episode.id => Some(*generation),
        _ => None,
    }) else {
        panic!("podcast switch must request progress");
    };
    assert!(state.current_attempt_generation().is_none());

    let actions = [
        Action::TargetVolumeChanged(63),
        Action::RepeatModeChanged(RepeatMode::All),
        Action::ShuffleEnabledChanged {
            enabled: true,
            seed: 42,
        },
        Action::QueueItemMovedBefore {
            id: stable_queue_item_id(&moved.id),
            before: stable_queue_item_id(&outgoing.id),
        },
        Action::EnqueueMedia {
            item: appended.clone(),
        },
        Action::RadioEnabledChanged(true),
        Action::PlayerStatusChanged {
            generation: outgoing_generation,
            status: PlaybackStatus::Stopped,
        },
        Action::PlayerEnded {
            generation: outgoing_generation,
        },
    ];
    let (state, saw_volume_command) =
        apply_actions_and_persist_sessions(state, actions, &mut storage, 200)?;
    assert!(saw_volume_command);
    assert_eq!(storage.load_session()?, Some(saved_song_session));
    assert_eq!(state.playback().target_volume, 63);
    assert_eq!(state.queue().repeat(), RepeatMode::All);
    assert!(state.queue().is_shuffled());
    assert!(state.queue().radio_enabled());
    assert!(
        state
            .queue()
            .items()
            .iter()
            .any(|item| item.media() == &appended)
    );
    assert_eq!(
        state.podcasts().pending_progress_generation(),
        Some(progress_generation)
    );

    let (state, loaded_effects) = reduce(
        state,
        Action::PodcastProgressLoaded {
            generation: progress_generation,
            progress: None,
        },
    );
    let coherent = persisted_session(&loaded_effects);
    assert_eq!(coherent.queue, state.queue().snapshot());
    assert_eq!(coherent.playback, state.playback().clone());
    storage.save_session(coherent, 300)?;
    let expected = coherent.clone();
    drop(storage);

    let storage = SqliteStorage::open(&path)?;
    assert_eq!(storage.load_session()?, Some(expected));
    Ok(())
}

#[test]
fn shuffle_repeat_radio_and_queue_reorder_preserve_current_and_active_ids()
-> Result<(), Box<dyn Error>> {
    let items = [
        song("queue-a", "Queue A", "Artist A"),
        song("queue-b", "Queue B", "Artist B"),
        song("queue-c", "Queue C", "Artist C"),
    ];
    let (state, effects) = reduce(
        AppState::default(),
        Action::SearchSubmitted {
            query: "queue".to_owned(),
            filter: SearchFilter::Songs,
        },
    );
    let [Effect::Search { generation, .. }] = effects.as_slice() else {
        panic!("queue fixture search must issue one provider effect");
    };
    let (mut state, _) = reduce(
        state,
        Action::SearchCompleted {
            generation: *generation,
            result: Ok(SearchPage::new(
                items.iter().cloned().map(SearchItem::Playable).collect(),
            )),
        },
    );
    for index in 0..items.len() {
        (state, _) = reduce(state, Action::ActivateSearchResult { index });
    }

    let current_id = stable_queue_item_id(&items[2].id);
    assert_eq!(
        state.queue().current().map(QueueItem::id),
        Some(&current_id)
    );
    let logical_ids = state
        .queue()
        .items()
        .iter()
        .map(|item| item.id().clone())
        .collect::<HashSet<_>>();

    let (state, shuffle_effects) = reduce(
        state,
        Action::ShuffleEnabledChanged {
            enabled: true,
            seed: 42,
        },
    );
    assert!(state.queue().is_shuffled());
    assert_eq!(state.queue().active_ids().first(), Some(&current_id));
    assert!(matches!(shuffle_effects.as_slice(), [Effect::Persist(_)]));

    let (state, repeat_effects) = reduce(state, Action::RepeatModeChanged(RepeatMode::All));
    assert_eq!(state.queue().repeat(), RepeatMode::All);
    assert!(matches!(repeat_effects.as_slice(), [Effect::Persist(_)]));

    let (state, radio_effects) = reduce(state, Action::RadioEnabledChanged(true));
    assert!(state.queue().radio_enabled());
    assert!(matches!(radio_effects.first(), Some(Effect::Persist(_))));

    let moved = stable_queue_item_id(&items[1].id);
    let before = stable_queue_item_id(&items[0].id);
    let active_before_move = state.queue().active_ids().to_vec();
    let (state, reorder_effects) = reduce(
        state,
        Action::QueueItemMovedBefore {
            id: moved.clone(),
            before: before.clone(),
        },
    );
    assert!(matches!(reorder_effects.as_slice(), [Effect::Persist(_)]));
    assert_eq!(
        state.queue().current().map(QueueItem::id),
        Some(&current_id)
    );
    assert_eq!(
        state
            .queue()
            .items()
            .iter()
            .map(|item| item.id().clone())
            .collect::<Vec<_>>(),
        vec![moved, before, stable_queue_item_id(&items[2].id),]
    );
    assert_eq!(
        state
            .queue()
            .active_ids()
            .iter()
            .cloned()
            .collect::<HashSet<_>>(),
        logical_ids
    );
    assert_ne!(state.queue().active_ids(), active_before_move);

    let backend = TestBackend::new(140, 40);
    let mut terminal = Terminal::new(backend)?;
    terminal.draw(|frame| {
        render_with_model(frame, &state, &Theme::default(), &RenderModel::default());
    })?;
    let rendered = buffer_text(terminal.backend().buffer());
    assert!(rendered.contains("Shuffle on"));
    assert!(rendered.contains("Repeat All"));
    assert!(rendered.contains("Radio on"));

    Ok(())
}

#[test]
fn queue_playback_controls_use_stable_ids_and_typed_player_effects() {
    let items = [
        song("control-a", "Control A", "Artist A"),
        song("control-b", "Control B", "Artist B"),
    ];
    let (state, effects) = reduce(
        AppState::new(config_without_lyrics()),
        Action::SearchSubmitted {
            query: "controls".to_owned(),
            filter: SearchFilter::Songs,
        },
    );
    let [Effect::Search { generation, .. }] = effects.as_slice() else {
        panic!("search effect");
    };
    let (mut state, _) = reduce(
        state,
        Action::SearchCompleted {
            generation: *generation,
            result: Ok(SearchPage::new(
                items.iter().cloned().map(SearchItem::Playable).collect(),
            )),
        },
    );
    for index in 0..items.len() {
        (state, _) = reduce(state, Action::ActivateSearchResult { index });
    }

    let first_id = stable_queue_item_id(&items[0].id);
    let (state, play_effects) = reduce(
        state,
        Action::PlayQueueItem {
            id: first_id.clone(),
        },
    );
    assert!(matches!(
        play_effects.as_slice(),
        [Effect::Resolve { .. }, Effect::Persist(_)]
    ));
    assert_eq!(state.queue().current().map(QueueItem::id), Some(&first_id));
    let generation = state
        .current_attempt_generation()
        .unwrap_or_else(|| panic!("playback attempt"));

    let (state, _) = reduce(
        state,
        Action::PlayerStatusChanged {
            generation,
            status: PlaybackStatus::Playing,
        },
    );
    let (state, pause_effects) = reduce(state, Action::TogglePlayback);
    assert_eq!(pause_effects, vec![Effect::Player(PlayerCommand::Pause)]);
    let (state, _) = reduce(
        state,
        Action::PlayerStatusChanged {
            generation,
            status: PlaybackStatus::Paused,
        },
    );
    let (state, resume_effects) = reduce(state, Action::TogglePlayback);
    assert_eq!(resume_effects, vec![Effect::Player(PlayerCommand::Resume)]);

    let second_id = stable_queue_item_id(&items[1].id);
    let (state, next_effects) = reduce(state, Action::NextRequested);
    assert!(matches!(
        next_effects.as_slice(),
        [Effect::Resolve { .. }, Effect::Persist(_)]
    ));
    assert_eq!(state.queue().current().map(QueueItem::id), Some(&second_id));

    let (state, previous_effects) = reduce(state, Action::PreviousRequested);
    assert!(matches!(
        previous_effects.as_slice(),
        [Effect::Resolve { .. }, Effect::Persist(_)]
    ));
    assert_eq!(state.queue().current().map(QueueItem::id), Some(&first_id));
}

#[test]
fn authenticated_library_content_and_anonymous_connect_prompt() -> Result<(), Box<dyn Error>> {
    let anonymous = AppState::default();
    let backend = TestBackend::new(140, 40);
    let mut terminal = Terminal::new(backend)?;
    terminal.draw(|frame| {
        render_with_model(
            frame,
            &anonymous,
            &Theme::default(),
            &RenderModel::default().with_view(NavigationItem::Library),
        );
    })?;
    let anonymous_render = buffer_text(terminal.backend().buffer());
    assert!(anonymous_render.contains("[a] Connect account"));

    let (anonymous, anonymous_load) = reduce(
        anonymous,
        Action::LibraryRequested {
            section: LibrarySection::Playlists,
        },
    );
    assert!(
        anonymous_load.is_empty(),
        "anonymous mode must not issue an authenticated request"
    );
    let (anonymous, connect_effects) = reduce(
        anonymous,
        Action::ConnectAccountRequested {
            browser: Browser::Firefox,
        },
    );
    assert_eq!(
        connect_effects,
        vec![Effect::ConnectAccount {
            browser: Browser::Firefox,
        }]
    );

    let (authenticated, auth_effects) = reduce(
        anonymous,
        Action::AuthenticationChanged(AuthenticationState::Authenticated),
    );
    assert!(auth_effects.is_empty());
    let (loading, library_effects) = reduce(
        authenticated,
        Action::LibraryRequested {
            section: LibrarySection::Playlists,
        },
    );
    let [
        Effect::LoadLibrary {
            generation,
            section,
            continuation,
        },
    ] = library_effects.as_slice()
    else {
        panic!("authenticated library must issue one load effect");
    };
    assert_eq!(*section, LibrarySection::Playlists);
    assert!(continuation.is_none());

    let playlist = LibraryItem::Playlist(BrowseItem {
        id: "playlist-favorites".to_owned(),
        title: "Terminal Favorites".to_owned(),
        subtitle: Some("42 songs".to_owned()),
        artwork_url: None,
    });
    let (loaded, completion_effects) = reduce(
        loading,
        Action::LibraryCompleted {
            generation: *generation,
            result: Ok(Page {
                items: vec![playlist.clone()],
                continuation: Some("opaque-next-page".to_owned()),
                stale: false,
            }),
        },
    );
    assert!(completion_effects.is_empty());
    assert_eq!(loaded.library().items(), &[playlist]);
    assert!(loaded.library().selected_id().is_some());
    assert!(loaded.library().continuation().is_some());

    terminal.draw(|frame| {
        render_with_model(
            frame,
            &loaded,
            &Theme::default(),
            &RenderModel::default().with_view(NavigationItem::Library),
        );
    })?;
    let authenticated_render = buffer_text(terminal.backend().buffer());
    assert!(authenticated_render.contains("Terminal Favorites"));
    assert!(authenticated_render.contains("[m] Load more"));
    assert!(!authenticated_render.contains("Connect account"));

    Ok(())
}

#[test]
fn offline_cached_chart_is_visible_with_explicit_stale_marker() -> Result<(), Box<dyn Error>> {
    let us = region("us");
    let cached_item = song("cached-us", "Cached US hit", "Offline Artist");
    let (state, effects) = reduce(
        AppState::default(),
        Action::ChartsRequested { region: us.clone() },
    );
    let (generation, _) = chart_request(&effects);

    let cached_at = 1_700_000_000;
    let (state, completion_effects) = reduce(
        state,
        Action::CachedChartsCompleted {
            generation,
            region: us.clone(),
            observed_at: cached_at + 3_601,
            result: Ok(Some(
                ChartCachePayload::try_new(
                    us.clone(),
                    vec![ChartSection::new("Cached top songs", vec![cached_item])],
                    cached_at,
                    cached_at + 3_600,
                )
                .unwrap_or_else(|error| panic!("valid cached chart: {}", error.message())),
            )),
        },
    );
    assert!(completion_effects.is_empty());
    assert!(state.charts().loading());
    let (state, completion_effects) = reduce(
        state,
        Action::ChartsCompleted {
            generation,
            region: us,
            received_at: cached_at + 3_601,
            result: Err(ytermusic::app::AppError::new(
                ytermusic::app::AppErrorCategory::Charts,
                "offline",
            )),
        },
    );
    assert!(completion_effects.is_empty());
    assert!(state.charts().stale());
    assert_eq!(state.charts().cached_at(), Some(cached_at));
    assert!(state.charts().error().is_none());

    let backend = TestBackend::new(140, 40);
    let mut terminal = Terminal::new(backend)?;
    terminal.draw(|frame| {
        render_with_model(
            frame,
            &state,
            &Theme::default(),
            &RenderModel::default().with_view(NavigationItem::Charts),
        );
    })?;
    let rendered = buffer_text(terminal.backend().buffer());
    assert!(rendered.contains("STALE"));
    assert!(rendered.contains("Cached US hit"));

    Ok(())
}

#[test]
fn dependency_failure_keeps_browsing_usable_and_exposes_repair_action() -> Result<(), Box<dyn Error>>
{
    let report = DoctorReport::new(vec![
        DiagnosticRow::new(
            "browsing",
            DiagnosticStatus::Healthy,
            "metadata browsing available",
        ),
        DiagnosticRow::new(
            "mpv",
            DiagnosticStatus::Unhealthy,
            "not found | brew install mpv",
        ),
        DiagnosticRow::new(
            "playback",
            DiagnosticStatus::Unhealthy,
            "unavailable; browsing still works",
        ),
    ]);
    let (state, report_effects) =
        reduce(AppState::default(), Action::DependencyReportLoaded(report));
    assert!(report_effects.is_empty());
    assert!(state.dependencies().browsing_available());
    assert!(!state.dependencies().playback_available());

    let item = song("browse-only", "Still searchable", "Metadata Artist");
    let (state, search_effects) = reduce(
        state,
        Action::SearchSubmitted {
            query: "still works".to_owned(),
            filter: SearchFilter::Songs,
        },
    );
    let [Effect::Search { generation, .. }] = search_effects.as_slice() else {
        panic!("dependency failure must not block browsing/search");
    };
    let (state, _) = reduce(
        state,
        Action::SearchCompleted {
            generation: *generation,
            result: Ok(SearchPage::new(vec![SearchItem::Playable(item)])),
        },
    );
    let (state, playback_effects) = reduce(state, Action::ActivateSearchResult { index: 0 });
    assert!(
        playback_effects
            .iter()
            .all(|effect| !matches!(effect, Effect::Resolve { .. })),
        "unavailable playback dependencies must block only resolution"
    );
    assert_eq!(state.playback().status, PlaybackStatus::Failed);
    assert_eq!(state.queue().items().len(), 1);

    let backend = TestBackend::new(140, 40);
    let mut terminal = Terminal::new(backend)?;
    terminal.draw(|frame| {
        render_with_model(
            frame,
            &state,
            &Theme::default(),
            &RenderModel::default().with_view(NavigationItem::Settings),
        );
    })?;
    let rendered = buffer_text(terminal.backend().buffer());
    assert!(rendered.contains("Browsing available"));
    assert!(rendered.contains("Fade in/out: 250 ms / 250 ms"));
    assert!(rendered.contains("brew install mpv"));
    assert!(rendered.contains("[d] Recheck dependencies"));

    let (_, repair_effects) = reduce(state, Action::DependencyCheckRequested);
    assert_eq!(repair_effects, vec![Effect::CheckDependencies]);

    Ok(())
}

#[test]
fn search_selection_and_continuation_use_stable_ids_and_reject_stale_pages() {
    let first = song("page-a", "Page A", "Artist A");
    let selected = song("page-b", "Page B", "Artist B");
    let more = song("page-c", "Page C", "Artist C");
    let (state, effects) = reduce(
        AppState::default(),
        Action::SearchSubmitted {
            query: "pages".to_owned(),
            filter: SearchFilter::Songs,
        },
    );
    let [Effect::Search { generation, .. }] = effects.as_slice() else {
        panic!("initial search effect");
    };
    let (state, _) = reduce(
        state,
        Action::SearchCompleted {
            generation: *generation,
            result: Ok(SearchPage::new(vec![
                SearchItem::Playable(first.clone()),
                SearchItem::Playable(selected.clone()),
            ])
            .with_continuation("opaque-search-page")),
        },
    );
    let selected_id = SearchItemId::Media(selected.id.clone());
    let (state, selection_effects) = reduce(
        state,
        Action::SearchSelectionChanged {
            id: selected_id.clone(),
        },
    );
    assert!(selection_effects.is_empty());
    assert_eq!(state.search().selected_id(), Some(&selected_id));

    let (state, more_effects) = reduce(state, Action::SearchMoreRequested);
    let [
        Effect::SearchMore {
            generation: more_generation,
            continuation,
            ..
        },
    ] = more_effects.as_slice()
    else {
        panic!("continuation must issue one search-more effect");
    };
    assert_eq!(continuation.as_str(), "opaque-search-page");
    assert!(!format!("{continuation:?}").contains("opaque-search-page"));
    let stale_generation = *more_generation;

    let (state, superseding_effects) = reduce(
        state,
        Action::SearchSubmitted {
            query: "pages refreshed".to_owned(),
            filter: SearchFilter::Songs,
        },
    );
    let [
        Effect::Search {
            generation: refreshed_generation,
            ..
        },
    ] = superseding_effects.as_slice()
    else {
        panic!("refresh effect");
    };
    let expected = state.clone();
    let (state, stale_effects) = reduce(
        state,
        Action::SearchCompleted {
            generation: stale_generation,
            result: Ok(SearchPage::new(vec![SearchItem::Playable(more.clone())])),
        },
    );
    assert!(stale_effects.is_empty());
    assert_eq!(state, expected);

    let (state, _) = reduce(
        state,
        Action::SearchCompleted {
            generation: *refreshed_generation,
            result: Ok(SearchPage::new(vec![
                SearchItem::Playable(more),
                SearchItem::Playable(selected),
            ])),
        },
    );
    assert_eq!(state.search().selected_id(), Some(&selected_id));
}

#[test]
fn provider_podcast_search_mapping_retains_browse_id_and_page_provenance() {
    let provider_page = Page {
        items: vec![ytermusic::provider::SearchItem::Podcast(BrowseItem {
            id: "podcast-stable-id".to_owned(),
            title: "Mapped Podcast".to_owned(),
            subtitle: Some("Mapped creator".to_owned()),
            artwork_url: None,
        })],
        continuation: Some("provider-next".to_owned()),
        stale: true,
    };

    let page = SearchPage::from_provider(provider_page);

    assert_eq!(
        page.continuation()
            .map(ytermusic::app::OpaqueContinuation::as_str),
        Some("provider-next")
    );
    assert!(page.stale());
    let [SearchItem::Metadata(metadata)] = page.items() else {
        panic!("podcast browse result must map to metadata");
    };
    assert_eq!(metadata.kind(), SearchMetadataKind::Podcast);
    assert_eq!(metadata.provider_id(), Some("podcast-stable-id"));
}

#[test]
fn chart_refresh_retains_selected_media_id_when_order_changes() {
    let first = song("chart-first", "First", "Artist");
    let selected = song("chart-selected", "Selected", "Artist");
    let us = region("us");
    let (state, effects) = reduce(
        AppState::default(),
        Action::ChartsRequested { region: us.clone() },
    );
    let (generation, _) = chart_request(&effects);
    let (state, _) = reduce(
        state,
        Action::ChartsCompleted {
            generation,
            region: us.clone(),
            received_at: 1_000,
            result: Ok(vec![ChartSection::new(
                "Top",
                vec![first.clone(), selected.clone()],
            )]),
        },
    );
    let (state, selection_effects) = reduce(
        state,
        Action::ChartSelectionChanged {
            media_id: selected.id.clone(),
        },
    );
    assert!(selection_effects.is_empty());
    assert_eq!(state.charts().selected_id(), Some(&selected.id));

    let (state, effects) = reduce(state, Action::ChartsRequested { region: us.clone() });
    let (generation, _) = chart_request(&effects);
    let (state, _) = reduce(
        state,
        Action::ChartsCompleted {
            generation,
            region: us,
            received_at: 2_000,
            result: Ok(vec![ChartSection::new("Top", vec![selected, first])]),
        },
    );
    assert_eq!(
        state.charts().selected_id().map(|id| id.video_id.as_str()),
        Some("chart-selected")
    );
}

#[test]
fn library_continuation_deduplicates_and_retains_selected_stable_id() {
    let first = LibraryItem::Playlist(BrowseItem {
        id: "library-first".to_owned(),
        title: "First playlist".to_owned(),
        subtitle: None,
        artwork_url: None,
    });
    let second = LibraryItem::Playlist(BrowseItem {
        id: "library-second".to_owned(),
        title: "Second playlist".to_owned(),
        subtitle: None,
        artwork_url: None,
    });
    let (state, _) = reduce(
        AppState::default(),
        Action::AuthenticationChanged(AuthenticationState::Authenticated),
    );
    let (state, effects) = reduce(
        state,
        Action::LibraryRequested {
            section: LibrarySection::Playlists,
        },
    );
    let [Effect::LoadLibrary { generation, .. }] = effects.as_slice() else {
        panic!("initial library request");
    };
    let (state, _) = reduce(
        state,
        Action::LibraryCompleted {
            generation: *generation,
            result: Ok(Page {
                items: vec![first.clone()],
                continuation: Some("library-more".to_owned()),
                stale: false,
            }),
        },
    );
    let selected_id = stable_library_item_id(&first);
    let (state, _) = reduce(
        state,
        Action::LibrarySelectionChanged {
            id: selected_id.clone(),
        },
    );

    let (state, effects) = reduce(state, Action::LibraryMoreRequested);
    let [
        Effect::LoadLibrary {
            generation,
            continuation: Some(continuation),
            ..
        },
    ] = effects.as_slice()
    else {
        panic!("library continuation request");
    };
    assert_eq!(continuation.as_str(), "library-more");
    let (state, _) = reduce(
        state,
        Action::LibraryCompleted {
            generation: *generation,
            result: Ok(Page {
                items: vec![first, second],
                continuation: None,
                stale: false,
            }),
        },
    );
    assert_eq!(state.library().items().len(), 2);
    assert_eq!(state.library().selected_id(), Some(&selected_id));
}

#[test]
fn discovery_and_library_pages_have_bounded_accumulation() {
    let search_items = (0..MAX_VIEW_ITEMS + 10)
        .map(|index| {
            SearchItem::Playable(song(
                &format!("bounded-search-{index}"),
                &format!("Bounded Search {index}"),
                "Bounded Artist",
            ))
        })
        .collect();
    let (state, effects) = reduce(
        AppState::default(),
        Action::SearchSubmitted {
            query: "bounded".to_owned(),
            filter: SearchFilter::Songs,
        },
    );
    let [Effect::Search { generation, .. }] = effects.as_slice() else {
        panic!("search effect");
    };
    let (state, _) = reduce(
        state,
        Action::SearchCompleted {
            generation: *generation,
            result: Ok(SearchPage::new(search_items).with_continuation("more-search")),
        },
    );
    assert_eq!(state.search().items().len(), MAX_VIEW_ITEMS);
    assert!(
        state.search().continuation().is_none(),
        "a saturated in-memory view must not keep requesting more pages"
    );

    let (state, _) = reduce(
        state,
        Action::AuthenticationChanged(AuthenticationState::Authenticated),
    );
    let (state, effects) = reduce(
        state,
        Action::LibraryRequested {
            section: LibrarySection::Playlists,
        },
    );
    let [Effect::LoadLibrary { generation, .. }] = effects.as_slice() else {
        panic!("library effect");
    };
    let library_items = (0..MAX_VIEW_ITEMS + 10)
        .map(|index| {
            LibraryItem::Playlist(BrowseItem {
                id: format!("bounded-library-{index}"),
                title: format!("Bounded Library {index}"),
                subtitle: None,
                artwork_url: None,
            })
        })
        .collect();
    let (state, _) = reduce(
        state,
        Action::LibraryCompleted {
            generation: *generation,
            result: Ok(Page {
                items: library_items,
                continuation: Some("more-library".to_owned()),
                stale: false,
            }),
        },
    );
    assert_eq!(state.library().items().len(), MAX_VIEW_ITEMS);
    assert!(state.library().continuation().is_none());
}

#[test]
fn chart_and_podcast_payloads_are_capped_at_app_boundary() {
    let region = region("us");
    let (state, effects) = reduce(
        AppState::default(),
        Action::ChartsRequested {
            region: region.clone(),
        },
    );
    let (generation, _) = chart_request(&effects);
    let oversized_chart_items = (0..=MAX_ITEMS_PER_SHELF)
        .map(|index| {
            song(
                &format!("bounded-chart-{index}"),
                &format!("Bounded Chart {index}"),
                "Chart Artist",
            )
        })
        .collect();
    let mut sections = vec![ChartSection::new("Oversized", oversized_chart_items)];
    sections.extend(
        (1..=MAX_SECTIONS).map(|index| ChartSection::new(format!("Section {index}"), Vec::new())),
    );
    let (state, _) = reduce(
        state,
        Action::ChartsCompleted {
            generation,
            region,
            received_at: 1_000,
            result: Ok(sections),
        },
    );
    assert_eq!(state.charts().sections().len(), MAX_SECTIONS);
    assert_eq!(
        state.charts().sections()[0].items().len(),
        MAX_ITEMS_PER_SHELF
    );

    let (state, effects) = reduce(
        state,
        Action::SearchSubmitted {
            query: "bounded podcast".to_owned(),
            filter: SearchFilter::Podcasts,
        },
    );
    let [Effect::Search { generation, .. }] = effects.as_slice() else {
        panic!("podcast search effect");
    };
    let metadata = SearchMetadata::new(SearchMetadataKind::Podcast, "Bounded Podcast")
        .with_provider_id("bounded-podcast");
    let (state, _) = reduce(
        state,
        Action::SearchCompleted {
            generation: *generation,
            result: Ok(SearchPage::new(vec![SearchItem::Metadata(metadata)])),
        },
    );
    let (state, effects) = reduce(state, Action::OpenSelectedPodcast);
    let [Effect::LoadPodcast { generation, .. }] = effects.as_slice() else {
        panic!("podcast load effect");
    };
    let episodes = (0..=MAX_VIEW_ITEMS)
        .map(|index| MediaItem {
            kind: MediaKind::PodcastEpisode,
            ..song(
                &format!("bounded-episode-{index}"),
                &format!("Bounded Episode {index}"),
                "Podcast Creator",
            )
        })
        .collect();
    let (state, _) = reduce(
        state,
        Action::PodcastCompleted {
            generation: *generation,
            result: Ok(Podcast {
                id: "bounded-podcast".to_owned(),
                title: "Bounded Podcast".to_owned(),
                creators: vec!["Podcast Creator".to_owned()],
                description: None,
                artwork_url: None,
                episodes,
            }),
        },
    );
    assert_eq!(
        state.podcasts().show().map(|show| show.episodes.len()),
        Some(MAX_VIEW_ITEMS)
    );
}

#[test]
fn playing_records_history_once_and_home_history_views_use_stable_entry_ids()
-> Result<(), Box<dyn Error>> {
    let item = song("history-song", "History Song", "Remembered Artist");
    let (state, effects) = reduce(
        AppState::default(),
        Action::SearchSubmitted {
            query: "history".to_owned(),
            filter: SearchFilter::Songs,
        },
    );
    let [Effect::Search { generation, .. }] = effects.as_slice() else {
        panic!("search effect");
    };
    let (state, _) = reduce(
        state,
        Action::SearchCompleted {
            generation: *generation,
            result: Ok(SearchPage::new(vec![SearchItem::Playable(item.clone())])),
        },
    );
    let (state, _) = reduce(state, Action::ActivateSearchResult { index: 0 });
    let playback_generation = state
        .current_attempt_generation()
        .unwrap_or_else(|| panic!("active playback attempt"));
    let (state, _) = reduce(
        state,
        Action::ResolveSucceeded {
            generation: playback_generation,
        },
    );
    let (state, history_effects) = reduce(
        state,
        Action::PlayerStatusChanged {
            generation: playback_generation,
            status: PlaybackStatus::Playing,
        },
    );
    assert_eq!(
        history_effects,
        vec![
            Effect::RecordHistory { item: item.clone() },
            Effect::Notify(NowPlayingNotification::from_media(
                playback_generation,
                &item,
            )),
        ]
    );
    let (state, duplicate_effects) = reduce(
        state,
        Action::PlayerStatusChanged {
            generation: playback_generation,
            status: PlaybackStatus::Playing,
        },
    );
    assert!(duplicate_effects.is_empty());
    let state = assert_notification_dedupe_after_first_playing(state, playback_generation);

    let (state, load_effects) = reduce(state, Action::HistoryRequested);
    let [Effect::LoadHistory { generation, limit }] = load_effects.as_slice() else {
        panic!("history view must load recent entries");
    };
    assert!(*limit > 0);
    let entry = HistoryEntry {
        id: 77,
        item: item.clone(),
        played_at: 1_700_000_100,
    };
    let older = HistoryEntry {
        id: 76,
        item,
        played_at: 1_700_000_000,
    };
    let (state, completion_effects) = reduce(
        state,
        Action::HistoryCompleted {
            generation: *generation,
            result: Ok(vec![entry.clone(), older]),
        },
    );
    assert!(completion_effects.is_empty());
    assert_eq!(state.history().entries().first(), Some(&entry));
    assert_eq!(state.history().selected_id(), Some(77));
    let (state, selection_effects) = reduce(state, Action::HistorySelectionChanged { id: 76 });
    assert!(selection_effects.is_empty());
    assert_eq!(state.history().selected_id(), Some(76));

    for (view, expected) in [
        (NavigationItem::Home, "Continue listening"),
        (NavigationItem::History, "History Song"),
    ] {
        let backend = TestBackend::new(140, 40);
        let mut terminal = Terminal::new(backend)?;
        terminal.draw(|frame| {
            render_with_model(
                frame,
                &state,
                &Theme::default(),
                &RenderModel::default().with_view(view),
            );
        })?;
        assert!(buffer_text(terminal.backend().buffer()).contains(expected));
    }

    Ok(())
}

fn assert_notification_dedupe_after_first_playing(
    mut state: AppState,
    generation: Generation,
) -> AppState {
    for status in [
        PlaybackStatus::Paused,
        PlaybackStatus::Playing,
        PlaybackStatus::Buffering,
        PlaybackStatus::Playing,
    ] {
        let (next, effects) = reduce(state, Action::PlayerStatusChanged { generation, status });
        assert!(effects.is_empty());
        state = next;
    }
    let prior_status = state.playback().status;
    let (state, effects) = reduce(
        state,
        Action::PlayerStatusChanged {
            generation: Generation::new(generation.value().saturating_add(99)),
            status: PlaybackStatus::Playing,
        },
    );
    assert!(effects.is_empty());
    assert_eq!(state.playback().status, prior_status);
    state
}

#[test]
fn disabled_notifications_still_record_history_without_notifying() {
    let item = song("quiet-song", "Quiet Song", "Quiet Artist");
    let mut config = Config::default();
    config.notifications.enabled = false;
    let (state, _) = reduce(
        AppState::new(config),
        Action::EnqueueMedia { item: item.clone() },
    );
    let (state, _) = reduce(
        state,
        Action::PlayQueueItem {
            id: stable_queue_item_id(&item.id),
        },
    );
    let generation = state
        .current_attempt_generation()
        .unwrap_or_else(|| panic!("active playback attempt"));
    let (state, _) = reduce(state, Action::ResolveSucceeded { generation });
    let (_, effects) = reduce(
        state,
        Action::PlayerStatusChanged {
            generation,
            status: PlaybackStatus::Playing,
        },
    );

    assert_eq!(effects, vec![Effect::RecordHistory { item }]);
}
