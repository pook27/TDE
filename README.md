# TDE - Terminal Desktop Environment

A keyboard-centric, dual-mode (tiling and floating) TUI desktop environment for headless/SSH machines, built in Rust with `ratatui`, `crossterm`, `portable-pty`, and `vt100`.

---

## Quick Start

```bash
# Requires Rust 1.75+ (stable)
cargo build --release
./target/release/tde

```

Or run in development mode:

```bash
cargo run

```

---

## Input & Navigation

TDE uses direct `Alt` (Meta) keybindings for zero-latency, stateless navigation, alongside spatial mouse support over SSH.

### Global Shortcuts

| Key | Action |
| --- | --- |
| `Alt+Space` | Open Command Bar Overlay (type app name to launch) |
| `Alt+E` | Open Virtual File Explorer (splits screen) |
| `Alt+V` | Vertical Split (side-by-side) |
| `Alt+S` | Horizontal Split (top-and-bottom) |
| `Alt+X` | Close focused pane |
| `Alt+Arrow Keys` | Move focus (Left/Down/Up/Right) |
| `Alt+G` | Toggle between Tiling and GUI (Floating) Desktop Mode |
| `Alt+Q` | Quit TDE |

### Explorer Shortcuts (When focused on File Explorer)

| Key | Action |
| --- | --- |
| `Up / Down` | Navigate files |
| `Enter` | Open file in `nvim` / Enter directory |
| `c` | Create new file (add `/` to end for directory) |
| `Shift+D` | Delete file/directory |

### Mouse Support

| Action | Result |
| --- | --- |
| `Left Click` | Hit-tests the layout tree, instantly shifts focus, and raises floating windows to the front |
| `Click & Drag` | Moves floating windows around the screen (GUI mode only) |
| `Scroll Wheel` | Navigates up/down through lists (e.g., within the File Explorer) |

All other keystrokes are forwarded verbatim to the active child shell or application. Bracketed Paste is fully supported for instantaneous, single-frame block pasting over SSH.

---

## Architecture

TDE is heavily optimized for zero-allocation render loops and low-latency network constraints via event batching and channel draining. The codebase is strictly modularized by domain:

* `src/main.rs` - Application setup, RAII TerminalGuard, and Tokio async entry point.
* `src/app.rs` - Core `AppState`, `AppEvent` MPSC loop, channel draining, and the `ratatui` rendering pass.
* `src/layout.rs` - `LayoutNode` binary tree, zero-allocation closure traversals (`walk_rects`), and geometry math.
* `src/gui/` - Visual compositor (`draw_gui`) and `FloatingWindow` data structures.
* `src/pty.rs` - `TerminalPane` data model, child process/shell lifecycle, and background PTY reader threads.
* `src/input.rs` - Keystroke routing (`dispatch_input`) and VT100 byte translation.
* `src/vfs.rs` - `ExplorerPane` data model and asynchronous directory reading tasks.

## Dependency Notes

| Crate | Role |
| --- | --- |
| `ratatui` | TUI framework (layout, widgets, rendering) |
| `crossterm` | Cross-platform terminal I/O, raw mode, mouse events |
| `portable-pty` | PTY pair creation, shell spawning |
| `vt100` | VT100/ANSI escape sequence parser → virtual screen |
| `tui-term` | Renders a `vt100::Screen` as a ratatui `Widget` |
| `tokio` | Async runtime for concurrent I/O streams |

---

## Safety & Cleanup

`TerminalGuard` is a zero-size RAII wrapper that implements `Drop`.

Because `Drop` runs on panics, early returns, and normal exits alike, the SSH session is always restored to a sane state - preventing cooked/broken terminals or trailing escape sequences after a crash. Child processes (like `nvim` or `bash`) are sent graceful exit sequences (`:qa!`) and then forcefully killed on drop to prevent thread deadlocks and zombie processes.

---

## Future Roadmap

* [ ] **GUI Start Menu** - Integrate the Command Bar into an interactive taskbar / start menu widget.
* [ ] **Tab / Workspace Support** - Multiple virtual desktops.
* [ ] **Dolphin-style Grid Explorer** - Upgrade the VFS list to a spatial icon grid.
