//! 16-color ANSI palette + background/foreground/cursor/selection, loaded
//! once at startup from a YAML scheme. `style.rs` and `main.rs` resolve all
//! colors through `palette::get()`; if no scheme was loaded, the defaults
//! match the previously-hardcoded values byte-for-byte.

use std::sync::RwLock;

#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Palette {
    pub background: [f32; 4],
    pub foreground: [f32; 4],
    pub cursor: [f32; 4],
    /// Stored un-premultiplied. The renderer applies a theme-dependent alpha
    /// at draw time and premultiplies there.
    pub selection: [f32; 4],
    /// Index 0..=7 normal, 8..=15 bright. Order matches ANSI:
    /// black, red, green, yellow, blue, magenta, cyan, white.
    pub ansi: [[f32; 4]; 16],
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

/// Parse a YAML-subset scheme file. Missing fields keep their defaults;
/// malformed lines are logged via `eprintln!` and skipped so a typo in one
/// color doesn't blank out the rest.
pub fn parse_yaml(src: &str) -> Palette {
    let mut p = Palette::defaults();
    for (lineno, raw) in src.lines().enumerate() {
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            eprintln!("palette: line {}: missing ':'", lineno + 1);
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        if let Err(e) = apply(&mut p, key, value) {
            eprintln!("palette: line {}: {}", lineno + 1, e);
        }
    }
    p
}

fn strip_comment(s: &str) -> &str {
    match s.find('#') {
        Some(i) => &s[..i],
        None => s,
    }
}

fn apply(p: &mut Palette, key: &str, value: &str) -> Result<(), String> {
    match key {
        "background" => p.background = rgba_from(value)?,
        "foreground" => p.foreground = rgba_from(value)?,
        "cursor" => p.cursor = rgba_from(value)?,
        "selection" => p.selection = rgba_from(value)?,
        "black" => set_pair(&mut p.ansi, 0, value)?,
        "red" => set_pair(&mut p.ansi, 1, value)?,
        "green" => set_pair(&mut p.ansi, 2, value)?,
        "yellow" => set_pair(&mut p.ansi, 3, value)?,
        "blue" => set_pair(&mut p.ansi, 4, value)?,
        "magenta" => set_pair(&mut p.ansi, 5, value)?,
        "cyan" => set_pair(&mut p.ansi, 6, value)?,
        "white" => set_pair(&mut p.ansi, 7, value)?,
        _ => return Err(format!("unknown key '{}'", key)),
    }
    Ok(())
}

fn set_pair(ansi: &mut [[f32; 4]; 16], hue: usize, value: &str) -> Result<(), String> {
    let v = value
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .ok_or_else(|| format!("expected [normal, bright] array, got '{}'", value))?;
    let parts: Vec<&str> = v.split(',').map(str::trim).collect();
    if parts.len() != 2 {
        return Err(format!("expected exactly 2 values, got {}", parts.len()));
    }
    ansi[hue] = rgba_from(parts[0])?;
    ansi[hue + 8] = rgba_from(parts[1])?;
    Ok(())
}

pub fn rgba_from(s: &str) -> Result<[f32; 4], String> {
    let rgb = parse_hex(s)?;
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

/// Accepts `0xRRGGBB` (6 hex digits) or `0xRGB` (3 hex digits expanded
/// CSS-style: each nibble duplicated, so `0xfff` → `0xffffff` not `0x000fff`).
/// Other lengths are ambiguous (is `0xff` `0x0000ff` blue or `0xffffff` white?)
/// so they're rejected outright.
fn parse_hex(s: &str) -> Result<u32, String> {
    let body = s
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .ok_or_else(|| format!("expected 0x-prefixed hex literal, got '{}'", s))?;
    match body.len() {
        6 => u32::from_str_radix(body, 16).map_err(|e| format!("invalid hex '{}': {}", s, e)),
        3 => {
            let v = u32::from_str_radix(body, 16)
                .map_err(|e| format!("invalid hex '{}': {}", s, e))?;
            let r = (v >> 8) & 0xf;
            let g = (v >> 4) & 0xf;
            let b = v & 0xf;
            Ok((r * 0x11) << 16 | (g * 0x11) << 8 | (b * 0x11))
        }
        _ => Err(format!("hex literal must be 3 or 6 digits, got '{}'", s)),
    }
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
        assert_eq!(parse_yaml(""), Palette::defaults());
    }

    #[test]
    fn comments_and_blank_lines_skipped() {
        let p = parse_yaml("# comment\n\n   \n# another\n");
        assert_eq!(p, Palette::defaults());
    }

    #[test]
    fn parses_singletons() {
        let src = "background: 0xfbfaf7\nforeground: 0x2d2519\ncursor: 0x1a00cc\nselection: 0x3366d9\n";
        let p = parse_yaml(src);
        assert_eq!(to_bytes(p.background), [0xfb, 0xfa, 0xf7]);
        assert_eq!(to_bytes(p.foreground), [0x2d, 0x25, 0x19]);
        assert_eq!(to_bytes(p.cursor), [0x1a, 0x00, 0xcc]);
        assert_eq!(to_bytes(p.selection), [0x33, 0x66, 0xd9]);
    }

    #[test]
    fn parses_ansi_pair() {
        let p = parse_yaml("red: [0xab0000, 0xff5555]\n");
        assert_eq!(to_bytes(p.ansi[1]), [0xab, 0x00, 0x00]);
        assert_eq!(to_bytes(p.ansi[9]), [0xff, 0x55, 0x55]);
        // Unaffected hue stays default.
        assert_eq!(p.ansi[2], Palette::defaults().ansi[2]);
    }

    #[test]
    fn three_digit_hex_is_css_shorthand() {
        let p = parse_yaml("black: [0x000, 0xfff]\nred: [0xf00, 0x0f0]\n");
        assert_eq!(to_bytes(p.ansi[0]), [0x00, 0x00, 0x00]);
        assert_eq!(to_bytes(p.ansi[8]), [0xff, 0xff, 0xff]);
        // 0xf00 expands to 0xff0000, not 0x000f00.
        assert_eq!(to_bytes(p.ansi[1]), [0xff, 0x00, 0x00]);
        assert_eq!(to_bytes(p.ansi[9]), [0x00, 0xff, 0x00]);
    }

    #[test]
    fn ambiguous_hex_lengths_rejected() {
        // 4-digit and 5-digit forms have no obvious meaning.
        let p = parse_yaml("background: 0xabcd\n");
        assert_eq!(p.background, Palette::defaults().background);
        let p = parse_yaml("background: 0xabcde\n");
        assert_eq!(p.background, Palette::defaults().background);
    }

    #[test]
    fn bad_lines_dont_poison_good_ones() {
        let src = "background: 0xfbfaf7\nbogus_key: 0x000000\nforeground: nothex\ncursor: 0x1a00cc\n";
        let p = parse_yaml(src);
        assert_eq!(to_bytes(p.background), [0xfb, 0xfa, 0xf7]);
        assert_eq!(to_bytes(p.cursor), [0x1a, 0x00, 0xcc]);
        // Foreground unchanged because the value was invalid.
        assert_eq!(p.foreground, Palette::defaults().foreground);
    }

    #[test]
    fn pair_wrong_arity_rejected() {
        let p = parse_yaml("red: [0xab0000]\n");
        assert_eq!(p.ansi[1], Palette::defaults().ansi[1]);
        assert_eq!(p.ansi[9], Palette::defaults().ansi[9]);
    }

    #[test]
    fn trailing_comment_after_value() {
        let p = parse_yaml("background: 0x123456 # nice color\n");
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
        let p = parse_yaml("background: 0xABCDEF\n");
        assert_eq!(to_bytes(p.background), [0xab, 0xcd, 0xef]);
    }

    #[test]
    fn capital_x_prefix_accepted() {
        let p = parse_yaml("background: 0X123456\n");
        assert_eq!(to_bytes(p.background), [0x12, 0x34, 0x56]);
    }

    #[test]
    fn missing_prefix_rejected() {
        // Bare hex (no 0x) is not a valid literal — the line must be skipped
        // and the default preserved.
        let p = parse_yaml("background: abcdef\n");
        assert_eq!(p.background, Palette::defaults().background);
    }

    #[test]
    fn line_without_colon_skipped() {
        // A line missing the ':' separator should be reported and skipped
        // without affecting sibling lines.
        let src = "background: 0x111111\nnocolonhere\nforeground: 0x222222\n";
        let p = parse_yaml(src);
        assert_eq!(to_bytes(p.background), [0x11, 0x11, 0x11]);
        assert_eq!(to_bytes(p.foreground), [0x22, 0x22, 0x22]);
    }

    #[test]
    fn multiple_ansi_hues_in_one_file() {
        // All hues in a single document should be applied independently and
        // simultaneously — none of them should clobber the others.
        let src = "\
black:   [0x000000, 0x808080]
red:     [0xaa0000, 0xff5555]
green:   [0x00aa00, 0x55ff55]
yellow:  [0xaa5500, 0xffff55]
blue:    [0x0000aa, 0x5555ff]
magenta: [0xaa00aa, 0xff55ff]
cyan:    [0x00aaaa, 0x55ffff]
white:   [0xaaaaaa, 0xffffff]
";
        let p = parse_yaml(src);
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
        // Tabs and extra spaces around the separator, inside the array
        // brackets, and around the comma should all be tolerated.
        let src = "background:\t  0x111111\nred:  [  0xaa0000 ,\t0xff5555  ]\n";
        let p = parse_yaml(src);
        assert_eq!(to_bytes(p.background), [0x11, 0x11, 0x11]);
        assert_eq!(to_bytes(p.ansi[1]), [0xaa, 0x00, 0x00]);
        assert_eq!(to_bytes(p.ansi[9]), [0xff, 0x55, 0x55]);
    }

    #[test]
    fn parse_is_idempotent() {
        // Parsing the same source twice must yield equal Palettes — the parser
        // has no hidden state and starts from defaults each call.
        let src = "\
background: 0xfbfaf7
foreground: 0x2d2519
red: [0xab0000, 0xff5555]
blue: [0x0000ab, 0x5555ff]
";
        assert_eq!(parse_yaml(src), parse_yaml(src));
    }

    #[test]
    fn selection_alpha_is_one() {
        // The renderer applies its own theme-dependent alpha at draw time,
        // so the parsed selection color must be stored un-premultiplied with
        // alpha == 1.0 regardless of the RGB value supplied.
        let p = parse_yaml("selection: 0x000000\n");
        assert_eq!(p.selection[3], 1.0);
        assert_eq!(p.selection, [0.0, 0.0, 0.0, 1.0]);
        let p = parse_yaml("selection: 0x3366d9\n");
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
}
