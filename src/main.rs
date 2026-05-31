//! TDE — Terminal Desktop Environment, Phase 3: Dynamic Pane Management
//!
//! ## What's new in Phase 3
//!
//! - Single-pane startup; all layout is built interactively.
//! - `Ctrl+B v` → vertical split (SplitHorizontal, left/right)
//! - `Ctrl+B s` → horizontal split (SplitVertical, top/bottom)
//! - `Ctrl+B x` → close focused pane (tree pruned, focus re-routed)
//! - Shell exit (`exit`, `Ctrl+D`) → automatic pane close, same pruning path
//! - Last pane closed → application exits gracefully
//!
//! ## Tree mutation strategy
//!
//! `LayoutNode` is a recursive owned enum.  Rust's ownership rules prevent
//! "put back what you took out of a Box" patterns, so both tree mutations
//! (split and prune) use `std::mem::replace` with a sentinel value to
//! temporarily move a node out, transform it, and put the result back — all
//! without unsafe code.
//!
//! ### Splitting
//!
//! ```text
//! split_pane(target, new_id, kind):
//!   walk tree until LayoutNode::Pane(target) found
//!   replace it with SplitHorizontal/SplitVertical {
//!     old_child: Pane(target),
//!     new_child: Pane(new_id),
//!   }
//! ```
//!
//! ### Pruning
//!
//! Each recursive call returns a `PruneResult`:
//!
//! ```text
//! enum PruneResult {
//!     NotFound,                // target wasn't in this subtree
//!     Pruned { survivor },     // target found+removed; caller uses survivor
//!                              // (None when the root itself was removed)
//! }
//! ```
//!
//! A split node delegates to each child.  If `prune(left_child)` says
//! `Pruned { survivor: Some(s) }`, the split node replaces *itself* with `s`
//! (the surviving right subtree).  If survivor is `None` the left was a
//! newly-created sentinel we used to move the node; this path never occurs in
//! practice because we only prune real `Pane` ids.
//!
//! ## Architecture (unchanged from Phase 2)
//!
//! ```text
//!  AppState { layout: LayoutNode, panes: HashMap<PaneId,TerminalPane>,
//!             focus: PaneId, mode: InputMode, next_id: PaneId }
//!
//!  AppEvent::{PtyOutput{pane_id,bytes}, PtyExited{pane_id}, Input(Event)}
//!
//!  run_event_loop()
//!   ├─ PtyOutput  → parser.process()  → draw()
//!   ├─ PtyExited  → close_pane()      → draw()  [or quit if last]
//!   └─ Input      → dispatch_input()  → draw()
//!        ├─ Normal / Ctrl+B   → PrefixActive
//!        ├─ Normal / other    → forward to focused PTY
//!        ├─ Prefix / h j k l → move_focus()
//!        ├─ Prefix / v       → split_vertical()    (left/right)
//!        ├─ Prefix / s       → split_horizontal()  (top/bottom)
//!        ├─ Prefix / x       → close_pane()
//!        └─ Prefix / q       → quit
//! ```

pub mod layout;
pub mod vfs;
pub mod pty;
pub mod input;

use std::{
    collections::HashMap,
    fs::{File, OpenOptions},
    io::{self, Read, Write},
    path::PathBuf,
    sync::{Mutex, OnceLock},
    time::Duration,
};

use layout::{
    centered_rect, centroid_x, centroid_y,
    ranges_overlap_h, ranges_overlap_v,
    Dir, LayoutNode, PaneId, SplitKind,
};
use vfs::{ExplorerEntry, ExplorerPane, spawn_dir_read};
use pty::{TerminalPane, spawn_pane_reader};
use input::dispatch_input;

// ── Debug logger ─────────────────────────────────────────────────────────────
// Writes to /tmp/tde_debug.log. We cannot use stdout (ratatui owns it) or
// eprintln! (goes to the alternate screen).
//
// The file descriptor is opened exactly once and stored in a static OnceLock.
// Every subsequent call to dlog() acquires the mutex, formats one line, and
// calls write_all() — no open()/close() syscall pair per message.
//
// IMPORTANT: main() must call std::fs::write("/tmp/tde_debug.log", "") to
// truncate the log *before* the first dlog() call initialises the OnceLock,
// otherwise the OnceLock's append-mode open would see a stale file.
static DEBUG_LOG: OnceLock<Mutex<File>> = OnceLock::new();

fn dlog(msg: &str) {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let file_mutex = DEBUG_LOG.get_or_init(|| {
        let f = OpenOptions::new()
            .create(true)
            .append(true)
            .open("/tmp/tde_debug.log")
            .expect("cannot open debug log");
        Mutex::new(f)
    });
    if let Ok(mut guard) = file_mutex.lock() {
        let _ = writeln!(*guard, "[{ts}] {msg}");
    }
}

use anyhow::{Context, Result};
use crossterm::{
    event::{
        Event, EventStream, MouseButton, MouseEventKind,
        EnableBracketedPaste, DisableBracketedPaste,
        EnableMouseCapture, DisableMouseCapture,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures::StreamExt;
use portable_pty::CommandBuilder;
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, List, ListItem, Clear},
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
// § 4  Split kind  →  see layout.rs
// ═══════════════════════════════════════════════════════════════════════════════

// ═══════════════════════════════════════════════════════════════════════════════
// § 6  TerminalPane  →  see pty.rs
// ═══════════════════════════════════════════════════════════════════════════════

// TerminalPane, SharedParser, shell_cmd, and spawn_pane_reader live in pty.rs
// and are imported above.

// ═══════════════════════════════════════════════════════════════════════════════
// § 6.5  Generic pane discriminant
// ═══════════════════════════════════════════════════════════════════════════════

// ExplorerEntry and ExplorerPane live in vfs.rs; imported above.

pub enum AppPane {
    Terminal(TerminalPane),
    Explorer(ExplorerPane),
}

// ═══════════════════════════════════════════════════════════════════════════════
// § 7  Layout tree  →  see layout.rs
// ═══════════════════════════════════════════════════════════════════════════════

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
    action: OverlayAction,
    input: String,
}

pub struct AppState {
    layout:  LayoutNode,
    panes:   HashMap<PaneId, AppPane>,
    focus:   PaneId,
    next_id: PaneId,
    overlay: Option<AppOverlay>,
}

impl AppState {
    /// Single-pane startup: one terminal filling the entire content area.
    fn new(area: Rect) -> Result<(Self, Box<dyn Read + Send>)> {
        let id: PaneId = 0;
        let rows = area.height.saturating_sub(2).max(2);
        let cols = area.width.saturating_sub(2).max(8);

        let (pane, reader) = TerminalPane::new(id, rows, cols, None)?;
        let mut panes = HashMap::new();
        panes.insert(id, AppPane::Terminal(pane));

        Ok((
                Self {
                    layout:  LayoutNode::Pane(id),
                    panes,
                    focus:   id,
                    next_id: 1,
                    overlay: None,
                },
                reader,
        ))
    }

    // ── Focus movement (unchanged from Phase 2) ──────────────────────────────

    fn move_focus(&mut self, area: Rect, dir: Dir) {
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

    /// Determines the best split direction based on the focused pane's current dimensions.
    fn smart_split_kind(&self, area: Rect, preferred: SplitKind) -> SplitKind {
        // Walk until we find the focused pane rect; stop immediately after.
        let focus_id = self.focus;
        let mut focus_rect = area;
        self.layout.walk_rects(area, &mut |id, rect| {
            if id == focus_id { focus_rect = rect; }
        });

        // Terminal fonts are usually ~2x as tall as they are wide.
        // 45 cols is very narrow. 12 rows is very short.
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
    fn do_split(
        &mut self,
        area: Rect,
        kind: SplitKind,
        cmd: Option<CommandBuilder>,
    ) -> Result<(PaneId, Box<dyn Read + Send>)> {
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

        Ok((new_id, reader))
    }

    /// Split the focused pane and spawn a File Explorer
    fn do_split_explorer(&mut self, area: Rect, kind: SplitKind, tx: mpsc::Sender<AppEvent>) -> Result<()> {
        let new_id = self.next_id;
        self.next_id += 1;

        let (rows, cols) = self.new_pane_size(area, kind);

        // Start at user's home dir or root
        let home = std::env::var("HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("/"));
        let explorer = ExplorerPane::new(new_id, home.clone());

        self.panes.insert(new_id, AppPane::Explorer(explorer));
        self.layout.split_pane(self.focus, new_id, kind);

        if let Some(AppPane::Terminal(existing)) = self.panes.get_mut(&self.focus) {
            existing.resize(rows, cols)?;
        }

        self.focus = new_id;

        // Trigger initial async read
        spawn_dir_read(new_id, home, tx);

        Ok(())
    }

    /// Compute the PTY size that a newly created pane will have after splitting
    /// the focused pane.
    fn new_pane_size(&self, area: Rect, kind: SplitKind) -> (u16, u16) {
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
    /// Returns `true` if the *last* pane was just closed (caller should quit).
    fn close_pane(&mut self, target: PaneId, area: Rect) -> Result<bool> {
        // Guard: ignore stale PtyExited events for already-removed panes.
        if !self.panes.contains_key(&target) {
            return Ok(false);
        }

        // ── Special case: only one pane left ──────────────────────────────
        if self.panes.len() == 1 {
            // Remove it and signal quit.
            self.panes.remove(&target);
            return Ok(true);
        }

        // ── Choose the pane that will receive focus after removal ─────────
        // Walk the tree exactly once to collect ordered ids; we need the
        // positional index of `target` and the list of survivors, both of
        // which fall out of a single traversal.
        let all_ids = self.layout.all_pane_ids(); // one Vec, one walk
        let target_pos = all_ids.iter().position(|id| *id == target).unwrap_or(0);
        // survivors: all ids except the one being removed
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
                .unwrap_or(positional_focus) // all survivors are explorers — accept it
        } else {
            positional_focus
        };
        dlog(&format!("close_pane: target={target} positional_focus={positional_focus} new_focus={new_focus}"));

        // ── Prune the layout tree ──────────────────────────────────────────
        self.layout.prune_pane(target);

        // ── Remove from HashMap (drops TerminalPane → drops child) ────────
        self.panes.remove(&target);

        // ── Update focus ──────────────────────────────────────────────────
        self.focus = new_focus;

        // ── Resize survivors to fit their new (expanded) rects ────────────
        self.resize_all(area)?;

        Ok(false)
    }

    // ── Resize all panes ─────────────────────────────────────────────────────

    fn resize_all(&mut self, area: Rect) -> Result<()> {
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
    /// `(x, y)`.  Returns `true` when focus actually changed (so the caller
    /// knows whether a redraw is needed).
    fn click_focus(&mut self, area: Rect, x: u16, y: u16) -> bool {
        let mut hit: Option<PaneId> = None;
        self.layout.walk_rects(area, &mut |id, rect| {
            // Check containment including the border cells.
            if x >= rect.x
                && x < rect.x + rect.width
                && y >= rect.y
                && y < rect.y + rect.height
            {
                hit = Some(id);
            }
        });
        if let Some(id) = hit {
            if id != self.focus {
                dlog(&format!("click_focus: ({x},{y}) → pane {id}"));
                self.focus = id;
                return true;
            }
        }
        false
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// § 9  Geometry helpers  →  see layout.rs
// ═══════════════════════════════════════════════════════════════════════════════

// centroid_x, centroid_y, centered_rect, ranges_overlap_v, ranges_overlap_h
// are imported from layout:: at the top of this file.

// ═══════════════════════════════════════════════════════════════════════════════
// § 10  RAII terminal guard
// ═══════════════════════════════════════════════════════════════════════════════

struct TerminalGuard;

impl TerminalGuard {
    fn new() -> Result<Self> {
        enable_raw_mode().context("enable raw mode")?;
        execute!(
            io::stdout(),
            EnterAlternateScreen,
            EnableBracketedPaste,
            EnableMouseCapture,
        ).context("enter alternate screen")?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            DisableMouseCapture,
            DisableBracketedPaste,
            LeaveAlternateScreen,
        );
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// § 11  Shell helper + § 12  Background I/O tasks  →  see pty.rs
// ═══════════════════════════════════════════════════════════════════════════════

// shell_cmd and spawn_pane_reader live in pty.rs and are imported above.

// spawn_dir_read lives in vfs.rs and is imported above.

async fn input_task(tx: mpsc::Sender<AppEvent>) {
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
        // This rescues the stream if a SIGCHLD from a killed process interrupted it.
        let raw_result = crossterm::terminal::enable_raw_mode();
        dlog(&format!("input_task: enable_raw_mode after stream error: {raw_result:?}"));
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// § 13  Key → PTY byte translation  +  § 14  Input dispatch  →  see input.rs
// ═══════════════════════════════════════════════════════════════════════════════

// key_to_bytes, forward_key, and dispatch_input live in input.rs and
// dispatch_input is imported above.

// ═══════════════════════════════════════════════════════════════════════════════
// § 15  Rendering
// ═══════════════════════════════════════════════════════════════════════════════

mod theme {
    use ratatui::style::Color;
    pub const ACCENT:          Color = Color::Cyan;
    pub const DIM_BORDER:      Color = Color::DarkGray;
    pub const TITLE_BADGE_FG:  Color = Color::Black;
    pub const TITLE_BADGE_BG:  Color = Color::Cyan;
    pub const KEY_HINT:        Color = Color::Yellow;
    pub const DIM_TEXT:        Color = Color::DarkGray;
}

fn draw(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    state: &AppState,
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
                        "  Terminal Desktop Environment",
                        Style::default().fg(theme::ACCENT),
                    ),
                    // Pane count indicator on the right side gives useful feedback.
                    Span::styled(
                        format!("  [{} pane(s)]", state.panes.len()),
                        Style::default().fg(theme::DIM_TEXT),
                    ),
            ])),
            top_area,
        );

        // ── Bottom status bar (Context-Aware) ─────────────────────────────
        let is_explorer = matches!(state.panes.get(&state.focus), Some(AppPane::Explorer(_)));
        
        let mut hints = Vec::new();
        
        // Inject Explorer-specific hints if focused
        if is_explorer {
            hints.extend(vec![
                Span::styled(" c ", Style::default().fg(theme::KEY_HINT).add_modifier(Modifier::BOLD)),
                Span::styled("create │", Style::default().fg(theme::DIM_TEXT)),
                Span::styled(" D ", Style::default().fg(theme::KEY_HINT).add_modifier(Modifier::BOLD)),
                Span::styled("delete │", Style::default().fg(theme::DIM_TEXT)),
                Span::styled(" Enter ", Style::default().fg(theme::KEY_HINT).add_modifier(Modifier::BOLD)),
                Span::styled("open │", Style::default().fg(theme::DIM_TEXT)),
            ]);
        }
        
        // Standard window management hints
        hints.extend(vec![
            Span::styled(" Alt+Space ", Style::default().fg(theme::KEY_HINT).add_modifier(Modifier::BOLD)),
            Span::styled("cmd │", Style::default().fg(theme::DIM_TEXT)),
            Span::styled(" Alt+E ", Style::default().fg(theme::KEY_HINT).add_modifier(Modifier::BOLD)),
            Span::styled("exp │", Style::default().fg(theme::DIM_TEXT)),
            Span::styled(" Alt+V/S ", Style::default().fg(theme::KEY_HINT).add_modifier(Modifier::BOLD)),
            Span::styled("split │", Style::default().fg(theme::DIM_TEXT)),
            Span::styled(" Alt+X ", Style::default().fg(theme::KEY_HINT).add_modifier(Modifier::BOLD)),
            Span::styled("close │", Style::default().fg(theme::DIM_TEXT)),
            Span::styled(" Alt+H/J/K/L ", Style::default().fg(theme::KEY_HINT).add_modifier(Modifier::BOLD)),
            Span::styled("focus │", Style::default().fg(theme::DIM_TEXT)),
            Span::styled(" Alt+Q ", Style::default().fg(theme::KEY_HINT).add_modifier(Modifier::BOLD)),
            Span::styled("quit", Style::default().fg(theme::DIM_TEXT)),
        ]);
        
        frame.render_widget(Paragraph::new(Line::from(hints)), bot_area);

        // ── Tiled panes ───────────────────────────────────────────────────
        // walk_rects drives rendering directly — no intermediate Vec.
        let focus_id = state.focus;
        state.layout.walk_rects(content_area, &mut |id, rect| {
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
                        let icon = if e.is_dir { "📁" } else { "📄" };
                        let style = if e.is_dir { Style::default().fg(Color::Blue) } else { Style::default() };
                        ListItem::new(format!(" {} {}", icon, e.name)).style(style)
                    }).collect();

                    let list = List::new(items)
                        .highlight_style(Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD))
                        .highlight_symbol(">> ");

                    // borrow_mut() gives the mutable reference ratatui needs
                    // without cloning the ListState.
                    frame.render_stateful_widget(
                        list, inner,
                        &mut *exp.list_state.borrow_mut(),
                    );
                }
            }
        });
        // ── Draw Overlay (Always on top) ──────────────────────────────────
        if let Some(overlay) = &state.overlay {
            // A small centered box: 40% width, 3 lines tall
            let mut overlay_area = centered_rect(40, 20, full);
            overlay_area.height = 3; 

            // Clear the background to prevent underlying text from bleeding through
            frame.render_widget(Clear, overlay_area);

            let title = match overlay.action {
                OverlayAction::SpawnCommand => " Run Command (Alt+Space) ",
                OverlayAction::CreateFile { .. } => " Create File/Dir (ends with / for dir) ",
            };

            let block = Block::default()
                .title(Span::styled(title, Style::default().fg(theme::ACCENT).add_modifier(Modifier::BOLD)))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme::ACCENT));

            // Render the text with a simple block cursor at the end
            let text = format!(" {}█", overlay.input);
            frame.render_widget(Paragraph::new(text).block(block), overlay_area);
        }
    })?;
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════════
// § 16  Event loop
// ═══════════════════════════════════════════════════════════════════════════════

async fn run_event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    state:    &mut AppState,
    content_area: Rect,
    rx:       &mut mpsc::Receiver<AppEvent>,
    tx:       &mpsc::Sender<AppEvent>,
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
                // Guard against output from already-removed panes, and ensure it's a terminal
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
            //
            // We treat a shell exit identically to a manual `Ctrl+B x` close.
            // This prevents "dead" panes from accumulating.
            AppEvent::PtyExited { pane_id } => {
                dlog(&format!("event_loop: PtyExited pane_id={pane_id}"));
                let should_quit = state.close_pane(pane_id, area)?;
                dlog(&format!("event_loop: after close_pane should_quit={should_quit} remaining_panes={}", state.panes.len()));
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
                        // Forward the entire string to the PTY at once
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

                    // If a split was performed, start the reader thread for
                    // the new pane immediately before the next draw.
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

// ═══════════════════════════════════════════════════════════════════════════════
// § 17  Entry point
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::main]
async fn main() -> Result<()> {
    // Truncate the debug log at startup so each run starts fresh.
    let _ = std::fs::write("/tmp/tde_debug.log", "");
    dlog("main: TDE starting");

    let (term_cols, term_rows) =
        crossterm::terminal::size().context("query terminal size")?;

    // RAII guard — terminal restored on any exit path, including panics.
    let _guard = TerminalGuard::new()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))
        .context("create ratatui terminal")?;
    terminal.clear()?;

    let content_area = Rect {
        x:      0,
        y:      1,
        width:  term_cols,
        height: term_rows.saturating_sub(2),
    };

    // Initialise with a single pane.
    let (mut state, initial_reader) =
        AppState::new(content_area).context("init app state")?;

    // Channel capacity: 512 is sufficient for bursty PTY output from many panes.
    let (tx, mut rx) = mpsc::channel::<AppEvent>(512);

    // Start the reader thread for the initial pane.
    spawn_pane_reader(0, initial_reader, tx.clone());

    // Crossterm input task.
    let tx_input = tx.clone();
    tokio::spawn(async move { input_task(tx_input).await });

    // tx is passed into the event loop so it can start new reader threads for
    // dynamically created panes.
    run_event_loop(&mut terminal, &mut state, content_area, &mut rx, &tx).await?;

    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════════
// § 18  Tests
// ═══════════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    // ── Geometry ──────────────────────────────────────────────────────────────

    #[test]
    fn centroid_math() {
        let r = Rect { x: 10, y: 20, width: 40, height: 10 };
        assert_eq!(centroid_x(r), 30);
        assert_eq!(centroid_y(r), 25);
    }

    #[test]
    fn overlap_helpers() {
        let a = Rect { x:  0, y: 0, width: 40, height: 20 };
        let b = Rect { x: 40, y: 0, width: 40, height: 20 };
        let c = Rect { x:  0, y: 20, width: 80, height: 20 };

        assert!( ranges_overlap_v(a, b), "a/b same rows → v-overlap");
        assert!(!ranges_overlap_h(a, b), "a/b different cols → no h-overlap");
        assert!( ranges_overlap_h(a, c), "a/c same cols → h-overlap");
        assert!(!ranges_overlap_v(a, c), "a/c different rows → no v-overlap");
    }

    // ── Layout tree: collect_pane_rects ───────────────────────────────────────

    #[test]
    fn collect_rects_single_pane() {
        let tree = LayoutNode::Pane(0);
        let area = Rect { x: 0, y: 0, width: 80, height: 24 };
        let mut rects = Vec::new();
        tree.walk_rects(area, &mut |id, rect| rects.push((id, rect)));
        assert_eq!(rects.len(), 1);
        assert_eq!(rects[0].0, 0);
        assert_eq!(rects[0].1, area);
    }

    #[test]
    fn collect_rects_horizontal_split() {
        let tree = LayoutNode::SplitHorizontal {
            left:  Box::new(LayoutNode::Pane(0)),
            right: Box::new(LayoutNode::Pane(1)),
            ratio: 50,
        };
        let area = Rect { x: 0, y: 0, width: 80, height: 24 };
        let mut rects = Vec::new();
        tree.walk_rects(area, &mut |id, rect| rects.push((id, rect)));
        assert_eq!(rects.len(), 2);
        // Left pane should be to the left of the right pane.
        assert!(centroid_x(rects[0].1) < centroid_x(rects[1].1));
    }

    // ── Tree mutation: split_pane ─────────────────────────────────────────────

    #[test]
    fn split_pane_leaf() {
        let mut tree = LayoutNode::Pane(0);
        let found = tree.split_pane(0, 1, SplitKind::Vertical);
        assert!(found, "should find and split pane 0");

        let ids = tree.all_pane_ids();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&0));
        assert!(ids.contains(&1));

        // Must now be a SplitHorizontal.
        assert!(matches!(tree, LayoutNode::SplitHorizontal { .. }));
    }

    #[test]
    fn split_pane_deep_target() {
        // Tree: SplitHorizontal { Pane(0), Pane(1) }
        // Split Pane(1) vertically → SplitHorizontal { Pane(0), SplitHorizontal { Pane(1), Pane(2) } }
        let mut tree = LayoutNode::SplitHorizontal {
            left:  Box::new(LayoutNode::Pane(0)),
            right: Box::new(LayoutNode::Pane(1)),
            ratio: 50,
        };
        let found = tree.split_pane(1, 2, SplitKind::Vertical);
        assert!(found);

        let ids = tree.all_pane_ids();
        assert_eq!(ids.len(), 3);
        assert!(ids.contains(&0) && ids.contains(&1) && ids.contains(&2));
    }

    #[test]
    fn split_pane_wrong_target_returns_false() {
        let mut tree = LayoutNode::Pane(0);
        let found = tree.split_pane(99, 1, SplitKind::Vertical);
        assert!(!found, "pane 99 doesn't exist");
        assert!(matches!(tree, LayoutNode::Pane(0)));
    }

    // ── Tree mutation: prune_pane ─────────────────────────────────────────────

    #[test]
    fn prune_right_child_of_split() {
        // Before: SplitHorizontal { Pane(0), Pane(1) }
        // Prune pane 1 → tree should become just Pane(0)
        let mut tree = LayoutNode::SplitHorizontal {
            left:  Box::new(LayoutNode::Pane(0)),
            right: Box::new(LayoutNode::Pane(1)),
            ratio: 50,
        };
        tree.prune_pane(1);

        let ids = tree.all_pane_ids();
        assert_eq!(ids, vec![0], "only pane 0 should survive");
        assert!(matches!(tree, LayoutNode::Pane(0)), "root should collapse to Pane(0)");
    }

    #[test]
    fn prune_left_child_of_split() {
        // Before: SplitHorizontal { Pane(0), Pane(1) }
        // Prune pane 0 → tree should become just Pane(1)
        let mut tree = LayoutNode::SplitHorizontal {
            left:  Box::new(LayoutNode::Pane(0)),
            right: Box::new(LayoutNode::Pane(1)),
            ratio: 50,
        };
        tree.prune_pane(0);

        let ids = tree.all_pane_ids();
        assert_eq!(ids, vec![1]);
        assert!(matches!(tree, LayoutNode::Pane(1)));
    }

    #[test]
    fn prune_leaf_from_deep_tree() {
        // Tree:
        //   SplitH { Pane(0), SplitV { Pane(1), Pane(2) } }
        //
        // Prune pane 2 →
        //   SplitH { Pane(0), Pane(1) }
        let mut tree = LayoutNode::SplitHorizontal {
            left: Box::new(LayoutNode::Pane(0)),
            right: Box::new(LayoutNode::SplitVertical {
                top:    Box::new(LayoutNode::Pane(1)),
                bottom: Box::new(LayoutNode::Pane(2)),
                ratio:  50,
            }),
            ratio: 50,
        };
        tree.prune_pane(2);

        let ids = tree.all_pane_ids();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&0) && ids.contains(&1));

        // Right child of the top split should now be just Pane(1), not a SplitV.
        match &tree {
            LayoutNode::SplitHorizontal { right, .. } => {
                assert!(matches!(**right, LayoutNode::Pane(1)));
            }
            _ => panic!("root should still be SplitHorizontal"),
        }
    }

    #[test]
    fn prune_nonexistent_target_is_noop() {
        let mut tree = LayoutNode::SplitHorizontal {
            left:  Box::new(LayoutNode::Pane(0)),
            right: Box::new(LayoutNode::Pane(1)),
            ratio: 50,
        };
        tree.prune_pane(99);
        assert_eq!(tree.all_pane_ids().len(), 2, "tree unchanged");
    }

    // ── all_pane_ids document order ───────────────────────────────────────────

    #[test]
    fn all_pane_ids_order() {
        //   SplitH { Pane(0), SplitV { Pane(1), Pane(2) } }
        let tree = LayoutNode::SplitHorizontal {
            left: Box::new(LayoutNode::Pane(0)),
            right: Box::new(LayoutNode::SplitVertical {
                top:    Box::new(LayoutNode::Pane(1)),
                bottom: Box::new(LayoutNode::Pane(2)),
                ratio:  50,
            }),
            ratio: 50,
        };
        assert_eq!(tree.all_pane_ids(), vec![0, 1, 2]);
    }

    // ── Focus selection after close ───────────────────────────────────────────

    #[test]
    fn close_pane_focus_selection() {
        // Build tree: [0, 1, 2] in document order; focus is on 2.
        // After closing 2, focus should move to 1 (the pane before it).
        let tree = LayoutNode::SplitHorizontal {
            left: Box::new(LayoutNode::Pane(0)),
            right: Box::new(LayoutNode::SplitVertical {
                top:    Box::new(LayoutNode::Pane(1)),
                bottom: Box::new(LayoutNode::Pane(2)),
                ratio:  50,
            }),
            ratio: 50,
        };
        let all = tree.all_pane_ids(); // [0, 1, 2]
        let target: PaneId = 2;
        let target_pos = all.iter().position(|id| *id == target).unwrap(); // 2
        let survivors: Vec<PaneId> = all.iter().copied().filter(|id| *id != target).collect();
        let new_focus = if target_pos > 0 { all[target_pos - 1] } else { survivors[0] };
        assert_eq!(new_focus, 1);
    }
}
