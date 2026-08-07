use ratatui::text::Line;

use crate::app::{AppState, SearchItem};

use super::super::interaction::{ListSurface, RenderedRowTarget};
use super::super::render::{
    DatasetKey, DatasetUpdate, SelectionViewport, bounded_format_cells, loading_label,
    ordered_dataset_key,
};

#[cfg(test)]
pub(crate) fn lines(
    state: &AppState,
    row_limit: usize,
    available_width: usize,
    draft: Option<&str>,
) -> Vec<Line<'static>> {
    lines_with_viewport(
        state,
        row_limit,
        available_width,
        draft,
        &mut SelectionViewport::default(),
    )
}

#[cfg(test)]
pub(crate) fn lines_with_viewport(
    state: &AppState,
    row_limit: usize,
    available_width: usize,
    draft: Option<&str>,
    viewport_memory: &mut SelectionViewport,
) -> Vec<Line<'static>> {
    lines_with_viewport_and_targets(
        state,
        row_limit,
        available_width,
        draft,
        viewport_memory,
        None,
    )
}

#[allow(
    clippy::too_many_lines,
    reason = "search status, viewport, footer, and optional target collection share one bounded pass"
)]
#[cfg(test)]
pub(crate) fn lines_with_viewport_and_targets(
    state: &AppState,
    row_limit: usize,
    available_width: usize,
    draft: Option<&str>,
    viewport_memory: &mut SelectionViewport,
    targets: Option<&mut Vec<RenderedRowTarget>>,
) -> Vec<Line<'static>> {
    lines_with_viewport_and_targets_with_spinner(
        state,
        row_limit,
        available_width,
        draft,
        viewport_memory,
        targets,
        0,
    )
}

#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "search status, viewport, footer, targets, and immutable spinner share one bounded pass"
)]
pub(crate) fn lines_with_viewport_and_targets_with_spinner(
    state: &AppState,
    row_limit: usize,
    available_width: usize,
    draft: Option<&str>,
    viewport_memory: &mut SelectionViewport,
    mut targets: Option<&mut Vec<RenderedRowTarget>>,
    spinner_index: usize,
) -> Vec<Line<'static>> {
    if row_limit == 0 {
        return Vec::new();
    }
    let mut lines = Vec::with_capacity(row_limit.min(16));
    lines.push(Line::from(bounded_format_cells(
        available_width,
        format_args!(
            "Query: {}  ·  Filter: {:?}",
            draft.unwrap_or_else(|| state.search().query()),
            state.search().filter()
        ),
    )));
    if lines.len() >= row_limit {
        return lines;
    }
    lines.push(Line::from(""));
    if lines.len() >= row_limit {
        return lines;
    }
    if state.search().stale() {
        lines.push(Line::from("STALE · cached search results"));
        if lines.len() >= row_limit {
            return lines;
        }
    }
    if state.search().loading() {
        lines.push(loading_label(spinner_index, "Searching", available_width));
    } else if let Some(error) = state.search().error() {
        lines.push(Line::from(bounded_format_cells(
            available_width,
            format_args!("! {}", error.message()),
        )));
    } else if state.search().items().is_empty() {
        lines.push(Line::from("No results"));
    } else {
        let items = state.search().items();
        let selected_index = state
            .search()
            .selected_id()
            .and_then(|selected| items.iter().position(|item| item.stable_id() == *selected));
        let available_rows = row_limit.saturating_sub(lines.len());
        let key = search_dataset_key(
            state.search().filter(),
            state.search().generation(),
            state.search().loading_more(),
            items,
        );
        let mut probe = viewport_memory.clone();
        let probe_range =
            probe.visible_range(items.len(), selected_index, available_rows, key.clone());
        let loading_more = state.search().loading_more();
        let has_footer = loading_more || state.search().continuation().is_some();
        let show_footer = has_footer && available_rows > 1 && probe_range.end == items.len();
        let list_rows = available_rows.saturating_sub(usize::from(show_footer));
        let viewport = viewport_memory.visible_range(items.len(), selected_index, list_rows, key);
        for (offset, item) in items[viewport.clone()].iter().enumerate() {
            if let Some(targets) = targets.as_deref_mut() {
                targets.push(viewport_memory.row_target(
                    lines.len(),
                    ListSurface::Search,
                    viewport.start.saturating_add(offset),
                ));
            }
            match item {
                SearchItem::Playable(media) => {
                    let creator = media
                        .creators
                        .first()
                        .map_or("Unknown artist", String::as_str);
                    let marker = if state.search().selected_id() == Some(&item.stable_id()) {
                        "▶"
                    } else {
                        " "
                    };
                    lines.push(Line::from(bounded_format_cells(
                        available_width,
                        format_args!("{marker} {} — {creator}", media.title),
                    )));
                }
                SearchItem::Metadata(metadata) => {
                    let marker = if state.search().selected_id() == Some(&item.stable_id()) {
                        "▶"
                    } else {
                        " "
                    };
                    lines.push(Line::from(bounded_format_cells(
                        available_width,
                        format_args!("{marker} ◇ {}", metadata.title()),
                    )));
                }
            }
        }
        if show_footer {
            lines.push(if loading_more {
                loading_label(spinner_index, "Loading more", available_width)
            } else {
                Line::from("[m] Load more")
            });
        }
    }
    lines
}

#[derive(Hash)]
enum SearchDatasetIdentity<'a> {
    Playable(&'a crate::domain::MediaId),
    MetadataProvider(crate::app::SearchMetadataKind, &'a str),
    MetadataStructural(crate::app::SearchMetadataKind, usize),
}

fn search_dataset_key(
    filter: crate::domain::SearchFilter,
    generation: crate::app::Generation,
    loading_more: bool,
    items: &[SearchItem],
) -> DatasetKey {
    let identities = items.iter().enumerate().map(|(index, item)| match item {
        SearchItem::Playable(media) => SearchDatasetIdentity::Playable(&media.id),
        SearchItem::Metadata(metadata) => metadata.provider_id().map_or_else(
            || SearchDatasetIdentity::MetadataStructural(metadata.kind(), index),
            |provider_id| SearchDatasetIdentity::MetadataProvider(metadata.kind(), provider_id),
        ),
    });
    ordered_dataset_key(
        &filter,
        &generation,
        identities,
        if loading_more {
            DatasetUpdate::AppendInProgress
        } else {
            DatasetUpdate::Replace
        },
    )
}

#[cfg(test)]
mod tests {
    use crate::app::{Generation, SearchItem, SearchMetadata, SearchMetadataKind};
    use crate::domain::SearchFilter;

    use super::search_dataset_key;

    #[test]
    fn viewport_dataset_key_never_depends_on_metadata_titles() {
        let first = vec![SearchItem::Metadata(SearchMetadata::new(
            SearchMetadataKind::Podcast,
            "first external title",
        ))];
        let renamed = vec![SearchItem::Metadata(SearchMetadata::new(
            SearchMetadataKind::Podcast,
            "renamed external title",
        ))];

        assert_eq!(
            search_dataset_key(SearchFilter::Podcasts, Generation::default(), false, &first),
            search_dataset_key(
                SearchFilter::Podcasts,
                Generation::default(),
                false,
                &renamed,
            ),
        );
    }
}
