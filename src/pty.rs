//! PTY management: the `TerminalPane` data model, its lifecycle helpers, and
//! the background thread that pumps PTY output into the event channel.
//!
//! This module is the only place that touches `portable_pty` directly.
//! Everything above (AppState, the event loop) talks to PTY state through the
//! `TerminalPane` API and the `SharedParser` type alias.

use std::{
    io::{Read, Write},
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use tokio::sync::mpsc;

use crate::layout::PaneId;
use crate::{dlog, AppEvent};

// ═══════════════════════════════════════════════════════════════════════════════
// § 1  Type alias
// ═══════════════════════════════════════════════════════════════════════════════

/// A VT100 parser shared between the PTY reader thread (writer) and the render
/// thread (reader).  The `Arc<Mutex<_>>` lets both sides hold a clone without
/// lifetime coupling.
pub type SharedParser = Arc<Mutex<vt100::Parser>>;

// ═══════════════════════════════════════════════════════════════════════════════
// § 2  TerminalPane
// ═══════════════════════════════════════════════════════════════════════════════

pub struct TerminalPane {
    pub id:     PaneId,
    /// Dropping `_child` sends SIGHUP to the shell and waits — exactly what we
    /// want when a pane is removed from the HashMap.
    pub _child:  Box<dyn Child + Send + Sync>,
    pub master:  Box<dyn MasterPty + Send>,
    pub writer:  Box<dyn Write + Send>,
    pub parser:  SharedParser,
}

impl TerminalPane {
    pub fn new(
        id: PaneId,
        rows: u16,
        cols: u16,
        custom_cmd: Option<CommandBuilder>,
    ) -> Result<(Self, Box<dyn Read + Send>)> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .context("openpty")?;

        let cmd    = custom_cmd.unwrap_or_else(shell_cmd);
        let child  = pair.slave.spawn_command(cmd).context("spawn shell")?;
        let reader = pair.master.try_clone_reader().context("clone PTY reader")?;
        let writer = pair.master.take_writer().context("take PTY writer")?;
        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 0)));

        Ok((Self { id, _child: child, master: pair.master, writer, parser }, reader))
    }

    pub fn resize(&mut self, rows: u16, cols: u16) -> Result<()> {
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

        // Give the process a brief window to exit gracefully.
        std::thread::sleep(std::time::Duration::from_millis(50));

        // Force kill as a fallback if it was unresponsive.
        dlog(&format!("TerminalPane::drop killing pane_id={}", self.id));
        let _ = self._child.kill();
        dlog(&format!("TerminalPane::drop done pane_id={}", self.id));
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// § 3  Shell helper
// ═══════════════════════════════════════════════════════════════════════════════

/// Build a `CommandBuilder` for the user's preferred shell (from `$SHELL`),
/// falling back to `/bin/bash`.  Sets `TERM=xterm-256color` so programs that
/// probe terminfo work correctly inside the emulated terminal.
pub fn shell_cmd() -> CommandBuilder {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into());
    let mut cmd = CommandBuilder::new(shell);
    cmd.env("TERM", "xterm-256color");
    cmd
}

// ═══════════════════════════════════════════════════════════════════════════════
// § 4  Background PTY reader thread
// ═══════════════════════════════════════════════════════════════════════════════

/// Spawn a native OS thread that reads raw bytes from the PTY master and
/// forwards them as `AppEvent::PtyOutput` messages.
///
/// When the PTY EOF or an error is seen (the shell has exited), the thread
/// sends `AppEvent::PtyExited` with spin-retry semantics so the message is
/// never silently dropped even when the channel is momentarily full.
pub fn spawn_pane_reader(
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
