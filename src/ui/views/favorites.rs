use ratatui::text::Line;

use crate::app::AppState;

use super::super::interaction::{ListSurface, RenderedRowTarget};
use super::super::render::{
    DatasetUpdate, SelectionViewport, bounded_format_cells, loading_label, ordered_dataset_key,
};

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
    let favorites = state.favorites();
    let mut lines = Vec::with_capacity(row_limit.min(16));
    lines.push(Line::from("Favorites"));
    if lines.len() >= row_limit {
        return lines;
    }
    if favorites.loading() || (!favorites.loaded() && favorites.error().is_none()) {
        lines.push(loading_label(
            spinner_index,
            "Loading favorites",
            available_width,
        ));
        if lines.len() >= row_limit {
            return lines;
        }
    }
    if let Some(error) = favorites.error() {
        lines.push(Line::from(bounded_format_cells(
            available_width,
            format_args!("! {}", error.message()),
        )));
        if lines.len() >= row_limit {
            return lines;
        }
    }
    if favorites.entries().is_empty() {
        if favorites.loaded() && !favorites.loading() && favorites.error().is_none() {
            lines.push(Line::from("No favorites yet · press f on any song"));
        }
        lines.truncate(row_limit);
        return lines;
    }

    let selected_index = favorites.selected_id().and_then(|selected| {
        favorites
            .entries()
            .iter()
            .position(|entry| &entry.item.id == selected)
    });
    let available_rows = row_limit.saturating_sub(lines.len());
    let key = ordered_dataset_key(
        &"favorites",
        &favorites.generation(),
        favorites.entries().iter().map(|entry| &entry.item.id),
        DatasetUpdate::Reconcile,
    );
    let viewport = viewport_memory.visible_range(
        favorites.entries().len(),
        selected_index,
        available_rows,
        key,
    );
    for (offset, entry) in favorites.entries()[viewport.clone()].iter().enumerate() {
        if let Some(targets) = targets.as_deref_mut() {
            targets.push(viewport_memory.row_target(
                lines.len(),
                ListSurface::Favorites,
                viewport.start.saturating_add(offset),
            ));
        }
        let marker = if favorites.selected_id() == Some(&entry.item.id) {
            "▶"
        } else {
            " "
        };
        let creator = entry
            .item
            .creators
            .first()
            .map_or("Unknown artist", String::as_str);
        lines.push(Line::from(bounded_format_cells(
            available_width,
            format_args!("{marker} {} — {creator}", entry.item.title),
        )));
    }
    lines.truncate(row_limit);
    lines
}
