//! The [`Grid`] cell store — the backing buffer for one screen (primary or
//! alternate).
//!
//! Split out of `terminal.rs` as a self-contained type. `Grid` owns a flat
//! `Vec<Cell>` addressed through a `row_offset` so a full-screen scroll is an
//! O(cols) pointer bump rather than an O(area) memmove (the `YUTANI_NO_RING`
//! kill-switch forces the old memmove path for A/B correctness checks), plus
//! the image placements anchored to that screen and per-row damage tracking.
//! The owning [`Terminal`] and its scroll-region logic live in the parent
//! module and reach this type via `use super::*`.

use super::*;
use crate::style::Cell;

/// `YUTANI_NO_RING=1` disables the ring-buffer scroll fast path, forcing the
/// old `copy_within` memmove on every full-screen scroll. A correctness/perf
/// A-B kill-switch: identical behavior with it set means the ring is sound.
fn no_ring_enabled() -> bool {
    use std::sync::OnceLock;
    static E: OnceLock<bool> = OnceLock::new();
    *E.get_or_init(|| {
        std::env::var("YUTANI_NO_RING").map(|v| !v.is_empty() && v != "0").unwrap_or(false)
    })
}

#[derive(Clone)]
pub struct Grid {
    pub cells: Vec<Cell>,
    pub rows: usize,
    pub cols: usize,
    /// Image placements anchored to this grid. Per-Grid so primary and
    /// alternate screens hold their own image state — entering an alt-screen
    /// app like vim hides the primary's images and exposes the alt's
    /// (initially empty), matching how text content is segregated.
    pub placements: Vec<Placement>,
    /// Per-row "rendered content changed since the renderer last consumed
    /// damage" flags, sized `rows`. Set only when a write *actually* changes a
    /// cell (re-printing the identical glyph+style does not dirty the row), so
    /// the renderer can re-emit just the changed rows and reuse cached vertex
    /// segments for the rest. The renderer reads this via
    /// `Terminal::row_damage` and clears it with `clear_row_damage` after each
    /// rebuild. A freshly built grid starts fully dirty so the first frame
    /// emits everything.
    dirty_rows: Vec<bool>,
    /// Ring-buffer rotation: logical row `r` lives at physical row
    /// `(r + row_offset) % rows`. A full-screen scroll-up just advances this
    /// (O(cols)) instead of memmoving the whole cell array (O(rows*cols)) — the
    /// dominant cost under streaming output. All cell access goes through
    /// `idx`/`row_base` (offset-aware); multi-row physical ops `linearize`
    /// first. `YUTANI_NO_RING=1` forces the old memmove path for A/B checks.
    row_offset: usize,
    /// When true, the ring scroll fast path is disabled (always memmove). Seeded
    /// from `YUTANI_NO_RING`; tests flip it to diff the two paths.
    pub(super) disable_ring: bool,
}

impl Grid {
    pub fn new(rows: usize, cols: usize, blank: Cell) -> Self {
        Self {
            cells: vec![blank; rows * cols],
            rows,
            cols,
            placements: Vec::new(),
            dirty_rows: vec![true; rows],
            row_offset: 0,
            disable_ring: no_ring_enabled(),
        }
    }

    /// Physical storage row for a logical row, through the ring rotation.
    #[inline]
    fn phys_row(&self, row: usize) -> usize {
        (row + self.row_offset) % self.rows.max(1)
    }

    /// First cell index of a logical row in the (rotated) backing store.
    #[inline]
    pub(super) fn row_base(&self, row: usize) -> usize {
        self.phys_row(row) * self.cols
    }

    /// Rotate the backing store so logical row 0 is at physical 0 again
    /// (`row_offset == 0`), restoring the linear layout the multi-row
    /// `copy_within` operations assume. No-op when already linear.
    fn linearize(&mut self) {
        if self.row_offset != 0 {
            self.cells.rotate_left(self.row_offset * self.cols);
            self.row_offset = 0;
        }
    }

    fn idx(&self, row: usize, col: usize) -> usize {
        self.row_base(row) + col
    }

    pub fn get(&self, row: usize, col: usize) -> Cell {
        self.cells[self.idx(row, col)]
    }

    /// Mark a single row's rendered content as changed.
    #[inline]
    pub fn mark_dirty(&mut self, row: usize) {
        if let Some(d) = self.dirty_rows.get_mut(row) {
            *d = true;
        }
    }

    /// Mark every row dirty — used when content shifts wholesale (resize,
    /// reset, alt-screen toggle) or when a mutator can't cheaply localize its
    /// damage. Always correct, just less optimal.
    pub fn mark_all_dirty(&mut self) {
        for d in &mut self.dirty_rows {
            *d = true;
        }
    }

    /// The renderer's view of which live-grid rows changed since the last
    /// `clear_row_damage`.
    pub fn row_damage(&self) -> &[bool] {
        &self.dirty_rows
    }

    /// Reset all damage flags. The renderer calls this after it has finished
    /// emitting (or reusing) every row for the frame.
    pub fn clear_row_damage(&mut self) {
        for d in &mut self.dirty_rows {
            *d = false;
        }
    }

    pub fn set(&mut self, row: usize, col: usize, cell: Cell) {
        let i = self.idx(row, col);
        // Change-gated: re-printing the identical cell must not dirty the row.
        // This is the single natural choke point the renderer's per-row cache
        // depends on, and it also kills churn from apps that repaint identical
        // frames cell-by-cell.
        if self.cells[i] != cell {
            self.cells[i] = cell;
            self.mark_dirty(row);
        }
    }

    pub fn get_mut(&mut self, row: usize, col: usize) -> &mut Cell {
        let i = self.idx(row, col);
        // The caller gets unchecked mutable access, so we can't change-gate
        // here — conservatively mark the row dirty. Only the Kitty placeholder
        // diacritic path uses this, which is rare and does change the cell.
        self.mark_dirty(row);
        &mut self.cells[i]
    }

    pub fn row(&self, row: usize) -> &[Cell] {
        let start = self.row_base(row);
        &self.cells[start..start + self.cols]
    }

    pub fn clear(&mut self, blank: Cell) {
        for c in &mut self.cells {
            *c = blank;
        }
        // Whole grid is blank now; collapse the ring so the layout is linear.
        self.row_offset = 0;
        // ED 2 / full-reset path: an image-aware terminal usually exposes
        // explicit delete commands, but most TUI image users (icat / imgcat /
        // chafa) lean on screen-clear as the implicit reset. Dropping all
        // placements here matches what users observe in Kitty.
        self.placements.clear();
        self.mark_all_dirty();
    }

    /// Fill cells `from..to` of `row` with `blank`.
    pub fn clear_row(&mut self, row: usize, from: usize, to: usize, blank: Cell) {
        let base = self.row_base(row);
        // Change-gated like `set`: clearing an already-blank span leaves the
        // row clean, so an app blanking lines it never wrote doesn't dirty them.
        for i in from..to.min(self.cols) {
            if self.cells[base + i] != blank {
                self.cells[base + i] = blank;
                self.mark_dirty(row);
            }
        }
    }

    /// Shift rows `[top..=bottom]` up by `n` within columns `[left..=right]`,
    /// filling freed cells with `blank`. Cells outside the column range are
    /// untouched — that's what makes tmux-style per-pane scrolling work.
    ///
    /// Returns placements that scrolled fully off the top of the grid as a
    /// side effect of the shift. The caller decides whether to route them
    /// into scrollback (only `Terminal::scroll_region_up_by` does, and only
    /// when the live grid is the full-screen primary).
    /// `shift_damage` is set only by a scrollback-growing full-screen scroll
    /// (see [`Terminal::scroll_region_up_by`]): there the shifted content keeps
    /// its absolute-line identity, so each row's damage flag moves up with its
    /// cells and the renderer reuses the unchanged rows by abs line. For every
    /// other caller (partial region, DECSLRM, IL/DL) content changes per row
    /// position, so the whole region is marked dirty.
    pub fn scroll_region_up(
        &mut self,
        top: usize,
        bottom: usize,
        left: usize,
        right: usize,
        n: usize,
        blank: Cell,
        shift_damage: bool,
    ) -> Vec<Placement> {
        let region = bottom - top + 1;
        let n = n.min(region);
        if n == 0 || left > right || right >= self.cols {
            return Vec::new();
        }
        let width = right - left + 1;
        let full_width = left == 0 && right == self.cols - 1;
        let full_screen = top == 0 && bottom == self.rows - 1;
        // O(1) scroll-up: when the whole screen scrolls, relabel rows by
        // advancing the ring offset instead of memmoving the cell array (the
        // dominant cost under streaming output). The freed rows are cleared
        // below; `clear_row` is offset-aware, so it blanks the right cells.
        let use_ring = full_width && full_screen && n < region && !self.disable_ring;
        if !use_ring {
            // The `copy_within` paths below address cells by `r * cols`, which
            // is only valid for a linear (offset 0) layout.
            self.linearize();
        }
        if shift_damage && full_width {
            // Move each row's damage up with its content; the freed rows at the
            // bottom are re-marked by `clear_row` below. Read-ahead (r+n) is
            // always above the write (r), so a forward sweep is safe. Guarded
            // like the cell copy: when n == region nothing shifts (every row is
            // cleared), and `bottom - n` would underflow.
            if n < region {
                for r in top..=bottom - n {
                    self.dirty_rows[r] = self.dirty_rows[r + n];
                }
            }
        } else {
            // Content shifts between rows across the whole region, so every row
            // in it renders differently afterward.
            for r in top..=bottom {
                self.mark_dirty(r);
            }
        }
        if use_ring {
            // Rows `n..rows` become `0..rows-n`; rows `0..n` roll off the top.
            self.row_offset = (self.row_offset + n) % self.rows;
        } else if n < region {
            // Copy only if there's something to shift. When n == region, every
            // row of the region gets cleared and nothing moves.
            for r in top..=bottom - n {
                let src = (r + n) * self.cols + left;
                let dst = r * self.cols + left;
                if full_width {
                    self.cells.copy_within(src..src + self.cols, dst);
                } else {
                    self.cells.copy_within(src..src + width, dst);
                }
            }
        }
        for r in bottom + 1 - n..=bottom {
            if full_width {
                self.clear_row(r, 0, self.cols, blank);
            } else {
                self.clear_row(r, left, right + 1, blank);
            }
        }
        // Shift placements that intersect the scroll region. Partial-width
        // scrolls (DECSLRM) leave images alone — the use case (per-pane tmux
        // scrolling with images) is rare and the math for "shift only the
        // pixels inside the column range" doesn't fit a whole-cell anchor.
        if full_width {
            self.shift_placements_up_in(top, bottom, n)
        } else {
            Vec::new()
        }
    }

    /// Shift rows `[top..=bottom]` down by `n` within columns `[left..=right]`,
    /// filling freed cells at the top of the region with `blank`.
    pub fn scroll_region_down(
        &mut self,
        top: usize,
        bottom: usize,
        left: usize,
        right: usize,
        n: usize,
        blank: Cell,
    ) {
        let region = bottom - top + 1;
        let n = n.min(region);
        if n == 0 || left > right || right >= self.cols {
            return;
        }
        // The `copy_within` below addresses cells by `r * cols` — collapse the
        // ring first. (Scroll-down is rare; no ring fast path for it.)
        self.linearize();
        for r in top..=bottom {
            self.mark_dirty(r);
        }
        let width = right - left + 1;
        let full_width = left == 0 && right == self.cols - 1;
        if n < region {
            for r in (top + n..=bottom).rev() {
                let src = (r - n) * self.cols + left;
                let dst = r * self.cols + left;
                if full_width {
                    self.cells.copy_within(src..src + self.cols, dst);
                } else {
                    self.cells.copy_within(src..src + width, dst);
                }
            }
        }
        for r in top..top + n {
            if full_width {
                self.clear_row(r, 0, self.cols, blank);
            } else {
                self.clear_row(r, left, right + 1, blank);
            }
        }
        if full_width {
            self.shift_placements_down_in(top, bottom, n);
        }
    }

    /// Shift placements intersecting `[top, bottom]` up by `n` rows. Drops
    /// any that have fully scrolled off the top of the grid (`bottom_row <= 0`).
    /// Returns the dropped placements so the caller can route them into
    /// scrollback when appropriate (`Terminal::scroll_region_up_by` does).
    pub(crate) fn shift_placements_up_in(
        &mut self,
        top: usize,
        bottom: usize,
        n: usize,
    ) -> Vec<Placement> {
        let mut dropped = Vec::new();
        self.placements.retain_mut(|p| {
            if p.rows_intersect(top, bottom) {
                p.top_row -= n as isize;
                if p.fully_off_grid(self.rows, self.cols) {
                    dropped.push(p.clone());
                    return false;
                }
            }
            true
        });
        dropped
    }

    /// Mirror of `shift_placements_up_in` for down-scroll. Placements that
    /// fall off the bottom are dropped (not surfaced to the caller — there's
    /// no "scrollback below" to route them to).
    pub(crate) fn shift_placements_down_in(
        &mut self,
        top: usize,
        bottom: usize,
        n: usize,
    ) {
        self.placements.retain_mut(|p| {
            if p.rows_intersect(top, bottom) {
                p.top_row += n as isize;
                if p.fully_off_grid(self.rows, self.cols) {
                    return false;
                }
            }
            true
        });
    }
}
