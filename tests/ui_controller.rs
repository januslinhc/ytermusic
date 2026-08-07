use std::error::Error;

use crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MediaKeyCode, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::{Terminal, backend::TestBackend, layout::Rect};
use ytermusic::{
    app::{
        Action, AppState, ArtworkSurface, Effect, PlayerCommand, SearchFilter, SearchItem,
        SearchPage, reduce, stable_queue_item_id,
    },
    auth::Browser,
    config::Config,
    domain::{ArtworkUrl, ChartSection, MediaId, MediaItem, MediaKind, RegionCode, RepeatMode},
    lyrics::{LyricsDocument, LyricsSource, TimedLyricLine},
    podcast_rankings::{PodcastRecommendationPage, parse_apple_top_shows},
    provider::{
        AuthenticationState, LibraryItem, LibrarySection, MAX_ITEMS_PER_SHELF, Page, Podcast,
    },
    queue::{MAX_EXPLICIT_LIST_ITEMS, QueueItemId},
    storage::{FavoriteEntry, HistoryEntry},
    ui::{
        controller::{UiController, reduce_key, reduce_mouse},
        input::{InputMode, SemanticAction, TextEntryContext},
        interaction::{HitTarget, InteractionSnapshot, InteractionStore, ListSurface},
        layout::LayoutMode,
        motion::{MotionFrame, ProgressPresentation},
        render::{
            FocusRegion, NavigationItem, Overlay, RenderModel, render_with_model,
            render_with_model_and_interactions,
        },
        theme::Theme,
    },
};

#[test]
fn ui_motion_render_model_carries_an_exact_transient_frame() {
    let frame = MotionFrame {
        elapsed_ms: 987,
        spinner_index: 7,
        progress: ProgressPresentation {
            fraction: 0.625,
            shimmer_phase: 0.375,
        },
    };
    let model = RenderModel::default().with_motion_frame(frame);

    assert_eq!(model.motion_frame(), frame);
}

type TestResult = Result<(), Box<dyn Error>>;

fn plain(character: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE)
}

fn shifted(character: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(character), KeyModifiers::SHIFT)
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn artwork_surface(view: NavigationItem) -> ArtworkSurface {
    match view {
        NavigationItem::Home => ArtworkSurface::Home,
        NavigationItem::Search => ArtworkSurface::Search,
        NavigationItem::Charts => ArtworkSurface::Charts,
        NavigationItem::Podcasts => ArtworkSurface::Podcasts,
        NavigationItem::Library => ArtworkSurface::Library,
        NavigationItem::Favorites => ArtworkSurface::Favorites,
        NavigationItem::History => ArtworkSurface::History,
        NavigationItem::Settings => ArtworkSurface::Settings,
    }
}

fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
    MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    }
}

fn interaction_store(target: HitTarget) -> InteractionStore {
    let mut store = InteractionStore::default();
    let Some(mut map) = store.begin_frame() else {
        panic!("interaction frame");
    };
    assert!(map.push(Rect::new(3, 4, 1, 1), target));
    assert!(store.publish(map));
    store
}

fn rendered_interaction_store(
    state: &AppState,
    controller: &UiController,
    width: u16,
    height: u16,
) -> Result<InteractionStore, Box<dyn Error>> {
    let mut store = InteractionStore::default();
    let Some(mut map) = store.begin_frame() else {
        panic!("interaction frame");
    };
    let mut terminal = Terminal::new(TestBackend::new(width, height))?;
    terminal.draw(|frame| {
        render_with_model_and_interactions(
            frame,
            state,
            &Theme::default(),
            controller.model(),
            &mut map,
        );
    })?;
    assert!(store.publish(map));
    Ok(store)
}

fn target_coordinate(
    snapshot: &InteractionSnapshot,
    target: HitTarget,
    width: u16,
    height: u16,
) -> (u16, u16) {
    for row in 0..height {
        for column in 0..width {
            if snapshot.resolve(column, row) == Some(target) {
                return (column, row);
            }
        }
    }
    panic!("rendered target missing: {target:?}");
}

#[test]
fn mouse_navigation_click_uses_navigation_activation_and_left_down_only() {
    let state = AppState::default();
    let store = interaction_store(HitTarget::Navigation(NavigationItem::Search));
    let Some(snapshot) = store.latest() else {
        panic!("interaction snapshot");
    };
    let controller = UiController::default().with_focus(FocusRegion::Player);

    let (controller, actions) = reduce_mouse(
        controller,
        &state,
        mouse(MouseEventKind::Down(MouseButton::Left), 3, 4),
        Some(snapshot),
    );
    assert_eq!(controller.model().view, NavigationItem::Search);
    assert_eq!(controller.model().focus, FocusRegion::Navigation);
    assert_eq!(
        actions,
        vec![Action::ArtworkSurfaceChanged {
            surface: ArtworkSurface::Search,
        }]
    );

    let (controller, actions) = reduce_mouse(
        controller,
        &state,
        mouse(MouseEventKind::Up(MouseButton::Left), 3, 4),
        Some(snapshot),
    );
    assert_eq!(controller.model().view, NavigationItem::Search);
    assert!(actions.is_empty(), "mouse up must not double-dispatch");
}

#[test]
fn mouse_navigation_destinations_preserve_keyboard_lazy_load_actions() {
    let state = AppState::default();
    for destination in NavigationItem::ALL {
        let store = interaction_store(HitTarget::Navigation(destination));
        let (clicked, click_actions) = reduce_mouse(
            UiController::default(),
            &state,
            mouse(MouseEventKind::Down(MouseButton::Left), 3, 4),
            store.latest(),
        );
        let (_, keyboard_actions) = reduce_key(
            UiController::default()
                .with_view(destination)
                .with_focus(FocusRegion::Navigation),
            &state,
            key(KeyCode::Enter),
        );
        assert_eq!(clicked.model().view, destination);
        assert_eq!(clicked.model().focus, FocusRegion::Navigation);
        assert_eq!(click_actions, keyboard_actions, "{destination:?}");
    }
}

#[test]
fn mouse_navigation_exits_hidden_search_text_entry_before_global_input() {
    let state = AppState::default();
    let (mut controller, _) = reduce_key(UiController::default(), &state, plain('/'));
    for character in "hidden draft".chars() {
        (controller, _) = reduce_key(controller, &state, plain(character));
    }
    assert_eq!(
        controller.input_mode(),
        InputMode::TextEntry(TextEntryContext::Search)
    );
    assert_eq!(controller.input_text(), "hidden draft");

    for destination in [NavigationItem::Charts, NavigationItem::Home] {
        let store = interaction_store(HitTarget::Navigation(destination));
        let (clicked, _) = reduce_mouse(
            controller,
            &state,
            mouse(MouseEventKind::Down(MouseButton::Left), 3, 4),
            store.latest(),
        );
        assert_eq!(clicked.input_mode(), InputMode::Normal);
        assert!(clicked.input_text().is_empty());
        assert!(clicked.model().search_draft().is_none());
        assert_eq!(clicked.model().focus, FocusRegion::Navigation);

        let (after_key, actions) = reduce_key(clicked, &state, plain('n'));
        assert_eq!(actions, vec![Action::NextRequested]);
        let (after_wheel, _) = reduce_mouse(
            after_key,
            &state,
            mouse(MouseEventKind::ScrollDown, 0, 0),
            None,
        );
        assert_ne!(after_wheel.model().view, destination);

        controller = reduce_key(UiController::default(), &state, plain('/')).0;
        (controller, _) = reduce_key(controller, &state, plain('x'));
    }
}

#[test]
fn mouse_semantic_controls_dispatch_exact_existing_actions() {
    let state = AppState::default();
    for (semantic, expected) in [
        (SemanticAction::PreviousTrack, Action::PreviousRequested),
        (
            SemanticAction::SeekBackward,
            Action::SeekRelativeRequested { seconds: -10 },
        ),
        (SemanticAction::TogglePlayback, Action::TogglePlayback),
        (
            SemanticAction::SeekForward,
            Action::SeekRelativeRequested { seconds: 10 },
        ),
        (SemanticAction::NextTrack, Action::NextRequested),
    ] {
        let store = interaction_store(HitTarget::Semantic(semantic));
        let (_, actions) = reduce_mouse(
            UiController::default(),
            &state,
            mouse(MouseEventKind::Down(MouseButton::Left), 3, 4),
            store.latest(),
        );
        assert_eq!(actions, vec![expected]);
    }
}

fn playback_state(position_ms: u64, duration_ms: Option<u64>) -> AppState {
    let item = song("mouse-progress", "Mouse progress");
    let media_id = item.id.clone();
    let (state, _) = reduce(AppState::default(), Action::EnqueueMedia { item });
    let (state, _) = reduce(
        state,
        Action::PlayQueueItem {
            id: stable_queue_item_id(&media_id),
        },
    );
    let Some(generation) = state.current_attempt_generation() else {
        panic!("playing item generation");
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

#[test]
fn mouse_progress_uses_bounded_proportion_and_away_from_zero_relative_seconds() {
    let state = playback_state(50_500, Some(101_000));
    for (numerator, expected_seconds) in [(0, -51), (5, 0), (10, 51)] {
        let store = interaction_store(HitTarget::Progress {
            numerator,
            denominator: 10,
        });
        let (_, actions) = reduce_mouse(
            UiController::default(),
            &state,
            mouse(MouseEventKind::Down(MouseButton::Left), 3, 4),
            store.latest(),
        );
        let expected = if expected_seconds == 0 {
            Vec::new()
        } else {
            vec![Action::SeekRelativeRequested {
                seconds: expected_seconds,
            }]
        };
        assert_eq!(actions, expected);
        assert_eq!(state.playback().position_ms, 50_500);
    }

    for duration_ms in [None, Some(0)] {
        let state = playback_state(500, duration_ms);
        let store = interaction_store(HitTarget::Progress {
            numerator: 1,
            denominator: 1,
        });
        let (_, actions) = reduce_mouse(
            UiController::default(),
            &state,
            mouse(MouseEventKind::Down(MouseButton::Left), 3, 4),
            store.latest(),
        );
        assert!(actions.is_empty());
    }
}

#[test]
fn mouse_wheel_reuses_keyboard_selection_and_modal_policy() {
    let state = AppState::default();
    let controller = UiController::default().with_focus(FocusRegion::Navigation);
    let (keyboard, keyboard_actions) = reduce_key(controller.clone(), &state, plain('j'));
    let (wheel, wheel_actions) = reduce_mouse(
        controller,
        &state,
        mouse(MouseEventKind::ScrollDown, 99, 99),
        None,
    );
    assert_eq!(wheel.model(), keyboard.model());
    assert_eq!(wheel_actions, keyboard_actions);

    let (help, _) = reduce_key(UiController::default(), &state, plain('?'));
    let before = help.model().clone();
    let (help, actions) = reduce_mouse(help, &state, mouse(MouseEventKind::ScrollDown, 3, 4), None);
    assert_eq!(help.model(), &before);
    assert!(actions.is_empty());
}

#[test]
fn help_mouse_wheel_scrolls_clipped_help_without_moving_background() -> TestResult {
    let state = podcast_recommendation_state();
    let background = UiController::default()
        .with_view(NavigationItem::Podcasts)
        .with_focus(FocusRegion::Content);
    let (mut controller, _) = reduce_key(background, &state, shifted('?'));
    controller.reconcile_layout(&state, Some(Rect::new(0, 0, 60, 10)));
    let before_model = controller.model().clone();
    let mut before = Terminal::new(TestBackend::new(60, 10))?;
    before.draw(|frame| {
        render_with_model(frame, &state, &Theme::default(), controller.model());
    })?;
    assert!(!before.backend().to_string().contains("Quit"));

    for _ in 0..20 {
        (controller, _) = reduce_mouse(
            controller,
            &state,
            mouse(MouseEventKind::ScrollDown, 0, 0),
            None,
        );
    }
    assert_eq!(controller.model().view, before_model.view);
    assert_eq!(controller.model().focus, before_model.focus);
    assert_eq!(controller.model().overlay, Some(Overlay::Help));
    let mut after = Terminal::new(TestBackend::new(60, 10))?;
    after.draw(|frame| {
        render_with_model(frame, &state, &Theme::default(), controller.model());
    })?;
    assert!(after.backend().to_string().contains("Quit"));

    controller.reconcile_layout(&state, Some(Rect::new(0, 0, 100, 40)));
    let mut grown = Terminal::new(TestBackend::new(100, 40))?;
    grown.draw(|frame| {
        render_with_model(frame, &state, &Theme::default(), controller.model());
    })?;
    assert!(grown.backend().to_string().contains("Navigation"));

    for _ in 0..40 {
        (controller, _) = reduce_mouse(
            controller,
            &state,
            mouse(MouseEventKind::ScrollUp, 0, 0),
            None,
        );
    }
    let mut reset = Terminal::new(TestBackend::new(60, 10))?;
    reset.draw(|frame| {
        render_with_model(frame, &state, &Theme::default(), controller.model());
    })?;
    let rendered = reset.backend().to_string();
    assert!(rendered.contains("Navigation"));
    assert!(!rendered.contains("Quit"));
    Ok(())
}

#[test]
fn mouse_wheel_matches_keyboard_for_content_queue_and_text_entry() {
    let podcast_state = podcast_recommendation_state();
    let content = UiController::default()
        .with_view(NavigationItem::Podcasts)
        .with_focus(FocusRegion::Content);
    let (keyboard, keyboard_actions) = reduce_key(content.clone(), &podcast_state, plain('j'));
    let (wheel, wheel_actions) = reduce_mouse(
        content,
        &podcast_state,
        mouse(MouseEventKind::ScrollDown, 0, 0),
        None,
    );
    assert_eq!(wheel.model(), keyboard.model());
    assert_eq!(wheel_actions, keyboard_actions);

    let (queue_state, _) = apply_actions(
        AppState::default(),
        vec![
            Action::EnqueueMedia {
                item: song("wheel-one", "Wheel one"),
            },
            Action::EnqueueMedia {
                item: song("wheel-two", "Wheel two"),
            },
        ],
    );
    let queue = UiController::default().with_focus(FocusRegion::Queue);
    let (keyboard, keyboard_actions) = reduce_key(queue.clone(), &queue_state, plain('j'));
    let (wheel, wheel_actions) = reduce_mouse(
        queue,
        &queue_state,
        mouse(MouseEventKind::ScrollDown, 0, 0),
        None,
    );
    assert_eq!(wheel.model(), keyboard.model());
    assert_eq!(wheel_actions, keyboard_actions);

    let (search, _) = reduce_key(UiController::default(), &queue_state, plain('/'));
    let before = search.clone();
    let (search, actions) = reduce_mouse(
        search,
        &queue_state,
        mouse(MouseEventKind::ScrollDown, 0, 0),
        None,
    );
    assert_eq!(search, before);
    assert!(actions.is_empty());
}

#[test]
fn mouse_clicks_fail_closed_for_missing_stale_outside_and_modal_geometry() {
    let state = AppState::default();
    let mut store = interaction_store(HitTarget::Semantic(SemanticAction::NextTrack));
    let Some(retained_snapshot) = store.latest().cloned() else {
        panic!("snapshot");
    };
    store.invalidate();
    for snapshot in [None, Some(&retained_snapshot)] {
        let (_, actions) = reduce_mouse(
            UiController::default(),
            &state,
            mouse(MouseEventKind::Down(MouseButton::Left), 3, 4),
            snapshot,
        );
        assert!(actions.is_empty());
    }

    let fresh = interaction_store(HitTarget::Semantic(SemanticAction::NextTrack));
    let (_, actions) = reduce_mouse(
        UiController::default(),
        &state,
        mouse(MouseEventKind::Down(MouseButton::Left), 30, 40),
        fresh.latest(),
    );
    assert!(actions.is_empty());

    let (help, _) = reduce_key(UiController::default(), &state, plain('?'));
    let (_, actions) = reduce_mouse(
        help,
        &state,
        mouse(MouseEventKind::Down(MouseButton::Left), 3, 4),
        fresh.latest(),
    );
    assert!(actions.is_empty());
}

#[test]
fn mouse_surface_matrix_selects_then_activates_search_and_rejects_bad_indices() {
    let first = song("mouse-search-first", "Mouse Search First");
    let second = song("mouse-search-second", "Mouse Search Second");
    let (state, effects) = reduce(
        AppState::default(),
        Action::SearchSubmitted {
            query: "mouse".to_owned(),
            filter: SearchFilter::Songs,
        },
    );
    let [Effect::Search { generation, .. }] = effects.as_slice() else {
        panic!("search fixture must load");
    };
    let (state, _) = reduce(
        state,
        Action::SearchCompleted {
            generation: *generation,
            result: Ok(SearchPage::new(vec![
                SearchItem::Playable(first.clone()),
                SearchItem::Playable(second.clone()),
            ])),
        },
    );
    let controller = UiController::default()
        .with_view(NavigationItem::Search)
        .with_focus(FocusRegion::Player);
    let target = HitTarget::ListRow {
        surface: ListSurface::Search,
        stable_index: 1,
    };
    let store = interaction_store(target);

    let (controller, actions) = reduce_mouse(
        controller,
        &state,
        mouse(MouseEventKind::Down(MouseButton::Left), 3, 4),
        store.latest(),
    );
    assert_eq!(controller.model().view, NavigationItem::Search);
    assert_eq!(controller.model().focus, FocusRegion::Content);
    assert_eq!(
        actions,
        vec![Action::SearchSelectionChanged {
            id: SearchItem::Playable(second.clone()).stable_id(),
        }]
    );

    let (selected_state, _) = apply_actions(state.clone(), actions);
    let (_, actions) = reduce_mouse(
        controller,
        &selected_state,
        mouse(MouseEventKind::Down(MouseButton::Left), 3, 4),
        store.latest(),
    );
    assert_eq!(
        actions,
        vec![Action::PlayMediaList {
            items: vec![first, second.clone()],
            selected_id: second.id,
            shuffle_seed: None,
        }]
    );

    let invalid = interaction_store(HitTarget::ListRow {
        surface: ListSurface::Search,
        stable_index: 99,
    });
    let (_, actions) = reduce_mouse(
        UiController::default().with_view(NavigationItem::Search),
        &state,
        mouse(MouseEventKind::Down(MouseButton::Left), 3, 4),
        invalid.latest(),
    );
    assert!(actions.is_empty());
}

#[test]
fn rendered_search_row_click_exits_hidden_search_editing_before_select_and_activate() -> TestResult
{
    let first = song("hidden-search-first", "Hidden Search First");
    let second = song("hidden-search-second", "Hidden Search Second");
    let (state, effects) = reduce(
        AppState::default(),
        Action::SearchSubmitted {
            query: "old".to_owned(),
            filter: SearchFilter::Songs,
        },
    );
    let [Effect::Search { generation, .. }] = effects.as_slice() else {
        panic!("search fixture load");
    };
    let (state, _) = reduce(
        state,
        Action::SearchCompleted {
            generation: *generation,
            result: Ok(SearchPage::new(vec![
                SearchItem::Playable(first.clone()),
                SearchItem::Playable(second.clone()),
            ])),
        },
    );
    let (mut controller, _) = reduce_key(UiController::default(), &state, plain('/'));
    for character in "new draft".chars() {
        (controller, _) = reduce_key(controller, &state, plain(character));
    }
    let invalid = interaction_store(HitTarget::ListRow {
        surface: ListSurface::Search,
        stable_index: 99,
    });
    let (unchanged, actions) = reduce_mouse(
        controller.clone(),
        &state,
        mouse(MouseEventKind::Down(MouseButton::Left), 3, 4),
        invalid.latest(),
    );
    assert_eq!(unchanged, controller);
    assert!(actions.is_empty());
    let store = rendered_interaction_store(&state, &controller, 90, 30)?;
    let target = HitTarget::ListRow {
        surface: ListSurface::Search,
        stable_index: 1,
    };
    let snapshot = store
        .latest()
        .unwrap_or_else(|| panic!("rendered snapshot"));
    let (column, row) = target_coordinate(snapshot, target, 90, 30);

    let (controller, actions) = reduce_mouse(
        controller,
        &state,
        mouse(MouseEventKind::Down(MouseButton::Left), column, row),
        Some(snapshot),
    );
    assert_eq!(controller.input_mode(), InputMode::Normal);
    assert!(controller.input_text().is_empty());
    assert!(controller.model().search_draft().is_none());
    assert!(matches!(
        actions.as_slice(),
        [Action::SearchSelectionChanged { .. }]
    ));
    let (state, _) = apply_actions(state, actions);

    let (controller, actions) = reduce_mouse(
        controller,
        &state,
        mouse(MouseEventKind::Down(MouseButton::Left), column, row),
        Some(snapshot),
    );
    assert_eq!(
        actions,
        vec![Action::PlayMediaList {
            items: vec![first, second.clone()],
            selected_id: second.id,
            shuffle_seed: None,
        }]
    );
    let (_, global) = reduce_key(controller, &state, plain('n'));
    assert_eq!(global, vec![Action::NextRequested]);
    Ok(())
}

#[test]
fn rendered_queue_row_click_exits_hidden_search_editing_before_select_and_activate() -> TestResult {
    let first = song("hidden-queue-first", "Hidden Queue First");
    let second = song("hidden-queue-second", "Hidden Queue Second");
    let second_id = stable_queue_item_id(&second.id);
    let (state, _) = apply_actions(
        AppState::default(),
        vec![
            Action::EnqueueMedia { item: first },
            Action::EnqueueMedia { item: second },
        ],
    );
    let (mut controller, _) = reduce_key(UiController::default(), &state, plain('/'));
    (controller, _) = reduce_key(controller, &state, plain('x'));
    let store = rendered_interaction_store(&state, &controller, 140, 40)?;
    let target = HitTarget::ListRow {
        surface: ListSurface::Queue,
        stable_index: 1,
    };
    let snapshot = store
        .latest()
        .unwrap_or_else(|| panic!("rendered snapshot"));
    let (column, row) = target_coordinate(snapshot, target, 140, 40);

    let (controller, actions) = reduce_mouse(
        controller,
        &state,
        mouse(MouseEventKind::Down(MouseButton::Left), column, row),
        Some(snapshot),
    );
    assert_eq!(controller.input_mode(), InputMode::Normal);
    assert!(controller.input_text().is_empty());
    assert!(controller.model().search_draft().is_none());
    assert_eq!(controller.model().focus, FocusRegion::Queue);
    assert!(actions.is_empty());

    let (controller, actions) = reduce_mouse(
        controller,
        &state,
        mouse(MouseEventKind::Down(MouseButton::Left), column, row),
        Some(snapshot),
    );
    assert_eq!(actions, vec![Action::PlayQueueItem { id: second_id }]);
    let (controller, wheel_actions) = reduce_mouse(
        controller,
        &state,
        mouse(MouseEventKind::ScrollUp, column, row),
        None,
    );
    assert!(wheel_actions.is_empty());
    assert_eq!(controller.model().focus, FocusRegion::Queue);
    Ok(())
}

#[test]
fn mouse_surface_matrix_selects_then_confirms_pickers_and_palette() {
    let state = AppState::default();

    let (browser, _) = reduce_key(UiController::default(), &state, plain('a'));
    let target = HitTarget::ListRow {
        surface: ListSurface::BrowserPicker,
        stable_index: 1,
    };
    let store = interaction_store(target);
    let (browser, actions) = reduce_mouse(
        browser,
        &state,
        mouse(MouseEventKind::Down(MouseButton::Left), 3, 4),
        store.latest(),
    );
    assert!(actions.is_empty());
    assert_eq!(
        browser.model().browser_picker.selected_browser(),
        Browser::Chrome
    );
    let (browser, actions) = reduce_mouse(
        browser,
        &state,
        mouse(MouseEventKind::Down(MouseButton::Left), 3, 4),
        store.latest(),
    );
    assert_eq!(browser.model().overlay, None);
    assert_eq!(
        actions,
        vec![Action::ConnectAccountRequested {
            browser: Browser::Chrome,
        }]
    );

    let (country, _) = reduce_key(UiController::default(), &state, plain('c'));
    let target = HitTarget::ListRow {
        surface: ListSurface::CountryPicker,
        stable_index: 1,
    };
    let store = interaction_store(target);
    let (country, actions) = reduce_mouse(
        country,
        &state,
        mouse(MouseEventKind::Down(MouseButton::Left), 3, 4),
        store.latest(),
    );
    assert!(actions.is_empty());
    assert_eq!(country.country_picker().selected_region(), &region("US"));
    let (country, actions) = reduce_mouse(
        country,
        &state,
        mouse(MouseEventKind::Down(MouseButton::Left), 3, 4),
        store.latest(),
    );
    assert_eq!(country.model().overlay, None);
    assert_eq!(actions.len(), 2);

    let (palette, _) = reduce_key(UiController::default(), &state, shifted(':'));
    let target = HitTarget::ListRow {
        surface: ListSurface::CommandPalette,
        stable_index: 1,
    };
    let store = interaction_store(target);
    let (palette, actions) = reduce_mouse(
        palette,
        &state,
        mouse(MouseEventKind::Down(MouseButton::Left), 3, 4),
        store.latest(),
    );
    assert!(actions.is_empty());
    assert_eq!(palette.model().palette.selected_index(), 1);
    let (palette, actions) = reduce_mouse(
        palette,
        &state,
        mouse(MouseEventKind::Down(MouseButton::Left), 3, 4),
        store.latest(),
    );
    assert!(actions.is_empty());
    assert_eq!(palette.model().overlay, Some(Overlay::Help));
}

#[test]
fn mouse_surface_matrix_queue_click_uses_rendered_current_selection_then_single_activation() {
    let item = song("mouse-queue-current", "Mouse Queue Current");
    let id = stable_queue_item_id(&item.id);
    let (state, _) = reduce(AppState::default(), Action::EnqueueMedia { item });
    let (state, _) = reduce(state, Action::PlayQueueItem { id: id.clone() });
    let store = interaction_store(HitTarget::ListRow {
        surface: ListSurface::Queue,
        stable_index: 0,
    });

    let (controller, actions) = reduce_mouse(
        UiController::default(),
        &state,
        mouse(MouseEventKind::Down(MouseButton::Left), 3, 4),
        store.latest(),
    );
    assert_eq!(controller.model().focus, FocusRegion::Queue);
    assert_eq!(actions, vec![Action::PlayQueueItem { id }]);
    let (_, up_actions) = reduce_mouse(
        controller,
        &state,
        mouse(MouseEventKind::Up(MouseButton::Left), 3, 4),
        store.latest(),
    );
    assert!(up_actions.is_empty());
}

#[test]
fn mouse_surface_matrix_queue_noncurrent_row_selects_then_activates_in_compact_panel() {
    let first = song("mouse-queue-first", "Mouse Queue First");
    let second = song("mouse-queue-second", "Mouse Queue Second");
    let second_id = stable_queue_item_id(&second.id);
    let (state, _) = apply_actions(
        AppState::default(),
        vec![
            Action::EnqueueMedia { item: first },
            Action::EnqueueMedia { item: second },
        ],
    );
    let starting = UiController::default().with_focus(FocusRegion::Queue);
    let store = rendered_interaction_store(&state, &starting, 90, 30)
        .unwrap_or_else(|error| panic!("compact queue render: {error}"));
    let target = HitTarget::ListRow {
        surface: ListSurface::Queue,
        stable_index: 1,
    };
    let snapshot = store
        .latest()
        .unwrap_or_else(|| panic!("compact queue snapshot"));
    let (column, row) = target_coordinate(snapshot, target, 90, 30);

    let (controller, actions) = reduce_mouse(
        starting,
        &state,
        mouse(MouseEventKind::Down(MouseButton::Left), column, row),
        Some(snapshot),
    );
    assert!(actions.is_empty());
    assert_eq!(controller.model().focus, FocusRegion::Queue);
    assert_eq!(controller.queue_selected_id(), Some(&second_id));
    assert_eq!(
        controller
            .model()
            .normalized_for_layout(LayoutMode::Compact)
            .compact_panel,
        ytermusic::ui::render::CompactPanel::Queue,
    );

    let (controller, actions) = reduce_mouse(
        controller,
        &state,
        mouse(MouseEventKind::Down(MouseButton::Left), column, row),
        Some(snapshot),
    );
    assert_eq!(actions, vec![Action::PlayQueueItem { id: second_id }]);
    let (_, actions) = reduce_mouse(
        controller,
        &state,
        mouse(MouseEventKind::Up(MouseButton::Left), column, row),
        Some(snapshot),
    );
    assert!(actions.is_empty());
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one matrix verifies every remaining content surface against identical click semantics"
)]
fn mouse_surface_matrix_selects_then_activates_remaining_content_surfaces() {
    let chart_first = song("mouse-chart-first", "Mouse Chart First");
    let chart_item = song("mouse-chart", "Mouse Chart");
    let gb = region("GB");
    let (chart_state, effects) = reduce(
        AppState::default(),
        Action::ChartsRequested { region: gb.clone() },
    );
    let [
        Effect::ReadChartCache { generation, .. },
        Effect::LoadCharts { .. },
    ] = effects.as_slice()
    else {
        panic!("chart fixture load");
    };
    let (chart_state, _) = reduce(
        chart_state,
        Action::ChartsCompleted {
            generation: *generation,
            region: gb,
            received_at: 1,
            result: Ok(vec![ChartSection::new(
                "Songs".to_owned(),
                vec![chart_first.clone(), chart_item.clone()],
            )]),
        },
    );
    assert_surface_select_then_activate(
        chart_state,
        NavigationItem::Charts,
        ListSurface::Charts,
        1,
        Action::ChartRowSelectionChanged { item_index: 1 },
        &[Action::PlayMediaList {
            items: vec![chart_first, chart_item.clone()],
            selected_id: chart_item.id,
            shuffle_seed: None,
        }],
    );

    let recommendation_state = podcast_recommendation_state();
    let recommendation_id = recommendation_state.podcasts().recommendations()[1]
        .source_id()
        .clone();
    assert_surface_select_then_activate(
        recommendation_state,
        NavigationItem::Podcasts,
        ListSurface::PodcastRecommendations,
        1,
        Action::PodcastRecommendationSelectionChanged {
            id: recommendation_id,
        },
        &[Action::OpenSelectedPodcastRecommendation],
    );

    let episode_state = opened_podcast_state();
    let Some(show) = episode_state.podcasts().show() else {
        panic!("episode fixture show");
    };
    let episode_items = show.episodes.clone();
    let episode_id = show.episodes[1].id.clone();
    assert_surface_select_then_activate(
        episode_state,
        NavigationItem::Podcasts,
        ListSurface::PodcastEpisodes,
        1,
        Action::PodcastSelectionChanged {
            media_id: episode_id.clone(),
        },
        &[Action::PlayMediaList {
            items: episode_items,
            selected_id: episode_id,
            shuffle_seed: None,
        }],
    );

    let library_first = song("mouse-library-first", "Mouse Library First");
    let library_item = song("mouse-library", "Mouse Library");
    let (library_state, _) = reduce(
        AppState::default(),
        Action::AuthenticationChanged(AuthenticationState::Authenticated),
    );
    let (library_state, effects) = reduce(
        library_state,
        Action::LibraryRequested {
            section: LibrarySection::Songs,
        },
    );
    let [Effect::LoadLibrary { generation, .. }] = effects.as_slice() else {
        panic!("library fixture load");
    };
    let (library_state, _) = reduce(
        library_state,
        Action::LibraryCompleted {
            generation: *generation,
            result: Ok(Page {
                items: vec![
                    LibraryItem::Playable(library_first.clone()),
                    LibraryItem::Playable(library_item.clone()),
                ],
                continuation: None,
                stale: false,
            }),
        },
    );
    assert_surface_select_then_activate(
        library_state,
        NavigationItem::Library,
        ListSurface::Library,
        1,
        Action::LibrarySelectionChanged {
            id: ytermusic::app::stable_library_item_id(&LibraryItem::Playable(
                library_item.clone(),
            )),
        },
        &[Action::PlayMediaList {
            items: vec![library_first, library_item.clone()],
            selected_id: library_item.id,
            shuffle_seed: None,
        }],
    );

    let history_first = song("mouse-history-first", "Mouse History First");
    let history_item = song("mouse-history", "Mouse History");
    let (history_state, effects) = reduce(AppState::default(), Action::HistoryRequested);
    let [Effect::LoadHistory { generation, .. }] = effects.as_slice() else {
        panic!("history fixture load");
    };
    let (history_state, _) = reduce(
        history_state,
        Action::HistoryCompleted {
            generation: *generation,
            result: Ok(vec![
                HistoryEntry {
                    id: 40,
                    item: history_first.clone(),
                    played_at: 2,
                },
                HistoryEntry {
                    id: 41,
                    item: history_item.clone(),
                    played_at: 1,
                },
            ]),
        },
    );
    assert_surface_select_then_activate(
        history_state,
        NavigationItem::History,
        ListSurface::History,
        1,
        Action::HistorySelectionChanged { id: 41 },
        &[Action::PlayMediaList {
            items: vec![history_first, history_item.clone()],
            selected_id: history_item.id,
            shuffle_seed: None,
        }],
    );

    let favorite_first = song("mouse-favorite-first", "Mouse Favorite First");
    let favorite_item = song("mouse-favorite", "Mouse Favorite");
    let favorites_state = loaded_favorites(vec![favorite_first.clone(), favorite_item.clone()]);
    assert_surface_select_then_activate(
        favorites_state,
        NavigationItem::Favorites,
        ListSurface::Favorites,
        1,
        Action::FavoriteSelectionChanged {
            media_id: favorite_item.id.clone(),
        },
        &[Action::PlayMediaList {
            items: vec![favorite_first, favorite_item.clone()],
            selected_id: favorite_item.id,
            shuffle_seed: None,
        }],
    );
}

fn assert_surface_select_then_activate(
    state: AppState,
    view: NavigationItem,
    surface: ListSurface,
    stable_index: usize,
    selection: Action,
    activation: &[Action],
) {
    let store = interaction_store(HitTarget::ListRow {
        surface,
        stable_index,
    });
    let controller = UiController::default()
        .with_view(view)
        .with_focus(FocusRegion::Player);
    let (controller, actions) = reduce_mouse(
        controller,
        &state,
        mouse(MouseEventKind::Down(MouseButton::Left), 3, 4),
        store.latest(),
    );
    assert_eq!(controller.model().focus, FocusRegion::Content);
    assert_eq!(actions, vec![selection]);
    let (state, _) = apply_actions(state, actions);
    let (_, actions) = reduce_mouse(
        controller,
        &state,
        mouse(MouseEventKind::Down(MouseButton::Left), 3, 4),
        store.latest(),
    );
    assert_eq!(actions, activation);
}

fn region(value: &str) -> RegionCode {
    match RegionCode::parse(value) {
        Ok(region) => region,
        Err(error) => panic!("test region must be valid: {error}"),
    }
}

fn song(video_id: &str, title: &str) -> MediaItem {
    MediaItem {
        id: MediaId {
            provider: "youtube-music".to_owned(),
            video_id: video_id.to_owned(),
        },
        kind: MediaKind::Song,
        title: title.to_owned(),
        creators: vec!["Controller Artist".to_owned()],
        collection: None,
        duration_ms: Some(180_000),
        artwork_url: None,
        explicit: false,
    }
}

fn loaded_favorites(items: Vec<MediaItem>) -> AppState {
    let entries = items
        .into_iter()
        .enumerate()
        .map(|(index, item)| FavoriteEntry {
            id: i64::try_from(index).unwrap_or_default(),
            item,
            favorited_at: i64::try_from(index).unwrap_or_default(),
        })
        .collect();
    let (state, effects) = reduce(AppState::default(), Action::FavoritesRequested);
    let [Effect::LoadFavorites { generation }] = effects.as_slice() else {
        panic!("favorites fixture must load");
    };
    reduce(
        state,
        Action::FavoritesCompleted {
            generation: *generation,
            result: Ok(entries),
        },
    )
    .0
}

fn timed_lyrics_state(video_id: &str) -> AppState {
    let item = song(video_id, "Lyrics fixture");
    let (state, effects) = reduce(
        AppState::default(),
        Action::SearchSubmitted {
            query: "lyrics".to_owned(),
            filter: SearchFilter::Songs,
        },
    );
    let [Effect::Search { generation, .. }] = effects.as_slice() else {
        panic!("lyrics fixture search must load");
    };
    let (state, _) = reduce(
        state,
        Action::SearchCompleted {
            generation: *generation,
            result: Ok(SearchPage::new(vec![SearchItem::Playable(item.clone())])),
        },
    );
    let (state, effects) = reduce(state, Action::ActivateSearchResult { index: 0 });
    let Some(generation) = effects.iter().find_map(|effect| match effect {
        Effect::LoadLyrics { generation, .. } => Some(*generation),
        _ => None,
    }) else {
        panic!("lyrics fixture must request lyrics");
    };
    let document = LyricsDocument::new(
        LyricsSource::Lrclib,
        Some("plain fallback must not control timed scrolling".to_owned()),
        ["one", "two", "three", "four"]
            .into_iter()
            .enumerate()
            .map(|(index, text)| {
                let start = u64::try_from(index).unwrap_or(0).saturating_mul(1_000);
                TimedLyricLine::new(start, Some(start.saturating_add(1_000)), text)
                    .unwrap_or_else(|error| panic!("valid timed lyric: {error}"))
            })
            .collect(),
        false,
    )
    .unwrap_or_else(|error| panic!("valid lyrics document: {error}"));
    reduce(
        state,
        Action::LyricsCompleted {
            generation,
            media_id: item.id.into(),
            result: Ok(Some(document)),
        },
    )
    .0
}

fn plain_lyrics_state(video_id: &str) -> AppState {
    let item = song(video_id, "Plain lyrics fixture");
    let (state, effects) = reduce(
        AppState::default(),
        Action::SearchSubmitted {
            query: "plain lyrics".to_owned(),
            filter: SearchFilter::Songs,
        },
    );
    let [Effect::Search { generation, .. }] = effects.as_slice() else {
        panic!("plain lyrics fixture search must load");
    };
    let (state, _) = reduce(
        state,
        Action::SearchCompleted {
            generation: *generation,
            result: Ok(SearchPage::new(vec![SearchItem::Playable(item.clone())])),
        },
    );
    let (state, effects) = reduce(state, Action::ActivateSearchResult { index: 0 });
    let Some(generation) = effects.iter().find_map(|effect| match effect {
        Effect::LoadLyrics { generation, .. } => Some(*generation),
        _ => None,
    }) else {
        panic!("plain lyrics fixture must request lyrics");
    };
    let document = LyricsDocument::new(
        LyricsSource::YouTubeMusic,
        Some("abcdefghijklmnopqrstuvwxyz012345".to_owned()),
        Vec::new(),
        false,
    )
    .unwrap_or_else(|error| panic!("valid plain lyrics document: {error}"));
    reduce(
        state,
        Action::LyricsCompleted {
            generation,
            media_id: item.id.into(),
            result: Ok(Some(document)),
        },
    )
    .0
}

#[test]
fn lyrics_overlay_toggles_and_suppresses_background_commands() {
    let state = timed_lyrics_state("overlay");
    let (mut controller, actions) = reduce_key(
        UiController::default(),
        &state,
        KeyEvent::new(KeyCode::Char('L'), KeyModifiers::SHIFT),
    );
    assert!(actions.is_empty());
    assert_eq!(controller.model().overlay, Some(Overlay::Lyrics));
    assert!(controller.model().lyrics.follow_active());

    for event in [plain(' '), plain('n'), plain(']'), plain('q')] {
        let (next, actions) = reduce_key(controller, &state, event);
        controller = next;
        assert!(actions.is_empty());
        assert_eq!(controller.model().overlay, Some(Overlay::Lyrics));
    }

    let (controller, _) = reduce_key(controller, &state, key(KeyCode::Esc));
    assert_eq!(controller.model().overlay, None);
    let (controller, _) = reduce_key(
        controller,
        &state,
        KeyEvent::new(KeyCode::Char('L'), KeyModifiers::SHIFT),
    );
    assert_eq!(controller.model().overlay, Some(Overlay::Lyrics));
    let (controller, _) = reduce_key(
        controller,
        &state,
        KeyEvent::new(KeyCode::Char('L'), KeyModifiers::SHIFT),
    );
    assert_eq!(controller.model().overlay, None);
}

#[test]
fn lyrics_overlay_manual_scroll_recenter_and_media_reset_follow_state() {
    let state = timed_lyrics_state("first-track");
    let (controller, _) = reduce_key(
        UiController::default(),
        &state,
        KeyEvent::new(KeyCode::Char('L'), KeyModifiers::SHIFT),
    );
    let (controller, _) = reduce_key(controller, &state, plain('j'));
    assert!(!controller.model().lyrics.follow_active());
    assert_eq!(controller.model().lyrics.selected_line(), Some(1));
    assert_eq!(controller.model().lyrics.scroll(), 1);
    let mut controller = controller;
    controller.reconcile_layout(&state, Some(Rect::new(0, 0, 100, 40)));
    assert_eq!(controller.model().lyrics.selected_line(), Some(1));
    assert_eq!(controller.model().lyrics.scroll(), 1);
    let (controller, _) = reduce_key(controller, &state, key(KeyCode::Up));
    assert_eq!(controller.model().lyrics.selected_line(), Some(0));
    let (controller, _) = reduce_key(controller, &state, key(KeyCode::Enter));
    assert!(controller.model().lyrics.follow_active());

    let (mut controller, _) = reduce_key(controller, &state, plain('k'));
    assert!(!controller.model().lyrics.follow_active());
    let replacement = timed_lyrics_state("replacement-track");
    controller.reconcile_state(&replacement);
    assert!(controller.model().lyrics.follow_active());
    assert_eq!(
        controller.model().lyrics.selected_line(),
        replacement.lyrics().active_line_index()
    );
}

#[test]
fn plain_lyrics_overlay_scrolls_with_vim_and_arrow_keys() {
    let state = plain_lyrics_state("plain-scroll");
    let (mut controller, _) = reduce_key(
        UiController::default(),
        &state,
        KeyEvent::new(KeyCode::Char('L'), KeyModifiers::SHIFT),
    );
    controller.reconcile_layout(&state, Some(Rect::new(0, 0, 20, 6)));
    let (controller, _) = reduce_key(controller, &state, plain('j'));
    assert!(!controller.model().lyrics.follow_active());
    assert_eq!(controller.model().lyrics.scroll(), 1);
    let (controller, _) = reduce_key(controller, &state, key(KeyCode::Down));
    assert_eq!(controller.model().lyrics.scroll(), 1);
    let mut controller = controller;
    for _ in 0..100 {
        (controller, _) = reduce_key(controller, &state, plain('j'));
    }
    assert_eq!(controller.model().lyrics.scroll(), 1);
    let (controller, _) = reduce_key(controller, &state, plain('k'));
    assert_eq!(controller.model().lyrics.scroll(), 0);
    let (mut controller, _) = reduce_key(controller, &state, plain('j'));
    assert_eq!(controller.model().lyrics.scroll(), 1);
    controller.reconcile_layout(&state, Some(Rect::new(0, 0, 100, 40)));
    assert_eq!(controller.model().lyrics.scroll(), 0);
    let (controller, _) = reduce_key(controller, &state, key(KeyCode::Enter));
    assert!(controller.model().lyrics.follow_active());
    assert_eq!(controller.model().lyrics.scroll(), 0);
}

#[test]
fn mouse_wheel_scrolls_lyrics_overlay_without_leaking_to_background() {
    let state = timed_lyrics_state("mouse-lyrics-wheel");
    let (controller, _) = reduce_key(
        UiController::default()
            .with_view(NavigationItem::Charts)
            .with_focus(FocusRegion::Content),
        &state,
        KeyEvent::new(KeyCode::Char('L'), KeyModifiers::SHIFT),
    );
    let (controller, actions) = reduce_mouse(
        controller,
        &state,
        mouse(MouseEventKind::ScrollDown, 0, 0),
        None,
    );
    assert!(actions.is_empty());
    assert_eq!(controller.model().view, NavigationItem::Charts);
    assert_eq!(controller.model().focus, FocusRegion::Content);
    assert!(!controller.model().lyrics.follow_active());
    assert_eq!(controller.model().lyrics.selected_line(), Some(1));
}

#[test]
fn plain_lyrics_mouse_wheel_enters_manual_scroll_without_exposing_text_targets() -> TestResult {
    let state = plain_lyrics_state("plain-mouse-wheel");
    let (mut controller, _) = reduce_key(
        UiController::default()
            .with_view(NavigationItem::History)
            .with_focus(FocusRegion::Content),
        &state,
        KeyEvent::new(KeyCode::Char('L'), KeyModifiers::SHIFT),
    );
    controller.reconcile_layout(&state, Some(Rect::new(0, 0, 20, 6)));
    let store = rendered_interaction_store(&state, &controller, 20, 6)?;
    assert!(store.latest().is_some_and(InteractionSnapshot::is_empty));

    let (controller, actions) = reduce_mouse(
        controller,
        &state,
        mouse(MouseEventKind::ScrollDown, 0, 0),
        store.latest(),
    );
    assert!(actions.is_empty());
    assert_eq!(controller.model().view, NavigationItem::History);
    assert_eq!(controller.model().focus, FocusRegion::Content);
    assert!(!controller.model().lyrics.follow_active());
    assert_eq!(controller.model().lyrics.scroll(), 1);
    let (controller, _) = reduce_mouse(
        controller,
        &state,
        mouse(MouseEventKind::ScrollUp, 0, 0),
        store.latest(),
    );
    assert_eq!(controller.model().lyrics.scroll(), 0);
    Ok(())
}

fn apply_actions(mut state: AppState, actions: Vec<Action>) -> (AppState, Vec<Effect>) {
    let mut all_effects = Vec::new();
    for action in actions {
        let (next, effects) = reduce(state, action);
        state = next;
        all_effects.extend(effects);
    }
    (state, all_effects)
}

fn podcast_recommendations(
    country: &str,
    rows: &[(&str, &str, &str)],
) -> PodcastRecommendationPage {
    let results = rows
        .iter()
        .map(|(id, title, publisher)| {
            serde_json::json!({"id": id, "name": title, "artistName": publisher})
        })
        .collect::<Vec<_>>();
    let bytes = serde_json::to_vec(&serde_json::json!({
        "feed": {"country": country, "results": results}
    }))
    .unwrap_or_else(|error| panic!("podcast fixture must encode: {error}"));
    parse_apple_top_shows(&bytes)
        .unwrap_or_else(|error| panic!("podcast fixture must parse: {error}"))
}

fn podcast_recommendation_state() -> AppState {
    let us = region("US");
    let page = podcast_recommendations(
        "US",
        &[
            ("daily", "The Daily", "NYT"),
            ("up-first", "Up First", "NPR"),
        ],
    );
    let (state, effects) = reduce(
        AppState::new(Config {
            region: us.clone(),
            ..Config::default()
        }),
        Action::PodcastRecommendationsRequested { region: us.clone() },
    );
    let [Effect::LoadPodcastRecommendations { generation, .. }] = effects.as_slice() else {
        panic!("podcast recommendation fixture must load");
    };
    reduce(
        state,
        Action::PodcastRecommendationsCompleted {
            generation: *generation,
            requested_region: us,
            result: Ok(page),
        },
    )
    .0
}

fn opened_podcast_state() -> AppState {
    opened_podcast_state_with_artwork(None)
}

fn opened_podcast_state_with_artwork(artwork_url: Option<url::Url>) -> AppState {
    let episode = MediaItem {
        kind: MediaKind::PodcastEpisode,
        ..song("opened-episode", "Opened Episode")
    };
    let first_episode = MediaItem {
        kind: MediaKind::PodcastEpisode,
        artwork_url,
        ..song("opened-episode-first", "Opened Episode First")
    };
    let podcast = Podcast {
        id: "opened-show".to_owned(),
        title: "Opened Show".to_owned(),
        creators: vec!["Host".to_owned()],
        description: None,
        artwork_url: None,
        episodes: vec![first_episode, episode],
    };
    let metadata = ytermusic::app::SearchMetadata::new(
        ytermusic::app::SearchMetadataKind::Podcast,
        "Opened Show",
    )
    .with_provider_id("opened-show");
    let (state, effects) = reduce(
        AppState::default(),
        Action::SearchSubmitted {
            query: "opened show".to_owned(),
            filter: SearchFilter::Podcasts,
        },
    );
    let [Effect::Search { generation, .. }] = effects.as_slice() else {
        panic!("podcast fixture search must load");
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
        panic!("podcast fixture must open");
    };
    reduce(
        state,
        Action::PodcastCompleted {
            generation: *generation,
            result: Ok(podcast),
        },
    )
    .0
}

fn selected_search_playable(item: &MediaItem) -> AppState {
    let (state, effects) = reduce(
        AppState::default(),
        Action::SearchSubmitted {
            query: "favorite target".to_owned(),
            filter: SearchFilter::Songs,
        },
    );
    let [Effect::Search { generation, .. }] = effects.as_slice() else {
        panic!("search fixture must load");
    };
    reduce(
        state,
        Action::SearchCompleted {
            generation: *generation,
            result: Ok(SearchPage::new(vec![SearchItem::Playable(item.clone())])),
        },
    )
    .0
}

fn selected_chart_playable(item: &MediaItem) -> AppState {
    loaded_shuffled_chart(std::slice::from_ref(item), 0)
}

fn selected_library_playable(item: &MediaItem) -> AppState {
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
        panic!("library fixture must load");
    };
    reduce(
        state,
        Action::LibraryCompleted {
            generation: *generation,
            result: Ok(Page {
                items: vec![LibraryItem::Playable(item.clone())],
                continuation: None,
                stale: false,
            }),
        },
    )
    .0
}

fn selected_history_playable(item: &MediaItem) -> AppState {
    let (state, effects) = reduce(AppState::default(), Action::HistoryRequested);
    let [Effect::LoadHistory { generation, .. }] = effects.as_slice() else {
        panic!("history fixture must load");
    };
    reduce(
        state,
        Action::HistoryCompleted {
            generation: *generation,
            result: Ok(vec![HistoryEntry {
                id: 1,
                item: item.clone(),
                played_at: 1,
            }]),
        },
    )
    .0
}

#[test]
fn favorite_shortcut_targets_selected_playable_content_without_other_ui_changes() {
    let search = song("favorite-search", "Favorite Search");
    let chart = song("favorite-chart", "Favorite Chart");
    let library = song("favorite-library", "Favorite Library");
    let history = song("favorite-history", "Favorite History");
    let podcast_state = opened_podcast_state();
    let Some(podcast) = podcast_state
        .podcasts()
        .show()
        .and_then(|show| show.episodes.first())
        .cloned()
    else {
        panic!("opened podcast fixture must have an episode");
    };

    for (name, state, view, expected) in [
        (
            "search",
            selected_search_playable(&search),
            NavigationItem::Search,
            search,
        ),
        (
            "charts",
            selected_chart_playable(&chart),
            NavigationItem::Charts,
            chart,
        ),
        (
            "library",
            selected_library_playable(&library),
            NavigationItem::Library,
            library,
        ),
        (
            "history",
            selected_history_playable(&history),
            NavigationItem::History,
            history,
        ),
        (
            "podcast episode",
            podcast_state,
            NavigationItem::Podcasts,
            podcast,
        ),
    ] {
        let controller = UiController::default()
            .with_view(view)
            .with_focus(FocusRegion::Content)
            .with_shuffle_seed(91);
        let (next, actions) = reduce_key(controller.clone(), &state, plain('f'));
        assert_eq!(next, controller, "{name}");
        assert_eq!(
            actions,
            vec![Action::FavoriteToggleRequested { item: expected }],
            "{name}"
        );
        let (_, shuffle) = reduce_key(next, &state, plain('s'));
        assert_eq!(
            shuffle,
            vec![Action::ShuffleEnabledChanged {
                enabled: !state.queue().is_shuffled(),
                seed: 91,
            }],
            "{name}"
        );
    }
}

#[test]
fn favorites_content_moves_toggles_and_activates_the_explicit_list() {
    let first = song("favorites-first", "Favorites First");
    let selected = song("favorites-selected", "Favorites Selected");
    let state = loaded_favorites(vec![first.clone(), selected.clone()]);
    let controller = UiController::default()
        .with_view(NavigationItem::Favorites)
        .with_focus(FocusRegion::Content)
        .with_shuffle_seed(73);

    let (controller, actions) = reduce_key(controller, &state, key(KeyCode::Down));
    assert_eq!(
        actions,
        vec![Action::FavoriteSelectionChanged {
            media_id: selected.id.clone(),
        }]
    );
    let (state, _) = apply_actions(state, actions);

    let (same_controller, actions) = reduce_key(controller.clone(), &state, plain('f'));
    assert_eq!(same_controller, controller);
    assert_eq!(
        actions,
        vec![Action::FavoriteToggleRequested {
            item: selected.clone(),
        }]
    );

    let (_, actions) = reduce_key(controller, &state, key(KeyCode::Enter));
    assert_eq!(
        actions,
        vec![Action::PlayMediaList {
            items: vec![first, selected.clone()],
            selected_id: selected.id,
            shuffle_seed: None,
        }]
    );
}

#[test]
fn favorites_selection_routes_selected_artwork() -> TestResult {
    let mut first = song("favorites-art-first", "Favorites Art First");
    first.artwork_url = Some(url::Url::parse("https://example.com/first.jpg")?);
    let mut selected = song("favorites-art-selected", "Favorites Art Selected");
    let expected = url::Url::parse("https://example.com/selected.jpg")?;
    selected.artwork_url = Some(expected.clone());
    let (state, effects) = reduce(AppState::default(), Action::FavoritesRequested);
    let [Effect::LoadFavorites { generation }] = effects.as_slice() else {
        panic!("favorites fixture must load");
    };
    let (state, _) = reduce(
        state,
        Action::FavoritesCompleted {
            generation: *generation,
            result: Ok(vec![
                FavoriteEntry {
                    id: 1,
                    item: first,
                    favorited_at: 2,
                },
                FavoriteEntry {
                    id: 2,
                    item: selected.clone(),
                    favorited_at: 1,
                },
            ]),
        },
    );
    let (state, _) = reduce(
        state,
        Action::ArtworkSurfaceChanged {
            surface: ArtworkSurface::Favorites,
        },
    );
    let (_, effects) = reduce(
        state,
        Action::FavoriteSelectionChanged {
            media_id: selected.id,
        },
    );
    assert!(matches!(
        effects.as_slice(),
        [Effect::FetchArtwork { url, .. }] if url.as_url() == &expected
    ));
    Ok(())
}

#[test]
fn favorite_shortcut_targets_selected_queue_item_and_current_player_item() {
    let current = song("favorite-current", "Favorite Current");
    let selected = song("favorite-selected-queue", "Favorite Selected Queue");
    let (state, _) = reduce(
        AppState::default(),
        Action::EnqueueMedia {
            item: current.clone(),
        },
    );
    let (state, _) = reduce(
        state,
        Action::EnqueueMedia {
            item: selected.clone(),
        },
    );

    let (queue, move_actions) = reduce_key(
        UiController::default().with_focus(FocusRegion::Queue),
        &state,
        plain('j'),
    );
    assert!(move_actions.is_empty());
    let (queue_after, actions) = reduce_key(queue.clone(), &state, plain('f'));
    assert_eq!(queue_after, queue);
    assert_eq!(
        actions,
        vec![Action::FavoriteToggleRequested { item: selected }]
    );

    assert_eq!(
        state.playback().status,
        ytermusic::domain::PlaybackStatus::Stopped
    );
    assert_eq!(state.playback().current.as_ref(), Some(&current.id));
    let mut player = UiController::default().with_focus(FocusRegion::Player);
    player.reconcile_state(&state);
    let (player_after, actions) = reduce_key(player.clone(), &state, plain('f'));
    assert_eq!(player_after, player);
    assert_eq!(
        actions,
        vec![Action::FavoriteToggleRequested { item: current }]
    );
}

#[test]
fn player_favorite_targets_playback_current_while_podcast_progress_is_pending() {
    let music = song("favorite-playing-music", "Favorite Playing Music");
    let podcast = MediaItem {
        kind: MediaKind::PodcastEpisode,
        ..song("favorite-pending-podcast", "Favorite Pending Podcast")
    };
    let (state, _) = reduce(
        AppState::default(),
        Action::EnqueueMedia {
            item: music.clone(),
        },
    );
    let (state, _) = reduce(
        state,
        Action::EnqueueMedia {
            item: podcast.clone(),
        },
    );
    let (state, _) = reduce(
        state,
        Action::PlayQueueItem {
            id: stable_queue_item_id(&music.id),
        },
    );
    let (state, effects) = reduce(
        state,
        Action::PlayQueueItem {
            id: stable_queue_item_id(&podcast.id),
        },
    );
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, Effect::LoadPodcastProgress { .. }))
    );
    assert_eq!(
        state
            .queue()
            .current()
            .map(ytermusic::queue::QueueItem::media),
        Some(&podcast)
    );
    assert_eq!(state.playback().current.as_ref(), Some(&music.id));

    let mut controller = UiController::default().with_focus(FocusRegion::Player);
    controller.reconcile_state(&state);
    let (next, actions) = reduce_key(controller.clone(), &state, plain('f'));

    assert_eq!(next, controller);
    assert_eq!(
        actions,
        vec![Action::FavoriteToggleRequested { item: music }]
    );
}

#[test]
fn player_favorite_targets_canonical_media_during_resolution() {
    let item = song("favorite-resolving", "Favorite Resolving");
    let (state, _) = reduce(
        AppState::default(),
        Action::EnqueueMedia { item: item.clone() },
    );
    let (state, _) = reduce(
        state,
        Action::PlayQueueItem {
            id: stable_queue_item_id(&item.id),
        },
    );
    assert_eq!(state.playback().current.as_ref(), Some(&item.id));
    assert_eq!(
        state.playback().status,
        ytermusic::domain::PlaybackStatus::Resolving
    );

    let mut controller = UiController::default().with_focus(FocusRegion::Player);
    controller.reconcile_state(&state);
    let (next, actions) = reduce_key(controller.clone(), &state, plain('f'));

    assert_eq!(next, controller);
    assert_eq!(actions, vec![Action::FavoriteToggleRequested { item }]);
}

#[test]
fn favorite_shortcut_ignores_navigation_metadata_recommendations_settings_and_empty_surfaces() {
    let playable = song("favorite-navigation", "Favorite Navigation");
    let selected = selected_search_playable(&playable);
    let (_, actions) = reduce_key(
        UiController::default()
            .with_view(NavigationItem::Search)
            .with_focus(FocusRegion::Navigation),
        &selected,
        plain('f'),
    );
    assert!(actions.is_empty());

    let metadata = ytermusic::app::SearchMetadata::new(
        ytermusic::app::SearchMetadataKind::Album,
        "Favorite metadata",
    );
    let (metadata_state, effects) = reduce(
        AppState::default(),
        Action::SearchSubmitted {
            query: "metadata".to_owned(),
            filter: SearchFilter::All,
        },
    );
    let [Effect::Search { generation, .. }] = effects.as_slice() else {
        panic!("metadata fixture must load");
    };
    let metadata_state = reduce(
        metadata_state,
        Action::SearchCompleted {
            generation: *generation,
            result: Ok(SearchPage::new(vec![SearchItem::Metadata(metadata)])),
        },
    )
    .0;

    for (name, state, view) in vec![
        ("search metadata", metadata_state, NavigationItem::Search),
        (
            "podcast recommendations",
            podcast_recommendation_state(),
            NavigationItem::Podcasts,
        ),
        ("settings", AppState::default(), NavigationItem::Settings),
        ("home", AppState::default(), NavigationItem::Home),
        ("empty search", AppState::default(), NavigationItem::Search),
        ("empty charts", AppState::default(), NavigationItem::Charts),
        (
            "empty podcasts",
            AppState::default(),
            NavigationItem::Podcasts,
        ),
        (
            "empty library",
            AppState::default(),
            NavigationItem::Library,
        ),
        (
            "empty history",
            AppState::default(),
            NavigationItem::History,
        ),
    ] {
        let (_, actions) = reduce_key(
            UiController::default()
                .with_view(view)
                .with_focus(FocusRegion::Content),
            &state,
            plain('f'),
        );
        assert!(actions.is_empty(), "{name}");
    }

    let (_, actions) = reduce_key(
        UiController::default().with_focus(FocusRegion::Player),
        &AppState::default(),
        plain('f'),
    );
    assert!(actions.is_empty(), "empty player");

    let (_, actions) = reduce_key(
        UiController::default().with_focus(FocusRegion::Queue),
        &AppState::default(),
        plain('f'),
    );
    assert!(actions.is_empty(), "empty queue");
}

#[test]
fn favorite_shortcut_ignores_loading_and_error_content_with_retained_selection() {
    let item = song("favorite-retained", "Favorite Retained");
    let (state, effects) = reduce(
        AppState::default(),
        Action::SearchSubmitted {
            query: "retained".to_owned(),
            filter: SearchFilter::Songs,
        },
    );
    let [Effect::Search { generation, .. }] = effects.as_slice() else {
        panic!("search fixture must load");
    };
    let (state, _) = reduce(
        state,
        Action::SearchCompleted {
            generation: *generation,
            result: Ok(
                SearchPage::new(vec![SearchItem::Playable(item)]).with_continuation("next-page")
            ),
        },
    );
    let (loading, effects) = reduce(state, Action::SearchMoreRequested);
    let [Effect::SearchMore { generation, .. }] = effects.as_slice() else {
        panic!("search continuation fixture must load");
    };
    assert!(loading.search().selected_id().is_some());
    assert!(loading.search().loading_more());

    let controller = UiController::default()
        .with_view(NavigationItem::Search)
        .with_focus(FocusRegion::Content);
    let (_, actions) = reduce_key(controller.clone(), &loading, plain('f'));
    assert!(actions.is_empty(), "loading content");

    let error = reduce(
        loading,
        Action::SearchCompleted {
            generation: *generation,
            result: Err(ytermusic::app::AppError::new(
                ytermusic::app::AppErrorCategory::Search,
                "page failed",
            )),
        },
    )
    .0;
    assert!(error.search().selected_id().is_some());
    assert!(error.search().error().is_some());
    let (_, actions) = reduce_key(controller, &error, plain('f'));
    assert!(actions.is_empty(), "error content");
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "table fixtures exercise the same explicit-list contract across every playable surface"
)]
fn explicit_list_activation_replaces_queue_from_every_loaded_playable_surface() {
    struct Case {
        name: &'static str,
        state: AppState,
        view: NavigationItem,
        items: Vec<MediaItem>,
        selected_id: MediaId,
    }

    let search_first = song("explicit-search-first", "Explicit Search First");
    let search_second = song("explicit-search-second", "Explicit Search Second");
    let (search_state, effects) = reduce(
        AppState::default(),
        Action::SearchSubmitted {
            query: "explicit".to_owned(),
            filter: SearchFilter::Songs,
        },
    );
    let [Effect::Search { generation, .. }] = effects.as_slice() else {
        panic!("search fixture must load");
    };
    let (search_state, _) = reduce(
        search_state,
        Action::SearchCompleted {
            generation: *generation,
            result: Ok(SearchPage::new(vec![
                SearchItem::Playable(search_first.clone()),
                SearchItem::Playable(search_second.clone()),
            ])),
        },
    );
    let (search_state, _) = reduce(
        search_state,
        Action::SearchSelectionChanged {
            id: SearchItem::Playable(search_second.clone()).stable_id(),
        },
    );

    let chart_first = song("explicit-chart-first", "Explicit Chart First");
    let chart_second = song("explicit-chart-second", "Explicit Chart Second");
    let gb = region("GB");
    let (chart_state, effects) = reduce(
        AppState::default(),
        Action::ChartsRequested { region: gb.clone() },
    );
    let [
        Effect::ReadChartCache { generation, .. },
        Effect::LoadCharts { .. },
    ] = effects.as_slice()
    else {
        panic!("chart fixture must load");
    };
    let (chart_state, _) = reduce(
        chart_state,
        Action::ChartsCompleted {
            generation: *generation,
            region: gb,
            received_at: 1,
            result: Ok(vec![
                ChartSection::new("First".to_owned(), vec![chart_first.clone()]),
                ChartSection::new("Second".to_owned(), vec![chart_second.clone()]),
            ]),
        },
    );
    let (chart_state, _) = reduce(
        chart_state,
        Action::ChartRowSelectionChanged { item_index: 1 },
    );

    let history_first = song("explicit-history-first", "Explicit History First");
    let history_second = song("explicit-history-second", "Explicit History Second");
    let (history_state, effects) = reduce(AppState::default(), Action::HistoryRequested);
    let [Effect::LoadHistory { generation, .. }] = effects.as_slice() else {
        panic!("history fixture must load");
    };
    let (history_state, _) = reduce(
        history_state,
        Action::HistoryCompleted {
            generation: *generation,
            result: Ok(vec![
                HistoryEntry {
                    id: 1,
                    item: history_first.clone(),
                    played_at: 2,
                },
                HistoryEntry {
                    id: 2,
                    item: history_second.clone(),
                    played_at: 1,
                },
            ]),
        },
    );
    let (history_state, _) = reduce(history_state, Action::HistorySelectionChanged { id: 2 });

    let library_first = song("explicit-library-first", "Explicit Library First");
    let library_second = song("explicit-library-second", "Explicit Library Second");
    let (library_state, _) = reduce(
        AppState::default(),
        Action::AuthenticationChanged(AuthenticationState::Authenticated),
    );
    let (library_state, effects) = reduce(
        library_state,
        Action::LibraryRequested {
            section: LibrarySection::Songs,
        },
    );
    let [Effect::LoadLibrary { generation, .. }] = effects.as_slice() else {
        panic!("library fixture must load");
    };
    let (library_state, _) = reduce(
        library_state,
        Action::LibraryCompleted {
            generation: *generation,
            result: Ok(Page {
                items: vec![
                    LibraryItem::Playable(library_first.clone()),
                    LibraryItem::Playable(library_second.clone()),
                ],
                continuation: None,
                stale: false,
            }),
        },
    );
    let (library_state, _) = reduce(
        library_state,
        Action::LibrarySelectionChanged {
            id: ytermusic::app::stable_library_item_id(&LibraryItem::Playable(
                library_second.clone(),
            )),
        },
    );

    let podcast_state = opened_podcast_state();
    let Some(show) = podcast_state.podcasts().show() else {
        panic!("podcast fixture must be open");
    };
    let podcast_items = show.episodes.clone();
    let podcast_selected = podcast_items[1].id.clone();
    let (podcast_state, _) = reduce(
        podcast_state,
        Action::PodcastSelectionChanged {
            media_id: podcast_selected.clone(),
        },
    );

    let cases = [
        Case {
            name: "search",
            state: search_state,
            view: NavigationItem::Search,
            items: vec![search_first, search_second.clone()],
            selected_id: search_second.id,
        },
        Case {
            name: "charts",
            state: chart_state,
            view: NavigationItem::Charts,
            items: vec![chart_first, chart_second.clone()],
            selected_id: chart_second.id,
        },
        Case {
            name: "history",
            state: history_state,
            view: NavigationItem::History,
            items: vec![history_first, history_second.clone()],
            selected_id: history_second.id,
        },
        Case {
            name: "library",
            state: library_state,
            view: NavigationItem::Library,
            items: vec![library_first, library_second.clone()],
            selected_id: library_second.id,
        },
        Case {
            name: "podcast episodes",
            state: podcast_state,
            view: NavigationItem::Podcasts,
            items: podcast_items,
            selected_id: podcast_selected,
        },
    ];

    for case in cases {
        let (state, _) = reduce(
            case.state,
            Action::ShuffleEnabledChanged {
                enabled: true,
                seed: 9,
            },
        );
        let controller = UiController::default()
            .with_view(case.view)
            .with_focus(FocusRegion::Content)
            .with_shuffle_seed(700);
        let (_, actions) = reduce_key(controller, &state, key(KeyCode::Enter));
        assert_eq!(
            actions,
            vec![Action::PlayMediaList {
                items: case.items,
                selected_id: case.selected_id,
                shuffle_seed: Some(700),
            }],
            "{}",
            case.name
        );
    }
}

#[test]
fn explicit_list_excludes_metadata_and_keeps_only_the_first_duplicate_media_id() {
    let first = song("explicit-duplicate", "First Duplicate");
    let duplicate = MediaItem {
        title: "Later Duplicate".to_owned(),
        ..first.clone()
    };
    let other_provider = MediaItem {
        id: MediaId {
            provider: "other-provider".to_owned(),
            video_id: first.id.video_id.clone(),
        },
        title: "Same video, other provider".to_owned(),
        ..first.clone()
    };
    let selected = song("explicit-selected", "Selected");
    let metadata = ytermusic::app::SearchMetadata::new(
        ytermusic::app::SearchMetadataKind::Album,
        "Metadata row",
    );
    let (state, effects) = reduce(
        AppState::default(),
        Action::SearchSubmitted {
            query: "duplicates".to_owned(),
            filter: SearchFilter::All,
        },
    );
    let [Effect::Search { generation, .. }] = effects.as_slice() else {
        panic!("search fixture must load");
    };
    let (state, _) = reduce(
        state,
        Action::SearchCompleted {
            generation: *generation,
            result: Ok(SearchPage::new(vec![
                SearchItem::Playable(first.clone()),
                SearchItem::Metadata(metadata),
                SearchItem::Playable(duplicate),
                SearchItem::Playable(other_provider.clone()),
                SearchItem::Playable(selected.clone()),
            ])),
        },
    );
    let (state, _) = reduce(
        state,
        Action::SearchSelectionChanged {
            id: SearchItem::Playable(selected.clone()).stable_id(),
        },
    );
    let (_, actions) = reduce_key(
        UiController::default()
            .with_view(NavigationItem::Search)
            .with_focus(FocusRegion::Content),
        &state,
        key(KeyCode::Enter),
    );
    assert_eq!(
        actions,
        vec![Action::PlayMediaList {
            items: vec![first, other_provider, selected.clone()],
            selected_id: selected.id,
            shuffle_seed: None,
        }]
    );
}

#[test]
fn explicit_list_seed_is_consumed_only_by_valid_shuffled_activation() {
    let item = song("explicit-seed", "Explicit Seed");
    let (state, effects) = reduce(
        AppState::default(),
        Action::SearchSubmitted {
            query: "seed".to_owned(),
            filter: SearchFilter::Songs,
        },
    );
    let [Effect::Search { generation, .. }] = effects.as_slice() else {
        panic!("search fixture must load");
    };
    let (state, _) = reduce(
        state,
        Action::SearchCompleted {
            generation: *generation,
            result: Ok(SearchPage::new(vec![SearchItem::Playable(item.clone())])),
        },
    );
    let (shuffled, _) = reduce(
        state.clone(),
        Action::ShuffleEnabledChanged {
            enabled: true,
            seed: 1,
        },
    );
    let controller = UiController::default()
        .with_view(NavigationItem::Search)
        .with_focus(FocusRegion::Content)
        .with_shuffle_seed(41);
    let (controller, first) = reduce_key(controller, &shuffled, key(KeyCode::Enter));
    let (_, second) = reduce_key(controller, &shuffled, key(KeyCode::Enter));
    assert!(matches!(
        first.as_slice(),
        [Action::PlayMediaList {
            shuffle_seed: Some(41),
            ..
        }]
    ));
    assert!(matches!(
        second.as_slice(),
        [Action::PlayMediaList {
            shuffle_seed: Some(42),
            ..
        }]
    ));

    let controller = UiController::default()
        .with_view(NavigationItem::Search)
        .with_focus(FocusRegion::Content)
        .with_shuffle_seed(41);
    let (controller, unshuffled) = reduce_key(controller, &state, key(KeyCode::Enter));
    assert!(matches!(
        unshuffled.as_slice(),
        [Action::PlayMediaList {
            shuffle_seed: None,
            ..
        }]
    ));
    let (_, shuffle) = reduce_key(controller, &state, plain('s'));
    assert_eq!(
        shuffle,
        vec![Action::ShuffleEnabledChanged {
            enabled: true,
            seed: 41,
        }]
    );

    let invalid = UiController::default()
        .with_view(NavigationItem::Settings)
        .with_focus(FocusRegion::Content)
        .with_shuffle_seed(41);
    let (invalid, actions) = reduce_key(invalid, &shuffled, key(KeyCode::Enter));
    assert!(actions.is_empty());
    let (_, shuffle) = reduce_key(invalid, &AppState::default(), plain('s'));
    assert_eq!(
        shuffle,
        vec![Action::ShuffleEnabledChanged {
            enabled: true,
            seed: 41,
        }]
    );
}

#[test]
fn explicit_list_keeps_queue_activation_direct_and_explicit_enqueue_append_only() {
    let first = song("explicit-queue-first", "Explicit Queue First");
    let second = song("explicit-queue-second", "Explicit Queue Second");
    let (state, _) = reduce(
        AppState::default(),
        Action::EnqueueMedia {
            item: first.clone(),
        },
    );
    let (state, _) = reduce(
        state,
        Action::EnqueueMedia {
            item: second.clone(),
        },
    );
    assert_eq!(state.queue().active_ids().len(), 2);
    let (_, actions) = reduce_key(
        UiController::default().with_focus(FocusRegion::Queue),
        &state,
        key(KeyCode::Enter),
    );
    assert_eq!(
        actions,
        vec![Action::PlayQueueItem {
            id: stable_queue_item_id(&first.id),
        }]
    );
}

#[test]
fn explicit_list_nonplayable_library_selection_emits_no_action_or_seed() {
    let (state, _) = reduce(
        AppState::default(),
        Action::AuthenticationChanged(AuthenticationState::Authenticated),
    );
    let (state, effects) = reduce(
        state,
        Action::LibraryRequested {
            section: LibrarySection::Playlists,
        },
    );
    let [Effect::LoadLibrary { generation, .. }] = effects.as_slice() else {
        panic!("library fixture must load");
    };
    let metadata = ytermusic::provider::BrowseItem {
        id: "playlist".to_owned(),
        title: "Playlist".to_owned(),
        subtitle: None,
        artwork_url: None,
    };
    let (state, _) = reduce(
        state,
        Action::LibraryCompleted {
            generation: *generation,
            result: Ok(Page {
                items: vec![LibraryItem::Playlist(metadata)],
                continuation: None,
                stale: false,
            }),
        },
    );
    let controller = UiController::default()
        .with_view(NavigationItem::Library)
        .with_focus(FocusRegion::Content)
        .with_shuffle_seed(81);
    let (controller, actions) = reduce_key(controller, &state, key(KeyCode::Enter));
    assert!(actions.is_empty());
    let (_, actions) = reduce_key(controller, &state, plain('s'));
    assert_eq!(
        actions,
        vec![Action::ShuffleEnabledChanged {
            enabled: true,
            seed: 81,
        }]
    );
}

#[test]
fn explicit_list_home_content_activation_does_not_replay_the_queue() {
    let queued = song("home-queued", "Home Queued");
    let (state, _) = reduce(
        AppState::default(),
        Action::EnqueueMedia {
            item: queued.clone(),
        },
    );
    let (state, _) = reduce(
        state,
        Action::PlayQueueItem {
            id: stable_queue_item_id(&queued.id),
        },
    );
    assert!(state.queue().current().is_some());

    let (_, actions) = reduce_key(
        UiController::default()
            .with_view(NavigationItem::Home)
            .with_focus(FocusRegion::Content),
        &state,
        key(KeyCode::Enter),
    );

    assert!(actions.is_empty());
}

#[test]
fn explicit_list_library_activation_includes_playable_rows_from_all_loaded_pages() {
    let first = song("library-page-one", "Library Page One");
    let selected = song("library-page-two", "Library Page Two");
    let metadata = ytermusic::provider::BrowseItem {
        id: "library-metadata".to_owned(),
        title: "Library Metadata".to_owned(),
        subtitle: None,
        artwork_url: None,
    };
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
        panic!("initial library page must load");
    };
    let (state, _) = reduce(
        state,
        Action::LibraryCompleted {
            generation: *generation,
            result: Ok(Page {
                items: vec![
                    LibraryItem::Playable(first.clone()),
                    LibraryItem::Playlist(metadata),
                ],
                continuation: Some("library-page-two-token".to_owned()),
                stale: false,
            }),
        },
    );
    let (state, effects) = reduce(state, Action::LibraryMoreRequested);
    let [
        Effect::LoadLibrary {
            generation,
            continuation: Some(continuation),
            ..
        },
    ] = effects.as_slice()
    else {
        panic!("library continuation must load");
    };
    assert_eq!(continuation.as_str(), "library-page-two-token");
    let (state, _) = reduce(
        state,
        Action::LibraryCompleted {
            generation: *generation,
            result: Ok(Page {
                items: vec![LibraryItem::Playable(selected.clone())],
                continuation: None,
                stale: false,
            }),
        },
    );
    let (state, _) = reduce(
        state,
        Action::LibrarySelectionChanged {
            id: ytermusic::app::stable_library_item_id(&LibraryItem::Playable(selected.clone())),
        },
    );

    let (_, actions) = reduce_key(
        UiController::default()
            .with_view(NavigationItem::Library)
            .with_focus(FocusRegion::Content),
        &state,
        key(KeyCode::Enter),
    );

    assert_eq!(
        actions,
        vec![Action::PlayMediaList {
            items: vec![first, selected.clone()],
            selected_id: selected.id,
            shuffle_seed: None,
        }]
    );
}

fn loaded_shuffled_chart(items: &[MediaItem], selected_index: usize) -> AppState {
    let chart_region = region("GB");
    let (state, effects) = reduce(
        AppState::default(),
        Action::ChartsRequested {
            region: chart_region.clone(),
        },
    );
    let [
        Effect::ReadChartCache { generation, .. },
        Effect::LoadCharts { .. },
    ] = effects.as_slice()
    else {
        panic!("chart fixture must load");
    };
    let sections = items
        .chunks(MAX_ITEMS_PER_SHELF)
        .enumerate()
        .map(|(index, items)| ChartSection::new(format!("Oversized {index}"), items.to_vec()))
        .collect();
    let (state, _) = reduce(
        state,
        Action::ChartsCompleted {
            generation: *generation,
            region: chart_region,
            received_at: 1,
            result: Ok(sections),
        },
    );
    let (state, _) = reduce(
        state,
        Action::ChartRowSelectionChanged {
            item_index: selected_index,
        },
    );
    reduce(
        state,
        Action::ShuffleEnabledChanged {
            enabled: true,
            seed: 1,
        },
    )
    .0
}

#[test]
fn explicit_list_oversized_chart_caps_payload_and_preserves_shuffle_seed() {
    let items = (0..=MAX_EXPLICIT_LIST_ITEMS)
        .map(|index| song(&format!("oversized-{index}"), &format!("Oversized {index}")))
        .collect::<Vec<_>>();
    let selected_id = items[MAX_EXPLICIT_LIST_ITEMS].id.clone();
    let oversized = loaded_shuffled_chart(&items, MAX_EXPLICIT_LIST_ITEMS);
    let controller = UiController::default()
        .with_view(NavigationItem::Charts)
        .with_focus(FocusRegion::Content)
        .with_shuffle_seed(301);

    let (controller, actions) = reduce_key(controller, &oversized, key(KeyCode::Enter));
    let [
        Action::PlayMediaList {
            items,
            selected_id: actual_selected,
            shuffle_seed,
        },
    ] = actions.as_slice()
    else {
        panic!("oversized chart must emit one explicit-list action");
    };
    assert_eq!(items.len(), MAX_EXPLICIT_LIST_ITEMS + 1);
    assert_eq!(actual_selected, &selected_id);
    assert_eq!(*shuffle_seed, Some(301));

    let valid_item = song("valid-after-oversized", "Valid After Oversized");
    let valid = loaded_shuffled_chart(std::slice::from_ref(&valid_item), 0);
    let (_, actions) = reduce_key(
        controller
            .with_view(NavigationItem::Charts)
            .with_focus(FocusRegion::Content),
        &valid,
        key(KeyCode::Enter),
    );
    assert_eq!(
        actions,
        vec![Action::PlayMediaList {
            items: vec![valid_item.clone()],
            selected_id: valid_item.id,
            shuffle_seed: Some(301),
        }]
    );
}

#[test]
fn explicit_list_oversized_chart_finds_selection_after_retained_payload() {
    let items = (0..=(MAX_EXPLICIT_LIST_ITEMS + 1))
        .map(|index| {
            song(
                &format!("selected-after-bound-{index}"),
                &format!("Selected After Bound {index}"),
            )
        })
        .collect::<Vec<_>>();
    let selected_index = MAX_EXPLICIT_LIST_ITEMS + 1;
    let selected_id = items[selected_index].id.clone();
    let state = loaded_shuffled_chart(&items, selected_index);

    let (_, actions) = reduce_key(
        UiController::default()
            .with_view(NavigationItem::Charts)
            .with_focus(FocusRegion::Content)
            .with_shuffle_seed(401),
        &state,
        key(KeyCode::Enter),
    );
    let [
        Action::PlayMediaList {
            items,
            selected_id: actual_selected,
            shuffle_seed,
        },
    ] = actions.as_slice()
    else {
        panic!("oversized chart must emit one explicit-list action");
    };
    assert_eq!(items.len(), MAX_EXPLICIT_LIST_ITEMS + 1);
    assert!(!items.iter().any(|item| item.id == selected_id));
    assert_eq!(actual_selected, &selected_id);
    assert_eq!(*shuffle_seed, Some(401));
}

#[test]
fn account_shortcut_opens_bounded_browser_picker_and_confirms_exact_choice() {
    let (mut controller, actions) =
        reduce_key(UiController::default(), &AppState::default(), plain('a'));
    assert!(actions.is_empty());
    assert_eq!(controller.model().overlay, Some(Overlay::BrowserPicker));
    assert_eq!(controller.model().browser_picker.choices(), &Browser::ALL);

    for _ in 1..Browser::ALL.len() {
        (controller, _) = reduce_key(controller, &AppState::default(), key(KeyCode::Down));
    }
    let (_, actions) = reduce_key(controller, &AppState::default(), key(KeyCode::Enter));
    assert_eq!(
        actions,
        vec![Action::ConnectAccountRequested {
            browser: Browser::Vivaldi,
        }]
    );
    let (_, effects) = apply_actions(AppState::default(), actions);
    assert_eq!(
        effects,
        vec![Effect::ConnectAccount {
            browser: Browser::Vivaldi,
        }]
    );
}

#[test]
fn cancelling_browser_picker_is_a_no_op() {
    let (controller, _) = reduce_key(UiController::default(), &AppState::default(), plain('a'));
    let (controller, actions) = reduce_key(controller, &AppState::default(), key(KeyCode::Esc));

    assert_eq!(controller.model().overlay, None);
    assert!(actions.is_empty());
}

#[test]
fn navigation_activation_requests_initial_library_and_history_loads_once() {
    let (mut state, _) = reduce(
        AppState::default(),
        Action::AuthenticationChanged(AuthenticationState::Authenticated),
    );

    let library = UiController::default()
        .with_view(NavigationItem::Library)
        .with_focus(FocusRegion::Navigation);
    let (library, actions) = reduce_key(library, &state, key(KeyCode::Enter));
    assert_eq!(library.model().focus, FocusRegion::Content);
    assert_eq!(
        actions,
        vec![
            Action::LibraryRequested {
                section: LibrarySection::Songs,
            },
            Action::ArtworkSurfaceChanged {
                surface: ArtworkSurface::Library,
            },
        ]
    );
    (state, _) = apply_actions(state, actions);

    let (library, _) = reduce_key(library, &state, key(KeyCode::Left));
    let (_, actions) = reduce_key(library, &state, key(KeyCode::Enter));
    assert!(
        !actions
            .iter()
            .any(|action| matches!(action, Action::LibraryRequested { .. }))
    );

    let history = UiController::default()
        .with_view(NavigationItem::History)
        .with_focus(FocusRegion::Navigation);
    let (history, actions) = reduce_key(history, &state, key(KeyCode::Enter));
    assert_eq!(history.model().focus, FocusRegion::Content);
    assert_eq!(
        actions,
        vec![
            Action::HistoryRequested,
            Action::ArtworkSurfaceChanged {
                surface: ArtworkSurface::History,
            },
        ]
    );
    (state, _) = apply_actions(state, actions);

    let (history, _) = reduce_key(history, &state, key(KeyCode::Left));
    let (_, actions) = reduce_key(history, &state, key(KeyCode::Enter));
    assert!(!actions.contains(&Action::HistoryRequested));

    let anonymous_library = UiController::default()
        .with_view(NavigationItem::Library)
        .with_focus(FocusRegion::Navigation);
    let (anonymous_library, actions) =
        reduce_key(anonymous_library, &AppState::default(), key(KeyCode::Enter));
    assert_eq!(anonymous_library.model().focus, FocusRegion::Content);
    assert_eq!(
        actions,
        vec![Action::ArtworkSurfaceChanged {
            surface: ArtworkSurface::Library,
        }]
    );
}

#[test]
fn favorites_navigation_requests_only_the_initial_load() {
    let controller = UiController::default()
        .with_view(NavigationItem::Favorites)
        .with_focus(FocusRegion::Navigation);
    let (controller, actions) = reduce_key(controller, &AppState::default(), key(KeyCode::Enter));
    assert_eq!(
        actions,
        vec![
            Action::FavoritesRequested,
            Action::ArtworkSurfaceChanged {
                surface: ArtworkSurface::Favorites,
            },
        ]
    );
    let (loading, _) = apply_actions(AppState::default(), actions);
    let (_, actions) = reduce_key(
        controller
            .with_view(NavigationItem::Favorites)
            .with_focus(FocusRegion::Navigation),
        &loading,
        key(KeyCode::Enter),
    );
    assert_eq!(
        actions,
        vec![Action::ArtworkSurfaceChanged {
            surface: ArtworkSurface::Favorites,
        }]
    );

    let loaded = loaded_favorites(Vec::new());
    let (_, actions) = reduce_key(
        UiController::default()
            .with_view(NavigationItem::Favorites)
            .with_focus(FocusRegion::Navigation),
        &loaded,
        key(KeyCode::Enter),
    );
    assert_eq!(
        actions,
        vec![Action::ArtworkSurfaceChanged {
            surface: ArtworkSurface::Favorites,
        }]
    );
}

#[test]
fn entering_loaded_favorites_resynchronizes_its_selected_artwork() -> TestResult {
    let mut favorite = song("favorite-navigation-art", "Favorite Navigation Art");
    let favorite_art = url::Url::parse("https://example.com/favorite-navigation.jpg")?;
    favorite.artwork_url = Some(favorite_art.clone());
    let mut state = loaded_favorites(vec![favorite.clone()]);

    let mut search = song("search-navigation-art", "Search Navigation Art");
    search.artwork_url = Some(url::Url::parse(
        "https://example.com/search-navigation.jpg",
    )?);
    let (next, effects) = reduce(
        state,
        Action::SearchSubmitted {
            query: "artwork owner".to_owned(),
            filter: SearchFilter::Songs,
        },
    );
    state = next;
    let [Effect::Search { generation, .. }] = effects.as_slice() else {
        panic!("search fixture must load");
    };
    let (next, _) = reduce(
        state,
        Action::SearchCompleted {
            generation: *generation,
            result: Ok(SearchPage::new(vec![SearchItem::Playable(search)])),
        },
    );
    state = next;

    let controller = UiController::default()
        .with_view(NavigationItem::Favorites)
        .with_focus(FocusRegion::Navigation);
    let (_, actions) = reduce_key(controller, &state, key(KeyCode::Enter));
    assert_eq!(
        actions,
        vec![Action::ArtworkSurfaceChanged {
            surface: ArtworkSurface::Favorites,
        }]
    );
    let (_, effects) = apply_actions(state, actions);
    assert!(matches!(
        effects.as_slice(),
        [Effect::FetchArtwork { url, .. }] if url.as_url() == &favorite_art
    ));
    Ok(())
}

fn favorite_artwork_state() -> Result<(AppState, MediaItem), Box<dyn Error>> {
    let mut favorite = song("favorite-exit-art", "Favorite Exit Art");
    favorite.artwork_url = Some(url::Url::parse("https://example.com/favorite-exit.jpg")?);
    let state = reduce(
        loaded_favorites(vec![favorite.clone()]),
        Action::ArtworkSurfaceChanged {
            surface: ArtworkSurface::Favorites,
        },
    )
    .0;
    Ok((state, favorite))
}

#[test]
fn cycling_from_favorites_to_home_and_settings_clears_favorite_artwork() -> TestResult {
    for (destination, inputs) in [
        (
            NavigationItem::Settings,
            vec![key(KeyCode::Right), key(KeyCode::Right)],
        ),
        (NavigationItem::Home, vec![key(KeyCode::Left); 5]),
    ] {
        let (mut state, _) = favorite_artwork_state()?;
        let mut controller = UiController::default()
            .with_view(NavigationItem::Favorites)
            .with_focus(FocusRegion::Navigation);
        let mut all_effects = Vec::new();
        for input in inputs {
            let (next_controller, actions) = reduce_key(controller, &state, input);
            controller = next_controller;
            let (next_state, effects) = apply_actions(state, actions);
            state = next_state;
            all_effects.extend(effects);
        }
        assert_eq!(controller.model().view, destination);
        assert!(
            all_effects
                .iter()
                .any(|effect| matches!(effect, Effect::ClearArtwork)),
            "{destination:?} did not clear favorite artwork: {all_effects:?}"
        );
        assert!(state.artwork().requested_url().is_none());
    }
    Ok(())
}

#[test]
fn mouse_navigation_from_favorites_to_home_and_settings_clears_favorite_artwork() -> TestResult {
    for destination in [NavigationItem::Home, NavigationItem::Settings] {
        let (state, _) = favorite_artwork_state()?;
        let store = interaction_store(HitTarget::Navigation(destination));
        let (controller, actions) = reduce_mouse(
            UiController::default()
                .with_view(NavigationItem::Favorites)
                .with_focus(FocusRegion::Content),
            &state,
            mouse(MouseEventKind::Down(MouseButton::Left), 3, 4),
            store.latest(),
        );
        assert_eq!(controller.model().view, destination);
        let (state, effects) = apply_actions(state, actions);
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, Effect::ClearArtwork)),
            "{destination:?} did not clear favorite artwork: {effects:?}"
        );
        assert!(state.artwork().requested_url().is_none());
    }
    Ok(())
}

#[test]
fn leaving_favorites_during_load_prevents_late_completion_from_claiming_artwork() -> TestResult {
    let controller = UiController::default()
        .with_view(NavigationItem::Favorites)
        .with_focus(FocusRegion::Navigation);
    let (controller, actions) = reduce_key(controller, &AppState::default(), key(KeyCode::Enter));
    let (state, _) = apply_actions(AppState::default(), actions);
    let generation = state.favorites().generation();

    let store = interaction_store(HitTarget::Navigation(NavigationItem::Home));
    let (_, actions) = reduce_mouse(
        controller,
        &state,
        mouse(MouseEventKind::Down(MouseButton::Left), 3, 4),
        store.latest(),
    );
    let (state, _) = apply_actions(state, actions);
    let mut favorite = song("late-favorite-art", "Late Favorite Art");
    favorite.artwork_url = Some(url::Url::parse("https://example.com/late-favorite.jpg")?);
    let (state, effects) = reduce(
        state,
        Action::FavoritesCompleted {
            generation,
            result: Ok(vec![FavoriteEntry {
                id: 1,
                item: favorite,
                favorited_at: 1,
            }]),
        },
    );

    assert!(effects.is_empty(), "late completion effects: {effects:?}");
    assert!(state.artwork().requested_url().is_none());
    Ok(())
}

#[test]
fn slash_navigation_from_favorites_revokes_favorite_artwork_ownership() -> TestResult {
    let (state, _) = favorite_artwork_state()?;
    let controller = UiController::default()
        .with_view(NavigationItem::Favorites)
        .with_focus(FocusRegion::Content);

    let (controller, actions) = reduce_key(controller, &state, plain('/'));

    assert_eq!(controller.model().view, NavigationItem::Search);
    assert!(actions.contains(&Action::ArtworkSurfaceChanged {
        surface: ArtworkSurface::Search,
    }));
    let (state, effects) = apply_actions(state, actions);
    assert!(state.artwork().requested_url().is_none());
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, Effect::ClearArtwork))
    );
    Ok(())
}

#[test]
fn vertical_navigation_from_favorites_establishes_destination_artwork() -> TestResult {
    for (input, destination) in [
        (key(KeyCode::Up), NavigationItem::Library),
        (key(KeyCode::Down), NavigationItem::History),
    ] {
        let (state, _) = favorite_artwork_state()?;
        let controller = UiController::default()
            .with_view(NavigationItem::Favorites)
            .with_focus(FocusRegion::Navigation);

        let (controller, actions) = reduce_key(controller, &state, input);

        assert_eq!(controller.model().view, destination);
        assert!(actions.contains(&Action::ArtworkSurfaceChanged {
            surface: artwork_surface(destination),
        }));
        let (state, effects) = apply_actions(state, actions);
        assert!(state.artwork().requested_url().is_none());
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, Effect::ClearArtwork)),
            "{destination:?}: {effects:?}"
        );
    }
    Ok(())
}

#[test]
fn reentering_favorites_after_mutation_error_restores_retained_selection_artwork() -> TestResult {
    let (state, favorite) = favorite_artwork_state()?;
    let expected = ArtworkUrl::try_from(
        favorite
            .artwork_url
            .clone()
            .ok_or("favorite artwork fixture")?,
    )?;
    let (state, _) = reduce(
        state,
        Action::ArtworkSurfaceChanged {
            surface: ArtworkSurface::Home,
        },
    );
    let (state, effects) = reduce(
        state,
        Action::FavoriteToggleRequested {
            item: favorite.clone(),
        },
    );
    let [Effect::RemoveFavorite { generation, .. }] = effects.as_slice() else {
        panic!("favorite removal fixture");
    };
    let (state, _) = reduce(
        state,
        Action::FavoriteMutationCompleted {
            generation: *generation,
            media_id: favorite.id.clone(),
            mutation: ytermusic::app::FavoriteMutation::Remove,
            result: Err(ytermusic::app::AppError::new(
                ytermusic::app::AppErrorCategory::Favorites,
                "favorite storage unavailable",
            )),
        },
    );
    assert_eq!(state.favorites().selected_id(), Some(&favorite.id));
    assert!(state.favorites().error().is_some());

    let (_, actions) = reduce_key(
        UiController::default()
            .with_view(NavigationItem::Favorites)
            .with_focus(FocusRegion::Navigation),
        &state,
        key(KeyCode::Enter),
    );
    let (state, effects) = apply_actions(state, actions);

    assert_eq!(state.artwork_surface(), ArtworkSurface::Favorites);
    assert_eq!(state.artwork().requested_url(), Some(&expected));
    assert!(matches!(
        effects.as_slice(),
        [Effect::FetchArtwork { url, .. }] if url == &expected
    ));
    Ok(())
}

#[test]
fn entering_failed_empty_favorites_clears_stale_artwork() {
    let old_artwork = ArtworkUrl::try_from(
        url::Url::parse("https://example.com/failed-empty-stale.jpg")
            .unwrap_or_else(|error| panic!("stale artwork URL: {error}")),
    )
    .unwrap_or_else(|error| panic!("stale artwork fixture: {error}"));
    let (state, _) = reduce(
        AppState::default(),
        Action::ArtworkRequested { url: old_artwork },
    );
    let (state, effects) = reduce(state, Action::FavoritesRequested);
    let [Effect::LoadFavorites { generation }] = effects.as_slice() else {
        panic!("favorites load fixture");
    };
    let (state, _) = reduce(
        state,
        Action::FavoritesCompleted {
            generation: *generation,
            result: Err(ytermusic::app::AppError::new(
                ytermusic::app::AppErrorCategory::Favorites,
                "favorites unavailable",
            )),
        },
    );

    let (_, actions) = reduce_key(
        UiController::default()
            .with_view(NavigationItem::Favorites)
            .with_focus(FocusRegion::Navigation),
        &state,
        key(KeyCode::Enter),
    );
    let (state, effects) = apply_actions(state, actions);

    assert_eq!(state.artwork_surface(), ArtworkSurface::Favorites);
    assert!(state.artwork().requested_url().is_none());
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, Effect::ClearArtwork))
    );
}

fn with_stale_navigation_artwork(state: AppState) -> AppState {
    let url = url::Url::parse("https://example.com/stale-navigation.jpg")
        .unwrap_or_else(|error| panic!("stale artwork URL: {error}"));
    let artwork = ArtworkUrl::try_from(url)
        .unwrap_or_else(|error| panic!("valid stale artwork URL: {error}"));
    reduce(state, Action::ArtworkRequested { url: artwork }).0
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one navigation matrix audits artwork ownership for every top-level destination"
)]
fn every_navigation_destination_establishes_selected_or_clear_artwork() -> TestResult {
    let artwork_item = |id: &str, title: &str, url: &str| -> Result<MediaItem, Box<dyn Error>> {
        let mut item = song(id, title);
        item.artwork_url = Some(url::Url::parse(url)?);
        Ok(item)
    };
    let search = artwork_item(
        "navigation-search-art",
        "Navigation Search Art",
        "https://example.com/navigation-search.jpg",
    )?;
    let chart = artwork_item(
        "navigation-chart-art",
        "Navigation Chart Art",
        "https://example.com/navigation-chart.jpg",
    )?;
    let library = artwork_item(
        "navigation-library-art",
        "Navigation Library Art",
        "https://example.com/navigation-library.jpg",
    )?;
    let history = artwork_item(
        "navigation-history-art",
        "Navigation History Art",
        "https://example.com/navigation-history.jpg",
    )?;
    let favorite = artwork_item(
        "navigation-favorite-art",
        "Navigation Favorite Art",
        "https://example.com/navigation-favorite.jpg",
    )?;
    let podcast_url = url::Url::parse("https://example.com/navigation-podcast.jpg")?;

    let cases = vec![
        ("home", NavigationItem::Home, AppState::default(), None),
        (
            "search",
            NavigationItem::Search,
            selected_search_playable(&search),
            search.artwork_url.clone(),
        ),
        (
            "charts",
            NavigationItem::Charts,
            selected_chart_playable(&chart),
            chart.artwork_url.clone(),
        ),
        (
            "podcasts",
            NavigationItem::Podcasts,
            opened_podcast_state_with_artwork(Some(podcast_url.clone())),
            Some(podcast_url),
        ),
        (
            "library loading",
            NavigationItem::Library,
            selected_library_playable(&library),
            None,
        ),
        (
            "favorites",
            NavigationItem::Favorites,
            loaded_favorites(vec![favorite.clone()]),
            favorite.artwork_url.clone(),
        ),
        (
            "history loading",
            NavigationItem::History,
            selected_history_playable(&history),
            None,
        ),
        (
            "settings",
            NavigationItem::Settings,
            AppState::default(),
            None,
        ),
    ];

    for (name, view, state, expected) in cases {
        for mouse_path in [false, true] {
            let state = with_stale_navigation_artwork(state.clone());
            let actions = if mouse_path {
                let store = interaction_store(HitTarget::Navigation(view));
                reduce_mouse(
                    UiController::default()
                        .with_view(NavigationItem::Favorites)
                        .with_focus(FocusRegion::Content),
                    &state,
                    mouse(MouseEventKind::Down(MouseButton::Left), 3, 4),
                    store.latest(),
                )
                .1
            } else {
                reduce_key(
                    UiController::default()
                        .with_view(view)
                        .with_focus(FocusRegion::Navigation),
                    &state,
                    key(KeyCode::Enter),
                )
                .1
            };
            let (state, _) = apply_actions(state, actions);
            assert_eq!(
                state.artwork().requested_url().map(ArtworkUrl::as_url),
                expected.as_ref(),
                "{name}, mouse={mouse_path}"
            );
        }
    }
    Ok(())
}

#[test]
fn empty_and_loading_favorites_navigation_clear_stale_artwork() {
    let empty = loaded_favorites(Vec::new());
    let loading = reduce(AppState::default(), Action::FavoritesRequested).0;

    for (name, state) in [("empty", empty), ("loading", loading)] {
        let state = with_stale_navigation_artwork(state);
        let (_, actions) = reduce_key(
            UiController::default()
                .with_view(NavigationItem::Favorites)
                .with_focus(FocusRegion::Navigation),
            &state,
            key(KeyCode::Enter),
        );
        let (state, effects) = apply_actions(state, actions);
        assert!(state.artwork().requested_url().is_none(), "{name}");
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, Effect::ClearArtwork)),
            "{name}: {effects:?}"
        );
    }
}

#[test]
fn horizontal_navigation_switches_and_wraps_views_without_leaving_navigation() {
    let state = AppState::default();

    for (start, input, expected) in [
        (
            NavigationItem::Home,
            key(KeyCode::Right),
            NavigationItem::Search,
        ),
        (NavigationItem::Home, plain('l'), NavigationItem::Search),
        (
            NavigationItem::Search,
            key(KeyCode::Left),
            NavigationItem::Home,
        ),
        (NavigationItem::Search, plain('h'), NavigationItem::Home),
        (
            NavigationItem::Settings,
            key(KeyCode::Right),
            NavigationItem::Home,
        ),
        (NavigationItem::Settings, plain('l'), NavigationItem::Home),
        (
            NavigationItem::Home,
            key(KeyCode::Left),
            NavigationItem::Settings,
        ),
        (NavigationItem::Home, plain('h'), NavigationItem::Settings),
    ] {
        let controller = UiController::default()
            .with_view(start)
            .with_focus(FocusRegion::Navigation);
        let (controller, actions) = reduce_key(controller, &state, input);

        assert_eq!(controller.model().view, expected);
        assert_eq!(controller.model().focus, FocusRegion::Navigation);
        assert_eq!(
            actions,
            vec![Action::ArtworkSurfaceChanged {
                surface: artwork_surface(expected),
            }]
        );
    }
}

#[test]
fn horizontal_navigation_dispatches_lazy_loads_for_entered_views() {
    let (state, _) = reduce(
        AppState::default(),
        Action::AuthenticationChanged(AuthenticationState::Authenticated),
    );
    let cases = [
        (
            NavigationItem::Charts,
            NavigationItem::Podcasts,
            Action::PodcastRecommendationsRequested {
                region: state.podcasts().requested_region().clone(),
            },
        ),
        (
            NavigationItem::Podcasts,
            NavigationItem::Library,
            Action::LibraryRequested {
                section: LibrarySection::Songs,
            },
        ),
        (
            NavigationItem::Library,
            NavigationItem::Favorites,
            Action::FavoritesRequested,
        ),
        (
            NavigationItem::Favorites,
            NavigationItem::History,
            Action::HistoryRequested,
        ),
    ];

    for (start, expected_view, expected_action) in cases {
        let controller = UiController::default()
            .with_view(start)
            .with_focus(FocusRegion::Navigation);
        let (controller, actions) = reduce_key(controller, &state, key(KeyCode::Right));

        assert_eq!(controller.model().view, expected_view);
        assert_eq!(controller.model().focus, FocusRegion::Navigation);
        assert_eq!(
            actions,
            vec![
                expected_action,
                Action::ArtworkSurfaceChanged {
                    surface: artwork_surface(expected_view),
                },
            ]
        );
    }
}

#[test]
fn podcast_recommendation_navigation_requests_only_an_unloaded_idle_surface() {
    let us = region("US");
    let state = AppState::new(Config {
        region: us.clone(),
        ..Config::default()
    });
    let controller = UiController::default()
        .with_view(NavigationItem::Podcasts)
        .with_focus(FocusRegion::Navigation);

    let (controller, actions) = reduce_key(controller, &state, key(KeyCode::Enter));
    assert_eq!(controller.model().focus, FocusRegion::Content);
    assert_eq!(
        actions,
        vec![
            Action::PodcastRecommendationsRequested { region: us },
            Action::ArtworkSurfaceChanged {
                surface: ArtworkSurface::Podcasts,
            },
        ]
    );

    let (loading_state, _) = apply_actions(state, actions);
    let loading_controller = controller.with_focus(FocusRegion::Navigation);
    let (_, actions) = reduce_key(loading_controller, &loading_state, key(KeyCode::Enter));
    assert!(
        !actions
            .iter()
            .any(|action| matches!(action, Action::PodcastRecommendationsRequested { .. }))
    );

    let populated_state = podcast_recommendation_state();
    let populated_controller = UiController::default()
        .with_view(NavigationItem::Podcasts)
        .with_focus(FocusRegion::Navigation);
    let (_, actions) = reduce_key(populated_controller, &populated_state, key(KeyCode::Enter));
    assert!(
        !actions
            .iter()
            .any(|action| matches!(action, Action::PodcastRecommendationsRequested { .. }))
    );

    let opened_state = opened_podcast_state();
    let opened_controller = UiController::default()
        .with_view(NavigationItem::Podcasts)
        .with_focus(FocusRegion::Navigation);
    let (_, actions) = reduce_key(opened_controller, &opened_state, key(KeyCode::Enter));
    assert!(
        !actions
            .iter()
            .any(|action| matches!(action, Action::PodcastRecommendationsRequested { .. }))
    );
}

#[test]
fn podcast_recommendation_navigation_waits_for_manual_show_loading() {
    let metadata = ytermusic::app::SearchMetadata::new(
        ytermusic::app::SearchMetadataKind::Podcast,
        "Loading Show",
    )
    .with_provider_id("loading-show");
    let (state, effects) = reduce(
        AppState::default(),
        Action::SearchSubmitted {
            query: "loading show".to_owned(),
            filter: SearchFilter::Podcasts,
        },
    );
    let [Effect::Search { generation, .. }] = effects.as_slice() else {
        panic!("manual podcast fixture search must load");
    };
    let (state, _) = reduce(
        state,
        Action::SearchCompleted {
            generation: *generation,
            result: Ok(SearchPage::new(vec![SearchItem::Metadata(metadata)])),
        },
    );
    let (state, effects) = reduce(state, Action::OpenSelectedPodcast);
    assert!(matches!(effects.as_slice(), [Effect::LoadPodcast { .. }]));
    assert!(state.podcasts().loading());
    assert!(state.podcasts().active_generation().is_some());
    assert!(state.podcasts().show().is_none());
    assert!(state.podcasts().recommendations().is_empty());

    let controller = UiController::default()
        .with_view(NavigationItem::Podcasts)
        .with_focus(FocusRegion::Navigation);
    let (controller, actions) = reduce_key(controller, &state, key(KeyCode::Enter));

    assert_eq!(controller.model().focus, FocusRegion::Content);
    assert!(
        !actions
            .iter()
            .any(|action| matches!(action, Action::PodcastRecommendationsRequested { .. }))
    );
}

#[test]
fn podcast_recommendation_selection_moves_by_stable_id_and_wraps() {
    let mut state = podcast_recommendation_state();
    let first = state.podcasts().recommendations()[0].source_id().clone();
    let second = state.podcasts().recommendations()[1].source_id().clone();
    let controller = UiController::default().with_view(NavigationItem::Podcasts);

    let (controller, actions) = reduce_key(controller, &state, key(KeyCode::Down));
    assert_eq!(
        actions,
        vec![Action::PodcastRecommendationSelectionChanged { id: second.clone() }]
    );
    (state, _) = apply_actions(state, actions);

    let (controller, actions) = reduce_key(controller, &state, key(KeyCode::Down));
    assert_eq!(
        actions,
        vec![Action::PodcastRecommendationSelectionChanged { id: first.clone() }]
    );
    (state, _) = apply_actions(state, actions);

    let (_, actions) = reduce_key(controller, &state, key(KeyCode::Up));
    assert_eq!(
        actions,
        vec![Action::PodcastRecommendationSelectionChanged { id: second }]
    );
}

#[test]
fn podcast_recommendation_submit_opens_once_while_matching() {
    let state = podcast_recommendation_state();
    let controller = UiController::default().with_view(NavigationItem::Podcasts);

    let (controller, actions) = reduce_key(controller, &state, key(KeyCode::Enter));
    assert_eq!(actions, vec![Action::OpenSelectedPodcastRecommendation]);
    let (matching_state, effects) = apply_actions(state, actions);
    assert!(matches!(
        effects.as_slice(),
        [Effect::ResolvePodcastRecommendation { .. }]
    ));

    let (_, actions) = reduce_key(controller, &matching_state, key(KeyCode::Enter));
    assert!(
        actions.is_empty(),
        "submitting while the selected recommendation is matching must not duplicate work"
    );
}

#[test]
fn podcast_recommendation_cancel_closes_only_an_unobscured_open_show() {
    let state = opened_podcast_state();
    let controller = UiController::default().with_view(NavigationItem::Podcasts);

    let (controller, actions) = reduce_key(controller, &state, key(KeyCode::Esc));
    assert_eq!(actions, vec![Action::ClosePodcast]);
    assert_eq!(controller.model().view, NavigationItem::Podcasts);
    assert!(!controller.quit_requested());

    let (covered, _) = reduce_key(
        UiController::default().with_view(NavigationItem::Podcasts),
        &state,
        plain('?'),
    );
    let (covered, actions) = reduce_key(covered, &state, key(KeyCode::Esc));
    assert!(actions.is_empty(), "the overlay must consume Escape first");
    assert_eq!(covered.model().overlay, None);

    let empty_controller = UiController::default().with_view(NavigationItem::Podcasts);
    let (empty_controller, actions) =
        reduce_key(empty_controller, &AppState::default(), key(KeyCode::Esc));
    assert!(actions.is_empty());
    assert_eq!(empty_controller.model().view, NavigationItem::Podcasts);
    assert!(!empty_controller.quit_requested());
}

#[test]
fn manual_podcast_search_activation_never_uses_recommendation_action() {
    let metadata = ytermusic::app::SearchMetadata::new(
        ytermusic::app::SearchMetadataKind::Podcast,
        "Manual Search Show",
    )
    .with_provider_id("manual-show");
    let (state, effects) = reduce(
        AppState::default(),
        Action::SearchSubmitted {
            query: "manual".to_owned(),
            filter: SearchFilter::Podcasts,
        },
    );
    let [Effect::Search { generation, .. }] = effects.as_slice() else {
        panic!("manual podcast search must load");
    };
    let (state, _) = reduce(
        state,
        Action::SearchCompleted {
            generation: *generation,
            result: Ok(SearchPage::new(vec![SearchItem::Metadata(metadata)])),
        },
    );
    let controller = UiController::default().with_view(NavigationItem::Search);

    let (_, actions) = reduce_key(controller, &state, key(KeyCode::Enter));
    assert_eq!(actions, vec![Action::OpenSelectedPodcast]);
    assert!(!actions.contains(&Action::OpenSelectedPodcastRecommendation));
}

#[test]
fn library_navigation_activation_does_not_supersede_a_continuation_load() {
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
        panic!("initial library request must load");
    };
    let (state, _) = reduce(
        state,
        Action::LibraryCompleted {
            generation: *generation,
            result: Ok(Page {
                items: vec![LibraryItem::Playable(song(
                    "library-continuation",
                    "Library Continuation",
                ))],
                continuation: Some("next-library-page".to_owned()),
                stale: false,
            }),
        },
    );
    let (state, effects) = reduce(state, Action::LibraryMoreRequested);
    assert!(matches!(
        effects.as_slice(),
        [Effect::LoadLibrary {
            continuation: Some(continuation),
            ..
        }] if continuation.as_str() == "next-library-page"
    ));
    assert!(!state.library().loading());
    assert!(state.library().loading_more());
    assert!(state.library().active_generation().is_some());

    let controller = UiController::default()
        .with_view(NavigationItem::Library)
        .with_focus(FocusRegion::Navigation);
    let (controller, actions) = reduce_key(controller, &state, key(KeyCode::Enter));

    assert_eq!(controller.model().focus, FocusRegion::Content);
    assert!(
        !actions
            .iter()
            .any(|action| matches!(action, Action::LibraryRequested { .. }))
    );
}

#[test]
fn real_search_keys_submit_then_activate_the_stable_result() -> TestResult {
    let mut state = AppState::default();
    let mut controller = UiController::default();

    (controller, _) = reduce_key(controller, &state, plain('/'));
    assert_eq!(controller.model().view, NavigationItem::Search);
    assert_eq!(
        controller.input_mode(),
        InputMode::TextEntry(TextEntryContext::Search)
    );

    for character in "midnight".chars() {
        (controller, _) = reduce_key(controller, &state, plain(character));
    }
    assert_eq!(controller.input_text(), "midnight");
    let mut draft_terminal = Terminal::new(TestBackend::new(90, 30))?;
    draft_terminal.draw(|frame| {
        render_with_model(frame, &state, &Theme::default(), controller.model());
    })?;
    assert!(draft_terminal.backend().to_string().contains("midnight"));

    let (next_controller, actions) = reduce_key(controller, &state, key(KeyCode::Enter));
    controller = next_controller;
    assert_eq!(
        actions,
        vec![Action::SearchSubmitted {
            query: "midnight".to_owned(),
            filter: SearchFilter::All,
        }]
    );
    let (next_state, effects) = apply_actions(state, actions);
    state = next_state;
    let [Effect::Search { generation, .. }] = effects.as_slice() else {
        panic!("search submit must reach the app reducer");
    };

    let item = song("controller-song", "Controller Song");
    (state, _) = reduce(
        state,
        Action::SearchCompleted {
            generation: *generation,
            result: Ok(SearchPage::new(vec![SearchItem::Playable(item.clone())])),
        },
    );
    let (next_controller, actions) = reduce_key(controller, &state, key(KeyCode::Enter));
    controller = next_controller;
    assert_eq!(
        actions,
        vec![Action::PlayMediaList {
            items: vec![item.clone()],
            selected_id: item.id.clone(),
            shuffle_seed: None,
        }]
    );
    let (state, effects) = apply_actions(state, actions);
    assert!(effects.iter().any(
        |effect| matches!(effect, Effect::Resolve { item: resolved, .. } if resolved == &item)
    ));

    let mut terminal = Terminal::new(TestBackend::new(90, 30))?;
    terminal.draw(|frame| {
        render_with_model(frame, &state, &Theme::default(), controller.model());
    })?;
    let rendered = terminal.backend().to_string();
    assert!(rendered.contains("Controller Song"));
    assert!(rendered.contains("Queue · 1"));
    Ok(())
}

#[test]
fn direct_key_and_palette_share_the_country_picker_dispatcher() -> TestResult {
    let hk = region("hk");
    let us = region("us");
    let state = AppState::new(Config {
        region: hk.clone(),
        ..Config::default()
    });

    let mut direct = UiController::default().with_view(NavigationItem::Charts);
    (direct, _) = reduce_key(direct, &state, plain('c'));
    assert_eq!(direct.model().overlay, Some(Overlay::CountryPicker));
    assert_eq!(direct.country_picker().selected_region(), &hk);

    let mut terminal = Terminal::new(TestBackend::new(90, 30))?;
    terminal.draw(|frame| {
        render_with_model(frame, &state, &Theme::default(), direct.model());
    })?;
    let rendered = terminal.backend().to_string();
    assert!(rendered.contains("Country picker"));
    assert!(rendered.contains("Hong Kong"));

    (direct, _) = reduce_key(direct, &state, key(KeyCode::Down));
    let (direct, direct_actions) = reduce_key(direct, &state, key(KeyCode::Enter));
    assert_eq!(direct.model().overlay, None);
    assert_eq!(
        direct_actions,
        vec![
            Action::ChartsRequested { region: us.clone() },
            Action::PodcastRecommendationsRequested { region: us.clone() },
        ]
    );

    let mut palette = UiController::default().with_view(NavigationItem::Charts);
    (palette, _) = reduce_key(palette, &state, plain(':'));
    assert_eq!(palette.model().overlay, Some(Overlay::CommandPalette));
    for character in "country".chars() {
        (palette, _) = reduce_key(palette, &state, plain(character));
    }
    (palette, _) = reduce_key(palette, &state, key(KeyCode::Enter));
    assert_eq!(palette.model().overlay, Some(Overlay::CountryPicker));
    assert_eq!(palette.country_picker().selected_region(), &hk);
    (palette, _) = reduce_key(palette, &state, plain('j'));
    let (palette, palette_actions) = reduce_key(palette, &state, key(KeyCode::Enter));
    assert_eq!(palette.model().overlay, None);
    assert_eq!(
        palette_actions,
        vec![
            Action::ChartsRequested { region: us.clone() },
            Action::PodcastRecommendationsRequested { region: us },
        ]
    );
    assert_eq!(palette_actions, direct_actions);
    Ok(())
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one end-to-end controller scenario verifies stable movement and activation across every content view"
)]
fn list_movement_and_activation_use_stable_app_actions() {
    let first = song("chart-first", "Chart First");
    let second = song("chart-second", "Chart Second");
    let region = region("GB");
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
        panic!("chart request must start a load");
    };
    let (state, _) = reduce(
        state,
        Action::ChartsCompleted {
            generation: *generation,
            region,
            received_at: 1_000,
            result: Ok(vec![ChartSection::new(
                "Top songs".to_owned(),
                vec![first.clone(), second.clone()],
            )]),
        },
    );

    let controller = UiController::default().with_view(NavigationItem::Charts);
    let (controller, actions) = reduce_key(controller, &state, key(KeyCode::Down));
    assert_eq!(
        actions,
        vec![Action::ChartRowSelectionChanged { item_index: 1 }]
    );
    let (state, _) = apply_actions(state, actions);
    let (_, actions) = reduce_key(controller, &state, key(KeyCode::Enter));
    assert_eq!(
        actions,
        vec![Action::PlayMediaList {
            items: vec![first, second.clone()],
            selected_id: second.id,
            shuffle_seed: None,
        }]
    );

    let episode_one = MediaItem {
        kind: MediaKind::PodcastEpisode,
        ..song("episode-one", "Episode One")
    };
    let episode_two = MediaItem {
        kind: MediaKind::PodcastEpisode,
        ..song("episode-two", "Episode Two")
    };
    let podcast = Podcast {
        id: "show".to_owned(),
        title: "Controller Show".to_owned(),
        creators: vec!["Host".to_owned()],
        description: None,
        artwork_url: None,
        episodes: vec![episode_one.clone(), episode_two.clone()],
    };
    let mut podcast_state = AppState::default();
    let metadata = ytermusic::app::SearchMetadata::new(
        ytermusic::app::SearchMetadataKind::Podcast,
        "Controller Show",
    )
    .with_provider_id("show");
    let (next, effects) = reduce(
        podcast_state,
        Action::SearchSubmitted {
            query: "show".to_owned(),
            filter: SearchFilter::Podcasts,
        },
    );
    podcast_state = next;
    let [Effect::Search { generation, .. }] = effects.as_slice() else {
        panic!("podcast fixture search must load");
    };
    (podcast_state, _) = reduce(
        podcast_state,
        Action::SearchCompleted {
            generation: *generation,
            result: Ok(SearchPage::new(vec![SearchItem::Metadata(metadata)])),
        },
    );
    let (next, effects) = reduce(podcast_state, Action::OpenSelectedPodcast);
    podcast_state = next;
    let [Effect::LoadPodcast { generation, .. }] = effects.as_slice() else {
        panic!("opening a podcast must load its episodes");
    };
    (podcast_state, _) = reduce(
        podcast_state,
        Action::PodcastCompleted {
            generation: *generation,
            result: Ok(podcast),
        },
    );
    let podcast_controller = UiController::default().with_view(NavigationItem::Podcasts);
    let (podcast_controller, actions) =
        reduce_key(podcast_controller, &podcast_state, key(KeyCode::Down));
    assert_eq!(
        actions,
        vec![Action::PodcastSelectionChanged {
            media_id: episode_two.id.clone(),
        }]
    );
    assert!(
        !actions
            .iter()
            .any(|action| matches!(action, Action::PlayPodcastEpisode { .. }))
    );
    let (podcast_state, _) = apply_actions(podcast_state, actions);
    let (_, actions) = reduce_key(podcast_controller, &podcast_state, key(KeyCode::Enter));
    assert_eq!(
        actions,
        vec![Action::PlayMediaList {
            items: vec![episode_one, episode_two.clone()],
            selected_id: episode_two.id,
            shuffle_seed: None,
        }]
    );
}

#[test]
fn chart_duplicate_media_ids_move_by_occurrence_instead_of_jumping_sections() {
    let coming = song("coming-of-age", "Coming Of Age Story");
    let duplicate = song("please-summer", "Please Summer!");
    let before_later_duplicate = song("before-later-duplicate", "Before Later Duplicate");
    let region = region("KR");
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
        panic!("chart request must start a load");
    };
    let (state, _) = reduce(
        state,
        Action::ChartsCompleted {
            generation: *generation,
            region,
            received_at: 1_000,
            result: Ok(vec![
                ChartSection::new(
                    "Daily Top Music Videos".to_owned(),
                    vec![coming, duplicate.clone()],
                ),
                ChartSection::new(
                    "Top 100 Music Videos".to_owned(),
                    vec![before_later_duplicate.clone(), duplicate],
                ),
            ]),
        },
    );
    let (state, _) = reduce(state, Action::ChartRowSelectionChanged { item_index: 3 });
    assert_eq!(state.charts().selected_index(), Some(3));

    let controller = UiController::default().with_view(NavigationItem::Charts);
    let (_, actions) = reduce_key(controller, &state, key(KeyCode::Up));
    assert_eq!(
        actions,
        vec![Action::ChartRowSelectionChanged { item_index: 2 }]
    );

    let (state, _) = apply_actions(state, actions);
    assert_eq!(state.charts().selected_index(), Some(2));
    assert_eq!(
        state.charts().selected_id(),
        Some(&before_later_duplicate.id)
    );
}

#[test]
fn playback_modes_volume_and_palette_use_one_dispatcher() {
    let state = AppState::new(Config {
        playback: ytermusic::config::PlaybackConfig {
            volume: 95,
            ..ytermusic::config::PlaybackConfig::default()
        },
        ..Config::default()
    });
    let controller = UiController::default();

    let (controller, actions) = reduce_key(controller, &state, plain('+'));
    assert_eq!(actions, vec![Action::TargetVolumeChanged(100)]);
    let (state, effects) = apply_actions(state, actions);
    assert_eq!(state.playback().target_volume, 100);
    assert!(effects.contains(&Effect::Player(PlayerCommand::Volume(100))));
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, Effect::Persist(_)))
    );

    let (controller, actions) = reduce_key(controller, &state, plain('s'));
    assert_eq!(
        actions,
        vec![Action::ShuffleEnabledChanged {
            enabled: true,
            seed: 1,
        }]
    );
    let (state, _) = apply_actions(state, actions);
    let (controller, actions) = reduce_key(controller, &state, plain('r'));
    assert_eq!(actions, vec![Action::RepeatModeChanged(RepeatMode::One)]);

    let (controller, direct_actions) = reduce_key(controller, &state, plain('e'));
    assert_eq!(direct_actions, vec![Action::RadioEnabledChanged(true)]);

    let mut palette = controller;
    (palette, _) = reduce_key(palette, &state, plain(':'));
    for character in "endless".chars() {
        (palette, _) = reduce_key(palette, &state, plain(character));
    }
    let (_, palette_actions) = reduce_key(palette, &state, key(KeyCode::Enter));
    assert_eq!(palette_actions, direct_actions);
}

#[test]
fn seek_shortcuts_use_music_and_configured_podcast_intervals() {
    let music = song("seek-song", "Seek Song");
    let (music_state, _) = reduce(
        AppState::default(),
        Action::EnqueueMedia {
            item: music.clone(),
        },
    );
    let (music_state, _) = reduce(
        music_state,
        Action::PlayQueueItem {
            id: stable_queue_item_id(&music.id),
        },
    );
    let (_, actions) = reduce_key(
        UiController::default(),
        &music_state,
        KeyEvent::new(KeyCode::Left, KeyModifiers::SHIFT),
    );
    assert_eq!(
        actions,
        vec![Action::SeekRelativeRequested { seconds: -10 }]
    );
    let (_, actions) = reduce_key(
        UiController::default(),
        &music_state,
        KeyEvent::new(KeyCode::Right, KeyModifiers::SHIFT),
    );
    assert_eq!(actions, vec![Action::SeekRelativeRequested { seconds: 10 }]);

    let mut config = Config::default();
    config.podcast.skip_backward_seconds = 17;
    config.podcast.skip_forward_seconds = 43;
    let episode = MediaItem {
        kind: MediaKind::PodcastEpisode,
        ..song("seek-episode", "Seek Episode")
    };
    let (podcast_state, _) = reduce(
        AppState::new(config),
        Action::EnqueueMedia {
            item: episode.clone(),
        },
    );
    let (podcast_state, _) = reduce(
        podcast_state,
        Action::PlayQueueItem {
            id: stable_queue_item_id(&episode.id),
        },
    );
    let (_, backward) = reduce_key(
        UiController::default(),
        &podcast_state,
        KeyEvent::new(KeyCode::Left, KeyModifiers::SHIFT),
    );
    let (_, forward) = reduce_key(
        UiController::default(),
        &podcast_state,
        KeyEvent::new(KeyCode::Right, KeyModifiers::SHIFT),
    );
    assert_eq!(
        backward,
        vec![Action::SeekRelativeRequested { seconds: -17 }]
    );
    assert_eq!(forward, vec![Action::SeekRelativeRequested { seconds: 43 }]);
}

#[test]
fn queue_selection_reorder_navigation_and_cancel_remain_ui_stable() {
    let first = song("queue-first", "Queue First");
    let second = song("queue-second", "Queue Second");
    let third = song("queue-third", "Queue Third");
    let (state, _) = apply_actions(
        AppState::default(),
        vec![
            Action::EnqueueMedia {
                item: first.clone(),
            },
            Action::EnqueueMedia {
                item: second.clone(),
            },
            Action::EnqueueMedia {
                item: third.clone(),
            },
        ],
    );

    let controller = UiController::default().with_focus(FocusRegion::Queue);
    let (controller, actions) = reduce_key(controller, &state, key(KeyCode::Down));
    assert!(actions.is_empty());
    assert_eq!(
        controller.queue_selected_id(),
        Some(&stable_queue_item_id(&second.id))
    );

    let (controller, actions) = reduce_key(controller, &state, plain(']'));
    assert_eq!(
        actions,
        vec![Action::QueueItemMovedBefore {
            id: stable_queue_item_id(&third.id),
            before: stable_queue_item_id(&second.id),
        }]
    );
    let (_, actions) = reduce_key(controller, &state, key(KeyCode::Enter));
    assert_eq!(
        actions,
        vec![Action::PlayQueueItem {
            id: stable_queue_item_id(&second.id),
        }]
    );

    let controller = UiController::default();
    let (controller, _) = reduce_key(controller, &state, key(KeyCode::Left));
    assert_eq!(controller.model().focus, FocusRegion::Navigation);
    let (controller, _) = reduce_key(controller, &state, key(KeyCode::Down));
    assert_eq!(controller.model().view, NavigationItem::Search);
    let (controller, _) = reduce_key(controller, &state, plain('?'));
    assert_eq!(controller.model().overlay, Some(Overlay::Help));
    let (controller, _) = reduce_key(controller, &state, key(KeyCode::Esc));
    assert_eq!(controller.model().overlay, None);
    let (controller, _) = reduce_key(controller, &state, plain('Q'));
    assert_eq!(controller.model().focus, FocusRegion::Queue);
    let (controller, _) = reduce_key(controller, &state, plain('q'));
    assert!(controller.quit_requested());

    let unknown = QueueItemId::from("not-present");
    assert_ne!(controller.queue_selected_id(), Some(&unknown));
}

#[test]
fn tab_focus_cycles_only_navigation_content_and_player() {
    let state = AppState::default();
    let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);
    let back_tab = KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT);

    let mut controller = UiController::default().with_focus(FocusRegion::Navigation);
    for expected in [
        FocusRegion::Content,
        FocusRegion::Player,
        FocusRegion::Navigation,
    ] {
        (controller, _) = reduce_key(controller, &state, tab);
        assert_eq!(controller.model().focus, expected);
    }

    for expected in [
        FocusRegion::Player,
        FocusRegion::Content,
        FocusRegion::Navigation,
    ] {
        (controller, _) = reduce_key(controller, &state, back_tab);
        assert_eq!(controller.model().focus, expected);
    }

    let (forward_from_queue, _) = reduce_key(
        UiController::default().with_focus(FocusRegion::Queue),
        &state,
        tab,
    );
    assert_eq!(forward_from_queue.model().focus, FocusRegion::Player);

    let (backward_from_queue, _) = reduce_key(
        UiController::default().with_focus(FocusRegion::Queue),
        &state,
        back_tab,
    );
    assert_eq!(backward_from_queue.model().focus, FocusRegion::Navigation);
}

#[test]
fn tab_focus_does_not_replace_queue_toggle_or_leak_through_overlays() {
    let state = AppState::default();
    let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);

    let (controller, _) = reduce_key(UiController::default(), &state, plain('Q'));
    assert_eq!(controller.model().focus, FocusRegion::Queue);
    let (controller, _) = reduce_key(controller, &state, plain('Q'));
    assert_eq!(controller.model().focus, FocusRegion::Content);

    let mut controller = UiController::default().with_focus(FocusRegion::Navigation);
    (controller, _) = reduce_key(controller, &state, plain('?'));
    assert_eq!(controller.model().overlay, Some(Overlay::Help));
    (controller, _) = reduce_key(controller, &state, tab);
    assert_eq!(controller.model().focus, FocusRegion::Navigation);

    (controller, _) = reduce_key(controller, &state, key(KeyCode::Esc));
    (controller, _) = reduce_key(controller, &state, plain('c'));
    assert_eq!(controller.model().overlay, Some(Overlay::CountryPicker));
    (controller, _) = reduce_key(controller, &state, tab);
    assert_eq!(controller.model().focus, FocusRegion::Navigation);

    (controller, _) = reduce_key(controller, &state, key(KeyCode::Esc));
    (controller, _) = reduce_key(controller, &state, shifted(':'));
    assert_eq!(controller.model().overlay, Some(Overlay::CommandPalette));
    (controller, _) = reduce_key(controller, &state, tab);
    assert_eq!(controller.model().focus, FocusRegion::Navigation);
}

#[test]
fn help_and_country_overlays_suppress_background_playback_and_queue_commands() {
    let state = AppState::default();
    let mut controller = UiController::default().with_focus(FocusRegion::Queue);

    (controller, _) = reduce_key(controller, &state, plain('?'));
    assert_eq!(controller.model().overlay, Some(Overlay::Help));
    for event in [
        plain(' '),
        plain('n'),
        plain(']'),
        key(KeyCode::F(8)),
        key(KeyCode::Media(MediaKeyCode::PlayPause)),
        KeyEvent::new(KeyCode::Left, KeyModifiers::SHIFT),
    ] {
        let (next, actions) = reduce_key(controller, &state, event);
        controller = next;
        assert!(actions.is_empty());
        assert_eq!(controller.model().overlay, Some(Overlay::Help));
    }

    (controller, _) = reduce_key(controller, &state, key(KeyCode::Esc));
    (controller, _) = reduce_key(controller, &state, plain('c'));
    assert_eq!(controller.model().overlay, Some(Overlay::CountryPicker));
    for event in [
        plain(' '),
        plain('n'),
        plain(']'),
        key(KeyCode::F(8)),
        key(KeyCode::Media(MediaKeyCode::PlayPause)),
        KeyEvent::new(KeyCode::Right, KeyModifiers::SHIFT),
    ] {
        let (next, actions) = reduce_key(controller, &state, event);
        controller = next;
        assert!(actions.is_empty());
        assert_eq!(controller.model().overlay, Some(Overlay::CountryPicker));
    }
}
