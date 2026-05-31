//! `gui` — Visual Desktop Compositor for TDE Phase 5.
//!
//! This module provides the floating-window layer that lives alongside the
//! existing tiling layout.  The two sub-modules are:
//!
//! - [`window`]     — `FloatingWindow` data type (id + screen rect).
//! - [`compositor`] — `draw_gui` render pass (wallpaper + window stack).
//!
//! Neither sub-module touches PTY state or the event loop.  They are purely
//! concerned with *what pixels appear on screen* given an `&AppState`.

pub mod window;
pub mod compositor;

// Re-export the most commonly used items so call-sites can write
// `crate::gui::FloatingWindow` instead of `crate::gui::window::FloatingWindow`.
pub use window::FloatingWindow;
pub use compositor::draw_gui;
