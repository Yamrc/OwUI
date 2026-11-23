//! Window-owned subsystem lifetimes and checked public operation boundaries.

use std::sync::Arc;

use crate::element::configuration::Configuration;
use crate::element::mounted::{self, ElementAccess, Life, MountedElements};
use crate::element::{ElementError, ElementId};
use crate::input::{InputEvent, InputState, route};
use crate::layout::{Layout, Size};
use crate::scene::Scene;
use crate::schedule::{ProcessBudget, ProcessOutcome, Progress, Scheduler, Work, WorkCounts};
use crate::view::{Inbox, InitCx, View, ViewElement};

use super::AppError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WindowPhase {
    Ready,
    Working,
    Failed,
}

pub(crate) struct WindowState {
    pub(crate) elements: MountedElements,
    pub(crate) configuration: Configuration,
    pub(crate) layout: Layout,
    pub(crate) work: Scheduler,
    pub(crate) input: InputState,
    pub(crate) scene: Scene,
    pub(crate) inbox: Arc<Inbox>,
    pub(crate) root: ElementId,
    pub(crate) title: String,
    pub(crate) size: Size,
    pub(crate) phase: WindowPhase,
    pub(crate) epoch: u64,
    pub(crate) progress: Progress,
}

impl WindowState {
    pub(crate) fn new<V: View>(
        title: String,
        size: Size,
        create: impl FnOnce(&mut InitCx<'_, V>) -> V + 'static,
    ) -> Result<Self, AppError> {
        let mut elements = MountedElements::new();
        let mut configuration = Configuration::new();
        let mut layout = Layout::new(size);
        let mut work = Scheduler::new();
        let mut input = InputState::new();
        let inbox = Arc::new(Inbox::default());

        let root = {
            let mut access = ElementAccess {
                elements: &mut elements,
                configuration: &mut configuration,
                layout: &mut layout,
                work: &mut work,
                inbox: &inbox,
                requests: input.requests_mut(),
            };
            mounted::apply(&mut access, None, None, ViewElement::new(create))?
        };

        Ok(Self {
            elements,
            configuration,
            layout,
            work,
            input,
            scene: Scene::default(),
            inbox,
            root,
            title,
            size,
            phase: WindowPhase::Working,
            epoch: 0,
            progress: Progress::default(),
        })
    }

    pub(crate) fn access(&mut self) -> ElementAccess<'_> {
        ElementAccess {
            elements: &mut self.elements,
            configuration: &mut self.configuration,
            layout: &mut self.layout,
            work: &mut self.work,
            inbox: &self.inbox,
            requests: self.input.requests_mut(),
        }
    }

    pub(crate) fn check(&self) -> Result<(), AppError> {
        if self.phase == WindowPhase::Failed {
            Err(AppError::Failed)
        } else {
            Ok(())
        }
    }

    pub(crate) fn ready(&self) -> Result<(), AppError> {
        self.check()?;

        if self.phase == WindowPhase::Ready {
            Ok(())
        } else {
            Err(AppError::NotReady)
        }
    }

    fn guarded<R>(
        &mut self,
        operation: impl FnOnce(&mut Self) -> Result<R, AppError>,
    ) -> Result<R, AppError> {
        struct Operation<'a> {
            window: &'a mut WindowState,
            finished: bool,
        }

        impl Drop for Operation<'_> {
            fn drop(&mut self) {
                if !self.finished {
                    self.window.phase = WindowPhase::Failed;
                    crate::schedule::clear_ancestry(self.window);
                    route::discard_effects(&mut self.window.input);
                }
            }
        }

        self.phase = WindowPhase::Working;
        let mut guard = Operation {
            window: self,
            finished: false,
        };

        let result = operation(guard.window);

        if result.is_err() {
            guard.window.phase = WindowPhase::Failed;
            crate::schedule::clear_ancestry(guard.window);
            route::discard_effects(&mut guard.window.input);
        }

        guard.finished = true;
        result
    }

    fn accept_wakes(&mut self) {
        if self.progress.accepted_wakes {
            return;
        }

        self.progress.accepted_wakes = true;
        for wake in self.inbox.drain() {
            let row = if wake.accept() {
                self.elements
                    .row(wake.owner)
                    .filter(|row| self.elements.nodes[row.0].life == Life::Live)
            } else {
                None
            };

            if let Some(row) = row {
                self.layout
                    .note_work(&self.elements, wake.owner.local(), Work::PREPARE);
                self.work.request(wake.owner.local(), row, Work::PREPARE);
            } else {
                self.work.counts.retired_requests =
                    self.work.counts.retired_requests.saturating_add(1);
            }
        }
    }

    pub(crate) fn process(&mut self, budget: ProcessBudget) -> Result<ProcessOutcome, AppError> {
        self.check()?;

        let before = self.work.counts;
        if self.phase == WindowPhase::Ready {
            self.progress = Progress::default();
        }

        self.accept_wakes();
        if self.phase == WindowPhase::Ready
            && !self.work.has_tickets()
            && !self.configuration.has_frontier()
            && !self.input.has_effects()
            && self.elements.order_from.is_none()
            && self.elements.retired.is_empty()
        {
            return Ok(ProcessOutcome::Settled {
                epoch: self.epoch,
                counts: self.work.counts - before,
            });
        }

        self.guarded(|window| {
            let settled = crate::schedule::drive(window, budget)?;

            if settled {
                window.epoch = window
                    .epoch
                    .checked_add(1)
                    .ok_or(ElementError::RevisionExhausted)?;
                window.phase = WindowPhase::Ready;

                Ok(ProcessOutcome::Settled {
                    epoch: window.epoch,
                    counts: window.work.counts - before,
                })
            } else {
                Ok(ProcessOutcome::Pending {
                    phase: window.progress.phase,
                    counts: window.work.counts - before,
                })
            }
        })
    }

    pub(crate) fn update(&mut self) -> Result<WorkCounts, AppError> {
        match self.process(ProcessBudget::Unlimited)? {
            ProcessOutcome::Settled { counts, .. } => Ok(counts),
            ProcessOutcome::Pending { .. } => {
                unreachable!("an unlimited cooperative drive only returns at settlement")
            }
        }
    }

    pub(crate) fn submit_input(
        &mut self,
        event: InputEvent,
    ) -> Result<Option<ElementId>, AppError> {
        self.ready()?;

        self.progress = Progress::default();
        self.accept_wakes();

        self.guarded(|window| {
            Ok(route::submit(
                &mut window.elements,
                &window.configuration,
                &mut window.layout,
                &mut window.work,
                &mut window.input,
                event,
            )?)
        })
    }

    pub(crate) fn submit_activation(&mut self, target: ElementId) -> Result<(), AppError> {
        self.ready()?;
        if !self.elements.contains(target)
            || !crate::input::qualified(&self.elements, &self.layout, target.local())
        {
            return Err(ElementError::Missing(target).into());
        }

        self.progress = Progress::default();
        self.accept_wakes();

        self.guarded(|window| {
            Ok(route::submit_activation(
                &mut window.elements,
                &window.configuration,
                &mut window.layout,
                &mut window.work,
                &mut window.input,
                target,
            )?)
        })
    }

    pub(crate) fn dispatch(&mut self, event: InputEvent) -> Result<Option<ElementId>, AppError> {
        self.update()?;

        let target = self.submit_input(event)?;
        self.update()?;

        Ok(target)
    }

    pub(crate) fn activate(&mut self, target: ElementId) -> Result<(), AppError> {
        self.update()?;

        self.submit_activation(target)?;
        self.update()?;

        Ok(())
    }

    pub(crate) fn resize(&mut self, size: Size) -> Result<(), AppError> {
        self.check()?;
        if self.layout.set_viewport(size)? {
            self.size = size;
            self.work
                .require_viewport_layout(&self.elements, &self.layout);
            let root = self.root;
            let mut access = self.access();
            access.request(root, Work::LAYOUT);
        }
        self.update()?;
        Ok(())
    }

    pub(crate) fn close(&mut self) -> Result<(), AppError> {
        crate::schedule::clear_ancestry(self);
        self.phase = WindowPhase::Working;
        route::begin_close(&mut self.input);

        let targets: Vec<_> = self
            .input
            .focus
            .into_iter()
            .chain(
                self.input
                    .pointers
                    .values()
                    .flat_map(|pointer| pointer.hover.into_iter().chain(pointer.capture)),
            )
            .collect();
        for key in targets {
            if self.elements.row_local(key).is_some() {
                let id = self.elements.id(key);
                mounted::ensure_environment(&mut self.access(), id)?;
            }
        }

        let cancellation = route::close(
            &mut self.elements,
            &self.configuration,
            &mut self.layout,
            &mut self.work,
            &mut self.input,
        );

        self.elements.retire(self.root.local());
        let retirement = crate::schedule::retire(self);

        cancellation?;
        retirement?;
        Ok(())
    }
}
