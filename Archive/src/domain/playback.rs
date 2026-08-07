use std::{fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

use super::MediaId;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub enum RepeatMode {
    Off,
    One,
    All,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub enum PlaybackStatus {
    Stopped,
    Resolving,
    Buffering,
    Playing,
    Paused,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct PlaybackSnapshot {
    pub current: Option<MediaId>,
    pub status: PlaybackStatus,
    pub position_ms: u64,
    pub duration_ms: Option<u64>,
    pub target_volume: u8,
    pub playback_speed: f64,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct RegionCode(String);

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum RegionCodeError {
    #[error("region code must contain exactly two ASCII letters, got {value:?}")]
    InvalidFormat { value: String },
}

impl RegionCode {
    /// Parses a two-letter ASCII region code and normalizes it to uppercase.
    ///
    /// # Errors
    ///
    /// Returns [`RegionCodeError::InvalidFormat`] when `value` is not exactly
    /// two ASCII alphabetic characters.
    pub fn parse(value: &str) -> Result<Self, RegionCodeError> {
        if value.len() == 2 && value.bytes().all(|byte| byte.is_ascii_alphabetic()) {
            return Ok(Self(value.to_ascii_uppercase()));
        }

        Err(RegionCodeError::InvalidFormat {
            value: value.to_owned(),
        })
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for RegionCode {
    fn default() -> Self {
        Self("ZZ".to_owned())
    }
}

impl fmt::Display for RegionCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for RegionCode {
    type Err = RegionCodeError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl Serialize for RegionCode {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for RegionCode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(serde::de::Error::custom)
    }
}
