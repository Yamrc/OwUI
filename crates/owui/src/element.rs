//! Typed declarations, persistent element behavior, and scoped configuration access.
//!
//! An [`Element`] keeps its concrete type while builders configure it. Consumption
//! separates its local [`Attrs`] from the actual [`MountedElement`]; mounting then
//! initializes that behavior's State. A matching child identity and mounted type
//! retain State across later declarations, including declarations with different
//! wrapper types. Built-in and external controls use the same protocol.
//!
//! Mounted callbacks read effective properties from their contexts. Local values
//! mask inherited values; absence remains distinct from an explicit default.
//! Runtime writes update this canonical source, not a retained declaration copy.
//! See [`MountedElement`] for replacement and failure boundaries.
//!
//! Configuration-mutating contexts such as [`MountCx`], [`UpdateCx`] and
//! [`LayoutCx`] return property references tied to the current context borrow;
//! those references must cease to be used before mutating the context. Read-only
//! configuration views in [`SceneCx`], [`SemanticCx`], [`HitTestCx`], [`EventCx`]
//! and [`UnmountCx`] keep input references independent of output/effect recording.
//! Neither kind of reference can outlive the callback's configuration lifetime.

mod builtin;
pub(crate) mod call;
pub(crate) mod children;
pub(crate) mod configuration;
pub(crate) mod mounted;
mod property;

use std::any::TypeId;
use std::sync::Arc;

use thiserror::Error;

pub use builtin::{Button, Column, Text, TextInput};
pub use children::ChildrenCx;
pub(crate) use property::InheritedValues;
pub use property::{
    Background, Enabled, Foreground, Gap, Height, Name, Properties, Property, Width,
};

use crate::color::Color;
use crate::id::LocalKey;
use crate::input::{
    EventCx, EventKey, HitTestCx, InputEvent, Propagation, SemanticContent, SemanticCx,
};
use crate::layout::{Constraints, Geometry, LayoutCx, Point, Rect, Size};
use crate::scene::SceneCx;
use crate::schedule::Work;
use crate::view::{Action, ActionBinding};

use configuration::ConfigRead;
use mounted::ElementAccess;
use property::PropertyChange;

/// A domain- and generation-checked handle that does not keep an element alive.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ElementId(crate::id::Id);

impl crate::id::Key for ElementId {
    fn from_id(id: crate::id::Id) -> Self {
        Self(id)
    }

    fn id(self) -> crate::id::Id {
        self.0
    }
}

impl ElementId {
    pub(crate) fn local(self) -> LocalKey {
        self.0.slot
    }

    pub(crate) fn domain(self) -> u64 {
        self.0.domain
    }
}

/// A declaration identity within one parent's direct child set.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Key {
    Index(u64),
    Text(Arc<str>),
}

impl From<u64> for Key {
    fn from(value: u64) -> Self {
        Self::Index(value)
    }
}

impl From<usize> for Key {
    fn from(value: usize) -> Self {
        Self::Index(value as u64)
    }
}

impl From<&str> for Key {
    fn from(value: &str) -> Self {
        Self::Text(Arc::from(value))
    }
}

impl From<String> for Key {
    fn from(value: String) -> Self {
        Self::Text(Arc::from(value))
    }
}

/// Layout and output participation without destroying the mounted identity.
///
/// Hidden elements occupy layout space but do not produce interactive or visible
/// output. Collapsed elements do not occupy space. Neither setting removes State
/// or clears unresolved work; ancestor qualification also affects participation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Visibility {
    #[default]
    Visible,
    Hidden,
    Collapsed,
}

/// Local declaration attributes; an empty set allocates no heap storage.
///
/// These values describe the next application of a declaration. They do not track
/// runtime changes after mounting. Inherited values are resolved by mounted callback
/// contexts, not by this builder; use their effective getters inside callbacks.
#[derive(Default)]
pub struct Attrs {
    pub(crate) key: Option<Key>,
    pub(crate) local: Properties,
    pub(crate) handlers: Vec<(TypeId, ActionBinding)>,
    pub(crate) visibility: Visibility,
}

impl Attrs {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get<P: Property>(&self, property: P) -> P::Value {
        self.local.get(property)
    }

    pub fn get_ref<P: Property>(&self, _property: P) -> Option<&P::Value> {
        self.local.get_ref::<P>()
    }

    pub fn properties(&self) -> &Properties {
        &self.local
    }

    pub fn properties_mut(&mut self) -> &mut Properties {
        &mut self.local
    }

    pub fn visibility(&self) -> Visibility {
        self.visibility
    }
}

/// Operational failures in the logical element core.
#[derive(Debug, Error)]
pub enum ElementError {
    #[error("element {0:?} is no longer mounted")]
    Missing(ElementId),
    #[error("element {0:?} is already executing")]
    Reentrant(ElementId),
    #[error("element {child:?} is not a child of {parent:?}")]
    WrongParent { parent: ElementId, child: ElementId },
    #[error("duplicate child key: {0:?}")]
    DuplicateKey(Key),
    #[error("child {0:?} was retained more than once")]
    DuplicateChild(ElementId),
    #[error("invalid geometry or constraints")]
    InvalidGeometry,
    #[error("element behavior and retained state do not match")]
    TypeMismatch,
    #[error("a one-shot initializer was already consumed without successful completion")]
    InitializerConsumed,
    #[error("element {id:?} has unresolved failed work {work:?}")]
    UnresolvedWork { id: ElementId, work: Work },
    #[error("configuration revision space is exhausted")]
    RevisionExhausted,
    #[error("element depth cannot be represented")]
    DepthExhausted,
    #[error("presentation order contains conflicting rank {rank}")]
    OrderConflict { rank: usize },
    #[error("element operation failed: {0}")]
    Operation(String),
}

/// A typed, configurable declaration consumed once by the framework.
///
/// Native and attribute builders should return `Self`, so they can be interleaved
/// without changing the declaration type. [`into_parts`](Self::into_parts) separates
/// attributes from persistent behavior; it transfers owned data only and must not
/// mount, run factories, advance deferred child sources, subscribe or issue commands.
///
/// Reconciliation first matches a child within its parent's direct set, then checks
/// the actual [`Mounted`](Self::Mounted) type. A different declaration wrapper can
/// retain the same identity and State when it produces that same mounted type.
/// Sharing only a State type does not make different mounted types compatible.
/// Public declarations must expose publicly reachable mounted and State types.
pub trait Element: Sized + 'static {
    /// Persistent behavior whose concrete type determines replacement compatibility.
    type Mounted: MountedElement;

    fn attrs(&self) -> &Attrs;

    fn attrs_mut(&mut self) -> &mut Attrs;

    /// Moves the declaration into separate configuration and mounted behavior.
    fn into_parts(self) -> ElementParts<Self::Mounted>;
}

/// The owned configuration and behavior resulting from declaration consumption.
///
/// Attributes become the framework's canonical source; `element` becomes the actual
/// mounted behavior. State is initialized by [`MountedElement::mount`], not by
/// declaration normalization. This value is itself an [`Element`] with mounted type `E`.
pub struct ElementParts<E: MountedElement> {
    pub attrs: Attrs,
    pub element: E,
}

impl<E: MountedElement> Element for ElementParts<E> {
    type Mounted = E;

    fn attrs(&self) -> &Attrs {
        &self.attrs
    }

    fn attrs_mut(&mut self) -> &mut Attrs {
        &mut self.attrs
    }

    fn into_parts(self) -> Self {
        self
    }
}

/// Why the current behavior is receiving a configuration update.
///
/// This describes the current update, not a replay log or proof that retained State
/// still represents an earlier successful computation. Recovery is reported separately
/// by [`UpdateCx::recovery_required`] and must be checked before an unchanged fast path.
pub enum UpdateReason<'a, E: MountedElement> {
    /// The incoming behavior can compare the old behavior only during this callback.
    Replacement { previous: &'a E },
    /// Effective inherited configuration changed without replacing this behavior.
    ///
    /// `affects` summarizes processing flags, not the complete set of changed keys;
    /// a presence change can have empty flags. Query [`UpdateCx::property_changed`]
    /// when the distinction matters.
    Inherited { affects: Work },
}

/// The shared lifecycle, layout, interaction, and output protocol of a mounted element.
///
/// This value owns persistent native behavior, not a copy of declaration attributes.
/// Its State survives replacements of the same concrete mounted type. Read and write
/// framework properties, visibility and handlers through callback contexts; successful
/// runtime writes remain canonical until a later declaration successfully replaces
/// that source.
///
/// On compatible replacement, the incoming behavior updates the retained State while
/// [`UpdateReason::Replacement`] temporarily exposes the old behavior. Success commits
/// the new behavior and source. Failure rejects them and restores that node's source
/// from entry to the replacement attempt, including any earlier runtime overrides.
/// This is not a transaction over State, other nodes or external services. Runtime
/// writes outside a protected replacement also have no general callback rollback.
///
/// Layout failure restores the executing node's local layout bookkeeping and leaves
/// dependency/failure obligations unresolved. Errors or caught unwinds do not undo
/// retained State mutations, nested successful child edits or business effects. Recovery
/// must repair derived State and request necessary work, or remove the failed identity,
/// before current output can be published. Irrevocable cleanup failure is not a replayable
/// callback and may require closing and reconstructing the window.
pub trait MountedElement: Sized + 'static {
    /// Persistent state initialized at mount and retained across compatible replacements.
    ///
    /// Equality of this type alone is not replacement compatibility. The framework does
    /// not clone State to provide transactional rollback.
    type State: 'static;

    fn mount(&mut self, cx: &mut MountCx<'_>) -> Result<Self::State, ElementError>;

    /// Applies current configuration and requests outputs affected by own fields.
    ///
    /// Effective property processing flags are already queued before this callback.
    /// Overrides request additional work for native fields or retained State; canonical
    /// property values are read through `cx`, not from `previous` attributes.
    ///
    /// In recovery mode, previous fields are not proof that State still represents them.
    /// An unchanged fast path must check [`UpdateCx::recovery_required`] first. Equality
    /// cannot clear failure debt or new work issued during the callback.
    fn update(
        &mut self,
        _state: &mut Self::State,
        _reason: UpdateReason<'_, Self>,
        cx: &mut UpdateCx<'_>,
    ) -> Result<(), ElementError> {
        cx.invalidate(Work::all());
        Ok(())
    }

    /// Performs deferred CPU work or reconciles a complete direct child set.
    ///
    /// An [`UpdateCx::children`] scope describes the complete retained set; omitted
    /// children are retired on scope success. Failure preserves the prior membership
    /// but does not undo nested successful child updates. Consumed one-shot sources
    /// cannot be replayed by an unchanged retry; their owner must receive fresh input
    /// or be removed.
    fn prepare(
        &mut self,
        _state: &mut Self::State,
        _cx: &mut UpdateCx<'_>,
    ) -> Result<(), ElementError> {
        Ok(())
    }

    /// Measures and places direct children inside the supplied constraints.
    ///
    /// Successful layout invalidates own output even at unchanged size. Nested child
    /// measurement is synchronous and can recurse; the caller controls thread stack
    /// configuration. The framework does not impose an arbitrary element-depth limit.
    fn layout(
        &mut self,
        state: &mut Self::State,
        cx: &mut LayoutCx<'_>,
        constraints: Constraints,
    ) -> Result<Size, ElementError>;

    /// Handles a route phase; owned effects execute after the callback borrow ends.
    fn event(
        &self,
        _state: &mut Self::State,
        _cx: &mut EventCx<'_>,
        _event: &InputEvent,
    ) -> Propagation {
        Propagation::Continue
    }

    /// Records complete local presentation content; composition applies world clipping.
    fn scene(&self, _state: &Self::State, cx: &mut SceneCx<'_>) -> Result<(), ElementError> {
        let color = cx.get(Background);
        if color.a != 0.0 {
            cx.rect(Rect::new(Point::ZERO, cx.size()), color);
        }

        Ok(())
    }

    /// Supplies local semantic content; the framework applies participation and disabling.
    fn semantics(&self, _state: &Self::State, cx: &SemanticCx<'_>) -> SemanticContent {
        SemanticContent {
            name: cx.get_ref(Name).cloned(),
            ..SemanticContent::default()
        }
    }

    /// Refines a qualified broad-phase candidate in local coordinates.
    ///
    /// This callback does not override ancestor visibility or enabling. Its read-only
    /// configuration corresponds to the qualified candidate supplied by the framework.
    /// A world point already admitted by the half-open layout bounds is converted to
    /// local f32 coordinates; upper-edge subtraction rounding is clamped just inside
    /// `size` so the default rectangle test agrees with the world-space test.
    fn hit_test(
        &self,
        _state: &Self::State,
        _cx: &HitTestCx<'_>,
        size: Size,
        point: Point,
    ) -> bool {
        Rect::new(Point::ZERO, size).contains(point)
    }

    /// Releases logical resources after interaction cancellation, before State is dropped.
    fn unmount(
        &mut self,
        _state: &mut Self::State,
        _cx: &mut UnmountCx<'_>,
    ) -> Result<(), ElementError> {
        Ok(())
    }
}

/// Shared attribute builders that preserve the concrete declaration type.
pub trait ElementExt: Element {
    fn key(mut self, key: impl Into<Key>) -> Self {
        self.attrs_mut().key = Some(key.into());
        self
    }

    fn set<P: Property>(mut self, property: P, value: P::Value) -> Self {
        self.attrs_mut().local.set(property, value);
        self
    }

    fn on<K: EventKey>(mut self, _event: K, action: Action<K::Data>) -> Self {
        let handlers = &mut self.attrs_mut().handlers;
        let key = TypeId::of::<K>();
        if let Some((_, existing)) = handlers.iter_mut().find(|(kind, _)| *kind == key) {
            *existing = action.binding;
        } else {
            handlers.push((key, action.binding));
        }

        self
    }

    fn background(self, color: Color) -> Self {
        self.set(Background, color)
    }

    fn foreground(self, color: Color) -> Self {
        self.set(Foreground, color)
    }

    fn gap(self, value: f32) -> Self {
        self.set(Gap, value)
    }

    fn width(self, value: f32) -> Self {
        self.set(Width, value)
    }

    fn height(self, value: f32) -> Self {
        self.set(Height, value)
    }

    fn enabled(self, value: bool) -> Self {
        self.set(Enabled, value)
    }

    fn a11y_name(self, name: impl Into<Arc<str>>) -> Self {
        self.set(Name, name.into())
    }

    fn visibility(mut self, value: Visibility) -> Self {
        self.attrs_mut().visibility = value;
        self
    }
}

impl<D: Element> ElementExt for D {}

/// Initialization access after identity and canonical configuration have been allocated.
///
/// Getters resolve effective values and preserve explicit presence. Mutations write
/// the framework's canonical source and schedule their declared effects; they are
/// not changes to the consumed declaration. Property references are short borrows
/// of this context and cannot remain in use across its mutable operations.
pub struct MountCx<'a> {
    pub(crate) access: ElementAccess<'a>,
    pub(crate) current: ElementId,
}

impl MountCx<'_> {
    pub fn id(&self) -> ElementId {
        self.current
    }

    /// Reads the current canonical visibility, including during terminal cleanup.
    pub fn current_visibility(&self) -> Visibility {
        self.access.read(self.current).visibility()
    }

    pub fn get<P: Property>(&self, property: P) -> P::Value {
        self.access.read(self.current).get(property)
    }

    /// Borrows an explicitly provided effective value without cloning or using a default.
    ///
    /// The reference is tied to this context borrow. Finish using it before calling
    /// a mutable context method; clone the value or a shared handle when it must survive.
    pub fn get_ref<P: Property>(&self, property: P) -> Option<&P::Value> {
        self.access.read(self.current).get_ref(property)
    }

    pub fn set<P: Property>(&mut self, property: P, value: P::Value) -> Result<(), ElementError> {
        self.access.set(self.current, property, value)
    }

    pub fn remove<P: Property>(&mut self, property: P) -> Result<(), ElementError> {
        self.access.remove(self.current, property)
    }

    pub fn visibility(&mut self, value: Visibility) -> Result<(), ElementError> {
        self.access.set_visibility(self.current, value)
    }

    pub fn on<K: EventKey>(
        &mut self,
        event: K,
        action: Action<K::Data>,
    ) -> Result<(), ElementError> {
        self.access.bind(self.current, event, action)
    }
}

/// Configuration and preparation capabilities limited to the executing element.
///
/// Reads use current effective configuration, including runtime overrides. Changes
/// reported for this callback are value or presence changes; they are not a replayable
/// declaration or a complete history of mutations. [`recovery_required`](Self::recovery_required)
/// is independent of equality and processing flags.
pub struct UpdateCx<'a> {
    pub(crate) access: ElementAccess<'a>,
    pub(crate) current: ElementId,
    pub(crate) changes: &'a [PropertyChange],
    pub(crate) recovery: bool,
}

impl UpdateCx<'_> {
    pub fn id(&self) -> ElementId {
        self.current
    }

    /// Reads the current canonical visibility, including during terminal cleanup.
    pub fn current_visibility(&self) -> Visibility {
        self.access.read(self.current).visibility()
    }

    pub fn get<P: Property>(&self, property: P) -> P::Value {
        self.access.read(self.current).get(property)
    }

    /// Borrows an explicitly provided effective value without cloning or using a default.
    ///
    /// The reference is tied to this context borrow. Finish using it before calling
    /// a mutable context method; clone the value or a shared handle when it must survive.
    pub fn get_ref<P: Property>(&self, property: P) -> Option<&P::Value> {
        self.access.read(self.current).get_ref(property)
    }

    pub fn set<P: Property>(&mut self, property: P, value: P::Value) -> Result<(), ElementError> {
        self.access.set(self.current, property, value)
    }

    pub fn remove<P: Property>(&mut self, property: P) -> Result<(), ElementError> {
        self.access.remove(self.current, property)
    }

    pub fn visibility(&mut self, value: Visibility) -> Result<(), ElementError> {
        self.access.set_visibility(self.current, value)
    }

    pub fn on<K: EventKey>(
        &mut self,
        event: K,
        action: Action<K::Data>,
    ) -> Result<(), ElementError> {
        self.access.bind(self.current, event, action)
    }

    /// Tests whether this callback's effective value or presence changed for a property.
    ///
    /// This can be true even when [`property_changes`](Self::property_changes) is empty.
    pub fn property_changed<P: Property>(&self, _property: P) -> bool {
        self.changes
            .iter()
            .any(|change| change.key == TypeId::of::<P>())
    }

    pub fn property_changes(&self) -> Work {
        self.changes
            .iter()
            .fold(Work::empty(), |work, change| work | change.affects)
    }

    /// Reports unresolved update or stage failure at entry to this attempt.
    ///
    /// It is not cleared by unchanged inputs. Derived State may be partially mutated;
    /// repair it and request required work before taking a cached no-op path.
    pub fn recovery_required(&self) -> bool {
        self.recovery
    }

    pub fn invalidate(&mut self, work: Work) {
        self.access.request(self.current, work);
    }

    pub fn invalidate_hit_test(&mut self) {
        self.access.invalidate_hit_test(self.current);
    }

    pub fn children<R>(
        &mut self,
        process: impl FnOnce(&mut ChildrenCx<'_>) -> Result<R, ElementError>,
    ) -> Result<R, ElementError> {
        children::edit(&mut self.access, self.current, process)
    }
}

/// Cleanup access while the retired body's configuration and last geometry still exist.
///
/// Configuration is read-only and reflects the terminal source, not historical
/// declaration attributes. Borrowed property values have that cleanup scope's lifetime;
/// output-independent references cannot be retained after it ends.
pub struct UnmountCx<'a> {
    pub(crate) current: ElementId,
    pub(crate) configuration: ConfigRead<'a>,
    pub(crate) geometry: Option<Geometry>,
}

impl<'a> UnmountCx<'a> {
    pub fn id(&self) -> ElementId {
        self.current
    }

    /// Reads the current canonical visibility, including during terminal cleanup.
    pub fn current_visibility(&self) -> Visibility {
        self.configuration.visibility()
    }

    pub fn get<P: Property>(&self, property: P) -> P::Value {
        self.configuration.get(property)
    }

    /// Borrows a provided effective value for the read-only cleanup configuration lifetime.
    pub fn get_ref<P: Property>(&self, property: P) -> Option<&'a P::Value> {
        self.configuration.get_ref(property)
    }

    pub fn last_geometry(&self) -> Option<Geometry> {
        self.geometry
    }
}
