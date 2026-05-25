//! `WindowState` pointer handling: pixel<->cell mapping, the title-bar
//! chrome band, mouse reporting to the PTY, hover-URL tracking, and local
//! text selection (word/line granularity, copy).

use crate::*;

impl WindowState {
    /// 1-based (col, row) form of `pixel_to_visual_cell` for mouse reporting.
    pub(crate) fn pixel_to_cell(&self, px: f64, py: f64) -> (u16, u16) {
        let (c, r) = self.pixel_to_visual_cell(px, py);
        (c as u16 + 1, r as u16 + 1)
    }

    /// True when a window-relative `py` (physical px) falls inside the title
    /// bar / toolbar chrome band at the top of the window. The app draws with
    /// `fullsize_content_view` so terminal content renders behind the
    /// translucent macOS title bar. Pointer events landing in this band are the
    /// user driving the window chrome — dragging the bar, hitting the traffic
    /// lights — and must be swallowed rather than translated into mouse reports
    /// for the shell below, and the cursor must be the arrow rather than the
    /// grid's I-beam. The band tracks the live native title-bar height (see
    /// `chrome_band_px` / `refresh_chrome_band`), not the scroll-animated
    /// decorator offset, because the native title bar doesn't move with scroll.
    pub(crate) fn in_top_toolbar(&self, py: f64) -> bool {
        py_in_top_toolbar(py, self.chrome_band_px)
    }

    /// Recompute `chrome_band_px` from the live native title-bar height.
    ///
    /// The renderer's `WINDOW_PADDING + DECORATOR_HEIGHT` reserve is fixed in
    /// physical px, but the native title bar is a fixed number of *points*, so
    /// on a Retina display it's physically taller than the reserve. Sizing the
    /// band to the reserve left it shorter than the bar, and since macOS
    /// swallows pointer-moved events over the bar, the band's logic never ran
    /// up there — the grid's I-beam stayed frozen over the title bar. Track the
    /// real bar height instead (plus `CHROME_BAND_MARGIN_PX`, so the lowest grid
    /// move we still receive lands inside the band and flips the cursor to the
    /// arrow before the events cut out). Falls back to the reserve when the
    /// query fails or yields something shorter than the reserve (e.g. low-DPI,
    /// where the reserve already comfortably covers the bar).
    pub(crate) fn refresh_chrome_band(&mut self) {
        let reserve = (WINDOW_PADDING + DECORATOR_HEIGHT) as f64;
        self.chrome_band_px =
            chrome_band_from(native_titlebar_height_physical(&self.window), reserve);
        // When the tab bar is hidden, the chrome band is the title
        // bar alone — record it so we can derive the tab bar's height (the
        // difference) while it's shown. `contentLayoutRect` already excludes the
        // bar, so `chrome_band_px` already grew to include it.
        if !native_tab_bar_visible(&self.window) {
            self.titlebar_only_px = self.chrome_band_px;
        }
    }

    /// Extra vertical pixels the native tab bar consumes (0 when
    /// it's hidden). The grid reserves this at the top so cells sit below the
    /// bar rather than behind it.
    pub(crate) fn chrome_extra_top(&self) -> f32 {
        (self.chrome_band_px - self.titlebar_only_px).max(0.0) as f32
    }

    /// Forward a mouse event to the PTY in the host's preferred encoding,
    /// if any tracking mode is enabled. `motion` is set for drag/move events.
    pub(crate) fn report_mouse(&mut self, button: input::MouseButton, press: bool, motion: bool) {
        let mp = self.active_tab().terminal.mouse_protocol();
        if !mp.enabled() {
            return;
        }
        if motion && !mp.button_motion && !mp.any_motion {
            return;
        }
        if motion && mp.button_motion && !mp.any_motion && self.held_button.is_none() {
            return;
        }
        let (col, row) = self.pixel_to_cell(self.mouse_x, self.mouse_y);
        if motion {
            // Coalesce: only report when the cell changes.
            if self.active_tab().last_reported_cell == Some((col, row)) {
                return;
            }
            self.active_tab_mut().last_reported_cell = Some((col, row));
        }
        let bytes = input::encode_mouse(button, col, row, press, motion, mp.sgr, self.modifiers);
        self.write_pty(&bytes);
    }

    /// Cell under a window-pixel coord, in 0-based (col, visual_row) form,
    /// clamped to the grid. Used to anchor and update text selection.
    ///
    /// The renderer puts each row's baseline at `top_offset + (r+1)*lh`, so
    /// row 0's drawn box starts at `top_offset + lh - ascender` rather than
    /// at `top_offset`. We align the hit-test strip with that drawn box;
    /// any in-progress smooth-scroll offset is folded in too so the mapping
    /// stays consistent during sub-line slides.
    pub(crate) fn pixel_to_visual_cell(&self, px: f64, py: f64) -> (usize, isize) {
        let metrics = self.shared.with_font(|f| f.face().size_metrics().unwrap());
        let line_height = ((metrics.ascender - metrics.descender) >> 6) as f64;
        let ascender = (metrics.ascender >> 6) as f64;
        let descender = (metrics.descender >> 6) as f64;
        let bg_h = ascender - descender;
        let cell_w = self.shared.with_font(|f| f.cell_width()) as f64;
        // Mirror the renderer's dynamic decorator offset: full DECORATOR_HEIGHT
        // at both scroll-range boundaries (live grid and top of scrollback),
        // easing to 0 over one line in either direction. Out-of-sync formulas
        // here would drift the hit-test by a row vs. what's actually drawn.
        let view_offset = self.active_tab().terminal.view_offset() as f64;
        let scrollback_len = if self.active_tab().terminal.on_alt_screen() {
            0.0
        } else {
            self.active_tab().terminal.scrollback_len() as f64
        };
        let dist_from_bottom = view_offset * line_height + self.active_tab().scroll_y;
        let dist_from_top = (scrollback_len - view_offset) * line_height - self.active_tab().scroll_y;
        let near = (dist_from_bottom / line_height)
            .min(dist_from_top / line_height)
            .clamp(0.0, 1.0);
        let chrome_offset = DECORATOR_HEIGHT as f64 * (1.0 - near);
        // Strip top = renderer's `baseline - ascender - (lh - bg_h)/2`
        // for row 0, where baseline_0 = WP + chrome + line_height.
        let strip_pad = (line_height - bg_h) * 0.5;
        // Mirror the renderer's tab-bar push-down so the hit-test
        // tracks the offset grid.
        let row_strip_top = WINDOW_PADDING as f64
            + chrome_offset
            + self.chrome_extra_top() as f64
            + line_height
            - ascender
            - strip_pad;
        let col = ((px - WINDOW_PADDING as f64) / cell_w).floor() as i64;
        let row = ((py - row_strip_top - self.active_tab().scroll_y) / line_height).floor() as i64;
        let col = col.clamp(0, self.active_tab().terminal.cols as i64 - 1) as usize;
        let row = row.clamp(0, self.active_tab().terminal.rows as i64 - 1) as isize;
        (col, row)
    }

    /// Pixel coord → absolute (line, col) selection point.
    pub(crate) fn pixel_to_selection_point(&self, px: f64, py: f64) -> (isize, usize) {
        let (col, vrow) = self.pixel_to_visual_cell(px, py);
        (self.active_tab().terminal.visual_to_abs_line(vrow), col)
    }

    /// Recompute the URL under the mouse pointer. Tracks Cmd state so the
    /// underline overlay and pointer cursor only appear while the user is
    /// actually holding the modifier; releasing Cmd clears the hover. Any
    /// state change here flips the system cursor icon and invalidates the
    /// frame so the underline can repaint.
    pub(crate) fn update_hover_url(&mut self) {
        // The pointer is over the title bar / toolbar band — window chrome,
        // not the grid. This method also runs on events that don't move the
        // mouse (PTY output, Cmd press/release, scroll, prompt jumps), and
        // must not flip the chrome's arrow back to the grid's I-beam (or to a
        // Pointer from a URL on the clamped row-0 hit-test) while it sits there.
        // CursorMoved owns the cursor in this band and clears any hovered URL.
        if self.in_top_toolbar(self.mouse_y) {
            return;
        }
        let new = if self.modifiers.super_key() {
            let (col, vrow) = self.pixel_to_visual_cell(self.mouse_x, self.mouse_y);
            let abs_line = self.active_tab().terminal.visual_to_abs_line(vrow);
            find_url_at(&self.active_tab().terminal, abs_line, col)
        } else {
            None
        };
        if new == self.active_tab().hover_url {
            return;
        }
        let icon = if new.is_some() {
            winit::window::CursorIcon::Pointer
        } else {
            winit::window::CursorIcon::Text
        };
        self.window.set_cursor_icon(icon);
        self.active_tab_mut().hover_url = new;
        self.invalidate();
    }

    /// Anchor a new selection at the mouse position. Click count cycles
    /// 1 → 2 → 3 → 1 for click sequences within the threshold on the same
    /// cell, picking Cell / Word / Line granularity respectively.
    pub(crate) fn handle_mouse_press(&mut self) {
        let p = self.pixel_to_selection_point(self.mouse_x, self.mouse_y);
        let now = std::time::Instant::now();
        let continued = self.active_tab()
            .last_click
            .map(|(t, c)| c == p && now.duration_since(t) < DOUBLE_CLICK_THRESHOLD)
            .unwrap_or(false);
        self.active_tab_mut().click_count = if continued { (self.active_tab().click_count % 3) + 1 } else { 1 };
        self.active_tab_mut().last_click = Some((now, p));
        self.active_tab_mut().selection_mode = match self.active_tab().click_count {
            1 => SelectionMode::Cell,
            2 => SelectionMode::Word,
            _ => SelectionMode::Line,
        };
        self.active_tab_mut().press_cell = Some(p);
        self.active_tab_mut().press_pixel = Some((self.mouse_x, self.mouse_y));
        // Word and Line modes show their selection on click. Cell mode waits
        // until the drag exceeds DRAG_THRESHOLD_PX so a plain click doesn't
        // briefly highlight a single character.
        self.active_tab_mut().selection = match self.active_tab().selection_mode {
            SelectionMode::Cell => None,
            _ => self.compute_selection(p, p),
        };
    }

    /// Update the head of the active selection from the current mouse pos.
    pub(crate) fn handle_mouse_drag(&mut self) {
        let Some(p0) = self.active_tab().press_cell else { return };
        if self.active_tab().selection_mode == SelectionMode::Cell && self.active_tab().selection.is_none() {
            let Some((px, py)) = self.active_tab().press_pixel else { return };
            let dx = self.mouse_x - px;
            let dy = self.mouse_y - py;
            if dx * dx + dy * dy < DRAG_THRESHOLD_PX * DRAG_THRESHOLD_PX {
                return;
            }
        }
        let p = self.pixel_to_selection_point(self.mouse_x, self.mouse_y);
        self.active_tab_mut().selection = self.compute_selection(p0, p);
    }

    pub(crate) fn handle_mouse_release(&mut self) {
        self.active_tab_mut().press_cell = None;
        self.active_tab_mut().press_pixel = None;
    }

    /// Build a selection from two cells under the current `selection_mode`.
    /// In Word / Line mode, each end snaps outward to the word or line edge.
    pub(crate) fn compute_selection(&self, a: (isize, usize), b: (isize, usize)) -> Option<Selection> {
        let (start, end) = if a <= b { (a, b) } else { (b, a) };
        let (start, end) = match self.active_tab().selection_mode {
            SelectionMode::Cell => (start, end),
            SelectionMode::Word => (self.word_start(start), self.word_end(end)),
            SelectionMode::Line => {
                let last = self.active_tab().terminal.cols.saturating_sub(1);
                ((start.0, 0), (end.0, last))
            }
        };
        Some(Selection { anchor: start, head: end })
    }

    /// Walk left from `p` while the previous cell is a word char.
    pub(crate) fn word_start(&self, p: (isize, usize)) -> (isize, usize) {
        let Some(line) = self.active_tab().terminal.line_at(p.0) else { return p };
        if p.1 >= line.len() || !is_word_char(line[p.1].ch) {
            return p;
        }
        let mut col = p.1;
        while col > 0 && is_word_char(line[col - 1].ch) {
            col -= 1;
        }
        (p.0, col)
    }

    /// Walk right from `p` while the next cell is a word char.
    pub(crate) fn word_end(&self, p: (isize, usize)) -> (isize, usize) {
        let Some(line) = self.active_tab().terminal.line_at(p.0) else { return p };
        if p.1 >= line.len() || !is_word_char(line[p.1].ch) {
            return p;
        }
        let mut col = p.1;
        while col + 1 < line.len() && is_word_char(line[col + 1].ch) {
            col += 1;
        }
        (p.0, col)
    }

    pub(crate) fn clear_selection(&mut self) -> bool {
        // Reset multi-click bookkeeping too — typing should make the next
        // click count as a fresh single-click.
        self.active_tab_mut().last_click = None;
        self.active_tab_mut().click_count = 0;
        if self.active_tab().selection.is_some() {
            self.active_tab_mut().selection = None;
            true
        } else {
            false
        }
    }

    /// Materialize the current selection as plain text, trimming trailing
    /// whitespace per line and joining with '\n'.
    pub(crate) fn selection_text(&self) -> Option<String> {
        let sel = self.active_tab().selection.as_ref()?;
        let (start, end) = sel.range();
        let mut out = String::new();
        for line in start.0..=end.0 {
            let Some(cells) = self.active_tab().terminal.line_at(line) else { continue };
            let from = if line == start.0 { start.1 } else { 0 };
            let to_inclusive = if line == end.0 { end.1 } else { cells.len().saturating_sub(1) };
            let to = (to_inclusive + 1).min(cells.len());
            let from = from.min(to);
            let row_text: String = cells[from..to].iter().map(|c| c.ch).collect();
            // Trim trailing spaces — selecting a full line shouldn't paste
            // padding into the clipboard.
            let trimmed = row_text.trim_end_matches(' ');
            out.push_str(trimmed);
            if line < end.0 {
                out.push('\n');
            }
        }
        Some(out)
    }

    pub(crate) fn copy_selection(&self) {
        let Some(text) = self.selection_text() else { return };
        if text.is_empty() {
            return;
        }
        match arboard::Clipboard::new().and_then(|mut c| c.set_text(text)) {
            Ok(()) => {}
            Err(e) => eprintln!("clipboard write failed: {e}"),
        }
    }

    /// Select and copy the most recent completed command's output (OSC 133
    /// `OutputStart`..`CommandEnd`). Returns false (no-op) when no completed
    /// command has any output. Drives the Cmd-Shift-O keybinding.
    pub(crate) fn select_last_command_output(&mut self) -> bool {
        let Some((start_line, end_line)) = self.active_tab().terminal.last_command_output_span() else {
            return false;
        };
        let last_col = self.active_tab().terminal.cols.saturating_sub(1);
        self.active_tab_mut().selection = Some(Selection {
            anchor: (start_line, 0),
            head: (end_line, last_col),
        });
        self.active_tab_mut().selection_mode = SelectionMode::Cell;
        self.copy_selection();
        self.invalidate();
        true
    }
}
