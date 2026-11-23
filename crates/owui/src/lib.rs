//! Declarative views, persistent element behavior, and host-driven UI processing.
//!
//! A [`view::View`] keeps business state and dependencies in ordinary Rust fields
//! and builds [`element::Element`] declarations. Declaration builders preserve
//! their concrete type. On consumption, attributes are separated from
//! [`element::MountedElement`] behavior; compatible replacements retain its State.
//! Typed [`view::Action`] callbacks mutate their owning View and explicitly choose
//! whether to rebuild it.
//!
//! [`application::App`] owns logical windows, not native windows or a rendering
//! backend. The host supplies input, drives processing, and consumes published
//! [`scene::Scene`] fragments and semantics. No event loop, background processor,
//! or server is started. Scene primitives are local drawing intent. Consumers use
//! published geometry and clip data to attach them to world coordinates and supply
//! text shaping and rendering.
//!
//! Synchronous entry points such as [`application::App::activate`] settle work
//! before and after one input admission. Hosts that schedule their own work use
//! [`application::App::submit_input`] or [`application::App::submit_activation`]
//! followed by [`application::App::process`] with a [`schedule::ProcessBudget`].
//! A Pending window rejects dependent reads and independent input until it settles;
//! completed callbacks and State changes are not rolled back.
//!
//! Element callbacks, including nested child measurement, execute synchronously on
//! the caller's thread and use its available stack. The library does not resize that
//! stack, preempt callbacks, or promise arbitrary nesting depth.
//!
//! # Example
//!
//! A typed activation updates a View field, rebuilds the button declaration, and
//! publishes the new semantic name without creating a native window.
//!
//! ```
//! use owui::application::{App, AppError, Window};
//! use owui::element::{Button, Element, ElementExt};
//! use owui::input::Activate;
//! use owui::view::{BuildCx, Rebuild, View};
//!
//! struct Counter {
//!     clicks: usize,
//! }
//!
//! impl View for Counter {
//!     fn build(&self, cx: &BuildCx<'_, Self>) -> impl Element {
//!         Button::new(format!("Clicks: {}", self.clicks)).on(
//!             Activate,
//!             cx.action(|this, _, _: &()| {
//!                 this.clicks += 1;
//!                 Rebuild::This
//!             }),
//!         )
//!     }
//! }
//!
//! # fn main() -> Result<(), AppError> {
//! let mut app = App::new();
//! let window = app.window(Window::new("Counter").size(200.0, 60.0), |_| {
//!     Counter { clicks: 0 }
//! })?;
//! let button = app.child(window, app.root(window)?, 0_u64)?
//!     .expect("Counter builds one button at its root View boundary");
//!
//! app.activate(window, button)?;
//! assert_eq!(
//!     app.semantics(window, button)?.and_then(|content| content.name.as_deref()),
//!     Some("Clicks: 1"),
//! );
//!
//! app.close(window)?;
//! # Ok(())
//! # }
//! ```

mod id;

pub mod application;
pub mod color;
pub mod element;
pub mod input;
pub mod layout;
pub mod scene;
pub mod schedule;
pub mod view;
