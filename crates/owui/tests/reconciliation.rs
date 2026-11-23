use owui::application::{App, Window, WindowId};
use owui::element::{Button, Column, Element, ElementExt, ElementId};
use owui::input::Activate;
use owui::view::{BuildCx, Rebuild, View};

struct CounterView {
    activations: usize,
}

impl View for CounterView {
    fn build(&self, cx: &BuildCx<'_, Self>) -> impl Element {
        Button::new(format!("child activations: {}", self.activations)).on(
            Activate,
            cx.action(|this, _, _: &()| {
                this.activations += 1;
                Rebuild::This
            }),
        )
    }
}

struct TreeView {
    reverse_children: bool,
    include_first: bool,
}

impl View for TreeView {
    fn build(&self, cx: &BuildCx<'_, Self>) -> impl Element {
        let reverse = Button::new("reverse").key("reverse").on(
            Activate,
            cx.action(|this, _, _: &()| {
                this.reverse_children = !this.reverse_children;
                Rebuild::This
            }),
        );
        let toggle_first = Button::new("toggle first").key("toggle-first").on(
            Activate,
            cx.action(|this, _, _: &()| {
                this.include_first = !this.include_first;
                Rebuild::This
            }),
        );
        let first = cx.view("first", |_| CounterView { activations: 0 });
        let second = cx.view("second", |_| CounterView { activations: 0 });
        let mut column = Column::new().child(reverse).child(toggle_first);

        if self.reverse_children {
            column = column.child(second);
            if self.include_first {
                column = column.child(first);
            }
        } else {
            if self.include_first {
                column = column.child(first);
            }
            column = column.child(second);
        }

        column
    }
}

fn tree() -> (App, WindowId, ElementId) {
    let mut app = App::new();
    let window = app
        .window(Window::new("Reconciliation").size(200.0, 100.0), |_| {
            TreeView {
                reverse_children: false,
                include_first: true,
            }
        })
        .unwrap();
    let root = app.root(window).unwrap();
    let column = app.child(window, root, 0_u64).unwrap().unwrap();

    (app, window, column)
}

#[test]
fn keyed_child_keeps_state_and_identity_when_siblings_reorder() {
    let (mut app, window, column) = tree();
    let first = app.child(window, column, "first").unwrap().unwrap();
    let first_button = app.child(window, first, 0_u64).unwrap().unwrap();

    app.activate(window, first_button).unwrap();
    assert_eq!(
        app.semantics(window, first_button)
            .unwrap()
            .and_then(|semantics| semantics.name.as_deref()),
        Some("child activations: 1")
    );

    let reverse = app.child(window, column, "reverse").unwrap().unwrap();
    app.activate(window, reverse).unwrap();

    assert_eq!(app.child(window, column, "first").unwrap(), Some(first));
    assert_eq!(app.child(window, first, 0_u64).unwrap(), Some(first_button));
    assert_eq!(
        app.semantics(window, first_button)
            .unwrap()
            .and_then(|semantics| semantics.name.as_deref()),
        Some("child activations: 1")
    );
}

#[test]
fn retired_child_identity_does_not_alias_after_reinsertion() {
    let (mut app, window, column) = tree();
    let old_first = app.child(window, column, "first").unwrap().unwrap();
    let old_button = app.child(window, old_first, 0_u64).unwrap().unwrap();
    let toggle = app.child(window, column, "toggle-first").unwrap().unwrap();

    app.activate(window, toggle).unwrap();
    assert_eq!(app.child(window, column, "first").unwrap(), None);
    assert_eq!(app.geometry(window, old_first).unwrap(), None);
    assert_eq!(app.semantics(window, old_button).unwrap(), None);

    app.activate(window, toggle).unwrap();
    let new_first = app.child(window, column, "first").unwrap().unwrap();
    let new_button = app.child(window, new_first, 0_u64).unwrap().unwrap();

    assert_ne!(new_first, old_first);
    assert_ne!(new_button, old_button);
    assert_eq!(app.semantics(window, old_button).unwrap(), None);
}
