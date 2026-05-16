use crate::ansi::{self, Event};
use crate::style::{Cell, Style};
use std::collections::VecDeque;

#[derive(Clone)]
pub struct Grid {
    pub cells: Vec<Cell>,
    pub rows: usize,
    pub cols: usize,
}

impl Grid {
    pub fn new(rows: usize, cols: usize, blank: Cell) -> Self {
        Self {
            cells: vec![blank; rows * cols],
            rows,
            cols,
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
    pub fn scroll_region_up(
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
        }
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
                }
                self.scrollback.push_back(line);
            }
            // Match scroll_region_up_by: keep the user's view of historical
            // content stable while new lines stream into scrollback.
            if self.view_offset > 0 {
                self.view_offset = (self.view_offset + spill).min(self.scrollback.len());
            }
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
        self.primary = new_primary;
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
        self.active_grid_mut()
            .scroll_region_up(top, bottom, left, right, n, blank);
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
            _ => {}
        }
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
    }
}

/// Decode an even-length lowercase/uppercase hex string into its ASCII form.
/// XTGETTCAP queries name capabilities this way (`Co` → "436f").
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
}
