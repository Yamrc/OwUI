use std::sync::Arc;

use crate::element::{
    Attrs, Background, Element, ElementError, ElementParts, Enabled, Foreground, MountCx,
    MountedElement, UpdateCx, UpdateReason,
};
use crate::input::{
    Activate, EventCx, EventPhase, InputEvent, KeyCode, PointerId, PointerKind, Propagation, Role,
    SemanticContent, SemanticCx,
};
use crate::layout::{Constraints, LayoutCx, Point, Rect, Size};
use crate::scene::SceneCx;
use crate::schedule::Work;

use super::constrain;

/// A keyboard, pointer, and semantic activation target.
pub struct Button {
    attrs: Attrs,
    label: Arc<str>,
}

impl Button {
    pub fn new(label: impl Into<Arc<str>>) -> Self {
        Self {
            attrs: Attrs::new(),
            label: label.into(),
        }
    }
}

#[doc(hidden)]
#[derive(Default)]
pub struct ButtonState {
    pressed: Option<PointerId>,
    key_down: bool,
}

impl Element for Button {
    type Mounted = MountedButton;

    fn attrs(&self) -> &Attrs {
        &self.attrs
    }

    fn attrs_mut(&mut self) -> &mut Attrs {
        &mut self.attrs
    }

    fn into_parts(self) -> ElementParts<Self::Mounted> {
        ElementParts {
            attrs: self.attrs,
            element: MountedButton { label: self.label },
        }
    }
}

#[doc(hidden)]
pub struct MountedButton {
    label: Arc<str>,
}

impl MountedElement for MountedButton {
    type State = ButtonState;

    fn mount(&mut self, _: &mut MountCx<'_>) -> Result<Self::State, ElementError> {
        Ok(ButtonState::default())
    }

    fn update(
        &mut self,
        state: &mut Self::State,
        reason: UpdateReason<'_, Self>,
        cx: &mut UpdateCx<'_>,
    ) -> Result<(), ElementError> {
        let recovery = cx.recovery_required();
        let mut work = Work::empty();
        if !cx.get(Enabled) {
            if state.pressed.take().is_some() {
                work |= Work::SCENE;
            }
            state.key_down = false;
        }

        if recovery {
            work |= Work::LAYOUT | Work::SCENE | Work::SEMANTICS;
        } else if let UpdateReason::Replacement { previous } = reason
            && self.label != previous.label
        {
            work |= Work::SCENE | Work::SEMANTICS;
        }

        if !work.is_empty() {
            cx.invalidate(work);
        }

        Ok(())
    }

    fn layout(
        &mut self,
        _: &mut Self::State,
        cx: &mut LayoutCx<'_>,
        constraints: Constraints,
    ) -> Result<Size, ElementError> {
        cx.hit_testable(true)?;
        constrain(cx, constraints, Size::new(96.0, 28.0))
    }

    fn event(
        &self,
        state: &mut Self::State,
        cx: &mut EventCx<'_>,
        event: &InputEvent,
    ) -> Propagation {
        if cx.phase() != EventPhase::Target {
            return Propagation::Continue;
        }

        let enabled = cx.get(Enabled);
        match event {
            InputEvent::Pointer {
                id,
                kind: PointerKind::Down,
                ..
            } if enabled && state.pressed.is_none() => {
                state.pressed = Some(*id);
                cx.capture(*id);
                cx.focus();
                cx.invalidate(Work::SCENE);
                Propagation::Stop
            }
            InputEvent::Pointer {
                id,
                kind: PointerKind::Up,
                ..
            } if state.pressed == Some(*id) => {
                let activate = enabled && cx.hit() == Some(true);
                state.pressed = None;
                cx.release(*id);
                cx.invalidate(Work::SCENE);
                if activate {
                    cx.emit(Activate, ());
                }
                Propagation::Stop
            }
            InputEvent::Pointer {
                id,
                kind: PointerKind::Cancel,
                ..
            } if state.pressed == Some(*id) => {
                state.pressed = None;
                cx.release(*id);
                cx.invalidate(Work::SCENE);
                Propagation::Continue
            }
            InputEvent::Focus(false) => {
                if let Some(pointer) = state.pressed.take() {
                    cx.release(pointer);
                    cx.invalidate(Work::SCENE);
                }
                state.key_down = false;
                Propagation::Continue
            }
            InputEvent::Key {
                code: KeyCode::Enter | KeyCode::Space,
                pressed,
            } if enabled => {
                if *pressed && !state.key_down {
                    cx.emit(Activate, ());
                }
                state.key_down = *pressed;
                Propagation::Stop
            }
            InputEvent::Activate if enabled => {
                cx.emit(Activate, ());
                Propagation::Stop
            }
            _ => Propagation::Continue,
        }
    }

    fn scene(&self, state: &Self::State, cx: &mut SceneCx<'_>) -> Result<(), ElementError> {
        let mut color = cx.get(Background);
        if color.a != 0.0 {
            if state.pressed.is_some() {
                color.r *= 0.75;
                color.g *= 0.75;
                color.b *= 0.75;
            }
            cx.rect(Rect::new(Point::ZERO, cx.size()), color);
        }

        cx.text(
            Point::new(4.0, 4.0),
            Arc::clone(&self.label),
            cx.get(Foreground),
        );
        Ok(())
    }

    fn semantics(&self, _: &Self::State, _: &SemanticCx<'_>) -> SemanticContent {
        SemanticContent {
            role: Role::Button,
            name: Some(Arc::clone(&self.label)),
            focusable: true,
            ..SemanticContent::default()
        }
    }
}
