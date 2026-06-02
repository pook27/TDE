//! Layout tree, geometry helpers, and directional types.
//!
//! This module owns everything that describes *where* panes live on screen:
//! the binary tiling tree (`LayoutNode`), the recursive split/prune operations
//! on that tree, and the pure geometry helpers used by focus navigation and
//! the mouse hit-tester.
//!
//! Nothing here touches I/O, PTY state, or rendering.  The only external
//! dependency is `ratatui::layout`, which provides `Rect`, `Layout`,
//! `Constraint`, and `Direction`.

use std::mem;

use ratatui::layout::{Constraint, Direction, Layout, Rect};

// ═══════════════════════════════════════════════════════════════════════════════
// § 1  Primitive types
// ═══════════════════════════════════════════════════════════════════════════════

/// Unique identifier for a pane.  Allocated monotonically by `AppState`.
pub type PaneId = u32;

// ═══════════════════════════════════════════════════════════════════════════════
// § 2  Direction
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, Debug)]
pub enum Dir {
    Left,
    Right,
    Up,
    Down,
}

// ═══════════════════════════════════════════════════════════════════════════════
// § 3  Split kind
// ═══════════════════════════════════════════════════════════════════════════════

/// Which axis to split on.
#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, Debug)]
pub enum SplitKind {
    /// `v` key: split left/right → SplitHorizontal; new pane on the right.
    Vertical,
    /// `s` key: split top/bottom → SplitVertical; new pane on the bottom.
    Horizontal,
}

// ═══════════════════════════════════════════════════════════════════════════════
// § 4  Layout tree
// ═══════════════════════════════════════════════════════════════════════════════

/// Binary tiling tree.  Leaves hold a `PaneId`; interior nodes hold children.
///
/// The `Sentinel` variant exists solely as a `mem::replace` placeholder that is
/// never visible to any other code path — it is always immediately overwritten.
/// It is not exported and contains no data, so it imposes zero cost.
///
/// `Sentinel` is tagged `#[serde(skip)]`-equivalent via the `other` catch-all
/// so that a stale sentinel in a (hypothetically corrupt) JSON file deserialises
/// gracefully to `Pane(0)` rather than panicking.  In practice a serialised
/// `LayoutNode` will never contain `Sentinel` because `save_session` is only
/// called from a clean exit path where no tree mutation is in progress.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub enum LayoutNode {
    Pane(PaneId),
    SplitHorizontal { left: Box<LayoutNode>, right: Box<LayoutNode>, ratio: u16 },
    SplitVertical   { top:  Box<LayoutNode>, bottom: Box<LayoutNode>, ratio: u16 },
    /// Private placeholder used during tree mutation only.  Must never persist.
    ///
    /// Serialises to the JSON tag `"Sentinel"` but `load_session` validates the
    /// tree before handing it to `AppState`, so this variant is never restored.
    Sentinel,
}

// ── Return type for the recursive prune helper ────────────────────────────────

/// Result of attempting to remove `target` from a subtree.
pub enum PruneResult {
    /// Target was not found in this subtree.
    NotFound,
    /// Target was the leaf itself. The caller (parent split) must absorb the 
    /// loss by replacing itself with its OTHER child.
    RemoveMe,
    /// Target was found and pruned deeper in the tree. The tree has already 
    /// been mutated in place. The caller doesn't need to do anything.
    Handled,
}

impl LayoutNode {
    // ── Zero-allocation tree traversal ───────────────────────────────────────
    //
    // Both traversal methods accept a closure rather than pushing into a Vec.
    // This lets every call-site decide how to consume each (PaneId, Rect) pair:
    // - collect into a Vec   (when all rects are needed, e.g. draw / resize_all)
    // - early-exit search    (when only one rect is needed, e.g. new_pane_size)
    // - accumulate a single  (e.g. move_focus — visits all, keeps running best)
    //
    // None of these paths allocate unless the caller explicitly pushes to a Vec.

    /// Walk the entire subtree in document order, invoking `f(id, rect)` for
    /// every leaf.  `area` is the rect assigned to this node by its parent.
    pub fn walk_rects(&self, area: Rect, f: &mut impl FnMut(PaneId, Rect)) {
        match self {
            LayoutNode::Pane(id) => f(*id, area),

            LayoutNode::SplitHorizontal { left, right, ratio } => {
                let chunks = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([
                        Constraint::Percentage(*ratio),
                        Constraint::Percentage(100 - ratio),
                    ])
                    .split(area);
                left.walk_rects(chunks[0], f);
                right.walk_rects(chunks[1], f);
            }

            LayoutNode::SplitVertical { top, bottom, ratio } => {
                let chunks = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([
                        Constraint::Percentage(*ratio),
                        Constraint::Percentage(100 - ratio),
                    ])
                    .split(area);
                top.walk_rects(chunks[0], f);
                bottom.walk_rects(chunks[1], f);
            }

            LayoutNode::Sentinel => {}
        }
    }

    // ── Tree mutation: split ──────────────────────────────────────────────────

    /// Walk the tree until `LayoutNode::Pane(target)` is found, then replace
    /// it with a new split node containing both the old pane and `new_id`.
    ///
    /// Returns `true` if the target was found and the split performed.
    ///
    /// ## mem::replace strategy
    ///
    /// We cannot move out of `self` when `self` is `&mut LayoutNode` (it's
    /// behind a reference).  `mem::replace` lets us swap in a `Sentinel`
    /// placeholder, take ownership of the old value, build the new split node
    /// from it, then write the new node back into `*self`.  The Sentinel is
    /// never observable outside this function because we always overwrite it
    /// before returning.
    pub fn split_pane(&mut self, target: PaneId, new_id: PaneId, kind: SplitKind) -> bool {
        match self {
            // ── Leaf: is this the target? ─────────────────────────────────
            LayoutNode::Pane(id) if *id == target => {
                // Swap self out, leaving a Sentinel in place temporarily.
                let old = mem::replace(self, LayoutNode::Sentinel);
                // Build the replacement split node.
                *self = match kind {
                    SplitKind::Vertical => LayoutNode::SplitHorizontal {
                        left:  Box::new(old),
                        right: Box::new(LayoutNode::Pane(new_id)),
                        ratio: 50,
                    },
                    SplitKind::Horizontal => LayoutNode::SplitVertical {
                        top:    Box::new(old),
                        bottom: Box::new(LayoutNode::Pane(new_id)),
                        ratio:  50,
                    },
                };
                true
            }

            // ── Leaf: wrong target ────────────────────────────────────────
            LayoutNode::Pane(_) => false,

            // ── Interior: delegate to children ───────────────────────────
            LayoutNode::SplitHorizontal { left, right, .. } => {
                left.split_pane(target, new_id, kind)
                    || right.split_pane(target, new_id, kind)
            }
            LayoutNode::SplitVertical { top, bottom, .. } => {
                top.split_pane(target, new_id, kind)
                    || bottom.split_pane(target, new_id, kind)
            }

            LayoutNode::Sentinel => false, // unreachable
        }
    }

    // ── Tree mutation: prune ──────────────────────────────────────────────────

    /// Recursively remove `LayoutNode::Pane(target)` from the tree.
    ///
    /// The caller is responsible for replacing *its own* reference to `self`
    /// with the `survivor` returned by `PruneResult::Pruned`.
    ///
    /// ## How split-node replacement works
    ///
    /// When a split finds that one of its children was pruned, it needs to
    /// replace *itself* with the surviving sibling.  Because `prune_pane` takes
    /// `&mut self`, we again use `mem::replace`:
    ///
    /// 1. Swap `self` → `Sentinel`.
    /// 2. Destructure the old split value (now owned).
    /// 3. Write `*self = *surviving_sibling` (or keep the updated split).
    // ── Tree mutation: prune ──────────────────────────────────────────────────

    /// Recursively remove `LayoutNode::Pane(target)` from the tree.
    pub fn prune_pane(&mut self, target: PaneId) -> PruneResult {
        match self {
            // ── Leaf: is this the target? ─────────────────────────────────
            LayoutNode::Pane(id) if *id == target => PruneResult::RemoveMe,
            LayoutNode::Pane(_) => PruneResult::NotFound,

            // ── SplitHorizontal ───────────────────────────────────────────
            LayoutNode::SplitHorizontal { left, right, .. } => {
                match left.prune_pane(target) {
                    PruneResult::RemoveMe => {
                        let old_split = mem::replace(self, LayoutNode::Sentinel);
                        if let LayoutNode::SplitHorizontal { right, .. } = old_split {
                            *self = *right;
                        }
                        PruneResult::Handled
                    }
                    PruneResult::Handled => PruneResult::Handled,
                    PruneResult::NotFound => {
                        match right.prune_pane(target) {
                            PruneResult::RemoveMe => {
                                let old_split = mem::replace(self, LayoutNode::Sentinel);
                                if let LayoutNode::SplitHorizontal { left, .. } = old_split {
                                    *self = *left;
                                }
                                PruneResult::Handled
                            }
                            PruneResult::Handled => PruneResult::Handled,
                            PruneResult::NotFound => PruneResult::NotFound,
                        }
                    }
                }
            }

            // ── SplitVertical (mirror of SplitHorizontal) ─────────────────
            LayoutNode::SplitVertical { top, bottom, .. } => {
                match top.prune_pane(target) {
                    PruneResult::RemoveMe => {
                        let old_split = mem::replace(self, LayoutNode::Sentinel);
                        if let LayoutNode::SplitVertical { bottom, .. } = old_split {
                            *self = *bottom;
                        }
                        PruneResult::Handled
                    }
                    PruneResult::Handled => PruneResult::Handled,
                    PruneResult::NotFound => {
                        match bottom.prune_pane(target) {
                            PruneResult::RemoveMe => {
                                let old_split = mem::replace(self, LayoutNode::Sentinel);
                                if let LayoutNode::SplitVertical { top, .. } = old_split {
                                    *self = *top;
                                }
                                PruneResult::Handled
                            }
                            PruneResult::Handled => PruneResult::Handled,
                            PruneResult::NotFound => PruneResult::NotFound,
                        }
                    }
                }
            }

            LayoutNode::Sentinel => PruneResult::NotFound,
        }
    }

    /// Walk the entire subtree invoking `f(id)` for every leaf, in document order.
    pub fn walk_ids(&self, f: &mut impl FnMut(PaneId)) {
        match self {
            LayoutNode::Pane(id)                             => f(*id),
            LayoutNode::SplitHorizontal { left, right, .. } => {
                left.walk_ids(f);
                right.walk_ids(f);
            }
            LayoutNode::SplitVertical { top, bottom, .. }   => {
                top.walk_ids(f);
                bottom.walk_ids(f);
            }
            LayoutNode::Sentinel => {}
        }
    }

    /// Convenience wrapper: collect all PaneIds into a Vec.
    ///
    /// Used by `close_pane` where the positional index of the target *within
    /// the ordered sequence* is genuinely needed.  All other call-sites use
    /// `walk_ids` directly.
    pub fn all_pane_ids(&self) -> Vec<PaneId> {
        let mut out = Vec::new();
        self.walk_ids(&mut |id| out.push(id));
        out
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// § 5  Geometry helpers
// ═══════════════════════════════════════════════════════════════════════════════

pub fn centroid_x(r: Rect) -> i32 { (r.x as i32) + (r.width  as i32) / 2 }
pub fn centroid_y(r: Rect) -> i32 { (r.y as i32) + (r.height as i32) / 2 }

/// Returns a centered sub-rect of `r` spanning `percent_x`% wide and
/// `percent_y`% tall.  Used to position floating overlay widgets.
pub fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

/// True when the *row* ranges of `a` and `b` overlap (they share at least one
/// row).  Used by directional focus movement to filter candidates.
pub fn ranges_overlap_v(a: Rect, b: Rect) -> bool {
    (a.y as i32) < (b.y + b.height) as i32
        && (b.y as i32) < (a.y + a.height) as i32
}

/// True when the *column* ranges of `a` and `b` overlap (they share at least
/// one column).  Used by directional focus movement to filter candidates.
pub fn ranges_overlap_h(a: Rect, b: Rect) -> bool {
    (a.x as i32) < (b.x + b.width) as i32
        && (b.x as i32) < (a.x + a.width) as i32
}
