use owui::application::{App, AppError, Window};
use owui::element::{Button, Element, ElementExt};
use owui::input::Activate;
use owui::schedule::{ProcessBudget, ProcessOutcome};
use owui::view::{BuildCx, Rebuild, View};

struct CounterView {
    activations: usize,
}

impl View for CounterView {
    fn build(&self, cx: &BuildCx<'_, Self>) -> impl Element {
        Button::new(format!("activations: {}", self.activations)).on(
            Activate,
            cx.action(|this, _, _: &()| {
                this.activations += 1;
                Rebuild::This
            }),
        )
    }
}

#[test]
fn pending_processing_gates_output_and_does_not_replay_completed_action() {
    let mut app = App::new();
    let window = app
        .window(Window::new("Processing").size(200.0, 60.0), |_| {
            CounterView { activations: 0 }
        })
        .unwrap();
    let root = app.root(window).unwrap();
    let button = app.child(window, root, 0_u64).unwrap().unwrap();

    app.submit_activation(window, button).unwrap();
    assert!(matches!(
        app.process(window, ProcessBudget::Steps(1)).unwrap(),
        ProcessOutcome::Pending { .. }
    ));
    assert!(matches!(
        app.semantics(window, button),
        Err(AppError::NotReady)
    ));
    assert!(matches!(app.scene(window), Err(AppError::NotReady)));

    assert!(matches!(
        app.process(window, ProcessBudget::Unlimited).unwrap(),
        ProcessOutcome::Settled { .. }
    ));
    assert_eq!(
        app.semantics(window, button)
            .unwrap()
            .and_then(|semantics| semantics.name.as_deref()),
        Some("activations: 1")
    );
}
