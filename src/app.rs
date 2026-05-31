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
    centered_rect, centroid_x, centroid_y,
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

        // ── The Safety Limiter ──
        // Refuse to split if the focused pane is already too small
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

        // Mutate the tree: replace Pane(focus) with Split{Pane(focus), Pane(new)}.
        self.layout.split_pane(self.focus, new_id, kind);

        // Also resize the *existing* focused pane to its new (halved) rect.
        if let Some(AppPane::Terminal(existing_term)) = self.panes.get_mut(&self.focus) {
            existing_term.resize(rows, cols)?;
        }

        // Shift focus to the newly created pane.
        self.focus = new_id;

        Ok(Some((new_id, reader)))
    }

    /// Split the focused pane and spawn a File Explorer.
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

        // ── The Safety Limiter ──
        match kind {
            SplitKind::Vertical if focus_rect.width < 12 => return Ok(()),
            SplitKind::Horizontal if focus_rect.height < 6 => return Ok(()),
            _ => {}
        }

        let new_id = self.next_id;
        self.next_id += 1;

        let (rows, cols) = self.new_pane_size(area, kind);

        let home = std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/"));
        let explorer = ExplorerPane::new(new_id, home.clone());

        self.panes.insert(new_id, AppPane::Explorer(explorer));
        self.layout.split_pane(self.focus, new_id, kind);

        if let Some(AppPane::Terminal(existing)) = self.panes.get_mut(&self.focus) {
            existing.resize(rows, cols)?;
        }

        self.focus = new_id;

        // Trigger initial async read.
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
        // Collect (id, rows, cols) first so we don't hold a borrow on
        // self.layout while mutably borrowing self.panes.
        let mut to_resize: Vec<(PaneId, u16, u16)> = Vec::new();
        self.layout.walk_rects(area, &mut |id, rect| {
            let rows = rect.height.saturating_sub(2).max(2);
            let cols = rect.width.saturating_sub(2).max(8);
            to_resize.push((id, rows, cols));
        });
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
                    // Check if the (x, y) coordinates fall within this pane's rectangle
                    if x >= rect.x && x < rect.x + rect.width && y >= rect.y && y < rect.y + rect.height {
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
                // The topmost window that contains the click wins.
                let hit = self.floating_windows.iter().rev().find(|w| {
                    x >= w.area.x
                        && x < w.area.x + w.area.width
                        && y >= w.area.y
                        && y < w.area.y + w.area.height
                });
                if let Some(win) = hit {
                    let id = win.id;
                    if id != self.focus {
                        dlog(&format!("click_focus/gui: ({x},{y}) → pane {id}"));
                        self.focus = id;
                        return true;
                    }
                }
                false
            }
        }
    }

    // ── GUI mode toggle ───────────────────────────────────────────────────────

    /// Toggle between `DesktopMode::Tiling` and `DesktopMode::Gui`.
    ///
    /// ## Cascade population
    ///
    /// When switching **to** `DesktopMode::Gui`, if `floating_windows` is
    /// empty we populate it with one `FloatingWindow` per existing pane using
    /// the `cascade_rect` geometry helper.  The pane ids come from
    /// `layout.all_pane_ids()` so their order matches the left-to-right,
    /// top-to-bottom document order of the tiling tree.
    ///
    /// We do **not** repopulate if `floating_windows` is already non-empty —
    /// that preserves any window positions the user may have arranged.
    ///
    /// `content_area` is the content rect (full screen minus chrome bars) and
    /// is forwarded to `cascade_rect` for geometry computation.
    pub fn toggle_gui_mode(&mut self, content_area: Rect) {
        match self.mode {
            DesktopMode::Tiling => {
                self.mode = DesktopMode::Gui;

                // Populate the window stack if it has never been built.
                if self.floating_windows.is_empty() {
                    let ids = self.layout.all_pane_ids();
                    for (n, id) in ids.into_iter().enumerate() {
                        let rect = cascade_rect(n as u16, content_area);
                        self.floating_windows.push(FloatingWindow::new(id, rect));
                        dlog(&format!(
                            "toggle_gui: cascade window {n} → pane {id} at {:?}",
                            rect
                        ));
                    }
                }
            }

            DesktopMode::Gui => {
                self.mode = DesktopMode::Tiling;
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
                        // Receiver dropped — event loop has exited; stop entirely.
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
        // Brief pause before recreating the stream to avoid a tight spin loop
        // in the unlikely event of a persistent error condition.
        tokio::time::sleep(Duration::from_millis(50)).await;
        // Force crossterm to re-sync its internal state with the OS terminal.
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

        // ── Bottom status bar (Context-Aware) ─────────────────────────────
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

        // ── Content area: branch on desktop mode ──────────────────────────
        //
        // Both branches receive `content_area` — the rect between the two
        // chrome bars.  Neither branch touches `top_area` or `bot_area`.
        match state.mode {
            // ── Tiling: existing zero-allocation walk_rects loop ──────────
            DesktopMode::Tiling => {
                let focus_id = state.focus;
                state.layout.walk_rects(content_area, &mut |id, rect| {
                    if rect.width < 2 || rect.height < 2 { return; }

                    let Some(pane) = state.panes.get(&id) else { return };
                    let focused = id == focus_id;

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

                    match pane {
                        AppPane::Terminal(term) => {
                            let guard = term.parser.lock().expect("parser poisoned");
                            frame.render_widget(PseudoTerminal::new(guard.screen()), inner);
                        }
                        AppPane::Explorer(exp) => {
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
                });
            }

            // ── GUI: floating compositor ──────────────────────────────────
            DesktopMode::Gui => {
                draw_gui(frame, state, content_area);
            }
        }

        // ── Draw Overlay (Always on top, both modes) ──────────────────────
        if let Some(overlay) = &state.overlay {
            let mut overlay_area = centered_rect(40, 20, full);
            overlay_area.height = 3;

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
        let event = tokio::select! {
            ev = rx.recv() => match ev {
                Some(e) => e,
                None    => break,
            },
            _ = tokio::time::sleep(Duration::from_millis(500)) => continue,
        };

        match event {
            // ── PTY output ────────────────────────────────────────────────
            AppEvent::PtyOutput { pane_id, bytes } => {
                if let Some(AppPane::Terminal(term)) = state.panes.get(&pane_id) {
                    term.parser.lock().expect("parser poisoned").process(&bytes);
                }
                draw(terminal, state)?;
            }

            AppEvent::ExplorerUpdate { pane_id, path, entries } => {
                if let Some(AppPane::Explorer(exp)) = state.panes.get_mut(&pane_id) {
                    exp.cwd = path;
                    exp.entries = entries;
                    exp.list_state.borrow_mut().select(Some(0));
                }
                draw(terminal, state)?;
            }

            // ── Shell exited → automatic pane close ───────────────────────
            AppEvent::PtyExited { pane_id } => {
                dlog(&format!("event_loop: PtyExited pane_id={pane_id}"));
                let should_quit = state.close_pane(pane_id, area)?;
                dlog(&format!(
                    "event_loop: after close_pane should_quit={should_quit} remaining_panes={}",
                    state.panes.len()
                ));
                if should_quit {
                    break;
                }
                draw(terminal, state)?;
            }

            // ── Input events ──────────────────────────────────────────────
            AppEvent::Input(ev) => match ev {

                // Fast-path for pasted text (prevents rendering 300 frames for 300 characters)
                Event::Paste(text) => {
                    if let Some(AppPane::Terminal(term)) = state.panes.get_mut(&state.focus) {
                        let _ = term.writer.write_all(text.as_bytes());
                        let _ = term.writer.flush();
                    }
                    draw(terminal, state)?;
                }

                Event::Key(key_ev) => {
                    let (should_quit, new_pane) =
                        dispatch_input(state, area, key_ev, tx)?;

                    if should_quit {
                        break;
                    }

                    if let Some((new_id, reader)) = new_pane {
                        spawn_pane_reader(new_id, reader, tx.clone());
                    }

                    draw(terminal, state)?;
                }

                Event::Resize(new_cols, new_rows) => {
                    area = Rect {
                        x:      0,
                        y:      1,
                        width:  new_cols,
                        height: new_rows.saturating_sub(2),
                    };
                    state.resize_all(area)?;
                    draw(terminal, state)?;
                }

                Event::Mouse(m) if matches!(m.kind, MouseEventKind::Moved) => {}

                // ── Left-click: spatial focus ──────────────────────────────
                Event::Mouse(m)
                    if matches!(m.kind, MouseEventKind::Down(MouseButton::Left)) =>
                {
                    if state.click_focus(area, m.column, m.row) {
                        draw(terminal, state)?;
                    }
                }

                // ── Scroll wheel: Explorer navigation ─────────────────────
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
                                    draw(terminal, state)?;
                                }
                            }
                            MouseEventKind::ScrollDown => {
                                if i < exp.entries.len().saturating_sub(1) {
                                    exp.list_state.borrow_mut().select(Some(i + 1));
                                    draw(terminal, state)?;
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
    }
    Ok(())
}
