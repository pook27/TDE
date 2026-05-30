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

use std::{
    collections::HashMap,
    io::{self, Read, Write},
    mem,
    sync::{Arc, Mutex},
    time::Duration,
    path::PathBuf,
    fs::OpenOptions,
};

// ── Debug logger ─────────────────────────────────────────────────────────────
// Writes to /tmp/tde_debug.log. We cannot use stdout (ratatui owns it) or
// eprintln! (goes to the alternate screen). Open the file once per call to
// avoid needing a global mutex — performance doesn't matter for debug logging.
fn dlog(msg: &str) {
    use std::io::Write as _;
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    if let Ok(mut f) = OpenOptions::new()
        .create(true).append(true).open("/tmp/tde_debug.log")
    {
        let _ = writeln!(f, "[{ts}] {msg}");
    }
}

use anyhow::{Context, Result};
use crossterm::{
    event::{Event, EventStream, KeyCode, KeyEvent, KeyModifiers, MouseEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures::StreamExt;
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, List, ListItem, ListState, Clear},
    Terminal,
};
use tokio::sync::mpsc;
use tui_term::widget::PseudoTerminal;

// ═══════════════════════════════════════════════════════════════════════════════
// § 1  Primitive types
// ═══════════════════════════════════════════════════════════════════════════════

type PaneId = u32;
type SharedParser = Arc<Mutex<vt100::Parser>>;

// ═══════════════════════════════════════════════════════════════════════════════
// § 2  Events
// ═══════════════════════════════════════════════════════════════════════════════

enum AppEvent {
    PtyOutput { pane_id: PaneId, bytes: Vec<u8> },
    PtyExited { pane_id: PaneId },
    Input(Event),
    ExplorerUpdate { pane_id: PaneId, path: PathBuf, entries: Vec<ExplorerEntry> },
}

// ═══════════════════════════════════════════════════════════════════════════════
// § 3  Direction
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Clone, Copy, Debug)]
enum Dir {
    Left,
    Right,
    Up,
    Down,
}

// ═══════════════════════════════════════════════════════════════════════════════
// § 4  Split kind
// ═══════════════════════════════════════════════════════════════════════════════

/// Which axis to split on.
#[derive(Clone, Copy, Debug)]
enum SplitKind {
    /// `v` key: split left/right → SplitHorizontal; new pane on the right.
    Vertical,
    /// `s` key: split top/bottom → SplitVertical; new pane on the bottom.
    Horizontal,
}

// ═══════════════════════════════════════════════════════════════════════════════
// § 6  TerminalPane
// ═══════════════════════════════════════════════════════════════════════════════

struct TerminalPane {
    id: PaneId,
    /// Dropping `_child` sends SIGHUP to the shell and waits — exactly what we
    /// want when a pane is removed from the HashMap.
    _child: Box<dyn Child + Send + Sync>,
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    parser: SharedParser,
}

impl TerminalPane {
    fn new(id: PaneId, rows: u16, cols: u16, custom_cmd: Option<CommandBuilder>) -> Result<(Self, Box<dyn Read + Send>)> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .context("openpty")?;

        let cmd = custom_cmd.unwrap_or_else(shell_cmd);
        let child  = pair.slave.spawn_command(cmd).context("spawn shell")?;
        let reader = pair.master.try_clone_reader().context("clone PTY reader")?;
        let writer = pair.master.take_writer().context("take PTY writer")?;
        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 0)));

        Ok((Self { id, _child: child, master: pair.master, writer, parser }, reader))
    }

    fn resize(&mut self, rows: u16, cols: u16) -> Result<()> {
        self.master
            .resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .context("resize PTY")?;
        let mut g = self.parser.lock().expect("parser poisoned");
        *g = vt100::Parser::new(rows, cols, 0);
        Ok(())
    }
}

impl Drop for TerminalPane {
    fn drop(&mut self) {
        dlog(&format!("TerminalPane::drop pane_id={}", self.id));
        // Attempt a clean exit first. We inject an Escape key followed by the 
        // standard Vim force-quit command. This allows the child process to 
        // close its own file descriptors cleanly, avoiding OS-level PTY panics.
        let _ = self.writer.write_all(b"\x1b:qa!\r");
        let _ = self.writer.flush();

        // Give the process a brief window to exit gracefully
        std::thread::sleep(std::time::Duration::from_millis(50));

        // Force kill as a fallback if it was unresponsive
        dlog(&format!("TerminalPane::drop killing pane_id={}", self.id));
        let _ = self._child.kill();
        dlog(&format!("TerminalPane::drop done pane_id={}", self.id));
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// § 6.5  Explorer & Generic Panes
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Clone)]
struct ExplorerEntry {
    name: String,
    is_dir: bool,
}

struct ExplorerPane {
    id: PaneId,
    cwd: PathBuf,
    entries: Vec<ExplorerEntry>,
    list_state: ListState,
}

impl ExplorerPane {
    fn new(id: PaneId, path: PathBuf) -> Self {
        let mut list_state = ListState::default();
        list_state.select(Some(0));
        Self {
            id,
            cwd: path,
            entries: Vec::new(),
            list_state,
        }
    }
}

enum AppPane {
    Terminal(TerminalPane),
    Explorer(ExplorerPane),
}

// ═══════════════════════════════════════════════════════════════════════════════
// § 7  Layout tree
// ═══════════════════════════════════════════════════════════════════════════════

/// Binary tiling tree.  Leaves hold a `PaneId`; interior nodes hold children.
///
/// The `Sentinel` variant exists solely as a `mem::replace` placeholder that is
/// never visible to any other code path — it is always immediately overwritten.
/// It is not `pub` and contains no data, so it imposes zero cost.
enum LayoutNode {
    Pane(PaneId),
    SplitHorizontal { left: Box<LayoutNode>, right: Box<LayoutNode>, ratio: u16 },
    SplitVertical   { top:  Box<LayoutNode>, bottom: Box<LayoutNode>, ratio: u16 },
    /// Private placeholder used during tree mutation only.  Must never persist.
    Sentinel,
}

// ── Return type for the recursive prune helper ────────────────────────────────

/// Result of attempting to remove `target` from a subtree.
enum PruneResult {
    /// Target was not found in this subtree; the caller need not change anything.
    NotFound,
    /// Target was found and removed.  `survivor` is the node that should
    /// replace the one that was pruned:
    ///
    /// - `Some(node)` → replace the pruned node (or its parent split) with this.
    /// - `None`       → the node that was pruned *was* a `Sentinel` placeholder;
    ///                  this never happens for real pane ids.
    Pruned { survivor: Option<Box<LayoutNode>> },
}

impl LayoutNode {
    // ── Layout computation ───────────────────────────────────────────────────

    fn collect_pane_rects(&self, area: Rect, out: &mut Vec<(PaneId, Rect)>) {
        match self {
            LayoutNode::Pane(id) => out.push((*id, area)),

            LayoutNode::SplitHorizontal { left, right, ratio } => {
                let chunks = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([
                        Constraint::Percentage(*ratio),
                        Constraint::Percentage(100 - ratio),
                    ])
                    .split(area);
                left.collect_pane_rects(chunks[0], out);
                right.collect_pane_rects(chunks[1], out);
            }

            LayoutNode::SplitVertical { top, bottom, ratio } => {
                let chunks = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([
                        Constraint::Percentage(*ratio),
                        Constraint::Percentage(100 - ratio),
                    ])
                    .split(area);
                top.collect_pane_rects(chunks[0], out);
                bottom.collect_pane_rects(chunks[1], out);
            }

            LayoutNode::Sentinel => {} // unreachable in normal operation
        }
    }

    // ── Tree mutation: split ──────────────────────────────────────────────────

    /// Walk the tree until `LayoutNode::Pane(target)` is found, then replace
    /// it with a new split node containing both the old pane and `new_id`.
    ///
    /// Returns `true` if the target was found and the split performed.
    ///
    /// ## mem::replace strategy
    ///
    /// We cannot move out of `self` when `self` is `&mut LayoutNode` (it's
    /// behind a reference).  `mem::replace` lets us swap in a `Sentinel`
    /// placeholder, take ownership of the old value, build the new split node
    /// from it, then write the new node back into `*self`.  The Sentinel is
    /// never observable outside this function because we always overwrite it
    /// before returning.
    fn split_pane(&mut self, target: PaneId, new_id: PaneId, kind: SplitKind) -> bool {
        match self {
            // ── Leaf: is this the target? ─────────────────────────────────
            LayoutNode::Pane(id) if *id == target => {
                // Swap self out, leaving a Sentinel in place temporarily.
                let old = mem::replace(self, LayoutNode::Sentinel);
                // Build the replacement split node.
                *self = match kind {
                    SplitKind::Vertical => LayoutNode::SplitHorizontal {
                        left:  Box::new(old),
                        right: Box::new(LayoutNode::Pane(new_id)),
                        ratio: 50,
                    },
                    SplitKind::Horizontal => LayoutNode::SplitVertical {
                        top:    Box::new(old),
                        bottom: Box::new(LayoutNode::Pane(new_id)),
                        ratio:  50,
                    },
                };
                true
            }

            // ── Leaf: wrong target ────────────────────────────────────────
            LayoutNode::Pane(_) => false,

            // ── Interior: delegate to children ───────────────────────────
            LayoutNode::SplitHorizontal { left, right, .. } => {
                left.split_pane(target, new_id, kind)
                    || right.split_pane(target, new_id, kind)
            }
            LayoutNode::SplitVertical { top, bottom, .. } => {
                top.split_pane(target, new_id, kind)
                    || bottom.split_pane(target, new_id, kind)
            }

            LayoutNode::Sentinel => false, // unreachable
        }
    }

    // ── Tree mutation: prune ──────────────────────────────────────────────────

    /// Recursively remove `LayoutNode::Pane(target)` from the tree.
    ///
    /// The caller is responsible for replacing *its own* reference to `self`
    /// with the `survivor` returned by `PruneResult::Pruned`.
    ///
    /// ## How split-node replacement works
    ///
    /// When a split finds that one of its children was pruned, it needs to
    /// replace *itself* with the surviving sibling.  Because `prune_pane` takes
    /// `&mut self`, we again use `mem::replace`:
    ///
    /// 1. Swap `self` → `Sentinel`.
    /// 2. Destructure the old split value (now owned).
    /// 3. Write `*self = *surviving_sibling` (or keep the updated split).
    fn prune_pane(&mut self, target: PaneId) -> PruneResult {
        match self {
            // ── Leaf: is this the target? ─────────────────────────────────
            LayoutNode::Pane(id) if *id == target => {
                // Signal to the caller: replace me with my sibling.
                // We do NOT modify `self` here; the caller (a split arm below,
                // or AppState::prune) will overwrite us.
                PruneResult::Pruned { survivor: None }
            }

            LayoutNode::Pane(_) => PruneResult::NotFound,

            // ── SplitHorizontal ───────────────────────────────────────────
            LayoutNode::SplitHorizontal { left, right, .. } => {
                // Try left child first.
                match left.prune_pane(target) {
                    PruneResult::Pruned { survivor: None } => {
                        // Left leaf was the target.  Replace this whole split
                        // with the right child.
                        let old_split = mem::replace(self, LayoutNode::Sentinel);
                        let right_child = match old_split {
                            LayoutNode::SplitHorizontal { right, .. } => right,
                            _ => unreachable!(),
                        };
                        *self = *right_child;
                        PruneResult::Pruned { survivor: None }
                        // Returning None tells *our* parent: replace your
                        // reference to us with us (we've already updated *self).
                        // The parent's job is already done.
                    }

                    PruneResult::Pruned { survivor: Some(new_left) } => {
                        // A deeper node was pruned; left sub-tree was replaced.
                        *left = new_left;
                        // This split node itself is unchanged in structure.
                        PruneResult::Pruned { survivor: None }
                    }

                    PruneResult::NotFound => {
                        // Try right child.
                        match right.prune_pane(target) {
                            PruneResult::Pruned { survivor: None } => {
                                // Right leaf was the target.  Replace this split
                                // with the left child.
                                let old_split = mem::replace(self, LayoutNode::Sentinel);
                                let left_child = match old_split {
                                    LayoutNode::SplitHorizontal { left, .. } => left,
                                    _ => unreachable!(),
                                };
                                *self = *left_child;
                                PruneResult::Pruned { survivor: None }
                            }

                            PruneResult::Pruned { survivor: Some(new_right) } => {
                                *right = new_right;
                                PruneResult::Pruned { survivor: None }
                            }

                            PruneResult::NotFound => PruneResult::NotFound,
                        }
                    }
                }
            }

            // ── SplitVertical (mirror of SplitHorizontal) ─────────────────
            LayoutNode::SplitVertical { top, bottom, .. } => {
                match top.prune_pane(target) {
                    PruneResult::Pruned { survivor: None } => {
                        let old_split = mem::replace(self, LayoutNode::Sentinel);
                        let bottom_child = match old_split {
                            LayoutNode::SplitVertical { bottom, .. } => bottom,
                            _ => unreachable!(),
                        };
                        *self = *bottom_child;
                        PruneResult::Pruned { survivor: None }
                    }

                    PruneResult::Pruned { survivor: Some(new_top) } => {
                        *top = new_top;
                        PruneResult::Pruned { survivor: None }
                    }

                    PruneResult::NotFound => {
                        match bottom.prune_pane(target) {
                            PruneResult::Pruned { survivor: None } => {
                                let old_split = mem::replace(self, LayoutNode::Sentinel);
                                let top_child = match old_split {
                                    LayoutNode::SplitVertical { top, .. } => top,
                                    _ => unreachable!(),
                                };
                                *self = *top_child;
                                PruneResult::Pruned { survivor: None }
                            }

                            PruneResult::Pruned { survivor: Some(new_bottom) } => {
                                *bottom = new_bottom;
                                PruneResult::Pruned { survivor: None }
                            }

                            PruneResult::NotFound => PruneResult::NotFound,
                        }
                    }
                }
            }

            LayoutNode::Sentinel => PruneResult::NotFound,
        }
    }

    /// Return all PaneIds present in this subtree, in document order.
    fn all_pane_ids(&self) -> Vec<PaneId> {
        let mut out = Vec::new();
        self.collect_ids(&mut out);
        out
    }

    fn collect_ids(&self, out: &mut Vec<PaneId>) {
        match self {
            LayoutNode::Pane(id)                           => out.push(*id),
            LayoutNode::SplitHorizontal { left, right, .. } => {
                left.collect_ids(out);
                right.collect_ids(out);
            }
            LayoutNode::SplitVertical { top, bottom, .. }  => {
                top.collect_ids(out);
                bottom.collect_ids(out);
            }
            LayoutNode::Sentinel => {}
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// § 8  AppState
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Clone)]
enum OverlayAction {
    /// Alt+Space to spawn a custom command
    SpawnCommand,
    /// 'c' in Explorer to create a file/directory
    CreateFile { cwd: PathBuf },
}

struct AppOverlay {
    action: OverlayAction,
    input: String,
}

struct AppState {
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
        let mut rects = Vec::new();
        self.layout.collect_pane_rects(area, &mut rects);

        let cur = match rects.iter().find(|(id, _)| *id == self.focus) {
            Some((_, r)) => *r,
            None         => return,
        };
        let cx = centroid_x(cur);
        let cy = centroid_y(cur);

        let mut best: Option<(PaneId, i32)> = None;

        for (id, rect) in &rects {
            if *id == self.focus { continue; }
            let rx = centroid_x(*rect);
            let ry = centroid_y(*rect);

            let ok = match dir {
                Dir::Left  => rx < cx && ranges_overlap_v(cur, *rect),
                Dir::Right => rx > cx && ranges_overlap_v(cur, *rect),
                Dir::Up    => ry < cy && ranges_overlap_h(cur, *rect),
                Dir::Down  => ry > cy && ranges_overlap_h(cur, *rect),
            };
            if !ok { continue; }

            let dist = match dir {
                Dir::Left | Dir::Right => (rx - cx).abs(),
                Dir::Up   | Dir::Down  => (ry - cy).abs(),
            };
            match best {
                None                       => best = Some((*id, dist)),
                Some((_, bd)) if dist < bd => best = Some((*id, dist)),
                _                          => {}
            }
        }

        if let Some((new_focus, _)) = best {
            self.focus = new_focus;
        }
    }

    // ── Smart split heuristic ────────────────────────────────────────────────

    /// Determines the best split direction based on the focused pane's current dimensions.
    fn smart_split_kind(&self, area: Rect, preferred: SplitKind) -> SplitKind {
        let mut rects = Vec::new();
        self.layout.collect_pane_rects(area, &mut rects);
        let focus_rect = rects
            .iter()
            .find(|(id, _)| *id == self.focus)
            .map(|(_, r)| *r)
            .unwrap_or(area);

        // Terminal fonts are usually ~2x as tall as they are wide. 
        // 45 cols is very narrow. 12 rows is very short.
        match preferred {
            SplitKind::Vertical => {
                // Prefers left/right split. Override if we are too narrow but have height.
                if focus_rect.width < 45 && focus_rect.height >= 12 {
                    SplitKind::Horizontal
                } else {
                    SplitKind::Vertical
                }
            }
            SplitKind::Horizontal => {
                // Prefers top/bottom split. Override if we are too short but have width.
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
        let mut rects = Vec::new();
        self.layout.collect_pane_rects(area, &mut rects);

        let focus_rect = rects
            .iter()
            .find(|(id, _)| *id == self.focus)
            .map(|(_, r)| *r)
            .unwrap_or(area);

        match kind {
            SplitKind::Vertical => {
                // New pane will be the right half.
                let cols = (focus_rect.width / 2).saturating_sub(2).max(8);
                let rows = focus_rect.height.saturating_sub(2).max(2);
                (rows, cols)
            }
            SplitKind::Horizontal => {
                // New pane will be the bottom half.
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
        // We pick from surviving panes using document order; prefer the pane
        // immediately before the target, falling back to the one after.
        // Among all candidates, prefer a Terminal pane over an Explorer pane
        // so that closing a pane never silently strands focus on the explorer
        // (which swallows all regular keypresses).
        let survivors: Vec<PaneId> = self.layout
            .all_pane_ids()
            .into_iter()
            .filter(|id| *id != target)
            .collect();

        // Pick the last survivor that comes before target in document order,
        // or the first overall.
        let all_ids = self.layout.all_pane_ids();
        let target_pos = all_ids.iter().position(|id| *id == target).unwrap_or(0);
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
        let mut rects = Vec::new();
        self.layout.collect_pane_rects(area, &mut rects);

        for (id, rect) in rects {
            let rows = rect.height.saturating_sub(2).max(2);
            let cols = rect.width.saturating_sub(2).max(8);
            if let Some(AppPane::Terminal(term)) = self.panes.get_mut(&id) {
                term.resize(rows, cols)?;
            }
        }
        Ok(())
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// § 9  Geometry helpers
// ═══════════════════════════════════════════════════════════════════════════════

fn centroid_x(r: Rect) -> i32 { (r.x as i32) + (r.width  as i32) / 2 }
fn centroid_y(r: Rect) -> i32 { (r.y as i32) + (r.height as i32) / 2 }

/// Helper to create a centered bounding box for floating overlays
fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

fn ranges_overlap_v(a: Rect, b: Rect) -> bool {
    (a.y as i32) < (b.y + b.height) as i32
        && (b.y as i32) < (a.y + a.height) as i32
}
fn ranges_overlap_h(a: Rect, b: Rect) -> bool {
    (a.x as i32) < (b.x + b.width) as i32
        && (b.x as i32) < (a.x + a.width) as i32
}

// ═══════════════════════════════════════════════════════════════════════════════
// § 10  RAII terminal guard
// ═══════════════════════════════════════════════════════════════════════════════

struct TerminalGuard;

impl TerminalGuard {
    fn new() -> Result<Self> {
        enable_raw_mode().context("enable raw mode")?;
        execute!(io::stdout(), EnterAlternateScreen).context("enter alternate screen")?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// § 11  Shell helper
// ═══════════════════════════════════════════════════════════════════════════════

fn shell_cmd() -> CommandBuilder {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into());
    let mut cmd = CommandBuilder::new(shell);
    cmd.env("TERM", "xterm-256color");
    cmd
}

// ═══════════════════════════════════════════════════════════════════════════════
// § 12  Background I/O tasks
// ═══════════════════════════════════════════════════════════════════════════════

fn spawn_pane_reader(
    pane_id: PaneId,
    mut reader: Box<dyn Read + Send>,
    tx: mpsc::Sender<AppEvent>,
) {
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => {
                    // Keep retrying the send for PtyExited — it is a small,
                    // infrequent message and we must not silently drop it.
                    // If the channel is full, spin-wait briefly rather than
                    // blocking indefinitely with blocking_send on a saturated
                    // channel (which would hold the tx clone alive forever).
                    loop {
                        match tx.try_send(AppEvent::PtyExited { pane_id }) {
                            Ok(_) => break,
                            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                                std::thread::sleep(std::time::Duration::from_millis(5));
                            }
                            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => break,
                        }
                    }
                    break;
                }
                Ok(n) => {
                    // PTY output: prefer non-blocking. If the channel is full,
                    // drop this frame — the terminal will simply not update for
                    // one read cycle, which is far better than stalling the
                    // reader thread and blocking the PTY pipe.
                    let _ = tx.try_send(AppEvent::PtyOutput {
                        pane_id,
                        bytes: buf[..n].to_vec(),
                    });
                }
            }
        }
    });
}

fn spawn_dir_read(pane_id: PaneId, path: PathBuf, tx: mpsc::Sender<AppEvent>) {
    tokio::spawn(async move {
        let mut entries = Vec::new();

        // Always add a way to go up a directory, unless we are at root
        if path.parent().is_some() {
            entries.push(ExplorerEntry { name: "..".to_string(), is_dir: true });
        }

        if let Ok(mut read_dir) = tokio::fs::read_dir(&path).await {
            while let Ok(Some(entry)) = read_dir.next_entry().await {
                let name = entry.file_name().to_string_lossy().to_string();
                let is_dir = entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false);
                entries.push(ExplorerEntry { name, is_dir });
            }
        }

        // Sort: Directories first, then alphabetical
        entries.sort_by(|a, b| {
            b.is_dir.cmp(&a.is_dir).then_with(|| a.name.cmp(&b.name))
        });

        let _ = tx.send(AppEvent::ExplorerUpdate { pane_id, path, entries }).await;
    });
}

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
// § 13  Key → PTY byte translation (unchanged from Phase 1)
// ═══════════════════════════════════════════════════════════════════════════════

fn key_to_bytes(key: KeyEvent) -> Option<Vec<u8>> {
    let b = match key.code {
        KeyCode::Char(c) if key.modifiers.contains(KeyModifiers::CONTROL) => {
            vec![(c.to_ascii_lowercase() as u8).wrapping_sub(b'a').wrapping_add(1) & 0x1f]
        }
        KeyCode::Char(c)   => c.to_string().into_bytes(),
        KeyCode::Enter     => vec![b'\r'],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Delete    => vec![0x1b, b'[', b'3', b'~'],
        KeyCode::Tab       => vec![b'\t'],
        KeyCode::Esc       => vec![0x1b],
        KeyCode::Up        => vec![0x1b, b'[', b'A'],
        KeyCode::Down      => vec![0x1b, b'[', b'B'],
        KeyCode::Right     => vec![0x1b, b'[', b'C'],
        KeyCode::Left      => vec![0x1b, b'[', b'D'],
        KeyCode::PageUp    => vec![0x1b, b'[', b'5', b'~'],
        KeyCode::PageDown  => vec![0x1b, b'[', b'6', b'~'],
        KeyCode::Home      => vec![0x1b, b'[', b'H'],
        KeyCode::End       => vec![0x1b, b'[', b'F'],
        KeyCode::F(1)      => vec![0x1b, b'O', b'P'],
        KeyCode::F(2)      => vec![0x1b, b'O', b'Q'],
        KeyCode::F(3)      => vec![0x1b, b'O', b'R'],
        KeyCode::F(4)      => vec![0x1b, b'O', b'S'],
        KeyCode::F(5)      => vec![0x1b, b'[', b'1', b'5', b'~'],
        KeyCode::F(6)      => vec![0x1b, b'[', b'1', b'7', b'~'],
        KeyCode::F(7)      => vec![0x1b, b'[', b'1', b'8', b'~'],
        KeyCode::F(8)      => vec![0x1b, b'[', b'1', b'9', b'~'],
        KeyCode::F(9)      => vec![0x1b, b'[', b'2', b'0', b'~'],
        KeyCode::F(10)     => vec![0x1b, b'[', b'2', b'1', b'~'],
        KeyCode::F(11)     => vec![0x1b, b'[', b'2', b'3', b'~'],
        KeyCode::F(12)     => vec![0x1b, b'[', b'2', b'4', b'~'],
        _ => return None,
    };
    Some(b)
}

fn forward_key(key: KeyEvent, writer: &mut Box<dyn Write + Send>) -> Result<()> {
    if let Some(bytes) = key_to_bytes(key) {
        writer.write_all(&bytes).context("write to PTY")?;
        writer.flush().context("flush PTY")?;
    }
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════════
// § 14  Input dispatch
// ═══════════════════════════════════════════════════════════════════════════════

/// Process one key event.
///
/// Returns `(should_quit, Option<(PaneId, reader)>)`:
/// - `should_quit` → event loop must break.
/// - `Some((id, reader))` → a new pane was just created; caller must spawn its
///   reader thread before the next draw call.
fn dispatch_input(
    state: &mut AppState,
    area: Rect,
    key: KeyEvent,
    tx: &mpsc::Sender<AppEvent>, // Prefix unused with an underscore since we don't need it right now
) -> Result<(bool, Option<(PaneId, Box<dyn Read + Send>)>)> {

    dlog(&format!(
            "dispatch_input: key={:?} modifiers={:?} focus={} panes={:?}",
            key.code,
            key.modifiers,
            state.focus,
            state.panes.keys().collect::<Vec<_>>()
    ));

    // ── 1. Intercept input if Overlay is active ──────────────────────────────
    if let Some(mut overlay) = state.overlay.take() {
        match key.code {
            KeyCode::Esc => {
                // Cancel overlay (state.overlay remains None)
            }
            KeyCode::Backspace => {
                overlay.input.pop();
                state.overlay = Some(overlay);
            }
            KeyCode::Char(c) => {
                overlay.input.push(c);
                state.overlay = Some(overlay);
            }
            KeyCode::Enter => {
                // Execute the overlay action!
                match overlay.action {
                    OverlayAction::SpawnCommand => {
                        if !overlay.input.trim().is_empty() {
                            // Basic command parsing (splits by space)
                            let mut parts = overlay.input.trim().split_whitespace();
                            let mut cmd = CommandBuilder::new(parts.next().unwrap());
                            for arg in parts { cmd.arg(arg); }
                            cmd.env("TERM", "xterm-256color");

                            // Smart split: prefers vertical, but flips to horizontal if cramped
                            let kind = state.smart_split_kind(area, SplitKind::Vertical);
                            let (new_id, reader) = state.do_split(area, kind, Some(cmd))?;
                            return Ok((false, Some((new_id, reader))));
                        }
                    }
                    OverlayAction::CreateFile { cwd } => {
                        if !overlay.input.trim().is_empty() {
                            let mut path = cwd.clone();
                            path.push(overlay.input.trim());

                            // If it ends with '/', make a directory. Otherwise, make an empty file.
                            if overlay.input.ends_with('/') {
                                let _ = std::fs::create_dir_all(&path);
                            } else {
                                let _ = std::fs::File::create(&path);
                            }

                            // Trigger an async refresh of the focused Explorer pane
                            if let Some(AppPane::Explorer(exp)) = state.panes.get(&state.focus) {
                                spawn_dir_read(exp.id, exp.cwd.clone(), tx.clone());
                            }
                        }
                    }
                }
            }
            _ => {
                // Ignore other keys but keep overlay open
                state.overlay = Some(overlay);
            }
        }
        return Ok((false, None));
    }

    // Check if the ALT modifier is pressed
    if key.modifiers.contains(KeyModifiers::ALT) {
        match key.code {
            // ── Quit ──────────────────────────────────────────────────
            KeyCode::Char('q') => return Ok((true, None)),

            // ── Focus movement ────────────────────────────────────────
            KeyCode::Char('h') => state.move_focus(area, Dir::Left),
            KeyCode::Char('l') => state.move_focus(area, Dir::Right),
            KeyCode::Char('k') => state.move_focus(area, Dir::Up),
            KeyCode::Char('j') => state.move_focus(area, Dir::Down),

            // ── Split vertical (left / right) ─────────────────────────
            KeyCode::Char('v') => {
                let (new_id, reader) = state.do_split(area, SplitKind::Vertical, None)?;
                return Ok((false, Some((new_id, reader))));
            }

            // ── Split horizontal (top / bottom) ───────────────────────
            KeyCode::Char('s') => {
                let (new_id, reader) = state.do_split(area, SplitKind::Horizontal, None)?;
                return Ok((false, Some((new_id, reader))));
            }

            // ── Close focused pane ────────────────────────────────────
            KeyCode::Char('x') => {
                let target = state.focus;
                let should_quit = state.close_pane(target, area)?;
                return Ok((should_quit, None));
            }

            // ── Split Explorer ──────────────────────────────────────────────
           KeyCode::Char('e') => {
                // Smart split: prefers vertical, but flips to horizontal if cramped
                let kind = state.smart_split_kind(area, SplitKind::Vertical);
                state.do_split_explorer(area, kind, tx.clone())?;
                return Ok((false, None)); // tx handles async data
            }

            // ── Command Bar Overlay ───────────────────────────────────────
            KeyCode::Char(' ') => { // Alt + Space
                state.overlay = Some(AppOverlay {
                    action: OverlayAction::SpawnCommand,
                    input: String::new(),
                });
                return Ok((false, None));
            }

            // Ignore other Alt bindings
            _ => {}
        }
    } else {
        // We defer actions that mutate the layout to avoid borrow checker conflicts
        // since we are currently holding a mut borrow on state.panes.
        let mut deferred_action = None;

        if let Some(pane) = state.panes.get_mut(&state.focus) {
            match pane {
                AppPane::Terminal(term) => {
                    dlog(&format!("dispatch_input: forwarding key to pane {}", term.id));
                    let result = forward_key(key, &mut term.writer);
                    dlog(&format!("dispatch_input: forward_key result: {result:?}"));
                    result?;
                }
                AppPane::Explorer(exp) => {
                    match key.code {
                        KeyCode::Char('j') | KeyCode::Down => {
                            let i = exp.list_state.selected().unwrap_or(0);
                            if i < exp.entries.len().saturating_sub(1) { exp.list_state.select(Some(i + 1)); }
                        }
                        KeyCode::Char('k') | KeyCode::Up => {
                            let i = exp.list_state.selected().unwrap_or(0);
                            if i > 0 { exp.list_state.select(Some(i - 1)); }
                        }
                        KeyCode::Char('D') => { // shift + 'd' to delete
                            if let Some(i) = exp.list_state.selected() {
                                if let Some(entry) = exp.entries.get(i) {
                                    if entry.name != ".." {
                                        let mut path = exp.cwd.clone();
                                        path.push(&entry.name);
                                        deferred_action = Some(("delete", path));
                                    }
                                }
                            }
                        }
                        KeyCode::Char('c') => { // 'c' to create
                            state.overlay = Some(AppOverlay {
                                action: OverlayAction::CreateFile { cwd: exp.cwd.clone() },
                                input: String::new(),
                            });
                        }
                        KeyCode::Enter => {
                            if let Some(i) = exp.list_state.selected() {
                                if let Some(entry) = exp.entries.get(i) {
                                    let mut path = exp.cwd.clone();
                                    if entry.name == ".." {
                                        path.pop();
                                        spawn_dir_read(exp.id, path, tx.clone());
                                    } else {
                                        path.push(&entry.name);
                                        if entry.is_dir {
                                            spawn_dir_read(exp.id, path, tx.clone());
                                        } else {
                                            // It's a file, trigger nvim open
                                            deferred_action = Some(("open", path));
                                        }
                                    }
                                }
                            }
                        }
                        _ => {
                            dlog(&format!("dispatch_input: explorer pane swallowed key {:?} (not a handled binding)", key.code));
                        }
                    }
                }
            }
        }

        if let Some((action, path)) = deferred_action {
            if action == "open" {
                let mut cmd = CommandBuilder::new("nvim");
                cmd.arg(path.to_string_lossy().as_ref());
                cmd.env("TERM", "xterm-256color");

                // Split vertically and open nvim
                let (new_id, reader) = state.do_split(area, SplitKind::Vertical, Some(cmd))?;
                return Ok((false, Some((new_id, reader))));

            } else if action == "delete" {
                if path.is_dir() {
                    let _ = std::fs::remove_dir_all(&path);
                } else {
                    let _ = std::fs::remove_file(&path);
                }

                // Refresh the current explorer pane
                if let Some(AppPane::Explorer(exp)) = state.panes.get(&state.focus) {
                    spawn_dir_read(exp.id, exp.cwd.clone(), tx.clone());
                }
            }
        }
    }
    Ok((false, None))
}

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
    pub const PREFIX_FG:       Color = Color::Black;
    pub const PREFIX_BG:       Color = Color::Yellow;
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
        let mut pane_rects = Vec::new();
        state.layout.collect_pane_rects(content_area, &mut pane_rects);

        for (id, rect) in pane_rects {
            let Some(pane) = state.panes.get(&id) else { continue };
            let focused = id == state.focus;

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

                    // We need a mutable reference to state to render the selection highlight
                    let mut state = exp.list_state.clone(); 
                    frame.render_stateful_widget(list, inner, &mut state);
                }
            }
        }
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
                    exp.list_state.select(Some(0)); // Reset selection to top
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
                dlog(&format!("event_loop: after close_pane should_quit={should_quit} remaining_panes={:?}", state.panes.keys().collect::<Vec<_>>()));
                if should_quit {
                    break;
                }
                draw(terminal, state)?;
            }

            // ── Input events ──────────────────────────────────────────────
            AppEvent::Input(ev) => match ev {
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
        tree.collect_pane_rects(area, &mut rects);
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
        tree.collect_pane_rects(area, &mut rects);
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
