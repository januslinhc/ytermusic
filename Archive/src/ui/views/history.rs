use ratatui::text::Line;

use crate::app::AppState;

use super::super::interaction::{ListSurface, RenderedRowTarget};
use super::super::render::{SelectionViewport, bounded_format_cells, dataset_key, loading_label};

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
    let history = state.history();
    let mut lines = Vec::with_capacity(row_limit.min(16));
    lines.push(Line::from("Listening history"));
    if lines.len() >= row_limit {
        return lines;
    }
    if history.loading() {
        lines.push(loading_label(
            spinner_index,
            "Loading history",
            available_width,
        ));
    } else if let Some(error) = history.error() {
        lines.push(Line::from(bounded_format_cells(
            available_width,
            format_args!("! {}", error.message()),
        )));
    } else if history.entries().is_empty() {
        lines.push(Line::from("No listening history yet"));
    } else {
        let selected_index = history.selected_id().and_then(|selected| {
            history
                .entries()
                .iter()
                .position(|entry| entry.id == selected)
        });
        let available_rows = row_limit.saturating_sub(lines.len());
        let key = dataset_key(&(
            history.generation(),
            history
                .entries()
                .iter()
                .map(|entry| entry.id)
                .collect::<Vec<_>>(),
        ));
        let viewport = viewport_memory.visible_range(
            history.entries().len(),
            selected_index,
            available_rows,
            key,
        );
        for (offset, entry) in history.entries()[viewport.clone()].iter().enumerate() {
            if let Some(targets) = targets.as_deref_mut() {
                targets.push(viewport_memory.row_target(
                    lines.len(),
                    ListSurface::History,
                    viewport.start.saturating_add(offset),
                ));
            }
            let marker = if history.selected_id() == Some(entry.id) {
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
                format_args!(
                    "{marker} {} — {creator} · played {}",
                    entry.item.title, entry.played_at
                ),
            )));
        }
    }
    lines.truncate(row_limit);
    lines
}
