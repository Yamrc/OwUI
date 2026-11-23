//! Stateful view boundaries, typed actions, and lifecycle-owned subscriptions.
//!
//! A [`View`] keeps business state and dependencies in ordinary Rust fields. Its
//! build declares elements and binds actions; [`InitCx`] and [`ActionCx`] own
//! subscription operations outside construction. Keyed child views retain their
//! concrete View state while each declaration's factory/configuration is consumed
//! at most once. [`BuildCx::view_with`] defines that application boundary.
//!
//! [`Invalidator`] queues coalesced hints for a live scope, not immediate callbacks
//! or business commands. The host-driven window admits its Inbox at most once per
//! logical update cycle; hints produced later are left for a later cycle. Keeping
//! a hint or an [`Action`] does not keep the target view or subscription alive.

use std::any::Any;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use crate::element::{Attrs, Element, ElementError, ElementId, Key, MountCx, UpdateCx};
use crate::input::InputRequest;
use crate::layout::{Constraints, LayoutCx, Point, Size};
use crate::schedule::Work;

/// A persistent UI object that declares its current element hierarchy.
///
/// Business dependencies and local state remain ordinary fields. [`build`](Self::build)
/// constructs a declaration from the current state; it must not issue business
/// commands or register subscriptions. Typed [`BuildCx::action`] callbacks mutate
/// the live View after the originating element borrow ends and can request a local
/// rebuild without rebuilding its parent.
///
/// Key and concrete View type determine retention of a child boundary. Factory
/// captures are initial input, not an automatic field synchronization mechanism;
/// use [`BuildCx::view_with`] for subsequent configuration. Failed builds/configuration
/// do not roll back View fields, successful nested edits or external effects.
pub trait View: Sized + 'static {
    fn build(&self, cx: &BuildCx<'_, Self>) -> impl Element;
}

/// Whether an action requests another build of its owning view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rebuild {
    None,
    This,
}

struct WakeState {
    live: AtomicBool,
    queued: AtomicBool,
}

struct Lifetime(Arc<WakeState>);

impl Lifetime {
    fn new() -> Self {
        Self(Arc::new(WakeState {
            live: AtomicBool::new(true),
            queued: AtomicBool::new(false),
        }))
    }
}

impl Drop for Lifetime {
    fn drop(&mut self) {
        self.0.live.store(false, Ordering::Release);
    }
}

pub(crate) struct Wake {
    pub owner: ElementId,
    state: Arc<WakeState>,
}

impl Wake {
    pub fn accept(&self) -> bool {
        self.state.queued.swap(false, Ordering::AcqRel);
        self.state.live.load(Ordering::Acquire)
    }
}

#[derive(Default)]
pub(crate) struct Inbox {
    queue: Mutex<VecDeque<Wake>>,
    pending: AtomicBool,
}

impl Inbox {
    pub fn drain(&self) -> VecDeque<Wake> {
        if !self.pending.swap(false, Ordering::AcqRel) {
            return VecDeque::new();
        }

        let mut queue = self.queue.lock().unwrap_or_else(PoisonError::into_inner);
        std::mem::take(&mut *queue)
    }
}

/// A thread-safe, coalescing request to rebuild one live view.
///
/// Requests are hints admitted by a host-driven update cycle, not synchronous builds.
/// Repeated requests for the same lifecycle scope coalesce while its hint is queued;
/// different subscription scopes can independently target the same view. Liveness is
/// checked again at admission, so a queued hint can be discarded after retirement.
/// Keeping this value does not extend the view or subscription lifetime.
#[derive(Clone)]
pub struct Invalidator {
    owner: ElementId,
    state: Arc<WakeState>,
    inbox: Arc<Inbox>,
}

impl Invalidator {
    /// Enqueues a hint if its lifecycle scope is still live.
    pub fn request(&self) {
        if !self.state.live.load(Ordering::Acquire) {
            return;
        }
        if self.state.queued.swap(true, Ordering::AcqRel) {
            return;
        }

        let mut queue = self
            .inbox
            .queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        queue.push_back(Wake {
            owner: self.owner,
            state: self.state.clone(),
        });
        self.inbox.pending.store(true, Ordering::Release);
    }

    /// Reports current scope liveness without keeping it live or authorizing a queued hint.
    pub fn is_live(&self) -> bool {
        self.state.live.load(Ordering::Acquire)
    }
}

struct Link {
    key: Key,
    _life: Lifetime,
    _guard: Box<dyn Any>,
}

#[derive(Default)]
pub(crate) struct Links {
    entries: Vec<Link>,
}

impl Links {
    fn try_watch<G: 'static, E>(
        &mut self,
        key: Key,
        owner: ElementId,
        inbox: &Arc<Inbox>,
        connect: impl FnOnce(Invalidator) -> Result<G, E>,
    ) -> Result<(), E> {
        let life = Lifetime::new();
        let guard = connect(Invalidator {
            owner,
            state: life.0.clone(),
            inbox: inbox.clone(),
        })?;

        let next = Link {
            key,
            _life: life,
            _guard: Box::new(guard),
        };
        if let Some(index) = self.entries.iter().position(|link| link.key == next.key) {
            self.entries.remove(index);
        }
        self.entries.push(next);

        Ok(())
    }

    fn watch<G: 'static>(
        &mut self,
        key: Key,
        owner: ElementId,
        inbox: &Arc<Inbox>,
        connect: impl FnOnce(Invalidator) -> G,
    ) {
        if let Some(index) = self.entries.iter().position(|link| link.key == key) {
            self.entries.remove(index);
        }

        let life = Lifetime::new();
        let guard = connect(Invalidator {
            owner,
            state: life.0.clone(),
            inbox: inbox.clone(),
        });

        self.entries.push(Link {
            key,
            _life: life,
            _guard: Box::new(guard),
        });
    }
}

/// View initialization access during one new mounted identity's factory call.
///
/// The factory is consumed once. Subscriptions installed here belong to the view's
/// lifecycle rather than a build or declaration. Factory failure cannot replay the
/// consumed closure; a new declaration/identity must supply fresh initialization.
pub struct InitCx<'a, V: View> {
    id: ElementId,
    inbox: &'a Arc<Inbox>,
    life: &'a Lifetime,
    links: &'a mut Links,
    marker: PhantomData<fn() -> V>,
}

impl<V: View> InitCx<'_, V> {
    pub fn id(&self) -> ElementId {
        self.id
    }

    pub fn invalidator(&self) -> Invalidator {
        Invalidator {
            owner: self.id,
            state: self.life.0.clone(),
            inbox: self.inbox.clone(),
        }
    }

    /// Retains a guard for a subscription until replacement or view retirement.
    ///
    /// The guard's destructor must disconnect its source. Reusing a key invalidates
    /// the old scope before dropping its guard, then calls `connect`; failure/unwind
    /// is not transactional restoration of that previous connection. The supplied
    /// invalidator belongs to this subscription scope, not to the view's whole lifetime.
    pub fn watch<G: 'static>(
        &mut self,
        key: impl Into<Key>,
        connect: impl FnOnce(Invalidator) -> G,
    ) {
        self.links.watch(key.into(), self.id, self.inbox, connect);
    }

    /// Replaces a subscription only after connecting the new source succeeds.
    ///
    /// On failure the previous connection remains active, and the attempted
    /// connection's invalidator becomes inactive. The source error is returned
    /// unchanged. The old and new connections can briefly overlap on success.
    pub fn try_watch<G: 'static, E>(
        &mut self,
        key: impl Into<Key>,
        connect: impl FnOnce(Invalidator) -> Result<G, E>,
    ) -> Result<(), E> {
        self.links
            .try_watch(key.into(), self.id, self.inbox, connect)
    }
}

/// Read-only construction capabilities for one view build.
///
/// Creating an Action or child-view declaration only transfers owned captures; it
/// does not execute the callback, factory or configuration function. Captures must
/// be suitable for later application, not borrows of the current build.
pub struct BuildCx<'a, V: View> {
    id: ElementId,
    marker: PhantomData<&'a V>,
}

impl<V: View> BuildCx<'_, V> {
    pub fn id(&self) -> ElementId {
        self.id
    }

    /// Binds a callback to this view's identity without keeping the view alive.
    ///
    /// Captures must outlive the build call. Captured values are not refreshed by
    /// later builds; read changing view data through the callback's `this` argument.
    /// The callback executes after the originating element borrow has ended.
    pub fn action<D: 'static>(
        &self,
        callback: impl FnMut(&mut V, &mut ActionCx<'_, V>, &D) -> Rebuild + 'static,
    ) -> Action<D> {
        Action {
            binding: ActionBinding {
                owner: self.id,
                callback: Rc::new(Callback::<V, D, _> {
                    function: RefCell::new(callback),
                    marker: PhantomData,
                }),
            },
            marker: PhantomData,
        }
    }

    /// Declares an independently rebuildable child view.
    ///
    /// The factory runs only for a new identity of this key and concrete View type.
    /// Compatible retained instances discard the new factory without invoking it;
    /// captured data therefore does not refresh their fields. Use
    /// [`view_with`](Self::view_with) for configuration on later declarations.
    /// Normalization and the parent's build do not execute the factory.
    pub fn view<C: View>(
        &self,
        key: impl Into<Key>,
        create: impl FnOnce(&mut InitCx<'_, C>) -> C + 'static,
    ) -> impl Element {
        ViewElement::new(create).with_key(key.into())
    }

    /// Configures a retained child when this declaration is applied.
    ///
    /// New instances run `create`, then `configure`, then their first build.
    /// Compatible retained instances discard `create` and run only this declaration's
    /// `configure`; [`Rebuild::This`] requests another child build. Neither callback
    /// runs inside the parent's build or declaration normalization.
    ///
    /// Each closure is consumed once. Failure/unwind after consumption cannot replay
    /// it through an unchanged retry; fresh configuration requires a fresh declaration.
    /// Successful configuration is not repeated merely because a later build fails.
    /// View fields, installed subscriptions, nested successes and external effects are
    /// not a general rollback transaction; unresolved failure still gates publication.
    pub fn view_with<C: View>(
        &self,
        key: impl Into<Key>,
        create: impl FnOnce(&mut InitCx<'_, C>) -> C + 'static,
        configure: impl FnOnce(&mut C, &mut ActionCx<'_, C>) -> Rebuild + 'static,
    ) -> impl Element {
        let mut element = ViewElement::new(create).with_key(key.into());
        element.body.configure = Initializer::Pending(Box::new(configure));
        element
    }
}

/// Capabilities available while mutating one view through an action or configuration.
///
/// This scope can own subscriptions and defer focus requests; it does not provide
/// unrestricted mutable access to the element hierarchy. Business state changes
/// remain ordinary mutations of the callback's typed View value.
pub struct ActionCx<'a, V: View> {
    id: ElementId,
    inbox: &'a Arc<Inbox>,
    life: &'a Lifetime,
    links: &'a mut Links,
    requests: &'a mut Vec<InputRequest>,
    marker: PhantomData<fn() -> V>,
}

impl<V: View> ActionCx<'_, V> {
    pub fn id(&self) -> ElementId {
        self.id
    }

    pub fn invalidator(&self) -> Invalidator {
        Invalidator {
            owner: self.id,
            state: self.life.0.clone(),
            inbox: self.inbox.clone(),
        }
    }

    /// Replaces a lifecycle-owned subscription, disconnecting the old key before connecting.
    ///
    /// This has the same guard/liveness contract as [`InitCx::watch`]. Use
    /// [`try_watch`](Self::try_watch) when a failed connection must retain the old one.
    pub fn watch<G: 'static>(
        &mut self,
        key: impl Into<Key>,
        connect: impl FnOnce(Invalidator) -> G,
    ) {
        self.links.watch(key.into(), self.id, self.inbox, connect);
    }

    /// Replaces a subscription only after connecting the new source succeeds.
    ///
    /// On failure the previous connection remains active, and the attempted
    /// connection's invalidator becomes inactive. The source error is returned
    /// unchanged. The old and new connections can briefly overlap on success.
    pub fn try_watch<G: 'static, E>(
        &mut self,
        key: impl Into<Key>,
        connect: impl FnOnce(Invalidator) -> Result<G, E>,
    ) -> Result<(), E> {
        self.links
            .try_watch(key.into(), self.id, self.inbox, connect)
    }

    /// Requests focus after the current callback. Inactive or foreign targets are ignored.
    pub fn focus(&mut self, element: ElementId) {
        self.requests.push(InputRequest::Focus(Some(element)));
    }

    pub fn clear_focus(&mut self) {
        self.requests.push(InputRequest::Focus(None));
    }
}

/// A reusable typed callback targeting a particular live view instance.
///
/// Clones can be installed on different elements. Replacing or removing one
/// installation invalidates its queued events, not the other installations.
pub struct Action<D: 'static> {
    pub(crate) binding: ActionBinding,
    marker: PhantomData<fn(&D)>,
}

impl<D: 'static> Clone for Action<D> {
    fn clone(&self) -> Self {
        Self {
            binding: self.binding.clone(),
            marker: PhantomData,
        }
    }
}

#[derive(Clone)]
pub(crate) struct ActionBinding {
    pub owner: ElementId,
    callback: Rc<dyn ActionCall>,
}

pub(crate) struct PendingAction {
    pub source: ElementId,
    pub binding: ActionBinding,
    pub data: Box<dyn Any>,
}

pub(crate) struct ActionEnv<'a> {
    pub owner: ElementId,
    pub inbox: &'a Arc<Inbox>,
    pub requests: &'a mut Vec<InputRequest>,
}

impl ActionBinding {
    pub(crate) fn same_registration(&self, other: &Self) -> bool {
        self.owner == other.owner && Rc::ptr_eq(&self.callback, &other.callback)
    }

    pub fn call(
        &self,
        state: &mut dyn Any,
        env: &mut ActionEnv<'_>,
        data: &dyn Any,
    ) -> Result<Rebuild, ElementError> {
        self.callback.call(state, env, data)
    }
}

trait ActionCall {
    fn call(
        &self,
        state: &mut dyn Any,
        env: &mut ActionEnv<'_>,
        data: &dyn Any,
    ) -> Result<Rebuild, ElementError>;
}

struct Callback<V, D, F> {
    function: RefCell<F>,
    marker: PhantomData<fn(V, D)>,
}

impl<V, D, F> ActionCall for Callback<V, D, F>
where
    V: View,
    D: 'static,
    F: FnMut(&mut V, &mut ActionCx<'_, V>, &D) -> Rebuild + 'static,
{
    fn call(
        &self,
        state: &mut dyn Any,
        env: &mut ActionEnv<'_>,
        data: &dyn Any,
    ) -> Result<Rebuild, ElementError> {
        let state = state
            .downcast_mut::<ViewState<V>>()
            .ok_or(ElementError::TypeMismatch)?;
        let data = data.downcast_ref::<D>().ok_or(ElementError::TypeMismatch)?;
        let mut function = self
            .function
            .try_borrow_mut()
            .map_err(|_| ElementError::Reentrant(env.owner))?;

        let mut cx = ActionCx {
            id: env.owner,
            inbox: env.inbox,
            life: &state.life,
            links: &mut state.links,
            requests: env.requests,
            marker: PhantomData,
        };
        Ok(function(&mut state.value, &mut cx, data))
    }
}

type Factory<V> = Box<dyn FnOnce(&mut InitCx<'_, V>) -> V>;
type Configure<V> = Box<dyn FnOnce(&mut V, &mut ActionCx<'_, V>) -> Rebuild>;

enum Initializer<T> {
    Pending(T),
    Applied,
    Failed,
}

pub(crate) struct ViewElement<V: View> {
    attrs: Attrs,
    body: MountedView<V>,
}

impl<V: View> ViewElement<V> {
    pub fn new(create: impl FnOnce(&mut InitCx<'_, V>) -> V + 'static) -> Self {
        Self {
            attrs: Attrs::new(),
            body: MountedView {
                create: Initializer::Pending(Box::new(create)),
                configure: Initializer::Applied,
            },
        }
    }

    fn with_key(mut self, key: Key) -> Self {
        self.attrs.key = Some(key);
        self
    }
}

impl<V: View> Element for ViewElement<V> {
    type Mounted = MountedView<V>;

    fn attrs(&self) -> &Attrs {
        &self.attrs
    }

    fn attrs_mut(&mut self) -> &mut Attrs {
        &mut self.attrs
    }

    fn into_parts(self) -> crate::element::ElementParts<Self::Mounted> {
        crate::element::ElementParts {
            attrs: self.attrs,
            element: self.body,
        }
    }
}

/// Mounted behavior of an independently rebuildable View boundary.
#[doc(hidden)]
pub struct MountedView<V: View> {
    create: Initializer<Factory<V>>,
    configure: Initializer<Configure<V>>,
}

/// Retained business state and lifecycle scope of a View boundary.
#[doc(hidden)]
pub struct ViewState<V: View> {
    pub(crate) value: V,
    life: Lifetime,
    links: Links,
}

impl<V: View> Drop for ViewState<V> {
    fn drop(&mut self) {
        self.life.0.live.store(false, Ordering::Release);
        for link in &self.links.entries {
            link._life.0.live.store(false, Ordering::Release);
        }
    }
}

impl<V: View> MountedView<V> {
    fn configure(
        &mut self,
        state: &mut ViewState<V>,
        cx: &mut UpdateCx<'_>,
    ) -> Result<Rebuild, ElementError> {
        let configure = match std::mem::replace(&mut self.configure, Initializer::Failed) {
            Initializer::Pending(configure) => configure,
            Initializer::Applied => {
                self.configure = Initializer::Applied;
                return Ok(Rebuild::None);
            }
            Initializer::Failed => return Err(ElementError::InitializerConsumed),
        };

        let mut action_cx = ActionCx {
            id: cx.id(),
            inbox: cx.access.inbox,
            life: &state.life,
            links: &mut state.links,
            requests: cx.access.requests,
            marker: PhantomData,
        };
        let rebuild = configure(&mut state.value, &mut action_cx);

        self.configure = Initializer::Applied;
        Ok(rebuild)
    }
}

impl<V: View> crate::element::MountedElement for MountedView<V> {
    type State = ViewState<V>;

    fn mount(&mut self, cx: &mut MountCx<'_>) -> Result<Self::State, ElementError> {
        let create = match std::mem::replace(&mut self.create, Initializer::Failed) {
            Initializer::Pending(create) => create,
            _ => return Err(ElementError::InitializerConsumed),
        };

        let life = Lifetime::new();
        let mut links = Links::default();
        let value = {
            let mut init = InitCx {
                id: cx.id(),
                inbox: cx.access.inbox,
                life: &life,
                links: &mut links,
                marker: PhantomData,
            };
            create(&mut init)
        };

        self.create = Initializer::Applied;
        Ok(ViewState { value, life, links })
    }

    fn update(
        &mut self,
        state: &mut Self::State,
        reason: crate::element::UpdateReason<'_, Self>,
        cx: &mut UpdateCx<'_>,
    ) -> Result<(), ElementError> {
        if matches!(reason, crate::element::UpdateReason::Replacement { .. }) {
            self.create = Initializer::Applied;
            if self.configure(state, cx)? == Rebuild::This {
                cx.invalidate(Work::PREPARE);
            }
        }

        if cx.recovery_required() {
            cx.invalidate(Work::PREPARE);
        }
        Ok(())
    }

    fn prepare(
        &mut self,
        state: &mut Self::State,
        cx: &mut UpdateCx<'_>,
    ) -> Result<(), ElementError> {
        self.configure(state, cx)?;

        let build = BuildCx {
            id: cx.id(),
            marker: PhantomData,
        };
        let declaration = state.value.build(&build);

        cx.children(|children| {
            children.realize(0_u64, declaration)?;
            Ok(())
        })
    }

    fn layout(
        &mut self,
        _: &mut Self::State,
        cx: &mut LayoutCx<'_>,
        constraints: Constraints,
    ) -> Result<Size, ElementError> {
        if let Some(child) = cx.child(0) {
            let size = cx.measure(child, constraints)?;
            cx.place(child, Point::ZERO)?;
            Ok(constraints.constrain(size))
        } else {
            Ok(constraints.min)
        }
    }

    fn unmount(
        &mut self,
        state: &mut Self::State,
        _: &mut crate::element::UnmountCx<'_>,
    ) -> Result<(), ElementError> {
        state.life.0.live.store(false, Ordering::Release);
        for link in &state.links.entries {
            link._life.0.live.store(false, Ordering::Release);
        }

        state.links.entries.clear();
        Ok(())
    }
}

/// Executes one already source-bound Action after source borrowing has ended.
pub(crate) fn run_action(
    access: &mut crate::element::mounted::ElementAccess<'_>,
    action: PendingAction,
) -> Result<(), ElementError> {
    if !access.elements.contains(action.source) || !access.elements.contains(action.binding.owner) {
        access.work.counts.retired_requests = access.work.counts.retired_requests.saturating_add(1);
        return Ok(());
    }

    let owner = action.binding.owner;
    let rebuild = {
        let body = access
            .elements
            .body_mut(owner.local())
            .ok_or(ElementError::Reentrant(owner))?;
        let mut env = ActionEnv {
            owner,
            inbox: access.inbox,
            requests: access.requests,
        };
        action
            .binding
            .call(body.state_mut(), &mut env, action.data.as_ref())?
    };

    access.work.counts.actions = access.work.counts.actions.saturating_add(1);
    if rebuild == Rebuild::This {
        access.request(owner, Work::PREPARE);
    }

    Ok(())
}
