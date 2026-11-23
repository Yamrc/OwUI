//! Typed property declarations with shared metadata and inline storage for common value shapes.

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::ops::Deref;
use std::rc::Rc;
use std::sync::Arc;

use crate::color::Color;
use crate::layout::Size;
use crate::schedule::Work;

/// Defines a property independently of any particular element implementation.
pub trait Property: 'static {
    type Value: Clone + PartialEq + 'static;

    const AFFECTS: Work;
    const INHERITED: bool = false;

    fn default_value() -> Self::Value;
}

enum Value {
    Bool(bool),
    Float(f32),
    Integer(usize),
    Color(Color),
    Size(Size),
    Text(Arc<str>),
    Other(Box<dyn Any>),
}

impl Value {
    fn new<T: Clone + 'static>(value: T) -> Self {
        let any = &value as &dyn Any;

        if let Some(value) = any.downcast_ref::<bool>() {
            Self::Bool(*value)
        } else if let Some(value) = any.downcast_ref::<f32>() {
            Self::Float(*value)
        } else if let Some(value) = any.downcast_ref::<usize>() {
            Self::Integer(*value)
        } else if let Some(value) = any.downcast_ref::<Color>() {
            Self::Color(*value)
        } else if let Some(value) = any.downcast_ref::<Size>() {
            Self::Size(*value)
        } else if let Some(value) = any.downcast_ref::<Arc<str>>() {
            Self::Text(value.clone())
        } else {
            Self::Other(Box::new(value))
        }
    }

    fn any(&self) -> &dyn Any {
        match self {
            Self::Bool(value) => value,
            Self::Float(value) => value,
            Self::Integer(value) => value,
            Self::Color(value) => value,
            Self::Size(value) => value,
            Self::Text(value) => value,
            Self::Other(value) => value.as_ref(),
        }
    }
}

struct Descriptor {
    key: TypeId,
    affects: Work,
    inherited: bool,
    equal: fn(&Value, &Value) -> bool,
    clone_value: fn(&Value) -> Value,
}

fn equal_values<P: Property>(left: &Value, right: &Value) -> bool {
    let left = left
        .any()
        .downcast_ref::<P::Value>()
        .expect("property entries preserve their declared value type");
    let right = right
        .any()
        .downcast_ref::<P::Value>()
        .expect("property entries preserve their declared value type");
    left == right
}

fn clone_value<P: Property>(source: &Value) -> Value {
    let value = source
        .any()
        .downcast_ref::<P::Value>()
        .expect("property entries preserve their declared value type");
    Value::new(value.clone())
}

pub(crate) struct Entry {
    descriptor: &'static Descriptor,
    value: Value,
}

impl Entry {
    fn new<P: Property>(value: P::Value) -> Self {
        Self {
            descriptor: const {
                &Descriptor {
                    key: TypeId::of::<P>(),
                    affects: P::AFFECTS,
                    inherited: P::INHERITED,
                    equal: equal_values::<P>,
                    clone_value: clone_value::<P>,
                }
            },
            value: Value::new(value),
        }
    }

    fn change(&self) -> PropertyChange {
        PropertyChange {
            key: self.descriptor.key,
            affects: self.descriptor.affects,
            inherited: self.descriptor.inherited,
        }
    }
}

impl Clone for Entry {
    fn clone(&self) -> Self {
        Self {
            descriptor: self.descriptor,
            value: (self.descriptor.clone_value)(&self.value),
        }
    }
}

impl PartialEq for Entry {
    fn eq(&self, other: &Self) -> bool {
        self.descriptor.key == other.descriptor.key
            && (self.descriptor.equal)(&self.value, &other.value)
    }
}

/// Identifies a changed value or presence independently of its processing flags.
///
/// `inherited` describes the property's declaration, not the source that supplied its value.
#[derive(Clone, Copy)]
pub(crate) struct PropertyChange {
    pub key: TypeId,
    pub affects: Work,
    pub inherited: bool,
}

/// Owned change keys with inline storage for a single entry.
#[derive(Clone, Default)]
pub(crate) enum PropertyChanges {
    #[default]
    Empty,
    One(PropertyChange),
    Many(Vec<PropertyChange>),
}

impl PropertyChanges {
    pub(crate) fn push(&mut self, change: PropertyChange) {
        match self {
            Self::Empty => *self = Self::One(change),
            Self::One(previous) => *self = Self::Many(vec![*previous, change]),
            Self::Many(changes) => changes.push(change),
        }
    }

    /// Coalesces only the suffix; an active delivery's entered prefix remains unchanged.
    pub(crate) fn merge_suffix(&mut self, changes: &[PropertyChange], start: usize) {
        for change in changes {
            if let Some(existing) = self.slice_mut()[start..]
                .iter_mut()
                .find(|existing| existing.key == change.key)
            {
                existing.affects |= change.affects;
            } else {
                self.push(*change);
            }
        }
    }

    pub(crate) fn discard_prefix(&mut self, count: usize) {
        assert!(
            count <= self.len(),
            "configuration delivery cannot acknowledge more keys than its retained ledger"
        );
        if count == 0 {
            return;
        }

        match self {
            Self::Empty | Self::One(_) => *self = Self::Empty,
            Self::Many(changes) => {
                drop(changes.drain(..count));
                self.collapse_small();
            }
        }
    }

    pub(crate) fn coalesce(&mut self) {
        let Self::Many(changes) = self else {
            return;
        };

        let mut index = 1;
        while index < changes.len() {
            let change = changes[index];
            if let Some(previous) = changes[..index]
                .iter_mut()
                .find(|previous| previous.key == change.key)
            {
                previous.affects |= change.affects;
                changes.remove(index);
            } else {
                index += 1;
            }
        }

        self.collapse_small();
    }

    fn slice_mut(&mut self) -> &mut [PropertyChange] {
        match self {
            Self::Empty => &mut [],
            Self::One(change) => std::slice::from_mut(change),
            Self::Many(changes) => changes.as_mut_slice(),
        }
    }

    fn collapse_small(&mut self) {
        let Self::Many(changes) = self else {
            return;
        };

        let small = match changes.as_slice() {
            [] => Self::Empty,
            [change] => Self::One(*change),
            _ => return,
        };
        *self = small;
    }
}

impl Deref for PropertyChanges {
    type Target = [PropertyChange];

    fn deref(&self) -> &Self::Target {
        match self {
            Self::Empty => &[],
            Self::One(change) => std::slice::from_ref(change),
            Self::Many(changes) => changes.as_slice(),
        }
    }
}

/// A small declaration set. Common values do not allocate individual boxes.
#[derive(Default)]
pub struct Properties {
    entries: Vec<Entry>,
}

const LOCAL_TYPE_INDEX_FROM: usize = 8;
const INLINE_LOCAL_TYPES: usize = 32;

struct LocalTypeIndex {
    inline: [Option<(TypeId, usize)>; INLINE_LOCAL_TYPES],
    len: usize,
    heap: Vec<(TypeId, usize)>,
}

impl LocalTypeIndex {
    fn new(properties: &Properties) -> Self {
        let mut index = Self {
            inline: [None; INLINE_LOCAL_TYPES],
            len: 0,
            heap: Vec::new(),
        };
        if properties.entries.len() <= INLINE_LOCAL_TYPES {
            for (position, entry) in properties.entries.iter().enumerate() {
                index.inline[position] = Some((entry.descriptor.key, position));
            }
            index.len = properties.entries.len();
            index.inline[..index.len].sort_unstable_by_key(|entry| {
                entry
                    .expect("the populated local TypeId prefix is complete")
                    .0
            });
        } else {
            index.heap = properties
                .entries
                .iter()
                .enumerate()
                .map(|(position, entry)| (entry.descriptor.key, position))
                .collect();
            index.heap.sort_unstable_by_key(|(key, _)| *key);
        }
        index
    }

    fn position(&self, key: TypeId) -> Option<usize> {
        if !self.heap.is_empty() {
            return self
                .heap
                .binary_search_by_key(&key, |(entry, _)| *entry)
                .ok()
                .map(|position| self.heap[position].1);
        }
        self.inline[..self.len]
            .binary_search_by_key(&key, |entry| {
                entry
                    .expect("the populated local TypeId prefix is complete")
                    .0
            })
            .ok()
            .and_then(|position| self.inline[position].map(|(_, position)| position))
    }
}

#[derive(Clone, Copy)]
enum LocalLookup<'a> {
    Linear,
    Indexed(&'a LocalTypeIndex),
}

impl LocalLookup<'_> {
    fn contains(self, properties: &Properties, key: TypeId) -> bool {
        match self {
            Self::Linear => properties.entry(key).is_some(),
            Self::Indexed(index) => index.position(key).is_some(),
        }
    }

    fn entry(self, properties: &Properties, key: TypeId) -> Option<&Entry> {
        match self {
            Self::Linear => properties.entry(key),
            Self::Indexed(index) => index
                .position(key)
                .map(|position| &properties.entries[position]),
        }
    }
}

#[derive(Clone, Copy)]
struct LocalProperties<'a> {
    values: &'a Properties,
    types: LocalLookup<'a>,
}

// The lookup owns only TypeIds and positions, never Values or snapshot handles.
pub(crate) struct InheritedValues {
    values: Properties,
    lookup: Option<Rc<HashMap<TypeId, usize>>>,
}

impl InheritedValues {
    fn with_lookup(values: Properties, existing: Option<&Self>, input: Option<&Self>) -> Self {
        let lookup = if values.entries.len() < 64 {
            None
        } else {
            let shared = [existing, input].into_iter().flatten().find_map(|source| {
                source.lookup.as_ref().filter(|_| {
                    values.entries.len() == source.values.entries.len()
                        && values
                            .entries
                            .iter()
                            .zip(&source.values.entries)
                            .all(|(left, right)| left.descriptor.key == right.descriptor.key)
                })
            });
            Some(match shared {
                Some(lookup) => Rc::clone(lookup),
                None => Rc::new(
                    values
                        .entries
                        .iter()
                        .enumerate()
                        .map(|(position, entry)| (entry.descriptor.key, position))
                        .collect(),
                ),
            })
        };
        Self { values, lookup }
    }

    pub(crate) fn get_ref<P: Property>(&self) -> Option<&P::Value> {
        self.entry(TypeId::of::<P>())
            .and_then(|entry| entry.value.any().downcast_ref::<P::Value>())
    }

    fn entry(&self, key: TypeId) -> Option<&Entry> {
        match &self.lookup {
            Some(lookup) => lookup
                .get(&key)
                .map(|&position| &self.values.entries[position]),
            None => self.values.entry(key),
        }
    }

    pub(crate) fn equivalent(&self, other: &Self) -> bool {
        self.values.equivalent(&other.values)
    }

    fn same_inherited_shape(&self, other: &Self) -> bool {
        let mut current = self
            .values
            .entries
            .iter()
            .filter(|entry| entry.descriptor.inherited);
        let mut previous = other
            .values
            .entries
            .iter()
            .filter(|entry| entry.descriptor.inherited);

        loop {
            match (current.next(), previous.next()) {
                (Some(current), Some(previous))
                    if current.descriptor.key == previous.descriptor.key => {}
                (None, None) => return true,
                _ => return false,
            }
        }
    }
}

impl Properties {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set<P: Property>(&mut self, property: P, value: P::Value) {
        drop(self.set_deferred(property, value));
    }

    /// Installs an explicit value and returns the replaced entry without dropping it.
    ///
    /// The property token is dropped before mutation. The caller owns the removed entry and
    /// must finish mutation bookkeeping before dropping it.
    pub(crate) fn set_deferred<P: Property>(
        &mut self,
        property: P,
        value: P::Value,
    ) -> Option<Entry> {
        drop(property);
        self.set_value_deferred::<P>(value)
    }

    /// Installs a value by property type and returns the replaced entry without dropping it.
    ///
    /// No property token is accepted, so its destruction can precede external bookkeeping.
    /// The caller must finish mutation bookkeeping before dropping the returned entry.
    pub(crate) fn set_value_deferred<P: Property>(&mut self, value: P::Value) -> Option<Entry> {
        let entry = Entry::new::<P>(value);

        if let Some(existing) = self
            .entries
            .iter_mut()
            .find(|item| item.descriptor.key == entry.descriptor.key)
        {
            Some(std::mem::replace(existing, entry))
        } else {
            self.entries.push(entry);
            None
        }
    }

    /// Returns a cloned explicit value, or the property's default when absent.
    pub fn get<P: Property>(&self, _property: P) -> P::Value {
        self.get_type::<P>()
    }

    pub(crate) fn get_type<P: Property>(&self) -> P::Value {
        self.get_ref::<P>()
            .cloned()
            .unwrap_or_else(P::default_value)
    }

    pub(crate) fn get_ref<P: Property>(&self) -> Option<&P::Value> {
        self.entry(TypeId::of::<P>())
            .and_then(|entry| entry.value.any().downcast_ref::<P::Value>())
    }

    pub(crate) fn has_inherited(&self) -> bool {
        self.entries.iter().any(|entry| entry.descriptor.inherited)
    }

    /// Reports whether an explicit value is present, including an explicit default value.
    pub fn contains<P: Property>(&self, _property: P) -> bool {
        self.entry(TypeId::of::<P>()).is_some()
    }

    pub fn remove<P: Property>(&mut self, property: P) {
        drop(self.remove_deferred(property));
    }

    /// Removes an explicit value and returns its entry without dropping it.
    ///
    /// The property token is dropped before mutation. The caller owns the removed entry and
    /// must finish mutation bookkeeping before dropping it.
    pub(crate) fn remove_deferred<P: Property>(&mut self, property: P) -> Option<Entry> {
        drop(property);
        self.remove_type_deferred::<P>()
    }

    /// Removes a value by property type and returns its entry without dropping it.
    ///
    /// No property token is accepted, so its destruction can precede external bookkeeping.
    /// The caller must finish mutation bookkeeping before dropping the returned entry.
    pub(crate) fn remove_type_deferred<P: Property>(&mut self) -> Option<Entry> {
        let index = self
            .entries
            .iter()
            .position(|entry| entry.descriptor.key == TypeId::of::<P>())?;
        Some(self.entries.remove(index))
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub(crate) fn equivalent(&self, previous: &Self) -> bool {
        self.entries.len() == previous.entries.len()
            && self.entries.iter().all(|entry| {
                previous
                    .entry(entry.descriptor.key)
                    .is_some_and(|old| entry == old)
            })
    }

    /// Compares effective values and presence without cloning either configuration.
    ///
    /// Local values mask the corresponding input values. Input values participate only for
    /// inherited properties; an absent value differs from an explicit default value.
    pub(crate) fn effective_changes(
        new_local: &Self,
        new_input: Option<&InheritedValues>,
        old_local: &Self,
        old_input: Option<&InheritedValues>,
    ) -> PropertyChanges {
        if Self::same_local_shape(new_local, old_local)
            && Self::same_input_shape(new_input, old_input)
        {
            if new_local.entries.len() >= LOCAL_TYPE_INDEX_FROM {
                let types = LocalTypeIndex::new(new_local);
                let lookup = LocalLookup::Indexed(&types);
                return Self::changes_with_same_shape(
                    LocalProperties {
                        values: new_local,
                        types: lookup,
                    },
                    new_input,
                    LocalProperties {
                        values: old_local,
                        types: lookup,
                    },
                    old_input,
                );
            }

            return Self::changes_with_same_shape(
                LocalProperties {
                    values: new_local,
                    types: LocalLookup::Linear,
                },
                new_input,
                LocalProperties {
                    values: old_local,
                    types: LocalLookup::Linear,
                },
                old_input,
            );
        }
        if new_local.entries.is_empty() && old_local.entries.is_empty() {
            return Self::inherited_changes(new_input, old_input);
        }

        match (
            new_local.entries.len() >= LOCAL_TYPE_INDEX_FROM,
            old_local.entries.len() >= LOCAL_TYPE_INDEX_FROM,
        ) {
            (false, false) => Self::changes_by_linear(
                new_local,
                new_input,
                old_local,
                old_input,
                |key| new_input.and_then(|input| input.values.entry(key)),
                |key| old_input.and_then(|input| input.values.entry(key)),
            ),
            (true, false) => {
                let new_types = LocalTypeIndex::new(new_local);
                Self::effective_changes_with_local_indexes(
                    LocalProperties {
                        values: new_local,
                        types: LocalLookup::Indexed(&new_types),
                    },
                    new_input,
                    LocalProperties {
                        values: old_local,
                        types: LocalLookup::Linear,
                    },
                    old_input,
                )
            }
            (false, true) => {
                let old_types = LocalTypeIndex::new(old_local);
                Self::effective_changes_with_local_indexes(
                    LocalProperties {
                        values: new_local,
                        types: LocalLookup::Linear,
                    },
                    new_input,
                    LocalProperties {
                        values: old_local,
                        types: LocalLookup::Indexed(&old_types),
                    },
                    old_input,
                )
            }
            (true, true) => {
                let new_types = LocalTypeIndex::new(new_local);
                let old_types = LocalTypeIndex::new(old_local);
                Self::effective_changes_with_local_indexes(
                    LocalProperties {
                        values: new_local,
                        types: LocalLookup::Indexed(&new_types),
                    },
                    new_input,
                    LocalProperties {
                        values: old_local,
                        types: LocalLookup::Indexed(&old_types),
                    },
                    old_input,
                )
            }
        }
    }

    fn same_local_shape(new_local: &Properties, old_local: &Properties) -> bool {
        new_local.entries.len() == old_local.entries.len()
            && new_local
                .entries
                .iter()
                .zip(&old_local.entries)
                .all(|(new, old)| new.descriptor.key == old.descriptor.key)
    }

    fn same_input_shape(
        new_input: Option<&InheritedValues>,
        old_input: Option<&InheritedValues>,
    ) -> bool {
        match (new_input, old_input) {
            (None, None) => true,
            (Some(new), Some(old)) => new.same_inherited_shape(old),
            _ => false,
        }
    }

    fn changes_with_same_shape(
        new_local: LocalProperties<'_>,
        new_input: Option<&InheritedValues>,
        old_local: LocalProperties<'_>,
        old_input: Option<&InheritedValues>,
    ) -> PropertyChanges {
        let mut changes = PropertyChanges::default();
        for (new, old) in new_local
            .values
            .entries
            .iter()
            .zip(&old_local.values.entries)
        {
            if new != old {
                changes.push(new.change());
            }
        }

        let new_inherited = new_input
            .into_iter()
            .flat_map(|input| input.values.entries.iter())
            .filter(|entry| {
                entry.descriptor.inherited
                    && !new_local
                        .types
                        .contains(new_local.values, entry.descriptor.key)
            });
        let mut old_inherited = old_input
            .into_iter()
            .flat_map(|input| input.values.entries.iter())
            .filter(|entry| {
                entry.descriptor.inherited
                    && !old_local
                        .types
                        .contains(old_local.values, entry.descriptor.key)
            });
        for new in new_inherited {
            let old = old_inherited
                .next()
                .expect("equal effective TypeId shapes retain paired inherited entries");
            debug_assert_eq!(new.descriptor.key, old.descriptor.key);
            if new != old {
                changes.push(new.change());
            }
        }
        debug_assert!(old_inherited.next().is_none());

        changes
    }

    fn inherited_changes(
        new_input: Option<&InheritedValues>,
        old_input: Option<&InheritedValues>,
    ) -> PropertyChanges {
        let mut changes = PropertyChanges::default();

        if let Some(new_input) = new_input {
            for entry in new_input
                .values
                .entries
                .iter()
                .filter(|entry| entry.descriptor.inherited)
            {
                match old_input
                    .and_then(|input| input.entry(entry.descriptor.key))
                    .filter(|old| old.descriptor.inherited)
                {
                    Some(old) if entry == old => {}
                    _ => changes.push(entry.change()),
                }
            }
        }

        if let Some(old_input) = old_input {
            for entry in old_input
                .values
                .entries
                .iter()
                .filter(|entry| entry.descriptor.inherited)
            {
                if new_input
                    .and_then(|input| input.entry(entry.descriptor.key))
                    .filter(|new| new.descriptor.inherited)
                    .is_none()
                {
                    changes.push(entry.change());
                }
            }
        }

        changes
    }

    fn changes_by_linear<'new, 'old>(
        new_local: &'new Self,
        new_input: Option<&'new InheritedValues>,
        old_local: &'old Self,
        old_input: Option<&'old InheritedValues>,
        new_find: impl Fn(TypeId) -> Option<&'new Entry>,
        old_find: impl Fn(TypeId) -> Option<&'old Entry>,
    ) -> PropertyChanges {
        let mut changes = PropertyChanges::default();

        for entry in new_local.effective_entries(new_input) {
            let old = old_local
                .entry(entry.descriptor.key)
                .or_else(|| old_find(entry.descriptor.key).filter(|old| old.descriptor.inherited));
            match old {
                Some(old) if entry == old => {}
                _ => changes.push(entry.change()),
            }
        }

        for old in old_local.effective_entries(old_input) {
            let new = new_local
                .entry(old.descriptor.key)
                .or_else(|| new_find(old.descriptor.key).filter(|new| new.descriptor.inherited));
            if new.is_none() {
                changes.push(old.change());
            }
        }

        changes
    }

    fn effective_changes_with_local_indexes<'new, 'old>(
        new_local: LocalProperties<'new>,
        new_input: Option<&'new InheritedValues>,
        old_local: LocalProperties<'old>,
        old_input: Option<&'old InheritedValues>,
    ) -> PropertyChanges {
        let new_linear = |key| new_input.and_then(|input| input.values.entry(key));
        let old_linear = |key| old_input.and_then(|input| input.values.entry(key));
        let new_index = new_input.and_then(|input| {
            input
                .lookup
                .as_deref()
                .map(|lookup| (&input.values, lookup))
        });
        let old_index = old_input.and_then(|input| {
            input
                .lookup
                .as_deref()
                .map(|lookup| (&input.values, lookup))
        });
        match (new_index, old_index) {
            (None, None) => Self::changes_by(
                new_local, new_input, old_local, old_input, new_linear, old_linear,
            ),
            (Some((values, lookup)), None) => Self::changes_by(
                new_local,
                new_input,
                old_local,
                old_input,
                |key| lookup.get(&key).map(|&position| &values.entries[position]),
                old_linear,
            ),
            (None, Some((values, lookup))) => Self::changes_by(
                new_local,
                new_input,
                old_local,
                old_input,
                new_linear,
                |key| lookup.get(&key).map(|&position| &values.entries[position]),
            ),
            (Some((new_values, new_lookup)), Some((old_values, old_lookup))) => Self::changes_by(
                new_local,
                new_input,
                old_local,
                old_input,
                |key| {
                    new_lookup
                        .get(&key)
                        .map(|&position| &new_values.entries[position])
                },
                |key| {
                    old_lookup
                        .get(&key)
                        .map(|&position| &old_values.entries[position])
                },
            ),
        }
    }

    fn changes_by<'new, 'old>(
        new_local: LocalProperties<'new>,
        new_input: Option<&'new InheritedValues>,
        old_local: LocalProperties<'old>,
        old_input: Option<&'old InheritedValues>,
        new_find: impl Fn(TypeId) -> Option<&'new Entry>,
        old_find: impl Fn(TypeId) -> Option<&'old Entry>,
    ) -> PropertyChanges {
        let mut changes = PropertyChanges::default();

        for entry in new_local
            .values
            .effective_entries_indexed(new_input, new_local.types)
        {
            match old_local
                .values
                .effective_entry(old_local.types, &old_find, entry.descriptor.key)
            {
                Some(old) if entry == old => {}
                _ => changes.push(entry.change()),
            }
        }

        for old in old_local
            .values
            .effective_entries_indexed(old_input, old_local.types)
        {
            if new_local
                .values
                .effective_entry(new_local.types, &new_find, old.descriptor.key)
                .is_none()
            {
                changes.push(old.change());
            }
        }

        changes
    }

    /// Composes an inherited environment, comparing borrowed candidates before cloning values.
    ///
    /// With no inherited local values, the input handle is reused. Otherwise an equivalent
    /// existing output is reused; only a changed output requires owned value clones. Input and
    /// existing output are already composed inherited environments and remain unmodified.
    pub(crate) fn compose_inherited(
        &self,
        input: Option<&Rc<InheritedValues>>,
        existing: Option<&Rc<InheritedValues>>,
    ) -> Option<Rc<InheritedValues>> {
        if !self.has_inherited() {
            return input.cloned();
        }

        let input_values = input.map(|input| input.as_ref());
        if let Some(existing) = existing
            && self.inherited_matches(input_values, existing.as_ref())
        {
            return Some(Rc::clone(existing));
        }

        let entries = self
            .effective_entries(input_values)
            .filter(|entry| entry.descriptor.inherited)
            .cloned()
            .collect();

        Some(Rc::new(InheritedValues::with_lookup(
            Self { entries },
            existing.map(|values| values.as_ref()),
            input_values,
        )))
    }

    fn entry(&self, key: TypeId) -> Option<&Entry> {
        self.entries
            .iter()
            .find(|entry| entry.descriptor.key == key)
    }

    fn effective_entry<'a>(
        &'a self,
        local_types: LocalLookup<'_>,
        find: &impl Fn(TypeId) -> Option<&'a Entry>,
        key: TypeId,
    ) -> Option<&'a Entry> {
        local_types
            .entry(self, key)
            .or_else(|| find(key).filter(|entry| entry.descriptor.inherited))
    }

    fn effective_entries<'a>(
        &'a self,
        input: Option<&'a InheritedValues>,
    ) -> impl Iterator<Item = &'a Entry> {
        self.entries.iter().chain(
            input
                .into_iter()
                .flat_map(|input| input.values.entries.iter())
                .filter(move |entry| {
                    entry.descriptor.inherited && self.entry(entry.descriptor.key).is_none()
                }),
        )
    }

    fn effective_entries_indexed<'a>(
        &'a self,
        input: Option<&'a InheritedValues>,
        local_types: LocalLookup<'_>,
    ) -> impl Iterator<Item = &'a Entry> {
        self.entries.iter().chain(
            input
                .into_iter()
                .flat_map(|input| input.values.entries.iter())
                .filter(move |entry| {
                    entry.descriptor.inherited && !local_types.contains(self, entry.descriptor.key)
                }),
        )
    }

    fn inherited_matches(&self, input: Option<&InheritedValues>, output: &InheritedValues) -> bool {
        match output.lookup.as_deref() {
            None => self.matches_by(input, output, |key| output.values.entry(key)),
            Some(lookup) => self.matches_by(input, output, |key| {
                lookup
                    .get(&key)
                    .map(|&position| &output.values.entries[position])
            }),
        }
    }

    fn matches_by<'output>(
        &self,
        input: Option<&InheritedValues>,
        output: &'output InheritedValues,
        find: impl Fn(TypeId) -> Option<&'output Entry>,
    ) -> bool {
        let mut count = 0;
        for entry in self
            .effective_entries(input)
            .filter(|entry| entry.descriptor.inherited)
        {
            if !find(entry.descriptor.key).is_some_and(|old| entry == old) {
                return false;
            }
            count += 1;
        }

        count == output.values.entries.len()
    }
}

impl Clone for Properties {
    fn clone(&self) -> Self {
        Self {
            entries: self.entries.clone(),
        }
    }
}

/// A background fill shared by built-in and external elements.
pub struct Background;

impl Property for Background {
    type Value = Color;
    const AFFECTS: Work = Work::SCENE;

    fn default_value() -> Color {
        Color::TRANSPARENT
    }
}

pub struct Foreground;

impl Property for Foreground {
    type Value = Color;
    const AFFECTS: Work = Work::SCENE;
    const INHERITED: bool = true;

    fn default_value() -> Color {
        Color::WHITE
    }
}

pub struct Gap;

impl Property for Gap {
    type Value = f32;
    const AFFECTS: Work = Work::LAYOUT;

    fn default_value() -> f32 {
        0.0
    }
}

pub struct Width;

impl Property for Width {
    type Value = f32;
    const AFFECTS: Work = Work::LAYOUT;

    fn default_value() -> f32 {
        f32::INFINITY
    }
}

pub struct Height;

impl Property for Height {
    type Value = f32;
    const AFFECTS: Work = Work::LAYOUT;

    fn default_value() -> f32 {
        f32::INFINITY
    }
}

pub struct Enabled;

impl Property for Enabled {
    type Value = bool;
    const AFFECTS: Work = Work::SEMANTICS;

    fn default_value() -> bool {
        true
    }
}

pub struct Name;

impl Property for Name {
    type Value = Arc<str>;
    const AFFECTS: Work = Work::SEMANTICS;

    fn default_value() -> Arc<str> {
        Arc::from("")
    }
}
