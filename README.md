# TDE — Terminal Desktop Environment

A keyboard-centric, tiling TUI desktop environment for headless/SSH machines, built in Rust with `ratatui` + `crossterm` + `portable-pty` + `vt100`.

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

## Key Bindings (Alt Modifiers)

TDE uses direct `Alt` (Meta) keybindings for zero-latency, stateless navigation, bypassing the need for a tmux-style prefix key.

### Global Shortcuts

| Key | Action |
| --- | --- |
| `Alt+Space` | Open Command Bar Overlay (type app name to launch) |
| `Alt+E` | Open Virtual File Explorer (splits screen) |
| `Alt+V` | Vertical Split (side-by-side) |
| `Alt+S` | Horizontal Split (top-and-bottom) |
| `Alt+X` | Close focused pane |
| `Alt+H/J/K/L` | Move focus (Left/Down/Up/Right) using centroid math |
| `Alt+Q` | Quit TDE |

### Explorer Shortcuts (When focused on File Explorer)

| Key | Action |
| --- | --- |
| `j / k` or `Up / Down` | Navigate files |
| `Enter` | Open file in `nvim` / Enter directory |
| `c` | Create new file (add `/` to end for directory) |
| `Shift+D` | Delete file/directory |

All other keystrokes are forwarded verbatim to the active child shell or application.

---

## Architecture

TDE separates layout structure from pane data to satisfy Rust's strict borrowing rules, and uses an asynchronous MPSC event loop to prevent PTY I/O from blocking the UI thread.

```text
AppState
 ├─ layout:  LayoutNode (Binary Tree: SplitHorizontal, SplitVertical, Pane(id))
 ├─ panes:   HashMap<PaneId, AppPane> (O(1) data access for Terminals & Explorers)
 ├─ focus:   PaneId (Active pane tracking)
 └─ overlay: Option<AppOverlay> (Floating command bar/text input)

run_event_loop()
 ├─ AppEvent::PtyOutput      → parser.process()  → draw()
 ├─ AppEvent::PtyExited      → close_pane()      → draw()
 ├─ AppEvent::ExplorerUpdate → update_vfs()      → draw()
 └─ AppEvent::Input          → dispatch_input()  → draw()
      ├─ Overlay Active  → capture text / execute command
      ├─ Alt pressed     → window management / splits
      └─ Passthrough     → forward to focused PTY or Explorer

```

## Dependency Notes

| Crate | Role |
| --- | --- |
| `ratatui` | TUI framework (layout, widgets, rendering) |
| `crossterm` | Cross-platform terminal I/O, raw mode, events |
| `portable-pty` | PTY pair creation, shell spawning |
| `vt100` | VT100/ANSI escape sequence parser → virtual screen |
| `tui-term` | Renders a `vt100::Screen` as a ratatui `Widget` |
| `tokio` | Async runtime for concurrent I/O streams |

---

## Safety & Cleanup

`TerminalGuard` is a zero-size RAII wrapper that implements `Drop`.

Because `Drop` runs on panics, early returns, and normal exits alike, the SSH session is always restored to a sane state — no more broken terminals after a crash. Child processes (like `nvim` or `bash`) are sent graceful exit sequences (`:qa!`) and then forcefully killed on drop to prevent thread deadlocks and zombie processes.

---

## Future Roadmap

* [ ] **Phase 5: Visual Desktop Compositor** — Swap the tiling tree for a floating layout engine with Z-indexing and mouse click routing.
* [ ] **GUI Start Menu** — Integrate the Command Bar into an interactive taskbar widget.
* [ ] **Tab / Workspace Support** — Multiple virtual desktops.
* [ ] **Dolphin-style Grid Explorer** — Upgrade the VFS list to a spatial icon grid.

