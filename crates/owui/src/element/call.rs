//! Scoped body ownership and callback attempts; snapshots remain in their data owners.

use std::num::NonZeroUsize;

use crate::layout::{Constraints, Size};
use crate::schedule::{AttemptKind, Work};

use super::configuration::Environment;
use super::mounted::{ElementAccess, ElementCall, Life, Row};
use super::property::PropertyChanges;
use super::{Attrs, ElementError, ElementId};

fn handle(index: usize) -> NonZeroUsize {
    NonZeroUsize::new(index + 1).expect("allocated attempt indices have a nonzero encoding")
}

/// The body leaves growable storage while callbacks can mount or retire descendants.
pub(crate) struct CallFrame<'call, 'owners> {
    access: &'call mut ElementAccess<'owners>,
    id: ElementId,
    body: Option<Box<dyn ElementCall>>,
    kind: Option<AttemptKind>,
    source: Option<NonZeroUsize>,
    retired: Option<NonZeroUsize>,
    delivery: Option<NonZeroUsize>,
    recovery: bool,
    started: bool,
    complete: bool,
}

impl<'call, 'owners> CallFrame<'call, 'owners> {
    pub(crate) fn enter(
        access: &'call mut ElementAccess<'owners>,
        id: ElementId,
        kind: Option<AttemptKind>,
    ) -> Result<Self, ElementError> {
        let row = access.elements.row(id).ok_or(ElementError::Missing(id))?;
        if access.elements.nodes[row.0].life != Life::Live {
            return Err(ElementError::Missing(id));
        }

        let body = access.elements.nodes[row.0]
            .body
            .take()
            .ok_or(ElementError::Reentrant(id))?;
        access.elements.frames += 1;
        let mut frame = Self {
            access,
            id,
            body: Some(body),
            kind,
            source: None,
            retired: None,
            delivery: None,
            recovery: false,
            started: false,
            complete: false,
        };

        if let Some(kind) = kind {
            frame.recovery = frame.access.work.begin_attempt(row, kind);
            frame.started = true;
        }

        Ok(frame)
    }

    fn row(&self) -> Row {
        self.access
            .elements
            .row(self.id)
            .expect("an owned call body retains its identity row")
    }

    pub(crate) fn install(
        &mut self,
        attrs: Attrs,
        parent: Environment,
    ) -> Result<PropertyChanges, ElementError> {
        let row = self.row();
        let index = self
            .access
            .configuration
            .protect_replace(row, self.id.local(), attrs)?;
        self.source = Some(handle(index));

        let previous_output = self.access.configuration.protected_output(index);
        let mut changes = self
            .access
            .configuration
            .protected_changes(row, index, &parent);
        let delivered = self.refresh_environment(parent, Some(previous_output))?;
        changes.merge_suffix(&delivered, 0);

        Ok(changes)
    }

    pub(crate) fn refresh_environment(
        &mut self,
        parent: Environment,
        previous_output: Option<Environment>,
    ) -> Result<PropertyChanges, ElementError> {
        let row = self.row();
        let refreshed =
            self.access
                .configuration
                .refresh(row, self.id.local(), parent, previous_output)?;
        if refreshed.output_changed {
            self.access
                .configuration
                .queue_descendants(row, self.id.local());
        }

        self.retired = self
            .access
            .configuration
            .defer_environments(refreshed.retired_input, refreshed.retired_output)
            .map(handle);

        let (changes, prefix) = self.access.configuration.begin_delivery(self.id.local());
        self.delivery = prefix;

        Ok(changes)
    }

    pub(crate) fn parts(&mut self) -> (&mut dyn ElementCall, ElementAccess<'_>, bool) {
        let body = self
            .body
            .as_mut()
            .expect("call frames own their detached body");
        (body.as_mut(), self.access.reborrow(), self.recovery)
    }

    pub(crate) fn access(&mut self) -> &mut ElementAccess<'owners> {
        self.access
    }

    fn restore_body(&mut self) {
        if let Some(body) = self.body.take() {
            let row = self.row();
            self.access.elements.nodes[row.0].body = Some(body);
            self.access.elements.frames -= 1;
        }
    }

    /// Commits all callback bookkeeping before releasing user-bearing old references.
    pub(crate) fn finish(&mut self) {
        let row = self.row();
        if let Some(prefix) = self.delivery.take() {
            self.access
                .configuration
                .finish_delivery(self.id.local(), prefix, true);
        }
        if let Some(kind) = self.kind.filter(|_| self.started) {
            self.access.work.finish_attempt(row, kind);
            self.started = false;
        }

        let source = self
            .source
            .take()
            .map(|index| self.access.configuration.commit_protected(index.get() - 1));
        let retired = self.retired.take().map(|index| {
            self.access
                .configuration
                .release_environments(index.get() - 1)
        });

        self.complete = true;
        self.restore_body();

        drop(retired);
        drop(source);
    }
}

impl Drop for CallFrame<'_, '_> {
    fn drop(&mut self) {
        if self.complete {
            if std::thread::panicking() {
                self.access.work.fault.get_or_insert(self.id);
            }
            self.restore_body();
            return;
        }

        let row = self.row();
        if let Some(prefix) = self.delivery.take() {
            self.access
                .configuration
                .finish_delivery(self.id.local(), prefix, false);
        }

        let source = self.source.take().map(|index| {
            self.access
                .configuration
                .rollback_protected(row, self.id.local(), index.get() - 1)
        });
        let retired = self.retired.take().map(|index| {
            self.access
                .configuration
                .release_environments(index.get() - 1)
        });

        if let Some(kind) = self.kind.filter(|_| self.started) {
            self.access.work.fail_attempt(self.id.local(), row, kind);
            self.access
                .layout
                .note_configuration(self.access.elements, self.id.local());
            self.access.layout.invalidate(row);
            self.access.work.geometry_dirty(self.id.local(), row);
            self.started = false;
        }

        self.restore_body();

        drop(retired);
        drop(source);
    }
}

/// Detaches only the layout body; measurement owns its attempt and rollback metadata.
/// This smaller lease does not carry unused configuration-delivery fields on every recursive call.
pub(crate) struct LayoutBody<'call, 'owners> {
    access: &'call mut ElementAccess<'owners>,
    id: ElementId,
    body: Option<Box<dyn ElementCall>>,
}

impl<'call, 'owners> LayoutBody<'call, 'owners> {
    pub(crate) fn enter(
        access: &'call mut ElementAccess<'owners>,
        id: ElementId,
    ) -> Result<Self, ElementError> {
        let row = access.elements.row(id).ok_or(ElementError::Missing(id))?;
        if access.elements.nodes[row.0].life != Life::Live {
            return Err(ElementError::Missing(id));
        }

        let body = access.elements.nodes[row.0]
            .body
            .take()
            .ok_or(ElementError::Reentrant(id))?;
        access.elements.frames += 1;

        Ok(Self {
            access,
            id,
            body: Some(body),
        })
    }

    pub(crate) fn parts(&mut self) -> (&mut dyn ElementCall, ElementAccess<'_>) {
        (
            self.body
                .as_mut()
                .expect("layout leases own their detached body")
                .as_mut(),
            self.access.reborrow(),
        )
    }
}

impl Drop for LayoutBody<'_, '_> {
    fn drop(&mut self) {
        if let Some(body) = self.body.take() {
            let row = self
                .access
                .elements
                .row(self.id)
                .expect("an active body lease prevents physical identity reclamation");
            self.access.elements.nodes[row.0].body = Some(body);
            self.access.elements.frames -= 1;
        }
    }
}

/// Covers measurement prerequisites too, before prepare can mutate retained State.
pub(crate) struct MeasureAttempt<'call, 'owners> {
    access: &'call mut ElementAccess<'owners>,
    id: ElementId,
    snapshot: Option<NonZeroUsize>,
    probe: Option<NonZeroUsize>,
    recovery: bool,
    forced: bool,
    started: bool,
    complete: bool,
}

impl<'call, 'owners> MeasureAttempt<'call, 'owners> {
    pub(crate) fn enter(
        access: &'call mut ElementAccess<'owners>,
        id: ElementId,
    ) -> Result<Self, ElementError> {
        let row = access.elements.row(id).ok_or(ElementError::Missing(id))?;
        if !access.elements.contains(id) {
            return Err(ElementError::Missing(id));
        }
        if access.elements.nodes[row.0].body.is_none() {
            return Err(ElementError::Reentrant(id));
        }

        let resuming_park = access.layout.records[row.0].parked
            && access.layout.records[row.0].park_resume_required;
        let forced = access.work.pending(row).contains(Work::LAYOUT)
            || access.work.failure(row).contains(Work::LAYOUT)
            || resuming_park;
        let mut attempt = Self {
            access,
            id,
            snapshot: None,
            probe: None,
            recovery: false,
            forced,
            started: false,
            complete: false,
        };

        attempt.recovery = attempt.access.work.begin_attempt(row, AttemptKind::Layout);
        attempt.started = true;
        attempt.probe = Some(handle(
            attempt
                .access
                .layout
                .begin_probe(id.local(), attempt.access.elements),
        ));
        attempt.access.layout.records[row.0].demand = true;
        attempt.access.layout.records[row.0].parked = false;

        Ok(attempt)
    }

    fn row(&self) -> Row {
        self.access
            .elements
            .row(self.id)
            .expect("measurement retains its allocated identity")
    }

    pub(crate) fn access(&mut self) -> &mut ElementAccess<'owners> {
        self.access
    }

    pub(crate) fn recovery(&self) -> bool {
        self.recovery
    }

    pub(crate) fn forced(&self) -> bool {
        self.forced
    }

    pub(crate) fn begin_layout(&mut self) {
        let row = self.row();
        self.access.work.consume(row, Work::LAYOUT);
        self.snapshot = Some(handle(
            self.access.layout.begin_measure(self.id.local(), row),
        ));
    }

    pub(crate) fn finish_local(&mut self, constraints: Constraints, size: Size) {
        let row = self.row();
        let index = self
            .snapshot
            .take()
            .expect("executed layout owns a local snapshot");
        let current = !self.access.work.pending(row).contains(Work::LAYOUT);
        let (current, inspected) = self.access.layout.finish_measure(
            index.get() - 1,
            constraints,
            size,
            current,
            self.access.elements,
            self.access.configuration,
        );

        self.access.work.counts.layout_dependency_checks = self
            .access
            .work
            .counts
            .layout_dependency_checks
            .saturating_add(inspected);

        if !current {
            self.access.request(self.id, Work::LAYOUT);
        }
    }

    pub(crate) fn finish(&mut self, constraints: Constraints, size: Size, suspended: bool) {
        let row = self.row();
        if suspended {
            self.access.work.suspend_attempt();
        } else {
            self.access.work.finish_attempt(row, AttemptKind::Layout);
            if self.access.layout.records[row.0].park_resume_required {
                self.access.layout.records[row.0].park_resume_required = false;
            }
        }
        self.started = false;

        if let Some(probe) = self.probe.take() {
            let current = self.access.layout.records[row.0]
                .measurement
                .is_some_and(|key| {
                    key.constraints == constraints
                        && key.viewport_epoch == self.access.layout.viewport_epoch()
                })
                && self.access.configuration.checked(row)
                && (suspended
                    || (!self.access.configuration.needs_delivery(self.id.local())
                        && !self
                            .access
                            .work
                            .pending(row)
                            .intersects(Work::PREPARE | Work::LAYOUT)
                        && !self.access.work.recovery_required(row)));
            let visibility = self
                .access
                .layout
                .dependencies
                .get(&self.id.local())
                .and_then(|use_| use_.visibility);
            self.access.layout.finish_probe(
                probe.get() - 1,
                crate::layout::Contribution {
                    constraints,
                    size,
                    occupied: !suspended,
                    valid: true,
                    measured: true,
                    visibility,
                },
                current,
            );
        }

        self.complete = true;
    }
}

impl Drop for MeasureAttempt<'_, '_> {
    fn drop(&mut self) {
        if self.complete {
            return;
        }

        let row = self.row();
        if let Some(index) = self.snapshot.take() {
            self.access.layout.fail_measure(index.get() - 1, row);
        } else {
            self.access.layout.invalidate(row);
        }
        if let Some(probe) = self.probe.take() {
            self.access.layout.fail_probe(probe.get() - 1);
        }

        if self.started {
            self.access
                .work
                .fail_attempt(self.id.local(), row, AttemptKind::Layout);
            self.access
                .layout
                .note_configuration(self.access.elements, self.id.local());
            self.access.work.geometry_dirty(self.id.local(), row);
        }
    }
}

/// Lifecycle destruction after an irrevocable commit cannot be treated as a retryable callback.
pub(crate) fn drop_committed<T>(access: &mut ElementAccess<'_>, id: ElementId, value: T) {
    struct Fault<'a> {
        work: &'a mut crate::schedule::Scheduler,
        id: ElementId,
    }

    impl Drop for Fault<'_> {
        fn drop(&mut self) {
            if std::thread::panicking() {
                self.work.fault.get_or_insert(self.id);
            }
        }
    }

    let _fault = Fault {
        work: access.work,
        id,
    };

    drop(value);
}
