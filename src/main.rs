//! TDE — Terminal Desktop Environment, Phase 5: Visual Desktop Compositor
//!
//! ## Architecture
//!
//! ```text
//!  AppState { layout: LayoutNode, panes: HashMap<PaneId,AppPane>,
//!             focus: PaneId, next_id, overlay,
//!             mode: DesktopMode, floating_windows: Vec<FloatingWindow> }
//!
//!  AppEvent::{PtyOutput{pane_id,bytes}, PtyExited{pane_id},
//!             Input(Event), ExplorerUpdate{pane_id,path,entries}}
//!
//!  run_event_loop()
//!   ├─ PtyOutput      → parser.process()   → draw()
//!   ├─ PtyExited      → close_pane()       → draw()   [or quit if last]
//!   ├─ ExplorerUpdate → exp.entries = ...  → draw()
//!   └─ Input          → dispatch_input()   → draw()
//!        ├─ Alt+G  → toggle_gui_mode()         (Phase 5)
//!        ├─ Alt+V/S → do_split()
//!        ├─ Alt+X  → close_pane()
//!        ├─ Alt+E  → do_split_explorer()
//!        ├─ Alt+Arrow → move_focus()
//!        └─ Alt+Q  → quit
//!
//!  draw() branches on state.mode:
//!   ├─ DesktopMode::Tiling → layout.walk_rects() loop  (existing)
//!   └─ DesktopMode::Gui    → gui::compositor::draw_gui (Phase 5)
//! ```

// ── Module declarations ───────────────────────────────────────────────────────

pub mod app;
pub mod gui;        // ← Phase 5: Visual Desktop Compositor
pub mod input;
pub mod layout;
pub mod pty;
pub mod vfs;

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
        // 1. Disable features BEFORE leaving the alternate screen so the terminal
        //    correctly applies them to the active buffer.
        let _ = execute!(
            io::stdout(),
            DisableMouseCapture,
            DisableBracketedPaste,
            crossterm::cursor::Show,
            LeaveAlternateScreen,
        );
        
        // 2. Give the terminal emulator a tiny fraction of a second to process 
        //    the disable sequences. This guarantees any in-flight mouse movements 
        //    are swallowed before we drop raw mode and return to the shell.
        std::thread::sleep(std::time::Duration::from_millis(50));
        
        // 3. Finally, release the TTY back to the OS.
        let _ = disable_raw_mode();
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// § 17  Entry point
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::main]
async fn main() -> Result<()> {
    let _ = std::fs::write("/tmp/tde_debug.log", "");
    dlog("main: TDE starting (Phase 5 — Visual Desktop Compositor)");

    let (term_cols, term_rows) =
        crossterm::terminal::size().context("query terminal size")?;

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

    let (mut state, initial_reader) =
        AppState::new(content_area).context("init app state")?;

    let (tx, mut rx) = mpsc::channel::<AppEvent>(512);

    spawn_pane_reader(0, initial_reader, tx.clone());

    let tx_input = tx.clone();
    tokio::spawn(async move { input_task(tx_input).await });

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
    use crate::gui::window::cascade_rect;
    use ratatui::layout::Rect;

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
        assert!( ranges_overlap_v(a, b));
        assert!(!ranges_overlap_h(a, b));
        assert!( ranges_overlap_h(a, c));
        assert!(!ranges_overlap_v(a, c));
    }

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
        assert!(centroid_x(rects[0].1) < centroid_x(rects[1].1));
    }

    #[test]
    fn split_pane_leaf() {
        let mut tree = LayoutNode::Pane(0);
        assert!(tree.split_pane(0, 1, SplitKind::Vertical));
        let ids = tree.all_pane_ids();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&0) && ids.contains(&1));
        assert!(matches!(tree, LayoutNode::SplitHorizontal { .. }));
    }

    #[test]
    fn split_pane_deep_target() {
        let mut tree = LayoutNode::SplitHorizontal {
            left:  Box::new(LayoutNode::Pane(0)),
            right: Box::new(LayoutNode::Pane(1)),
            ratio: 50,
        };
        assert!(tree.split_pane(1, 2, SplitKind::Vertical));
        let ids = tree.all_pane_ids();
        assert_eq!(ids.len(), 3);
        assert!(ids.contains(&0) && ids.contains(&1) && ids.contains(&2));
    }

    #[test]
    fn split_pane_wrong_target_returns_false() {
        let mut tree = LayoutNode::Pane(0);
        assert!(!tree.split_pane(99, 1, SplitKind::Vertical));
        assert!(matches!(tree, LayoutNode::Pane(0)));
    }

    #[test]
    fn prune_right_child_of_split() {
        let mut tree = LayoutNode::SplitHorizontal {
            left:  Box::new(LayoutNode::Pane(0)),
            right: Box::new(LayoutNode::Pane(1)),
            ratio: 50,
        };
        tree.prune_pane(1);
        assert_eq!(tree.all_pane_ids(), vec![0]);
        assert!(matches!(tree, LayoutNode::Pane(0)));
    }

    #[test]
    fn prune_left_child_of_split() {
        let mut tree = LayoutNode::SplitHorizontal {
            left:  Box::new(LayoutNode::Pane(0)),
            right: Box::new(LayoutNode::Pane(1)),
            ratio: 50,
        };
        tree.prune_pane(0);
        assert_eq!(tree.all_pane_ids(), vec![1]);
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
        assert_eq!(tree.all_pane_ids().len(), 2);
    }

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
        let all = tree.all_pane_ids();
        let target: PaneId = 2;
        let target_pos = all.iter().position(|id| *id == target).unwrap();
        let survivors: Vec<PaneId> = all.iter().copied().filter(|id| *id != target).collect();
        let new_focus = if target_pos > 0 { all[target_pos - 1] } else { survivors[0] };
        assert_eq!(new_focus, 1);
    }

    // ── GUI: cascade_rect geometry ────────────────────────────────────────────

    #[test]
    fn cascade_rect_window_zero_is_centred() {
        let screen = Rect { x: 0, y: 0, width: 200, height: 50 };
        let r = cascade_rect(0, screen);
        assert_eq!(r.width,  120, "60% of 200");
        assert_eq!(r.height, 30,  "60% of 50");
        assert!(r.x + r.width  <= screen.x + screen.width,  "overflows right");
        assert!(r.y + r.height <= screen.y + screen.height, "overflows bottom");
    }

    #[test]
    fn cascade_rect_never_overflows() {
        let screen = Rect { x: 0, y: 1, width: 120, height: 40 };
        for n in 0..20u16 {
            let r = cascade_rect(n, screen);
            assert!(
                r.x + r.width  <= screen.x + screen.width,
                "window {n} overflows right: {r:?}"
            );
            assert!(
                r.y + r.height <= screen.y + screen.height,
                "window {n} overflows bottom: {r:?}"
            );
        }
    }

    #[test]
    fn cascade_rect_successive_windows_differ() {
        let screen = Rect { x: 0, y: 0, width: 200, height: 60 };
        let r0 = cascade_rect(0, screen);
        let r1 = cascade_rect(1, screen);
        assert!(r0.x != r1.x || r0.y != r1.y, "consecutive windows overlap perfectly");
    }

    // ── HARDCORE STRESS TESTS & EDGE CASES ────────────────────────────────────

    #[test]
    fn prune_zigzag_stress_test() {
        // Deep unbalanced tree: [0 | [1 / [2 | 3]]]
        // We delete 2. The split [2 | 3] must perfectly collapse to just Pane 3.
        // The tree should become: [0 | [1 / 3]]
        let mut tree = LayoutNode::SplitHorizontal {
            left: Box::new(LayoutNode::Pane(0)),
            right: Box::new(LayoutNode::SplitVertical {
                top: Box::new(LayoutNode::Pane(1)),
                bottom: Box::new(LayoutNode::SplitHorizontal {
                    left: Box::new(LayoutNode::Pane(2)),
                    right: Box::new(LayoutNode::Pane(3)),
                    ratio: 50,
                }),
                ratio: 50,
            }),
            ratio: 50,
        };

        tree.prune_pane(2);

        // Verify the remaining structure
        let ids = tree.all_pane_ids();
        assert_eq!(ids, vec![0, 1, 3], "Tree did not retain the correct survivors");

        // Deep structural verify
        match tree {
            LayoutNode::SplitHorizontal { right, .. } => {
                match *right {
                    LayoutNode::SplitVertical { bottom, .. } => {
                        assert!(
                            matches!(*bottom, LayoutNode::Pane(3)), 
                            "CRITICAL: Failed to cleanly collapse the SplitHorizontal into Pane(3)"
                        );
                    }
                    _ => panic!("Expected SplitVertical on the right"),
                }
            }
            _ => panic!("Expected SplitHorizontal at the root"),
        }
    }

    #[test]
    fn cascade_rect_tiny_terminal_violation() {
        // ASSUMPTION: The terminal is always a standard size.
        // What happens if the user resizes their terminal to be extremely tiny, 
        // or connects via a tiny mobile SSH client?
        let screen = Rect { x: 0, y: 0, width: 15, height: 5 };
        let r = cascade_rect(0, screen);

        // `cascade_rect` currently tries to enforce a minimum width of 20 and height of 6.
        // If it forces these minimums, the window will exceed the screen boundary.
        // When Ratatui tries to draw outside the screen, or draw borders on a crushed Rect, it PANICS.
        assert!(
            r.width <= screen.width,
            "CRITICAL FLAW: Floating window width ({}) exceeds screen width ({}). Ratatui will panic!",
            r.width, screen.width
        );
        assert!(
            r.height <= screen.height,
            "CRITICAL FLAW: Floating window height ({}) exceeds screen height ({}). Ratatui will panic!",
            r.height, screen.height
        );
    }

    #[test]
    fn tiling_crushed_rect_violation() {
        use crate::app;
        let screen = Rect { x: 0, y: 0, width: 80, height: 24 };
        let (mut state, _) = app::AppState::new(screen).unwrap();

        // Attempt to violently spawn 20 vertical panes on a small screen
        for _ in 1..20 {
            let _ = state.do_split(screen, SplitKind::Vertical, None).unwrap();
        }

        let mut min_width = 80;
        state.layout.walk_rects(screen, &mut |_id, rect| {
            if rect.width < min_width {
                min_width = rect.width;
            }
        });

        // The safety limiter should have stepped in and refused splits long before width < 2
        assert!(
            min_width >= 2,
            "CRITICAL FLAW: Tiling engine crushed a pane to width {}. Block::inner() will panic!",
            min_width
        );
    }
}
