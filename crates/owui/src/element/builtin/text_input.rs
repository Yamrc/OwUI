use std::sync::Arc;

use crate::element::{
    Attrs, Element, ElementError, ElementParts, Enabled, Foreground, MountCx, MountedElement, Name,
    UpdateCx, UpdateReason,
};
use crate::input::{
    Changed, EventCx, EventPhase, InputEvent, KeyCode, PointerKind, Propagation, Role,
    SemanticContent, SemanticCx, Submit,
};
use crate::layout::{Constraints, LayoutCx, Point, Size};
use crate::scene::SceneCx;
use crate::schedule::Work;

use super::constrain;

/// An internally owned edit buffer with explicit submission.
///
/// Text is appended at the end of the buffer. Backspace removes its final
/// Unicode scalar value. Composition text is held separately until committed;
/// losing focus cancels composition without submitting the edit. Changed events require an
/// actual text mutation; submission remains repeatable even when the text is unchanged.
pub struct TextInput {
    attrs: Attrs,
    initial: Option<String>,
    reset: Option<String>,
}

impl TextInput {
    pub fn new() -> Self {
        Self {
            attrs: Attrs::new(),
            initial: None,
            reset: None,
        }
    }

    /// Supplies the initial buffer on first mount; compatible replacements retain editing State.
    pub fn initial(mut self, text: impl Into<String>) -> Self {
        self.initial = Some(text.into());
        self
    }

    /// Replaces the buffer on mount or the next compatible update and cancels composition.
    ///
    /// Reset takes precedence over initial text and does not emit an input event.
    pub fn reset(mut self, text: impl Into<String>) -> Self {
        self.reset = Some(text.into());
        self
    }
}

impl Default for TextInput {
    fn default() -> Self {
        Self::new()
    }
}

/// Persistent editing data owned by a text input instance.
#[derive(Default)]
pub struct EditState {
    text: String,
    composition: String,
}

impl EditState {
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn composition(&self) -> &str {
        &self.composition
    }
}

impl Element for TextInput {
    type Mounted = MountedTextInput;

    fn attrs(&self) -> &Attrs {
        &self.attrs
    }

    fn attrs_mut(&mut self) -> &mut Attrs {
        &mut self.attrs
    }

    fn into_parts(self) -> ElementParts<Self::Mounted> {
        ElementParts {
            attrs: self.attrs,
            element: MountedTextInput {
                initial: self.initial,
                reset: self.reset,
            },
        }
    }
}

#[doc(hidden)]
pub struct MountedTextInput {
    initial: Option<String>,
    reset: Option<String>,
}

impl MountedElement for MountedTextInput {
    type State = EditState;

    fn mount(&mut self, _: &mut MountCx<'_>) -> Result<Self::State, ElementError> {
        let mut text = self
            .reset
            .take()
            .or(self.initial.take())
            .unwrap_or_default();
        trim_edit_buffer(&mut text);

        Ok(EditState {
            text,
            ..EditState::default()
        })
    }

    fn update(
        &mut self,
        state: &mut Self::State,
        _reason: UpdateReason<'_, Self>,
        cx: &mut UpdateCx<'_>,
    ) -> Result<(), ElementError> {
        let recovery = cx.recovery_required();
        self.initial = None;
        if let Some(text) = self.reset.take()
            && (recovery || state.text != text || !state.composition.is_empty())
        {
            state.text = text;
            state.composition = String::new();
            trim_edit_buffer(&mut state.text);
            cx.invalidate(Work::LAYOUT | Work::SCENE | Work::SEMANTICS);
        } else if recovery {
            cx.invalidate(Work::LAYOUT | Work::SCENE | Work::SEMANTICS);
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
        constrain(cx, constraints, Size::new(240.0, 28.0))
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
                kind: PointerKind::Down,
                ..
            } if enabled => {
                cx.focus();
                Propagation::Stop
            }
            InputEvent::Focus(false) => {
                if !state.composition.is_empty() {
                    state.composition = String::new();
                    cx.invalidate(Work::SCENE);
                }
                Propagation::Continue
            }
            InputEvent::Text(text) if enabled => {
                if !text.is_empty() {
                    state.text.push_str(text);
                    cx.emit_with(Changed, || state.text.clone());
                    cx.invalidate(Work::SCENE | Work::SEMANTICS);
                }
                Propagation::Stop
            }
            InputEvent::Composition { text, committed } if enabled => {
                if *committed {
                    let had_composition = !state.composition.is_empty();
                    state.composition = String::new();
                    if !text.is_empty() {
                        state.text.push_str(text);
                        cx.emit_with(Changed, || state.text.clone());
                        cx.invalidate(Work::SCENE | Work::SEMANTICS);
                    } else if had_composition {
                        cx.invalidate(Work::SCENE);
                    }
                } else if state.composition != *text {
                    state.composition.clone_from(text);
                    trim_edit_buffer(&mut state.composition);
                    cx.invalidate(Work::SCENE);
                }
                Propagation::Stop
            }
            InputEvent::Key {
                code: KeyCode::Backspace,
                pressed: true,
            } if enabled => {
                if state.composition.is_empty() && state.text.pop().is_some() {
                    trim_edit_buffer(&mut state.text);
                    cx.emit_with(Changed, || state.text.clone());
                    cx.invalidate(Work::SCENE | Work::SEMANTICS);
                }
                Propagation::Stop
            }
            InputEvent::Key {
                code: KeyCode::Enter,
                pressed: true,
            } if enabled => {
                if state.composition.is_empty() {
                    cx.emit_with(Submit, || state.text.clone());
                }
                Propagation::Stop
            }
            InputEvent::Key {
                code: KeyCode::Escape,
                pressed: true,
            } => {
                if !state.composition.is_empty() {
                    state.composition = String::new();
                    cx.invalidate(Work::SCENE);
                }
                Propagation::Stop
            }
            _ => Propagation::Continue,
        }
    }

    fn scene(&self, state: &Self::State, cx: &mut SceneCx<'_>) -> Result<(), ElementError> {
        cx.text(
            Point::new(4.0, 4.0),
            state.text.as_str(),
            cx.get(Foreground),
        );
        Ok(())
    }

    fn semantics(&self, state: &Self::State, cx: &SemanticCx<'_>) -> SemanticContent {
        SemanticContent {
            role: Role::TextInput,
            name: cx.get_ref(Name).cloned(),
            value: Some(Arc::from(state.text.as_str())),
            focusable: true,
            ..SemanticContent::default()
        }
    }
}

fn trim_edit_buffer(buffer: &mut String) {
    if buffer.is_empty() {
        *buffer = String::new();
        return;
    }

    let retained = buffer.len().max(16);
    if buffer.capacity() > retained.saturating_mul(4) && buffer.capacity() > 64 * 1024 {
        buffer.shrink_to(retained.saturating_mul(2));
    }
}
