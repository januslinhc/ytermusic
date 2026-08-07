use std::fmt;

use ratatui::text::Line;

use crate::app::AppState;

use super::super::interaction::{ListSurface, RenderedRowTarget};
use super::super::render::{
    SelectionViewport, bounded_format_cells, dataset_key, format_duration, loading_label,
};

#[cfg(test)]
pub(crate) fn lines(
    state: &AppState,
    row_limit: usize,
    available_width: usize,
) -> Vec<Line<'static>> {
    lines_with_viewports(
        state,
        row_limit,
        available_width,
        &mut SelectionViewport::default(),
        &mut SelectionViewport::default(),
    )
}

#[cfg(test)]
pub(crate) fn lines_with_viewports(
    state: &AppState,
    row_limit: usize,
    available_width: usize,
    recommendation_viewport: &mut SelectionViewport,
    episode_viewport: &mut SelectionViewport,
) -> Vec<Line<'static>> {
    lines_with_viewports_and_targets(
        state,
        row_limit,
        available_width,
        recommendation_viewport,
        episode_viewport,
        None,
    )
}

#[cfg(test)]
pub(crate) fn lines_with_viewports_and_targets(
    state: &AppState,
    row_limit: usize,
    available_width: usize,
    recommendation_viewport: &mut SelectionViewport,
    episode_viewport: &mut SelectionViewport,
    targets: Option<&mut Vec<RenderedRowTarget>>,
) -> Vec<Line<'static>> {
    lines_with_viewports_and_targets_with_spinner(
        state,
        row_limit,
        available_width,
        recommendation_viewport,
        episode_viewport,
        targets,
        0,
    )
}

#[allow(
    clippy::too_many_arguments,
    reason = "podcast lists share two bounded viewports, targets, and immutable spinner state"
)]
pub(crate) fn lines_with_viewports_and_targets_with_spinner(
    state: &AppState,
    row_limit: usize,
    available_width: usize,
    recommendation_viewport: &mut SelectionViewport,
    episode_viewport: &mut SelectionViewport,
    targets: Option<&mut Vec<RenderedRowTarget>>,
    spinner_index: usize,
) -> Vec<Line<'static>> {
    if row_limit == 0 {
        return Vec::new();
    }

    let podcasts = state.podcasts();
    if let Some(show) = podcasts.show() {
        return show_lines(
            state,
            row_limit,
            available_width,
            show,
            episode_viewport,
            targets,
        );
    }
    if !podcasts.recommendations().is_empty() {
        return recommendation_lines(
            state,
            row_limit,
            available_width,
            recommendation_viewport,
            targets,
            spinner_index,
        );
    }

    let mut lines = Vec::with_capacity(row_limit.min(4));
    if podcasts.loading() {
        push_bounded(
            &mut lines,
            row_limit,
            available_width,
            format_args!("Podcasts & episodes"),
        );
        push_loading(
            &mut lines,
            row_limit,
            available_width,
            spinner_index,
            "Loading podcast",
        );
    } else if podcasts.error().is_some() {
        push_bounded(
            &mut lines,
            row_limit,
            available_width,
            format_args!("Podcasts & episodes"),
        );
        push_bounded(
            &mut lines,
            row_limit,
            available_width,
            format_args!("! Podcast unavailable"),
        );
        push_search_hint(&mut lines, row_limit, available_width);
    } else if podcasts.recommendations_loading() {
        push_bounded(
            &mut lines,
            row_limit,
            available_width,
            format_args!("Top podcasts in {}", podcasts.requested_region()),
        );
        push_loading(
            &mut lines,
            row_limit,
            available_width,
            spinner_index,
            "Loading recommendations",
        );
    } else if podcasts.recommendation_error().is_some() {
        push_bounded(
            &mut lines,
            row_limit,
            available_width,
            format_args!("Top podcasts in {}", podcasts.requested_region()),
        );
        push_bounded(
            &mut lines,
            row_limit,
            available_width,
            format_args!("! Podcast recommendations unavailable"),
        );
        push_search_hint(&mut lines, row_limit, available_width);
    } else {
        push_bounded(
            &mut lines,
            row_limit,
            available_width,
            format_args!("Podcasts & episodes"),
        );
        push_search_hint(&mut lines, row_limit, available_width);
    }
    lines
}

#[allow(
    clippy::too_many_lines,
    reason = "podcast status rows and selectable recommendation geometry share one bounded pass"
)]
fn recommendation_lines(
    state: &AppState,
    row_limit: usize,
    available_width: usize,
    viewport_memory: &mut SelectionViewport,
    mut targets: Option<&mut Vec<RenderedRowTarget>>,
    spinner_index: usize,
) -> Vec<Line<'static>> {
    let podcasts = state.podcasts();
    let mut lines = Vec::with_capacity(row_limit.min(16));
    let region = if podcasts.recommendations_loading() {
        podcasts.requested_region()
    } else {
        podcasts
            .effective_region()
            .unwrap_or_else(|| podcasts.requested_region())
    };
    push_bounded(
        &mut lines,
        row_limit,
        available_width,
        format_args!("Top podcasts in {region}"),
    );

    if podcasts.resolve_loading() {
        push_loading(
            &mut lines,
            row_limit,
            available_width,
            spinner_index,
            "Finding on YouTube Music",
        );
    } else if podcasts.resolve_error().is_some() {
        push_bounded(
            &mut lines,
            row_limit,
            available_width,
            format_args!("! Unavailable on YouTube Music"),
        );
        push_search_hint(&mut lines, row_limit, available_width);
    } else if podcasts.loading() {
        push_loading(
            &mut lines,
            row_limit,
            available_width,
            spinner_index,
            "Loading podcast",
        );
    } else if podcasts.error().is_some() {
        push_bounded(
            &mut lines,
            row_limit,
            available_width,
            format_args!("! Podcast unavailable"),
        );
        push_search_hint(&mut lines, row_limit, available_width);
    } else if podcasts.recommendations_loading() {
        push_loading(
            &mut lines,
            row_limit,
            available_width,
            spinner_index,
            "Loading recommendations",
        );
    } else if podcasts.recommendation_error().is_some() {
        push_bounded(
            &mut lines,
            row_limit,
            available_width,
            format_args!("! Recommendations could not be refreshed"),
        );
    }

    let available_rows = row_limit.saturating_sub(lines.len());
    let selected_index = podcasts.selected_recommendation().and_then(|selected| {
        podcasts
            .recommendations()
            .iter()
            .position(|recommendation| recommendation.source_id() == selected)
    });
    let key = recommendation_dataset_key(state);
    let viewport = viewport_memory.visible_range(
        podcasts.recommendations().len(),
        selected_index,
        available_rows,
        key,
    );
    for (offset, recommendation) in podcasts.recommendations()[viewport.clone()]
        .iter()
        .enumerate()
    {
        if let Some(targets) = targets.as_deref_mut() {
            targets.push(viewport_memory.row_target(
                lines.len(),
                ListSurface::PodcastRecommendations,
                viewport.start.saturating_add(offset),
            ));
        }
        let marker = if podcasts.selected_recommendation() == Some(recommendation.source_id()) {
            "▶"
        } else {
            " "
        };
        push_bounded(
            &mut lines,
            row_limit,
            available_width,
            format_args!(
                "{marker} {}. {}  ·  {}",
                recommendation.rank(),
                recommendation.title(),
                recommendation.publisher()
            ),
        );
    }
    lines
}

fn recommendation_dataset_key(state: &AppState) -> u64 {
    let podcasts = state.podcasts();
    dataset_key(&(
        podcasts.recommendation_generation(),
        podcasts.requested_region(),
        podcasts.effective_region(),
        podcasts
            .recommendations()
            .iter()
            .map(crate::podcast_rankings::PodcastRecommendation::source_id)
            .collect::<Vec<_>>(),
    ))
}

fn show_lines(
    state: &AppState,
    row_limit: usize,
    available_width: usize,
    show: &crate::provider::Podcast,
    viewport_memory: &mut SelectionViewport,
    mut targets: Option<&mut Vec<RenderedRowTarget>>,
) -> Vec<Line<'static>> {
    let mut lines = Vec::with_capacity(row_limit.min(16));
    push_bounded(
        &mut lines,
        row_limit,
        available_width,
        format_args!("Podcasts & episodes"),
    );
    let creators = if show.creators.is_empty() {
        CreatorList::Unknown
    } else {
        CreatorList::Known(&show.creators)
    };
    push_bounded(
        &mut lines,
        row_limit,
        available_width,
        format_args!("{} — {creators}", show.title),
    );

    let available_rows = row_limit.saturating_sub(lines.len());
    let selected_index = state.podcasts().selected_episode().and_then(|selected| {
        show.episodes
            .iter()
            .position(|episode| &episode.id == selected)
    });
    let key = dataset_key(&(
        state.podcasts().generation(),
        show.id.as_str(),
        show.episodes
            .iter()
            .map(|episode| &episode.id)
            .collect::<Vec<_>>(),
    ));
    let viewport =
        viewport_memory.visible_range(show.episodes.len(), selected_index, available_rows, key);
    for (offset, episode) in show.episodes[viewport.clone()].iter().enumerate() {
        if let Some(targets) = targets.as_deref_mut() {
            targets.push(viewport_memory.row_target(
                lines.len(),
                ListSurface::PodcastEpisodes,
                viewport.start.saturating_add(offset),
            ));
        }
        let selected = state.podcasts().selected_episode() == Some(&episode.id);
        let marker = if selected { "▶" } else { " " };
        let resume = if state.playback().current.as_ref() == Some(&episode.id)
            && state.playback().position_ms > 0
        {
            format!("Resume {}", format_duration(state.playback().position_ms))
        } else {
            episode
                .duration_ms
                .map_or_else(|| "--:--".to_owned(), format_duration)
        };
        push_bounded(
            &mut lines,
            row_limit,
            available_width,
            format_args!("{marker} {}  ·  {resume}", episode.title),
        );
    }
    if show.episodes.is_empty() {
        push_bounded(
            &mut lines,
            row_limit,
            available_width,
            format_args!("No episodes"),
        );
    }
    lines
}

fn push_search_hint(lines: &mut Vec<Line<'static>>, row_limit: usize, available_width: usize) {
    push_bounded(
        lines,
        row_limit,
        available_width,
        format_args!("Press / to search podcasts"),
    );
}

fn push_loading(
    lines: &mut Vec<Line<'static>>,
    row_limit: usize,
    available_width: usize,
    spinner_index: usize,
    text: &str,
) {
    if lines.len() < row_limit {
        lines.push(loading_label(spinner_index, text, available_width));
    }
}

fn push_bounded(
    lines: &mut Vec<Line<'static>>,
    row_limit: usize,
    available_width: usize,
    arguments: fmt::Arguments<'_>,
) {
    if lines.len() < row_limit {
        lines.push(Line::from(bounded_format_cells(available_width, arguments)));
    }
}

enum CreatorList<'a> {
    Unknown,
    Known(&'a [String]),
}

impl fmt::Display for CreatorList<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unknown => formatter.write_str("Unknown creator"),
            Self::Known(creators) => {
                for (index, creator) in creators.iter().enumerate() {
                    if index > 0 {
                        formatter.write_str(", ")?;
                    }
                    formatter.write_str(creator)?;
                }
                Ok(())
            }
        }
    }
}
