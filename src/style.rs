#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Style {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub reverse: bool,
    pub color_bg: Option<[f32; 4]>,
    pub color_fg: Option<[f32; 4]>,
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
                30..=37 => self.color_fg = Some(ansi_color((params[i] - 30) as u8, false)),
                39 => self.color_fg = None,
                40..=47 => self.color_bg = Some(ansi_color((params[i] - 40) as u8, false)),
                49 => self.color_bg = None,
                90..=97 => self.color_fg = Some(ansi_color((params[i] - 90) as u8, true)),
                100..=107 => self.color_bg = Some(ansi_color((params[i] - 100) as u8, true)),
                38 | 48 => {
                    let target_fg = params[i] == 38;
                    if i + 1 < params.len() {
                        match params[i + 1] {
                            5 if i + 2 < params.len() => {
                                let c = xterm_256(params[i + 2] as u8);
                                if target_fg {
                                    self.color_fg = Some(c);
                                } else {
                                    self.color_bg = Some(c);
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
                                } else {
                                    self.color_bg = Some(c);
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
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Cell {
    pub ch: char,
    pub style: Style,
}

impl Cell {
    pub fn new(ch: char, style: Style) -> Self {
        Self { ch, style }
    }
}

fn ansi_color(n: u8, bright: bool) -> [f32; 4] {
    const DIM: [[f32; 3]; 8] = [
        [0.0, 0.0, 0.0],
        [0.67, 0.0, 0.0],
        [0.0, 0.67, 0.0],
        [0.67, 0.67, 0.0],
        [0.0, 0.0, 0.67],
        [0.67, 0.0, 0.67],
        [0.0, 0.67, 0.67],
        [0.75, 0.75, 0.75],
    ];
    const BRIGHT: [[f32; 3]; 8] = [
        [0.5, 0.5, 0.5],
        [1.0, 0.33, 0.33],
        [0.33, 1.0, 0.33],
        [1.0, 1.0, 0.33],
        [0.33, 0.33, 1.0],
        [1.0, 0.33, 1.0],
        [0.33, 1.0, 1.0],
        [1.0, 1.0, 1.0],
    ];
    let p = if bright { BRIGHT[n as usize] } else { DIM[n as usize] };
    [p[0], p[1], p[2], 1.0]
}

fn rgb(r: u8, g: u8, b: u8) -> [f32; 4] {
    [r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, 1.0]
}

fn xterm_256(n: u8) -> [f32; 4] {
    match n {
        0..=7 => ansi_color(n, false),
        8..=15 => ansi_color(n - 8, true),
        16..=231 => {
            let n = n - 16;
            let r = n / 36;
            let g = (n % 36) / 6;
            let b = n % 6;
            let conv = |v: u8| {
                if v == 0 {
                    0.0
                } else {
                    (55.0 + v as f32 * 40.0) / 255.0
                }
            };
            [conv(r), conv(g), conv(b), 1.0]
        }
        232..=255 => {
            let v = (8 + (n as u16 - 232) * 10) as f32 / 255.0;
            [v, v, v, 1.0]
        }
    }
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
        assert_eq!(s.color_fg, Some([1.0, 0.33, 0.33, 1.0]));
    }

    #[test]
    fn truecolor_fg() {
        let mut s = Style::new();
        s.apply_sgr(&[38, 2, 255, 128, 0]);
        assert_eq!(s.color_fg, Some([1.0, 128.0 / 255.0, 0.0, 1.0]));
    }

    #[test]
    fn truecolor_bg() {
        let mut s = Style::new();
        s.apply_sgr(&[48, 2, 10, 20, 30]);
        assert_eq!(
            s.color_bg,
            Some([10.0 / 255.0, 20.0 / 255.0, 30.0 / 255.0, 1.0])
        );
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
}
