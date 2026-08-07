use url::Url;
use ytermusic::{
    app::{
        Action, AppError, AppErrorCategory, AppState, ArtworkSurface, ChartCachePayload,
        ChartSection, DiagnosticCategory, Effect, FavoriteMutation, Generation, PlayerCommand,
        PodcastProviderId, SearchFilter, SearchItem, SearchMetadata, SearchMetadataKind,
        SearchPage, SessionCheckpoint, reduce, stable_queue_item_id,
    },
    config::Config,
    domain::{ArtworkUrl, MediaId, MediaItem, MediaKind, PlaybackStatus, RegionCode, RepeatMode},
    lyrics::{LyricsDocument, LyricsSource, TimedLyricLine},
    notifications::NowPlayingNotification,
    podcast_rankings::{
        MAX_PODCAST_RECOMMENDATIONS, PodcastRecommendationPage, parse_apple_top_shows,
    },
    provider::{
        AuthenticationState, BrowseItem, ChartCacheKey, LibraryItem, LibrarySection, Page, Podcast,
    },
    queue::{MAX_EXPLICIT_LIST_ITEMS, QueueItem},
    resolver::{AnalysisStreamUrl, PreviewStreamUrl, ResolvedStream},
    storage::{FAVORITES_LIMIT, FavoriteEntry, HistoryEntry, PodcastProgress},
};

fn media(provider: &str, video_id: &str) -> MediaItem {
    MediaItem {
        id: MediaId {
            provider: provider.to_owned(),
            video_id: video_id.to_owned(),
        },
        kind: MediaKind::Song,
        title: format!("Song {video_id}"),
        creators: vec!["Artist".to_owned()],
        collection: None,
        duration_ms: Some(180_000),
        artwork_url: None,
        explicit: false,
    }
}

fn favorite(item: MediaItem, id: i64, favorited_at: i64) -> FavoriteEntry {
    FavoriteEntry {
        id,
        item,
        favorited_at,
    }
}

#[test]
fn favorites_request_loads_and_preserves_stable_selection() {
    let first = favorite(media("youtube-music", "first"), 1, 20);
    let second = favorite(media("youtube-music", "second"), 2, 10);
    let (state, effects) = reduce(AppState::default(), Action::FavoritesRequested);
    let [Effect::LoadFavorites { generation }] = effects.as_slice() else {
        panic!("favorites load effect");
    };
    assert!(state.favorites().loading());
    assert!(!state.favorites().loaded());

    let (state, _) = reduce(
        state,
        Action::FavoritesCompleted {
            generation: *generation,
            result: Ok(vec![first.clone(), second.clone()]),
        },
    );
    let (state, _) = reduce(
        state,
        Action::FavoriteSelectionChanged {
            media_id: second.item.id.clone(),
        },
    );
    let (state, effects) = reduce(state, Action::FavoritesRequested);
    let [Effect::LoadFavorites { generation }] = effects.as_slice() else {
        panic!("favorites reload effect");
    };
    let (state, _) = reduce(
        state,
        Action::FavoritesCompleted {
            generation: *generation,
            result: Ok(vec![second.clone(), first]),
        },
    );
    assert_eq!(state.favorites().selected_id(), Some(&second.item.id));
    assert!(state.favorites().loaded());
    assert!(!state.favorites().loading());
}

#[test]
fn background_favorites_completion_does_not_claim_home_artwork() {
    let mut item = media("youtube-music", "background-favorite-art");
    item.artwork_url = Some(url("https://images.example.test/background-favorite.jpg"));
    let (state, effects) = reduce(AppState::default(), Action::FavoritesRequested);
    let [Effect::LoadFavorites { generation }] = effects.as_slice() else {
        panic!("favorites load effect");
    };

    let (state, effects) = reduce(
        state,
        Action::FavoritesCompleted {
            generation: *generation,
            result: Ok(vec![favorite(item, 1, 1)]),
        },
    );

    assert!(
        effects.is_empty(),
        "background completion effects: {effects:?}"
    );
    assert!(state.artwork().requested_url().is_none());
}

#[test]
fn late_search_completion_does_not_replace_favorites_artwork() {
    let mut favorite_item = media("youtube-music", "owned-favorite-art");
    let favorite_url = artwork_url("https://images.example.test/owned-favorite.jpg");
    favorite_item.artwork_url = Some(favorite_url.as_url().clone());
    let mut search_item = media("youtube-music", "late-search-art");
    let search_url = artwork_url("https://images.example.test/late-search.jpg");
    search_item.artwork_url = Some(search_url.as_url().clone());

    let (state, effects) = reduce(
        AppState::default(),
        Action::SearchSubmitted {
            query: "late search".to_owned(),
            filter: SearchFilter::Songs,
        },
    );
    let [
        Effect::Search {
            generation: search_generation,
            ..
        },
    ] = effects.as_slice()
    else {
        panic!("search load effect");
    };
    let search_generation = *search_generation;
    let (state, effects) = reduce(state, Action::FavoritesRequested);
    let [
        Effect::LoadFavorites {
            generation: favorites_generation,
        },
    ] = effects.as_slice()
    else {
        panic!("favorites load effect");
    };
    let (state, _) = reduce(
        state,
        Action::FavoritesCompleted {
            generation: *favorites_generation,
            result: Ok(vec![favorite(favorite_item, 1, 1)]),
        },
    );
    let (state, effects) = reduce(
        state,
        Action::ArtworkSurfaceChanged {
            surface: ArtworkSurface::Favorites,
        },
    );
    assert!(fetched_artwork(&effects, &favorite_url));

    let (state, effects) = reduce(
        state,
        Action::SearchCompleted {
            generation: search_generation,
            result: Ok(SearchPage::new(vec![SearchItem::Playable(search_item)])),
        },
    );

    assert!(effects.is_empty(), "late search effects: {effects:?}");
    assert_eq!(state.artwork().requested_url(), Some(&favorite_url));
}

#[test]
fn background_favorite_mutation_completion_preserves_visible_surface_artwork() {
    let mut favorite_item = media("youtube-music", "background-mutation-favorite");
    favorite_item.artwork_url = Some(url(
        "https://images.example.test/background-mutation-favorite.jpg",
    ));
    let state = loaded_favorites(vec![favorite(favorite_item.clone(), 1, 1)]);
    let visible = artwork_url("https://images.example.test/visible-search.jpg");
    let (state, _) = reduce(
        state,
        Action::ArtworkRequested {
            url: visible.clone(),
        },
    );
    let (state, effects) = reduce(
        state,
        Action::FavoriteToggleRequested {
            item: favorite_item.clone(),
        },
    );
    let [Effect::RemoveFavorite { generation, .. }] = effects.as_slice() else {
        panic!("remove favorite effect");
    };

    let (state, effects) = reduce(
        state,
        Action::FavoriteMutationCompleted {
            generation: *generation,
            media_id: favorite_item.id,
            mutation: FavoriteMutation::Remove,
            result: Ok(Vec::new()),
        },
    );

    assert!(
        effects.is_empty(),
        "background mutation effects: {effects:?}"
    );
    assert_eq!(state.artwork().requested_url(), Some(&visible));
}

fn loaded_favorites(entries: Vec<FavoriteEntry>) -> AppState {
    let (state, effects) = reduce(AppState::default(), Action::FavoritesRequested);
    let [Effect::LoadFavorites { generation }] = effects.as_slice() else {
        panic!("favorites load effect");
    };
    reduce(
        state,
        Action::FavoritesCompleted {
            generation: *generation,
            result: Ok(entries),
        },
    )
    .0
}

#[test]
fn favorites_add_and_remove_complete_without_optimistic_mutation() {
    let item = media("youtube-music", "toggle");
    let state = loaded_favorites(Vec::new());
    let (state, effects) = reduce(
        state,
        Action::FavoriteToggleRequested { item: item.clone() },
    );
    let [Effect::AddFavorite { generation, .. }] = effects.as_slice() else {
        panic!("add favorite effect");
    };
    assert!(state.favorites().entries().is_empty());
    assert_eq!(
        state
            .favorites()
            .pending_mutation()
            .map(ytermusic::app::PendingFavoriteMutation::mutation),
        Some(FavoriteMutation::Add)
    );
    let added = favorite(item.clone(), 9, 100);
    let (state, _) = reduce(
        state,
        Action::FavoriteMutationCompleted {
            generation: *generation,
            media_id: item.id.clone(),
            mutation: FavoriteMutation::Add,
            result: Ok(vec![added]),
        },
    );
    assert_eq!(state.favorites().entries().len(), 1);

    let (state, effects) = reduce(
        state,
        Action::FavoriteToggleRequested { item: item.clone() },
    );
    let [Effect::RemoveFavorite { generation, .. }] = effects.as_slice() else {
        panic!("remove favorite effect");
    };
    assert_eq!(state.favorites().entries().len(), 1);
    let (state, _) = reduce(
        state,
        Action::FavoriteMutationCompleted {
            generation: *generation,
            media_id: item.id,
            mutation: FavoriteMutation::Remove,
            result: Ok(Vec::new()),
        },
    );
    assert!(state.favorites().entries().is_empty());
    assert!(state.favorites().selected_id().is_none());
    assert!(state.favorites().pending_mutation().is_none());
}

#[test]
fn removing_middle_favorite_selects_the_row_now_at_the_same_index() {
    let entries = (0..5)
        .map(|index| {
            favorite(
                media("youtube-music", &format!("middle-{index}")),
                i64::from(index),
                i64::from(5 - index),
            )
        })
        .collect::<Vec<_>>();
    let removed = entries[2].item.clone();
    let expected = entries[3].item.id.clone();
    let state = loaded_favorites(entries.clone());
    let remaining = entries
        .into_iter()
        .filter(|entry| entry.item.id != removed.id)
        .collect::<Vec<_>>();
    let (state, _) = reduce(
        state,
        Action::FavoriteSelectionChanged {
            media_id: removed.id.clone(),
        },
    );
    let (state, effects) = reduce(
        state,
        Action::FavoriteToggleRequested {
            item: removed.clone(),
        },
    );
    let [Effect::RemoveFavorite { generation, .. }] = effects.as_slice() else {
        panic!("remove favorite effect");
    };
    let (state, _) = reduce(
        state,
        Action::FavoriteMutationCompleted {
            generation: *generation,
            media_id: removed.id,
            mutation: FavoriteMutation::Remove,
            result: Ok(remaining),
        },
    );

    assert_eq!(state.favorites().selected_id(), Some(&expected));
}

#[test]
fn removing_final_favorite_selects_the_previous_final_row() {
    let entries = (0..5)
        .map(|index| {
            favorite(
                media("youtube-music", &format!("final-{index}")),
                i64::from(index),
                i64::from(5 - index),
            )
        })
        .collect::<Vec<_>>();
    let removed = entries[4].item.clone();
    let expected = entries[3].item.id.clone();
    let remaining = entries[..4].to_vec();
    let state = loaded_favorites(entries);
    let (state, _) = reduce(
        state,
        Action::FavoriteSelectionChanged {
            media_id: removed.id.clone(),
        },
    );
    let (state, effects) = reduce(
        state,
        Action::FavoriteToggleRequested {
            item: removed.clone(),
        },
    );
    let [Effect::RemoveFavorite { generation, .. }] = effects.as_slice() else {
        panic!("remove favorite effect");
    };
    let (state, _) = reduce(
        state,
        Action::FavoriteMutationCompleted {
            generation: *generation,
            media_id: removed.id,
            mutation: FavoriteMutation::Remove,
            result: Ok(remaining),
        },
    );

    assert_eq!(state.favorites().selected_id(), Some(&expected));
}

#[test]
fn favorites_reject_stale_completions_and_repeated_pending_toggle() {
    let item = media("youtube-music", "stable");
    let state = loaded_favorites(Vec::new());
    let (state, effects) = reduce(
        state,
        Action::FavoriteToggleRequested { item: item.clone() },
    );
    let [Effect::AddFavorite { generation, .. }] = effects.as_slice() else {
        panic!("add favorite effect");
    };
    let (state, repeated_effects) = reduce(
        state,
        Action::FavoriteToggleRequested { item: item.clone() },
    );
    assert!(repeated_effects.is_empty());
    let pending = state.favorites().pending_mutation().cloned();
    let (state, stale_effects) = reduce(
        state,
        Action::FavoriteMutationCompleted {
            generation: Generation::new(generation.value().saturating_sub(1)),
            media_id: item.id,
            mutation: FavoriteMutation::Add,
            result: Ok(Vec::new()),
        },
    );
    assert!(stale_effects.is_empty());
    assert_eq!(state.favorites().pending_mutation(), pending.as_ref());
}

#[test]
fn favorites_reject_same_generation_completion_for_wrong_identity_or_direction() {
    let item = media("youtube-music", "pending-match");
    let state = loaded_favorites(Vec::new());
    let (state, effects) = reduce(
        state,
        Action::FavoriteToggleRequested { item: item.clone() },
    );
    let [Effect::AddFavorite { generation, .. }] = effects.as_slice() else {
        panic!("add favorite effect");
    };
    let pending = state.favorites().pending_mutation().cloned();

    for (media_id, mutation) in [
        (
            MediaId {
                provider: item.id.provider.clone(),
                video_id: "wrong-id".to_owned(),
            },
            FavoriteMutation::Add,
        ),
        (item.id.clone(), FavoriteMutation::Remove),
    ] {
        let (next, effects) = reduce(
            state.clone(),
            Action::FavoriteMutationCompleted {
                generation: *generation,
                media_id,
                mutation,
                result: Ok(vec![favorite(item.clone(), 1, 1)]),
            },
        );
        assert!(effects.is_empty());
        assert_eq!(next.favorites().entries(), state.favorites().entries());
        assert_eq!(next.favorites().pending_mutation(), pending.as_ref());
    }
}

#[test]
fn matching_favorite_mutation_failure_preserves_entries_and_clears_pending() {
    let canonical = favorite(media("youtube-music", "canonical"), 1, 10);
    let added = media("youtube-music", "failed-add");
    let state = loaded_favorites(vec![canonical.clone()]);
    let (state, effects) = reduce(
        state,
        Action::FavoriteToggleRequested {
            item: added.clone(),
        },
    );
    let [Effect::AddFavorite { generation, .. }] = effects.as_slice() else {
        panic!("add favorite effect");
    };
    let error = AppError::new(AppErrorCategory::Favorites, "favorite could not be added");
    let (state, _) = reduce(
        state,
        Action::FavoriteMutationCompleted {
            generation: *generation,
            media_id: added.id,
            mutation: FavoriteMutation::Add,
            result: Err(error.clone()),
        },
    );
    assert_eq!(
        state.favorites().entries(),
        std::slice::from_ref(&canonical)
    );
    assert_eq!(state.favorites().error(), Some(&error));
    assert!(state.favorites().pending_mutation().is_none());

    let removed = canonical.item.clone();
    let (state, effects) = reduce(
        state,
        Action::FavoriteToggleRequested {
            item: removed.clone(),
        },
    );
    let [Effect::RemoveFavorite { generation, .. }] = effects.as_slice() else {
        panic!("remove favorite effect");
    };
    let error = AppError::new(AppErrorCategory::Favorites, "favorite could not be removed");
    let (state, _) = reduce(
        state,
        Action::FavoriteMutationCompleted {
            generation: *generation,
            media_id: removed.id,
            mutation: FavoriteMutation::Remove,
            result: Err(error.clone()),
        },
    );
    assert_eq!(state.favorites().entries(), &[canonical]);
    assert_eq!(state.favorites().error(), Some(&error));
    assert!(state.favorites().pending_mutation().is_none());
}

#[test]
fn favorites_cap_visible_entries_and_display_storage_errors() {
    let entries = (0..=FAVORITES_LIMIT)
        .map(|index| {
            let stable_index = i64::try_from(index).unwrap_or_else(|_| panic!("fixture index"));
            favorite(
                media("youtube-music", &format!("fav-{index}")),
                stable_index,
                -stable_index,
            )
        })
        .collect();
    let state = loaded_favorites(entries);
    assert_eq!(state.favorites().entries().len(), FAVORITES_LIMIT);
    let preserved = state.favorites().entries().to_vec();

    let (state, effects) = reduce(state, Action::FavoritesRequested);
    let [Effect::LoadFavorites { generation }] = effects.as_slice() else {
        panic!("favorites reload effect");
    };
    let error = AppError::new(AppErrorCategory::Favorites, "favorites could not be loaded");
    let (state, _) = reduce(
        state,
        Action::FavoritesCompleted {
            generation: *generation,
            result: Err(error.clone()),
        },
    );
    assert_eq!(state.favorites().entries(), preserved);
    assert_eq!(state.favorites().error(), Some(&error));
}

#[test]
fn favorites_removing_playing_item_leaves_playback_and_queue_unchanged() {
    let item = media("youtube-music", "playing-favorite");
    let state = loaded_favorites(vec![favorite(item.clone(), 1, 1)]);
    let (state, _) = reduce(state, Action::EnqueueMedia { item: item.clone() });
    let queue_id = stable_queue_item_id(&item.id);
    let (state, _) = reduce(state, Action::PlayQueueItem { id: queue_id });
    let queue_before = state.queue().snapshot();
    let playback_before = state.playback().clone();
    let (state, effects) = reduce(
        state,
        Action::FavoriteToggleRequested { item: item.clone() },
    );
    let [Effect::RemoveFavorite { generation, .. }] = effects.as_slice() else {
        panic!("remove favorite effect");
    };
    let (state, completion_effects) = reduce(
        state,
        Action::FavoriteMutationCompleted {
            generation: *generation,
            media_id: item.id,
            mutation: FavoriteMutation::Remove,
            result: Ok(Vec::new()),
        },
    );
    assert!(completion_effects.is_empty());
    assert_eq!(state.queue().snapshot(), queue_before);
    assert_eq!(state.playback(), &playback_before);
}

#[test]
fn play_media_list_replaces_queue_atomically_and_starts_selected_item() {
    let old = media("youtube-music", "old");
    let first = media("youtube-music", "first");
    let selected = media("youtube-music", "selected");
    let last = media("youtube-music", "last");
    let state = state_with_results(
        config_without_lyrics(),
        vec![SearchItem::Playable(old.clone())],
    );
    let (state, _) = reduce(state, Action::ActivateSearchResult { index: 0 });
    let (state, _) = reduce(state, Action::RepeatModeChanged(RepeatMode::All));
    let (state, _) = reduce(state, Action::RadioEnabledChanged(true));

    let (state, effects) = reduce(
        state,
        Action::PlayMediaList {
            items: vec![first.clone(), selected.clone(), first.clone(), last.clone()],
            selected_id: selected.id.clone(),
            shuffle_seed: None,
        },
    );

    assert_eq!(
        state
            .queue()
            .items()
            .iter()
            .map(QueueItem::media)
            .collect::<Vec<_>>(),
        vec![&first, &selected, &last]
    );
    assert_eq!(
        state.queue().current().map(QueueItem::media),
        Some(&selected)
    );
    assert_eq!(state.queue().repeat(), RepeatMode::All);
    assert!(!state.queue().is_shuffled());
    assert!(!state.queue().radio_enabled());
    assert!(state.pending_radio_generation().is_none());
    assert_eq!(state.playback().current.as_ref(), Some(&selected.id));
    assert_eq!(state.playback().status, PlaybackStatus::Resolving);
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, Effect::Resolve { item, .. } if item == &selected))
    );
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, Effect::Persist(_)))
    );
    assert!(
        effects
            .iter()
            .all(|effect| !matches!(effect, Effect::FillRadio { .. }))
    );
}

#[test]
fn play_media_list_shuffle_keeps_selected_item_current() {
    let first = media("youtube-music", "shuffle-first");
    let selected = media("youtube-music", "shuffle-selected");
    let last = media("youtube-music", "shuffle-last");

    let (state, effects) = reduce(
        AppState::new(config_without_lyrics()),
        Action::PlayMediaList {
            items: vec![first, selected.clone(), last],
            selected_id: selected.id.clone(),
            shuffle_seed: Some(41),
        },
    );

    assert!(state.queue().is_shuffled());
    assert_eq!(state.queue().snapshot().shuffle_seed, Some(41));
    assert_eq!(
        state.queue().current().map(QueueItem::media),
        Some(&selected)
    );
    assert_eq!(
        state.queue().snapshot().active[0],
        stable_queue_item_id(&selected.id)
    );
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, Effect::Resolve { item, .. } if item == &selected))
    );
}

#[test]
fn play_media_list_preparation_failures_preserve_existing_playback() {
    let old = media("youtube-music", "preserved");
    let state = state_with_results(
        config_without_lyrics(),
        vec![SearchItem::Playable(old.clone())],
    );
    let (state, _) = reduce(state, Action::ActivateSearchResult { index: 0 });
    let (state, _) = reduce(state, Action::RepeatModeChanged(RepeatMode::One));
    let (state, _) = reduce(
        state,
        Action::ShuffleEnabledChanged {
            enabled: true,
            seed: 73,
        },
    );
    let (state, _) = reduce(state, Action::RadioEnabledChanged(true));

    let missing = media("youtube-music", "missing-selection");
    let too_many = (0..=MAX_EXPLICIT_LIST_ITEMS)
        .map(|index| media("youtube-music", &format!("overflow-{index}")))
        .collect::<Vec<_>>();
    let invalid_cases = [
        (Vec::new(), missing.id.clone()),
        (vec![media("youtube-music", "present")], missing.id),
        (too_many.clone(), too_many[0].id.clone()),
    ];

    for (items, selected_id) in invalid_cases {
        let expected_queue = state.queue().snapshot();
        let expected_playback = state.playback().clone();
        let expected_presentation = state.player_presentation().clone();
        let expected_attempt = state.current_attempt_generation();
        let expected_resolve = state.current_resolve_generation();
        let expected_podcast_epoch = state.current_podcast_epoch();
        let expected_radio_generation = state.pending_radio_generation();
        let diagnostic_count = state.diagnostics().len();

        let (next, effects) = reduce(
            state.clone(),
            Action::PlayMediaList {
                items,
                selected_id,
                shuffle_seed: Some(99),
            },
        );

        assert_eq!(next.queue().snapshot(), expected_queue);
        assert_eq!(next.playback(), &expected_playback);
        assert_eq!(next.player_presentation(), &expected_presentation);
        assert_eq!(next.current_attempt_generation(), expected_attempt);
        assert_eq!(next.current_resolve_generation(), expected_resolve);
        assert_eq!(next.current_podcast_epoch(), expected_podcast_epoch);
        assert_eq!(next.pending_radio_generation(), expected_radio_generation);
        assert_eq!(next.diagnostics().len(), diagnostic_count + 1);
        assert_eq!(
            next.diagnostics()
                .last()
                .map(ytermusic::app::Diagnostic::category),
            Some(DiagnosticCategory::State)
        );
        assert_eq!(
            next.diagnostics()
                .last()
                .map(ytermusic::app::Diagnostic::message),
            Some("The playable list could not replace the queue; playback was not changed")
        );
        assert!(effects.is_empty());
    }
}

fn config_without_lyrics() -> Config {
    let mut config = Config::default();
    config.lyrics.enabled = false;
    config
}

fn preview_url(value: &str) -> PreviewStreamUrl {
    PreviewStreamUrl::parse(value)
        .unwrap_or_else(|error| panic!("test preview URL should parse: {error}"))
}

fn with_preview(state: AppState, generation: Generation, value: &str) -> AppState {
    reduce(
        state,
        Action::PreviewStreamUpdated {
            generation,
            preview_url: Some(preview_url(value)),
        },
    )
    .0
}

fn with_analysis(state: AppState, generation: Generation, value: &str) -> AppState {
    reduce(
        state,
        Action::AnalysisStreamUpdated {
            generation,
            stream_url: Some(analysis_url(value)),
        },
    )
    .0
}

fn analysis_url(value: &str) -> AnalysisStreamUrl {
    let mut stream = ResolvedStream::new(
        MediaId {
            provider: "analysis-provider-secret".to_owned(),
            video_id: "analysis-video-secret".to_owned(),
        },
        Url::parse(value).unwrap_or_else(|error| panic!("test analysis URL should parse: {error}")),
        time::OffsetDateTime::UNIX_EPOCH,
    );
    stream.title = Some("analysis-title-secret".to_owned());
    stream.duration_ms = Some(180_000);
    stream.codec = Some("opus".to_owned());
    stream.format_id = Some("251".to_owned());
    stream
        .analysis_stream_url()
        .unwrap_or_else(|| panic!("test analysis URL should be eligible"))
}

fn timed_lyrics() -> LyricsDocument {
    LyricsDocument::new(
        LyricsSource::Lrclib,
        None,
        vec![
            TimedLyricLine::new(1_000, Some(2_000), "first")
                .unwrap_or_else(|error| panic!("{error}")),
            TimedLyricLine::new(2_000, None, "second").unwrap_or_else(|error| panic!("{error}")),
        ],
        false,
    )
    .unwrap_or_else(|error| panic!("{error}"))
}

#[test]
fn playable_music_starts_generation_safe_lyrics_work() {
    for kind in [MediaKind::Song, MediaKind::Video] {
        let mut item = media(
            "youtube-music",
            if kind == MediaKind::Song {
                "song-lyrics"
            } else {
                "video-lyrics"
            },
        );
        item.kind = kind;
        let state = state_with_results(Config::default(), vec![SearchItem::Playable(item.clone())]);

        let (state, effects) = reduce(state, Action::ActivateSearchResult { index: 0 });

        let Some((generation, loaded)) = effects.iter().find_map(|effect| match effect {
            Effect::LoadLyrics { generation, item } => Some((*generation, item.media())),
            _ => None,
        }) else {
            panic!("playable music must request lyrics");
        };
        assert_eq!(loaded, &item);
        assert_eq!(state.lyrics().active_generation(), Some(generation));
        assert_eq!(state.lyrics().media_id(), Some(&item.id));
        assert!(state.lyrics().loading());
    }
}

#[test]
fn podcasts_and_disabled_configuration_do_not_request_lyrics() {
    let episode = podcast_episode("spoken-word");
    let state = state_with_results(Config::default(), vec![SearchItem::Playable(episode)]);
    let (_, effects) = reduce(state, Action::ActivateSearchResult { index: 0 });
    assert!(
        effects
            .iter()
            .all(|effect| !matches!(effect, Effect::LoadLyrics { .. }))
    );

    let mut config = Config::default();
    config.lyrics.enabled = false;
    let song = media("youtube-music", "disabled-lyrics");
    let state = state_with_results(config, vec![SearchItem::Playable(song)]);
    let (state, effects) = reduce(state, Action::ActivateSearchResult { index: 0 });
    assert!(
        effects
            .iter()
            .all(|effect| !matches!(effect, Effect::LoadLyrics { .. }))
    );
    assert!(state.lyrics().media_id().is_none());
}

#[test]
fn lyrics_completion_requires_matching_generation_and_media_and_tracks_backward_seek() {
    let item = media("youtube-music", "synchronized");
    let state = state_with_results(Config::default(), vec![SearchItem::Playable(item.clone())]);
    let (state, effects) = reduce(state, Action::ActivateSearchResult { index: 0 });
    let generation = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::LoadLyrics { generation, .. } => Some(*generation),
            _ => None,
        })
        .unwrap_or_else(|| panic!("missing lyrics load"));
    let wrong = MediaId {
        provider: "youtube-music".to_owned(),
        video_id: "wrong".to_owned(),
    };
    let expected = state.clone();
    let (state, _) = reduce(
        state,
        Action::LyricsCompleted {
            generation,
            media_id: wrong.into(),
            result: Ok(Some(timed_lyrics())),
        },
    );
    assert_eq!(state, expected);
    let (state, _) = reduce(
        state,
        Action::LyricsCompleted {
            generation: Generation::new(generation.value() + 1),
            media_id: item.id.clone().into(),
            result: Ok(Some(timed_lyrics())),
        },
    );
    assert_eq!(state, expected);
    let (state, _) = reduce(
        state,
        Action::LyricsCompleted {
            generation,
            media_id: item.id.clone().into(),
            result: Ok(Some(timed_lyrics())),
        },
    );
    let playback_generation = state
        .current_attempt_generation()
        .unwrap_or_else(|| panic!("missing playback generation"));
    let (state, _) = reduce(
        state,
        Action::PlayerProgress {
            generation: playback_generation,
            media_id: item.id.clone(),
            position_ms: 2_500,
            duration_ms: item.duration_ms,
        },
    );
    assert_eq!(state.lyrics().active_line_index(), Some(1));
    let (state, _) = reduce(
        state,
        Action::PlayerProgress {
            generation: playback_generation,
            media_id: item.id,
            position_ms: 1_500,
            duration_ms: Some(180_000),
        },
    );
    assert_eq!(state.lyrics().active_line_index(), Some(0));
}

#[test]
fn replacement_stop_and_resolve_failure_invalidate_lyrics_without_harming_audio() {
    let first = media("youtube-music", "first-lyrics");
    let second = media("youtube-music", "second-lyrics");
    let mut state = state_with_results(
        Config::default(),
        vec![
            SearchItem::Playable(first.clone()),
            SearchItem::Playable(second.clone()),
        ],
    );
    (state, _) = reduce(state, Action::ActivateSearchResult { index: 0 });
    let first_lyrics_generation = state
        .lyrics()
        .active_generation()
        .unwrap_or_else(|| panic!("missing lyrics generation"));
    let (state, effects) = reduce(state, Action::ActivateSearchResult { index: 1 });
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, Effect::ClearLyrics))
    );
    assert_ne!(
        state.lyrics().active_generation(),
        Some(first_lyrics_generation)
    );
    let playback_generation = state
        .current_attempt_generation()
        .unwrap_or_else(|| panic!("missing playback generation"));
    let (state, _) = reduce(
        state,
        Action::ResolveFailed {
            generation: playback_generation,
            error: AppError::new(AppErrorCategory::Resolve, "resolve failed"),
        },
    );
    assert!(state.lyrics().active_generation().is_none());
    assert_eq!(state.playback().status, PlaybackStatus::Failed);

    let state = state_with_results(Config::default(), vec![SearchItem::Playable(first)]);
    let (state, effects) = reduce(state, Action::ActivateSearchResult { index: 0 });
    let generation = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::LoadLyrics { generation, .. } => Some(*generation),
            _ => None,
        })
        .unwrap_or_else(|| panic!("missing lyrics load"));
    let playback_generation = state
        .current_attempt_generation()
        .unwrap_or_else(|| panic!("missing playback generation"));
    let (state, effects) = reduce(
        state,
        Action::PlayerStatusChanged {
            generation: playback_generation,
            status: PlaybackStatus::Stopped,
        },
    );
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, Effect::ClearLyrics))
    );
    let playback = state.playback().clone();
    let (state, _) = reduce(
        state,
        Action::LyricsCompleted {
            generation,
            media_id: second.id.into(),
            result: Err(AppError::new(
                AppErrorCategory::Lyrics,
                "lyrics unavailable",
            )),
        },
    );
    assert_eq!(state.playback(), &playback);
}

#[test]
fn song_to_podcast_replacement_clears_lyrics_before_progress_lookup_finishes() {
    let song = media("youtube-music", "song-before-podcast");
    let episode = podcast_episode("replacement-podcast");
    let mut state = state_with_results(Config::default(), vec![SearchItem::Playable(song.clone())]);
    (state, _) = reduce(state, Action::ActivateSearchResult { index: 0 });
    assert_eq!(state.lyrics().media_id(), Some(&song.id));
    (state, _) = reduce(
        state,
        Action::EnqueueMedia {
            item: episode.clone(),
        },
    );

    let (state, effects) = reduce(
        state,
        Action::PlayQueueItem {
            id: stable_queue_item_id(&episode.id),
        },
    );

    assert!(state.podcasts().pending_progress_generation().is_some());
    assert!(state.lyrics().media_id().is_none());
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, Effect::ClearLyrics))
    );
    assert!(effects.iter().any(|effect| matches!(
        effect,
        Effect::LoadPodcastProgress { media_id, .. } if media_id == &episode.id
    )));
}

fn podcast_episode(video_id: &str) -> MediaItem {
    MediaItem {
        kind: MediaKind::PodcastEpisode,
        ..media("youtube-music", video_id)
    }
}

fn assert_progress_load_before_resolution(
    effects: &[Effect],
    expected_media_id: &MediaId,
) -> Generation {
    let Some(generation) = effects.iter().find_map(|effect| match effect {
        Effect::LoadPodcastProgress {
            generation,
            media_id,
        } if media_id == expected_media_id => Some(*generation),
        _ => None,
    }) else {
        panic!("podcast start must load persisted progress");
    };
    assert!(
        !effects
            .iter()
            .any(|effect| matches!(effect, Effect::Resolve { .. }))
    );
    assert!(
        !effects
            .iter()
            .any(|effect| matches!(effect, Effect::SavePodcastProgress(_)))
    );
    assert!(
        !effects
            .iter()
            .any(|effect| matches!(effect, Effect::Persist(_)))
    );
    generation
}

fn page(items: Vec<MediaItem>) -> SearchPage {
    SearchPage::new(items.into_iter().map(SearchItem::Playable).collect())
}

fn state_with_results(config: Config, items: Vec<SearchItem>) -> AppState {
    let (state, effects) = reduce(
        AppState::new(config),
        Action::SearchSubmitted {
            query: "songs".to_owned(),
            filter: SearchFilter::Songs,
        },
    );
    let [Effect::Search { generation, .. }] = effects.as_slice() else {
        panic!("search submission must emit exactly one search effect");
    };
    let (state, completion_effects) = reduce(
        state,
        Action::SearchCompleted {
            generation: *generation,
            result: Ok(SearchPage::new(items)),
        },
    );
    assert!(
        completion_effects
            .iter()
            .all(|effect| matches!(effect, Effect::FetchArtwork { .. } | Effect::ClearArtwork))
    );
    state
}

fn checkpoint(state: &AppState) -> SessionCheckpoint {
    SessionCheckpoint {
        queue: state.queue().snapshot(),
        playback: state.playback().clone(),
    }
}

fn current_resolve_generation(state: &AppState) -> Generation {
    let Some(generation) = state.current_resolve_generation() else {
        panic!("expected an active resolution generation");
    };
    generation
}

fn current_attempt_generation(state: &AppState) -> Generation {
    let Some(generation) = state.current_attempt_generation() else {
        panic!("expected a current playback attempt generation");
    };
    generation
}

fn state_with_playback_progress(
    mut item: MediaItem,
    position_ms: u64,
    duration_ms: Option<u64>,
) -> AppState {
    item.duration_ms = duration_ms;
    let (state, _) = reduce(
        AppState::default(),
        Action::EnqueueMedia { item: item.clone() },
    );
    let (state, _) = reduce(
        state,
        Action::PlayQueueItem {
            id: stable_queue_item_id(&item.id),
        },
    );
    let generation = current_attempt_generation(&state);
    reduce(
        state,
        Action::PlayerProgress {
            generation,
            media_id: item.id,
            position_ms,
            duration_ms,
        },
    )
    .0
}

#[test]
fn relative_seek_without_current_playable_media_is_a_no_op() {
    let state = AppState::default();
    let expected = state.clone();
    let (state, effects) = reduce(state, Action::SeekRelativeRequested { seconds: 10 });
    assert_eq!(state, expected);
    assert!(effects.is_empty());
}

#[test]
fn relative_seek_clamps_to_known_start_and_end_without_mutating_progress() {
    for (position_ms, seconds, expected_seconds) in
        [(5_000, -10, -5), (175_000, 10, 5), (60_000, 10, 10)]
    {
        let state = state_with_playback_progress(
            media("youtube-music", "bounded-seek"),
            position_ms,
            Some(180_000),
        );
        let expected = state.clone();
        let (state, effects) = reduce(state, Action::SeekRelativeRequested { seconds });
        assert_eq!(
            state, expected,
            "progress must remain supervisor-authoritative"
        );
        assert_eq!(
            effects,
            vec![Effect::Player(PlayerCommand::SeekRelative {
                seconds: expected_seconds,
            })]
        );
    }

    let state = state_with_playback_progress(
        media("youtube-music", "zero-delta-seek"),
        180_000,
        Some(180_000),
    );
    let expected = state.clone();
    let (state, effects) = reduce(state, Action::SeekRelativeRequested { seconds: 10 });
    assert_eq!(state, expected);
    assert!(effects.is_empty());
}

#[test]
fn relative_seek_rounds_subsecond_bounded_deltas_away_from_zero() {
    for (position_ms, duration_ms, seconds, expected_seconds) in
        [(500, 180_500, -10, -1), (180_000, 180_500, 10, 1)]
    {
        let state = state_with_playback_progress(
            media("youtube-music", "subsecond-bounded-seek"),
            position_ms,
            Some(duration_ms),
        );
        let expected = state.clone();
        let (state, effects) = reduce(state, Action::SeekRelativeRequested { seconds });
        assert_eq!(
            state, expected,
            "subsecond clamping must not mutate displayed progress optimistically"
        );
        assert_eq!(
            effects,
            vec![Effect::Player(PlayerCommand::SeekRelative {
                seconds: expected_seconds,
            })]
        );
    }
}

#[test]
fn relative_seek_with_unknown_duration_clamps_only_to_representable_bounds() {
    let cases = [
        (3_000, -10, -3),
        (60_000, 10, 10),
        (u64::MAX - 5_000, 10, 5),
        (10_000, i64::MIN, -10),
    ];
    for (position_ms, seconds, expected_seconds) in cases {
        let state = state_with_playback_progress(
            media("youtube-music", "unknown-duration-seek"),
            position_ms,
            None,
        );
        let expected = state.clone();
        let (state, effects) = reduce(state, Action::SeekRelativeRequested { seconds });
        assert_eq!(state, expected);
        assert_eq!(
            effects,
            vec![Effect::Player(PlayerCommand::SeekRelative {
                seconds: expected_seconds,
            })]
        );
    }
}

fn region(value: &str) -> RegionCode {
    match RegionCode::parse(value) {
        Ok(region) => region,
        Err(error) => panic!("test region must be valid: {error}"),
    }
}

fn url(value: &str) -> Url {
    match Url::parse(value) {
        Ok(url) => url,
        Err(error) => panic!("test artwork URL must be valid: {error}"),
    }
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

fn podcast_recommendations_with_artwork(
    country: &str,
    rows: &[(&str, &str, &str, Option<&str>)],
) -> PodcastRecommendationPage {
    let results = rows
        .iter()
        .map(|(id, title, publisher, artwork)| {
            serde_json::json!({
                "id": id,
                "name": title,
                "artistName": publisher,
                "artworkUrl100": artwork,
            })
        })
        .collect::<Vec<_>>();
    let bytes = serde_json::to_vec(&serde_json::json!({
        "feed": {"country": country, "results": results}
    }))
    .unwrap_or_else(|error| panic!("podcast artwork fixture must encode: {error}"));
    parse_apple_top_shows(&bytes)
        .unwrap_or_else(|error| panic!("podcast artwork fixture must parse: {error}"))
}

#[test]
fn podcast_recommendation_state_starts_in_the_configured_region() {
    let config = Config {
        region: region("JP"),
        ..Config::default()
    };

    let state = AppState::new(config);

    assert_eq!(state.podcasts().requested_region(), &region("JP"));
    assert!(state.podcasts().effective_region().is_none());
    assert!(state.podcasts().recommendations().is_empty());
}

#[test]
fn podcast_recommendation_actions_and_effects_redact_external_text() {
    let title = "sentinel-podcast-title";
    let publisher = "sentinel-podcast-publisher";
    let provider_id = "sentinel-podcast-provider-id";
    let page = podcast_recommendations("US", &[("source-id", title, publisher)]);
    let recommendation = &page.items()[0];
    let effect = Effect::ResolvePodcastRecommendation {
        generation: Generation::new(1),
        recommendation: recommendation.clone(),
    };
    let action = Action::PodcastRecommendationResolved {
        generation: Generation::new(1),
        result: Ok(PodcastProviderId::new(provider_id.to_owned())
            .unwrap_or_else(|| panic!("valid provider id"))),
    };
    let error_action = Action::PodcastRecommendationResolved {
        generation: Generation::new(1),
        result: Err(AppError::new(
            AppErrorCategory::Podcast,
            "sentinel-upstream-error-message",
        )),
    };

    let debug = format!("{effect:?} {action:?} {error_action:?}");
    for sentinel in [
        title,
        publisher,
        provider_id,
        "sentinel-upstream-error-message",
    ] {
        assert!(!debug.contains(sentinel), "debug leaked {sentinel}");
    }
}

#[test]
fn podcast_provider_ids_enforce_the_provider_opaque_id_invariant_and_redact() {
    assert!(PodcastProviderId::new(String::new()).is_none());
    assert!(PodcastProviderId::new("x".repeat(512)).is_some());
    assert!(PodcastProviderId::new("x".repeat(513)).is_none());
    assert!(PodcastProviderId::new("contains whitespace".to_owned()).is_none());
    assert!(PodcastProviderId::new("contains\u{0007}control".to_owned()).is_none());

    let sentinel = "sentinel-provider-opaque-id";
    let id = PodcastProviderId::new(sentinel.to_owned())
        .unwrap_or_else(|| panic!("valid opaque provider id"));
    assert!(!format!("{id:?} {id}").contains(sentinel));
    assert_eq!(id.as_str(), sentinel);
}

#[test]
fn podcast_recommendation_context_changes_invalidate_an_active_match() {
    let us = region("US");
    let page = podcast_recommendations(
        "US",
        &[
            ("daily", "The Daily", "NYT"),
            ("up-first", "Up First", "NPR"),
        ],
    );
    let second_id = page.items()[1].source_id().clone();
    let (state, effects) = reduce(
        AppState::default(),
        Action::PodcastRecommendationsRequested { region: us.clone() },
    );
    let [Effect::LoadPodcastRecommendations { generation, .. }] = effects.as_slice() else {
        panic!("source request");
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
        panic!("match request");
    };
    let stale_match = *generation;

    let (state, _) = reduce(
        state,
        Action::PodcastRecommendationSelectionChanged { id: second_id },
    );
    assert!(state.podcasts().active_resolve_generation().is_none());
    assert!(!state.podcasts().resolve_loading());
    let before = state.clone();
    let (state, effects) = reduce(
        state,
        Action::PodcastRecommendationResolved {
            generation: stale_match,
            result: Ok(PodcastProviderId::new("stale-provider".to_owned())
                .unwrap_or_else(|| panic!("valid provider id"))),
        },
    );
    assert!(effects.is_empty());
    assert_eq!(state, before);
}

#[test]
fn podcast_recommendation_same_selection_is_a_noop_and_new_context_clears_match_error() {
    let us = region("US");
    let page = podcast_recommendations(
        "US",
        &[
            ("daily", "The Daily", "NYT"),
            ("up-first", "Up First", "NPR"),
        ],
    );
    let first_id = page.items()[0].source_id().clone();
    let second_id = page.items()[1].source_id().clone();
    let (state, effects) = reduce(
        AppState::default(),
        Action::PodcastRecommendationsRequested { region: us.clone() },
    );
    let [Effect::LoadPodcastRecommendations { generation, .. }] = effects.as_slice() else {
        panic!("source request");
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
        panic!("match request");
    };
    let active_match = *generation;
    let before_same = state.clone();

    let (state, effects) = reduce(
        state,
        Action::PodcastRecommendationSelectionChanged {
            id: first_id.clone(),
        },
    );
    assert!(effects.is_empty());
    assert_eq!(state, before_same);

    let (state, _) = reduce(
        state,
        Action::PodcastRecommendationResolved {
            generation: active_match,
            result: Err(AppError::new(
                AppErrorCategory::Podcast,
                "Match unavailable",
            )),
        },
    );
    assert!(state.podcasts().resolve_error().is_some());
    let (state, _) = reduce(
        state,
        Action::PodcastRecommendationSelectionChanged { id: second_id },
    );
    assert!(state.podcasts().resolve_error().is_none());

    let (state, effects) = reduce(state, Action::OpenSelectedPodcastRecommendation);
    let [Effect::ResolvePodcastRecommendation { generation, .. }] = effects.as_slice() else {
        panic!("second match request");
    };
    let (state, _) = reduce(
        state,
        Action::PodcastRecommendationResolved {
            generation: *generation,
            result: Err(AppError::new(
                AppErrorCategory::Podcast,
                "Second match unavailable",
            )),
        },
    );
    let jp = region("JP");
    let (state, effects) = reduce(
        state,
        Action::PodcastRecommendationsRequested { region: jp.clone() },
    );
    assert!(state.podcasts().resolve_error().is_some());
    let [Effect::LoadPodcastRecommendations { generation, .. }] = effects.as_slice() else {
        panic!("new-country request");
    };
    let (state, _) = reduce(
        state,
        Action::PodcastRecommendationsCompleted {
            generation: *generation,
            requested_region: jp,
            result: Ok(podcast_recommendations(
                "JP",
                &[("jp-show", "Japan Show", "Publisher")],
            )),
        },
    );
    assert!(state.podcasts().resolve_error().is_none());
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "the artwork lifecycle assertions remain in reducer protocol order"
)]
fn podcast_recommendation_artwork_tracks_auto_selection_changes_close_and_stale_completion() {
    let first_art = artwork_url("https://images.example.test/podcast-first.jpg");
    let second_art = artwork_url("https://images.example.test/podcast-second.jpg");
    let episode_art = artwork_url("https://images.example.test/podcast-episode.jpg");
    let page = podcast_recommendations_with_artwork(
        "US",
        &[
            (
                "first",
                "First Show",
                "Publisher",
                Some(first_art.as_url().as_str()),
            ),
            (
                "second",
                "Second Show",
                "Publisher",
                Some(second_art.as_url().as_str()),
            ),
            ("no-art", "No Art", "Publisher", None),
        ],
    );
    let second_id = page.items()[1].source_id().clone();
    let no_art_id = page.items()[2].source_id().clone();
    let us = region("US");
    let (state, effects) = reduce(
        with_artwork_surface(AppState::default(), ArtworkSurface::Podcasts),
        Action::PodcastRecommendationsRequested { region: us.clone() },
    );
    let [Effect::LoadPodcastRecommendations { generation, .. }] = effects.as_slice() else {
        panic!("source request");
    };
    let (state, effects) = reduce(
        state,
        Action::PodcastRecommendationsCompleted {
            generation: *generation,
            requested_region: us,
            result: Ok(page),
        },
    );
    let [
        Effect::FetchArtwork {
            generation: first_art_generation,
            url,
        },
    ] = effects.as_slice()
    else {
        panic!("auto-selection must request recommendation artwork");
    };
    assert_eq!(url, &first_art);

    let stale_art_generation = *first_art_generation;
    let (state, effects) = reduce(
        state,
        Action::PodcastRecommendationSelectionChanged {
            id: second_id.clone(),
        },
    );
    let [Effect::FetchArtwork { url, .. }] = effects.as_slice() else {
        panic!("selection change must request recommendation artwork");
    };
    assert_eq!(url, &second_art);
    let before_stale = state.clone();
    let (state, effects) = reduce(
        state,
        Action::ArtworkCompleted {
            generation: stale_art_generation,
            result: Ok(()),
        },
    );
    assert!(effects.is_empty());
    assert_eq!(state, before_stale);

    let (state, effects) = reduce(
        state,
        Action::PodcastRecommendationSelectionChanged { id: no_art_id },
    );
    assert!(matches!(effects.as_slice(), [Effect::ClearArtwork]));
    assert!(state.artwork().requested_url().is_none());

    let (state, effects) = reduce(
        state,
        Action::PodcastRecommendationSelectionChanged { id: second_id },
    );
    assert!(matches!(effects.as_slice(), [Effect::FetchArtwork { .. }]));
    let (state, effects) = reduce(state, Action::OpenSelectedPodcastRecommendation);
    let [Effect::ResolvePodcastRecommendation { generation, .. }] = effects.as_slice() else {
        panic!("match request");
    };
    let (state, effects) = reduce(
        state,
        Action::PodcastRecommendationResolved {
            generation: *generation,
            result: Ok(PodcastProviderId::new("provider-show".to_owned())
                .unwrap_or_else(|| panic!("valid provider id"))),
        },
    );
    let [Effect::LoadPodcast { generation, .. }] = effects.as_slice() else {
        panic!("show request");
    };
    let mut episode = podcast_episode("episode-with-art");
    episode.artwork_url = Some(episode_art.as_url().clone());
    let (state, effects) = reduce(
        state,
        Action::PodcastCompleted {
            generation: *generation,
            result: Ok(Podcast {
                id: "provider-show".to_owned(),
                title: "Second Show".to_owned(),
                creators: vec!["Publisher".to_owned()],
                description: None,
                artwork_url: None,
                episodes: vec![episode],
            }),
        },
    );
    let [Effect::FetchArtwork { url, .. }] = effects.as_slice() else {
        panic!("open show must request episode artwork");
    };
    assert_eq!(url, &episode_art);

    let (state, effects) = reduce(state, Action::ClosePodcast);
    let [Effect::FetchArtwork { url, .. }] = effects.as_slice() else {
        panic!("close must return to selected recommendation artwork");
    };
    assert_eq!(url, &second_art);
    assert!(state.podcasts().show().is_none());
}

#[test]
fn podcast_recommendation_request_and_completion_are_region_and_generation_scoped() {
    let jp = region("JP");
    let us = region("US");
    let (state, effects) = reduce(
        AppState::default(),
        Action::PodcastRecommendationsRequested { region: jp.clone() },
    );
    let [
        Effect::LoadPodcastRecommendations {
            generation,
            region: effect_region,
        },
    ] = effects.as_slice()
    else {
        panic!("recommendation request must emit exactly one source effect");
    };
    assert_eq!(effect_region, &jp);
    assert_eq!(
        state.podcasts().active_recommendation_generation(),
        Some(*generation)
    );
    assert!(state.podcasts().recommendations_loading());

    let stale_generation = *generation;
    let (state, effects) = reduce(
        state,
        Action::PodcastRecommendationsRequested { region: us.clone() },
    );
    assert_eq!(effects.len(), 1);
    let stale_page = podcast_recommendations("JP", &[("jp-daily", "Daily JP", "Publisher")]);
    let before = state.clone();
    let (state, effects) = reduce(
        state,
        Action::PodcastRecommendationsCompleted {
            generation: stale_generation,
            requested_region: jp,
            result: Ok(stale_page),
        },
    );
    assert!(effects.is_empty());
    assert_eq!(state, before);

    let active_generation = state
        .podcasts()
        .active_recommendation_generation()
        .unwrap_or_else(|| panic!("US request must remain active"));
    let page = podcast_recommendations(
        "US",
        &[
            ("daily", "The Daily", "NYT"),
            ("up-first", "Up First", "NPR"),
        ],
    );
    let daily_id = page.items()[0].source_id().clone();
    let (state, effects) = reduce(
        state,
        Action::PodcastRecommendationsCompleted {
            generation: active_generation,
            requested_region: us.clone(),
            result: Ok(page),
        },
    );
    assert!(effects.is_empty());
    assert_eq!(state.podcasts().effective_region(), Some(&us));
    assert_eq!(state.podcasts().selected_recommendation(), Some(&daily_id));
    assert!(!state.podcasts().recommendations_loading());
}

#[test]
fn podcast_recommendation_completion_rebounds_items_and_preserves_valid_selection() {
    let us = region("US");
    let rows = (0..(MAX_PODCAST_RECOMMENDATIONS + 4))
        .map(|index| {
            (
                format!("id-{index}"),
                format!("Show {index}"),
                "Publisher".to_owned(),
            )
        })
        .collect::<Vec<_>>();
    let borrowed = rows
        .iter()
        .map(|(id, title, publisher)| (id.as_str(), title.as_str(), publisher.as_str()))
        .collect::<Vec<_>>();
    let page = podcast_recommendations("US", &borrowed);
    let retained_id = page.items()[1].source_id().clone();
    let (state, request_effects) = reduce(
        AppState::default(),
        Action::PodcastRecommendationsRequested { region: us.clone() },
    );
    let [Effect::LoadPodcastRecommendations { generation, .. }] = request_effects.as_slice() else {
        panic!("recommendation request effect");
    };
    let (state, _) = reduce(
        state,
        Action::PodcastRecommendationsCompleted {
            generation: *generation,
            requested_region: us.clone(),
            result: Ok(page),
        },
    );
    let (state, _) = reduce(
        state,
        Action::PodcastRecommendationSelectionChanged {
            id: retained_id.clone(),
        },
    );
    let refresh_page = podcast_recommendations(
        "US",
        &[
            ("replacement", "Replacement", "Publisher"),
            ("id-1", "Show 1", "Publisher"),
        ],
    );
    let (state, effects) = reduce(
        state,
        Action::PodcastRecommendationsRequested { region: us.clone() },
    );
    let [Effect::LoadPodcastRecommendations { generation, .. }] = effects.as_slice() else {
        panic!("refresh effect");
    };
    let (state, _) = reduce(
        state,
        Action::PodcastRecommendationsCompleted {
            generation: *generation,
            requested_region: us,
            result: Ok(refresh_page),
        },
    );
    assert!(state.podcasts().recommendations().len() <= MAX_PODCAST_RECOMMENDATIONS);
    assert_eq!(
        state.podcasts().selected_recommendation(),
        Some(&retained_id)
    );
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "the preservation regression keeps discovery and show state in one lifecycle"
)]
fn podcast_recommendation_selection_and_failures_preserve_discovery_and_open_show() {
    let us = region("US");
    let page = podcast_recommendations("US", &[("daily", "The Daily", "NYT")]);
    let daily_id = page.items()[0].source_id().clone();
    let unknown_id = podcast_recommendations("US", &[("unknown", "Unknown", "Other")]).items()[0]
        .source_id()
        .clone();
    let (state, effects) = reduce(
        with_artwork_surface(AppState::default(), ArtworkSurface::Podcasts),
        Action::PodcastRecommendationsRequested { region: us.clone() },
    );
    let [Effect::LoadPodcastRecommendations { generation, .. }] = effects.as_slice() else {
        panic!("request effect");
    };
    let (state, _) = reduce(
        state,
        Action::PodcastRecommendationsCompleted {
            generation: *generation,
            requested_region: us.clone(),
            result: Ok(page),
        },
    );
    let (state, effects) = reduce(
        state,
        Action::PodcastRecommendationSelectionChanged { id: unknown_id },
    );
    assert!(effects.is_empty());
    assert_eq!(state.podcasts().selected_recommendation(), Some(&daily_id));

    let show_art = artwork_url("https://images.example.test/open-show-episode.jpg");
    let mut open_episode = podcast_episode("open-episode");
    open_episode.artwork_url = Some(show_art.as_url().clone());
    let show = Podcast {
        id: "open-show".to_owned(),
        title: "Open Show".to_owned(),
        creators: vec!["Host".to_owned()],
        description: None,
        artwork_url: None,
        episodes: vec![open_episode],
    };
    let metadata =
        SearchMetadata::new(SearchMetadataKind::Podcast, "Open Show").with_provider_id("open-show");
    let (state, effects) = reduce(
        state,
        Action::SearchSubmitted {
            query: "open show".to_owned(),
            filter: SearchFilter::Podcasts,
        },
    );
    let [Effect::Search { generation, .. }] = effects.as_slice() else {
        panic!("manual podcast search effect");
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
        panic!("show load effect");
    };
    let (state, effects) = reduce(
        state,
        Action::PodcastCompleted {
            generation: *generation,
            result: Ok(show),
        },
    );
    let [Effect::FetchArtwork { url, .. }] = effects.as_slice() else {
        panic!("open show artwork effect");
    };
    assert_eq!(url, &show_art);
    let recommendation_ids = state
        .podcasts()
        .recommendations()
        .iter()
        .map(|recommendation| recommendation.source_id().clone())
        .collect::<Vec<_>>();
    let (state, request_effects) = reduce(
        state,
        Action::PodcastRecommendationsRequested { region: us.clone() },
    );
    let [Effect::LoadPodcastRecommendations { generation, .. }] = request_effects.as_slice() else {
        panic!("source refresh effect");
    };
    let (state, effects) = reduce(
        state,
        Action::PodcastRecommendationsCompleted {
            generation: *generation,
            requested_region: us,
            result: Err(AppError::new(
                AppErrorCategory::Podcast,
                "Rankings unavailable",
            )),
        },
    );
    assert!(effects.is_empty());
    assert!(state.podcasts().show().is_some());
    assert_eq!(state.artwork().requested_url(), Some(&show_art));
    assert_eq!(
        state
            .podcasts()
            .recommendations()
            .iter()
            .map(|recommendation| recommendation.source_id().clone())
            .collect::<Vec<_>>(),
        recommendation_ids
    );
    assert_eq!(state.podcasts().selected_recommendation(), Some(&daily_id));
    assert!(state.podcasts().recommendation_error().is_some());
}

fn artwork_url(value: &str) -> ArtworkUrl {
    match ArtworkUrl::try_from(url(value)) {
        Ok(url) => url,
        Err(error) => panic!("test artwork URL must be accepted: {error}"),
    }
}

fn fetched_artwork(effects: &[Effect], expected: &ArtworkUrl) -> bool {
    effects
        .iter()
        .any(|effect| matches!(effect, Effect::FetchArtwork { url, .. } if url == expected))
}

fn state_with_pending_artwork() -> AppState {
    let (state, effects) = reduce(
        AppState::default(),
        Action::ArtworkRequested {
            url: artwork_url("https://images.example.test/pending-old-art.jpg"),
        },
    );
    assert!(matches!(effects.as_slice(), [Effect::FetchArtwork { .. }]));
    state
}

fn with_artwork_surface(state: AppState, surface: ArtworkSurface) -> AppState {
    reduce(state, Action::ArtworkSurfaceChanged { surface }).0
}

fn state_with_pending_artwork_for(surface: ArtworkSurface) -> AppState {
    let state = with_artwork_surface(AppState::default(), surface);
    let (state, effects) = reduce(
        state,
        Action::ArtworkRequested {
            url: artwork_url("https://images.example.test/pending-old-art.jpg"),
        },
    );
    assert!(matches!(effects.as_slice(), [Effect::FetchArtwork { .. }]));
    state
}

fn assert_artwork_cleared(state: &AppState, effects: &[Effect], label: &str) {
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, Effect::ClearArtwork)),
        "{label} did not emit artwork invalidation"
    );
    assert!(
        state.artwork().requested_url().is_none(),
        "{label} retained the requested artwork URL"
    );
    assert!(
        state.artwork().ready_url().is_none(),
        "{label} retained ready artwork"
    );
    assert!(
        state.artwork().active_generation().is_none(),
        "{label} retained the old artwork generation"
    );
    assert!(!state.artwork().loading(), "{label} remained loading");
}

fn state_with_single_radio_item(
    config: Config,
    item: MediaItem,
) -> (AppState, Generation, Generation) {
    let state = state_with_results(config, vec![SearchItem::Playable(item)]);
    let (state, _) = reduce(state, Action::ActivateSearchResult { index: 0 });
    let attempt = current_attempt_generation(&state);
    let (state, _) = reduce(state, Action::RadioEnabledChanged(true));
    let Some(radio_generation) = state.pending_radio_generation() else {
        panic!("enabling radio on a short queue must request a fill");
    };
    (state, attempt, radio_generation)
}

fn pending_podcast_after_playing_song() -> (AppState, Generation, Generation, MediaItem, MediaItem)
{
    let song = media("youtube-music", "outgoing-song");
    let episode = podcast_episode("pending-episode");
    let mut state = AppState::default();
    for item in [song.clone(), episode.clone()] {
        (state, _) = reduce(state, Action::EnqueueMedia { item });
    }
    let (state, _) = reduce(
        state,
        Action::PlayQueueItem {
            id: stable_queue_item_id(&song.id),
        },
    );
    let outgoing_generation = current_attempt_generation(&state);
    let (state, _) = reduce(
        state,
        Action::PlayerStatusChanged {
            generation: outgoing_generation,
            status: PlaybackStatus::Playing,
        },
    );
    let (state, effects) = reduce(
        state,
        Action::PlayQueueItem {
            id: stable_queue_item_id(&episode.id),
        },
    );
    let progress_generation = assert_progress_load_before_resolution(&effects, &episode.id);
    (
        state,
        outgoing_generation,
        progress_generation,
        song,
        episode,
    )
}

fn loaded_chart_generation(effects: &[Effect]) -> Generation {
    let Some(generation) = effects.iter().find_map(|effect| match effect {
        Effect::LoadCharts { generation, .. } => Some(*generation),
        _ => None,
    }) else {
        panic!("chart request must load live data");
    };
    generation
}

fn assert_terminal_status_ends_attempt(terminal_status: PlaybackStatus) {
    let current = media("youtube-music", "terminal-status");
    let state = state_with_results(
        config_without_lyrics(),
        vec![SearchItem::Playable(current.clone())],
    );
    let (state, _) = reduce(state, Action::ActivateSearchResult { index: 0 });
    let generation = current_attempt_generation(&state);

    let (state, effects) = reduce(
        state,
        Action::PlayerStatusChanged {
            generation,
            status: terminal_status,
        },
    );
    assert_eq!(state.playback().status, terminal_status);
    assert!(state.current_attempt_generation().is_none());
    assert!(state.current_resolve_generation().is_none());
    assert_eq!(effects, vec![Effect::Persist(checkpoint(&state))]);
    let expected = state.clone();

    let (state, effects) = reduce(
        state,
        Action::PlayerStatusChanged {
            generation,
            status: PlaybackStatus::Playing,
        },
    );
    assert_eq!(state, expected);
    assert!(effects.is_empty());

    let (state, effects) = reduce(
        state,
        Action::PlayerProgress {
            generation,
            media_id: current.id,
            position_ms: 50_000,
            duration_ms: Some(200_000),
        },
    );
    assert_eq!(state, expected);
    assert!(effects.is_empty());

    let (state, effects) = reduce(state, Action::PlayerEnded { generation });
    assert_eq!(state, expected);
    assert!(effects.is_empty());

    let (state, effects) = reduce(
        state,
        Action::ResolveFailed {
            generation,
            error: AppError::new(AppErrorCategory::Resolve, "Delayed terminal failure"),
        },
    );
    assert_eq!(state, expected);
    assert!(effects.is_empty());
}

#[test]
fn default_state_uses_configured_playback_targets() {
    let mut config = Config::default();
    config.playback.volume = 63;
    config.podcast.speed = 1.4;

    let state = AppState::new(config);

    assert_eq!(state.playback().status, PlaybackStatus::Stopped);
    assert_eq!(state.playback().position_ms, 0);
    assert_eq!(state.playback().target_volume, 63);
    assert_eq!(state.playback().playback_speed.to_bits(), 1.4_f64.to_bits());
    assert!(state.playback().current.is_none());
    assert!(state.queue().items().is_empty());
}

#[test]
fn search_submissions_increment_generation_and_newer_search_supersedes_older() {
    let (state, first_effects) = reduce(
        AppState::default(),
        Action::SearchSubmitted {
            query: "a".to_owned(),
            filter: SearchFilter::All,
        },
    );
    let first = Generation::new(1);
    assert_eq!(
        first_effects,
        vec![Effect::Search {
            generation: first,
            query: "a".to_owned(),
            filter: SearchFilter::All,
        }]
    );
    assert_eq!(state.search().generation(), first);
    assert_eq!(state.search().active_generation(), Some(first));
    assert!(state.search().loading());

    let (state, second_effects) = reduce(
        state,
        Action::SearchSubmitted {
            query: "ab".to_owned(),
            filter: SearchFilter::Songs,
        },
    );
    let second = Generation::new(2);
    assert_eq!(
        second_effects,
        vec![Effect::Search {
            generation: second,
            query: "ab".to_owned(),
            filter: SearchFilter::Songs,
        }]
    );
    assert_eq!(state.search().query(), "ab");
    assert_eq!(state.search().filter(), SearchFilter::Songs);
    assert_eq!(state.search().generation(), second);
    assert_eq!(state.search().active_generation(), Some(second));
    assert!(state.search().loading());
    assert!(state.search().items().is_empty());
}

#[test]
fn stale_search_completion_is_ignored_entirely() {
    let (state, _) = reduce(
        AppState::default(),
        Action::SearchSubmitted {
            query: "a".to_owned(),
            filter: SearchFilter::All,
        },
    );
    let first = state.search().generation();
    let (state, _) = reduce(
        state,
        Action::SearchSubmitted {
            query: "ab".to_owned(),
            filter: SearchFilter::All,
        },
    );
    let expected = state.clone();

    let (state, effects) = reduce(
        state,
        Action::SearchCompleted {
            generation: first,
            result: Ok(page(vec![media("youtube-music", "old")])),
        },
    );

    assert_eq!(state, expected);
    assert!(effects.is_empty());
    assert!(state.search().items().is_empty());
    assert!(state.search().loading());
    assert_eq!(state.search().query(), "ab");
}

#[test]
fn current_search_success_replaces_items_and_stops_loading() {
    let (state, _) = reduce(
        AppState::default(),
        Action::SearchSubmitted {
            query: "current".to_owned(),
            filter: SearchFilter::Songs,
        },
    );
    let generation = state.search().generation();
    let expected_page = page(vec![media("youtube-music", "current")]);

    let (state, effects) = reduce(
        state,
        Action::SearchCompleted {
            generation,
            result: Ok(expected_page.clone()),
        },
    );

    assert!(effects.is_empty());
    assert_eq!(state.search().items(), expected_page.items());
    assert!(!state.search().loading());
    assert!(state.search().active_generation().is_none());
    assert!(state.search().error().is_none());
}

#[test]
fn current_search_error_is_safe_and_stops_loading() {
    let (state, _) = reduce(
        AppState::default(),
        Action::SearchSubmitted {
            query: "current".to_owned(),
            filter: SearchFilter::Songs,
        },
    );
    let generation = state.search().generation();
    let error = AppError::new(
        AppErrorCategory::Search,
        "Search is temporarily unavailable",
    );

    let (state, effects) = reduce(
        state,
        Action::SearchCompleted {
            generation,
            result: Err(error.clone()),
        },
    );

    assert!(effects.is_empty());
    assert!(!state.search().loading());
    assert!(state.search().items().is_empty());
    assert_eq!(state.search().error(), Some(&error));
    assert!(state.search().active_generation().is_none());
}

#[test]
fn activating_playable_result_selects_unique_queue_item_and_requests_resolution() {
    let selected = media("youtube-music", "selected");
    let state = state_with_results(
        config_without_lyrics(),
        vec![SearchItem::Playable(selected.clone())],
    );

    let (state, effects) = reduce(state, Action::ActivateSearchResult { index: 0 });
    let generation = current_resolve_generation(&state);

    assert_eq!(state.queue().items().len(), 1);
    let Some(current) = state.queue().current() else {
        panic!("activated result must become current");
    };
    assert_eq!(current.id(), &stable_queue_item_id(&selected.id));
    assert_eq!(current.media(), &selected);
    assert_eq!(state.playback().current.as_ref(), Some(&selected.id));
    assert_eq!(state.playback().status, PlaybackStatus::Resolving);
    assert!(state.pending_radio_generation().is_none());
    assert_eq!(
        effects,
        vec![
            Effect::Resolve {
                generation,
                item: selected,
                start_ms: None,
            },
            Effect::Persist(checkpoint(&state)),
        ]
    );
}

#[test]
fn activating_podcast_search_result_loads_progress_before_resolution() {
    let episode = podcast_episode("search-episode");
    let state = state_with_results(
        Config::default(),
        vec![SearchItem::Playable(episode.clone())],
    );

    let (state, effects) = reduce(state, Action::ActivateSearchResult { index: 0 });

    assert_progress_load_before_resolution(&effects, &episode.id);
    assert_eq!(
        state.queue().current().map(QueueItem::media),
        Some(&episode)
    );
    assert!(state.current_attempt_generation().is_none());
}

#[test]
fn queue_next_and_previous_load_podcast_progress_before_resolution() {
    let previous_episode = podcast_episode("previous-episode");
    let current_song = media("youtube-music", "navigation-song");
    let next_episode = podcast_episode("next-episode");
    let mut state = AppState::default();
    for item in [
        previous_episode.clone(),
        current_song.clone(),
        next_episode.clone(),
    ] {
        (state, _) = reduce(state, Action::EnqueueMedia { item });
    }
    let (state, _) = reduce(
        state,
        Action::PlayQueueItem {
            id: stable_queue_item_id(&current_song.id),
        },
    );

    let (state, next_effects) = reduce(state, Action::NextRequested);
    assert_progress_load_before_resolution(&next_effects, &next_episode.id);

    let (state, _) = reduce(
        state,
        Action::PlayQueueItem {
            id: stable_queue_item_id(&current_song.id),
        },
    );
    let (_, previous_effects) = reduce(state, Action::PreviousRequested);
    assert_progress_load_before_resolution(&previous_effects, &previous_episode.id);
}

#[test]
fn stopped_podcast_retry_reloads_progress_before_resolution() {
    let episode = podcast_episode("stopped-retry");
    let (state, _) = reduce(
        AppState::default(),
        Action::EnqueueMedia {
            item: episode.clone(),
        },
    );
    let (state, effects) = reduce(
        state,
        Action::PlayQueueItem {
            id: stable_queue_item_id(&episode.id),
        },
    );
    let progress_generation = assert_progress_load_before_resolution(&effects, &episode.id);
    let (state, _) = reduce(
        state,
        Action::PodcastProgressLoaded {
            generation: progress_generation,
            progress: None,
        },
    );
    let attempt = current_attempt_generation(&state);
    let (state, stopped_effects) = reduce(
        state,
        Action::PlayerStatusChanged {
            generation: attempt,
            status: PlaybackStatus::Stopped,
        },
    );
    assert!(matches!(
        stopped_effects.as_slice(),
        [
            Effect::SavePodcastProgress(checkpoint),
            Effect::Persist(_)
        ] if checkpoint.media_id() == &episode.id
            && checkpoint.playback_epoch() == 1
            && !checkpoint.played()
    ));

    let (_, retry_effects) = reduce(state, Action::TogglePlayback);
    assert_progress_load_before_resolution(&retry_effects, &episode.id);
}

#[test]
fn newer_song_start_invalidates_pending_podcast_progress_completion() {
    let episode = podcast_episode("superseded-episode");
    let song = media("youtube-music", "superseding-song");
    let state = state_with_results(
        Config::default(),
        vec![
            SearchItem::Playable(episode.clone()),
            SearchItem::Playable(song.clone()),
        ],
    );
    let (state, effects) = reduce(state, Action::ActivateSearchResult { index: 0 });
    let stale_generation = assert_progress_load_before_resolution(&effects, &episode.id);
    let (state, effects) = reduce(state, Action::ActivateSearchResult { index: 1 });
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, Effect::Resolve { item, .. } if item == &song))
    );
    let expected = state.clone();

    let (state, effects) = reduce(
        state,
        Action::PodcastProgressLoaded {
            generation: stale_generation,
            progress: None,
        },
    );

    assert_eq!(state, expected);
    assert!(effects.is_empty());
}

#[test]
fn newer_podcast_start_invalidates_older_pending_progress_completion() {
    let first = podcast_episode("pending-first");
    let second = podcast_episode("pending-second");
    let mut state = AppState::default();
    for item in [first.clone(), second.clone()] {
        (state, _) = reduce(state, Action::EnqueueMedia { item });
    }
    let (state, effects) = reduce(
        state,
        Action::PlayQueueItem {
            id: stable_queue_item_id(&first.id),
        },
    );
    let stale_generation = assert_progress_load_before_resolution(&effects, &first.id);
    let (state, effects) = reduce(
        state,
        Action::PlayQueueItem {
            id: stable_queue_item_id(&second.id),
        },
    );
    let current_generation = assert_progress_load_before_resolution(&effects, &second.id);
    let expected = state.clone();

    let (state, stale_effects) = reduce(
        state,
        Action::PodcastProgressLoaded {
            generation: stale_generation,
            progress: None,
        },
    );
    assert_eq!(state, expected);
    assert!(stale_effects.is_empty());

    let (state, current_effects) = reduce(
        state,
        Action::PodcastProgressLoaded {
            generation: current_generation,
            progress: None,
        },
    );
    assert_eq!(state.playback().current.as_ref(), Some(&second.id));
    assert!(
        current_effects
            .iter()
            .any(|effect| matches!(effect, Effect::Resolve { item, .. } if item == &second))
    );
}

#[test]
fn pending_podcast_rejects_every_delayed_outgoing_playback_action() {
    let (pending, outgoing_generation, progress_generation, song, episode) =
        pending_podcast_after_playing_song();
    assert_eq!(
        pending.podcasts().pending_progress_generation(),
        Some(progress_generation)
    );
    assert_eq!(
        pending.queue().current().map(QueueItem::media),
        Some(&episode)
    );

    let stale_actions = [
        (
            "resolve success",
            Action::ResolveSucceeded {
                generation: outgoing_generation,
            },
        ),
        (
            "resolve failure",
            Action::ResolveFailed {
                generation: outgoing_generation,
                error: AppError::new(AppErrorCategory::Resolve, "delayed resolve failure"),
            },
        ),
        (
            "playing status",
            Action::PlayerStatusChanged {
                generation: outgoing_generation,
                status: PlaybackStatus::Paused,
            },
        ),
        (
            "stopped status",
            Action::PlayerStatusChanged {
                generation: outgoing_generation,
                status: PlaybackStatus::Stopped,
            },
        ),
        (
            "failed status",
            Action::PlayerStatusChanged {
                generation: outgoing_generation,
                status: PlaybackStatus::Failed,
            },
        ),
        (
            "progress",
            Action::PlayerProgress {
                generation: outgoing_generation,
                media_id: song.id.clone(),
                position_ms: 91_000,
                duration_ms: song.duration_ms,
            },
        ),
        (
            "format",
            Action::ResolvedFormatUpdated {
                generation: outgoing_generation,
                quality: ytermusic::app::ResolverQuality::new(Some("aac"), Some("140")),
            },
        ),
        (
            "preview",
            Action::PreviewStreamUpdated {
                generation: outgoing_generation,
                preview_url: Some(preview_url("https://video.invalid/stale")),
            },
        ),
        (
            "telemetry",
            Action::PlaybackTelemetryUpdated {
                generation: outgoing_generation,
                effective_volume: 17.0,
                fade: Some(ytermusic::app::FadeActivity::Out),
            },
        ),
        (
            "natural end",
            Action::PlayerEnded {
                generation: outgoing_generation,
            },
        ),
    ];

    for (label, action) in stale_actions {
        let (state, effects) = reduce(pending.clone(), action);
        assert_eq!(state, pending, "{label} must not mutate pending playback");
        assert!(effects.is_empty(), "{label} must not emit effects");
    }
}

#[test]
fn podcast_switch_checkpoints_outgoing_epoch_before_loading_new_progress() {
    let outgoing = podcast_episode("outgoing-podcast");
    let incoming = podcast_episode("incoming-podcast");
    let mut state = AppState::default();
    for item in [outgoing.clone(), incoming.clone()] {
        (state, _) = reduce(state, Action::EnqueueMedia { item });
    }
    let (state, effects) = reduce(
        state,
        Action::PlayQueueItem {
            id: stable_queue_item_id(&outgoing.id),
        },
    );
    let progress_generation = assert_progress_load_before_resolution(&effects, &outgoing.id);
    let (state, _) = reduce(
        state,
        Action::PodcastProgressLoaded {
            generation: progress_generation,
            progress: None,
        },
    );
    let outgoing_generation = current_attempt_generation(&state);
    let (state, _) = reduce(
        state,
        Action::PlayerProgress {
            generation: outgoing_generation,
            media_id: outgoing.id.clone(),
            position_ms: 55_000,
            duration_ms: outgoing.duration_ms,
        },
    );

    let (state, effects) = reduce(
        state,
        Action::PlayQueueItem {
            id: stable_queue_item_id(&incoming.id),
        },
    );

    let [
        Effect::SavePodcastProgress(checkpoint),
        Effect::LoadPodcastProgress { media_id, .. },
    ] = effects.as_slice()
    else {
        panic!("podcast switch must checkpoint before loading new progress");
    };
    assert_eq!(checkpoint.media_id(), &outgoing.id);
    assert_eq!(checkpoint.playback_epoch(), 1);
    assert_eq!(checkpoint.position_ms(), 55_000);
    assert!(!checkpoint.played());
    assert_eq!(media_id, &incoming.id);
    assert!(state.current_attempt_generation().is_none());
    assert!(state.current_resolve_generation().is_none());
    assert!(state.current_podcast_epoch().is_none());
    assert_eq!(
        state.queue().current().map(QueueItem::media),
        Some(&incoming)
    );
}

#[test]
fn podcast_progress_uses_canonical_queued_media_after_duplicate_append() {
    let mut canonical = podcast_episode("canonical-queued-podcast");
    canonical.title = "Canonical queued title".to_owned();
    canonical.creators = vec!["Canonical host".to_owned()];
    canonical.duration_ms = Some(60_000);
    let mut refreshed = canonical.clone();
    refreshed.title = "Refreshed provider title".to_owned();
    refreshed.creators = vec!["Refreshed host".to_owned()];
    refreshed.duration_ms = Some(300_000);
    let metadata = SearchMetadata::new(SearchMetadataKind::Podcast, "Canonical Show")
        .with_provider_id("canonical-show");
    let mut state = state_with_results(Config::default(), vec![SearchItem::Metadata(metadata)]);
    (state, _) = reduce(
        state,
        Action::EnqueueMedia {
            item: canonical.clone(),
        },
    );
    let (state, effects) = reduce(state, Action::OpenSelectedPodcast);
    let [Effect::LoadPodcast { generation, .. }] = effects.as_slice() else {
        panic!("podcast show must load");
    };
    let (state, _) = reduce(
        state,
        Action::PodcastCompleted {
            generation: *generation,
            result: Ok(Podcast {
                id: "canonical-show".to_owned(),
                title: "Canonical Show".to_owned(),
                creators: vec!["Show Host".to_owned()],
                description: None,
                artwork_url: None,
                episodes: vec![refreshed],
            }),
        },
    );
    let (state, effects) = reduce(
        state,
        Action::PlayPodcastEpisode {
            media_id: canonical.id.clone(),
        },
    );
    let progress_generation = assert_progress_load_before_resolution(&effects, &canonical.id);

    let (state, effects) = reduce(
        state,
        Action::PodcastProgressLoaded {
            generation: progress_generation,
            progress: Some(PodcastProgress {
                video_id: canonical.id.video_id.clone(),
                playback_epoch: 7,
                position_ms: 120_000,
                duration_ms: Some(300_000),
                played: false,
                updated_at: 1,
            }),
        },
    );

    assert_eq!(
        state.queue().current().map(QueueItem::media),
        Some(&canonical)
    );
    assert_eq!(state.playback().position_ms, 60_000);
    assert_eq!(state.playback().duration_ms, canonical.duration_ms);
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, Effect::Resolve { item, .. } if item == &canonical))
    );
    let Some(baseline) = effects.iter().find_map(|effect| match effect {
        Effect::SavePodcastProgress(checkpoint) => Some(checkpoint),
        _ => None,
    }) else {
        panic!("canonical podcast start must emit a baseline checkpoint");
    };
    assert_eq!(baseline.playback_epoch(), 8);
    assert_eq!(baseline.position_ms(), 60_000);
    assert_eq!(baseline.duration_ms(), canonical.duration_ms);
}

#[test]
fn stable_queue_ids_are_not_ambiguous_raw_concatenations() {
    let first = MediaId {
        provider: "ab".to_owned(),
        video_id: "c".to_owned(),
    };
    let second = MediaId {
        provider: "a".to_owned(),
        video_id: "bc".to_owned(),
    };

    assert_ne!(stable_queue_item_id(&first), stable_queue_item_id(&second));
    assert_eq!(stable_queue_item_id(&first), stable_queue_item_id(&first));
}

#[test]
fn player_progress_only_updates_current_media_and_status_updates_snapshot() {
    let current = media("youtube-music", "current");
    let other = media("youtube-music", "other");
    let state = state_with_results(
        Config::default(),
        vec![SearchItem::Playable(current.clone())],
    );
    let (state, _) = reduce(state, Action::ActivateSearchResult { index: 0 });
    let generation = current_attempt_generation(&state);
    let expected = state.clone();

    let (state, effects) = reduce(
        state,
        Action::PlayerProgress {
            generation,
            media_id: other.id,
            position_ms: 20_000,
            duration_ms: Some(240_000),
        },
    );
    assert_eq!(state, expected);
    assert!(effects.is_empty());

    let (state, effects) = reduce(
        state,
        Action::PlayerProgress {
            generation,
            media_id: current.id,
            position_ms: 12_345,
            duration_ms: Some(200_000),
        },
    );
    assert!(effects.is_empty());
    assert_eq!(state.playback().position_ms, 12_345);
    assert_eq!(state.playback().duration_ms, Some(200_000));

    let (state, effects) = reduce(
        state,
        Action::PlayerStatusChanged {
            generation,
            status: PlaybackStatus::Paused,
        },
    );
    assert!(effects.is_empty());
    assert_eq!(state.playback().status, PlaybackStatus::Paused);
}

#[test]
fn matching_player_lifecycle_keeps_attempt_after_resolution_success() {
    let current = media("youtube-music", "lifecycle");
    let state = state_with_results(
        config_without_lyrics(),
        vec![SearchItem::Playable(current.clone())],
    );
    let (state, _) = reduce(state, Action::ActivateSearchResult { index: 0 });
    let generation = current_attempt_generation(&state);
    assert_eq!(state.current_resolve_generation(), Some(generation));

    let (state, effects) = reduce(state, Action::ResolveSucceeded { generation });
    assert!(effects.is_empty());
    assert!(state.current_resolve_generation().is_none());
    assert_eq!(state.current_attempt_generation(), Some(generation));
    assert_eq!(state.playback().status, PlaybackStatus::Buffering);

    let (state, effects) = reduce(
        state,
        Action::PlayerStatusChanged {
            generation,
            status: PlaybackStatus::Playing,
        },
    );
    assert_eq!(
        effects,
        vec![
            Effect::RecordHistory {
                item: current.clone(),
            },
            Effect::Notify(NowPlayingNotification::from_media(generation, &current)),
        ]
    );
    assert_eq!(state.playback().status, PlaybackStatus::Playing);

    let (state, effects) = reduce(
        state,
        Action::PlayerProgress {
            generation,
            media_id: current.id,
            position_ms: 42_000,
            duration_ms: Some(200_000),
        },
    );
    assert!(effects.is_empty());
    assert_eq!(state.playback().position_ms, 42_000);

    let state = with_analysis(state, generation, "https://media.invalid/natural-end");
    let (state, effects) = reduce(state, Action::PlayerEnded { generation });
    assert_eq!(state.playback().status, PlaybackStatus::Stopped);
    assert!(state.current_attempt_generation().is_none());
    assert!(state.current_resolve_generation().is_none());
    assert!(state.player_presentation().analysis_url().is_none());
    assert_eq!(effects, vec![Effect::Persist(checkpoint(&state))]);
}

#[test]
fn matching_stopped_status_ends_attempt_and_rejects_later_observations() {
    assert_terminal_status_ends_attempt(PlaybackStatus::Stopped);
}

#[test]
fn matching_failed_status_ends_attempt_and_rejects_later_observations() {
    assert_terminal_status_ends_attempt(PlaybackStatus::Failed);
}

#[test]
fn resolution_success_rejects_delayed_failure_for_same_generation() {
    let state = state_with_results(
        Config::default(),
        vec![SearchItem::Playable(media("youtube-music", "resolved"))],
    );
    let (state, _) = reduce(state, Action::ActivateSearchResult { index: 0 });
    let generation = current_attempt_generation(&state);
    let (state, _) = reduce(state, Action::ResolveSucceeded { generation });
    let expected = state.clone();

    let (state, effects) = reduce(
        state,
        Action::ResolveFailed {
            generation,
            error: AppError::new(AppErrorCategory::Resolve, "Delayed failure"),
        },
    );

    assert_eq!(state, expected);
    assert!(effects.is_empty());
}

#[test]
fn stale_old_track_player_observations_are_ignored_entirely() {
    let first = media("youtube-music", "old-attempt");
    let second = media("youtube-music", "current-attempt");
    let state = state_with_results(
        Config::default(),
        vec![
            SearchItem::Playable(first.clone()),
            SearchItem::Playable(second),
        ],
    );
    let (state, _) = reduce(state, Action::ActivateSearchResult { index: 0 });
    let stale_generation = current_attempt_generation(&state);
    let (state, _) = reduce(state, Action::ActivateSearchResult { index: 1 });
    let expected = state.clone();

    let (state, effects) = reduce(
        state,
        Action::PlayerStatusChanged {
            generation: stale_generation,
            status: PlaybackStatus::Paused,
        },
    );
    assert_eq!(state, expected);
    assert!(effects.is_empty());

    let (state, effects) = reduce(
        state,
        Action::PlayerProgress {
            generation: stale_generation,
            media_id: first.id,
            position_ms: 99_000,
            duration_ms: Some(100_000),
        },
    );
    assert_eq!(state, expected);
    assert!(effects.is_empty());

    let (state, effects) = reduce(
        state,
        Action::PlayerEnded {
            generation: stale_generation,
        },
    );
    assert_eq!(state, expected);
    assert!(effects.is_empty());
}

#[test]
fn duplicate_player_end_advances_queue_only_once() {
    let first = media("youtube-music", "first");
    let second = media("youtube-music", "second");
    let third = media("youtube-music", "third");
    let state = state_with_results(
        Config::default(),
        vec![
            SearchItem::Playable(first),
            SearchItem::Playable(second.clone()),
            SearchItem::Playable(third),
        ],
    );
    let (state, _) = reduce(state, Action::ActivateSearchResult { index: 0 });
    let (state, _) = reduce(state, Action::ActivateSearchResult { index: 1 });
    let (state, _) = reduce(state, Action::ActivateSearchResult { index: 2 });
    let (state, _) = reduce(state, Action::ActivateSearchResult { index: 0 });
    let ended_generation = current_attempt_generation(&state);

    let (state, _) = reduce(
        state,
        Action::PlayerEnded {
            generation: ended_generation,
        },
    );
    assert_eq!(state.queue().current().map(QueueItem::media), Some(&second));
    let expected = state.clone();

    let (state, effects) = reduce(
        state,
        Action::PlayerEnded {
            generation: ended_generation,
        },
    );
    assert_eq!(state, expected);
    assert!(effects.is_empty());
}

#[test]
fn stale_progress_is_ignored_when_same_media_id_is_replayed() {
    let current = media("youtube-music", "repeat");
    let state = state_with_results(
        Config::default(),
        vec![SearchItem::Playable(current.clone())],
    );
    let (state, _) = reduce(state, Action::ActivateSearchResult { index: 0 });
    let (state, _) = reduce(state, Action::RepeatModeChanged(RepeatMode::One));
    let stale_generation = current_attempt_generation(&state);
    let (state, _) = reduce(
        state,
        Action::PlayerEnded {
            generation: stale_generation,
        },
    );
    let current_generation = current_attempt_generation(&state);
    assert_ne!(current_generation, stale_generation);
    assert_eq!(state.playback().current.as_ref(), Some(&current.id));
    let expected = state.clone();

    let (state, effects) = reduce(
        state,
        Action::PlayerProgress {
            generation: stale_generation,
            media_id: current.id,
            position_ms: 123_000,
            duration_ms: Some(180_000),
        },
    );

    assert_eq!(state, expected);
    assert!(effects.is_empty());
}

#[test]
fn active_resolve_failure_without_auto_skip_stays_failed_and_records_diagnostic() {
    let first = media("youtube-music", "first");
    let second = media("youtube-music", "second");
    let state = state_with_results(
        config_without_lyrics(),
        vec![
            SearchItem::Playable(first.clone()),
            SearchItem::Playable(second),
        ],
    );
    let (state, _) = reduce(state, Action::ActivateSearchResult { index: 0 });
    let (state, _) = reduce(state, Action::ActivateSearchResult { index: 1 });
    let (state, _) = reduce(state, Action::ActivateSearchResult { index: 0 });
    let generation = current_resolve_generation(&state);
    let error = AppError::new(AppErrorCategory::Resolve, "This item is unavailable");

    let (state, effects) = reduce(
        state,
        Action::ResolveFailed {
            generation,
            error: error.clone(),
        },
    );

    assert_eq!(state.playback().status, PlaybackStatus::Failed);
    assert_eq!(state.playback().current.as_ref(), Some(&first.id));
    assert!(state.current_resolve_generation().is_none());
    let Some(diagnostic) = state.diagnostics().last() else {
        panic!("active resolve failure must be diagnosed");
    };
    assert_eq!(diagnostic.category(), DiagnosticCategory::Resolve);
    assert_eq!(diagnostic.message(), error.message());
    assert_eq!(diagnostic.media_id(), Some(&first.id));
    assert_eq!(effects, vec![Effect::Persist(checkpoint(&state))]);
}

#[test]
fn stale_resolve_failure_is_ignored_entirely() {
    let first = media("youtube-music", "first");
    let second = media("youtube-music", "second");
    let state = state_with_results(
        Config::default(),
        vec![SearchItem::Playable(first), SearchItem::Playable(second)],
    );
    let (state, _) = reduce(state, Action::ActivateSearchResult { index: 0 });
    let stale_generation = current_resolve_generation(&state);
    let (state, _) = reduce(state, Action::ActivateSearchResult { index: 1 });
    let expected = state.clone();

    let (state, effects) = reduce(
        state,
        Action::ResolveFailed {
            generation: stale_generation,
            error: AppError::new(AppErrorCategory::Resolve, "Old failure"),
        },
    );

    assert_eq!(state, expected);
    assert!(effects.is_empty());
}

#[test]
fn active_resolve_failure_with_auto_skip_resolves_next_item_and_persists() {
    let mut config = config_without_lyrics();
    config.behavior.auto_skip_unavailable = true;
    let first = media("youtube-music", "first");
    let second = media("youtube-music", "second");
    let state = state_with_results(
        config,
        vec![
            SearchItem::Playable(first.clone()),
            SearchItem::Playable(second.clone()),
        ],
    );
    let (state, _) = reduce(state, Action::ActivateSearchResult { index: 0 });
    let (state, _) = reduce(state, Action::ActivateSearchResult { index: 1 });
    let (state, _) = reduce(state, Action::ActivateSearchResult { index: 0 });
    let failed_generation = current_resolve_generation(&state);

    let (state, effects) = reduce(
        state,
        Action::ResolveFailed {
            generation: failed_generation,
            error: AppError::new(AppErrorCategory::Resolve, "Unavailable"),
        },
    );
    let next_generation = current_resolve_generation(&state);

    assert_ne!(next_generation, failed_generation);
    assert_eq!(state.queue().current().map(QueueItem::media), Some(&second));
    assert_eq!(state.playback().current.as_ref(), Some(&second.id));
    assert_eq!(state.playback().status, PlaybackStatus::Resolving);
    assert_eq!(state.diagnostics().len(), 1);
    assert_eq!(
        effects,
        vec![
            Effect::Resolve {
                generation: next_generation,
                item: second,
                start_ms: None,
            },
            Effect::Persist(checkpoint(&state)),
        ]
    );
}

#[test]
fn natural_end_resolves_next_and_respects_repeat_modes() {
    let first = media("youtube-music", "first");
    let second = media("youtube-music", "second");
    let state = state_with_results(
        config_without_lyrics(),
        vec![
            SearchItem::Playable(first.clone()),
            SearchItem::Playable(second.clone()),
        ],
    );
    let (state, _) = reduce(state, Action::ActivateSearchResult { index: 0 });
    let (state, _) = reduce(state, Action::ActivateSearchResult { index: 1 });
    let (state, _) = reduce(state, Action::ActivateSearchResult { index: 0 });
    let first_attempt = current_attempt_generation(&state);

    let (state, effects) = reduce(
        state,
        Action::PlayerEnded {
            generation: first_attempt,
        },
    );
    let generation = current_resolve_generation(&state);
    assert_eq!(state.queue().current().map(QueueItem::media), Some(&second));
    assert_eq!(
        effects,
        vec![
            Effect::Resolve {
                generation,
                item: second.clone(),
                start_ms: None,
            },
            Effect::Persist(checkpoint(&state)),
        ]
    );

    let (state, _) = reduce(state, Action::RepeatModeChanged(RepeatMode::One));
    let repeated_attempt = current_attempt_generation(&state);
    let (state, effects) = reduce(
        state,
        Action::PlayerEnded {
            generation: repeated_attempt,
        },
    );
    let generation = current_resolve_generation(&state);
    assert_eq!(state.queue().current().map(QueueItem::media), Some(&second));
    assert_eq!(
        effects,
        vec![
            Effect::Resolve {
                generation,
                item: second,
                start_ms: None,
            },
            Effect::Persist(checkpoint(&state)),
        ]
    );

    let (state, _) = reduce(state, Action::RepeatModeChanged(RepeatMode::All));
    let wrapping_attempt = current_attempt_generation(&state);
    let (state, effects) = reduce(
        state,
        Action::PlayerEnded {
            generation: wrapping_attempt,
        },
    );
    let generation = current_resolve_generation(&state);
    assert_eq!(state.queue().current().map(QueueItem::media), Some(&first));
    assert_eq!(
        effects,
        vec![
            Effect::Resolve {
                generation,
                item: first,
                start_ms: None,
            },
            Effect::Persist(checkpoint(&state)),
        ]
    );
}

#[test]
fn natural_advance_to_podcast_loads_progress_before_resolution() {
    let song = media("youtube-music", "ending-before-podcast");
    let episode = podcast_episode("natural-next-episode");
    let mut state = AppState::default();
    for item in [song.clone(), episode.clone()] {
        (state, _) = reduce(state, Action::EnqueueMedia { item });
    }
    let (state, _) = reduce(
        state,
        Action::PlayQueueItem {
            id: stable_queue_item_id(&song.id),
        },
    );
    let ending_attempt = current_attempt_generation(&state);

    let (_, effects) = reduce(
        state,
        Action::PlayerEnded {
            generation: ending_attempt,
        },
    );

    assert_progress_load_before_resolution(&effects, &episode.id);
}

#[test]
fn repeating_podcast_saves_completion_before_reloading_progress() {
    let episode = podcast_episode("repeat-podcast");
    let (state, _) = reduce(
        AppState::default(),
        Action::EnqueueMedia {
            item: episode.clone(),
        },
    );
    let (state, effects) = reduce(
        state,
        Action::PlayQueueItem {
            id: stable_queue_item_id(&episode.id),
        },
    );
    let progress_generation = assert_progress_load_before_resolution(&effects, &episode.id);
    let (state, _) = reduce(
        state,
        Action::PodcastProgressLoaded {
            generation: progress_generation,
            progress: None,
        },
    );
    let ending_attempt = current_attempt_generation(&state);
    let (state, _) = reduce(state, Action::RepeatModeChanged(RepeatMode::One));

    let (_, effects) = reduce(
        state,
        Action::PlayerEnded {
            generation: ending_attempt,
        },
    );

    let checkpoint_index = effects
        .iter()
        .position(|effect| {
            matches!(
                effect,
                Effect::SavePodcastProgress(checkpoint)
                    if checkpoint.media_id() == &episode.id
                        && checkpoint.playback_epoch() == 1
                        && checkpoint.played()
            )
        })
        .unwrap_or_else(|| panic!("natural end must checkpoint the completed podcast"));
    let load_index = effects
        .iter()
        .position(|effect| {
            matches!(
                effect,
                Effect::LoadPodcastProgress { media_id, .. } if media_id == &episode.id
            )
        })
        .unwrap_or_else(|| panic!("repeat-one must reload persisted podcast progress"));
    assert!(checkpoint_index < load_index);
    assert!(
        !effects
            .iter()
            .any(|effect| matches!(effect, Effect::Resolve { .. }))
    );
}

#[test]
fn resolve_auto_skip_to_podcast_loads_progress_before_resolution() {
    let song = media("youtube-music", "failed-before-podcast");
    let episode = podcast_episode("auto-skip-episode");
    let mut config = Config::default();
    config.behavior.auto_skip_unavailable = true;
    let mut state = AppState::new(config);
    for item in [song.clone(), episode.clone()] {
        (state, _) = reduce(state, Action::EnqueueMedia { item });
    }
    let (state, _) = reduce(
        state,
        Action::PlayQueueItem {
            id: stable_queue_item_id(&song.id),
        },
    );
    let failed_generation = current_resolve_generation(&state);

    let (_, effects) = reduce(
        state,
        Action::ResolveFailed {
            generation: failed_generation,
            error: AppError::new(AppErrorCategory::Resolve, "fixture resolution failure"),
        },
    );

    assert_progress_load_before_resolution(&effects, &episode.id);
}

#[test]
fn radio_resume_to_podcast_loads_progress_before_resolution() {
    let seed = media("youtube-music", "radio-before-podcast");
    let episode = podcast_episode("radio-podcast");
    let (state, ending_attempt, radio_generation) =
        state_with_single_radio_item(Config::default(), seed);
    let (state, _) = reduce(
        state,
        Action::PlayerEnded {
            generation: ending_attempt,
        },
    );

    let (_, effects) = reduce(
        state,
        Action::RadioFillCompleted {
            generation: radio_generation,
            result: Ok(vec![episode.clone()]),
        },
    );

    assert_progress_load_before_resolution(&effects, &episode.id);
}

#[test]
fn pending_podcast_start_cancels_an_older_radio_resume_intent() {
    let seed = media("youtube-music", "radio-race-seed");
    let episode = podcast_episode("radio-race-podcast");
    let appended_song = media("youtube-music", "late-radio-song");
    let (state, ending_attempt, radio_generation) =
        state_with_single_radio_item(Config::default(), seed);
    let (state, _) = reduce(
        state,
        Action::PlayerEnded {
            generation: ending_attempt,
        },
    );
    let (state, _) = reduce(
        state,
        Action::EnqueueMedia {
            item: episode.clone(),
        },
    );
    let (state, effects) = reduce(
        state,
        Action::PlayQueueItem {
            id: stable_queue_item_id(&episode.id),
        },
    );
    let progress_generation = assert_progress_load_before_resolution(&effects, &episode.id);

    let (state, effects) = reduce(
        state,
        Action::RadioFillCompleted {
            generation: radio_generation,
            result: Ok(vec![appended_song]),
        },
    );

    assert_eq!(
        state.queue().current().map(QueueItem::media),
        Some(&episode)
    );
    assert_eq!(
        state.podcasts().pending_progress_generation(),
        Some(progress_generation)
    );
    assert!(
        !effects
            .iter()
            .any(|effect| matches!(effect, Effect::Resolve { .. }))
    );
}

#[test]
fn radio_fill_is_single_flight_stale_safe_unique_and_persisted() {
    let first = media("youtube-music", "first");
    let second = media("youtube-music", "second");
    let third = media("youtube-music", "third");
    let state = state_with_results(
        config_without_lyrics(),
        vec![
            SearchItem::Playable(first),
            SearchItem::Playable(second.clone()),
        ],
    );
    let (state, _) = reduce(state, Action::ActivateSearchResult { index: 0 });
    let (state, _) = reduce(state, Action::ActivateSearchResult { index: 1 });
    let (state, _) = reduce(state, Action::ActivateSearchResult { index: 0 });
    let ending_attempt = current_attempt_generation(&state);
    let (state, _) = reduce(state, Action::RadioEnabledChanged(true));

    let (state, effects) = reduce(
        state,
        Action::PlayerEnded {
            generation: ending_attempt,
        },
    );
    let resolve_generation = current_resolve_generation(&state);
    let Some(radio_generation) = state.pending_radio_generation() else {
        panic!("low radio queue must start one fill");
    };
    assert_eq!(
        effects,
        vec![
            Effect::Resolve {
                generation: resolve_generation,
                item: second.clone(),
                start_ms: None,
            },
            Effect::Persist(checkpoint(&state)),
        ]
    );

    let expected = state.clone();
    let (state, effects) = reduce(state, Action::CheckRadioFill);
    assert_eq!(state, expected);
    assert!(effects.is_empty());

    let (state, effects) = reduce(
        state,
        Action::RadioFillCompleted {
            generation: resolve_generation,
            result: Ok(vec![third.clone()]),
        },
    );
    assert_eq!(state, expected);
    assert!(effects.is_empty());

    let (state, effects) = reduce(
        state,
        Action::RadioFillCompleted {
            generation: radio_generation,
            result: Ok(vec![second, third.clone(), third]),
        },
    );

    assert!(state.pending_radio_generation().is_none());
    assert_eq!(state.queue().items().len(), 3);
    assert_eq!(effects, vec![Effect::Persist(checkpoint(&state))]);
}

#[test]
fn enabling_radio_on_short_queue_persists_and_starts_one_fill() {
    let seed = media("youtube-music", "radio-seed");
    let state = state_with_results(Config::default(), vec![SearchItem::Playable(seed.clone())]);
    let (state, _) = reduce(state, Action::ActivateSearchResult { index: 0 });

    let (state, effects) = reduce(state, Action::RadioEnabledChanged(true));
    let Some(generation) = state.pending_radio_generation() else {
        panic!("enabling radio must start a fill");
    };

    assert_eq!(
        effects,
        vec![
            Effect::Persist(checkpoint(&state)),
            Effect::FillRadio {
                generation,
                seed: seed.id,
            },
        ]
    );
    let expected = state.clone();
    let (state, effects) = reduce(state, Action::RadioEnabledChanged(true));
    assert_eq!(state, expected);
    assert!(effects.is_empty());
}

#[test]
fn first_activation_after_enabling_empty_radio_starts_fill() {
    let seed = media("youtube-music", "first-radio-item");
    let (state, effects) = reduce(
        AppState::new(config_without_lyrics()),
        Action::RadioEnabledChanged(true),
    );
    assert!(state.queue().items().is_empty());
    assert!(state.pending_radio_generation().is_none());
    assert_eq!(effects, vec![Effect::Persist(checkpoint(&state))]);

    let (state, _) = reduce(
        state,
        Action::SearchSubmitted {
            query: "first".to_owned(),
            filter: SearchFilter::Songs,
        },
    );
    let search_generation = state.search().generation();
    let (state, _) = reduce(
        state,
        Action::SearchCompleted {
            generation: search_generation,
            result: Ok(page(vec![seed.clone()])),
        },
    );

    let (state, effects) = reduce(state, Action::ActivateSearchResult { index: 0 });
    let resolve_generation = current_attempt_generation(&state);
    let Some(radio_generation) = state.pending_radio_generation() else {
        panic!("first radio activation must start a fill");
    };
    assert_eq!(
        effects,
        vec![
            Effect::Resolve {
                generation: resolve_generation,
                item: seed.clone(),
                start_ms: None,
            },
            Effect::Persist(checkpoint(&state)),
            Effect::FillRadio {
                generation: radio_generation,
                seed: seed.id,
            },
        ]
    );
}

#[test]
fn slow_radio_fill_resumes_after_matched_end_stops_at_tail() {
    let seed = media("youtube-music", "radio-seed");
    let next = media("youtube-music", "radio-next");
    let (state, attempt, radio_generation) =
        state_with_single_radio_item(config_without_lyrics(), seed);

    let (state, effects) = reduce(
        state,
        Action::PlayerEnded {
            generation: attempt,
        },
    );
    assert_eq!(state.playback().status, PlaybackStatus::Stopped);
    assert!(state.current_attempt_generation().is_none());
    assert_eq!(state.pending_radio_generation(), Some(radio_generation));
    assert_eq!(effects, vec![Effect::Persist(checkpoint(&state))]);

    let (state, effects) = reduce(
        state,
        Action::RadioFillCompleted {
            generation: radio_generation,
            result: Ok(vec![next.clone()]),
        },
    );
    let next_attempt = current_attempt_generation(&state);

    assert_eq!(state.queue().current().map(QueueItem::media), Some(&next));
    assert_eq!(state.playback().status, PlaybackStatus::Resolving);
    assert!(state.pending_radio_generation().is_none());
    assert_eq!(
        effects,
        vec![
            Effect::Resolve {
                generation: next_attempt,
                item: next,
                start_ms: None,
            },
            Effect::Persist(checkpoint(&state)),
        ]
    );
}

#[test]
fn explicit_stop_while_radio_fill_is_pending_does_not_resume_appended_items() {
    let seed = media("youtube-music", "radio-seed");
    let next = media("youtube-music", "radio-next");
    let (state, attempt, radio_generation) =
        state_with_single_radio_item(config_without_lyrics(), seed.clone());

    let (state, effects) = reduce(
        state,
        Action::PlayerStatusChanged {
            generation: attempt,
            status: PlaybackStatus::Stopped,
        },
    );
    assert_eq!(state.playback().status, PlaybackStatus::Stopped);
    assert!(state.current_attempt_generation().is_none());
    assert_eq!(effects, vec![Effect::Persist(checkpoint(&state))]);

    let (state, effects) = reduce(
        state,
        Action::RadioFillCompleted {
            generation: radio_generation,
            result: Ok(vec![next]),
        },
    );

    assert_eq!(state.queue().items().len(), 2);
    assert_eq!(state.queue().current().map(QueueItem::media), Some(&seed));
    assert_eq!(state.playback().status, PlaybackStatus::Stopped);
    assert!(state.current_attempt_generation().is_none());
    assert!(state.current_resolve_generation().is_none());
    assert_eq!(effects, vec![Effect::Persist(checkpoint(&state))]);
}

#[test]
fn disabling_radio_clears_pending_continuation_before_reenable() {
    let seed = media("youtube-music", "radio-seed");
    let next = media("youtube-music", "radio-next");
    let (state, attempt, _) = state_with_single_radio_item(Config::default(), seed.clone());
    let (state, _) = reduce(
        state,
        Action::PlayerEnded {
            generation: attempt,
        },
    );

    let (state, _) = reduce(state, Action::RadioEnabledChanged(false));
    assert!(state.pending_radio_generation().is_none());
    let (state, _) = reduce(state, Action::RadioEnabledChanged(true));
    let Some(radio_generation) = state.pending_radio_generation() else {
        panic!("re-enabling radio must start a replacement fill");
    };

    let (state, effects) = reduce(
        state,
        Action::RadioFillCompleted {
            generation: radio_generation,
            result: Ok(vec![next]),
        },
    );

    assert_eq!(state.queue().items().len(), 2);
    assert_eq!(state.queue().current().map(QueueItem::media), Some(&seed));
    assert_eq!(state.playback().status, PlaybackStatus::Stopped);
    assert!(state.current_attempt_generation().is_none());
    assert!(state.current_resolve_generation().is_none());
    assert_eq!(effects, vec![Effect::Persist(checkpoint(&state))]);
}

#[test]
fn activating_replacement_clears_pending_continuation_intent() {
    let seed = media("youtube-music", "radio-seed");
    let replacement = media("youtube-music", "replacement");
    let next = media("youtube-music", "radio-next");
    let (state, attempt, radio_generation) = state_with_single_radio_item(Config::default(), seed);
    let (state, _) = reduce(
        state,
        Action::PlayerEnded {
            generation: attempt,
        },
    );

    let (state, _) = reduce(
        state,
        Action::SearchSubmitted {
            query: "replacement".to_owned(),
            filter: SearchFilter::Songs,
        },
    );
    let search_generation = state.search().generation();
    let (state, _) = reduce(
        state,
        Action::SearchCompleted {
            generation: search_generation,
            result: Ok(page(vec![replacement.clone()])),
        },
    );
    let (state, _) = reduce(state, Action::ActivateSearchResult { index: 0 });
    let replacement_attempt = current_attempt_generation(&state);

    let (state, effects) = reduce(
        state,
        Action::RadioFillCompleted {
            generation: radio_generation,
            result: Ok(vec![next]),
        },
    );

    assert_eq!(state.queue().items().len(), 3);
    assert_eq!(
        state.queue().current().map(QueueItem::media),
        Some(&replacement)
    );
    assert_eq!(state.playback().status, PlaybackStatus::Resolving);
    assert_eq!(
        state.current_attempt_generation(),
        Some(replacement_attempt)
    );
    assert_eq!(effects, vec![Effect::Persist(checkpoint(&state))]);
}

#[test]
fn failed_tail_without_auto_skip_appends_radio_items_but_does_not_resume() {
    let seed = media("youtube-music", "radio-seed");
    let next = media("youtube-music", "radio-next");
    let (state, attempt, radio_generation) =
        state_with_single_radio_item(config_without_lyrics(), seed.clone());

    let (state, effects) = reduce(
        state,
        Action::ResolveFailed {
            generation: attempt,
            error: AppError::new(AppErrorCategory::Resolve, "Unavailable"),
        },
    );
    assert_eq!(state.playback().status, PlaybackStatus::Failed);
    assert_eq!(effects, vec![Effect::Persist(checkpoint(&state))]);

    let (state, effects) = reduce(
        state,
        Action::RadioFillCompleted {
            generation: radio_generation,
            result: Ok(vec![next]),
        },
    );

    assert_eq!(state.queue().items().len(), 2);
    assert_eq!(state.queue().current().map(QueueItem::media), Some(&seed));
    assert_eq!(state.playback().status, PlaybackStatus::Failed);
    assert!(state.current_attempt_generation().is_none());
    assert!(state.current_resolve_generation().is_none());
    assert_eq!(effects, vec![Effect::Persist(checkpoint(&state))]);
}

#[test]
fn auto_skip_failure_at_tail_requests_radio_after_empty_fill() {
    let mut config = config_without_lyrics();
    config.behavior.auto_skip_unavailable = true;
    let seed = media("youtube-music", "radio-seed");
    let (state, attempt, first_fill) = state_with_single_radio_item(config, seed.clone());
    let (state, effects) = reduce(
        state,
        Action::RadioFillCompleted {
            generation: first_fill,
            result: Ok(Vec::new()),
        },
    );
    assert!(effects.is_empty());
    assert!(state.pending_radio_generation().is_none());

    let (state, effects) = reduce(
        state,
        Action::ResolveFailed {
            generation: attempt,
            error: AppError::new(AppErrorCategory::Resolve, "Unavailable"),
        },
    );
    let Some(next_fill) = state.pending_radio_generation() else {
        panic!("failed tail must request another radio fill");
    };

    assert_eq!(state.playback().status, PlaybackStatus::Failed);
    assert!(state.current_attempt_generation().is_none());
    assert_eq!(
        effects,
        vec![
            Effect::Persist(checkpoint(&state)),
            Effect::FillRadio {
                generation: next_fill,
                seed: seed.id,
            },
        ]
    );
}

#[test]
fn pending_radio_fill_is_retained_and_resumes_failed_tail() {
    let mut config = config_without_lyrics();
    config.behavior.auto_skip_unavailable = true;
    let seed = media("youtube-music", "radio-seed");
    let next = media("youtube-music", "radio-next");
    let (state, attempt, radio_generation) = state_with_single_radio_item(config, seed);

    let (state, effects) = reduce(
        state,
        Action::ResolveFailed {
            generation: attempt,
            error: AppError::new(AppErrorCategory::Resolve, "Unavailable"),
        },
    );
    assert_eq!(state.playback().status, PlaybackStatus::Failed);
    assert_eq!(state.pending_radio_generation(), Some(radio_generation));
    assert_eq!(effects, vec![Effect::Persist(checkpoint(&state))]);

    let (state, effects) = reduce(
        state,
        Action::RadioFillCompleted {
            generation: radio_generation,
            result: Ok(vec![next.clone()]),
        },
    );
    let next_attempt = current_attempt_generation(&state);
    assert_eq!(state.queue().current().map(QueueItem::media), Some(&next));
    assert_eq!(
        effects,
        vec![
            Effect::Resolve {
                generation: next_attempt,
                item: next,
                start_ms: None,
            },
            Effect::Persist(checkpoint(&state)),
        ]
    );
}

#[test]
fn radio_fill_error_does_not_persist_or_mutate_queue() {
    let seed = media("youtube-music", "radio-seed");
    let (state, _, radio_generation) = state_with_single_radio_item(Config::default(), seed);
    let expected_queue = state.queue().snapshot();

    let (state, effects) = reduce(
        state,
        Action::RadioFillCompleted {
            generation: radio_generation,
            result: Err(AppError::new(AppErrorCategory::Radio, "Radio unavailable")),
        },
    );

    assert!(effects.is_empty());
    assert_eq!(state.queue().snapshot(), expected_queue);
    assert!(state.pending_radio_generation().is_none());
    let Some(diagnostic) = state.diagnostics().last() else {
        panic!("radio fill error must add a diagnostic");
    };
    assert_eq!(diagnostic.category(), DiagnosticCategory::Radio);
}

#[test]
fn empty_radio_fill_does_not_persist_or_mutate_queue() {
    let seed = media("youtube-music", "radio-seed");
    let (state, _, radio_generation) = state_with_single_radio_item(Config::default(), seed);
    let expected_queue = state.queue().snapshot();

    let (state, effects) = reduce(
        state,
        Action::RadioFillCompleted {
            generation: radio_generation,
            result: Ok(Vec::new()),
        },
    );

    assert!(effects.is_empty());
    assert_eq!(state.queue().snapshot(), expected_queue);
    assert!(state.pending_radio_generation().is_none());
}

#[test]
fn all_duplicate_radio_fill_does_not_persist_or_mutate_queue() {
    let seed = media("youtube-music", "radio-seed");
    let (state, _, radio_generation) =
        state_with_single_radio_item(Config::default(), seed.clone());
    let expected_queue = state.queue().snapshot();

    let (state, effects) = reduce(
        state,
        Action::RadioFillCompleted {
            generation: radio_generation,
            result: Ok(vec![seed]),
        },
    );

    assert!(effects.is_empty());
    assert_eq!(state.queue().snapshot(), expected_queue);
    assert!(state.pending_radio_generation().is_none());
}

#[test]
fn duplicate_activation_resolves_canonical_queue_media() {
    let mut canonical = media("youtube-music", "canonical");
    canonical.title = "Original title".to_owned();
    canonical.duration_ms = Some(111_000);
    canonical.artwork_url = Some(url("https://images.example.test/original.jpg"));
    let state = state_with_results(
        config_without_lyrics(),
        vec![SearchItem::Playable(canonical.clone())],
    );
    let state = with_artwork_surface(state, ArtworkSurface::Search);
    let (state, _) = reduce(state, Action::ActivateSearchResult { index: 0 });

    let mut refreshed = canonical.clone();
    refreshed.title = "Refreshed title".to_owned();
    refreshed.duration_ms = Some(999_000);
    refreshed.artwork_url = Some(url("https://images.example.test/refreshed.jpg"));
    let (state, _) = reduce(
        state,
        Action::SearchSubmitted {
            query: "refreshed".to_owned(),
            filter: SearchFilter::Songs,
        },
    );
    let search_generation = state.search().generation();
    let (state, _) = reduce(
        state,
        Action::SearchCompleted {
            generation: search_generation,
            result: Ok(page(vec![refreshed])),
        },
    );

    let (state, effects) = reduce(state, Action::ActivateSearchResult { index: 0 });
    let generation = current_attempt_generation(&state);

    assert_eq!(state.queue().items().len(), 1);
    assert_eq!(
        state.queue().current().map(QueueItem::media),
        Some(&canonical)
    );
    assert_eq!(state.playback().current.as_ref(), Some(&canonical.id));
    assert_eq!(state.playback().duration_ms, canonical.duration_ms);
    let canonical_artwork = artwork_url("https://images.example.test/original.jpg");
    assert_eq!(
        effects,
        vec![
            Effect::Resolve {
                generation,
                item: canonical,
                start_ms: None,
            },
            Effect::FetchArtwork {
                generation: state.artwork().generation(),
                url: canonical_artwork,
            },
            Effect::Persist(checkpoint(&state)),
        ]
    );
}

#[test]
fn invalid_or_nonplayable_search_selection_adds_diagnostic_without_panicking() {
    let metadata = SearchMetadata::new(SearchMetadataKind::Album, "Metadata only");
    let state = state_with_results(
        Config::default(),
        vec![
            SearchItem::Metadata(metadata),
            SearchItem::Playable(media("youtube-music", "playable")),
        ],
    );

    let (state, effects) = reduce(state, Action::ActivateSearchResult { index: 99 });
    assert!(effects.is_empty());
    assert!(state.queue().items().is_empty());
    assert_eq!(state.diagnostics().len(), 1);
    assert_eq!(
        state.diagnostics()[0].category(),
        DiagnosticCategory::Selection
    );

    let (state, effects) = reduce(state, Action::ActivateSearchResult { index: 0 });
    assert!(effects.is_empty());
    assert!(state.queue().items().is_empty());
    assert_eq!(state.diagnostics().len(), 2);
    assert_eq!(
        state.diagnostics()[1].category(),
        DiagnosticCategory::Selection
    );
}

#[test]
fn charts_request_allocates_generation_marks_loading_and_emits_effect() {
    let requested_region = region("hk");

    let (state, effects) = reduce(
        AppState::default(),
        Action::ChartsRequested {
            region: requested_region.clone(),
        },
    );
    let generation = Generation::new(1);

    assert_eq!(
        effects,
        vec![
            Effect::ReadChartCache {
                generation,
                region: requested_region.clone(),
                key: ChartCacheKey::new(requested_region.clone()),
            },
            Effect::LoadCharts {
                generation,
                region: requested_region.clone(),
            },
        ]
    );
    assert_eq!(state.charts().region(), Some(&requested_region));
    assert_eq!(state.charts().generation(), generation);
    assert_eq!(state.charts().active_generation(), Some(generation));
    assert!(state.charts().loading());
    assert!(state.charts().sections().is_empty());
    assert!(state.charts().error().is_none());
}

#[test]
fn current_charts_success_updates_normalized_sections() {
    let requested_region = region("us");
    let (state, _) = reduce(
        AppState::default(),
        Action::ChartsRequested {
            region: requested_region.clone(),
        },
    );
    let generation = state.charts().generation();
    let section = ChartSection::new("Top songs", vec![media("youtube-music", "chart-current")]);

    let (state, effects) = reduce(
        state,
        Action::ChartsCompleted {
            generation,
            region: requested_region.clone(),
            received_at: 1_000,
            result: Ok(vec![section.clone()]),
        },
    );

    let [Effect::StoreChartCache { key, payload }] = effects.as_slice() else {
        panic!("live chart success must emit a cache store");
    };
    assert_eq!(key, &ChartCacheKey::new(requested_region));
    assert_eq!(payload.sections(), std::slice::from_ref(&section));
    assert_eq!(state.charts().sections(), &[section]);
    assert!(!state.charts().loading());
    assert!(state.charts().active_generation().is_none());
    assert!(state.charts().error().is_none());
}

#[test]
fn live_chart_refresh_preserves_duplicate_occurrence_within_its_section() {
    let requested_region = region("kr");
    let duplicate = media("youtube-music", "duplicate-chart-row");
    let initial_sections = vec![
        ChartSection::new(
            "Daily",
            vec![media("youtube-music", "daily-before"), duplicate.clone()],
        ),
        ChartSection::new(
            "Top 100",
            vec![media("youtube-music", "top-before"), duplicate.clone()],
        ),
    ];
    let (state, _) = reduce(
        AppState::default(),
        Action::ChartsRequested {
            region: requested_region.clone(),
        },
    );
    let generation = state.charts().generation();
    let state = reduce(
        state,
        Action::ChartsCompleted {
            generation,
            region: requested_region.clone(),
            received_at: 1_000,
            result: Ok(initial_sections),
        },
    )
    .0;
    let state = reduce(state, Action::ChartRowSelectionChanged { item_index: 3 }).0;

    let (state, _) = reduce(
        state,
        Action::ChartsRequested {
            region: requested_region.clone(),
        },
    );
    let generation = state.charts().generation();
    let refreshed_sections = vec![
        ChartSection::new(
            "Daily",
            vec![
                media("youtube-music", "daily-new-a"),
                media("youtube-music", "daily-new-b"),
                media("youtube-music", "daily-new-c"),
                duplicate.clone(),
            ],
        ),
        ChartSection::new(
            "Top 100",
            vec![media("youtube-music", "top-before"), duplicate.clone()],
        ),
    ];
    let state = reduce(
        state,
        Action::ChartsCompleted {
            generation,
            region: requested_region,
            received_at: 2_000,
            result: Ok(refreshed_sections),
        },
    )
    .0;

    assert_eq!(state.charts().selected_id(), Some(&duplicate.id));
    assert_eq!(state.charts().selected_index(), Some(5));

    let requested_region = region("kr");
    let (state, _) = reduce(
        state,
        Action::ChartsRequested {
            region: requested_region.clone(),
        },
    );
    let generation = state.charts().generation();
    let state = reduce(
        state,
        Action::ChartsCompleted {
            generation,
            region: requested_region,
            received_at: 3_000,
            result: Ok(vec![
                ChartSection::new(
                    "Daily",
                    vec![
                        media("youtube-music", "daily-new-a"),
                        media("youtube-music", "daily-new-b"),
                        media("youtube-music", "daily-new-c"),
                        duplicate.clone(),
                    ],
                ),
                ChartSection::new("Top 100", vec![media("youtube-music", "top-before")]),
            ]),
        },
    )
    .0;
    assert_eq!(state.charts().selected_id(), Some(&duplicate.id));
    assert_eq!(state.charts().selected_index(), Some(3));
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one cache/live fallback trace keeps duplicate occurrence transitions explicit"
)]
fn cached_chart_refresh_preserves_duplicate_occurrence_within_its_section() {
    let requested_region = region("kr");
    let duplicate = media("youtube-music", "duplicate-cached-chart-row");
    let initial_sections = vec![
        ChartSection::new(
            "Daily",
            vec![
                media("youtube-music", "cached-daily-before"),
                duplicate.clone(),
            ],
        ),
        ChartSection::new(
            "Top 100",
            vec![
                media("youtube-music", "cached-top-before"),
                duplicate.clone(),
            ],
        ),
    ];
    let (state, _) = reduce(
        AppState::default(),
        Action::ChartsRequested {
            region: requested_region.clone(),
        },
    );
    let generation = state.charts().generation();
    let state = reduce(
        state,
        Action::ChartsCompleted {
            generation,
            region: requested_region.clone(),
            received_at: 1_000,
            result: Ok(initial_sections),
        },
    )
    .0;
    let state = reduce(state, Action::ChartRowSelectionChanged { item_index: 3 }).0;

    let (state, _) = reduce(
        state,
        Action::ChartsRequested {
            region: requested_region.clone(),
        },
    );
    let generation = state.charts().generation();
    let cached_at = 2_000;
    let cached_sections = vec![
        ChartSection::new(
            "Daily",
            vec![
                media("youtube-music", "cached-daily-new-a"),
                media("youtube-music", "cached-daily-new-b"),
                media("youtube-music", "cached-daily-new-c"),
                duplicate.clone(),
            ],
        ),
        ChartSection::new(
            "Top 100",
            vec![
                media("youtube-music", "cached-top-before"),
                duplicate.clone(),
            ],
        ),
    ];
    let state = reduce(
        state,
        Action::CachedChartsCompleted {
            generation,
            region: requested_region.clone(),
            observed_at: cached_at,
            result: Ok(Some(
                ChartCachePayload::try_new(
                    requested_region.clone(),
                    cached_sections,
                    cached_at,
                    cached_at + 3_600,
                )
                .unwrap_or_else(|error| panic!("valid chart cache: {}", error.message())),
            )),
        },
    )
    .0;
    let state = reduce(
        state,
        Action::ChartsCompleted {
            generation,
            region: requested_region,
            received_at: cached_at,
            result: Err(AppError::new(AppErrorCategory::Charts, "offline")),
        },
    )
    .0;

    assert_eq!(state.charts().selected_id(), Some(&duplicate.id));
    assert_eq!(state.charts().selected_index(), Some(5));

    let requested_region = region("kr");
    let (state, _) = reduce(
        state,
        Action::ChartsRequested {
            region: requested_region.clone(),
        },
    );
    let generation = state.charts().generation();
    let cached_at = 3_000;
    let state = reduce(
        state,
        Action::CachedChartsCompleted {
            generation,
            region: requested_region.clone(),
            observed_at: cached_at,
            result: Ok(Some(
                ChartCachePayload::try_new(
                    requested_region.clone(),
                    vec![
                        ChartSection::new(
                            "Daily",
                            vec![
                                media("youtube-music", "cached-daily-new-a"),
                                media("youtube-music", "cached-daily-new-b"),
                                media("youtube-music", "cached-daily-new-c"),
                                duplicate.clone(),
                            ],
                        ),
                        ChartSection::new(
                            "Top 100",
                            vec![media("youtube-music", "cached-top-before")],
                        ),
                    ],
                    cached_at,
                    cached_at + 3_600,
                )
                .unwrap_or_else(|error| panic!("valid chart cache: {}", error.message())),
            )),
        },
    )
    .0;
    let state = reduce(
        state,
        Action::ChartsCompleted {
            generation,
            region: requested_region,
            received_at: cached_at,
            result: Err(AppError::new(AppErrorCategory::Charts, "offline")),
        },
    )
    .0;
    assert_eq!(state.charts().selected_id(), Some(&duplicate.id));
    assert_eq!(state.charts().selected_index(), Some(3));
}

#[test]
fn current_charts_error_is_visible_and_stops_loading() {
    let requested_region = region("gb");
    let (state, _) = reduce(
        AppState::default(),
        Action::ChartsRequested {
            region: requested_region.clone(),
        },
    );
    let generation = state.charts().generation();
    let error = AppError::new(
        AppErrorCategory::Charts,
        "Charts are temporarily unavailable",
    );

    let (state, effects) = reduce(
        state,
        Action::ChartsCompleted {
            generation,
            region: requested_region.clone(),
            received_at: 1_000,
            result: Err(error.clone()),
        },
    );

    assert!(effects.is_empty());
    assert!(state.charts().sections().is_empty());
    assert!(state.charts().loading());
    assert!(state.charts().error().is_none());
    let (state, effects) = reduce(
        state,
        Action::CachedChartsCompleted {
            generation,
            region: requested_region,
            observed_at: 1_000,
            result: Ok(None),
        },
    );
    assert!(effects.is_empty());
    assert!(!state.charts().loading());
    assert!(state.charts().active_generation().is_none());
    assert_eq!(state.charts().error(), Some(&error));
}

#[test]
fn superseded_charts_ignore_stale_success_and_error_entirely() {
    let stale_region = region("hk");
    let (state, _) = reduce(
        AppState::default(),
        Action::ChartsRequested {
            region: stale_region.clone(),
        },
    );
    let stale_generation = state.charts().generation();
    let current_region = region("us");
    let (state, _) = reduce(
        state,
        Action::ChartsRequested {
            region: current_region.clone(),
        },
    );
    let expected = state.clone();

    let (state, effects) = reduce(
        state,
        Action::ChartsCompleted {
            generation: stale_generation,
            region: stale_region.clone(),
            received_at: 1_000,
            result: Ok(vec![ChartSection::new(
                "Old charts",
                vec![media("youtube-music", "old-chart")],
            )]),
        },
    );
    assert_eq!(state, expected);
    assert!(effects.is_empty());

    let (state, effects) = reduce(
        state,
        Action::ChartsCompleted {
            generation: stale_generation,
            region: stale_region,
            received_at: 1_001,
            result: Err(AppError::new(AppErrorCategory::Charts, "Old failure")),
        },
    );
    assert_eq!(state, expected);
    assert!(effects.is_empty());
    assert_eq!(state.charts().region(), Some(&current_region));
    assert!(state.charts().loading());
}

#[test]
fn new_attempts_and_stopped_status_clear_stale_player_presentation() {
    let first = media("youtube-music", "presentation-first");
    let second = media("youtube-music", "presentation-second");
    let state = state_with_results(
        Config::default(),
        vec![SearchItem::Playable(first), SearchItem::Playable(second)],
    );
    let (state, _) = reduce(state, Action::ActivateSearchResult { index: 0 });
    let first_generation = current_attempt_generation(&state);
    let (state, _) = reduce(
        state,
        Action::ResolvedFormatUpdated {
            generation: first_generation,
            quality: ytermusic::app::ResolverQuality::new(Some("opus"), Some("251")),
        },
    );
    let (state, _) = reduce(
        state,
        Action::PreviewStreamUpdated {
            generation: first_generation,
            preview_url: Some(preview_url("https://video.invalid/first")),
        },
    );
    let (state, _) = reduce(
        state,
        Action::PlaybackTelemetryUpdated {
            generation: first_generation,
            effective_volume: 61.0,
            fade: Some(ytermusic::app::FadeActivity::In),
        },
    );
    assert!(state.player_presentation().quality().known());
    assert_eq!(
        state
            .player_presentation()
            .preview_url()
            .map(PreviewStreamUrl::as_url),
        Some(
            &Url::parse("https://video.invalid/first")
                .unwrap_or_else(|error| { panic!("test preview URL should parse: {error}") })
        )
    );

    let (state, _) = reduce(state, Action::ActivateSearchResult { index: 1 });
    assert!(!state.player_presentation().quality().known());
    assert!(state.player_presentation().preview_url().is_none());
    assert_eq!(state.player_presentation().fade(), None);
    assert!(state.player_presentation().effective_volume().abs() <= f64::EPSILON);
    let second_generation = current_attempt_generation(&state);

    let (state, _) = reduce(
        state,
        Action::ResolvedFormatUpdated {
            generation: second_generation,
            quality: ytermusic::app::ResolverQuality::new(Some("aac"), Some("140")),
        },
    );
    let (state, _) = reduce(
        state,
        Action::PreviewStreamUpdated {
            generation: second_generation,
            preview_url: Some(preview_url("https://video.invalid/second")),
        },
    );
    let (state, _) = reduce(
        state,
        Action::PlaybackTelemetryUpdated {
            generation: second_generation,
            effective_volume: 44.0,
            fade: Some(ytermusic::app::FadeActivity::Out),
        },
    );
    let (state, _) = reduce(
        state,
        Action::PlayerStatusChanged {
            generation: second_generation,
            status: PlaybackStatus::Stopped,
        },
    );
    assert!(!state.player_presentation().quality().known());
    assert!(state.player_presentation().preview_url().is_none());
    assert_eq!(state.player_presentation().fade(), None);
    assert!(state.player_presentation().effective_volume().abs() <= f64::EPSILON);
}

#[test]
fn analysis_stream_is_generation_scoped_redacted_and_cleared_by_every_terminal_lifecycle() {
    let first = media("youtube-music", "analysis-first");
    let second = media("youtube-music", "analysis-second");
    let state = state_with_results(
        Config::default(),
        vec![SearchItem::Playable(first), SearchItem::Playable(second)],
    );
    let (state, _) = reduce(state, Action::ActivateSearchResult { index: 0 });
    let first_generation = current_attempt_generation(&state);
    let secret = "https://media.invalid/audio?token=transient-analysis-secret";
    let update = Action::AnalysisStreamUpdated {
        generation: first_generation,
        stream_url: Some(analysis_url(secret)),
    };
    assert!(!format!("{update:?}").contains("transient-analysis-secret"));
    let (state, _) = reduce(state, update);
    assert_eq!(
        state
            .player_presentation()
            .analysis_url()
            .map(AnalysisStreamUrl::as_url)
            .map(Url::as_str),
        Some(secret)
    );
    assert!(!format!("{state:?}").contains("transient-analysis-secret"));
    let session_json = serde_json::to_string(&checkpoint(&state))
        .unwrap_or_else(|error| panic!("session checkpoint should serialize: {error}"));
    let queue_json = serde_json::to_string(&state.queue().snapshot())
        .unwrap_or_else(|error| panic!("queue snapshot should serialize: {error}"));
    for durable_payload in [session_json, queue_json] {
        assert!(!durable_payload.contains("transient-analysis-secret"));
        assert!(!durable_payload.contains("media.invalid"));
    }

    let stale_generation = Generation::new(first_generation.value().saturating_add(100));
    let (state, _) = reduce(
        state,
        Action::AnalysisStreamUpdated {
            generation: stale_generation,
            stream_url: None,
        },
    );
    assert!(state.player_presentation().analysis_url().is_some());

    let (state, _) = reduce(
        state,
        Action::AnalysisStreamUpdated {
            generation: first_generation,
            stream_url: None,
        },
    );
    assert!(state.player_presentation().analysis_url().is_none());
    let (state, _) = reduce(
        state,
        Action::AnalysisStreamUpdated {
            generation: first_generation,
            stream_url: Some(analysis_url(secret)),
        },
    );
    let (state, _) = reduce(state, Action::ActivateSearchResult { index: 1 });
    assert!(state.player_presentation().analysis_url().is_none());

    let second_generation = current_attempt_generation(&state);
    let (state, _) = reduce(
        state,
        Action::AnalysisStreamUpdated {
            generation: second_generation,
            stream_url: Some(analysis_url(secret)),
        },
    );
    let (state, _) = reduce(
        state,
        Action::PlayerStatusChanged {
            generation: second_generation,
            status: PlaybackStatus::Stopped,
        },
    );
    assert!(state.player_presentation().analysis_url().is_none());

    // An explicit resolver failure closes the generation and must not retain
    // an in-memory stream URL.
    let (state, _) = reduce(state, Action::TogglePlayback);
    let resolve_generation = current_attempt_generation(&state);
    let (state, _) = reduce(
        state,
        Action::AnalysisStreamUpdated {
            generation: resolve_generation,
            stream_url: Some(analysis_url(secret)),
        },
    );
    let (state, _) = reduce(
        state,
        Action::ResolveFailed {
            generation: resolve_generation,
            error: AppError::new(AppErrorCategory::Resolve, "analysis resolve failure"),
        },
    );
    assert!(state.player_presentation().analysis_url().is_none());
}

#[test]
fn failed_player_status_clears_transient_analysis_stream() {
    let state = state_with_results(
        Config::default(),
        vec![SearchItem::Playable(media(
            "youtube-music",
            "analysis-player-failure",
        ))],
    );
    let (state, _) = reduce(state, Action::ActivateSearchResult { index: 0 });
    let generation = current_attempt_generation(&state);
    let state = with_analysis(state, generation, "https://media.invalid/player-failure");
    let (state, _) = reduce(
        state,
        Action::PlayerStatusChanged {
            generation,
            status: PlaybackStatus::Failed,
        },
    );
    assert!(state.player_presentation().analysis_url().is_none());
}

#[test]
fn matched_failed_status_freezes_presentation_rejects_late_telemetry_and_next_attempt_resets() {
    let mut failed = media("youtube-music", "presentation-failed");
    failed.kind = MediaKind::PodcastEpisode;
    let replacement = media("youtube-music", "presentation-replacement");
    let state = state_with_results(
        Config::default(),
        vec![
            SearchItem::Playable(failed.clone()),
            SearchItem::Playable(replacement),
        ],
    );
    let (state, effects) = reduce(state, Action::ActivateSearchResult { index: 0 });
    let progress_generation = assert_progress_load_before_resolution(&effects, &failed.id);
    let (state, _) = reduce(
        state,
        Action::PodcastProgressLoaded {
            generation: progress_generation,
            progress: None,
        },
    );
    let generation = current_attempt_generation(&state);
    let (state, _) = reduce(
        state,
        Action::PlayerProgress {
            generation,
            media_id: failed.id.clone(),
            position_ms: 65_000,
            duration_ms: failed.duration_ms,
        },
    );
    let quality = ytermusic::app::ResolverQuality::new(Some("opus"), Some("251"));
    let (state, _) = reduce(
        state,
        Action::ResolvedFormatUpdated {
            generation,
            quality: quality.clone(),
        },
    );
    let state = with_preview(state, generation, "https://video.invalid/failed");
    let (state, _) = reduce(
        state,
        Action::PlaybackTelemetryUpdated {
            generation,
            effective_volume: 44.0,
            fade: Some(ytermusic::app::FadeActivity::Out),
        },
    );

    let (state, effects) = reduce(
        state,
        Action::PlayerStatusChanged {
            generation,
            status: PlaybackStatus::Failed,
        },
    );

    assert_eq!(state.playback().status, PlaybackStatus::Failed);
    assert!(state.current_attempt_generation().is_none());
    assert!(state.current_resolve_generation().is_none());
    assert_eq!(state.player_presentation().quality(), &quality);
    assert!(state.player_presentation().preview_url().is_none());
    assert_eq!(
        state.player_presentation().fade(),
        Some(ytermusic::app::FadeActivity::Out)
    );
    assert!((state.player_presentation().effective_volume() - 44.0).abs() <= f64::EPSILON);
    assert!(matches!(
        effects.as_slice(),
        [
            Effect::SavePodcastProgress(checkpoint),
            Effect::Persist(_)
        ] if checkpoint.media_id() == &failed.id && !checkpoint.played()
    ));
    assert!(effects.iter().any(|effect| {
        matches!(
            effect,
            Effect::SavePodcastProgress(checkpoint)
                if checkpoint.media_id() == &failed.id
                    && checkpoint.position_ms() == 65_000
                    && !checkpoint.played()
        )
    }));

    let frozen = state.clone();
    let (state, effects) = reduce(
        state,
        Action::PlaybackTelemetryUpdated {
            generation,
            effective_volume: 1.0,
            fade: Some(ytermusic::app::FadeActivity::In),
        },
    );
    assert_eq!(state, frozen);
    assert!(effects.is_empty());

    let (state, effects) = reduce(state, Action::ActivateSearchResult { index: 1 });
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, Effect::Resolve { .. }))
    );
    assert!(state.current_attempt_generation().is_some());
    assert!(!state.player_presentation().quality().known());
    assert_eq!(state.player_presentation().fade(), None);
    assert!(state.player_presentation().effective_volume().abs() <= f64::EPSILON);
}

#[test]
fn artwork_request_allocates_generation_marks_loading_and_emits_effect() {
    let url = artwork_url("https://images.example.test/cover.jpg");

    let (state, effects) = reduce(
        AppState::default(),
        Action::ArtworkRequested { url: url.clone() },
    );
    let generation = Generation::new(1);

    assert_eq!(
        effects,
        vec![Effect::FetchArtwork {
            generation,
            url: url.clone(),
        }]
    );
    assert_eq!(state.artwork().requested_url(), Some(&url));
    assert!(state.artwork().ready_url().is_none());
    assert_eq!(state.artwork().generation(), generation);
    assert_eq!(state.artwork().active_generation(), Some(generation));
    assert!(state.artwork().loading());
    assert!(state.artwork().error().is_none());
}

#[test]
fn current_artwork_success_marks_requested_url_ready() {
    let url = artwork_url("https://images.example.test/current.jpg");
    let (state, _) = reduce(
        AppState::default(),
        Action::ArtworkRequested { url: url.clone() },
    );
    let generation = state.artwork().generation();

    let (state, effects) = reduce(
        state,
        Action::ArtworkCompleted {
            generation,
            result: Ok(()),
        },
    );

    assert!(effects.is_empty());
    assert_eq!(state.artwork().requested_url(), Some(&url));
    assert_eq!(state.artwork().ready_url(), Some(&url));
    assert!(!state.artwork().loading());
    assert!(state.artwork().active_generation().is_none());
    assert!(state.artwork().error().is_none());
}

#[test]
fn current_artwork_error_is_visible_and_stops_loading() {
    let (state, _) = reduce(
        AppState::default(),
        Action::ArtworkRequested {
            url: artwork_url("https://images.example.test/error.jpg"),
        },
    );
    let generation = state.artwork().generation();
    let error = AppError::new(AppErrorCategory::Artwork, "Artwork could not be loaded");

    let (state, effects) = reduce(
        state,
        Action::ArtworkCompleted {
            generation,
            result: Err(error.clone()),
        },
    );

    assert!(effects.is_empty());
    assert!(state.artwork().ready_url().is_none());
    assert!(!state.artwork().loading());
    assert!(state.artwork().active_generation().is_none());
    assert_eq!(state.artwork().error(), Some(&error));
}

#[test]
fn superseded_artwork_ignores_stale_success_and_error_entirely() {
    let (state, _) = reduce(
        AppState::default(),
        Action::ArtworkRequested {
            url: artwork_url("https://images.example.test/old.jpg"),
        },
    );
    let stale_generation = state.artwork().generation();
    let current_url = artwork_url("https://images.example.test/current.jpg");
    let (state, _) = reduce(
        state,
        Action::ArtworkRequested {
            url: current_url.clone(),
        },
    );
    let expected = state.clone();

    let (state, effects) = reduce(
        state,
        Action::ArtworkCompleted {
            generation: stale_generation,
            result: Ok(()),
        },
    );
    assert_eq!(state, expected);
    assert!(effects.is_empty());

    let (state, effects) = reduce(
        state,
        Action::ArtworkCompleted {
            generation: stale_generation,
            result: Err(AppError::new(AppErrorCategory::Artwork, "Old failure")),
        },
    );
    assert_eq!(state, expected);
    assert!(effects.is_empty());
    assert_eq!(state.artwork().requested_url(), Some(&current_url));
    assert!(state.artwork().loading());
}

#[test]
fn selecting_playable_search_result_automatically_requests_its_artwork() {
    let first = media("youtube-music", "first-artwork");
    let mut second = media("youtube-music", "second-artwork");
    let expected_url = artwork_url("https://images.example.test/second.jpg");
    second.artwork_url = Some(expected_url.as_url().clone());
    let selected_id = SearchItem::Playable(second.clone()).stable_id();
    let state = state_with_results(
        Config::default(),
        vec![SearchItem::Playable(first), SearchItem::Playable(second)],
    );
    let state = with_artwork_surface(state, ArtworkSurface::Search);

    let (state, effects) = reduce(state, Action::SearchSelectionChanged { id: selected_id });

    assert_eq!(
        effects,
        vec![Effect::FetchArtwork {
            generation: state.artwork().generation(),
            url: expected_url.clone(),
        }]
    );
    assert_eq!(state.artwork().requested_url(), Some(&expected_url));
}

#[test]
fn starting_playback_automatically_requests_current_media_artwork() {
    let mut item = media("youtube-music", "playback-artwork");
    let expected_url = artwork_url("https://images.example.test/playback.jpg");
    item.artwork_url = Some(expected_url.as_url().clone());
    let queue_id = stable_queue_item_id(&item.id);
    let (state, enqueue_effects) = reduce(AppState::default(), Action::EnqueueMedia { item });
    assert!(enqueue_effects.iter().any(|effect| {
        matches!(
            effect,
            Effect::FetchArtwork { url, .. } if url == &expected_url
        )
    }));

    let (state, effects) = reduce(state, Action::PlayQueueItem { id: queue_id });

    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, Effect::Resolve { .. }))
    );
    assert!(
        !effects
            .iter()
            .any(|effect| matches!(effect, Effect::FetchArtwork { .. })),
        "the same artwork URL was fetched twice"
    );
    assert_eq!(state.artwork().requested_url(), Some(&expected_url));
}

#[test]
fn chart_auto_selection_synchronizes_its_artwork() {
    let mut selected = media("youtube-music", "chart-artwork");
    let expected_url = artwork_url("https://images.example.test/chart-auto.jpg");
    selected.artwork_url = Some(expected_url.as_url().clone());
    let hk = region("hk");
    let (state, effects) = reduce(
        with_artwork_surface(AppState::default(), ArtworkSurface::Charts),
        Action::ChartsRequested { region: hk.clone() },
    );
    let generation = loaded_chart_generation(&effects);

    let (state, effects) = reduce(
        state,
        Action::ChartsCompleted {
            generation,
            region: hk,
            received_at: 1_000,
            result: Ok(vec![ChartSection::new("Top", vec![selected])]),
        },
    );

    assert!(
        effects.iter().any(|effect| {
            matches!(
                effect,
                Effect::FetchArtwork { url, .. } if url == &expected_url
            )
        }),
        "chart auto-selection did not synchronize artwork"
    );
    assert_eq!(state.artwork().requested_url(), Some(&expected_url));
}

#[test]
fn chart_artwork_sync_deduplicates_and_clears_art_to_no_art() {
    let mut first = media("youtube-music", "chart-with-art");
    let first_url = artwork_url("https://images.example.test/chart-with-art.jpg");
    first.artwork_url = Some(first_url.as_url().clone());
    let second = media("youtube-music", "chart-without-art");
    let hk = region("hk");
    let (state, effects) = reduce(
        with_artwork_surface(AppState::default(), ArtworkSurface::Charts),
        Action::ChartsRequested { region: hk.clone() },
    );
    let generation = loaded_chart_generation(&effects);
    let (state, _) = reduce(
        state,
        Action::ChartsCompleted {
            generation,
            region: hk,
            received_at: 1_000,
            result: Ok(vec![ChartSection::new(
                "Top",
                vec![first.clone(), second.clone()],
            )]),
        },
    );

    let (state, duplicate_effects) =
        reduce(state, Action::ChartSelectionChanged { media_id: first.id });
    assert!(
        duplicate_effects.is_empty(),
        "same-URL chart selection requested duplicate artwork"
    );

    let (state, clear_effects) = reduce(
        state,
        Action::ChartSelectionChanged {
            media_id: second.id.clone(),
        },
    );
    assert_eq!(clear_effects, vec![Effect::ClearArtwork]);
    assert!(state.artwork().requested_url().is_none());
    assert!(state.artwork().ready_url().is_none());
    assert!(state.artwork().active_generation().is_none());
    assert!(!state.artwork().loading());

    let expected = state.clone();
    let (state, repeated_clear) = reduce(
        state,
        Action::ChartSelectionChanged {
            media_id: second.id,
        },
    );
    assert!(repeated_clear.is_empty());
    assert_eq!(state, expected);

    let (state, invalid_effects) = reduce(
        state,
        Action::ChartSelectionChanged {
            media_id: MediaId {
                provider: "youtube-music".to_owned(),
                video_id: "missing-chart-item".to_owned(),
            },
        },
    );
    assert!(invalid_effects.is_empty());
    assert_eq!(state, expected);
}

#[test]
fn playback_identity_sync_orders_resolve_before_artwork_clear() {
    let mut first = media("youtube-music", "queue-with-art");
    first.artwork_url = Some(url("https://images.example.test/queue-with-art.jpg"));
    let second = media("youtube-music", "queue-without-art");
    let second_id = stable_queue_item_id(&second.id);
    let (state, _) = reduce(AppState::default(), Action::EnqueueMedia { item: first });
    let (state, _) = reduce(state, Action::EnqueueMedia { item: second });

    let (state, effects) = reduce(state, Action::PlayQueueItem { id: second_id });

    let Some(resolve_index) = effects
        .iter()
        .position(|effect| matches!(effect, Effect::Resolve { .. }))
    else {
        panic!("playback identity change did not resolve");
    };
    let Some(clear_index) = effects
        .iter()
        .position(|effect| matches!(effect, Effect::ClearArtwork))
    else {
        panic!("art-to-no-art playback identity change did not clear artwork");
    };
    assert!(
        resolve_index < clear_index,
        "artwork synchronization ran before Resolve"
    );
    assert!(state.artwork().requested_url().is_none());
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one cross-surface contract test keeps identical auto-selection behavior auditable"
)]
fn completion_auto_selections_sync_search_podcast_library_and_history_artwork() {
    {
        let mut item = media("youtube-music", "search-auto-art");
        let expected = artwork_url("https://images.example.test/search-auto.jpg");
        item.artwork_url = Some(expected.as_url().clone());
        let (state, effects) = reduce(
            with_artwork_surface(AppState::default(), ArtworkSurface::Search),
            Action::SearchSubmitted {
                query: "art".to_owned(),
                filter: SearchFilter::Songs,
            },
        );
        let [Effect::Search { generation, .. }] = effects.as_slice() else {
            panic!("search submission must load");
        };
        let (state, effects) = reduce(
            state,
            Action::SearchCompleted {
                generation: *generation,
                result: Ok(SearchPage::new(vec![SearchItem::Playable(item)])),
            },
        );
        assert!(fetched_artwork(&effects, &expected));
        assert_eq!(state.artwork().requested_url(), Some(&expected));
    }

    {
        let metadata =
            SearchMetadata::new(SearchMetadataKind::Podcast, "Show").with_provider_id("show-id");
        let state = with_artwork_surface(
            state_with_results(Config::default(), vec![SearchItem::Metadata(metadata)]),
            ArtworkSurface::Podcasts,
        );
        let (state, effects) = reduce(state, Action::OpenSelectedPodcast);
        let [Effect::LoadPodcast { generation, .. }] = effects.as_slice() else {
            panic!("selected podcast must load");
        };
        let mut episode = podcast_episode("podcast-auto-art");
        let expected = artwork_url("https://images.example.test/podcast-auto.jpg");
        episode.artwork_url = Some(expected.as_url().clone());
        let (state, effects) = reduce(
            state,
            Action::PodcastCompleted {
                generation: *generation,
                result: Ok(Podcast {
                    id: "show-id".to_owned(),
                    title: "Show".to_owned(),
                    creators: vec!["Host".to_owned()],
                    description: None,
                    artwork_url: None,
                    episodes: vec![episode],
                }),
            },
        );
        assert!(fetched_artwork(&effects, &expected));
        assert_eq!(state.artwork().requested_url(), Some(&expected));
    }

    {
        let (state, _) = reduce(
            with_artwork_surface(AppState::default(), ArtworkSurface::Library),
            Action::AuthenticationChanged(AuthenticationState::Authenticated),
        );
        let (state, effects) = reduce(
            state,
            Action::LibraryRequested {
                section: LibrarySection::Songs,
            },
        );
        let [Effect::LoadLibrary { generation, .. }] = effects.as_slice() else {
            panic!("library request must load");
        };
        let mut item = media("youtube-music", "library-auto-art");
        let expected = artwork_url("https://images.example.test/library-auto.jpg");
        item.artwork_url = Some(expected.as_url().clone());
        let (state, effects) = reduce(
            state,
            Action::LibraryCompleted {
                generation: *generation,
                result: Ok(Page {
                    items: vec![LibraryItem::Playable(item)],
                    continuation: None,
                    stale: false,
                }),
            },
        );
        assert!(fetched_artwork(&effects, &expected));
        assert_eq!(state.artwork().requested_url(), Some(&expected));
    }

    {
        let (state, effects) = reduce(
            with_artwork_surface(AppState::default(), ArtworkSurface::History),
            Action::HistoryRequested,
        );
        let [Effect::LoadHistory { generation, .. }] = effects.as_slice() else {
            panic!("history request must load");
        };
        let mut item = media("youtube-music", "history-auto-art");
        let expected = artwork_url("https://images.example.test/history-auto.jpg");
        item.artwork_url = Some(expected.as_url().clone());
        let (state, effects) = reduce(
            state,
            Action::HistoryCompleted {
                generation: *generation,
                result: Ok(vec![HistoryEntry {
                    id: 1,
                    item,
                    played_at: 100,
                }]),
            },
        );
        assert!(fetched_artwork(&effects, &expected));
        assert_eq!(state.artwork().requested_url(), Some(&expected));
    }
}

#[test]
fn provider_search_metadata_retains_artwork_for_selection_sync() {
    let expected = artwork_url("https://images.example.test/metadata-art.jpg");
    let page = SearchPage::from_provider(Page {
        items: vec![ytermusic::provider::SearchItem::Podcast(BrowseItem {
            id: "metadata-podcast".to_owned(),
            title: "Metadata Podcast".to_owned(),
            subtitle: None,
            artwork_url: Some(expected.as_url().clone()),
        })],
        continuation: None,
        stale: false,
    });
    let (state, effects) = reduce(
        with_artwork_surface(AppState::default(), ArtworkSurface::Search),
        Action::SearchSubmitted {
            query: "podcast".to_owned(),
            filter: SearchFilter::Podcasts,
        },
    );
    let [Effect::Search { generation, .. }] = effects.as_slice() else {
        panic!("search submission must load");
    };
    let (state, effects) = reduce(
        state,
        Action::SearchCompleted {
            generation: *generation,
            result: Ok(page),
        },
    );
    assert!(fetched_artwork(&effects, &expected));
    assert_eq!(state.artwork().requested_url(), Some(&expected));
}

#[test]
fn active_empty_and_error_search_completions_clear_old_artwork() {
    for (label, result) in [
        ("empty search", Ok(SearchPage::new(Vec::new()))),
        (
            "search error",
            Err(AppError::new(
                AppErrorCategory::Search,
                "search unavailable",
            )),
        ),
    ] {
        let (state, _) = reduce(
            state_with_pending_artwork_for(ArtworkSurface::Search),
            Action::SearchSubmitted {
                query: label.to_owned(),
                filter: SearchFilter::Songs,
            },
        );
        let generation = state.search().generation();

        let (state, effects) = reduce(state, Action::SearchCompleted { generation, result });

        assert_artwork_cleared(&state, &effects, label);
    }
}

#[test]
fn stale_empty_search_completion_keeps_current_artwork() {
    let (state, _) = reduce(
        state_with_pending_artwork(),
        Action::SearchSubmitted {
            query: "old".to_owned(),
            filter: SearchFilter::Songs,
        },
    );
    let stale_generation = state.search().generation();
    let (state, _) = reduce(
        state,
        Action::SearchSubmitted {
            query: "current".to_owned(),
            filter: SearchFilter::Songs,
        },
    );
    let expected = state.clone();

    let (state, effects) = reduce(
        state,
        Action::SearchCompleted {
            generation: stale_generation,
            result: Ok(SearchPage::new(Vec::new())),
        },
    );

    assert_eq!(state, expected);
    assert!(effects.is_empty());
    assert!(state.artwork().active_generation().is_some());
}

#[test]
fn active_empty_and_error_chart_completions_clear_old_artwork() {
    for (label, result) in [
        ("empty charts", Ok(Vec::new())),
        (
            "charts error",
            Err(AppError::new(
                AppErrorCategory::Charts,
                "charts unavailable",
            )),
        ),
    ] {
        let hk = region("hk");
        let (state, effects) = reduce(
            state_with_pending_artwork_for(ArtworkSurface::Charts),
            Action::ChartsRequested { region: hk.clone() },
        );
        let generation = loaded_chart_generation(&effects);

        let (state, effects) = reduce(
            state,
            Action::ChartsCompleted {
                generation,
                region: hk,
                received_at: 1_000,
                result,
            },
        );

        assert_artwork_cleared(&state, &effects, label);
    }
}

#[test]
fn active_empty_and_error_podcast_completions_clear_old_artwork() {
    for (label, result) in [
        (
            "empty podcast",
            Ok(Podcast {
                id: "show-id".to_owned(),
                title: "Show".to_owned(),
                creators: vec!["Host".to_owned()],
                description: None,
                artwork_url: None,
                episodes: Vec::new(),
            }),
        ),
        (
            "podcast error",
            Err(AppError::new(
                AppErrorCategory::Podcast,
                "podcast unavailable",
            )),
        ),
    ] {
        let metadata =
            SearchMetadata::new(SearchMetadataKind::Podcast, "Show").with_provider_id("show-id");
        let state = with_artwork_surface(
            state_with_results(Config::default(), vec![SearchItem::Metadata(metadata)]),
            ArtworkSurface::Podcasts,
        );
        let (state, _) = reduce(
            state,
            Action::ArtworkRequested {
                url: artwork_url("https://images.example.test/pending-old-art.jpg"),
            },
        );
        let (state, effects) = reduce(state, Action::OpenSelectedPodcast);
        let [Effect::LoadPodcast { generation, .. }] = effects.as_slice() else {
            panic!("selected podcast must load");
        };

        let (state, effects) = reduce(
            state,
            Action::PodcastCompleted {
                generation: *generation,
                result,
            },
        );

        assert_artwork_cleared(&state, &effects, label);
    }
}

#[test]
fn active_empty_and_error_library_completions_clear_old_artwork() {
    for (label, result) in [
        (
            "empty library",
            Ok(Page {
                items: Vec::new(),
                continuation: None,
                stale: false,
            }),
        ),
        (
            "library error",
            Err(AppError::new(
                AppErrorCategory::Library,
                "library unavailable",
            )),
        ),
    ] {
        let (state, _) = reduce(
            state_with_pending_artwork_for(ArtworkSurface::Library),
            Action::AuthenticationChanged(AuthenticationState::Authenticated),
        );
        let (state, effects) = reduce(
            state,
            Action::LibraryRequested {
                section: LibrarySection::Songs,
            },
        );
        let [Effect::LoadLibrary { generation, .. }] = effects.as_slice() else {
            panic!("library request must load");
        };

        let (state, effects) = reduce(
            state,
            Action::LibraryCompleted {
                generation: *generation,
                result,
            },
        );

        assert_artwork_cleared(&state, &effects, label);
    }
}

#[test]
fn active_empty_and_error_history_completions_clear_old_artwork() {
    for (label, result) in [
        ("empty history", Ok(Vec::new())),
        (
            "history error",
            Err(AppError::new(
                AppErrorCategory::History,
                "history unavailable",
            )),
        ),
    ] {
        let (state, effects) = reduce(
            state_with_pending_artwork_for(ArtworkSurface::History),
            Action::HistoryRequested,
        );
        let [Effect::LoadHistory { generation, .. }] = effects.as_slice() else {
            panic!("history request must load");
        };

        let (state, effects) = reduce(
            state,
            Action::HistoryCompleted {
                generation: *generation,
                result,
            },
        );

        assert_artwork_cleared(&state, &effects, label);
    }
}
