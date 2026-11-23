//! Backend-independent presentation fragments.
//!
//! A scene contains owned presentation data, not callbacks or copies of the
//! logical element tree. It does not expose native or GPU resource access.

use std::cell::Cell;
use std::collections::{BTreeMap, HashMap};
use std::num::NonZeroUsize;
use std::sync::Arc;

use crate::color::Color;
use crate::element::configuration::ConfigRead;
use crate::element::{ElementError, ElementId, Property, Visibility};
use crate::id::LocalKey;
use crate::layout::{ClipBounds, Point, Rect, Size};

/// Owned drawing intent in element-local coordinates.
///
/// Text contains unshaped strings. A consumer supplies shaping and rendering;
/// rectangle coordinates are local to the owning fragment's layout bounds.
#[derive(Clone, Debug, PartialEq)]
pub enum Primitive {
    Rect {
        rect: Rect,
        color: Color,
    },

    Text {
        origin: Point,
        text: Arc<str>,
        color: Color,
    },
}

/// Complete local primitives attached to one element's world bounds and clip.
///
/// Local drawing may extend beyond the layout bounds. Consumers translate by
/// `bounds.origin` and apply [`Scene::clip_bounds`] for exact half-open clipping.
/// `clip` is a conservative rectangle cover and can include excluded endpoints;
/// storage does not clip individual primitives.
#[derive(Debug)]
pub struct Fragment {
    pub element: ElementId,
    pub bounds: Rect,
    pub clip: Rect,
    pub primitives: Vec<Primitive>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FragmentRow(usize);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FragmentOrder {
    Indexed(usize),
    Unindexed(usize),
}

#[derive(Clone, Copy)]
struct SummaryIndex(NonZeroUsize);

impl SummaryIndex {
    fn new(index: usize) -> Self {
        Self(
            NonZeroUsize::new(index + 1)
                .expect("allocated summary indices have a nonzero encoding"),
        )
    }

    fn get(self) -> usize {
        self.0.get() - 1
    }
}

struct FragmentRecord {
    fragment: Fragment,
    order: FragmentOrder,
    summary: Option<SummaryIndex>,
}

struct CachedSummary {
    owner: FragmentRow,
    summary: PrimitiveSummary,
}

struct RankChange {
    row: FragmentRow,
    rank: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SceneDisposition {
    Recorded,
    Empty,
    Culled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SceneChange {
    pub(crate) membership_changed: bool,
    pub(crate) disposition: SceneDisposition,
}

// Small fragments avoid a retained coordinate range. This only selects a cache path.
const SUMMARY_PRIMITIVES: usize = 4;
const RETAINED_BYTES: usize = 64 * 1024;

/// The latest composed presentation fragments in logical tree order.
///
/// Empty or provably clipped-out payloads do not occupy entries. Primitives remain
/// in element-local coordinates; consumers apply each fragment's world origin and
/// exact [`Self::clip_bounds`]. Fragment `clip` fields are conservative covers.
/// Individual primitives are not guaranteed to intersect the exact clip.
#[derive(Default)]
pub struct Scene {
    domain: Option<u64>,
    fragments: Vec<FragmentRecord>,
    positions: HashMap<LocalKey, FragmentRow>,
    position_capacity: usize,

    clip_overrides: HashMap<LocalKey, ClipBounds>,
    clip_capacity: usize,

    order: BTreeMap<usize, FragmentRow>,
    unindexed: Vec<FragmentRow>,

    summaries: Vec<CachedSummary>,
    order_scratch: Vec<RankChange>,

    revision: u64,
}

impl Scene {
    /// Returns the wrapping revision token of successful composition changes.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Iterates composed fragments in logical tree order.
    pub fn fragments(&self) -> impl Iterator<Item = &Fragment> {
        self.order
            .values()
            .map(|row| &self.fragments[row.0].fragment)
    }

    /// Iterates composed fragments together with their exact clip endpoints.
    /// Unlike querying by identity for each item, this uses the existing ordered dense rows.
    pub fn fragments_with_clip(&self) -> impl Iterator<Item = (&Fragment, ClipBounds)> {
        self.order.values().map(|row| {
            let fragment = &self.fragments[row.0].fragment;
            let clip = self
                .clip_overrides
                .get(&fragment.element.local())
                .copied()
                .unwrap_or_else(|| ClipBounds::from(fragment.clip));

            (fragment, clip)
        })
    }

    /// Returns the retained fragment for a domain- and generation-matching identity.
    pub fn fragment(&self, id: ElementId) -> Option<&Fragment> {
        self.row(id).map(|row| &self.fragments[row.0].fragment)
    }

    /// Returns the retained fragment's exact half-open clipping endpoints.
    ///
    /// Most fragments derive these from their rectangle cover. Only numeric cases
    /// where that cover differs retain a sparse endpoint companion.
    pub fn clip_bounds(&self, id: ElementId) -> Option<ClipBounds> {
        let row = self.row(id)?;

        Some(
            self.clip_overrides
                .get(&id.local())
                .copied()
                .unwrap_or_else(|| ClipBounds::from(self.fragments[row.0].fragment.clip)),
        )
    }

    pub fn primitive_count(&self) -> usize {
        self.fragments
            .iter()
            .map(|record| record.fragment.primitives.len())
            .sum()
    }

    pub fn len(&self) -> usize {
        self.fragments.len()
    }

    pub fn is_empty(&self) -> bool {
        self.fragments.is_empty()
    }

    fn row(&self, id: ElementId) -> Option<FragmentRow> {
        if self.domain != Some(id.domain()) {
            return None;
        }

        self.positions.get(&id.local()).copied()
    }

    fn check_domain(&self, id: ElementId) -> Result<(), ElementError> {
        if self.domain.is_some_and(|domain| domain != id.domain()) {
            return Err(ElementError::Missing(id));
        }

        Ok(())
    }

    pub(crate) fn take_primitives(&mut self, id: ElementId) -> Vec<Primitive> {
        let Some(row) = self.row(id) else {
            return Vec::new();
        };

        self.remove_summary(row);
        std::mem::take(&mut self.fragments[row.0].fragment.primitives)
    }

    pub(crate) fn publish_fragment(
        &mut self,
        mut fragment: Fragment,
        exact_clip: ClipBounds,
        rank: usize,
        ordered: bool,
    ) -> Result<SceneChange, ElementError> {
        let id = fragment.element;
        self.check_domain(id)?;
        validate_rect(fragment.bounds)?;
        validate_clip(fragment.clip, exact_clip)?;

        if fragment.primitives.is_empty() {
            return Ok(SceneChange {
                membership_changed: self.remove(id),
                disposition: SceneDisposition::Empty,
            });
        }

        let (summary, world) = summarize_primitives(&fragment.primitives, fragment.bounds.origin)?;
        let disposition = summary.disposition(world, exact_clip);
        if disposition == SceneDisposition::Culled {
            return Ok(SceneChange {
                membership_changed: self.remove(id),
                disposition,
            });
        }

        let previous = self.row(id);
        if ordered
            && self
                .order
                .get(&rank)
                .is_some_and(|row| Some(*row) != previous)
        {
            return Err(ElementError::OrderConflict { rank });
        }

        let summary = (fragment.primitives.len() > SUMMARY_PRIMITIVES).then_some(summary);
        let primitive_count = fragment.primitives.len();
        shrink_vec(&mut fragment.primitives, primitive_count);
        let cover = fragment.clip;

        if let Some(row) = previous {
            self.set_summary(row, summary);
            if ordered {
                self.index(row, rank);
            }

            let previous = std::mem::replace(&mut self.fragments[row.0].fragment, fragment);
            self.publish_clip(id.local(), cover, exact_clip);

            drop(previous);
        } else {
            self.fragments.reserve(1);
            self.positions.reserve(1);
            self.position_capacity = self.position_capacity.max(self.positions.capacity());
            if !ordered {
                self.unindexed.reserve(1);
            }
            if summary.is_some() {
                self.summaries.reserve(1);
            }

            let row = FragmentRow(self.fragments.len());
            let order = if ordered {
                FragmentOrder::Indexed(rank)
            } else {
                FragmentOrder::Unindexed(self.unindexed.len())
            };
            self.fragments.push(FragmentRecord {
                fragment,
                order,
                summary: None,
            });

            self.positions.insert(id.local(), row);
            self.domain = Some(id.domain());
            if ordered {
                self.order.insert(rank, row);
            } else {
                self.unindexed.push(row);
            }

            self.set_summary(row, summary);
            self.publish_clip(id.local(), cover, exact_clip);
        }

        Ok(SceneChange {
            membership_changed: previous.is_none(),
            disposition: SceneDisposition::Recorded,
        })
    }

    pub(crate) fn move_fragment(
        &mut self,
        id: ElementId,
        bounds: Rect,
        clip: Rect,
        exact_clip: ClipBounds,
    ) -> Result<SceneChange, ElementError> {
        let row = self.row(id).ok_or(ElementError::Missing(id))?;
        validate_rect(bounds)?;
        validate_clip(clip, exact_clip)?;

        let record = &self.fragments[row.0];
        if record.fragment.primitives.is_empty() {
            return Ok(SceneChange {
                membership_changed: self.remove(id),
                disposition: SceneDisposition::Empty,
            });
        }

        let (summary, world) = match record.summary {
            Some(index) => {
                let summary = self.summaries[index.get()].summary;
                let world = summary.project(bounds.origin, &record.fragment.primitives)?;
                (summary, world)
            }
            None => summarize_primitives(&record.fragment.primitives, bounds.origin)?,
        };

        let disposition = summary.disposition(world, exact_clip);
        if disposition == SceneDisposition::Culled {
            return Ok(SceneChange {
                membership_changed: self.remove(id),
                disposition,
            });
        }

        let fragment = &mut self.fragments[row.0].fragment;
        fragment.bounds = bounds;
        fragment.clip = clip;
        self.publish_clip(id.local(), clip, exact_clip);

        Ok(SceneChange {
            membership_changed: false,
            disposition: SceneDisposition::Recorded,
        })
    }

    fn publish_clip(&mut self, key: LocalKey, cover: Rect, exact: ClipBounds) {
        if ClipBounds::from(cover) == exact {
            self.remove_clip(key);
        } else {
            self.clip_overrides.insert(key, exact);
            self.clip_capacity = self.clip_capacity.max(self.clip_overrides.capacity());
        }
    }

    fn remove_clip(&mut self, key: LocalKey) {
        if self.clip_overrides.remove(&key).is_some() && self.clip_overrides.is_empty() {
            self.clip_overrides = HashMap::new();
            self.clip_capacity = 0;
        }
    }

    fn set_summary(&mut self, row: FragmentRow, summary: Option<PrimitiveSummary>) {
        match (self.fragments[row.0].summary, summary) {
            (Some(index), Some(summary)) => self.summaries[index.get()].summary = summary,
            (Some(_), None) => self.remove_summary(row),
            (None, Some(summary)) => {
                let index = SummaryIndex::new(self.summaries.len());
                self.summaries.push(CachedSummary {
                    owner: row,
                    summary,
                });
                self.fragments[row.0].summary = Some(index);
            }
            (None, None) => {}
        }
    }

    fn remove_summary(&mut self, row: FragmentRow) {
        let Some(index) = self.fragments[row.0].summary.take() else {
            return;
        };

        self.summaries.swap_remove(index.get());
        if index.get() < self.summaries.len() {
            let moved = self.summaries[index.get()].owner;
            self.fragments[moved.0].summary = Some(index);
        }
    }

    fn unindex(&mut self, row: FragmentRow) {
        match self.fragments[row.0].order {
            FragmentOrder::Indexed(rank) => {
                self.order.remove(&rank);
            }
            FragmentOrder::Unindexed(position) => {
                self.unindexed.swap_remove(position);
                if position < self.unindexed.len() {
                    let moved = self.unindexed[position];
                    self.fragments[moved.0].order = FragmentOrder::Unindexed(position);
                }
            }
        }
    }

    fn index(&mut self, row: FragmentRow, rank: usize) {
        if self.fragments[row.0].order == FragmentOrder::Indexed(rank) {
            return;
        }

        self.unindex(row);
        self.fragments[row.0].order = FragmentOrder::Indexed(rank);
        self.order.insert(rank, row);
    }

    pub(crate) fn remove(&mut self, id: ElementId) -> bool {
        let Some(row) = self.row(id) else {
            return false;
        };

        self.unindex(row);
        self.remove_summary(row);
        self.remove_clip(id.local());
        self.positions.remove(&id.local());
        let removed = self.fragments.swap_remove(row.0);
        if row.0 < self.fragments.len() {
            let moved = &self.fragments[row.0];
            self.positions.insert(moved.fragment.element.local(), row);
            match moved.order {
                FragmentOrder::Indexed(rank) => {
                    self.order.insert(rank, row);
                }
                FragmentOrder::Unindexed(position) => self.unindexed[position] = row,
            }
            if let Some(index) = moved.summary {
                self.summaries[index.get()].owner = row;
            }
        }

        drop(removed);

        if self.fragments.is_empty() {
            self.fragments = Vec::new();
            self.positions = HashMap::new();
            self.position_capacity = 0;
            self.clip_overrides = HashMap::new();
            self.clip_capacity = 0;
            self.unindexed = Vec::new();
            self.summaries = Vec::new();
            self.order_scratch = Vec::new();
        }

        true
    }

    pub(crate) fn reindex_order_from(
        &mut self,
        from: usize,
        rank: impl Fn(ElementId) -> Option<usize>,
    ) -> Result<u64, ElementError> {
        let mut changes = std::mem::take(&mut self.order_scratch);
        let result = self.apply_order_delta(from, rank, &mut changes);

        let demand = changes.len();
        shrink_vec(&mut changes, demand);
        changes.clear();
        self.order_scratch = changes;

        result
    }

    fn apply_order_delta(
        &mut self,
        from: usize,
        rank: impl Fn(ElementId) -> Option<usize>,
        changes: &mut Vec<RankChange>,
    ) -> Result<u64, ElementError> {
        for &row in self.order.range(from..).map(|(_, row)| row) {
            let id = self.fragments[row.0].fragment.element;
            let rank = rank(id).ok_or(ElementError::Missing(id))?;
            changes.push(RankChange { row, rank });
        }

        for &row in &self.unindexed {
            let id = self.fragments[row.0].fragment.element;
            let rank = rank(id).ok_or(ElementError::Missing(id))?;
            changes.push(RankChange { row, rank });
        }

        changes.sort_unstable_by_key(|change| change.rank);
        for pair in changes.windows(2) {
            if pair[0].rank == pair[1].rank {
                return Err(ElementError::OrderConflict { rank: pair[0].rank });
            }
        }

        for change in &*changes {
            if change.rank < from && self.order.contains_key(&change.rank) {
                return Err(ElementError::OrderConflict { rank: change.rank });
            }
        }

        // Remove the complete affected old order before any target rank is installed.
        if from == 0 {
            self.order.clear();
        } else {
            for change in &*changes {
                if let FragmentOrder::Indexed(previous) = self.fragments[change.row.0].order {
                    self.order.remove(&previous);
                }
            }
        }

        for change in &*changes {
            self.fragments[change.row.0].order = FragmentOrder::Indexed(change.rank);
            self.order.insert(change.rank, change.row);
        }
        self.unindexed = Vec::new();

        Ok(changes.len() as u64)
    }

    pub(crate) fn changed(&mut self) {
        self.revision = self.revision.wrapping_add(1);

        let fragments = self.fragments.len();
        let summaries = self.summaries.len();
        let unindexed = self.unindexed.len();
        shrink_vec(&mut self.fragments, fragments);
        shrink_vec(&mut self.summaries, summaries);
        shrink_vec(&mut self.unindexed, unindexed);
        shrink_vec(&mut self.order_scratch, fragments);

        let clips = self.clip_overrides.len().max(16);
        let clip_bucket_bytes = size_of::<(LocalKey, ClipBounds)>() + 1;
        if self.clip_capacity > clips.saturating_mul(4)
            && self.clip_capacity.saturating_mul(clip_bucket_bytes) > RETAINED_BYTES
        {
            self.clip_overrides.shrink_to(clips.saturating_mul(2));
            self.clip_capacity = self.clip_overrides.capacity();
        }

        if self.positions.is_empty() {
            self.positions = HashMap::new();
            self.position_capacity = 0;
            return;
        }

        let demand = self.positions.len().max(16);
        let bucket_bytes = size_of::<(LocalKey, FragmentRow)>() + 1;

        // Tombstones can reduce reported capacity without releasing the bucket allocation, uh.
        if self.position_capacity > demand.saturating_mul(4)
            && self.position_capacity.saturating_mul(bucket_bytes) > RETAINED_BYTES
        {
            self.positions.shrink_to(demand.saturating_mul(2));
            self.position_capacity = self.positions.capacity();
        }
    }
}

fn shrink_vec<T>(values: &mut Vec<T>, demand: usize) {
    let demand = demand.max(values.len());
    if demand == 0 {
        *values = Vec::new();
        return;
    }

    let demand = demand.max(16);
    if values.capacity() > demand.saturating_mul(4)
        && values.capacity().saturating_mul(size_of::<T>()) > RETAINED_BYTES
    {
        values.shrink_to(demand.saturating_mul(2));
    }
}

#[derive(Clone, Copy)]
struct CoordinateRange {
    left: f64,
    top: f64,
    right: f64,
    bottom: f64,
}

impl CoordinateRange {
    fn empty() -> Self {
        Self {
            left: f64::INFINITY,
            top: f64::INFINITY,
            right: f64::NEG_INFINITY,
            bottom: f64::NEG_INFINITY,
        }
    }

    fn include(&mut self, x: f64, y: f64) {
        self.left = self.left.min(x);
        self.top = self.top.min(y);
        self.right = self.right.max(x);
        self.bottom = self.bottom.max(y);
    }

    fn include_point(&mut self, point: Point) {
        self.include(f64::from(point.x), f64::from(point.y));
    }

    fn include_rect(&mut self, rect: Rect) {
        self.include_point(rect.origin);
        self.include(
            f64::from(rect.origin.x + rect.size.width),
            f64::from(rect.origin.y + rect.size.height),
        );
    }

    fn project(self, origin: Point) -> Self {
        let (left, right) = project_axis(self.left, self.right, origin.x);
        let (top, bottom) = project_axis(self.top, self.bottom, origin.y);

        Self {
            left,
            top,
            right,
            bottom,
        }
    }

    fn is_finite(self) -> bool {
        self.left.is_finite()
            && self.top.is_finite()
            && self.right.is_finite()
            && self.bottom.is_finite()
    }

    fn intersects(self, clip: ClipBounds) -> bool {
        self.left < f64::from(clip.max.x)
            && self.top < f64::from(clip.max.y)
            && self.right > f64::from(clip.min.x)
            && self.bottom > f64::from(clip.min.y)
    }
}

fn project_axis(low: f64, high: f64, origin: f32) -> (f64, f64) {
    // The lower bound is an actual submitted f32 origin. Rounded f32 addition is
    // monotone, and nonnegative extents cannot lower a projected origin, so its
    // single translated minimum is exact even when upper edges need padding.
    // It can avoid some stupid correctness issues.
    let exact_low = origin + low as f32;
    let low = (f64::from(origin) + low).next_down();
    let high = (f64::from(origin) + high).next_up();
    let magnitude = low.abs().max(high.abs());
    let unit = f64::from(f32::EPSILON) / 2.0;

    // A projected rectangle uses two f32 additions, including possible subnormal rounding.
    let padding = (magnitude * unit * (2.0 + unit) + 2.0 * f64::from(f32::from_bits(1))).next_up();
    let high = (high + padding).next_up() as f32;

    (f64::from(exact_low), f64::from(high))
}

#[derive(Clone, Copy)]
struct PrimitiveSummary {
    coordinates: CoordinateRange,
    unknown_text: bool,
    has_rect_ink: bool,
}

impl PrimitiveSummary {
    fn project(
        self,
        origin: Point,
        primitives: &[Primitive],
    ) -> Result<CoordinateRange, ElementError> {
        let world = self.coordinates.project(origin);
        if world.is_finite() {
            return Ok(world);
        }

        // A conservative interval can exceed f32 near MAX even when actual additions do not.
        project_primitives(primitives, origin)
    }

    fn disposition(self, world: CoordinateRange, clip: ClipBounds) -> SceneDisposition {
        if clip.is_empty()
            || (!self.unknown_text && (!self.has_rect_ink || !world.intersects(clip)))
        {
            SceneDisposition::Culled
        } else {
            SceneDisposition::Recorded
        }
    }
}

fn summarize_primitives(
    primitives: &[Primitive],
    origin: Point,
) -> Result<(PrimitiveSummary, CoordinateRange), ElementError> {
    let mut summary = PrimitiveSummary {
        coordinates: CoordinateRange::empty(),
        unknown_text: false,
        has_rect_ink: false,
    };
    let mut world = CoordinateRange::empty();

    for primitive in primitives {
        match primitive {
            Primitive::Rect { rect, color } => {
                validate_rect(*rect)?;
                validate_color(*color)?;

                summary.coordinates.include_point(rect.origin);
                summary.coordinates.include(
                    (f64::from(rect.origin.x) + f64::from(rect.size.width)).next_up(),
                    (f64::from(rect.origin.y) + f64::from(rect.size.height)).next_up(),
                );
                summary.has_rect_ink |= rect.size.width > 0.0 && rect.size.height > 0.0;

                let rect = Rect::new(origin.add(rect.origin), rect.size);
                validate_rect(rect)?;
                world.include_rect(rect);
            }
            Primitive::Text {
                origin: local,
                color,
                ..
            } => {
                validate_point(*local)?;
                validate_color(*color)?;
                summary.coordinates.include_point(*local);
                summary.unknown_text = true;

                let point = origin.add(*local);
                validate_point(point)?;
                world.include_point(point);
            }
        }
    }

    Ok((summary, world))
}

fn project_primitives(
    primitives: &[Primitive],
    origin: Point,
) -> Result<CoordinateRange, ElementError> {
    let mut world = CoordinateRange::empty();

    for primitive in primitives {
        match primitive {
            Primitive::Rect { rect, .. } => {
                let rect = Rect::new(origin.add(rect.origin), rect.size);
                validate_rect(rect)?;
                world.include_rect(rect);
            }
            Primitive::Text { origin: local, .. } => {
                let point = origin.add(*local);
                validate_point(point)?;
                world.include_point(point);
            }
        }
    }

    Ok(world)
}

fn validate_point(point: Point) -> Result<(), ElementError> {
    if point.x.is_finite() && point.y.is_finite() {
        Ok(())
    } else {
        Err(ElementError::InvalidGeometry)
    }
}

fn validate_rect(rect: Rect) -> Result<(), ElementError> {
    validate_point(rect.origin)?;
    if rect.size.is_valid()
        && (rect.origin.x + rect.size.width).is_finite()
        && (rect.origin.y + rect.size.height).is_finite()
    {
        Ok(())
    } else {
        Err(ElementError::InvalidGeometry)
    }
}

fn validate_clip(cover: Rect, exact: ClipBounds) -> Result<(), ElementError> {
    validate_rect(cover)?;
    if !exact.is_valid() {
        return Err(ElementError::InvalidGeometry);
    }

    let covering = ClipBounds::from(cover);
    if exact.is_empty()
        || (covering.min.x <= exact.min.x
            && covering.min.y <= exact.min.y
            && covering.max.x >= exact.max.x
            && covering.max.y >= exact.max.y)
    {
        Ok(())
    } else {
        Err(ElementError::InvalidGeometry)
    }
}

fn validate_color(color: Color) -> Result<(), ElementError> {
    if color.r.is_finite() && color.g.is_finite() && color.b.is_finite() && color.a.is_finite() {
        Ok(())
    } else {
        Err(ElementError::InvalidGeometry)
    }
}

/// World-space observations made while recording one local fragment.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct SceneDeps(u8);

impl SceneDeps {
    pub(crate) const WORLD_BOUNDS: Self = Self(1);
    pub(crate) const WORLD_CLIP: Self = Self(2);

    pub(crate) const fn empty() -> Self {
        Self(0)
    }

    pub(crate) const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

/// Records complete local presentation data using read-only effective configuration.
///
/// World bounds and clip reads register dependencies for future recording. Local size
/// and configuration reads do not. Submitted primitives are not culled during recording.
pub struct SceneCx<'a> {
    id: ElementId,
    bounds: Rect,
    clip: Rect,
    exact_clip: ClipBounds,
    configuration: ConfigRead<'a>,
    output: &'a mut Vec<Primitive>,
    dependencies: Cell<SceneDeps>,
}

impl<'a> SceneCx<'a> {
    pub(crate) fn new(
        id: ElementId,
        bounds: Rect,
        clip: Rect,
        exact_clip: ClipBounds,
        configuration: ConfigRead<'a>,
        output: &'a mut Vec<Primitive>,
    ) -> Self {
        Self {
            id,
            bounds,
            clip,
            exact_clip,
            configuration,
            output,
            dependencies: Cell::new(SceneDeps::empty()),
        }
    }

    pub fn id(&self) -> ElementId {
        self.id
    }

    pub fn current_visibility(&self) -> Visibility {
        self.configuration.visibility()
    }

    /// Returns the effective property value or its default.
    pub fn get<P: Property>(&self, property: P) -> P::Value {
        self.configuration.get(property)
    }

    /// Borrows an explicitly provided effective property independently of output writes.
    pub fn get_ref<P: Property>(&self, property: P) -> Option<&'a P::Value> {
        self.configuration.get_ref(property)
    }

    /// Returns the local layout size without observing world position or clip.
    pub fn size(&self) -> Size {
        self.bounds.size
    }

    /// Returns world layout bounds and registers a world-bounds dependency.
    pub fn bounds(&self) -> Rect {
        self.dependencies
            .set(self.dependencies.get().union(SceneDeps::WORLD_BOUNDS));

        self.bounds
    }

    /// Returns a conservative world clip cover and registers a world-clip dependency.
    pub fn clip(&self) -> Rect {
        self.dependencies
            .set(self.dependencies.get().union(SceneDeps::WORLD_CLIP));

        self.clip
    }

    /// Returns exact half-open clipping endpoints and registers a world-clip dependency.
    pub fn clip_bounds(&self) -> ClipBounds {
        self.dependencies
            .set(self.dependencies.get().union(SceneDeps::WORLD_CLIP));

        self.exact_clip
    }

    pub(crate) fn dependencies(&self) -> SceneDeps {
        self.dependencies.get()
    }

    /// Appends a complete local rectangle, including portions outside the world clip.
    pub fn rect(&mut self, rect: Rect, color: Color) {
        self.output.push(Primitive::Rect { rect, color });
    }

    /// Appends owned, unshaped text at a local origin; its ink extent is unknown.
    pub fn text(&mut self, origin: Point, text: impl Into<Arc<str>>, color: Color) {
        self.output.push(Primitive::Text {
            origin,
            text: text.into(),
            color,
        });
    }
}
