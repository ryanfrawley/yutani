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
    /// The fallback floor here — the raw, *unscaled* `WINDOW_PADDING +
    /// DECORATOR_HEIGHT` sum — is fixed in physical px (it is only a
    /// conservative lower bound; the grid's own reserve is DPI-scaled via
    /// `dpi_px`). The native title bar, by contrast, is a fixed number of
    /// *points*, so on a Retina display it's physically taller than the
    /// reserve. Sizing the
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
        // The native bar height is already DPI-correct (points → px); scale the
        // safety margin too so the band's slop is a constant apparent size.
        let margin = dpi_px(CHROME_BAND_MARGIN_PX as f32, self.dpi) as f64;
        self.chrome_band_px =
            chrome_band_from(native_titlebar_height_physical(&self.window), reserve, margin);
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

    /// Refresh the chrome band and, when the native tab bar's height actually
    /// changed, reflow the grid + GPU buffers so cells land below the bar
    /// rather than behind it.
    ///
    /// The tab bar shows or hides as tabs join or leave the group, which
    /// shrinks/grows the usable height — but on macOS that doesn't raise a
    /// `Resized` event: the full-size content view keeps its pixel size, only
    /// `contentLayoutRect` moves. So when a second tab appears, the originating
    /// window (which sees only a focus *loss*, never a resize) would otherwise
    /// keep its bar-free grid geometry and render its prompt stranded behind
    /// the freshly-shown bar. Driving this from the focus/occlusion handlers
    /// catches both that window and the new tab. The change guard keeps an
    /// ordinary app-switch (bar height unchanged) a no-op.
    pub(crate) fn reflow_for_tab_bar(&mut self) {
        let before = self.chrome_extra_top();
        self.refresh_chrome_band();
        if (self.chrome_extra_top() - before).abs() < 0.5 {
            return;
        }
        let metrics = self.with_font(|f| f.metrics());
        let size = WindowState::get_viewport_size(
            self.surface.config.width as f32,
            self.surface.config.height as f32,
            self.with_font(|f| f.cell_width()),
            ((metrics.ascender - metrics.descender) >> 6) as usize,
            self.chrome_extra_top(),
            self.dpi,
        );
        let grid_changed = size.char_width != self.active_tab().terminal.cols
            || size.char_height != self.active_tab().terminal.rows;
        self.active_tab_mut().terminal.resize(size.char_width, size.char_height);
        if grid_changed {
            self.notify_pty_size(size.char_width, size.char_height);
        }
        self.resize_buffers();
        self.active_tab_mut().cursor_anim = None;
        self.invalidate();
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
        let metrics = self.with_font(|f| f.metrics());
        let line_height = ((metrics.ascender - metrics.descender) >> 6) as f64;
        let ascender = (metrics.ascender >> 6) as f64;
        let descender = (metrics.descender >> 6) as f64;
        let bg_h = ascender - descender;
        let cell_w = self.with_font(|f| f.cell_width()) as f64;
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
        let chrome_offset = dpi_px(DECORATOR_HEIGHT, self.dpi) as f64 * (1.0 - near);
        // Strip top = renderer's `baseline - ascender - (lh - bg_h)/2`
        // for row 0, where baseline_0 = WP + chrome + line_height.
        let strip_pad = (line_height - bg_h) * 0.5;
        // Mirror the renderer's tab-bar push-down so the hit-test
        // tracks the offset grid. `WINDOW_PADDING` is DPI-scaled here exactly
        // as the renderer scales it, so the hit-test strip stays aligned with
        // the drawn cells on every backing scale.
        let window_padding = dpi_px(WINDOW_PADDING, self.dpi) as f64;
        let row_strip_top = window_padding
            + chrome_offset
            + self.chrome_extra_top() as f64
            + line_height
            - ascender
            - strip_pad;
        let col = ((px - window_padding) / cell_w).floor() as i64;
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
        self.window.set_cursor(icon);
        self.active_tab_mut().hover_url = new;
        self.invalidate();
    }

    /// Anchor a new selection at the mouse position. Click count cycles
    /// 1 → 2 → 3 → 1 for click sequences within the threshold on the same
    /// cell, picking Cell / Word / Line granularity respectively.
    pub(crate) fn handle_mouse_press(&mut self) {
        let p = self.pixel_to_selection_point(self.mouse_x, self.mouse_y);
        // Shift+click extends the existing selection instead of starting a
        // fresh one. The original drag origin (`press_cell`, kept alive across
        // the release) stays anchored and the clicked cell becomes the moving
        // end — so repeated shift+clicks above or below keep dragging that same
        // tail toward the pointer, regardless of direction. Honors the live
        // `selection_mode`, so a shift+click after a double / triple click
        // extends by word / line. Falls through to a fresh anchor when there's
        // no origin to pivot on (e.g. right after typing cleared the selection).
        if self.modifiers.shift_key() {
            if let Some(anchor) = self.active_tab().press_cell {
                self.active_tab_mut().press_pixel = Some((self.mouse_x, self.mouse_y));
                self.active_tab_mut().selection = self.compute_selection(anchor, p);
                return;
            }
        }
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
            // `dx`/`dy` are physical pixels; DPI-scale the threshold so the
            // click-vs-drag slop is a constant apparent distance across displays.
            let threshold = dpi_px(DRAG_THRESHOLD_PX as f32, self.dpi) as f64;
            if dx * dx + dy * dy < threshold * threshold {
                return;
            }
        }
        let p = self.pixel_to_selection_point(self.mouse_x, self.mouse_y);
        self.active_tab_mut().selection = self.compute_selection(p0, p);
    }

    pub(crate) fn handle_mouse_release(&mut self) {
        // Deliberately keep `press_cell`: it's the selection's origin, and a
        // later shift+click pivots the moving end around it. `press_pixel` only
        // gates the cell-mode click-vs-drag slop on the next press, so drop it.
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
        // click count as a fresh single-click. Drop the stored origin as well
        // so a later shift+click can't pivot around a now-stale cell (the row
        // it named may have scrolled away once the selection is gone).
        self.active_tab_mut().last_click = None;
        self.active_tab_mut().click_count = 0;
        self.active_tab_mut().press_cell = None;
        self.active_tab_mut().press_pixel = None;
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
            // Build the row text. Skip wide-char spacer cells — the lead cell
            // already carries the real character, so a copied 中 / 🚀 is one
            // codepoint, not the character plus a phantom. Cells holding a
            // grapheme cluster (emoji ZWJ/flag/skin-tone, base + combining
            // marks) copy the whole cluster string, not just the lead codepoint.
            let mut row_text = String::new();
            for cell in &cells[from..to] {
                if cell.is_wide_spacer() {
                    continue;
                }
                match cell.cluster {
                    Some(id) => {
                        row_text.push_str(self.active_tab().terminal.cluster_str(id).unwrap_or(""))
                    }
                    None => row_text.push(cell.ch),
                }
            }
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

    /// Select the whole buffer — every scrollback row plus the live grid (just
    /// the visible grid on the alt screen, which keeps no scrollback). Uses the
    /// same absolute-line coordinates as drag selection (`0..scrollback_len` is
    /// scrollback, then the grid rows), so `selection_text` materializes it
    /// unchanged. Drives the context menu's Select All.
    pub(crate) fn select_all(&mut self) {
        let term = &self.active_tab().terminal;
        let last_col = term.cols.saturating_sub(1);
        let last_line = if term.on_alt_screen() {
            term.rows as isize - 1
        } else {
            term.scrollback_len() as isize + term.rows as isize - 1
        };
        self.active_tab_mut().selection = Some(Selection {
            anchor: (0, 0),
            head: (last_line.max(0), last_col),
        });
        self.active_tab_mut().selection_mode = SelectionMode::Cell;
        self.invalidate();
    }

    /// Show the native right-click context menu and apply the chosen command.
    /// `link` is the hyperlink under the click, if any — it gates the Open/Copy
    /// Link items and is the target those act on. Runs a modal menu loop
    /// (no-op off macOS), then dispatches the pick against the active tab.
    pub(crate) fn show_context_menu(&mut self, link: Option<HoverUrl>) {
        let cmd = context_menu::show(ContextMenuItems {
            has_selection: self.active_tab().selection.is_some(),
            has_link: link.is_some(),
        });
        let Some(cmd) = cmd else { return };
        match cmd {
            ContextMenuCommand::Copy => self.copy_selection(),
            ContextMenuCommand::Paste => self.paste_from_clipboard(),
            ContextMenuCommand::SelectAll => self.select_all(),
            ContextMenuCommand::OpenLink => {
                if let Some(hu) = &link {
                    if is_safe_url(&hu.url) {
                        open_url(&hu.url);
                    }
                }
            }
            ContextMenuCommand::CopyLink => {
                if let Some(hu) = &link {
                    match arboard::Clipboard::new().and_then(|mut c| c.set_text(hu.url.clone())) {
                        Ok(()) => {}
                        Err(e) => eprintln!("clipboard write failed: {e}"),
                    }
                }
            }
            // Clear screen + scrollback, homing the cursor — fed through the
            // parser exactly as shell output would arrive, so the model stays
            // consistent without writing to the shell's stdin. Mirrors the
            // Cmd-K behavior other macOS terminals ship.
            ContextMenuCommand::Clear => {
                self.active_tab_mut().terminal.feed("\x1b[H\x1b[2J\x1b[3J");
                self.clear_selection();
                self.invalidate();
            }
        }
    }
}
