//! Translate winit key events into the byte sequences a unix terminal expects
//! on the PTY. There is no local edit buffer here — every keystroke goes
//! straight to the slave, which is what full-screen apps (vim, less, htop)
//! require and what shell line editors (zle, readline) handle on their side.

use winit::keyboard::{Key, ModifiersState, NamedKey};

/// Mouse button identifier (in xterm code-space, before modifier/motion flags).
/// 0 = left, 1 = middle, 2 = right, 64 = wheel-up, 65 = wheel-down.
pub type MouseButton = u8;

pub const MOUSE_LEFT: MouseButton = 0;
pub const MOUSE_MIDDLE: MouseButton = 1;
pub const MOUSE_RIGHT: MouseButton = 2;
pub const MOUSE_WHEEL_UP: MouseButton = 64;
pub const MOUSE_WHEEL_DOWN: MouseButton = 65;

/// How many wheel-up and wheel-down notches a pixel delta should produce
/// when forwarding scroll to an app that has asked for mouse tracking
/// (tmux, vim, less, htop).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct WheelNotches {
    pub up: u32,
    pub down: u32,
}

/// Drain a pixel accumulator into discrete wheel notches.
///
/// Trackpads stream many sub-line `PixelDelta` events; truncating each one
/// independently rounds every event to 0 and produces no wheel reports
/// until a single large event finally crosses the line-height threshold
/// (and then fires a burst). This accumulates pixels across events and
/// emits one notch per `line_height` drained.
///
/// A direction change zeroes residue first so leftover pixels from a
/// prior up-scroll can't fire as a stale down event after the user
/// reverses (or vice versa).
///
/// Returns the number of up and down notches to emit; the caller is
/// responsible for sending them to the PTY.
pub fn drain_wheel_accum(
    accum: &mut f64,
    delta_pixels: f64,
    line_height: f64,
) -> WheelNotches {
    if line_height <= 0.0 {
        return WheelNotches::default();
    }
    if delta_pixels.signum() != 0.0
        && accum.signum() != 0.0
        && delta_pixels.signum() != accum.signum()
    {
        *accum = 0.0;
    }
    *accum += delta_pixels;
    let mut notches = WheelNotches::default();
    while *accum >= line_height {
        notches.up += 1;
        *accum -= line_height;
    }
    while *accum <= -line_height {
        notches.down += 1;
        *accum += line_height;
    }
    notches
}

/// Encode a mouse event for the PTY in either SGR (1006) or legacy X10 form.
/// `col`/`row` are 1-based cell positions. `motion` is set for events emitted
/// by drag/move tracking. `press` is true on button-down (and for wheel
/// notches), false on button-up; legacy X10 ignores it (release uses button 3).
pub fn encode_mouse(
    button: MouseButton,
    col: u16,
    row: u16,
    press: bool,
    motion: bool,
    sgr: bool,
    mods: ModifiersState,
) -> Vec<u8> {
    let mut b = button;
    if motion {
        b += 32;
    }
    if mods.shift_key() {
        b += 4;
    }
    if mods.alt_key() {
        b += 8;
    }
    if mods.control_key() {
        b += 16;
    }
    if sgr {
        let final_byte = if press { 'M' } else { 'm' };
        return format!("\x1b[<{};{};{}{}", b, col, row, final_byte).into_bytes();
    }
    // Legacy X10: button + col + row each offset by 32, capped at 255.
    let cb = if press { b.saturating_add(32) } else { 35 }; // 3 + 32 = release
    let cx = (col as u32).saturating_add(32).min(255) as u8;
    let cy = (row as u32).saturating_add(32).min(255) as u8;
    vec![0x1b, b'[', b'M', cb, cx, cy]
}

/// Bytes to write to the PTY for a key press. `text` is the layout-translated
/// text from `KeyEvent::text` (may be empty for Ctrl-modified events on some
/// platforms; we then fall back to the character in `logical`). `app_cursor`
/// is the terminal's DECCKM bit — when set, unmodified arrow / Home / End
/// emit SS3 forms (`\eOA`) instead of CSI (`\e[A`). Returns `None` for keys
/// we don't translate (Cmd-anything, dead keys, unbound named keys).
pub fn encode_key(
    logical: &Key,
    text: Option<&str>,
    mods: ModifiersState,
    app_cursor: bool,
) -> Option<Vec<u8>> {
    // Cmd is reserved for system shortcuts on macOS (copy/paste/quit).
    if mods.super_key() {
        return None;
    }

    let ctrl = mods.control_key();
    let alt = mods.alt_key();
    let shift = mods.shift_key();

    // xterm modifier-encoding parameter: 1 + (shift|alt<<1|ctrl<<2). `None`
    // when no modifiers are held — caller emits the un-parameterized form.
    let mod_bits = (shift as u8) | ((alt as u8) << 1) | ((ctrl as u8) << 2);
    let mod_param = (mod_bits != 0).then(|| mod_bits + 1);

    if let Key::Named(named) = logical {
        return named_key(*named, alt, mod_param, app_cursor);
    }

    // Fall back to logical_key's character if `text` is missing.
    let s = text.or_else(|| match logical {
        Key::Character(s) => Some(s.as_str()),
        _ => None,
    })?;
    let first = s.chars().next()?;
    let single_char = s.chars().nth(1).is_none();

    let mut out = Vec::with_capacity(s.len() + 1);
    if alt {
        out.push(0x1b);
    }
    if ctrl && single_char {
        if let Some(code) = ctrl_byte(first) {
            out.push(code);
            return Some(out);
        }
    }
    out.extend_from_slice(s.as_bytes());
    Some(out)
}

/// Map a printable char to its Ctrl-modified byte. Letters fold case; the
/// classic punctuation forms map to 0x00, 0x1b…0x1f, 0x7f.
fn ctrl_byte(ch: char) -> Option<u8> {
    Some(match ch {
        'a'..='z' => (ch as u8) - b'a' + 1,
        'A'..='Z' => (ch as u8) - b'A' + 1,
        '@' | ' ' => 0x00,
        '[' => 0x1b,
        '\\' => 0x1c,
        ']' => 0x1d,
        '^' => 0x1e,
        '_' => 0x1f,
        '?' => 0x7f,
        _ => return None,
    })
}

fn named_key(
    named: NamedKey,
    alt: bool,
    mod_param: Option<u8>,
    app_cursor: bool,
) -> Option<Vec<u8>> {
    // Cursor / Home / End. `\e[<fb>` normally; `\eO<fb>` in app-cursor mode
    // when no modifiers are held; `\e[1;<mod><fb>` whenever any modifier is
    // held (xterm escapes modifiers through the CSI form regardless of mode).
    let csi_letter = |fb: u8| -> Vec<u8> {
        match mod_param {
            None if app_cursor => vec![0x1b, b'O', fb],
            None => vec![0x1b, b'[', fb],
            Some(p) => format!("\x1b[1;{}{}", p, fb as char).into_bytes(),
        }
    };
    // `\e[<n>~` with optional `;<mod>` before the tilde.
    let tilde = |n: u8| -> Vec<u8> {
        match mod_param {
            None => format!("\x1b[{}~", n).into_bytes(),
            Some(p) => format!("\x1b[{};{}~", n, p).into_bytes(),
        }
    };
    // Simple C0 controls — Alt prefixes with ESC, other modifiers ignored
    // (terminals disagree on how to encode e.g. Ctrl+Enter; pass it through).
    let control = |b: u8| -> Vec<u8> {
        if alt {
            vec![0x1b, b]
        } else {
            vec![b]
        }
    };

    Some(match named {
        NamedKey::Enter => control(b'\r'),
        NamedKey::Tab if mod_param == Some(2) => vec![0x1b, b'[', b'Z'], // Shift+Tab
        NamedKey::Tab => control(b'\t'),
        NamedKey::Backspace => control(0x7f),
        NamedKey::Escape => control(0x1b),
        NamedKey::Space => control(b' '),
        NamedKey::ArrowUp => csi_letter(b'A'),
        NamedKey::ArrowDown => csi_letter(b'B'),
        NamedKey::ArrowRight => csi_letter(b'C'),
        NamedKey::ArrowLeft => csi_letter(b'D'),
        NamedKey::Home => csi_letter(b'H'),
        NamedKey::End => csi_letter(b'F'),
        NamedKey::PageUp => tilde(5),
        NamedKey::PageDown => tilde(6),
        NamedKey::Insert => tilde(2),
        NamedKey::Delete => tilde(3),
        NamedKey::F1 => vec![0x1b, b'O', b'P'],
        NamedKey::F2 => vec![0x1b, b'O', b'Q'],
        NamedKey::F3 => vec![0x1b, b'O', b'R'],
        NamedKey::F4 => vec![0x1b, b'O', b'S'],
        NamedKey::F5 => tilde(15),
        NamedKey::F6 => tilde(17),
        NamedKey::F7 => tilde(18),
        NamedKey::F8 => tilde(19),
        NamedKey::F9 => tilde(20),
        NamedKey::F10 => tilde(21),
        NamedKey::F11 => tilde(23),
        NamedKey::F12 => tilde(24),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use winit::keyboard::SmolStr;

    fn ch(c: &str) -> Key {
        Key::Character(SmolStr::new(c))
    }

    #[test]
    fn plain_letter_passes_through() {
        assert_eq!(
            encode_key(&ch("a"), Some("a"), ModifiersState::empty(), false),
            Some(b"a".to_vec()),
        );
    }

    #[test]
    fn ctrl_letter_becomes_control_code() {
        assert_eq!(
            encode_key(&ch("c"), None, ModifiersState::CONTROL, false),
            Some(vec![0x03]),
        );
    }

    #[test]
    fn ctrl_shift_letter_still_folds() {
        // Ctrl+Shift+A — text is "A" but the control code is the same as ^A.
        assert_eq!(
            encode_key(
                &ch("A"),
                Some("A"),
                ModifiersState::CONTROL | ModifiersState::SHIFT,
                false,
            ),
            Some(vec![0x01]),
        );
    }

    #[test]
    fn ctrl_left_bracket_is_escape() {
        assert_eq!(
            encode_key(&ch("["), None, ModifiersState::CONTROL, false),
            Some(vec![0x1b]),
        );
    }

    #[test]
    fn alt_letter_prefixes_esc() {
        assert_eq!(
            encode_key(&ch("a"), Some("a"), ModifiersState::ALT, false),
            Some(vec![0x1b, b'a']),
        );
    }

    #[test]
    fn cmd_is_swallowed() {
        assert_eq!(
            encode_key(&ch("c"), Some("c"), ModifiersState::SUPER, false),
            None,
        );
    }

    #[test]
    fn enter_is_carriage_return() {
        assert_eq!(
            encode_key(&Key::Named(NamedKey::Enter), None, ModifiersState::empty(), false),
            Some(vec![b'\r']),
        );
    }

    #[test]
    fn backspace_is_del() {
        assert_eq!(
            encode_key(
                &Key::Named(NamedKey::Backspace),
                None,
                ModifiersState::empty(),
                false,
            ),
            Some(vec![0x7f]),
        );
    }

    #[test]
    fn escape_key() {
        assert_eq!(
            encode_key(&Key::Named(NamedKey::Escape), None, ModifiersState::empty(), false),
            Some(vec![0x1b]),
        );
    }

    #[test]
    fn arrow_up_unmodified() {
        assert_eq!(
            encode_key(&Key::Named(NamedKey::ArrowUp), None, ModifiersState::empty(), false),
            Some(b"\x1b[A".to_vec()),
        );
    }

    #[test]
    fn arrow_up_with_ctrl_uses_modifier_param() {
        // ctrl bit = 4, +1 = 5 → "\e[1;5A"
        assert_eq!(
            encode_key(&Key::Named(NamedKey::ArrowUp), None, ModifiersState::CONTROL, false),
            Some(b"\x1b[1;5A".to_vec()),
        );
    }

    #[test]
    fn shift_tab_is_cbt() {
        assert_eq!(
            encode_key(&Key::Named(NamedKey::Tab), None, ModifiersState::SHIFT, false),
            Some(b"\x1b[Z".to_vec()),
        );
    }

    #[test]
    fn function_keys() {
        assert_eq!(
            encode_key(&Key::Named(NamedKey::F1), None, ModifiersState::empty(), false),
            Some(b"\x1bOP".to_vec()),
        );
        assert_eq!(
            encode_key(&Key::Named(NamedKey::F5), None, ModifiersState::empty(), false),
            Some(b"\x1b[15~".to_vec()),
        );
    }

    #[test]
    fn page_keys_with_modifiers() {
        // shift bit 1 + 1 = 2 → "\e[5;2~"
        assert_eq!(
            encode_key(&Key::Named(NamedKey::PageUp), None, ModifiersState::SHIFT, false),
            Some(b"\x1b[5;2~".to_vec()),
        );
    }

    #[test]
    fn arrow_in_app_cursor_mode_uses_ss3() {
        assert_eq!(
            encode_key(
                &Key::Named(NamedKey::ArrowUp),
                None,
                ModifiersState::empty(),
                true,
            ),
            Some(b"\x1bOA".to_vec()),
        );
        assert_eq!(
            encode_key(
                &Key::Named(NamedKey::Home),
                None,
                ModifiersState::empty(),
                true,
            ),
            Some(b"\x1bOH".to_vec()),
        );
    }

    #[test]
    fn arrow_in_app_cursor_mode_falls_back_to_csi_with_modifiers() {
        // Even in app-cursor mode, holding a modifier forces the CSI form so
        // the parameter encoding still works.
        assert_eq!(
            encode_key(
                &Key::Named(NamedKey::ArrowUp),
                None,
                ModifiersState::CONTROL,
                true,
            ),
            Some(b"\x1b[1;5A".to_vec()),
        );
    }

    #[test]
    fn mouse_sgr_press_and_release() {
        assert_eq!(
            encode_mouse(MOUSE_LEFT, 12, 5, true, false, true, ModifiersState::empty()),
            b"\x1b[<0;12;5M".to_vec(),
        );
        assert_eq!(
            encode_mouse(MOUSE_LEFT, 12, 5, false, false, true, ModifiersState::empty()),
            b"\x1b[<0;12;5m".to_vec(),
        );
    }

    #[test]
    fn mouse_sgr_motion_and_modifiers() {
        // motion (+32) + shift (+4) on right button.
        assert_eq!(
            encode_mouse(MOUSE_RIGHT, 1, 1, true, true, true, ModifiersState::SHIFT),
            b"\x1b[<38;1;1M".to_vec(),
        );
    }

    #[test]
    fn mouse_legacy_x10_form() {
        // 1-based col 1 → 33, row 1 → 33; left button + 32 = 32 (' ').
        assert_eq!(
            encode_mouse(MOUSE_LEFT, 1, 1, true, false, false, ModifiersState::empty()),
            vec![0x1b, b'[', b'M', 32, 33, 33],
        );
    }

    #[test]
    fn mouse_wheel_up() {
        assert_eq!(
            encode_mouse(MOUSE_WHEEL_UP, 5, 7, true, false, true, ModifiersState::empty()),
            b"\x1b[<64;5;7M".to_vec(),
        );
    }

    #[test]
    fn alt_enter_prefixes_esc() {
        assert_eq!(
            encode_key(&Key::Named(NamedKey::Enter), None, ModifiersState::ALT, false),
            Some(vec![0x1b, b'\r']),
        );
    }

    // --- drain_wheel_accum ---------------------------------------------
    //
    // The conventions tested below mirror the PTY mouse-tracking path in
    // `WindowEvent::MouseWheel`: positive pixels mean "scroll up" (content
    // moves down, wheel-up notches), negative means "scroll down".

    const LH: f64 = 20.0;

    #[test]
    fn slow_subline_events_accumulate_into_one_up_notch() {
        // Ten 2-pixel ticks (total 20px = one line) should fire exactly one
        // wheel-up notch — not zero, which is what the old truncating
        // `(p.y / line_height) as i32` produced.
        let mut accum = 0.0;
        let mut total = WheelNotches::default();
        for _ in 0..10 {
            let n = drain_wheel_accum(&mut accum, 2.0, LH);
            total.up += n.up;
            total.down += n.down;
        }
        assert_eq!(total, WheelNotches { up: 1, down: 0 });
        // Residue should be ~0 after consuming exactly one line worth.
        assert!(accum.abs() < 1e-9, "accum = {}", accum);
    }

    #[test]
    fn slow_subline_events_eventually_emit_after_residue() {
        // Nine 2-pixel ticks (18px) is below the threshold — no notch yet.
        let mut accum = 0.0;
        for _ in 0..9 {
            assert_eq!(
                drain_wheel_accum(&mut accum, 2.0, LH),
                WheelNotches::default(),
            );
        }
        assert!((accum - 18.0).abs() < 1e-9);
        // One more 2px tick crosses 20px and fires.
        assert_eq!(
            drain_wheel_accum(&mut accum, 2.0, LH),
            WheelNotches { up: 1, down: 0 },
        );
    }

    #[test]
    fn fast_single_event_fires_multiple_notches() {
        // A single 65-pixel kick at line-height 20 should produce 3 notches
        // and leave 5px of residue.
        let mut accum = 0.0;
        let n = drain_wheel_accum(&mut accum, 65.0, LH);
        assert_eq!(n, WheelNotches { up: 3, down: 0 });
        assert!((accum - 5.0).abs() < 1e-9, "accum = {}", accum);
    }

    #[test]
    fn fast_single_event_fires_multiple_down_notches() {
        let mut accum = 0.0;
        let n = drain_wheel_accum(&mut accum, -45.0, LH);
        assert_eq!(n, WheelNotches { up: 0, down: 2 });
        assert!((accum - -5.0).abs() < 1e-9, "accum = {}", accum);
    }

    #[test]
    fn direction_change_drops_prior_residue() {
        // Scroll up enough to leave +18px residue (no notch yet).
        let mut accum = 0.0;
        let n = drain_wheel_accum(&mut accum, 18.0, LH);
        assert_eq!(n, WheelNotches::default());
        assert!((accum - 18.0).abs() < 1e-9);
        // User reverses with a small downward tick. The 18px of prior
        // up-residue must be discarded — otherwise the new -2px would only
        // bring the accumulator down to +16, and a *third* downward tick
        // would still owe an up notch from the original gesture.
        let n = drain_wheel_accum(&mut accum, -2.0, LH);
        assert_eq!(n, WheelNotches::default());
        assert!((accum - -2.0).abs() < 1e-9, "accum = {}", accum);
    }

    #[test]
    fn same_direction_preserves_residue() {
        // Two same-direction events should NOT trigger the reset.
        let mut accum = 0.0;
        drain_wheel_accum(&mut accum, 12.0, LH);
        drain_wheel_accum(&mut accum, 12.0, LH);
        // 24px total → one notch fires, 4px residue retained.
        assert!((accum - 4.0).abs() < 1e-9, "accum = {}", accum);
    }

    #[test]
    fn line_delta_converted_to_pixels_uses_same_drain() {
        // Caller multiplies the LineDelta by line_height before calling, so
        // one full line of LineDelta input becomes exactly one notch with
        // zero residue, matching a single 20px PixelDelta event.
        let mut accum = 0.0;
        let line_delta = 1.0_f32;
        let pixels = line_delta as f64 * LH;
        let n = drain_wheel_accum(&mut accum, pixels, LH);
        assert_eq!(n, WheelNotches { up: 1, down: 0 });
        assert!(accum.abs() < 1e-9);
    }

    #[test]
    fn zero_delta_is_noop() {
        let mut accum = 7.0;
        let n = drain_wheel_accum(&mut accum, 0.0, LH);
        assert_eq!(n, WheelNotches::default());
        // A zero delta has no sign, so it must not trigger the
        // direction-change reset — residue is preserved.
        assert!((accum - 7.0).abs() < 1e-9);
    }

    #[test]
    fn nonpositive_line_height_is_safe() {
        // Defensive: a degenerate font metric must not loop forever or
        // panic. No notches, no accumulator mutation.
        let mut accum = 5.0;
        let n = drain_wheel_accum(&mut accum, 100.0, 0.0);
        assert_eq!(n, WheelNotches::default());
        assert!((accum - 5.0).abs() < 1e-9);
    }
}
