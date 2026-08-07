use ratatui::text::Line;

use crate::{
    app::{AppState, stable_library_item_id},
    provider::{AuthenticationState, LibraryItem},
};

use super::super::interaction::{ListSurface, RenderedRowTarget};
use super::super::render::{
    DatasetUpdate, SelectionViewport, bounded_format_cells, loading_label, ordered_dataset_key,
};

#[cfg(test)]
pub(crate) fn lines(
    state: &AppState,
    row_limit: usize,
    available_width: usize,
) -> Vec<Line<'static>> {
    lines_with_viewport(
        state,
        row_limit,
        available_width,
        &mut SelectionViewport::default(),
    )
}

#[cfg(test)]
pub(crate) fn lines_with_viewport(
    state: &AppState,
    row_limit: usize,
    available_width: usize,
    viewport_memory: &mut SelectionViewport,
) -> Vec<Line<'static>> {
    lines_with_viewport_and_targets(state, row_limit, available_width, viewport_memory, None)
}

#[cfg(test)]
pub(crate) fn lines_with_viewport_and_targets(
    state: &AppState,
    row_limit: usize,
    available_width: usize,
    viewport_memory: &mut SelectionViewport,
    targets: Option<&mut Vec<RenderedRowTarget>>,
) -> Vec<Line<'static>> {
    lines_with_viewport_and_targets_with_spinner(
        state,
        row_limit,
        available_width,
        viewport_memory,
        targets,
        0,
    )
}

pub(crate) fn lines_with_viewport_and_targets_with_spinner(
    state: &AppState,
    row_limit: usize,
    available_width: usize,
    viewport_memory: &mut SelectionViewport,
    mut targets: Option<&mut Vec<RenderedRowTarget>>,
    spinner_index: usize,
) -> Vec<Line<'static>> {
    if row_limit == 0 {
        return Vec::new();
    }
    let library = state.library();
    let mut lines = Vec::with_capacity(row_limit.min(16));
    lines.push(Line::from(bounded_format_cells(
        available_width,
        format_args!("Library · {:?}", library.section()),
    )));
    if lines.len() >= row_limit {
        return lines;
    }
    if library.authentication() == AuthenticationState::Unauthenticated {
        lines.push(Line::from("Connect account to browse your saved music."));
        lines.push(Line::from("[a] Connect account"));
        lines.truncate(row_limit);
        return lines;
    }

    if library.stale() {
        lines.push(Line::from("STALE · cached library content"));
        if lines.len() >= row_limit {
            return lines;
        }
    }
    if library.loading() {
        lines.push(loading_label(
            spinner_index,
            "Loading library",
            available_width,
        ));
        if lines.len() >= row_limit {
            return lines;
        }
    }
    if let Some(error) = library.error() {
        lines.push(Line::from(bounded_format_cells(
            available_width,
            format_args!("! {}", error.message()),
        )));
        if lines.len() >= row_limit {
            return lines;
        }
    }
    if library.items().is_empty() && !library.loading() && library.error().is_none() {
        lines.push(Line::from("No saved items"));
    }

    let selected_index = library.selected_id().and_then(|selected| {
        library
            .items()
            .iter()
            .position(|item| stable_library_item_id(item) == *selected)
    });
    let available_rows = row_limit.saturating_sub(lines.len());
    let key = ordered_dataset_key(
        &library.section(),
        &library.generation(),
        library.items().iter().map(stable_library_item_id),
        if library.loading_more() {
            DatasetUpdate::AppendInProgress
        } else {
            DatasetUpdate::Replace
        },
    );
    let mut probe = viewport_memory.clone();
    let probe_range = probe.visible_range(
        library.items().len(),
        selected_index,
        available_rows,
        key.clone(),
    );
    let loading_more = library.loading_more();
    let has_footer = loading_more || library.continuation().is_some();
    let show_footer = has_footer && available_rows > 1 && probe_range.end == library.items().len();
    let list_rows = available_rows.saturating_sub(usize::from(show_footer));
    let viewport =
        viewport_memory.visible_range(library.items().len(), selected_index, list_rows, key);
    for (offset, item) in library.items()[viewport.clone()].iter().enumerate() {
        if let Some(targets) = targets.as_deref_mut() {
            targets.push(viewport_memory.row_target(
                lines.len(),
                ListSurface::Library,
                viewport.start.saturating_add(offset),
            ));
        }
        let selected = library.selected_id() == Some(&stable_library_item_id(item));
        let marker = if selected { "▶" } else { " " };
        let (title, subtitle) = item_text(item);
        lines.push(Line::from(bounded_format_cells(
            available_width,
            format_args!("{marker} {title}{subtitle}"),
        )));
    }
    if show_footer {
        lines.push(if loading_more {
            loading_label(spinner_index, "Loading more", available_width)
        } else {
            Line::from("[m] Load more")
        });
    }
    lines
}

fn item_text(item: &LibraryItem) -> (&str, String) {
    match item {
        LibraryItem::Playable(media) => {
            let creator = media
                .creators
                .first()
                .map_or("Unknown artist", String::as_str);
            (&media.title, format!(" — {creator}"))
        }
        LibraryItem::Album(item)
        | LibraryItem::Artist(item)
        | LibraryItem::Playlist(item)
        | LibraryItem::Podcast(item) => (
            &item.title,
            item.subtitle
                .as_deref()
                .map_or_else(String::new, |subtitle| format!(" — {subtitle}")),
        ),
    }
}
