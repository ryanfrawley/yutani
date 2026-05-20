//! 16-color ANSI palette + background/foreground/cursor/selection, loaded
//! once at startup from a TOML scheme. `style.rs` and `main.rs` resolve all
//! colors through `palette::get()`; if no scheme was loaded, the defaults
//! match the previously-hardcoded values byte-for-byte.

use std::sync::RwLock;

/// Color-depth cap a scheme advertises. Cells whose resolved color exceeds
/// the cap (e.g. a truecolor SGR under `Ansi16`) are snapped onto the
/// supported set at render time by [`Palette::project`].
///
/// 88-color (rxvt's compact 4×4×4 cube) is deliberately omitted — the
/// schemes ecosystem effectively doesn't ship 88-color content, and
/// supporting it would mean carrying a second snap table for no real
/// payoff.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ColorCap {
    /// Strict two-tone: every cell collapses to either `foreground` or
    /// `background` based on its perceptual luma.
    Mono,
    /// Normal ANSI 0..=7 only. Bright variants get folded onto their
    /// matching normal entry (saturation, not brightness, wins).
    Ansi8,
    /// Normal + bright ANSI (`ansi[0..16]`).
    Ansi16,
    /// xterm-256: ANSI + 6×6×6 cube + 24-step grays.
    Xterm256,
    /// Pass-through — no projection. The default.
    Truecolor,
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Palette {
    pub background: [f32; 4],
    pub foreground: [f32; 4],
    pub cursor: [f32; 4],
    /// Stored un-premultiplied. The renderer applies a theme-dependent alpha
    /// at draw time and premultiplies there.
    pub selection: [f32; 4],
    /// Optional override applied to a cell's glyph color while that cell is
    /// part of the active selection. `None` (the default) leaves selected
    /// text in its underlying fg so the translucent selection overlay just
    /// tints it; `Some(c)` paints the glyph in `c` instead — useful when a
    /// scheme's selection bg doesn't have enough contrast against the
    /// default fg.
    pub selection_fg: Option<[f32; 4]>,
    /// Index 0..=7 normal, 8..=15 bright. Order matches ANSI:
    /// black, red, green, yellow, blue, magenta, cyan, white.
    pub ansi: [[f32; 4]; 16],
    /// Color-depth ceiling. Cell colors exceeding it are snapped via
    /// `project()` before going to the GPU.
    pub max_colors: ColorCap,
    /// Per-scheme glow + scanline overrides. Each `Some(_)` field is a
    /// candidate override for the matching `glow_*` config slot; the
    /// `theme_overrides_glow` config setting decides whether scheme values
    /// or config values win when both are present. `None` always defers
    /// to the config value.
    pub glow: GlowOverrides,
}

/// Optional per-scheme overrides for the renderer's glow + scanline knobs.
/// Mirrors the `glow_*` fields on the runtime `Config` struct in `main.rs`
/// one-for-one. A scheme can override any subset; unset fields stay `None`
/// and let the config value through unchanged.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct GlowOverrides {
    pub match_brightness: Option<bool>,
    pub match_bright_ansi: Option<bool>,
    pub match_foreground: Option<bool>,
    pub threshold: Option<f32>,
    pub intensity: Option<f32>,
    pub softness: Option<f32>,
    pub hue_tolerance_deg: Option<f32>,
    pub fg_tolerance: Option<f32>,
    pub iterations: Option<usize>,
    pub scanlines: Option<bool>,
    pub scanline_strength: Option<f32>,
    pub scanline_period: Option<f32>,
    pub scanlines_content: Option<bool>,
    pub scanlines_content_strength: Option<f32>,
    pub scanline_color_bright: Option<[f32; 4]>,
    pub scanline_color_dark: Option<[f32; 4]>,
    pub scanlines_skip_primary_bg: Option<bool>,
    pub scanlines_content_attenuation: Option<f32>,
}

impl GlowOverrides {
    /// All-`None` initializer usable from `const fn` (notably
    /// `Palette::defaults`). `Default::default()` is not const-callable.
    pub const NONE: Self = Self {
        match_brightness: None,
        match_bright_ansi: None,
        match_foreground: None,
        threshold: None,
        intensity: None,
        softness: None,
        hue_tolerance_deg: None,
        fg_tolerance: None,
        iterations: None,
        scanlines: None,
        scanline_strength: None,
        scanline_period: None,
        scanlines_content: None,
        scanlines_content_strength: None,
        scanline_color_bright: None,
        scanline_color_dark: None,
        scanlines_skip_primary_bg: None,
        scanlines_content_attenuation: None,
    };
}

impl Palette {
    pub const fn defaults() -> Self {
        // These mirror the old DIM/BRIGHT arrays from style.rs and the
        // default_fg / clear_color / cursor_color / selection_bg constants
        // from main.rs. Keep them in sync — a missing scheme should produce
        // zero rendering change.
        Self {
            background: [1.0, 1.0, 1.0, 1.0],
            foreground: [0.0, 0.0, 0.0, 1.0],
            cursor: [0.1, 0.0, 0.8, 1.0],
            selection: [0.20, 0.40, 0.85, 1.0],
            selection_fg: None,
            ansi: [
                [0.0, 0.0, 0.0, 1.0],
                [0.67, 0.0, 0.0, 1.0],
                [0.0, 0.67, 0.0, 1.0],
                [0.67, 0.67, 0.0, 1.0],
                [0.0, 0.0, 0.67, 1.0],
                [0.67, 0.0, 0.67, 1.0],
                [0.0, 0.67, 0.67, 1.0],
                [0.75, 0.75, 0.75, 1.0],
                [0.5, 0.5, 0.5, 1.0],
                [1.0, 0.33, 0.33, 1.0],
                [0.33, 1.0, 0.33, 1.0],
                [1.0, 1.0, 0.33, 1.0],
                [0.33, 0.33, 1.0, 1.0],
                [1.0, 0.33, 1.0, 1.0],
                [0.33, 1.0, 1.0, 1.0],
                [1.0, 1.0, 1.0, 1.0],
            ],
            max_colors: ColorCap::Truecolor,
            glow: GlowOverrides::NONE,
        }
    }

    /// Snap an arbitrary linear-RGB cell color onto whatever this
    /// palette's [`ColorCap`] permits. Transparent input (alpha == 0)
    /// passes through unchanged — it's a "no color" sentinel used by the
    /// renderer's masking paths, not an actual sample to be quantised.
    /// Likewise [`ColorCap::Truecolor`] is identity, so the common case
    /// (no cap) costs one branch.
    pub fn project(&self, c: [f32; 4]) -> [f32; 4] {
        if c[3] == 0.0 || self.max_colors == ColorCap::Truecolor {
            return c;
        }
        match self.max_colors {
            ColorCap::Truecolor => c,
            ColorCap::Mono => {
                // Pick whichever end the input is closer to in linear RGB.
                // "Closest" — not "above midpoint luma" — is the right
                // model for both dark-on-light and light-on-dark schemes:
                // dim grey on a white-bg scheme should snap to fg (black
                // ink), and bright grey on a black-bg scheme should snap
                // to fg (white ink). A luma-threshold model gets the
                // second case but inverts the first.
                nearest(c, &[self.foreground, self.background])
            }
            ColorCap::Ansi8 => nearest(c, &self.ansi[0..8]),
            ColorCap::Ansi16 => nearest(c, &self.ansi[0..16]),
            ColorCap::Xterm256 => {
                // Walk all 256 entries via xterm_256 so we share the cube /
                // gray-ramp definition with cell rendering — no second
                // hand-coded table to drift out of sync.
                let mut best_idx = 0u8;
                let mut best_d = f32::INFINITY;
                for n in 0..=255u8 {
                    let entry = self.xterm_256(n);
                    let d = luma_dist_sq(c, entry);
                    if d < best_d {
                        best_d = d;
                        best_idx = n;
                    }
                }
                self.xterm_256(best_idx)
            }
        }
    }

    pub fn ansi(&self, n: u8, bright: bool) -> [f32; 4] {
        let idx = (n & 7) as usize + if bright { 8 } else { 0 };
        self.ansi[idx]
    }

    pub fn xterm_256(&self, n: u8) -> [f32; 4] {
        match n {
            0..=7 => self.ansi(n, false),
            8..=15 => self.ansi(n - 8, true),
            16..=231 => {
                // xterm cube: each axis picks from {0, 95, 135, 175, 215, 255}
                // in sRGB byte space. Linearize so the GPU's sRGB encoding
                // produces those same byte values on screen.
                let n = n - 16;
                let r = n / 36;
                let g = (n % 36) / 6;
                let b = n % 6;
                let byte = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
                [
                    srgb_to_linear(byte(r)),
                    srgb_to_linear(byte(g)),
                    srgb_to_linear(byte(b)),
                    1.0,
                ]
            }
            232..=255 => {
                // 24-step grayscale ramp at sRGB bytes 8, 18, 28, ..., 238.
                let byte = (8 + (n as u16 - 232) * 10) as u8;
                let v = srgb_to_linear(byte);
                [v, v, v, 1.0]
            }
        }
    }
}

static PALETTE: RwLock<Palette> = RwLock::new(Palette::defaults());

/// Replace the live palette. Cmd-Shift-R / the Config::load path call this
/// at runtime to swap color schemes without restarting; callers are
/// responsible for re-pushing any palette-derived data baked into GPU
/// uniforms (see `State::reload_config`).
pub fn install(p: Palette) {
    *PALETTE.write().expect("palette lock poisoned") = p;
}

pub fn get() -> Palette {
    *PALETTE.read().expect("palette lock poisoned")
}

/// Test-only serialization for any test that mutates the global
/// `PALETTE`. `cargo test` runs unit tests in parallel by default;
/// two tests calling `install` concurrently would race the
/// `RwLock<Palette>` write and corrupt the state observed by their
/// peers. Tests grab this mutex before installing a custom palette
/// and hold it until they've restored defaults.
#[cfg(test)]
pub(crate) static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Parse a TOML scheme file. Missing keys keep their defaults; a key whose
/// value is the wrong type or out of range is logged via `eprintln!` and
/// skipped so one bad color doesn't blank out the rest. A document that
/// doesn't parse as TOML at all falls back wholesale to the defaults.
pub fn parse_toml(src: &str) -> Palette {
    let mut p = Palette::defaults();
    let table: toml::Table = match src.parse() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("palette: invalid TOML: {}", e);
            return p;
        }
    };
    for (key, value) in &table {
        if let Err(e) = apply(&mut p, key, value) {
            eprintln!("palette: {}: {}", key, e);
        }
    }
    p
}

/// Squared Euclidean distance weighted by Rec. 601 luma coefficients.
/// Luma-weighted because raw RGB distance treats green changes the same
/// as blue, which under-counts how visible green shifts actually are.
fn luma_dist_sq(a: [f32; 4], b: [f32; 4]) -> f32 {
    let dr = a[0] - b[0];
    let dg = a[1] - b[1];
    let db = a[2] - b[2];
    0.299 * dr * dr + 0.587 * dg * dg + 0.114 * db * db
}

fn nearest(c: [f32; 4], candidates: &[[f32; 4]]) -> [f32; 4] {
    let mut best = candidates[0];
    let mut best_d = luma_dist_sq(c, best);
    for &cand in &candidates[1..] {
        let d = luma_dist_sq(c, cand);
        if d < best_d {
            best_d = d;
            best = cand;
        }
    }
    best
}

fn apply(p: &mut Palette, key: &str, value: &toml::Value) -> Result<(), String> {
    match key {
        "background" => p.background = rgb_from_value(value)?,
        "foreground" => p.foreground = rgb_from_value(value)?,
        "cursor" => p.cursor = rgb_from_value(value)?,
        "selection" => p.selection = rgb_from_value(value)?,
        "selection_fg" | "selection_foreground" => p.selection_fg = Some(rgb_from_value(value)?),
        "black" => set_pair(&mut p.ansi, 0, value)?,
        "red" => set_pair(&mut p.ansi, 1, value)?,
        "green" => set_pair(&mut p.ansi, 2, value)?,
        "yellow" => set_pair(&mut p.ansi, 3, value)?,
        "blue" => set_pair(&mut p.ansi, 4, value)?,
        "magenta" => set_pair(&mut p.ansi, 5, value)?,
        "cyan" => set_pair(&mut p.ansi, 6, value)?,
        "white" => set_pair(&mut p.ansi, 7, value)?,
        "max_colors" => p.max_colors = parse_color_cap(value)?,
        "glow_match_brightness" => p.glow.match_brightness = Some(want_bool(value)?),
        "glow_match_bright_ansi" => p.glow.match_bright_ansi = Some(want_bool(value)?),
        "glow_match_foreground" => p.glow.match_foreground = Some(want_bool(value)?),
        "glow_threshold" => p.glow.threshold = Some(want_f32(value)?.clamp(0.0, 1.0)),
        "glow_intensity" => p.glow.intensity = Some(want_f32(value)?.max(0.0)),
        "glow_softness" => p.glow.softness = Some(want_f32(value)?.clamp(0.0, 1.0)),
        "glow_hue_tolerance_deg" => p.glow.hue_tolerance_deg = Some(want_f32(value)?.clamp(0.0, 180.0)),
        "glow_fg_tolerance" => p.glow.fg_tolerance = Some(want_f32(value)?.clamp(0.0, 3.0_f32.sqrt())),
        "glow_iterations" => p.glow.iterations = Some(want_usize(value)?),
        "glow_scanlines" => p.glow.scanlines = Some(want_bool(value)?),
        "glow_scanline_strength" => p.glow.scanline_strength = Some(want_f32(value)?.clamp(0.0, 1.0)),
        "glow_scanline_period" => p.glow.scanline_period = Some(want_f32(value)?.max(1.0)),
        "glow_scanlines_content" => p.glow.scanlines_content = Some(want_bool(value)?),
        "glow_scanlines_content_strength" => p.glow.scanlines_content_strength = Some(want_f32(value)?.clamp(0.0, 1.0)),
        "glow_scanline_color_bright" => p.glow.scanline_color_bright = Some(rgb_from_value(value)?),
        "glow_scanline_color_dark" => p.glow.scanline_color_dark = Some(rgb_from_value(value)?),
        "glow_scanlines_skip_primary_bg" => p.glow.scanlines_skip_primary_bg = Some(want_bool(value)?),
        "glow_scanlines_content_attenuation" => p.glow.scanlines_content_attenuation = Some(want_f32(value)?.clamp(0.0, 1.0)),
        _ => return Err(format!("unknown key '{}'", key)),
    }
    Ok(())
}

fn want_bool(value: &toml::Value) -> Result<bool, String> {
    value
        .as_bool()
        .ok_or_else(|| format!("expected true/false, got {}", value.type_str()))
}

/// Accept either a TOML float or integer for a numeric slot — TOML reads a
/// bare `0` as an integer and `0.6` as a float, and we don't want users to
/// have to remember which keys demand a decimal point.
fn want_f32(value: &toml::Value) -> Result<f32, String> {
    if let Some(f) = value.as_float() {
        Ok(f as f32)
    } else if let Some(i) = value.as_integer() {
        Ok(i as f32)
    } else {
        Err(format!("expected number, got {}", value.type_str()))
    }
}

fn want_usize(value: &toml::Value) -> Result<usize, String> {
    let i = value
        .as_integer()
        .ok_or_else(|| format!("expected non-negative integer, got {}", value.type_str()))?;
    usize::try_from(i).map_err(|_| format!("expected non-negative integer, got {}", i))
}

fn parse_color_cap(value: &toml::Value) -> Result<ColorCap, String> {
    // Accept the canonical string spellings, plus bare integers (`max_colors
    // = 16`) since the depth names are mostly numbers anyway.
    if let Some(n) = value.as_integer() {
        return match n {
            8 => Ok(ColorCap::Ansi8),
            16 => Ok(ColorCap::Ansi16),
            256 => Ok(ColorCap::Xterm256),
            16_777_216 => Ok(ColorCap::Truecolor),
            _ => Err(format!(
                "max_colors: expected one of mono | 8 | 16 | 256 | truecolor, got {}",
                n
            )),
        };
    }
    let s = value
        .as_str()
        .ok_or_else(|| format!("max_colors: expected a string or integer, got {}", value.type_str()))?;
    match s {
        "mono" | "monochrome" => Ok(ColorCap::Mono),
        "8" => Ok(ColorCap::Ansi8),
        "16" => Ok(ColorCap::Ansi16),
        "256" => Ok(ColorCap::Xterm256),
        "truecolor" | "16m" | "16777216" => Ok(ColorCap::Truecolor),
        _ => Err(format!(
            "max_colors: expected one of mono | 8 | 16 | 256 | truecolor, got '{}'",
            s
        )),
    }
}

fn set_pair(ansi: &mut [[f32; 4]; 16], hue: usize, value: &toml::Value) -> Result<(), String> {
    let arr = value
        .as_array()
        .ok_or_else(|| format!("expected [normal, bright] array, got {}", value.type_str()))?;
    if arr.len() != 2 {
        return Err(format!("expected exactly 2 values, got {}", arr.len()));
    }
    ansi[hue] = rgb_from_value(&arr[0])?;
    ansi[hue + 8] = rgb_from_value(&arr[1])?;
    Ok(())
}

/// Resolve a TOML value to a linear-space RGBA. Colors are written as bare
/// `0xRRGGBB` hex integers (TOML's native hex literal), so the value must be
/// an integer in `0..=0xFFFFFF`. Alpha is always 1.0; the renderer applies
/// its own alpha where it needs translucency.
pub fn rgb_from_value(value: &toml::Value) -> Result<[f32; 4], String> {
    let n = value
        .as_integer()
        .ok_or_else(|| format!("expected hex color like 0xRRGGBB, got {}", value.type_str()))?;
    if !(0..=0xFF_FFFF).contains(&n) {
        return Err(format!("color out of range 0x000000..0xFFFFFF, got {:#x}", n));
    }
    let rgb = n as u32;
    let r = srgb_to_linear(((rgb >> 16) & 0xff) as u8);
    let g = srgb_to_linear(((rgb >> 8) & 0xff) as u8);
    let b = srgb_to_linear((rgb & 0xff) as u8);
    Ok([r, g, b, 1.0])
}

/// Convert a single sRGB-encoded byte to a linear-space float in [0, 1].
/// Required because the GPU surface format is sRGB: wgpu gamma-encodes the
/// linear values we write, so feeding sRGB bytes through as if they were
/// linear would brighten everything one notch. With this conversion, the
/// pixel that lands on screen matches the user's hex literal.
pub fn srgb_to_linear(c: u8) -> f32 {
    let v = c as f32 / 255.0;
    if v <= 0.04045 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

/// Inverse of `srgb_to_linear`, snapped to a byte for OSC 10/11/12 reports
/// and other places that need to round-trip the palette to its source hex.
pub fn linear_to_srgb_u8(c: f32) -> u8 {
    let v = if c <= 0.0031308 {
        c * 12.92
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    };
    (v * 255.0).round().clamp(0.0, 255.0) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Re-encode a stored linear color back to its sRGB byte form. Tests use
    /// this to assert against the user-visible hex value rather than the
    /// internal linear representation, which would couple the assertions to
    /// the exact gamma curve.
    fn to_bytes(c: [f32; 4]) -> [u8; 3] {
        [linear_to_srgb_u8(c[0]), linear_to_srgb_u8(c[1]), linear_to_srgb_u8(c[2])]
    }

    #[test]
    fn empty_input_equals_defaults() {
        assert_eq!(parse_toml(""), Palette::defaults());
    }

    #[test]
    fn comments_and_blank_lines_skipped() {
        let p = parse_toml("# comment\n\n   \n# another\n");
        assert_eq!(p, Palette::defaults());
    }

    #[test]
    fn parses_singletons() {
        let src = "background = 0xfbfaf7\nforeground = 0x2d2519\ncursor = 0x1a00cc\nselection = 0x3366d9\n";
        let p = parse_toml(src);
        assert_eq!(to_bytes(p.background), [0xfb, 0xfa, 0xf7]);
        assert_eq!(to_bytes(p.foreground), [0x2d, 0x25, 0x19]);
        assert_eq!(to_bytes(p.cursor), [0x1a, 0x00, 0xcc]);
        assert_eq!(to_bytes(p.selection), [0x33, 0x66, 0xd9]);
    }

    #[test]
    fn parses_ansi_pair() {
        let p = parse_toml("red = [0xab0000, 0xff5555]\n");
        assert_eq!(to_bytes(p.ansi[1]), [0xab, 0x00, 0x00]);
        assert_eq!(to_bytes(p.ansi[9]), [0xff, 0x55, 0x55]);
        // Unaffected hue stays default.
        assert_eq!(p.ansi[2], Palette::defaults().ansi[2]);
    }

    #[test]
    fn short_hex_is_high_bytes_not_css_shorthand() {
        // Colors are plain TOML integers now, so `0xff` means 0x0000ff
        // (blue), NOT the CSS shorthand `0xffffff`. Leading zeroes are
        // implied by the integer's magnitude.
        let p = parse_toml("background = 0xff\nforeground = 0xf00\n");
        assert_eq!(to_bytes(p.background), [0x00, 0x00, 0xff]);
        assert_eq!(to_bytes(p.foreground), [0x00, 0x0f, 0x00]);
    }

    #[test]
    fn out_of_range_color_rejected() {
        // A value past 0xFFFFFF can't be a 24-bit color; the slot keeps
        // its default rather than wrapping or truncating silently.
        let p = parse_toml("background = 0x1000000\n");
        assert_eq!(p.background, Palette::defaults().background);
        // Negative integers are likewise refused.
        let p = parse_toml("background = -1\n");
        assert_eq!(p.background, Palette::defaults().background);
    }

    #[test]
    fn decimal_integer_color_accepted() {
        // Hex is the convention, but any integer in range is a valid color
        // — 16777215 == 0xFFFFFF == white.
        let p = parse_toml("background = 16777215\n");
        assert_eq!(to_bytes(p.background), [0xff, 0xff, 0xff]);
    }

    #[test]
    fn bad_values_dont_poison_good_ones() {
        // Wrong-typed values (a string where a color integer is expected,
        // an unknown key) are skipped per-key; sibling keys still apply.
        let src = "background = 0xfbfaf7\nbogus_key = 0x000000\nforeground = \"nothex\"\ncursor = 0x1a00cc\n";
        let p = parse_toml(src);
        assert_eq!(to_bytes(p.background), [0xfb, 0xfa, 0xf7]);
        assert_eq!(to_bytes(p.cursor), [0x1a, 0x00, 0xcc]);
        // Foreground unchanged because the value was the wrong type.
        assert_eq!(p.foreground, Palette::defaults().foreground);
    }

    #[test]
    fn invalid_toml_falls_back_to_defaults() {
        // A document that isn't valid TOML at all (here, a bareword value)
        // can't be parsed key-by-key, so the whole scheme reverts to
        // defaults rather than guessing.
        let p = parse_toml("background = 0x111111\nnonsense\n");
        assert_eq!(p, Palette::defaults());
    }

    #[test]
    fn pair_wrong_arity_rejected() {
        let p = parse_toml("red = [0xab0000]\n");
        assert_eq!(p.ansi[1], Palette::defaults().ansi[1]);
        assert_eq!(p.ansi[9], Palette::defaults().ansi[9]);
    }

    #[test]
    fn trailing_comment_after_value() {
        let p = parse_toml("background = 0x123456 # nice color\n");
        assert_eq!(to_bytes(p.background), [0x12, 0x34, 0x56]);
    }

    #[test]
    fn xterm_256_uses_palette_for_low_colors() {
        let mut p = Palette::defaults();
        p.ansi[1] = [0.1, 0.2, 0.3, 1.0];
        p.ansi[9] = [0.4, 0.5, 0.6, 1.0];
        assert_eq!(p.xterm_256(1), [0.1, 0.2, 0.3, 1.0]);
        assert_eq!(p.xterm_256(9), [0.4, 0.5, 0.6, 1.0]);
    }

    #[test]
    fn xterm_256_cube_and_gray() {
        let p = Palette::defaults();
        // Cube entry 196 is the brightest pure red in the 6×6×6 cube
        // (r=5, g=0, b=0 → bytes 255/0/0).
        assert_eq!(to_bytes(p.xterm_256(196)), [0xff, 0x00, 0x00]);
        // Gray ramp entry 244 sits at byte 8 + (244-232)*10 = 128.
        let g = p.xterm_256(244);
        assert_eq!(g[0], g[1]);
        assert_eq!(g[1], g[2]);
        assert_eq!(to_bytes(g), [0x80, 0x80, 0x80]);
    }

    #[test]
    fn ansi_indexing() {
        let p = Palette::defaults();
        assert_eq!(p.ansi(0, false), p.ansi[0]);
        assert_eq!(p.ansi(7, false), p.ansi[7]);
        assert_eq!(p.ansi(0, true), p.ansi[8]);
        assert_eq!(p.ansi(7, true), p.ansi[15]);
    }

    #[test]
    fn uppercase_hex_digits_accepted() {
        let p = parse_toml("background = 0xABCDEF\n");
        assert_eq!(to_bytes(p.background), [0xab, 0xcd, 0xef]);
    }

    #[test]
    fn underscore_separated_hex_accepted() {
        // TOML permits digit-group underscores in integer literals; they
        // shouldn't change the parsed color.
        let p = parse_toml("background = 0xab_cd_ef\n");
        assert_eq!(to_bytes(p.background), [0xab, 0xcd, 0xef]);
    }

    #[test]
    fn multiple_ansi_hues_in_one_file() {
        // All hues in a single document should be applied independently and
        // simultaneously — none of them should clobber the others.
        let src = "\
black   = [0x000000, 0x808080]
red     = [0xaa0000, 0xff5555]
green   = [0x00aa00, 0x55ff55]
yellow  = [0xaa5500, 0xffff55]
blue    = [0x0000aa, 0x5555ff]
magenta = [0xaa00aa, 0xff55ff]
cyan    = [0x00aaaa, 0x55ffff]
white   = [0xaaaaaa, 0xffffff]
";
        let p = parse_toml(src);
        assert_eq!(to_bytes(p.ansi[0]), [0x00, 0x00, 0x00]);
        assert_eq!(to_bytes(p.ansi[7]), [0xaa, 0xaa, 0xaa]);
        assert_eq!(to_bytes(p.ansi[8]), [0x80, 0x80, 0x80]);
        assert_eq!(to_bytes(p.ansi[15]), [0xff, 0xff, 0xff]);
        // Spot-check a middle hue to confirm pairs aren't crossed.
        assert_eq!(to_bytes(p.ansi[4]), [0x00, 0x00, 0xaa]);
        assert_eq!(to_bytes(p.ansi[12]), [0x55, 0x55, 0xff]);
    }

    #[test]
    fn whitespace_tolerance() {
        // Extra spaces around the separator and inside the array brackets
        // are all tolerated by the TOML grammar.
        let src = "background   =  0x111111\nred =  [ 0xaa0000 , 0xff5555 ]\n";
        let p = parse_toml(src);
        assert_eq!(to_bytes(p.background), [0x11, 0x11, 0x11]);
        assert_eq!(to_bytes(p.ansi[1]), [0xaa, 0x00, 0x00]);
        assert_eq!(to_bytes(p.ansi[9]), [0xff, 0x55, 0x55]);
    }

    #[test]
    fn parse_is_idempotent() {
        // Parsing the same source twice must yield equal Palettes — the parser
        // has no hidden state and starts from defaults each call.
        let src = "\
background = 0xfbfaf7
foreground = 0x2d2519
red = [0xab0000, 0xff5555]
blue = [0x0000ab, 0x5555ff]
";
        assert_eq!(parse_toml(src), parse_toml(src));
    }

    #[test]
    fn selection_fg_defaults_to_none() {
        // Omitting the key keeps the legacy behavior — selected text retains
        // its underlying fg and only the translucent overlay tints it.
        assert_eq!(Palette::defaults().selection_fg, None);
        assert_eq!(parse_toml("background = 0x111111\n").selection_fg, None);
    }

    #[test]
    fn parses_selection_fg() {
        let p = parse_toml("selection_fg = 0xffeeaa\n");
        let c = p.selection_fg.expect("selection_fg should be set");
        assert_eq!(to_bytes(c), [0xff, 0xee, 0xaa]);
        // Alias spelling — kitty-style — must reach the same field.
        let p = parse_toml("selection_foreground = 0x010203\n");
        let c = p.selection_fg.expect("alias should populate selection_fg");
        assert_eq!(to_bytes(c), [0x01, 0x02, 0x03]);
    }

    #[test]
    fn selection_alpha_is_one() {
        // The renderer applies its own theme-dependent alpha at draw time,
        // so the parsed selection color must be stored un-premultiplied with
        // alpha == 1.0 regardless of the RGB value supplied.
        let p = parse_toml("selection = 0x000000\n");
        assert_eq!(p.selection[3], 1.0);
        assert_eq!(p.selection, [0.0, 0.0, 0.0, 1.0]);
        let p = parse_toml("selection = 0x3366d9\n");
        assert_eq!(p.selection[3], 1.0);
    }

    #[test]
    fn srgb_round_trip_byte_exact() {
        // Every byte in [0, 255] should survive sRGB → linear → sRGB without
        // drifting. If this ever loses fidelity we'd see OSC 11 reports that
        // don't match the user's scheme hex.
        for b in 0..=255u8 {
            assert_eq!(linear_to_srgb_u8(srgb_to_linear(b)), b);
        }
    }

    #[test]
    fn default_cap_is_truecolor() {
        // Existing schemes don't carry a cap → must behave as truecolor so
        // upgrading the binary doesn't suddenly quantise everyone's terminal.
        assert_eq!(Palette::defaults().max_colors, ColorCap::Truecolor);
    }

    #[test]
    fn parse_max_colors_string_values() {
        let cases = [
            ("mono", ColorCap::Mono),
            ("monochrome", ColorCap::Mono),
            ("8", ColorCap::Ansi8),
            ("16", ColorCap::Ansi16),
            ("256", ColorCap::Xterm256),
            ("truecolor", ColorCap::Truecolor),
            ("16m", ColorCap::Truecolor),
        ];
        for (literal, want) in cases {
            let p = parse_toml(&format!("max_colors = \"{}\"\n", literal));
            assert_eq!(p.max_colors, want, "literal '{}'", literal);
        }
    }

    #[test]
    fn parse_max_colors_integer_values() {
        // Bare integers are accepted too, since the depth names are mostly
        // numbers: `max_colors = 16`.
        let cases = [
            (8, ColorCap::Ansi8),
            (16, ColorCap::Ansi16),
            (256, ColorCap::Xterm256),
            (16_777_216, ColorCap::Truecolor),
        ];
        for (literal, want) in cases {
            let p = parse_toml(&format!("max_colors = {}\n", literal));
            assert_eq!(p.max_colors, want, "literal {}", literal);
        }
    }

    #[test]
    fn parse_max_colors_unknown_keeps_default() {
        // Typos shouldn't escalate into a full reset — same contract as
        // other invalid palette values.
        let p = parse_toml("max_colors = 42\n");
        assert_eq!(p.max_colors, ColorCap::Truecolor);
        let p = parse_toml("max_colors = \"lots\"\n");
        assert_eq!(p.max_colors, ColorCap::Truecolor);
    }

    #[test]
    fn project_truecolor_is_identity() {
        // Truecolor cap = pass-through, including alpha. A non-identity
        // result here would change rendered output for every existing user.
        let p = Palette::defaults();
        let c = [0.123, 0.456, 0.789, 1.0];
        assert_eq!(p.project(c), c);
    }

    #[test]
    fn project_preserves_transparent() {
        // Alpha == 0 is the "no SGR bg set, let the window show through"
        // sentinel; projecting it would paint the palette bg over every
        // unstyled cell and break the layered glow path.
        let mut p = Palette::defaults();
        p.max_colors = ColorCap::Mono;
        assert_eq!(p.project([0.0, 0.0, 0.0, 0.0]), [0.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn project_mono_picks_fg_for_light_input_on_dark_scheme() {
        let mut p = Palette::defaults();
        p.background = [0.0, 0.0, 0.0, 1.0];
        p.foreground = [1.0, 1.0, 1.0, 1.0];
        p.max_colors = ColorCap::Mono;
        // White input → fg; black → bg.
        assert_eq!(p.project([1.0, 1.0, 1.0, 1.0]), p.foreground);
        assert_eq!(p.project([0.0, 0.0, 0.0, 1.0]), p.background);
    }

    #[test]
    fn project_mono_midpoint_split_works_for_light_scheme() {
        // Dark-on-light scheme: the threshold should bend with the scheme,
        // not stay pinned at 0.5. A dim grey on a white-bg scheme should
        // still land on fg (the only "ink") rather than vanishing into bg.
        let mut p = Palette::defaults();
        p.background = [1.0, 1.0, 1.0, 1.0];
        p.foreground = [0.0, 0.0, 0.0, 1.0];
        p.max_colors = ColorCap::Mono;
        let dim_grey = [0.3, 0.3, 0.3, 1.0];
        assert_eq!(p.project(dim_grey), p.foreground);
    }

    #[test]
    fn project_ansi16_snaps_pure_red_to_red_entry() {
        // The defaults' red entries are (0.67, 0, 0) and (1.0, 0.33, 0.33).
        // A solid red input should choose one of those, not a stray hue.
        let mut p = Palette::defaults();
        p.max_colors = ColorCap::Ansi16;
        let out = p.project([1.0, 0.0, 0.0, 1.0]);
        // Either normal-red or bright-red is acceptable — both are red and
        // either is correct under the Ansi16 cap; pinning to one over the
        // other would couple the test to the chosen distance metric.
        let candidates = [p.ansi[1], p.ansi[9]];
        assert!(
            candidates.contains(&out),
            "expected red snap, got {:?}",
            out,
        );
    }

    #[test]
    fn project_ansi8_excludes_bright_entries() {
        // Ansi8 cap must NOT return any bright slot, even if the input
        // would be closer to one — that's the whole point of the cap.
        let mut p = Palette::defaults();
        p.max_colors = ColorCap::Ansi8;
        // Pick a bright-leaning input.
        let out = p.project([1.0, 0.33, 0.33, 1.0]);
        for bright in &p.ansi[8..16] {
            assert_ne!(&out, bright, "bright entry leaked under Ansi8 cap");
        }
    }

    #[test]
    fn project_xterm256_in_palette_input_is_stable() {
        // Re-projecting an already-in-cube color must round-trip to the
        // same entry — drift here would mean palette-indexed cells changed
        // colour just by passing through projection.
        let mut p = Palette::defaults();
        p.max_colors = ColorCap::Xterm256;
        let c196 = p.xterm_256(196);
        assert_eq!(p.project(c196), c196);
    }

    #[test]
    fn install_overwrites_live_palette() {
        // The Cmd-Shift-R reload path depends on `install` actually replacing
        // the live palette — the prior OnceLock storage would silently keep
        // the first installed value. This pins overwrite semantics so a
        // regression would surface in tests instead of in the user's running
        // terminal. Serialised against itself by being the only test that
        // mutates PALETTE.
        let baseline = get();
        let mut p = Palette::defaults();
        p.background = [0.12, 0.34, 0.56, 1.0];
        install(p);
        assert_eq!(get().background, [0.12, 0.34, 0.56, 1.0]);
        // Restore so any test added later doesn't observe poisoned state.
        install(baseline);
    }

    #[test]
    fn srgb_anchor_values() {
        // Pin the linearization at known anchor points so a refactor that
        // accidentally drops the gamma curve fails loudly here rather than
        // silently brightening everything.
        assert_eq!(srgb_to_linear(0), 0.0);
        assert_eq!(srgb_to_linear(255), 1.0);
        // Mid-gray (128/255 ≈ 0.502 sRGB) lands near 0.216 in linear space.
        let mid = srgb_to_linear(128);
        assert!((mid - 0.2159).abs() < 0.001, "mid-gray linearized to {}", mid);
    }

    //
    // GlowOverrides: per-scheme overrides for the renderer's glow + scanline
    // knobs. The fields below pin the parser-side contract — clamping,
    // forgiving error handling, and the const initializer — so the
    // tiebreaker logic in `main.rs` can rely on well-formed `Option<T>`
    // values regardless of what the scheme file said.
    //

    #[test]
    fn defaults_glow_overrides_are_all_none() {
        // No scheme key → no override candidate. The tiebreaker in
        // `apply_glow_config` requires every slot to start `None` so
        // config values pass through unmodified for schemes that don't
        // opt in.
        assert_eq!(Palette::defaults().glow, GlowOverrides::NONE);
    }

    #[test]
    fn glow_overrides_none_const_matches_default() {
        // `GlowOverrides::NONE` is a hand-written const for use in
        // `Palette::defaults()` (which is `const fn` and so can't call
        // `Default::default()`). If anyone adds a field to the struct
        // without extending NONE, this catches the drift.
        assert_eq!(GlowOverrides::NONE, GlowOverrides::default());
    }

    #[test]
    fn empty_toml_leaves_glow_overrides_none() {
        // Sanity: the parser doesn't synthesise glow overrides out of
        // thin air. Schemes without any glow_ keys must round-trip to
        // `NONE`.
        let p = parse_toml("# only comments\nbackground = 0x000000\n");
        assert_eq!(p.glow, GlowOverrides::NONE);
    }

    #[test]
    fn parses_glow_bool_overrides() {
        let src = "\
glow_match_brightness = true
glow_match_bright_ansi = false
glow_match_foreground = true
glow_scanlines = true
glow_scanlines_content = false
glow_scanlines_skip_primary_bg = true
";
        let p = parse_toml(src);
        assert_eq!(p.glow.match_brightness, Some(true));
        assert_eq!(p.glow.match_bright_ansi, Some(false));
        assert_eq!(p.glow.match_foreground, Some(true));
        assert_eq!(p.glow.scanlines, Some(true));
        assert_eq!(p.glow.scanlines_content, Some(false));
        assert_eq!(p.glow.scanlines_skip_primary_bg, Some(true));
    }

    #[test]
    fn parses_glow_numeric_overrides_in_range() {
        // Each numeric override key writes its value verbatim when it
        // sits inside the slot's permitted range — no clamping should
        // alter the result here. Note `glow_hue_tolerance_deg` and
        // `glow_scanline_period` are written as bare integers to confirm
        // the parser accepts integers where a float is expected.
        let src = "\
glow_threshold = 0.7
glow_intensity = 1.5
glow_softness = 0.4
glow_hue_tolerance_deg = 30
glow_fg_tolerance = 0.5
glow_iterations = 4
glow_scanline_strength = 0.6
glow_scanline_period = 5
glow_scanlines_content_strength = 0.3
glow_scanlines_content_attenuation = 0.8
";
        let p = parse_toml(src);
        assert_eq!(p.glow.threshold, Some(0.7));
        assert_eq!(p.glow.intensity, Some(1.5));
        assert_eq!(p.glow.softness, Some(0.4));
        assert_eq!(p.glow.hue_tolerance_deg, Some(30.0));
        assert_eq!(p.glow.fg_tolerance, Some(0.5));
        assert_eq!(p.glow.iterations, Some(4));
        assert_eq!(p.glow.scanline_strength, Some(0.6));
        assert_eq!(p.glow.scanline_period, Some(5.0));
        assert_eq!(p.glow.scanlines_content_strength, Some(0.3));
        assert_eq!(p.glow.scanlines_content_attenuation, Some(0.8));
    }

    #[test]
    fn parses_glow_scanline_color_overrides() {
        // Color overrides go through the same `rgb_from_value` path as
        // foreground/background, so the linear-space round-trip and
        // alpha == 1.0 contract apply.
        let p = parse_toml("glow_scanline_color_bright = 0xff00ff\nglow_scanline_color_dark = 0x112233\n");
        let bright = p.glow.scanline_color_bright.expect("bright override");
        let dark = p.glow.scanline_color_dark.expect("dark override");
        assert_eq!(to_bytes(bright), [0xff, 0x00, 0xff]);
        assert_eq!(to_bytes(dark), [0x11, 0x22, 0x33]);
        assert_eq!(bright[3], 1.0);
        assert_eq!(dark[3], 1.0);
    }

    #[test]
    fn glow_threshold_clamps_high() {
        // Threshold lives on [0, 1]; an out-of-range scheme value must
        // be clamped on parse so downstream consumers don't need to
        // sanitise the override themselves.
        let p = parse_toml("glow_threshold = 2.0\n");
        assert_eq!(p.glow.threshold, Some(1.0));
    }

    #[test]
    fn glow_softness_clamps_low() {
        // Softness lives on [0, 1]; negative values clamp up to 0.
        let p = parse_toml("glow_softness = -0.2\n");
        assert_eq!(p.glow.softness, Some(0.0));
    }

    #[test]
    fn glow_scanline_period_clamps_to_one() {
        // Period < 1 would produce a sub-pixel scanline ridge that
        // aliases badly; the parser floors it at 1.0.
        let p = parse_toml("glow_scanline_period = 0.5\n");
        assert_eq!(p.glow.scanline_period, Some(1.0));
    }

    #[test]
    fn glow_intensity_clamps_low() {
        // Intensity is unbounded above but pinned at 0 below — a
        // negative scheme value would subtract light, which the
        // pipeline can't represent.
        let p = parse_toml("glow_intensity = -1.0\n");
        assert_eq!(p.glow.intensity, Some(0.0));
    }

    #[test]
    fn glow_hue_tolerance_clamps_high() {
        // Hue tolerance lives on [0, 180] degrees; values beyond 180
        // would match the entire colour wheel twice.
        let p = parse_toml("glow_hue_tolerance_deg = 500\n");
        assert_eq!(p.glow.hue_tolerance_deg, Some(180.0));
    }

    #[test]
    fn glow_fg_tolerance_clamps_to_sqrt3() {
        // The fg-tolerance ceiling is sqrt(3), the max linear-RGB
        // Euclidean distance between black and white.
        let p = parse_toml("glow_fg_tolerance = 5.0\n");
        let got = p.glow.fg_tolerance.expect("fg_tolerance override");
        assert!((got - 3.0_f32.sqrt()).abs() < 1e-6, "got {}", got);
    }

    #[test]
    fn rgb_from_value_accepts_black_and_white_boundaries() {
        // The two ends of the legal 24-bit range must both decode: 0x000000
        // is pure black and 0xFFFFFF is pure white. These are the boundary
        // values the range check `0..=0xFFFFFF` admits, so a fencepost slip
        // in that comparison would surface here.
        let black = rgb_from_value(&toml::Value::Integer(0x000000)).expect("0x000000 in range");
        assert_eq!(to_bytes(black), [0x00, 0x00, 0x00]);
        assert_eq!(black[3], 1.0);
        let white = rgb_from_value(&toml::Value::Integer(0xFF_FFFF)).expect("0xFFFFFF in range");
        assert_eq!(to_bytes(white), [0xff, 0xff, 0xff]);
        assert_eq!(white[3], 1.0);
    }

    #[test]
    fn rgb_from_value_rejects_just_past_white() {
        // 0x1000000 is one past the 24-bit ceiling — accepting it would
        // wrap into the green/blue bytes and silently mis-colour the slot,
        // so it must be an Err rather than a truncated success.
        let r = rgb_from_value(&toml::Value::Integer(0x1_000000));
        assert!(r.is_err(), "0x1000000 should be rejected, got {:?}", r);
    }

    #[test]
    fn rgb_from_value_rejects_negative() {
        // TOML integers are signed; a negative literal can't be a colour and
        // must not be reinterpreted as a large unsigned value.
        let r = rgb_from_value(&toml::Value::Integer(-1));
        assert!(r.is_err(), "-1 should be rejected, got {:?}", r);
    }

    #[test]
    fn rgb_from_value_rejects_non_integer_types() {
        // Anything that isn't a bare integer (string, float, bool) can't be a
        // 0xRRGGBB literal and must be refused so the per-key skip kicks in.
        assert!(rgb_from_value(&toml::Value::String("0xffffff".into())).is_err());
        assert!(rgb_from_value(&toml::Value::Float(1.0)).is_err());
        assert!(rgb_from_value(&toml::Value::Boolean(true)).is_err());
    }

    #[test]
    fn glow_numeric_overrides_accept_float_and_integer_alike() {
        // `want_f32` must take both a TOML float and a bare integer for the
        // same slot, so users needn't remember which keys demand a decimal
        // point. Here `glow_threshold` is given as a float and again as an
        // integer (0 and 1, the range endpoints) and both must land.
        let pf = parse_toml("glow_threshold = 0.0\n");
        assert_eq!(pf.glow.threshold, Some(0.0));
        let pi = parse_toml("glow_threshold = 1\n");
        assert_eq!(pi.glow.threshold, Some(1.0));
    }

    #[test]
    fn invalid_glow_values_dont_poison_good_ones() {
        // Same forgiving contract as `bad_values_dont_poison_good_ones`:
        // a wrong-typed glow value leaves its slot at `None` but sibling
        // glow keys still parse. Values must stay valid TOML (strings here)
        // so the document parses at all.
        let src = "\
glow_threshold = \"notanumber\"
glow_intensity = 1.25
glow_match_brightness = \"maybe\"
glow_match_foreground = true
glow_scanline_color_bright = \"nothex\"
glow_scanline_period = 4
";
        let p = parse_toml(src);
        assert_eq!(p.glow.threshold, None);
        assert_eq!(p.glow.intensity, Some(1.25));
        assert_eq!(p.glow.match_brightness, None);
        assert_eq!(p.glow.match_foreground, Some(true));
        assert_eq!(p.glow.scanline_color_bright, None);
        assert_eq!(p.glow.scanline_period, Some(4.0));
    }
}
