// Paul-Williams-style VT state machine. We only implement the slice of VT we
// care about: C0 control chars, a subset of CSI, a few ESC one-shots, and
// OSC (swallowed). DCS, SOS, PM, APC, and C1 are not parsed.

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Print(char),

    // C0 controls
    Bell,
    Backspace,
    Tab,
    LineFeed,
    CarriageReturn,

    // Cursor movement (all params are clamped to >= 1 by the parser)
    CursorUp(u16),
    CursorDown(u16),
    CursorForward(u16),
    CursorBack(u16),
    // Row, col — 1-based. Either may be omitted; parser substitutes 1.
    CursorPosition(u16, u16),
    CursorHorizontalAbs(u16),
    CursorVerticalAbs(u16),

    // Erase — 0: cursor→end, 1: start→cursor, 2: all, 3: scrollback (ED only)
    EraseInDisplay(u16),
    EraseInLine(u16),

    // Scrolling
    ScrollUp(u16),
    ScrollDown(u16),
    // top/bottom 1-based. None = default (full screen / no region).
    SetScrollRegion(Option<u16>, Option<u16>),

    Sgr(Vec<u16>),

    // DEC private mode (CSI ? N h/l). One event per param.
    PrivateModeSet(u16),
    PrivateModeReset(u16),

    // ESC one-shots
    SaveCursor,    // ESC 7
    RestoreCursor, // ESC 8
    FullReset,     // ESC c

    // Unsupported sequences are silently dropped; tests assert this.
}

enum State {
    Ground,
    Escape,
    CsiEntry,
    CsiParam,
    CsiIntermediate,
    CsiIgnore,
    OscString,
    OscEsc,
}

pub struct Parser {
    state: State,
    params: Vec<u16>,
    // Accumulating digits for the current param. When we hit ';' or final byte
    // we flush this into `params`.
    cur_param: Option<u16>,
    private: bool,
}

impl Parser {
    pub fn new() -> Self {
        Self {
            state: State::Ground,
            params: Vec::with_capacity(8),
            cur_param: None,
            private: false,
        }
    }

    pub fn feed(&mut self, ch: char, mut emit: impl FnMut(Event)) {
        match self.state {
            State::Ground => self.ground(ch, &mut emit),
            State::Escape => self.escape(ch, &mut emit),
            State::CsiEntry => self.csi_entry(ch, &mut emit),
            State::CsiParam => self.csi_param(ch, &mut emit),
            State::CsiIntermediate => self.csi_intermediate(ch, &mut emit),
            State::CsiIgnore => self.csi_ignore(ch),
            State::OscString => self.osc(ch),
            State::OscEsc => self.osc_esc(ch),
        }
    }

    fn ground(&mut self, ch: char, emit: &mut impl FnMut(Event)) {
        match ch {
            '\x07' => emit(Event::Bell),
            '\x08' => emit(Event::Backspace),
            '\t' => emit(Event::Tab),
            '\n' => emit(Event::LineFeed),
            '\r' => emit(Event::CarriageReturn),
            '\x1b' => self.state = State::Escape,
            c if (c as u32) < 0x20 => {} // ignore other C0
            c => emit(Event::Print(c)),
        }
    }

    fn escape(&mut self, ch: char, emit: &mut impl FnMut(Event)) {
        match ch {
            '[' => {
                self.reset_csi();
                self.state = State::CsiEntry;
            }
            ']' => self.state = State::OscString,
            '7' => {
                emit(Event::SaveCursor);
                self.state = State::Ground;
            }
            '8' => {
                emit(Event::RestoreCursor);
                self.state = State::Ground;
            }
            'c' => {
                emit(Event::FullReset);
                self.state = State::Ground;
            }
            _ => self.state = State::Ground,
        }
    }

    fn reset_csi(&mut self) {
        self.params.clear();
        self.cur_param = None;
        self.private = false;
    }

    fn csi_entry(&mut self, ch: char, emit: &mut impl FnMut(Event)) {
        match ch {
            '?' => {
                self.private = true;
                self.state = State::CsiParam;
            }
            '<' | '=' | '>' => self.state = State::CsiIgnore,
            '0'..='9' => {
                self.push_digit(ch);
                self.state = State::CsiParam;
            }
            ';' => {
                self.flush_param();
                self.state = State::CsiParam;
            }
            ' '..='/' => self.state = State::CsiIntermediate, // 0x20-0x2F
            '@'..='~' => {
                self.dispatch_csi(ch, emit);
                self.state = State::Ground;
            }
            _ => self.state = State::Ground,
        }
    }

    fn csi_param(&mut self, ch: char, emit: &mut impl FnMut(Event)) {
        match ch {
            '0'..='9' => self.push_digit(ch),
            ';' => self.flush_param(),
            ' '..='/' => self.state = State::CsiIntermediate,
            '@'..='~' => {
                self.dispatch_csi(ch, emit);
                self.state = State::Ground;
            }
            _ => self.state = State::Ground,
        }
    }

    fn csi_intermediate(&mut self, ch: char, emit: &mut impl FnMut(Event)) {
        match ch {
            ' '..='/' => {} // collect (we don't use intermediates)
            '@'..='~' => {
                self.dispatch_csi(ch, emit);
                self.state = State::Ground;
            }
            _ => self.state = State::Ground,
        }
    }

    fn csi_ignore(&mut self, ch: char) {
        // Consume through final byte, emit nothing.
        if ('@'..='~').contains(&ch) {
            self.state = State::Ground;
        }
    }

    fn osc(&mut self, ch: char) {
        match ch {
            '\x07' => self.state = State::Ground,
            '\x1b' => self.state = State::OscEsc,
            _ => {} // swallow
        }
    }

    fn osc_esc(&mut self, _ch: char) {
        // Either ESC \ terminates (we got \); anything else aborts. Treat both
        // the same — state machine isn't precise enough to care.
        self.state = State::Ground;
    }

    fn push_digit(&mut self, ch: char) {
        let d = (ch as u8 - b'0') as u16;
        let p = self.cur_param.unwrap_or(0);
        // saturate, don't wrap
        self.cur_param = Some(p.saturating_mul(10).saturating_add(d));
    }

    fn flush_param(&mut self) {
        self.params.push(self.cur_param.unwrap_or(0));
        self.cur_param = None;
    }

    fn dispatch_csi(&mut self, final_byte: char, emit: &mut impl FnMut(Event)) {
        // Commit any trailing digits as a final param.
        if self.cur_param.is_some() {
            self.flush_param();
        }

        if self.private {
            self.dispatch_private_csi(final_byte, emit);
            return;
        }

        let p0 = self.params.first().copied();
        let p1 = self.params.get(1).copied();
        let get1 = |p: Option<u16>| p.and_then(|v| if v == 0 { None } else { Some(v) }).unwrap_or(1);

        match final_byte {
            'A' => emit(Event::CursorUp(get1(p0))),
            'B' | 'e' => emit(Event::CursorDown(get1(p0))),
            'C' | 'a' => emit(Event::CursorForward(get1(p0))),
            'D' => emit(Event::CursorBack(get1(p0))),
            'G' | '`' => emit(Event::CursorHorizontalAbs(get1(p0))),
            'd' => emit(Event::CursorVerticalAbs(get1(p0))),
            'H' | 'f' => emit(Event::CursorPosition(get1(p0), get1(p1))),
            'J' => emit(Event::EraseInDisplay(p0.unwrap_or(0))),
            'K' => emit(Event::EraseInLine(p0.unwrap_or(0))),
            'S' => emit(Event::ScrollUp(get1(p0))),
            'T' => emit(Event::ScrollDown(get1(p0))),
            'r' => emit(Event::SetScrollRegion(
                p0.filter(|&v| v != 0),
                p1.filter(|&v| v != 0),
            )),
            'm' => emit(Event::Sgr(std::mem::take(&mut self.params))),
            _ => {} // unsupported final byte — silently drop
        }
    }

    fn dispatch_private_csi(&mut self, final_byte: char, emit: &mut impl FnMut(Event)) {
        match final_byte {
            'h' => {
                for &p in &self.params {
                    emit(Event::PrivateModeSet(p));
                }
            }
            'l' => {
                for &p in &self.params {
                    emit(Event::PrivateModeReset(p));
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(input: &str) -> Vec<Event> {
        let mut p = Parser::new();
        let mut out = Vec::new();
        for ch in input.chars() {
            p.feed(ch, |e| out.push(e));
        }
        out
    }

    #[test]
    fn plain_text() {
        assert_eq!(collect("ab"), vec![Event::Print('a'), Event::Print('b')]);
    }

    #[test]
    fn c0_controls() {
        assert_eq!(
            collect("\x07\x08\t\r\n"),
            vec![
                Event::Bell,
                Event::Backspace,
                Event::Tab,
                Event::CarriageReturn,
                Event::LineFeed,
            ],
        );
    }

    #[test]
    fn cursor_up_default_is_1() {
        assert_eq!(collect("\x1b[A"), vec![Event::CursorUp(1)]);
    }

    #[test]
    fn cursor_up_explicit() {
        assert_eq!(collect("\x1b[5A"), vec![Event::CursorUp(5)]);
    }

    #[test]
    fn cursor_position_full() {
        assert_eq!(
            collect("\x1b[3;10H"),
            vec![Event::CursorPosition(3, 10)],
        );
    }

    #[test]
    fn cursor_position_defaults_to_1_1() {
        assert_eq!(collect("\x1b[H"), vec![Event::CursorPosition(1, 1)]);
    }

    #[test]
    fn cursor_position_partial_defaults() {
        // "\x1b[;5H" → row defaults to 1, col=5
        assert_eq!(collect("\x1b[;5H"), vec![Event::CursorPosition(1, 5)]);
        // "\x1b[3;H" → row=3, col defaults to 1
        assert_eq!(collect("\x1b[3;H"), vec![Event::CursorPosition(3, 1)]);
    }

    #[test]
    fn erase_display_and_line() {
        assert_eq!(
            collect("\x1b[2J\x1b[0K"),
            vec![Event::EraseInDisplay(2), Event::EraseInLine(0)],
        );
    }

    #[test]
    fn sgr_reset_empty() {
        assert_eq!(collect("\x1b[m"), vec![Event::Sgr(vec![])]);
    }

    #[test]
    fn sgr_multi() {
        assert_eq!(
            collect("\x1b[1;31;48;5;16m"),
            vec![Event::Sgr(vec![1, 31, 48, 5, 16])],
        );
    }

    #[test]
    fn private_mode_set_and_reset() {
        assert_eq!(
            collect("\x1b[?25h\x1b[?25l"),
            vec![Event::PrivateModeSet(25), Event::PrivateModeReset(25)],
        );
    }

    #[test]
    fn private_mode_multi() {
        // ?1049h and ?25h in one sequence
        assert_eq!(
            collect("\x1b[?25;1049h"),
            vec![Event::PrivateModeSet(25), Event::PrivateModeSet(1049)],
        );
    }

    #[test]
    fn set_scroll_region() {
        assert_eq!(
            collect("\x1b[3;20r"),
            vec![Event::SetScrollRegion(Some(3), Some(20))],
        );
        assert_eq!(
            collect("\x1b[r"),
            vec![Event::SetScrollRegion(None, None)],
        );
    }

    #[test]
    fn esc_save_restore_reset() {
        assert_eq!(
            collect("\x1b7\x1b8\x1bc"),
            vec![Event::SaveCursor, Event::RestoreCursor, Event::FullReset],
        );
    }

    #[test]
    fn osc_terminated_by_bel_or_st_is_swallowed() {
        assert_eq!(
            collect("a\x1b]0;title\x07b"),
            vec![Event::Print('a'), Event::Print('b')],
        );
        assert_eq!(
            collect("a\x1b]0;title\x1b\\b"),
            vec![Event::Print('a'), Event::Print('b')],
        );
    }

    #[test]
    fn unsupported_csi_is_swallowed() {
        // no handler for 'Z' (CBT) — consumed, no event
        assert_eq!(collect("a\x1b[2Zb"), vec![Event::Print('a'), Event::Print('b')]);
    }

    #[test]
    fn greater_than_introducer_is_ignored() {
        // \x1b[>c is the "send device attributes" query from the host;
        // treat as non-SGR private and swallow.
        assert_eq!(collect("a\x1b[>cb"), vec![Event::Print('a'), Event::Print('b')]);
    }

    #[test]
    fn esc_split_across_feeds() {
        let mut p = Parser::new();
        let mut out = Vec::new();
        for ch in "a\x1b".chars() {
            p.feed(ch, |e| out.push(e));
        }
        for ch in "[31mb".chars() {
            p.feed(ch, |e| out.push(e));
        }
        assert_eq!(
            out,
            vec![Event::Print('a'), Event::Sgr(vec![31]), Event::Print('b')],
        );
    }

    #[test]
    fn param_saturates_not_wraps() {
        // 65536 wraps to 0 in u16 if we use wrapping — assert we saturate.
        let ev = collect("\x1b[99999A");
        assert!(matches!(ev.as_slice(), &[Event::CursorUp(u16::MAX)]));
    }
}
