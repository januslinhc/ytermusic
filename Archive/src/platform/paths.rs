use std::{
    fs, io,
    path::{Path, PathBuf},
};

use directories::ProjectDirs;

/// Platform-native locations for every durable and cached runtime artifact.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppPaths {
    config_file: PathBuf,
    database_file: PathBuf,
    log_directory: PathBuf,
    cache_directory: PathBuf,
}

impl AppPaths {
    /// Resolves the operating system's conventional per-user directories.
    ///
    /// # Errors
    ///
    /// Returns an unsupported error when the platform cannot identify a home
    /// directory for the current user.
    pub fn discover() -> io::Result<Self> {
        let project = ProjectDirs::from("dev", "ytermusic", "ytermusic").ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "platform application directories are unavailable",
            )
        })?;
        Ok(Self::from_roots(
            project.config_dir(),
            project.data_dir(),
            project.cache_dir(),
        ))
    }

    #[must_use]
    pub fn from_roots(
        config_directory: impl AsRef<Path>,
        data_directory: impl AsRef<Path>,
        cache_directory: impl AsRef<Path>,
    ) -> Self {
        Self {
            config_file: config_directory.as_ref().join("config.toml"),
            database_file: data_directory.as_ref().join("ytermusic.db"),
            log_directory: data_directory.as_ref().join("logs"),
            cache_directory: cache_directory.as_ref().to_owned(),
        }
    }

    /// Creates every directory needed before logging, migration, or caching.
    ///
    /// # Errors
    ///
    /// Returns the first filesystem error encountered.
    pub fn ensure_directories(&self) -> io::Result<()> {
        if let Some(config_directory) = self.config_file.parent() {
            fs::create_dir_all(config_directory)?;
        }
        if let Some(data_directory) = self.database_file.parent() {
            fs::create_dir_all(data_directory)?;
        }
        fs::create_dir_all(&self.log_directory)?;
        fs::create_dir_all(&self.cache_directory)
    }

    #[must_use]
    pub fn config_file(&self) -> &Path {
        &self.config_file
    }

    #[must_use]
    pub fn database_file(&self) -> &Path {
        &self.database_file
    }

    #[must_use]
    pub fn log_directory(&self) -> &Path {
        &self.log_directory
    }

    #[must_use]
    pub fn log_file(&self) -> PathBuf {
        self.log_directory.join("ytermusic.log")
    }

    #[must_use]
    pub fn cache_directory(&self) -> &Path {
        &self.cache_directory
    }
}
