//! Character cell width (a compact `wcwidth`).
//!
//! The terminal grid is a fixed monospace matrix: every character occupies a
//! whole number of cells. Most do one, but East-Asian ideographs, Hangul,
//! fullwidth forms, and emoji are *wide* — they're drawn two cells across, and
//! the shell's own line editor (zsh/ZLE, readline) lays them out that way too.
//! When the terminal disagrees with the application about a character's width,
//! the two cursor models drift apart and the screen smears — a stray cursor
//! block next to a pasted emoji, mis-placed prompts after CJK input, and so on.
//!
//! Rather than vendor a full Unicode-width table (and a dependency), we mirror
//! what modern terminals (kitty, alacritty, wezterm, iTerm2) settled on: the
//! Unicode *Wide* + *Fullwidth* East-Asian classes plus the default
//! emoji-presentation ranges all count as two cells. The codepoint that
//! prompted this (✅ U+2705) lands in the dingbats emoji range below.
//!
//! Combining marks and other zero-width characters are intentionally treated
//! as width 1 here: the grid has no combining-mark model yet, so collapsing
//! them to zero would orphan them. Narrowing that is a separate change.

/// Number of grid cells `ch` occupies: `2` for wide (CJK / fullwidth / emoji),
/// `1` for everything else. Never returns `0` — see the module note on
/// combining marks.
pub fn char_cells(ch: char) -> usize {
    if is_wide(ch as u32) {
        2
    } else {
        1
    }
}

/// Whether `ch` is an emoji codepoint we'd want to render in color — the astral
/// emoji planes plus the BMP emoji-symbol ranges. Deliberately excludes CJK /
/// fullwidth (which are wide but not emoji), so the color rasterizer is only
/// consulted for genuine emoji that the monochrome font chain misses.
pub fn is_emoji(ch: char) -> bool {
    let cp = ch as u32;
    if (0x2300..0x2C00).contains(&cp) {
        return is_wide_bmp_symbol(cp);
    }
    const EMOJI: &[(u32, u32)] = &[
        (0x1F000, 0x1FAFF), // mahjong/dominoes/cards … symbols & pictographs ext-A
        (0x1F100, 0x1F1FF), // enclosed alphanumeric supplement (regional indicators)
    ];
    EMOJI.iter().any(|&(lo, hi)| cp >= lo && cp <= hi)
}

/// East-Asian Wide / Fullwidth plus the default-emoji-presentation ranges.
/// Ranges are sorted so the lookup can early-out; kept inclusive and explicit
/// to stay readable against the Unicode charts they mirror.
fn is_wide(cp: u32) -> bool {
    // Fast path: all of Latin/ASCII and the bulk of common text is narrow and
    // sits below the first wide block (Hangul Jamo at U+1100).
    if cp < 0x1100 {
        return false;
    }
    // The scattered BMP emoji-symbol ranges (U+231A..U+2B55) interleave with
    // narrow symbols, so they live in their own table; the contiguous CJK /
    // fullwidth / astral-emoji blocks are below. (U+2329/U+232A angle brackets
    // fall in this window too and are handled there.)
    if (0x2300..0x2C00).contains(&cp) {
        return is_wide_bmp_symbol(cp);
    }
    const WIDE: &[(u32, u32)] = &[
        (0x1100, 0x115F),   // Hangul Jamo (initial consonants)
        (0x2E80, 0x303E),   // CJK radicals, Kangxi, CJK symbols & punctuation
        (0x3041, 0x33FF),   // Hiragana, Katakana, Bopomofo … CJK compat
        (0x3400, 0x4DBF),   // CJK Unified Ideographs Extension A
        (0x4E00, 0x9FFF),   // CJK Unified Ideographs
        (0xA000, 0xA4CF),   // Yi syllables / radicals
        (0xA960, 0xA97F),   // Hangul Jamo Extended-A
        (0xAC00, 0xD7A3),   // Hangul Syllables
        (0xF900, 0xFAFF),   // CJK Compatibility Ideographs
        (0xFE10, 0xFE19),   // vertical forms
        (0xFE30, 0xFE6F),   // CJK compatibility / small form variants
        (0xFF00, 0xFF60),   // fullwidth ASCII variants
        (0xFFE0, 0xFFE6),   // fullwidth signs
        (0x1F004, 0x1F004), // 🀄 mahjong red dragon
        (0x1F0CF, 0x1F0CF), // 🃏 playing card black joker
        (0x1F18E, 0x1F18E), // 🆎 AB button
        (0x1F191, 0x1F19A), // 🆑–🆚 squared latin
        (0x1F200, 0x1F2FF), // enclosed ideographic supplement
        (0x1F300, 0x1F64F), // misc symbols & pictographs + emoticons
        (0x1F680, 0x1F6FF), // transport & map symbols
        (0x1F900, 0x1F9FF), // supplemental symbols & pictographs
        (0x1FA00, 0x1FAFF), // chess/symbols & extended-A pictographs
        (0x20000, 0x3FFFD), // CJK Unified Ideographs Extensions B–G (plane 2–3)
    ];
    WIDE.iter().any(|&(lo, hi)| cp >= lo && cp <= hi)
}

/// Wide codepoints in the U+2300..U+2BFF window: the default-emoji-presentation
/// symbols scattered through the BMP symbol blocks, interleaved with many narrow
/// symbols. Split out so the contiguous high blocks stay a tidy range table.
fn is_wide_bmp_symbol(cp: u32) -> bool {
    const WIDE: &[(u32, u32)] = &[
        (0x2329, 0x232A), // 〈〉 angle brackets
        (0x231A, 0x231B), // ⌚⌛ watch / hourglass
        (0x23E9, 0x23EC), // ⏩–⏬ fast-forward arrows
        (0x23F0, 0x23F0), // ⏰ alarm clock
        (0x23F3, 0x23F3), // ⏳ hourglass flowing
        (0x25FD, 0x25FE), // ◽◾ medium small squares
        (0x2614, 0x2615), // ☔☕ umbrella / hot beverage
        (0x2648, 0x2653), // ♈–♓ zodiac
        (0x267F, 0x267F), // ♿ wheelchair
        (0x2693, 0x2693), // ⚓ anchor
        (0x26A1, 0x26A1), // ⚡ high voltage
        (0x26AA, 0x26AB), // ⚪⚫ circles
        (0x26BD, 0x26BE), // ⚽⚾ soccer / baseball
        (0x26C4, 0x26C5), // ⛄⛅ snowman / sun behind cloud
        (0x26CE, 0x26CE), // ⛎ ophiuchus
        (0x26D4, 0x26D4), // ⛔ no entry
        (0x26EA, 0x26EA), // ⛪ church
        (0x26F2, 0x26F3), // ⛲⛳ fountain / flag in hole
        (0x26F5, 0x26F5), // ⛵ sailboat
        (0x26FA, 0x26FA), // ⛺ tent
        (0x26FD, 0x26FD), // ⛽ fuel pump
        (0x2705, 0x2705), // ✅ white heavy check mark
        (0x270A, 0x270B), // ✊✋ raised fist / hand
        (0x2728, 0x2728), // ✨ sparkles
        (0x274C, 0x274C), // ❌ cross mark
        (0x274E, 0x274E), // ❎ negative squared cross
        (0x2753, 0x2755), // ❓❔❕ question / exclamation
        (0x2757, 0x2757), // ❗ exclamation
        (0x2795, 0x2797), // ➕➖➗ plus / minus / divide
        (0x27B0, 0x27B0), // ➰ curly loop
        (0x27BF, 0x27BF), // ➿ double curly loop
        (0x2B1B, 0x2B1C), // ⬛⬜ large squares
        (0x2B50, 0x2B50), // ⭐ star
        (0x2B55, 0x2B55), // ⭕ heavy large circle
    ];
    WIDE.iter().any(|&(lo, hi)| cp >= lo && cp <= hi)
}

#[cfg(test)]
mod tests {
    use super::{char_cells, is_emoji};

    #[test]
    fn ascii_is_narrow() {
        for c in ' '..='~' {
            assert_eq!(char_cells(c), 1, "{c:?} should be 1 cell");
        }
    }

    #[test]
    fn reported_checkmark_is_wide() {
        // The bug that started this: ✅ U+2705 must reserve two cells so the
        // shell's cursor model stays in lockstep with the grid.
        assert_eq!(char_cells('\u{2705}'), 2);
    }

    #[test]
    fn cjk_and_emoji_are_wide() {
        assert_eq!(char_cells('中'), 2);
        assert_eq!(char_cells('한'), 2);
        assert_eq!(char_cells('あ'), 2);
        assert_eq!(char_cells('Ａ'), 2); // fullwidth A
        assert_eq!(char_cells('🚀'), 2);
        assert_eq!(char_cells('😀'), 2);
        assert_eq!(char_cells('🧑'), 2);
    }

    #[test]
    fn latin_accents_stay_narrow() {
        assert_eq!(char_cells('é'), 1);
        assert_eq!(char_cells('ñ'), 1);
        assert_eq!(char_cells('Ω'), 1);
    }

    #[test]
    fn arrows_and_box_drawing_stay_narrow() {
        // These render in a single cell in the grid; widening them would
        // desync the many TUIs that draw borders with them.
        assert_eq!(char_cells('→'), 1);
        assert_eq!(char_cells('│'), 1);
        assert_eq!(char_cells('█'), 1);
    }

    #[test]
    fn hangul_jamo_lower_boundary_is_exact() {
        // The first wide block starts at U+1100 (Hangul Jamo); the codepoint
        // immediately below it must stay narrow.
        assert_eq!(char_cells('\u{10FF}'), 1);
        assert_eq!(char_cells('\u{1100}'), 2);
    }

    #[test]
    fn angle_brackets_are_wide_amid_narrow_neighbours() {
        // The lone wide pair U+2329/U+232A sits between narrow symbols; only the
        // brackets themselves widen.
        assert_eq!(char_cells('\u{2328}'), 1);
        assert_eq!(char_cells('\u{2329}'), 2);
        assert_eq!(char_cells('\u{232B}'), 1);
    }

    #[test]
    fn box_drawing_block_stays_narrow() {
        // U+2500 box drawing must stay one cell so TUI borders line up; the
        // codepoint just below the symbol window is narrow too.
        assert_eq!(char_cells('\u{24FF}'), 1);
        assert_eq!(char_cells('\u{2500}'), 1);
    }

    #[test]
    fn regional_indicators_are_emoji_but_single_cell() {
        // U+1F1E6 (regional indicator A) is treated as an emoji codepoint for
        // color rasterization, but `char_cells` leaves it at one cell — the
        // wide-cell table deliberately omits the regional-indicator range.
        assert!(is_emoji('\u{1F1E6}'));
        assert_eq!(char_cells('\u{1F1E6}'), 1);
    }

    #[test]
    fn astral_emoji_and_cjk_extensions_are_wide() {
        assert_eq!(char_cells('\u{1FAFF}'), 2); // top of pictographs ext-A
        assert_eq!(char_cells('\u{20000}'), 2); // CJK Unified Ideographs Ext-B
    }

    #[test]
    fn tag_characters_are_narrow() {
        // U+E0000 (tags) is a control range, not a wide block.
        assert_eq!(char_cells('\u{E0000}'), 1);
    }

    #[test]
    fn is_emoji_true_for_genuine_emoji() {
        assert!(is_emoji('\u{1F600}')); // 😀 astral emoji
        assert!(is_emoji('\u{2705}')); // ✅ BMP dingbat emoji
        assert!(is_emoji('\u{1F1E6}')); // regional indicator A
    }

    #[test]
    fn is_emoji_false_for_wide_non_emoji_and_plain_text() {
        assert!(!is_emoji('\u{4E2D}')); // 中 CJK — wide but not emoji
        assert!(!is_emoji('a')); // ASCII
        assert!(!is_emoji('\u{2500}')); // box drawing — narrow symbol
        assert!(!is_emoji('\u{AC00}')); // 가 Hangul — wide but not emoji
    }
}
