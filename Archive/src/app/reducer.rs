use crate::{
    domain::{ArtworkUrl, MediaId, MediaItem, MediaKind, PlaybackStatus, RegionCode},
    podcast_rankings::MAX_PODCAST_RECOMMENDATIONS,
    provider::ChartCacheKey,
    queue::{Queue, QueueItem, QueueItemId},
};

use super::state::ChartSelectionAnchor;
use super::{
    Action, AppError, AppErrorCategory, AppState, ArtworkSurface, ChartSection, Diagnostic,
    DiagnosticCategory, Effect, FavoriteMutation, Generation, MAX_VIEW_ITEMS, OpaqueContinuation,
    PendingFavoriteMutation, PodcastProgressCheckpoint, PodcastProviderId, RADIO_FILL_THRESHOLD,
    SearchItem, SessionCheckpoint, stable_library_item_id, stable_queue_item_id,
};

const HISTORY_VIEW_LIMIT: usize = 200;
const MAX_PODCAST_PLAYBACK_EPOCH: u64 = i64::MAX as u64;

#[must_use]
#[allow(
    clippy::too_many_lines,
    reason = "the exhaustive reducer keeps every state mutation in one auditable dispatch point"
)]
pub fn reduce(mut state: AppState, action: Action) -> (AppState, Vec<Effect>) {
    let effects = match action {
        Action::SearchSubmitted { query, filter } => search_submitted(&mut state, query, filter),
        Action::SearchCompleted { generation, result } => {
            if state.search.active_generation != Some(generation) {
                return (state, Vec::new());
            }

            search_completed(&mut state, result);
            sync_selected_search_artwork(&mut state)
        }
        Action::SearchSelectionChanged { id } => {
            let selected = state
                .search
                .items
                .iter()
                .find(|item| item.stable_id() == id);
            if selected.is_some() {
                state.search.selected_id = Some(id);
                sync_selected_search_artwork(&mut state)
            } else {
                Vec::new()
            }
        }
        Action::SearchMoreRequested => search_more_requested(&mut state),
        Action::ChartsRequested { region } => charts_requested(&mut state, region),
        Action::ChartsCompleted {
            generation,
            region,
            received_at,
            result,
        } => {
            if !is_active_chart_request(&state, generation, &region) {
                return (state, Vec::new());
            }

            let mut effects = charts_completed(&mut state, region, received_at, result);
            effects.extend(sync_selected_chart_artwork(&mut state));
            effects
        }
        Action::CachedChartsCompleted {
            generation,
            region,
            observed_at,
            result,
        } => {
            if !is_active_chart_request(&state, generation, &region) {
                return (state, Vec::new());
            }

            cached_charts_completed(&mut state, observed_at, result);
            sync_selected_chart_artwork(&mut state)
        }
        Action::ChartSelectionChanged { media_id } => {
            let item_index = state
                .charts
                .sections
                .iter()
                .flat_map(ChartSection::items)
                .position(|item| item.id == media_id);
            item_index.map_or_else(Vec::new, |item_index| {
                select_chart_row(&mut state, item_index)
            })
        }
        Action::ChartRowSelectionChanged { item_index } => select_chart_row(&mut state, item_index),
        Action::OpenSelectedPodcast => open_selected_podcast(&mut state),
        Action::PodcastRecommendationsRequested { region } => {
            podcast_recommendations_requested(&mut state, region)
        }
        Action::PodcastRecommendationsCompleted {
            generation,
            requested_region,
            result,
        } => {
            if state.podcasts.active_recommendation_generation != Some(generation)
                || state.podcasts.requested_region != requested_region
            {
                return (state, Vec::new());
            }
            podcast_recommendations_completed(&mut state, result);
            sync_podcast_surface_artwork(&mut state)
        }
        Action::PodcastRecommendationSelectionChanged { id } => {
            if state.podcasts.selected_recommendation.as_ref() == Some(&id) {
                Vec::new()
            } else if state
                .podcasts
                .recommendations
                .iter()
                .any(|recommendation| recommendation.source_id() == &id)
            {
                invalidate_recommendation_resolve(&mut state);
                state.podcasts.resolve_error = None;
                state.podcasts.selected_recommendation = Some(id);
                sync_podcast_surface_artwork(&mut state)
            } else {
                Vec::new()
            }
        }
        Action::OpenSelectedPodcastRecommendation => {
            open_selected_podcast_recommendation(&mut state)
        }
        Action::PodcastRecommendationResolved { generation, result } => {
            if state.podcasts.active_resolve_generation != Some(generation) {
                return (state, Vec::new());
            }
            podcast_recommendation_resolved(&mut state, result)
        }
        Action::ClosePodcast => {
            close_podcast(&mut state);
            sync_selected_podcast_recommendation_artwork(&mut state)
        }
        Action::PodcastCompleted { generation, result } => {
            if state.podcasts.active_generation != Some(generation) {
                return (state, Vec::new());
            }

            podcast_completed(&mut state, result);
            sync_selected_podcast_artwork(&mut state)
        }
        Action::PodcastSelectionChanged { media_id } => {
            if state
                .podcasts
                .show
                .as_ref()
                .is_some_and(|show| show.episodes.iter().any(|item| item.id == media_id))
            {
                state.podcasts.selected_episode = Some(media_id);
                sync_selected_podcast_artwork(&mut state)
            } else {
                Vec::new()
            }
        }
        Action::PlayPodcastEpisode { media_id } => play_podcast_episode(&mut state, &media_id),
        Action::PodcastProgressLoaded {
            generation,
            progress,
        } => {
            if state.podcasts.pending_progress_generation != Some(generation) {
                return (state, Vec::new());
            }

            podcast_progress_loaded(&mut state, progress)
        }
        Action::AuthenticationChanged(authentication) => {
            authentication_changed(&mut state, authentication);
            Vec::new()
        }
        Action::ConnectAccountRequested { browser } => {
            if state.library.authentication == crate::provider::AuthenticationState::Unauthenticated
            {
                vec![Effect::ConnectAccount { browser }]
            } else {
                Vec::new()
            }
        }
        Action::LibraryRequested { section } => library_requested(&mut state, section),
        Action::LibraryMoreRequested => library_more_requested(&mut state),
        Action::LibrarySelectionChanged { id } => {
            if state
                .library
                .items
                .iter()
                .any(|item| stable_library_item_id(item) == id)
            {
                state.library.selected_id = Some(id);
                sync_selected_library_artwork(&mut state)
            } else {
                Vec::new()
            }
        }
        Action::LibraryCompleted { generation, result } => {
            if state.library.active_generation != Some(generation) {
                return (state, Vec::new());
            }

            library_completed(&mut state, result);
            sync_selected_library_artwork(&mut state)
        }
        Action::DependencyCheckRequested => {
            if state.dependencies.checking {
                Vec::new()
            } else {
                state.dependencies.checking = true;
                vec![Effect::CheckDependencies]
            }
        }
        Action::DependencyReportLoaded(report) => {
            state.dependencies.report = Some(report);
            state.dependencies.checking = false;
            Vec::new()
        }
        Action::HistoryRequested => history_requested(&mut state),
        Action::HistorySelectionChanged { id } => {
            if state.history.entries.iter().any(|entry| entry.id == id) {
                state.history.selected_id = Some(id);
                sync_selected_history_artwork(&mut state)
            } else {
                Vec::new()
            }
        }
        Action::HistoryCompleted { generation, result } => {
            if state.history.active_generation != Some(generation) {
                return (state, Vec::new());
            }

            history_completed(&mut state, result);
            sync_selected_history_artwork(&mut state)
        }
        Action::FavoritesRequested => favorites_requested(&mut state),
        Action::FavoriteSelectionChanged { media_id } => {
            if state
                .favorites
                .entries
                .iter()
                .any(|entry| entry.item.id == media_id)
            {
                state.favorites.selected_id = Some(media_id);
            }
            sync_selected_favorite_artwork(&mut state)
        }
        Action::FavoriteToggleRequested { item } => favorite_toggle_requested(&mut state, item),
        Action::FavoritesCompleted { generation, result } => {
            if state.favorites.active_generation != Some(generation)
                || state.favorites.pending_mutation.is_some()
            {
                return (state, Vec::new());
            }
            favorites_completed(&mut state, result);
            sync_selected_favorite_artwork(&mut state)
        }
        Action::FavoriteMutationCompleted {
            generation,
            media_id,
            mutation,
            result,
        } => {
            if state.favorites.active_generation != Some(generation)
                || state
                    .favorites
                    .pending_mutation
                    .as_ref()
                    .is_none_or(|pending| {
                        pending.media_id() != &media_id || pending.mutation() != mutation
                    })
            {
                return (state, Vec::new());
            }
            favorites_completed(&mut state, result);
            sync_selected_favorite_artwork(&mut state)
        }
        Action::ArtworkRequested { url } => artwork_requested(&mut state, url),
        Action::ArtworkSurfaceChanged { surface } => artwork_surface_changed(&mut state, surface),
        Action::ArtworkCompleted { generation, result } => {
            if state.artwork.active_generation != Some(generation) {
                return (state, Vec::new());
            }

            artwork_completed(&mut state, result);
            Vec::new()
        }
        Action::LyricsRequested { item } => request_lyrics(&mut state, item.into_media()),
        Action::LyricsCompleted {
            generation,
            media_id,
            result,
        } => {
            if state.lyrics.active_generation != Some(generation)
                || state.lyrics.media_id.as_ref() != Some(media_id.media_id())
                || state.playback.current.as_ref() != Some(media_id.media_id())
            {
                return (state, Vec::new());
            }
            state.lyrics.active_generation = None;
            state.lyrics.loading = false;
            match result {
                Ok(document) => {
                    state.lyrics.error = None;
                    state.lyrics.document = document;
                    update_active_lyric_line(&mut state);
                }
                Err(error) => {
                    state.lyrics.error = Some(error);
                    state.lyrics.document = None;
                    state.lyrics.active_line_index = None;
                }
            }
            Vec::new()
        }
        Action::ActivateSearchResult { index } => activate_search_result(&mut state, index),
        Action::EnqueueMedia { item } => enqueue_media(&mut state, item),
        Action::EnqueueSelectedSearchResult => enqueue_selected_search_result(&mut state),
        Action::PlayMediaList {
            items,
            selected_id,
            shuffle_seed,
        } => play_media_list(&mut state, items, &selected_id, shuffle_seed),
        Action::PlayQueueItem { id } => play_queue_item(&mut state, &id),
        Action::TogglePlayback => toggle_playback(&mut state),
        Action::NextRequested => navigate_queue(&mut state, QueueDirection::Next),
        Action::PreviousRequested => navigate_queue(&mut state, QueueDirection::Previous),
        Action::SeekRelativeRequested { seconds } => seek_relative(&state, seconds),
        Action::PlayerProgress {
            generation,
            media_id,
            position_ms,
            duration_ms,
        } => {
            if state.current_attempt_generation == Some(generation)
                && state.playback.current.as_ref() == Some(&media_id)
            {
                state.playback.position_ms = position_ms;
                state.playback.duration_ms = duration_ms;
                update_active_lyric_line(&mut state);
                if state.queue.current().is_some_and(|item| {
                    item.media().id == media_id && item.media().kind == MediaKind::PodcastEpisode
                }) && let Some(playback_epoch) = state.current_podcast_epoch
                {
                    return (
                        state,
                        vec![Effect::SavePodcastProgress(PodcastProgressCheckpoint::new(
                            media_id,
                            playback_epoch,
                            position_ms,
                            duration_ms,
                            false,
                        ))],
                    );
                }
            }
            Vec::new()
        }
        Action::PlayerStatusChanged { generation, status } => {
            player_status_changed(&mut state, generation, status)
        }
        Action::ResolveSucceeded { generation } => {
            if state.active_resolve_generation != Some(generation)
                || state.current_attempt_generation != Some(generation)
            {
                return (state, Vec::new());
            }

            state.active_resolve_generation = None;
            state.playback.status = PlaybackStatus::Buffering;
            Vec::new()
        }
        Action::ResolvedFormatUpdated {
            generation,
            quality,
        } => {
            if state.current_attempt_generation == Some(generation) {
                state.player_presentation.quality = quality;
            }
            Vec::new()
        }
        Action::PreviewStreamUpdated {
            generation,
            preview_url,
        } => {
            if state.current_attempt_generation == Some(generation) {
                state.player_presentation.preview_url = preview_url;
            }
            Vec::new()
        }
        Action::AnalysisStreamUpdated {
            generation,
            stream_url,
        } => {
            if state.current_attempt_generation == Some(generation) {
                state.player_presentation.analysis_url = stream_url;
            }
            Vec::new()
        }
        Action::PlaybackTelemetryUpdated {
            generation,
            effective_volume,
            fade,
        } => {
            if state.current_attempt_generation == Some(generation) {
                state.player_presentation.effective_volume =
                    normalize_effective_volume(effective_volume);
                state.player_presentation.fade = fade;
            }
            Vec::new()
        }
        Action::ResolveFailed { generation, error } => {
            if state.active_resolve_generation != Some(generation)
                || state.current_attempt_generation != Some(generation)
            {
                return (state, Vec::new());
            }

            resolve_failed(&mut state, &error)
        }
        Action::PlayerEnded { generation } => {
            if state.current_attempt_generation != Some(generation) {
                return (state, Vec::new());
            }

            player_ended(&mut state)
        }
        Action::TargetVolumeChanged(volume) => {
            let volume = volume.min(100);
            if state.playback.target_volume == volume {
                Vec::new()
            } else {
                state.playback.target_volume = volume;
                vec![
                    Effect::Player(super::PlayerCommand::Volume(volume)),
                    persist(&state),
                ]
            }
        }
        Action::RepeatModeChanged(repeat) => {
            if state.queue.repeat() == repeat {
                Vec::new()
            } else {
                state.queue.set_repeat(repeat);
                vec![persist(&state)]
            }
        }
        Action::ShuffleEnabledChanged { enabled, seed } => {
            if state.queue.is_shuffled() == enabled {
                Vec::new()
            } else {
                state.queue.set_shuffle(enabled, seed);
                vec![persist(&state)]
            }
        }
        Action::QueueItemMovedBefore { id, before } => {
            match state.queue.move_before(&id, &before) {
                Ok(()) => vec![persist(&state)],
                Err(error) => {
                    state.diagnostics.push(Diagnostic::new(
                        DiagnosticCategory::State,
                        error.to_string(),
                        None,
                    ));
                    Vec::new()
                }
            }
        }
        Action::RadioEnabledChanged(enabled) => radio_enabled_changed(&mut state, enabled),
        Action::CheckRadioFill => maybe_request_radio_fill(&mut state).into_iter().collect(),
        Action::RadioFillCompleted { generation, result } => {
            if state.pending_radio_generation != Some(generation) {
                return (state, Vec::new());
            }

            radio_fill_completed(&mut state, result)
        }
        Action::RuntimeDiagnostic {
            category,
            message,
            media_id,
        } => {
            state
                .diagnostics
                .push(Diagnostic::new(category, message, media_id));
            Vec::new()
        }
    };

    let persist_allowed = session_persistence_is_allowed(&state);
    let effects = effects
        .into_iter()
        .filter(|effect| !matches!(effect, Effect::Persist(_)) || persist_allowed)
        .collect();
    (state, effects)
}

fn search_submitted(
    state: &mut AppState,
    query: String,
    filter: super::SearchFilter,
) -> Vec<Effect> {
    state.search.query.clone_from(&query);
    state.search.filter = filter;
    state.search.loading = false;
    state.search.loading_more = false;
    state.search.items.clear();
    state.search.continuation = None;
    state.search.stale = false;
    state.search.error = None;
    state.search.active_generation = None;

    match allocate_generation(state) {
        Ok(generation) => {
            state.search.generation = generation;
            state.search.active_generation = Some(generation);
            state.search.loading = true;
            vec![Effect::Search {
                generation,
                query,
                filter,
            }]
        }
        Err(error) => {
            state.search.error = Some(error.clone());
            record_error(state, DiagnosticCategory::State, &error, None);
            Vec::new()
        }
    }
}

fn search_completed(state: &mut AppState, result: Result<super::SearchPage, AppError>) {
    let loading_more = state.search.loading_more;
    state.search.active_generation = None;
    state.search.loading = false;
    state.search.loading_more = false;
    match result {
        Ok(page) => {
            let previous_selection = state.search.selected_id.take();
            let (mut items, continuation, is_stale) = page.into_parts();
            if loading_more {
                for item in items {
                    if state.search.items.len() >= MAX_VIEW_ITEMS {
                        break;
                    }
                    let id = item.stable_id();
                    if !state
                        .search
                        .items
                        .iter()
                        .any(|existing| existing.stable_id() == id)
                    {
                        state.search.items.push(item);
                    }
                }
            } else {
                items.truncate(MAX_VIEW_ITEMS);
                state.search.items = items;
            }
            state.search.selected_id = previous_selection
                .filter(|selected| {
                    state
                        .search
                        .items
                        .iter()
                        .any(|item| item.stable_id() == *selected)
                })
                .or_else(|| state.search.items.first().map(SearchItem::stable_id));
            state.search.continuation =
                continuation.filter(|_| state.search.items.len() < MAX_VIEW_ITEMS);
            state.search.stale = is_stale;
            state.search.error = None;
        }
        Err(error) => {
            if !loading_more {
                state.search.items.clear();
                state.search.selected_id = None;
                state.search.continuation = None;
                state.search.stale = false;
            }
            state.search.error = Some(error);
        }
    }
}

fn search_more_requested(state: &mut AppState) -> Vec<Effect> {
    if state.search.active_generation.is_some() {
        return Vec::new();
    }
    let Some(continuation) = state.search.continuation.clone() else {
        return Vec::new();
    };
    match allocate_generation(state) {
        Ok(generation) => {
            state.search.generation = generation;
            state.search.active_generation = Some(generation);
            state.search.loading_more = true;
            state.search.error = None;
            vec![Effect::SearchMore {
                generation,
                query: state.search.query.clone(),
                filter: state.search.filter,
                continuation,
            }]
        }
        Err(error) => {
            state.search.error = Some(error.clone());
            record_error(state, DiagnosticCategory::State, &error, None);
            Vec::new()
        }
    }
}

fn charts_requested(state: &mut AppState, region: RegionCode) -> Vec<Effect> {
    state.charts.region = Some(region.clone());
    state.charts.active_generation = None;
    state.charts.loading = false;
    state.charts.sections.clear();
    state.charts.stale = false;
    state.charts.cached_at = None;
    state.charts.error = None;
    state.charts.cache_pending = false;
    state.charts.live_pending = false;
    state.charts.cached_candidate = None;
    state.charts.cache_observed_at = None;
    state.charts.live_error = None;

    match allocate_generation(state) {
        Ok(generation) => {
            state.charts.generation = generation;
            state.charts.active_generation = Some(generation);
            state.charts.loading = true;
            state.charts.cache_pending = true;
            state.charts.live_pending = true;
            let key = ChartCacheKey::new(region.clone());
            vec![
                Effect::ReadChartCache {
                    generation,
                    region: region.clone(),
                    key,
                },
                Effect::LoadCharts { generation, region },
            ]
        }
        Err(error) => {
            state.charts.error = Some(error.clone());
            record_error(state, DiagnosticCategory::State, &error, None);
            Vec::new()
        }
    }
}

fn charts_completed(
    state: &mut AppState,
    region: RegionCode,
    received_at: i64,
    result: Result<Vec<ChartSection>, AppError>,
) -> Vec<Effect> {
    state.charts.live_pending = false;
    match result {
        Ok(sections) => {
            let previous_selection = state.charts.selected_anchor.clone().or_else(|| {
                chart_selection_anchor(
                    &state.charts.sections,
                    state.charts.selected_id.as_ref(),
                    state.charts.selected_index,
                )
            });
            state.charts.sections = bounded_chart_sections(sections);
            let selection = chart_selection(&state.charts.sections, previous_selection);
            set_chart_selection(state, selection);
            state.charts.stale = false;
            state.charts.cached_at = None;
            state.charts.error = None;
            state.charts.active_generation = None;
            state.charts.loading = false;
            state.charts.cache_pending = false;
            state.charts.cached_candidate = None;
            state.charts.cache_observed_at = None;
            state.charts.live_error = None;

            let expires_at = received_at.saturating_add(super::CHART_CACHE_TTL_SECONDS);
            match super::ChartCachePayload::try_new(
                region.clone(),
                state.charts.sections.clone(),
                received_at,
                expires_at,
            ) {
                Ok(payload) => vec![Effect::StoreChartCache {
                    key: ChartCacheKey::new(region),
                    payload,
                }],
                Err(error) => {
                    record_error(state, DiagnosticCategory::State, &error, None);
                    Vec::new()
                }
            }
        }
        Err(error) => {
            state.charts.live_error = Some(error);
            finalize_chart_sources(state);
            Vec::new()
        }
    }
}

fn cached_charts_completed(
    state: &mut AppState,
    observed_at: i64,
    result: Result<Option<super::ChartCachePayload>, AppError>,
) {
    state.charts.cache_pending = false;
    state.charts.cache_observed_at = Some(observed_at);
    state.charts.cached_candidate = match result {
        Ok(Some(payload))
            if state
                .charts
                .region
                .as_ref()
                .is_some_and(|region| payload.region() == region) =>
        {
            Some(payload)
        }
        _ => None,
    };
    finalize_chart_sources(state);
}

fn finalize_chart_sources(state: &mut AppState) {
    if state.charts.live_pending || state.charts.cache_pending {
        return;
    }

    state.charts.active_generation = None;
    state.charts.loading = false;
    if let Some(page) = state.charts.cached_candidate.take() {
        let observed_at = state.charts.cache_observed_at.unwrap_or(i64::MAX);
        let cache_is_stale = page.stale_at(observed_at);
        let (sections, cached_at, _) = page.into_parts();
        let previous_selection = state.charts.selected_anchor.clone().or_else(|| {
            chart_selection_anchor(
                &state.charts.sections,
                state.charts.selected_id.as_ref(),
                state.charts.selected_index,
            )
        });
        state.charts.sections = bounded_chart_sections(sections);
        let selection = chart_selection(&state.charts.sections, previous_selection);
        set_chart_selection(state, selection);
        state.charts.stale = cache_is_stale;
        state.charts.cached_at = Some(cached_at);
        state.charts.error = None;
    } else {
        state.charts.sections.clear();
        state.charts.selected_id = None;
        state.charts.selected_index = None;
        state.charts.selected_anchor = None;
        state.charts.stale = false;
        state.charts.cached_at = None;
        state.charts.error = state.charts.live_error.take();
    }
    state.charts.cache_observed_at = None;
    state.charts.live_error = None;
}

fn is_active_chart_request(state: &AppState, generation: Generation, region: &RegionCode) -> bool {
    state.charts.active_generation == Some(generation)
        && state.charts.region.as_ref() == Some(region)
}

fn chart_selection_anchor(
    sections: &[ChartSection],
    selected_id: Option<&MediaId>,
    selected_index: Option<usize>,
) -> Option<ChartSelectionAnchor> {
    let selected_id = selected_id?;
    let selected_index = selected_index?;
    let mut section_start = 0usize;
    for (section_index, section) in sections.iter().enumerate() {
        let section_end = section_start.saturating_add(section.items().len());
        if selected_index < section_end {
            let local_index = selected_index.saturating_sub(section_start);
            if &section.items().get(local_index)?.id != selected_id {
                return None;
            }
            let section_ordinal = sections[..section_index]
                .iter()
                .filter(|candidate| candidate.title() == section.title())
                .count();
            let occurrence_in_section = section.items()[..local_index]
                .iter()
                .filter(|item| &item.id == selected_id)
                .count();
            let global_occurrence = sections
                .iter()
                .flat_map(ChartSection::items)
                .take(selected_index)
                .filter(|item| &item.id == selected_id)
                .count();
            return Some(ChartSelectionAnchor {
                media_id: selected_id.clone(),
                section_title: section.title().to_owned(),
                section_ordinal,
                occurrence_in_section,
                global_occurrence,
            });
        }
        section_start = section_end;
    }
    None
}

fn chart_selection(
    sections: &[ChartSection],
    previous: Option<ChartSelectionAnchor>,
) -> Option<(MediaId, usize)> {
    if let Some(previous) = previous {
        let mut section_start = 0usize;
        let mut matching_section_ordinal = 0usize;
        for section in sections {
            if section.title() == previous.section_title {
                if matching_section_ordinal == previous.section_ordinal
                    && let Some((local_index, item)) = section
                        .items()
                        .iter()
                        .enumerate()
                        .filter(|(_, item)| item.id == previous.media_id)
                        .nth(previous.occurrence_in_section)
                {
                    return Some((item.id.clone(), section_start.saturating_add(local_index)));
                }
                matching_section_ordinal = matching_section_ordinal.saturating_add(1);
            }
            section_start = section_start.saturating_add(section.items().len());
        }
        if let Some((index, item)) = sections
            .iter()
            .flat_map(ChartSection::items)
            .enumerate()
            .filter(|(_, item)| item.id == previous.media_id)
            .nth(previous.global_occurrence)
        {
            return Some((item.id.clone(), index));
        }
        if let Some((index, item)) = sections
            .iter()
            .flat_map(ChartSection::items)
            .enumerate()
            .find(|(_, item)| item.id == previous.media_id)
        {
            return Some((item.id.clone(), index));
        }
    }
    sections
        .iter()
        .flat_map(ChartSection::items)
        .enumerate()
        .next()
        .map(|(index, item)| (item.id.clone(), index))
}

fn set_chart_selection(state: &mut AppState, selection: Option<(MediaId, usize)>) {
    let Some((selected_id, selected_index)) = selection else {
        state.charts.selected_id = None;
        state.charts.selected_index = None;
        state.charts.selected_anchor = None;
        return;
    };
    state.charts.selected_id = Some(selected_id);
    state.charts.selected_index = Some(selected_index);
    state.charts.selected_anchor = chart_selection_anchor(
        &state.charts.sections,
        state.charts.selected_id.as_ref(),
        state.charts.selected_index,
    );
}

fn select_chart_row(state: &mut AppState, item_index: usize) -> Vec<Effect> {
    let selected_id = state
        .charts
        .sections
        .iter()
        .flat_map(ChartSection::items)
        .nth(item_index)
        .map(|item| item.id.clone());
    let Some(selected_id) = selected_id else {
        return Vec::new();
    };
    state.charts.selected_id = Some(selected_id);
    state.charts.selected_index = Some(item_index);
    state.charts.selected_anchor = chart_selection_anchor(
        &state.charts.sections,
        state.charts.selected_id.as_ref(),
        state.charts.selected_index,
    );
    sync_selected_chart_artwork(state)
}

fn bounded_chart_sections(mut sections: Vec<ChartSection>) -> Vec<ChartSection> {
    sections.truncate(crate::provider::MAX_SECTIONS);
    for section in &mut sections {
        section.items.truncate(crate::provider::MAX_ITEMS_PER_SHELF);
    }
    sections
}

fn open_selected_podcast(state: &mut AppState) -> Vec<Effect> {
    let selected = state.search.selected_id.clone();
    let provider_id = state
        .search
        .items
        .iter()
        .find(|item| selected.as_ref().is_some_and(|id| item.stable_id() == *id))
        .and_then(|item| match item {
            SearchItem::Metadata(metadata)
                if metadata.kind() == super::SearchMetadataKind::Podcast =>
            {
                metadata.provider_id().map(str::to_owned)
            }
            SearchItem::Playable(_) | SearchItem::Metadata(_) => None,
        });
    let Some(provider_id) = provider_id.and_then(PodcastProviderId::new) else {
        state.diagnostics.push(Diagnostic::new(
            DiagnosticCategory::Selection,
            "The selected search result is not a podcast show",
            None,
        ));
        return Vec::new();
    };

    invalidate_recommendation_resolve(state);
    state.podcasts.active_generation = None;
    state.podcasts.loading = false;
    state.podcasts.error = None;
    match allocate_generation(state) {
        Ok(generation) => {
            state.podcasts.generation = generation;
            state.podcasts.active_generation = Some(generation);
            state.podcasts.loading = true;
            vec![Effect::LoadPodcast {
                generation,
                id: provider_id,
            }]
        }
        Err(error) => {
            state.podcasts.error = Some(error.clone());
            record_error(state, DiagnosticCategory::State, &error, None);
            Vec::new()
        }
    }
}

fn podcast_recommendations_requested(state: &mut AppState, region: RegionCode) -> Vec<Effect> {
    invalidate_recommendation_resolve(state);
    state.podcasts.requested_region = region.clone();
    state.podcasts.active_recommendation_generation = None;
    state.podcasts.recommendations_loading = false;
    state.podcasts.recommendation_error = None;

    match allocate_generation(state) {
        Ok(generation) => {
            state.podcasts.recommendation_generation = generation;
            state.podcasts.active_recommendation_generation = Some(generation);
            state.podcasts.recommendations_loading = true;
            vec![Effect::LoadPodcastRecommendations { generation, region }]
        }
        Err(error) => {
            state.podcasts.recommendation_error = Some(error.clone());
            record_error(state, DiagnosticCategory::State, &error, None);
            Vec::new()
        }
    }
}

fn podcast_recommendations_completed(
    state: &mut AppState,
    result: Result<crate::podcast_rankings::PodcastRecommendationPage, AppError>,
) {
    state.podcasts.active_recommendation_generation = None;
    state.podcasts.recommendations_loading = false;
    match result {
        Ok(page) => {
            let previous_selection = state.podcasts.selected_recommendation.take();
            let effective_region = page.region().clone();
            let mut recommendations = page.items().to_vec();
            recommendations.truncate(MAX_PODCAST_RECOMMENDATIONS);
            state.podcasts.selected_recommendation = previous_selection
                .filter(|selected| {
                    recommendations
                        .iter()
                        .any(|recommendation| recommendation.source_id() == selected)
                })
                .or_else(|| {
                    recommendations
                        .first()
                        .map(|recommendation| recommendation.source_id().clone())
                });
            state.podcasts.effective_region = Some(effective_region);
            state.podcasts.recommendations = recommendations;
            state.podcasts.recommendation_error = None;
            state.podcasts.resolve_error = None;
        }
        Err(error) => {
            state.podcasts.recommendation_error = Some(error);
        }
    }
}

fn open_selected_podcast_recommendation(state: &mut AppState) -> Vec<Effect> {
    let selected = state.podcasts.selected_recommendation.as_ref();
    let recommendation = state
        .podcasts
        .recommendations
        .iter()
        .find(|recommendation| selected == Some(recommendation.source_id()))
        .cloned();
    let Some(recommendation) = recommendation else {
        return Vec::new();
    };

    state.podcasts.active_resolve_generation = None;
    state.podcasts.resolve_loading = false;
    state.podcasts.resolve_error = None;
    match allocate_generation(state) {
        Ok(generation) => {
            state.podcasts.resolve_generation = generation;
            state.podcasts.active_resolve_generation = Some(generation);
            state.podcasts.resolve_loading = true;
            vec![Effect::ResolvePodcastRecommendation {
                generation,
                recommendation,
            }]
        }
        Err(error) => {
            state.podcasts.resolve_error = Some(error.clone());
            record_error(state, DiagnosticCategory::State, &error, None);
            Vec::new()
        }
    }
}

fn podcast_recommendation_resolved(
    state: &mut AppState,
    result: Result<PodcastProviderId, AppError>,
) -> Vec<Effect> {
    state.podcasts.active_resolve_generation = None;
    state.podcasts.resolve_loading = false;
    let Ok(provider_id) = result else {
        state.podcasts.resolve_error = Some(podcast_match_error());
        return Vec::new();
    };

    match allocate_generation(state) {
        Ok(generation) => {
            state.podcasts.generation = generation;
            state.podcasts.active_generation = Some(generation);
            state.podcasts.loading = true;
            state.podcasts.error = None;
            state.podcasts.resolve_error = None;
            vec![Effect::LoadPodcast {
                generation,
                id: provider_id,
            }]
        }
        Err(error) => {
            state.podcasts.resolve_error = Some(error.clone());
            record_error(state, DiagnosticCategory::State, &error, None);
            Vec::new()
        }
    }
}

fn podcast_match_error() -> AppError {
    AppError::new(
        AppErrorCategory::Podcast,
        "Podcast recommendation could not be matched",
    )
}

fn close_podcast(state: &mut AppState) {
    invalidate_recommendation_resolve(state);
    state.podcasts.active_generation = None;
    state.podcasts.loading = false;
    state.podcasts.show = None;
    state.podcasts.selected_episode = None;
    state.podcasts.pending_progress_generation = None;
    state.podcasts.pending_media = None;
    state.podcasts.error = None;
}

fn invalidate_recommendation_resolve(state: &mut AppState) {
    state.podcasts.active_resolve_generation = None;
    state.podcasts.resolve_loading = false;
}

fn podcast_completed(state: &mut AppState, result: Result<crate::provider::Podcast, AppError>) {
    state.podcasts.active_generation = None;
    state.podcasts.loading = false;
    match result {
        Ok(mut show) => {
            show.episodes.truncate(MAX_VIEW_ITEMS);
            state.podcasts.selected_episode = state
                .podcasts
                .selected_episode
                .take()
                .filter(|selected| show.episodes.iter().any(|item| &item.id == selected))
                .or_else(|| show.episodes.first().map(|item| item.id.clone()));
            state.podcasts.show = Some(show);
            state.podcasts.error = None;
        }
        Err(error) => {
            state.podcasts.show = None;
            state.podcasts.selected_episode = None;
            state.podcasts.error = Some(error);
        }
    }
}

fn play_podcast_episode(state: &mut AppState, media_id: &MediaId) -> Vec<Effect> {
    let episode = state.podcasts.show.as_ref().and_then(|show| {
        show.episodes
            .iter()
            .find(|item| &item.id == media_id)
            .cloned()
    });
    let Some(episode) = episode else {
        state.diagnostics.push(Diagnostic::new(
            DiagnosticCategory::Selection,
            "The selected podcast episode is unavailable",
            Some(media_id.clone()),
        ));
        return Vec::new();
    };

    state.podcasts.selected_episode = Some(media_id.clone());
    begin_playback(state, episode)
}

fn podcast_progress_loaded(
    state: &mut AppState,
    progress: Option<crate::storage::PodcastProgress>,
) -> Vec<Effect> {
    state.podcasts.pending_progress_generation = None;
    let Some(pending_episode) = state.podcasts.pending_media.take() else {
        return Vec::new();
    };
    let saved = progress.filter(|saved| saved.video_id == pending_episode.id.video_id);
    let playback_epoch = saved.as_ref().map_or(Some(1), |saved| {
        saved
            .playback_epoch
            .checked_add(1)
            .filter(|epoch| *epoch <= MAX_PODCAST_PLAYBACK_EPOCH)
    });
    let Some(playback_epoch) = playback_epoch else {
        let error = AppError::new(
            AppErrorCategory::State,
            "Podcast playback epoch space is exhausted",
        );
        state.podcasts.error = Some(error.clone());
        record_error(
            state,
            DiagnosticCategory::State,
            &error,
            Some(pending_episode.id),
        );
        return Vec::new();
    };
    state.podcasts.error = None;

    let queue_id = stable_queue_item_id(&pending_episode.id);
    let _ = state
        .queue
        .append_unique(QueueItem::new(queue_id.clone(), pending_episode));
    if state.queue.select(&queue_id).is_err() {
        return Vec::new();
    }
    let Some(episode) = state.queue.current().map(|item| item.media().clone()) else {
        return Vec::new();
    };
    let resume_position_ms = saved.filter(|saved| !saved.played).map_or(0, |saved| {
        saved
            .position_ms
            .min(episode.duration_ms.unwrap_or(u64::MAX))
    });
    let mut effects = begin_resolution_at(
        state,
        episode.clone(),
        resume_position_ms,
        Some(playback_epoch),
    );
    let attempt_installed = state.current_attempt_generation.is_some()
        && state.current_attempt_generation == state.active_resolve_generation
        && state.current_podcast_epoch == Some(playback_epoch);
    if attempt_installed {
        effects.insert(
            0,
            Effect::SavePodcastProgress(PodcastProgressCheckpoint::new(
                episode.id,
                playback_epoch,
                resume_position_ms,
                episode.duration_ms,
                false,
            )),
        );
    }
    push_session_persist_if_coherent(state, &mut effects);
    effects
}

fn authentication_changed(
    state: &mut AppState,
    authentication: crate::provider::AuthenticationState,
) {
    state.library.authentication = authentication;
    if authentication == crate::provider::AuthenticationState::Unauthenticated {
        state.library.active_generation = None;
        state.library.loading = false;
        state.library.loading_more = false;
        state.library.items.clear();
        state.library.selected_id = None;
        state.library.continuation = None;
        state.library.stale = false;
        state.library.error = None;
    }
}

fn library_requested(
    state: &mut AppState,
    section: crate::provider::LibrarySection,
) -> Vec<Effect> {
    if state.library.authentication == crate::provider::AuthenticationState::Unauthenticated {
        return Vec::new();
    }

    let section_changed = state.library.section != section;
    state.library.section = section;
    state.library.active_generation = None;
    state.library.loading = false;
    state.library.loading_more = false;
    state.library.error = None;
    if section_changed {
        state.library.items.clear();
        state.library.selected_id = None;
        state.library.continuation = None;
        state.library.stale = false;
    }
    match allocate_generation(state) {
        Ok(generation) => {
            state.library.generation = generation;
            state.library.active_generation = Some(generation);
            state.library.loading = true;
            vec![Effect::LoadLibrary {
                generation,
                section,
                continuation: None,
            }]
        }
        Err(error) => {
            state.library.error = Some(error.clone());
            record_error(state, DiagnosticCategory::State, &error, None);
            Vec::new()
        }
    }
}

fn library_more_requested(state: &mut AppState) -> Vec<Effect> {
    if state.library.authentication == crate::provider::AuthenticationState::Unauthenticated
        || state.library.active_generation.is_some()
    {
        return Vec::new();
    }
    let Some(continuation) = state.library.continuation.clone() else {
        return Vec::new();
    };
    match allocate_generation(state) {
        Ok(generation) => {
            state.library.generation = generation;
            state.library.active_generation = Some(generation);
            state.library.loading_more = true;
            state.library.error = None;
            vec![Effect::LoadLibrary {
                generation,
                section: state.library.section,
                continuation: Some(continuation),
            }]
        }
        Err(error) => {
            state.library.error = Some(error.clone());
            record_error(state, DiagnosticCategory::State, &error, None);
            Vec::new()
        }
    }
}

fn library_completed(
    state: &mut AppState,
    result: Result<crate::provider::Page<crate::provider::LibraryItem>, AppError>,
) {
    let loading_more = state.library.loading_more;
    state.library.active_generation = None;
    state.library.loading = false;
    state.library.loading_more = false;
    match result {
        Ok(page) => {
            let previous_selection = state.library.selected_id.take();
            let crate::provider::Page {
                mut items,
                continuation,
                stale: is_stale,
            } = page;
            if loading_more {
                for item in items {
                    if state.library.items.len() >= MAX_VIEW_ITEMS {
                        break;
                    }
                    let id = stable_library_item_id(&item);
                    if !state
                        .library
                        .items
                        .iter()
                        .any(|existing| stable_library_item_id(existing) == id)
                    {
                        state.library.items.push(item);
                    }
                }
            } else {
                items.truncate(MAX_VIEW_ITEMS);
                state.library.items = items;
            }
            state.library.selected_id = previous_selection
                .filter(|selected| {
                    state
                        .library
                        .items
                        .iter()
                        .any(|item| stable_library_item_id(item) == *selected)
                })
                .or_else(|| state.library.items.first().map(stable_library_item_id));
            state.library.continuation = continuation
                .and_then(OpaqueContinuation::new)
                .filter(|_| state.library.items.len() < MAX_VIEW_ITEMS);
            state.library.stale = is_stale;
            state.library.error = None;
        }
        Err(error) => {
            state.library.error = Some(error);
        }
    }
}

fn history_requested(state: &mut AppState) -> Vec<Effect> {
    state.history.active_generation = None;
    state.history.loading = false;
    state.history.error = None;
    match allocate_generation(state) {
        Ok(generation) => {
            state.history.generation = generation;
            state.history.active_generation = Some(generation);
            state.history.loading = true;
            vec![Effect::LoadHistory {
                generation,
                limit: HISTORY_VIEW_LIMIT,
            }]
        }
        Err(error) => {
            state.history.error = Some(error.clone());
            record_error(state, DiagnosticCategory::State, &error, None);
            Vec::new()
        }
    }
}

fn history_completed(
    state: &mut AppState,
    result: Result<Vec<crate::storage::HistoryEntry>, AppError>,
) {
    state.history.active_generation = None;
    state.history.loading = false;
    match result {
        Ok(mut entries) => {
            entries.truncate(HISTORY_VIEW_LIMIT);
            let previous = state.history.selected_id.take();
            state.history.entries = entries;
            state.history.selected_id = previous
                .filter(|selected| {
                    state
                        .history
                        .entries
                        .iter()
                        .any(|entry| entry.id == *selected)
                })
                .or_else(|| state.history.entries.first().map(|entry| entry.id));
            state.history.error = None;
        }
        Err(error) => {
            state.history.entries.clear();
            state.history.selected_id = None;
            state.history.error = Some(error);
        }
    }
}

fn favorites_requested(state: &mut AppState) -> Vec<Effect> {
    state.favorites.active_generation = None;
    state.favorites.loading = false;
    state.favorites.pending_mutation = None;
    state.favorites.error = None;
    match allocate_generation(state) {
        Ok(generation) => {
            state.favorites.generation = generation;
            state.favorites.active_generation = Some(generation);
            state.favorites.loading = true;
            vec![Effect::LoadFavorites { generation }]
        }
        Err(error) => {
            state.favorites.error = Some(error.clone());
            record_error(state, DiagnosticCategory::State, &error, None);
            Vec::new()
        }
    }
}

fn favorite_toggle_requested(state: &mut AppState, item: MediaItem) -> Vec<Effect> {
    if !state.favorites.loaded || state.favorites.pending_mutation.is_some() {
        return Vec::new();
    }
    let mutation = if state
        .favorites
        .entries
        .iter()
        .any(|entry| entry.item.id == item.id)
    {
        FavoriteMutation::Remove
    } else {
        FavoriteMutation::Add
    };
    let generation = match allocate_generation(state) {
        Ok(generation) => generation,
        Err(error) => {
            state.favorites.error = Some(error.clone());
            record_error(state, DiagnosticCategory::State, &error, None);
            return Vec::new();
        }
    };
    state.favorites.generation = generation;
    state.favorites.active_generation = Some(generation);
    state.favorites.error = None;
    state.favorites.pending_mutation = Some(PendingFavoriteMutation {
        media_id: item.id.clone(),
        mutation,
    });
    match mutation {
        FavoriteMutation::Add => vec![Effect::AddFavorite { generation, item }],
        FavoriteMutation::Remove => vec![Effect::RemoveFavorite {
            generation,
            media_id: item.id,
        }],
    }
}

fn favorites_completed(
    state: &mut AppState,
    result: Result<Vec<crate::storage::FavoriteEntry>, AppError>,
) {
    state.favorites.active_generation = None;
    state.favorites.loading = false;
    state.favorites.pending_mutation = None;
    match result {
        Ok(mut entries) => {
            entries.truncate(crate::storage::FAVORITES_LIMIT);
            let previous = state.favorites.selected_id.take();
            let previous_index = previous.as_ref().and_then(|selected| {
                state
                    .favorites
                    .entries
                    .iter()
                    .position(|entry| entry.item.id == *selected)
            });
            state.favorites.entries = entries;
            state.favorites.selected_id = previous
                .filter(|selected| {
                    state
                        .favorites
                        .entries
                        .iter()
                        .any(|entry| entry.item.id == *selected)
                })
                .or_else(|| {
                    let index = previous_index
                        .unwrap_or(0)
                        .min(state.favorites.entries.len().saturating_sub(1));
                    state
                        .favorites
                        .entries
                        .get(index)
                        .map(|entry| entry.item.id.clone())
                });
            state.favorites.loaded = true;
            state.favorites.error = None;
        }
        Err(error) => state.favorites.error = Some(error),
    }
}

fn artwork_requested(state: &mut AppState, url: ArtworkUrl) -> Vec<Effect> {
    state.artwork.requested_url = Some(url.clone());
    state.artwork.ready_url = None;
    state.artwork.active_generation = None;
    state.artwork.loading = false;
    state.artwork.error = None;

    match allocate_generation(state) {
        Ok(generation) => {
            state.artwork.generation = generation;
            state.artwork.active_generation = Some(generation);
            state.artwork.loading = true;
            vec![Effect::FetchArtwork { generation, url }]
        }
        Err(error) => {
            state.artwork.error = Some(error.clone());
            record_error(state, DiagnosticCategory::State, &error, None);
            Vec::new()
        }
    }
}

fn sync_artwork(state: &mut AppState, target: Option<ArtworkUrl>) -> Vec<Effect> {
    match target {
        Some(url) => maybe_request_artwork(state, url),
        None => clear_artwork(state),
    }
}

fn maybe_request_artwork(state: &mut AppState, url: ArtworkUrl) -> Vec<Effect> {
    if state.artwork.requested_url.as_ref() == Some(&url) {
        Vec::new()
    } else {
        artwork_requested(state, url)
    }
}

fn clear_artwork(state: &mut AppState) -> Vec<Effect> {
    if state.artwork.requested_url.is_none()
        && state.artwork.ready_url.is_none()
        && state.artwork.active_generation.is_none()
        && !state.artwork.loading
        && state.artwork.error.is_none()
    {
        return Vec::new();
    }
    state.artwork.requested_url = None;
    state.artwork.ready_url = None;
    state.artwork.active_generation = None;
    state.artwork.loading = false;
    state.artwork.error = None;
    vec![Effect::ClearArtwork]
}

fn media_artwork(media: &MediaItem) -> Option<ArtworkUrl> {
    media
        .artwork_url
        .clone()
        .and_then(|url| ArtworkUrl::try_from(url).ok())
}

enum SelectedArtwork {
    Clear,
    Target(ArtworkUrl),
}

fn sync_selected_artwork(state: &mut AppState, selected: SelectedArtwork) -> Vec<Effect> {
    match selected {
        SelectedArtwork::Clear => clear_artwork(state),
        SelectedArtwork::Target(url) => maybe_request_artwork(state, url),
    }
}

fn selected_artwork(target: Option<ArtworkUrl>) -> SelectedArtwork {
    target.map_or(SelectedArtwork::Clear, SelectedArtwork::Target)
}

fn selected_search_artwork(state: &AppState) -> SelectedArtwork {
    let Some(selected) = state.search.selected_id.as_ref() else {
        return SelectedArtwork::Clear;
    };
    state
        .search
        .items
        .iter()
        .find(|item| item.stable_id() == *selected)
        .map_or(SelectedArtwork::Clear, |item| {
            selected_artwork(match item {
                SearchItem::Playable(media) => media_artwork(media),
                SearchItem::Metadata(metadata) => metadata.artwork_url().cloned(),
            })
        })
}

fn sync_selected_search_artwork(state: &mut AppState) -> Vec<Effect> {
    let selected = selected_search_artwork(state);
    sync_surface_artwork(state, ArtworkSurface::Search, selected)
}

fn selected_chart_artwork(state: &AppState) -> SelectedArtwork {
    let (Some(selected), Some(selected_index)) = (
        state.charts.selected_id.as_ref(),
        state.charts.selected_index,
    ) else {
        return SelectedArtwork::Clear;
    };
    state
        .charts
        .sections
        .iter()
        .flat_map(ChartSection::items)
        .nth(selected_index)
        .filter(|item| &item.id == selected)
        .map_or(SelectedArtwork::Clear, |item| {
            selected_artwork(media_artwork(item))
        })
}

fn sync_selected_chart_artwork(state: &mut AppState) -> Vec<Effect> {
    let selected = selected_chart_artwork(state);
    sync_surface_artwork(state, ArtworkSurface::Charts, selected)
}

fn selected_podcast_artwork(state: &AppState) -> SelectedArtwork {
    let Some(show) = state.podcasts.show.as_ref() else {
        return SelectedArtwork::Clear;
    };
    match state.podcasts.selected_episode.as_ref() {
        Some(selected) => show
            .episodes
            .iter()
            .find(|item| &item.id == selected)
            .map_or(SelectedArtwork::Clear, |item| {
                selected_artwork(media_artwork(item))
            }),
        None => selected_artwork(
            show.artwork_url
                .clone()
                .and_then(|url| ArtworkUrl::try_from(url).ok()),
        ),
    }
}

fn sync_selected_podcast_artwork(state: &mut AppState) -> Vec<Effect> {
    let selected = selected_podcast_artwork(state);
    sync_surface_artwork(state, ArtworkSurface::Podcasts, selected)
}

fn selected_podcast_recommendation_artwork(state: &AppState) -> SelectedArtwork {
    let Some(selected) = state.podcasts.selected_recommendation.as_ref() else {
        return SelectedArtwork::Clear;
    };
    state
        .podcasts
        .recommendations
        .iter()
        .find(|recommendation| recommendation.source_id() == selected)
        .map_or(SelectedArtwork::Clear, |recommendation| {
            selected_artwork(recommendation.artwork_url().cloned())
        })
}

fn sync_selected_podcast_recommendation_artwork(state: &mut AppState) -> Vec<Effect> {
    let selected = selected_podcast_recommendation_artwork(state);
    sync_surface_artwork(state, ArtworkSurface::Podcasts, selected)
}

fn sync_podcast_surface_artwork(state: &mut AppState) -> Vec<Effect> {
    if state.podcasts.show.is_some() {
        sync_selected_podcast_artwork(state)
    } else {
        sync_selected_podcast_recommendation_artwork(state)
    }
}

fn library_item_artwork(item: &crate::provider::LibraryItem) -> Option<ArtworkUrl> {
    let url = match item {
        crate::provider::LibraryItem::Playable(media) => return media_artwork(media),
        crate::provider::LibraryItem::Album(item)
        | crate::provider::LibraryItem::Artist(item)
        | crate::provider::LibraryItem::Playlist(item)
        | crate::provider::LibraryItem::Podcast(item) => item.artwork_url.clone(),
    };
    url.and_then(|url| ArtworkUrl::try_from(url).ok())
}

fn selected_library_artwork(state: &AppState) -> SelectedArtwork {
    let Some(selected) = state.library.selected_id.as_ref() else {
        return SelectedArtwork::Clear;
    };
    state
        .library
        .items
        .iter()
        .find(|item| stable_library_item_id(item) == *selected)
        .map_or(SelectedArtwork::Clear, |item| {
            selected_artwork(library_item_artwork(item))
        })
}

fn sync_selected_library_artwork(state: &mut AppState) -> Vec<Effect> {
    let selected = selected_library_artwork(state);
    sync_surface_artwork(state, ArtworkSurface::Library, selected)
}

fn selected_history_artwork(state: &AppState) -> SelectedArtwork {
    let Some(selected) = state.history.selected_id else {
        return SelectedArtwork::Clear;
    };
    state
        .history
        .entries
        .iter()
        .find(|entry| entry.id == selected)
        .map_or(SelectedArtwork::Clear, |entry| {
            selected_artwork(media_artwork(&entry.item))
        })
}

fn sync_selected_history_artwork(state: &mut AppState) -> Vec<Effect> {
    let selected = selected_history_artwork(state);
    sync_surface_artwork(state, ArtworkSurface::History, selected)
}

fn selected_favorite_artwork(state: &AppState) -> SelectedArtwork {
    let Some(selected) = state.favorites.selected_id.as_ref() else {
        return SelectedArtwork::Clear;
    };
    state
        .favorites
        .entries
        .iter()
        .find(|entry| &entry.item.id == selected)
        .map_or(SelectedArtwork::Clear, |entry| {
            selected_artwork(media_artwork(&entry.item))
        })
}

fn sync_selected_favorite_artwork(state: &mut AppState) -> Vec<Effect> {
    let selected = selected_favorite_artwork(state);
    sync_surface_artwork(state, ArtworkSurface::Favorites, selected)
}

fn sync_surface_artwork(
    state: &mut AppState,
    surface: ArtworkSurface,
    selected: SelectedArtwork,
) -> Vec<Effect> {
    if state.artwork_surface == surface {
        sync_selected_artwork(state, selected)
    } else {
        Vec::new()
    }
}

fn artwork_surface_changed(state: &mut AppState, surface: ArtworkSurface) -> Vec<Effect> {
    state.artwork_surface = surface;
    let selected = match surface {
        ArtworkSurface::Home | ArtworkSurface::Settings => SelectedArtwork::Clear,
        ArtworkSurface::Search
            if state.search.loading
                || state.search.loading_more
                || state.search.error.is_some() =>
        {
            SelectedArtwork::Clear
        }
        ArtworkSurface::Search => selected_search_artwork(state),
        ArtworkSurface::Charts if state.charts.loading || state.charts.error.is_some() => {
            SelectedArtwork::Clear
        }
        ArtworkSurface::Charts => selected_chart_artwork(state),
        ArtworkSurface::Podcasts
            if state.podcasts.loading
                || state.podcasts.recommendations_loading
                || state.podcasts.resolve_loading
                || state.podcasts.error.is_some()
                || state.podcasts.recommendation_error.is_some()
                || state.podcasts.resolve_error.is_some() =>
        {
            SelectedArtwork::Clear
        }
        ArtworkSurface::Podcasts => {
            if state.podcasts.show.is_some() {
                selected_podcast_artwork(state)
            } else {
                selected_podcast_recommendation_artwork(state)
            }
        }
        ArtworkSurface::Library
            if state.library.loading
                || state.library.loading_more
                || state.library.error.is_some() =>
        {
            SelectedArtwork::Clear
        }
        ArtworkSurface::Library => selected_library_artwork(state),
        ArtworkSurface::Favorites => selected_favorite_artwork(state),
        ArtworkSurface::History if state.history.loading || state.history.error.is_some() => {
            SelectedArtwork::Clear
        }
        ArtworkSurface::History => selected_history_artwork(state),
    };
    sync_selected_artwork(state, selected)
}

fn artwork_completed(state: &mut AppState, result: Result<(), AppError>) {
    state.artwork.active_generation = None;
    state.artwork.loading = false;
    match result {
        Ok(()) => {
            state
                .artwork
                .ready_url
                .clone_from(&state.artwork.requested_url);
            state.artwork.error = None;
        }
        Err(error) => {
            state.artwork.ready_url = None;
            state.artwork.error = Some(error);
        }
    }
}

fn radio_fill_completed(
    state: &mut AppState,
    result: Result<Vec<MediaItem>, AppError>,
) -> Vec<Effect> {
    state.pending_radio_generation = None;
    match result {
        Ok(items) => {
            let appended = append_unique_media(state, items);
            if appended.is_empty() {
                return Vec::new();
            }

            let mut effects = if state.resume_radio_after_fill {
                state.resume_radio_after_fill = false;
                resume_first_appended(state, &appended[0])
            } else {
                Vec::new()
            };
            push_session_persist_if_coherent(state, &mut effects);
            effects
        }
        Err(error) => {
            let media_id = state.queue.current().map(|item| item.media().id.clone());
            record_error(state, DiagnosticCategory::Radio, &error, media_id);
            Vec::new()
        }
    }
}

fn radio_enabled_changed(state: &mut AppState, enabled: bool) -> Vec<Effect> {
    if state.queue.radio_enabled() == enabled {
        return Vec::new();
    }

    state.queue.set_radio(enabled);
    if !enabled {
        state.pending_radio_generation = None;
        state.resume_radio_after_fill = false;
    }
    let mut effects = vec![persist(state)];
    if enabled && let Some(effect) = maybe_request_radio_fill(state) {
        effects.push(effect);
    }
    effects
}

fn activate_search_result(state: &mut AppState, index: usize) -> Vec<Effect> {
    let Some(item) = state.search.items.get(index).cloned() else {
        state.diagnostics.push(Diagnostic::new(
            DiagnosticCategory::Selection,
            format!("Search result {index} is unavailable"),
            None,
        ));
        return Vec::new();
    };
    let SearchItem::Playable(media) = item else {
        state.diagnostics.push(Diagnostic::new(
            DiagnosticCategory::Selection,
            "Selected search result is not playable",
            None,
        ));
        return Vec::new();
    };

    let queue_id = stable_queue_item_id(&media.id);
    let _ = state
        .queue
        .append_unique(QueueItem::new(queue_id.clone(), media.clone()));
    if state.queue.select(&queue_id).is_err() {
        state.diagnostics.push(Diagnostic::new(
            DiagnosticCategory::State,
            "The selected queue item could not be activated",
            Some(media.id),
        ));
        return Vec::new();
    }

    let Some(canonical_media) = state.queue.current().map(|item| item.media().clone()) else {
        state.diagnostics.push(Diagnostic::new(
            DiagnosticCategory::State,
            "The selected queue item has no canonical media",
            Some(media.id),
        ));
        return Vec::new();
    };

    let mut effects = begin_playback(state, canonical_media);
    push_session_persist_if_coherent(state, &mut effects);
    if let Some(effect) = maybe_request_radio_fill(state) {
        effects.push(effect);
    }
    effects
}

fn enqueue_selected_search_result(state: &mut AppState) -> Vec<Effect> {
    let Some(selected_id) = state.search.selected_id.clone() else {
        state.diagnostics.push(Diagnostic::new(
            DiagnosticCategory::Selection,
            "No search result is selected",
            None,
        ));
        return Vec::new();
    };
    let Some(item) = state
        .search
        .items
        .iter()
        .find(|item| item.stable_id() == selected_id)
        .cloned()
    else {
        state.diagnostics.push(Diagnostic::new(
            DiagnosticCategory::Selection,
            "The selected search result is unavailable",
            None,
        ));
        return Vec::new();
    };
    let SearchItem::Playable(media) = item else {
        state.diagnostics.push(Diagnostic::new(
            DiagnosticCategory::Selection,
            "Selected search result is not playable",
            None,
        ));
        return Vec::new();
    };

    enqueue_media(state, media)
}

fn enqueue_media(state: &mut AppState, media: MediaItem) -> Vec<Effect> {
    let queue_id = stable_queue_item_id(&media.id);
    if !state.queue.append_unique(QueueItem::new(queue_id, media)) {
        return Vec::new();
    }

    let previous_playback = state.playback.current.clone();
    initialize_stopped_playback_from_queue(state);
    let mut effects = vec![persist(state)];
    if state.playback.current != previous_playback {
        let artwork = state
            .queue
            .current()
            .and_then(|item| media_artwork(item.media()));
        effects.extend(sync_artwork(state, artwork));
    }
    effects
}

fn initialize_stopped_playback_from_queue(state: &mut AppState) {
    if state.playback.current.is_some() {
        return;
    }
    let Some(media) = state.queue.current().map(QueueItem::media) else {
        return;
    };
    state.playback.current = Some(media.id.clone());
    state.playback.status = PlaybackStatus::Stopped;
    state.playback.position_ms = 0;
    state.playback.duration_ms = media.duration_ms;
}

fn play_media_list(
    state: &mut AppState,
    items: Vec<MediaItem>,
    selected_id: &MediaId,
    shuffle_seed: Option<u64>,
) -> Vec<Effect> {
    let Ok(candidate) =
        Queue::from_explicit_list(items, selected_id, state.queue.repeat(), shuffle_seed)
    else {
        state.diagnostics.push(Diagnostic::new(
            DiagnosticCategory::State,
            "The playable list could not replace the queue; playback was not changed",
            None,
        ));
        return Vec::new();
    };

    state.queue = candidate;
    state.pending_radio_generation = None;
    state.resume_radio_after_fill = false;
    play_current_queue_item(state)
}

fn play_queue_item(state: &mut AppState, id: &QueueItemId) -> Vec<Effect> {
    if let Err(error) = state.queue.select(id) {
        state.diagnostics.push(Diagnostic::new(
            DiagnosticCategory::Selection,
            error.to_string(),
            None,
        ));
        return Vec::new();
    }
    play_current_queue_item(state)
}

fn play_current_queue_item(state: &mut AppState) -> Vec<Effect> {
    let Some(media) = state.queue.current().map(|item| item.media().clone()) else {
        state.diagnostics.push(Diagnostic::new(
            DiagnosticCategory::Selection,
            "The queue has no playable item",
            None,
        ));
        return Vec::new();
    };

    let mut effects = begin_playback(state, media);
    push_session_persist_if_coherent(state, &mut effects);
    if let Some(effect) = maybe_request_radio_fill(state) {
        effects.push(effect);
    }
    effects
}

fn toggle_playback(state: &mut AppState) -> Vec<Effect> {
    match state.playback.status {
        PlaybackStatus::Playing => vec![Effect::Player(super::PlayerCommand::Pause)],
        PlaybackStatus::Paused => vec![Effect::Player(super::PlayerCommand::Resume)],
        PlaybackStatus::Stopped | PlaybackStatus::Failed if state.queue.current().is_some() => {
            play_current_queue_item(state)
        }
        _ => Vec::new(),
    }
}

fn seek_relative(state: &AppState, seconds: i64) -> Vec<Effect> {
    let Some(current_media_id) = state.playback.current.as_ref() else {
        return Vec::new();
    };
    if state
        .queue
        .current()
        .is_none_or(|item| &item.media().id != current_media_id)
    {
        return Vec::new();
    }

    let requested_ms = seconds.unsigned_abs().saturating_mul(1_000);
    let target_ms = if seconds.is_negative() {
        state.playback.position_ms.saturating_sub(requested_ms)
    } else {
        state.playback.position_ms.saturating_add(requested_ms)
    };
    let bounded_ms = state
        .playback
        .duration_ms
        .map_or(target_ms, |duration_ms| target_ms.min(duration_ms));
    let delta_ms = i128::from(bounded_ms) - i128::from(state.playback.position_ms);
    let rounded_seconds = if delta_ms.is_positive() {
        delta_ms.saturating_add(999) / 1_000
    } else {
        delta_ms.saturating_sub(999) / 1_000
    };
    let Ok(seconds) = i64::try_from(rounded_seconds) else {
        return Vec::new();
    };
    if seconds == 0 {
        Vec::new()
    } else {
        vec![Effect::Player(super::PlayerCommand::SeekRelative {
            seconds,
        })]
    }
}

#[derive(Clone, Copy)]
enum QueueDirection {
    Next,
    Previous,
}

fn navigate_queue(state: &mut AppState, direction: QueueDirection) -> Vec<Effect> {
    let selected = match direction {
        QueueDirection::Next => state.queue.next(),
        QueueDirection::Previous => state.queue.previous(),
    }
    .is_some();
    if !selected {
        return Vec::new();
    }
    play_current_queue_item(state)
}

fn player_status_changed(
    state: &mut AppState,
    generation: Generation,
    status: PlaybackStatus,
) -> Vec<Effect> {
    if state.current_attempt_generation != Some(generation) {
        return Vec::new();
    }

    state.playback.status = status;
    if status == PlaybackStatus::Playing {
        let mut effects = Vec::new();
        if let Some(item) = state.queue.current().map(|item| item.media().clone()) {
            if state.history_recorded_generation != Some(generation) {
                state.history_recorded_generation = Some(generation);
                effects.push(Effect::RecordHistory { item: item.clone() });
            }
            if state.notifications_enabled()
                && state.notification_emitted_generation != Some(generation)
            {
                state.notification_emitted_generation = Some(generation);
                effects.push(Effect::Notify(
                    crate::notifications::NowPlayingNotification::from_media(generation, &item),
                ));
            }
        }
        return effects;
    }
    if matches!(status, PlaybackStatus::Stopped | PlaybackStatus::Failed) {
        let podcast_checkpoint = state.queue.current().and_then(|item| {
            (item.media().kind == MediaKind::PodcastEpisode
                && state.playback.current.as_ref() == Some(&item.media().id))
            .then_some(state.current_podcast_epoch)
            .flatten()
            .map(|playback_epoch| {
                Effect::SavePodcastProgress(PodcastProgressCheckpoint::new(
                    item.media().id.clone(),
                    playback_epoch,
                    state.playback.position_ms,
                    state.playback.duration_ms,
                    false,
                ))
            })
        });
        state.active_resolve_generation = None;
        state.current_attempt_generation = None;
        state.current_podcast_epoch = None;
        state.resume_radio_after_fill = false;
        state.player_presentation.preview_url = None;
        state.player_presentation.analysis_url = None;
        // Failed playback keeps the last observed presentation as a frozen
        // diagnostic snapshot; closing the generation rejects later updates.
        if status == PlaybackStatus::Stopped {
            clear_player_presentation(state);
        }
        let clear_lyrics = invalidate_lyrics(state).then_some(Effect::ClearLyrics);
        return podcast_checkpoint
            .into_iter()
            .chain(clear_lyrics)
            .chain(std::iter::once(persist(state)))
            .collect();
    }
    Vec::new()
}

fn resolve_failed(state: &mut AppState, error: &AppError) -> Vec<Effect> {
    let clear_lyrics = invalidate_lyrics(state).then_some(Effect::ClearLyrics);
    state.active_resolve_generation = None;
    state.current_attempt_generation = None;
    state.current_podcast_epoch = None;
    state.resume_radio_after_fill = false;
    state.history_recorded_generation = None;
    state.notification_emitted_generation = None;
    clear_player_presentation(state);
    state.playback.status = PlaybackStatus::Failed;
    let failed_media_id = state.playback.current.clone();
    record_error(
        state,
        DiagnosticCategory::Resolve,
        error,
        failed_media_id.clone(),
    );

    if !state.behavior.auto_skip_unavailable {
        return clear_lyrics
            .into_iter()
            .chain(std::iter::once(persist(state)))
            .collect();
    }

    let next = state.queue.next().cloned();
    let Some(next) = next.filter(|item| Some(&item.media().id) != failed_media_id.as_ref()) else {
        state.resume_radio_after_fill = state.queue.needs_radio_fill(RADIO_FILL_THRESHOLD);
        let mut effects = clear_lyrics
            .into_iter()
            .chain(std::iter::once(persist(state)))
            .collect::<Vec<_>>();
        if let Some(effect) = maybe_request_radio_fill(state) {
            effects.push(effect);
        }
        return effects;
    };

    let mut effects = clear_lyrics.into_iter().collect::<Vec<_>>();
    effects.extend(begin_playback(state, next.media().clone()));
    push_session_persist_if_coherent(state, &mut effects);
    if let Some(effect) = maybe_request_radio_fill(state) {
        effects.push(effect);
    }
    effects
}

fn player_ended(state: &mut AppState) -> Vec<Effect> {
    let clear_lyrics = invalidate_lyrics(state).then_some(Effect::ClearLyrics);
    let podcast_checkpoint = state.queue.current().and_then(|item| {
        (item.media().kind == MediaKind::PodcastEpisode
            && state.playback.current.as_ref() == Some(&item.media().id))
        .then_some(state.current_podcast_epoch)
        .flatten()
        .map(|playback_epoch| {
            let position_ms = state
                .playback
                .duration_ms
                .map_or(state.playback.position_ms, |duration| {
                    duration.max(state.playback.position_ms)
                });
            Effect::SavePodcastProgress(PodcastProgressCheckpoint::new(
                item.media().id.clone(),
                playback_epoch,
                position_ms,
                state.playback.duration_ms,
                true,
            ))
        })
    });
    state.active_resolve_generation = None;
    state.current_attempt_generation = None;
    state.current_podcast_epoch = None;
    let next = state.queue.next().cloned();
    // Save-before-load is the podcast replay ordering contract. The runtime
    // must preserve this vector order on one FIFO storage lane.
    let mut effects = podcast_checkpoint
        .into_iter()
        .chain(clear_lyrics)
        .collect::<Vec<_>>();
    if let Some(next) = next {
        effects.extend(begin_playback(state, next.media().clone()));
        push_session_persist_if_coherent(state, &mut effects);
    } else {
        state.playback.status = PlaybackStatus::Stopped;
        state.resume_radio_after_fill = state.queue.needs_radio_fill(RADIO_FILL_THRESHOLD);
        clear_player_presentation(state);
        effects.push(persist(state));
    }

    if let Some(effect) = maybe_request_radio_fill(state) {
        effects.push(effect);
    }
    effects
}

fn begin_playback(state: &mut AppState, media: MediaItem) -> Vec<Effect> {
    if media.kind != MediaKind::PodcastEpisode {
        state.podcasts.pending_progress_generation = None;
        state.podcasts.pending_media = None;
        state.resume_radio_after_fill = false;
        return begin_resolution_at(state, media, 0, None);
    }

    match allocate_generation(state) {
        Ok(generation) => {
            let outgoing_checkpoint = active_podcast_transition_checkpoint(state);
            let clear_lyrics = invalidate_lyrics(state).then_some(Effect::ClearLyrics);
            invalidate_attempt_for_pending_podcast(state);
            state.resume_radio_after_fill = false;
            let media_id = media.id.clone();
            state.podcasts.pending_progress_generation = Some(generation);
            state.podcasts.pending_media = Some(media);
            let mut effects = outgoing_checkpoint
                .into_iter()
                .chain(clear_lyrics)
                .collect::<Vec<_>>();
            effects.push(Effect::LoadPodcastProgress {
                generation,
                media_id,
            });
            effects
        }
        Err(error) => {
            restore_prior_queue_selection(state);
            let media_id = media.id;
            state.podcasts.error = Some(error.clone());
            record_error(state, DiagnosticCategory::State, &error, Some(media_id));
            Vec::new()
        }
    }
}

fn active_podcast_transition_checkpoint(state: &AppState) -> Option<Effect> {
    let playback_epoch = state
        .current_attempt_generation
        .and(state.current_podcast_epoch)?;
    let media_id = state.playback.current.clone()?;
    Some(Effect::SavePodcastProgress(PodcastProgressCheckpoint::new(
        media_id,
        playback_epoch,
        state.playback.position_ms,
        state.playback.duration_ms,
        false,
    )))
}

fn invalidate_attempt_for_pending_podcast(state: &mut AppState) {
    state.active_resolve_generation = None;
    state.current_attempt_generation = None;
    state.current_podcast_epoch = None;
    state.history_recorded_generation = None;
    state.notification_emitted_generation = None;
    clear_player_presentation(state);
}

fn restore_prior_queue_selection(state: &mut AppState) {
    let Some(queue_id) = state
        .podcasts
        .pending_media
        .as_ref()
        .map(|media| &media.id)
        .into_iter()
        .chain(state.playback.current.as_ref())
        .find_map(|media_id| {
            state
                .queue
                .items()
                .iter()
                .find(|item| &item.media().id == media_id)
                .map(|item| item.id().clone())
        })
    else {
        return;
    };
    let _ = state.queue.select(&queue_id);
}

fn begin_resolution_at(
    state: &mut AppState,
    media: MediaItem,
    resume_position_ms: u64,
    podcast_epoch: Option<u64>,
) -> Vec<Effect> {
    let artwork = media_artwork(&media);
    let clear_lyrics = invalidate_lyrics(state).then_some(Effect::ClearLyrics);
    state.active_resolve_generation = None;
    state.current_attempt_generation = None;
    state.current_podcast_epoch = podcast_epoch;
    state.resume_radio_after_fill = false;
    state.history_recorded_generation = None;
    state.notification_emitted_generation = None;
    clear_player_presentation(state);
    state.playback.current = Some(media.id.clone());
    state.playback.position_ms = resume_position_ms;
    state.playback.duration_ms = media.duration_ms;
    if !state.dependencies.playback_available() {
        state.current_podcast_epoch = None;
        state.playback.status = PlaybackStatus::Failed;
        let error = AppError::new(
            AppErrorCategory::PlaybackUnavailable,
            "Playback dependencies are unavailable; browsing remains available",
        );
        record_error(state, DiagnosticCategory::Resolve, &error, Some(media.id));
        return clear_lyrics
            .into_iter()
            .chain(sync_artwork(state, artwork))
            .collect();
    }

    let mut effects = clear_lyrics.into_iter().collect::<Vec<_>>();
    effects.extend(match allocate_generation(state) {
        Ok(generation) => {
            state.active_resolve_generation = Some(generation);
            state.current_attempt_generation = Some(generation);
            state.playback.status = PlaybackStatus::Resolving;
            let mut resolution = vec![Effect::Resolve {
                generation,
                item: media.clone(),
                start_ms: (resume_position_ms > 0).then_some(resume_position_ms),
            }];
            if state.lyrics_enabled && matches!(media.kind, MediaKind::Song | MediaKind::Video) {
                resolution.extend(request_lyrics(state, media));
            }
            resolution
        }
        Err(error) => {
            state.active_resolve_generation = None;
            state.current_attempt_generation = None;
            state.current_podcast_epoch = None;
            state.playback.status = PlaybackStatus::Failed;
            record_error(state, DiagnosticCategory::State, &error, Some(media.id));
            Vec::new()
        }
    });
    effects.extend(sync_artwork(state, artwork));
    effects
}

fn set_lyrics_loading(state: &mut AppState, generation: Generation, media_id: &MediaId) {
    state.lyrics.active_generation = Some(generation);
    state.lyrics.media_id = Some(media_id.clone());
    state.lyrics.loading = true;
    state.lyrics.error = None;
    state.lyrics.document = None;
    state.lyrics.active_line_index = None;
}

fn request_lyrics(state: &mut AppState, item: MediaItem) -> Vec<Effect> {
    if !state.lyrics_enabled
        || !matches!(item.kind, MediaKind::Song | MediaKind::Video)
        || state.playback.current.as_ref() != Some(&item.id)
    {
        return Vec::new();
    }
    match allocate_generation(state) {
        Ok(generation) => {
            set_lyrics_loading(state, generation, &item.id);
            vec![Effect::LoadLyrics {
                generation,
                item: item.into(),
            }]
        }
        Err(error) => {
            record_error(state, DiagnosticCategory::State, &error, Some(item.id));
            Vec::new()
        }
    }
}

fn invalidate_lyrics(state: &mut AppState) -> bool {
    let had_work = state.lyrics.active_generation.is_some() || state.lyrics.media_id.is_some();
    state.lyrics = super::LyricsState::default();
    had_work
}

fn update_active_lyric_line(state: &mut AppState) {
    state.lyrics.active_line_index = state.lyrics.document.as_ref().and_then(|document| {
        let position_ms = state.playback.position_ms;
        let index = document
            .timed()
            .partition_point(|line| line.start_ms() <= position_ms)
            .checked_sub(1)?;
        document.timed()[index]
            .end_ms()
            .is_none_or(|end_ms| position_ms < end_ms)
            .then_some(index)
    });
}

fn maybe_request_radio_fill(state: &mut AppState) -> Option<Effect> {
    if state.pending_radio_generation.is_some()
        || !state.queue.needs_radio_fill(RADIO_FILL_THRESHOLD)
    {
        return None;
    }
    let seed = state.queue.current().map(|item| item.media().id.clone())?;

    match allocate_generation(state) {
        Ok(generation) => {
            state.pending_radio_generation = Some(generation);
            Some(Effect::FillRadio { generation, seed })
        }
        Err(error) => {
            record_error(state, DiagnosticCategory::State, &error, Some(seed));
            None
        }
    }
}

fn append_unique_media(state: &mut AppState, items: Vec<MediaItem>) -> Vec<QueueItemId> {
    let mut appended = Vec::new();
    for media in items {
        let id = stable_queue_item_id(&media.id);
        if state.queue.append_unique(QueueItem::new(id.clone(), media)) {
            appended.push(id);
        }
    }
    appended
}

fn resume_first_appended(state: &mut AppState, first_appended: &QueueItemId) -> Vec<Effect> {
    if state.queue.select(first_appended).is_err() {
        state.diagnostics.push(Diagnostic::new(
            DiagnosticCategory::State,
            "The first appended radio item could not be selected",
            None,
        ));
        return Vec::new();
    }

    let Some(media) = state.queue.current().map(|item| item.media().clone()) else {
        state.diagnostics.push(Diagnostic::new(
            DiagnosticCategory::State,
            "The selected radio item has no canonical media",
            None,
        ));
        return Vec::new();
    };
    begin_playback(state, media)
}

fn allocate_generation(state: &mut AppState) -> Result<Generation, AppError> {
    let Some(generation) = state.last_generation.checked_next() else {
        return Err(AppError::new(
            AppErrorCategory::State,
            "Operation generation space is exhausted",
        ));
    };
    state.last_generation = generation;
    Ok(generation)
}

fn normalize_effective_volume(volume: f64) -> f64 {
    if volume.is_nan() || volume == f64::NEG_INFINITY {
        0.0
    } else if volume == f64::INFINITY {
        100.0
    } else {
        volume.clamp(0.0, 100.0)
    }
}

fn clear_player_presentation(state: &mut AppState) {
    state.player_presentation.effective_volume = 0.0;
    state.player_presentation.fade = None;
    state.player_presentation.quality = super::ResolverQuality::default();
    state.player_presentation.preview_url = None;
    state.player_presentation.analysis_url = None;
}

fn record_error(
    state: &mut AppState,
    category: DiagnosticCategory,
    error: &AppError,
    media_id: Option<MediaId>,
) {
    state
        .diagnostics
        .push(Diagnostic::new(category, error.message(), media_id));
}

fn push_session_persist_if_coherent(state: &AppState, effects: &mut Vec<Effect>) {
    if session_persistence_is_allowed(state) {
        effects.push(persist(state));
    }
}

fn session_persistence_is_allowed(state: &AppState) -> bool {
    let queue_current = state.queue.current().map(|item| &item.media().id);
    state.podcasts.pending_progress_generation.is_none()
        && queue_current == state.playback.current.as_ref()
}

fn persist(state: &AppState) -> Effect {
    Effect::Persist(SessionCheckpoint {
        queue: state.queue.snapshot(),
        playback: state.playback.clone(),
    })
}

#[cfg(test)]
mod tests {
    use crate::{
        domain::{MediaId, MediaKind, RegionCode},
        podcast_rankings::parse_apple_top_shows,
        queue::QueueItem,
        storage::{PodcastProgress, SqliteStorage, Storage},
    };

    use super::*;

    fn media(video_id: &str) -> MediaItem {
        MediaItem {
            id: MediaId {
                provider: "youtube-music".to_owned(),
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

    fn recommendation_page() -> crate::podcast_rankings::PodcastRecommendationPage {
        match parse_apple_top_shows(
            br#"{"feed":{"country":"US","results":[{"id":"daily","name":"The Daily","artistName":"NYT"}]}}"#,
        ) {
            Ok(page) => page,
            Err(error) => panic!("valid recommendation fixture: {error}"),
        }
    }

    #[test]
    fn podcast_recommendation_generation_paths_do_not_alias_at_exhaustion() {
        let us = RegionCode::parse("US").unwrap_or_else(|error| panic!("valid region: {error}"));
        let mut request_state = AppState {
            last_generation: Generation::new(u64::MAX),
            ..AppState::default()
        };
        request_state.podcasts.recommendation_generation = Generation::new(7);
        let (request_state, effects) = reduce(
            request_state,
            Action::PodcastRecommendationsRequested { region: us },
        );
        assert!(effects.is_empty());
        assert_eq!(
            request_state.podcasts.recommendation_generation(),
            Generation::new(7)
        );
        assert_eq!(
            request_state
                .podcasts
                .recommendation_error()
                .map(AppError::category),
            Some(AppErrorCategory::State)
        );

        let page = recommendation_page();
        let selected = page.items()[0].source_id().clone();
        let mut resolve_state = AppState {
            last_generation: Generation::new(u64::MAX),
            ..AppState::default()
        };
        resolve_state.podcasts.recommendations = page.items().to_vec();
        resolve_state.podcasts.selected_recommendation = Some(selected);
        resolve_state.podcasts.resolve_generation = Generation::new(8);
        let (resolve_state, effects) =
            reduce(resolve_state, Action::OpenSelectedPodcastRecommendation);
        assert!(effects.is_empty());
        assert_eq!(
            resolve_state.podcasts.resolve_generation(),
            Generation::new(8)
        );
        assert_eq!(
            resolve_state
                .podcasts
                .resolve_error()
                .map(AppError::category),
            Some(AppErrorCategory::State)
        );

        let active_resolve = Generation::new(9);
        let mut detail_state = AppState {
            last_generation: Generation::new(u64::MAX),
            ..AppState::default()
        };
        detail_state.podcasts.active_resolve_generation = Some(active_resolve);
        detail_state.podcasts.resolve_loading = true;
        detail_state.podcasts.generation = Generation::new(6);
        let (detail_state, effects) = reduce(
            detail_state,
            Action::PodcastRecommendationResolved {
                generation: active_resolve,
                result: Ok(PodcastProviderId::new("provider-id".to_owned())
                    .unwrap_or_else(|| panic!("valid provider id"))),
            },
        );
        assert!(effects.is_empty());
        assert_eq!(detail_state.podcasts.generation(), Generation::new(6));
        assert_eq!(
            detail_state
                .podcasts
                .resolve_error()
                .map(AppError::category),
            Some(AppErrorCategory::State)
        );
    }

    #[test]
    fn podcast_recommendation_match_errors_are_normalized_without_external_text() {
        let generation = Generation::new(5);
        let mut state = AppState::default();
        state.podcasts.active_resolve_generation = Some(generation);
        state.podcasts.resolve_loading = true;

        let (state, effects) = reduce(
            state,
            Action::PodcastRecommendationResolved {
                generation,
                result: Err(AppError::new(
                    AppErrorCategory::Search,
                    "Sensitive external title and provider id",
                )),
            },
        );

        assert!(effects.is_empty());
        let Some(error) = state.podcasts.resolve_error() else {
            panic!("safe match error");
        };
        assert_eq!(error.category(), AppErrorCategory::Podcast);
        assert_eq!(
            error.message(),
            "Podcast recommendation could not be matched"
        );
    }

    #[test]
    fn generation_exhaustion_does_not_alias_or_emit_an_effect() {
        let state = AppState {
            last_generation: Generation::new(u64::MAX),
            ..AppState::default()
        };

        let (state, effects) = reduce(
            state,
            Action::ChartsRequested {
                region: RegionCode::default(),
            },
        );

        assert!(effects.is_empty());
        assert_eq!(state.last_generation, Generation::new(u64::MAX));
        assert_eq!(state.charts.generation(), Generation::default());
        assert!(state.active_chart_generation().is_none());
        assert!(!state.charts.loading());
        let Some(error) = state.charts.error() else {
            panic!("generation exhaustion must be visible as a safe state error");
        };
        assert_eq!(error.category(), AppErrorCategory::State);
        let Some(diagnostic) = state.diagnostics().last() else {
            panic!("generation exhaustion must add a diagnostic");
        };
        assert_eq!(diagnostic.category(), DiagnosticCategory::State);
        assert_eq!(diagnostic.message(), error.message());
    }

    #[test]
    fn resolve_generation_exhaustion_does_not_alias() {
        let item = media("resolve-boundary");
        let mut state = AppState {
            last_generation: Generation::new(u64::MAX),
            ..AppState::default()
        };

        let effects = begin_playback(&mut state, item.clone());

        assert!(effects.is_empty());
        assert_eq!(state.last_generation, Generation::new(u64::MAX));
        assert!(state.current_resolve_generation().is_none());
        assert!(state.current_attempt_generation().is_none());
        assert_eq!(state.playback.current.as_ref(), Some(&item.id));
        assert_eq!(state.playback.status, PlaybackStatus::Failed);
        let Some(diagnostic) = state.diagnostics().last() else {
            panic!("resolve generation exhaustion must add a diagnostic");
        };
        assert_eq!(diagnostic.category(), DiagnosticCategory::State);
    }

    #[test]
    fn pending_podcast_generation_exhaustion_preserves_prior_pending_intent() {
        let mut first = media("pending-before-exhaustion");
        first.kind = MediaKind::PodcastEpisode;
        let mut second = media("replacement-at-exhaustion");
        second.kind = MediaKind::PodcastEpisode;
        let mut state = AppState::default();
        for item in [first.clone(), second.clone()] {
            (state, _) = reduce(state, Action::EnqueueMedia { item });
        }
        let (mut state, effects) = reduce(
            state,
            Action::PlayQueueItem {
                id: stable_queue_item_id(&first.id),
            },
        );
        let [
            Effect::LoadPodcastProgress {
                generation: pending_generation,
                media_id,
            },
        ] = effects.as_slice()
        else {
            panic!("first podcast must remain pending on its progress generation");
        };
        assert_eq!(media_id, &first.id);
        let pending_generation = *pending_generation;
        assert!(state.current_attempt_generation().is_none());
        state.last_generation = Generation::new(u64::MAX);

        let (state, effects) = reduce(
            state,
            Action::PlayQueueItem {
                id: stable_queue_item_id(&second.id),
            },
        );

        assert!(effects.is_empty());
        assert_eq!(
            state.podcasts.pending_progress_generation(),
            Some(pending_generation)
        );
        assert_eq!(state.podcasts.pending_media.as_ref(), Some(&first));
        assert_eq!(state.queue.current().map(QueueItem::media), Some(&first));
        assert!(state.current_attempt_generation().is_none());
        assert!(state.current_resolve_generation().is_none());

        let diagnostics_before_completion = state.diagnostics().len();
        let (state, effects) = reduce(
            state,
            Action::PodcastProgressLoaded {
                generation: pending_generation,
                progress: None,
            },
        );
        assert_eq!(state.playback.current.as_ref(), Some(&first.id));
        assert!(state.podcasts.pending_progress_generation().is_none());
        assert!(state.podcasts.pending_media.is_none());
        assert_eq!(state.playback.status, PlaybackStatus::Failed);
        assert_eq!(state.diagnostics().len(), diagnostics_before_completion + 1);
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, Effect::Persist(_)))
        );
    }

    #[test]
    fn unqueued_pending_podcast_exhaustion_restores_prior_playback_selection() {
        let prior = media("prior-playback-selection");
        let mut pending = media("unqueued-pending-episode");
        pending.kind = MediaKind::PodcastEpisode;
        let mut replacement = media("queued-podcast-replacement");
        replacement.kind = MediaKind::PodcastEpisode;
        let mut state = AppState::default();
        for item in [prior.clone(), replacement.clone()] {
            (state, _) = reduce(state, Action::EnqueueMedia { item });
        }
        state.podcasts.show = Some(crate::provider::Podcast {
            id: "pending-show".to_owned(),
            title: "Pending Show".to_owned(),
            creators: vec!["Host".to_owned()],
            description: None,
            artwork_url: None,
            episodes: vec![pending.clone()],
        });
        let (state, _) = reduce(
            state,
            Action::PlayQueueItem {
                id: stable_queue_item_id(&prior.id),
            },
        );
        let active_generation = state
            .current_attempt_generation()
            .unwrap_or_else(|| panic!("prior song must have an active playback attempt"));
        let (state, _) = reduce(
            state,
            Action::PlayerStatusChanged {
                generation: active_generation,
                status: PlaybackStatus::Playing,
            },
        );
        let (mut state, effects) = reduce(
            state,
            Action::PlayPodcastEpisode {
                media_id: pending.id.clone(),
            },
        );
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, Effect::ClearLyrics))
        );
        let Some((pending_generation, media_id)) = effects.iter().find_map(|effect| match effect {
            Effect::LoadPodcastProgress {
                generation,
                media_id,
            } => Some((generation, media_id)),
            _ => None,
        }) else {
            panic!("unqueued podcast must remain pending on its progress generation");
        };
        assert_eq!(media_id, &pending.id);
        let pending_generation = *pending_generation;
        assert!(
            state
                .queue
                .items()
                .iter()
                .all(|item| item.media().id != pending.id)
        );
        assert_eq!(state.queue.current().map(QueueItem::media), Some(&prior));
        assert_eq!(state.playback.current.as_ref(), Some(&prior.id));
        assert!(state.current_attempt_generation().is_none());
        state.last_generation = Generation::new(u64::MAX);

        let (state, effects) = reduce(
            state,
            Action::PlayQueueItem {
                id: stable_queue_item_id(&replacement.id),
            },
        );

        assert!(effects.is_empty());
        assert_eq!(
            state.podcasts.pending_progress_generation(),
            Some(pending_generation)
        );
        assert_eq!(state.podcasts.pending_media.as_ref(), Some(&pending));
        assert_eq!(state.queue.current().map(QueueItem::media), Some(&prior));
        assert_eq!(state.playback.current.as_ref(), Some(&prior.id));
        assert!(state.current_attempt_generation().is_none());
        assert!(state.current_resolve_generation().is_none());

        let (state, effects) = reduce(
            state,
            Action::PodcastProgressLoaded {
                generation: pending_generation,
                progress: None,
            },
        );
        assert!(state.podcasts.pending_progress_generation().is_none());
        assert!(state.podcasts.pending_media.is_none());
        assert_eq!(state.queue.current().map(QueueItem::media), Some(&pending));
        assert_eq!(state.playback.current.as_ref(), Some(&pending.id));
        assert_eq!(state.playback.status, PlaybackStatus::Failed);
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, Effect::Persist(_)))
        );
    }

    #[test]
    fn podcast_progress_generation_exhaustion_preserves_active_attempt_and_selection() {
        let outgoing = media("active-before-podcast-exhaustion");
        let mut incoming = media("podcast-generation-boundary");
        incoming.kind = MediaKind::PodcastEpisode;
        let mut state = AppState::default();
        for item in [outgoing.clone(), incoming.clone()] {
            (state, _) = reduce(state, Action::EnqueueMedia { item });
        }
        let (mut state, _) = reduce(
            state,
            Action::PlayQueueItem {
                id: stable_queue_item_id(&outgoing.id),
            },
        );
        let active_generation = state
            .current_attempt_generation()
            .unwrap_or_else(|| panic!("outgoing attempt must be active"));
        state.last_generation = Generation::new(u64::MAX);

        let (state, effects) = reduce(
            state,
            Action::PlayQueueItem {
                id: stable_queue_item_id(&incoming.id),
            },
        );

        assert!(
            effects
                .iter()
                .all(|effect| !matches!(effect, Effect::LoadPodcastProgress { .. }))
        );
        assert_eq!(state.current_attempt_generation(), Some(active_generation));
        assert_eq!(state.current_resolve_generation(), Some(active_generation));
        assert_eq!(state.playback.current.as_ref(), Some(&outgoing.id));
        assert_eq!(state.queue.current().map(QueueItem::media), Some(&outgoing));
        assert!(state.podcasts.pending_progress_generation().is_none());
        assert!(state.podcasts.pending_media.is_none());
    }

    #[test]
    fn radio_generation_exhaustion_does_not_alias() {
        let item = media("radio-boundary");
        let queue_item = QueueItem::new(stable_queue_item_id(&item.id), item);
        let mut state = AppState {
            last_generation: Generation::new(u64::MAX),
            ..AppState::default()
        };
        assert!(state.queue.append_unique(queue_item));
        state.queue.set_radio(true);

        let effect = maybe_request_radio_fill(&mut state);

        assert!(effect.is_none());
        assert_eq!(state.last_generation, Generation::new(u64::MAX));
        assert!(state.pending_radio_generation().is_none());
        let Some(diagnostic) = state.diagnostics().last() else {
            panic!("radio generation exhaustion must add a diagnostic");
        };
        assert_eq!(diagnostic.category(), DiagnosticCategory::State);
    }

    #[test]
    fn resolve_generation_exhaustion_preserves_completed_podcast_progress_row()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::TempDir::new()?;
        let path = directory.path().join("generation-exhaustion.sqlite3");
        let mut episode = media("generation-exhaustion-podcast");
        episode.kind = MediaKind::PodcastEpisode;
        let prior = PodcastProgress {
            video_id: episode.id.video_id.clone(),
            playback_epoch: 7,
            position_ms: 180_000,
            duration_ms: episode.duration_ms,
            played: true,
            updated_at: 100,
        };
        let mut storage = SqliteStorage::open(&path)?;
        storage.save_podcast_progress(&prior)?;

        let (state, _) = reduce(
            AppState::default(),
            Action::EnqueueMedia {
                item: episode.clone(),
            },
        );
        let (mut state, start_effects) = reduce(
            state,
            Action::PlayQueueItem {
                id: stable_queue_item_id(&episode.id),
            },
        );
        let [
            Effect::LoadPodcastProgress {
                generation: progress_generation,
                media_id,
            },
        ] = start_effects.as_slice()
        else {
            panic!("podcast fixture must load stored progress");
        };
        assert_eq!(media_id, &episode.id);
        let loaded = storage.load_podcast_progress(&episode.id.video_id)?;
        state.last_generation = Generation::new(u64::MAX);

        let (state, effects) = reduce(
            state,
            Action::PodcastProgressLoaded {
                generation: *progress_generation,
                progress: loaded,
            },
        );
        let mut progress_save_count = 0;
        for effect in &effects {
            match effect {
                Effect::SavePodcastProgress(checkpoint) => {
                    progress_save_count += 1;
                    storage.save_podcast_progress(&PodcastProgress {
                        video_id: checkpoint.media_id().video_id.clone(),
                        playback_epoch: checkpoint.playback_epoch(),
                        position_ms: checkpoint.position_ms(),
                        duration_ms: checkpoint.duration_ms(),
                        played: checkpoint.played(),
                        updated_at: 200,
                    })?;
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
        assert!(state.podcasts.pending_progress_generation().is_none());
        assert!(state.podcasts.pending_media.is_none());
        let Some(diagnostic) = state.diagnostics().last() else {
            panic!("resolve generation exhaustion must add a diagnostic");
        };
        assert_eq!(diagnostic.category(), DiagnosticCategory::State);
        assert_eq!(diagnostic.media_id(), Some(&episode.id));
        Ok(())
    }

    #[test]
    fn podcast_epoch_exhaustion_does_not_wrap_or_start_playback() {
        let mut episode = media("podcast-epoch-boundary");
        episode.kind = MediaKind::PodcastEpisode;
        let progress_generation = Generation::new(7);
        let state = AppState {
            podcasts: crate::app::PodcastState {
                show: Some(crate::provider::Podcast {
                    id: "boundary-show".to_owned(),
                    title: "Boundary Show".to_owned(),
                    creators: vec!["Host".to_owned()],
                    description: None,
                    artwork_url: None,
                    episodes: vec![episode.clone()],
                }),
                pending_progress_generation: Some(progress_generation),
                pending_media: Some(episode.clone()),
                ..crate::app::PodcastState::default()
            },
            ..AppState::default()
        };

        let (state, effects) = reduce(
            state,
            Action::PodcastProgressLoaded {
                generation: progress_generation,
                progress: Some(crate::storage::PodcastProgress {
                    video_id: episode.id.video_id,
                    playback_epoch: i64::MAX as u64,
                    position_ms: 180_000,
                    duration_ms: episode.duration_ms,
                    played: true,
                    updated_at: 100,
                }),
            },
        );

        assert!(effects.is_empty());
        assert!(state.current_attempt_generation().is_none());
        assert!(state.podcasts.pending_progress_generation().is_none());
        assert!(state.podcasts.pending_media.is_none());
        let Some(diagnostic) = state.diagnostics().last() else {
            panic!("podcast epoch exhaustion must add a diagnostic");
        };
        assert_eq!(diagnostic.category(), DiagnosticCategory::State);
        assert!(
            diagnostic.message().contains("epoch"),
            "the diagnostic must explain the exhausted epoch"
        );
    }
}
