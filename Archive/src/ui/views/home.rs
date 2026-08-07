use ratatui::text::Line;

use crate::{app::AppState, provider::AuthenticationState};

use super::super::render::bounded_format_cells;

pub(crate) fn lines(
    state: &AppState,
    row_limit: usize,
    available_width: usize,
) -> Vec<Line<'static>> {
    let mut lines = Vec::with_capacity(row_limit.min(16));
    lines.push(Line::from("For you"));
    if let Some(current) = state.queue().current() {
        lines.push(Line::from("Continue listening"));
        let creator = current
            .media()
            .creators
            .first()
            .map_or("Unknown artist", String::as_str);
        lines.push(Line::from(bounded_format_cells(
            available_width,
            format_args!("▶ {} — {creator}", current.media().title),
        )));
    } else {
        lines.push(Line::from("Start with Search, Charts, or Podcasts."));
    }
    if lines.len() < row_limit {
        let region = state
            .charts()
            .region()
            .map_or("--", |region| region.as_str());
        lines.push(Line::from(bounded_format_cells(
            available_width,
            format_args!("Trending country · {region}"),
        )));
    }
    if lines.len() < row_limit {
        lines.push(Line::from(
            if state.library().authentication() == AuthenticationState::Authenticated {
                "Library connected"
            } else {
                "Library · Connect account for saved music"
            },
        ));
    }
    if !state.history().entries().is_empty() && lines.len() < row_limit {
        lines.push(Line::from("Recently played"));
        for entry in state.history().entries() {
            if lines.len() >= row_limit {
                break;
            }
            lines.push(Line::from(bounded_format_cells(
                available_width,
                format_args!("• {}", entry.item.title),
            )));
        }
    }
    lines.truncate(row_limit);
    lines
}
