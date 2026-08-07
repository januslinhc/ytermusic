use std::time::Duration;

use ytermusic::{
    config::PlaybackConfig,
    fade::{
        FadeCancel, FadeController, FadeDirection, FadeEnvelope, FadeIntent, envelope_for_intent,
    },
};

fn assert_volume_eq(actual: f64, expected: f64) {
    assert_eq!(
        actual.to_bits(),
        expected.to_bits(),
        "expected volume {expected}, got {actual}"
    );
}

#[test]
fn linear_envelope_interpolates_and_clamps_after_duration() {
    let envelope = FadeEnvelope::linear(0.0, 80.0, Duration::from_secs(2));

    assert_volume_eq(envelope.sample(Duration::ZERO), 0.0);
    assert_volume_eq(envelope.sample(Duration::from_secs(1)), 40.0);
    assert_volume_eq(envelope.sample(Duration::from_secs(3)), 80.0);
}

#[test]
fn linear_envelope_is_clamped_at_both_boundaries() {
    let envelope = FadeEnvelope::linear(20.0, 60.0, Duration::from_secs(2));

    assert_volume_eq(envelope.sample(Duration::ZERO), 20.0);
    assert_volume_eq(envelope.sample(Duration::from_secs(2)), 60.0);
    assert_volume_eq(envelope.sample(Duration::from_secs(20)), 60.0);
    assert!(!envelope.is_complete(Duration::from_millis(1_999)));
    assert!(envelope.is_complete(Duration::from_secs(2)));
    assert!(envelope.is_complete(Duration::from_secs(20)));
}

#[test]
fn zero_duration_envelope_immediately_returns_destination() {
    let envelope = FadeEnvelope::linear(15.0, 75.0, Duration::ZERO);

    assert_volume_eq(envelope.sample(Duration::ZERO), 75.0);
    assert_volume_eq(envelope.sample(Duration::from_secs(1)), 75.0);
    assert!(envelope.is_complete(Duration::ZERO));
}

#[test]
fn newer_fade_starts_at_current_effective_volume_and_preserves_target() {
    let mut controller = FadeController::new(80.0);
    controller.start(0.0, 80.0, Duration::from_secs(4));
    controller.tick(Duration::from_secs(1));

    assert_volume_eq(controller.effective_volume(), 20.0);

    controller.start_from_current(0.0, Duration::from_secs(1));

    assert_volume_eq(controller.effective_volume(), 20.0);
    assert_volume_eq(controller.target_volume(), 80.0);
    assert!(controller.is_active());
}

#[test]
fn explicit_start_also_replaces_an_active_fade_without_a_volume_jump() {
    let mut controller = FadeController::new(80.0);
    controller.start(0.0, 80.0, Duration::from_secs(4));
    controller.tick(Duration::from_secs(1));

    controller.start(99.0, 0.0, Duration::from_secs(1));

    assert_volume_eq(controller.effective_volume(), 20.0);
    controller.tick(Duration::from_millis(500));
    assert_volume_eq(controller.effective_volume(), 10.0);
}

#[test]
fn completing_a_fade_sets_exact_destination_and_clears_it() {
    let mut controller = FadeController::new(80.0);
    controller.start(0.0, 80.0, Duration::from_secs(3));

    controller.tick(Duration::from_secs(4));

    assert_volume_eq(controller.effective_volume(), 80.0);
    assert_volume_eq(controller.target_volume(), 80.0);
    assert!(!controller.is_active());

    controller.tick(Duration::from_secs(1));
    assert_volume_eq(controller.effective_volume(), 80.0);
}

#[test]
fn direction_tracks_active_fades_through_ticks_completion_and_cancel() {
    let mut controller = FadeController::new(80.0);

    assert_eq!(controller.direction(), None);

    controller.start(0.0, 80.0, Duration::from_secs(2));
    assert_eq!(controller.direction(), Some(FadeDirection::In));
    controller.tick(Duration::from_secs(1));
    assert_eq!(controller.direction(), Some(FadeDirection::In));
    controller.tick(Duration::from_secs(1));
    assert_eq!(controller.direction(), None);

    controller.start_from_current(0.0, Duration::from_secs(2));
    assert_eq!(controller.direction(), Some(FadeDirection::Out));
    controller.tick(Duration::from_secs(1));
    assert_eq!(controller.direction(), Some(FadeDirection::Out));
    controller.cancel(FadeCancel::KeepCurrent);
    assert_eq!(controller.direction(), None);
}

#[test]
fn zero_duration_fade_reaches_its_endpoint_without_an_active_direction() {
    let mut controller = FadeController::new(80.0);

    controller.start(80.0, 0.0, Duration::ZERO);

    assert_volume_eq(controller.effective_volume(), 0.0);
    assert_eq!(controller.direction(), None);
}

#[test]
fn equal_endpoint_envelope_has_no_audible_direction() {
    let mut controller = FadeController::new(20.0);

    controller.start(20.0, 20.0, Duration::from_secs(1));

    assert!(controller.is_active());
    assert_eq!(controller.direction(), None);
}

#[test]
fn zero_duration_start_is_immediate_and_inactive() {
    let mut controller = FadeController::new(80.0);

    controller.start(10.0, 35.0, Duration::ZERO);

    assert_volume_eq(controller.effective_volume(), 35.0);
    assert_volume_eq(controller.target_volume(), 80.0);
    assert!(!controller.is_active());
}

#[test]
fn cancelling_can_restore_latest_target_or_force_silence() {
    let mut controller = FadeController::new(80.0);
    controller.start(80.0, 0.0, Duration::from_secs(4));
    controller.tick(Duration::from_secs(1));
    controller.set_target_volume(65.0);

    controller.cancel(FadeCancel::RestoreTarget);

    assert_volume_eq(controller.effective_volume(), 65.0);
    assert_volume_eq(controller.target_volume(), 65.0);
    assert!(!controller.is_active());

    controller.start_from_current(0.0, Duration::from_secs(2));
    controller.cancel(FadeCancel::Silence);

    assert_volume_eq(controller.effective_volume(), 0.0);
    assert_volume_eq(controller.target_volume(), 65.0);
    assert!(!controller.is_active());
}

#[test]
fn cancelling_can_keep_current_effective_volume() {
    let mut controller = FadeController::new(80.0);
    controller.start(0.0, 80.0, Duration::from_secs(4));
    controller.tick(Duration::from_secs(1));

    controller.cancel(FadeCancel::KeepCurrent);

    assert_volume_eq(controller.effective_volume(), 20.0);
    assert_volume_eq(controller.target_volume(), 80.0);
    assert!(!controller.is_active());
}

#[test]
fn changing_target_retargets_active_fade_in_over_remaining_time() {
    let mut controller = FadeController::new(80.0);
    controller.start(0.0, 80.0, Duration::from_secs(4));
    controller.tick(Duration::from_secs(1));

    controller.set_target_volume(50.0);

    assert_volume_eq(controller.effective_volume(), 20.0);
    assert_volume_eq(controller.target_volume(), 50.0);
    assert!(controller.is_active());

    controller.tick(Duration::from_secs(3));
    assert_volume_eq(controller.effective_volume(), 50.0);
    assert!(!controller.is_active());
}

#[test]
fn changing_target_during_fade_out_preserves_silencing_envelope() {
    let mut controller = FadeController::new(80.0);
    controller.start(80.0, 0.0, Duration::from_secs(4));
    controller.tick(Duration::from_secs(1));

    controller.set_target_volume(50.0);
    controller.tick(Duration::from_secs(3));

    assert_volume_eq(controller.effective_volume(), 0.0);
    assert_volume_eq(controller.target_volume(), 50.0);
    assert!(!controller.is_active());
}

#[test]
fn play_and_resume_map_to_configured_fade_in() {
    let config = PlaybackConfig {
        volume: 72,
        fade_in_ms: 600,
        fade_out_ms: 900,
    };

    for intent in [FadeIntent::Play, FadeIntent::Resume] {
        let envelope = envelope_for_intent(intent, &config, 33.0);

        assert_volume_eq(envelope.sample(Duration::ZERO), 0.0);
        assert_volume_eq(envelope.sample(Duration::from_millis(300)), 36.0);
        assert_volume_eq(envelope.sample(Duration::from_millis(600)), 72.0);
        assert!(!envelope.is_complete(Duration::from_millis(599)));
        assert!(envelope.is_complete(Duration::from_millis(600)));
    }
}

#[test]
fn pause_stop_replace_and_natural_end_map_to_configured_fade_out() {
    let config = PlaybackConfig {
        volume: 72,
        fade_in_ms: 600,
        fade_out_ms: 900,
    };

    for intent in [
        FadeIntent::Pause,
        FadeIntent::Stop,
        FadeIntent::Replace,
        FadeIntent::NaturalEnd,
    ] {
        let envelope = envelope_for_intent(intent, &config, 60.0);

        assert_volume_eq(envelope.sample(Duration::ZERO), 60.0);
        assert_volume_eq(envelope.sample(Duration::from_millis(450)), 30.0);
        assert_volume_eq(envelope.sample(Duration::from_millis(900)), 0.0);
        assert!(!envelope.is_complete(Duration::from_millis(899)));
        assert!(envelope.is_complete(Duration::from_millis(900)));
    }
}

#[test]
fn mapped_resume_interrupts_fade_out_without_a_volume_jump() {
    let config = PlaybackConfig {
        volume: 80,
        fade_in_ms: 2_000,
        fade_out_ms: 4_000,
    };
    let mut controller = FadeController::new(80.0);
    let fade_out = envelope_for_intent(FadeIntent::Pause, &config, 60.0);

    controller.start_envelope(fade_out);

    assert_volume_eq(controller.effective_volume(), 60.0);
    controller.tick(Duration::from_secs(1));
    assert_volume_eq(controller.effective_volume(), 45.0);

    let resume = envelope_for_intent(FadeIntent::Resume, &config, controller.effective_volume());
    controller.start_envelope(resume);

    assert_volume_eq(controller.effective_volume(), 45.0);
    assert_volume_eq(controller.target_volume(), 80.0);
    controller.tick(Duration::from_secs(1));
    assert_volume_eq(controller.effective_volume(), 62.5);
    assert!(controller.is_active());

    controller.tick(Duration::from_secs(1));
    assert_volume_eq(controller.effective_volume(), 80.0);
    assert_volume_eq(controller.target_volume(), 80.0);
    assert!(!controller.is_active());
}

#[test]
fn zero_duration_mapped_envelope_completes_immediately() {
    let config = PlaybackConfig {
        volume: 70,
        fade_in_ms: 0,
        fade_out_ms: 900,
    };
    let mut controller = FadeController::new(70.0);
    let play = envelope_for_intent(FadeIntent::Play, &config, 35.0);

    controller.start_envelope(play);

    assert_volume_eq(controller.effective_volume(), 70.0);
    assert!(!controller.is_active());
}

#[test]
fn zero_duration_intent_mappings_are_immediate() {
    let config = PlaybackConfig {
        volume: 70,
        fade_in_ms: 0,
        fade_out_ms: 0,
    };

    let play = envelope_for_intent(FadeIntent::Play, &config, 35.0);
    let stop = envelope_for_intent(FadeIntent::Stop, &config, 35.0);

    assert_volume_eq(play.sample(Duration::ZERO), 70.0);
    assert!(play.is_complete(Duration::ZERO));
    assert_volume_eq(stop.sample(Duration::ZERO), 0.0);
    assert!(stop.is_complete(Duration::ZERO));
}

#[test]
fn volumes_are_normalized_to_safe_finite_range() {
    let cases = [
        (f64::NAN, 0.0),
        (f64::NEG_INFINITY, 0.0),
        (f64::INFINITY, 100.0),
        (-25.0, 0.0),
        (125.0, 100.0),
        (42.0, 42.0),
    ];

    for (requested, expected) in cases {
        let envelope = FadeEnvelope::linear(requested, requested, Duration::from_secs(1));
        assert_volume_eq(envelope.sample(Duration::ZERO), expected);
        assert_volume_eq(envelope.sample(Duration::from_millis(500)), expected);

        let controller = FadeController::new(requested);
        assert_volume_eq(controller.target_volume(), expected);
        assert_volume_eq(controller.effective_volume(), expected);
    }
}

#[test]
fn controller_normalizes_every_requested_volume_without_nan_or_infinity() {
    let mut controller = FadeController::new(80.0);

    controller.start(f64::NAN, f64::INFINITY, Duration::from_secs(2));
    assert_volume_eq(controller.effective_volume(), 0.0);
    controller.tick(Duration::from_secs(1));
    assert_volume_eq(controller.effective_volume(), 50.0);
    assert!(controller.effective_volume().is_finite());

    controller.set_target_volume(f64::NEG_INFINITY);
    assert_volume_eq(controller.target_volume(), 0.0);
    controller.cancel(FadeCancel::RestoreTarget);
    assert_volume_eq(controller.effective_volume(), 0.0);
}
