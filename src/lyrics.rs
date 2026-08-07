use std::{
    collections::{HashMap, VecDeque},
    fmt,
    hash::{Hash, Hasher},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use futures::StreamExt;
use serde::Deserialize;
use serde::de::{DeserializeSeed, SeqAccess, Visitor};
use thiserror::Error;
use unicode_normalization::{UnicodeNormalization, char::is_combining_mark};
use url::Url;

use crate::{
    domain::{MediaId, MediaItem},
    provider::{MusicProvider, ProviderError, ProviderErrorKind},
};

/// Maximum accepted response body for a future lyrics transport.
pub const MAX_LYRICS_RESPONSE_BYTES: usize = 1024 * 1024;
/// Maximum combined plain and synchronized lyric text retained in memory.
pub const MAX_LYRICS_TEXT_BYTES: usize = 256 * 1024;
/// Maximum number of synchronized lines retained in one document.
pub const MAX_TIMED_LYRIC_LINES: usize = 4_096;
/// Maximum UTF-8 byte length retained for one synchronized line.
pub const MAX_TIMED_LYRIC_LINE_BYTES: usize = 4 * 1024;
const MAX_LRCLIB_RESULTS: usize = 50;
const MAX_LRCLIB_FALLBACK_REQUESTS: usize = 3;
const LRCLIB_TITLE_SEPARATORS: [&str; 3] = [" - ", " – ", " — "];
const LRCLIB_DURATION_TOLERANCE_MS: u64 = 2_000;
const MAX_LYRICS_METADATA_BYTES: usize = 4 * 1024;
const MAX_LYRICS_ARTISTS: usize = 32;
const MAX_LRC_SOURCE_LINE_BYTES: usize = 8 * 1024;
const LRCLIB_BASE_URL: &str = "https://lrclib.net/api/search";
const LRCLIB_USER_AGENT: &str = concat!(
    "ytermusic/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/ytermusic/ytermusic)"
);
const LRCLIB_CACHE_CAPACITY: usize = 128;
const LRCLIB_CACHE_TTL_MS: u64 = 15 * 60 * 1_000;
const LRCLIB_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const LRCLIB_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// Normalizes lyric text before storage or terminal-width measurement.
///
/// Newlines remain structural, CRLF and lone CR use LF, tabs become one
/// visible ASCII space, and all other C0/C1 controls are discarded.
pub(crate) fn normalize_lyrics_text(text: &str) -> String {
    normalize_lyrics_text_with_line_separator(text, '\n')
}

fn normalize_timed_lyric_text(text: &str) -> String {
    normalize_lyrics_text_with_line_separator(text, ' ')
}

fn normalize_lyrics_text_with_line_separator(text: &str, line_separator: char) -> String {
    let mut normalized = String::with_capacity(text.len());
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '\r' => {
                if characters.peek() == Some(&'\n') {
                    characters.next();
                }
                normalized.push(line_separator);
            }
            '\n' => normalized.push(line_separator),
            '\t' => normalized.push(' '),
            character if character.is_control() => {}
            character => normalized.push(character),
        }
    }
    normalized
}

#[derive(Clone)]
pub struct LrclibHttpRequest {
    url: Url,
    user_agent: &'static str,
}

impl LrclibHttpRequest {
    #[must_use]
    pub fn url(&self) -> &Url {
        &self.url
    }

    #[must_use]
    pub const fn user_agent(&self) -> &'static str {
        self.user_agent
    }
}

impl fmt::Debug for LrclibHttpRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LrclibHttpRequest")
            .field("scheme", &self.url.scheme())
            .field("path", &self.url.path())
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
pub struct LrclibHttpResponse {
    status: u16,
    body: Vec<u8>,
    redirected: bool,
}

impl LrclibHttpResponse {
    #[must_use]
    pub const fn new(status: u16, body: Vec<u8>, redirected: bool) -> Self {
        Self {
            status,
            body,
            redirected,
        }
    }
}

impl fmt::Debug for LrclibHttpResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LrclibHttpResponse")
            .field("status", &self.status)
            .field("body_bytes", &self.body.len())
            .field("redirected", &self.redirected)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum LrclibSourceError {
    #[error("lyrics source is unavailable")]
    Unavailable,
    #[error("lyrics source returned a redirect")]
    Redirected,
    #[error("lyrics source returned a non-success status")]
    NonSuccess,
    #[error("lyrics source response exceeds the accepted size")]
    ResponseTooLarge,
    #[error("lyrics source returned an invalid response")]
    InvalidResponse,
}

#[async_trait]
pub trait LrclibTransport: Send + Sync {
    async fn get(
        &self,
        request: LrclibHttpRequest,
    ) -> Result<LrclibHttpResponse, LrclibSourceError>;
}

pub trait LrclibClock: Send + Sync {
    fn now_ms(&self) -> u64;
}

#[derive(Debug)]
pub struct MonotonicLrclibClock {
    started: Instant,
}

impl Default for MonotonicLrclibClock {
    fn default() -> Self {
        Self {
            started: Instant::now(),
        }
    }
}

impl LrclibClock for MonotonicLrclibClock {
    fn now_ms(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX)
    }
}

pub struct ReqwestLrclibTransport {
    client: reqwest::Client,
}

impl ReqwestLrclibTransport {
    /// Builds the hardened HTTPS transport used by the production lyrics source.
    ///
    /// # Errors
    ///
    /// Returns a payload-free error when the HTTP client cannot be initialized.
    pub fn new() -> Result<Self, LrclibSourceError> {
        let client = reqwest::Client::builder()
            .use_rustls_tls()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(LRCLIB_CONNECT_TIMEOUT)
            .timeout(LRCLIB_REQUEST_TIMEOUT)
            .build()
            .map_err(|_| LrclibSourceError::Unavailable)?;
        Ok(Self { client })
    }
}

impl fmt::Debug for ReqwestLrclibTransport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ReqwestLrclibTransport")
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl LrclibTransport for ReqwestLrclibTransport {
    async fn get(
        &self,
        request: LrclibHttpRequest,
    ) -> Result<LrclibHttpResponse, LrclibSourceError> {
        if request.url.scheme() != "https" {
            return Err(LrclibSourceError::Unavailable);
        }
        let response = self
            .client
            .get(request.url)
            .header(reqwest::header::USER_AGENT, request.user_agent)
            .send()
            .await
            .map_err(|_| LrclibSourceError::Unavailable)?;
        let status = response.status();
        if status.is_redirection() {
            return Err(LrclibSourceError::Redirected);
        }
        if !status.is_success() {
            return Err(LrclibSourceError::NonSuccess);
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_LYRICS_RESPONSE_BYTES as u64)
        {
            return Err(LrclibSourceError::ResponseTooLarge);
        }

        let mut body = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| LrclibSourceError::Unavailable)?;
            if body.len().saturating_add(chunk.len()) > MAX_LYRICS_RESPONSE_BYTES {
                return Err(LrclibSourceError::ResponseTooLarge);
            }
            body.extend_from_slice(&chunk);
        }
        Ok(LrclibHttpResponse::new(status.as_u16(), body, false))
    }
}

#[derive(Clone, Eq, Hash, PartialEq)]
struct LrclibCacheKey {
    media_id: MediaId,
    metadata_fingerprint: u64,
}

#[derive(Clone)]
struct LrclibCacheEntry {
    expires_at_ms: u64,
    document: Option<LyricsDocument>,
}

enum CacheLookup {
    Miss,
    Hit(Option<LyricsDocument>),
}

#[derive(Default)]
struct LrclibCache {
    entries: HashMap<LrclibCacheKey, LrclibCacheEntry>,
    order: VecDeque<LrclibCacheKey>,
}

pub struct LrclibClient {
    transport: Arc<dyn LrclibTransport>,
    clock: Arc<dyn LrclibClock>,
    cache: Mutex<LrclibCache>,
    capacity: usize,
    ttl_ms: u64,
}

impl LrclibClient {
    /// Builds a production LRCLIB client with bounded in-memory caching.
    ///
    /// # Errors
    ///
    /// Returns a payload-free error when the hardened HTTP client cannot be initialized.
    pub fn new() -> Result<Self, LrclibSourceError> {
        Ok(Self::with_dependencies(
            Arc::new(ReqwestLrclibTransport::new()?),
            Arc::new(MonotonicLrclibClock::default()),
            LRCLIB_CACHE_CAPACITY,
            LRCLIB_CACHE_TTL_MS,
        ))
    }

    #[must_use]
    pub fn with_dependencies(
        transport: Arc<dyn LrclibTransport>,
        clock: Arc<dyn LrclibClock>,
        capacity: usize,
        ttl_ms: u64,
    ) -> Self {
        Self {
            transport,
            clock,
            cache: Mutex::new(LrclibCache::default()),
            capacity: capacity.max(1),
            ttl_ms,
        }
    }

    /// Fetches a conservatively matched synchronized lyric document.
    ///
    /// # Errors
    ///
    /// Returns only payload-free transport or parsing classifications.
    pub async fn fetch(
        &self,
        item: &MediaItem,
    ) -> Result<Option<LyricsDocument>, LrclibSourceError> {
        if !request_metadata_is_bounded(item) {
            return Err(LrclibSourceError::InvalidResponse);
        }
        let Some(primary_artist) = item.creators.first() else {
            return Ok(None);
        };
        let key = LrclibCacheKey {
            media_id: item.id.clone(),
            metadata_fingerprint: metadata_fingerprint(item),
        };
        let now_ms = self.clock.now_ms();
        if let CacheLookup::Hit(document) = self.cached(&key, now_ms) {
            return Ok(document);
        }

        let artists = item.creators.iter().map(String::as_str).collect::<Vec<_>>();
        let request =
            LrclibMatchRequest::new(&item.title, primary_artist, &artists, item.duration_ms)
                .with_collection(item.collection.as_deref());
        let strict_body = self
            .search(
                &item.title,
                Some(primary_artist),
                item.collection.as_deref(),
            )
            .await?;
        let mut document = match_lrclib_response(&strict_body, &request)
            .map_err(|_| LrclibSourceError::InvalidResponse)?;
        if document.is_none() {
            let fallback_body = self.search(&item.title, None, None).await?;
            document =
                match_lrclib_verified_fallback_response(&fallback_body, &item.title, &request)
                    .map_err(|_| LrclibSourceError::InvalidResponse)?;
        }
        if document.is_none() {
            for variant in fallback_title_variants(&item.title) {
                let fallback_body = self.search(&variant, None, None).await?;
                document = match_lrclib_fallback_response(&fallback_body, &variant, &request)
                    .map_err(|_| LrclibSourceError::InvalidResponse)?;
                if document.is_some() {
                    break;
                }
            }
        }
        self.insert_cache(key, now_ms.saturating_add(self.ttl_ms), document.clone());
        Ok(document)
    }

    async fn search(
        &self,
        track_name: &str,
        artist_name: Option<&str>,
        album_name: Option<&str>,
    ) -> Result<Vec<u8>, LrclibSourceError> {
        let mut url = Url::parse(LRCLIB_BASE_URL).map_err(|_| LrclibSourceError::Unavailable)?;
        url.query_pairs_mut().append_pair("track_name", track_name);
        if let Some(artist_name) = artist_name {
            url.query_pairs_mut()
                .append_pair("artist_name", artist_name);
        }
        if let Some(album_name) = album_name {
            url.query_pairs_mut().append_pair("album_name", album_name);
        }
        let response = self
            .transport
            .get(LrclibHttpRequest {
                url,
                user_agent: LRCLIB_USER_AGENT,
            })
            .await?;
        validated_lrclib_body(response)
    }

    fn cached(&self, key: &LrclibCacheKey, now_ms: u64) -> CacheLookup {
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(entry) = cache.entries.get(key) else {
            return CacheLookup::Miss;
        };
        if entry.expires_at_ms <= now_ms {
            cache.entries.remove(key);
            cache.order.retain(|candidate| candidate != key);
            return CacheLookup::Miss;
        }
        CacheLookup::Hit(entry.document.clone())
    }

    fn insert_cache(
        &self,
        key: LrclibCacheKey,
        expires_at_ms: u64,
        document: Option<LyricsDocument>,
    ) {
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.order.retain(|candidate| candidate != &key);
        cache.order.push_back(key.clone());
        cache.entries.insert(
            key,
            LrclibCacheEntry {
                expires_at_ms,
                document,
            },
        );
        while cache.entries.len() > self.capacity {
            if let Some(oldest) = cache.order.pop_front() {
                cache.entries.remove(&oldest);
            } else {
                break;
            }
        }
    }
}

fn validated_lrclib_body(response: LrclibHttpResponse) -> Result<Vec<u8>, LrclibSourceError> {
    if response.redirected {
        return Err(LrclibSourceError::Redirected);
    }
    if !(200..300).contains(&response.status) {
        return Err(LrclibSourceError::NonSuccess);
    }
    if response.body.len() > MAX_LYRICS_RESPONSE_BYTES {
        return Err(LrclibSourceError::ResponseTooLarge);
    }
    Ok(response.body)
}

impl fmt::Debug for LrclibClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let cache_entries = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entries
            .len();
        formatter
            .debug_struct("LrclibClient")
            .field("cache_entries", &cache_entries)
            .field("capacity", &self.capacity)
            .field("ttl_ms", &self.ttl_ms)
            .finish_non_exhaustive()
    }
}

fn metadata_fingerprint(item: &MediaItem) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    item.title.hash(&mut hasher);
    item.creators.hash(&mut hasher);
    item.collection.hash(&mut hasher);
    item.duration_ms.hash(&mut hasher);
    hasher.finish()
}

fn request_metadata_is_bounded(item: &MediaItem) -> bool {
    let bounded_text = |text: &str| {
        !text.trim().is_empty()
            && text.len() <= MAX_LYRICS_METADATA_BYTES
            && !text.chars().any(char::is_control)
    };
    bounded_text(&item.title)
        && !item.creators.is_empty()
        && item.creators.len() <= MAX_LYRICS_ARTISTS
        && item.creators.iter().all(|artist| bounded_text(artist))
        && item.collection.as_deref().is_none_or(bounded_text)
}

fn fallback_title_variants(title: &str) -> Vec<String> {
    if title.len() > MAX_LYRICS_METADATA_BYTES {
        return Vec::new();
    }

    let Some(normalized_full) = normalize_match_text(title) else {
        return Vec::new();
    };
    let mut accepted = Vec::<(String, String)>::with_capacity(MAX_LRCLIB_FALLBACK_REQUESTS);
    let mut remainder = title;
    let mut found_separator = false;

    loop {
        let separator = LRCLIB_TITLE_SEPARATORS
            .iter()
            .filter_map(|separator| {
                remainder
                    .find(separator)
                    .map(|index| (index, separator.len()))
            })
            .min_by_key(|(index, _)| *index);
        let segment = if let Some((index, separator_len)) = separator {
            found_separator = true;
            let segment = &remainder[..index];
            remainder = &remainder[index + separator_len..];
            segment
        } else if found_separator {
            remainder
        } else {
            break;
        };
        let segment = segment.trim();

        if segment.len() <= MAX_LYRICS_METADATA_BYTES {
            let Some(normalized) = normalize_match_text(segment) else {
                if separator.is_none() {
                    break;
                }
                continue;
            };
            if normalized
                .chars()
                .filter(|character| character.is_alphanumeric())
                .take(2)
                .count()
                == 2
                && normalized_full != normalized
                && accepted
                    .iter()
                    .all(|(_, prior_normalized)| prior_normalized != &normalized)
            {
                accepted.push((segment.to_owned(), normalized));
                if accepted.len() == MAX_LRCLIB_FALLBACK_REQUESTS {
                    break;
                }
            }
        }

        if separator.is_none() {
            break;
        }
    }

    accepted.into_iter().map(|(variant, _)| variant).collect()
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum LyricsSourceServiceError {
    #[error("music provider could not load lyrics")]
    Provider(#[source] ProviderError),
    #[error("external lyrics source could not load lyrics")]
    External(#[source] LrclibSourceError),
}

pub struct LyricsSourceService {
    provider: Arc<dyn MusicProvider>,
    lrclib: Option<Arc<LrclibClient>>,
    external_sync: bool,
}

impl LyricsSourceService {
    #[must_use]
    pub const fn new(
        provider: Arc<dyn MusicProvider>,
        lrclib: Arc<LrclibClient>,
        external_sync: bool,
    ) -> Self {
        Self {
            provider,
            lrclib: Some(lrclib),
            external_sync,
        }
    }

    /// Builds a source that retains provider lyrics when the optional external
    /// client is disabled or could not be initialized.
    #[must_use]
    pub const fn provider_only(provider: Arc<dyn MusicProvider>) -> Self {
        Self {
            provider,
            lrclib: None,
            external_sync: false,
        }
    }

    /// Loads plain provider lyrics and optional conservatively matched synchronized lyrics.
    ///
    /// # Errors
    ///
    /// Returns payload-free source classifications when neither source can provide a document.
    pub async fn load(
        &self,
        item: &MediaItem,
    ) -> Result<Option<LyricsDocument>, LyricsSourceServiceError> {
        let provider_request = self.provider.lyrics(&item.id);
        let (provider_result, external_result) = if self.external_sync
            && let Some(lrclib) = &self.lrclib
        {
            tokio::join!(provider_request, lrclib.fetch(item))
        } else {
            (provider_request.await, Ok(None))
        };

        let (youtube, provider_error) = match provider_result {
            Ok(lyrics) => (Some(lyrics), None),
            Err(error) if error.kind == ProviderErrorKind::NotFound => (None, None),
            Err(error) => (None, Some(error)),
        };

        let (external, external_error) = match external_result {
            Ok(document) => (document, None),
            Err(error) => (None, Some(error)),
        };

        if let Some(external) = external {
            if external.instrumental() && youtube.is_none() {
                return Ok(Some(external));
            }
            if !external.timed().is_empty() {
                let merged = LyricsDocument::new(
                    LyricsSource::Lrclib,
                    youtube.as_ref().map(|lyrics| lyrics.text().to_owned()),
                    external.timed().to_vec(),
                    false,
                );
                return Ok(Some(merged.unwrap_or(external)));
            }
            if youtube.is_none() {
                return Ok(Some(external));
            }
        }

        if let Some(youtube) = youtube {
            let document = LyricsDocument::new(
                LyricsSource::YouTubeMusic,
                Some(youtube.text().to_owned()),
                Vec::new(),
                false,
            )
            .map_err(|_| LyricsSourceServiceError::External(LrclibSourceError::InvalidResponse))?;
            return Ok(Some(document));
        }
        if let Some(error) = provider_error {
            return Err(LyricsSourceServiceError::Provider(error));
        }
        if let Some(error) = external_error {
            return Err(LyricsSourceServiceError::External(error));
        }
        Ok(None)
    }
}

impl fmt::Debug for LyricsSourceService {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LyricsSourceService")
            .field("external_sync", &self.external_sync)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum LyricsParseError {
    #[error("lyrics response exceeds the accepted size")]
    ResponseTooLarge,
    #[error("lyrics response contains too many results")]
    TooManyResults,
    #[error("lyrics response is malformed")]
    MalformedResponse,
    #[error("synchronized lyrics contain a malformed timestamp")]
    MalformedTimestamp,
    #[error("a synchronized lyric line exceeds the accepted size")]
    LineTooLarge,
    #[error("lyrics content exceeds the accepted size")]
    LyricsTooLarge,
    #[error("lyrics response contains ambiguous equally ranked matches")]
    AmbiguousMatch,
    #[error("lyrics response contains no usable lyric content")]
    MissingLyrics,
}

#[derive(Clone, Copy)]
pub struct LrclibMatchRequest<'a> {
    title: &'a str,
    primary_artist: &'a str,
    artists: &'a [&'a str],
    duration_ms: Option<u64>,
    collection: Option<&'a str>,
}

impl<'a> LrclibMatchRequest<'a> {
    #[must_use]
    pub const fn new(
        title: &'a str,
        primary_artist: &'a str,
        artists: &'a [&'a str],
        duration_ms: Option<u64>,
    ) -> Self {
        Self {
            title,
            primary_artist,
            artists,
            duration_ms,
            collection: None,
        }
    }

    #[must_use]
    pub const fn with_collection(mut self, collection: Option<&'a str>) -> Self {
        self.collection = collection;
        self
    }
}

/// Parses a bounded LRC document and retains a plain-text fallback when supplied.
///
/// # Errors
///
/// Malformed timed lines are skipped. Returns a payload-free parse error when no
/// content remains or a response, line, line-count, or retained-text bound is
/// violated.
pub fn parse_lrc(
    synchronized: &str,
    plain_fallback: Option<&str>,
) -> Result<LyricsDocument, LyricsParseError> {
    if synchronized.len() > MAX_LYRICS_RESPONSE_BYTES {
        return Err(LyricsParseError::ResponseTooLarge);
    }

    let synchronized = normalize_lyrics_text(synchronized);
    let plain = normalize_plain_fallback(plain_fallback)?;
    let mut parsed = Vec::new();
    let mut source_order = 0_usize;
    let mut retained_bytes = plain.as_ref().map_or(0, String::len);

    for raw_line in synchronized.lines() {
        let (timestamps, text) = match parse_lrc_line(raw_line) {
            Ok(parsed) => parsed,
            Err(LyricsParseError::MalformedTimestamp) => continue,
            Err(error) => return Err(error),
        };
        let text = text.trim();
        if timestamps.is_empty() || text.is_empty() {
            continue;
        }
        if text.len() > MAX_TIMED_LYRIC_LINE_BYTES {
            return Err(LyricsParseError::LineTooLarge);
        }

        for timestamp in timestamps {
            if parsed.len() >= MAX_TIMED_LYRIC_LINES {
                return Err(LyricsParseError::LyricsTooLarge);
            }
            retained_bytes = retained_bytes.saturating_add(text.len());
            if retained_bytes > MAX_LYRICS_TEXT_BYTES {
                return Err(LyricsParseError::LyricsTooLarge);
            }
            parsed.push((timestamp, source_order, text.to_owned()));
            source_order = source_order.saturating_add(1);
        }
    }

    parsed.sort_by_key(|(timestamp, order, _)| (*timestamp, *order));
    parsed.dedup_by_key(|(timestamp, _, _)| *timestamp);

    let mut timed = Vec::with_capacity(parsed.len());
    for (index, (start_ms, _, text)) in parsed.iter().enumerate() {
        let end_ms = parsed.get(index + 1).map(|(next_start, _, _)| *next_start);
        let line = TimedLyricLine::new(*start_ms, end_ms, text)
            .map_err(|_| LyricsParseError::LyricsTooLarge)?;
        timed.push(line);
    }

    LyricsDocument::new(LyricsSource::Lrclib, plain, timed, false).map_err(|error| match error {
        LyricsError::MissingLyrics | LyricsError::EmptyText { .. } => {
            LyricsParseError::MissingLyrics
        }
        LyricsError::TextTooLong { .. }
        | LyricsError::InvalidTimestampRange { .. }
        | LyricsError::InstrumentalHasLyrics
        | LyricsError::TooManyTimedLines { .. }
        | LyricsError::TimedLinesOutOfOrder { .. }
        | LyricsError::TotalTextTooLong { .. } => LyricsParseError::LyricsTooLarge,
    })
}

fn normalize_plain_fallback(plain: Option<&str>) -> Result<Option<String>, LyricsParseError> {
    let Some(plain) = plain else {
        return Ok(None);
    };
    let plain = plain.trim();
    if plain.len() > MAX_LYRICS_TEXT_BYTES {
        return Err(LyricsParseError::LyricsTooLarge);
    }
    let plain = normalize_lyrics_text(plain);
    let plain = plain.trim();
    Ok((!plain.is_empty()).then(|| plain.to_owned()))
}

fn parse_lrc_line(raw_line: &str) -> Result<(Vec<u64>, &str), LyricsParseError> {
    if raw_line.len() > MAX_LRC_SOURCE_LINE_BYTES {
        return Err(LyricsParseError::LineTooLarge);
    }
    let mut remainder = raw_line.trim_start();
    let mut timestamps = Vec::new();

    while let Some(token_start) = remainder.strip_prefix('[') {
        let Some(close_index) = token_start.find(']') else {
            return if token_start
                .chars()
                .next()
                .is_some_and(|character| character.is_ascii_digit())
            {
                Err(LyricsParseError::MalformedTimestamp)
            } else {
                Ok((timestamps, remainder))
            };
        };
        let token = &token_start[..close_index];
        if let Some(timestamp) = parse_lrc_timestamp(token)? {
            timestamps.push(timestamp);
        } else {
            if timestamps.is_empty() {
                return Ok((Vec::new(), ""));
            }
            return Err(LyricsParseError::MalformedTimestamp);
        }
        remainder = &token_start[close_index + 1..];
    }

    Ok((timestamps, remainder))
}

fn parse_lrc_timestamp(token: &str) -> Result<Option<u64>, LyricsParseError> {
    const METADATA_TAGS: [&str; 8] = ["ar", "al", "ti", "by", "offset", "re", "ve", "length"];
    let Some((minutes, seconds_and_fraction)) = token.split_once(':') else {
        return Err(LyricsParseError::MalformedTimestamp);
    };
    if METADATA_TAGS
        .iter()
        .any(|tag| minutes.eq_ignore_ascii_case(tag))
    {
        return Ok(None);
    }
    if minutes.is_empty() || !minutes.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(LyricsParseError::MalformedTimestamp);
    }
    let (seconds, fraction_ms) = match seconds_and_fraction.split_once('.') {
        Some((seconds, fraction)) if fraction.len() == 2 => {
            (seconds, parse_ascii_u64(fraction)?.saturating_mul(10))
        }
        Some((seconds, fraction)) if fraction.len() == 3 => (seconds, parse_ascii_u64(fraction)?),
        Some(_) => return Err(LyricsParseError::MalformedTimestamp),
        None => (seconds_and_fraction, 0),
    };
    if seconds.len() != 2 || !seconds.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(LyricsParseError::MalformedTimestamp);
    }
    let minutes = parse_ascii_u64(minutes)?;
    let seconds = parse_ascii_u64(seconds)?;
    if seconds >= 60 {
        return Err(LyricsParseError::MalformedTimestamp);
    }
    let start_ms = minutes
        .checked_mul(60_000)
        .and_then(|minutes| {
            seconds
                .checked_mul(1_000)
                .and_then(|seconds| minutes.checked_add(seconds))
        })
        .and_then(|timestamp| timestamp.checked_add(fraction_ms))
        .ok_or(LyricsParseError::MalformedTimestamp)?;
    Ok(Some(start_ms))
}

fn parse_ascii_u64(value: &str) -> Result<u64, LyricsParseError> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(LyricsParseError::MalformedTimestamp);
    }
    value
        .parse()
        .map_err(|_| LyricsParseError::MalformedTimestamp)
}

/// Selects a unique, conservatively matched LRCLIB result from a bounded search response.
///
/// # Errors
///
/// Returns a payload-free error when the JSON or lyric content is malformed,
/// exceeds a configured bound, or contains equally ranked accepted matches.
pub fn match_lrclib_response(
    response: &[u8],
    request: &LrclibMatchRequest<'_>,
) -> Result<Option<LyricsDocument>, LyricsParseError> {
    if response.len() > MAX_LYRICS_RESPONSE_BYTES {
        return Err(LyricsParseError::ResponseTooLarge);
    }
    let Some(prepared) = PreparedMatch::new(request) else {
        return Ok(None);
    };

    let mut deserializer = serde_json::Deserializer::from_slice(response);
    let accumulator = LrclibResponseSeed { request: &prepared }
        .deserialize(&mut deserializer)
        .map_err(|_| LyricsParseError::MalformedResponse)?;
    deserializer
        .end()
        .map_err(|_| LyricsParseError::MalformedResponse)?;
    accumulator.finish()
}

fn match_lrclib_fallback_response(
    response: &[u8],
    exact_title_variant: &str,
    request: &LrclibMatchRequest<'_>,
) -> Result<Option<LyricsDocument>, LyricsParseError> {
    match_lrclib_fallback_response_with_policy(
        response,
        exact_title_variant,
        request,
        FallbackIdentityPolicy::AllowUniqueUnverified,
    )
}

fn match_lrclib_verified_fallback_response(
    response: &[u8],
    exact_title: &str,
    request: &LrclibMatchRequest<'_>,
) -> Result<Option<LyricsDocument>, LyricsParseError> {
    match_lrclib_fallback_response_with_policy(
        response,
        exact_title,
        request,
        FallbackIdentityPolicy::RequireVerified,
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FallbackIdentityPolicy {
    AllowUniqueUnverified,
    RequireVerified,
}

fn match_lrclib_fallback_response_with_policy(
    response: &[u8],
    exact_title_variant: &str,
    request: &LrclibMatchRequest<'_>,
    identity_policy: FallbackIdentityPolicy,
) -> Result<Option<LyricsDocument>, LyricsParseError> {
    if response.len() > MAX_LYRICS_RESPONSE_BYTES {
        return Err(LyricsParseError::ResponseTooLarge);
    }
    let (Some(request), Some(title)) = (
        PreparedMatch::new(request),
        normalize_match_text(exact_title_variant),
    ) else {
        return Ok(None);
    };

    let mut deserializer = serde_json::Deserializer::from_slice(response);
    let accumulator = LrclibFallbackResponseSeed {
        request: &request,
        title: &title,
    }
    .deserialize(&mut deserializer)
    .map_err(|_| LyricsParseError::MalformedResponse)?;
    deserializer
        .end()
        .map_err(|_| LyricsParseError::MalformedResponse)?;
    accumulator.finish(identity_policy)
}

struct PreparedMatch {
    title: String,
    primary_artist: String,
    artists: Vec<String>,
    duration_ms: Option<u64>,
    collection: Option<String>,
}

impl PreparedMatch {
    fn new(request: &LrclibMatchRequest<'_>) -> Option<Self> {
        let title = normalize_match_text(request.title)?;
        let primary_artist = normalize_match_text(request.primary_artist)?;
        if title.is_empty() || primary_artist.is_empty() {
            return None;
        }

        let mut artists = request
            .artists
            .iter()
            .map(|artist| normalize_match_text(artist))
            .collect::<Option<Vec<_>>>()?;
        if !artists.iter().any(|artist| artist == &primary_artist) {
            artists.push(primary_artist.clone());
        }
        artists.sort_unstable();
        artists.dedup();
        let collection = match request.collection {
            Some(collection) => Some(normalize_match_text(collection)?),
            None => None,
        };

        Some(Self {
            title,
            primary_artist,
            artists,
            duration_ms: request.duration_ms,
            collection,
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LrclibRecord {
    track_name: String,
    artist_name: String,
    #[serde(default)]
    album_name: Option<String>,
    duration: Option<f64>,
    plain_lyrics: Option<String>,
    synced_lyrics: Option<String>,
    #[serde(default)]
    instrumental: bool,
}

struct LrclibResponseSeed<'a> {
    request: &'a PreparedMatch,
}

impl<'de> DeserializeSeed<'de> for LrclibResponseSeed<'_> {
    type Value = MatchAccumulator<CandidateRank>;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_seq(LrclibResponseVisitor {
            request: self.request,
        })
    }
}

struct LrclibResponseVisitor<'a> {
    request: &'a PreparedMatch,
}

struct LrclibFallbackResponseSeed<'a> {
    request: &'a PreparedMatch,
    title: &'a str,
}

impl<'de> DeserializeSeed<'de> for LrclibFallbackResponseSeed<'_> {
    type Value = FallbackMatchAccumulator;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_seq(LrclibFallbackResponseVisitor {
            request: self.request,
            title: self.title,
        })
    }
}

struct LrclibFallbackResponseVisitor<'a> {
    request: &'a PreparedMatch,
    title: &'a str,
}

impl<'de> Visitor<'de> for LrclibFallbackResponseVisitor<'_> {
    type Value = FallbackMatchAccumulator;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a bounded LRCLIB search result array")
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut accumulator = FallbackMatchAccumulator::default();
        while let Some(record) = sequence.next_element::<LrclibRecord>()? {
            accumulator.ranked.result_count = accumulator.ranked.result_count.saturating_add(1);
            if accumulator.ranked.result_count > MAX_LRCLIB_RESULTS {
                accumulator.ranked.failure = Some(LyricsParseError::TooManyResults);
                continue;
            }
            if accumulator.ranked.failure.is_some() {
                continue;
            }
            match accepted_fallback_document(&record, self.title, self.request) {
                Ok(Some((rank, document))) => accumulator.consider(rank, document),
                Ok(None) => {}
                Err(error) => accumulator.ranked.failure = Some(error),
            }
        }
        Ok(accumulator)
    }
}

impl<'de> Visitor<'de> for LrclibResponseVisitor<'_> {
    type Value = MatchAccumulator<CandidateRank>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a bounded LRCLIB search result array")
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut accumulator = MatchAccumulator::default();
        while let Some(record) = sequence.next_element::<LrclibRecord>()? {
            accumulator.result_count = accumulator.result_count.saturating_add(1);
            if accumulator.result_count > MAX_LRCLIB_RESULTS {
                accumulator.failure = Some(LyricsParseError::TooManyResults);
                continue;
            }
            if accumulator.failure.is_some() {
                continue;
            }
            match accepted_document(&record, self.request) {
                Ok(Some((rank, document))) => accumulator.consider(rank, document),
                Ok(None) => {}
                Err(error) => accumulator.failure = Some(error),
            }
        }
        Ok(accumulator)
    }
}

struct MatchAccumulator<Rank> {
    result_count: usize,
    selected: Option<LyricsDocument>,
    rank: Option<Rank>,
    tied: bool,
    failure: Option<LyricsParseError>,
}

impl<Rank> Default for MatchAccumulator<Rank> {
    fn default() -> Self {
        Self {
            result_count: 0,
            selected: None,
            rank: None,
            tied: false,
            failure: None,
        }
    }
}

impl<Rank: Copy + Ord> MatchAccumulator<Rank> {
    fn consider(&mut self, rank: Rank, document: LyricsDocument) {
        let Some(current_rank) = self.rank else {
            self.selected = Some(document);
            self.rank = Some(rank);
            return;
        };
        match rank.cmp(&current_rank) {
            std::cmp::Ordering::Greater => {
                self.selected = Some(document);
                self.rank = Some(rank);
                self.tied = false;
            }
            std::cmp::Ordering::Equal
                if self
                    .selected
                    .as_ref()
                    .is_some_and(|selected| selected != &document) =>
            {
                self.tied = true;
            }
            std::cmp::Ordering::Equal | std::cmp::Ordering::Less => {}
        }
    }

    fn finish(self) -> Result<Option<LyricsDocument>, LyricsParseError> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if self.tied {
            return Err(LyricsParseError::AmbiguousMatch);
        }
        Ok(self.selected)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FallbackCandidateRank {
    artist_match: bool,
    album_match: bool,
    synchronized: bool,
    duration_delta_ms: u64,
}

impl Ord for FallbackCandidateRank {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.artist_match
            .cmp(&other.artist_match)
            .then_with(|| self.album_match.cmp(&other.album_match))
            .then_with(|| self.synchronized.cmp(&other.synchronized))
            .then_with(|| other.duration_delta_ms.cmp(&self.duration_delta_ms))
    }
}

impl PartialOrd for FallbackCandidateRank {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Default)]
struct FallbackMatchAccumulator {
    ranked: MatchAccumulator<FallbackCandidateRank>,
    identity_verified: bool,
    first_unverified: Option<LyricsDocument>,
    conflicting_unverified: bool,
}

impl FallbackMatchAccumulator {
    fn consider(&mut self, rank: FallbackCandidateRank, document: LyricsDocument) {
        if rank.artist_match || rank.album_match {
            self.identity_verified = true;
        } else if let Some(first) = &self.first_unverified {
            self.conflicting_unverified |= first != &document;
        } else {
            self.first_unverified = Some(document.clone());
        }
        self.ranked.consider(rank, document);
    }

    fn finish(
        self,
        identity_policy: FallbackIdentityPolicy,
    ) -> Result<Option<LyricsDocument>, LyricsParseError> {
        if self.ranked.failure.is_some() {
            return self.ranked.finish();
        }
        if identity_policy == FallbackIdentityPolicy::RequireVerified && !self.identity_verified {
            return Ok(None);
        }
        if !self.identity_verified && self.conflicting_unverified {
            return Err(LyricsParseError::AmbiguousMatch);
        }
        self.ranked.finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CandidateRank {
    synchronized: bool,
    album_match: bool,
    duration_delta_ms: u64,
}

impl Ord for CandidateRank {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.synchronized
            .cmp(&other.synchronized)
            .then_with(|| self.album_match.cmp(&other.album_match))
            .then_with(|| other.duration_delta_ms.cmp(&self.duration_delta_ms))
    }
}

impl PartialOrd for CandidateRank {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

fn accepted_document(
    record: &LrclibRecord,
    request: &PreparedMatch,
) -> Result<Option<(CandidateRank, LyricsDocument)>, LyricsParseError> {
    validate_lrclib_record(record)?;

    let Some(title) = normalize_match_text(&record.track_name) else {
        return Ok(None);
    };
    if title != request.title || !artist_matches(&record.artist_name, request) {
        return Ok(None);
    }

    let duration_ms = normalize_duration(record.duration)?;
    let (Some(expected), Some(candidate)) = (request.duration_ms, duration_ms) else {
        return Ok(None);
    };
    let duration_delta_ms = expected.abs_diff(candidate);
    if duration_delta_ms > LRCLIB_DURATION_TOLERANCE_MS {
        return Ok(None);
    }
    let album_match = album_matches(record, request);
    let Some(document) = lrclib_document(record)? else {
        return Ok(None);
    };
    let rank = CandidateRank {
        synchronized: !document.timed().is_empty(),
        album_match,
        duration_delta_ms,
    };
    Ok(Some((rank, document)))
}

fn accepted_fallback_document(
    record: &LrclibRecord,
    exact_title: &str,
    request: &PreparedMatch,
) -> Result<Option<(FallbackCandidateRank, LyricsDocument)>, LyricsParseError> {
    validate_lrclib_record(record)?;

    let Some(title) = normalize_match_text(&record.track_name) else {
        return Ok(None);
    };
    if title != exact_title {
        return Ok(None);
    }

    let duration_ms = normalize_duration(record.duration)?;
    let (Some(expected), Some(candidate)) = (request.duration_ms, duration_ms) else {
        return Ok(None);
    };
    let duration_delta_ms = expected.abs_diff(candidate);
    if duration_delta_ms > LRCLIB_DURATION_TOLERANCE_MS {
        return Ok(None);
    }
    let artist_match = artist_matches(&record.artist_name, request);
    let album_match = album_matches(record, request);
    let Some(document) = lrclib_document(record)? else {
        return Ok(None);
    };
    let rank = FallbackCandidateRank {
        artist_match,
        album_match,
        synchronized: !document.timed().is_empty(),
        duration_delta_ms,
    };
    Ok(Some((rank, document)))
}

fn validate_lrclib_record(record: &LrclibRecord) -> Result<(), LyricsParseError> {
    if record.track_name.len() > MAX_LYRICS_METADATA_BYTES
        || record.artist_name.len() > MAX_LYRICS_METADATA_BYTES
        || record
            .album_name
            .as_ref()
            .is_some_and(|album| album.len() > MAX_LYRICS_METADATA_BYTES)
        || record
            .plain_lyrics
            .as_ref()
            .is_some_and(|lyrics| lyrics.len() > MAX_LYRICS_TEXT_BYTES)
        || record
            .synced_lyrics
            .as_ref()
            .is_some_and(|lyrics| lyrics.len() > MAX_LYRICS_RESPONSE_BYTES)
    {
        return Err(LyricsParseError::LyricsTooLarge);
    }
    Ok(())
}

fn album_matches(record: &LrclibRecord, request: &PreparedMatch) -> bool {
    request.collection.as_ref().is_some_and(|expected| {
        !expected.is_empty()
            && record
                .album_name
                .as_deref()
                .and_then(normalize_match_text)
                .as_ref()
                == Some(expected)
    })
}

fn lrclib_document(record: &LrclibRecord) -> Result<Option<LyricsDocument>, LyricsParseError> {
    if record.instrumental {
        if record
            .plain_lyrics
            .as_deref()
            .is_some_and(|lyrics| !lyrics.trim().is_empty())
            || record
                .synced_lyrics
                .as_deref()
                .is_some_and(|lyrics| !lyrics.trim().is_empty())
        {
            return Err(LyricsParseError::MalformedResponse);
        }
        return LyricsDocument::new(LyricsSource::Lrclib, None, Vec::new(), true)
            .map(Some)
            .map_err(|_| LyricsParseError::MalformedResponse);
    }

    let synchronized = record.synced_lyrics.as_deref().unwrap_or_default();
    match parse_lrc(synchronized, record.plain_lyrics.as_deref()) {
        Ok(document) => Ok(Some(document)),
        Err(LyricsParseError::MissingLyrics) => Ok(None),
        Err(error) => Err(error),
    }
}

fn normalize_duration(duration_seconds: Option<f64>) -> Result<Option<u64>, LyricsParseError> {
    let Some(duration_seconds) = duration_seconds else {
        return Ok(None);
    };
    if !duration_seconds.is_finite() || duration_seconds.is_sign_negative() {
        return Err(LyricsParseError::MalformedResponse);
    }
    let duration = std::time::Duration::try_from_secs_f64(duration_seconds)
        .map_err(|_| LyricsParseError::MalformedResponse)?;
    let duration_ms =
        u64::try_from(duration.as_millis()).map_err(|_| LyricsParseError::MalformedResponse)?;
    Ok(Some(duration_ms))
}

fn artist_matches(candidate: &str, request: &PreparedMatch) -> bool {
    if normalize_match_text(candidate).as_deref() == Some(request.primary_artist.as_str()) {
        return true;
    }
    if request.artists.len() < 2 || !candidate.contains([',', ';']) {
        return false;
    }

    let components = candidate
        .split([',', ';'])
        .map(normalize_match_text)
        .collect::<Option<Vec<_>>>();
    let Some(mut components) = components else {
        return false;
    };
    if components.len() < 2 || components.iter().any(String::is_empty) {
        return false;
    }
    let original_len = components.len();
    components.sort_unstable();
    components.dedup();
    components.len() == original_len && components == request.artists
}

fn normalize_match_text(value: &str) -> Option<String> {
    if value.len() > MAX_LYRICS_METADATA_BYTES {
        return None;
    }
    let mut normalized = String::new();
    let mut separator_pending = false;
    for character in value.trim().nfc().flat_map(char::to_lowercase) {
        if character.is_alphanumeric() || (is_combining_mark(character) && !normalized.is_empty()) {
            if separator_pending && !normalized.is_empty() {
                normalized.push(' ');
            }
            if normalized.len().saturating_add(character.len_utf8()) > MAX_LYRICS_METADATA_BYTES {
                return None;
            }
            normalized.push(character);
            separator_pending = false;
        } else if character != '\'' && character != '\u{2019}' {
            separator_pending = !normalized.is_empty();
        }
    }
    Some(normalized)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LyricsSource {
    YouTubeMusic,
    Lrclib,
}

/// A presentation-only cross-fade between adjacent synchronized lyric lines.
///
/// Progress is expressed in thousandths so rendering remains deterministic and
/// does not need its own timer or mutable animation state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LyricTransition {
    outgoing_index: Option<usize>,
    incoming_index: usize,
    progress_millis: u16,
}

impl LyricTransition {
    #[must_use]
    pub const fn outgoing_index(self) -> Option<usize> {
        self.outgoing_index
    }

    #[must_use]
    pub const fn incoming_index(self) -> usize {
        self.incoming_index
    }

    #[must_use]
    pub const fn progress_millis(self) -> u16 {
        self.progress_millis
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct TimedLyricLine {
    start_ms: u64,
    end_ms: Option<u64>,
    text: String,
}

impl TimedLyricLine {
    /// Creates a bounded synchronized lyric line.
    ///
    /// # Errors
    ///
    /// Returns an error when text is empty or oversized, or when the optional
    /// end precedes the start.
    pub fn new(start_ms: u64, end_ms: Option<u64>, text: &str) -> Result<Self, LyricsError> {
        let text = text.trim();
        if text.len() > MAX_TIMED_LYRIC_LINE_BYTES {
            return Err(LyricsError::TextTooLong {
                field: "timed.text",
                bytes: text.len(),
                limit: MAX_TIMED_LYRIC_LINE_BYTES,
            });
        }
        let text = normalize_timed_lyric_text(text);
        let text = text.trim();
        if text.is_empty() {
            return Err(LyricsError::EmptyText {
                field: "timed.text",
            });
        }
        if end_ms.is_some_and(|end_ms| end_ms < start_ms) {
            return Err(LyricsError::InvalidTimestampRange { start_ms, end_ms });
        }

        Ok(Self {
            start_ms,
            end_ms,
            text: text.to_owned(),
        })
    }

    #[must_use]
    pub const fn start_ms(&self) -> u64 {
        self.start_ms
    }

    #[must_use]
    pub const fn end_ms(&self) -> Option<u64> {
        self.end_ms
    }

    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }
}

impl fmt::Debug for TimedLyricLine {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TimedLyricLine")
            .field("start_ms", &self.start_ms)
            .field("end_ms", &self.end_ms)
            .field("text_bytes", &self.text.len())
            .finish()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct LyricsDocument {
    source: LyricsSource,
    plain: Option<String>,
    timed: Vec<TimedLyricLine>,
    instrumental: bool,
}

impl LyricsDocument {
    /// Creates a bounded normalized lyrics document.
    ///
    /// # Errors
    ///
    /// Returns an error when fallback and instrumental semantics conflict, or
    /// when text, line count, or timestamp ordering exceeds model boundaries.
    pub fn new(
        source: LyricsSource,
        plain: Option<String>,
        timed: Vec<TimedLyricLine>,
        instrumental: bool,
    ) -> Result<Self, LyricsError> {
        let plain = plain
            .map(|plain| {
                let plain = plain.trim();
                if plain.len() > MAX_LYRICS_TEXT_BYTES {
                    return Err(LyricsError::TextTooLong {
                        field: "plain",
                        bytes: plain.len(),
                        limit: MAX_LYRICS_TEXT_BYTES,
                    });
                }
                let plain = normalize_lyrics_text(plain);
                let plain = plain.trim();
                if plain.is_empty() {
                    Err(LyricsError::EmptyText { field: "plain" })
                } else {
                    Ok(plain.to_owned())
                }
            })
            .transpose()?;

        if instrumental && (plain.is_some() || !timed.is_empty()) {
            return Err(LyricsError::InstrumentalHasLyrics);
        }
        if !instrumental && plain.is_none() && timed.is_empty() {
            return Err(LyricsError::MissingLyrics);
        }
        if timed.len() > MAX_TIMED_LYRIC_LINES {
            return Err(LyricsError::TooManyTimedLines {
                lines: timed.len(),
                limit: MAX_TIMED_LYRIC_LINES,
            });
        }

        for (index, pair) in timed.windows(2).enumerate() {
            if pair[0].start_ms >= pair[1].start_ms {
                return Err(LyricsError::TimedLinesOutOfOrder {
                    previous_index: index,
                    next_index: index + 1,
                });
            }
        }

        let total_text_bytes = plain.as_ref().map_or(0, String::len).saturating_add(
            timed
                .iter()
                .map(|line| line.text.len())
                .fold(0_usize, usize::saturating_add),
        );
        if total_text_bytes > MAX_LYRICS_TEXT_BYTES {
            return Err(LyricsError::TotalTextTooLong {
                bytes: total_text_bytes,
                limit: MAX_LYRICS_TEXT_BYTES,
            });
        }

        Ok(Self {
            source,
            plain,
            timed,
            instrumental,
        })
    }

    #[must_use]
    pub const fn source(&self) -> LyricsSource {
        self.source
    }

    #[must_use]
    pub fn plain(&self) -> Option<&str> {
        self.plain.as_deref()
    }

    #[must_use]
    pub fn timed(&self) -> &[TimedLyricLine] {
        &self.timed
    }

    #[must_use]
    pub const fn instrumental(&self) -> bool {
        self.instrumental
    }

    #[must_use]
    pub fn active_line(&self, position_ms: u64) -> Option<&TimedLyricLine> {
        let candidate_index = self
            .timed
            .partition_point(|line| line.start_ms <= position_ms)
            .checked_sub(1)?;
        let candidate = &self.timed[candidate_index];

        candidate
            .end_ms
            .is_none_or(|end_ms| position_ms < end_ms)
            .then_some(candidate)
    }

    /// Derives the synchronized-line cross-fade directly from playback time.
    ///
    /// A transition begins inclusively at the incoming line timestamp and is
    /// settled inclusively at the end of its window. The window is capped at
    /// 400 ms and at half of each finite neighboring line duration, preventing
    /// fades from overlapping even for very short lines.
    #[must_use]
    pub fn transition_at(&self, position_ms: u64) -> Option<LyricTransition> {
        const MAX_FADE_MILLIS: u64 = 400;
        const COMPLETE: u16 = 1_000;

        let incoming_index = self
            .timed
            .partition_point(|line| line.start_ms <= position_ms)
            .checked_sub(1)?;
        let incoming = &self.timed[incoming_index];
        if incoming.end_ms.is_some_and(|end_ms| position_ms >= end_ms) {
            return None;
        }

        let finite_duration = |index: usize| {
            let line = &self.timed[index];
            // A later explicit end must never extend through the next line,
            // while an earlier explicit end still shortens the usable span.
            self.timed
                .get(index + 1)
                .map(|next| {
                    line.end_ms
                        .map_or(next.start_ms, |end_ms| end_ms.min(next.start_ms))
                })
                .or(line.end_ms)
                .map(|end_ms| end_ms.saturating_sub(line.start_ms))
        };
        let mut window_ms = MAX_FADE_MILLIS;
        if incoming_index > 0
            && let Some(previous_duration) = finite_duration(incoming_index - 1)
        {
            window_ms = window_ms.min(previous_duration / 2);
        }
        if let Some(incoming_duration) = finite_duration(incoming_index) {
            window_ms = window_ms.min(incoming_duration / 2);
        }

        let elapsed_ms = position_ms.saturating_sub(incoming.start_ms);
        if window_ms == 0 || elapsed_ms >= window_ms {
            return Some(LyricTransition {
                outgoing_index: None,
                incoming_index,
                progress_millis: COMPLETE,
            });
        }
        let progress_millis = u16::try_from(
            u128::from(elapsed_ms).saturating_mul(u128::from(COMPLETE)) / u128::from(window_ms),
        )
        .unwrap_or(COMPLETE);
        let outgoing_index = incoming_index.checked_sub(1).filter(|previous_index| {
            self.timed[*previous_index]
                .end_ms
                .is_none_or(|end_ms| end_ms >= incoming.start_ms)
        });
        Some(LyricTransition {
            outgoing_index,
            incoming_index,
            progress_millis,
        })
    }
}

impl fmt::Debug for LyricsDocument {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text_bytes = self.plain.as_ref().map_or(0, String::len).saturating_add(
            self.timed
                .iter()
                .map(|line| line.text.len())
                .fold(0_usize, usize::saturating_add),
        );

        formatter
            .debug_struct("LyricsDocument")
            .field("has_plain", &self.plain.is_some())
            .field("timed_lines", &self.timed.len())
            .field("instrumental", &self.instrumental)
            .field("text_bytes", &text_bytes)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum LyricsError {
    #[error("{field} must contain non-whitespace text")]
    EmptyText { field: &'static str },
    #[error("{field} contains {bytes} bytes; limit is {limit}")]
    TextTooLong {
        field: &'static str,
        bytes: usize,
        limit: usize,
    },
    #[error("timed lyric end must not precede its start")]
    InvalidTimestampRange { start_ms: u64, end_ms: Option<u64> },
    #[error("instrumental lyrics document must not contain lyric text")]
    InstrumentalHasLyrics,
    #[error("non-instrumental lyrics document requires plain or timed lyrics")]
    MissingLyrics,
    #[error("lyrics document contains {lines} timed lines; limit is {limit}")]
    TooManyTimedLines { lines: usize, limit: usize },
    #[error("timed lyrics are not strictly ordered")]
    TimedLinesOutOfOrder {
        previous_index: usize,
        next_index: usize,
    },
    #[error("lyrics document contains {bytes} text bytes; limit is {limit}")]
    TotalTextTooLong { bytes: usize, limit: usize },
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        error::Error,
        sync::{
            Arc, Mutex,
            atomic::{AtomicU64, AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use async_trait::async_trait;

    use crate::{
        domain::{ChartSection, MediaId, MediaItem, MediaKind, RegionCode, SearchFilter},
        provider::{
            AuthenticationState, LibraryItem, LibrarySection, MusicProvider, Page, PlainLyrics,
            Podcast, ProviderError, ProviderErrorKind, ProviderOperation, ProviderResult,
            SearchItem,
        },
    };

    use super::{
        LrclibClient, LrclibClock, LrclibHttpRequest, LrclibHttpResponse, LrclibMatchRequest,
        LrclibSourceError, LrclibTransport, LyricsDocument, LyricsError, LyricsParseError,
        LyricsSource, LyricsSourceService, MAX_LRCLIB_FALLBACK_REQUESTS, MAX_LRCLIB_RESULTS,
        MAX_LYRICS_METADATA_BYTES, MAX_LYRICS_RESPONSE_BYTES, MAX_LYRICS_TEXT_BYTES,
        MAX_TIMED_LYRIC_LINE_BYTES, MAX_TIMED_LYRIC_LINES, TimedLyricLine, fallback_title_variants,
        match_lrclib_fallback_response, match_lrclib_response,
        match_lrclib_verified_fallback_response, parse_lrc,
    };

    const MARCY_FALLBACK_RESPONSE: &str = r#"[
      {"trackName":"ラブソング","artistName":"OKAMOTO'S","albumName":"OKAMOTO'S",
       "duration":257.114558,"plainLyrics":"wrong","syncedLyrics":"[00:06.03]wrong"},
      {"trackName":"ラブソング","artistName":"マルシィ","albumName":"Marcy -Sweet and Bitter-",
       "duration":256.0,"plainLyrics":"right","syncedLyrics":"[00:18.50]right"}
    ]"#;
    const NANCY_FALLBACK_RESPONSE: &str = r#"[{"id":35078027,"trackName":"How Could You","artistName":"Sanchez","albumName":"Stays on My Mind","duration":238.0,"plainLyrics":"wrong","syncedLyrics":"[00:47.52]Why couldn't you just realize"}]"#;

    #[test]
    fn fallback_title_variants_extract_bounded_exact_bilingual_segments() {
        assert_eq!(
            fallback_title_variants("與我無關 - Not My Problem"),
            vec!["與我無關", "Not My Problem"]
        );
        assert_eq!(
            fallback_title_variants("Original — Translation – Alternate - Fourth"),
            vec!["Original", "Translation", "Alternate"]
        );
    }

    #[test]
    fn fallback_title_variants_reject_unsafe_or_redundant_segments() {
        assert!(fallback_title_variants("Unsplittable title").is_empty());
        assert!(fallback_title_variants("Unspaced-Separator").is_empty());
        assert_eq!(fallback_title_variants("A - Valid - valid"), vec!["Valid"]);
        assert!(fallback_title_variants("Valid - ").is_empty());
        assert!(fallback_title_variants(&"x".repeat(MAX_LYRICS_METADATA_BYTES + 1)).is_empty());
        assert!(
            fallback_title_variants("one - two - three - four").len()
                <= MAX_LRCLIB_FALLBACK_REQUESTS
        );
    }

    #[test]
    fn lrclib_fallback_accepts_unique_exact_segment_and_duration_with_artist_alias()
    -> Result<(), Box<dyn Error>> {
        let request = LrclibMatchRequest::new(
            "與我無關 - Not My Problem",
            "MC Cheung Tinfu",
            &["MC Cheung Tinfu"],
            Some(205_000),
        );
        let response = r#"[{
            "id":9979227,
            "trackName":"與我無關",
            "artistName":"MC 張天賦",
            "albumName":"與我無關",
            "duration":205.0,
            "plainLyrics":"你 應該都不再認得 舊年",
            "syncedLyrics":"[00:17.49]你 應該都不再認得 舊年"
        }]"#
        .as_bytes();

        let document = match_lrclib_fallback_response(response, "與我無關", &request)?
            .ok_or("expected multilingual fallback")?;
        assert_eq!(document.source(), LyricsSource::Lrclib);
        assert_eq!(document.timed()[0].start_ms(), 17_490);
        Ok(())
    }

    #[test]
    fn lrclib_verified_full_title_rejects_unique_unverified_duration_match()
    -> Result<(), Box<dyn Error>> {
        let request = LrclibMatchRequest::new(
            "How could you?",
            "Nancy Kwai",
            &["Nancy Kwai"],
            Some(236_000),
        );

        assert!(
            match_lrclib_verified_fallback_response(
                NANCY_FALLBACK_RESPONSE.as_bytes(),
                "How could you?",
                &request,
            )?
            .is_none()
        );
        Ok(())
    }

    #[test]
    fn lrclib_verified_full_title_prefers_artist_verified_candidate_in_any_order()
    -> Result<(), Box<dyn Error>> {
        let request = lrclib_fallback_request(None);
        let response = br#"[
          {"trackName":"Original","artistName":"Different Artist","duration":205.0,
           "plainLyrics":"unverified","syncedLyrics":"[00:01]unverified"},
          {"trackName":"Original","artistName":"Request Artist","duration":206.5,
           "plainLyrics":"verified","syncedLyrics":null}
        ]"#;
        let mut reversed = serde_json::from_slice::<serde_json::Value>(response)?;
        reversed
            .as_array_mut()
            .ok_or("expected response array")?
            .reverse();
        let reversed = serde_json::to_vec(&reversed)?;

        for response in [response.as_slice(), reversed.as_slice()] {
            let matched = match_lrclib_verified_fallback_response(response, "Original", &request)?
                .ok_or("expected artist-verified candidate")?;
            assert_eq!(matched.plain(), Some("verified"));
        }
        Ok(())
    }

    fn lrclib_fallback_request(collection: Option<&str>) -> LrclibMatchRequest<'_> {
        LrclibMatchRequest::new(
            "Original - Translation",
            "Request Artist",
            &["Request Artist"],
            Some(205_000),
        )
        .with_collection(collection)
    }

    #[test]
    fn lrclib_fallback_rejects_partial_title() -> Result<(), Box<dyn Error>> {
        let response = br#"[{
            "trackName":"Original Song","artistName":"Different Artist","duration":205.0,
            "plainLyrics":"must reject","syncedLyrics":"[00:01]must reject"
        }]"#;

        assert!(
            match_lrclib_fallback_response(response, "Original", &lrclib_fallback_request(None))?
                .is_none()
        );
        Ok(())
    }

    #[test]
    fn lrclib_fallback_rejects_missing_or_over_tolerance_duration() -> Result<(), Box<dyn Error>> {
        let missing_candidate = br#"[{
            "trackName":"Original","artistName":"Different Artist","duration":null,
            "plainLyrics":"must reject","syncedLyrics":null
        }]"#;
        let over_tolerance = br#"[{
            "trackName":"Original","artistName":"Different Artist","duration":207.001,
            "plainLyrics":"must reject","syncedLyrics":null
        }]"#;
        let unknown_request = LrclibMatchRequest::new(
            "Original - Translation",
            "Request Artist",
            &["Request Artist"],
            None,
        );
        let known_candidate = br#"[{
            "trackName":"Original","artistName":"Different Artist","duration":205.0,
            "plainLyrics":"must reject","syncedLyrics":null
        }]"#;

        for (response, request) in [
            (missing_candidate.as_slice(), lrclib_fallback_request(None)),
            (over_tolerance.as_slice(), lrclib_fallback_request(None)),
            (known_candidate.as_slice(), unknown_request),
        ] {
            assert!(match_lrclib_fallback_response(response, "Original", &request)?.is_none());
        }
        Ok(())
    }

    #[test]
    fn lrclib_fallback_rejects_distinct_equal_ranked_documents() {
        let response = br#"[
          {"trackName":"Original","artistName":"Different Artist","duration":205.0,
           "plainLyrics":"one","syncedLyrics":"[00:01]one"},
          {"trackName":"Original","artistName":"Different Artist","duration":205.0,
           "plainLyrics":"two","syncedLyrics":"[00:01]two"}
        ]"#;

        assert_eq!(
            match_lrclib_fallback_response(response, "Original", &lrclib_fallback_request(None)),
            Err(LyricsParseError::AmbiguousMatch)
        );
    }

    #[test]
    fn lrclib_fallback_rejects_competing_unverified_duration_matches_in_any_order()
    -> Result<(), Box<dyn Error>> {
        let request =
            LrclibMatchRequest::new("ラブソング - Love Song", "Marcy", &["Marcy"], Some(257_000))
                .with_collection(Some("Love Song"));
        let mut reversed = serde_json::from_str::<serde_json::Value>(MARCY_FALLBACK_RESPONSE)?;
        reversed
            .as_array_mut()
            .ok_or("expected response array")?
            .reverse();
        let reversed = serde_json::to_vec(&reversed)?;

        for response in [MARCY_FALLBACK_RESPONSE.as_bytes(), reversed.as_slice()] {
            assert_eq!(
                match_lrclib_fallback_response(response, "ラブソング", &request),
                Err(LyricsParseError::AmbiguousMatch)
            );
        }
        Ok(())
    }

    #[test]
    fn lrclib_fallback_does_not_verify_empty_normalized_album_identity() {
        let response = br#"[
          {"trackName":"Original","artistName":"Different Artist","albumName":"---",
           "duration":205.0,"plainLyrics":"one","syncedLyrics":"[00:01]one"},
          {"trackName":"Original","artistName":"Another Artist","albumName":"!!!",
           "duration":205.5,"plainLyrics":"two","syncedLyrics":"[00:01]two"}
        ]"#;
        let request = lrclib_fallback_request(Some("..."));

        assert_eq!(
            match_lrclib_fallback_response(response, "Original", &request),
            Err(LyricsParseError::AmbiguousMatch)
        );
    }

    #[test]
    fn lrclib_fallback_verified_candidate_resolves_unverified_conflicts()
    -> Result<(), Box<dyn Error>> {
        let response = br#"[
          {"trackName":"Original","artistName":"Different Artist","duration":205.0,
           "plainLyrics":"unverified one","syncedLyrics":"[00:01]unverified one"},
          {"trackName":"Original","artistName":"Another Artist","duration":205.1,
           "plainLyrics":"unverified two","syncedLyrics":"[00:01]unverified two"},
          {"trackName":"Original","artistName":"Request Artist","duration":206.5,
           "plainLyrics":"verified","syncedLyrics":null}
        ]"#;

        let matched =
            match_lrclib_fallback_response(response, "Original", &lrclib_fallback_request(None))?
                .ok_or("expected verified candidate")?;
        assert_eq!(matched.plain(), Some("verified"));
        Ok(())
    }

    #[test]
    fn lrclib_fallback_collapses_identical_unverified_documents_across_duration_ranks()
    -> Result<(), Box<dyn Error>> {
        let response = br#"[
          {"trackName":"Original","artistName":"Different Artist","duration":205.0,
           "plainLyrics":"same","syncedLyrics":"[00:01]same"},
          {"trackName":"Original","artistName":"Another Artist","duration":206.5,
           "plainLyrics":"same","syncedLyrics":"[00:01]same"}
        ]"#;

        let matched =
            match_lrclib_fallback_response(response, "Original", &lrclib_fallback_request(None))?
                .ok_or("expected identical fallback documents to collapse")?;
        assert_eq!(matched.timed()[0].text(), "same");
        Ok(())
    }

    #[test]
    fn lrclib_fallback_collapses_identical_equal_ranked_documents() -> Result<(), Box<dyn Error>> {
        let response = br#"[
          {"trackName":"Original","artistName":"Different Artist","albumName":"One",
           "duration":205.0,"plainLyrics":"same","syncedLyrics":"[00:01]same"},
          {"trackName":"Original","artistName":"Different Artist","albumName":"Two",
           "duration":205.0,"plainLyrics":"same","syncedLyrics":"[00:01]same"}
        ]"#;

        let matched =
            match_lrclib_fallback_response(response, "Original", &lrclib_fallback_request(None))?
                .ok_or("expected identical fallback documents to collapse")?;
        assert_eq!(matched.timed()[0].text(), "same");
        Ok(())
    }

    #[test]
    fn lrclib_fallback_bounds_body_metadata_results_and_lyric_text() {
        let request = lrclib_fallback_request(None);
        let oversized_body = vec![b' '; MAX_LYRICS_RESPONSE_BYTES + 1];
        assert_eq!(
            match_lrclib_fallback_response(&oversized_body, "Original", &request),
            Err(LyricsParseError::ResponseTooLarge)
        );

        let oversized_metadata = "x".repeat(MAX_LYRICS_METADATA_BYTES + 1);
        let response = format!(
            r#"[{{"trackName":"Original","artistName":"{oversized_metadata}","duration":205.0,"plainLyrics":"plain","syncedLyrics":null}}]"#
        );
        assert_eq!(
            match_lrclib_fallback_response(response.as_bytes(), "Original", &request),
            Err(LyricsParseError::LyricsTooLarge)
        );

        let too_many = format!(
            "[{}]",
            std::iter::repeat_n(
                r#"{"trackName":"no","artistName":"no","duration":205.0,"plainLyrics":null,"syncedLyrics":null}"#,
                MAX_LRCLIB_RESULTS + 1
            )
            .collect::<Vec<_>>()
            .join(",")
        );
        assert_eq!(
            match_lrclib_fallback_response(too_many.as_bytes(), "Original", &request),
            Err(LyricsParseError::TooManyResults)
        );

        let oversized_lyrics = "x".repeat(MAX_LYRICS_TEXT_BYTES + 1);
        let response = format!(
            r#"[{{"trackName":"Original","artistName":"Different Artist","duration":205.0,"plainLyrics":"{oversized_lyrics}","syncedLyrics":null}}]"#
        );
        assert_eq!(
            match_lrclib_fallback_response(response.as_bytes(), "Original", &request),
            Err(LyricsParseError::LyricsTooLarge)
        );
    }

    #[test]
    fn lrclib_fallback_prefers_exact_artist_before_album_sync_and_duration()
    -> Result<(), Box<dyn Error>> {
        let response = br#"[
          {"trackName":"Original","artistName":"Different Artist","albumName":"Request Album",
           "duration":205.0,"plainLyrics":"lower","syncedLyrics":"[00:01]lower"},
          {"trackName":"Original","artistName":"Request Artist","albumName":"Other Album",
           "duration":206.5,"plainLyrics":"exact artist","syncedLyrics":null}
        ]"#;

        let matched = match_lrclib_fallback_response(
            response,
            "Original",
            &lrclib_fallback_request(Some("Request Album")),
        )?
        .ok_or("expected exact artist candidate")?;
        assert_eq!(matched.plain(), Some("exact artist"));
        assert!(matched.timed().is_empty());
        Ok(())
    }

    #[test]
    fn lrclib_fallback_prefers_exact_album_before_sync_and_duration() -> Result<(), Box<dyn Error>>
    {
        let response = br#"[
          {"trackName":"Original","artistName":"Different Artist","albumName":"Other Album",
           "duration":205.0,"plainLyrics":"lower","syncedLyrics":"[00:01]lower"},
          {"trackName":"Original","artistName":"Different Artist","albumName":"Request Album",
           "duration":206.5,"plainLyrics":"exact album","syncedLyrics":null}
        ]"#;

        let matched = match_lrclib_fallback_response(
            response,
            "Original",
            &lrclib_fallback_request(Some("Request Album")),
        )?
        .ok_or("expected exact album candidate")?;
        assert_eq!(matched.plain(), Some("exact album"));
        assert!(matched.timed().is_empty());
        Ok(())
    }

    #[test]
    fn lrclib_fallback_prefers_synchronized_lyrics_before_closer_duration()
    -> Result<(), Box<dyn Error>> {
        let response = br#"[
          {"trackName":"Original","artistName":"Request Artist","duration":205.0,
           "plainLyrics":"closer plain","syncedLyrics":null},
          {"trackName":"Original","artistName":"Request Artist","duration":206.5,
           "plainLyrics":"timed","syncedLyrics":"[00:01]timed"}
        ]"#;

        let matched =
            match_lrclib_fallback_response(response, "Original", &lrclib_fallback_request(None))?
                .ok_or("expected synchronized fallback candidate")?;
        assert_eq!(matched.timed()[0].text(), "timed");
        Ok(())
    }

    #[test]
    fn lrclib_fallback_prefers_closest_duration_after_metadata_and_content()
    -> Result<(), Box<dyn Error>> {
        let response = br#"[
          {"trackName":"Original","artistName":"Request Artist","duration":206.5,
           "plainLyrics":"farther","syncedLyrics":"[00:01]farther"},
          {"trackName":"Original","artistName":"Request Artist","duration":205.25,
           "plainLyrics":"closer","syncedLyrics":"[00:01]closer"}
        ]"#;

        let matched =
            match_lrclib_fallback_response(response, "Original", &lrclib_fallback_request(None))?
                .ok_or("expected closest-duration fallback candidate")?;
        assert_eq!(matched.timed()[0].text(), "closer");
        Ok(())
    }

    #[test]
    fn fallback_title_variants_reject_punctuation_reduced_one_character_segments() {
        assert_eq!(fallback_title_variants("A! - Valid"), vec!["Valid"]);
    }

    #[test]
    fn fallback_title_variants_reject_nfc_collapsed_one_character_segments() {
        assert_eq!(fallback_title_variants("A\u{301} - Valid"), vec!["Valid"]);
    }

    #[test]
    fn fallback_title_variants_reject_lowercase_expanded_one_character_segments() {
        assert_eq!(fallback_title_variants("İ - Valid"), vec!["Valid"]);
    }

    #[test]
    fn fallback_title_variants_reject_combining_mark_padded_one_character_segments() {
        assert_eq!(fallback_title_variants("i\u{307} - Valid"), vec!["Valid"]);
    }

    fn lyric_fade_document() -> LyricsDocument {
        LyricsDocument::new(
            LyricsSource::Lrclib,
            None,
            vec![
                TimedLyricLine::new(1_000, Some(2_000), "first")
                    .unwrap_or_else(|error| panic!("lyric fade fixture: {error}")),
                TimedLyricLine::new(2_000, Some(3_000), "second")
                    .unwrap_or_else(|error| panic!("lyric fade fixture: {error}")),
                TimedLyricLine::new(3_000, None, "final")
                    .unwrap_or_else(|error| panic!("lyric fade fixture: {error}")),
            ],
            false,
        )
        .unwrap_or_else(|error| panic!("lyric fade document: {error}"))
    }

    #[test]
    fn lyric_fade_transition_boundaries_are_inclusive_and_text_free() {
        let document = lyric_fade_document();
        assert_eq!(document.transition_at(999), None);
        let first_start = document
            .transition_at(1_000)
            .unwrap_or_else(|| panic!("first transition at incoming timestamp"));
        assert_eq!(first_start.outgoing_index(), None);
        assert_eq!(first_start.incoming_index(), 0);
        assert_eq!(first_start.progress_millis(), 0);

        let start = document
            .transition_at(2_000)
            .unwrap_or_else(|| panic!("transition at incoming timestamp"));
        assert_eq!(start.outgoing_index(), Some(0));
        assert_eq!(start.incoming_index(), 1);
        assert_eq!(start.progress_millis(), 0);

        let midpoint = document
            .transition_at(2_200)
            .unwrap_or_else(|| panic!("transition at midpoint"));
        assert_eq!(midpoint.outgoing_index(), Some(0));
        assert_eq!(midpoint.incoming_index(), 1);
        assert_eq!(midpoint.progress_millis(), 500);

        let settled = document
            .transition_at(2_400)
            .unwrap_or_else(|| panic!("transition at settled boundary"));
        assert_eq!(settled.outgoing_index(), None);
        assert_eq!(settled.incoming_index(), 1);
        assert_eq!(settled.progress_millis(), 1_000);
        let debug = format!("{start:?}");
        assert!(!debug.contains("first") && !debug.contains("second"));
    }

    #[test]
    fn lyric_fade_short_and_open_ended_lines_have_non_overlapping_windows() {
        let document = LyricsDocument::new(
            LyricsSource::Lrclib,
            None,
            vec![
                TimedLyricLine::new(0, Some(100), "zero")
                    .unwrap_or_else(|error| panic!("short lyric fade fixture: {error}")),
                TimedLyricLine::new(100, Some(200), "one")
                    .unwrap_or_else(|error| panic!("short lyric fade fixture: {error}")),
                TimedLyricLine::new(200, None, "two")
                    .unwrap_or_else(|error| panic!("short lyric fade fixture: {error}")),
            ],
            false,
        )
        .unwrap_or_else(|error| panic!("short lyric fade document: {error}"));

        assert_eq!(
            document
                .transition_at(125)
                .unwrap_or_else(|| panic!("short transition midpoint"))
                .progress_millis(),
            500
        );
        assert_eq!(
            document
                .transition_at(150)
                .unwrap_or_else(|| panic!("short transition boundary"))
                .progress_millis(),
            1_000
        );
        let final_start = document
            .transition_at(200)
            .unwrap_or_else(|| panic!("open final transition start"));
        assert_eq!(final_start.outgoing_index(), Some(1));
        assert_eq!(final_start.progress_millis(), 0);
        assert_eq!(
            document
                .transition_at(250)
                .unwrap_or_else(|| panic!("open final transition boundary"))
                .progress_millis(),
            1_000
        );
        assert_eq!(
            document
                .transition_at(u64::MAX)
                .unwrap_or_else(|| panic!("open final remains active"))
                .incoming_index(),
            2
        );
    }

    #[test]
    fn lyric_fade_uses_adjacent_starts_when_explicit_ends_overlap_later_lines() {
        let document = LyricsDocument::new(
            LyricsSource::Lrclib,
            None,
            vec![
                TimedLyricLine::new(0, Some(1_000), "zero")
                    .unwrap_or_else(|error| panic!("overlap lyric fade fixture: {error}")),
                TimedLyricLine::new(100, Some(1_000), "one")
                    .unwrap_or_else(|error| panic!("overlap lyric fade fixture: {error}")),
                TimedLyricLine::new(200, None, "two")
                    .unwrap_or_else(|error| panic!("overlap lyric fade fixture: {error}")),
            ],
            false,
        )
        .unwrap_or_else(|error| panic!("overlap lyric fade document: {error}"));

        assert_eq!(
            document
                .transition_at(125)
                .unwrap_or_else(|| panic!("overlap transition midpoint"))
                .progress_millis(),
            500
        );
        let second_settled = document
            .transition_at(150)
            .unwrap_or_else(|| panic!("overlap transition settled boundary"));
        assert_eq!(second_settled.outgoing_index(), None);
        assert_eq!(second_settled.progress_millis(), 1_000);

        let final_settled = document
            .transition_at(250)
            .unwrap_or_else(|| panic!("overlap final settled boundary"));
        assert_eq!(final_settled.incoming_index(), 2);
        assert_eq!(final_settled.outgoing_index(), None);
        assert_eq!(final_settled.progress_millis(), 1_000);
    }

    #[test]
    fn lyric_fade_keeps_earlier_explicit_ends_as_duration_bounds() {
        let document = LyricsDocument::new(
            LyricsSource::Lrclib,
            None,
            vec![
                TimedLyricLine::new(0, Some(40), "zero")
                    .unwrap_or_else(|error| panic!("gapped lyric fade fixture: {error}")),
                TimedLyricLine::new(100, Some(140), "one")
                    .unwrap_or_else(|error| panic!("gapped lyric fade fixture: {error}")),
                TimedLyricLine::new(200, None, "two")
                    .unwrap_or_else(|error| panic!("gapped lyric fade fixture: {error}")),
            ],
            false,
        )
        .unwrap_or_else(|error| panic!("gapped lyric fade document: {error}"));

        let start = document
            .transition_at(100)
            .unwrap_or_else(|| panic!("gapped transition start"));
        assert_eq!(start.incoming_index(), 1);
        assert_eq!(start.outgoing_index(), None);
        assert_eq!(start.progress_millis(), 0);

        assert_eq!(
            document
                .transition_at(110)
                .unwrap_or_else(|| panic!("gapped transition midpoint"))
                .progress_millis(),
            500
        );
        assert_eq!(
            document
                .transition_at(120)
                .unwrap_or_else(|| panic!("gapped transition settled boundary"))
                .progress_millis(),
            1_000
        );
    }

    #[derive(Clone, Default)]
    struct SourceTransport {
        requests: Arc<Mutex<Vec<LrclibHttpRequest>>>,
        response: Arc<Mutex<Option<LrclibHttpResponse>>>,
    }

    impl SourceTransport {
        fn returning(response: LrclibHttpResponse) -> Self {
            Self {
                response: Arc::new(Mutex::new(Some(response))),
                ..Self::default()
            }
        }

        fn requests(&self) -> Vec<LrclibHttpRequest> {
            self.requests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }
    }

    #[async_trait]
    impl LrclibTransport for SourceTransport {
        async fn get(
            &self,
            request: LrclibHttpRequest,
        ) -> Result<LrclibHttpResponse, LrclibSourceError> {
            self.requests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(request);
            self.response
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
                .ok_or(LrclibSourceError::Unavailable)
        }
    }

    #[derive(Clone, Default)]
    struct SequencedSourceTransport {
        requests: Arc<Mutex<Vec<LrclibHttpRequest>>>,
        responses: Arc<Mutex<VecDeque<Result<LrclibHttpResponse, LrclibSourceError>>>>,
    }

    impl SequencedSourceTransport {
        fn new(
            responses: impl IntoIterator<Item = Result<LrclibHttpResponse, LrclibSourceError>>,
        ) -> Self {
            Self {
                requests: Arc::new(Mutex::new(Vec::new())),
                responses: Arc::new(Mutex::new(responses.into_iter().collect())),
            }
        }

        fn from_responses(responses: impl IntoIterator<Item = LrclibHttpResponse>) -> Self {
            Self::new(responses.into_iter().map(Ok))
        }

        fn requests(&self) -> Vec<LrclibHttpRequest> {
            self.requests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }
    }

    #[async_trait]
    impl LrclibTransport for SequencedSourceTransport {
        async fn get(
            &self,
            request: LrclibHttpRequest,
        ) -> Result<LrclibHttpResponse, LrclibSourceError> {
            self.requests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(request);
            self.responses
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pop_front()
                .unwrap_or(Err(LrclibSourceError::Unavailable))
        }
    }

    #[derive(Clone, Default)]
    struct SourceClock(Arc<AtomicU64>);

    impl LrclibClock for SourceClock {
        fn now_ms(&self) -> u64 {
            self.0.load(Ordering::SeqCst)
        }
    }

    impl SourceClock {
        fn set(&self, now_ms: u64) {
            self.0.store(now_ms, Ordering::SeqCst);
        }
    }

    fn source_item(title: &str) -> MediaItem {
        MediaItem {
            id: MediaId {
                provider: "youtube-music".to_owned(),
                video_id: "fixture-video".to_owned(),
            },
            kind: MediaKind::Song,
            title: title.to_owned(),
            creators: vec!["Main Artist".to_owned()],
            collection: None,
            duration_ms: Some(180_000),
            artwork_url: None,
            explicit: false,
        }
    }

    #[tokio::test]
    async fn source_rejects_competing_unverified_duration_matches() {
        let transport = SequencedSourceTransport::from_responses([
            LrclibHttpResponse::new(200, b"[]".to_vec(), false),
            LrclibHttpResponse::new(200, b"[]".to_vec(), false),
            LrclibHttpResponse::new(200, MARCY_FALLBACK_RESPONSE.as_bytes().to_vec(), false),
        ]);
        let client = LrclibClient::with_dependencies(
            Arc::new(transport.clone()),
            Arc::new(SourceClock::default()),
            8,
            60_000,
        );
        let mut item = source_item("ラブソング - Love Song");
        item.id.video_id = "U5rOnKb-kt8".to_owned();
        item.creators = vec!["Marcy".to_owned()];
        item.collection = Some("Love Song".to_owned());
        item.duration_ms = Some(257_000);

        assert_eq!(
            client.fetch(&item).await,
            Err(LrclibSourceError::InvalidResponse)
        );
        let requests = transport.requests();
        assert_eq!(requests.len(), 3);
        assert!(
            requests[2]
                .url()
                .query_pairs()
                .any(|(key, value)| key == "track_name" && value == "ラブソング")
        );
    }

    #[tokio::test]
    async fn source_rejects_unverified_full_title_nancy_match() -> Result<(), Box<dyn Error>> {
        let transport = SequencedSourceTransport::from_responses([
            LrclibHttpResponse::new(200, b"[]".to_vec(), false),
            LrclibHttpResponse::new(200, NANCY_FALLBACK_RESPONSE.as_bytes().to_vec(), false),
        ]);
        let client = LrclibClient::with_dependencies(
            Arc::new(transport.clone()),
            Arc::new(SourceClock::default()),
            8,
            60_000,
        );
        let mut item = source_item("How could you?");
        item.id.video_id = "uY69HlDnkic".to_owned();
        item.creators = vec!["Nancy Kwai".to_owned()];
        item.collection = None;
        item.duration_ms = Some(236_000);

        let document = client.fetch(&item).await?;
        let requests = transport.requests();
        assert_eq!(requests.len(), 2);
        let fallback = requests[1].url().query_pairs().collect::<Vec<_>>();
        assert!(
            fallback
                .iter()
                .any(|(key, value)| { key == "track_name" && value == "How could you?" })
        );
        assert!(!fallback.iter().any(|(key, _)| key == "artist_name"));
        assert!(!fallback.iter().any(|(key, _)| key == "album_name"));
        assert!(document.is_none());
        assert!(client.fetch(&item).await?.is_none());
        assert_eq!(transport.requests().len(), 2);
        Ok(())
    }

    #[tokio::test]
    async fn source_falls_back_to_full_title_for_translated_artist_plain_lyrics()
    -> Result<(), Box<dyn Error>> {
        let record = r#"[{"trackName":"我看見今晚的月色很美，你呢？","artistName":"晚安莉莉","albumName":"Goodnight, Lillie.","duration":259.0,"plainLyrics":"第一行\n第二行","syncedLyrics":null}]"#
            .as_bytes();
        let transport = SequencedSourceTransport::from_responses([
            LrclibHttpResponse::new(200, b"[]".to_vec(), false),
            LrclibHttpResponse::new(200, record.to_vec(), false),
        ]);
        let client = LrclibClient::with_dependencies(
            Arc::new(transport.clone()),
            Arc::new(SourceClock::default()),
            8,
            60_000,
        );
        let mut item = source_item("我看見今晚的月色很美，你呢？");
        item.creators = vec!["Goodnight, Lillie".to_owned()];
        item.collection = Some("Goodnight, Lillie.".to_owned());
        item.duration_ms = Some(259_000);

        let document = client.fetch(&item).await?.ok_or("full-title lyrics")?;

        assert_eq!(document.source(), LyricsSource::Lrclib);
        assert_eq!(document.plain(), Some("第一行\n第二行"));
        assert!(document.timed().is_empty());
        let requests = transport.requests();
        assert_eq!(requests.len(), 2);
        let fallback = requests[1].url().query_pairs().collect::<Vec<_>>();
        assert!(fallback.iter().any(|(key, value)| {
            key == "track_name" && value == "我看見今晚的月色很美，你呢？"
        }));
        assert!(!fallback.iter().any(|(key, _)| key == "artist_name"));
        assert!(!fallback.iter().any(|(key, _)| key == "album_name"));
        Ok(())
    }

    #[tokio::test]
    async fn source_falls_back_to_exact_title_segment_for_lrclib_9979227()
    -> Result<(), Box<dyn Error>> {
        let fallback = r#"[{"trackName":"與我無關","artistName":"MC 張天賦","albumName":"Have A Good Time","duration":205.0,"plainLyrics":"plain","syncedLyrics":"[00:01.00]timed"}]"#;
        let transport = SequencedSourceTransport::from_responses([
            LrclibHttpResponse::new(200, b"[]".to_vec(), false),
            LrclibHttpResponse::new(200, b"[]".to_vec(), false),
            LrclibHttpResponse::new(200, fallback.as_bytes().to_vec(), false),
        ]);
        let client = LrclibClient::with_dependencies(
            Arc::new(transport.clone()),
            Arc::new(SourceClock::default()),
            8,
            60_000,
        );
        let mut item = source_item("與我無關 - Not My Problem");
        item.creators = vec!["MC Cheung Tinfu".to_owned()];
        item.collection = Some("Have A Good Time".to_owned());
        item.duration_ms = Some(205_000);

        let document = client.fetch(&item).await?.ok_or("fallback lyrics")?;

        assert!(!document.timed().is_empty());
        let requests = transport.requests();
        assert_eq!(requests.len(), 3);
        let strict_query = requests[0].url().query_pairs().collect::<Vec<_>>();
        assert!(strict_query.iter().any(|(key, value)| {
            key == "track_name" && value == "與我無關 - Not My Problem"
        }));
        assert!(
            strict_query
                .iter()
                .any(|(key, value)| key == "artist_name" && value == "MC Cheung Tinfu")
        );
        assert!(
            strict_query
                .iter()
                .any(|(key, value)| key == "album_name" && value == "Have A Good Time")
        );
        let full_title_query = requests[1].url().query_pairs().collect::<Vec<_>>();
        assert!(full_title_query.iter().any(|(key, value)| {
            key == "track_name" && value == "與我無關 - Not My Problem"
        }));
        assert!(!full_title_query.iter().any(|(key, _)| key == "artist_name"));
        assert!(!full_title_query.iter().any(|(key, _)| key == "album_name"));
        let fallback_query = requests[2].url().query_pairs().collect::<Vec<_>>();
        assert!(
            fallback_query
                .iter()
                .any(|(key, value)| key == "track_name" && value == "與我無關")
        );
        assert!(!fallback_query.iter().any(|(key, _)| key == "artist_name"));
        assert!(!fallback_query.iter().any(|(key, _)| key == "album_name"));
        assert!(requests.iter().all(|request| {
            request.url().scheme() == "https"
                && request.url().path() == "/api/search"
                && request.user_agent().starts_with("ytermusic/")
                && request.user_agent().contains("github.com")
        }));
        Ok(())
    }

    #[tokio::test]
    async fn source_builds_https_encoded_search_with_identifying_user_agent()
    -> Result<(), Box<dyn Error>> {
        let body = br#"[{"trackName":"Song & More","artistName":"Main Artist","duration":180.0,"plainLyrics":"plain","syncedLyrics":"[00:01.00]timed"}]"#.to_vec();
        let transport = SourceTransport::returning(LrclibHttpResponse::new(200, body, false));
        let client = LrclibClient::with_dependencies(
            Arc::new(transport.clone()),
            Arc::new(SourceClock::default()),
            8,
            60_000,
        );

        let mut item = source_item("Song & More");
        item.collection = Some("Album / Name".to_owned());
        let document = client.fetch(&item).await?;

        assert!(document.is_some());
        let requests = transport.requests();
        assert_eq!(requests.len(), 1);
        let request = requests.first().ok_or("expected one request")?;
        assert_eq!(request.url().scheme(), "https");
        assert_eq!(request.url().path(), "/api/search");
        assert!(request.url().as_str().contains("track_name=Song+%26+More"));
        assert!(request.url().as_str().contains("artist_name=Main+Artist"));
        assert!(request.url().as_str().contains("album_name=Album+%2F+Name"));
        assert!(request.user_agent().starts_with("ytermusic/"));
        assert!(request.user_agent().contains("github.com"));
        Ok(())
    }

    #[tokio::test]
    async fn source_unsplittable_strict_no_match_makes_one_full_title_fallback()
    -> Result<(), Box<dyn Error>> {
        let transport = SequencedSourceTransport::from_responses([
            LrclibHttpResponse::new(200, b"[]".to_vec(), false),
            LrclibHttpResponse::new(200, b"[]".to_vec(), false),
        ]);
        let client = LrclibClient::with_dependencies(
            Arc::new(transport.clone()),
            Arc::new(SourceClock::default()),
            8,
            60_000,
        );

        assert!(
            client
                .fetch(&source_item("Unsplittable title"))
                .await?
                .is_none()
        );
        let requests = transport.requests();
        assert_eq!(requests.len(), 2);
        let fallback = requests[1].url().query_pairs().collect::<Vec<_>>();
        assert!(
            fallback
                .iter()
                .any(|(key, value)| key == "track_name" && value == "Unsplittable title")
        );
        assert!(!fallback.iter().any(|(key, _)| key == "artist_name"));
        assert!(!fallback.iter().any(|(key, _)| key == "album_name"));
        Ok(())
    }

    #[tokio::test]
    async fn source_fallbacks_are_bounded_sequential_and_stop_on_first_match()
    -> Result<(), Box<dyn Error>> {
        let accepted = br#"[{"trackName":"two","artistName":"Alias","duration":180.0,"plainLyrics":"plain","syncedLyrics":"[00:01]timed"}]"#.to_vec();
        let unmatched = br#"[{"trackName":"not one","artistName":"Alias","duration":180.0,"plainLyrics":"plain","syncedLyrics":"[00:01]timed"}]"#.to_vec();
        let transport = SequencedSourceTransport::from_responses([
            LrclibHttpResponse::new(200, b"[]".to_vec(), false),
            LrclibHttpResponse::new(200, b"[]".to_vec(), false),
            LrclibHttpResponse::new(200, unmatched, false),
            LrclibHttpResponse::new(200, accepted, false),
        ]);
        let client = LrclibClient::with_dependencies(
            Arc::new(transport.clone()),
            Arc::new(SourceClock::default()),
            8,
            60_000,
        );

        let document = client
            .fetch(&source_item("one - two - three - four"))
            .await?
            .ok_or("second fallback must match")?;

        assert!(!document.timed().is_empty());
        let requests = transport.requests();
        assert_eq!(requests.len(), 4);
        let fallback_titles = requests[2..]
            .iter()
            .map(|request| {
                request
                    .url()
                    .query_pairs()
                    .find_map(|(key, value)| (key == "track_name").then(|| value.into_owned()))
                    .ok_or("fallback track name")
            })
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(fallback_titles, ["one", "two"]);
        Ok(())
    }

    #[tokio::test]
    async fn source_fallback_count_never_exceeds_configured_cap() -> Result<(), Box<dyn Error>> {
        let responses =
            std::iter::repeat_with(|| LrclibHttpResponse::new(200, b"[]".to_vec(), false))
                .take(2 + MAX_LRCLIB_FALLBACK_REQUESTS);
        let transport = SequencedSourceTransport::from_responses(responses);
        let client = LrclibClient::with_dependencies(
            Arc::new(transport.clone()),
            Arc::new(SourceClock::default()),
            8,
            60_000,
        );

        assert!(
            client
                .fetch(&source_item("one - two - three - four - five"))
                .await?
                .is_none()
        );
        assert_eq!(transport.requests().len(), 2 + MAX_LRCLIB_FALLBACK_REQUESTS);
        Ok(())
    }

    #[tokio::test]
    async fn source_caches_fallback_under_original_item_fingerprint() -> Result<(), Box<dyn Error>>
    {
        let accepted = br#"[{"trackName":"Original","artistName":"Alias","duration":180.0,"plainLyrics":"plain","syncedLyrics":"[00:01]timed"}]"#.to_vec();
        let transport = SequencedSourceTransport::from_responses([
            LrclibHttpResponse::new(200, b"[]".to_vec(), false),
            LrclibHttpResponse::new(200, b"[]".to_vec(), false),
            LrclibHttpResponse::new(200, accepted, false),
        ]);
        let client = LrclibClient::with_dependencies(
            Arc::new(transport.clone()),
            Arc::new(SourceClock::default()),
            8,
            60_000,
        );
        let item = source_item("Original - Translation");

        assert!(client.fetch(&item).await?.is_some());
        assert!(client.fetch(&item).await?.is_some());
        assert_eq!(transport.requests().len(), 3);
        Ok(())
    }

    #[tokio::test]
    async fn source_strict_failures_do_not_start_fallbacks_or_reveal_payloads() {
        let cases = [
            (
                Ok(LrclibHttpResponse::new(200, b"[]".to_vec(), true)),
                LrclibSourceError::Redirected,
            ),
            (
                Ok(LrclibHttpResponse::new(503, b"private".to_vec(), false)),
                LrclibSourceError::NonSuccess,
            ),
            (
                Ok(LrclibHttpResponse::new(
                    200,
                    vec![b'x'; MAX_LYRICS_RESPONSE_BYTES + 1],
                    false,
                )),
                LrclibSourceError::ResponseTooLarge,
            ),
            (
                Ok(LrclibHttpResponse::new(200, b"private".to_vec(), false)),
                LrclibSourceError::InvalidResponse,
            ),
            (
                Err(LrclibSourceError::Unavailable),
                LrclibSourceError::Unavailable,
            ),
        ];

        for (response, expected) in cases {
            let transport = SequencedSourceTransport::new([response]);
            let client = LrclibClient::with_dependencies(
                Arc::new(transport.clone()),
                Arc::new(SourceClock::default()),
                8,
                60_000,
            );

            let Err(error) = client
                .fetch(&source_item("private title - private translation"))
                .await
            else {
                panic!("strict failure must fail closed");
            };
            assert_eq!(error, expected);
            assert_eq!(transport.requests().len(), 1);
            for rendered in [format!("{error}"), format!("{error:?}")] {
                assert!(!rendered.contains("private"));
                assert!(!rendered.contains("fixture-video"));
                assert!(!rendered.contains("track_name"));
            }
        }
    }

    #[tokio::test]
    async fn source_fallback_failures_fail_closed_without_trying_later_variants() {
        let ambiguous = br#"[
            {"trackName":"Original - Translation","artistName":"Main Artist","duration":180.0,"plainLyrics":"one","syncedLyrics":"[00:01]one"},
            {"trackName":"Original - Translation","artistName":"Main Artist","duration":180.0,"plainLyrics":"two","syncedLyrics":"[00:01]two"}
        ]"#.to_vec();
        let cases = [
            (
                Ok(LrclibHttpResponse::new(200, b"[]".to_vec(), true)),
                LrclibSourceError::Redirected,
            ),
            (
                Ok(LrclibHttpResponse::new(503, b"private".to_vec(), false)),
                LrclibSourceError::NonSuccess,
            ),
            (
                Ok(LrclibHttpResponse::new(
                    200,
                    vec![b'x'; MAX_LYRICS_RESPONSE_BYTES + 1],
                    false,
                )),
                LrclibSourceError::ResponseTooLarge,
            ),
            (
                Ok(LrclibHttpResponse::new(200, b"private".to_vec(), false)),
                LrclibSourceError::InvalidResponse,
            ),
            (
                Ok(LrclibHttpResponse::new(200, ambiguous, false)),
                LrclibSourceError::InvalidResponse,
            ),
            (
                Err(LrclibSourceError::Unavailable),
                LrclibSourceError::Unavailable,
            ),
        ];

        for (fallback_response, expected) in cases {
            let transport = SequencedSourceTransport::new([
                Ok(LrclibHttpResponse::new(200, b"[]".to_vec(), false)),
                fallback_response,
            ]);
            let client = LrclibClient::with_dependencies(
                Arc::new(transport.clone()),
                Arc::new(SourceClock::default()),
                8,
                60_000,
            );

            let Err(error) = client.fetch(&source_item("Original - Translation")).await else {
                panic!("invalid fallback must fail closed");
            };
            assert_eq!(error, expected);
            assert_eq!(transport.requests().len(), 2);
            for rendered in [format!("{error}"), format!("{error:?}")] {
                assert!(!rendered.contains("private"));
                assert!(!rendered.contains("fixture-video"));
                assert!(!rendered.contains("track_name"));
            }
        }
    }

    #[tokio::test]
    async fn source_rejects_redirect_status_oversize_and_malformed_without_payloads() {
        let cases = [
            (
                LrclibHttpResponse::new(200, b"[]".to_vec(), true),
                LrclibSourceError::Redirected,
            ),
            (
                LrclibHttpResponse::new(503, b"private upstream details".to_vec(), false),
                LrclibSourceError::NonSuccess,
            ),
            (
                LrclibHttpResponse::new(200, vec![b'x'; MAX_LYRICS_RESPONSE_BYTES + 1], false),
                LrclibSourceError::ResponseTooLarge,
            ),
            (
                LrclibHttpResponse::new(200, b"private malformed json".to_vec(), false),
                LrclibSourceError::InvalidResponse,
            ),
        ];

        for (response, expected) in cases {
            let client = LrclibClient::with_dependencies(
                Arc::new(SourceTransport::returning(response)),
                Arc::new(SourceClock::default()),
                8,
                60_000,
            );
            let Err(error) = client.fetch(&source_item("Song")).await else {
                panic!("unsafe response must fail");
            };
            assert_eq!(error, expected);
            for rendered in [format!("{error}"), format!("{error:?}")] {
                assert!(!rendered.contains("private"));
                assert!(!rendered.contains("http"));
                assert!(!rendered.contains("fixture-video"));
            }
        }
    }

    #[tokio::test]
    async fn source_rejects_oversized_request_metadata_before_dispatch()
    -> Result<(), Box<dyn Error>> {
        let transport =
            SourceTransport::returning(LrclibHttpResponse::new(200, b"[]".to_vec(), false));
        let client = LrclibClient::with_dependencies(
            Arc::new(transport.clone()),
            Arc::new(SourceClock::default()),
            8,
            60_000,
        );
        let item = source_item(&"x".repeat(super::MAX_LYRICS_METADATA_BYTES + 1));

        let Err(error) = client.fetch(&item).await else {
            panic!("oversized request metadata must fail");
        };

        assert_eq!(error, LrclibSourceError::InvalidResponse);
        assert!(transport.requests().is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn source_cache_keys_metadata_expires_and_evicts_oldest() -> Result<(), Box<dyn Error>> {
        let body = br#"[{"trackName":"Song","artistName":"Main Artist","duration":180.0,"plainLyrics":"plain","syncedLyrics":"[00:01]timed"}]"#.to_vec();
        let transport = SourceTransport::returning(LrclibHttpResponse::new(200, body, false));
        let clock = SourceClock::default();
        let client = LrclibClient::with_dependencies(
            Arc::new(transport.clone()),
            Arc::new(clock.clone()),
            1,
            100,
        );

        client.fetch(&source_item("Song")).await?;
        client.fetch(&source_item("Song")).await?;
        assert_eq!(transport.requests().len(), 1);

        let mut changed_metadata = source_item("Song");
        changed_metadata.collection = Some("Different album".to_owned());
        client.fetch(&changed_metadata).await?;
        assert_eq!(transport.requests().len(), 2);

        clock.set(101);
        client.fetch(&changed_metadata).await?;
        assert_eq!(transport.requests().len(), 3);

        client.fetch(&source_item("Song")).await?;
        assert_eq!(transport.requests().len(), 4);
        Ok(())
    }

    #[derive(Clone)]
    struct BlockingTransport {
        calls: Arc<AtomicUsize>,
        first_started: Arc<tokio::sync::Notify>,
        release_first: Arc<tokio::sync::Notify>,
        response: LrclibHttpResponse,
    }

    #[async_trait]
    impl LrclibTransport for BlockingTransport {
        async fn get(
            &self,
            _request: LrclibHttpRequest,
        ) -> Result<LrclibHttpResponse, LrclibSourceError> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if call == 0 {
                self.first_started.notify_one();
                self.release_first.notified().await;
            }
            Ok(self.response.clone())
        }
    }

    #[tokio::test]
    async fn source_does_not_hold_cache_mutex_while_transport_is_pending()
    -> Result<(), Box<dyn Error>> {
        let transport = BlockingTransport {
            calls: Arc::new(AtomicUsize::new(0)),
            first_started: Arc::new(tokio::sync::Notify::new()),
            release_first: Arc::new(tokio::sync::Notify::new()),
            response: LrclibHttpResponse::new(
                200,
                br#"[{"trackName":"Song","artistName":"Main Artist","duration":180.0,"plainLyrics":"plain","syncedLyrics":"[00:01]timed"}]"#.to_vec(),
                false,
            ),
        };
        let client = Arc::new(LrclibClient::with_dependencies(
            Arc::new(transport.clone()),
            Arc::new(SourceClock::default()),
            8,
            60_000,
        ));
        let first_client = Arc::clone(&client);
        let first = tokio::spawn(async move { first_client.fetch(&source_item("Song")).await });
        transport.first_started.notified().await;

        tokio::time::timeout(
            Duration::from_millis(250),
            client.fetch(&source_item("Song")),
        )
        .await
        .map_err(|_| "second request blocked on cache mutex")??;
        transport.release_first.notify_one();
        first.await??;
        assert_eq!(transport.calls.load(Ordering::SeqCst), 2);
        Ok(())
    }

    #[derive(Clone)]
    struct PlainProvider {
        lyrics: Option<PlainLyrics>,
        calls: Arc<AtomicUsize>,
        started: Option<Arc<tokio::sync::Notify>>,
        release: Option<Arc<tokio::sync::Notify>>,
    }

    fn provider_unsupported<T>(operation: ProviderOperation) -> ProviderResult<T> {
        Err(ProviderError::new(
            operation,
            ProviderErrorKind::Unsupported,
        ))
    }

    #[async_trait]
    impl MusicProvider for PlainProvider {
        async fn search(
            &self,
            _query: &str,
            _filter: SearchFilter,
        ) -> ProviderResult<Page<SearchItem>> {
            provider_unsupported(ProviderOperation::Search)
        }

        async fn charts(&self, _region: &RegionCode) -> ProviderResult<Vec<ChartSection>> {
            provider_unsupported(ProviderOperation::Charts)
        }

        async fn playlist(&self, _id: &str) -> ProviderResult<Vec<MediaItem>> {
            provider_unsupported(ProviderOperation::Playlist)
        }

        async fn podcast(&self, _id: &str) -> ProviderResult<Podcast> {
            provider_unsupported(ProviderOperation::Podcast)
        }

        async fn radio(&self, _seed: &MediaId) -> ProviderResult<Vec<MediaItem>> {
            provider_unsupported(ProviderOperation::Radio)
        }

        async fn lyrics(&self, _id: &MediaId) -> ProviderResult<PlainLyrics> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some(started) = &self.started {
                started.notify_one();
            }
            if let Some(release) = &self.release {
                release.notified().await;
            }
            self.lyrics.clone().ok_or_else(|| {
                ProviderError::new(ProviderOperation::Lyrics, ProviderErrorKind::NotFound)
            })
        }

        async fn library(&self, _section: LibrarySection) -> ProviderResult<Page<LibraryItem>> {
            provider_unsupported(ProviderOperation::Library)
        }

        fn authentication(&self) -> AuthenticationState {
            AuthenticationState::Unauthenticated
        }
    }

    #[tokio::test]
    async fn source_service_disables_external_calls_and_keeps_youtube_plain()
    -> Result<(), Box<dyn Error>> {
        let provider = PlainProvider {
            lyrics: Some(PlainLyrics::new("youtube plain")?),
            calls: Arc::new(AtomicUsize::new(0)),
            started: None,
            release: None,
        };
        let transport = SourceTransport::returning(LrclibHttpResponse::new(
            200,
            br#"[{"trackName":"Song","artistName":"Main Artist","duration":180.0,"plainLyrics":"external","syncedLyrics":"[00:01]timed"}]"#.to_vec(),
            false,
        ));
        let lrclib = Arc::new(LrclibClient::with_dependencies(
            Arc::new(transport.clone()),
            Arc::new(SourceClock::default()),
            8,
            60_000,
        ));
        let service = LyricsSourceService::new(Arc::new(provider), lrclib, false);

        let document = service
            .load(&source_item("Song"))
            .await?
            .ok_or("expected plain lyrics")?;

        assert_eq!(document.source(), LyricsSource::YouTubeMusic);
        assert_eq!(document.plain(), Some("youtube plain"));
        assert!(document.timed().is_empty());
        assert!(transport.requests().is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn source_service_keeps_youtube_plain_without_an_external_client()
    -> Result<(), Box<dyn Error>> {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = PlainProvider {
            lyrics: Some(PlainLyrics::new("youtube-only plain")?),
            calls: Arc::clone(&calls),
            started: None,
            release: None,
        };
        let service = LyricsSourceService::provider_only(Arc::new(provider));

        let document = service
            .load(&source_item("Song"))
            .await?
            .ok_or("expected provider lyrics without external client")?;

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(document.source(), LyricsSource::YouTubeMusic);
        assert_eq!(document.plain(), Some("youtube-only plain"));
        assert!(document.timed().is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn source_service_prefers_matched_sync_with_youtube_plain_fallback()
    -> Result<(), Box<dyn Error>> {
        let provider = PlainProvider {
            lyrics: Some(PlainLyrics::new("youtube plain")?),
            calls: Arc::new(AtomicUsize::new(0)),
            started: None,
            release: None,
        };
        let transport = SourceTransport::returning(LrclibHttpResponse::new(
            200,
            br#"[{"trackName":"Song","artistName":"Main Artist","duration":180.0,"plainLyrics":"external plain","syncedLyrics":"[00:01]timed"}]"#.to_vec(),
            false,
        ));
        let lrclib = Arc::new(LrclibClient::with_dependencies(
            Arc::new(transport),
            Arc::new(SourceClock::default()),
            8,
            60_000,
        ));
        let service = LyricsSourceService::new(Arc::new(provider), lrclib, true);

        let document = service
            .load(&source_item("Song"))
            .await?
            .ok_or("expected synchronized lyrics")?;

        assert_eq!(document.source(), LyricsSource::Lrclib);
        assert_eq!(document.plain(), Some("youtube plain"));
        assert_eq!(document.timed()[0].text(), "timed");
        Ok(())
    }

    #[tokio::test]
    async fn source_service_keeps_youtube_plain_when_external_request_fails()
    -> Result<(), Box<dyn Error>> {
        let provider = PlainProvider {
            lyrics: Some(PlainLyrics::new("youtube fallback")?),
            calls: Arc::new(AtomicUsize::new(0)),
            started: None,
            release: None,
        };
        let transport = SourceTransport::returning(LrclibHttpResponse::new(
            503,
            b"private external failure".to_vec(),
            false,
        ));
        let service = LyricsSourceService::new(
            Arc::new(provider),
            Arc::new(LrclibClient::with_dependencies(
                Arc::new(transport),
                Arc::new(SourceClock::default()),
                8,
                60_000,
            )),
            true,
        );

        let document = service
            .load(&source_item("Song"))
            .await?
            .ok_or("expected provider fallback")?;

        assert_eq!(document.source(), LyricsSource::YouTubeMusic);
        assert_eq!(document.plain(), Some("youtube fallback"));
        Ok(())
    }

    #[tokio::test]
    async fn source_service_keeps_youtube_plain_over_external_instrumental()
    -> Result<(), Box<dyn Error>> {
        let provider = PlainProvider {
            lyrics: Some(PlainLyrics::new("youtube plain")?),
            calls: Arc::new(AtomicUsize::new(0)),
            started: None,
            release: None,
        };
        let transport = SourceTransport::returning(LrclibHttpResponse::new(
            200,
            br#"[{"trackName":"Song","artistName":"Main Artist","duration":180.0,"plainLyrics":null,"syncedLyrics":null,"instrumental":true}]"#.to_vec(),
            false,
        ));
        let service = LyricsSourceService::new(
            Arc::new(provider),
            Arc::new(LrclibClient::with_dependencies(
                Arc::new(transport),
                Arc::new(SourceClock::default()),
                8,
                60_000,
            )),
            true,
        );

        let document = service
            .load(&source_item("Song"))
            .await?
            .ok_or("expected YouTube lyrics")?;

        assert_eq!(document.source(), LyricsSource::YouTubeMusic);
        assert_eq!(document.plain(), Some("youtube plain"));
        assert!(!document.instrumental());
        Ok(())
    }

    #[tokio::test]
    async fn source_service_starts_enabled_sources_concurrently() -> Result<(), Box<dyn Error>> {
        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let provider = PlainProvider {
            lyrics: Some(PlainLyrics::new("youtube plain")?),
            calls: Arc::new(AtomicUsize::new(0)),
            started: Some(Arc::clone(&started)),
            release: Some(Arc::clone(&release)),
        };
        let transport =
            SourceTransport::returning(LrclibHttpResponse::new(200, b"[]".to_vec(), false));
        let service = Arc::new(LyricsSourceService::new(
            Arc::new(provider),
            Arc::new(LrclibClient::with_dependencies(
                Arc::new(transport.clone()),
                Arc::new(SourceClock::default()),
                8,
                60_000,
            )),
            true,
        ));
        let task_service = Arc::clone(&service);
        let task = tokio::spawn(async move { task_service.load(&source_item("Song")).await });
        started.notified().await;

        tokio::time::timeout(Duration::from_millis(250), async {
            while transport.requests().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .map_err(|_| "external source did not start while provider was pending")?;

        release.notify_one();
        assert!(task.await??.is_some());
        Ok(())
    }

    fn line(start_ms: u64, end_ms: Option<u64>, text: &str) -> Result<TimedLyricLine, LyricsError> {
        TimedLyricLine::new(start_ms, end_ms, text)
    }

    #[test]
    fn public_transport_and_model_limits_are_conservative() {
        const {
            assert!(MAX_LYRICS_RESPONSE_BYTES >= MAX_LYRICS_TEXT_BYTES);
            assert!(MAX_LYRICS_TEXT_BYTES >= MAX_TIMED_LYRIC_LINE_BYTES);
            assert!(MAX_TIMED_LYRIC_LINES > 0);
        }
    }

    #[test]
    fn timed_line_constructor_trims_and_exposes_read_only_fields() -> Result<(), Box<dyn Error>> {
        let lyric = line(1_000, Some(2_000), "  a secret line  ")?;

        assert_eq!(lyric.start_ms(), 1_000);
        assert_eq!(lyric.end_ms(), Some(2_000));
        assert_eq!(lyric.text(), "a secret line");
        Ok(())
    }

    #[test]
    fn timed_line_rejects_empty_or_whitespace_only_text() {
        for text in ["", "  \n\t "] {
            assert!(matches!(
                line(0, None, text),
                Err(LyricsError::EmptyText {
                    field: "timed.text"
                })
            ));
        }
    }

    #[test]
    fn timed_line_enforces_utf8_byte_limit_without_splitting_text() -> Result<(), Box<dyn Error>> {
        let exact = "é".repeat(MAX_TIMED_LYRIC_LINE_BYTES / "é".len());
        let accepted = line(0, None, &exact)?;
        let oversized = format!("{exact}é");

        assert_eq!(accepted.text(), exact);
        assert!(matches!(
            line(0, None, &oversized),
            Err(LyricsError::TextTooLong {
                field: "timed.text",
                ..
            })
        ));
        Ok(())
    }

    #[test]
    fn timed_line_rejects_end_before_start() {
        assert!(matches!(
            line(2_000, Some(1_999), "line"),
            Err(LyricsError::InvalidTimestampRange { .. })
        ));
    }

    #[test]
    fn document_constructor_exposes_source_and_fallbacks() -> Result<(), Box<dyn Error>> {
        let timed = vec![line(500, None, "timed secret")?];
        let document = LyricsDocument::new(
            LyricsSource::YouTubeMusic,
            Some("  plain secret  ".to_owned()),
            timed,
            false,
        )?;

        assert_eq!(document.source(), LyricsSource::YouTubeMusic);
        assert_eq!(document.plain(), Some("plain secret"));
        assert_eq!(document.timed().len(), 1);
        assert!(!document.instrumental());
        Ok(())
    }

    #[test]
    fn non_instrumental_document_requires_plain_or_timed_fallback() {
        assert!(matches!(
            LyricsDocument::new(LyricsSource::Lrclib, None, Vec::new(), false),
            Err(LyricsError::MissingLyrics)
        ));
        assert!(matches!(
            LyricsDocument::new(
                LyricsSource::Lrclib,
                Some(" \n ".to_owned()),
                Vec::new(),
                false
            ),
            Err(LyricsError::EmptyText { field: "plain" })
        ));
    }

    #[test]
    fn instrumental_document_cannot_pretend_to_contain_lyrics() -> Result<(), Box<dyn Error>> {
        let instrumental = LyricsDocument::new(LyricsSource::YouTubeMusic, None, Vec::new(), true)?;

        assert!(instrumental.instrumental());
        assert_eq!(instrumental.plain(), None);
        assert!(instrumental.timed().is_empty());
        assert!(matches!(
            LyricsDocument::new(
                LyricsSource::YouTubeMusic,
                Some("not instrumental".to_owned()),
                Vec::new(),
                true
            ),
            Err(LyricsError::InstrumentalHasLyrics)
        ));
        assert!(matches!(
            LyricsDocument::new(
                LyricsSource::YouTubeMusic,
                None,
                vec![line(0, None, "not instrumental")?],
                true
            ),
            Err(LyricsError::InstrumentalHasLyrics)
        ));
        Ok(())
    }

    #[test]
    fn document_rejects_too_many_timed_lines() -> Result<(), Box<dyn Error>> {
        let lyric = line(0, None, "line")?;
        let timed = vec![lyric; MAX_TIMED_LYRIC_LINES + 1];

        assert!(matches!(
            LyricsDocument::new(LyricsSource::Lrclib, None, timed, false),
            Err(LyricsError::TooManyTimedLines { .. })
        ));
        Ok(())
    }

    #[test]
    fn document_rejects_non_increasing_timestamps() -> Result<(), Box<dyn Error>> {
        let out_of_order = vec![line(2_000, None, "later")?, line(1_000, None, "earlier")?];
        let duplicate = vec![line(1_000, None, "first")?, line(1_000, None, "second")?];

        for timed in [out_of_order, duplicate] {
            assert!(matches!(
                LyricsDocument::new(LyricsSource::Lrclib, None, timed, false),
                Err(LyricsError::TimedLinesOutOfOrder { .. })
            ));
        }
        Ok(())
    }

    #[test]
    fn document_rejects_total_text_over_byte_limit() -> Result<(), Box<dyn Error>> {
        let plain = "a".repeat(MAX_LYRICS_TEXT_BYTES);
        let timed = vec![line(0, None, "b")?];

        assert!(matches!(
            LyricsDocument::new(LyricsSource::Lrclib, Some(plain), timed, false),
            Err(LyricsError::TotalTextTooLong { .. })
        ));
        Ok(())
    }

    #[test]
    fn active_line_obeys_start_end_and_open_ended_boundaries() -> Result<(), Box<dyn Error>> {
        let document = LyricsDocument::new(
            LyricsSource::Lrclib,
            None,
            vec![
                line(1_000, Some(2_000), "first secret")?,
                line(2_000, Some(2_500), "second secret")?,
                line(3_000, None, "final secret")?,
            ],
            false,
        )?;

        assert_eq!(document.active_line(999).map(TimedLyricLine::text), None);
        assert_eq!(
            document.active_line(1_000).map(TimedLyricLine::text),
            Some("first secret")
        );
        assert_eq!(
            document.active_line(1_999).map(TimedLyricLine::text),
            Some("first secret")
        );
        assert_eq!(
            document.active_line(2_000).map(TimedLyricLine::text),
            Some("second secret")
        );
        assert_eq!(document.active_line(2_500).map(TimedLyricLine::text), None);
        assert_eq!(document.active_line(2_999).map(TimedLyricLine::text), None);
        assert_eq!(
            document.active_line(3_000).map(TimedLyricLine::text),
            Some("final secret")
        );
        assert_eq!(
            document.active_line(u64::MAX).map(TimedLyricLine::text),
            Some("final secret")
        );
        Ok(())
    }

    #[test]
    fn open_ended_line_yields_to_the_next_start() -> Result<(), Box<dyn Error>> {
        let document = LyricsDocument::new(
            LyricsSource::Lrclib,
            None,
            vec![
                line(1_000, None, "first secret")?,
                line(2_000, None, "next secret")?,
            ],
            false,
        )?;

        assert_eq!(
            document.active_line(1_999).map(TimedLyricLine::text),
            Some("first secret")
        );
        assert_eq!(
            document.active_line(2_000).map(TimedLyricLine::text),
            Some("next secret")
        );
        Ok(())
    }

    #[test]
    fn debug_output_is_summary_only_and_redacts_lyrics() -> Result<(), Box<dyn Error>> {
        let line_secret = "LINE_SECRET_DO_NOT_LOG";
        let plain_secret = "PLAIN_SECRET_DO_NOT_LOG";
        let lyric = line(1_000, None, line_secret)?;
        let line_debug = format!("{lyric:?}");
        let document = LyricsDocument::new(
            LyricsSource::Lrclib,
            Some(plain_secret.to_owned()),
            vec![lyric],
            false,
        )?;
        let document_debug = format!("{document:?}");

        assert!(!line_debug.contains(line_secret));
        assert!(!document_debug.contains(line_secret));
        assert!(!document_debug.contains(plain_secret));
        assert!(line_debug.contains("text_bytes"));
        assert!(document_debug.contains("timed_lines"));
        Ok(())
    }

    #[test]
    fn lrc_parses_supported_timestamps_and_derives_line_ends() -> Result<(), Box<dyn Error>> {
        let document = parse_lrc("[00:01]first\n[00:02.34]second\n[01:03.456]third", None)?;

        let actual = document
            .timed()
            .iter()
            .map(|line| (line.start_ms(), line.end_ms(), line.text()))
            .collect::<Vec<_>>();
        assert_eq!(
            actual,
            vec![
                (1_000, Some(2_340), "first"),
                (2_340, Some(63_456), "second"),
                (63_456, None, "third"),
            ]
        );
        Ok(())
    }

    #[test]
    fn lyric_models_normalize_line_endings_and_control_characters() -> Result<(), Box<dyn Error>> {
        let plain = PlainLyrics::new("one\r\ntwo\rthree\tfour\x1b\x07")?;
        assert_eq!(plain.text(), "one\ntwo\nthree four");

        let timed = TimedLyricLine::new(
            1_000,
            None,
            "  café\u{0301}\r\nnext\rlast\n🙂\t\x1b\x07אב  ",
        )?;
        assert_eq!(timed.text(), "café\u{0301} next last 🙂 אב");
        assert!(!timed.text().chars().any(char::is_control));

        let document = LyricsDocument::new(
            LyricsSource::YouTubeMusic,
            Some("first\r\nsecond\rthird\tfourth\x1b\x07".to_owned()),
            Vec::new(),
            false,
        )?;
        assert_eq!(document.plain(), Some("first\nsecond\nthird fourth"));
        Ok(())
    }

    #[test]
    fn lrc_parser_normalizes_crlf_lone_cr_tabs_and_controls() -> Result<(), Box<dyn Error>> {
        let document = parse_lrc(
            "[00:01]first\r\n[00:02]second\r[00:03]tab\ttext\x1b\x07🙂אב",
            Some("plain\r\nfallback\rline\ttwo\x1b\x07"),
        )?;

        assert_eq!(document.plain(), Some("plain\nfallback\nline two"));
        assert_eq!(
            document
                .timed()
                .iter()
                .map(TimedLyricLine::text)
                .collect::<Vec<_>>(),
            ["first", "second", "tab text🙂אב"]
        );
        assert!(
            document
                .timed()
                .iter()
                .all(|line| !line.text().contains('\r'))
        );
        Ok(())
    }

    #[test]
    fn lrc_expands_multiple_timestamps_and_sorts_out_of_order_input() -> Result<(), Box<dyn Error>>
    {
        let document = parse_lrc("[00:03]last\n[00:02][00:01]repeated", None)?;

        let actual = document
            .timed()
            .iter()
            .map(|line| (line.start_ms(), line.text()))
            .collect::<Vec<_>>();
        assert_eq!(
            actual,
            vec![(1_000, "repeated"), (2_000, "repeated"), (3_000, "last")]
        );
        Ok(())
    }

    #[test]
    fn lrc_skips_blank_text_and_keeps_first_duplicate_deterministically()
    -> Result<(), Box<dyn Error>> {
        let document = parse_lrc(
            "[00:01]first\n[00:00]   \n[00:01]second\n[00:02]final",
            None,
        )?;

        let actual = document
            .timed()
            .iter()
            .map(TimedLyricLine::text)
            .collect::<Vec<_>>();
        assert_eq!(actual, vec!["first", "final"]);
        Ok(())
    }

    #[test]
    fn lrc_skips_malformed_timestamps_without_discarding_valid_lines() -> Result<(), Box<dyn Error>>
    {
        let document = parse_lrc(
            "[00:01]first\n[0x:01]bad\n[00:60]bad\n[00:01.2]bad\n[00:01.2345]bad\n[00:01bad\n[00:02]last",
            Some("plain fallback"),
        )?;

        let actual = document
            .timed()
            .iter()
            .map(|line| (line.start_ms(), line.text()))
            .collect::<Vec<_>>();
        assert_eq!(actual, vec![(1_000, "first"), (2_000, "last")]);
        assert_eq!(document.plain(), Some("plain fallback"));
        Ok(())
    }

    #[test]
    fn lrc_rejects_oversized_input_lines_and_retained_text() {
        let oversized_response = "x".repeat(MAX_LYRICS_RESPONSE_BYTES + 1);
        assert_eq!(
            parse_lrc(&oversized_response, Some("fallback")),
            Err(LyricsParseError::ResponseTooLarge)
        );

        let oversized_line = format!("[00:00]{}", "x".repeat(MAX_TIMED_LYRIC_LINE_BYTES + 1));
        assert_eq!(
            parse_lrc(&oversized_line, None),
            Err(LyricsParseError::LineTooLarge)
        );

        let oversized_metadata_line = format!(
            "[ar:{}]",
            "x".repeat(MAX_TIMED_LYRIC_LINE_BYTES.saturating_mul(2))
        );
        assert_eq!(
            parse_lrc(&oversized_metadata_line, Some("fallback")),
            Err(LyricsParseError::LineTooLarge)
        );

        let oversized_plain = "é".repeat(MAX_LYRICS_TEXT_BYTES / "é".len() + 1);
        assert_eq!(
            parse_lrc("[ar:metadata only]", Some(&oversized_plain)),
            Err(LyricsParseError::LyricsTooLarge)
        );

        let too_many_lines = (0..=MAX_TIMED_LYRIC_LINES)
            .map(|minute| format!("[{minute:02}:00]x"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            parse_lrc(&too_many_lines, None),
            Err(LyricsParseError::LyricsTooLarge)
        );
    }

    #[test]
    fn lrc_uses_plain_fallback_when_no_timed_lines_exist() -> Result<(), Box<dyn Error>> {
        let document = parse_lrc("[ar:Artist]\n[00:00]   ", Some("  plain fallback  "))?;

        assert!(document.timed().is_empty());
        assert_eq!(document.plain(), Some("plain fallback"));
        Ok(())
    }

    fn request<'a>(artists: &'a [&'a str]) -> LrclibMatchRequest<'a> {
        LrclibMatchRequest::new("The Song!", "Main Artist", artists, Some(180_000))
    }

    #[test]
    fn lrclib_requires_exact_punctuation_case_whitespace_normalized_metadata()
    -> Result<(), Box<dyn Error>> {
        let response = br#"[
          {"id":12,"trackName":"  THE song  ","artistName":"main ARTIST",
           "duration":180.4,"plainLyrics":"plain accepted","syncedLyrics":null},
          {"id":13,"trackName":"The Songs","artistName":"Main Artist",
           "duration":180.0,"plainLyrics":"must reject","syncedLyrics":null}
        ]"#;

        let matched = match_lrclib_response(response, &request(&["Main Artist"]))?
            .ok_or("expected exact normalized match")?;
        assert_eq!(matched.plain(), Some("plain accepted"));
        assert_eq!(matched.source(), LyricsSource::Lrclib);
        assert!(!format!("{matched:?}").contains("id"));
        Ok(())
    }

    #[test]
    fn lrclib_accepts_safe_exact_artist_list_agreement() -> Result<(), Box<dyn Error>> {
        let response = br#"[{
          "trackName":"The Song","artistName":"Guest Artist, Main Artist",
          "duration":180.0,"plainLyrics":"collaboration","syncedLyrics":null,
          "url":"https://example.invalid/provider-secret"
        }]"#;
        let artists = ["Main Artist", "Guest Artist"];

        let matched = match_lrclib_response(response, &request(&artists))?
            .ok_or("expected exact artist-list agreement")?;
        assert_eq!(matched.plain(), Some("collaboration"));
        assert!(!format!("{matched:?}").contains("example.invalid"));
        Ok(())
    }

    #[test]
    fn lrclib_normalization_ignores_straight_and_curly_apostrophes() -> Result<(), Box<dyn Error>> {
        let request =
            LrclibMatchRequest::new("Dont Stop", "Main Artist", &["Main Artist"], Some(180_000));

        for response in [
            br#"[{"trackName":"Don't Stop","artistName":"Main Artist","duration":180.0,"plainLyrics":"accepted","syncedLyrics":null}]"#.as_slice(),
            "[{\"trackName\":\"Don’t Stop\",\"artistName\":\"Main Artist\",\"duration\":180.0,\"plainLyrics\":\"accepted\",\"syncedLyrics\":null}]".as_bytes(),
        ] {
            assert!(match_lrclib_response(response, &request)?.is_some());
        }
        Ok(())
    }

    #[test]
    fn lrclib_rejects_partial_artist_and_duration_mismatch() -> Result<(), Box<dyn Error>> {
        for response in [
            br#"[{"trackName":"The Song","artistName":"Main Artist feat Guest","duration":180.0,"plainLyrics":"no","syncedLyrics":null}]"#.as_slice(),
            br#"[{"trackName":"The Song","artistName":"Main Artist","duration":183.0,"plainLyrics":"no","syncedLyrics":null}]"#.as_slice(),
        ] {
            assert!(match_lrclib_response(response, &request(&["Main Artist"]))?.is_none());
        }
        Ok(())
    }

    #[test]
    fn lrclib_rejects_candidates_without_safe_duration_agreement() -> Result<(), Box<dyn Error>> {
        let missing_candidate_duration = br#"[{
          "trackName":"The Song","artistName":"Main Artist",
          "duration":null,"plainLyrics":"must reject","syncedLyrics":"[00:01]must reject"
        }]"#;
        assert!(
            match_lrclib_response(missing_candidate_duration, &request(&["Main Artist"]))?
                .is_none()
        );

        let unknown_request_duration =
            LrclibMatchRequest::new("The Song", "Main Artist", &["Main Artist"], None);
        let known_candidate_duration = br#"[{
          "trackName":"The Song","artistName":"Main Artist",
          "duration":180.0,"plainLyrics":"must reject","syncedLyrics":"[00:01]must reject"
        }]"#;
        assert!(
            match_lrclib_response(known_candidate_duration, &unknown_request_duration)?.is_none()
        );
        Ok(())
    }

    #[test]
    fn lrclib_missing_duration_candidates_cannot_create_false_ambiguity()
    -> Result<(), Box<dyn Error>> {
        let response = br#"[
          {"trackName":"The Song","artistName":"Main Artist","duration":null,
           "plainLyrics":"unsafe","syncedLyrics":"[00:01]unsafe"},
          {"trackName":"The Song","artistName":"Main Artist","duration":180.0,
           "plainLyrics":"safe","syncedLyrics":"[00:01]safe"}
        ]"#;

        let matched = match_lrclib_response(response, &request(&["Main Artist"]))?
            .ok_or("expected the duration-agreeing candidate")?;
        assert_eq!(matched.plain(), Some("safe"));
        assert_eq!(matched.timed()[0].text(), "safe");
        Ok(())
    }

    #[test]
    fn lrclib_rejects_ambiguous_equal_matches() {
        let response = br#"[
          {"trackName":"The Song","artistName":"Main Artist","duration":180.0,"plainLyrics":"one","syncedLyrics":null},
          {"trackName":"The Song!","artistName":"Main Artist","duration":180.0,"plainLyrics":"two","syncedLyrics":null}
        ]"#;

        assert_eq!(
            match_lrclib_response(response, &request(&["Main Artist"])),
            Err(LyricsParseError::AmbiguousMatch)
        );
    }

    fn lrclib_ranking_request(collection: Option<&str>) -> LrclibMatchRequest<'_> {
        LrclibMatchRequest::new("Malibu Nights", "LANY", &["LANY"], Some(287_000))
            .with_collection(collection)
    }

    #[test]
    fn lrclib_rank_prefers_exact_album_independent_of_response_order() -> Result<(), Box<dyn Error>>
    {
        let responses = [
            br#"[
              {"trackName":"Malibu Nights","artistName":"LANY","albumName":"LANY Videos","duration":287.0,"plainLyrics":"other","syncedLyrics":"[00:01]other"},
              {"trackName":"LANY - Malibu Nights","artistName":"LANY","albumName":"LANY - Malibu Nights","duration":278.0,"plainLyrics":"wrong","syncedLyrics":"[00:01]wrong"},
              {"trackName":"Malibu Nights","artistName":"LANY","albumName":"LANY","duration":287.0,"plainLyrics":"right","syncedLyrics":"[00:01]right"}
            ]"#
            .as_slice(),
            br#"[
              {"trackName":"Malibu Nights","artistName":"LANY","albumName":"LANY","duration":287.0,"plainLyrics":"right","syncedLyrics":"[00:01]right"},
              {"trackName":"LANY - Malibu Nights","artistName":"LANY","albumName":"LANY - Malibu Nights","duration":278.0,"plainLyrics":"wrong","syncedLyrics":"[00:01]wrong"},
              {"trackName":"Malibu Nights","artistName":"LANY","albumName":"LANY Videos","duration":287.0,"plainLyrics":"other","syncedLyrics":"[00:01]other"}
            ]"#
            .as_slice(),
        ];

        for response in responses {
            let matched = match_lrclib_response(response, &lrclib_ranking_request(Some("LANY")))?
                .ok_or("expected album-ranked synchronized lyrics")?;
            assert_eq!(matched.timed()[0].text(), "right");
        }
        Ok(())
    }

    #[test]
    fn lrclib_rank_collapses_identical_equal_ranked_documents() -> Result<(), Box<dyn Error>> {
        let response = br#"[
          {"trackName":"Malibu Nights","artistName":"LANY","albumName":"One","duration":287.0,"plainLyrics":"same","syncedLyrics":"[00:01]same"},
          {"trackName":"Malibu Nights","artistName":"LANY","albumName":"Two","duration":287.0,"plainLyrics":"same","syncedLyrics":"[00:01]same"}
        ]"#;

        let matched = match_lrclib_response(response, &lrclib_ranking_request(None))?
            .ok_or("expected identical candidates to collapse")?;
        assert_eq!(matched.timed()[0].text(), "same");
        Ok(())
    }

    #[test]
    fn lrclib_rank_keeps_distinct_equal_candidates_ambiguous_without_album() {
        let response = br#"[
          {"trackName":"Malibu Nights","artistName":"LANY","albumName":"One","duration":287.0,"plainLyrics":"one","syncedLyrics":"[00:01]one"},
          {"trackName":"Malibu Nights","artistName":"LANY","albumName":"Two","duration":287.0,"plainLyrics":"two","syncedLyrics":"[00:01]two"}
        ]"#;

        assert_eq!(
            match_lrclib_response(response, &lrclib_ranking_request(None)),
            Err(LyricsParseError::AmbiguousMatch)
        );
    }

    #[test]
    fn lrclib_rank_prefers_closest_duration_with_equal_album() -> Result<(), Box<dyn Error>> {
        let response = br#"[
          {"trackName":"Malibu Nights","artistName":"LANY","albumName":"LANY","duration":288.0,"plainLyrics":"near","syncedLyrics":"[00:01]near"},
          {"trackName":"Malibu Nights","artistName":"LANY","albumName":"LANY","duration":287.0,"plainLyrics":"exact","syncedLyrics":"[00:01]exact"}
        ]"#;

        let matched = match_lrclib_response(response, &lrclib_ranking_request(Some("LANY")))?
            .ok_or("expected closest duration")?;
        assert_eq!(matched.timed()[0].text(), "exact");
        Ok(())
    }

    #[test]
    fn lrclib_rank_bounds_candidate_album_metadata() {
        let oversized_album = "x".repeat(MAX_LYRICS_METADATA_BYTES + 1);
        let response = format!(
            r#"[{{"trackName":"Malibu Nights","artistName":"LANY","albumName":"{oversized_album}","duration":287.0,"plainLyrics":"plain","syncedLyrics":"[00:01]timed"}}]"#
        );

        assert_eq!(
            match_lrclib_response(response.as_bytes(), &lrclib_ranking_request(Some("LANY"))),
            Err(LyricsParseError::LyricsTooLarge)
        );
    }

    #[test]
    fn lrclib_prefers_synchronized_only_after_metadata_acceptance() -> Result<(), Box<dyn Error>> {
        let response = br#"[
          {"trackName":"The Song","artistName":"Main Artist","duration":180.0,
           "plainLyrics":"accepted plain","syncedLyrics":null},
          {"trackName":"Wrong Song","artistName":"Main Artist","duration":180.0,
           "plainLyrics":"wrong plain","syncedLyrics":"[00:01]wrong synchronized"},
          {"trackName":"The Song!","artistName":"Main Artist","duration":180.5,
           "plainLyrics":"accepted synchronized plain","syncedLyrics":"[00:01.25]right synchronized"}
        ]"#;

        let matched = match_lrclib_response(response, &request(&["Main Artist"]))?
            .ok_or("expected synchronized exact match")?;
        assert_eq!(matched.timed()[0].start_ms(), 1_250);
        assert_eq!(matched.timed()[0].text(), "right synchronized");
        Ok(())
    }

    #[test]
    fn lrclib_bounds_response_results_and_candidate_text() {
        let oversized = vec![b' '; MAX_LYRICS_RESPONSE_BYTES + 1];
        assert_eq!(
            match_lrclib_response(&oversized, &request(&["Main Artist"])),
            Err(LyricsParseError::ResponseTooLarge)
        );

        let too_many = format!(
            "[{}]",
            std::iter::repeat_n(
                r#"{"trackName":"no","artistName":"no","plainLyrics":null,"syncedLyrics":null}"#,
                51
            )
            .collect::<Vec<_>>()
            .join(",")
        );
        assert_eq!(
            match_lrclib_response(too_many.as_bytes(), &request(&["Main Artist"])),
            Err(LyricsParseError::TooManyResults)
        );
    }
}
