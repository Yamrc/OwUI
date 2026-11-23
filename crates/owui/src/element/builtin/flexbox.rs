use crate::element::{
    Attrs, ChildrenCx, Element, ElementError, ElementId, ElementParts, Gap, MountCx,
    MountedElement, UpdateCx, UpdateReason, Visibility,
};
use crate::layout::{
    ChildSizeChange, Constraints, LayoutCx, Point, Size, SizeContribution, SizePlan,
};
use crate::schedule::Work;

use super::constrain;

trait ChildSource {
    fn apply(self: Box<Self>, children: &mut ChildrenCx<'_>) -> Result<(), ElementError>;
}

struct One<E>(E);

impl<E: Element> ChildSource for One<E> {
    fn apply(self: Box<Self>, children: &mut ChildrenCx<'_>) -> Result<(), ElementError> {
        children.push(self.0)?;
        Ok(())
    }
}

struct Many<I>(I);

impl<I, E> ChildSource for Many<I>
where
    I: Iterator<Item = E> + 'static,
    E: Element,
{
    fn apply(self: Box<Self>, children: &mut ChildrenCx<'_>) -> Result<(), ElementError> {
        for element in self.0 {
            children.push(element)?;
        }
        Ok(())
    }
}

/// A vertical container. Bulk children are consumed without per-item boxing.
///
/// Each declaration supplies a complete child set consumed during preparation. A failed
/// consumed set requires a fresh declaration or removal; it cannot be replayed by invalidation.
pub struct Column {
    attrs: Attrs,
    children: Vec<Box<dyn ChildSource>>,
}

impl Default for Column {
    fn default() -> Self {
        Self::new()
    }
}

impl Column {
    pub fn new() -> Self {
        Self {
            attrs: Attrs::new(),
            children: Vec::new(),
        }
    }

    pub fn child<E: Element>(mut self, element: E) -> Self {
        self.children.push(Box::new(One(element)));
        self
    }

    pub fn children<I, E>(mut self, elements: I) -> Self
    where
        I: IntoIterator<Item = E>,
        I::IntoIter: 'static,
        E: Element,
    {
        self.children.push(Box::new(Many(elements.into_iter())));
        self
    }
}

impl Element for Column {
    type Mounted = MountedColumn;

    fn attrs(&self) -> &Attrs {
        &self.attrs
    }

    fn attrs_mut(&mut self) -> &mut Attrs {
        &mut self.attrs
    }

    fn into_parts(self) -> ElementParts<Self::Mounted> {
        ElementParts {
            attrs: self.attrs,
            element: MountedColumn {
                input: ChildInput::Pending(self.children),
            },
        }
    }
}

enum ChildInput {
    Pending(Vec<Box<dyn ChildSource>>),
    Applied,
    FailedConsumed,
}

#[doc(hidden)]
pub struct MountedColumn {
    input: ChildInput,
}

const BLOCK_SIZE: usize = 64;

#[derive(Clone, Copy, Default)]
struct BlockSummary {
    height: f64,
    occupied: usize,
    width: f32,
    queued: bool,
}

impl BlockSummary {
    fn include(&mut self, contribution: SizeContribution) -> Result<(), ElementError> {
        if contribution.occupied {
            self.height += f64::from(contribution.size.height);
            self.occupied = self
                .occupied
                .checked_add(1)
                .ok_or(ElementError::InvalidGeometry)?;
            self.width = self.width.max(contribution.size.width);
        }
        if !self.height.is_finite() {
            return Err(ElementError::InvalidGeometry);
        }
        Ok(())
    }
}

#[doc(hidden)]
#[derive(Default)]
pub struct ColumnState {
    blocks: Vec<BlockSummary>,
    count: usize,
    gap: f32,
    cached_natural: Size,
    valid: bool,
}

impl ColumnState {
    fn rebuild(
        &mut self,
        cx: &mut LayoutCx<'_>,
        constraints: Constraints,
        count: usize,
    ) -> Result<(), ElementError> {
        self.blocks.clear();
        let blocks = count.div_ceil(BLOCK_SIZE);
        self.blocks.reserve(blocks);

        for block in 0..blocks {
            cx.visit_size_blocks(1);
            let summary = summarize_block(cx, constraints, block, count, SizeRead::Measure)?;
            self.blocks.push(summary);
        }
        Ok(())
    }

    fn update_blocks(
        &mut self,
        cx: &mut LayoutCx<'_>,
        constraints: Constraints,
        count: usize,
        gap: f32,
        changed: Vec<ChildSizeChange>,
    ) -> Result<Option<usize>, ElementError> {
        let mut placement = None;

        for change in &changed {
            let child = child_at(cx, change.position);
            assert_eq!(
                child, change.child,
                "Column size plans retain their direct child positions"
            );
            let current = read_size(cx, child, constraints, SizeRead::Current)?;
            let from = match change.previous {
                None => Some(0),
                Some(previous) if previous.occupied != current.occupied => {
                    let previous_height = if previous.occupied {
                        previous.size.height
                    } else {
                        0.0
                    };
                    let current_height = if current.occupied {
                        current.size.height
                    } else {
                        0.0
                    };
                    if gap == 0.0 && previous_height == current_height {
                        None
                    } else {
                        Some(change.position)
                    }
                }
                Some(previous)
                    if current.occupied && previous.size.height != current.size.height =>
                {
                    Some(change.position + 1)
                }
                Some(_) => None,
            };
            if let Some(from) = from.filter(|from| *from < count) {
                placement = Some(placement.map_or(from, |previous: usize| previous.min(from)));
            }

            let block = change.position / BLOCK_SIZE;
            self.blocks[block].queued = true;
        }

        for change in changed {
            let block = change.position / BLOCK_SIZE;
            if std::mem::take(&mut self.blocks[block].queued) {
                cx.visit_size_blocks(1);
                self.blocks[block] =
                    summarize_block(cx, constraints, block, count, SizeRead::Current)?;
            }
        }

        Ok(placement)
    }

    fn natural_size(&self, cx: &mut LayoutCx<'_>, gap: f32) -> Result<Size, ElementError> {
        let mut height = 0.0_f64;
        let mut occupied = 0_usize;
        let mut width = 0.0_f32;

        for block in &self.blocks {
            cx.visit_size_blocks(1);
            height += block.height;
            occupied = occupied
                .checked_add(block.occupied)
                .ok_or(ElementError::InvalidGeometry)?;
            width = width.max(block.width);
        }

        let height = cursor_height(height, occupied, gap)?;
        let size = Size::new(width, height as f32);
        if !size.is_valid() {
            return Err(ElementError::InvalidGeometry);
        }

        Ok(size)
    }

    fn trim(&mut self) {
        if self.blocks.is_empty() {
            self.blocks = Vec::new();
            return;
        }

        let retained = self.blocks.len().max(16);
        if self.blocks.capacity() > retained.saturating_mul(4)
            && self
                .blocks
                .capacity()
                .saturating_mul(size_of::<BlockSummary>())
                > 64 * 1024
        {
            self.blocks.shrink_to(retained.saturating_mul(2));
        }
    }
}

#[derive(Clone, Copy)]
enum SizeRead {
    Measure,
    Current,
}

fn child_at(cx: &LayoutCx<'_>, index: usize) -> ElementId {
    cx.child(index)
        .expect("Column layout retains its direct child set while measuring descendants")
}

fn read_size(
    cx: &mut LayoutCx<'_>,
    child: ElementId,
    constraints: Constraints,
    read: SizeRead,
) -> Result<SizeContribution, ElementError> {
    let contribution = match read {
        SizeRead::Current => cx.size_contribution(child, constraints)?,
        SizeRead::Measure => None,
    };

    let contribution = match contribution {
        Some(contribution) => contribution,
        None => SizeContribution {
            size: cx.measure_size(child, constraints)?,
            occupied: cx.visibility(child)? != Visibility::Collapsed,
        },
    };

    if !contribution.size.is_valid() {
        return Err(ElementError::InvalidGeometry);
    }

    Ok(contribution)
}

fn summarize_block(
    cx: &mut LayoutCx<'_>,
    constraints: Constraints,
    block: usize,
    count: usize,
    read: SizeRead,
) -> Result<BlockSummary, ElementError> {
    let start = block * BLOCK_SIZE;
    let end = start.saturating_add(BLOCK_SIZE).min(count);
    let mut summary = BlockSummary::default();

    for index in start..end {
        let child = child_at(cx, index);
        let contribution = read_size(cx, child, constraints, read)?;
        summary.include(contribution)?;
    }
    Ok(summary)
}

fn cursor_height(height: f64, occupied: usize, gap: f32) -> Result<f64, ElementError> {
    let cursor = height + f64::from(gap) * occupied.saturating_sub(1) as f64;
    if !cursor.is_finite() {
        return Err(ElementError::InvalidGeometry);
    }
    Ok(cursor)
}

fn place_suffix(
    state: &ColumnState,
    cx: &mut LayoutCx<'_>,
    constraints: Constraints,
    count: usize,
    gap: f32,
    from: usize,
) -> Result<(), ElementError> {
    if from >= count {
        return Ok(());
    }

    let first_block = from / BLOCK_SIZE;
    let mut height = 0.0_f64;
    let mut occupied = 0_usize;
    for block in &state.blocks[..first_block] {
        cx.visit_size_blocks(1);
        height += block.height;
        occupied = occupied
            .checked_add(block.occupied)
            .ok_or(ElementError::InvalidGeometry)?;
    }

    for (block_index, block) in state.blocks.iter().enumerate().skip(first_block) {
        cx.visit_size_blocks(1);
        let start = block_index * BLOCK_SIZE;
        let end = start.saturating_add(BLOCK_SIZE).min(count);
        let mut cursor = cursor_height(height, occupied, gap)?;
        let mut has_occupied = occupied != 0;

        for index in start..end {
            let child = child_at(cx, index);
            let contribution = read_size(cx, child, constraints, SizeRead::Current)?;
            if contribution.occupied && has_occupied {
                cursor += f64::from(gap);
            }
            let origin = cursor as f32;
            if !origin.is_finite() {
                return Err(ElementError::InvalidGeometry);
            }
            if index >= from {
                cx.place(child, Point::new(0.0, origin))?;
            }
            if contribution.occupied {
                cursor += f64::from(contribution.size.height);
                has_occupied = true;
            }
        }
        height += block.height;
        occupied = occupied
            .checked_add(block.occupied)
            .ok_or(ElementError::InvalidGeometry)?;
    }
    Ok(())
}

impl MountedElement for MountedColumn {
    type State = ColumnState;

    fn mount(&mut self, _: &mut MountCx<'_>) -> Result<Self::State, ElementError> {
        Ok(ColumnState::default())
    }

    fn update(
        &mut self,
        state: &mut Self::State,
        reason: UpdateReason<'_, Self>,
        cx: &mut UpdateCx<'_>,
    ) -> Result<(), ElementError> {
        let recovery = cx.recovery_required();
        if recovery || matches!(self.input, ChildInput::FailedConsumed) {
            state.valid = false;
        }
        if matches!(self.input, ChildInput::FailedConsumed) {
            return Err(ElementError::InitializerConsumed);
        }

        if recovery
            || matches!(reason, UpdateReason::Replacement { .. })
            || matches!(self.input, ChildInput::Pending(_))
        {
            let work = if recovery {
                Work::PREPARE | Work::LAYOUT
            } else {
                Work::PREPARE
            };
            cx.invalidate(work);
        }

        Ok(())
    }

    fn prepare(
        &mut self,
        state: &mut Self::State,
        cx: &mut UpdateCx<'_>,
    ) -> Result<(), ElementError> {
        let reusable = state.valid && !cx.recovery_required();
        state.valid = false;
        let sources = match std::mem::replace(&mut self.input, ChildInput::FailedConsumed) {
            ChildInput::Pending(sources) => sources,
            ChildInput::Applied => {
                self.input = ChildInput::Applied;
                state.valid = reusable;
                return Ok(());
            }
            ChildInput::FailedConsumed => return Err(ElementError::InitializerConsumed),
        };

        cx.children(|children| {
            for source in sources {
                source.apply(children)?;
            }
            Ok(())
        })?;

        self.input = ChildInput::Applied;

        // Summaries still describe the last successful parent use; the next size plan
        // separately validates membership and any new child sizing obligations.
        state.valid = reusable;

        Ok(())
    }

    fn layout(
        &mut self,
        state: &mut Self::State,
        cx: &mut LayoutCx<'_>,
        constraints: Constraints,
    ) -> Result<Size, ElementError> {
        let reusable = state.valid;
        state.valid = false;
        if matches!(self.input, ChildInput::FailedConsumed) {
            return Err(ElementError::InitializerConsumed);
        }

        let gap = cx.get(Gap);
        if !constraints.is_valid() || !gap.is_finite() || gap < 0.0 {
            return Err(ElementError::InvalidGeometry);
        }

        let count = cx.child_count();
        let child_constraints = Constraints::loose(Size::new(constraints.max.width, f32::INFINITY));
        let plan = cx.size_plan(child_constraints)?;
        let reusable = reusable
            && !cx.recovery_required()
            && state.count == count
            && state.gap == gap
            && state.blocks.len() == count.div_ceil(BLOCK_SIZE);

        let (natural, placement) = match plan {
            SizePlan::Incremental { changed } if reusable && changed.is_empty() => {
                (state.cached_natural, None)
            }
            SizePlan::Incremental { changed } if reusable => {
                let placement = state.update_blocks(cx, child_constraints, count, gap, changed)?;
                (state.natural_size(cx, gap)?, placement)
            }
            SizePlan::Rebuild | SizePlan::Incremental { .. } => {
                state.rebuild(cx, child_constraints, count)?;
                (state.natural_size(cx, gap)?, (count != 0).then_some(0))
            }
        };
        if let Some(from) = placement {
            place_suffix(state, cx, child_constraints, count, gap, from)?;
        }

        cx.content_size(natural)?;
        let size = constrain(cx, constraints, natural)?;

        if cx.size_inputs_current() {
            state.cached_natural = natural;
            state.count = count;
            state.gap = gap;
            state.trim();
            state.valid = true;
        }

        Ok(size)
    }
}
