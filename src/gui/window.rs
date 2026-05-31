//! `gui/window.rs` — The `FloatingWindow` data type.
//!
//! A `FloatingWindow` is a lightweight record that associates a pane (by id)
//! with an arbitrary on-screen `Rect`.  The rest of the window's appearance —
//! its title, border colour, and content — are derived from the pane data
//! stored in `AppState::panes`.
//!
//! ## Ownership model
//!
//! `FloatingWindow` owns nothing heavy.  It is cheaply `Clone`-able and lives
//! in `AppState::floating_windows: Vec<FloatingWindow>`.  The `Vec` is ordered
//! back-to-front: index 0 is the bottommost window, the last element is the
//! topmost (drawn last, hit-tested first for future mouse-drag support).

use crate::layout::PaneId;
use ratatui::layout::Rect;

// ─── FloatingWindow ──────────────────────────────────────────────────────────

/// One entry in the floating window stack.
///
/// Fields are `pub` because the compositor, input handler, and `AppState`
/// methods all need direct read/write access.
#[derive(Clone, Debug)]
pub struct FloatingWindow {
    /// The pane whose content is rendered inside this window.
    pub id: PaneId,
    /// The window's position and size in absolute terminal coordinates.
    /// Includes the one-cell border on every side, so the usable inner area
    /// is `area` shrunk by 1 on each edge (use `Block::inner` to compute it).
    pub area: Rect,
}

impl FloatingWindow {
    /// Construct a `FloatingWindow` directly from its components.
    pub fn new(id: PaneId, area: Rect) -> Self {
        Self { id, area }
    }
}

// ─── Cascade geometry ────────────────────────────────────────────────────────

/// Number of terminal cells each successive window is offset by (x and y).
///
/// A value of 2 gives a clear visual separation without wasting too much
/// screen space on a typical 80×24 or 220×50 terminal.
pub const CASCADE_STEP: u16 = 2;

/// Width of each floating window expressed as a percentage of `screen_width`.
pub const WIN_WIDTH_PCT: u16 = 60;

/// Height of each floating window expressed as a percentage of `screen_height`.
pub const WIN_HEIGHT_PCT: u16 = 60;

/// Compute the `Rect` for the `n`-th window in a cascade (0-indexed).
///
/// The first window (`n == 0`) is centred on `screen`.  Each subsequent
/// window is shifted right and down by [`CASCADE_STEP`] cells.  All windows
/// are clamped so they never extend beyond the right or bottom edge of
/// `screen`, which prevents rendering panics on small terminals.
///
/// # Arguments
///
/// * `n`      — zero-based window index in the cascade.
/// * `screen` — the full available area (typically the content area, i.e.
///              the full terminal minus the top/bottom chrome bars).
pub fn cascade_rect(n: u16, screen: Rect) -> Rect {
    // Compute base dimensions (percentage of screen).
    let win_w = (screen.width  * WIN_WIDTH_PCT  / 100).max(20);
    let win_h = (screen.height * WIN_HEIGHT_PCT / 100).max(6);

    // Centre of the screen.
    let centre_x = screen.x + screen.width  / 2;
    let centre_y = screen.y + screen.height / 2;

    // Top-left of a centred window.
    let base_x = centre_x.saturating_sub(win_w / 2);
    let base_y = centre_y.saturating_sub(win_h / 2);

    // Apply per-window offset.
    let offset = CASCADE_STEP * n;
    let x = base_x.saturating_add(offset);
    let y = base_y.saturating_add(offset);

    // Clamp so the window never overflows the screen boundary.
    let max_x = screen.x + screen.width.saturating_sub(win_w);
    let max_y = screen.y + screen.height.saturating_sub(win_h);
    let x = x.min(max_x);
    let y = y.min(max_y);

    Rect { x, y, width: win_w, height: win_h }
}
