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
    // DECSLRM — CSI Pl ; Pr s. left/right 1-based. None for both = the
    // ambiguous CSI s form: either reset-margins (when DECLRMM is on) or
    // SCOSC save-cursor (when DECLRMM is off). The terminal disambiguates.
    SetLeftRightMargin(Option<u16>, Option<u16>),

    // Editing — all params clamped to >= 1
    InsertLine(u16),  // CSI L (IL)
    DeleteLine(u16),  // CSI M (DL)
    InsertChar(u16),  // CSI @ (ICH)
    DeleteChar(u16),  // CSI P (DCH)
    EraseChar(u16),   // CSI X (ECH)

    // CSI <n>n — DSR. 5 = ask if OK, 6 = report cursor position. Caller is
    // expected to write a reply back to the host.
    DeviceStatusReport(u16),

    // CSI c (or CSI 0c) — Primary Device Attributes. Caller replies with
    // identification (e.g. \e[?1;2c).
    DeviceAttributes,
    // CSI > c — Secondary Device Attributes (terminal type / version).
    SecondaryDeviceAttributes,

    // CSI Ps t — XTWINOPS window-manipulation query. Apps use this to
    // ask the terminal for its size in pixels or cells:
    //   14 → text area in pixels       → \e[4;<height>;<width>t
    //   16 → cell size in pixels       → \e[6;<height>;<width>t
    //   18 → text area in characters   → \e[8;<rows>;<cols>t
    // Only the query subset is parsed; the action subset (resize, move,
    // raise, etc.) is ignored. Kitty's icat depends on 14/16 for its
    // pixel-precise placement math.
    XtwinopsQuery(u16),

    // CSI <n> SP q — DECSCUSR. 0/1=blink block, 2=block, 3=blink underline,
    // 4=underline, 5=blink bar, 6=bar.
    SetCursorStyle(u16),

    // OSC payload, decoded between OSC introducer and the BEL/ST terminator.
    // Format is typically "<Pn>;<rest>" — caller dispatches by code.
    Osc(String),

    // DCS payload (everything between ESC P and ST). Used by xterm's
    // XTGETTCAP termcap query — apps like vim probe terminal capabilities
    // here on startup. SOS/PM are still swallowed silently.
    Dcs(String),

    // APC payload (everything between ESC _ and ST). Used by Kitty's
    // graphics protocol — `\e_G<ctrl>;<base64>\e\\` for inline images.
    // The terminal-level dispatcher (`handle_apc`) is what decides
    // whether the payload is interesting; this layer just captures.
    Apc(String),

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
    // ESC followed by a 0x20-0x2F intermediate (SCS designators like
    // `ESC ( B`, `ESC ) 0`, plus `ESC SP F/G/L/M/N` for ANSI conformance).
    // We don't implement charset switching, but we must still consume the
    // final byte or it leaks into Ground and gets printed.
    EscIntermediate,
    CsiEntry,
    CsiParam,
    CsiIntermediate,
    CsiIgnore,
    OscString,
    OscEsc,
    // DCS payload, captured for the host (XTGETTCAP queries land here).
    DcsString,
    DcsEsc,
    // APC payload, captured for the Kitty graphics protocol dispatcher.
    ApcString,
    ApcEsc,
    // SOS / PM payload — same framing (BEL or ESC \) but no consumer,
    // so we discard the bytes rather than buffer them.
    StringSwallow,
    StringEsc,
}

pub struct Parser {
    state: State,
    params: Vec<u16>,
    // Accumulating digits for the current param. When we hit ';' or final byte
    // we flush this into `params`.
    cur_param: Option<u16>,
    private: bool,
    // `>` introducer (secondary DA, modify-other-keys queries…).
    intro_gt: bool,
    // Last intermediate byte (0x20-0x2F) seen in the CSI. We only check for
    // a single space, which selects DECSCUSR-style finals like `q`.
    intermediate: Option<char>,
    // OSC payload accumulator. Flushed on BEL or ST.
    osc_buf: String,
    // DCS payload accumulator. Same lifecycle as `osc_buf`.
    dcs_buf: String,
    // APC payload accumulator. Kitty graphics chunks land here.
    apc_buf: String,
}

impl Parser {
    pub fn new() -> Self {
        Self {
            state: State::Ground,
            params: Vec::with_capacity(8),
            cur_param: None,
            private: false,
            intro_gt: false,
            intermediate: None,
            osc_buf: String::new(),
            dcs_buf: String::new(),
            apc_buf: String::new(),
        }
    }

    pub fn feed(&mut self, ch: char, mut emit: impl FnMut(Event)) {
        match self.state {
            State::Ground => self.ground(ch, &mut emit),
            State::Escape => self.escape(ch, &mut emit),
            State::EscIntermediate => self.esc_intermediate(ch),
            State::CsiEntry => self.csi_entry(ch, &mut emit),
            State::CsiParam => self.csi_param(ch, &mut emit),
            State::CsiIntermediate => self.csi_intermediate(ch, &mut emit),
            State::CsiIgnore => self.csi_ignore(ch),
            State::OscString => self.osc(ch, &mut emit),
            State::OscEsc => self.osc_esc(ch, &mut emit),
            State::DcsString => self.dcs(ch, &mut emit),
            State::DcsEsc => self.dcs_esc(ch, &mut emit),
            State::ApcString => self.apc(ch, &mut emit),
            State::ApcEsc => self.apc_esc(ch, &mut emit),
            State::StringSwallow => self.string_swallow(ch),
            State::StringEsc => self.string_esc(),
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
            // DCS — captured for XTGETTCAP and similar.
            'P' => self.state = State::DcsString,
            // APC payload — captured for the Kitty graphics dispatcher.
            // SOS ('X') / PM ('^') are still swallowed — must be consumed
            // up to ST so their payload doesn't leak into ground state and
            // get printed verbatim.
            '_' => self.state = State::ApcString,
            'X' | '^' => self.state = State::StringSwallow,
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
            // Intermediate byte (SCS designators `( ) * +`, `SP` for ANSI
            // conformance, etc.). Wait for the final byte so it doesn't
            // leak into Ground — e.g. `ESC ( B` would print a stray "B".
            ' '..='/' => self.state = State::EscIntermediate,
            _ => self.state = State::Ground,
        }
    }

    fn esc_intermediate(&mut self, ch: char) {
        // Stay in this state as long as additional intermediates arrive;
        // any final byte (0x30-0x7E) terminates the sequence. We don't
        // implement charset switching, so the dispatch is a no-op.
        //
        // TODO: if we ever need to support legacy ncurses apps that draw
        // boxes via DEC Special Graphics (`ESC ( 0` then ASCII letters
        // remapped to line-drawing — `q`→─, `x`→│, `l`→┌, etc.), wire it
        // up here. Sketch: track `g0`/`g1` charset on the Parser, dispatch
        // on `(intermediate, ch)` to set them, handle SI (0x0F) / SO (0x0E)
        // in `ground()` to toggle the active slot, and remap chars in
        // `Event::Print` before emission. Modern apps (tmux, vim, anything
        // UTF-8) use Unicode box-drawing directly and don't need this.
        if !(' '..='/').contains(&ch) {
            self.state = State::Ground;
        }
    }

    fn reset_csi(&mut self) {
        self.params.clear();
        self.cur_param = None;
        self.private = false;
        self.intro_gt = false;
        self.intermediate = None;
    }

    fn csi_entry(&mut self, ch: char, emit: &mut impl FnMut(Event)) {
        match ch {
            '?' => {
                self.private = true;
                self.state = State::CsiParam;
            }
            '>' => {
                self.intro_gt = true;
                self.state = State::CsiParam;
            }
            '<' | '=' => self.state = State::CsiIgnore,
            '0'..='9' => {
                self.push_digit(ch);
                self.state = State::CsiParam;
            }
            // `;` is the legacy separator; `:` is ITU T.416 / ECMA-48
            // sub-parameter separator. Modern apps (and `kitty +kitten
            // icat`'s unicode-placeholder SGR) emit truecolor as
            // `\e[38:2:R:G:Bm` rather than `\e[38;2;R;G;Bm`.
            // Treating them the same here lets the existing SGR
            // handler see `[38, 2, R, G, B]` and dispatch truecolor
            // correctly. Worst case (advanced underline `4:N`): the
            // sub-param leaks as a separate top-level SGR, which is a
            // slight overload but better than aborting the whole CSI.
            ';' | ':' => {
                self.flush_param();
                self.state = State::CsiParam;
            }
            ' '..='/' => {
                self.intermediate = Some(ch);
                self.state = State::CsiIntermediate;
            }
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
            // See the equivalent `;` | `:` branch in `csi_entry`.
            ';' | ':' => self.flush_param(),
            ' '..='/' => {
                self.intermediate = Some(ch);
                self.state = State::CsiIntermediate;
            }
            '@'..='~' => {
                self.dispatch_csi(ch, emit);
                self.state = State::Ground;
            }
            _ => self.state = State::Ground,
        }
    }

    fn csi_intermediate(&mut self, ch: char, emit: &mut impl FnMut(Event)) {
        match ch {
            ' '..='/' => self.intermediate = Some(ch),
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

    fn osc(&mut self, ch: char, emit: &mut impl FnMut(Event)) {
        match ch {
            '\x07' => {
                self.flush_osc(emit);
                self.state = State::Ground;
            }
            '\x1b' => self.state = State::OscEsc,
            _ => self.osc_buf.push(ch),
        }
    }

    fn osc_esc(&mut self, ch: char, emit: &mut impl FnMut(Event)) {
        // ESC \ is the proper ST terminator; anything else aborts the OSC
        // string without emitting (the bytes were already accumulated, so
        // drop them).
        if ch == '\\' {
            self.flush_osc(emit);
        } else {
            self.osc_buf.clear();
        }
        self.state = State::Ground;
    }

    fn dcs(&mut self, ch: char, emit: &mut impl FnMut(Event)) {
        match ch {
            '\x07' => {
                self.flush_dcs(emit);
                self.state = State::Ground;
            }
            '\x1b' => self.state = State::DcsEsc,
            _ => self.dcs_buf.push(ch),
        }
    }

    fn dcs_esc(&mut self, ch: char, emit: &mut impl FnMut(Event)) {
        match ch {
            // Proper String Terminator: emit the DCS and return to ground.
            '\\' => {
                self.flush_dcs(emit);
                self.state = State::Ground;
            }
            // tmux passthrough escapes a literal ESC inside the DCS body
            // by doubling it (`ESC ESC`). When we see two consecutive
            // ESCs, treat the pair as a single ESC byte in the body and
            // stay in DcsString — there's no legitimate DCS use case for
            // two unescaped ESCs in a row anyway. Without this the next
            // dcs_esc call would clear the buf and abort the whole DCS,
            // and any tmux-wrapped content would never reach the
            // dispatcher.
            '\x1b' => {
                self.dcs_buf.push('\x1b');
                self.state = State::DcsString;
            }
            // Anything else after ESC inside DCS aborts the sequence
            // silently — same policy the OSC / APC paths use.
            _ => {
                self.dcs_buf.clear();
                self.state = State::Ground;
            }
        }
    }

    fn flush_dcs(&mut self, emit: &mut impl FnMut(Event)) {
        let s = std::mem::take(&mut self.dcs_buf);
        if !s.is_empty() {
            emit(Event::Dcs(s));
        }
    }

    fn apc(&mut self, ch: char, emit: &mut impl FnMut(Event)) {
        match ch {
            '\x07' => {
                self.flush_apc(emit);
                self.state = State::Ground;
            }
            '\x1b' => self.state = State::ApcEsc,
            _ => self.apc_buf.push(ch),
        }
    }

    fn apc_esc(&mut self, ch: char, emit: &mut impl FnMut(Event)) {
        // ESC \ is the proper ST terminator; anything else aborts the APC
        // string without emitting (matching the OSC/DCS contract).
        if ch == '\\' {
            self.flush_apc(emit);
        } else {
            self.apc_buf.clear();
        }
        self.state = State::Ground;
    }

    fn flush_apc(&mut self, emit: &mut impl FnMut(Event)) {
        let s = std::mem::take(&mut self.apc_buf);
        if !s.is_empty() {
            emit(Event::Apc(s));
        }
    }

    fn string_swallow(&mut self, ch: char) {
        match ch {
            '\x07' => self.state = State::Ground,
            '\x1b' => self.state = State::StringEsc,
            _ => {} // swallow payload
        }
    }

    fn string_esc(&mut self) {
        // ESC inside a string sequence either terminates it (ESC \\ — proper
        // ST) or aborts. Either way, return to ground without emitting.
        self.state = State::Ground;
    }

    fn flush_osc(&mut self, emit: &mut impl FnMut(Event)) {
        let s = std::mem::take(&mut self.osc_buf);
        if !s.is_empty() {
            emit(Event::Osc(s));
        }
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

        // DECSCUSR (CSI <n> SP q) is the only intermediate-bearing sequence we
        // care about; drop any other intermediate-laden sequence rather than
        // misinterpreting the final byte.
        if self.intermediate == Some(' ') && final_byte == 'q' {
            let p0 = self.params.first().copied();
            emit(Event::SetCursorStyle(p0.unwrap_or(0)));
            return;
        }
        if self.intermediate.is_some() {
            return;
        }

        if self.intro_gt {
            if final_byte == 'c' {
                emit(Event::SecondaryDeviceAttributes);
            }
            return;
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
            'L' => emit(Event::InsertLine(get1(p0))),
            'M' => emit(Event::DeleteLine(get1(p0))),
            '@' => emit(Event::InsertChar(get1(p0))),
            'P' => emit(Event::DeleteChar(get1(p0))),
            'X' => emit(Event::EraseChar(get1(p0))),
            'n' => emit(Event::DeviceStatusReport(p0.unwrap_or(0))),
            'c' => emit(Event::DeviceAttributes),
            'r' => emit(Event::SetScrollRegion(
                p0.filter(|&v| v != 0),
                p1.filter(|&v| v != 0),
            )),
            // DECSLRM. Always emit — the terminal layer decides whether to
            // apply it as margin-set or interpret bare `s` as SCOSC.
            's' => emit(Event::SetLeftRightMargin(
                p0.filter(|&v| v != 0),
                p1.filter(|&v| v != 0),
            )),
            'm' => emit(Event::Sgr(std::mem::take(&mut self.params))),
            // XTWINOPS — only the report-size subset; resize/move/etc.
            // are silently ignored to avoid letting apps move the
            // window without user consent.
            't' => {
                if let Some(ps) = p0 {
                    if matches!(ps, 14 | 16 | 18) {
                        emit(Event::XtwinopsQuery(ps));
                    }
                }
            }
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
    fn csi_accepts_colon_as_subparam_separator() {
        // ITU T.416 / ECMA-48 form: colon separates sub-params of a
        // single parameter. Modern apps (notably `kitty +kitten
        // icat`'s unicode-placeholder SGR) emit truecolor as
        // `\e[38:2:R:G:Bm` instead of `\e[38;2;R;G;Bm`. The pre-fix
        // parser aborted the whole CSI on `:`, so the SGR never
        // applied, the placeholder cell's fg color was never set,
        // and `decode_kitty_placeholder_image_id` returned None —
        // visible as a grid of tofu instead of the image.
        assert_eq!(
            collect("\x1b[38:2:252:37:152m"),
            vec![Event::Sgr(vec![38, 2, 252, 37, 152])],
        );
        // Mixed separators in a single sequence still work — params
        // accumulate uniformly regardless of which delimiter was used.
        assert_eq!(
            collect("\x1b[38:2;252:37;152m"),
            vec![Event::Sgr(vec![38, 2, 252, 37, 152])],
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
    fn osc_emits_payload_on_bel_or_st() {
        assert_eq!(
            collect("a\x1b]0;title\x07b"),
            vec![Event::Print('a'), Event::Osc("0;title".into()), Event::Print('b')],
        );
        assert_eq!(
            collect("a\x1b]11;?\x1b\\b"),
            vec![Event::Print('a'), Event::Osc("11;?".into()), Event::Print('b')],
        );
    }

    #[test]
    fn dcs_payload_emits_event() {
        // Vim's startup probe: DCS + q <hex names> ST.
        assert_eq!(
            collect("a\x1bP+q436f;6b75\x1b\\b"),
            vec![
                Event::Print('a'),
                Event::Dcs("+q436f;6b75".into()),
                Event::Print('b'),
            ],
        );
    }

    #[test]
    fn dcs_terminated_by_bel_emits_event() {
        assert_eq!(
            collect("a\x1bPjunk\x07b"),
            vec![Event::Print('a'), Event::Dcs("junk".into()), Event::Print('b')],
        );
    }

    #[test]
    fn dcs_doubled_esc_is_treated_as_literal_esc_in_body() {
        // tmux passthrough convention: inside `ESC P tmux; ...ESC\\`,
        // a literal ESC byte is doubled. The parser must un-double
        // them rather than abort the DCS on the first internal ESC.
        // Without this, the next dcs_esc call would clear the buf
        // and any tmux-wrapped content would silently vanish.
        let s = "\x1bPtmux;\x1b\x1b_Gpayload\x1b\\";
        assert_eq!(
            collect(s),
            vec![Event::Dcs("tmux;\x1b_Gpayload".into())],
        );
    }

    #[test]
    fn dcs_unrecognized_esc_intro_still_aborts() {
        // A solitary ESC followed by something that isn't another
        // ESC (the tmux-escape signal) and isn't `\\` (the proper
        // ST) is a malformed DCS — abort silently, same policy as
        // before the tmux-passthrough change. The aborting char is
        // consumed by the dcs_esc transition; subsequent chars land
        // back in Ground and print as ordinary text.
        let s = "a\x1bPbody\x1bXmore\x1b\\b";
        assert_eq!(
            collect(s),
            vec![
                Event::Print('a'),
                Event::Print('m'),
                Event::Print('o'),
                Event::Print('r'),
                Event::Print('e'),
                Event::Print('b'),
            ],
        );
    }

    #[test]
    fn sos_pm_are_swallowed() {
        // APC moved to its own Event variant for the Kitty graphics
        // dispatcher; SOS/PM are still consumed-but-not-emitted.
        for intro in ['X', '^'] {
            let s = format!("a\x1b{}payload\x1b\\b", intro);
            assert_eq!(
                collect(&s),
                vec![Event::Print('a'), Event::Print('b')],
                "introducer {intro:?}",
            );
        }
    }

    #[test]
    fn apc_payload_emits_event_with_st_terminator() {
        assert_eq!(
            collect("a\x1b_GfooBar\x1b\\b"),
            vec![Event::Print('a'), Event::Apc("GfooBar".into()), Event::Print('b')],
        );
    }

    #[test]
    fn apc_payload_emits_event_with_bel_terminator() {
        assert_eq!(
            collect("a\x1b_GfooBar\x07b"),
            vec![Event::Print('a'), Event::Apc("GfooBar".into()), Event::Print('b')],
        );
    }

    #[test]
    fn apc_payload_aborted_by_non_st_esc_does_not_emit() {
        assert_eq!(
            collect("a\x1b_Gfoo\x1bZb"),
            vec![Event::Print('a'), Event::Print('b')],
        );
    }

    #[test]
    fn osc_aborted_by_other_esc_does_not_emit() {
        // ESC followed by something other than \ aborts without emitting.
        assert_eq!(
            collect("a\x1b]0;x\x1bZb"),
            vec![Event::Print('a'), Event::Print('b')],
        );
    }

    #[test]
    fn unsupported_csi_is_swallowed() {
        // no handler for 'Z' (CBT) — consumed, no event
        assert_eq!(collect("a\x1b[2Zb"), vec![Event::Print('a'), Event::Print('b')]);
    }

    #[test]
    fn primary_and_secondary_device_attributes() {
        assert_eq!(
            collect("\x1b[c\x1b[0c\x1b[>c"),
            vec![
                Event::DeviceAttributes,
                Event::DeviceAttributes,
                Event::SecondaryDeviceAttributes,
            ],
        );
    }

    #[test]
    fn decscusr_with_space_intermediate() {
        assert_eq!(
            collect("\x1b[2 q\x1b[ q"),
            vec![Event::SetCursorStyle(2), Event::SetCursorStyle(0)],
        );
    }

    #[test]
    fn unknown_intermediate_drops_sequence() {
        // CSI 2 ' p — intermediate '\'' isn't one we recognize, so silent drop.
        assert_eq!(collect("a\x1b[2'pb"), vec![Event::Print('a'), Event::Print('b')]);
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
    fn editing_csi_emits_expected_events() {
        assert_eq!(
            collect("\x1b[3L\x1b[2M\x1b[5@\x1b[4P\x1b[6X"),
            vec![
                Event::InsertLine(3),
                Event::DeleteLine(2),
                Event::InsertChar(5),
                Event::DeleteChar(4),
                Event::EraseChar(6),
            ],
        );
    }

    #[test]
    fn editing_csi_defaults_to_1() {
        assert_eq!(
            collect("\x1b[L\x1b[M\x1b[@\x1b[P\x1b[X"),
            vec![
                Event::InsertLine(1),
                Event::DeleteLine(1),
                Event::InsertChar(1),
                Event::DeleteChar(1),
                Event::EraseChar(1),
            ],
        );
    }

    #[test]
    fn device_status_report_parses() {
        assert_eq!(
            collect("\x1b[6n\x1b[5n\x1b[n"),
            vec![
                Event::DeviceStatusReport(6),
                Event::DeviceStatusReport(5),
                Event::DeviceStatusReport(0),
            ],
        );
    }

    #[test]
    fn esc_scs_designators_are_consumed() {
        // SCS sequences `ESC ( B`, `ESC ) 0`, `ESC * A`, `ESC + B`. We don't
        // implement charset switching but must not print the final byte.
        // Regression: tmux/starship-style prompts terminate styled segments
        // with `ESC ( B`, which previously leaked a stray "B" into output.
        assert_eq!(
            collect("a\x1b(Bb\x1b)0c\x1b*Ad\x1b+Be"),
            vec![
                Event::Print('a'),
                Event::Print('b'),
                Event::Print('c'),
                Event::Print('d'),
                Event::Print('e'),
            ],
        );
    }

    #[test]
    fn esc_space_intermediate_is_consumed() {
        // `ESC SP F/G/L/M/N` — ANSI conformance / 7-vs-8-bit controls. Same
        // shape as SCS: intermediate byte then a final. Must not print.
        assert_eq!(
            collect("a\x1b Fb\x1b Gc"),
            vec![Event::Print('a'), Event::Print('b'), Event::Print('c')],
        );
    }

    #[test]
    fn param_saturates_not_wraps() {
        // 65536 wraps to 0 in u16 if we use wrapping — assert we saturate.
        let ev = collect("\x1b[99999A");
        assert!(matches!(ev.as_slice(), &[Event::CursorUp(u16::MAX)]));
    }

    #[test]
    fn apc_aborted_then_valid_apc_does_not_carry_over_buffer() {
        // Regression guard: an aborted APC must clear `apc_buf` so a
        // subsequent valid APC doesn't emit the discarded payload
        // concatenated with the new one.
        let mut p = Parser::new();
        let mut out = Vec::new();
        for ch in "\x1b_Gdiscarded\x1bZ".chars() {
            p.feed(ch, |e| out.push(e));
        }
        // Aborted — no Apc event yet.
        assert!(out.is_empty(), "aborted APC must not emit");
        for ch in "\x1b_Gkept\x1b\\".chars() {
            p.feed(ch, |e| out.push(e));
        }
        // Only the second APC should be emitted, and its payload must
        // not contain bytes from the discarded sequence.
        assert_eq!(out, vec![Event::Apc("Gkept".into())]);
    }

    #[test]
    fn apc_empty_payload_emits_nothing() {
        // `flush_apc` short-circuits on empty — pin the contract so an
        // empty APC doesn't surface as an `Apc("")` event the dispatcher
        // would have to filter.
        assert_eq!(collect("\x1b_\x1b\\"), vec![]);
    }
}
