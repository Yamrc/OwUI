use owui::application::{App, AppError, Window};
use owui::element::{
    Attrs, Button, Element, ElementError, ElementParts, MountCx, MountedElement, UpdateCx,
};
use owui::input::{InputEvent, PointerId, PointerKind};
use owui::layout::{Constraints, LayoutCx, Point, Size};
use owui::view::{BuildCx, View};

struct Offset;

impl MountedElement for Offset {
    type State = ();

    fn mount(&mut self, _: &mut MountCx<'_>) -> Result<(), ElementError> {
        Ok(())
    }

    fn prepare(&mut self, _: &mut (), cx: &mut UpdateCx<'_>) -> Result<(), ElementError> {
        cx.children(|children| {
            children.realize("button", Button::new("edge"))?;
            Ok(())
        })
    }

    fn layout(
        &mut self,
        _: &mut (),
        cx: &mut LayoutCx<'_>,
        constraints: Constraints,
    ) -> Result<Size, ElementError> {
        let button = cx.child(0).expect("prepared button");
        cx.measure(button, Constraints::tight(Size::new(2.0, 20.0)))?;
        cx.place(button, Point::new(-1.0, 0.0))?;
        Ok(constraints.min)
    }
}

struct Page;

impl View for Page {
    fn build(&self, _: &BuildCx<'_, Self>) -> impl Element {
        ElementParts {
            attrs: Attrs::new(),
            element: Offset,
        }
    }
}

fn move_pointer(
    app: &mut App,
    window: owui::application::WindowId,
    x: f32,
) -> Result<Option<owui::element::ElementId>, AppError> {
    app.dispatch(
        window,
        InputEvent::Pointer {
            id: PointerId(0),
            kind: PointerKind::Move,
            position: Point::new(x, 4.0),
        },
    )
}

#[test]
fn default_hit_refinement_agrees_with_included_world_edge_after_rounding() -> Result<(), AppError> {
    let mut app = App::new();
    let window = app.window(Window::new("fractional hit edge").size(100.0, 40.0), |_| {
        Page
    })?;
    let offset = app
        .child(window, app.root(window)?, 0_u64)?
        .expect("offset");
    let button = app.child(window, offset, "button")?.expect("button");
    let bounds = app.geometry(window, button)?.expect("geometry").bounds;
    let clip = app.clip_bounds(window, button)?.expect("clip");
    let included = 1.0_f32.next_down();
    assert!(bounds.contains(Point::new(included, 4.0)));
    assert!(clip.contains(Point::new(included, 4.0)));

    assert_eq!(move_pointer(&mut app, window, 0.5)?, Some(button));
    assert_eq!(move_pointer(&mut app, window, 1.0)?, None);
    assert_eq!(move_pointer(&mut app, window, included)?, Some(button));

    app.close(window)?;
    Ok(())
}
