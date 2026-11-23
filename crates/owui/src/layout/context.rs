//! Callback access to this element's layout and its direct-child dependencies.

use crate::element::mounted::ElementAccess;
use crate::element::{ChildrenCx, ElementError, ElementId, Property, Visibility};
use crate::input::EventKey;
use crate::schedule::Work;
use crate::view::Action;

use super::dependency::GroupKey;
use super::measure::measure;
use super::{Constraints, MeasureMode, Point, Rect, Size, SizeContribution, SizePlan};

/// Layout and configuration access restricted to this element and its direct children.
///
/// Property getters read the executing element's canonical effective source. Mutable
/// operations and child callbacks require short borrows; copied IDs/contributions
/// can survive them, but references into the context cannot remain in use.
///
/// Measurement refreshes configuration and preparation prerequisites before using
/// cached size. Local results require finite extents, origins and world attachments;
/// positive infinity is allowed only for an unbounded constraint maximum. Errors
/// preserve unresolved work rather than publishing partially current geometry.
pub struct LayoutCx<'a> {
    pub(crate) access: ElementAccess<'a>,
    pub(crate) current: ElementId,
    pub(crate) recovery: bool,
}

impl LayoutCx<'_> {
    pub fn id(&self) -> ElementId {
        self.current
    }

    pub fn current_visibility(&self) -> Visibility {
        self.access.read(self.current).visibility()
    }

    pub fn viewport(&self) -> Rect {
        self.access.layout.viewport()
    }

    pub fn get<P: Property>(&self, property: P) -> P::Value {
        self.access.read(self.current).get(property)
    }

    /// Borrows a provided effective property without cloning or substituting a default.
    ///
    /// The reference is tied to this context borrow. Finish using it before mutation,
    /// measuring a child or refreshing child visibility; copy or clone needed data first.
    pub fn get_ref<P: Property>(&self, property: P) -> Option<&P::Value> {
        self.access.read(self.current).get_ref(property)
    }

    pub fn set<P: Property>(&mut self, property: P, value: P::Value) -> Result<(), ElementError> {
        self.access.set(self.current, property, value)
    }

    pub fn remove<P: Property>(&mut self, property: P) -> Result<(), ElementError> {
        self.access.remove(self.current, property)
    }

    pub fn on<K: EventKey>(
        &mut self,
        event: K,
        action: Action<K::Data>,
    ) -> Result<(), ElementError> {
        self.access.bind(self.current, event, action)
    }

    pub fn set_visibility(&mut self, visibility: Visibility) -> Result<(), ElementError> {
        self.access.set_visibility(self.current, visibility)
    }

    pub fn recovery_required(&self) -> bool {
        self.recovery
    }

    pub fn invalidate(&mut self, work: Work) {
        self.access.request(self.current, work);
    }

    pub fn invalidate_hit_test(&mut self) {
        self.access.invalidate_hit_test(self.current);
    }

    pub fn child_count(&self) -> usize {
        self.access.elements.child_count(self.current.local())
    }

    pub fn child(&self, index: usize) -> Option<ElementId> {
        self.access.elements.child_at(self.current.local(), index)
    }

    /// Refreshes direct-child configuration and records a visibility dependency.
    ///
    /// A successful parent layout retains this use even when a Collapsed branch skips
    /// measurement or the child is parked. Later visibility changes wake that live
    /// parent; this read does not measure size or clear the child's failure debt.
    /// The child must belong to this element's current direct set.
    pub fn visibility(&mut self, child: ElementId) -> Result<Visibility, ElementError> {
        self.access.elements.check_child(self.current, child)?;
        crate::element::mounted::ensure_current(&mut self.access, child)?;

        let row = self.access.elements.check_child(self.current, child)?;
        let visibility = self.access.configuration.read(row).visibility();
        self.access
            .layout
            .record_visibility(self.current.local(), child.local(), visibility);

        Ok(visibility)
    }

    /// Reconciles a complete direct-child set through one scoped edit.
    ///
    /// Omitted children retire on success. Failure keeps prior membership but does
    /// not undo nested successful child mutations. A committed structure/order change
    /// requeues parent layout and invalidates an active size plan and copied positions.
    pub fn children<R>(
        &mut self,
        process: impl FnOnce(&mut ChildrenCx<'_>) -> Result<R, ElementError>,
    ) -> Result<R, ElementError> {
        crate::element::children::edit(&mut self.access, self.current, process)
    }

    /// Measures a direct child and registers a conservative parent use.
    ///
    /// Configuration and preparation run before a cache hit is accepted. This use
    /// does not assert that the parent depends only on size: later child layout work
    /// can invalidate the parent even when measured size is unchanged. Invalid
    /// constraints, stale/foreign children or nested callback failures are reported.
    /// Use [`measure_size`](Self::measure_size) for a size/participation-only dependency.
    pub fn measure(
        &mut self,
        child: ElementId,
        constraints: Constraints,
    ) -> Result<Size, ElementError> {
        self.access.elements.check_child(self.current, child)?;

        measure(
            &mut self.access,
            child,
            constraints,
            MeasureMode::Conservative,
        )
    }

    /// Starts an optional single-key size-only group for every current direct child.
    ///
    /// Request the plan once, before any child use, to permit Incremental reuse. An
    /// Incremental plan retains unchanged committed uses without sibling rediscovery;
    /// process its changed entries using current contributions or [`measure_size`](Self::measure_size).
    /// Rebuild requires measuring all current children to establish a new size-only group.
    /// Both plans remain subject to [`size_inputs_current`](Self::size_inputs_current).
    ///
    /// Recovery, constraint/viewport/environment or membership changes, prior child
    /// uses in this attempt, and uncertain or mixed keys give Rebuild. Earlier staged
    /// uses are preserved, including a same-key measurement; conflicting or repeated
    /// requests do not erase conservative obligations. Legal mixed/conservative/eager
    /// operations can complete with ordinary per-child dependencies without validating
    /// a group. Group uncertainty by itself is not an operational failure.
    ///
    /// The plan and copied positions belong only to this parent attempt. Child-set
    /// edits or new callback work can revoke them; neither a copied plan nor a State
    /// cache key authorizes ignoring failure or freshness debt. Invalid constraints
    /// and failures while obtaining the current environment are returned as errors.
    pub fn size_plan(&mut self, constraints: Constraints) -> Result<SizePlan, ElementError> {
        if !constraints.is_valid() {
            return Err(ElementError::InvalidGeometry);
        }

        crate::element::mounted::ensure_environment(&mut self.access, self.current)?;
        let row = self
            .access
            .elements
            .row(self.current)
            .ok_or(ElementError::Missing(self.current))?;

        let key = GroupKey {
            constraints,
            viewport_epoch: self.access.layout.viewport_epoch(),
            output_epoch: self.access.configuration.output_epoch(row),
        };

        Ok(self.access.layout.plan_group(
            self.current.local(),
            key,
            self.access.elements,
            self.recovery,
        ))
    }

    /// Tests the active group's receipt after child callbacks without sibling rediscovery.
    ///
    /// This checks the executing plan, membership/environment and entered-use coverage;
    /// taint, new dirty tails or missing staged changes make it false. It does not
    /// mutate State or discharge work. A container may return its computed Size while
    /// false, but must not mark a derived cache current on that evidence.
    ///
    /// True is limited to this check in this attempt: it neither proves a published
    /// window epoch nor replaces the caller's full-child obligations on Rebuild. Later
    /// callbacks/edits can revoke it. Legal mixed or parked layouts can complete through
    /// ordinary dependencies without validating a group; false alone is not an error.
    pub fn size_inputs_current(&self) -> bool {
        let Some(attempt) = self
            .access
            .layout
            .attempts
            .last()
            .filter(|attempt| attempt.key == self.current.local())
        else {
            return false;
        };
        let Some(key) = attempt.group_key else {
            return false;
        };
        let Some(group) = self.access.layout.groups.get(&self.current.local()) else {
            return false;
        };
        let Some(row) = self.access.elements.row(self.current) else {
            return false;
        };

        !attempt.tainted
            && !group.structure_dirty
            && group.dirty.keys.is_empty()
            && group.key == key
            && group.count == self.child_count()
            && key.viewport_epoch == self.access.layout.viewport_epoch()
            && key.output_epoch == self.access.configuration.output_epoch(row)
            && self.access.configuration.checked(row)
            && !self.access.work.pending(row).contains(Work::LAYOUT)
            && attempt.retained.as_ref().is_none_or(|retained| {
                !retained.bulk
                    || retained
                        .entered
                        .keys
                        .iter()
                        .all(|child| attempt.uses.values.contains_key(child))
            })
    }

    pub(crate) fn visit_size_blocks(&mut self, blocks: usize) {
        self.access.work.counts.layout_block_visits = self
            .access
            .work
            .counts
            .layout_block_visits
            .saturating_add(u64::try_from(blocks).unwrap_or(u64::MAX));
    }

    /// Copies a current staged or retained single-key contribution for a direct child.
    ///
    /// `None` requires [`measure_size`](Self::measure_size): failed, dirty, parked,
    /// undelivered or unproven inputs cannot be substituted with the child's latest
    /// eager cache. The copy holds no reference into growable storage and does not
    /// create an independent cache identity or register a new measurement key.
    ///
    /// Invalid constraints and stale, foreign or wrong-parent child handles return
    /// errors. Cache use remains subject to [`size_inputs_current`](Self::size_inputs_current)
    /// after callbacks; a previously copied contribution is not a freshness proof.
    pub fn size_contribution(
        &mut self,
        child: ElementId,
        constraints: Constraints,
    ) -> Result<Option<SizeContribution>, ElementError> {
        if !constraints.is_valid() {
            return Err(ElementError::InvalidGeometry);
        }
        self.access.elements.check_child(self.current, child)?;

        self.access.work.counts.layout_dependency_checks = self
            .access
            .work
            .counts
            .layout_dependency_checks
            .saturating_add(1);

        Ok(self.access.layout.contribution(
            self.current.local(),
            child.local(),
            constraints,
            self.access.configuration,
            self.access.elements,
            self.access.work,
        ))
    }

    /// Measures a direct child whose parent dependency is limited to size and participation.
    ///
    /// After successful parent layout, the committed contribution records this key's
    /// size and occupancy. Same-size/current child work can stop without rerunning the
    /// parent; failure and new callback debt remain independent of equality. Hidden
    /// children occupy space, while Collapsed contributions do not.
    ///
    /// Eager child-scope queries do not overwrite that committed parent contribution.
    /// Conservative use or multiple keys remain legal but disable size-only reuse for
    /// the affected dependency/group. Prerequisites and errors are the same as
    /// [`measure`](Self::measure); this does not promise unbounded recursive measurement.
    pub fn measure_size(
        &mut self,
        child: ElementId,
        constraints: Constraints,
    ) -> Result<Size, ElementError> {
        self.access.elements.check_child(self.current, child)?;

        measure(&mut self.access, child, constraints, MeasureMode::SizeOnly)
    }

    /// Sets a direct child's parent-local origin without measuring it.
    ///
    /// Origins must be finite. Placement does not replace a size/visibility use or
    /// authorize a stale measured result; world attachment is projected separately.
    pub fn place(&mut self, child: ElementId, origin: Point) -> Result<(), ElementError> {
        let row = self.access.elements.check_child(self.current, child)?;
        if self.access.layout.place(row, origin)? {
            self.access.work.geometry_dirty(child.local(), row);
        }

        Ok(())
    }

    /// Suspends a direct child's layout/output participation without retiring its State.
    ///
    /// Parking is a retained virtualization choice, not removal or failure repair.
    /// It revokes size-group trust and permits ordinary per-child fallback; it must
    /// not be used to acknowledge an unresolved failure or hide new work. A later
    /// measurement resumes layout demand. Recorded visibility dependencies can still
    /// wake a live parent for a parked or unmeasured child.
    pub fn park(&mut self, child: ElementId) -> Result<(), ElementError> {
        let row = self.access.elements.check_child(self.current, child)?;
        self.access
            .layout
            .park_use(self.current.local(), child.local());

        if self.access.layout.park(row) {
            self.access.work.geometry_dirty(child.local(), row);
        }

        Ok(())
    }

    /// Sets the finite local content extent independently of the allocated layout size.
    ///
    /// This can exceed the returned layout size for scrolling or overflow. Negative,
    /// NaN or infinite extents return [`ElementError::InvalidGeometry`]. If omitted,
    /// successful layout uses its returned size as content extent.
    pub fn content_size(&mut self, size: Size) -> Result<(), ElementError> {
        if !size.is_valid() {
            return Err(ElementError::InvalidGeometry);
        }

        let row = self
            .access
            .elements
            .row(self.current)
            .expect("layout context retains its own row");

        self.access.layout.records[row.0].geometry.content_size = size;
        self.access.layout.records[row.0].content_set = true;

        Ok(())
    }

    pub fn clip_children(&mut self, clip: bool) -> Result<(), ElementError> {
        let row = self
            .access
            .elements
            .row(self.current)
            .ok_or(ElementError::Missing(self.current))?;

        if self.access.layout.records[row.0].clip_children != clip {
            self.access.layout.records[row.0].clip_children = clip;
            self.access.work.geometry_dirty(self.current.local(), row);
        }

        Ok(())
    }

    pub fn hit_testable(&mut self, enabled: bool) -> Result<(), ElementError> {
        let row = self
            .access
            .elements
            .row(self.current)
            .ok_or(ElementError::Missing(self.current))?;

        if self.access.layout.records[row.0].hit_testable != enabled {
            self.access.layout.records[row.0].hit_testable = enabled;
            self.access.work.geometry_dirty(self.current.local(), row);
        }

        self.access.work.hit_dirty();
        Ok(())
    }
}
