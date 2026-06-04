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
    time::{Duration, Instant},
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
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect, Alignment},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph},
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
    /// Periodic hardware-stats tick from the background sysinfo poller.
    /// The payload is a pre-formatted status string ready for direct rendering.
    SystemTick(String),
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
#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
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
    /// Right-click context menu on a file/directory entry.
    ///
    /// `target_path` — the absolute path of the entry that was right-clicked.
    /// `is_dir`      — whether the entry is a directory (affects Delete behaviour).
    /// `menu_index`  — the currently highlighted item (0 = Open, 1 = Delete, 2 = Cancel).
    /// `x` / `y`    — terminal cell coordinates where the menu's top-left corner spawns.
    ContextMenu {
        target_path: PathBuf,
        is_dir:      bool,
        menu_index:  usize,
        x:           u16,
        y:           u16,
    },
}

pub struct AppOverlay {
    pub action: OverlayAction,
    pub input:  String,
}

// ═══════════════════════════════════════════════════════════════════════════════
// § 9  Session persistence
// ═══════════════════════════════════════════════════════════════════════════════

/// Serialisable description of a single pane.
///
/// We cannot serialise live PTY state (`TerminalPane`) or the `RefCell`-heavy
/// `ExplorerPane` directly.  Instead we capture only the information needed to
/// *recreate* the pane on the next startup: its id, its kind, and — for
/// Explorer panes — the directory it was browsing.  Terminal panes are always
/// restored as a fresh shell.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub struct PaneBlueprint {
    pub id:   PaneId,
    pub kind: PaneBlueprintKind,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub enum PaneBlueprintKind {
    Terminal { custom_command: Option<String> },
    Explorer { cwd: PathBuf },
}

/// The complete on-disk session snapshot.
///
/// Serialised as JSON by `save_session` and deserialised by `load_session`.
/// Every field is `pub` so the caller (typically `main`) can inspect or patch
/// the struct before handing it to `load_session` if needed.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct SessionState {
    /// The binary tiling tree.  Pane ids inside the tree match those in `panes`.
    pub layout:           LayoutNode,
    /// The pane that had keyboard focus when the session was saved.
    pub focus:            PaneId,
    /// The next free pane id (monotonically increasing allocator).
    pub next_id:          PaneId,
    /// Whether the session was saved in Tiling or Gui desktop mode.
    pub mode:             DesktopMode,
    /// The ordered floating-window stack (back → front).
    /// May be empty when `mode == DesktopMode::Tiling`.
    pub floating_windows: Vec<FloatingWindow>,
    /// One blueprint per pane, in arbitrary order.
    pub panes:            Vec<PaneBlueprint>,
}

/// Serialize `state` to a `SessionState` and write it atomically to `path`.
///
/// ### Atomic write strategy
///
/// We write to `<path>.tmp` first, then `rename` over `path`.  On any POSIX
/// filesystem `rename` is atomic with respect to crashes, so a power-loss
/// during the write never leaves a truncated session file — the previous
/// session is either fully present or replaced by the new one.
///
/// ### What is NOT saved
///
/// * The PTY scroll-back buffer and current screen contents — restored as a
///   fresh shell instead.
/// * `AppState::overlay` — transient UI state, always starts closed.
/// * `AppState::drag_state` — transient UI state, always starts `None`.
/// * Explorer `selected_index` / `scroll_offset` / `entries` — re-populated
///   by the background `spawn_dir_read` task at startup.
pub fn save_session(state: &AppState, path: &std::path::Path) -> Result<()> {
    // Build the blueprints list by iterating the live pane map.
    // The map is unordered; order doesn't matter because `load_session`
    // looks up blueprints by id rather than position.
    let panes: Vec<PaneBlueprint> = state.panes.iter().map(|(&id, pane)| {
        let kind = match pane {
            AppPane::Terminal(term) => PaneBlueprintKind::Terminal { 
                custom_command: term.custom_command.clone()
            },
            AppPane::Explorer(exp) => PaneBlueprintKind::Explorer { cwd: exp.cwd.clone() },
        };
        PaneBlueprint { id, kind }
    }).collect();

    let session = SessionState {
        layout:           state.layout.clone(),
        focus:            state.focus,
        next_id:          state.next_id,
        mode:             state.mode,
        floating_windows: state.floating_windows.clone(),
        panes,
    };

    // Serialize to JSON.
    let json = serde_json::to_string_pretty(&session)?;

    // Atomic write: write to a temp file alongside the target, then rename.
    let tmp_path = path.with_extension("json.tmp");
    std::fs::write(&tmp_path, json.as_bytes())?;
    std::fs::rename(&tmp_path, path)?;

    dlog(&format!("save_session: wrote {} bytes to {}", json.len(), path.display()));
    Ok(())
}

/// Deserialise a `SessionState` from `path` and reconstruct a live `AppState`.
///
/// Returns `(AppState, Vec<(PaneId, Box<dyn Read + Send>)>)`.  The caller is
/// responsible for calling `spawn_pane_reader` for every `(id, reader)` pair,
/// and `spawn_dir_read` for every Explorer pane, before entering the event loop.
///
/// ### Fallibility
///
/// This function returns `Err` on I/O or JSON parse failures.  The caller
/// (`main`) should fall back to `AppState::new()` when this returns `Err` so
/// that a missing or corrupt session file is silently treated as a fresh start.
///
/// ### PTY size
///
/// `area` is the content `Rect` computed from the initial terminal size.
/// All panes are sized to fit inside `area`; the layout tree's ratio values
/// are preserved so the proportions match what was saved.
pub fn load_session(
    path:  &std::path::Path,
    area:  Rect,
    tx:    &mpsc::Sender<AppEvent>,
) -> Result<(AppState, Vec<(PaneId, Box<dyn Read + Send>)>)> {
    let json = std::fs::read_to_string(path)?;
    let session: SessionState = serde_json::from_str(&json)?;

    dlog(&format!(
        "load_session: restoring {} pane(s), mode={:?}, focus={}",
        session.panes.len(), session.mode, session.focus,
    ));

    let mut panes:   HashMap<PaneId, AppPane> = HashMap::new();
    let mut readers: Vec<(PaneId, Box<dyn Read + Send>)> = Vec::new();

    // Re-create every pane from its blueprint.
    // PTY dimensions: derive from `area` using the same formula as
    // `AppState::new` so the terminal is never created with zero rows/cols.
    let rows = area.height.saturating_sub(2).max(2);
    let cols = area.width.saturating_sub(2).max(8);

    for bp in &session.panes {
        match &bp.kind {
            PaneBlueprintKind::Terminal { custom_command } => {
                let (pane, reader) = TerminalPane::new(bp.id, rows, cols, custom_command.clone())?;
                panes.insert(bp.id, AppPane::Terminal(pane));
                readers.push((bp.id, reader));
                dlog(&format!("load_session: spawned terminal pane {}", bp.id));
            }
            PaneBlueprintKind::Explorer { cwd } => {
                let explorer = ExplorerPane::new(bp.id, cwd.clone());
                panes.insert(bp.id, AppPane::Explorer(explorer));
                // Kick off the background directory read so entries populate
                // immediately after the event loop starts.
                spawn_dir_read(bp.id, cwd.clone(), tx.clone());
                dlog(&format!("load_session: restored explorer pane {} at {}", bp.id, cwd.display()));
            }
        }
    }

    // Validate: if the session file is corrupt and references pane ids that
    // weren't restored, fall back to a single-pane default rather than
    // crashing inside `AppState` methods that assume the layout is consistent.
    let valid_focus = if panes.contains_key(&session.focus) {
        session.focus
    } else {
        // Pick the numerically smallest id we actually have.
        panes.keys().copied().min().unwrap_or(0)
    };

    let state = AppState {
        layout:           session.layout,
        panes,
        focus:            valid_focus,
        next_id:          session.next_id,
        overlay:          None,
        mode:             session.mode,
        floating_windows: session.floating_windows,
        drag_state:       None,
        sys_status:       String::new(),
    };

    Ok((state, readers))
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
    /// Pre-formatted system-tray string updated every second by the background
    /// sysinfo poller.  Empty until the first `SystemTick` arrives.
    pub sys_status:       String,
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
                    sys_status:       String::new(),
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
        cmd:  Option<String>,
    ) -> Result<Option<(PaneId, Box<dyn Read + Send>)>> {
        let new_id = self.next_id;
        self.next_id += 1;
        // ── Handle spawning from an empty desktop ─────────────────────────
        if self.panes.is_empty() {
            let rows = area.height.saturating_sub(2).max(2);
            let cols = area.width.saturating_sub(2).max(8);
            let (pane, reader) = TerminalPane::new(new_id, rows, cols, cmd)?;
            self.panes.insert(new_id, AppPane::Terminal(pane));
            self.layout = LayoutNode::Pane(new_id);
            self.focus = new_id;
            if self.mode == DesktopMode::Gui {
                self.floating_windows.push(FloatingWindow::new(new_id, cascade_rect(0, area)));
            }
            return Ok(Some((new_id, reader)));
        }
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

        // ── Handle spawning from an empty desktop ─────────────────────────
        if self.panes.is_empty() {
            self.panes.insert(new_id, AppPane::Explorer(explorer));
            self.layout = LayoutNode::Pane(new_id);
            self.focus = new_id;
            if self.mode == DesktopMode::Gui {
                self.floating_windows.push(FloatingWindow::new(new_id, cascade_rect(0, area)));
            }
            spawn_dir_read(new_id, home, tx);
            return Ok(());
        }
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
            self.layout = LayoutNode::Sentinel; // Empty the tree
            return Ok(false);
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

    /// Map a raw `PaneId` to a stable 1-based display number shown in borders
    /// and taskbar badges.
    ///
    /// Pane ids are allocated monotonically and never reused within a session,
    /// so closing pane 2 of [1, 2, 3] would leave a gap if we showed the raw
    /// id.  Instead we sort all *currently live* ids and return the 1-based
    /// position of `target` in that sorted list, giving the user a compact
    /// sequence that always starts at 1 and has no holes.
    ///
    /// `unwrap_or(0) + 1` means an unknown id safely shows as "1" rather than
    /// panicking; in practice this path is unreachable because callers only
    /// pass ids that exist in `self.panes`.
    pub fn display_num(&self, target: PaneId) -> usize {
        let mut ids: Vec<PaneId> = self.panes.keys().copied().collect();
        ids.sort_unstable();
        ids.iter().position(|&id| id == target).unwrap_or(0) + 1
    }

    /// Build the inner text of a taskbar badge for `id` without allocating on
    /// the common (shell-idle) path.  Returns `"{num}: {process_name}"`.
    ///
    /// Uses `display_num` so the badge shows a compact 1-based window number
    /// rather than the raw allocator id.
    #[inline]
    fn taskbar_label_for(&self, id: PaneId) -> String {
        let num = self.display_num(id);
        match self.panes.get(&id) {
            Some(AppPane::Terminal(term)) => {
                let name = term.custom_command.clone().unwrap_or_else(|| term.foreground_process_name());
                if term.is_dead {
                    format!("{num}: [Exited]")
                } else {
                    format!("{num}: {name}")
                }
            }
            Some(AppPane::Explorer(_)) => format!("{num}: Explorer"),
            None                       => format!("{num}: ?"),
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

                        // Dynamic process name with 1-based display number.
                        let num = state.display_num(id);
                        let label = match state.panes.get(&id) {
                            Some(AppPane::Terminal(term)) => {
                                format!("[{}: {}]", num, term.foreground_process_name())
                            }
                            Some(AppPane::Explorer(_)) => format!("[{num}: Explorer]"),
                            None                       => format!("[{num}: ?]"),
                        };

                        taskbar.push(Span::styled(label, badge_style));
                    }

                    frame.render_widget(Paragraph::new(Line::from(taskbar)), bot_area);

                    // ── System Tray ───────────────────────────────────────────
                    //
                    // Split `bot_area` into [taskbar | sys_tray] only when there
                    // is something to show.  Using a fixed `Length(45)` for the
                    // tray keeps the taskbar badges from jumping as the clock
                    // ticks.  The tray cell is right-aligned so the clock/stats
                    // sit flush against the right edge.
                    if !state.sys_status.is_empty() {
                        let bot_chunks = Layout::default()
                            .direction(Direction::Horizontal)
                            .constraints([
                                Constraint::Min(0),
                                Constraint::Length(45),
                            ])
                            .split(bot_area);

                        let taskbar_area  = bot_chunks[0];
                        let sys_tray_area = bot_chunks[1];

                        // Re-render the taskbar into the (now narrower) left chunk.
                        // We must rebuild the `taskbar` spans here because the Vec
                        // was moved into the Paragraph above.
                        let n_windows2 = state.floating_windows.len();
                        let mut taskbar2: Vec<Span> = Vec::with_capacity(1 + n_windows2 * 2);
                        taskbar2.push(Span::styled(
                            "[ TDE Start ]",
                            Style::default()
                                .fg(theme::TITLE_BADGE_FG)
                                .bg(theme::ACCENT)
                                .add_modifier(Modifier::BOLD),
                        ));
                        let mut sorted_ids2: Vec<PaneId> =
                            state.floating_windows.iter().map(|w| w.id).collect();
                        sorted_ids2.sort_unstable();
                        for id2 in sorted_ids2 {
                            taskbar2.push(Span::raw("  "));
                            let is_focused2 = id2 == state.focus;
                            let badge_style2 = if is_focused2 {
                                Style::default()
                                    .fg(theme::TASKBAR_FOCUS_FG)
                                    .bg(theme::TASKBAR_FOCUS_BG)
                                    .add_modifier(Modifier::BOLD)
                            } else {
                                Style::default().fg(theme::DIM_TEXT)
                            };
                            let num2 = state.display_num(id2);
                            let label2 = match state.panes.get(&id2) {
                                Some(AppPane::Terminal(term)) => {
                                    format!("[{}: {}]", num2, term.foreground_process_name())
                                }
                                Some(AppPane::Explorer(_)) => format!("[{num2}: Explorer]"),
                                None                       => format!("[{num2}: ?]"),
                            };
                            taskbar2.push(Span::styled(label2, badge_style2));
                        }
                        frame.render_widget(Clear, bot_area);
                        frame.render_widget(Paragraph::new(Line::from(taskbar2)), taskbar_area);

                        frame.render_widget(
                            Paragraph::new(state.sys_status.as_str())
                                .alignment(Alignment::Right)
                                .style(Style::default().fg(theme::ACCENT)),
                            sys_tray_area,
                        );
                    }
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
                                            format!(" [{}] ✖ Process Completed — Alt+X to close ", state.display_num(id)),
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

                                let name = term.custom_command.clone().unwrap_or_else(|| term.foreground_process_name());
                                let num = state.display_num(id);
                                let title_str = if focused {
                                    format!(" [{}] ● {} ", num, name)
                                } else {
                                    format!(" [{}] {} ", num, name)
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

                                let num = state.display_num(id);
                                let title_str = if focused {
                                    format!(" [{}] ● Explorer ", num)
                                } else {
                                    format!(" [{}] Explorer ", num)
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
                                    render_explorer_grid(frame, exp, inner);
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
                match &overlay.action {
                    // ── Context Menu ─────────────────────────────────────────
                    //
                    // Render a 3-item floating List directly under the cursor.
                    // The menu is 20 columns wide and 5 rows tall (3 items + 2
                    // border rows).  We clamp both axes so the box never bleeds
                    // off the terminal edge.
                    OverlayAction::ContextMenu { menu_index, x, y, .. } => {
                        const MENU_W: u16 = 20;
                        const MENU_H: u16 = 5; // 3 items + top + bottom border

                        // Clamp so the menu stays entirely on-screen.
                        let clamped_x = (*x).min(full.width.saturating_sub(MENU_W));
                        let clamped_y = (*y).min(full.height.saturating_sub(MENU_H));

                        let menu_area = Rect {
                            x:      clamped_x,
                            y:      clamped_y,
                            width:  MENU_W,
                            height: MENU_H,
                        };

                        let items: Vec<ListItem> = vec![
                            ListItem::new(" 📂 Open   "),
                            ListItem::new(" 🗑  Delete "),
                            ListItem::new(" ❌ Cancel "),
                        ];

                        let highlight_style = Style::default()
                            .fg(Color::Black)
                            .bg(theme::ACCENT)
                            .add_modifier(Modifier::BOLD);

                        let menu_list = List::new(items)
                            .block(
                                Block::default()
                                    .borders(Borders::ALL)
                                    .border_style(Style::default().fg(theme::ACCENT)),
                            )
                            .highlight_style(highlight_style)
                            .highlight_symbol("▶ ");

                        let mut list_state = ListState::default();
                        list_state.select(Some(*menu_index));

                        frame.render_widget(Clear, menu_area);
                        frame.render_stateful_widget(menu_list, menu_area, &mut list_state);
                    }

                    // ── Text-input overlays (SpawnCommand, CreateFile) ────────
                    _ => {
                        // Fix 4: derive the area via `overlay_rect` so the
                        // mouse-dismiss logic uses the exact same bounding box.
                        let overlay_area = overlay_rect(full);

                        frame.render_widget(Clear, overlay_area);

                        let title = match overlay.action {
                            OverlayAction::SpawnCommand      => " Run Command (Alt+Space) ",
                            OverlayAction::CreateFile { .. } => " Create File/Dir (ends with / for dir) ",
                            OverlayAction::ContextMenu { .. } => unreachable!(),
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
                }
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
                *exp.selected_index.borrow_mut() = 0;
                *exp.scroll_offset.borrow_mut() = 0;
            }
            *needs_draw = true;
        }

        // ── System tray tick ─────────────────────────────────────────────
        AppEvent::SystemTick(status) => {
            state.sys_status = status;
            // Only trigger a redraw when in GUI mode — in Tiling mode the
            // tray is not rendered, so there is nothing to refresh.
            if state.mode == DesktopMode::Gui {
                *needs_draw = true;
            }
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
                        // ── Context Menu: hit-test and dispatch ───────────────
                        // Delegate to `input::dispatch_mouse_overlay`, which owns
                        // all context-menu mouse logic (hit-test, Open/Delete/Cancel
                        // execution).  That function takes the overlay out of state
                        // itself — if the click was inside the menu it executes the
                        // action and drops the overlay; if outside it also drops it
                        // (dismiss).  Either way the overlay is gone after the call,
                        // so we never fall through to the normal pane-focus routing.
                        // ── Context Menu: check for click interactions ─────────
                        if matches!(
                            state.overlay.as_ref().map(|o| &o.action),
                            Some(OverlayAction::ContextMenu { .. })
                        ) {
                            if let Some((new_id, reader)) = crate::input::dispatch_mouse_overlay(state, *area, m, tx)? {
                                crate::pty::spawn_pane_reader(new_id, reader, tx.clone());
                            }
                            *needs_draw = true;
                            return Ok(());
                        }

                        // ── Text-input overlays: dismiss only on outside click ─
                        let ov = overlay_rect(full);
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
                        enum ExplorerClickAction {
                            OpenDir(PathBuf),
                            OpenFile(PathBuf),
                        }

                        let mut explorer_action: Option<ExplorerClickAction> = None;

                        let focused_id = state.focus;

                        let pane_rect = match state.mode {
                            DesktopMode::Tiling => {
                                let mut found = None;

                                state.layout.walk_rects(*area, &mut |id, rect| {
                                    if id == focused_id {
                                        found = Some(rect);
                                    }
                                });

                                found
                            }

                            DesktopMode::Gui => {
                                state
                                    .floating_windows
                                    .iter()
                                    .find(|w| w.id == focused_id)
                                    .map(|w| w.area)
                            }
                        };

                        if let Some(rect) = pane_rect {
                            if let Some(AppPane::Explorer(exp)) = state.panes.get_mut(&focused_id) {

                                if m.row >= rect.y + 3 {

                                    let local_x = m.column.saturating_sub(rect.x);
                                    let local_y = m.row.saturating_sub(rect.y + 3);

                                    let cell_width: u16 = 14;
                                    let cell_height: u16 = 4;

                                    let cols =
                                        ((rect.width.max(cell_width)) / cell_width).max(1) as usize;

                                    let grid_col = (local_x / cell_width) as usize;
                                    let grid_row = (local_y / cell_height) as usize;

                                    let scroll_offset = *exp.scroll_offset.borrow();

                                    let clicked_idx =
                                        ((scroll_offset + grid_row) * cols) + grid_col;

                                    if clicked_idx < exp.entries.len() {

                                        let selection_changed = *exp.selected_index.borrow() != clicked_idx;
                                        *exp.selected_index.borrow_mut() = clicked_idx;
                                        if selection_changed {
                                            *needs_draw = true;
                                        }

                                        let now = Instant::now();

                                        let is_double_click = {
                                            let mut last = exp.last_click.borrow_mut();

                                            let result = match *last {
                                                Some((idx, ts))
                                                    if idx == clicked_idx
                                                        && now.duration_since(ts)
                                                            < Duration::from_millis(500) =>
                                                    {
                                                        true
                                                    }

                                                _ => false,
                                            };

                                            *last = Some((clicked_idx, now));

                                            result
                                        };

                                        if is_double_click {
                                            let entry = exp.entries[clicked_idx].clone();

                                            let path = if entry.name == ".." {
                                                exp.cwd
                                                    .parent()
                                                    .map(|p| p.to_path_buf())
                                                    .unwrap_or_else(|| exp.cwd.clone())
                                            } else {
                                                exp.cwd.join(&entry.name)
                                            };

                                            explorer_action = Some(
                                                if entry.is_dir {
                                                    ExplorerClickAction::OpenDir(path)
                                                } else {
                                                    ExplorerClickAction::OpenFile(path)
                                                }
                                            );
                                        }
                                    }
                                }
                            }
                        }
                        match explorer_action {
                            Some(ExplorerClickAction::OpenDir(path)) => {
                                spawn_dir_read(focused_id, path, tx.clone());
                                *needs_draw = true;
                            }

                            Some(ExplorerClickAction::OpenFile(path)) => {
                                let cmd_str = format!("nvim {}", path.display());

                                if let Some((new_id, reader)) =
                                    state.do_split(*area, SplitKind::Vertical, Some(cmd_str))?
                                {
                                    spawn_pane_reader(new_id, reader, tx.clone());
                                }

                                *needs_draw = true;
                            }

                            None => {}
                        }

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

            // ── Right-click: spawn Context Menu over an Explorer entry ────
            //
            // Hit-test mirrors the left-click explorer grid math exactly:
            //   • border = 1 cell each side  (Block::default().borders(Borders::ALL))
            //   • header = 3 rows            (render_explorer_grid header_area height)
            //   • cell_width = 14, cell_height = 4  (same constants as left-click)
            //
            // We deliberately do NOT use `click_focus` here: the focus stays on
            // whatever was already active; the menu is ephemeral.  If the user
            // right-clicks *outside* all Explorer panes the event is silently
            // swallowed (no menu is shown).
            Event::Mouse(m)
                if matches!(m.kind, MouseEventKind::Down(MouseButton::Right)) =>
                {
                    // Dismiss any open overlay first (keeps state clean).
                    if state.overlay.is_some() {
                        state.overlay = None;
                        *needs_draw = true;
                    }

                    // Identify which pane (if any) was right-clicked.
                    let col = m.column;
                    let row = m.row;

                    let mut hit_pane: Option<(PaneId, Rect)> = None;

                    match state.mode {
                        DesktopMode::Tiling => {
                            state.layout.walk_rects(*area, &mut |id, rect| {
                                if col >= rect.x
                                    && col < rect.x + rect.width
                                    && row >= rect.y
                                    && row < rect.y + rect.height
                                {
                                    hit_pane = Some((id, rect));
                                }
                            });
                        }
                        DesktopMode::Gui => {
                            // Top-most window wins (reverse iteration).
                            for win in state.floating_windows.iter().rev() {
                                if col >= win.area.x
                                    && col < win.area.x + win.area.width
                                    && row >= win.area.y
                                    && row < win.area.y + win.area.height
                                {
                                    hit_pane = Some((win.id, win.area));
                                    break;
                                }
                            }
                        }
                    }

                    if let Some((pane_id, rect)) = hit_pane {
                        // Only open a context menu on Explorer panes.
                        //
                        // The explorer inner area is:
                        //   x: rect.x + 1  (left border)
                        //   y: rect.y + 1 + 3  (top border + 3-row header rendered by
                        //                        render_explorer_grid)
                        // Which means grid content starts at rect.y + 4 in absolute
                        // terminal rows.  However `render_explorer_grid` receives
                        // `block.inner(rect)` which subtracts 1 from each edge —
                        // the inner area's y is `rect.y + 1`.  The header_area is
                        // then 3 rows inside that inner area, so actual grid rows
                        // begin at `rect.y + 1 + 3 = rect.y + 4`.
                        //
                        // The left-click handler uses `rect.y + 3` (where rect is the
                        // *inner* content area passed into render_explorer_grid, which
                        // already has the border subtracted).  In handle_event the
                        // pane_rect is the full block rect, so we must offset by the
                        // border (1) + header (3) = 4 from the outer rect.y.
                        if row >= rect.y + 4 {
                            if let Some(AppPane::Explorer(exp)) =
                                state.panes.get_mut(&pane_id)
                            {
                                // ── Grid math (identical to left-click handler) ──
                                // The left-click handler offsets by rect.y + 3 for
                                // `local_y`.  That handler receives pane_rect from
                                // walk_rects/floating_windows which is the *outer*
                                // block rect.  But then it subtracts `rect.y + 3`
                                // from m.row — that "+3" accounts for the top border
                                // (1 row) plus the 2-row path header inside inner,
                                // i.e. inner.y = rect.y+1, header_area height = 3,
                                // so grid starts at inner.y+3 = rect.y+4.
                                // Wait — left-click uses `rect.y + 3` not `rect.y + 4`.
                                // Re-read: in left click, rect comes from walk_rects
                                // (outer rect), and `m.row >= rect.y + 3` guards the
                                // grid; then `local_y = m.row - (rect.y + 3)`.
                                // That's because render_explorer_grid is called with
                                // block.inner(rect) — inner subtracts border (1) →
                                // inner.y = rect.y+1.  Header_area height = 3 with y
                                // = inner.y.  Grid area starts at inner.y+3 = rect.y+4.
                                // BUT the guard is `>= rect.y + 3` and local_y
                                // = `m.row - (rect.y + 3)`, so when m.row == rect.y+3
                                // local_y==0 which maps to the very first grid row.
                                // This is a 1-row off-by-one that the existing code
                                // lives with (it selects the header row as row 0).
                                // We replicate this exactly so right-click and
                                // left-click select the same cell index.
                                let local_x = col.saturating_sub(rect.x);
                                let local_y = row.saturating_sub(rect.y + 3);

                                let cell_width:  u16 = 14;
                                let cell_height: u16 = 4;

                                let cols =
                                    ((rect.width.max(cell_width)) / cell_width).max(1) as usize;

                                let grid_col = (local_x / cell_width) as usize;
                                let grid_row = (local_y / cell_height) as usize;

                                let scroll_offset = *exp.scroll_offset.borrow();

                                let entry_idx =
                                    ((scroll_offset + grid_row) * cols) + grid_col;

                                if entry_idx < exp.entries.len() {
                                    let entry = &exp.entries[entry_idx];
                                    let target_path = if entry.name == ".." {
                                        exp.cwd
                                            .parent()
                                            .map(|p| p.to_path_buf())
                                            .unwrap_or_else(|| exp.cwd.clone())
                                    } else {
                                        exp.cwd.join(&entry.name)
                                    };
                                    let is_dir = entry.is_dir;

                                    // Update selection to the right-clicked cell.
                                    *exp.selected_index.borrow_mut() = entry_idx;

                                    // Spawn the context menu directly under the cursor.
                                    state.overlay = Some(AppOverlay {
                                        action: OverlayAction::ContextMenu {
                                            target_path,
                                            is_dir,
                                            menu_index: 0,
                                            x: col,
                                            y: row,
                                        },
                                        input: String::new(),
                                    });
                                    *needs_draw = true;
                                }
                            }
                        }
                    }
                }

            // ── Drag: move the focused floating window ────────────────────
            //
            // Unsnap logic: if the window being dragged is currently snapped
            // (unsnapped_area.is_some()), restore its pre-snap geometry first,
            // then re-centre it under the cursor before applying normal delta
            // movement.  This gives the Windows-Aero-Snap "grab and pull away"
            // behaviour without any heap allocation.
            Event::Mouse(m)
                if matches!(m.kind, MouseEventKind::Drag(MouseButton::Left)) =>
                {
                    if let Some((id, last_x, last_y)) = state.drag_state {
                        let dx = m.column as i32 - last_x as i32;
                        let dy = m.row    as i32 - last_y as i32;

                        if let Some(win) = state.floating_windows.iter_mut().find(|w| w.id == id) {
                            // ── Unsnap: restore saved geometry and re-centre ──
                            if let Some(saved) = win.unsnapped_area.take() {
                                win.area = saved;
                                // Horizontally centre the restored window on the
                                // cursor so it doesn't jump to an arbitrary edge.
                                win.area.x = m.column.saturating_sub(win.area.width / 2);
                                // win.area.y is kept from the saved rect; the
                                // normal boundary-clamping below will correct it
                                // if it has gone out of range.
                            }

                            // ── Normal delta movement ─────────────────────────
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

            // ── Mouse-up: release drag, apply snap if at an edge ─────────
            //
            // Aero-Snap zones (all thresholds use saturating arithmetic):
            //
            //   Left snap   — cursor column ≤ area.x + 1
            //                 → fills left half  (x: area.x, w: area.width/2)
            //   Right snap  — cursor column ≥ area.x + area.width - 2
            //                 → fills right half (x: area.x + area.width/2, w: area.width - area.width/2)
            //   Maximise    — cursor row    ≤ area.y + 1
            //                 → fills entire content area
            //
            // In every snap case the current `win.area` is first saved into
            // `win.unsnapped_area` (only when not already snapped, to avoid
            // overwriting the original geometry with a half-screen rect).
            Event::Mouse(m)
                if matches!(m.kind, MouseEventKind::Up(MouseButton::Left)) =>
                {
                    if let Some((id, _, _)) = state.drag_state {
                        if let Some(win) = state.floating_windows.iter_mut().find(|w| w.id == id) {
                            // Snap-zone thresholds.
                            let left_edge  = area.x.saturating_add(1);
                            let right_edge = area.x.saturating_add(area.width).saturating_sub(2);
                            let top_edge   = area.y.saturating_add(1);

                            if m.column <= left_edge {
                                // ── Left snap ────────────────────────────────
                                if win.unsnapped_area.is_none() {
                                    win.unsnapped_area = Some(win.area);
                                }
                                let half_w = area.width / 2;
                                win.area = Rect {
                                    x:      area.x,
                                    y:      area.y,
                                    width:  half_w,
                                    height: area.height,
                                };

                            } else if m.column >= right_edge {
                                // ── Right snap ───────────────────────────────
                                if win.unsnapped_area.is_none() {
                                    win.unsnapped_area = Some(win.area);
                                }
                                let half_w = area.width / 2;
                                win.area = Rect {
                                    x:      area.x.saturating_add(half_w),
                                    y:      area.y,
                                    // Give the right pane the remaining width so
                                    // the two halves tile without a gap on odd
                                    // terminal widths.
                                    width:  area.width.saturating_sub(half_w),
                                    height: area.height,
                                };

                            } else if m.row <= top_edge {
                                // ── Maximise ─────────────────────────────────
                                if win.unsnapped_area.is_none() {
                                    win.unsnapped_area = Some(win.area);
                                }
                                win.area = Rect {
                                    x:      area.x,
                                    y:      area.y,
                                    width:  area.width,
                                    height: area.height,
                                };
                            }
                            // No snap zone hit → area unchanged, leave
                            // unsnapped_area as-is (None for a normal float,
                            // Some(_) if it was already snapped before this drag).
                        }
                    }

                    state.drag_state = None;
                    *needs_draw = true;
                }

            // ── Scroll wheel: Explorer navigation ────────────────────────
            Event::Mouse(m)
                if matches!(
                    m.kind,
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                ) =>
                {
                    let focused_id = state.focus;

                    let pane_rect = match state.mode {
                        DesktopMode::Tiling => {
                            let mut found = None;

                            state.layout.walk_rects(*area, &mut |id, rect| {
                                if id == focused_id {
                                    found = Some(rect);
                                }
                            });

                            found
                        }

                        DesktopMode::Gui => state
                            .floating_windows
                            .iter()
                            .find(|w| w.id == focused_id)
                            .map(|w| w.area),
                    };

                    if let (Some(rect), Some(AppPane::Explorer(exp))) =
                        (pane_rect, state.panes.get_mut(&focused_id))
                    {
                        let inner_width = rect.width.saturating_sub(2);
                        let inner_height = rect.height.saturating_sub(2);
                        let grid_width = inner_width;
                        let grid_height = inner_height.saturating_sub(3);

                        let cell_width: u16 = 14;
                        let cell_height: u16 = 4;
                        let cols = (grid_width / cell_width).max(1) as usize;
                        let visible_rows = (grid_height / cell_height).max(1) as usize;
                        let total_rows = exp.entries.len().div_ceil(cols);
                        let max_scroll = total_rows.saturating_sub(visible_rows);

                        let cur = *exp.scroll_offset.borrow();
                        let next = match m.kind {
                            MouseEventKind::ScrollUp => cur.saturating_sub(1),
                            MouseEventKind::ScrollDown => cur.saturating_add(1).min(max_scroll),
                            _ => cur,
                        };

                        if next != cur {
                            *exp.scroll_offset.borrow_mut() = next;
                            *needs_draw = true;
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

    // Save the session to ~/.config/tde/session.json exactly once on exit
    let session_dir = std::env::var("HOME")
        .map(|h| std::path::PathBuf::from(h).join(".config").join("tde"))
        .unwrap_or_else(|_| std::path::PathBuf::from("/tmp/tde"));
    let session_path = session_dir.join("session.json");

    if let Err(e) = save_session(state, &session_path) {
        dlog(&format!("Failed to save session: {}", e));
    }

    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════════
// § 19  Grid Renderer (Dolphin-style Large Icons)
// ═══════════════════════════════════════════════════════════════════════════════

pub fn render_explorer_grid(
    frame: &mut ratatui::Frame,
    exp: &ExplorerPane,
    area: Rect,
) {
    if area.height < 6 {
        return;
    }

    let header_area = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: 3,
    };

    let grid_area = Rect {
        x: area.x,
        y: area.y + 3,
        width: area.width,
        height: area.height.saturating_sub(3),
    };

    let header = Block::default().borders(Borders::BOTTOM);
    let header_inner = header.inner(header_area);

    frame.render_widget(header, header_area);

    frame.render_widget(
        Paragraph::new(format!(" 📁 {} ", exp.cwd.display())),
        header_inner,
    );

    let cell_width = 14;
    let cell_height = 4;

    if grid_area.width < cell_width || grid_area.height < cell_height {
        return;
    }

    let cols = (grid_area.width / cell_width).max(1) as usize;
    let rows = (grid_area.height / cell_height).max(1) as usize;

    let selected = *exp.selected_index.borrow();

    // Clamp the viewport scroll to valid grid rows, but do not force the
    // selected item back into view. Mouse-wheel scrolling should move the
    // visible viewport like a desktop file manager, independent of selection.
    let total_rows = exp.entries.len().div_ceil(cols);
    let max_scroll = total_rows.saturating_sub(rows);
    let scroll = (*exp.scroll_offset.borrow()).min(max_scroll);
    *exp.scroll_offset.borrow_mut() = scroll;

    let start_idx = scroll * cols;
    let end_idx = (start_idx + rows * cols).min(exp.entries.len());

    // ── Draw Visible Cells ──
    for i in start_idx..end_idx {
        let entry = &exp.entries[i];
        let is_selected = i == selected;

        let rel_idx = i - start_idx;
        let grid_x = (rel_idx % cols) as u16;
        let grid_y = (rel_idx / cols) as u16;

        let cell_rect = Rect {
            x: grid_area.x + grid_x * cell_width,
            y: grid_area.y + grid_y * cell_height,
            width: cell_width,
            height: cell_height,
        };

        let icon = if entry.is_dir { "📁" } else { "📄" };
        let icon_style = if entry.is_dir { Style::default().fg(Color::Blue) } else { Style::default() };
        let text_style = if is_selected {
            Style::default().bg(theme::ACCENT).fg(Color::Black).add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };

        // Truncate long filenames cleanly
        let mut name = entry.name.clone();
        if name.len() > (cell_width - 2) as usize {
            name.truncate((cell_width - 5) as usize);
            name.push_str("...");
        }

        let icon_line = Line::from(Span::styled(format!("  {}  ", icon), icon_style)).alignment(Alignment::Center);
        let name_line = Line::from(Span::styled(name, text_style)).alignment(Alignment::Center);

        let p = Paragraph::new(vec![Line::default(), icon_line, name_line])
            .style(if is_selected { Style::default().bg(Color::DarkGray) } else { Style::default() });

        frame.render_widget(p, cell_rect);
    }
}
