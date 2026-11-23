use std::cell::Cell;
use std::rc::Rc;

use owui::application::{App, AppError, Window};
use owui::color::Color;
use owui::element::{
    Attrs, Column, Element, ElementError, ElementExt, ElementId, ElementParts, Foreground, MountCx,
    MountedElement, Text, UpdateCx, UpdateReason,
};
use owui::layout::{Constraints, LayoutCx, Point, Rect, Size};
use owui::scene::{Primitive, SceneCx};
use owui::schedule::Work;
use owui::view::{BuildCx, View};

struct RetainedChild;

impl View for RetainedChild {
    fn build(&self, _: &BuildCx<'_, Self>) -> impl Element {
        Text::new("retained").size(Size::new(40.0, 12.0))
    }
}

struct Theme(Rc<Cell<bool>>);

impl View for Theme {
    fn build(&self, cx: &BuildCx<'_, Self>) -> impl Element {
        let column = Column::new().child(cx.view("child", |_| RetainedChild));
        if self.0.get() {
            column.foreground(Color::BLACK)
        } else {
            column
        }
    }
}

fn text_color(app: &App, window: owui::application::WindowId, id: ElementId) -> Color {
    let fragment = app
        .scene(window)
        .unwrap()
        .fragment(id)
        .expect("visible text has a fragment");
    let Primitive::Text { color, .. } = fragment.primitives[0] else {
        panic!("expected text primitive")
    };
    color
}

#[test]
fn removing_column_overlay_updates_builtin_text_in_retained_child_view() -> Result<(), AppError> {
    let themed = Rc::new(Cell::new(true));
    let mut app = App::new();
    let window = app.window(Window::new("retained inherited theme"), {
        let themed = Rc::clone(&themed);
        move |_| Theme(Rc::clone(&themed))
    })?;

    let root = app.root(window)?;
    let column = app.child(window, root, 0_u64)?.expect("column");
    let child_view = app.child(window, column, "child")?.expect("child view");
    let text = app.child(window, child_view, 0_u64)?.expect("text");
    assert_eq!(text_color(&app, window, text), Color::BLACK);

    themed.set(false);
    app.invalidate(window, root, Work::PREPARE)?;
    app.update(window)?;

    assert_eq!(app.child(window, column, "child")?, Some(child_view));
    assert_eq!(app.child(window, child_view, 0_u64)?, Some(text));
    assert_eq!(text_color(&app, window, text), Color::WHITE);

    app.close(window)?;
    Ok(())
}

struct CachedLeaf;

impl MountedElement for CachedLeaf {
    type State = Color;

    fn mount(&mut self, cx: &mut MountCx<'_>) -> Result<Self::State, ElementError> {
        Ok(cx.get(Foreground))
    }

    fn update(
        &mut self,
        state: &mut Self::State,
        _: UpdateReason<'_, Self>,
        cx: &mut UpdateCx<'_>,
    ) -> Result<(), ElementError> {
        *state = cx.get(Foreground);
        cx.invalidate(Work::SCENE);
        Ok(())
    }

    fn layout(
        &mut self,
        _: &mut Self::State,
        _: &mut LayoutCx<'_>,
        constraints: Constraints,
    ) -> Result<Size, ElementError> {
        Ok(constraints.constrain(Size::new(24.0, 12.0)))
    }

    fn scene(&self, state: &Self::State, cx: &mut SceneCx<'_>) -> Result<(), ElementError> {
        let bounds = Rect::new(Point::ZERO, cx.size());
        cx.rect(bounds, *state);
        cx.rect(bounds, cx.get(Foreground));
        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ProviderChange {
    Install,
    RemoveWithoutChildEdit,
    RemoveAndKeep,
}

struct Provider {
    change: Rc<Cell<ProviderChange>>,
}

impl MountedElement for Provider {
    type State = Option<ElementId>;

    fn mount(&mut self, cx: &mut MountCx<'_>) -> Result<Self::State, ElementError> {
        cx.set(Foreground, Color::BLACK)?;
        Ok(None)
    }

    fn update(
        &mut self,
        _: &mut Self::State,
        _: UpdateReason<'_, Self>,
        _: &mut UpdateCx<'_>,
    ) -> Result<(), ElementError> {
        Ok(())
    }

    fn prepare(
        &mut self,
        state: &mut Self::State,
        cx: &mut UpdateCx<'_>,
    ) -> Result<(), ElementError> {
        match (*state, self.change.get()) {
            (None, _) => {
                *state = Some(cx.children(|children| {
                    children.realize(
                        "leaf",
                        ElementParts {
                            attrs: Attrs::new(),
                            element: CachedLeaf,
                        },
                    )
                })?);
            }
            (Some(_), ProviderChange::Install) => cx.set(Foreground, Color::BLACK)?,
            (Some(_), ProviderChange::RemoveWithoutChildEdit) => {
                cx.remove(Foreground)?;
            }
            (Some(leaf), ProviderChange::RemoveAndKeep) => {
                cx.remove(Foreground)?;
                cx.children(|children| children.keep(leaf))?;
            }
        }

        Ok(())
    }

    fn layout(
        &mut self,
        _: &mut Self::State,
        cx: &mut LayoutCx<'_>,
        constraints: Constraints,
    ) -> Result<Size, ElementError> {
        let leaf = cx.child(0).expect("the provider retains one leaf");
        let size = cx.measure(leaf, constraints)?;
        cx.place(leaf, Point::ZERO)?;
        Ok(size)
    }
}

struct ProviderView(Rc<Cell<ProviderChange>>);

impl View for ProviderView {
    fn build(&self, _: &BuildCx<'_, Self>) -> impl Element {
        ElementParts {
            attrs: Attrs::new(),
            element: Provider {
                change: Rc::clone(&self.0),
            },
        }
    }
}

fn cached_leaf_colors(
    app: &App,
    window: owui::application::WindowId,
    leaf: ElementId,
) -> Result<[Color; 2], AppError> {
    let fragment = app.scene(window)?.fragment(leaf).expect("visible leaf");
    let [
        Primitive::Rect {
            color: cached_state,
            ..
        },
        Primitive::Rect {
            color: current_config,
            ..
        },
    ] = fragment.primitives.as_slice()
    else {
        panic!("expected cached-state and current-configuration primitives")
    };
    Ok([*cached_state, *current_config])
}

fn remove_provider_override(keep: bool) -> Result<[Color; 2], AppError> {
    let change = Rc::new(Cell::new(ProviderChange::Install));
    let mut app = App::new();
    let window = app.window(Window::new("runtime inherited source"), {
        let change = Rc::clone(&change);
        move |_| ProviderView(change)
    })?;
    let provider = app
        .child(window, app.root(window)?, 0_u64)?
        .expect("provider");
    let leaf = app.child(window, provider, "leaf")?.expect("leaf");
    assert_eq!(cached_leaf_colors(&app, window, leaf)?, [Color::BLACK; 2]);

    change.set(if keep {
        ProviderChange::RemoveAndKeep
    } else {
        ProviderChange::RemoveWithoutChildEdit
    });
    app.invalidate(window, provider, Work::PREPARE)?;
    app.update(window)?;
    assert_eq!(app.child(window, provider, "leaf")?, Some(leaf));

    let colors = cached_leaf_colors(&app, window, leaf)?;
    app.close(window)?;
    Ok(colors)
}

#[test]
fn removing_last_runtime_overlay_refreshes_untouched_retained_child() -> Result<(), AppError> {
    assert_eq!(remove_provider_override(false)?, [Color::WHITE; 2]);
    Ok(())
}

#[test]
fn keeping_child_while_removing_last_overlay_delivers_new_values_before_scene()
-> Result<(), AppError> {
    assert_eq!(remove_provider_override(true)?, [Color::WHITE; 2]);
    Ok(())
}
