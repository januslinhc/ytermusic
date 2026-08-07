use std::{
    collections::HashMap,
    fmt,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use serde::{
    Deserialize, Deserializer,
    de::{IgnoredAny, SeqAccess, Visitor},
};
use serde_json::Value;
use thiserror::Error;
use unicode_normalization::{UnicodeNormalization as _, char::is_combining_mark};
use url::Url;

use crate::{
    app::PodcastProviderId,
    domain::{ArtworkUrl, RegionCode},
    provider::SearchItem,
};

#[cfg(test)]
thread_local! {
    static DESERIALIZED_APPLE_ROWS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

pub const MAX_PODCAST_RECOMMENDATIONS: usize = 20;
pub const MAX_PODCAST_FEED_BYTES: usize = 512 * 1024;
const MAX_PODCAST_SOURCE_ID_BYTES: usize = 128;
const MAX_PODCAST_TEXT_BYTES: usize = 512;
const MAX_OS_LOCALE_BYTES: usize = 128;
const PODCAST_CACHE_TTL_MILLIS: i64 = 60 * 60 * 1_000;
const MAX_PODCAST_CACHE_COUNTRIES: usize = 16;
const PODCAST_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const PODCAST_TOTAL_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Eq, Hash, PartialEq)]
pub struct PodcastRecommendationId(String);

impl fmt::Debug for PodcastRecommendationId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PodcastRecommendationId([REDACTED])")
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct PodcastRecommendation {
    source_id: PodcastRecommendationId,
    rank: usize,
    title: String,
    publisher: String,
    artwork_url: Option<ArtworkUrl>,
}

impl PodcastRecommendation {
    #[must_use]
    pub const fn source_id(&self) -> &PodcastRecommendationId {
        &self.source_id
    }

    #[must_use]
    pub const fn rank(&self) -> usize {
        self.rank
    }

    #[must_use]
    pub fn title(&self) -> &str {
        &self.title
    }

    #[must_use]
    pub fn publisher(&self) -> &str {
        &self.publisher
    }

    #[must_use]
    pub const fn artwork_url(&self) -> Option<&ArtworkUrl> {
        self.artwork_url.as_ref()
    }
}

impl fmt::Debug for PodcastRecommendation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PodcastRecommendation")
            .field("source_id_redacted", &true)
            .field("rank", &self.rank)
            .field("title_redacted", &true)
            .field("publisher_redacted", &true)
            .field("has_artwork", &self.artwork_url.is_some())
            .finish_non_exhaustive()
    }
}

#[must_use]
pub(crate) fn match_podcast_recommendation(
    recommendation: &PodcastRecommendation,
    candidates: &[SearchItem],
) -> Option<PodcastProviderId> {
    let expected_title = normalize_match_text(recommendation.title())?;
    if expected_title.is_empty() {
        return None;
    }
    let expected_publisher = normalize_match_text(recommendation.publisher())?;
    let mut exact = candidates.iter().filter_map(|candidate| {
        let SearchItem::Podcast(candidate) = candidate else {
            return None;
        };
        if candidate.title.len() > MAX_PODCAST_TEXT_BYTES
            || candidate
                .subtitle
                .as_ref()
                .is_some_and(|publisher| publisher.len() > MAX_PODCAST_TEXT_BYTES)
        {
            return None;
        }
        (normalize_match_text(&candidate.title)? == expected_title)
            .then(|| PodcastProviderId::new(candidate.id.clone()))
            .flatten()
            .and_then(|id| {
                let publisher_score = match candidate.subtitle.as_deref() {
                    Some(publisher) => publisher_overlap(&expected_publisher, publisher)?,
                    None => 0,
                };
                Some((id, publisher_score))
            })
    });
    let first = exact.next()?;
    let Some(second) = exact.next() else {
        return Some(first.0);
    };
    let mut best = first;
    let mut tied = false;
    for candidate in std::iter::once(second).chain(exact) {
        match candidate.1.cmp(&best.1) {
            std::cmp::Ordering::Greater => {
                best = candidate;
                tied = false;
            }
            std::cmp::Ordering::Equal => tied = true,
            std::cmp::Ordering::Less => {}
        }
    }
    (best.1 > 0 && !tied).then_some(best.0)
}

fn normalize_match_text(value: &str) -> Option<String> {
    let mut canonical = String::new();
    for character in value.trim().nfc().take(MAX_PODCAST_TEXT_BYTES + 1) {
        if canonical.len().saturating_add(character.len_utf8()) > MAX_PODCAST_TEXT_BYTES {
            return None;
        }
        canonical.push(character);
    }
    let mut normalized = String::new();
    let mut separator_pending = false;
    for character in canonical.chars().flat_map(char::to_lowercase) {
        if character.is_alphanumeric() {
            if separator_pending && !normalized.is_empty() {
                normalized.push(' ');
            }
            if normalized.len().saturating_add(character.len_utf8()) > MAX_PODCAST_TEXT_BYTES {
                return None;
            }
            normalized.push(character);
            separator_pending = false;
        } else if is_combining_mark(character) && !normalized.is_empty() {
            if normalized.len().saturating_add(character.len_utf8()) > MAX_PODCAST_TEXT_BYTES {
                return None;
            }
            normalized.push(character);
        } else if character != '\'' && character != '\u{2019}' {
            separator_pending = !normalized.is_empty();
        }
    }
    Some(normalized)
}

fn publisher_overlap(expected: &str, candidate: &str) -> Option<usize> {
    let candidate = normalize_match_text(candidate)?;
    Some(
        expected
            .split_whitespace()
            .filter(|word| {
                candidate
                    .split_whitespace()
                    .any(|candidate| candidate == *word)
            })
            .count(),
    )
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PodcastRecommendationPage {
    region: RegionCode,
    items: Vec<PodcastRecommendation>,
}

impl PodcastRecommendationPage {
    #[must_use]
    pub const fn region(&self) -> &RegionCode {
        &self.region
    }

    #[must_use]
    pub fn items(&self) -> &[PodcastRecommendation] {
        &self.items
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum PodcastRankingError {
    #[error("podcast rankings are unavailable")]
    Unavailable,
    #[error("podcast ranking response is invalid")]
    InvalidResponse,
    #[error("podcast ranking response exceeds the input limit")]
    TooLarge,
}

#[async_trait]
pub trait PodcastRankingSource: Send + Sync {
    /// Returns the current Top Shows page for the requested region.
    ///
    /// # Errors
    ///
    /// Returns a payload-free [`PodcastRankingError`] when the feed cannot be
    /// fetched or parsed safely.
    async fn top_shows(
        &self,
        requested: &RegionCode,
    ) -> Result<PodcastRecommendationPage, PodcastRankingError>;
}

#[async_trait]
trait PodcastRankingTransport: Send + Sync {
    async fn get(&self, url: Url) -> Result<Vec<u8>, PodcastRankingError>;
}

trait PodcastRankingClock: Send + Sync {
    fn now_millis(&self) -> i64;
}

struct SystemClock;

impl PodcastRankingClock for SystemClock {
    fn now_millis(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|duration| i64::try_from(duration.as_millis()).ok())
            .unwrap_or(i64::MAX)
    }
}

struct ReqwestPodcastRankingTransport {
    client: reqwest::Client,
}

impl ReqwestPodcastRankingTransport {
    fn new() -> Result<Self, PodcastRankingError> {
        let client = reqwest::Client::builder()
            .connect_timeout(PODCAST_CONNECT_TIMEOUT)
            .timeout(PODCAST_TOTAL_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| PodcastRankingError::Unavailable)?;
        Ok(Self { client })
    }
}

#[async_trait]
impl PodcastRankingTransport for ReqwestPodcastRankingTransport {
    async fn get(&self, url: Url) -> Result<Vec<u8>, PodcastRankingError> {
        let mut response = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|_| PodcastRankingError::Unavailable)?;
        validate_response_metadata(response.status(), response.content_length())?;
        let mut body = BoundedBody::new(response.content_length())?;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| PodcastRankingError::Unavailable)?
        {
            body.push(&chunk)?;
        }
        Ok(body.finish())
    }
}

fn validate_response_metadata(
    status: reqwest::StatusCode,
    content_length: Option<u64>,
) -> Result<(), PodcastRankingError> {
    if !status.is_success() {
        return if status.is_server_error() {
            Err(PodcastRankingError::Unavailable)
        } else {
            Err(PodcastRankingError::InvalidResponse)
        };
    }
    if content_length
        .is_some_and(|length| length > u64::try_from(MAX_PODCAST_FEED_BYTES).unwrap_or(u64::MAX))
    {
        return Err(PodcastRankingError::TooLarge);
    }
    Ok(())
}

struct BoundedBody {
    bytes: Vec<u8>,
}

impl BoundedBody {
    fn new(content_length: Option<u64>) -> Result<Self, PodcastRankingError> {
        if content_length.is_some_and(|length| {
            length > u64::try_from(MAX_PODCAST_FEED_BYTES).unwrap_or(u64::MAX)
        }) {
            return Err(PodcastRankingError::TooLarge);
        }
        let capacity = content_length
            .and_then(|length| usize::try_from(length).ok())
            .unwrap_or(0);
        Ok(Self {
            bytes: Vec::with_capacity(capacity),
        })
    }

    fn push(&mut self, chunk: &[u8]) -> Result<(), PodcastRankingError> {
        if self
            .bytes
            .len()
            .checked_add(chunk.len())
            .is_none_or(|length| length > MAX_PODCAST_FEED_BYTES)
        {
            return Err(PodcastRankingError::TooLarge);
        }
        self.bytes.extend_from_slice(chunk);
        Ok(())
    }

    fn finish(self) -> Vec<u8> {
        self.bytes
    }
}

#[derive(Clone)]
struct CacheEntry {
    page: PodcastRecommendationPage,
    fetched_at_millis: i64,
    last_access: u64,
    request_sequence: u64,
}

#[derive(Default)]
struct PodcastRankingCache {
    entries: HashMap<RegionCode, CacheEntry>,
    access_sequence: u64,
    request_sequence: u64,
}

impl PodcastRankingCache {
    fn get(&mut self, region: &RegionCode, now_millis: i64) -> Option<PodcastRecommendationPage> {
        self.access_sequence = self.access_sequence.saturating_add(1);
        let access_sequence = self.access_sequence;
        if let Some(entry) = self.entries.get_mut(region) {
            let age = now_millis.saturating_sub(entry.fetched_at_millis);
            if (0..PODCAST_CACHE_TTL_MILLIS).contains(&age) {
                entry.last_access = access_sequence;
                return Some(entry.page.clone());
            }
        }
        self.entries.remove(region);
        None
    }

    fn next_request_sequence(&mut self) -> Option<u64> {
        self.request_sequence = self.request_sequence.checked_add(1)?;
        Some(self.request_sequence)
    }

    fn insert(
        &mut self,
        region: RegionCode,
        page: PodcastRecommendationPage,
        now_millis: i64,
        request_sequence: u64,
    ) {
        if self
            .entries
            .get(&region)
            .is_some_and(|entry| entry.request_sequence > request_sequence)
        {
            return;
        }
        self.access_sequence = self.access_sequence.saturating_add(1);
        if !self.entries.contains_key(&region)
            && self.entries.len() >= MAX_PODCAST_CACHE_COUNTRIES
            && let Some(evicted) = self
                .entries
                .iter()
                .min_by(|(left_region, left), (right_region, right)| {
                    left.last_access
                        .cmp(&right.last_access)
                        .then_with(|| left_region.as_str().cmp(right_region.as_str()))
                })
                .map(|(region, _)| region.clone())
        {
            self.entries.remove(&evicted);
        }
        self.entries.insert(
            region,
            CacheEntry {
                page,
                fetched_at_millis: now_millis,
                last_access: self.access_sequence,
                request_sequence,
            },
        );
    }
}

pub struct ApplePodcastRankingSource {
    locale: Option<String>,
    transport: Arc<dyn PodcastRankingTransport>,
    clock: Arc<dyn PodcastRankingClock>,
    cache: Mutex<PodcastRankingCache>,
}

impl ApplePodcastRankingSource {
    /// Creates a source with a dedicated hardened HTTP client and captures the
    /// process locale for subsequent `ZZ` region resolution.
    ///
    /// # Errors
    ///
    /// Returns [`PodcastRankingError::Unavailable`] if the HTTP client cannot
    /// be constructed.
    pub fn new() -> Result<Self, PodcastRankingError> {
        Ok(Self::with_dependencies(
            sys_locale::get_locale(),
            Arc::new(ReqwestPodcastRankingTransport::new()?),
            Arc::new(SystemClock),
        ))
    }

    fn with_dependencies(
        locale: Option<String>,
        transport: Arc<dyn PodcastRankingTransport>,
        clock: Arc<dyn PodcastRankingClock>,
    ) -> Self {
        Self {
            locale,
            transport,
            clock,
            cache: Mutex::new(PodcastRankingCache::default()),
        }
    }
}

#[async_trait]
impl PodcastRankingSource for ApplePodcastRankingSource {
    async fn top_shows(
        &self,
        requested: &RegionCode,
    ) -> Result<PodcastRecommendationPage, PodcastRankingError> {
        let region = effective_region(requested, self.locale.as_deref());
        let now_millis = self.clock.now_millis();
        let request_sequence = {
            let mut cache = self
                .cache
                .lock()
                .map_err(|_| PodcastRankingError::Unavailable)?;
            if let Some(page) = cache.get(&region, now_millis) {
                return Ok(page);
            }
            cache
                .next_request_sequence()
                .ok_or(PodcastRankingError::Unavailable)?
        };

        let url = apple_top_shows_url(&region)?;
        let bytes = self.transport.get(url).await?;
        let page = parse_apple_top_shows(&bytes)?;
        if page.region() != &region {
            return Err(PodcastRankingError::InvalidResponse);
        }
        self.cache
            .lock()
            .map_err(|_| PodcastRankingError::Unavailable)?
            .insert(
                region,
                page.clone(),
                self.clock.now_millis(),
                request_sequence,
            );
        Ok(page)
    }
}

fn effective_region(requested: &RegionCode, locale: Option<&str>) -> RegionCode {
    if requested.as_str() != "ZZ" {
        return requested.clone();
    }
    locale
        .and_then(locale_country)
        .and_then(|country| RegionCode::parse(country).ok())
        .unwrap_or_else(|| RegionCode::parse("US").unwrap_or_default())
}

fn locale_country(locale: &str) -> Option<&str> {
    if locale.len() > MAX_OS_LOCALE_BYTES {
        return None;
    }
    let base = locale.split(['.', '@']).next()?;
    let mut subtags = base.split(['_', '-']);
    let language = subtags.next()?;
    if !(2..=8).contains(&language.len())
        || !language.bytes().all(|byte| byte.is_ascii_alphabetic())
    {
        return None;
    }
    let mut country = subtags.next()?;
    if country.len() == 4 && country.bytes().all(|byte| byte.is_ascii_alphabetic()) {
        country = subtags.next()?;
    }
    if country.len() != 2 || !country.bytes().all(|byte| byte.is_ascii_alphabetic()) {
        return None;
    }
    Some(country)
}

fn apple_top_shows_url(region: &RegionCode) -> Result<Url, PodcastRankingError> {
    Url::parse(&format!(
        "https://rss.marketingtools.apple.com/api/v2/{}/podcasts/top/20/podcasts.json",
        region.as_str().to_ascii_lowercase()
    ))
    .map_err(|_| PodcastRankingError::InvalidResponse)
}

#[derive(Deserialize)]
struct AppleDocument {
    feed: AppleFeed,
}

#[derive(Deserialize)]
struct AppleFeed {
    country: Value,
    results: BoundedAppleResults,
}

struct BoundedAppleResults(Vec<PodcastRecommendation>);

impl<'de> Deserialize<'de> for BoundedAppleResults {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ResultsVisitor;

        impl<'de> Visitor<'de> for ResultsVisitor {
            type Value = BoundedAppleResults;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("an array of podcast ranking results")
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut items = Vec::with_capacity(MAX_PODCAST_RECOMMENDATIONS);
                while items.len() < MAX_PODCAST_RECOMMENDATIONS {
                    let Some(result) = sequence.next_element::<AppleResult>()? else {
                        return Ok(BoundedAppleResults(items));
                    };
                    note_deserialized_apple_row();
                    if let Some(item) = recommendation_from_apple(&result, items.len() + 1) {
                        items.push(item);
                    }
                }
                while sequence.next_element::<IgnoredAny>()?.is_some() {}
                Ok(BoundedAppleResults(items))
            }
        }

        deserializer.deserialize_seq(ResultsVisitor)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AppleResult {
    #[serde(default)]
    artist_name: Option<Value>,
    #[serde(default)]
    id: Option<Value>,
    #[serde(default)]
    name: Option<Value>,
    #[serde(default)]
    artwork_url100: Option<Value>,
}

fn note_deserialized_apple_row() {
    #[cfg(test)]
    DESERIALIZED_APPLE_ROWS.set(DESERIALIZED_APPLE_ROWS.get() + 1);
}

#[cfg(test)]
fn reset_deserialized_apple_rows() {
    DESERIALIZED_APPLE_ROWS.set(0);
}

#[cfg(test)]
fn deserialized_apple_rows() -> usize {
    DESERIALIZED_APPLE_ROWS.get()
}

/// Parses a bounded Apple Top Shows response into discovery-only podcast data.
///
/// # Errors
///
/// Returns a redacted [`PodcastRankingError`] when the response exceeds the
/// input limit or does not have the expected feed shape and country code.
pub fn parse_apple_top_shows(
    bytes: &[u8],
) -> Result<PodcastRecommendationPage, PodcastRankingError> {
    if bytes.len() > MAX_PODCAST_FEED_BYTES {
        return Err(PodcastRankingError::TooLarge);
    }

    let document: AppleDocument =
        serde_json::from_slice(bytes).map_err(|_| PodcastRankingError::InvalidResponse)?;
    let country = document
        .feed
        .country
        .as_str()
        .ok_or(PodcastRankingError::InvalidResponse)?
        .trim();
    let region = RegionCode::parse(country).map_err(|_| PodcastRankingError::InvalidResponse)?;
    let items = document.feed.results.0;

    if items.is_empty() {
        return Err(PodcastRankingError::InvalidResponse);
    }

    Ok(PodcastRecommendationPage { region, items })
}

fn recommendation_from_apple(result: &AppleResult, rank: usize) -> Option<PodcastRecommendation> {
    let source_id = bounded_required_text(result.id.as_ref(), MAX_PODCAST_SOURCE_ID_BYTES)?;
    let title = bounded_required_text(result.name.as_ref(), MAX_PODCAST_TEXT_BYTES)?;
    let publisher = bounded_optional_text(result.artist_name.as_ref())?;
    let artwork_url = parse_artwork_url(result.artwork_url100.as_ref());

    Some(PodcastRecommendation {
        source_id: PodcastRecommendationId(source_id),
        rank,
        title,
        publisher,
        artwork_url,
    })
}

fn bounded_required_text(value: Option<&Value>, max_bytes: usize) -> Option<String> {
    let value = value?.as_str()?.trim();
    (!value.is_empty() && value.len() <= max_bytes).then(|| value.to_owned())
}

fn bounded_optional_text(value: Option<&Value>) -> Option<String> {
    let Some(value) = value else {
        return Some(String::new());
    };
    let value = value.as_str()?.trim();
    (value.len() <= MAX_PODCAST_TEXT_BYTES).then(|| value.to_owned())
}

fn parse_artwork_url(value: Option<&Value>) -> Option<ArtworkUrl> {
    let raw = value?.as_str()?.trim();
    if raw.is_empty() || raw.len() > MAX_PODCAST_TEXT_BYTES {
        return None;
    }
    let url = Url::parse(raw).ok()?;
    (url.scheme() == "https")
        .then_some(url)
        .and_then(|url| ArtworkUrl::try_from(url).ok())
}

#[cfg(test)]
mod tests {
    use super::{
        ApplePodcastRankingSource, BoundedBody, MAX_PODCAST_FEED_BYTES,
        MAX_PODCAST_RECOMMENDATIONS, MAX_PODCAST_SOURCE_ID_BYTES, MAX_PODCAST_TEXT_BYTES,
        PodcastRankingClock, PodcastRankingError, PodcastRankingSource, PodcastRankingTransport,
        deserialized_apple_rows, effective_region, match_podcast_recommendation,
        parse_apple_top_shows, reset_deserialized_apple_rows, validate_response_metadata,
    };
    use crate::{
        domain::{MediaId, MediaItem, MediaKind, RegionCode},
        provider::{BrowseItem, SearchItem},
    };
    use async_trait::async_trait;
    use reqwest::StatusCode;
    use serde_json::json;
    use std::{
        collections::HashMap,
        sync::{
            Arc, Mutex,
            atomic::{AtomicI64, AtomicUsize, Ordering},
        },
    };
    use tokio::sync::Notify;
    use url::Url;

    const US_FIXTURE: &str = r#"
        {
          "feed": {
            "country": "us",
            "results": [
              {
                "artistName": " The New York Times ",
                "id": "1200361736",
                "name": " The Daily ",
                "artworkUrl100": "https://example.test/daily.jpg"
              },
              {
                "artistName": " NPR ",
                "id": "510289021",
                "name": " Up First ",
                "artworkUrl100": "http://example.test/up-first.jpg"
              }
            ]
          }
        }
    "#;

    fn recommendation(title: &str, publisher: &str) -> super::PodcastRecommendation {
        let bytes = serde_json::to_vec(&json!({
            "feed": {"country": "us", "results": [{
                "artistName": publisher,
                "id": "source-id",
                "name": title
            }]}
        }))
        .unwrap_or_else(|error| panic!("serialize recommendation fixture: {error}"));
        parse_apple_top_shows(&bytes)
            .unwrap_or_else(|error| panic!("parse recommendation fixture: {error}"))
            .items()[0]
            .clone()
    }

    fn podcast(id: &str, title: &str, creator: Option<&str>) -> SearchItem {
        SearchItem::Podcast(BrowseItem {
            id: id.to_owned(),
            title: title.to_owned(),
            subtitle: creator.map(str::to_owned),
            artwork_url: None,
        })
    }

    #[test]
    fn exact_normalized_title_wins_and_publisher_breaks_ties() {
        let recommendation = recommendation(" The  Daily! ", "The New York Times");
        let candidates = vec![
            podcast("wrong", "The Daily", Some("Another Publisher")),
            podcast("match", "the-daily", Some("New York Times")),
            podcast("weak", "The Daily News", Some("The New York Times")),
        ];

        let matched = match_podcast_recommendation(&recommendation, &candidates)
            .unwrap_or_else(|| panic!("expected an exact normalized match"));

        assert_eq!(matched.as_str(), "match");
    }

    #[test]
    fn canonically_equivalent_titles_and_publishers_match() {
        let recommendation = recommendation("Caf\u{e9} Society", "Cr\u{e8}me Audio");
        let candidates = vec![
            podcast(
                "unrelated",
                "Cafe\u{301} Society",
                Some("Another Publisher"),
            ),
            podcast(
                "canonical",
                "Cafe\u{301} Society",
                Some("Cre\u{300}me Audio"),
            ),
        ];

        let matched = match_podcast_recommendation(&recommendation, &candidates)
            .unwrap_or_else(|| panic!("expected canonical Unicode match"));

        assert_eq!(matched.as_str(), "canonical");
    }

    #[test]
    fn non_latin_combining_marks_match_their_precomposed_form() {
        let recommendation = recommendation("Μ\u{3ac}θημα", "Εκδ\u{f3}της");
        let candidates = [podcast("greek", "Μα\u{301}θημα", Some("Εκδο\u{301}της"))];

        let matched = match_podcast_recommendation(&recommendation, &candidates)
            .unwrap_or_else(|| panic!("expected non-Latin canonical match"));

        assert_eq!(matched.as_str(), "greek");
    }

    #[test]
    fn ambiguous_exact_titles_without_publisher_distinction_are_rejected() {
        let recommendation = recommendation("Show", "Publisher");
        let candidates = vec![
            podcast("one", "show", None),
            podcast("two", "Show!", Some("Unrelated")),
        ];

        assert!(match_podcast_recommendation(&recommendation, &candidates).is_none());
    }

    #[test]
    fn ordering_does_not_change_an_unambiguous_match() {
        let recommendation = recommendation("Show", "Publisher Network");
        let first = podcast("match", "SHOW", Some("Publisher Network"));
        let second = podcast("other", "Show", Some("Another Network"));

        let forward =
            match_podcast_recommendation(&recommendation, &[first.clone(), second.clone()])
                .unwrap_or_else(|| panic!("expected forward match"));
        let reversed = match_podcast_recommendation(&recommendation, &[second, first])
            .unwrap_or_else(|| panic!("expected reversed match"));

        assert_eq!(forward, reversed);
        assert_eq!(forward.as_str(), "match");
    }

    #[test]
    fn weak_non_podcast_and_invalid_provider_ids_are_rejected() {
        let recommendation = recommendation("Exact Show", "Publisher");
        let non_podcast = crate::provider::BrowseItem {
            id: "album-id".to_owned(),
            title: "Exact Show".to_owned(),
            subtitle: Some("Publisher".to_owned()),
            artwork_url: None,
        };
        let invalid = vec![
            podcast("", "Exact Show", Some("Publisher")),
            podcast("contains whitespace", "Exact Show", Some("Publisher")),
            podcast(&"x".repeat(513), "Exact Show", Some("Publisher")),
            SearchItem::Album(non_podcast),
            SearchItem::Playable(MediaItem {
                id: MediaId {
                    provider: "youtube".to_owned(),
                    video_id: "episode".to_owned(),
                },
                kind: MediaKind::PodcastEpisode,
                title: "Exact Show".to_owned(),
                creators: vec!["Publisher".to_owned()],
                collection: None,
                duration_ms: None,
                artwork_url: None,
                explicit: false,
            }),
            podcast("weak", "Exact Shows", Some("Publisher")),
        ];

        assert!(match_podcast_recommendation(&recommendation, &invalid).is_none());
    }

    #[test]
    fn oversized_candidate_fields_are_rejected_before_matching() {
        let recommendation = recommendation("Show", "Publisher");
        let oversized_title = format!("Show{}", "!".repeat(MAX_PODCAST_TEXT_BYTES));
        let oversized_publisher = format!("Publisher{}", "!".repeat(MAX_PODCAST_TEXT_BYTES));

        assert!(
            match_podcast_recommendation(
                &recommendation,
                &[podcast("title", &oversized_title, Some("Publisher"))],
            )
            .is_none()
        );
        assert!(
            match_podcast_recommendation(
                &recommendation,
                &[podcast("publisher", "Show", Some(&oversized_publisher))],
            )
            .is_none()
        );
    }

    #[test]
    fn matcher_outputs_remain_secret_safe() {
        let recommendation = recommendation("Sensitive title", "Sensitive publisher");
        let matched = match_podcast_recommendation(
            &recommendation,
            &[podcast(
                "sensitive-provider-id",
                "Sensitive title",
                Some("Sensitive publisher"),
            )],
        )
        .unwrap_or_else(|| panic!("expected match"));

        let recommendation_debug = format!("{recommendation:?}");
        let matched_debug = format!("{matched:?}");
        for sensitive in [
            "Sensitive title",
            "Sensitive publisher",
            "sensitive-provider-id",
        ] {
            assert!(!recommendation_debug.contains(sensitive));
            assert!(!matched_debug.contains(sensitive));
        }
    }

    fn region(value: &str) -> RegionCode {
        RegionCode::parse(value).unwrap_or_else(|error| panic!("invalid test region: {error}"))
    }

    fn fixture(country: &str) -> Vec<u8> {
        fixture_with_title(country, "Show")
    }

    fn fixture_with_title(country: &str, title: &str) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "feed": {
                "country": country,
                "results": [{"artistName": "Publisher", "id": "id", "name": title}]
            }
        }))
        .unwrap_or_else(|error| panic!("fixture serialization failed: {error}"))
    }

    #[derive(Default)]
    struct FakeClock(AtomicI64);

    impl FakeClock {
        fn set(&self, now_millis: i64) {
            self.0.store(now_millis, Ordering::SeqCst);
        }
    }

    impl PodcastRankingClock for FakeClock {
        fn now_millis(&self) -> i64 {
            self.0.load(Ordering::SeqCst)
        }
    }

    #[derive(Default)]
    struct FakeTransport {
        urls: Mutex<Vec<Url>>,
        failures: Mutex<HashMap<String, PodcastRankingError>>,
        responses: Mutex<HashMap<String, Vec<u8>>>,
    }

    impl FakeTransport {
        fn request_count(&self) -> usize {
            self.urls.lock().map_or(0, |urls| urls.len())
        }

        fn requested_urls(&self) -> Vec<String> {
            self.urls.lock().map_or_else(
                |_| Vec::new(),
                |urls| urls.iter().map(Url::to_string).collect(),
            )
        }

        fn fail_country(&self, country: &str, error: PodcastRankingError) {
            if let Ok(mut failures) = self.failures.lock() {
                failures.insert(country.to_owned(), error);
            }
        }

        fn respond_to_country(&self, country: &str, bytes: Vec<u8>) {
            if let Ok(mut responses) = self.responses.lock() {
                responses.insert(country.to_owned(), bytes);
            }
        }
    }

    #[async_trait]
    impl PodcastRankingTransport for FakeTransport {
        async fn get(&self, url: Url) -> Result<Vec<u8>, PodcastRankingError> {
            if let Ok(mut urls) = self.urls.lock() {
                urls.push(url.clone());
            }
            let country = url
                .path_segments()
                .and_then(|mut segments| segments.nth(2))
                .unwrap_or("us");
            if let Some(error) = self
                .failures
                .lock()
                .ok()
                .and_then(|failures| failures.get(country).copied())
            {
                return Err(error);
            }
            if let Some(bytes) = self
                .responses
                .lock()
                .ok()
                .and_then(|responses| responses.get(country).cloned())
            {
                return Ok(bytes);
            }
            Ok(fixture(country))
        }
    }

    fn test_source(
        locale: Option<&str>,
        transport: Arc<dyn PodcastRankingTransport>,
        clock: Arc<dyn PodcastRankingClock>,
    ) -> ApplePodcastRankingSource {
        ApplePodcastRankingSource::with_dependencies(locale.map(str::to_owned), transport, clock)
    }

    #[test]
    fn configured_country_wins_over_locale() {
        assert_eq!(
            effective_region(&region("JP"), Some("en_US.UTF-8")),
            region("JP")
        );
    }

    #[test]
    fn zz_detects_country_from_os_locale_or_falls_back_to_us() {
        for (locale, expected) in [
            (Some("zh_HK"), "HK"),
            (Some("en-US"), "US"),
            (Some("zh-Hant-HK"), "HK"),
            (Some("sr-Latn-RS"), "RS"),
            (Some("en-US-u-hc-h12"), "US"),
            (Some("C"), "US"),
            (Some("not-a-locale"), "US"),
            (None, "US"),
        ] {
            assert_eq!(effective_region(&region("ZZ"), locale), region(expected));
        }
    }

    struct OrderedSameCountryTransport {
        requests: AtomicUsize,
        first_started: Notify,
        release_first: Notify,
        second_fails: bool,
    }

    #[async_trait]
    impl PodcastRankingTransport for OrderedSameCountryTransport {
        async fn get(&self, url: Url) -> Result<Vec<u8>, PodcastRankingError> {
            let country = url
                .path_segments()
                .and_then(|mut segments| segments.nth(2))
                .unwrap_or("us")
                .to_owned();
            let request = self.requests.fetch_add(1, Ordering::SeqCst);
            if request == 0 {
                self.first_started.notify_one();
                self.release_first.notified().await;
                return Ok(fixture_with_title(&country, "Older response"));
            }
            if self.second_fails {
                return Err(PodcastRankingError::Unavailable);
            }
            Ok(fixture_with_title(&country, "Newer response"))
        }
    }

    #[tokio::test]
    async fn older_same_country_completion_cannot_replace_newer_success()
    -> Result<(), Box<dyn std::error::Error>> {
        let transport = Arc::new(OrderedSameCountryTransport {
            requests: AtomicUsize::new(0),
            first_started: Notify::new(),
            release_first: Notify::new(),
            second_fails: false,
        });
        let source = Arc::new(test_source(
            None,
            transport.clone(),
            Arc::new(FakeClock::default()),
        ));
        let first_source = source.clone();
        let first = tokio::spawn(async move { first_source.top_shows(&region("JP")).await });
        transport.first_started.notified().await;

        let second_page = source.top_shows(&region("JP")).await?;
        assert_eq!(second_page.items()[0].title(), "Newer response");
        transport.release_first.notify_one();
        let first_page = first.await??;
        assert_eq!(first_page.items()[0].title(), "Older response");

        let cached = source.top_shows(&region("JP")).await?;
        assert_eq!(cached.items()[0].title(), "Newer response");
        assert_eq!(transport.requests.load(Ordering::SeqCst), 2);
        Ok(())
    }

    #[tokio::test]
    async fn older_same_country_completion_can_fill_cache_after_newer_failure()
    -> Result<(), Box<dyn std::error::Error>> {
        let transport = Arc::new(OrderedSameCountryTransport {
            requests: AtomicUsize::new(0),
            first_started: Notify::new(),
            release_first: Notify::new(),
            second_fails: true,
        });
        let source = Arc::new(test_source(
            None,
            transport.clone(),
            Arc::new(FakeClock::default()),
        ));
        let first_source = source.clone();
        let first = tokio::spawn(async move { first_source.top_shows(&region("JP")).await });
        transport.first_started.notified().await;

        assert_eq!(
            source.top_shows(&region("JP")).await,
            Err(PodcastRankingError::Unavailable)
        );
        transport.release_first.notify_one();
        let first_page = first.await??;
        assert_eq!(first_page.items()[0].title(), "Older response");

        let cached = source.top_shows(&region("JP")).await?;
        assert_eq!(cached.items()[0].title(), "Older response");
        assert_eq!(transport.requests.load(Ordering::SeqCst), 2);
        Ok(())
    }

    #[tokio::test]
    async fn top_shows_uses_exact_lowercase_country_url_and_one_hour_cache()
    -> Result<(), Box<dyn std::error::Error>> {
        let transport = Arc::new(FakeTransport::default());
        let clock = Arc::new(FakeClock::default());
        let source = test_source(Some("en_US.UTF-8"), transport.clone(), clock.clone());

        source.top_shows(&region("JP")).await?;
        clock.set(3_599_999);
        source.top_shows(&region("JP")).await?;
        assert_eq!(transport.request_count(), 1);
        assert_eq!(
            transport.requested_urls(),
            ["https://rss.marketingtools.apple.com/api/v2/jp/podcasts/top/20/podcasts.json"]
        );

        clock.set(3_600_000);
        source.top_shows(&region("JP")).await?;
        assert_eq!(transport.request_count(), 2);
        Ok(())
    }

    #[tokio::test]
    async fn cache_is_capped_at_sixteen_countries_with_deterministic_eviction()
    -> Result<(), Box<dyn std::error::Error>> {
        let transport = Arc::new(FakeTransport::default());
        let clock = Arc::new(FakeClock::default());
        let source = test_source(None, transport.clone(), clock.clone());

        for suffix in b'A'..=b'P' {
            source
                .top_shows(&region(&format!("A{}", char::from(suffix))))
                .await?;
        }
        source.top_shows(&region("AA")).await?;
        source.top_shows(&region("AQ")).await?;
        source.top_shows(&region("AA")).await?;
        source.top_shows(&region("AB")).await?;

        assert_eq!(transport.request_count(), 18);
        Ok(())
    }

    #[tokio::test]
    async fn failures_are_typed_secret_safe_and_not_cached() {
        let transport = Arc::new(FakeTransport::default());
        transport.fail_country("jp", PodcastRankingError::Unavailable);
        transport.respond_to_country("hk", br#"{"secret":"do-not-print"}"#.to_vec());
        transport.respond_to_country("kr", vec![b'x'; MAX_PODCAST_FEED_BYTES + 1]);
        let clock = Arc::new(FakeClock::default());
        let source = test_source(None, transport.clone(), clock);

        for expected in [
            PodcastRankingError::Unavailable,
            PodcastRankingError::Unavailable,
        ] {
            let Err(error) = source.top_shows(&region("JP")).await else {
                panic!("transport failure unexpectedly succeeded");
            };
            assert_eq!(error, expected);
            let rendered = format!("{error:?} {error}");
            assert!(!rendered.contains("secret"));
            assert!(!rendered.contains("marketingtools"));
        }
        for (country, expected) in [
            ("HK", PodcastRankingError::InvalidResponse),
            ("HK", PodcastRankingError::InvalidResponse),
            ("KR", PodcastRankingError::TooLarge),
            ("KR", PodcastRankingError::TooLarge),
        ] {
            let error = source.top_shows(&region(country)).await;
            assert_eq!(error, Err(expected));
        }
        assert_eq!(transport.request_count(), 6);
    }

    #[test]
    fn status_oversize_and_malformed_failures_are_typed() {
        assert_eq!(
            validate_response_metadata(StatusCode::SERVICE_UNAVAILABLE, None),
            Err(PodcastRankingError::Unavailable)
        );
        assert_eq!(
            validate_response_metadata(
                StatusCode::OK,
                Some(u64::try_from(MAX_PODCAST_FEED_BYTES).unwrap_or(u64::MAX) + 1)
            ),
            Err(PodcastRankingError::TooLarge)
        );
        assert_eq!(
            parse_apple_top_shows(br#"{"secret":"do-not-print"}"#),
            Err(PodcastRankingError::InvalidResponse)
        );
    }

    #[test]
    fn body_collection_enforces_cap_without_content_length() {
        let mut body = BoundedBody::new(None).unwrap_or_else(|error| panic!("{error}"));
        body.push(&vec![b'a'; MAX_PODCAST_FEED_BYTES])
            .unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(body.push(b"x"), Err(PodcastRankingError::TooLarge));

        let mut incorrect_length =
            BoundedBody::new(Some(1)).unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(
            incorrect_length.push(&vec![b'x'; MAX_PODCAST_FEED_BYTES + 1]),
            Err(PodcastRankingError::TooLarge)
        );
    }

    struct ProgressTransport {
        started: Notify,
        release: Notify,
        other_country_started: Notify,
        urls: Mutex<Vec<Url>>,
    }

    #[async_trait]
    impl PodcastRankingTransport for ProgressTransport {
        async fn get(&self, url: Url) -> Result<Vec<u8>, PodcastRankingError> {
            let country = url
                .path_segments()
                .and_then(|mut segments| segments.nth(2))
                .unwrap_or("us")
                .to_owned();
            if let Ok(mut urls) = self.urls.lock() {
                urls.push(url);
            }
            if country == "jp" {
                self.started.notify_one();
                self.release.notified().await;
            } else {
                self.other_country_started.notify_one();
            }
            Ok(fixture(&country))
        }
    }

    #[tokio::test]
    async fn cache_mutex_is_not_held_across_transport_await()
    -> Result<(), Box<dyn std::error::Error>> {
        let transport = Arc::new(ProgressTransport {
            started: Notify::new(),
            release: Notify::new(),
            other_country_started: Notify::new(),
            urls: Mutex::new(Vec::new()),
        });
        let source = Arc::new(test_source(
            None,
            transport.clone(),
            Arc::new(FakeClock::default()),
        ));
        let jp_source = source.clone();
        let jp = tokio::spawn(async move { jp_source.top_shows(&region("JP")).await });
        transport.started.notified().await;

        let us_source = source.clone();
        let us = tokio::spawn(async move { us_source.top_shows(&region("US")).await });
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            transport.other_country_started.notified(),
        )
        .await
        .unwrap_or_else(|_| panic!("second country transport was blocked by the cache mutex"));
        let us_page = us.await??;
        assert_eq!(us_page.region(), &region("US"));

        transport.release.notify_one();
        jp.await??;
        Ok(())
    }

    #[test]
    fn parses_country_rank_title_publisher_and_https_artwork()
    -> Result<(), Box<dyn std::error::Error>> {
        let page = parse_apple_top_shows(US_FIXTURE.as_bytes())?;

        assert_eq!(page.region(), &RegionCode::parse("US")?);
        assert_eq!(page.items().len(), 2);
        assert_eq!(page.items()[0].rank(), 1);
        assert_eq!(page.items()[0].title(), "The Daily");
        assert_eq!(page.items()[0].publisher(), "The New York Times");
        assert!(page.items()[0].artwork_url().is_some());
        assert_eq!(page.items()[1].rank(), 2);
        assert!(page.items()[1].artwork_url().is_none());
        Ok(())
    }

    #[test]
    fn drops_invalid_rows_and_caps_results_at_twenty() -> Result<(), Box<dyn std::error::Error>> {
        let mut results = vec![
            json!({"id": "", "name": "Missing ID"}),
            json!({"id": "missing-title", "name": "  "}),
            json!({"id": "x".repeat(129), "name": "Oversized ID"}),
            json!({"id": "oversized-title", "name": "x".repeat(513)}),
        ];
        results.extend((0..25).map(|index| {
            json!({
                "artistName": format!(" Publisher {index} "),
                "id": format!("id-{index}"),
                "name": format!(" Show {index} ")
            })
        }));
        let bytes = serde_json::to_vec(&json!({
            "feed": {"country": "jp", "results": results}
        }))?;

        let page = parse_apple_top_shows(&bytes)?;

        assert_eq!(page.items().len(), MAX_PODCAST_RECOMMENDATIONS);
        assert_eq!(page.items()[0].rank(), 1);
        assert_eq!(page.items()[0].title(), "Show 0");
        assert_eq!(page.items()[19].rank(), 20);
        assert_eq!(page.items()[19].title(), "Show 19");
        Ok(())
    }

    #[test]
    fn stops_deserializing_rows_after_twenty_valid_results()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut results = (0..MAX_PODCAST_RECOMMENDATIONS)
            .map(|index| json!({"id": format!("id-{index}"), "name": format!("Show {index}")}))
            .collect::<Vec<_>>();
        results.extend((0..200).map(|index| {
            json!({
                "id": format!("ignored-{index}"),
                "name": format!("Ignored {index}"),
                "unused": [index, index, index]
            })
        }));
        let bytes = serde_json::to_vec(&json!({
            "feed": {"country": "us", "results": results}
        }))?;
        reset_deserialized_apple_rows();

        let page = parse_apple_top_shows(&bytes)?;

        assert_eq!(page.items().len(), MAX_PODCAST_RECOMMENDATIONS);
        assert_eq!(deserialized_apple_rows(), MAX_PODCAST_RECOMMENDATIONS);
        Ok(())
    }

    #[test]
    fn rejects_empty_invalid_and_oversized_feeds() {
        assert_eq!(
            parse_apple_top_shows(b""),
            Err(PodcastRankingError::InvalidResponse)
        );
        assert_eq!(
            parse_apple_top_shows(br#"{"feed":[]}"#),
            Err(PodcastRankingError::InvalidResponse)
        );
        assert_eq!(
            parse_apple_top_shows(br#"{"feed":{"country":"us","results":[]}}"#),
            Err(PodcastRankingError::InvalidResponse)
        );
        assert_eq!(
            parse_apple_top_shows(br#"{"feed":{"country":"us","results":[{"id":"","name":""}]}}"#),
            Err(PodcastRankingError::InvalidResponse)
        );
        assert_eq!(
            parse_apple_top_shows(&vec![b' '; MAX_PODCAST_FEED_BYTES + 1]),
            Err(PodcastRankingError::TooLarge)
        );
    }

    #[test]
    fn rejects_invalid_country_and_non_string_required_text() {
        let cases: [(&str, &[u8]); 4] = [
            (
                "invalid country",
                br#"{"feed":{"country":"usa","results":[{"id":"id","name":"Show"}]}}"#,
            ),
            (
                "non-string ID",
                br#"{"feed":{"country":"us","results":[{"id":7,"name":"Show"}]}}"#,
            ),
            (
                "non-string title",
                br#"{"feed":{"country":"us","results":[{"id":"id","name":[]}]}}"#,
            ),
            (
                "non-string publisher",
                br#"{"feed":{"country":"us","results":[{"artistName":{},"id":"id","name":"Show"}]}}"#,
            ),
        ];

        for (label, bytes) in cases {
            assert_eq!(
                parse_apple_top_shows(bytes),
                Err(PodcastRankingError::InvalidResponse),
                "{label}"
            );
        }
    }

    #[test]
    fn enforces_optional_text_boundaries_and_accepts_exact_limits()
    -> Result<(), Box<dyn std::error::Error>> {
        let oversized_publisher = serde_json::to_vec(&json!({
            "feed": {"country": "us", "results": [{
                "artistName": "p".repeat(MAX_PODCAST_TEXT_BYTES + 1),
                "id": "id",
                "name": "Show"
            }]}
        }))?;
        assert_eq!(
            parse_apple_top_shows(&oversized_publisher),
            Err(PodcastRankingError::InvalidResponse)
        );

        let optional_fields_invalid = serde_json::to_vec(&json!({
            "feed": {"country": "us", "results": [{
                "artistName": "Publisher",
                "id": "id",
                "name": "Show",
                "artworkUrl100": "a".repeat(MAX_PODCAST_TEXT_BYTES + 1)
            }, {
                "artistName": "Publisher 2",
                "id": "id-2",
                "name": "Show 2",
                "artworkUrl100": 42
            }]}
        }))?;
        let page = parse_apple_top_shows(&optional_fields_invalid)?;
        assert!(page.items().iter().all(|item| item.artwork_url().is_none()));

        let artwork_prefix = "https://example.test/";
        let artwork = format!(
            "{artwork_prefix}{}",
            "a".repeat(MAX_PODCAST_TEXT_BYTES - artwork_prefix.len())
        );
        let title = "t".repeat(MAX_PODCAST_TEXT_BYTES);
        let publisher = "p".repeat(MAX_PODCAST_TEXT_BYTES);
        let exact_limits = serde_json::to_vec(&json!({
            "feed": {"country": "hk", "results": [{
                "artistName": publisher,
                "id": "i".repeat(MAX_PODCAST_SOURCE_ID_BYTES),
                "name": title,
                "artworkUrl100": artwork
            }]}
        }))?;
        let page = parse_apple_top_shows(&exact_limits)?;
        assert_eq!(page.items()[0].title().len(), MAX_PODCAST_TEXT_BYTES);
        assert_eq!(page.items()[0].publisher().len(), MAX_PODCAST_TEXT_BYTES);
        assert!(page.items()[0].artwork_url().is_some());
        Ok(())
    }

    #[test]
    fn recommendation_debug_redacts_source_identity() -> Result<(), Box<dyn std::error::Error>> {
        let page = parse_apple_top_shows(US_FIXTURE.as_bytes())?;
        let recommendation = &page.items()[0];

        let id_debug = format!("{:?}", recommendation.source_id());
        let recommendation_debug = format!("{recommendation:?}");

        assert!(!id_debug.contains("1200361736"));
        assert!(!recommendation_debug.contains("1200361736"));
        assert!(!recommendation_debug.contains("The Daily"));
        assert!(!recommendation_debug.contains("The New York Times"));
        assert!(!recommendation_debug.contains("daily.jpg"));
        assert!(recommendation_debug.contains("rank"));
        Ok(())
    }
}
