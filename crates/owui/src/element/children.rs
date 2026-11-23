//! Complete direct-child reconciliation and its commit/abort boundary.

use std::collections::HashMap;
use std::num::NonZeroUsize;

use crate::id::LocalKey;
use crate::layout::{Constraints, MeasureMode, Point, Size};
use crate::schedule::Work;

use super::mounted::{self, ElementAccess};
use super::{Element, ElementError, ElementId, Key};

pub(crate) struct Child {
    pub(crate) key: LocalKey,
    declaration: Option<Key>,
}

const INDEX_PROMOTION_KEYS: usize = 64;
const INDEX_RETENTION_KEYS: usize = 48;

// Positions belong to sealed membership; Small stores only keyed positions, in order.
// The promotion boundary selects hashing, not which child sets are legal.
enum KeyIndex {
    Empty,
    One(usize),
    Small(Vec<usize>),
    Indexed(HashMap<Key, usize>),
}

impl KeyIndex {
    fn update_indexed(
        mut keys: HashMap<Key, usize>,
        old: &[Child],
        next: &[Child],
        first: usize,
    ) -> Self {
        for child in old.iter().skip(first) {
            if let Some(key) = &child.declaration {
                keys.remove(key);
            }
        }
        for (position, child) in next.iter().enumerate().skip(first) {
            if let Some(key) = &child.declaration {
                keys.insert(key.clone(), position);
            }
        }

        Self::Indexed(keys)
    }

    fn update(self, old: &[Child], next: &[Child], key_count: usize, first: Option<usize>) -> Self {
        let Some(first) = first else {
            return self;
        };
        let index = match self {
            Self::Indexed(keys)
                if (INDEX_RETENTION_KEYS..INDEX_PROMOTION_KEYS).contains(&key_count) =>
            {
                return Self::update_indexed(keys, old, next, first);
            }
            index => index,
        };

        // insert/keep retain declarations along the unchanged ID prefix.
        match key_count {
            0 => Self::Empty,
            1 => {
                let prefix = match index {
                    Self::Empty => None,
                    Self::One(position) => (position < first).then_some(position),
                    Self::Small(positions) => positions
                        .first()
                        .copied()
                        .filter(|&position| position < first),
                    Self::Indexed(keys) => {
                        keys.values().copied().find(|&position| position < first)
                    }
                };
                let position = prefix
                    .or_else(|| {
                        next.iter()
                            .enumerate()
                            .skip(first)
                            .find_map(|(position, child)| {
                                child.declaration.as_ref().map(|_| position)
                            })
                    })
                    .expect("one keyed child retains a declaration position");

                Self::One(position)
            }
            2..=63 => {
                let mut positions = match index {
                    Self::Empty => Vec::with_capacity(key_count),
                    Self::One(position) => {
                        let mut positions = Vec::with_capacity(key_count);
                        if position < first {
                            positions.push(position);
                        }

                        positions
                    }
                    Self::Small(mut positions) => {
                        let prefix = positions.partition_point(|&position| position < first);
                        positions.truncate(prefix);

                        positions
                    }
                    Self::Indexed(keys) => {
                        let mut positions = Vec::with_capacity(key_count);
                        positions.extend(keys.into_values().filter(|&position| position < first));
                        positions.sort_unstable();

                        positions
                    }
                };
                positions.reserve(key_count - positions.len());
                for (position, child) in next.iter().enumerate().skip(first) {
                    if child.declaration.is_some() {
                        positions.push(position);
                    }
                }
                if positions.capacity() > key_count.saturating_mul(4).max(16) {
                    positions.shrink_to(key_count.saturating_mul(2).max(16));
                }

                Self::Small(positions)
            }
            _ => {
                let mut keys = match index {
                    Self::Empty => HashMap::with_capacity(key_count),
                    Self::One(position) => {
                        let mut keys = HashMap::with_capacity(key_count);
                        if position < first
                            && let Some(key) = &next[position].declaration
                        {
                            keys.insert(key.clone(), position);
                        }

                        keys
                    }
                    Self::Small(positions) => {
                        let mut keys = HashMap::with_capacity(key_count);
                        for position in positions
                            .into_iter()
                            .take_while(|&position| position < first)
                        {
                            if let Some(key) = &next[position].declaration {
                                keys.insert(key.clone(), position);
                            }
                        }

                        keys
                    }
                    Self::Indexed(mut keys) => {
                        for child in old.iter().skip(first) {
                            if let Some(key) = &child.declaration {
                                keys.remove(key);
                            }
                        }

                        keys
                    }
                };
                for (position, child) in next.iter().enumerate().skip(first) {
                    if let Some(key) = &child.declaration {
                        keys.insert(key.clone(), position);
                    }
                }

                Self::Indexed(keys)
            }
        }
    }
}

pub(crate) struct ChildSet {
    pub(crate) owner: LocalKey,
    pub(crate) ordered: Vec<Child>,
    keys: KeyIndex,
}

impl ChildSet {
    pub(crate) fn new(owner: LocalKey) -> Self {
        Self {
            owner,
            ordered: Vec::new(),
            keys: KeyIndex::Empty,
        }
    }

    pub(crate) fn find(&self, key: &Key) -> Option<usize> {
        match &self.keys {
            KeyIndex::Empty => None,
            KeyIndex::One(position) => {
                (self.ordered[*position].declaration.as_ref() == Some(key)).then_some(*position)
            }
            KeyIndex::Small(positions) => positions
                .iter()
                .copied()
                .find(|&position| self.ordered[position].declaration.as_ref() == Some(key)),
            KeyIndex::Indexed(keys) => keys.get(key).copied(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Retention {
    Unused,
    Kept,
    Replaced,
}

pub(crate) struct ChildScratch {
    next: Vec<Child>,
    used: Vec<Retention>,
    keys: HashMap<Key, usize>,
    created: Vec<LocalKey>,
}

impl ChildScratch {
    pub(crate) fn new() -> Self {
        Self {
            next: Vec::new(),
            used: Vec::new(),
            keys: HashMap::new(),
            created: Vec::new(),
        }
    }

    fn bytes(&self) -> usize {
        self.next
            .capacity()
            .saturating_mul(size_of::<Child>())
            .saturating_add(self.used.capacity().saturating_mul(size_of::<Retention>()))
            .saturating_add(
                self.created
                    .capacity()
                    .saturating_mul(size_of::<LocalKey>()),
            )
            .saturating_add(
                self.keys
                    .capacity()
                    .saturating_mul(size_of::<(Key, usize)>() + 1),
            )
    }

    fn clear(&mut self, membership: usize) {
        let keys = self.keys.len();
        let created = self.created.len();
        self.next.clear();
        self.used.clear();
        self.keys.clear();
        self.created.clear();

        fn trim<T>(values: &mut Vec<T>, demand: usize) {
            if values.capacity() > demand.saturating_mul(4).max(16)
                && values.capacity().saturating_mul(size_of::<T>()) > 64 * 1024
            {
                values.shrink_to(demand.saturating_mul(2).max(16));
            }
        }

        trim(&mut self.next, membership);
        trim(&mut self.used, membership);
        trim(&mut self.created, created);
        if self.keys.capacity() > keys.saturating_mul(4).max(16)
            && self
                .keys
                .capacity()
                .saturating_mul(size_of::<(Key, usize)>() + 1)
                > 64 * 1024
        {
            self.keys.shrink_to(keys.saturating_mul(2).max(16));
        }
    }
}

/// One complete edit of this parent's child set; omitted members retire only on commit.
///
/// Old membership stays in its owner during the callback. Nested calls may grow
/// the arena but cannot physically remove an active set; its scope-local index
/// does not retain a Vec-element borrow across that growth.
pub struct ChildrenCx<'a> {
    access: ElementAccess<'a>,
    parent: ElementId,
    slot: usize,
    scratch: Option<ChildScratch>,
    key_count: usize,
    finished: bool,
}

impl ChildrenCx<'_> {
    fn slot(&self) -> usize {
        // Nested scopes can grow the arena, but physical removal waits for all callbacks to finish.
        debug_assert_eq!(
            self.access.elements.children[self.slot].owner,
            self.parent.local(),
            "active child scopes retain their scope-local set index"
        );

        self.slot
    }

    fn old(&self) -> &ChildSet {
        &self.access.elements.children[self.slot()]
    }

    fn scratch(&self) -> &ChildScratch {
        self.scratch.as_ref().expect("active scope owns scratch")
    }

    fn scratch_mut(&mut self) -> &mut ChildScratch {
        self.scratch.as_mut().expect("active scope owns scratch")
    }

    pub fn realize<D: Element>(
        &mut self,
        key: impl Into<Key>,
        declaration: D,
    ) -> Result<ElementId, ElementError> {
        self.insert(Some(key.into()), declaration)
    }

    pub fn push<D: Element>(&mut self, declaration: D) -> Result<ElementId, ElementError> {
        let key = declaration.attrs().key.clone();
        self.insert(key, declaration)
    }

    fn check_key(&self, key: &Key) -> Result<Option<usize>, ElementError> {
        let position = self.scratch().next.len();
        let old = self.old();
        let index = old
            .ordered
            .get(position)
            .filter(|child| child.declaration.as_ref() == Some(key))
            .map(|_| position)
            .or_else(|| old.find(key));
        let duplicate = index.map_or_else(
            || self.scratch().keys.contains_key(key),
            |index| self.scratch().used[index] != Retention::Unused,
        );
        if duplicate {
            return Err(ElementError::DuplicateKey(key.clone()));
        }

        Ok(index)
    }

    fn insert<D: Element>(
        &mut self,
        key: Option<Key>,
        declaration: D,
    ) -> Result<ElementId, ElementError> {
        let position = self.scratch().next.len();
        let index = match &key {
            Some(key) => self.check_key(key)?,
            None => self
                .old()
                .ordered
                .get(position)
                .filter(|child| {
                    child.declaration.is_none()
                        && self.scratch().used[position] == Retention::Unused
                })
                .map(|_| position),
        };

        let existing = index.map(|index| self.access.elements.id(self.old().ordered[index].key));
        let id = mounted::apply(&mut self.access, Some(self.parent), existing, declaration)?;

        if let Some(index) = index {
            let same = self.old().ordered[index].key == id.local();
            self.scratch_mut().used[index] = if same {
                Retention::Kept
            } else {
                Retention::Replaced
            };
            if !same {
                self.scratch_mut().created.push(id.local());
            }
        } else {
            self.scratch_mut().created.push(id.local());
        }

        if let Some(key) = &key {
            self.key_count += 1;
            if index != Some(position) {
                self.scratch_mut().keys.insert(key.clone(), position);
            }
        }
        self.scratch_mut().next.push(Child {
            key: id.local(),
            declaration: key,
        });

        Ok(id)
    }

    pub fn find(&self, key: &Key) -> Option<ElementId> {
        if let Some(index) = self.scratch().keys.get(key) {
            return Some(self.access.elements.id(self.scratch().next[*index].key));
        }

        let index = self.old().find(key)?;
        if let Some(child) = self
            .scratch()
            .next
            .get(index)
            .filter(|child| child.declaration.as_ref() == Some(key))
        {
            return Some(self.access.elements.id(child.key));
        }

        Some(self.access.elements.id(self.old().ordered[index].key))
    }

    pub fn keep(&mut self, child: ElementId) -> Result<(), ElementError> {
        let row = self.access.elements.check_child(self.parent, child)?;
        let position = self.access.elements.nodes[row.0].position;
        let old = self
            .old()
            .ordered
            .get(position)
            .filter(|entry| entry.key == child.local())
            .ok_or(ElementError::WrongParent {
                parent: self.parent,
                child,
            })?;
        let key = old.declaration.clone();
        if self.scratch().used[position] != Retention::Unused {
            return Err(ElementError::DuplicateChild(child));
        }

        mounted::ensure_current(&mut self.access, child)?;

        if let Some(key) = &key {
            self.key_count += 1;
            if position != self.scratch().next.len() {
                let next = self.scratch().next.len();
                self.scratch_mut().keys.insert(key.clone(), next);
            }
        }
        self.scratch_mut().used[position] = Retention::Kept;
        self.scratch_mut().next.push(Child {
            key: child.local(),
            declaration: key,
        });

        Ok(())
    }

    pub fn measure(
        &mut self,
        child: ElementId,
        constraints: Constraints,
    ) -> Result<Size, ElementError> {
        self.access.elements.check_child(self.parent, child)?;

        crate::layout::measure(&mut self.access, child, constraints, MeasureMode::Eager)
    }

    pub fn place(&mut self, child: ElementId, origin: Point) -> Result<(), ElementError> {
        let row = self.access.elements.check_child(self.parent, child)?;

        if self.access.layout.place(row, origin)? {
            self.access.work.geometry_dirty(child.local(), row);
        }

        Ok(())
    }

    pub fn previous_key(&self, child: ElementId) -> Result<Option<&Key>, ElementError> {
        let row = self.access.elements.check_child(self.parent, child)?;
        let position = self.access.elements.nodes[row.0].position;
        Ok(self
            .old()
            .ordered
            .get(position)
            .filter(|entry| entry.key == child.local())
            .and_then(|entry| entry.declaration.as_ref()))
    }

    /// Visits old direct children containing keyboard focus or pointer capture.
    /// Hover alone does not pin a child in a virtualized retained set.
    pub fn active_children(&self) -> impl Iterator<Item = ElementId> + '_ {
        self.old()
            .ordered
            .iter()
            .filter(|child| self.access.elements.is_active(child.key))
            .map(|child| self.access.elements.id(child.key))
    }

    fn finish(&mut self) {
        let old_len = self.old().ordered.len();
        let next_len = self.scratch().next.len();
        let first = self
            .old()
            .ordered
            .iter()
            .zip(&self.scratch().next)
            .position(|(old, next)| old.key != next.key)
            .or_else(|| (old_len != next_len).then_some(old_len.min(next_len)));
        let from = first.map(|first| {
            self.old()
                .ordered
                .get(first)
                .and_then(|child| self.access.elements.rank(child.key))
                .unwrap_or_else(|| self.access.elements.after_subtree(self.parent.local()))
        });

        let slot = self.slot();
        let mut scratch = self
            .scratch
            .take()
            .expect("finished child scope owns scratch");
        let current = &mut self.access.elements.children[slot];
        let keys = std::mem::replace(&mut current.keys, KeyIndex::Empty).update(
            &current.ordered,
            &scratch.next,
            self.key_count,
            first,
        );

        let next = ChildSet {
            owner: self.parent.local(),
            ordered: std::mem::take(&mut scratch.next),
            keys,
        };
        let old = std::mem::replace(&mut self.access.elements.children[slot], next);

        let current = &mut self.access.elements.children[slot];
        if current.ordered.capacity() > current.ordered.len().saturating_mul(4).max(16)
            && current
                .ordered
                .capacity()
                .saturating_mul(size_of::<Child>())
                > 64 * 1024
        {
            current
                .ordered
                .shrink_to(current.ordered.len().saturating_mul(2).max(16));
        }
        if let KeyIndex::Indexed(index) = &mut current.keys
            && (old_len > next_len.saturating_mul(4).max(16)
                || index.capacity() > self.key_count.saturating_mul(4).max(16))
        {
            index.shrink_to(self.key_count.saturating_mul(2).max(16));
        }

        if let Some(first) = first {
            for (position, child) in self.access.elements.children[slot]
                .ordered
                .iter()
                .enumerate()
                .skip(first)
            {
                let row = self
                    .access
                    .elements
                    .row_local(child.key)
                    .expect("new child membership retains identities");
                self.access.elements.nodes[row.0].position = position;
            }
        }

        for (child, retention) in old.ordered.iter().zip(&scratch.used) {
            if *retention != Retention::Kept {
                self.access.elements.retire(child.key);
            }
        }

        if let Some(from) = from {
            self.access.layout.invalidate_structure(self.parent.local());
            self.access.elements.request_order(from);
            self.access.request(self.parent, Work::LAYOUT);
        }

        scratch.next = old.ordered;
        scratch.clear(next_len);
        self.finished = true;
        self.return_scratch(scratch);
    }

    fn return_scratch(&mut self, scratch: ChildScratch) {
        let budget: usize = 8 * 1024 * 1024;
        let retained = self
            .access
            .elements
            .child_scratch
            .iter()
            .map(ChildScratch::bytes)
            .sum::<usize>();
        if scratch.bytes() <= budget.saturating_sub(retained) {
            self.access.elements.child_scratch.push(scratch);
        }
    }
}

impl Drop for ChildrenCx<'_> {
    fn drop(&mut self) {
        if let Some(mut scratch) = self.scratch.take() {
            if !self.finished {
                for key in scratch.created.drain(..) {
                    self.access.elements.retire(key);
                }
            }

            scratch.clear(self.old().ordered.len());
            self.return_scratch(scratch);
        }
    }
}

pub(crate) fn edit<R>(
    access: &mut ElementAccess<'_>,
    parent: ElementId,
    process: impl FnOnce(&mut ChildrenCx<'_>) -> Result<R, ElementError>,
) -> Result<R, ElementError> {
    let row = access
        .elements
        .row(parent)
        .ok_or(ElementError::Missing(parent))?;

    if access.elements.nodes[row.0].children.is_none() {
        let index = access.elements.children.len();
        access.elements.children.push(ChildSet::new(parent.local()));
        access.elements.nodes[row.0].children = Some(
            NonZeroUsize::new(index + 1)
                .expect("allocated child-set indices retain a nonzero encoding"),
        );
    }

    let slot = access.elements.nodes[row.0]
        .children
        .expect("child set was allocated")
        .get()
        - 1;
    let old_len = access.elements.children[slot].ordered.len();
    access
        .elements
        .child_scratch
        .reserve(access.elements.frames + 1);

    // A tiny parent must not consume the wide membership buffer that its next child needs.
    let selected = access
        .elements
        .child_scratch
        .iter()
        .enumerate()
        .filter(|(_, scratch)| scratch.next.capacity() <= old_len.saturating_mul(4).max(16))
        .min_by_key(|(_, scratch)| scratch.next.capacity().abs_diff(old_len))
        .map(|(index, _)| index);
    let mut scratch = selected
        .map(|index| access.elements.child_scratch.swap_remove(index))
        .unwrap_or_else(ChildScratch::new);
    scratch.used.resize(old_len, Retention::Unused);

    let mut cx = ChildrenCx {
        access: access.reborrow(),
        parent,
        slot,
        scratch: Some(scratch),
        key_count: 0,
        finished: false,
    };

    let result = process(&mut cx)?;

    cx.finish();

    Ok(result)
}
