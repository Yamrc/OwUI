//! World geometry attachment, exact clip exceptions and output qualification deltas.

use crate::element::mounted::{MountedElements, Row};
use crate::element::{ElementError, Visibility};
use crate::id::LocalKey;
use crate::schedule::Work;

use super::{ClipBounds, Geometry, Layout, Point, Rect, Size};

impl Layout {
    pub(crate) fn mark_changed(&mut self, key: LocalKey, row: Row) {
        if !self.records[row.0].changed_queued {
            self.records[row.0].changed_queued = true;
            self.changed.push(key);
        }
    }

    pub(crate) fn mark_qualification(&mut self, key: LocalKey, row: Row) {
        if !self.records[row.0].qualification_queued {
            self.records[row.0].qualification_queued = true;
            self.qualification_changed.push(key);
        }
    }

    pub(crate) fn clear_deltas(&mut self, elements: &MountedElements) {
        for key in self.changed.drain(..) {
            if let Some(row) = elements.row_local(key) {
                self.records[row.0].changed_queued = false;
            }
        }

        for key in self.qualification_changed.drain(..) {
            if let Some(row) = elements.row_local(key) {
                self.records[row.0].qualification_queued = false;
            }
        }
    }

    pub(crate) fn clip_override(&self, key: LocalKey) -> Option<ClipBounds> {
        self.clip_overrides.get(&key).copied()
    }

    pub(crate) fn clip_bounds(&self, key: LocalKey, row: Row) -> ClipBounds {
        self.clip_override(key)
            .unwrap_or_else(|| ClipBounds::from(self.records[row.0].geometry.clip))
    }

    fn publish_clip(&mut self, key: LocalKey, cover: Rect, bounds: ClipBounds) {
        if ClipBounds::from(cover) == bounds {
            self.clip_overrides.remove(&key);
        } else {
            self.clip_overrides.insert(key, bounds);
            self.clip_capacity = self.clip_capacity.max(self.clip_overrides.capacity());
        }
    }

    pub(crate) fn geometry(&self, row: Row) -> Option<Geometry> {
        let record = &self.records[row.0];
        (record.participating
            && !record.parked
            && record.world_set
            && record
                .measurement
                .is_some_and(|key| key.viewport_epoch == self.viewport_epoch))
        .then_some(record.geometry)
    }

    pub(crate) fn cleanup_geometry(&self, row: Row) -> Option<Geometry> {
        self.records[row.0]
            .world_set
            .then_some(self.records[row.0].geometry)
    }

    pub(crate) fn eligible(&self, row: Row) -> bool {
        let record = &self.records[row.0];
        record.participating && record.visible && record.enabled && !record.parked
    }

    pub(crate) fn place(&mut self, row: Row, origin: Point) -> Result<bool, ElementError> {
        if !origin.x.is_finite() || !origin.y.is_finite() {
            return Err(ElementError::InvalidGeometry);
        }

        if self.records[row.0].geometry.origin == origin {
            return Ok(false);
        }

        self.records[row.0].geometry.origin = origin;
        Ok(true)
    }

    pub(crate) fn park(&mut self, row: Row) -> bool {
        if self.records[row.0].parked {
            return false;
        }

        self.records[row.0].parked = true;
        self.records[row.0].park_resume_required = true;
        true
    }
}

/// Projects one checked world record; descendant demand/output gates are separate from caches.
pub(crate) fn project_one(
    elements: &MountedElements,
    configuration: &crate::element::configuration::Configuration,
    layout: &mut Layout,
    work: &mut crate::schedule::Scheduler,
    key: LocalKey,
) -> Result<(), ElementError> {
    let Some(row) = elements.row_local(key) else {
        return Ok(());
    };

    work.begin_geometry(row);
    if !elements.contains_local(key) {
        return Ok(());
    }

    let parent = elements
        .parent(key)
        .and_then(|parent| elements.row_local(parent).map(|row| (parent, row)));
    let policy = configuration.read(row).visibility();
    let collapsed = policy == Visibility::Collapsed;

    let (
        parent_origin,
        parent_clip,
        parent_cover,
        parent_participating,
        parent_visible,
        parent_enabled,
    ) = match parent {
        Some((parent, parent_row)) => {
            let record = &layout.records[parent_row.0];
            let clip = layout.clip_bounds(parent, parent_row);
            let clip = if record.clip_children {
                clip.intersect(ClipBounds::from(record.geometry.bounds))
            } else {
                clip
            };

            (
                record.geometry.bounds.origin,
                clip,
                record.geometry.clip,
                record.participating && !record.published_collapsed,
                record.visible,
                record.enabled,
            )
        }
        None => (
            Point::ZERO,
            ClipBounds::from(layout.viewport),
            layout.viewport,
            true,
            true,
            true,
        ),
    };

    let record = &layout.records[row.0];
    let participating = parent_participating
        && record.demand
        && !record.parked
        && record
            .measurement
            .is_some_and(|key| key.viewport_epoch == layout.viewport_epoch);
    let visible = participating && parent_visible && policy == Visibility::Visible;
    let enabled = parent_enabled && configuration.read(row).get(crate::element::Enabled);

    let origin = if participating {
        parent_origin.add(record.geometry.origin)
    } else {
        Point::ZERO
    };
    let bounds = Rect::new(
        origin,
        if participating {
            record.geometry.size
        } else {
            Size::ZERO
        },
    );
    let clip_bounds = if visible {
        parent_clip
    } else {
        ClipBounds::from(Rect::new(origin, Size::ZERO))
    };
    let clip = clip_bounds.to_rect().unwrap_or(parent_cover);

    if !bounds.is_valid() || !clip.is_valid() || !clip_bounds.is_valid() {
        return Err(ElementError::InvalidGeometry);
    }

    let qualifier_changed = record.participating != participating
        || record.visible != visible
        || record.enabled != enabled;
    let clip_changed = record.geometry.clip != clip || layout.clip_bounds(key, row) != clip_bounds;
    let geometry_changed = !record.world_set || record.geometry.bounds != bounds || clip_changed;
    let descendants = !record.world_set
        || record.geometry.bounds.origin != origin
        || (record.clip_children && record.geometry.bounds.size != bounds.size)
        || clip_changed
        || qualifier_changed
        || record.published_clip_children != record.clip_children
        || record.published_collapsed != collapsed;
    let hit_changed = record.published_hit_testable != record.hit_testable;

    layout.publish_clip(key, clip, clip_bounds);
    let record = &mut layout.records[row.0];
    record.geometry.bounds = bounds;
    record.geometry.clip = clip;
    record.participating = participating;
    record.visible = visible;
    record.enabled = enabled;
    record.world_set |= participating;
    record.published_clip_children = record.clip_children;
    record.published_hit_testable = record.hit_testable;
    record.published_collapsed = collapsed;

    work.counts.geometry_visits = work.counts.geometry_visits.saturating_add(1);
    if geometry_changed || qualifier_changed || hit_changed {
        layout.mark_changed(key, row);
        work.hit_changed = true;
    }
    if qualifier_changed {
        layout.mark_qualification(key, row);
    }

    if visible {
        let deferred = work.pending(row) & (Work::SCENE | Work::SEMANTICS);
        work.request(key, row, deferred);
    }

    if descendants {
        for child in elements.child_keys(key) {
            let row = elements
                .row_local(child)
                .expect("projected topology retains children");
            work.geometry_dirty(child, row);
        }
    }

    Ok(())
}
