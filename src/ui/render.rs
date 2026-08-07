use std::{
    collections::hash_map::DefaultHasher,
    fmt,
    hash::{Hash, Hasher},
    ops::Range,
};

use ratatui::{
    Frame,
    buffer::CellWidth,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
};
use unicode_segmentation::UnicodeSegmentation;

use crate::{
    app::AppState,
    domain::{MediaKind, PlaybackStatus},
    lyrics::normalize_lyrics_text,
    queue::{Queue, QueueItemId},
};

use super::{
    animation::{AnimationFrameStore, AnimationKey},
    artwork::{
        ArtworkPresentation, ArtworkPresentationStore, ArtworkWidget, CellSize,
        PRODUCTION_ARTWORK_SIZE,
    },
    controller::{BrowserPickerState, CountryChoice, CountryPickerState},
    input::{PaletteEntry, SemanticAction, palette_entries},
    interaction::{HitTarget, InteractionMap, ListSurface, RenderedRowTarget},
    layout::LayoutMode,
    motion::{MotionFrame, ProgressPresentation, SPINNER_FRAMES, SelectionMotion},
    spectrum::{
        MAX_SPECTRUM_BANDS, MAX_SPECTRUM_LEVEL, SpectrumPresentation, effective_spectrum_fps,
    },
    theme::{ColorCapability, Theme},
};

const MAX_RENDERED_ROWS: usize = 128;
/// Compact mode clips the existing bounded static grid to a thumbnail instead
/// of starting a second fetch or introducing a layout-keyed cache.
const COMPACT_ARTWORK_SIZE: CellSize = CellSize::new(12, 5);
const COMPACT_ARTWORK_PANEL_WIDTH: u16 = COMPACT_ARTWORK_SIZE.width + 2;
const COMPACT_ARTWORK_PANEL_HEIGHT: u16 = COMPACT_ARTWORK_SIZE.height + 2;
/// Content and queue rows retain this many columns at the compact boundary.
const COMPACT_MIN_MAIN_WIDTH: u16 = 44;
/// Tiny lyrics replace only a sufficiently roomy identity field; playback
/// status, telemetry, and any visible controls retain their existing budget.
const TINY_LYRICS_MIN_IDENTITY_WIDTH: u16 = 8;
/// Maximum UTF-8 bytes retained from one external text fragment before
/// palette storage, formatting, or clipping.
pub const CLIP_BYTE_INSPECTION_BUDGET: usize = 64 * 1024;
/// Maximum extended grapheme clusters retained or inspected from one external
/// text fragment.
pub const CLIP_GRAPHEME_INSPECTION_BUDGET: usize = 4 * 1024;
const CLIP_SPAN_INSPECTION_BUDGET: usize = 4 * 1024;
const HELP_TEXT: [&str; 13] = [
    "Navigation     ↑ ↓ ← → or h j k l",
    "Search         /",
    "Commands       :",
    "Play / pause   Space · F8 · Media Play/Pause",
    "Track          n/F9/Media Next · p/F7/Media Previous",
    "Seek           Shift+Left back · Shift+Right forward",
    "Volume         + / -",
    "Queue modes    s shuffle · r repeat · e radio",
    "Queue order    [ up · ] down",
    "Data           a connect · m more · d recheck",
    "Region         c country",
    "Close          Esc",
    "Quit           q or Ctrl-c",
];
pub(crate) const HELP_LINE_COUNT: usize = HELP_TEXT.len();

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum DatasetUpdate {
    Replace,
    AppendInProgress,
    Reconcile,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct OrderedDatasetKey {
    context: u64,
    generation: u64,
    identities: Vec<u64>,
    update: DatasetUpdate,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) enum DatasetKey {
    Scalar(u64),
    Ordered(OrderedDatasetKey),
}

impl From<u64> for DatasetKey {
    fn from(value: u64) -> Self {
        Self::Scalar(value)
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct SelectionViewport {
    start: usize,
    dataset_key: Option<DatasetKey>,
    selected_identity: Option<u64>,
    selected_offset: usize,
}

impl SelectionViewport {
    pub(crate) fn visible_range<K: Into<DatasetKey>>(
        &mut self,
        total: usize,
        selected: Option<usize>,
        max_rows: usize,
        dataset_key: K,
    ) -> Range<usize> {
        let dataset_key = dataset_key.into();
        if total == 0 {
            self.start = 0;
            self.dataset_key = Some(dataset_key);
            self.selected_identity = None;
            self.selected_offset = 0;
            return 0..0;
        }
        let selected = selected.filter(|selected| *selected < total);

        let preserves_dataset = self
            .dataset_key
            .as_ref()
            .is_some_and(|previous| dataset_transition_preserves(previous, &dataset_key));
        if !preserves_dataset {
            self.start = self
                .dataset_key
                .as_ref()
                .and_then(|previous| {
                    remapped_viewport_start(
                        previous,
                        &dataset_key,
                        self.start,
                        selected,
                        self.selected_identity,
                        self.selected_offset,
                    )
                })
                .unwrap_or(0);
        }
        self.dataset_key = Some(dataset_key);
        if max_rows == 0 {
            self.start = self.start.min(total.saturating_sub(1));
            return 0..0;
        }

        let max_start = total.saturating_sub(max_rows);
        self.start = self.start.min(max_start);
        if let Some(selected) = selected {
            if selected < self.start {
                self.start = selected;
            } else if selected >= self.start.saturating_add(max_rows) {
                self.start = selected.saturating_add(1).saturating_sub(max_rows);
            }
        }
        self.start = self.start.min(max_start);
        self.update_selection_anchor(selected);
        self.start..self.start.saturating_add(max_rows).min(total)
    }

    fn update_selection_anchor(&mut self, selected: Option<usize>) {
        let Some(selected) = selected else {
            self.selected_identity = None;
            self.selected_offset = 0;
            return;
        };
        self.selected_identity = match self.dataset_key.as_ref() {
            Some(DatasetKey::Ordered(key)) => key.identities.get(selected).copied(),
            Some(DatasetKey::Scalar(_)) | None => None,
        };
        self.selected_offset = selected.saturating_sub(self.start);
    }

    pub(crate) fn row_target(
        &self,
        line_index: usize,
        surface: ListSurface,
        stable_index: usize,
    ) -> RenderedRowTarget {
        RenderedRowTarget {
            line_index,
            surface,
            stable_index,
            dataset_key: self.dataset_key.clone().unwrap_or(DatasetKey::Scalar(0)),
        }
    }
}

fn remapped_viewport_start(
    previous: &DatasetKey,
    next: &DatasetKey,
    previous_start: usize,
    selected: Option<usize>,
    selected_identity: Option<u64>,
    selected_offset: usize,
) -> Option<usize> {
    let (DatasetKey::Ordered(previous), DatasetKey::Ordered(next)) = (previous, next) else {
        return None;
    };
    if previous.context != next.context {
        return None;
    }
    if next.update != DatasetUpdate::Reconcile {
        return None;
    }
    if let (Some(selected), Some(selected_identity)) = (selected, selected_identity)
        && (next.identities.get(selected) == Some(&selected_identity)
            || !next.identities.contains(&selected_identity))
    {
        return Some(selected.saturating_sub(selected_offset));
    }
    let top_identity = previous.identities.get(previous_start)?;
    next.identities
        .iter()
        .position(|identity| identity == top_identity)
}

#[doc(hidden)]
#[derive(Debug, Default)]
pub struct ViewportMemory {
    pub(crate) search: SelectionViewport,
    pub(crate) charts: SelectionViewport,
    pub(crate) podcast_recommendations: SelectionViewport,
    pub(crate) podcast_episodes: SelectionViewport,
    pub(crate) library: SelectionViewport,
    pub(crate) favorites: SelectionViewport,
    pub(crate) history: SelectionViewport,
    pub(crate) queue: SelectionViewport,
    pub(crate) country_picker: SelectionViewport,
    pub(crate) browser_picker: SelectionViewport,
    selection_motion: Box<SelectionMotionMemory>,
    progress_motion_regions: Vec<Rect>,
    spinner_motion_regions: Vec<Rect>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct SelectionPresentation {
    pub(crate) logical_index: Option<usize>,
    pub(crate) cursor_index: Option<usize>,
    pub(crate) transitioning: bool,
}

#[derive(Clone, Debug, Default)]
struct SelectionMotionSlot {
    motion: SelectionMotion,
    dataset_key: Option<DatasetKey>,
    visible_range: Range<usize>,
    selected_identity: Option<u64>,
    motion_region: Option<Rect>,
    seen: bool,
}

#[derive(Debug)]
struct SelectionMotionMemory {
    slots: [SelectionMotionSlot; LIST_SURFACE_COUNT],
    frame_area: Option<Rect>,
    frame_now_ms: u64,
}

const LIST_SURFACE_COUNT: usize = 13;

impl Default for SelectionMotionMemory {
    fn default() -> Self {
        Self {
            slots: std::array::from_fn(|_| SelectionMotionSlot::default()),
            frame_area: None,
            frame_now_ms: 0,
        }
    }
}

impl ViewportMemory {
    fn begin_selection_frame(&mut self, area: Rect, now_ms: u64) {
        if self
            .selection_motion
            .frame_area
            .is_some_and(|previous| previous != area)
        {
            self.selection_motion.slots = std::array::from_fn(|_| SelectionMotionSlot::default());
        }
        self.selection_motion.frame_area = Some(area);
        self.selection_motion.frame_now_ms = now_ms;
        self.progress_motion_regions.clear();
        self.spinner_motion_regions.clear();
        for slot in &mut self.selection_motion.slots {
            slot.seen = false;
        }
    }

    fn present_selection(
        &mut self,
        surface: ListSurface,
        total: usize,
        logical_index: Option<usize>,
        visible_range: Range<usize>,
        dataset_key: DatasetKey,
        now_ms: u64,
    ) -> SelectionPresentation {
        let slot = &mut self.selection_motion.slots[list_surface_index(surface)];
        slot.seen = true;
        let logical_index = logical_index.filter(|index| *index < total);
        let selected_identity =
            logical_index.and_then(|index| dataset_identity(&dataset_key, index));
        let Some(logical_index) = logical_index.filter(|index| visible_range.contains(index))
        else {
            slot.motion.reset();
            slot.dataset_key = Some(dataset_key);
            slot.visible_range = visible_range;
            slot.selected_identity = selected_identity;
            return SelectionPresentation {
                logical_index,
                ..SelectionPresentation::default()
            };
        };
        let target_offset = logical_index.saturating_sub(visible_range.start);
        let compatible = slot.dataset_key.as_ref().is_some_and(|previous| {
            slot.visible_range == visible_range
                && selection_dataset_preserves(
                    previous,
                    &dataset_key,
                    slot.selected_identity,
                    selected_identity,
                )
        });
        if compatible {
            if slot.motion.current_index() != Some(target_offset) {
                slot.motion.retarget(target_offset, now_ms);
            }
        } else {
            slot.motion.snap(target_offset, now_ms);
        }
        slot.dataset_key = Some(dataset_key);
        slot.visible_range = visible_range.clone();
        slot.selected_identity = selected_identity;
        let cursor_index = slot
            .motion
            .rounded_index(now_ms)
            .map(|offset| visible_range.start.saturating_add(offset))
            .filter(|index| visible_range.contains(index));
        SelectionPresentation {
            logical_index: Some(logical_index),
            cursor_index,
            transitioning: slot.motion.is_transitioning(now_ms),
        }
    }

    fn end_selection_frame(&mut self) {
        for slot in &mut self.selection_motion.slots {
            if !slot.seen {
                *slot = SelectionMotionSlot::default();
            }
        }
    }

    pub(crate) fn selection_transitioning(&self) -> bool {
        self.selection_motion.slots.iter().any(|slot| {
            slot.seen
                && slot
                    .motion
                    .is_transitioning(self.selection_motion.frame_now_ms)
        })
    }

    pub(crate) const fn progress_motion_visible(&self) -> bool {
        !self.progress_motion_regions.is_empty()
    }

    pub(crate) const fn spinner_motion_visible(&self) -> bool {
        !self.spinner_motion_regions.is_empty()
    }

    fn observe_spinner_lines(&mut self, lines: &[Line<'_>], area: Rect) {
        for (index, line) in lines.iter().enumerate() {
            let text = line_text(line);
            if SPINNER_FRAMES.iter().any(|frame| text.starts_with(frame))
                && let Ok(offset) = u16::try_from(index)
            {
                self.spinner_motion_regions.push(Rect::new(
                    area.x,
                    area.y.saturating_add(offset),
                    text.cell_width().min(area.width),
                    1,
                ));
            }
        }
    }

    fn set_selection_motion_region(&mut self, surface: ListSurface, region: Option<Rect>) {
        self.selection_motion.slots[list_surface_index(surface)].motion_region = region;
    }

    fn occlude_motion(&mut self, popup: Rect) {
        self.progress_motion_regions
            .retain(|region| !rect_contains(popup, *region));
        self.spinner_motion_regions
            .retain(|region| !rect_contains(popup, *region));
        for slot in &mut self.selection_motion.slots {
            if slot
                .motion_region
                .is_some_and(|region| rect_contains(popup, region))
            {
                slot.seen = false;
            }
        }
    }
}

const fn rect_contains(outer: Rect, inner: Rect) -> bool {
    inner.x >= outer.x
        && inner.y >= outer.y
        && inner.right() <= outer.right()
        && inner.bottom() <= outer.bottom()
}

const fn list_surface_index(surface: ListSurface) -> usize {
    match surface {
        ListSurface::Home => 0,
        ListSurface::Search => 1,
        ListSurface::Charts => 2,
        ListSurface::PodcastRecommendations => 3,
        ListSurface::PodcastEpisodes => 4,
        ListSurface::Library => 5,
        ListSurface::Favorites => 6,
        ListSurface::History => 7,
        ListSurface::Queue => 8,
        ListSurface::CommandPalette => 9,
        ListSurface::CountryPicker => 10,
        ListSurface::BrowserPicker => 11,
        ListSurface::Lyrics => 12,
    }
}

fn dataset_identity(key: &DatasetKey, index: usize) -> Option<u64> {
    match key {
        DatasetKey::Ordered(key) => key.identities.get(index).copied(),
        DatasetKey::Scalar(_) => None,
    }
}

fn selection_dataset_preserves(
    previous: &DatasetKey,
    next: &DatasetKey,
    previous_identity: Option<u64>,
    next_identity: Option<u64>,
) -> bool {
    if dataset_transition_preserves(previous, next) {
        return true;
    }
    let (DatasetKey::Ordered(previous), DatasetKey::Ordered(next)) = (previous, next) else {
        return false;
    };
    previous.context == next.context
        && next.update == DatasetUpdate::Reconcile
        && previous_identity.is_some()
        && previous_identity == next_identity
}

pub(crate) fn dataset_key<T: Hash + ?Sized>(value: &T) -> u64 {
    hash_value(value)
}

pub(crate) fn ordered_dataset_key<C, G, I, T>(
    context: &C,
    generation: &G,
    identities: I,
    update: DatasetUpdate,
) -> DatasetKey
where
    C: Hash + ?Sized,
    G: Hash + ?Sized,
    I: IntoIterator<Item = T>,
    T: Hash,
{
    DatasetKey::Ordered(OrderedDatasetKey {
        context: hash_value(context),
        generation: hash_value(generation),
        identities: identities
            .into_iter()
            .map(|identity| hash_value(&identity))
            .collect(),
        update,
    })
}

fn dataset_transition_preserves(previous: &DatasetKey, next: &DatasetKey) -> bool {
    if previous == next {
        return true;
    }
    let (DatasetKey::Ordered(previous), DatasetKey::Ordered(next)) = (previous, next) else {
        return false;
    };
    if previous.context != next.context {
        return false;
    }
    (next.update == DatasetUpdate::AppendInProgress && previous.identities == next.identities)
        || (previous.update == DatasetUpdate::AppendInProgress
            && next.update == DatasetUpdate::Replace
            && previous.generation == next.generation
            && next.identities.starts_with(&previous.identities))
}

fn hash_value<T: Hash + ?Sized>(value: &T) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum NavigationItem {
    #[default]
    Home,
    Search,
    Charts,
    Podcasts,
    Library,
    Favorites,
    History,
    Settings,
}

impl NavigationItem {
    pub const ALL: [Self; 8] = [
        Self::Home,
        Self::Search,
        Self::Charts,
        Self::Podcasts,
        Self::Library,
        Self::Favorites,
        Self::History,
        Self::Settings,
    ];

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Home => "Home",
            Self::Search => "Search",
            Self::Charts => "Charts",
            Self::Podcasts => "Podcasts",
            Self::Library => "Library",
            Self::Favorites => "Favorites",
            Self::History => "History",
            Self::Settings => "Settings",
        }
    }

    #[must_use]
    pub const fn compact_label(self) -> &'static str {
        match self {
            Self::Podcasts => "Pods",
            Self::Favorites => "Favs",
            Self::Home
            | Self::Search
            | Self::Charts
            | Self::Library
            | Self::History
            | Self::Settings => self.label(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum FocusRegion {
    Navigation,
    #[default]
    Content,
    Queue,
    Player,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Overlay {
    Help,
    CommandPalette,
    CountryPicker,
    BrowserPicker,
    Lyrics,
}

#[derive(Clone, Eq, Hash, PartialEq)]
pub struct LyricsOverlayState {
    pub(crate) follow_active: bool,
    pub(crate) selected_line: Option<usize>,
    pub(crate) scroll: usize,
    pub(crate) media_key: Option<u64>,
    pub(crate) plain_max_scroll: usize,
}

impl fmt::Debug for LyricsOverlayState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LyricsOverlayState")
            .field("follow_active", &self.follow_active)
            .field("selected_line", &self.selected_line)
            .field("scroll", &self.scroll)
            .field("has_media", &self.media_key.is_some())
            .field("plain_max_scroll", &self.plain_max_scroll)
            .finish()
    }
}

impl LyricsOverlayState {
    #[must_use]
    pub const fn follow_active(&self) -> bool {
        self.follow_active
    }

    #[must_use]
    pub const fn selected_line(&self) -> Option<usize> {
        self.selected_line
    }

    #[must_use]
    pub const fn scroll(&self) -> usize {
        self.scroll
    }
}

impl Default for LyricsOverlayState {
    fn default() -> Self {
        Self {
            follow_active: true,
            selected_line: None,
            scroll: 0,
            media_key: None,
            plain_max_scroll: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum CompactPanel {
    #[default]
    Content,
    Queue,
}

#[derive(Clone, Debug, Default, Eq, Hash, PartialEq)]
pub struct CommandPaletteState {
    query: String,
    query_truncated: bool,
    selected: usize,
    scroll: usize,
}

impl CommandPaletteState {
    #[must_use]
    pub fn query(&self) -> &str {
        &self.query
    }

    #[must_use]
    pub const fn selected_index(&self) -> usize {
        self.selected
    }

    #[must_use]
    pub const fn scroll(&self) -> usize {
        self.scroll
    }

    #[must_use]
    pub fn selected_action(&self) -> Option<SemanticAction> {
        let entries = self.matching_entries();
        let selected = self.normalized_selected(entries.len())?;
        entries.get(selected).map(|entry| entry.action)
    }

    #[must_use]
    pub fn viewport(&self, max_rows: usize) -> PaletteViewport {
        let entries = self.matching_entries();
        let total = entries.len();
        let selected = self.normalized_selected(total);
        let max_start = total.saturating_sub(max_rows);
        let mut start = self.scroll.min(max_start);
        if let Some(selected) = selected
            && max_rows > 0
        {
            if selected < start {
                start = selected;
            } else if selected >= start.saturating_add(max_rows) {
                start = selected.saturating_add(1).saturating_sub(max_rows);
            }
        }
        start = start.min(max_start);
        let end = start.saturating_add(max_rows).min(total);
        PaletteViewport {
            start,
            total,
            selected,
            entries: entries[start..end].to_vec(),
        }
    }

    pub(crate) fn matching_entries(&self) -> Vec<PaletteEntry> {
        if self.query_truncated {
            return Vec::new();
        }
        let query = self.query.trim().to_ascii_lowercase();
        palette_entries()
            .iter()
            .copied()
            .filter(|entry| {
                query.is_empty()
                    || entry.label.to_ascii_lowercase().contains(&query)
                    || entry.shortcut.to_ascii_lowercase().contains(&query)
            })
            .collect()
    }

    const fn normalized_selected(&self, total: usize) -> Option<usize> {
        if total == 0 {
            None
        } else {
            Some(if self.selected < total {
                self.selected
            } else {
                total - 1
            })
        }
    }

    pub(crate) fn move_by(&mut self, delta: isize) {
        let total = self.matching_entries().len();
        let Some(selected) = self.normalized_selected(total) else {
            return;
        };
        self.selected = if delta.is_negative() {
            selected
                .checked_sub(delta.unsigned_abs())
                .unwrap_or(total - 1)
        } else {
            selected.saturating_add(delta.unsigned_abs()) % total
        };
    }

    pub(crate) fn select(&mut self, index: usize) {
        self.selected = index;
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PaletteViewport {
    pub start: usize,
    pub total: usize,
    pub selected: Option<usize>,
    pub entries: Vec<PaletteEntry>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RenderModel {
    pub view: NavigationItem,
    pub focus: FocusRegion,
    pub overlay: Option<Overlay>,
    pub compact_panel: CompactPanel,
    pub palette: CommandPaletteState,
    pub country_picker: CountryPickerState,
    pub browser_picker: BrowserPickerState,
    pub lyrics: LyricsOverlayState,
    pub(crate) help_scroll: usize,
    pub(crate) help_max_scroll: usize,
    queue_selected_id: Option<QueueItemId>,
    search_draft: String,
    search_editing: bool,
    visualizer_max_fps: u8,
    motion_frame: MotionFrame,
    motion_frame_set: bool,
}

#[derive(Clone, Copy)]
struct EffectiveRenderModel<'a> {
    view: NavigationItem,
    focus: FocusRegion,
    overlay: Option<Overlay>,
    compact_panel: CompactPanel,
    palette: &'a CommandPaletteState,
    country_picker: &'a CountryPickerState,
    browser_picker: &'a BrowserPickerState,
    lyrics: &'a LyricsOverlayState,
    help_scroll: usize,
    queue_selected_id: Option<&'a QueueItemId>,
    search_draft: Option<&'a str>,
    motion_frame: MotionFrame,
    motion_frame_set: bool,
}

impl RenderModel {
    #[must_use]
    pub const fn with_view(mut self, view: NavigationItem) -> Self {
        self.view = view;
        self
    }

    #[must_use]
    pub const fn with_focus(mut self, focus: FocusRegion) -> Self {
        self.focus = focus;
        self
    }

    #[must_use]
    pub const fn with_visualizer_max_fps(mut self, max_fps: u8) -> Self {
        self.visualizer_max_fps = effective_spectrum_fps(max_fps);
        self
    }

    #[must_use]
    pub const fn with_motion_frame(mut self, frame: MotionFrame) -> Self {
        self.motion_frame = frame;
        self.motion_frame_set = true;
        self
    }

    #[must_use]
    pub const fn motion_frame(&self) -> MotionFrame {
        self.motion_frame
    }

    pub(crate) fn set_motion_frame(&mut self, frame: MotionFrame) {
        self.motion_frame = frame;
        self.motion_frame_set = true;
    }

    #[must_use]
    pub const fn with_overlay(mut self, overlay: Overlay) -> Self {
        self.overlay = Some(overlay);
        self
    }

    #[must_use]
    pub fn with_palette_query(mut self, query: impl AsRef<str>) -> Self {
        let (query, truncated) = bounded_grapheme_prefix(
            query.as_ref(),
            CLIP_BYTE_INSPECTION_BUDGET,
            CLIP_GRAPHEME_INSPECTION_BUDGET,
        );
        query.clone_into(&mut self.palette.query);
        self.palette.query_truncated = truncated;
        self.palette.selected = 0;
        self.palette.scroll = 0;
        self
    }

    #[must_use]
    pub const fn with_palette_selection(mut self, selected: usize) -> Self {
        self.palette.selected = selected;
        self
    }

    #[must_use]
    pub const fn with_palette_scroll(mut self, scroll: usize) -> Self {
        self.palette.scroll = scroll;
        self
    }

    #[must_use]
    pub const fn toggle_compact_panel(mut self) -> Self {
        match self.compact_panel {
            CompactPanel::Content => {
                self.compact_panel = CompactPanel::Queue;
                self.focus = FocusRegion::Queue;
            }
            CompactPanel::Queue => {
                self.compact_panel = CompactPanel::Content;
                self.focus = FocusRegion::Content;
            }
        }
        self
    }

    pub(crate) fn set_search_draft(&mut self, draft: &str) {
        draft.clone_into(&mut self.search_draft);
        self.search_editing = true;
    }

    pub(crate) fn clear_search_draft(&mut self) {
        self.search_draft.clear();
        self.search_editing = false;
    }

    #[must_use]
    pub fn search_draft(&self) -> Option<&str> {
        self.search_editing.then_some(self.search_draft.as_str())
    }

    #[must_use]
    pub const fn queue_selected_id(&self) -> Option<&QueueItemId> {
        self.queue_selected_id.as_ref()
    }

    pub(crate) fn set_queue_selected_id(&mut self, selected: Option<QueueItemId>) {
        self.queue_selected_id = selected;
    }

    /// Returns the UI-only focus and compact-panel state that is visible in
    /// `layout`, leaving the controller's stored model unchanged.
    #[must_use]
    pub fn normalized_for_layout(&self, layout: LayoutMode) -> Self {
        let effective = self.effective_for_layout(layout);
        let mut normalized = self.clone();
        normalized.focus = effective.focus;
        normalized.compact_panel = effective.compact_panel;
        normalized
    }

    fn effective_for_layout(&self, layout: LayoutMode) -> EffectiveRenderModel<'_> {
        let mut focus = self.focus;
        let mut compact_panel = self.compact_panel;
        match layout {
            LayoutMode::Wide => {}
            LayoutMode::Compact => match focus {
                FocusRegion::Content => compact_panel = CompactPanel::Content,
                FocusRegion::Queue => compact_panel = CompactPanel::Queue,
                FocusRegion::Navigation | FocusRegion::Player => {}
            },
            LayoutMode::Tiny => {
                compact_panel = CompactPanel::Content;
                focus = FocusRegion::Content;
            }
        }
        EffectiveRenderModel {
            view: self.view,
            focus,
            overlay: self.overlay,
            compact_panel,
            palette: &self.palette,
            country_picker: &self.country_picker,
            browser_picker: &self.browser_picker,
            lyrics: &self.lyrics,
            help_scroll: self.help_scroll,
            queue_selected_id: self.queue_selected_id.as_ref(),
            search_draft: self.search_editing.then_some(&self.search_draft),
            motion_frame: self.motion_frame,
            motion_frame_set: self.motion_frame_set,
        }
    }
}

impl Default for RenderModel {
    fn default() -> Self {
        Self {
            view: NavigationItem::Home,
            focus: FocusRegion::Content,
            overlay: None,
            compact_panel: CompactPanel::Content,
            palette: CommandPaletteState::default(),
            country_picker: CountryPickerState::default(),
            browser_picker: BrowserPickerState::default(),
            lyrics: LyricsOverlayState::default(),
            help_scroll: 0,
            help_max_scroll: 0,
            queue_selected_id: None,
            search_draft: String::new(),
            search_editing: false,
            visualizer_max_fps: 15,
            motion_frame: MotionFrame::default(),
            motion_frame_set: false,
        }
    }
}

/// Renders the default application presentation without performing I/O or
/// mutating application state.
pub fn render(frame: &mut Frame<'_>, state: &AppState, theme: &Theme) {
    render_with_model(frame, state, theme, &RenderModel::default());
}

/// Renders a prepared artwork presentation without fetching or decoding during
/// the terminal draw.
pub fn render_artwork(
    frame: &mut Frame<'_>,
    area: Rect,
    presentation: &ArtworkPresentation,
    capability: ColorCapability,
) {
    frame.render_widget(ArtworkWidget::new(presentation, capability), area);
}

fn render_fitted_artwork(
    frame: &mut Frame<'_>,
    area: Rect,
    presentation: &ArtworkPresentation,
    capability: ColorCapability,
) {
    frame.render_widget(ArtworkWidget::new_fitted(presentation, capability), area);
}

/// Applies the artwork policy for one layout without performing draw-time I/O.
///
/// Wide mode may use a current playing or safely frozen animation, compact mode
/// is static-only, and tiny mode does not inspect either store.
pub(crate) fn artwork_presentation_from_stores(
    state: &AppState,
    artwork: Option<&ArtworkPresentationStore>,
    animation: Option<&AnimationFrameStore>,
    size: CellSize,
    layout: LayoutMode,
) -> Option<ArtworkPresentation> {
    if layout == LayoutMode::Tiny {
        return None;
    }
    let static_presentation = || {
        let store = artwork?;
        Some(match state.artwork().requested_url() {
            Some(url) => store
                .presentation(state.artwork().generation(), url)
                .unwrap_or_else(ArtworkPresentation::unavailable),
            None => ArtworkPresentation::unavailable(),
        })
    };
    let animated = || {
        if layout != LayoutMode::Wide
            || !state.animated_artwork_enabled()
            || !matches!(
                state.playback().status,
                PlaybackStatus::Playing | PlaybackStatus::Paused
            )
        {
            return None;
        }
        let generation = state.current_attempt_generation()?;
        let media_id = state.playback().current.as_ref()?;
        let current = state.queue().current()?.media();
        if current.kind != MediaKind::Video || &current.id != media_id {
            return None;
        }
        let key = AnimationKey::new(generation, media_id.clone(), size);
        animation?.presentation(&key).map(ArtworkPresentation::Grid)
    };
    animated().or_else(static_presentation)
}

/// Renders a deterministic UI-only presentation around immutable app state.
///
/// `RenderModel` holds temporary focus and overlay choices so the application
/// reducer remains free of terminal concerns.
pub fn render_with_model(
    frame: &mut Frame<'_>,
    state: &AppState,
    theme: &Theme,
    model: &RenderModel,
) {
    render_with_model_inner(
        frame,
        state,
        theme,
        model,
        RenderEnhancements::new(None, None, model.visualizer_max_fps),
        &mut ViewportMemory::default(),
        None,
    );
}

/// Renders with persistent viewport and motion presentation state.
///
/// This is primarily useful to deterministic render harnesses that need to
/// advance an explicit [`MotionFrame`] across multiple frames.
#[doc(hidden)]
pub fn render_with_model_and_motion_memory(
    frame: &mut Frame<'_>,
    state: &AppState,
    theme: &Theme,
    model: &RenderModel,
    memory: &mut ViewportMemory,
) {
    render_with_model_inner(
        frame,
        state,
        theme,
        model,
        RenderEnhancements::new(None, None, model.visualizer_max_fps),
        memory,
        None,
    );
}

/// Renders with persistent motion state while recording the frame's hit map.
#[doc(hidden)]
pub fn render_with_model_and_motion_memory_and_interactions(
    frame: &mut Frame<'_>,
    state: &AppState,
    theme: &Theme,
    model: &RenderModel,
    memory: &mut ViewportMemory,
    interactions: &mut InteractionMap,
) {
    render_with_model_inner(
        frame,
        state,
        theme,
        model,
        RenderEnhancements::new(None, None, model.visualizer_max_fps),
        memory,
        Some(interactions),
    );
}

/// Renders while recording bounded hit geometry for the same completed frame.
pub fn render_with_model_and_interactions(
    frame: &mut Frame<'_>,
    state: &AppState,
    theme: &Theme,
    model: &RenderModel,
    interactions: &mut InteractionMap,
) {
    render_with_model_inner(
        frame,
        state,
        theme,
        model,
        RenderEnhancements::new(None, None, model.visualizer_max_fps),
        &mut ViewportMemory::default(),
        Some(interactions),
    );
}

/// Renders the application with one already-bounded artwork presentation.
pub fn render_with_model_and_artwork(
    frame: &mut Frame<'_>,
    state: &AppState,
    theme: &Theme,
    model: &RenderModel,
    artwork: &ArtworkPresentation,
) {
    render_with_model_inner(
        frame,
        state,
        theme,
        model,
        RenderEnhancements::new(Some(artwork), None, model.visualizer_max_fps),
        &mut ViewportMemory::default(),
        None,
    );
}

/// Renders the application with one already-bounded spectrum presentation.
pub fn render_with_model_and_spectrum(
    frame: &mut Frame<'_>,
    state: &AppState,
    theme: &Theme,
    model: &RenderModel,
    spectrum: &SpectrumPresentation,
) {
    render_with_model_inner(
        frame,
        state,
        theme,
        model,
        RenderEnhancements::new(None, Some(spectrum), model.visualizer_max_fps),
        &mut ViewportMemory::default(),
        None,
    );
}

pub(crate) fn render_with_model_and_viewports(
    frame: &mut Frame<'_>,
    state: &AppState,
    theme: &Theme,
    model: &RenderModel,
    enhancements: RenderEnhancements<'_>,
    viewports: &mut ViewportMemory,
) {
    render_with_model_inner(frame, state, theme, model, enhancements, viewports, None);
}

pub(crate) fn render_with_model_and_viewports_and_interactions(
    frame: &mut Frame<'_>,
    state: &AppState,
    theme: &Theme,
    model: &RenderModel,
    enhancements: RenderEnhancements<'_>,
    viewports: &mut ViewportMemory,
    interactions: &mut InteractionMap,
) {
    render_with_model_inner(
        frame,
        state,
        theme,
        model,
        enhancements,
        viewports,
        Some(interactions),
    );
}

fn render_with_model_inner(
    frame: &mut Frame<'_>,
    state: &AppState,
    theme: &Theme,
    model: &RenderModel,
    enhancements: RenderEnhancements<'_>,
    viewports: &mut ViewportMemory,
    mut interactions: Option<&mut InteractionMap>,
) {
    let area = frame.area();
    if area.width == 0 || area.height == 0 {
        return;
    }
    viewports.begin_selection_frame(area, model.motion_frame.elapsed_ms);

    frame.render_widget(
        Block::new().style(Style::default().fg(theme.foreground).bg(theme.background)),
        area,
    );

    let layout = LayoutMode::for_area(area);
    let model = model.effective_for_layout(layout);
    let enhancements = RenderEnhancements {
        spectrum: state
            .visualizer_enabled()
            .then_some(enhancements.spectrum)
            .flatten(),
        ..enhancements
    };
    match layout {
        LayoutMode::Wide => render_wide(
            frame,
            area,
            state,
            theme,
            &model,
            enhancements,
            viewports,
            interactions.as_deref_mut(),
        ),
        LayoutMode::Compact => {
            render_compact(
                frame,
                area,
                state,
                theme,
                &model,
                enhancements,
                viewports,
                interactions.as_deref_mut(),
            );
        }
        LayoutMode::Tiny => render_tiny(
            frame,
            area,
            state,
            theme,
            &model,
            enhancements,
            viewports,
            interactions.as_deref_mut(),
        ),
    }

    if let Some(overlay) = model.overlay {
        if let Some(interactions) = interactions.as_deref_mut() {
            interactions.clear();
        }
        render_overlay(
            frame,
            area,
            state,
            theme,
            overlay,
            &model,
            viewports,
            interactions,
        );
    }
    viewports.end_selection_frame();
}

#[derive(Clone, Copy)]
pub(crate) struct RenderEnhancements<'a> {
    artwork: Option<&'a ArtworkPresentation>,
    spectrum: Option<&'a SpectrumPresentation>,
    visualizer_max_fps: u8,
}

impl<'a> RenderEnhancements<'a> {
    pub(crate) const fn new(
        artwork: Option<&'a ArtworkPresentation>,
        spectrum: Option<&'a SpectrumPresentation>,
        visualizer_max_fps: u8,
    ) -> Self {
        Self {
            artwork,
            spectrum,
            visualizer_max_fps,
        }
    }
}

#[must_use]
pub fn truncate_cells(input: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }

    let (prefix, budget_cut) = capped_str(input);
    let Some(boundary) = truncation_boundary(prefix, max_width, budget_cut) else {
        return input.to_owned();
    };

    let mut truncated = String::with_capacity(boundary + '…'.len_utf8());
    truncated.push_str(&input[..boundary]);
    truncated.push('…');
    truncated
}

/// Clips a styled line at an extended-grapheme boundary while retaining the
/// original line and span styles.
#[must_use]
pub fn clip_line(line: &Line<'static>, max_width: usize) -> Line<'static> {
    let (logical, budget_cut) = capped_line_text(line, max_width);
    let boundary = truncation_boundary(&logical, max_width, budget_cut);
    let retained_end = boundary.unwrap_or(logical.len());
    let mut spans = normalized_spans(line, &logical[..retained_end]);

    if boundary.is_some() && max_width > 0 {
        push_styled_text(&mut spans, "…", style_at_offset(line, retained_end));
    }
    Line {
        style: line.style,
        alignment: line.alignment,
        spans,
    }
}

fn capped_str(input: &str) -> (&str, bool) {
    capped_str_to(input, CLIP_BYTE_INSPECTION_BUDGET)
}

fn capped_str_to(input: &str, byte_budget: usize) -> (&str, bool) {
    let mut end = input.len().min(byte_budget);
    while !input.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    (&input[..end], end < input.len())
}

fn bounded_grapheme_prefix(
    input: &str,
    byte_budget: usize,
    grapheme_budget: usize,
) -> (&str, bool) {
    if grapheme_budget == 0 {
        return ("", !input.is_empty());
    }

    let (capped, byte_cut) = capped_str_to(input, byte_budget);
    let mut grapheme_count = 0;
    let mut last_boundary = 0;
    let mut boundary_before_last = 0;
    for (start, grapheme) in capped.grapheme_indices(true) {
        grapheme_count += 1;
        boundary_before_last = last_boundary;
        last_boundary = start.saturating_add(grapheme.len());
        if grapheme_count >= grapheme_budget {
            if last_boundary < capped.len() {
                return (&capped[..last_boundary], true);
            }
            return if byte_cut {
                (&capped[..boundary_before_last], true)
            } else {
                (capped, false)
            };
        }
    }

    if byte_cut {
        (&capped[..boundary_before_last], true)
    } else {
        (capped, false)
    }
}

fn capped_line_text(line: &Line<'static>, max_width: usize) -> (String, bool) {
    let initial_capacity = max_width.saturating_mul(4).min(CLIP_BYTE_INSPECTION_BUDGET);
    let mut logical = String::with_capacity(initial_capacity);
    let mut budget_cut = false;

    for (index, span) in line.spans.iter().enumerate() {
        if index >= CLIP_SPAN_INSPECTION_BUDGET {
            budget_cut = true;
            break;
        }
        let content = span.content.as_ref();
        let remaining = CLIP_BYTE_INSPECTION_BUDGET.saturating_sub(logical.len());
        if content.len() <= remaining {
            logical.push_str(content);
            if logical.len() == CLIP_BYTE_INSPECTION_BUDGET && index + 1 < line.spans.len() {
                budget_cut = true;
                break;
            }
            continue;
        }

        let mut take = remaining;
        while !content.is_char_boundary(take) {
            take = take.saturating_sub(1);
        }
        logical.push_str(&content[..take]);
        budget_cut = true;
        break;
    }
    (logical, budget_cut)
}

fn normalized_spans(line: &Line<'static>, logical: &str) -> Vec<Span<'static>> {
    let mut normalized = Vec::with_capacity(
        line.spans
            .len()
            .min(CLIP_SPAN_INSPECTION_BUDGET)
            .min(CLIP_GRAPHEME_INSPECTION_BUDGET),
    );
    let mut span_index = 0;
    let mut span_start: usize = 0;
    let mut fallback = line.style;

    for (grapheme_start, grapheme) in logical
        .grapheme_indices(true)
        .take(CLIP_GRAPHEME_INSPECTION_BUDGET)
    {
        let style = loop {
            let Some(span) = line
                .spans
                .get(span_index)
                .filter(|_| span_index < CLIP_SPAN_INSPECTION_BUDGET)
            else {
                break fallback;
            };
            let span_end = span_start.saturating_add(span.content.len());
            if grapheme_start < span_end {
                break span.style;
            }
            fallback = span.style;
            span_start = span_end;
            span_index = span_index.saturating_add(1);
        };
        push_styled_text(&mut normalized, grapheme, style);
    }
    normalized
}

fn push_styled_text(spans: &mut Vec<Span<'static>>, content: &str, style: Style) {
    if let Some(previous) = spans.last_mut().filter(|span| span.style == style) {
        previous.content.to_mut().push_str(content);
    } else {
        spans.push(Span::styled(content.to_owned(), style));
    }
}

fn truncation_boundary(input: &str, max_width: usize, budget_cut: bool) -> Option<usize> {
    if input.is_empty() {
        return budget_cut.then_some(0);
    }
    if max_width == 0 {
        return Some(0);
    }

    let content_width = max_width.saturating_sub(usize::from("…".cell_width()));
    let mut width: usize = 0;
    let mut grapheme_count = 0;
    let mut ellipsis_boundary = 0;
    let mut ellipsis_boundary_before_last = 0;
    for (start, grapheme) in input.grapheme_indices(true) {
        grapheme_count += 1;
        let end = start.saturating_add(grapheme.len());
        let grapheme_width = if grapheme.contains(char::is_control) {
            0
        } else {
            usize::from(grapheme.cell_width())
        };
        let next_width = width.saturating_add(grapheme_width);
        if next_width > max_width {
            return Some(ellipsis_boundary);
        }
        ellipsis_boundary_before_last = ellipsis_boundary;
        width = next_width;
        if width <= content_width {
            ellipsis_boundary = end;
        }
        if grapheme_count >= CLIP_GRAPHEME_INSPECTION_BUDGET && end < input.len() {
            return Some(ellipsis_boundary);
        }
    }
    if budget_cut {
        return Some(ellipsis_boundary_before_last);
    }
    None
}

fn style_at_offset(line: &Line<'static>, target: usize) -> Style {
    let mut offset: usize = 0;
    let mut fallback = line.style;
    for span in line.spans.iter().take(CLIP_SPAN_INSPECTION_BUDGET) {
        let end = offset.saturating_add(span.content.len());
        if target < end {
            return span.style;
        }
        fallback = span.style;
        offset = end;
    }
    fallback
}

#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "layout rendering keeps frame state and matching hit geometry at one boundary"
)]
fn render_wide(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &AppState,
    theme: &Theme,
    model: &EffectiveRenderModel<'_>,
    enhancements: RenderEnhancements<'_>,
    viewports: &mut ViewportMemory,
    mut interactions: Option<&mut InteractionMap>,
) {
    let RenderEnhancements {
        artwork,
        spectrum,
        visualizer_max_fps,
    } = enhancements;
    let player_height =
        5 + if spectrum.is_some() { 3 } else { 0 } + if has_timed_lyrics(state) { 3 } else { 0 };
    let rows = split(
        area,
        Direction::Vertical,
        [Constraint::Min(0), Constraint::Length(player_height)],
    );
    if let Some(artwork) = artwork {
        let columns = split(
            rows[0],
            Direction::Horizontal,
            [
                Constraint::Length(22),
                Constraint::Min(24),
                Constraint::Length(PRODUCTION_ARTWORK_SIZE.width.saturating_add(2)),
                Constraint::Length(34),
            ],
        );
        render_navigation(
            frame,
            columns[0],
            theme,
            model,
            false,
            state,
            interactions.as_deref_mut(),
        );
        render_content(
            frame,
            columns[1],
            state,
            theme,
            model,
            visualizer_max_fps,
            viewports,
            interactions.as_deref_mut(),
        );
        render_artwork_panel(frame, columns[2], theme, artwork, PRODUCTION_ARTWORK_SIZE);
        render_queue(
            frame,
            columns[3],
            state.queue(),
            theme,
            model,
            viewports,
            interactions.as_deref_mut(),
        );
    } else {
        let columns = split(
            rows[0],
            Direction::Horizontal,
            [
                Constraint::Length(22),
                Constraint::Min(24),
                Constraint::Length(34),
            ],
        );
        render_navigation(
            frame,
            columns[0],
            theme,
            model,
            false,
            state,
            interactions.as_deref_mut(),
        );
        render_content(
            frame,
            columns[1],
            state,
            theme,
            model,
            visualizer_max_fps,
            viewports,
            interactions.as_deref_mut(),
        );
        render_queue(
            frame,
            columns[2],
            state.queue(),
            theme,
            model,
            viewports,
            interactions.as_deref_mut(),
        );
    }
    render_player(
        frame,
        rows[1],
        state,
        theme,
        model,
        false,
        spectrum,
        viewports,
        interactions,
    );
}

fn render_artwork_panel(
    frame: &mut Frame<'_>,
    area: Rect,
    theme: &Theme,
    artwork: &ArtworkPresentation,
    maximum_size: CellSize,
) {
    frame.render_widget(panel_block("Artwork", false, theme), area);
    let inner = Rect::new(
        area.x.saturating_add(1),
        area.y.saturating_add(1),
        area.width.saturating_sub(2).min(maximum_size.width),
        area.height.saturating_sub(2).min(maximum_size.height),
    );
    if maximum_size == COMPACT_ARTWORK_SIZE {
        render_fitted_artwork(frame, inner, artwork, theme.capability());
    } else {
        render_artwork(frame, inner, artwork, theme.capability());
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "layout rendering keeps frame state and matching hit geometry at one boundary"
)]
fn render_compact(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &AppState,
    theme: &Theme,
    model: &EffectiveRenderModel<'_>,
    enhancements: RenderEnhancements<'_>,
    viewports: &mut ViewportMemory,
    mut interactions: Option<&mut InteractionMap>,
) {
    let RenderEnhancements {
        artwork,
        spectrum,
        visualizer_max_fps,
    } = enhancements;
    let player_height = 5 + u16::from(spectrum.is_some()) + u16::from(has_timed_lyrics(state));
    let rows = split(
        area,
        Direction::Vertical,
        [
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(player_height),
        ],
    );
    render_navigation(
        frame,
        rows[0],
        theme,
        model,
        true,
        state,
        interactions.as_deref_mut(),
    );
    let can_show_artwork = artwork.is_some()
        && rows[1].width >= COMPACT_MIN_MAIN_WIDTH.saturating_add(COMPACT_ARTWORK_PANEL_WIDTH)
        && rows[1].height >= COMPACT_ARTWORK_PANEL_HEIGHT;
    let (main, artwork_area) = if can_show_artwork {
        let columns = split(
            rows[1],
            Direction::Horizontal,
            [
                Constraint::Min(COMPACT_MIN_MAIN_WIDTH),
                Constraint::Length(COMPACT_ARTWORK_PANEL_WIDTH),
            ],
        );
        (columns[0], Some(columns[1]))
    } else {
        (rows[1], None)
    };
    match model.compact_panel {
        CompactPanel::Content => render_content(
            frame,
            main,
            state,
            theme,
            model,
            visualizer_max_fps,
            viewports,
            interactions.as_deref_mut(),
        ),
        CompactPanel::Queue => render_queue(
            frame,
            main,
            state.queue(),
            theme,
            model,
            viewports,
            interactions.as_deref_mut(),
        ),
    }
    if let (Some(artwork), Some(artwork_area)) = (artwork, artwork_area) {
        render_artwork_panel(frame, artwork_area, theme, artwork, COMPACT_ARTWORK_SIZE);
    }
    render_player(
        frame,
        rows[2],
        state,
        theme,
        model,
        true,
        spectrum,
        viewports,
        interactions,
    );
}

#[allow(
    clippy::too_many_arguments,
    reason = "layout rendering keeps frame state and matching hit geometry at one boundary"
)]
fn render_tiny(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &AppState,
    theme: &Theme,
    model: &EffectiveRenderModel<'_>,
    enhancements: RenderEnhancements<'_>,
    viewports: &mut ViewportMemory,
    mut interactions: Option<&mut InteractionMap>,
) {
    let RenderEnhancements {
        visualizer_max_fps, ..
    } = enhancements;
    let player_height = u16::from(area.height > 0);
    let content_height = area.height.saturating_sub(player_height);
    let content_row = Rect::new(area.x, area.y, area.width, content_height);
    let player = Rect::new(
        area.x,
        area.y.saturating_add(content_height),
        area.width,
        player_height,
    );

    if content_row.height > 0 {
        let title = format!("▶ {}", model.view.label());
        let available = usize::from(content_row.width.saturating_sub(2));
        let mut targets =
            Vec::with_capacity(usize::from(content_row.height).min(MAX_RENDERED_ROWS));
        let mut lines = content_lines_with_viewports_and_targets_with_spinner(
            state,
            model.view,
            usize::from(content_row.height.saturating_sub(2)),
            available,
            model.search_draft,
            visualizer_max_fps,
            viewports,
            Some(&mut targets),
            model.motion_frame.spinner_index,
        );
        viewports.observe_spinner_lines(&lines, panel_inner(content_row));
        apply_selection_motion(
            &mut lines,
            &targets,
            viewports,
            theme,
            model.motion_frame.elapsed_ms,
            panel_inner(content_row),
        );
        register_row_targets(
            interactions.as_deref_mut(),
            panel_inner(content_row),
            &targets,
        );
        clip_lines(&mut lines, available);
        let block = panel_block(&title, true, theme);
        frame.render_widget(Paragraph::new(lines).block(block), content_row);
    }

    render_player_line(frame, player, state, theme, interactions);
}

#[allow(
    clippy::too_many_lines,
    reason = "navigation text and its exact hit geometry are constructed together"
)]
fn render_navigation(
    frame: &mut Frame<'_>,
    area: Rect,
    theme: &Theme,
    model: &EffectiveRenderModel<'_>,
    compact: bool,
    state: &AppState,
    mut interactions: Option<&mut InteractionMap>,
) {
    let focused = model.focus == FocusRegion::Navigation;
    if compact {
        let queue_count = state.queue().items().len();
        let tabs = match model.compact_panel {
            CompactPanel::Content => format!("[* Content] [Q Queue · {queue_count}]"),
            CompactPanel::Queue => format!("[Content] [* Q Queue · {queue_count}]"),
        };
        let title = format!("Navigation · {tabs}");
        let available_width = usize::from(area.width.saturating_sub(2));
        let roomy_width = NavigationItem::ALL
            .iter()
            .map(|item| 2 + usize::from(item.compact_label().cell_width()))
            .sum::<usize>()
            + 3 * NavigationItem::ALL.len().saturating_sub(1);
        // At the compact minimum every full label fits once decorative
        // alignment padding and rule glyphs are removed.
        let condensed = roomy_width > available_width;
        if let Some(interactions) = interactions.as_deref_mut() {
            let visible = panel_inner(area);
            let mut column = visible.x;
            for (index, item) in NavigationItem::ALL.iter().enumerate() {
                let marker = if *item == model.view {
                    "▶ "
                } else if condensed {
                    ""
                } else {
                    "  "
                };
                let label_width = item.compact_label().cell_width();
                let label_area = Rect::new(
                    column.saturating_add(marker.cell_width()),
                    visible.y,
                    label_width,
                    1,
                );
                interactions.push_clipped(label_area, visible, HitTarget::Navigation(*item));
                let separator_width = if index + 1 == NavigationItem::ALL.len() {
                    0
                } else if condensed {
                    1
                } else {
                    3
                };
                column = column
                    .saturating_add(marker.cell_width())
                    .saturating_add(label_width)
                    .saturating_add(separator_width);
            }
        }
        let spans = NavigationItem::ALL
            .iter()
            .enumerate()
            .flat_map(|(index, item)| {
                let marker = if *item == model.view {
                    "▶ "
                } else if condensed {
                    ""
                } else {
                    "  "
                };
                let separator = if index + 1 == NavigationItem::ALL.len() {
                    ""
                } else if condensed {
                    " "
                } else {
                    " │ "
                };
                [
                    Span::styled(
                        format!("{marker}{}", item.compact_label()),
                        navigation_style(*item == model.view, theme),
                    ),
                    Span::raw(separator),
                ]
            })
            .collect::<Vec<_>>();
        frame.render_widget(
            Paragraph::new(Line::from(spans)).block(panel_block(&title, focused, theme)),
            area,
        );
        return;
    }

    let lines = NavigationItem::ALL
        .iter()
        .map(|item| {
            let selected = *item == model.view;
            let marker = if selected { "▶" } else { " " };
            Line::styled(
                format!("{marker} {}", item.label()),
                navigation_style(selected, theme),
            )
        })
        .collect::<Vec<_>>();
    if let Some(interactions) = interactions {
        let visible = panel_inner(area);
        for (index, item) in NavigationItem::ALL.iter().enumerate() {
            let Some(row_offset) = u16::try_from(index).ok() else {
                break;
            };
            interactions.push_clipped(
                Rect::new(
                    visible.x.saturating_add(2),
                    visible.y.saturating_add(row_offset),
                    item.label().cell_width(),
                    1,
                ),
                visible,
                HitTarget::Navigation(*item),
            );
        }
    }
    frame.render_widget(
        Paragraph::new(lines).block(panel_block("Navigation", focused, theme)),
        area,
    );
}

fn panel_inner(area: Rect) -> Rect {
    Rect::new(
        area.x.saturating_add(1),
        area.y.saturating_add(1),
        area.width.saturating_sub(2),
        area.height.saturating_sub(2),
    )
}

pub(crate) fn loading_label(
    spinner_index: usize,
    text: &str,
    available_width: usize,
) -> Line<'static> {
    let frame = SPINNER_FRAMES[spinner_index % SPINNER_FRAMES.len()];
    Line::from(bounded_format_cells(
        available_width,
        format_args!("{frame} {text}"),
    ))
}

fn register_row_targets(
    interactions: Option<&mut InteractionMap>,
    visible: Rect,
    targets: &[RenderedRowTarget],
) {
    let Some(interactions) = interactions else {
        return;
    };
    for target in targets {
        let Some(offset) = u16::try_from(target.line_index).ok() else {
            continue;
        };
        interactions.push_clipped(
            Rect::new(
                visible.x,
                visible.y.saturating_add(offset),
                visible.width,
                1,
            ),
            visible,
            HitTarget::ListRow {
                surface: target.surface,
                stable_index: target.stable_index,
            },
        );
    }
}

fn apply_selection_motion(
    lines: &mut [Line<'static>],
    targets: &[RenderedRowTarget],
    viewports: &mut ViewportMemory,
    theme: &Theme,
    now_ms: u64,
    visible: Rect,
) {
    for surface_index in 0..LIST_SURFACE_COUNT {
        let Some(surface) = targets
            .iter()
            .find(|target| list_surface_index(target.surface) == surface_index)
            .map(|target| target.surface)
        else {
            continue;
        };
        let surface_targets = targets
            .iter()
            .filter(|target| target.surface == surface)
            .collect::<Vec<_>>();
        let Some(first) = surface_targets.first() else {
            continue;
        };
        let Some(last) = surface_targets.last() else {
            continue;
        };
        let visible_range = first.stable_index..last.stable_index.saturating_add(1);
        let total = visible_range.end;
        let logical_index = surface_targets.iter().find_map(|target| {
            line_text(lines.get(target.line_index)?)
                .starts_with('▶')
                .then_some(target.stable_index)
        });
        let presentation = viewports.present_selection(
            surface,
            total,
            logical_index,
            visible_range,
            first.dataset_key.clone(),
            now_ms,
        );
        let motion_region = presentation.transitioning.then(|| {
            let line_index = surface_targets
                .iter()
                .find(|target| Some(target.stable_index) == presentation.cursor_index)
                .map_or(0, |target| target.line_index);
            Rect::new(
                visible.x,
                visible
                    .y
                    .saturating_add(u16::try_from(line_index).unwrap_or(u16::MAX)),
                1,
                1,
            )
        });
        viewports.set_selection_motion_region(surface, motion_region);
        for target in surface_targets {
            if let Some(line) = lines.get_mut(target.line_index) {
                style_selection_row(
                    line,
                    target.stable_index == presentation.cursor_index.unwrap_or(usize::MAX),
                    target.stable_index == presentation.logical_index.unwrap_or(usize::MAX),
                    theme,
                );
            }
        }
    }
}

fn line_text(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

fn selection_row_content(line: &Line<'_>) -> String {
    let text = line_text(line);
    text.chars().skip(1).collect()
}

fn style_selection_row(line: &mut Line<'static>, cursor: bool, logical: bool, theme: &Theme) {
    let content = selection_row_content(line);
    let marker = if cursor {
        "▶"
    } else if logical {
        "●"
    } else {
        " "
    };
    let marker_style = if cursor || logical {
        Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let content_style = if logical {
        Style::default()
            .fg(theme.foreground)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.foreground)
    };
    *line = Line::from(vec![
        Span::styled(marker, marker_style),
        Span::styled(content, content_style),
    ]);
}

#[allow(
    clippy::too_many_arguments,
    reason = "content rendering keeps viewport and matching hit geometry at one boundary"
)]
fn render_content(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &AppState,
    theme: &Theme,
    model: &EffectiveRenderModel<'_>,
    visualizer_max_fps: u8,
    viewports: &mut ViewportMemory,
    interactions: Option<&mut InteractionMap>,
) {
    let focused = model.focus == FocusRegion::Content;
    let available_rows = usize::from(area.height.saturating_sub(2));
    let available_width = usize::from(area.width.saturating_sub(2));
    let mut targets = Vec::with_capacity(available_rows.min(MAX_RENDERED_ROWS));
    let mut lines = content_lines_with_viewports_and_targets_with_spinner(
        state,
        model.view,
        available_rows,
        available_width,
        model.search_draft,
        visualizer_max_fps,
        viewports,
        Some(&mut targets),
        model.motion_frame.spinner_index,
    );
    viewports.observe_spinner_lines(&lines, panel_inner(area));
    apply_selection_motion(
        &mut lines,
        &targets,
        viewports,
        theme,
        model.motion_frame.elapsed_ms,
        panel_inner(area),
    );
    register_row_targets(interactions, panel_inner(area), &targets);
    clip_lines(&mut lines, available_width);
    let title = if focused {
        format!("{} · focused content", model.view.label())
    } else {
        format!("{} · content", model.view.label())
    };
    frame.render_widget(
        Paragraph::new(lines).block(panel_block(&title, focused, theme)),
        area,
    );
}

#[cfg(test)]
fn content_lines(
    state: &AppState,
    view: NavigationItem,
    available_rows: usize,
    available_width: usize,
    search_draft: Option<&str>,
) -> Vec<Line<'static>> {
    content_lines_with_viewports(
        state,
        view,
        available_rows,
        available_width,
        search_draft,
        15,
        &mut ViewportMemory::default(),
    )
}

#[cfg(test)]
fn content_lines_with_viewports(
    state: &AppState,
    view: NavigationItem,
    available_rows: usize,
    available_width: usize,
    search_draft: Option<&str>,
    visualizer_max_fps: u8,
    viewports: &mut ViewportMemory,
) -> Vec<Line<'static>> {
    content_lines_with_viewports_and_targets(
        state,
        view,
        available_rows,
        available_width,
        search_draft,
        visualizer_max_fps,
        viewports,
        None,
    )
}

#[allow(
    clippy::too_many_arguments,
    reason = "view dispatch passes the shared viewport and row-target accumulators explicitly"
)]
#[cfg(test)]
fn content_lines_with_viewports_and_targets(
    state: &AppState,
    view: NavigationItem,
    available_rows: usize,
    available_width: usize,
    search_draft: Option<&str>,
    visualizer_max_fps: u8,
    viewports: &mut ViewportMemory,
    targets: Option<&mut Vec<RenderedRowTarget>>,
) -> Vec<Line<'static>> {
    content_lines_with_viewports_and_targets_with_spinner(
        state,
        view,
        available_rows,
        available_width,
        search_draft,
        visualizer_max_fps,
        viewports,
        targets,
        0,
    )
}

#[allow(
    clippy::too_many_arguments,
    reason = "view dispatch passes bounded viewport, target, and immutable motion state explicitly"
)]
fn content_lines_with_viewports_and_targets_with_spinner(
    state: &AppState,
    view: NavigationItem,
    available_rows: usize,
    available_width: usize,
    search_draft: Option<&str>,
    visualizer_max_fps: u8,
    viewports: &mut ViewportMemory,
    targets: Option<&mut Vec<RenderedRowTarget>>,
    spinner_index: usize,
) -> Vec<Line<'static>> {
    let row_limit = available_rows.min(MAX_RENDERED_ROWS);
    let mut lines = Vec::with_capacity(row_limit.min(16));
    if row_limit == 0 {
        return lines;
    }
    match view {
        NavigationItem::Home => {
            lines = super::views::home::lines(state, row_limit, available_width);
        }
        NavigationItem::Search => {
            lines = super::views::search::lines_with_viewport_and_targets_with_spinner(
                state,
                row_limit,
                available_width,
                search_draft,
                &mut viewports.search,
                targets,
                spinner_index,
            );
        }
        NavigationItem::Charts => {
            lines = super::views::charts::lines_with_viewport_and_targets_with_spinner(
                state,
                row_limit,
                available_width,
                &mut viewports.charts,
                targets,
                spinner_index,
            );
        }
        NavigationItem::Podcasts => {
            lines = super::views::podcasts::lines_with_viewports_and_targets_with_spinner(
                state,
                row_limit,
                available_width,
                &mut viewports.podcast_recommendations,
                &mut viewports.podcast_episodes,
                targets,
                spinner_index,
            );
        }
        NavigationItem::Library => {
            lines = super::views::library::lines_with_viewport_and_targets_with_spinner(
                state,
                row_limit,
                available_width,
                &mut viewports.library,
                targets,
                spinner_index,
            );
        }
        NavigationItem::Favorites => {
            lines = super::views::favorites::lines_with_viewport_and_targets_with_spinner(
                state,
                row_limit,
                available_width,
                &mut viewports.favorites,
                targets,
                spinner_index,
            );
        }
        NavigationItem::History => {
            lines = super::views::history::lines_with_viewport_and_targets_with_spinner(
                state,
                row_limit,
                available_width,
                &mut viewports.history,
                targets,
                spinner_index,
            );
        }
        NavigationItem::Settings => {
            lines = super::views::settings::lines(
                state,
                row_limit,
                available_width,
                visualizer_max_fps,
            );
        }
    }
    lines.truncate(row_limit);
    lines
}

fn render_queue(
    frame: &mut Frame<'_>,
    area: Rect,
    queue: &Queue,
    theme: &Theme,
    model: &EffectiveRenderModel<'_>,
    viewports: &mut ViewportMemory,
    interactions: Option<&mut InteractionMap>,
) {
    let available_rows = usize::from(area.height.saturating_sub(2)).min(MAX_RENDERED_ROWS);
    let available_width = usize::from(area.width.saturating_sub(2));
    let selected = (model.focus == FocusRegion::Queue)
        .then_some(model.queue_selected_id)
        .flatten();
    let mut targets = Vec::with_capacity(available_rows.min(MAX_RENDERED_ROWS));
    let mut lines = super::views::queue::lines_with_viewport_and_targets(
        queue,
        selected,
        available_rows,
        available_width,
        &mut viewports.queue,
        Some(&mut targets),
    );
    apply_selection_motion(
        &mut lines,
        &targets,
        viewports,
        theme,
        model.motion_frame.elapsed_ms,
        panel_inner(area),
    );
    register_row_targets(interactions, panel_inner(area), &targets);
    clip_lines(&mut lines, available_width);
    let title = format!("Queue · {}", queue.items().len());
    frame.render_widget(
        Paragraph::new(lines).block(panel_block(
            &title,
            model.focus == FocusRegion::Queue,
            theme,
        )),
        area,
    );
}

#[allow(
    clippy::too_many_arguments,
    reason = "player text and its exact hit geometry are rendered at one boundary"
)]
fn render_player(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &AppState,
    theme: &Theme,
    model: &EffectiveRenderModel<'_>,
    compact: bool,
    spectrum: Option<&SpectrumPresentation>,
    viewports: &mut ViewportMemory,
    interactions: Option<&mut InteractionMap>,
) {
    let available_width = usize::from(area.width.saturating_sub(2));
    let progress = progress_presentation(state, model);
    let controls =
        player_controls_layout_for_mode(state, compact, available_width, theme, progress);
    if state.playback().status == PlaybackStatus::Playing
        && state
            .playback()
            .duration_ms
            .is_some_and(|duration| state.playback().position_ms < duration)
        && let Some((offset, width)) = controls.progress
    {
        let inner = panel_inner(area);
        viewports.progress_motion_regions.push(Rect::new(
            inner.x.saturating_add(offset),
            inner.y.saturating_add(2),
            width,
            1,
        ));
    }
    if let Some(interactions) = interactions {
        register_player_interactions(interactions, area, &controls);
    }
    let mut lines = player_lines(
        state,
        compact,
        available_width,
        theme,
        spectrum,
        Some(controls.line),
    );
    clip_lines(&mut lines, available_width);
    frame.render_widget(
        Paragraph::new(lines).block(panel_block(
            "Player · persistent",
            model.focus == FocusRegion::Player,
            theme,
        )),
        area,
    );
}

fn playback_media(state: &AppState) -> Option<&crate::domain::MediaItem> {
    let media_id = state.playback().current.as_ref()?;
    state
        .queue()
        .items()
        .iter()
        .find(|item| &item.media().id == media_id)
        .map(crate::queue::QueueItem::media)
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PlayerControlLabel {
    action: Option<SemanticAction>,
    full: String,
    compact: String,
    tiny: &'static str,
}

fn player_control_labels(state: &AppState) -> [PlayerControlLabel; 5] {
    let playback = state.playback();
    let podcast =
        playback_media(state).is_some_and(|media| media.kind == MediaKind::PodcastEpisode);
    let backward_seconds = if podcast {
        state.podcast_skip_backward_seconds()
    } else {
        state.music_seek_seconds()
    };
    let forward_seconds = if podcast {
        state.podcast_skip_forward_seconds()
    } else {
        state.music_seek_seconds()
    };
    let (playback_action, playback_full, playback_compact, playback_tiny) = match playback.status {
        PlaybackStatus::Playing => (
            Some(SemanticAction::TogglePlayback),
            "[Space Pause]",
            "[Spc Pause]",
            "[Spc]",
        ),
        PlaybackStatus::Paused | PlaybackStatus::Stopped | PlaybackStatus::Failed
            if state.queue().current().is_some() =>
        {
            (
                Some(SemanticAction::TogglePlayback),
                "[Space Play]",
                "[Spc Play]",
                "[Spc]",
            )
        }
        PlaybackStatus::Resolving | PlaybackStatus::Buffering => {
            (None, "[- Loading…]", "[- Load]", "[-]")
        }
        PlaybackStatus::Stopped | PlaybackStatus::Failed | PlaybackStatus::Paused => {
            (None, "[- Play]", "[- Play]", "[-]")
        }
    };

    [
        PlayerControlLabel {
            action: Some(SemanticAction::PreviousTrack),
            full: "[p Previous]".to_owned(),
            compact: "[p]".to_owned(),
            tiny: "[p]",
        },
        PlayerControlLabel {
            action: Some(SemanticAction::SeekBackward),
            full: format!("[⇧← −{backward_seconds}s]"),
            compact: format!("[⇧← −{backward_seconds}s]"),
            tiny: "[←]",
        },
        PlayerControlLabel {
            action: playback_action,
            full: playback_full.to_owned(),
            compact: playback_compact.to_owned(),
            tiny: playback_tiny,
        },
        PlayerControlLabel {
            action: Some(SemanticAction::SeekForward),
            full: format!("[⇧→ +{forward_seconds}s]"),
            compact: format!("[⇧→ +{forward_seconds}s]"),
            tiny: "[→]",
        },
        PlayerControlLabel {
            action: Some(SemanticAction::NextTrack),
            full: "[n Next]".to_owned(),
            compact: "[n]".to_owned(),
            tiny: "[n]",
        },
    ]
}

struct PlayerControlsLayout {
    line: Line<'static>,
    controls: Vec<(u16, u16, SemanticAction)>,
    progress: Option<(u16, u16)>,
}

fn player_controls_layout_for_mode(
    state: &AppState,
    compact: bool,
    available_width: usize,
    theme: &Theme,
    progress: ProgressPresentation,
) -> PlayerControlsLayout {
    let (required, optional) = if compact {
        (String::new(), String::new())
    } else {
        (
            format!(
                "Shuffle {}   Repeat {:?}   Radio {}",
                on_off(state.queue().is_shuffled()),
                state.queue().repeat(),
                on_off(state.queue().radio_enabled())
            ),
            quality_text(state),
        )
    };
    player_controls_layout(
        state,
        available_width,
        &required,
        &optional,
        theme,
        progress,
    )
}

fn player_controls_layout(
    state: &AppState,
    available_width: usize,
    required_telemetry: &str,
    optional_telemetry: &str,
    theme: &Theme,
    progress_presentation: ProgressPresentation,
) -> PlayerControlsLayout {
    const MIN_PROGRESS_WIDTH: usize = 5;
    const MAX_PROGRESS_WIDTH: usize = 20;

    let labels = player_control_labels(state);
    let join = |tier: fn(&PlayerControlLabel) -> &str| {
        labels.iter().map(tier).collect::<Vec<_>>().join(" ")
    };
    let compact = join(|label| label.compact.as_str());
    let tiny = join(|label| label.tiny);
    let required_width = usize::from(required_telemetry.cell_width());
    let required_suffix_width = if required_telemetry.is_empty() {
        0
    } else {
        3usize.saturating_add(required_width)
    };
    let reserved_width = 1usize
        .saturating_add(MIN_PROGRESS_WIDTH)
        .saturating_add(required_suffix_width);
    let full = join(|label| label.full.as_str());
    let tier = if usize::from(full.as_str().cell_width()).saturating_add(reserved_width)
        <= available_width
    {
        0
    } else if usize::from(compact.as_str().cell_width()).saturating_add(reserved_width)
        <= available_width
    {
        1
    } else {
        2
    };
    let controls = match tier {
        0 => full.as_str(),
        1 => compact.as_str(),
        _ => tiny.as_str(),
    };
    let controls_width = usize::from(controls.cell_width());
    let progress_width = available_width
        .saturating_sub(controls_width)
        .saturating_sub(1)
        .saturating_sub(required_suffix_width)
        .min(MAX_PROGRESS_WIDTH);
    let progress = progress_bar_line(state, progress_width, progress_presentation, theme);
    let progress_text = line_text_content(&progress);
    let used_width = controls_width
        .saturating_add(1)
        .saturating_add(usize::from(progress_text.as_str().cell_width()))
        .saturating_add(required_suffix_width);
    let optional_width = available_width.saturating_sub(used_width);
    let optional = if optional_width >= 8 {
        truncate_cells(optional_telemetry, optional_width)
    } else {
        String::new()
    };
    let required = if required_telemetry.is_empty() {
        String::new()
    } else {
        format!("   {required_telemetry}")
    };
    let mut spans = Vec::with_capacity(progress.spans.len().saturating_add(3));
    spans.push(Span::raw(format!("{controls} ")));
    spans.extend(progress.spans);
    spans.push(Span::raw(required));
    spans.push(Span::raw(optional));
    let line = clip_line(&Line::from(spans), available_width);
    let mut control_regions = Vec::with_capacity(labels.len());
    let mut offset = 0_u16;
    for label in &labels {
        let rendered = match tier {
            0 => label.full.as_str(),
            1 => label.compact.as_str(),
            _ => label.tiny,
        };
        let width = rendered.cell_width();
        if let Some(action) = label.action {
            control_regions.push((offset, width, action));
        }
        offset = offset.saturating_add(width).saturating_add(1);
    }
    let progress_width = progress_text.as_str().cell_width();
    let progress_offset = controls.cell_width().saturating_add(1);
    PlayerControlsLayout {
        line,
        controls: control_regions,
        progress: (state
            .playback()
            .duration_ms
            .is_some_and(|duration| duration > 0)
            && progress_width >= 2)
            .then_some((progress_offset, progress_width)),
    }
}

fn register_player_interactions(
    interactions: &mut InteractionMap,
    area: Rect,
    layout: &PlayerControlsLayout,
) {
    let visible = panel_inner(area);
    let row = visible.y.saturating_add(2);
    for &(offset, width, action) in &layout.controls {
        interactions.push_clipped(
            Rect::new(visible.x.saturating_add(offset), row, width, 1),
            visible,
            HitTarget::Semantic(action),
        );
    }
    if let Some((offset, width)) = layout.progress {
        let denominator = width.saturating_sub(1);
        if denominator > 0 {
            for numerator in 0..width {
                interactions.push_clipped(
                    Rect::new(
                        visible.x.saturating_add(offset).saturating_add(numerator),
                        row,
                        1,
                        1,
                    ),
                    visible,
                    HitTarget::Progress {
                        numerator,
                        denominator,
                    },
                );
            }
        }
    }
}

fn progress_bar_line(
    state: &AppState,
    width: usize,
    presentation: ProgressPresentation,
    theme: &Theme,
) -> Line<'static> {
    if width < 2 {
        return Line::default();
    }
    let playback = state.playback();
    if playback.duration_ms.is_none_or(|duration| duration == 0) {
        return Line::from(Span::styled(
            "─".repeat(width),
            Style::default().fg(theme.muted),
        ));
    }
    let total_eighths = fraction_to_eighths(presentation.fraction, width);
    let shimmer_cell = (playback.status == PlaybackStatus::Playing)
        .then(|| shimmer_cell(presentation.shimmer_phase, total_eighths, width));
    let mut spans = Vec::with_capacity(width);
    for index in 0..width {
        let consumed = index.saturating_mul(8);
        let remaining = total_eighths.saturating_sub(consumed).min(8);
        let (symbol, filled) = match remaining {
            0 => ("░", false),
            1 => ("▏", true),
            2 => ("▎", true),
            3 => ("▍", true),
            4 => ("▌", true),
            5 => ("▋", true),
            6 => ("▊", true),
            7 => ("▉", true),
            _ => ("█", true),
        };
        let color = if filled {
            progress_color(theme, index, width, shimmer_cell == Some(index))
        } else {
            theme.muted
        };
        spans.push(Span::styled(symbol, Style::default().fg(color)));
    }
    Line::from(spans)
}

fn progress_presentation(
    state: &AppState,
    model: &EffectiveRenderModel<'_>,
) -> ProgressPresentation {
    if model.motion_frame_set {
        model.motion_frame.progress
    } else {
        authoritative_progress_presentation(state)
    }
}

#[allow(
    clippy::cast_precision_loss,
    reason = "playback milliseconds are normalized only at this transient presentation boundary"
)]
fn authoritative_progress_presentation(state: &AppState) -> ProgressPresentation {
    let playback = state.playback();
    let fraction = playback
        .duration_ms
        .filter(|duration| *duration > 0)
        .map_or(0.0, |duration| {
            playback.position_ms.min(duration) as f64 / duration as f64
        });
    ProgressPresentation::new(fraction, 0.0)
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    reason = "finite normalized motion is converted into a bounded number of eighth-cell glyphs"
)]
fn fraction_to_eighths(fraction: f64, width: usize) -> usize {
    let bounded = if fraction.is_finite() {
        fraction.clamp(0.0, 1.0)
    } else {
        0.0
    };
    (bounded * width.saturating_mul(8) as f64).round() as usize
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    reason = "finite normalized shimmer phase selects one cell in the already bounded filled span"
)]
fn shimmer_cell(phase: f64, total_eighths: usize, width: usize) -> usize {
    let filled_cells = total_eighths.div_ceil(8).min(width).max(1);
    let bounded = if phase.is_finite() {
        phase.clamp(0.0, 1.0)
    } else {
        0.0
    };
    (bounded * filled_cells.saturating_sub(1) as f64).round() as usize
}

fn progress_color(theme: &Theme, index: usize, width: usize, shimmer: bool) -> Color {
    let base = match (theme.accent, theme.selection) {
        (Color::Rgb(start_r, start_g, start_b), Color::Rgb(end_r, end_g, end_b)) => Color::Rgb(
            interpolate_channel(start_r, end_r, index, width),
            interpolate_channel(start_g, end_g, index, width),
            interpolate_channel(start_b, end_b, index, width),
        ),
        _ if index.saturating_mul(2) < width => theme.accent,
        _ => theme.selection,
    };
    if shimmer {
        match (base, theme.foreground) {
            (Color::Rgb(r, g, b), Color::Rgb(fr, fg, fb)) => Color::Rgb(
                midpoint_channel(r, fr),
                midpoint_channel(g, fg),
                midpoint_channel(b, fb),
            ),
            _ => theme.foreground,
        }
    } else {
        base
    }
}

fn interpolate_channel(start: u8, end: u8, index: usize, width: usize) -> u8 {
    let denominator = width.saturating_sub(1).max(1);
    let start = i64::from(start);
    let delta = i64::from(end).saturating_sub(start);
    let index = i64::try_from(index).unwrap_or(i64::MAX);
    let denominator = i64::try_from(denominator).unwrap_or(i64::MAX);
    let value = start.saturating_add(delta.saturating_mul(index) / denominator);
    u8::try_from(value.clamp(0, i64::from(u8::MAX))).unwrap_or(u8::MAX)
}

fn midpoint_channel(start: u8, end: u8) -> u8 {
    let midpoint = u16::from(start).saturating_add(u16::from(end)) / 2;
    u8::try_from(midpoint).unwrap_or(u8::MAX)
}

fn line_text_content(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

fn player_control_line(state: &AppState, compact: bool, available_width: usize) -> Line<'static> {
    let (required_telemetry, optional_telemetry) = if compact {
        (String::new(), String::new())
    } else {
        (
            format!(
                "Shuffle {}   Repeat {:?}   Radio {}",
                on_off(state.queue().is_shuffled()),
                state.queue().repeat(),
                on_off(state.queue().radio_enabled())
            ),
            quality_text(state),
        )
    };
    player_controls_layout(
        state,
        available_width,
        &required_telemetry,
        &optional_telemetry,
        &Theme::default(),
        authoritative_progress_presentation(state),
    )
    .line
}

fn player_lines(
    state: &AppState,
    compact: bool,
    available_width: usize,
    theme: &Theme,
    spectrum: Option<&SpectrumPresentation>,
    controls_text: Option<Line<'static>>,
) -> Vec<Line<'static>> {
    let playback = state.playback();
    let presentation = state.player_presentation();
    let current = playback_media(state);
    let current_title = current.map_or("Nothing playing", |media| media.title.as_str());
    let current_creator = current
        .and_then(|media| media.creators.first())
        .map(String::as_str);
    let podcast = current.is_some_and(|media| media.kind == MediaKind::PodcastEpisode);
    let position = format_duration(playback.position_ms);
    let duration = playback
        .duration_ms
        .map_or_else(|| "--:--".to_owned(), format_duration);
    let status = status_label(playback.status);
    let fade = presentation
        .fade()
        .map_or_else(String::new, |fade| format!("   Fade {}", fade.label()));
    let speed = if podcast {
        format!("   Speed {:.2}×", playback.playback_speed)
    } else {
        String::new()
    };
    let details = if compact {
        let compact_fade = presentation
            .fade()
            .map_or_else(String::new, |fade| format!(" F:{}", fade.label()));
        let compact_speed = if podcast {
            format!(" Sp:{:.2}×", playback.playback_speed)
        } else {
            String::new()
        };
        format!(
            "{position}/{duration} T{} E{:.0}{compact_fade}{compact_speed} S:{} R:{:?} Ra:{}{}",
            playback.target_volume,
            presentation.effective_volume(),
            on_off(state.queue().is_shuffled()),
            state.queue().repeat(),
            on_off(state.queue().radio_enabled()),
            compact_quality_text(state)
        )
    } else {
        format!(
            "{position} / {duration}   Target {}%   Effective {:.0}%{fade}{speed}",
            playback.target_volume,
            presentation.effective_volume()
        )
    };
    let mut lines = vec![
        Line::from(bounded_format_cells(
            available_width,
            format_args!(
                "{}  {}  ·  {status}",
                playback_icon(playback.status),
                current_title
            ),
        )),
        Line::from(bounded_format_cells(
            available_width,
            format_args!("{details}"),
        )),
    ];
    if let Some(creator) = current_creator {
        lines[0] = Line::from(bounded_format_cells(
            available_width,
            format_args!(
                "{}  {} — {}  ·  {status}",
                playback_icon(playback.status),
                current_title,
                creator
            ),
        ));
    }
    lines.push(
        controls_text.unwrap_or_else(|| player_control_line(state, compact, available_width)),
    );
    if let Some(spectrum) = spectrum {
        lines.extend(spectrum_lines(
            spectrum,
            if compact { 1 } else { 3 },
            available_width,
            theme,
        ));
    }
    lines.extend(automatic_lyrics_lines(
        state,
        if compact { 1 } else { 3 },
        available_width,
        theme,
    ));
    lines
}

fn spectrum_lines(
    presentation: &SpectrumPresentation,
    rows: usize,
    available_width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    if rows == 0 || available_width == 0 {
        return Vec::new();
    }
    let band_count = available_width.min(MAX_SPECTRUM_BANDS);
    let levels = presentation.frame().map_or_else(
        || vec![0; band_count],
        |frame| resample_spectrum(frame.levels(), band_count),
    );
    let total_steps = rows.saturating_mul(8);

    (0..rows)
        .map(|row| {
            let steps_below = (rows.saturating_sub(row + 1)).saturating_mul(8);
            let spans = levels
                .iter()
                .enumerate()
                .map(|(index, level)| {
                    let scaled = usize::from(*level)
                        .saturating_mul(total_steps)
                        .div_ceil(usize::from(MAX_SPECTRUM_LEVEL));
                    let eighths = scaled.saturating_sub(steps_below).min(8);
                    let eighths = if row + 1 == rows && eighths == 0 {
                        1
                    } else {
                        eighths
                    };
                    let style = spectrum_style(
                        theme,
                        theme.capability(),
                        index,
                        band_count,
                        *level,
                        presentation.paused() || presentation.failed(),
                    );
                    Span::styled(spectrum_glyph(eighths), style)
                })
                .collect::<Vec<_>>();
            Line::from(spans)
        })
        .collect()
}

fn lerp_channel(start: u8, end: u8, numerator: u16, denominator: u16) -> u8 {
    if denominator == 0 {
        return start;
    }
    let numerator = numerator.min(denominator);
    let start = u32::from(start);
    let end = u32::from(end);
    let denominator = u32::from(denominator);
    let numerator = u32::from(numerator);
    let value = if end >= start {
        start + (end - start) * numerator / denominator
    } else {
        start - (start - end) * numerator / denominator
    };
    u8::try_from(value).unwrap_or(u8::MAX)
}

fn spectrum_color(
    theme: &Theme,
    capability: ColorCapability,
    band: usize,
    bands: usize,
    level: u8,
) -> Color {
    match capability {
        ColorCapability::TrueColor => {
            let Color::Rgb(accent_red, accent_green, accent_blue) = theme.accent else {
                return theme.accent;
            };
            let Color::Rgb(foreground_red, foreground_green, foreground_blue) = theme.foreground
            else {
                return theme.foreground;
            };
            let last = bands.saturating_sub(1);
            let band = band.min(last);
            let middle = last.div_ceil(2);
            let bright = |channel| lerp_channel(channel, u8::MAX, 1, 3);
            let interpolate_band = |accent, foreground| {
                if band <= middle {
                    lerp_channel(
                        accent,
                        foreground,
                        u16::try_from(band).unwrap_or(u16::MAX),
                        u16::try_from(middle).unwrap_or(u16::MAX),
                    )
                } else {
                    lerp_channel(
                        foreground,
                        bright(foreground),
                        u16::try_from(band - middle).unwrap_or(u16::MAX),
                        u16::try_from(last - middle).unwrap_or(u16::MAX),
                    )
                }
            };
            let intensity = 60_u16.saturating_add(
                40_u16.saturating_mul(u16::from(level.min(MAX_SPECTRUM_LEVEL)))
                    / u16::from(MAX_SPECTRUM_LEVEL),
            );
            let brighten_level = |channel| lerp_channel(0, channel, intensity, 100);
            Color::Rgb(
                brighten_level(interpolate_band(accent_red, foreground_red)),
                brighten_level(interpolate_band(accent_green, foreground_green)),
                brighten_level(interpolate_band(accent_blue, foreground_blue)),
            )
        }
        ColorCapability::Ansi256 | ColorCapability::Basic => {
            if level == 0 {
                theme.muted
            } else {
                match band.saturating_mul(3) / bands.max(1) {
                    0 => theme.accent,
                    1 => theme.selection,
                    _ => theme.foreground,
                }
            }
        }
        ColorCapability::Monochrome => Color::Reset,
    }
}

fn spectrum_style(
    theme: &Theme,
    capability: ColorCapability,
    band: usize,
    bands: usize,
    level: u8,
    muted: bool,
) -> Style {
    if capability == ColorCapability::Monochrome {
        return if muted || level < MAX_SPECTRUM_LEVEL / 2 {
            Style::default().add_modifier(Modifier::DIM)
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        };
    }
    if muted {
        Style::default().fg(theme.muted)
    } else {
        Style::default().fg(spectrum_color(theme, capability, band, bands, level))
    }
}

fn resample_spectrum(levels: &[u8], band_count: usize) -> Vec<u8> {
    if levels.is_empty() || band_count == 0 {
        return Vec::new();
    }
    (0..band_count)
        .map(|index| {
            let start = index.saturating_mul(levels.len()) / band_count;
            let end = (index + 1)
                .saturating_mul(levels.len())
                .div_ceil(band_count)
                .min(levels.len());
            levels[start..end].iter().copied().max().unwrap_or(0)
        })
        .collect()
}

fn spectrum_glyph(eighths: usize) -> &'static str {
    const GLYPHS: [&str; 9] = [" ", "▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];
    GLYPHS[eighths.min(8)]
}

fn has_timed_lyrics(state: &AppState) -> bool {
    state
        .lyrics()
        .document()
        .is_some_and(|document| !document.timed().is_empty())
}

fn automatic_lyrics_lines(
    state: &AppState,
    max_rows: usize,
    available_width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    if max_rows == 0 || available_width == 0 {
        return Vec::new();
    }
    let Some(document) = state.lyrics().document() else {
        return Vec::new();
    };
    let timed = document.timed();
    let Some(active) = state
        .lyrics()
        .active_line_index()
        .filter(|index| *index < timed.len())
    else {
        return Vec::new();
    };
    let transition = document.transition_at(state.playback().position_ms);
    let desired = max_rows.min(3);
    let mut start = if desired >= 3 {
        active.saturating_sub(1)
    } else {
        active
    };
    start = start.min(timed.len().saturating_sub(desired));
    timed
        .iter()
        .enumerate()
        .skip(start)
        .take(desired)
        .map(|(index, lyric)| {
            let style = lyric_transition_style(theme, index, active, transition);
            Line::from(Span::styled(
                truncate_cells(lyric.text(), available_width),
                style,
            ))
        })
        .collect()
}

fn lyric_transition_style(
    theme: &Theme,
    line_index: usize,
    active_index: usize,
    transition: Option<crate::lyrics::LyricTransition>,
) -> Style {
    const MIDPOINT: u16 = 500;
    let progress = transition.map_or(1_000, crate::lyrics::LyricTransition::progress_millis);
    let incoming = transition.is_some_and(|value| value.incoming_index() == line_index)
        || (transition.is_none() && line_index == active_index);
    let outgoing = transition.is_some_and(|value| value.outgoing_index() == Some(line_index));
    let emphasized = (incoming && progress >= MIDPOINT) || (outgoing && progress < MIDPOINT);

    let mut style = match theme.capability() {
        ColorCapability::TrueColor => {
            let foreground = if incoming {
                interpolate_color(theme.muted, theme.accent, progress)
            } else if outgoing {
                interpolate_color(theme.accent, theme.muted, progress)
            } else {
                theme.muted
            };
            Style::default().fg(foreground)
        }
        ColorCapability::Ansi256 | ColorCapability::Basic => {
            // Limited-color terminals use three deterministic thirds. Bold
            // still changes only at the shared 500‰ emphasis midpoint.
            let intensity = if incoming {
                match progress {
                    0..333 => 0,
                    333..667 => 1,
                    _ => 2,
                }
            } else if outgoing {
                match progress {
                    0..333 => 2,
                    333..667 => 1,
                    _ => 0,
                }
            } else {
                0
            };
            let foreground = match intensity {
                0 => theme.muted,
                1 => theme.selection,
                _ => theme.accent,
            };
            let style = Style::default().fg(foreground);
            if intensity == 0 {
                style.add_modifier(Modifier::DIM)
            } else {
                style
            }
        }
        ColorCapability::Monochrome => {
            if emphasized {
                Style::default()
            } else {
                Style::default().add_modifier(Modifier::DIM)
            }
        }
    };
    if emphasized {
        style = style.add_modifier(Modifier::BOLD);
    }
    style
}

fn interpolate_color(start: Color, end: Color, progress_millis: u16) -> Color {
    match (start, end) {
        (
            Color::Rgb(start_red, start_green, start_blue),
            Color::Rgb(end_red, end_green, end_blue),
        ) => Color::Rgb(
            lyric_lerp_channel(start_red, end_red, progress_millis),
            lyric_lerp_channel(start_green, end_green, progress_millis),
            lyric_lerp_channel(start_blue, end_blue, progress_millis),
        ),
        (_, end) if progress_millis >= 500 => end,
        (start, _) => start,
    }
}

fn lyric_lerp_channel(start: u8, end: u8, progress_millis: u16) -> u8 {
    let progress = u32::from(progress_millis.min(1_000));
    let remaining = 1_000_u32.saturating_sub(progress);
    let value = u32::from(start)
        .saturating_mul(remaining)
        .saturating_add(u32::from(end).saturating_mul(progress))
        / 1_000;
    u8::try_from(value).unwrap_or(u8::MAX)
}

fn render_player_line(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &AppState,
    theme: &Theme,
    interactions: Option<&mut InteractionMap>,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let layout = tiny_player_layout(state, usize::from(area.width));
    let TinyPlayerLayout {
        text,
        controls,
        identity,
    } = layout;
    let base_style = Style::default()
        .fg(theme.foreground)
        .bg(theme.background)
        .add_modifier(Modifier::BOLD);
    frame.render_widget(Paragraph::new(text).style(base_style), area);
    if let Some((offset, width)) =
        identity.filter(|(_, width)| *width >= TINY_LYRICS_MIN_IDENTITY_WIDTH)
        && let Some(lyric) = automatic_lyrics_lines(state, 1, usize::from(width), theme).pop()
    {
        let lyric_area = Rect::new(area.x.saturating_add(offset), area.y, width, 1);
        let neutral_style = Style::reset().fg(theme.foreground).bg(theme.background);
        frame.render_widget(
            Paragraph::new(" ".repeat(usize::from(width))).style(neutral_style),
            lyric_area,
        );
        frame.render_widget(
            Paragraph::new(lyric).style(Style::default().bg(theme.background)),
            lyric_area,
        );
    }
    if let Some(interactions) = interactions {
        for (offset, width, action) in controls {
            interactions.push_clipped(
                Rect::new(area.x.saturating_add(offset), area.y, width, 1),
                area,
                HitTarget::Semantic(action),
            );
        }
    }
}

#[cfg(test)]
fn tiny_player_text(state: &AppState, available_width: usize) -> String {
    tiny_player_layout(state, available_width).text
}

struct TinyPlayerLayout {
    text: String,
    controls: Vec<(u16, u16, SemanticAction)>,
    identity: Option<(u16, u16)>,
}

#[allow(
    clippy::too_many_lines,
    reason = "tiny telemetry and control geometry share one width-budget calculation"
)]
fn tiny_player_layout(state: &AppState, available_width: usize) -> TinyPlayerLayout {
    let playback = state.playback();
    let presentation = state.player_presentation();
    let current = playback_media(state);
    let current_title = current.map_or("Nothing playing", |media| media.title.as_str());
    let creator = current
        .and_then(|media| media.creators.first())
        .map_or("", String::as_str);
    let status = playback_icon(playback.status).to_owned();
    let progress = format!(
        "{}/{}",
        compact_duration(playback.position_ms),
        playback
            .duration_ms
            .map_or_else(|| "--".to_owned(), compact_duration)
    );
    let effective = presentation.effective_volume().round().clamp(0.0, 100.0);
    let fade = match presentation.fade() {
        Some(crate::app::FadeActivity::In) => '↑',
        Some(crate::app::FadeActivity::Out) => '↓',
        None => '·',
    };
    let volume = format!("v{}/{effective:.0}{fade}", playback.target_volume.min(100));
    let modes = format!(
        "{}{}{}",
        if state.queue().is_shuffled() {
            'S'
        } else {
            's'
        },
        match state.queue().repeat() {
            crate::domain::RepeatMode::Off => '-',
            crate::domain::RepeatMode::One => '1',
            crate::domain::RepeatMode::All => 'A',
        },
        if state.queue().radio_enabled() {
            'E'
        } else {
            'e'
        }
    );
    let podcast = current.is_some_and(|media| media.kind == MediaKind::PodcastEpisode);
    let speed = podcast.then(|| format!("x{:.1}", playback.playback_speed));
    let quality = presentation.quality().known().then(|| {
        format!(
            "q{}/{}",
            one_cell_label(presentation.quality().format_id().unwrap_or("?")),
            one_cell_label(presentation.quality().codec().unwrap_or("?"))
        )
    });

    let mut fixed = vec![status, progress, volume, modes];
    if let Some(speed) = speed {
        fixed.push(speed);
    }
    if let Some(quality) = quality {
        fixed.push(quality);
    }
    let control_labels = player_control_labels(state);
    let tiny_controls = control_labels
        .iter()
        .map(|label| label.tiny)
        .collect::<Vec<_>>()
        .join(" ");
    let controls_width = usize::from(tiny_controls.as_str().cell_width());
    let fixed_without_controls_width = fixed
        .iter()
        .map(|field| usize::from(field.as_str().cell_width()))
        .sum::<usize>();
    let identity_minimum = identity_minimum_width(current_title, creator);
    let required_with_controls = fixed_without_controls_width
        .saturating_add(identity_minimum)
        .saturating_add(fixed.len())
        .saturating_add(1)
        .saturating_add(controls_width);
    let controls_included = required_with_controls <= available_width;
    if controls_included {
        fixed.push(tiny_controls);
    }
    let fixed_width = fixed
        .iter()
        .map(|field| usize::from(field.as_str().cell_width()))
        .sum::<usize>();
    let separator_slots = fixed.len();
    let separators = available_width
        .saturating_sub(fixed_width.saturating_add(identity_minimum))
        .min(separator_slots);
    let identity_budget = available_width.saturating_sub(fixed_width.saturating_add(separators));
    let identity = compact_identity(current_title, creator, identity_budget);

    let mut fields = Vec::with_capacity(fixed.len().saturating_add(1));
    fields.push(fixed.remove(0));
    fields.push(identity);
    fields.extend(fixed);
    let mut line = String::new();
    let final_index = fields.len().saturating_sub(1);
    let mut controls_start = None;
    let mut identity = None;
    for (index, field) in fields.into_iter().enumerate() {
        if index > 0 && index <= separators {
            line.push(' ');
        }
        if index == 1 {
            identity = Some((line.as_str().cell_width(), field.as_str().cell_width()));
        }
        if controls_included && index == final_index {
            controls_start = Some(line.as_str().cell_width());
        }
        line.push_str(&field);
    }
    let mut controls = Vec::with_capacity(control_labels.len());
    if let Some(mut offset) = controls_start {
        for label in control_labels {
            let width = label.tiny.cell_width();
            if let Some(action) = label.action {
                controls.push((offset, width, action));
            }
            offset = offset.saturating_add(width).saturating_add(1);
        }
    }
    TinyPlayerLayout {
        text: truncate_cells(&line, available_width),
        controls,
        identity,
    }
}

fn compact_duration(milliseconds: u64) -> String {
    let total_seconds = milliseconds / 1_000;
    if total_seconds < 600 {
        return format!("{}:{:02}", total_seconds / 60, total_seconds % 60);
    }
    let minutes = total_seconds / 60;
    if minutes < 1_000 {
        return format!("{}m", minutes.min(999));
    }
    let hours = total_seconds / 3_600;
    if hours < 1_000 {
        return format!("{}h", hours.min(999));
    }
    "999+".to_owned()
}

fn identity_minimum_width(title: &str, creator: &str) -> usize {
    let title = first_grapheme_width(title).max(1);
    if creator.is_empty() {
        return title;
    }
    title
        .saturating_add(1)
        .saturating_add(first_grapheme_width(creator).max(1))
}

fn compact_identity(title: &str, creator: &str, budget: usize) -> String {
    if creator.is_empty() {
        return cell_prefix(title, budget);
    }
    let title_minimum = first_grapheme_width(title).max(1);
    let creator_minimum = first_grapheme_width(creator).max(1);
    let content_budget = budget.saturating_sub(1);
    if content_budget < title_minimum.saturating_add(creator_minimum) {
        return cell_prefix(title, budget);
    }
    let extra = content_budget
        .saturating_sub(title_minimum)
        .saturating_sub(creator_minimum);
    let title_budget = title_minimum.saturating_add(extra.saturating_add(1) / 2);
    let creator_budget = creator_minimum.saturating_add(extra / 2);
    format!(
        "{}/{}",
        cell_prefix(title, title_budget),
        cell_prefix(creator, creator_budget)
    )
}

fn one_cell_label(label: &str) -> String {
    let label = cell_prefix(label, 1);
    if label.is_empty() {
        "?".to_owned()
    } else {
        label
    }
}

fn first_grapheme_width(value: &str) -> usize {
    value
        .graphemes(true)
        .take(CLIP_GRAPHEME_INSPECTION_BUDGET)
        .find(|grapheme| !grapheme.contains(char::is_control))
        .map_or(0, |grapheme| usize::from(grapheme.cell_width()))
}

fn cell_prefix(value: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    let mut prefix = String::new();
    let mut width = 0usize;
    for grapheme in value.graphemes(true).take(CLIP_GRAPHEME_INSPECTION_BUDGET) {
        if grapheme.contains(char::is_control) {
            continue;
        }
        let grapheme_width = usize::from(grapheme.cell_width());
        if width.saturating_add(grapheme_width) > max_width {
            break;
        }
        prefix.push_str(grapheme);
        width = width.saturating_add(grapheme_width);
    }
    prefix
}

fn quality_text(state: &AppState) -> String {
    let quality = state.player_presentation().quality();
    if !quality.known() {
        return String::new();
    }
    let format_id = quality.format_id().unwrap_or("?");
    let codec = quality.codec().unwrap_or("?");
    format!("   Quality {format_id}/{codec}")
}

fn compact_quality_text(state: &AppState) -> String {
    let quality = state.player_presentation().quality();
    if !quality.known() {
        return String::new();
    }
    let format_id = quality.format_id().unwrap_or("?");
    let codec = quality.codec().unwrap_or("?");
    format!(" Q:{format_id}/{codec}")
}

#[allow(
    clippy::too_many_arguments,
    reason = "overlay rendering keeps modal geometry and targets in one replacement pass"
)]
#[allow(
    clippy::too_many_lines,
    reason = "overlay layout keeps modal geometry, row targets, and shared motion presentation together"
)]
fn render_overlay(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &AppState,
    theme: &Theme,
    overlay: Overlay,
    model: &EffectiveRenderModel<'_>,
    viewports: &mut ViewportMemory,
    interactions: Option<&mut InteractionMap>,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    if area.width < 12 || area.height < 4 {
        viewports.occlude_motion(area);
        let label = match overlay {
            Overlay::Help => "Help · Esc",
            Overlay::CommandPalette => "Commands · Esc",
            Overlay::CountryPicker => "Country · Esc",
            Overlay::BrowserPicker => "Browser · Esc",
            Overlay::Lyrics => "Lyrics · Esc",
        };
        frame.render_widget(Clear, area);
        frame.render_widget(
            Paragraph::new(truncate_cells(label, usize::from(area.width))).style(
                Style::default()
                    .fg(theme.foreground)
                    .bg(theme.background)
                    .add_modifier(Modifier::BOLD),
            ),
            area,
        );
        return;
    }

    let popup = centered_rect(
        area,
        match overlay {
            Overlay::Help => 74,
            Overlay::CommandPalette => 68,
            Overlay::CountryPicker => 48,
            Overlay::BrowserPicker => 36,
            Overlay::Lyrics => 84,
        },
        match overlay {
            Overlay::Help => 15,
            Overlay::CommandPalette => 22,
            Overlay::CountryPicker => 20,
            Overlay::BrowserPicker => 12,
            Overlay::Lyrics => 28,
        },
    );
    viewports.occlude_motion(popup);
    frame.render_widget(Clear, popup);
    let available_rows = usize::from(popup.height.saturating_sub(2));
    let available_width = usize::from(popup.width.saturating_sub(2));
    let mut targets = overlay_target_buffer(overlay, true, available_rows);
    let (title, mut lines) = match overlay {
        Overlay::Help => (
            "Help · ? or Esc to close",
            help_lines(model.help_scroll, available_rows),
        ),
        Overlay::CommandPalette => (
            "Command palette · type to filter · Esc",
            palette_lines_with_targets(
                model.palette,
                available_rows,
                available_width,
                targets.as_mut(),
            ),
        ),
        Overlay::CountryPicker => (
            "Country picker · Enter select · Esc",
            country_picker_lines_with_viewport_and_targets(
                model.country_picker,
                available_rows,
                available_width,
                &mut viewports.country_picker,
                targets.as_mut(),
            ),
        ),
        Overlay::BrowserPicker => (
            "Browser picker · Enter import · Esc",
            browser_picker_lines_with_viewport_and_targets(
                *model.browser_picker,
                available_rows,
                available_width,
                &mut viewports.browser_picker,
                targets.as_mut(),
            ),
        ),
        Overlay::Lyrics => (
            if model.lyrics.follow_active() {
                "Lyrics · following · arrows/j/k scroll · Enter recenter · L/Esc"
            } else {
                "Lyrics · manual · Enter recenter · L/Esc"
            },
            lyrics_overlay_lines_with_motion(
                state,
                model.lyrics,
                available_rows,
                available_width,
                theme,
                model.motion_frame.spinner_index,
                Some(viewports),
                model.motion_frame.elapsed_ms,
            ),
        ),
    };
    lines.truncate(available_rows);
    viewports.observe_spinner_lines(&lines, panel_inner(popup));
    apply_selection_motion(
        &mut lines,
        targets.as_deref().unwrap_or_default(),
        viewports,
        theme,
        model.motion_frame.elapsed_ms,
        panel_inner(popup),
    );
    register_row_targets(
        interactions,
        panel_inner(popup),
        targets.as_deref().unwrap_or_default(),
    );
    clip_lines(&mut lines, available_width);
    frame.render_widget(
        Paragraph::new(lines).block(panel_block(title, true, theme)),
        popup,
    );
}

fn overlay_target_buffer(
    overlay: Overlay,
    interactions_enabled: bool,
    available_rows: usize,
) -> Option<Vec<RenderedRowTarget>> {
    (interactions_enabled
        && matches!(
            overlay,
            Overlay::CommandPalette | Overlay::CountryPicker | Overlay::BrowserPicker
        ))
    .then(|| Vec::with_capacity(available_rows.min(MAX_RENDERED_ROWS)))
}

#[cfg(test)]
fn lyrics_overlay_lines(
    state: &AppState,
    overlay: &LyricsOverlayState,
    available_rows: usize,
    available_width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    lyrics_overlay_lines_with_spinner(state, overlay, available_rows, available_width, theme, 0)
}

#[cfg(test)]
fn lyrics_overlay_lines_with_spinner(
    state: &AppState,
    overlay: &LyricsOverlayState,
    available_rows: usize,
    available_width: usize,
    theme: &Theme,
    spinner_index: usize,
) -> Vec<Line<'static>> {
    lyrics_overlay_lines_with_motion(
        state,
        overlay,
        available_rows,
        available_width,
        theme,
        spinner_index,
        None,
        0,
    )
}

#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "lyrics rendering consumes bounded overlay geometry and immutable motion phase"
)]
fn lyrics_overlay_lines_with_motion(
    state: &AppState,
    overlay: &LyricsOverlayState,
    available_rows: usize,
    available_width: usize,
    theme: &Theme,
    spinner_index: usize,
    motion_memory: Option<&mut ViewportMemory>,
    now_ms: u64,
) -> Vec<Line<'static>> {
    let row_limit = available_rows.min(MAX_RENDERED_ROWS);
    if row_limit == 0 || available_width == 0 {
        return Vec::new();
    }
    if state.lyrics().loading() {
        return vec![loading_label(
            spinner_index,
            "Loading lyrics…",
            available_width,
        )];
    }
    let Some(document) = state.lyrics().document() else {
        return vec![Line::from("Lyrics unavailable")];
    };
    let attribution = Line::from(Span::styled(
        match document.source() {
            crate::lyrics::LyricsSource::YouTubeMusic => "Source: YouTube Music",
            crate::lyrics::LyricsSource::Lrclib => "Source: LRCLIB",
        },
        Style::default().fg(theme.muted),
    ));
    let content_rows = row_limit.saturating_sub(1);
    if content_rows == 0 {
        return vec![attribution];
    }
    if document.instrumental() {
        return vec![attribution, Line::from("Instrumental")];
    }
    if !document.timed().is_empty() {
        let timed = document.timed();
        let focused = if overlay.follow_active {
            state.lyrics().active_line_index()
        } else {
            overlay.selected_line
        }
        .unwrap_or_default()
        .min(timed.len().saturating_sub(1));
        let anchor = if overlay.follow_active {
            focused
        } else {
            overlay.scroll.min(timed.len().saturating_sub(1))
        };
        let max_start = timed.len().saturating_sub(content_rows);
        let start = anchor.saturating_sub(content_rows / 2).min(max_start);
        let visible_range = start..start.saturating_add(content_rows).min(timed.len());
        let presentation = motion_memory.map_or(
            SelectionPresentation {
                logical_index: Some(focused),
                cursor_index: Some(focused),
                transitioning: false,
            },
            |memory| {
                memory.present_selection(
                    ListSurface::Lyrics,
                    timed.len(),
                    Some(focused),
                    visible_range.clone(),
                    DatasetKey::Scalar(dataset_key(&(
                        state.playback().current.as_ref(),
                        timed.len(),
                        timed.first().map(crate::lyrics::TimedLyricLine::text),
                        timed.last().map(crate::lyrics::TimedLyricLine::text),
                    ))),
                    now_ms,
                )
            },
        );
        let timed_lines = timed
            .iter()
            .enumerate()
            .skip(start)
            .take(content_rows)
            .map(|(index, lyric)| {
                let mut style = Style::default().fg(
                    if index == state.lyrics().active_line_index().unwrap_or(usize::MAX) {
                        theme.accent
                    } else {
                        theme.foreground
                    },
                );
                if index == state.lyrics().active_line_index().unwrap_or(usize::MAX)
                    || (!overlay.follow_active && index == focused)
                {
                    style = style.add_modifier(Modifier::BOLD);
                }
                let cursor = presentation.cursor_index == Some(index);
                let logical = presentation.logical_index == Some(index);
                let marker = if cursor {
                    "▶ "
                } else if logical {
                    "● "
                } else {
                    "  "
                };
                Line::from(vec![
                    Span::styled(
                        marker,
                        if cursor || logical {
                            Style::default()
                                .fg(theme.accent)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default()
                        },
                    ),
                    Span::styled(
                        truncate_cells(lyric.text(), available_width.saturating_sub(2)),
                        style,
                    ),
                ])
            })
            .collect::<Vec<_>>();
        return std::iter::once(attribution).chain(timed_lines).collect();
    }
    let plain_lines = document.plain().map_or_else(Vec::new, |plain| {
        wrap_lyrics_text(plain, overlay.scroll, content_rows, available_width)
    });
    std::iter::once(attribution).chain(plain_lines).collect()
}

fn wrap_lyrics_text(
    text: &str,
    requested_start: usize,
    row_limit: usize,
    available_width: usize,
) -> Vec<Line<'static>> {
    let text = normalize_lyrics_text(text);
    let total_rows = wrapped_lyrics_row_count_normalized(&text, available_width);
    let start = requested_start.min(total_rows.saturating_sub(row_limit));
    let mut lines = Vec::with_capacity(row_limit.min(16));
    let mut row_index = 0_usize;
    for logical in text.split('\n') {
        if lines.len() >= row_limit {
            break;
        }
        if logical.is_empty() {
            retain_wrapped_lyrics_row(&mut lines, String::new(), &mut row_index, start);
            continue;
        }
        let mut current = String::new();
        let mut width = 0_usize;
        for grapheme in logical.graphemes(true) {
            let grapheme_width = usize::from(grapheme.cell_width());
            if !current.is_empty() && width.saturating_add(grapheme_width) > available_width {
                retain_wrapped_lyrics_row(
                    &mut lines,
                    std::mem::take(&mut current),
                    &mut row_index,
                    start,
                );
                if lines.len() >= row_limit {
                    break;
                }
                width = 0;
            }
            if grapheme_width <= available_width {
                current.push_str(grapheme);
                width = width.saturating_add(grapheme_width);
            }
        }
        if lines.len() < row_limit && !current.is_empty() {
            retain_wrapped_lyrics_row(&mut lines, current, &mut row_index, start);
        }
    }
    lines
}

pub(crate) fn wrapped_lyrics_row_count(text: &str, available_width: usize) -> usize {
    let text = normalize_lyrics_text(text);
    wrapped_lyrics_row_count_normalized(&text, available_width)
}

fn wrapped_lyrics_row_count_normalized(text: &str, available_width: usize) -> usize {
    text.split('\n').fold(0_usize, |total, logical| {
        if logical.is_empty() {
            return total.saturating_add(1);
        }
        let mut rows = 0_usize;
        let mut width = 0_usize;
        let mut has_content = false;
        for grapheme in logical.graphemes(true) {
            let grapheme_width = usize::from(grapheme.cell_width());
            if has_content && width.saturating_add(grapheme_width) > available_width {
                rows = rows.saturating_add(1);
                width = 0;
                has_content = false;
            }
            if grapheme_width <= available_width {
                width = width.saturating_add(grapheme_width);
                has_content = true;
            }
        }
        total
            .saturating_add(rows)
            .saturating_add(usize::from(has_content))
    })
}

pub(crate) fn lyrics_overlay_viewport(area: Rect) -> Option<(usize, usize)> {
    if area.width < 12 || area.height < 4 {
        return None;
    }
    let popup = centered_rect(area, 84, 28);
    Some((
        usize::from(popup.height.saturating_sub(3)).min(MAX_RENDERED_ROWS),
        usize::from(popup.width.saturating_sub(2)),
    ))
}

fn retain_wrapped_lyrics_row(
    lines: &mut Vec<Line<'static>>,
    row: String,
    row_index: &mut usize,
    start: usize,
) {
    if *row_index >= start {
        lines.push(Line::from(row));
    }
    *row_index = row_index.saturating_add(1);
}

fn browser_picker_lines_with_viewport_and_targets(
    picker: BrowserPickerState,
    available_rows: usize,
    available_width: usize,
    viewport_memory: &mut SelectionViewport,
    mut targets: Option<&mut Vec<RenderedRowTarget>>,
) -> Vec<Line<'static>> {
    let choices = picker.choices();
    let selected = choices
        .iter()
        .position(|browser| *browser == picker.selected_browser())
        .unwrap_or(0);
    let viewport = viewport_memory.visible_range(
        choices.len(),
        Some(selected),
        available_rows.min(MAX_RENDERED_ROWS),
        dataset_key(choices),
    );
    choices
        .get(viewport.clone())
        .unwrap_or_default()
        .iter()
        .enumerate()
        .map(|(index, browser)| {
            if let Some(targets) = targets.as_deref_mut() {
                targets.push(viewport_memory.row_target(
                    index,
                    ListSurface::BrowserPicker,
                    viewport.start.saturating_add(index),
                ));
            }
            let marker = if viewport.start.saturating_add(index) == selected {
                "▶"
            } else {
                " "
            };
            Line::from(bounded_format_cells(
                available_width,
                format_args!("{marker} {}", browser.label()),
            ))
        })
        .collect()
}

#[cfg(test)]
fn country_picker_lines(
    picker: &CountryPickerState,
    available_rows: usize,
    available_width: usize,
) -> Vec<Line<'static>> {
    country_picker_lines_with_viewport(
        picker,
        available_rows,
        available_width,
        &mut SelectionViewport::default(),
    )
}

#[cfg(test)]
fn country_picker_lines_with_viewport(
    picker: &CountryPickerState,
    available_rows: usize,
    available_width: usize,
    viewport_memory: &mut SelectionViewport,
) -> Vec<Line<'static>> {
    country_picker_lines_with_viewport_and_targets(
        picker,
        available_rows,
        available_width,
        viewport_memory,
        None,
    )
}

fn country_picker_lines_with_viewport_and_targets(
    picker: &CountryPickerState,
    available_rows: usize,
    available_width: usize,
    viewport_memory: &mut SelectionViewport,
    mut targets: Option<&mut Vec<RenderedRowTarget>>,
) -> Vec<Line<'static>> {
    let row_limit = available_rows.min(MAX_RENDERED_ROWS);
    let viewport = viewport_memory.visible_range(
        picker.choices().len(),
        Some(picker.selected_index()),
        row_limit,
        dataset_key(
            &picker
                .choices()
                .iter()
                .map(CountryChoice::region)
                .collect::<Vec<_>>(),
        ),
    );
    picker
        .choices()
        .get(viewport.clone())
        .unwrap_or_default()
        .iter()
        .enumerate()
        .map(|(index, choice)| {
            let absolute_index = viewport.start.saturating_add(index);
            if let Some(targets) = targets.as_deref_mut() {
                targets.push(viewport_memory.row_target(
                    index,
                    ListSurface::CountryPicker,
                    absolute_index,
                ));
            }
            let marker = if picker.selected_index() == absolute_index {
                "▶"
            } else {
                " "
            };
            Line::from(bounded_format_cells(
                available_width,
                format_args!("{marker} {} · {}", choice.label(), choice.region().as_str()),
            ))
        })
        .collect()
}

fn help_lines(scroll: usize, available_rows: usize) -> Vec<Line<'static>> {
    HELP_TEXT
        .into_iter()
        .skip(scroll.min(HELP_LINE_COUNT.saturating_sub(available_rows)))
        .take(available_rows)
        .map(Line::from)
        .collect()
}

#[must_use]
pub(crate) fn help_overlay_viewport(area: Rect) -> usize {
    if area.width < 12 || area.height < 4 {
        return 0;
    }
    usize::from(centered_rect(area, 74, 15).height.saturating_sub(2))
}

#[cfg(test)]
fn palette_lines(
    palette: &CommandPaletteState,
    available_rows: usize,
    available_width: usize,
) -> Vec<Line<'static>> {
    palette_lines_with_targets(palette, available_rows, available_width, None)
}

fn palette_lines_with_targets(
    palette: &CommandPaletteState,
    available_rows: usize,
    available_width: usize,
    mut targets: Option<&mut Vec<RenderedRowTarget>>,
) -> Vec<Line<'static>> {
    if available_rows == 0 {
        return Vec::new();
    }

    let mut lines = Vec::with_capacity(available_rows);
    let truncation_marker = if palette.query_truncated { "…" } else { "" };
    lines.push(Line::from(bounded_format_cells(
        available_width,
        format_args!("Query: {}{truncation_marker}", palette.query()),
    )));
    if lines.len() >= available_rows {
        return lines;
    }
    let viewport = palette.viewport(available_rows.saturating_sub(1));
    if viewport.total == 0 && available_rows > 1 {
        lines.push(Line::from("No matching commands"));
        return lines;
    }
    let motion_dataset_key = hash_value(&(palette.query(), &viewport.entries));
    lines.extend(viewport.entries.iter().enumerate().map(|(offset, entry)| {
        let index = viewport.start.saturating_add(offset);
        if let Some(targets) = targets.as_deref_mut() {
            targets.push(RenderedRowTarget {
                line_index: 1usize.saturating_add(offset),
                surface: ListSurface::CommandPalette,
                stable_index: index,
                dataset_key: DatasetKey::Scalar(motion_dataset_key),
            });
        }
        let marker = if viewport.selected == Some(index) {
            "▶"
        } else {
            " "
        };
        Line::from(format!("{marker} {:<28} {}", entry.label, entry.shortcut))
    }));
    lines
}

fn centered_rect(area: Rect, preferred_width: u16, preferred_height: u16) -> Rect {
    let width = preferred_width.min(area.width.saturating_sub(2)).max(1);
    let height = preferred_height.min(area.height.saturating_sub(2)).max(1);
    Rect::new(
        area.x.saturating_add((area.width - width) / 2),
        area.y.saturating_add((area.height - height) / 2),
        width,
        height,
    )
}

fn split<const N: usize>(
    area: Rect,
    direction: Direction,
    constraints: [Constraint; N],
) -> Vec<Rect> {
    Layout::default()
        .direction(direction)
        .constraints(constraints)
        .split(area)
        .to_vec()
}

fn panel_block(title: &str, focused: bool, theme: &Theme) -> Block<'static> {
    let marker = if focused { "[*]" } else { "[ ]" };
    let border_type = if focused {
        BorderType::Double
    } else {
        BorderType::Plain
    };
    Block::new()
        .borders(Borders::ALL)
        .border_type(border_type)
        .title(format!("{marker} {title}"))
        .title_style(
            Style::default()
                .fg(if focused {
                    theme.selection
                } else {
                    theme.muted
                })
                .add_modifier(Modifier::BOLD),
        )
        .border_style(Style::default().fg(if focused { theme.accent } else { theme.muted }))
        .style(Style::default().fg(theme.foreground).bg(theme.background))
}

fn navigation_style(selected: bool, theme: &Theme) -> Style {
    if selected {
        Style::default()
            .fg(theme.selection)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.foreground)
    }
}

fn clip_lines(lines: &mut [Line<'static>], width: usize) {
    for line in lines {
        *line = clip_line(line, width);
    }
}

pub(crate) fn bounded_format_cells(max_width: usize, arguments: fmt::Arguments<'_>) -> String {
    if max_width == 0 {
        return String::new();
    }

    let mut writer = BoundedFormatWriter {
        buffer: String::with_capacity(max_width.saturating_mul(4).min(CLIP_BYTE_INSPECTION_BUDGET)),
        cut: false,
    };
    let _ = fmt::write(&mut writer, arguments);
    let Some(boundary) = truncation_boundary(&writer.buffer, max_width, writer.cut) else {
        return writer.buffer;
    };

    let mut bounded = String::with_capacity(boundary.saturating_add('…'.len_utf8()));
    bounded.push_str(&writer.buffer[..boundary]);
    bounded.push('…');
    bounded
}

struct BoundedFormatWriter {
    buffer: String,
    cut: bool,
}

impl fmt::Write for BoundedFormatWriter {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        if value.is_empty() {
            return Ok(());
        }
        let remaining = CLIP_BYTE_INSPECTION_BUDGET.saturating_sub(self.buffer.len());
        if value.len() <= remaining {
            self.buffer.push_str(value);
            return Ok(());
        }

        let mut take = remaining;
        while !value.is_char_boundary(take) {
            take = take.saturating_sub(1);
        }
        self.buffer.push_str(&value[..take]);
        self.cut = true;
        Err(fmt::Error)
    }
}

pub(crate) fn format_duration(milliseconds: u64) -> String {
    let total_seconds = milliseconds / 1000;
    let hours = total_seconds / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let seconds = total_seconds % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes:02}:{seconds:02}")
    }
}

const fn status_label(status: PlaybackStatus) -> &'static str {
    match status {
        PlaybackStatus::Stopped => "Stopped",
        PlaybackStatus::Resolving => "Resolving",
        PlaybackStatus::Buffering => "Buffering",
        PlaybackStatus::Playing => "Playing",
        PlaybackStatus::Paused => "Paused",
        PlaybackStatus::Failed => "Failed",
    }
}

const fn playback_icon(status: PlaybackStatus) -> &'static str {
    match status {
        PlaybackStatus::Playing => "❚❚",
        PlaybackStatus::Paused => "▶",
        PlaybackStatus::Resolving | PlaybackStatus::Buffering => "…",
        PlaybackStatus::Stopped => "■",
        PlaybackStatus::Failed => "!",
    }
}

const fn on_off(value: bool) -> &'static str {
    if value { "on" } else { "off" }
}

#[cfg(test)]
mod tests {
    use std::{error::Error, sync::Arc};

    use proptest::prelude::*;
    use ratatui::{Terminal, backend::TestBackend, style::Color};

    use crate::{
        app::{
            Action, AppError, AppErrorCategory, Effect, FavoriteMutation, SearchItem,
            SearchMetadata, SearchMetadataKind, SearchPage, reduce, stable_library_item_id,
        },
        config::Config,
        domain::{ChartSection, MediaId, MediaItem, MediaKind, RegionCode, SearchFilter},
        lyrics::{LyricsDocument, LyricsSource, TimedLyricLine},
        podcast_rankings::{PodcastRecommendationPage, parse_apple_top_shows},
        provider::{AuthenticationState, LibraryItem, LibrarySection, Page, Podcast},
        queue::{Queue, QueueItem, QueueItemId},
        storage::{FavoriteEntry, HistoryEntry},
        ui::{
            animation::{AnimationFrameStore, AnimationKey},
            artwork::{
                ArtworkGrid, ArtworkPresentation, ArtworkPresentationStore,
                PRODUCTION_ARTWORK_SIZE, decode_rgb_frame,
            },
            spectrum::{
                SpectrumFrame, SpectrumFrameStore, SpectrumKey, SpectrumPresentation,
                SpectrumTarget,
            },
        },
    };

    use super::*;

    #[test]
    fn selection_motion_glides_retargets_and_never_moves_the_viewport() {
        let mut memory = ViewportMemory::default();
        let area = Rect::new(0, 0, 80, 24);
        let range = memory.search.visible_range(12, Some(2), 8, 7_u64);
        let start = memory.search.start;

        memory.begin_selection_frame(area, 0);
        let initial = memory.present_selection(
            ListSurface::Search,
            12,
            Some(2),
            range.clone(),
            DatasetKey::Scalar(7),
            0,
        );
        memory.end_selection_frame();
        assert_eq!(initial.cursor_index, Some(2));
        assert!(!initial.transitioning);

        memory.begin_selection_frame(area, 0);
        let moved = memory.present_selection(
            ListSurface::Search,
            12,
            Some(3),
            range.clone(),
            DatasetKey::Scalar(7),
            0,
        );
        memory.end_selection_frame();
        assert_eq!(moved.logical_index, Some(3));
        assert_eq!(moved.cursor_index, Some(2));
        assert!(moved.transitioning);
        assert_eq!(memory.search.start, start);

        memory.begin_selection_frame(area, 60);
        let before_retarget = memory.present_selection(
            ListSurface::Search,
            12,
            Some(3),
            range.clone(),
            DatasetKey::Scalar(7),
            60,
        );
        memory.end_selection_frame();
        memory.begin_selection_frame(area, 60);
        let retargeted = memory.present_selection(
            ListSurface::Search,
            12,
            Some(1),
            range,
            DatasetKey::Scalar(7),
            60,
        );
        memory.end_selection_frame();
        assert_eq!(retargeted.cursor_index, before_retarget.cursor_index);
        assert!(retargeted.transitioning);

        memory.begin_selection_frame(area, 210);
        let finished = memory.present_selection(
            ListSurface::Search,
            12,
            Some(1),
            0..8,
            DatasetKey::Scalar(7),
            210,
        );
        memory.end_selection_frame();
        assert_eq!(finished.cursor_index, Some(1));
        assert!(!finished.transitioning);
    }

    #[test]
    fn selection_motion_caps_large_moves_and_snaps_offscreen_or_incompatible_data() {
        let mut memory = ViewportMemory::default();
        let area = Rect::new(0, 0, 80, 24);
        memory.begin_selection_frame(area, 0);
        let _ = memory.present_selection(
            ListSurface::Charts,
            40,
            Some(0),
            0..21,
            DatasetKey::Scalar(1),
            0,
        );
        memory.end_selection_frame();

        memory.begin_selection_frame(area, 1);
        let capped = memory.present_selection(
            ListSurface::Charts,
            40,
            Some(20),
            0..21,
            DatasetKey::Scalar(1),
            1,
        );
        memory.end_selection_frame();
        assert_eq!(capped.cursor_index, Some(14));
        assert!(capped.transitioning);

        memory.begin_selection_frame(area, 2);
        let offscreen = memory.present_selection(
            ListSurface::Charts,
            40,
            Some(30),
            25..33,
            DatasetKey::Scalar(1),
            2,
        );
        memory.end_selection_frame();
        assert_eq!(offscreen.cursor_index, Some(30));
        assert!(!offscreen.transitioning);

        memory.begin_selection_frame(area, 3);
        let replaced = memory.present_selection(
            ListSurface::Charts,
            40,
            Some(31),
            25..33,
            DatasetKey::Scalar(2),
            3,
        );
        memory.end_selection_frame();
        assert_eq!(replaced.cursor_index, Some(31));
        assert!(!replaced.transitioning);
    }

    #[test]
    fn selection_motion_resets_for_hidden_zero_reconciled_and_resized_surfaces() {
        let mut memory = ViewportMemory::default();
        let area = Rect::new(0, 0, 80, 24);
        let old_key = ordered_dataset_key(
            &"reconcile",
            &1_u64,
            [10_u64, 11, 12],
            DatasetUpdate::Reconcile,
        );
        memory.begin_selection_frame(area, 0);
        let _ = memory.present_selection(ListSurface::Library, 3, Some(1), 0..3, old_key, 0);
        memory.end_selection_frame();

        let new_key = ordered_dataset_key(
            &"reconcile",
            &2_u64,
            [9_u64, 10, 11, 12],
            DatasetUpdate::Reconcile,
        );
        memory.begin_selection_frame(area, 1);
        let reconciled =
            memory.present_selection(ListSurface::Library, 4, Some(2), 0..4, new_key, 1);
        memory.end_selection_frame();
        assert_eq!(reconciled.cursor_index, Some(2));
        assert!(!reconciled.transitioning);

        memory.begin_selection_frame(area, 2);
        let empty = memory.present_selection(
            ListSurface::Library,
            0,
            None,
            0..0,
            DatasetKey::Scalar(3),
            2,
        );
        memory.end_selection_frame();
        assert_eq!(empty, SelectionPresentation::default());

        memory.begin_selection_frame(area, 3);
        let _ = memory.present_selection(
            ListSurface::Library,
            8,
            Some(1),
            0..8,
            DatasetKey::Scalar(4),
            3,
        );
        memory.end_selection_frame();
        memory.begin_selection_frame(area, 4);
        memory.end_selection_frame();
        memory.begin_selection_frame(area, 5);
        let after_hidden = memory.present_selection(
            ListSurface::Library,
            8,
            Some(5),
            0..8,
            DatasetKey::Scalar(4),
            5,
        );
        memory.end_selection_frame();
        assert_eq!(after_hidden.cursor_index, Some(5));
        assert!(!after_hidden.transitioning);

        memory.begin_selection_frame(Rect::new(0, 0, 60, 18), 6);
        let resized = memory.present_selection(
            ListSurface::Library,
            8,
            Some(6),
            0..8,
            DatasetKey::Scalar(4),
            6,
        );
        memory.end_selection_frame();
        assert_eq!(resized.cursor_index, Some(6));
        assert!(!resized.transitioning);
    }

    #[test]
    fn selection_motion_shared_row_style_covers_every_targeted_list_surface() {
        let surfaces = [
            ListSurface::Search,
            ListSurface::Charts,
            ListSurface::PodcastRecommendations,
            ListSurface::PodcastEpisodes,
            ListSurface::Library,
            ListSurface::Favorites,
            ListSurface::History,
            ListSurface::Queue,
            ListSurface::CommandPalette,
            ListSurface::CountryPicker,
            ListSurface::BrowserPicker,
        ];
        let mut memory = ViewportMemory::default();
        let theme = Theme::default();
        let mut initial_lines = Vec::new();
        let mut targets = Vec::new();
        for surface in surfaces {
            let base = initial_lines.len();
            initial_lines.push(Line::from("▶ first"));
            initial_lines.push(Line::from("  second"));
            targets.push(RenderedRowTarget {
                line_index: base,
                surface,
                stable_index: 0,
                dataset_key: DatasetKey::Scalar(1),
            });
            targets.push(RenderedRowTarget {
                line_index: base + 1,
                surface,
                stable_index: 1,
                dataset_key: DatasetKey::Scalar(1),
            });
        }
        memory.begin_selection_frame(Rect::new(0, 0, 80, 24), 0);
        apply_selection_motion(
            &mut initial_lines,
            &targets,
            &mut memory,
            &theme,
            0,
            Rect::new(0, 0, 80, 24),
        );
        memory.end_selection_frame();

        let mut moved_lines = initial_lines
            .chunks_exact(2)
            .flat_map(|_| [Line::from("  first"), Line::from("▶ second")])
            .collect::<Vec<_>>();
        memory.begin_selection_frame(Rect::new(0, 0, 80, 24), 1);
        apply_selection_motion(
            &mut moved_lines,
            &targets,
            &mut memory,
            &theme,
            1,
            Rect::new(0, 0, 80, 24),
        );
        memory.end_selection_frame();

        for rows in moved_lines.chunks_exact(2) {
            assert_eq!(super::line_text(&rows[0]), "▶ first");
            assert_eq!(super::line_text(&rows[1]), "● second");
            assert!(rows[1].spans[1].style.add_modifier.contains(Modifier::BOLD));
        }
    }

    #[test]
    fn selection_motion_uses_stable_dataset_identity_when_clipped_labels_match() {
        let theme = Theme::default();
        let mut viewport = SelectionViewport::default();
        let mut memory = ViewportMemory::default();
        let first_key =
            ordered_dataset_key(&"search", &1_u8, ["old-a", "old-b"], DatasetUpdate::Replace);
        let _ = viewport.visible_range(2, Some(0), 2, first_key);
        let first_targets = [
            viewport.row_target(0, ListSurface::Search, 0),
            viewport.row_target(1, ListSurface::Search, 1),
        ];
        let mut first_lines = [Line::from("▶ duplicate…"), Line::from("  duplicate…")];
        memory.begin_selection_frame(Rect::new(0, 0, 20, 4), 0);
        apply_selection_motion(
            &mut first_lines,
            &first_targets,
            &mut memory,
            &theme,
            0,
            Rect::new(0, 0, 20, 4),
        );
        memory.end_selection_frame();

        let replacement_key =
            ordered_dataset_key(&"search", &2_u8, ["new-a", "new-b"], DatasetUpdate::Replace);
        let _ = viewport.visible_range(2, Some(1), 2, replacement_key);
        let replacement_targets = [
            viewport.row_target(0, ListSurface::Search, 0),
            viewport.row_target(1, ListSurface::Search, 1),
        ];
        let mut replacement_lines = [Line::from("  duplicate…"), Line::from("▶ duplicate…")];
        memory.begin_selection_frame(Rect::new(0, 0, 20, 4), 1);
        apply_selection_motion(
            &mut replacement_lines,
            &replacement_targets,
            &mut memory,
            &theme,
            1,
            Rect::new(0, 0, 20, 4),
        );
        memory.end_selection_frame();

        assert_eq!(super::line_text(&replacement_lines[0]), "  duplicate…");
        assert_eq!(super::line_text(&replacement_lines[1]), "▶ duplicate…");
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "one geometry trace covers visible, partial, full, and fallback overlay occlusion"
    )]
    fn completed_frame_motion_visibility_excludes_tiny_progress() -> Result<(), Box<dyn Error>> {
        let state = artwork_policy_state(None);
        let model = RenderModel::default().with_motion_frame(MotionFrame::default());

        let mut tiny_memory = ViewportMemory::default();
        let mut tiny = Terminal::new(TestBackend::new(40, 12))?;
        tiny.draw(|frame| {
            render_with_model_and_viewports(
                frame,
                &state,
                &Theme::default(),
                &model,
                RenderEnhancements::new(None, None, 15),
                &mut tiny_memory,
            );
        })?;
        assert!(!tiny_memory.progress_motion_visible());

        let mut compact_memory = ViewportMemory::default();
        let mut compact = Terminal::new(TestBackend::new(80, 24))?;
        compact.draw(|frame| {
            render_with_model_and_viewports(
                frame,
                &state,
                &Theme::default(),
                &model,
                RenderEnhancements::new(None, None, 15),
                &mut compact_memory,
            );
        })?;
        assert!(compact_memory.progress_motion_visible());

        compact.draw(|frame| {
            render_with_model_and_viewports(
                frame,
                &state,
                &Theme::default(),
                &model.clone().with_overlay(Overlay::Help),
                RenderEnhancements::new(None, None, 15),
                &mut compact_memory,
            );
        })?;
        assert!(
            compact_memory.progress_motion_visible(),
            "centered overlay leaves the compact player progress row visible"
        );

        compact.draw(|frame| {
            render_with_model_and_viewports(
                frame,
                &state,
                &Theme::default(),
                &model.clone().with_overlay(Overlay::Lyrics),
                RenderEnhancements::new(None, None, 15),
                &mut compact_memory,
            );
        })?;
        assert!(
            !compact_memory.progress_motion_visible(),
            "large lyrics popup fully covers compact progress cells"
        );

        let loading = reduce(
            AppState::default(),
            Action::SearchSubmitted {
                query: "visible spinner".to_owned(),
                filter: SearchFilter::Songs,
            },
        )
        .0;
        let visible_loading = RenderModel::default().with_view(NavigationItem::Search);
        compact.draw(|frame| {
            render_with_model_and_viewports(
                frame,
                &loading,
                &Theme::default(),
                &visible_loading,
                RenderEnhancements::new(None, None, 15),
                &mut compact_memory,
            );
        })?;
        assert!(compact_memory.spinner_motion_visible());

        compact.draw(|frame| {
            render_with_model_and_viewports(
                frame,
                &loading,
                &Theme::default(),
                &visible_loading.clone().with_overlay(Overlay::Help),
                RenderEnhancements::new(None, None, 15),
                &mut compact_memory,
            );
        })?;
        assert!(
            compact_memory.spinner_motion_visible(),
            "spinner prefix remains visible to the left of the centered popup"
        );

        let mut narrow_memory = ViewportMemory::default();
        let mut narrow = Terminal::new(TestBackend::new(10, 20))?;
        narrow.draw(|frame| {
            render_with_model_and_viewports(
                frame,
                &loading,
                &Theme::default(),
                &visible_loading.with_overlay(Overlay::Help),
                RenderEnhancements::new(None, None, 15),
                &mut narrow_memory,
            );
        })?;
        assert!(
            !narrow_memory.spinner_motion_visible(),
            "full-surface overlay fallback hides all prior motion"
        );
        Ok(())
    }

    #[test]
    fn overlay_target_buffer_exists_only_for_interactive_visible_rows() {
        assert!(overlay_target_buffer(Overlay::CommandPalette, false, 8).is_none());
        assert!(overlay_target_buffer(Overlay::Help, true, 8).is_none());

        let targets = overlay_target_buffer(
            Overlay::CommandPalette,
            true,
            MAX_RENDERED_ROWS.saturating_add(8),
        )
        .unwrap_or_else(|| panic!("interactive overlay target buffer"));
        assert!(targets.is_empty());
        assert_eq!(targets.capacity(), MAX_RENDERED_ROWS);
    }

    #[test]
    fn playback_control_semantic_action_exists_only_when_toggle_can_act() {
        let resolving = lyrics_test_state(Some(timed_test_document()), 1_500);
        assert!(player_control_labels(&resolving)[2].action.is_none());

        let generation = resolving
            .current_attempt_generation()
            .unwrap_or_else(|| panic!("playback action fixture generation"));
        let (playing, _) = reduce(
            resolving,
            Action::PlayerStatusChanged {
                generation,
                status: PlaybackStatus::Playing,
            },
        );
        assert_eq!(
            player_control_labels(&playing)[2].action,
            Some(SemanticAction::TogglePlayback)
        );
        for status in [
            PlaybackStatus::Paused,
            PlaybackStatus::Stopped,
            PlaybackStatus::Failed,
        ] {
            let (state, _) = reduce(
                playing.clone(),
                Action::PlayerStatusChanged { generation, status },
            );
            assert_eq!(
                player_control_labels(&state)[2].action,
                Some(SemanticAction::TogglePlayback),
                "{status:?} with a current queue item can restart playback"
            );
        }

        let (buffering, _) = reduce(
            playing,
            Action::PlayerStatusChanged {
                generation,
                status: PlaybackStatus::Buffering,
            },
        );
        assert!(player_control_labels(&buffering)[2].action.is_none());

        assert!(
            player_control_labels(&AppState::default())[2]
                .action
                .is_none()
        );
    }

    #[test]
    fn spectrum_wide_and_compact_reserve_rows_while_tiny_stays_one_line()
    -> Result<(), Box<dyn Error>> {
        let state = lyrics_test_state(Some(timed_test_document()), 1_500);
        let (_, presentation) = spectrum_fixture(&state, 64, 3, &[24, 18, 12, 6])?;

        let wide = render_spectrum_presentation(&state, &presentation, 140, 40)?;
        let compact = render_spectrum_presentation(&state, &presentation, 90, 30)?;
        let tiny = render_spectrum_presentation(&state, &presentation, 40, 12)?;

        let wide_player = line_index_containing(&wide, "Player · persistent")?;
        let compact_player = line_index_containing(&compact, "Player · persistent")?;
        assert_eq!(wide.lines().count().saturating_sub(wide_player), 11);
        assert_eq!(compact.lines().count().saturating_sub(compact_player), 7);
        assert!(wide.contains('█') || wide.contains('▄'), "{wide}");
        assert!(compact.contains('█') || compact.contains('▄'), "{compact}");
        assert!(!tiny.contains('█') && !tiny.contains('▁'), "{tiny}");
        assert_eq!(tiny.lines().count(), 12);
        Ok(())
    }

    #[test]
    fn spectrum_bass_uses_accent_and_paused_frame_is_frozen_and_dimmed()
    -> Result<(), Box<dyn Error>> {
        let state = lyrics_test_state(Some(timed_test_document()), 1_500);
        let (store, presentation) = spectrum_fixture(&state, 8, 3, &[24, 24, 16, 12, 8, 6, 4, 2])?;
        let theme = Theme::default();
        let playing = render_spectrum_buffer(&state, &presentation, 140, 40, &theme)?;
        assert!(playing.content.iter().any(|cell| {
            matches!(cell.symbol(), "█" | "▇" | "▆" | "▅" | "▄" | "▃" | "▂" | "▁")
                && cell.fg == theme.accent
        }));

        let key = spectrum_key(&state, 8, 3)?;
        let run = store.request(key.clone()).ok_or("spectrum run")?;
        let frame = Arc::new(
            SpectrumFrame::new(vec![24, 24, 16, 12, 8, 6, 4, 2].into_boxed_slice())
                .ok_or("frame")?,
        );
        assert!(store.publish(&run, frame));
        assert!(store.pause(&run));
        let paused = store.presentation(&key);
        let paused_buffer = render_spectrum_buffer(&state, &paused, 140, 40, &theme)?;
        assert!(paused_buffer.content.iter().any(|cell| {
            matches!(cell.symbol(), "█" | "▇" | "▆" | "▅" | "▄" | "▃" | "▂" | "▁")
                && cell.fg == theme.muted
        }));
        assert!(paused.paused());
        assert_eq!(paused.frame(), store.presentation(&key).frame());
        Ok(())
    }

    #[test]
    fn spectrum_gradient_true_color_varies_by_band_and_level() -> Result<(), Box<dyn Error>> {
        let state = lyrics_test_state(Some(timed_test_document()), 1_500);
        let (_, bands) = spectrum_fixture(&state, 3, 1, &[24, 24, 24])?;
        let theme = Theme::for_capability(ColorCapability::TrueColor);
        let colors = spectrum_lines(&bands, 1, 3, &theme)[0]
            .spans
            .iter()
            .map(|span| span.style.fg.ok_or("true-color spectrum foreground"))
            .collect::<Result<Vec<_>, _>>()?;
        assert_ne!(colors[0], colors[1]);
        assert_ne!(colors[1], colors[2]);

        let (_, quiet) = spectrum_fixture(&state, 1, 1, &[4])?;
        let (_, loud) = spectrum_fixture(&state, 1, 1, &[24])?;
        let quiet_color = spectrum_lines(&quiet, 1, 1, &theme)[0].spans[0]
            .style
            .fg
            .ok_or("quiet true-color spectrum foreground")?;
        let loud_color = spectrum_lines(&loud, 1, 1, &theme)[0].spans[0]
            .style
            .fg
            .ok_or("loud true-color spectrum foreground")?;
        let brightness = |color| match color {
            Color::Rgb(red, green, blue) => u16::from(red) + u16::from(green) + u16::from(blue),
            _ => 0,
        };
        assert!(brightness(loud_color) > brightness(quiet_color));
        assert!(matches!(loud_color, Color::Rgb(..)));
        Ok(())
    }

    #[test]
    fn spectrum_gradient_limited_colors_are_theme_safe_and_monochrome_uses_modifiers()
    -> Result<(), Box<dyn Error>> {
        let state = lyrics_test_state(Some(timed_test_document()), 1_500);
        let (_, presentation) = spectrum_fixture(&state, 3, 1, &[24, 24, 24])?;
        for capability in [ColorCapability::Ansi256, ColorCapability::Basic] {
            let theme = Theme::for_capability(capability);
            let colors = spectrum_lines(&presentation, 1, 3, &theme)[0]
                .spans
                .iter()
                .map(|span| span.style.fg.ok_or("limited-color spectrum foreground"))
                .collect::<Result<Vec<_>, _>>()?;
            assert_eq!(
                colors,
                vec![theme.accent, theme.selection, theme.foreground]
            );
            assert!(
                colors
                    .iter()
                    .all(|color| [theme.accent, theme.selection, theme.foreground].contains(color))
            );
        }

        let theme = Theme::for_capability(ColorCapability::Monochrome);
        let spans = &spectrum_lines(&presentation, 1, 3, &theme)[0].spans;
        assert!(spans.iter().all(|span| span.style.fg.is_none()));
        assert!(
            spans
                .iter()
                .all(|span| span.style.add_modifier.contains(Modifier::BOLD))
        );
        Ok(())
    }

    #[test]
    fn spectrum_missing_and_failed_analysis_render_quiet_baselines() -> Result<(), Box<dyn Error>> {
        let state = lyrics_test_state(Some(timed_test_document()), 1_500);
        let missing =
            render_spectrum_presentation(&state, &SpectrumPresentation::quiet(), 140, 40)?;
        assert!(missing.contains('▁'), "{missing}");

        let key = spectrum_key(&state, 64, 3)?;
        let store = SpectrumFrameStore::new();
        let run = store.request(key.clone()).ok_or("spectrum run")?;
        assert!(store.fail(&run));
        let failed = store.presentation(&key);
        assert!(failed.failed());
        let failed_render = render_spectrum_presentation(&state, &failed, 140, 40)?;
        assert!(failed_render.contains('▁'), "{failed_render}");
        let theme = Theme::default();
        let failed_buffer = render_spectrum_buffer(&state, &failed, 140, 40, &theme)?;
        assert!(
            failed_buffer
                .content
                .iter()
                .any(|cell| { cell.symbol() == "▁" && cell.fg == theme.muted })
        );
        Ok(())
    }

    #[test]
    fn spectrum_consecutive_frames_change_without_covering_player_content()
    -> Result<(), Box<dyn Error>> {
        let state = lyrics_test_state(Some(timed_test_document()), 1_500);
        let key = spectrum_key(&state, 64, 3)?;
        let store = SpectrumFrameStore::new();
        let run = store.request(key.clone()).ok_or("spectrum run")?;
        let low = Arc::new(SpectrumFrame::new(vec![2; 64].into_boxed_slice()).ok_or("low frame")?);
        assert!(store.publish(&run, low));
        let artwork = ArtworkPresentation::unavailable();
        let first =
            render_spectrum_and_artwork(&state, &store.presentation(&key), &artwork, 140, 40)?;
        let compact_first =
            render_spectrum_presentation(&state, &store.presentation(&key), 90, 30)?;
        let high =
            Arc::new(SpectrumFrame::new(vec![24; 64].into_boxed_slice()).ok_or("high frame")?);
        assert!(store.publish(&run, high));
        let second =
            render_spectrum_and_artwork(&state, &store.presentation(&key), &artwork, 140, 40)?;
        let compact_second =
            render_spectrum_presentation(&state, &store.presentation(&key), 90, 30)?;
        assert_ne!(first, second);
        assert_ne!(compact_first, compact_second);
        for rendered in [&first, &second] {
            for required in [
                "Lyrics render fixture",
                "Artwork",
                "Artwork unavailable",
                "00:01 / 03:00",
                "Target 80%",
                "Shuffle off",
                "[- Loading…]",
                "previous",
                "current",
                "next",
            ] {
                assert!(
                    rendered.contains(required),
                    "missing {required:?}:\n{rendered}"
                );
            }
        }
        Ok(())
    }

    #[test]
    fn spectrum_disabled_preserves_the_exact_pre_feature_layout() -> Result<(), Box<dyn Error>> {
        let mut config = Config::default();
        config.visualizer.enabled = false;
        let state = AppState::new(config);
        let baseline = render_plain_model(&state, 140, 40)?;
        let with_spectrum =
            render_spectrum_presentation(&state, &SpectrumPresentation::quiet(), 140, 40)?;
        assert_eq!(with_spectrum, baseline);
        Ok(())
    }

    #[test]
    fn spectrum_resampling_preserves_peaks_when_narrowing() {
        assert_eq!(resample_spectrum(&[1, 24, 2, 23], 2), vec![24, 23]);
        assert_eq!(resample_spectrum(&[7, 19], 4), vec![7, 7, 19, 19]);
    }

    #[test]
    fn spectrum_resize_keeps_queue_viewport_stable_and_player_visible() -> Result<(), Box<dyn Error>>
    {
        let mut state = AppState::default();
        for index in 0..40 {
            (state, _) = reduce(
                state,
                Action::EnqueueMedia {
                    item: queue_item(&format!("resize-{index}")).media().clone(),
                },
            );
        }
        let selected = state.queue().items()[35].id().clone();
        let mut model = RenderModel::default().with_focus(FocusRegion::Queue);
        model.set_queue_selected_id(Some(selected));
        let spectrum = SpectrumPresentation::quiet();
        let mut viewports = ViewportMemory::default();

        let wide =
            render_spectrum_with_viewports(&state, &model, &spectrum, 140, 40, &mut viewports)?;
        let compact =
            render_spectrum_with_viewports(&state, &model, &spectrum, 90, 30, &mut viewports)?;
        let wide_again =
            render_spectrum_with_viewports(&state, &model, &spectrum, 140, 40, &mut viewports)?;

        for rendered in [&wide, &compact, &wide_again] {
            assert!(rendered.contains("▶ Song resize-35"), "{rendered}");
            assert!(rendered.contains("Player · persistent"), "{rendered}");
            assert!(rendered.contains('▁'), "{rendered}");
        }
        assert!(wide.contains("Song resize-6"), "{wide}");
        assert!(compact.contains("Song resize-17"), "{compact}");
        assert!(wide_again.contains("Song resize-16"), "{wide_again}");
        assert!(!wide_again.contains("Song resize-6"), "{wide_again}");
        Ok(())
    }

    #[test]
    fn artwork_policy_without_static_url_keeps_fallback_except_for_wide_animation()
    -> Result<(), Box<dyn Error>> {
        let state = artwork_policy_state(None);
        let static_store = ArtworkPresentationStore::new();
        for layout in [LayoutMode::Wide, LayoutMode::Compact] {
            let presentation = artwork_presentation_from_stores(
                &state,
                Some(&static_store),
                None,
                PRODUCTION_ARTWORK_SIZE,
                layout,
            );
            assert!(
                is_unavailable_presentation(presentation.as_ref()),
                "{layout:?}"
            );
        }
        for layout in [LayoutMode::Wide, LayoutMode::Compact, LayoutMode::Tiny] {
            assert!(
                artwork_presentation_from_stores(
                    &state,
                    None,
                    None,
                    PRODUCTION_ARTWORK_SIZE,
                    layout,
                )
                .is_none(),
                "without a static store there is no fallback panel: {layout:?}"
            );
        }

        let generation = state
            .current_attempt_generation()
            .ok_or("no-url animation generation")?;
        let media_id = state
            .playback()
            .current
            .clone()
            .ok_or("no-url animation media")?;
        let key = AnimationKey::new(generation, media_id, PRODUCTION_ARTWORK_SIZE);
        let animation = AnimationFrameStore::new();
        assert!(animation.request(key.clone()));
        assert!(animation.publish(&key, solid_animation_frame([220, 20, 20])?));
        let wide = artwork_presentation_from_stores(
            &state,
            Some(&static_store),
            Some(&animation),
            PRODUCTION_ARTWORK_SIZE,
            LayoutMode::Wide,
        )
        .ok_or("wide animated presentation")?;
        assert!(presentation_starts_with_rgb(&wide, [220, 20, 20]));
        let compact = artwork_presentation_from_stores(
            &state,
            Some(&static_store),
            Some(&animation),
            PRODUCTION_ARTWORK_SIZE,
            LayoutMode::Compact,
        );
        assert!(is_unavailable_presentation(compact.as_ref()));
        assert!(
            artwork_presentation_from_stores(
                &state,
                Some(&static_store),
                Some(&animation),
                PRODUCTION_ARTWORK_SIZE,
                LayoutMode::Tiny,
            )
            .is_none(),
            "tiny must return before either observable store presentation"
        );
        Ok(())
    }

    #[test]
    fn artwork_policy_missing_or_stale_static_slot_is_secret_safe_fallback()
    -> Result<(), Box<dyn Error>> {
        let secret = "SIGNED_ARTWORK_TOKEN_MUST_NOT_LEAK";
        let state = artwork_policy_state(Some(url::Url::parse(&format!(
            "https://art.invalid/policy?signature={secret}"
        ))?));
        let store = ArtworkPresentationStore::new();
        for layout in [LayoutMode::Wide, LayoutMode::Compact] {
            let missing = artwork_presentation_from_stores(
                &state,
                Some(&store),
                None,
                PRODUCTION_ARTWORK_SIZE,
                layout,
            );
            assert!(is_unavailable_presentation(missing.as_ref()), "{layout:?}");
            assert!(!format!("{missing:?}").contains(secret));
        }

        let requested = state
            .artwork()
            .requested_url()
            .ok_or("signed artwork request")?;
        let stale_generation =
            crate::app::Generation::new(state.artwork().generation().value().saturating_add(1));
        store.request(stale_generation, requested);
        assert!(store.publish(
            stale_generation,
            requested,
            ArtworkPresentation::Grid(solid_animation_frame([20, 20, 220])?),
        ));
        for layout in [LayoutMode::Wide, LayoutMode::Compact] {
            let presentation = artwork_presentation_from_stores(
                &state,
                Some(&store),
                None,
                PRODUCTION_ARTWORK_SIZE,
                layout,
            );
            assert!(
                is_unavailable_presentation(presentation.as_ref()),
                "{layout:?}"
            );
            assert!(!format!("{store:?} {presentation:?}").contains(secret));
        }
        Ok(())
    }

    #[test]
    fn animation_store_latest_frame_changes_wide_artwork_without_covering_player_or_lyrics()
    -> Result<(), Box<dyn Error>> {
        let state = playing_test_state(lyrics_test_state(Some(timed_test_document()), 1_500));
        let generation = state
            .current_attempt_generation()
            .ok_or("animation fixture must have a playback generation")?;
        let media_id = state
            .playback()
            .current
            .clone()
            .ok_or("animation fixture must have current media")?;
        let key = AnimationKey::new(generation, media_id, PRODUCTION_ARTWORK_SIZE);
        let store = AnimationFrameStore::new();
        let static_store = static_artwork_store(&state, [20, 180, 20])?;
        assert!(store.request(key.clone()));

        let first = solid_animation_frame([220, 20, 20])?;
        assert!(store.publish(&key, Arc::clone(&first)));
        let first_render = render_animation_store_frame(&state, &static_store, &store, 140, 40)?;

        let second = solid_animation_frame([20, 20, 220])?;
        assert!(store.publish(&key, Arc::clone(&second)));
        let second_render = render_animation_store_frame(&state, &static_store, &store, 140, 40)?;

        assert_ne!(
            first_render, second_render,
            "consecutive frames must redraw"
        );
        for rendered in [&first_render, &second_render] {
            assert!(rendered.contains("[Space Pause]"), "{rendered}");
            assert!(rendered.contains("current"), "{rendered}");
        }
        Ok(())
    }

    #[test]
    fn animation_is_wide_only_and_pause_retains_the_current_frame() -> Result<(), Box<dyn Error>> {
        let state = playing_test_state(lyrics_test_state(Some(timed_test_document()), 1_500));
        let generation = state
            .current_attempt_generation()
            .ok_or("animation fixture must have a playback generation")?;
        let media_id = state
            .playback()
            .current
            .clone()
            .ok_or("animation fixture must have current media")?;
        let key = AnimationKey::new(generation, media_id, PRODUCTION_ARTWORK_SIZE);
        let store = AnimationFrameStore::new();
        let static_store = static_artwork_store(&state, [20, 180, 20])?;
        assert!(store.request(key.clone()));
        assert!(store.publish(&key, solid_animation_frame([120, 30, 10])?));

        let wide_before = render_animation_store_frame(&state, &static_store, &store, 140, 40)?;
        let compact_before = render_animation_store_frame(&state, &static_store, &store, 90, 30)?;
        let tiny_before = render_animation_store_frame(&state, &static_store, &store, 40, 12)?;
        assert!(store.publish(&key, solid_animation_frame([10, 30, 120])?));
        let wide_after = render_animation_store_frame(&state, &static_store, &store, 140, 40)?;
        let compact_after = render_animation_store_frame(&state, &static_store, &store, 90, 30)?;
        let tiny_after = render_animation_store_frame(&state, &static_store, &store, 40, 12)?;

        assert_ne!(wide_before, wide_after);
        assert_eq!(compact_before, compact_after);
        assert_eq!(tiny_before, tiny_after);
        assert!(store.pause(&key));
        assert!(!store.publish(&key, solid_animation_frame([180, 180, 10])?));
        let paused_state = reduce(
            state,
            Action::PlayerStatusChanged {
                generation,
                status: PlaybackStatus::Paused,
            },
        )
        .0;
        let wide_paused =
            render_animation_store_frame(&paused_state, &static_store, &store, 140, 40)?;

        assert!(wide_after.contains("Rgb(10, 30, 120)"), "{wide_after}");
        assert!(wide_paused.contains("Rgb(10, 30, 120)"), "{wide_paused}");
        assert!(!wide_paused.contains("Rgb(20, 180, 20)"), "{wide_paused}");
        assert!(
            compact_after.contains("Rgb(20, 180, 20)"),
            "{compact_after}"
        );
        assert!(!tiny_after.contains("Rgb(20, 180, 20)"), "{tiny_after}");
        assert!(!tiny_after.contains("Rgb(10, 30, 120)"), "{tiny_after}");
        assert!(compact_after.contains("current"), "{compact_after}");
        assert!(tiny_after.contains("Lyrics render fixture"), "{tiny_after}");
        Ok(())
    }

    #[test]
    fn compact_artwork_is_bounded_without_starving_content_or_queue() -> Result<(), Box<dyn Error>>
    {
        let state = lyrics_test_state(Some(timed_test_document()), 1_500);
        let artwork = ArtworkPresentation::Grid(solid_animation_frame([20, 180, 20])?);

        for panel in [CompactPanel::Content, CompactPanel::Queue] {
            let model = RenderModel {
                compact_panel: panel,
                ..RenderModel::default()
            };
            let mut terminal = Terminal::new(TestBackend::new(60, 18))?;
            terminal.draw(|frame| {
                render_with_model_and_artwork(frame, &state, &Theme::default(), &model, &artwork);
            })?;
            let first_artwork_column = (0..60)
                .find(|&x| {
                    (0..18).any(|y| {
                        terminal
                            .backend()
                            .buffer()
                            .cell((x, y))
                            .is_some_and(|cell| cell.fg == Color::Rgb(20, 180, 20))
                    })
                })
                .ok_or("compact artwork pixels")?;
            let rendered = terminal.backend().to_string();
            assert!(
                first_artwork_column >= 47,
                "compact artwork must preserve at least 44 content columns:\n{rendered}"
            );
            match panel {
                CompactPanel::Content => {
                    assert!(rendered.contains("Lyrics render fixture"), "{rendered}");
                }
                CompactPanel::Queue => {
                    assert!(rendered.contains("Queue"), "{rendered}");
                }
            }
        }
        Ok(())
    }

    #[test]
    fn tiny_layout_ignores_every_artwork_presentation() -> Result<(), Box<dyn Error>> {
        let state = lyrics_test_state(Some(timed_test_document()), 1_500);
        let artwork = ArtworkPresentation::Grid(solid_animation_frame([20, 180, 20])?);
        let mut baseline = Terminal::new(TestBackend::new(40, 12))?;
        baseline.draw(|frame| {
            render_with_model(frame, &state, &Theme::default(), &RenderModel::default());
        })?;
        let mut candidate = Terminal::new(TestBackend::new(40, 12))?;
        candidate.draw(|frame| {
            render_with_model_and_artwork(
                frame,
                &state,
                &Theme::default(),
                &RenderModel::default(),
                &artwork,
            );
        })?;

        assert_eq!(candidate.backend().buffer(), baseline.backend().buffer());
        assert!(!candidate.backend().to_string().contains('▀'));
        Ok(())
    }

    #[test]
    fn missing_or_failed_animation_uses_the_existing_static_fallback() -> Result<(), Box<dyn Error>>
    {
        let state = lyrics_test_state(Some(timed_test_document()), 1_500);
        let generation = state
            .current_attempt_generation()
            .ok_or("animation fixture must have a playback generation")?;
        let media_id = state
            .playback()
            .current
            .clone()
            .ok_or("animation fixture must have current media")?;
        let key = AnimationKey::new(generation, media_id, PRODUCTION_ARTWORK_SIZE);
        let store = AnimationFrameStore::new();
        let static_store = static_artwork_store(&state, [20, 180, 20])?;
        assert!(store.request(key.clone()));
        let static_presentation = state
            .artwork()
            .requested_url()
            .and_then(|url| static_store.presentation(state.artwork().generation(), url))
            .ok_or("static artwork fixture must be published")?;
        for (width, height) in [(140, 40), (90, 30), (40, 12)] {
            let expected_static =
                render_artwork_presentation(&state, &static_presentation, width, height)?;
            let missing =
                render_animation_store_frame(&state, &static_store, &store, width, height)?;
            assert_eq!(missing, expected_static);
            assert!(!missing.contains("Artwork unavailable"), "{missing}");
            assert_eq!(
                missing.contains("Rgb(20, 180, 20)"),
                LayoutMode::from_dimensions(width, height) != LayoutMode::Tiny,
                "{missing}"
            );
        }
        assert!(store.fail(&key));
        for (width, height) in [(140, 40), (90, 30), (40, 12)] {
            let expected_static =
                render_artwork_presentation(&state, &static_presentation, width, height)?;
            let failed =
                render_animation_store_frame(&state, &static_store, &store, width, height)?;
            assert_eq!(failed, expected_static);
            assert_eq!(
                failed.contains("Rgb(20, 180, 20)"),
                LayoutMode::from_dimensions(width, height) != LayoutMode::Tiny,
                "{failed}"
            );
        }
        Ok(())
    }

    #[test]
    fn wide_artwork_rejects_disabled_and_stale_animation_frames() -> Result<(), Box<dyn Error>> {
        let mut config = Config::default();
        config.artwork.animated = false;
        let disabled = playing_test_state(lyrics_test_state_with_config(
            config,
            Some(timed_test_document()),
            1_500,
        ));
        let generation = disabled
            .current_attempt_generation()
            .ok_or("disabled animation generation")?;
        let media_id = disabled
            .playback()
            .current
            .clone()
            .ok_or("disabled animation media")?;
        let current_key = AnimationKey::new(generation, media_id.clone(), PRODUCTION_ARTWORK_SIZE);
        let animation = AnimationFrameStore::new();
        assert!(animation.request(current_key));
        assert!(animation.publish(
            &AnimationKey::new(generation, media_id.clone(), PRODUCTION_ARTWORK_SIZE),
            solid_animation_frame([220, 20, 20])?,
        ));
        let static_store = static_artwork_store(&disabled, [20, 180, 20])?;
        let disabled_presentation = artwork_presentation_from_stores(
            &disabled,
            Some(&static_store),
            Some(&animation),
            PRODUCTION_ARTWORK_SIZE,
            LayoutMode::Wide,
        )
        .ok_or("disabled static presentation")?;
        assert!(presentation_starts_with_rgb(
            &disabled_presentation,
            [20, 180, 20]
        ));

        let enabled = playing_test_state(lyrics_test_state(Some(timed_test_document()), 1_500));
        let stale_generation = crate::app::Generation::new(
            enabled
                .current_attempt_generation()
                .ok_or("enabled animation generation")?
                .value()
                .saturating_add(1),
        );
        let stale_key = AnimationKey::new(stale_generation, media_id, PRODUCTION_ARTWORK_SIZE);
        let stale = AnimationFrameStore::new();
        assert!(stale.request(stale_key.clone()));
        assert!(stale.publish(&stale_key, solid_animation_frame([20, 20, 220])?));
        let enabled_static = static_artwork_store(&enabled, [20, 180, 20])?;
        let stale_presentation = artwork_presentation_from_stores(
            &enabled,
            Some(&enabled_static),
            Some(&stale),
            PRODUCTION_ARTWORK_SIZE,
            LayoutMode::Wide,
        )
        .ok_or("stale static presentation")?;
        assert!(presentation_starts_with_rgb(
            &stale_presentation,
            [20, 180, 20]
        ));
        Ok(())
    }

    #[test]
    fn lyrics_player_lines_show_synced_context_by_layout_and_accent_current() {
        let state = lyrics_test_state(Some(timed_test_document()), 1_500);
        let theme = Theme::default();
        let wide = automatic_lyrics_lines(&state, 3, 80, &theme);
        assert_eq!(line_text(&wide), "previous\ncurrent\nnext");
        assert_eq!(wide[1].spans[0].style.fg, Some(theme.accent));

        let compact = automatic_lyrics_lines(&state, 1, 40, &theme);
        assert_eq!(line_text(&compact), "current");
        assert_eq!(compact[0].spans[0].style.fg, Some(theme.accent));

        let tiny = automatic_lyrics_lines(&state, 1, 8, &theme);
        assert_eq!(line_text(&tiny), "current");
        assert!(automatic_lyrics_lines(&state, 0, 8, &theme).is_empty());
    }

    #[test]
    fn lyric_fade_true_color_has_exact_start_midpoint_and_settled_boundaries() {
        let theme = Theme::for_capability(ColorCapability::TrueColor);
        let at_start = automatic_lyrics_lines(
            &lyrics_test_state(Some(timed_test_document()), 1_000),
            3,
            80,
            &theme,
        );
        let at_midpoint = automatic_lyrics_lines(
            &lyrics_test_state(Some(timed_test_document()), 1_200),
            3,
            80,
            &theme,
        );
        let settled = automatic_lyrics_lines(
            &lyrics_test_state(Some(timed_test_document()), 1_400),
            3,
            80,
            &theme,
        );

        for lines in [&at_start, &at_midpoint, &settled] {
            assert_eq!(lines.len(), 3);
            assert_eq!(line_text(lines), "previous\ncurrent\nnext");
        }
        assert_eq!(at_start[0].spans[0].style.fg, Some(theme.accent));
        assert_eq!(at_start[1].spans[0].style.fg, Some(theme.muted));
        assert!(
            at_start[0].spans[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert!(
            !at_start[1].spans[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert_eq!(
            at_midpoint[0].spans[0].style.fg,
            Some(Color::Rgb(113, 176, 206))
        );
        assert_eq!(
            at_midpoint[1].spans[0].style.fg,
            Some(Color::Rgb(113, 176, 206))
        );
        assert!(
            at_midpoint[1].spans[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert_eq!(settled[0].spans[0].style.fg, Some(theme.muted));
        assert_eq!(settled[1].spans[0].style.fg, Some(theme.accent));
    }

    #[test]
    fn lyric_fade_short_lines_and_open_final_line_remain_bounded() {
        let document = LyricsDocument::new(
            LyricsSource::Lrclib,
            None,
            vec![
                TimedLyricLine::new(0, Some(100), "zero")
                    .unwrap_or_else(|error| panic!("short lyric fixture: {error}")),
                TimedLyricLine::new(100, Some(200), "one")
                    .unwrap_or_else(|error| panic!("short lyric fixture: {error}")),
                TimedLyricLine::new(200, None, "two")
                    .unwrap_or_else(|error| panic!("short lyric fixture: {error}")),
            ],
            false,
        )
        .unwrap_or_else(|error| panic!("short lyric document: {error}"));
        let theme = Theme::default();
        let midpoint = automatic_lyrics_lines(
            &lyrics_test_state(Some(document.clone()), 125),
            3,
            80,
            &theme,
        );
        let settled = automatic_lyrics_lines(
            &lyrics_test_state(Some(document.clone()), 150),
            3,
            80,
            &theme,
        );
        let final_start = automatic_lyrics_lines(
            &lyrics_test_state(Some(document.clone()), 200),
            3,
            80,
            &theme,
        );
        let final_settled =
            automatic_lyrics_lines(&lyrics_test_state(Some(document), 250), 3, 80, &theme);

        assert_eq!(line_text(&midpoint), "zero\none\ntwo");
        assert_eq!(line_text(&settled), "zero\none\ntwo");
        assert_eq!(line_text(&final_start), "zero\none\ntwo");
        assert_eq!(line_text(&final_settled), "zero\none\ntwo");
        assert_eq!(midpoint[0].spans[0].style.fg, midpoint[1].spans[0].style.fg);
        assert_eq!(settled[1].spans[0].style.fg, Some(theme.accent));
        assert_eq!(final_start[1].spans[0].style.fg, Some(theme.accent));
        assert_eq!(final_start[2].spans[0].style.fg, Some(theme.muted));
        assert_eq!(final_settled[2].spans[0].style.fg, Some(theme.accent));
    }

    #[test]
    fn lyric_fade_limited_and_monochrome_styles_are_capability_safe() {
        for capability in [ColorCapability::Ansi256, ColorCapability::Basic] {
            let theme = Theme::for_capability(capability);
            let start = automatic_lyrics_lines(
                &lyrics_test_state(Some(timed_test_document()), 1_000),
                3,
                80,
                &theme,
            );
            let midpoint = automatic_lyrics_lines(
                &lyrics_test_state(Some(timed_test_document()), 1_200),
                3,
                80,
                &theme,
            );
            let settled = automatic_lyrics_lines(
                &lyrics_test_state(Some(timed_test_document()), 1_400),
                3,
                80,
                &theme,
            );
            assert_eq!(start[0].spans[0].style.fg, Some(theme.accent));
            assert_eq!(start[1].spans[0].style.fg, Some(theme.muted));
            assert_eq!(midpoint[0].spans[0].style.fg, Some(theme.selection));
            assert_eq!(midpoint[1].spans[0].style.fg, Some(theme.selection));
            assert_eq!(settled[1].spans[0].style.fg, Some(theme.accent));
        }

        let theme = Theme::for_capability(ColorCapability::Monochrome);
        let start = automatic_lyrics_lines(
            &lyrics_test_state(Some(timed_test_document()), 1_000),
            3,
            80,
            &theme,
        );
        let midpoint = automatic_lyrics_lines(
            &lyrics_test_state(Some(timed_test_document()), 1_200),
            3,
            80,
            &theme,
        );
        assert!(start.iter().all(|line| line.spans[0].style.fg.is_none()));
        assert!(midpoint.iter().all(|line| line.spans[0].style.fg.is_none()));
        assert!(
            start[0].spans[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert!(
            midpoint[1].spans[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
    }

    #[test]
    fn lyric_fade_gap_never_reaccents_the_ended_previous_line() {
        let document = LyricsDocument::new(
            LyricsSource::Lrclib,
            None,
            vec![
                TimedLyricLine::new(0, Some(40), "ended")
                    .unwrap_or_else(|error| panic!("gapped render fixture: {error}")),
                TimedLyricLine::new(100, Some(200), "incoming")
                    .unwrap_or_else(|error| panic!("gapped render fixture: {error}")),
                TimedLyricLine::new(200, None, "next")
                    .unwrap_or_else(|error| panic!("gapped render fixture: {error}")),
            ],
            false,
        )
        .unwrap_or_else(|error| panic!("gapped render document: {error}"));
        let theme = Theme::default();
        let lines = automatic_lyrics_lines(&lyrics_test_state(Some(document), 100), 3, 80, &theme);

        assert_eq!(line_text(&lines), "ended\nincoming\nnext");
        assert_eq!(lines[0].spans[0].style.fg, Some(theme.muted));
        assert!(
            !lines[0].spans[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert_eq!(lines[1].spans[0].style.fg, Some(theme.muted));
    }

    #[test]
    fn lyric_fade_limited_colors_have_three_inverse_progress_stages() {
        for capability in [ColorCapability::Ansi256, ColorCapability::Basic] {
            let theme = Theme::for_capability(capability);
            let quarter = automatic_lyrics_lines(
                &lyrics_test_state(Some(timed_test_document()), 1_100),
                3,
                80,
                &theme,
            );
            let midpoint = automatic_lyrics_lines(
                &lyrics_test_state(Some(timed_test_document()), 1_200),
                3,
                80,
                &theme,
            );
            let three_quarters = automatic_lyrics_lines(
                &lyrics_test_state(Some(timed_test_document()), 1_300),
                3,
                80,
                &theme,
            );
            let incoming = [
                quarter[1].spans[0].style,
                midpoint[1].spans[0].style,
                three_quarters[1].spans[0].style,
            ];
            let outgoing = [
                quarter[0].spans[0].style,
                midpoint[0].spans[0].style,
                three_quarters[0].spans[0].style,
            ];

            assert_eq!(incoming[0].fg, Some(theme.muted));
            assert_eq!(incoming[1].fg, Some(theme.selection));
            assert_eq!(incoming[2].fg, Some(theme.accent));
            assert_eq!(outgoing[0].fg, Some(theme.accent));
            assert_eq!(outgoing[1].fg, Some(theme.selection));
            assert_eq!(outgoing[2].fg, Some(theme.muted));
            assert!(incoming[0].add_modifier.contains(Modifier::DIM));
            assert!(incoming[1].add_modifier.contains(Modifier::BOLD));
            assert!(incoming[2].add_modifier.contains(Modifier::BOLD));
            assert!(outgoing[0].add_modifier.contains(Modifier::BOLD));
            assert!(!outgoing[1].add_modifier.contains(Modifier::BOLD));
            assert!(outgoing[2].add_modifier.contains(Modifier::DIM));
            assert!(incoming.windows(2).all(|pair| pair[0] != pair[1]));
            assert!(outgoing.windows(2).all(|pair| pair[0] != pair[1]));
            assert!(
                incoming
                    .iter()
                    .chain(outgoing.iter())
                    .filter_map(|style| style.fg)
                    .all(|color| [theme.muted, theme.selection, theme.accent].contains(&color))
            );
        }
    }

    #[test]
    fn lyric_fade_is_position_derived_for_pause_and_seeks_without_layout_movement() {
        let document = timed_test_document();
        let theme = Theme::default();
        let playing = playing_test_state(lyrics_test_state(Some(document.clone()), 1_100));
        let generation = playing
            .current_attempt_generation()
            .unwrap_or_else(|| panic!("lyric fade playback generation"));
        let paused = reduce(
            playing.clone(),
            Action::PlayerStatusChanged {
                generation,
                status: PlaybackStatus::Paused,
            },
        )
        .0;
        let playing_lines = automatic_lyrics_lines(&playing, 3, 80, &theme);
        let paused_lines = automatic_lyrics_lines(&paused, 3, 80, &theme);
        assert_eq!(playing_lines, paused_lines);

        let media_id = playing
            .playback()
            .current
            .clone()
            .unwrap_or_else(|| panic!("lyric fade current media"));
        let backward = playing_lines;
        let forward_state = reduce(
            playing,
            Action::PlayerProgress {
                generation,
                media_id: media_id.clone(),
                position_ms: 2_100,
                duration_ms: Some(180_000),
            },
        )
        .0;
        let forward = automatic_lyrics_lines(&forward_state, 3, 80, &theme);
        let backward_state = reduce(
            forward_state,
            Action::PlayerProgress {
                generation,
                media_id,
                position_ms: 1_100,
                duration_ms: Some(180_000),
            },
        )
        .0;
        let backward_again = automatic_lyrics_lines(&backward_state, 3, 80, &theme);
        assert_eq!(backward_again, backward);
        assert_ne!(forward, backward);
        assert_eq!(line_text(&backward), "previous\ncurrent\nnext");
        assert_eq!(line_text(&forward), "previous\ncurrent\nnext");

        let compact_start = automatic_lyrics_lines(
            &lyrics_test_state(Some(timed_test_document()), 1_000),
            1,
            40,
            &theme,
        );
        let compact_midpoint = automatic_lyrics_lines(
            &lyrics_test_state(Some(timed_test_document()), 1_200),
            1,
            40,
            &theme,
        );
        assert_eq!(compact_start.len(), 1);
        assert_eq!(compact_midpoint.len(), 1);
        assert_eq!(line_text(&compact_start), "current");
        assert_eq!(line_text(&compact_midpoint), "current");
        let tiny_start = automatic_lyrics_lines(
            &lyrics_test_state(Some(document.clone()), 1_000),
            1,
            8,
            &theme,
        );
        let tiny_midpoint =
            automatic_lyrics_lines(&lyrics_test_state(Some(document), 1_200), 1, 8, &theme);
        assert_eq!(tiny_start.len(), 1);
        assert_eq!(tiny_midpoint.len(), 1);
        assert_eq!(line_text(&tiny_start), "current");
        assert_eq!(line_text(&tiny_midpoint), "current");
    }

    #[test]
    fn lyrics_compact_player_renders_the_automatic_current_line() -> Result<(), Box<dyn Error>> {
        let state = lyrics_test_state(Some(timed_test_document()), 1_500);
        let mut terminal = Terminal::new(TestBackend::new(90, 30))?;
        terminal.draw(|frame| {
            render_with_model(frame, &state, &Theme::default(), &RenderModel::default());
        })?;
        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("current"), "{rendered}");
        Ok(())
    }

    #[test]
    fn lyrics_tiny_player_renders_the_current_line_with_timestamp_style_when_roomy()
    -> Result<(), Box<dyn Error>> {
        let document = timed_test_document();
        let state = playing_test_state(lyrics_test_state(Some(document.clone()), 1_200));
        let theme = Theme::for_capability(ColorCapability::TrueColor);
        let mut terminal = Terminal::new(TestBackend::new(59, 17))?;
        terminal.draw(|frame| {
            render_with_model(frame, &state, &theme, &RenderModel::default());
        })?;

        let rendered = terminal.backend().to_string();
        let player = rendered
            .lines()
            .last()
            .unwrap_or_default()
            .trim_matches('"');
        assert!(player.contains("current"), "{rendered}");
        for required in ["❚❚", "[p]", "[←]", "[Spc]", "[→]", "[n]"] {
            assert!(player.contains(required), "missing `{required}`: {player}");
        }
        let lyric_column =
            u16::try_from(player.find("current").ok_or("tiny lyric column missing")?)?;
        let expected = lyric_transition_style(&theme, 1, 1, document.transition_at(1_200));
        let cell = terminal
            .backend()
            .buffer()
            .cell((lyric_column, 16))
            .ok_or("tiny lyric cell missing")?;
        assert_eq!(cell.fg, expected.fg.unwrap_or(theme.foreground));
        assert_eq!(cell.modifier, expected.add_modifier);
        Ok(())
    }

    #[test]
    fn lyrics_tiny_player_resets_transition_style_across_reused_terminal_frames()
    -> Result<(), Box<dyn Error>> {
        let document = timed_test_document();
        for capability in [ColorCapability::TrueColor, ColorCapability::Monochrome] {
            let theme = Theme::for_capability(capability);
            let mut terminal = Terminal::new(TestBackend::new(59, 17))?;
            let expectations = match capability {
                ColorCapability::TrueColor => [
                    (1_000, theme.muted, Modifier::empty()),
                    (1_200, Color::Rgb(113, 176, 206), Modifier::BOLD),
                    (1_400, theme.accent, Modifier::BOLD),
                ],
                ColorCapability::Monochrome => [
                    (1_000, Color::Reset, Modifier::DIM),
                    (1_200, Color::Reset, Modifier::BOLD),
                    (1_400, Color::Reset, Modifier::BOLD),
                ],
                ColorCapability::Ansi256 | ColorCapability::Basic => unreachable!(),
            };
            for (position_ms, expected_foreground, expected_modifier) in expectations {
                let state =
                    playing_test_state(lyrics_test_state(Some(document.clone()), position_ms));
                let layout = tiny_player_layout(&state, 59);
                let (lyric_offset, lyric_width) = layout
                    .identity
                    .ok_or("tiny lyric identity region missing")?;
                assert!(lyric_width >= TINY_LYRICS_MIN_IDENTITY_WIDTH);
                let next_offset = layout
                    .controls
                    .iter()
                    .find_map(|(offset, _, action)| {
                        (*action == SemanticAction::NextTrack).then_some(*offset)
                    })
                    .ok_or("tiny next control missing")?;

                terminal.draw(|frame| {
                    render_with_model(frame, &state, &theme, &RenderModel::default());
                })?;

                let buffer = terminal.backend().buffer();
                for column in lyric_offset..lyric_offset.saturating_add("current".cell_width()) {
                    let cell = buffer
                        .cell((column, 16))
                        .ok_or("tiny styled lyric cell missing")?;
                    assert_eq!(
                        cell.fg, expected_foreground,
                        "{capability:?} at {position_ms}ms, column {column}"
                    );
                    assert_eq!(
                        cell.bg, theme.background,
                        "{capability:?} at {position_ms}ms, column {column}"
                    );
                    assert_eq!(
                        cell.modifier, expected_modifier,
                        "{capability:?} at {position_ms}ms, column {column}"
                    );
                }
                let trailing = buffer
                    .cell((lyric_offset.saturating_add(lyric_width - 1), 16))
                    .ok_or("tiny trailing identity cell missing")?;
                assert_eq!(trailing.fg, theme.foreground);
                assert_eq!(trailing.bg, theme.background);
                assert_eq!(trailing.modifier, Modifier::empty());

                for column in [0, next_offset] {
                    let cell = buffer
                        .cell((column, 16))
                        .ok_or("tiny preserved player cell missing")?;
                    assert_eq!(cell.fg, theme.foreground);
                    assert_eq!(cell.bg, theme.background);
                    assert_eq!(cell.modifier, Modifier::BOLD);
                }
            }
        }
        Ok(())
    }

    #[test]
    fn lyrics_tiny_player_keeps_the_player_fallback_when_lyric_budget_is_too_small()
    -> Result<(), Box<dyn Error>> {
        let state = playing_test_state(lyrics_test_state(Some(timed_test_document()), 1_200));
        let mut terminal = Terminal::new(TestBackend::new(49, 17))?;
        terminal.draw(|frame| {
            render_with_model(frame, &state, &Theme::default(), &RenderModel::default());
        })?;

        let rendered = terminal.backend().to_string();
        let player = rendered.lines().last().unwrap_or_default();
        assert!(!player.contains("current"), "{player}");
        assert!(player.contains("❚❚"), "{player}");
        assert!(player.contains("0:01/3:00"), "{player}");
        for required in ["[p]", "[←]", "[Spc]", "[→]", "[n]"] {
            assert!(player.contains(required), "missing `{required}`: {player}");
        }

        let narrow = render_state_for_test(&state, 20, 8)?;
        let narrow_player = narrow.lines().last().unwrap_or_default();
        assert!(!narrow_player.contains("current"), "{narrow_player}");
        assert!(narrow_player.contains("❚❚"), "{narrow_player}");
        assert!(usize::from(narrow_player.trim_matches('"').cell_width()) <= 20);
        Ok(())
    }

    #[test]
    fn lyrics_tiny_player_uses_paused_and_seek_positions_and_normalized_text()
    -> Result<(), Box<dyn Error>> {
        let normalized = LyricsDocument::new(
            LyricsSource::Lrclib,
            None,
            vec![
                TimedLyricLine::new(0, Some(1_000), "old")?,
                TimedLyricLine::new(1_000, Some(2_000), "safe\r\n\t\u{1b}\u{7}lyric")?,
                TimedLyricLine::new(2_000, None, "sought")?,
            ],
            false,
        )?;
        let playing = playing_test_state(lyrics_test_state(Some(normalized), 1_200));
        let generation = playing
            .current_attempt_generation()
            .ok_or("tiny paused fixture generation")?;
        let paused = reduce(
            playing,
            Action::PlayerStatusChanged {
                generation,
                status: PlaybackStatus::Paused,
            },
        )
        .0;
        let paused_render = render_state_for_test(&paused, 59, 17)?;
        assert!(paused_render.contains("safe  lyric"), "{paused_render}");
        assert!(
            !paused_render.chars().any(|character| {
                character.is_control() && character != '\n' && character != '\r'
            })
        );

        let media_id = paused
            .playback()
            .current
            .clone()
            .ok_or("tiny seek fixture media")?;
        let sought = reduce(
            paused,
            Action::PlayerProgress {
                generation,
                media_id,
                position_ms: 2_100,
                duration_ms: Some(180_000),
            },
        )
        .0;
        let sought_render = render_state_for_test(&sought, 59, 17)?;
        assert!(sought_render.contains("sought"), "{sought_render}");
        assert!(!sought_render.contains("safe  lyric"), "{sought_render}");
        Ok(())
    }

    #[test]
    fn lyrics_plain_text_is_overlay_only_and_statuses_are_safe() {
        let theme = Theme::default();
        let plain = LyricsDocument::new(
            LyricsSource::YouTubeMusic,
            Some("plain first\nplain second".to_owned()),
            Vec::new(),
            false,
        )
        .unwrap_or_else(|error| panic!("plain lyrics fixture: {error}"));
        let plain_state = lyrics_test_state(Some(plain), 0);
        assert!(automatic_lyrics_lines(&plain_state, 3, 80, &theme).is_empty());
        let model = LyricsOverlayState::default();
        assert_eq!(
            line_text(&lyrics_overlay_lines(&plain_state, &model, 4, 20, &theme)),
            "Source: YouTube Music\nplain first\nplain second"
        );

        let instrumental = LyricsDocument::new(LyricsSource::Lrclib, None, Vec::new(), true)
            .unwrap_or_else(|error| panic!("instrumental fixture: {error}"));
        assert_eq!(
            line_text(&lyrics_overlay_lines(
                &lyrics_test_state(Some(instrumental), 0),
                &model,
                3,
                40,
                &theme
            )),
            "Source: LRCLIB\nInstrumental"
        );
        assert_eq!(
            line_text(&lyrics_overlay_lines(
                &lyrics_loading_state(),
                &model,
                3,
                40,
                &theme
            )),
            "⠋ Loading lyrics…"
        );
        assert_eq!(
            line_text(&lyrics_overlay_lines(
                &lyrics_test_state(None, 0),
                &model,
                3,
                40,
                &theme
            )),
            "Lyrics unavailable"
        );

        let manual = LyricsOverlayState {
            follow_active: false,
            selected_line: Some(1),
            scroll: 1,
            media_key: None,
            plain_max_scroll: 1,
        };
        assert_eq!(
            line_text(&lyrics_overlay_lines(&plain_state, &manual, 2, 20, &theme)),
            "Source: YouTube Music\nplain second"
        );

        let wrapped = LyricsDocument::new(
            LyricsSource::YouTubeMusic,
            Some("abcdefghij".to_owned()),
            Vec::new(),
            false,
        )
        .unwrap_or_else(|error| panic!("wrapped plain lyrics fixture: {error}"));
        assert_eq!(
            line_text(&lyrics_overlay_lines(
                &lyrics_test_state(Some(wrapped), 0),
                &manual,
                2,
                5,
                &theme
            )),
            "Source: YouTube Music\nfghij"
        );
    }

    #[test]
    fn lyric_wrapping_sanitizes_controls_and_keeps_row_count_exact() {
        let text = "a\r\nb\rc\td\x1b[31mE\x07🙂\u{0301}אב\u{0085}z";

        for width in [0, 1, 2, 4, 80] {
            let lines = wrap_lyrics_text(text, 0, usize::MAX, width);
            assert_eq!(
                wrapped_lyrics_row_count(text, width),
                lines.len(),
                "row count diverged from wrapping at width {width}"
            );
            let rendered = line_text(&lines);
            assert!(
                rendered
                    .chars()
                    .all(|character| character == '\n' || !character.is_control())
            );
        }

        assert_eq!(
            line_text(&wrap_lyrics_text(text, 0, usize::MAX, 80)),
            "a\nb\nc d[31mE🙂\u{0301}אבz"
        );
    }

    proptest! {
        #[test]
        fn lyric_wrapping_never_measures_controls_and_count_matches_output(
            controls in prop::collection::vec(0_u8..=0x9f, 0..64),
            width in 0_usize..=12,
        ) {
            let mut text = String::from("é\u{0301}🙂אב");
            text.extend(controls.into_iter().map(char::from));
            text.push_str("終わり");

            let lines = wrap_lyrics_text(&text, 0, usize::MAX, width);
            prop_assert_eq!(wrapped_lyrics_row_count(&text, width), lines.len());
            for line in lines {
                let rendered = line_text_ref(&line);
                prop_assert!(!rendered.chars().any(char::is_control));
            }
        }
    }

    #[test]
    fn lyrics_overlay_attributes_both_sources_without_leaking_into_player() {
        let theme = Theme::default();
        for (source, expected) in [
            (LyricsSource::YouTubeMusic, "Source: YouTube Music"),
            (LyricsSource::Lrclib, "Source: LRCLIB"),
        ] {
            let document = LyricsDocument::new(
                source,
                Some("attributed lyric".to_owned()),
                Vec::new(),
                false,
            )
            .unwrap_or_else(|error| panic!("attribution fixture: {error}"));
            let state = lyrics_test_state(Some(document), 0);
            let overlay =
                lyrics_overlay_lines(&state, &LyricsOverlayState::default(), 2, 24, &theme);
            assert_eq!(line_text(&overlay), format!("{expected}\nattributed lyric"));
            assert!(automatic_lyrics_lines(&state, 3, 80, &theme).is_empty());
        }

        assert_eq!(
            lyrics_overlay_viewport(Rect::new(0, 0, 20, 6)),
            Some((1, 16)),
            "one of the two inner rows is reserved for source attribution"
        );
    }

    #[test]
    fn lyrics_manual_viewport_stays_stable_and_follow_recenters_after_resize() {
        let theme = Theme::default();
        let mut manual = LyricsOverlayState {
            follow_active: false,
            selected_line: Some(1),
            scroll: 1,
            media_key: None,
            plain_max_scroll: 0,
        };
        let early = lyrics_test_state(Some(long_timed_test_document()), 1_500);
        let late = lyrics_test_state(Some(long_timed_test_document()), 7_500);
        let early_lines = lyrics_overlay_lines(&early, &manual, 3, 30, &theme);
        let late_lines = lyrics_overlay_lines(&late, &manual, 3, 30, &theme);
        assert_eq!(line_text(&early_lines), line_text(&late_lines));

        manual.follow_active = true;
        let recentered = lyrics_overlay_lines(&late, &manual, 3, 30, &theme);
        assert!(line_text(&recentered).contains("line-7"));
        let resized = lyrics_overlay_lines(&late, &manual, 2, 12, &theme);
        assert_eq!(line_text(&resized), "Source: LRCLIB\n▶ line-7");
    }

    #[test]
    fn selection_motion_glides_across_manual_timed_lyrics_rows() {
        let theme = Theme::default();
        let state = lyrics_test_state(Some(long_timed_test_document()), 1_500);
        let mut overlay = LyricsOverlayState {
            follow_active: false,
            selected_line: Some(1),
            scroll: 1,
            media_key: None,
            plain_max_scroll: 0,
        };
        let mut memory = ViewportMemory::default();
        memory.begin_selection_frame(Rect::new(0, 0, 80, 24), 0);
        let _ = lyrics_overlay_lines_with_motion(
            &state,
            &overlay,
            5,
            30,
            &theme,
            0,
            Some(&mut memory),
            0,
        );
        memory.end_selection_frame();

        overlay.selected_line = Some(2);
        memory.begin_selection_frame(Rect::new(0, 0, 80, 24), 1);
        let moving = lyrics_overlay_lines_with_motion(
            &state,
            &overlay,
            5,
            30,
            &theme,
            0,
            Some(&mut memory),
            1,
        );
        memory.end_selection_frame();
        let moving = line_text(&moving);
        assert!(moving.contains("▶ line-1"), "{moving}");
        assert!(moving.contains("● line-2"), "{moving}");
    }

    #[test]
    fn lyrics_settings_report_each_enhancement_toggle() {
        let mut config = Config::default();
        config.lyrics.enabled = false;
        config.lyrics.external_sync = false;
        config.artwork.animated = false;
        let state = AppState::new(config);
        let rendered = line_text(&super::super::views::settings::lines(&state, 12, 80, 15));
        assert!(rendered.contains("Lyrics: off"), "{rendered}");
        assert!(rendered.contains("External sync: off"), "{rendered}");
        assert!(rendered.contains("Animated artwork: off"), "{rendered}");
    }

    #[test]
    fn lyrics_overlay_debug_redacts_media_fingerprint() {
        let state = LyricsOverlayState {
            media_key: Some(42),
            ..LyricsOverlayState::default()
        };
        let debug = format!("{state:?}");
        assert!(!debug.contains("media_key"));
        assert!(!debug.contains("42"));
        assert!(debug.contains("has_media: true"));
    }

    #[test]
    fn selection_viewport_keeps_rows_stationary_until_selection_crosses_an_edge() {
        let key = 1;
        let mut v = SelectionViewport::default();

        assert_eq!(v.visible_range(20, Some(10), 5, key), 6..11);
        assert_eq!(v.visible_range(20, Some(9), 5, key), 6..11);
        assert_eq!(v.visible_range(20, Some(5), 5, key), 5..10);
    }

    #[test]
    fn selection_viewport_scrolls_only_after_crossing_an_edge() {
        let mut viewport = SelectionViewport::default();

        assert_eq!(viewport.visible_range(20, Some(4), 5, 1), 0..5);
        assert_eq!(viewport.visible_range(20, Some(3), 5, 1), 0..5);
        assert_eq!(viewport.visible_range(20, Some(5), 5, 1), 1..6);
    }

    #[test]
    fn selection_viewport_reveals_wrapped_selections() {
        let mut viewport = SelectionViewport::default();

        assert_eq!(viewport.visible_range(20, Some(19), 5, 1), 15..20);
        assert_eq!(viewport.visible_range(20, Some(0), 5, 1), 0..5);
        assert_eq!(viewport.visible_range(20, Some(19), 5, 1), 15..20);
    }

    #[test]
    fn selection_viewport_handles_empty_zero_and_invalid_selections() {
        let mut viewport = SelectionViewport::default();

        assert_eq!(viewport.visible_range(0, Some(0), 5, 1), 0..0);
        assert_eq!(viewport.visible_range(20, Some(10), 0, 1), 0..0);
        assert_eq!(viewport.visible_range(4, Some(99), 2, 1), 0..2);
        assert_eq!(viewport.visible_range(4, None, 2, 1), 0..2);
    }

    #[test]
    fn selection_viewport_resets_for_new_dataset_before_revealing_selection() {
        let mut viewport = SelectionViewport::default();

        assert_eq!(viewport.visible_range(20, Some(10), 5, 1), 6..11);
        assert_eq!(viewport.visible_range(20, Some(2), 5, 2), 0..5);
        assert_eq!(viewport.visible_range(20, Some(10), 5, 3), 6..11);
    }

    #[test]
    fn selection_viewport_clamps_shrinking_content_and_resized_windows() {
        let mut viewport = SelectionViewport::default();

        assert_eq!(viewport.visible_range(20, Some(15), 5, 1), 11..16);
        assert_eq!(viewport.visible_range(13, Some(12), 5, 1), 8..13);
        assert_eq!(viewport.visible_range(13, Some(12), 8, 1), 5..13);
        assert_eq!(viewport.visible_range(13, Some(12), 3, 1), 10..13);
        assert_eq!(viewport.visible_range(13, Some(10), 3, 1), 10..13);
    }

    #[test]
    fn favorites_viewport_keeps_local_offset_after_selected_row_removal() {
        let entries = indexed_media("favorite-offset", 20)
            .into_iter()
            .enumerate()
            .map(|(index, item)| FavoriteEntry {
                id: i64::try_from(index).unwrap_or_default(),
                item,
                favorited_at: i64::try_from(20usize.saturating_sub(index)).unwrap_or_default(),
            })
            .collect::<Vec<_>>();
        let initial = entries[10].item.id.clone();
        let selected = entries[9].item.clone();
        let remaining = entries
            .iter()
            .filter(|entry| entry.item.id != selected.id)
            .cloned()
            .collect::<Vec<_>>();
        let (state, effects) = reduce(AppState::default(), Action::FavoritesRequested);
        let [Effect::LoadFavorites { generation }] = effects.as_slice() else {
            panic!("favorites load effect");
        };
        let (state, _) = reduce(
            state,
            Action::FavoritesCompleted {
                generation: *generation,
                result: Ok(entries),
            },
        );
        let (state, _) = reduce(
            state,
            Action::FavoriteSelectionChanged { media_id: initial },
        );
        let mut viewport = SelectionViewport::default();
        let _ = super::super::views::favorites::lines_with_viewport_and_targets(
            &state,
            7,
            48,
            &mut viewport,
            None,
        );
        let (state, _) = reduce(
            state,
            Action::FavoriteSelectionChanged {
                media_id: selected.id.clone(),
            },
        );
        let before = line_text(
            &super::super::views::favorites::lines_with_viewport_and_targets(
                &state,
                7,
                48,
                &mut viewport,
                None,
            ),
        );
        let (state, effects) = reduce(state, Action::FavoriteToggleRequested { item: selected });
        let [
            Effect::RemoveFavorite {
                generation,
                media_id,
            },
        ] = effects.as_slice()
        else {
            panic!("remove favorite effect");
        };
        let (state, _) = reduce(
            state,
            Action::FavoriteMutationCompleted {
                generation: *generation,
                media_id: media_id.clone(),
                mutation: FavoriteMutation::Remove,
                result: Ok(remaining),
            },
        );
        let after = line_text(
            &super::super::views::favorites::lines_with_viewport_and_targets(
                &state,
                7,
                48,
                &mut viewport,
                None,
            ),
        );

        assert!(before.contains("Song favorite-offset-5"), "{before}");
        assert!(after.contains("Song favorite-offset-5"), "{after}");
        assert!(!after.contains("Song favorite-offset-4"), "{after}");
        assert!(after.contains("▶ Song favorite-offset-10"), "{after}");
    }

    #[test]
    fn ordered_viewport_preserves_top_identity_when_a_prior_row_is_removed() {
        let previous = (0_u64..20).collect::<Vec<_>>();
        let next = previous
            .iter()
            .copied()
            .filter(|identity| *identity != 2)
            .collect::<Vec<_>>();
        let mut viewport = SelectionViewport::default();
        let previous_key = ordered_dataset_key(
            &"anchor-removal",
            &1_u64,
            &previous,
            DatasetUpdate::Reconcile,
        );
        assert_eq!(
            viewport.visible_range(previous.len(), Some(10), 5, previous_key.clone()),
            6..11
        );
        assert_eq!(
            viewport.visible_range(previous.len(), Some(9), 5, previous_key),
            6..11
        );
        let next_key =
            ordered_dataset_key(&"anchor-removal", &2_u64, &next, DatasetUpdate::Reconcile);

        assert_eq!(
            viewport.visible_range(next.len(), Some(8), 5, next_key),
            5..10
        );
        assert_eq!(
            next[5], 6,
            "the prior top-visible identity must remain anchored"
        );
    }

    #[test]
    fn ordered_viewport_preserves_selected_offset_across_prepend_and_reorder() {
        let previous = (0_u64..20).collect::<Vec<_>>();
        let mut viewport = SelectionViewport::default();
        let previous_key = ordered_dataset_key(
            &"anchor-reconcile",
            &1_u64,
            &previous,
            DatasetUpdate::Reconcile,
        );
        assert_eq!(
            viewport.visible_range(previous.len(), Some(10), 5, previous_key.clone()),
            6..11
        );
        assert_eq!(
            viewport.visible_range(previous.len(), Some(9), 5, previous_key),
            6..11
        );

        let prepended = [98_u64, 99]
            .into_iter()
            .chain(previous.iter().copied())
            .collect::<Vec<_>>();
        let prepended_key = ordered_dataset_key(
            &"anchor-reconcile",
            &2_u64,
            &prepended,
            DatasetUpdate::Reconcile,
        );
        assert_eq!(
            viewport.visible_range(prepended.len(), Some(11), 5, prepended_key),
            8..13
        );
        assert_eq!(prepended[8], 6);

        let reordered = (5_u64..20).chain(0..5).collect::<Vec<_>>();
        let reordered_key = ordered_dataset_key(
            &"anchor-reconcile",
            &3_u64,
            &reordered,
            DatasetUpdate::Reconcile,
        );
        assert_eq!(
            viewport.visible_range(reordered.len(), Some(4), 5, reordered_key),
            1..6
        );
        assert_eq!(reordered[1], 6);
    }

    #[test]
    fn ordered_viewport_keeps_nearest_replacement_at_removed_selection_offset() {
        let previous = (0_u64..20).collect::<Vec<_>>();
        let next = previous
            .iter()
            .copied()
            .filter(|identity| *identity != 6)
            .collect::<Vec<_>>();
        let mut viewport = SelectionViewport::default();
        let previous_key = ordered_dataset_key(
            &"selected-anchor-removal",
            &1_u64,
            &previous,
            DatasetUpdate::Reconcile,
        );
        assert_eq!(
            viewport.visible_range(previous.len(), Some(10), 5, previous_key.clone()),
            6..11
        );
        assert_eq!(
            viewport.visible_range(previous.len(), Some(6), 5, previous_key),
            6..11
        );
        let next_key = ordered_dataset_key(
            &"selected-anchor-removal",
            &2_u64,
            &next,
            DatasetUpdate::Reconcile,
        );

        assert_eq!(
            viewport.visible_range(next.len(), Some(6), 5, next_key),
            6..11
        );
        assert_eq!(next[6], 7);
    }

    #[test]
    fn ordered_viewport_clamps_removed_selected_top_anchor_at_boundaries() {
        for (previous, removed, selected, max_rows, expected) in [
            ((0_u64..5).collect::<Vec<_>>(), 0, 0, 3, 0..3),
            ((0_u64..5).collect::<Vec<_>>(), 4, 3, 1, 3..4),
        ] {
            let next = previous
                .iter()
                .copied()
                .filter(|identity| *identity != removed)
                .collect::<Vec<_>>();
            let mut viewport = SelectionViewport::default();
            let previous_key = ordered_dataset_key(
                &"selected-anchor-boundary",
                &removed,
                &previous,
                DatasetUpdate::Reconcile,
            );
            let removed_index = previous
                .iter()
                .position(|identity| *identity == removed)
                .unwrap_or_else(|| panic!("removed identity fixture"));
            assert!(
                viewport
                    .visible_range(previous.len(), Some(removed_index), max_rows, previous_key)
                    .contains(&removed_index)
            );
            let next_key = ordered_dataset_key(
                &"selected-anchor-boundary",
                &removed.wrapping_add(10),
                &next,
                DatasetUpdate::Reconcile,
            );

            assert_eq!(
                viewport.visible_range(next.len(), Some(selected), max_rows, next_key),
                expected
            );
        }
    }

    #[test]
    fn ordered_viewport_resize_keeps_selection_visible_and_range_bounded() {
        let identities = (0_u64..20).collect::<Vec<_>>();
        let key = ordered_dataset_key(
            &"anchor-resize",
            &1_u64,
            &identities,
            DatasetUpdate::Reconcile,
        );
        let mut viewport = SelectionViewport::default();
        assert_eq!(
            viewport.visible_range(identities.len(), Some(10), 5, key.clone()),
            6..11
        );
        let smaller = viewport.visible_range(identities.len(), Some(10), 3, key.clone());
        assert!(smaller.contains(&10));
        assert_eq!(smaller.len(), 3);
        assert!(smaller.end <= identities.len());
        let larger = viewport.visible_range(identities.len(), Some(10), 8, key);
        assert!(larger.contains(&10));
        assert!(larger.len() <= 8);
        assert!(larger.end <= identities.len());
    }

    #[test]
    fn selection_viewport_preserves_offset_through_transient_zero_rows() {
        let mut viewport = SelectionViewport::default();

        assert_eq!(viewport.visible_range(20, Some(10), 5, 1), 6..11);
        assert_eq!(viewport.visible_range(20, Some(9), 5, 1), 6..11);
        assert_eq!(viewport.visible_range(20, Some(9), 0, 1), 0..0);
        assert_eq!(viewport.visible_range(20, Some(9), 5, 1), 6..11);
    }

    #[test]
    fn selection_viewport_applies_dataset_reset_during_zero_rows() {
        let mut viewport = SelectionViewport::default();

        assert_eq!(viewport.visible_range(20, Some(10), 5, 1), 6..11);
        assert_eq!(viewport.visible_range(20, Some(9), 0, 2), 0..0);
        assert_eq!(viewport.visible_range(20, Some(9), 5, 2), 5..10);
    }

    #[test]
    fn tiny_terminal_budget_keeps_deep_search_selection_visible() -> Result<(), Box<dyn Error>> {
        let items = indexed_media("tiny-visible", 20);
        let selected = SearchItem::Playable(items[19].clone()).stable_id();
        let (state, _) = reduce(
            AppState::default(),
            Action::SearchSubmitted {
                query: "tiny".to_owned(),
                filter: SearchFilter::Songs,
            },
        );
        let generation = state.search().generation();
        let (state, _) = reduce(
            state,
            Action::SearchCompleted {
                generation,
                result: Ok(SearchPage::new(
                    items.into_iter().map(SearchItem::Playable).collect(),
                )),
            },
        );
        let (state, _) = reduce(state, Action::SearchSelectionChanged { id: selected });
        let model = RenderModel::default().with_view(NavigationItem::Search);
        let mut terminal = Terminal::new(TestBackend::new(40, 10))?;

        terminal.draw(|frame| render_with_model(frame, &state, &Theme::default(), &model))?;
        let rendered = terminal.backend().to_string();

        assert!(rendered.contains("▶ Song tiny-visible-19"), "{rendered}");
        Ok(())
    }

    #[test]
    fn tiny_terminal_multiframe_library_and_history_keep_independent_pages()
    -> Result<(), Box<dyn Error>> {
        let (library, library_moved) = tiny_library_frames();
        let (history, history_moved) = tiny_history_frames();

        let mut terminal = Terminal::new(TestBackend::new(40, 10))?;
        let theme = Theme::default();
        let mut viewports = ViewportMemory::default();
        for (view, initial, moved, visible, hidden) in [
            (
                NavigationItem::Library,
                &library,
                &library_moved,
                "Song tiny-library-5",
                "Song tiny-library-4",
            ),
            (
                NavigationItem::History,
                &history,
                &history_moved,
                "Song tiny-history-5",
                "Song tiny-history-4",
            ),
        ] {
            let model = RenderModel::default().with_view(view);
            terminal.draw(|frame| {
                render_with_model_and_viewports(
                    frame,
                    initial,
                    &theme,
                    &model,
                    RenderEnhancements::new(None, None, 15),
                    &mut viewports,
                );
            })?;
            terminal.draw(|frame| {
                render_with_model_and_viewports(
                    frame,
                    moved,
                    &theme,
                    &model,
                    RenderEnhancements::new(None, None, 15),
                    &mut viewports,
                );
            })?;
            let rendered = terminal.backend().to_string();
            assert!(rendered.contains(visible), "{rendered}");
            assert!(!rendered.contains(hidden), "{rendered}");
        }
        Ok(())
    }

    fn tiny_library_frames() -> (AppState, AppState) {
        let items = indexed_media("tiny-library", 12)
            .into_iter()
            .map(LibraryItem::Playable)
            .collect::<Vec<_>>();
        let selected_10 = stable_library_item_id(&items[10]);
        let selected_9 = stable_library_item_id(&items[9]);
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
            panic!("library load effect");
        };
        let (state, _) = reduce(
            state,
            Action::LibraryCompleted {
                generation: *generation,
                result: Ok(Page {
                    items,
                    continuation: None,
                    stale: false,
                }),
            },
        );
        let (state, _) = reduce(state, Action::LibrarySelectionChanged { id: selected_10 });
        let (moved, _) = reduce(
            state.clone(),
            Action::LibrarySelectionChanged { id: selected_9 },
        );
        (state, moved)
    }

    fn tiny_history_frames() -> (AppState, AppState) {
        let entries = indexed_media("tiny-history", 12)
            .into_iter()
            .enumerate()
            .map(|(index, item)| HistoryEntry {
                id: i64::try_from(index).unwrap_or_else(|_| panic!("small history fixture")),
                item,
                played_at: 1_000
                    + i64::try_from(index).unwrap_or_else(|_| panic!("small history fixture")),
            })
            .collect::<Vec<_>>();
        let (state, effects) = reduce(AppState::default(), Action::HistoryRequested);
        let [Effect::LoadHistory { generation, .. }] = effects.as_slice() else {
            panic!("history load effect");
        };
        let (state, _) = reduce(
            state,
            Action::HistoryCompleted {
                generation: *generation,
                result: Ok(entries),
            },
        );
        let (state, _) = reduce(state, Action::HistorySelectionChanged { id: 10 });
        let (moved, _) = reduce(state.clone(), Action::HistorySelectionChanged { id: 9 });
        (state, moved)
    }

    #[test]
    fn viewport_memory_keeps_every_long_list_independent() {
        let mut memory = ViewportMemory::default();

        macro_rules! assert_persistent_slot {
            ($slot:expr) => {{
                assert_eq!($slot.visible_range(20, Some(10), 5, 7), 6..11);
                assert_eq!($slot.visible_range(20, Some(9), 5, 7), 6..11);
            }};
        }

        assert_persistent_slot!(&mut memory.search);
        assert_persistent_slot!(&mut memory.charts);
        assert_persistent_slot!(&mut memory.podcast_recommendations);
        assert_persistent_slot!(&mut memory.podcast_episodes);
        assert_persistent_slot!(&mut memory.library);
        assert_persistent_slot!(&mut memory.favorites);
        assert_persistent_slot!(&mut memory.history);
        assert_persistent_slot!(&mut memory.queue);
        assert_persistent_slot!(&mut memory.country_picker);
        assert_persistent_slot!(&mut memory.browser_picker);

        assert_eq!(memory.search.visible_range(20, Some(5), 5, 7), 5..10);
        assert_eq!(memory.charts.visible_range(20, Some(9), 5, 7), 6..11);
        assert_eq!(
            memory.country_picker.visible_range(20, Some(9), 5, 7),
            6..11
        );
    }

    #[test]
    fn search_rows_stay_stationary_across_rendered_selection_frames() {
        let items = indexed_media("stable-search", 20);
        let ids = items
            .iter()
            .cloned()
            .map(SearchItem::Playable)
            .map(|item| item.stable_id())
            .collect::<Vec<_>>();
        let (state, _) = reduce(
            AppState::default(),
            Action::SearchSubmitted {
                query: "stable viewport".to_owned(),
                filter: SearchFilter::Songs,
            },
        );
        let generation = state.search().generation();
        let (state, _) = reduce(
            state,
            Action::SearchCompleted {
                generation,
                result: Ok(SearchPage::new(
                    items.into_iter().map(SearchItem::Playable).collect(),
                )),
            },
        );
        let mut viewport = SelectionViewport::default();

        let (state, _) = reduce(
            state,
            Action::SearchSelectionChanged {
                id: ids[10].clone(),
            },
        );
        let first = line_text(&super::super::views::search::lines_with_viewport(
            &state,
            7,
            48,
            None,
            &mut viewport,
        ));
        let (state, _) = reduce(state, Action::SearchSelectionChanged { id: ids[9].clone() });
        let second = line_text(&super::super::views::search::lines_with_viewport(
            &state,
            7,
            48,
            None,
            &mut viewport,
        ));
        let (state, _) = reduce(state, Action::SearchSelectionChanged { id: ids[5].clone() });
        let third = line_text(&super::super::views::search::lines_with_viewport(
            &state,
            7,
            48,
            None,
            &mut viewport,
        ));

        assert!(first.contains("Song stable-search-6"));
        assert!(second.contains("Song stable-search-6"));
        assert!(!second.contains("Song stable-search-5"));
        assert!(third.contains("Song stable-search-5"));
    }

    #[test]
    fn search_viewport_stays_stationary_while_pagination_extends_the_dataset() {
        let items = indexed_media("paged-search", 12);
        let ids = items
            .iter()
            .cloned()
            .map(SearchItem::Playable)
            .map(|item| item.stable_id())
            .collect::<Vec<_>>();
        let (state, _) = reduce(
            AppState::default(),
            Action::SearchSubmitted {
                query: "pagination".to_owned(),
                filter: SearchFilter::Songs,
            },
        );
        let generation = state.search().generation();
        let (state, _) = reduce(
            state,
            Action::SearchCompleted {
                generation,
                result: Ok(
                    SearchPage::new(items.into_iter().map(SearchItem::Playable).collect())
                        .with_continuation("next"),
                ),
            },
        );
        let (state, _) = reduce(
            state,
            Action::SearchSelectionChanged {
                id: ids[10].clone(),
            },
        );
        let mut viewport = SelectionViewport::default();
        let _ =
            super::super::views::search::lines_with_viewport(&state, 7, 48, None, &mut viewport);
        let (state, _) = reduce(state, Action::SearchSelectionChanged { id: ids[9].clone() });
        let before = line_text(&super::super::views::search::lines_with_viewport(
            &state,
            7,
            48,
            None,
            &mut viewport,
        ));
        let (state, effects) = reduce(state, Action::SearchMoreRequested);
        let [Effect::SearchMore { generation, .. }] = effects.as_slice() else {
            panic!("search continuation effect");
        };
        let loading = line_text(&super::super::views::search::lines_with_viewport(
            &state,
            7,
            48,
            None,
            &mut viewport,
        ));
        let (state, _) = reduce(
            state,
            Action::SearchCompleted {
                generation: *generation,
                result: Ok(SearchPage::new(
                    indexed_media("paged-search-more", 2)
                        .into_iter()
                        .map(SearchItem::Playable)
                        .collect(),
                )),
            },
        );
        let appended = line_text(&super::super::views::search::lines_with_viewport(
            &state,
            7,
            48,
            None,
            &mut viewport,
        ));

        for rendered in [&before, &loading, &appended] {
            assert!(rendered.contains("Song paged-search-6"), "{rendered}");
            assert!(!rendered.contains("Song paged-search-5"), "{rendered}");
        }
    }

    #[test]
    fn library_viewport_stays_stationary_while_pagination_extends_the_dataset() {
        let items = indexed_media("paged-library", 12)
            .into_iter()
            .map(LibraryItem::Playable)
            .collect::<Vec<_>>();
        let selected_10 = stable_library_item_id(&items[10]);
        let selected_9 = stable_library_item_id(&items[9]);
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
            panic!("library load effect");
        };
        let (state, _) = reduce(
            state,
            Action::LibraryCompleted {
                generation: *generation,
                result: Ok(Page {
                    items,
                    continuation: Some("next".to_owned()),
                    stale: false,
                }),
            },
        );
        let (state, _) = reduce(state, Action::LibrarySelectionChanged { id: selected_10 });
        let mut viewport = SelectionViewport::default();
        let _ = super::super::views::library::lines_with_viewport(&state, 5, 48, &mut viewport);
        let (state, _) = reduce(state, Action::LibrarySelectionChanged { id: selected_9 });
        let before = line_text(&super::super::views::library::lines_with_viewport(
            &state,
            5,
            48,
            &mut viewport,
        ));
        let (state, effects) = reduce(state, Action::LibraryMoreRequested);
        let [Effect::LoadLibrary { generation, .. }] = effects.as_slice() else {
            panic!("library continuation effect");
        };
        let loading = line_text(&super::super::views::library::lines_with_viewport(
            &state,
            5,
            48,
            &mut viewport,
        ));
        let (state, _) = reduce(
            state,
            Action::LibraryCompleted {
                generation: *generation,
                result: Ok(Page {
                    items: indexed_media("paged-library-more", 2)
                        .into_iter()
                        .map(LibraryItem::Playable)
                        .collect(),
                    continuation: None,
                    stale: false,
                }),
            },
        );
        let appended = line_text(&super::super::views::library::lines_with_viewport(
            &state,
            5,
            48,
            &mut viewport,
        ));

        for rendered in [&before, &loading, &appended] {
            assert!(rendered.contains("Song paged-library-7"), "{rendered}");
            assert!(!rendered.contains("Song paged-library-6"), "{rendered}");
        }
    }

    #[test]
    fn search_replacement_with_shared_first_id_resets_before_revealing_selection() {
        let items = indexed_media("replace-search", 12);
        let ids = items
            .iter()
            .cloned()
            .map(SearchItem::Playable)
            .map(|item| item.stable_id())
            .collect::<Vec<_>>();
        let mut replacement = items.clone();
        replacement[1] = indexed_media("different-tail", 1).remove(0);
        let (state, _) = reduce(
            AppState::default(),
            Action::SearchSubmitted {
                query: "first".to_owned(),
                filter: SearchFilter::Songs,
            },
        );
        let generation = state.search().generation();
        let (state, _) = reduce(
            state,
            Action::SearchCompleted {
                generation,
                result: Ok(SearchPage::new(
                    items.into_iter().map(SearchItem::Playable).collect(),
                )),
            },
        );
        let (state, _) = reduce(
            state,
            Action::SearchSelectionChanged {
                id: ids[10].clone(),
            },
        );
        let mut viewport = SelectionViewport::default();
        let _ =
            super::super::views::search::lines_with_viewport(&state, 7, 48, None, &mut viewport);
        let (state, _) = reduce(state, Action::SearchSelectionChanged { id: ids[9].clone() });
        let _ =
            super::super::views::search::lines_with_viewport(&state, 7, 48, None, &mut viewport);
        let (state, _) = reduce(
            state,
            Action::SearchSubmitted {
                query: "replacement".to_owned(),
                filter: SearchFilter::Songs,
            },
        );
        let generation = state.search().generation();
        let (state, _) = reduce(
            state,
            Action::SearchCompleted {
                generation,
                result: Ok(SearchPage::new(
                    replacement.into_iter().map(SearchItem::Playable).collect(),
                )),
            },
        );
        let lines =
            super::super::views::search::lines_with_viewport(&state, 7, 48, None, &mut viewport);
        let first_row = line_text(&lines[2..3]);

        assert!(first_row.contains("Song replace-search-5"), "{first_row}");
    }

    #[test]
    fn providerless_metadata_replacement_resets_without_title_identity() {
        let media = indexed_media("metadata-search", 12);
        let selected_10 = SearchItem::Playable(media[10].clone()).stable_id();
        let selected_9 = SearchItem::Playable(media[9].clone()).stable_id();
        let mut first = media
            .iter()
            .cloned()
            .map(SearchItem::Playable)
            .collect::<Vec<_>>();
        first[1] = SearchItem::Metadata(SearchMetadata::new(
            SearchMetadataKind::Podcast,
            "external first title",
        ));
        let mut replacement = first.clone();
        replacement[1] = SearchItem::Metadata(SearchMetadata::new(
            SearchMetadataKind::Podcast,
            "external renamed title",
        ));
        let (state, _) = reduce(
            AppState::default(),
            Action::SearchSubmitted {
                query: "metadata first".to_owned(),
                filter: SearchFilter::All,
            },
        );
        let generation = state.search().generation();
        let (state, _) = reduce(
            state,
            Action::SearchCompleted {
                generation,
                result: Ok(SearchPage::new(first)),
            },
        );
        let (state, _) = reduce(state, Action::SearchSelectionChanged { id: selected_10 });
        let mut viewport = SelectionViewport::default();
        let _ =
            super::super::views::search::lines_with_viewport(&state, 7, 48, None, &mut viewport);
        let (state, _) = reduce(
            state,
            Action::SearchSelectionChanged {
                id: selected_9.clone(),
            },
        );
        let _ =
            super::super::views::search::lines_with_viewport(&state, 7, 48, None, &mut viewport);
        let (state, _) = reduce(
            state,
            Action::SearchSubmitted {
                query: "metadata replacement".to_owned(),
                filter: SearchFilter::All,
            },
        );
        let generation = state.search().generation();
        let (state, _) = reduce(
            state,
            Action::SearchCompleted {
                generation,
                result: Ok(SearchPage::new(replacement)),
            },
        );
        let lines =
            super::super::views::search::lines_with_viewport(&state, 7, 48, None, &mut viewport);
        let first_row = line_text(&lines[2..3]);

        assert!(first_row.contains("Song metadata-search-5"), "{first_row}");
        assert_eq!(state.search().selected_id(), Some(&selected_9));
    }

    #[test]
    fn library_replacement_with_shared_first_id_resets_before_revealing_selection() {
        let items = indexed_media("replace-library", 12)
            .into_iter()
            .map(LibraryItem::Playable)
            .collect::<Vec<_>>();
        let selected_10 = stable_library_item_id(&items[10]);
        let selected_9 = stable_library_item_id(&items[9]);
        let mut replacement = items.clone();
        replacement[1] =
            LibraryItem::Playable(indexed_media("different-library-tail", 1).remove(0));
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
            panic!("library load effect");
        };
        let (state, _) = reduce(
            state,
            Action::LibraryCompleted {
                generation: *generation,
                result: Ok(Page {
                    items,
                    continuation: None,
                    stale: false,
                }),
            },
        );
        let (state, _) = reduce(state, Action::LibrarySelectionChanged { id: selected_10 });
        let mut viewport = SelectionViewport::default();
        let _ = super::super::views::library::lines_with_viewport(&state, 5, 48, &mut viewport);
        let (state, _) = reduce(
            state,
            Action::LibrarySelectionChanged {
                id: selected_9.clone(),
            },
        );
        let _ = super::super::views::library::lines_with_viewport(&state, 5, 48, &mut viewport);
        let (state, effects) = reduce(
            state,
            Action::LibraryRequested {
                section: LibrarySection::Songs,
            },
        );
        let [Effect::LoadLibrary { generation, .. }] = effects.as_slice() else {
            panic!("replacement library load effect");
        };
        let (state, _) = reduce(
            state,
            Action::LibraryCompleted {
                generation: *generation,
                result: Ok(Page {
                    items: replacement,
                    continuation: None,
                    stale: false,
                }),
            },
        );
        let lines = super::super::views::library::lines_with_viewport(&state, 5, 48, &mut viewport);
        let first_row = line_text(&lines[1..2]);

        assert!(first_row.contains("Song replace-library-6"), "{first_row}");
        assert_eq!(state.library().selected_id(), Some(&selected_9));
    }

    #[test]
    fn wide_queue_pane_renders_seeded_active_order() -> Result<(), Box<dyn Error>> {
        assert_seeded_active_queue_order(34, 35)
    }

    #[test]
    fn compact_queue_pane_renders_seeded_active_order() -> Result<(), Box<dyn Error>> {
        assert_seeded_active_queue_order(90, 23)
    }

    #[test]
    fn content_lines_bound_dynamic_fields_before_composition() -> Result<(), Box<dyn Error>> {
        let huge = "界".repeat(700_000);
        let width = 40;

        let (state, _) = reduce(
            AppState::default(),
            Action::SearchSubmitted {
                query: huge.clone(),
                filter: SearchFilter::All,
            },
        );
        let generation = state.search().generation();
        let (state, _) = reduce(
            state,
            Action::SearchCompleted {
                generation,
                result: Ok(SearchPage::new(vec![
                    SearchItem::Playable(MediaItem {
                        id: MediaId {
                            provider: "youtube-music".to_owned(),
                            video_id: "huge".to_owned(),
                        },
                        kind: MediaKind::Song,
                        title: huge.clone(),
                        creators: vec![huge.clone()],
                        collection: None,
                        duration_ms: None,
                        artwork_url: None,
                        explicit: false,
                    }),
                    SearchItem::Playable(MediaItem {
                        id: MediaId {
                            provider: "youtube-music".to_owned(),
                            video_id: "huge-creator".to_owned(),
                        },
                        kind: MediaKind::Song,
                        title: "Short title".to_owned(),
                        creators: vec![huge.clone()],
                        collection: None,
                        duration_ms: None,
                        artwork_url: None,
                        explicit: false,
                    }),
                    SearchItem::Metadata(SearchMetadata::new(
                        SearchMetadataKind::Album,
                        huge.clone(),
                    )),
                ])),
            },
        );
        let search_lines = content_lines(&state, NavigationItem::Search, 8, width, None);
        assert_precomposed_lines_are_bounded(&search_lines, width);
        assert!(search_lines[0].spans[0].content.ends_with('…'));
        assert!(search_lines[2].spans[0].content.ends_with('…'));
        assert!(search_lines[3].spans[0].content.ends_with('…'));
        assert!(search_lines[4].spans[0].content.ends_with('…'));

        let (error_state, _) = reduce(
            AppState::default(),
            Action::SearchSubmitted {
                query: "error".to_owned(),
                filter: SearchFilter::All,
            },
        );
        let generation = error_state.search().generation();
        let (error_state, _) = reduce(
            error_state,
            Action::SearchCompleted {
                generation,
                result: Err(AppError::new(AppErrorCategory::Search, huge.clone())),
            },
        );
        let error_lines = content_lines(&error_state, NavigationItem::Search, 4, width, None);
        assert_precomposed_lines_are_bounded(&error_lines, width);
        assert!(error_lines[2].spans[0].content.ends_with('…'));

        let region = RegionCode::parse("hk")?;
        let (chart_state, _) = reduce(
            AppState::default(),
            Action::ChartsRequested {
                region: region.clone(),
            },
        );
        let generation = chart_state.charts().generation();
        let (chart_state, _) = reduce(
            chart_state,
            Action::ChartsCompleted {
                generation,
                region,
                received_at: 1_000,
                result: Ok(vec![ChartSection::new(huge, Vec::new())]),
            },
        );
        let chart_lines = content_lines(&chart_state, NavigationItem::Charts, 4, width, None);
        assert_precomposed_lines_are_bounded(&chart_lines, width);
        assert!(chart_lines[1].spans[0].content.ends_with('…'));
        Ok(())
    }

    #[test]
    fn queue_player_and_palette_lines_bound_dynamic_fields_before_composition()
    -> Result<(), Box<dyn Error>> {
        let huge = "界".repeat(700_000);
        let width = 40;
        let item = QueueItem::new("huge", media_with_title(&huge));
        let queue = Queue::from_items(vec![item])?;
        let queue_lines = super::super::views::queue::lines(&queue, None, 4, width);
        assert_precomposed_lines_are_bounded(&queue_lines, width);
        assert!(queue_lines[0].spans[0].content.ends_with('…'));

        let (state, _) = reduce(
            AppState::default(),
            Action::SearchSubmitted {
                query: "huge".to_owned(),
                filter: SearchFilter::All,
            },
        );
        let generation = state.search().generation();
        let (state, _) = reduce(
            state,
            Action::SearchCompleted {
                generation,
                result: Ok(SearchPage::new(vec![SearchItem::Playable(
                    media_with_title(&huge),
                )])),
            },
        );
        let (state, _) = reduce(state, Action::ActivateSearchResult { index: 0 });
        let full_player = player_lines(&state, false, width, &Theme::default(), None, None);
        assert_precomposed_lines_are_bounded(&full_player, width);
        assert!(full_player[0].spans[0].content.ends_with('…'));
        let tiny_player = tiny_player_text(&state, width);
        assert!(usize::from(tiny_player.as_str().cell_width()) <= width);
        assert!(tiny_player.contains('界'));
        assert!(tiny_player.ends_with("s-e"));
        assert!(tiny_player.len() <= CLIP_BYTE_INSPECTION_BUDGET + 128);

        let palette_model = RenderModel::default().with_palette_query("q".repeat(2 * 1024 * 1024));
        let palette = palette_lines(&palette_model.palette, 4, width);
        assert_precomposed_lines_are_bounded(&palette, width);
        assert!(palette[0].spans[0].content.ends_with('…'));

        let combining_query = format!("A{}", "\u{301}".repeat(1_100_000));
        let combining_model = RenderModel::default().with_palette_query(&combining_query);
        assert_eq!(combining_model.palette.selected_action(), None);
        let combining_palette = palette_lines(&combining_model.palette, 2, width);
        assert_precomposed_lines_are_bounded(&combining_palette, width);
        assert_eq!(combining_palette[0].spans[0].content, "Query: …");
        Ok(())
    }

    #[test]
    fn mouse_progress_bar_requires_two_borderless_cells() {
        let state = lyrics_test_state(None, 0);
        assert_eq!(
            line_text_content(&progress_bar_line(
                &state,
                1,
                authoritative_progress_presentation(&state),
                &Theme::default(),
            )),
            ""
        );
        assert_eq!(
            line_text_content(&progress_bar_line(
                &state,
                2,
                authoritative_progress_presentation(&state),
                &Theme::default(),
            )),
            "░░"
        );
    }

    #[test]
    fn normal_player_keeps_playback_identity_while_podcast_progress_is_pending() {
        let state = pending_podcast_player_state();

        let rendered = line_text(&player_lines(
            &state,
            false,
            120,
            &Theme::default(),
            None,
            None,
        ));

        assert!(rendered.contains("Outgoing Song — Outgoing Artist"));
        assert!(!rendered.contains("Pending Podcast"));
        assert!(!rendered.contains("Pending Host"));
        assert!(!rendered.contains("Speed"));
    }

    #[test]
    fn tiny_player_keeps_playback_identity_while_podcast_progress_is_pending() {
        let state = pending_podcast_player_state();

        let rendered = tiny_player_text(&state, 120);

        assert!(rendered.contains("Outgoing Song/Outgoing Artist"));
        assert!(!rendered.contains("Pending Podcast"));
        assert!(!rendered.contains("Pending Host"));
        assert!(!rendered.contains("x1.0"));
    }

    #[test]
    fn players_keep_nothing_playing_fallback_without_a_playback_identity() {
        let state = AppState::default();

        assert!(
            line_text(&player_lines(
                &state,
                false,
                80,
                &Theme::default(),
                None,
                None,
            ))
            .contains("Nothing playing")
        );
        assert!(tiny_player_text(&state, 80).contains("Nothing playing"));
    }

    #[test]
    fn queue_viewport_keeps_offscreen_selection_visible() -> Result<(), Box<dyn Error>> {
        let mut queue = Queue::from_items(
            (0..12)
                .map(|index| queue_item(&format!("queue-{index}")))
                .collect(),
        )?;
        queue.select(&QueueItemId::from("queue-10"))?;

        let lines = super::super::views::queue::lines(&queue, None, 4, 48);

        assert_selected_row(&lines, 4, 48, "▶ Song queue-10");
        Ok(())
    }

    #[test]
    fn search_viewport_keeps_offscreen_selection_and_edge_footer_visible() {
        let items = indexed_media("search", 12);
        let selected = items[11].clone();
        let (state, _) = reduce(
            AppState::default(),
            Action::SearchSubmitted {
                query: "viewport".to_owned(),
                filter: SearchFilter::Songs,
            },
        );
        let generation = state.search().generation();
        let (state, _) = reduce(
            state,
            Action::SearchCompleted {
                generation,
                result: Ok(
                    SearchPage::new(items.into_iter().map(SearchItem::Playable).collect())
                        .with_continuation("search-more"),
                ),
            },
        );
        let (state, _) = reduce(
            state,
            Action::SearchSelectionChanged {
                id: SearchItem::Playable(selected).stable_id(),
            },
        );

        let lines = super::super::views::search::lines(&state, 6, 48, None);

        assert_selected_row(&lines, 6, 48, "▶ Song search-11");
        assert!(line_text(&lines).contains("[m] Load more"));
    }

    #[test]
    fn chart_viewport_reserves_pinned_header_and_keeps_offscreen_selection_visible()
    -> Result<(), Box<dyn Error>> {
        let sections = (0..3)
            .map(|section| {
                ChartSection::new(
                    format!("Section {}", section + 1),
                    indexed_media(&format!("chart-{section}"), 4),
                )
            })
            .collect::<Vec<_>>();
        let selected = sections[2].items()[0].id.clone();
        let region = RegionCode::parse("HK")?;
        let (state, _) = reduce(
            AppState::default(),
            Action::ChartsRequested {
                region: region.clone(),
            },
        );
        let generation = state.charts().generation();
        let (state, _) = reduce(
            state,
            Action::ChartsCompleted {
                generation,
                region,
                received_at: 1_000,
                result: Ok(sections),
            },
        );
        let (state, _) = reduce(state, Action::ChartSelectionChanged { media_id: selected });

        let lines = super::super::views::charts::lines(&state, 5, 48);

        assert_selected_row(&lines, 5, 48, "▶ Song chart-2-0");
        assert!(line_text(&lines).contains("• Section 3"));
        Ok(())
    }

    #[test]
    fn chart_viewport_keeps_deep_selection_with_its_section_header() -> Result<(), Box<dyn Error>> {
        let sections = vec![
            ChartSection::new("Long section", indexed_media("deep-chart", 14)),
            ChartSection::new("Following section", indexed_media("later-chart", 3)),
        ];
        let selected = sections[0].items()[12].id.clone();
        let region = RegionCode::parse("HK")?;
        let (state, _) = reduce(
            AppState::default(),
            Action::ChartsRequested {
                region: region.clone(),
            },
        );
        let generation = state.charts().generation();
        let (state, _) = reduce(
            state,
            Action::ChartsCompleted {
                generation,
                region,
                received_at: 1_000,
                result: Ok(sections),
            },
        );
        let (state, _) = reduce(state, Action::ChartSelectionChanged { media_id: selected });

        let lines = super::super::views::charts::lines(&state, 5, 48);

        assert_selected_row(&lines, 5, 48, "▶ Song deep-chart-12");
        let rendered = lines.iter().map(line_text_ref).collect::<Vec<_>>();
        let header_index = rendered
            .iter()
            .position(|line| line.contains("• Long section"))
            .unwrap_or_else(|| panic!("selected section header must remain visible"));
        let selected_index = rendered
            .iter()
            .position(|line| line.contains("▶ Song deep-chart-12"))
            .unwrap_or_else(|| panic!("selected chart row must remain visible"));
        assert!(header_index < selected_index);
        assert_eq!(
            rendered
                .iter()
                .filter(|line| line.contains("deep-chart-"))
                .map(String::as_str)
                .collect::<Vec<_>>(),
            vec![
                "  Song deep-chart-10 — Artist",
                "  Song deep-chart-11 — Artist",
                "▶ Song deep-chart-12 — Artist",
            ]
        );
        Ok(())
    }

    fn chart_viewport_fixture() -> Result<AppState, Box<dyn Error>> {
        let sections = (0..3)
            .map(|section| {
                ChartSection::new(
                    format!("Section {}", section + 1),
                    indexed_media(&format!("trace-{section}"), 5),
                )
            })
            .collect::<Vec<_>>();
        let region = RegionCode::parse("HK")?;
        let (state, _) = reduce(
            AppState::default(),
            Action::ChartsRequested {
                region: region.clone(),
            },
        );
        let generation = state.charts().generation();
        Ok(reduce(
            state,
            Action::ChartsCompleted {
                generation,
                region,
                received_at: 1_000,
                result: Ok(sections),
            },
        )
        .0)
    }

    fn render_chart_trace_step(
        state: AppState,
        selected_index: usize,
        viewport: &mut SelectionViewport,
        row_limit: usize,
    ) -> (AppState, usize, Vec<usize>, String) {
        let state = reduce(
            state,
            Action::ChartRowSelectionChanged {
                item_index: selected_index,
            },
        )
        .0;
        let mut targets = Vec::new();
        let lines = super::super::views::charts::lines_with_viewport_and_targets(
            &state,
            row_limit,
            48,
            viewport,
            Some(&mut targets),
        );
        let visible = targets
            .iter()
            .map(|target| target.stable_index)
            .collect::<Vec<_>>();
        let pinned = lines
            .iter()
            .map(line_text_ref)
            .find(|line| line.starts_with("• "))
            .unwrap_or_else(|| panic!("chart trace pinned header"));
        (state, viewport.start, visible, pinned)
    }

    #[test]
    fn chart_viewport_moves_only_at_visible_boundary() -> Result<(), Box<dyn Error>> {
        let mut state = chart_viewport_fixture()?;
        let mut viewport = SelectionViewport::default();
        let down = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10];
        let down_starts = [0, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7];
        let mut actual_down = Vec::new();

        for (selected, expected_start) in down.into_iter().zip(down_starts) {
            let (next, start, visible, pinned) =
                render_chart_trace_step(state, selected, &mut viewport, 6);
            eprintln!(
                "down selected={selected} start={start} visible={visible:?} pinned={pinned:?}"
            );
            actual_down.push((selected, start, visible, expected_start));
            state = next;
        }

        let up = [9, 8, 7, 6, 5, 4, 3, 2, 1, 0];
        let up_starts = [7, 7, 7, 6, 5, 4, 3, 2, 1, 0];
        let mut actual_up = Vec::new();
        for (selected, expected_start) in up.into_iter().zip(up_starts) {
            let (next, start, visible, pinned) =
                render_chart_trace_step(state, selected, &mut viewport, 6);
            eprintln!("up selected={selected} start={start} visible={visible:?} pinned={pinned:?}");
            actual_up.push((selected, start, visible, expected_start));
            state = next;
        }
        for (selected, start, visible, expected_start) in actual_down.into_iter().chain(actual_up) {
            assert_eq!(start, expected_start, "selected={selected}");
            assert_eq!(visible, (start..start + 4).collect::<Vec<_>>());
        }
        Ok(())
    }

    #[test]
    fn chart_duplicate_ids_keep_the_later_occurrence_stable_when_moving_up()
    -> Result<(), Box<dyn Error>> {
        let duplicate = indexed_media("duplicate", 1)
            .into_iter()
            .next()
            .unwrap_or_else(|| panic!("duplicate fixture"));
        let sections = vec![
            ChartSection::new(
                "Daily Top".to_owned(),
                vec![
                    indexed_media("daily", 1).remove(0),
                    duplicate.clone(),
                    indexed_media("daily-tail", 3).remove(0),
                    indexed_media("daily-tail", 3).remove(1),
                    indexed_media("daily-tail", 3).remove(2),
                ],
            ),
            ChartSection::new(
                "Top 100".to_owned(),
                vec![
                    indexed_media("top", 1).remove(0),
                    indexed_media("top-tail", 3).remove(0),
                    indexed_media("top-tail", 3).remove(1),
                    indexed_media("top-tail", 3).remove(2),
                    duplicate,
                ],
            ),
        ];
        let region = RegionCode::parse("KR")?;
        let (state, _) = reduce(
            AppState::default(),
            Action::ChartsRequested {
                region: region.clone(),
            },
        );
        let generation = state.charts().generation();
        let state = reduce(
            state,
            Action::ChartsCompleted {
                generation,
                region,
                received_at: 1_000,
                result: Ok(sections),
            },
        )
        .0;
        let mut viewport = SelectionViewport::default();

        let (state, start, before, pinned) = render_chart_trace_step(state, 9, &mut viewport, 6);
        assert_eq!(start, 6);
        assert_eq!(before, vec![6, 7, 8, 9]);
        assert_eq!(pinned, "• Top 100");

        let (_, start, after, pinned) = render_chart_trace_step(state, 8, &mut viewport, 6);
        assert_eq!(start, 6);
        assert_eq!(after, before);
        assert_eq!(pinned, "• Top 100");
        Ok(())
    }

    #[test]
    fn chart_pinned_title_changes_without_shifting_item_rows() -> Result<(), Box<dyn Error>> {
        let state = chart_viewport_fixture()?;
        let mut viewport = SelectionViewport::default();

        let (state, start, before, pinned) = render_chart_trace_step(state, 4, &mut viewport, 8);
        assert_eq!(start, 0);
        assert_eq!(before, vec![0, 1, 2, 3, 4, 5]);
        assert_eq!(pinned, "• Section 1");

        let (_, start, after, pinned) = render_chart_trace_step(state, 5, &mut viewport, 8);
        assert_eq!(start, 0);
        assert_eq!(after, before);
        assert_eq!(pinned, "• Section 2");
        Ok(())
    }

    #[test]
    fn chart_viewport_clamps_across_grow_shrink_zero_and_one_item_row() -> Result<(), Box<dyn Error>>
    {
        let state = chart_viewport_fixture()?;
        let mut viewport = SelectionViewport::default();

        let (state, start, visible, _) = render_chart_trace_step(state, 10, &mut viewport, 6);
        assert_eq!((start, visible), (7, vec![7, 8, 9, 10]));

        let (state, start, visible, _) = render_chart_trace_step(state, 10, &mut viewport, 9);
        assert_eq!((start, visible), (7, vec![7, 8, 9, 10, 11, 12, 13]));

        let (state, start, visible, _) = render_chart_trace_step(state, 10, &mut viewport, 5);
        assert_eq!((start, visible), (8, vec![8, 9, 10]));

        assert!(
            super::super::views::charts::lines_with_viewport(&state, 0, 48, &mut viewport)
                .is_empty()
        );
        assert_eq!(viewport.start, 8);
        let one_header_row =
            super::super::views::charts::lines_with_viewport(&state, 2, 48, &mut viewport);
        assert_eq!(line_text(&one_header_row), "Trending in HK\n• Section 3");
        assert_eq!(viewport.start, 8);

        let (_, start, visible, _) = render_chart_trace_step(state, 10, &mut viewport, 6);
        assert_eq!((start, visible), (8, vec![8, 9, 10, 11]));

        let state = chart_viewport_fixture()?;
        let (state, start, visible, _) = render_chart_trace_step(state, 10, &mut viewport, 3);
        assert_eq!((start, visible), (10, vec![10]));
        let (_, start, visible, _) = render_chart_trace_step(state, 10, &mut viewport, 6);
        assert_eq!((start, visible), (10, vec![10, 11, 12, 13]));
        Ok(())
    }

    #[test]
    fn chart_viewport_keeps_empty_state_and_rejects_missing_selection() -> Result<(), Box<dyn Error>>
    {
        let region = RegionCode::parse("HK")?;
        let (loading, _) = reduce(
            AppState::default(),
            Action::ChartsRequested {
                region: region.clone(),
            },
        );
        let generation = loading.charts().generation();
        assert_eq!(
            line_text(&super::super::views::charts::lines(&loading, 3, 48)),
            "Trending in HK\n⠋ Loading regional charts"
        );
        let empty = reduce(
            loading,
            Action::ChartsCompleted {
                generation,
                region,
                received_at: 1_000,
                result: Ok(Vec::new()),
            },
        )
        .0;
        assert_eq!(
            line_text(&super::super::views::charts::lines(&empty, 3, 48)),
            "Trending in HK\nChoose a country with c to load regional charts."
        );

        let state = chart_viewport_fixture()?;
        let selected = state.charts().selected_id().cloned();
        let missing = MediaId {
            provider: "youtube-music".to_owned(),
            video_id: "missing-chart-item".to_owned(),
        };
        let state = reduce(state, Action::ChartSelectionChanged { media_id: missing }).0;
        assert_eq!(state.charts().selected_id(), selected.as_ref());
        Ok(())
    }

    #[test]
    fn podcast_viewport_keeps_offscreen_episode_selection_visible() {
        let episodes = indexed_media("episode", 12)
            .into_iter()
            .map(|media| MediaItem {
                kind: MediaKind::PodcastEpisode,
                ..media
            })
            .collect::<Vec<_>>();
        let selected = episodes[10].id.clone();
        let metadata = SearchMetadata::new(SearchMetadataKind::Podcast, "Viewport Show")
            .with_provider_id("viewport-show");
        let (state, _) = reduce(
            AppState::default(),
            Action::SearchSubmitted {
                query: "viewport show".to_owned(),
                filter: SearchFilter::Podcasts,
            },
        );
        let generation = state.search().generation();
        let (state, _) = reduce(
            state,
            Action::SearchCompleted {
                generation,
                result: Ok(SearchPage::new(vec![SearchItem::Metadata(metadata)])),
            },
        );
        let (state, effects) = reduce(state, Action::OpenSelectedPodcast);
        let [Effect::LoadPodcast { generation, .. }] = effects.as_slice() else {
            panic!("podcast fixture must load");
        };
        let (state, _) = reduce(
            state,
            Action::PodcastCompleted {
                generation: *generation,
                result: Ok(Podcast {
                    id: "viewport-show".to_owned(),
                    title: "Viewport Show".to_owned(),
                    creators: vec!["Host".to_owned()],
                    description: None,
                    artwork_url: None,
                    episodes,
                }),
            },
        );
        let (state, _) = reduce(
            state,
            Action::PodcastSelectionChanged { media_id: selected },
        );

        let lines = super::super::views::podcasts::lines(&state, 5, 48);

        assert_selected_row(&lines, 5, 48, "▶ Song episode-10");
    }

    #[test]
    fn podcast_recommendations_loading_uses_requested_region() -> Result<(), Box<dyn Error>> {
        let (state, _) = reduce(
            AppState::default(),
            Action::PodcastRecommendationsRequested {
                region: RegionCode::parse("JP")?,
            },
        );

        let rendered = line_text(&super::super::views::podcasts::lines(&state, 4, 48));

        assert!(rendered.contains("Top podcasts in JP"), "{rendered}");
        assert!(rendered.contains("⠋ Loading recommendations"), "{rendered}");
        Ok(())
    }

    #[test]
    fn podcast_recommendations_render_effective_region_rank_and_publisher()
    -> Result<(), Box<dyn Error>> {
        let state = loaded_podcast_recommendations(
            "ZZ",
            "JP",
            &[
                ("daily", "The Daily", "The New York Times"),
                ("up-first", "Up First", "NPR"),
            ],
        )?;

        let rendered = line_text(&super::super::views::podcasts::lines(&state, 5, 64));

        assert!(rendered.contains("Top podcasts in JP"), "{rendered}");
        assert!(
            rendered.contains("▶ 1. The Daily  ·  The New York Times"),
            "{rendered}"
        );
        assert!(rendered.contains("  2. Up First  ·  NPR"), "{rendered}");
        assert!(!rendered.contains("daily"), "source ID leaked: {rendered}");
        Ok(())
    }

    #[test]
    fn podcast_recommendation_refresh_uses_requested_then_returned_effective_region()
    -> Result<(), Box<dyn Error>> {
        let state = loaded_podcast_recommendations(
            "US",
            "US",
            &[("daily", "The Daily", "The New York Times")],
        )?;
        let jp = RegionCode::parse("JP")?;
        let (state, effects) = reduce(
            state,
            Action::PodcastRecommendationsRequested { region: jp.clone() },
        );
        let [Effect::LoadPodcastRecommendations { generation, .. }] = effects.as_slice() else {
            panic!("recommendation refresh effect");
        };

        let loading = line_text(&super::super::views::podcasts::lines(&state, 5, 64));
        assert!(loading.contains("Top podcasts in JP"), "{loading}");
        assert!(loading.contains("▶ 1. The Daily"), "{loading}");
        assert!(!loading.contains("Top podcasts in US"), "{loading}");

        let page =
            podcast_recommendation_page("HK", &[("global-news", "Global News Podcast", "BBC")])?;
        let (state, _) = reduce(
            state,
            Action::PodcastRecommendationsCompleted {
                generation: *generation,
                requested_region: jp,
                result: Ok(page),
            },
        );
        let loaded = line_text(&super::super::views::podcasts::lines(&state, 5, 64));
        assert!(loaded.contains("Top podcasts in HK"), "{loaded}");
        assert!(
            loaded.contains("▶ 1. Global News Podcast  ·  BBC"),
            "{loaded}"
        );
        Ok(())
    }

    #[test]
    fn podcast_recommendation_match_loading_keeps_selected_list_row() -> Result<(), Box<dyn Error>>
    {
        let state = loaded_podcast_recommendations(
            "JP",
            "JP",
            &[
                ("daily", "The Daily", "The New York Times"),
                ("up-first", "Up First", "NPR"),
            ],
        )?;
        let (state, _) = reduce(state, Action::OpenSelectedPodcastRecommendation);

        let rendered = line_text(&super::super::views::podcasts::lines(&state, 6, 64));

        assert!(
            rendered.contains("⠋ Finding on YouTube Music"),
            "{rendered}"
        );
        assert!(
            rendered.contains("▶ 1. The Daily  ·  The New York Times"),
            "{rendered}"
        );
        Ok(())
    }

    #[test]
    fn podcast_recommendation_source_failure_is_safe_and_offers_search()
    -> Result<(), Box<dyn Error>> {
        let requested = RegionCode::parse("JP")?;
        let (state, effects) = reduce(
            AppState::default(),
            Action::PodcastRecommendationsRequested {
                region: requested.clone(),
            },
        );
        let [Effect::LoadPodcastRecommendations { generation, .. }] = effects.as_slice() else {
            panic!("recommendation request effect");
        };
        let (state, _) = reduce(
            state,
            Action::PodcastRecommendationsCompleted {
                generation: *generation,
                requested_region: requested,
                result: Err(AppError::new(
                    AppErrorCategory::Podcast,
                    "SECRET upstream ranking response",
                )),
            },
        );

        let rendered = line_text(&super::super::views::podcasts::lines(&state, 5, 48));

        assert!(
            rendered.contains("Podcast recommendations unavailable"),
            "{rendered}"
        );
        assert!(
            rendered.contains("Press / to search podcasts"),
            "{rendered}"
        );
        assert!(!rendered.contains("SECRET"), "raw error leaked: {rendered}");
        Ok(())
    }

    #[test]
    fn podcast_recommendation_match_failure_keeps_list_and_selection() -> Result<(), Box<dyn Error>>
    {
        let state = loaded_podcast_recommendations(
            "JP",
            "JP",
            &[("daily", "The Daily", "The New York Times")],
        )?;
        let (state, effects) = reduce(state, Action::OpenSelectedPodcastRecommendation);
        let [Effect::ResolvePodcastRecommendation { generation, .. }] = effects.as_slice() else {
            panic!("recommendation resolve effect");
        };
        let (state, _) = reduce(
            state,
            Action::PodcastRecommendationResolved {
                generation: *generation,
                result: Err(AppError::new(
                    AppErrorCategory::Search,
                    "SECRET ambiguous provider response",
                )),
            },
        );

        let rendered = line_text(&super::super::views::podcasts::lines(&state, 6, 64));

        assert!(
            rendered.contains("Unavailable on YouTube Music"),
            "{rendered}"
        );
        assert!(
            rendered.contains("Press / to search podcasts"),
            "{rendered}"
        );
        assert!(
            rendered.contains("▶ 1. The Daily  ·  The New York Times"),
            "{rendered}"
        );
        assert!(!rendered.contains("SECRET"), "raw error leaked: {rendered}");
        Ok(())
    }

    #[test]
    fn opened_podcast_takes_precedence_over_recommendation_refresh() -> Result<(), Box<dyn Error>> {
        let state = loaded_podcast_recommendations(
            "JP",
            "JP",
            &[("daily", "The Daily", "The New York Times")],
        )?;
        let (state, effects) = reduce(state, Action::OpenSelectedPodcastRecommendation);
        let [Effect::ResolvePodcastRecommendation { generation, .. }] = effects.as_slice() else {
            panic!("recommendation resolve effect");
        };
        let provider_id = crate::app::PodcastProviderId::new("daily-provider".to_owned())
            .unwrap_or_else(|| panic!("valid provider ID"));
        let (state, effects) = reduce(
            state,
            Action::PodcastRecommendationResolved {
                generation: *generation,
                result: Ok(provider_id),
            },
        );
        let [Effect::LoadPodcast { generation, .. }] = effects.as_slice() else {
            panic!("podcast detail effect");
        };
        let (state, _) = reduce(
            state,
            Action::PodcastCompleted {
                generation: *generation,
                result: Ok(Podcast {
                    id: "daily-provider".to_owned(),
                    title: "The Daily".to_owned(),
                    creators: vec!["The New York Times".to_owned()],
                    description: None,
                    artwork_url: None,
                    episodes: vec![MediaItem {
                        id: MediaId {
                            provider: "youtube-music".to_owned(),
                            video_id: "episode-1".to_owned(),
                        },
                        kind: MediaKind::PodcastEpisode,
                        title: "The Sunday Read".to_owned(),
                        creators: vec!["The Daily".to_owned()],
                        collection: None,
                        duration_ms: Some(90_000),
                        artwork_url: None,
                        explicit: false,
                    }],
                }),
            },
        );
        let (state, _) = reduce(
            state,
            Action::PodcastRecommendationsRequested {
                region: RegionCode::parse("US")?,
            },
        );

        let rendered = line_text(&super::super::views::podcasts::lines(&state, 5, 64));

        assert!(
            rendered.contains("The Daily — The New York Times"),
            "{rendered}"
        );
        assert!(
            rendered.contains("▶ The Sunday Read  ·  01:30"),
            "{rendered}"
        );
        assert!(!rendered.contains("Top podcasts"), "{rendered}");
        Ok(())
    }

    #[test]
    fn empty_podcast_view_offers_manual_search() {
        let rendered = line_text(&super::super::views::podcasts::lines(
            &AppState::default(),
            3,
            48,
        ));

        assert!(
            rendered.contains("Press / to search podcasts"),
            "{rendered}"
        );
    }

    #[test]
    fn manual_podcast_detail_loading_and_error_remain_visible_and_bounded() {
        let metadata = SearchMetadata::new(SearchMetadataKind::Podcast, "Manual Show")
            .with_provider_id("manual-show");
        let (state, _) = reduce(
            AppState::default(),
            Action::SearchSubmitted {
                query: "manual show".to_owned(),
                filter: SearchFilter::Podcasts,
            },
        );
        let generation = state.search().generation();
        let (state, _) = reduce(
            state,
            Action::SearchCompleted {
                generation,
                result: Ok(SearchPage::new(vec![SearchItem::Metadata(metadata)])),
            },
        );
        let (state, effects) = reduce(state, Action::OpenSelectedPodcast);
        let [Effect::LoadPodcast { generation, .. }] = effects.as_slice() else {
            panic!("podcast detail effect");
        };

        let loading = line_text(&super::super::views::podcasts::lines(&state, 3, 32));
        assert!(loading.contains("⠋ Loading podcast"), "{loading}");

        let (state, _) = reduce(
            state,
            Action::PodcastCompleted {
                generation: *generation,
                result: Err(AppError::new(
                    AppErrorCategory::Podcast,
                    "SECRET-PODCAST-BODY https://private.example/secret-episode-path-619 provider-id-8472",
                )),
            },
        );
        let error_lines = super::super::views::podcasts::lines(&state, 3, 72);
        let error = line_text(&error_lines);
        assert!(error.contains("! Podcast unavailable"), "{error}");
        assert!(error.contains("Press / to search podcasts"), "{error}");
        for secret in [
            "SECRET-PODCAST-BODY",
            "private.example",
            "secret-episode-path-619",
            "provider-id-8472",
        ] {
            assert!(
                !error.contains(secret),
                "detail error leaked {secret}: {error}"
            );
        }
        assert_precomposed_lines_are_bounded(&error_lines, 72);
    }

    #[test]
    fn retained_podcast_list_uses_safe_refresh_and_detail_failure_statuses()
    -> Result<(), Box<dyn Error>> {
        let state = loaded_podcast_recommendations(
            "US",
            "US",
            &[("daily", "The Daily", "The New York Times")],
        )?;
        let jp = RegionCode::parse("JP")?;
        let (state, effects) = reduce(
            state,
            Action::PodcastRecommendationsRequested { region: jp.clone() },
        );
        let [Effect::LoadPodcastRecommendations { generation, .. }] = effects.as_slice() else {
            panic!("recommendation refresh effect");
        };
        let (state, _) = reduce(
            state,
            Action::PodcastRecommendationsCompleted {
                generation: *generation,
                requested_region: jp,
                result: Err(AppError::new(
                    AppErrorCategory::Podcast,
                    "SECRET-REFRESH-BODY",
                )),
            },
        );
        let refresh = line_text(&super::super::views::podcasts::lines(&state, 5, 64));
        assert!(
            refresh.contains("! Recommendations could not be refreshed"),
            "{refresh}"
        );
        assert!(refresh.contains("▶ 1. The Daily"), "{refresh}");
        assert!(!refresh.contains("SECRET-REFRESH-BODY"), "{refresh}");

        let (state, effects) = reduce(state, Action::OpenSelectedPodcastRecommendation);
        let [Effect::ResolvePodcastRecommendation { generation, .. }] = effects.as_slice() else {
            panic!("recommendation resolve effect");
        };
        let provider_id = crate::app::PodcastProviderId::new("daily-provider".to_owned())
            .unwrap_or_else(|| panic!("valid provider ID"));
        let (state, effects) = reduce(
            state,
            Action::PodcastRecommendationResolved {
                generation: *generation,
                result: Ok(provider_id),
            },
        );
        let [Effect::LoadPodcast { generation, .. }] = effects.as_slice() else {
            panic!("podcast detail effect");
        };
        let (state, _) = reduce(
            state,
            Action::PodcastCompleted {
                generation: *generation,
                result: Err(AppError::new(
                    AppErrorCategory::Podcast,
                    "SECRET-DETAIL-BODY",
                )),
            },
        );
        let detail_lines = super::super::views::podcasts::lines(&state, 5, 64);
        let detail = line_text(&detail_lines);
        assert!(detail.contains("! Podcast unavailable"), "{detail}");
        assert!(detail.contains("Press / to search podcasts"), "{detail}");
        assert!(detail.contains("▶ 1. The Daily"), "{detail}");
        assert!(!detail.contains("SECRET-DETAIL-BODY"), "{detail}");

        let tiny = super::super::views::podcasts::lines(&state, 2, 4);
        assert_eq!(tiny.len(), 2);
        assert_precomposed_lines_are_bounded(&tiny, 4);
        Ok(())
    }

    #[test]
    fn podcast_recommendation_rows_are_bounded_for_tiny_unicode_viewports()
    -> Result<(), Box<dyn Error>> {
        let title = "界".repeat(120);
        let publisher = "出版社".repeat(40);
        let state = loaded_podcast_recommendations(
            "JP",
            "JP",
            &[("unicode", title.as_str(), publisher.as_str())],
        )?;

        for (row_limit, width) in [(1, 1), (2, 4), (3, 12)] {
            let lines = super::super::views::podcasts::lines(&state, row_limit, width);
            assert!(lines.len() <= row_limit);
            assert_precomposed_lines_are_bounded(&lines, width);
        }
        Ok(())
    }

    #[test]
    fn library_viewport_keeps_offscreen_selection_and_edge_footer_visible() {
        let items = indexed_media("library", 12)
            .into_iter()
            .map(LibraryItem::Playable)
            .collect::<Vec<_>>();
        let selected = stable_library_item_id(&items[11]);
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
        let (state, _) = reduce(
            state,
            Action::LibraryCompleted {
                generation: *generation,
                result: Ok(Page {
                    items,
                    continuation: Some("library-more".to_owned()),
                    stale: false,
                }),
            },
        );
        let (state, _) = reduce(state, Action::LibrarySelectionChanged { id: selected });

        let lines = super::super::views::library::lines(&state, 5, 48);

        assert_selected_row(&lines, 5, 48, "▶ Song library-11");
        assert!(line_text(&lines).contains("[m] Load more"));
    }

    #[test]
    fn history_viewport_keeps_offscreen_selection_visible() {
        let entries = indexed_media("history", 12)
            .into_iter()
            .enumerate()
            .map(|(index, item)| HistoryEntry {
                id: i64::try_from(index).unwrap_or_else(|_| panic!("small history fixture")),
                item,
                played_at: 1_000
                    + i64::try_from(index).unwrap_or_else(|_| panic!("small history fixture")),
            })
            .collect::<Vec<_>>();
        let (state, effects) = reduce(AppState::default(), Action::HistoryRequested);
        let [Effect::LoadHistory { generation, .. }] = effects.as_slice() else {
            panic!("history fixture must load");
        };
        let (state, _) = reduce(
            state,
            Action::HistoryCompleted {
                generation: *generation,
                result: Ok(entries),
            },
        );
        let (state, _) = reduce(state, Action::HistorySelectionChanged { id: 10 });

        let lines = super::super::views::history::lines(&state, 4, 48);

        assert_selected_row(&lines, 4, 48, "▶ Song history-10");
    }

    #[test]
    fn country_picker_viewport_keeps_offscreen_selection_visible() -> Result<(), Box<dyn Error>> {
        let picker = CountryPickerState::for_region(&RegionCode::parse("SG")?);

        let lines = country_picker_lines(&picker, 4, 48);

        assert_selected_row(&lines, 4, 48, "▶ Singapore · SG");
        Ok(())
    }

    #[test]
    fn effective_layout_model_borrows_palette_storage() {
        let model = RenderModel::default()
            .with_palette_query("q".repeat(2 * 1024 * 1024))
            .with_focus(FocusRegion::Queue)
            .toggle_compact_panel();

        let effective = model.effective_for_layout(LayoutMode::Tiny);

        assert!(std::ptr::eq(effective.palette, &raw const model.palette));
        assert_eq!(effective.view, model.view);
        assert_eq!(effective.overlay, model.overlay);
        assert_eq!(effective.focus, FocusRegion::Content);
        assert_eq!(effective.compact_panel, CompactPanel::Content);
    }

    fn assert_seeded_active_queue_order(width: u16, height: u16) -> Result<(), Box<dyn Error>> {
        let mut queue =
            Queue::from_items(["a", "b", "c", "d"].into_iter().map(queue_item).collect())?;
        queue.select(&QueueItemId::from("c"))?;
        queue.set_shuffle(true, 13);
        queue.move_before(&QueueItemId::from("d"), &QueueItemId::from("a"))?;

        let logical = queue
            .items()
            .iter()
            .map(|item| item.id().as_str())
            .collect::<Vec<_>>();
        let active = queue
            .active_items()
            .map(|item| item.id().as_str())
            .collect::<Vec<_>>();
        assert_ne!(logical, active);
        assert_eq!(active.first().copied(), Some("c"));

        let mut terminal = Terminal::new(TestBackend::new(width, height))?;
        let theme = Theme::for_capability(ColorCapability::TrueColor);
        let model = RenderModel::default().with_focus(FocusRegion::Queue);
        let effective = model.effective_for_layout(LayoutMode::Wide);
        let mut viewports = ViewportMemory::default();
        terminal.draw(|frame| {
            render_queue(
                frame,
                frame.area(),
                &queue,
                &theme,
                &effective,
                &mut viewports,
                None,
            );
        })?;
        let rendered = terminal.backend().to_string();

        let mut previous = None;
        for id in active {
            let needle = format!("Song {id}");
            let position = rendered
                .find(&needle)
                .ok_or_else(|| std::io::Error::other(format!("missing queue row `{needle}`")))?;
            if let Some(previous) = previous {
                assert!(
                    previous < position,
                    "queue rows must follow active playback order:\n{rendered}"
                );
            }
            previous = Some(position);
        }
        assert!(
            rendered
                .lines()
                .nth(1)
                .is_some_and(|line| line.contains("▶ Song c")),
            "the current active item must be marked on the first row:\n{rendered}"
        );
        Ok(())
    }

    fn pending_podcast_player_state() -> AppState {
        let song = MediaItem {
            id: MediaId {
                provider: "youtube-music".to_owned(),
                video_id: "outgoing-player-song".to_owned(),
            },
            kind: MediaKind::Song,
            title: "Outgoing Song".to_owned(),
            creators: vec!["Outgoing Artist".to_owned()],
            collection: None,
            duration_ms: Some(180_000),
            artwork_url: None,
            explicit: false,
        };
        let podcast = MediaItem {
            id: MediaId {
                provider: "youtube-music".to_owned(),
                video_id: "pending-player-podcast".to_owned(),
            },
            kind: MediaKind::PodcastEpisode,
            title: "Pending Podcast".to_owned(),
            creators: vec!["Pending Host".to_owned()],
            collection: None,
            duration_ms: Some(240_000),
            artwork_url: None,
            explicit: false,
        };
        let mut state = AppState::default();
        for item in [song.clone(), podcast.clone()] {
            (state, _) = reduce(state, Action::EnqueueMedia { item });
        }
        let (state, _) = reduce(
            state,
            Action::PlayQueueItem {
                id: crate::app::stable_queue_item_id(&song.id),
            },
        );
        let generation = state
            .current_attempt_generation()
            .unwrap_or_else(|| panic!("outgoing song must have an active attempt"));
        let (state, _) = reduce(
            state,
            Action::PlayerStatusChanged {
                generation,
                status: PlaybackStatus::Playing,
            },
        );
        reduce(
            state,
            Action::PlayQueueItem {
                id: crate::app::stable_queue_item_id(&podcast.id),
            },
        )
        .0
    }

    fn solid_animation_frame(rgb: [u8; 3]) -> Result<Arc<ArtworkGrid>, Box<dyn Error>> {
        let cells = usize::from(PRODUCTION_ARTWORK_SIZE.width)
            .saturating_mul(usize::from(PRODUCTION_ARTWORK_SIZE.height))
            .saturating_mul(2);
        let pixels = std::iter::repeat_n(rgb, cells)
            .flatten()
            .collect::<Vec<_>>();
        Ok(Arc::new(decode_rgb_frame(
            &pixels,
            PRODUCTION_ARTWORK_SIZE,
        )?))
    }

    fn presentation_starts_with_rgb(presentation: &ArtworkPresentation, expected: [u8; 3]) -> bool {
        let ArtworkPresentation::Grid(grid) = presentation else {
            return false;
        };
        grid.cell(0, 0).is_some_and(|cell| {
            let color = cell.foreground();
            [color.red(), color.green(), color.blue()] == expected
        })
    }

    fn is_unavailable_presentation(presentation: Option<&ArtworkPresentation>) -> bool {
        matches!(
            presentation,
            Some(ArtworkPresentation::Fallback(fallback))
                if fallback.icon() == "♪" && fallback.metadata() == "Artwork unavailable"
        )
    }

    fn artwork_policy_state(artwork_url: Option<url::Url>) -> AppState {
        let item = MediaItem {
            id: MediaId {
                provider: "youtube-music".to_owned(),
                video_id: "artwork-policy-video".to_owned(),
            },
            kind: MediaKind::Video,
            title: "Artwork policy fixture".to_owned(),
            creators: vec!["Artist".to_owned()],
            collection: None,
            duration_ms: Some(180_000),
            artwork_url,
            explicit: false,
        };
        let id = crate::app::stable_queue_item_id(&item.id);
        let (state, _) = reduce(AppState::default(), Action::EnqueueMedia { item });
        let (state, _) = reduce(state, Action::PlayQueueItem { id });
        playing_test_state(state)
    }

    fn render_animation_store_frame(
        state: &AppState,
        static_store: &ArtworkPresentationStore,
        animation_store: &AnimationFrameStore,
        width: u16,
        height: u16,
    ) -> Result<String, Box<dyn Error>> {
        let presentation = artwork_presentation_from_stores(
            state,
            Some(static_store),
            Some(animation_store),
            PRODUCTION_ARTWORK_SIZE,
            LayoutMode::from_dimensions(width, height),
        );
        match presentation {
            Some(presentation) => render_artwork_presentation(state, &presentation, width, height),
            None => render_artwork_presentation(
                state,
                &ArtworkPresentation::unavailable(),
                width,
                height,
            ),
        }
    }

    fn render_artwork_presentation(
        state: &AppState,
        presentation: &ArtworkPresentation,
        width: u16,
        height: u16,
    ) -> Result<String, Box<dyn Error>> {
        let mut terminal = Terminal::new(TestBackend::new(width, height))?;
        terminal.draw(|frame| {
            render_with_model_and_artwork(
                frame,
                state,
                &Theme::default(),
                &RenderModel::default(),
                presentation,
            );
        })?;
        let backend = terminal.backend();
        let colors = (0..height)
            .flat_map(|y| {
                (0..width).filter_map(move |x| {
                    backend.buffer().cell((x, y)).map(|cell| (cell.fg, cell.bg))
                })
            })
            .collect::<Vec<_>>();
        Ok(format!("{backend}\ncolors:{colors:?}"))
    }

    fn static_artwork_store(
        state: &AppState,
        rgb: [u8; 3],
    ) -> Result<ArtworkPresentationStore, Box<dyn Error>> {
        let url = state
            .artwork()
            .requested_url()
            .ok_or("static artwork fixture must request artwork")?;
        let generation = state.artwork().generation();
        let store = ArtworkPresentationStore::new();
        store.request(generation, url);
        assert!(store.publish(
            generation,
            url,
            ArtworkPresentation::Grid(solid_animation_frame(rgb)?),
        ));
        Ok(store)
    }

    fn timed_test_document() -> LyricsDocument {
        LyricsDocument::new(
            LyricsSource::Lrclib,
            None,
            vec![
                TimedLyricLine::new(0, Some(1_000), "previous")
                    .unwrap_or_else(|error| panic!("timed fixture: {error}")),
                TimedLyricLine::new(1_000, Some(2_000), "current")
                    .unwrap_or_else(|error| panic!("timed fixture: {error}")),
                TimedLyricLine::new(2_000, None, "next")
                    .unwrap_or_else(|error| panic!("timed fixture: {error}")),
            ],
            false,
        )
        .unwrap_or_else(|error| panic!("timed document fixture: {error}"))
    }

    fn long_timed_test_document() -> LyricsDocument {
        let timed = (0_u64..10)
            .map(|index| {
                let start = index.saturating_mul(1_000);
                TimedLyricLine::new(
                    start,
                    Some(start.saturating_add(1_000)),
                    &format!("line-{index}"),
                )
                .unwrap_or_else(|error| panic!("long timed fixture: {error}"))
            })
            .collect();
        LyricsDocument::new(LyricsSource::Lrclib, None, timed, false)
            .unwrap_or_else(|error| panic!("long timed document fixture: {error}"))
    }

    fn lyrics_loading_state() -> AppState {
        start_lyrics_test_state("loading").0
    }

    fn lyrics_test_state(document: Option<LyricsDocument>, position_ms: u64) -> AppState {
        lyrics_test_state_with_config(Config::default(), document, position_ms)
    }

    fn render_state_for_test(
        state: &AppState,
        width: u16,
        height: u16,
    ) -> Result<String, Box<dyn Error>> {
        let mut terminal = Terminal::new(TestBackend::new(width, height))?;
        terminal.draw(|frame| {
            render_with_model(frame, state, &Theme::default(), &RenderModel::default());
        })?;
        Ok(terminal.backend().to_string())
    }

    fn lyrics_test_state_with_config(
        config: Config,
        document: Option<LyricsDocument>,
        position_ms: u64,
    ) -> AppState {
        let (state, generation, media_id) = start_lyrics_test_state_with_config("render", config);
        let (state, _) = reduce(
            state,
            Action::LyricsCompleted {
                generation,
                media_id: media_id.clone().into(),
                result: Ok(document),
            },
        );
        let playback_generation = state
            .current_attempt_generation()
            .unwrap_or_else(|| panic!("lyrics render fixture playback generation"));
        reduce(
            state,
            Action::PlayerProgress {
                generation: playback_generation,
                media_id,
                position_ms,
                duration_ms: Some(180_000),
            },
        )
        .0
    }

    fn playing_test_state(state: AppState) -> AppState {
        let generation = state
            .current_attempt_generation()
            .unwrap_or_else(|| panic!("playing fixture generation"));
        reduce(
            state,
            Action::PlayerStatusChanged {
                generation,
                status: PlaybackStatus::Playing,
            },
        )
        .0
    }

    fn start_lyrics_test_state(label: &str) -> (AppState, crate::app::Generation, MediaId) {
        start_lyrics_test_state_with_config(label, Config::default())
    }

    fn start_lyrics_test_state_with_config(
        label: &str,
        config: Config,
    ) -> (AppState, crate::app::Generation, MediaId) {
        let item = MediaItem {
            id: MediaId {
                provider: "youtube-music".to_owned(),
                video_id: format!("lyrics-{label}"),
            },
            kind: MediaKind::Video,
            title: "Lyrics render fixture".to_owned(),
            creators: vec!["Artist".to_owned()],
            collection: None,
            duration_ms: Some(180_000),
            artwork_url: Some(
                url::Url::parse("https://art.invalid/lyrics-render")
                    .unwrap_or_else(|error| panic!("artwork fixture: {error}")),
            ),
            explicit: false,
        };
        let media_id = item.id.clone();
        let (state, effects) = reduce(
            AppState::new(config),
            Action::SearchSubmitted {
                query: "lyrics".to_owned(),
                filter: SearchFilter::Songs,
            },
        );
        let [Effect::Search { generation, .. }] = effects.as_slice() else {
            panic!("lyrics render fixture search effect");
        };
        let (state, _) = reduce(
            state,
            Action::SearchCompleted {
                generation: *generation,
                result: Ok(SearchPage::new(vec![SearchItem::Playable(item)])),
            },
        );
        let (state, effects) = reduce(state, Action::ActivateSearchResult { index: 0 });
        let Some(generation) = effects.iter().find_map(|effect| match effect {
            Effect::LoadLyrics { generation, .. } => Some(*generation),
            _ => None,
        }) else {
            panic!("lyrics render fixture load effect");
        };
        (state, generation, media_id)
    }

    fn spectrum_key(state: &AppState, bands: u16, rows: u8) -> Result<SpectrumKey, Box<dyn Error>> {
        let generation = state
            .current_attempt_generation()
            .ok_or("spectrum fixture playback generation")?;
        let media_id = state
            .playback()
            .current
            .clone()
            .ok_or("spectrum fixture current media")?;
        let target = SpectrumTarget::new(bands, rows).ok_or("spectrum fixture target")?;
        Ok(SpectrumKey::new(generation, media_id, target))
    }

    fn spectrum_fixture(
        state: &AppState,
        bands: u16,
        rows: u8,
        seed: &[u8],
    ) -> Result<(SpectrumFrameStore, SpectrumPresentation), Box<dyn Error>> {
        let key = spectrum_key(state, bands, rows)?;
        let store = SpectrumFrameStore::new();
        let run = store.request(key.clone()).ok_or("spectrum fixture run")?;
        let levels = (0..usize::from(bands))
            .map(|index| seed[index % seed.len()])
            .collect::<Vec<_>>();
        let frame = Arc::new(
            SpectrumFrame::new(levels.into_boxed_slice()).ok_or("spectrum fixture frame")?,
        );
        assert!(store.publish(&run, frame));
        let presentation = store.presentation(&key);
        Ok((store, presentation))
    }

    fn render_plain_model(
        state: &AppState,
        width: u16,
        height: u16,
    ) -> Result<String, Box<dyn Error>> {
        let mut terminal = Terminal::new(TestBackend::new(width, height))?;
        terminal.draw(|frame| {
            render_with_model(frame, state, &Theme::default(), &RenderModel::default());
        })?;
        Ok(terminal.backend().to_string())
    }

    fn render_spectrum_presentation(
        state: &AppState,
        presentation: &SpectrumPresentation,
        width: u16,
        height: u16,
    ) -> Result<String, Box<dyn Error>> {
        let mut terminal = Terminal::new(TestBackend::new(width, height))?;
        terminal.draw(|frame| {
            render_with_model_and_spectrum(
                frame,
                state,
                &Theme::default(),
                &RenderModel::default(),
                presentation,
            );
        })?;
        Ok(terminal.backend().to_string())
    }

    fn render_spectrum_buffer(
        state: &AppState,
        presentation: &SpectrumPresentation,
        width: u16,
        height: u16,
        theme: &Theme,
    ) -> Result<ratatui::buffer::Buffer, Box<dyn Error>> {
        let mut terminal = Terminal::new(TestBackend::new(width, height))?;
        terminal.draw(|frame| {
            render_with_model_and_spectrum(
                frame,
                state,
                theme,
                &RenderModel::default(),
                presentation,
            );
        })?;
        Ok(terminal.backend().buffer().clone())
    }

    fn render_spectrum_and_artwork(
        state: &AppState,
        spectrum: &SpectrumPresentation,
        artwork: &ArtworkPresentation,
        width: u16,
        height: u16,
    ) -> Result<String, Box<dyn Error>> {
        let mut terminal = Terminal::new(TestBackend::new(width, height))?;
        terminal.draw(|frame| {
            render_with_model_inner(
                frame,
                state,
                &Theme::default(),
                &RenderModel::default(),
                RenderEnhancements::new(Some(artwork), Some(spectrum), 15),
                &mut ViewportMemory::default(),
                None,
            );
        })?;
        Ok(terminal.backend().to_string())
    }

    fn render_spectrum_with_viewports(
        state: &AppState,
        model: &RenderModel,
        spectrum: &SpectrumPresentation,
        width: u16,
        height: u16,
        viewports: &mut ViewportMemory,
    ) -> Result<String, Box<dyn Error>> {
        let mut terminal = Terminal::new(TestBackend::new(width, height))?;
        terminal.draw(|frame| {
            render_with_model_and_viewports(
                frame,
                state,
                &Theme::default(),
                model,
                RenderEnhancements::new(None, Some(spectrum), 15),
                viewports,
            );
        })?;
        Ok(terminal.backend().to_string())
    }

    fn line_index_containing(rendered: &str, needle: &str) -> Result<usize, Box<dyn Error>> {
        rendered
            .lines()
            .position(|line| line.contains(needle))
            .ok_or_else(|| format!("missing {needle:?} in:\n{rendered}").into())
    }

    fn queue_item(id: &str) -> QueueItem {
        QueueItem::new(
            id,
            MediaItem {
                id: MediaId {
                    provider: "youtube-music".to_owned(),
                    video_id: id.to_owned(),
                },
                kind: MediaKind::Song,
                title: format!("Song {id}"),
                creators: vec!["Artist".to_owned()],
                collection: None,
                duration_ms: Some(180_000),
                artwork_url: None,
                explicit: false,
            },
        )
    }

    fn media_with_title(title: &str) -> MediaItem {
        MediaItem {
            id: MediaId {
                provider: "youtube-music".to_owned(),
                video_id: "huge".to_owned(),
            },
            kind: MediaKind::Song,
            title: title.to_owned(),
            creators: vec!["Artist".to_owned()],
            collection: None,
            duration_ms: None,
            artwork_url: None,
            explicit: false,
        }
    }

    fn indexed_media(prefix: &str, count: usize) -> Vec<MediaItem> {
        (0..count)
            .map(|index| queue_item(&format!("{prefix}-{index}")).media().clone())
            .collect()
    }

    fn loaded_podcast_recommendations(
        requested_region: &str,
        effective_region: &str,
        rows: &[(&str, &str, &str)],
    ) -> Result<AppState, Box<dyn Error>> {
        let requested_region = RegionCode::parse(requested_region)?;
        let (state, effects) = reduce(
            AppState::default(),
            Action::PodcastRecommendationsRequested {
                region: requested_region.clone(),
            },
        );
        let [Effect::LoadPodcastRecommendations { generation, .. }] = effects.as_slice() else {
            panic!("recommendation request effect");
        };
        let page = podcast_recommendation_page(effective_region, rows)?;
        let (state, _) = reduce(
            state,
            Action::PodcastRecommendationsCompleted {
                generation: *generation,
                requested_region,
                result: Ok(page),
            },
        );
        Ok(state)
    }

    fn podcast_recommendation_page(
        region: &str,
        rows: &[(&str, &str, &str)],
    ) -> Result<PodcastRecommendationPage, Box<dyn Error>> {
        let results = rows
            .iter()
            .map(|(id, title, publisher)| {
                serde_json::json!({"id": id, "name": title, "artistName": publisher})
            })
            .collect::<Vec<_>>();
        let bytes = serde_json::to_vec(&serde_json::json!({
            "feed": {"country": region, "results": results}
        }))?;
        Ok(parse_apple_top_shows(&bytes)?)
    }

    fn line_text(lines: &[Line<'_>]) -> String {
        lines
            .iter()
            .map(line_text_ref)
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn line_text_ref(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>()
    }

    fn assert_selected_row(
        lines: &[Line<'static>],
        row_limit: usize,
        available_width: usize,
        expected: &str,
    ) {
        assert!(lines.len() <= row_limit);
        assert_precomposed_lines_are_bounded(lines, available_width);
        let rendered = line_text(lines);
        assert!(
            rendered.contains(expected),
            "missing selected row `{expected}` in:\n{rendered}"
        );
    }

    fn assert_precomposed_lines_are_bounded(lines: &[Line<'static>], width: usize) {
        for line in lines {
            let bytes = line
                .spans
                .iter()
                .map(|span| span.content.len())
                .sum::<usize>();
            let cells = line
                .spans
                .iter()
                .map(|span| usize::from(span.content.as_ref().cell_width()))
                .sum::<usize>();
            assert!(
                bytes <= CLIP_BYTE_INSPECTION_BUDGET + 128,
                "line was fully composed before clipping: {bytes} bytes"
            );
            assert!(cells <= width, "line exceeds its {width}-cell viewport");
        }
    }
}
