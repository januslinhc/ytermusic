use std::{collections::BTreeSet, error::Error};

use proptest::{
    prelude::*,
    test_runner::{Config as ProptestConfig, RngAlgorithm, RngSeed, TestCaseError},
};
use rand::{SeedableRng, seq::SliceRandom};
use rand_chacha::ChaCha8Rng;
use ytermusic::{
    domain::{MediaId, MediaItem, MediaKind, RepeatMode},
    queue::{
        MAX_EXPLICIT_LIST_ITEMS, Queue, QueueError, QueueItem, QueueItemId, QueueReplacementError,
        QueueSnapshot, stable_queue_item_id,
    },
};

fn media_item(id: &str) -> MediaItem {
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
    }
}

fn item(id: &str) -> QueueItem {
    QueueItem::new(id, media_item(id))
}

fn logical_ids(queue: &Queue) -> Vec<String> {
    queue
        .items()
        .iter()
        .map(|item| item.id().as_str().to_owned())
        .collect()
}

fn active_ids(queue: &Queue) -> Vec<String> {
    queue
        .active_ids()
        .iter()
        .map(|id| id.as_str().to_owned())
        .collect()
}

fn active_item_ids(queue: &Queue) -> Vec<String> {
    queue
        .active_items()
        .map(|item| item.id().as_str().to_owned())
        .collect()
}

fn current_id(queue: &Queue) -> Option<String> {
    queue.current().map(|item| item.id().as_str().to_owned())
}

fn active_media_ids(queue: &Queue) -> Vec<MediaId> {
    queue
        .active_items()
        .map(|item| item.media().id.clone())
        .collect()
}

#[test]
fn explicit_list_deduplicates_full_media_ids_in_source_order() -> Result<(), QueueReplacementError>
{
    let first = media_item("same");
    let mut duplicate = first.clone();
    duplicate.title = "Duplicate row".to_owned();
    let mut other_provider = media_item("same");
    other_provider.id.provider = "podcast-provider".to_owned();
    let tail = media_item("tail");

    let queue = Queue::from_explicit_list(
        vec![
            first.clone(),
            duplicate,
            other_provider.clone(),
            tail.clone(),
        ],
        &other_provider.id,
        RepeatMode::Off,
        None,
    )?;

    let logical_media_ids: Vec<_> = queue
        .items()
        .iter()
        .map(|item| item.media().id.clone())
        .collect();
    assert_eq!(
        logical_media_ids,
        vec![first.id, other_provider.id, tail.id]
    );
    Ok(())
}

#[test]
fn explicit_list_rejects_a_selected_item_missing_after_deduplication() {
    let missing = MediaId {
        provider: "youtube-music".to_owned(),
        video_id: "missing".to_owned(),
    };

    assert!(matches!(
        Queue::from_explicit_list(
            vec![media_item("a"), media_item("b")],
            &missing,
            RepeatMode::Off,
            None,
        ),
        Err(QueueReplacementError::SelectedItemNotFound { id }) if id == missing
    ));
}

#[test]
fn explicit_list_shuffle_keeps_selected_first_and_randomizes_only_the_remainder()
-> Result<(), QueueReplacementError> {
    let items: Vec<_> = ["a", "b", "c", "d", "e"]
        .into_iter()
        .map(media_item)
        .collect();
    let selected = items[2].id.clone();

    let queue = Queue::from_explicit_list(items.clone(), &selected, RepeatMode::Off, Some(42))?;

    let mut expected: Vec<_> = items
        .into_iter()
        .map(|item| item.id)
        .filter(|id| id != &selected)
        .collect();
    expected.shuffle(&mut ChaCha8Rng::seed_from_u64(42));
    expected.insert(0, selected.clone());
    assert_eq!(active_media_ids(&queue), expected);
    assert_eq!(
        queue.current().map(|item| &item.media().id),
        Some(&selected)
    );
    assert!(queue.is_shuffled());
    Ok(())
}

#[test]
fn explicit_list_preserves_the_requested_repeat_mode() -> Result<(), QueueReplacementError> {
    let selected = media_item("b").id;

    let queue = Queue::from_explicit_list(
        vec![media_item("a"), media_item("b")],
        &selected,
        RepeatMode::All,
        None,
    )?;

    assert_eq!(queue.repeat(), RepeatMode::All);
    Ok(())
}

#[test]
fn explicit_list_always_disables_endless_radio() -> Result<(), QueueReplacementError> {
    let selected = media_item("a").id;

    let queue = Queue::from_explicit_list(vec![media_item("a")], &selected, RepeatMode::One, None)?;

    assert!(!queue.radio_enabled());
    Ok(())
}

#[test]
fn explicit_list_accepts_1024_unique_items_and_rejects_1025() -> Result<(), QueueReplacementError> {
    let items: Vec<_> = (0..=MAX_EXPLICIT_LIST_ITEMS)
        .map(|index| media_item(&format!("item-{index}")))
        .collect();
    let selected = items[0].id.clone();

    let queue = Queue::from_explicit_list(
        items[..MAX_EXPLICIT_LIST_ITEMS].to_vec(),
        &selected,
        RepeatMode::Off,
        None,
    )?;
    assert_eq!(queue.items().len(), MAX_EXPLICIT_LIST_ITEMS);

    assert!(matches!(
        Queue::from_explicit_list(items, &selected, RepeatMode::Off, None),
        Err(QueueReplacementError::TooManyItems {
            actual,
            limit: MAX_EXPLICIT_LIST_ITEMS,
        }) if actual == MAX_EXPLICIT_LIST_ITEMS + 1
    ));
    Ok(())
}

#[test]
fn explicit_list_selects_a_non_first_item_without_reordering_the_unshuffled_list()
-> Result<(), QueueReplacementError> {
    let items: Vec<_> = ["a", "b", "c"].into_iter().map(media_item).collect();
    let selected = items[1].id.clone();

    let queue = Queue::from_explicit_list(items.clone(), &selected, RepeatMode::Off, None)?;

    let logical_media_ids: Vec<_> = queue
        .items()
        .iter()
        .map(|item| item.media().id.clone())
        .collect();
    assert_eq!(
        logical_media_ids,
        items.into_iter().map(|item| item.id).collect::<Vec<_>>()
    );
    assert_eq!(
        queue.current().map(|item| &item.media().id),
        Some(&selected)
    );
    assert_eq!(
        queue.current().map(QueueItem::id),
        Some(&stable_queue_item_id(&selected))
    );
    assert!(!queue.is_shuffled());
    Ok(())
}

#[test]
fn explicit_list_applies_the_item_cap_after_full_media_id_deduplication()
-> Result<(), QueueReplacementError> {
    let unique: Vec<_> = (0..MAX_EXPLICIT_LIST_ITEMS)
        .map(|index| media_item(&format!("item-{index}")))
        .collect();
    let selected = unique[0].id.clone();
    let mut rows = unique.clone();
    rows.extend(unique.iter().take(32).cloned());

    let queue = Queue::from_explicit_list(rows, &selected, RepeatMode::Off, None)?;

    assert_eq!(queue.items().len(), MAX_EXPLICIT_LIST_ITEMS);
    Ok(())
}

#[test]
fn sequential_navigation_and_selection_use_stable_ids() -> Result<(), QueueError> {
    let mut queue = Queue::from_items(vec![item("a"), item("b"), item("c")])?;

    assert_eq!(current_id(&queue).as_deref(), Some("a"));
    assert_eq!(queue.next().map(|item| item.id().as_str()), Some("b"));
    assert_eq!(queue.next().map(|item| item.id().as_str()), Some("c"));
    assert_eq!(queue.previous().map(|item| item.id().as_str()), Some("b"));

    queue.select(&QueueItemId::from("a"))?;

    assert_eq!(current_id(&queue).as_deref(), Some("a"));
    Ok(())
}

#[test]
fn repeat_one_replays_the_current_item_in_both_directions() -> Result<(), QueueError> {
    let mut queue = Queue::from_items(vec![item("a"), item("b"), item("c")])?;
    queue.select(&QueueItemId::from("b"))?;
    queue.set_repeat(RepeatMode::One);

    assert_eq!(queue.next().map(|item| item.id().as_str()), Some("b"));
    assert_eq!(queue.previous().map(|item| item.id().as_str()), Some("b"));
    assert_eq!(queue.repeat(), RepeatMode::One);
    Ok(())
}

#[test]
fn repeat_all_wraps_at_both_edges() -> Result<(), QueueError> {
    let mut queue = Queue::from_items(vec![item("a"), item("b"), item("c")])?;
    queue.set_repeat(RepeatMode::All);
    queue.select(&QueueItemId::from("c"))?;

    assert_eq!(queue.next().map(|item| item.id().as_str()), Some("a"));

    queue.select(&QueueItemId::from("a"))?;

    assert_eq!(queue.previous().map(|item| item.id().as_str()), Some("c"));
    Ok(())
}

#[test]
fn repeat_off_returns_none_at_edges_without_changing_current() -> Result<(), QueueError> {
    let mut queue = Queue::from_items(vec![item("a"), item("b"), item("c")])?;
    queue.select(&QueueItemId::from("c"))?;
    let at_end = queue.snapshot();

    assert!(queue.next().is_none());
    assert_eq!(queue.snapshot(), at_end);

    queue.select(&QueueItemId::from("a"))?;
    let at_beginning = queue.snapshot();

    assert!(queue.previous().is_none());
    assert_eq!(queue.snapshot(), at_beginning);
    Ok(())
}

#[test]
fn seeded_shuffle_is_deterministic_complete_and_keeps_current_first() -> Result<(), QueueError> {
    let items = vec![item("a"), item("b"), item("c"), item("d"), item("e")];
    let mut first = Queue::from_items(items.clone())?;
    let mut second = Queue::from_items(items)?;
    first.select(&QueueItemId::from("b"))?;
    second.select(&QueueItemId::from("b"))?;

    first.set_shuffle(true, 42);
    second.set_shuffle(true, 42);

    let mut expected = vec![
        "a".to_owned(),
        "c".to_owned(),
        "d".to_owned(),
        "e".to_owned(),
    ];
    expected.shuffle(&mut ChaCha8Rng::seed_from_u64(42));
    expected.insert(0, "b".to_owned());

    assert_eq!(active_ids(&first), expected);
    assert_eq!(active_ids(&first), active_ids(&second));
    assert_eq!(current_id(&first).as_deref(), Some("b"));
    assert!(first.is_shuffled());

    let unique: BTreeSet<_> = active_ids(&first).into_iter().collect();
    assert_eq!(
        unique,
        BTreeSet::from_iter(["a", "b", "c", "d", "e"].map(str::to_owned))
    );
    Ok(())
}

#[test]
fn setting_the_same_shuffle_seed_is_idempotent() -> Result<(), QueueError> {
    let mut queue = Queue::from_items(vec![item("a"), item("b"), item("c"), item("d")])?;
    queue.set_shuffle(true, 7);
    let first_order = active_ids(&queue);

    queue.set_shuffle(true, 7);

    assert_eq!(active_ids(&queue), first_order);
    Ok(())
}

#[test]
fn disabling_shuffle_restores_logical_order_without_changing_current() -> Result<(), QueueError> {
    let mut queue = Queue::from_items(vec![item("a"), item("b"), item("c"), item("d")])?;
    queue.select(&QueueItemId::from("c"))?;
    queue.set_shuffle(true, 91);

    queue.set_shuffle(false, 91);

    assert_eq!(active_ids(&queue), vec!["a", "b", "c", "d"]);
    assert_eq!(current_id(&queue).as_deref(), Some("c"));
    assert!(!queue.is_shuffled());
    Ok(())
}

#[test]
fn active_items_follow_active_order_across_mutation_and_restore() -> Result<(), Box<dyn Error>> {
    let mut queue = Queue::from_items(vec![item("a"), item("b"), item("c"), item("d")])?;
    queue.select(&QueueItemId::from("c"))?;
    queue.set_shuffle(true, 13);

    assert_ne!(logical_ids(&queue), active_ids(&queue));
    assert_eq!(active_item_ids(&queue), active_ids(&queue));

    queue.append(item("e"))?;
    assert_eq!(active_item_ids(&queue), active_ids(&queue));

    queue.remove(&QueueItemId::from("b"))?;
    assert_eq!(active_item_ids(&queue), active_ids(&queue));

    queue.move_before(&QueueItemId::from("d"), &QueueItemId::from("a"))?;
    assert_eq!(active_item_ids(&queue), active_ids(&queue));

    let restored = Queue::restore(queue.snapshot())?;
    assert_eq!(active_item_ids(&restored), active_ids(&restored));

    let decoded: Queue = serde_json::from_str(&serde_json::to_string(&queue)?)?;
    assert_eq!(active_item_ids(&decoded), active_ids(&decoded));
    Ok(())
}

#[test]
fn append_rejects_duplicates_while_append_unique_skips_them() -> Result<(), QueueError> {
    let mut queue = Queue::from_items(vec![item("a")])?;

    assert!(matches!(
        queue.append(item("a")),
        Err(QueueError::DuplicateLogicalId { id }) if id.as_str() == "a"
    ));
    assert!(!queue.append_unique(item("a")));
    assert!(queue.append_unique(item("b")));
    assert_eq!(logical_ids(&queue), vec!["a", "b"]);
    assert_eq!(active_ids(&queue), vec!["a", "b"]);
    Ok(())
}

#[test]
fn appending_to_an_empty_queue_selects_the_new_item() -> Result<(), QueueError> {
    let mut queue = Queue::from_items(Vec::new())?;

    queue.append(item("a"))?;

    assert_eq!(current_id(&queue).as_deref(), Some("a"));
    Ok(())
}

#[test]
fn clear_empties_queue_content_and_resets_modes() -> Result<(), QueueError> {
    let mut queue = Queue::from_items(vec![item("a"), item("b")])?;
    queue.set_repeat(RepeatMode::All);
    queue.set_shuffle(true, 8);
    queue.set_radio(true);

    queue.clear();

    assert!(queue.items().is_empty());
    assert!(queue.active_ids().is_empty());
    assert!(queue.current().is_none());
    assert_eq!(queue.repeat(), RepeatMode::Off);
    assert!(!queue.is_shuffled());
    assert!(!queue.radio_enabled());
    Ok(())
}

#[test]
fn removing_items_around_current_preserves_current() -> Result<(), QueueError> {
    let mut queue = Queue::from_items(vec![item("a"), item("b"), item("c"), item("d"), item("e")])?;
    queue.select(&QueueItemId::from("c"))?;

    queue.remove(&QueueItemId::from("a"))?;
    queue.remove(&QueueItemId::from("e"))?;

    assert_eq!(current_id(&queue).as_deref(), Some("c"));
    assert_eq!(logical_ids(&queue), vec!["b", "c", "d"]);
    Ok(())
}

#[test]
fn removing_current_selects_next_then_previous_then_none() -> Result<(), QueueError> {
    let mut queue = Queue::from_items(vec![item("a"), item("b"), item("c"), item("d")])?;
    queue.select(&QueueItemId::from("b"))?;

    queue.remove(&QueueItemId::from("b"))?;
    assert_eq!(current_id(&queue).as_deref(), Some("c"));

    queue.select(&QueueItemId::from("d"))?;
    queue.remove(&QueueItemId::from("d"))?;
    assert_eq!(current_id(&queue).as_deref(), Some("c"));

    let mut singleton = Queue::from_items(vec![item("only")])?;
    singleton.remove(&QueueItemId::from("only"))?;
    assert!(singleton.current().is_none());
    Ok(())
}

#[test]
fn removing_the_current_shuffled_tail_selects_the_previous_active_item() -> Result<(), QueueError> {
    let mut queue = Queue::from_items(vec![item("a"), item("b"), item("c"), item("d")])?;
    queue.set_shuffle(true, 37);
    let tail_index = queue.active_ids().len() - 1;
    let tail = queue.active_ids()[tail_index].clone();
    let expected = queue.active_ids()[tail_index - 1].clone();
    queue.select(&tail)?;

    queue.remove(&tail)?;

    assert_eq!(current_id(&queue).as_deref(), Some(expected.as_str()));
    Ok(())
}

#[test]
fn move_before_updates_logical_and_active_orders_even_when_shuffled() -> Result<(), QueueError> {
    let mut queue = Queue::from_items(vec![item("a"), item("b"), item("c"), item("d")])?;
    queue.select(&QueueItemId::from("c"))?;

    queue.move_before(&QueueItemId::from("d"), &QueueItemId::from("b"))?;
    assert_eq!(logical_ids(&queue), vec!["a", "d", "b", "c"]);
    assert_eq!(active_ids(&queue), vec!["a", "d", "b", "c"]);

    queue.set_shuffle(true, 13);
    let mut expected_active = active_ids(&queue);
    let moved_position = expected_active
        .iter()
        .position(|id| id == "a")
        .ok_or_else(|| QueueError::ItemNotFound {
            id: QueueItemId::from("a"),
        })?;
    let moved = expected_active.remove(moved_position);
    let target_position = expected_active
        .iter()
        .position(|id| id == "c")
        .ok_or_else(|| QueueError::ItemNotFound {
            id: QueueItemId::from("c"),
        })?;
    expected_active.insert(target_position, moved);

    queue.move_before(&QueueItemId::from("a"), &QueueItemId::from("c"))?;

    assert_eq!(logical_ids(&queue), vec!["d", "b", "a", "c"]);
    assert_eq!(active_ids(&queue), expected_active);
    assert_eq!(current_id(&queue).as_deref(), Some("c"));
    Ok(())
}

#[test]
fn radio_fill_is_based_on_the_strict_count_after_current() -> Result<(), QueueError> {
    let mut queue = Queue::from_items(vec![item("a"), item("b"), item("c")])?;
    queue.select(&QueueItemId::from("b"))?;

    assert!(!queue.needs_radio_fill(2));

    queue.set_radio(true);

    assert!(!queue.needs_radio_fill(1));
    assert!(queue.needs_radio_fill(2));
    assert!(!queue.append_unique(item("c")));
    assert!(queue.append_unique(item("d")));
    assert!(!queue.needs_radio_fill(2));
    assert!(queue.needs_radio_fill(3));
    Ok(())
}

#[test]
fn snapshot_and_queue_serialization_round_trip() -> Result<(), Box<dyn Error>> {
    let mut queue = Queue::from_items(vec![item("a"), item("b"), item("c"), item("d")])?;
    queue.select(&QueueItemId::from("c"))?;
    queue.set_repeat(RepeatMode::All);
    queue.set_shuffle(true, 123);
    queue.set_radio(true);
    let expected = queue.snapshot();

    let snapshot_json = serde_json::to_string(&expected)?;
    let decoded_snapshot: QueueSnapshot = serde_json::from_str(&snapshot_json)?;
    let restored = Queue::restore(decoded_snapshot)?;

    assert_eq!(restored.snapshot(), expected);

    let queue_json = serde_json::to_string(&queue)?;
    let decoded_queue: Queue = serde_json::from_str(&queue_json)?;
    assert_eq!(decoded_queue.snapshot(), queue.snapshot());
    assert_eq!(
        serde_json::to_value(&queue)?,
        serde_json::to_value(queue.snapshot())?,
        "derived lookup state must not enter the durable queue format"
    );
    Ok(())
}

#[test]
fn restore_rejects_duplicate_logical_ids() -> Result<(), QueueError> {
    let queue = Queue::from_items(vec![item("a"), item("b")])?;
    let mut snapshot = queue.snapshot();
    snapshot.logical.push(item("a"));

    assert!(matches!(
        Queue::restore(snapshot),
        Err(QueueError::DuplicateLogicalId { id }) if id.as_str() == "a"
    ));
    Ok(())
}

#[test]
fn restore_rejects_active_ids_that_are_not_an_exact_permutation() -> Result<(), QueueError> {
    let queue = Queue::from_items(vec![item("a"), item("b"), item("c")])?;
    let snapshot = queue.snapshot();

    let mut duplicate = snapshot.clone();
    duplicate.active = vec![
        QueueItemId::from("a"),
        QueueItemId::from("a"),
        QueueItemId::from("c"),
    ];
    assert!(matches!(
        Queue::restore(duplicate),
        Err(QueueError::DuplicateActiveId { id }) if id.as_str() == "a"
    ));

    let mut unknown = snapshot.clone();
    unknown.active = vec![
        QueueItemId::from("a"),
        QueueItemId::from("b"),
        QueueItemId::from("unknown"),
    ];
    assert!(matches!(
        Queue::restore(unknown),
        Err(QueueError::ActiveIdNotFound { id }) if id.as_str() == "unknown"
    ));

    let mut missing = snapshot;
    missing.active.pop();
    assert!(matches!(
        Queue::restore(missing),
        Err(QueueError::ActiveIdsMismatch {
            logical_count: 3,
            active_count: 2
        })
    ));
    Ok(())
}

#[test]
fn restore_rejects_permuted_active_order_when_shuffle_is_disabled() -> Result<(), QueueError> {
    let queue = Queue::from_items(vec![item("a"), item("b"), item("c")])?;
    let mut snapshot = queue.snapshot();
    snapshot.active.swap(0, 1);

    assert!(matches!(
        Queue::restore(snapshot),
        Err(QueueError::UnshuffledOrderMismatch {
            index: 0,
            logical_id,
            active_id,
        }) if logical_id.as_str() == "a" && active_id.as_str() == "b"
    ));
    Ok(())
}

#[test]
fn queue_deserialization_rejects_permuted_active_order_when_shuffle_is_disabled()
-> Result<(), Box<dyn Error>> {
    let queue = Queue::from_items(vec![item("a"), item("b"), item("c")])?;
    let mut snapshot = queue.snapshot();
    snapshot.active.swap(0, 1);
    let json = serde_json::to_string(&snapshot)?;

    assert!(
        serde_json::from_str::<Queue>(&json).is_err(),
        "unshuffled queue deserialization accepted inconsistent active order"
    );
    Ok(())
}

#[test]
fn valid_unshuffled_snapshot_still_restores() -> Result<(), QueueError> {
    let queue = Queue::from_items(vec![item("a"), item("b"), item("c")])?;
    let expected = queue.snapshot();

    let restored = Queue::restore(expected.clone())?;

    assert_eq!(restored.snapshot(), expected);
    Ok(())
}

#[test]
fn valid_shuffled_snapshot_still_restores() -> Result<(), QueueError> {
    let mut queue = Queue::from_items(vec![item("a"), item("b"), item("c")])?;
    queue.set_shuffle(true, 73);
    let expected = queue.snapshot();

    let restored = Queue::restore(expected.clone())?;

    assert_eq!(restored.snapshot(), expected);
    Ok(())
}

#[test]
fn restore_rejects_invalid_or_missing_current_ids() -> Result<(), QueueError> {
    let queue = Queue::from_items(vec![item("a"), item("b")])?;
    let snapshot = queue.snapshot();

    let mut unknown = snapshot.clone();
    unknown.current = Some(QueueItemId::from("unknown"));
    assert!(matches!(
        Queue::restore(unknown),
        Err(QueueError::CurrentIdNotFound { id }) if id.as_str() == "unknown"
    ));

    let mut missing = snapshot;
    missing.current = None;
    assert!(matches!(
        Queue::restore(missing),
        Err(QueueError::MissingCurrent)
    ));
    Ok(())
}

#[derive(Clone, Debug)]
enum Edit {
    Append(usize),
    Remove(usize),
    Select(usize),
    Move {
        item_index: usize,
        target_index: usize,
    },
    Shuffle {
        enabled: bool,
        seed: u64,
    },
}

fn edit_strategy() -> impl Strategy<Value = Edit> {
    prop_oneof![
        any::<usize>().prop_map(Edit::Append),
        any::<usize>().prop_map(Edit::Remove),
        any::<usize>().prop_map(Edit::Select),
        (any::<usize>(), any::<usize>()).prop_map(|(item_index, target_index)| Edit::Move {
            item_index,
            target_index,
        }),
        (any::<bool>(), any::<u64>()).prop_map(|(enabled, seed)| Edit::Shuffle { enabled, seed }),
    ]
}

fn property_error(error: &QueueError) -> TestCaseError {
    TestCaseError::fail(error.to_string())
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 128,
        failure_persistence: None,
        rng_algorithm: RngAlgorithm::ChaCha,
        rng_seed: RngSeed::Fixed(0x5155_EE00),
        ..ProptestConfig::default()
    })]

    #[test]
    fn arbitrary_valid_edit_sequences_preserve_queue_invariants(
        id_pool in prop::collection::btree_set(0_u8..64, 1..20),
        edits in prop::collection::vec(edit_strategy(), 0..100),
    ) {
        let mut queue =
            Queue::from_items(Vec::new()).map_err(|error| property_error(&error))?;
        let ids: Vec<_> = id_pool.into_iter().collect();

        for edit in edits {
            match edit {
                Edit::Append(index) => {
                    let id = format!("id-{}", ids[index % ids.len()]);
                    let _ = queue.append_unique(item(&id));
                }
                Edit::Remove(index) if !queue.items().is_empty() => {
                    let id = queue.items()[index % queue.items().len()].id().clone();
                    queue
                        .remove(&id)
                        .map_err(|error| property_error(&error))?;
                }
                Edit::Select(index) if !queue.active_ids().is_empty() => {
                    let id = queue.active_ids()[index % queue.active_ids().len()].clone();
                    queue
                        .select(&id)
                        .map_err(|error| property_error(&error))?;
                }
                Edit::Move {
                    item_index,
                    target_index,
                } if queue.items().len() > 1 => {
                    let len = queue.items().len();
                    let moved_index = item_index % len;
                    let mut before_index = target_index % (len - 1);
                    if before_index >= moved_index {
                        before_index += 1;
                    }
                    let moved_id = queue.items()[moved_index].id().clone();
                    let before_id = queue.items()[before_index].id().clone();
                    queue
                        .move_before(&moved_id, &before_id)
                        .map_err(|error| property_error(&error))?;
                }
                Edit::Shuffle { enabled, seed } => queue.set_shuffle(enabled, seed),
                Edit::Remove(_)
                | Edit::Select(_)
                | Edit::Move { .. } => {}
            }

            let logical = logical_ids(&queue);
            let active = active_ids(&queue);
            let active_items = active_item_ids(&queue);
            let logical_set: BTreeSet<_> = logical.iter().cloned().collect();
            let active_set: BTreeSet<_> = active.iter().cloned().collect();

            prop_assert_eq!(logical.len(), logical_set.len());
            prop_assert_eq!(active.len(), active_set.len());
            prop_assert_eq!(&active_items, &active);
            prop_assert_eq!(&active_set, &logical_set);

            if logical.is_empty() {
                prop_assert!(queue.current().is_none());
            } else {
                let current = current_id(&queue)
                    .ok_or_else(|| TestCaseError::fail("non-empty queue lost its current item"))?;
                prop_assert!(logical_set.contains(&current));
                prop_assert!(active_set.contains(&current));
            }
        }
    }
}
