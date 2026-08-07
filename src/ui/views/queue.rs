use ratatui::text::Line;

use crate::queue::{Queue, QueueItemId};

use super::super::interaction::{ListSurface, RenderedRowTarget};
use super::super::render::{SelectionViewport, bounded_format_cells, dataset_key};

#[cfg(test)]
pub(crate) fn lines(
    queue: &Queue,
    selected_id: Option<&QueueItemId>,
    available_rows: usize,
    available_width: usize,
) -> Vec<Line<'static>> {
    lines_with_viewport(
        queue,
        selected_id,
        available_rows,
        available_width,
        &mut SelectionViewport::default(),
    )
}

#[cfg(test)]
pub(crate) fn lines_with_viewport(
    queue: &Queue,
    selected_id: Option<&QueueItemId>,
    available_rows: usize,
    available_width: usize,
    viewport_memory: &mut SelectionViewport,
) -> Vec<Line<'static>> {
    lines_with_viewport_and_targets(
        queue,
        selected_id,
        available_rows,
        available_width,
        viewport_memory,
        None,
    )
}

pub(crate) fn lines_with_viewport_and_targets(
    queue: &Queue,
    selected_id: Option<&QueueItemId>,
    available_rows: usize,
    available_width: usize,
    viewport_memory: &mut SelectionViewport,
    mut targets: Option<&mut Vec<RenderedRowTarget>>,
) -> Vec<Line<'static>> {
    if available_rows == 0 {
        return Vec::new();
    }
    let current_id = queue.current().map(crate::queue::QueueItem::id);
    let marked_id = selected_id.or(current_id);
    let mut lines = Vec::with_capacity(available_rows.min(16));
    if queue.items().is_empty() {
        lines.push(Line::from("Queue is empty"));
    } else {
        let selected_index = marked_id.and_then(|marked_id| {
            queue
                .active_ids()
                .iter()
                .position(|item_id| item_id == marked_id)
        });
        let key = dataset_key(queue.active_ids());
        let viewport = viewport_memory.visible_range(
            queue.active_ids().len(),
            selected_index,
            available_rows,
            key,
        );
        for (offset, item) in queue
            .active_items()
            .skip(viewport.start)
            .take(viewport.len())
            .enumerate()
        {
            if let Some(targets) = targets.as_deref_mut() {
                targets.push(viewport_memory.row_target(
                    lines.len(),
                    ListSurface::Queue,
                    viewport.start.saturating_add(offset),
                ));
            }
            let marker = if marked_id == Some(item.id()) {
                "▶"
            } else {
                " "
            };
            lines.push(Line::from(bounded_format_cells(
                available_width,
                format_args!("{marker} {}", item.media().title),
            )));
        }
    }
    lines
}
