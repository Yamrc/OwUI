//! Broad-phase projection over actual qualified regions and logical-order deltas.

use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap};
use std::num::NonZeroUsize;

use crate::element::ElementError;
use crate::element::configuration::Configuration;
use crate::element::mounted::{MountedElements, Row};
use crate::id::LocalKey;
use crate::layout::{ClipBounds, Layout, Point};
use crate::schedule::WorkCounts;

use super::{CleanupPose, HitTestCx, needs_trim_vec, trim_map, trim_vec};

const CELL_SIZE: f64 = 96.0;
const MAX_CELLS: i64 = 64;
const SPARE_MEMBER_BYTES: usize = 1024;
const TRIM_DEBT_BYTES: usize = 64 * 1024;
const TRIM_SLOT_INDEX_FROM: usize = 8;
type Cell = (i32, i32);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CellRange {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

#[derive(Clone, Copy)]
struct Member {
    key: LocalKey,
    link: u8,
}

struct Link {
    cell: Cell,
    position: usize,
}

enum Membership {
    Grid { range: CellRange, links: Vec<Link> },
    Large { position: usize },
}

struct Region {
    key: LocalKey,
    bounds: ClipBounds,
    order: usize,
    membership: Membership,
}

struct Bucket {
    members: Vec<Member>,
}

#[derive(Default)]
pub(crate) struct HitIndex {
    regions: Vec<Region>,
    members: HashMap<LocalKey, usize>,
    member_capacity: usize,
    order: BTreeMap<usize, usize>,
    cells: HashMap<Cell, Bucket>,
    cell_capacity: usize,
    trim_cells: Vec<Cell>,
    trim_slots: HashMap<Cell, NonZeroUsize>,
    spare_members: Vec<Member>,
    large: Vec<LocalKey>,
}

impl HitIndex {
    fn queue_trim(&mut self, cell: Cell) {
        if self.trim_slots.is_empty() {
            if self.trim_cells.contains(&cell) {
                return;
            }

            self.trim_cells.push(cell);
            if self.trim_cells.len() >= TRIM_SLOT_INDEX_FROM {
                self.trim_slots.reserve(self.trim_cells.len());
                for (position, &cell) in self.trim_cells.iter().enumerate() {
                    self.trim_slots.insert(
                        cell,
                        NonZeroUsize::new(position + 1)
                            .expect("allocated bucket trim slots retain a nonzero encoding"),
                    );
                }
            }
            return;
        }

        let slot = self.trim_cells.len();
        if let Entry::Vacant(entry) = self.trim_slots.entry(cell) {
            self.trim_cells.push(cell);
            entry.insert(
                NonZeroUsize::new(slot + 1)
                    .expect("allocated bucket trim slots retain a nonzero encoding"),
            );
        }
    }

    fn remove_trim(&mut self, cell: Cell) {
        if self.trim_slots.is_empty() {
            if let Some(position) = self.trim_cells.iter().position(|queued| *queued == cell) {
                self.trim_cells.swap_remove(position);
            }
            return;
        }

        let Some(slot) = self.trim_slots.remove(&cell) else {
            return;
        };

        let position = slot.get() - 1;
        let removed = self.trim_cells.swap_remove(position);
        debug_assert_eq!(removed, cell);
        if let Some(moved) = self.trim_cells.get(position) {
            *self
                .trim_slots
                .get_mut(moved)
                .expect("moved trim debt retains its reverse slot") =
                NonZeroUsize::new(position + 1)
                    .expect("allocated bucket trim slots retain a nonzero encoding");
        }
    }

    pub(crate) fn sync(
        &mut self,
        elements: &MountedElements,
        layout: &Layout,
        counts: &mut WorkCounts,
    ) -> Result<bool, ElementError> {
        let mut changed = false;
        for &key in &elements.retired {
            changed |= self.remove(key, counts);
        }

        if let Some(from) = elements.order_from {
            let visits = self.reindex_order_from(from, |key| elements.rank(key))?;
            counts.hit_order_visits = counts.hit_order_visits.saturating_add(visits);
            changed = true;
        }

        for &key in &layout.changed {
            changed |= self.sync_region(key, elements, layout, counts)?;
        }
        for &key in &layout.qualification_changed {
            changed |= self.sync_region(key, elements, layout, counts)?;
        }

        Ok(changed)
    }

    fn sync_region(
        &mut self,
        key: LocalKey,
        elements: &MountedElements,
        layout: &Layout,
        counts: &mut WorkCounts,
    ) -> Result<bool, ElementError> {
        match hit_bounds(elements, layout, key) {
            Some(bounds) => {
                let rank = elements
                    .rank(key)
                    .ok_or(ElementError::Missing(elements.id(key)))?;
                self.update(key, bounds, rank, counts)
            }
            None => Ok(self.remove(key, counts)),
        }
    }

    pub(crate) fn reindex_order_from(
        &mut self,
        from: usize,
        rank: impl Fn(LocalKey) -> Option<usize>,
    ) -> Result<u64, ElementError> {
        let mut changes = Vec::new();
        let mut next = BTreeMap::new();
        for (&previous, &row) in self.order.range(from..) {
            let key = self.regions[row].key;
            let rank = rank(key).ok_or_else(|| {
                ElementError::Operation("a retained hit region has no logical rank".into())
            })?;
            if self
                .order
                .get(&rank)
                .is_some_and(|member| rank < from && *member != row)
                || next.insert(rank, row).is_some()
            {
                return Err(ElementError::OrderConflict { rank });
            }
            changes.push((row, previous, rank));
        }

        for &(_, previous, _) in &changes {
            self.order.remove(&previous);
        }
        for &(row, _, rank) in &changes {
            self.regions[row].order = rank;
            self.order.insert(rank, row);
        }

        Ok(changes.len() as u64)
    }

    fn update(
        &mut self,
        key: LocalKey,
        bounds: impl Into<ClipBounds>,
        order: usize,
        counts: &mut WorkCounts,
    ) -> Result<bool, ElementError> {
        let bounds = bounds.into();
        if !bounds.is_valid() {
            return Err(ElementError::InvalidGeometry);
        }
        if bounds.is_empty() {
            return Ok(self.remove(key, counts));
        }
        if self
            .order
            .get(&order)
            .is_some_and(|row| self.regions[*row].key != key)
        {
            return Err(ElementError::OrderConflict { rank: order });
        }

        let covered = cells(bounds);
        if let Some(&row) = self.members.get(&key) {
            let region = &self.regions[row];
            let same = match (&region.membership, covered) {
                (Membership::Grid { range, .. }, Some(next)) => *range == next,
                (Membership::Large { .. }, None) => true,
                _ => false,
            };
            if same {
                let changed = region.bounds != bounds || region.order != order;
                if region.order != order {
                    self.order.remove(&region.order);
                    self.order.insert(order, row);
                }
                self.regions[row].bounds = bounds;
                self.regions[row].order = order;
                return Ok(changed);
            }
        }

        let mut links = self.detach(key, counts).unwrap_or_default();
        let membership = if let Some(range) = covered {
            let count = (i64::from(range.right) - i64::from(range.left) + 1)
                * (i64::from(range.bottom) - i64::from(range.top) + 1);
            let count = count as usize;
            if links.capacity() > count * 2 {
                links = Vec::with_capacity(count);
            } else {
                links.reserve_exact(count);
            }
            for x in range.left..=range.right {
                for y in range.top..=range.bottom {
                    let cell = (x, y);
                    let (position, needs_trim) = {
                        let bucket = match self.cells.entry(cell) {
                            Entry::Occupied(entry) => entry.into_mut(),
                            Entry::Vacant(entry) => entry.insert(Bucket {
                                members: std::mem::take(&mut self.spare_members),
                            }),
                        };
                        let position = bucket.members.len();
                        bucket.members.push(Member {
                            key,
                            link: links.len() as u8,
                        });
                        (position, needs_trim_vec(&bucket.members))
                    };
                    if needs_trim {
                        self.queue_trim(cell);
                    } else {
                        self.remove_trim(cell);
                    }
                    links.push(Link { cell, position });
                    counts.hit_memberships = counts.hit_memberships.saturating_add(1);
                }
            }
            Membership::Grid { range, links }
        } else {
            let position = self.large.len();
            self.large.push(key);
            counts.hit_memberships = counts.hit_memberships.saturating_add(1);
            Membership::Large { position }
        };

        let row = self.regions.len();
        self.regions.push(Region {
            key,
            bounds,
            order,
            membership,
        });
        self.members.insert(key, row);
        self.member_capacity = self.member_capacity.max(self.members.capacity());
        self.cell_capacity = self.cell_capacity.max(self.cells.capacity());
        self.order.insert(order, row);

        Ok(true)
    }

    pub(crate) fn remove(&mut self, key: LocalKey, counts: &mut WorkCounts) -> bool {
        let removed = self.detach(key, counts).is_some();
        if self.regions.is_empty() {
            self.regions = Vec::new();
            self.members = HashMap::new();
            self.member_capacity = 0;
            self.cells = HashMap::new();
            self.cell_capacity = 0;
            self.trim_cells = Vec::new();
            self.trim_slots = HashMap::new();
            self.large = Vec::new();
            self.spare_members = Vec::new();
        }
        removed
    }

    fn detach(&mut self, key: LocalKey, counts: &mut WorkCounts) -> Option<Vec<Link>> {
        let row = self.members.remove(&key)?;
        let region = self.regions.swap_remove(row);
        self.order.remove(&region.order);
        if let Some(moved) = self.regions.get(row) {
            *self
                .members
                .get_mut(&moved.key)
                .expect("the moved region retains its actual member row") = row;
            *self
                .order
                .get_mut(&moved.order)
                .expect("the moved region retains its ordered row") = row;
        }

        match region.membership {
            Membership::Grid { mut links, .. } => {
                for link in links.drain(..) {
                    let (moved, emptied) = match self.cells.entry(link.cell) {
                        Entry::Occupied(mut entry) => {
                            let bucket = entry.get_mut();
                            let removed = bucket.members.swap_remove(link.position);
                            debug_assert_eq!(removed.key, key);
                            let moved = bucket.members.get(link.position).copied();
                            if bucket.members.is_empty() {
                                let bucket = entry.remove();
                                let members = bucket.members;
                                if members.capacity().saturating_mul(size_of::<Member>())
                                    <= SPARE_MEMBER_BYTES
                                    && members.capacity() > self.spare_members.capacity()
                                {
                                    self.spare_members = members;
                                }
                                (moved, true)
                            } else {
                                (moved, false)
                            }
                        }
                        Entry::Vacant(_) => {
                            unreachable!("a grid reverse link retains its bucket until removal")
                        }
                    };
                    if emptied {
                        self.remove_trim(link.cell);
                    } else if self
                        .cells
                        .get(&link.cell)
                        .is_some_and(|bucket| needs_trim_vec(&bucket.members))
                    {
                        self.queue_trim(link.cell);
                    } else {
                        self.remove_trim(link.cell);
                    }
                    if let Some(moved) = moved {
                        let row = self.members[&moved.key];
                        let Membership::Grid { links, .. } = &mut self.regions[row].membership
                        else {
                            unreachable!("a grid bucket cannot contain a large-list member");
                        };
                        links[usize::from(moved.link)].position = link.position;
                    }
                    counts.hit_memberships = counts.hit_memberships.saturating_add(1);
                }
                Some(links)
            }
            Membership::Large { position } => {
                let removed = self.large.swap_remove(position);
                debug_assert_eq!(removed, key);
                if let Some(&moved) = self.large.get(position) {
                    let row = self.members[&moved];
                    let Membership::Large {
                        position: moved_position,
                    } = &mut self.regions[row].membership
                    else {
                        unreachable!("the large list cannot contain a grid member");
                    };
                    *moved_position = position;
                }
                counts.hit_memberships = counts.hit_memberships.saturating_add(1);
                Some(Vec::new())
            }
        }
    }

    pub(crate) fn target(
        &self,
        elements: &MountedElements,
        configuration: &Configuration,
        layout: &Layout,
        counts: &mut WorkCounts,
        point: Point,
    ) -> Option<LocalKey> {
        self.target_with(point, counts, |key, counts| {
            let Some(row) = elements.row_local(key) else {
                return false;
            };
            let Some(geometry) = layout.cleanup_geometry(row) else {
                return false;
            };
            effective_hit(
                elements,
                configuration,
                layout,
                key,
                row,
                CleanupPose::from_geometry(geometry),
                layout.clip_bounds(key, row),
                point,
                counts,
            )
        })
    }

    fn target_with(
        &self,
        point: Point,
        counts: &mut WorkCounts,
        refine: impl Fn(LocalKey, &mut WorkCounts) -> bool,
    ) -> Option<LocalKey> {
        if !point.x.is_finite() || !point.y.is_finite() {
            return None;
        }

        let cell = coordinate(point.x).zip(coordinate(point.y));
        let candidates = cell
            .and_then(|cell| self.cells.get(&cell))
            .into_iter()
            .flat_map(|bucket| bucket.members.iter())
            .map(|member| member.key)
            .chain(self.large.iter().copied());
        let mut target = None;
        let mut rank = 0;
        for key in candidates {
            counts.hit_candidates = counts.hit_candidates.saturating_add(1);
            let region = &self.regions[self.members[&key]];
            if (target.is_none() || region.order > rank)
                && region.bounds.contains(point)
                && refine(key, counts)
            {
                target = Some(key);
                rank = region.order;
            }
        }

        target
    }

    pub(crate) fn trim(&mut self) {
        trim_vec(&mut self.regions);
        trim_vec(&mut self.large);
        trim_map(&mut self.members, &mut self.member_capacity);
        trim_map(&mut self.cells, &mut self.cell_capacity);
        while let Some(cell) = self.trim_cells.pop() {
            if let Some(slot) = self.trim_slots.remove(&cell) {
                debug_assert_eq!(slot.get() - 1, self.trim_cells.len());
            } else {
                debug_assert!(self.trim_slots.is_empty());
                debug_assert!(self.trim_cells.len() < TRIM_SLOT_INDEX_FROM);
            }
            let bucket = self
                .cells
                .get_mut(&cell)
                .expect("queued trim debt retains its live bucket");
            trim_vec(&mut bucket.members);
        }
        debug_assert!(self.trim_slots.is_empty());
        if self
            .trim_slots
            .capacity()
            .saturating_mul(size_of::<(Cell, NonZeroUsize)>())
            > TRIM_DEBT_BYTES
        {
            self.trim_slots = HashMap::new();
        }
        let queue = std::mem::take(&mut self.trim_cells);
        super::retain_buffer(&mut self.trim_cells, queue);
        if self.regions.is_empty() {
            self.spare_members = Vec::new();
        }
    }
}

fn hit_bounds(elements: &MountedElements, layout: &Layout, key: LocalKey) -> Option<ClipBounds> {
    if !elements.contains_local(key) {
        return None;
    }
    let row = elements.row_local(key)?;
    if !layout.eligible(row) || !layout.records[row.0].hit_testable {
        return None;
    }
    let geometry = layout.geometry(row)?;
    Some(ClipBounds::from(geometry.bounds).intersect(layout.clip_bounds(key, row)))
}

#[expect(
    clippy::too_many_arguments,
    reason = "Read-only owners, checked identity/row and prepared geometry stay disjoint"
)]
pub(super) fn effective_hit(
    elements: &MountedElements,
    configuration: &Configuration,
    layout: &Layout,
    key: LocalKey,
    row: Row,
    pose: CleanupPose,
    clip_bounds: ClipBounds,
    point: Point,
    counts: &mut WorkCounts,
) -> bool {
    if !elements.contains_local(key) || !layout.eligible(row) || !layout.records[row.0].world_set {
        return false;
    }
    if !layout.records[row.0].hit_testable
        || !pose.bounds.contains(point)
        || !clip_bounds.contains(point)
    {
        return false;
    }
    let Some(body) = elements.body(key) else {
        return false;
    };

    let local = world_to_local(point, pose.bounds);
    let cx = HitTestCx::new(elements.id(key), configuration.read(row));
    counts.hit_refinements = counts.hit_refinements.saturating_add(1);
    body.hit_test(&cx, pose.bounds.size, local)
}

fn world_to_local(point: Point, bounds: crate::layout::Rect) -> Point {
    fn axis(world: f32, origin: f32, extent: f32) -> f32 {
        let local = world - origin;
        let world_end = origin + extent;
        if local >= extent && world < world_end {
            extent.next_down()
        } else {
            local
        }
    }

    Point::new(
        axis(point.x, bounds.origin.x, bounds.size.width),
        axis(point.y, bounds.origin.y, bounds.size.height),
    )
}

fn coordinate(value: f32) -> Option<i32> {
    if !value.is_finite() {
        return None;
    }
    let cell = (f64::from(value) / CELL_SIZE).floor();
    if cell < f64::from(i32::MIN) || cell > f64::from(i32::MAX) {
        return None;
    }
    Some(cell as i32)
}

fn cells(bounds: impl Into<ClipBounds>) -> Option<CellRange> {
    let bounds = bounds.into();
    if !bounds.is_valid() || bounds.is_empty() {
        return None;
    }

    let left = coordinate(bounds.min.x)?;
    let top = coordinate(bounds.min.y)?;
    let right = coordinate(bounds.max.x.next_down())?;
    let bottom = coordinate(bounds.max.y.next_down())?;
    let count = (i64::from(right) - i64::from(left) + 1)
        .checked_mul(i64::from(bottom) - i64::from(top) + 1)?;
    (count > 0 && count <= MAX_CELLS).then_some(CellRange {
        left,
        top,
        right,
        bottom,
    })
}
