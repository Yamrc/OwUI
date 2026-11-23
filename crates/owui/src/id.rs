//! Domain-qualified external identities and compact window-local slot keys.

use std::hash::Hash;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicU64, Ordering};

slotmap::new_key_type! {
    pub(crate) struct LocalKey;
}

static NEXT_DOMAIN: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Id {
    pub(crate) domain: u64,
    pub(crate) slot: LocalKey,
}

pub(crate) trait Key: Copy + Eq + Hash {
    fn from_id(id: Id) -> Self;
    fn id(self) -> Id;
}

/// The primary identity index; values need not share physical slot positions.
pub(crate) struct Slots<K, V> {
    domain: u64,
    values: slotmap::SlotMap<LocalKey, V>,
    marker: PhantomData<fn() -> K>,
}

impl<K: Key, V> Slots<K, V> {
    pub fn with_key() -> Self {
        let domain = NEXT_DOMAIN
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                value.checked_add(1)
            })
            .expect("identity domains must not wrap and alias another live storage domain");

        Self {
            domain,
            values: slotmap::SlotMap::with_key(),
            marker: PhantomData,
        }
    }

    pub fn qualify(&self, slot: LocalKey) -> K {
        K::from_id(Id {
            domain: self.domain,
            slot,
        })
    }

    pub fn insert(&mut self, value: V) -> K {
        let slot = self.values.insert(value);
        self.qualify(slot)
    }

    pub fn get(&self, key: K) -> Option<&V> {
        let id = key.id();
        if id.domain != self.domain {
            return None;
        }

        self.values.get(id.slot)
    }

    pub fn get_mut(&mut self, key: K) -> Option<&mut V> {
        let id = key.id();
        if id.domain != self.domain {
            return None;
        }

        self.values.get_mut(id.slot)
    }

    pub fn get_local(&self, key: LocalKey) -> Option<&V> {
        self.values.get(key)
    }

    pub fn get_local_mut(&mut self, key: LocalKey) -> Option<&mut V> {
        self.values.get_mut(key)
    }

    pub fn remove(&mut self, key: K) -> Option<V> {
        let id = key.id();
        if id.domain != self.domain {
            return None;
        }

        self.values.remove(id.slot)
    }

    pub fn remove_local(&mut self, key: LocalKey) -> Option<V> {
        self.values.remove(key)
    }

    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = (K, &mut V)> {
        let domain = self.domain;
        self.values
            .iter_mut()
            .map(move |(slot, value)| (K::from_id(Id { domain, slot }), value))
    }
}
