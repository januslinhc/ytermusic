use std::{error::Error, fmt};

use async_trait::async_trait;
use url::Url;

use crate::diagnostics::sanitize;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlayerErrorCategory {
    Spawn,
    Connection,
    Protocol,
    Command,
    Backend,
    Closed,
}

#[derive(Clone, Eq, PartialEq)]
pub struct PlayerError {
    category: PlayerErrorCategory,
    message: String,
}

impl PlayerError {
    #[must_use]
    pub fn new(category: PlayerErrorCategory, message: impl AsRef<str>) -> Self {
        Self {
            category,
            message: sanitize(message.as_ref()),
        }
    }

    #[must_use]
    pub const fn category(&self) -> PlayerErrorCategory {
        self.category
    }

    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for PlayerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl fmt::Debug for PlayerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PlayerError")
            .field("category", &self.category)
            .field("message", &self.message)
            .finish()
    }
}

impl Error for PlayerError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlayerEndReason {
    Natural,
    UrlRejected,
    Replaced,
    Stopped,
    Unknown,
}

/// Monotonic identifier for one mpv load lifecycle.
///
/// Epochs carry no media URL or other user data.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct LoadEpoch(u64);

impl LoadEpoch {
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum PlayerEvent {
    LoadStarted {
        epoch: LoadEpoch,
    },
    FileLoaded {
        epoch: LoadEpoch,
    },
    Progress {
        epoch: LoadEpoch,
        position_ms: u64,
        duration_ms: Option<u64>,
    },
    PauseChanged {
        epoch: LoadEpoch,
        paused: bool,
    },
    VolumeChanged(f64),
    SpeedChanged(f64),
    Ended {
        epoch: LoadEpoch,
        reason: PlayerEndReason,
    },
    Shutdown,
}

#[async_trait]
pub trait PlayerBackend: Send {
    /// Submits one load in event-delivery order.
    ///
    /// Within one backend session, successful calls and
    /// [`PlayerEvent::LoadStarted`] events have the same order. Session
    /// termination invalidates outstanding pairs. On error, the backend must
    /// discard pending load events from that session so the next successful
    /// submission starts a fresh event order.
    async fn load(&mut self, url: &Url, start_ms: Option<u64>) -> Result<(), PlayerError>;
    /// Terminates the current session and discards all pending events while
    /// keeping the backend reusable for a later load.
    async fn reset_session(&mut self);
    async fn set_paused(&mut self, paused: bool) -> Result<(), PlayerError>;
    async fn seek_relative(&mut self, seconds: i64) -> Result<(), PlayerError>;
    async fn set_volume(&mut self, volume: f64) -> Result<(), PlayerError>;
    async fn set_speed(&mut self, speed: f64) -> Result<(), PlayerError>;
    async fn next_event(&mut self) -> Result<PlayerEvent, PlayerError>;
    async fn shutdown(&mut self) -> Result<(), PlayerError>;
}
