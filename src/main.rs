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
//! ## Architecture
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
//! ```

// ── Module declarations ───────────────────────────────────────────────────────

pub mod app;
pub mod layout;
pub mod vfs;
pub mod pty;
pub mod input;

// ── Imports ───────────────────────────────────────────────────────────────────

use std::{
    fs::{File, OpenOptions},
    io::{self, Write},
    sync::{Mutex, OnceLock},
};

use anyhow::{Context, Result};
use crossterm::{
    event::{
        EnableBracketedPaste, DisableBracketedPaste,
        EnableMouseCapture,  DisableMouseCapture,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use tokio::sync::mpsc;

use app::{AppEvent, AppState, input_task, run_event_loop};
use pty::spawn_pane_reader;

// ── Debug logger ──────────────────────────────────────────────────────────────
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

pub fn dlog(msg: &str) {
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

    let content_area = ratatui::layout::Rect {
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
    use crate::layout::{
        centroid_x, centroid_y, ranges_overlap_h, ranges_overlap_v,
        LayoutNode, PaneId, SplitKind,
    };
    use ratatui::layout::Rect;

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

        assert!(matches!(tree, LayoutNode::SplitHorizontal { .. }));
    }

    #[test]
    fn split_pane_deep_target() {
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
        let target_pos = all.iter().position(|id| *id == target).unwrap();
        let survivors: Vec<PaneId> = all.iter().copied().filter(|id| *id != target).collect();
        let new_focus = if target_pos > 0 { all[target_pos - 1] } else { survivors[0] };
        assert_eq!(new_focus, 1);
    }
}
