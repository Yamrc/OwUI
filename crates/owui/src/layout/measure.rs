//! Guarded synchronous measurement after configuration and preparation prerequisites.

use crate::element::mounted::ElementAccess;
use crate::element::{ElementError, ElementId, Visibility};
use crate::schedule::Work;

use super::{Constraints, LayoutCx, MeasureKey, MeasureMode, Size};

/// Measures after resolving configuration and preparation, with one owned prerequisite attempt.
pub(crate) fn measure(
    access: &mut ElementAccess<'_>,
    id: ElementId,
    constraints: Constraints,
    mode: MeasureMode,
) -> Result<Size, ElementError> {
    if !constraints.is_valid() {
        return Err(ElementError::InvalidGeometry);
    }

    let row = access.elements.row(id).ok_or(ElementError::Missing(id))?;
    let old_size = access.layout.records[row.0].geometry.size;
    let unpark = access.layout.records[row.0].parked;
    let resume_required = unpark && access.layout.records[row.0].park_resume_required;
    let parent = access.elements.parent(id.local());

    let mut attempt = crate::element::call::MeasureAttempt::enter(access, id)?;
    crate::element::mounted::ensure_current(attempt.access(), id)?;
    let collapsed = attempt.access().configuration.read(row).visibility() == Visibility::Collapsed;
    if !collapsed {
        crate::element::mounted::prepare(attempt.access(), id)?;
    }

    let row = attempt
        .access()
        .elements
        .row(id)
        .expect("prepared measurement retains an identity");
    let occupied = attempt.access().configuration.read(row).visibility() != Visibility::Collapsed;

    let key = MeasureKey {
        constraints,
        viewport_epoch: attempt.access().layout.viewport_epoch(),
    };
    let cached = !attempt.forced()
        && !attempt.access().work.pending(row).contains(Work::LAYOUT)
        && attempt.access().layout.records[row.0].measurement == Some(key);
    if cached {
        let size = attempt.access().layout.records[row.0].geometry.size;
        if unpark {
            attempt.access().work.geometry_dirty(id.local(), row);
        }
        if let Some(parent) = parent {
            attempt.access().layout.record_use(
                parent,
                id.local(),
                constraints,
                mode,
                size,
                occupied,
            );
        }

        attempt.access().work.counts.layout_hits =
            attempt.access().work.counts.layout_hits.saturating_add(1);
        attempt.finish(constraints, size, !occupied);
        return Ok(size);
    }

    attempt.begin_layout();
    let recovery = attempt.recovery();
    let size = if !occupied {
        Size::ZERO
    } else {
        let mut body_lease = crate::element::call::LayoutBody::enter(attempt.access(), id)?;

        let size = {
            let (body, access) = body_lease.parts();
            let mut cx = LayoutCx {
                access,
                current: id,
                recovery,
            };
            body.layout(&mut cx, constraints)?
        };

        if !size.is_valid()
            || size.width < constraints.min.width
            || size.width > constraints.max.width
            || size.height < constraints.min.height
            || size.height > constraints.max.height
        {
            return Err(ElementError::InvalidGeometry);
        }

        drop(body_lease);
        size
    };

    attempt.finish_local(constraints, size);
    if unpark && resume_required && occupied {
        attempt.access().layout.records[row.0].park_resume_required = false;
    }
    {
        let access = attempt.access();
        access.work.counts.layouts = access.work.counts.layouts.saturating_add(1);
        access.work.geometry_dirty(id.local(), row);
        access.work.hit_dirty();
        access
            .work
            .request(id.local(), row, Work::SCENE | Work::SEMANTICS);

        if let Some(parent) = parent {
            let parent_row = access
                .elements
                .row_local(parent)
                .expect("measured children retain their parent");
            if !access.layout.records[parent_row.0].laying_out && mode != MeasureMode::SizeOnly {
                access.work.notify_layout(id.local(), row);
            }
            access
                .layout
                .record_use(parent, id.local(), constraints, mode, size, occupied);
        }

        if old_size != size {
            access.layout.mark_changed(id.local(), row);
        }
    }

    attempt.finish(constraints, size, !occupied);
    Ok(size)
}
