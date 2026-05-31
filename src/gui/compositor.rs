//! `gui/compositor.rs` — The floating-window render pass.
//!
//! `draw_gui` is called by `app::draw` when `state.mode == DesktopMode::Gui`.
//! It is responsible for:
//!
//! 1. **Wallpaper** — a solid background block with a centred desktop title.
//! 2. **Window stack** — iterate `state.floating_windows` bottom-to-top and
//!    for each window:
//!    a. `Clear` the region to prevent text bleed from layers below.
//!    b. Draw the window chrome (border + title), highlighted if focused.
//!    c. Render the underlying `AppPane` content (terminal or explorer).
//!
//! ## Why a separate function?
//!
//! Keeping the compositor isolated from `app::draw` means:
//! - The tiling render path is completely unmodified.
//! - The compositor can be tested, profiled, and extended (e.g. z-ordering,
//!   window resize handles) without touching the event loop.
//! - Neither path holds state across frames; every call is a pure render of
//!   the current `&AppState` snapshot.

use ratatui::{
    Frame,
    layout::{Alignment, Rect},
    style::{Color, Modifier, Style},
    text::Span,
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph},
};
use tui_term::widget::PseudoTerminal;

use crate::app::{AppPane, AppState, theme};

// ─── Wallpaper ───────────────────────────────────────────────────────────────

/// Dark background colour used as the desktop wallpaper.
///
/// `Color::Rgb(18, 18, 24)` approximates a near-black cool-grey that works
/// well as a window manager background without being pure #000000.
const WALLPAPER_BG: Color = Color::Rgb(18, 18, 24);

/// Accent colour for the desktop title text.
const WALLPAPER_FG: Color = Color::Rgb(80, 130, 200);

// ─── Public entry point ──────────────────────────────────────────────────────

/// Render the entire GUI compositor pass into `frame`.
///
/// Called by `app::draw` when `state.mode == DesktopMode::Gui`.
///
/// # Arguments
///
/// * `frame`     — ratatui frame for the current draw cycle.
/// * `state`     — immutable snapshot of application state.
/// * `full_area` — the **content** area (full terminal minus top/bottom bars).
///                 This is *not* `frame.area()` — the bars are rendered by the
///                 caller before and after this function.
pub fn draw_gui(frame: &mut Frame, state: &AppState, full_area: Rect) {
    // ── 1. Wallpaper ─────────────────────────────────────────────────────────
    draw_wallpaper(frame, full_area, state.floating_windows.len());

    // ── 2. Floating window stack (back → front) ───────────────────────────────
    //
    // Windows are stored bottom-to-top in `state.floating_windows`, so we
    // iterate in forward order.  The last element paints on top of everything
    // else, which is the correct compositor ordering.
    for win in &state.floating_windows {
        // Guard: skip stale windows whose pane was removed mid-session.
        if win.area.width < 2 || win.area.height < 2 { continue; }
        
        let Some(pane) = state.panes.get(&win.id) else { continue };

        let focused = win.id == state.focus;

        // a. Clear the window region to prevent content from layers below
        //    bleeding through transparent cells in the pane's content.
        frame.render_widget(Clear, win.area);

        // b. Window chrome.
        let border_style = if focused {
            Style::default()
                .fg(theme::ACCENT)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme::DIM_BORDER)
        };

        let title_str = match pane {
            AppPane::Terminal(_) => {
                if focused {
                    format!(" [{}] ● Terminal ", win.id)
                } else {
                    format!(" [{}] Terminal ", win.id)
                }
            }
            AppPane::Explorer(_) => {
                if focused {
                    format!(" [{}] ● Explorer ", win.id)
                } else {
                    format!(" [{}] Explorer ", win.id)
                }
            }
        };

        let title_style = if focused {
            Style::default()
                .fg(theme::ACCENT)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme::DIM_TEXT)
        };

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(border_style)
            .title(Span::styled(title_str, title_style));

        // Compute the inner area before consuming `block` in render_widget.
        let inner = block.inner(win.area);
        frame.render_widget(block, win.area);

        // c. Pane content — rendered identically to the tiling draw loop so
        //    behaviour (cursor, colours, list highlighting) is consistent in
        //    both modes.
        render_pane_content(frame, pane, inner);
    }
}

// ─── Private helpers ─────────────────────────────────────────────────────────

/// Render the desktop wallpaper: a filled background block with a centred
/// title and a faint hint about how many windows are open.
fn draw_wallpaper(frame: &mut Frame, area: Rect, window_count: usize) {
    let wallpaper_block = Block::default()
        .style(Style::default().bg(WALLPAPER_BG));
    frame.render_widget(wallpaper_block, area);

    // Centred title line — rendered as a `Paragraph` with `Alignment::Center`
    // so it stays centred even when the terminal is resized.
    let title_text = if window_count == 0 {
        "TDE Desktop\n\nNo open windows — press Alt+V or Alt+S to split a pane,\nthen Alt+G to return to tiling mode.".to_string()
    } else {
        format!(
            "TDE Desktop\n\n{} window{} open",
            window_count,
            if window_count == 1 { "" } else { "s" },
        )
    };

    // Vertical centering: leave (height/2 - 2) blank rows above the text.
    let text_height = 3u16;
    let top_pad = area.height.saturating_sub(text_height) / 2;

    let text_area = Rect {
        x:      area.x,
        y:      area.y + top_pad,
        width:  area.width,
        height: text_height,
    };

    frame.render_widget(
        Paragraph::new(title_text)
            .alignment(Alignment::Center)
            .style(Style::default().fg(WALLPAPER_FG)),
        text_area,
    );
}

/// Render the content of a single pane into `inner_area`.
///
/// This is an exact mirror of the pane-content rendering in `app::draw`'s
/// `walk_rects` closure so that terminals and explorers look identical in
/// both tiling and GUI modes.
pub fn render_pane_content(frame: &mut Frame, pane: &AppPane, inner_area: Rect) {
    match pane {
        AppPane::Terminal(term) => {
            let guard = term.parser.lock().expect("parser poisoned");
            frame.render_widget(PseudoTerminal::new(guard.screen()), inner_area);
        }

        AppPane::Explorer(exp) => {
            let items: Vec<ListItem> = exp.entries.iter().map(|e| {
                let icon  = if e.is_dir { "📁" } else { "📄" };
                let style = if e.is_dir {
                    Style::default().fg(Color::Blue)
                } else {
                    Style::default()
                };
                ListItem::new(format!(" {} {}", icon, e.name)).style(style)
            }).collect();

            let list = List::new(items)
                .highlight_style(
                    Style::default()
                        .bg(Color::DarkGray)
                        .add_modifier(Modifier::BOLD),
                )
                .highlight_symbol(">> ");

            frame.render_stateful_widget(
                list,
                inner_area,
                &mut *exp.list_state.borrow_mut(),
            );
        }
    }
}
