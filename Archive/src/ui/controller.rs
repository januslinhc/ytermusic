use crossterm::event::{KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use std::collections::HashSet;
use std::hash::{DefaultHasher, Hash, Hasher};

use crate::{
    app::{Action, AppState, ArtworkSurface, SearchItem, stable_library_item_id},
    auth::Browser,
    domain::{ChartSection, MediaItem, MediaKind, RegionCode, RepeatMode},
    provider::{AuthenticationState, LibraryItem},
    queue::{MAX_EXPLICIT_LIST_ITEMS, QueueItemId},
};

use super::{
    input::{InputAction, InputMode, SemanticAction, TextEntryContext, map_event},
    interaction::{HitTarget, InteractionSnapshot, ListSurface},
    motion::MotionFrame,
    render::{
        FocusRegion, HELP_LINE_COUNT, NavigationItem, Overlay, RenderModel, help_overlay_viewport,
        lyrics_overlay_viewport, wrapped_lyrics_row_count,
    },
};

const MAX_INPUT_BYTES: usize = 4 * 1024;
const MAX_INPUT_CHARS: usize = 1_024;
const MAX_COUNTRIES: usize = 32;
const COUNTRY_OPTIONS: [(&str, &str); 14] = [
    ("HK", "Hong Kong"),
    ("US", "United States"),
    ("JP", "Japan"),
    ("KR", "South Korea"),
    ("TW", "Taiwan"),
    ("GB", "United Kingdom"),
    ("CA", "Canada"),
    ("AU", "Australia"),
    ("IN", "India"),
    ("BR", "Brazil"),
    ("DE", "Germany"),
    ("FR", "France"),
    ("MX", "Mexico"),
    ("SG", "Singapore"),
];

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct BrowserPickerState {
    selected: usize,
}

impl BrowserPickerState {
    #[must_use]
    pub const fn choices(&self) -> &[Browser] {
        &Browser::ALL
    }

    #[must_use]
    pub fn selected_browser(self) -> Browser {
        Browser::ALL
            .get(self.selected)
            .copied()
            .unwrap_or(Browser::Brave)
    }

    pub(crate) fn move_by(&mut self, delta: isize) {
        self.selected = if delta.is_negative() {
            self.selected
                .checked_sub(delta.unsigned_abs())
                .unwrap_or(Browser::ALL.len() - 1)
        } else {
            self.selected.saturating_add(delta.unsigned_abs()) % Browser::ALL.len()
        };
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct CountryChoice {
    region: RegionCode,
    label: &'static str,
}

impl CountryChoice {
    #[must_use]
    pub const fn region(&self) -> &RegionCode {
        &self.region
    }

    #[must_use]
    pub const fn label(&self) -> &'static str {
        self.label
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct CountryPickerState {
    choices: Vec<CountryChoice>,
    selected: usize,
}

impl CountryPickerState {
    #[must_use]
    pub fn for_region(current: &RegionCode) -> Self {
        let mut choices = COUNTRY_OPTIONS
            .into_iter()
            .filter_map(|(code, label)| {
                RegionCode::parse(code)
                    .ok()
                    .map(|region| CountryChoice { region, label })
            })
            .take(MAX_COUNTRIES)
            .collect::<Vec<_>>();
        let selected = choices
            .iter()
            .position(|choice| choice.region() == current)
            .unwrap_or_else(|| {
                if choices.len() < MAX_COUNTRIES {
                    choices.push(CountryChoice {
                        region: current.clone(),
                        label: "Current region",
                    });
                    choices.len().saturating_sub(1)
                } else {
                    0
                }
            });
        Self { choices, selected }
    }

    #[must_use]
    pub fn choices(&self) -> &[CountryChoice] {
        &self.choices
    }

    #[must_use]
    pub const fn selected_index(&self) -> usize {
        self.selected
    }

    #[must_use]
    pub fn selected_region(&self) -> &RegionCode {
        self.choices[self.selected].region()
    }

    fn move_by(&mut self, delta: isize) {
        let len = self.choices.len();
        if len == 0 {
            return;
        }
        self.selected = if delta.is_negative() {
            self.selected
                .checked_sub(delta.unsigned_abs())
                .unwrap_or(len - 1)
        } else {
            self.selected.saturating_add(delta.unsigned_abs()) % len
        };
    }
}

impl Default for CountryPickerState {
    fn default() -> Self {
        Self::for_region(&RegionCode::default())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct UiController {
    model: RenderModel,
    input_mode: InputMode,
    input: String,
    next_shuffle_seed: u64,
    quit_requested: bool,
}

impl Default for UiController {
    fn default() -> Self {
        Self {
            model: RenderModel::default(),
            input_mode: InputMode::Normal,
            input: String::new(),
            next_shuffle_seed: 1,
            quit_requested: false,
        }
    }
}

impl UiController {
    #[must_use]
    pub const fn model(&self) -> &RenderModel {
        &self.model
    }

    pub(crate) fn set_motion_frame(&mut self, frame: MotionFrame) {
        self.model.set_motion_frame(frame);
    }

    #[must_use]
    pub const fn input_mode(&self) -> InputMode {
        self.input_mode
    }

    #[must_use]
    pub fn input_text(&self) -> &str {
        &self.input
    }

    #[must_use]
    pub const fn country_picker(&self) -> &CountryPickerState {
        &self.model.country_picker
    }

    #[must_use]
    pub const fn with_view(mut self, view: NavigationItem) -> Self {
        self.model.view = view;
        self
    }

    #[must_use]
    pub const fn with_focus(mut self, focus: FocusRegion) -> Self {
        self.model.focus = focus;
        self
    }

    #[must_use]
    pub const fn with_shuffle_seed(mut self, seed: u64) -> Self {
        self.next_shuffle_seed = seed;
        self
    }

    #[must_use]
    pub const fn queue_selected_id(&self) -> Option<&QueueItemId> {
        self.model.queue_selected_id()
    }

    #[must_use]
    pub const fn quit_requested(&self) -> bool {
        self.quit_requested
    }
}

#[must_use]
pub fn reduce_key(
    controller: UiController,
    state: &AppState,
    event: KeyEvent,
) -> (UiController, Vec<Action>) {
    let mode = controller.input_mode;
    match map_event(mode, event) {
        Some(input) => reduce_input(controller, state, input),
        None => (controller, Vec::new()),
    }
}

/// Reduces one mouse gesture against geometry from the latest completed frame.
///
/// Left-button down is the sole click edge; the paired button-up event is
/// intentionally ignored so one physical click dispatches at most once.
#[must_use]
pub fn reduce_mouse(
    mut controller: UiController,
    state: &AppState,
    event: MouseEvent,
    snapshot: Option<&InteractionSnapshot>,
) -> (UiController, Vec<Action>) {
    controller.reconcile_queue_selection(state);
    controller.reconcile_lyrics_media(state);
    match event.kind {
        MouseEventKind::ScrollUp => {
            if controller.input_mode != InputMode::Normal && controller.model.overlay.is_none() {
                return (controller, Vec::new());
            }
            dispatch_semantic(controller, state, SemanticAction::MoveUp)
        }
        MouseEventKind::ScrollDown => {
            if controller.input_mode != InputMode::Normal && controller.model.overlay.is_none() {
                return (controller, Vec::new());
            }
            dispatch_semantic(controller, state, SemanticAction::MoveDown)
        }
        MouseEventKind::Down(MouseButton::Left) => {
            let Some(target) =
                snapshot.and_then(|snapshot| snapshot.resolve(event.column, event.row))
            else {
                return (controller, Vec::new());
            };
            reduce_hit_target(controller, state, target)
        }
        MouseEventKind::Down(_)
        | MouseEventKind::Up(_)
        | MouseEventKind::Drag(_)
        | MouseEventKind::Moved
        | MouseEventKind::ScrollLeft
        | MouseEventKind::ScrollRight => (controller, Vec::new()),
    }
}

fn reduce_hit_target(
    mut controller: UiController,
    state: &AppState,
    target: HitTarget,
) -> (UiController, Vec<Action>) {
    match target {
        HitTarget::Semantic(action) => dispatch_semantic(controller, state, action),
        HitTarget::Navigation(view) => {
            if controller.model.overlay.is_some() {
                return (controller, Vec::new());
            }
            controller.model.view = view;
            controller.model.focus = FocusRegion::Navigation;
            controller.input_mode = InputMode::Normal;
            controller.input.clear();
            controller.model.clear_search_draft();
            let (mut controller, actions) = activate_navigation(controller, state);
            controller.model.focus = FocusRegion::Navigation;
            (controller, actions)
        }
        HitTarget::Progress {
            numerator,
            denominator,
        } => {
            if controller.model.overlay.is_some() {
                return (controller, Vec::new());
            }
            let Some(duration_ms) = state
                .playback()
                .duration_ms
                .filter(|duration| *duration > 0)
            else {
                return (controller, Vec::new());
            };
            if denominator == 0 || numerator > denominator {
                return (controller, Vec::new());
            }
            let target_ms = (u128::from(duration_ms) * u128::from(numerator)
                / u128::from(denominator))
            .min(u128::from(duration_ms));
            let delta_ms = i128::try_from(target_ms).unwrap_or(i128::MAX)
                - i128::from(state.playback().position_ms);
            let whole_seconds = delta_ms / 1_000;
            let rounded_seconds = if delta_ms != 0 && delta_ms % 1_000 != 0 {
                whole_seconds.saturating_add(delta_ms.signum())
            } else {
                whole_seconds
            };
            let Some(seconds) = i64::try_from(rounded_seconds).ok() else {
                return (controller, Vec::new());
            };
            if seconds == 0 {
                (controller, Vec::new())
            } else {
                (controller, vec![Action::SeekRelativeRequested { seconds }])
            }
        }
        HitTarget::ListRow {
            surface,
            stable_index,
        } => reduce_list_row(controller, state, surface, stable_index),
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "the exhaustive surface dispatcher keeps bounds, focus, selection, and activation auditable"
)]
fn reduce_list_row(
    mut controller: UiController,
    state: &AppState,
    surface: ListSurface,
    stable_index: usize,
) -> (UiController, Vec<Action>) {
    match surface {
        ListSurface::CountryPicker if controller.model.overlay == Some(Overlay::CountryPicker) => {
            if stable_index >= controller.model.country_picker.choices().len() {
                return (controller, Vec::new());
            }
            if controller.model.country_picker.selected_index() == stable_index {
                return submit(controller, state);
            }
            controller.model.country_picker.selected = stable_index;
            (controller, Vec::new())
        }
        ListSurface::BrowserPicker if controller.model.overlay == Some(Overlay::BrowserPicker) => {
            if stable_index >= controller.model.browser_picker.choices().len() {
                return (controller, Vec::new());
            }
            if controller.model.browser_picker.selected == stable_index {
                return submit(controller, state);
            }
            controller.model.browser_picker.selected = stable_index;
            (controller, Vec::new())
        }
        ListSurface::CommandPalette
            if controller.model.overlay == Some(Overlay::CommandPalette) =>
        {
            let total = controller.model.palette.matching_entries().len();
            if stable_index >= total {
                return (controller, Vec::new());
            }
            if controller.model.palette.selected_index() == stable_index {
                return submit(controller, state);
            }
            controller.model.palette.select(stable_index);
            (controller, Vec::new())
        }
        ListSurface::Lyrics if controller.model.overlay == Some(Overlay::Lyrics) => {
            (controller, Vec::new())
        }
        ListSurface::CountryPicker
        | ListSurface::BrowserPicker
        | ListSurface::CommandPalette
        | ListSurface::Lyrics => (controller, Vec::new()),
        _ if controller.model.overlay.is_some() => (controller, Vec::new()),
        ListSurface::Queue => reduce_queue_row(controller, state, stable_index),
        ListSurface::Search => {
            if controller.model.view != NavigationItem::Search {
                return (controller, Vec::new());
            }
            let Some(item) = state.search().items().get(stable_index) else {
                return (controller, Vec::new());
            };
            let id = item.stable_id();
            normalize_background_list_input(&mut controller);
            controller.model.focus = FocusRegion::Content;
            if state.search().selected_id() == Some(&id) {
                submit(controller, state)
            } else {
                (controller, vec![Action::SearchSelectionChanged { id }])
            }
        }
        ListSurface::Charts => {
            if controller.model.view != NavigationItem::Charts {
                return (controller, Vec::new());
            }
            let Some(_) = state
                .charts()
                .sections()
                .iter()
                .flat_map(ChartSection::items)
                .nth(stable_index)
            else {
                return (controller, Vec::new());
            };
            normalize_background_list_input(&mut controller);
            controller.model.focus = FocusRegion::Content;
            if state.charts().selected_index() == Some(stable_index) {
                submit(controller, state)
            } else {
                (
                    controller,
                    vec![Action::ChartRowSelectionChanged {
                        item_index: stable_index,
                    }],
                )
            }
        }
        ListSurface::PodcastRecommendations => {
            if controller.model.view != NavigationItem::Podcasts
                || state.podcasts().show().is_some()
            {
                return (controller, Vec::new());
            }
            let Some(item) = state.podcasts().recommendations().get(stable_index) else {
                return (controller, Vec::new());
            };
            let id = item.source_id().clone();
            normalize_background_list_input(&mut controller);
            controller.model.focus = FocusRegion::Content;
            if state.podcasts().selected_recommendation() == Some(&id) {
                submit(controller, state)
            } else {
                (
                    controller,
                    vec![Action::PodcastRecommendationSelectionChanged { id }],
                )
            }
        }
        ListSurface::PodcastEpisodes => {
            if controller.model.view != NavigationItem::Podcasts {
                return (controller, Vec::new());
            }
            let Some(item) = state
                .podcasts()
                .show()
                .and_then(|show| show.episodes.get(stable_index))
            else {
                return (controller, Vec::new());
            };
            let media_id = item.id.clone();
            normalize_background_list_input(&mut controller);
            controller.model.focus = FocusRegion::Content;
            if state.podcasts().selected_episode() == Some(&media_id) {
                submit(controller, state)
            } else {
                (
                    controller,
                    vec![Action::PodcastSelectionChanged { media_id }],
                )
            }
        }
        ListSurface::Library => {
            if controller.model.view != NavigationItem::Library {
                return (controller, Vec::new());
            }
            let Some(item) = state.library().items().get(stable_index) else {
                return (controller, Vec::new());
            };
            let id = stable_library_item_id(item);
            normalize_background_list_input(&mut controller);
            controller.model.focus = FocusRegion::Content;
            if state.library().selected_id() == Some(&id) {
                submit(controller, state)
            } else {
                (controller, vec![Action::LibrarySelectionChanged { id }])
            }
        }
        ListSurface::Favorites => {
            if controller.model.view != NavigationItem::Favorites {
                return (controller, Vec::new());
            }
            let Some(item) = state.favorites().entries().get(stable_index) else {
                return (controller, Vec::new());
            };
            let media_id = item.item.id.clone();
            normalize_background_list_input(&mut controller);
            controller.model.focus = FocusRegion::Content;
            if state.favorites().selected_id() == Some(&media_id) {
                submit(controller, state)
            } else {
                (
                    controller,
                    vec![Action::FavoriteSelectionChanged { media_id }],
                )
            }
        }
        ListSurface::History => {
            if controller.model.view != NavigationItem::History {
                return (controller, Vec::new());
            }
            let Some(item) = state.history().entries().get(stable_index) else {
                return (controller, Vec::new());
            };
            let id = item.id;
            normalize_background_list_input(&mut controller);
            controller.model.focus = FocusRegion::Content;
            if state.history().selected_id() == Some(id) {
                submit(controller, state)
            } else {
                (controller, vec![Action::HistorySelectionChanged { id }])
            }
        }
        ListSurface::Home => (controller, Vec::new()),
    }
}

fn reduce_queue_row(
    mut controller: UiController,
    state: &AppState,
    stable_index: usize,
) -> (UiController, Vec<Action>) {
    let Some(id) = state.queue().active_ids().get(stable_index).cloned() else {
        return (controller, Vec::new());
    };
    normalize_background_list_input(&mut controller);
    controller.model.focus = FocusRegion::Queue;
    if controller.model.queue_selected_id() == Some(&id) {
        submit(controller, state)
    } else {
        controller.model.set_queue_selected_id(Some(id));
        (controller, Vec::new())
    }
}

fn normalize_background_list_input(controller: &mut UiController) {
    controller.input_mode = InputMode::Normal;
    controller.input.clear();
    controller.model.clear_search_draft();
}

#[must_use]
pub fn reduce_input(
    mut controller: UiController,
    state: &AppState,
    input: InputAction,
) -> (UiController, Vec<Action>) {
    controller.reconcile_queue_selection(state);
    controller.reconcile_lyrics_media(state);
    match input {
        InputAction::InsertCharacter(character) => {
            if controller.input.len() < MAX_INPUT_BYTES
                && controller.input.chars().count() < MAX_INPUT_CHARS
            {
                controller.input.push(character);
                controller.sync_palette_query();
                controller.sync_search_draft();
            }
            (controller, Vec::new())
        }
        InputAction::Semantic(action) => dispatch_semantic(controller, state, action),
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "the exhaustive semantic dispatcher keeps keyboard commands mapped in one auditable place"
)]
fn dispatch_semantic(
    mut controller: UiController,
    state: &AppState,
    action: SemanticAction,
) -> (UiController, Vec<Action>) {
    if controller
        .model
        .overlay
        .is_some_and(|overlay| !overlay_accepts(overlay, action))
    {
        return (controller, Vec::new());
    }

    match action {
        SemanticAction::Quit => {
            controller.quit_requested = true;
            (controller, Vec::new())
        }
        SemanticAction::ToggleHelp => {
            controller.model.overlay = if controller.model.overlay == Some(Overlay::Help) {
                None
            } else {
                Some(Overlay::Help)
            };
            controller.model.help_scroll = 0;
            controller.input_mode = InputMode::Normal;
            controller.input.clear();
            controller.model.clear_search_draft();
            (controller, Vec::new())
        }
        SemanticAction::OpenSearch => {
            controller.model.view = NavigationItem::Search;
            controller.model.focus = FocusRegion::Content;
            controller.model.overlay = None;
            controller.input_mode = InputMode::TextEntry(TextEntryContext::Search);
            controller.input.clear();
            controller.sync_search_draft();
            let actions = navigation_actions(NavigationItem::Search, state);
            (controller, actions)
        }
        SemanticAction::OpenPalette => {
            controller.model.overlay = Some(Overlay::CommandPalette);
            controller.input_mode = InputMode::TextEntry(TextEntryContext::Palette);
            controller.input.clear();
            controller.sync_palette_query();
            (controller, Vec::new())
        }
        SemanticAction::ChooseCountry => {
            controller.open_country_picker(state);
            (controller, Vec::new())
        }
        SemanticAction::ToggleLyrics => {
            if controller.model.overlay == Some(Overlay::Lyrics) {
                controller.model.overlay = None;
            } else {
                controller.model.overlay = Some(Overlay::Lyrics);
                controller.model.lyrics.follow_active = true;
                controller.model.lyrics.selected_line = state.lyrics().active_line_index();
                controller.model.lyrics.scroll =
                    controller.model.lyrics.selected_line.unwrap_or_default();
            }
            controller.input_mode = InputMode::Normal;
            controller.input.clear();
            (controller, Vec::new())
        }
        SemanticAction::MoveUp if controller.model.overlay == Some(Overlay::BrowserPicker) => {
            controller.model.browser_picker.move_by(-1);
            (controller, Vec::new())
        }
        SemanticAction::MoveUp | SemanticAction::MoveDown
            if controller.model.overlay == Some(Overlay::Help) =>
        {
            controller.model.help_scroll = if action == SemanticAction::MoveUp {
                controller.model.help_scroll.saturating_sub(1)
            } else {
                controller
                    .model
                    .help_scroll
                    .saturating_add(1)
                    .min(controller.model.help_max_scroll)
            };
            (controller, Vec::new())
        }
        SemanticAction::MoveDown if controller.model.overlay == Some(Overlay::BrowserPicker) => {
            controller.model.browser_picker.move_by(1);
            (controller, Vec::new())
        }
        SemanticAction::MoveUp if controller.model.overlay == Some(Overlay::CountryPicker) => {
            controller.model.country_picker.move_by(-1);
            (controller, Vec::new())
        }
        SemanticAction::MoveDown if controller.model.overlay == Some(Overlay::CountryPicker) => {
            controller.model.country_picker.move_by(1);
            (controller, Vec::new())
        }
        SemanticAction::MoveUp if controller.model.overlay == Some(Overlay::CommandPalette) => {
            controller.model.palette.move_by(-1);
            (controller, Vec::new())
        }
        SemanticAction::MoveDown if controller.model.overlay == Some(Overlay::CommandPalette) => {
            controller.model.palette.move_by(1);
            (controller, Vec::new())
        }
        SemanticAction::MoveUp | SemanticAction::MoveDown
            if controller.model.overlay == Some(Overlay::Lyrics) =>
        {
            let document = state.lyrics().document();
            let total = document.map_or(0, |document| document.timed().len());
            if total > 0 {
                let current = controller
                    .model
                    .lyrics
                    .selected_line
                    .or_else(|| state.lyrics().active_line_index())
                    .unwrap_or_default()
                    .min(total - 1);
                let selected = if action == SemanticAction::MoveUp {
                    current.saturating_sub(1)
                } else {
                    current.saturating_add(1).min(total - 1)
                };
                controller.model.lyrics.follow_active = false;
                controller.model.lyrics.selected_line = Some(selected);
                controller.model.lyrics.scroll = selected;
            } else if document.and_then(|document| document.plain()).is_some() {
                controller.model.lyrics.follow_active = false;
                controller.model.lyrics.scroll = if action == SemanticAction::MoveUp {
                    controller.model.lyrics.scroll.saturating_sub(1)
                } else {
                    controller
                        .model
                        .lyrics
                        .scroll
                        .saturating_add(1)
                        .min(controller.model.lyrics.plain_max_scroll)
                };
                controller.model.lyrics.selected_line = Some(controller.model.lyrics.scroll);
            }
            (controller, Vec::new())
        }
        SemanticAction::MoveUp | SemanticAction::MoveDown => {
            let delta = if action == SemanticAction::MoveUp {
                -1
            } else {
                1
            };
            move_selection(controller, state, delta)
        }
        SemanticAction::MoveLeft => {
            if controller.model.focus == FocusRegion::Navigation {
                return switch_navigation_view(controller, state, -1);
            }
            controller.model.focus = match controller.model.focus {
                FocusRegion::Navigation | FocusRegion::Content | FocusRegion::Player => {
                    FocusRegion::Navigation
                }
                FocusRegion::Queue => FocusRegion::Content,
            };
            (controller, Vec::new())
        }
        SemanticAction::MoveRight => {
            if controller.model.focus == FocusRegion::Navigation {
                return switch_navigation_view(controller, state, 1);
            }
            controller.model.focus = match controller.model.focus {
                FocusRegion::Navigation => FocusRegion::Content,
                FocusRegion::Content => FocusRegion::Queue,
                FocusRegion::Queue | FocusRegion::Player => FocusRegion::Player,
            };
            (controller, Vec::new())
        }
        SemanticAction::CycleFocusForward | SemanticAction::CycleFocusBackward => {
            controller.model.focus = cycle_focus(
                controller.model.focus,
                action == SemanticAction::CycleFocusForward,
            );
            (controller, Vec::new())
        }
        SemanticAction::TogglePlayback => (controller, vec![Action::TogglePlayback]),
        SemanticAction::NextTrack => (controller, vec![Action::NextRequested]),
        SemanticAction::PreviousTrack => (controller, vec![Action::PreviousRequested]),
        SemanticAction::ToggleFavorite => {
            let action = favorite_target(&controller, state)
                .map(|item| Action::FavoriteToggleRequested { item });
            (controller, action.into_iter().collect())
        }
        SemanticAction::SeekBackward | SemanticAction::SeekForward => {
            let is_podcast = state
                .queue()
                .current()
                .is_some_and(|item| item.media().kind == MediaKind::PodcastEpisode);
            let seconds = if is_podcast {
                if action == SemanticAction::SeekBackward {
                    state.podcast_skip_backward_seconds()
                } else {
                    state.podcast_skip_forward_seconds()
                }
            } else {
                state.music_seek_seconds()
            };
            let seconds = i64::try_from(seconds).ok().and_then(|seconds| {
                if action == SemanticAction::SeekBackward {
                    seconds.checked_neg()
                } else {
                    Some(seconds)
                }
            });
            (
                controller,
                seconds
                    .map(|seconds| Action::SeekRelativeRequested { seconds })
                    .into_iter()
                    .collect(),
            )
        }
        SemanticAction::VolumeUp | SemanticAction::VolumeDown => {
            let current = state.playback().target_volume;
            let volume = if action == SemanticAction::VolumeUp {
                current.saturating_add(5).min(100)
            } else {
                current.saturating_sub(5)
            };
            (controller, vec![Action::TargetVolumeChanged(volume)])
        }
        SemanticAction::ToggleShuffle => {
            let enabled = !state.queue().is_shuffled();
            let seed = controller.next_shuffle_seed;
            if enabled {
                controller.next_shuffle_seed = controller.next_shuffle_seed.wrapping_add(1);
            }
            (
                controller,
                vec![Action::ShuffleEnabledChanged { enabled, seed }],
            )
        }
        SemanticAction::CycleRepeat => {
            let repeat = match state.queue().repeat() {
                RepeatMode::Off => RepeatMode::One,
                RepeatMode::One => RepeatMode::All,
                RepeatMode::All => RepeatMode::Off,
            };
            (controller, vec![Action::RepeatModeChanged(repeat)])
        }
        SemanticAction::ToggleRadio => (
            controller,
            vec![Action::RadioEnabledChanged(!state.queue().radio_enabled())],
        ),
        SemanticAction::MoveQueueItemUp | SemanticAction::MoveQueueItemDown => {
            reorder_queue(controller, state, action == SemanticAction::MoveQueueItemUp)
        }
        SemanticAction::ConnectAccount => {
            controller.model.browser_picker = BrowserPickerState::default();
            controller.model.overlay = Some(Overlay::BrowserPicker);
            controller.input_mode = InputMode::Normal;
            controller.input.clear();
            (controller, Vec::new())
        }
        SemanticAction::LoadMore => {
            let actions = match controller.model.view {
                NavigationItem::Search => vec![Action::SearchMoreRequested],
                NavigationItem::Library => vec![Action::LibraryMoreRequested],
                NavigationItem::Home
                | NavigationItem::Charts
                | NavigationItem::Podcasts
                | NavigationItem::Favorites
                | NavigationItem::History
                | NavigationItem::Settings => Vec::new(),
            };
            (controller, actions)
        }
        SemanticAction::RecheckDependencies => (controller, vec![Action::DependencyCheckRequested]),
        SemanticAction::ToggleQueuePanel => {
            controller.model = controller.model.toggle_compact_panel();
            (controller, Vec::new())
        }
        SemanticAction::DeleteBackward if controller.input_mode != InputMode::Normal => {
            let _ = controller.input.pop();
            controller.sync_palette_query();
            controller.sync_search_draft();
            (controller, Vec::new())
        }
        SemanticAction::Cancel => {
            let close_podcast = controller.model.overlay.is_none()
                && controller.input_mode == InputMode::Normal
                && controller.model.view == NavigationItem::Podcasts
                && state.podcasts().show().is_some();
            controller.model.overlay = None;
            controller.model.help_scroll = 0;
            controller.input_mode = InputMode::Normal;
            controller.input.clear();
            controller.model.clear_search_draft();
            (
                controller,
                close_podcast
                    .then_some(Action::ClosePodcast)
                    .into_iter()
                    .collect(),
            )
        }
        SemanticAction::Submit => submit(controller, state),
        SemanticAction::DeleteBackward => (controller, Vec::new()),
    }
}

#[allow(
    clippy::match_same_arms,
    reason = "the explicit directional table keeps every focus transition auditable"
)]
const fn cycle_focus(focus: FocusRegion, forward: bool) -> FocusRegion {
    match (focus, forward) {
        (FocusRegion::Navigation, true) => FocusRegion::Content,
        (FocusRegion::Content | FocusRegion::Queue, true) => FocusRegion::Player,
        (FocusRegion::Player, true) => FocusRegion::Navigation,
        (FocusRegion::Navigation, false) => FocusRegion::Player,
        (FocusRegion::Content | FocusRegion::Queue, false) => FocusRegion::Navigation,
        (FocusRegion::Player, false) => FocusRegion::Content,
    }
}

const fn overlay_accepts(overlay: Overlay, action: SemanticAction) -> bool {
    match overlay {
        Overlay::Help => matches!(
            action,
            SemanticAction::Quit
                | SemanticAction::ToggleHelp
                | SemanticAction::MoveUp
                | SemanticAction::MoveDown
                | SemanticAction::Cancel
        ),
        Overlay::CountryPicker => matches!(
            action,
            SemanticAction::Quit
                | SemanticAction::MoveUp
                | SemanticAction::MoveDown
                | SemanticAction::Cancel
                | SemanticAction::Submit
        ),
        Overlay::BrowserPicker => matches!(
            action,
            SemanticAction::Quit
                | SemanticAction::MoveUp
                | SemanticAction::MoveDown
                | SemanticAction::Cancel
                | SemanticAction::Submit
        ),
        Overlay::CommandPalette => true,
        Overlay::Lyrics => matches!(
            action,
            SemanticAction::ToggleLyrics
                | SemanticAction::MoveUp
                | SemanticAction::MoveDown
                | SemanticAction::Cancel
                | SemanticAction::Submit
        ),
    }
}

fn submit(mut controller: UiController, state: &AppState) -> (UiController, Vec<Action>) {
    if controller.model.overlay == Some(Overlay::Lyrics) {
        controller.model.lyrics.follow_active = true;
        controller.model.lyrics.selected_line = state.lyrics().active_line_index();
        controller.model.lyrics.scroll = controller.model.lyrics.selected_line.unwrap_or_default();
        return (controller, Vec::new());
    }

    if controller.model.overlay == Some(Overlay::BrowserPicker) {
        let browser = controller.model.browser_picker.selected_browser();
        controller.model.overlay = None;
        controller.input_mode = InputMode::Normal;
        return (
            controller,
            vec![Action::ConnectAccountRequested { browser }],
        );
    }

    if controller.model.overlay == Some(Overlay::CountryPicker) {
        let region = controller.model.country_picker.selected_region().clone();
        controller.model.overlay = None;
        controller.input_mode = InputMode::Normal;
        return (
            controller,
            vec![
                Action::ChartsRequested {
                    region: region.clone(),
                },
                Action::PodcastRecommendationsRequested { region },
            ],
        );
    }

    if controller.model.overlay == Some(Overlay::CommandPalette) {
        let selected = controller.model.palette.selected_action();
        controller.model.overlay = None;
        controller.input_mode = InputMode::Normal;
        controller.input.clear();
        return match selected {
            Some(SemanticAction::Submit | SemanticAction::OpenPalette) | None => {
                (controller, Vec::new())
            }
            Some(action) => dispatch_semantic(controller, state, action),
        };
    }

    if controller.input_mode == InputMode::TextEntry(TextEntryContext::Search) {
        controller.input_mode = InputMode::Normal;
        let query = std::mem::take(&mut controller.input);
        controller.model.clear_search_draft();
        return (
            controller,
            vec![Action::SearchSubmitted {
                query,
                filter: state.search().filter(),
            }],
        );
    }

    if controller.model.focus == FocusRegion::Navigation {
        return activate_navigation(controller, state);
    }

    let (controller, actions) = match controller.model.focus {
        FocusRegion::Queue => {
            let actions = controller
                .model
                .queue_selected_id()
                .cloned()
                .map(|id| vec![Action::PlayQueueItem { id }])
                .unwrap_or_default();
            (controller, actions)
        }
        FocusRegion::Navigation | FocusRegion::Player => (controller, Vec::new()),
        FocusRegion::Content => match controller.model.view {
            NavigationItem::Search => selected_search_actions(controller, state),
            NavigationItem::Charts => selected_chart_actions(controller, state),
            NavigationItem::Podcasts => selected_podcast_actions(controller, state),
            NavigationItem::Library => selected_library_actions(controller, state),
            NavigationItem::Favorites => selected_favorite_actions(controller, state),
            NavigationItem::History => selected_history_actions(controller, state),
            NavigationItem::Home | NavigationItem::Settings => (controller, Vec::new()),
        },
    };
    (controller, actions)
}

fn activate_navigation(
    mut controller: UiController,
    state: &AppState,
) -> (UiController, Vec<Action>) {
    controller.model.focus = FocusRegion::Content;
    let actions = navigation_actions(controller.model.view, state);
    (controller, actions)
}

fn switch_navigation_view(
    mut controller: UiController,
    state: &AppState,
    delta: isize,
) -> (UiController, Vec<Action>) {
    controller.model.view = moved_value(&NavigationItem::ALL, Some(&controller.model.view), delta)
        .copied()
        .unwrap_or_default();
    let actions = navigation_actions(controller.model.view, state);
    (controller, actions)
}

fn navigation_actions(view: NavigationItem, state: &AppState) -> Vec<Action> {
    let mut actions = navigation_load_action(view, state)
        .into_iter()
        .collect::<Vec<_>>();
    let surface = match view {
        NavigationItem::Home => ArtworkSurface::Home,
        NavigationItem::Search => ArtworkSurface::Search,
        NavigationItem::Charts => ArtworkSurface::Charts,
        NavigationItem::Podcasts => ArtworkSurface::Podcasts,
        NavigationItem::Library => ArtworkSurface::Library,
        NavigationItem::Favorites => ArtworkSurface::Favorites,
        NavigationItem::History => ArtworkSurface::History,
        NavigationItem::Settings => ArtworkSurface::Settings,
    };
    actions.push(Action::ArtworkSurfaceChanged { surface });
    actions
}

fn navigation_load_action(view: NavigationItem, state: &AppState) -> Option<Action> {
    match view {
        NavigationItem::Library
            if state.library().authentication() == AuthenticationState::Authenticated
                && state.library().active_generation().is_none() =>
        {
            Some(Action::LibraryRequested {
                section: state.library().section(),
            })
        }
        NavigationItem::History if !state.history().loading() => Some(Action::HistoryRequested),
        NavigationItem::Favorites
            if !state.favorites().loaded()
                && !state.favorites().loading()
                && state.favorites().active_generation().is_none() =>
        {
            Some(Action::FavoritesRequested)
        }
        NavigationItem::Podcasts
            if state.podcasts().show().is_none()
                && state.podcasts().recommendations().is_empty()
                && state
                    .podcasts()
                    .active_recommendation_generation()
                    .is_none()
                && !state.podcasts().recommendations_loading()
                && state.podcasts().active_generation().is_none()
                && !state.podcasts().loading() =>
        {
            Some(Action::PodcastRecommendationsRequested {
                region: state.podcasts().requested_region().clone(),
            })
        }
        NavigationItem::Home
        | NavigationItem::Search
        | NavigationItem::Charts
        | NavigationItem::Podcasts
        | NavigationItem::Library
        | NavigationItem::Favorites
        | NavigationItem::History
        | NavigationItem::Settings => None,
    }
}

fn selected_podcast_actions(
    controller: UiController,
    state: &AppState,
) -> (UiController, Vec<Action>) {
    let podcasts = state.podcasts();
    if let Some(show) = podcasts.show() {
        return activate_media_list(
            controller,
            state,
            &show.episodes,
            podcasts.selected_episode().cloned(),
        );
    }
    if podcasts.recommendations().is_empty()
        || podcasts.selected_recommendation().is_none()
        || podcasts.recommendations_loading()
        || podcasts.active_recommendation_generation().is_some()
        || podcasts.resolve_loading()
        || podcasts.active_resolve_generation().is_some()
        || podcasts.loading()
        || podcasts.active_generation().is_some()
    {
        (controller, Vec::new())
    } else {
        (controller, vec![Action::OpenSelectedPodcastRecommendation])
    }
}

fn selected_search_actions(
    controller: UiController,
    state: &AppState,
) -> (UiController, Vec<Action>) {
    let selected = state.search().selected_id();
    let item = state
        .search()
        .items()
        .iter()
        .find(|item| selected == Some(&item.stable_id()));
    match item {
        Some(SearchItem::Playable(media)) => activate_media_list(
            controller,
            state,
            state.search().items().iter().filter_map(|item| match item {
                SearchItem::Playable(media) => Some(media),
                SearchItem::Metadata(_) => None,
            }),
            Some(media.id.clone()),
        ),
        Some(SearchItem::Metadata(metadata))
            if metadata.kind() == crate::app::SearchMetadataKind::Podcast =>
        {
            (controller, vec![Action::OpenSelectedPodcast])
        }
        Some(SearchItem::Metadata(_)) | None => (controller, Vec::new()),
    }
}

fn selected_chart_actions(
    controller: UiController,
    state: &AppState,
) -> (UiController, Vec<Action>) {
    let Some(selected) = selected_chart_item(state) else {
        return (controller, Vec::new());
    };
    activate_media_list(
        controller,
        state,
        state
            .charts()
            .sections()
            .iter()
            .flat_map(ChartSection::items),
        Some(selected.id),
    )
}

fn selected_chart_item(state: &AppState) -> Option<MediaItem> {
    let selected = state.charts().selected_id()?;
    let selected_index = state.charts().selected_index()?;
    state
        .charts()
        .sections()
        .iter()
        .flat_map(ChartSection::items)
        .nth(selected_index)
        .filter(|item| &item.id == selected)
        .cloned()
}

fn favorite_target(controller: &UiController, state: &AppState) -> Option<MediaItem> {
    match controller.model.focus {
        FocusRegion::Navigation => None,
        FocusRegion::Queue => {
            let selected = controller.model.queue_selected_id()?;
            state
                .queue()
                .active_items()
                .find(|item| item.id() == selected)
                .map(|item| item.media().clone())
        }
        FocusRegion::Player => {
            let current = state.playback().current.as_ref()?;
            state
                .queue()
                .items()
                .iter()
                .find(|item| &item.media().id == current)
                .map(|item| item.media().clone())
        }
        FocusRegion::Content => match controller.model.view {
            NavigationItem::Search
                if state.search().loading()
                    || state.search().loading_more()
                    || state.search().error().is_some() =>
            {
                None
            }
            NavigationItem::Search => {
                let selected = state.search().selected_id()?;
                state.search().items().iter().find_map(|item| match item {
                    SearchItem::Playable(media) if item.stable_id() == *selected => {
                        Some(media.clone())
                    }
                    SearchItem::Playable(_) | SearchItem::Metadata(_) => None,
                })
            }
            NavigationItem::Charts
                if state.charts().loading() || state.charts().error().is_some() =>
            {
                None
            }
            NavigationItem::Charts => selected_chart_item(state),
            NavigationItem::Podcasts
                if state.podcasts().loading()
                    || state.podcasts().recommendations_loading()
                    || state.podcasts().resolve_loading()
                    || state.podcasts().error().is_some()
                    || state.podcasts().recommendation_error().is_some()
                    || state.podcasts().resolve_error().is_some() =>
            {
                None
            }
            NavigationItem::Podcasts => {
                let selected = state.podcasts().selected_episode()?;
                state
                    .podcasts()
                    .show()?
                    .episodes
                    .iter()
                    .find(|episode| &episode.id == selected)
                    .cloned()
            }
            NavigationItem::Library
                if state.library().loading()
                    || state.library().loading_more()
                    || state.library().error().is_some() =>
            {
                None
            }
            NavigationItem::Library => {
                let selected = state.library().selected_id()?;
                state.library().items().iter().find_map(|item| match item {
                    LibraryItem::Playable(media) if stable_library_item_id(item) == *selected => {
                        Some(media.clone())
                    }
                    LibraryItem::Playable(_)
                    | LibraryItem::Album(_)
                    | LibraryItem::Artist(_)
                    | LibraryItem::Playlist(_)
                    | LibraryItem::Podcast(_) => None,
                })
            }
            NavigationItem::History
                if state.history().loading() || state.history().error().is_some() =>
            {
                None
            }
            NavigationItem::History => {
                let selected = state.history().selected_id()?;
                state
                    .history()
                    .entries()
                    .iter()
                    .find(|entry| entry.id == selected)
                    .map(|entry| entry.item.clone())
            }
            NavigationItem::Favorites => selected_favorite_item(state),
            NavigationItem::Home | NavigationItem::Settings => None,
        },
    }
}

fn selected_favorite_item(state: &AppState) -> Option<MediaItem> {
    if state.favorites().loading() {
        return None;
    }
    let selected = state.favorites().selected_id()?;
    state
        .favorites()
        .entries()
        .iter()
        .find(|entry| &entry.item.id == selected)
        .map(|entry| entry.item.clone())
}

fn selected_library_actions(
    controller: UiController,
    state: &AppState,
) -> (UiController, Vec<Action>) {
    let selected = state.library().selected_id();
    let item = state
        .library()
        .items()
        .iter()
        .find(|item| selected == Some(&stable_library_item_id(item)));
    match item {
        Some(LibraryItem::Playable(media)) => activate_media_list(
            controller,
            state,
            state
                .library()
                .items()
                .iter()
                .filter_map(|item| match item {
                    LibraryItem::Playable(media) => Some(media),
                    LibraryItem::Album(_)
                    | LibraryItem::Artist(_)
                    | LibraryItem::Playlist(_)
                    | LibraryItem::Podcast(_) => None,
                }),
            Some(media.id.clone()),
        ),
        Some(
            LibraryItem::Album(_)
            | LibraryItem::Artist(_)
            | LibraryItem::Playlist(_)
            | LibraryItem::Podcast(_),
        )
        | None => (controller, Vec::new()),
    }
}

fn selected_history_actions(
    controller: UiController,
    state: &AppState,
) -> (UiController, Vec<Action>) {
    let selected_id = state
        .history()
        .selected_id()
        .and_then(|selected| {
            state
                .history()
                .entries()
                .iter()
                .find(|entry| entry.id == selected)
        })
        .map(|entry| entry.item.id.clone());
    activate_media_list(
        controller,
        state,
        state.history().entries().iter().map(|entry| &entry.item),
        selected_id,
    )
}

fn selected_favorite_actions(
    controller: UiController,
    state: &AppState,
) -> (UiController, Vec<Action>) {
    let selected_id = selected_favorite_item(state).map(|item| item.id);
    activate_media_list(
        controller,
        state,
        state.favorites().entries().iter().map(|entry| &entry.item),
        selected_id,
    )
}

fn activate_media_list<'a>(
    mut controller: UiController,
    state: &AppState,
    items: impl IntoIterator<Item = &'a MediaItem>,
    selected_id: Option<crate::domain::MediaId>,
) -> (UiController, Vec<Action>) {
    let Some(selected_id) = selected_id else {
        return (controller, Vec::new());
    };
    let mut unique_ids = HashSet::new();
    let mut selected_found = false;
    let mut retained = Vec::with_capacity(MAX_EXPLICIT_LIST_ITEMS + 1);
    for item in items {
        if !unique_ids.insert(&item.id) {
            continue;
        }
        selected_found |= item.id == selected_id;
        if retained.len() <= MAX_EXPLICIT_LIST_ITEMS {
            retained.push(item.clone());
        }
    }
    if !selected_found {
        return (controller, Vec::new());
    }
    let oversized = retained.len() > MAX_EXPLICIT_LIST_ITEMS;
    let shuffle_seed = state.queue().is_shuffled().then(|| {
        let seed = controller.next_shuffle_seed;
        if !oversized {
            controller.next_shuffle_seed = controller.next_shuffle_seed.wrapping_add(1);
        }
        seed
    });
    (
        controller,
        vec![Action::PlayMediaList {
            items: retained,
            selected_id,
            shuffle_seed,
        }],
    )
}

fn move_selection(
    mut controller: UiController,
    state: &AppState,
    delta: isize,
) -> (UiController, Vec<Action>) {
    if controller.model.focus == FocusRegion::Navigation {
        return switch_navigation_view(controller, state, delta);
    }
    let action = match controller.model.focus {
        FocusRegion::Navigation => unreachable!("navigation focus returned above"),
        FocusRegion::Queue => {
            let selected = moved_value(
                state.queue().active_ids(),
                controller.model.queue_selected_id(),
                delta,
            )
            .cloned();
            controller.model.set_queue_selected_id(selected);
            None
        }
        FocusRegion::Player => None,
        FocusRegion::Content => move_content_selection(&controller, state, delta),
    };
    (controller, action.into_iter().collect())
}

fn move_content_selection(
    controller: &UiController,
    state: &AppState,
    delta: isize,
) -> Option<Action> {
    match controller.model.view {
        NavigationItem::Search => {
            let ids = state
                .search()
                .items()
                .iter()
                .map(SearchItem::stable_id)
                .collect::<Vec<_>>();
            moved_value(&ids, state.search().selected_id(), delta)
                .cloned()
                .map(|id| Action::SearchSelectionChanged { id })
        }
        NavigationItem::Charts => {
            let total = state
                .charts()
                .sections()
                .iter()
                .flat_map(ChartSection::items)
                .count();
            moved_index(total, state.charts().selected_index(), delta)
                .map(|item_index| Action::ChartRowSelectionChanged { item_index })
        }
        NavigationItem::Podcasts => {
            if let Some(show) = state.podcasts().show() {
                let ids = show
                    .episodes
                    .iter()
                    .map(|episode| episode.id.clone())
                    .collect::<Vec<_>>();
                moved_value(&ids, state.podcasts().selected_episode(), delta)
                    .cloned()
                    .map(|media_id| Action::PodcastSelectionChanged { media_id })
            } else {
                let ids = state
                    .podcasts()
                    .recommendations()
                    .iter()
                    .map(|recommendation| recommendation.source_id().clone())
                    .collect::<Vec<_>>();
                moved_value(&ids, state.podcasts().selected_recommendation(), delta)
                    .cloned()
                    .map(|id| Action::PodcastRecommendationSelectionChanged { id })
            }
        }
        NavigationItem::Library => {
            let ids = state
                .library()
                .items()
                .iter()
                .map(stable_library_item_id)
                .collect::<Vec<_>>();
            moved_value(&ids, state.library().selected_id(), delta)
                .cloned()
                .map(|id| Action::LibrarySelectionChanged { id })
        }
        NavigationItem::History => {
            let ids = state
                .history()
                .entries()
                .iter()
                .map(|entry| entry.id)
                .collect::<Vec<_>>();
            moved_value(&ids, state.history().selected_id().as_ref(), delta)
                .copied()
                .map(|id| Action::HistorySelectionChanged { id })
        }
        NavigationItem::Favorites => {
            let ids = state
                .favorites()
                .entries()
                .iter()
                .map(|entry| entry.item.id.clone())
                .collect::<Vec<_>>();
            moved_value(&ids, state.favorites().selected_id(), delta)
                .cloned()
                .map(|media_id| Action::FavoriteSelectionChanged { media_id })
        }
        NavigationItem::Home | NavigationItem::Settings => None,
    }
}

fn moved_index(total: usize, selected: Option<usize>, delta: isize) -> Option<usize> {
    if total == 0 {
        return None;
    }
    let current = selected.filter(|index| *index < total).unwrap_or(0);
    if delta.is_negative() {
        current
            .checked_sub(delta.unsigned_abs())
            .or_else(|| total.checked_sub(1))
    } else {
        Some(current.saturating_add(delta.unsigned_abs()) % total)
    }
}

fn moved_value<'a, T: PartialEq>(
    values: &'a [T],
    selected: Option<&T>,
    delta: isize,
) -> Option<&'a T> {
    if values.is_empty() {
        return None;
    }
    let current = selected
        .and_then(|selected| values.iter().position(|value| value == selected))
        .unwrap_or(0);
    let next = if delta.is_negative() {
        current
            .checked_sub(delta.unsigned_abs())
            .unwrap_or(values.len() - 1)
    } else {
        current.saturating_add(delta.unsigned_abs()) % values.len()
    };
    values.get(next)
}

fn reorder_queue(
    controller: UiController,
    state: &AppState,
    upward: bool,
) -> (UiController, Vec<Action>) {
    let Some(selected) = controller.model.queue_selected_id() else {
        return (controller, Vec::new());
    };
    let active = state.queue().active_ids();
    let Some(index) = active.iter().position(|id| id == selected) else {
        return (controller, Vec::new());
    };
    let action = if upward {
        index
            .checked_sub(1)
            .map(|previous| Action::QueueItemMovedBefore {
                id: selected.clone(),
                before: active[previous].clone(),
            })
    } else {
        active
            .get(index.saturating_add(1))
            .map(|next| Action::QueueItemMovedBefore {
                id: next.clone(),
                before: selected.clone(),
            })
    };
    (controller, action.into_iter().collect())
}

impl UiController {
    /// Reconciles transient UI state after application media changes.
    pub fn reconcile_state(&mut self, state: &AppState) {
        self.reconcile_queue_selection(state);
        self.reconcile_lyrics_media(state);
    }

    /// Reconciles width-dependent plain-lyrics scrolling after draw-area changes.
    pub fn reconcile_layout(&mut self, state: &AppState, area: Option<Rect>) {
        self.model.help_max_scroll = area
            .map(help_overlay_viewport)
            .map_or(0, |rows| HELP_LINE_COUNT.saturating_sub(rows));
        self.model.help_scroll = self.model.help_scroll.min(self.model.help_max_scroll);
        let Some(plain) = state
            .lyrics()
            .document()
            .filter(|document| document.timed().is_empty())
            .and_then(|document| document.plain())
        else {
            self.model.lyrics.plain_max_scroll = 0;
            return;
        };
        let max_scroll = area
            .and_then(lyrics_overlay_viewport)
            .map_or(0, |(rows, width)| {
                wrapped_lyrics_row_count(plain, width).saturating_sub(rows)
            });
        self.model.lyrics.plain_max_scroll = max_scroll;
        self.model.lyrics.scroll = self.model.lyrics.scroll.min(max_scroll);
        if !self.model.lyrics.follow_active {
            self.model.lyrics.selected_line = Some(self.model.lyrics.scroll);
        }
    }

    fn reconcile_lyrics_media(&mut self, state: &AppState) {
        let media_key = state.lyrics().media_id().map(|media_id| {
            let mut hasher = DefaultHasher::new();
            media_id.hash(&mut hasher);
            hasher.finish()
        });
        if self.model.lyrics.media_key != media_key {
            self.model.lyrics.media_key = media_key;
            self.model.lyrics.follow_active = true;
            self.model.lyrics.selected_line = state.lyrics().active_line_index();
            self.model.lyrics.scroll = self.model.lyrics.selected_line.unwrap_or_default();
            self.model.lyrics.plain_max_scroll = 0;
        }
    }

    fn open_country_picker(&mut self, state: &AppState) {
        let region = state.charts().region().cloned().unwrap_or_default();
        self.model.country_picker = CountryPickerState::for_region(&region);
        self.model.overlay = Some(Overlay::CountryPicker);
        self.input_mode = InputMode::Normal;
        self.input.clear();
    }

    fn sync_palette_query(&mut self) {
        if self.input_mode == InputMode::TextEntry(TextEntryContext::Palette) {
            self.model = self.model.clone().with_palette_query(&self.input);
        }
    }

    fn sync_search_draft(&mut self) {
        if self.input_mode == InputMode::TextEntry(TextEntryContext::Search) {
            self.model.set_search_draft(&self.input);
        }
    }

    fn reconcile_queue_selection(&mut self, state: &AppState) {
        let active = state.queue().active_ids();
        let selected = self
            .model
            .queue_selected_id()
            .filter(|selected| active.iter().any(|id| id == *selected))
            .cloned()
            .or_else(|| state.queue().current().map(|item| item.id().clone()))
            .or_else(|| active.first().cloned());
        self.model.set_queue_selected_id(selected);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{app::reduce, storage::FavoriteEntry};

    fn favorite(video_id: &str, title: &str, id: i64) -> FavoriteEntry {
        FavoriteEntry {
            id,
            item: MediaItem {
                id: crate::domain::MediaId {
                    provider: "youtube-music".to_owned(),
                    video_id: video_id.to_owned(),
                },
                kind: MediaKind::Song,
                title: title.to_owned(),
                creators: vec!["Favorite Artist".to_owned()],
                collection: None,
                duration_ms: Some(180_000),
                artwork_url: None,
                explicit: false,
            },
            favorited_at: id,
        }
    }

    #[test]
    fn explicit_list_favorites_helper_uses_all_loaded_unique_entries_and_selected_id() {
        let first = favorite("favorite-first", "Favorite First", 1);
        let duplicate = FavoriteEntry {
            id: 2,
            item: MediaItem {
                title: "Later Duplicate".to_owned(),
                ..first.item.clone()
            },
            favorited_at: 2,
        };
        let selected = favorite("favorite-selected", "Favorite Selected", 3);
        let (state, effects) = reduce(AppState::default(), Action::FavoritesRequested);
        let [crate::app::Effect::LoadFavorites { generation }] = effects.as_slice() else {
            panic!("favorites fixture must load");
        };
        let (state, _) = reduce(
            state,
            Action::FavoritesCompleted {
                generation: *generation,
                result: Ok(vec![first.clone(), duplicate, selected.clone()]),
            },
        );
        let (state, _) = reduce(
            state,
            Action::FavoriteSelectionChanged {
                media_id: selected.item.id.clone(),
            },
        );
        let (state, _) = reduce(
            state,
            Action::ShuffleEnabledChanged {
                enabled: true,
                seed: 1,
            },
        );

        assert_eq!(selected_favorite_item(&state), Some(selected.item.clone()));

        let (_, actions) =
            selected_favorite_actions(UiController::default().with_shuffle_seed(55), &state);
        assert_eq!(
            actions,
            vec![Action::PlayMediaList {
                items: vec![first.item, selected.item.clone()],
                selected_id: selected.item.id,
                shuffle_seed: Some(55),
            }]
        );
    }
}
