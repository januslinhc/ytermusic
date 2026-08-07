use std::error::Error;

use tempfile::TempDir;
use ytermusic::{
    app::{
        Action, AppError, AppErrorCategory, AppState, ChartCachePayload, Effect, Generation,
        MAX_CHART_CACHE_BYTES, reduce,
    },
    domain::{ChartSection, MediaId, MediaItem, MediaKind, RegionCode},
    provider::ChartCacheKey,
    storage::{SqliteStorage, Storage},
};

fn region(value: &str) -> RegionCode {
    RegionCode::parse(value).unwrap_or_else(|error| panic!("valid test region: {error}"))
}

fn song(video_id: &str, title: &str) -> MediaItem {
    MediaItem {
        id: MediaId {
            provider: "youtube-music".to_owned(),
            video_id: video_id.to_owned(),
        },
        kind: MediaKind::Song,
        title: title.to_owned(),
        creators: vec!["Cache Artist".to_owned()],
        collection: None,
        duration_ms: Some(180_000),
        artwork_url: None,
        explicit: false,
    }
}

fn section(video_id: &str, title: &str) -> Vec<ChartSection> {
    vec![ChartSection::new("Top songs", vec![song(video_id, title)])]
}

fn request(region: RegionCode) -> (AppState, Generation) {
    let (state, effects) = reduce(
        AppState::default(),
        Action::ChartsRequested {
            region: region.clone(),
        },
    );
    let [
        Effect::ReadChartCache {
            generation,
            region: cache_region,
            key,
        },
        Effect::LoadCharts {
            generation: live_generation,
            region: live_region,
        },
    ] = effects.as_slice()
    else {
        panic!("chart request must start cache and live effects");
    };
    assert_eq!(cache_region, &region);
    assert_eq!(live_region, &region);
    assert_eq!(key, &ChartCacheKey::new(region));
    assert_eq!(generation, live_generation);
    (state, *generation)
}

fn payload(
    region: RegionCode,
    video_id: &str,
    title: &str,
    stored_at: i64,
    expires_at: i64,
) -> ChartCachePayload {
    ChartCachePayload::try_new(region, section(video_id, title), stored_at, expires_at)
        .unwrap_or_else(|error| panic!("valid cache payload: {}", error.message()))
}

#[test]
fn chart_cache_payload_rejects_an_oversized_encoded_document() {
    let oversized_title = "x".repeat(MAX_CHART_CACHE_BYTES);
    let Err(error) = ChartCachePayload::try_new(
        region("US"),
        section("oversized", &oversized_title),
        100,
        200,
    ) else {
        panic!("oversized cache payload must be rejected");
    };
    assert_eq!(error.category(), AppErrorCategory::Charts);
    assert!(error.message().contains("encoded limit"));
}

#[test]
fn chart_cache_deserialization_enforces_constructor_invariants() {
    let invalid = r#"{"region":"US","sections":[],"stored_at":200,"expires_at":100}"#;

    assert!(
        serde_json::from_str::<ChartCachePayload>(invalid).is_err(),
        "deserialization must reject provenance that construction rejects"
    );
}

fn live_success(
    state: AppState,
    generation: Generation,
    region: RegionCode,
    title: &str,
) -> (AppState, Vec<Effect>) {
    reduce(
        state,
        Action::ChartsCompleted {
            generation,
            region,
            received_at: 2_000,
            result: Ok(section("live", title)),
        },
    )
}

#[test]
fn fresh_live_charts_win_in_either_completion_order_and_store_once() {
    let us = region("US");
    let cached = payload(us.clone(), "cached", "Cached chart", 500, 1_000);

    let (cache_first, generation) = request(us.clone());
    let (cache_first, effects) = reduce(
        cache_first,
        Action::CachedChartsCompleted {
            generation,
            region: us.clone(),
            observed_at: 1_500,
            result: Ok(Some(cached.clone())),
        },
    );
    assert!(effects.is_empty());
    assert!(cache_first.charts().loading());
    assert!(cache_first.charts().sections().is_empty());

    let (cache_first, effects) = live_success(cache_first, generation, us.clone(), "Fresh chart");
    let [Effect::StoreChartCache { key, payload }] = effects.as_slice() else {
        panic!("accepted live charts must emit one bounded cache store");
    };
    assert_eq!(key, &ChartCacheKey::new(us.clone()));
    assert_eq!(payload.region(), &us);
    assert_eq!(payload.stored_at(), 2_000);
    assert_eq!(
        cache_first.charts().sections()[0].items()[0].title,
        "Fresh chart"
    );
    assert!(!cache_first.charts().stale());

    let (live_first, generation) = request(us.clone());
    let (live_first, effects) = live_success(live_first, generation, us.clone(), "Fresh chart");
    assert!(matches!(
        effects.as_slice(),
        [Effect::StoreChartCache { .. }]
    ));
    let expected = live_first.clone();
    let (live_first, late_effects) = reduce(
        live_first,
        Action::CachedChartsCompleted {
            generation,
            region: us,
            observed_at: 1_500,
            result: Ok(Some(cached)),
        },
    );
    assert_eq!(live_first, expected);
    assert!(late_effects.is_empty());
}

#[test]
fn live_error_uses_matching_cache_and_miss_uses_the_live_error_in_either_order() {
    let gb = region("GB");
    let offline = AppError::new(AppErrorCategory::Charts, "network unavailable");
    let cached = payload(gb.clone(), "cached", "Offline chart", 500, 1_000);

    let (state, generation) = request(gb.clone());
    let (state, effects) = reduce(
        state,
        Action::ChartsCompleted {
            generation,
            region: gb.clone(),
            received_at: 1_500,
            result: Err(offline.clone()),
        },
    );
    assert!(effects.is_empty());
    assert!(state.charts().loading());
    let (state, effects) = reduce(
        state,
        Action::CachedChartsCompleted {
            generation,
            region: gb.clone(),
            observed_at: 1_500,
            result: Ok(Some(cached)),
        },
    );
    assert!(effects.is_empty());
    assert!(!state.charts().loading());
    assert!(state.charts().stale());
    assert_eq!(state.charts().cached_at(), Some(500));
    assert_eq!(
        state.charts().sections()[0].items()[0].title,
        "Offline chart"
    );
    assert!(state.charts().error().is_none());

    let (state, generation) = request(gb.clone());
    let (state, effects) = reduce(
        state,
        Action::CachedChartsCompleted {
            generation,
            region: gb.clone(),
            observed_at: 1_500,
            result: Ok(None),
        },
    );
    assert!(effects.is_empty());
    assert!(state.charts().loading());
    let (state, effects) = reduce(
        state,
        Action::ChartsCompleted {
            generation,
            region: gb,
            received_at: 1_500,
            result: Err(offline.clone()),
        },
    );
    assert!(effects.is_empty());
    assert_eq!(state.charts().error(), Some(&offline));
    assert!(!state.charts().loading());
}

#[test]
fn cache_error_and_live_error_resolve_to_the_live_error_in_both_orders() {
    let region = region("CA");
    let live_error = AppError::new(AppErrorCategory::Charts, "live failed");
    let cache_error = AppError::new(AppErrorCategory::Charts, "cache failed");

    let (state, generation) = request(region.clone());
    let (state, _) = reduce(
        state,
        Action::CachedChartsCompleted {
            generation,
            region: region.clone(),
            observed_at: 1_000,
            result: Err(cache_error.clone()),
        },
    );
    let (state, _) = reduce(
        state,
        Action::ChartsCompleted {
            generation,
            region: region.clone(),
            received_at: 1_000,
            result: Err(live_error.clone()),
        },
    );
    assert_eq!(state.charts().error(), Some(&live_error));

    let (state, generation) = request(region.clone());
    let (state, _) = reduce(
        state,
        Action::ChartsCompleted {
            generation,
            region: region.clone(),
            received_at: 1_000,
            result: Err(live_error.clone()),
        },
    );
    let (state, _) = reduce(
        state,
        Action::CachedChartsCompleted {
            generation,
            region,
            observed_at: 1_000,
            result: Err(cache_error),
        },
    );
    assert_eq!(state.charts().error(), Some(&live_error));
}

#[test]
fn chart_completions_require_both_the_active_generation_and_region() {
    let hk = region("HK");
    let us = region("US");
    let (state, generation) = request(hk.clone());
    let expected = state.clone();

    let (state, effects) = reduce(
        state,
        Action::ChartsCompleted {
            generation,
            region: us.clone(),
            received_at: 1_000,
            result: Ok(section("wrong-live", "Wrong live region")),
        },
    );
    assert_eq!(state, expected);
    assert!(effects.is_empty());

    let (state, effects) = reduce(
        state,
        Action::CachedChartsCompleted {
            generation,
            region: us.clone(),
            observed_at: 1_000,
            result: Ok(Some(payload(
                us,
                "wrong-cache",
                "Wrong cache region",
                100,
                900,
            ))),
        },
    );
    assert_eq!(state, expected);
    assert!(effects.is_empty());

    let (state, effects) = live_success(state, generation, hk, "Correct region");
    assert!(matches!(
        effects.as_slice(),
        [Effect::StoreChartCache { .. }]
    ));
    assert_eq!(
        state.charts().sections()[0].items()[0].title,
        "Correct region"
    );
}

#[test]
fn expired_sqlite_entry_is_consumed_by_the_read_effect_as_a_stale_fallback()
-> Result<(), Box<dyn Error>> {
    let directory = TempDir::new()?;
    let mut storage = SqliteStorage::open(directory.path().join("cache.db"))?;
    let us = region("US");
    let (state, generation) = request(us.clone());
    let (state, effects) = reduce(
        state,
        Action::ChartsCompleted {
            generation,
            region: us.clone(),
            received_at: 100,
            result: Ok(section("sqlite", "SQLite offline chart")),
        },
    );
    let [Effect::StoreChartCache { key, payload }] = effects.as_slice() else {
        panic!("live completion must produce the storage write");
    };
    let encoded = payload
        .encoded()
        .unwrap_or_else(|error| panic!("cache encoding: {}", error.message()));
    storage.put_metadata(
        &key.to_string(),
        &encoded,
        payload.expires_at(),
        payload.stored_at(),
    )?;
    drop(state);

    let (state, effects) = reduce(
        AppState::default(),
        Action::ChartsRequested { region: us.clone() },
    );
    let Effect::ReadChartCache {
        generation,
        region,
        key,
    } = &effects[0]
    else {
        panic!("first chart effect must read cache");
    };
    let cached_result = match storage.get_metadata_entry(&key.to_string())? {
        Some(entry) => Ok(Some(
            ChartCachePayload::from_metadata_entry(region, &entry)
                .unwrap_or_else(|error| panic!("cache decoding: {}", error.message())),
        )),
        None => Ok(None),
    };
    let (state, _) = reduce(
        state,
        Action::CachedChartsCompleted {
            generation: *generation,
            region: region.clone(),
            observed_at: 4_000,
            result: cached_result,
        },
    );
    let (state, _) = reduce(
        state,
        Action::ChartsCompleted {
            generation: *generation,
            region: region.clone(),
            received_at: 4_000,
            result: Err(AppError::new(AppErrorCategory::Charts, "offline")),
        },
    );
    assert!(state.charts().stale());
    assert_eq!(
        state.charts().sections()[0].items()[0].title,
        "SQLite offline chart"
    );
    Ok(())
}
