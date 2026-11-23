//! Admission routing and FIFO effect application over prepared qualification.

use crate::element::configuration::Configuration;
use crate::element::mounted::MountedElements;
use crate::element::{ElementError, ElementId};
use crate::id::LocalKey;
use crate::layout::{Layout, Point};
use crate::schedule::{Scheduler, Work};
use crate::view::PendingAction;

use super::hit;
use super::{
    Cleanup, CleanupPose, EventCx, EventPhase, InputEvent, InputRequest, InputState, KeyCode,
    PointerId, PointerKind, PointerState, Propagation, prepared_geometry, qualified, retain_buffer,
};

pub(crate) fn submit(
    elements: &mut MountedElements,
    configuration: &Configuration,
    layout: &mut Layout,
    work: &mut Scheduler,
    input: &mut InputState,
    event: InputEvent,
) -> Result<Option<ElementId>, ElementError> {
    if event
        .position()
        .is_some_and(|point| !point.x.is_finite() || !point.y.is_finite())
    {
        return Ok(None);
    }
    if input.closing || (!input.window_focused && event.position().is_some()) {
        return Ok(None);
    }
    if matches!(
        event,
        InputEvent::Key {
            code: KeyCode::Tab,
            pressed: true
        }
    ) && input.window_focused
        && !input.batch.suppress_acquisition
    {
        input.batch.default_tab = Some(input.semantics.snapshot_focus(input.focus));
    }

    let (target, propagation) = match &event {
        InputEvent::Focus(focused) => {
            input.window_focused = *focused;
            if !focused {
                input.batch.suppress_acquisition = true;
                input.batch.default_tab = None;
                input.deferred_focus = None;
                cancel_all(elements, configuration, layout, work, input)?;
            }
            (None, Propagation::Continue)
        }
        InputEvent::Pointer { id, kind, position } => {
            if matches!(kind, PointerKind::Up | PointerKind::Cancel) {
                input.batch.terminal_pointers.insert(*id);
            }
            let inside = matches!(kind, PointerKind::Move | PointerKind::Enter);
            let pointer = input
                .pointers
                .entry(*id)
                .or_insert_with(|| PointerState::new(*position, inside));
            pointer.position = *position;
            if inside {
                pointer.inside = true;
            } else if matches!(kind, PointerKind::Leave | PointerKind::Cancel) {
                pointer.inside = false;
            }
            input.pointer_capacity = input.pointer_capacity.max(input.pointers.capacity());
            match kind {
                PointerKind::Leave => {
                    let previous = input.pointers.get(id).and_then(|pointer| pointer.hover);
                    set_hover(
                        elements,
                        configuration,
                        layout,
                        work,
                        input,
                        *id,
                        None,
                        *position,
                    )?;
                    (previous, Propagation::Continue)
                }
                PointerKind::Enter => {
                    let target =
                        hit_target(elements, configuration, layout, work, input, *position);
                    set_hover(
                        elements,
                        configuration,
                        layout,
                        work,
                        input,
                        *id,
                        target,
                        *position,
                    )?;
                    (target, Propagation::Continue)
                }
                PointerKind::Cancel => {
                    let target = cancel_pointer(elements, configuration, layout, work, input, *id)?;
                    (target, Propagation::Continue)
                }
                PointerKind::Move | PointerKind::Down | PointerKind::Up => {
                    let captured = input
                        .pointers
                        .get(id)
                        .and_then(|pointer| pointer.capture)
                        .filter(|key| qualified(elements, layout, *key));
                    let geometric = if *kind == PointerKind::Move || captured.is_none() {
                        hit_target(elements, configuration, layout, work, input, *position)
                    } else {
                        None
                    };
                    if *kind == PointerKind::Move {
                        set_hover(
                            elements,
                            configuration,
                            layout,
                            work,
                            input,
                            *id,
                            geometric,
                            *position,
                        )?;
                    }
                    let target = captured.or(geometric);
                    let result = route_optional(
                        elements,
                        configuration,
                        layout,
                        work,
                        input,
                        target,
                        &event,
                    );
                    if *kind == PointerKind::Up {
                        release_capture(elements, input, *id, None);
                    }
                    (target, result?)
                }
            }
        }
        InputEvent::Scroll { position, .. } => {
            let target = hit_target(elements, configuration, layout, work, input, *position);
            let propagation =
                route_optional(elements, configuration, layout, work, input, target, &event)?;
            (target, propagation)
        }
        InputEvent::Key { .. }
        | InputEvent::Text(_)
        | InputEvent::Composition { .. }
        | InputEvent::Activate => {
            let target = input
                .focus
                .filter(|key| input.window_focused && qualified(elements, layout, *key));
            let propagation =
                route_optional(elements, configuration, layout, work, input, target, &event)?;
            (target, propagation)
        }
    };

    if propagation == Propagation::Stop {
        input.batch.default_tab = None;
    }

    Ok(target.map(|key| elements.id(key)))
}

pub(crate) fn submit_activation(
    elements: &mut MountedElements,
    configuration: &Configuration,
    layout: &mut Layout,
    work: &mut Scheduler,
    input: &mut InputState,
    target: ElementId,
) -> Result<(), ElementError> {
    if input.closing || !elements.contains(target) || !qualified(elements, layout, target.local()) {
        return Err(ElementError::Missing(target));
    }
    route(
        elements,
        configuration,
        layout,
        work,
        input,
        target.local(),
        &InputEvent::Activate,
    )?;
    Ok(())
}

fn hit_target(
    elements: &MountedElements,
    configuration: &Configuration,
    layout: &Layout,
    work: &mut Scheduler,
    input: &InputState,
    position: Point,
) -> Option<LocalKey> {
    input
        .hit
        .target(elements, configuration, layout, &mut work.counts, position)
}

fn route_optional(
    elements: &mut MountedElements,
    configuration: &Configuration,
    layout: &mut Layout,
    work: &mut Scheduler,
    input: &mut InputState,
    target: Option<LocalKey>,
    event: &InputEvent,
) -> Result<Propagation, ElementError> {
    match target {
        Some(target) => route(elements, configuration, layout, work, input, target, event),
        None => Ok(Propagation::Continue),
    }
}

fn route(
    elements: &mut MountedElements,
    configuration: &Configuration,
    layout: &mut Layout,
    work: &mut Scheduler,
    input: &mut InputState,
    target: LocalKey,
    event: &InputEvent,
) -> Result<Propagation, ElementError> {
    let mut path = std::mem::take(&mut input.path);
    let mut current = elements.parent(target);
    while let Some(key) = current {
        path.push(key);
        current = elements.parent(key);
    }

    let result = (|| {
        for &key in path.iter().rev() {
            if dispatch(
                elements,
                configuration,
                layout,
                work,
                input,
                key,
                event,
                EventPhase::Capture,
                false,
            )? == Propagation::Stop
            {
                return Ok(Propagation::Stop);
            }
        }

        if dispatch(
            elements,
            configuration,
            layout,
            work,
            input,
            target,
            event,
            EventPhase::Target,
            false,
        )? == Propagation::Stop
        {
            return Ok(Propagation::Stop);
        }

        for &key in &path {
            if dispatch(
                elements,
                configuration,
                layout,
                work,
                input,
                key,
                event,
                EventPhase::Bubble,
                false,
            )? == Propagation::Stop
            {
                return Ok(Propagation::Stop);
            }
        }
        Ok(Propagation::Continue)
    })();

    retain_buffer(&mut input.path, path);
    result
}

#[expect(
    clippy::too_many_arguments,
    reason = "Disjoint owners and stage metadata keep event configuration read-only"
)]
fn dispatch(
    elements: &mut MountedElements,
    configuration: &Configuration,
    layout: &mut Layout,
    work: &mut Scheduler,
    input: &mut InputState,
    key: LocalKey,
    event: &InputEvent,
    phase: EventPhase,
    cleanup: bool,
) -> Result<Propagation, ElementError> {
    let id = elements.id(key);
    let Some(row) = elements.row_local(key) else {
        return if cleanup {
            Ok(Propagation::Continue)
        } else {
            Err(ElementError::Missing(id))
        };
    };
    if !cleanup && !elements.contains_local(key) {
        return Err(ElementError::Missing(id));
    }

    let current_pose = || prepared_geometry(elements, layout, key).map(CleanupPose::from_geometry);
    let saved_pose = if cleanup {
        input.saved_cleanup_pose(key)
    } else {
        None
    };
    let pose = saved_pose.or_else(current_pose);
    let Some(pose) = pose else {
        return if cleanup {
            Ok(Propagation::Continue)
        } else {
            Err(ElementError::Missing(id))
        };
    };
    let clip_bounds = saved_pose.map_or_else(
        || layout.clip_bounds(key, row),
        |pose| input.saved_cleanup_clip_bounds(key, pose),
    );
    let hit = event.position().map(|point| {
        hit::effective_hit(
            elements,
            configuration,
            layout,
            key,
            row,
            pose,
            clip_bounds,
            point,
            &mut work.counts,
        )
    });
    let mut cx = EventCx {
        current: id,
        bounds: pose.bounds,
        clip: pose.clip,
        clip_bounds,
        hit,
        phase,
        configuration: configuration.read(row),
        work: Work::empty(),
        hit_dirty: false,
        requests: std::mem::take(&mut input.event_requests),
        emissions: std::mem::take(&mut input.event_emissions),
    };

    let propagation = {
        let body = elements.body_mut(key).ok_or(ElementError::Reentrant(id))?;
        body.event(&mut cx, event)
    };

    // Publish effects and dirty work before dropping any unconsumed user payload.
    input.requests.append(&mut cx.requests);
    layout.note_work(elements, key, cx.work);
    if cx.work.contains(Work::LAYOUT) {
        layout.invalidate(row);
    }
    work.request(key, row, cx.work);
    if cx.hit_dirty {
        work.hit_dirty();
    }
    work.counts.events = work.counts.events.saturating_add(1);

    for emission in cx.emissions.drain(..) {
        let binding = if !input.closing && elements.contains_local(key) {
            cx.configuration.binding(emission.event).cloned()
        } else {
            None
        };
        if let Some(binding) = binding {
            input.actions.push_back(PendingAction {
                source: id,
                binding,
                data: emission.data,
            });
        }
    }
    retain_buffer(&mut input.event_requests, cx.requests);
    retain_buffer(&mut input.event_emissions, cx.emissions);
    Ok(propagation)
}

/// Applies one FIFO request or default-Tab selection at a cooperative boundary.
///
/// The caller repeats this before Actions, with a budget check after each unit,
/// and does not call it between mutable callbacks and spatial publication.
/// Already qualified targets remain runnable when their event merely invalidated
/// measurement. Returning callbacks leave their follow-up requests owned for the
/// next unit; no unbounded feedback drain is hidden in this call.
pub(crate) fn apply_requests(
    elements: &mut MountedElements,
    configuration: &Configuration,
    layout: &mut Layout,
    work: &mut Scheduler,
    input: &mut InputState,
) -> Result<usize, ElementError> {
    input.request_queue.extend(input.requests.drain(..));
    if let Some(request) = input.request_queue.pop_front() {
        let result = match request {
            InputRequest::Focus(target) => {
                request_focus(elements, configuration, layout, work, input, target)
            }
            InputRequest::Capture(pointer, target) => request_capture(
                elements,
                configuration,
                layout,
                work,
                input,
                pointer,
                target,
            ),
            InputRequest::Release(pointer, target) => {
                if elements.row(target).is_some() {
                    release_capture(elements, input, pointer, Some(target.local()));
                }
                Ok(())
            }
        };
        return result.map(|()| 1);
    }
    if !input.actions.is_empty() {
        return Ok(0);
    }

    let Some(tab) = input.batch.default_tab.take() else {
        return Ok(0);
    };
    if !input.window_focused || input.batch.suppress_acquisition || input.closing {
        return Ok(1);
    }
    let target = tab
        .next(input.focus, |key| elements.rank(key))
        .map(|key| elements.id(key));
    request_focus(elements, configuration, layout, work, input, target).map(|()| 1)
}

fn request_focus(
    elements: &mut MountedElements,
    configuration: &Configuration,
    layout: &mut Layout,
    work: &mut Scheduler,
    input: &mut InputState,
    target: Option<ElementId>,
) -> Result<(), ElementError> {
    input.deferred_focus = None;
    let Some(target) = target else {
        return set_focus(elements, configuration, layout, work, input, None);
    };
    if !input.window_focused
        || input.batch.suppress_acquisition
        || input.closing
        || !elements.contains(target)
    {
        return Ok(());
    }
    if qualified(elements, layout, target.local()) {
        return set_focus(
            elements,
            configuration,
            layout,
            work,
            input,
            Some(target.local()),
        );
    }
    if configuration.has_frontier()
        || work.has_prepare()
        || work.has_layout()
        || work.has_geometry()
    {
        input.deferred_focus = Some(target);
    }
    Ok(())
}

pub(crate) fn resolve_deferred_focus(
    elements: &mut MountedElements,
    configuration: &Configuration,
    layout: &mut Layout,
    work: &mut Scheduler,
    input: &mut InputState,
) -> Result<(), ElementError> {
    let Some(target) = input.deferred_focus.take() else {
        return Ok(());
    };
    if !input.window_focused
        || input.batch.suppress_acquisition
        || input.closing
        || !elements.contains(target)
        || !qualified(elements, layout, target.local())
    {
        return Ok(());
    }
    set_focus(
        elements,
        configuration,
        layout,
        work,
        input,
        Some(target.local()),
    )
}

fn set_focus(
    elements: &mut MountedElements,
    configuration: &Configuration,
    layout: &mut Layout,
    work: &mut Scheduler,
    input: &mut InputState,
    target: Option<LocalKey>,
) -> Result<(), ElementError> {
    if input.focus == target {
        return Ok(());
    }
    if let Some(previous) = input.focus.take() {
        elements.change_active(previous, false);
        dispatch(
            elements,
            configuration,
            layout,
            work,
            input,
            previous,
            &InputEvent::Focus(false),
            EventPhase::Target,
            true,
        )?;
    }
    input.focus = target;
    if let Some(target) = target {
        elements.change_active(target, true);
        dispatch(
            elements,
            configuration,
            layout,
            work,
            input,
            target,
            &InputEvent::Focus(true),
            EventPhase::Target,
            false,
        )?;
    }
    Ok(())
}

fn request_capture(
    elements: &mut MountedElements,
    configuration: &Configuration,
    layout: &mut Layout,
    work: &mut Scheduler,
    input: &mut InputState,
    pointer: PointerId,
    target: ElementId,
) -> Result<(), ElementError> {
    if input.closing
        || !input.window_focused
        || input.batch.suppress_acquisition
        || input.batch.terminal_pointers.contains(&pointer)
        || !elements.contains(target)
        || !qualified(elements, layout, target.local())
    {
        return Ok(());
    }
    let Some(state) = input.pointers.get_mut(&pointer) else {
        return Ok(());
    };
    if state.capture == Some(target.local()) {
        return Ok(());
    }
    let previous = state.capture.take();
    let position = state.position;
    if let Some(previous) = previous {
        elements.change_active(previous, false);
        dispatch(
            elements,
            configuration,
            layout,
            work,
            input,
            previous,
            &InputEvent::Pointer {
                id: pointer,
                kind: PointerKind::Cancel,
                position,
            },
            EventPhase::Target,
            true,
        )?;
    }
    input
        .pointers
        .get_mut(&pointer)
        .expect("observed pointer survives its cancellation callback")
        .capture = Some(target.local());
    elements.change_active(target.local(), true);
    Ok(())
}

fn release_capture(
    elements: &mut MountedElements,
    input: &mut InputState,
    pointer: PointerId,
    owner: Option<LocalKey>,
) {
    let Some(state) = input.pointers.get_mut(&pointer) else {
        return;
    };
    if owner.is_some_and(|owner| state.capture != Some(owner)) {
        return;
    }
    if let Some(previous) = state.capture.take() {
        elements.change_active(previous, false);
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "Hover transitions need disjoint owners plus pointer, target and real position"
)]
fn set_hover(
    elements: &mut MountedElements,
    configuration: &Configuration,
    layout: &mut Layout,
    work: &mut Scheduler,
    input: &mut InputState,
    pointer: PointerId,
    target: Option<LocalKey>,
    position: Point,
) -> Result<(), ElementError> {
    let target = target.filter(|key| {
        input.window_focused
            && !input.batch.suppress_acquisition
            && !input.closing
            && qualified(elements, layout, *key)
    });
    let Some(state) = input.pointers.get_mut(&pointer) else {
        return Ok(());
    };
    let previous = state.hover;
    state.position = position;
    if previous == target {
        return Ok(());
    }
    state.hover = None;
    if let Some(previous) = previous {
        dispatch(
            elements,
            configuration,
            layout,
            work,
            input,
            previous,
            &InputEvent::Pointer {
                id: pointer,
                kind: PointerKind::Leave,
                position,
            },
            EventPhase::Target,
            true,
        )?;
    }
    if let Some(target) = target {
        input
            .pointers
            .get_mut(&pointer)
            .expect("observed pointer survives its leave callback")
            .hover = Some(target);
        dispatch(
            elements,
            configuration,
            layout,
            work,
            input,
            target,
            &InputEvent::Pointer {
                id: pointer,
                kind: PointerKind::Enter,
                position,
            },
            EventPhase::Target,
            false,
        )?;
    }
    Ok(())
}

fn cancel_pointer(
    elements: &mut MountedElements,
    configuration: &Configuration,
    layout: &mut Layout,
    work: &mut Scheduler,
    input: &mut InputState,
    pointer: PointerId,
) -> Result<Option<LocalKey>, ElementError> {
    let Some(state) = input.pointers.get_mut(&pointer) else {
        return Ok(None);
    };
    state.inside = false;
    let capture = state.capture.take();
    let hover = state.hover.take();
    let position = state.position;
    if let Some(capture) = capture {
        elements.change_active(capture, false);
    }
    let target = capture.or(hover);
    let mut first_error = None;
    if let Some(target) = target {
        record_error(
            &mut first_error,
            dispatch(
                elements,
                configuration,
                layout,
                work,
                input,
                target,
                &InputEvent::Pointer {
                    id: pointer,
                    kind: PointerKind::Cancel,
                    position,
                },
                EventPhase::Target,
                true,
            )
            .map(|_| ()),
        );
    }
    if let Some(hover) = hover {
        record_error(
            &mut first_error,
            dispatch(
                elements,
                configuration,
                layout,
                work,
                input,
                hover,
                &InputEvent::Pointer {
                    id: pointer,
                    kind: PointerKind::Leave,
                    position,
                },
                EventPhase::Target,
                true,
            )
            .map(|_| ()),
        );
    }
    match first_error {
        Some(error) => Err(error),
        None => Ok(target),
    }
}

pub(crate) fn cancel_invalid(
    elements: &mut MountedElements,
    configuration: &Configuration,
    layout: &mut Layout,
    work: &mut Scheduler,
    input: &mut InputState,
) -> Result<(), ElementError> {
    cancel_references(elements, configuration, layout, work, input, false)
}

fn cancel_all(
    elements: &mut MountedElements,
    configuration: &Configuration,
    layout: &mut Layout,
    work: &mut Scheduler,
    input: &mut InputState,
) -> Result<(), ElementError> {
    cancel_references(elements, configuration, layout, work, input, true)
}

fn cancel_references(
    elements: &mut MountedElements,
    configuration: &Configuration,
    layout: &mut Layout,
    work: &mut Scheduler,
    input: &mut InputState,
    all: bool,
) -> Result<(), ElementError> {
    let mut cleanup = std::mem::take(&mut input.cleanup);
    if input
        .focus
        .is_some_and(|key| all || !input.window_focused || !qualified(elements, layout, key))
    {
        let key = input
            .focus
            .take()
            .expect("the selected invalid focus was present");
        elements.change_active(key, false);
        cleanup.push(Cleanup::Focus(key));
    }
    for (&pointer, state) in &mut input.pointers {
        if all {
            state.inside = false;
        }
        if state
            .capture
            .is_some_and(|key| all || !qualified(elements, layout, key))
        {
            let target = state
                .capture
                .take()
                .expect("the selected invalid capture was present");
            elements.change_active(target, false);
            cleanup.push(Cleanup::Capture {
                pointer,
                target,
                position: state.position,
            });
        }
        if state
            .hover
            .is_some_and(|key| all || !qualified(elements, layout, key))
        {
            let target = state
                .hover
                .take()
                .expect("the selected invalid hover was present");
            cleanup.push(Cleanup::Hover {
                pointer,
                target,
                position: state.position,
            });
        }
    }

    let mut first_error = None;
    for item in cleanup.drain(..) {
        let (target, event) = match item {
            Cleanup::Focus(target) => (target, InputEvent::Focus(false)),
            Cleanup::Capture {
                pointer,
                target,
                position,
            } => (
                target,
                InputEvent::Pointer {
                    id: pointer,
                    kind: PointerKind::Cancel,
                    position,
                },
            ),
            Cleanup::Hover {
                pointer,
                target,
                position,
            } => (
                target,
                InputEvent::Pointer {
                    id: pointer,
                    kind: PointerKind::Leave,
                    position,
                },
            ),
        };
        record_error(
            &mut first_error,
            dispatch(
                elements,
                configuration,
                layout,
                work,
                input,
                target,
                &event,
                EventPhase::Target,
                true,
            )
            .map(|_| ()),
        );
    }
    retain_buffer(&mut input.cleanup, cleanup);
    let rehit = std::mem::take(&mut input.rehit_pending);
    if rehit && !all && input.window_focused && !input.batch.suppress_acquisition && !input.closing
    {
        let mut pointers = std::mem::take(&mut input.pointer_scratch);
        pointers.extend(
            input
                .pointers
                .iter()
                .filter(|(_, state)| state.inside)
                .map(|(&pointer, state)| (pointer, state.position)),
        );
        for &(pointer, position) in &pointers {
            let target = hit_target(elements, configuration, layout, work, input, position);
            record_error(
                &mut first_error,
                set_hover(
                    elements,
                    configuration,
                    layout,
                    work,
                    input,
                    pointer,
                    target,
                    position,
                ),
            );
        }
        retain_buffer(&mut input.pointer_scratch, pointers);
    }
    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// Releases terminal masks only at complete same-cycle settlement, never at a yield.
pub(crate) fn finish_batch(input: &mut InputState) {
    debug_assert!(
        !input.has_effects(),
        "settlement cannot discard runnable input effects"
    );
    debug_assert!(
        input.cleanup_poses.is_empty() && input.cleanup_clip_bounds.is_empty(),
        "settlement follows cleanup-pose delivery and explicit release"
    );

    input.deferred_focus = None;
    input.batch.default_tab = None;
    input.batch.terminal_pointers.clear();
    input.batch.suppress_acquisition = input.closing;
    input
        .pointers
        .retain(|_, state| state.inside || state.hover.is_some() || state.capture.is_some());
    input.trim();
}

pub(crate) fn close(
    elements: &mut MountedElements,
    configuration: &Configuration,
    layout: &mut Layout,
    work: &mut Scheduler,
    input: &mut InputState,
) -> Result<(), ElementError> {
    begin_close(input);
    let result = cancel_all(elements, configuration, layout, work, input);
    discard_effects(input);
    result
}

pub(crate) fn begin_close(input: &mut InputState) {
    input.closing = true;
    input.batch.suppress_acquisition = true;
    discard_effects(input);
}

pub(crate) fn discard_effects(input: &mut InputState) {
    input.deferred_focus = None;
    input.batch.default_tab = None;
    input.request_queue.clear();
    input.requests.clear();
    input.event_requests.clear();

    let actions = std::mem::take(&mut input.actions);
    let emissions = std::mem::take(&mut input.event_emissions);
    drop(actions);
    drop(emissions);
}

fn record_error(first: &mut Option<ElementError>, result: Result<(), ElementError>) {
    if let Err(error) = result
        && first.is_none()
    {
        *first = Some(error);
    }
}
