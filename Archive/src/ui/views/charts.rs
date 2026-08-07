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

#[allow(
    clippy::too_many_lines,
    reason = "chart status, pinned section, rows, targets, and spinner share one bounded pass"
)]
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
    let mut lines = Vec::with_capacity(row_limit.min(16));
    let region = state
        .charts()
        .region()
        .map_or("--", |region| region.as_str());
    lines.push(Line::from(bounded_format_cells(
        available_width,
        format_args!("Trending in {region}"),
    )));
    if lines.len() >= row_limit {
        return lines;
    }

    if state.charts().stale() {
        lines.push(Line::from("STALE · cached chart content"));
        if lines.len() >= row_limit {
            return lines;
        }
    }
    if state.charts().loading() {
        lines.push(loading_label(
            spinner_index,
            "Loading regional charts",
            available_width,
        ));
    } else if let Some(error) = state.charts().error() {
        lines.push(Line::from(bounded_format_cells(
            available_width,
            format_args!("! {}", error.message()),
        )));
    } else if state.charts().sections().is_empty() {
        lines.push(Line::from(
            "Choose a country with c to load regional charts.",
        ));
    } else {
        let total_items = state
            .charts()
            .sections()
            .iter()
            .map(|section| section.items().len())
            .fold(0usize, usize::saturating_add);
        let selected_index = state
            .charts()
            .selected_index()
            .filter(|index| *index < total_items);
        let available_rows = row_limit.saturating_sub(lines.len());
        let item_rows = available_rows.saturating_sub(1);
        let key = charts_dataset_key(state);
        let viewport = viewport_memory.visible_range(total_items, selected_index, item_rows, key);
        let first_visible_item = (!viewport.is_empty()).then_some(viewport.start);
        let pinned_section = pinned_section_index(
            state.charts().sections(),
            selected_index,
            first_visible_item,
        );
        if available_rows > 0
            && let Some(section) =
                pinned_section.and_then(|index| state.charts().sections().get(index))
        {
            lines.push(Line::from(bounded_format_cells(
                available_width,
                format_args!("• {}", section.title()),
            )));
        }

        let mut item_index = 0usize;
        for section in state.charts().sections() {
            for item in section.items() {
                if viewport.contains(&item_index) {
                    if let Some(targets) = targets.as_deref_mut() {
                        targets.push(viewport_memory.row_target(
                            lines.len(),
                            ListSurface::Charts,
                            item_index,
                        ));
                    }
                    let marker = if selected_index == Some(item_index) {
                        "▶"
                    } else {
                        " "
                    };
                    let creator = item
                        .creators
                        .first()
                        .map_or("Unknown artist", String::as_str);
                    lines.push(Line::from(bounded_format_cells(
                        available_width,
                        format_args!("{marker} {} — {creator}", item.title),
                    )));
                }
                item_index = item_index.saturating_add(1);
                if item_index >= viewport.end {
                    break;
                }
            }
            if item_index >= viewport.end {
                break;
            }
        }
    }
    lines
}

fn pinned_section_index(
    sections: &[crate::domain::ChartSection],
    selected_item_index: Option<usize>,
    first_visible_item_index: Option<usize>,
) -> Option<usize> {
    selected_item_index
        .and_then(|index| section_index_for_item(sections, index))
        .or_else(|| {
            first_visible_item_index.and_then(|index| section_index_for_item(sections, index))
        })
        .or_else(|| (!sections.is_empty()).then_some(0))
}

fn section_index_for_item(
    sections: &[crate::domain::ChartSection],
    target_index: usize,
) -> Option<usize> {
    let mut item_index = 0usize;
    for (section_index, section) in sections.iter().enumerate() {
        let section_end = item_index.saturating_add(section.items().len());
        if target_index < section_end {
            return Some(section_index);
        }
        item_index = section_end;
    }
    None
}

fn charts_dataset_key(state: &AppState) -> u64 {
    dataset_key(&(
        state.charts().generation(),
        state.charts().region(),
        state
            .charts()
            .sections()
            .iter()
            .map(|section| {
                section
                    .items()
                    .iter()
                    .map(|item| &item.id)
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>(),
    ))
}

#[cfg(test)]
mod tests {
    use crate::{
        app::{Action, reduce},
        domain::{ChartSection, MediaId, MediaItem, MediaKind, RegionCode},
    };

    use super::*;

    fn chart_item(index: usize) -> MediaItem {
        MediaItem {
            id: MediaId {
                provider: "youtube-music".to_owned(),
                video_id: format!("dataset-{index}"),
            },
            kind: MediaKind::Song,
            title: format!("Chart item {index}"),
            creators: vec!["Chart artist".to_owned()],
            collection: None,
            duration_ms: Some(180_000),
            artwork_url: None,
            explicit: false,
        }
    }

    fn chart_sections(order: impl IntoIterator<Item = usize>) -> Vec<ChartSection> {
        let items = order.into_iter().map(chart_item).collect::<Vec<_>>();
        items
            .chunks(5)
            .enumerate()
            .map(|(index, items)| {
                ChartSection::new(format!("Section {}", index + 1), items.to_vec())
            })
            .collect()
    }

    fn selected_item_index(state: &AppState) -> Option<usize> {
        let selected = state.charts().selected_id()?;
        state
            .charts()
            .sections()
            .iter()
            .flat_map(ChartSection::items)
            .position(|item| &item.id == selected)
    }

    #[test]
    fn pinned_section_prefers_selection_and_falls_back_to_first_visible_item() {
        let sections = chart_sections(0..15);

        let section_title = |index: usize| -> &str {
            sections
                .get(index)
                .unwrap_or_else(|| panic!("pinned section {index}"))
                .title()
        };
        assert_eq!(
            pinned_section_index(&sections, None, Some(6)).map(section_title),
            Some("Section 2")
        );
        assert_eq!(
            pinned_section_index(&sections, Some(usize::MAX), Some(11)).map(section_title),
            Some("Section 3")
        );
        assert_eq!(
            pinned_section_index(&sections, Some(2), Some(11)).map(section_title),
            Some("Section 1")
        );
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "one multiframe trace keeps reducer transitions and shared viewport memory explicit"
    )]
    fn chart_dataset_key_obeys_multiframe_reset_and_preservation_contract() {
        let region = RegionCode::parse("HK").unwrap_or_else(|error| panic!("region: {error}"));
        let original_sections = chart_sections(0..15);
        let (state, _) = reduce(
            AppState::default(),
            Action::ChartsRequested {
                region: region.clone(),
            },
        );
        let generation = state.charts().generation();
        let state = reduce(
            state,
            Action::ChartsCompleted {
                generation,
                region: region.clone(),
                received_at: 1_000,
                result: Ok(original_sections),
            },
        )
        .0;
        let selected = chart_item(10).id;
        let state = reduce(
            state,
            Action::ChartSelectionChanged {
                media_id: selected.clone(),
            },
        )
        .0;
        let key = charts_dataset_key(&state);
        let mut viewport = SelectionViewport::default();
        assert_eq!(viewport.visible_range(15, Some(10), 4, key), 7..11);

        let state = reduce(
            state,
            Action::ChartSelectionChanged {
                media_id: chart_item(9).id,
            },
        )
        .0;
        assert_eq!(charts_dataset_key(&state), key);
        assert_eq!(viewport.visible_range(15, Some(9), 4, key), 7..11);
        assert_eq!(viewport.visible_range(15, Some(9), 6, key), 7..13);

        let state = reduce(
            state,
            Action::ChartSelectionChanged {
                media_id: selected.clone(),
            },
        )
        .0;
        let reordered_sections = chart_sections((5..15).chain(0..5));
        let (state, _) = reduce(
            state,
            Action::ChartsRequested {
                region: region.clone(),
            },
        );
        let refresh_generation = state.charts().generation();
        assert_ne!(refresh_generation, generation);
        let state = reduce(
            state,
            Action::ChartsCompleted {
                generation: refresh_generation,
                region: region.clone(),
                received_at: 1_600,
                result: Ok(reordered_sections),
            },
        )
        .0;
        assert_eq!(state.charts().selected_id(), Some(&selected));
        let reordered_key = charts_dataset_key(&state);
        assert_ne!(reordered_key, key);
        let reordered_selected = selected_item_index(&state)
            .unwrap_or_else(|| panic!("selected chart item after reorder"));
        assert_eq!(reordered_selected, 5);
        assert_eq!(
            viewport.visible_range(15, Some(reordered_selected), 4, reordered_key),
            2..6
        );

        let new_region = RegionCode::parse("US").unwrap_or_else(|error| panic!("region: {error}"));
        let (state, _) = reduce(
            state,
            Action::ChartsRequested {
                region: new_region.clone(),
            },
        );
        let new_generation = state.charts().generation();
        assert_ne!(new_generation, generation);
        let state = reduce(
            state,
            Action::ChartsCompleted {
                generation: new_generation,
                region: new_region,
                received_at: 2_000,
                result: Ok(chart_sections(100..115)),
            },
        )
        .0;
        let state = reduce(
            state,
            Action::ChartSelectionChanged {
                media_id: chart_item(102).id,
            },
        )
        .0;
        let replacement_key = charts_dataset_key(&state);
        assert_ne!(replacement_key, reordered_key);
        assert_eq!(
            viewport.visible_range(15, selected_item_index(&state), 4, replacement_key),
            0..4
        );
    }
}
