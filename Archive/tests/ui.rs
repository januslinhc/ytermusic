use std::{
    collections::HashSet,
    error::Error,
    io::Cursor,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use async_trait::async_trait;
use bytes::Bytes;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MediaKeyCode};
use futures::stream;
use image::{DynamicImage, ImageFormat, Rgba, RgbaImage};
use ratatui::{
    Terminal,
    backend::TestBackend,
    buffer::{Buffer, Cell, CellWidth},
    layout::{Alignment, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget},
};
use unicode_segmentation::UnicodeSegmentation;
use url::Url;
use ytermusic::{
    app::{
        Action, AppError, AppErrorCategory, AppState, Effect, FadeActivity, ResolverQuality,
        SearchFilter, SearchItem, SearchPage, reduce,
    },
    config::{Config, PlaybackConfig, PodcastConfig},
    domain::{ChartSection, MediaId, MediaItem, MediaKind, PlaybackStatus, RegionCode, RepeatMode},
    podcast_rankings::parse_apple_top_shows,
    provider::{AuthenticationState, LibraryItem, LibrarySection, Page, Podcast},
    storage::{FavoriteEntry, HistoryEntry},
    ui::{
        artwork::{
            ArtworkByteStream, ArtworkCache, ArtworkCell, ArtworkFetchError, ArtworkFetcher,
            ArtworkIdentity, ArtworkPresentation, ArtworkWidget, CachedArtworkService, CellSize,
            Rgb, decode_artwork,
        },
        input::{
            InputAction, InputMode, SemanticAction, TextEntryContext, map_event, palette_entries,
        },
        interaction::{
            FrameRevision, HitTarget, InteractionMap, InteractionSnapshot, InteractionStore,
            ListSurface, MAX_INTERACTION_REGIONS,
        },
        layout::LayoutMode,
        motion::{MotionFrame, ProgressPresentation},
        render::{
            CLIP_BYTE_INSPECTION_BUDGET, CLIP_GRAPHEME_INSPECTION_BUDGET, CompactPanel,
            FocusRegion, NavigationItem, Overlay, RenderModel, ViewportMemory, clip_line, render,
            render_artwork, render_with_model, render_with_model_and_artwork,
            render_with_model_and_interactions, render_with_model_and_motion_memory,
            render_with_model_and_motion_memory_and_interactions, render_with_model_and_spectrum,
            truncate_cells,
        },
        spectrum::SpectrumPresentation,
        theme::{ColorCapability, TerminalColorSnapshot, Theme, detect_color_capability},
    },
};

#[test]
fn favorites_is_positioned_after_library_in_top_level_navigation() {
    assert_eq!(
        NavigationItem::ALL
            .into_iter()
            .map(NavigationItem::label)
            .collect::<Vec<_>>(),
        vec![
            "Home",
            "Search",
            "Charts",
            "Podcasts",
            "Library",
            "Favorites",
            "History",
            "Settings",
        ]
    );
}

#[test]
fn terminal_color_detection_maps_no_color_terminal_and_declared_depths() {
    let snapshot = |output_is_terminal, no_color, term: Option<&str>, colorterm: Option<&str>| {
        TerminalColorSnapshot::new(output_is_terminal, no_color, term, colorterm)
    };

    assert_eq!(
        detect_color_capability(&snapshot(false, false, Some("xterm-256color"), None)),
        ColorCapability::Monochrome
    );
    assert_eq!(
        detect_color_capability(&snapshot(
            true,
            true,
            Some("xterm-256color"),
            Some("truecolor")
        )),
        ColorCapability::Monochrome
    );
    assert_eq!(
        detect_color_capability(&snapshot(true, false, Some("dumb"), None)),
        ColorCapability::Monochrome
    );
    assert_eq!(
        detect_color_capability(&snapshot(true, false, Some("xterm"), None)),
        ColorCapability::Basic
    );
    assert_eq!(
        detect_color_capability(&snapshot(true, false, Some("screen-256color"), None)),
        ColorCapability::Ansi256
    );
    assert_eq!(
        detect_color_capability(&snapshot(
            true,
            false,
            Some("xterm-256color"),
            Some("24BIT")
        )),
        ColorCapability::TrueColor
    );
    assert_eq!(
        detect_color_capability(&snapshot(true, false, Some("xterm-direct"), None)),
        ColorCapability::TrueColor
    );
}

fn resolved_cells(
    map: &InteractionMap,
    width: u16,
    height: u16,
    target: HitTarget,
) -> Vec<(u16, u16)> {
    (0..height)
        .flat_map(|row| (0..width).map(move |column| (column, row)))
        .filter(|(column, row)| map.resolve(*column, *row, map.revision()) == Some(target))
        .collect()
}

fn assert_exact_wide_mouse_geometry(
    map: &InteractionMap,
    progress: &[(u16, u16, u16, u16)],
) -> Result<(), Box<dyn Error>> {
    for (index, item) in NavigationItem::ALL.iter().enumerate() {
        let row = 1_u16.saturating_add(u16::try_from(index)?);
        let expected = (3..3_u16.saturating_add(item.label().cell_width()))
            .map(|column| (column, row))
            .collect::<Vec<_>>();
        assert_eq!(
            resolved_cells(map, 140, 40, HitTarget::Navigation(*item)),
            expected,
            "{item:?}"
        );
    }
    let labels = [
        (SemanticAction::PreviousTrack, "[p Previous]"),
        (SemanticAction::SeekBackward, "[⇧← −10s]"),
        (SemanticAction::TogglePlayback, "[Space Pause]"),
        (SemanticAction::SeekForward, "[⇧→ +10s]"),
        (SemanticAction::NextTrack, "[n Next]"),
    ];
    let mut column = 1_u16;
    for (action, label) in labels {
        let end = column.saturating_add(label.cell_width());
        assert_eq!(
            resolved_cells(map, 140, 40, HitTarget::Semantic(action)),
            (column..end).map(|column| (column, 38)).collect::<Vec<_>>(),
            "{action:?}"
        );
        column = end.saturating_add(1);
    }
    let first_progress_column = column;
    assert_eq!(
        progress,
        (0_u16..20)
            .map(|numerator| {
                (
                    first_progress_column.saturating_add(numerator),
                    38,
                    numerator,
                    19,
                )
            })
            .collect::<Vec<_>>()
    );
    Ok(())
}

fn assert_exact_compact_mouse_geometry(map: &InteractionMap, progress: &[(u16, u16, u16, u16)]) {
    let mut navigation_column = 1_u16;
    for item in NavigationItem::ALL {
        let marker_width = 2;
        let start = navigation_column.saturating_add(marker_width);
        let end = start.saturating_add(item.compact_label().cell_width());
        assert_eq!(
            resolved_cells(map, 90, 30, HitTarget::Navigation(item)),
            (start..end).map(|column| (column, 1)).collect::<Vec<_>>(),
            "{item:?}"
        );
        navigation_column = end.saturating_add(3);
    }
    assert_exact_mouse_controls(map, 90, 30, 28, 1, progress);
}

fn assert_exact_roomy_tiny_mouse_geometry(map: &InteractionMap) {
    let labels = [
        (SemanticAction::PreviousTrack, "[p]"),
        (SemanticAction::SeekBackward, "[←]"),
        (SemanticAction::TogglePlayback, "[Spc]"),
        (SemanticAction::SeekForward, "[→]"),
        (SemanticAction::NextTrack, "[n]"),
    ];
    let mut column = 38_u16;
    for (action, label) in labels {
        let end = column.saturating_add(label.cell_width());
        assert_eq!(
            resolved_cells(map, 59, 17, HitTarget::Semantic(action)),
            (column..end).map(|column| (column, 16)).collect::<Vec<_>>(),
            "{action:?}"
        );
        column = end.saturating_add(1);
    }
    assert_eq!(column, 60, "the last separator is clipped beyond the frame");
}

fn assert_exact_mouse_controls(
    map: &InteractionMap,
    width: u16,
    height: u16,
    row: u16,
    start: u16,
    progress: &[(u16, u16, u16, u16)],
) {
    let labels = [
        (SemanticAction::PreviousTrack, "[p Previous]"),
        (SemanticAction::SeekBackward, "[⇧← −10s]"),
        (SemanticAction::TogglePlayback, "[Space Pause]"),
        (SemanticAction::SeekForward, "[⇧→ +10s]"),
        (SemanticAction::NextTrack, "[n Next]"),
    ];
    let mut column = start;
    for (action, label) in labels {
        let end = column.saturating_add(label.cell_width());
        assert_eq!(
            resolved_cells(map, width, height, HitTarget::Semantic(action)),
            (column..end)
                .map(|column| (column, row))
                .collect::<Vec<_>>(),
            "{action:?}"
        );
        column = end.saturating_add(1);
    }
    let first_progress_column = column;
    assert_eq!(
        progress,
        (0_u16..20)
            .map(|numerator| {
                (
                    first_progress_column.saturating_add(numerator),
                    row,
                    numerator,
                    19,
                )
            })
            .collect::<Vec<_>>()
    );
}

#[test]
fn mouse_rendered_interactions_cover_exact_visible_navigation_controls_and_known_progress()
-> Result<(), Box<dyn Error>> {
    let state = playback_state(
        MediaKind::Song,
        PlaybackStatus::Playing,
        50_000,
        Some(100_000),
        Config::default(),
    );
    for (width, height) in [(140, 40), (90, 30), (59, 17), (40, 12)] {
        let mut map = InteractionMap::new(FrameRevision::new(1));
        let mut terminal = Terminal::new(TestBackend::new(width, height))?;
        terminal.draw(|frame| {
            render_with_model_and_interactions(
                frame,
                &state,
                &Theme::default(),
                &RenderModel::default(),
                &mut map,
            );
        })?;

        if width >= 60 {
            for item in NavigationItem::ALL {
                assert!(
                    !resolved_cells(&map, width, height, HitTarget::Navigation(item)).is_empty()
                );
            }
        }
        for action in [
            SemanticAction::PreviousTrack,
            SemanticAction::SeekBackward,
            SemanticAction::TogglePlayback,
            SemanticAction::SeekForward,
            SemanticAction::NextTrack,
        ] {
            let visible = width >= 60
                || terminal.backend().to_string().contains(match action {
                    SemanticAction::PreviousTrack => "[p]",
                    SemanticAction::SeekBackward => "[←]",
                    SemanticAction::TogglePlayback => "[Spc]",
                    SemanticAction::SeekForward => "[→]",
                    SemanticAction::NextTrack => "[n]",
                    _ => unreachable!(),
                });
            assert_eq!(
                !resolved_cells(&map, width, height, HitTarget::Semantic(action)).is_empty(),
                visible,
                "{width}x{height}: {action:?}"
            );
        }
        let progress = (0..height)
            .flat_map(|row| (0..width).map(move |column| (column, row)))
            .filter_map(
                |(column, row)| match map.resolve(column, row, map.revision()) {
                    Some(HitTarget::Progress {
                        numerator,
                        denominator,
                    }) => Some((column, row, numerator, denominator)),
                    _ => None,
                },
            )
            .collect::<Vec<_>>();
        if width == 140 {
            assert_exact_wide_mouse_geometry(&map, &progress)?;
        } else if width == 90 {
            assert_exact_compact_mouse_geometry(&map, &progress);
        } else if width == 59 {
            assert_exact_roomy_tiny_mouse_geometry(&map);
            assert!(progress.is_empty());
        }
        if width >= 60 {
            assert!(!progress.is_empty());
            assert_eq!(progress.first().map(|cell| cell.2), Some(0));
            assert_eq!(
                progress.last().map(|cell| cell.2),
                progress.last().map(|cell| cell.3)
            );
        } else {
            assert!(progress.is_empty());
        }
    }

    Ok(())
}

#[test]
fn mouse_rendered_interactions_exclude_disabled_unknown_and_modal_backgrounds()
-> Result<(), Box<dyn Error>> {
    for duration_ms in [None, Some(0)] {
        let state = playback_state(
            MediaKind::Song,
            PlaybackStatus::Playing,
            0,
            duration_ms,
            Config::default(),
        );
        let mut map = InteractionMap::new(FrameRevision::new(2));
        let mut terminal = Terminal::new(TestBackend::new(140, 40))?;
        terminal.draw(|frame| {
            render_with_model_and_interactions(
                frame,
                &state,
                &Theme::default(),
                &RenderModel::default(),
                &mut map,
            );
        })?;
        assert!((0..40).all(|row| (0..140).all(|column| !matches!(
            map.resolve(column, row, map.revision()),
            Some(HitTarget::Progress { .. })
        ))));
    }
    for status in [PlaybackStatus::Resolving, PlaybackStatus::Buffering] {
        let state = playback_state(
            MediaKind::Song,
            status,
            10_000,
            Some(100_000),
            Config::default(),
        );
        let mut map = InteractionMap::new(FrameRevision::new(3));
        let mut terminal = Terminal::new(TestBackend::new(140, 40))?;
        terminal.draw(|frame| {
            render_with_model_and_interactions(
                frame,
                &state,
                &Theme::default(),
                &RenderModel::default(),
                &mut map,
            );
        })?;
        assert!(
            resolved_cells(
                &map,
                140,
                40,
                HitTarget::Semantic(SemanticAction::TogglePlayback)
            )
            .is_empty()
        );
    }

    let state = playback_state(
        MediaKind::Song,
        PlaybackStatus::Playing,
        10_000,
        Some(100_000),
        Config::default(),
    );
    for overlay in [Overlay::Help, Overlay::Lyrics] {
        let mut model = RenderModel::default();
        model.overlay = Some(overlay);
        let mut map = InteractionMap::new(FrameRevision::new(4));
        let mut terminal = Terminal::new(TestBackend::new(140, 40))?;
        terminal.draw(|frame| {
            render_with_model_and_interactions(frame, &state, &Theme::default(), &model, &mut map);
        })?;
        assert!(map.is_empty(), "{overlay:?}");
    }
    for overlay in [
        Overlay::CommandPalette,
        Overlay::CountryPicker,
        Overlay::BrowserPicker,
    ] {
        let mut model = RenderModel::default();
        model.overlay = Some(overlay);
        let mut map = InteractionMap::new(FrameRevision::new(5));
        let mut terminal = Terminal::new(TestBackend::new(140, 40))?;
        terminal.draw(|frame| {
            render_with_model_and_interactions(frame, &state, &Theme::default(), &model, &mut map);
        })?;
        assert!(!map.is_empty(), "{overlay:?}");
        assert!((0..40).all(|row| (0..140).all(|column| matches!(
            map.resolve(column, row, map.revision()),
            None | Some(HitTarget::ListRow { .. })
        ))));
    }
    Ok(())
}

#[test]
fn interaction_map_is_bounded_and_rejects_zero_area_regions() {
    let revision = FrameRevision::new(7);
    let mut map = InteractionMap::new(revision);
    assert!(!map.push(
        Rect::new(1, 1, 0, 2),
        HitTarget::Semantic(SemanticAction::TogglePlayback),
    ));
    assert!(!map.push(
        Rect::new(1, 1, 2, 0),
        HitTarget::Semantic(SemanticAction::TogglePlayback),
    ));
    assert!(!map.push(
        Rect::new(1, 1, 2, 1),
        HitTarget::Progress {
            numerator: 0,
            denominator: 0,
        },
    ));
    for index in 0..MAX_INTERACTION_REGIONS {
        assert!(map.push(
            Rect::new(0, 0, 1, 1),
            HitTarget::ListRow {
                surface: ListSurface::Search,
                stable_index: index,
            },
        ));
    }
    assert!(!map.push(
        Rect::new(0, 0, 1, 1),
        HitTarget::Semantic(SemanticAction::Submit),
    ));
    assert_eq!(map.len(), MAX_INTERACTION_REGIONS);
}

#[test]
fn interaction_map_rejects_fully_clipped_regions_and_retains_only_visible_cells() {
    let revision = FrameRevision::new(8);
    let mut map = InteractionMap::new(revision);
    let target = HitTarget::Semantic(SemanticAction::Submit);
    assert!(!map.push_clipped(Rect::new(20, 20, 2, 2), Rect::new(0, 0, 10, 10), target,));
    assert!(map.push_clipped(Rect::new(8, 8, 4, 4), Rect::new(0, 0, 10, 10), target,));
    assert_eq!(map.resolve(9, 9, revision), Some(target));
    assert_eq!(map.resolve(10, 9, revision), None);
}

#[test]
fn interaction_map_resolves_topmost_last_with_half_open_boundaries() {
    let revision = FrameRevision::new(7);
    let mut map = InteractionMap::new(revision);
    assert!(map.push(
        Rect::new(2, 3, 4, 2),
        HitTarget::Semantic(SemanticAction::TogglePlayback),
    ));
    assert!(map.push(
        Rect::new(3, 3, 2, 1),
        HitTarget::Semantic(SemanticAction::NextTrack),
    ));

    assert_eq!(
        map.resolve(2, 3, revision),
        Some(HitTarget::Semantic(SemanticAction::TogglePlayback))
    );
    assert_eq!(
        map.resolve(3, 3, revision),
        Some(HitTarget::Semantic(SemanticAction::NextTrack))
    );
    assert_eq!(map.resolve(6, 3, revision), None);
    assert_eq!(map.resolve(2, 5, revision), None);
    assert_eq!(map.resolve(2, 3, FrameRevision::new(6)), None);
}

#[test]
fn interaction_map_store_replaces_latest_frame_and_rejects_stale_publication() {
    let mut store = InteractionStore::default();
    let Some(mut first) = store.begin_frame() else {
        panic!("first revision should be available");
    };
    let first_revision = first.revision();
    assert!(first.push(
        Rect::new(0, 0, 1, 1),
        HitTarget::Navigation(NavigationItem::Home),
    ));
    assert!(store.publish(first));
    let Some(retained_first) = store.latest().cloned() else {
        panic!("published snapshot should be available");
    };
    assert_eq!(
        store.latest().and_then(|snapshot| snapshot.resolve(0, 0)),
        Some(HitTarget::Navigation(NavigationItem::Home))
    );

    let mut stale = InteractionMap::new(first_revision);
    assert!(stale.push(
        Rect::new(0, 0, 1, 1),
        HitTarget::Navigation(NavigationItem::Search),
    ));
    let Some(second) = store.begin_frame() else {
        panic!("second revision should be available");
    };
    assert!(
        store.latest().is_none(),
        "an in-progress frame must invalidate stale geometry"
    );
    assert_eq!(
        retained_first.resolve(0, 0),
        None,
        "an externally retained snapshot must fail closed after invalidation"
    );
    assert!(!store.publish(stale));
    assert!(store.publish(second));
    assert_eq!(
        store.latest().map(InteractionSnapshot::revision),
        first_revision.next()
    );
    assert_eq!(
        store.latest().and_then(|snapshot| snapshot.resolve(0, 0)),
        None
    );
    assert_eq!(
        retained_first.resolve(0, 0),
        None,
        "publishing a replacement must not revalidate an older snapshot"
    );
}

#[test]
fn interaction_map_revision_exhaustion_never_wraps_or_aliases() {
    assert_eq!(FrameRevision::new(u64::MAX).next(), None);
}

#[test]
fn interaction_map_snapshot_fails_closed_after_its_store_is_dropped() {
    let mut store = InteractionStore::default();
    let Some(mut map) = store.begin_frame() else {
        panic!("frame revision should be available");
    };
    assert!(map.push(
        Rect::new(0, 0, 1, 1),
        HitTarget::Semantic(SemanticAction::Submit),
    ));
    assert!(store.publish(map));
    let Some(snapshot) = store.latest().cloned() else {
        panic!("published snapshot should be available");
    };
    assert_eq!(
        snapshot.resolve(0, 0),
        Some(HitTarget::Semantic(SemanticAction::Submit))
    );

    drop(store);
    assert_eq!(snapshot.resolve(0, 0), None);
}

#[test]
fn interaction_map_debug_is_summary_only_and_redacts_target_payloads() {
    let mut map = InteractionMap::new(FrameRevision::new(91));
    assert!(map.push(
        Rect::new(1, 2, 3, 4),
        HitTarget::ListRow {
            surface: ListSurface::Search,
            stable_index: 777_777,
        },
    ));
    assert!(map.push(
        Rect::new(2, 3, 1, 1),
        HitTarget::Semantic(SemanticAction::TogglePlayback),
    ));

    let debug = format!("{map:?}");
    assert!(debug.contains("region_count"));
    assert!(debug.contains("semantic_count"));
    assert!(debug.contains("list_row_count"));
    assert!(!debug.contains("777777"));
    assert!(!debug.contains("TogglePlayback"));
    assert!(!debug.contains("Search"));
}

#[test]
fn mouse_surface_matrix_registers_only_visible_search_rows_with_absolute_indices()
-> Result<(), Box<dyn Error>> {
    let items = (0..12)
        .map(|index| {
            SearchItem::Playable(MediaItem {
                id: MediaId {
                    provider: "fixture-provider".to_owned(),
                    video_id: format!("mouse-row-{index}"),
                },
                kind: MediaKind::Song,
                title: format!("Mouse row {index}"),
                creators: vec!["Fixture creator".to_owned()],
                collection: None,
                duration_ms: Some(1_000),
                artwork_url: None,
                explicit: false,
            })
        })
        .collect::<Vec<_>>();
    let (state, _) = reduce(
        AppState::default(),
        Action::SearchSubmitted {
            query: "mouse".to_owned(),
            filter: SearchFilter::Songs,
        },
    );
    let generation = state.search().generation();
    let (state, _) = reduce(
        state,
        Action::SearchCompleted {
            generation,
            result: Ok(SearchPage::new(items.clone())),
        },
    );
    let (state, _) = reduce(
        state,
        Action::SearchSelectionChanged {
            id: items[10].stable_id(),
        },
    );
    let model = RenderModel::default().with_view(NavigationItem::Search);
    let mut map = InteractionMap::new(FrameRevision::new(1));
    let mut terminal = Terminal::new(TestBackend::new(40, 12))?;
    terminal.draw(|frame| {
        render_with_model_and_interactions(frame, &state, &Theme::default(), &model, &mut map);
    })?;

    let visible = (0..12)
        .filter(|index| {
            !resolved_cells(
                &map,
                40,
                12,
                HitTarget::ListRow {
                    surface: ListSurface::Search,
                    stable_index: *index,
                },
            )
            .is_empty()
        })
        .collect::<Vec<_>>();
    assert_eq!(visible, vec![4, 5, 6, 7, 8, 9, 10]);
    assert_eq!(map.resolve(1, 1, map.revision()), None, "query header");
    assert_eq!(map.resolve(1, 2, map.revision()), None, "blank status row");
    Ok(())
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one deterministic trace compares presentation, settled state, and every hit cell"
)]
fn selection_motion_search_keeps_logical_selection_distinct_from_gliding_cursor()
-> Result<(), Box<dyn Error>> {
    let items = (0..3)
        .map(|index| {
            SearchItem::Playable(MediaItem {
                id: MediaId {
                    provider: "fixture-provider".to_owned(),
                    video_id: format!("selection-motion-{index}"),
                },
                kind: MediaKind::Song,
                title: format!("Selection motion row {index}"),
                creators: vec!["Fixture creator".to_owned()],
                collection: None,
                duration_ms: Some(1_000),
                artwork_url: None,
                explicit: false,
            })
        })
        .collect::<Vec<_>>();
    let (state, _) = reduce(
        AppState::default(),
        Action::SearchSubmitted {
            query: "motion".to_owned(),
            filter: SearchFilter::Songs,
        },
    );
    let generation = state.search().generation();
    let (state, _) = reduce(
        state,
        Action::SearchCompleted {
            generation,
            result: Ok(SearchPage::new(items.clone())),
        },
    );
    let mut memory = ViewportMemory::default();
    let mut terminal = Terminal::new(TestBackend::new(80, 24))?;
    let mut first_map = InteractionMap::new(FrameRevision::new(41));
    terminal.draw(|frame| {
        render_with_model_and_motion_memory_and_interactions(
            frame,
            &state,
            &Theme::default(),
            &RenderModel::default()
                .with_view(NavigationItem::Search)
                .with_motion_frame(MotionFrame {
                    elapsed_ms: 0,
                    ..MotionFrame::default()
                }),
            &mut memory,
            &mut first_map,
        );
    })?;

    let (state, _) = reduce(
        state,
        Action::SearchSelectionChanged {
            id: items[2].stable_id(),
        },
    );
    terminal.draw(|frame| {
        render_with_model_and_motion_memory(
            frame,
            &state,
            &Theme::default(),
            &RenderModel::default()
                .with_view(NavigationItem::Search)
                .with_motion_frame(MotionFrame {
                    elapsed_ms: 1,
                    ..MotionFrame::default()
                }),
            &mut memory,
        );
    })?;
    let rendered = terminal.backend().to_string();
    assert!(
        rendered.contains("▶ Selection motion row 0"),
        "cursor should begin at the previous visual row:\n{rendered}"
    );
    assert!(
        rendered.contains("● Selection motion row 2"),
        "logical row should remain visibly selected:\n{rendered}"
    );

    let mut intermediate_map = InteractionMap::new(FrameRevision::new(42));
    terminal.draw(|frame| {
        render_with_model_and_motion_memory_and_interactions(
            frame,
            &state,
            &Theme::default(),
            &RenderModel::default()
                .with_view(NavigationItem::Search)
                .with_motion_frame(MotionFrame {
                    elapsed_ms: 30,
                    ..MotionFrame::default()
                }),
            &mut memory,
            &mut intermediate_map,
        );
    })?;
    for row in 0..24 {
        for column in 0..80 {
            assert_eq!(
                first_map.resolve(column, row, FrameRevision::new(41)),
                intermediate_map.resolve(column, row, FrameRevision::new(42)),
                "hit geometry changed at ({column}, {row})"
            );
        }
    }

    terminal.draw(|frame| {
        render_with_model_and_motion_memory(
            frame,
            &state,
            &Theme::default(),
            &RenderModel::default()
                .with_view(NavigationItem::Search)
                .with_motion_frame(MotionFrame {
                    elapsed_ms: 200,
                    ..MotionFrame::default()
                }),
            &mut memory,
        );
    })?;
    let settled = terminal.backend().to_string();
    assert!(settled.contains("▶ Selection motion row 2"), "{settled}");
    assert!(!settled.contains("● Selection motion row 2"), "{settled}");

    let (tiny_initial, _) = reduce(
        state,
        Action::SearchSelectionChanged {
            id: items[0].stable_id(),
        },
    );
    let mut tiny_memory = ViewportMemory::default();
    let mut tiny = Terminal::new(TestBackend::new(40, 12))?;
    tiny.draw(|frame| {
        render_with_model_and_motion_memory(
            frame,
            &tiny_initial,
            &Theme::default(),
            &RenderModel::default()
                .with_view(NavigationItem::Search)
                .with_motion_frame(MotionFrame::default()),
            &mut tiny_memory,
        );
    })?;
    let (tiny_moved, _) = reduce(
        tiny_initial,
        Action::SearchSelectionChanged {
            id: items[2].stable_id(),
        },
    );
    tiny.draw(|frame| {
        render_with_model_and_motion_memory(
            frame,
            &tiny_moved,
            &Theme::default(),
            &RenderModel::default()
                .with_view(NavigationItem::Search)
                .with_motion_frame(MotionFrame {
                    elapsed_ms: 1,
                    ..MotionFrame::default()
                }),
            &mut tiny_memory,
        );
    })?;
    let tiny_moving = tiny.backend().to_string();
    assert!(
        tiny_moving.contains("▶ Selection motion row 0"),
        "{tiny_moving}"
    );
    assert!(
        tiny_moving.contains("● Selection motion row 2"),
        "{tiny_moving}"
    );

    Ok(())
}

#[test]
fn mouse_surface_matrix_modal_rows_replace_background_and_nonchoices_have_no_target()
-> Result<(), Box<dyn Error>> {
    for (overlay, surface, first_row) in [
        (Overlay::CountryPicker, ListSurface::CountryPicker, 0),
        (Overlay::BrowserPicker, ListSurface::BrowserPicker, 0),
        (Overlay::CommandPalette, ListSurface::CommandPalette, 0),
    ] {
        let model = RenderModel::default().with_overlay(overlay);
        let mut map = InteractionMap::new(FrameRevision::new(1));
        let mut terminal = Terminal::new(TestBackend::new(90, 30))?;
        terminal.draw(|frame| {
            render_with_model_and_interactions(
                frame,
                &AppState::default(),
                &Theme::default(),
                &model,
                &mut map,
            );
        })?;
        assert!(
            !resolved_cells(
                &map,
                90,
                30,
                HitTarget::ListRow {
                    surface,
                    stable_index: first_row,
                },
            )
            .is_empty(),
            "{overlay:?}"
        );
        assert!(
            resolved_cells(&map, 90, 30, HitTarget::Navigation(NavigationItem::Home)).is_empty(),
            "modal background leaked for {overlay:?}"
        );
    }

    for overlay in [Overlay::Help, Overlay::Lyrics] {
        let model = RenderModel::default().with_overlay(overlay);
        let mut map = InteractionMap::new(FrameRevision::new(2));
        let mut terminal = Terminal::new(TestBackend::new(90, 30))?;
        terminal.draw(|frame| {
            render_with_model_and_interactions(
                frame,
                &AppState::default(),
                &Theme::default(),
                &model,
                &mut map,
            );
        })?;
        assert!(
            map.is_empty(),
            "{overlay:?} must not expose text/background targets"
        );
    }
    Ok(())
}

#[test]
fn mouse_rendered_picker_matrix_uses_exact_choice_rows_and_palette_viewport_indices()
-> Result<(), Box<dyn Error>> {
    let cases = vec![
        (
            RenderModel::default().with_overlay(Overlay::CountryPicker),
            ListSurface::CountryPicker,
            13,
            0,
        ),
        (
            RenderModel::default().with_overlay(Overlay::BrowserPicker),
            ListSurface::BrowserPicker,
            7,
            0,
        ),
        (
            RenderModel::default()
                .with_overlay(Overlay::CommandPalette)
                .with_palette_selection(20),
            ListSurface::CommandPalette,
            20,
            0,
        ),
    ];
    for (model, surface, visible_index, clipped_index) in cases {
        let mut map = InteractionMap::new(FrameRevision::new(21));
        let mut terminal = Terminal::new(TestBackend::new(60, 18))?;
        terminal.draw(|frame| {
            render_with_model_and_interactions(
                frame,
                &AppState::default(),
                &Theme::default(),
                &model,
                &mut map,
            );
        })?;
        let visible = resolved_cells(
            &map,
            60,
            18,
            HitTarget::ListRow {
                surface,
                stable_index: visible_index,
            },
        );
        assert!(!visible.is_empty(), "{surface:?}");
        let row = visible[0].1;
        assert!(visible.iter().all(|(_, candidate)| *candidate == row));
        if surface == ListSurface::CommandPalette {
            assert!(
                resolved_cells(
                    &map,
                    60,
                    18,
                    HitTarget::ListRow {
                        surface,
                        stable_index: clipped_index,
                    },
                )
                .is_empty(),
                "offscreen palette row leaked"
            );
        }
        assert!(
            resolved_cells(&map, 60, 18, HitTarget::Navigation(NavigationItem::Home)).is_empty()
        );
        let rendered = terminal.backend().to_string();
        let title = match surface {
            ListSurface::CountryPicker => "Country picker",
            ListSurface::BrowserPicker => "Browser picker",
            ListSurface::CommandPalette => "Command palette",
            _ => unreachable!(),
        };
        let (title_x, title_y) = text_position(&rendered, title);
        assert_eq!(map.resolve(title_x, title_y, map.revision()), None);
    }
    Ok(())
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one rendered matrix proves row identity and clipping across every list surface"
)]
fn mouse_rendered_surface_matrix_maps_exact_rows_across_wide_compact_and_tiny()
-> Result<(), Box<dyn Error>> {
    let cases = vec![
        (
            chart_mouse_state(),
            RenderModel::default().with_view(NavigationItem::Charts),
            ListSurface::Charts,
            35,
            "Chart row 35",
            "Trending in US",
            (140, 40),
        ),
        (
            podcast_recommendation_mouse_state(),
            RenderModel::default().with_view(NavigationItem::Podcasts),
            ListSurface::PodcastRecommendations,
            18,
            "Podcast row 18",
            "Top podcasts in US",
            (60, 18),
        ),
        (
            podcast_episode_mouse_state(),
            RenderModel::default().with_view(NavigationItem::Podcasts),
            ListSurface::PodcastEpisodes,
            35,
            "Episode row 35",
            "Podcasts & episodes",
            (40, 12),
        ),
        (
            library_mouse_state(),
            RenderModel::default().with_view(NavigationItem::Library),
            ListSurface::Library,
            35,
            "Library row 35",
            "Library · Songs",
            (60, 18),
        ),
        (
            history_mouse_state(),
            RenderModel::default().with_view(NavigationItem::History),
            ListSurface::History,
            35,
            "History row 35",
            "Listening history",
            (40, 12),
        ),
        (
            favorites_mouse_state(),
            RenderModel::default().with_view(NavigationItem::Favorites),
            ListSurface::Favorites,
            35,
            "Favorite row 35",
            "Favorites",
            (40, 12),
        ),
        (
            queue_mouse_state(),
            RenderModel::default().with_focus(FocusRegion::Queue),
            ListSurface::Queue,
            35,
            "Queue row 35",
            "Queue · 40",
            (140, 40),
        ),
    ];

    for (state, model, surface, selected_index, selected_text, header_text, (width, height)) in
        cases
    {
        let mut map = InteractionMap::new(FrameRevision::new(20));
        let mut terminal = Terminal::new(TestBackend::new(width, height))?;
        terminal.draw(|frame| {
            render_with_model_and_interactions(frame, &state, &Theme::default(), &model, &mut map);
        })?;
        let rendered = terminal.backend().to_string();
        let expected = HitTarget::ListRow {
            surface,
            stable_index: selected_index,
        };
        let (selected_x, selected_y) =
            targeted_text_position(&rendered, selected_text, &map, expected);
        assert_eq!(
            map.resolve(selected_x, selected_y, map.revision()),
            Some(expected)
        );
        assert!(
            resolved_cells(
                &map,
                width,
                height,
                HitTarget::ListRow {
                    surface,
                    stable_index: 0,
                },
            )
            .is_empty(),
            "clipped first row leaked for {surface:?}"
        );
        let (header_x, header_y) = text_position(&rendered, header_text);
        assert_eq!(
            map.resolve(header_x, header_y, map.revision()),
            None,
            "header/status row mapped for {surface:?}"
        );
        for non_row in ["• Songs", "[m] Load more", "Mouse Show — Mouse host"] {
            if rendered.contains(non_row) {
                let (x, y) = text_position(&rendered, non_row);
                assert_eq!(
                    map.resolve(x, y, map.revision()),
                    None,
                    "sticky header/footer mapped for {surface:?}: {non_row}"
                );
            }
        }
        for index in 0..40 {
            let cells = resolved_cells(
                &map,
                width,
                height,
                HitTarget::ListRow {
                    surface,
                    stable_index: index,
                },
            );
            if let Some((_, row)) = cells.first().copied() {
                assert!(cells.iter().all(|(_, candidate)| *candidate == row));
            }
        }
    }
    Ok(())
}

#[test]
fn mouse_rendered_loading_error_and_empty_rows_never_become_list_targets()
-> Result<(), Box<dyn Error>> {
    let (loading, _) = reduce(
        AppState::default(),
        Action::SearchSubmitted {
            query: "loading".to_owned(),
            filter: SearchFilter::Songs,
        },
    );
    let generation = loading.search().generation();
    let error = reduce(
        loading.clone(),
        Action::SearchCompleted {
            generation,
            result: Err(AppError::new(
                AppErrorCategory::Search,
                "safe fixture failure",
            )),
        },
    )
    .0;
    for state in [AppState::default(), loading, error] {
        let mut map = InteractionMap::new(FrameRevision::new(22));
        let mut terminal = Terminal::new(TestBackend::new(60, 18))?;
        terminal.draw(|frame| {
            render_with_model_and_interactions(
                frame,
                &state,
                &Theme::default(),
                &RenderModel::default().with_view(NavigationItem::Search),
                &mut map,
            );
        })?;
        assert!((0..18).all(|row| (0..60).all(|column| !matches!(
            map.resolve(column, row, map.revision()),
            Some(HitTarget::ListRow {
                surface: ListSurface::Search,
                ..
            })
        ))));
    }
    Ok(())
}

fn text_position(rendered: &str, needle: &str) -> (u16, u16) {
    for (row, line) in rendered.lines().enumerate() {
        if let Some(column) = line.find(needle) {
            return (
                u16::try_from(column).unwrap_or(u16::MAX),
                u16::try_from(row).unwrap_or(u16::MAX),
            );
        }
    }
    panic!("missing rendered text: {needle}\n{rendered}");
}

fn targeted_text_position(
    rendered: &str,
    needle: &str,
    map: &InteractionMap,
    target: HitTarget,
) -> (u16, u16) {
    for (row, line) in rendered.lines().enumerate() {
        for (column, _) in line.match_indices(needle) {
            let position = (
                u16::try_from(column).unwrap_or(u16::MAX),
                u16::try_from(row).unwrap_or(u16::MAX),
            );
            if map.resolve(position.0, position.1, map.revision()) == Some(target) {
                return position;
            }
        }
    }
    panic!("missing targeted rendered text: {needle}\n{rendered}");
}

fn mouse_media(prefix: &str, index: usize, kind: MediaKind) -> MediaItem {
    MediaItem {
        id: MediaId {
            provider: "youtube-music".to_owned(),
            video_id: format!("{prefix}-{index}"),
        },
        kind,
        title: format!("{prefix} row {index}"),
        creators: vec!["Mouse fixture".to_owned()],
        collection: None,
        duration_ms: Some(1_000),
        artwork_url: None,
        explicit: false,
    }
}

fn test_region() -> RegionCode {
    RegionCode::parse("US").unwrap_or_else(|error| panic!("valid test region: {error}"))
}

fn chart_mouse_state() -> AppState {
    let region = test_region();
    let items = (0..40)
        .map(|index| mouse_media("Chart", index, MediaKind::Song))
        .collect::<Vec<_>>();
    let selected = items[35].id.clone();
    let (state, effects) = reduce(
        AppState::default(),
        Action::ChartsRequested {
            region: region.clone(),
        },
    );
    let [
        Effect::ReadChartCache { generation, .. },
        Effect::LoadCharts { .. },
    ] = effects.as_slice()
    else {
        panic!("chart fixture load");
    };
    let (state, _) = reduce(
        state,
        Action::ChartsCompleted {
            generation: *generation,
            region,
            received_at: 1,
            result: Ok(vec![ChartSection::new("Songs".to_owned(), items)]),
        },
    );
    reduce(state, Action::ChartSelectionChanged { media_id: selected }).0
}

fn podcast_recommendation_mouse_state() -> AppState {
    let region = test_region();
    let rows = (0..20)
        .map(|index| {
            serde_json::json!({
                "id": format!("podcast-{index}"),
                "name": format!("Podcast row {index}"),
                "artistName": "Mouse publisher",
            })
        })
        .collect::<Vec<_>>();
    let bytes = serde_json::to_vec(&serde_json::json!({
        "feed": { "country": "US", "results": rows }
    }))
    .unwrap_or_else(|error| panic!("podcast fixture encode: {error}"));
    let page = parse_apple_top_shows(&bytes)
        .unwrap_or_else(|error| panic!("podcast fixture parse: {error}"));
    let selected = page.items()[18].source_id().clone();
    let (state, effects) = reduce(
        AppState::default(),
        Action::PodcastRecommendationsRequested {
            region: region.clone(),
        },
    );
    let [Effect::LoadPodcastRecommendations { generation, .. }] = effects.as_slice() else {
        panic!("recommendation fixture load");
    };
    let (state, _) = reduce(
        state,
        Action::PodcastRecommendationsCompleted {
            generation: *generation,
            requested_region: region,
            result: Ok(page),
        },
    );
    reduce(
        state,
        Action::PodcastRecommendationSelectionChanged { id: selected },
    )
    .0
}

fn podcast_episode_mouse_state() -> AppState {
    let episodes = (0..40)
        .map(|index| mouse_media("Episode", index, MediaKind::PodcastEpisode))
        .collect::<Vec<_>>();
    let selected = episodes[35].id.clone();
    let metadata = ytermusic::app::SearchMetadata::new(
        ytermusic::app::SearchMetadataKind::Podcast,
        "Mouse Show",
    )
    .with_provider_id("mouse-show");
    let (state, effects) = reduce(
        AppState::default(),
        Action::SearchSubmitted {
            query: "show".to_owned(),
            filter: SearchFilter::Podcasts,
        },
    );
    let [Effect::Search { generation, .. }] = effects.as_slice() else {
        panic!("show fixture search");
    };
    let (state, _) = reduce(
        state,
        Action::SearchCompleted {
            generation: *generation,
            result: Ok(SearchPage::new(vec![SearchItem::Metadata(metadata)])),
        },
    );
    let (state, effects) = reduce(state, Action::OpenSelectedPodcast);
    let [Effect::LoadPodcast { generation, .. }] = effects.as_slice() else {
        panic!("show fixture load");
    };
    let (state, _) = reduce(
        state,
        Action::PodcastCompleted {
            generation: *generation,
            result: Ok(Podcast {
                id: "mouse-show".to_owned(),
                title: "Mouse Show".to_owned(),
                creators: vec!["Mouse host".to_owned()],
                description: None,
                artwork_url: None,
                episodes,
            }),
        },
    );
    reduce(
        state,
        Action::PodcastSelectionChanged { media_id: selected },
    )
    .0
}

fn library_mouse_state() -> AppState {
    let items = (0..40)
        .map(|index| LibraryItem::Playable(mouse_media("Library", index, MediaKind::Song)))
        .collect::<Vec<_>>();
    let selected = ytermusic::app::stable_library_item_id(&items[35]);
    let (state, _) = reduce(
        AppState::default(),
        Action::AuthenticationChanged(AuthenticationState::Authenticated),
    );
    let (state, effects) = reduce(
        state,
        Action::LibraryRequested {
            section: LibrarySection::Songs,
        },
    );
    let [Effect::LoadLibrary { generation, .. }] = effects.as_slice() else {
        panic!("library fixture load");
    };
    let (state, _) = reduce(
        state,
        Action::LibraryCompleted {
            generation: *generation,
            result: Ok(Page {
                items,
                continuation: Some("more".to_owned()),
                stale: false,
            }),
        },
    );
    reduce(state, Action::LibrarySelectionChanged { id: selected }).0
}

fn history_mouse_state() -> AppState {
    let entries = (0..40)
        .map(|index| HistoryEntry {
            id: i64::try_from(index).unwrap_or_default(),
            item: mouse_media("History", index, MediaKind::Song),
            played_at: i64::try_from(40usize.saturating_sub(index)).unwrap_or_default(),
        })
        .collect::<Vec<_>>();
    let (state, effects) = reduce(AppState::default(), Action::HistoryRequested);
    let [Effect::LoadHistory { generation, .. }] = effects.as_slice() else {
        panic!("history fixture load");
    };
    let (state, _) = reduce(
        state,
        Action::HistoryCompleted {
            generation: *generation,
            result: Ok(entries),
        },
    );
    reduce(state, Action::HistorySelectionChanged { id: 35 }).0
}

fn favorites_mouse_state() -> AppState {
    let entries = (0..40)
        .map(|index| FavoriteEntry {
            id: i64::try_from(index).unwrap_or_default(),
            item: mouse_media("Favorite", index, MediaKind::Song),
            favorited_at: i64::try_from(40usize.saturating_sub(index)).unwrap_or_default(),
        })
        .collect::<Vec<_>>();
    let selected = entries[35].item.id.clone();
    let (state, effects) = reduce(AppState::default(), Action::FavoritesRequested);
    let [Effect::LoadFavorites { generation }] = effects.as_slice() else {
        panic!("favorites fixture load");
    };
    let state = reduce(
        state,
        Action::FavoritesCompleted {
            generation: *generation,
            result: Ok(entries),
        },
    )
    .0;
    reduce(
        state,
        Action::FavoriteSelectionChanged { media_id: selected },
    )
    .0
}

fn complete_favorites(state: AppState, result: Result<Vec<FavoriteEntry>, AppError>) -> AppState {
    let (state, effects) = reduce(state, Action::FavoritesRequested);
    let [Effect::LoadFavorites { generation }] = effects.as_slice() else {
        panic!("favorites fixture load");
    };
    reduce(
        state,
        Action::FavoritesCompleted {
            generation: *generation,
            result,
        },
    )
    .0
}

#[test]
fn favorites_render_loading_empty_populated_and_retained_error_states() -> Result<(), Box<dyn Error>>
{
    let (loading, _) = reduce(AppState::default(), Action::FavoritesRequested);
    let empty = complete_favorites(AppState::default(), Ok(Vec::new()));
    let populated = favorites_mouse_state();
    let error = complete_favorites(
        populated.clone(),
        Err(AppError::new(
            AppErrorCategory::Favorites,
            "favorites are full; remove one before adding another",
        )),
    );

    for (state, expected) in [
        (&loading, "Loading favorites"),
        (&empty, "No favorites yet · press f"),
        (&populated, "Favorite row 35"),
        (&error, "favorites are full"),
    ] {
        let mut terminal = Terminal::new(TestBackend::new(60, 18))?;
        terminal.draw(|frame| {
            render_with_model(
                frame,
                state,
                &Theme::default(),
                &RenderModel::default()
                    .with_view(NavigationItem::Favorites)
                    .with_focus(FocusRegion::Content),
            );
        })?;
        let rendered = terminal.backend().to_string();
        assert!(
            rendered.contains(expected),
            "missing `{expected}`:\n{rendered}"
        );
    }
    assert_eq!(error.favorites().entries().len(), 40);
    Ok(())
}

fn queue_mouse_state() -> AppState {
    let mut state = AppState::default();
    let mut selected = None;
    for index in 0..40 {
        let item = mouse_media("Queue", index, MediaKind::Song);
        if index == 35 {
            selected = Some(ytermusic::app::stable_queue_item_id(&item.id));
        }
        state = reduce(state, Action::EnqueueMedia { item }).0;
    }
    reduce(
        state,
        Action::PlayQueueItem {
            id: selected.unwrap_or_else(|| panic!("queue selection")),
        },
    )
    .0
}

#[test]
fn spectrum_layouts_render_quiet_baseline_only_when_enabled_and_not_tiny()
-> Result<(), Box<dyn Error>> {
    let presentation = SpectrumPresentation::quiet();
    for (width, height, expected) in [(140, 40, true), (90, 30, true), (40, 12, false)] {
        let mut terminal = Terminal::new(TestBackend::new(width, height))?;
        terminal.draw(|frame| {
            render_with_model_and_spectrum(
                frame,
                &AppState::default(),
                &Theme::default(),
                &RenderModel::default(),
                &presentation,
            );
        })?;
        let rendered = terminal.backend().to_string();
        assert_eq!(
            rendered.contains('▁'),
            expected,
            "{width}x{height}:\n{rendered}"
        );
        assert!(rendered.contains("Nothing playing"), "{rendered}");
    }

    let mut config = Config::default();
    config.visualizer.enabled = false;
    let state = AppState::new(config);
    let mut baseline = Terminal::new(TestBackend::new(140, 40))?;
    baseline.draw(|frame| {
        render_with_model(frame, &state, &Theme::default(), &RenderModel::default());
    })?;
    let mut candidate = Terminal::new(TestBackend::new(140, 40))?;
    candidate.draw(|frame| {
        render_with_model_and_spectrum(
            frame,
            &state,
            &Theme::default(),
            &RenderModel::default(),
            &presentation,
        );
    })?;
    assert_eq!(candidate.backend().buffer(), baseline.backend().buffer());
    Ok(())
}

#[test]
fn spectrum_settings_show_enabled_state_and_bounded_frame_rate() -> Result<(), Box<dyn Error>> {
    let mut terminal = Terminal::new(TestBackend::new(140, 40))?;
    let model = RenderModel::default()
        .with_view(NavigationItem::Settings)
        .with_visualizer_max_fps(30);
    terminal.draw(|frame| {
        render_with_model(frame, &AppState::default(), &Theme::default(), &model);
    })?;
    let rendered = terminal.backend().to_string();
    assert!(rendered.contains("Spectrum visualizer: on"), "{rendered}");
    assert!(
        rendered.contains("Spectrum frame-rate cap: 30 FPS"),
        "{rendered}"
    );
    assert!(!rendered.contains("Spectrum frame-rate cap: 15 FPS"));
    Ok(())
}

#[test]
fn layout_mode_uses_only_frame_dimensions_at_documented_boundaries() {
    let cases = [
        ((140, 40), LayoutMode::Wide),
        ((90, 30), LayoutMode::Compact),
        ((40, 10), LayoutMode::Tiny),
        ((120, 32), LayoutMode::Wide),
        ((119, 32), LayoutMode::Compact),
        ((120, 31), LayoutMode::Compact),
        ((60, 18), LayoutMode::Compact),
        ((59, 18), LayoutMode::Tiny),
        ((60, 17), LayoutMode::Tiny),
        ((0, 0), LayoutMode::Tiny),
    ];

    for ((width, height), expected) in cases {
        assert_eq!(
            LayoutMode::from_dimensions(width, height),
            expected,
            "{width}x{height}"
        );
        assert_eq!(
            LayoutMode::for_area(Rect::new(17, 23, width, height)),
            expected,
            "the area origin must not affect the mode"
        );
    }
}

#[test]
fn normal_mode_maps_all_global_shortcuts() {
    let cases = [
        (plain('q'), SemanticAction::Quit),
        (shifted('?'), SemanticAction::ToggleHelp),
        (plain('/'), SemanticAction::OpenSearch),
        (shifted(':'), SemanticAction::OpenPalette),
        (plain(' '), SemanticAction::TogglePlayback),
        (plain('n'), SemanticAction::NextTrack),
        (plain('p'), SemanticAction::PreviousTrack),
        (plain('f'), SemanticAction::ToggleFavorite),
        (plain('+'), SemanticAction::VolumeUp),
        (plain('-'), SemanticAction::VolumeDown),
        (plain('s'), SemanticAction::ToggleShuffle),
        (plain('r'), SemanticAction::CycleRepeat),
        (plain('e'), SemanticAction::ToggleRadio),
        (plain('['), SemanticAction::MoveQueueItemUp),
        (plain(']'), SemanticAction::MoveQueueItemDown),
        (plain('a'), SemanticAction::ConnectAccount),
        (plain('m'), SemanticAction::LoadMore),
        (plain('d'), SemanticAction::RecheckDependencies),
        (plain('c'), SemanticAction::ChooseCountry),
        (shifted('L'), SemanticAction::ToggleLyrics),
        (shifted('Q'), SemanticAction::ToggleQueuePanel),
        (key(KeyCode::Esc), SemanticAction::Cancel),
    ];

    for (event, expected) in cases {
        assert_eq!(
            map_event(InputMode::Normal, event),
            Some(InputAction::Semantic(expected)),
            "{event:?}"
        );
    }
}

#[test]
fn favorite_shortcut_remains_text_in_search_and_palette_entry() {
    for context in [TextEntryContext::Search, TextEntryContext::Palette] {
        assert_eq!(
            map_event(InputMode::TextEntry(context), plain('f')),
            Some(InputAction::InsertCharacter('f'))
        );
    }
}

#[test]
fn function_and_media_keys_map_to_transport_actions() {
    let cases = [
        (KeyCode::F(7), SemanticAction::PreviousTrack),
        (KeyCode::F(8), SemanticAction::TogglePlayback),
        (KeyCode::F(9), SemanticAction::NextTrack),
        (
            KeyCode::Media(MediaKeyCode::TrackPrevious),
            SemanticAction::PreviousTrack,
        ),
        (
            KeyCode::Media(MediaKeyCode::PlayPause),
            SemanticAction::TogglePlayback,
        ),
        (
            KeyCode::Media(MediaKeyCode::TrackNext),
            SemanticAction::NextTrack,
        ),
    ];

    for (code, expected) in cases {
        assert_eq!(
            map_event(InputMode::Normal, key(code)),
            Some(InputAction::Semantic(expected)),
            "{code:?}"
        );
    }

    for code in [
        KeyCode::Media(MediaKeyCode::Play),
        KeyCode::Media(MediaKeyCode::Pause),
    ] {
        assert_eq!(map_event(InputMode::Normal, key(code)), None, "{code:?}");
    }

    for context in [TextEntryContext::Search, TextEntryContext::Palette] {
        let mode = InputMode::TextEntry(context);
        for (code, _) in cases {
            assert_eq!(map_event(mode, key(code)), None, "{mode:?} {code:?}");
        }
    }
}

#[test]
fn transport_palette_metadata_lists_function_media_and_seek_bindings() {
    let shortcut = |action| {
        palette_entries()
            .iter()
            .find(|entry| entry.action == action)
            .map(|entry| entry.shortcut)
    };

    assert_eq!(
        shortcut(SemanticAction::PreviousTrack),
        Some("p / F7 / Media Previous")
    );
    assert_eq!(
        shortcut(SemanticAction::TogglePlayback),
        Some("Space / F8 / Media Play/Pause")
    );
    assert_eq!(
        shortcut(SemanticAction::NextTrack),
        Some("n / F9 / Media Next")
    );
    assert_eq!(shortcut(SemanticAction::SeekBackward), Some("Shift+Left"));
    assert_eq!(shortcut(SemanticAction::SeekForward), Some("Shift+Right"));
}

#[test]
fn shifted_arrows_map_to_relative_seek_only_in_normal_mode() {
    let backward = KeyEvent::new(KeyCode::Left, KeyModifiers::SHIFT);
    let forward = KeyEvent::new(KeyCode::Right, KeyModifiers::SHIFT);
    assert_eq!(
        map_event(InputMode::Normal, backward),
        Some(InputAction::Semantic(SemanticAction::SeekBackward))
    );
    assert_eq!(
        map_event(InputMode::Normal, forward),
        Some(InputAction::Semantic(SemanticAction::SeekForward))
    );

    for context in [TextEntryContext::Search, TextEntryContext::Palette] {
        let mode = InputMode::TextEntry(context);
        assert_eq!(map_event(mode, backward), None);
        assert_eq!(map_event(mode, forward), None);
    }
}

#[test]
fn tab_focus_shortcuts_map_in_normal_mode() {
    assert_eq!(
        map_event(
            InputMode::Normal,
            KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE),
        ),
        Some(InputAction::Semantic(SemanticAction::CycleFocusForward)),
    );
    assert_eq!(
        map_event(
            InputMode::Normal,
            KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT),
        ),
        Some(InputAction::Semantic(SemanticAction::CycleFocusBackward,)),
    );
}

#[test]
fn tab_focus_shortcuts_are_consumed_during_text_entry() {
    for context in [TextEntryContext::Search, TextEntryContext::Palette] {
        let mode = InputMode::TextEntry(context);
        assert_eq!(
            map_event(mode, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            None,
        );
        assert_eq!(
            map_event(mode, KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT)),
            None,
        );
    }
}

#[test]
fn lyrics_shortcut_is_normal_mode_only() {
    assert_eq!(
        map_event(InputMode::Normal, shifted('L')),
        Some(InputAction::Semantic(SemanticAction::ToggleLyrics))
    );
    for context in [TextEntryContext::Search, TextEntryContext::Palette] {
        assert_eq!(
            map_event(InputMode::TextEntry(context), plain('l')),
            Some(InputAction::InsertCharacter('l'))
        );
        assert_eq!(
            map_event(InputMode::TextEntry(context), shifted('L')),
            Some(InputAction::InsertCharacter('L'))
        );
    }
}

#[test]
fn arrow_and_vim_navigation_are_semantically_equivalent() {
    let cases = [
        (KeyCode::Up, 'k', SemanticAction::MoveUp),
        (KeyCode::Down, 'j', SemanticAction::MoveDown),
        (KeyCode::Left, 'h', SemanticAction::MoveLeft),
        (KeyCode::Right, 'l', SemanticAction::MoveRight),
    ];

    for (arrow, vim, expected) in cases {
        assert_eq!(
            map_event(InputMode::Normal, key(arrow)),
            Some(InputAction::Semantic(expected))
        );
        assert_eq!(
            map_event(InputMode::Normal, plain(vim)),
            Some(InputAction::Semantic(expected))
        );
    }
}

#[test]
fn text_entry_consumes_printable_keys_before_global_mapping() {
    for context in [TextEntryContext::Search, TextEntryContext::Palette] {
        let mode = InputMode::TextEntry(context);
        for character in ['q', '?', '/', ':', ' ', 'n', 's', 'l', 'L', '界'] {
            let event = if matches!(character, '?' | ':') {
                shifted(character)
            } else {
                plain(character)
            };
            assert_eq!(
                map_event(mode, event),
                Some(InputAction::InsertCharacter(character))
            );
        }
        assert_eq!(
            map_event(mode, shifted('Q')),
            Some(InputAction::InsertCharacter('Q')),
            "text entry consumes uppercase Q before the global queue toggle"
        );
        assert_eq!(
            map_event(mode, shifted('L')),
            Some(InputAction::InsertCharacter('L')),
            "text entry consumes uppercase L before the lyrics toggle"
        );

        assert_eq!(
            map_event(mode, key(KeyCode::Backspace)),
            Some(InputAction::Semantic(SemanticAction::DeleteBackward))
        );
        assert_eq!(
            map_event(mode, key(KeyCode::Enter)),
            Some(InputAction::Semantic(SemanticAction::Submit))
        );
        assert_eq!(
            map_event(mode, key(KeyCode::Esc)),
            Some(InputAction::Semantic(SemanticAction::Cancel))
        );
        assert_eq!(
            map_event(mode, key(KeyCode::Up)),
            Some(InputAction::Semantic(SemanticAction::MoveUp))
        );
    }
}

#[test]
fn key_mapper_accepts_press_and_repeat_but_rejects_release_and_unsafe_modifiers() {
    assert_eq!(
        map_event(
            InputMode::Normal,
            KeyEvent::new_with_kind(KeyCode::Char('n'), KeyModifiers::NONE, KeyEventKind::Press),
        ),
        Some(InputAction::Semantic(SemanticAction::NextTrack))
    );
    assert_eq!(
        map_event(
            InputMode::Normal,
            KeyEvent::new_with_kind(KeyCode::Char('n'), KeyModifiers::NONE, KeyEventKind::Repeat),
        ),
        Some(InputAction::Semantic(SemanticAction::NextTrack))
    );
    assert_eq!(
        map_event(
            InputMode::Normal,
            KeyEvent::new_with_kind(
                KeyCode::Char('n'),
                KeyModifiers::NONE,
                KeyEventKind::Release
            ),
        ),
        None
    );
    assert_eq!(
        map_event(
            InputMode::Normal,
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL),
        ),
        None
    );
    assert_eq!(
        map_event(
            InputMode::TextEntry(TextEntryContext::Search),
            KeyEvent::new(KeyCode::Char('\u{7}'), KeyModifiers::NONE),
        ),
        None,
        "control characters are not printable text input"
    );
    assert_eq!(
        map_event(
            InputMode::Normal,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        ),
        Some(InputAction::Semantic(SemanticAction::Quit))
    );
}

#[test]
fn international_text_entry_accepts_option_and_altgr_but_keeps_globals_strict() {
    let international = [
        (KeyEvent::new(KeyCode::Char('é'), KeyModifiers::ALT), 'é'),
        (
            KeyEvent::new(
                KeyCode::Char('@'),
                KeyModifiers::CONTROL | KeyModifiers::ALT,
            ),
            '@',
        ),
        (
            KeyEvent::new(KeyCode::Char('É'), KeyModifiers::SHIFT | KeyModifiers::ALT),
            'É',
        ),
        (
            KeyEvent::new(
                KeyCode::Char('€'),
                KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT,
            ),
            '€',
        ),
    ];
    for context in [TextEntryContext::Search, TextEntryContext::Palette] {
        let mode = InputMode::TextEntry(context);
        for (event, expected) in international {
            assert_eq!(
                map_event(mode, event),
                Some(InputAction::InsertCharacter(expected))
            );
        }
        for modifiers in [
            KeyModifiers::CONTROL,
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            KeyModifiers::SUPER,
            KeyModifiers::HYPER,
            KeyModifiers::META,
            KeyModifiers::ALT | KeyModifiers::SUPER,
        ] {
            assert_eq!(
                map_event(mode, KeyEvent::new(KeyCode::Char('x'), modifiers),),
                None,
                "unsafe modifier combination {modifiers:?} must not insert text"
            );
        }
        assert_eq!(
            map_event(
                mode,
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            ),
            Some(InputAction::Semantic(SemanticAction::Quit)),
            "exact Ctrl-C remains a global quit action"
        );
    }

    assert_eq!(
        map_event(
            InputMode::Normal,
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::ALT),
        ),
        None
    );
    assert_eq!(
        map_event(
            InputMode::Normal,
            KeyEvent::new(
                KeyCode::Char('c'),
                KeyModifiers::CONTROL | KeyModifiers::ALT,
            ),
        ),
        None
    );
    assert_eq!(
        map_event(
            InputMode::Normal,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        ),
        Some(InputAction::Semantic(SemanticAction::Quit))
    );
}

#[test]
fn every_semantic_action_has_one_nonempty_unique_palette_entry() {
    let entries = palette_entries();
    let actions: HashSet<_> = entries.iter().map(|entry| entry.action).collect();
    let labels: HashSet<_> = entries.iter().map(|entry| entry.label).collect();
    let shortcuts: HashSet<_> = entries.iter().map(|entry| entry.shortcut).collect();

    assert_eq!(entries.len(), SemanticAction::ALL.len());
    assert_eq!(actions.len(), SemanticAction::ALL.len());
    assert_eq!(labels.len(), entries.len());
    assert_eq!(shortcuts.len(), entries.len());
    assert!(entries.iter().all(|entry| {
        !entry.label.trim().is_empty()
            && !entry.shortcut.trim().is_empty()
            && SemanticAction::ALL.contains(&entry.action)
    }));
    assert_eq!(
        actions,
        SemanticAction::ALL.iter().copied().collect::<HashSet<_>>()
    );
}

#[test]
fn mixed_width_text_is_truncated_by_display_cells() {
    let value = "A界e\u{301}👩‍💻 — a deliberately long title";

    for width in 0..=16 {
        let truncated = truncate_cells(value, width);
        assert!(
            usize::from(truncated.as_str().cell_width()) <= width,
            "{width}: {truncated:?}"
        );
    }
    assert_eq!(truncate_cells("界", 1), "…");
    assert_eq!(truncate_cells("e\u{301}", 1), "e\u{301}");
    assert_eq!(truncate_cells("unchanged", 20), "unchanged");
}

#[test]
fn truncation_never_splits_flags_combining_or_zwj_graphemes() {
    assert_eq!(truncate_cells("🇭🇰X", 2), "…");
    assert_eq!(truncate_cells("🇭🇰XY", 3), "🇭🇰…");
    assert_eq!(truncate_cells("e\u{301}XY", 2), "e\u{301}…");
    assert_eq!(truncate_cells("👨‍👩‍👧‍👦XY", 3), "👨‍👩‍👧‍👦…");
}

#[test]
fn halfwidth_katakana_truncation_matches_the_terminal_backend() -> Result<(), Box<dyn Error>> {
    let truncated = truncate_cells("ｶﾞX", 2);

    assert_eq!(truncated, "…");
    assert!(truncated.as_str().cell_width() <= 2);

    let backend = TestBackend::new(2, 1);
    let mut terminal = Terminal::new(backend)?;
    terminal.draw(|frame| frame.render_widget(Paragraph::new(truncated.as_str()), frame.area()))?;
    let buffer = terminal.backend().buffer();
    assert_eq!(buffer.cell((0, 0)).map(Cell::symbol), Some("…"));
    assert_eq!(buffer.cell((1, 0)).map(Cell::symbol), Some(" "));
    Ok(())
}

#[test]
fn whole_line_clipping_is_atomic_across_span_boundaries_and_preserves_styles()
-> Result<(), Box<dyn Error>> {
    let line_style = Style::default().bg(Color::Black);
    let first_style = Style::default().fg(Color::Red).add_modifier(Modifier::BOLD);
    let second_style = Style::default()
        .fg(Color::Blue)
        .add_modifier(Modifier::ITALIC);
    let adjacent_style = Style::default()
        .fg(Color::Green)
        .add_modifier(Modifier::UNDERLINED);
    let line = Line::from(vec![
        Span::styled("👨‍", first_style),
        Span::styled("👩‍👧‍👦", second_style),
        Span::styled("XY", adjacent_style),
    ])
    .style(line_style)
    .alignment(Alignment::Center);

    let clipped = clip_line(&line, 3);
    assert_eq!(clipped.style, line_style);
    assert_eq!(clipped.alignment, line.alignment);
    assert_eq!(
        clipped.spans,
        vec![
            Span::styled("👨‍👩‍👧‍👦", first_style),
            Span::styled("…", adjacent_style),
        ]
    );

    let normalized = clip_line(&line, 4);
    assert_eq!(normalized.alignment, line.alignment);
    assert_eq!(
        normalized.spans,
        vec![
            Span::styled("👨‍👩‍👧‍👦", first_style),
            Span::styled("XY", adjacent_style),
        ]
    );

    let omitted = clip_line(&line, 2);
    assert_eq!(omitted.style, line_style);
    assert_eq!(omitted.alignment, line.alignment);
    assert_eq!(omitted.spans, vec![Span::styled("…", first_style)]);

    let backend = TestBackend::new(3, 1);
    let mut terminal = Terminal::new(backend)?;
    terminal.draw(|frame| frame.render_widget(Paragraph::new(clipped), frame.area()))?;
    let buffer = terminal.backend().buffer();
    assert_eq!(buffer.cell((0, 0)).map(Cell::symbol), Some("👨‍👩‍👧‍👦"));
    assert_eq!(buffer.cell((0, 0)).map(|cell| cell.fg), Some(Color::Red));
    assert_eq!(buffer.cell((0, 0)).map(|cell| cell.bg), Some(Color::Black));
    assert_eq!(
        buffer.cell((0, 0)).map(|cell| cell.modifier),
        Some(Modifier::BOLD)
    );
    assert_eq!(buffer.cell((1, 0)).map(Cell::symbol), Some(" "));
    assert_eq!(buffer.cell((2, 0)).map(Cell::symbol), Some("…"));
    assert_eq!(buffer.cell((2, 0)).map(|cell| cell.fg), Some(Color::Green));
    assert_eq!(buffer.cell((2, 0)).map(|cell| cell.bg), Some(Color::Black));
    assert_eq!(
        buffer.cell((2, 0)).map(|cell| cell.modifier),
        Some(Modifier::UNDERLINED)
    );
    Ok(())
}

#[test]
fn narrow_truncation_of_long_input_has_bounded_output() {
    let input = "a".repeat(250_000);
    let truncated = truncate_cells(&input, 4);
    assert_eq!(truncated, "aaa…");
    assert_eq!(truncated.as_str().cell_width(), 4);
}

#[test]
fn truncation_caps_many_zero_width_graphemes_and_marks_the_hidden_suffix() {
    let input = format!("ok{}tail", "\u{200b}".repeat(20_000));

    let truncated = truncate_cells(&input, 20);

    assert!(truncated.ends_with('…'));
    assert!(
        truncated.len() < 16_000,
        "grapheme inspection must stop independently of the byte budget"
    );
    assert!(truncated.as_str().cell_width() <= 20);
}

#[test]
fn line_clipping_drops_a_byte_budget_cut_combining_cluster() {
    let line_style = Style::default().bg(Color::Black);
    let base_style = Style::default().fg(Color::Red);
    let combining_style = Style::default().fg(Color::Blue);
    let suffix_style = Style::default().fg(Color::Green);
    let line = Line::from(vec![
        Span::styled("A", base_style),
        Span::styled("\u{301}".repeat(100_000), combining_style),
        Span::styled("TAIL", suffix_style),
    ])
    .style(line_style);

    let clipped = clip_line(&line, 20);

    assert_eq!(clipped.style, line_style);
    assert_eq!(clipped.spans, vec![Span::styled("…", base_style)]);
}

#[test]
fn wide_layout_snapshot() -> Result<(), Box<dyn Error>> {
    assert_ui_snapshot(
        "wide",
        140,
        40,
        &RenderModel::default()
            .with_view(NavigationItem::Search)
            .with_focus(FocusRegion::Content),
    )
}

#[test]
fn compact_layout_snapshot() -> Result<(), Box<dyn Error>> {
    assert_ui_snapshot(
        "compact",
        90,
        30,
        &RenderModel::default()
            .with_view(NavigationItem::Search)
            .with_focus(FocusRegion::Content),
    )
}

#[test]
fn player_control_labels_show_music_actions_and_current_pause_state() -> Result<(), Box<dyn Error>>
{
    let state = fixed_state();

    let wide = render_state(140, 40, &state)?;
    let compact = render_state(90, 30, &state)?;
    for label in [
        "[p Previous]",
        "[⇧← −10s]",
        "[Space Pause]",
        "[⇧→ +10s]",
        "[n Next]",
    ] {
        assert!(wide.contains(label), "missing `{label}`:\n{wide}");
        assert!(compact.contains(label), "missing `{label}`:\n{compact}");
    }

    Ok(())
}

#[test]
fn player_control_labels_use_podcast_intervals_and_current_play_state() -> Result<(), Box<dyn Error>>
{
    let state = playback_state(
        MediaKind::PodcastEpisode,
        PlaybackStatus::Paused,
        30_000,
        Some(120_000),
        Config {
            podcast: PodcastConfig {
                skip_backward_seconds: 15,
                skip_forward_seconds: 30,
                ..PodcastConfig::default()
            },
            ..Config::default()
        },
    );

    let compact = render_state(60, 18, &state)?;
    for label in ["[p]", "[⇧← −15s]", "[Spc Play]", "[⇧→ +30s]", "[n]"] {
        assert!(compact.contains(label), "missing `{label}`:\n{compact}");
    }
    assert!(!compact.contains("−10s"), "{compact}");
    assert!(compact.contains('█') && compact.contains('░'), "{compact}");

    Ok(())
}

#[test]
fn compact_minimum_keeps_shift_seek_keys_with_maximum_configured_intervals()
-> Result<(), Box<dyn Error>> {
    let state = playback_state(
        MediaKind::PodcastEpisode,
        PlaybackStatus::Playing,
        30_000,
        Some(120_000),
        Config {
            podcast: PodcastConfig {
                skip_backward_seconds: 600,
                skip_forward_seconds: 600,
                ..PodcastConfig::default()
            },
            ..Config::default()
        },
    );
    let compact = render_state(60, 18, &state)?;

    for label in ["[p]", "[⇧← −600s]", "[Spc Pause]", "[⇧→ +600s]", "[n]"] {
        assert!(compact.contains(label), "missing `{label}`:\n{compact}");
    }

    Ok(())
}

#[test]
fn wide_boundary_prioritizes_controls_and_minimum_progress_over_wide_quality()
-> Result<(), Box<dyn Error>> {
    let state = playback_state(
        MediaKind::Song,
        PlaybackStatus::Playing,
        50_000,
        Some(100_000),
        Config::default(),
    );
    let state = with_quality(state, &"界".repeat(64), &"測".repeat(64));
    let rendered = render_state(120, 32, &state)?;
    let controls = rendered
        .lines()
        .find(|line| line.contains("[p"))
        .unwrap_or_else(|| panic!("wide boundary must render controls:\n{rendered}"));

    for key in ["[p", "⇧←", "Space Pause", "⇧→", "[n"] {
        assert!(controls.contains(key), "missing `{key}` in `{controls}`");
    }
    let progress = controls
        .split_whitespace()
        .find(|field| field.contains('█') && field.contains('░'))
        .unwrap_or_else(|| panic!("proportional progress bar missing from `{controls}`"));
    assert!(usize::from(progress.cell_width()) >= 5, "`{progress}`");

    Ok(())
}

#[test]
fn player_progress_bar_clamps_start_middle_end_and_disables_unknown_duration()
-> Result<(), Box<dyn Error>> {
    let scenarios = [
        (0, Some(100_000), "░".repeat(20)),
        (
            50_000,
            Some(100_000),
            format!("{}{}", "█".repeat(10), "░".repeat(10)),
        ),
        (100_000, Some(100_000), "█".repeat(20)),
        (u64::MAX, Some(100_000), "█".repeat(20)),
        (50_000, None, "─".repeat(20)),
        (50_000, Some(0), "─".repeat(20)),
    ];

    for (position_ms, duration_ms, expected) in scenarios {
        let state = playback_state(
            MediaKind::Song,
            PlaybackStatus::Playing,
            position_ms,
            duration_ms,
            Config::default(),
        );
        let rendered = render_state(120, 32, &state)?;
        assert!(
            rendered.contains(&expected),
            "{position_ms:?}/{duration_ms:?}:\n{rendered}"
        );
    }

    Ok(())
}

#[test]
fn progress_bar_uses_exact_motion_fraction_gradient_and_fractional_cell()
-> Result<(), Box<dyn Error>> {
    let state = playback_state(
        MediaKind::Song,
        PlaybackStatus::Paused,
        50_000,
        Some(100_000),
        Config::default(),
    );
    let model = RenderModel::default().with_motion_frame(MotionFrame {
        elapsed_ms: 400,
        spinner_index: 5,
        progress: ProgressPresentation {
            fraction: 0.025,
            shimmer_phase: 0.75,
        },
    });
    let mut map = InteractionMap::new(FrameRevision::new(90));
    let mut terminal = Terminal::new(TestBackend::new(140, 40))?;
    terminal.draw(|frame| {
        render_with_model_and_interactions(frame, &state, &Theme::default(), &model, &mut map);
    })?;
    let mut progress = (0..40)
        .flat_map(|row| (0..140).map(move |column| (column, row)))
        .filter_map(
            |(column, row)| match map.resolve(column, row, map.revision()) {
                Some(HitTarget::Progress { numerator, .. }) => Some((numerator, column, row)),
                _ => None,
            },
        )
        .collect::<Vec<_>>();
    progress.sort_unstable();
    assert_eq!(progress.len(), 20);
    let cells = progress
        .iter()
        .filter_map(|(_, column, row)| terminal.backend().buffer().cell((*column, *row)))
        .collect::<Vec<_>>();
    assert_eq!(cells[0].symbol(), "▌");
    assert!(cells[1..].iter().all(|cell| cell.symbol() == "░"));
    assert_eq!(cells[0].fg, Theme::default().accent);
    assert!(
        cells[1..]
            .iter()
            .all(|cell| cell.fg == Theme::default().muted)
    );
    Ok(())
}

#[test]
fn progress_bar_fallback_ramp_reaches_both_theme_accents() -> Result<(), Box<dyn Error>> {
    let state = playback_state(
        MediaKind::Song,
        PlaybackStatus::Paused,
        100_000,
        Some(100_000),
        Config::default(),
    );
    for capability in [ColorCapability::Ansi256, ColorCapability::Basic] {
        let theme = Theme::for_capability(capability);
        let model = RenderModel::default().with_motion_frame(MotionFrame {
            progress: ProgressPresentation {
                fraction: 1.0,
                shimmer_phase: 0.0,
            },
            ..MotionFrame::default()
        });
        let mut terminal = Terminal::new(TestBackend::new(140, 40))?;
        terminal.draw(|frame| render_with_model(frame, &state, &theme, &model))?;
        let buffer = terminal.backend().buffer();
        assert!(
            buffer
                .content
                .iter()
                .any(|cell| cell.symbol() == "█" && cell.fg == theme.accent)
        );
        assert!(
            buffer
                .content
                .iter()
                .any(|cell| cell.symbol() == "█" && cell.fg == theme.selection)
        );
    }
    Ok(())
}

#[test]
fn progress_bar_shimmer_moves_only_within_fill_and_paused_frames_freeze()
-> Result<(), Box<dyn Error>> {
    let render_progress = |status, shimmer_phase| -> Result<Vec<(String, Color)>, Box<dyn Error>> {
        let state = playback_state(
            MediaKind::Song,
            status,
            50_000,
            Some(100_000),
            Config::default(),
        );
        let model = RenderModel::default().with_motion_frame(MotionFrame {
            progress: ProgressPresentation {
                fraction: 0.5,
                shimmer_phase,
            },
            ..MotionFrame::default()
        });
        let mut map = InteractionMap::new(FrameRevision::new(91));
        let mut terminal = Terminal::new(TestBackend::new(140, 40))?;
        terminal.draw(|frame| {
            render_with_model_and_interactions(frame, &state, &Theme::default(), &model, &mut map);
        })?;
        let mut cells = (0..40)
            .flat_map(|row| (0..140).map(move |column| (column, row)))
            .filter_map(
                |(column, row)| match map.resolve(column, row, map.revision()) {
                    Some(HitTarget::Progress { numerator, .. }) => terminal
                        .backend()
                        .buffer()
                        .cell((column, row))
                        .map(|cell| (numerator, cell.symbol().to_owned(), cell.fg)),
                    _ => None,
                },
            )
            .collect::<Vec<_>>();
        cells.sort_unstable_by_key(|cell| cell.0);
        Ok(cells
            .into_iter()
            .map(|(_, symbol, color)| (symbol, color))
            .collect())
    };

    let shimmer_start = render_progress(PlaybackStatus::Playing, 0.0)?;
    let shimmer_end = render_progress(PlaybackStatus::Playing, 1.0)?;
    assert_ne!(shimmer_start[..10], shimmer_end[..10]);
    assert_eq!(shimmer_start[10..], shimmer_end[10..]);
    assert_ne!(shimmer_start[0].1, Theme::default().accent);
    assert_ne!(shimmer_end[9].1, shimmer_start[9].1);
    assert_ne!(shimmer_start[1].1, Theme::default().accent);

    let paused_start = render_progress(PlaybackStatus::Paused, 0.0)?;
    let paused_end = render_progress(PlaybackStatus::Paused, 1.0)?;
    assert_eq!(paused_start, paused_end);
    Ok(())
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "the table keeps all approved loading surfaces and shared frame indices together"
)]
fn spinner_frames_prefix_every_visible_loading_surface_and_wrap() -> Result<(), Box<dyn Error>> {
    let region = test_region();
    let search = reduce(
        AppState::default(),
        Action::SearchSubmitted {
            query: "spinner".to_owned(),
            filter: SearchFilter::Songs,
        },
    )
    .0;
    let charts = reduce(
        AppState::default(),
        Action::ChartsRequested {
            region: region.clone(),
        },
    )
    .0;
    let podcasts = reduce(
        AppState::default(),
        Action::PodcastRecommendationsRequested { region },
    )
    .0;
    let library = reduce(
        reduce(
            AppState::default(),
            Action::AuthenticationChanged(AuthenticationState::Authenticated),
        )
        .0,
        Action::LibraryRequested {
            section: LibrarySection::Songs,
        },
    )
    .0;
    let favorites = reduce(AppState::default(), Action::FavoritesRequested).0;
    let history = reduce(AppState::default(), Action::HistoryRequested).0;
    let lyrics = playback_state(
        MediaKind::Song,
        PlaybackStatus::Resolving,
        0,
        Some(100_000),
        Config::default(),
    );
    assert!(lyrics.lyrics().loading());

    for (spinner_index, frame) in [(0, "⠋"), (1, "⠙"), (10, "⠋")] {
        for (view, state, label) in [
            (NavigationItem::Search, &search, "Searching"),
            (NavigationItem::Charts, &charts, "Loading regional charts"),
            (
                NavigationItem::Podcasts,
                &podcasts,
                "Loading recommendations",
            ),
            (NavigationItem::Library, &library, "Loading library"),
            (NavigationItem::Favorites, &favorites, "Loading favorites"),
            (NavigationItem::History, &history, "Loading history"),
        ] {
            let mut terminal = Terminal::new(TestBackend::new(80, 24))?;
            let model = RenderModel::default()
                .with_view(view)
                .with_motion_frame(MotionFrame {
                    spinner_index,
                    ..MotionFrame::default()
                });
            terminal.draw(|render_frame| {
                render_with_model(render_frame, state, &Theme::default(), &model);
            })?;
            let rendered = terminal.backend().to_string();
            assert!(
                rendered.contains(&format!("{frame} {label}")),
                "{view:?} index {spinner_index}:\n{rendered}"
            );
        }

        let mut terminal = Terminal::new(TestBackend::new(80, 24))?;
        let model = RenderModel::default()
            .with_overlay(Overlay::Lyrics)
            .with_motion_frame(MotionFrame {
                spinner_index,
                ..MotionFrame::default()
            });
        terminal.draw(|render_frame| {
            render_with_model(render_frame, &lyrics, &Theme::default(), &model);
        })?;
        assert!(
            terminal
                .backend()
                .to_string()
                .contains(&format!("{frame} Loading lyrics…"))
        );
    }

    let hidden = RenderModel::default().with_motion_frame(MotionFrame {
        spinner_index: 1,
        ..MotionFrame::default()
    });
    let mut terminal = Terminal::new(TestBackend::new(80, 24))?;
    terminal.draw(|frame| render_with_model(frame, &search, &Theme::default(), &hidden))?;
    assert!(!terminal.backend().to_string().contains("⠙ Searching"));

    let resolving = RenderModel::default().with_motion_frame(MotionFrame {
        spinner_index: 1,
        ..MotionFrame::default()
    });
    let mut terminal = Terminal::new(TestBackend::new(140, 40))?;
    terminal.draw(|frame| render_with_model(frame, &lyrics, &Theme::default(), &resolving))?;
    let rendered = terminal.backend().to_string();
    assert!(rendered.contains("[- Loading…]"));
    assert!(!rendered.contains("⠙ Loading…"));
    Ok(())
}

#[test]
fn spinner_loading_more_and_refresh_states_retain_rows_and_errors() -> Result<(), Box<dyn Error>> {
    let item = mouse_media("Spinner retained", 0, MediaKind::Song);
    let (search, _) = reduce(
        AppState::default(),
        Action::SearchSubmitted {
            query: "retained".to_owned(),
            filter: SearchFilter::Songs,
        },
    );
    let search_generation = search.search().generation();
    let search = reduce(
        search,
        Action::SearchCompleted {
            generation: search_generation,
            result: Ok(
                SearchPage::new(vec![SearchItem::Playable(item.clone())]).with_continuation("more")
            ),
        },
    )
    .0;
    let search = reduce(search, Action::SearchMoreRequested).0;

    let (library, _) = reduce(
        AppState::default(),
        Action::AuthenticationChanged(AuthenticationState::Authenticated),
    );
    let (library, _) = reduce(
        library,
        Action::LibraryRequested {
            section: LibrarySection::Songs,
        },
    );
    let library_generation = library.library().generation();
    let library = reduce(
        library,
        Action::LibraryCompleted {
            generation: library_generation,
            result: Ok(Page {
                items: vec![LibraryItem::Playable(item.clone())],
                continuation: Some("more".to_owned()),
                stale: false,
            }),
        },
    )
    .0;
    let library = reduce(library, Action::LibraryMoreRequested).0;

    for (view, state) in [
        (NavigationItem::Search, &search),
        (NavigationItem::Library, &library),
    ] {
        let model = RenderModel::default()
            .with_view(view)
            .with_motion_frame(MotionFrame {
                spinner_index: 1,
                ..MotionFrame::default()
            });
        let mut terminal = Terminal::new(TestBackend::new(80, 24))?;
        terminal.draw(|frame| render_with_model(frame, state, &Theme::default(), &model))?;
        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("Spinner retained row 0"), "{rendered}");
        assert!(rendered.contains("⠙ Loading more"), "{rendered}");
    }

    let entry = FavoriteEntry {
        id: 1,
        item,
        favorited_at: 1,
    };
    let populated = complete_favorites(AppState::default(), Ok(vec![entry]));
    let (refreshing, effects) = reduce(populated, Action::FavoritesRequested);
    let [Effect::LoadFavorites { generation }] = effects.as_slice() else {
        panic!("favorites refresh generation");
    };
    let error = reduce(
        refreshing.clone(),
        Action::FavoritesCompleted {
            generation: *generation,
            result: Err(AppError::new(
                AppErrorCategory::Favorites,
                "retained refresh error",
            )),
        },
    )
    .0;
    let model = RenderModel::default()
        .with_view(NavigationItem::Favorites)
        .with_motion_frame(MotionFrame {
            spinner_index: 1,
            ..MotionFrame::default()
        });
    for (state, status) in [
        (&refreshing, "⠙ Loading favorites"),
        (&error, "retained refresh error"),
    ] {
        let mut terminal = Terminal::new(TestBackend::new(80, 24))?;
        terminal.draw(|frame| render_with_model(frame, state, &Theme::default(), &model))?;
        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("Spinner retained row 0"), "{rendered}");
        assert!(rendered.contains(status), "{rendered}");
    }
    Ok(())
}

#[test]
fn playback_control_status_matrix_distinguishes_actions_from_disabled_states()
-> Result<(), Box<dyn Error>> {
    for (status, expected) in [
        (PlaybackStatus::Playing, "[Space Pause]"),
        (PlaybackStatus::Paused, "[Space Play]"),
        (PlaybackStatus::Stopped, "[Space Play]"),
        (PlaybackStatus::Failed, "[Space Play]"),
        (PlaybackStatus::Resolving, "[- Loading…]"),
        (PlaybackStatus::Buffering, "[- Loading…]"),
    ] {
        let state = playback_state(
            MediaKind::Song,
            status,
            50_000,
            Some(100_000),
            Config::default(),
        );
        let rendered = render_state(140, 40, &state)?;
        assert!(rendered.contains(expected), "{status:?}:\n{rendered}");
    }

    let rendered = render_state(140, 40, &AppState::default())?;
    assert!(rendered.contains("[- Play]"), "{rendered}");
    assert!(!rendered.contains("[Space Play]"), "{rendered}");

    Ok(())
}

#[test]
fn tiny_player_adds_abbreviated_controls_only_when_all_telemetry_still_fits()
-> Result<(), Box<dyn Error>> {
    let state = playback_state(
        MediaKind::Song,
        PlaybackStatus::Playing,
        50_000,
        Some(100_000),
        Config::default(),
    );
    let roomy = render_state(59, 17, &state)?;
    let Some(player) = roomy.lines().last() else {
        panic!("roomy tiny render must include a player row");
    };
    let player = player.trim_matches('"');
    for label in ["[p]", "[←]", "[Spc]", "[→]", "[n]"] {
        assert!(player.contains(label), "missing `{label}` in `{player}`");
    }
    assert!(player.contains("0:50/1:40"), "{player}");
    assert!(player.contains("v80/0·"), "{player}");
    assert!(player.contains("s-e"), "{player}");
    assert!(
        usize::from(player.cell_width()) <= 59,
        "width {}: `{player}`",
        player.cell_width()
    );

    let narrow = render_state(20, 8, &state)?;
    let Some(player) = narrow.lines().last() else {
        panic!("narrow tiny render must include a player row");
    };
    let player = player.trim_matches('"');
    assert!(!player.contains("[Spc]"), "{player}");
    assert!(usize::from(player.cell_width()) <= 20);

    Ok(())
}

#[test]
fn compact_boundary_keeps_every_selected_navigation_label_visible() -> Result<(), Box<dyn Error>> {
    for item in NavigationItem::ALL {
        let rendered = render_ui(
            60,
            18,
            &RenderModel::default()
                .with_view(item)
                .with_focus(FocusRegion::Navigation),
        )?;
        let selection = format!("▶ {}", item.compact_label());
        assert!(
            rendered
                .lines()
                .take(3)
                .any(|line| line.contains(&selection)),
            "60x18 navigation must show `{selection}` without color:\n{rendered}"
        );
    }
    Ok(())
}

#[test]
fn compact_queue_tab_toggles_and_renders_the_accessibly_focused_queue() -> Result<(), Box<dyn Error>>
{
    let content = RenderModel::default()
        .with_view(NavigationItem::Search)
        .with_focus(FocusRegion::Content);
    let queue = content.toggle_compact_panel();
    assert_eq!(queue.compact_panel, CompactPanel::Queue);
    assert_eq!(queue.focus, FocusRegion::Queue);

    let content_again = queue.clone().toggle_compact_panel();
    assert_eq!(content_again.compact_panel, CompactPanel::Content);
    assert_eq!(content_again.focus, FocusRegion::Content);

    assert_ui_snapshot("compact_queue", 90, 30, &queue)
}

#[test]
fn render_model_normalizes_only_the_effective_layout_focus() {
    let queue = RenderModel::default()
        .with_view(NavigationItem::Search)
        .toggle_compact_panel();

    assert_eq!(
        queue.normalized_for_layout(LayoutMode::Compact),
        queue,
        "the visible compact queue remains focused"
    );
    assert_eq!(
        queue.normalized_for_layout(LayoutMode::Wide),
        queue,
        "wide mode exposes both content and queue"
    );

    let tiny = queue.normalized_for_layout(LayoutMode::Tiny);
    assert_eq!(tiny.compact_panel, CompactPanel::Content);
    assert_eq!(tiny.focus, FocusRegion::Content);
    assert_eq!(queue.compact_panel, CompactPanel::Queue);
    assert_eq!(queue.focus, FocusRegion::Queue);
}

#[test]
fn queue_focused_model_renders_compact_then_tiny_without_hidden_focus() -> Result<(), Box<dyn Error>>
{
    let model = RenderModel::default()
        .with_view(NavigationItem::Search)
        .toggle_compact_panel();

    let compact = render_ui(90, 30, &model)?;
    assert!(compact.contains("[*] Queue"));
    assert!(!compact.contains("focused content"));

    let tiny = render_ui(40, 10, &model)?;
    assert!(tiny.contains("[*] ▶ Search"));
    assert!(!tiny.contains("Queue"));
    assert_eq!(model.focus, FocusRegion::Queue);
    assert_eq!(model.compact_panel, CompactPanel::Queue);
    Ok(())
}

#[test]
fn content_title_only_claims_focus_when_content_is_focused() -> Result<(), Box<dyn Error>> {
    let focused = RenderModel::default()
        .with_view(NavigationItem::Search)
        .with_focus(FocusRegion::Content);
    assert!(render_ui(140, 40, &focused)?.contains("Search · focused content"));

    let queue_focused = focused.with_focus(FocusRegion::Queue);
    let rendered = render_ui(140, 40, &queue_focused)?;
    assert!(!rendered.contains("Search · focused content"));
    assert!(rendered.contains("Search · content"));
    Ok(())
}

#[test]
fn tiny_layout_snapshot() -> Result<(), Box<dyn Error>> {
    assert_ui_snapshot(
        "tiny",
        40,
        10,
        &RenderModel::default()
            .with_view(NavigationItem::Search)
            .with_focus(FocusRegion::Content),
    )
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "the tiny-player acceptance scenario exercises every required telemetry concept together"
)]
fn tiny_player_keeps_identity_and_every_telemetry_concept_within_forty_cells()
-> Result<(), Box<dyn Error>> {
    let item = MediaItem {
        id: MediaId {
            provider: "youtube-music".to_owned(),
            video_id: "tiny-podcast".to_owned(),
        },
        kind: MediaKind::PodcastEpisode,
        title: "T界界界界界界界界界界界界界".to_owned(),
        creators: vec!["C測試測試測試測試測試測試".to_owned()],
        collection: Some("Tiny show".to_owned()),
        duration_ms: Some(180_000),
        artwork_url: None,
        explicit: false,
    };
    let mut state = AppState::new(Config {
        playback: PlaybackConfig {
            volume: 100,
            ..PlaybackConfig::default()
        },
        podcast: PodcastConfig {
            speed: 4.0,
            ..PodcastConfig::default()
        },
        ..Config::default()
    });
    let (next, _) = reduce(
        state,
        Action::SearchSubmitted {
            query: "tiny podcast".to_owned(),
            filter: SearchFilter::Episodes,
        },
    );
    state = next;
    let generation = state.search().generation();
    let (next, _) = reduce(
        state,
        Action::SearchCompleted {
            generation,
            result: Ok(SearchPage::new(vec![SearchItem::Playable(item)])),
        },
    );
    state = next;
    let (next, effects) = reduce(state, Action::ActivateSearchResult { index: 0 });
    state = next;
    let [Effect::LoadPodcastProgress { generation, .. }] = effects.as_slice() else {
        panic!("podcast activation must load persisted progress");
    };
    let progress_generation = *generation;
    (state, _) = reduce(
        state,
        Action::PodcastProgressLoaded {
            generation: progress_generation,
            progress: None,
        },
    );
    let Some(generation) = state.current_attempt_generation() else {
        panic!("loaded podcast progress must start playback");
    };
    (state, _) = reduce(
        state,
        Action::PlayerStatusChanged {
            generation,
            status: PlaybackStatus::Playing,
        },
    );
    let Some(media_id) = state.playback().current.clone() else {
        panic!("podcast must be current");
    };
    (state, _) = reduce(
        state,
        Action::PlayerProgress {
            generation,
            media_id,
            position_ms: 65_000,
            duration_ms: Some(180_000),
        },
    );
    (state, _) = reduce(
        state,
        Action::ResolvedFormatUpdated {
            generation,
            quality: ResolverQuality::new(Some("opus"), Some("251")),
        },
    );
    (state, _) = reduce(
        state,
        Action::PlaybackTelemetryUpdated {
            generation,
            effective_volume: 100.0,
            fade: Some(FadeActivity::Out),
        },
    );
    (state, _) = reduce(
        state,
        Action::ShuffleEnabledChanged {
            enabled: true,
            seed: 7,
        },
    );
    (state, _) = reduce(state, Action::RepeatModeChanged(RepeatMode::All));
    (state, _) = reduce(state, Action::RadioEnabledChanged(true));
    (state, _) = reduce(
        state,
        Action::PlayerStatusChanged {
            generation,
            status: PlaybackStatus::Failed,
        },
    );

    let mut terminal = Terminal::new(TestBackend::new(40, 10))?;
    terminal.draw(|frame| {
        render_with_model(
            frame,
            &state,
            &Theme::default(),
            &RenderModel::default().with_view(NavigationItem::Podcasts),
        );
    })?;
    let player = (0..40)
        .filter_map(|column| terminal.backend().buffer().cell((column, 9)))
        .map(Cell::symbol)
        .collect::<String>();
    let player = player.trim_end();

    for token in ["!", "T/C", "1:05/3:00", "v100/100↓", "SAE", "x4.0", "q2/o"] {
        assert!(player.contains(token), "missing `{token}` in `{player}`");
    }
    assert!(usize::from(player.cell_width()) <= 40);
    Ok(())
}

#[test]
fn help_overlay_snapshot_is_clamped_in_a_tiny_viewport() -> Result<(), Box<dyn Error>> {
    assert_ui_snapshot(
        "help",
        40,
        10,
        &RenderModel::default().with_overlay(Overlay::Help),
    )
}

#[test]
fn help_overlay_lists_transport_and_seek_bindings() -> Result<(), Box<dyn Error>> {
    let rendered = render_ui(80, 20, &RenderModel::default().with_overlay(Overlay::Help))?;

    for binding in [
        "Space · F8 · Media Play/Pause",
        "n/F9/Media Next",
        "p/F7/Media Previous",
        "Shift+Left back · Shift+Right forward",
    ] {
        assert!(
            rendered.contains(binding),
            "missing `{binding}`:\n{rendered}"
        );
    }
    Ok(())
}

#[test]
fn command_palette_snapshot_is_clamped_in_a_tiny_viewport() -> Result<(), Box<dyn Error>> {
    assert_ui_snapshot(
        "palette",
        40,
        10,
        &RenderModel::default().with_overlay(Overlay::CommandPalette),
    )
}

#[test]
fn command_palette_filters_and_reports_the_selected_semantic_action() -> Result<(), Box<dyn Error>>
{
    let model = RenderModel::default()
        .with_overlay(Overlay::CommandPalette)
        .with_palette_query("volume")
        .with_palette_selection(1);

    assert_eq!(
        model.palette.selected_action(),
        Some(SemanticAction::VolumeDown)
    );
    let viewport = model.palette.viewport(1);
    assert_eq!(viewport.total, 2);
    assert_eq!(viewport.start, 1);
    assert_eq!(viewport.entries.len(), 1);
    assert_eq!(viewport.entries[0].action, SemanticAction::VolumeDown);

    let rendered = render_ui(40, 10, &model)?;
    assert!(rendered.contains("Query: volume"));
    assert!(rendered.contains("▶ Volume down"));
    assert!(!rendered.contains("Quit"));
    Ok(())
}

#[test]
fn palette_query_is_bounded_before_storage_matching_and_rendering() -> Result<(), Box<dyn Error>> {
    let huge_query = "q".repeat(2 * 1024 * 1024);
    let model = RenderModel::default()
        .with_overlay(Overlay::CommandPalette)
        .with_palette_query(huge_query.as_str());
    let stored = model.palette.query();

    assert!(stored.len() <= CLIP_BYTE_INSPECTION_BUDGET);
    assert!(
        stored.graphemes(true).count() <= CLIP_GRAPHEME_INSPECTION_BUDGET,
        "palette matching must never lowercase an unbounded query"
    );
    assert_eq!(stored, &huge_query[..stored.len()]);

    let rendered = render_ui(40, 10, &model)?;
    assert!(
        rendered.len() < 10_000,
        "a fixed terminal must not retain the hidden query suffix"
    );

    let deceptive_query = format!(
        "volume{}x",
        " ".repeat(CLIP_BYTE_INSPECTION_BUDGET.saturating_sub("volume".len()))
    );
    let deceptive_model = RenderModel::default().with_palette_query(&deceptive_query);
    assert!(deceptive_model.palette.query().len() < deceptive_query.len());
    assert_eq!(
        deceptive_model.palette.selected_action(),
        None,
        "a hidden suffix must not turn a non-match into an executable prefix"
    );
    assert_eq!(deceptive_model.palette.viewport(10).total, 0);
    Ok(())
}

#[test]
fn every_palette_action_can_be_selected_and_revealed_in_a_bounded_overlay()
-> Result<(), Box<dyn Error>> {
    for (index, entry) in palette_entries().iter().enumerate() {
        let model = RenderModel::default()
            .with_overlay(Overlay::CommandPalette)
            .with_palette_selection(index);
        assert_eq!(model.palette.selected_action(), Some(entry.action));

        let rendered = render_ui(40, 10, &model)?;
        assert!(
            rendered.contains(&format!("▶ {}", entry.label)),
            "selected action {entry:?} must be scrolled into view"
        );
        assert_eq!(rendered.lines().count(), 10);
    }
    Ok(())
}

#[test]
fn palette_viewport_clamps_scroll_and_keeps_selection_visible() {
    let model = RenderModel::default()
        .with_palette_selection(15)
        .with_palette_scroll(3);
    let viewport = model.palette.viewport(4);

    assert_eq!(viewport.start, 12);
    assert_eq!(viewport.entries.len(), 4);
    assert_eq!(viewport.selected, Some(15));
    assert!(viewport.start <= 15);
    assert!(15 < viewport.start + viewport.entries.len());
}

#[test]
fn primary_renderer_and_overlays_do_not_panic_at_zero_or_tiny_areas() -> Result<(), Box<dyn Error>>
{
    for (width, height) in [
        (0, 0),
        (0, 1),
        (1, 0),
        (1, 1),
        (2, 2),
        (7, 3),
        (15, 4),
        (40, 10),
    ] {
        let state = fixed_state();
        let theme = Theme::for_capability(ColorCapability::Monochrome);
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend)?;

        terminal.draw(|frame| render(frame, &state, &theme))?;

        for overlay in [Overlay::Help, Overlay::CommandPalette] {
            terminal.draw(|frame| {
                render_with_model(
                    frame,
                    &state,
                    &theme,
                    &RenderModel::default().with_overlay(overlay),
                );
            })?;
        }
    }
    Ok(())
}

#[test]
fn known_two_by_two_rgba_image_maps_to_one_half_block_row() -> Result<(), Box<dyn Error>> {
    let bytes = png_bytes(
        2,
        2,
        &[
            Rgba([255, 0, 0, 255]),
            Rgba([0, 255, 0, 255]),
            Rgba([0, 0, 255, 255]),
            Rgba([255, 255, 0, 255]),
        ],
    );

    let grid = decode_artwork(&bytes, CellSize::new(2, 1))?;

    assert_eq!(grid.width(), 2);
    assert_eq!(grid.height(), 1);
    assert_eq!(grid.cells().len(), 2);
    assert_eq!(grid.cell(0, 0).copied().map(ArtworkCell::glyph), Some('▀'));
    assert_eq!(
        grid.cell(0, 0).copied().map(ArtworkCell::foreground),
        Some(Rgb::new(255, 0, 0))
    );
    assert_eq!(
        grid.cell(0, 0).copied().map(ArtworkCell::background),
        Some(Rgb::new(0, 0, 255))
    );
    assert_eq!(
        grid.cell(1, 0).copied().map(ArtworkCell::foreground),
        Some(Rgb::new(0, 255, 0))
    );
    assert_eq!(
        grid.cell(1, 0).copied().map(ArtworkCell::background),
        Some(Rgb::new(255, 255, 0))
    );
    Ok(())
}

#[test]
fn pure_artwork_widget_renders_exact_half_block_colors() -> Result<(), Box<dyn Error>> {
    let bytes = png_bytes(
        2,
        2,
        &[
            Rgba([255, 0, 0, 255]),
            Rgba([0, 255, 0, 255]),
            Rgba([0, 0, 255, 255]),
            Rgba([255, 255, 0, 255]),
        ],
    );
    let presentation =
        ArtworkPresentation::Grid(Arc::new(decode_artwork(&bytes, CellSize::new(2, 1))?));
    let mut terminal = Terminal::new(TestBackend::new(2, 1))?;

    terminal.draw(|frame| {
        render_artwork(
            frame,
            frame.area(),
            &presentation,
            ColorCapability::TrueColor,
        );
    })?;

    let left = terminal
        .backend()
        .buffer()
        .cell((0, 0))
        .ok_or_else(|| std::io::Error::other("missing left artwork cell"))?;
    let right = terminal
        .backend()
        .buffer()
        .cell((1, 0))
        .ok_or_else(|| std::io::Error::other("missing right artwork cell"))?;
    assert_eq!(
        (left.symbol(), left.fg, left.bg),
        ("▀", Color::Rgb(255, 0, 0), Color::Rgb(0, 0, 255))
    );
    assert_eq!(
        (right.symbol(), right.fg, right.bg),
        ("▀", Color::Rgb(0, 255, 0), Color::Rgb(255, 255, 0))
    );
    Ok(())
}

#[test]
fn pure_artwork_widget_renders_fallback_icon_and_metadata() -> Result<(), Box<dyn Error>> {
    let presentation = ArtworkPresentation::unavailable();
    let mut terminal = Terminal::new(TestBackend::new(24, 1))?;

    terminal.draw(|frame| {
        render_artwork(frame, frame.area(), &presentation, ColorCapability::Basic);
    })?;

    assert!(
        terminal
            .backend()
            .to_string()
            .contains("♪ Artwork unavailable")
    );
    Ok(())
}

#[test]
fn wide_runtime_artwork_panel_renders_pixels_and_fallback_without_covering_player()
-> Result<(), Box<dyn Error>> {
    let grid = ArtworkPresentation::Grid(Arc::new(decode_artwork(
        &png_bytes(
            2,
            2,
            &[
                Rgba([255, 0, 0, 255]),
                Rgba([0, 255, 0, 255]),
                Rgba([0, 0, 255, 255]),
                Rgba([255, 255, 0, 255]),
            ],
        ),
        CellSize::new(2, 1),
    )?));
    let mut terminal = Terminal::new(TestBackend::new(120, 32))?;
    terminal.draw(|frame| {
        render_with_model_and_artwork(
            frame,
            &AppState::default(),
            &Theme::default(),
            &RenderModel::default(),
            &grid,
        );
    })?;
    let buffer = terminal.backend().buffer();
    assert!((0..buffer.area.height).any(|y| {
        (0..buffer.area.width).any(|x| {
            buffer.cell((x, y)).is_some_and(|cell| {
                cell.symbol() == "▀"
                    && cell.fg == Color::Rgb(255, 0, 0)
                    && cell.bg == Color::Rgb(0, 0, 255)
            })
        })
    }));
    assert!(
        terminal
            .backend()
            .to_string()
            .contains("Player · persistent")
    );

    terminal.draw(|frame| {
        render_with_model_and_artwork(
            frame,
            &AppState::default(),
            &Theme::default(),
            &RenderModel::default(),
            &ArtworkPresentation::unavailable(),
        );
    })?;
    let rendered = terminal.backend().to_string();
    assert!(rendered.contains("♪ Artwork unavailable"));
    assert!(rendered.contains("Player · persistent"));
    Ok(())
}

#[test]
fn runtime_artwork_panel_uses_the_theme_color_capability() -> Result<(), Box<dyn Error>> {
    let grid = ArtworkPresentation::Grid(Arc::new(decode_artwork(
        &png_bytes(1, 2, &[Rgba([255, 0, 0, 255]), Rgba([0, 0, 255, 255])]),
        CellSize::new(1, 1),
    )?));
    let mut terminal = Terminal::new(TestBackend::new(120, 32))?;

    terminal.draw(|frame| {
        render_with_model_and_artwork(
            frame,
            &AppState::default(),
            &Theme::for_capability(ColorCapability::Monochrome),
            &RenderModel::default(),
            &grid,
        );
    })?;

    let buffer = terminal.backend().buffer();
    assert!((0..buffer.area.height).all(|y| {
        (0..buffer.area.width).all(|x| {
            buffer.cell((x, y)).is_none_or(|cell| {
                !matches!(cell.fg, Color::Rgb(..)) && !matches!(cell.bg, Color::Rgb(..))
            })
        })
    }));
    assert!(
        terminal
            .backend()
            .to_string()
            .contains("♪ Artwork unavailable")
    );
    Ok(())
}

#[test]
fn artwork_widget_clips_and_handles_zero_or_tiny_areas_without_panicking()
-> Result<(), Box<dyn Error>> {
    let grid = ArtworkPresentation::Grid(Arc::new(decode_artwork(
        &png_bytes(
            2,
            2,
            &[
                Rgba([1, 2, 3, 255]),
                Rgba([4, 5, 6, 255]),
                Rgba([7, 8, 9, 255]),
                Rgba([10, 11, 12, 255]),
            ],
        ),
        CellSize::new(2, 1),
    )?));
    let fallback = ArtworkPresentation::unavailable();

    for (width, height) in [(0, 0), (0, 1), (1, 0), (1, 1), (2, 1)] {
        for presentation in [&grid, &fallback] {
            let mut terminal = Terminal::new(TestBackend::new(width, height))?;
            terminal.draw(|frame| {
                render_artwork(
                    frame,
                    frame.area(),
                    presentation,
                    ColorCapability::TrueColor,
                );
            })?;
        }
    }
    Ok(())
}

#[test]
fn artwork_grid_resets_prestyled_cells_and_clips_from_the_original_area()
-> Result<(), Box<dyn Error>> {
    let presentation = ArtworkPresentation::Grid(Arc::new(decode_artwork(
        &png_bytes(
            2,
            2,
            &[
                Rgba([255, 0, 0, 255]),
                Rgba([0, 255, 0, 255]),
                Rgba([0, 0, 255, 255]),
                Rgba([255, 255, 0, 255]),
            ],
        ),
        CellSize::new(2, 1),
    )?));
    let mut buffer = prefilled_buffer(Rect::new(5, 3, 3, 1));

    ArtworkWidget::new(&presentation, ColorCapability::TrueColor)
        .render(Rect::new(4, 3, 4, 1), &mut buffer);

    assert_exact_cell(
        &buffer,
        (5, 3),
        "▀",
        Color::Rgb(0, 255, 0),
        Color::Rgb(255, 255, 0),
    );
    for x in [6, 7] {
        assert_exact_cell(&buffer, (x, 3), " ", Color::Reset, Color::Reset);
    }
    Ok(())
}

#[test]
fn fitted_artwork_preserves_endpoints_and_uses_lower_center_for_one_cell()
-> Result<(), Box<dyn Error>> {
    let presentation = ArtworkPresentation::Grid(Arc::new(decode_artwork(
        &png_bytes(
            4,
            2,
            &[
                Rgba([255, 0, 0, 255]),
                Rgba([0, 255, 0, 255]),
                Rgba([0, 0, 255, 255]),
                Rgba([255, 255, 0, 255]),
                Rgba([255, 0, 0, 255]),
                Rgba([0, 255, 0, 255]),
                Rgba([0, 0, 255, 255]),
                Rgba([255, 255, 0, 255]),
            ],
        ),
        CellSize::new(4, 1),
    )?));
    let mut buffer = Buffer::empty(Rect::new(0, 0, 2, 1));

    ArtworkWidget::new_fitted(&presentation, ColorCapability::TrueColor)
        .render(Rect::new(0, 0, 2, 1), &mut buffer);

    assert_exact_cell(
        &buffer,
        (0, 0),
        "▀",
        Color::Rgb(255, 0, 0),
        Color::Rgb(255, 0, 0),
    );
    assert_exact_cell(
        &buffer,
        (1, 0),
        "▀",
        Color::Rgb(255, 255, 0),
        Color::Rgb(255, 255, 0),
    );

    let mut one_cell = Buffer::empty(Rect::new(0, 0, 1, 1));
    ArtworkWidget::new_fitted(&presentation, ColorCapability::TrueColor)
        .render(Rect::new(0, 0, 1, 1), &mut one_cell);
    assert_exact_cell(
        &one_cell,
        (0, 0),
        "▀",
        Color::Rgb(0, 255, 0),
        Color::Rgb(0, 255, 0),
    );
    Ok(())
}

#[test]
fn fitted_production_grid_preserves_all_four_edges() -> Result<(), Box<dyn Error>> {
    let mut pixels = vec![Rgba([0, 0, 0, 255]); 21 * 16];
    for (x, y, color) in [
        (0, 0, Rgba([255, 0, 0, 255])),
        (20, 0, Rgba([0, 255, 0, 255])),
        (0, 14, Rgba([0, 0, 255, 255])),
        (20, 14, Rgba([255, 255, 0, 255])),
    ] {
        pixels[y * 21 + x] = color;
        pixels[(y + 1) * 21 + x] = color;
    }
    let presentation = ArtworkPresentation::Grid(Arc::new(decode_artwork(
        &png_bytes(21, 16, &pixels),
        CellSize::new(21, 8),
    )?));
    let mut buffer = Buffer::empty(Rect::new(0, 0, 12, 5));

    ArtworkWidget::new_fitted(&presentation, ColorCapability::TrueColor)
        .render(Rect::new(0, 0, 12, 5), &mut buffer);

    for (position, expected) in [
        ((0, 0), Color::Rgb(255, 0, 0)),
        ((11, 0), Color::Rgb(0, 255, 0)),
        ((0, 4), Color::Rgb(0, 0, 255)),
        ((11, 4), Color::Rgb(255, 255, 0)),
    ] {
        let cell = buffer.cell(position).ok_or("fitted corner cell")?;
        assert_eq!((cell.fg, cell.bg), (expected, expected), "{position:?}");
    }
    Ok(())
}

#[test]
fn fitted_artwork_handles_empty_grids_and_zero_target_bounds() -> Result<(), Box<dyn Error>> {
    let empty = ArtworkPresentation::Grid(Arc::new(decode_artwork(&[], CellSize::new(0, 0))?));
    let mut buffer = prefilled_buffer(Rect::new(0, 0, 2, 2));

    ArtworkWidget::new_fitted(&empty, ColorCapability::TrueColor)
        .render(Rect::new(0, 0, 2, 2), &mut buffer);
    ArtworkWidget::new_fitted(&empty, ColorCapability::TrueColor)
        .render(Rect::new(0, 0, 0, 2), &mut buffer);
    ArtworkWidget::new_fitted(&empty, ColorCapability::TrueColor)
        .render(Rect::new(0, 0, 2, 0), &mut buffer);

    for y in 0..2 {
        for x in 0..2 {
            assert_exact_cell(&buffer, (x, y), " ", Color::Reset, Color::Reset);
        }
    }
    Ok(())
}

#[test]
fn artwork_fallback_resets_and_renders_exactly_through_a_clipped_area() {
    let presentation = ArtworkPresentation::unavailable();
    let mut buffer = prefilled_buffer(Rect::new(10, 7, 5, 1));

    ArtworkWidget::new(&presentation, ColorCapability::Basic)
        .render(Rect::new(9, 7, 4, 1), &mut buffer);

    assert_exact_cell(&buffer, (10, 7), " ", Color::Reset, Color::Reset);
    assert_exact_cell(&buffer, (11, 7), "A", Color::Reset, Color::Reset);
    assert_exact_cell(&buffer, (12, 7), "r", Color::Reset, Color::Reset);
    let Some(outside) = buffer.cell((13, 7)) else {
        panic!("missing cell outside the clipped widget");
    };
    assert_eq!(
        (outside.symbol(), outside.fg, outside.bg, outside.modifier),
        (
            "X",
            Color::White,
            Color::Magenta,
            Modifier::BOLD | Modifier::DIM | Modifier::REVERSED,
        ),
    );
}

#[test]
fn alpha_is_composited_deterministically_and_odd_sources_fill_exact_cell_dimensions()
-> Result<(), Box<dyn Error>> {
    let bytes = png_bytes(
        1,
        3,
        &[
            Rgba([200, 100, 50, 128]),
            Rgba([10, 20, 30, 0]),
            Rgba([1, 2, 3, 255]),
        ],
    );

    let grid = decode_artwork(&bytes, CellSize::new(1, 2))?;

    assert_eq!((grid.width(), grid.height(), grid.cells().len()), (1, 2, 2));
    assert_eq!(
        grid.cell(0, 0).copied().map(ArtworkCell::foreground),
        Some(Rgb::new(100, 50, 25))
    );
    assert_eq!(
        grid.cell(0, 0).copied().map(ArtworkCell::background),
        Some(Rgb::new(0, 0, 0))
    );
    assert_eq!(
        grid.cell(0, 1).copied().map(ArtworkCell::background),
        Some(Rgb::new(1, 2, 3))
    );
    Ok(())
}

#[test]
fn malformed_and_resource_exhausting_artwork_is_rejected_without_panicking()
-> Result<(), Box<dyn Error>> {
    assert!(decode_artwork(b"not an image", CellSize::new(2, 2)).is_err());
    assert!(decode_artwork(&[], CellSize::new(2, 2)).is_err());
    let oversized = vec![0; 4 * 1024 * 1024 + 1];
    assert!(decode_artwork(&oversized, CellSize::new(2, 2)).is_err());

    let valid = png_bytes(1, 1, &[Rgba([1, 2, 3, 255])]);
    assert!(decode_artwork(&valid, CellSize::new(u16::MAX, u16::MAX)).is_err());

    let empty = decode_artwork(&valid, CellSize::new(0, 7))?;
    assert_eq!(
        (empty.width(), empty.height(), empty.cells().len()),
        (0, 7, 0)
    );
    Ok(())
}

#[test]
fn artwork_identity_and_cache_use_full_urls_hide_secrets_key_dimensions_and_evict()
-> Result<(), Box<dyn Error>> {
    let first = Url::parse(
        "https://lh3.googleusercontent.com/art/cover.jpg?id=one&token=SECRET_ONE#PRIVATE_FRAGMENT",
    )?;
    let exact_same = first.clone();
    let semantically_distinct =
        Url::parse("https://lh3.googleusercontent.com/art/cover.jpg?id=two&token=SECRET_TWO")?;
    let second = Url::parse("https://lh3.googleusercontent.com/art/second.jpg?sig=PRIVATE")?;
    let third = Url::parse("https://lh3.googleusercontent.com/art/third.jpg?sig=PRIVATE")?;

    let identity = ArtworkIdentity::from_url(&first);
    assert_eq!(identity, ArtworkIdentity::from_url(&exact_same));
    assert_ne!(
        identity,
        ArtworkIdentity::from_url(&semantically_distinct),
        "different query semantics must not collide"
    );
    let debug = format!("{identity:?}");
    let distinct_debug = format!("{:?}", ArtworkIdentity::from_url(&semantically_distinct));
    assert_eq!(debug, "ArtworkIdentity([REDACTED])");
    assert_eq!(
        debug, distinct_debug,
        "debug output must not distinguish secret artwork URLs"
    );
    assert!(!debug.contains("SECRET"));
    assert!(!debug.contains("token="));
    assert!(!debug.contains("cover.jpg"));
    assert!(!debug.contains("PRIVATE_FRAGMENT"));

    let grid = Arc::new(decode_artwork(
        &png_bytes(1, 1, &[Rgba([1, 2, 3, 255])]),
        CellSize::new(1, 1),
    )?);
    let mut cache = ArtworkCache::new(2);
    cache.insert(identity.clone(), CellSize::new(1, 1), Arc::clone(&grid));
    assert!(cache.get(&identity, CellSize::new(1, 1)).is_some());
    assert!(
        cache
            .get(
                &ArtworkIdentity::from_url(&semantically_distinct),
                CellSize::new(1, 1)
            )
            .is_none()
    );
    assert!(cache.get(&identity, CellSize::new(2, 1)).is_none());

    cache.insert(
        ArtworkIdentity::from_url(&second),
        CellSize::new(1, 1),
        Arc::clone(&grid),
    );
    cache.insert(
        ArtworkIdentity::from_url(&third),
        CellSize::new(1, 1),
        Arc::clone(&grid),
    );
    assert_eq!(cache.len(), 2);
    assert!(cache.get(&identity, CellSize::new(1, 1)).is_none());

    let cache_debug = format!("{cache:?}");
    assert!(!cache_debug.contains("PRIVATE"));
    assert!(!cache_debug.contains("googleusercontent"));
    Ok(())
}

#[tokio::test]
async fn injected_artwork_service_caches_fetches_and_returns_safe_fallbacks()
-> Result<(), Box<dyn Error>> {
    let image = png_bytes(1, 2, &[Rgba([8, 9, 10, 255]), Rgba([11, 12, 13, 255])]);
    let calls = Arc::new(AtomicUsize::new(0));
    let fetcher = FakeFetcher {
        bytes: Some(image),
        calls: Arc::clone(&calls),
    };
    let mut service = CachedArtworkService::new(fetcher, 2);
    let url = Url::parse("https://example.invalid/cover?signature=DO_NOT_EXPOSE")?;
    let size = CellSize::new(1, 1);

    assert!(matches!(
        service.load(&url, size, ColorCapability::TrueColor).await,
        ArtworkPresentation::Grid(_)
    ));
    assert!(matches!(
        service.load(&url, size, ColorCapability::TrueColor).await,
        ArtworkPresentation::Grid(_)
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let fallback = service.load(&url, size, ColorCapability::Monochrome).await;
    let ArtworkPresentation::Fallback(fallback) = fallback else {
        panic!("monochrome terminals should use a deterministic fallback");
    };
    assert_eq!(fallback.icon(), "♪");
    assert_eq!(fallback.metadata(), "Artwork unavailable");
    assert!(!format!("{fallback:?}").contains("DO_NOT_EXPOSE"));
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let failing_calls = Arc::new(AtomicUsize::new(0));
    let mut failing = CachedArtworkService::new(
        FakeFetcher {
            bytes: None,
            calls: Arc::clone(&failing_calls),
        },
        2,
    );
    assert!(matches!(
        failing.load(&url, size, ColorCapability::TrueColor).await,
        ArtworkPresentation::Fallback(_)
    ));
    assert_eq!(failing_calls.load(Ordering::SeqCst), 1);
    assert!(!format!("{failing:?}").contains("DO_NOT_EXPOSE"));
    Ok(())
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn plain(character: char) -> KeyEvent {
    key(KeyCode::Char(character))
}

fn shifted(character: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(character), KeyModifiers::SHIFT)
}

fn playback_state(
    kind: MediaKind,
    status: PlaybackStatus,
    position_ms: u64,
    duration_ms: Option<u64>,
    config: Config,
) -> AppState {
    let item = MediaItem {
        id: MediaId {
            provider: "fixture-provider".to_owned(),
            video_id: "player-controls-fixture".to_owned(),
        },
        kind,
        title: "Player controls fixture".to_owned(),
        creators: vec!["Fixture creator".to_owned()],
        collection: None,
        duration_ms,
        artwork_url: None,
        explicit: false,
    };
    let filter = if kind == MediaKind::PodcastEpisode {
        SearchFilter::Episodes
    } else {
        SearchFilter::Songs
    };
    let (mut state, _) = reduce(
        AppState::new(config),
        Action::SearchSubmitted {
            query: "player controls".to_owned(),
            filter,
        },
    );
    let generation = state.search().generation();
    (state, _) = reduce(
        state,
        Action::SearchCompleted {
            generation,
            result: Ok(SearchPage::new(vec![SearchItem::Playable(item)])),
        },
    );
    let effects;
    (state, effects) = reduce(state, Action::ActivateSearchResult { index: 0 });
    if kind == MediaKind::PodcastEpisode {
        let [Effect::LoadPodcastProgress { generation, .. }] = effects.as_slice() else {
            panic!("podcast fixture must request persisted progress");
        };
        (state, _) = reduce(
            state,
            Action::PodcastProgressLoaded {
                generation: *generation,
                progress: None,
            },
        );
    }
    let Some(generation) = state.current_attempt_generation() else {
        panic!("fixture activation must create a playback attempt");
    };
    (state, _) = reduce(state, Action::PlayerStatusChanged { generation, status });
    let Some(media_id) = state.playback().current.clone() else {
        panic!("fixture must be current");
    };
    reduce(
        state,
        Action::PlayerProgress {
            generation,
            media_id,
            position_ms,
            duration_ms,
        },
    )
    .0
}

fn with_quality(state: AppState, codec: &str, format_id: &str) -> AppState {
    let Some(generation) = state.current_attempt_generation() else {
        panic!("quality fixture must retain a playback attempt");
    };
    reduce(
        state,
        Action::ResolvedFormatUpdated {
            generation,
            quality: ResolverQuality::new(Some(codec), Some(format_id)),
        },
    )
    .0
}

fn fixed_state() -> AppState {
    let item = MediaItem {
        id: MediaId {
            provider: "youtube-music".to_owned(),
            video_id: "fixture-song".to_owned(),
        },
        kind: MediaKind::Song,
        title: "月明かり e\u{301}cho 👩‍💻 — a title long enough to test cell-safe clipping"
            .to_owned(),
        creators: vec!["Terminal Ensemble".to_owned(), "測試 Artist".to_owned()],
        collection: Some("Midnight Sessions".to_owned()),
        duration_ms: Some(245_000),
        artwork_url: None,
        explicit: false,
    };
    let (state, _) = reduce(
        AppState::default(),
        Action::SearchSubmitted {
            query: "night coding".to_owned(),
            filter: SearchFilter::Songs,
        },
    );
    let generation = state.search().generation();
    let (state, _) = reduce(
        state,
        Action::SearchCompleted {
            generation,
            result: Ok(SearchPage::new(vec![SearchItem::Playable(item)])),
        },
    );
    let (state, _) = reduce(state, Action::ActivateSearchResult { index: 0 });
    let Some(generation) = state.current_attempt_generation() else {
        panic!("activation must create a playback attempt");
    };
    let (state, _) = reduce(
        state,
        Action::PlayerStatusChanged {
            generation,
            status: PlaybackStatus::Playing,
        },
    );
    let Some(media_id) = state.playback().current.clone() else {
        panic!("activated fixture item must be current");
    };
    let (state, _) = reduce(
        state,
        Action::PlayerProgress {
            generation,
            media_id,
            position_ms: 73_000,
            duration_ms: Some(245_000),
        },
    );
    reduce(
        state,
        Action::PlaybackTelemetryUpdated {
            generation,
            effective_volume: 80.0,
            fade: None,
        },
    )
    .0
}

fn assert_ui_snapshot(
    name: &str,
    width: u16,
    height: u16,
    model: &RenderModel,
) -> Result<(), Box<dyn Error>> {
    let state = fixed_state();
    let theme = Theme::for_capability(ColorCapability::TrueColor);
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend)?;
    terminal.draw(|frame| render_with_model(frame, &state, &theme, model))?;

    insta::assert_snapshot!(name, terminal.backend().to_string());
    Ok(())
}

fn render_ui(width: u16, height: u16, model: &RenderModel) -> Result<String, Box<dyn Error>> {
    let state = fixed_state();
    let theme = Theme::for_capability(ColorCapability::TrueColor);
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend)?;
    terminal.draw(|frame| render_with_model(frame, &state, &theme, model))?;
    Ok(terminal.backend().to_string())
}

fn render_state(width: u16, height: u16, state: &AppState) -> Result<String, Box<dyn Error>> {
    let theme = Theme::for_capability(ColorCapability::TrueColor);
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend)?;
    terminal.draw(|frame| render_with_model(frame, state, &theme, &RenderModel::default()))?;
    Ok(terminal.backend().to_string())
}

fn prefilled_buffer(area: Rect) -> Buffer {
    let mut buffer = Buffer::empty(area);
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            if let Some(cell) = buffer.cell_mut((x, y)) {
                cell.set_symbol("X").set_style(
                    Style::default()
                        .fg(Color::White)
                        .bg(Color::Magenta)
                        .add_modifier(Modifier::BOLD | Modifier::DIM | Modifier::REVERSED),
                );
            }
        }
    }
    buffer
}

fn assert_exact_cell(
    buffer: &Buffer,
    position: (u16, u16),
    symbol: &str,
    foreground: Color,
    background: Color,
) {
    let Some(cell) = buffer.cell(position) else {
        panic!("missing test cell at {position:?}");
    };
    assert_eq!(cell.symbol(), symbol, "{position:?}");
    assert_eq!(cell.fg, foreground, "{position:?}");
    assert_eq!(cell.bg, background, "{position:?}");
    assert_eq!(cell.modifier, Modifier::empty(), "{position:?}");
}

fn png_bytes(width: u32, height: u32, pixels: &[Rgba<u8>]) -> Vec<u8> {
    let Some(image) = RgbaImage::from_raw(
        width,
        height,
        pixels.iter().flat_map(|pixel| pixel.0).collect::<Vec<_>>(),
    ) else {
        panic!("pixel count must match dimensions");
    };
    let mut bytes = Cursor::new(Vec::new());
    if let Err(error) = DynamicImage::ImageRgba8(image).write_to(&mut bytes, ImageFormat::Png) {
        panic!("in-memory PNG encoding failed: {error}");
    }
    bytes.into_inner()
}

struct FakeFetcher {
    bytes: Option<Vec<u8>>,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl ArtworkFetcher for FakeFetcher {
    async fn fetch(&self, _url: &Url) -> Result<ArtworkByteStream, ArtworkFetchError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let bytes = self
            .bytes
            .clone()
            .ok_or_else(ArtworkFetchError::unavailable)?;
        Ok(Box::pin(stream::iter([Ok(Bytes::from(bytes))])))
    }
}
