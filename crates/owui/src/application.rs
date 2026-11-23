//! Logical window lifetimes, consistent publication, and host-driven processing.
//!
//! [`App`] owns logical windows and their mounted state. [`Window`] supplies initial
//! settings; creating it through [`App::window`] neither opens a native window nor
//! starts an event loop or server. The host provides input and consumes Scene and
//! semantic output. Use [`App::close`] for terminal interaction cancellation and
//! unmount callbacks.
//!
//! # Window readiness
//!
//! A logical window has three processing states:
//!
//! - **Ready:** the required work for its current publication is complete. Child,
//!   geometry, clipping, semantics, Scene, and focus reads are available; independent input
//!   can be admitted. Hidden, Collapsed, or unmeasured retained elements need not
//!   have public output.
//! - **Working:** processing or an admitted input cycle is unfinished. A bounded
//!   [`App::process`] can return [`ProcessOutcome::Pending`] in this state. Dependent
//!   reads and readiness-only submission return [`AppError::NotReady`]; resume
//!   processing, invalidate more work, or close the window.
//! - **Failed:** an escaped processing failure prevents further updates or input.
//!   Dependent reads return [`AppError::Failed`]. Close the window and construct a
//!   replacement; there is no resume operation for a failed window.
//!
//! Root identity, title, element/window counts, cumulative work counts, and progress
//! are diagnostic reads available while Working or Failed. They do not expose a
//! consistent output snapshot. [`App::progress`] reports a phase only while Working.
//!
//! # Processing and input cycles
//!
//! Each cycle admits the rebuild-hint Inbox once, at its first processing or input
//! submission entry. Hints arriving after that admission wait for a later cycle;
//! resuming Pending does not receive another batch. An [`Invalidator`](crate::view::Invalidator)
//! enqueues a hint but does not execute a View build by itself.
//!
//! [`App::submit_input`] and [`App::submit_activation`] require Ready output, route
//! one input synchronously against that prepared output, and leave its effects for
//! [`App::process`]. They do not first settle newly received rebuild hints. A host
//! that needs those hints published before admission processes them first.
//! [`App::dispatch`] and [`App::activate`] are synchronous conveniences: settle
//! existing work, admit one input, then settle that input cycle. [`App::update`]
//! resumes or starts a cycle without admitting input.
//!
//! Input emissions capture their source's handler binding before yielding. Owned
//! original Actions drain before mutable configuration installation, child edits,
//! prepare, or layout callbacks; they are not rebound to later handlers on resume.
//! Requests produced during mutable processing wait for stable current qualification.
//!
//! Budgets are cooperative safe-boundary limits, not deadlines or callback counts.
//! A nested measurement or finite mandatory cleanup can exceed a nominal unit;
//! user callbacks cannot be preempted. Pending retains completed callbacks, State
//! changes, and nested edits rather than rolling them back or replaying them.
//! Unlimited conveniences have no termination guarantee for infinite user feedback.
//!
//! # Errors and panics
//!
//! Operational admission errors, such as a foreign activation target, are rejected
//! before routing and do not by themselves fail the window. A processing error or
//! unresolved required work marks it Failed. An unwinding panic crossing the window
//! boundary also marks it Failed; panics are not converted into [`AppError`]. State
//! and already accepted nested edits are not transactional. Normal close errors do
//! not restore an invalidated window handle.

pub(crate) mod window;

use thiserror::Error;

use crate::element::{ElementError, ElementId, Key};
use crate::id::Slots;
use crate::input::{InputEvent, Semantics};
use crate::layout::{ClipBounds, Geometry, Size};
use crate::scene::Scene;
use crate::schedule::{ProcessBudget, ProcessOutcome, ProcessPhase, Progress, Work, WorkCounts};
use crate::view::{InitCx, View};
use window::{WindowPhase, WindowState};

/// A domain- and generation-checked window handle that does not keep it open.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct WindowId(crate::id::Id);

impl crate::id::Key for WindowId {
    fn from_id(id: crate::id::Id) -> Self {
        Self(id)
    }

    fn id(self) -> crate::id::Id {
        self.0
    }
}

/// Initial title and size of a logical window.
///
/// This value is a declaration of settings. [`App::window`] creates and settles the
/// logical UI; neither operation creates a native window or rendering surface.
pub struct Window {
    title: String,
    size: Size,
}

impl Window {
    /// Sets the title and uses an initial size of 800 by 600 logical units.
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            size: Size::new(800.0, 600.0),
        }
    }

    /// Sets the initial width and height in logical units.
    ///
    /// [`App::window`] rejects nonfinite or negative dimensions when creating the window.
    pub fn size(mut self, width: f32, height: f32) -> Self {
        self.size = Size::new(width, height);
        self
    }
}

/// Failures of logical window admission, processing, and cleanup.
///
/// [`Self::NotReady`] is an unfinished cycle, not a failed window: resume with
/// [`App::process`]. [`Self::Failed`] requires close and reconstruction. An invalid
/// size or activation target rejected before callbacks does not itself fail a window.
#[derive(Debug, Error)]
pub enum AppError {
    /// The handle is closed, stale, or belongs to another App.
    #[error("logical window {0:?} is closed")]
    Closed(WindowId),
    #[error("window size must be finite and nonnegative")]
    InvalidSize,
    #[error(transparent)]
    Element(#[from] ElementError),
    /// Dependent output or independent input was requested while the window is Working.
    #[error("window output is not ready; resume processing before dependent reads or new input")]
    NotReady,
    /// Processing cannot resume; diagnostic reads and [`App::close`] remain available.
    #[error("window processing failed; close and reconstruct this window before continuing")]
    Failed,
    #[error("operation failed: {operation}; cleanup failed: {cleanup}")]
    Cleanup {
        operation: Box<AppError>,
        cleanup: Box<AppError>,
    },
}

/// Coordinates logical windows without owning the application's business services.
pub struct App {
    windows: Slots<WindowId, WindowState>,
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

impl App {
    /// Creates an empty logical-window owner without starting background processing.
    pub fn new() -> Self {
        Self {
            windows: Slots::with_key(),
        }
    }

    /// Runs application initialization synchronously and returns this App on success.
    ///
    /// Errors from the supplied closure are returned unchanged.
    pub fn initialize(
        mut self,
        initialize: impl FnOnce(&mut Self) -> Result<(), AppError>,
    ) -> Result<Self, AppError> {
        initialize(&mut self)?;

        Ok(self)
    }

    /// Mounts a root View and synchronously settles the window's first publication.
    ///
    /// The factory runs once for the new root identity and receives its [`InitCx`].
    /// Success returns a Ready logical window; no native window or renderer is created.
    /// Initial processing uses [`ProcessBudget::Unlimited`] and can run indefinitely
    /// if user callbacks do not return or keep producing work.
    ///
    /// # Errors
    ///
    /// Invalid dimensions return [`AppError::InvalidSize`] before creation. If initial
    /// publication fails, cleanup is attempted before returning the error; a second
    /// cleanup error is reported with [`AppError::Cleanup`]. No window handle is returned.
    pub fn window<V: View>(
        &mut self,
        window: Window,
        create: impl FnOnce(&mut InitCx<'_, V>) -> V + 'static,
    ) -> Result<WindowId, AppError> {
        if !window.size.is_valid() {
            return Err(AppError::InvalidSize);
        }

        let mut state = WindowState::new(window.title, window.size, create)?;
        if let Err(operation) = state.update() {
            return match state.close() {
                Ok(()) => Err(operation),
                Err(cleanup) => Err(AppError::Cleanup {
                    operation: Box::new(operation),
                    cleanup: Box::new(cleanup),
                }),
            };
        }

        Ok(self.windows.insert(state))
    }

    /// Starts or resumes one processing cycle with an unlimited cooperative budget.
    ///
    /// This does not admit input. It returns after settlement, not Pending; infinite
    /// returning feedback has no termination guarantee. Use [`Self::process`] when
    /// the host needs to yield between complete units. Counts cover this call only.
    pub fn update(&mut self, window: WindowId) -> Result<WorkCounts, AppError> {
        self.get_mut(window)?.update()
    }

    /// Starts or resumes a cycle under a soft safe-boundary budget.
    ///
    /// [`ProcessOutcome::Settled`] makes dependent reads available.
    /// [`ProcessOutcome::Pending`] leaves the window Working without undoing completed
    /// callbacks or State changes. Call this method again to continue the same cycle;
    /// the Inbox is not admitted again on that resume.
    ///
    /// `ProcessBudget::Steps(0)` can settle if no admitted unit remains necessary,
    /// and using exactly the required units also settles. A budget is not a callback
    /// count or deadline: synchronous nested measurement, mandatory finite cleanup,
    /// and bookkeeping can exceed its nominal granularity. Counts cover this call only.
    ///
    /// # Errors
    ///
    /// Closed and Failed windows cannot be processed. Escaped processing errors or
    /// unresolved required work fail the window; Pending is not an error. User panics
    /// propagate rather than becoming an [`AppError`].
    pub fn process(
        &mut self,
        window: WindowId,
        budget: ProcessBudget,
    ) -> Result<ProcessOutcome, AppError> {
        self.get_mut(window)?.process(budget)
    }

    /// Settles each open window synchronously, stopping at the first error.
    ///
    /// Earlier windows are not rolled back if a later window fails. Like [`Self::update`],
    /// this has no termination guarantee for infinite user feedback.
    pub fn update_all(&mut self) -> Result<(), AppError> {
        for (_, window) in self.windows.iter_mut() {
            window.update()?;
        }

        Ok(())
    }

    /// Settles existing work, routes one input against Ready output, then settles its cycle.
    ///
    /// Both processing passes use [`ProcessBudget::Unlimited`]. For cooperative host
    /// scheduling, use [`Self::submit_input`] followed by [`Self::process`] instead.
    /// The returned target identifies the admission route; resulting Actions may have
    /// retired that element by the time this method returns.
    pub fn dispatch(
        &mut self,
        window: WindowId,
        event: InputEvent,
    ) -> Result<Option<ElementId>, AppError> {
        self.get_mut(window)?.dispatch(event)
    }

    /// Settles existing work, admits activation of the target, then settles its cycle.
    ///
    /// The target must be live and qualified for activation, but need not be the current
    /// focus or a Tab stop. This is the unlimited counterpart of [`Self::submit_activation`]
    /// plus [`Self::process`], not a native input injection API.
    pub fn activate(&mut self, window: WindowId, target: ElementId) -> Result<(), AppError> {
        self.get_mut(window)?.activate(target)
    }

    /// Routes one input synchronously against a Ready window and starts its Working cycle.
    ///
    /// This may execute routing callbacks and collect effects, but does not settle them.
    /// Resume with [`Self::process`]; even a successful admission with no target needs
    /// processing before another independent input or dependent read.
    ///
    /// The cycle admits the Inbox once without first processing its new rebuild hints.
    /// If those hints must be published before this input, process them first. Captured
    /// original Actions retain their source binding and drain before mutable processing.
    ///
    /// # Errors
    ///
    /// Working output returns [`AppError::NotReady`]; Closed and Failed windows reject
    /// submission. Errors or unwinding panics escaping routing fail the window.
    pub fn submit_input(
        &mut self,
        window: WindowId,
        event: InputEvent,
    ) -> Result<Option<ElementId>, AppError> {
        self.get_mut(window)?.submit_input(event)
    }

    /// Admits activation of a qualified live target against Ready output without settling.
    ///
    /// The target need not be focused or a Tab stop. Success starts a Working cycle;
    /// use [`Self::process`] to drain its captured effects and publish the result. Like
    /// [`Self::submit_input`], this does not pre-settle newly admitted rebuild hints.
    ///
    /// # Errors
    ///
    /// Working windows return [`AppError::NotReady`]. A rejected target is reported
    /// before callbacks and does not itself fail the window. Closed/Failed windows
    /// reject admission; failures escaping routed callbacks fail the window.
    pub fn submit_activation(
        &mut self,
        window: WindowId,
        target: ElementId,
    ) -> Result<(), AppError> {
        self.get_mut(window)?.submit_activation(target)
    }

    /// Changes the logical viewport size and synchronously settles its consequences.
    ///
    /// Nonfinite or negative dimensions return [`AppError::InvalidSize`] before mutation.
    /// This can finish an existing Working cycle and uses unlimited processing, not a
    /// bounded resize operation.
    pub fn resize(&mut self, window: WindowId, size: Size) -> Result<(), AppError> {
        if !size.is_valid() {
            return Err(AppError::InvalidSize);
        }

        self.get_mut(window)?.resize(size)
    }

    /// Marks current element work without replacing a declaration or invoking callbacks.
    ///
    /// Nonempty work changes a Ready window to Working; further invalidation can be
    /// merged while Working. Empty work is a no-op after identity validation. A stale,
    /// foreign, or retired element is rejected without failing an otherwise usable window.
    pub fn invalidate(
        &mut self,
        window: WindowId,
        id: ElementId,
        work: Work,
    ) -> Result<(), AppError> {
        let window = self.get_mut(window)?;
        if !window.elements.contains(id) {
            return Err(ElementError::Missing(id).into());
        }

        if !work.is_empty() {
            if window.phase == WindowPhase::Ready {
                window.progress = Progress::default();
                window.phase = WindowPhase::Working;
            }
            window.access().request(id, work);
        }

        Ok(())
    }

    /// Invalidates the handle, discards business effects, and performs terminal cleanup.
    ///
    /// This is available for Ready, Working, and Failed windows. It does not run queued
    /// business Actions merely to close. Interactions are cancelled while records exist,
    /// then initialized elements unmount children first. Normal cleanup errors do not
    /// skip the other required cleanup callbacks. The handle stays invalid even on Err.
    pub fn close(&mut self, window: WindowId) -> Result<(), AppError> {
        let mut state = self
            .windows
            .remove(window)
            .ok_or(AppError::Closed(window))?;

        state.close()
    }

    /// Reads the root identity diagnostically, including while Working or Failed.
    pub fn root(&self, window: WindowId) -> Result<ElementId, AppError> {
        Ok(self.get(window)?.root)
    }

    /// Looks up a direct child in a Ready window's current logical structure.
    ///
    /// Working/Failed windows reject this dependent read. A missing child or parent
    /// identity returns None rather than manufacturing a handle.
    pub fn child(
        &self,
        window: WindowId,
        parent: ElementId,
        key: impl Into<Key>,
    ) -> Result<Option<ElementId>, AppError> {
        let window = self.get(window)?;
        window.ready()?;

        Ok(window.elements.find_child(parent, &key.into()))
    }

    /// Reads current participating geometry from a Ready window.
    ///
    /// Returns None for an unavailable element or geometry, including an unmeasured or
    /// parked element. Hidden elements can retain layout geometry without Scene output.
    /// [`Geometry::clip`] is a conservative Rect cover; use [`Self::clip_bounds`] when
    /// exact half-open clipping membership matters.
    pub fn geometry(&self, window: WindowId, id: ElementId) -> Result<Option<Geometry>, AppError> {
        let window = self.get(window)?;
        window.ready()?;

        Ok(window
            .elements
            .row(id)
            .and_then(|row| window.layout.geometry(row)))
    }

    /// Reads exact half-open clipping endpoints for current participating geometry.
    ///
    /// This uses the same Ready gate and geometry availability as [`Self::geometry`].
    /// The rectangle in [`Geometry::clip`] is a conservative representable cover and
    /// may include an exclusive endpoint because of subtract-and-reconstruct rounding.
    /// Use these [`ClipBounds`] endpoints for exact membership or clipping decisions.
    pub fn clip_bounds(
        &self,
        window: WindowId,
        id: ElementId,
    ) -> Result<Option<ClipBounds>, AppError> {
        let window = self.get(window)?;
        window.ready()?;

        Ok(window
            .elements
            .row(id)
            .filter(|row| window.layout.geometry(*row).is_some())
            .map(|row| window.layout.clip_bounds(id.local(), row)))
    }

    /// Reads an element's published semantic projection from a Ready window.
    ///
    /// Returns None when no semantic content is published for the identity. Working
    /// and Failed windows reject the read rather than exposing a mixed projection.
    pub fn semantics(
        &self,
        window: WindowId,
        id: ElementId,
    ) -> Result<Option<&Semantics>, AppError> {
        let window = self.get(window)?;
        window.ready()?;

        Ok(window.input.semantic(id, &window.elements))
    }

    /// Borrows the composed local drawing fragments of a Ready window.
    ///
    /// Rendering and text shaping belong to the consumer. Working/Failed output is
    /// rejected; no partially processed Scene is published through this method.
    pub fn scene(&self, window: WindowId) -> Result<&Scene, AppError> {
        let window = self.get(window)?;
        window.ready()?;

        Ok(&window.scene)
    }

    /// Reads element focus after the Ready window's qualification and effects settle.
    pub fn focused(&self, window: WindowId) -> Result<Option<ElementId>, AppError> {
        let window = self.get(window)?;
        window.ready()?;

        Ok(window.input.focused(&window.elements))
    }

    /// Reads cumulative diagnostic work counts, including while Working or Failed.
    ///
    /// Counts include window initialization, input routing, and processing. In contrast,
    /// [`ProcessOutcome`] contains the counts for its one processing call.
    pub fn counts(&self, window: WindowId) -> Result<WorkCounts, AppError> {
        Ok(self.get(window)?.work.counts)
    }

    /// Reads the current live logical-element count diagnostically in any window state.
    ///
    /// This is not a Scene fragment, visible element, or native resource count.
    pub fn element_count(&self, window: WindowId) -> Result<usize, AppError> {
        Ok(self.get(window)?.elements.len())
    }

    /// Borrows the logical title diagnostically, including while Working or Failed.
    pub fn title(&self, window: WindowId) -> Result<&str, AppError> {
        Ok(&self.get(window)?.title)
    }

    /// Reads the unfinished prerequisite phase while Working, including after Pending.
    ///
    /// Ready and Failed windows return None; this diagnostic is not a Ready/Failed test.
    pub fn progress(&self, window: WindowId) -> Result<Option<ProcessPhase>, AppError> {
        let window = self.get(window)?;
        Ok((window.phase == WindowPhase::Working).then_some(window.progress.phase))
    }

    /// Reads the number of logical windows still owned by this App.
    pub fn window_count(&self) -> usize {
        self.windows.len()
    }

    fn get(&self, id: WindowId) -> Result<&WindowState, AppError> {
        self.windows.get(id).ok_or(AppError::Closed(id))
    }

    fn get_mut(&mut self, id: WindowId) -> Result<&mut WindowState, AppError> {
        let window = self.windows.get_mut(id).ok_or(AppError::Closed(id))?;
        window.check()?;

        Ok(window)
    }
}
