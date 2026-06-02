//! PTY management: the `TerminalPane` data model, its lifecycle helpers, and
//! the background thread that pumps PTY output into the event channel.
//!
//! This module is the only place that touches `portable_pty` directly.
//! Everything above (AppState, the event loop) talks to PTY state through the
//! `TerminalPane` API and the `SharedParser` type alias.

use std::{
    cell::Cell,
    io::{Read, Write},
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use tokio::sync::mpsc;

use crate::layout::PaneId;
use crate::app::AppEvent;
use crate::dlog;

// ═══════════════════════════════════════════════════════════════════════════════
// § 1  Type alias
// ═══════════════════════════════════════════════════════════════════════════════

/// A VT100 parser shared between the PTY reader thread (writer) and the render
/// thread (reader).  The `Arc<Mutex<_>>` lets both sides hold a clone without
/// lifetime coupling.
pub type SharedParser = Arc<Mutex<vt100::Parser>>;

// ═══════════════════════════════════════════════════════════════════════════════
// § 2  Process-name cache
// ═══════════════════════════════════════════════════════════════════════════════

/// Cheap, single-owner cache for the foreground process name.
///
/// Stored inline on `TerminalPane` (not heap-allocated separately).  Uses
/// `Cell` so the cache can be updated through a shared `&self` reference
/// inside `foreground_process_name` without requiring `&mut self` — matching
/// how the render path holds an immutable reference to the pane.
///
/// Layout:
/// - `last_pid`  — the PTY-leader PID last seen; `u32::MAX` = uninitialised.
/// - `last_name` — the cached comm string for that PID.
///
/// `last_name` is a `String` (heap) but it is only re-allocated when the
/// foreground process actually changes, which is infrequent relative to the
/// 60 fps render cadence.  The fast-path (same PID) returns a clone of the
/// cached value without touching the filesystem at all.
struct ProcNameCache {
    /// PID of the process whose name is in `last_name`, or `u32::MAX` if
    /// uninitialised.
    last_pid:  Cell<u32>,
    /// Interior-mutable string so we can update via `&self`.
    last_name: std::cell::RefCell<String>,
}

impl ProcNameCache {
    fn new() -> Self {
        Self {
            last_pid:  Cell::new(u32::MAX),
            last_name: std::cell::RefCell::new(String::new()),
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// § 3  TerminalPane
// ═══════════════════════════════════════════════════════════════════════════════

pub struct TerminalPane {
    pub id:        PaneId,
    pub _child:    Box<dyn Child + Send + Sync>,
    pub master:    Box<dyn MasterPty + Send>,
    pub writer:    Box<dyn Write + Send>,
    pub parser:    SharedParser,
    pub custom_command: Option<String>,
    pub is_custom: bool,
    pub is_dead:   bool,
    proc_cache:    ProcNameCache,
}

impl TerminalPane {
    pub fn new(
        id: PaneId,
        rows: u16,
        cols: u16,
        custom_command: Option<String>,
    ) -> Result<(Self, Box<dyn Read + Send>)> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .context("openpty")?;

        let is_custom = custom_command.is_some();
        
        // Parse the raw command string into a CommandBuilder
        let cmd = if let Some(ref cmd_str) = custom_command {
            let mut parts = cmd_str.trim().split_whitespace();
            if let Some(bin) = parts.next() {
                let mut c = CommandBuilder::new(bin);
                for arg in parts { c.arg(arg); }
                c.env("TERM", "xterm-256color");
                c
            } else {
                shell_cmd()
            }
        } else {
            shell_cmd()
        };

        let child     = pair.slave.spawn_command(cmd).context("spawn shell")?;
        let reader    = pair.master.try_clone_reader().context("clone PTY reader")?;
        let writer    = pair.master.take_writer().context("take PTY writer")?;
        let parser    = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 0)));

        Ok((
            Self {
                id,
                _child:     child,
                master:     pair.master,
                writer,
                parser,
                custom_command,
                is_custom,
                is_dead:    false,
                proc_cache: ProcNameCache::new(),
            },
            reader,
        ))
    }

    // ── Non-destructive resize ────────────────────────────────────────────────

    /// Resize the underlying PTY kernel buffer **and** the VT100 parser.
    ///
    /// Unlike the previous implementation which replaced the parser with a
    /// brand-new `vt100::Parser::new(rows, cols, 0)` (destroying all
    /// scrollback and the current screen state), this calls
    /// `screen_mut().set_size()` so that existing cell contents, scrollback,
    /// and cursor state are preserved across resize events.
    pub fn resize(&mut self, rows: u16, cols: u16) -> Result<()> {
        self.master
            .resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .context("resize PTY")?;

        let mut g = self.parser.lock().expect("parser poisoned");
        // `set_size` on `Screen` is non-destructive — it reflowes the existing
        // content to the new dimensions rather than wiping it.
        g.set_size(rows, cols);
        Ok(())
    }

    // ── Foreground process name ───────────────────────────────────────────────

    /// Return the name of the process currently running in the foreground of
    /// this PTY's process group.
    ///
    /// ## Performance contract
    ///
    /// This is called from the 60 fps taskbar render path, so it **must not**
    /// allocate on the common (idle shell) case.  The strategy:
    ///
    /// 1. Ask the PTY master for the current foreground process group leader's
    ///    PID via `process_group_leader()`.  This is a single `ioctl` — cheap.
    /// 2. Compare that PID to `proc_cache.last_pid`.
    ///    - **Same PID** → return a clone of `proc_cache.last_name`.  One heap
    ///      allocation (the returned `String`), no filesystem access.
    ///    - **Different / missing PID** → read the first ~32 bytes of
    ///      `/proc/<pid>/stat`, parse the `(comm)` field, store it in the
    ///      cache, and return a clone.  This path is taken only when the
    ///      foreground command actually changes, which is rare.
    ///
    /// Graceful fallbacks:
    /// - `process_group_leader()` returns `None` → fall back to the shell name
    ///   derived from `_child.process_id()` → `/proc/<pid>/stat`.
    /// - Any filesystem error → return a static-like default derived from the
    ///   shell binary name.
    pub fn foreground_process_name(&self) -> String {
        // ── Step 1: resolve the best available PID ────────────────────────
        //
        // Prefer the PTY foreground-process-group leader because that is the
        // currently running foreground command (e.g. `nvim`, `top`).  When
        // the PTY is idle at a shell prompt the leader *is* the shell itself,
        // which is exactly what we want to display.
        //
        // Fall back to the child's own PID when `process_group_leader`
        // returns `None` (can happen on some platforms or in edge cases).
        let pid: u32 = match self.master.process_group_leader() {
            Some(pgid) if pgid > 0 => pgid as u32,
            _ => match self._child.process_id() {
                Some(pid) => pid,
                None      => return self.shell_basename(),
            },
        };

        // ── Step 2: cache hit → zero filesystem access ────────────────────
        if self.proc_cache.last_pid.get() == pid {
            return self.proc_cache.last_name.borrow().clone();
        }

        // ── Step 3: cache miss → read /proc/<pid>/stat ────────────────────
        let name = read_proc_comm(pid).unwrap_or_else(|| self.shell_basename());

        // Update the cache.
        self.proc_cache.last_pid.set(pid);
        *self.proc_cache.last_name.borrow_mut() = name.clone();

        name
    }

    /// Derive the shell's basename from the `$SHELL` environment variable as a
    /// last-resort fallback.  Allocates once; result is typically 4–6 bytes.
    #[inline(never)]   // keep the fast path lean; this is only called on error
    fn shell_basename(&self) -> String {
        std::env::var("SHELL")
            .ok()
            .and_then(|s| {
                // Extract the basename: everything after the last '/'.
                s.rfind('/').map(|i| s[i + 1..].to_owned())
            })
            .unwrap_or_else(|| "bash".to_owned())
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// § 4  /proc helper (Linux-only, safe on non-Linux)
// ═══════════════════════════════════════════════════════════════════════════════

/// Read `/proc/<pid>/stat` and extract the comm name (field 2, the value
/// between the first `(` and the matching last `)` in the file).
///
/// We read only the first 64 bytes of the file.  The Linux kernel writes the
/// comm field truncated to 15 characters (TASK_COMM_LEN − 1) so the `(comm)`
/// portion is at most 17 bytes; the leading `"<pid> "` is at most 10 bytes.
/// 64 bytes therefore always covers the entire field with room to spare, and
/// avoids reading the rest of the (potentially long) stat line.
///
/// Returns `None` on any I/O error or parse failure so callers can use a
/// graceful fallback.
fn read_proc_comm(pid: u32) -> Option<String> {
    // Stack-allocate the path string: "/proc/" (6) + up to 10 digits + "/stat" (5) + NUL = 22.
    // We use a small fixed-size array rather than `format!` to avoid heap allocation.
    let mut path_buf = [0u8; 32];
    let path_str = {
        use std::io::Write as _;
        let mut cursor = std::io::Cursor::new(&mut path_buf[..]);
        write!(cursor, "/proc/{}/stat", pid).ok()?;
        let len = cursor.position() as usize;
        std::str::from_utf8(&path_buf[..len]).ok()?
    };

    // Open and read only the first 64 bytes.  `std::fs::File` is fine here —
    // we are on a dedicated background read; this is NOT called from the async
    // executor thread.
    let mut file = std::fs::File::open(path_str).ok()?;
    let mut stat_buf = [0u8; 64];
    let n = Read::read(&mut file, &mut stat_buf).ok()?;
    let stat_slice = &stat_buf[..n];

    // Find the comm field: first '(' … last ')'.
    // Using `last` for ')' correctly handles the rare case where the comm
    // string itself contains a ')' character (e.g. a process named "a)b").
    let open  = stat_slice.iter().position(|&b| b == b'(')?;
    let close = stat_slice.iter().rposition(|&b| b == b')')?;

    if close <= open {
        return None;
    }

    // Safety: /proc/stat comms are kernel-generated ASCII; invalid UTF-8 is
    // theoretically possible with exotic comm names so we use from_utf8_lossy.
    let comm = std::str::from_utf8(&stat_slice[open + 1..close])
        .ok()
        .filter(|s| !s.is_empty())
        .map(|s| s.to_owned())?;

    Some(comm)
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
// § 5  Shell helper
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
// § 6  Background PTY reader thread
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
