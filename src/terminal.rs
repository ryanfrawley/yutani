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

    /// Shift rows `[top..=bottom]` up by `n`, filling freed rows at the bottom
    /// with `blank`. No-op if `n == 0`.
    pub fn scroll_region_up(&mut self, top: usize, bottom: usize, n: usize, blank: Cell) {
        let region = bottom - top + 1;
        let n = n.min(region);
        if n == 0 {
            return;
        }
        for r in top..=bottom - n {
            let src = (r + n) * self.cols;
            let dst = r * self.cols;
            self.cells.copy_within(src..src + self.cols, dst);
        }
        for r in bottom + 1 - n..=bottom {
            self.clear_row(r, 0, self.cols, blank);
        }
    }

    /// Shift rows `[top..=bottom]` down by `n`, filling freed rows at the top.
    pub fn scroll_region_down(&mut self, top: usize, bottom: usize, n: usize, blank: Cell) {
        let region = bottom - top + 1;
        let n = n.min(region);
        if n == 0 {
            return;
        }
        for r in (top + n..=bottom).rev() {
            let src = (r - n) * self.cols;
            let dst = r * self.cols;
            self.cells.copy_within(src..src + self.cols, dst);
        }
        for r in top..top + n {
            self.clear_row(r, 0, self.cols, blank);
        }
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
    autowrap: bool,
    cursor_visible: bool,
    parser: ansi::Parser,
    scrollback: VecDeque<Vec<Cell>>,
    scrollback_limit: usize,
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
            autowrap: true,
            cursor_visible: true,
            parser: ansi::Parser::new(),
            scrollback: VecDeque::new(),
            scrollback_limit,
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

    pub fn row(&self, row: usize) -> &[Cell] {
        self.active_grid().row(row)
    }

    pub fn scrollback_len(&self) -> usize {
        self.scrollback.len()
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        if cols == 0 || rows == 0 || (cols == self.cols && rows == self.rows) {
            return;
        }
        let blank = Cell::new(' ', self.cursor.style);
        self.primary = resize_grid(&self.primary, rows, cols, blank);
        self.alternate = Grid::new(rows, cols, blank);
        self.cols = cols;
        self.rows = rows;
        self.cursor.row = self.cursor.row.min(rows - 1);
        self.cursor.col = self.cursor.col.min(cols - 1);
        self.cursor.wrap_pending = false;
        self.scroll_top = 0;
        self.scroll_bottom = rows - 1;
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
                self.active_grid_mut()
                    .scroll_region_down(top, bottom, n as usize, blank);
            }
            Event::SetScrollRegion(top, bottom) => self.set_scroll_region(top, bottom),
            Event::Sgr(params) => self.cursor.style.apply_sgr(&params),
            Event::PrivateModeSet(n) => self.private_mode(n, true),
            Event::PrivateModeReset(n) => self.private_mode(n, false),
            Event::SaveCursor => self.save_cursor(),
            Event::RestoreCursor => self.restore_cursor(),
            Event::FullReset => self.full_reset(),
        }
    }

    fn print(&mut self, ch: char) {
        if self.cursor.wrap_pending && self.autowrap {
            self.cursor.col = 0;
            self.cursor.wrap_pending = false;
            self.line_feed_no_cr();
        }
        let cell = Cell::new(ch, self.cursor.style);
        let row = self.cursor.row;
        let col = self.cursor.col;
        if row < self.rows && col < self.cols {
            self.active_grid_mut().set(row, col, cell);
        }
        if self.cursor.col + 1 >= self.cols {
            if self.autowrap {
                self.cursor.wrap_pending = true;
            }
            // when autowrap is off, cursor sticks at the last column
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
        let grid = self.active_grid_mut();
        match mode {
            0 => {
                grid.clear_row(row, col, grid.cols, blank);
                for r in (row + 1)..grid.rows {
                    grid.clear_row(r, 0, grid.cols, blank);
                }
            }
            1 => {
                for r in 0..row {
                    grid.clear_row(r, 0, grid.cols, blank);
                }
                grid.clear_row(row, 0, col + 1, blank);
            }
            2 | 3 => grid.clear(blank),
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
        // When the scroll region covers the full primary screen, the lines
        // rolling off the top are appended to scrollback.
        let full_region = self.scroll_top == 0 && self.scroll_bottom == self.rows - 1;
        if !self.use_alternate && full_region && self.scrollback_limit > 0 {
            for _ in 0..n.min(self.rows) {
                let line = self.primary.row(self.scroll_top).to_vec();
                if self.scrollback.len() == self.scrollback_limit {
                    self.scrollback.pop_front();
                }
                self.scrollback.push_back(line);
            }
        }
        let blank = self.blank();
        let top = self.scroll_top;
        let bottom = self.scroll_bottom;
        self.active_grid_mut()
            .scroll_region_up(top, bottom, n, blank);
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
            7 => self.autowrap = set,
            25 => self.cursor_visible = set,
            1049 | 1047 | 47 => self.switch_screen(set, code == 1049),
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
        self.autowrap = true;
        self.cursor_visible = true;
        self.scrollback.clear();
    }
}

fn resize_grid(old: &Grid, new_rows: usize, new_cols: usize, blank: Cell) -> Grid {
    let mut g = Grid::new(new_rows, new_cols, blank);
    let rows = old.rows.min(new_rows);
    let cols = old.cols.min(new_cols);
    for r in 0..rows {
        for c in 0..cols {
            g.set(r, c, old.get(r, c));
        }
    }
    g
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
}
