/// Where a cell's foreground / background color came from. Stored
/// alongside the resolved RGBA so a live palette swap (Cmd-Shift-R
/// after editing the color scheme) can sweep every cell and
/// re-resolve `Indexed` slots against the new palette without
/// disturbing `Truecolor` cells (whose RGB was an absolute value
/// from the app, not palette-relative).
///
/// `Indexed(n)` covers the full xterm-256 space: 0..=15 = ANSI
/// (matching what SGR 30..37 / 90..97 / 40..47 / 100..107 / 38;5;0..15
/// produce); 16..=231 = the 6×6×6 cube; 232..=255 = grayscale ramp.
/// `palette::Palette::xterm_256(n)` handles all three ranges.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ColorSource {
    Default,
    Indexed(u8),
    Truecolor,
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Style {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub reverse: bool,
    pub color_bg: Option<[f32; 4]>,
    pub color_fg: Option<[f32; 4]>,
    /// SGR-level provenance of `color_fg`. Tracked so a live palette
    /// swap can rebuild `color_fg` from the new scheme. Always
    /// consistent with `color_fg`: `Default` ↔ `None`, anything else
    /// ↔ `Some(_)`.
    pub color_fg_source: ColorSource,
    pub color_bg_source: ColorSource,
}

impl Style {
    pub fn new() -> Self {
        Self {
            bold: false,
            italic: false,
            underline: false,
            reverse: false,
            color_bg: None,
            color_fg: None,
            color_fg_source: ColorSource::Default,
            color_bg_source: ColorSource::Default,
        }
    }

    pub fn apply_sgr(&mut self, params: &[u16]) {
        if params.is_empty() {
            *self = Style::new();
            return;
        }
        let mut i = 0;
        while i < params.len() {
            match params[i] {
                0 => *self = Style::new(),
                1 => self.bold = true,
                3 => self.italic = true,
                4 => self.underline = true,
                7 => self.reverse = true,
                22 => self.bold = false,
                23 => self.italic = false,
                24 => self.underline = false,
                27 => self.reverse = false,
                30..=37 => {
                    let idx = (params[i] - 30) as u8;
                    self.color_fg = Some(ansi_color(idx, false));
                    self.color_fg_source = ColorSource::Indexed(idx);
                }
                39 => {
                    self.color_fg = None;
                    self.color_fg_source = ColorSource::Default;
                }
                40..=47 => {
                    let idx = (params[i] - 40) as u8;
                    self.color_bg = Some(ansi_color(idx, false));
                    self.color_bg_source = ColorSource::Indexed(idx);
                }
                49 => {
                    self.color_bg = None;
                    self.color_bg_source = ColorSource::Default;
                }
                90..=97 => {
                    // Bright ANSI maps to xterm-256 indices 8..15.
                    let idx = (params[i] - 90) as u8 + 8;
                    self.color_fg = Some(ansi_color(idx - 8, true));
                    self.color_fg_source = ColorSource::Indexed(idx);
                }
                100..=107 => {
                    let idx = (params[i] - 100) as u8 + 8;
                    self.color_bg = Some(ansi_color(idx - 8, true));
                    self.color_bg_source = ColorSource::Indexed(idx);
                }
                38 | 48 => {
                    let target_fg = params[i] == 38;
                    if i + 1 < params.len() {
                        match params[i + 1] {
                            5 if i + 2 < params.len() => {
                                let n = params[i + 2] as u8;
                                let c = xterm_256(n);
                                if target_fg {
                                    self.color_fg = Some(c);
                                    self.color_fg_source = ColorSource::Indexed(n);
                                } else {
                                    self.color_bg = Some(c);
                                    self.color_bg_source = ColorSource::Indexed(n);
                                }
                                i += 2;
                            }
                            2 if i + 4 < params.len() => {
                                let c = rgb(
                                    params[i + 2] as u8,
                                    params[i + 3] as u8,
                                    params[i + 4] as u8,
                                );
                                if target_fg {
                                    self.color_fg = Some(c);
                                    self.color_fg_source = ColorSource::Truecolor;
                                } else {
                                    self.color_bg = Some(c);
                                    self.color_bg_source = ColorSource::Truecolor;
                                }
                                i += 4;
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
            i += 1;
        }
    }

    /// Re-resolve `Indexed` foreground / background colors against the
    /// currently-installed palette. No-op for `Default` (the color
    /// stays `None`) and `Truecolor` (the absolute RGB stays as the
    /// app emitted it). Called by [`crate::terminal::Terminal::reresolve_palette`]
    /// after Cmd-Shift-R loads a new color scheme so already-painted
    /// cells reflect the new palette without re-running their SGR
    /// sequences.
    pub fn reresolve_palette(&mut self) {
        if let ColorSource::Indexed(n) = self.color_fg_source {
            self.color_fg = Some(xterm_256(n));
        }
        if let ColorSource::Indexed(n) = self.color_bg_source {
            self.color_bg = Some(xterm_256(n));
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Cell {
    pub ch: char,
    pub style: Style,
    /// Kitty virtual-placement image id when this cell holds a
    /// U+10EEEE placeholder. The renderer scans for these to
    /// reconstruct the per-image bounding box and draw the tile.
    /// `None` for every other cell (the common case), so the existing
    /// `Cell::new(' ', _)` defaults are unchanged.
    ///
    /// The low 24 bits come from the cell's FG truecolor; the high 8
    /// bits come from an optional third combining diacritic on the
    /// placeholder char (kitty's "image id high byte" extension).
    pub placeholder_image_id: Option<u32>,
    /// 0-based row index within the image's cell grid, set from the
    /// first combining diacritic that follows U+10EEEE. Meaningful
    /// only when `placeholder_image_id` is `Some`.
    pub placeholder_image_row: u16,
    /// 0-based column index within the image's cell grid, set from
    /// the second combining diacritic that follows U+10EEEE.
    /// Meaningful only when `placeholder_image_id` is `Some`.
    pub placeholder_image_col: u16,
}

impl Cell {
    pub fn new(ch: char, style: Style) -> Self {
        Self {
            ch,
            style,
            placeholder_image_id: None,
            placeholder_image_row: 0,
            placeholder_image_col: 0,
        }
    }
}

fn ansi_color(n: u8, bright: bool) -> [f32; 4] {
    crate::palette::get().ansi(n, bright)
}

fn rgb(r: u8, g: u8, b: u8) -> [f32; 4] {
    // SGR truecolor params are sRGB bytes (`\e[38;2;R;G;Bm` matches what web
    // hex codes mean). Linearize them so the GPU's sRGB-encoded write lands
    // on the user's intended pixel value — same reason `palette::rgba_from`
    // does this for scheme files.
    use crate::palette::srgb_to_linear;
    [srgb_to_linear(r), srgb_to_linear(g), srgb_to_linear(b), 1.0]
}

fn xterm_256(n: u8) -> [f32; 4] {
    crate::palette::get().xterm_256(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reset_from_empty() {
        let mut s = Style::new();
        s.bold = true;
        s.color_fg = Some([1.0, 0.0, 0.0, 1.0]);
        s.apply_sgr(&[]);
        assert_eq!(s, Style::new());
    }

    #[test]
    fn reset_from_zero() {
        let mut s = Style::new();
        s.bold = true;
        s.apply_sgr(&[0]);
        assert_eq!(s, Style::new());
    }

    #[test]
    fn bold_italic_underline() {
        let mut s = Style::new();
        s.apply_sgr(&[1, 3, 4]);
        assert!(s.bold);
        assert!(s.italic);
        assert!(s.underline);
        s.apply_sgr(&[22, 23, 24]);
        assert!(!s.bold);
        assert!(!s.italic);
        assert!(!s.underline);
    }

    #[test]
    fn fg_basic_and_default() {
        let mut s = Style::new();
        s.apply_sgr(&[31]);
        assert!(s.color_fg.is_some());
        s.apply_sgr(&[39]);
        assert_eq!(s.color_fg, None);
    }

    #[test]
    fn bg_basic_and_default() {
        let mut s = Style::new();
        s.apply_sgr(&[41]);
        assert!(s.color_bg.is_some());
        s.apply_sgr(&[49]);
        assert_eq!(s.color_bg, None);
    }

    #[test]
    fn bright_fg() {
        let mut s = Style::new();
        s.apply_sgr(&[91]);
        // SGR 91 hits the default bright-red entry in Palette::defaults(),
        // which is still defined in linear space (hand-picked).
        assert_eq!(s.color_fg, Some([1.0, 0.33, 0.33, 1.0]));
    }

    #[test]
    fn truecolor_fg() {
        let mut s = Style::new();
        s.apply_sgr(&[38, 2, 255, 128, 0]);
        let c = s.color_fg.expect("fg set");
        assert_eq!(crate::palette::linear_to_srgb_u8(c[0]), 255);
        assert_eq!(crate::palette::linear_to_srgb_u8(c[1]), 128);
        assert_eq!(crate::palette::linear_to_srgb_u8(c[2]), 0);
        assert_eq!(c[3], 1.0);
    }

    #[test]
    fn truecolor_bg() {
        let mut s = Style::new();
        s.apply_sgr(&[48, 2, 10, 20, 30]);
        let c = s.color_bg.expect("bg set");
        assert_eq!(crate::palette::linear_to_srgb_u8(c[0]), 10);
        assert_eq!(crate::palette::linear_to_srgb_u8(c[1]), 20);
        assert_eq!(crate::palette::linear_to_srgb_u8(c[2]), 30);
        assert_eq!(c[3], 1.0);
    }

    #[test]
    fn xterm256_fg() {
        let mut s = Style::new();
        s.apply_sgr(&[38, 5, 196]);
        assert!(s.color_fg.is_some());
    }

    #[test]
    fn combined_sequence() {
        let mut s = Style::new();
        s.apply_sgr(&[1, 31, 48, 5, 8]);
        assert!(s.bold);
        assert!(s.color_fg.is_some());
        assert!(s.color_bg.is_some());
    }

    #[test]
    fn truncated_extended_sgr_is_ignored() {
        let mut s = Style::new();
        s.apply_sgr(&[38, 2, 10]);
        assert_eq!(s.color_fg, None);
    }

    #[test]
    fn sgr_sets_color_source_alongside_rgba() {
        // Basic ANSI fg.
        let mut s = Style::new();
        s.apply_sgr(&[31]);
        assert_eq!(s.color_fg_source, ColorSource::Indexed(1));
        // Bright ANSI fg maps to indices 8..15.
        let mut s = Style::new();
        s.apply_sgr(&[91]);
        assert_eq!(s.color_fg_source, ColorSource::Indexed(9));
        // 256-color cube preserves the raw index.
        let mut s = Style::new();
        s.apply_sgr(&[38, 5, 196]);
        assert_eq!(s.color_fg_source, ColorSource::Indexed(196));
        // Truecolor flags as such — must NOT be re-resolved later.
        let mut s = Style::new();
        s.apply_sgr(&[38, 2, 200, 100, 50]);
        assert_eq!(s.color_fg_source, ColorSource::Truecolor);
        // bg parallel
        let mut s = Style::new();
        s.apply_sgr(&[41]);
        assert_eq!(s.color_bg_source, ColorSource::Indexed(1));
        // Default clears the source back to Default.
        let mut s = Style::new();
        s.apply_sgr(&[31]);
        s.apply_sgr(&[39]);
        assert_eq!(s.color_fg_source, ColorSource::Default);
        assert_eq!(s.color_fg, None);
    }

    #[test]
    fn reresolve_palette_updates_indexed_rgba_after_palette_swap() {
        // Regression for the live-reload bug: a cell whose fg was set
        // via SGR 31 (red, indexed) carries the OLD palette's red RGBA
        // until reresolve runs.
        let _guard = crate::palette::TEST_LOCK.lock().expect("test lock");
        crate::palette::install(crate::palette::Palette::defaults());
        let mut s = Style::new();
        s.apply_sgr(&[31]);
        let before = s.color_fg.expect("fg set");

        // Swap palette: rewrite ansi[1] (red slot) to bright green.
        let mut new_palette = crate::palette::Palette::defaults();
        new_palette.ansi[1] = [0.0, 1.0, 0.0, 1.0];
        crate::palette::install(new_palette);

        // Before re-resolve, the cell still carries the OLD red.
        assert_eq!(s.color_fg, Some(before));

        // After re-resolve, it tracks the new palette's slot 1.
        s.reresolve_palette();
        assert_eq!(s.color_fg, Some([0.0, 1.0, 0.0, 1.0]));

        // Restore defaults so the next test sees a clean palette.
        crate::palette::install(crate::palette::Palette::defaults());
    }

    #[test]
    fn reresolve_palette_leaves_truecolor_alone() {
        // Truecolor cells carry absolute RGB the app specified — a
        // palette swap must NOT touch them.
        let _guard = crate::palette::TEST_LOCK.lock().expect("test lock");
        crate::palette::install(crate::palette::Palette::defaults());
        let mut s = Style::new();
        s.apply_sgr(&[38, 2, 200, 100, 50]);
        let before = s.color_fg.expect("fg set");

        let mut new_palette = crate::palette::Palette::defaults();
        new_palette.ansi[1] = [0.0, 1.0, 0.0, 1.0];
        crate::palette::install(new_palette);

        s.reresolve_palette();
        assert_eq!(s.color_fg, Some(before), "truecolor must not be re-resolved");

        crate::palette::install(crate::palette::Palette::defaults());
    }

    #[test]
    fn reresolve_palette_leaves_default_alone() {
        let _guard = crate::palette::TEST_LOCK.lock().expect("test lock");
        crate::palette::install(crate::palette::Palette::defaults());
        let mut s = Style::new();
        // No fg SGR — color_fg is None / source Default.
        let mut new_palette = crate::palette::Palette::defaults();
        new_palette.ansi[1] = [0.0, 1.0, 0.0, 1.0];
        crate::palette::install(new_palette);
        s.reresolve_palette();
        assert_eq!(s.color_fg, None);
        assert_eq!(s.color_fg_source, ColorSource::Default);
        crate::palette::install(crate::palette::Palette::defaults());
    }
}
