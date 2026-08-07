use std::time::Duration;

use crate::config::PlaybackConfig;

const SILENCE: f64 = 0.0;
const MAX_VOLUME: f64 = 100.0;

/// A pure linear volume transition on the application's 0–100 volume scale.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FadeEnvelope {
    from: f64,
    to: f64,
    duration: Duration,
}

impl FadeEnvelope {
    /// Creates a linear transition between two safely normalized volumes.
    ///
    /// Finite values are clamped to `0.0..=100.0`, `NaN` and negative
    /// infinity become silence, and positive infinity becomes `100.0`.
    #[must_use]
    pub fn linear(from: f64, to: f64, duration: Duration) -> Self {
        Self {
            from: normalize_volume(from),
            to: normalize_volume(to),
            duration,
        }
    }

    /// Samples the transition without consulting a clock or changing state.
    #[must_use]
    pub fn sample(self, elapsed: Duration) -> f64 {
        if self.duration.is_zero() || elapsed >= self.duration {
            return self.to;
        }
        if elapsed.is_zero() {
            return self.from;
        }

        let progress = elapsed.as_secs_f64() / self.duration.as_secs_f64();
        normalize_volume(self.from + (self.to - self.from) * progress)
    }

    /// Returns whether the transition has reached its destination.
    #[must_use]
    pub fn is_complete(self, elapsed: Duration) -> bool {
        elapsed >= self.duration
    }
}

/// The result to apply when an in-progress fade is cancelled.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FadeCancel {
    /// Restore the latest volume requested by the user.
    RestoreTarget,
    /// Force the effective output to silence while preserving user intent.
    Silence,
    /// Leave the effective output at the last sampled volume.
    KeepCurrent,
}

/// A playback event that selects a configured fade direction and duration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FadeIntent {
    Play,
    Resume,
    Pause,
    Stop,
    Replace,
    NaturalEnd,
}

/// The audible direction of an active volume transition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FadeDirection {
    In,
    Out,
}

/// Maps playback intent and immutable configuration to a pure fade envelope.
///
/// Play and resume begin at silence and use the configured fade-in. All
/// terminating or replacing intents begin at `current_volume` and use the
/// configured fade-out.
#[must_use]
pub fn envelope_for_intent(
    intent: FadeIntent,
    config: &PlaybackConfig,
    current_volume: f64,
) -> FadeEnvelope {
    match intent {
        FadeIntent::Play | FadeIntent::Resume => FadeEnvelope::linear(
            SILENCE,
            f64::from(config.volume),
            Duration::from_millis(config.fade_in_ms),
        ),
        FadeIntent::Pause | FadeIntent::Stop | FadeIntent::Replace | FadeIntent::NaturalEnd => {
            FadeEnvelope::linear(
                current_volume,
                SILENCE,
                Duration::from_millis(config.fade_out_ms),
            )
        }
    }
}

/// Owns the currently effective output volume and at most one fade envelope.
#[derive(Clone, Debug, PartialEq)]
pub struct FadeController {
    target_volume: f64,
    effective_volume: f64,
    envelope: Option<FadeEnvelope>,
    elapsed: Duration,
}

impl FadeController {
    /// Creates an idle controller at a safely normalized user target.
    #[must_use]
    pub fn new(target_volume: f64) -> Self {
        let target_volume = normalize_volume(target_volume);
        Self {
            target_volume,
            effective_volume: target_volume,
            envelope: None,
            elapsed: Duration::ZERO,
        }
    }

    /// Returns the latest safely normalized volume requested by the user.
    #[must_use]
    pub fn target_volume(&self) -> f64 {
        self.target_volume
    }

    /// Returns the current safely normalized volume to apply to audio output.
    #[must_use]
    pub fn effective_volume(&self) -> f64 {
        self.effective_volume
    }

    /// Returns whether a fade still has samples remaining.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.envelope.is_some()
    }

    /// Returns the direction of the active fade, or `None` at a stable endpoint.
    #[must_use]
    pub fn direction(&self) -> Option<FadeDirection> {
        self.envelope.and_then(|envelope| {
            if envelope.to > envelope.from {
                Some(FadeDirection::In)
            } else if envelope.to < envelope.from {
                Some(FadeDirection::Out)
            } else {
                None
            }
        })
    }

    /// Updates user intent without causing an abrupt change during a fade.
    ///
    /// An audible destination is retargeted from the current sample over the
    /// old envelope's remaining duration. A fade to silence continues while
    /// retaining the new target for a later restore. When idle, the effective
    /// volume changes immediately.
    pub fn set_target_volume(&mut self, target_volume: f64) {
        self.target_volume = normalize_volume(target_volume);

        let Some(envelope) = self.envelope else {
            self.effective_volume = self.target_volume;
            return;
        };

        if envelope.to != SILENCE {
            let remaining = envelope.duration.saturating_sub(self.elapsed);
            self.start_from_current(self.target_volume, remaining);
        }
    }

    /// Starts a fade, replacing any active fade without an effective-volume jump.
    ///
    /// When a fade is already active, its latest sampled effective volume takes
    /// precedence over `from`. This ensures a newer transition is continuous.
    pub fn start(&mut self, from: f64, to: f64, duration: Duration) {
        self.start_envelope(FadeEnvelope::linear(from, to, duration));
    }

    /// Starts a prepared envelope without exposing or duplicating its policy.
    ///
    /// An idle controller honors the envelope's original starting volume. When
    /// replacing an active fade, the new envelope instead begins at the current
    /// effective volume while preserving its destination and duration.
    pub fn start_envelope(&mut self, envelope: FadeEnvelope) {
        let envelope = if self.is_active() {
            FadeEnvelope::linear(self.effective_volume, envelope.to, envelope.duration)
        } else {
            envelope
        };
        self.begin(envelope);
    }

    /// Starts a fade from the current effective volume.
    pub fn start_from_current(&mut self, to: f64, duration: Duration) {
        self.start_envelope(FadeEnvelope::linear(self.effective_volume, to, duration));
    }

    /// Advances the active fade solely by the supplied monotonic delta.
    ///
    /// The returned value is the effective volume after advancing.
    pub fn tick(&mut self, delta: Duration) -> f64 {
        let Some(envelope) = self.envelope else {
            return self.effective_volume;
        };

        self.elapsed = self.elapsed.saturating_add(delta);
        self.effective_volume = envelope.sample(self.elapsed);

        if envelope.is_complete(self.elapsed) {
            self.effective_volume = envelope.to;
            self.envelope = None;
            self.elapsed = Duration::ZERO;
        }

        self.effective_volume
    }

    /// Cancels the active envelope and applies the requested stable outcome.
    pub fn cancel(&mut self, outcome: FadeCancel) {
        self.envelope = None;
        self.elapsed = Duration::ZERO;
        self.effective_volume = match outcome {
            FadeCancel::RestoreTarget => self.target_volume,
            FadeCancel::Silence => SILENCE,
            FadeCancel::KeepCurrent => normalize_volume(self.effective_volume),
        };
    }

    fn begin(&mut self, envelope: FadeEnvelope) {
        self.elapsed = Duration::ZERO;
        self.effective_volume = envelope.sample(self.elapsed);

        if envelope.is_complete(self.elapsed) {
            self.envelope = None;
        } else {
            self.envelope = Some(envelope);
        }
    }
}

fn normalize_volume(volume: f64) -> f64 {
    if volume.is_nan() || volume == f64::NEG_INFINITY {
        SILENCE
    } else if volume == f64::INFINITY {
        MAX_VOLUME
    } else {
        volume.clamp(SILENCE, MAX_VOLUME)
    }
}
