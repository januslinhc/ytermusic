use std::{borrow::Cow, fmt, io};

use serde_json::{Map, Value, json};
use ytmapi_rs::{
    auth::AuthToken,
    error::ErrorKind,
    parse::{ParseFrom, ProcessedResult},
    query::{PostMethod, PostQuery, Query},
};

use crate::{
    domain::{ChartSection, RegionCode},
    provider::{MAX_RESPONSE_BYTES, ParseError, parse_chart_response},
};

use super::charts::{ChartPlaylistReference, parse_chart_playlist_references};

#[derive(Clone, Eq, PartialEq)]
pub struct ChartsQuery {
    region: RegionCode,
}

impl ChartsQuery {
    #[must_use]
    pub const fn new(region: RegionCode) -> Self {
        Self { region }
    }

    #[must_use]
    pub const fn region(&self) -> &RegionCode {
        &self.region
    }
}

impl fmt::Debug for ChartsQuery {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ChartsQuery")
            .field("region", &self.region)
            .finish_non_exhaustive()
    }
}

#[doc(hidden)]
pub struct ChartsQueryOutput {
    sections: Vec<ChartSection>,
    playlist_references: Vec<ChartPlaylistReference>,
}

impl ParseFrom<ChartsQuery> for ChartsQueryOutput {
    fn parse_from(processed: ProcessedResult<'_, ChartsQuery>) -> ytmapi_rs::Result<Self> {
        let value = processed.json.into_inner();
        normalize_processed_chart_value(&value)
    }
}

impl ChartsQueryOutput {
    #[cfg(test)]
    pub(crate) fn from_sections(sections: Vec<ChartSection>) -> Self {
        Self {
            sections,
            playlist_references: Vec::new(),
        }
    }

    pub(crate) fn from_playlist_references(
        playlist_references: Vec<ChartPlaylistReference>,
    ) -> Self {
        Self {
            sections: Vec::new(),
            playlist_references,
        }
    }

    pub(crate) fn into_parts(self) -> (Vec<ChartSection>, Vec<ChartPlaylistReference>) {
        (self.sections, self.playlist_references)
    }
}

impl fmt::Debug for ChartsQueryOutput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ChartsQueryOutput")
            .field("section_count", &self.sections.len())
            .field("playlist_reference_count", &self.playlist_references.len())
            .finish_non_exhaustive()
    }
}

impl<A: AuthToken> Query<A> for ChartsQuery {
    type Output = ChartsQueryOutput;
    type Method = PostMethod;
}

impl PostQuery for ChartsQuery {
    fn header(&self) -> Map<String, Value> {
        Map::from_iter([
            ("browseId".to_owned(), json!("FEmusic_charts")),
            (
                "formData".to_owned(),
                json!({"selectedValues": [self.region.as_str()]}),
            ),
        ])
    }

    fn params(&self) -> Vec<(&str, Cow<'_, str>)> {
        Vec::new()
    }

    fn path(&self) -> &'static str {
        "browse"
    }
}

fn normalize_processed_chart_value(value: &Value) -> ytmapi_rs::Result<ChartsQueryOutput> {
    // ytmapi-rs 0.3.2 buffers the transport body before exposing processed
    // JSON. Task 11's one-mebibyte parser limit still bounds accepted chart
    // input here, but cannot retroactively cap that upstream transport buffer.
    let encoded = encode_processed_chart_value(value)?;
    match parse_chart_response(&encoded) {
        Ok(report) => Ok(ChartsQueryOutput {
            sections: report.value,
            playlist_references: Vec::new(),
        }),
        Err(ParseError::UnusableResponse { .. }) => {
            let references = parse_chart_playlist_references(&encoded).map_err(|_| {
                ErrorKind::InvalidResponse {
                    response: "processed chart response was rejected".to_owned(),
                }
            })?;
            Ok(ChartsQueryOutput::from_playlist_references(references))
        }
        Err(_) => Err(ErrorKind::InvalidResponse {
            response: "processed chart response was rejected".to_owned(),
        }
        .into()),
    }
}

fn encode_processed_chart_value(value: &Value) -> ytmapi_rs::Result<Vec<u8>> {
    let mut writer = CappedWriter::new(MAX_RESPONSE_BYTES + 1);
    let serialization = serde_json::to_writer(&mut writer, value);
    if writer.bytes.len() <= MAX_RESPONSE_BYTES {
        serialization
            .map_err(|_| io::Error::other("processed chart response could not be encoded"))?;
    }
    Ok(writer.bytes)
}

struct CappedWriter {
    bytes: Vec<u8>,
    limit: usize,
}

impl CappedWriter {
    const fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
        }
    }
}

impl io::Write for CappedWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let writable = self.limit.saturating_sub(self.bytes.len());
        let written = writable.min(buffer.len());
        self.bytes.extend_from_slice(&buffer[..written]);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use crate::provider::MAX_RESPONSE_BYTES;

    use super::{encode_processed_chart_value, normalize_processed_chart_value};

    const CHARTS_FIXTURE: &[u8] = include_bytes!("../../tests/fixtures/charts_hk.json");
    const JP_PLAYLIST_CHARTS_FIXTURE: &[u8] =
        include_bytes!("../../tests/fixtures/charts_playlist_carousel_jp.json");

    #[test]
    fn processed_chart_value_is_bounded_and_normalized_without_raw_debug()
    -> Result<(), Box<dyn std::error::Error>> {
        let value: Value = serde_json::from_slice(CHARTS_FIXTURE)?;

        let output = normalize_processed_chart_value(&value)?;
        let rendered = format!("{output:?}");
        assert!(rendered.contains("section_count"));
        assert!(!rendered.contains("hk_fixture_01"));
        assert!(!rendered.contains("https://"));

        let (sections, playlist_references) = output.into_parts();
        assert!(playlist_references.is_empty());
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].title(), "Top songs");
        assert_eq!(sections[0].items()[0].id.video_id, "hk_fixture_01");
        Ok(())
    }

    #[test]
    fn processed_jp_chart_playlist_is_recognized() -> Result<(), Box<dyn std::error::Error>> {
        let value: Value = serde_json::from_slice(JP_PLAYLIST_CHARTS_FIXTURE)?;

        let output = normalize_processed_chart_value(&value)?;
        let (sections, playlist_references) = output.into_parts();

        assert!(sections.is_empty());
        assert_eq!(playlist_references.len(), 1);
        assert_eq!(playlist_references[0].title(), "Trending 20 Japan");
        assert_eq!(playlist_references[0].playlist_id(), "JP_CHART_FIXTURE");
        Ok(())
    }

    #[test]
    fn processed_chart_playlist_recognition_is_country_agnostic()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut value: Value = serde_json::from_slice(JP_PLAYLIST_CHARTS_FIXTURE)?;
        value["contents"]["singleColumnBrowseResultsRenderer"]["tabs"][0]["tabRenderer"]["content"]
            ["sectionListRenderer"]["contents"][2]["musicCarouselShelfRenderer"]["contents"][0]["musicTwoRowItemRenderer"]
            ["title"]["runs"][0]["text"] = Value::String("Trending 20 United States".to_owned());
        value["contents"]["singleColumnBrowseResultsRenderer"]["tabs"][0]["tabRenderer"]["content"]
            ["sectionListRenderer"]["contents"][2]["musicCarouselShelfRenderer"]["contents"][0]["musicTwoRowItemRenderer"]
            ["title"]["runs"][0]["navigationEndpoint"]["browseEndpoint"]["browseId"] =
            Value::String("VLUS_CHART_FIXTURE".to_owned());

        let (_, playlist_references) = normalize_processed_chart_value(&value)?.into_parts();

        assert_eq!(playlist_references[0].title(), "Trending 20 United States");
        assert_eq!(playlist_references[0].playlist_id(), "US_CHART_FIXTURE");
        Ok(())
    }

    #[test]
    fn rejected_processed_chart_value_never_renders_payload() {
        let sentinel = "RAW_CHART_PAYLOAD_SENTINEL";
        let value = serde_json::json!({"unrelated": sentinel});

        let Err(error) = normalize_processed_chart_value(&value) else {
            panic!("unrelated processed response must fail");
        };
        let rendered = format!("{error:?} {error}");
        assert!(!rendered.contains(sentinel));
    }

    #[test]
    fn processed_chart_encoding_stops_at_one_byte_over_the_parser_limit()
    -> Result<(), Box<dyn std::error::Error>> {
        let value = serde_json::json!({"oversized": "x".repeat(MAX_RESPONSE_BYTES * 2)});

        let encoded = encode_processed_chart_value(&value)?;

        assert_eq!(encoded.len(), MAX_RESPONSE_BYTES + 1);
        Ok(())
    }
}
