pub const MAX_UI_MOTION_FPS: u8 = 30;
pub const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

const SPINNER_FRAME_MS: u64 = 80;
const ORDINARY_PROGRESS_MS: u64 = 180;
const SEEK_PROGRESS_MS: u64 = 90;
const SELECTION_MOTION_MS: u64 = 150;
const MAX_SELECTION_GLIDE_ROWS: i64 = 6;
const FRACTION_SCALE: u32 = 1_000_000;
const ROW_SCALE: i64 = 1_000_000;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MotionFrame {
    pub elapsed_ms: u64,
    pub spinner_index: usize,
    pub progress: ProgressPresentation,
}

impl MotionFrame {
    #[must_use]
    pub fn new(elapsed_ms: u64, fraction: f64, shimmer_phase: f64) -> Self {
        Self {
            elapsed_ms,
            spinner_index: spinner_index(elapsed_ms),
            progress: ProgressPresentation::new(fraction, shimmer_phase),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ProgressPresentation {
    pub fraction: f64,
    pub shimmer_phase: f64,
}

impl ProgressPresentation {
    #[must_use]
    pub fn new(fraction: f64, shimmer_phase: f64) -> Self {
        Self {
            fraction: finite_unit(fraction),
            shimmer_phase: finite_unit(shimmer_phase),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ProgressChange {
    #[default]
    Continuous,
    Seek,
    Media,
}

#[derive(Clone, Debug, Default)]
pub struct ProgressMotion {
    generation: Option<u64>,
    duration_ms: Option<u64>,
    start_ppm: u32,
    target_ppm: u32,
    transition_started_ms: u64,
    transition_duration_ms: u64,
    playing: bool,
    shimmer_accumulated_ms: u64,
    shimmer_started_ms: u64,
}

impl ProgressMotion {
    pub fn reconcile(
        &mut self,
        now_ms: u64,
        generation: u64,
        position_ms: u64,
        duration_ms: Option<u64>,
        playing: bool,
        change: ProgressChange,
    ) {
        let current_ppm = self.presentation_ppm(now_ms);
        let media_changed = self.generation != Some(generation)
            || self.duration_ms != duration_ms
            || change == ProgressChange::Media;
        let target_ppm = duration_ms
            .filter(|duration| *duration > 0)
            .map_or(0, |duration| fraction_ppm(position_ms, duration));

        self.shimmer_accumulated_ms = shimmer_elapsed(
            self.shimmer_accumulated_ms,
            self.shimmer_started_ms,
            now_ms,
            self.playing,
        );
        self.shimmer_started_ms = now_ms;
        self.generation = Some(generation);
        self.duration_ms = duration_ms.filter(|duration| *duration > 0);
        let pausing = self.playing && !playing;
        self.playing = playing;
        self.transition_started_ms = now_ms;
        self.target_ppm = target_ppm;

        if target_ppm == FRACTION_SCALE {
            self.start_ppm = target_ppm;
            self.transition_duration_ms = 0;
        } else if pausing {
            self.start_ppm = current_ppm;
            self.target_ppm = current_ppm;
            self.transition_duration_ms = 0;
        } else if media_changed {
            self.start_ppm = target_ppm;
            self.transition_duration_ms = 0;
        } else {
            self.start_ppm = current_ppm;
            self.transition_duration_ms = match change {
                ProgressChange::Seek => SEEK_PROGRESS_MS,
                ProgressChange::Continuous => ORDINARY_PROGRESS_MS,
                ProgressChange::Media => 0,
            };
            if self.start_ppm == self.target_ppm {
                self.transition_duration_ms = 0;
            }
        }
    }

    #[must_use]
    pub fn presentation(&self, now_ms: u64) -> ProgressPresentation {
        if self.duration_ms.is_none() {
            return ProgressPresentation::default();
        }
        let shimmer_ms = shimmer_elapsed(
            self.shimmer_accumulated_ms,
            self.shimmer_started_ms,
            now_ms,
            self.playing,
        );
        let shimmer_remainder = u32::try_from(shimmer_ms % 1_000).unwrap_or(0);
        let shimmer_phase = f64::from(shimmer_remainder) / 1_000.0;
        ProgressPresentation::new(ppm_to_unit(self.presentation_ppm(now_ms)), shimmer_phase)
    }

    fn presentation_ppm(&self, now_ms: u64) -> u32 {
        let Some(duration_ms) = self.duration_ms else {
            return 0;
        };
        let elapsed_ms = now_ms.saturating_sub(self.transition_started_ms);
        let playing_elapsed_ms = if self.playing { elapsed_ms } else { 0 };
        let progress_delta = playing_elapsed_ms
            .saturating_mul(u64::from(FRACTION_SCALE))
            .checked_div(duration_ms)
            .unwrap_or(0);
        let moving_target = u64::from(self.target_ppm)
            .saturating_add(progress_delta)
            .min(u64::from(FRACTION_SCALE));
        let moving_target = u32::try_from(moving_target).unwrap_or(FRACTION_SCALE);
        if self.transition_duration_ms == 0 {
            moving_target
        } else {
            interpolate_u32(
                self.start_ppm,
                moving_target,
                eased_phase_ppm(elapsed_ms, self.transition_duration_ms),
            )
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct SelectionMotion {
    start_microrows: i64,
    target_index: Option<usize>,
    started_ms: u64,
    duration_ms: u64,
}

impl SelectionMotion {
    #[must_use]
    pub fn new(index: usize, now_ms: u64) -> Self {
        Self {
            start_microrows: index_to_microrows(index),
            target_index: Some(index),
            started_ms: now_ms,
            duration_ms: 0,
        }
    }

    pub fn retarget(&mut self, index: usize, now_ms: u64) {
        let current = self.position_microrows(now_ms);
        let target = index_to_microrows(index);
        let max_distance = MAX_SELECTION_GLIDE_ROWS * ROW_SCALE;
        self.start_microrows = current.clamp(target - max_distance, target + max_distance);
        self.target_index = Some(index);
        self.started_ms = now_ms;
        self.duration_ms = if self.start_microrows == target {
            0
        } else {
            SELECTION_MOTION_MS
        };
    }

    pub fn snap(&mut self, index: usize, now_ms: u64) {
        self.start_microrows = index_to_microrows(index);
        self.target_index = Some(index);
        self.started_ms = now_ms;
        self.duration_ms = 0;
    }

    pub fn reset(&mut self) {
        self.start_microrows = 0;
        self.target_index = None;
        self.started_ms = 0;
        self.duration_ms = 0;
    }

    #[must_use]
    pub fn position(&self, now_ms: u64) -> f64 {
        microrows_to_position(self.position_microrows(now_ms))
    }

    #[must_use]
    pub fn current_index(&self) -> Option<usize> {
        self.target_index
    }

    #[must_use]
    pub fn rounded_index(&self, now_ms: u64) -> Option<usize> {
        self.target_index?;
        let position = self.position_microrows(now_ms);
        let rounded = position.saturating_add(ROW_SCALE / 2) / ROW_SCALE;
        usize::try_from(rounded.max(0)).ok()
    }

    #[must_use]
    pub fn is_transitioning(&self, now_ms: u64) -> bool {
        self.target_index.is_some() && now_ms.saturating_sub(self.started_ms) < self.duration_ms
    }

    fn position_microrows(&self, now_ms: u64) -> i64 {
        let Some(target_index) = self.target_index else {
            return 0;
        };
        let target = index_to_microrows(target_index);
        if self.duration_ms == 0 {
            return target;
        }
        interpolate_i64(
            self.start_microrows,
            target,
            eased_phase_ppm(now_ms.saturating_sub(self.started_ms), self.duration_ms),
        )
    }
}

#[must_use]
pub fn spinner_index(elapsed_ms: u64) -> usize {
    ((elapsed_ms / SPINNER_FRAME_MS) % SPINNER_FRAMES.len() as u64) as usize
}

fn finite_unit(value: f64) -> f64 {
    if value.is_nan() {
        0.0
    } else {
        value.clamp(0.0, 1.0)
    }
}

fn fraction_ppm(position_ms: u64, duration_ms: u64) -> u32 {
    let scaled = u128::from(position_ms.min(duration_ms)) * u128::from(FRACTION_SCALE);
    let fraction = scaled.checked_div(u128::from(duration_ms)).unwrap_or(0);
    u32::try_from(fraction).unwrap_or(FRACTION_SCALE)
}

fn ppm_to_unit(value: u32) -> f64 {
    f64::from(value) / f64::from(FRACTION_SCALE)
}

fn eased_phase_ppm(elapsed_ms: u64, duration_ms: u64) -> u32 {
    if duration_ms == 0 {
        return FRACTION_SCALE;
    }
    let linear = u128::from(elapsed_ms.min(duration_ms)) * u128::from(FRACTION_SCALE)
        / u128::from(duration_ms);
    let inverse = u128::from(FRACTION_SCALE).saturating_sub(linear);
    let inverse_cubed = inverse * inverse * inverse;
    let scale_squared = u128::from(FRACTION_SCALE) * u128::from(FRACTION_SCALE);
    let eased = u128::from(FRACTION_SCALE).saturating_sub(inverse_cubed / scale_squared);
    u32::try_from(eased).unwrap_or(FRACTION_SCALE)
}

fn interpolate_u32(start: u32, target: u32, phase_ppm: u32) -> u32 {
    let delta = i128::from(target) - i128::from(start);
    let offset = delta * i128::from(phase_ppm) / i128::from(FRACTION_SCALE);
    u32::try_from(i128::from(start) + offset).unwrap_or(if delta.is_negative() {
        0
    } else {
        u32::MAX
    })
}

fn interpolate_i64(start: i64, target: i64, phase_ppm: u32) -> i64 {
    let delta = i128::from(target) - i128::from(start);
    let offset = delta * i128::from(phase_ppm) / i128::from(FRACTION_SCALE);
    i64::try_from(i128::from(start) + offset).unwrap_or(if delta.is_negative() {
        i64::MIN
    } else {
        i64::MAX
    })
}

fn index_to_microrows(index: usize) -> i64 {
    i64::try_from(index)
        .unwrap_or(i64::MAX / ROW_SCALE)
        .saturating_mul(ROW_SCALE)
}

fn microrows_to_position(value: i64) -> f64 {
    let rows = value / ROW_SCALE;
    let fraction = value % ROW_SCALE;
    let rows = i32::try_from(rows).unwrap_or(if rows.is_negative() {
        i32::MIN
    } else {
        i32::MAX
    });
    let fraction = i32::try_from(fraction).unwrap_or(0);
    f64::from(rows) + f64::from(fraction) / 1_000_000.0
}

fn shimmer_elapsed(accumulated_ms: u64, started_ms: u64, now_ms: u64, playing: bool) -> u64 {
    if playing {
        accumulated_ms.saturating_add(now_ms.saturating_sub(started_ms))
    } else {
        accumulated_ms
    }
}

#[cfg(test)]
mod tests {
    use super::{
        MotionFrame, ProgressChange, ProgressMotion, SPINNER_FRAMES, SelectionMotion, spinner_index,
    };

    fn assert_close(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 0.000_001,
            "{actual} != {expected}"
        );
    }

    #[test]
    fn spinner_advances_through_the_shared_sequence_and_wraps() {
        assert_eq!(spinner_index(0), 0);
        assert_eq!(spinner_index(80), 1);
        assert_eq!(spinner_index(720), 9);
        assert_eq!(spinner_index(800), 0);
        assert_eq!(SPINNER_FRAMES[spinner_index(880)], "⠙");
    }

    #[test]
    fn motion_frame_clamps_non_finite_and_out_of_range_progress() {
        let frame = MotionFrame::new(160, f64::INFINITY, -2.0);
        assert_eq!(frame.elapsed_ms, 160);
        assert_eq!(frame.spinner_index, 2);
        assert_close(frame.progress.fraction, 1.0);
        assert_close(frame.progress.shimmer_phase, 0.0);
    }

    #[test]
    fn ordinary_progress_retarget_eases_monotonically() {
        let mut motion = ProgressMotion::default();
        motion.reconcile(0, 1, 2_000, Some(10_000), true, ProgressChange::Media);
        motion.reconcile(
            1_000,
            1,
            4_000,
            Some(10_000),
            true,
            ProgressChange::Continuous,
        );

        let start = motion.presentation(1_000).fraction;
        let middle = motion.presentation(1_090).fraction;
        let end = motion.presentation(1_180).fraction;
        assert_close(start, 0.3);
        assert!(start < middle && middle < end);
        assert_close(end, 0.418);
    }

    #[test]
    fn seek_converges_faster_than_an_ordinary_retarget() {
        let mut ordinary = ProgressMotion::default();
        ordinary.reconcile(0, 1, 1_000, Some(10_000), false, ProgressChange::Media);
        ordinary.reconcile(
            1_000,
            1,
            8_000,
            Some(10_000),
            false,
            ProgressChange::Continuous,
        );
        let mut seek = ProgressMotion::default();
        seek.reconcile(0, 1, 1_000, Some(10_000), false, ProgressChange::Media);
        seek.reconcile(1_000, 1, 8_000, Some(10_000), false, ProgressChange::Seek);

        assert!(seek.presentation(1_090).fraction > ordinary.presentation(1_090).fraction);
        assert_close(seek.presentation(1_090).fraction, 0.8);
    }

    #[test]
    fn media_generation_change_snaps_instead_of_easing() {
        let mut motion = ProgressMotion::default();
        motion.reconcile(0, 1, 8_000, Some(10_000), true, ProgressChange::Media);
        motion.reconcile(10, 2, 500, Some(10_000), true, ProgressChange::Continuous);

        assert_close(motion.presentation(10).fraction, 0.05);
    }

    #[test]
    fn pause_freezes_fill_and_shimmer_then_resume_continues() {
        let mut motion = ProgressMotion::default();
        motion.reconcile(0, 1, 2_000, Some(10_000), true, ProgressChange::Media);
        let playing = motion.presentation(1_000);
        motion.reconcile(
            1_000,
            1,
            3_000,
            Some(10_000),
            false,
            ProgressChange::Continuous,
        );
        let paused = motion.presentation(4_000);
        assert_close(paused.fraction, playing.fraction);
        assert_close(paused.shimmer_phase, playing.shimmer_phase);

        motion.reconcile(
            4_000,
            1,
            3_000,
            Some(10_000),
            true,
            ProgressChange::Continuous,
        );
        assert!(motion.presentation(4_500).fraction > paused.fraction);
        assert!(
            (motion.presentation(4_500).shimmer_phase - paused.shimmer_phase).abs() > 0.000_001
        );
    }

    #[test]
    fn end_of_track_is_exact_and_unknown_duration_is_empty() {
        let mut motion = ProgressMotion::default();
        motion.reconcile(0, 1, 9_999, Some(10_000), true, ProgressChange::Media);
        assert_close(motion.presentation(1).fraction, 1.0);

        motion.reconcile(2, 2, 5_000, None, true, ProgressChange::Media);
        assert_close(motion.presentation(10_000).fraction, 0.0);
    }

    #[test]
    fn selection_glides_one_row_and_finishes_exactly() {
        let mut motion = SelectionMotion::new(2, 0);
        motion.retarget(3, 100);
        assert_close(motion.position(100), 2.0);
        assert!(motion.position(175) > 2.0 && motion.position(175) < 3.0);
        assert_close(motion.position(250), 3.0);
        assert!(!motion.is_transitioning(250));
    }

    #[test]
    fn selection_rapid_retarget_starts_at_current_visual_position() {
        let mut motion = SelectionMotion::new(0, 0);
        motion.retarget(4, 0);
        let visual = motion.position(60);
        motion.retarget(1, 60);

        assert_close(motion.position(60), visual);
        assert!(motion.position(120) < visual);
    }

    #[test]
    fn selection_large_moves_are_capped_but_reach_the_target() {
        let mut motion = SelectionMotion::new(0, 0);
        motion.retarget(100, 0);
        assert!(motion.position(0) >= 94.0);
        assert_close(motion.position(150), 100.0);
    }

    #[test]
    fn selection_snap_and_reset_clear_transition() {
        let mut motion = SelectionMotion::new(1, 0);
        motion.retarget(5, 0);
        motion.snap(9, 20);
        assert_close(motion.position(20), 9.0);
        assert!(!motion.is_transitioning(20));

        motion.reset();
        assert_eq!(motion.current_index(), None);
        assert!(!motion.is_transitioning(1_000));
    }

    #[test]
    fn zero_elapsed_time_is_safe_for_both_motion_types() {
        let mut progress = ProgressMotion::default();
        progress.reconcile(7, 1, 250, Some(1_000), false, ProgressChange::Media);
        assert_close(progress.presentation(7).fraction, 0.25);

        let mut selection = SelectionMotion::new(4, 7);
        selection.retarget(5, 7);
        assert_close(selection.position(7), 4.0);
    }
}
