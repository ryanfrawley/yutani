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
/// A cell's foreground / background color as its *source*, not a resolved RGBA.
/// Resolved to `[f32; 4]` at render time via the live palette ([`CellColor::resolve`]).
/// Storing the source (one enum) instead of the resolved value + provenance
/// halves the per-cell color footprint, lets `Cell` be `Eq` (no floats), and
/// makes a live palette swap automatic (no cached colors to rebuild).
///
/// `Indexed(n)` covers the full xterm-256 space (0..=15 ANSI, 16..=231 cube,
/// 232..=255 grayscale). `Rgb` holds the raw sRGB bytes from `38;2;r;g;b`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum CellColor {
    #[default]
    Default,
    Indexed(u8),
    Rgb([u8; 3]),
}

impl CellColor {
    /// Resolve to linear RGBA. `Default` yields the caller's contextual default
    /// (e.g. the theme fg or bg); `Indexed`/`Rgb` go through the same palette /
    /// sRGB-linearization the parser used to apply eagerly — so the pixel is
    /// identical, just computed lazily.
    #[inline]
    pub fn resolve(self, default: [f32; 4]) -> [f32; 4] {
        match self {
            CellColor::Default => default,
            CellColor::Indexed(n) => crate::palette::get().xterm_256(n),
            CellColor::Rgb([r, g, b]) => rgb(r, g, b),
        }
    }

}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Style {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub reverse: bool,
    pub fg: CellColor,
    pub bg: CellColor,
}

impl Style {
    pub fn new() -> Self {
        Self {
            bold: false,
            italic: false,
            underline: false,
            reverse: false,
            fg: CellColor::Default,
            bg: CellColor::Default,
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
                30..=37 => self.fg = CellColor::Indexed((params[i] - 30) as u8),
                39 => self.fg = CellColor::Default,
                40..=47 => self.bg = CellColor::Indexed((params[i] - 40) as u8),
                49 => self.bg = CellColor::Default,
                // Bright ANSI maps to xterm-256 indices 8..15.
                90..=97 => self.fg = CellColor::Indexed((params[i] - 90) as u8 + 8),
                100..=107 => self.bg = CellColor::Indexed((params[i] - 100) as u8 + 8),
                38 | 48 => {
                    let target_fg = params[i] == 38;
                    if i + 1 < params.len() {
                        match params[i + 1] {
                            5 if i + 2 < params.len() => {
                                let c = CellColor::Indexed(params[i + 2] as u8);
                                if target_fg { self.fg = c; } else { self.bg = c; }
                                i += 2;
                            }
                            2 if i + 4 < params.len() => {
                                let c = CellColor::Rgb([
                                    params[i + 2] as u8,
                                    params[i + 3] as u8,
                                    params[i + 4] as u8,
                                ]);
                                if target_fg { self.fg = c; } else { self.bg = c; }
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

}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
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
    /// OSC 8 explicit-hyperlink target, as an id into the terminal's
    /// `HyperlinkStore` (`crate::terminal::Terminal::hyperlink_uri`).
    /// `Some` for cells printed while a hyperlink was open; `None` for the
    /// common case. Independent of `style` so an SGR reset (`CSI 0 m`) does
    /// not clear the link — only `OSC 8 ; ; ST` does.
    pub hyperlink: Option<std::num::NonZeroU32>,
}

impl Cell {
    pub fn new(ch: char, style: Style) -> Self {
        Self {
            ch,
            style,
            placeholder_image_id: None,
            placeholder_image_row: 0,
            placeholder_image_col: 0,
            hyperlink: None,
        }
    }
}

fn rgb(r: u8, g: u8, b: u8) -> [f32; 4] {
    // SGR truecolor params are sRGB bytes (`\e[38;2;R;G;Bm` matches what web
    // hex codes mean). Linearize them so the GPU's sRGB-encoded write lands
    // on the user's intended pixel value — same reason `palette::rgb_from_value`
    // does this for scheme files.
    use crate::palette::srgb_to_linear;
    [srgb_to_linear(r), srgb_to_linear(g), srgb_to_linear(b), 1.0]
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cell_is_compact() {
        // CellColor stores the color *source*, not a resolved RGBA + provenance,
        // so Cell stays small — it's copied per char on every write and
        // memmoved/scanned in bulk. Guard against regrowth (was ~80 bytes when
        // each color was an `Option<[f32; 4]>` + `ColorSource`).
        let cell = std::mem::size_of::<Cell>();
        let cc = std::mem::size_of::<CellColor>();
        assert!(cell <= 40, "Cell grew to {cell} bytes");
        assert!(cc <= 4, "CellColor is {cc} bytes");
    }

    #[test]
    fn reset_from_empty() {
        let mut s = Style::new();
        s.bold = true;
        s.fg = CellColor::Rgb([255, 0, 0]);
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
        assert_eq!(s.fg, CellColor::Indexed(1));
        s.apply_sgr(&[39]);
        assert_eq!(s.fg, CellColor::Default);
    }

    #[test]
    fn bg_basic_and_default() {
        let mut s = Style::new();
        s.apply_sgr(&[41]);
        assert_eq!(s.bg, CellColor::Indexed(1));
        s.apply_sgr(&[49]);
        assert_eq!(s.bg, CellColor::Default);
    }

    #[test]
    fn bright_fg() {
        let mut s = Style::new();
        s.apply_sgr(&[91]);
        // Bright ANSI 1 -> xterm-256 index 9.
        assert_eq!(s.fg, CellColor::Indexed(9));
    }

    #[test]
    fn truecolor_fg() {
        let mut s = Style::new();
        s.apply_sgr(&[38, 2, 255, 128, 0]);
        assert_eq!(s.fg, CellColor::Rgb([255, 128, 0]));
        // Resolving round-trips the sRGB bytes through linear space.
        let c = s.fg.resolve([0.0, 0.0, 0.0, 1.0]);
        assert_eq!(crate::palette::linear_to_srgb_u8(c[0]), 255);
        assert_eq!(crate::palette::linear_to_srgb_u8(c[1]), 128);
        assert_eq!(crate::palette::linear_to_srgb_u8(c[2]), 0);
        assert_eq!(c[3], 1.0);
    }

    #[test]
    fn truecolor_bg() {
        let mut s = Style::new();
        s.apply_sgr(&[48, 2, 10, 20, 30]);
        assert_eq!(s.bg, CellColor::Rgb([10, 20, 30]));
        let c = s.bg.resolve([0.0, 0.0, 0.0, 1.0]);
        assert_eq!(crate::palette::linear_to_srgb_u8(c[0]), 10);
        assert_eq!(crate::palette::linear_to_srgb_u8(c[1]), 20);
        assert_eq!(crate::palette::linear_to_srgb_u8(c[2]), 30);
        assert_eq!(c[3], 1.0);
    }

    #[test]
    fn xterm256_fg() {
        let mut s = Style::new();
        s.apply_sgr(&[38, 5, 196]);
        assert_eq!(s.fg, CellColor::Indexed(196));
    }

    #[test]
    fn combined_sequence() {
        let mut s = Style::new();
        s.apply_sgr(&[1, 31, 48, 5, 8]);
        assert!(s.bold);
        assert_ne!(s.fg, CellColor::Default);
        assert_ne!(s.bg, CellColor::Default);
    }

    #[test]
    fn truncated_extended_sgr_is_ignored() {
        let mut s = Style::new();
        s.apply_sgr(&[38, 2, 10]);
        assert_eq!(s.fg, CellColor::Default);
    }

    #[test]
    fn sgr_sets_color_source_alongside_rgba() {
        // Basic ANSI fg.
        let mut s = Style::new();
        s.apply_sgr(&[31]);
        assert_eq!(s.fg, CellColor::Indexed(1));
        // Bright ANSI fg maps to indices 8..15.
        let mut s = Style::new();
        s.apply_sgr(&[91]);
        assert_eq!(s.fg, CellColor::Indexed(9));
        // 256-color cube preserves the raw index.
        let mut s = Style::new();
        s.apply_sgr(&[38, 5, 196]);
        assert_eq!(s.fg, CellColor::Indexed(196));
        // Truecolor keeps the raw sRGB bytes.
        let mut s = Style::new();
        s.apply_sgr(&[38, 2, 200, 100, 50]);
        assert_eq!(s.fg, CellColor::Rgb([200, 100, 50]));
        // bg parallel
        let mut s = Style::new();
        s.apply_sgr(&[41]);
        assert_eq!(s.bg, CellColor::Indexed(1));
        // Default resets back to Default.
        let mut s = Style::new();
        s.apply_sgr(&[31]);
        s.apply_sgr(&[39]);
        assert_eq!(s.fg, CellColor::Default);
    }

    #[test]
    fn indexed_color_resolves_against_live_palette() {
        // Storing the source (not a cached RGBA) means a palette swap is
        // reflected immediately by `resolve` — no per-cell rewrite needed.
        let _guard = crate::palette::TEST_LOCK.lock().expect("test lock");
        crate::palette::install(crate::palette::Palette::defaults());
        let mut s = Style::new();
        s.apply_sgr(&[31]); // red, indexed slot 1
        assert_eq!(s.fg, CellColor::Indexed(1));
        assert_eq!(s.fg.resolve([0.0; 4]), crate::palette::get().ansi(1, false));

        let mut new_palette = crate::palette::Palette::defaults();
        new_palette.ansi[1] = [0.0, 1.0, 0.0, 1.0];
        crate::palette::install(new_palette);
        // Same CellColor, new palette -> new resolved color.
        assert_eq!(s.fg.resolve([0.0; 4]), [0.0, 1.0, 0.0, 1.0]);
        crate::palette::install(crate::palette::Palette::defaults());
    }

    #[test]
    fn truecolor_resolve_is_palette_independent() {
        let _guard = crate::palette::TEST_LOCK.lock().expect("test lock");
        crate::palette::install(crate::palette::Palette::defaults());
        let mut s = Style::new();
        s.apply_sgr(&[38, 2, 200, 100, 50]);
        let before = s.fg.resolve([0.0; 4]);
        let mut new_palette = crate::palette::Palette::defaults();
        new_palette.ansi[1] = [0.0, 1.0, 0.0, 1.0];
        crate::palette::install(new_palette);
        assert_eq!(s.fg.resolve([0.0; 4]), before, "truecolor must not track the palette");
        crate::palette::install(crate::palette::Palette::defaults());
    }

    #[test]
    fn default_resolves_to_caller_default() {
        let s = Style::new();
        assert_eq!(s.fg, CellColor::Default);
        assert_eq!(s.fg.resolve([0.1, 0.2, 0.3, 1.0]), [0.1, 0.2, 0.3, 1.0]);
    }
}
