use std::{
    fmt, fs, io,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::domain::RegionCode;

#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    pub region: RegionCode,
    pub playback: PlaybackConfig,
    pub podcast: PodcastConfig,
    pub behavior: BehaviorConfig,
    pub lyrics: LyricsConfig,
    pub artwork: ArtworkConfig,
    pub visualizer: VisualizerConfig,
    pub notifications: NotificationsConfig,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct PlaybackConfig {
    /// Target output volume on a 0–100 scale. Defaults to 80.
    pub volume: u8,
    /// Defaults to a conservative 250 ms fade-in.
    pub fade_in_ms: u64,
    /// Defaults to a conservative 250 ms fade-out.
    pub fade_out_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct PodcastConfig {
    /// Playback multiplier. Defaults to 1.0 (original speed).
    pub speed: f64,
    /// Defaults to a 15-second replay interval for missed speech.
    pub skip_backward_seconds: u64,
    /// Defaults to a 30-second interval for skipping past a segment.
    pub skip_forward_seconds: u64,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct BehaviorConfig {
    /// Defaults to a brief 60-second cache to keep resolved streams fresh.
    pub resolver_cache_seconds: u64,
    /// Defaults to resuming the last session.
    pub resume_session: bool,
    /// Defaults to preserving unavailable entries instead of silently skipping them.
    pub auto_skip_unavailable: bool,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct LyricsConfig {
    /// Enables lyrics retrieval and presentation. Defaults to enabled.
    pub enabled: bool,
    /// Allows bounded track metadata to be sent to an external sync source.
    pub external_sync: bool,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct ArtworkConfig {
    /// Enables genuine video-backed artwork when available. Defaults to enabled.
    pub animated: bool,
    /// Maximum animation frame rate. Defaults to 8 frames per second.
    pub max_fps: u8,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct VisualizerConfig {
    /// Enables the sound-reactive player spectrum. Defaults to enabled.
    pub enabled: bool,
    /// Maximum spectrum frame rate. Defaults to 15 frames per second.
    pub max_fps: u8,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct NotificationsConfig {
    /// Enables native now-playing notifications. Defaults to enabled.
    pub enabled: bool,
    /// Pre-registered Windows `AppUserModelID`. Defaults to absent.
    pub windows_aum_id: Option<WindowsAumId>,
}

#[derive(Clone, PartialEq, Serialize)]
#[serde(transparent)]
pub struct WindowsAumId(String);

impl WindowsAumId {
    /// Parses Microsoft's bounded application-defined `AppUserModelID` form.
    ///
    /// # Errors
    ///
    /// Returns a redacted error for empty, oversized, whitespace-bearing, or
    /// structurally incomplete identifiers.
    pub fn parse(value: &str) -> Result<Self, WindowsAumIdError> {
        let valid_chars = value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_')
        });
        let mut sections = value.split('.');
        let company = sections.next().unwrap_or_default();
        let product = sections.next().unwrap_or_default();
        if value.is_empty()
            || value.chars().count() > 128
            || !valid_chars
            || company.is_empty()
            || product.is_empty()
            || value.split('.').any(str::is_empty)
        {
            return Err(WindowsAumIdError);
        }
        Ok(Self(value.to_owned()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for WindowsAumId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(serde::de::Error::custom)
    }
}

impl fmt::Debug for WindowsAumId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("WindowsAumId([REDACTED])")
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[error("invalid Windows AppUserModelID")]
pub struct WindowsAumIdError;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("invalid value for {field}: {value}; expected {expected}")]
    InvalidValue {
        field: &'static str,
        value: String,
        expected: &'static str,
    },
    #[error("failed to {operation} configuration path {path}: {source}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to parse configuration at {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("failed to serialize configuration: {source}")]
    Serialize {
        #[source]
        source: toml::ser::Error,
    },
}

impl Default for PlaybackConfig {
    fn default() -> Self {
        Self {
            volume: 80,
            fade_in_ms: 250,
            fade_out_ms: 250,
        }
    }
}

impl Default for PodcastConfig {
    fn default() -> Self {
        Self {
            speed: 1.0,
            skip_backward_seconds: 15,
            skip_forward_seconds: 30,
        }
    }
}

impl Default for BehaviorConfig {
    fn default() -> Self {
        Self {
            resolver_cache_seconds: 60,
            resume_session: true,
            auto_skip_unavailable: false,
        }
    }
}

impl Default for LyricsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            external_sync: true,
        }
    }
}

impl Default for ArtworkConfig {
    fn default() -> Self {
        Self {
            animated: true,
            max_fps: 8,
        }
    }
}

impl Default for VisualizerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_fps: 15,
        }
    }
}

impl Default for NotificationsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            windows_aum_id: None,
        }
    }
}

impl Config {
    /// Loads and validates configuration from `path`.
    ///
    /// A missing file is treated as a request for [`Config::default`].
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Io`] for read failures other than a missing file,
    /// [`ConfigError::Parse`] for malformed TOML, or
    /// [`ConfigError::InvalidValue`] when parsed values fail validation.
    pub fn load<P>(path: P) -> Result<Self, ConfigError>
    where
        P: AsRef<Path>,
    {
        let path = path.as_ref();
        let contents = match fs::read_to_string(path) {
            Ok(contents) => contents,
            Err(source) if source.kind() == io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(source) => {
                return Err(ConfigError::Io {
                    operation: "read",
                    path: path.to_owned(),
                    source,
                });
            }
        };
        let config: Self = toml::from_str(&contents).map_err(|source| ConfigError::Parse {
            path: path.to_owned(),
            source,
        })?;

        config.validate()?;
        Ok(config)
    }

    /// Validates and writes pretty TOML configuration to `path`.
    ///
    /// Missing parent directories are created when the path has a non-empty
    /// parent.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::InvalidValue`] when validation fails,
    /// [`ConfigError::Serialize`] when TOML serialization fails, or
    /// [`ConfigError::Io`] when directories or the file cannot be written.
    pub fn save<P>(&self, path: P) -> Result<(), ConfigError>
    where
        P: AsRef<Path>,
    {
        self.validate()?;

        let path = path.as_ref();
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent).map_err(|source| ConfigError::Io {
                operation: "create parent directories for",
                path: parent.to_owned(),
                source,
            })?;
        }
        let contents =
            toml::to_string_pretty(self).map_err(|source| ConfigError::Serialize { source })?;
        fs::write(path, contents).map_err(|source| ConfigError::Io {
            operation: "write",
            path: path.to_owned(),
            source,
        })
    }

    /// Verifies that every numeric setting is within its supported boundary.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::InvalidValue`] with the field, actual value, and
    /// expected range for the first invalid setting.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if !(0..=100).contains(&self.playback.volume) {
            return Err(invalid_value(
                "playback.volume",
                &self.playback.volume,
                "an integer from 0 through 100",
            ));
        }
        if !(0..=10_000).contains(&self.playback.fade_in_ms) {
            return Err(invalid_value(
                "playback.fade_in_ms",
                &self.playback.fade_in_ms,
                "milliseconds from 0 through 10000",
            ));
        }
        if !(0..=10_000).contains(&self.playback.fade_out_ms) {
            return Err(invalid_value(
                "playback.fade_out_ms",
                &self.playback.fade_out_ms,
                "milliseconds from 0 through 10000",
            ));
        }
        if !self.podcast.speed.is_finite() || !(0.5..=3.0).contains(&self.podcast.speed) {
            return Err(invalid_value(
                "podcast.speed",
                &self.podcast.speed,
                "a finite number from 0.5 through 3.0",
            ));
        }
        if !(1..=600).contains(&self.podcast.skip_backward_seconds) {
            return Err(invalid_value(
                "podcast.skip_backward_seconds",
                &self.podcast.skip_backward_seconds,
                "seconds from 1 through 600",
            ));
        }
        if !(1..=600).contains(&self.podcast.skip_forward_seconds) {
            return Err(invalid_value(
                "podcast.skip_forward_seconds",
                &self.podcast.skip_forward_seconds,
                "seconds from 1 through 600",
            ));
        }
        if !(0..=300).contains(&self.behavior.resolver_cache_seconds) {
            return Err(invalid_value(
                "behavior.resolver_cache_seconds",
                &self.behavior.resolver_cache_seconds,
                "seconds from 0 through 300",
            ));
        }
        if !(1..=15).contains(&self.artwork.max_fps) {
            return Err(invalid_value(
                "artwork.max_fps",
                &self.artwork.max_fps,
                "frames per second from 1 through 15",
            ));
        }
        if !(1..=30).contains(&self.visualizer.max_fps) {
            return Err(invalid_value(
                "visualizer.max_fps",
                &self.visualizer.max_fps,
                "frames per second from 1 through 30",
            ));
        }

        Ok(())
    }
}

fn invalid_value(
    field: &'static str,
    value: &impl ToString,
    expected: &'static str,
) -> ConfigError {
    ConfigError::InvalidValue {
        field,
        value: value.to_string(),
        expected,
    }
}
