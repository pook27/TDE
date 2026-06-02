//! Virtual file-system layer: the Explorer pane data model and its async
//! directory-reading task.
//!
//! `ExplorerPane` is a pure data struct — it has no knowledge of rendering or
//! PTY plumbing.  The only coupling back to the rest of the crate is through
//! `crate::AppEvent`, which carries `ExplorerUpdate` results back to the event
//! loop via the `mpsc` channel.

use std::{
    cell::RefCell,
    path::PathBuf,
    time::Instant,
};

use tokio::sync::mpsc;

use crate::layout::PaneId;
use crate::app::AppEvent;

// ═══════════════════════════════════════════════════════════════════════════════
// § 1  Directory entry
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Clone)]
pub struct ExplorerEntry {
    pub name:   String,
    pub is_dir: bool,
}

// ═══════════════════════════════════════════════════════════════════════════════
// § 2  Explorer pane state
// ═══════════════════════════════════════════════════════════════════════════════

pub struct ExplorerPane {
    pub id:      PaneId,
    pub cwd:     PathBuf,
    pub entries: Vec<ExplorerEntry>,
    pub selected_index: RefCell<usize>,
    pub scroll_offset:  RefCell<usize>,
    pub last_click: RefCell<Option<(usize, Instant)>>,
}

impl ExplorerPane {
    pub fn new(id: PaneId, path: PathBuf) -> Self {
        Self {
            id,
            cwd: path,
            entries: Vec::new(),
            selected_index: RefCell::new(0),
            scroll_offset:  RefCell::new(0),
            last_click: RefCell::new(None),
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// § 3  Async directory reader
// ═══════════════════════════════════════════════════════════════════════════════

/// Spawn a Tokio task that reads `path` and sends an `AppEvent::ExplorerUpdate`
/// back to the event loop.  The task is fire-and-forget; errors (unreadable
/// directories, channel closure) are silently swallowed since the Explorer will
/// simply remain at its last known state.
pub fn spawn_dir_read(pane_id: PaneId, path: PathBuf, tx: mpsc::Sender<AppEvent>) {
    tokio::spawn(async move {
        let mut entries = Vec::new();

        // Always add a way to go up a directory, unless we are at root.
        if path.parent().is_some() {
            entries.push(ExplorerEntry { name: "..".to_string(), is_dir: true });
        }

        if let Ok(mut read_dir) = tokio::fs::read_dir(&path).await {
            while let Ok(Some(entry)) = read_dir.next_entry().await {
                let name   = entry.file_name().to_string_lossy().to_string();
                let is_dir = entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false);
                entries.push(ExplorerEntry { name, is_dir });
            }
        }

        // Sort: directories first, then alphabetical within each group.
        entries.sort_by(|a, b| {
            b.is_dir.cmp(&a.is_dir).then_with(|| a.name.cmp(&b.name))
        });

        let _ = tx.send(AppEvent::ExplorerUpdate { pane_id, path, entries }).await;
    });
}
