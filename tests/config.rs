use std::error::Error;

use ytermusic::app::AppState;
use ytermusic::config::{
    ArtworkConfig, BehaviorConfig, Config, ConfigError, LyricsConfig, NotificationsConfig,
    PlaybackConfig, PodcastConfig, VisualizerConfig, WindowsAumId,
};
use ytermusic::domain::RegionCode;

#[test]
fn region_code_normalizes_ascii_letters_to_uppercase() -> Result<(), Box<dyn Error>> {
    let region = RegionCode::parse("hk")?;

    assert_eq!(region.as_str(), "HK");
    Ok(())
}

#[test]
fn region_code_rejects_non_two_letter_values() {
    assert!(RegionCode::parse("hong-kong").is_err());
}

#[test]
fn config_defaults_include_visualizer_documented_baseline() {
    let config = Config::default();

    assert_eq!(config.region.as_str(), "ZZ");
    assert_eq!(config.playback.volume, 80);
    assert_eq!(config.podcast.speed.to_bits(), 1.0_f64.to_bits());
    assert!(config.lyrics.enabled);
    assert!(config.lyrics.external_sync);
    assert!(config.artwork.animated);
    assert_eq!(config.artwork.max_fps, 8);
    assert!(config.visualizer.enabled);
    assert_eq!(config.visualizer.max_fps, 15);
}

#[test]
fn config_defaults_include_notifications_and_seek_controls() {
    let config = Config::default();

    assert!(config.notifications.enabled);
    assert!(config.notifications.windows_aum_id.is_none());

    let state = AppState::new(config);
    assert_eq!(state.music_seek_seconds(), 10);
    assert_eq!(state.podcast_skip_backward_seconds(), 15);
    assert_eq!(state.podcast_skip_forward_seconds(), 30);
    assert!(state.notifications_enabled());
}

#[test]
fn windows_aum_id_is_optional_bounded_validated_and_redacted() -> Result<(), Box<dyn Error>> {
    let config: Config = toml::from_str(
        r#"
[notifications]
windows_aum_id = "ExampleCompany.Ytermusic"
"#,
    )?;
    let aum_id = config
        .notifications
        .windows_aum_id
        .as_ref()
        .ok_or("configured AUM ID missing")?;
    assert_eq!(aum_id.as_str(), "ExampleCompany.Ytermusic");
    assert!(!format!("{config:?} {aum_id:?}").contains("ExampleCompany.Ytermusic"));

    assert!(WindowsAumId::parse("has spaces.Ytermusic").is_err());
    assert!(WindowsAumId::parse(&"A".repeat(129)).is_err());
    assert!(WindowsAumId::parse("OnlyOneSection").is_err());
    Ok(())
}

#[test]
fn app_state_retains_non_default_notification_and_seek_controls() {
    let mut config = Config::default();
    config.podcast.skip_backward_seconds = 17;
    config.podcast.skip_forward_seconds = 43;
    config.notifications.enabled = false;

    let state = AppState::new(config);

    assert_eq!(state.podcast_skip_backward_seconds(), 17);
    assert_eq!(state.podcast_skip_forward_seconds(), 43);
    assert!(!state.notifications_enabled());
}

#[test]
fn minimal_config_deserializes_with_visualizer_section_defaults() -> Result<(), Box<dyn Error>> {
    let config: Config = toml::from_str("region = \"hk\"\n")?;

    assert_eq!(config.region.as_str(), "HK");
    assert_eq!(config.lyrics, LyricsConfig::default());
    assert_eq!(config.artwork, ArtworkConfig::default());
    assert_eq!(config.visualizer, VisualizerConfig::default());
    assert_eq!(config.notifications, NotificationsConfig::default());
    Ok(())
}

#[test]
fn visualizer_frame_rate_bounds_are_inclusive() {
    for max_fps in [1, 30] {
        let mut config = Config::default();
        config.visualizer.max_fps = max_fps;

        assert!(config.validate().is_ok());
    }
}

#[test]
fn visualizer_frame_rate_outside_bounds_is_a_typed_field_error() {
    for max_fps in [0, 31] {
        let mut config = Config::default();
        config.visualizer.max_fps = max_fps;

        assert!(matches!(
            config.validate(),
            Err(ConfigError::InvalidValue {
                field: "visualizer.max_fps",
                ..
            })
        ));
    }
}

#[test]
fn visualizer_enabled_is_copied_into_read_only_app_state() {
    let mut config = Config::default();
    config.visualizer.enabled = false;

    let state = AppState::new(config);

    assert!(!state.visualizer_enabled());
}

#[test]
fn artwork_frame_rate_bounds_are_inclusive() {
    for max_fps in [1, 15] {
        let mut config = Config::default();
        config.artwork.max_fps = max_fps;

        assert!(config.validate().is_ok());
    }
}

#[test]
fn artwork_frame_rate_outside_bounds_is_a_typed_field_error() {
    for max_fps in [0, 16] {
        let mut config = Config::default();
        config.artwork.max_fps = max_fps;

        assert!(matches!(
            config.validate(),
            Err(ConfigError::InvalidValue {
                field: "artwork.max_fps",
                ..
            })
        ));
    }
}

#[test]
fn example_config_includes_visualizer_defaults_and_validation() -> Result<(), Box<dyn Error>> {
    let config: Config = toml::from_str(include_str!("../config.example.toml"))?;

    assert_eq!(config, Config::default());
    assert!(config.validate().is_ok());
    Ok(())
}

#[test]
fn fade_in_above_ten_seconds_is_invalid() {
    let mut config = Config::default();
    config.playback.fade_in_ms = 60_000;

    assert!(config.validate().is_err());
}

#[test]
fn volume_bounds_are_inclusive() {
    for volume in [0, 100] {
        let mut config = Config::default();
        config.playback.volume = volume;

        assert!(config.validate().is_ok());
    }
}

#[test]
fn volume_above_one_hundred_is_invalid() {
    let mut config = Config::default();
    config.playback.volume = 101;

    assert!(config.validate().is_err());
}

#[test]
fn fade_in_bounds_are_inclusive() {
    for fade_in_ms in [0, 10_000] {
        let mut config = Config::default();
        config.playback.fade_in_ms = fade_in_ms;

        assert!(config.validate().is_ok());
    }
}

#[test]
fn fade_in_above_ten_thousand_milliseconds_is_invalid() {
    let mut config = Config::default();
    config.playback.fade_in_ms = 10_001;

    assert!(config.validate().is_err());
}

#[test]
fn fade_out_bounds_are_inclusive() {
    for fade_out_ms in [0, 10_000] {
        let mut config = Config::default();
        config.playback.fade_out_ms = fade_out_ms;

        assert!(config.validate().is_ok());
    }
}

#[test]
fn fade_out_above_ten_thousand_milliseconds_is_invalid() {
    let mut config = Config::default();
    config.playback.fade_out_ms = 10_001;

    assert!(config.validate().is_err());
}

#[test]
fn podcast_speed_bounds_are_inclusive() {
    for speed in [0.5, 3.0] {
        let mut config = Config::default();
        config.podcast.speed = speed;

        assert!(config.validate().is_ok());
    }
}

#[test]
fn podcast_speed_below_half_is_invalid() {
    let mut config = Config::default();
    config.podcast.speed = 0.49;

    assert!(config.validate().is_err());
}

#[test]
fn podcast_speed_above_three_is_invalid() {
    let mut config = Config::default();
    config.podcast.speed = 3.01;

    assert!(config.validate().is_err());
}

#[test]
fn non_finite_podcast_speed_is_invalid() {
    for speed in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let mut config = Config::default();
        config.podcast.speed = speed;

        assert!(config.validate().is_err());
    }
}

#[test]
fn podcast_skip_backward_bounds_are_inclusive() {
    for seconds in [1, 600] {
        let mut config = Config::default();
        config.podcast.skip_backward_seconds = seconds;

        assert!(config.validate().is_ok());
    }
}

#[test]
fn podcast_skip_backward_outside_bounds_is_invalid() {
    for seconds in [0, 601] {
        let mut config = Config::default();
        config.podcast.skip_backward_seconds = seconds;

        assert!(config.validate().is_err());
    }
}

#[test]
fn podcast_skip_forward_bounds_are_inclusive() {
    for seconds in [1, 600] {
        let mut config = Config::default();
        config.podcast.skip_forward_seconds = seconds;

        assert!(config.validate().is_ok());
    }
}

#[test]
fn podcast_skip_forward_outside_bounds_is_invalid() {
    for seconds in [0, 601] {
        let mut config = Config::default();
        config.podcast.skip_forward_seconds = seconds;

        assert!(config.validate().is_err());
    }
}

#[test]
fn resolver_cache_bounds_are_inclusive() {
    for seconds in [0, 300] {
        let mut config = Config::default();
        config.behavior.resolver_cache_seconds = seconds;

        assert!(config.validate().is_ok());
    }
}

#[test]
fn resolver_cache_above_five_minutes_is_invalid() {
    let mut config = Config::default();
    config.behavior.resolver_cache_seconds = 301;

    assert!(config.validate().is_err());
}

#[test]
fn loading_a_missing_file_returns_defaults() -> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("missing.toml");

    assert_eq!(Config::load(&path)?, Config::default());
    Ok(())
}

#[test]
fn visualizer_save_and_load_round_trip_and_create_parent_directories() -> Result<(), Box<dyn Error>>
{
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("nested").join("config.toml");
    let defaults = Config::default();
    let expected = Config {
        region: RegionCode::parse("hk")?,
        playback: PlaybackConfig {
            volume: 42,
            ..defaults.playback
        },
        podcast: PodcastConfig {
            speed: 1.25,
            ..defaults.podcast
        },
        behavior: BehaviorConfig {
            resume_session: false,
            ..defaults.behavior
        },
        lyrics: LyricsConfig {
            external_sync: false,
            ..defaults.lyrics
        },
        artwork: ArtworkConfig {
            animated: false,
            ..defaults.artwork
        },
        visualizer: VisualizerConfig {
            enabled: false,
            max_fps: 20,
        },
        notifications: NotificationsConfig {
            enabled: false,
            ..NotificationsConfig::default()
        },
    };

    expected.save(&path)?;

    let loaded = Config::load(&path)?;
    assert!(!loaded.notifications.enabled);
    assert_eq!(loaded, expected);
    Ok(())
}

#[test]
fn malformed_toml_returns_a_parse_error() -> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("config.toml");
    std::fs::write(&path, "[playback\nvolume = 80")?;

    let Err(error) = Config::load(&path) else {
        panic!("malformed TOML must not load");
    };

    assert!(matches!(error, ConfigError::Parse { .. }));
    Ok(())
}

#[test]
fn parsed_invalid_values_return_a_validation_error() -> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("config.toml");
    std::fs::write(&path, "[playback]\nvolume = 101\n")?;

    let Err(error) = Config::load(&path) else {
        panic!("invalid config values must not load");
    };

    assert!(matches!(
        error,
        ConfigError::InvalidValue {
            field: "playback.volume",
            ..
        }
    ));
    Ok(())
}

#[test]
fn invalid_region_deserialization_returns_a_parse_error() -> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("config.toml");
    std::fs::write(&path, "region = \"hong-kong\"\n")?;

    let Err(error) = Config::load(&path) else {
        panic!("invalid region must not deserialize");
    };

    assert!(matches!(error, ConfigError::Parse { .. }));
    Ok(())
}

#[test]
fn non_not_found_io_errors_are_typed() -> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;

    let Err(error) = Config::load(directory.path()) else {
        panic!("a directory is not a config file");
    };

    assert!(matches!(error, ConfigError::Io { .. }));
    Ok(())
}

#[test]
fn save_validates_before_writing() -> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("config.toml");
    let mut config = Config::default();
    config.playback.volume = 101;

    let Err(error) = config.save(&path) else {
        panic!("invalid config must not be written");
    };

    assert!(matches!(
        error,
        ConfigError::InvalidValue {
            field: "playback.volume",
            ..
        }
    ));
    assert!(!path.exists());
    Ok(())
}
