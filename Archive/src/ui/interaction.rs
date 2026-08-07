use std::{
    fmt,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use ratatui::layout::Rect;

use super::{
    input::SemanticAction,
    render::{DatasetKey, NavigationItem},
};

/// Maximum number of hit regions retained for one rendered frame.
pub const MAX_INTERACTION_REGIONS: usize = 512;

/// Monotonic identity for one attempted terminal frame.
#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct FrameRevision(u64);

impl FrameRevision {
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Advances without wrapping and aliasing an older frame.
    #[must_use]
    pub const fn next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    const fn value(self) -> u64 {
        self.0
    }
}

impl fmt::Debug for FrameRevision {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("FrameRevision")
            .field(&self.0)
            .finish()
    }
}

/// A bounded list surface identifier that never retains provider identity or content.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ListSurface {
    Home,
    Search,
    Charts,
    PodcastRecommendations,
    PodcastEpisodes,
    Library,
    Favorites,
    History,
    Queue,
    CommandPalette,
    CountryPicker,
    BrowserPicker,
    Lyrics,
}

/// UI-local location of a selectable row in a rendered line buffer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RenderedRowTarget {
    pub(crate) line_index: usize,
    pub(crate) surface: ListSurface,
    pub(crate) stable_index: usize,
    pub(crate) dataset_key: DatasetKey,
}

/// A semantic target whose payload is limited to stable UI-local coordinates.
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub enum HitTarget {
    Semantic(SemanticAction),
    Navigation(NavigationItem),
    ListRow {
        surface: ListSurface,
        stable_index: usize,
    },
    Progress {
        numerator: u16,
        denominator: u16,
    },
}

impl HitTarget {
    const fn is_valid(self) -> bool {
        match self {
            Self::Progress {
                numerator,
                denominator,
            } => denominator != 0 && numerator <= denominator,
            Self::Semantic(_) | Self::Navigation(_) | Self::ListRow { .. } => true,
        }
    }

    const fn category(self) -> &'static str {
        match self {
            Self::Semantic(_) => "semantic",
            Self::Navigation(_) => "navigation",
            Self::ListRow { .. } => "list_row",
            Self::Progress { .. } => "progress",
        }
    }
}

impl fmt::Debug for HitTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HitTarget")
            .field("category", &self.category())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy)]
struct HitRegion {
    area: Rect,
    target: HitTarget,
}

/// Mutable, bounded hit geometry for one frame under construction.
#[derive(Clone)]
pub struct InteractionMap {
    revision: FrameRevision,
    regions: Vec<HitRegion>,
}

impl InteractionMap {
    #[must_use]
    pub fn new(revision: FrameRevision) -> Self {
        Self {
            revision,
            regions: Vec::new(),
        }
    }

    #[must_use]
    pub const fn revision(&self) -> FrameRevision {
        self.revision
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.regions.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.regions.is_empty()
    }

    /// Removes all background regions when a modal layer replaces the frame.
    pub fn clear(&mut self) {
        self.regions.clear();
    }

    /// Adds a non-empty valid region unless this frame has reached its fixed cap.
    pub fn push(&mut self, area: Rect, target: HitTarget) -> bool {
        if area.is_empty() || !target.is_valid() || self.regions.len() == MAX_INTERACTION_REGIONS {
            return false;
        }
        self.regions.push(HitRegion { area, target });
        true
    }

    /// Clips a region to the visible frame before bounded insertion.
    pub fn push_clipped(&mut self, area: Rect, visible: Rect, target: HitTarget) -> bool {
        let Some(area) = intersect(area, visible) else {
            return false;
        };
        self.push(area, target)
    }

    /// Resolves only against this exact frame, with later regions treated as topmost.
    #[must_use]
    pub fn resolve(&self, column: u16, row: u16, revision: FrameRevision) -> Option<HitTarget> {
        if revision != self.revision {
            return None;
        }
        self.regions
            .iter()
            .rev()
            .find(|region| contains(region.area, column, row))
            .map(|region| region.target)
    }

    fn into_snapshot(self, current_revision: Arc<AtomicU64>) -> InteractionSnapshot {
        InteractionSnapshot {
            revision: self.revision,
            regions: self.regions.into_boxed_slice(),
            current_revision,
        }
    }
}

impl fmt::Debug for InteractionMap {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        debug_summary(formatter, "InteractionMap", self.revision, &self.regions)
    }
}

/// Immutable hit geometry retained only for the latest successful frame.
#[derive(Clone)]
pub struct InteractionSnapshot {
    revision: FrameRevision,
    regions: Box<[HitRegion]>,
    current_revision: Arc<AtomicU64>,
}

impl InteractionSnapshot {
    #[must_use]
    pub const fn revision(&self) -> FrameRevision {
        self.revision
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.regions.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.regions.is_empty()
    }

    #[must_use]
    pub fn resolve(&self, column: u16, row: u16) -> Option<HitTarget> {
        if self.current_revision.load(Ordering::Acquire) != self.revision.value() {
            return None;
        }
        self.regions
            .iter()
            .rev()
            .find(|region| contains(region.area, column, row))
            .map(|region| region.target)
    }
}

impl fmt::Debug for InteractionSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        debug_summary(
            formatter,
            "InteractionSnapshot",
            self.revision,
            &self.regions,
        )
    }
}

/// Owns the one-frame publication lifecycle and invalidates geometry while drawing.
pub struct InteractionStore {
    next_revision: Option<FrameRevision>,
    in_progress: Option<FrameRevision>,
    latest: Option<InteractionSnapshot>,
    current_revision: Arc<AtomicU64>,
}

impl Default for InteractionStore {
    fn default() -> Self {
        Self {
            next_revision: Some(FrameRevision::new(1)),
            in_progress: None,
            latest: None,
            current_revision: Arc::new(AtomicU64::new(0)),
        }
    }
}

impl InteractionStore {
    /// Invalidates the published map before returning a fresh frame map.
    pub fn begin_frame(&mut self) -> Option<InteractionMap> {
        self.invalidate();
        let revision = self.next_revision?;
        self.next_revision = revision.next();
        self.in_progress = Some(revision);
        Some(InteractionMap::new(revision))
    }

    /// Publishes only the current in-progress frame, rejecting older completions.
    pub fn publish(&mut self, map: InteractionMap) -> bool {
        if self.in_progress != Some(map.revision()) {
            return false;
        }
        let revision = map.revision();
        self.in_progress = None;
        self.latest = Some(map.into_snapshot(Arc::clone(&self.current_revision)));
        self.current_revision
            .store(revision.value(), Ordering::Release);
        true
    }

    /// Invalidates all borrowed or owned snapshots from the previous frame.
    pub fn invalidate(&mut self) {
        self.current_revision.store(0, Ordering::Release);
        self.latest = None;
        self.in_progress = None;
    }

    #[must_use]
    pub const fn latest(&self) -> Option<&InteractionSnapshot> {
        self.latest.as_ref()
    }
}

impl fmt::Debug for InteractionStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InteractionStore")
            .field("has_next_revision", &self.next_revision.is_some())
            .field("in_progress", &self.in_progress)
            .field("latest", &self.latest)
            .finish_non_exhaustive()
    }
}

impl Drop for InteractionStore {
    fn drop(&mut self) {
        self.current_revision.store(0, Ordering::Release);
    }
}

fn contains(area: Rect, column: u16, row: u16) -> bool {
    column >= area.x
        && u32::from(column) < u32::from(area.x) + u32::from(area.width)
        && row >= area.y
        && u32::from(row) < u32::from(area.y) + u32::from(area.height)
}

fn intersect(left: Rect, right: Rect) -> Option<Rect> {
    let x = left.x.max(right.x);
    let y = left.y.max(right.y);
    let right_edge = (u32::from(left.x) + u32::from(left.width))
        .min(u32::from(right.x) + u32::from(right.width));
    let bottom_edge = (u32::from(left.y) + u32::from(left.height))
        .min(u32::from(right.y) + u32::from(right.height));
    let width = right_edge.checked_sub(u32::from(x))?;
    let height = bottom_edge.checked_sub(u32::from(y))?;
    if width == 0 || height == 0 {
        return None;
    }
    Some(Rect::new(
        x,
        y,
        u16::try_from(width).ok()?,
        u16::try_from(height).ok()?,
    ))
}

fn debug_summary(
    formatter: &mut fmt::Formatter<'_>,
    name: &str,
    revision: FrameRevision,
    regions: &[HitRegion],
) -> fmt::Result {
    let mut semantic_count = 0_usize;
    let mut navigation_count = 0_usize;
    let mut list_row_count = 0_usize;
    let mut progress_count = 0_usize;
    for region in regions {
        match region.target {
            HitTarget::Semantic(_) => semantic_count += 1,
            HitTarget::Navigation(_) => navigation_count += 1,
            HitTarget::ListRow { .. } => list_row_count += 1,
            HitTarget::Progress { .. } => progress_count += 1,
        }
    }
    formatter
        .debug_struct(name)
        .field("revision", &revision)
        .field("region_count", &regions.len())
        .field("semantic_count", &semantic_count)
        .field("navigation_count", &navigation_count)
        .field("list_row_count", &list_row_count)
        .field("progress_count", &progress_count)
        .finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exhausted_store_rejects_a_late_max_revision_frame() {
        let maximum = FrameRevision::new(u64::MAX);
        let mut store = InteractionStore {
            next_revision: Some(maximum),
            in_progress: None,
            latest: None,
            current_revision: Arc::new(AtomicU64::new(0)),
        };
        let Some(frame) = store.begin_frame() else {
            panic!("maximum revision should be issued once");
        };

        assert!(store.begin_frame().is_none());
        assert!(!store.publish(frame));
        assert!(store.latest().is_none());
    }
}
