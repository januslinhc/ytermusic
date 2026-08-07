use std::{
    collections::{HashMap, HashSet},
    fmt,
};

use rand::{SeedableRng, seq::SliceRandom};
use rand_chacha::ChaCha8Rng;
use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

use crate::domain::{MediaId, MediaItem, RepeatMode};

pub const MAX_EXPLICIT_LIST_ITEMS: usize = 1_024;

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Deserialize, Serialize)]
pub struct QueueItemId(String);

impl QueueItemId {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for QueueItemId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl From<&str> for QueueItemId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl From<String> for QueueItemId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct QueueItem {
    id: QueueItemId,
    media: MediaItem,
}

impl QueueItem {
    #[must_use]
    pub fn new(id: impl Into<QueueItemId>, media: MediaItem) -> Self {
        Self {
            id: id.into(),
            media,
        }
    }

    #[must_use]
    pub fn id(&self) -> &QueueItemId {
        &self.id
    }

    #[must_use]
    pub fn media(&self) -> &MediaItem {
        &self.media
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct QueueSnapshot {
    pub logical: Vec<QueueItem>,
    pub active: Vec<QueueItemId>,
    pub current: Option<QueueItemId>,
    pub repeat: RepeatMode,
    pub shuffle_seed: Option<u64>,
    pub radio: bool,
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum QueueError {
    #[error("logical queue contains duplicate item id `{id}`")]
    DuplicateLogicalId { id: QueueItemId },
    #[error("queue item id `{id}` was not found")]
    ItemNotFound { id: QueueItemId },
    #[error("active queue contains duplicate item id `{id}`")]
    DuplicateActiveId { id: QueueItemId },
    #[error("active queue references unknown item id `{id}`")]
    ActiveIdNotFound { id: QueueItemId },
    #[error(
        "active queue is not a permutation of the logical queue: \
         {active_count} active ids for {logical_count} logical items"
    )]
    ActiveIdsMismatch {
        logical_count: usize,
        active_count: usize,
    },
    #[error(
        "unshuffled active id `{active_id}` at index {index} does not match \
         logical id `{logical_id}`"
    )]
    UnshuffledOrderMismatch {
        index: usize,
        logical_id: QueueItemId,
        active_id: QueueItemId,
    },
    #[error("current queue item id `{id}` is not present")]
    CurrentIdNotFound { id: QueueItemId },
    #[error("a non-empty queue must have a current item")]
    MissingCurrent,
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum QueueReplacementError {
    #[error("explicit list contains {actual} unique items; the limit is {limit}")]
    TooManyItems { actual: usize, limit: usize },
    #[error("selected media item was not found in the explicit list")]
    SelectedItemNotFound { id: MediaId },
    #[error(transparent)]
    InvalidQueue(#[from] QueueError),
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Queue {
    logical: Vec<QueueItem>,
    active: Vec<QueueItemId>,
    #[serde(skip)]
    logical_indices: HashMap<QueueItemId, usize>,
    current: Option<QueueItemId>,
    repeat: RepeatMode,
    shuffle_seed: Option<u64>,
    radio: bool,
}

impl Default for Queue {
    fn default() -> Self {
        Self {
            logical: Vec::new(),
            active: Vec::new(),
            logical_indices: HashMap::new(),
            current: None,
            repeat: RepeatMode::Off,
            shuffle_seed: None,
            radio: false,
        }
    }
}

impl<'de> Deserialize<'de> for Queue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let snapshot = QueueSnapshot::deserialize(deserializer)?;
        Self::restore(snapshot).map_err(serde::de::Error::custom)
    }
}

impl Queue {
    /// Builds a fresh queue from an explicitly activated list.
    ///
    /// Media items are de-duplicated by their full provider identity in source
    /// order. The selected item becomes current, repeat mode is preserved,
    /// endless radio is disabled, and an optional seed enables deterministic
    /// shuffle while keeping the selected item first in active order.
    ///
    /// # Errors
    ///
    /// Returns [`QueueReplacementError`] if the de-duplicated list exceeds
    /// [`MAX_EXPLICIT_LIST_ITEMS`], the selected media ID is absent, or the
    /// resulting queue violates an internal queue invariant.
    pub fn from_explicit_list(
        items: Vec<MediaItem>,
        selected: &MediaId,
        repeat: RepeatMode,
        shuffle_seed: Option<u64>,
    ) -> Result<Self, QueueReplacementError> {
        let mut seen = HashSet::with_capacity(items.len());
        let mut logical = Vec::with_capacity(items.len().min(MAX_EXPLICIT_LIST_ITEMS));

        for media in items {
            if seen.insert(media.id.clone()) {
                let id = stable_queue_item_id(&media.id);
                logical.push(QueueItem::new(id, media));
            }
        }

        if logical.len() > MAX_EXPLICIT_LIST_ITEMS {
            return Err(QueueReplacementError::TooManyItems {
                actual: logical.len(),
                limit: MAX_EXPLICIT_LIST_ITEMS,
            });
        }

        let selected_id = stable_queue_item_id(selected);
        if !seen.contains(selected) {
            return Err(QueueReplacementError::SelectedItemNotFound {
                id: selected.clone(),
            });
        }

        let mut candidate = Self::from_items(logical)?;
        candidate.select(&selected_id)?;
        candidate.set_repeat(repeat);
        if let Some(seed) = shuffle_seed {
            candidate.set_shuffle(true, seed);
        }
        candidate.set_radio(false);

        Self::restore(candidate.snapshot()).map_err(QueueReplacementError::from)
    }

    /// Creates a queue in logical order, selecting its first item when non-empty.
    ///
    /// # Errors
    ///
    /// Returns [`QueueError::DuplicateLogicalId`] if two items have the same
    /// stable queue ID.
    pub fn from_items(items: Vec<QueueItem>) -> Result<Self, QueueError> {
        let active: Vec<_> = items.iter().map(|item| item.id.clone()).collect();
        let current = active.first().cloned();

        Self::restore(QueueSnapshot {
            logical: items,
            active,
            current,
            repeat: RepeatMode::Off,
            shuffle_seed: None,
            radio: false,
        })
    }

    #[must_use]
    pub fn items(&self) -> &[QueueItem] {
        &self.logical
    }

    #[must_use]
    pub fn current(&self) -> Option<&QueueItem> {
        let current = self.current.as_ref()?;
        let index = self.logical_indices.get(current)?;
        self.logical.get(*index)
    }

    #[must_use]
    pub fn active_ids(&self) -> &[QueueItemId] {
        &self.active
    }

    /// Iterates queue items in the order used for playback.
    ///
    /// This differs from [`Self::items`] while shuffle is enabled.
    #[must_use]
    pub fn active_items(
        &self,
    ) -> impl DoubleEndedIterator<Item = &QueueItem> + ExactSizeIterator + '_ {
        self.active
            .iter()
            .map(|id| &self.logical[self.logical_indices[id]])
    }

    /// Appends an item to both logical and active order.
    ///
    /// # Errors
    ///
    /// Returns [`QueueError::DuplicateLogicalId`] if the stable ID is already
    /// present.
    pub fn append(&mut self, item: QueueItem) -> Result<(), QueueError> {
        if self.contains(item.id()) {
            return Err(QueueError::DuplicateLogicalId {
                id: item.id.clone(),
            });
        }

        self.push_item(item);
        Ok(())
    }

    #[must_use]
    pub fn append_unique(&mut self, item: QueueItem) -> bool {
        if self.contains(item.id()) {
            return false;
        }

        self.push_item(item);
        true
    }

    /// Removes an item and keeps the current selection stable where possible.
    ///
    /// If the current item is removed, the next item at the same active
    /// position is selected, falling back to the previous item at the end.
    ///
    /// # Errors
    ///
    /// Returns [`QueueError::ItemNotFound`] if `id` is not in the queue.
    pub fn remove(&mut self, id: &QueueItemId) -> Result<QueueItem, QueueError> {
        let logical_index = self
            .logical_indices
            .get(id)
            .copied()
            .ok_or_else(|| QueueError::ItemNotFound { id: id.clone() })?;
        let active_index = self
            .active
            .iter()
            .position(|active_id| active_id == id)
            .ok_or_else(|| QueueError::ItemNotFound { id: id.clone() })?;
        let removed_current = self.current.as_ref() == Some(id);

        let removed = self.logical.remove(logical_index);
        self.active.remove(active_index);
        let _ = self.logical_indices.remove(id);
        self.reindex_logical_from(logical_index);

        if removed_current {
            self.current = self
                .active
                .get(active_index)
                .or_else(|| self.active.last())
                .cloned();
        }

        Ok(removed)
    }

    /// Moves one item immediately before another in logical and active order.
    ///
    /// # Errors
    ///
    /// Returns [`QueueError::ItemNotFound`] if either ID is not in the queue.
    pub fn move_before(
        &mut self,
        id: &QueueItemId,
        before: &QueueItemId,
    ) -> Result<(), QueueError> {
        let logical_from = self
            .logical_indices
            .get(id)
            .copied()
            .ok_or_else(|| QueueError::ItemNotFound { id: id.clone() })?;
        let logical_before = self
            .logical_indices
            .get(before)
            .copied()
            .ok_or_else(|| QueueError::ItemNotFound { id: before.clone() })?;
        let active_from = self
            .active
            .iter()
            .position(|active_id| active_id == id)
            .ok_or_else(|| QueueError::ItemNotFound { id: id.clone() })?;
        let active_before = self
            .active
            .iter()
            .position(|active_id| active_id == before)
            .ok_or_else(|| QueueError::ItemNotFound { id: before.clone() })?;

        if id != before {
            move_entry_before(&mut self.logical, logical_from, logical_before);
            move_entry_before(&mut self.active, active_from, active_before);
            self.reindex_logical_from(logical_from.min(logical_before));
        }

        Ok(())
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// Selects an item by its stable ID.
    ///
    /// # Errors
    ///
    /// Returns [`QueueError::ItemNotFound`] if `id` is not active.
    pub fn select(&mut self, id: &QueueItemId) -> Result<(), QueueError> {
        if !self.active.iter().any(|active_id| active_id == id) {
            return Err(QueueError::ItemNotFound { id: id.clone() });
        }

        self.current = Some(id.clone());
        Ok(())
    }

    #[allow(
        clippy::should_implement_trait,
        reason = "queue navigation is required API, not iterator consumption"
    )]
    pub fn next(&mut self) -> Option<&QueueItem> {
        if self.repeat == RepeatMode::One {
            return self.current();
        }

        let next_index = match self.current_active_index() {
            Some(index) if index + 1 < self.active.len() => index + 1,
            Some(_) if self.repeat == RepeatMode::All => 0,
            Some(_) => return None,
            None => 0,
        };
        self.select_active_index(next_index)
    }

    pub fn previous(&mut self) -> Option<&QueueItem> {
        if self.repeat == RepeatMode::One {
            return self.current();
        }

        let previous_index = match self.current_active_index() {
            Some(index) if index > 0 => index - 1,
            Some(_) if self.repeat == RepeatMode::All => self.active.len().checked_sub(1)?,
            Some(_) => return None,
            None => self.active.len().checked_sub(1)?,
        };
        self.select_active_index(previous_index)
    }

    pub fn set_repeat(&mut self, repeat: RepeatMode) {
        self.repeat = repeat;
    }

    #[must_use]
    pub fn repeat(&self) -> RepeatMode {
        self.repeat
    }

    pub fn set_shuffle(&mut self, enabled: bool, seed: u64) {
        if !enabled {
            self.active = self.logical.iter().map(|item| item.id.clone()).collect();
            self.shuffle_seed = None;
            return;
        }

        if self.shuffle_seed == Some(seed) {
            return;
        }

        let mut active: Vec<_> = self
            .logical
            .iter()
            .map(|item| item.id.clone())
            .filter(|id| self.current.as_ref() != Some(id))
            .collect();
        active.shuffle(&mut ChaCha8Rng::seed_from_u64(seed));

        if let Some(current) = &self.current {
            active.insert(0, current.clone());
        }

        self.active = active;
        self.shuffle_seed = Some(seed);
    }

    #[must_use]
    pub fn is_shuffled(&self) -> bool {
        self.shuffle_seed.is_some()
    }

    pub fn set_radio(&mut self, enabled: bool) {
        self.radio = enabled;
    }

    #[must_use]
    pub fn radio_enabled(&self) -> bool {
        self.radio
    }

    #[must_use]
    pub fn needs_radio_fill(&self, threshold: usize) -> bool {
        if !self.radio {
            return false;
        }

        let remaining = self
            .current_active_index()
            .map_or(self.active.len(), |index| self.active.len() - index - 1);
        remaining < threshold
    }

    #[must_use]
    pub fn snapshot(&self) -> QueueSnapshot {
        QueueSnapshot {
            logical: self.logical.clone(),
            active: self.active.clone(),
            current: self.current.clone(),
            repeat: self.repeat,
            shuffle_seed: self.shuffle_seed,
            radio: self.radio,
        }
    }

    /// Restores a queue after validating all persisted invariants.
    ///
    /// # Errors
    ///
    /// Returns a typed [`QueueError`] for duplicate logical or active IDs,
    /// inconsistent active state for the persisted mode, or invalid current
    /// selection.
    pub fn restore(snapshot: QueueSnapshot) -> Result<Self, QueueError> {
        let logical_indices = validate_snapshot(&snapshot)?;

        Ok(Self {
            logical: snapshot.logical,
            active: snapshot.active,
            logical_indices,
            current: snapshot.current,
            repeat: snapshot.repeat,
            shuffle_seed: snapshot.shuffle_seed,
            radio: snapshot.radio,
        })
    }

    fn contains(&self, id: &QueueItemId) -> bool {
        self.logical_indices.contains_key(id)
    }

    fn push_item(&mut self, item: QueueItem) {
        let id = item.id.clone();
        let logical_index = self.logical.len();
        self.logical.push(item);
        self.logical_indices.insert(id.clone(), logical_index);
        self.active.push(id.clone());

        if self.current.is_none() {
            self.current = Some(id);
        }
    }

    fn current_active_index(&self) -> Option<usize> {
        let current = self.current.as_ref()?;
        self.active.iter().position(|id| id == current)
    }

    fn select_active_index(&mut self, index: usize) -> Option<&QueueItem> {
        self.current = self.active.get(index).cloned();
        self.current()
    }

    fn reindex_logical_from(&mut self, start: usize) {
        for (index, item) in self.logical.iter().enumerate().skip(start) {
            self.logical_indices.insert(item.id().clone(), index);
        }
    }
}

#[must_use]
pub fn stable_queue_item_id(media_id: &MediaId) -> QueueItemId {
    QueueItemId::from(format!(
        "media:{}:{}:{}:{}",
        media_id.provider.len(),
        media_id.provider,
        media_id.video_id.len(),
        media_id.video_id
    ))
}

fn move_entry_before<T>(entries: &mut Vec<T>, from: usize, before: usize) {
    let entry = entries.remove(from);
    let insertion_index = if from < before { before - 1 } else { before };
    entries.insert(insertion_index, entry);
}

fn validate_snapshot(snapshot: &QueueSnapshot) -> Result<HashMap<QueueItemId, usize>, QueueError> {
    let mut logical_indices = HashMap::with_capacity(snapshot.logical.len());
    for (index, item) in snapshot.logical.iter().enumerate() {
        if logical_indices.insert(item.id().clone(), index).is_some() {
            return Err(QueueError::DuplicateLogicalId {
                id: item.id().clone(),
            });
        }
    }

    let mut active_ids = HashSet::with_capacity(snapshot.active.len());
    for id in &snapshot.active {
        if !active_ids.insert(id.clone()) {
            return Err(QueueError::DuplicateActiveId { id: id.clone() });
        }
        if !logical_indices.contains_key(id) {
            return Err(QueueError::ActiveIdNotFound { id: id.clone() });
        }
    }

    if snapshot.active.len() != snapshot.logical.len() {
        return Err(QueueError::ActiveIdsMismatch {
            logical_count: snapshot.logical.len(),
            active_count: snapshot.active.len(),
        });
    }

    if snapshot.shuffle_seed.is_none() {
        for (index, (item, active_id)) in snapshot.logical.iter().zip(&snapshot.active).enumerate()
        {
            if item.id() != active_id {
                return Err(QueueError::UnshuffledOrderMismatch {
                    index,
                    logical_id: item.id().clone(),
                    active_id: active_id.clone(),
                });
            }
        }
    }

    match &snapshot.current {
        Some(id) if !logical_indices.contains_key(id) => {
            Err(QueueError::CurrentIdNotFound { id: id.clone() })
        }
        None if !snapshot.logical.is_empty() => Err(QueueError::MissingCurrent),
        _ => Ok(logical_indices),
    }
}
