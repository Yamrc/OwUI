//! Canonical local sources, immutable inherited environments, and delivery freshness.

use std::any::TypeId;
use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::rc::Rc;

use crate::id::LocalKey;
use crate::input::EventKey;
use crate::view::ActionBinding;

use super::mounted::Row;
use super::property::{Entry, PropertyChange, PropertyChanges};
use super::{Attrs, ElementError, InheritedValues, Properties, Property, Visibility};

/// A resolved read view; obtaining a value never invokes element callbacks.
#[derive(Clone, Copy)]
pub(crate) struct ConfigRead<'a> {
    local: Option<&'a Properties>,
    inherited: Option<&'a InheritedValues>,
    handlers: Option<&'a [(TypeId, ActionBinding)]>,
    visibility: Visibility,
}

impl<'a> ConfigRead<'a> {
    pub(crate) fn get<P: Property>(&self, property: P) -> P::Value {
        self.get_ref(property)
            .cloned()
            .unwrap_or_else(P::default_value)
    }

    pub(crate) fn get_ref<P: Property>(&self, _property: P) -> Option<&'a P::Value> {
        self.local
            .and_then(|local| local.get_ref::<P>())
            .or_else(|| {
                if P::INHERITED {
                    self.inherited.and_then(|values| values.get_ref::<P>())
                } else {
                    None
                }
            })
    }

    pub(crate) fn visibility(&self) -> Visibility {
        self.visibility
    }

    pub(crate) fn has_handler<K: EventKey>(&self, _event: K) -> bool {
        self.binding(TypeId::of::<K>()).is_some()
    }

    pub(crate) fn binding(&self, event: TypeId) -> Option<&'a ActionBinding> {
        self.handlers?
            .iter()
            .find(|(key, _)| *key == event)
            .map(|(_, binding)| binding)
    }
}

#[derive(Clone, Default)]
pub(crate) struct Environment {
    pub(crate) values: Option<Rc<InheritedValues>>,
    pub(crate) epoch: u64,
}

impl Environment {
    fn equivalent(&self, other: &Self) -> bool {
        match (&self.values, &other.values) {
            (None, None) => true,
            (Some(left), Some(right)) => Rc::ptr_eq(left, right) || left.equivalent(right),
            _ => false,
        }
    }
}

#[derive(Clone, Copy)]
struct SourceIndex(NonZeroUsize);

impl SourceIndex {
    fn new(index: usize) -> Self {
        Self(
            NonZeroUsize::new(index + 1).expect("allocated source indices have a nonzero encoding"),
        )
    }

    fn get(self) -> usize {
        self.0.get() - 1
    }
}

pub(crate) struct Source {
    local: Properties,
    handlers: Vec<(TypeId, ActionBinding)>,
    inherited: bool,
    output: Environment,
}

impl Source {
    fn from_attrs(attrs: Attrs, previous_output: Environment) -> (Option<Self>, Visibility) {
        let source = if attrs.local.is_empty() && attrs.handlers.is_empty() {
            None
        } else {
            let inherited = attrs.local.has_inherited();
            Some(Self {
                local: attrs.local,
                handlers: attrs.handlers,
                inherited,
                output: previous_output,
            })
        };

        (source, attrs.visibility)
    }
}

// Passthrough nodes store only their input. A sourced row retains its last published
// output beside the actual local configuration and handlers.
struct ConfigRow {
    source: Option<SourceIndex>,
    input: Environment,
    source_epoch: u64,
    checked_epoch: u64,
    visibility: Visibility,
    built: bool,
    source_dirty: bool,
    force_walk: bool,
    walk_pending: bool,
    frontier_queued: bool,
}

/// Protected source handles avoid moving large attribute snapshots onto callback stacks.
pub(crate) struct SourceSnapshot {
    source: Option<SourceIndex>,
    visibility: Visibility,
    input: Environment,
}

pub(crate) struct Mutation {
    pub(crate) change: PropertyChange,
    pub(crate) previous: Option<Entry>,
}

pub(crate) struct Refresh {
    pub(crate) changes: PropertyChanges,
    pub(crate) output_changed: bool,
    pub(crate) retired_input: Environment,
    pub(crate) retired_output: Option<Environment>,
}

/// User-bearing source and environment values stay owned until the caller commits.
pub(crate) struct SourceCommit {
    _snapshot: SourceSnapshot,
    _source: Option<Source>,
}

pub(crate) struct RemovedConfiguration {
    _input: Environment,
    _source: Option<Source>,
    _delivery: Option<PendingDelivery>,
}

struct PendingDelivery {
    changes: PropertyChanges,
    active_prefix: Option<NonZeroUsize>,
}

pub(crate) struct Configuration {
    rows: Vec<ConfigRow>,
    deliveries: HashMap<LocalKey, PendingDelivery>,
    sources: Vec<Option<Source>>,
    free_sources: Vec<usize>,
    live_sources: usize,
    epoch: u64,
    rollback_headroom: u64,
    snapshots: Vec<SourceSnapshot>,
    retired_environments: Vec<(Environment, Option<Environment>)>,
    path_scratch: Vec<LocalKey>,
    frontier: Vec<LocalKey>,
}

impl Configuration {
    pub(crate) fn new() -> Self {
        Self {
            rows: Vec::new(),
            deliveries: HashMap::new(),
            sources: Vec::new(),
            free_sources: Vec::new(),
            live_sources: 0,
            epoch: 0,
            rollback_headroom: 0,
            snapshots: Vec::new(),
            retired_environments: Vec::new(),
            path_scratch: Vec::new(),
            frontier: Vec::new(),
        }
    }

    pub(crate) fn reserve_row(&mut self) {
        self.rows.reserve(1);
    }

    pub(crate) fn append(&mut self, attrs: Attrs, parent: Environment) {
        let (source, visibility) = Source::from_attrs(attrs, parent.clone());
        let source = source.map(|source| self.put_source(source));
        self.rows.push(ConfigRow {
            source,
            input: parent,
            source_epoch: self.epoch,
            checked_epoch: 0,
            visibility,
            built: false,
            source_dirty: true,
            force_walk: false,
            walk_pending: false,
            frontier_queued: false,
        });
    }

    pub(crate) fn read(&self, row: Row) -> ConfigRead<'_> {
        let record = &self.rows[row.0];
        let source = record
            .source
            .and_then(|index| self.sources[index.get()].as_ref());
        ConfigRead {
            local: source.map(|source| &source.local),
            inherited: record.input.values.as_deref(),
            handlers: source.map(|source| source.handlers.as_slice()),
            visibility: record.visibility,
        }
    }

    pub(crate) fn take_path(&mut self) -> Vec<LocalKey> {
        std::mem::take(&mut self.path_scratch)
    }

    pub(crate) fn return_path(&mut self, mut path: Vec<LocalKey>) {
        path.clear();
        if self.path_scratch.is_empty()
            && path.capacity().saturating_mul(size_of::<LocalKey>()) <= 64 * 1024
        {
            self.path_scratch = path;
        }
    }

    pub(crate) fn revision(&self) -> u64 {
        self.epoch
    }

    pub(crate) fn needs_delivery(&self, key: LocalKey) -> bool {
        self.deliveries
            .get(&key)
            .is_some_and(|delivery| !delivery.changes.is_empty())
    }

    fn retain_changes(&mut self, key: LocalKey, changes: &[PropertyChange]) {
        if changes.is_empty() {
            return;
        }

        let delivery = self
            .deliveries
            .entry(key)
            .or_insert_with(|| PendingDelivery {
                changes: PropertyChanges::default(),
                active_prefix: None,
            });
        let prefix = delivery.active_prefix.map_or(0, |prefix| prefix.get() - 1);
        delivery.changes.merge_suffix(changes, prefix);
    }

    /// Keeps callback-entry keys owned until the matching successful delivery.
    pub(crate) fn begin_delivery(
        &mut self,
        key: LocalKey,
    ) -> (PropertyChanges, Option<NonZeroUsize>) {
        let Some(delivery) = self.deliveries.get_mut(&key) else {
            return (PropertyChanges::default(), None);
        };
        assert!(
            delivery.active_prefix.is_none(),
            "a mounted body has only one active configuration delivery"
        );

        let prefix = NonZeroUsize::new(delivery.changes.len() + 1)
            .expect("allocated delivery prefixes have a nonzero encoding");
        let changes = delivery.changes.clone();
        delivery.active_prefix = Some(prefix);

        (changes, Some(prefix))
    }

    pub(crate) fn finish_delivery(
        &mut self,
        key: LocalKey,
        prefix: NonZeroUsize,
        successful: bool,
    ) {
        let Some(delivery) = self.deliveries.get_mut(&key) else {
            panic!("an active configuration delivery retains its owned key ledger");
        };
        assert_eq!(
            delivery.active_prefix,
            Some(prefix),
            "configuration delivery acknowledges its own entry prefix"
        );

        delivery.active_prefix = None;
        if successful {
            delivery.changes.discard_prefix(prefix.get() - 1);
        } else {
            delivery.changes.coalesce();
        }
        if delivery.changes.is_empty() {
            self.deliveries.remove(&key);
        }
    }

    pub(crate) fn checked(&self, row: Row) -> bool {
        let record = &self.rows[row.0];
        record.built
            && !record.source_dirty
            && !record.force_walk
            && record.checked_epoch == self.epoch
    }

    /// Proves an unchanged local source and the exact current parent input without rewalking ancestry.
    pub(crate) fn input_current(&self, row: Row, parent: &Environment) -> bool {
        let record = &self.rows[row.0];
        record.built
            && !record.source_dirty
            && !record.force_walk
            && record.source_epoch <= record.checked_epoch
            && record.input.equivalent(parent)
    }

    pub(crate) fn output_epoch(&self, row: Row) -> u64 {
        self.output(row).epoch
    }

    pub(crate) fn output(&self, row: Row) -> Environment {
        let record = &self.rows[row.0];
        record
            .source
            .and_then(|index| self.sources[index.get()].as_ref())
            .map_or_else(|| record.input.clone(), |source| source.output.clone())
    }

    fn put_source(&mut self, source: Source) -> SourceIndex {
        self.live_sources += 1;
        if let Some(index) = self.free_sources.pop() {
            self.sources[index] = Some(source);
            SourceIndex::new(index)
        } else {
            let index = self.sources.len();
            self.sources.push(Some(source));
            SourceIndex::new(index)
        }
    }

    fn take_source(&mut self, index: SourceIndex) -> Option<Source> {
        let source = self.sources[index.get()].take();
        if source.is_some() {
            self.live_sources -= 1;
            let headroom = usize::try_from(self.rollback_headroom)
                .expect("active source guards fit their reserved capacity");
            self.free_sources.reserve(1 + headroom);
            self.free_sources.push(index.get());
        }

        source
    }

    fn issue_epoch(&mut self) -> Result<u64, ElementError> {
        let next = self
            .epoch
            .checked_add(1)
            .ok_or(ElementError::RevisionExhausted)?;
        if next.checked_add(self.rollback_headroom).is_none() {
            return Err(ElementError::RevisionExhausted);
        }

        self.epoch = next;
        Ok(next)
    }

    fn reserve_rollback(&mut self) -> Result<(), ElementError> {
        let reserved = self
            .rollback_headroom
            .checked_add(1)
            .ok_or(ElementError::RevisionExhausted)?;
        if self.epoch.checked_add(reserved).is_none() {
            return Err(ElementError::RevisionExhausted);
        }
        let reserve = usize::try_from(reserved).map_err(|_| ElementError::RevisionExhausted)?;

        self.frontier.reserve(reserve);
        self.free_sources.reserve(reserve);
        self.snapshots.reserve(1);
        self.rollback_headroom = reserved;

        Ok(())
    }

    fn mark_root(&mut self, row: Row, key: LocalKey, force: bool) {
        let record = &mut self.rows[row.0];
        record.force_walk |= force;
        record.walk_pending |= force;
        record.source_dirty = true;
        if !record.frontier_queued {
            record.frontier_queued = true;
            let headroom = usize::try_from(self.rollback_headroom)
                .expect("active source guards fit their reserved capacity");
            self.frontier.reserve(1 + headroom);
            self.frontier.push(key);
        }
    }

    pub(crate) fn next_root(&mut self) -> Option<LocalKey> {
        self.frontier.pop()
    }

    pub(crate) fn begin_root(&mut self, row: Row) -> bool {
        self.rows[row.0].frontier_queued = false;
        std::mem::take(&mut self.rows[row.0].walk_pending)
    }

    pub(crate) fn queue_descendants(&mut self, row: Row, key: LocalKey) {
        let record = &mut self.rows[row.0];
        record.walk_pending = true;
        if !record.frontier_queued {
            record.frontier_queued = true;
            let headroom = usize::try_from(self.rollback_headroom)
                .expect("active source guards fit their reserved capacity");
            self.frontier.reserve(1 + headroom);
            self.frontier.push(key);
        }
    }

    pub(crate) fn has_frontier(&self) -> bool {
        !self.frontier.is_empty()
    }

    fn attrs_changed(&self, row: Row, attrs: &Attrs) -> bool {
        let record = &self.rows[row.0];
        if record.visibility != attrs.visibility {
            return true;
        }
        let source = record
            .source
            .and_then(|index| self.sources[index.get()].as_ref());
        let Some(source) = source else {
            return !attrs.local.is_empty() || !attrs.handlers.is_empty();
        };
        if !source.local.equivalent(&attrs.local) || source.handlers.len() != attrs.handlers.len() {
            return true;
        }
        attrs.handlers.iter().any(|(key, binding)| {
            !source
                .handlers
                .iter()
                .any(|(old_key, old)| old_key == key && old.same_registration(binding))
        })
    }

    pub(crate) fn begin_replace(
        &mut self,
        row: Row,
        key: LocalKey,
        attrs: Attrs,
    ) -> Result<SourceSnapshot, ElementError> {
        let changed = self.attrs_changed(row, &attrs);
        self.reserve_rollback()?;
        let epoch = if changed {
            match self.issue_epoch() {
                Ok(epoch) => epoch,
                Err(error) => {
                    self.rollback_headroom -= 1;
                    return Err(error);
                }
            }
        } else {
            self.rows[row.0].source_epoch
        };

        let previous_output = self.output(row);
        let (source, visibility) = Source::from_attrs(attrs, previous_output);
        let source = source.map(|source| self.put_source(source));
        let record = &mut self.rows[row.0];
        let snapshot = SourceSnapshot {
            source: record.source,
            visibility: record.visibility,
            input: record.input.clone(),
        };

        record.source = source;
        record.visibility = visibility;
        record.source_epoch = epoch;
        if changed {
            self.mark_root(row, key, false);
        }

        Ok(snapshot)
    }

    pub(crate) fn replacement_changes(
        &self,
        row: Row,
        snapshot: &SourceSnapshot,
        parent: &Environment,
    ) -> PropertyChanges {
        let empty = Properties::new();
        let current = self.rows[row.0]
            .source
            .and_then(|index| self.sources[index.get()].as_ref());
        let previous = snapshot
            .source
            .and_then(|index| self.sources[index.get()].as_ref());

        Properties::effective_changes(
            current.map_or(&empty, |source| &source.local),
            parent.values.as_deref(),
            previous.map_or(&empty, |source| &source.local),
            snapshot.input.values.as_deref(),
        )
    }

    pub(crate) fn protected_output(&self, index: usize) -> Environment {
        let snapshot = &self.snapshots[index];
        snapshot
            .source
            .and_then(|source| self.sources[source.get()].as_ref())
            .map_or_else(|| snapshot.input.clone(), |source| source.output.clone())
    }

    pub(crate) fn protect_replace(
        &mut self,
        row: Row,
        key: LocalKey,
        attrs: Attrs,
    ) -> Result<usize, ElementError> {
        let snapshot = self.begin_replace(row, key, attrs)?;
        let index = self.snapshots.len();
        self.snapshots.push(snapshot);

        Ok(index)
    }

    pub(crate) fn protected_changes(
        &self,
        row: Row,
        index: usize,
        parent: &Environment,
    ) -> PropertyChanges {
        self.replacement_changes(row, &self.snapshots[index], parent)
    }

    pub(crate) fn commit_protected(&mut self, index: usize) -> SourceCommit {
        assert_eq!(
            index + 1,
            self.snapshots.len(),
            "source snapshots follow nested callback ownership"
        );

        let snapshot = self
            .snapshots
            .pop()
            .expect("active replacement owns a source snapshot");
        self.commit_replace(snapshot)
    }

    pub(crate) fn rollback_protected(
        &mut self,
        row: Row,
        key: LocalKey,
        index: usize,
    ) -> SourceCommit {
        assert_eq!(
            index + 1,
            self.snapshots.len(),
            "source rollback restores its own nested snapshot"
        );

        let snapshot = self
            .snapshots
            .pop()
            .expect("active replacement owns a source snapshot");
        self.rollback_replace(row, key, snapshot)
    }

    pub(crate) fn defer_environments(
        &mut self,
        input: Environment,
        output: Option<Environment>,
    ) -> Option<usize> {
        if input.values.is_none() && output.as_ref().is_none_or(|env| env.values.is_none()) {
            return None;
        }

        let index = self.retired_environments.len();
        self.retired_environments.push((input, output));

        Some(index)
    }

    pub(crate) fn release_environments(
        &mut self,
        index: usize,
    ) -> (Environment, Option<Environment>) {
        assert_eq!(
            index + 1,
            self.retired_environments.len(),
            "retired environments retain nested ownership"
        );

        self.retired_environments
            .pop()
            .expect("active delivery owns retired environment references")
    }

    pub(crate) fn commit_replace(&mut self, snapshot: SourceSnapshot) -> SourceCommit {
        self.rollback_headroom -= 1;
        let source = snapshot.source.and_then(|index| self.take_source(index));

        SourceCommit {
            _snapshot: snapshot,
            _source: source,
        }
    }

    /// Returns rejected values only after restoring source ownership and freshness debt.
    pub(crate) fn rollback_replace(
        &mut self,
        row: Row,
        key: LocalKey,
        snapshot: SourceSnapshot,
    ) -> SourceCommit {
        self.rollback_headroom -= 1;
        // Headroom was reserved, while the version itself is issued at actual rollback.
        self.epoch += 1;

        let record = &mut self.rows[row.0];
        let rejected = record.source;
        record.source = snapshot.source;
        record.visibility = snapshot.visibility;
        record.source_epoch = self.epoch;
        self.mark_root(row, key, true);
        let source = rejected.and_then(|index| self.take_source(index));

        SourceCommit {
            _snapshot: snapshot,
            _source: source,
        }
    }

    pub(crate) fn refresh(
        &mut self,
        row: Row,
        key: LocalKey,
        parent: Environment,
        previous_output: Option<Environment>,
    ) -> Result<Refresh, ElementError> {
        let previous_output = previous_output.unwrap_or_else(|| self.output(row));
        let record = &self.rows[row.0];
        let empty = Properties::new();
        let source = record
            .source
            .and_then(|index| self.sources[index.get()].as_ref());
        let local = source.map_or(&empty, |source| &source.local);
        let input_changed = !record.input.equivalent(&parent);
        let changes = if input_changed {
            Properties::effective_changes(
                local,
                parent.values.as_deref(),
                local,
                record.input.values.as_deref(),
            )
        } else {
            PropertyChanges::default()
        };

        let force = record.force_walk;
        let needs_output = !record.built || record.source_dirty || input_changed || force;
        let values = match source {
            Some(source) if source.inherited && needs_output => {
                local.compose_inherited(parent.values.as_ref(), previous_output.values.as_ref())
            }
            Some(source) if source.inherited => previous_output.values.clone(),
            _ => parent.values.clone(),
        };
        let mut output = Environment {
            values,
            epoch: previous_output.epoch,
        };
        let same_output = output.equivalent(&previous_output);
        let output_changed = !same_output || force;
        if output_changed {
            output.epoch = self.issue_epoch()?;
        } else {
            output.epoch = previous_output.epoch;
        }

        self.retain_changes(key, &changes);
        let source_index = self.rows[row.0].source;
        let retired_output = source_index.map(|index| {
            let source = self.sources[index.get()]
                .as_mut()
                .expect("refresh retains its local source");
            std::mem::replace(&mut source.output, output.clone())
        });
        let retired_input = {
            let record = &mut self.rows[row.0];
            std::mem::replace(&mut record.input, parent)
        };
        let record = &mut self.rows[row.0];
        record.checked_epoch = self.epoch;
        record.built = true;
        record.source_dirty = false;
        record.force_walk = false;

        Ok(Refresh {
            changes,
            output_changed,
            retired_input,
            retired_output,
        })
    }

    fn source_mut(&mut self, row: Row) -> &mut Source {
        let index = match self.rows[row.0].source {
            Some(index) => index,
            None => {
                let output = self.rows[row.0].input.clone();
                let index = self.put_source(Source {
                    local: Properties::new(),
                    handlers: Vec::new(),
                    inherited: false,
                    output,
                });
                self.rows[row.0].source = Some(index);
                index
            }
        };
        self.sources[index.get()]
            .as_mut()
            .expect("current source index remains owned until commit")
    }

    pub(crate) fn set<P: Property>(
        &mut self,
        row: Row,
        key: LocalKey,
        property: P,
        value: P::Value,
    ) -> Result<Option<Mutation>, ElementError> {
        drop(property);

        let same = self
            .read(row)
            .local
            .and_then(|local| local.get_ref::<P>())
            .is_some_and(|old| old == &value);
        if same {
            return Ok(None);
        }

        let change = PropertyChange {
            key: TypeId::of::<P>(),
            affects: P::AFFECTS,
            inherited: P::INHERITED,
        };
        let epoch = self.issue_epoch()?;
        let outgoing_dirty = self.rows[row.0].source_dirty || change.inherited;
        self.rows[row.0].source_epoch = epoch;
        self.mark_root(row, key, false);
        self.rows[row.0].source_dirty = outgoing_dirty;

        let source = self.source_mut(row);
        let previous = source.local.set_value_deferred::<P>(value);
        source.inherited = source.local.has_inherited();

        Ok(Some(Mutation { change, previous }))
    }

    pub(crate) fn remove<P: Property>(
        &mut self,
        row: Row,
        key: LocalKey,
        property: P,
    ) -> Result<Option<Mutation>, ElementError> {
        drop(property);

        if self
            .read(row)
            .local
            .and_then(|local| local.get_ref::<P>())
            .is_none()
        {
            return Ok(None);
        }

        let change = PropertyChange {
            key: TypeId::of::<P>(),
            affects: P::AFFECTS,
            inherited: P::INHERITED,
        };
        let epoch = self.issue_epoch()?;
        let outgoing_dirty = self.rows[row.0].source_dirty || change.inherited;
        self.rows[row.0].source_epoch = epoch;
        self.mark_root(row, key, false);
        self.rows[row.0].source_dirty = outgoing_dirty;

        let source = self.source_mut(row);
        let previous = source.local.remove_type_deferred::<P>();
        source.inherited = source.local.has_inherited();

        Ok(Some(Mutation { change, previous }))
    }

    pub(crate) fn set_visibility(
        &mut self,
        row: Row,
        key: LocalKey,
        value: Visibility,
    ) -> Result<bool, ElementError> {
        if self.rows[row.0].visibility == value {
            return Ok(false);
        }

        let epoch = self.issue_epoch()?;
        let outgoing_dirty = self.rows[row.0].source_dirty;
        self.rows[row.0].visibility = value;
        self.rows[row.0].source_epoch = epoch;
        self.mark_root(row, key, false);
        self.rows[row.0].source_dirty = outgoing_dirty;

        Ok(true)
    }

    pub(crate) fn bind(
        &mut self,
        row: Row,
        key: LocalKey,
        event: TypeId,
        binding: ActionBinding,
    ) -> Result<Option<ActionBinding>, ElementError> {
        let epoch = self.issue_epoch()?;
        let outgoing_dirty = self.rows[row.0].source_dirty;
        self.rows[row.0].source_epoch = epoch;
        self.mark_root(row, key, false);
        self.rows[row.0].source_dirty = outgoing_dirty;

        let handlers = &mut self.source_mut(row).handlers;
        if let Some((_, existing)) = handlers.iter_mut().find(|(kind, _)| *kind == event) {
            Ok(Some(std::mem::replace(existing, binding)))
        } else {
            handlers.push((event, binding));
            Ok(None)
        }
    }

    /// Other owners perform the same row move before the returned source is dropped.
    pub(crate) fn remove_row(&mut self, row: Row, key: LocalKey) -> RemovedConfiguration {
        let removed = self.rows.swap_remove(row.0);
        let source = removed.source.and_then(|index| self.take_source(index));

        RemovedConfiguration {
            _input: removed.input,
            _source: source,
            _delivery: self.deliveries.remove(&key),
        }
    }

    pub(crate) fn can_reclaim(&self) -> bool {
        self.rollback_headroom == 0
            && self.snapshots.is_empty()
            && self.retired_environments.is_empty()
            && self
                .deliveries
                .values()
                .all(|delivery| delivery.active_prefix.is_none())
    }

    pub(crate) fn trim(&mut self) {
        if !self.can_reclaim() {
            return;
        }

        if self.deliveries.is_empty() {
            if self
                .deliveries
                .capacity()
                .saturating_mul(size_of::<(LocalKey, PendingDelivery)>())
                > 64 * 1024
            {
                self.deliveries = HashMap::new();
            }
        } else if self.deliveries.capacity() > self.deliveries.len().saturating_mul(4).max(16) {
            self.deliveries
                .shrink_to(self.deliveries.len().saturating_mul(2).max(16));
        }

        if self.live_sources == 0 {
            self.sources.clear();
            self.sources.shrink_to_fit();
            self.free_sources.clear();
            self.free_sources.shrink_to(16);
        } else if self.sources.len() > self.live_sources.saturating_mul(4).max(16) {
            let mut remap = Vec::with_capacity(self.sources.len());
            let mut sources = Vec::with_capacity(self.live_sources.saturating_mul(2));
            for source in self.sources.drain(..) {
                remap.push(source.as_ref().map(|_| SourceIndex::new(sources.len())));
                if let Some(source) = source {
                    sources.push(Some(source));
                }
            }

            for row in &mut self.rows {
                if let Some(index) = row.source {
                    row.source = remap[index.get()];
                }
            }

            self.sources = sources;
            self.free_sources.clear();
            self.free_sources
                .shrink_to(self.live_sources.saturating_mul(2).max(16));
        }

        if self.frontier.capacity() > self.rows.len().saturating_mul(4).max(16)
            && self
                .frontier
                .capacity()
                .saturating_mul(size_of::<LocalKey>())
                > 64 * 1024
        {
            self.frontier
                .shrink_to(self.rows.len().saturating_mul(2).max(16));
        }

        if self.rows.capacity() > self.rows.len().saturating_mul(4).max(16)
            && self.rows.capacity().saturating_mul(size_of::<ConfigRow>()) > 64 * 1024
        {
            self.rows
                .shrink_to(self.rows.len().saturating_mul(2).max(16));
        }
    }
}
