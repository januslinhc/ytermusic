mod ytdlp;

use std::{
    error::Error,
    fmt,
    path::{Path, PathBuf},
};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::domain::{MediaId, MediaItem};

pub use ytdlp::{ResolverClock, SystemResolverClock, YtDlpResolver};

const MAX_PREVIEW_URL_BYTES: usize = 8_192;
pub(super) const MAX_RESOLVED_AUDIO_URL_BYTES: usize = 8_192;

/// A transient, bounded audio URL suitable for the optional spectrum analyzer.
///
/// Construction is intentionally limited to [`ResolvedStream`] so callers
/// cannot turn unrelated URLs into analysis inputs.
#[derive(Clone, Eq, Hash, PartialEq)]
pub struct AnalysisStreamUrl(url::Url);

impl AnalysisStreamUrl {
    fn from_resolved_url(url: &url::Url) -> Option<Self> {
        let serialized = url.as_str();
        (serialized.len() <= MAX_RESOLVED_AUDIO_URL_BYTES
            && url.scheme() == "https"
            && url.host().is_some())
        .then(|| Self(url.clone()))
    }

    #[must_use]
    pub const fn as_url(&self) -> &url::Url {
        &self.0
    }
}

impl fmt::Debug for AnalysisStreamUrl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("AnalysisStreamUrl([REDACTED])")
    }
}

impl fmt::Display for AnalysisStreamUrl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[REDACTED analysis stream URL]")
    }
}

#[derive(Clone, Eq, Hash, PartialEq)]
pub struct PreviewStreamUrl(url::Url);

impl PreviewStreamUrl {
    /// Parses a bounded HTTPS preview stream URL.
    ///
    /// # Errors
    ///
    /// Returns an error when the value is oversized, malformed, lacks a host,
    /// or does not use HTTPS.
    pub fn parse(value: &str) -> Result<Self, PreviewStreamUrlError> {
        let value = value.trim();
        if value.is_empty() || value.len() > MAX_PREVIEW_URL_BYTES {
            return Err(PreviewStreamUrlError);
        }
        let url = url::Url::parse(value).map_err(|_| PreviewStreamUrlError)?;
        if url.as_str().len() > MAX_PREVIEW_URL_BYTES
            || url.scheme() != "https"
            || url.host().is_none()
        {
            return Err(PreviewStreamUrlError);
        }
        Ok(Self(url))
    }

    #[must_use]
    pub const fn as_url(&self) -> &url::Url {
        &self.0
    }
}

impl fmt::Debug for PreviewStreamUrl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PreviewStreamUrl([REDACTED])")
    }
}

impl fmt::Display for PreviewStreamUrl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[REDACTED preview stream URL]")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PreviewStreamUrlError;

impl fmt::Display for PreviewStreamUrlError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("preview stream URL must be a bounded HTTPS URL with a host")
    }
}

impl Error for PreviewStreamUrlError {}

#[derive(Clone, Eq, Hash, PartialEq)]
pub struct AuthIdentity(String);

impl AuthIdentity {
    /// Creates an opaque identity used to isolate authenticated cache entries.
    ///
    /// # Errors
    ///
    /// Returns an error when `value` is empty or contains only whitespace.
    pub fn new(value: impl Into<String>) -> Result<Self, AuthIdentityError> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(AuthIdentityError);
        }
        Ok(Self(value))
    }

    pub(super) fn cache_key(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for AuthIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("AuthIdentity([REDACTED])")
    }
}

impl fmt::Display for AuthIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[REDACTED auth identity]")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AuthIdentityError;

impl fmt::Display for AuthIdentityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("authentication identity must not be empty")
    }
}

impl Error for AuthIdentityError {}

#[derive(Clone, Eq, PartialEq)]
pub struct CookieFile {
    path: PathBuf,
    identity: AuthIdentity,
}

impl CookieFile {
    #[must_use]
    pub fn new(path: impl Into<PathBuf>, identity: AuthIdentity) -> Self {
        Self {
            path: path.into(),
            identity,
        }
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    pub(super) const fn identity(&self) -> &AuthIdentity {
        &self.identity
    }
}

impl fmt::Debug for CookieFile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CookieFile([REDACTED])")
    }
}

impl fmt::Display for CookieFile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[REDACTED cookie file]")
    }
}

#[derive(Clone, PartialEq)]
pub struct ResolvedStream {
    pub media_id: MediaId,
    pub url: url::Url,
    pub preview_url: Option<PreviewStreamUrl>,
    pub title: Option<String>,
    pub duration_ms: Option<u64>,
    pub codec: Option<String>,
    pub format_id: Option<String>,
    pub resolved_at: time::OffsetDateTime,
    analysis_raw_eligible: bool,
}

impl ResolvedStream {
    /// Creates a stream from an already parsed audio URL.
    ///
    /// This constructor can validate only the canonical URL representation.
    /// Resolver adapters handling raw external input should use
    /// [`Self::from_raw_audio_url`] so the pre-parse byte bound is retained.
    #[must_use]
    pub fn new(media_id: MediaId, url: url::Url, resolved_at: time::OffsetDateTime) -> Self {
        Self {
            media_id,
            url,
            preview_url: None,
            title: None,
            duration_ms: None,
            codec: None,
            format_id: None,
            resolved_at,
            analysis_raw_eligible: true,
        }
    }

    /// Creates a playable stream from a raw external audio URL while retaining
    /// whether that raw input was eligible for transient analysis.
    ///
    /// The raw string is never stored. Inputs over the analysis byte bound may
    /// still produce valid playback streams, but their analysis accessor
    /// returns `None`.
    ///
    /// # Errors
    ///
    /// Returns a typed, redacted error for empty, malformed, unsupported, or
    /// hostless playback URLs.
    pub fn from_raw_audio_url(
        media_id: MediaId,
        value: &str,
        resolved_at: time::OffsetDateTime,
    ) -> Result<Self, ResolvedAudioUrlError> {
        let value = value.trim();
        if value.is_empty() {
            return Err(ResolvedAudioUrlError::Empty);
        }
        let url = url::Url::parse(value).map_err(|_| ResolvedAudioUrlError::Invalid)?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(ResolvedAudioUrlError::UnsupportedScheme);
        }
        if url.host().is_none() {
            return Err(ResolvedAudioUrlError::MissingHost);
        }
        let mut stream = Self::new(media_id, url, resolved_at);
        if value.len() > MAX_RESOLVED_AUDIO_URL_BYTES {
            stream.analysis_raw_eligible = false;
        }
        Ok(stream)
    }

    /// Returns a redacted, in-memory-only analysis source when the resolved
    /// audio URL meets the analyzer's canonical bounds and HTTPS requirements.
    /// Extractor-backed streams additionally validate the trimmed raw URL
    /// before parsing, so canonicalization cannot bypass the same byte limit.
    #[must_use]
    pub fn analysis_stream_url(&self) -> Option<AnalysisStreamUrl> {
        self.analysis_raw_eligible
            .then(|| AnalysisStreamUrl::from_resolved_url(&self.url))
            .flatten()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResolvedAudioUrlError {
    Empty,
    Invalid,
    UnsupportedScheme,
    MissingHost,
}

impl fmt::Display for ResolvedAudioUrlError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::Empty => "resolved audio URL is empty",
            Self::Invalid => "resolved audio URL is invalid",
            Self::UnsupportedScheme => "resolved audio URL scheme is unsupported",
            Self::MissingHost => "resolved audio URL has no host",
        };
        formatter.write_str(message)
    }
}

impl Error for ResolvedAudioUrlError {}

impl fmt::Debug for ResolvedStream {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResolvedStream")
            .field("media_id", &"[REDACTED provider identity]")
            .field("url", &"[REDACTED stream URL]")
            .field(
                "preview_url",
                &self
                    .preview_url
                    .as_ref()
                    .map(|_| "[REDACTED preview stream URL]"),
            )
            .field("title", &self.title.as_ref().map(|_| "[REDACTED title]"))
            .field("duration_ms", &self.duration_ms)
            .field("codec", &self.codec)
            .field("format_id", &self.format_id)
            .field("resolved_at", &self.resolved_at)
            .field("analysis_raw_eligible", &"[REDACTED transient eligibility]")
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResolvePolicy {
    UseCache,
    ForceRefresh,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResolveErrorCategory {
    Cancellation,
    UnsupportedInput,
    InvalidInput,
    Process,
    Extractor,
    InvalidResponse,
    MissingStream,
}

impl ResolveErrorCategory {
    const fn label(self) -> &'static str {
        match self {
            Self::Cancellation => "cancelled",
            Self::UnsupportedInput => "unsupported input",
            Self::InvalidInput => "invalid input",
            Self::Process => "resolver process failure",
            Self::Extractor => "extractor failure",
            Self::InvalidResponse => "invalid extractor response",
            Self::MissingStream => "missing stream",
        }
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct ResolveError {
    category: ResolveErrorCategory,
    message: String,
}

impl ResolveError {
    pub(super) fn new(category: ResolveErrorCategory, message: impl Into<String>) -> Self {
        Self {
            category,
            message: message.into(),
        }
    }

    #[must_use]
    pub const fn category(&self) -> ResolveErrorCategory {
        self.category
    }
}

impl fmt::Display for ResolveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.category.label(), self.message)
    }
}

impl fmt::Debug for ResolveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResolveError")
            .field("category", &self.category)
            .field("message", &self.message)
            .finish()
    }
}

impl Error for ResolveError {}

#[async_trait]
pub trait Resolver: Send + Sync {
    /// Resolves one media item into a short-lived playable stream.
    ///
    /// # Errors
    ///
    /// Returns a stable, sanitized resolution error.
    async fn resolve_with_policy(
        &self,
        item: &MediaItem,
        auth: Option<&CookieFile>,
        policy: ResolvePolicy,
        cancel: CancellationToken,
    ) -> Result<ResolvedStream, ResolveError>;

    /// Resolves one media item, reusing a live cache entry when available.
    ///
    /// # Errors
    ///
    /// Returns a stable, sanitized resolution error.
    async fn resolve(
        &self,
        item: &MediaItem,
        auth: Option<&CookieFile>,
        cancel: CancellationToken,
    ) -> Result<ResolvedStream, ResolveError> {
        self.resolve_with_policy(item, auth, ResolvePolicy::UseCache, cancel)
            .await
    }
}
