//! Actual semantic-content membership, qualification decoration, and Tab order.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use crate::element::configuration::ConfigRead;
use crate::element::mounted::{MountedElements, Row};
use crate::element::{ElementError, ElementId, Name};
use crate::id::LocalKey;
use crate::layout::Layout;
use crate::schedule::WorkCounts;

use super::{Role, SemanticContent, Semantics, trim_map};

struct SemanticRecord {
    output: Semantics,
    published: bool,
}

#[derive(Default)]
struct FocusOrder {
    members: HashMap<LocalKey, usize>,
    capacity: usize,
    ordered: BTreeMap<usize, LocalKey>,
    cursor: Option<(LocalKey, usize)>,
}

#[derive(Clone, Copy)]
struct FocusMember {
    key: LocalKey,
    rank: usize,
}

pub(super) struct FocusSnapshot {
    members: Vec<FocusMember>,
    cursor: Option<(LocalKey, usize)>,
}

impl FocusSnapshot {
    pub(super) fn next(
        &self,
        current: Option<LocalKey>,
        rank: impl Fn(LocalKey) -> Option<usize>,
    ) -> Option<LocalKey> {
        if self.members.is_empty() {
            return None;
        }

        let index = current.and_then(|key| {
            if let Some((cached, index)) = self.cursor
                && cached == key
            {
                return Some(index);
            }
            let rank = rank(key)?;
            let index = self
                .members
                .binary_search_by_key(&rank, |member| member.rank)
                .ok()?;
            (self.members[index].key == key).then_some(index)
        });
        let next = index.map_or(0, |index| {
            if index + 1 == self.members.len() {
                0
            } else {
                index + 1
            }
        });

        Some(self.members[next].key)
    }
}

impl FocusOrder {
    fn set(&mut self, key: LocalKey, rank: Option<usize>) -> Result<(), ElementError> {
        let previous = self.members.get(&key).copied();
        if previous == rank {
            return Ok(());
        }
        if let Some(rank) = rank
            && self.ordered.get(&rank).is_some_and(|member| *member != key)
        {
            return Err(ElementError::OrderConflict { rank });
        }

        self.remove(key);
        if let Some(rank) = rank {
            self.members.insert(key, rank);
            self.capacity = self.capacity.max(self.members.capacity());
            self.ordered.insert(rank, key);
        }
        self.cursor = None;

        Ok(())
    }

    fn remove(&mut self, key: LocalKey) {
        if let Some(rank) = self.members.remove(&key) {
            let removed = self.ordered.remove(&rank);
            debug_assert_eq!(removed, Some(key));
            self.cursor = None;
        }
        if self.members.is_empty() {
            self.members = HashMap::new();
            self.capacity = 0;
        }
    }

    fn reindex_from(
        &mut self,
        from: usize,
        rank: impl Fn(LocalKey) -> Option<usize>,
    ) -> Result<u64, ElementError> {
        let mut changes = Vec::new();
        let mut next = BTreeMap::new();
        for (&previous, &key) in self.ordered.range(from..) {
            let rank = rank(key).ok_or_else(|| {
                ElementError::Operation("a retained Tab member has no logical rank".into())
            })?;
            if self
                .ordered
                .get(&rank)
                .is_some_and(|member| rank < from && *member != key)
                || next.insert(rank, key).is_some()
            {
                return Err(ElementError::OrderConflict { rank });
            }
            changes.push((key, previous, rank));
        }

        for &(_, previous, _) in &changes {
            self.ordered.remove(&previous);
        }
        for &(key, _, rank) in &changes {
            self.members.insert(key, rank);
            self.ordered.insert(rank, key);
        }
        if !changes.is_empty() {
            self.cursor = None;
        }

        Ok(changes.len() as u64)
    }

    fn snapshot(&mut self, current: Option<LocalKey>) -> FocusSnapshot {
        let members: Vec<_> = self
            .ordered
            .iter()
            .map(|(&rank, &key)| FocusMember { key, rank })
            .collect();
        let cursor = current.and_then(|key| {
            let rank = match self.cursor {
                Some((cached, rank)) if cached == key => rank,
                _ => *self.members.get(&key)?,
            };
            let index = members
                .binary_search_by_key(&rank, |member| member.rank)
                .ok()?;
            self.cursor = Some((key, rank));
            Some((key, index))
        });

        FocusSnapshot { members, cursor }
    }
}

#[derive(Default)]
pub(super) struct SemanticProjection {
    records: HashMap<LocalKey, SemanticRecord>,
    capacity: usize,
    focus: FocusOrder,
}

impl SemanticProjection {
    pub(super) fn get(&self, key: LocalKey) -> Option<&Semantics> {
        let record = self.records.get(&key)?;
        record.published.then_some(&record.output)
    }

    pub(super) fn publish(
        &mut self,
        id: ElementId,
        mut content: SemanticContent,
        configuration: ConfigRead<'_>,
        layout: &Layout,
        row: Row,
        rank: Option<usize>,
    ) -> Result<(), ElementError> {
        if let Some(name) = configuration.get_ref(Name) {
            content.name = Some(Arc::clone(name));
        }

        let key = id.local();
        if !has_content(&content) {
            self.remove(key);
            return Ok(());
        }

        let published = participates(layout, row);
        let focus_rank = if published && layout.eligible(row) && content.focusable {
            Some(rank.ok_or(ElementError::Missing(id))?)
        } else {
            None
        };

        self.focus.set(key, focus_rank)?;
        self.records.insert(
            key,
            SemanticRecord {
                output: Semantics {
                    role: content.role,
                    name: content.name,
                    value: content.value,
                    disabled: !layout.records[row.0].enabled,
                    focusable: content.focusable,
                    total_items: content.total_items,
                    position: content.position,
                },
                published,
            },
        );
        self.capacity = self.capacity.max(self.records.capacity());
        Ok(())
    }

    fn decorate(
        &mut self,
        key: LocalKey,
        elements: &MountedElements,
        layout: &Layout,
    ) -> Result<(), ElementError> {
        let Some(record) = self.records.get(&key) else {
            return Ok(());
        };
        let Some(row) = elements.row_local(key) else {
            return Err(ElementError::Missing(elements.id(key)));
        };

        let published = elements.contains_local(key) && participates(layout, row);
        let rank = if published && layout.eligible(row) && record.output.focusable {
            Some(
                elements
                    .rank(key)
                    .ok_or(ElementError::Missing(elements.id(key)))?,
            )
        } else {
            None
        };
        self.focus.set(key, rank)?;
        let record = self
            .records
            .get_mut(&key)
            .expect("decoration retains its actual content record");
        record.published = published;
        record.output.disabled = !layout.records[row.0].enabled;
        Ok(())
    }

    pub(super) fn sync_qualification(
        &mut self,
        elements: &MountedElements,
        layout: &Layout,
        counts: &mut WorkCounts,
    ) -> Result<(), ElementError> {
        for &key in &elements.retired {
            self.remove(key);
        }

        if let Some(from) = elements.order_from {
            let visits = self.focus.reindex_from(from, |key| elements.rank(key))?;
            counts.hit_order_visits = counts.hit_order_visits.saturating_add(visits);
        }

        for &key in &layout.qualification_changed {
            self.decorate(key, elements, layout)?;
        }
        for &key in &layout.changed {
            self.decorate(key, elements, layout)?;
        }

        Ok(())
    }

    pub(super) fn snapshot_focus(&mut self, current: Option<LocalKey>) -> FocusSnapshot {
        self.focus.snapshot(current)
    }

    pub(super) fn remove(&mut self, key: LocalKey) {
        self.focus.remove(key);
        self.records.remove(&key);
        if self.records.is_empty() {
            self.records = HashMap::new();
            self.capacity = 0;
        }
    }

    pub(super) fn trim(&mut self) {
        trim_map(&mut self.records, &mut self.capacity);
        trim_map(&mut self.focus.members, &mut self.focus.capacity);
    }
}

fn has_content(content: &SemanticContent) -> bool {
    content.role != Role::Generic
        || content.name.is_some()
        || content.value.is_some()
        || content.focusable
        || content.total_items.is_some()
        || content.position.is_some()
}

fn participates(layout: &Layout, row: Row) -> bool {
    layout.records[row.0].visible && layout.geometry(row).is_some()
}
