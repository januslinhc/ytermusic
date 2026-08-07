use std::{error::Error, io};

use serde_json::{Value, json};
use url::Url;
use ytermusic::{
    app,
    domain::{self, MediaId, MediaItem, MediaKind, RegionCode},
    provider::{
        self, BrowseItem, ChartSection, LibraryItem, MAX_ITEMS_PER_SHELF, MAX_METADATA_RUNS,
        MAX_RESPONSE_BYTES, MAX_SECTIONS, MAX_THUMBNAILS, MAX_VIDEO_ID_BYTES, MAX_WARNINGS, Page,
        ParseError, ParseReport, ParseResource, ParseWarningKind, Podcast, SearchItem,
        parse_chart_response, parse_search_response,
    },
};

const ATV: &str = "MUSIC_VIDEO_TYPE_ATV";
const UGC: &str = "MUSIC_VIDEO_TYPE_UGC";
const OMV: &str = "MUSIC_VIDEO_TYPE_OMV";
const PODCAST_EPISODE: &str = "MUSIC_VIDEO_TYPE_PODCAST_EPISODE";

fn encode(value: &Value) -> Result<Vec<u8>, serde_json::Error> {
    serde_json::to_vec(value)
}

fn chart_sections(sections: &[Value]) -> Value {
    json!({
        "contents": {
            "singleColumnBrowseResultsRenderer": {
                "tabs": [{
                    "tabRenderer": {
                        "content": {
                            "sectionListRenderer": {
                                "contents": sections
                            }
                        }
                    }
                }]
            }
        }
    })
}

fn chart_response(items: &[Value]) -> Value {
    chart_sections(&[json!({
        "musicCarouselShelfRenderer": {
            "header": {
                "musicCarouselShelfBasicHeaderRenderer": {
                    "title": {"runs": [{"text": "Fixture chart"}]}
                }
            },
            "contents": items
        }
    })])
}

fn search_response(items: &[Value]) -> Value {
    json!({
        "contents": {
            "tabbedSearchResultsRenderer": {
                "tabs": [{
                    "tabRenderer": {
                        "content": {
                            "sectionListRenderer": {
                                "contents": [{
                                    "musicShelfRenderer": {
                                        "title": {"runs": [{"text": "Fixture results"}]},
                                        "contents": items
                                    }
                                }]
                            }
                        }
                    }
                }]
            }
        }
    })
}

fn responsive_item(
    playlist_video_id: Option<&str>,
    endpoint_video_id: Option<&str>,
    media_type: Option<&str>,
) -> Value {
    let playlist_data = playlist_video_id.map(|video_id| json!({"videoId": video_id}));
    let supported_configs = media_type.map(|media_type| {
        json!({
            "watchEndpointMusicConfig": {
                "musicVideoType": media_type
            }
        })
    });

    json!({
        "musicResponsiveListItemRenderer": {
            "playlistItemData": playlist_data,
            "flexColumns": [
                {
                    "musicResponsiveListItemFlexColumnRenderer": {
                        "text": {"runs": [{"text": "Fixture title"}]}
                    }
                },
                {
                    "musicResponsiveListItemFlexColumnRenderer": {
                        "text": {
                            "runs": [{
                                "text": "Fixture artist",
                                "navigationEndpoint": {
                                    "browseEndpoint": {
                                        "browseId": "UC_FIXTURE",
                                        "browseEndpointContextSupportedConfigs": {
                                            "browseEndpointContextMusicConfig": {
                                                "pageType": "MUSIC_PAGE_TYPE_ARTIST"
                                            }
                                        }
                                    }
                                }
                            }]
                        }
                    }
                }
            ],
            "thumbnail": {
                "musicThumbnailRenderer": {
                    "thumbnail": {
                        "thumbnails": [{
                            "url": "https://fixtures.invalid/art.jpg",
                            "width": 100,
                            "height": 100
                        }]
                    }
                }
            },
            "overlay": {
                "musicItemThumbnailOverlayRenderer": {
                    "content": {
                        "musicPlayButtonRenderer": {
                            "playNavigationEndpoint": {
                                "watchEndpoint": {
                                    "videoId": endpoint_video_id,
                                    "watchEndpointMusicSupportedConfigs": supported_configs
                                }
                            }
                        }
                    }
                }
            }
        }
    })
}

fn replace_array(
    value: &mut Value,
    pointer: &str,
    replacement: Vec<Value>,
) -> Result<(), io::Error> {
    let target = value
        .pointer_mut(pointer)
        .ok_or_else(|| io::Error::other("fixture pointer should exist"))?;
    *target = Value::Array(replacement);
    Ok(())
}

fn signed_artwork() -> Result<Url, url::ParseError> {
    Url::parse(
        "https://signed-user:signed-password@media.invalid/art.jpg\
         ?signature=SIGNED_QUERY_SENTINEL#SIGNED_FRAGMENT_SENTINEL",
    )
}

fn media_with_signed_artwork() -> Result<MediaItem, url::ParseError> {
    Ok(MediaItem {
        id: MediaId {
            provider: "youtube-music".to_owned(),
            video_id: "debug-fixture".to_owned(),
        },
        kind: MediaKind::Song,
        title: "Debug fixture".to_owned(),
        creators: vec!["Fixture creator".to_owned()],
        collection: None,
        duration_ms: None,
        artwork_url: Some(signed_artwork()?),
        explicit: false,
    })
}

fn assert_debug_is_secret_safe(rendered: &str) {
    for secret in [
        "CONTINUATION_TOKEN_SENTINEL",
        "signed-user",
        "signed-password",
        "SIGNED_QUERY_SENTINEL",
        "SIGNED_FRAGMENT_SENTINEL",
    ] {
        assert!(
            !rendered.contains(secret),
            "Debug output exposed sentinel {secret}: {rendered}"
        );
    }
}

#[test]
fn normalized_success_models_have_summary_only_secret_safe_debug() -> Result<(), Box<dyn Error>> {
    let media = media_with_signed_artwork()?;
    let browse = BrowseItem {
        id: "MPSP_FIXTURE".to_owned(),
        title: "Fixture browse item".to_owned(),
        subtitle: None,
        artwork_url: Some(signed_artwork()?),
    };
    let search_item = SearchItem::Playable(media.clone());
    let page = Page {
        items: vec![search_item.clone()],
        continuation: Some("CONTINUATION_TOKEN_SENTINEL".to_owned()),
        stale: false,
    };
    let podcast = Podcast {
        id: "MPSP_FIXTURE".to_owned(),
        title: "Fixture podcast".to_owned(),
        creators: vec!["Fixture creator".to_owned()],
        description: None,
        artwork_url: Some(signed_artwork()?),
        episodes: vec![media.clone()],
    };
    let library_item = LibraryItem::Podcast(browse.clone());
    let chart = ChartSection::new("Fixture chart", vec![media.clone()]);
    let report = ParseReport {
        value: page.clone(),
        warnings: Vec::new(),
    };

    for rendered in [
        format!("{media:?}"),
        format!("{:?}", vec![media]),
        format!("{browse:?}"),
        format!("{search_item:?}"),
        format!("{page:?}"),
        format!("{podcast:?}"),
        format!("{library_item:?}"),
        format!("{chart:?}"),
        format!("{report:?}"),
    ] {
        assert_debug_is_secret_safe(&rendered);
    }

    let generic_page = Page {
        items: vec!["GENERIC_PAGE_PAYLOAD_SENTINEL"],
        continuation: Some("CONTINUATION_TOKEN_SENTINEL".to_owned()),
        stale: true,
    };
    let generic_report = ParseReport {
        value: "GENERIC_REPORT_PAYLOAD_SENTINEL",
        warnings: Vec::new(),
    };
    let page_debug = format!("{generic_page:?}");
    let report_debug = format!("{generic_report:?}");
    assert!(!page_debug.contains("GENERIC_PAGE_PAYLOAD_SENTINEL"));
    assert!(!report_debug.contains("GENERIC_REPORT_PAYLOAD_SENTINEL"));
    assert!(page_debug.contains("continuation_present: true"));
    assert!(page_debug.contains("item_count: 1"));
    assert!(report_debug.contains("warning_count: 0"));
    Ok(())
}

#[test]
fn provider_and_app_share_domain_catalog_types_without_adapters() {
    let app_filter: app::SearchFilter = provider::SearchFilter::Episodes;
    let domain_filter: domain::SearchFilter = app_filter;
    assert_eq!(domain_filter, domain::SearchFilter::Episodes);

    let app_section = app::ChartSection::new("Shared chart", Vec::new());
    let domain_section: domain::ChartSection = app_section;
    let provider_section: provider::ChartSection = domain_section;
    assert_eq!(provider_section.title(), "Shared chart");
    assert!(provider_section.items().is_empty());
}

#[test]
fn parser_rejects_oversized_input_before_deserialization() {
    let oversized = vec![b' '; MAX_RESPONSE_BYTES + 1];
    assert_eq!(
        parse_chart_response(&oversized),
        Err(ParseError::InputTooLarge { response: "charts" })
    );
    assert_eq!(
        parse_search_response(&oversized),
        Err(ParseError::InputTooLarge { response: "search" })
    );
}

#[test]
fn parser_rejects_wide_section_item_and_warning_arrays() -> Result<(), Box<dyn Error>> {
    let sections = vec![json!({}); MAX_SECTIONS + 1];
    assert_eq!(
        parse_chart_response(&encode(&chart_sections(&sections))?),
        Err(ParseError::ResourceLimit {
            response: "charts",
            resource: ParseResource::Sections,
        })
    );

    let items = vec![json!({}); MAX_ITEMS_PER_SHELF + 1];
    assert_eq!(
        parse_chart_response(&encode(&chart_response(&items))?),
        Err(ParseError::ResourceLimit {
            response: "charts",
            resource: ParseResource::Items,
        })
    );
    assert_eq!(
        parse_search_response(&encode(&search_response(&items))?),
        Err(ParseError::ResourceLimit {
            response: "search",
            resource: ParseResource::Items,
        })
    );

    let warning_items = vec![json!({}); MAX_WARNINGS + 1];
    assert!(
        warning_items.len() <= MAX_ITEMS_PER_SHELF,
        "warning fixture must remain inside the item bound"
    );
    assert_eq!(
        parse_chart_response(&encode(&chart_response(&warning_items))?),
        Err(ParseError::ResourceLimit {
            response: "charts",
            resource: ParseResource::Warnings,
        })
    );
    Ok(())
}

#[test]
fn metadata_and_thumbnail_arrays_are_bounded_per_item() -> Result<(), Box<dyn Error>> {
    let mut wide_metadata =
        responsive_item(Some("wide-metadata"), Some("wide-metadata"), Some(ATV));
    replace_array(
        &mut wide_metadata,
        "/musicResponsiveListItemRenderer/flexColumns/1/musicResponsiveListItemFlexColumnRenderer/text/runs",
        vec![json!({"text": "noise"}); MAX_METADATA_RUNS + 1],
    )?;
    let valid = responsive_item(Some("valid-sibling"), Some("valid-sibling"), Some(ATV));
    let metadata_report = parse_chart_response(&encode(&chart_response(&[wide_metadata, valid]))?)?;
    assert_eq!(metadata_report.value[0].items.len(), 1);
    assert_eq!(
        metadata_report.warnings[0].kind,
        ParseWarningKind::ResourceLimit {
            resource: ParseResource::MetadataRuns,
        }
    );

    let mut wide_thumbnails =
        responsive_item(Some("wide-thumbnails"), Some("wide-thumbnails"), Some(ATV));
    replace_array(
        &mut wide_thumbnails,
        "/musicResponsiveListItemRenderer/thumbnail/musicThumbnailRenderer/thumbnail/thumbnails",
        vec![
            json!({
                "url": "https://fixtures.invalid/art.jpg",
                "width": 100,
                "height": 100
            });
            MAX_THUMBNAILS + 1
        ],
    )?;
    let thumbnail_report = parse_chart_response(&encode(&chart_response(&[wide_thumbnails]))?)?;
    assert_eq!(thumbnail_report.value[0].items.len(), 1);
    assert_eq!(thumbnail_report.value[0].items[0].artwork_url, None);
    assert_eq!(
        thumbnail_report.warnings[0].kind,
        ParseWarningKind::ResourceLimit {
            resource: ParseResource::Thumbnails,
        }
    );
    Ok(())
}

#[test]
fn recognized_empty_partial_and_all_invalid_shelves_are_distinct() -> Result<(), Box<dyn Error>> {
    let empty_chart = parse_chart_response(&encode(&chart_response(&[]))?)?;
    let empty_search = parse_search_response(&encode(&search_response(&[]))?)?;
    assert!(empty_chart.value[0].items.is_empty());
    assert!(empty_search.value.items.is_empty());

    let unknown = responsive_item(
        Some("unknown-kind"),
        Some("unknown-kind"),
        Some("MUSIC_VIDEO_TYPE_FUTURE"),
    );
    assert_eq!(
        parse_chart_response(&encode(&chart_response(std::slice::from_ref(&unknown)))?),
        Err(ParseError::UnusableResponse { response: "charts" })
    );
    assert_eq!(
        parse_search_response(&encode(&search_response(std::slice::from_ref(&unknown)))?),
        Err(ParseError::UnusableResponse { response: "search" })
    );

    let valid = responsive_item(Some("valid-kind"), Some("valid-kind"), Some(ATV));
    let partial = parse_search_response(&encode(&search_response(&[unknown, valid.clone()]))?)?;
    assert_eq!(partial.value.items.len(), 1);
    assert_eq!(
        partial.warnings[0].kind,
        ParseWarningKind::UnsupportedMediaKind
    );

    let missing_kind = responsive_item(Some("missing-kind"), Some("missing-kind"), None);
    let missing_partial = parse_chart_response(&encode(&chart_response(&[missing_kind, valid]))?)?;
    assert_eq!(missing_partial.value[0].items.len(), 1);
    assert_eq!(
        missing_partial.warnings[0].kind,
        ParseWarningKind::UnsupportedMediaKind
    );
    Ok(())
}

#[test]
fn only_positive_media_types_are_mapped_and_endpoint_video_id_is_a_fallback()
-> Result<(), Box<dyn Error>> {
    let report = parse_chart_response(&encode(&chart_response(&[
        responsive_item(None, Some("endpoint-fallback"), Some(ATV)),
        responsive_item(Some("ugc-id"), Some("ugc-id"), Some(UGC)),
        responsive_item(Some("omv-id"), Some("omv-id"), Some(OMV)),
        responsive_item(
            Some("podcast-id"),
            Some("podcast-id"),
            Some(PODCAST_EPISODE),
        ),
    ]))?)?;
    let items = &report.value[0].items;
    assert_eq!(items[0].id.video_id, "endpoint-fallback");
    assert_eq!(items[0].kind, MediaKind::Song);
    assert_eq!(items[1].kind, MediaKind::Video);
    assert_eq!(items[2].kind, MediaKind::Video);
    assert_eq!(items[3].kind, MediaKind::PodcastEpisode);
    Ok(())
}

#[test]
fn opaque_video_ids_are_bounded_and_never_trimmed_or_collapsed() -> Result<(), Box<dyn Error>> {
    let exact_id = "opaque_%2F+id";
    let overlong_id = "x".repeat(MAX_VIDEO_ID_BYTES + 1);
    let report = parse_chart_response(&encode(&chart_response(&[
        responsive_item(Some(exact_id), Some(exact_id), Some(ATV)),
        responsive_item(Some(" opaque_%2F+id "), Some(exact_id), Some(ATV)),
        responsive_item(Some("opaque id"), Some("opaque id"), Some(ATV)),
        responsive_item(Some("opaque\tid"), Some("opaque\tid"), Some(ATV)),
        responsive_item(Some(""), Some(""), Some(ATV)),
        responsive_item(Some(&overlong_id), Some(&overlong_id), Some(ATV)),
    ]))?)?;

    assert_eq!(report.value[0].items.len(), 1);
    assert_eq!(report.value[0].items[0].id.video_id, exact_id);
    assert_eq!(report.warnings.len(), 5);
    assert!(
        report
            .warnings
            .iter()
            .all(|warning| warning.kind == ParseWarningKind::InvalidVideoId)
    );
    Ok(())
}

#[test]
fn matching_redundant_video_ids_are_accepted() -> Result<(), Box<dyn Error>> {
    let report = parse_chart_response(&encode(&chart_response(&[responsive_item(
        Some("same-video-id"),
        Some("same-video-id"),
        Some(ATV),
    )]))?)?;

    assert_eq!(report.value[0].items.len(), 1);
    assert_eq!(report.value[0].items[0].id.video_id, "same-video-id");
    assert!(report.warnings.is_empty());
    Ok(())
}

#[test]
fn conflicting_video_ids_are_skipped_with_a_structured_warning() -> Result<(), Box<dyn Error>> {
    let conflicting = responsive_item(
        Some("playlist-video-id"),
        Some("endpoint-video-id"),
        Some(ATV),
    );
    let valid = responsive_item(Some("valid-video-id"), Some("valid-video-id"), Some(ATV));
    let report = parse_search_response(&encode(&search_response(&[conflicting, valid]))?)?;

    assert_eq!(report.value.items.len(), 1);
    let Some(SearchItem::Playable(item)) = report.value.items.first() else {
        return Err(io::Error::other("valid sibling should remain playable").into());
    };
    assert_eq!(item.id.video_id, "valid-video-id");
    assert_eq!(report.warnings.len(), 1);
    assert_eq!(
        report.warnings[0].kind,
        ParseWarningKind::ConflictingVideoId
    );
    Ok(())
}

#[test]
fn chart_cache_keys_still_require_validated_region_codes() -> Result<(), Box<dyn Error>> {
    let key = provider::ChartCacheKey::new(RegionCode::parse("hk")?);
    assert_eq!(key.to_string(), "charts:HK");
    Ok(())
}
