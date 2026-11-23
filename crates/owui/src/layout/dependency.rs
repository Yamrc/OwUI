//! Parent-committed child uses, size-group receipts and probe/attempt bookkeeping.

use std::collections::HashMap;

use crate::element::mounted::{ElementAccess, MountedElements, Row};
use crate::element::{ElementError, ElementId, Visibility};
use crate::id::LocalKey;
use crate::schedule::Work;

use super::measure::measure;
use super::{Constraints, Layout, MeasureKey, MeasureMode, Point, Size};

// Only local rollback fields are retained, on the Layout-owned attempt stack.
// Published bounds/clip and full measure keys are not copied into recursive frames.
pub(super) struct ParentAttempt {
    pub(super) key: LocalKey,
    snapshot: LayoutSnapshot,
    pub(super) uses: Uses,
    pub(super) group_key: Option<GroupKey>,
    pub(super) retained: Option<RetainedUses>,
    pub(super) tainted: bool,
}

pub(super) struct LayoutSnapshot {
    origin: Point,
    size: Size,
    content_size: Size,
    clip_children: bool,
    hit_testable: bool,
    content_set: bool,
}

#[derive(Clone, Copy)]
pub(crate) struct Contribution {
    pub(crate) constraints: Constraints,
    pub(crate) size: Size,
    pub(crate) occupied: bool,
    pub(crate) valid: bool,
    pub(crate) measured: bool,
    pub(crate) visibility: Option<Visibility>,
}

/// A copied size and participation contribution from a size-only child use.
///
/// A contribution can be staged for the executing parent or retained from its last
/// successful use. It is not the child's latest arbitrary eager measurement and is
/// not an independent cache handle. `occupied` is false for a Collapsed contribution;
/// Hidden children still occupy layout space.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SizeContribution {
    pub size: Size,
    pub occupied: bool,
}

/// One direct child's first parent-committed baseline in an unfinished size group.
///
/// Repeated invalidations preserve this baseline until the parent successfully
/// commits a new use. It permits old/current size and occupancy comparisons even
/// when several child changes cancel in a block total. `position` belongs to the
/// plan's direct-child order; changing that order invalidates the copied position.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ChildSizeChange {
    pub child: ElementId,
    pub position: usize,
    /// A past parent contribution, not permission to bypass current measurement.
    pub previous: Option<SizeContribution>,
}

/// Whether a single-key container can reuse its preceding committed child group.
///
/// A plan is evidence for one executing layout attempt, not a persistent identity.
/// See [`size_plan`](crate::layout::LayoutCx::size_plan) for entry order and fallback, and
/// [`size_inputs_current`](crate::layout::LayoutCx::size_inputs_current) before trusting a derived State cache.
#[derive(Debug)]
pub enum SizePlan {
    /// Reconstruct all current child uses before establishing a new single-key group.
    Rebuild,

    /// Retain unchanged uses and process actual unfinished changes without rediscovery.
    ///
    /// Entries are not promised to be sorted. New callback work is distinct from this
    /// entered set and is not acknowledged by clearing it.
    Incremental { changed: Vec<ChildSizeChange> },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct GroupKey {
    pub(super) constraints: Constraints,
    pub(super) viewport_epoch: u64,
    pub(super) output_epoch: u64,
}

#[derive(Default)]
pub(super) struct Uses {
    pub(super) keys: Vec<LocalKey>,
    pub(super) values: HashMap<LocalKey, Contribution>,
    allocated: usize,
}

impl Contribution {
    fn output(self) -> SizeContribution {
        SizeContribution {
            size: self.size,
            occupied: self.occupied,
        }
    }
}

impl Uses {
    fn insert(&mut self, key: LocalKey, contribution: Contribution) {
        if let Some(old) = self.values.get_mut(&key) {
            if !contribution.measured {
                old.visibility = contribution.visibility;
            } else {
                let valid = contribution.valid
                    && (!old.measured
                        || (old.valid && old.constraints == contribution.constraints));
                *old = Contribution {
                    valid,
                    visibility: old.visibility.or(contribution.visibility),
                    ..contribution
                };
            }
        } else {
            self.keys.push(key);
            self.values.insert(key, contribution);
        }

        self.allocated = self.allocated.max(self.values.capacity());
    }

    fn clear(&mut self) {
        for key in self.keys.drain(..) {
            self.values.remove(&key);
        }
    }

    pub(super) fn bytes(&self) -> usize {
        self.allocated
            .max(self.values.capacity())
            .saturating_mul(size_of::<(LocalKey, Contribution)>() + 1)
            .saturating_add(self.keys.capacity().saturating_mul(size_of::<LocalKey>()))
    }
}

#[derive(Clone, Copy)]
pub(super) struct DirtySize {
    index: usize,
    previous: Option<SizeContribution>,
    current: Option<Contribution>,
}

#[derive(Default)]
pub(super) struct DirtySizes {
    pub(super) keys: Vec<LocalKey>,
    values: HashMap<LocalKey, DirtySize>,
    allocated: usize,
}

impl DirtySizes {
    fn take_cached(scratch: &mut Vec<Self>, members: usize) -> Self {
        let limit = members.saturating_mul(4).max(16);
        let selected = scratch
            .iter()
            .enumerate()
            .filter(|(_, dirty)| {
                let capacity = dirty
                    .allocated
                    .max(dirty.values.capacity())
                    .max(dirty.keys.capacity());
                capacity <= limit || dirty.bytes() <= 64 * 1024
            })
            .min_by_key(|(_, dirty)| {
                let capacity = dirty
                    .allocated
                    .max(dirty.values.capacity())
                    .min(dirty.keys.capacity());
                if capacity >= members {
                    (0, capacity - members)
                } else {
                    (1, members - capacity)
                }
            })
            .map(|(index, _)| index);
        selected
            .map(|index| scratch.swap_remove(index))
            .unwrap_or_default()
    }

    pub(super) fn bytes(&self) -> usize {
        self.allocated
            .max(self.values.capacity())
            .saturating_mul(size_of::<(LocalKey, DirtySize)>() + 1)
            .saturating_add(self.keys.capacity().saturating_mul(size_of::<LocalKey>()))
    }

    fn mark(&mut self, key: LocalKey, previous: Option<SizeContribution>) {
        if let Some(change) = self.values.get_mut(&key) {
            change.current = None;
        } else {
            let index = self.keys.len();
            self.keys.push(key);
            self.values.insert(
                key,
                DirtySize {
                    index,
                    previous,
                    current: None,
                },
            );
        }

        self.allocated = self.allocated.max(self.values.capacity());
    }

    pub(super) fn remove(&mut self, key: LocalKey) -> Option<DirtySize> {
        let removed = self.values.remove(&key)?;
        self.keys.swap_remove(removed.index);
        if let Some(&moved) = self.keys.get(removed.index) {
            self.values
                .get_mut(&moved)
                .expect("moved dirty keys retain their inverse entry")
                .index = removed.index;
        }

        Some(removed)
    }

    fn restore(&mut self, key: LocalKey, entered: DirtySize) {
        if let Some(current) = self.values.get_mut(&key) {
            current.previous = entered.previous;
            current.current = None;
        } else {
            let index = self.keys.len();
            self.keys.push(key);
            self.values.insert(key, DirtySize { index, ..entered });
        }

        self.allocated = self.allocated.max(self.values.capacity());
    }

    pub(super) fn trim(&mut self) {
        self.allocated = self.allocated.max(self.values.capacity());
        let oversized = self.allocated > self.keys.len().saturating_mul(4).max(16)
            && self
                .allocated
                .saturating_mul(size_of::<(LocalKey, DirtySize)>() + 1)
                > 64 * 1024;
        if oversized {
            if self.keys.is_empty() {
                self.values = HashMap::new();
            } else {
                self.values
                    .shrink_to(self.keys.len().saturating_mul(2).max(16));
            }
            self.allocated = self.values.capacity();
        }

        if self.keys.capacity() > self.keys.len().saturating_mul(4).max(16)
            && self.keys.capacity().saturating_mul(size_of::<LocalKey>()) > 64 * 1024
        {
            self.keys
                .shrink_to(self.keys.len().saturating_mul(2).max(16));
        }
    }
}

pub(super) struct SizeGroup {
    pub(super) key: GroupKey,
    pub(super) count: usize,
    pub(super) valid: bool,
    open: bool,
    pub(super) structure_dirty: bool,
    pub(super) dirty: DirtySizes,
    rollback_slots: usize,
    pub(super) trim_queued: bool,
}

impl SizeGroup {
    pub(super) fn take_idle_dirty(&mut self) -> Option<DirtySizes> {
        if self.open
            || self.rollback_slots != 0
            || !self.dirty.keys.is_empty()
            || !self.dirty.values.is_empty()
            || self.dirty.bytes() <= 64 * 1024
        {
            return None;
        }

        Some(std::mem::take(&mut self.dirty))
    }
}

pub(super) struct RetainedUses {
    key: GroupKey,
    pub(super) entered: DirtySizes,
    pub(super) bulk: bool,
}

pub(super) struct IncomingProbe {
    child: LocalKey,
    parent: Option<LocalKey>,
    entered: Option<DirtySize>,
}

impl Layout {
    pub(crate) fn note_work(&mut self, elements: &MountedElements, key: LocalKey, work: Work) {
        if work.intersects(Work::PREPARE | Work::LAYOUT) {
            self.note_configuration(elements, key);
        }
    }

    pub(crate) fn note_configuration(&mut self, elements: &MountedElements, key: LocalKey) {
        let Some(parent) = elements.parent(key) else {
            return;
        };

        let previous = self
            .dependencies
            .get(&key)
            .copied()
            .map(Contribution::output);
        let Some(group) = self
            .groups
            .get_mut(&parent)
            .filter(|group| group.valid || group.open)
        else {
            return;
        };

        if !group.open
            && group.rollback_slots == 0
            && group.dirty.keys.is_empty()
            && group.dirty.values.is_empty()
            && group.dirty.bytes() == 0
        {
            group.dirty =
                DirtySizes::take_cached(&mut self.dirty_scratch, elements.child_count(parent));
        }

        if !group.dirty.values.contains_key(&key) {
            let headroom = group.rollback_slots + 1;
            group.dirty.keys.reserve(headroom);
            group.dirty.values.reserve(headroom);
        }

        group.dirty.mark(key, previous);
        if !group.trim_queued {
            self.group_trim.push(parent);
            group.trim_queued = true;
        }
    }

    pub(crate) fn settle_incoming(
        &mut self,
        elements: &MountedElements,
        configuration: &crate::element::configuration::Configuration,
        work: &crate::schedule::Scheduler,
        child: LocalKey,
    ) {
        let Some(parent) = elements.parent(child) else {
            return;
        };
        if self
            .groups
            .get(&parent)
            .is_none_or(|group| !group.dirty.values.contains_key(&child))
        {
            return;
        }

        let Some(row) = elements.row_local(child) else {
            return;
        };
        let Some(contribution) = self.dependencies.get(&child).copied() else {
            return;
        };

        let current = configuration.checked(row)
            && !configuration.needs_delivery(child)
            && !work.recovery_required(row)
            && !work.pending(row).intersects(Work::PREPARE | Work::LAYOUT)
            && !self.records[row.0].parked
            && self.records[row.0].geometry.size == contribution.size
            && (configuration.read(row).visibility() != Visibility::Collapsed)
                == contribution.occupied
            && self.records[row.0].measurement.is_some_and(|key| {
                key.constraints == contribution.constraints
                    && key.viewport_epoch == self.viewport_epoch
            });

        if current && let Some(group) = self.groups.get_mut(&parent) {
            group.dirty.remove(child);
        }
    }

    pub(crate) fn invalidate_structure(&mut self, parent: LocalKey) {
        if let Some(group) = self.groups.get_mut(&parent) {
            group.valid = false;
            group.structure_dirty = true;
        }

        if let Some(attempt) = self
            .attempts
            .last_mut()
            .filter(|attempt| attempt.key == parent)
        {
            attempt.tainted = true;
        }
    }

    pub(crate) fn begin_probe(&mut self, child: LocalKey, elements: &MountedElements) -> usize {
        self.probes.reserve(1);
        let parent = elements.parent(child);
        let entered = parent
            .and_then(|parent| self.groups.get_mut(&parent))
            .and_then(|group| {
                let entered = group.dirty.remove(child);
                if entered.is_some() {
                    group.rollback_slots += 1;
                }
                entered
            });

        let index = self.probes.len();
        self.probes.push(IncomingProbe {
            child,
            parent,
            entered,
        });

        index
    }

    pub(crate) fn finish_probe(&mut self, index: usize, contribution: Contribution, current: bool) {
        assert_eq!(
            index + 1,
            self.probes.len(),
            "size probes follow nested measurement ownership"
        );

        let probe = self
            .probes
            .pop()
            .expect("active measurements own their incoming size probe");
        let Some(parent) = probe.parent else {
            return;
        };

        let in_parent = self.attempts.last().is_some_and(|attempt| {
            attempt.key == parent && attempt.uses.values.contains_key(&probe.child)
        });
        let Some(group) = self.groups.get_mut(&parent) else {
            return;
        };
        let Some(mut entered) = probe.entered else {
            return;
        };

        group.rollback_slots -= 1;
        if !group.valid && !group.open {
            return;
        }

        if group.dirty.values.contains_key(&probe.child) {
            entered.current = None;
            group.dirty.restore(probe.child, entered);
            return;
        }
        if in_parent && current {
            return;
        }

        let same_key = contribution.constraints == group.key.constraints
            && self.viewport_epoch == group.key.viewport_epoch;
        if current && same_key && entered.previous == Some(contribution.output()) {
            return;
        }

        entered.current = (current && same_key).then_some(contribution);
        group.dirty.restore(probe.child, entered);
    }

    pub(crate) fn fail_probe(&mut self, index: usize) {
        assert_eq!(
            index + 1,
            self.probes.len(),
            "failed size probes restore their own entry debt"
        );

        let probe = self
            .probes
            .pop()
            .expect("failed measurements own their incoming size probe");
        let Some(parent) = probe.parent else {
            return;
        };

        if let Some(group) = self.groups.get_mut(&parent) {
            group.valid = false;
            if let Some(mut entered) = probe.entered {
                group.rollback_slots -= 1;
                entered.current = None;
                group.dirty.restore(probe.child, entered);
            }
        }
    }

    // Heap-attempt construction must not inflate the persistently recursive measurement frame.
    #[inline(never)]
    pub(crate) fn begin_measure(&mut self, key: LocalKey, row: Row) -> usize {
        let record = &mut self.records[row.0];
        let snapshot = LayoutSnapshot {
            origin: record.geometry.origin,
            size: record.geometry.size,
            content_size: record.geometry.content_size,
            clip_children: record.clip_children,
            hit_testable: record.hit_testable,
            content_set: record.content_set,
        };

        let index = self.attempts.len();
        self.uses_scratch.reserve(self.attempts.len() + 1);
        self.sparse_uses_scratch.reserve(self.attempts.len() + 1);
        self.dirty_scratch.reserve(self.attempts.len() + 1);
        let uses = self.uses_scratch.pop().unwrap_or_default();
        self.attempts.push(ParentAttempt {
            key,
            snapshot,
            uses,
            group_key: None,
            retained: None,
            tainted: false,
        });

        record.measurement = None;
        record.demand = true;
        record.parked = false;
        record.laying_out = true;
        record.content_set = false;

        index
    }

    pub(super) fn plan_group(
        &mut self,
        parent: LocalKey,
        key: GroupKey,
        elements: &MountedElements,
        recovery: bool,
    ) -> SizePlan {
        let index = self.attempts.len() - 1;
        assert_eq!(
            self.attempts[index].key, parent,
            "size plans belong to the executing parent layout"
        );

        if self.attempts[index].group_key.is_some() {
            self.attempts[index].tainted = true;
            if let Some(group) = self.groups.get_mut(&parent) {
                group.valid = false;
            }
            return SizePlan::Rebuild;
        }

        let count = elements.child_count(parent);
        let preceding = &self.attempts[index].uses;
        let preused = !preceding.keys.is_empty();
        let conflicting = preceding.keys.iter().any(|child| {
            let use_ = &preceding.values[child];
            use_.measured && (!use_.valid || use_.constraints != key.constraints)
        });

        self.attempts[index].tainted |= conflicting;
        let reusable = !preused
            && !self.attempts[index].tainted
            && self.groups.get(&parent).is_some_and(|group| {
                group.valid
                    && !group.structure_dirty
                    && !group.open
                    && group.key == key
                    && group.count == count
                    && !recovery
                    && group.dirty.keys.iter().all(|child| {
                        elements.contains_local(*child) && elements.parent(*child) == Some(parent)
                    })
            });

        let group = self.groups.entry(parent).or_insert_with(|| SizeGroup {
            key,
            count,
            valid: false,
            open: false,
            structure_dirty: false,
            dirty: DirtySizes::default(),
            rollback_slots: 0,
            trim_queued: false,
        });
        let entered = std::mem::replace(
            &mut group.dirty,
            DirtySizes::take_cached(&mut self.dirty_scratch, count),
        );
        group.key = key;
        group.count = count;
        group.open = true;
        group.structure_dirty = false;
        group.valid = reusable;

        group.dirty.keys.reserve(group.rollback_slots + 1);
        group.dirty.values.reserve(group.rollback_slots + 1);
        if !group.trim_queued {
            self.group_trim.push(parent);
            group.trim_queued = true;
        }
        self.groups_capacity = self.groups_capacity.max(self.groups.capacity());

        let mut changes = Vec::new();
        if reusable {
            changes.reserve_exact(entered.keys.len());
            for &child in &entered.keys {
                let change = entered
                    .values
                    .get(&child)
                    .expect("entered group keys retain their comparison data");
                let row = elements
                    .row_local(child)
                    .expect("reusable group members retain their rows");
                changes.push(ChildSizeChange {
                    child: elements.id(child),
                    position: elements.nodes[row.0].position,
                    previous: change.previous,
                });
            }

            let previous = std::mem::replace(
                &mut self.attempts[index].uses,
                self.sparse_uses_scratch.pop().unwrap_or_default(),
            );
            self.recycle_uses(previous, false);
            for &child in &entered.keys {
                if let Some(contribution) =
                    entered.values.get(&child).and_then(|change| change.current)
                {
                    self.attempts[index].uses.insert(child, contribution);
                }
            }
        }

        self.attempts[index].group_key = Some(key);
        self.attempts[index].retained = Some(RetainedUses {
            key,
            entered,
            bulk: reusable,
        });

        if reusable {
            SizePlan::Incremental { changed: changes }
        } else {
            SizePlan::Rebuild
        }
    }

    pub(super) fn contribution(
        &self,
        parent: LocalKey,
        child: LocalKey,
        constraints: Constraints,
        configuration: &crate::element::configuration::Configuration,
        elements: &MountedElements,
        work: &crate::schedule::Scheduler,
    ) -> Option<SizeContribution> {
        let attempt = self
            .attempts
            .last()
            .filter(|attempt| attempt.key == parent)?;

        let row = elements.row_local(child)?;
        let parent_row = elements.row_local(parent)?;
        let parent_env = configuration.output(parent_row);
        let collapsed = configuration.read(row).visibility() == Visibility::Collapsed;
        let fresh = configuration.checked(parent_row)
            && configuration.input_current(row, &parent_env)
            && !self.records[row.0].parked
            && self.records[row.0].measurement.is_some_and(|key| {
                key.constraints == constraints && key.viewport_epoch == self.viewport_epoch
            })
            && (collapsed
                || (!configuration.needs_delivery(child)
                    && !work.pending(row).intersects(Work::PREPARE | Work::LAYOUT)
                    && !work.recovery_required(row)));

        if !fresh {
            return None;
        }
        if attempt
            .group_key
            .is_some_and(|key| configuration.output_epoch(parent_row) != key.output_epoch)
        {
            return None;
        }

        if let Some(contribution) = attempt.uses.values.get(&child) {
            return (contribution.valid && contribution.constraints == constraints)
                .then_some(contribution.output());
        }

        let retained = attempt.retained.as_ref().filter(|retained| retained.bulk)?;
        let group = self.groups.get(&parent)?;
        if attempt.tainted
            || group.structure_dirty
            || group.key != retained.key
            || retained.key.constraints != constraints
            || retained.key.viewport_epoch != self.viewport_epoch
            || retained.key.output_epoch != configuration.output_epoch(parent_row)
            || group.dirty.values.contains_key(&child)
        {
            return None;
        }

        if let Some(change) = retained.entered.values.get(&child) {
            return change
                .current
                .filter(|value| value.constraints == constraints && value.valid)
                .map(Contribution::output);
        }

        self.dependencies
            .get(&child)
            .copied()
            .filter(|value| value.valid && value.constraints == constraints)
            .map(Contribution::output)
    }

    pub(crate) fn finish_measure(
        &mut self,
        index: usize,
        constraints: Constraints,
        size: Size,
        mut current: bool,
        elements: &MountedElements,
        configuration: &crate::element::configuration::Configuration,
    ) -> (bool, u64) {
        assert_eq!(
            index + 1,
            self.attempts.len(),
            "measurement snapshots finish in nested call order"
        );

        let mut attempt = self
            .attempts
            .pop()
            .expect("completed parent owns its staged child uses");
        let row = elements
            .row_local(attempt.key)
            .expect("completed layout retains its identity row");

        let bulk = attempt
            .retained
            .as_ref()
            .is_some_and(|retained| retained.bulk);
        let count = elements.child_count(attempt.key);
        let mut group_valid = !attempt.tainted;

        if let Some(key) = attempt.group_key {
            let parent_row = elements
                .row_local(attempt.key)
                .expect("completed layout retains its parent identity");
            let group = self
                .groups
                .get(&attempt.key)
                .expect("size plans own an actual parent group");
            group_valid &= configuration.checked(parent_row);
            current &= !group.structure_dirty
                && group.dirty.keys.is_empty()
                && configuration.output_epoch(parent_row) == key.output_epoch
                && self.viewport_epoch == key.viewport_epoch;
            if bulk {
                let retained = attempt
                    .retained
                    .as_ref()
                    .expect("bulk groups own entered dirty comparison data");
                current &= retained
                    .entered
                    .keys
                    .iter()
                    .all(|child| attempt.uses.values.contains_key(child));
            }
        }

        if bulk {
            for &child in &attempt.uses.keys {
                let contribution = attempt
                    .uses
                    .values
                    .get(&child)
                    .copied()
                    .expect("staged use keys retain values");
                group_valid &= contribution.valid
                    && attempt
                        .group_key
                        .is_some_and(|key| contribution.constraints == key.constraints);
                if contribution.valid || contribution.visibility.is_some() {
                    self.dependencies.insert(child, contribution);
                } else {
                    self.dependencies.remove(&child);
                }
            }
        } else {
            let used_count = attempt.uses.keys.len();
            group_valid &= used_count == count;
            for child in elements.child_keys(attempt.key) {
                match attempt.uses.values.get(&child).copied() {
                    Some(contribution)
                        if contribution.valid || contribution.visibility.is_some() =>
                    {
                        group_valid &= contribution.valid
                            && attempt
                                .group_key
                                .is_some_and(|key| contribution.constraints == key.constraints);
                        self.dependencies.insert(child, contribution);
                    }
                    _ => {
                        self.dependencies.remove(&child);
                        group_valid = false;
                    }
                }
            }
        }

        self.dependency_capacity = self.dependency_capacity.max(self.dependencies.capacity());
        if let Some(group) = self.groups.get_mut(&attempt.key) {
            group.open = false;
            group.valid = attempt.group_key.is_some() && current && group_valid;
            group.count = count;
        }

        let record = &mut self.records[row.0];
        record.geometry.size = size;
        if !record.content_set {
            record.geometry.content_size = size;
        }
        record.measurement = current.then_some(MeasureKey {
            constraints,
            viewport_epoch: self.viewport_epoch,
        });
        record.laying_out = false;

        let inspected = (if bulk { attempt.uses.keys.len() } else { count })
            .saturating_add(attempt.uses.keys.len())
            .saturating_add(
                attempt
                    .retained
                    .as_ref()
                    .map_or(0, |retained| retained.entered.keys.len()),
            );

        if let Some(retained) = attempt.retained.take() {
            self.recycle_dirty(retained.entered);
        }
        self.recycle_uses(attempt.uses, bulk);

        (current, u64::try_from(inspected).unwrap_or(u64::MAX))
    }

    pub(crate) fn fail_measure(&mut self, index: usize, row: Row) {
        assert_eq!(
            index + 1,
            self.attempts.len(),
            "failed measurement restores its own nested snapshot"
        );

        let mut attempt = self
            .attempts
            .pop()
            .expect("an active measure owns its local snapshot");
        let snapshot = attempt.snapshot;

        self.dependency_capacity = self.dependency_capacity.max(self.dependencies.capacity());
        if let Some(group) = self.groups.get_mut(&attempt.key) {
            group.open = false;
            group.valid = false;
        }

        let bulk = attempt
            .retained
            .as_ref()
            .is_some_and(|retained| retained.bulk);
        if let Some(retained) = attempt.retained.take() {
            self.recycle_dirty(retained.entered);
        }
        self.recycle_uses(attempt.uses, bulk);

        let record = &mut self.records[row.0];
        record.geometry.origin = snapshot.origin;
        record.geometry.size = snapshot.size;
        record.geometry.content_size = snapshot.content_size;
        record.clip_children = snapshot.clip_children;
        record.hit_testable = snapshot.hit_testable;
        record.content_set = snapshot.content_set;
        record.measurement = None;
        record.laying_out = false;
    }

    fn recycle_uses(&mut self, mut uses: Uses, sparse: bool) {
        uses.clear();
        let bytes = uses.bytes();
        if sparse && bytes <= 64 * 1024 {
            self.sparse_uses_scratch.push(uses);
        } else if bytes <= 16 * 1024 * 1024 {
            self.uses_scratch.push(uses);
        }
    }

    pub(super) fn recycle_dirty(&mut self, mut dirty: DirtySizes) {
        let bytes = dirty.bytes();
        for key in dirty.keys.drain(..) {
            dirty.values.remove(&key);
        }

        if bytes != 0 && bytes <= 16 * 1024 * 1024 {
            self.dirty_scratch.push(dirty);
        }
    }

    pub(super) fn record_visibility(
        &mut self,
        parent: LocalKey,
        child: LocalKey,
        visibility: Visibility,
    ) {
        let Some(attempt) = self
            .attempts
            .last_mut()
            .filter(|attempt| attempt.key == parent)
        else {
            return;
        };

        let retained = attempt
            .retained
            .as_ref()
            .is_some_and(|retained| retained.bulk);
        let mut contribution = attempt
            .uses
            .values
            .get(&child)
            .copied()
            .or_else(|| {
                retained
                    .then(|| self.dependencies.get(&child).copied())
                    .flatten()
            })
            .unwrap_or(Contribution {
                constraints: Constraints::tight(Size::ZERO),
                size: Size::ZERO,
                occupied: visibility != Visibility::Collapsed,
                valid: false,
                measured: false,
                visibility: None,
            });

        contribution.visibility = Some(visibility);
        attempt.uses.insert(child, contribution);
    }

    pub(crate) fn has_visibility_use(&self, child: LocalKey) -> bool {
        self.dependencies
            .get(&child)
            .is_some_and(|use_| use_.visibility.is_some())
    }

    pub(crate) fn record_use(
        &mut self,
        parent: LocalKey,
        child: LocalKey,
        constraints: Constraints,
        mode: MeasureMode,
        size: Size,
        occupied: bool,
    ) {
        if let Some(attempt) = self
            .attempts
            .last_mut()
            .filter(|attempt| attempt.key == parent)
        {
            let next = Contribution {
                constraints,
                size,
                occupied,
                valid: mode == MeasureMode::SizeOnly,
                measured: true,
                visibility: None,
            };
            attempt.uses.insert(child, next);

            if attempt
                .group_key
                .is_some_and(|key| mode != MeasureMode::SizeOnly || key.constraints != constraints)
            {
                attempt.tainted = true;
                if let Some(group) = self.groups.get_mut(&parent) {
                    group.valid = false;
                }
            }
        } else if mode != MeasureMode::SizeOnly {
            self.dependencies.remove(&child);
            if let Some(group) = self.groups.get_mut(&parent) {
                group.valid = false;
            }
        }
    }

    pub(super) fn park_use(&mut self, parent: LocalKey, child: LocalKey) {
        let previous = self.dependencies.get(&child).copied();
        if let Some(group) = self.groups.get_mut(&parent) {
            group.valid = false;
            group.dirty.remove(child);
        }

        if let Some(attempt) = self
            .attempts
            .last_mut()
            .filter(|attempt| attempt.key == parent)
        {
            attempt.tainted = true;
            if let Some(mut contribution) = attempt.uses.values.get(&child).copied().or(previous) {
                contribution.valid = false;
                attempt.uses.insert(child, contribution);
            }
        }
    }
}

pub(crate) fn suppressed(
    elements: &MountedElements,
    configuration: &crate::element::configuration::Configuration,
    key: LocalKey,
) -> bool {
    let mut current = Some(key);
    while let Some(key) = current {
        let Some(row) = elements.row_local(key) else {
            return true;
        };
        if configuration.read(row).visibility() == Visibility::Collapsed {
            return true;
        }
        current = elements.parent(key);
    }

    false
}

/// Propagates one notification, using only a committed single-key size dependency.
pub(crate) fn propagate_one(
    access: &mut ElementAccess<'_>,
    key: LocalKey,
    row: Row,
) -> Result<(), ElementError> {
    let Some(parent) = access.elements.parent(key) else {
        return Ok(());
    };

    let parent_row = access
        .elements
        .row_local(parent)
        .expect("layout dependency retains its parent");
    if access.work.pending(parent_row).contains(Work::LAYOUT) {
        return Ok(());
    }

    if let Some(previous) = access
        .layout
        .dependencies
        .get(&key)
        .copied()
        .filter(|use_| use_.visibility.is_some())
    {
        access.work.counts.layout_dependency_checks = access
            .work
            .counts
            .layout_dependency_checks
            .saturating_add(1);
        if previous.visibility != Some(access.configuration.read(row).visibility()) {
            access.work.counts.layout_ancestors =
                access.work.counts.layout_ancestors.saturating_add(1);
            access.request(access.elements.id(parent), Work::LAYOUT);
            return Ok(());
        }
        if !previous.measured {
            return Ok(());
        }
    }

    if let Some(previous) = access
        .layout
        .dependencies
        .get(&key)
        .copied()
        .filter(|use_| use_.valid)
        && !access.work.recovery_required(parent_row)
        && access.layout.records[parent_row.0]
            .measurement
            .is_some_and(|key| key.viewport_epoch == access.layout.viewport_epoch())
        && !access.work.recovery_required(row)
    {
        access.work.counts.layout_dependency_checks = access
            .work
            .counts
            .layout_dependency_checks
            .saturating_add(1);

        let id = access.elements.id(key);
        let size = measure(access, id, previous.constraints, MeasureMode::SizeOnly)?;
        let occupied = access.configuration.read(row).visibility() != Visibility::Collapsed;

        // A probe is not a parent commit. Changed values stay staged until the parent lays out.
        if size == previous.size && occupied == previous.occupied {
            return Ok(());
        }
    }

    access.work.counts.layout_ancestors = access.work.counts.layout_ancestors.saturating_add(1);
    access.request(access.elements.id(parent), Work::LAYOUT);
    Ok(())
}
