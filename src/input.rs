//! Keyboard input handling: VT-byte translation, PTY forwarding, and the main
//! input dispatcher that routes key events to the correct pane or global action.
//!
//! `dispatch_input` is the single entry-point called by the event loop.  It
//! mutates `AppState` in-place and returns a `(should_quit, Option<new_pane>)`
//! pair so the event loop can act on splits and quits without holding a borrow
//! on the state.

use std::io::{Read, Write};

use anyhow::{Context, Result};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use portable_pty::CommandBuilder;
use ratatui::layout::Rect;
use tokio::sync::mpsc;

use crate::layout::{Dir, PaneId, SplitKind};
use crate::vfs::spawn_dir_read;
use crate::app::{AppEvent, AppOverlay, AppPane, AppState, OverlayAction};
use crate::dlog;

// ═══════════════════════════════════════════════════════════════════════════════
// § 1  Key → PTY byte translation
// ═══════════════════════════════════════════════════════════════════════════════

/// Map a crossterm `KeyEvent` to the raw byte sequence a VT100 terminal would
/// send for that key.  Returns `None` for keys with no defined VT mapping
/// (e.g. bare modifier keys, unrecognised F-keys).
pub fn key_to_bytes(key: KeyEvent) -> Option<Vec<u8>> {
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

// ═══════════════════════════════════════════════════════════════════════════════
// § 2  PTY forwarding
// ═══════════════════════════════════════════════════════════════════════════════

/// Translate `key` to bytes and write them directly into a PTY writer.
/// No-ops cleanly if the key has no VT mapping.
pub fn forward_key(key: KeyEvent, writer: &mut Box<dyn Write + Send>) -> Result<()> {
    if let Some(bytes) = key_to_bytes(key) {
        writer.write_all(&bytes).context("write to PTY")?;
        writer.flush().context("flush PTY")?;
    }
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════════
// § 3  Input dispatcher
// ═══════════════════════════════════════════════════════════════════════════════

/// Process one key event.
///
/// Returns `(should_quit, Option<(PaneId, reader)>)`:
/// - `should_quit` → event loop must break.
/// - `Some((id, reader))` → a new pane was just created; caller must spawn its
///   reader thread before the next draw call.
pub fn dispatch_input(
    state: &mut AppState,
    area:  Rect,
    key:   KeyEvent,
    tx:    &mpsc::Sender<AppEvent>,
) -> Result<(bool, Option<(PaneId, Box<dyn Read + Send>)>)> {

    dlog(&format!(
        "dispatch_input: key={:?} modifiers={:?} focus={} n_panes={}",
        key.code, key.modifiers, state.focus, state.panes.len()
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

    // ── 2. Alt-key global bindings ────────────────────────────────────────────
    if key.modifiers.contains(KeyModifiers::ALT) {
        match key.code {
            // ── Quit ──────────────────────────────────────────────────────
            KeyCode::Char('q') => return Ok((true, None)),

            // ── Focus movement ────────────────────────────────────────────
            KeyCode::Char('h') => state.move_focus(area, Dir::Left),
            KeyCode::Char('l') => state.move_focus(area, Dir::Right),
            KeyCode::Char('k') => state.move_focus(area, Dir::Up),
            KeyCode::Char('j') => state.move_focus(area, Dir::Down),

            // ── Split vertical (left / right) ──────────────────────────────
            KeyCode::Char('v') => {
                let (new_id, reader) = state.do_split(area, SplitKind::Vertical, None)?;
                return Ok((false, Some((new_id, reader))));
            }

            // ── Split horizontal (top / bottom) ────────────────────────────
            KeyCode::Char('s') => {
                let (new_id, reader) = state.do_split(area, SplitKind::Horizontal, None)?;
                return Ok((false, Some((new_id, reader))));
            }

            // ── Close focused pane ─────────────────────────────────────────
            KeyCode::Char('x') => {
                let target = state.focus;
                let should_quit = state.close_pane(target, area)?;
                return Ok((should_quit, None));
            }

            // ── Split Explorer ─────────────────────────────────────────────
            KeyCode::Char('e') => {
                // Smart split: prefers vertical, but flips to horizontal if cramped
                let kind = state.smart_split_kind(area, SplitKind::Vertical);
                state.do_split_explorer(area, kind, tx.clone())?;
                return Ok((false, None)); // tx handles async data
            }

            // ── Command Bar Overlay ────────────────────────────────────────
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
        // ── 3. Normal mode: forward to focused pane ───────────────────────────
        // We defer actions that mutate the layout to avoid borrow checker
        // conflicts while holding a mut borrow on state.panes.
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
                            let i = exp.list_state.borrow().selected().unwrap_or(0);
                            if i < exp.entries.len().saturating_sub(1) {
                                exp.list_state.borrow_mut().select(Some(i + 1));
                            }
                        }
                        KeyCode::Char('k') | KeyCode::Up => {
                            let i = exp.list_state.borrow().selected().unwrap_or(0);
                            if i > 0 {
                                exp.list_state.borrow_mut().select(Some(i - 1));
                            }
                        }
                        KeyCode::Char('D') => { // Shift+D to delete
                            if let Some(i) = exp.list_state.borrow().selected() {
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
                            if let Some(i) = exp.list_state.borrow().selected() {
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
                                            // It's a file — open it in nvim
                                            deferred_action = Some(("open", path));
                                        }
                                    }
                                }
                            }
                        }
                        _ => {
                            dlog(&format!(
                                "dispatch_input: explorer pane swallowed key {:?} (not a handled binding)",
                                key.code
                            ));
                        }
                    }
                }
            }
        }

        // ── 4. Deferred layout mutations ──────────────────────────────────────
        if let Some((action, path)) = deferred_action {
            if action == "open" {
                let mut cmd = CommandBuilder::new("nvim");
                cmd.arg(path.as_os_str());
                cmd.env("TERM", "xterm-256color");

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
