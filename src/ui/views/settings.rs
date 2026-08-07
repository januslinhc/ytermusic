use ratatui::text::Line;

use crate::app::AppState;

use super::super::render::bounded_format_cells;

pub(crate) fn lines(
    state: &AppState,
    row_limit: usize,
    available_width: usize,
    visualizer_max_fps: u8,
) -> Vec<Line<'static>> {
    let mut lines = Vec::with_capacity(row_limit.min(16));
    lines.push(Line::from("Playback settings"));
    lines.push(Line::from(bounded_format_cells(
        available_width,
        format_args!("Target volume: {}%", state.playback().target_volume),
    )));
    lines.push(Line::from(format!(
        "Lyrics: {}",
        if state.lyrics_enabled() { "on" } else { "off" }
    )));
    lines.push(Line::from(format!(
        "External sync: {}",
        if state.lyrics_external_sync_enabled() {
            "on"
        } else {
            "off"
        }
    )));
    lines.push(Line::from(format!(
        "Animated artwork: {}",
        if state.animated_artwork_enabled() {
            "on"
        } else {
            "off"
        }
    )));
    lines.push(Line::from(format!(
        "Spectrum visualizer: {}",
        if state.visualizer_enabled() {
            "on"
        } else {
            "off"
        }
    )));
    lines.push(Line::from(bounded_format_cells(
        available_width,
        format_args!("Spectrum frame-rate cap: {visualizer_max_fps} FPS"),
    )));
    lines.push(Line::from(bounded_format_cells(
        available_width,
        format_args!("Playback speed: {:.2}×", state.playback().playback_speed),
    )));
    lines.push(Line::from(bounded_format_cells(
        available_width,
        format_args!(
            "Fade in/out: {} ms / {} ms",
            state.player_presentation().fade_in_ms(),
            state.player_presentation().fade_out_ms()
        ),
    )));
    lines.push(Line::from(if state.dependencies().browsing_available() {
        "Browsing available"
    } else {
        "Browsing unavailable"
    }));
    lines.push(Line::from(if state.dependencies().playback_available() {
        "Playback available"
    } else {
        "Playback unavailable"
    }));

    for row in state.dependencies().rows() {
        if lines.len() >= row_limit {
            break;
        }
        lines.push(Line::from(bounded_format_cells(
            available_width,
            format_args!(
                "{} · {:?} · {}",
                row.component(),
                row.status(),
                row.detail()
            ),
        )));
    }
    if lines.len() < row_limit {
        lines.push(Line::from(if state.dependencies().checking() {
            "… Rechecking dependencies"
        } else {
            "[d] Recheck dependencies"
        }));
    }
    lines.truncate(row_limit);
    lines
}
