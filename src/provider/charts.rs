use std::fmt;

use serde_json::Value;
use thiserror::Error;
use url::Url;

use crate::domain::{MediaId, MediaItem, MediaKind, RegionCode};

use super::{ChartSection, Page, SearchItem};

/// Maximum accepted encoded provider response size: one mebibyte.
pub const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
/// Maximum number of recognized sections in one response.
pub const MAX_SECTIONS: usize = 64;
/// Maximum number of entries in one recognized shelf.
pub const MAX_ITEMS_PER_SHELF: usize = 256;
/// Maximum number of creator and collection runs inspected for one item.
pub const MAX_METADATA_RUNS: usize = 64;
/// Maximum number of artwork candidates inspected for one item.
pub const MAX_THUMBNAILS: usize = 32;
/// Maximum number of structured warnings retained for one response.
pub const MAX_WARNINGS: usize = 128;
/// Maximum encoded byte length of an opaque playable video identifier.
pub const MAX_VIDEO_ID_BYTES: usize = 128;
const MAX_PLAYLIST_ID_BYTES: usize = 512;

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct ChartPlaylistReference {
    title: String,
    playlist_id: String,
}

impl ChartPlaylistReference {
    #[must_use]
    pub(crate) fn new(title: impl Into<String>, playlist_id: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            playlist_id: playlist_id.into(),
        }
    }

    #[must_use]
    #[cfg(test)]
    pub(crate) fn title(&self) -> &str {
        &self.title
    }

    #[must_use]
    #[cfg(test)]
    pub(crate) fn playlist_id(&self) -> &str {
        &self.playlist_id
    }

    pub(crate) fn into_parts(self) -> (String, String) {
        (self.title, self.playlist_id)
    }
}

impl fmt::Debug for ChartPlaylistReference {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ChartPlaylistReference")
            .field("title_redacted", &true)
            .field("playlist_id_redacted", &true)
            .finish()
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ChartCacheKey {
    region: RegionCode,
}

impl ChartCacheKey {
    #[must_use]
    pub const fn new(region: RegionCode) -> Self {
        Self { region }
    }

    #[must_use]
    pub const fn region(&self) -> &RegionCode {
        &self.region
    }
}

impl fmt::Display for ChartCacheKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "charts:{}", self.region)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ParseWarningKind {
    MissingVideoId,
    InvalidVideoId,
    ConflictingVideoId,
    MissingTitle,
    UnsupportedMediaKind,
    InvalidDuration,
    InvalidArtworkUrl,
    UnsupportedItemRenderer,
    ResourceLimit { resource: ParseResource },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ParseWarning {
    pub section_index: usize,
    pub item_index: usize,
    pub kind: ParseWarningKind,
}

#[derive(Clone, Eq, PartialEq)]
pub struct ParseReport<T> {
    pub value: T,
    pub warnings: Vec<ParseWarning>,
}

impl<T> fmt::Debug for ParseReport<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ParseReport")
            .field("value_redacted", &true)
            .field("warning_count", &self.warnings.len())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ParseResource {
    Sections,
    Items,
    MetadataRuns,
    Thumbnails,
    Warnings,
}

impl fmt::Display for ParseResource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Sections => "sections",
            Self::Items => "items",
            Self::MetadataRuns => "metadata runs",
            Self::Thumbnails => "thumbnails",
            Self::Warnings => "warnings",
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ParseError {
    #[error("{response} response exceeds the input limit")]
    InputTooLarge { response: &'static str },
    #[error("invalid JSON in {response} response")]
    InvalidJson { response: &'static str },
    #[error("unsupported {response} response shape")]
    UnsupportedShape { response: &'static str },
    #[error("{response} response exceeds the {resource} limit")]
    ResourceLimit {
        response: &'static str,
        resource: ParseResource,
    },
    #[error("{response} response contains no usable items")]
    UnusableResponse { response: &'static str },
}

/// Parses a normalized chart response.
///
/// # Errors
///
/// Returns a redacted [`ParseError`] when the response is not JSON or does not
/// contain a recognized `YouTube` Music chart container.
pub fn parse_chart_response(bytes: &[u8]) -> Result<ParseReport<Vec<ChartSection>>, ParseError> {
    let document = parse_document(bytes, "charts")?;
    let sections = section_list(&document, "singleColumnBrowseResultsRenderer")
        .ok_or(ParseError::UnsupportedShape { response: "charts" })?;
    ensure_within_limit(
        sections.len(),
        MAX_SECTIONS,
        "charts",
        ParseResource::Sections,
    )?;
    let mut parsed_sections = Vec::new();
    let mut warnings = Vec::new();
    let mut found_chart_shelf = false;
    let mut saw_candidate = false;
    let mut usable_items = 0_usize;

    for (section_index, section) in sections.iter().enumerate() {
        let Some(shelf) = section.get("musicCarouselShelfRenderer") else {
            continue;
        };
        let Some(contents) = shelf.get("contents").and_then(Value::as_array) else {
            continue;
        };
        found_chart_shelf = true;
        saw_candidate |= !contents.is_empty();
        ensure_within_limit(
            contents.len(),
            MAX_ITEMS_PER_SHELF,
            "charts",
            ParseResource::Items,
        )?;

        let title = chart_title(shelf).unwrap_or_else(|| "Charts".to_owned());
        let items = parse_shelf_items(contents, section_index, &mut warnings, "charts")?;
        usable_items = usable_items.saturating_add(items.len());
        parsed_sections.push(ChartSection { title, items });
    }

    if !found_chart_shelf {
        return Err(ParseError::UnsupportedShape { response: "charts" });
    }
    if saw_candidate && usable_items == 0 {
        return Err(ParseError::UnusableResponse { response: "charts" });
    }

    Ok(ParseReport {
        value: parsed_sections,
        warnings,
    })
}

pub(crate) fn parse_chart_playlist_references(
    bytes: &[u8],
) -> Result<Vec<ChartPlaylistReference>, ParseError> {
    let document = parse_document(bytes, "charts")?;
    let sections = section_list(&document, "singleColumnBrowseResultsRenderer")
        .ok_or(ParseError::UnsupportedShape { response: "charts" })?;
    ensure_within_limit(
        sections.len(),
        MAX_SECTIONS,
        "charts",
        ParseResource::Sections,
    )?;

    let mut references = Vec::new();
    let mut found_chart_shelf = false;
    for section in sections {
        let Some(contents) = section
            .get("musicCarouselShelfRenderer")
            .and_then(|shelf| shelf.get("contents"))
            .and_then(Value::as_array)
        else {
            continue;
        };
        found_chart_shelf = true;
        ensure_within_limit(
            contents.len(),
            MAX_ITEMS_PER_SHELF,
            "charts",
            ParseResource::Items,
        )?;

        for content in contents {
            let Some(reference) = chart_playlist_reference(content) else {
                continue;
            };
            references.push(reference);
            ensure_within_limit(
                references.len(),
                MAX_SECTIONS,
                "charts",
                ParseResource::Sections,
            )?;
        }
    }

    if !found_chart_shelf {
        return Err(ParseError::UnsupportedShape { response: "charts" });
    }
    if references.is_empty() {
        return Err(ParseError::UnusableResponse { response: "charts" });
    }
    Ok(references)
}

/// Parses normalized playable items from a search response.
///
/// # Errors
///
/// Returns a redacted [`ParseError`] when the response is not JSON or does not
/// contain a recognized `YouTube` Music search container.
pub fn parse_search_response(bytes: &[u8]) -> Result<ParseReport<Page<SearchItem>>, ParseError> {
    let document = parse_document(bytes, "search")?;
    let sections = section_list(&document, "tabbedSearchResultsRenderer")
        .ok_or(ParseError::UnsupportedShape { response: "search" })?;
    ensure_within_limit(
        sections.len(),
        MAX_SECTIONS,
        "search",
        ParseResource::Sections,
    )?;
    let mut items = Vec::new();
    let mut warnings = Vec::new();
    let mut found_search_shelf = false;
    let mut saw_candidate = false;

    for (section_index, section) in sections.iter().enumerate() {
        let Some(shelf) = section.get("musicShelfRenderer") else {
            continue;
        };
        let Some(contents) = shelf.get("contents").and_then(Value::as_array) else {
            continue;
        };
        found_search_shelf = true;
        saw_candidate |= !contents.is_empty();
        ensure_within_limit(
            contents.len(),
            MAX_ITEMS_PER_SHELF,
            "search",
            ParseResource::Items,
        )?;
        items.extend(
            parse_shelf_items(contents, section_index, &mut warnings, "search")?
                .into_iter()
                .map(SearchItem::Playable),
        );
    }

    if !found_search_shelf {
        return Err(ParseError::UnsupportedShape { response: "search" });
    }
    if saw_candidate && items.is_empty() {
        return Err(ParseError::UnusableResponse { response: "search" });
    }

    Ok(ParseReport {
        value: Page {
            items,
            continuation: None,
            stale: false,
        },
        warnings,
    })
}

fn parse_document(bytes: &[u8], response: &'static str) -> Result<Value, ParseError> {
    if bytes.len() > MAX_RESPONSE_BYTES {
        return Err(ParseError::InputTooLarge { response });
    }
    serde_json::from_slice(bytes).map_err(|_| ParseError::InvalidJson { response })
}

fn ensure_within_limit(
    actual: usize,
    limit: usize,
    response: &'static str,
    resource: ParseResource,
) -> Result<(), ParseError> {
    if actual > limit {
        return Err(ParseError::ResourceLimit { response, resource });
    }
    Ok(())
}

fn section_list<'a>(document: &'a Value, renderer_name: &str) -> Option<&'a [Value]> {
    let tabs = document
        .get("contents")?
        .get(renderer_name)?
        .get("tabs")?
        .as_array()?;

    tabs.iter().find_map(|tab| {
        tab.get("tabRenderer")?
            .get("content")?
            .get("sectionListRenderer")?
            .get("contents")?
            .as_array()
            .map(Vec::as_slice)
    })
}

fn chart_title(shelf: &Value) -> Option<String> {
    formatted_text(
        shelf
            .get("header")?
            .get("musicCarouselShelfBasicHeaderRenderer")?
            .get("title")?,
    )
}

fn chart_playlist_reference(content: &Value) -> Option<ChartPlaylistReference> {
    let title = content.get("musicTwoRowItemRenderer")?.get("title")?;
    let rendered_title = formatted_text(title)?;
    if rendered_title.trim().is_empty() {
        return None;
    }
    let browse_id = title.get("runs")?.as_array()?.iter().find_map(|run| {
        run.get("navigationEndpoint")?
            .get("browseEndpoint")?
            .get("browseId")?
            .as_str()
    })?;
    let playlist_id = browse_id.strip_prefix("VL")?;
    if playlist_id.is_empty()
        || playlist_id.len() > MAX_PLAYLIST_ID_BYTES
        || playlist_id
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
    {
        return None;
    }
    Some(ChartPlaylistReference::new(rendered_title, playlist_id))
}

fn parse_shelf_items(
    contents: &[Value],
    section_index: usize,
    warnings: &mut Vec<ParseWarning>,
    response: &'static str,
) -> Result<Vec<MediaItem>, ParseError> {
    let mut items = Vec::new();

    for (item_index, content) in contents.iter().enumerate() {
        let Some(renderer) = content.get("musicResponsiveListItemRenderer") else {
            push_warning(
                warnings,
                ParseWarning {
                    section_index,
                    item_index,
                    kind: ParseWarningKind::UnsupportedItemRenderer,
                },
                response,
            )?;
            continue;
        };

        match parse_media_item(renderer) {
            Ok(parsed) => {
                for kind in parsed.warnings {
                    push_warning(
                        warnings,
                        ParseWarning {
                            section_index,
                            item_index,
                            kind,
                        },
                        response,
                    )?;
                }
                items.push(parsed.item);
            }
            Err(kind) => push_warning(
                warnings,
                ParseWarning {
                    section_index,
                    item_index,
                    kind,
                },
                response,
            )?,
        }
    }

    Ok(items)
}

fn push_warning(
    warnings: &mut Vec<ParseWarning>,
    warning: ParseWarning,
    response: &'static str,
) -> Result<(), ParseError> {
    if warnings.len() >= MAX_WARNINGS {
        return Err(ParseError::ResourceLimit {
            response,
            resource: ParseResource::Warnings,
        });
    }
    warnings.push(warning);
    Ok(())
}

struct ParsedMediaItem {
    item: MediaItem,
    warnings: Vec<ParseWarningKind>,
}

fn parse_media_item(renderer: &Value) -> Result<ParsedMediaItem, ParseWarningKind> {
    let video_id = video_id(renderer)?;
    let title = flex_column_text(renderer, 0).ok_or(ParseWarningKind::MissingTitle)?;
    let kind = media_kind(renderer)?;
    let metadata_runs = metadata_runs(renderer)?;
    let (duration_ms, duration_warning) = duration(renderer);
    let (artwork_url, artwork_warning) = artwork(renderer);
    let warnings = duration_warning
        .into_iter()
        .chain(artwork_warning)
        .collect();

    Ok(ParsedMediaItem {
        item: MediaItem {
            id: MediaId {
                provider: "youtube-music".to_owned(),
                video_id,
            },
            kind,
            title,
            creators: creators(&metadata_runs),
            collection: collection(&metadata_runs),
            duration_ms,
            artwork_url,
            explicit: is_explicit(renderer),
        },
        warnings,
    })
}

fn video_id(renderer: &Value) -> Result<String, ParseWarningKind> {
    let playlist_id = renderer
        .get("playlistItemData")
        .and_then(|data| data.get("videoId"));
    let endpoint_id = watch_endpoint(renderer).and_then(|endpoint| endpoint.get("videoId"));
    match (playlist_id, endpoint_id) {
        (Some(playlist_id), Some(endpoint_id)) => {
            let playlist_id = validate_video_id(playlist_id)?;
            let endpoint_id = validate_video_id(endpoint_id)?;
            if playlist_id != endpoint_id {
                return Err(ParseWarningKind::ConflictingVideoId);
            }
            Ok(playlist_id.to_owned())
        }
        (Some(video_id), None) | (None, Some(video_id)) => {
            Ok(validate_video_id(video_id)?.to_owned())
        }
        (None, None) => Err(ParseWarningKind::MissingVideoId),
    }
}

fn validate_video_id(candidate: &Value) -> Result<&str, ParseWarningKind> {
    let raw = candidate.as_str().ok_or(ParseWarningKind::InvalidVideoId)?;
    if raw.is_empty()
        || raw.len() > MAX_VIDEO_ID_BYTES
        || raw
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
    {
        return Err(ParseWarningKind::InvalidVideoId);
    }
    Ok(raw)
}

fn watch_endpoint(renderer: &Value) -> Option<&Value> {
    renderer
        .get("overlay")?
        .get("musicItemThumbnailOverlayRenderer")?
        .get("content")?
        .get("musicPlayButtonRenderer")?
        .get("playNavigationEndpoint")?
        .get("watchEndpoint")
}

fn media_kind(renderer: &Value) -> Result<MediaKind, ParseWarningKind> {
    let video_type = watch_endpoint(renderer)
        .and_then(|endpoint| endpoint.get("watchEndpointMusicSupportedConfigs"))
        .and_then(|configs| configs.get("watchEndpointMusicConfig"))
        .and_then(|config| config.get("musicVideoType"))
        .and_then(Value::as_str);

    match video_type {
        Some("MUSIC_VIDEO_TYPE_ATV") => Ok(MediaKind::Song),
        Some("MUSIC_VIDEO_TYPE_UGC" | "MUSIC_VIDEO_TYPE_OMV") => Ok(MediaKind::Video),
        Some("MUSIC_VIDEO_TYPE_PODCAST_EPISODE") => Ok(MediaKind::PodcastEpisode),
        _ => Err(ParseWarningKind::UnsupportedMediaKind),
    }
}

fn flex_column_text(renderer: &Value, index: usize) -> Option<String> {
    let column = renderer.get("flexColumns")?.as_array()?.get(index)?;
    formatted_text(
        column
            .get("musicResponsiveListItemFlexColumnRenderer")?
            .get("text")?,
    )
}

fn formatted_text(text: &Value) -> Option<String> {
    if let Some(simple_text) = text.get("simpleText").and_then(Value::as_str) {
        return normalize_text(simple_text);
    }

    let joined = text
        .get("runs")?
        .as_array()?
        .iter()
        .filter_map(|run| run.get("text").and_then(Value::as_str))
        .collect::<String>();
    normalize_text(&joined)
}

fn normalize_text(text: &str) -> Option<String> {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    (!normalized.is_empty()).then_some(normalized)
}

fn non_empty_string(value: Option<&Value>) -> Option<String> {
    normalize_text(value?.as_str()?)
}

fn metadata_runs(renderer: &Value) -> Result<Vec<&Value>, ParseWarningKind> {
    let columns = renderer
        .get("flexColumns")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    if columns.len().saturating_sub(1) > MAX_METADATA_RUNS {
        return Err(ParseWarningKind::ResourceLimit {
            resource: ParseResource::MetadataRuns,
        });
    }

    let mut runs = Vec::new();
    for column in columns.iter().skip(1) {
        let Some(column_runs) = column
            .get("musicResponsiveListItemFlexColumnRenderer")
            .and_then(|renderer| renderer.get("text"))
            .and_then(|text| text.get("runs"))
            .and_then(Value::as_array)
        else {
            continue;
        };
        let Some(total_runs) = runs.len().checked_add(column_runs.len()) else {
            return Err(ParseWarningKind::ResourceLimit {
                resource: ParseResource::MetadataRuns,
            });
        };
        if total_runs > MAX_METADATA_RUNS {
            return Err(ParseWarningKind::ResourceLimit {
                resource: ParseResource::MetadataRuns,
            });
        }
        runs.extend(column_runs);
    }
    Ok(runs)
}

fn page_type(run: &Value) -> Option<&str> {
    run.get("navigationEndpoint")?
        .get("browseEndpoint")?
        .get("browseEndpointContextSupportedConfigs")?
        .get("browseEndpointContextMusicConfig")?
        .get("pageType")?
        .as_str()
}

fn creators(metadata_runs: &[&Value]) -> Vec<String> {
    let mut creators = Vec::new();
    for run in metadata_runs {
        if !matches!(
            page_type(run),
            Some("MUSIC_PAGE_TYPE_ARTIST" | "MUSIC_PAGE_TYPE_PODCAST_SHOW_DETAIL_PAGE")
        ) {
            continue;
        }
        let Some(creator) = non_empty_string(run.get("text")) else {
            continue;
        };
        if !creators.contains(&creator) {
            creators.push(creator);
        }
    }
    creators
}

fn collection(metadata_runs: &[&Value]) -> Option<String> {
    metadata_runs
        .iter()
        .copied()
        .find(|run| page_type(run) == Some("MUSIC_PAGE_TYPE_ALBUM"))
        .and_then(|run| non_empty_string(run.get("text")))
}

fn duration(renderer: &Value) -> (Option<u64>, Option<ParseWarningKind>) {
    let duration_text = renderer
        .get("fixedColumns")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|column| {
            column
                .get("musicResponsiveListItemFixedColumnRenderer")
                .and_then(|fixed| fixed.get("text"))
                .and_then(formatted_text)
        })
        .find(|text| text.contains(':'));

    let Some(duration_text) = duration_text else {
        return (None, None);
    };
    match parse_duration_ms(&duration_text) {
        Some(duration_ms) => (Some(duration_ms), None),
        None => (None, Some(ParseWarningKind::InvalidDuration)),
    }
}

fn parse_duration_ms(value: &str) -> Option<u64> {
    let parts = value
        .split(':')
        .map(str::parse::<u64>)
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    let seconds = match parts.as_slice() {
        [minutes, seconds] if *seconds < 60 => minutes.checked_mul(60)?.checked_add(*seconds)?,
        [hours, minutes, seconds] if *minutes < 60 && *seconds < 60 => hours
            .checked_mul(60)?
            .checked_add(*minutes)?
            .checked_mul(60)?
            .checked_add(*seconds)?,
        _ => return None,
    };
    seconds.checked_mul(1_000)
}

fn artwork(renderer: &Value) -> (Option<Url>, Option<ParseWarningKind>) {
    let Some(thumbnails) = renderer
        .get("thumbnail")
        .and_then(|thumbnail| thumbnail.get("musicThumbnailRenderer"))
        .and_then(|renderer| renderer.get("thumbnail"))
        .and_then(|thumbnail| thumbnail.get("thumbnails"))
        .and_then(Value::as_array)
    else {
        return (None, None);
    };
    if thumbnails.len() > MAX_THUMBNAILS {
        return (
            None,
            Some(ParseWarningKind::ResourceLimit {
                resource: ParseResource::Thumbnails,
            }),
        );
    }

    let mut saw_invalid_url = false;
    let best = thumbnails
        .iter()
        .filter_map(|thumbnail| {
            let raw_url = thumbnail.get("url").and_then(Value::as_str)?;
            let Ok(url) = Url::parse(raw_url) else {
                saw_invalid_url = true;
                return None;
            };
            if !matches!(url.scheme(), "http" | "https") {
                saw_invalid_url = true;
                return None;
            }
            let width = thumbnail.get("width").and_then(Value::as_u64).unwrap_or(0);
            let height = thumbnail.get("height").and_then(Value::as_u64).unwrap_or(0);
            Some((width.saturating_mul(height), url))
        })
        .max_by_key(|(area, _)| *area)
        .map(|(_, url)| url);

    let warning = saw_invalid_url.then_some(ParseWarningKind::InvalidArtworkUrl);
    (best, warning)
}

fn is_explicit(renderer: &Value) -> bool {
    renderer
        .get("badges")
        .and_then(Value::as_array)
        .is_some_and(|badges| {
            badges.iter().any(|badge| {
                badge
                    .get("musicInlineBadgeRenderer")
                    .and_then(|renderer| renderer.get("icon"))
                    .and_then(|icon| icon.get("iconType"))
                    .and_then(Value::as_str)
                    == Some("MUSIC_EXPLICIT_BADGE")
            })
        })
}
