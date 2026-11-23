use std::sync::Arc;

use crate::element::{
    Attrs, Element, ElementError, ElementParts, Foreground, MountCx, MountedElement, UpdateCx,
    UpdateReason,
};
use crate::input::{Role, SemanticContent, SemanticCx};
use crate::layout::{Constraints, LayoutCx, Point, Size};
use crate::scene::SceneCx;
use crate::schedule::Work;

use super::constrain;

/// Unshaped text data with caller-supplied logical measurements.
///
/// The natural size defaults to zero. Callers provide logical measurements through
/// `size`, width, or height; the host's text system performs shaping and measurement.
pub struct Text {
    attrs: Attrs,
    text: Arc<str>,
    size: Size,
}

impl Text {
    pub fn new(text: impl Into<Arc<str>>) -> Self {
        Self {
            attrs: Attrs::new(),
            text: text.into(),
            size: Size::ZERO,
        }
    }

    /// Sets the natural logical size before [`crate::element::Width`] and
    /// [`crate::element::Height`] property overrides.
    pub fn size(mut self, size: Size) -> Self {
        self.size = size;
        self
    }
}

impl Element for Text {
    type Mounted = MountedText;

    fn attrs(&self) -> &Attrs {
        &self.attrs
    }

    fn attrs_mut(&mut self) -> &mut Attrs {
        &mut self.attrs
    }

    fn into_parts(self) -> ElementParts<Self::Mounted> {
        ElementParts {
            attrs: self.attrs,
            element: MountedText {
                text: self.text,
                size: self.size,
            },
        }
    }
}

#[doc(hidden)]
pub struct MountedText {
    text: Arc<str>,
    size: Size,
}

impl MountedElement for MountedText {
    type State = ();

    fn mount(&mut self, _: &mut MountCx<'_>) -> Result<(), ElementError> {
        Ok(())
    }

    fn update(
        &mut self,
        _: &mut (),
        reason: UpdateReason<'_, Self>,
        cx: &mut UpdateCx<'_>,
    ) -> Result<(), ElementError> {
        if cx.recovery_required() {
            cx.invalidate(Work::LAYOUT | Work::SCENE | Work::SEMANTICS);
            return Ok(());
        }

        let mut work = Work::empty();
        if let UpdateReason::Replacement { previous } = reason {
            if self.size != previous.size {
                work |= Work::LAYOUT;
            }
            if self.text != previous.text {
                work |= Work::SCENE | Work::SEMANTICS;
            }
        }

        if !work.is_empty() {
            cx.invalidate(work);
        }

        Ok(())
    }

    fn layout(
        &mut self,
        _: &mut (),
        cx: &mut LayoutCx<'_>,
        constraints: Constraints,
    ) -> Result<Size, ElementError> {
        constrain(cx, constraints, self.size)
    }

    fn scene(&self, _: &(), cx: &mut SceneCx<'_>) -> Result<(), ElementError> {
        cx.text(Point::ZERO, Arc::clone(&self.text), cx.get(Foreground));
        Ok(())
    }

    fn semantics(&self, _: &(), _: &SemanticCx<'_>) -> SemanticContent {
        SemanticContent {
            role: Role::Text,
            name: Some(Arc::clone(&self.text)),
            ..SemanticContent::default()
        }
    }
}
