//! Invalidation work, soft processing budgets, and resumable outcomes.
//!
//! [`crate::application::App::process`] admits complete framework units under a
//! [`ProcessBudget`]. Exhausting that budget can return [`ProcessOutcome::Pending`]
//! without failing the window or rolling back completed callbacks. It is not proof
//! that a finite feedback chain is invalid, nor a wall-clock deadline.
//!
//! Synchronous nested measurement and mandatory finite cleanup can exceed the
//! nominal granularity of a unit. Callbacks run on the caller's thread and cannot be
//! preempted; the library neither changes its stack size nor promises arbitrary depth.

use std::collections::{BTreeMap, VecDeque};

use bitflags::bitflags;

use crate::application::window::WindowState;
use crate::element::mounted::{self, ConfigDelivery, MountedElements, Row};
use crate::element::{ElementError, ElementId, UnmountCx};
use crate::id::LocalKey;
use crate::input::{SemanticCx, route};
use crate::layout::{self, Constraints, MeasureMode};
use crate::scene::{Fragment, SceneCx, SceneDeps, SceneDisposition};
use crate::view;

bitflags! {
    /// Element work invalidated by State or configuration changes.
    ///
    /// Flags can be combined. Marking work does not execute callbacks immediately;
    /// the host drives prerequisites with [`crate::application::App::process`] or
    /// an unlimited convenience. Deferred output on an inactive element need not
    /// produce a public fragment merely because a flag is set.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct Work: u8 {
        /// Prepare pending declarations or a View rebuild, including direct-child edits.
        const PREPARE = 1 << 0;
        /// Revalidate local measurement and its parent size/participation dependencies.
        const LAYOUT = 1 << 1;
        /// Re-record local drawing content when its output prerequisites are available.
        const SCENE = 1 << 2;
        /// Refresh local semantic content; qualification decoration is handled separately.
        const SEMANTICS = 1 << 3;
    }
}

impl Work {
    pub(crate) const STAGES: [Self; 4] =
        [Self::PREPARE, Self::LAYOUT, Self::SCENE, Self::SEMANTICS];

    pub(crate) const fn index(self) -> usize {
        self.bits().trailing_zeros() as usize
    }
}

/// A per-call soft budget for complete cooperative framework units.
///
/// A unit can be an owned request or Action, configuration delivery, preparation,
/// local measurement, or an output/projection consumer. It is not one callback or
/// a fixed amount of CPU time. Nested child work and finite mandatory cleanup or
/// reclamation can exceed that granularity; a callback that does not return cannot
/// be preempted. Pure bookkeeping and consistency checks can also run before a yield.
///
/// A yield preserves finished work and owned pending effects. It neither replays
/// consumed Actions nor classifies a large finite chain as a processing error.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ProcessBudget {
    /// Process until the current cycle settles.
    ///
    /// There is no termination guarantee for infinite returning feedback or a
    /// callback that does not return. Use [`Self::Steps`] for cooperative host scheduling.
    #[default]
    Unlimited,
    /// Admit at most this many further units, yielding only if necessary work remains.
    ///
    /// `Steps(0)` can return Settled when no admitted unit is needed. Finishing with
    /// exactly the available units also returns Settled, not an extra Pending result.
    Steps(usize),
}

/// The next unfinished prerequisite reported for a Working window.
///
/// This diagnostic can move back to an earlier phase when effects create new work;
/// it is not a promise that the next unit is a particular callback. Ready and Failed
/// are window states, not phases of this enum.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessPhase {
    /// Owned requests, captured Actions, or deferred focus qualification.
    Effects,
    /// Inherited-environment freshness and retained configuration delivery.
    Configuration,
    /// Pending declaration or View preparation.
    Prepare,
    /// Local measurement and parent contribution revalidation.
    Layout,
    /// Logical order, world geometry, qualification, and input projection.
    Spatial,
    /// Semantic content and qualification decoration.
    Semantics,
    /// Local recording, fragment attachment, and logical-order composition.
    Scene,
    /// Terminal cleanup, unmount callbacks, and safe owner reclamation.
    Retirement,
}

/// A processing result returned without an escaped failure.
///
/// Counts describe this one call, not lifetime totals. Pending is a resumable
/// Working state, not an error or a transaction rollback: callbacks and State changes
/// already completed remain accepted. Dependent reads and independent submission
/// stay unavailable until settlement. Failures are returned separately as
/// [`crate::application::AppError`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessOutcome {
    /// Required work for the current publication has settled and output is Ready.
    Settled {
        /// Publication epoch; an idle call can leave it unchanged.
        epoch: u64,
        /// Work observed during this call.
        counts: WorkCounts,
    },
    /// A safe budget boundary was reached with necessary work still unfinished.
    Pending {
        /// The prerequisite at which processing can continue.
        phase: ProcessPhase,
        /// Work observed during this call, which may already include executed Actions.
        counts: WorkCounts,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AttemptKind {
    Update,
    Prepare,
    Layout,
    Scene,
    Semantics,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RenderState {
    Unrecorded,
    Recorded,
    Empty,
    Released,
}

struct AncestryCursor {
    target: LocalKey,
    epoch: u64,
    path: Vec<LocalKey>,
    refreshed: Option<LocalKey>,
}

/// Owned continuation checkpoints between complete processing units.
///
/// A budget yield retains these queues/cursors, not an executing callback frame or
/// borrowed rollback snapshot. Inbox admission belongs to the cycle and is not reset
/// merely because a host resumes Pending.
pub(crate) struct Progress {
    pub(crate) phase: ProcessPhase,
    pub(crate) config_stack: Vec<LocalKey>,
    ancestry: Option<AncestryCursor>,
    pub(crate) effects_ready: bool,
    pub(crate) accepted_wakes: bool,
    order_current: bool,
    hit_current: bool,
    decoration_current: bool,
    scene_order_current: bool,
    spatial_pending: bool,
    cleanup_saved: bool,
    frontier_ready: bool,
    retirement_fresh: bool,
    scene_changed: bool,
}

impl Default for Progress {
    fn default() -> Self {
        Self {
            phase: ProcessPhase::Effects,
            config_stack: Vec::new(),
            ancestry: None,
            effects_ready: true,
            accepted_wakes: false,
            order_current: false,
            hit_current: false,
            decoration_current: false,
            scene_order_current: false,
            spatial_pending: false,
            cleanup_saved: false,
            frontier_ready: false,
            retirement_fresh: false,
            scene_changed: false,
        }
    }
}

#[derive(Clone, Copy, Default)]
struct SceneSpatial {
    bounds: bool,
    clip: bool,
    qualification: bool,
}

impl SceneSpatial {
    fn merge(&mut self, next: Self) {
        self.bounds |= next.bounds;
        self.clip |= next.clip;
        self.qualification |= next.qualification;
    }

    fn changed(self) -> bool {
        self.bounds || self.clip || self.qualification
    }

    fn observed_by(self, dependencies: SceneDeps) -> bool {
        (self.bounds && dependencies.contains(SceneDeps::WORLD_BOUNDS))
            || (self.clip && dependencies.contains(SceneDeps::WORLD_CLIP))
    }
}

struct Pending {
    work: Work,
    queued: Work,
    failed: Work,
    retry: Work,
    failed_update: bool,
    layout_notify: bool,
    layout_explicit: bool,
    layout_required: bool,
    geometry_queued: bool,
    render: RenderState,
    scene_deps: SceneDeps,
    scene_spatial: SceneSpatial,
    composition_queued: bool,
    unmount_started: bool,
}

impl Default for Pending {
    fn default() -> Self {
        Self {
            work: Work::empty(),
            queued: Work::empty(),
            failed: Work::empty(),
            retry: Work::empty(),
            failed_update: false,
            layout_notify: false,
            layout_explicit: false,
            layout_required: false,
            geometry_queued: false,
            render: RenderState::Unrecorded,
            scene_deps: SceneDeps::empty(),
            scene_spatial: SceneSpatial::default(),
            composition_queued: false,
            unmount_started: false,
        }
    }
}

/// Scheduling owns work and attempt accounting, not element/layout/output records.
pub(crate) struct Scheduler {
    rows: Vec<Pending>,
    stages: [VecDeque<LocalKey>; 4],
    preparations: BTreeMap<u32, VecDeque<LocalKey>>,
    spare_preparations: VecDeque<LocalKey>,
    geometry: Vec<LocalKey>,
    geometry_depths: BTreeMap<u32, VecDeque<LocalKey>>,
    spare_geometry: VecDeque<LocalKey>,
    geometry_pending: usize,
    compositions: VecDeque<LocalKey>,
    composition_pending: usize,
    required_layout: Vec<LocalKey>,
    failures: Vec<LocalKey>,
    attempts: usize,
    needs_audit: bool,
    pub(crate) hit_changed: bool,
    pub(crate) fault: Option<ElementId>,
    pub(crate) counts: WorkCounts,
}

impl Scheduler {
    pub(crate) fn new() -> Self {
        Self {
            rows: Vec::new(),
            stages: std::array::from_fn(|_| VecDeque::new()),
            preparations: BTreeMap::new(),
            spare_preparations: VecDeque::new(),
            geometry: Vec::new(),
            geometry_depths: BTreeMap::new(),
            spare_geometry: VecDeque::new(),
            geometry_pending: 0,
            compositions: VecDeque::new(),
            composition_pending: 0,
            required_layout: Vec::new(),
            failures: Vec::new(),
            attempts: 0,
            needs_audit: true,
            hit_changed: false,
            fault: None,
            counts: WorkCounts::default(),
        }
    }

    pub(crate) fn reserve_row(&mut self) {
        self.rows.reserve(1);
    }

    pub(crate) fn append(&mut self) {
        self.rows.push(Pending::default());
    }

    pub(crate) fn pending(&self, row: Row) -> Work {
        self.rows[row.0].work
    }

    pub(crate) fn consume(&mut self, row: Row, work: Work) {
        self.rows[row.0].work.remove(work);
    }

    pub(crate) fn recovery_required(&self, row: Row) -> bool {
        let record = &self.rows[row.0];
        record.failed_update || !record.failed.is_empty()
    }

    fn runnable(&self, row: Row, stage: Work) -> bool {
        let record = &self.rows[row.0];
        (!record.failed_update && !record.failed.contains(stage)) || record.retry.contains(stage)
    }

    pub(crate) fn update_failed(&self, row: Row) -> bool {
        self.rows[row.0].failed_update
    }

    pub(crate) fn failure(&self, row: Row) -> Work {
        if self.rows[row.0].failed_update {
            Work::all()
        } else {
            self.rows[row.0].failed
        }
    }

    pub(crate) fn failed_keys(&self) -> &[LocalKey] {
        &self.failures
    }

    pub(crate) fn required_keys(&self) -> &[LocalKey] {
        &self.required_layout
    }

    pub(crate) fn defer(&mut self, row: Row, work: Work) {
        self.rows[row.0].work |= work;
    }

    pub(crate) fn request(&mut self, key: LocalKey, row: Row, work: Work) {
        if work.is_empty() {
            return;
        }
        let missing = work & !self.rows[row.0].queued;
        let require_layout = work.contains(Work::LAYOUT) && !self.rows[row.0].layout_required;
        if require_layout {
            self.required_layout.reserve(1 + self.attempts);
        }
        for stage in Work::STAGES {
            if missing.contains(stage) {
                self.stages[stage.index()].reserve(1 + self.attempts);
            }
        }

        self.needs_audit |= work.intersects(Work::PREPARE | Work::LAYOUT);
        let record = &mut self.rows[row.0];
        record.work |= work;
        record.retry |= if record.failed_update {
            work
        } else {
            work & record.failed
        };
        record.layout_notify |= work.contains(Work::LAYOUT);
        if require_layout {
            record.layout_required = true;
            self.required_layout.push(key);
        }
        record.queued |= missing;
        self.counts.invalidations = self.counts.invalidations.saturating_add(1);
        for stage in Work::STAGES {
            if missing.contains(stage) {
                self.stages[stage.index()].push_back(key);
            }
        }
    }

    pub(crate) fn notify_layout(&mut self, key: LocalKey, row: Row) {
        if !self.rows[row.0].queued.contains(Work::LAYOUT) {
            self.stages[Work::LAYOUT.index()].reserve(1 + self.attempts);
        }

        let record = &mut self.rows[row.0];
        record.layout_notify = true;
        record.layout_explicit = true;
        if !record.queued.contains(Work::LAYOUT) {
            record.queued.insert(Work::LAYOUT);
            self.stages[Work::LAYOUT.index()].push_back(key);
        }
    }

    pub(crate) fn geometry_dirty(&mut self, key: LocalKey, row: Row) {
        if !self.rows[row.0].geometry_queued {
            self.geometry.reserve(1 + self.attempts);
        }

        let record = &mut self.rows[row.0];
        if !record.geometry_queued {
            record.geometry_queued = true;
            self.geometry_pending += 1;
            self.geometry.push(key);
        }
    }

    pub(crate) fn hit_dirty(&mut self) {
        self.hit_changed = true;
    }

    pub(crate) fn begin_geometry(&mut self, row: Row) {
        if std::mem::take(&mut self.rows[row.0].geometry_queued) {
            self.geometry_pending -= 1;
        }
    }

    pub(crate) fn has_geometry(&self) -> bool {
        self.geometry_pending != 0
    }

    pub(crate) fn has_layout(&self) -> bool {
        !self.stages[Work::LAYOUT.index()].is_empty()
    }

    fn next_geometry(&mut self, elements: &MountedElements) -> Option<LocalKey> {
        loop {
            while let Some(key) = self.geometry.pop() {
                let Some(row) = elements.row_local(key) else {
                    continue;
                };
                if !self.rows[row.0].geometry_queued {
                    continue;
                }
                if !elements.contains_local(key) {
                    self.begin_geometry(row);
                    continue;
                }
                let depth = elements.nodes[row.0].depth;
                if !self.geometry_depths.contains_key(&depth) {
                    self.geometry_depths
                        .insert(depth, std::mem::take(&mut self.spare_geometry));
                }
                self.geometry_depths
                    .get_mut(&depth)
                    .expect("the selected spatial depth bucket was inserted")
                    .push_back(key);
            }
            let (&depth, _) = self.geometry_depths.first_key_value()?;
            let queue = self
                .geometry_depths
                .get_mut(&depth)
                .expect("the first spatial depth bucket remains owned");
            let key = queue
                .pop_front()
                .expect("spatial depth buckets remain nonempty until detached");
            if queue.is_empty() {
                let queue = self
                    .geometry_depths
                    .remove(&depth)
                    .expect("the emptied spatial bucket remains indexed");
                if queue.capacity().saturating_mul(size_of::<LocalKey>()) <= 64 * 1024 {
                    self.spare_geometry = queue;
                }
            }
            let Some(row) = elements.row_local(key) else {
                continue;
            };
            if !self.rows[row.0].geometry_queued {
                continue;
            }
            if !elements.contains_local(key) {
                self.begin_geometry(row);
                continue;
            }
            return Some(key);
        }
    }

    fn composition_dirty(&mut self, key: LocalKey, row: Row, spatial: SceneSpatial) {
        if !spatial.changed() {
            return;
        }
        if !self.rows[row.0].composition_queued {
            self.compositions.reserve(1 + self.attempts);
        }

        let record = &mut self.rows[row.0];
        record.scene_spatial.merge(spatial);
        if !record.composition_queued {
            record.composition_queued = true;
            self.composition_pending += 1;
            self.compositions.push_back(key);
        }
    }

    fn next_composition(&mut self, elements: &MountedElements) -> Option<(LocalKey, Row)> {
        while let Some(key) = self.compositions.pop_front() {
            let Some(row) = elements.row_local(key) else {
                continue;
            };
            if !self.rows[row.0].composition_queued {
                continue;
            }
            if !elements.contains_local(key) {
                self.begin_composition(row);
                continue;
            }
            return Some((key, row));
        }
        None
    }

    fn begin_composition(&mut self, row: Row) -> SceneSpatial {
        let record = &mut self.rows[row.0];
        if std::mem::take(&mut record.composition_queued) {
            self.composition_pending -= 1;
        }
        std::mem::take(&mut record.scene_spatial)
    }

    pub(crate) fn next_stage(
        &mut self,
        stage: Work,
        elements: &MountedElements,
    ) -> Option<(LocalKey, Row)> {
        while let Some(key) = self.stages[stage.index()].pop_front() {
            let Some(row) = elements.row_local(key) else {
                continue;
            };
            let record = &mut self.rows[row.0];
            record.queued.remove(stage);
            if !elements.contains_local(key) || !record.work.contains(stage) {
                continue;
            }
            record.work.remove(stage);
            return Some((key, row));
        }
        None
    }

    pub(crate) fn next_prepare(&mut self, elements: &MountedElements) -> Option<(LocalKey, Row)> {
        loop {
            while let Some(key) = self.stages[Work::PREPARE.index()].pop_front() {
                let Some(row) = elements.row_local(key) else {
                    continue;
                };
                if !elements.contains_local(key) || !self.rows[row.0].work.contains(Work::PREPARE) {
                    self.rows[row.0].queued.remove(Work::PREPARE);
                    continue;
                }
                let depth = elements
                    .depth(key)
                    .expect("queued live preparations have a topology depth");
                if !self.preparations.contains_key(&depth) {
                    self.preparations
                        .insert(depth, std::mem::take(&mut self.spare_preparations));
                }
                self.preparations
                    .get_mut(&depth)
                    .expect("the selected depth bucket was inserted")
                    .push_back(key);
                self.counts.prepare_ordered = self.counts.prepare_ordered.saturating_add(1);
            }
            let (&depth, _) = self.preparations.first_key_value()?;
            let queue = self
                .preparations
                .get_mut(&depth)
                .expect("first depth bucket remains owned");
            let key = queue
                .pop_front()
                .expect("preparation buckets are nonempty until detached");
            if queue.is_empty() {
                let queue = self
                    .preparations
                    .remove(&depth)
                    .expect("the emptied bucket remains indexed");
                if queue.capacity().saturating_mul(size_of::<LocalKey>()) <= 64 * 1024 {
                    self.spare_preparations = queue;
                }
            }
            let Some(row) = elements.row_local(key) else {
                continue;
            };
            self.rows[row.0].queued.remove(Work::PREPARE);
            if !elements.contains_local(key) || !self.rows[row.0].work.contains(Work::PREPARE) {
                continue;
            }
            return Some((key, row));
        }
    }

    pub(crate) fn next_layout_note(
        &mut self,
        elements: &MountedElements,
    ) -> Option<(LocalKey, Row)> {
        while let Some(key) = self.stages[Work::LAYOUT.index()].pop_front() {
            let Some(row) = elements.row_local(key) else {
                continue;
            };
            let record = &mut self.rows[row.0];
            record.queued.remove(Work::LAYOUT);
            if !elements.contains_local(key) {
                continue;
            }
            if !record.work.contains(Work::LAYOUT) && !record.layout_explicit {
                record.layout_notify = false;
                continue;
            }
            if record.layout_notify {
                return Some((key, row));
            }
        }
        None
    }

    fn begin_layout_note(&mut self, row: Row) {
        self.rows[row.0].layout_notify = false;
        self.rows[row.0].layout_explicit = false;
    }

    fn return_ticket(&mut self, key: LocalKey, row: Row, stage: Work) {
        if !self.rows[row.0].queued.contains(stage) {
            self.stages[stage.index()].reserve(1 + self.attempts);
        }

        let record = &mut self.rows[row.0];
        if !record.queued.contains(stage) {
            record.queued.insert(stage);
            self.stages[stage.index()].push_front(key);
        }
    }

    fn return_stage(&mut self, key: LocalKey, row: Row, stage: Work) {
        self.rows[row.0].work.insert(stage);
        self.return_ticket(key, row, stage);
    }

    fn require_layout(&mut self, key: LocalKey, row: Row) {
        if !self.rows[row.0].layout_required {
            self.required_layout.reserve(1 + self.attempts);

            self.rows[row.0].layout_required = true;
            self.required_layout.push(key);
        }
    }

    pub(crate) fn require_viewport_layout(
        &mut self,
        elements: &MountedElements,
        layout: &crate::layout::Layout,
    ) {
        for (index, node) in elements.nodes.iter().enumerate() {
            if node.life == mounted::Life::Live && layout.records[index].demand {
                self.require_layout(node.key, Row(index));
            }
        }
        self.needs_audit = true;
    }

    pub(crate) fn begin_attempt(&mut self, row: Row, kind: AttemptKind) -> bool {
        self.needs_audit |= matches!(
            kind,
            AttemptKind::Update | AttemptKind::Prepare | AttemptKind::Layout
        );
        let recovery = self.recovery_required(row);
        let attempts = self.attempts + 1;
        self.failures.reserve(attempts);
        self.geometry.reserve(attempts);
        self.compositions.reserve(attempts);
        self.required_layout.reserve(attempts);
        for queue in &mut self.stages {
            queue.reserve(attempts);
        }
        self.attempts = attempts;
        match kind {
            AttemptKind::Update => {}
            AttemptKind::Prepare => {
                self.rows[row.0].work.remove(Work::PREPARE);
                self.rows[row.0].retry.remove(Work::PREPARE);
            }
            AttemptKind::Layout => {
                self.rows[row.0].work.remove(Work::LAYOUT);
                self.rows[row.0].retry.remove(Work::LAYOUT);
            }
            AttemptKind::Scene => {
                self.rows[row.0].work.remove(Work::SCENE);
                self.rows[row.0].retry.remove(Work::SCENE);
            }
            AttemptKind::Semantics => {
                self.rows[row.0].work.remove(Work::SEMANTICS);
                self.rows[row.0].retry.remove(Work::SEMANTICS);
            }
        }
        recovery
    }

    pub(crate) fn suspend_attempt(&mut self) {
        self.attempts -= 1;
    }

    pub(crate) fn finish_attempt(&mut self, row: Row, kind: AttemptKind) {
        match kind {
            AttemptKind::Update => self.rows[row.0].failed_update = false,
            AttemptKind::Prepare => self.rows[row.0].failed.remove(Work::PREPARE),
            AttemptKind::Layout => self.rows[row.0].failed.remove(Work::LAYOUT),
            AttemptKind::Scene => self.rows[row.0].failed.remove(Work::SCENE),
            AttemptKind::Semantics => self.rows[row.0].failed.remove(Work::SEMANTICS),
        }
        self.attempts -= 1;
    }

    pub(crate) fn fail_attempt(&mut self, key: LocalKey, row: Row, kind: AttemptKind) {
        self.needs_audit = true;
        // This attempt uses its reserved slots; normal pushes still leave the other guards' slots free.
        self.attempts -= 1;
        let retry = self.rows[row.0].retry;
        if !self.recovery_required(row) {
            self.failures.reserve(1 + self.attempts);
            self.failures.push(key);
        }
        if matches!(
            kind,
            AttemptKind::Update | AttemptKind::Prepare | AttemptKind::Layout
        ) {
            self.require_layout(key, row);
        }
        match kind {
            AttemptKind::Update => self.rows[row.0].failed_update = true,
            AttemptKind::Prepare => {
                self.rows[row.0].failed.insert(Work::PREPARE);
                self.request(key, row, Work::PREPARE);
            }
            AttemptKind::Layout => {
                let notification = self.rows[row.0].layout_notify;
                self.rows[row.0].failed.insert(Work::LAYOUT);
                self.request(key, row, Work::LAYOUT);
                self.rows[row.0].layout_notify = notification;
            }
            AttemptKind::Scene => {
                self.rows[row.0].failed.insert(Work::SCENE);
                self.request(key, row, Work::SCENE);
            }
            AttemptKind::Semantics => {
                self.rows[row.0].failed.insert(Work::SEMANTICS);
                self.request(key, row, Work::SEMANTICS);
            }
        }
        // Work created by the callback remains dirty, but a failed Update cannot authorize its own retry.
        self.rows[row.0].retry = if kind == AttemptKind::Update {
            Work::empty()
        } else {
            retry
        };
    }

    pub(crate) fn render_state(&self, row: Row) -> RenderState {
        self.rows[row.0].render
    }

    pub(crate) fn scene_dependencies(&self, row: Row) -> SceneDeps {
        self.rows[row.0].scene_deps
    }

    pub(crate) fn set_render(&mut self, row: Row, state: RenderState, deps: SceneDeps) {
        self.rows[row.0].render = state;
        self.rows[row.0].scene_deps = deps;
    }

    pub(crate) fn has_prepare(&self) -> bool {
        !self.preparations.is_empty() || !self.stages[Work::PREPARE.index()].is_empty()
    }

    pub(crate) fn has_tickets(&self) -> bool {
        self.needs_audit
            || self.fault.is_some()
            || self.has_prepare()
            || self.stages.iter().any(|queue| !queue.is_empty())
            || self.geometry_pending != 0
            || self.composition_pending != 0
    }

    pub(crate) fn remove_row(&mut self, row: Row) {
        self.needs_audit = true;
        let removed = self.rows.swap_remove(row.0);
        if removed.geometry_queued {
            self.geometry_pending -= 1;
        }
        if removed.composition_queued {
            self.composition_pending -= 1;
        }
    }

    pub(crate) fn trim(&mut self, elements: &MountedElements, layout: &crate::layout::Layout) {
        let rows = &mut self.rows;
        self.required_layout.retain(|key| {
            let Some(row) = elements.row_local(*key) else {
                return false;
            };
            let record = &mut rows[row.0];
            let measurement = &layout.records[row.0];
            let current = measurement
                .measurement
                .is_some_and(|key| key.viewport_epoch == layout.viewport_epoch());
            let keep = record.failed_update
                || record.failed.contains(Work::LAYOUT)
                || (measurement.demand && (record.work.contains(Work::LAYOUT) || !current));
            record.layout_required = keep;
            keep
        });
        self.failures.retain(|key| {
            elements.row_local(*key).is_some_and(|row| {
                let record = &self.rows[row.0];
                record.failed_update || !record.failed.is_empty()
            })
        });
        for queue in &mut self.stages {
            trim_ticket_queue(queue);
        }
        trim_ticket_queue(&mut self.compositions);
        trim_key_buffer(&mut self.geometry);
        trim_key_buffer(&mut self.required_layout);
        trim_key_buffer(&mut self.failures);
        if self.rows.capacity() > self.rows.len().saturating_mul(4).max(16)
            && self.rows.capacity().saturating_mul(size_of::<Pending>()) > 64 * 1024
        {
            self.rows
                .shrink_to(self.rows.len().saturating_mul(2).max(16));
        }
    }
}

fn trim_key_buffer(keys: &mut Vec<LocalKey>) {
    let demand = keys.len().max(16);
    if keys.capacity() > demand.saturating_mul(4)
        && keys.capacity().saturating_mul(size_of::<LocalKey>()) > 64 * 1024
    {
        keys.shrink_to(demand.saturating_mul(2));
    }
}

fn trim_ticket_queue(queue: &mut VecDeque<LocalKey>) {
    let demand = queue.len().max(16);
    if queue.capacity() > demand.saturating_mul(4)
        && queue.capacity().saturating_mul(size_of::<LocalKey>()) > 64 * 1024
    {
        queue.shrink_to(demand.saturating_mul(2));
    }
}

struct Budget {
    remaining: Option<usize>,
}

impl Budget {
    fn new(budget: ProcessBudget) -> Self {
        Self {
            remaining: match budget {
                ProcessBudget::Unlimited => None,
                ProcessBudget::Steps(steps) => Some(steps),
            },
        }
    }

    fn admit(&mut self, window: &mut WindowState, phase: ProcessPhase) -> bool {
        window.progress.phase = phase;
        if let Some(remaining) = &mut self.remaining {
            if *remaining == 0 {
                return false;
            }
            *remaining -= 1;
        }
        if phase != ProcessPhase::Configuration {
            clear_ancestry(window);
        }
        window.work.counts.process_units = window.work.counts.process_units.saturating_add(1);
        true
    }
}

fn begin_mutable(window: &mut WindowState) {
    debug_assert!(
        !window.input.has_actions(),
        "owned Actions must drain before mutable framework phases"
    );

    window.progress.effects_ready = false;
    window.progress.frontier_ready = false;
    window.progress.order_current = false;
    window.progress.hit_current = false;
    window.progress.decoration_current = false;
    window.progress.scene_order_current = false;
    window.progress.retirement_fresh = false;
}

fn configuration_pending(window: &WindowState, key: LocalKey, row: Row) -> bool {
    !window.configuration.checked(row)
        || (window.elements.contains_local(key)
            && window.configuration.needs_delivery(key)
            && !window.work.update_failed(row))
}

fn prepare_pending(window: &WindowState, key: LocalKey, row: Row) -> bool {
    window.elements.contains_local(key)
        && window.work.pending(row).contains(Work::PREPARE)
        && window.work.runnable(row, Work::PREPARE)
        && !layout::suppressed(&window.elements, &window.configuration, key)
}

pub(crate) fn clear_ancestry(window: &mut WindowState) {
    if let Some(cursor) = window.progress.ancestry.take() {
        window.configuration.return_path(cursor.path);
    }
}

fn configuration_refresh_callback_free(window: &WindowState, key: LocalKey, row: Row) -> bool {
    if window.work.update_failed(row) || !window.elements.contains_local(key) {
        return true;
    }
    if window.configuration.needs_delivery(key) {
        return false;
    }

    let parent = window
        .elements
        .parent(key)
        .and_then(|parent| window.elements.row_local(parent))
        .map(|row| window.configuration.output(row))
        .unwrap_or_default();
    window.configuration.input_current(row, &parent)
}

fn unchecked_ancestor(window: &mut WindowState, target: LocalKey) -> Option<LocalKey> {
    if let Some(row) = window.elements.row_local(target)
        && window.configuration.checked(row)
        && !window.configuration.needs_delivery(target)
        && !window.work.update_failed(row)
    {
        return None;
    }

    if let Some(mut cursor) = window.progress.ancestry.take() {
        let current = cursor.target == target
            && cursor.epoch == window.configuration.revision()
            && window.elements.contains_local(target)
            && cursor.refreshed.is_none_or(|key| {
                window
                    .elements
                    .row_local(key)
                    .is_some_and(|row| !configuration_pending(window, key, row))
            });
        if current {
            while let Some(&key) = cursor.path.last() {
                let Some(row) = window.elements.row_local(key) else {
                    break;
                };
                if configuration_pending(window, key, row) {
                    window.progress.ancestry = Some(cursor);
                    return Some(key);
                }
                cursor.path.pop();
            }
            let complete = cursor.path.is_empty();
            window.configuration.return_path(cursor.path);
            if complete {
                return None;
            }
        } else {
            window.configuration.return_path(cursor.path);
        }
    }

    let mut current = Some(target);
    let mut first = None;
    while let Some(key) = current {
        let row = window.elements.row_local(key)?;
        if configuration_pending(window, key, row) {
            first = Some(key);
        }
        current = window.elements.parent(key);
    }
    let first = first?;
    // Fresh, shallow, and callback-bearing refreshes keep the allocation-free scanner.
    if window.elements.depth(target).is_none_or(|depth| depth < 8)
        || !window.elements.contains_local(target)
    {
        return Some(first);
    }
    let first_row = window
        .elements
        .row_local(first)
        .expect("an unchecked ancestor retains its identity row");
    if !configuration_refresh_callback_free(window, first, first_row) {
        return Some(first);
    }

    let mut path = window.configuration.take_path();
    let mut current = Some(target);
    while let Some(key) = current {
        path.push(key);
        if key == first {
            break;
        }
        current = window.elements.parent(key);
    }
    window.progress.ancestry = Some(AncestryCursor {
        target,
        epoch: window.configuration.revision(),
        path,
        refreshed: None,
    });
    Some(first)
}

fn push_configuration_children(window: &mut WindowState, key: LocalKey) {
    let Some(row) = window.elements.row_local(key) else {
        return;
    };
    if let Some(index) = window.elements.nodes[row.0].children {
        window.progress.config_stack.extend(
            window.elements.children[index.get() - 1]
                .ordered
                .iter()
                .rev()
                .map(|child| child.key),
        );
    }
}

fn next_configuration(window: &mut WindowState) -> Option<LocalKey> {
    loop {
        if window.progress.config_stack.is_empty() {
            window
                .progress
                .config_stack
                .push(window.configuration.next_root()?);
        }
        let key = *window
            .progress
            .config_stack
            .last()
            .expect("a configuration visit owns its continuation key");
        let Some(row) = window.elements.row_local(key) else {
            window.progress.config_stack.pop();
            continue;
        };
        if configuration_pending(window, key, row) {
            return Some(key);
        }
        let walk = window.configuration.begin_root(row);
        window.progress.config_stack.pop();
        window.work.counts.configuration_visits =
            window.work.counts.configuration_visits.saturating_add(1);
        if walk {
            push_configuration_children(window, key);
        }
        if prepare_pending(window, key, row) {
            window.work.return_ticket(key, row, Work::PREPARE);
            return None;
        }
    }
}

fn refresh_configuration(
    window: &mut WindowState,
    key: LocalKey,
    from_stack: bool,
) -> Result<(), ElementError> {
    let Some(row) = window.elements.row_local(key) else {
        clear_ancestry(window);
        return Err(ElementError::Missing(window.elements.id(key)));
    };
    let walk = window.configuration.begin_root(row);
    let id = window.elements.id(key);
    let delivery = if window.work.update_failed(row) || !window.elements.contains_local(key) {
        ConfigDelivery::Defer
    } else {
        ConfigDelivery::Deliver
    };
    let retry = window.work.rows[row.0].retry;
    let callback_free = configuration_refresh_callback_free(window, key, row);
    let mut ancestry = window.progress.ancestry.take();
    let continuation = ancestry.as_ref().is_some_and(|cursor| {
        cursor.path.last().copied() == Some(key)
            && cursor.epoch == window.configuration.revision()
            && window.elements.contains_local(cursor.target)
    });
    // Epoch equality cannot license crossing an Element callback: it may alter
    // delivery, recovery, or children without issuing a configuration epoch.
    if (!continuation || !callback_free)
        && let Some(cursor) = ancestry.take()
    {
        window.configuration.return_path(cursor.path);
    }
    begin_mutable(window);

    if let Err(error) = mounted::refresh_current(&mut window.access(), id, delivery) {
        if let Some(cursor) = ancestry.take() {
            window.configuration.return_path(cursor.path);
        }
        return Err(error);
    }

    let row = window
        .elements
        .row(id)
        .expect("configuration callbacks retain their own identity");
    if delivery == ConfigDelivery::Defer {
        // Pure freshness must not turn an old failure into an authorized retry.
        window.work.rows[row.0].retry = retry;
    }
    let walk = window.configuration.begin_root(row) || walk;
    if from_stack {
        let current = window.progress.config_stack.pop();
        debug_assert_eq!(current, Some(key));
    }
    if walk {
        push_configuration_children(window, key);
    }
    window.work.counts.configuration_visits =
        window.work.counts.configuration_visits.saturating_add(1);
    if prepare_pending(window, key, row) {
        window.work.return_ticket(key, row, Work::PREPARE);
    }
    if let Some(mut cursor) = ancestry {
        if cursor.epoch == window.configuration.revision()
            && window.elements.contains_local(cursor.target)
            && !configuration_pending(window, key, row)
        {
            cursor.path.pop();
            cursor.refreshed = Some(key);
            if cursor.path.is_empty() {
                window.configuration.return_path(cursor.path);
            } else {
                window.progress.ancestry = Some(cursor);
            }
        } else {
            window.configuration.return_path(cursor.path);
        }
    }
    Ok(())
}

fn retiring_environment_pending(window: &WindowState) -> bool {
    for &key in &window.elements.retired {
        if window
            .elements
            .row_local(key)
            .is_some_and(|row| !window.configuration.checked(row))
        {
            return true;
        }
    }
    false
}

fn refresh_retiring_configuration(window: &mut WindowState) -> Result<(), ElementError> {
    clear_ancestry(window);
    let mut keys = window.elements.retired.clone();
    keys.sort_unstable_by_key(|key| window.elements.depth(*key).unwrap_or(0));

    for key in keys {
        if window.elements.row_local(key).is_none() {
            continue;
        }
        let mut retries = Vec::new();
        let mut current = Some(key);
        while let Some(key) = current {
            let row = window
                .elements
                .row_local(key)
                .expect("retiring environment ancestry retains its identity");
            if window.configuration.checked(row) {
                break;
            }
            retries.push((key, window.work.rows[row.0].retry));
            current = window.elements.parent(key);
        }
        if retries.is_empty() {
            continue;
        }
        let id = window.elements.id(key);

        mounted::ensure_environment(&mut window.access(), id)?;

        for (key, retry) in retries {
            let row = window
                .elements
                .row_local(key)
                .expect("pure environment refresh retains every identity");
            window.work.rows[row.0].retry = retry;
            if window.elements.contains_local(key)
                && !window.work.update_failed(row)
                && window.configuration.needs_delivery(key)
            {
                window.progress.config_stack.push(key);
            }
            window.work.counts.configuration_visits =
                window.work.counts.configuration_visits.saturating_add(1);
        }
    }
    Ok(())
}

fn parked_ancestor(window: &WindowState, key: LocalKey) -> bool {
    let mut current = Some(key);
    while let Some(key) = current {
        let Some(row) = window.elements.row_local(key) else {
            return true;
        };
        if window.layout.records[row.0].parked {
            return true;
        }
        current = window.elements.parent(key);
    }
    false
}

fn demanded(window: &WindowState, key: LocalKey, row: Row) -> bool {
    (key == window.root.local() || window.layout.records[row.0].demand)
        && !parked_ancestor(window, key)
        && !layout::suppressed(&window.elements, &window.configuration, key)
}

fn layout_notification_demanded(window: &WindowState, key: LocalKey, row: Row) -> bool {
    let parent = window.elements.parent(key);
    let ordinary = (key == window.root.local() || window.layout.records[row.0].demand)
        && !parked_ancestor(window, key);
    (ordinary
        || (window.layout.has_visibility_use(key)
            && parent.is_none_or(|parent| !parked_ancestor(window, parent))))
        && parent.is_none_or(|parent| {
            !layout::suppressed(&window.elements, &window.configuration, parent)
        })
}

fn check_obligations(window: &mut WindowState) -> Result<(), ElementError> {
    if !window.work.needs_audit {
        return Ok(());
    }

    window.work.counts.obligation_audits = window.work.counts.obligation_audits.saturating_add(1);
    audit_obligations(window)?;

    window.work.needs_audit = false;
    Ok(())
}

fn audit_obligations(window: &WindowState) -> Result<(), ElementError> {
    if let Some(id) = window.work.fault {
        return Err(ElementError::UnresolvedWork {
            id,
            work: Work::all(),
        });
    }
    for &key in window.work.failed_keys() {
        let Some(row) = window.elements.row_local(key) else {
            continue;
        };
        if !window.elements.contains_local(key)
            || layout::suppressed(&window.elements, &window.configuration, key)
        {
            continue;
        }
        let work = window.work.failure(row);
        if !work.is_empty() {
            return Err(ElementError::UnresolvedWork {
                id: window.elements.id(key),
                work,
            });
        }
    }
    for &key in window.work.required_keys() {
        let Some(row) = window.elements.row_local(key) else {
            continue;
        };
        if !window.elements.contains_local(key) || !demanded(window, key, row) {
            continue;
        }
        let record = &window.layout.records[row.0];
        let current = record
            .measurement
            .is_some_and(|key| key.viewport_epoch == window.layout.viewport_epoch());
        if window.work.pending(row).contains(Work::LAYOUT) || !current {
            return Err(ElementError::UnresolvedWork {
                id: window.elements.id(key),
                work: Work::LAYOUT,
            });
        }
    }
    Ok(())
}

fn save_cleanup_geometry(window: &mut WindowState) {
    if !window.progress.cleanup_saved {
        window
            .input
            .prepare_cleanup_geometry(&window.elements, &window.layout);
        window.progress.cleanup_saved = true;
    }
}

fn project(window: &mut WindowState, key: LocalKey) -> Result<(), ElementError> {
    let row = window
        .elements
        .row_local(key)
        .ok_or_else(|| ElementError::Missing(window.elements.id(key)))?;
    let record = &window.layout.records[row.0];
    let bounds = record.geometry.bounds;
    let clip = record.geometry.clip;
    let exact_clip = window.layout.clip_bounds(key, row);
    let participating = record.participating;
    let visible = record.visible;
    let enabled = record.enabled;
    let collapsed = record.published_collapsed;
    let world_set = record.world_set;

    save_cleanup_geometry(window);
    layout::project_one(
        &window.elements,
        &window.configuration,
        &mut window.layout,
        &mut window.work,
        key,
    )?;

    let record = &window.layout.records[row.0];
    let spatial = SceneSpatial {
        bounds: !world_set || bounds != record.geometry.bounds,
        clip: !world_set
            || clip != record.geometry.clip
            || exact_clip != window.layout.clip_bounds(key, row),
        qualification: participating != record.participating
            || visible != record.visible
            || enabled != record.enabled,
    };
    window.work.composition_dirty(key, row, spatial);
    if collapsed && !record.published_collapsed {
        // The collapsed interval may have parked a propagation tail without changing its values.
        window.configuration.queue_descendants(row, key);
        if prepare_pending(window, key, row) {
            window.work.return_ticket(key, row, Work::PREPARE);
        }
    }

    window.progress.spatial_pending = true;
    window.progress.hit_current = false;
    window.progress.decoration_current = false;
    Ok(())
}

fn interaction_callbacks_pending(window: &WindowState) -> bool {
    let invalid = |key| {
        !window.input.window_focused
            || !crate::input::qualified(&window.elements, &window.layout, key)
    };
    window.input.focus.is_some_and(invalid)
        || window.input.pointers.values().any(|pointer| {
            pointer.hover.is_some_and(invalid) || pointer.capture.is_some_and(invalid)
        })
        || (window.input.rehit_pending
            && window.input.window_focused
            && !window.input.closing
            && window.input.pointers.values().any(|pointer| pointer.inside))
}

fn remove_retired_scene(window: &mut WindowState) {
    for &key in &window.elements.retired {
        let removed = window.scene.remove(window.elements.id(key));
        window.progress.scene_changed |= removed;
        if removed {
            window.work.counts.scene_order_changes =
                window.work.counts.scene_order_changes.saturating_add(1);
        }
    }
}

fn output_visible(window: &WindowState, row: Row) -> bool {
    window.layout.records[row.0].visible && window.layout.geometry(row).is_some()
}

struct OutputAttempt<'a> {
    work: &'a mut Scheduler,
    key: LocalKey,
    row: Row,
    kind: AttemptKind,
    complete: bool,
}

impl<'a> OutputAttempt<'a> {
    fn enter(work: &'a mut Scheduler, key: LocalKey, row: Row, kind: AttemptKind) -> Self {
        work.begin_attempt(row, kind);
        Self {
            work,
            key,
            row,
            kind,
            complete: false,
        }
    }

    fn finish(&mut self) {
        self.work.finish_attempt(self.row, self.kind);
        self.complete = true;
    }
}

impl Drop for OutputAttempt<'_> {
    fn drop(&mut self) {
        if !self.complete {
            self.work.fail_attempt(self.key, self.row, self.kind);
        }
    }
}

fn semantics(window: &mut WindowState, key: LocalKey, row: Row) -> Result<(), ElementError> {
    let id = window.elements.id(key);
    let mut attempt = OutputAttempt::enter(&mut window.work, key, row, AttemptKind::Semantics);
    let body = window
        .elements
        .body(key)
        .ok_or(ElementError::Reentrant(id))?;
    let cx = SemanticCx::new(id, window.configuration.read(row));
    let content = body.semantics(&cx);
    attempt.work.counts.semantic_updates = attempt.work.counts.semantic_updates.saturating_add(1);

    window.input.publish_semantic(
        id,
        content,
        &window.elements,
        &window.configuration,
        &window.layout,
    )?;

    attempt.finish();
    Ok(())
}

fn record_scene(window: &mut WindowState, key: LocalKey, row: Row) -> Result<(), ElementError> {
    let id = window.elements.id(key);
    let geometry = window
        .layout
        .geometry(row)
        .ok_or(ElementError::UnresolvedWork {
            id,
            work: Work::LAYOUT,
        })?;
    let exact_clip = window.layout.clip_bounds(key, row);
    let mut attempt = OutputAttempt::enter(&mut window.work, key, row, AttemptKind::Scene);
    let mut primitives = window.scene.take_primitives(id);
    primitives.clear();
    let dependencies = {
        let body = window
            .elements
            .body(key)
            .ok_or(ElementError::Reentrant(id))?;
        let mut cx = SceneCx::new(
            id,
            geometry.bounds,
            geometry.clip,
            exact_clip,
            window.configuration.read(row),
            &mut primitives,
        );
        body.scene(&mut cx)?;
        cx.dependencies()
    };

    attempt.work.counts.scenes = attempt.work.counts.scenes.saturating_add(1);
    let rank = window.elements.rank(key).ok_or(ElementError::Missing(id))?;
    let change = window.scene.publish_fragment(
        Fragment {
            element: id,
            bounds: geometry.bounds,
            clip: geometry.clip,
            primitives,
        },
        exact_clip,
        rank,
        true,
    )?;

    let state = match change.disposition {
        SceneDisposition::Recorded => RenderState::Recorded,
        SceneDisposition::Empty => RenderState::Empty,
        SceneDisposition::Culled => RenderState::Released,
    };
    attempt.work.set_render(row, state, dependencies);
    if change.membership_changed {
        attempt.work.counts.scene_order_changes =
            attempt.work.counts.scene_order_changes.saturating_add(1);
    }
    window.progress.scene_changed = true;
    attempt.finish();
    Ok(())
}

fn compose_scene(window: &mut WindowState, key: LocalKey, row: Row) -> Result<(), ElementError> {
    let spatial = window.work.begin_composition(row);
    let id = window.elements.id(key);
    let state = window.work.render_state(row);
    let dependencies = window.work.scene_dependencies(row);
    window.work.counts.scene_compositions = window.work.counts.scene_compositions.saturating_add(1);
    if !output_visible(window, row) || window.layout.clip_bounds(key, row).is_empty() {
        let removed = window.scene.remove(id);
        window.progress.scene_changed |= removed;
        if state == RenderState::Recorded {
            window
                .work
                .set_render(row, RenderState::Released, dependencies);
        }
        if spatial.observed_by(dependencies) {
            window.work.defer(row, Work::SCENE);
        }
        return Ok(());
    }
    if window.work.pending(row).contains(Work::SCENE) {
        window.work.return_ticket(key, row, Work::SCENE);
        return Ok(());
    }
    if matches!(state, RenderState::Unrecorded | RenderState::Released)
        || spatial.observed_by(dependencies)
    {
        window.work.request(key, row, Work::SCENE);
        return Ok(());
    }
    if state == RenderState::Recorded && (spatial.bounds || spatial.clip) {
        let geometry = window
            .layout
            .geometry(row)
            .ok_or(ElementError::UnresolvedWork {
                id,
                work: Work::LAYOUT,
            })?;
        let exact_clip = window.layout.clip_bounds(key, row);
        let change = window
            .scene
            .move_fragment(id, geometry.bounds, geometry.clip, exact_clip)?;
        let state = match change.disposition {
            SceneDisposition::Recorded => RenderState::Recorded,
            SceneDisposition::Empty => RenderState::Empty,
            SceneDisposition::Culled => RenderState::Released,
        };
        window.work.set_render(row, state, dependencies);
        window.progress.scene_changed = true;
        if change.membership_changed {
            window.work.counts.scene_order_changes =
                window.work.counts.scene_order_changes.saturating_add(1);
        }
    }
    Ok(())
}

/// Drives concrete subsystem prerequisites without publishing a partial epoch.
///
/// Each admitted unit owns one request, Action, configuration delivery, prepare,
/// local measurement, or output operation. Traversal/projection consumers also use
/// units. Synchronous nested child work and finite detached cleanup/retirement batches
/// may exceed the cooperative budget; returning focus feedback always returns to the
/// budget gate. Exhausting exactly the necessary units still reports settlement.
pub(crate) fn drive(window: &mut WindowState, budget: ProcessBudget) -> Result<bool, ElementError> {
    let mut budget = Budget::new(budget);
    loop {
        if let Some(id) = window.work.fault {
            return Err(ElementError::UnresolvedWork {
                id,
                work: Work::all(),
            });
        }

        if window.input.has_actions() {
            window.progress.effects_ready = true;
        }
        if window.progress.effects_ready && window.input.has_effects() {
            if !budget.admit(window, ProcessPhase::Effects) {
                return Ok(false);
            }
            let requests = route::apply_requests(
                &mut window.elements,
                &window.configuration,
                &mut window.layout,
                &mut window.work,
                &mut window.input,
            )?;
            if requests != 0 {
                window.work.counts.requests =
                    window.work.counts.requests.saturating_add(requests as u64);
            } else if let Some(action) = window.input.actions_mut().pop_front() {
                view::run_action(&mut window.access(), action)?;
            }
            continue;
        }

        if window.progress.effects_ready
            && window.progress.frontier_ready
            && window.input.has_deferred_focus()
        {
            if !budget.admit(window, ProcessPhase::Effects) {
                return Ok(false);
            }
            route::resolve_deferred_focus(
                &mut window.elements,
                &window.configuration,
                &mut window.layout,
                &mut window.work,
                &mut window.input,
            )?;
            window.work.counts.requests = window.work.counts.requests.saturating_add(1);
            continue;
        }

        if let Some((key, row)) = window.work.next_prepare(&window.elements) {
            if layout::suppressed(&window.elements, &window.configuration, key)
                || !window.work.runnable(row, Work::PREPARE)
            {
                continue;
            }
            if let Some(ancestor) = unchecked_ancestor(window, key) {
                window.work.return_ticket(key, row, Work::PREPARE);
                if !budget.admit(window, ProcessPhase::Configuration) {
                    return Ok(false);
                }
                refresh_configuration(window, ancestor, false)?;
            } else {
                if !budget.admit(window, ProcessPhase::Prepare) {
                    window.work.return_ticket(key, row, Work::PREPARE);
                    return Ok(false);
                }
                begin_mutable(window);
                let id = window.elements.id(key);
                mounted::prepare(&mut window.access(), id)?;
            }
            continue;
        }

        let configuration = next_configuration(window);
        if window.work.has_prepare() {
            continue;
        }
        if let Some(key) = configuration {
            let row = window
                .elements
                .row_local(key)
                .expect("a selected configuration visit remains allocated");
            if prepare_pending(window, key, row) {
                window.work.return_ticket(key, row, Work::PREPARE);
                continue;
            }
            let ancestor = unchecked_ancestor(window, key).unwrap_or(key);
            if !budget.admit(window, ProcessPhase::Configuration) {
                return Ok(false);
            }
            refresh_configuration(window, ancestor, ancestor == key)?;
            continue;
        }

        if let Some((key, row)) = window.work.next_layout_note(&window.elements) {
            let parent = window
                .elements
                .parent(key)
                .and_then(|parent| window.elements.row_local(parent));
            if key == window.root.local()
                || !layout_notification_demanded(window, key, row)
                || !window.work.runnable(row, Work::LAYOUT)
                || parent.is_some_and(|row| window.work.pending(row).contains(Work::LAYOUT))
            {
                window.work.begin_layout_note(row);
                continue;
            }
            if let Some(ancestor) = unchecked_ancestor(window, key) {
                window.work.return_ticket(key, row, Work::LAYOUT);
                if !budget.admit(window, ProcessPhase::Configuration) {
                    return Ok(false);
                }
                refresh_configuration(window, ancestor, false)?;
                continue;
            }
            if !budget.admit(window, ProcessPhase::Layout) {
                window.work.return_ticket(key, row, Work::LAYOUT);
                return Ok(false);
            }
            begin_mutable(window);
            window.work.begin_layout_note(row);
            layout::propagate_one(&mut window.access(), key, row)?;
            continue;
        }
        if let Some(row) = window.elements.row(window.root)
            && window.work.pending(row).contains(Work::LAYOUT)
            && window.work.runnable(row, Work::LAYOUT)
            && !layout::suppressed(&window.elements, &window.configuration, window.root.local())
        {
            let root = window.root.local();
            if let Some(ancestor) = unchecked_ancestor(window, root) {
                if !budget.admit(window, ProcessPhase::Configuration) {
                    return Ok(false);
                }
                refresh_configuration(window, ancestor, false)?;
                continue;
            }
            if !budget.admit(window, ProcessPhase::Layout) {
                return Ok(false);
            }
            begin_mutable(window);
            let root = window.root;
            let constraints = Constraints::tight(window.size);
            layout::measure(&mut window.access(), root, constraints, MeasureMode::Eager)?;
            continue;
        }

        if !window.elements.retired.is_empty() && !window.progress.retirement_fresh {
            if retiring_environment_pending(window) {
                if !budget.admit(window, ProcessPhase::Configuration) {
                    return Ok(false);
                }
                refresh_retiring_configuration(window)?;
                window.progress.retirement_fresh = true;
                continue;
            }
            window.progress.retirement_fresh = true;
        }
        check_obligations(window)?;

        if window.elements.order_from.is_some() && !window.progress.order_current {
            if !budget.admit(window, ProcessPhase::Spatial) {
                return Ok(false);
            }
            begin_mutable(window);
            let visits = window.elements.rebuild_order(window.root.local());
            window.work.counts.tree_order_visits =
                window.work.counts.tree_order_visits.saturating_add(visits);
            window.progress.order_current = true;
            window.progress.spatial_pending = true;
            continue;
        }
        if let Some(key) = window.work.next_geometry(&window.elements) {
            if !budget.admit(window, ProcessPhase::Spatial) {
                window.work.geometry.reserve(1 + window.work.attempts);
                window.work.geometry.push(key);
                return Ok(false);
            }
            window.progress.effects_ready = false;
            window.progress.frontier_ready = false;
            project(window, key)?;
            continue;
        }
        if window.work.has_geometry() {
            return Err(ElementError::UnresolvedWork {
                id: window.root,
                work: Work::LAYOUT,
            });
        }

        let spatial_consumers = window.progress.spatial_pending
            || window.work.hit_changed
            || !window.layout.changed.is_empty()
            || !window.layout.qualification_changed.is_empty()
            || window.elements.order_from.is_some()
            || !window.elements.retired.is_empty();
        if spatial_consumers && !window.progress.hit_current {
            if !budget.admit(window, ProcessPhase::Spatial) {
                return Ok(false);
            }
            save_cleanup_geometry(window);
            window
                .input
                .sync_hit(&window.elements, &window.layout, &mut window.work)?;
            window.progress.hit_current = true;
            window.progress.spatial_pending = true;
            continue;
        }

        if window.progress.spatial_pending {
            if interaction_callbacks_pending(window) && !budget.admit(window, ProcessPhase::Effects)
            {
                return Ok(false);
            }
            clear_ancestry(window);
            route::cancel_invalid(
                &mut window.elements,
                &window.configuration,
                &mut window.layout,
                &mut window.work,
                &mut window.input,
            )?;
            window.input.clear_cleanup_geometry();
            window.progress.cleanup_saved = false;
            window.progress.spatial_pending = false;
            window.progress.effects_ready = true;
            window.progress.frontier_ready = true;
            if window.work.hit_changed {
                window.progress.hit_current = false;
            }
            continue;
        }
        if !window.progress.effects_ready || !window.progress.frontier_ready {
            window.progress.effects_ready = true;
            window.progress.frontier_ready = true;
            continue;
        }
        if window.input.has_effects() || window.input.has_deferred_focus() {
            continue;
        }
        check_obligations(window)?;

        if spatial_consumers && !window.progress.decoration_current {
            if !budget.admit(window, ProcessPhase::Semantics) {
                return Ok(false);
            }
            window.input.decorate_semantics(
                &window.elements,
                &window.layout,
                &mut window.work.counts,
            )?;
            window.progress.decoration_current = true;
            continue;
        }
        if let Some((key, row)) = window.work.next_stage(Work::SEMANTICS, &window.elements) {
            if !output_visible(window, row) {
                window.work.defer(row, Work::SEMANTICS);
                continue;
            }
            if let Some(ancestor) = unchecked_ancestor(window, key) {
                window.work.return_stage(key, row, Work::SEMANTICS);
                if !budget.admit(window, ProcessPhase::Configuration) {
                    return Ok(false);
                }
                refresh_configuration(window, ancestor, false)?;
                continue;
            }
            if !budget.admit(window, ProcessPhase::Semantics) {
                window.work.return_stage(key, row, Work::SEMANTICS);
                return Ok(false);
            }
            semantics(window, key, row)?;
            continue;
        }

        if window.elements.order_from.is_some() && !window.progress.scene_order_current {
            if !window.scene.is_empty() && !budget.admit(window, ProcessPhase::Scene) {
                return Ok(false);
            }
            clear_ancestry(window);
            remove_retired_scene(window);
            let from = window
                .elements
                .order_from
                .expect("unconsumed scene order has a dirty suffix");
            let elements = &window.elements;
            let visits = window
                .scene
                .reindex_order_from(from, |id| elements.rank(id.local()))?;
            window.work.counts.scene_order_visits =
                window.work.counts.scene_order_visits.saturating_add(visits);
            window.work.counts.scene_order_changes = window
                .work
                .counts
                .scene_order_changes
                .saturating_add(visits);
            window.progress.scene_changed |= visits != 0;
            window.progress.scene_order_current = true;
            continue;
        }
        if let Some((key, row)) = window.work.next_composition(&window.elements) {
            if !budget.admit(window, ProcessPhase::Scene) {
                window.work.compositions.reserve(1 + window.work.attempts);
                window.work.compositions.push_front(key);
                return Ok(false);
            }
            compose_scene(window, key, row)?;
            continue;
        }
        if let Some((key, row)) = window.work.next_stage(Work::SCENE, &window.elements) {
            if !output_visible(window, row) || window.layout.clip_bounds(key, row).is_empty() {
                clear_ancestry(window);
                let id = window.elements.id(key);
                let removed = window.scene.remove(id);
                window.progress.scene_changed |= removed;
                if removed || window.work.render_state(row) == RenderState::Unrecorded {
                    let dependencies = window.work.scene_dependencies(row);
                    window
                        .work
                        .set_render(row, RenderState::Released, dependencies);
                }
                window.work.defer(row, Work::SCENE);
                continue;
            }
            if let Some(ancestor) = unchecked_ancestor(window, key) {
                window.work.return_stage(key, row, Work::SCENE);
                if !budget.admit(window, ProcessPhase::Configuration) {
                    return Ok(false);
                }
                refresh_configuration(window, ancestor, false)?;
                continue;
            }
            if !budget.admit(window, ProcessPhase::Scene) {
                window.work.return_stage(key, row, Work::SCENE);
                return Ok(false);
            }
            record_scene(window, key, row)?;
            continue;
        }
        if window.work.composition_pending != 0 {
            return Err(ElementError::UnresolvedWork {
                id: window.root,
                work: Work::SCENE,
            });
        }

        if !window.elements.retired.is_empty() {
            if !budget.admit(window, ProcessPhase::Retirement) {
                return Ok(false);
            }
            retire(window)?;
            continue;
        }

        check_obligations(window)?;
        debug_assert!(!window.input.has_effects() && !window.input.has_deferred_focus());

        clear_ancestry(window);
        window.elements.order_from = None;
        window.layout.clear_deltas(&window.elements);
        if window.progress.scene_changed {
            window.scene.changed();
            window.progress.scene_changed = false;
        }

        window.work.trim(&window.elements, &window.layout);
        trim_key_buffer(&mut window.progress.config_stack);
        window.configuration.trim();
        window.layout.trim();
        window.elements.trim();
        route::finish_batch(&mut window.input);

        return Ok(true);
    }
}

/// Cancels while records exist, unmounts children first, then reclaims one aligned batch.
///
/// The driver enters this boundary after effect closure; close enters it after
/// terminally discarding business effects. Normal unmount errors do not skip other
/// callbacks or remapping. All removed user-bearing owners remain in an owned bundle
/// until every dense move, child-set inverse and identity invalidation has committed.
pub(crate) fn retire(window: &mut WindowState) -> Result<(), ElementError> {
    clear_ancestry(window);
    if window.elements.retired.is_empty() {
        return Ok(());
    }
    if window.elements.frames != 0
        || window.work.attempts != 0
        || !window.configuration.can_reclaim()
        || !window.layout.can_reclaim()
    {
        return Err(ElementError::Reentrant(window.root));
    }

    if retiring_environment_pending(window) {
        refresh_retiring_configuration(window)?;
    }

    let cancellation = route::cancel_invalid(
        &mut window.elements,
        &window.configuration,
        &mut window.layout,
        &mut window.work,
        &mut window.input,
    );
    if window.input.has_effects() {
        return cancellation.and(Err(ElementError::UnresolvedWork {
            id: window.root,
            work: Work::all(),
        }));
    }
    let mut first_error = cancellation.err();
    // Keep the owning retirement ledger intact if an unmount callback unwinds.
    let mut keys = window.elements.retired.clone();
    keys.sort_unstable_by_key(|key| std::cmp::Reverse(window.elements.depth(*key).unwrap_or(0)));
    for &key in &keys {
        let Some(row) = window.elements.row_local(key) else {
            continue;
        };
        window.work.counts.retirement_visits =
            window.work.counts.retirement_visits.saturating_add(1);
        if window.work.rows[row.0].unmount_started || window.elements.nodes[row.0].body.is_none() {
            continue;
        }
        window.work.rows[row.0].unmount_started = true;
        let id = window.elements.id(key);
        let mut cx = UnmountCx {
            current: id,
            configuration: window.configuration.read(row),
            geometry: window.layout.cleanup_geometry(row),
        };
        let result = window
            .elements
            .body_mut(key)
            .expect("initialized retirement retains its body")
            .unmount(&mut cx);
        window.work.counts.unmounts = window.work.counts.unmounts.saturating_add(1);
        if let Err(error) = result
            && first_error.is_none()
        {
            first_error = Some(error);
        }
    }

    for &key in &keys {
        window.input.remove_projection(key, &mut window.work.counts);
        let removed = window.scene.remove(window.elements.id(key));
        window.progress.scene_changed |= removed;
        if removed {
            window.work.counts.scene_order_changes =
                window.work.counts.scene_order_changes.saturating_add(1);
        }
    }

    let drop_owner = keys
        .first()
        .map(|key| window.elements.id(*key))
        .unwrap_or(window.root);
    let mut removed = Vec::with_capacity(keys.len());
    for &key in &keys {
        let Some(row) = window.elements.row_local(key) else {
            continue;
        };
        let children = window.elements.take_child_set(key);
        let configuration = window.configuration.remove_row(row, key);
        window.layout.remove_row(row, key, &window.elements);
        window.work.remove_row(row);
        let node = window.elements.remove_row(row);
        removed.push((node.body, configuration, children));
    }

    window.elements.retired.clear();
    if window
        .elements
        .retired
        .capacity()
        .saturating_mul(size_of::<LocalKey>())
        > 64 * 1024
    {
        window.elements.retired = Vec::new();
    }
    window.input.clear_cleanup_geometry();
    window.progress.cleanup_saved = false;

    crate::element::call::drop_committed(&mut window.access(), drop_owner, removed);

    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// Diagnostic counts of callbacks, admitted units, and framework traversal work.
///
/// [`crate::application::App::counts`] returns lifetime totals for an open window;
/// [`ProcessOutcome`] and [`crate::application::App::update`] return per-call deltas.
/// Input admission itself can do work before a subsequent processing call begins.
///
/// These counters are not timings, allocation measurements, or a one-callback-per-unit
/// guarantee. They saturate instead of making UI work invalid; saturation limits
/// diagnostic precision, not how much finite work the framework accepts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WorkCounts {
    pub mounts: u64,
    pub updates: u64,
    pub unmounts: u64,
    pub preparations: u64,
    pub layouts: u64,
    pub layout_hits: u64,
    pub scenes: u64,
    pub semantic_updates: u64,
    pub events: u64,
    pub actions: u64,
    pub requests: u64,
    /// Complete cooperative units admitted during processing, including pure consumers.
    pub process_units: u64,
    /// Full required-measurement and failure-ledger audit invocations.
    pub obligation_audits: u64,
    pub configuration_visits: u64,
    pub invalidations: u64,
    pub retired_requests: u64,
    pub prepare_ordered: u64,
    pub layout_ancestors: u64,
    pub geometry_visits: u64,
    pub tree_order_visits: u64,
    pub hit_memberships: u64,
    pub hit_candidates: u64,
    pub hit_order_visits: u64,
    pub scene_order_changes: u64,
    pub scene_order_visits: u64,
    pub configuration_updates: u64,
    pub layout_dependency_checks: u64,
    pub layout_block_visits: u64,
    pub scene_compositions: u64,
    pub hit_refinements: u64,
    pub retirement_visits: u64,
}

impl std::ops::Sub for WorkCounts {
    type Output = Self;

    fn sub(self, rhs: Self) -> Self {
        Self {
            mounts: self.mounts.saturating_sub(rhs.mounts),
            updates: self.updates.saturating_sub(rhs.updates),
            unmounts: self.unmounts.saturating_sub(rhs.unmounts),
            preparations: self.preparations.saturating_sub(rhs.preparations),
            layouts: self.layouts.saturating_sub(rhs.layouts),
            layout_hits: self.layout_hits.saturating_sub(rhs.layout_hits),
            scenes: self.scenes.saturating_sub(rhs.scenes),
            semantic_updates: self.semantic_updates.saturating_sub(rhs.semantic_updates),
            events: self.events.saturating_sub(rhs.events),
            actions: self.actions.saturating_sub(rhs.actions),
            requests: self.requests.saturating_sub(rhs.requests),
            process_units: self.process_units.saturating_sub(rhs.process_units),
            obligation_audits: self.obligation_audits.saturating_sub(rhs.obligation_audits),
            configuration_visits: self
                .configuration_visits
                .saturating_sub(rhs.configuration_visits),
            invalidations: self.invalidations.saturating_sub(rhs.invalidations),
            retired_requests: self.retired_requests.saturating_sub(rhs.retired_requests),
            prepare_ordered: self.prepare_ordered.saturating_sub(rhs.prepare_ordered),
            layout_ancestors: self.layout_ancestors.saturating_sub(rhs.layout_ancestors),
            geometry_visits: self.geometry_visits.saturating_sub(rhs.geometry_visits),
            tree_order_visits: self.tree_order_visits.saturating_sub(rhs.tree_order_visits),
            hit_memberships: self.hit_memberships.saturating_sub(rhs.hit_memberships),
            hit_candidates: self.hit_candidates.saturating_sub(rhs.hit_candidates),
            hit_order_visits: self.hit_order_visits.saturating_sub(rhs.hit_order_visits),
            scene_order_changes: self
                .scene_order_changes
                .saturating_sub(rhs.scene_order_changes),
            scene_order_visits: self
                .scene_order_visits
                .saturating_sub(rhs.scene_order_visits),
            configuration_updates: self
                .configuration_updates
                .saturating_sub(rhs.configuration_updates),
            layout_dependency_checks: self
                .layout_dependency_checks
                .saturating_sub(rhs.layout_dependency_checks),
            layout_block_visits: self
                .layout_block_visits
                .saturating_sub(rhs.layout_block_visits),
            scene_compositions: self
                .scene_compositions
                .saturating_sub(rhs.scene_compositions),
            hit_refinements: self.hit_refinements.saturating_sub(rhs.hit_refinements),
            retirement_visits: self.retirement_visits.saturating_sub(rhs.retirement_visits),
        }
    }
}
