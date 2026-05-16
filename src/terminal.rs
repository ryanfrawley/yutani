use crate::ansi::{self, Event};
use crate::images::ImageId;
use crate::style::{Cell, Style};
use std::collections::VecDeque;

/// Per-instance handle for a placement on the grid. Distinct from the image
/// data behind it so multiple placements of the same image can be tracked
/// and removed independently (Kitty `p=` semantics in phase 3).
pub type PlacementId = u32;

/// One image placement anchored at a grid position. The anchor uses `isize`
/// so a placement whose top edge has scrolled above the viewport (during a
/// scroll burst or smooth-scroll animation) is still representable; the
/// renderer clips with the camera ortho rather than the grid clamping it.
#[derive(Clone, Debug, PartialEq)]
pub struct Placement {
    pub id: PlacementId,
    pub image: ImageId,
    /// Anchor row in viewport coords (negative = top edge above viewport).
    pub top_row: isize,
    /// Anchor column in viewport coords (negative = left edge off-screen).
    pub left_col: isize,
    /// Extent in cells. Drives hit-testing (which cells the image covers
    /// for the purposes of scrolling and the eventual delete-by-cell ops)
    /// and the off-screen check that promotes placements to scrollback.
    pub rows: u16,
    pub cols: u16,
    /// Tie-break when two placements overlap. Higher = drawn later (on
    /// top). Phase 1 doesn't expose this to apps — kept at 0 — but the
    /// field is here so the eventual Kitty `z=` parameter has a home.
    pub z: i32,
}

impl Placement {
    /// Row immediately past the last covered row (exclusive). May exceed
    /// grid height when the placement extends below the viewport.
    pub fn bottom_row(&self) -> isize {
        self.top_row + self.rows as isize
    }

    /// Column immediately past the last covered column (exclusive).
    pub fn right_col(&self) -> isize {
        self.left_col + self.cols as isize
    }

    /// True when no part of the placement intersects the viewport — used to
    /// drop placements after scroll/resize so the live list stays bounded.
    pub fn fully_off_grid(&self, grid_rows: usize, grid_cols: usize) -> bool {
        self.bottom_row() <= 0
            || self.top_row >= grid_rows as isize
            || self.right_col() <= 0
            || self.left_col >= grid_cols as isize
    }

    /// True when the placement's row span has any overlap with [top, bottom]
    /// (inclusive). Used by scroll-region shifting to pick which placements
    /// move with the region's rows.
    fn rows_intersect(&self, top: usize, bottom: usize) -> bool {
        self.bottom_row() > top as isize && self.top_row <= bottom as isize
    }
}

/// How a parser-supplied width / height parameter wants to translate into
/// cell extent. iTerm2 OSC 1337 accepts all four forms; Kitty has its own
/// (slice-N-much-later) but the spec maps cleanly onto this enum.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ImageSizeSpec {
    /// No explicit param — match the image's native size, converted to
    /// cells via the current font metrics.
    Auto,
    /// Exact cell count.
    Cells(u16),
    /// Pixel count; main.rs converts to cells via cell-pixel size.
    Pixels(u32),
    /// Percent of the viewport in the corresponding axis. iTerm uses this
    /// for responsive sizing (`width=50%`).
    Percent(u32),
}

/// A decoded-base64 image payload waiting for main-thread pickup. The
/// terminal parses the protocol-level wrapper (OSC 1337 today, others
/// later) and pushes one of these per inline image; `main.rs` drains
/// `Terminal::take_pending_image_uploads` each frame, fires
/// `Store::request_insert`, and inserts the corresponding `Placement`
/// using the pre-allocated `ImageId`.
#[derive(Clone, Debug, PartialEq)]
pub struct PendingImageUpload {
    /// Raw image bytes (PNG/JPEG/GIF/WebP) — passed straight to the
    /// decode worker. Not yet base64-encoded.
    pub bytes: Vec<u8>,
    /// Pixel dimensions from a header-only peek. `None` if the format
    /// wasn't recognised; main.rs should still attempt the full decode
    /// (which will fail cleanly via `DecodeError::Decode` if so).
    pub pixel_size: Option<(u32, u32)>,
    pub width: ImageSizeSpec,
    pub height: ImageSizeSpec,
    pub preserve_aspect: bool,
    /// iTerm2 `doNotMoveCursor=1`: caller skips the post-placement
    /// line-feed sequence. P2.3 reads this; P2.2 just records.
    pub do_not_move_cursor: bool,
    pub label: Option<String>,
    /// Cell anchor captured when the OSC was processed. main.rs uses this
    /// when it calls `Terminal::insert_placement` so the placement lands
    /// at the cursor position *before* the cursor advanced past the
    /// image — even if subsequent PTY input arrives before the drain.
    pub cell_anchor: (isize, isize),
    /// Cell extent (rows, cols) computed by the OSC handler from the
    /// size spec + image dims + current cell-pixel size.
    pub cell_extent: (u16, u16),
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
}

impl Grid {
    pub fn new(rows: usize, cols: usize, blank: Cell) -> Self {
        Self {
            cells: vec![blank; rows * cols],
            rows,
            cols,
            placements: Vec::new(),
        }
    }

    fn idx(&self, row: usize, col: usize) -> usize {
        row * self.cols + col
    }

    pub fn get(&self, row: usize, col: usize) -> Cell {
        self.cells[self.idx(row, col)]
    }

    pub fn set(&mut self, row: usize, col: usize, cell: Cell) {
        let i = self.idx(row, col);
        self.cells[i] = cell;
    }

    pub fn row(&self, row: usize) -> &[Cell] {
        let start = row * self.cols;
        &self.cells[start..start + self.cols]
    }

    pub fn clear(&mut self, blank: Cell) {
        for c in &mut self.cells {
            *c = blank;
        }
        // ED 2 / full-reset path: an image-aware terminal usually exposes
        // explicit delete commands, but most TUI image users (icat / imgcat /
        // chafa) lean on screen-clear as the implicit reset. Dropping all
        // placements here matches what users observe in Kitty.
        self.placements.clear();
    }

    /// Fill cells `from..to` of `row` with `blank`.
    pub fn clear_row(&mut self, row: usize, from: usize, to: usize, blank: Cell) {
        let base = row * self.cols;
        for i in from..to.min(self.cols) {
            self.cells[base + i] = blank;
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
    pub fn scroll_region_up(
        &mut self,
        top: usize,
        bottom: usize,
        left: usize,
        right: usize,
        n: usize,
        blank: Cell,
    ) -> Vec<Placement> {
        let region = bottom - top + 1;
        let n = n.min(region);
        if n == 0 || left > right || right >= self.cols {
            return Vec::new();
        }
        let width = right - left + 1;
        let full_width = left == 0 && right == self.cols - 1;
        // Copy only if there's something to shift. When n == region, every
        // row of the region gets cleared and nothing moves.
        if n < region {
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

/// Logical cursor shape. The DEC blink/steady distinction collapses here —
/// we don't blink yet.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum CursorShape {
    Block,
    Underline,
    Bar,
}

/// Snapshot of which mouse-tracking modes the host has enabled. The front-end
/// uses this to decide whether to swallow mouse events for its own UI vs.
/// forward them to the PTY.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct MouseProtocol {
    pub press_release: bool, // ?1000
    pub button_motion: bool, // ?1002
    pub any_motion: bool,    // ?1003
    pub sgr: bool,           // ?1006
}

impl MouseProtocol {
    pub fn enabled(&self) -> bool {
        self.press_release || self.button_motion || self.any_motion
    }
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Cursor {
    pub row: usize,
    pub col: usize,
    pub style: Style,
    // DECAWM deferred-wrap: a print that lands in the last column sets this
    // to true without advancing the cursor; the wrap happens on the next
    // print (if autowrap is still on). CR / LF / cursor addressing clear it.
    pub wrap_pending: bool,
}

impl Cursor {
    pub fn new() -> Self {
        Self {
            row: 0,
            col: 0,
            style: Style::new(),
            wrap_pending: false,
        }
    }
}

pub struct Terminal {
    pub cols: usize,
    pub rows: usize,
    primary: Grid,
    alternate: Grid,
    use_alternate: bool,
    cursor: Cursor,
    saved_primary: Option<Cursor>,
    saved_alternate: Option<Cursor>,
    scroll_top: usize,    // inclusive, 0-based
    scroll_bottom: usize, // inclusive, 0-based
    // DECLRMM (`?69`). When false (default) the next two fields are forced
    // to full width and CSI `s` with no params is SCOSC save-cursor.
    lrmm_enabled: bool,
    scroll_left: usize,  // inclusive, 0-based
    scroll_right: usize, // inclusive, 0-based
    autowrap: bool,
    cursor_visible: bool,
    // DECCKM (private mode 1). When true, unmodified cursor keys send SS3
    // forms (\eOA…) instead of CSI (\e[A…). The encoder reads this via
    // `app_cursor_keys()`.
    app_cursor_keys: bool,
    // DECSCUSR raw value: 0/1 = blink block, 2 = block, 3/4 = underline,
    // 5/6 = bar. Renderer reads via `cursor_shape()`.
    cursor_style_dec: u16,
    // Mouse-tracking flags (DEC private modes ?1000/?1002/?1003/?1006).
    mouse_press_release: bool,
    mouse_button_motion: bool,
    mouse_any_motion: bool,
    mouse_sgr: bool,
    // Bracketed paste (?2004): when set, the GUI wraps pasted text in
    // ESC [ 200 ~ … ESC [ 201 ~ before sending it to the PTY.
    bracketed_paste: bool,
    // Theme-derived defaults the terminal reports back for OSC 10/11/12
    // queries. Set by the front end via `set_default_colors`.
    default_fg_rgb: [u8; 3],
    default_bg_rgb: [u8; 3],
    default_cursor_rgb: [u8; 3],
    // Bytes the host has asked us to send back (DSR replies, etc.). Caller
    // drains via `take_response()` after each `feed`.
    pending_response: Vec<u8>,
    parser: ansi::Parser,
    scrollback: VecDeque<Vec<Cell>>,
    scrollback_limit: usize,
    // Scrollback viewport offset: number of history lines currently shifted
    // into view at the top of the viewport. 0 == showing the live grid.
    view_offset: usize,
    // Placements that have scrolled fully off the top of the primary grid.
    // `scrollback_row` indexes into `scrollback` (0 = oldest). When the
    // scrollback evicts its front rows, those indices shift down and
    // placements with a now-negative index are dropped. Alternate-screen
    // placements never reach here; the alt buffer has no scrollback.
    scrollback_placements: VecDeque<ScrollbackPlacement>,
    // Monotonic id source for new placements. Slice 2 doesn't yet allocate
    // these (test code does); slice 4 hooks it up via the debug keybind.
    next_placement_id: PlacementId,
    // Config-driven: when false, placements that fully scroll off the top
    // are dropped rather than promoted to `scrollback_placements`. Saves
    // memory in long-running shells with heavy image traffic. The trade-
    // off — losing the image when the user scrolls history back — is
    // small in phase 1 since scrollback rendering of placements isn't
    // wired up yet.
    keep_placements_in_scrollback: bool,
    // Per-frame outbox: parsed but not yet decoded image payloads. The
    // main thread drains this via `take_pending_image_uploads` and fires
    // the async decode + GPU upload. Kept on Terminal (rather than
    // returned from `feed`) because a single PTY chunk can contain
    // multiple OSC 1337 sequences and we want them all visible in one
    // drain.
    pending_image_uploads: Vec<PendingImageUpload>,
    // Font metrics in framebuffer pixels. The OSC 1337 handler needs
    // these to translate pixel-spec sizing to cell extent. State pushes
    // them in via `set_cell_size_px` at construction and on every font-
    // size change. Defaults to 1×1 — any pre-setter OSC produces a tiny
    // placement rather than panicking.
    cell_w_px: u32,
    line_h_px: u32,
}

#[derive(Clone, Debug, PartialEq)]
struct ScrollbackPlacement {
    /// Index into `Terminal::scrollback` of the placement's anchor row.
    /// 0 = oldest scrollback row. Decremented in lockstep with scrollback
    /// eviction; placements that would go negative are dropped.
    scrollback_row: isize,
    placement: Placement,
}

impl Terminal {
    pub fn new(cols: usize, rows: usize, scrollback_limit: usize) -> Self {
        assert!(cols > 0 && rows > 0, "terminal must be non-empty");
        let blank = Cell::new(' ', Style::new());
        Self {
            cols,
            rows,
            primary: Grid::new(rows, cols, blank),
            alternate: Grid::new(rows, cols, blank),
            use_alternate: false,
            cursor: Cursor::new(),
            saved_primary: None,
            saved_alternate: None,
            scroll_top: 0,
            scroll_bottom: rows - 1,
            lrmm_enabled: false,
            scroll_left: 0,
            scroll_right: cols - 1,
            autowrap: true,
            cursor_visible: true,
            app_cursor_keys: false,
            cursor_style_dec: 0,
            mouse_press_release: false,
            mouse_button_motion: false,
            mouse_any_motion: false,
            mouse_sgr: false,
            bracketed_paste: false,
            default_fg_rgb: [0xcc, 0xcc, 0xcc],
            default_bg_rgb: [0x00, 0x00, 0x00],
            default_cursor_rgb: [0xcc, 0xcc, 0xcc],
            pending_response: Vec::new(),
            parser: ansi::Parser::new(),
            scrollback: VecDeque::new(),
            scrollback_limit,
            view_offset: 0,
            scrollback_placements: VecDeque::new(),
            next_placement_id: 1,
            keep_placements_in_scrollback: true,
            pending_image_uploads: Vec::new(),
            cell_w_px: 1,
            line_h_px: 1,
        }
    }

    /// Tell the terminal how big a cell is in framebuffer pixels. Drives
    /// the OSC-1337 sizing math (Pixels/Percent specs and Auto fallback).
    /// State calls this once at construction and again on every font-size
    /// change; the values are otherwise stable across the session.
    pub fn set_cell_size_px(&mut self, cell_w_px: u32, line_h_px: u32) {
        self.cell_w_px = cell_w_px.max(1);
        self.line_h_px = line_h_px.max(1);
    }

    /// Drain pending image payloads parsed since the last call. main.rs
    /// invokes this each frame to hand decode jobs to the worker.
    pub fn take_pending_image_uploads(&mut self) -> Vec<PendingImageUpload> {
        std::mem::take(&mut self.pending_image_uploads)
    }

    /// Toggle whether fully-scrolled-off placements survive in
    /// `scrollback_placements`. Setting this to false also clears the
    /// current scrollback-placement queue — otherwise existing entries
    /// would linger until the next eviction.
    pub fn set_keep_placements_in_scrollback(&mut self, keep: bool) {
        self.keep_placements_in_scrollback = keep;
        if !keep {
            self.scrollback_placements.clear();
        }
    }

    /// Allocate a fresh placement id and insert the placement into the active
    /// grid. Returns the id so callers (parsers, the debug keybind) can refer
    /// back to it for delete/move ops in later phases.
    pub fn insert_placement(
        &mut self,
        image: ImageId,
        top_row: isize,
        left_col: isize,
        rows: u16,
        cols: u16,
        z: i32,
    ) -> PlacementId {
        let id = self.next_placement_id;
        self.next_placement_id = self.next_placement_id.wrapping_add(1).max(1);
        let placement = Placement { id, image, top_row, left_col, rows, cols, z };
        self.active_grid_mut().placements.push(placement);
        id
    }

    /// Remove every placement that references `image_id`, across both
    /// grids and scrollback. Returns the number removed. Used by main.rs
    /// on decode failure: the placement was inserted optimistically at
    /// OSC time (so the cursor could advance synchronously), and we
    /// need to clean it up when the worker reports the bytes were
    /// undecodable / oversized / over-budget.
    pub fn remove_placements_with_image(&mut self, image_id: ImageId) -> usize {
        let mut removed = 0;
        self.primary.placements.retain(|p| {
            let drop = p.image == image_id;
            if drop { removed += 1; }
            !drop
        });
        self.alternate.placements.retain(|p| {
            let drop = p.image == image_id;
            if drop { removed += 1; }
            !drop
        });
        self.scrollback_placements.retain(|sp| {
            let drop = sp.placement.image == image_id;
            if drop { removed += 1; }
            !drop
        });
        removed
    }

    /// Snapshot of placements anchored to the *live* grid (active screen).
    /// The renderer uses this each frame to build draw rectangles. Does NOT
    /// include placements that have scrolled into scrollback — for those use
    /// `scrollback_placements_in_view`, which maps a scrollback-row anchor
    /// onto a viewport row based on the current view_offset.
    pub fn live_placements(&self) -> &[Placement] {
        &self.active_grid().placements
    }

    /// Scrollback placements that intersect the visible scrollback strip,
    /// rebased into viewport coordinates. Returned `Placement`s are clones
    /// of the originals with `top_row` adjusted so the renderer can apply
    /// the same `top_row * line_height + decorator_offset + scroll_y` math
    /// it already uses for `live_placements()`. `top_row` may be negative
    /// when the placement straddles the top of the viewport — that's fine,
    /// the camera ortho clips it.
    ///
    /// Returns an empty Vec on the alt screen (scrollback is primary-only)
    /// or when `view_offset == 0` (no scrollback visible). Intersection is
    /// against `[0, viewport_rows)`; a placement entirely above the visible
    /// scrollback strip or entirely below the live grid is filtered out.
    pub fn scrollback_placements_in_view(
        &self,
        viewport_rows: usize,
    ) -> Vec<Placement> {
        if self.use_alternate || self.view_offset == 0 {
            return Vec::new();
        }
        let sb_len = self.scrollback.len() as isize;
        let view_off = self.view_offset as isize;
        // Scrollback row sb_r appears at viewport row sb_r - (sb_len - view_off).
        // Equivalently: shift = view_off - sb_len; viewport_row = sb_r + shift.
        let shift = view_off - sb_len;
        let vp_rows = viewport_rows as isize;
        let mut out = Vec::new();
        for sp in &self.scrollback_placements {
            let top = sp.scrollback_row + shift;
            let bottom = top + sp.placement.rows as isize;
            if bottom <= 0 || top >= vp_rows {
                continue;
            }
            let mut p = sp.placement.clone();
            p.top_row = top;
            out.push(p);
        }
        out
    }

    /// Union of ImageIds referenced by every live + scrollback placement,
    /// across both primary and alternate grids. Fed to `images::Store::retain`
    /// each frame so the store drops images whose last placement just went
    /// away (mark-and-sweep refcounting). One-frame deferred drop is
    /// invisible at 60Hz and keeps `terminal.rs` free of GPU types.
    pub fn referenced_image_ids(&self) -> std::collections::HashSet<ImageId> {
        let mut out = std::collections::HashSet::new();
        for p in &self.primary.placements {
            out.insert(p.image);
        }
        for p in &self.alternate.placements {
            out.insert(p.image);
        }
        for sp in &self.scrollback_placements {
            out.insert(sp.placement.image);
        }
        out
    }

    pub fn feed(&mut self, s: &str) {
        let mut events = Vec::new();
        for ch in s.chars() {
            self.parser.feed(ch, |e| events.push(e));
        }
        for e in events {
            self.dispatch(e);
        }
    }

    pub fn cursor(&self) -> Cursor {
        self.cursor
    }

    pub fn cursor_visible(&self) -> bool {
        self.cursor_visible
    }

    pub fn app_cursor_keys(&self) -> bool {
        self.app_cursor_keys
    }

    pub fn cursor_shape(&self) -> CursorShape {
        match self.cursor_style_dec {
            3 | 4 => CursorShape::Underline,
            5 | 6 => CursorShape::Bar,
            _ => CursorShape::Block,
        }
    }

    /// DECSCUSR distinguishes blinking (0/1/3/5) from steady (2/4/6) variants.
    /// Default (value 0) is blink-block, matching xterm.
    pub fn cursor_blink(&self) -> bool {
        matches!(self.cursor_style_dec, 0 | 1 | 3 | 5)
    }

    pub fn bracketed_paste(&self) -> bool {
        self.bracketed_paste
    }

    /// Mouse-protocol state, in a form the front-end can consume directly.
    pub fn mouse_protocol(&self) -> MouseProtocol {
        MouseProtocol {
            press_release: self.mouse_press_release,
            button_motion: self.mouse_button_motion,
            any_motion: self.mouse_any_motion,
            sgr: self.mouse_sgr,
        }
    }

    /// Update the colors the terminal reports back for OSC 10/11/12 queries.
    /// Front-end should call this on theme change.
    pub fn set_default_colors(&mut self, fg: [u8; 3], bg: [u8; 3], cursor: [u8; 3]) {
        self.default_fg_rgb = fg;
        self.default_bg_rgb = bg;
        self.default_cursor_rgb = cursor;
    }

    /// Drain any bytes the emulator wants written back to the host. Returns
    /// an empty vec when there's nothing pending.
    pub fn take_response(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending_response)
    }

    pub fn row(&self, row: usize) -> &[Cell] {
        self.active_grid().row(row)
    }

    pub fn scrollback_len(&self) -> usize {
        self.scrollback.len()
    }

    pub fn view_offset(&self) -> usize {
        self.view_offset
    }

    pub fn at_top(&self) -> bool {
        self.view_offset >= self.scrollback.len()
    }

    pub fn at_bottom(&self) -> bool {
        self.view_offset == 0
    }

    /// True while a full-screen app (vim, less, htop) is on the alternate
    /// screen. Callers use this to suppress scrollback-targeted gestures
    /// like the mouse wheel, since the alt screen has no scroll history.
    pub fn on_alt_screen(&self) -> bool {
        self.use_alternate
    }

    /// Shift the viewport up by `n` lines, pulling older scrollback into view.
    /// No-op (returns false) on the alternate screen or if already at the top.
    pub fn scroll_up(&mut self, n: usize) -> bool {
        if self.use_alternate {
            return false;
        }
        let new = (self.view_offset + n).min(self.scrollback.len());
        if new == self.view_offset {
            return false;
        }
        self.view_offset = new;
        true
    }

    /// Shift the viewport down by `n` lines, toward the live grid.
    pub fn scroll_down(&mut self, n: usize) -> bool {
        if self.use_alternate || self.view_offset == 0 {
            return false;
        }
        self.view_offset = self.view_offset.saturating_sub(n);
        true
    }

    pub fn scroll_to_bottom(&mut self) {
        self.view_offset = 0;
    }

    /// Look up the cell visible at `(visual_row, col)`, accounting for any
    /// active scrollback offset. When the viewport is on the live grid (or
    /// on the alt screen), this is just the grid cell.
    pub fn visible_cell(&self, visual_row: usize, col: usize) -> Cell {
        if self.use_alternate || self.view_offset == 0 {
            return self.active_grid().get(visual_row, col);
        }
        let scrollback_visible = self.view_offset.min(self.rows);
        if visual_row < scrollback_visible {
            let sb_idx = self.scrollback.len() - self.view_offset + visual_row;
            self.scrollback[sb_idx]
                .get(col)
                .copied()
                .unwrap_or(Cell::new(' ', Style::new()))
        } else {
            self.primary.get(visual_row - scrollback_visible, col)
        }
    }

    /// Convert a visual row (relative to the current viewport) to an absolute
    /// line index that's stable across scrolling: 0..scrollback_len indexes
    /// scrollback (oldest first); scrollback_len + r indexes grid row r.
    /// On the alt screen there's no scrollback, so visual_row maps directly.
    pub fn visual_to_abs_line(&self, visual_row: isize) -> isize {
        if self.use_alternate {
            return visual_row;
        }
        self.scrollback.len() as isize - self.view_offset as isize + visual_row
    }

    /// Borrow a row's cells by absolute-line index. Returns `None` for indices
    /// outside the buffer (scrolled-off rows, or below the live grid).
    pub fn line_at(&self, abs_line: isize) -> Option<&[Cell]> {
        if abs_line < 0 {
            return None;
        }
        let abs = abs_line as usize;
        if !self.use_alternate && abs < self.scrollback.len() {
            return Some(&self.scrollback[abs]);
        }
        let base = if self.use_alternate {
            0
        } else {
            self.scrollback.len()
        };
        if abs < base {
            return None;
        }
        let row = abs - base;
        if row < self.rows {
            Some(self.active_grid().row(row))
        } else {
            None
        }
    }

    /// Cell at any signed `visual_row`, including phantom rows above (-1, -2, …)
    /// or below (`rows`, `rows + 1`, …) the visible viewport. Returns `None`
    /// when no content is available there: alt-screen out-of-bounds, scrollback
    /// exhausted above, or beyond the live grid below. Used by the renderer to
    /// draw extra phantom rows for smooth-scroll continuity — emitting two on
    /// each side keeps the area covered through the entire sub-line slide so
    /// rows don't pop into / out of existence at snap boundaries.
    pub fn extended_cell(&self, visual_row: isize, col: usize) -> Option<Cell> {
        if self.use_alternate {
            if visual_row < 0 || visual_row as usize >= self.rows {
                return None;
            }
            return Some(self.alternate.get(visual_row as usize, col));
        }
        let sb_target = self.scrollback.len() as isize - self.view_offset as isize + visual_row;
        if sb_target < 0 {
            return None;
        }
        let sb_target = sb_target as usize;
        if sb_target < self.scrollback.len() {
            return self.scrollback[sb_target].get(col).copied();
        }
        let primary_row = sb_target - self.scrollback.len();
        if primary_row >= self.rows {
            return None;
        }
        Some(self.primary.get(primary_row, col))
    }

    /// Visual row of the live cursor. Includes rows up to `rows + 1` — i.e.
    /// the renderer's phantom-row range below the viewport — so the cursor
    /// stays drawn whenever any pixel of its row could be visible (smooth
    /// sub-line scroll, decorator-offset push at the bottom edge, window
    /// padding slack). Returns `None` only when the cursor is beyond that,
    /// truly off-screen.
    pub fn cursor_visual_row(&self) -> Option<usize> {
        if self.use_alternate {
            return Some(self.cursor.row);
        }
        let scrollback_visible = self.view_offset.min(self.rows);
        let visual = self.cursor.row + scrollback_visible;
        if visual <= self.rows + 1 {
            Some(visual)
        } else {
            None
        }
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        if cols == 0 || rows == 0 || (cols == self.cols && rows == self.rows) {
            return;
        }
        let blank = Cell::new(' ', self.cursor.style);
        let old_rows = self.rows;

        // Vertical reflow on the primary screen so the bottom of the grid
        // (where the prompt almost always lives) survives shrink/grow
        // cycles. Shrink spills the top rows into scrollback; grow pulls
        // them back so previously-hidden content re-uncovers. Both anchor
        // the cursor to the rows it moved with. Alt screen has no
        // scrollback — fall through to the old clamp-only behavior.
        let (spill, refill) = if self.use_alternate {
            (0, 0)
        } else if rows < old_rows {
            (old_rows - rows, 0)
        } else if rows > old_rows {
            let extra = rows - old_rows;
            (0, extra.min(self.scrollback.len()))
        } else {
            (0, 0)
        };

        if spill > 0 && self.scrollback_limit > 0 {
            for r in 0..spill {
                let line = self.primary.row(r).to_vec();
                if self.scrollback.len() == self.scrollback_limit {
                    self.scrollback.pop_front();
                    self.evict_scrollback_placement_front();
                }
                self.scrollback.push_back(line);
            }
            // Match scroll_region_up_by: keep the user's view of historical
            // content stable while new lines stream into scrollback.
            if self.view_offset > 0 {
                self.view_offset = (self.view_offset + spill).min(self.scrollback.len());
            }
        }

        // Migrate primary placements through the spill: those anchored in
        // spilled rows promote to scrollback_placements; those below shift
        // up so they stay over the same logical row. Mirrors what
        // scroll_region_up_by does for a stream of LF events.
        let mut placements = std::mem::take(&mut self.primary.placements);
        if spill > 0 && self.scrollback_limit > 0 {
            let sb_len_after_spill = self.scrollback.len() as isize;
            let mut keep: Vec<Placement> = Vec::with_capacity(placements.len());
            for mut p in placements.drain(..) {
                // Already straddling above the viewport pre-resize — leave it
                // as-is; subsequent scroll_region_up_by calls will handle.
                if p.top_row < 0 {
                    keep.push(p);
                    continue;
                }
                let original_top = p.top_row;
                if (original_top as usize) < spill {
                    // Anchored in a spilled row. Its scrollback index is the
                    // pre-spill index of that row in scrollback (which is
                    // `(sb_len_after - spill) + original_top`). If negative
                    // (extreme case where spill exceeded scrollback limit and
                    // this row was itself evicted), drop the placement.
                    // Also dropped wholesale when retention is disabled.
                    if self.keep_placements_in_scrollback {
                        let scrollback_row = sb_len_after_spill - spill as isize + original_top;
                        if scrollback_row >= 0 {
                            self.scrollback_placements.push_back(ScrollbackPlacement {
                                scrollback_row,
                                placement: p,
                            });
                        }
                    }
                } else {
                    p.top_row -= spill as isize;
                    keep.push(p);
                }
            }
            placements = keep;
        }

        let mut new_primary = Grid::new(rows, cols, blank);
        let mut dst_row = 0usize;
        if refill > 0 {
            let take_from = self.scrollback.len() - refill;
            let pulled: Vec<Vec<Cell>> = self.scrollback.drain(take_from..).collect();
            for src in pulled {
                let width = cols.min(src.len());
                for c in 0..width {
                    new_primary.set(dst_row, c, src[c]);
                }
                dst_row += 1;
            }
            // We just removed `refill` rows from the tail of scrollback. Any
            // view_offset pointing into that tail is now stale; clamp it.
            self.view_offset = self.view_offset.min(self.scrollback.len());

            // Migrate placements through the refill: live placements shift
            // down to make room for the refilled rows; scrollback placements
            // in the drained tail promote to live.
            let sb_post = self.scrollback.len() as isize;
            for p in &mut placements {
                p.top_row += refill as isize;
            }
            let mut i = 0;
            while i < self.scrollback_placements.len() {
                if self.scrollback_placements[i].scrollback_row >= sb_post {
                    let sp = self.scrollback_placements.remove(i).unwrap();
                    let mut p = sp.placement;
                    p.top_row = sp.scrollback_row - sb_post;
                    placements.push(p);
                } else {
                    i += 1;
                }
            }
        }
        let src_start = spill;
        for old_r in src_start..old_rows {
            if dst_row >= rows {
                break;
            }
            let width = cols.min(self.cols);
            for c in 0..width {
                new_primary.set(dst_row, c, self.primary.get(old_r, c));
            }
            dst_row += 1;
        }
        // Final pass: clamp placements to the new grid. Anything fully
        // off-screen (e.g. anchored to columns past a horizontal shrink, or
        // anchored to rows the grow couldn't refill) is dropped now so the
        // live list stays tight. Partially off-screen placements keep their
        // extent and rely on the renderer's clipping.
        placements.retain(|p| !p.fully_off_grid(rows, cols));
        new_primary.placements = placements;

        self.primary = new_primary;
        // Alternate is wiped wholesale on resize (matches the existing cell
        // behavior — alt apps redraw on SIGWINCH), so its placements go too.
        self.alternate = Grid::new(rows, cols, blank);

        // Cursor follows the rows it sat with: spill moves the top down
        // into the grid (cursor steps up by `spill`); refill puts new rows
        // above (cursor steps down by `refill`). Clamp to grid bounds.
        if self.use_alternate {
            self.cursor.row = self.cursor.row.min(rows - 1);
        } else {
            let r = self.cursor.row as isize - spill as isize + refill as isize;
            self.cursor.row = r.clamp(0, rows as isize - 1) as usize;
        }
        self.cursor.col = self.cursor.col.min(cols - 1);
        self.cursor.wrap_pending = false;
        self.cols = cols;
        self.rows = rows;
        self.scroll_top = 0;
        self.scroll_bottom = rows - 1;
        self.scroll_left = 0;
        self.scroll_right = cols - 1;
        self.lrmm_enabled = false;
        self.view_offset = self.view_offset.min(self.scrollback.len());
    }

    fn active_grid(&self) -> &Grid {
        if self.use_alternate {
            &self.alternate
        } else {
            &self.primary
        }
    }

    fn active_grid_mut(&mut self) -> &mut Grid {
        if self.use_alternate {
            &mut self.alternate
        } else {
            &mut self.primary
        }
    }

    fn blank(&self) -> Cell {
        Cell::new(' ', self.cursor.style)
    }

    fn dispatch(&mut self, event: Event) {
        match event {
            Event::Print(ch) => self.print(ch),
            Event::Bell => {}
            Event::Backspace => self.backspace(),
            Event::Tab => self.tab(),
            Event::LineFeed => self.line_feed(),
            Event::CarriageReturn => {
                self.cursor.col = 0;
                self.cursor.wrap_pending = false;
            }
            Event::CursorUp(n) => self.move_cursor(-(n as isize), 0),
            Event::CursorDown(n) => self.move_cursor(n as isize, 0),
            Event::CursorForward(n) => self.move_cursor(0, n as isize),
            Event::CursorBack(n) => self.move_cursor(0, -(n as isize)),
            Event::CursorPosition(row, col) => self.set_cursor_1_based(row, col),
            Event::CursorHorizontalAbs(col) => {
                self.cursor.col = ((col as usize).max(1) - 1).min(self.cols - 1);
                self.cursor.wrap_pending = false;
            }
            Event::CursorVerticalAbs(row) => {
                self.cursor.row = ((row as usize).max(1) - 1).min(self.rows - 1);
                self.cursor.wrap_pending = false;
            }
            Event::EraseInDisplay(mode) => self.erase_in_display(mode),
            Event::EraseInLine(mode) => self.erase_in_line(mode),
            Event::ScrollUp(n) => self.scroll_region_up_by(n as usize),
            Event::ScrollDown(n) => {
                let blank = self.blank();
                let top = self.scroll_top;
                let bottom = self.scroll_bottom;
                let left = self.scroll_left;
                let right = self.scroll_right;
                self.active_grid_mut()
                    .scroll_region_down(top, bottom, left, right, n as usize, blank);
            }
            Event::SetScrollRegion(top, bottom) => self.set_scroll_region(top, bottom),
            Event::SetLeftRightMargin(l, r) => self.set_left_right_margin(l, r),
            Event::InsertLine(n) => self.insert_lines(n as usize),
            Event::DeleteLine(n) => self.delete_lines(n as usize),
            Event::InsertChar(n) => self.insert_chars(n as usize),
            Event::DeleteChar(n) => self.delete_chars(n as usize),
            Event::EraseChar(n) => self.erase_chars(n as usize),
            Event::DeviceStatusReport(code) => self.device_status_report(code),
            Event::DeviceAttributes => self.reply(b"\x1b[?1;2c"),
            // VT220 ID, firmware version 276, ROM cartridge 0 — what xterm
            // sends. Apps just check that the reply is well-formed.
            Event::SecondaryDeviceAttributes => self.reply(b"\x1b[>0;276;0c"),
            Event::SetCursorStyle(n) => self.cursor_style_dec = n,
            Event::Osc(s) => self.handle_osc(&s),
            Event::Dcs(s) => self.handle_dcs(&s),
            Event::Sgr(params) => self.cursor.style.apply_sgr(&params),
            Event::PrivateModeSet(n) => self.private_mode(n, true),
            Event::PrivateModeReset(n) => self.private_mode(n, false),
            Event::SaveCursor => self.save_cursor(),
            Event::RestoreCursor => self.restore_cursor(),
            Event::FullReset => self.full_reset(),
        }
    }

    fn print(&mut self, ch: char) {
        // With DECLRMM enabled, autowrap pivots on the right margin instead
        // of the screen edge, and wraps back to the left margin. We detect
        // the "inside LRM" case so that cursor positions sitting *outside*
        // the margins (apps can CUP anywhere) still wrap at the screen edge
        // the way xterm does.
        let inside_lrm =
            self.cursor.col >= self.scroll_left && self.cursor.col <= self.scroll_right;
        let wrap_at = if inside_lrm {
            self.scroll_right
        } else {
            self.cols - 1
        };
        let wrap_to = if inside_lrm { self.scroll_left } else { 0 };

        if self.cursor.wrap_pending && self.autowrap {
            self.cursor.col = wrap_to;
            self.cursor.wrap_pending = false;
            self.line_feed_no_cr();
        }
        let cell = Cell::new(ch, self.cursor.style);
        let row = self.cursor.row;
        let col = self.cursor.col;
        if row < self.rows && col < self.cols {
            self.active_grid_mut().set(row, col, cell);
        }
        if self.cursor.col >= wrap_at {
            if self.autowrap {
                self.cursor.wrap_pending = true;
            }
            // when autowrap is off, cursor sticks at the wrap column
        } else {
            self.cursor.col += 1;
        }
    }

    fn backspace(&mut self) {
        if self.cursor.col > 0 {
            self.cursor.col -= 1;
        }
        self.cursor.wrap_pending = false;
    }

    fn tab(&mut self) {
        let next = ((self.cursor.col / 8) + 1) * 8;
        self.cursor.col = next.min(self.cols - 1);
        self.cursor.wrap_pending = false;
    }

    fn line_feed(&mut self) {
        self.cursor.wrap_pending = false;
        self.line_feed_no_cr();
    }

    // Move cursor down one row. Scrolls the region iff cursor is at
    // scroll_bottom; otherwise just advances (clamped to rows-1).
    fn line_feed_no_cr(&mut self) {
        if self.cursor.row == self.scroll_bottom {
            self.scroll_region_up_by(1);
        } else if self.cursor.row + 1 < self.rows {
            self.cursor.row += 1;
        }
    }

    fn move_cursor(&mut self, drow: isize, dcol: isize) {
        self.cursor.wrap_pending = false;
        let new_row = (self.cursor.row as isize + drow).clamp(0, self.rows as isize - 1);
        let new_col = (self.cursor.col as isize + dcol).clamp(0, self.cols as isize - 1);
        self.cursor.row = new_row as usize;
        self.cursor.col = new_col as usize;
    }

    fn set_cursor_1_based(&mut self, row: u16, col: u16) {
        self.cursor.row = ((row as usize).max(1) - 1).min(self.rows - 1);
        self.cursor.col = ((col as usize).max(1) - 1).min(self.cols - 1);
        self.cursor.wrap_pending = false;
    }

    fn erase_in_display(&mut self, mode: u16) {
        let blank = self.blank();
        let row = self.cursor.row;
        let col = self.cursor.col;
        match mode {
            0 => {
                let grid = self.active_grid_mut();
                grid.clear_row(row, col, grid.cols, blank);
                for r in (row + 1)..grid.rows {
                    grid.clear_row(r, 0, grid.cols, blank);
                }
            }
            1 => {
                let grid = self.active_grid_mut();
                for r in 0..row {
                    grid.clear_row(r, 0, grid.cols, blank);
                }
                grid.clear_row(row, 0, col + 1, blank);
            }
            2 => self.active_grid_mut().clear(blank),
            // ED 3 — xterm "Erase Saved Lines": drop the scrollback buffer
            // (and snap the viewport back to the live grid) without touching
            // on-screen content. Used by `clear -x` / `tput E3`.
            3 => {
                self.scrollback.clear();
                self.scrollback_placements.clear();
                self.view_offset = 0;
            }
            _ => {}
        }
    }

    fn erase_in_line(&mut self, mode: u16) {
        let blank = self.blank();
        let row = self.cursor.row;
        let col = self.cursor.col;
        let grid = self.active_grid_mut();
        match mode {
            0 => grid.clear_row(row, col, grid.cols, blank),
            1 => grid.clear_row(row, 0, col + 1, blank),
            2 => grid.clear_row(row, 0, grid.cols, blank),
            _ => {}
        }
    }

    fn scroll_region_up_by(&mut self, n: usize) {
        // Lines rolling off the top only become scrollback when the *whole*
        // grid is the scroll region — partial regions (DECSTBM or DECSLRM
        // narrower than the screen) just shift in place.
        let full_region = self.scroll_top == 0
            && self.scroll_bottom == self.rows - 1
            && self.scroll_left == 0
            && self.scroll_right == self.cols - 1;
        if !self.use_alternate && full_region && self.scrollback_limit > 0 {
            for _ in 0..n.min(self.rows) {
                let line = self.primary.row(self.scroll_top).to_vec();
                if self.scrollback.len() == self.scrollback_limit {
                    self.scrollback.pop_front();
                    self.evict_scrollback_placement_front();
                }
                self.scrollback.push_back(line);
                // Keep the user's view of historical content stable while
                // new lines stream into scrollback. visible_cell indexes from
                // the end of scrollback, so without this bump every appended
                // line would shift the viewport down by one row.
                if self.view_offset > 0 {
                    self.view_offset = (self.view_offset + 1).min(self.scrollback.len());
                }
            }
        }
        let blank = self.blank();
        let top = self.scroll_top;
        let bottom = self.scroll_bottom;
        let left = self.scroll_left;
        let right = self.scroll_right;
        let dropped = self
            .active_grid_mut()
            .scroll_region_up(top, bottom, left, right, n, blank);
        // Promote scrolled-off placements into scrollback when the live grid
        // is the full-screen primary. Their anchor row in scrollback is
        // (current scrollback length - rows above the *original* viewport top),
        // which the grid already shifted to negative for us — recover the row
        // by anchoring at "current scrollback end + the placement's now-negative
        // top_row" (top_row==-1 means the row that was just pushed at the back).
        if !self.use_alternate && full_region && self.keep_placements_in_scrollback {
            let sb_len = self.scrollback.len() as isize;
            for p in dropped {
                let scrollback_row = sb_len + p.top_row;
                if scrollback_row >= 0 {
                    self.scrollback_placements.push_back(ScrollbackPlacement {
                        scrollback_row,
                        placement: p,
                    });
                }
            }
        }
        // If retention is off, `dropped` is simply discarded — same effect
        // as letting the placement fall off the bottom of `scroll_region_down`.
    }

    /// Decrement scrollback-placement indices because scrollback popped one
    /// row off the front; drop any whose anchor row was the evicted one.
    ///
    /// Phase 1 doesn't render scrollback placements so a strict
    /// drop-on-top-row-eviction rule is sufficient. A future slice that adds
    /// "image partially visible at scrollback top edge" rendering can relax
    /// this to drop only after the placement's bottom row scrolls off.
    fn evict_scrollback_placement_front(&mut self) {
        self.scrollback_placements.retain(|sp| sp.scrollback_row > 0);
        for sp in &mut self.scrollback_placements {
            sp.scrollback_row -= 1;
        }
    }

    #[cfg(test)]
    pub(crate) fn scrollback_placements_for_test(&self) -> Vec<(isize, &Placement)> {
        self.scrollback_placements
            .iter()
            .map(|sp| (sp.scrollback_row, &sp.placement))
            .collect()
    }

    // IL: blank lines pushed in at the cursor row; rows below shift down and
    // anything past scroll_bottom is lost. No-op outside the scroll region —
    // including outside the LRM column range when DECLRMM is on.
    fn insert_lines(&mut self, n: usize) {
        if self.cursor.row < self.scroll_top
            || self.cursor.row > self.scroll_bottom
            || self.cursor.col < self.scroll_left
            || self.cursor.col > self.scroll_right
        {
            return;
        }
        let blank = self.blank();
        let top = self.cursor.row;
        let bottom = self.scroll_bottom;
        let left = self.scroll_left;
        let right = self.scroll_right;
        self.active_grid_mut()
            .scroll_region_down(top, bottom, left, right, n, blank);
        self.cursor.wrap_pending = false;
    }

    // DL: cursor row and below shift up by n; bottom of region filled blank.
    fn delete_lines(&mut self, n: usize) {
        if self.cursor.row < self.scroll_top
            || self.cursor.row > self.scroll_bottom
            || self.cursor.col < self.scroll_left
            || self.cursor.col > self.scroll_right
        {
            return;
        }
        let blank = self.blank();
        let top = self.cursor.row;
        let bottom = self.scroll_bottom;
        let left = self.scroll_left;
        let right = self.scroll_right;
        self.active_grid_mut()
            .scroll_region_up(top, bottom, left, right, n, blank);
        self.cursor.wrap_pending = false;
    }

    // ICH: shift cells at and right of cursor n columns to the right; fill the
    // gap with blanks. With DECLRMM on, cells stop at the right margin instead
    // of the screen edge, so per-pane edits don't leak past the divider.
    fn insert_chars(&mut self, n: usize) {
        let row = self.cursor.row;
        let col = self.cursor.col;
        let cols = self.cols;
        let right = self.scroll_right;
        if col >= cols || col > right {
            return;
        }
        let n = n.min(right + 1 - col);
        let blank = self.blank();
        let grid = self.active_grid_mut();
        let base = row * cols;
        if col + n <= right {
            grid.cells.copy_within(base + col..base + right + 1 - n, base + col + n);
        }
        for i in col..col + n {
            grid.cells[base + i] = blank;
        }
        self.cursor.wrap_pending = false;
    }

    // DCH: cells right of cursor shift left by n; right margin filled blank.
    fn delete_chars(&mut self, n: usize) {
        let row = self.cursor.row;
        let col = self.cursor.col;
        let cols = self.cols;
        let right = self.scroll_right;
        if col >= cols || col > right {
            return;
        }
        let n = n.min(right + 1 - col);
        let blank = self.blank();
        let grid = self.active_grid_mut();
        let base = row * cols;
        if col + n <= right {
            grid.cells.copy_within(base + col + n..base + right + 1, base + col);
        }
        for i in right + 1 - n..=right {
            grid.cells[base + i] = blank;
        }
        self.cursor.wrap_pending = false;
    }

    // ECH: replace n cells starting at the cursor with blanks. Cursor unchanged.
    // Clipped to the right margin so per-pane erases don't reach into the
    // neighbor.
    fn erase_chars(&mut self, n: usize) {
        let row = self.cursor.row;
        let col = self.cursor.col;
        let right = self.scroll_right;
        if col > right {
            return;
        }
        let end = (col + n).min(right + 1);
        let blank = self.blank();
        self.active_grid_mut().clear_row(row, col, end, blank);
    }

    /// DECSLRM (`CSI Pl ; Pr s`). When DECLRMM is enabled, sets the column
    /// margins and homes the cursor like DECSTBM does for rows. When DECLRMM
    /// is disabled, the bare `CSI s` form is SCOSC (save cursor) — that's the
    /// classic xterm overload; with params it's silently ignored.
    fn set_left_right_margin(&mut self, left: Option<u16>, right: Option<u16>) {
        if !self.lrmm_enabled {
            if left.is_none() && right.is_none() {
                self.save_cursor();
            }
            return;
        }
        let l = left.map(|v| v as usize).unwrap_or(1).saturating_sub(1);
        let r = right
            .map(|v| v as usize)
            .unwrap_or(self.cols)
            .saturating_sub(1);
        if l < r && r < self.cols {
            self.scroll_left = l;
            self.scroll_right = r;
        } else {
            // Spec says invalid range resets to full width.
            self.scroll_left = 0;
            self.scroll_right = self.cols - 1;
        }
        // DECSLRM homes the cursor.
        self.cursor.row = 0;
        self.cursor.col = 0;
        self.cursor.wrap_pending = false;
    }

    fn set_scroll_region(&mut self, top: Option<u16>, bottom: Option<u16>) {
        let t = top.map(|v| v as usize).unwrap_or(1).saturating_sub(1);
        let b = bottom.map(|v| v as usize).unwrap_or(self.rows).saturating_sub(1);
        if t < b && b < self.rows {
            self.scroll_top = t;
            self.scroll_bottom = b;
        }
        // DECSTBM homes the cursor.
        self.cursor.row = 0;
        self.cursor.col = 0;
        self.cursor.wrap_pending = false;
    }

    fn private_mode(&mut self, code: u16, set: bool) {
        match code {
            1 => self.app_cursor_keys = set,
            7 => self.autowrap = set,
            25 => self.cursor_visible = set,
            1000 => self.mouse_press_release = set,
            1002 => self.mouse_button_motion = set,
            1003 => self.mouse_any_motion = set,
            1006 => self.mouse_sgr = set,
            2004 => self.bracketed_paste = set,
            1049 | 1047 | 47 => self.switch_screen(set, code == 1049),
            // DECLRMM. Enabling/disabling resets the margins to the full
            // screen — apps must re-issue DECSLRM after enabling.
            69 => {
                self.lrmm_enabled = set;
                self.scroll_left = 0;
                self.scroll_right = self.cols - 1;
            }
            _ => {}
        }
    }

    fn reply(&mut self, bytes: &[u8]) {
        self.pending_response.extend_from_slice(bytes);
    }

    /// OSC payload is `Pn[;data...]`. We dispatch on the numeric Pn.
    fn handle_osc(&mut self, s: &str) {
        let (head, rest) = s.split_once(';').unwrap_or((s, ""));
        let code: u16 = match head.parse() {
            Ok(n) => n,
            Err(_) => return,
        };
        match code {
            // Title setting (0/1/2): ignore.
            0 | 1 | 2 => {}
            10 if rest == "?" => self.reply_color(10, self.default_fg_rgb),
            11 if rest == "?" => self.reply_color(11, self.default_bg_rgb),
            12 if rest == "?" => self.reply_color(12, self.default_cursor_rgb),
            // iTerm2 proprietary namespace. Only `File=...` (inline
            // images) is implemented; everything else is silently
            // dropped to match iTerm's "unknown verb is a no-op" contract.
            1337 => self.handle_osc_1337(rest),
            _ => {}
        }
    }

    /// `OSC 1337 ; <verb>=<args> [: <base64>] ST` — iTerm2's proprietary
    /// channel. The one verb we care about is `File=key=val,...:<base64>`
    /// for inline images.
    fn handle_osc_1337(&mut self, payload: &str) {
        // The verb prefix is the run up to the first '='. For `File=…` the
        // remaining text is the param list (key=val pairs separated by ';')
        // followed by a ':' and base64 payload.
        let Some(rest) = payload.strip_prefix("File=") else {
            return;
        };
        // Split the param-list from the base64 payload on the FIRST ':'.
        // Base64 alphabet doesn't include ':', so any colon separates the
        // wrapper from the body cleanly.
        let Some((args, b64)) = rest.split_once(':') else {
            return;
        };

        let mut width = ImageSizeSpec::Auto;
        let mut height = ImageSizeSpec::Auto;
        let mut preserve_aspect = true;
        let mut inline = true;
        let mut do_not_move_cursor = false;
        let mut name: Option<String> = None;

        for kv in args.split(';') {
            if kv.is_empty() {
                continue;
            }
            let Some((k, v)) = kv.split_once('=') else { continue };
            match k {
                "width" => width = parse_iterm_size(v).unwrap_or(ImageSizeSpec::Auto),
                "height" => height = parse_iterm_size(v).unwrap_or(ImageSizeSpec::Auto),
                "preserveAspectRatio" => preserve_aspect = v != "0",
                // `inline=0` means "download mode" — iTerm offers to save
                // the file. We don't have a download UI; just skip.
                "inline" => inline = v != "0",
                "doNotMoveCursor" => do_not_move_cursor = v != "0",
                "name" => {
                    // Filename is base64'd in iTerm's spec. Best-effort
                    // decode — only used as a debug label.
                    use base64::Engine;
                    if let Ok(b) = base64::engine::general_purpose::STANDARD.decode(v) {
                        if let Ok(s) = String::from_utf8(b) {
                            name = Some(s);
                        }
                    }
                }
                // `size` and any unknown keys are accepted-but-ignored.
                _ => {}
            }
        }

        if !inline {
            return;
        }

        // Base64 payload. iTerm allows internal newlines / spaces for
        // wrapping — strip whitespace before decode.
        use base64::Engine;
        let cleaned: String = b64.chars().filter(|c| !c.is_ascii_whitespace()).collect();
        let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(cleaned.as_bytes()) else {
            return;
        };

        let pixel_size = crate::images::peek_dimensions(&bytes);
        let cell_extent = compute_cell_extent(
            width,
            height,
            pixel_size,
            self.cell_w_px,
            self.line_h_px,
            self.cols as u16,
            self.rows as u16,
            preserve_aspect,
        );

        let original_row = self.cursor.row as isize;
        let original_col = self.cursor.col as isize;

        // Cursor advance: line-feed once per cell row the image will
        // occupy. This pushes any subsequent text below the image AND
        // triggers scroll-up at the bottom of the grid. We don't insert
        // the placement here (main.rs owns the Store), so we need to
        // compensate the captured anchor for any scrolls those LFs
        // produced — measured via the cursor's row delta so the math
        // stays robust to whatever `line_feed` actually does (DECSTBM,
        // origin mode, etc.).
        let rows = cell_extent.0 as isize;
        if !do_not_move_cursor {
            for _ in 0..rows {
                self.line_feed();
            }
        }
        let cursor_advance = self.cursor.row as isize - original_row;
        let scrolls = if do_not_move_cursor { 0 } else { rows - cursor_advance };
        let cell_anchor = (original_row - scrolls, original_col);

        self.pending_image_uploads.push(PendingImageUpload {
            bytes,
            pixel_size,
            width,
            height,
            preserve_aspect,
            do_not_move_cursor,
            label: name,
            cell_anchor,
            cell_extent,
        });
    }

    /// Handle a captured DCS payload. Currently we only implement xterm's
    /// XTGETTCAP query (`+q<hex>;<hex>;...`); other DCS strings are dropped.
    fn handle_dcs(&mut self, s: &str) {
        let Some(rest) = s.strip_prefix("+q") else {
            return;
        };
        // Group by status so we can emit one DCS per group rather than one
        // per cap — apps parse either form, but fewer round-trips is nicer.
        let mut known: Vec<(String, String)> = Vec::new();
        let mut unknown: Vec<String> = Vec::new();
        for hex_name in rest.split(';') {
            let Some(name) = hex_decode_ascii(hex_name) else { continue };
            match termcap_value(&name) {
                Some(v) => known.push((hex_name.to_ascii_lowercase(), hex_encode(v))),
                None => unknown.push(hex_name.to_ascii_lowercase()),
            }
        }
        if !known.is_empty() {
            let body = known
                .iter()
                .map(|(n, v)| format!("{}={}", n, v))
                .collect::<Vec<_>>()
                .join(";");
            let s = format!("\x1bP1+r{}\x1b\\", body);
            self.pending_response.extend_from_slice(s.as_bytes());
        }
        if !unknown.is_empty() {
            let body = unknown.join(";");
            let s = format!("\x1bP0+r{}\x1b\\", body);
            self.pending_response.extend_from_slice(s.as_bytes());
        }
    }

    fn reply_color(&mut self, code: u16, rgb: [u8; 3]) {
        // xterm RGB form repeats each 8-bit channel for 16-bit precision —
        // apps (vim, tmux) parse `rgb:RRRR/GGGG/BBBB` as either 8- or 16-bit.
        let s = format!(
            "\x1b]{};rgb:{:02x}{:02x}/{:02x}{:02x}/{:02x}{:02x}\x1b\\",
            code, rgb[0], rgb[0], rgb[1], rgb[1], rgb[2], rgb[2],
        );
        self.pending_response.extend_from_slice(s.as_bytes());
    }

    // CSI 5n → "\e[0n" (terminal OK). CSI 6n → "\e[<row>;<col>R" (cursor
    // position, 1-based). Other codes are ignored — we don't implement them
    // and replying with anything would only confuse the host.
    fn device_status_report(&mut self, code: u16) {
        match code {
            5 => self.reply(b"\x1b[0n"),
            6 => {
                let s = format!("\x1b[{};{}R", self.cursor.row + 1, self.cursor.col + 1);
                self.pending_response.extend_from_slice(s.as_bytes());
            }
            _ => {}
        }
    }

    fn switch_screen(&mut self, to_alt: bool, clear_and_save: bool) {
        if to_alt && !self.use_alternate {
            if clear_and_save {
                self.saved_primary = Some(self.cursor);
            }
            let blank = Cell::new(' ', Style::new());
            self.alternate.clear(blank);
            self.use_alternate = true;
            if clear_and_save {
                self.cursor = Cursor::new();
            }
        } else if !to_alt && self.use_alternate {
            self.use_alternate = false;
            if clear_and_save {
                if let Some(cur) = self.saved_primary.take() {
                    self.cursor = cur;
                }
            }
        }
    }

    fn save_cursor(&mut self) {
        let slot = if self.use_alternate {
            &mut self.saved_alternate
        } else {
            &mut self.saved_primary
        };
        *slot = Some(self.cursor);
    }

    fn restore_cursor(&mut self) {
        let saved = if self.use_alternate {
            self.saved_alternate
        } else {
            self.saved_primary
        };
        if let Some(cur) = saved {
            self.cursor = cur;
        }
    }

    fn full_reset(&mut self) {
        let blank = Cell::new(' ', Style::new());
        self.primary.clear(blank);
        self.alternate.clear(blank);
        self.cursor = Cursor::new();
        self.saved_primary = None;
        self.saved_alternate = None;
        self.use_alternate = false;
        self.scroll_top = 0;
        self.scroll_bottom = self.rows - 1;
        self.lrmm_enabled = false;
        self.scroll_left = 0;
        self.scroll_right = self.cols - 1;
        self.autowrap = true;
        self.cursor_visible = true;
        self.app_cursor_keys = false;
        self.cursor_style_dec = 0;
        self.mouse_press_release = false;
        self.mouse_button_motion = false;
        self.mouse_any_motion = false;
        self.mouse_sgr = false;
        self.bracketed_paste = false;
        self.pending_response.clear();
        self.scrollback.clear();
        // Grid::clear already dropped per-grid placements above; also drop
        // scrollback placements since the scrollback rows they anchor to are
        // about to be cleared.
        self.scrollback_placements.clear();
        self.next_placement_id = 1;
    }
}

/// Decode an even-length lowercase/uppercase hex string into its ASCII form.
/// XTGETTCAP queries name capabilities this way (`Co` → "436f").
/// Compute the cell extent (rows, cols) of an inline image given the
/// iTerm2 size specs, the image's native pixel dimensions (if known),
/// the current cell-pixel size, and the viewport extent.
///
/// preserveAspectRatio kicks in when one axis is `Auto` and the other is
/// explicit: the auto-side scales proportionally to the image's native
/// aspect ratio. When both are explicit, both are honoured verbatim
/// (matching iTerm's behaviour — explicit beats preserve).
///
/// Falls back to `(1, 1)` when no useful information is available
/// (Auto + Auto + no pixel size); the placement still appears, just
/// tiny, until decode completes and the user re-issues the OSC with
/// better params.
fn compute_cell_extent(
    width: ImageSizeSpec,
    height: ImageSizeSpec,
    image_px: Option<(u32, u32)>,
    cell_w_px: u32,
    line_h_px: u32,
    viewport_cols: u16,
    viewport_rows: u16,
    preserve_aspect: bool,
) -> (u16, u16) {
    // Resolve each axis to pixels first, then to cells. Lets us apply
    // preserveAspectRatio in pixel space where the math is cleaner.
    let cell_w_px = cell_w_px.max(1);
    let line_h_px = line_h_px.max(1);
    let viewport_w_px = (viewport_cols as u32).saturating_mul(cell_w_px);
    let viewport_h_px = (viewport_rows as u32).saturating_mul(line_h_px);

    // `cell_axis_px` is the cell size on the axis being resolved — width
    // axis uses cell_w_px, height axis uses line_h_px. Passing it
    // explicitly avoids a fragile equality check on viewport sizes (which
    // could match coincidentally on a square viewport).
    let resolve = |spec: ImageSizeSpec,
                   viewport_axis_px: u32,
                   cell_axis_px: u32,
                   img_axis_px: Option<u32>|
     -> Option<u32> {
        match spec {
            ImageSizeSpec::Cells(n) => Some((n as u32).saturating_mul(cell_axis_px)),
            ImageSizeSpec::Pixels(n) => Some(n),
            ImageSizeSpec::Percent(n) => {
                Some(viewport_axis_px.saturating_mul(n).saturating_div(100).max(1))
            }
            ImageSizeSpec::Auto => img_axis_px,
        }
    };

    let img_w = image_px.map(|(w, _)| w);
    let img_h = image_px.map(|(_, h)| h);
    let mut w_px = resolve(width, viewport_w_px, cell_w_px, img_w);
    let mut h_px = resolve(height, viewport_h_px, line_h_px, img_h);

    if preserve_aspect {
        if let Some((iw, ih)) = image_px {
            if iw > 0 && ih > 0 {
                // Only override the side that was left as Auto. Explicit
                // sizing always wins; preserve only fills in the missing
                // axis from the image's native aspect ratio.
                match (width, height) {
                    (ImageSizeSpec::Auto, _) => {
                        if let Some(h) = h_px {
                            w_px = Some(((h as u64) * (iw as u64) / (ih as u64)).max(1) as u32);
                        }
                    }
                    (_, ImageSizeSpec::Auto) => {
                        if let Some(w) = w_px {
                            h_px = Some(((w as u64) * (ih as u64) / (iw as u64)).max(1) as u32);
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    // Ceiling divide pixels → cells. Fallback to 1 if the spec is
    // unresolvable (e.g. Auto with no header peek).
    let cols = w_px
        .map(|px| ((px + cell_w_px - 1) / cell_w_px).max(1))
        .unwrap_or(1);
    let rows = h_px
        .map(|px| ((px + line_h_px - 1) / line_h_px).max(1))
        .unwrap_or(1);
    (
        rows.min(u16::MAX as u32) as u16,
        cols.min(u16::MAX as u32) as u16,
    )
}

/// Parse an iTerm2 OSC 1337 size token. Accepts:
///
/// - `auto` (case-insensitive) → [`ImageSizeSpec::Auto`]
/// - `N` (digits only) → [`ImageSizeSpec::Cells`]
/// - `Npx` → [`ImageSizeSpec::Pixels`]
/// - `N%` → [`ImageSizeSpec::Percent`]
///
/// Returns `None` for anything else (negative numbers, unsupported units,
/// empty string). Callers default to `Auto` on `None`.
fn parse_iterm_size(s: &str) -> Option<ImageSizeSpec> {
    if s.eq_ignore_ascii_case("auto") || s.is_empty() {
        return Some(ImageSizeSpec::Auto);
    }
    if let Some(num) = s.strip_suffix("px") {
        return num.parse::<u32>().ok().map(ImageSizeSpec::Pixels);
    }
    if let Some(num) = s.strip_suffix('%') {
        return num.parse::<u32>().ok().map(ImageSizeSpec::Percent);
    }
    // Default unit is cells. u16 because Cells max ~screen columns; a
    // higher value would still be safely clamped to grid bounds later.
    s.parse::<u16>().ok().map(ImageSizeSpec::Cells)
}

fn hex_decode_ascii(s: &str) -> Option<String> {
    if s.is_empty() || s.len() % 2 != 0 {
        return None;
    }
    let mut out = String::with_capacity(s.len() / 2);
    for chunk in s.as_bytes().chunks(2) {
        let hi = (chunk[0] as char).to_digit(16)?;
        let lo = (chunk[1] as char).to_digit(16)?;
        out.push(((hi << 4) | lo) as u8 as char);
    }
    Some(out)
}

/// Hex-encode an ASCII string in the form XTGETTCAP expects (lowercase).
fn hex_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.as_bytes() {
        out.push_str(&format!("{:02x}", b));
    }
    out
}

/// Look up a termcap / terminfo capability value. We answer the subset that
/// vim and similar editors actually probe — colors, key sequences, and a
/// handful of terminal flags. Both two-letter (termcap) and long-form
/// (terminfo) names are accepted; values mirror what we *actually* emit, so
/// there's no risk of advertising support we don't implement.
fn termcap_value(name: &str) -> Option<&'static str> {
    Some(match name {
        // Terminal identification & capabilities.
        "TN" | "name" => "xterm-256color",
        "Co" | "colors" => "256",
        "RGB" | "Tc" => "8",
        "bce" => "",
        // Cursor keys (CSI form — apps that care about app-cursor mode read
        // smkx/rmkx separately and switch their own buffers).
        "ku" | "kcuu1" => "\x1b[A",
        "kd" | "kcud1" => "\x1b[B",
        "kr" | "kcuf1" => "\x1b[C",
        "kl" | "kcub1" => "\x1b[D",
        // Navigation cluster.
        "kh" | "khome" => "\x1b[H",
        "@7" | "kend" => "\x1b[F",
        "kP" | "kpp" => "\x1b[5~",
        "kN" | "knp" => "\x1b[6~",
        "kI" | "kich1" => "\x1b[2~",
        "kD" | "kdch1" => "\x1b[3~",
        "kb" | "kbs" => "\x7f",
        "kB" | "kcbt" => "\x1b[Z",
        // Function keys.
        "k1" | "kf1" => "\x1bOP",
        "k2" | "kf2" => "\x1bOQ",
        "k3" | "kf3" => "\x1bOR",
        "k4" | "kf4" => "\x1bOS",
        "k5" | "kf5" => "\x1b[15~",
        "k6" | "kf6" => "\x1b[17~",
        "k7" | "kf7" => "\x1b[18~",
        "k8" | "kf8" => "\x1b[19~",
        "k9" | "kf9" => "\x1b[20~",
        "k;" | "kf10" => "\x1b[21~",
        "F1" | "kf11" => "\x1b[23~",
        "F2" | "kf12" => "\x1b[24~",
        // Shifted nav (xterm modifier-encoded forms — shift = param 2).
        "#2" | "kHOM" => "\x1b[1;2H",
        "*7" | "kEND" => "\x1b[1;2F",
        "#4" | "kLFT" => "\x1b[1;2D",
        "%i" | "kRIT" => "\x1b[1;2C",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Place a 1×1 image with default fields and return its id. Most tests
    /// care only about anchor row/col + extent, not the id or z; this keeps
    /// the test bodies focused.
    fn place(t: &mut Terminal, image: u32, row: isize, col: isize, rows: u16, cols: u16) -> u32 {
        t.insert_placement(ImageId(image), row, col, rows, cols, 0)
    }

    fn live_anchors(t: &Terminal) -> Vec<(u32, isize, isize, u16, u16)> {
        t.live_placements()
            .iter()
            .map(|p| (p.image.0, p.top_row, p.left_col, p.rows, p.cols))
            .collect()
    }

    /// Dump the visible grid as newline-separated row strings, with trailing
    /// spaces trimmed from each row for readability.
    fn render(t: &Terminal) -> String {
        let mut out = String::new();
        for r in 0..t.rows {
            let row: String = t.row(r).iter().map(|c| c.ch).collect();
            out.push_str(row.trim_end());
            if r + 1 < t.rows {
                out.push('\n');
            }
        }
        out
    }

    #[test]
    fn plain_text_goes_into_row_0() {
        let mut t = Terminal::new(10, 3, 100);
        t.feed("hello");
        assert_eq!(render(&t), "hello\n\n");
        assert_eq!(t.cursor().row, 0);
        assert_eq!(t.cursor().col, 5);
    }

    #[test]
    fn crlf_moves_to_next_row() {
        let mut t = Terminal::new(10, 3, 100);
        t.feed("a\r\nb");
        assert_eq!(render(&t), "a\nb\n");
        assert_eq!(t.cursor().row, 1);
        assert_eq!(t.cursor().col, 1);
    }

    #[test]
    fn cursor_position_is_1_based() {
        let mut t = Terminal::new(10, 5, 100);
        t.feed("\x1b[3;4H");
        assert_eq!(t.cursor().row, 2);
        assert_eq!(t.cursor().col, 3);
        // writing at that location
        t.feed("X");
        assert_eq!(t.row(2)[3].ch, 'X');
    }

    #[test]
    fn autowrap_end_of_line() {
        let mut t = Terminal::new(3, 3, 100);
        t.feed("abcd");
        // "abc" fills row 0, 'd' wraps to row 1 col 0
        assert_eq!(t.row(0)[0].ch, 'a');
        assert_eq!(t.row(0)[1].ch, 'b');
        assert_eq!(t.row(0)[2].ch, 'c');
        assert_eq!(t.row(1)[0].ch, 'd');
        assert_eq!(t.cursor().row, 1);
        assert_eq!(t.cursor().col, 1);
    }

    #[test]
    fn autowrap_disabled_sticks_at_last_column() {
        let mut t = Terminal::new(3, 3, 100);
        t.feed("\x1b[?7l"); // autowrap off
        t.feed("abcd");
        assert_eq!(t.row(0)[2].ch, 'd');
        assert_eq!(t.cursor().col, 2);
    }

    #[test]
    fn scroll_when_lf_at_bottom() {
        let mut t = Terminal::new(5, 2, 100);
        t.feed("AAA\r\nBBB\r\nCCC");
        // Row 2 doesn't exist; the "AAA" line scrolled off, now row 0=BBB, row 1=CCC
        assert_eq!(t.row(0).iter().map(|c| c.ch).collect::<String>(), "BBB  ");
        assert_eq!(t.row(1).iter().map(|c| c.ch).collect::<String>(), "CCC  ");
        assert_eq!(t.scrollback_len(), 1);
    }

    #[test]
    fn erase_display_mode_3_clears_scrollback_only() {
        let mut t = Terminal::new(5, 2, 100);
        t.feed("AAAAA\r\nBBBBB\r\nCCCCC\r\nDDDDD");
        assert_eq!(t.scrollback_len(), 2);
        t.feed("\x1b[3J");
        assert_eq!(t.scrollback_len(), 0);
        // Visible grid is untouched.
        assert_eq!(t.row(0).iter().map(|c| c.ch).collect::<String>(), "CCCCC");
        assert_eq!(t.row(1).iter().map(|c| c.ch).collect::<String>(), "DDDDD");
    }

    #[test]
    fn erase_display_mode_0() {
        let mut t = Terminal::new(5, 3, 100);
        t.feed("ABCDE\r\nFGHIJ\r\nKLMNO");
        t.feed("\x1b[2;3H"); // row 2, col 3 (0-based: 1, 2)
        t.feed("\x1b[0J"); // erase cursor to end of screen
        assert_eq!(t.row(0).iter().map(|c| c.ch).collect::<String>(), "ABCDE");
        assert_eq!(t.row(1).iter().map(|c| c.ch).collect::<String>(), "FG   ");
        assert_eq!(t.row(2).iter().map(|c| c.ch).collect::<String>(), "     ");
    }

    #[test]
    fn erase_line_mode_0_from_cursor() {
        let mut t = Terminal::new(5, 1, 100);
        t.feed("ABCDE");
        t.feed("\x1b[1;3H"); // col 3 (0-based: 2)
        t.feed("\x1b[K");
        assert_eq!(t.row(0).iter().map(|c| c.ch).collect::<String>(), "AB   ");
    }

    #[test]
    fn sgr_styles_cells() {
        let mut t = Terminal::new(5, 1, 100);
        t.feed("\x1b[31mA\x1b[0mB");
        assert!(t.row(0)[0].style.color_fg.is_some());
        assert_eq!(t.row(0)[1].style.color_fg, None);
    }

    #[test]
    fn alt_screen_preserves_primary() {
        let mut t = Terminal::new(5, 2, 100);
        t.feed("ABC");
        t.feed("\x1b[?1049h"); // enter alt screen, save cursor, clear alt
        t.feed("XYZ");
        assert_eq!(t.row(0).iter().map(|c| c.ch).collect::<String>(), "XYZ  ");
        t.feed("\x1b[?1049l"); // exit alt, restore cursor
        assert_eq!(t.row(0).iter().map(|c| c.ch).collect::<String>(), "ABC  ");
        assert_eq!(t.cursor().col, 3);
    }

    #[test]
    fn save_and_restore_cursor() {
        let mut t = Terminal::new(5, 3, 100);
        t.feed("\x1b[2;3H");
        t.feed("\x1b7"); // save
        t.feed("\x1b[1;1H");
        t.feed("\x1b8"); // restore
        assert_eq!(t.cursor().row, 1);
        assert_eq!(t.cursor().col, 2);
    }

    #[test]
    fn decslrm_ignored_when_lrmm_off() {
        // CSI 1;5s with DECLRMM disabled is silently dropped — margins stay
        // full-width.
        let mut t = Terminal::new(10, 5, 100);
        t.feed("\x1b[1;5s");
        // Prove margins are still full: fill a row, scroll the region down,
        // and confirm the rightmost cols moved (they wouldn't if clipped).
        t.feed("\x1b[1;1H");
        for c in 0..10 {
            t.feed(&format!("{}", (b'a' + c) as char));
        }
        // SU 1 row — without LRM, the whole row clears.
        t.feed("\x1b[S");
        assert_eq!(t.row(0).iter().map(|c| c.ch).collect::<String>(), "          ");
    }

    #[test]
    fn csi_s_no_params_is_save_cursor_when_lrmm_off() {
        // With DECLRMM off, bare `ESC[s` is SCOSC: save the cursor in the same
        // slot DECSC (ESC 7) uses, so ESC 8 restores it.
        let mut t = Terminal::new(10, 5, 100);
        t.feed("\x1b[3;5H\x1b[s\x1b[1;1H\x1b8");
        assert_eq!(t.cursor().row, 2);
        assert_eq!(t.cursor().col, 4);
    }

    #[test]
    fn decslrm_clips_scroll_up_to_margins() {
        // Reproduce tmux's per-pane scroll: enable DECLRMM, set LRM to the
        // "left pane" columns, scroll up — cells outside the LRM (the
        // divider + right pane) must NOT move.
        let mut t = Terminal::new(10, 5, 100);
        // Fill row 0 with "abcdefghij" (cols 0..9).
        t.feed("abcdefghij");
        // Mark a divider in col 6 on rows 1..4 by direct CUP+Print.
        for r in 2..=5 {
            t.feed(&format!("\x1b[{};7H|", r));
        }
        // Enable DECLRMM, set left=1, right=6 (1-based: cols 0..5).
        t.feed("\x1b[?69h\x1b[1;6s");
        // Set scroll region to rows 1..5 (1-based), put cursor inside.
        t.feed("\x1b[1;5r\x1b[1;1H");
        // SU 5 — should blank cols 0..5 of all 5 rows but leave col 6+
        // untouched (the divider remains).
        t.feed("\x1b[5S");
        for r in 1..=4 {
            assert_eq!(t.row(r)[6].ch, '|', "row {r}: divider should survive LRM scroll");
        }
        // And cols 0..5 of row 0 (with our 'abcdef') should now be blank
        // since SU consumed them.
        for c in 0..6 {
            assert_eq!(t.row(0)[c].ch, ' ', "col {c} should be blank after SU");
        }
        // Col 6 of row 0 stays 'g' — outside LRM, untouched.
        assert_eq!(t.row(0)[6].ch, 'g');
    }

    #[test]
    fn decslrm_clips_linefeed_scroll_to_margins() {
        // Same setup but the scroll is triggered by `\n` at scroll_bottom
        // (the path tmux actually uses while painting per-pane content).
        let mut t = Terminal::new(8, 4, 100);
        // Put 'X' in col 5 across every row as a "divider".
        for r in 1..=4 {
            t.feed(&format!("\x1b[{};6HX", r));
        }
        t.feed("\x1b[?69h\x1b[1;5s"); // LRM cols 1..5 (0..4)
        // Position cursor inside LRM at scroll_bottom, then LF — should
        // scroll within LRM only.
        t.feed("\x1b[4;1H\n");
        for r in 0..4 {
            assert_eq!(t.row(r)[5].ch, 'X', "row {r}: divider must survive LF scroll");
        }
    }

    #[test]
    fn decslrm_clips_insert_and_delete_char() {
        let mut t = Terminal::new(10, 1, 100);
        t.feed("abcdefghij");
        t.feed("\x1b[?69h\x1b[1;5s"); // LRM cols 1..5 (0..4)
        // ICH 2 at col 0 — cells shift within LRM only; cols 5..9 untouched.
        t.feed("\x1b[1;1H\x1b[2@");
        let s: String = t.row(0).iter().map(|c| c.ch).collect();
        assert_eq!(s, "  abcfghij");
        // DCH 2 at col 0 — pulls cells from within LRM only.
        t.feed("\x1b[2P");
        let s: String = t.row(0).iter().map(|c| c.ch).collect();
        assert_eq!(s, "abc  fghij");
    }

    #[test]
    fn decslrm_clips_erase_char_to_right_margin() {
        let mut t = Terminal::new(10, 1, 100);
        t.feed("abcdefghij");
        t.feed("\x1b[?69h\x1b[1;5s");
        t.feed("\x1b[1;3H\x1b[10X"); // ECH 10 at col 3 — clip to col 5
        let s: String = t.row(0).iter().map(|c| c.ch).collect();
        assert_eq!(s, "ab   fghij");
    }

    #[test]
    fn decslrm_69l_disables_and_resets() {
        let mut t = Terminal::new(10, 1, 100);
        t.feed("abcdefghij");
        t.feed("\x1b[?69h\x1b[1;5s"); // LRM on, cols 1..5
        t.feed("\x1b[?69l"); // disable LRMM → margins reset, future DECSLRM ignored
        // Now SU should affect the full width again.
        t.feed("\x1b[1S");
        let s: String = t.row(0).iter().map(|c| c.ch).collect();
        assert_eq!(s, "          ");
    }

    #[test]
    fn full_region_scrollback_requires_full_lrm() {
        // With a narrowed LRM the "full region" rule that pushes lines to
        // scrollback shouldn't fire — scrollback should stay empty.
        let mut t = Terminal::new(10, 3, 100);
        t.feed("row1\n\rrow2\n\rrow3");
        t.feed("\x1b[?69h\x1b[1;5s"); // LRM cols 1..5
        let prior = t.scrollback_len();
        // Force a scroll inside the LRM via SU on the full scroll region.
        t.feed("\x1b[3;1H\x1b[3S");
        assert_eq!(t.scrollback_len(), prior, "narrow LRM must not push to scrollback");
    }

    #[test]
    fn autowrap_inside_lrm_wraps_at_right_margin() {
        // 10 cols, 4 rows. LRM cols 1..5 (0..4). Print 7 chars starting at
        // col 0 row 0: chars 1-5 land in row 0 cols 0..4, char 6 wraps to
        // row 1 col 0 (the LEFT margin), char 7 lands at row 1 col 1.
        // Crucially: row 0 cols 5..9 stay blank — without LRM-aware wrap,
        // the run would have spilled into the neighbor pane.
        let mut t = Terminal::new(10, 4, 100);
        t.feed("\x1b[?69h\x1b[1;5s\x1b[1;1H"); // LRM, cursor to (0,0)
        t.feed("abcdefg");
        let r0: String = t.row(0).iter().map(|c| c.ch).collect();
        let r1: String = t.row(1).iter().map(|c| c.ch).collect();
        assert_eq!(r0, "abcde     ", "row 0 must not bleed past right margin");
        assert_eq!(r1, "fg        ", "wrap target column must be left margin");
    }

    #[test]
    fn autowrap_outside_lrm_uses_screen_edge() {
        // xterm-compat: when the cursor is sitting OUTSIDE the LRM (because
        // CUP placed it there — CUP isn't clipped), the wrap should fall
        // back to the screen edge rather than snapping to the left margin.
        let mut t = Terminal::new(10, 4, 100);
        t.feed("\x1b[?69h\x1b[1;5s"); // LRM cols 1..5
        t.feed("\x1b[1;7H"); // CUP to (0, 6) — outside LRM (col 6 > right=4)
        t.feed("xyzw");      // prints at cols 6,7,8,9 → wrap_pending
        t.feed("Q");         // wraps to (1, 0), prints 'Q'
        let r0: String = t.row(0).iter().map(|c| c.ch).collect();
        let r1: String = t.row(1).iter().map(|c| c.ch).collect();
        assert_eq!(r0, "      xyzw");
        assert_eq!(&r1[..2], "Q ", "outside-LRM wrap targets screen col 0, not left margin");
    }

    #[test]
    fn autowrap_disabled_sticks_at_right_margin() {
        // With DECAWM off, printing past the right margin should overwrite
        // the rightmost cell in place — same shape as the existing screen-
        // edge behavior, but pinned to the LRM right edge.
        let mut t = Terminal::new(10, 2, 100);
        t.feed("\x1b[?69h\x1b[1;5s\x1b[?7l\x1b[1;1H"); // LRM + DECAWM off
        t.feed("abcdefg");
        let r0: String = t.row(0).iter().map(|c| c.ch).collect();
        // Cols 0..3 keep their original chars; col 4 (right margin) ends up
        // holding the last char printed; rest of the row stays blank.
        assert_eq!(&r0[..4], "abcd");
        assert_eq!(r0.chars().nth(4), Some('g'));
        assert_eq!(&r0[5..], "     ");
    }

    #[test]
    fn decslrm_invalid_range_resets_to_full_width() {
        // VT spec: a DECSLRM request with left >= right (or out-of-bounds
        // right) must reset margins to the full screen rather than leaving
        // a previously-narrowed range in place. Apps rely on this to "clear"
        // margins by sending e.g. `CSI 1;1s`.
        let mut t = Terminal::new(10, 1, 100);
        t.feed("abcdefghij");
        // Establish a narrow LRM first so we can prove the next request
        // *resets* rather than no-ops.
        t.feed("\x1b[?69h\x1b[1;5s");
        // Now an invalid range: left == right (both 1-based '3'), which is
        // l >= r in the parsed form. Per spec, this should reset to full.
        t.feed("\x1b[3;3s");
        // SU 1 — if margins were reset, the entire row clears; if the old
        // narrow LRM survived, cols 5..9 would remain "fghij".
        t.feed("\x1b[1S");
        let s: String = t.row(0).iter().map(|c| c.ch).collect();
        assert_eq!(s, "          ", "invalid DECSLRM must reset margins to full width");
    }

    #[test]
    fn decslrm_right_beyond_cols_resets_to_full_width() {
        // Similar to above but tests the other invalid form: right > cols.
        // tmux occasionally emits `CSI 1;<huge>s` when computing margins
        // against a stale geometry; we must treat that as "reset to full".
        let mut t = Terminal::new(10, 1, 100);
        t.feed("abcdefghij");
        t.feed("\x1b[?69h\x1b[1;5s"); // narrow first
        t.feed("\x1b[1;99s"); // right > cols → reset
        t.feed("\x1b[1S");
        let s: String = t.row(0).iter().map(|c| c.ch).collect();
        assert_eq!(s, "          ");
    }

    #[test]
    fn decslrm_homes_the_cursor() {
        // Per VT spec, DECSLRM (like DECSTBM) homes the cursor to (0,0)
        // after setting margins. Apps depend on this when bootstrapping a
        // per-pane drawing context — they don't issue a separate CUP.
        let mut t = Terminal::new(10, 5, 100);
        t.feed("\x1b[3;7H"); // park cursor mid-screen
        assert_eq!(t.cursor().row, 2);
        assert_eq!(t.cursor().col, 6);
        t.feed("\x1b[?69h\x1b[2;6s"); // DECSLRM
        assert_eq!(t.cursor().row, 0, "DECSLRM should home cursor row");
        assert_eq!(t.cursor().col, 0, "DECSLRM should home cursor col");
    }

    #[test]
    fn cup_is_not_clipped_to_lrm() {
        // xterm behavior: DECSLRM only constrains scroll-style *operations*;
        // CUP/HVP can still address any cell on the screen. tmux relies on
        // this — it sets a left-pane LRM, then CUPs to the right pane to
        // draw the divider/right-pane content.
        let mut t = Terminal::new(10, 3, 100);
        t.feed("\x1b[?69h\x1b[1;5s"); // LRM cols 0..4 (narrow)
        // CUP to col 8 (1-based 9), well outside the LRM range.
        t.feed("\x1b[1;9HZ");
        assert_eq!(t.row(0)[8].ch, 'Z', "CUP must place cell outside LRM");
        // And the cursor itself should sit there, not get clamped to col 4.
        // After printing 'Z' at col 8 the cursor advances to col 9.
        assert_eq!(t.cursor().col, 9);
    }

    #[test]
    fn decslrm_clips_scroll_down_to_margins() {
        // Symmetric coverage to decslrm_clips_scroll_up_to_margins: SD (CSI T)
        // must also respect LRM so reverse-scroll inside a pane doesn't
        // disturb cells in neighboring panes.
        let mut t = Terminal::new(10, 5, 100);
        // Fill row 4 with content inside the LRM and a divider in col 6.
        t.feed("\x1b[5;1Habcdef|hij");
        // Mark the divider on the other rows too so we can detect leakage.
        for r in 1..=4 {
            t.feed(&format!("\x1b[{};7H|", r));
        }
        t.feed("\x1b[?69h\x1b[1;6s"); // LRM cols 0..5
        t.feed("\x1b[1;5r\x1b[1;1H"); // scroll region rows 0..4
        t.feed("\x1b[5T"); // SD 5 — should blank cols 0..5 within region
        for r in 0..=4 {
            assert_eq!(t.row(r)[6].ch, '|', "row {r}: divider must survive SD");
        }
        // Cols 0..5 of every row should now be blank (SD pushed content out).
        for c in 0..6 {
            assert_eq!(t.row(4)[c].ch, ' ', "row 4 col {c} should be blank after SD");
        }
    }

    #[test]
    fn insert_line_noop_when_cursor_outside_lrm_columns() {
        // IL/DL are documented as no-ops when the cursor is outside the
        // scroll region. With DECLRMM that "scroll region" is 2D — the
        // cursor must be inside the column range too. This matters because
        // an app drawing into the right pane should not accidentally
        // shift rows in the left pane just because it issued IL with the
        // cursor parked over there.
        let mut t = Terminal::new(10, 3, 100);
        t.feed("row1xxxxxx\n\rrow2xxxxxx\n\rrow3xxxxxx");
        t.feed("\x1b[?69h\x1b[1;5s"); // LRM cols 0..4
        // Park cursor at col 7 (outside LRM), then IL 1.
        t.feed("\x1b[1;8H\x1b[L");
        // Row 0 should still read "row1xxxxx" — IL was a no-op.
        let s: String = t.row(0).iter().map(|c| c.ch).collect();
        assert_eq!(s, "row1xxxxxx", "IL must be a no-op when cursor is outside LRM cols");
    }

    #[test]
    fn delete_line_noop_when_cursor_outside_lrm_columns() {
        // Mirror of the IL test above for DL.
        let mut t = Terminal::new(10, 3, 100);
        t.feed("row1xxxxxx\n\rrow2xxxxxx\n\rrow3xxxxxx");
        t.feed("\x1b[?69h\x1b[1;5s"); // LRM cols 0..4
        t.feed("\x1b[1;8H\x1b[M"); // DL 1 with cursor outside LRM
        let s: String = t.row(0).iter().map(|c| c.ch).collect();
        assert_eq!(s, "row1xxxxxx", "DL must be a no-op when cursor is outside LRM cols");
    }

    #[test]
    fn full_reset_clears_lrmm_state() {
        // RIS (`ESC c`) must restore DECLRMM to disabled and margins to
        // full width. Without this, an app that crashes mid-session and
        // issues RIS would still see a narrowed scroll region — a classic
        // "terminal stuck" symptom.
        let mut t = Terminal::new(10, 1, 100);
        t.feed("\x1b[?69h\x1b[1;5s"); // narrow LRM
        t.feed("\x1bc"); // RIS
        // Now reprint and SU — should clear the whole row, proving LRM
        // reset to full width AND DECLRMM is disabled (so a subsequent
        // bare `CSI s` is SCOSC again, not reset-margins).
        t.feed("abcdefghij\x1b[1S");
        let s: String = t.row(0).iter().map(|c| c.ch).collect();
        assert_eq!(s, "          ", "RIS must reset LRM to full width");
        // Verify DECLRMM was disabled too: bare CSI s must now be SCOSC.
        // Park cursor, save via `CSI s`, move, restore — should land back.
        t.feed("\x1b[1;5H\x1b[s\x1b[1;1H\x1b8");
        assert_eq!(t.cursor().col, 4, "after RIS, bare CSI s should be SCOSC");
    }

    #[test]
    fn resize_clears_lrmm_state() {
        // Resize must drop LRMM — the old margins are tied to the old
        // column count and would be nonsensical (or out of bounds) after
        // a resize. The renderer assumes scroll_right < cols.
        let mut t = Terminal::new(10, 3, 100);
        t.feed("\x1b[?69h\x1b[1;5s"); // narrow LRM on the 10-col grid
        t.resize(20, 3); // grow to 20 cols
        // Fill row 0 across the new width then SU — if LRMM survived, the
        // old narrow margin would leak through and cols 5..19 wouldn't
        // clear. We expect them to clear (LRMM disabled, full width).
        t.feed("\x1b[1;1H");
        for c in 0..20 {
            t.feed(&format!("{}", (b'a' + (c % 26) as u8) as char));
        }
        t.feed("\x1b[1S");
        let s: String = t.row(0).iter().map(|c| c.ch).collect();
        assert_eq!(s, "                    ", "resize must clear LRMM state");
    }

    #[test]
    fn cup_and_print_column_of_chars() {
        // Reproduce the way tmux paints a vertical pane border: for each row,
        // CUP to (row, divider_col) then Print('│'). Whole column must be
        // filled, including rows where nothing else was written.
        let mut t = Terminal::new(7, 5, 100);
        for r in 1..=5 {
            t.feed(&format!("\x1b[{};4H│", r));
        }
        for r in 0..5 {
            assert_eq!(t.row(r)[3].ch, '│', "row {r}: divider missing");
        }
    }

    #[test]
    fn alt_screen_cup_column_paint_survives() {
        // Same as above but inside alt-screen (where tmux actually runs).
        let mut t = Terminal::new(7, 5, 100);
        t.feed("\x1b[?1049h");
        for r in 1..=5 {
            t.feed(&format!("\x1b[{};4H│", r));
        }
        for r in 0..5 {
            assert_eq!(t.row(r)[3].ch, '│', "alt-screen row {r}: divider missing");
        }
    }

    #[test]
    fn backspace_moves_left_without_erasing() {
        let mut t = Terminal::new(5, 1, 100);
        t.feed("ABC\x08");
        assert_eq!(t.cursor().col, 2);
        assert_eq!(t.row(0)[2].ch, 'C'); // not erased
    }

    #[test]
    fn tab_advances_to_next_stop() {
        let mut t = Terminal::new(20, 1, 100);
        t.feed("A\t");
        assert_eq!(t.cursor().col, 8);
        t.feed("B\t");
        assert_eq!(t.cursor().col, 16);
    }

    #[test]
    fn cursor_visible_toggle() {
        let mut t = Terminal::new(5, 1, 100);
        assert!(t.cursor_visible());
        t.feed("\x1b[?25l");
        assert!(!t.cursor_visible());
        t.feed("\x1b[?25h");
        assert!(t.cursor_visible());
    }

    #[test]
    fn set_scroll_region_then_lf_scrolls_only_region() {
        let mut t = Terminal::new(5, 5, 100);
        t.feed("A\r\nB\r\nC\r\nD\r\nE");
        t.feed("\x1b[2;4r"); // region rows 2..=4 (0-based 1..=3); homes cursor
        assert_eq!(t.cursor().row, 0);
        t.feed("\x1b[4;1H"); // cursor at row 4 (0-based 3, last in region)
        t.feed("\n"); // LF at scroll_bottom → scroll region up by 1
        assert_eq!(t.row(0).iter().map(|c| c.ch).collect::<String>(), "A    ");
        // inside region: B (was at row 1) rolled off; row 1 now holds C.
        assert_eq!(t.row(1).iter().map(|c| c.ch).collect::<String>(), "C    ");
        assert_eq!(t.row(2).iter().map(|c| c.ch).collect::<String>(), "D    ");
        // bottom of region was cleared
        assert_eq!(t.row(3).iter().map(|c| c.ch).collect::<String>(), "     ");
        // outside region — untouched
        assert_eq!(t.row(4).iter().map(|c| c.ch).collect::<String>(), "E    ");
    }

    #[test]
    fn insert_line_pushes_rows_down_within_region() {
        let mut t = Terminal::new(3, 4, 100);
        t.feed("AAA\r\nBBB\r\nCCC\r\nDDD");
        t.feed("\x1b[2;1H"); // cursor row 1 (0-based)
        t.feed("\x1b[L"); // IL 1
        assert_eq!(t.row(0).iter().map(|c| c.ch).collect::<String>(), "AAA");
        assert_eq!(t.row(1).iter().map(|c| c.ch).collect::<String>(), "   ");
        assert_eq!(t.row(2).iter().map(|c| c.ch).collect::<String>(), "BBB");
        assert_eq!(t.row(3).iter().map(|c| c.ch).collect::<String>(), "CCC");
    }

    #[test]
    fn insert_line_respects_scroll_region() {
        let mut t = Terminal::new(3, 4, 100);
        t.feed("AAA\r\nBBB\r\nCCC\r\nDDD");
        t.feed("\x1b[2;3r"); // region rows 2..=3
        t.feed("\x1b[2;1H"); // cursor at row 1 (top of region)
        t.feed("\x1b[L");
        // outside region untouched
        assert_eq!(t.row(0).iter().map(|c| c.ch).collect::<String>(), "AAA");
        // inside region: blank inserted at top, BBB shifts down, CCC pushed off
        assert_eq!(t.row(1).iter().map(|c| c.ch).collect::<String>(), "   ");
        assert_eq!(t.row(2).iter().map(|c| c.ch).collect::<String>(), "BBB");
        assert_eq!(t.row(3).iter().map(|c| c.ch).collect::<String>(), "DDD");
    }

    #[test]
    fn insert_line_outside_region_is_noop() {
        let mut t = Terminal::new(3, 4, 100);
        t.feed("AAA\r\nBBB\r\nCCC\r\nDDD");
        t.feed("\x1b[2;3r"); // region rows 2..=3 (0-based 1..=2)
        t.feed("\x1b[1;1H"); // cursor at row 0 (outside region)
        t.feed("\x1b[L");
        assert_eq!(t.row(0).iter().map(|c| c.ch).collect::<String>(), "AAA");
        assert_eq!(t.row(1).iter().map(|c| c.ch).collect::<String>(), "BBB");
        assert_eq!(t.row(2).iter().map(|c| c.ch).collect::<String>(), "CCC");
    }

    #[test]
    fn delete_line_shifts_rows_up() {
        let mut t = Terminal::new(3, 4, 100);
        t.feed("AAA\r\nBBB\r\nCCC\r\nDDD");
        t.feed("\x1b[2;1H");
        t.feed("\x1b[M"); // DL 1
        assert_eq!(t.row(0).iter().map(|c| c.ch).collect::<String>(), "AAA");
        assert_eq!(t.row(1).iter().map(|c| c.ch).collect::<String>(), "CCC");
        assert_eq!(t.row(2).iter().map(|c| c.ch).collect::<String>(), "DDD");
        assert_eq!(t.row(3).iter().map(|c| c.ch).collect::<String>(), "   ");
    }

    #[test]
    fn delete_line_does_not_push_to_scrollback() {
        let mut t = Terminal::new(3, 4, 100);
        t.feed("AAA\r\nBBB\r\nCCC\r\nDDD");
        t.feed("\x1b[1;1H");
        t.feed("\x1b[M");
        assert_eq!(t.scrollback_len(), 0);
    }

    #[test]
    fn insert_char_shifts_within_row() {
        let mut t = Terminal::new(6, 1, 100);
        t.feed("ABCDEF");
        t.feed("\x1b[1;3H"); // col 3 (0-based 2)
        t.feed("\x1b[2@");
        // CD shifted right by 2; right edge dropped.
        assert_eq!(t.row(0).iter().map(|c| c.ch).collect::<String>(), "AB  CD");
    }

    #[test]
    fn insert_char_clamps_to_eol() {
        let mut t = Terminal::new(5, 1, 100);
        t.feed("ABCDE");
        t.feed("\x1b[1;3H");
        t.feed("\x1b[99@"); // clamps to 3 cols
        assert_eq!(t.row(0).iter().map(|c| c.ch).collect::<String>(), "AB   ");
    }

    #[test]
    fn delete_char_shifts_within_row() {
        let mut t = Terminal::new(6, 1, 100);
        t.feed("ABCDEF");
        t.feed("\x1b[1;3H"); // col 3
        t.feed("\x1b[2P");
        // CD removed; EF slides left; right edge filled.
        assert_eq!(t.row(0).iter().map(|c| c.ch).collect::<String>(), "ABEF  ");
    }

    #[test]
    fn erase_char_replaces_in_place() {
        let mut t = Terminal::new(6, 1, 100);
        t.feed("ABCDEF");
        t.feed("\x1b[1;3H");
        t.feed("\x1b[3X");
        assert_eq!(t.row(0).iter().map(|c| c.ch).collect::<String>(), "AB   F");
        // cursor unchanged
        assert_eq!(t.cursor().col, 2);
    }

    #[test]
    fn dsr_5_replies_ok() {
        let mut t = Terminal::new(5, 3, 100);
        t.feed("\x1b[5n");
        assert_eq!(t.take_response(), b"\x1b[0n".to_vec());
        // Drained — second call is empty.
        assert!(t.take_response().is_empty());
    }

    #[test]
    fn dsr_6_reports_cursor_position_1_based() {
        let mut t = Terminal::new(10, 5, 100);
        t.feed("\x1b[3;7H"); // row 3, col 7 (1-based)
        t.feed("\x1b[6n");
        assert_eq!(t.take_response(), b"\x1b[3;7R".to_vec());
    }

    #[test]
    fn dsr_unknown_code_no_reply() {
        let mut t = Terminal::new(5, 3, 100);
        t.feed("\x1b[99n");
        assert!(t.take_response().is_empty());
    }

    #[test]
    fn deccm_set_and_reset_toggles_app_cursor_keys() {
        let mut t = Terminal::new(5, 3, 100);
        assert!(!t.app_cursor_keys());
        t.feed("\x1b[?1h");
        assert!(t.app_cursor_keys());
        t.feed("\x1b[?1l");
        assert!(!t.app_cursor_keys());
    }

    #[test]
    fn primary_da_replies_with_vt100() {
        let mut t = Terminal::new(5, 3, 100);
        t.feed("\x1b[c");
        assert_eq!(t.take_response(), b"\x1b[?1;2c".to_vec());
    }

    #[test]
    fn secondary_da_replies_with_vt220() {
        let mut t = Terminal::new(5, 3, 100);
        t.feed("\x1b[>c");
        assert_eq!(t.take_response(), b"\x1b[>0;276;0c".to_vec());
    }

    #[test]
    fn cursor_blink_follows_decscusr() {
        let mut t = Terminal::new(5, 3, 100);
        assert!(t.cursor_blink()); // default 0 — blink block
        t.feed("\x1b[2 q");
        assert!(!t.cursor_blink()); // steady block
        t.feed("\x1b[3 q");
        assert!(t.cursor_blink()); // blink underline
        t.feed("\x1b[6 q");
        assert!(!t.cursor_blink()); // steady bar
    }

    #[test]
    fn decscusr_sets_cursor_shape() {
        let mut t = Terminal::new(5, 3, 100);
        assert_eq!(t.cursor_shape(), CursorShape::Block);
        t.feed("\x1b[3 q");
        assert_eq!(t.cursor_shape(), CursorShape::Underline);
        t.feed("\x1b[5 q");
        assert_eq!(t.cursor_shape(), CursorShape::Bar);
        t.feed("\x1b[2 q");
        assert_eq!(t.cursor_shape(), CursorShape::Block);
    }

    #[test]
    fn osc_color_query_replies_with_default() {
        let mut t = Terminal::new(5, 3, 100);
        t.set_default_colors([0xab, 0xcd, 0xef], [0x01, 0x02, 0x03], [0x77, 0x88, 0x99]);
        t.feed("\x1b]10;?\x07");
        assert_eq!(
            t.take_response(),
            b"\x1b]10;rgb:abab/cdcd/efef\x1b\\".to_vec(),
        );
        t.feed("\x1b]11;?\x1b\\");
        assert_eq!(
            t.take_response(),
            b"\x1b]11;rgb:0101/0202/0303\x1b\\".to_vec(),
        );
        t.feed("\x1b]12;?\x07");
        assert_eq!(
            t.take_response(),
            b"\x1b]12;rgb:7777/8888/9999\x1b\\".to_vec(),
        );
    }

    #[test]
    fn line_at_returns_scrollback_then_grid() {
        let mut t = Terminal::new(5, 2, 100);
        t.feed("AAAAA\r\nBBBBB\r\nCCCCC\r\nDDDDD");
        // scrollback = [AAAAA, BBBBB], grid = [CCCCC, DDDDD]
        assert_eq!(t.scrollback_len(), 2);
        assert_eq!(t.line_at(0).unwrap()[0].ch, 'A');
        assert_eq!(t.line_at(1).unwrap()[0].ch, 'B');
        assert_eq!(t.line_at(2).unwrap()[0].ch, 'C');
        assert_eq!(t.line_at(3).unwrap()[0].ch, 'D');
        assert!(t.line_at(4).is_none());
        assert!(t.line_at(-1).is_none());
    }

    #[test]
    fn visual_to_abs_line_matches_extended_cell() {
        let mut t = Terminal::new(5, 2, 100);
        t.feed("AAAAA\r\nBBBBB\r\nCCCCC\r\n");
        t.scroll_up(1);
        // Visual row 0 = newest scrollback (BBBBB) at abs index 1.
        let abs = t.visual_to_abs_line(0);
        assert_eq!(t.line_at(abs).unwrap()[0].ch, 'B');
        let abs = t.visual_to_abs_line(1);
        assert_eq!(t.line_at(abs).unwrap()[0].ch, 'C');
    }

    #[test]
    fn xtgettcap_replies_with_known_caps() {
        let mut t = Terminal::new(5, 3, 100);
        // "Co" = 0x43 0x6f -> "436f"; "ku" = "6b75". Both are known.
        t.feed("\x1bP+q436f;6b75\x1b\\");
        let reply = String::from_utf8(t.take_response()).unwrap();
        // Single 1+r DCS containing both name=value pairs.
        assert!(reply.starts_with("\x1bP1+r"), "got {reply:?}");
        assert!(reply.ends_with("\x1b\\"), "got {reply:?}");
        // "Co" = "256" → 323536, "ku" = ESC[A → 1b5b41
        assert!(reply.contains("436f=323536"), "got {reply:?}");
        assert!(reply.contains("6b75=1b5b41"), "got {reply:?}");
    }

    #[test]
    fn xtgettcap_replies_with_unknown_separately() {
        let mut t = Terminal::new(5, 3, 100);
        // "ZZ" = "5a5a" — not a real cap.
        t.feed("\x1bP+q5a5a\x1b\\");
        let reply = String::from_utf8(t.take_response()).unwrap();
        assert_eq!(reply, "\x1bP0+r5a5a\x1b\\");
    }

    #[test]
    fn xtgettcap_groups_known_and_unknown() {
        let mut t = Terminal::new(5, 3, 100);
        // "Co" known, "ZZ" unknown.
        t.feed("\x1bP+q436f;5a5a\x1b\\");
        let reply = String::from_utf8(t.take_response()).unwrap();
        assert!(reply.contains("\x1bP1+r436f=323536\x1b\\"));
        assert!(reply.contains("\x1bP0+r5a5a\x1b\\"));
    }

    #[test]
    fn xtgettcap_uppercase_hex_normalizes_in_reply() {
        let mut t = Terminal::new(5, 3, 100);
        t.feed("\x1bP+q436F\x1b\\"); // uppercase F
        let reply = String::from_utf8(t.take_response()).unwrap();
        // Reply hex names are emitted lowercase regardless of query case.
        assert!(reply.contains("436f="), "got {reply:?}");
    }

    #[test]
    fn dcs_without_xtgettcap_prefix_is_ignored() {
        let mut t = Terminal::new(5, 3, 100);
        t.feed("\x1bP$qmhello\x1b\\"); // DECRQSS or similar — not implemented
        assert!(t.take_response().is_empty());
    }

    #[test]
    fn hex_codec_roundtrip() {
        for s in &["Co", "ku", "k1", "RGB", ""] {
            if s.is_empty() {
                continue;
            }
            let h = hex_encode(s);
            let back = hex_decode_ascii(&h).unwrap();
            assert_eq!(&back, s, "roundtrip {s:?}");
        }
        // Odd-length and non-hex are rejected.
        assert!(hex_decode_ascii("abc").is_none());
        assert!(hex_decode_ascii("zz").is_none());
    }

    #[test]
    fn osc_set_title_does_not_reply() {
        let mut t = Terminal::new(5, 3, 100);
        t.feed("\x1b]0;hello\x07");
        assert!(t.take_response().is_empty());
    }

    #[test]
    fn mouse_modes_track_private_set_reset() {
        let mut t = Terminal::new(5, 3, 100);
        t.feed("\x1b[?1000h\x1b[?1006h");
        let mp = t.mouse_protocol();
        assert!(mp.press_release);
        assert!(mp.sgr);
        assert!(mp.enabled());
        t.feed("\x1b[?1000l\x1b[?1006l");
        assert!(!t.mouse_protocol().enabled());
    }

    #[test]
    fn bracketed_paste_mode_tracks() {
        let mut t = Terminal::new(5, 3, 100);
        assert!(!t.bracketed_paste());
        t.feed("\x1b[?2004h");
        assert!(t.bracketed_paste());
        t.feed("\x1b[?2004l");
        assert!(!t.bracketed_paste());
    }

    #[test]
    fn full_reset_clears_app_cursor_keys_and_response() {
        let mut t = Terminal::new(5, 3, 100);
        t.feed("\x1b[?1h\x1b[5n");
        t.feed("\x1bc"); // RIS
        assert!(!t.app_cursor_keys());
        assert!(t.take_response().is_empty());
    }

    #[test]
    fn full_reset() {
        let mut t = Terminal::new(5, 3, 100);
        t.feed("ABC\r\nXYZ\x1b[?25l");
        t.feed("\x1bc");
        assert_eq!(render(&t), "\n\n");
        assert!(t.cursor_visible());
        assert_eq!(t.cursor().row, 0);
        assert_eq!(t.cursor().col, 0);
    }

    #[test]
    fn osc_is_swallowed() {
        let mut t = Terminal::new(10, 1, 100);
        t.feed("\x1b]0;title\x07after");
        assert_eq!(t.row(0).iter().map(|c| c.ch).take(5).collect::<String>(), "after");
    }

    #[test]
    fn prompt_sp_pattern() {
        // The pattern that caused the %-bug: zsh prints '%', pads with spaces,
        // then CR and CSI K to wipe it before printing the real prompt.
        let mut t = Terminal::new(10, 2, 100);
        t.feed("%         \r\x1b[K$ ");
        assert_eq!(t.row(0).iter().map(|c| c.ch).collect::<String>(), "$         ");
        assert_eq!(t.cursor().row, 0);
        assert_eq!(t.cursor().col, 2);
    }

    #[test]
    fn resize_preserves_top_left_content() {
        let mut t = Terminal::new(5, 3, 100);
        t.feed("HELLO");
        t.resize(3, 3);
        assert_eq!(t.row(0).iter().map(|c| c.ch).collect::<String>(), "HEL");
        assert_eq!(t.cols, 3);
    }

    /// Helper: read a row of cells back as a trimmed string, for legibility
    /// in the resize-reflow assertions below.
    fn row_string(cells: &[Cell]) -> String {
        cells.iter().map(|c| c.ch).collect::<String>().trim_end().to_string()
    }

    #[test]
    fn resize_vertical_shrink_spills_top_rows_into_scrollback() {
        let mut t = Terminal::new(10, 5, 100);
        t.feed("L1\r\nL2\r\nL3\r\nL4\r\nL5");
        // Pre-conditions: top row holds L1, cursor sits on the last grid row.
        assert_eq!(row_string(t.row(0)), "L1");
        assert_eq!(t.cursor().row, 4);
        assert_eq!(t.scrollback_len(), 0);

        t.resize(10, 3);

        // Two top rows spill into scrollback in chronological order.
        assert_eq!(t.scrollback_len(), 2);
        assert_eq!(row_string(t.line_at(0).unwrap()), "L1");
        assert_eq!(row_string(t.line_at(1).unwrap()), "L2");
        // Grid now holds the bottom three lines.
        assert_eq!(row_string(t.row(0)), "L3");
        assert_eq!(row_string(t.row(1)), "L4");
        assert_eq!(row_string(t.row(2)), "L5");
        // Cursor was at row 4; spill of 2 brings it down to row 2 (still on L5).
        assert_eq!(t.cursor().row, 2);
    }

    #[test]
    fn resize_vertical_grow_pulls_from_scrollback() {
        let mut t = Terminal::new(10, 3, 100);
        t.feed("L1\r\nL2\r\nL3\r\nL4\r\nL5");
        // Pre-conditions: 2 rows already in scrollback, cursor on L5 (row 2).
        assert_eq!(t.scrollback_len(), 2);
        assert_eq!(t.cursor().row, 2);

        t.resize(10, 5);

        // Scrollback fully drained back into the grid.
        assert_eq!(t.scrollback_len(), 0);
        assert_eq!(row_string(t.row(0)), "L1");
        assert_eq!(row_string(t.row(1)), "L2");
        assert_eq!(row_string(t.row(2)), "L3");
        assert_eq!(row_string(t.row(3)), "L4");
        assert_eq!(row_string(t.row(4)), "L5");
        // Cursor steps down by the pull count (2) to stay on L5.
        assert_eq!(t.cursor().row, 4);
    }

    #[test]
    fn resize_shrink_then_grow_round_trip_preserves_content() {
        // The user-reported regression: shrinking and growing back used to
        // leave blank rows where the prompt had been.
        let mut t = Terminal::new(10, 5, 100);
        t.feed("L1\r\nL2\r\nL3\r\nL4\r\nL5");
        let before = render(&t);
        assert_eq!(t.cursor().row, 4);

        t.resize(10, 3);
        t.resize(10, 5);

        assert_eq!(render(&t), before);
        assert_eq!(t.cursor().row, 4);
        assert_eq!(t.scrollback_len(), 0);
    }

    #[test]
    fn resize_vertical_shrink_evicts_oldest_when_scrollback_full() {
        // scrollback_limit = 1 means a 2-row spill must drop the older line.
        let mut t = Terminal::new(10, 5, 1);
        t.feed("L1\r\nL2\r\nL3\r\nL4\r\nL5");
        assert_eq!(t.scrollback_len(), 0);

        t.resize(10, 3);

        // Spill of 2 with capacity 1: L1 evicted via pop_front, L2 kept.
        assert_eq!(t.scrollback_len(), 1);
        assert_eq!(row_string(t.line_at(0).unwrap()), "L2");
        // Grid still holds the bottom three lines.
        assert_eq!(row_string(t.row(0)), "L3");
        assert_eq!(row_string(t.row(1)), "L4");
        assert_eq!(row_string(t.row(2)), "L5");
    }

    #[test]
    fn resize_vertical_grow_with_no_scrollback_adds_blank_rows_at_top() {
        let mut t = Terminal::new(10, 3, 100);
        // No input — empty grid, empty scrollback, cursor at origin.
        assert_eq!(t.scrollback_len(), 0);
        assert_eq!(t.cursor().row, 0);

        t.resize(10, 5);

        assert_eq!(t.scrollback_len(), 0);
        // Nothing was pulled — cursor untouched, all rows blank.
        assert_eq!(t.cursor().row, 0);
        for r in 0..5 {
            assert_eq!(row_string(t.row(r)), "");
        }
    }

    #[test]
    fn resize_on_alt_screen_does_not_touch_scrollback() {
        let mut t = Terminal::new(10, 5, 100);
        // Push two lines into primary scrollback before switching screens.
        t.feed("L1\r\nL2\r\nL3\r\nL4\r\nL5\r\nL6\r\nL7");
        let scrollback_before = t.scrollback_len();
        assert_eq!(scrollback_before, 2);

        // Enter alt screen and write something there.
        t.feed("\x1b[?1049h");
        t.feed("alt-text");
        assert_eq!(row_string(t.row(0)), "alt-text");

        // Shrink + grow on the alt screen must not touch primary scrollback.
        t.resize(10, 3);
        assert_eq!(t.scrollback_len(), scrollback_before);
        t.resize(10, 5);
        assert_eq!(t.scrollback_len(), scrollback_before);

        // Alt grid is rebuilt blank on resize (existing alt-screen behavior).
        for r in 0..5 {
            assert_eq!(row_string(t.row(r)), "");
        }
    }

    #[test]
    fn resize_no_op_when_dimensions_match() {
        let mut t = Terminal::new(10, 5, 100);
        t.feed("L1\r\nL2\r\nL3\r\nL4\r\nL5");
        let before_render = render(&t);
        let before_scrollback = t.scrollback_len();
        let before_cursor = t.cursor();
        let before_view = t.view_offset();

        t.resize(10, 5);

        assert_eq!(render(&t), before_render);
        assert_eq!(t.scrollback_len(), before_scrollback);
        assert_eq!(t.cursor().row, before_cursor.row);
        assert_eq!(t.cursor().col, before_cursor.col);
        assert_eq!(t.view_offset(), before_view);
    }

    #[test]
    fn scroll_up_pulls_scrollback_into_view() {
        let mut t = Terminal::new(5, 2, 100);
        // Fill beyond the viewport so a line is forced into scrollback.
        t.feed("AAAAA\r\nBBBBB\r\nCCCCC\r\n");
        assert_eq!(t.scrollback_len(), 2);
        assert!(t.at_bottom());
        assert!(t.scroll_up(1));
        assert_eq!(t.view_offset(), 1);
        // The top visible row is now the newest scrollback line (BBBBB); the
        // bottom visible row is what was previously row 0 of the grid (CCCCC).
        assert_eq!(t.visible_cell(0, 0).ch, 'B');
        assert_eq!(t.visible_cell(1, 0).ch, 'C');
    }

    #[test]
    fn viewport_stays_anchored_as_new_lines_enter_scrollback() {
        let mut t = Terminal::new(5, 2, 100);
        t.feed("AAAAA\r\nBBBBB\r\nCCCCC\r\n");
        // scrollback = [AAAAA, BBBBB], grid = [CCCCC, _]
        assert_eq!(t.scrollback_len(), 2);
        assert!(t.scroll_up(2));
        // Top of viewport pinned to AAAAA; bottom to BBBBB.
        assert_eq!(t.visible_cell(0, 0).ch, 'A');
        assert_eq!(t.visible_cell(1, 0).ch, 'B');

        // New lines stream in. The visible content must not shift.
        t.feed("DDDDD\r\nEEEEE\r\n");
        assert_eq!(t.visible_cell(0, 0).ch, 'A');
        assert_eq!(t.visible_cell(1, 0).ch, 'B');
    }

    #[test]
    fn viewport_pins_to_top_when_oldest_scrollback_evicts() {
        // scrollback_limit = 3 — once full, pop_front evicts oldest.
        let mut t = Terminal::new(5, 2, 3);
        t.feed("AAAAA\r\nBBBBB\r\nCCCCC\r\nDDDDD\r\nEEEEE\r\n");
        // scrollback = [BBBBB, CCCCC, DDDDD] (AAAAA already evicted),
        // grid = [EEEEE, _]
        assert_eq!(t.scrollback_len(), 3);
        assert!(t.scroll_up(3));
        assert_eq!(t.visible_cell(0, 0).ch, 'B');
        assert_eq!(t.visible_cell(1, 0).ch, 'C');

        // Push a new line: BBBBB is evicted. We can no longer show it, so
        // pin to the new oldest line (CCCCC) without indexing past the buffer.
        t.feed("FFFFF\r\n");
        assert_eq!(t.scrollback_len(), 3);
        assert_eq!(t.view_offset(), 3);
        assert_eq!(t.visible_cell(0, 0).ch, 'C');
        assert_eq!(t.visible_cell(1, 0).ch, 'D');
    }

    #[test]
    fn scroll_clamps_at_ends() {
        let mut t = Terminal::new(5, 2, 100);
        t.feed("AAAAA\r\nBBBBB\r\nCCCCC\r\n");
        // Scrolling past the top is a no-op.
        assert!(t.scroll_up(10));
        assert!(!t.scroll_up(1));
        assert!(t.at_top());
        // Scrolling back past bottom is a no-op.
        assert!(t.scroll_down(10));
        assert!(!t.scroll_down(1));
        assert!(t.at_bottom());
    }

    #[test]
    fn scroll_is_noop_on_alt_screen() {
        let mut t = Terminal::new(5, 2, 100);
        t.feed("AAAAA\r\nBBBBB\r\nCCCCC\r\n"); // populate scrollback
        t.feed("\x1b[?1049h"); // enter alt screen
        assert!(!t.scroll_up(1));
        assert_eq!(t.view_offset(), 0);
    }

    #[test]
    fn cursor_visual_row_hidden_when_scrolled_off() {
        let mut t = Terminal::new(5, 3, 100);
        // Push 4 lines into scrollback while parking the cursor on the
        // last row of the live grid (row=2).
        t.feed("AAAAA\r\nBBBBB\r\nCCCCC\r\nDDDDD\r\nEEEEE\r\nFFFFF\r\n");
        assert_eq!(t.cursor_visual_row(), Some(2));
        // 1 line back: visual=3 (== rows). Still drawn at the bottom edge.
        t.scroll_up(1);
        assert_eq!(t.cursor_visual_row(), Some(3));
        // 2 lines back: visual=4 (rows + 1). The renderer's phantom band
        // covers this so the cursor can still intersect the window at the
        // bottom edge — keep drawing.
        t.scroll_up(1);
        assert_eq!(t.cursor_visual_row(), Some(4));
        // 3 lines back: visual=5 (rows + 2). Beyond the phantom band; hide.
        t.scroll_up(1);
        assert_eq!(t.cursor_visual_row(), None);
        t.scroll_to_bottom();
        assert_eq!(t.cursor_visual_row(), Some(2));
    }

    #[test]
    fn extended_cell_matches_visible_inside_viewport() {
        let mut t = Terminal::new(5, 2, 100);
        t.feed("AAAAA\r\nBBBBB\r\nCCCCC\r\n");
        t.scroll_up(1);
        // Within [0, rows), extended_cell must agree with visible_cell.
        for r in 0..t.rows {
            for c in 0..t.cols {
                assert_eq!(
                    t.extended_cell(r as isize, c).map(|x| x.ch),
                    Some(t.visible_cell(r, c).ch),
                );
            }
        }
    }

    #[test]
    fn extended_cell_above_none_when_no_scrollback() {
        let mut t = Terminal::new(5, 3, 100);
        t.feed("AAAAA");
        assert_eq!(t.scrollback_len(), 0);
        assert!(t.extended_cell(-1, 0).is_none());
        assert!(t.extended_cell(-2, 0).is_none());
    }

    #[test]
    fn extended_cell_above_none_when_at_top() {
        let mut t = Terminal::new(5, 2, 100);
        t.feed("AAAAA\r\nBBBBB\r\nCCCCC\r\n");
        assert_eq!(t.scrollback_len(), 2);
        assert!(t.scroll_up(t.scrollback_len()));
        assert!(t.at_top());
        assert!(t.extended_cell(-1, 0).is_none());
        assert!(t.extended_cell(-2, 0).is_none());
    }

    #[test]
    fn extended_cell_above_returns_two_rows_of_history() {
        let mut t = Terminal::new(5, 2, 100);
        // scrollback = [AAAAA (oldest), BBBBB, CCCCC]
        t.feed("AAAAA\r\nBBBBB\r\nCCCCC\r\nDDDDD\r\n");
        assert_eq!(t.scrollback_len(), 3);
        // Scroll up by 1: visual row 0 is CCCCC; -1 = BBBBB; -2 = AAAAA.
        assert!(t.scroll_up(1));
        assert_eq!(t.visible_cell(0, 0).ch, 'C');
        assert_eq!(t.extended_cell(-1, 0).map(|c| c.ch), Some('B'));
        assert_eq!(t.extended_cell(-2, 0).map(|c| c.ch), Some('A'));
        // Only 3 lines exist; nothing further back.
        assert!(t.extended_cell(-3, 0).is_none());
    }

    #[test]
    fn extended_cell_below_none_when_at_bottom() {
        let mut t = Terminal::new(5, 2, 100);
        t.feed("AAAAA\r\nBBBBB\r\nCCCCC\r\n");
        assert!(t.at_bottom());
        assert!(t.extended_cell(t.rows as isize, 0).is_none());
        assert!(t.extended_cell(t.rows as isize + 1, 0).is_none());
    }

    #[test]
    fn extended_cell_below_returns_two_rows_of_live_grid() {
        let mut t = Terminal::new(5, 2, 100);
        // scrollback = [AAAAA, BBBBB]; primary rows = [CCCCC, DDDDD]
        t.feed("AAAAA\r\nBBBBB\r\nCCCCC\r\nDDDDD");
        assert_eq!(t.scrollback_len(), 2);
        // Scroll up 2: visible rows are [AAAAA, BBBBB]; below them are CCCCC then DDDDD.
        assert!(t.scroll_up(2));
        assert_eq!(t.visible_cell(0, 0).ch, 'A');
        assert_eq!(t.visible_cell(1, 0).ch, 'B');
        assert_eq!(t.extended_cell(2, 0).map(|c| c.ch), Some('C'));
        assert_eq!(t.extended_cell(3, 0).map(|c| c.ch), Some('D'));
    }

    #[test]
    fn extended_cell_alt_screen_only_returns_in_bounds() {
        let mut t = Terminal::new(5, 2, 100);
        t.feed("AAAAA\r\nBBBBB\r\nCCCCC\r\n");
        t.feed("\x1b[?1049h");
        // Phantom rows are suppressed on the alt screen.
        assert!(t.extended_cell(-1, 0).is_none());
        assert!(t.extended_cell(-2, 0).is_none());
        assert!(t.extended_cell(t.rows as isize, 0).is_none());
        assert!(t.extended_cell(t.rows as isize + 1, 0).is_none());
        // In-bounds rows still resolve (to the alt grid).
        assert!(t.extended_cell(0, 0).is_some());
        assert!(t.extended_cell((t.rows - 1) as isize, 0).is_some());
    }

    //
    // Image-placement tests (slice 2).
    //

    #[test]
    fn insert_placement_lands_on_active_grid_with_unique_ids() {
        let mut t = Terminal::new(20, 10, 100);
        let a = place(&mut t, 7, 2, 3, 4, 5);
        let b = place(&mut t, 8, 0, 0, 2, 2);
        assert_ne!(a, b);
        let anchors = live_anchors(&t);
        assert_eq!(anchors.len(), 2);
        assert!(anchors.contains(&(7, 2, 3, 4, 5)));
        assert!(anchors.contains(&(8, 0, 0, 2, 2)));
    }

    #[test]
    fn insert_placement_targets_alternate_when_active() {
        let mut t = Terminal::new(20, 10, 100);
        t.feed("\x1b[?1049h"); // switch to alt
        place(&mut t, 1, 0, 0, 2, 2);
        assert_eq!(t.live_placements().len(), 1);
        t.feed("\x1b[?1049l"); // back to primary
        assert!(t.live_placements().is_empty());
        t.feed("\x1b[?1049h");
        // Re-entering alt clears the alt buffer (matches existing alt-cell
        // behaviour); the placement we made here is gone.
        assert!(t.live_placements().is_empty());
    }

    #[test]
    fn grid_clear_drops_placements() {
        let mut t = Terminal::new(20, 10, 100);
        place(&mut t, 1, 0, 0, 2, 2);
        place(&mut t, 2, 5, 5, 3, 3);
        t.feed("\x1b[2J"); // ED 2 — erase whole screen
        assert!(t.live_placements().is_empty());
    }

    #[test]
    fn cell_erase_within_row_does_not_touch_placements() {
        // EL (erase-in-line) and partial-row clears must not delete images.
        // Kitty / iTerm both keep placements alive across cell-erase ops.
        let mut t = Terminal::new(20, 10, 100);
        place(&mut t, 1, 3, 0, 2, 4);
        t.feed("\x1b[3;1H"); // CUP row 3 col 1 (1-based)
        t.feed("\x1b[2K"); // erase entire line
        assert_eq!(t.live_placements().len(), 1);
    }

    #[test]
    fn ed3_clears_scrollback_placements() {
        let mut t = Terminal::new(20, 5, 100);
        place(&mut t, 1, 0, 0, 2, 2);
        // SU 3 — unconditional scroll-up, doesn't depend on cursor row.
        t.feed("\x1b[3S");
        assert!(t.live_placements().is_empty());
        assert!(!t.scrollback_placements_for_test().is_empty());
        t.feed("\x1b[3J");
        assert!(t.scrollback_placements_for_test().is_empty());
    }

    #[test]
    fn full_reset_clears_all_placements_and_id_state() {
        let mut t = Terminal::new(20, 5, 100);
        let _a = place(&mut t, 1, 0, 0, 1, 1);
        let _b = place(&mut t, 2, 2, 2, 1, 1);
        // Force a scrollback placement too.
        place(&mut t, 3, 0, 0, 1, 1);
        t.feed("\x1b[2S");
        t.feed("\x1bc"); // RIS — full reset
        assert!(t.live_placements().is_empty());
        assert!(t.scrollback_placements_for_test().is_empty());
        // First placement after reset gets id 1 again.
        let id = place(&mut t, 9, 0, 0, 1, 1);
        assert_eq!(id, 1);
    }

    #[test]
    fn scroll_region_up_shifts_intersecting_placements_only() {
        let mut t = Terminal::new(20, 10, 100);
        // Above scroll region — stays put.
        let above_id = place(&mut t, 1, 0, 0, 1, 2);
        // In region — shifts up.
        let in_id = place(&mut t, 2, 5, 0, 2, 2);
        // Below region — stays put.
        let below_id = place(&mut t, 3, 9, 0, 1, 2);
        // Set scroll region to rows 4..=8 (1-based 5..=9) and feed nothing —
        // call the internal scroll directly via a region scroll-up sequence.
        // CSI 5;9 r sets the region; then SU 2 scrolls within it.
        t.feed("\x1b[5;9r\x1b[2S");
        let by_id: std::collections::HashMap<u32, isize> = t
            .live_placements()
            .iter()
            .map(|p| (p.id, p.top_row))
            .collect();
        assert_eq!(by_id[&above_id], 0);
        assert_eq!(by_id[&in_id], 3); // 5 - 2
        assert_eq!(by_id[&below_id], 9);
    }

    #[test]
    fn scroll_region_up_full_screen_evicts_to_scrollback() {
        let mut t = Terminal::new(20, 5, 100);
        let id = place(&mut t, 1, 0, 0, 2, 2);
        // Scroll the whole grid up by 2 — the placement (rows 0,1) fully
        // exits the top. Should land in scrollback at index 0 (oldest).
        t.feed("\x1b[2S");
        assert!(t.live_placements().is_empty());
        let sb = t.scrollback_placements_for_test();
        assert_eq!(sb.len(), 1);
        assert_eq!(sb[0].0, 0); // anchor at oldest scrollback row
        assert_eq!(sb[0].1.id, id);
    }

    // ---- scrollback_placements_in_view: viewport-row mapping ----
    //
    // The renderer uses this to draw images that have scrolled into history
    // when the user scrolls back. The mapping puts scrollback row sb_r at
    // viewport row sb_r - (sb_len - view_off). Tests below pin that down
    // for the three interesting positions plus alt-screen / no-offset
    // short-circuits.

    #[test]
    fn scrollback_in_view_empty_when_not_scrolled() {
        // view_offset == 0: even with promoted placements, nothing renders
        // through this path — the live grid (now empty of them) is what
        // the user sees.
        let mut t = Terminal::new(20, 5, 100);
        place(&mut t, 1, 0, 0, 2, 2);
        t.feed("\x1b[2S");
        assert_eq!(t.view_offset(), 0);
        assert!(t.scrollback_placements_in_view(t.rows).is_empty());
    }

    #[test]
    fn scrollback_in_view_empty_on_alt_screen() {
        // Alt screen has no scrollback. Even if entries existed (they
        // don't — alt promotions are blocked upstream), this accessor
        // refuses to surface them.
        let mut t = Terminal::new(20, 5, 100);
        place(&mut t, 1, 0, 0, 2, 2);
        t.feed("\x1b[2S");
        t.feed("\x1b[?1049h"); // enter alt screen
        assert!(t.on_alt_screen());
        assert!(t.scrollback_placements_in_view(t.rows).is_empty());
    }

    #[test]
    fn scrollback_in_view_maps_anchor_to_viewport_row() {
        // Image lands at oldest scrollback row (0). Scroll back by the
        // full scrollback length and that row sits at the top of the
        // viewport (viewport_row == 0).
        let mut t = Terminal::new(20, 5, 100);
        let id = place(&mut t, 1, 0, 0, 2, 2);
        t.feed("\x1b[2S"); // promote to scrollback, sb_len now 2
        assert_eq!(t.scrollback_len(), 2);
        assert!(t.scroll_up(2));
        let in_view = t.scrollback_placements_in_view(t.rows);
        assert_eq!(in_view.len(), 1);
        assert_eq!(in_view[0].image.0, id);
        assert_eq!(in_view[0].top_row, 0);
        assert_eq!(in_view[0].rows, 2);
    }

    #[test]
    fn scrollback_in_view_filters_two_distinct_rows() {
        // Two placements at distinct scrollback rows. With view_offset=1
        // only the most-recently-evicted (highest scrollback_row) is in
        // the visible scrollback strip; the older one sits above row 0
        // by more than its height and is filtered.
        let mut t = Terminal::new(20, 5, 100);
        place(&mut t, 1, 0, 0, 1, 1);
        t.feed("\x1b[1S"); // promotes first
        place(&mut t, 2, 0, 0, 1, 1);
        t.feed("\x1b[1S"); // promotes second
        assert_eq!(t.scrollback_len(), 2);
        assert!(t.scroll_up(1));
        let in_view = t.scrollback_placements_in_view(t.rows);
        assert_eq!(in_view.len(), 1);
        assert_eq!(in_view[0].image.0, 2);
        assert_eq!(in_view[0].top_row, 0);
    }

    #[test]
    fn scrollback_in_view_straddles_live_boundary() {
        // 2-row image promoted, then scroll back by 1. Its anchor row is
        // one row above the viewport top but bottom_row=1 still spills
        // into the visible area. The renderer relies on this — it draws
        // the whole image, the camera ortho clips above row 0.
        let mut t = Terminal::new(20, 5, 100);
        place(&mut t, 1, 0, 0, 2, 2);
        t.feed("\x1b[2S");
        assert!(t.scroll_up(1));
        let in_view = t.scrollback_placements_in_view(t.rows);
        assert_eq!(in_view.len(), 1);
        // sb_len=2, view_off=1 → shift=-1; sb_row=0 → viewport_row=-1
        assert_eq!(in_view[0].top_row, -1);
        assert_eq!(in_view[0].rows, 2);
    }

    #[test]
    fn scrollback_in_view_filters_above_top() {
        // Scroll back by 1 only, but image was promoted many rows ago.
        // Anchor + height land entirely above viewport row 0 → filtered.
        let mut t = Terminal::new(20, 5, 100);
        place(&mut t, 1, 0, 0, 1, 1);
        t.feed("\x1b[1S"); // promotes (scrollback_row = 0)
        // Push more scrollback so view_offset=1 leaves the placement above.
        // SU evicts the top row of the live grid into scrollback.
        t.feed("\x1b[5S");
        assert!(t.scrollback_len() >= 6);
        assert!(t.scroll_up(1));
        // sb_len>=6, view_off=1 → shift<=-5; sb_row=0 → top_row<=-5;
        // bottom_row = top_row + 1 <= -4 → filtered.
        assert!(t.scrollback_placements_in_view(t.rows).is_empty());
    }

    #[test]
    fn scrollback_in_view_filters_below_grid() {
        // Image at the newest scrollback row. Scroll all the way back: it
        // slides off the bottom of the viewport (top_row >= viewport_rows)
        // and gets filtered.
        let mut t = Terminal::new(20, 5, 100);
        for _ in 0..10 {
            t.feed("\n");
        }
        place(&mut t, 1, 0, 0, 1, 1);
        t.feed("\x1b[1S"); // newest scrollback row
        let sb_len = t.scrollback_len();
        assert!(sb_len > t.rows);
        assert!(t.scroll_up(sb_len));
        // view_off == sb_len → shift = 0. Newest sb_row = sb_len-1, so
        // top_row = sb_len-1 >= viewport_rows → filtered.
        assert!(t.scrollback_placements_in_view(t.rows).is_empty());
    }

    #[test]
    fn scrollback_in_view_walks_back_into_view() {
        // End-to-end: place + scroll into scrollback + walk back via
        // scroll_up, then assert the placement reappears in the in-view
        // list. Mirrors the user flow `kitty +kitten icat ; wheel up`.
        let mut t = Terminal::new(20, 5, 100);
        let id = place(&mut t, 1, 0, 0, 2, 2);
        t.feed("\x1b[3S"); // image is gone from live; now in scrollback
        assert!(t.live_placements().is_empty());
        assert!(t.scrollback_placements_in_view(t.rows).is_empty()); // no scroll yet
        assert!(t.scroll_up(3));
        let in_view = t.scrollback_placements_in_view(t.rows);
        assert_eq!(in_view.len(), 1);
        assert_eq!(in_view[0].image.0, id);
    }

    #[test]
    fn scroll_region_up_straddling_image_stays_with_negative_top() {
        let mut t = Terminal::new(20, 5, 100);
        place(&mut t, 1, 0, 0, 3, 2);
        // Scroll by 1 — image now straddles top of viewport.
        t.feed("\x1b[1S");
        let anchors = live_anchors(&t);
        assert_eq!(anchors.len(), 1);
        assert_eq!(anchors[0].1, -1);
        assert!(t.scrollback_placements_for_test().is_empty());
        // Scroll by 2 more — bottom_row was 2, becomes 0 — fully off.
        t.feed("\x1b[2S");
        assert!(t.live_placements().is_empty());
        let sb = t.scrollback_placements_for_test();
        assert_eq!(sb.len(), 1);
        // First scroll pushed 1 row to scrollback; second scroll pushed 2
        // more. Image was anchored at the very first pushed row.
        assert_eq!(sb[0].0, 0);
    }

    #[test]
    fn scroll_region_down_drops_off_bottom() {
        let mut t = Terminal::new(20, 5, 100);
        place(&mut t, 1, 3, 0, 2, 2); // covers rows 3,4 in a 5-row grid
        // CSI T scrolls the region down by 1; placement shifts to top_row=4
        // (still partially visible — bottom_row=6, top_row=4 < 5 rows).
        t.feed("\x1b[1T");
        assert_eq!(t.live_placements().len(), 1);
        assert_eq!(t.live_placements()[0].top_row, 4);
        // One more — top_row=5 hits fully_off_grid's top_row >= rows test.
        t.feed("\x1b[1T");
        assert!(t.live_placements().is_empty());
    }

    #[test]
    fn scroll_partial_width_region_leaves_placements_alone() {
        let mut t = Terminal::new(20, 10, 100);
        // Enable DECLRMM and set a partial-width margin, then scroll.
        t.feed("\x1b[?69h\x1b[5;15s\x1b[1;10r");
        place(&mut t, 1, 2, 7, 2, 4);
        t.feed("\x1b[2S");
        // Placement unchanged — partial-width scroll skips placements.
        assert_eq!(t.live_placements()[0].top_row, 2);
    }

    #[test]
    fn resize_vertical_shrink_spills_placement_to_scrollback() {
        let mut t = Terminal::new(20, 10, 100);
        place(&mut t, 1, 2, 0, 2, 4); // anchored in rows 2..4
        // Shrink rows 10 → 6. spill = 4 (rows 0..4). Placement at row 2 is
        // anchored in a spilled row, so it should land in scrollback.
        t.resize(20, 6);
        assert!(t.live_placements().is_empty());
        let sb = t.scrollback_placements_for_test();
        assert_eq!(sb.len(), 1);
        // Spilled 4 rows; placement's row 2 is the 3rd-from-oldest of the
        // spilled batch (sb_row = 0 + 2 = 2).
        assert_eq!(sb[0].0, 2);
    }

    #[test]
    fn resize_vertical_shrink_keeps_low_placements_live() {
        let mut t = Terminal::new(20, 10, 100);
        place(&mut t, 1, 7, 0, 2, 4); // rows 7..9
        // Shrink 10 → 6, spill = 4. Placement at row 7 stays live; shifts up
        // by 4 to row 3.
        t.resize(20, 6);
        assert_eq!(t.live_placements().len(), 1);
        assert_eq!(t.live_placements()[0].top_row, 3);
    }

    #[test]
    fn resize_vertical_grow_promotes_scrollback_placement_back() {
        let mut t = Terminal::new(20, 6, 100);
        place(&mut t, 1, 0, 0, 2, 4);
        // Force a single-row scroll so placement straddles, then another to
        // fully evict — but actually simplest: resize-shrink to push it into
        // scrollback, then resize-grow to pull it back.
        t.resize(20, 3); // spill=3, placement (row 0..1) → sb at row 0
        assert_eq!(t.scrollback_placements_for_test().len(), 1);
        // Now grow back. refill = up to scrollback length. The placement at
        // sb row 0 should not be the FIRST refilled, because refill drains
        // the TAIL of scrollback (most recent), not the head.
        t.resize(20, 6);
        // Scrollback had 3 entries (sb rows 0,1,2). refill drains the most
        // recent (rows 1,2 — there are 3 rows but new_rows-old_rows = 3, so
        // all 3 drain). sb_post = 0. Placement at sb_row=0 is in the
        // drained range (>= sb_post=0), so it promotes. Its new top_row =
        // 0 - 0 = 0.
        assert_eq!(t.scrollback_placements_for_test().len(), 0);
        assert_eq!(t.live_placements().len(), 1);
        assert_eq!(t.live_placements()[0].top_row, 0);
    }

    #[test]
    fn resize_horizontal_shrink_drops_placements_past_new_width() {
        let mut t = Terminal::new(20, 5, 100);
        place(&mut t, 1, 0, 2, 1, 4); // cols 2..6
        place(&mut t, 2, 0, 15, 1, 2); // cols 15..17
        t.resize(10, 5); // new cols = 10
        // First survives (left_col=2 < 10, even though right edge=6 fits).
        // Second's left_col=15 >= 10 → fully_off_grid → dropped.
        let images: Vec<u32> = t.live_placements().iter().map(|p| p.image.0).collect();
        assert_eq!(images, vec![1]);
    }

    #[test]
    fn referenced_image_ids_unions_primary_alt_and_scrollback() {
        use std::collections::HashSet;
        let mut t = Terminal::new(10, 5, 100);
        // Primary placements with two distinct images.
        place(&mut t, 10, 0, 0, 1, 2);
        place(&mut t, 20, 0, 0, 1, 2);
        // Scroll one out so it lands in scrollback.
        t.feed("\x1b[1S");
        // Alt screen placement with a third image.
        t.feed("\x1b[?1049h");
        place(&mut t, 30, 0, 0, 1, 1);
        t.feed("\x1b[?1049l");
        // Back on primary: dedupe re-includes 10 and 20 (still live or sb),
        // plus 30 from alt grid.
        let ids: HashSet<u32> = t.referenced_image_ids().iter().map(|i| i.0).collect();
        assert!(ids.contains(&10));
        assert!(ids.contains(&20));
        assert!(ids.contains(&30));
    }

    //
    // iTerm2 OSC 1337 parsing tests (P2.2). The parser only fills the
    // `pending_image_uploads` queue here — cursor advancement and
    // placement creation land in P2.3.
    //

    /// Build a minimal PNG and wrap it in an iTerm2 OSC 1337 `File=…`
    /// payload. `extra` is appended to the args list (e.g. `;width=5`).
    fn iterm_osc(extra: &str) -> String {
        use base64::Engine;
        // 2×2 RGBA PNG — encoded via the `image` crate to stay aligned with
        // what real callers send and what our header peek expects.
        let img = image::RgbaImage::from_pixel(2, 2, image::Rgba([10, 20, 30, 255]));
        let mut png: Vec<u8> = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageOutputFormat::Png)
            .unwrap();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        format!("\x1b]1337;File=inline=1{}:{}\x07", extra, b64)
    }

    #[test]
    fn osc_1337_minimal_payload_queues_one_upload() {
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&iterm_osc(""));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        // Header peek runs synchronously inside `handle_osc_1337` — the
        // 2×2 dimensions of our fixture should round-trip.
        assert_eq!(uploads[0].pixel_size, Some((2, 2)));
        // Auto sizing when no width/height given.
        assert_eq!(uploads[0].width, ImageSizeSpec::Auto);
        assert_eq!(uploads[0].height, ImageSizeSpec::Auto);
        assert!(uploads[0].preserve_aspect);
        assert!(!uploads[0].do_not_move_cursor);
        // Bytes round-trip — re-decoding should give back the same image.
        assert!(crate::images::peek_dimensions(&uploads[0].bytes).is_some());
    }

    #[test]
    fn osc_1337_parses_explicit_cell_sizing() {
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&iterm_osc(";width=10;height=5"));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        assert_eq!(uploads[0].width, ImageSizeSpec::Cells(10));
        assert_eq!(uploads[0].height, ImageSizeSpec::Cells(5));
    }

    #[test]
    fn osc_1337_parses_pixel_and_percent_sizing() {
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&iterm_osc(";width=200px;height=50%"));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        assert_eq!(uploads[0].width, ImageSizeSpec::Pixels(200));
        assert_eq!(uploads[0].height, ImageSizeSpec::Percent(50));
    }

    #[test]
    fn osc_1337_preserve_aspect_off() {
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&iterm_osc(";preserveAspectRatio=0"));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        assert!(!uploads[0].preserve_aspect);
    }

    #[test]
    fn osc_1337_do_not_move_cursor_flag() {
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&iterm_osc(";doNotMoveCursor=1"));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        assert!(uploads[0].do_not_move_cursor);
    }

    #[test]
    fn osc_1337_inline_zero_is_dropped() {
        // inline=0 is download mode in iTerm; we have no download UI, so
        // the OSC must be a clean no-op rather than a partial decode.
        let mut t = Terminal::new(80, 24, 100);
        let payload = iterm_osc(";inline=0");
        // Strip the original `inline=1` so only `inline=0` is present.
        let payload = payload.replace("inline=1;", "");
        t.feed(&payload);
        assert!(t.take_pending_image_uploads().is_empty());
    }

    #[test]
    fn osc_1337_bad_base64_silently_dropped() {
        let mut t = Terminal::new(80, 24, 100);
        // `==` in the middle is not a valid base64 stream.
        t.feed("\x1b]1337;File=inline=1:not!base64!\x07");
        assert!(t.take_pending_image_uploads().is_empty());
    }

    #[test]
    fn osc_1337_missing_colon_silently_dropped() {
        // No `:` separator between args and body → no payload to decode.
        let mut t = Terminal::new(80, 24, 100);
        t.feed("\x1b]1337;File=inline=1;width=5\x07");
        assert!(t.take_pending_image_uploads().is_empty());
    }

    #[test]
    fn osc_1337_non_file_verb_silently_dropped() {
        // iTerm uses OSC 1337 for many things; we only handle File.
        let mut t = Terminal::new(80, 24, 100);
        t.feed("\x1b]1337;SetMark\x07");
        t.feed("\x1b]1337;CursorShape=1\x07");
        assert!(t.take_pending_image_uploads().is_empty());
    }

    #[test]
    fn osc_1337_unknown_keys_ignored_not_failed() {
        // iTerm contract: unknown keys are silently accepted so newer
        // params don't break older parsers.
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&iterm_osc(";futureKey=42;size=100;width=5"));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        assert_eq!(uploads[0].width, ImageSizeSpec::Cells(5));
    }

    #[test]
    fn osc_1337_st_terminator_accepted() {
        // The VT parser accepts BEL (0x07) or ESC \ (ST) — same payload
        // either way. Real iTerm callers use both.
        let mut t = Terminal::new(80, 24, 100);
        let with_bel = iterm_osc("");
        let with_st = with_bel.replace('\x07', "\x1b\\");
        t.feed(&with_st);
        assert_eq!(t.take_pending_image_uploads().len(), 1);
    }

    #[test]
    fn osc_1337_multiple_payloads_in_one_feed_all_queued() {
        let mut t = Terminal::new(80, 24, 100);
        let combo = format!("{}{}", iterm_osc(";width=5"), iterm_osc(";width=10"));
        t.feed(&combo);
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 2);
        assert_eq!(uploads[0].width, ImageSizeSpec::Cells(5));
        assert_eq!(uploads[1].width, ImageSizeSpec::Cells(10));
    }

    #[test]
    fn osc_1337_take_drains_queue() {
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&iterm_osc(""));
        let first = t.take_pending_image_uploads();
        assert_eq!(first.len(), 1);
        // Second take returns empty — pending list is owned-moved out.
        let second = t.take_pending_image_uploads();
        assert!(second.is_empty());
    }

    #[test]
    fn osc_1337_size_parser_handles_all_iterm_forms() {
        assert_eq!(parse_iterm_size("auto"), Some(ImageSizeSpec::Auto));
        assert_eq!(parse_iterm_size("AUTO"), Some(ImageSizeSpec::Auto));
        assert_eq!(parse_iterm_size(""), Some(ImageSizeSpec::Auto));
        assert_eq!(parse_iterm_size("5"), Some(ImageSizeSpec::Cells(5)));
        assert_eq!(parse_iterm_size("100px"), Some(ImageSizeSpec::Pixels(100)));
        assert_eq!(parse_iterm_size("50%"), Some(ImageSizeSpec::Percent(50)));
        // Unsupported units fall through to None — caller substitutes Auto.
        assert_eq!(parse_iterm_size("5em"), None);
        assert_eq!(parse_iterm_size("-1"), None);
    }

    //
    // P2.3: cell-extent math + cursor advance + anchor capture.
    //

    #[test]
    fn compute_cell_extent_explicit_cells_passes_through() {
        let (rows, cols) = compute_cell_extent(
            ImageSizeSpec::Cells(10),
            ImageSizeSpec::Cells(5),
            Some((100, 50)),
            8,
            16,
            80,
            24,
            true,
        );
        assert_eq!((rows, cols), (5, 10));
    }

    #[test]
    fn compute_cell_extent_pixels_ceil_divides() {
        // 100px / 8px cell = 12.5 → ceil → 13 cells.
        let (_rows, cols) = compute_cell_extent(
            ImageSizeSpec::Pixels(100),
            ImageSizeSpec::Auto,
            Some((100, 16)),
            8,
            16,
            80,
            24,
            false,
        );
        assert_eq!(cols, 13);
    }

    #[test]
    fn compute_cell_extent_percent_of_viewport() {
        // 50% of 640px viewport = 320px → ceil-div by 8 = 40 cells.
        let (_rows, cols) = compute_cell_extent(
            ImageSizeSpec::Percent(50),
            ImageSizeSpec::Auto,
            Some((100, 16)),
            8,
            16,
            80, // viewport_cols → viewport_w = 640px
            24,
            false,
        );
        assert_eq!(cols, 40);
    }

    #[test]
    fn compute_cell_extent_auto_uses_image_dims() {
        // 32×48 image, 8×16 cells → 4 cols, 3 rows.
        let (rows, cols) = compute_cell_extent(
            ImageSizeSpec::Auto,
            ImageSizeSpec::Auto,
            Some((32, 48)),
            8,
            16,
            80,
            24,
            true,
        );
        assert_eq!((rows, cols), (3, 4));
    }

    #[test]
    fn compute_cell_extent_preserve_aspect_fills_auto_axis() {
        // width=Cells(10) (=80px), height=Auto, image is 100x50 (aspect 2:1),
        // preserve=true. height_px = 80 * 50/100 = 40 → 40/16 = 2.5 → 3 rows.
        let (rows, cols) = compute_cell_extent(
            ImageSizeSpec::Cells(10),
            ImageSizeSpec::Auto,
            Some((100, 50)),
            8,
            16,
            80,
            24,
            true,
        );
        assert_eq!((rows, cols), (3, 10));
    }

    #[test]
    fn compute_cell_extent_preserve_aspect_does_not_override_explicit() {
        // Both axes explicit → preserve is ignored. iTerm contract.
        let (rows, cols) = compute_cell_extent(
            ImageSizeSpec::Cells(10),
            ImageSizeSpec::Cells(2),
            Some((100, 100)),
            8,
            16,
            80,
            24,
            true,
        );
        assert_eq!((rows, cols), (2, 10));
    }

    #[test]
    fn compute_cell_extent_no_pixel_size_no_explicit_falls_back_to_one_cell() {
        // Worst case — format wasn't recognised by peek_dimensions and the
        // OSC didn't supply sizing. Visible-but-tiny beats a panic.
        let (rows, cols) = compute_cell_extent(
            ImageSizeSpec::Auto,
            ImageSizeSpec::Auto,
            None,
            8,
            16,
            80,
            24,
            true,
        );
        assert_eq!((rows, cols), (1, 1));
    }

    #[test]
    fn osc_1337_advances_cursor_by_image_rows() {
        let mut t = Terminal::new(80, 24, 100);
        // Set known cell size: 8×16. The fixture is a 2×2 PNG; auto sizing
        // gives 1×1 cells.
        t.set_cell_size_px(8, 16);
        // Move cursor to a known row first.
        t.feed("\x1b[5;1H"); // CUP row 5 col 1
        t.feed(&iterm_osc(";width=4;height=3")); // 3 rows × 4 cols
        // Cursor should have moved down 3 rows: row 4 (0-indexed) + 3 = 7.
        assert_eq!(t.cursor().row, 7);
        assert_eq!(t.cursor().col, 0);

        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        // Anchor was captured at the cursor's pre-LF position (row 4 since
        // CUP is 1-based: row 5 → index 4). Col 0 (CUP `;1` → index 0).
        assert_eq!(uploads[0].cell_anchor, (4, 0));
        assert_eq!(uploads[0].cell_extent, (3, 4));
    }

    #[test]
    fn osc_1337_do_not_move_cursor_skips_advance() {
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        t.feed("\x1b[5;1H"); // row 5 col 1 (1-based)
        t.feed(&iterm_osc(";width=4;height=3;doNotMoveCursor=1"));
        // Cursor stays at original position.
        assert_eq!(t.cursor().row, 4);
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads[0].cell_anchor, (4, 0));
        assert_eq!(uploads[0].cell_extent, (3, 4));
    }

    #[test]
    fn osc_1337_near_bottom_scrolls_grid_and_anchor_follows() {
        // Cursor at row 22 (1-based 23) in a 24-row grid, image is 5 rows.
        // 5 LFs from row 22: rows 22→23 is the only non-scrolling LF.
        // 4 LFs scroll. Anchor should be 22 - 4 = 18.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        t.feed("\x1b[23;1H"); // row 23 (1-based) = index 22
        t.feed(&iterm_osc(";width=4;height=5"));
        assert_eq!(t.cursor().row, 23); // pinned at bottom
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads[0].cell_anchor.0, 18);
        // The 4 scrolls also push any prior scrollback-eligible content
        // out — just sanity-check the anchor here.
    }

    //
    // P2.4: remove_placements_with_image — failure-cleanup helper.
    //

    #[test]
    fn remove_placements_with_image_drops_matching_entries_across_grids() {
        let mut t = Terminal::new(40, 10, 100);
        // Two placements referencing image 7 on primary; one referencing
        // image 8 on primary. Switch to alt and add another image-7
        // placement. Switch back, ensure remove(7) drops all three image-7
        // entries (across primary + alt) but leaves image 8 alone.
        place(&mut t, 7, 0, 0, 1, 2);
        place(&mut t, 7, 2, 0, 1, 2);
        place(&mut t, 8, 4, 0, 1, 2);
        t.feed("\x1b[?1049h");
        place(&mut t, 7, 0, 0, 1, 2);
        t.feed("\x1b[?1049l");

        let removed = t.remove_placements_with_image(ImageId(7));
        assert_eq!(removed, 3);
        // image 8 survives on primary.
        let primary_images: Vec<u32> =
            t.live_placements().iter().map(|p| p.image.0).collect();
        assert_eq!(primary_images, vec![8]);
        // alt grid is also cleared of image 7.
        t.feed("\x1b[?1049h");
        assert!(t.live_placements().is_empty());
    }

    #[test]
    fn remove_placements_with_image_clears_scrollback_entries() {
        let mut t = Terminal::new(20, 5, 100);
        place(&mut t, 99, 0, 0, 1, 2);
        // Scroll it into scrollback.
        t.feed("\x1b[1S");
        assert_eq!(t.scrollback_placements_for_test().len(), 1);
        let removed = t.remove_placements_with_image(ImageId(99));
        assert_eq!(removed, 1);
        assert!(t.scrollback_placements_for_test().is_empty());
    }

    #[test]
    fn remove_placements_with_image_unknown_id_is_no_op() {
        let mut t = Terminal::new(20, 5, 100);
        place(&mut t, 1, 0, 0, 1, 1);
        // Removing an id no placement references shouldn't touch the
        // surviving placements.
        let removed = t.remove_placements_with_image(ImageId(42));
        assert_eq!(removed, 0);
        assert_eq!(t.live_placements().len(), 1);
    }

    #[test]
    fn osc_1337_with_no_cell_size_set_still_parses_cleanly() {
        // Default cell size is 1×1 — pixel-spec produces huge cell counts
        // but the math shouldn't panic and the queue should still get an
        // entry so callers can detect the OSC arrived.
        let mut t = Terminal::new(80, 24, 100);
        // Skip set_cell_size_px deliberately.
        t.feed(&iterm_osc(";width=10;height=2"));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        assert_eq!(uploads[0].cell_extent, (2, 10));
    }

    #[test]
    fn keep_placements_in_scrollback_false_drops_on_full_screen_scroll() {
        let mut t = Terminal::new(20, 5, 100);
        t.set_keep_placements_in_scrollback(false);
        place(&mut t, 1, 0, 0, 1, 2);
        t.feed("\x1b[1S");
        // Without retention, the scrolled-off placement is dropped — not
        // promoted to scrollback_placements.
        assert!(t.live_placements().is_empty());
        assert!(t.scrollback_placements_for_test().is_empty());
    }

    #[test]
    fn keep_placements_in_scrollback_false_drops_on_resize_spill() {
        let mut t = Terminal::new(20, 10, 100);
        t.set_keep_placements_in_scrollback(false);
        place(&mut t, 1, 2, 0, 2, 4);
        t.resize(20, 6); // spill = 4, placement at row 2 spills
        assert!(t.live_placements().is_empty());
        assert!(t.scrollback_placements_for_test().is_empty());
    }

    #[test]
    fn set_keep_placements_in_scrollback_false_clears_existing_queue() {
        let mut t = Terminal::new(20, 5, 100);
        place(&mut t, 1, 0, 0, 1, 2);
        t.feed("\x1b[1S"); // placement → scrollback_placements
        assert_eq!(t.scrollback_placements_for_test().len(), 1);
        t.set_keep_placements_in_scrollback(false);
        // Toggle drains the queue — otherwise stale entries would linger
        // until the next scrollback eviction.
        assert!(t.scrollback_placements_for_test().is_empty());
    }

    #[test]
    fn scrollback_eviction_drops_scrollback_placement_at_index_0() {
        let mut t = Terminal::new(10, 3, 2); // scrollback_limit = 2
        place(&mut t, 1, 0, 0, 1, 2);
        // Scroll twice — fills scrollback with 2 entries; our placement
        // (1 row tall, anchored at row 0) lands in scrollback at index 0
        // after the first scroll and stays put through the second.
        t.feed("\x1b[2S");
        assert_eq!(t.scrollback_placements_for_test().len(), 1);
        assert_eq!(t.scrollback_placements_for_test()[0].0, 0);
        // Third scroll: scrollback is at limit, pop_front evicts the row at
        // index 0 — placement anchored there must be dropped.
        t.feed("\x1b[1S");
        assert!(t.scrollback_placements_for_test().is_empty());
    }

    //
    // Placement helper tests (slice 2 follow-ups).
    //

    #[test]
    fn placement_fully_off_grid_each_edge() {
        // Pin every branch of the off-grid check independently. Drifting any
        // one of these to `<` vs `<=` (or `>=` vs `>`) would silently keep a
        // dead placement in the live list across scroll / resize.
        let mk = |top: isize, left: isize, rows: u16, cols: u16| Placement {
            id: 1,
            image: ImageId(1),
            top_row: top,
            left_col: left,
            rows,
            cols,
            z: 0,
        };
        let grid_r = 10usize;
        let grid_c = 8usize;

        // Off top: bottom_row == 0 (top=-1 + rows=1).
        assert!(mk(-1, 0, 1, 1).fully_off_grid(grid_r, grid_c));
        // Boundary on top edge: bottom_row == 1 → barely visible, NOT off.
        assert!(!mk(-1, 0, 2, 1).fully_off_grid(grid_r, grid_c));

        // Off bottom: top_row == grid_rows.
        assert!(mk(grid_r as isize, 0, 1, 1).fully_off_grid(grid_r, grid_c));
        // Boundary: top_row == grid_rows - 1 → last row, still on.
        assert!(!mk(grid_r as isize - 1, 0, 1, 1).fully_off_grid(grid_r, grid_c));

        // Off left: right_col == 0.
        assert!(mk(0, -2, 1, 2).fully_off_grid(grid_r, grid_c));
        // Boundary: right_col == 1 → leftmost column visible.
        assert!(!mk(0, -1, 1, 2).fully_off_grid(grid_r, grid_c));

        // Off right: left_col == grid_cols.
        assert!(mk(0, grid_c as isize, 1, 1).fully_off_grid(grid_r, grid_c));
        // Boundary: left_col == grid_cols - 1 → rightmost column visible.
        assert!(!mk(0, grid_c as isize - 1, 1, 1).fully_off_grid(grid_r, grid_c));

        // Fully inside is the obvious negative case.
        assert!(!mk(2, 2, 1, 1).fully_off_grid(grid_r, grid_c));
    }

    #[test]
    fn placement_rows_intersect_boundaries() {
        // Exclusive-on-top, inclusive-on-bottom semantics: a placement whose
        // bottom_row equals `top` (i.e. lives entirely in rows above the
        // region) must NOT intersect, while top_row==bottom must intersect.
        // Driving scroll-region math from here, a flip would shift the wrong
        // placements during a single-row scroll.
        let mk = |top: isize, rows: u16| Placement {
            id: 1, image: ImageId(1), top_row: top, left_col: 0, rows, cols: 1, z: 0,
        };
        // Region [5..=9].
        // Placement at rows 3..=4 → bottom_row=5 == top → no overlap.
        assert!(!mk(3, 2).rows_intersect(5, 9));
        // Placement at rows 4..=5 → bottom_row=6 > 5 and top_row=4 <= 9 → yes.
        assert!(mk(4, 2).rows_intersect(5, 9));
        // Placement starting exactly at top of region.
        assert!(mk(5, 1).rows_intersect(5, 9));
        // Placement starting exactly at bottom of region (top_row == bottom).
        assert!(mk(9, 3).rows_intersect(5, 9));
        // Placement just past the bottom — top_row=10 > 9 → no.
        assert!(!mk(10, 1).rows_intersect(5, 9));
    }

    #[test]
    fn referenced_image_ids_is_empty_when_no_placements_anywhere() {
        // The renderer feeds this set straight into Store::retain every
        // frame; an empty union must yield an empty set (which evicts
        // everything) rather than e.g. a panic from the HashSet builder.
        let t = Terminal::new(10, 5, 100);
        assert!(t.referenced_image_ids().is_empty());
    }

    #[test]
    fn insert_lines_shifts_placements_down_within_region() {
        // IL routes through scroll_region_down with cursor.row as top — the
        // placement at row 2 should slide down to row 4. Mirrors the
        // existing direct-CSI-T test but via the higher-level IL command.
        let mut t = Terminal::new(20, 10, 100);
        place(&mut t, 1, 2, 0, 1, 4);
        // Cursor to row 1 (1-based) so IL's region top = 0 and the placement
        // at row 2 falls inside.
        t.feed("\x1b[1;1H\x1b[2L"); // CUP 1,1 then IL 2
        assert_eq!(t.live_placements().len(), 1);
        assert_eq!(t.live_placements()[0].top_row, 4);
    }

    #[test]
    fn delete_lines_shifts_placements_up_within_region() {
        // DL routes through scroll_region_up; full-width grid means the
        // shift-and-maybe-drop placement path runs. Placement at row 3
        // shifts up to row 1.
        let mut t = Terminal::new(20, 10, 100);
        place(&mut t, 1, 3, 0, 1, 4);
        t.feed("\x1b[1;1H\x1b[2M"); // CUP 1,1 then DL 2
        assert_eq!(t.live_placements().len(), 1);
        assert_eq!(t.live_placements()[0].top_row, 1);
    }

    #[test]
    fn delete_lines_drops_placement_that_fully_exits_top_of_region() {
        // DL on a full-screen region with cursor at home: a placement
        // entirely within the deleted span should fall off the top. Because
        // DL is NOT scroll_region_up_by (it doesn't push into scrollback),
        // the placement is dropped outright — not promoted.
        let mut t = Terminal::new(20, 10, 100);
        place(&mut t, 1, 0, 0, 2, 4);
        t.feed("\x1b[1;1H\x1b[3M"); // CUP 1,1 then DL 3
        assert!(t.live_placements().is_empty());
        // DL doesn't feed scrollback, so the placement is gone — not
        // promoted like a true SU would do.
        assert!(t.scrollback_placements_for_test().is_empty());
    }

    #[test]
    fn viewport_scroll_down_leaves_placements_alone() {
        // Scrolling the viewport is a pure read-side operation — placements
        // are anchored in grid coords, not visual rows, so view_offset
        // must not perturb them. Regression guard for a future "smart"
        // scroll that accidentally touches Grid::placements.
        let mut t = Terminal::new(20, 5, 100);
        place(&mut t, 1, 2, 0, 1, 2);
        // Push history into scrollback so view_offset has somewhere to go.
        t.feed("\r\nline\r\nline\r\nline\r\nline\r\nline");
        let before: Vec<_> = t
            .live_placements()
            .iter()
            .map(|p| (p.id, p.top_row, p.left_col))
            .collect();
        let _ = t.scroll_up(2);
        let _ = t.scroll_down(1);
        let after: Vec<_> = t
            .live_placements()
            .iter()
            .map(|p| (p.id, p.top_row, p.left_col))
            .collect();
        assert_eq!(before, after);
    }

    #[test]
    fn scrollback_eviction_keeps_remaining_indices_consistent() {
        // Build two scrollback placements at distinct indices, then evict the
        // oldest scrollback row. The surviving placement's index must
        // decrement (not get dropped, not stay stale) so future lookups by
        // sb row still resolve to it.
        let mut t = Terminal::new(10, 3, 4); // scrollback_limit = 4
        place(&mut t, 1, 0, 0, 1, 2); // A at row 0
        place(&mut t, 2, 1, 0, 1, 2); // B at row 1
        // First scroll: A exits to sb row 0, B slides up to live row 0.
        t.feed("\x1b[1S");
        assert_eq!(t.scrollback_placements_for_test().len(), 1);
        // Second scroll: B exits — scrollback now has 2 entries, A at sb_row 0
        // and B at sb_row 1.
        t.feed("\x1b[1S");
        let sb = t.scrollback_placements_for_test();
        assert_eq!(sb.len(), 2);
        let by_image: std::collections::HashMap<u32, isize> =
            sb.iter().map(|(row, p)| (p.image.0, *row)).collect();
        assert_eq!(by_image[&1], 0);
        assert_eq!(by_image[&2], 1);

        // Two more scrolls of empty rows fill scrollback to its limit (4).
        t.feed("\x1b[2S");
        // Now a 5th scroll forces pop_front → sb_row 0 evicted (A dropped),
        // surviving entries decrement: B should be at sb_row 0.
        t.feed("\x1b[1S");
        let sb = t.scrollback_placements_for_test();
        assert_eq!(sb.len(), 1);
        assert_eq!(sb[0].0, 0);
        assert_eq!(sb[0].1.image.0, 2);
    }

    #[test]
    fn resize_shrink_then_grow_round_trips_placement_anchor() {
        // Same placement, shrink-then-grow by the same amount — should land
        // back at (roughly) the original row. Catches a sign-flip in the
        // refill math that would otherwise only show up via the
        // visible-but-wrong-position bug.
        let mut t = Terminal::new(20, 10, 100);
        place(&mut t, 1, 2, 0, 2, 4);
        t.resize(20, 4); // spill = 6 — placement goes to scrollback.
        assert!(t.live_placements().is_empty());
        assert_eq!(t.scrollback_placements_for_test().len(), 1);
        t.resize(20, 10);
        // Refill drains the most-recently-pushed scrollback rows back into
        // the grid; the placement at the oldest spilled row should
        // re-emerge in the live list.
        assert_eq!(t.live_placements().len(), 1);
        // Original top_row was 2; after a 6-row spill the placement landed
        // at sb_row 2, and the refill puts it back at top_row 2.
        assert_eq!(t.live_placements()[0].top_row, 2);
    }

    //
    // P2.5: compute_cell_extent edge cases — bounds, saturation, divide-by-
    // zero guards. Behavior that would silently regress to a panic or a
    // wrong-sized placement on a malformed input.
    //

    #[test]
    fn compute_cell_extent_both_pixels_with_preserve_aspect_uses_explicit() {
        // Explicit beats preserve, even when both axes are explicit and
        // would distort the aspect. iTerm contract — verified end-to-end
        // here since this is a common "looks stretched" footgun.
        let (rows, cols) = compute_cell_extent(
            ImageSizeSpec::Pixels(80),
            ImageSizeSpec::Pixels(160),
            Some((100, 100)), // square image
            8,
            16,
            80,
            24,
            true, // preserve_aspect on
        );
        // 80/8 = 10 cols, 160/16 = 10 rows. Aspect-distorted but explicit.
        assert_eq!((rows, cols), (10, 10));
    }

    #[test]
    fn compute_cell_extent_percent_zero_clamps_to_one_cell() {
        // 0% would naturally produce 0 pixels; the resolve closure has a
        // `.max(1)` so callers can't accidentally request a zero-sized
        // placement that the renderer would skip.
        let (rows, cols) = compute_cell_extent(
            ImageSizeSpec::Percent(0),
            ImageSizeSpec::Percent(0),
            Some((100, 100)),
            8,
            16,
            80,
            24,
            false,
        );
        assert_eq!((rows, cols), (1, 1));
    }

    #[test]
    fn compute_cell_extent_percent_two_hundred_oversizes_past_viewport() {
        // Percent isn't clamped to 100 — `width=200%` yields a placement
        // that extends past the right edge. The grid + renderer handle
        // clipping; the math just produces the raw extent.
        let (_rows, cols) = compute_cell_extent(
            ImageSizeSpec::Percent(200),
            ImageSizeSpec::Auto,
            Some((100, 16)),
            8,
            16,
            80, // viewport_w_px = 640
            24,
            false,
        );
        // 200% of 640 = 1280px → 1280/8 = 160 cells (2x the 80-col grid).
        assert_eq!(cols, 160);
    }

    #[test]
    fn compute_cell_extent_width_past_viewport_still_produces_extent() {
        // `Cells(1000)` on an 80-col grid: the parser doesn't clamp;
        // the grid's off-grid check handles overflow downstream. Pinning
        // that the math passes the raw count through (so future fixes
        // happen in the right place — the grid, not here).
        let (_rows, cols) = compute_cell_extent(
            ImageSizeSpec::Cells(1000),
            ImageSizeSpec::Cells(1),
            Some((100, 100)),
            8,
            16,
            80,
            24,
            false,
        );
        assert_eq!(cols, 1000);
    }

    #[test]
    fn compute_cell_extent_zero_image_dims_with_preserve_does_not_panic() {
        // Some malformed payloads have peek_dimensions returning (0, 0).
        // The preserve_aspect branch divides by ih / iw — the `if iw > 0
        // && ih > 0` guard must hold or we crash on a zero image.
        let (rows, cols) = compute_cell_extent(
            ImageSizeSpec::Auto,
            ImageSizeSpec::Cells(5),
            Some((0, 0)),
            8,
            16,
            80,
            24,
            true,
        );
        // height resolves to 5 cells; width stays Auto with no source
        // (image dim 0 → resolve returns Some(0) → ceil-div to 1).
        assert_eq!(rows, 5);
        assert_eq!(cols, 1);
    }

    #[test]
    fn compute_cell_extent_pixel_inputs_well_above_u16_clamp_to_u16_max() {
        // Pixel inputs that produce far more cells than u16 can hold must
        // clamp at u16::MAX rather than truncate. Stay below the
        // ceil-divide overflow threshold (see the should_panic test below)
        // — pick a value that still produces > 65_535 cells but doesn't
        // overflow `px + cell - 1` in the divisor path.
        let huge = 1_000_000_000u32; // 1B px / 8px-cell = 125M cells → clamps.
        let (rows, cols) = compute_cell_extent(
            ImageSizeSpec::Pixels(huge),
            ImageSizeSpec::Pixels(huge),
            None,
            8,
            16,
            80,
            24,
            false,
        );
        assert_eq!(rows, u16::MAX);
        assert_eq!(cols, u16::MAX);
    }

    // BUG: `(px + cell_w_px - 1) / cell_w_px` in compute_cell_extent
    // overflows in debug builds (and silently wraps in release) when px is
    // within (cell - 1) of u32::MAX. A `saturating_add` or pre-clamp before
    // the ceil-div would fix it. Pinned here as #[should_panic] so the
    // existence of the bug is visible to whoever fixes it — flip this to
    // a positive assertion (u16::MAX clamp) once the saturating fix lands.
    #[test]
    #[should_panic(expected = "attempt to add with overflow")]
    fn compute_cell_extent_pixels_near_u32_max_overflows_today() {
        let _ = compute_cell_extent(
            ImageSizeSpec::Pixels(u32::MAX - 1),
            ImageSizeSpec::Auto,
            None,
            8,
            16,
            80,
            24,
            false,
        );
    }

    #[test]
    fn set_cell_size_px_zero_clamps_to_one() {
        // Caller may pass 0 during a degenerate resize (e.g. window
        // minimized to a 0-px height); the OSC math would divide by zero.
        // The clamp turns that into a tiny-but-valid cell size.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(0, 0);
        // 100% percent against 80 cells of 1px each = 80 cells.
        t.feed(&iterm_osc(";width=100%;height=100%"));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        // With cell_w_px = 1 (clamped from 0), 100% of viewport = 80 cells.
        assert_eq!(uploads[0].cell_extent.1, 80);
    }

    #[test]
    fn set_cell_size_px_change_between_oscs_uses_active_value() {
        // Each OSC computes cell_extent from the cell size active at
        // parse time — a font-size change between OSCs must not retro-
        // active the prior upload.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        t.feed(&iterm_osc(";width=16px;height=16px"));
        // Bump cell size — second OSC should use the new value.
        t.set_cell_size_px(16, 32);
        t.feed(&iterm_osc(";width=16px;height=16px"));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 2);
        // First OSC: 16px / 8px-cell = 2 cols; 16px / 16px-line = 1 row.
        assert_eq!(uploads[0].cell_extent, (1, 2));
        // Second OSC: 16px / 16px-cell = 1 col; 16px / 32px-line = 1 row
        // (ceil-div of 16/32 floors to 0 then `.max(1)` brings it to 1).
        assert_eq!(uploads[1].cell_extent, (1, 1));
    }

    #[test]
    fn osc_1337_many_unknown_keys_with_known_mixed() {
        // Defensive parser: an arbitrary salad of unknown keys mixed with
        // known ones should still yield a clean upload with the known
        // values respected. Real iTerm payloads include `size=`, `type=`,
        // and others we don't model — accept-and-ignore.
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&iterm_osc(
            ";size=12345;type=image/png;futureA=1;width=7;futureB=2;height=3;futureC=hello;preserveAspectRatio=0",
        ));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        assert_eq!(uploads[0].width, ImageSizeSpec::Cells(7));
        assert_eq!(uploads[0].height, ImageSizeSpec::Cells(3));
        assert!(!uploads[0].preserve_aspect);
    }

    #[test]
    fn osc_1337_name_with_invalid_base64_leaves_label_none() {
        // `name=` is best-effort: an invalid base64 value must not abort
        // the whole OSC; the upload still arrives, just with label None.
        let mut t = Terminal::new(80, 24, 100);
        // `iterm_osc` already includes `inline=1`; append a bogus name.
        t.feed(&iterm_osc(";name=!!!notbase64!!!"));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1, "bad name must not drop the OSC");
        assert!(uploads[0].label.is_none());
    }

    #[test]
    fn osc_1337_split_across_feed_calls_still_parses() {
        // The ANSI parser is stateful per-char. An OSC arriving in two
        // (or more) feed() chunks should still produce exactly one
        // upload — the parser accumulates the OSC body until ST/BEL.
        let mut t = Terminal::new(80, 24, 100);
        let osc = iterm_osc(";width=4");
        let mid = osc.len() / 2;
        t.feed(&osc[..mid]);
        // No terminator yet — queue must be empty.
        assert!(t.take_pending_image_uploads().is_empty());
        t.feed(&osc[mid..]);
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1, "chunked OSC should reassemble into one upload");
        assert_eq!(uploads[0].width, ImageSizeSpec::Cells(4));
    }

    #[test]
    fn osc_1337_strips_internal_whitespace_in_base64() {
        // Real iTerm callers wrap base64 at 76 chars. The handler strips
        // ASCII whitespace before decode. Test it at the terminal level
        // (the e2e test in images.rs covers the GPU path too).
        let mut t = Terminal::new(80, 24, 100);
        use base64::Engine;
        let img = image::RgbaImage::from_pixel(2, 2, image::Rgba([0, 0, 0, 255]));
        let mut png: Vec<u8> = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageOutputFormat::Png)
            .unwrap();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        // Sprinkle tabs, spaces, and newlines through the payload.
        let mut wrapped = String::new();
        for (i, ch) in b64.chars().enumerate() {
            if i > 0 && i % 4 == 0 {
                wrapped.push_str("\n\t ");
            }
            wrapped.push(ch);
        }
        let osc = format!("\x1b]1337;File=inline=1:{}\x07", wrapped);
        t.feed(&osc);
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1, "whitespace in base64 must be stripped, not fail decode");
        assert_eq!(uploads[0].pixel_size, Some((2, 2)));
    }

    #[test]
    fn remove_placements_with_image_drops_multiple_placements_in_one_grid() {
        // Same image placed 3 times on a single grid (e.g. an app re-using
        // the same texture). remove_with_image must drop all 3, not just
        // the first match.
        let mut t = Terminal::new(20, 10, 100);
        place(&mut t, 5, 0, 0, 1, 2);
        place(&mut t, 5, 2, 0, 1, 2);
        place(&mut t, 5, 4, 0, 1, 2);
        place(&mut t, 6, 6, 0, 1, 2); // different image — must survive
        assert_eq!(t.live_placements().len(), 4);
        let removed = t.remove_placements_with_image(ImageId(5));
        assert_eq!(removed, 3);
        let surviving: Vec<u32> = t.live_placements().iter().map(|p| p.image.0).collect();
        assert_eq!(surviving, vec![6]);
    }

    #[test]
    fn osc_1337_in_decstbm_scroll_region_anchor_follows_scrolls() {
        // The anchor-compensation math uses cursor row-delta to count
        // scrolls, which should stay robust when a DECSTBM scroll region
        // is active. Cursor at the bottom of a [10..20] region: an image
        // taller than the remaining rows triggers in-region scrolling
        // rather than full-grid scrolling. The anchor must still resolve
        // to the original visual row.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        // DECSTBM: top=10, bottom=20 (1-based). After this, scroll_top=9,
        // scroll_bottom=19. CUP also moves cursor to home of region.
        t.feed("\x1b[10;20r");
        // Move cursor to bottom of region: row 20 (1-based) = index 19.
        t.feed("\x1b[20;1H");
        // Image with 3 rows — all 3 line-feeds at scroll_bottom will scroll
        // the in-region rows up. Cursor stays at row 19.
        t.feed(&iterm_osc(";width=4;height=3"));
        assert_eq!(t.cursor().row, 19, "cursor pinned at scroll_bottom");
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        // 3 line-feeds, 0 cursor advance → scrolls = 3 - 0 = 3. Anchor =
        // original_row 19 - 3 = 16.
        assert_eq!(uploads[0].cell_anchor, (16, 0));
        assert_eq!(uploads[0].cell_extent, (3, 4));
    }

    #[test]
    fn alt_screen_image_isolated_from_primary_referenced_ids_reflects_both() {
        // Per-grid placements + a global referenced_image_ids() union:
        // live_placements() must respect the active screen, while
        // retain()-input must keep both grids' images alive across an
        // alt-screen flip.
        let mut t = Terminal::new(40, 10, 100);
        place(&mut t, 100, 0, 0, 1, 2);
        // Switching to alt clears alt grid (matches text behaviour).
        t.feed("\x1b[?1049h");
        assert!(t.live_placements().is_empty(), "alt is fresh");
        place(&mut t, 200, 0, 0, 1, 2);
        let live_ids_on_alt: Vec<u32> =
            t.live_placements().iter().map(|p| p.image.0).collect();
        assert_eq!(live_ids_on_alt, vec![200]);

        // referenced_image_ids unions BOTH grids — primary's image survives
        // for retain() purposes even while alt is active, so the texture
        // isn't dropped + re-uploaded on every screen flip.
        let refs: std::collections::HashSet<u32> =
            t.referenced_image_ids().iter().map(|i| i.0).collect();
        assert!(refs.contains(&100));
        assert!(refs.contains(&200));

        // Back to primary: alt's image survives in the alt grid (switching
        // back doesn't clear alt — only switching TO alt does).
        t.feed("\x1b[?1049l");
        let live_ids_back: Vec<u32> =
            t.live_placements().iter().map(|p| p.image.0).collect();
        assert_eq!(live_ids_back, vec![100]);
        let refs_back: std::collections::HashSet<u32> =
            t.referenced_image_ids().iter().map(|i| i.0).collect();
        assert!(refs_back.contains(&100));
        assert!(refs_back.contains(&200));
    }

    #[test]
    fn keep_placements_in_scrollback_toggle_cycle_does_not_refill_queue() {
        // Disable drops the queue; re-enable should NOT magically reinstate
        // previously dropped entries (we have nothing to reinstate from).
        // Pins the documented one-way semantics so a future change that
        // tries to be clever about retention doesn't silently resurrect
        // entries.
        let mut t = Terminal::new(20, 5, 100);
        place(&mut t, 1, 0, 0, 1, 2);
        t.feed("\x1b[1S");
        assert_eq!(t.scrollback_placements_for_test().len(), 1);
        t.set_keep_placements_in_scrollback(false);
        assert!(t.scrollback_placements_for_test().is_empty());
        t.set_keep_placements_in_scrollback(true);
        // Re-enable doesn't restore — the dropped entry is gone for good.
        assert!(t.scrollback_placements_for_test().is_empty());
        // New scrolls now repopulate as expected.
        place(&mut t, 2, 0, 0, 1, 2);
        t.feed("\x1b[1S");
        assert_eq!(t.scrollback_placements_for_test().len(), 1);
    }
}
