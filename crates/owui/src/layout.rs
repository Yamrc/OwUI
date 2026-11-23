//! Local measurement, committed child uses, and world geometry projection.
//!
//! [`LayoutCx`] measures and places direct children. Size-only uses retain the
//! parent's committed size/participation contribution, separately from a child's
//! latest eager result. Optional groups can reuse unchanged contributions without
//! remeasuring every sibling; their receipts are limited to the executing attempt.
//! Visibility reads also retain participation dependencies after successful layout.
//!
//! Geometry uses finite representable sizes and origins. [`ClipBounds`] preserves
//! half-open clipping endpoints when an origin-and-size [`Rect`] can only provide
//! a conservative cover. A copied geometry or receipt is not proof of a current
//! published window epoch.
//!
//! Framework traversals use worklists. Element child measurement remains a
//! synchronous Rust call chain: callers control their thread stack configuration,
//! and the library neither sets it nor promises unbounded nesting.

mod context;
mod dependency;
mod geometry;
mod measure;
mod projection;

use std::collections::HashMap;

use crate::element::ElementError;
use crate::element::mounted::{MountedElements, Row};
use crate::id::LocalKey;

pub use context::LayoutCx;
pub use dependency::{ChildSizeChange, SizeContribution, SizePlan};
pub use geometry::{ClipBounds, Constraints, Geometry, Point, Rect, Size};

pub(crate) use dependency::{Contribution, propagate_one, suppressed};
pub(crate) use measure::measure;
pub(crate) use projection::project_one;

use dependency::{DirtySizes, IncomingProbe, LayoutSnapshot, ParentAttempt, SizeGroup, Uses};

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct MeasureKey {
    pub(crate) constraints: Constraints,
    pub(crate) viewport_epoch: u64,
}

#[derive(Default)]
pub(crate) struct LayoutRecord {
    pub(crate) geometry: Geometry,
    pub(crate) measurement: Option<MeasureKey>,
    pub(crate) demand: bool,
    pub(crate) parked: bool,
    pub(crate) park_resume_required: bool,
    pub(crate) laying_out: bool,
    pub(crate) participating: bool,
    pub(crate) visible: bool,
    pub(crate) enabled: bool,
    pub(crate) world_set: bool,
    pub(crate) clip_children: bool,
    pub(crate) hit_testable: bool,
    pub(crate) content_set: bool,

    pub(crate) published_clip_children: bool,
    pub(crate) published_hit_testable: bool,
    pub(crate) published_collapsed: bool,

    changed_queued: bool,
    qualification_queued: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MeasureMode {
    Conservative,
    SizeOnly,
    Eager,
}

/// Layout data shares the primary dense row coordinates without owning identities.
pub(crate) struct Layout {
    pub(crate) records: Vec<LayoutRecord>,
    viewport: Rect,
    viewport_epoch: u64,

    attempts: Vec<ParentAttempt>,
    uses_scratch: Vec<Uses>,
    sparse_uses_scratch: Vec<Uses>,
    dirty_scratch: Vec<DirtySizes>,
    probes: Vec<IncomingProbe>,
    groups: HashMap<LocalKey, SizeGroup>,
    group_trim: Vec<LocalKey>,
    groups_capacity: usize,
    dependency_capacity: usize,

    clip_overrides: HashMap<LocalKey, ClipBounds>,
    clip_capacity: usize,
    pub(crate) dependencies: HashMap<LocalKey, Contribution>,

    pub(crate) changed: Vec<LocalKey>,
    pub(crate) qualification_changed: Vec<LocalKey>,
}

impl Layout {
    pub(crate) fn new(size: Size) -> Self {
        Self {
            records: Vec::new(),
            viewport: Rect::new(Point::ZERO, size),
            viewport_epoch: 0,
            attempts: Vec::new(),
            uses_scratch: Vec::new(),
            sparse_uses_scratch: Vec::new(),
            dirty_scratch: Vec::new(),
            probes: Vec::new(),
            groups: HashMap::new(),
            group_trim: Vec::new(),
            groups_capacity: 0,
            dependency_capacity: 0,
            clip_overrides: HashMap::new(),
            clip_capacity: 0,
            dependencies: HashMap::new(),
            changed: Vec::new(),
            qualification_changed: Vec::new(),
        }
    }

    pub(crate) fn reserve_row(&mut self) {
        self.records.reserve(1);
    }

    pub(crate) fn append(&mut self) {
        self.records.push(LayoutRecord::default());
    }

    pub(crate) fn viewport(&self) -> Rect {
        self.viewport
    }

    pub(crate) fn viewport_epoch(&self) -> u64 {
        self.viewport_epoch
    }

    pub(crate) fn set_viewport(&mut self, size: Size) -> Result<bool, ElementError> {
        if !size.is_valid() {
            return Err(ElementError::InvalidGeometry);
        }

        let next = Rect::new(Point::ZERO, size);
        if next == self.viewport {
            return Ok(false);
        }

        let epoch = self
            .viewport_epoch
            .checked_add(1)
            .ok_or(ElementError::RevisionExhausted)?;

        self.viewport = next;
        self.viewport_epoch = epoch;

        Ok(true)
    }

    pub(crate) fn invalidate(&mut self, row: Row) {
        self.records[row.0].measurement = None;
    }

    pub(crate) fn remove_row(&mut self, row: Row, key: LocalKey, elements: &MountedElements) {
        assert!(
            self.attempts.is_empty(),
            "layout rows cannot move while measurement snapshots are active"
        );

        self.records.swap_remove(row.0);
        self.dependencies.remove(&key);
        self.clip_overrides.remove(&key);
        self.groups.remove(&key);

        if let Some(parent) = elements
            .parent(key)
            .and_then(|parent| self.groups.get_mut(&parent))
        {
            parent.dirty.remove(key);
            parent.valid = false;
        }
    }

    pub(crate) fn can_reclaim(&self) -> bool {
        self.attempts.is_empty() && self.probes.is_empty()
    }

    pub(crate) fn trim(&mut self) {
        assert!(
            self.can_reclaim(),
            "layout caches shrink only after attempts and probes finish"
        );

        while let Some(key) = self.group_trim.pop() {
            let dirty = if let Some(group) = self.groups.get_mut(&key) {
                group.trim_queued = false;
                if let Some(dirty) = group.take_idle_dirty() {
                    Some(dirty)
                } else {
                    group.dirty.trim();
                    None
                }
            } else {
                None
            };
            if let Some(dirty) = dirty {
                self.recycle_dirty(dirty);
            }
        }

        let mut scratch_bytes = self
            .dependencies
            .len()
            .saturating_mul(size_of::<(LocalKey, Contribution)>() + size_of::<LocalKey>() + 1)
            .saturating_mul(4)
            .saturating_add(64 * 1024)
            .min(16 * 1024 * 1024);

        self.uses_scratch.retain(|uses| {
            let bytes = uses.bytes();
            if bytes > scratch_bytes {
                return false;
            }
            scratch_bytes -= bytes;
            true
        });
        self.sparse_uses_scratch.retain(|uses| {
            let bytes = uses.bytes();
            if bytes > scratch_bytes {
                return false;
            }
            scratch_bytes -= bytes;
            true
        });
        self.dirty_scratch.retain(|dirty| {
            let bytes = dirty.bytes();
            if bytes > scratch_bytes {
                return false;
            }
            scratch_bytes -= bytes;
            true
        });

        if self
            .uses_scratch
            .capacity()
            .saturating_mul(size_of::<Uses>())
            > 64 * 1024
        {
            self.uses_scratch.shrink_to(self.uses_scratch.len());
        }
        if self
            .sparse_uses_scratch
            .capacity()
            .saturating_mul(size_of::<Uses>())
            > 64 * 1024
        {
            self.sparse_uses_scratch
                .shrink_to(self.sparse_uses_scratch.len());
        }
        if self
            .dirty_scratch
            .capacity()
            .saturating_mul(size_of::<DirtySizes>())
            > 64 * 1024
        {
            self.dirty_scratch.shrink_to(self.dirty_scratch.len());
        }

        self.groups_capacity = self.groups_capacity.max(self.groups.capacity());
        if self.groups_capacity > self.groups.len().saturating_mul(4).max(16)
            && self
                .groups_capacity
                .saturating_mul(size_of::<(LocalKey, SizeGroup)>() + 1)
                > 64 * 1024
        {
            self.groups
                .shrink_to(self.groups.len().saturating_mul(2).max(16));
            self.groups_capacity = self.groups.capacity();
        }

        self.clip_capacity = self.clip_capacity.max(self.clip_overrides.capacity());
        if self.clip_capacity > self.clip_overrides.len().saturating_mul(4).max(16)
            && self
                .clip_capacity
                .saturating_mul(size_of::<(LocalKey, ClipBounds)>() + 1)
                > 64 * 1024
        {
            self.clip_overrides
                .shrink_to(self.clip_overrides.len().saturating_mul(2).max(16));
            self.clip_capacity = self.clip_overrides.capacity();
        }

        self.dependency_capacity = self.dependency_capacity.max(self.dependencies.capacity());
        if self.dependency_capacity > self.dependencies.len().saturating_mul(4).max(16)
            && self
                .dependency_capacity
                .saturating_mul(size_of::<(LocalKey, Contribution)>() + 1)
                > 64 * 1024
        {
            self.dependencies
                .shrink_to(self.dependencies.len().saturating_mul(2).max(16));
            self.dependency_capacity = self.dependencies.capacity();
        }

        if self
            .group_trim
            .capacity()
            .saturating_mul(size_of::<LocalKey>())
            > 64 * 1024
        {
            self.group_trim.shrink_to(16);
        }
        if self
            .probes
            .capacity()
            .saturating_mul(size_of::<IncomingProbe>())
            > 64 * 1024
        {
            self.probes.shrink_to(16);
        }

        if self.records.capacity() > self.records.len().saturating_mul(4).max(16)
            && self
                .records
                .capacity()
                .saturating_mul(size_of::<LayoutRecord>())
                > 64 * 1024
        {
            self.records
                .shrink_to(self.records.len().saturating_mul(2).max(16));
        }

        if self.attempts.is_empty()
            && self
                .attempts
                .capacity()
                .saturating_mul(size_of::<LayoutSnapshot>())
                > 64 * 1024
        {
            self.attempts.shrink_to(16);
        }
    }
}
