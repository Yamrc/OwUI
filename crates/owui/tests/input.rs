use owui::application::{App, Window};
use owui::color::Color;
use owui::element::{Button, Column, Element, ElementExt, ElementId};
use owui::input::{Activate, InputEvent, PointerId, PointerKind};
use owui::layout::Point;
use owui::scene::Primitive;
use owui::view::{BuildCx, Rebuild, View};

struct ClickView {
    activations: usize,
}

impl View for ClickView {
    fn build(&self, cx: &BuildCx<'_, Self>) -> impl Element {
        let button = Button::new(format!("activations: {}", self.activations))
            .key("clicker")
            .background(Color::WHITE)
            .on(
                Activate,
                cx.action(|this, _, _: &()| {
                    this.activations += 1;
                    Rebuild::This
                }),
            );
        Column::new().child(button)
    }
}

fn background(app: &App, window: owui::application::WindowId, button: ElementId) -> Color {
    let fragment = app
        .scene(window)
        .unwrap()
        .fragment(button)
        .expect("visible button has a scene fragment");
    match fragment.primitives.first() {
        Some(Primitive::Rect { color, .. }) => *color,
        primitive => panic!("expected button background rectangle, found {primitive:?}"),
    }
}

#[test]
fn pointer_down_after_host_blur_does_not_latch_pressed_state_without_capture() {
    let mut app = App::new();
    let window = app
        .window(
            Window::new("Blurred pointer admission").size(200.0, 80.0),
            |_| ClickView { activations: 0 },
        )
        .unwrap();
    let root = app.root(window).unwrap();
    let column = app.child(window, root, 0_u64).unwrap().unwrap();
    let button = app.child(window, column, "clicker").unwrap().unwrap();

    app.dispatch(window, InputEvent::Focus(false)).unwrap();
    assert_eq!(background(&app, window, button), Color::WHITE);
    assert_eq!(
        app.dispatch(
            window,
            InputEvent::Pointer {
                id: PointerId(7),
                kind: PointerKind::Down,
                position: Point::new(10.0, 10.0),
            },
        )
        .unwrap(),
        None
    );
    assert_eq!(background(&app, window, button), Color::WHITE);

    app.dispatch(
        window,
        InputEvent::Pointer {
            id: PointerId(7),
            kind: PointerKind::Up,
            position: Point::new(150.0, 60.0),
        },
    )
    .unwrap();
    assert_eq!(background(&app, window, button), Color::WHITE);

    app.close(window).unwrap();
}

#[test]
fn captured_pointer_up_outside_clears_pressed_state_without_activation() {
    let mut app = App::new();
    let window = app
        .window(Window::new("Pointer capture").size(200.0, 80.0), |_| {
            ClickView { activations: 0 }
        })
        .unwrap();
    let root = app.root(window).unwrap();
    let column = app.child(window, root, 0_u64).unwrap().unwrap();
    let button = app.child(window, column, "clicker").unwrap().unwrap();

    let pointer = PointerId(7);
    assert_eq!(
        app.dispatch(
            window,
            InputEvent::Pointer {
                id: pointer,
                kind: PointerKind::Down,
                position: Point::new(10.0, 10.0),
            },
        )
        .unwrap(),
        Some(button)
    );
    assert_eq!(background(&app, window, button).r, 0.75);

    assert_eq!(
        app.dispatch(
            window,
            InputEvent::Pointer {
                id: pointer,
                kind: PointerKind::Up,
                position: Point::new(150.0, 60.0),
            },
        )
        .unwrap(),
        Some(button)
    );
    assert_eq!(background(&app, window, button), Color::WHITE);
    assert_eq!(
        app.semantics(window, button)
            .unwrap()
            .and_then(|semantics| semantics.name.as_deref()),
        Some("activations: 0")
    );
}
