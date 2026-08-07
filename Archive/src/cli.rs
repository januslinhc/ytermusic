use std::{
    io::{self, Write as _},
    path::PathBuf,
    process::ExitCode,
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use clap::{Parser, Subcommand};
use secrecy::SecretString;
use thiserror::Error;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt as _, util::SubscriberInitExt as _};

use crate::{
    auth::{
        AuthError, AuthService, AuthStatus, KeyringSecretVault, SecretVault, YtMusicAuthVerifier,
        YtMusicAuthenticatedProviderFactory,
    },
    config::Config,
    diagnostics::{DependencyChecker, DoctorReport, Platform},
    lyrics::{LrclibClient, LyricsSourceService},
    notifications::{NativeNotifier, NotificationArtworkCache, initialize_notification_service},
    platform::paths::AppPaths,
    player::{
        mpv::MpvBackend,
        supervisor::{PlayerSupervisor, TokioTickSource},
    },
    podcast_rankings::ApplePodcastRankingSource,
    process::{ExecutableLocator, ProcessRunner, SystemExecutableLocator, TokioProcessRunner},
    provider::{MusicProvider, YtMusicProvider},
    resolver::{SystemResolverClock, YtDlpResolver},
    runtime::{
        ArtworkRuntimeComponents, CrosstermTerminalControl, FifoStorage, HttpArtworkFetcher,
        Runtime, RuntimeAccountService, RuntimePlayer, RuntimeServices, RuntimeStorage,
        SharedMusicProvider, StartupFactory, SystemRuntimeDependencies, TuiEventSource,
        TuiRenderer, launch_application,
    },
    storage::SqliteStorage,
    ui::{
        animation::{
            AnimationFrameStore, AnimationWorker, FfmpegAnimationDecoder, TokioAnimationPacer,
        },
        artwork::{PRODUCTION_ARTWORK_CACHE_CAPACITY, PRODUCTION_ARTWORK_SIZE},
        spectrum::{FfmpegSpectrumDecoder, SpectrumFrameStore, SpectrumWorker, TokioSpectrumPacer},
        theme::{Theme, detected_color_capability},
    },
};

pub use crate::auth::Browser;

const NOTIFICATION_CACHE_STARTUP_TIMEOUT: Duration = Duration::from_millis(100);

#[derive(Debug, Parser)]
#[command(name = "ytermusic", about = "Music without leaving your terminal")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    Doctor,
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum AuthCommand {
    Import {
        #[arg(value_enum)]
        browser: Browser,
    },
    Status,
    Logout,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum AuthExecutionError {
    #[error(transparent)]
    Authentication(#[from] AuthError),
    #[error("could not write authentication result")]
    Output,
}

/// Executes one authentication command against injected boundaries.
///
/// # Errors
///
/// Returns an authentication error when the selected operation fails, or an
/// output error when its secret-free status line cannot be written.
pub async fn execute_auth(
    command: AuthCommand,
    service: &AuthService,
    output: &mut dyn io::Write,
) -> Result<(), AuthExecutionError> {
    let line = match command {
        AuthCommand::Import { browser } => {
            service.import_browser(browser).await?;
            "connected\n"
        }
        AuthCommand::Status => match service.status().await? {
            AuthStatus::Connected => "connected\n",
            AuthStatus::Expired => "expired\n",
            AuthStatus::Anonymous => "anonymous\n",
        },
        AuthCommand::Logout => {
            service.logout().await?;
            "logged out\n"
        }
    };
    output
        .write_all(line.as_bytes())
        .map_err(|_| AuthExecutionError::Output)
}

/// Runs dependency diagnostics with injected system boundaries.
///
/// # Errors
///
/// Returns an error when the report cannot be written.
pub async fn execute_doctor(
    locator: &dyn ExecutableLocator,
    runner: &dyn ProcessRunner,
    platform: Platform,
    output: &mut dyn io::Write,
) -> io::Result<u8> {
    let report = DependencyChecker::new(locator, runner, platform)
        .check()
        .await;
    output.write_all(report.render().as_bytes())?;
    Ok(report.exit_code())
}

struct ProductionStartup {
    vault: Arc<KeyringSecretVault>,
    runner: Arc<TokioProcessRunner>,
    locator: Arc<SystemExecutableLocator>,
}

impl ProductionStartup {
    fn new() -> Self {
        Self {
            vault: Arc::new(KeyringSecretVault::new()),
            runner: Arc::new(TokioProcessRunner),
            locator: Arc::new(SystemExecutableLocator),
        }
    }

    fn animation_worker(
        config: &Config,
        store: Arc<AnimationFrameStore>,
        ffmpeg: Option<PathBuf>,
    ) -> Option<AnimationWorker> {
        if !config.artwork.animated {
            return None;
        }
        ffmpeg.map(|ffmpeg| {
            AnimationWorker::spawn(
                Arc::new(FfmpegAnimationDecoder::new(ffmpeg)),
                Arc::new(TokioAnimationPacer),
                store,
                config.artwork.max_fps,
            )
        })
    }

    fn spectrum_worker(
        config: &Config,
        store: Arc<SpectrumFrameStore>,
        ffmpeg: Option<PathBuf>,
    ) -> Option<SpectrumWorker> {
        if !config.visualizer.enabled {
            return None;
        }
        ffmpeg.map(|ffmpeg| {
            SpectrumWorker::spawn(
                Arc::new(FfmpegSpectrumDecoder::new(ffmpeg)),
                Arc::new(TokioSpectrumPacer),
                store,
                config.visualizer.max_fps,
            )
        })
    }
}

#[async_trait]
impl StartupFactory for ProductionStartup {
    fn resolve_paths(&self) -> anyhow::Result<AppPaths> {
        let paths = AppPaths::discover()?;
        paths.ensure_directories()?;
        Ok(paths)
    }

    fn initialize_logging(&self, paths: &AppPaths) -> anyhow::Result<Box<dyn Send>> {
        let appender = tracing_appender::rolling::never(paths.log_directory(), "ytermusic.log");
        let (writer, guard) = tracing_appender::non_blocking(appender);
        tracing_subscriber::registry()
            .with(EnvFilter::new("ytermusic=info"))
            .with(
                tracing_subscriber::fmt::layer()
                    .with_ansi(false)
                    .with_writer(writer),
            )
            .try_init()
            .map_err(|error| anyhow::anyhow!("could not initialize file logging: {error}"))?;
        Ok(Box::new(guard))
    }

    fn load_config(&self, paths: &AppPaths) -> anyhow::Result<Config> {
        Ok(Config::load(paths.config_file())?)
    }

    async fn migrate_storage(&self, paths: &AppPaths) -> anyhow::Result<Arc<dyn RuntimeStorage>> {
        let database_file = paths.database_file().to_owned();
        tokio::task::spawn_blocking(move || {
            let storage = SqliteStorage::open(database_file)?;
            let fifo = FifoStorage::spawn(Box::new(storage))?;
            Ok::<Arc<dyn RuntimeStorage>, anyhow::Error>(Arc::new(fifo))
        })
        .await?
    }

    async fn load_credentials(&self) -> anyhow::Result<Option<SecretString>> {
        Ok(self.vault.load_cookie().await?)
    }

    async fn construct_provider(
        &self,
        credentials: Option<SecretString>,
    ) -> anyhow::Result<Arc<dyn MusicProvider>> {
        let provider = match credentials {
            Some(cookie) => YtMusicProvider::from_cookie(cookie).await?,
            None => YtMusicProvider::new_unauthenticated().await?,
        };
        Ok(Arc::new(provider))
    }

    async fn check_dependencies(&self) -> anyhow::Result<DoctorReport> {
        Ok(DependencyChecker::new(
            self.locator.as_ref(),
            self.runner.as_ref(),
            Platform::current(),
        )
        .check()
        .await)
    }

    #[allow(
        clippy::too_many_lines,
        reason = "production composition keeps optional media workers visibly independent"
    )]
    async fn enter_tui(
        &self,
        paths: AppPaths,
        config: Config,
        storage: Arc<dyn RuntimeStorage>,
        provider: Arc<dyn MusicProvider>,
        dependencies: DoctorReport,
    ) -> anyhow::Result<()> {
        let color_capability = detected_color_capability();
        let cache_root = paths.cache_directory().to_owned();
        let windows_aum_id = config.notifications.windows_aum_id.clone();
        let notifier = initialize_notification_service(
            config.notifications.enabled,
            NOTIFICATION_CACHE_STARTUP_TIMEOUT,
            move || NotificationArtworkCache::new(&cache_root),
            move |cache| NativeNotifier::from_prepared_cache(cache, windows_aum_id),
        )
        .await;
        if config.notifications.enabled && notifier.is_none() {
            tracing::warn!("native notification is unavailable");
        }
        let authentication = provider.authentication();
        let provider = Arc::new(SharedMusicProvider::new(provider));
        let account = Arc::new(RuntimeAccountService::new(
            Arc::new(AuthService::new(
                self.vault.clone(),
                self.runner.clone(),
                Arc::new(YtMusicAuthVerifier::new()),
            )),
            Arc::new(YtMusicAuthenticatedProviderFactory),
            provider.clone(),
        ));
        let playback_available = dependencies.playback_available();
        let lyrics = config.lyrics.enabled.then(|| {
            Arc::new(if let Ok(lrclib) = LrclibClient::new() {
                LyricsSourceService::new(
                    provider.clone(),
                    Arc::new(lrclib),
                    config.lyrics.external_sync,
                )
            } else {
                tracing::warn!(
                    "external lyrics source unavailable; provider lyrics remain available"
                );
                LyricsSourceService::provider_only(provider.clone())
            })
        });
        let dependency_service = Arc::new(SystemRuntimeDependencies::new(
            self.locator.clone(),
            self.runner.clone(),
            Platform::current(),
        ));
        let artwork = ArtworkRuntimeComponents::new(
            HttpArtworkFetcher::new()
                .map_err(|_| anyhow::anyhow!("could not initialize artwork HTTP client"))?,
            PRODUCTION_ARTWORK_CACHE_CAPACITY,
            PRODUCTION_ARTWORK_SIZE,
            color_capability,
        );
        let artwork_store = artwork.presentation_store();
        let animation_store = Arc::new(AnimationFrameStore::new());
        let spectrum_store = Arc::new(SpectrumFrameStore::new());
        let ffmpeg = if config.artwork.animated || config.visualizer.enabled {
            self.locator.find("ffmpeg")?
        } else {
            None
        };
        let animation =
            Self::animation_worker(&config, Arc::clone(&animation_store), ffmpeg.clone());
        let spectrum = Self::spectrum_worker(&config, Arc::clone(&spectrum_store), ffmpeg);
        let mut services = RuntimeServices::new(storage)
            .with_account_provider(provider, account)
            .with_dependencies(dependency_service)
            .with_artwork(artwork.runtime_artwork())
            .with_terminal(Arc::new(CrosstermTerminalControl))
            .with_startup_actions(authentication, dependencies);
        if let Some(notifier) = notifier {
            services = services.with_notifier(Arc::new(notifier));
        }
        if let Some(animation) = animation {
            services = services.with_animation(animation);
        }
        if let Some(spectrum) = spectrum {
            services = services.with_spectrum(spectrum);
        }

        if let Some(lyrics) = lyrics {
            services = services.with_lyrics(lyrics);
        }

        match ApplePodcastRankingSource::new() {
            Ok(source) => services = services.with_podcast_rankings(Arc::new(source)),
            Err(_) => {
                tracing::warn!("podcast ranking source unavailable; browsing remains available");
            }
        }

        if playback_available
            && let (Some(mpv), Some(yt_dlp)) =
                (self.locator.find("mpv")?, self.locator.find("yt-dlp")?)
        {
            match MpvBackend::spawn(mpv).await {
                Ok(backend) => {
                    let process_runner: Arc<dyn ProcessRunner> = self.runner.clone();
                    let resolver = Arc::new(YtDlpResolver::new(
                        yt_dlp,
                        process_runner,
                        Arc::new(SystemResolverClock),
                        Duration::from_secs(config.behavior.resolver_cache_seconds),
                    ));
                    let supervisor = PlayerSupervisor::spawn(
                        resolver,
                        Box::new(backend),
                        config.playback.clone(),
                        Box::new(TokioTickSource::new(Duration::from_millis(25))),
                    );
                    let (controller, actions) = supervisor.into_parts();
                    let player: Arc<dyn RuntimePlayer> = Arc::new(controller);
                    services = services
                        .with_player(player)
                        .with_player_actions(Box::new(actions));
                }
                Err(error) => {
                    tracing::error!(message = %error, "mpv startup failed; browsing remains available");
                }
            }
        }

        let renderer = TuiRenderer::new(Theme::for_capability(color_capability))?
            .with_artwork_store(artwork_store)
            .with_animation_store(animation_store)
            .with_spectrum_store(spectrum_store);
        Runtime::new(config, services)
            .run(TuiEventSource::new()?, renderer)
            .await?;
        Ok(())
    }
}

/// Parses the command-line arguments and runs the selected command.
///
/// # Errors
///
/// Returns an error if the selected command cannot be completed or its output
/// cannot be written.
pub async fn run() -> anyhow::Result<ExitCode> {
    let cli = Cli::parse();
    match cli.command {
        Some(Command::Doctor) => {
            let mut stdout = io::stdout().lock();
            let code = execute_doctor(
                &SystemExecutableLocator,
                &TokioProcessRunner,
                Platform::current(),
                &mut stdout,
            )
            .await?;
            stdout.flush()?;
            Ok(ExitCode::from(code))
        }
        Some(Command::Auth { command }) => {
            let service = AuthService::new(
                Arc::new(KeyringSecretVault::new()),
                Arc::new(TokioProcessRunner),
                Arc::new(YtMusicAuthVerifier::new()),
            );
            let mut stdout = io::stdout().lock();
            execute_auth(command, &service, &mut stdout).await?;
            stdout.flush()?;
            Ok(ExitCode::SUCCESS)
        }
        None => {
            launch_application(&ProductionStartup::new()).await?;
            Ok(ExitCode::SUCCESS)
        }
    }
}
