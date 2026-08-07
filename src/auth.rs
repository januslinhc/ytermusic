use std::{
    ffi::OsString, fmt, io::Write as _, panic::AssertUnwindSafe, path::Path, str::FromStr,
    sync::Arc, time::Duration,
};

use async_trait::async_trait;
use clap::ValueEnum;
use futures::FutureExt as _;
use secrecy::{ExposeSecret as _, SecretSlice, SecretString, zeroize::Zeroize as _};
use tempfile::TempPath;
use thiserror::Error;
use tokio::io::AsyncReadExt as _;

use crate::{
    process::{CommandSpec, ProcessRunner},
    provider::{
        LibrarySection, MusicProvider, ProviderError, ProviderErrorKind, ProviderResult,
        YtMusicProvider,
    },
};

pub const KEYRING_SERVICE: &str = "ytermusic";
pub const KEYRING_ACCOUNT: &str = "youtube-music-cookie";
pub const MAX_COOKIE_JAR_BYTES: usize = 2 * 1_024 * 1_024;
pub const MAX_COOKIE_LINE_BYTES: usize = 8 * 1_024;
pub const MAX_COOKIE_VALUE_BYTES: usize = 4 * 1_024;
pub const MAX_COOKIE_HEADER_BYTES: usize = 16 * 1_024;

const YT_DLP: &str = "yt-dlp";
const YOUTUBE_MUSIC_HOST: &str = "music.youtube.com";
const NETSCAPE_COOKIE_HEADER: &[u8] = b"# Netscape HTTP Cookie File\n";
const CLEANUP_ATTEMPTS: usize = 5;
const CLEANUP_INITIAL_DELAY: Duration = Duration::from_millis(10);

/// Browser cookie stores accepted by yt-dlp.
///
/// A closed enum prevents browser names from becoming command-line options.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, ValueEnum)]
pub enum Browser {
    Brave,
    Chrome,
    Chromium,
    Edge,
    Firefox,
    Opera,
    Safari,
    Vivaldi,
}

impl Browser {
    pub const ALL: [Self; 8] = [
        Self::Brave,
        Self::Chrome,
        Self::Chromium,
        Self::Edge,
        Self::Firefox,
        Self::Opera,
        Self::Safari,
        Self::Vivaldi,
    ];

    #[must_use]
    pub const fn as_ytdlp_name(self) -> &'static str {
        match self {
            Self::Brave => "brave",
            Self::Chrome => "chrome",
            Self::Chromium => "chromium",
            Self::Edge => "edge",
            Self::Firefox => "firefox",
            Self::Opera => "opera",
            Self::Safari => "safari",
            Self::Vivaldi => "vivaldi",
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Brave => "Brave",
            Self::Chrome => "Chrome",
            Self::Chromium => "Chromium",
            Self::Edge => "Edge",
            Self::Firefox => "Firefox",
            Self::Opera => "Opera",
            Self::Safari => "Safari",
            Self::Vivaldi => "Vivaldi",
        }
    }
}

impl FromStr for Browser {
    type Err = UnsupportedBrowser;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "brave" => Ok(Self::Brave),
            "chrome" => Ok(Self::Chrome),
            "chromium" => Ok(Self::Chromium),
            "edge" => Ok(Self::Edge),
            "firefox" => Ok(Self::Firefox),
            "opera" => Ok(Self::Opera),
            "safari" => Ok(Self::Safari),
            "vivaldi" => Ok(Self::Vivaldi),
            _ => Err(UnsupportedBrowser),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("unsupported browser")]
pub struct UnsupportedBrowser;

/// Coarse authentication failures that never retain underlying secret-bearing data.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum AuthError {
    #[error("secure credential storage is unavailable")]
    VaultUnavailable,
    #[error("browser cookie export is unavailable")]
    ExportUnavailable,
    #[error("browser cookie export failed")]
    ExportFailed,
    #[error("browser cookie export is invalid")]
    InvalidCookieJar,
    #[error("browser cookie export has no usable authentication cookies")]
    NoAuthenticationCookies,
    #[error("browser session was rejected")]
    VerificationRejected,
    #[error("browser session could not be verified")]
    VerificationUnavailable,
    #[error("temporary browser cookie export could not be removed")]
    CleanupFailed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuthStatus {
    Connected,
    Expired,
    Anonymous,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Verification {
    Connected,
    Expired,
}

#[async_trait]
pub trait SecretVault: Send + Sync {
    /// Loads Ytermusic's cookie header, if present.
    ///
    /// # Errors
    ///
    /// Returns an error when the secure store cannot be accessed.
    async fn load_cookie(&self) -> Result<Option<SecretString>, AuthError>;

    /// Stores Ytermusic's cookie header.
    ///
    /// # Errors
    ///
    /// Returns an error when the secure store cannot be accessed.
    async fn store_cookie(&self, cookie: SecretString) -> Result<(), AuthError>;

    /// Deletes Ytermusic's cookie header.
    ///
    /// # Errors
    ///
    /// Returns an error when the secure store cannot be accessed.
    async fn delete_cookie(&self) -> Result<(), AuthError>;
}

/// Synchronous secure-store operations wrapped by [`KeyringSecretVault`].
///
/// This narrow seam keeps platform keyring calls testable without touching a
/// user's real credential store.
pub trait BlockingSecretStore: Send + Sync {
    /// Loads Ytermusic's cookie header, if present.
    ///
    /// # Errors
    ///
    /// Returns an error when the secure store cannot be accessed.
    fn load_cookie(&self) -> Result<Option<SecretString>, AuthError>;

    /// Stores Ytermusic's cookie header.
    ///
    /// # Errors
    ///
    /// Returns an error when the secure store cannot be accessed.
    fn store_cookie(&self, cookie: SecretString) -> Result<(), AuthError>;

    /// Deletes Ytermusic's cookie header.
    ///
    /// # Errors
    ///
    /// Returns an error when the secure store cannot be accessed.
    fn delete_cookie(&self) -> Result<(), AuthError>;
}

#[derive(Clone)]
pub struct KeyringSecretVault {
    store: Arc<dyn BlockingSecretStore>,
}

impl KeyringSecretVault {
    #[must_use]
    pub fn new() -> Self {
        Self {
            store: Arc::new(OsKeyringStore),
        }
    }

    #[doc(hidden)]
    #[must_use]
    pub fn with_store<S>(store: Arc<S>) -> Self
    where
        S: BlockingSecretStore + 'static,
    {
        Self { store }
    }
}

impl Default for KeyringSecretVault {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for KeyringSecretVault {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("KeyringSecretVault")
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl SecretVault for KeyringSecretVault {
    async fn load_cookie(&self) -> Result<Option<SecretString>, AuthError> {
        let store = Arc::clone(&self.store);
        tokio::task::spawn_blocking(move || store.load_cookie())
            .await
            .unwrap_or(Err(AuthError::VaultUnavailable))
    }

    async fn store_cookie(&self, cookie: SecretString) -> Result<(), AuthError> {
        let store = Arc::clone(&self.store);
        tokio::task::spawn_blocking(move || store.store_cookie(cookie))
            .await
            .unwrap_or(Err(AuthError::VaultUnavailable))
    }

    async fn delete_cookie(&self) -> Result<(), AuthError> {
        let store = Arc::clone(&self.store);
        tokio::task::spawn_blocking(move || store.delete_cookie())
            .await
            .unwrap_or(Err(AuthError::VaultUnavailable))
    }
}

#[derive(Clone, Copy, Debug)]
struct OsKeyringStore;

impl OsKeyringStore {
    fn entry() -> Result<keyring::Entry, AuthError> {
        keyring::Entry::new(KEYRING_SERVICE, KEYRING_ACCOUNT)
            .map_err(|_| AuthError::VaultUnavailable)
    }
}

impl BlockingSecretStore for OsKeyringStore {
    fn load_cookie(&self) -> Result<Option<SecretString>, AuthError> {
        match Self::entry()?.get_password() {
            Ok(cookie) => Ok(Some(SecretString::from(cookie))),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(_) => Err(AuthError::VaultUnavailable),
        }
    }

    fn store_cookie(&self, cookie: SecretString) -> Result<(), AuthError> {
        Self::entry()?
            .set_password(cookie.expose_secret())
            .map_err(|_| AuthError::VaultUnavailable)
    }

    fn delete_cookie(&self) -> Result<(), AuthError> {
        match Self::entry()?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(_) => Err(AuthError::VaultUnavailable),
        }
    }
}

#[async_trait]
pub trait AuthVerifier: Send + Sync {
    /// Checks a secret cookie header against an authenticated canary.
    ///
    /// # Errors
    ///
    /// Returns an error when the provider cannot complete the canary.
    async fn verify(&self, cookie: &SecretString) -> Result<Verification, AuthError>;
}

#[async_trait]
pub trait AuthenticatedProviderFactory: Send + Sync {
    /// Creates a provider from one in-memory cookie header.
    ///
    /// # Errors
    ///
    /// Returns a typed provider error when authenticated session construction fails.
    async fn create(&self, cookie: &SecretString) -> ProviderResult<Arc<dyn MusicProvider>>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct YtMusicAuthenticatedProviderFactory;

#[async_trait]
impl AuthenticatedProviderFactory for YtMusicAuthenticatedProviderFactory {
    async fn create(&self, cookie: &SecretString) -> ProviderResult<Arc<dyn MusicProvider>> {
        YtMusicProvider::from_cookie(cookie.clone())
            .await
            .map(|provider| Arc::new(provider) as Arc<dyn MusicProvider>)
    }
}

#[derive(Clone)]
pub struct YtMusicAuthVerifier {
    factory: Arc<dyn AuthenticatedProviderFactory>,
}

impl YtMusicAuthVerifier {
    #[must_use]
    pub fn new() -> Self {
        Self {
            factory: Arc::new(YtMusicAuthenticatedProviderFactory),
        }
    }

    #[doc(hidden)]
    #[must_use]
    pub fn with_factory(factory: Arc<dyn AuthenticatedProviderFactory>) -> Self {
        Self { factory }
    }
}

impl Default for YtMusicAuthVerifier {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for YtMusicAuthVerifier {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("YtMusicAuthVerifier")
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl AuthVerifier for YtMusicAuthVerifier {
    async fn verify(&self, cookie: &SecretString) -> Result<Verification, AuthError> {
        let provider = match self.factory.create(cookie).await {
            Ok(provider) => provider,
            Err(error) => return classify_verification_error(error),
        };
        match provider.library(LibrarySection::Playlists).await {
            Ok(_) => Ok(Verification::Connected),
            Err(error) => classify_verification_error(error),
        }
    }
}

fn classify_verification_error(error: ProviderError) -> Result<Verification, AuthError> {
    if error.kind == ProviderErrorKind::AuthenticationRequired {
        Ok(Verification::Expired)
    } else {
        Err(AuthError::VerificationUnavailable)
    }
}

#[derive(Clone)]
pub struct AuthService {
    vault: Arc<dyn SecretVault>,
    runner: Arc<dyn ProcessRunner>,
    verifier: Arc<dyn AuthVerifier>,
}

impl AuthService {
    #[must_use]
    pub fn new(
        vault: Arc<dyn SecretVault>,
        runner: Arc<dyn ProcessRunner>,
        verifier: Arc<dyn AuthVerifier>,
    ) -> Self {
        Self {
            vault,
            runner,
            verifier,
        }
    }

    /// Imports and verifies one browser session before replacing the vault entry.
    ///
    /// # Errors
    ///
    /// Returns a coarse export, parsing, verification, cleanup, or vault error.
    pub async fn import_browser(&self, browser: Browser) -> Result<(), AuthError> {
        let cookie = self.prepare_browser_cookie(browser).await?;
        self.commit_browser_cookie(cookie).await
    }

    pub(crate) async fn prepare_browser_cookie(
        &self,
        browser: Browser,
    ) -> Result<SecretString, AuthError> {
        let cookie = {
            let exported = export_browser_cookie_jar(Arc::clone(&self.runner), browser).await?;
            let parsed = parse_netscape_cookie_jar(exported.contents.expose_secret());
            match (exported.process_succeeded, parsed) {
                (_, Ok(cookie)) => cookie,
                (false, Err(_)) => return Err(AuthError::ExportFailed),
                (true, Err(error)) => return Err(error),
            }
        };

        match self.verifier.verify(&cookie).await? {
            Verification::Connected => Ok(cookie),
            Verification::Expired => Err(AuthError::VerificationRejected),
        }
    }

    pub(crate) async fn commit_browser_cookie(
        &self,
        cookie: SecretString,
    ) -> Result<(), AuthError> {
        self.vault.store_cookie(cookie).await
    }

    /// Reports whether no credential exists, the credential works, or it was rejected.
    ///
    /// # Errors
    ///
    /// Returns an error when the vault or verification provider is unavailable.
    pub async fn status(&self) -> Result<AuthStatus, AuthError> {
        let Some(cookie) = self.vault.load_cookie().await? else {
            return Ok(AuthStatus::Anonymous);
        };
        match self.verifier.verify(&cookie).await? {
            Verification::Connected => Ok(AuthStatus::Connected),
            Verification::Expired => Ok(AuthStatus::Expired),
        }
    }

    /// Deletes Ytermusic's own credential-store entry.
    ///
    /// # Errors
    ///
    /// Returns an error when the OS credential store is unavailable.
    pub async fn logout(&self) -> Result<(), AuthError> {
        self.vault.delete_cookie().await
    }
}

impl fmt::Debug for AuthService {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AuthService")
            .finish_non_exhaustive()
    }
}

/// Converts a Netscape cookie jar into a bounded in-memory Cookie header.
///
/// # Errors
///
/// Returns an error for an oversized or malformed secret-bearing field, or
/// when no usable `YouTube` Music authentication cookie remains.
pub fn parse_netscape_cookie_jar(jar: &[u8]) -> Result<SecretString, AuthError> {
    parse_netscape_cookie_jar_at(jar, time::OffsetDateTime::now_utc().unix_timestamp())
}

/// Deterministic variant of [`parse_netscape_cookie_jar`] for an injected Unix timestamp.
///
/// # Errors
///
/// Returns an error for an oversized or malformed secret-bearing field, or
/// when no usable `YouTube` Music authentication cookie remains.
pub fn parse_netscape_cookie_jar_at(jar: &[u8], now_unix: i64) -> Result<SecretString, AuthError> {
    if jar.len() > MAX_COOKIE_JAR_BYTES {
        return Err(AuthError::InvalidCookieJar);
    }
    let jar = std::str::from_utf8(jar).map_err(|_| AuthError::InvalidCookieJar)?;
    let mut selected = AUTH_COOKIE_NAMES
        .iter()
        .map(|_| None)
        .collect::<Vec<Option<CookieCandidate>>>();

    for raw_line in jar.split('\n') {
        if raw_line.len() > MAX_COOKIE_LINE_BYTES {
            return Err(AuthError::InvalidCookieJar);
        }
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        if line.trim().is_empty() {
            continue;
        }
        let line = if let Some(cookie_line) = line.strip_prefix("#HttpOnly_") {
            cookie_line
        } else if line.starts_with('#') {
            continue;
        } else {
            line
        };

        let Some(fields) = CookieFields::parse(line) else {
            continue;
        };
        let Some(domain_rank) = youtube_music_domain_rank(fields.domain, fields.include_subdomains)
        else {
            continue;
        };
        if fields.path != "/" || fields.is_expired(now_unix) {
            continue;
        }
        if !valid_cookie_name(fields.name) {
            return Err(AuthError::InvalidCookieJar);
        }
        let Some(index) = AUTH_COOKIE_NAMES
            .iter()
            .position(|candidate| *candidate == fields.name)
        else {
            continue;
        };
        if fields.value.len() > MAX_COOKIE_VALUE_BYTES || !valid_cookie_value(fields.value) {
            return Err(AuthError::InvalidCookieJar);
        }

        let replace = selected[index]
            .as_ref()
            .is_none_or(|current| domain_rank > current.domain_rank);
        if replace {
            selected[index] = Some(CookieCandidate {
                domain_rank,
                value: SecretString::from(fields.value),
            });
        }
    }

    let header_bytes = AUTH_COOKIE_NAMES
        .iter()
        .zip(selected.iter())
        .filter_map(|(name, candidate)| candidate.as_ref().map(|value| (name, value)))
        .try_fold(0_usize, |length, (name, candidate)| {
            let separator = usize::from(length != 0) * 2;
            length
                .checked_add(separator)
                .and_then(|length| length.checked_add(name.len()))
                .and_then(|length| length.checked_add(1))
                .and_then(|length| length.checked_add(candidate.value.expose_secret().len()))
        })
        .ok_or(AuthError::InvalidCookieJar)?;
    if header_bytes == 0 {
        return Err(AuthError::NoAuthenticationCookies);
    }
    if header_bytes > MAX_COOKIE_HEADER_BYTES {
        return Err(AuthError::InvalidCookieJar);
    }

    let mut header = String::with_capacity(header_bytes);
    for (name, candidate) in AUTH_COOKIE_NAMES.iter().zip(selected.iter()) {
        let Some(candidate) = candidate else {
            continue;
        };
        if !header.is_empty() {
            header.push_str("; ");
        }
        header.push_str(name);
        header.push('=');
        header.push_str(candidate.value.expose_secret());
    }
    Ok(SecretString::from(header))
}

// Only cookies used to establish or maintain a YouTube browser session are
// retained. Preference, analytics, experiment, and tracking cookies are
// intentionally excluded.
const AUTH_COOKIE_NAMES: [&str; 9] = [
    "SID",
    "HSID",
    "SSID",
    "APISID",
    "SAPISID",
    "__Secure-1PSID",
    "__Secure-3PSID",
    "__Secure-1PAPISID",
    "__Secure-3PAPISID",
];

struct CookieCandidate {
    domain_rank: u8,
    value: SecretString,
}

struct CookieFields<'a> {
    domain: &'a str,
    include_subdomains: bool,
    path: &'a str,
    expiry: i64,
    name: &'a str,
    value: &'a str,
}

impl<'a> CookieFields<'a> {
    fn parse(line: &'a str) -> Option<Self> {
        let mut fields = line.split('\t');
        let domain = fields.next()?;
        let include_subdomains = parse_netscape_bool(fields.next()?)?;
        let path = fields.next()?;
        let _secure = parse_netscape_bool(fields.next()?)?;
        let expiry = fields.next()?.parse::<i64>().ok()?;
        let name = fields.next()?;
        let value = fields.next()?;
        if fields.next().is_some() {
            return None;
        }
        Some(Self {
            domain,
            include_subdomains,
            path,
            expiry,
            name,
            value,
        })
    }

    const fn is_expired(&self, now_unix: i64) -> bool {
        self.expiry != 0 && self.expiry <= now_unix
    }
}

const fn parse_netscape_bool(value: &str) -> Option<bool> {
    match value.as_bytes() {
        b"TRUE" => Some(true),
        b"FALSE" => Some(false),
        _ => None,
    }
}

fn youtube_music_domain_rank(domain: &str, include_subdomains: bool) -> Option<u8> {
    if domain.len() > 255
        || domain
            .bytes()
            .any(|byte| !(byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-')))
    {
        return None;
    }
    let normalized = domain.strip_prefix('.').unwrap_or(domain);
    if normalized.eq_ignore_ascii_case(YOUTUBE_MUSIC_HOST) {
        return Some(if include_subdomains { 2 } else { 3 });
    }
    if include_subdomains && normalized.eq_ignore_ascii_case("youtube.com") {
        return Some(1);
    }
    None
}

fn valid_cookie_name(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|byte| {
            byte.is_ascii()
                && !byte.is_ascii_control()
                && !matches!(
                    byte,
                    b' ' | b'('
                        | b')'
                        | b'<'
                        | b'>'
                        | b'@'
                        | b','
                        | b';'
                        | b':'
                        | b'\\'
                        | b'"'
                        | b'/'
                        | b'['
                        | b']'
                        | b'?'
                        | b'='
                        | b'{'
                        | b'}'
                )
        })
}

fn valid_cookie_value(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            matches!(
                byte,
                0x21 | 0x23..=0x2B | 0x2D..=0x3A | 0x3C..=0x5B | 0x5D..=0x7E
            )
        })
}

async fn export_browser_cookie_jar(
    runner: Arc<dyn ProcessRunner>,
    browser: Browser,
) -> Result<ExportedCookieJar, AuthError> {
    // Dropping this JoinHandle detaches the owned lifecycle, so caller
    // cancellation cannot abandon a live child or its temporary file.
    tokio::spawn(run_export_lifecycle(runner, browser))
        .await
        .map_err(|_| AuthError::ExportUnavailable)?
}

#[cfg(test)]
async fn export_browser_cookie_jar_with_completion(
    runner: Arc<dyn ProcessRunner>,
    browser: Browser,
    cleanup_completion: tokio::sync::oneshot::Sender<()>,
) -> Result<ExportedCookieJar, AuthError> {
    tokio::spawn(async move {
        let result = run_export_lifecycle(runner, browser).await;
        let _ = cleanup_completion.send(());
        result
    })
    .await
    .map_err(|_| AuthError::ExportUnavailable)?
}

async fn run_export_lifecycle(
    runner: Arc<dyn ProcessRunner>,
    browser: Browser,
) -> Result<ExportedCookieJar, AuthError> {
    let export = create_private_export()?;
    let result = AssertUnwindSafe(run_export_process(
        runner.as_ref(),
        browser,
        export.as_ref(),
    ))
    .catch_unwind()
    .await
    .unwrap_or(Err(AuthError::ExportUnavailable));
    let cleanup = cleanup_private_export(export).await;
    match (result, cleanup) {
        (_, Err(error)) => Err(error),
        (result, Ok(())) => result,
    }
}

async fn run_export_process(
    runner: &dyn ProcessRunner,
    browser: Browser,
    export: &Path,
) -> Result<ExportedCookieJar, AuthError> {
    let spec = CommandSpec::new(
        YT_DLP,
        [
            OsString::from("--cookies-from-browser"),
            OsString::from(browser.as_ytdlp_name()),
            OsString::from("--cookies"),
            export.as_os_str().to_owned(),
        ],
    );
    let mut output = runner
        .output(spec)
        .await
        .map_err(|_| AuthError::ExportUnavailable)?;
    let succeeded = output.status.success();
    output.stdout.zeroize();
    output.stderr.zeroize();
    let contents = read_bounded_export(export).await?;
    Ok(ExportedCookieJar {
        contents,
        process_succeeded: succeeded,
    })
}

struct ExportedCookieJar {
    contents: SecretSlice<u8>,
    process_succeeded: bool,
}

fn create_private_export() -> Result<TempPath, AuthError> {
    let mut file = tempfile::Builder::new()
        .prefix("ytermusic-cookie-")
        .tempfile()
        .map_err(|_| AuthError::ExportUnavailable)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        file.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|_| AuthError::ExportUnavailable)?;
    }
    file.write_all(NETSCAPE_COOKIE_HEADER)
        .and_then(|()| file.flush())
        .map_err(|_| AuthError::ExportUnavailable)?;
    Ok(file.into_temp_path())
}

async fn cleanup_private_export(export: TempPath) -> Result<(), AuthError> {
    let path = export.to_path_buf();
    let mut delay = CLEANUP_INITIAL_DELAY;
    // Windows can report a short-lived sharing violation after process reap.
    // Keep the TempPath guard alive while bounded asynchronous retries run.
    for attempt in 0..CLEANUP_ATTEMPTS {
        match tokio::fs::remove_file(&path).await {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(_) if attempt + 1 == CLEANUP_ATTEMPTS => {
                return Err(AuthError::CleanupFailed);
            }
            Err(_) => {
                tokio::time::sleep(delay).await;
                delay = delay.saturating_mul(2);
            }
        }
    }
    Err(AuthError::CleanupFailed)
}

async fn read_bounded_export(path: &Path) -> Result<SecretSlice<u8>, AuthError> {
    let file = tokio::fs::File::open(path)
        .await
        .map_err(|_| AuthError::ExportFailed)?;
    let limit = u64::try_from(MAX_COOKIE_JAR_BYTES)
        .map_err(|_| AuthError::ExportFailed)?
        .saturating_add(1);
    let mut reader = file.take(limit);
    let mut contents = ZeroizingBytes::default();
    reader
        .read_to_end(&mut contents.0)
        .await
        .map_err(|_| AuthError::ExportFailed)?;
    if contents.0.len() > MAX_COOKIE_JAR_BYTES {
        return Err(AuthError::InvalidCookieJar);
    }
    Ok(contents.into_secret())
}

#[derive(Default)]
struct ZeroizingBytes(Vec<u8>);

impl ZeroizingBytes {
    fn into_secret(mut self) -> SecretSlice<u8> {
        SecretSlice::from(std::mem::take(&mut self.0))
    }
}

impl Drop for ZeroizingBytes {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

#[cfg(test)]
mod export_lifecycle_tests {
    #![allow(
        clippy::expect_used,
        reason = "the unit-test fixture must fail immediately when setup is invalid"
    )]

    use std::{
        fs::{self, OpenOptions},
        io::Write as _,
        path::PathBuf,
        process::ExitStatus,
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        },
        time::Duration,
    };

    use async_trait::async_trait;
    use tokio::sync::{Notify, oneshot};

    use super::{Browser, NETSCAPE_COOKIE_HEADER, export_browser_cookie_jar_with_completion};
    use crate::process::{CommandSpec, ProcessError, ProcessOutput, ProcessRunner};

    const TEST_TIMEOUT: Duration = Duration::from_secs(1);
    const VALID_EXPORT: &[u8] = b"# Netscape HTTP Cookie File\n\
        .youtube.com\tTRUE\t/\tTRUE\t4102444800\tSAPISID\timported\n";

    #[cfg(unix)]
    fn exit_status(code: i32) -> ExitStatus {
        use std::os::unix::process::ExitStatusExt as _;

        ExitStatus::from_raw(code << 8)
    }

    #[cfg(windows)]
    fn exit_status(code: i32) -> ExitStatus {
        use std::os::windows::process::ExitStatusExt as _;

        #[expect(clippy::cast_sign_loss, reason = "test exit codes are non-negative")]
        ExitStatus::from_raw(code as u32)
    }

    struct HeldExportRunner {
        started: Notify,
        release: Notify,
        completed: AtomicBool,
        path: Mutex<Option<PathBuf>>,
    }

    impl HeldExportRunner {
        fn new() -> Self {
            Self {
                started: Notify::new(),
                release: Notify::new(),
                completed: AtomicBool::new(false),
                path: Mutex::new(None),
            }
        }

        fn path(&self) -> Option<PathBuf> {
            self.path
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }
    }

    #[async_trait]
    impl ProcessRunner for HeldExportRunner {
        async fn output(&self, spec: CommandSpec) -> Result<ProcessOutput, ProcessError> {
            let path = PathBuf::from(
                spec.args
                    .get(3)
                    .expect("export command should contain the temporary path"),
            );
            assert_eq!(
                fs::read(&path).expect("seeded cookie jar should be readable"),
                NETSCAPE_COOKIE_HEADER
            );
            *self
                .path
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(path.clone());
            let mut held = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .expect("fake child should hold the cookie jar open");
            self.started.notify_one();
            self.release.notified().await;
            held.set_len(0)
                .expect("fake child should truncate the cookie jar");
            held.write_all(VALID_EXPORT)
                .expect("fake child should write the cookie jar");
            held.flush()
                .expect("fake child should flush the cookie jar");
            drop(held);
            self.completed.store(true, Ordering::SeqCst);
            Ok(ProcessOutput {
                status: exit_status(1),
                stdout: Vec::new(),
                stderr: Vec::new(),
            })
        }
    }

    #[tokio::test]
    async fn cancelled_outer_waits_for_private_post_cleanup_notification() {
        let runner = Arc::new(HeldExportRunner::new());
        let process_runner: Arc<dyn ProcessRunner> = runner.clone();
        let (cleanup_sender, cleanup_receiver) = oneshot::channel();
        let outer = tokio::spawn(export_browser_cookie_jar_with_completion(
            process_runner,
            Browser::Brave,
            cleanup_sender,
        ));

        tokio::time::timeout(TEST_TIMEOUT, runner.started.notified())
            .await
            .expect("fake child should start before the test deadline");
        let path = runner
            .path()
            .expect("fake child should record the temporary path");
        assert!(path.exists());

        outer.abort();
        assert!(outer.await.is_err());
        assert!(path.exists());
        runner.release.notify_one();

        tokio::time::timeout(TEST_TIMEOUT, cleanup_receiver)
            .await
            .expect("supervised cleanup should finish before the test deadline")
            .expect("cleanup task should emit its completion notification");
        assert!(runner.completed.load(Ordering::SeqCst));
        assert!(!path.exists());
    }
}
