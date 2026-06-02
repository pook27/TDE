//! `app.rs` — Core application state and event loop for TDE.
//!
//! This module owns:
//!   - `AppEvent`          — the inter-task message type
//!   - `AppPane`           — discriminated union over terminal / explorer panes
//!   - `OverlayAction` / `AppOverlay` — transient command-input overlay
//!   - `DesktopMode`       — Tiling (classic) vs Gui (floating compositor)
//!   - `AppState`          — the entire mutable world: layout tree, pane map,
//!                           focus, desktop mode, floating window list
//!   - `run_event_loop`    — the async select! loop that drives everything
//!   - `input_task`        — the crossterm `EventStream` producer task
//!   - `draw`              — ratatui rendering pass (branches on `DesktopMode`)
//!   - `mod theme`         — palette constants

use std::{
    collections::HashMap,
    io::{self, Read, Write},
    path::PathBuf,
    time::Duration,
};

use crate::layout::{
    centroid_x, centroid_y,
    ranges_overlap_h, ranges_overlap_v,
    Dir, LayoutNode, PaneId, SplitKind,
};
use crate::vfs::{ExplorerEntry, ExplorerPane, spawn_dir_read};
use crate::pty::{TerminalPane, spawn_pane_reader};
use crate::input::dispatch_input;
use crate::gui::{compositor::draw_gui, window::{FloatingWindow, cascade_rect}};
use crate::dlog;

use anyhow::Result;
use crossterm::event::{Event, EventStream, MouseButton, MouseEventKind};
use futures::StreamExt;
use portable_pty::CommandBuilder;
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph},
    Terminal,
};
use tokio::sync::mpsc;
use tui_term::widget::PseudoTerminal;

// ═══════════════════════════════════════════════════════════════════════════════
// § 2  Events
// ═══════════════════════════════════════════════════════════════════════════════

pub enum AppEvent {
    PtyOutput { pane_id: PaneId, bytes: Vec<u8> },
    PtyExited { pane_id: PaneId },
    Input(Event),
    ExplorerUpdate { pane_id: PaneId, path: PathBuf, entries: Vec<ExplorerEntry> },
}

// ═══════════════════════════════════════════════════════════════════════════════
// § 3  Desktop mode
// ═══════════════════════════════════════════════════════════════════════════════

/// Controls which rendering pass `draw()` uses.
///
/// `Alt+G` toggles between the two modes at runtime.  The underlying pane
/// data (`AppState::panes`, `AppState::layout`) is shared between modes, so
/// switching is instantaneous — no re-spawning or state transfer needed.
///
/// `PartialEq` is derived so that `draw()` can branch with `==`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DesktopMode {
    /// Classic tiling layout driven by `LayoutNode::walk_rects`.
    Tiling,
    /// Floating compositor driven by `gui::compositor::draw_gui`.
    Gui,
}

// ═══════════════════════════════════════════════════════════════════════════════
// § 6.5  Generic pane discriminant
// ═══════════════════════════════════════════════════════════════════════════════

pub enum AppPane {
    Terminal(TerminalPane),
    Explorer(ExplorerPane),
}

// ═══════════════════════════════════════════════════════════════════════════════
// § 8  AppState
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Clone)]
pub enum OverlayAction {
    /// Alt+Space to spawn a custom command
    SpawnCommand,
    /// 'c' in Explorer to create a file/directory
    CreateFile { cwd: PathBuf },
}

pub struct AppOverlay {
    pub action: OverlayAction,
    pub input:  String,
}

pub struct AppState {
    pub layout:           LayoutNode,
    pub panes:            HashMap<PaneId, AppPane>,
    pub focus:            PaneId,
    pub next_id:          PaneId,
    pub overlay:          Option<AppOverlay>,
    // ── Phase 5 additions ──────────────────────────────────────────────────
    /// Current rendering mode: tiling grid or floating compositor.
    pub mode:             DesktopMode,
    /// The ordered floating-window stack (back → front / bottom → top).
    ///
    /// Populated on first `Alt+G` switch to `DesktopMode::Gui` if empty.
    /// Kept in sync with `AppState::panes`: entries are removed in
    /// `close_pane` when their backing pane is destroyed.
    pub floating_windows: Vec<FloatingWindow>,
    /// Active drag state: `Some((pane_id, last_mouse_x, last_mouse_y))`.
    ///
    /// Set on `MouseDown::Left` when a GUI window is clicked, cleared on
    /// `MouseUp::Left`.  During a `MouseDrag::Left` event the delta between
    /// the stored last position and the new position is applied to the
    /// window's `area` origin.
    pub drag_state:       Option<(PaneId, u16, u16)>,
}

impl AppState {
    /// Single-pane startup: one terminal filling the entire content area.
    pub fn new(area: Rect) -> Result<(Self, Box<dyn Read + Send>)> {
        let id: PaneId = 0;
        let rows = area.height.saturating_sub(2).max(2);
        let cols = area.width.saturating_sub(2).max(8);

        let (pane, reader) = TerminalPane::new(id, rows, cols, None)?;
        let mut panes = HashMap::new();
        panes.insert(id, AppPane::Terminal(pane));

        Ok((
            Self {
                layout:           LayoutNode::Pane(id),
                panes,
                focus:            id,
                next_id:          1,
                overlay:          None,
                mode:             DesktopMode::Tiling,
                floating_windows: Vec::new(),
                drag_state:       None,
            },
            reader,
        ))
    }

    // ── Focus movement ───────────────────────────────────────────────────────

    pub fn move_focus(&mut self, area: Rect, dir: Dir) {
        // First pass: find the focused pane's rect — no allocation.
        let focus_id = self.focus;
        let mut cur_rect: Option<Rect> = None;
        self.layout.walk_rects(area, &mut |id, rect| {
            if id == focus_id { cur_rect = Some(rect); }
        });
        let cur = match cur_rect {
            Some(r) => r,
            None    => return,
        };
        let cx = centroid_x(cur);
        let cy = centroid_y(cur);

        // Second pass: find the best directional neighbour — still no allocation.
        let mut best: Option<(PaneId, i32)> = None;
        self.layout.walk_rects(area, &mut |id, rect| {
            if id == focus_id { return; }
            let rx = centroid_x(rect);
            let ry = centroid_y(rect);
            let ok = match dir {
                Dir::Left  => rx < cx && ranges_overlap_v(cur, rect),
                Dir::Right => rx > cx && ranges_overlap_v(cur, rect),
                Dir::Up    => ry < cy && ranges_overlap_h(cur, rect),
                Dir::Down  => ry > cy && ranges_overlap_h(cur, rect),
            };
            if !ok { return; }
            let dist = match dir {
                Dir::Left | Dir::Right => (rx - cx).abs(),
                Dir::Up   | Dir::Down  => (ry - cy).abs(),
            };
            match best {
                None                       => best = Some((id, dist)),
                Some((_, bd)) if dist < bd => best = Some((id, dist)),
                _                          => {}
            }
        });

        if let Some((new_focus, _)) = best {
            self.focus = new_focus;
        }
    }

    // ── Smart split heuristic ────────────────────────────────────────────────

    /// Determines the best split direction based on the focused pane's current
    /// dimensions.
    pub fn smart_split_kind(&self, area: Rect, preferred: SplitKind) -> SplitKind {
        let focus_id = self.focus;
        let mut focus_rect = area;
        self.layout.walk_rects(area, &mut |id, rect| {
            if id == focus_id { focus_rect = rect; }
        });

        // Terminal fonts are usually ~2× as tall as they are wide.
        // 45 cols is very narrow; 12 rows is very short.
        match preferred {
            SplitKind::Vertical => {
                if focus_rect.width < 45 && focus_rect.height >= 12 {
                    SplitKind::Horizontal
                } else {
                    SplitKind::Vertical
                }
            }
            SplitKind::Horizontal => {
                if focus_rect.height < 12 && focus_rect.width >= 45 {
                    SplitKind::Vertical
                } else {
                    SplitKind::Horizontal
                }
            }
        }
    }

    // ── Dynamic split ────────────────────────────────────────────────────────

    /// Split the focused pane and return the new pane's reader for thread
    /// spawning.  `area` is used to compute correct initial PTY dimensions.
    pub fn do_split(
        &mut self,
        area: Rect,
        kind: SplitKind,
        cmd:  Option<CommandBuilder>,
    ) -> Result<Option<(PaneId, Box<dyn Read + Send>)>> {
        let focus_id = self.focus;
        let mut focus_rect = area;
        self.layout.walk_rects(area, &mut |id, rect| {
            if id == focus_id { focus_rect = rect; }
        });

        match kind {
            SplitKind::Vertical if focus_rect.width < 12 => return Ok(None),
            SplitKind::Horizontal if focus_rect.height < 6 => return Ok(None),
            _ => {}
        }

        let new_id = self.next_id;
        self.next_id += 1;

        let (rows, cols) = self.new_pane_size(area, kind);

        let (pane, reader) = TerminalPane::new(new_id, rows, cols, cmd)?;
        self.panes.insert(new_id, AppPane::Terminal(pane));

        self.layout.split_pane(self.focus, new_id, kind);
        self.focus = new_id;

        // If we are in GUI mode, spawn a floating window for the new pane immediately
        if self.mode == DesktopMode::Gui {
            let n = self.floating_windows.len() as u16;
            let rect = cascade_rect(n, area);
            self.floating_windows.push(FloatingWindow::new(new_id, rect));
        }

        // Force dimensions to sync regardless of mode
        let _ = self.resize_all(area);

        Ok(Some((new_id, reader)))
    }

    pub fn do_split_explorer(
        &mut self,
        area: Rect,
        kind: SplitKind,
        tx:   mpsc::Sender<AppEvent>,
    ) -> Result<()> {
        let focus_id = self.focus;
        let mut focus_rect = area;
        self.layout.walk_rects(area, &mut |id, rect| {
            if id == focus_id { focus_rect = rect; }
        });

        match kind {
            SplitKind::Vertical if focus_rect.width < 12 => return Ok(()),
            SplitKind::Horizontal if focus_rect.height < 6 => return Ok(()),
            _ => {}
        }

        let new_id = self.next_id;
        self.next_id += 1;

        let home = std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/"));
        let explorer = ExplorerPane::new(new_id, home.clone());

        self.panes.insert(new_id, AppPane::Explorer(explorer));
        self.layout.split_pane(self.focus, new_id, kind);
        self.focus = new_id;

        if self.mode == DesktopMode::Gui {
            let n = self.floating_windows.len() as u16;
            let rect = cascade_rect(n, area);
            self.floating_windows.push(FloatingWindow::new(new_id, rect));
        }

        let _ = self.resize_all(area);

        spawn_dir_read(new_id, home, tx);

        Ok(())
    }

    /// Compute the PTY size that a newly created pane will have after splitting
    /// the focused pane.
    pub fn new_pane_size(&self, area: Rect, kind: SplitKind) -> (u16, u16) {
        let focus_id = self.focus;
        let mut focus_rect = area;
        self.layout.walk_rects(area, &mut |id, rect| {
            if id == focus_id { focus_rect = rect; }
        });
        match kind {
            SplitKind::Vertical => {
                let cols = (focus_rect.width / 2).saturating_sub(2).max(8);
                let rows = focus_rect.height.saturating_sub(2).max(2);
                (rows, cols)
            }
            SplitKind::Horizontal => {
                let rows = (focus_rect.height / 2).saturating_sub(2).max(2);
                let cols = focus_rect.width.saturating_sub(2).max(8);
                (rows, cols)
            }
        }
    }

    // ── Pane close ───────────────────────────────────────────────────────────

    /// Remove `target` from both the layout tree and the pane HashMap.
    /// Drops `TerminalPane`, which drops `_child` → shell receives SIGHUP.
    ///
    /// Also removes any `FloatingWindow` whose id matches `target` so the
    /// GUI compositor never renders a window with a dangling pane reference.
    ///
    /// Returns `true` if the *last* pane was just closed (caller should quit).
    pub fn close_pane(&mut self, target: PaneId, area: Rect) -> Result<bool> {
        // Guard: ignore stale PtyExited events for already-removed panes.
        if !self.panes.contains_key(&target) {
            return Ok(false);
        }

        // ── Special case: only one pane left ──────────────────────────────
        if self.panes.len() == 1 {
            self.panes.remove(&target);
            self.floating_windows.retain(|w| w.id != target);
            return Ok(true);
        }

        // ── Choose the pane that will receive focus after removal ─────────
        let all_ids = self.layout.all_pane_ids();
        let target_pos = all_ids.iter().position(|id| *id == target).unwrap_or(0);
        let survivors: Vec<PaneId> = all_ids.iter()
            .copied()
            .filter(|id| *id != target)
            .collect();

        let positional_focus = if target_pos > 0 {
            all_ids[target_pos - 1]
        } else {
            survivors[0]
        };

        // Upgrade: if the positional candidate is an Explorer, try to find
        // any Terminal pane among survivors instead.
        let new_focus = if matches!(self.panes.get(&positional_focus), Some(AppPane::Explorer(_))) {
            survivors.iter()
                .find(|id| matches!(self.panes.get(id), Some(AppPane::Terminal(_))))
                .copied()
                .unwrap_or(positional_focus)
        } else {
            positional_focus
        };
        dlog(&format!("close_pane: target={target} positional_focus={positional_focus} new_focus={new_focus}"));

        // ── Prune the layout tree ──────────────────────────────────────────
        self.layout.prune_pane(target);

        // ── Remove from HashMap (drops TerminalPane → drops child) ────────
        self.panes.remove(&target);

        // ── Remove any floating window for the closed pane ────────────────
        //
        // This keeps `floating_windows` in sync with `panes` so the GUI
        // compositor never iterates a window that has no backing pane.
        self.floating_windows.retain(|w| w.id != target);

        // ── Update focus ──────────────────────────────────────────────────
        self.focus = new_focus;

        // ── Resize survivors to fit their new (expanded) rects ────────────
        self.resize_all(area)?;

        Ok(false)
    }

    // ── Resize all panes ─────────────────────────────────────────────────────

    pub fn resize_all(&mut self, area: Rect) -> Result<()> {
        let mut to_resize: Vec<(PaneId, u16, u16)> = Vec::new();

        match self.mode {
            DesktopMode::Tiling => {
                self.layout.walk_rects(area, &mut |id, rect| {
                    let rows = rect.height.saturating_sub(2).max(2);
                    let cols = rect.width.saturating_sub(2).max(8);
                    to_resize.push((id, rows, cols));
                });
            }
            DesktopMode::Gui => {
                for win in &self.floating_windows {
                    let rows = win.area.height.saturating_sub(2).max(2);
                    let cols = win.area.width.saturating_sub(2).max(8);
                    to_resize.push((win.id, rows, cols));
                }
            }
        }

        for (id, rows, cols) in to_resize {
            if let Some(AppPane::Terminal(term)) = self.panes.get_mut(&id) {
                term.resize(rows, cols)?;
            }
        }
        Ok(())
    }

    // ── Spatial mouse hit-test ────────────────────────────────────────────────

    /// Walk every pane rect and focus the one whose bounding box contains
    /// `(x, y)`.  In `DesktopMode::Gui` the floating window list is
    /// hit-tested top-to-bottom (last entry first) instead.
    ///
    /// Returns `true` when focus actually changed (so the caller knows whether
    /// a redraw is needed).
    pub fn click_focus(&mut self, area: Rect, x: u16, y: u16) -> bool {
        match self.mode {
            DesktopMode::Tiling => {
                let mut hit: Option<PaneId> = None;
                self.layout.walk_rects(area, &mut |id, rect| {
                    if x >= rect.x && x < rect.x + rect.width
                        && y >= rect.y && y < rect.y + rect.height
                    {
                        hit = Some(id);
                    }
                });

                if let Some(id) = hit {
                    if id != self.focus {
                        dlog(&format!("click_focus/tiling: ({x},{y}) → pane {id}"));
                        self.focus = id;
                        return true;
                    }
                }
                false
            }

            DesktopMode::Gui => {
                // Hit-test the window stack top-to-bottom (reverse iteration).
                let rev_idx = self.floating_windows.iter().rev().position(|w| {
                    x >= w.area.x
                        && x < w.area.x + w.area.width
                        && y >= w.area.y
                        && y < w.area.y + w.area.height
                });

                if let Some(rev_idx) = rev_idx {
                    let idx = self.floating_windows.len() - 1 - rev_idx;
                    let id  = self.floating_windows[idx].id;

                    let focus_changed = id != self.focus;
                    let needs_raise   = idx != self.floating_windows.len() - 1;

                    if focus_changed {
                        dlog(&format!("click_focus/gui: ({x},{y}) → pane {id}"));
                        self.focus = id;
                    }

                    if needs_raise {
                        let win = self.floating_windows.remove(idx);
                        self.floating_windows.push(win);
                        dlog(&format!("click_focus/gui: raised pane {id} to top (was idx {idx})"));
                    }

                    return focus_changed || needs_raise;
                }
                false
            }
        }
    }

    // ── Taskbar hit-test ─────────────────────────────────────────────────────

    /// Handle a click on the bottom taskbar row (only called when
    /// `m.row >= area.y + area.height`, i.e. the bottom chrome bar).
    ///
    /// Fix 2: badges are iterated in stable ascending `PaneId` order — matching
    /// the render — so the physical column of each badge never shifts when
    /// windows are raised.
    ///
    /// Layout mirrors the render exactly (see `draw` § "Taskbar"):
    ///
    /// ```text
    /// [ TDE Start ]  [0: bash]  [1: nvim]  …
    /// 0            12 15       X  …
    /// ```
    ///
    /// Returns `true` if state changed and a redraw is needed.
    pub fn click_taskbar(&mut self, x: u16) -> bool {
        const START_BTN_WIDTH: u16 = 13; // "[ TDE Start ]"
        const SEP_WIDTH:       u16 = 2;  // "  " between items

        if x < START_BTN_WIDTH {
            self.overlay = Some(AppOverlay {
                action: OverlayAction::SpawnCommand,
                input:  String::new(),
            });
            return true;
        }

        // Fix 2: iterate in sorted PaneId order, matching the stable render order.
        let mut sorted_ids: Vec<PaneId> = self.floating_windows.iter().map(|w| w.id).collect();
        sorted_ids.sort_unstable();

        let mut cursor: u16 = START_BTN_WIDTH + SEP_WIDTH;

        for id in sorted_ids {
            // Fix 3: use the dynamic process name for badge width computation.
            let label = self.taskbar_label_for(id);
            // "[" + label + "]"
            let badge_width: u16 = 2 + label.len() as u16;
            let badge_end = cursor + badge_width;

            if x >= cursor && x < badge_end {
                let focus_changed = id != self.focus;
                // Find stack index for raise logic.
                let stack_idx = self.floating_windows.iter().position(|w| w.id == id);
                let needs_raise = stack_idx
                    .map(|i| i != self.floating_windows.len() - 1)
                    .unwrap_or(false);

                if focus_changed {
                    dlog(&format!("click_taskbar: x={x} → pane {id}"));
                    self.focus = id;
                }
                if needs_raise {
                    if let Some(i) = stack_idx {
                        let w = self.floating_windows.remove(i);
                        self.floating_windows.push(w);
                        dlog(&format!("click_taskbar: raised pane {id} to top (was idx {i})"));
                    }
                }
                return focus_changed || needs_raise;
            }

            cursor = badge_end + SEP_WIDTH;
        }

        false
    }

    /// Build the inner text of a taskbar badge for `id` without allocating on
    /// the common (shell-idle) path.  Returns `"{id}: {process_name}"`.
    ///
    /// Fix 3: for Terminal panes calls `foreground_process_name()` so the badge
    /// reflects the currently-running foreground command.
    #[inline]
    fn taskbar_label_for(&self, id: PaneId) -> String {
        match self.panes.get(&id) {
            Some(AppPane::Terminal(term)) => {
                let proc_name = term.foreground_process_name();
                format!("{id}: {proc_name}")
            }
            Some(AppPane::Explorer(_)) => format!("{id}: Explorer"),
            None                       => format!("{id}: ?"),
        }
    }

    // ── GUI mode toggle ───────────────────────────────────────────────────────

    /// Toggle between `DesktopMode::Tiling` and `DesktopMode::Gui`.
    pub fn toggle_gui_mode(&mut self, content_area: Rect) {
        match self.mode {
            DesktopMode::Tiling => {
                self.mode = DesktopMode::Gui;

                let ids = self.layout.all_pane_ids();
                let existing_count = self.floating_windows.len() as u16;
                let mut added = 0;

                for id in ids {
                    let already_exists = self.floating_windows.iter().any(|w| w.id == id);
                    if !already_exists {
                        let rect = cascade_rect(existing_count + added, content_area);
                        self.floating_windows.push(FloatingWindow::new(id, rect));
                        added += 1;
                        dlog(&format!("toggle_gui: added new window for pane {id}"));
                    }
                }

                let _ = self.resize_all(content_area);
            }

            DesktopMode::Gui => {
                self.mode = DesktopMode::Tiling;
                let _ = self.resize_all(content_area);
            }
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// § 12  Background I/O tasks
// ═══════════════════════════════════════════════════════════════════════════════

pub async fn input_task(tx: mpsc::Sender<AppEvent>) {
    let mut iteration = 0u32;
    loop {
        iteration += 1;
        dlog(&format!("input_task: creating EventStream (iteration {iteration})"));
        let mut stream = EventStream::new();
        loop {
            match stream.next().await {
                Some(Ok(ev)) => {
                    if tx.send(AppEvent::Input(ev)).await.is_err() {
                        dlog("input_task: channel closed, exiting");
                        return;
                    }
                }
                Some(Err(e)) => {
                    dlog(&format!("input_task: EventStream error: {e} — recreating stream"));
                    break;
                }
                None => {
                    dlog("input_task: EventStream returned None — recreating stream");
                    break;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        let raw_result = crossterm::terminal::enable_raw_mode();
        dlog(&format!("input_task: enable_raw_mode after stream error: {raw_result:?}"));
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// § 15  Rendering
// ═══════════════════════════════════════════════════════════════════════════════

pub mod theme {
    use ratatui::style::Color;
    pub const ACCENT:         Color = Color::Cyan;
    pub const DIM_BORDER:     Color = Color::DarkGray;
    pub const TITLE_BADGE_FG: Color = Color::Black;
    pub const TITLE_BADGE_BG: Color = Color::Cyan;
    pub const KEY_HINT:       Color = Color::Yellow;
    pub const DIM_TEXT:       Color = Color::DarkGray;
    /// Fix 2: focused taskbar badge highlight — dark blue, clearly distinct.
    pub const TASKBAR_FOCUS_FG: Color = Color::White;
    pub const TASKBAR_FOCUS_BG: Color = Color::Blue;
    /// Fix 4: dead-pane border colour.
    pub const DEAD_BORDER:    Color = Color::Red;
}

// ── Fix 4: overlay bounding-box helper ───────────────────────────────────────
//
// Returns the exact `Rect` the overlay occupies so that the mouse-down handler
// can compare click coordinates against it without re-running the layout pass.
// The calculation must stay 100 % in sync with `draw()`'s overlay block:
//
//   let mut overlay_area = centered_rect(40, 20, full);
//   overlay_area.height  = 3;
//
// `centered_rect(pct_x, pct_y, area)` computes:
//   w = area.width  * pct_x / 100
//   h = area.height * pct_y / 100
//   x = area.x + (area.width  - w) / 2
//   y = area.y + (area.height - h) / 2
//
// We then override height to 3 (matching the draw call).
//
// All arithmetic uses saturating operations to prevent wrapping on extreme
// terminal sizes.
#[inline]
fn overlay_rect(full: Rect) -> Rect {
    let w = (full.width as u32 * 40 / 100).min(full.width as u32) as u16;
    let h = 3u16; // overridden from centered_rect's pct_y result
    let x = full.x + full.width.saturating_sub(w) / 2;
    let y = full.y + full.height.saturating_sub(h) / 2;
    Rect { x, y, width: w, height: h }
}

pub fn draw(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    state:    &AppState,
) -> Result<()> {
    terminal.draw(|frame| {
        let full = frame.area();

        // ── Outer chrome ──────────────────────────────────────────────────
        let outer = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(0),
                Constraint::Length(1),
            ])
            .split(full);

        let (top_area, content_area, bot_area) = (outer[0], outer[1], outer[2]);

        // ── Top bar ───────────────────────────────────────────────────────
        let mode_label = match state.mode {
            DesktopMode::Tiling => " TILING ",
            DesktopMode::Gui    => " GUI ",
        };
        let mode_style = match state.mode {
            DesktopMode::Tiling => Style::default().fg(theme::TITLE_BADGE_FG).bg(theme::TITLE_BADGE_BG),
            DesktopMode::Gui    => Style::default().fg(Color::Black).bg(Color::Magenta),
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    " TDE ",
                    Style::default()
                        .fg(theme::TITLE_BADGE_FG)
                        .bg(theme::TITLE_BADGE_BG)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    "  Terminal Desktop Environment  ",
                    Style::default().fg(theme::ACCENT),
                ),
                Span::styled(
                    mode_label,
                    mode_style.add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("  [{} pane(s)]", state.panes.len()),
                    Style::default().fg(theme::DIM_TEXT),
                ),
            ])),
            top_area,
        );

        // ── Bottom bar: key hints (Tiling) or taskbar (Gui) ──────────────
        match state.mode {
            DesktopMode::Tiling => {
                let is_explorer = matches!(
                    state.panes.get(&state.focus),
                    Some(AppPane::Explorer(_))
                );

                let mut hints: Vec<Span> = Vec::new();

                if is_explorer {
                    hints.extend([
                        Span::styled(" c ", Style::default().fg(theme::KEY_HINT).add_modifier(Modifier::BOLD)),
                        Span::styled("create │", Style::default().fg(theme::DIM_TEXT)),
                        Span::styled(" D ", Style::default().fg(theme::KEY_HINT).add_modifier(Modifier::BOLD)),
                        Span::styled("delete │", Style::default().fg(theme::DIM_TEXT)),
                        Span::styled(" Enter ", Style::default().fg(theme::KEY_HINT).add_modifier(Modifier::BOLD)),
                        Span::styled("open │", Style::default().fg(theme::DIM_TEXT)),
                    ]);
                }

                hints.extend([
                    Span::styled(" Alt+Space ", Style::default().fg(theme::KEY_HINT).add_modifier(Modifier::BOLD)),
                    Span::styled("cmd │", Style::default().fg(theme::DIM_TEXT)),
                    Span::styled(" Alt+E ", Style::default().fg(theme::KEY_HINT).add_modifier(Modifier::BOLD)),
                    Span::styled("exp │", Style::default().fg(theme::DIM_TEXT)),
                    Span::styled(" Alt+V/S ", Style::default().fg(theme::KEY_HINT).add_modifier(Modifier::BOLD)),
                    Span::styled("split │", Style::default().fg(theme::DIM_TEXT)),
                    Span::styled(" Alt+X ", Style::default().fg(theme::KEY_HINT).add_modifier(Modifier::BOLD)),
                    Span::styled("close │", Style::default().fg(theme::DIM_TEXT)),
                    Span::styled(" Alt+Arrows ", Style::default().fg(theme::KEY_HINT).add_modifier(Modifier::BOLD)),
                    Span::styled("focus │", Style::default().fg(theme::DIM_TEXT)),
                    Span::styled(" Alt+G ", Style::default().fg(theme::KEY_HINT).add_modifier(Modifier::BOLD)),
                    Span::styled("gui │", Style::default().fg(theme::DIM_TEXT)),
                    Span::styled(" Alt+Q ", Style::default().fg(theme::KEY_HINT).add_modifier(Modifier::BOLD)),
                    Span::styled("quit", Style::default().fg(theme::DIM_TEXT)),
                ]);

                frame.render_widget(Paragraph::new(Line::from(hints)), bot_area);
            }

            DesktopMode::Gui => {
                // ── Taskbar ───────────────────────────────────────────────
                //
                // Fix 2: badges are rendered in stable ascending PaneId order.
                // The focused badge is highlighted blue+bold; its position in
                // the bar never changes regardless of which window is "on top"
                // in the compositor stack.
                //
                // Fix 3: each badge shows the live foreground process name
                // instead of the static string "Terminal".
                //
                // Capacity: start-button + (sep + badge) per window.
                let n_windows = state.floating_windows.len();
                let mut taskbar: Vec<Span> = Vec::with_capacity(1 + n_windows * 2);

                // Start button.
                taskbar.push(Span::styled(
                    "[ TDE Start ]",
                    Style::default()
                        .fg(theme::TITLE_BADGE_FG)
                        .bg(theme::ACCENT)
                        .add_modifier(Modifier::BOLD),
                ));

                // Collect pane ids in stable ascending order (Fix 2).
                let mut sorted_ids: Vec<PaneId> =
                    state.floating_windows.iter().map(|w| w.id).collect();
                sorted_ids.sort_unstable();

                for id in sorted_ids {
                    // Two-space separator before every badge.
                    taskbar.push(Span::raw("  "));

                    let is_focused = id == state.focus;

                    // Fix 2: focused badge = blue background + bold white text.
                    //        Unfocused badge = dim gray as before.
                    let badge_style = if is_focused {
                        Style::default()
                            .fg(theme::TASKBAR_FOCUS_FG)
                            .bg(theme::TASKBAR_FOCUS_BG)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(theme::DIM_TEXT)
                    };

                    // Fix 3: dynamic process name.
                    let label = match state.panes.get(&id) {
                        Some(AppPane::Terminal(term)) => {
                            format!("[{}: {}]", id, term.foreground_process_name())
                        }
                        Some(AppPane::Explorer(_)) => format!("[{id}: Explorer]"),
                        None                       => format!("[{id}: ?]"),
                    };

                    taskbar.push(Span::styled(label, badge_style));
                }

                frame.render_widget(Paragraph::new(Line::from(taskbar)), bot_area);
            }
        }

        // ── Content area: branch on desktop mode ──────────────────────────
        match state.mode {
            // ── Tiling: existing zero-allocation walk_rects loop ──────────
            DesktopMode::Tiling => {
                let focus_id = state.focus;
                state.layout.walk_rects(content_area, &mut |id, rect| {
                    if rect.width < 2 || rect.height < 2 { return; }

                    let Some(pane) = state.panes.get(&id) else { return };
                    let focused = id == focus_id;

                    match pane {
                        // ── Fix 4: dead custom-command pane ───────────────
                        AppPane::Terminal(term) if term.is_dead => {
                            // Red border to signal the process has exited.
                            let block = Block::default()
                                .borders(Borders::ALL)
                                .border_style(
                                    Style::default()
                                        .fg(theme::DEAD_BORDER)
                                        .add_modifier(Modifier::BOLD),
                                )
                                .title(Span::styled(
                                    format!(" [{}] ✖ Process Completed — Alt+X to close ", id),
                                    Style::default()
                                        .fg(theme::DEAD_BORDER)
                                        .add_modifier(Modifier::BOLD),
                                ));

                            let inner = block.inner(rect);
                            frame.render_widget(block, rect);

                            // Render the last-known screen state so the output
                            // is visible for the user to read.
                            if inner.width > 0 && inner.height > 0 {
                                let guard = term.parser.lock().expect("parser poisoned");
                                frame.render_widget(PseudoTerminal::new(guard.screen()), inner);
                            }
                        }

                        // ── Live terminal pane ────────────────────────────
                        AppPane::Terminal(term) => {
                            let border_style = if focused {
                                Style::default().fg(theme::ACCENT).add_modifier(Modifier::BOLD)
                            } else {
                                Style::default().fg(theme::DIM_BORDER)
                            };

                            let title_str = if focused {
                                format!(" [{}] ● ", id)
                            } else {
                                format!(" [{}] ", id)
                            };

                            let title_style = if focused {
                                Style::default().fg(theme::ACCENT).add_modifier(Modifier::BOLD)
                            } else {
                                Style::default().fg(theme::DIM_TEXT)
                            };

                            let block = Block::default()
                                .borders(Borders::ALL)
                                .border_style(border_style)
                                .title(Span::styled(title_str, title_style));

                            let inner = block.inner(rect);
                            frame.render_widget(block, rect);

                            if inner.width > 0 && inner.height > 0 {
                                let guard = term.parser.lock().expect("parser poisoned");
                                frame.render_widget(PseudoTerminal::new(guard.screen()), inner);
                            }
                        }

                        // ── Explorer pane ─────────────────────────────────
                        AppPane::Explorer(exp) => {
                            let border_style = if focused {
                                Style::default().fg(theme::ACCENT).add_modifier(Modifier::BOLD)
                            } else {
                                Style::default().fg(theme::DIM_BORDER)
                            };

                            let title_str = if focused {
                                format!(" [{}] ● ", id)
                            } else {
                                format!(" [{}] ", id)
                            };

                            let title_style = if focused {
                                Style::default().fg(theme::ACCENT).add_modifier(Modifier::BOLD)
                            } else {
                                Style::default().fg(theme::DIM_TEXT)
                            };

                            let block = Block::default()
                                .borders(Borders::ALL)
                                .border_style(border_style)
                                .title(Span::styled(title_str, title_style));

                            let inner = block.inner(rect);
                            frame.render_widget(block, rect);

                            if inner.width > 0 && inner.height > 0 {
                                let items: Vec<ListItem> = exp.entries.iter().map(|e| {
                                    let icon  = if e.is_dir { "📁" } else { "📄" };
                                    let style = if e.is_dir {
                                        Style::default().fg(Color::Blue)
                                    } else {
                                        Style::default()
                                    };
                                    ListItem::new(format!(" {} {}", icon, e.name)).style(style)
                                }).collect();

                                let list = List::new(items)
                                    .highlight_style(
                                        Style::default()
                                            .bg(Color::DarkGray)
                                            .add_modifier(Modifier::BOLD),
                                    )
                                    .highlight_symbol(">> ");

                                frame.render_stateful_widget(
                                    list, inner,
                                    &mut *exp.list_state.borrow_mut(),
                                );
                            }
                        }
                    }
                });
            }

            // ── GUI: floating compositor ──────────────────────────────────
            DesktopMode::Gui => {
                draw_gui(frame, state, content_area);
            }
        }

        // ── Draw Overlay (Always on top, both modes) ──────────────────────
        if let Some(overlay) = &state.overlay {
            // Fix 4: derive the area via `overlay_rect` so the mouse-dismiss
            // logic uses the exact same bounding box as the renderer.
            let overlay_area = overlay_rect(full);

            frame.render_widget(Clear, overlay_area);

            let title = match overlay.action {
                OverlayAction::SpawnCommand      => " Run Command (Alt+Space) ",
                OverlayAction::CreateFile { .. } => " Create File/Dir (ends with / for dir) ",
            };

            let block = Block::default()
                .title(Span::styled(
                    title,
                    Style::default()
                        .fg(theme::ACCENT)
                        .add_modifier(Modifier::BOLD),
                ))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme::ACCENT));

            let text = format!(" {}█", overlay.input);
            frame.render_widget(Paragraph::new(text).block(block), overlay_area);
        }
    })?;
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════════
// § 16  Event loop
// ═══════════════════════════════════════════════════════════════════════════════

/// Process a single `AppEvent`, mutating `state` in-place.
///
/// All rendering is decoupled: instead of calling `draw()` directly, this
/// function sets `*needs_draw = true` whenever the screen should be refreshed.
/// The caller is responsible for issuing exactly one `draw()` call after
/// draining all pending events from the channel.
///
/// Quit conditions (last pane closed, `Alt+Q`) set `*should_quit = true` and
/// return immediately so the outer draining loop can break cleanly.
fn handle_event(
    state:       &mut AppState,
    event:       AppEvent,
    area:        &mut Rect,
    tx:          &mpsc::Sender<AppEvent>,
    needs_draw:  &mut bool,
    should_quit: &mut bool,
) -> Result<()> {
    match event {
        // ── PTY output ────────────────────────────────────────────────────
        AppEvent::PtyOutput { pane_id, bytes } => {
            if let Some(AppPane::Terminal(term)) = state.panes.get(&pane_id) {
                term.parser.lock().expect("parser poisoned").process(&bytes);
            }
            *needs_draw = true;
        }

        AppEvent::ExplorerUpdate { pane_id, path, entries } => {
            if let Some(AppPane::Explorer(exp)) = state.panes.get_mut(&pane_id) {
                exp.cwd = path;
                exp.entries = entries;
                exp.list_state.borrow_mut().select(Some(0));
            }
            *needs_draw = true;
        }

        // ── Shell exited → automatic pane close (or dead-pane retention) ─
        //
        // Fix 4: if the pane was spawned from a custom command (`is_custom`),
        // do NOT prune it.  Instead mark it dead so the renderer can draw a
        // completion banner while leaving the output visible.  The user
        // dismisses it with Alt+X, which goes through the normal `close_pane`
        // path.
        AppEvent::PtyExited { pane_id } => {
            dlog(&format!("event_loop: PtyExited pane_id={pane_id}"));

            let retain = if let Some(AppPane::Terminal(term)) = state.panes.get_mut(&pane_id) {
                if term.is_custom && !term.is_dead {
                    term.is_dead = true;
                    dlog(&format!("event_loop: custom pane {pane_id} marked dead (retained)"));
                    true
                } else {
                    false
                }
            } else {
                false
            };

            if !retain {
                let quit = state.close_pane(pane_id, *area)?;
                dlog(&format!(
                    "event_loop: after close_pane should_quit={quit} remaining_panes={}",
                    state.panes.len()
                ));
                if quit {
                    *should_quit = true;
                    return Ok(());
                }
            }

            *needs_draw = true;
        }

        // ── Input events ──────────────────────────────────────────────────
        AppEvent::Input(ev) => match ev {

            // Fast-path for pasted text (prevents rendering 300 frames
            // for 300 characters — now also benefits from channel draining)
            Event::Paste(text) => {
                if let Some(AppPane::Terminal(term)) = state.panes.get_mut(&state.focus) {
                    let _ = term.writer.write_all(text.as_bytes());
                    let _ = term.writer.flush();
                }
                *needs_draw = true;
            }

            Event::Key(key_ev) => {
                let (quit, new_pane) = dispatch_input(state, *area, key_ev, tx)?;

                if quit {
                    *should_quit = true;
                    return Ok(());
                }

                if let Some((new_id, reader)) = new_pane {
                    spawn_pane_reader(new_id, reader, tx.clone());
                }

                *needs_draw = true;
            }

            Event::Resize(new_cols, new_rows) => {
                *area = Rect {
                    x:      0,
                    y:      1,
                    width:  new_cols,
                    height: new_rows.saturating_sub(2),
                };
                state.resize_all(*area)?;
                *needs_draw = true;
            }

            Event::Mouse(m) if matches!(m.kind, MouseEventKind::Moved) => {}

            // ── Left-click: overlay dismiss, taskbar (GUI) or spatial ────
            //
            // Fix 4: if the overlay is active and the click lands *outside*
            // the popup bounding box, dismiss the overlay immediately and
            // swallow the click — do not route it to the underlying content.
            Event::Mouse(m)
                if matches!(m.kind, MouseEventKind::Down(MouseButton::Left)) =>
            {
                // Reconstruct `full` from `area` (content rect).
                // `area` = { x:0, y:1, width:cols, height:rows-2 }
                // `full` = { x:0, y:0, width:cols, height:rows }
                //        = area offset by the top chrome row, plus both chrome rows.
                let full = Rect {
                    x:      0,
                    y:      0,
                    width:  area.width,
                    height: area.height + 2, // content + top bar + bottom bar
                };

                // Fix 4: overlay dismiss — check before any other routing.
                if state.overlay.is_some() {
                    let ov = overlay_rect(full);
                    // Test whether the click is OUTSIDE the popup.
                    let inside = m.column >= ov.x
                        && m.column < ov.x.saturating_add(ov.width)
                        && m.row    >= ov.y
                        && m.row    < ov.y.saturating_add(ov.height);

                    if !inside {
                        state.overlay = None;
                        *needs_draw = true;
                        // Swallow the click — do not focus/drag underneath.
                        return Ok(());
                    }
                    // Click was inside the popup — fall through so the user
                    // can position the cursor or interact.  No routing to
                    // underlying panes since the overlay consumes input.
                    *needs_draw = true;
                    return Ok(());
                }

                let on_taskbar = m.row >= area.y + area.height;

                if on_taskbar && state.mode == DesktopMode::Gui {
                    if state.click_taskbar(m.column) {
                        *needs_draw = true;
                    }
                } else {
                    let changed = state.click_focus(*area, m.column, m.row);

                    if state.mode == DesktopMode::Gui {
                        if !state.floating_windows.is_empty() {
                            state.drag_state = Some((state.focus, m.column, m.row));
                        }
                    }

                    if changed {
                        *needs_draw = true;
                    }
                }
            }

            // ── Drag: move the focused floating window ────────────────────
            Event::Mouse(m)
                if matches!(m.kind, MouseEventKind::Drag(MouseButton::Left)) =>
            {
                if let Some((id, last_x, last_y)) = state.drag_state {
                    let dx = m.column as i32 - last_x as i32;
                    let dy = m.row    as i32 - last_y as i32;

                    if let Some(win) = state.floating_windows.iter_mut().find(|w| w.id == id) {
                        let new_x = win.area.x as i32 + dx;
                        let new_y = win.area.y as i32 + dy;

                        let max_x = area.width.saturating_sub(win.area.width) as i32;
                        let max_y = area.height.saturating_sub(win.area.height) as i32;

                        win.area.x = new_x.max(0).min(max_x) as u16;
                        win.area.y = new_y.max(0).min(max_y) as u16;
                    }

                    state.drag_state = Some((id, m.column, m.row));
                    *needs_draw = true;
                }
            }

            // ── Mouse-up: release drag ────────────────────────────────────
            Event::Mouse(m)
                if matches!(m.kind, MouseEventKind::Up(MouseButton::Left)) =>
            {
                state.drag_state = None;
            }

            // ── Scroll wheel: Explorer navigation ────────────────────────
            Event::Mouse(m)
                if matches!(
                    m.kind,
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                ) =>
            {
                let focused_id = state.focus;
                if let Some(AppPane::Explorer(exp)) = state.panes.get_mut(&focused_id) {
                    let i = exp.list_state.borrow().selected().unwrap_or(0);
                    match m.kind {
                        MouseEventKind::ScrollUp => {
                            if i > 0 {
                                exp.list_state.borrow_mut().select(Some(i - 1));
                                *needs_draw = true;
                            }
                        }
                        MouseEventKind::ScrollDown => {
                            if i < exp.entries.len().saturating_sub(1) {
                                exp.list_state.borrow_mut().select(Some(i + 1));
                                *needs_draw = true;
                            }
                        }
                        _ => {}
                    }
                }
            }

            Event::Mouse(_) => {}
            _ => {}
        },
    }
    Ok(())
}

pub async fn run_event_loop(
    terminal:     &mut Terminal<CrosstermBackend<io::Stdout>>,
    state:        &mut AppState,
    content_area: Rect,
    rx:           &mut mpsc::Receiver<AppEvent>,
    tx:           &mpsc::Sender<AppEvent>,
) -> Result<()> {
    let mut area = content_area;
    draw(terminal, state)?;

    loop {
        let first_event = tokio::select! {
            ev = rx.recv() => match ev {
                Some(e) => e,
                None    => break,
            },
            _ = tokio::time::sleep(Duration::from_millis(500)) => continue,
        };

        let mut needs_draw  = false;
        let mut should_quit = false;

        handle_event(state, first_event, &mut area, tx, &mut needs_draw, &mut should_quit)?;

        // ── Channel draining ──────────────────────────────────────────────
        while let Ok(next_ev) = rx.try_recv() {
            if should_quit { break; }
            handle_event(state, next_ev, &mut area, tx, &mut needs_draw, &mut should_quit)?;
        }

        if should_quit { break; }
        if needs_draw  { draw(terminal, state)?; }
    }
    Ok(())
}
