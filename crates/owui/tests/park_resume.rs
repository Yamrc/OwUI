use std::cell::Cell;
use std::rc::Rc;

use owui::application::{App, AppError, Window};
use owui::element::{
    Attrs, Column, Element, ElementError, ElementExt, ElementId, ElementParts, MountCx,
    MountedElement, UpdateCx,
};
use owui::layout::{Constraints, LayoutCx, Point, Size};
use owui::schedule::Work;
use owui::view::{BuildCx, View};

#[derive(Default)]
struct Model {
    parked: Cell<bool>,
    height: Cell<f32>,
    leaf_layouts: Cell<usize>,
}

struct Leaf(Rc<Model>);

impl MountedElement for Leaf {
    type State = ();

    fn mount(&mut self, _: &mut MountCx<'_>) -> Result<(), ElementError> {
        Ok(())
    }

    fn layout(
        &mut self,
        _: &mut (),
        _: &mut LayoutCx<'_>,
        constraints: Constraints,
    ) -> Result<Size, ElementError> {
        self.0.leaf_layouts.set(self.0.leaf_layouts.get() + 1);
        Ok(constraints.constrain(Size::new(32.0, self.0.height.get())))
    }
}

struct Host(Rc<Model>);

impl MountedElement for Host {
    type State = ();

    fn mount(&mut self, _: &mut MountCx<'_>) -> Result<(), ElementError> {
        Ok(())
    }

    fn prepare(&mut self, _: &mut (), cx: &mut UpdateCx<'_>) -> Result<(), ElementError> {
        cx.children(|children| {
            children.realize(
                "column",
                Column::new().child(
                    ElementParts {
                        attrs: Attrs::new(),
                        element: Leaf(Rc::clone(&self.0)),
                    }
                    .key("leaf"),
                ),
            )?;
            Ok(())
        })
    }

    fn layout(
        &mut self,
        _: &mut (),
        cx: &mut LayoutCx<'_>,
        constraints: Constraints,
    ) -> Result<Size, ElementError> {
        let column = cx.child(0).expect("one prepared column");
        if self.0.parked.get() {
            cx.park(column)?;
        } else {
            cx.measure(column, Constraints::loose(Size::new(120.0, 120.0)))?;
            cx.place(column, Point::ZERO)?;
        }
        Ok(constraints.min)
    }
}

struct Page(Rc<Model>);

impl View for Page {
    fn build(&self, _: &BuildCx<'_, Self>) -> impl Element {
        ElementParts {
            attrs: Attrs::new(),
            element: Host(Rc::clone(&self.0)),
        }
    }
}

fn park_fixture() -> Result<
    (
        App,
        owui::application::WindowId,
        ElementId,
        ElementId,
        Rc<Model>,
    ),
    AppError,
> {
    let mut app = App::new();
    let model = Rc::new(Model::default());
    model.height.set(10.0);
    let window = app.window(Window::new("parked layout debt").size(120.0, 120.0), {
        let model = Rc::clone(&model);
        move |_| Page(model)
    })?;
    let host = app.child(window, app.root(window)?, 0_u64)?.expect("host");
    let column = app.child(window, host, "column")?.expect("column");
    let leaf = app.child(window, column, "leaf")?.expect("leaf");
    assert_eq!(app.geometry(window, leaf)?.unwrap().size.height, 10.0);
    Ok((app, window, host, leaf, model))
}

fn park(app: &mut App, window: owui::application::WindowId, host: ElementId, model: &Model) {
    model.parked.set(true);
    app.invalidate(window, host, Work::LAYOUT).unwrap();
    app.update(window).unwrap();
}

fn unpark(app: &mut App, window: owui::application::WindowId, host: ElementId, model: &Model) {
    model.parked.set(false);
    app.invalidate(window, host, Work::LAYOUT).unwrap();
    app.update(window).unwrap();
}

#[test]
fn a_clean_parked_subtree_can_resume_from_its_measurement_cache() -> Result<(), AppError> {
    let (mut app, window, host, leaf, model) = park_fixture()?;
    park(&mut app, window, host, &model);
    assert!(app.geometry(window, leaf)?.is_none());

    let calls = model.leaf_layouts.get();
    unpark(&mut app, window, host, &model);
    assert_eq!(model.leaf_layouts.get(), calls);
    assert_eq!(app.geometry(window, leaf)?.unwrap().size.height, 10.0);

    app.close(window)?;
    Ok(())
}

#[test]
fn demanded_descendant_layout_debt_is_remeasured_when_a_parked_branch_resumes()
-> Result<(), AppError> {
    let (mut app, window, host, leaf, model) = park_fixture()?;
    park(&mut app, window, host, &model);
    let calls = model.leaf_layouts.get();

    model.height.set(20.0);
    app.invalidate(window, leaf, Work::LAYOUT)?;
    app.update(window)?;
    assert_eq!(model.leaf_layouts.get(), calls);
    assert!(app.geometry(window, leaf)?.is_none());

    unpark(&mut app, window, host, &model);
    assert!(model.leaf_layouts.get() > calls);
    assert_eq!(app.geometry(window, leaf)?.unwrap().size.height, 20.0);
    let column = app.child(window, host, "column")?.expect("column");
    assert_eq!(app.geometry(window, column)?.unwrap().size.height, 20.0);
    assert_eq!(app.geometry(window, host)?.unwrap().size.height, 120.0);

    app.close(window)?;
    Ok(())
}
