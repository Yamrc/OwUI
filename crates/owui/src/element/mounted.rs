//! Mounted body ownership, compact identity rows, and the unique logical topology.

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::Arc;

use crate::id::{LocalKey, Slots};
use crate::input::{
    EventCx, EventKey, HitTestCx, InputEvent, InputRequest, Propagation, SemanticContent,
    SemanticCx,
};
use crate::layout::{Constraints, Layout, LayoutCx, Point, Size};
use crate::scene::SceneCx;
use crate::schedule::{Scheduler, Work};
use crate::view::{Action, Inbox};

use super::call::{CallFrame, drop_committed};
use super::children::ChildSet;
use super::configuration::{ConfigRead, Configuration, Environment};
use super::property::PropertyChange;
use super::{
    Element, ElementError, ElementId, ElementParts, MountCx, MountedElement, Property, UnmountCx,
    UpdateCx, UpdateReason, Visibility,
};

/// A temporary dense location, never an external or queued identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Row(pub(crate) usize);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Life {
    Mounting,
    Live,
    Retiring,
}

pub(crate) trait ElementCall: Any {
    fn kind(&self) -> TypeId;

    fn state_mut(&mut self) -> &mut dyn Any;

    fn update_inherited(
        &mut self,
        cx: &mut UpdateCx<'_>,
        affects: Work,
    ) -> Result<(), ElementError>;

    fn prepare(&mut self, cx: &mut UpdateCx<'_>) -> Result<(), ElementError>;

    fn layout(
        &mut self,
        cx: &mut LayoutCx<'_>,
        constraints: Constraints,
    ) -> Result<Size, ElementError>;

    fn event(&mut self, cx: &mut EventCx<'_>, event: &InputEvent) -> Propagation;

    fn scene(&self, cx: &mut SceneCx<'_>) -> Result<(), ElementError>;

    fn semantics(&self, cx: &SemanticCx<'_>) -> SemanticContent;

    fn hit_test(&self, cx: &HitTestCx<'_>, size: Size, point: Point) -> bool;

    fn unmount(&mut self, cx: &mut UnmountCx<'_>) -> Result<(), ElementError>;
}

pub(crate) struct Entry<E: MountedElement> {
    pub(crate) element: E,
    pub(crate) state: E::State,
}

impl<E: MountedElement> ElementCall for Entry<E> {
    fn kind(&self) -> TypeId {
        TypeId::of::<E>()
    }

    fn state_mut(&mut self) -> &mut dyn Any {
        &mut self.state
    }

    fn update_inherited(
        &mut self,
        cx: &mut UpdateCx<'_>,
        affects: Work,
    ) -> Result<(), ElementError> {
        self.element.update(
            &mut self.state,
            super::UpdateReason::Inherited { affects },
            cx,
        )
    }

    fn prepare(&mut self, cx: &mut UpdateCx<'_>) -> Result<(), ElementError> {
        self.element.prepare(&mut self.state, cx)
    }

    fn layout(
        &mut self,
        cx: &mut LayoutCx<'_>,
        constraints: Constraints,
    ) -> Result<Size, ElementError> {
        self.element.layout(&mut self.state, cx, constraints)
    }

    fn event(&mut self, cx: &mut EventCx<'_>, event: &InputEvent) -> Propagation {
        self.element.event(&mut self.state, cx, event)
    }

    fn scene(&self, cx: &mut SceneCx<'_>) -> Result<(), ElementError> {
        self.element.scene(&self.state, cx)
    }

    fn semantics(&self, cx: &SemanticCx<'_>) -> SemanticContent {
        self.element.semantics(&self.state, cx)
    }

    fn hit_test(&self, cx: &HitTestCx<'_>, size: Size, point: Point) -> bool {
        self.element.hit_test(&self.state, cx, size, point)
    }

    fn unmount(&mut self, cx: &mut UnmountCx<'_>) -> Result<(), ElementError> {
        self.element.unmount(&mut self.state, cx)
    }
}

pub(crate) struct Node {
    pub(crate) key: LocalKey,
    pub(crate) parent: Option<LocalKey>,
    pub(crate) children: Option<NonZeroUsize>,
    pub(crate) position: usize,
    pub(crate) rank: usize,
    pub(crate) depth: u32,
    pub(crate) life: Life,
    pub(crate) body: Option<Box<dyn ElementCall>>,
}

/// Only identity, body, and topology live here; scheduling and outputs have separate owners.
pub(crate) struct MountedElements {
    identities: Slots<ElementId, Row>,
    pub(crate) nodes: Vec<Node>,
    pub(crate) children: Vec<ChildSet>,
    pub(crate) child_scratch: Vec<super::children::ChildScratch>,
    pub(crate) order: Vec<LocalKey>,
    pub(crate) order_from: Option<usize>,
    pub(crate) retired: Vec<LocalKey>,
    pub(crate) frames: usize,
    live: usize,
    active: HashMap<LocalKey, usize>,
    retire_scratch: Vec<LocalKey>,
    order_scratch: Vec<LocalKey>,
}

impl MountedElements {
    pub(crate) fn new() -> Self {
        Self {
            identities: Slots::with_key(),
            nodes: Vec::new(),
            children: Vec::new(),
            child_scratch: Vec::new(),
            order: Vec::new(),
            order_from: Some(0),
            retired: Vec::new(),
            frames: 0,
            live: 0,
            active: HashMap::new(),
            retire_scratch: Vec::new(),
            order_scratch: Vec::new(),
        }
    }

    pub(crate) fn id(&self, key: LocalKey) -> ElementId {
        self.identities.qualify(key)
    }

    pub(crate) fn row(&self, id: ElementId) -> Option<Row> {
        self.identities.get(id).copied()
    }

    pub(crate) fn row_local(&self, key: LocalKey) -> Option<Row> {
        self.identities.get_local(key).copied()
    }

    pub(crate) fn contains(&self, id: ElementId) -> bool {
        self.row(id)
            .is_some_and(|row| self.nodes[row.0].life == Life::Live)
    }

    pub(crate) fn contains_local(&self, key: LocalKey) -> bool {
        self.row_local(key)
            .is_some_and(|row| self.nodes[row.0].life == Life::Live)
    }

    pub(crate) fn len(&self) -> usize {
        self.live
    }

    pub(crate) fn parent(&self, key: LocalKey) -> Option<LocalKey> {
        self.row_local(key).and_then(|row| self.nodes[row.0].parent)
    }

    pub(crate) fn rank(&self, key: LocalKey) -> Option<usize> {
        let row = self.row_local(key)?;
        let node = &self.nodes[row.0];
        (node.life == Life::Live && node.rank != usize::MAX).then_some(node.rank)
    }

    pub(crate) fn depth(&self, key: LocalKey) -> Option<u32> {
        self.row_local(key).map(|row| self.nodes[row.0].depth)
    }

    pub(crate) fn child_keys(&self, key: LocalKey) -> impl Iterator<Item = LocalKey> + '_ {
        self.row_local(key)
            .and_then(|row| self.nodes[row.0].children)
            .map(|slot| &self.children[slot.get() - 1])
            .into_iter()
            .flat_map(|set| set.ordered.iter().map(|child| child.key))
    }

    pub(crate) fn child_count(&self, key: LocalKey) -> usize {
        self.row_local(key)
            .and_then(|row| self.nodes[row.0].children)
            .map_or(0, |index| self.children[index.get() - 1].ordered.len())
    }

    pub(crate) fn child_at(&self, key: LocalKey, index: usize) -> Option<ElementId> {
        let row = self.row_local(key)?;
        let set = self.nodes[row.0].children?;
        self.children[set.get() - 1]
            .ordered
            .get(index)
            .map(|child| self.id(child.key))
    }

    pub(crate) fn find_child(&self, parent: ElementId, key: &super::Key) -> Option<ElementId> {
        let row = self.row(parent)?;
        let set = self.nodes[row.0].children?;
        let index = self.children[set.get() - 1].find(key)?;
        Some(self.id(self.children[set.get() - 1].ordered[index].key))
    }

    pub(crate) fn check_child(
        &self,
        parent: ElementId,
        child: ElementId,
    ) -> Result<Row, ElementError> {
        let row = self.row(child).ok_or(ElementError::Missing(child))?;
        if self.nodes[row.0].life != Life::Live {
            return Err(ElementError::Missing(child));
        }
        if !self.contains(parent) || self.nodes[row.0].parent != Some(parent.local()) {
            return Err(ElementError::WrongParent { parent, child });
        }

        Ok(row)
    }

    pub(crate) fn allocate(&mut self, parent: Option<LocalKey>) -> Result<ElementId, ElementError> {
        let depth = match parent {
            Some(parent) => self
                .depth(parent)
                .ok_or_else(|| ElementError::Missing(self.id(parent)))?
                .checked_add(1)
                .ok_or(ElementError::DepthExhausted)?,
            None => 0,
        };

        let row = Row(self.nodes.len());
        let rollback_nodes = self.nodes.len() + 1;
        self.retired
            .reserve(rollback_nodes.saturating_sub(self.retired.len()));
        self.retire_scratch.reserve(rollback_nodes);

        let id = self.identities.insert(row);
        self.nodes.push(Node {
            key: id.local(),
            parent,
            children: None,
            position: 0,
            rank: usize::MAX,
            depth,
            life: Life::Mounting,
            body: None,
        });

        Ok(id)
    }

    pub(crate) fn set_body(&mut self, key: LocalKey, body: Box<dyn ElementCall>) {
        let row = self
            .row_local(key)
            .expect("allocated body has a primary identity row");

        self.nodes[row.0].body = Some(body);
        if self.nodes[row.0].life != Life::Live {
            self.live += 1;
        }
        self.nodes[row.0].life = Life::Live;
    }

    pub(crate) fn body(&self, key: LocalKey) -> Option<&dyn ElementCall> {
        self.row_local(key)
            .and_then(|row| self.nodes[row.0].body.as_deref())
    }

    pub(crate) fn body_mut(&mut self, key: LocalKey) -> Option<&mut dyn ElementCall> {
        let row = self.row_local(key)?;
        match self.nodes[row.0].body.as_mut() {
            Some(body) => Some(body.as_mut()),
            None => None,
        }
    }

    pub(crate) fn after_subtree(&self, key: LocalKey) -> usize {
        let mut current = key;
        while let Some(parent) = self.parent(current) {
            let parent_row = self
                .row_local(parent)
                .expect("retained subtree ancestry has identity rows");
            if let Some(slot) = self.nodes[parent_row.0].children {
                let current_row = self
                    .row_local(current)
                    .expect("retained subtree member has an identity");
                let position = self.nodes[current_row.0].position;
                if let Some(next) = self.children[slot.get() - 1].ordered.get(position + 1)
                    && let Some(rank) = self.rank(next.key)
                {
                    return rank;
                }
            }
            current = parent;
        }

        self.order.len()
    }

    pub(crate) fn request_order(&mut self, from: usize) {
        self.order_from = Some(self.order_from.map_or(from, |old| old.min(from)));
    }

    pub(crate) fn is_active(&self, key: LocalKey) -> bool {
        self.active.get(&key).is_some_and(|count| *count != 0)
    }

    pub(crate) fn change_active(&mut self, key: LocalKey, add: bool) {
        let mut current = Some(key);
        while let Some(key) = current {
            if add {
                let count = self.active.entry(key).or_default();
                *count += 1;
            } else if let Some(count) = self.active.get_mut(&key) {
                *count = count
                    .checked_sub(1)
                    .expect("active subtree summaries release retained references once");
                if *count == 0 {
                    self.active.remove(&key);
                }
            }
            current = self.parent(key);
        }
    }

    pub(crate) fn retire(&mut self, key: LocalKey) {
        if let Some(rank) = self.rank(key) {
            self.request_order(rank);
        }

        let mut stack = std::mem::take(&mut self.retire_scratch);
        stack.push(key);
        while let Some(key) = stack.pop() {
            let Some(row) = self.row_local(key) else {
                continue;
            };
            let node = &mut self.nodes[row.0];
            if node.life == Life::Retiring {
                continue;
            }

            if node.life == Life::Live {
                self.live -= 1;
            }
            node.life = Life::Retiring;
            if let Some(index) = node.children {
                stack.extend(
                    self.children[index.get() - 1]
                        .ordered
                        .iter()
                        .map(|child| child.key),
                );
            }
            self.retired.push(key);
        }

        if stack.capacity().saturating_mul(size_of::<LocalKey>()) <= 64 * 1024 {
            self.retire_scratch = stack;
        }
    }

    pub(crate) fn rebuild_order(&mut self, root: LocalKey) -> u64 {
        let Some(from) = self.order_from else {
            return 0;
        };

        let from = from.min(self.order.len());
        let mut stack = std::mem::take(&mut self.order_scratch);
        if from == 0 {
            stack.push(root);
        } else {
            let previous = self.order[from - 1];
            let mut current = previous;
            while let Some(parent) = self.parent(current) {
                let parent_row = self
                    .row_local(parent)
                    .expect("prefix ancestry retains its identity");
                if let Some(index) = self.nodes[parent_row.0].children {
                    let current_row = self
                        .row_local(current)
                        .expect("prefix members retain identity rows");
                    let position = self.nodes[current_row.0].position;
                    stack.extend(
                        self.children[index.get() - 1]
                            .ordered
                            .iter()
                            .skip(position + 1)
                            .map(|child| child.key),
                    );
                }
                current = parent;
            }
            stack.reverse();
            let previous_row = self
                .row_local(previous)
                .expect("the unchanged order prefix remains live");
            if let Some(index) = self.nodes[previous_row.0].children {
                stack.extend(
                    self.children[index.get() - 1]
                        .ordered
                        .iter()
                        .rev()
                        .map(|child| child.key),
                );
            }
        }

        self.order.truncate(from);
        let mut visited = 0_u64;
        while let Some(key) = stack.pop() {
            if !self.contains_local(key) {
                continue;
            }

            let row = self
                .row_local(key)
                .expect("walk keys belong to retained topology");
            self.nodes[row.0].rank = self.order.len();
            self.order.push(key);
            visited = visited.saturating_add(1);
            if let Some(index) = self.nodes[row.0].children {
                stack.extend(
                    self.children[index.get() - 1]
                        .ordered
                        .iter()
                        .rev()
                        .map(|child| child.key),
                );
            }
        }

        if stack.capacity().saturating_mul(size_of::<LocalKey>()) <= 64 * 1024 {
            self.order_scratch = stack;
        }

        visited
    }

    pub(crate) fn trim(&mut self) {
        assert_eq!(
            self.frames, 0,
            "mounted storage can shrink only after callback frames finish"
        );

        fn trim_vec<T>(values: &mut Vec<T>) {
            let demand = values.len().max(16);
            if values.capacity() > demand.saturating_mul(4)
                && values.capacity().saturating_mul(size_of::<T>()) > 64 * 1024
            {
                values.shrink_to(demand.saturating_mul(2));
            }
        }

        trim_vec(&mut self.nodes);
        trim_vec(&mut self.children);
        trim_vec(&mut self.order);
        trim_vec(&mut self.retired);
        trim_vec(&mut self.retire_scratch);
        trim_vec(&mut self.order_scratch);

        if self.active.is_empty() {
            self.active = HashMap::new();
        } else if self.active.capacity() > self.active.len().saturating_mul(4).max(16) {
            self.active
                .shrink_to(self.active.len().saturating_mul(2).max(16));
        }
    }

    pub(crate) fn take_child_set(&mut self, key: LocalKey) -> Option<ChildSet> {
        assert_eq!(
            self.frames, 0,
            "child-set remapping cannot overlap callback frames"
        );

        let row = self.row_local(key)?;
        let index = self.nodes[row.0].children.take()?.get() - 1;
        let removed = self.children.swap_remove(index);
        if let Some(moved) = self.children.get(index) {
            let owner = moved.owner;
            let owner_row = self
                .row_local(owner)
                .expect("moved child sets retain their owner identity");
            self.nodes[owner_row.0].children = NonZeroUsize::new(index + 1);
        }

        Some(removed)
    }

    /// The other dense owners must perform the same move before this Node is dropped.
    pub(crate) fn remove_row(&mut self, row: Row) -> Node {
        assert_eq!(
            self.frames, 0,
            "physical row remapping cannot overlap callback frames"
        );

        let node = self.nodes.swap_remove(row.0);
        self.identities.remove_local(node.key);
        self.active.remove(&node.key);
        if let Some(moved) = self.nodes.get(row.0) {
            *self
                .identities
                .get_local_mut(moved.key)
                .expect("moved dense row retains a primary identity") = row;
        }

        node
    }
}

/// Temporary structural capabilities, not ownership of the window or its output systems.
pub(crate) struct ElementAccess<'a> {
    pub(crate) elements: &'a mut MountedElements,
    pub(crate) configuration: &'a mut Configuration,
    pub(crate) layout: &'a mut Layout,
    pub(crate) work: &'a mut Scheduler,
    pub(crate) inbox: &'a Arc<Inbox>,
    pub(crate) requests: &'a mut Vec<InputRequest>,
}

impl ElementAccess<'_> {
    pub(crate) fn reborrow(&mut self) -> ElementAccess<'_> {
        ElementAccess {
            elements: &mut *self.elements,
            configuration: &mut *self.configuration,
            layout: &mut *self.layout,
            work: &mut *self.work,
            inbox: self.inbox,
            requests: &mut *self.requests,
        }
    }

    pub(crate) fn read(&self, id: ElementId) -> ConfigRead<'_> {
        let row = self
            .elements
            .row(id)
            .expect("an executing callback retains its allocated configuration row");
        self.configuration.read(row)
    }

    pub(crate) fn request(&mut self, id: ElementId, work: Work) {
        if work.is_empty() {
            return;
        }

        if let Some(row) = self.elements.row(id) {
            self.layout.note_work(self.elements, id.local(), work);
            if work.contains(Work::LAYOUT) {
                self.layout.invalidate(row);
            }
            self.work.request(id.local(), row, work);
        }
    }

    pub(crate) fn invalidate_hit_test(&mut self, id: ElementId) {
        if self.elements.row(id).is_some() {
            self.work.hit_dirty();
        }
    }

    pub(crate) fn set<P: Property>(
        &mut self,
        id: ElementId,
        property: P,
        value: P::Value,
    ) -> Result<(), ElementError> {
        let row = self.elements.row(id).ok_or(ElementError::Missing(id))?;

        if let Some(mutation) = self.configuration.set(row, id.local(), property, value)? {
            self.layout.note_configuration(self.elements, id.local());
            if mutation.change.affects.contains(Work::LAYOUT) {
                self.layout.invalidate(row);
            }
            self.work.request(id.local(), row, mutation.change.affects);
            if mutation.change.key == TypeId::of::<super::Enabled>() {
                self.work.geometry_dirty(id.local(), row);
            }

            drop_committed(self, id, mutation.previous);
        }

        Ok(())
    }

    pub(crate) fn remove<P: Property>(
        &mut self,
        id: ElementId,
        property: P,
    ) -> Result<(), ElementError> {
        let row = self.elements.row(id).ok_or(ElementError::Missing(id))?;

        if let Some(mutation) = self.configuration.remove(row, id.local(), property)? {
            self.layout.note_configuration(self.elements, id.local());
            if mutation.change.affects.contains(Work::LAYOUT) {
                self.layout.invalidate(row);
            }
            self.work.request(id.local(), row, mutation.change.affects);
            if mutation.change.key == TypeId::of::<super::Enabled>() {
                self.work.geometry_dirty(id.local(), row);
            }

            drop_committed(self, id, mutation.previous);
        }

        Ok(())
    }

    pub(crate) fn set_visibility(
        &mut self,
        id: ElementId,
        visibility: Visibility,
    ) -> Result<(), ElementError> {
        let row = self.elements.row(id).ok_or(ElementError::Missing(id))?;

        if self
            .configuration
            .set_visibility(row, id.local(), visibility)?
        {
            self.layout.note_configuration(self.elements, id.local());
            self.layout.invalidate(row);
            self.work.request(
                id.local(),
                row,
                Work::LAYOUT | Work::SCENE | Work::SEMANTICS,
            );
            self.work.geometry_dirty(id.local(), row);
        }

        Ok(())
    }

    pub(crate) fn bind<K: EventKey>(
        &mut self,
        id: ElementId,
        event: K,
        action: Action<K::Data>,
    ) -> Result<(), ElementError> {
        drop(event);

        let row = self.elements.row(id).ok_or(ElementError::Missing(id))?;

        let previous =
            self.configuration
                .bind(row, id.local(), TypeId::of::<K>(), action.binding)?;
        self.layout.note_configuration(self.elements, id.local());

        drop_committed(self, id, previous);

        Ok(())
    }
}

fn mark_changes(access: &mut ElementAccess<'_>, id: ElementId, changes: &[PropertyChange]) {
    let work = changes
        .iter()
        .fold(Work::empty(), |work, change| work | change.affects);
    access.request(id, work);

    if changes
        .iter()
        .any(|change| change.key == TypeId::of::<super::Enabled>())
    {
        let row = access
            .elements
            .row(id)
            .expect("configuration delivery retains an identity");
        access.work.geometry_dirty(id.local(), row);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConfigDelivery {
    Deliver,
    Defer,
}

fn pure_environment(
    access: &mut ElementAccess<'_>,
    key: LocalKey,
    parent: Environment,
) -> Result<(), ElementError> {
    let row = access
        .elements
        .row_local(key)
        .ok_or_else(|| ElementError::Missing(access.elements.id(key)))?;

    let refreshed = access.configuration.refresh(row, key, parent, None)?;
    if refreshed.output_changed {
        access.configuration.queue_descendants(row, key);
    }
    mark_changes(access, access.elements.id(key), &refreshed.changes);

    drop_committed(
        access,
        access.elements.id(key),
        (refreshed.retired_input, refreshed.retired_output),
    );

    Ok(())
}

/// Refreshes one node using its current parent output, without walking ancestors.
pub(crate) fn refresh_current(
    access: &mut ElementAccess<'_>,
    id: ElementId,
    delivery: ConfigDelivery,
) -> Result<(), ElementError> {
    let key = id.local();
    let row = access
        .elements
        .row_local(key)
        .ok_or_else(|| ElementError::Missing(access.elements.id(key)))?;

    let parent = access
        .elements
        .parent(key)
        .and_then(|parent| access.elements.row_local(parent))
        .map(|row| access.configuration.output(row))
        .unwrap_or_default();
    if delivery == ConfigDelivery::Defer
        || access.elements.nodes[row.0].body.is_none()
        || !access.elements.contains_local(key)
    {
        return pure_environment(access, key, parent);
    }
    let failed = access.work.update_failed(row);
    if access.configuration.checked(row) && !access.configuration.needs_delivery(key) && !failed {
        return Ok(());
    }

    let id = access.elements.id(key);
    let mut frame = CallFrame::enter(access, id, Some(crate::schedule::AttemptKind::Update))?;
    let changes = frame.refresh_environment(parent.clone(), None)?;
    mark_changes(frame.access(), id, &changes);

    if !changes.is_empty() || failed {
        let affects = changes
            .iter()
            .fold(Work::empty(), |work, change| work | change.affects);
        let (body, access, recovery) = frame.parts();
        let mut cx = UpdateCx {
            access,
            current: id,
            changes: &changes,
            recovery,
        };

        body.update_inherited(&mut cx, affects)?;

        cx.access.work.counts.updates = cx.access.work.counts.updates.saturating_add(1);
        cx.access.work.counts.configuration_updates = cx
            .access
            .work
            .counts
            .configuration_updates
            .saturating_add(1);
    }

    frame.finish();
    drop(frame);

    let row = access
        .elements
        .row(id)
        .expect("completed delivery retains its node");
    if !access.configuration.checked(row) {
        pure_environment(access, key, parent)?;
    }
    access
        .layout
        .settle_incoming(access.elements, access.configuration, access.work, key);

    Ok(())
}

/// Reprojects only environments, retaining any undelivered behavior keys.
pub(crate) fn ensure_environment(
    access: &mut ElementAccess<'_>,
    id: ElementId,
) -> Result<(), ElementError> {
    access.elements.row(id).ok_or(ElementError::Missing(id))?;

    let mut path = access.configuration.take_path();
    let mut current = Some(id.local());
    while let Some(key) = current {
        let row = access
            .elements
            .row_local(key)
            .ok_or_else(|| ElementError::Missing(access.elements.id(key)))?;
        if access.configuration.checked(row) {
            break;
        }

        path.push(key);
        current = access.elements.parent(key);
    }

    while let Some(key) = path.pop() {
        refresh_current(access, access.elements.id(key), ConfigDelivery::Defer)?;
    }

    access.configuration.return_path(path);

    Ok(())
}

/// Refreshes ancestors iteratively; currently executing ancestors only reproject their source.
pub(crate) fn ensure_current(
    access: &mut ElementAccess<'_>,
    id: ElementId,
) -> Result<(), ElementError> {
    let row = access.elements.row(id).ok_or(ElementError::Missing(id))?;
    if access.configuration.checked(row)
        && ((!access.work.update_failed(row) && !access.configuration.needs_delivery(id.local()))
            || access.elements.nodes[row.0].body.is_none())
    {
        return Ok(());
    }

    let mut path = access.configuration.take_path();
    let mut current = Some(id.local());
    while let Some(key) = current {
        let row = access
            .elements
            .row_local(key)
            .ok_or_else(|| ElementError::Missing(access.elements.id(key)))?;
        let blocked_delivery = (access.work.update_failed(row)
            || access.configuration.needs_delivery(key))
            && access.elements.nodes[row.0].body.is_some();
        if access.configuration.checked(row) && !blocked_delivery {
            break;
        }

        path.push(key);
        current = access.elements.nodes[row.0].parent;
    }

    while let Some(key) = path.pop() {
        let row = access
            .elements
            .row_local(key)
            .expect("configuration paths retain their identities");
        let delivery = if access.work.update_failed(row)
            && crate::layout::suppressed(access.elements, access.configuration, key)
        {
            ConfigDelivery::Defer
        } else {
            ConfigDelivery::Deliver
        };
        refresh_current(access, access.elements.id(key), delivery)?;
    }

    access.configuration.return_path(path);

    Ok(())
}

pub(crate) fn prepare(access: &mut ElementAccess<'_>, id: ElementId) -> Result<(), ElementError> {
    ensure_current(access, id)?;
    let row = access.elements.row(id).ok_or(ElementError::Missing(id))?;
    if !access.work.pending(row).contains(Work::PREPARE) {
        return Ok(());
    }

    let mut frame = CallFrame::enter(access, id, Some(crate::schedule::AttemptKind::Prepare))?;
    {
        let (body, access, recovery) = frame.parts();
        let mut cx = UpdateCx {
            access,
            current: id,
            changes: &[],
            recovery,
        };
        cx.access.work.counts.preparations = cx.access.work.counts.preparations.saturating_add(1);

        body.prepare(&mut cx)?;
    }

    frame.finish();
    drop(frame);

    access.layout.settle_incoming(
        access.elements,
        access.configuration,
        access.work,
        id.local(),
    );

    Ok(())
}

fn mount<E: MountedElement>(
    access: &mut ElementAccess<'_>,
    parent: Option<LocalKey>,
    attrs: super::Attrs,
    mut element: E,
) -> Result<ElementId, ElementError> {
    let input = parent
        .and_then(|parent| access.elements.row_local(parent))
        .map(|row| access.configuration.output(row))
        .unwrap_or_default();

    access.elements.nodes.reserve(1);
    access.configuration.reserve_row();
    access.layout.reserve_row();
    access.work.reserve_row();

    let id = access.elements.allocate(parent)?;
    access.configuration.append(attrs, input);
    access.layout.append();
    access.work.append();
    let row = access
        .elements
        .row(id)
        .expect("newly allocated identity has aligned rows");

    struct MountGuard<'a, 'b> {
        access: &'a mut ElementAccess<'b>,
        id: ElementId,
        complete: bool,
        started: bool,
    }

    impl Drop for MountGuard<'_, '_> {
        fn drop(&mut self) {
            let row = self
                .access
                .elements
                .row(self.id)
                .expect("mount guards retain their allocated identity");
            if !self.complete {
                if self.started {
                    self.access.work.fail_attempt(
                        self.id.local(),
                        row,
                        crate::schedule::AttemptKind::Update,
                    );
                }
                self.access.elements.retire(self.id.local());
            }

            self.access.elements.frames -= 1;
        }
    }

    access.elements.frames += 1;
    let mut guard = MountGuard {
        access,
        id,
        complete: false,
        started: false,
    };
    guard
        .access
        .work
        .begin_attempt(row, crate::schedule::AttemptKind::Update);
    guard.started = true;

    let state = {
        let mut cx = MountCx {
            access: guard.access.reborrow(),
            current: id,
        };

        element.mount(&mut cx)?
    };

    guard
        .access
        .elements
        .set_body(id.local(), Box::new(Entry { element, state }));
    guard.access.work.request(id.local(), row, Work::all());
    guard.access.work.geometry_dirty(id.local(), row);
    guard.access.work.counts.mounts = guard.access.work.counts.mounts.saturating_add(1);

    guard
        .access
        .work
        .finish_attempt(row, crate::schedule::AttemptKind::Update);
    guard.started = false;
    guard.complete = true;

    Ok(id)
}

/// Normalizes once, then hands actual mounted behavior to the compatible retained State.
pub(crate) fn apply<D: Element>(
    access: &mut ElementAccess<'_>,
    parent: Option<ElementId>,
    existing: Option<ElementId>,
    declaration: D,
) -> Result<ElementId, ElementError> {
    let ElementParts {
        mut attrs,
        mut element,
    } = declaration.into_parts();
    attrs.key = None;

    if let Some(parent) = parent {
        ensure_current(access, parent)?;
    }
    let Some(id) = existing else {
        return mount(access, parent.map(ElementId::local), attrs, element);
    };
    let row = if let Some(parent) = parent {
        access.elements.check_child(parent, id)?
    } else {
        access.elements.row(id).ok_or(ElementError::Missing(id))?
    };
    let compatible = access.elements.nodes[row.0]
        .body
        .as_ref()
        .is_some_and(|body| body.kind() == TypeId::of::<D::Mounted>());
    if !compatible {
        if access.elements.nodes[row.0].body.is_none() {
            return Err(ElementError::Reentrant(id));
        }
        return mount(access, parent.map(ElementId::local), attrs, element);
    }

    let old_visibility = access.configuration.read(row).visibility();
    let input = parent
        .and_then(|parent| access.elements.row(parent))
        .map(|row| access.configuration.output(row))
        .unwrap_or_default();
    let mut frame = CallFrame::enter(access, id, Some(crate::schedule::AttemptKind::Update))?;
    let changes = frame.install(attrs, input)?;
    mark_changes(frame.access(), id, &changes);
    if frame.access().read(id).visibility() != old_visibility {
        frame
            .access()
            .request(id, Work::LAYOUT | Work::SCENE | Work::SEMANTICS);
        frame.access().work.geometry_dirty(id.local(), row);
    }

    let previous = {
        let (body, access, recovery) = frame.parts();
        let any: &mut dyn Any = body;
        let entry = any
            .downcast_mut::<Entry<D::Mounted>>()
            .ok_or(ElementError::TypeMismatch)?;
        let mut cx = UpdateCx {
            access,
            current: id,
            changes: &changes,
            recovery,
        };

        element.update(
            &mut entry.state,
            UpdateReason::Replacement {
                previous: &entry.element,
            },
            &mut cx,
        )?;

        cx.access.work.counts.updates = cx.access.work.counts.updates.saturating_add(1);
        std::mem::replace(&mut entry.element, element)
    };

    frame.finish();
    drop(frame);

    drop_committed(access, id, previous);

    access.layout.settle_incoming(
        access.elements,
        access.configuration,
        access.work,
        id.local(),
    );

    Ok(id)
}
