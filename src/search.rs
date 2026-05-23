//! Find-in-scrollback: a small overlay (summoned with Cmd-F) that incrementally
//! searches the whole terminal buffer — live grid plus scrollback — for a typed
//! string, highlights every match, and lets you step through them with
//! Enter / Shift-Enter.
//!
//! Like [`crate::command_palette`], this module is deliberately pure: no GPU, no
//! winit, no `State`. It owns the find overlay's *logic* (the text field, the
//! smart-case substring matcher, and the open/edit/step state machine) so it can
//! be unit-tested in isolation. `main.rs` drives it from real key events, scans
//! the terminal for matches via [`match_line`], scrolls the viewport to the
//! current hit, and renders the box with the same quad/glyph batch the command
//! palette uses.

use crate::command_palette::TextField;

/// A single match: an inclusive `[start_col, end_col]` run of cells on the row
/// at absolute line `line` (the same stable line index `Terminal::line_at`
/// takes — scrollback first, then live-grid rows).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Match {
    pub line: isize,
    pub start_col: usize,
    pub end_col: usize,
}

/// The full find-overlay state: open flag, the query input, the match list, and
/// a cursor into it.
#[derive(Default)]
pub struct Search {
    pub open: bool,
    pub input: TextField,
    /// All matches across the buffer, in reading order (oldest line first).
    pub matches: Vec<Match>,
    /// Index of the "current" (emphasised) match within `matches`. Always a
    /// valid index when `matches` is non-empty; `0` when it's empty.
    pub current: usize,
}

impl Search {
    /// Open fresh: empty query, no matches.
    pub fn open(&mut self) {
        self.open = true;
        self.input.clear();
        self.matches.clear();
        self.current = 0;
    }

    pub fn close(&mut self) {
        self.open = false;
        self.input.clear();
        self.matches.clear();
        self.current = 0;
    }

    pub fn toggle(&mut self) {
        if self.open {
            self.close();
        } else {
            self.open();
        }
    }

    /// Insert a typed character into the query.
    pub fn type_char(&mut self, c: char) {
        self.input.insert(c);
    }

    /// Backspace in the query.
    pub fn backspace(&mut self) {
        self.input.backspace();
    }

    /// Replace the match list, clamping `current` so it stays a valid index
    /// (or `0` when the list becomes empty).
    pub fn set_matches(&mut self, matches: Vec<Match>) {
        self.matches = matches;
        if self.current >= self.matches.len() {
            self.current = 0;
        }
    }

    /// Step to the next match, wrapping around. No-op when there are none.
    pub fn next(&mut self) {
        if self.matches.is_empty() {
            return;
        }
        self.current = (self.current + 1) % self.matches.len();
    }

    /// Step to the previous match, wrapping around. No-op when there are none.
    pub fn prev(&mut self) {
        if self.matches.is_empty() {
            return;
        }
        self.current = (self.current + self.matches.len() - 1) % self.matches.len();
    }

    /// The current match, if any.
    pub fn current_match(&self) -> Option<Match> {
        self.matches.get(self.current).copied()
    }
}

/// Smart-case rule: a query is matched case-*sensitively* iff it contains an
/// uppercase character. An all-lowercase query matches case-insensitively. This
/// mirrors the familiar behaviour from vim/ripgrep/iTerm.
pub fn smart_case(query: &str) -> bool {
    query.chars().any(|c| c.is_uppercase())
}

/// All non-overlapping occurrences of `query` in `line`, as inclusive
/// `(start_col, end_col)` **character** index pairs. Character indices (not
/// bytes) so they line up with terminal columns, where the caller builds `line`
/// as one `char` per cell.
///
/// An empty query matches nothing. When `case_sensitive` is false, both sides
/// are folded to lowercase (one lowercase char per source char, so the column
/// mapping stays 1:1) before comparison; the returned indices still refer to
/// the original `line`'s character positions. Matches don't overlap: after a
/// hit, scanning resumes just past it.
pub fn match_line(line: &str, query: &str, case_sensitive: bool) -> Vec<(usize, usize)> {
    if query.is_empty() {
        return Vec::new();
    }

    // Fold each char to a single comparison char. For the case-insensitive
    // path we take the first char of `to_lowercase()` so a char still maps to
    // exactly one column (full Unicode case folding can change length, which
    // would desync column indices — not worth it for terminal search).
    let fold = |s: &str| -> Vec<char> {
        if case_sensitive {
            s.chars().collect()
        } else {
            s.chars()
                .map(|c| c.to_lowercase().next().unwrap_or(c))
                .collect()
        }
    };
    let hay = fold(line);
    let need = fold(query);
    let need_len = need.len();
    if need_len == 0 || hay.len() < need_len {
        return Vec::new();
    }

    let mut hits = Vec::new();
    let mut i = 0usize;
    while i + need_len <= hay.len() {
        if hay[i..i + need_len] == need[..] {
            hits.push((i, i + need_len - 1));
            i += need_len; // non-overlapping
        } else {
            i += 1;
        }
    }
    hits
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smart_case_lowercase_is_insensitive() {
        assert!(!smart_case("foo"));
        assert!(!smart_case("foo-bar_baz 123"));
    }

    #[test]
    fn smart_case_uppercase_is_sensitive() {
        assert!(smart_case("Foo"));
        assert!(smart_case("fooBar"));
        assert!(smart_case("ERROR"));
    }

    #[test]
    fn match_line_empty_query() {
        assert!(match_line("anything", "", false).is_empty());
    }

    #[test]
    fn match_line_no_match() {
        assert!(match_line("hello world", "xyz", false).is_empty());
    }

    #[test]
    fn match_line_single_hit_indices() {
        // "hello world": "world" starts at char 6, ends at char 10.
        assert_eq!(match_line("hello world", "world", false), vec![(6, 10)]);
    }

    #[test]
    fn match_line_hit_at_start_and_end() {
        assert_eq!(match_line("abcabc", "abc", false), vec![(0, 2), (3, 5)]);
    }

    #[test]
    fn match_line_non_overlapping() {
        // "aaaa" with needle "aa" -> (0,1) and (2,3), not (1,2).
        assert_eq!(match_line("aaaa", "aa", false), vec![(0, 1), (2, 3)]);
    }

    #[test]
    fn match_line_case_insensitive() {
        assert_eq!(match_line("Hello HELLO", "hello", false), vec![(0, 4), (6, 10)]);
    }

    #[test]
    fn match_line_case_sensitive() {
        // Only the lowercase "hello" matches.
        assert_eq!(match_line("Hello hello", "hello", true), vec![(6, 10)]);
    }

    #[test]
    fn match_line_multibyte_columns() {
        // Each accented char is one column; the match must report char
        // indices, not byte offsets.
        let line = "héllo wörld";
        // "wörld" begins at char 6 (h é l l o space) and ends at char 10.
        assert_eq!(match_line(line, "wörld", false), vec![(6, 10)]);
    }

    #[test]
    fn match_line_multibyte_query_case_insensitive() {
        assert_eq!(match_line("CAFÉ café", "café", false), vec![(0, 3), (5, 8)]);
    }

    #[test]
    fn ring_next_prev_wrap() {
        let mut s = Search::default();
        s.set_matches(vec![
            Match { line: 0, start_col: 0, end_col: 1 },
            Match { line: 1, start_col: 0, end_col: 1 },
            Match { line: 2, start_col: 0, end_col: 1 },
        ]);
        assert_eq!(s.current, 0);
        s.next();
        assert_eq!(s.current, 1);
        s.next();
        s.next(); // wraps 2 -> 0
        assert_eq!(s.current, 0);
        s.prev(); // wraps 0 -> 2
        assert_eq!(s.current, 2);
    }

    #[test]
    fn ring_navigation_empty_is_noop() {
        let mut s = Search::default();
        s.next();
        s.prev();
        assert_eq!(s.current, 0);
        assert!(s.current_match().is_none());
    }

    #[test]
    fn set_matches_clamps_current() {
        let mut s = Search::default();
        s.set_matches(vec![
            Match { line: 0, start_col: 0, end_col: 0 },
            Match { line: 1, start_col: 0, end_col: 0 },
            Match { line: 2, start_col: 0, end_col: 0 },
        ]);
        s.current = 2;
        // Shrinking the list below `current` resets it to 0.
        s.set_matches(vec![Match { line: 9, start_col: 0, end_col: 0 }]);
        assert_eq!(s.current, 0);
    }

    #[test]
    fn toggle_and_close_reset_state() {
        let mut s = Search::default();
        s.toggle();
        assert!(s.open);
        s.type_char('x');
        s.set_matches(vec![Match { line: 0, start_col: 0, end_col: 0 }]);
        s.toggle();
        assert!(!s.open);
        assert!(s.input.value.is_empty());
        assert!(s.matches.is_empty());
        assert_eq!(s.current, 0);
    }
}
