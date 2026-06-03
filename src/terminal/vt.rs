//! VT/CSI/SGR execution for [`Terminal`].
//!
//! Carved out of the main `terminal.rs` `impl Terminal` block: the `dispatch`
//! match that runs a parsed [`crate::ansi::Event`] and its arm handlers —
//! printing (`print`/`put_cell`/grapheme merging), cursor movement
//! (`backspace`/`tab`/`line_feed`/`move_cursor`/…), erase (`erase_in_display`/
//! `erase_in_line`), the line/character edit ops (`insert_lines`/`delete_lines`/
//! `insert_chars`/`delete_chars`/`erase_chars`), margins (DECSTBM/DECSLRM),
//! DEC `private_mode`, the DCS handler, device-status/color replies, and the
//! screen/cursor state ops (`switch_screen`, `save_cursor`/`restore_cursor`,
//! `full_reset`).
//!
//! The grid/cell model, scrollback, the scroll-region + semantic-mark
//! migration nucleus (`scroll_region_*`, `scroll_marks_*`, the `evict_*`
//! helpers), the OSC dispatcher, window-title and hyperlink handling, and the
//! `reply` response buffer all stay in the parent module; these handlers reach
//! them via `use super::*`, and the ones the parent / sibling submodules /
//! tests still call are `pub(super)`.

use super::*;

impl Terminal {
    pub(super) fn dispatch(&mut self, event: Event) {
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
                self.cursor.col = clamp_cursor_1based(col, self.cols);
                self.cursor.wrap_pending = false;
            }
            Event::CursorVerticalAbs(row) => {
                self.cursor.row = clamp_cursor_1based(row, self.rows);
                self.cursor.wrap_pending = false;
            }
            Event::EraseInDisplay(mode) => self.erase_in_display(mode),
            Event::EraseInLine(mode) => self.erase_in_line(mode),
            Event::ScrollUp(n) => self.scroll_region_up_by(n as usize),
            Event::ScrollDown(n) => self.scroll_region_down_by(n as usize),
            // IND / NEL behave like a line feed (NEL also returns the carriage);
            // RI is the upward counterpart, scrolling the region down at the top.
            Event::Index => self.line_feed_no_cr(),
            Event::NextLine => {
                self.cursor.col = 0;
                self.cursor.wrap_pending = false;
                self.line_feed_no_cr();
            }
            Event::ReverseIndex => self.reverse_index(),
            Event::SetScrollRegion(top, bottom) => self.set_scroll_region(top, bottom),
            Event::SetLeftRightMargin(l, r) => self.set_left_right_margin(l, r),
            Event::InsertLine(n) => self.insert_lines(n as usize),
            Event::DeleteLine(n) => self.delete_lines(n as usize),
            Event::InsertChar(n) => self.insert_chars(n as usize),
            Event::DeleteChar(n) => self.delete_chars(n as usize),
            Event::EraseChar(n) => self.erase_chars(n as usize),
            Event::DeviceStatusReport(code) => self.device_status_report(code),
            Event::PrivateDeviceStatusReport(code) => self.private_device_status_report(code),
            Event::DeviceAttributes => self.reply(b"\x1b[?1;2c"),
            // VT220 ID, firmware version 276, ROM cartridge 0 — what xterm
            // sends. Apps just check that the reply is well-formed.
            Event::SecondaryDeviceAttributes => self.reply(b"\x1b[>0;276;0c"),
            Event::SetCursorStyle(n) => self.cursor_style_dec = n,
            Event::Osc(s) => self.handle_osc(&s),
            Event::Dcs(s) => self.handle_dcs(&s),
            Event::Apc(s) => self.handle_apc(&s),
            Event::XtwinopsQuery(ps) => self.handle_xtwinops_query(ps),
            Event::Sgr(params) => self.cursor.style.apply_sgr(&params),
            Event::PrivateModeSet(n) => self.private_mode(n, true),
            Event::PrivateModeReset(n) => self.private_mode(n, false),
            Event::RequestMode { private, mode } => self.report_mode(private, mode),
            Event::SaveCursor => self.save_cursor(),
            Event::RestoreCursor => self.restore_cursor(),
            Event::FullReset => self.full_reset(),
        }
    }

    fn print(&mut self, ch: char) {
        // Kitty Unicode-placeholder absorption: when the most recent
        // print was U+10EEEE, the next up-to-three diacritics encode
        // position (row, col, id-high-byte) and must attach to that
        // cell instead of spawning their own. Without this, every
        // diacritic lands in its own cell with no glyph — the
        // user-visible "bunch of characters without glyphs" symptom
        // under tmux, where kitten icat falls back to the placeholder
        // protocol.
        if let Some(state) = self.placeholder_decode {
            if let Some(index) = kitty_placeholder_diacritic_index(ch) {
                self.apply_placeholder_diacritic(state, index);
                return;
            }
        }
        self.placeholder_decode = None;

        // Grapheme-cluster absorption: a ZWJ continuation, variation selector,
        // skin-tone modifier, combining mark, tag, or the second half of a
        // regional-indicator flag pair extends the previous grapheme rather than
        // landing in its own cell — so 👨‍👩‍👧, 👍🏽, ❤️, and 🇯🇵 stay one glyph.
        // Gated on the cursor still sitting right after that grapheme: any
        // CR/LF/CUP since leaves the anchor stale, and a stray combining mark
        // then prints on its own (the pre-existing behavior).
        // Skipped when grapheme clustering is disabled (DEC mode 2027 reset):
        // each codepoint then lands in its own cell, the legacy model some apps
        // assume when computing string widths.
        if self.grapheme_clustering {
            if let Some(a) = self.last_grapheme {
                if self.cursor.row == a.post_row && self.cursor.col == a.post_col {
                    let ri_pair = crate::width::is_regional_indicator(ch) && a.ri_pending;
                    if ri_pair || crate::width::extends_grapheme(a.last, ch) {
                        // A flag pair, or a VS16 forcing emoji presentation onto
                        // a narrow base, promotes a one-cell grapheme to two.
                        let widen = (ri_pair || ch == '\u{FE0F}') && !a.wide;
                        self.merge_grapheme(a, ch, widen);
                        return;
                    }
                }
            }
        }

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

        // Wide characters (CJK, fullwidth forms, emoji) occupy two columns:
        // the lead cell holds the glyph, the next is a `WIDE_SPACER`
        // placeholder. Keeping the grid two columns wide for these matches the
        // shell's own line-editor accounting, so the cursor models stay in
        // lockstep (a mismatch is what smeared pasted emoji into a stray
        // cursor-block). The Kitty placeholder codepoint is treated as narrow
        // regardless so its diacritic-encoded geometry is unaffected.
        let cells = if ch == '\u{10EEEE}' { 1 } else { crate::width::char_cells(ch) };

        // A wide glyph with only one column left before the wrap point can't
        // fit; xterm drops to the next line (leaving that final column blank)
        // before laying it down. Narrow chars still fill that last column.
        if cells == 2 && self.autowrap && self.cursor.col >= wrap_at {
            self.cursor.col = wrap_to;
            self.cursor.wrap_pending = false;
            self.line_feed_no_cr();
        }

        let mut cell = Cell::new(ch, self.cursor.style);
        // Carry the active OSC 8 hyperlink (if any) onto the cell. Kept off
        // `Style` so an SGR reset mid-link doesn't sever it.
        cell.hyperlink = self.cursor.hyperlink;
        // Kitty virtual placement: U+10EEEE is the placeholder
        // codepoint. The image id is encoded in the cell's foreground
        // color (24-bit truecolor); the renderer scans for these
        // cells and reconstructs the per-image bounding box.
        if ch == '\u{10EEEE}' {
            cell.placeholder_image_id = decode_kitty_placeholder_image_id(&cell.style);
        }
        let row = self.cursor.row;
        let col = self.cursor.col;
        if row < self.rows && col < self.cols {
            self.put_cell(row, col, cell);
            // Lay down the trailing spacer for a wide glyph (when there's room;
            // the wrap above guarantees it inside the margins).
            if cells == 2 && col + 1 < self.cols {
                let spacer = Cell::wide_spacer(self.cursor.style);
                self.put_cell(row, col + 1, spacer);
            }
            if ch == '\u{10EEEE}' && cell.placeholder_image_id.is_some() {
                self.placeholder_decode = Some(PlaceholderDecode {
                    cell_row: row,
                    cell_col: col,
                    next_slot: 0,
                });
            }
        }
        // Advance one column per cell the character consumed, honoring the wrap
        // point. The final column sets `wrap_pending` instead of stepping past
        // the margin; intermediate columns of a wide char step normally.
        for _ in 0..cells {
            if self.cursor.col >= wrap_at {
                if self.autowrap {
                    self.cursor.wrap_pending = true;
                }
                // when autowrap is off, cursor sticks at the wrap column
                break;
            } else {
                self.cursor.col += 1;
            }
        }
        // Record this grapheme so a following combining mark / ZWJ / skin tone /
        // flag partner can merge into it. `ri_pending` arms flag pairing when the
        // base is a lone regional indicator. The Kitty placeholder codepoint is
        // excluded: it isn't real text, and its trailing diacritics carry image
        // geometry (consumed by `placeholder_decode`), not cluster content.
        if row < self.rows && col < self.cols && ch != '\u{10EEEE}' {
            self.last_grapheme = Some(GraphemeAnchor {
                row,
                col,
                post_row: self.cursor.row,
                post_col: self.cursor.col,
                last: ch,
                ri_pending: crate::width::is_regional_indicator(ch),
                wide: cells == 2,
            });
        } else {
            self.last_grapheme = None;
        }
    }

    /// Fold codepoint `ch` into the grapheme anchored at `a` — appending it to
    /// the cell's interned cluster string (creating one from the lead char if
    /// this is the first extender). `widen` promotes a one-cell grapheme to two
    /// (a flag's second regional indicator, or a VS16 forcing emoji presentation
    /// onto a narrow base): a spacer is dropped and the cursor steps past it.
    /// Zero-width extenders leave the cursor put.
    fn merge_grapheme(&mut self, a: GraphemeAnchor, ch: char, widen: bool) {
        if a.row < self.rows && a.col < self.cols {
            let cell = self.active_grid().get(a.row, a.col);
            let mut s = match cell.cluster {
                Some(id) => self
                    .clusters
                    .get(id)
                    .map(str::to_string)
                    .unwrap_or_else(|| cell.ch.to_string()),
                None => cell.ch.to_string(),
            };
            s.push(ch);
            let id = self.clusters.intern(&s);
            let mut merged = cell;
            merged.cluster = Some(id);
            self.active_grid_mut().set(a.row, a.col, merged);
        }
        let mut post_col = a.post_col;
        if widen && self.cursor.col < self.cols {
            let spacer = Cell::wide_spacer(self.cursor.style);
            self.put_cell(self.cursor.row, self.cursor.col, spacer);
            if self.cursor.col + 1 < self.cols {
                self.cursor.col += 1;
            }
            post_col = self.cursor.col;
        }
        self.last_grapheme = Some(GraphemeAnchor {
            last: ch,
            ri_pending: false,
            wide: a.wide || widen,
            post_col,
            ..a
        });
    }

    /// Write `cell` at `(row, col)`, clearing any wide-pair partner the write
    /// would orphan. Overwriting the lead half of a wide character blanks its
    /// now-dangling spacer; overwriting a spacer blanks its now-dangling lead.
    /// Without this, partially-overwritten wide glyphs would leave a spacer
    /// that swallows a column or a lead glyph that bleeds into its neighbor.
    fn put_cell(&mut self, row: usize, col: usize, cell: Cell) {
        if row >= self.rows || col >= self.cols {
            return;
        }
        let blank = Cell::new(' ', self.cursor.style);
        let grid = self.active_grid();
        // Overwriting a spacer: its lead is the cell to the left.
        let clear_left = col > 0 && grid.get(row, col).is_wide_spacer();
        // Overwriting a lead whose spacer follows: a spacer at `col + 1`
        // always belongs to a lead at `col`, so replacing this cell orphans
        // it. (When the new cell is itself a wide lead, the caller re-lays a
        // fresh spacer at `col + 1` right after this write.)
        let clear_right = col + 1 < self.cols && grid.get(row, col + 1).is_wide_spacer();
        let g = self.active_grid_mut();
        if clear_left {
            g.set(row, col - 1, blank);
        }
        if clear_right {
            g.set(row, col + 1, blank);
        }
        g.set(row, col, cell);
    }

    /// Apply one decoded Kitty placeholder diacritic to the cell that
    /// the most recent `U+10EEEE` print landed in. `slot` advances
    /// row → col → id-high-byte; after the third diacritic the
    /// absorption state clears (a fourth diacritic in a row will fall
    /// through to the normal print path and behave like any other
    /// combining mark).
    fn apply_placeholder_diacritic(&mut self, mut state: PlaceholderDecode, index: u32) {
        if state.cell_row < self.rows && state.cell_col < self.cols {
            let cell = self.active_grid_mut().get_mut(state.cell_row, state.cell_col);
            match state.next_slot {
                0 => cell.placeholder_image_row = index.min(u16::MAX as u32) as u16,
                1 => cell.placeholder_image_col = index.min(u16::MAX as u32) as u16,
                2 => {
                    if let Some(id) = cell.placeholder_image_id {
                        let high = (index & 0xFF) << 24;
                        cell.placeholder_image_id = Some((id & 0x00FF_FFFF) | high);
                    }
                }
                _ => {}
            }
        }
        state.next_slot = state.next_slot.saturating_add(1);
        self.placeholder_decode = if state.next_slot < 3 { Some(state) } else { None };
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

    pub(super) fn line_feed(&mut self) {
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
        self.cursor.row = clamp_cursor_1based(row, self.rows);
        self.cursor.col = clamp_cursor_1based(col, self.cols);
        self.cursor.wrap_pending = false;
    }

    /// Clamp the scrollback view offset to the available history depth so a
    /// shrinking `scrollback` (eviction) or an over-eager bump can't scroll
    /// past the oldest retained line.
    pub(super) fn clamp_view_offset(&mut self) {
        self.view_offset = self.view_offset.min(self.scrollback.len());
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
            2 => {
                self.active_grid_mut().clear(blank);
                // The marks anchored to the rows we just blanked are stale.
                // (Marks live on the primary grid only.)
                if !self.use_alternate {
                    self.semantic_marks.clear();
                }
            }
            // ED 3 — xterm "Erase Saved Lines": drop the scrollback buffer
            // (and snap the viewport back to the live grid) without touching
            // on-screen content. Used by `clear -x` / `tput E3`.
            3 => {
                self.scrollback.clear();
                self.scrollback_placements.clear();
                // Marks anchored into the cleared scrollback are gone; live
                // marks on the on-screen content stay.
                self.semantic_marks
                    .retain(|m| matches!(m.anchor, MarkAnchor::Live { .. }));
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
        // IL at the top margin is how editors (vim) scroll content back: it
        // shifts [cursor.row, bottom] down. Treat it as a downward scroll of
        // that effective region so the slide can animate (the front end only
        // animates top-anchored regions, so a mid-screen open-line won't).
        let full_width = left == 0 && right == self.cols - 1;
        self.note_alt_region_scroll(false, n, top, bottom, full_width);
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
        // DL at the top margin shifts [cursor.row, bottom] up — the upward
        // counterpart to IL above.
        let full_width = left == 0 && right == self.cols - 1;
        self.note_alt_region_scroll(true, n, top, bottom, full_width);
        // DL never grows scrollback, so content changes per row position —
        // mark the region dirty rather than shifting damage flags.
        self.active_grid_mut()
            .scroll_region_up(top, bottom, left, right, n, blank, false);
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
        let base = grid.row_base(row);
        if col + n <= right {
            grid.cells.copy_within(base + col..base + right + 1 - n, base + col + n);
        }
        for i in col..col + n {
            grid.cells[base + i] = blank;
        }
        // Direct copy_within bypasses the change-gated `set`; the row's content
        // shifted, so mark it dirty for the renderer.
        grid.mark_dirty(row);
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
        let base = grid.row_base(row);
        if col + n <= right {
            grid.cells.copy_within(base + col + n..base + right + 1, base + col);
        }
        for i in right + 1 - n..=right {
            grid.cells[base + i] = blank;
        }
        grid.mark_dirty(row);
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
        let l = margin_param_0based(left, 1);
        let r = margin_param_0based(right, self.cols);
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
        let t = margin_param_0based(top, 1);
        let b = margin_param_0based(bottom, self.rows);
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
            1007 => self.alternate_scroll = set,
            2004 => self.bracketed_paste = set,
            1004 => self.focus_reporting = set,
            // Color-scheme update notifications (Contour mode 2031). Seed the
            // baseline polarity on enable so only a later light/dark flip
            // notifies; clear it on disable.
            2031 => {
                self.color_scheme_notify = set;
                self.last_notified_dark = if set { Some(self.bg_is_dark()) } else { None };
            }
            // Synchronized output: BSU (`h`) / ESU (`l`). The grid keeps
            // updating; the front end gates presentation on this flag.
            2026 => self.sync_update = set,
            // Grapheme clustering: reset (`l`) drops to legacy per-codepoint
            // placement; set (`h`) restores the default cluster absorption.
            2027 => self.grapheme_clustering = set,
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

    /// Current on/off state of a DEC private mode for DECRQM reporting, or
    /// `None` if we don't implement it. Mirrors [`private_mode`]: every code
    /// answered here is one we actually act on, so the report never claims
    /// support for a mode that does nothing.
    fn private_mode_state(&self, code: u16) -> Option<bool> {
        Some(match code {
            1 => self.app_cursor_keys,
            7 => self.autowrap,
            25 => self.cursor_visible,
            69 => self.lrmm_enabled,
            1000 => self.mouse_press_release,
            1002 => self.mouse_button_motion,
            1003 => self.mouse_any_motion,
            1004 => self.focus_reporting,
            1006 => self.mouse_sgr,
            1007 => self.alternate_scroll,
            47 | 1047 | 1049 => self.use_alternate,
            2004 => self.bracketed_paste,
            2026 => self.sync_update,
            2027 => self.grapheme_clustering,
            2031 => self.color_scheme_notify,
            _ => return None,
        })
    }

    /// Reply to a DECRQM request (`CSI [?] Ps $ p`) with a DECRPM report
    /// (`CSI [?] Ps ; Pm $ y`). Pm is 1 = set, 2 = reset, 0 = not recognized.
    /// We implement no ANSI (non-private) modes, so those always report 0.
    fn report_mode(&mut self, private: bool, mode: u16) {
        let pm = if private {
            match self.private_mode_state(mode) {
                Some(true) => 1,
                Some(false) => 2,
                None => 0,
            }
        } else {
            0
        };
        let prefix = if private { "?" } else { "" };
        let s = format!("\x1b[{prefix}{mode};{pm}$y");
        self.pending_response.extend_from_slice(s.as_bytes());
    }

    /// Handle a captured DCS payload. Currently we implement xterm's
    /// XTGETTCAP query (`+q<hex>;<hex>;...`) and tmux's passthrough
    /// wrapper (`tmux;<wrapped>`); other DCS strings are dropped.
    fn handle_dcs(&mut self, s: &str) {
        // tmux passthrough: apps running inside tmux that want to send
        // escape sequences to the OUTER terminal wrap them as
        // `ESC P tmux ; <wrapped> ESC \`. Inside `<wrapped>`, literal
        // ESC bytes are doubled (the ansi.rs parser already
        // un-doubles them in `dcs_esc`). Tmux strips this wrapper and
        // forwards the payload — but if someone cats a tmux-wrapped
        // recording directly into yutani (no tmux in the loop), we'd
        // never reach the inner sequences. Recognize the wrapper
        // here, strip it, and re-feed the body so any APC / CSI /
        // OSC inside dispatches through the normal pipeline.
        if let Some(wrapped) = s.strip_prefix("tmux;") {
            // The wrapped body is itself a sequence of escape codes —
            // re-parse it through a FRESH parser. Reusing
            // `self.parser` would inherit whatever state the outer
            // feed's Phase 1 ended in: when the PTY hands us a chunk
            // that splits a DCS mid-body, the outer parser is in
            // DcsString when handle_dcs gets called (Phase 2 dispatches
            // the prior complete DCS events first). Re-feeding the
            // inner `ESC _G ...` into that stuck state turns the
            // leading ESC into an unrecognized DCS escape, drops the
            // partial accumulator, and the rest of the inner APC
            // prints as plain text — visible as a screenful of base64
            // gibberish from each tmux-wrapped image transmission.
            //
            // A throwaway parser starts in Ground every time, so
            // re-entry is independent of whatever the outer parser is
            // in the middle of. Inner events still dispatch through
            // `self.dispatch`, so they hit the normal pipeline
            // (kitty_chunks accumulator, animation state, etc.).
            let mut inner = crate::ansi::Parser::new();
            let mut events = Vec::new();
            for ch in wrapped.chars() {
                inner.feed(ch, |e| events.push(e));
            }
            for e in events {
                self.dispatch(e);
            }
            return;
        }
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

    pub(super) fn reply_color(&mut self, code: u16, rgb: [u8; 3]) {
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

    // DEC private DSR (`CSI ? Ps n`). `CSI ? 996 n` queries the current
    // color-scheme preference (Contour's mode-2031 extension); we answer with
    // the same `CSI ? 997 ; Ps n` report the change notification uses, derived
    // from the background luminance so the query and the notification always
    // agree. Other private DSR codes are unimplemented and stay silent.
    fn private_device_status_report(&mut self, code: u16) {
        if code == 996 {
            let dark = self.bg_is_dark();
            self.push_color_scheme_report(dark);
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
            // No prompt line editing on the alt screen — drop any stale
            // current-input report so it can't leak across the switch.
            self.current_input = None;
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
        // A screen switch invalidates any in-flight scroll-animation capture
        // and its frozen rows — they belong to the screen we're leaving.
        self.alt_scroll_snapshot = None;
        self.alt_scroll_net = 0;
        self.alt_scroll_region = None;
        self.alt_scroll_poison = false;
        self.alt_anim_departing = None;
        // The newly-active grid's content is wholly different from what was on
        // screen a moment ago; its damage flags are stale. (The renderer also
        // clears its cache on the viewport_key change, but mark the grid too so
        // damage stays self-consistent.)
        self.active_grid_mut().mark_all_dirty();
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
        self.focus_reporting = false;
        // RIS restores the power-on state, where synchronized output is off.
        // Clearing it here also guarantees a BSU that's never followed by an ESU
        // can't keep the front end holding the present across a reset.
        self.sync_update = false;
        // Power-on default has grapheme clustering on.
        self.grapheme_clustering = true;
        self.alternate_scroll = true;
        self.alt_scroll_snapshot = None;
        self.alt_scroll_net = 0;
        self.alt_scroll_region = None;
        self.alt_scroll_poison = false;
        self.alt_anim_departing = None;
        // RIS clears the screen and scrollback; sliding the now-stale departing
        // rows in from the top would briefly show content that no longer
        // exists, so drop any pending scroll-on-output distance too.
        self.primary_scroll_net = 0;
        self.pending_response.clear();
        self.scrollback.clear();
        // Grid::clear already dropped per-grid placements above; also drop
        // scrollback placements since the scrollback rows they anchor to are
        // about to be cleared.
        self.scrollback_placements.clear();
        self.semantic_marks.clear();
        self.next_placement_id = 1;
    }
}
