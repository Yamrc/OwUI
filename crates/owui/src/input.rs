//! Input values, read-only callback contexts, and owned interaction effects.
//!
//! Routes collect requests and source-bound actions without editing configuration
//! or structure. The coordinator applies effects before mutable framework phases;
//! terminal interaction masks remain active until the complete cycle settles.

pub(crate) mod hit;
pub(crate) mod route;
mod semantics;

use std::any::{Any, TypeId};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use crate::element::configuration::{ConfigRead, Configuration};
use crate::element::mounted::{Life, MountedElements};
use crate::element::{ElementError, ElementId, Property, Visibility};
use crate::id::LocalKey;
use crate::layout::{ClipBounds, Geometry, Layout, Point, Rect};
use crate::schedule::{Scheduler, Work, WorkCounts};
use crate::view::PendingAction;

use hit::HitIndex;
use semantics::{FocusSnapshot, SemanticProjection};

/// A semantic event key and its owned payload type.
pub trait EventKey: 'static {
    type Data: 'static;
}

/// Activates an enabled control through pointer, keyboard, or accessibility input.
pub struct Activate;

impl EventKey for Activate {
    type Data = ();
}

/// Submits an editing control's current text.
pub struct Submit;

impl EventKey for Submit {
    type Data = String;
}

/// Reports a changed value without implying submission.
pub struct Changed;

impl EventKey for Changed {
    type Data = String;
}

/// A pointer identity, independent of the platform device representation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct PointerId(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointerKind {
    Move,
    Down,
    Up,
    /// Ends this pointer's interaction, including capture and hover.
    Cancel,
    /// A window-boundary entry from a host, or an element entry from the coordinator.
    Enter,
    /// A window-boundary exit from a host. Exiting does not cancel capture.
    Leave,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyCode {
    Enter,
    Space,
    Escape,
    Backspace,
    Tab,
    Character(char),
}

/// The route stage currently visiting an element.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventPhase {
    Capture,
    Target,
    Bubble,
}

/// Input delivered by a host or synthesized by the interaction coordinator.
///
/// A host-supplied Focus event changes logical window focus; element focus callbacks
/// are synthesized internally. Focus(false) cancels current interactions and
/// Focus(true) does not restore old element focus. Pointer and scroll positions
/// submitted while the window is unfocused are ignored.
#[derive(Clone, Debug, PartialEq)]
pub enum InputEvent {
    Pointer {
        id: PointerId,
        kind: PointerKind,
        position: Point,
    },
    Scroll {
        position: Point,
        delta: Point,
    },
    Key {
        code: KeyCode,
        pressed: bool,
    },
    Text(String),
    Composition {
        text: String,
        committed: bool,
    },
    Focus(bool),
    Activate,
}

impl InputEvent {
    pub fn position(&self) -> Option<Point> {
        match self {
            Self::Pointer { position, .. } | Self::Scroll { position, .. } => Some(*position),
            Self::Key { .. }
            | Self::Text(_)
            | Self::Composition { .. }
            | Self::Focus(_)
            | Self::Activate => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Propagation {
    #[default]
    Continue,
    Stop,
}

/// A control's role in the published semantic output.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Role {
    #[default]
    Generic,
    Button,
    TextInput,
    Text,
    List,
    ListItem,
    Progress,
}

/// Local semantic content supplied by a mounted element.
///
/// Participation and disabled state come from the framework's single qualification
/// projection. Focusable controls enter Tab order only while qualified; removing a
/// Tab stop does not itself revoke an existing programmatic focus.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SemanticContent {
    pub role: Role,
    pub name: Option<Arc<str>>,
    pub value: Option<Arc<str>>,
    pub focusable: bool,
    pub total_items: Option<usize>,
    pub position: Option<usize>,
}

/// Flat semantic output decorated with current framework qualification.
///
/// Geometry comes from the same published layout. Nonparticipating and hidden
/// elements have no published semantic output; visible disabled controls retain
/// their content with disabled set to true.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Semantics {
    pub role: Role,
    pub name: Option<Arc<str>>,
    pub value: Option<Arc<str>>,
    pub disabled: bool,
    pub focusable: bool,
    pub total_items: Option<usize>,
    pub position: Option<usize>,
}

/// Read-only effective configuration for one semantic-content callback.
pub struct SemanticCx<'a> {
    current: ElementId,
    configuration: ConfigRead<'a>,
}

impl<'a> SemanticCx<'a> {
    pub(crate) fn new(current: ElementId, configuration: ConfigRead<'a>) -> Self {
        Self {
            current,
            configuration,
        }
    }

    pub fn id(&self) -> ElementId {
        self.current
    }

    /// Reads current canonical visibility rather than the retained cleanup pose's qualification.
    pub fn current_visibility(&self) -> Visibility {
        self.configuration.visibility()
    }

    pub fn get<P: Property>(&self, property: P) -> P::Value {
        self.configuration.get(property)
    }

    pub fn get_ref<P: Property>(&self, property: P) -> Option<&'a P::Value> {
        self.configuration.get_ref(property)
    }
}

/// Read-only effective configuration for a local hit-test refinement.
pub struct HitTestCx<'a> {
    current: ElementId,
    configuration: ConfigRead<'a>,
}

impl<'a> HitTestCx<'a> {
    pub(crate) fn new(current: ElementId, configuration: ConfigRead<'a>) -> Self {
        Self {
            current,
            configuration,
        }
    }

    pub fn id(&self) -> ElementId {
        self.current
    }

    /// Reads current canonical visibility rather than the retained cleanup pose's qualification.
    pub fn current_visibility(&self) -> Visibility {
        self.configuration.visibility()
    }

    pub fn get<P: Property>(&self, property: P) -> P::Value {
        self.configuration.get(property)
    }

    pub fn get_ref<P: Property>(&self, property: P) -> Option<&'a P::Value> {
        self.configuration.get_ref(property)
    }
}

pub(crate) struct Emission {
    pub(crate) event: TypeId,
    pub(crate) data: Box<dyn Any>,
}

pub(crate) enum InputRequest {
    Focus(Option<ElementId>),
    Capture(PointerId, ElementId),
    Release(PointerId, ElementId),
}

/// Read-only configuration and owned effects for one element route stage.
///
/// Requests are applied after the current element borrow ends. Each emission is
/// bound to the source's current handler before yielding; actions execute before
/// configuration installation, structural editing, or layout callbacks.
pub struct EventCx<'a> {
    pub(crate) current: ElementId,
    pub(crate) bounds: Rect,
    pub(crate) clip: Rect,
    pub(crate) clip_bounds: ClipBounds,
    pub(crate) hit: Option<bool>,
    pub(crate) phase: EventPhase,
    pub(crate) configuration: ConfigRead<'a>,
    pub(crate) work: Work,
    pub(crate) hit_dirty: bool,
    pub(crate) requests: Vec<InputRequest>,
    pub(crate) emissions: Vec<Emission>,
}

impl<'a> EventCx<'a> {
    pub fn id(&self) -> ElementId {
        self.current
    }

    /// Reads current canonical visibility rather than the retained cleanup pose's qualification.
    pub fn current_visibility(&self) -> Visibility {
        self.configuration.visibility()
    }

    /// The prepared world bounds for this route stage.
    ///
    /// Cleanup can retain the pose preceding a spatial or qualification transition.
    /// State and configuration remain current; this is not a rollback snapshot.
    pub fn bounds(&self) -> Rect {
        self.bounds
    }

    /// A conservative cover of the prepared ancestor clip in window coordinates.
    ///
    /// Use [`Self::clip_bounds`] for exact half-open membership.
    pub fn clip(&self) -> Rect {
        self.clip
    }

    /// The exact prepared ancestor clip, including a retained cleanup clip when applicable.
    ///
    /// State, configuration and qualification remain current while cleanup retains old endpoints.
    pub fn clip_bounds(&self) -> ClipBounds {
        self.clip_bounds
    }

    /// Whether this event's position effectively hits this element before its callback.
    ///
    /// Applies prepared bounds, clipping, qualification, and local refinement.
    /// Capture changes routing, not this result. Events without positions return None.
    pub fn hit(&self) -> Option<bool> {
        self.hit
    }

    pub fn phase(&self) -> EventPhase {
        self.phase
    }

    pub fn get<P: Property>(&self, property: P) -> P::Value {
        self.configuration.get(property)
    }

    /// Borrows effective configuration independently of request and emission collection.
    pub fn get_ref<P: Property>(&self, property: P) -> Option<&'a P::Value> {
        self.configuration.get_ref(property)
    }

    pub fn invalidate(&mut self, work: Work) {
        self.work |= work;
    }

    /// Invalidates local refinement when State changes without changing geometry.
    pub fn invalidate_hit_test(&mut self) {
        self.hit_dirty = true;
    }

    pub fn focus(&mut self) {
        self.requests.push(InputRequest::Focus(Some(self.current)));
    }

    pub fn clear_focus(&mut self) {
        self.requests.push(InputRequest::Focus(None));
    }

    /// Requests capture of a pointer whose real position has been observed.
    ///
    /// Same-pointer capture is rejected throughout an Up or Cancel settlement.
    pub fn capture(&mut self, pointer: PointerId) {
        self.requests
            .push(InputRequest::Capture(pointer, self.current));
    }

    pub fn release(&mut self, pointer: PointerId) {
        self.requests
            .push(InputRequest::Release(pointer, self.current));
    }

    pub fn has_handler<K: EventKey>(&self, event: K) -> bool {
        self.configuration.has_handler(event)
    }

    /// Collects an owned payload, including when no handler is installed.
    ///
    /// An unconsumed payload is dropped after the source body borrow ends.
    pub fn emit<K: EventKey>(&mut self, _event: K, data: K::Data) {
        self.emissions.push(Emission {
            event: TypeId::of::<K>(),
            data: Box::new(data),
        });
    }

    /// Constructs and emits a payload only if the source currently has a handler.
    pub fn emit_with<K: EventKey>(&mut self, _event: K, create: impl FnOnce() -> K::Data) {
        if self.configuration.binding(TypeId::of::<K>()).is_some() {
            self.emissions.push(Emission {
                event: TypeId::of::<K>(),
                data: Box::new(create()),
            });
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct PointerState {
    pub(crate) position: Point,
    pub(crate) inside: bool,
    pub(crate) hover: Option<LocalKey>,
    pub(crate) capture: Option<LocalKey>,
}

impl PointerState {
    fn new(position: Point, inside: bool) -> Self {
        Self {
            position,
            inside,
            hover: None,
            capture: None,
        }
    }
}

#[derive(Default)]
struct InputBatch {
    terminal_pointers: HashSet<PointerId>,
    suppress_acquisition: bool,
    default_tab: Option<FocusSnapshot>,
}

pub(crate) enum Cleanup {
    Focus(LocalKey),
    Capture {
        pointer: PointerId,
        target: LocalKey,
        position: Point,
    },
    Hover {
        pointer: PointerId,
        target: LocalKey,
        position: Point,
    },
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct CleanupPose {
    bounds: Rect,
    clip: Rect,
}

impl CleanupPose {
    fn from_geometry(geometry: Geometry) -> Self {
        Self {
            bounds: geometry.bounds,
            clip: geometry.clip,
        }
    }
}

/// Sparse interaction references and actual hit/semantic output membership.
pub(crate) struct InputState {
    pub(crate) window_focused: bool,
    pub(crate) closing: bool,
    pub(crate) pointers: HashMap<PointerId, PointerState>,
    pointer_capacity: usize,
    pub(crate) focus: Option<LocalKey>,
    pub(crate) deferred_focus: Option<ElementId>,
    batch: InputBatch,
    requests: Vec<InputRequest>,
    request_queue: VecDeque<InputRequest>,
    actions: VecDeque<PendingAction>,
    pub(crate) event_requests: Vec<InputRequest>,
    pub(crate) event_emissions: Vec<Emission>,
    pub(crate) path: Vec<LocalKey>,
    pub(crate) cleanup: Vec<Cleanup>,
    pub(crate) pointer_scratch: Vec<(PointerId, Point)>,
    cleanup_poses: HashMap<LocalKey, CleanupPose>,
    cleanup_clip_bounds: HashMap<LocalKey, ClipBounds>,
    hit: HitIndex,
    semantics: SemanticProjection,
    pub(crate) rehit_pending: bool,
}

impl Default for InputState {
    fn default() -> Self {
        Self::new()
    }
}

impl InputState {
    pub(crate) fn new() -> Self {
        Self {
            window_focused: true,
            closing: false,
            pointers: HashMap::new(),
            pointer_capacity: 0,
            focus: None,
            deferred_focus: None,
            batch: InputBatch::default(),
            requests: Vec::new(),
            request_queue: VecDeque::new(),
            actions: VecDeque::new(),
            event_requests: Vec::new(),
            event_emissions: Vec::new(),
            path: Vec::new(),
            cleanup: Vec::new(),
            pointer_scratch: Vec::new(),
            cleanup_poses: HashMap::new(),
            cleanup_clip_bounds: HashMap::new(),
            hit: HitIndex::default(),
            semantics: SemanticProjection::default(),
            rehit_pending: false,
        }
    }

    pub(crate) fn requests_mut(&mut self) -> &mut Vec<InputRequest> {
        &mut self.requests
    }

    pub(crate) fn actions_mut(&mut self) -> &mut VecDeque<PendingAction> {
        &mut self.actions
    }

    pub(crate) fn has_actions(&self) -> bool {
        !self.actions.is_empty()
    }

    /// Owned effects; newly prepared requests still require the driver's qualification gate.
    /// An unresolved focus intent alone does not fence original actions.
    pub(crate) fn has_effects(&self) -> bool {
        !self.requests.is_empty()
            || !self.request_queue.is_empty()
            || !self.actions.is_empty()
            || self.batch.default_tab.is_some()
    }

    pub(crate) fn has_deferred_focus(&self) -> bool {
        self.deferred_focus.is_some()
    }

    pub(crate) fn focused(&self, elements: &MountedElements) -> Option<ElementId> {
        self.focus
            .filter(|key| self.window_focused && elements.contains_local(*key))
            .map(|key| elements.id(key))
    }

    pub(crate) fn semantic<'a>(
        &'a self,
        id: ElementId,
        elements: &MountedElements,
    ) -> Option<&'a Semantics> {
        elements
            .contains(id)
            .then(|| self.semantics.get(id.local()))
            .flatten()
    }

    /// Saves prepared bounds/clip for actual interaction targets before spatial overwrites.
    /// Numeric exceptions retain separate exact endpoints only for those actual targets.
    ///
    /// Repeated preparation keeps the first undelivered pose through Pending or fault.
    /// This retains no State or configuration snapshot and does not make an inactive
    /// target eligible. The caller clears it only after the frontier's finite cleanup
    /// and stationary callbacks have been delivered.
    pub(crate) fn prepare_cleanup_geometry(&mut self, elements: &MountedElements, layout: &Layout) {
        let targets = self.focus.into_iter().chain(
            self.pointers
                .values()
                .flat_map(|pointer| pointer.hover.into_iter().chain(pointer.capture)),
        );
        for key in targets {
            let std::collections::hash_map::Entry::Vacant(entry) = self.cleanup_poses.entry(key)
            else {
                continue;
            };
            if let Some(geometry) = prepared_geometry(elements, layout, key) {
                entry.insert(CleanupPose::from_geometry(geometry));
                if let Some(bounds) = layout.clip_override(key) {
                    self.cleanup_clip_bounds.insert(key, bounds);
                }
            }
        }
    }

    /// Releases saved poses after successful delivery at a complete qualification frontier.
    pub(crate) fn clear_cleanup_geometry(&mut self) {
        self.cleanup_poses = HashMap::new();
        self.cleanup_clip_bounds = HashMap::new();
    }

    fn saved_cleanup_pose(&self, key: LocalKey) -> Option<CleanupPose> {
        self.cleanup_poses.get(&key).copied()
    }

    fn saved_cleanup_clip_bounds(&self, key: LocalKey, pose: CleanupPose) -> ClipBounds {
        self.cleanup_clip_bounds
            .get(&key)
            .copied()
            .unwrap_or_else(|| ClipBounds::from(pose.clip))
    }

    /// Synchronizes only known geometry/qualification deltas and actual ordered hit members.
    ///
    /// Shared delta lists remain owned by Layout/Elements until all consumers finish.
    pub(crate) fn sync_hit(
        &mut self,
        elements: &MountedElements,
        layout: &Layout,
        work: &mut Scheduler,
    ) -> Result<bool, ElementError> {
        let changed = self.hit.sync(elements, layout, &mut work.counts)?;
        let refinement = std::mem::take(&mut work.hit_changed);
        self.rehit_pending |= changed || refinement;

        Ok(changed || refinement)
    }

    pub(crate) fn publish_semantic(
        &mut self,
        id: ElementId,
        content: SemanticContent,
        elements: &MountedElements,
        configuration: &Configuration,
        layout: &Layout,
    ) -> Result<(), ElementError> {
        let row = elements
            .row(id)
            .filter(|_| elements.contains(id))
            .ok_or(ElementError::Missing(id))?;

        self.semantics.publish(
            id,
            content,
            configuration.read(row),
            layout,
            row,
            elements.rank(id.local()),
        )
    }

    /// Applies qualification decoration without invoking semantic-content callbacks.
    pub(crate) fn decorate_semantics(
        &mut self,
        elements: &MountedElements,
        layout: &Layout,
        counts: &mut WorkCounts,
    ) -> Result<(), ElementError> {
        self.semantics.sync_qualification(elements, layout, counts)
    }

    pub(crate) fn remove_projection(&mut self, key: LocalKey, counts: &mut WorkCounts) {
        self.hit.remove(key, counts);
        self.semantics.remove(key);
    }

    pub(crate) fn trim(&mut self) {
        self.hit.trim();
        self.semantics.trim();
        trim_map(&mut self.pointers, &mut self.pointer_capacity);

        trim_deque(&mut self.request_queue);
        trim_deque(&mut self.actions);
        trim_vec(&mut self.requests);

        if self.batch.terminal_pointers.is_empty() {
            self.batch.terminal_pointers = HashSet::new();
        }
    }
}

const INPUT_BUFFER_BYTES: usize = 64 * 1024;

pub(crate) fn retain_buffer<T>(slot: &mut Vec<T>, mut values: Vec<T>) {
    values.clear();
    if values.capacity().saturating_mul(size_of::<T>()) <= INPUT_BUFFER_BYTES {
        *slot = values;
    }
}

pub(super) fn needs_trim_vec<T>(values: &Vec<T>) -> bool {
    !values.is_empty()
        && values.capacity() > values.len().saturating_mul(4).max(16)
        && values.capacity().saturating_mul(size_of::<T>()) > INPUT_BUFFER_BYTES
}

fn trim_vec<T>(values: &mut Vec<T>) {
    if values.is_empty() {
        *values = Vec::new();
    } else if needs_trim_vec(values) {
        values.shrink_to(values.len().saturating_mul(2).max(16));
    }
}

fn trim_deque<T>(values: &mut VecDeque<T>) {
    if values.is_empty() {
        *values = VecDeque::new();
    } else if values.capacity() > values.len().saturating_mul(4).max(16)
        && values.capacity().saturating_mul(size_of::<T>()) > INPUT_BUFFER_BYTES
    {
        values.shrink_to(values.len().saturating_mul(2).max(16));
    }
}

fn trim_map<K: Eq + std::hash::Hash, V>(values: &mut HashMap<K, V>, capacity: &mut usize) {
    // Tombstones can reduce reported capacity without releasing the bucket allocation.
    *capacity = (*capacity).max(values.capacity());
    if values.is_empty() {
        *values = HashMap::new();
        *capacity = 0;
    } else if *capacity > values.len().saturating_mul(4).max(16)
        && (*capacity).saturating_mul(size_of::<(K, V)>()) > INPUT_BUFFER_BYTES
    {
        values.shrink_to(values.len().saturating_mul(2).max(16));
        *capacity = values.capacity();
    }
}

pub(crate) fn qualified(elements: &MountedElements, layout: &Layout, key: LocalKey) -> bool {
    let Some(row) = elements.row_local(key) else {
        return false;
    };

    elements.nodes[row.0].life == Life::Live
        && layout.eligible(row)
        && layout.cleanup_geometry(row).is_some()
}

pub(crate) fn prepared_geometry(
    elements: &MountedElements,
    layout: &Layout,
    key: LocalKey,
) -> Option<Geometry> {
    layout.cleanup_geometry(elements.row_local(key)?)
}
