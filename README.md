# TDE — Tiling Desktop Environment
## Phase 1: Foundation (Layout & PTY)

A keyboard-centric, tiling TUI desktop environment for headless/SSH machines,
built in Rust with `ratatui` + `crossterm` + `portable-pty` + `vt100`.

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

## Architecture

```
main()
 ├─ TerminalGuard::new()          → enable raw mode + alternate screen
 ├─ spawn_pty(rows, cols)         → portable-pty master + child shell
 ├─ spawn_pty_reader (OS thread)  ──MPSC──► AppEvent::PtyOutput(bytes)
 ├─ spawn_input_task (tokio task) ──MPSC──► AppEvent::Input(event)
 └─ run_event_loop()
      ├─ AppEvent::PtyOutput  → vt100::Parser::process() → draw()
      ├─ AppEvent::Input(Key) → forward_key_to_pty()
      ├─ AppEvent::Input(Resize) → PTY resize + parser reset + draw()
      ├─ AppEvent::PtyExited  → show "Shell exited" banner
      └─ Ctrl+Q               → break → Drop(TerminalGuard) → clean exit
```

### Layout

```
┌─────────────────────────────────────────────┐  ← row 0
│ TDE  Phase 1 — Foundation (Layout & PTY)    │  top bar (1 line)
├─────────────────────────────────────────────┤  ← row 1
│ ┌─── Terminal ──────────────────────────┐   │
│ │                                       │   │
│ │   (live PTY output rendered via       │   │  main area (fills remaining)
│ │    vt100 parser → tui-term widget)    │   │
│ │                                       │   │
│ └───────────────────────────────────────┘   │
├─────────────────────────────────────────────┤  ← second-to-last row
│ Ctrl+Q quit  Ctrl+C interrupt  Ctrl+Z sus…  │  bottom bar (1 line)
└─────────────────────────────────────────────┘  ← last row
```

---

## Key Bindings

| Key       | Action                            |
|-----------|-----------------------------------|
| Ctrl+Q    | Quit TDE (graceful shutdown)      |
| All other | Forwarded verbatim to the shell   |

---

## Dependency Notes

| Crate          | Role                                               |
|----------------|----------------------------------------------------|
| `ratatui`      | TUI framework (layout, widgets, rendering)         |
| `crossterm`    | Cross-platform terminal I/O, raw mode, events      |
| `portable-pty` | PTY pair creation, shell spawning                  |
| `vt100`        | VT100/ANSI escape sequence parser → virtual screen |
| `tui-term`     | Renders a `vt100::Screen` as a ratatui `Widget`    |
| `tokio`        | Async runtime for concurrent I/O streams           |
| `anyhow`       | Ergonomic error propagation                        |

---

## Safety & Cleanup

`TerminalGuard` is a zero-size RAII wrapper that implements `Drop`:

```rust
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}
```

Because `Drop` runs on **panics, early returns, and normal exits alike**,
the SSH session is always restored to a sane state — no more broken terminals
after a crash.

---

## Phase 2 Roadmap (not yet implemented)

- [ ] Multiple panes (HSplit / VSplit tiling)
- [ ] Focus management + pane switching (Alt+Arrow or Vi-style)
- [ ] Tab / workspace support
- [ ] Status-bar integration (clock, hostname, load average)
- [ ] Configuration file (`~/.config/tde/config.toml`)
- [ ] Copy-mode + clipboard (a-la tmux)
