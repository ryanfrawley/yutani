    use super::*;

    /// Place a 1×1 image with default fields and return its id. Most tests
    /// care only about anchor row/col + extent, not the id or z; this keeps
    /// the test bodies focused.
    fn place(t: &mut Terminal, image: u32, row: isize, col: isize, rows: u16, cols: u16) -> u32 {
        t.insert_placement(ImageId(image), row, col, rows, cols, 0)
    }

    fn live_anchors(t: &Terminal) -> Vec<(u32, isize, isize, u16, u16)> {
        t.live_placements()
            .iter()
            .map(|p| (p.image.0, p.top_row, p.left_col, p.rows, p.cols))
            .collect()
    }

    #[test]
    fn clamp_cursor_1based_maps_and_clamps() {
        // VT params are 1-based with 0 meaning "default to 1", so both 0 and 1
        // land on index 0.
        assert_eq!(clamp_cursor_1based(0, 10), 0);
        assert_eq!(clamp_cursor_1based(1, 10), 0);
        // A normal in-range param is just v-1.
        assert_eq!(clamp_cursor_1based(5, 10), 4);
        // Past the axis it clamps to the last index.
        assert_eq!(clamp_cursor_1based(99, 10), 9);
        // A zero-sized axis must not underflow; it saturates to 0.
        assert_eq!(clamp_cursor_1based(1, 0), 0);
        assert_eq!(clamp_cursor_1based(7, 0), 0);
    }

    #[test]
    fn margin_param_0based_handles_default_and_saturation() {
        // Omitted param falls back to the (1-based) default, minus one.
        assert_eq!(margin_param_0based(None, 1), 0);
        assert_eq!(margin_param_0based(None, 24), 23);
        // A provided param is converted 1-based -> 0-based.
        assert_eq!(margin_param_0based(Some(5), 1), 4);
        // A bogus 0 param saturates to 0 rather than underflowing.
        assert_eq!(margin_param_0based(Some(0), 24), 0);
    }

    #[test]
    fn sgr_then_print_persists_color_source_on_cell() {
        // Pins the writer-path invariant `reresolve_palette_updates_*`
        // depends on: the printed cell must carry the cursor's style,
        // including its `ColorSource`. A regression here would let the
        // reresolve path appear to fail for unrelated reasons.
        let _guard = crate::palette::TEST_LOCK.lock().expect("test lock");
        crate::palette::install(crate::palette::Palette::defaults());
        let mut t = Terminal::new(5, 3, 10);
        t.feed("\x1b[31mA");
        let c = t.primary.get(0, 0);
        assert_eq!(c.ch, 'A');
        assert_eq!(c.style.fg, crate::style::CellColor::Indexed(1));
    }

    #[test]
    fn cell_colors_resolve_against_live_palette_no_rewrite() {
        // CellColor stores the source; a scheme swap is reflected at render via
        // `resolve`, with no per-cell rewrite. Verify across the live grid,
        // the alt grid, and scrollback, and that truecolor is palette-independent.
        use crate::palette;
        use crate::style::CellColor;
        let _guard = palette::TEST_LOCK.lock().expect("test lock");
        palette::install(palette::Palette::defaults());
        let mut t = Terminal::new(10, 5, 10);

        // Cell (0,0) red indexed fg; cell (1,0) truecolor bg.
        t.feed("\x1b[31mA\r\n\x1b[48;2;200;100;50mB");
        assert_eq!(t.primary.get(0, 0).style.fg, CellColor::Indexed(1));

        // Alt grid: yellow (slot 3).
        t.feed("\x1b[?1049h\x1b[33mC\x1b[?1049l");

        // Push the first painted row into scrollback.
        t.feed("\r\n\n\n\n\n\n");
        assert!(!t.scrollback.is_empty(), "fixture: row 0 should have scrolled off");
        assert_eq!(t.scrollback[0][0].ch, 'A');
        assert_eq!(t.scrollback[0][0].style.fg, CellColor::Indexed(1));

        // Swap slot 1 to bright green.
        let mut new_palette = palette::Palette::defaults();
        new_palette.ansi[1] = [0.0, 1.0, 0.0, 1.0];
        palette::install(new_palette);

        // The scrollback cell's *source* is unchanged but resolves to the new
        // color — no rewrite happened.
        assert_eq!(t.scrollback[0][0].style.fg, CellColor::Indexed(1));
        assert_eq!(t.scrollback[0][0].style.fg.resolve([0.0; 4]), [0.0, 1.0, 0.0, 1.0]);

        // Truecolor bg (now at scrollback[1][0]) resolves palette-independently.
        let tc = &t.scrollback[1][0];
        assert_eq!(tc.ch, 'B');
        assert_eq!(tc.style.bg, CellColor::Rgb([200, 100, 50]));
        assert_eq!(palette::linear_to_srgb_u8(tc.style.bg.resolve([0.0; 4])[0]), 200);

        // Alt grid cell tracks the live palette too (slot 3 unchanged here).
        assert_eq!(t.alternate.get(0, 0).style.fg, CellColor::Indexed(3));

        palette::install(palette::Palette::defaults());
    }

    /// Dump the visible grid as newline-separated row strings, with trailing
    /// spaces trimmed from each row for readability.
    fn render(t: &Terminal) -> String {
        let mut out = String::new();
        for r in 0..t.rows {
            let row: String = t.row(r).iter().map(|c| c.ch).collect();
            out.push_str(row.trim_end());
            if r + 1 < t.rows {
                out.push('\n');
            }
        }
        out
    }

    #[test]
    fn plain_text_goes_into_row_0() {
        let mut t = Terminal::new(10, 3, 100);
        t.feed("hello");
        assert_eq!(render(&t), "hello\n\n");
        assert_eq!(t.cursor().row, 0);
        assert_eq!(t.cursor().col, 5);
    }

    #[test]
    fn crlf_moves_to_next_row() {
        let mut t = Terminal::new(10, 3, 100);
        t.feed("a\r\nb");
        assert_eq!(render(&t), "a\nb\n");
        assert_eq!(t.cursor().row, 1);
        assert_eq!(t.cursor().col, 1);
    }

    #[test]
    fn cursor_position_is_1_based() {
        let mut t = Terminal::new(10, 5, 100);
        t.feed("\x1b[3;4H");
        assert_eq!(t.cursor().row, 2);
        assert_eq!(t.cursor().col, 3);
        // writing at that location
        t.feed("X");
        assert_eq!(t.row(2)[3].ch, 'X');
    }

    #[test]
    fn autowrap_end_of_line() {
        let mut t = Terminal::new(3, 3, 100);
        t.feed("abcd");
        // "abc" fills row 0, 'd' wraps to row 1 col 0
        assert_eq!(t.row(0)[0].ch, 'a');
        assert_eq!(t.row(0)[1].ch, 'b');
        assert_eq!(t.row(0)[2].ch, 'c');
        assert_eq!(t.row(1)[0].ch, 'd');
        assert_eq!(t.cursor().row, 1);
        assert_eq!(t.cursor().col, 1);
    }

    #[test]
    fn autowrap_disabled_sticks_at_last_column() {
        let mut t = Terminal::new(3, 3, 100);
        t.feed("\x1b[?7l"); // autowrap off
        t.feed("abcd");
        assert_eq!(t.row(0)[2].ch, 'd');
        assert_eq!(t.cursor().col, 2);
    }

    #[test]
    fn wide_char_occupies_two_cells() {
        let mut t = Terminal::new(10, 1, 100);
        t.feed("中x");
        assert_eq!(t.row(0)[0].ch, '中');
        assert!(t.row(0)[1].is_wide_spacer(), "second cell is a wide spacer");
        assert_eq!(t.row(0)[2].ch, 'x');
        // Cursor advanced by two for the wide char, one for 'x'.
        assert_eq!(t.cursor().col, 3);
    }

    #[test]
    fn emoji_checkmark_occupies_two_cells() {
        // The reported bug: ✅ U+2705 must reserve two columns so the shell's
        // cursor model stays in lockstep with the grid.
        let mut t = Terminal::new(10, 1, 100);
        t.feed("\u{2705}");
        assert_eq!(t.row(0)[0].ch, '\u{2705}');
        assert!(t.row(0)[1].is_wide_spacer());
        assert_eq!(t.cursor().col, 2);
    }

    #[test]
    fn wide_char_wraps_when_one_column_left() {
        // 3 cols; after "aa" only the last column is free, which can't hold a
        // two-cell glyph — it wraps to the next row, leaving the last col blank.
        let mut t = Terminal::new(3, 2, 100);
        t.feed("aa中");
        assert_eq!(t.row(0)[0].ch, 'a');
        assert_eq!(t.row(0)[1].ch, 'a');
        assert_eq!(t.row(0)[2].ch, ' ', "last column left blank — wide char didn't fit");
        assert_eq!(t.row(1)[0].ch, '中');
        assert!(t.row(1)[1].is_wide_spacer());
    }

    #[test]
    fn overwriting_wide_lead_clears_orphaned_spacer() {
        let mut t = Terminal::new(10, 1, 100);
        t.feed("中");
        t.feed("\r"); // cursor back to col 0
        t.feed("X"); // overwrite the lead half
        assert_eq!(t.row(0)[0].ch, 'X');
        assert_eq!(t.row(0)[1].ch, ' ', "orphaned spacer cleared to blank");
    }

    #[test]
    fn overwriting_wide_spacer_clears_orphaned_lead() {
        let mut t = Terminal::new(10, 1, 100);
        t.feed("中");
        t.feed("\r\x1b[1C"); // col 0, then cursor forward one → onto the spacer
        t.feed("Y"); // overwrite the spacer half
        assert_eq!(t.row(0)[0].ch, ' ', "orphaned lead cleared to blank");
        assert_eq!(t.row(0)[1].ch, 'Y');
    }

    // Resolve cell (r,c)'s grapheme: its cluster string if it has one, else the
    // single char. Mirrors what the renderer / copy do.
    fn grapheme_at(t: &Terminal, r: usize, c: usize) -> String {
        let cell = t.row(r)[c];
        match cell.cluster {
            Some(id) => t.cluster_str(id).unwrap().to_string(),
            None => cell.ch.to_string(),
        }
    }

    #[test]
    fn zwj_sequence_is_one_grapheme_two_cells() {
        let mut t = Terminal::new(10, 1, 100);
        t.feed("👨\u{200D}👩\u{200D}👧"); // family ZWJ sequence
        assert_eq!(grapheme_at(&t, 0, 0), "👨\u{200D}👩\u{200D}👧");
        assert!(t.row(0)[1].is_wide_spacer(), "base emoji keeps its 2-cell width");
        assert_eq!(t.row(0)[2].ch, ' ', "nothing spilled into col 2");
        assert_eq!(t.cursor().col, 2, "ZWJ members don't advance the cursor");
    }

    #[test]
    fn skin_tone_modifier_merges_into_base() {
        let mut t = Terminal::new(10, 1, 100);
        t.feed("👍\u{1F3FD}"); // thumbs up + medium skin tone
        assert_eq!(grapheme_at(&t, 0, 0), "👍\u{1F3FD}");
        assert_eq!(t.cursor().col, 2);
    }

    #[test]
    fn vs16_promotes_narrow_base_to_two_cell_emoji() {
        let mut t = Terminal::new(10, 1, 100);
        t.feed("\u{2764}\u{FE0F}"); // ❤ + VS16 forces emoji presentation
        assert_eq!(grapheme_at(&t, 0, 0), "\u{2764}\u{FE0F}");
        // VS16 promotes the narrow ❤ to a two-cell color emoji.
        assert!(t.row(0)[1].is_wide_spacer());
        assert_eq!(t.cursor().col, 2);
    }

    #[test]
    fn vs16_on_wide_base_does_not_double_widen() {
        // A base that's already two cells (an astral emoji) plus VS16 stays two
        // cells — the widening must not run twice.
        let mut t = Terminal::new(10, 1, 100);
        t.feed("\u{1F600}\u{FE0F}"); // 😀 + VS16
        assert_eq!(grapheme_at(&t, 0, 0), "\u{1F600}\u{FE0F}");
        assert!(t.row(0)[1].is_wide_spacer());
        assert_eq!(t.cursor().col, 2);
    }

    #[test]
    fn regional_indicator_pair_forms_one_two_cell_flag() {
        let mut t = Terminal::new(10, 1, 100);
        t.feed("🇯🇵"); // regional indicators J + P → Japan flag
        assert_eq!(grapheme_at(&t, 0, 0), "🇯🇵");
        assert!(t.row(0)[1].is_wide_spacer(), "flag pair widens to two cells");
        assert_eq!(t.cursor().col, 2);
    }

    #[test]
    fn third_regional_indicator_starts_a_new_flag() {
        let mut t = Terminal::new(10, 1, 100);
        t.feed("🇦🇧🇨"); // AB pairs into a flag; C begins a fresh (lone) one
        assert_eq!(grapheme_at(&t, 0, 0), "🇦🇧");
        assert!(t.row(0)[1].is_wide_spacer());
        assert_eq!(t.row(0)[2].ch, '🇨', "third RI is its own grapheme");
        assert!(t.row(0)[2].cluster.is_none());
    }

    #[test]
    fn combining_mark_merges_into_latin_base() {
        let mut t = Terminal::new(10, 1, 100);
        t.feed("e\u{0301}"); // e + combining acute → é
        assert_eq!(grapheme_at(&t, 0, 0), "e\u{0301}");
        assert_eq!(t.row(0)[0].ch, 'e', "lead codepoint stays the base");
        assert_eq!(t.cursor().col, 1);
    }

    #[test]
    fn cursor_move_breaks_grapheme_absorption() {
        let mut t = Terminal::new(10, 1, 100);
        t.feed("a");
        t.feed("\r"); // carriage return — cursor no longer after 'a'
        t.feed("\u{0301}"); // combining acute should NOT merge into 'a'
        assert!(t.row(0)[0].cluster.is_none(), "no merge after a cursor move");
    }

    #[test]
    fn mode_2027_disabled_falls_back_to_per_codepoint() {
        // Resetting grapheme clustering (DEC 2027) makes each codepoint land in
        // its own cell — the legacy model some apps assume when measuring text.
        let mut t = Terminal::new(10, 1, 100);
        t.feed("\x1b[?2027l");
        t.feed("e\u{0301}"); // e + combining acute
        assert!(t.row(0)[0].cluster.is_none(), "no cluster formed");
        assert_eq!(t.row(0)[0].ch, 'e');
        assert_eq!(t.row(0)[1].ch, '\u{0301}', "combining mark in its own cell");
        assert_eq!(t.cursor().col, 2, "cursor advanced once per codepoint");
    }

    #[test]
    fn mode_2027_set_restores_clustering() {
        // After turning clustering off then on again, absorption resumes.
        let mut t = Terminal::new(10, 1, 100);
        t.feed("\x1b[?2027l");
        t.feed("\x1b[?2027h");
        t.feed("e\u{0301}");
        assert_eq!(grapheme_at(&t, 0, 0), "e\u{0301}");
        assert_eq!(t.cursor().col, 1, "cluster absorbed back into one cell");
    }

    #[test]
    fn mode_2027_cleared_by_full_reset() {
        // RIS restores the power-on default, which has clustering on.
        let mut t = Terminal::new(10, 1, 100);
        t.feed("\x1b[?2027l");
        t.feed("\x1bc"); // RIS
        t.feed("e\u{0301}");
        assert_eq!(grapheme_at(&t, 0, 0), "e\u{0301}");
        assert_eq!(t.cursor().col, 1);
    }

    #[test]
    fn mode_2027_disabled_zwj_emoji_does_not_merge() {
        // With clustering off, the ZWJ family sequence is laid out codepoint by
        // codepoint: each emoji keeps its own 2-cell lead+spacer and the ZWJ
        // glue takes a narrow cell of its own. Contrast with
        // zwj_sequence_is_one_grapheme_two_cells, which fuses the whole run.
        let mut t = Terminal::new(20, 1, 100);
        t.feed("\x1b[?2027l");
        t.feed("👨\u{200D}👩"); // man + ZWJ + woman
        // No grapheme cluster anywhere along the run.
        assert!(t.row(0)[0].cluster.is_none(), "lead emoji not a cluster");
        // 👨 (wide): lead + spacer.
        assert_eq!(t.row(0)[0].ch, '👨');
        assert!(t.row(0)[1].is_wide_spacer());
        // ZWJ (narrow) lands in its own cell rather than being absorbed.
        assert_eq!(t.row(0)[2].ch, '\u{200D}');
        assert!(!t.row(0)[2].is_wide_spacer());
        // 👩 (wide): lead + spacer.
        assert_eq!(t.row(0)[3].ch, '👩');
        assert!(t.row(0)[4].is_wide_spacer());
        // Cursor advanced by the sum of per-codepoint widths: 2 + 1 + 2.
        assert_eq!(t.cursor().col, 5, "advance is sum of per-codepoint widths");
    }

    #[test]
    fn mode_2027_disabled_regional_indicators_do_not_form_flag() {
        // With clustering off, a regional-indicator pair does NOT fuse into a
        // single 2-cell flag (cf. regional_indicator_pair_forms_one_two_cell_flag).
        // Each RI is a narrow codepoint occupying its own single cell.
        let mut t = Terminal::new(10, 1, 100);
        t.feed("\x1b[?2027l");
        t.feed("🇯🇵"); // regional indicators J + P
        assert_eq!(t.row(0)[0].ch, '🇯');
        assert!(t.row(0)[0].cluster.is_none(), "no flag cluster formed");
        assert!(!t.row(0)[0].is_wide_spacer(), "RI is narrow, no spacer");
        assert_eq!(t.row(0)[1].ch, '🇵', "second RI is its own cell, not a spacer");
        assert!(t.row(0)[1].cluster.is_none());
        assert_eq!(t.cursor().col, 2, "two narrow cells, one per indicator");
    }

    #[test]
    fn mode_2027_toggle_is_independent_of_bracketed_paste() {
        // Mode 2027 and bracketed paste (2004) share the private_mode match but
        // must not alias: toggling one leaves the other untouched.
        let mut t = Terminal::new(10, 1, 100);
        // Enabling bracketed paste then disabling 2027 must keep paste enabled.
        t.feed("\x1b[?2004h");
        t.feed("\x1b[?2027l");
        assert!(t.bracketed_paste(), "disabling 2027 must not clear 2004");
        // And clustering is genuinely off: e + combining acute splits.
        t.feed("e\u{0301}");
        assert!(t.row(0)[0].cluster.is_none(), "2027 really did toggle off");
        assert_eq!(t.cursor().col, 2);

        // Conversely, disabling bracketed paste must not re-enable clustering.
        let mut t2 = Terminal::new(10, 1, 100);
        t2.feed("\x1b[?2027l"); // clustering off
        t2.feed("\x1b[?2004l"); // toggle the neighbor
        t2.feed("e\u{0301}");
        assert!(t2.row(0)[0].cluster.is_none(), "toggling 2004 must not touch 2027");
        assert_eq!(t2.cursor().col, 2);
    }

    #[test]
    fn mode_2027_disable_is_not_retroactive() {
        // Disabling clustering mid-line affects only subsequent input: a cluster
        // already laid down with clustering on stays fused; the next one splits.
        let mut t = Terminal::new(10, 2, 100);
        t.feed("e\u{0301}"); // default: clustering on → one cell
        assert_eq!(grapheme_at(&t, 0, 0), "e\u{0301}", "first cluster fused");
        assert_eq!(t.cursor().col, 1);

        t.feed("\r\n"); // fresh line so the second base is unambiguous
        t.feed("\x1b[?2027l"); // now disable clustering
        t.feed("e\u{0301}"); // → two cells
        assert!(t.row(1)[0].cluster.is_none(), "second base not clustered");
        assert_eq!(t.row(1)[0].ch, 'e');
        assert_eq!(t.row(1)[1].ch, '\u{0301}', "combining mark spills to its own cell");
        assert_eq!(t.cursor().col, 2);

        // The earlier cluster is untouched — disabling is not retroactive.
        assert_eq!(grapheme_at(&t, 0, 0), "e\u{0301}");
    }

    #[test]
    fn scroll_when_lf_at_bottom() {
        let mut t = Terminal::new(5, 2, 100);
        t.feed("AAA\r\nBBB\r\nCCC");
        // Row 2 doesn't exist; the "AAA" line scrolled off, now row 0=BBB, row 1=CCC
        assert_eq!(t.row(0).iter().map(|c| c.ch).collect::<String>(), "BBB  ");
        assert_eq!(t.row(1).iter().map(|c| c.ch).collect::<String>(), "CCC  ");
        assert_eq!(t.scrollback_len(), 1);
    }

    #[test]
    fn erase_display_mode_3_clears_scrollback_only() {
        let mut t = Terminal::new(5, 2, 100);
        t.feed("AAAAA\r\nBBBBB\r\nCCCCC\r\nDDDDD");
        assert_eq!(t.scrollback_len(), 2);
        t.feed("\x1b[3J");
        assert_eq!(t.scrollback_len(), 0);
        // Visible grid is untouched.
        assert_eq!(t.row(0).iter().map(|c| c.ch).collect::<String>(), "CCCCC");
        assert_eq!(t.row(1).iter().map(|c| c.ch).collect::<String>(), "DDDDD");
    }

    #[test]
    fn erase_display_mode_0() {
        let mut t = Terminal::new(5, 3, 100);
        t.feed("ABCDE\r\nFGHIJ\r\nKLMNO");
        t.feed("\x1b[2;3H"); // row 2, col 3 (0-based: 1, 2)
        t.feed("\x1b[0J"); // erase cursor to end of screen
        assert_eq!(t.row(0).iter().map(|c| c.ch).collect::<String>(), "ABCDE");
        assert_eq!(t.row(1).iter().map(|c| c.ch).collect::<String>(), "FG   ");
        assert_eq!(t.row(2).iter().map(|c| c.ch).collect::<String>(), "     ");
    }

    #[test]
    fn erase_line_mode_0_from_cursor() {
        let mut t = Terminal::new(5, 1, 100);
        t.feed("ABCDE");
        t.feed("\x1b[1;3H"); // col 3 (0-based: 2)
        t.feed("\x1b[K");
        assert_eq!(t.row(0).iter().map(|c| c.ch).collect::<String>(), "AB   ");
    }

    #[test]
    fn sgr_styles_cells() {
        let mut t = Terminal::new(5, 1, 100);
        t.feed("\x1b[31mA\x1b[0mB");
        assert_eq!(t.row(0)[0].style.fg, crate::style::CellColor::Indexed(1));
        assert_eq!(t.row(0)[1].style.fg, crate::style::CellColor::Default);
    }

    #[test]
    fn alt_screen_preserves_primary() {
        let mut t = Terminal::new(5, 2, 100);
        t.feed("ABC");
        t.feed("\x1b[?1049h"); // enter alt screen, save cursor, clear alt
        t.feed("XYZ");
        assert_eq!(t.row(0).iter().map(|c| c.ch).collect::<String>(), "XYZ  ");
        t.feed("\x1b[?1049l"); // exit alt, restore cursor
        assert_eq!(t.row(0).iter().map(|c| c.ch).collect::<String>(), "ABC  ");
        assert_eq!(t.cursor().col, 3);
    }

    #[test]
    fn save_and_restore_cursor() {
        let mut t = Terminal::new(5, 3, 100);
        t.feed("\x1b[2;3H");
        t.feed("\x1b7"); // save
        t.feed("\x1b[1;1H");
        t.feed("\x1b8"); // restore
        assert_eq!(t.cursor().row, 1);
        assert_eq!(t.cursor().col, 2);
    }

    #[test]
    fn decslrm_ignored_when_lrmm_off() {
        // CSI 1;5s with DECLRMM disabled is silently dropped — margins stay
        // full-width.
        let mut t = Terminal::new(10, 5, 100);
        t.feed("\x1b[1;5s");
        // Prove margins are still full: fill a row, scroll the region down,
        // and confirm the rightmost cols moved (they wouldn't if clipped).
        t.feed("\x1b[1;1H");
        for c in 0..10 {
            t.feed(&format!("{}", (b'a' + c) as char));
        }
        // SU 1 row — without LRM, the whole row clears.
        t.feed("\x1b[S");
        assert_eq!(t.row(0).iter().map(|c| c.ch).collect::<String>(), "          ");
    }

    #[test]
    fn csi_s_no_params_is_save_cursor_when_lrmm_off() {
        // With DECLRMM off, bare `ESC[s` is SCOSC: save the cursor in the same
        // slot DECSC (ESC 7) uses, so ESC 8 restores it.
        let mut t = Terminal::new(10, 5, 100);
        t.feed("\x1b[3;5H\x1b[s\x1b[1;1H\x1b8");
        assert_eq!(t.cursor().row, 2);
        assert_eq!(t.cursor().col, 4);
    }

    #[test]
    fn decslrm_clips_scroll_up_to_margins() {
        // Reproduce tmux's per-pane scroll: enable DECLRMM, set LRM to the
        // "left pane" columns, scroll up — cells outside the LRM (the
        // divider + right pane) must NOT move.
        let mut t = Terminal::new(10, 5, 100);
        // Fill row 0 with "abcdefghij" (cols 0..9).
        t.feed("abcdefghij");
        // Mark a divider in col 6 on rows 1..4 by direct CUP+Print.
        for r in 2..=5 {
            t.feed(&format!("\x1b[{};7H|", r));
        }
        // Enable DECLRMM, set left=1, right=6 (1-based: cols 0..5).
        t.feed("\x1b[?69h\x1b[1;6s");
        // Set scroll region to rows 1..5 (1-based), put cursor inside.
        t.feed("\x1b[1;5r\x1b[1;1H");
        // SU 5 — should blank cols 0..5 of all 5 rows but leave col 6+
        // untouched (the divider remains).
        t.feed("\x1b[5S");
        for r in 1..=4 {
            assert_eq!(t.row(r)[6].ch, '|', "row {r}: divider should survive LRM scroll");
        }
        // And cols 0..5 of row 0 (with our 'abcdef') should now be blank
        // since SU consumed them.
        for c in 0..6 {
            assert_eq!(t.row(0)[c].ch, ' ', "col {c} should be blank after SU");
        }
        // Col 6 of row 0 stays 'g' — outside LRM, untouched.
        assert_eq!(t.row(0)[6].ch, 'g');
    }

    #[test]
    fn decslrm_clips_linefeed_scroll_to_margins() {
        // Same setup but the scroll is triggered by `\n` at scroll_bottom
        // (the path tmux actually uses while painting per-pane content).
        let mut t = Terminal::new(8, 4, 100);
        // Put 'X' in col 5 across every row as a "divider".
        for r in 1..=4 {
            t.feed(&format!("\x1b[{};6HX", r));
        }
        t.feed("\x1b[?69h\x1b[1;5s"); // LRM cols 1..5 (0..4)
        // Position cursor inside LRM at scroll_bottom, then LF — should
        // scroll within LRM only.
        t.feed("\x1b[4;1H\n");
        for r in 0..4 {
            assert_eq!(t.row(r)[5].ch, 'X', "row {r}: divider must survive LF scroll");
        }
    }

    #[test]
    fn decslrm_clips_insert_and_delete_char() {
        let mut t = Terminal::new(10, 1, 100);
        t.feed("abcdefghij");
        t.feed("\x1b[?69h\x1b[1;5s"); // LRM cols 1..5 (0..4)
        // ICH 2 at col 0 — cells shift within LRM only; cols 5..9 untouched.
        t.feed("\x1b[1;1H\x1b[2@");
        let s: String = t.row(0).iter().map(|c| c.ch).collect();
        assert_eq!(s, "  abcfghij");
        // DCH 2 at col 0 — pulls cells from within LRM only.
        t.feed("\x1b[2P");
        let s: String = t.row(0).iter().map(|c| c.ch).collect();
        assert_eq!(s, "abc  fghij");
    }

    #[test]
    fn decslrm_clips_erase_char_to_right_margin() {
        let mut t = Terminal::new(10, 1, 100);
        t.feed("abcdefghij");
        t.feed("\x1b[?69h\x1b[1;5s");
        t.feed("\x1b[1;3H\x1b[10X"); // ECH 10 at col 3 — clip to col 5
        let s: String = t.row(0).iter().map(|c| c.ch).collect();
        assert_eq!(s, "ab   fghij");
    }

    #[test]
    fn decslrm_69l_disables_and_resets() {
        let mut t = Terminal::new(10, 1, 100);
        t.feed("abcdefghij");
        t.feed("\x1b[?69h\x1b[1;5s"); // LRM on, cols 1..5
        t.feed("\x1b[?69l"); // disable LRMM → margins reset, future DECSLRM ignored
        // Now SU should affect the full width again.
        t.feed("\x1b[1S");
        let s: String = t.row(0).iter().map(|c| c.ch).collect();
        assert_eq!(s, "          ");
    }

    #[test]
    fn full_region_scrollback_requires_full_lrm() {
        // With a narrowed LRM the "full region" rule that pushes lines to
        // scrollback shouldn't fire — scrollback should stay empty.
        let mut t = Terminal::new(10, 3, 100);
        t.feed("row1\n\rrow2\n\rrow3");
        t.feed("\x1b[?69h\x1b[1;5s"); // LRM cols 1..5
        let prior = t.scrollback_len();
        // Force a scroll inside the LRM via SU on the full scroll region.
        t.feed("\x1b[3;1H\x1b[3S");
        assert_eq!(t.scrollback_len(), prior, "narrow LRM must not push to scrollback");
    }

    #[test]
    fn autowrap_inside_lrm_wraps_at_right_margin() {
        // 10 cols, 4 rows. LRM cols 1..5 (0..4). Print 7 chars starting at
        // col 0 row 0: chars 1-5 land in row 0 cols 0..4, char 6 wraps to
        // row 1 col 0 (the LEFT margin), char 7 lands at row 1 col 1.
        // Crucially: row 0 cols 5..9 stay blank — without LRM-aware wrap,
        // the run would have spilled into the neighbor pane.
        let mut t = Terminal::new(10, 4, 100);
        t.feed("\x1b[?69h\x1b[1;5s\x1b[1;1H"); // LRM, cursor to (0,0)
        t.feed("abcdefg");
        let r0: String = t.row(0).iter().map(|c| c.ch).collect();
        let r1: String = t.row(1).iter().map(|c| c.ch).collect();
        assert_eq!(r0, "abcde     ", "row 0 must not bleed past right margin");
        assert_eq!(r1, "fg        ", "wrap target column must be left margin");
    }

    #[test]
    fn autowrap_outside_lrm_uses_screen_edge() {
        // xterm-compat: when the cursor is sitting OUTSIDE the LRM (because
        // CUP placed it there — CUP isn't clipped), the wrap should fall
        // back to the screen edge rather than snapping to the left margin.
        let mut t = Terminal::new(10, 4, 100);
        t.feed("\x1b[?69h\x1b[1;5s"); // LRM cols 1..5
        t.feed("\x1b[1;7H"); // CUP to (0, 6) — outside LRM (col 6 > right=4)
        t.feed("xyzw");      // prints at cols 6,7,8,9 → wrap_pending
        t.feed("Q");         // wraps to (1, 0), prints 'Q'
        let r0: String = t.row(0).iter().map(|c| c.ch).collect();
        let r1: String = t.row(1).iter().map(|c| c.ch).collect();
        assert_eq!(r0, "      xyzw");
        assert_eq!(&r1[..2], "Q ", "outside-LRM wrap targets screen col 0, not left margin");
    }

    #[test]
    fn autowrap_disabled_sticks_at_right_margin() {
        // With DECAWM off, printing past the right margin should overwrite
        // the rightmost cell in place — same shape as the existing screen-
        // edge behavior, but pinned to the LRM right edge.
        let mut t = Terminal::new(10, 2, 100);
        t.feed("\x1b[?69h\x1b[1;5s\x1b[?7l\x1b[1;1H"); // LRM + DECAWM off
        t.feed("abcdefg");
        let r0: String = t.row(0).iter().map(|c| c.ch).collect();
        // Cols 0..3 keep their original chars; col 4 (right margin) ends up
        // holding the last char printed; rest of the row stays blank.
        assert_eq!(&r0[..4], "abcd");
        assert_eq!(r0.chars().nth(4), Some('g'));
        assert_eq!(&r0[5..], "     ");
    }

    #[test]
    fn decslrm_invalid_range_resets_to_full_width() {
        // VT spec: a DECSLRM request with left >= right (or out-of-bounds
        // right) must reset margins to the full screen rather than leaving
        // a previously-narrowed range in place. Apps rely on this to "clear"
        // margins by sending e.g. `CSI 1;1s`.
        let mut t = Terminal::new(10, 1, 100);
        t.feed("abcdefghij");
        // Establish a narrow LRM first so we can prove the next request
        // *resets* rather than no-ops.
        t.feed("\x1b[?69h\x1b[1;5s");
        // Now an invalid range: left == right (both 1-based '3'), which is
        // l >= r in the parsed form. Per spec, this should reset to full.
        t.feed("\x1b[3;3s");
        // SU 1 — if margins were reset, the entire row clears; if the old
        // narrow LRM survived, cols 5..9 would remain "fghij".
        t.feed("\x1b[1S");
        let s: String = t.row(0).iter().map(|c| c.ch).collect();
        assert_eq!(s, "          ", "invalid DECSLRM must reset margins to full width");
    }

    #[test]
    fn decslrm_right_beyond_cols_resets_to_full_width() {
        // Similar to above but tests the other invalid form: right > cols.
        // tmux occasionally emits `CSI 1;<huge>s` when computing margins
        // against a stale geometry; we must treat that as "reset to full".
        let mut t = Terminal::new(10, 1, 100);
        t.feed("abcdefghij");
        t.feed("\x1b[?69h\x1b[1;5s"); // narrow first
        t.feed("\x1b[1;99s"); // right > cols → reset
        t.feed("\x1b[1S");
        let s: String = t.row(0).iter().map(|c| c.ch).collect();
        assert_eq!(s, "          ");
    }

    #[test]
    fn decslrm_homes_the_cursor() {
        // Per VT spec, DECSLRM (like DECSTBM) homes the cursor to (0,0)
        // after setting margins. Apps depend on this when bootstrapping a
        // per-pane drawing context — they don't issue a separate CUP.
        let mut t = Terminal::new(10, 5, 100);
        t.feed("\x1b[3;7H"); // park cursor mid-screen
        assert_eq!(t.cursor().row, 2);
        assert_eq!(t.cursor().col, 6);
        t.feed("\x1b[?69h\x1b[2;6s"); // DECSLRM
        assert_eq!(t.cursor().row, 0, "DECSLRM should home cursor row");
        assert_eq!(t.cursor().col, 0, "DECSLRM should home cursor col");
    }

    #[test]
    fn cup_is_not_clipped_to_lrm() {
        // xterm behavior: DECSLRM only constrains scroll-style *operations*;
        // CUP/HVP can still address any cell on the screen. tmux relies on
        // this — it sets a left-pane LRM, then CUPs to the right pane to
        // draw the divider/right-pane content.
        let mut t = Terminal::new(10, 3, 100);
        t.feed("\x1b[?69h\x1b[1;5s"); // LRM cols 0..4 (narrow)
        // CUP to col 8 (1-based 9), well outside the LRM range.
        t.feed("\x1b[1;9HZ");
        assert_eq!(t.row(0)[8].ch, 'Z', "CUP must place cell outside LRM");
        // And the cursor itself should sit there, not get clamped to col 4.
        // After printing 'Z' at col 8 the cursor advances to col 9.
        assert_eq!(t.cursor().col, 9);
    }

    #[test]
    fn decslrm_clips_scroll_down_to_margins() {
        // Symmetric coverage to decslrm_clips_scroll_up_to_margins: SD (CSI T)
        // must also respect LRM so reverse-scroll inside a pane doesn't
        // disturb cells in neighboring panes.
        let mut t = Terminal::new(10, 5, 100);
        // Fill row 4 with content inside the LRM and a divider in col 6.
        t.feed("\x1b[5;1Habcdef|hij");
        // Mark the divider on the other rows too so we can detect leakage.
        for r in 1..=4 {
            t.feed(&format!("\x1b[{};7H|", r));
        }
        t.feed("\x1b[?69h\x1b[1;6s"); // LRM cols 0..5
        t.feed("\x1b[1;5r\x1b[1;1H"); // scroll region rows 0..4
        t.feed("\x1b[5T"); // SD 5 — should blank cols 0..5 within region
        for r in 0..=4 {
            assert_eq!(t.row(r)[6].ch, '|', "row {r}: divider must survive SD");
        }
        // Cols 0..5 of every row should now be blank (SD pushed content out).
        for c in 0..6 {
            assert_eq!(t.row(4)[c].ch, ' ', "row 4 col {c} should be blank after SD");
        }
    }

    #[test]
    fn insert_line_noop_when_cursor_outside_lrm_columns() {
        // IL/DL are documented as no-ops when the cursor is outside the
        // scroll region. With DECLRMM that "scroll region" is 2D — the
        // cursor must be inside the column range too. This matters because
        // an app drawing into the right pane should not accidentally
        // shift rows in the left pane just because it issued IL with the
        // cursor parked over there.
        let mut t = Terminal::new(10, 3, 100);
        t.feed("row1xxxxxx\n\rrow2xxxxxx\n\rrow3xxxxxx");
        t.feed("\x1b[?69h\x1b[1;5s"); // LRM cols 0..4
        // Park cursor at col 7 (outside LRM), then IL 1.
        t.feed("\x1b[1;8H\x1b[L");
        // Row 0 should still read "row1xxxxx" — IL was a no-op.
        let s: String = t.row(0).iter().map(|c| c.ch).collect();
        assert_eq!(s, "row1xxxxxx", "IL must be a no-op when cursor is outside LRM cols");
    }

    #[test]
    fn delete_line_noop_when_cursor_outside_lrm_columns() {
        // Mirror of the IL test above for DL.
        let mut t = Terminal::new(10, 3, 100);
        t.feed("row1xxxxxx\n\rrow2xxxxxx\n\rrow3xxxxxx");
        t.feed("\x1b[?69h\x1b[1;5s"); // LRM cols 0..4
        t.feed("\x1b[1;8H\x1b[M"); // DL 1 with cursor outside LRM
        let s: String = t.row(0).iter().map(|c| c.ch).collect();
        assert_eq!(s, "row1xxxxxx", "DL must be a no-op when cursor is outside LRM cols");
    }

    #[test]
    fn full_reset_clears_lrmm_state() {
        // RIS (`ESC c`) must restore DECLRMM to disabled and margins to
        // full width. Without this, an app that crashes mid-session and
        // issues RIS would still see a narrowed scroll region — a classic
        // "terminal stuck" symptom.
        let mut t = Terminal::new(10, 1, 100);
        t.feed("\x1b[?69h\x1b[1;5s"); // narrow LRM
        t.feed("\x1bc"); // RIS
        // Now reprint and SU — should clear the whole row, proving LRM
        // reset to full width AND DECLRMM is disabled (so a subsequent
        // bare `CSI s` is SCOSC again, not reset-margins).
        t.feed("abcdefghij\x1b[1S");
        let s: String = t.row(0).iter().map(|c| c.ch).collect();
        assert_eq!(s, "          ", "RIS must reset LRM to full width");
        // Verify DECLRMM was disabled too: bare CSI s must now be SCOSC.
        // Park cursor, save via `CSI s`, move, restore — should land back.
        t.feed("\x1b[1;5H\x1b[s\x1b[1;1H\x1b8");
        assert_eq!(t.cursor().col, 4, "after RIS, bare CSI s should be SCOSC");
    }

    #[test]
    fn resize_clears_lrmm_state() {
        // Resize must drop LRMM — the old margins are tied to the old
        // column count and would be nonsensical (or out of bounds) after
        // a resize. The renderer assumes scroll_right < cols.
        let mut t = Terminal::new(10, 3, 100);
        t.feed("\x1b[?69h\x1b[1;5s"); // narrow LRM on the 10-col grid
        t.resize(20, 3); // grow to 20 cols
        // Fill row 0 across the new width then SU — if LRMM survived, the
        // old narrow margin would leak through and cols 5..19 wouldn't
        // clear. We expect them to clear (LRMM disabled, full width).
        t.feed("\x1b[1;1H");
        for c in 0..20 {
            t.feed(&format!("{}", (b'a' + (c % 26) as u8) as char));
        }
        t.feed("\x1b[1S");
        let s: String = t.row(0).iter().map(|c| c.ch).collect();
        assert_eq!(s, "                    ", "resize must clear LRMM state");
    }

    #[test]
    fn cup_and_print_column_of_chars() {
        // Reproduce the way tmux paints a vertical pane border: for each row,
        // CUP to (row, divider_col) then Print('│'). Whole column must be
        // filled, including rows where nothing else was written.
        let mut t = Terminal::new(7, 5, 100);
        for r in 1..=5 {
            t.feed(&format!("\x1b[{};4H│", r));
        }
        for r in 0..5 {
            assert_eq!(t.row(r)[3].ch, '│', "row {r}: divider missing");
        }
    }

    #[test]
    fn alt_screen_cup_column_paint_survives() {
        // Same as above but inside alt-screen (where tmux actually runs).
        let mut t = Terminal::new(7, 5, 100);
        t.feed("\x1b[?1049h");
        for r in 1..=5 {
            t.feed(&format!("\x1b[{};4H│", r));
        }
        for r in 0..5 {
            assert_eq!(t.row(r)[3].ch, '│', "alt-screen row {r}: divider missing");
        }
    }

    #[test]
    fn backspace_moves_left_without_erasing() {
        let mut t = Terminal::new(5, 1, 100);
        t.feed("ABC\x08");
        assert_eq!(t.cursor().col, 2);
        assert_eq!(t.row(0)[2].ch, 'C'); // not erased
    }

    #[test]
    fn tab_advances_to_next_stop() {
        let mut t = Terminal::new(20, 1, 100);
        t.feed("A\t");
        assert_eq!(t.cursor().col, 8);
        t.feed("B\t");
        assert_eq!(t.cursor().col, 16);
    }

    #[test]
    fn cursor_visible_toggle() {
        let mut t = Terminal::new(5, 1, 100);
        assert!(t.cursor_visible());
        t.feed("\x1b[?25l");
        assert!(!t.cursor_visible());
        t.feed("\x1b[?25h");
        assert!(t.cursor_visible());
    }

    #[test]
    fn set_scroll_region_then_lf_scrolls_only_region() {
        let mut t = Terminal::new(5, 5, 100);
        t.feed("A\r\nB\r\nC\r\nD\r\nE");
        t.feed("\x1b[2;4r"); // region rows 2..=4 (0-based 1..=3); homes cursor
        assert_eq!(t.cursor().row, 0);
        t.feed("\x1b[4;1H"); // cursor at row 4 (0-based 3, last in region)
        t.feed("\n"); // LF at scroll_bottom → scroll region up by 1
        assert_eq!(t.row(0).iter().map(|c| c.ch).collect::<String>(), "A    ");
        // inside region: B (was at row 1) rolled off; row 1 now holds C.
        assert_eq!(t.row(1).iter().map(|c| c.ch).collect::<String>(), "C    ");
        assert_eq!(t.row(2).iter().map(|c| c.ch).collect::<String>(), "D    ");
        // bottom of region was cleared
        assert_eq!(t.row(3).iter().map(|c| c.ch).collect::<String>(), "     ");
        // outside region — untouched
        assert_eq!(t.row(4).iter().map(|c| c.ch).collect::<String>(), "E    ");
    }

    #[test]
    fn insert_line_pushes_rows_down_within_region() {
        let mut t = Terminal::new(3, 4, 100);
        t.feed("AAA\r\nBBB\r\nCCC\r\nDDD");
        t.feed("\x1b[2;1H"); // cursor row 1 (0-based)
        t.feed("\x1b[L"); // IL 1
        assert_eq!(t.row(0).iter().map(|c| c.ch).collect::<String>(), "AAA");
        assert_eq!(t.row(1).iter().map(|c| c.ch).collect::<String>(), "   ");
        assert_eq!(t.row(2).iter().map(|c| c.ch).collect::<String>(), "BBB");
        assert_eq!(t.row(3).iter().map(|c| c.ch).collect::<String>(), "CCC");
    }

    #[test]
    fn insert_line_respects_scroll_region() {
        let mut t = Terminal::new(3, 4, 100);
        t.feed("AAA\r\nBBB\r\nCCC\r\nDDD");
        t.feed("\x1b[2;3r"); // region rows 2..=3
        t.feed("\x1b[2;1H"); // cursor at row 1 (top of region)
        t.feed("\x1b[L");
        // outside region untouched
        assert_eq!(t.row(0).iter().map(|c| c.ch).collect::<String>(), "AAA");
        // inside region: blank inserted at top, BBB shifts down, CCC pushed off
        assert_eq!(t.row(1).iter().map(|c| c.ch).collect::<String>(), "   ");
        assert_eq!(t.row(2).iter().map(|c| c.ch).collect::<String>(), "BBB");
        assert_eq!(t.row(3).iter().map(|c| c.ch).collect::<String>(), "DDD");
    }

    #[test]
    fn insert_line_outside_region_is_noop() {
        let mut t = Terminal::new(3, 4, 100);
        t.feed("AAA\r\nBBB\r\nCCC\r\nDDD");
        t.feed("\x1b[2;3r"); // region rows 2..=3 (0-based 1..=2)
        t.feed("\x1b[1;1H"); // cursor at row 0 (outside region)
        t.feed("\x1b[L");
        assert_eq!(t.row(0).iter().map(|c| c.ch).collect::<String>(), "AAA");
        assert_eq!(t.row(1).iter().map(|c| c.ch).collect::<String>(), "BBB");
        assert_eq!(t.row(2).iter().map(|c| c.ch).collect::<String>(), "CCC");
    }

    #[test]
    fn delete_line_shifts_rows_up() {
        let mut t = Terminal::new(3, 4, 100);
        t.feed("AAA\r\nBBB\r\nCCC\r\nDDD");
        t.feed("\x1b[2;1H");
        t.feed("\x1b[M"); // DL 1
        assert_eq!(t.row(0).iter().map(|c| c.ch).collect::<String>(), "AAA");
        assert_eq!(t.row(1).iter().map(|c| c.ch).collect::<String>(), "CCC");
        assert_eq!(t.row(2).iter().map(|c| c.ch).collect::<String>(), "DDD");
        assert_eq!(t.row(3).iter().map(|c| c.ch).collect::<String>(), "   ");
    }

    #[test]
    fn delete_line_does_not_push_to_scrollback() {
        let mut t = Terminal::new(3, 4, 100);
        t.feed("AAA\r\nBBB\r\nCCC\r\nDDD");
        t.feed("\x1b[1;1H");
        t.feed("\x1b[M");
        assert_eq!(t.scrollback_len(), 0);
    }

    #[test]
    fn insert_char_shifts_within_row() {
        let mut t = Terminal::new(6, 1, 100);
        t.feed("ABCDEF");
        t.feed("\x1b[1;3H"); // col 3 (0-based 2)
        t.feed("\x1b[2@");
        // CD shifted right by 2; right edge dropped.
        assert_eq!(t.row(0).iter().map(|c| c.ch).collect::<String>(), "AB  CD");
    }

    #[test]
    fn insert_char_clamps_to_eol() {
        let mut t = Terminal::new(5, 1, 100);
        t.feed("ABCDE");
        t.feed("\x1b[1;3H");
        t.feed("\x1b[99@"); // clamps to 3 cols
        assert_eq!(t.row(0).iter().map(|c| c.ch).collect::<String>(), "AB   ");
    }

    #[test]
    fn delete_char_shifts_within_row() {
        let mut t = Terminal::new(6, 1, 100);
        t.feed("ABCDEF");
        t.feed("\x1b[1;3H"); // col 3
        t.feed("\x1b[2P");
        // CD removed; EF slides left; right edge filled.
        assert_eq!(t.row(0).iter().map(|c| c.ch).collect::<String>(), "ABEF  ");
    }

    #[test]
    fn erase_char_replaces_in_place() {
        let mut t = Terminal::new(6, 1, 100);
        t.feed("ABCDEF");
        t.feed("\x1b[1;3H");
        t.feed("\x1b[3X");
        assert_eq!(t.row(0).iter().map(|c| c.ch).collect::<String>(), "AB   F");
        // cursor unchanged
        assert_eq!(t.cursor().col, 2);
    }

    #[test]
    fn dsr_5_replies_ok() {
        let mut t = Terminal::new(5, 3, 100);
        t.feed("\x1b[5n");
        assert_eq!(t.take_response(), b"\x1b[0n".to_vec());
        // Drained — second call is empty.
        assert!(t.take_response().is_empty());
    }

    #[test]
    fn dsr_6_reports_cursor_position_1_based() {
        let mut t = Terminal::new(10, 5, 100);
        t.feed("\x1b[3;7H"); // row 3, col 7 (1-based)
        t.feed("\x1b[6n");
        assert_eq!(t.take_response(), b"\x1b[3;7R".to_vec());
    }

    #[test]
    fn dsr_unknown_code_no_reply() {
        let mut t = Terminal::new(5, 3, 100);
        t.feed("\x1b[99n");
        assert!(t.take_response().is_empty());
    }

    // ---- DEC mode 2031: color-scheme update notifications ----

    #[test]
    fn mode_2031_query_996_reports_dark_for_dark_background() {
        // A fresh terminal defaults to a black background → dark preference.
        let mut t = Terminal::new(5, 3, 100);
        t.feed("\x1b[?996n");
        assert_eq!(t.take_response(), b"\x1b[?997;1n".to_vec());
    }

    #[test]
    fn mode_2031_query_996_reports_light_for_light_background() {
        let mut t = Terminal::new(5, 3, 100);
        t.set_default_colors([0x00, 0x00, 0x00], [0xff, 0xff, 0xff], [0x00, 0x00, 0x00]);
        t.feed("\x1b[?996n");
        assert_eq!(t.take_response(), b"\x1b[?997;2n".to_vec());
    }

    #[test]
    fn mode_2031_query_996_answered_even_when_unsubscribed() {
        // The state query is a direct request — it must reply regardless of
        // whether the app has enabled the 2031 change notifications.
        let mut t = Terminal::new(5, 3, 100);
        t.feed("\x1b[?996n");
        assert_eq!(t.take_response(), b"\x1b[?997;1n".to_vec());
    }

    #[test]
    fn notify_color_scheme_change_silent_when_unsubscribed() {
        let mut t = Terminal::new(5, 3, 100);
        // No `CSI ? 2031 h`, so the front end's post-theme hook is a no-op even
        // across a real light/dark flip.
        t.set_default_colors([0xcc; 3], [0xff, 0xff, 0xff], [0xcc; 3]);
        assert!(!t.notify_color_scheme_change());
        assert!(t.take_response().is_empty());
    }

    #[test]
    fn notify_color_scheme_change_emits_on_polarity_flip() {
        let mut t = Terminal::new(5, 3, 100);
        // Subscribe while dark (default black bg) → baseline polarity = dark.
        t.feed("\x1b[?2031h");
        // Flip the background to light, then run the front end's hook.
        t.set_default_colors([0x00; 3], [0xff, 0xff, 0xff], [0x00; 3]);
        assert!(t.notify_color_scheme_change());
        assert_eq!(t.take_response(), b"\x1b[?997;2n".to_vec());
    }

    #[test]
    fn notify_color_scheme_change_silent_without_polarity_flip() {
        let mut t = Terminal::new(5, 3, 100);
        t.feed("\x1b[?2031h"); // baseline dark
        // Same-polarity swap (black → a darker grey) must not notify.
        t.set_default_colors([0xcc; 3], [0x11, 0x11, 0x11], [0xcc; 3]);
        assert!(!t.notify_color_scheme_change());
        assert!(t.take_response().is_empty());
    }

    #[test]
    fn notify_color_scheme_change_debounces_repeat_polarity() {
        let mut t = Terminal::new(5, 3, 100);
        t.feed("\x1b[?2031h"); // baseline dark
        // First flip to light notifies.
        t.set_default_colors([0x00; 3], [0xff; 3], [0x00; 3]);
        assert!(t.notify_color_scheme_change());
        assert_eq!(t.take_response(), b"\x1b[?997;2n".to_vec());
        // Re-running the hook still in light is silent.
        t.set_default_colors([0x00; 3], [0xfe; 3], [0x00; 3]);
        assert!(!t.notify_color_scheme_change());
        assert!(t.take_response().is_empty());
        // Flipping back to dark notifies again.
        t.set_default_colors([0x00; 3], [0x00; 3], [0x00; 3]);
        assert!(t.notify_color_scheme_change());
        assert_eq!(t.take_response(), b"\x1b[?997;1n".to_vec());
    }

    #[test]
    fn mode_2031_reset_stops_notifications() {
        let mut t = Terminal::new(5, 3, 100);
        t.feed("\x1b[?2031h");
        t.feed("\x1b[?2031l"); // unsubscribe before any flip
        t.set_default_colors([0x00; 3], [0xff; 3], [0x00; 3]); // flip to light
        assert!(!t.notify_color_scheme_change());
        assert!(t.take_response().is_empty());
    }

    #[test]
    fn mode_2031_luminance_threshold_boundary() {
        // bg_is_dark uses Rec.709 luma >> 8 with a `< 128` cutoff. For a neutral
        // grey [v;3] the luma reduces exactly to v, so v=127 is the last dark
        // value and v=128 the first light one — pin both sides of the cutoff so
        // an off-by-one in the threshold (e.g. `<=` vs `<`) is caught.
        let mut t = Terminal::new(5, 3, 100);
        t.set_default_colors([0x00; 3], [127, 127, 127], [0x00; 3]);
        t.feed("\x1b[?996n");
        assert_eq!(t.take_response(), b"\x1b[?997;1n".to_vec(), "luma 127 is dark");

        t.set_default_colors([0x00; 3], [128, 128, 128], [0x00; 3]);
        t.feed("\x1b[?996n");
        assert_eq!(t.take_response(), b"\x1b[?997;2n".to_vec(), "luma 128 is light");
    }

    #[test]
    fn mode_2031_query_996_does_not_seed_notification_baseline() {
        // The 996 state query must be a pure read: it answers without touching
        // `last_notified_dark`. So after subscribing (baseline = dark) and then
        // querying, a subsequent real flip to light must still notify exactly
        // once — the query must not have moved the baseline.
        let mut t = Terminal::new(5, 3, 100);
        t.feed("\x1b[?2031h"); // baseline dark (default black bg)
        t.feed("\x1b[?996n"); // a query while still dark
        assert_eq!(t.take_response(), b"\x1b[?997;1n".to_vec());
        // Now flip to light: the notification must fire despite the prior query.
        t.set_default_colors([0x00; 3], [0xff; 3], [0x00; 3]);
        assert!(t.notify_color_scheme_change());
        assert_eq!(t.take_response(), b"\x1b[?997;2n".to_vec());
    }

    #[test]
    fn mode_2031_reenable_reseeds_baseline_to_current_background() {
        // Re-enabling 2031 re-seeds the baseline to the *current* background.
        // Subscribe while dark, flip to light without notifying (no hook call),
        // then re-enable: the baseline becomes light, so the front-end hook is
        // silent even though the polarity differs from the original enable.
        let mut t = Terminal::new(5, 3, 100);
        t.feed("\x1b[?2031h"); // baseline dark
        t.set_default_colors([0x00; 3], [0xff; 3], [0x00; 3]); // now light
        t.feed("\x1b[?2031h"); // re-enable → baseline reseeded to light
        assert!(!t.notify_color_scheme_change());
        assert!(t.take_response().is_empty());
    }

    #[test]
    fn private_dsr_unknown_code_is_silent() {
        // Only 996 is implemented for the private DSR. An unknown private DSR
        // such as `CSI ? 5 n` must produce no reply — answering would confuse a
        // host that never asked the color-scheme question.
        let mut t = Terminal::new(5, 3, 100);
        t.feed("\x1b[?5n");
        assert!(t.take_response().is_empty());
    }

    #[test]
    fn non_private_996_dsr_is_not_color_scheme_query() {
        // The color-scheme query is the *private* form (`CSI ? 996 n`). The
        // non-private `CSI 996 n` routes through the ordinary DSR handler, which
        // only knows codes 5/6, so it must stay silent — the `?` is load-bearing.
        let mut t = Terminal::new(5, 3, 100);
        t.feed("\x1b[996n");
        assert!(t.take_response().is_empty());
    }

    #[test]
    fn deccm_set_and_reset_toggles_app_cursor_keys() {
        let mut t = Terminal::new(5, 3, 100);
        assert!(!t.app_cursor_keys());
        t.feed("\x1b[?1h");
        assert!(t.app_cursor_keys());
        t.feed("\x1b[?1l");
        assert!(!t.app_cursor_keys());
    }

    #[test]
    fn primary_da_replies_with_vt100() {
        let mut t = Terminal::new(5, 3, 100);
        t.feed("\x1b[c");
        assert_eq!(t.take_response(), b"\x1b[?1;2c".to_vec());
    }

    #[test]
    fn secondary_da_replies_with_vt220() {
        let mut t = Terminal::new(5, 3, 100);
        t.feed("\x1b[>c");
        assert_eq!(t.take_response(), b"\x1b[>0;276;0c".to_vec());
    }

    #[test]
    fn cursor_blink_follows_decscusr() {
        let mut t = Terminal::new(5, 3, 100);
        assert!(t.cursor_blink()); // default 0 — blink block
        t.feed("\x1b[2 q");
        assert!(!t.cursor_blink()); // steady block
        t.feed("\x1b[3 q");
        assert!(t.cursor_blink()); // blink underline
        t.feed("\x1b[6 q");
        assert!(!t.cursor_blink()); // steady bar
    }

    #[test]
    fn decscusr_sets_cursor_shape() {
        let mut t = Terminal::new(5, 3, 100);
        assert_eq!(t.cursor_shape(), CursorShape::Block);
        t.feed("\x1b[3 q");
        assert_eq!(t.cursor_shape(), CursorShape::Underline);
        t.feed("\x1b[5 q");
        assert_eq!(t.cursor_shape(), CursorShape::Bar);
        t.feed("\x1b[2 q");
        assert_eq!(t.cursor_shape(), CursorShape::Block);
    }

    #[test]
    fn osc_color_query_replies_with_default() {
        let mut t = Terminal::new(5, 3, 100);
        t.set_default_colors([0xab, 0xcd, 0xef], [0x01, 0x02, 0x03], [0x77, 0x88, 0x99]);
        t.feed("\x1b]10;?\x07");
        assert_eq!(
            t.take_response(),
            b"\x1b]10;rgb:abab/cdcd/efef\x1b\\".to_vec(),
        );
        t.feed("\x1b]11;?\x1b\\");
        assert_eq!(
            t.take_response(),
            b"\x1b]11;rgb:0101/0202/0303\x1b\\".to_vec(),
        );
        t.feed("\x1b]12;?\x07");
        assert_eq!(
            t.take_response(),
            b"\x1b]12;rgb:7777/8888/9999\x1b\\".to_vec(),
        );
    }

    #[test]
    fn ring_buffer_matches_memmove_path() {
        // Differential torture test: the ring scroll fast path must produce the
        // exact same logical buffer (live grid + scrollback + cursor) as the
        // old `copy_within` memmove path, for arbitrary input. `a` uses the
        // ring; `b` is forced onto the memmove path. They must never diverge.
        fn snapshot(t: &Terminal) -> (usize, usize, Vec<String>, (usize, usize)) {
            let total = t.scrollback_len() + t.rows;
            let lines: Vec<String> = (0..total as isize)
                .map(|abs| {
                    t.line_at(abs)
                        .map(|row| row.iter().map(|c| c.ch).collect())
                        .unwrap_or_default()
                })
                .collect();
            let c = t.cursor();
            (t.rows, t.cols, lines, (c.row, c.col))
        }
        let mut a = Terminal::new(20, 6, 50); // ring enabled (default)
        let mut b = Terminal::new(20, 6, 50);
        b.primary.disable_ring = true;
        b.alternate.disable_ring = true;
        assert!(!a.primary.disable_ring, "test assumes ring on for `a`");

        let mut step = 0;
        let both = |a: &mut Terminal, b: &mut Terminal, s: &str, step: &mut i32| {
            a.feed(s);
            b.feed(s);
            *step += 1;
            assert_eq!(snapshot(a), snapshot(b), "ring vs memmove diverged at step {step}");
        };

        // Phase 1: plain lines -> repeated full-screen scroll-up (ring path) +
        // scrollback growth.
        for i in 0..30 {
            both(&mut a, &mut b, &format!("row number {i:02} xyz
"), &mut step);
        }
        // Phase 2: DECSTBM sub-region scroll (memmove/linearize path) while the
        // ring offset is non-zero on `a`.
        both(&mut a, &mut b, "\x1b[2;5r", &mut step);
        for i in 0..8 {
            both(&mut a, &mut b, &format!("sub{i}\r\n"), &mut step);
        }
        // Phase 3: insert / delete lines (linearize path).
        both(&mut a, &mut b, "\x1b[3;1H\x1b[3L", &mut step);
        both(&mut a, &mut b, "\x1b[2M", &mut step);
        // Phase 4: reset region, more full-screen scrolls (ring path again).
        both(&mut a, &mut b, "\x1b[r", &mut step);
        for i in 0..12 {
            both(&mut a, &mut b, &format!("again {i:02}\r\n"), &mut step);
        }
        // Phase 5: insert/delete chars within a row.
        both(&mut a, &mut b, "\x1b[1;3H\x1b[4@inserted\x1b[2P", &mut step);
        // Phase 6: erase display.
        both(&mut a, &mut b, "\x1b[2J\x1b[H", &mut step);
        for i in 0..10 {
            both(&mut a, &mut b, &format!("post-erase {i}\r\n"), &mut step);
        }
        // Phase 7: alt screen full-screen scroll (ring) then back to primary.
        both(&mut a, &mut b, "\x1b[?1049h", &mut step);
        for i in 0..10 {
            both(&mut a, &mut b, &format!("alt line {i}\r\n"), &mut step);
        }
        both(&mut a, &mut b, "\x1b[?1049l", &mut step);
        // Phase 8: resize while ring offset is non-zero, then more output.
        a.resize(12, 4);
        b.resize(12, 4);
        assert_eq!(snapshot(&a), snapshot(&b), "ring vs memmove diverged after resize");
        for i in 0..15 {
            both(&mut a, &mut b, &format!("rsz {i:02}\r\n"), &mut step);
        }
    }

    #[test]
    fn line_at_returns_scrollback_then_grid() {
        let mut t = Terminal::new(5, 2, 100);
        t.feed("AAAAA\r\nBBBBB\r\nCCCCC\r\nDDDDD");
        // scrollback = [AAAAA, BBBBB], grid = [CCCCC, DDDDD]
        assert_eq!(t.scrollback_len(), 2);
        assert_eq!(t.line_at(0).unwrap()[0].ch, 'A');
        assert_eq!(t.line_at(1).unwrap()[0].ch, 'B');
        assert_eq!(t.line_at(2).unwrap()[0].ch, 'C');
        assert_eq!(t.line_at(3).unwrap()[0].ch, 'D');
        assert!(t.line_at(4).is_none());
        assert!(t.line_at(-1).is_none());
    }

    #[test]
    fn visual_to_abs_line_matches_extended_cell() {
        let mut t = Terminal::new(5, 2, 100);
        t.feed("AAAAA\r\nBBBBB\r\nCCCCC\r\n");
        t.scroll_up(1);
        // Visual row 0 = newest scrollback (BBBBB) at abs index 1.
        let abs = t.visual_to_abs_line(0);
        assert_eq!(t.line_at(abs).unwrap()[0].ch, 'B');
        let abs = t.visual_to_abs_line(1);
        assert_eq!(t.line_at(abs).unwrap()[0].ch, 'C');
    }

    #[test]
    fn xtgettcap_replies_with_known_caps() {
        let mut t = Terminal::new(5, 3, 100);
        // "Co" = 0x43 0x6f -> "436f"; "ku" = "6b75". Both are known.
        t.feed("\x1bP+q436f;6b75\x1b\\");
        let reply = String::from_utf8(t.take_response()).unwrap();
        // Single 1+r DCS containing both name=value pairs.
        assert!(reply.starts_with("\x1bP1+r"), "got {reply:?}");
        assert!(reply.ends_with("\x1b\\"), "got {reply:?}");
        // "Co" = "256" → 323536, "ku" = ESC[A → 1b5b41
        assert!(reply.contains("436f=323536"), "got {reply:?}");
        assert!(reply.contains("6b75=1b5b41"), "got {reply:?}");
    }

    #[test]
    fn xtgettcap_replies_with_unknown_separately() {
        let mut t = Terminal::new(5, 3, 100);
        // "ZZ" = "5a5a" — not a real cap.
        t.feed("\x1bP+q5a5a\x1b\\");
        let reply = String::from_utf8(t.take_response()).unwrap();
        assert_eq!(reply, "\x1bP0+r5a5a\x1b\\");
    }

    #[test]
    fn xtgettcap_groups_known_and_unknown() {
        let mut t = Terminal::new(5, 3, 100);
        // "Co" known, "ZZ" unknown.
        t.feed("\x1bP+q436f;5a5a\x1b\\");
        let reply = String::from_utf8(t.take_response()).unwrap();
        assert!(reply.contains("\x1bP1+r436f=323536\x1b\\"));
        assert!(reply.contains("\x1bP0+r5a5a\x1b\\"));
    }

    #[test]
    fn xtgettcap_uppercase_hex_normalizes_in_reply() {
        let mut t = Terminal::new(5, 3, 100);
        t.feed("\x1bP+q436F\x1b\\"); // uppercase F
        let reply = String::from_utf8(t.take_response()).unwrap();
        // Reply hex names are emitted lowercase regardless of query case.
        assert!(reply.contains("436f="), "got {reply:?}");
    }

    #[test]
    fn dcs_without_xtgettcap_prefix_is_ignored() {
        let mut t = Terminal::new(5, 3, 100);
        t.feed("\x1bP$qmhello\x1b\\"); // DECRQSS or similar — not implemented
        assert!(t.take_response().is_empty());
    }

    #[test]
    fn dcs_tmux_passthrough_unwraps_and_reprocesses_body() {
        // tmux passthrough wraps an app's escape sequences for the
        // outer terminal: `ESC P tmux ; <body> ESC \\`, with literal
        // ESC bytes inside <body> doubled. Cat'ing a recording of
        // such output directly into yutani (no tmux in the loop)
        // should still dispatch the wrapped sequences — the ansi
        // parser un-doubles the ESCs and `handle_dcs` strips the
        // `tmux;` prefix and re-feeds the body through the parser.
        //
        // Pick an inner sequence whose effect we can observe: SGR 31
        // turns the cursor's fg red, and the printed 'A' should
        // carry that fg.
        let _guard = crate::palette::TEST_LOCK.lock().expect("test lock");
        crate::palette::install(crate::palette::Palette::defaults());
        let mut t = Terminal::new(5, 3, 100);
        // Doubled-ESC encoding of `ESC [ 3 1 m A`:
        t.feed("\x1bPtmux;\x1b\x1b[31mA\x1b\\");
        let cell = t.row(0)[0];
        assert_eq!(cell.ch, 'A');
        assert_eq!(
            cell.style.fg,
            crate::style::CellColor::Indexed(1),
            "wrapped SGR must have reached apply_sgr",
        );
    }

    #[test]
    fn dcs_tmux_passthrough_survives_pty_chunk_split_mid_next_dcs() {
        // Regression: when a PTY chunk delivers DCS#1 in full plus
        // the *start* of DCS#2, the outer parser ends Phase 1 in
        // DcsString (DCS#2 is still accumulating). Phase 2 then
        // dispatches DCS#1's event. If `handle_dcs` re-parses the
        // inner body through `self.parser` it inherits that stuck
        // DcsString state — the leading ESC of the inner APC turns
        // into an unrecognized DCS escape, the partial buf is
        // cleared, and the rest of the inner sequence is printed as
        // text instead of dispatched as an APC. A fresh, independent
        // parser sidesteps this entirely.
        let mut t = Terminal::new(20, 5, 100);
        t.set_cell_size_px(8, 16);
        // Feed DCS#1 complete + DCS#2's opener (no terminator yet).
        // DCS#1 wraps `ESC [31m A` (set fg red, print A).
        let chunk1 = "\x1bPtmux;\x1b\x1b[31mA\x1b\\\x1bPtmux;";
        t.feed(chunk1);
        // Without the fix: 'A' is never printed (the SGR + print
        // sequence got mangled by the stuck-DcsString re-feed).
        // With the fix: 'A' lands on the grid with red fg via the
        // properly-dispatched inner SGR + Print.
        let cell = t.row(0)[0];
        assert_eq!(cell.ch, 'A', "inner Print event must dispatch even when outer parser is mid-DCS");
        assert_eq!(
            cell.style.fg,
            crate::style::CellColor::Indexed(1),
            "inner SGR 31 must reach apply_sgr",
        );
        // Finish DCS#2 with a no-op body so the outer parser returns
        // to Ground cleanly for any follow-on chunk.
        t.feed("\x1b\\");
    }

    #[test]
    fn dcs_tmux_passthrough_dispatches_wrapped_kitty_transmit() {
        // Mirrors the real file from the bug report (~/bad-kitty.txt):
        // tmux-wrapped Kitty `a=T,i=N,...` payload. Walking it through
        // `Terminal::feed` should register the kitty image id so a
        // later `a=p,i=N` (or any lookup) sees it.
        let mut t = Terminal::new(20, 5, 100);
        t.set_cell_size_px(8, 16);
        // Build a tiny PNG just like the kitty E2E helpers.
        let png = {
            let buf = image::RgbaImage::from_pixel(2, 2, image::Rgba([0, 128, 255, 255]));
            let mut bytes = Vec::new();
            image::DynamicImage::ImageRgba8(buf)
                .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageOutputFormat::Png)
                .expect("encode");
            bytes
        };
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        // Wrap as tmux passthrough: doubled ESCs around the Kitty APC.
        let wrapped = format!(
            "\x1bPtmux;\x1b\x1b_Ga=T,f=100,i=4242,U=1;{}\x1b\x1b\\\x1b\\",
            b64,
        );
        t.feed(&wrapped);
        let uploads = t.take_pending_image_uploads();
        assert_eq!(
            uploads.len(), 1,
            "tmux-wrapped Kitty a=T must produce one pending upload",
        );
        assert_eq!(uploads[0].kitty_image_id, Some(4242));
    }

    #[test]
    fn a_f_without_i_targets_most_recently_completed_image() {
        // Per Kitty spec: when `i=` is missing on `a=f` (and `a=p` /
        // `a=d` / `a=a`), the most recently created image is the
        // implicit target. icat's animation stream relies on this —
        // frame transmissions for the GIF being animated arrive as
        // bare `a=f` with neither `i=` nor `m=`, no in-flight
        // chunked transmission to inherit from. Without the
        // last-completed fallback the frame silently drops and the
        // animation never plays past the base.
        let mut t = Terminal::new(20, 5, 100);
        t.set_cell_size_px(8, 16);
        // Establish a base image with explicit id 4242.
        let png = {
            let buf = image::RgbaImage::from_pixel(2, 2, image::Rgba([1, 2, 3, 255]));
            let mut bytes = Vec::new();
            image::DynamicImage::ImageRgba8(buf)
                .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageOutputFormat::Png)
                .expect("encode");
            bytes
        };
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        t.feed(&format!("\x1b_Ga=T,f=100,i=4242,U=1;{}\x1b\\", b64));
        let base_uploads = t.take_pending_image_uploads();
        assert_eq!(base_uploads.len(), 1);
        assert_eq!(base_uploads[0].kitty_image_id, Some(4242));

        // Now send a bare `a=f` — no `i=`, no `m=`. Should attach to
        // image 4242 via the spec fallback.
        let frame_png = {
            let buf = image::RgbaImage::from_pixel(1, 1, image::Rgba([9, 9, 9, 255]));
            let mut bytes = Vec::new();
            image::DynamicImage::ImageRgba8(buf)
                .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageOutputFormat::Png)
                .expect("encode");
            bytes
        };
        let b64 = base64::engine::general_purpose::STANDARD.encode(&frame_png);
        t.feed(&format!("\x1b_Ga=f,q=2;{}\x1b\\", b64));
        let frame_uploads = t.take_pending_image_uploads();
        assert_eq!(
            frame_uploads.len(),
            1,
            "bare a=f must produce one frame upload via implicit-i= fallback",
        );
        let up = &frame_uploads[0];
        assert!(up.animation_frame.is_some(), "a=f path flagged");
        assert_eq!(
            up.kitty_image_id,
            Some(4242),
            "implicit i= must resolve to the most recently completed image",
        );
    }

    #[test]
    fn a_f_without_i_drops_when_no_prior_image() {
        // Symmetric guard: no prior transmission, no implicit
        // fallback to leak into. Bare `a=f` must be ignored cleanly
        // rather than crash or create a stranded entry.
        let mut t = Terminal::new(20, 5, 100);
        t.set_cell_size_px(8, 16);
        let frame_png = {
            let buf = image::RgbaImage::from_pixel(1, 1, image::Rgba([9, 9, 9, 255]));
            let mut bytes = Vec::new();
            image::DynamicImage::ImageRgba8(buf)
                .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageOutputFormat::Png)
                .expect("encode");
            bytes
        };
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&frame_png);
        t.feed(&format!("\x1b_Ga=f,q=2;{}\x1b\\", b64));
        let uploads = t.take_pending_image_uploads();
        assert!(uploads.is_empty(), "no prior image → drop, don't crash");
    }

    #[test]
    fn dcs_tmux_passthrough_dispatches_wrapped_apc() {
        // The real-world case: tmux-wrapped Kitty graphics APC.
        // After unwrap, the body is `ESC _ G a=q,i=42 ESC \\` — a
        // Kitty capability query. Its dispatch path writes an `OK`
        // reply to `pending_response`; checking that proves the
        // unwrapped APC reached the right handler.
        let mut t = Terminal::new(5, 3, 100);
        t.feed("\x1bPtmux;\x1b\x1b_Ga=q,f=100,i=42\x1b\x1b\\\x1b\\");
        let reply = String::from_utf8(t.take_response()).unwrap_or_default();
        assert!(
            reply.contains("i=42") && reply.contains("OK"),
            "expected Kitty query OK reply with i=42; got {reply:?}",
        );
    }

    #[test]
    fn hex_codec_roundtrip() {
        for s in &["Co", "ku", "k1", "RGB", ""] {
            if s.is_empty() {
                continue;
            }
            let h = hex_encode(s);
            let back = hex_decode_ascii(&h).unwrap();
            assert_eq!(&back, s, "roundtrip {s:?}");
        }
        // Odd-length and non-hex are rejected.
        assert!(hex_decode_ascii("abc").is_none());
        assert!(hex_decode_ascii("zz").is_none());
    }

    #[test]
    fn osc_set_title_does_not_reply() {
        let mut t = Terminal::new(5, 3, 100);
        t.feed("\x1b]0;hello\x07");
        assert!(t.take_response().is_empty());
    }

    #[test]
    fn osc_2_sets_window_title() {
        let mut t = Terminal::new(5, 3, 100);
        t.feed("\x1b]2;my title\x07");
        assert_eq!(t.title(), Some("my title"));
        assert_eq!(t.take_title_update(), Some(Some("my title".to_string())));
        // Drained: no further update until it changes again.
        assert_eq!(t.take_title_update(), None);
    }

    #[test]
    fn osc_0_sets_window_title_with_st_terminator() {
        let mut t = Terminal::new(5, 3, 100);
        t.feed("\x1b]0;via osc0\x1b\\");
        assert_eq!(t.title(), Some("via osc0"));
        assert_eq!(t.take_title_update(), Some(Some("via osc0".to_string())));
    }

    #[test]
    fn osc_1_icon_title_is_ignored() {
        let mut t = Terminal::new(5, 3, 100);
        t.feed("\x1b]1;icon only\x07");
        assert_eq!(t.title(), None);
        assert_eq!(t.take_title_update(), None);
    }

    #[test]
    fn osc_empty_title_clears_back_to_none() {
        let mut t = Terminal::new(5, 3, 100);
        t.feed("\x1b]2;set\x07");
        assert_eq!(t.take_title_update(), Some(Some("set".to_string())));
        // Empty payload clears the manual title so the front end falls back.
        t.feed("\x1b]2;\x07");
        assert_eq!(t.title(), None);
        assert_eq!(t.take_title_update(), Some(None));
    }

    #[test]
    fn osc_same_title_repeated_is_not_dirty() {
        let mut t = Terminal::new(5, 3, 100);
        t.feed("\x1b]2;same\x07");
        assert_eq!(t.take_title_update(), Some(Some("same".to_string())));
        // Re-emitting the identical title doesn't re-flag dirty.
        t.feed("\x1b]2;same\x07");
        assert_eq!(t.take_title_update(), None);
    }

    #[test]
    fn manual_title_locks_out_osc() {
        let mut t = Terminal::new(5, 3, 100);
        // Pin a title via the palette path.
        t.set_manual_title("pinned");
        assert_eq!(t.title(), Some("pinned"));
        assert_eq!(t.take_title_update(), Some(Some("pinned".to_string())));
        // The shell now tries to set the title via OSC 0/2 — both ignored.
        t.feed("\x1b]2;from shell\x07");
        t.feed("\x1b]0;from shell\x07");
        assert_eq!(t.title(), Some("pinned"));
        assert_eq!(t.take_title_update(), None);
        // Even an empty OSC clear is ignored while locked.
        t.feed("\x1b]2;\x07");
        assert_eq!(t.title(), Some("pinned"));
        assert_eq!(t.take_title_update(), None);
    }

    #[test]
    fn clearing_manual_title_unlocks_osc() {
        let mut t = Terminal::new(5, 3, 100);
        t.set_manual_title("pinned");
        assert_eq!(t.take_title_update(), Some(Some("pinned".to_string())));
        // Clearing from the palette (empty) drops the override and unlocks.
        t.set_manual_title("");
        assert_eq!(t.title(), None);
        assert_eq!(t.take_title_update(), Some(None));
        // The shell can drive the title again.
        t.feed("\x1b]2;shell again\x07");
        assert_eq!(t.title(), Some("shell again"));
        assert_eq!(t.take_title_update(), Some(Some("shell again".to_string())));
    }

    #[test]
    fn manual_title_overrides_prior_osc_title() {
        let mut t = Terminal::new(5, 3, 100);
        // Shell sets a title first; then the user pins their own over it.
        t.feed("\x1b]2;shell\x07");
        assert_eq!(t.take_title_update(), Some(Some("shell".to_string())));
        t.set_manual_title("pinned");
        assert_eq!(t.title(), Some("pinned"));
        assert_eq!(t.take_title_update(), Some(Some("pinned".to_string())));
        // Subsequent shell requests stay locked out.
        t.feed("\x1b]2;shell again\x07");
        assert_eq!(t.title(), Some("pinned"));
        assert_eq!(t.take_title_update(), None);
    }

    #[test]
    fn osc_0_sets_window_title_with_bel_terminator() {
        // OSC 0 + BEL is the most common form programs emit; the ST-terminated
        // OSC 0 case is covered separately, so pin the BEL path too.
        let mut t = Terminal::new(5, 3, 100);
        t.feed("\x1b]0;via osc0\x07");
        assert_eq!(t.title(), Some("via osc0"));
        assert_eq!(t.take_title_update(), Some(Some("via osc0".to_string())));
    }

    #[test]
    fn osc_sequential_title_changes_each_mark_dirty() {
        // Distinct successive titles each produce their own pending update.
        let mut t = Terminal::new(5, 3, 100);
        t.feed("\x1b]2;first\x07");
        assert_eq!(t.take_title_update(), Some(Some("first".to_string())));
        t.feed("\x1b]2;second\x07");
        assert_eq!(t.title(), Some("second"));
        assert_eq!(t.take_title_update(), Some(Some("second".to_string())));
        assert_eq!(t.take_title_update(), None);
    }

    #[test]
    fn osc_two_changes_before_drain_coalesce_to_latest() {
        // The front end only drains once per feed; two changes between drains
        // collapse to the most recent value, not the intermediate one.
        let mut t = Terminal::new(5, 3, 100);
        t.feed("\x1b]2;old\x07");
        t.feed("\x1b]2;new\x07");
        assert_eq!(t.take_title_update(), Some(Some("new".to_string())));
        assert_eq!(t.take_title_update(), None);
    }

    #[test]
    fn osc_clear_then_set_again_round_trips() {
        // Manual title -> cleared (fall back to cwd) -> set again, with each
        // transition surfacing exactly one update.
        let mut t = Terminal::new(5, 3, 100);
        t.feed("\x1b]2;editing\x07");
        assert_eq!(t.take_title_update(), Some(Some("editing".to_string())));
        t.feed("\x1b]2;\x07"); // program exits, clears its title
        assert_eq!(t.title(), None);
        assert_eq!(t.take_title_update(), Some(None));
        t.feed("\x1b]2;again\x07"); // a new program sets one
        assert_eq!(t.title(), Some("again"));
        assert_eq!(t.take_title_update(), Some(Some("again".to_string())));
    }

    #[test]
    fn osc_empty_title_on_fresh_terminal_stays_clean() {
        // Clearing a title that was never set is a no-op: still None, not dirty.
        let mut t = Terminal::new(5, 3, 100);
        t.feed("\x1b]2;\x07");
        assert_eq!(t.title(), None);
        assert_eq!(t.take_title_update(), None);
    }

    #[test]
    fn osc_repeated_clear_is_not_dirty() {
        // After a clear is drained, a second empty payload doesn't re-flag dirty.
        let mut t = Terminal::new(5, 3, 100);
        t.feed("\x1b]2;set\x07");
        assert_eq!(t.take_title_update(), Some(Some("set".to_string())));
        t.feed("\x1b]2;\x07");
        assert_eq!(t.take_title_update(), Some(None));
        t.feed("\x1b]2;\x07");
        assert_eq!(t.take_title_update(), None);
    }

    #[test]
    fn osc_title_payload_preserves_embedded_semicolons() {
        // Only the first ';' splits the OSC code from its payload; the rest of
        // the title (which legitimately contains ';') is kept verbatim.
        let mut t = Terminal::new(5, 3, 100);
        t.feed("\x1b]2;vim: a; b; c\x07");
        assert_eq!(t.title(), Some("vim: a; b; c"));
    }

    #[test]
    fn osc_0_and_2_share_the_same_title_slot() {
        // OSC 0 (icon+title) and OSC 2 (title) both target the one displayed
        // title, so a later OSC 0 overrides an earlier OSC 2 with no extra
        // dirty churn for the no-op case.
        let mut t = Terminal::new(5, 3, 100);
        t.feed("\x1b]2;from-2\x07");
        assert_eq!(t.take_title_update(), Some(Some("from-2".to_string())));
        t.feed("\x1b]0;from-0\x07");
        assert_eq!(t.title(), Some("from-0"));
        assert_eq!(t.take_title_update(), Some(Some("from-0".to_string())));
    }

    #[test]
    fn osc_1_does_not_disturb_an_existing_title() {
        // An OSC 1 (icon name) arriving after a real title leaves the title and
        // its (already drained) dirty state untouched.
        let mut t = Terminal::new(5, 3, 100);
        t.feed("\x1b]2;real title\x07");
        assert_eq!(t.take_title_update(), Some(Some("real title".to_string())));
        t.feed("\x1b]1;icon\x07");
        assert_eq!(t.title(), Some("real title"));
        assert_eq!(t.take_title_update(), None);
    }

    #[test]
    fn mouse_modes_track_private_set_reset() {
        let mut t = Terminal::new(5, 3, 100);
        t.feed("\x1b[?1000h\x1b[?1006h");
        let mp = t.mouse_protocol();
        assert!(mp.press_release);
        assert!(mp.sgr);
        assert!(mp.enabled());
        t.feed("\x1b[?1000l\x1b[?1006l");
        assert!(!t.mouse_protocol().enabled());
    }

    #[test]
    fn bracketed_paste_mode_tracks() {
        let mut t = Terminal::new(5, 3, 100);
        assert!(!t.bracketed_paste());
        t.feed("\x1b[?2004h");
        assert!(t.bracketed_paste());
        t.feed("\x1b[?2004l");
        assert!(!t.bracketed_paste());
    }

    #[test]
    fn decrqm_reports_set_and_reset_for_private_mode() {
        let mut t = Terminal::new(5, 3, 100);
        // 2004 (bracketed paste) starts reset → Pm = 2.
        t.feed("\x1b[?2004$p");
        assert_eq!(t.take_response(), b"\x1b[?2004;2$y".to_vec());
        // Enable it, then query → Pm = 1.
        t.feed("\x1b[?2004h");
        t.feed("\x1b[?2004$p");
        assert_eq!(t.take_response(), b"\x1b[?2004;1$y".to_vec());
    }

    #[test]
    fn decrqm_tracks_live_mode_state() {
        // Grapheme clustering (2027) defaults on → 1; disabling flips it to 2.
        let mut t = Terminal::new(5, 3, 100);
        t.feed("\x1b[?2027$p");
        assert_eq!(t.take_response(), b"\x1b[?2027;1$y".to_vec());
        t.feed("\x1b[?2027l");
        t.feed("\x1b[?2027$p");
        assert_eq!(t.take_response(), b"\x1b[?2027;2$y".to_vec());
    }

    #[test]
    fn decrqm_unrecognized_private_mode_reports_zero() {
        let mut t = Terminal::new(5, 3, 100);
        t.feed("\x1b[?9999$p");
        assert_eq!(t.take_response(), b"\x1b[?9999;0$y".to_vec());
    }

    #[test]
    fn decrqm_ansi_mode_reports_zero() {
        // No ANSI (non-private) modes are implemented, so any are "not
        // recognized" — and the reply omits the `?` private prefix.
        let mut t = Terminal::new(5, 3, 100);
        t.feed("\x1b[4$p");
        assert_eq!(t.take_response(), b"\x1b[4;0$y".to_vec());
    }

    #[test]
    fn decrqm_reports_default_on_modes_without_prior_toggle() {
        // A mode that powers on enabled must report Pm = 1 on a fresh terminal,
        // with no `h`/`l` ever issued — proving `private_mode_state` reads the
        // live default, not a "seen a set" flag. Autowrap (7) and cursor
        // visibility (25) both default on in `Terminal::new`.
        let mut t = Terminal::new(5, 3, 100);
        t.feed("\x1b[?7$p");
        assert_eq!(t.take_response(), b"\x1b[?7;1$y".to_vec());
        t.feed("\x1b[?25$p");
        assert_eq!(t.take_response(), b"\x1b[?25;1$y".to_vec());
    }

    #[test]
    fn decrqm_alt_screen_mode_follows_screen_switch() {
        // 47/1047/1049 all map to `use_alternate` in `private_mode_state`.
        // On the primary screen the mode reads reset (2); after switching to
        // the alternate screen it reads set (1). Confirms DECRQM tracks the
        // real screen state rather than a per-code flag.
        let mut t = Terminal::new(5, 3, 100);
        t.feed("\x1b[?1049$p");
        assert_eq!(t.take_response(), b"\x1b[?1049;2$y".to_vec());
        t.feed("\x1b[?1049h"); // enter alternate screen
        t.feed("\x1b[?1049$p");
        assert_eq!(t.take_response(), b"\x1b[?1049;1$y".to_vec());
    }

    #[test]
    fn decrqm_query_does_not_mutate_queried_mode() {
        // DECRQM is a pure read: querying a disabled mode twice must report
        // reset (2) both times — the query must never flip the state it reads.
        let mut t = Terminal::new(5, 3, 100);
        t.feed("\x1b[?2004$p");
        assert_eq!(t.take_response(), b"\x1b[?2004;2$y".to_vec());
        t.feed("\x1b[?2004$p");
        assert_eq!(t.take_response(), b"\x1b[?2004;2$y".to_vec());
        assert!(!t.bracketed_paste(), "query must leave the mode untouched");
    }

    #[test]
    fn decrqm_omitted_param_reports_zero() {
        // A DECRQM with no param (`CSI ? $ p` / `CSI $ p`) defaults to mode 0,
        // which is never a real mode → "not recognized" (Pm = 0). The reply
        // echoes mode 0, keeping or dropping the `?` to match the request.
        let mut t = Terminal::new(5, 3, 100);
        t.feed("\x1b[?$p");
        assert_eq!(t.take_response(), b"\x1b[?0;0$y".to_vec());
        t.feed("\x1b[$p");
        assert_eq!(t.take_response(), b"\x1b[0;0$y".to_vec());
    }

    #[test]
    fn decrqm_reports_more_implemented_modes() {
        // A couple more round-trips beyond the existing 2004/2026/2027 cases.
        let mut t = Terminal::new(5, 3, 100);
        // Focus reporting (1004) defaults off; enable → 1.
        t.feed("\x1b[?1004h");
        t.feed("\x1b[?1004$p");
        assert_eq!(t.take_response(), b"\x1b[?1004;1$y".to_vec());
        // Cursor visibility (25) defaults on; hide via `?25l` → 2.
        t.feed("\x1b[?25l");
        t.feed("\x1b[?25$p");
        assert_eq!(t.take_response(), b"\x1b[?25;2$y".to_vec());
    }

    #[test]
    fn mode_2026_synchronized_output_tracks() {
        let mut t = Terminal::new(5, 3, 100);
        assert!(!t.sync_update());
        t.feed("\x1b[?2026h"); // BSU
        assert!(t.sync_update());
        t.feed("\x1b[?2026l"); // ESU
        assert!(!t.sync_update());
    }

    #[test]
    fn mode_2026_content_between_bsu_esu_still_applies_to_grid() {
        // Synchronized output gates *presentation*, not parsing: the grid must
        // keep updating while sync is held so the finished frame is correct when
        // the front end finally paints it.
        let mut t = Terminal::new(5, 3, 100);
        t.feed("\x1b[?2026hAB");
        assert!(t.sync_update());
        assert_eq!(t.row(0)[0].ch, 'A');
        assert_eq!(t.row(0)[1].ch, 'B');
        t.feed("C\x1b[?2026l");
        assert!(!t.sync_update());
        assert_eq!(t.row(0)[2].ch, 'C');
    }

    #[test]
    fn clear_sync_update_forces_release_and_later_esu_is_noop() {
        // The front end's safety timeout force-releases a sync frame an app
        // never ended; a subsequent stray ESU must not reassert anything.
        let mut t = Terminal::new(5, 3, 100);
        t.feed("\x1b[?2026h");
        assert!(t.sync_update());
        t.clear_sync_update();
        assert!(!t.sync_update());
        t.feed("\x1b[?2026l");
        assert!(!t.sync_update());
    }

    #[test]
    fn mode_2026_bsu_is_idempotent() {
        // Apps may emit BSU again without an intervening ESU (e.g. nested
        // frame guards); sync is a single boolean, so the second BSU is a
        // no-op and one ESU still fully releases the held frame.
        let mut t = Terminal::new(5, 3, 100);
        t.feed("\x1b[?2026h\x1b[?2026h");
        assert!(t.sync_update());
        t.feed("\x1b[?2026l");
        assert!(!t.sync_update());
    }

    #[test]
    fn mode_2026_toggles_across_feed_chunk_boundary() {
        // PTY reads split anywhere, so the private-mode sequence can arrive in
        // pieces; the parser must reassemble it across feed() calls rather than
        // resetting mid-sequence and dropping the toggle.
        let mut t = Terminal::new(5, 3, 100);
        t.feed("\x1b[?2026");
        assert!(!t.sync_update()); // sequence not yet terminated
        t.feed("h");
        assert!(t.sync_update());
        t.feed("\x1b[?20");
        t.feed("26l");
        assert!(!t.sync_update());
    }

    #[test]
    fn mode_2026_unrelated_private_mode_does_not_disturb_sync() {
        // Each DEC private mode is independent; toggling something else (here
        // cursor visibility, mode 25) while a frame is held must not perturb
        // the synchronized-output state.
        let mut t = Terminal::new(5, 3, 100);
        t.feed("\x1b[?2026h");
        assert!(t.sync_update());
        t.feed("\x1b[?25l"); // hide cursor mid-frame
        assert!(t.sync_update());
        assert!(!t.cursor_visible());
        t.feed("\x1b[?2026l");
        assert!(!t.sync_update());
    }

    #[test]
    fn clear_sync_update_when_not_in_sync_is_noop() {
        // The front-end safety timeout may fire clear_sync_update() defensively
        // even when no frame is held; that must be harmless rather than flip any
        // state.
        let mut t = Terminal::new(5, 3, 100);
        assert!(!t.sync_update());
        t.clear_sync_update();
        assert!(!t.sync_update());
    }

    #[test]
    fn mode_2026_cleared_by_full_reset() {
        // RIS restores the power-on state, which has synchronized output off.
        // A BSU that's never matched by an ESU must not survive the reset and
        // keep the front end holding the present.
        let mut t = Terminal::new(5, 3, 100);
        t.feed("\x1b[?2026h");
        assert!(t.sync_update());
        t.feed("\x1bc"); // RIS
        assert!(!t.sync_update());
    }

    #[test]
    fn mode_1004_focus_report_emits_in_and_out() {
        let mut t = Terminal::new(5, 3, 100);
        t.feed("\x1b[?1004h");
        // Focus-in → CSI I, focus-out → CSI O; each returns true (queued) so
        // the front end knows to flush.
        assert!(t.focus_report(true));
        assert_eq!(t.take_response(), b"\x1b[I".to_vec());
        assert!(t.focus_report(false));
        assert_eq!(t.take_response(), b"\x1b[O".to_vec());
    }

    #[test]
    fn focus_report_silent_when_mode_1004_disabled() {
        let mut t = Terminal::new(5, 3, 100);
        // No `CSI ? 1004 h` → focus changes produce nothing.
        assert!(!t.focus_report(true));
        assert!(!t.focus_report(false));
        assert!(t.take_response().is_empty());
    }

    #[test]
    fn mode_1004_reset_stops_focus_reports() {
        let mut t = Terminal::new(5, 3, 100);
        t.feed("\x1b[?1004h");
        t.feed("\x1b[?1004l");
        assert!(!t.focus_report(true));
        assert!(t.take_response().is_empty());
    }

    #[test]
    fn mode_1004_cleared_by_full_reset() {
        // RIS restores power-on state, where focus reporting is off.
        let mut t = Terminal::new(5, 3, 100);
        t.feed("\x1b[?1004h");
        assert!(t.focus_report(true));
        let _ = t.take_response();
        t.feed("\x1bc"); // RIS
        assert!(!t.focus_report(true));
        assert!(t.take_response().is_empty());
    }

    #[test]
    fn mode_1004_independent_of_bracketed_paste() {
        // 1004 and 2004 sit next to each other in the private-mode match arm;
        // toggling one must not bleed into the other (guards a fall-through or
        // copy-paste typo between the two arms).
        let mut t = Terminal::new(5, 3, 100);
        t.feed("\x1b[?1004h");
        assert!(t.focus_report(true), "1004 should be on");
        let _ = t.take_response();
        assert!(!t.bracketed_paste(), "enabling 1004 must not enable 2004");

        let mut t = Terminal::new(5, 3, 100);
        t.feed("\x1b[?2004h");
        assert!(t.bracketed_paste(), "2004 should be on");
        assert!(
            !t.focus_report(true),
            "enabling 2004 must not enable focus reporting"
        );
        assert!(t.take_response().is_empty());
    }

    #[test]
    fn mode_1004_and_2004_set_together_in_one_sequence() {
        // The parser emits one PrivateModeSet per param, so a multi-param `h`
        // must enable every listed mode independently.
        let mut t = Terminal::new(5, 3, 100);
        t.feed("\x1b[?1004;2004h");
        assert!(t.bracketed_paste(), "2004 enabled by combined sequence");
        assert!(
            t.focus_report(true),
            "1004 enabled by combined sequence"
        );
        assert_eq!(t.take_response(), b"\x1b[I".to_vec());
    }

    #[test]
    fn focus_report_does_not_debounce() {
        // Focus reporting is edge-driven by the front end: the terminal just
        // serializes exactly what it's told, so repeated same-direction reports
        // each queue another `CSI I` rather than collapsing into one.
        let mut t = Terminal::new(5, 3, 100);
        t.feed("\x1b[?1004h");
        assert!(t.focus_report(true));
        assert!(t.focus_report(true));
        assert!(t.focus_report(true));
        assert_eq!(t.take_response(), b"\x1b[I\x1b[I\x1b[I".to_vec());
    }

    #[test]
    fn focus_report_appends_after_pending_dsr_reply() {
        // A focus change can land while an earlier query reply is still
        // unflushed; the report must follow it in order, since `take_response`
        // hands back the buffer as a single ordered stream.
        let mut t = Terminal::new(10, 3, 100);
        t.feed("\x1b[?1004h");
        t.feed("\x1b[5n"); // DSR — queues `CSI 0 n`
        assert!(t.focus_report(true));
        assert_eq!(t.take_response(), b"\x1b[0n\x1b[I".to_vec());
    }

    #[test]
    fn full_reset_clears_app_cursor_keys_and_response() {
        let mut t = Terminal::new(5, 3, 100);
        t.feed("\x1b[?1h\x1b[5n");
        t.feed("\x1bc"); // RIS
        assert!(!t.app_cursor_keys());
        assert!(t.take_response().is_empty());
    }

    #[test]
    fn full_reset() {
        let mut t = Terminal::new(5, 3, 100);
        t.feed("ABC\r\nXYZ\x1b[?25l");
        t.feed("\x1bc");
        assert_eq!(render(&t), "\n\n");
        assert!(t.cursor_visible());
        assert_eq!(t.cursor().row, 0);
        assert_eq!(t.cursor().col, 0);
    }

    #[test]
    fn osc_is_swallowed() {
        let mut t = Terminal::new(10, 1, 100);
        t.feed("\x1b]0;title\x07after");
        assert_eq!(t.row(0).iter().map(|c| c.ch).take(5).collect::<String>(), "after");
    }

    #[test]
    fn prompt_sp_pattern() {
        // The pattern that caused the %-bug: zsh prints '%', pads with spaces,
        // then CR and CSI K to wipe it before printing the real prompt.
        let mut t = Terminal::new(10, 2, 100);
        t.feed("%         \r\x1b[K$ ");
        assert_eq!(t.row(0).iter().map(|c| c.ch).collect::<String>(), "$         ");
        assert_eq!(t.cursor().row, 0);
        assert_eq!(t.cursor().col, 2);
    }

    #[test]
    fn resize_preserves_top_left_content() {
        let mut t = Terminal::new(5, 3, 100);
        t.feed("HELLO");
        t.resize(3, 3);
        assert_eq!(t.row(0).iter().map(|c| c.ch).collect::<String>(), "HEL");
        assert_eq!(t.cols, 3);
    }

    /// Helper: read a row of cells back as a trimmed string, for legibility
    /// in the resize-reflow assertions below.
    fn row_string(cells: &[Cell]) -> String {
        cells.iter().map(|c| c.ch).collect::<String>().trim_end().to_string()
    }

    #[test]
    fn resize_vertical_shrink_spills_top_rows_into_scrollback() {
        let mut t = Terminal::new(10, 5, 100);
        t.feed("L1\r\nL2\r\nL3\r\nL4\r\nL5");
        // Pre-conditions: top row holds L1, cursor sits on the last grid row.
        assert_eq!(row_string(t.row(0)), "L1");
        assert_eq!(t.cursor().row, 4);
        assert_eq!(t.scrollback_len(), 0);

        t.resize(10, 3);

        // Two top rows spill into scrollback in chronological order.
        assert_eq!(t.scrollback_len(), 2);
        assert_eq!(row_string(t.line_at(0).unwrap()), "L1");
        assert_eq!(row_string(t.line_at(1).unwrap()), "L2");
        // Grid now holds the bottom three lines.
        assert_eq!(row_string(t.row(0)), "L3");
        assert_eq!(row_string(t.row(1)), "L4");
        assert_eq!(row_string(t.row(2)), "L5");
        // Cursor was at row 4; spill of 2 brings it down to row 2 (still on L5).
        assert_eq!(t.cursor().row, 2);
    }

    #[test]
    fn resize_vertical_grow_pulls_from_scrollback() {
        let mut t = Terminal::new(10, 3, 100);
        t.feed("L1\r\nL2\r\nL3\r\nL4\r\nL5");
        // Pre-conditions: 2 rows already in scrollback, cursor on L5 (row 2).
        assert_eq!(t.scrollback_len(), 2);
        assert_eq!(t.cursor().row, 2);

        t.resize(10, 5);

        // Scrollback fully drained back into the grid.
        assert_eq!(t.scrollback_len(), 0);
        assert_eq!(row_string(t.row(0)), "L1");
        assert_eq!(row_string(t.row(1)), "L2");
        assert_eq!(row_string(t.row(2)), "L3");
        assert_eq!(row_string(t.row(3)), "L4");
        assert_eq!(row_string(t.row(4)), "L5");
        // Cursor steps down by the pull count (2) to stay on L5.
        assert_eq!(t.cursor().row, 4);
    }

    #[test]
    fn resize_shrink_then_grow_round_trip_preserves_content() {
        // The user-reported regression: shrinking and growing back used to
        // leave blank rows where the prompt had been.
        let mut t = Terminal::new(10, 5, 100);
        t.feed("L1\r\nL2\r\nL3\r\nL4\r\nL5");
        let before = render(&t);
        assert_eq!(t.cursor().row, 4);

        t.resize(10, 3);
        t.resize(10, 5);

        assert_eq!(render(&t), before);
        assert_eq!(t.cursor().row, 4);
        assert_eq!(t.scrollback_len(), 0);
    }

    #[test]
    fn resize_vertical_shrink_evicts_oldest_when_scrollback_full() {
        // scrollback_limit = 1 means a 2-row spill must drop the older line.
        let mut t = Terminal::new(10, 5, 1);
        t.feed("L1\r\nL2\r\nL3\r\nL4\r\nL5");
        assert_eq!(t.scrollback_len(), 0);

        t.resize(10, 3);

        // Spill of 2 with capacity 1: L1 evicted via pop_front, L2 kept.
        assert_eq!(t.scrollback_len(), 1);
        assert_eq!(row_string(t.line_at(0).unwrap()), "L2");
        // Grid still holds the bottom three lines.
        assert_eq!(row_string(t.row(0)), "L3");
        assert_eq!(row_string(t.row(1)), "L4");
        assert_eq!(row_string(t.row(2)), "L5");
    }

    #[test]
    fn resize_vertical_grow_with_no_scrollback_adds_blank_rows_at_top() {
        let mut t = Terminal::new(10, 3, 100);
        // No input — empty grid, empty scrollback, cursor at origin.
        assert_eq!(t.scrollback_len(), 0);
        assert_eq!(t.cursor().row, 0);

        t.resize(10, 5);

        assert_eq!(t.scrollback_len(), 0);
        // Nothing was pulled — cursor untouched, all rows blank.
        assert_eq!(t.cursor().row, 0);
        for r in 0..5 {
            assert_eq!(row_string(t.row(r)), "");
        }
    }

    #[test]
    fn resize_shrink_keeps_top_content_when_grid_has_blank_rows_below() {
        // Regression for the stacked-prompt bug: a prompt sitting at the top
        // of an otherwise-empty grid must not churn into scrollback on
        // shrink. The old reflow spilled `old_rows - rows` top rows
        // unconditionally, so shrinking pushed the prompt into scrollback;
        // the shell then repainted a fresh prompt, and growing back pulled
        // the old copies in as stacked duplicates. With content-aware spill,
        // the blank rows below the content are trimmed instead.
        let mut t = Terminal::new(10, 10, 100);
        t.feed("BAR\r\nPROMPT"); // rows 0..1 hold content; cursor on row 1
        assert_eq!(t.cursor().row, 1);

        // Shrink well below the original height. The two content rows still
        // fit, so nothing spills and the prompt stays put.
        t.resize(10, 4);
        assert_eq!(t.scrollback_len(), 0);
        assert_eq!(row_string(t.row(0)), "BAR");
        assert_eq!(row_string(t.row(1)), "PROMPT");
        assert_eq!(t.cursor().row, 1);

        // Grow back: no scrollback to pull, so blank rows are added below and
        // the prompt does not move or duplicate.
        t.resize(10, 10);
        assert_eq!(t.scrollback_len(), 0);
        assert_eq!(row_string(t.row(0)), "BAR");
        assert_eq!(row_string(t.row(1)), "PROMPT");
        assert_eq!(t.cursor().row, 1);
        for r in 2..10 {
            assert_eq!(row_string(t.row(r)), "");
        }
    }

    #[test]
    fn resize_on_alt_screen_does_not_touch_scrollback() {
        let mut t = Terminal::new(10, 5, 100);
        // Push two lines into primary scrollback before switching screens.
        t.feed("L1\r\nL2\r\nL3\r\nL4\r\nL5\r\nL6\r\nL7");
        let scrollback_before = t.scrollback_len();
        assert_eq!(scrollback_before, 2);

        // Enter alt screen and write something there.
        t.feed("\x1b[?1049h");
        t.feed("alt-text");
        assert_eq!(row_string(t.row(0)), "alt-text");

        // Shrink + grow on the alt screen must not touch primary scrollback.
        t.resize(10, 3);
        assert_eq!(t.scrollback_len(), scrollback_before);
        t.resize(10, 5);
        assert_eq!(t.scrollback_len(), scrollback_before);

        // Alt grid is rebuilt blank on resize (existing alt-screen behavior).
        for r in 0..5 {
            assert_eq!(row_string(t.row(r)), "");
        }
    }

    #[test]
    fn resize_no_op_when_dimensions_match() {
        let mut t = Terminal::new(10, 5, 100);
        t.feed("L1\r\nL2\r\nL3\r\nL4\r\nL5");
        let before_render = render(&t);
        let before_scrollback = t.scrollback_len();
        let before_cursor = t.cursor();
        let before_view = t.view_offset();

        t.resize(10, 5);

        assert_eq!(render(&t), before_render);
        assert_eq!(t.scrollback_len(), before_scrollback);
        assert_eq!(t.cursor().row, before_cursor.row);
        assert_eq!(t.cursor().col, before_cursor.col);
        assert_eq!(t.view_offset(), before_view);
    }

    #[test]
    fn scroll_up_pulls_scrollback_into_view() {
        let mut t = Terminal::new(5, 2, 100);
        // Fill beyond the viewport so a line is forced into scrollback.
        t.feed("AAAAA\r\nBBBBB\r\nCCCCC\r\n");
        assert_eq!(t.scrollback_len(), 2);
        assert!(t.at_bottom());
        assert!(t.scroll_up(1));
        assert_eq!(t.view_offset(), 1);
        // The top visible row is now the newest scrollback line (BBBBB); the
        // bottom visible row is what was previously row 0 of the grid (CCCCC).
        assert_eq!(t.visible_cell(0, 0).ch, 'B');
        assert_eq!(t.visible_cell(1, 0).ch, 'C');
    }

    #[test]
    fn viewport_stays_anchored_as_new_lines_enter_scrollback() {
        let mut t = Terminal::new(5, 2, 100);
        t.feed("AAAAA\r\nBBBBB\r\nCCCCC\r\n");
        // scrollback = [AAAAA, BBBBB], grid = [CCCCC, _]
        assert_eq!(t.scrollback_len(), 2);
        assert!(t.scroll_up(2));
        // Top of viewport pinned to AAAAA; bottom to BBBBB.
        assert_eq!(t.visible_cell(0, 0).ch, 'A');
        assert_eq!(t.visible_cell(1, 0).ch, 'B');

        // New lines stream in. The visible content must not shift.
        t.feed("DDDDD\r\nEEEEE\r\n");
        assert_eq!(t.visible_cell(0, 0).ch, 'A');
        assert_eq!(t.visible_cell(1, 0).ch, 'B');
    }

    #[test]
    fn viewport_pins_to_top_when_oldest_scrollback_evicts() {
        // scrollback_limit = 3 — once full, pop_front evicts oldest.
        let mut t = Terminal::new(5, 2, 3);
        t.feed("AAAAA\r\nBBBBB\r\nCCCCC\r\nDDDDD\r\nEEEEE\r\n");
        // scrollback = [BBBBB, CCCCC, DDDDD] (AAAAA already evicted),
        // grid = [EEEEE, _]
        assert_eq!(t.scrollback_len(), 3);
        assert!(t.scroll_up(3));
        assert_eq!(t.visible_cell(0, 0).ch, 'B');
        assert_eq!(t.visible_cell(1, 0).ch, 'C');

        // Push a new line: BBBBB is evicted. We can no longer show it, so
        // pin to the new oldest line (CCCCC) without indexing past the buffer.
        t.feed("FFFFF\r\n");
        assert_eq!(t.scrollback_len(), 3);
        assert_eq!(t.view_offset(), 3);
        assert_eq!(t.visible_cell(0, 0).ch, 'C');
        assert_eq!(t.visible_cell(1, 0).ch, 'D');
    }

    #[test]
    fn scrollback_evicted_counts_front_evictions() {
        // 2-row grid, scrollback limit 2. The grid holds 2 lines before any
        // spill, so it takes 4 fed lines to fill scrollback to the limit.
        let mut t = Terminal::new(5, 2, 2);
        assert_eq!(t.scrollback_evicted(), 0);
        t.feed("AAAAA\r\nBBBBB\r\nCCCCC\r\n"); // sb=[AAAAA, BBBBB], grid=[CCCCC,_]
        assert_eq!(t.scrollback_len(), 2);
        assert_eq!(t.scrollback_evicted(), 0, "filling to the limit evicts nothing");
        t.feed("DDDDD\r\n"); // spill CCCCC → evict AAAAA
        assert_eq!(t.scrollback_evicted(), 1);
        t.feed("EEEEE\r\nFFFFF\r\n"); // spill DDDDD, EEEEE → evict BBBBB, CCCCC
        assert_eq!(t.scrollback_evicted(), 3);
        assert_eq!(t.scrollback_len(), 2, "len pins at the limit");
    }

    #[test]
    fn stable_abs_line_is_monotonic_across_eviction() {
        // The renderer keys its row cache by `scrollback_evicted + abs_line`.
        // Plain abs-line (`scrollback_len - view_offset + r`) pins once
        // scrollback is full, so it would alias distinct lines onto one key;
        // the folded id must keep advancing instead. This guards that invariant.
        let mut t = Terminal::new(5, 2, 3);
        let stable_top = |t: &Terminal| t.scrollback_evicted() as isize + t.visual_to_abs_line(0);
        let mut prev = stable_top(&t);
        let mut saw_eviction = false;
        for line in ["AAAAA", "BBBBB", "CCCCC", "DDDDD", "EEEEE", "FFFFF", "GGGGG"] {
            t.feed(line);
            t.feed("\r\n");
            let now = stable_top(&t);
            assert!(now >= prev, "stable top-line id must never go backwards");
            if t.scrollback_evicted() > 0 {
                saw_eviction = true;
                // Once full, plain abs-line is pinned at the limit…
                assert_eq!(t.visual_to_abs_line(0), t.scrollback_len() as isize);
                // …yet the folded id keeps climbing as content streams.
                assert!(now > prev, "folded id must advance even after eviction");
            }
            prev = now;
        }
        assert!(saw_eviction, "test should exercise the eviction path");
    }

    #[test]
    fn scroll_clamps_at_ends() {
        let mut t = Terminal::new(5, 2, 100);
        t.feed("AAAAA\r\nBBBBB\r\nCCCCC\r\n");
        // Scrolling past the top is a no-op.
        assert!(t.scroll_up(10));
        assert!(!t.scroll_up(1));
        assert!(t.at_top());
        // Scrolling back past bottom is a no-op.
        assert!(t.scroll_down(10));
        assert!(!t.scroll_down(1));
        assert!(t.at_bottom());
    }

    #[test]
    fn scroll_is_noop_on_alt_screen() {
        let mut t = Terminal::new(5, 2, 100);
        t.feed("AAAAA\r\nBBBBB\r\nCCCCC\r\n"); // populate scrollback
        t.feed("\x1b[?1049h"); // enter alt screen
        assert!(!t.scroll_up(1));
        assert_eq!(t.view_offset(), 0);
    }

    #[test]
    fn reverse_index_scrolls_region_down_at_top_margin() {
        let mut t = Terminal::new(3, 3, 100);
        t.feed("AAA\r\nBBB\r\nCCC"); // rows AAA, BBB, CCC
        t.feed("\x1b[H"); // cursor home (top margin)
        t.feed("\x1bM"); // RI: content slides down, blank fills the top
        assert_eq!(t.row(0).iter().map(|c| c.ch).collect::<String>(), "   ");
        assert_eq!(t.row(1).iter().map(|c| c.ch).collect::<String>(), "AAA");
        assert_eq!(t.row(2).iter().map(|c| c.ch).collect::<String>(), "BBB");
    }

    #[test]
    fn reverse_index_moves_cursor_up_off_margin() {
        let mut t = Terminal::new(3, 3, 100);
        t.feed("AAA\r\nBBB\r\nCCC");
        t.feed("\x1b[3;1H"); // row 2
        t.feed("\x1bM"); // RI off the top margin: cursor up, no scroll
        assert_eq!(t.cursor().row, 1);
        assert_eq!(t.row(0).iter().map(|c| c.ch).collect::<String>(), "AAA");
    }

    #[test]
    fn index_scrolls_region_up_at_bottom_margin() {
        let mut t = Terminal::new(3, 3, 100);
        t.feed("\x1b[?1049h"); // alt screen: no scrollback, just shifts
        t.feed("AAA\r\nBBB\r\nCCC"); // cursor at bottom margin (row 2)
        t.feed("\x1bD"); // IND: content slides up, blank fills the bottom
        assert_eq!(t.row(0).iter().map(|c| c.ch).collect::<String>(), "BBB");
        assert_eq!(t.row(1).iter().map(|c| c.ch).collect::<String>(), "CCC");
        assert_eq!(t.row(2).iter().map(|c| c.ch).collect::<String>(), "   ");
    }

    #[test]
    fn next_line_returns_carriage_and_indexes() {
        let mut t = Terminal::new(5, 3, 100);
        t.feed("\x1b[2;3H"); // row 1, col 2
        t.feed("\x1bE"); // NEL: CR + index
        assert_eq!(t.cursor().row, 2);
        assert_eq!(t.cursor().col, 0);
    }

    #[test]
    fn reverse_index_at_top_is_captured_for_animation() {
        let mut t = Terminal::new(3, 3, 100);
        t.feed("\x1b[?1049h");
        t.feed("AAA\r\nBBB\r\nCCC");
        t.feed("\x1b[H\x1bM"); // home + RI: full-screen scroll down
        let s = t.take_alt_scroll().expect("RI scroll captured");
        assert_eq!((s.up, s.rows), (false, 1));
    }

    #[test]
    fn alternate_scroll_mode_defaults_on_and_tracks_1007() {
        let mut t = Terminal::new(5, 3, 100);
        assert!(t.alternate_scroll(), "?1007 defaults on (xterm alternateScroll)");
        t.feed("\x1b[?1007l");
        assert!(!t.alternate_scroll());
        t.feed("\x1b[?1007h");
        assert!(t.alternate_scroll());
        // RIS restores the default.
        t.feed("\x1b[?1007l\x1bc");
        assert!(t.alternate_scroll());
    }

    #[test]
    fn alt_scroll_up_captures_departing_rows() {
        let mut t = Terminal::new(3, 4, 100);
        t.feed("\x1b[?1049h");
        t.feed("AAA\r\nBBB\r\nCCC\r\nDDD"); // rows 0..3
        t.feed("\x1b[1S"); // SU 1: full-region scroll up
        let s = t.take_alt_scroll().expect("scroll captured");
        assert_eq!((s.up, s.rows), (true, 1));
        // Live grid shifted up; the departing top row hangs in the phantom
        // band just above the viewport so the slide shows real content.
        assert_eq!(t.extended_cell(0, 0).unwrap().ch, 'B');
        assert_eq!(t.extended_cell(-1, 0).unwrap().ch, 'A');
        // Releasing the animation drops the frozen row.
        t.clear_alt_anim();
        assert!(t.extended_cell(-1, 0).is_none());
    }

    #[test]
    fn alt_scroll_down_captures_below_viewport() {
        let mut t = Terminal::new(3, 4, 100);
        t.feed("\x1b[?1049h");
        t.feed("AAA\r\nBBB\r\nCCC\r\nDDD");
        t.feed("\x1b[1T"); // SD 1: full-region scroll down
        let s = t.take_alt_scroll().expect("scroll captured");
        assert_eq!((s.up, s.rows), (false, 1));
        // Departing bottom row sits just below the viewport (row index == rows).
        assert_eq!(t.extended_cell(4, 0).unwrap().ch, 'D');
        // Top of the grid is now the blank scrolled in from above.
        assert_eq!(t.extended_cell(0, 0).unwrap().ch, ' ');
    }

    #[test]
    fn alt_scroll_accumulates_net_linefeeds() {
        let mut t = Terminal::new(3, 4, 100);
        t.feed("\x1b[?1049h");
        t.feed("AAA\r\nBBB\r\nCCC\r\nDDD");
        t.feed("\r\n\r\n"); // two line-feeds at the bottom row → net up 2
        let s = t.take_alt_scroll().expect("scroll captured");
        assert_eq!((s.up, s.rows), (true, 2));
        assert_eq!(t.extended_cell(-2, 0).unwrap().ch, 'A');
        assert_eq!(t.extended_cell(-1, 0).unwrap().ch, 'B');
    }

    #[test]
    fn alt_scroll_take_is_one_shot() {
        let mut t = Terminal::new(3, 4, 100);
        t.feed("\x1b[?1049h");
        t.feed("AAA\r\nBBB\r\nCCC\r\nDDD\x1b[1S");
        assert!(t.take_alt_scroll().is_some());
        assert!(t.take_alt_scroll().is_none(), "second take is empty");
    }

    #[test]
    fn alt_scroll_less_backward_ri_is_captured() {
        // The exact byte sequence `less` emits to scroll backward one line on a
        // full screen (no DECSTBM): home + RI, repaint top line, repaint the
        // status line. RI at the top margin must register as a downward scroll.
        let mut t = Terminal::new(40, 10, 100);
        t.feed("\x1b[?1049h");
        for i in 1..=10 {
            t.feed(&format!("line {}\r\n", i));
        }
        // The front end drains the capture every feed; mirror that so the
        // fill's trailing scroll doesn't poison the window we care about.
        t.take_alt_scroll();
        // Now scroll backward, exactly as captured from less.
        t.feed("\r\x1b[K\x1b[H\x1bMline 2\x1b[m\r\n\x1b[10;1H\r\x1b[K:\x1b[K");
        let s = t.take_alt_scroll().expect("less backward RI captured");
        assert_eq!(s.up, false);
        assert_eq!(s.rows, 1);
        assert_eq!((s.region_top, s.region_bottom), (0, 9));
    }

    #[test]
    fn alt_scroll_insert_line_at_top_captured_as_down() {
        // vim scrolls back with `CSI L` (IL) at the top of a narrowed region —
        // not RI. IL at the region top is a downward scroll of [top, bottom].
        let mut t = Terminal::new(40, 12, 100);
        t.feed("\x1b[?1049h");
        for i in 1..=12 {
            t.feed(&format!("line {}\r\n", i));
        }
        t.take_alt_scroll(); // drain the fill's scroll
        t.feed("\x1b[1;11r\x1b[1;1H\x1b[L"); // region 1..11, home, insert line
        let s = t.take_alt_scroll().expect("IL captured as down-scroll");
        assert_eq!(s.up, false);
        assert_eq!(s.rows, 1);
        assert_eq!((s.region_top, s.region_bottom), (0, 10));
    }

    #[test]
    fn alt_scroll_delete_line_at_top_captured_as_up() {
        let mut t = Terminal::new(40, 12, 100);
        t.feed("\x1b[?1049h");
        for i in 1..=12 {
            t.feed(&format!("line {}\r\n", i));
        }
        t.take_alt_scroll();
        t.feed("\x1b[1;11r\x1b[1;1H\x1b[M"); // region 1..11, home, delete line
        let s = t.take_alt_scroll().expect("DL captured as up-scroll");
        assert_eq!(s.up, true);
        assert_eq!(s.rows, 1);
        assert_eq!((s.region_top, s.region_bottom), (0, 10));
    }

    #[test]
    fn alt_scroll_index_at_bottom_captured_as_up() {
        // IND (ESC D) at the bottom margin is a plain line-feed downward of the
        // cursor that scrolls the region up. On the alt screen this must be
        // captured as an UP scroll via the line-feed path, mirroring SU.
        let mut t = Terminal::new(3, 3, 100);
        t.feed("\x1b[?1049h");
        t.feed("AAA\r\nBBB\r\nCCC"); // cursor parked at the bottom margin (row 2)
        t.feed("\x1bD"); // IND at the bottom margin: region scrolls up
        let s = t.take_alt_scroll().expect("IND scroll captured");
        assert_eq!((s.up, s.rows), (true, 1));
        assert_eq!((s.region_top, s.region_bottom), (0, 2));
    }

    #[test]
    fn alt_scroll_insert_line_mid_screen_reports_cursor_region_top() {
        // IL captures the effective region [cursor.row, scroll_bottom]. With the
        // cursor mid-screen (row 2, not the region top) the capture's region_top
        // is 2, so the front end's `region_top == 0` gate would skip animating
        // it — but the capture itself still records the true region.
        let mut t = Terminal::new(40, 6, 100);
        t.feed("\x1b[?1049h");
        for i in 1..=6 {
            t.feed(&format!("line {}\r\n", i));
        }
        t.take_alt_scroll(); // drain the fill's scroll
        t.feed("\x1b[3;1H\x1b[L"); // cursor to row 2 (1-based 3), insert line
        let s = t.take_alt_scroll().expect("mid-screen IL captured");
        assert_eq!((s.up, s.rows), (false, 1));
        assert_eq!(s.region_top, 2);
        assert_eq!(s.region_bottom, 5);
    }

    #[test]
    fn alt_scroll_insert_lines_multi_reports_row_count() {
        // IL with n > 1 (`CSI 3L`) is a downward scroll of n rows.
        let mut t = Terminal::new(40, 12, 100);
        t.feed("\x1b[?1049h");
        for i in 1..=12 {
            t.feed(&format!("line {}\r\n", i));
        }
        t.take_alt_scroll(); // drain the fill's scroll
        t.feed("\x1b[1;11r\x1b[1;1H\x1b[3L"); // region 1..11, home, insert 3 lines
        let s = t.take_alt_scroll().expect("multi-line IL captured");
        assert_eq!((s.up, s.rows), (false, 3));
        assert_eq!((s.region_top, s.region_bottom), (0, 10));
    }

    #[test]
    fn next_line_at_bottom_margin_resets_column_and_scrolls() {
        // NEL (ESC E) at the bottom margin: the cursor is already at the bottom
        // row, so NEL returns the carriage (col -> 0) AND scrolls the region up.
        let mut t = Terminal::new(3, 3, 100);
        t.feed("\x1b[?1049h");
        t.feed("AAA\r\nBBB\r\nCC"); // cursor at row 2, col 2
        assert_eq!((t.cursor().row, t.cursor().col), (2, 2));
        t.feed("\x1bE"); // NEL at the bottom margin
        assert_eq!(t.cursor().col, 0, "carriage returned");
        assert_eq!(t.cursor().row, 2, "stays pinned to the bottom margin");
        // Content scrolled up: top row gone, blank filled at the bottom.
        assert_eq!(t.row(0).iter().map(|c| c.ch).collect::<String>(), "BBB");
        assert_eq!(t.row(1).iter().map(|c| c.ch).collect::<String>(), "CC ");
        assert_eq!(t.row(2).iter().map(|c| c.ch).collect::<String>(), "   ");
    }

    #[test]
    fn alt_scroll_ri_then_opposite_delete_line_is_poisoned() {
        // A direction flip inside one feed window breaks the single-shift model:
        // RI at the top margin scrolls the region down, then DL at the same row
        // scrolls it up. The mixed up/down poisons the capture, so nothing is
        // reported even though each op on its own would be.
        let mut t = Terminal::new(40, 6, 100);
        t.feed("\x1b[?1049h");
        for i in 1..=6 {
            t.feed(&format!("line {}\r\n", i));
        }
        t.take_alt_scroll(); // drain the fill's scroll
        // RI at home (down) followed by DL at home (up): opposite directions.
        t.feed("\x1b[H\x1bM\x1b[H\x1b[M");
        assert!(
            t.take_alt_scroll().is_none(),
            "opposite-direction ops in one window poison the capture"
        );
    }

    #[test]
    fn alt_scroll_reports_sub_region_bounds() {
        let mut t = Terminal::new(3, 5, 100);
        t.feed("\x1b[?1049h");
        // Scroll region rows 1..4 (0-based 0..3) — an app reserving the last
        // line. The capture records the region; the front end decides whether
        // it can animate it (only top-anchored regions; here top == 0).
        t.feed("\x1b[1;4r");
        t.feed("\x1b[H"); // home into the region
        t.feed("\x1b[1S"); // SU within the region
        let s = t.take_alt_scroll().expect("sub-region scroll captured");
        assert_eq!((s.up, s.rows), (true, 1));
        assert_eq!((s.region_top, s.region_bottom), (0, 3));
    }

    #[test]
    fn alt_scroll_mixed_direction_is_not_animated() {
        let mut t = Terminal::new(3, 4, 100);
        t.feed("\x1b[?1049h");
        t.feed("AAA\r\nBBB\r\nCCC\r\nDDD");
        t.feed("\x1b[1S\x1b[1T"); // up then down in one window
        assert!(t.take_alt_scroll().is_none(), "direction flip poisons");
    }

    #[test]
    fn alt_scroll_not_captured_on_primary_screen() {
        let mut t = Terminal::new(3, 2, 100);
        // Primary-screen scrolling rolls into real scrollback, not animation.
        t.feed("AAA\r\nBBB\r\nCCC\r\nDDD");
        assert!(t.take_alt_scroll().is_none());
    }

    #[test]
    fn alt_scroll_su_n_reports_rows_and_departing_rows() {
        // A single `CSI nS` with n>1 must report rows==n with the correct
        // departing edge: the top n rows of the pre-scroll frame.
        let mut t = Terminal::new(3, 4, 100);
        t.feed("\x1b[?1049h");
        t.feed("AAA\r\nBBB\r\nCCC\r\nDDD");
        t.feed("\x1b[2S"); // SU 2 in one escape
        let s = t.take_alt_scroll().expect("scroll captured");
        assert_eq!((s.up, s.rows), (true, 2));
        // Departing rows hang above the viewport: -2 = oldest top (A), -1 = B.
        assert_eq!(t.extended_cell(-2, 0).unwrap().ch, 'A');
        assert_eq!(t.extended_cell(-1, 0).unwrap().ch, 'B');
        // Live grid has shifted up by 2; row 0 is now C.
        assert_eq!(t.extended_cell(0, 0).unwrap().ch, 'C');
    }

    #[test]
    fn alt_scroll_net_capped_at_grid_height() {
        // Scrolling further than the grid is tall caps the reported distance
        // at `rows` (the most that can possibly be animated).
        let mut t = Terminal::new(3, 4, 100); // rows == 4
        t.feed("\x1b[?1049h");
        t.feed("AAA\r\nBBB\r\nCCC\r\nDDD");
        t.feed("\x1b[10S"); // SU 10 — far more than the 4-row grid
        let s = t.take_alt_scroll().expect("scroll captured");
        assert_eq!((s.up, s.rows), (true, 4), "net distance capped at rows");
    }

    #[test]
    fn alt_scroll_cleared_on_alt_screen_switch() {
        // Leaving the alt screen (?1049l) clears any captured scroll state.
        // Re-entering the alt screen without scrolling must yield None rather
        // than leaking the prior window's capture.
        let mut t = Terminal::new(3, 4, 100);
        t.feed("\x1b[?1049h");
        t.feed("AAA\r\nBBB\r\nCCC\r\nDDD");
        t.feed("\x1b[1S"); // capture an up-scroll...
        t.feed("\x1b[?1049l"); // ...then leave the alt screen before taking it.
        t.feed("\x1b[?1049h"); // back on the alt screen, no scroll this window.
        assert!(t.take_alt_scroll().is_none(), "switch clears capture");
    }

    #[test]
    fn alt_scroll_none_when_nothing_scrolled() {
        // Cursor moves and in-place overwrites on the alt screen don't scroll
        // the frame, so there's nothing to animate.
        let mut t = Terminal::new(3, 4, 100);
        t.feed("\x1b[?1049h");
        t.feed("AAA\r\nBBB\r\nCCC\r\nDDD");
        let _ = t.take_alt_scroll(); // drain the setup window.
        t.feed("\x1b[1;1H"); // home the cursor
        t.feed("XXX"); // overwrite row 0 in place — no scroll
        t.feed("\x1b[2;1HYYY"); // move to row 1 and overwrite — no scroll
        assert!(t.take_alt_scroll().is_none(), "no scroll → nothing to animate");
    }

    #[test]
    fn alt_scroll_clear_anim_is_idempotent_when_nothing_captured() {
        // clear_alt_anim must be safe to call when no animation rows are stashed,
        // and repeated calls stay a no-op.
        let mut t = Terminal::new(3, 4, 100);
        t.feed("\x1b[?1049h");
        t.feed("AAA\r\nBBB\r\nCCC\r\nDDD");
        // Nothing taken yet → no frozen rows.
        t.clear_alt_anim();
        t.clear_alt_anim();
        assert!(t.extended_cell(-1, 0).is_none());
        // And after a real capture, a double clear is still fine.
        t.feed("\x1b[1S");
        assert!(t.take_alt_scroll().is_some());
        t.clear_alt_anim();
        t.clear_alt_anim();
        assert!(t.extended_cell(-1, 0).is_none());
    }

    #[test]
    fn alt_scroll_cleared_on_resize() {
        // A resize retires any captured (but not-yet-taken) scroll window.
        let mut t = Terminal::new(3, 4, 100);
        t.feed("\x1b[?1049h");
        t.feed("AAA\r\nBBB\r\nCCC\r\nDDD");
        t.feed("\x1b[1S"); // capture, but don't take
        t.resize(5, 6);
        assert!(t.take_alt_scroll().is_none(), "resize clears capture");
    }

    #[test]
    fn alt_scroll_cleared_on_ris() {
        // RIS (\x1bc) resets capture state along with everything else.
        let mut t = Terminal::new(3, 4, 100);
        t.feed("\x1b[?1049h");
        t.feed("AAA\r\nBBB\r\nCCC\r\nDDD");
        t.feed("\x1b[1S\x1bc");
        assert!(t.take_alt_scroll().is_none(), "RIS clears capture");
    }

    #[test]
    fn primary_scroll_counts_lines_pushed_into_scrollback() {
        // A 2-row grid: the first two lines fill rows 0 and 1, then each
        // further LF rolls one line into scrollback. Four lines → 2 scrolls.
        let mut t = Terminal::new(3, 2, 100);
        t.feed("AAA\r\nBBB\r\nCCC\r\nDDD");
        assert_eq!(t.take_primary_scroll(), 2, "two rows rolled into scrollback");
    }

    #[test]
    fn primary_scroll_take_is_one_shot() {
        let mut t = Terminal::new(3, 2, 100);
        t.feed("AAA\r\nBBB\r\nCCC");
        assert!(t.take_primary_scroll() > 0);
        assert_eq!(t.take_primary_scroll(), 0, "second take is drained");
    }

    #[test]
    fn primary_scroll_accumulates_across_feeds_until_taken() {
        // The counter sums across feeds within one animation window: two feeds
        // of one scroll each report 2 if not drained between them.
        let mut t = Terminal::new(3, 2, 100);
        t.feed("AAA\r\nBBB"); // fills both rows, no scroll yet
        t.feed("\r\nCCC"); // scroll 1
        t.feed("\r\nDDD"); // scroll 1 more
        assert_eq!(t.take_primary_scroll(), 2, "two scrolls accumulate");
    }

    #[test]
    fn primary_scroll_capped_at_grid_height() {
        // A burst far taller than the grid caps at `rows` — the rows beyond a
        // full screen have already scrolled past anything a slide could show.
        let mut t = Terminal::new(3, 2, 100); // rows == 2
        t.feed("A\r\nB\r\nC\r\nD\r\nE\r\nF\r\nG");
        assert_eq!(t.take_primary_scroll(), 2, "net distance capped at rows");
    }

    #[test]
    fn primary_scroll_not_captured_on_alt_screen() {
        // Alt-screen scrolls go through the alt animation path, not this one.
        let mut t = Terminal::new(3, 2, 100);
        t.feed("\x1b[?1049h");
        t.feed("AAA\r\nBBB\r\nCCC\r\nDDD");
        assert_eq!(t.take_primary_scroll(), 0, "alt screen uses take_alt_scroll");
    }

    #[test]
    fn primary_scroll_not_captured_without_scrollback() {
        // With scrollback disabled, lines don't roll into history, so there's
        // nothing to slide in from above.
        let mut t = Terminal::new(3, 2, 0); // scrollback_limit == 0
        t.feed("AAA\r\nBBB\r\nCCC\r\nDDD");
        assert_eq!(t.take_primary_scroll(), 0, "no scrollback → no slide");
    }

    #[test]
    fn primary_scroll_not_captured_for_partial_decstbm_region() {
        // A DECSTBM scroll region narrower than the full grid shifts rows in
        // place — nothing rolls into scrollback — so it must not register a
        // slide. Grid is 4 rows; set the region to rows 2..3 (1-based 2;3),
        // park the cursor at the bottom margin, then feed enough LFs to scroll
        // the region several times.
        let mut t = Terminal::new(4, 4, 100);
        t.feed("\x1b[2;3r"); // DECSTBM: top margin row 2, bottom row 3 (partial)
        t.feed("\x1b[3;1H"); // move cursor to the bottom margin (row 3)
        t.feed("X\nY\nZ\nW"); // LFs at the bottom margin scroll the region
        assert_eq!(
            t.take_primary_scroll(),
            0,
            "partial DECSTBM region shifts in place, no scrollback push"
        );
        // And the grid above/below the region is untouched scrollback-wise.
        assert_eq!(t.scrollback_len(), 0, "partial region must not grow scrollback");
    }

    #[test]
    fn primary_scroll_not_captured_on_reverse_index() {
        // RI (`ESC M`) at the top margin scrolls the region *down* via
        // scroll_region_down_by, which never pushes to scrollback and so must
        // never register a scroll-on-output slide.
        let mut t = Terminal::new(3, 2, 100);
        t.feed("\x1b[H"); // cursor home — at the top margin
        t.feed("\x1bM\x1bM\x1bM"); // three reverse indexes
        assert_eq!(
            t.take_primary_scroll(),
            0,
            "reverse index scrolls down, not into scrollback"
        );
    }

    #[test]
    fn primary_scroll_resize_shrink_spill_does_not_count() {
        // A vertical-shrink resize spills top rows into scrollback through its
        // own path (not scroll_region_up_by), so it must not register a slide:
        // a window resize is not output streaming in and should never trigger
        // the scroll-on-output animation.
        let mut t = Terminal::new(4, 4, 100);
        t.feed("AAA\r\nBBB\r\nCCC\r\nDDD"); // fill the grid, cursor on last row
        assert_eq!(t.take_primary_scroll(), 0, "filling without overflow: no scroll");
        t.resize(4, 2); // shrink height → spills top rows into scrollback
        assert!(t.scrollback_len() > 0, "shrink should have spilled rows");
        assert_eq!(
            t.take_primary_scroll(),
            0,
            "resize spill is not output streaming — must not animate"
        );
    }

    #[test]
    fn primary_scroll_resize_does_not_clear_pending_count() {
        // resize retires the alt-scroll animation state, but the primary
        // scroll-on-output count is drained per-feed by the front end, so a
        // resize between accumulation and drain leaves the pending count
        // intact (documents current behavior; mirrors that resize doesn't
        // touch primary_scroll_net).
        let mut t = Terminal::new(3, 2, 100);
        t.feed("AAA\r\nBBB\r\nCCC"); // one scroll into scrollback, undrained
        t.resize(3, 3); // grow — does not push primary rows into scrollback
        assert_eq!(
            t.take_primary_scroll(),
            1,
            "pending count survives an intervening resize"
        );
    }

    #[test]
    fn primary_scroll_resumes_after_returning_from_alt_screen() {
        // Entering and leaving the alt screen must not leak alt scrolls into
        // the primary count, and primary scrolls after returning still count.
        let mut t = Terminal::new(3, 2, 100);
        t.feed("\x1b[?1049h"); // enter alt screen
        t.feed("AAA\r\nBBB\r\nCCC\r\nDDD"); // scrolls on the alt screen
        t.feed("\x1b[?1049l"); // leave alt screen, back to primary
        assert_eq!(
            t.take_primary_scroll(),
            0,
            "alt-screen scrolls never touch the primary count"
        );
        t.feed("EEE\r\nFFF\r\nGGG"); // one scroll on the primary screen
        assert_eq!(
            t.take_primary_scroll(),
            1,
            "primary scroll after returning is counted"
        );
    }

    #[test]
    fn primary_scroll_full_region_after_decstbm_reset() {
        // Once DECSTBM is reset to the full grid, scrolls push into scrollback
        // again and the slide resumes — confirms the partial-region exclusion
        // is keyed on the *current* region, not a sticky flag.
        let mut t = Terminal::new(4, 4, 100);
        t.feed("\x1b[2;3r"); // partial region
        t.feed("\x1b[3;1H");
        t.feed("X\nY\nZ"); // scrolls within the partial region (not counted)
        assert_eq!(t.take_primary_scroll(), 0, "partial region not counted");
        t.feed("\x1b[r"); // DECSTBM reset → full grid is the region again
        t.feed("\x1b[4;1H"); // cursor to the bottom row
        t.feed("\nP\nQ\nR"); // three LFs at the bottom margin → three scrolls
        assert_eq!(
            t.take_primary_scroll(),
            3,
            "full-region scrolls counted again after DECSTBM reset"
        );
    }

    #[test]
    fn primary_scroll_cleared_on_ris() {
        // RIS clears the screen + scrollback, so any pending slide distance
        // must be dropped — its departing rows no longer exist.
        let mut t = Terminal::new(3, 2, 100);
        t.feed("AAA\r\nBBB\r\nCCC\x1bc");
        assert_eq!(t.take_primary_scroll(), 0, "RIS clears the pending slide");
    }

    #[test]
    fn cursor_visual_row_hidden_when_scrolled_off() {
        let mut t = Terminal::new(5, 3, 100);
        // Push 4 lines into scrollback while parking the cursor on the
        // last row of the live grid (row=2).
        t.feed("AAAAA\r\nBBBBB\r\nCCCCC\r\nDDDDD\r\nEEEEE\r\nFFFFF\r\n");
        assert_eq!(t.cursor_visual_row(), Some(2));
        // 1 line back: visual=3 (== rows). Still drawn at the bottom edge.
        t.scroll_up(1);
        assert_eq!(t.cursor_visual_row(), Some(3));
        // 2 lines back: visual=4 (rows + 1). The renderer's phantom band
        // covers this so the cursor can still intersect the window at the
        // bottom edge — keep drawing.
        t.scroll_up(1);
        assert_eq!(t.cursor_visual_row(), Some(4));
        // 3 lines back: visual=5 (rows + 2). Beyond the phantom band; hide.
        t.scroll_up(1);
        assert_eq!(t.cursor_visual_row(), None);
        t.scroll_to_bottom();
        assert_eq!(t.cursor_visual_row(), Some(2));
    }

    #[test]
    fn extended_cell_matches_visible_inside_viewport() {
        let mut t = Terminal::new(5, 2, 100);
        t.feed("AAAAA\r\nBBBBB\r\nCCCCC\r\n");
        t.scroll_up(1);
        // Within [0, rows), extended_cell must agree with visible_cell.
        for r in 0..t.rows {
            for c in 0..t.cols {
                assert_eq!(
                    t.extended_cell(r as isize, c).map(|x| x.ch),
                    Some(t.visible_cell(r, c).ch),
                );
            }
        }
    }

    #[test]
    fn extended_cell_above_none_when_no_scrollback() {
        let mut t = Terminal::new(5, 3, 100);
        t.feed("AAAAA");
        assert_eq!(t.scrollback_len(), 0);
        assert!(t.extended_cell(-1, 0).is_none());
        assert!(t.extended_cell(-2, 0).is_none());
    }

    #[test]
    fn extended_cell_above_none_when_at_top() {
        let mut t = Terminal::new(5, 2, 100);
        t.feed("AAAAA\r\nBBBBB\r\nCCCCC\r\n");
        assert_eq!(t.scrollback_len(), 2);
        assert!(t.scroll_up(t.scrollback_len()));
        assert!(t.at_top());
        assert!(t.extended_cell(-1, 0).is_none());
        assert!(t.extended_cell(-2, 0).is_none());
    }

    #[test]
    fn extended_cell_above_returns_two_rows_of_history() {
        let mut t = Terminal::new(5, 2, 100);
        // scrollback = [AAAAA (oldest), BBBBB, CCCCC]
        t.feed("AAAAA\r\nBBBBB\r\nCCCCC\r\nDDDDD\r\n");
        assert_eq!(t.scrollback_len(), 3);
        // Scroll up by 1: visual row 0 is CCCCC; -1 = BBBBB; -2 = AAAAA.
        assert!(t.scroll_up(1));
        assert_eq!(t.visible_cell(0, 0).ch, 'C');
        assert_eq!(t.extended_cell(-1, 0).map(|c| c.ch), Some('B'));
        assert_eq!(t.extended_cell(-2, 0).map(|c| c.ch), Some('A'));
        // Only 3 lines exist; nothing further back.
        assert!(t.extended_cell(-3, 0).is_none());
    }

    #[test]
    fn extended_cell_below_none_when_at_bottom() {
        let mut t = Terminal::new(5, 2, 100);
        t.feed("AAAAA\r\nBBBBB\r\nCCCCC\r\n");
        assert!(t.at_bottom());
        assert!(t.extended_cell(t.rows as isize, 0).is_none());
        assert!(t.extended_cell(t.rows as isize + 1, 0).is_none());
    }

    #[test]
    fn extended_cell_below_returns_two_rows_of_live_grid() {
        let mut t = Terminal::new(5, 2, 100);
        // scrollback = [AAAAA, BBBBB]; primary rows = [CCCCC, DDDDD]
        t.feed("AAAAA\r\nBBBBB\r\nCCCCC\r\nDDDDD");
        assert_eq!(t.scrollback_len(), 2);
        // Scroll up 2: visible rows are [AAAAA, BBBBB]; below them are CCCCC then DDDDD.
        assert!(t.scroll_up(2));
        assert_eq!(t.visible_cell(0, 0).ch, 'A');
        assert_eq!(t.visible_cell(1, 0).ch, 'B');
        assert_eq!(t.extended_cell(2, 0).map(|c| c.ch), Some('C'));
        assert_eq!(t.extended_cell(3, 0).map(|c| c.ch), Some('D'));
    }

    #[test]
    fn extended_cell_alt_screen_only_returns_in_bounds() {
        let mut t = Terminal::new(5, 2, 100);
        t.feed("AAAAA\r\nBBBBB\r\nCCCCC\r\n");
        t.feed("\x1b[?1049h");
        // Phantom rows are suppressed on the alt screen.
        assert!(t.extended_cell(-1, 0).is_none());
        assert!(t.extended_cell(-2, 0).is_none());
        assert!(t.extended_cell(t.rows as isize, 0).is_none());
        assert!(t.extended_cell(t.rows as isize + 1, 0).is_none());
        // In-bounds rows still resolve (to the alt grid).
        assert!(t.extended_cell(0, 0).is_some());
        assert!(t.extended_cell((t.rows - 1) as isize, 0).is_some());
    }

    //
    // Image-placement tests (slice 2).
    //

    #[test]
    fn insert_placement_lands_on_active_grid_with_unique_ids() {
        let mut t = Terminal::new(20, 10, 100);
        let a = place(&mut t, 7, 2, 3, 4, 5);
        let b = place(&mut t, 8, 0, 0, 2, 2);
        assert_ne!(a, b);
        let anchors = live_anchors(&t);
        assert_eq!(anchors.len(), 2);
        assert!(anchors.contains(&(7, 2, 3, 4, 5)));
        assert!(anchors.contains(&(8, 0, 0, 2, 2)));
    }

    #[test]
    fn insert_placement_targets_alternate_when_active() {
        let mut t = Terminal::new(20, 10, 100);
        t.feed("\x1b[?1049h"); // switch to alt
        place(&mut t, 1, 0, 0, 2, 2);
        assert_eq!(t.live_placements().len(), 1);
        t.feed("\x1b[?1049l"); // back to primary
        assert!(t.live_placements().is_empty());
        t.feed("\x1b[?1049h");
        // Re-entering alt clears the alt buffer (matches existing alt-cell
        // behaviour); the placement we made here is gone.
        assert!(t.live_placements().is_empty());
    }

    #[test]
    fn grid_clear_drops_placements() {
        let mut t = Terminal::new(20, 10, 100);
        place(&mut t, 1, 0, 0, 2, 2);
        place(&mut t, 2, 5, 5, 3, 3);
        t.feed("\x1b[2J"); // ED 2 — erase whole screen
        assert!(t.live_placements().is_empty());
    }

    #[test]
    fn cell_erase_within_row_does_not_touch_placements() {
        // EL (erase-in-line) and partial-row clears must not delete images.
        // Kitty / iTerm both keep placements alive across cell-erase ops.
        let mut t = Terminal::new(20, 10, 100);
        place(&mut t, 1, 3, 0, 2, 4);
        t.feed("\x1b[3;1H"); // CUP row 3 col 1 (1-based)
        t.feed("\x1b[2K"); // erase entire line
        assert_eq!(t.live_placements().len(), 1);
    }

    #[test]
    fn ed3_clears_scrollback_placements() {
        let mut t = Terminal::new(20, 5, 100);
        place(&mut t, 1, 0, 0, 2, 2);
        // SU 3 — unconditional scroll-up, doesn't depend on cursor row.
        t.feed("\x1b[3S");
        assert!(t.live_placements().is_empty());
        assert!(!t.scrollback_placements_for_test().is_empty());
        t.feed("\x1b[3J");
        assert!(t.scrollback_placements_for_test().is_empty());
    }

    #[test]
    fn full_reset_clears_all_placements_and_id_state() {
        let mut t = Terminal::new(20, 5, 100);
        let _a = place(&mut t, 1, 0, 0, 1, 1);
        let _b = place(&mut t, 2, 2, 2, 1, 1);
        // Force a scrollback placement too.
        place(&mut t, 3, 0, 0, 1, 1);
        t.feed("\x1b[2S");
        t.feed("\x1bc"); // RIS — full reset
        assert!(t.live_placements().is_empty());
        assert!(t.scrollback_placements_for_test().is_empty());
        // First placement after reset gets id 1 again.
        let id = place(&mut t, 9, 0, 0, 1, 1);
        assert_eq!(id, 1);
    }

    #[test]
    fn scroll_region_up_shifts_intersecting_placements_only() {
        let mut t = Terminal::new(20, 10, 100);
        // Above scroll region — stays put.
        let above_id = place(&mut t, 1, 0, 0, 1, 2);
        // In region — shifts up.
        let in_id = place(&mut t, 2, 5, 0, 2, 2);
        // Below region — stays put.
        let below_id = place(&mut t, 3, 9, 0, 1, 2);
        // Set scroll region to rows 4..=8 (1-based 5..=9) and feed nothing —
        // call the internal scroll directly via a region scroll-up sequence.
        // CSI 5;9 r sets the region; then SU 2 scrolls within it.
        t.feed("\x1b[5;9r\x1b[2S");
        let by_id: std::collections::HashMap<u32, isize> = t
            .live_placements()
            .iter()
            .map(|p| (p.id, p.top_row))
            .collect();
        assert_eq!(by_id[&above_id], 0);
        assert_eq!(by_id[&in_id], 3); // 5 - 2
        assert_eq!(by_id[&below_id], 9);
    }

    #[test]
    fn scroll_region_up_full_screen_evicts_to_scrollback() {
        let mut t = Terminal::new(20, 5, 100);
        let id = place(&mut t, 1, 0, 0, 2, 2);
        // Scroll the whole grid up by 2 — the placement (rows 0,1) fully
        // exits the top. Should land in scrollback at index 0 (oldest).
        t.feed("\x1b[2S");
        assert!(t.live_placements().is_empty());
        let sb = t.scrollback_placements_for_test();
        assert_eq!(sb.len(), 1);
        assert_eq!(sb[0].0, 0); // anchor at oldest scrollback row
        assert_eq!(sb[0].1.id, id);
    }

    // ---- scrollback_placements_in_view: viewport-row mapping ----
    //
    // The renderer uses this to draw images that have scrolled into history
    // when the user scrolls back. The mapping puts scrollback row sb_r at
    // viewport row sb_r - (sb_len - view_off). Tests below pin that down
    // for the three interesting positions plus alt-screen / no-offset
    // short-circuits.

    #[test]
    fn scrollback_in_view_empty_when_not_scrolled() {
        // view_offset == 0: even with promoted placements, nothing renders
        // through this path — the live grid (now empty of them) is what
        // the user sees.
        let mut t = Terminal::new(20, 5, 100);
        place(&mut t, 1, 0, 0, 2, 2);
        t.feed("\x1b[2S");
        assert_eq!(t.view_offset(), 0);
        assert!(t.scrollback_placements_in_view(t.rows).is_empty());
    }

    #[test]
    fn scrollback_in_view_empty_on_alt_screen() {
        // Alt screen has no scrollback. Even if entries existed (they
        // don't — alt promotions are blocked upstream), this accessor
        // refuses to surface them.
        let mut t = Terminal::new(20, 5, 100);
        place(&mut t, 1, 0, 0, 2, 2);
        t.feed("\x1b[2S");
        t.feed("\x1b[?1049h"); // enter alt screen
        assert!(t.on_alt_screen());
        assert!(t.scrollback_placements_in_view(t.rows).is_empty());
    }

    #[test]
    fn scrollback_in_view_maps_anchor_to_viewport_row() {
        // Image lands at oldest scrollback row (0). Scroll back by the
        // full scrollback length and that row sits at the top of the
        // viewport (viewport_row == 0).
        let mut t = Terminal::new(20, 5, 100);
        let id = place(&mut t, 1, 0, 0, 2, 2);
        t.feed("\x1b[2S"); // promote to scrollback, sb_len now 2
        assert_eq!(t.scrollback_len(), 2);
        assert!(t.scroll_up(2));
        let in_view = t.scrollback_placements_in_view(t.rows);
        assert_eq!(in_view.len(), 1);
        assert_eq!(in_view[0].image.0, id);
        assert_eq!(in_view[0].top_row, 0);
        assert_eq!(in_view[0].rows, 2);
    }

    #[test]
    fn scrollback_in_view_filters_two_distinct_rows() {
        // Place image 1, scroll it into history, push enough more
        // scrollback that image 1 is well above the 2-row smooth-scroll
        // slack window; then place image 2 and scroll just one row back.
        // image 1 is filtered (far above the slack), image 2 is at
        // viewport row 0.
        let mut t = Terminal::new(20, 5, 100);
        place(&mut t, 1, 0, 0, 1, 1);
        t.feed("\x1b[1S"); // image 1 → scrollback row 0
        t.feed("\x1b[5S"); // push image 1 further above the slack window
        place(&mut t, 2, 0, 0, 1, 1);
        t.feed("\x1b[1S"); // image 2 → newest scrollback row
        let sb_len = t.scrollback_len();
        assert!(sb_len >= 7);
        assert!(t.scroll_up(1));
        let in_view = t.scrollback_placements_in_view(t.rows);
        assert_eq!(in_view.len(), 1);
        assert_eq!(in_view[0].image.0, 2);
        assert_eq!(in_view[0].top_row, 0);
    }

    #[test]
    fn scrollback_in_view_keeps_placement_within_top_slack() {
        // Placement whose discrete viewport position is 1 row above the
        // top of the viewport — fully off-screen by integer math, but
        // smooth-scroll can move it down by up to one line_height before
        // the next view_offset tick. Filter must keep it in the slack
        // window so the image fades in smoothly from the top edge instead
        // of snapping into view when view_offset increments.
        let mut t = Terminal::new(20, 5, 100);
        place(&mut t, 1, 0, 0, 1, 1);
        t.feed("\x1b[1S"); // sb_row=0
        t.feed("\x1b[1S"); // push image to sb shift territory: sb_len=2
        assert_eq!(t.scrollback_len(), 2);
        assert!(t.scroll_up(1));
        // sb_len=2, view_off=1 → shift=-1; image at sb_row=0 → top=-1,
        // bottom=0. Old filter (bottom <= 0) excluded; new slack keeps it.
        let in_view = t.scrollback_placements_in_view(t.rows);
        assert_eq!(in_view.len(), 1);
        assert_eq!(in_view[0].top_row, -1);
    }

    #[test]
    fn scrollback_in_view_keeps_placement_within_bottom_slack() {
        // Mirror image of the above: placement just past the bottom of
        // the viewport stays in the slack window so smooth-scroll up can
        // reveal its top edge.
        let mut t = Terminal::new(20, 5, 100);
        place(&mut t, 1, 0, 0, 1, 1);
        t.feed("\x1b[1S"); // sb_row=0
        // Build scrollback so the image lands one row below viewport
        // after scrolling all the way back. With viewport_rows=5 and slack=2,
        // a top_row of 5 or 6 must still be returned.
        for _ in 0..5 {
            t.feed("\n");
        }
        let sb_len = t.scrollback_len();
        assert!(t.scroll_up(sb_len));
        let in_view = t.scrollback_placements_in_view(t.rows);
        // sb_row=0, shift=0 → top=0; with slack we expect it kept.
        assert_eq!(in_view.len(), 1);
        assert_eq!(in_view[0].top_row, 0);
    }

    #[test]
    fn scrollback_in_view_straddles_live_boundary() {
        // 2-row image promoted, then scroll back by 1. Its anchor row is
        // one row above the viewport top but bottom_row=1 still spills
        // into the visible area. The renderer relies on this — it draws
        // the whole image, the camera ortho clips above row 0.
        let mut t = Terminal::new(20, 5, 100);
        place(&mut t, 1, 0, 0, 2, 2);
        t.feed("\x1b[2S");
        assert!(t.scroll_up(1));
        let in_view = t.scrollback_placements_in_view(t.rows);
        assert_eq!(in_view.len(), 1);
        // sb_len=2, view_off=1 → shift=-1; sb_row=0 → viewport_row=-1
        assert_eq!(in_view[0].top_row, -1);
        assert_eq!(in_view[0].rows, 2);
    }

    #[test]
    fn scrollback_in_view_filters_above_top() {
        // Scroll back by 1 only, but image was promoted many rows ago.
        // Anchor + height land entirely above viewport row 0 → filtered.
        let mut t = Terminal::new(20, 5, 100);
        place(&mut t, 1, 0, 0, 1, 1);
        t.feed("\x1b[1S"); // promotes (scrollback_row = 0)
        // Push more scrollback so view_offset=1 leaves the placement above.
        // SU evicts the top row of the live grid into scrollback.
        t.feed("\x1b[5S");
        assert!(t.scrollback_len() >= 6);
        assert!(t.scroll_up(1));
        // sb_len>=6, view_off=1 → shift<=-5; sb_row=0 → top_row<=-5;
        // bottom_row = top_row + 1 <= -4 → filtered.
        assert!(t.scrollback_placements_in_view(t.rows).is_empty());
    }

    #[test]
    fn scrollback_in_view_filters_below_grid() {
        // To make an image filtered when scrolled fully back, it has to
        // sit at a scrollback row past `viewport_rows + ROW_SLACK`. Once
        // promoted the image's scrollback_row is fixed, so we have to
        // build up lots of scrollback BEFORE placing it.
        let mut t = Terminal::new(20, 5, 100);
        // 15 newlines from row 0 scroll the bottom 11 times, putting
        // 11 rows in scrollback.
        for _ in 0..15 {
            t.feed("\n");
        }
        place(&mut t, 1, 0, 0, 1, 1); // image at grid row 0
        t.feed("\x1b[1S"); // promotes; image sits at scrollback_row ~11
        let sb_len = t.scrollback_len();
        assert!(sb_len > t.rows + 2);
        assert!(t.scroll_up(sb_len));
        // view_off == sb_len → shift = 0. Image's scrollback_row is past
        // viewport_rows + slack (5 + 2 = 7), so it's off the bottom and
        // filtered.
        assert!(t.scrollback_placements_in_view(t.rows).is_empty());
    }

    #[test]
    fn scrollback_in_view_walks_back_into_view() {
        // End-to-end: place + scroll into scrollback + walk back via
        // scroll_up, then assert the placement reappears in the in-view
        // list. Mirrors the user flow `kitty +kitten icat ; wheel up`.
        let mut t = Terminal::new(20, 5, 100);
        let id = place(&mut t, 1, 0, 0, 2, 2);
        t.feed("\x1b[3S"); // image is gone from live; now in scrollback
        assert!(t.live_placements().is_empty());
        assert!(t.scrollback_placements_in_view(t.rows).is_empty()); // no scroll yet
        assert!(t.scroll_up(3));
        let in_view = t.scrollback_placements_in_view(t.rows);
        assert_eq!(in_view.len(), 1);
        assert_eq!(in_view[0].image.0, id);
    }

    #[test]
    fn scroll_region_up_straddling_image_stays_with_negative_top() {
        let mut t = Terminal::new(20, 5, 100);
        place(&mut t, 1, 0, 0, 3, 2);
        // Scroll by 1 — image now straddles top of viewport.
        t.feed("\x1b[1S");
        let anchors = live_anchors(&t);
        assert_eq!(anchors.len(), 1);
        assert_eq!(anchors[0].1, -1);
        assert!(t.scrollback_placements_for_test().is_empty());
        // Scroll by 2 more — bottom_row was 2, becomes 0 — fully off.
        t.feed("\x1b[2S");
        assert!(t.live_placements().is_empty());
        let sb = t.scrollback_placements_for_test();
        assert_eq!(sb.len(), 1);
        // First scroll pushed 1 row to scrollback; second scroll pushed 2
        // more. Image was anchored at the very first pushed row.
        assert_eq!(sb[0].0, 0);
    }

    #[test]
    fn scroll_region_down_drops_off_bottom() {
        let mut t = Terminal::new(20, 5, 100);
        place(&mut t, 1, 3, 0, 2, 2); // covers rows 3,4 in a 5-row grid
        // CSI T scrolls the region down by 1; placement shifts to top_row=4
        // (still partially visible — bottom_row=6, top_row=4 < 5 rows).
        t.feed("\x1b[1T");
        assert_eq!(t.live_placements().len(), 1);
        assert_eq!(t.live_placements()[0].top_row, 4);
        // One more — top_row=5 hits fully_off_grid's top_row >= rows test.
        t.feed("\x1b[1T");
        assert!(t.live_placements().is_empty());
    }

    #[test]
    fn scroll_partial_width_region_leaves_placements_alone() {
        let mut t = Terminal::new(20, 10, 100);
        // Enable DECLRMM and set a partial-width margin, then scroll.
        t.feed("\x1b[?69h\x1b[5;15s\x1b[1;10r");
        place(&mut t, 1, 2, 7, 2, 4);
        t.feed("\x1b[2S");
        // Placement unchanged — partial-width scroll skips placements.
        assert_eq!(t.live_placements()[0].top_row, 2);
    }

    #[test]
    fn resize_vertical_shrink_spills_placement_to_scrollback() {
        let mut t = Terminal::new(20, 10, 100);
        place(&mut t, 1, 2, 0, 2, 4); // anchored in rows 2..4
        // Cursor on the last row: content reaches the bottom, so the shrink
        // spills the full height delta (the bottom-anchored case).
        t.feed("\x1b[10;1H");
        // Shrink rows 10 → 6. spill = 4 (rows 0..4). Placement at row 2 is
        // anchored in a spilled row, so it should land in scrollback.
        t.resize(20, 6);
        assert!(t.live_placements().is_empty());
        let sb = t.scrollback_placements_for_test();
        assert_eq!(sb.len(), 1);
        // Spilled 4 rows; placement's row 2 is the 3rd-from-oldest of the
        // spilled batch (sb_row = 0 + 2 = 2).
        assert_eq!(sb[0].0, 2);
    }

    #[test]
    fn resize_vertical_shrink_keeps_low_placements_live() {
        let mut t = Terminal::new(20, 10, 100);
        place(&mut t, 1, 7, 0, 2, 4); // rows 7..9
        // Cursor on the last row so the shrink spills the full height delta.
        t.feed("\x1b[10;1H");
        // Shrink 10 → 6, spill = 4. Placement at row 7 stays live; shifts up
        // by 4 to row 3.
        t.resize(20, 6);
        assert_eq!(t.live_placements().len(), 1);
        assert_eq!(t.live_placements()[0].top_row, 3);
    }

    #[test]
    fn resize_vertical_grow_promotes_scrollback_placement_back() {
        let mut t = Terminal::new(20, 6, 100);
        place(&mut t, 1, 0, 0, 2, 4);
        // Cursor on the last row so the shrink spills the full height delta
        // and the top-anchored placement is pushed into scrollback.
        t.feed("\x1b[6;1H");
        // Force a single-row scroll so placement straddles, then another to
        // fully evict — but actually simplest: resize-shrink to push it into
        // scrollback, then resize-grow to pull it back.
        t.resize(20, 3); // spill=3, placement (row 0..1) → sb at row 0
        assert_eq!(t.scrollback_placements_for_test().len(), 1);
        // Now grow back. refill = up to scrollback length. The placement at
        // sb row 0 should not be the FIRST refilled, because refill drains
        // the TAIL of scrollback (most recent), not the head.
        t.resize(20, 6);
        // Scrollback had 3 entries (sb rows 0,1,2). refill drains the most
        // recent (rows 1,2 — there are 3 rows but new_rows-old_rows = 3, so
        // all 3 drain). sb_post = 0. Placement at sb_row=0 is in the
        // drained range (>= sb_post=0), so it promotes. Its new top_row =
        // 0 - 0 = 0.
        assert_eq!(t.scrollback_placements_for_test().len(), 0);
        assert_eq!(t.live_placements().len(), 1);
        assert_eq!(t.live_placements()[0].top_row, 0);
    }

    #[test]
    fn resize_horizontal_shrink_preserves_off_screen_placements() {
        // Regression: a placement whose left_col is now past the grid's
        // right edge MUST survive the shrink. Horizontal off-screen is
        // recoverable — the user can widen the window and the
        // placement comes back into view. Dropping on shrink (the old
        // behavior) made images vanish permanently on any resize that
        // briefly hid them.
        let mut t = Terminal::new(20, 5, 100);
        place(&mut t, 1, 0, 2, 1, 4); // cols 2..6
        place(&mut t, 2, 0, 15, 1, 2); // cols 15..17
        t.resize(10, 5); // new cols = 10
        let images: Vec<u32> = t.live_placements().iter().map(|p| p.image.0).collect();
        assert_eq!(
            images,
            vec![1, 2],
            "both placements survive the shrink; the renderer clips off-screen draws",
        );
        // Grow back — the off-screen placement is still visible at its
        // original left_col.
        t.resize(20, 5);
        let images: Vec<u32> = t.live_placements().iter().map(|p| p.image.0).collect();
        assert_eq!(images, vec![1, 2]);
        assert_eq!(
            t.live_placements()[1].left_col,
            15,
            "left_col preserved across shrink + grow round-trip",
        );
    }

    #[test]
    fn referenced_image_ids_unions_primary_alt_and_scrollback() {
        use std::collections::HashSet;
        let mut t = Terminal::new(10, 5, 100);
        // Primary placements with two distinct images.
        place(&mut t, 10, 0, 0, 1, 2);
        place(&mut t, 20, 0, 0, 1, 2);
        // Scroll one out so it lands in scrollback.
        t.feed("\x1b[1S");
        // Alt screen placement with a third image.
        t.feed("\x1b[?1049h");
        place(&mut t, 30, 0, 0, 1, 1);
        t.feed("\x1b[?1049l");
        // Back on primary: dedupe re-includes 10 and 20 (still live or sb),
        // plus 30 from alt grid.
        let ids: HashSet<u32> = t.referenced_image_ids().iter().map(|i| i.0).collect();
        assert!(ids.contains(&10));
        assert!(ids.contains(&20));
        assert!(ids.contains(&30));
    }

    //
    // OSC 133 semantic prompt marks.
    //

    #[test]
    fn osc_133_full_command_cycle_builds_one_region() {
        let mut t = Terminal::new(80, 24, 100);
        // Prompt drawn on row 0, command typed, output on rows 1..2.
        t.feed("\x1b]133;A\x07");
        t.feed("$ \x1b]133;B\x07");
        t.feed("ls\r\n\x1b]133;C\x07");
        t.feed("file.txt\r\n\x1b]133;D;0\x07");
        let regions = t.command_regions();
        assert_eq!(regions.len(), 1);
        let r = regions[0];
        assert_eq!(r.prompt_start, 0);
        assert_eq!(r.input_start, Some(0));
        assert_eq!(r.output_start, Some(1));
        assert_eq!(r.command_end, Some(2));
        assert_eq!(r.exit_code, Some(0));
    }

    #[test]
    fn osc_133_nonzero_exit_code_captured() {
        let mut t = Terminal::new(80, 24, 100);
        t.feed("\x1b]133;A\x07\x1b]133;D;130\x07");
        let regions = t.command_regions();
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].exit_code, Some(130));
    }

    #[test]
    fn osc_133_command_end_without_code() {
        let mut t = Terminal::new(80, 24, 100);
        t.feed("\x1b]133;A\x07\x1b]133;D\x07");
        let regions = t.command_regions();
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].command_end, Some(0));
        assert_eq!(regions[0].exit_code, None);
    }

    #[test]
    fn osc_133_trailing_params_tolerated() {
        let mut t = Terminal::new(80, 24, 100);
        // `aid=` on A, an extra `key=value` sibling on D's exit field.
        t.feed("\x1b]133;A;aid=foo\x07");
        t.feed("\x1b]133;D;1;err=oops\x07");
        let regions = t.command_regions();
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].exit_code, Some(1));
    }

    #[test]
    fn osc_133_non_numeric_exit_field_is_no_code() {
        let mut t = Terminal::new(80, 24, 100);
        // A `key=value` first field means "no exit code".
        t.feed("\x1b]133;A\x07\x1b]133;D;aid=7\x07");
        assert_eq!(t.command_regions()[0].exit_code, None);
    }

    #[test]
    fn osc_133_unknown_kind_ignored() {
        let mut t = Terminal::new(80, 24, 100);
        t.feed("\x1b]133;Z\x07");
        t.feed("\x1b]133;\x07");
        assert!(t.command_regions().is_empty());
    }

    #[test]
    fn osc_133_interrupted_command_has_no_end() {
        let mut t = Terminal::new(80, 24, 100);
        // Ctrl-C before the command finishes: A, B, C, but no D.
        t.feed("\x1b]133;A\x07\x1b]133;B\x07\x1b]133;C\x07");
        let regions = t.command_regions();
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].command_end, None);
        assert_eq!(regions[0].output_start, Some(0));
    }

    #[test]
    fn osc_133_back_to_back_prompts_make_two_regions() {
        let mut t = Terminal::new(80, 24, 100);
        // A bare prompt (no command), then another prompt. The second A
        // closes the first (still-open) region.
        t.feed("\x1b]133;A\x07\x1b]133;B\x07");
        t.feed("\r\n\x1b]133;A\x07\x1b]133;B\x07");
        let regions = t.command_regions();
        assert_eq!(regions.len(), 2);
        assert_eq!(regions[0].prompt_start, 0);
        assert_eq!(regions[1].prompt_start, 1);
    }

    #[test]
    fn osc_133_marks_ignored_on_alt_screen() {
        let mut t = Terminal::new(80, 24, 100);
        t.feed("\x1b[?1049h"); // enter alt screen
        t.feed("\x1b]133;A\x07\x1b]133;D;0\x07");
        t.feed("\x1b[?1049l"); // leave alt screen
        assert!(t.command_regions().is_empty());
    }

    #[test]
    fn osc_133_ed2_clears_marks() {
        let mut t = Terminal::new(80, 24, 100);
        t.feed("\x1b]133;A\x07\x1b]133;D;0\x07");
        assert_eq!(t.command_regions().len(), 1);
        t.feed("\x1b[2J"); // ED 2 — full-screen clear
        assert!(t.command_regions().is_empty());
    }

    #[test]
    fn osc_133_st_terminator_accepted() {
        let mut t = Terminal::new(80, 24, 100);
        // String Terminator (ESC \) instead of BEL.
        t.feed("\x1b]133;A\x1b\\\x1b]133;D;0\x1b\\");
        assert_eq!(t.command_regions().len(), 1);
        assert_eq!(t.command_regions()[0].exit_code, Some(0));
    }

    #[test]
    fn osc_133_absolute_indices_include_scrollback() {
        // Marks anchor below scrollback. Push a few lines into scrollback
        // first, then a mark on the live grid: its absolute index must be
        // offset by the scrollback length.
        let mut t = Terminal::new(80, 2, 100);
        t.feed("a\r\nb\r\nc\r\nd"); // scrolls rows into scrollback
        let sb = t.scrollback.len() as isize;
        assert!(sb > 0);
        t.feed("\x1b]133;A\x07");
        let regions = t.command_regions();
        assert_eq!(regions.len(), 1);
        // Cursor is on the last live row; abs = scrollback_len + cursor.row.
        assert_eq!(regions[0].prompt_start, sb + t.cursor.row as isize);
    }

    //
    // OSC 133 mark lifecycle: scroll into scrollback, eviction, resize.
    //

    #[test]
    fn osc_133_mark_follows_row_into_scrollback() {
        let mut t = Terminal::new(10, 3, 100);
        // Prompt mark on row 0 over "P1", then scroll it off the top.
        t.feed("P1\x1b]133;A\x07");
        t.feed("\r\nL2\r\nL3\r\nL4");
        // "P1" has spilled into scrollback row 0.
        assert_eq!(t.scrollback_len(), 1);
        assert_eq!(row_string(t.line_at(0).unwrap()), "P1");
        // The mark's absolute index tracked it: it now points at scrollback 0.
        let regions = t.command_regions();
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].prompt_start, 0);
    }

    #[test]
    fn osc_133_mark_evicted_when_its_scrollback_row_falls_off() {
        // Scrollback limit 2: once the marked line is pushed past the limit
        // its anchor row is evicted and the region disappears.
        let mut t = Terminal::new(10, 2, 2);
        t.feed("P1\x1b]133;A\x07");
        // Each \r\n at the bottom row scrolls one line into scrollback.
        t.feed("\r\nL2\r\nL3\r\nL4\r\nL5\r\nL6");
        assert_eq!(t.scrollback_len(), 2); // capped at the limit
        // "P1" was pushed out the front; its mark went with it.
        assert!(t.line_at(0).map(|c| row_string(c)) != Some("P1".to_string()));
        assert!(t.command_regions().is_empty());
    }

    #[test]
    fn osc_133_resize_shrink_spills_mark_into_scrollback() {
        let mut t = Terminal::new(10, 5, 100);
        t.feed("P1\x1b]133;A\x07\r\nL2\r\nL3\r\nL4\r\nL5");
        // Mark sits live on row 0.
        assert_eq!(t.command_regions()[0].prompt_start, 0);
        // Shrink to 3 rows spills the top two (P1, L2) into scrollback.
        t.resize(10, 3);
        assert_eq!(t.scrollback_len(), 2);
        assert_eq!(row_string(t.line_at(0).unwrap()), "P1");
        // The mark followed P1 to scrollback row 0.
        let regions = t.command_regions();
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].prompt_start, 0);
    }

    #[test]
    fn osc_133_resize_grow_pulls_mark_back_to_live() {
        let mut t = Terminal::new(10, 5, 100);
        t.feed("P1\x1b]133;A\x07\r\nL2\r\nL3\r\nL4\r\nL5");
        t.resize(10, 3); // mark spills to scrollback
        assert_eq!(t.scrollback_len(), 2);
        t.resize(10, 5); // grow pulls the rows (and the mark) back
        assert_eq!(t.scrollback_len(), 0);
        // Mark is live again on row 0; with empty scrollback abs == 0.
        let regions = t.command_regions();
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].prompt_start, 0);
        assert_eq!(row_string(t.row(0)), "P1");
    }

    #[test]
    fn osc_133_shrink_grow_round_trip_preserves_region() {
        let mut t = Terminal::new(10, 5, 100);
        // A full command cycle on rows 0..2.
        t.feed("P1\x1b]133;A\x07\x1b]133;B\x07ls\r\n\x1b]133;C\x07out\r\n\x1b]133;D;0\x07");
        let before = t.command_regions();
        t.resize(10, 2);
        t.resize(10, 5);
        // Indices and exit code survive the round trip unchanged.
        assert_eq!(t.command_regions(), before);
    }

    #[test]
    fn osc_133_marks_survive_alt_screen_resize() {
        let mut t = Terminal::new(10, 5, 100);
        t.feed("P1\x1b]133;A\x07");
        let before = t.command_regions();
        t.feed("\x1b[?1049h"); // enter alt screen
        t.resize(10, 3); // alt resize: clamp-only, no scrollback churn
        t.feed("\x1b[?1049l"); // back to primary
        assert_eq!(t.command_regions(), before);
    }

    #[test]
    fn osc_133_viewport_scroll_does_not_move_marks() {
        let mut t = Terminal::new(10, 3, 100);
        t.feed("P1\x1b]133;A\x07\r\nL2\r\nL3\r\nL4");
        let before = t.command_regions();
        // Scrolling the viewport into history is purely a view change; the
        // marks' absolute anchors must not move.
        t.scroll_up(1);
        assert_eq!(t.command_regions(), before);
        t.scroll_down(1);
        assert_eq!(t.command_regions(), before);
    }

    #[test]
    fn osc_133_ed3_drops_scrollback_marks_keeps_live() {
        let mut t = Terminal::new(10, 3, 100);
        // One mark scrolled into scrollback, one live on the current screen.
        t.feed("P1\x1b]133;A\x07\r\nL2\r\nL3\r\nL4");
        assert_eq!(t.scrollback_len(), 1);
        t.feed("\x1b]133;A\x07"); // a second prompt mark, live
        assert_eq!(t.command_regions().len(), 2);
        t.feed("\x1b[3J"); // ED 3 — drop scrollback
        // The scrollback-anchored region is gone; the live one remains.
        let regions = t.command_regions();
        assert_eq!(regions.len(), 1);
    }

    //
    // OSC 133 prompt navigation (K5).
    //

    #[test]
    fn osc_133_prompt_navigation_jumps_between_prompts() {
        let mut t = Terminal::new(10, 3, 100);
        // Three prompts, each pushed up so they end at distinct absolute
        // lines (some in scrollback).
        t.feed("\x1b]133;A\x07one\r\n\r\n");
        t.feed("\x1b]133;A\x07two\r\n\r\n");
        t.feed("\x1b]133;A\x07three\r\n\r\n");

        let mut prompts: Vec<isize> =
            t.command_regions().iter().map(|r| r.prompt_start).collect();
        prompts.sort();
        assert_eq!(prompts.len(), 3);

        t.scroll_to_bottom();
        let top0 = t.visual_to_abs_line(0);

        // Walk up through the prompts above the live top.
        assert!(t.scroll_to_prev_prompt());
        let t1 = t.visual_to_abs_line(0);
        assert!(t1 < top0);
        assert!(prompts.contains(&t1));

        assert!(t.scroll_to_prev_prompt());
        let t2 = t.visual_to_abs_line(0);
        assert!(t2 < t1);
        assert_eq!(t2, prompts[0]); // landed on the oldest prompt

        // Nothing above the oldest prompt.
        assert!(!t.scroll_to_prev_prompt());
        assert_eq!(t.visual_to_abs_line(0), t2);

        // Walking back down returns to the prompt we came from.
        assert!(t.scroll_to_next_prompt());
        assert_eq!(t.visual_to_abs_line(0), t1);
    }

    #[test]
    fn osc_133_prompt_navigation_noop_without_marks() {
        let mut t = Terminal::new(10, 3, 100);
        t.feed("plain\r\noutput\r\nmore\r\nlines\r\n");
        assert!(!t.scroll_to_prev_prompt());
        assert!(!t.scroll_to_next_prompt());
    }

    #[test]
    fn osc_133_prompt_navigation_noop_on_alt_screen() {
        let mut t = Terminal::new(10, 3, 100);
        t.feed("\x1b]133;A\x07one\r\n\r\n\x1b]133;A\x07two\r\n\r\n");
        t.feed("\x1b[?1049h"); // alt screen has no scrollback to navigate
        assert!(!t.scroll_to_prev_prompt());
        assert!(!t.scroll_to_next_prompt());
    }

    //
    // OSC 133 prompt-status gutter markers (K6).
    //

    #[test]
    fn osc_133_status_marker_success() {
        let mut t = Terminal::new(10, 3, 100);
        t.feed("\x1b]133;A\x07\x1b]133;D;0\x07");
        assert_eq!(
            t.prompt_status_markers(),
            vec![(0, PromptStatus::Success)]
        );
    }

    #[test]
    fn osc_133_status_marker_failure() {
        let mut t = Terminal::new(10, 3, 100);
        t.feed("\x1b]133;A\x07\x1b]133;D;1\x07");
        assert_eq!(
            t.prompt_status_markers(),
            vec![(0, PromptStatus::Failure)]
        );
    }

    #[test]
    fn osc_133_status_marker_pending_until_command_ends() {
        let mut t = Terminal::new(10, 3, 100);
        // Prompt drawn, command still running (no D, or D without a code).
        t.feed("\x1b]133;A\x07\x1b]133;B\x07");
        assert_eq!(
            t.prompt_status_markers(),
            vec![(0, PromptStatus::Pending)]
        );
        t.feed("\x1b]133;D\x07"); // ended, but no exit code reported
        assert_eq!(
            t.prompt_status_markers(),
            vec![(0, PromptStatus::Pending)]
        );
    }

    #[test]
    fn osc_133_status_markers_track_each_prompt() {
        let mut t = Terminal::new(10, 3, 100);
        // Two completed commands (success then failure) on distinct rows.
        t.feed("\x1b]133;A\x07ok\r\n\x1b]133;D;0\x07");
        t.feed("\x1b]133;A\x07bad\r\n\x1b]133;D;1\x07");
        let markers = t.prompt_status_markers();
        assert_eq!(markers.len(), 2);
        // Each marker's line equals its region's prompt_start.
        let regions = t.command_regions();
        assert_eq!(markers[0].0, regions[0].prompt_start);
        assert_eq!(markers[0].1, PromptStatus::Success);
        assert_eq!(markers[1].0, regions[1].prompt_start);
        assert_eq!(markers[1].1, PromptStatus::Failure);
    }

    #[test]
    fn osc_133_status_markers_empty_without_marks() {
        let mut t = Terminal::new(10, 3, 100);
        t.feed("just some output\r\n");
        assert!(t.prompt_status_markers().is_empty());
    }

    //
    // OSC 2122 yutani-private current-input report (K8).
    //

    #[test]
    fn osc_2122_basic_report_sets_buffer_and_cursor() {
        let mut t = Terminal::new(80, 24, 100);
        // base64("git ") == "Z2l0IA==", cursor at end (4 chars).
        t.feed("\x1b]2122;4;Z2l0IA==\x07");
        let ci = t.current_input().expect("current input set");
        assert_eq!(ci.buffer, "git ");
        assert_eq!(ci.cursor, 4);
    }

    #[test]
    fn osc_2122_cursor_clamped_to_char_count() {
        let mut t = Terminal::new(80, 24, 100);
        // "git " is 4 chars; a cursor of 99 clamps to 4.
        t.feed("\x1b]2122;99;Z2l0IA==\x07");
        let ci = t.current_input().expect("current input set");
        assert_eq!(ci.buffer, "git ");
        assert_eq!(ci.cursor, 4);
    }

    #[test]
    fn osc_2122_empty_buffer_is_some_not_none() {
        let mut t = Terminal::new(80, 24, 100);
        // Empty edit line: cursor 0, empty base64 payload.
        t.feed("\x1b]2122;0;\x07");
        let ci = t.current_input().expect("empty line is still Some");
        assert_eq!(ci.buffer, "");
        assert_eq!(ci.cursor, 0);
    }

    #[test]
    fn osc_2122_multibyte_roundtrip_preserves_char_cursor() {
        let mut t = Terminal::new(80, 24, 100);
        // base64("café 🚀") == "Y2Fmw6kg8J+agA==". 6 code points
        // (c a f é space 🚀); place the cursor on char 5 (before the emoji).
        t.feed("\x1b]2122;5;Y2Fmw6kg8J+agA==\x07");
        let ci = t.current_input().expect("current input set");
        assert_eq!(ci.buffer, "café 🚀");
        assert_eq!(ci.buffer.chars().count(), 6);
        assert_eq!(ci.cursor, 5);
    }

    #[test]
    fn osc_2122_invalid_base64_is_ignored() {
        let mut t = Terminal::new(80, 24, 100);
        t.feed("\x1b]2122;2;not valid base64!!!\x07");
        assert!(t.current_input().is_none());
    }

    #[test]
    fn osc_2122_non_numeric_cursor_is_ignored() {
        let mut t = Terminal::new(80, 24, 100);
        t.feed("\x1b]2122;abc;Z2l0IA==\x07");
        assert!(t.current_input().is_none());
    }

    #[test]
    fn osc_2122_missing_semicolon_is_ignored() {
        let mut t = Terminal::new(80, 24, 100);
        // No `;` separating cursor from buffer at all.
        t.feed("\x1b]2122;4\x07");
        assert!(t.current_input().is_none());
    }

    #[test]
    fn osc_2122_malformed_leaves_prior_state_untouched() {
        let mut t = Terminal::new(80, 24, 100);
        t.feed("\x1b]2122;4;Z2l0IA==\x07"); // valid: "git "
        t.feed("\x1b]2122;abc;Z2l0IA==\x07"); // bad cursor: ignored
        let ci = t.current_input().expect("prior state retained");
        assert_eq!(ci.buffer, "git ");
        assert_eq!(ci.cursor, 4);
    }

    #[test]
    fn osc_2122_ignored_on_alternate_screen() {
        let mut t = Terminal::new(80, 24, 100);
        t.feed("\x1b[?1049h"); // enter alternate screen
        t.feed("\x1b]2122;4;Z2l0IA==\x07");
        assert!(t.current_input().is_none());
    }

    #[test]
    fn osc_2122_cleared_by_osc_133_command_submit() {
        let mut t = Terminal::new(80, 24, 100);
        // base64("echo") == "ZWNobw==".
        t.feed("\x1b]2122;4;ZWNobw==\x07");
        assert!(t.current_input().is_some());
        // OSC 133 `C` = command submitted -> clears the live input.
        t.feed("\x1b]133;C\x07");
        assert!(t.current_input().is_none());
    }

    #[test]
    fn osc_2122_cleared_when_entering_alternate_screen() {
        let mut t = Terminal::new(80, 24, 100);
        t.feed("\x1b]2122;4;ZWNobw==\x07");
        assert!(t.current_input().is_some());
        t.feed("\x1b[?1049h"); // enter alternate screen clears the report
        assert!(t.current_input().is_none());
    }

    //
    // OSC 133 select-last-command-output span (K7).
    //

    #[test]
    fn osc_133_last_output_span_basic() {
        let mut t = Terminal::new(10, 5, 100);
        // Prompt row 0; output "file1"/"file2" on rows 1..2; D on row 3.
        t.feed("\x1b]133;A\x07$ \x1b]133;B\x07ls\r\n");
        t.feed("\x1b]133;C\x07file1\r\nfile2\r\n\x1b]133;D;0\x07");
        // Output span is OutputStart (1) .. CommandEnd-1 (2), inclusive.
        assert_eq!(t.last_command_output_span(), Some((1, 2)));
    }

    #[test]
    fn osc_133_last_output_span_none_when_no_output() {
        let mut t = Terminal::new(10, 5, 100);
        // C and D land on the same row — the command produced nothing.
        t.feed("\x1b]133;A\x07\x1b]133;C\x07\x1b]133;D;0\x07");
        assert_eq!(t.last_command_output_span(), None);
    }

    #[test]
    fn osc_133_last_output_span_picks_most_recent_completed() {
        let mut t = Terminal::new(10, 10, 100);
        // First command: one line of output.
        t.feed("\x1b]133;A\x07\x1b]133;C\x07a\r\n\x1b]133;D;0\x07");
        // Second command: two lines of output (rows 1..2, D on row 3).
        t.feed("\x1b]133;A\x07\x1b]133;C\x07b\r\nc\r\n\x1b]133;D;0\x07");
        assert_eq!(t.last_command_output_span(), Some((1, 2)));
    }

    #[test]
    fn osc_133_last_output_span_skips_running_command() {
        let mut t = Terminal::new(10, 10, 100);
        // A completed command (output on row 0, D on row 1)...
        t.feed("\x1b]133;A\x07\x1b]133;C\x07done\r\n\x1b]133;D;0\x07");
        // ...then a still-running one (no D). The span falls back to the
        // last *completed* command.
        t.feed("\x1b]133;A\x07\x1b]133;C\x07running\r\n");
        assert_eq!(t.last_command_output_span(), Some((0, 0)));
    }

    #[test]
    fn osc_133_last_output_span_none_without_marks() {
        let mut t = Terminal::new(10, 5, 100);
        t.feed("plain output\r\n");
        assert_eq!(t.last_command_output_span(), None);
    }

    //
    // iTerm2 OSC 1337 parsing tests (P2.2). The parser only fills the
    // `pending_image_uploads` queue here — cursor advancement and
    // placement creation land in P2.3.
    //

    /// Build a minimal PNG and wrap it in an iTerm2 OSC 1337 `File=…`
    /// payload. `extra` is appended to the args list (e.g. `;width=5`).
    fn iterm_osc(extra: &str) -> String {
        use base64::Engine;
        // 2×2 RGBA PNG — encoded via the `image` crate to stay aligned with
        // what real callers send and what our header peek expects.
        let img = image::RgbaImage::from_pixel(2, 2, image::Rgba([10, 20, 30, 255]));
        let mut png: Vec<u8> = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageOutputFormat::Png)
            .unwrap();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        format!("\x1b]1337;File=inline=1{}:{}\x07", extra, b64)
    }

    //
    // OSC 7 working-directory reporting.
    //

    #[test]
    fn osc_7_file_url_sets_cwd() {
        let mut t = Terminal::new(80, 24, 100);
        t.feed("\x1b]7;file://myhost/Users/ry/projects\x07");
        assert_eq!(t.take_cwd_update().as_deref(), Some("/Users/ry/projects"));
    }

    #[test]
    fn osc_8_open_attaches_link_to_printed_cells() {
        let mut t = Terminal::new(80, 24, 100);
        // Open a link, print two glyphs, close it, print a third.
        t.feed("\x1b]8;;https://example.com/\x07AB\x1b]8;;\x07C");
        let a = t.visible_cell(0, 0).hyperlink.expect("A is linked");
        let b = t.visible_cell(0, 1).hyperlink.expect("B is linked");
        assert_eq!(a, b, "adjacent cells of one link share an id");
        assert_eq!(t.hyperlink_uri(a).as_deref(), Some("https://example.com/"));
        assert!(t.visible_cell(0, 2).hyperlink.is_none(), "C is after close");
    }

    #[test]
    fn osc_8_empty_uri_closes_link() {
        let mut t = Terminal::new(80, 24, 100);
        t.feed("\x1b]8;;https://a/\x07X\x1b]8;;\x07Y");
        assert!(t.visible_cell(0, 0).hyperlink.is_some());
        assert!(t.visible_cell(0, 1).hyperlink.is_none());
    }

    #[test]
    fn osc_8_ignores_id_param_and_keeps_uri() {
        let mut t = Terminal::new(80, 24, 100);
        // params field (`id=foo`) is accepted and ignored; URI still applies.
        t.feed("\x1b]8;id=foo;https://example.org/\x07Z");
        let z = t.visible_cell(0, 0).hyperlink.expect("Z is linked");
        assert_eq!(t.hyperlink_uri(z).as_deref(), Some("https://example.org/"));
    }

    #[test]
    fn osc_8_anonymous_same_uri_does_not_merge() {
        // Two separate anonymous opens of the same URI are distinct logical
        // links per the OSC 8 spec (only an explicit `id=` groups spans), so
        // they must intern to different ids and not co-highlight.
        let mut t = Terminal::new(80, 24, 100);
        t.feed("\x1b]8;;https://dup/\x07A\x1b]8;;\x07 \x1b]8;;https://dup/\x07B\x1b]8;;\x07");
        let a = t.visible_cell(0, 0).hyperlink.expect("A linked");
        let b = t.visible_cell(0, 2).hyperlink.expect("B linked");
        assert_ne!(a, b, "anonymous opens of the same URI stay distinct");
        // ...but both still resolve to that URI.
        assert_eq!(t.hyperlink_uri(a).as_deref(), Some("https://dup/"));
        assert_eq!(t.hyperlink_uri(b).as_deref(), Some("https://dup/"));
    }

    #[test]
    fn osc_8_explicit_id_groups_noncontiguous_spans() {
        // Two spans sharing `id=grp` and the same URI are one logical link:
        // they must intern to the same id even though unlinked text (and a
        // close) separates them.
        let mut t = Terminal::new(80, 24, 100);
        t.feed("\x1b]8;id=grp;https://x/\x07A\x1b]8;;\x07 mid \x1b]8;id=grp;https://x/\x07B\x1b]8;;\x07");
        let a = t.visible_cell(0, 0).hyperlink.expect("A linked");
        let b = t.visible_cell(0, 6).hyperlink.expect("B linked");
        assert_eq!(a, b, "same (id, uri) groups the spans");
    }

    #[test]
    fn osc_8_same_id_different_uri_is_distinct() {
        // The id is scoped to the URI: the same `id=` with a different target
        // is a different link.
        let mut t = Terminal::new(80, 24, 100);
        t.feed("\x1b]8;id=g;https://a/\x07A\x1b]8;;\x07\x1b]8;id=g;https://b/\x07B\x1b]8;;\x07");
        let a = t.visible_cell(0, 0).hyperlink.expect("A linked");
        let b = t.visible_cell(0, 1).hyperlink.expect("B linked");
        assert_ne!(a, b, "same id but different uri => different links");
    }

    #[test]
    fn osc_8_different_ids_same_uri_are_distinct() {
        let mut t = Terminal::new(80, 24, 100);
        t.feed("\x1b]8;id=one;https://x/\x07A\x1b]8;;\x07\x1b]8;id=two;https://x/\x07B\x1b]8;;\x07");
        let a = t.visible_cell(0, 0).hyperlink.expect("A linked");
        let b = t.visible_cell(0, 1).hyperlink.expect("B linked");
        assert_ne!(a, b, "different ids => different links");
    }

    #[test]
    fn osc_8_id_not_first_param_still_groups() {
        // The params field is a colon-separated key=value list and `id=` need
        // not be first. Two spans with `foo=bar:id=grp` (id second) and the
        // same URI are one logical link.
        let mut t = Terminal::new(80, 24, 100);
        t.feed("\x1b]8;foo=bar:id=grp;https://x/\x07A\x1b]8;;\x07 \x1b]8;baz=qux:id=grp;https://x/\x07B\x1b]8;;\x07");
        let a = t.visible_cell(0, 0).hyperlink.expect("A linked");
        let b = t.visible_cell(0, 2).hyperlink.expect("B linked");
        assert_eq!(a, b, "id= grouped regardless of param position");
        assert_eq!(t.hyperlink_uri(a).as_deref(), Some("https://x/"));
    }

    #[test]
    fn osc_8_empty_id_value_falls_back_to_anonymous() {
        // `id=` with an empty value is not a real id (filtered out), so each
        // open is anonymous and same-URI spans stay distinct.
        let mut t = Terminal::new(80, 24, 100);
        t.feed("\x1b]8;id=;https://x/\x07A\x1b]8;;\x07 \x1b]8;id=;https://x/\x07B\x1b]8;;\x07");
        let a = t.visible_cell(0, 0).hyperlink.expect("A linked");
        let b = t.visible_cell(0, 2).hyperlink.expect("B linked");
        assert_ne!(a, b, "empty id= is anonymous, spans stay distinct");
    }

    #[test]
    fn osc_8_explicit_id_and_anonymous_same_uri_stay_separate() {
        // A keyed (id=grp) span and a separate anonymous span sharing the URI
        // are different logical links: the anon open must not adopt the keyed
        // id, so they intern distinctly.
        let mut t = Terminal::new(80, 24, 100);
        t.feed("\x1b]8;id=grp;https://x/\x07A\x1b]8;;\x07 \x1b]8;;https://x/\x07B\x1b]8;;\x07");
        let a = t.visible_cell(0, 0).hyperlink.expect("A linked");
        let b = t.visible_cell(0, 2).hyperlink.expect("B linked");
        assert_ne!(a, b, "keyed and anonymous same-URI spans are distinct");
    }

    #[test]
    fn osc_8_survives_sgr_reset() {
        let mut t = Terminal::new(80, 24, 100);
        // An SGR reset (CSI 0 m) mid-link must NOT sever the hyperlink: it
        // lives on the cursor, not the SGR style.
        t.feed("\x1b]8;;https://keep/\x07A\x1b[0mB\x1b]8;;\x07");
        assert!(t.visible_cell(0, 0).hyperlink.is_some());
        assert!(
            t.visible_cell(0, 1).hyperlink.is_some(),
            "link persists across SGR reset"
        );
    }

    #[test]
    fn osc_8_over_long_uri_closes_instead_of_interning() {
        let mut t = Terminal::new(80, 24, 100);
        let long = "https://".to_string() + &"a".repeat(MAX_HYPERLINK_URI_LEN);
        t.feed(&format!("\x1b]8;;{long}\x07X"));
        assert!(
            t.visible_cell(0, 0).hyperlink.is_none(),
            "over-long URI is treated as a close"
        );
    }

    #[test]
    fn osc_8_link_resets_on_full_reset() {
        let mut t = Terminal::new(80, 24, 100);
        t.feed("\x1b]8;;https://x/\x07");
        t.feed("\x1bc"); // RIS full reset clears the cursor (and its link).
        t.feed("Y");
        assert!(t.visible_cell(0, 0).hyperlink.is_none());
    }

    #[test]
    fn osc_8_uri_with_semicolon_query_survives_intact() {
        // Only the first ';' separates params from the URI; semicolons inside
        // the URI (matrix/query params) must be kept verbatim, not truncated.
        let mut t = Terminal::new(80, 24, 100);
        t.feed("\x1b]8;;https://h/p?a=1;b=2;c=3\x07Q\x1b]8;;\x07");
        let q = t.visible_cell(0, 0).hyperlink.expect("Q is linked");
        assert_eq!(t.hyperlink_uri(q).as_deref(), Some("https://h/p?a=1;b=2;c=3"));
    }

    #[test]
    fn osc_8_malformed_no_semicolon_treats_whole_as_uri() {
        // `rest` with no ';' at all is malformed per spec; we keep it as the
        // URI rather than dropping the link entirely.
        let mut t = Terminal::new(80, 24, 100);
        t.feed("\x1b]8;https://noparams/\x07W\x1b]8;;\x07");
        let w = t.visible_cell(0, 0).hyperlink.expect("W is linked");
        assert_eq!(t.hyperlink_uri(w).as_deref(), Some("https://noparams/"));
    }

    #[test]
    fn osc_8_reopen_with_different_uri_gives_distinct_ids() {
        // Switching the active link mid-line (without an explicit close) must
        // retag subsequent cells with the new target's id.
        let mut t = Terminal::new(80, 24, 100);
        t.feed("\x1b]8;;https://one/\x07A\x1b]8;;https://two/\x07B\x1b]8;;\x07");
        let a = t.visible_cell(0, 0).hyperlink.expect("A linked");
        let b = t.visible_cell(0, 1).hyperlink.expect("B linked");
        assert_ne!(a, b, "different URIs must intern to different ids");
        assert_eq!(t.hyperlink_uri(a).as_deref(), Some("https://one/"));
        assert_eq!(t.hyperlink_uri(b).as_deref(), Some("https://two/"));
    }

    #[test]
    fn osc_8_left_open_tags_trailing_cells() {
        // A link never explicitly closed before input ends still tags every
        // glyph printed after it was opened.
        let mut t = Terminal::new(80, 24, 100);
        t.feed("\x1b]8;;https://open/\x07XYZ");
        let x = t.visible_cell(0, 0).hyperlink.expect("X linked");
        let y = t.visible_cell(0, 1).hyperlink.expect("Y linked");
        let z = t.visible_cell(0, 2).hyperlink.expect("Z linked");
        assert_eq!(x, y);
        assert_eq!(y, z);
        assert_eq!(t.hyperlink_uri(x).as_deref(), Some("https://open/"));
    }

    #[test]
    fn osc_8_open_then_close_with_no_glyphs_tags_nothing() {
        // Opening and immediately closing with nothing printed in between must
        // leave no tagged cell, and whatever prints afterward is unlinked.
        let mut t = Terminal::new(80, 24, 100);
        t.feed("\x1b]8;;https://void/\x07\x1b]8;;\x07P");
        assert!(t.visible_cell(0, 0).hyperlink.is_none(), "P prints after close");
    }

    #[test]
    fn osc_8_link_saved_and_restored_with_cursor() {
        // DECSC/DECRC (ESC 7 / ESC 8) save and restore the whole cursor,
        // including the active hyperlink. Open a link, save, close it, then
        // restore: the next glyph must carry the saved link again.
        let mut t = Terminal::new(80, 24, 100);
        t.feed("\x1b]8;;https://saved/\x07\x1b7"); // open + DECSC
        t.feed("\x1b]8;;\x07"); // close the active link
        t.feed("\x1b8"); // DECRC restores cursor (and its link)
        t.feed("R");
        let r = t.visible_cell(0, 0).hyperlink.expect("R relinked after restore");
        assert_eq!(t.hyperlink_uri(r).as_deref(), Some("https://saved/"));
    }

    #[test]
    fn osc_7_empty_host_triple_slash() {
        let mut t = Terminal::new(80, 24, 100);
        t.feed("\x1b]7;file:///var/log\x07");
        assert_eq!(t.take_cwd_update().as_deref(), Some("/var/log"));
    }

    #[test]
    fn osc_7_percent_decodes_path() {
        let mut t = Terminal::new(80, 24, 100);
        // "%20" -> space, "%C3%A9" -> é
        t.feed("\x1b]7;file://h/Users/ry/My%20Code/caf%C3%A9\x07");
        assert_eq!(
            t.take_cwd_update().as_deref(),
            Some("/Users/ry/My Code/café")
        );
    }

    #[test]
    fn osc_7_bare_absolute_path_accepted() {
        let mut t = Terminal::new(80, 24, 100);
        t.feed("\x1b]7;/tmp/work\x07");
        assert_eq!(t.take_cwd_update().as_deref(), Some("/tmp/work"));
    }

    #[test]
    fn osc_7_take_update_clears_dirty() {
        let mut t = Terminal::new(80, 24, 100);
        t.feed("\x1b]7;file://h/a/b\x07");
        assert_eq!(t.take_cwd_update().as_deref(), Some("/a/b"));
        // Second take with no new report yields nothing.
        assert_eq!(t.take_cwd_update(), None);
    }

    #[test]
    fn osc_7_unchanged_dir_does_not_redirty() {
        let mut t = Terminal::new(80, 24, 100);
        t.feed("\x1b]7;file://h/a/b\x07");
        assert_eq!(t.take_cwd_update().as_deref(), Some("/a/b"));
        // Re-emitting the identical directory (every prompt does this) must
        // not mark the cwd dirty again.
        t.feed("\x1b]7;file://h/a/b\x07");
        assert_eq!(t.take_cwd_update(), None);
        // A genuine change does dirty it.
        t.feed("\x1b]7;file://h/a/c\x07");
        assert_eq!(t.take_cwd_update().as_deref(), Some("/a/c"));
    }

    #[test]
    fn osc_7_malformed_payloads_ignored() {
        let mut t = Terminal::new(80, 24, 100);
        // Relative path, unknown scheme, host-only file URL, and empty body.
        t.feed("\x1b]7;relative/path\x07");
        t.feed("\x1b]7;http://example.com/x\x07");
        t.feed("\x1b]7;file://hostonly\x07");
        t.feed("\x1b]7;\x07");
        assert_eq!(t.take_cwd_update(), None);
    }

    #[test]
    fn percent_decode_passes_through_lone_percent() {
        // A trailing or malformed '%' is kept verbatim, not dropped.
        assert_eq!(percent_decode_path("/a%"), "/a%");
        assert_eq!(percent_decode_path("/a%zz/b"), "/a%zz/b");
        assert_eq!(percent_decode_path("/plain/path"), "/plain/path");
    }

    #[test]
    fn osc_7_st_terminator_accepted() {
        let mut t = Terminal::new(80, 24, 100);
        // String Terminator (ESC \) instead of BEL.
        t.feed("\x1b]7;file://h/a/b\x1b\\");
        assert_eq!(t.take_cwd_update().as_deref(), Some("/a/b"));
    }

    #[test]
    fn osc_1337_minimal_payload_queues_one_upload() {
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&iterm_osc(""));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        // Header peek runs synchronously inside `handle_osc_1337` — the
        // 2×2 dimensions of our fixture should round-trip.
        assert_eq!(uploads[0].pixel_size, Some((2, 2)));
        // Auto sizing when no width/height given.
        assert_eq!(uploads[0].width, ImageSizeSpec::Auto);
        assert_eq!(uploads[0].height, ImageSizeSpec::Auto);
        assert!(uploads[0].preserve_aspect);
        assert!(!uploads[0].do_not_move_cursor);
        // Bytes round-trip — re-decoding should give back the same image.
        assert!(crate::images::peek_dimensions(&uploads[0].bytes).is_some());
    }

    #[test]
    fn osc_1337_parses_explicit_cell_sizing() {
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&iterm_osc(";width=10;height=5"));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        assert_eq!(uploads[0].width, ImageSizeSpec::Cells(10));
        assert_eq!(uploads[0].height, ImageSizeSpec::Cells(5));
    }

    #[test]
    fn osc_1337_parses_pixel_and_percent_sizing() {
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&iterm_osc(";width=200px;height=50%"));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        assert_eq!(uploads[0].width, ImageSizeSpec::Pixels(200));
        assert_eq!(uploads[0].height, ImageSizeSpec::Percent(50));
    }

    #[test]
    fn osc_1337_preserve_aspect_off() {
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&iterm_osc(";preserveAspectRatio=0"));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        assert!(!uploads[0].preserve_aspect);
    }

    #[test]
    fn osc_1337_do_not_move_cursor_flag() {
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&iterm_osc(";doNotMoveCursor=1"));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        assert!(uploads[0].do_not_move_cursor);
    }

    #[test]
    fn osc_1337_inline_zero_is_dropped() {
        // inline=0 is download mode in iTerm; we have no download UI, so
        // the OSC must be a clean no-op rather than a partial decode.
        let mut t = Terminal::new(80, 24, 100);
        let payload = iterm_osc(";inline=0");
        // Strip the original `inline=1` so only `inline=0` is present.
        let payload = payload.replace("inline=1;", "");
        t.feed(&payload);
        assert!(t.take_pending_image_uploads().is_empty());
    }

    #[test]
    fn osc_1337_bad_base64_silently_dropped() {
        let mut t = Terminal::new(80, 24, 100);
        // `==` in the middle is not a valid base64 stream.
        t.feed("\x1b]1337;File=inline=1:not!base64!\x07");
        assert!(t.take_pending_image_uploads().is_empty());
    }

    #[test]
    fn osc_1337_missing_colon_silently_dropped() {
        // No `:` separator between args and body → no payload to decode.
        let mut t = Terminal::new(80, 24, 100);
        t.feed("\x1b]1337;File=inline=1;width=5\x07");
        assert!(t.take_pending_image_uploads().is_empty());
    }

    #[test]
    fn osc_1337_non_file_verb_silently_dropped() {
        // iTerm uses OSC 1337 for many things; we only handle File.
        let mut t = Terminal::new(80, 24, 100);
        t.feed("\x1b]1337;SetMark\x07");
        t.feed("\x1b]1337;CursorShape=1\x07");
        assert!(t.take_pending_image_uploads().is_empty());
    }

    #[test]
    fn osc_1337_unknown_keys_ignored_not_failed() {
        // iTerm contract: unknown keys are silently accepted so newer
        // params don't break older parsers.
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&iterm_osc(";futureKey=42;size=100;width=5"));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        assert_eq!(uploads[0].width, ImageSizeSpec::Cells(5));
    }

    #[test]
    fn osc_1337_st_terminator_accepted() {
        // The VT parser accepts BEL (0x07) or ESC \ (ST) — same payload
        // either way. Real iTerm callers use both.
        let mut t = Terminal::new(80, 24, 100);
        let with_bel = iterm_osc("");
        let with_st = with_bel.replace('\x07', "\x1b\\");
        t.feed(&with_st);
        assert_eq!(t.take_pending_image_uploads().len(), 1);
    }

    #[test]
    fn osc_1337_multiple_payloads_in_one_feed_all_queued() {
        let mut t = Terminal::new(80, 24, 100);
        let combo = format!("{}{}", iterm_osc(";width=5"), iterm_osc(";width=10"));
        t.feed(&combo);
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 2);
        assert_eq!(uploads[0].width, ImageSizeSpec::Cells(5));
        assert_eq!(uploads[1].width, ImageSizeSpec::Cells(10));
    }

    #[test]
    fn osc_1337_take_drains_queue() {
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&iterm_osc(""));
        let first = t.take_pending_image_uploads();
        assert_eq!(first.len(), 1);
        // Second take returns empty — pending list is owned-moved out.
        let second = t.take_pending_image_uploads();
        assert!(second.is_empty());
    }

    #[test]
    fn osc_1337_size_parser_handles_all_iterm_forms() {
        assert_eq!(parse_iterm_size("auto"), Some(ImageSizeSpec::Auto));
        assert_eq!(parse_iterm_size("AUTO"), Some(ImageSizeSpec::Auto));
        assert_eq!(parse_iterm_size(""), Some(ImageSizeSpec::Auto));
        assert_eq!(parse_iterm_size("5"), Some(ImageSizeSpec::Cells(5)));
        assert_eq!(parse_iterm_size("100px"), Some(ImageSizeSpec::Pixels(100)));
        assert_eq!(parse_iterm_size("50%"), Some(ImageSizeSpec::Percent(50)));
        // Unsupported units fall through to None — caller substitutes Auto.
        assert_eq!(parse_iterm_size("5em"), None);
        assert_eq!(parse_iterm_size("-1"), None);
    }

    //
    // XTWINOPS — what the kitty kitten queries on startup to learn cell
    // pixel size. Without these the kitten refuses to send images at all.
    //

    #[test]
    fn xtwinops_14_replies_with_text_area_pixel_size() {
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        t.feed("\x1b[14t");
        // height = rows * line_h = 24 * 16 = 384
        // width  = cols * cell_w = 80 * 8 = 640
        assert_eq!(t.take_response(), b"\x1b[4;384;640t");
    }

    #[test]
    fn xtwinops_16_replies_with_cell_pixel_size() {
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(9, 20);
        t.feed("\x1b[16t");
        // height = line_h = 20, width = cell_w = 9
        assert_eq!(t.take_response(), b"\x1b[6;20;9t");
    }

    #[test]
    fn xtwinops_18_replies_with_text_area_character_size() {
        let mut t = Terminal::new(80, 24, 100);
        t.feed("\x1b[18t");
        // rows=24, cols=80
        assert_eq!(t.take_response(), b"\x1b[8;24;80t");
    }

    #[test]
    fn xtwinops_unrelated_action_codes_are_ignored() {
        // 1 = de-iconify, 3 = move, 4 = resize, 5 = raise. Honoring
        // these would let any program move our window without consent.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        for ps in [1, 3, 4, 5, 22, 23] {
            t.feed(&format!("\x1b[{}t", ps));
            assert!(t.take_response().is_empty(), "ps={} should be silent", ps);
        }
    }

    #[test]
    fn xtwinops_uses_clamped_one_for_unset_cell_size() {
        // If State hasn't called set_cell_size_px yet, default is 1×1.
        // The kitten will still get a reply (no error), just a degenerate
        // one — better than nothing.
        let mut t = Terminal::new(80, 24, 100);
        t.feed("\x1b[14t");
        assert_eq!(t.take_response(), b"\x1b[4;24;80t");
    }

    //
    // K1.2: Kitty graphics-protocol control-data parser. Tests pin the
    // defaults (which apply when keys are omitted, common in real Kitty
    // payloads) and the action / format / transmission discriminants.
    //

    #[test]
    fn parse_kitty_control_empty_returns_defaults() {
        // Spec default when the control list is empty: a=T, f=100, t=d.
        let c = parse_kitty_control("").unwrap();
        assert_eq!(c.action, KittyAction::TransmitAndDisplay);
        assert_eq!(c.format, KittyFormat::Png);
        assert_eq!(c.transmission, KittyTransmission::Direct);
        assert!(!c.more_chunks);
        assert!(!c.do_not_move_cursor);
        assert_eq!(c.quiet, 0);
    }

    #[test]
    fn parse_kitty_control_action_variants() {
        assert_eq!(parse_kitty_control("a=t").unwrap().action, KittyAction::Transmit);
        assert_eq!(parse_kitty_control("a=T").unwrap().action, KittyAction::TransmitAndDisplay);
        assert_eq!(parse_kitty_control("a=q").unwrap().action, KittyAction::Query);
        assert_eq!(parse_kitty_control("a=p").unwrap().action, KittyAction::Place);
        assert_eq!(parse_kitty_control("a=d").unwrap().action, KittyAction::Delete);
        assert_eq!(parse_kitty_control("a=f").unwrap().action, KittyAction::AnimationFrame);
        assert_eq!(parse_kitty_control("a=a").unwrap().action, KittyAction::AnimationControl);
        // Unknown actions land on Other — the Kitty contract says
        // "unknown action = no-op", encoded as Other + dispatcher drop.
        assert_eq!(parse_kitty_control("a=Z").unwrap().action, KittyAction::Other);
    }

    #[test]
    fn parse_kitty_control_format_and_transmission() {
        assert_eq!(parse_kitty_control("f=100").unwrap().format, KittyFormat::Png);
        assert_eq!(parse_kitty_control("f=32").unwrap().format, KittyFormat::Rgba);
        assert_eq!(parse_kitty_control("f=24").unwrap().format, KittyFormat::Rgb);
        assert_eq!(parse_kitty_control("f=99").unwrap().format, KittyFormat::Other);
        assert_eq!(parse_kitty_control("t=d").unwrap().transmission, KittyTransmission::Direct);
        assert_eq!(parse_kitty_control("t=f").unwrap().transmission, KittyTransmission::File);
        assert_eq!(parse_kitty_control("t=s").unwrap().transmission, KittyTransmission::SharedMemory);
        assert_eq!(parse_kitty_control("t=x").unwrap().transmission, KittyTransmission::Other);
        assert_eq!(parse_kitty_control("t=t").unwrap().transmission, KittyTransmission::TempFile);
    }

    #[test]
    fn parse_kitty_control_ids_and_sizing() {
        let c = parse_kitty_control("i=42,p=7,c=10,r=5").unwrap();
        assert_eq!(c.image_id, Some(42));
        assert_eq!(c.placement_id, Some(7));
        assert_eq!(c.cells_cols, Some(10));
        assert_eq!(c.cells_rows, Some(5));
    }

    #[test]
    fn parse_kitty_control_chunking_and_cursor_and_quiet() {
        let c = parse_kitty_control("m=1,C=1,q=2").unwrap();
        assert!(c.more_chunks);
        assert!(c.do_not_move_cursor);
        assert_eq!(c.quiet, 2);
    }

    #[test]
    fn parse_kitty_control_unknown_keys_accepted() {
        // Forward-compat: unknown keys must not abort the parse.
        let c = parse_kitty_control("a=T,futureKey=99,o=z,i=1").unwrap();
        assert_eq!(c.action, KittyAction::TransmitAndDisplay);
        assert_eq!(c.image_id, Some(1));
    }

    #[test]
    fn parse_kitty_control_malformed_value_falls_back_to_default() {
        // i= with garbage → None (not Some(0)) so the dispatcher can
        // distinguish "no id given" from "id 0".
        let c = parse_kitty_control("i=notanumber,a=T").unwrap();
        assert_eq!(c.image_id, None);
        assert_eq!(c.action, KittyAction::TransmitAndDisplay);
    }

    #[test]
    fn parse_kitty_control_animation_keys_populate_overlapping_fields() {
        // `s=`, `v=`, `c=`, `r=`, `z=` are all overloaded between
        // transmission semantics and animation semantics. The parser
        // populates both alias fields; the dispatcher picks based on
        // action so a single raw value reaches the right place.
        let c = parse_kitty_control("a=f,i=7,r=3,c=2,z=50,X=4,Y=6,s=128,v=64").unwrap();
        assert_eq!(c.action, KittyAction::AnimationFrame);
        assert_eq!(c.image_id, Some(7));
        // r= is both cells_rows and anim_frame_num.
        assert_eq!(c.cells_rows, Some(3));
        assert_eq!(c.anim_frame_num, Some(3));
        // c= is both cells_cols and anim_compose_base / anim_make_current.
        assert_eq!(c.cells_cols, Some(2));
        assert_eq!(c.anim_compose_base, Some(2));
        assert_eq!(c.anim_make_current, Some(2));
        // z= is both z_index and anim_gap_ms.
        assert_eq!(c.z_index, Some(50));
        assert_eq!(c.anim_gap_ms, Some(50));
        // s= / v= remain source dimensions for `a=f` (raw pixel data).
        assert_eq!(c.source_w, Some(128));
        assert_eq!(c.source_h, Some(64));
        // ...and the animation aliases also get populated; the dispatcher
        // ignores them for `a=f`.
        assert_eq!(c.anim_control, Some(128));
        assert_eq!(c.anim_loop_count, Some(64));
    }

    #[test]
    fn parse_kitty_control_animation_control_keys() {
        // `a=a,i=1,s=3,v=0` → run with infinite loops.
        let c = parse_kitty_control("a=a,i=1,s=3,v=0").unwrap();
        assert_eq!(c.action, KittyAction::AnimationControl);
        assert_eq!(c.image_id, Some(1));
        assert_eq!(c.anim_control, Some(3));
        assert_eq!(c.anim_loop_count, Some(0));
        // `a=a,i=1,c=4` → make frame 4 current.
        let c = parse_kitty_control("a=a,i=1,c=4").unwrap();
        assert_eq!(c.anim_make_current, Some(4));
        // `a=a,i=1,r=2,z=200` → edit frame 2 gap to 200ms.
        let c = parse_kitty_control("a=a,i=1,r=2,z=200").unwrap();
        assert_eq!(c.anim_frame_num, Some(2));
        assert_eq!(c.anim_gap_ms, Some(200));
    }

    #[test]
    fn a_f_does_not_create_a_placement() {
        // Frame transmissions are metadata for the parent image, not
        // their own visible objects. The dispatcher must queue them
        // with `display_immediately: false` and `cell_extent: (0, 0)`
        // so main.rs's drain skips placement creation.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        let png = {
            let buf = image::RgbaImage::from_pixel(2, 2, image::Rgba([1, 2, 3, 255]));
            let mut bytes = Vec::new();
            image::DynamicImage::ImageRgba8(buf)
                .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageOutputFormat::Png)
                .expect("encode");
            bytes
        };
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        let apc = format!("\x1b_Ga=f,f=100,i=7,z=20;{}\x1b\\", b64);
        t.feed(&apc);
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        let up = &uploads[0];
        assert!(!up.display_immediately, "a=f must not display");
        assert_eq!(up.cell_extent, (0, 0), "a=f has no cell extent of its own");
        assert!(
            up.animation_frame.is_some(),
            "a=f must flag the animation_frame spec so the drain routes correctly",
        );
        assert_eq!(t.cursor().row, 0, "a=f must not advance the cursor");
        assert_eq!(t.cursor().col, 0);
    }

    #[test]
    fn a_f_chunked_assembles_into_one_upload() {
        // The chunking path on `a=f` mirrors `a=t`'s — split the
        // payload across multiple APCs with `m=1`, then a final `m=0`
        // chunk to flush. Only one PendingImageUpload should fall out
        // of the queue, carrying the assembled bytes.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        let png = {
            let buf = image::RgbaImage::from_pixel(3, 3, image::Rgba([9, 9, 9, 255]));
            let mut bytes = Vec::new();
            image::DynamicImage::ImageRgba8(buf)
                .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageOutputFormat::Png)
                .expect("encode");
            bytes
        };
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        let mid = b64.len() / 2;
        let (head, tail) = b64.split_at(mid);
        let apc1 = format!("\x1b_Ga=f,f=100,i=9,m=1;{}\x1b\\", head);
        let apc2 = format!("\x1b_Ga=f,f=100,i=9,m=0;{}\x1b\\", tail);
        t.feed(&apc1);
        // First chunk must not produce an upload — it's still in the
        // accumulator.
        assert!(
            t.take_pending_image_uploads().is_empty(),
            "intermediate m=1 chunk must not queue an upload",
        );
        t.feed(&apc2);
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1, "final chunk must finalize");
        let up = &uploads[0];
        assert!(up.animation_frame.is_some());
        assert!(up.bytes.starts_with(b"\x89PNG"), "assembled bytes look like PNG");
    }

    #[test]
    fn parse_kitty_control_capital_i_is_alias_for_image_id() {
        // icat uses I= (image number) for transmissions, not i=. The
        // protocol distinguishes them — number is meant to be a
        // client-side counter that the terminal maps to a real id —
        // but for our renderer's purposes the number works as the
        // identifier directly.
        let c = parse_kitty_control("I=60091135").unwrap();
        assert_eq!(c.image_id, Some(60091135));
        // If both keys are present, the lowercase `i=` wins.
        let c = parse_kitty_control("i=5,I=99").unwrap();
        assert_eq!(c.image_id, Some(5));
    }

    #[test]
    fn a_f_size_inference_picks_rgba_when_byte_count_matches_w_h_4() {
        // icat sends some a=f frames with no `f=` and raw RGBA
        // payload (w*h*4 bytes). The dispatcher must infer RGBA from
        // the byte count — *not* default to the base's f=24 or to
        // PNG. Mis-inference causes shifted rows that look like
        // tiled/repeating artifacts and the corruption compounds
        // across delta frames since each composes onto the previous.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        // Base: f=24 RGB, so kitty_image_formats[42] = Rgb.
        use base64::Engine;
        let raw_rgb_base: Vec<u8> = (0..4 * 4 * 3).map(|_| 0xAAu8).collect();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&raw_rgb_base);
        t.feed(&format!("\x1b_Ga=T,f=24,s=4,v=4,I=42;{}\x1b\\", b64));
        let _ = t.take_pending_image_uploads();

        // a=f frame, NO `f=`, payload is exactly w*h*4 bytes (RGBA).
        // Size inference picks RGBA; the raw-bypass path hands the
        // bytes through unchanged with `raw_rgba_dims` set.
        let raw_rgba_frame: Vec<u8> = (0..2 * 2 * 4).map(|_| 0x44u8).collect();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&raw_rgba_frame);
        t.feed(&format!("\x1b_Ga=f,s=2,v=2,I=42,z=100;{}\x1b\\", b64));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1, "frame queues despite omitted f=");
        let up = &uploads[0];
        assert!(up.animation_frame.is_some());
        assert_eq!(up.raw_rgba_dims, Some((2, 2)), "RGBA inferred and bypass taken");
        assert_eq!(up.bytes, raw_rgba_frame);
    }

    #[test]
    fn a_f_size_inference_picks_rgb_when_byte_count_matches_w_h_3() {
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        // Base is f=32 RGBA — so base-format inheritance would say
        // "RGBA" but the actual frame payload is RGB. Inference
        // must use the byte count to pick RGB instead.
        use base64::Engine;
        let raw_rgba_base: Vec<u8> = (0..4 * 4 * 4).map(|_| 0xAAu8).collect();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&raw_rgba_base);
        t.feed(&format!("\x1b_Ga=T,f=32,s=4,v=4,I=42;{}\x1b\\", b64));
        let _ = t.take_pending_image_uploads();

        // a=f with no f=, payload is exactly w*h*3 → must infer RGB.
        // Raw-bypass pads to RGBA (alpha=255).
        let raw_rgb_frame: Vec<u8> = (0..2 * 2 * 3).map(|_| 0x44u8).collect();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&raw_rgb_frame);
        t.feed(&format!("\x1b_Ga=f,s=2,v=2,I=42,z=100;{}\x1b\\", b64));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        let up = &uploads[0];
        assert!(up.animation_frame.is_some());
        assert_eq!(up.raw_rgba_dims, Some((2, 2)));
        assert_eq!(up.bytes.len(), 2 * 2 * 4);
        for px in up.bytes.chunks_exact(4) {
            assert_eq!(px[0..3], [0x44, 0x44, 0x44]);
            assert_eq!(px[3], 0xFF, "alpha padded to opaque for RGB input");
        }
    }

    #[test]
    fn a_f_mid_gap_byte_count_falls_back_to_base_format_not_png() {
        // Repro for "image decode failed: The image format could not
        // be determined" on kitty animations: a frame whose inflated
        // byte count sits between w*h*3+PAGE and w*h*4 used to fall
        // through every byte-count branch and end up at PNG (the
        // parser's "no f= seen" sentinel). The PNG decoder can't
        // recognize raw RGB(A) and prints the user-visible error.
        // With the fix, the resolver consults the base format and
        // accepts RGB/RGBA so the raw-bypass path runs.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        use base64::Engine;
        // Base: f=24 RGB at 4x4 → kitty_image_formats[42] = Rgb.
        let raw_rgb_base: Vec<u8> = (0..4 * 4 * 3).map(|_| 0x11u8).collect();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&raw_rgb_base);
        t.feed(&format!("\x1b_Ga=T,f=24,s=4,v=4,I=42;{}\x1b\\", b64));
        let _ = t.take_pending_image_uploads();

        // Frame omits f=. Build a payload sized to land in the gap
        // between w*h*3+PAGE and w*h*4 for w=h=64 (rgb=12288,
        // rgba=16384, PAGE=16384 so the +PAGE windows cover both —
        // pick larger dims to expose the gap).
        let (w, h) = (450u32, 450u32);
        let rgb = (w as usize) * (h as usize) * 3; // 607500
        let rgba = (w as usize) * (h as usize) * 4; // 810000
        let mid = rgb + 16 * 1024 + 10_000; // 633884 — past rgb+PAGE, under rgba
        assert!(mid > rgb + 16 * 1024);
        assert!(mid < rgba);
        let mid_payload = vec![0x77u8; mid];
        let b64 = base64::engine::general_purpose::STANDARD.encode(&mid_payload);
        t.feed(&format!("\x1b_Ga=f,s={},v={},I=42,z=10;{}\x1b\\", w, h, b64));
        let uploads = t.take_pending_image_uploads();
        // Frame queues via the raw-bypass path. Without the fix this
        // assertion fired because the frame got routed through the
        // PNG worker and was rejected before producing an upload.
        assert_eq!(uploads.len(), 1, "mid-gap frame must not be dropped");
        let up = &uploads[0];
        assert!(up.animation_frame.is_some());
        assert!(up.raw_rgba_dims.is_some(), "raw-bypass path taken (not PNG decoder)");
    }

    #[test]
    fn a_f_uses_lowercase_xy_as_destination_position() {
        // icat's a=f messages carry the frame's parent-coords via
        // lowercase `x=` / `y=` (not capital X/Y). On a=T those keys
        // mean source-crop; on a=f they mean "where in the parent
        // does this frame go". A regression that reads from
        // pixel_offset_x/y instead would stamp every frame at (0, 0)
        // and the animation would visibly distort.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        use base64::Engine;
        let raw_rgba = vec![0u8; 4 * 4 * 4];
        let b64 = base64::engine::general_purpose::STANDARD.encode(&raw_rgba);
        // Base.
        t.feed(&format!("\x1b_Ga=T,f=32,s=4,v=4,I=77;{}\x1b\\", b64));
        let _ = t.take_pending_image_uploads();
        // Frame with lowercase x=10, y=20 (and capital X/Y absent).
        let frame_rgba = vec![1u8; 2 * 2 * 4];
        let b64 = base64::engine::general_purpose::STANDARD.encode(&frame_rgba);
        t.feed(&format!(
            "\x1b_Ga=f,f=32,s=2,v=2,x=10,y=20,I=77,z=50;{}\x1b\\",
            b64,
        ));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        let spec = uploads[0]
            .animation_frame
            .as_ref()
            .expect("animation_frame populated");
        assert_eq!(spec.dst_x, 10, "lowercase x= must reach dst_x");
        assert_eq!(spec.dst_y, 20, "lowercase y= must reach dst_y");
    }

    #[test]
    fn a_t_base_image_with_omitted_f_infers_rgb_format() {
        // Regression: icat sometimes sends a=T with no f=. Payload
        // is raw RGB/RGBA but the parser defaults f= to PNG; the
        // dispatch path must run size-based inference. After the
        // raw-bypass perf change, raw inputs no longer round-trip
        // through PNG — they ride straight to the GPU upload path
        // via `raw_rgba_dims`. Assert the bypass marker is set with
        // the right dims and the bytes are raw RGBA (4 bytes/pixel,
        // alpha=255 padded from the RGB input).
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        use base64::Engine;
        let raw_rgb: Vec<u8> = (0..4 * 4 * 3).map(|i| (i % 256) as u8).collect();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&raw_rgb);
        t.feed(&format!("\x1b_Ga=T,s=4,v=4,I=99;{}\x1b\\", b64));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1, "base normalizes despite omitted f=");
        let up = &uploads[0];
        assert_eq!(up.kitty_image_id, Some(99));
        assert_eq!(
            up.raw_rgba_dims,
            Some((4, 4)),
            "raw RGB took the worker-bypass path",
        );
        assert_eq!(up.bytes.len(), 4 * 4 * 4, "RGB padded to RGBA");
        // Every 4th byte should be 0xFF (padded alpha).
        for px in up.bytes.chunks_exact(4) {
            assert_eq!(px[3], 0xFF);
        }
        // Subsequent a=f frames also bypass when the format inference
        // (or inherited base format) lands on a raw variant.
        let raw_rgb_frame: Vec<u8> = (0..2 * 2 * 3).map(|_| 0x33u8).collect();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&raw_rgb_frame);
        t.feed(&format!("\x1b_Ga=f,s=2,v=2,I=99,z=30;{}\x1b\\", b64));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        assert_eq!(uploads[0].raw_rgba_dims, Some((2, 2)));
        assert_eq!(uploads[0].bytes.len(), 2 * 2 * 4);
    }

    #[test]
    fn a_t_base_image_with_omitted_f_infers_rgba_format() {
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        use base64::Engine;
        // 4*4*4 byte payload → infer RGBA. After the raw-bypass
        // perf change, the dispatcher hands the bytes through
        // unchanged with `raw_rgba_dims` set.
        let raw_rgba: Vec<u8> = (0..4 * 4 * 4).map(|i| (i % 256) as u8).collect();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&raw_rgba);
        t.feed(&format!("\x1b_Ga=T,s=4,v=4,I=100;{}\x1b\\", b64));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        assert_eq!(uploads[0].raw_rgba_dims, Some((4, 4)));
        assert_eq!(uploads[0].bytes.len(), 4 * 4 * 4);
        // Bytes should be the raw payload truncated to expected size.
        assert_eq!(uploads[0].bytes, raw_rgba);
    }

    #[test]
    fn a_f_size_inference_keeps_png_when_payload_has_png_signature() {
        // App that genuinely sends PNG with no `f=100`. The 89 50 4E
        // 47 ... signature trumps everything else.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        // Build a real PNG.
        let png = {
            let buf = image::RgbaImage::from_pixel(4, 4, image::Rgba([1, 2, 3, 255]));
            let mut bytes = Vec::new();
            image::DynamicImage::ImageRgba8(buf)
                .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageOutputFormat::Png)
                .expect("encode");
            bytes
        };
        use base64::Engine;
        // Base also PNG so kitty_image_formats[42] = Png.
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        t.feed(&format!("\x1b_Ga=T,I=42;{}\x1b\\", b64));
        let _ = t.take_pending_image_uploads();

        // a=f with no f= and PNG signature → keep as PNG even though
        // a 4x4 image's byte count happens to land in some range.
        let frame_png = {
            let buf = image::RgbaImage::from_pixel(2, 2, image::Rgba([9, 9, 9, 255]));
            let mut bytes = Vec::new();
            image::DynamicImage::ImageRgba8(buf)
                .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageOutputFormat::Png)
                .expect("encode");
            bytes
        };
        let b64 = base64::engine::general_purpose::STANDARD.encode(&frame_png);
        t.feed(&format!("\x1b_Ga=f,I=42,z=100;{}\x1b\\", b64));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        let up = &uploads[0];
        assert!(up.animation_frame.is_some());
        // The bytes are still PNG (not re-encoded by the raw normalizer).
        assert!(up.bytes.starts_with(b"\x89PNG"));
    }

    #[test]
    fn parse_kitty_control_negative_z_doesnt_populate_gap_ms() {
        // `z=-1` parses as i32 z_index but fails u32 anim_gap_ms.
        // Important: dispatcher uses anim_gap_ms for animation paths,
        // so a negative z must not silently set a wrap-around gap.
        let c = parse_kitty_control("z=-1").unwrap();
        assert_eq!(c.z_index, Some(-1));
        assert_eq!(c.anim_gap_ms, None);
    }

    //
    // K1.3 / K1.4: Kitty graphics dispatch end-to-end. Builds a small
    // PNG, wraps it in an APC, feeds it through `Terminal::feed`, and
    // verifies the upload queue + cursor state. Same shape as the
    // `osc_1337_*` tests but for the Kitty wire format.
    //

    /// Build a Kitty graphics APC wrapping `png_bytes`. `args` is the
    /// pre-`;` control string (e.g. `"a=T,f=100,c=5,r=2"`).
    fn kitty_apc(args: &str, png_bytes: &[u8]) -> String {
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(png_bytes);
        format!("\x1b_G{};{}\x1b\\", args, b64)
    }

    /// Same as `kitty_apc` but with a control-only payload (no `;`).
    /// Used for `a=q` queries that carry no image data.
    fn kitty_apc_control_only(args: &str) -> String {
        format!("\x1b_G{}\x1b\\", args)
    }

    /// Re-encode the same fixture PNG that the iTerm tests use, so we
    /// know `peek_dimensions` will return Some((w, h)).
    fn kitty_png(w: u32, h: u32) -> Vec<u8> {
        let buf = image::RgbaImage::from_pixel(w, h, image::Rgba([0, 128, 255, 255]));
        let mut bytes: Vec<u8> = Vec::new();
        image::DynamicImage::ImageRgba8(buf)
            .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageOutputFormat::Png)
            .unwrap();
        bytes
    }

    #[test]
    fn kitty_apc_single_chunk_queues_one_upload() {
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        let png = kitty_png(4, 4);
        t.feed(&kitty_apc("a=T,f=100,c=2,r=1", &png));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        assert_eq!(uploads[0].cell_extent, (1, 2));
        assert_eq!(uploads[0].pixel_size, Some((4, 4)));
        // Cursor advanced by 1 row (cell_extent.0).
        assert_eq!(t.cursor().row, 1);
    }

    #[test]
    fn kitty_apc_default_action_is_transmit_and_display() {
        // `a=` omitted → default T per spec → image displays.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        let png = kitty_png(2, 2);
        t.feed(&kitty_apc("f=100,c=3,r=2", &png));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        assert_eq!(uploads[0].cell_extent, (2, 3));
    }

    #[test]
    fn kitty_apc_no_sizing_falls_back_to_image_native_extent() {
        // c/r omitted → Auto → cell extent = ceil(pixels / cell_size).
        // 16x16 image, 8x16 cell → 1 row × 2 cols.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        let png = kitty_png(16, 16);
        t.feed(&kitty_apc("a=T,f=100", &png));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        assert_eq!(uploads[0].cell_extent, (1, 2));
    }

    #[test]
    fn kitty_apc_do_not_move_cursor() {
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        t.feed("\x1b[10;1H"); // CUP row 10 col 1 → cursor.row = 9
        let png = kitty_png(4, 4);
        t.feed(&kitty_apc("a=T,f=100,c=2,r=1,C=1", &png));
        // C=1 must keep the cursor at row 9, NOT advance.
        assert_eq!(t.cursor().row, 9);
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads[0].cell_anchor, (9, 0));
    }

    #[test]
    fn kitty_apc_chunked_transmission_assembles_full_payload() {
        // Split the same payload across three APCs (m=1, m=1, m=0) with
        // the same i=. The accumulator must concatenate them in order
        // and only emit the upload on the terminator.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        let png = kitty_png(4, 4);
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        let chunk_size = (b64.len() / 3 + 1).max(4);
        let c1 = &b64[..chunk_size];
        let c2 = &b64[chunk_size..2 * chunk_size];
        let c3 = &b64[2 * chunk_size..];

        // First chunk carries the sizing.
        t.feed(&format!("\x1b_Ga=T,f=100,c=2,r=1,i=42,m=1;{}\x1b\\", c1));
        assert!(t.take_pending_image_uploads().is_empty());
        // Middle chunk continues — only `i=` and `m=1` matter, sizing
        // here would be ignored (and we leave it absent to verify that).
        t.feed(&format!("\x1b_Gi=42,m=1;{}\x1b\\", c2));
        assert!(t.take_pending_image_uploads().is_empty());
        // Terminal chunk — m=0 (or omitted; this test uses omitted to
        // pin the "absent m means last" path).
        t.feed(&format!("\x1b_Gi=42;{}\x1b\\", c3));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        assert_eq!(uploads[0].cell_extent, (1, 2));
        // Round-trip the payload: peek_dimensions on the assembled
        // bytes should still see 4×4.
        assert_eq!(uploads[0].pixel_size, Some((4, 4)));
    }

    #[test]
    fn kitty_apc_chunked_continuation_chunks_without_i_thread_to_first_chunks_id() {
        // Regression for the tmux-icat tofu bug: `kitten icat` puts
        // `i=` only on the FIRST chunk of a chunked transmission and
        // omits it on every continuation (and on the terminator).
        // Without the `current_chunked_id` threading in `handle_apc`,
        // continuation chunks fall into the anonymous bucket and the
        // id-keyed entry leaks — `register_kitty_image_id` never
        // fires, so placeholder cells later resolve to nothing.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        let png = kitty_png(4, 4);
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        let chunk_size = (b64.len() / 3 + 1).max(4);
        let c1 = &b64[..chunk_size];
        let c2 = &b64[chunk_size..2 * chunk_size];
        let c3 = &b64[2 * chunk_size..];
        // First chunk: i=43 (carries the sizing).
        t.feed(&format!("\x1b_Ga=T,f=100,c=2,r=1,i=43,m=1;{}\x1b\\", c1));
        // Continuation: NO i=, just m=1 — must still land in the id=43 entry.
        t.feed(&format!("\x1b_Ga=T,m=1;{}\x1b\\", c2));
        // Final chunk: NO i=, NO m= — must finalize the id=43 entry.
        t.feed(&format!("\x1b_Ga=T;{}\x1b\\", c3));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1, "id=43 stream must finalize");
        assert_eq!(uploads[0].kitty_image_id, Some(43));
        assert_eq!(uploads[0].pixel_size, Some((4, 4)));
    }

    #[test]
    fn kitty_apc_chunked_id_cleared_after_terminator() {
        // After an id-keyed chunked stream finalizes, a subsequent
        // unrelated chunked-without-id transmission must NOT inherit
        // the stale id. Otherwise a second image would pile onto the
        // first's bucket and corrupt both.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        let png = kitty_png(4, 4);
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        let mid = b64.len() / 2;
        // First image: id=43, two chunks, continuation drops `i=`.
        t.feed(&format!("\x1b_Ga=T,f=100,c=2,r=1,i=43,m=1;{}\x1b\\", &b64[..mid]));
        t.feed(&format!("\x1b_Ga=T;{}\x1b\\", &b64[mid..]));
        let first = t.take_pending_image_uploads();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].kitty_image_id, Some(43));
        // Second image: chunked, never says `i=`. Must go to the
        // anonymous bucket — NOT into a stale id=43 entry.
        t.feed(&format!("\x1b_Ga=T,f=100,c=2,r=1,m=1;{}\x1b\\", &b64[..mid]));
        t.feed(&format!("\x1b_Ga=T;{}\x1b\\", &b64[mid..]));
        let second = t.take_pending_image_uploads();
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].kitty_image_id, None);
    }

    #[test]
    fn kitty_apc_a_f_continuation_chunks_without_i_thread_to_first_chunks_id() {
        // Same threading guarantee for animation frames: icat sends
        // `i=43` on the first `a=f` chunk and omits it on
        // continuations. Without injection the continuation chunks
        // get dropped on the floor at the `client_id` guard at the
        // top of `handle_apc_animation_frame`.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        // Base image so kitty_image_formats[43] is recorded.
        use base64::Engine;
        let base_rgba = vec![0u8; 4 * 4 * 4];
        let b64 = base64::engine::general_purpose::STANDARD.encode(&base_rgba);
        t.feed(&format!("\x1b_Ga=T,f=32,s=4,v=4,i=43;{}\x1b\\", b64));
        let _ = t.take_pending_image_uploads();
        // Frame in 3 chunks; only the first carries `i=43`.
        let frame: Vec<u8> = vec![0xCDu8; 2 * 2 * 4];
        let b64 = base64::engine::general_purpose::STANDARD.encode(&frame);
        let chunk = (b64.len() / 3 + 1).max(4);
        let c1 = &b64[..chunk];
        let c2 = &b64[chunk..2 * chunk];
        let c3 = &b64[2 * chunk..];
        t.feed(&format!("\x1b_Ga=f,f=32,s=2,v=2,i=43,m=1;{}\x1b\\", c1));
        t.feed(&format!("\x1b_Ga=f,m=1;{}\x1b\\", c2));
        t.feed(&format!("\x1b_Ga=f;{}\x1b\\", c3));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1, "frame must finalize despite missing i=");
        assert!(uploads[0].animation_frame.is_some());
    }

    #[test]
    fn kitty_apc_a_f_chunked_frame_keeps_first_chunks_gap_ms() {
        // Regression for "animations run way too fast" under tmux.
        // `kitten icat` puts `z=N` (gap_ms), `x=`/`y=` (dst), `r=`
        // (target_slot), `c=` (compose_base) only on the FIRST `a=f`
        // chunk and omits them on continuations. The chunked
        // finalize was reading these off the LAST chunk's `ctrl`,
        // which zeroed them all — so every frame's gap_ms came out
        // as 0 and the playback ran at the 1ms floor (~1000 fps).
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        use base64::Engine;
        // Base so kitty_image_formats[43] is recorded.
        let base_rgba = vec![0u8; 4 * 4 * 4];
        let b64 = base64::engine::general_purpose::STANDARD.encode(&base_rgba);
        t.feed(&format!("\x1b_Ga=T,f=32,s=4,v=4,i=43;{}\x1b\\", b64));
        let _ = t.take_pending_image_uploads();
        // Frame in 2 chunks. First carries z=100 (gap_ms), x=5,
        // y=7, r=2 (target_slot), c=1 (compose_base). Second drops
        // all of them along with `i=`.
        let frame: Vec<u8> = vec![0xCDu8; 2 * 2 * 4];
        let b64 = base64::engine::general_purpose::STANDARD.encode(&frame);
        let mid = b64.len() / 2;
        t.feed(&format!(
            "\x1b_Ga=f,f=32,s=2,v=2,i=43,z=100,x=5,y=7,r=2,c=1,m=1;{}\x1b\\",
            &b64[..mid],
        ));
        t.feed(&format!("\x1b_Ga=f;{}\x1b\\", &b64[mid..]));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        let frame_spec = uploads[0]
            .animation_frame
            .as_ref()
            .expect("animation_frame spec present");
        assert_eq!(frame_spec.gap_ms, 100, "gap_ms preserved from first chunk");
        assert_eq!(frame_spec.dst_x, 5, "dst_x preserved from first chunk");
        assert_eq!(frame_spec.dst_y, 7, "dst_y preserved from first chunk");
        assert_eq!(frame_spec.target_slot, Some(2));
        assert_eq!(frame_spec.compose_base, Some(1));
    }

    #[test]
    fn kitty_apc_chunked_uses_first_chunks_sizing_not_last() {
        // First chunk: c=10. Last chunk: c=3 (would be ignored). Pin
        // the contract: first-chunk sizing wins.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        let png = kitty_png(4, 4);
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        let mid = b64.len() / 2;
        t.feed(&format!("\x1b_Ga=T,f=100,c=10,r=2,i=7,m=1;{}\x1b\\", &b64[..mid]));
        t.feed(&format!("\x1b_Ga=T,c=3,r=99,i=7;{}\x1b\\", &b64[mid..]));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        assert_eq!(uploads[0].cell_extent, (2, 10));
    }

    #[test]
    fn kitty_apc_unknown_format_silently_dropped() {
        // Unknown `f=` value (`f=99`) lands on KittyFormat::Other and
        // is silently dropped. f=24/32/100 are all wired up now —
        // see their focused tests.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        let png = kitty_png(4, 4);
        t.feed(&kitty_apc("a=T,f=99,c=2,r=1", &png));
        assert!(t.take_pending_image_uploads().is_empty());
    }

    #[test]
    fn kitty_apc_unsupported_transmission_silently_dropped() {
        // Unknown t= value falls into KittyTransmission::Other and
        // drops cleanly. (t=f / t=t / t=s are all implemented now —
        // see their own focused tests.)
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        let png = kitty_png(4, 4);
        for medium in ["x", "Q"] {
            t.feed(&kitty_apc(&format!("a=T,f=100,t={},c=2,r=1", medium), &png));
            assert!(
                t.take_pending_image_uploads().is_empty(),
                "t={} should drop",
                medium
            );
        }
    }

    #[test]
    fn kitty_apc_t_f_reads_file_from_disk() {
        // Real kitty +kitten icat picks `t=f` (file) by default for
        // local PNG inputs — the payload is a base64-encoded UTF-8
        // path. Write a PNG to a temp file, point an APC at it, and
        // assert the queue receives the file's bytes intact.
        use base64::Engine;
        let png = kitty_png(4, 4);
        let dir = std::env::temp_dir();
        let path = dir.join(format!("yutani-kitty-test-{}.png", std::process::id()));
        std::fs::write(&path, &png).expect("write temp png");
        let path_b64 = base64::engine::general_purpose::STANDARD.encode(path.to_str().unwrap());

        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        t.feed(&format!("\x1b_Ga=T,f=100,t=f,c=2,r=1;{}\x1b\\", path_b64));

        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1, "file-transmission upload should land");
        assert_eq!(uploads[0].bytes, png);
        assert_eq!(uploads[0].pixel_size, Some((4, 4)));
        assert_eq!(uploads[0].cell_extent, (1, 2));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn kitty_apc_t_f_missing_file_drops_silently() {
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD
            .encode("/definitely/does/not/exist.png");
        t.feed(&format!("\x1b_Ga=T,f=100,t=f;{}\x1b\\", b64));
        assert!(t.take_pending_image_uploads().is_empty());
    }

    #[test]
    fn kitty_apc_t_f_non_utf8_path_drops_silently() {
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        use base64::Engine;
        // Invalid UTF-8 in the path → reject before touching the
        // filesystem. (A real path with non-UTF-8 bytes on macOS would
        // also be rejected — kitty's spec implies UTF-8 paths.)
        let b64 = base64::engine::general_purpose::STANDARD.encode([0xFF, 0xFE, 0xFD]);
        t.feed(&format!("\x1b_Ga=T,f=100,t=f;{}\x1b\\", b64));
        assert!(t.take_pending_image_uploads().is_empty());
    }

    //
    // K6: spec corners (X/Y, z, x/y/w/h source crops, o=z zlib).
    //

    #[test]
    fn parse_kitty_control_pixel_offset_and_z() {
        let c = parse_kitty_control("X=3,Y=7,z=-5").unwrap();
        assert_eq!(c.pixel_offset_x, Some(3));
        assert_eq!(c.pixel_offset_y, Some(7));
        assert_eq!(c.z_index, Some(-5));
    }

    #[test]
    fn parse_kitty_control_source_crop_quad() {
        let c = parse_kitty_control("x=10,y=20,w=100,h=80").unwrap();
        assert_eq!(c.crop_x, Some(10));
        assert_eq!(c.crop_y, Some(20));
        assert_eq!(c.crop_w, Some(100));
        assert_eq!(c.crop_h, Some(80));
    }

    #[test]
    fn parse_kitty_control_zlib_compression() {
        let c = parse_kitty_control("o=z").unwrap();
        assert!(c.compressed_zlib);
        // Only `o=z` enables — other values (none defined yet) don't.
        let c = parse_kitty_control("o=q").unwrap();
        assert!(!c.compressed_zlib);
    }

    #[test]
    fn kitty_placement_params_partial_crop_returns_none() {
        // x/y/w/h is all-or-nothing per spec. Partial sets fall back to
        // None so the renderer samples the whole image.
        let mut ctrl = KittyControl::default();
        ctrl.crop_x = Some(10);
        ctrl.crop_w = Some(50);
        let (_, _, src) = kitty_placement_params(&ctrl);
        assert!(src.is_none());
    }

    #[test]
    fn kitty_placement_params_full_crop_passes_through() {
        let mut ctrl = KittyControl::default();
        ctrl.crop_x = Some(1);
        ctrl.crop_y = Some(2);
        ctrl.crop_w = Some(3);
        ctrl.crop_h = Some(4);
        let (_, _, src) = kitty_placement_params(&ctrl);
        assert_eq!(src, Some((1, 2, 3, 4)));
    }

    #[test]
    fn kitty_placement_params_zero_crop_size_falls_back_to_none() {
        // A degenerate w=0 / h=0 crop is treated as "no crop" — the
        // renderer can't sample a zero-size rect.
        let mut ctrl = KittyControl::default();
        ctrl.crop_x = Some(0);
        ctrl.crop_y = Some(0);
        ctrl.crop_w = Some(0);
        ctrl.crop_h = Some(10);
        let (_, _, src) = kitty_placement_params(&ctrl);
        assert!(src.is_none());
    }

    #[test]
    fn kitty_placement_params_defaults_zero() {
        let ctrl = KittyControl::default();
        let (offset, z, src) = kitty_placement_params(&ctrl);
        assert_eq!(offset, (0, 0));
        assert_eq!(z, 0);
        assert!(src.is_none());
    }

    #[test]
    fn kitty_apc_with_pixel_offset_and_z_lands_on_pending_upload() {
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        let png = kitty_png(4, 4);
        t.feed(&kitty_apc("a=T,f=100,c=2,r=1,X=4,Y=8,z=42", &png));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        assert_eq!(uploads[0].pixel_offset, (4, 8));
        assert_eq!(uploads[0].z_index, 42);
    }

    #[test]
    fn kitty_apc_with_source_crop_lands_on_pending_upload() {
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        let png = kitty_png(10, 10);
        t.feed(&kitty_apc("a=T,f=100,c=2,r=1,x=2,y=3,w=5,h=4", &png));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        assert_eq!(uploads[0].src_rect, Some((2, 3, 5, 4)));
    }

    #[test]
    fn kitty_apc_a_p_threads_pixel_offset_and_z_to_placement() {
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        t.register_kitty_image_id(7, ImageId(99));
        t.feed(&kitty_apc_control_only("a=p,i=7,c=2,r=1,X=3,Y=5,z=-2"));
        let placements = t.live_placements();
        assert_eq!(placements.len(), 1);
        assert_eq!(placements[0].pixel_offset, (3, 5));
        assert_eq!(placements[0].z, -2);
    }

    #[test]
    fn kitty_apc_o_z_zlib_inflates_before_decode() {
        // Compress a real PNG with zlib, send via t=d,o=z, assert the
        // inflated bytes round-trip into a working upload.
        use base64::Engine;
        use std::io::Write;
        let png = kitty_png(4, 4);
        let mut encoder = flate2::write::ZlibEncoder::new(
            Vec::new(),
            flate2::Compression::default(),
        );
        encoder.write_all(&png).unwrap();
        let compressed = encoder.finish().unwrap();
        // Compression of a tiny PNG often INFLATES because of headers
        // — that's fine for the test; the inflate path still has to work.
        let b64 = base64::engine::general_purpose::STANDARD.encode(&compressed);
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        t.feed(&format!("\x1b_Ga=T,f=100,o=z,c=2,r=1;{}\x1b\\", b64));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        assert_eq!(uploads[0].pixel_size, Some((4, 4)));
    }

    #[test]
    fn kitty_apc_o_z_malformed_zlib_silently_dropped() {
        // Garbage bytes claimed as zlib — inflate fails, no panic, no
        // partial upload.
        use base64::Engine;
        let bogus = base64::engine::general_purpose::STANDARD.encode(b"not zlib at all");
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        t.feed(&format!("\x1b_Ga=T,f=100,o=z;{}\x1b\\", bogus));
        assert!(t.take_pending_image_uploads().is_empty());
    }

    //
    // K3: POSIX shared-memory transmission (`t=s`). Unix-only — the
    // tests create a real SHM segment via libc, populate it, send the
    // APC, assert the upload landed AND the segment was unlinked.
    //

    #[cfg(unix)]
    fn write_shm(name: &str, bytes: &[u8]) {
        use std::ffi::CString;
        let c_name = CString::new(name).unwrap();
        unsafe {
            // O_CREAT | O_RDWR | 0o600 — caller is responsible for an
            // earlier unlink if reusing a name.
            let fd = libc::shm_open(
                c_name.as_ptr(),
                libc::O_CREAT | libc::O_RDWR,
                0o600,
            );
            assert!(fd >= 0, "shm_open failed");
            let r = libc::ftruncate(fd, bytes.len() as libc::off_t);
            assert_eq!(r, 0);
            let p = libc::mmap(
                std::ptr::null_mut(),
                bytes.len(),
                libc::PROT_WRITE | libc::PROT_READ,
                libc::MAP_SHARED,
                fd,
                0,
            );
            assert!(p != libc::MAP_FAILED);
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), p as *mut u8, bytes.len());
            libc::munmap(p, bytes.len());
            libc::close(fd);
        }
    }

    #[cfg(unix)]
    fn shm_object_exists(name: &str) -> bool {
        use std::ffi::CString;
        let c_name = CString::new(name).unwrap();
        unsafe {
            let fd = libc::shm_open(c_name.as_ptr(), libc::O_RDONLY, 0);
            if fd >= 0 {
                libc::close(fd);
                true
            } else {
                false
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn kitty_apc_t_s_reads_and_unlinks_shared_memory() {
        use base64::Engine;
        let png = kitty_png(4, 4);
        // POSIX SHM names start with `/`. Use the test PID for
        // uniqueness across parallel test runs.
        let name = format!("/yutani-shm-test-{}", std::process::id());
        // Pre-clean in case a previous failed run left it behind.
        unlink_kitty_shm(&name);
        write_shm(&name, &png);
        assert!(shm_object_exists(&name), "fixture should exist pre-test");
        let name_b64 = base64::engine::general_purpose::STANDARD.encode(&name);

        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        t.feed(&format!("\x1b_Ga=T,f=100,t=s,c=2,r=1;{}\x1b\\", name_b64));

        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1, "SHM upload should land");
        // On macOS the SHM segment rounds up to a page; the upload
        // bytes include zero padding past the PNG body. Compare the
        // prefix and let `peek_dimensions` confirm the PNG decoded.
        assert_eq!(&uploads[0].bytes[..png.len()], png.as_slice());
        assert_eq!(uploads[0].pixel_size, Some((4, 4)));
        assert!(!shm_object_exists(&name), "t=s must shm_unlink after read");
    }

    #[cfg(unix)]
    #[test]
    fn kitty_apc_t_s_unlinks_even_on_read_failure() {
        use base64::Engine;
        // Reference a nonexistent SHM name. The read fails (shm_open
        // returns ENOENT) but we still call shm_unlink — testing
        // that this doesn't panic or leave anything weird behind.
        let name = format!("/yutani-shm-missing-{}", std::process::id());
        unlink_kitty_shm(&name); // belt-and-braces
        let name_b64 = base64::engine::general_purpose::STANDARD.encode(&name);
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        t.feed(&format!("\x1b_Ga=T,f=100,t=s,c=1,r=1;{}\x1b\\", name_b64));
        assert!(t.take_pending_image_uploads().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn kitty_apc_t_s_with_raw_rgb_pngencodes_on_read() {
        // icat's preferred path for huge JPGs: raw RGB via SHM. The
        // SHM segment contains raw bytes; we read, PNG-encode, queue.
        use base64::Engine;
        let w = 4u32;
        let h = 4u32;
        let raw: Vec<u8> = (0..(w * h * 3) as u8).collect();
        let name = format!("/yutani-shm-raw-{}", std::process::id());
        unlink_kitty_shm(&name);
        write_shm(&name, &raw);
        let name_b64 = base64::engine::general_purpose::STANDARD.encode(&name);
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        t.feed(&format!(
            "\x1b_Ga=T,f=24,t=s,s={},v={},c=2,r=1;{}\x1b\\",
            w, h, name_b64
        ));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        assert_eq!(uploads[0].pixel_size, Some((w, h)));
        // Raw RGB now takes the worker-bypass path: bytes stay as
        // raw RGBA (alpha-padded), `raw_rgba_dims` carries the dims
        // for the store-side upload.
        assert_eq!(uploads[0].raw_rgba_dims, Some((w, h)));
        assert_eq!(uploads[0].bytes.len() as u32, w * h * 4);
        assert!(!shm_object_exists(&name));
    }

    //
    // K2: temp-file transmission (`t=t`). Same shape as `t=f` but
    // deletes the file after read, gated on the path being under
    // `std::env::temp_dir()`. This is what icat prefers for large
    // images — one APC + one file read instead of ~250 chunked APCs.
    //

    #[test]
    fn kitty_apc_t_t_reads_and_deletes_temp_file() {
        use base64::Engine;
        let png = kitty_png(4, 4);
        let dir = std::env::temp_dir();
        let path = dir.join(format!("yutani-kitty-t-test-{}.png", std::process::id()));
        std::fs::write(&path, &png).expect("write temp png");
        let path_b64 = base64::engine::general_purpose::STANDARD.encode(path.to_str().unwrap());
        // Sanity: file exists before the APC.
        assert!(path.exists(), "fixture should exist pre-test");

        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        t.feed(&format!("\x1b_Ga=T,f=100,t=t,c=2,r=1;{}\x1b\\", path_b64));

        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1, "temp-file upload should land");
        assert_eq!(uploads[0].bytes, png);
        // Per spec the terminal owns the unlink — file should be gone.
        assert!(!path.exists(), "t=t must delete the file after read");
    }

    #[test]
    fn kitty_apc_t_t_does_not_delete_files_outside_temp_dir() {
        // Defense-in-depth: even when the app asks for `t=t`, we only
        // delete files that actually live under temp_dir. A path
        // pointing outside is read (no privilege escalation — the app
        // could read it itself) but left in place.
        use base64::Engine;
        let png = kitty_png(2, 2);
        // Use the cargo target dir (definitely not under temp_dir) so
        // the test doesn't depend on writeable /tmp behaviour.
        let dir = std::env::current_dir().unwrap().join("target");
        std::fs::create_dir_all(&dir).ok();
        let path = dir.join(format!("yutani-outside-temp-{}.png", std::process::id()));
        std::fs::write(&path, &png).expect("write fixture");
        let path_b64 = base64::engine::general_purpose::STANDARD.encode(path.to_str().unwrap());

        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        t.feed(&format!("\x1b_Ga=T,f=100,t=t,c=1,r=1;{}\x1b\\", path_b64));

        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1, "non-temp file should still be read");
        assert!(path.exists(), "non-temp file must NOT be deleted");

        // Clean up after ourselves since the terminal won't.
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn kitty_apc_t_t_with_raw_rgb_reads_and_pngencodes() {
        // icat's preferred path for big JPGs: f=24 (raw RGB) over t=t
        // (temp file). The temp file contains raw `s*v*3` bytes; we
        // PNG-encode on read so the downstream decoder sees a PNG.
        use base64::Engine;
        let w = 4u32;
        let h = 4u32;
        let raw: Vec<u8> = (0..(w * h * 3) as u8).collect();
        let path = std::env::temp_dir().join(format!("yutani-raw-t-test-{}.rgb", std::process::id()));
        std::fs::write(&path, &raw).expect("write fixture");
        let path_b64 = base64::engine::general_purpose::STANDARD.encode(path.to_str().unwrap());

        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        t.feed(&format!(
            "\x1b_Ga=T,f=24,t=t,s={},v={},c=2,r=1;{}\x1b\\",
            w, h, path_b64
        ));

        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        assert_eq!(uploads[0].pixel_size, Some((w, h)));
        // Raw RGB takes the worker-bypass path; bytes are RGBA
        // (alpha-padded), not PNG re-encoded.
        assert_eq!(uploads[0].raw_rgba_dims, Some((w, h)));
        assert_eq!(uploads[0].bytes.len() as u32, w * h * 4);
        assert!(!path.exists(), "temp file should be deleted");
    }

    #[test]
    fn kitty_apc_query_replies_ok_for_t_t_temp_file() {
        // Capability handshake: must advertise `t=t` so icat will use
        // it instead of falling back to ~250 chunked direct APCs.
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&kitty_apc_control_only("a=q,i=8,f=100,t=t,s=1,v=1"));
        assert_eq!(t.take_response(), b"\x1b_Gi=8;OK\x1b\\");
        // And for raw RGB over temp file (the big-JPG fast path).
        t.feed(&kitty_apc_control_only("a=q,i=9,f=24,t=t,s=1,v=1"));
        assert_eq!(t.take_response(), b"\x1b_Gi=9;OK\x1b\\");
    }

    #[test]
    fn kitty_apc_anonymous_chunked_transmission_assembles() {
        // kitty +kitten icat omits `i=` on its chunked transmissions —
        // the spec says chunked MUST have an id, but reality differs.
        // Use the `kitty_chunks_anon` slot to thread chunks together.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        let png = kitty_png(4, 4);
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        let mid = b64.len() / 2;
        let (c1, c2) = b64.split_at(mid);

        // First chunk: m=1, no `i=`. Carries the sizing.
        t.feed(&format!("\x1b_Ga=T,q=2,f=100,m=1,c=2,r=1;{}\x1b\\", c1));
        assert!(t.take_pending_image_uploads().is_empty(),
            "first anon chunk must not flush");
        // Terminal chunk: m=0 (or omitted), no `i=`.
        t.feed(&format!("\x1b_Ga=T,m=0;{}\x1b\\", c2));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1, "anonymous chunks should coalesce");
        assert_eq!(uploads[0].cell_extent, (1, 2));
        assert_eq!(uploads[0].pixel_size, Some((4, 4)));
    }

    #[test]
    fn kitty_apc_anonymous_chunked_does_not_collide_with_id_keyed() {
        // Anonymous and id-keyed chunked transmissions use separate
        // slots so they can be in-flight at the same time. Pin that
        // by interleaving and verifying both land independently.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        let png_anon = kitty_png(2, 2);
        let png_keyed = kitty_png(3, 3);
        use base64::Engine;
        let b_anon = base64::engine::general_purpose::STANDARD.encode(&png_anon);
        let b_keyed = base64::engine::general_purpose::STANDARD.encode(&png_keyed);

        let (a1, a2) = b_anon.split_at(b_anon.len() / 2);
        let (k1, k2) = b_keyed.split_at(b_keyed.len() / 2);

        t.feed(&format!("\x1b_Ga=T,f=100,m=1,c=1,r=1;{}\x1b\\", a1));
        t.feed(&format!("\x1b_Ga=T,f=100,m=1,i=99,c=2,r=2;{}\x1b\\", k1));
        t.feed(&format!("\x1b_Ga=T,i=99,m=0;{}\x1b\\", k2));
        t.feed(&format!("\x1b_Ga=T,m=0;{}\x1b\\", a2));

        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 2);
        // Both should have valid pixel data — proves the buffers didn't
        // cross-contaminate.
        for up in &uploads {
            assert!(up.pixel_size.is_some(), "decoded cleanly: {:?}", up.cell_extent);
        }
    }

    #[test]
    fn kitty_apc_raw_rgb_direct_takes_worker_bypass() {
        // What `kitty +kitten icat` does for a JPG: decode locally to
        // raw RGB, ship over t=d, count on the terminal to handle
        // f=24. Used to PNG-encode on receive; now takes the
        // worker-bypass path — bytes stay as raw RGBA (alpha-padded
        // from RGB) and `raw_rgba_dims` carries the dims for the
        // store-side upload.
        use base64::Engine;
        let w = 4u32;
        let h = 4u32;
        let rgb: Vec<u8> = (0..(w * h * 3) as u8).collect();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&rgb);

        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        t.feed(&format!(
            "\x1b_Ga=T,f=24,s={},v={},c=2,r=1;{}\x1b\\",
            w, h, b64
        ));

        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        assert_eq!(uploads[0].pixel_size, Some((w, h)));
        assert_eq!(uploads[0].raw_rgba_dims, Some((w, h)));
        assert_eq!(uploads[0].bytes.len() as u32, w * h * 4);
        // Each pixel's alpha byte was padded to 0xFF.
        for px in uploads[0].bytes.chunks_exact(4) {
            assert_eq!(px[3], 0xFF);
        }
    }

    #[test]
    fn kitty_apc_raw_rgba_direct_takes_worker_bypass() {
        use base64::Engine;
        let w = 2u32;
        let h = 2u32;
        let rgba: Vec<u8> = (0..(w * h * 4) as u8).collect();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&rgba);

        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        t.feed(&format!(
            "\x1b_Ga=T,f=32,s={},v={};{}\x1b\\",
            w, h, b64
        ));

        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        assert_eq!(uploads[0].pixel_size, Some((w, h)));
        assert_eq!(uploads[0].raw_rgba_dims, Some((w, h)));
        // Bytes passed through unchanged.
        assert_eq!(uploads[0].bytes, rgba);
    }

    #[test]
    fn kitty_apc_raw_format_with_mismatched_byte_count_drops() {
        // s*v*3 mismatch — declared 4x4 RGB (48 bytes) but payload is
        // only 12. Reject so a buggy app can't crash the decoder.
        use base64::Engine;
        let too_few = base64::engine::general_purpose::STANDARD.encode(vec![0u8; 12]);
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        t.feed(&format!("\x1b_Ga=T,f=24,s=4,v=4;{}\x1b\\", too_few));
        assert!(t.take_pending_image_uploads().is_empty());
    }

    #[test]
    fn kitty_apc_raw_format_without_source_dims_drops() {
        // s= / v= are required for raw — there's no header to fall back on.
        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD.encode(vec![0u8; 48]);
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        t.feed(&format!("\x1b_Ga=T,f=24;{}\x1b\\", bytes));
        assert!(t.take_pending_image_uploads().is_empty());
    }

    #[test]
    fn parse_kitty_control_t_f_is_file() {
        assert_eq!(
            parse_kitty_control("t=f").unwrap().transmission,
            KittyTransmission::File
        );
    }

    #[test]
    fn kitty_apc_transmit_only_queues_upload_with_display_false() {
        // `a=t` (lowercase) — transmit-only, hold for a later `a=p`.
        // The upload IS queued (so the decode runs and the store gets
        // the pixels), but with display_immediately=false so main.rs
        // skips placement creation. Cursor stays put.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        t.feed("\x1b[5;1H"); // cursor at row 5 col 1
        let cursor_before = t.cursor().row;
        let png = kitty_png(4, 4);
        t.feed(&kitty_apc("a=t,f=100,c=2,r=1,i=1", &png));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        assert!(!uploads[0].display_immediately);
        assert_eq!(uploads[0].kitty_image_id, Some(1));
        // `a=t` MUST NOT advance the cursor — the placement happens
        // later via `a=p` and that's what moves the cursor.
        assert_eq!(t.cursor().row, cursor_before);
    }

    //
    // K5: virtual placements (U=1 + U+10EEEE placeholder cells)
    //

    /// Build the SGR truecolor escape for an image-id placeholder. The
    /// 24-bit id maps to RGB as (high, mid, low) bytes — matches what
    /// `decode_kitty_placeholder_image_id` reverses.
    fn placeholder_sgr_fg(image_id: u32) -> String {
        let r = ((image_id >> 16) & 0xFF) as u8;
        let g = ((image_id >> 8) & 0xFF) as u8;
        let b = (image_id & 0xFF) as u8;
        format!("\x1b[38;2;{};{};{}m", r, g, b)
    }

    #[test]
    fn parse_kitty_control_u_one_sets_virtual_placement() {
        let c = parse_kitty_control("U=1,i=1,a=T").unwrap();
        assert!(c.virtual_placement);
        let c = parse_kitty_control("a=T,i=1").unwrap();
        assert!(!c.virtual_placement, "default is false");
    }

    #[test]
    fn kitty_apc_virtual_placement_transmits_without_displaying() {
        // a=T,U=1,i=N: image goes into the store + kitty_image_ids
        // map, but no Placement is created. The cursor doesn't move
        // either — placeholders position it later.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        t.feed("\x1b[5;1H");
        let cursor_before = t.cursor().row;
        let png = kitty_png(4, 4);
        t.feed(&kitty_apc("a=T,U=1,f=100,i=42", &png));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        assert!(!uploads[0].display_immediately, "U=1 must not display");
        assert_eq!(uploads[0].kitty_image_id, Some(42));
        // No placement; cursor put.
        assert!(t.live_placements().is_empty());
        assert_eq!(t.cursor().row, cursor_before);
    }

    #[test]
    fn placeholder_cells_record_image_id_from_fg_color() {
        // SGR truecolor encodes a 24-bit id; print one placeholder
        // and read the cell back.
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&placeholder_sgr_fg(0xABCDEF));
        t.feed("\u{10EEEE}");
        let cell = t.extended_cell(0, 0).unwrap();
        assert_eq!(cell.placeholder_image_id, Some(0xABCDEF));
    }

    #[test]
    fn placeholder_cells_without_fg_color_have_no_image_id() {
        // Without an SGR fg, the cell's fg defaults to None — there's
        // no id to extract. Pin the contract so an accidental
        // placeholder doesn't act on whatever color was last printed.
        let mut t = Terminal::new(80, 24, 100);
        t.feed("\u{10EEEE}");
        let cell = t.extended_cell(0, 0).unwrap();
        assert_eq!(cell.placeholder_image_id, None);
    }

    #[test]
    fn placeholder_cells_with_zero_id_treated_as_no_id() {
        // (0, 0, 0) is the sentinel "no id" color. Print one such
        // placeholder and verify it's not picked up.
        let mut t = Terminal::new(80, 24, 100);
        t.feed("\x1b[38;2;0;0;0m");
        t.feed("\u{10EEEE}");
        let cell = t.extended_cell(0, 0).unwrap();
        assert_eq!(cell.placeholder_image_id, None);
    }

    #[test]
    fn placeholder_runs_single_full_row_collapses_to_one_run() {
        // Five placeholder cells in one screen row, each with the
        // same image_row (0) and consecutive image_col (0..5) — the
        // shape a normal `kitten icat` tiling produces. One run.
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&placeholder_sgr_fg(7));
        t.feed("\x1b[3;6H"); // row 2 (0-based), col 5 (0-based)
        for col in 0..5u32 {
            let col_dia = KITTY_PLACEHOLDER_DIACRITICS[col as usize];
            t.feed("\u{10EEEE}\u{0305}"); // row diacritic = index 0
            let mut s = String::new();
            s.push(col_dia);
            t.feed(&s);
        }
        let runs = t.kitty_placeholder_runs();
        assert_eq!(runs.len(), 1);
        let r = &runs[0];
        assert_eq!(r.client_id, 7);
        assert_eq!(r.screen_row, 2);
        assert_eq!((r.screen_col_start, r.screen_col_end), (5, 10));
        assert_eq!(r.image_row, 0);
        assert_eq!((r.image_col_start, r.image_col_end), (0, 5));
    }

    #[test]
    fn placeholder_runs_multi_row_block_emits_one_run_per_screen_row() {
        // 3×2 grid of placeholders. Each screen row carries a
        // different image_row diacritic. Output: 2 runs (one per
        // screen row), each 3 cells wide.
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&placeholder_sgr_fg(7));
        let row0 = KITTY_PLACEHOLDER_DIACRITICS[0]; // image_row = 0
        let row1 = KITTY_PLACEHOLDER_DIACRITICS[1]; // image_row = 1
        // Screen row 2.
        t.feed("\x1b[3;6H");
        for col in 0..3u32 {
            let col_dia = KITTY_PLACEHOLDER_DIACRITICS[col as usize];
            t.feed("\u{10EEEE}");
            let mut s = String::new();
            s.push(row0);
            s.push(col_dia);
            t.feed(&s);
        }
        // Screen row 3.
        t.feed("\x1b[4;6H");
        for col in 0..3u32 {
            let col_dia = KITTY_PLACEHOLDER_DIACRITICS[col as usize];
            t.feed("\u{10EEEE}");
            let mut s = String::new();
            s.push(row1);
            s.push(col_dia);
            t.feed(&s);
        }
        let runs = t.kitty_placeholder_runs();
        assert_eq!(runs.len(), 2, "one run per screen row");
        assert_eq!(runs[0].screen_row, 2);
        assert_eq!(runs[0].image_row, 0);
        assert_eq!(runs[1].screen_row, 3);
        assert_eq!(runs[1].image_row, 1);
    }

    #[test]
    fn placeholder_runs_two_distinct_image_ids_produce_two_runs() {
        // Adjacent cells encoding different ids must not merge.
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&placeholder_sgr_fg(7));
        t.feed("\x1b[1;1H");
        t.feed("\u{10EEEE}\u{0305}\u{0305}"); // id=7, row=0, col=0
        t.feed("\u{10EEEE}\u{0305}\u{030D}"); // id=7, row=0, col=1
        t.feed(&placeholder_sgr_fg(9));
        t.feed("\u{10EEEE}\u{0305}\u{0305}"); // id=9, row=0, col=0
        let runs = t.kitty_placeholder_runs();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].client_id, 7);
        assert_eq!((runs[0].screen_col_start, runs[0].screen_col_end), (0, 2));
        assert_eq!(runs[1].client_id, 9);
        assert_eq!((runs[1].screen_col_start, runs[1].screen_col_end), (2, 3));
    }

    #[test]
    fn placeholder_runs_break_on_image_col_gap() {
        // Cells at image_col 0, 1, then 3 (skipping 2) must split
        // into two runs. Otherwise the renderer would stretch
        // image_col 0..2 over a 3-cell span and skip image_col 2.
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&placeholder_sgr_fg(7));
        t.feed("\x1b[1;1H");
        let col0 = KITTY_PLACEHOLDER_DIACRITICS[0];
        let col1 = KITTY_PLACEHOLDER_DIACRITICS[1];
        let col3 = KITTY_PLACEHOLDER_DIACRITICS[3];
        for col in [col0, col1, col3] {
            t.feed("\u{10EEEE}");
            let mut s = String::new();
            s.push(KITTY_PLACEHOLDER_DIACRITICS[0]); // image_row = 0
            s.push(col);
            t.feed(&s);
        }
        let runs = t.kitty_placeholder_runs();
        assert_eq!(runs.len(), 2);
        assert_eq!((runs[0].image_col_start, runs[0].image_col_end), (0, 2));
        assert_eq!((runs[1].image_col_start, runs[1].image_col_end), (3, 4));
    }

    #[test]
    fn placeholder_runs_break_on_image_row_change_within_screen_row() {
        // Adjacent cells with different image_row diacritics start
        // separate runs. (Pathological — an encoder doesn't normally
        // do this — but it pins the contract.)
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&placeholder_sgr_fg(7));
        t.feed("\x1b[1;1H");
        t.feed("\u{10EEEE}\u{0305}\u{0305}"); // row 0
        t.feed("\u{10EEEE}\u{030D}\u{030D}"); // row 1
        let runs = t.kitty_placeholder_runs();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].image_row, 0);
        assert_eq!(runs[1].image_row, 1);
    }

    #[test]
    fn placeholder_runs_break_on_non_placeholder_cell() {
        // A plain character cell between placeholders splits the
        // run. The renderer should draw the left and right halves
        // as separate quads (each with the correct UV slice) so
        // the text shows through the gap.
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&placeholder_sgr_fg(7));
        t.feed("\x1b[1;1H");
        t.feed("\u{10EEEE}\u{0305}\u{0305}"); // col 0
        t.feed("\x1b[39mX"); // plain glyph
        t.feed(&placeholder_sgr_fg(7));
        t.feed("\u{10EEEE}\u{0305}\u{030E}"); // col 2
        let runs = t.kitty_placeholder_runs();
        assert_eq!(runs.len(), 2);
        assert_eq!((runs[0].screen_col_start, runs[0].screen_col_end), (0, 1));
        assert_eq!((runs[1].screen_col_start, runs[1].screen_col_end), (2, 3));
    }

    #[test]
    fn placeholder_runs_empty_when_no_placeholders() {
        let mut t = Terminal::new(80, 24, 100);
        t.feed("hello world");
        assert!(t.kitty_placeholder_runs().is_empty());
    }

    #[test]
    fn placeholder_runs_survive_scrolling_into_scrollback() {
        // Regression: after the image scrolls off the top and the
        // user scrolls back up to view it, the placeholder cells
        // still need to be found by the scanner. Pre-fix the scanner
        // only walked `active_grid()`, so once the cells migrated to
        // the scrollback ring the image silently disappeared
        // (visible as the space where the image had been but blank).
        //
        // Now `kitty_placeholder_runs` walks via `extended_cell`
        // which transparently picks up scrollback rows when
        // `view_offset > 0`. The returned run's `screen_row` is the
        // negative visual-row coord those scrollback cells now
        // occupy.
        let mut t = Terminal::new(20, 5, 100);
        // Paint a single placeholder row at the top.
        t.feed(&placeholder_sgr_fg(7));
        t.feed("\x1b[1;1H");
        t.feed("\u{10EEEE}\u{0305}\u{0305}"); // row 0, col 0 of image
        // Sanity: the run exists at visual row 0 with no scroll.
        let runs = t.kitty_placeholder_runs();
        assert_eq!(runs.len(), 1, "fixture: placeholder is on row 0");
        assert_eq!(runs[0].screen_row, 0);

        // Scroll the placeholder well past the phantom-row window so
        // it's fully out of the cell renderer's reach. With rows=5
        // and the 2-row phantom strip above, we need at least 8
        // line feeds to push the placeholder past visual_row=-2.
        for _ in 0..10 {
            t.feed("\r\n");
        }
        assert!(
            t.scrollback.len() >= 5,
            "fixture: placeholder should have scrolled well off the top, sb={}",
            t.scrollback.len(),
        );
        assert!(
            t.kitty_placeholder_runs().is_empty(),
            "placeholder is past the phantom-row window with view_offset=0",
        );

        // Pull the scrollback into view. With view_offset = sb_len,
        // the scrolled-off content fills the top of the viewport
        // and the placeholder lands on a positive visual row.
        let sb_len = t.scrollback.len();
        t.scroll_up(sb_len);
        let runs = t.kitty_placeholder_runs();
        assert_eq!(
            runs.len(),
            1,
            "placeholder must re-appear once scrolled back into view",
        );
        assert!(
            (0..t.rows as isize).contains(&runs[0].screen_row),
            "screen_row must land on a visible viewport row: got {}",
            runs[0].screen_row,
        );
    }

    #[test]
    fn placeholder_runs_partial_overwrite_shows_remainder_at_original_scale() {
        // The regression that motivates this whole shape: a 3-cell
        // run gets its first cell overwritten by ordinary text.
        // The remaining 2 cells still encode `image_col` 1..3, so
        // the renderer draws the right two-thirds of the image
        // (against the original `c=3` denominator) — NOT the whole
        // image stretched into a 2-cell rect. This test only proves
        // the data is preserved; the UV math lives in main.rs.
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&placeholder_sgr_fg(7));
        t.feed("\x1b[1;1H");
        for col in 0..3u32 {
            let col_dia = KITTY_PLACEHOLDER_DIACRITICS[col as usize];
            t.feed("\u{10EEEE}\u{0305}"); // image_row 0
            let mut s = String::new();
            s.push(col_dia);
            t.feed(&s);
        }
        // Overwrite the first cell with a regular character.
        t.feed("\x1b[1;1H");
        t.feed("\x1b[39mX");
        let runs = t.kitty_placeholder_runs();
        assert_eq!(runs.len(), 1, "surviving cells form one contiguous run");
        let r = &runs[0];
        assert_eq!((r.screen_col_start, r.screen_col_end), (1, 3));
        assert_eq!(
            (r.image_col_start, r.image_col_end),
            (1, 3),
            "image-col data preserved so the UV samples the right portion",
        );
    }

    #[test]
    // T and U1 name the kitty action (a=T) and unicode-placement (U=1) the
    // test exercises; keep them capitalized to match the protocol.
    #[allow(non_snake_case)]
    fn kitty_image_cell_extent_recorded_on_a_T_U1_finalize() {
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        let png = kitty_png(4, 4);
        t.feed(&kitty_apc("a=T,U=1,f=100,c=29,r=15,i=43", &png));
        // Drain the upload (irrelevant); the side-effect we care
        // about is the cached extent.
        let _ = t.take_pending_image_uploads();
        assert_eq!(t.kitty_image_cell_extent(43), Some((29, 15)));
    }

    #[test]
    fn kitty_image_cell_extent_missing_when_c_or_r_omitted() {
        // Without both `c=` and `r=` we can't define a tiling and
        // the renderer would have no UV denominator — record None.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        let png = kitty_png(4, 4);
        t.feed(&kitty_apc("a=T,U=1,f=100,c=29,i=44", &png));
        let _ = t.take_pending_image_uploads();
        assert_eq!(t.kitty_image_cell_extent(44), None);
    }

    #[test]
    fn kitty_image_cell_extent_cleared_on_a_d_i() {
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        let png = kitty_png(4, 4);
        t.feed(&kitty_apc("a=T,U=1,f=100,c=29,r=15,i=43", &png));
        let _ = t.take_pending_image_uploads();
        t.register_kitty_image_id(43, ImageId(1));
        assert_eq!(t.kitty_image_cell_extent(43), Some((29, 15)));
        t.feed(&kitty_apc_control_only("a=d,d=i,i=43"));
        assert_eq!(t.kitty_image_cell_extent(43), None);
    }

    #[test]
    fn kitty_image_cell_extent_cleared_on_a_d_a() {
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        let png = kitty_png(4, 4);
        t.feed(&kitty_apc("a=T,U=1,f=100,c=29,r=15,i=43", &png));
        let _ = t.take_pending_image_uploads();
        t.feed(&kitty_apc_control_only("a=d,d=a"));
        assert_eq!(t.kitty_image_cell_extent(43), None);
    }

    #[test]
    fn placeholder_diacritics_attach_to_previous_cell_not_their_own() {
        // Regression for the tmux unicode-placeholder bug: each cell
        // in the placeholder grid is U+10EEEE followed by combining
        // diacritics encoding (row, col). If the diacritics land in
        // their own cells they show up as glyph-less "tofu" because
        // most of those codepoints have no rasterized glyph.
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&placeholder_sgr_fg(0x111111));
        // U+10EEEE + 1st diacritic (row 0) + 2nd diacritic (col 0).
        t.feed("\u{10EEEE}\u{0305}\u{0305}");
        let cell0 = t.extended_cell(0, 0).unwrap();
        assert_eq!(cell0.placeholder_image_id, Some(0x111111));
        assert_eq!(cell0.placeholder_image_row, 0);
        assert_eq!(cell0.placeholder_image_col, 0);
        // Diacritics MUST NOT have landed in cells 1 and 2.
        let cell1 = t.extended_cell(0, 1).unwrap();
        let cell2 = t.extended_cell(0, 2).unwrap();
        assert_eq!(cell1.ch, ' ', "diacritic 1 must not occupy its own cell");
        assert_eq!(cell2.ch, ' ', "diacritic 2 must not occupy its own cell");
    }

    #[test]
    fn placeholder_diacritics_decode_row_and_column() {
        // Second diacritic in the kitty table → row 1; fourth → col 3.
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&placeholder_sgr_fg(0xCAFE00));
        t.feed("\u{10EEEE}\u{030D}\u{0310}"); // row=1, col=3
        let cell = t.extended_cell(0, 0).unwrap();
        assert_eq!(cell.placeholder_image_row, 1);
        assert_eq!(cell.placeholder_image_col, 3);
    }

    #[test]
    fn placeholder_third_diacritic_extends_image_id_high_byte() {
        // Third diacritic encodes the high byte (bits 24..31) of the
        // image id. The low 24 bits come from the FG truecolor; the
        // high byte adds the 25..32 bits without disturbing them.
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&placeholder_sgr_fg(0x00ABCDEF)); // low 24 bits
        // 1st='\u{0305}'→row 0, 2nd='\u{0305}'→col 0, 3rd='\u{030D}'→high=1
        t.feed("\u{10EEEE}\u{0305}\u{0305}\u{030D}");
        let cell = t.extended_cell(0, 0).unwrap();
        assert_eq!(cell.placeholder_image_id, Some(0x01ABCDEF));
    }

    #[test]
    fn placeholder_diacritic_absorption_clears_on_non_diacritic_char() {
        // After printing a real glyph, the absorption state must
        // reset — subsequent diacritics belong to that glyph (or
        // nothing), NOT the prior placeholder.
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&placeholder_sgr_fg(0x222222));
        t.feed("\u{10EEEE}");
        t.feed("a"); // breaks the placeholder absorption window
        t.feed("\u{0305}"); // attaches to 'a' (grapheme cluster), not the placeholder
        let cell0 = t.extended_cell(0, 0).unwrap();
        let cell1 = t.extended_cell(0, 1).unwrap();
        assert_eq!(cell0.placeholder_image_id, Some(0x222222));
        // The diacritic clusters onto the real glyph 'a', not the placeholder.
        assert_eq!(cell1.ch, 'a');
        assert_eq!(t.cluster_str(cell1.cluster.unwrap()).unwrap(), "a\u{0305}");
    }

    #[test]
    fn placeholder_diacritics_after_three_stop_being_absorbed() {
        // The protocol allows at most 3 diacritics after each
        // U+10EEEE. A fourth diacritic must be treated like any
        // other character (lands in its own cell here).
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&placeholder_sgr_fg(0x333333));
        t.feed("\u{10EEEE}\u{0305}\u{0305}\u{0305}\u{0305}");
        let cell0 = t.extended_cell(0, 0).unwrap();
        let cell1 = t.extended_cell(0, 1).unwrap();
        assert_eq!(cell0.placeholder_image_id, Some(0x00333333));
        assert_eq!(cell1.ch, '\u{0305}', "4th diacritic falls through");
    }

    #[test]
    fn placeholder_full_row_with_diacritics_keeps_cursor_aligned() {
        // Pin the regression that motivated all of this: a row of
        // placeholders (each U+10EEEE + 2 diacritics) leaves the
        // cursor exactly where it would be without diacritics. If
        // absorption were broken the cursor would land further right
        // and subsequent text would wrap.
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&placeholder_sgr_fg(0x444444));
        // 5 placeholder cells, each "U+10EEEE + row + col" diacritics.
        for col in 0..5u32 {
            let col_dia = KITTY_PLACEHOLDER_DIACRITICS[col as usize];
            t.feed("\u{10EEEE}\u{0305}");
            let mut s = String::new();
            s.push(col_dia);
            t.feed(&s);
        }
        // Cursor must be at col 5 (one per placeholder), not 15
        // (one per placeholder + diacritic + diacritic).
        assert_eq!(t.cursor().col, 5);
    }

    #[test]
    fn kitty_placeholder_diacritic_index_lookup_round_trips() {
        // Sanity: first → 0, second → 1, last → 296. Anything not in
        // the table returns None.
        assert_eq!(kitty_placeholder_diacritic_index('\u{0305}'), Some(0));
        assert_eq!(kitty_placeholder_diacritic_index('\u{030D}'), Some(1));
        assert_eq!(kitty_placeholder_diacritic_index('\u{1D244}'), Some(296));
        assert_eq!(kitty_placeholder_diacritic_index('a'), None);
        assert_eq!(kitty_placeholder_diacritic_index('\u{10EEEE}'), None);
    }

    #[test]
    fn kitty_placeholder_diacritic_table_is_sorted_for_binary_search() {
        // The lookup is a binary search and silently returns wrong
        // indices if the table ever ends up unsorted. Cheap structural
        // invariant — pin it so a future edit can't corrupt the lookup
        // without tripping a test.
        for pair in KITTY_PLACEHOLDER_DIACRITICS.windows(2) {
            assert!(pair[0] < pair[1], "table must be strictly ascending");
        }
        assert_eq!(KITTY_PLACEHOLDER_DIACRITICS.len(), 297);
    }

    #[test]
    fn kitty_placeholder_diacritic_index_returns_none_for_char_between_table_entries() {
        // U+0306 sits between table[0]=U+0305 and table[1]=U+030D.
        // Binary search must report "not found" rather than the bracket
        // index — a regression in the comparator would leak a Some.
        assert_eq!(kitty_placeholder_diacritic_index('\u{0306}'), None);
    }

    #[test]
    fn diacritic_with_no_prior_placeholder_lands_in_own_cell() {
        // First-char-in-the-feed diacritic: placeholder_decode is None,
        // so absorption must not engage. The diacritic prints as a
        // normal (glyph-less) cell at col 0 and the cursor advances.
        // Regression risk: an unconditional "if diacritic, absorb"
        // would silently eat the first diacritic the user types.
        let mut t = Terminal::new(80, 24, 100);
        t.feed("\u{0305}");
        let cell = t.extended_cell(0, 0).unwrap();
        assert_eq!(cell.ch, '\u{0305}');
        assert_eq!(cell.placeholder_image_id, None);
        assert_eq!(t.cursor().col, 1);
    }

    #[test]
    fn placeholder_without_fg_color_does_not_absorb_following_diacritics() {
        // U+10EEEE with no SGR fg → placeholder_image_id stays None,
        // so the print() path leaves placeholder_decode = None.
        // Subsequent diacritics MUST fall through to normal print
        // (each in its own cell), since the protocol's row/col/id-high
        // encoding has nothing to attach to.
        let mut t = Terminal::new(80, 24, 100);
        t.feed("\u{10EEEE}\u{0305}\u{030D}");
        let cell0 = t.extended_cell(0, 0).unwrap();
        let cell1 = t.extended_cell(0, 1).unwrap();
        assert_eq!(cell0.placeholder_image_id, None);
        assert_eq!(cell0.placeholder_image_row, 0);
        assert_eq!(cell0.placeholder_image_col, 0);
        // With no placeholder to absorb them, the diacritics fall through to
        // normal print: the first becomes a (degenerate) base in its own cell
        // and the second clusters onto it — they don't attach to U+10EEEE.
        assert_eq!(cell1.ch, '\u{0305}', "first diacritic starts a cell");
        assert_eq!(t.cluster_str(cell1.cluster.unwrap()).unwrap(), "\u{0305}\u{030D}");
        assert_eq!(t.cursor().col, 2);
    }

    #[test]
    fn placeholder_zero_id_fg_does_not_absorb_following_diacritics() {
        // (0,0,0) fg encodes id 0, which decode treats as "no id".
        // Same contract as the no-fg case: absorption must not engage.
        let mut t = Terminal::new(80, 24, 100);
        t.feed("\x1b[38;2;0;0;0m\u{10EEEE}\u{0305}");
        let cell0 = t.extended_cell(0, 0).unwrap();
        let cell1 = t.extended_cell(0, 1).unwrap();
        assert_eq!(cell0.placeholder_image_id, None);
        assert_eq!(cell1.ch, '\u{0305}');
    }

    #[test]
    fn placeholder_decode_state_survives_cup_between_placeholder_and_diacritic() {
        // CUP doesn't go through print(), so placeholder_decode is NOT
        // cleared by cursor movement. A diacritic after a CUP still
        // attaches to the cell the most recent U+10EEEE landed in —
        // NOT at the CUP destination. Pin this so a future "clear on
        // any cursor move" change is at least intentional and reviewed.
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&placeholder_sgr_fg(0x555555));
        t.feed("\u{10EEEE}");
        // Jump elsewhere on screen, then feed one diacritic.
        t.feed("\x1b[10;20H\u{030D}"); // table[1] → row=1
        let original = t.extended_cell(0, 0).unwrap();
        let cup_dest = t.extended_cell(9, 19).unwrap();
        assert_eq!(original.placeholder_image_row, 1, "diacritic landed on original cell");
        assert_eq!(cup_dest.ch, ' ', "diacritic did NOT land at CUP destination");
        assert_ne!(cup_dest.placeholder_image_row, 1);
    }

    #[test]
    fn resolve_kitty_format_mid_gap_with_none_fallback_defaults_to_rgb() {
        // Base-image (a=T) callers pass fallback=None. If the byte
        // count lands in the mid-gap (past rgb+PAGE, under rgba), the
        // resolver still must not hand the bytes to the PNG decoder —
        // it defaults to RGB so the raw-bypass path can run.
        let (w, h) = (450u32, 450u32);
        let rgb = (w as usize) * (h as usize) * 3;
        let mid = rgb + 16 * 1024 + 10_000;
        let payload = vec![0u8; mid];
        let resolved =
            resolve_kitty_format(KittyFormat::Png, &payload, Some(w), Some(h), None);
        assert!(matches!(resolved, KittyFormat::Rgb));
    }

    #[test]
    fn resolve_kitty_format_mid_gap_with_png_fallback_defaults_to_rgb() {
        // fallback=Some(Png) is degenerate — it means "the base was
        // also a PNG", which shouldn't happen for a mid-gap raw
        // payload. The resolver explicitly only honors RGB/RGBA
        // fallbacks; for Png it falls through to the RGB default
        // rather than ping-ponging the bytes through the PNG decoder.
        let (w, h) = (450u32, 450u32);
        let rgb = (w as usize) * (h as usize) * 3;
        let mid = rgb + 16 * 1024 + 10_000;
        let payload = vec![0u8; mid];
        let resolved = resolve_kitty_format(
            KittyFormat::Png,
            &payload,
            Some(w),
            Some(h),
            Some(KittyFormat::Png),
        );
        assert!(matches!(resolved, KittyFormat::Rgb));
    }

    #[test]
    fn resolve_kitty_format_exact_rgb_byte_count_beats_rgba_fallback() {
        // Pin the contract the existing
        // `a_f_size_inference_picks_rgb_when_byte_count_matches_w_h_3`
        // integration test depends on: when the payload is exactly
        // w*h*3 bytes, the resolver returns Rgb regardless of what
        // the base format said. Byte-count exact-match must beat
        // base-format inheritance — otherwise frames that change
        // bit-depth would be mis-decoded.
        let (w, h) = (2u32, 2u32);
        let rgb_payload = vec![0u8; (w as usize) * (h as usize) * 3];
        let resolved = resolve_kitty_format(
            KittyFormat::Png,
            &rgb_payload,
            Some(w),
            Some(h),
            Some(KittyFormat::Rgba),
        );
        assert!(matches!(resolved, KittyFormat::Rgb));
    }

    #[test]
    fn resolve_kitty_format_real_png_passthrough_ignores_fallback() {
        // The PNG signature short-circuit fires before any byte-count
        // or fallback logic. Without this, a payload that happens to
        // start with the PNG magic but whose length lands in the gap
        // would be mis-routed.
        let png = kitty_png(4, 4);
        let resolved = resolve_kitty_format(
            KittyFormat::Png,
            &png,
            Some(4),
            Some(4),
            Some(KittyFormat::Rgb),
        );
        assert!(matches!(resolved, KittyFormat::Png));
    }

    #[test]
    fn resolve_kitty_format_non_png_parsed_format_returns_as_is() {
        // When the parser already saw an explicit f=24 or f=32, the
        // resolver must not second-guess it — even if dimensions and
        // bytes would otherwise infer differently. Pin so the
        // resolver stays a narrow "fill in for PNG default" helper.
        let payload = vec![0u8; 999];
        let resolved = resolve_kitty_format(
            KittyFormat::Rgb,
            &payload,
            Some(2),
            Some(2),
            Some(KittyFormat::Rgba),
        );
        assert!(matches!(resolved, KittyFormat::Rgb));
    }

    #[test]
    fn placeholder_cells_scroll_with_grid() {
        // Placeholders ARE just cells — they move with scroll exactly
        // like any other content. After SU 1, our row=2 placeholders
        // appear at row 1.
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&placeholder_sgr_fg(42));
        t.feed("\x1b[3;1H"); // row 2 (0-based)
        t.feed("\u{10EEEE}\u{0305}\u{0305}");
        t.feed("\u{10EEEE}\u{0305}\u{030D}");
        t.feed("\x1b[1S"); // SU 1
        let runs = t.kitty_placeholder_runs();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].screen_row, 1, "row shifted from 2 → 1");
    }

    //
    // K4: image / placement ids + a=t / a=p / a=d
    //

    #[test]
    fn kitty_apc_a_p_places_previously_transmitted_image() {
        // Lifecycle test: a=t deposits the image with i=7 (no
        // placement); a=p,i=7,c=4,r=2 places it at the cursor.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        // a=t
        let png = kitty_png(4, 4);
        t.feed(&kitty_apc("a=t,f=100,c=4,r=2,i=7", &png));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        assert!(!uploads[0].display_immediately);
        // main.rs would call register_kitty_image_id here. Simulate.
        t.register_kitty_image_id(7, ImageId(99));
        // No placement yet.
        assert!(t.live_placements().is_empty());

        // a=p — place at cursor.
        t.feed("\x1b[5;1H");
        t.feed(&kitty_apc_control_only("a=p,i=7,c=4,r=2"));
        let placements = t.live_placements();
        assert_eq!(placements.len(), 1);
        assert_eq!(placements[0].image, ImageId(99));
        assert_eq!(placements[0].kitty_image_id, Some(7));
        // Cursor advanced by 2 rows (cell_extent).
        assert_eq!(t.cursor().row, 4 + 2);
    }

    #[test]
    fn kitty_apc_a_p_unknown_id_silently_drops() {
        // Per spec, placing an unknown image is a no-op.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        t.feed(&kitty_apc_control_only("a=p,i=999,c=2,r=1"));
        assert!(t.live_placements().is_empty());
    }

    #[test]
    fn kitty_apc_a_p_with_c_one_default_is_visible_placeholder() {
        // c=/r= omitted on a=p — fall back to (1,1) so the placement
        // is at least visible. The Kitty spec allows omitting c/r but
        // expects the terminal to know the image's natural cell size;
        // we don't track that yet, so (1,1) is the safer default.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        t.register_kitty_image_id(3, ImageId(50));
        t.feed(&kitty_apc_control_only("a=p,i=3"));
        let placements = t.live_placements();
        assert_eq!(placements.len(), 1);
        assert_eq!(placements[0].rows, 1);
        assert_eq!(placements[0].cols, 1);
    }

    #[test]
    fn kitty_apc_a_p_records_placement_id() {
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        t.register_kitty_image_id(5, ImageId(10));
        t.feed(&kitty_apc_control_only("a=p,i=5,p=42,c=1,r=1"));
        let placements = t.live_placements();
        assert_eq!(placements.len(), 1);
        assert_eq!(placements[0].kitty_placement_id, Some(42));
    }

    #[test]
    fn kitty_apc_a_p_with_capital_c_skips_cursor_advance() {
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        t.register_kitty_image_id(1, ImageId(1));
        t.feed("\x1b[5;1H");
        t.feed(&kitty_apc_control_only("a=p,i=1,c=4,r=3,C=1"));
        // C=1 → cursor doesn't move.
        assert_eq!(t.cursor().row, 4);
    }

    #[test]
    fn kitty_apc_a_d_by_image_removes_all_placements_for_image() {
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        t.register_kitty_image_id(7, ImageId(99));
        // Place the same image twice at different cells.
        t.feed(&kitty_apc_control_only("a=p,i=7,c=2,r=1"));
        t.feed(&kitty_apc_control_only("a=p,i=7,c=2,r=1"));
        assert_eq!(t.live_placements().len(), 2);

        t.feed(&kitty_apc_control_only("a=d,d=i,i=7"));
        assert!(t.live_placements().is_empty());
        // Mapping also gone — subsequent a=p,i=7 won't resurrect.
        assert!(t.kitty_image_id_lookup(7).is_none());
    }

    #[test]
    fn kitty_apc_a_d_by_placement_removes_only_matching() {
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        t.register_kitty_image_id(7, ImageId(99));
        t.feed(&kitty_apc_control_only("a=p,i=7,p=1,c=1,r=1"));
        t.feed(&kitty_apc_control_only("a=p,i=7,p=2,c=1,r=1"));
        t.feed(&kitty_apc_control_only("a=p,i=7,p=3,c=1,r=1"));
        assert_eq!(t.live_placements().len(), 3);

        t.feed(&kitty_apc_control_only("a=d,d=p,p=2"));
        let surviving: Vec<Option<u32>> = t
            .live_placements()
            .iter()
            .map(|p| p.kitty_placement_id)
            .collect();
        // p=2 is gone; p=1 and p=3 survive (order preserved).
        assert_eq!(surviving, vec![Some(1), Some(3)]);
        // Image mapping kept — only the placement was deleted.
        assert_eq!(t.kitty_image_id_lookup(7), Some(ImageId(99)));
    }

    #[test]
    fn kitty_apc_a_d_all_removes_kitty_only_not_iterm() {
        // a=d,d=a sweeps Kitty placements; iTerm / Cmd-Shift-I
        // placements (those without a kitty_image_id) survive.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        // iTerm-style placement (no kitty IDs).
        t.insert_placement(ImageId(1), 0, 0, 1, 1, 0);
        // Kitty placement.
        t.register_kitty_image_id(5, ImageId(50));
        t.feed(&kitty_apc_control_only("a=p,i=5,c=1,r=1"));
        assert_eq!(t.live_placements().len(), 2);

        t.feed(&kitty_apc_control_only("a=d,d=a"));
        let remaining = t.live_placements();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].kitty_image_id, None);
        // All Kitty mappings gone.
        assert!(t.kitty_image_id_lookup(5).is_none());
    }

    #[test]
    fn kitty_apc_referenced_image_ids_keeps_transmitted_only_images() {
        // `a=t` registers an image-id mapping but creates no placement.
        // The bare placement list would not reference the store id, so
        // mark-and-sweep would drop the GPU image. The map's values
        // need to make it into `referenced_image_ids` so the image
        // survives until either `a=p` references it or `a=d` clears it.
        let mut t = Terminal::new(80, 24, 100);
        t.register_kitty_image_id(7, ImageId(99));
        assert!(t.referenced_image_ids().contains(&ImageId(99)));
    }

    #[test]
    fn kitty_apc_register_idempotent_overwrites_with_new_store_id() {
        // Client retransmits the same `i=` with new pixels → mapping
        // updates to the new store id. Both the new and stale ids
        // appear in referenced until mark-and-sweep prunes the stale.
        let mut t = Terminal::new(80, 24, 100);
        t.register_kitty_image_id(7, ImageId(1));
        t.register_kitty_image_id(7, ImageId(2));
        assert_eq!(t.kitty_image_id_lookup(7), Some(ImageId(2)));
        let refs = t.referenced_image_ids();
        assert!(refs.contains(&ImageId(2)));
        // The old id is no longer reachable through the map.
        assert!(!refs.contains(&ImageId(1)));
    }

    #[test]
    fn kitty_apc_query_replies_ok_with_request_id() {
        // kitty +kitten icat probes support with `a=q,i=N` on startup.
        // For supported (format, transmission) tuples we reply
        // `\e_Gi=N;OK\e\\`. Defaults are f=100 / t=d (both supported).
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&kitty_apc_control_only("a=q,i=42,s=1,v=1"));
        assert!(t.take_pending_image_uploads().is_empty());
        let reply = t.take_response();
        assert_eq!(reply, b"\x1b_Gi=42;OK\x1b\\");
    }

    #[test]
    fn kitty_apc_query_without_id_replies_ok_idless() {
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&kitty_apc_control_only("a=q,s=1,v=1"));
        let reply = t.take_response();
        assert_eq!(reply, b"\x1b_G;OK\x1b\\");
    }

    #[test]
    fn kitty_apc_query_replies_ok_for_raw_rgb_direct() {
        // f=24 (raw RGB) over direct base64 IS supported — we PNG-encode
        // the raw bytes on the way in. icat uses this for JPG and other
        // non-PNG sources.
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&kitty_apc_control_only("a=q,i=5,f=24,s=1,v=1"));
        assert_eq!(t.take_response(), b"\x1b_Gi=5;OK\x1b\\");
    }

    #[test]
    fn kitty_apc_query_replies_enotsupported_for_raw_over_file() {
        // f=24 + t=f is a weird combo (file containing raw RGB bytes
        // with no header to know dimensions) and we don't handle it.
        // Pin the negative response.
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&kitty_apc_control_only("a=q,i=5,f=24,t=f,s=1,v=1"));
        let reply = t.take_response();
        let s = std::str::from_utf8(&reply).unwrap();
        assert!(s.starts_with("\x1b_Gi=5;ENOTSUPPORTED"), "got: {s}");
    }

    #[cfg(unix)]
    #[test]
    fn kitty_apc_query_replies_ok_for_shared_memory_on_unix() {
        // t=s is wired up on Unix (POSIX shm_open). On other targets
        // we'd reply ENOTSUPPORTED; gate the test on unix.
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&kitty_apc_control_only("a=q,i=6,f=100,t=s,s=1,v=1"));
        assert_eq!(t.take_response(), b"\x1b_Gi=6;OK\x1b\\");
    }

    #[test]
    fn kitty_apc_query_replies_ok_for_t_f_file() {
        // t=f is the path icat picks for local PNGs — it MUST be in
        // the "supported" set or the kitten won't use it.
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&kitty_apc_control_only("a=q,i=7,f=100,t=f,s=1,v=1"));
        assert_eq!(t.take_response(), b"\x1b_Gi=7;OK\x1b\\");
    }

    #[test]
    fn kitty_apc_query_quiet_modes() {
        // q=0 default → reply always. q=1 → suppress OK but still send
        // errors. q=2 → silence everything.
        let mut t = Terminal::new(80, 24, 100);

        t.feed(&kitty_apc_control_only("a=q,i=1,q=0")); // supported + q=0
        assert_eq!(t.take_response(), b"\x1b_Gi=1;OK\x1b\\");

        t.feed(&kitty_apc_control_only("a=q,i=1,q=1")); // supported + q=1
        assert!(t.take_response().is_empty(), "q=1 should suppress OK");

        // Trigger an actual error via an unknown transmission (t=x);
        // shared memory IS supported on Unix now so it's no longer
        // a reliable error trigger.
        t.feed(&kitty_apc_control_only("a=q,i=2,f=100,t=x,q=1")); // error + q=1
        let reply = t.take_response();
        assert!(
            std::str::from_utf8(&reply).unwrap().contains("ENOTSUPPORTED"),
            "q=1 must NOT suppress errors; got: {:?}",
            reply,
        );

        t.feed(&kitty_apc_control_only("a=q,i=3,f=100,t=x,q=2")); // error + q=2
        assert!(t.take_response().is_empty(), "q=2 must silence everything");
    }

    #[test]
    fn kitty_apc_bad_base64_silently_dropped() {
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        // Invalid base64 — decode fails, nothing queued.
        t.feed("\x1b_Ga=T,f=100,c=2,r=1;not!base64!\x1b\\");
        assert!(t.take_pending_image_uploads().is_empty());
    }

    #[test]
    fn kitty_apc_chunked_with_internal_whitespace_assembles() {
        // Real kitty payloads sometimes wrap base64 lines for
        // readability inside the APC. The handler strips whitespace.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        let png = kitty_png(2, 2);
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        let with_ws: String = b64
            .as_bytes()
            .chunks(8)
            .map(|s| std::str::from_utf8(s).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        t.feed(&format!("\x1b_Ga=T,f=100,c=1,r=1;{}\x1b\\", with_ws));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        assert_eq!(uploads[0].pixel_size, Some((2, 2)));
    }

    //
    // K1 gap-fill: edge cases around handle_apc, chunk lifecycle, query
    // formatting, and interactions with the iTerm path.
    //

    #[test]
    fn kitty_apc_only_g_verb_no_semicolon_does_not_panic() {
        // APC payload that's literally just "G" — no control string, no
        // payload, no `;`. Splits to ("", ""), parses to defaults (a=T),
        // f=100 PNG, t=d direct. Bare empty payload base64-decodes to
        // zero bytes; the upload is still queued (pin current behavior).
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        t.feed("\x1b_G\x1b\\");
        // Defaults are a=T,f=100,t=d → finalize fires. Empty base64 → zero
        // bytes → peek_dimensions returns None → cell_extent falls back to
        // (1, 1). Pin so a future tightening of the validator is intentional.
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        assert!(uploads[0].bytes.is_empty());
        assert_eq!(uploads[0].pixel_size, None);
    }

    #[test]
    fn kitty_apc_payload_without_g_prefix_is_silently_dropped() {
        // APC payloads not starting with `G` aren't Kitty — drop without
        // touching the upload queue or response buffer.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        t.feed("\x1b_other,a=T,f=100;ZGF0YQ==\x1b\\");
        t.feed("\x1b_X-custom\x1b\\");
        assert!(t.take_pending_image_uploads().is_empty());
        assert!(t.take_response().is_empty());
    }

    #[test]
    fn kitty_apc_empty_payload_after_semicolon_still_queues_upload() {
        // `G<ctrl>;` with empty body — base64 of "" succeeds and gives
        // zero bytes. handle_apc still pushes a pending upload; the
        // downstream Store decode is what ultimately fails. Pin the
        // current "queue first, validate later" behavior.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        t.feed("\x1b_Ga=T,f=100,c=2,r=1;\x1b\\");
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        assert!(uploads[0].bytes.is_empty(), "empty base64 → zero bytes");
        // Explicit c/r honored even when bytes are empty.
        assert_eq!(uploads[0].cell_extent, (1, 2));
    }

    #[test]
    fn kitty_apc_chunked_survives_interleaved_unrelated_apc() {
        // A non-Kitty APC arriving between chunks must not perturb the
        // accumulator keyed by image_id. Real terminals see all kinds of
        // APC payloads from misbehaving apps — this guards against a
        // future refactor that accidentally clears `kitty_chunks` on any
        // APC.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        let png = kitty_png(4, 4);
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        let mid = b64.len() / 2;

        t.feed(&format!("\x1b_Ga=T,f=100,c=2,r=1,i=11,m=1;{}\x1b\\", &b64[..mid]));
        // Unrelated APC payload — no G prefix.
        t.feed("\x1b_other-vendor-payload\x1b\\");
        // OSC sneaks in too.
        t.feed("\x1b]0;ignore me\x07");
        // Resume the same image — accumulator must still have the first half.
        t.feed(&format!("\x1b_Gi=11;{}\x1b\\", &b64[mid..]));

        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1, "interleaving must not lose the chunk buffer");
        assert_eq!(uploads[0].pixel_size, Some((4, 4)));
    }

    #[test]
    fn kitty_apc_chunked_terminator_with_no_buffer_falls_to_single_chunk() {
        // m=0 with an `i=` that has no in-flight buffer — falls through
        // to the single-chunk path. Pin: this should produce one upload
        // from the terminator's own payload (not zero, not two).
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        let png = kitty_png(2, 2);
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        t.feed(&format!("\x1b_Ga=T,f=100,c=1,r=1,i=77,m=0;{}\x1b\\", b64));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        assert_eq!(uploads[0].pixel_size, Some((2, 2)));
    }

    #[test]
    fn kitty_apc_chunks_cleared_after_flush_so_id_reuse_works() {
        // After a flush, the HashMap entry for that id is removed — so
        // a second transmission reusing the same id starts fresh and
        // gets its own first-chunk sizing (rather than inheriting the
        // prior one). Demonstrates the lifecycle without a private accessor.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        let png = kitty_png(4, 4);
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        let mid = b64.len() / 2;

        // First transmission with id=5, sized 2×1.
        t.feed(&format!("\x1b_Ga=T,f=100,c=2,r=1,i=5,m=1;{}\x1b\\", &b64[..mid]));
        t.feed(&format!("\x1b_Gi=5;{}\x1b\\", &b64[mid..]));
        let first = t.take_pending_image_uploads();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].cell_extent, (1, 2));

        // Reuse the same id with different sizing — must NOT inherit
        // the prior accumulator or its (2,1) sizing.
        t.feed(&format!("\x1b_Ga=T,f=100,c=4,r=2,i=5,m=1;{}\x1b\\", &b64[..mid]));
        t.feed(&format!("\x1b_Gi=5;{}\x1b\\", &b64[mid..]));
        let second = t.take_pending_image_uploads();
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].cell_extent, (2, 4), "second transmission's sizing wins");
    }

    #[test]
    fn kitty_apc_many_concurrent_image_ids_all_flush_independently() {
        // Interleave 5 chunked transmissions with distinct image_ids;
        // every one should flush cleanly when its terminator arrives.
        // Guards the HashMap-keyed-by-id design from a regression that
        // serializes uploads or cross-contaminates buffers.
        let mut t = Terminal::new(160, 60, 100);
        t.set_cell_size_px(8, 16);
        let png = kitty_png(4, 4);
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        let mid = b64.len() / 2;

        let ids = [101u32, 202, 303, 404, 505];
        // First-half chunks for all ids — interleaved.
        for &id in &ids {
            t.feed(&format!(
                "\x1b_Ga=T,f=100,c=2,r=1,i={},m=1;{}\x1b\\",
                id,
                &b64[..mid],
            ));
        }
        // No uploads yet — all in flight.
        assert!(t.take_pending_image_uploads().is_empty());
        // Send terminators in a different order — independence test.
        for &id in &[303, 101, 505, 202, 404] {
            t.feed(&format!("\x1b_Gi={};{}\x1b\\", id, &b64[mid..]));
        }
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 5);
        // All decoded to the original 4×4 — i.e. no buffer mixing.
        for up in &uploads {
            assert_eq!(up.pixel_size, Some((4, 4)));
        }
    }

    #[test]
    fn kitty_apc_query_explicit_q_zero_still_replies() {
        // Spec: q=0 == default == reply. Pin so a future shortcut
        // ("if q is set, suppress") doesn't silently break icat.
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&kitty_apc_control_only("a=q,i=9,q=0"));
        assert_eq!(t.take_response(), b"\x1b_Gi=9;OK\x1b\\");
    }

    #[test]
    fn kitty_apc_query_malformed_quiet_falls_back_to_zero_and_replies() {
        // `q=banana` doesn't parse — falls back to default 0, so the
        // reply fires. Pin the lenient parse contract.
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&kitty_apc_control_only("a=q,i=3,q=banana"));
        assert_eq!(t.take_response(), b"\x1b_Gi=3;OK\x1b\\");
    }

    #[test]
    fn kitty_apc_malformed_format_value_is_dropped() {
        // f=abc and f= (empty value) both fall to KittyFormat::Other,
        // which the dispatcher drops. Pin.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        let png = kitty_png(2, 2);
        t.feed(&kitty_apc("a=T,f=abc,c=1,r=1", &png));
        t.feed(&kitty_apc("a=T,f=,c=1,r=1", &png));
        assert!(t.take_pending_image_uploads().is_empty());
    }

    #[test]
    fn kitty_apc_explicit_c_and_r_fit_exactly_no_aspect_munging() {
        // K1's compute_cell_extent is called with preserve_aspect=true,
        // but when BOTH axes are explicit Cells the aspect branch is a
        // no-op — Kitty's c/r are exact cell extents. Pin so changing
        // the iTerm-shared default doesn't accidentally squish Kitty.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        // Use a wildly non-square source (32×4 px) with c=10,r=10. If
        // aspect were applied, one axis would be overridden; with both
        // explicit, we should get exactly (10, 10).
        let png = kitty_png(32, 4);
        t.feed(&kitty_apc("a=T,f=100,c=10,r=10", &png));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        assert_eq!(uploads[0].cell_extent, (10, 10));
    }

    #[test]
    fn kitty_apc_in_decstbm_scroll_region_anchor_follows_scrolls() {
        // Kitty mirror of osc_1337_in_decstbm_scroll_region_anchor_follows_scrolls.
        // Cursor pinned at scroll_bottom; an image taller than the
        // remaining region rows scrolls in-region. Anchor compensation
        // (original_row - scrolls) must still resolve to a visible row.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        t.feed("\x1b[10;20r"); // scroll region rows 10..20 (1-based)
        t.feed("\x1b[20;1H"); // cursor at row 20 (bottom of region)
        // Drain any uploads / responses from preamble (defensive).
        let _ = t.take_pending_image_uploads();
        let _ = t.take_response();
        let png = kitty_png(4, 4);
        // c=4, r=3 → 3 line-feeds at scroll_bottom → 3 in-region scrolls.
        t.feed(&kitty_apc("a=T,f=100,c=4,r=3", &png));
        assert_eq!(t.cursor().row, 19, "cursor pinned at scroll_bottom");
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        // 3 line-feeds, 0 cursor advance → scrolls = 3 - 0 = 3.
        // anchor row = 19 - 3 = 16.
        assert_eq!(uploads[0].cell_anchor, (16, 0));
        assert_eq!(uploads[0].cell_extent, (3, 4));
    }

    #[test]
    fn mixed_iterm_osc_and_kitty_apc_share_queue_in_arrival_order() {
        // Both paths push onto the same `pending_image_uploads` queue.
        // A feed containing one of each must surface both in arrival
        // order so the caller's hand-off to Store sees them as the
        // host sent them.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        // iTerm first (label = None per minimal OSC), then Kitty.
        let png = kitty_png(2, 2);
        let combo = format!("{}{}", iterm_osc(""), kitty_apc("a=T,f=100,c=1,r=1", &png));
        t.feed(&combo);
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 2);
        // Order: iTerm OSC was first → comes first.
        assert_eq!(uploads[0].label.as_deref(), None);
        assert_eq!(uploads[1].label.as_deref(), Some("kitty graphics"));
    }

    //
    // P2.3: cell-extent math + cursor advance + anchor capture.
    //

    #[test]
    fn compute_cell_extent_explicit_cells_passes_through() {
        let (rows, cols) = compute_cell_extent(
            ImageSizeSpec::Cells(10),
            ImageSizeSpec::Cells(5),
            Some((100, 50)),
            8,
            16,
            80,
            24,
            true,
        );
        assert_eq!((rows, cols), (5, 10));
    }

    #[test]
    fn compute_cell_extent_pixels_ceil_divides() {
        // 100px / 8px cell = 12.5 → ceil → 13 cells.
        let (_rows, cols) = compute_cell_extent(
            ImageSizeSpec::Pixels(100),
            ImageSizeSpec::Auto,
            Some((100, 16)),
            8,
            16,
            80,
            24,
            false,
        );
        assert_eq!(cols, 13);
    }

    #[test]
    fn compute_cell_extent_percent_of_viewport() {
        // 50% of 640px viewport = 320px → ceil-div by 8 = 40 cells.
        let (_rows, cols) = compute_cell_extent(
            ImageSizeSpec::Percent(50),
            ImageSizeSpec::Auto,
            Some((100, 16)),
            8,
            16,
            80, // viewport_cols → viewport_w = 640px
            24,
            false,
        );
        assert_eq!(cols, 40);
    }

    #[test]
    fn compute_cell_extent_auto_uses_image_dims() {
        // 32×48 image, 8×16 cells → 4 cols, 3 rows.
        let (rows, cols) = compute_cell_extent(
            ImageSizeSpec::Auto,
            ImageSizeSpec::Auto,
            Some((32, 48)),
            8,
            16,
            80,
            24,
            true,
        );
        assert_eq!((rows, cols), (3, 4));
    }

    #[test]
    fn compute_cell_extent_preserve_aspect_fills_auto_axis() {
        // width=Cells(10) (=80px), height=Auto, image is 100x50 (aspect 2:1),
        // preserve=true. height_px = 80 * 50/100 = 40 → 40/16 = 2.5 → 3 rows.
        let (rows, cols) = compute_cell_extent(
            ImageSizeSpec::Cells(10),
            ImageSizeSpec::Auto,
            Some((100, 50)),
            8,
            16,
            80,
            24,
            true,
        );
        assert_eq!((rows, cols), (3, 10));
    }

    #[test]
    fn compute_cell_extent_preserve_aspect_does_not_override_explicit() {
        // Both axes explicit → preserve is ignored. iTerm contract.
        let (rows, cols) = compute_cell_extent(
            ImageSizeSpec::Cells(10),
            ImageSizeSpec::Cells(2),
            Some((100, 100)),
            8,
            16,
            80,
            24,
            true,
        );
        assert_eq!((rows, cols), (2, 10));
    }

    #[test]
    fn compute_cell_extent_no_pixel_size_no_explicit_falls_back_to_one_cell() {
        // Worst case — format wasn't recognised by peek_dimensions and the
        // OSC didn't supply sizing. Visible-but-tiny beats a panic.
        let (rows, cols) = compute_cell_extent(
            ImageSizeSpec::Auto,
            ImageSizeSpec::Auto,
            None,
            8,
            16,
            80,
            24,
            true,
        );
        assert_eq!((rows, cols), (1, 1));
    }

    #[test]
    fn osc_1337_advances_cursor_by_image_rows() {
        let mut t = Terminal::new(80, 24, 100);
        // Set known cell size: 8×16. The fixture is a 2×2 PNG; auto sizing
        // gives 1×1 cells.
        t.set_cell_size_px(8, 16);
        // Move cursor to a known row first.
        t.feed("\x1b[5;1H"); // CUP row 5 col 1
        t.feed(&iterm_osc(";width=4;height=3")); // 3 rows × 4 cols
        // Cursor should have moved down 3 rows: row 4 (0-indexed) + 3 = 7.
        assert_eq!(t.cursor().row, 7);
        assert_eq!(t.cursor().col, 0);

        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        // Anchor was captured at the cursor's pre-LF position (row 4 since
        // CUP is 1-based: row 5 → index 4). Col 0 (CUP `;1` → index 0).
        assert_eq!(uploads[0].cell_anchor, (4, 0));
        assert_eq!(uploads[0].cell_extent, (3, 4));
    }

    #[test]
    fn osc_1337_do_not_move_cursor_skips_advance() {
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        t.feed("\x1b[5;1H"); // row 5 col 1 (1-based)
        t.feed(&iterm_osc(";width=4;height=3;doNotMoveCursor=1"));
        // Cursor stays at original position.
        assert_eq!(t.cursor().row, 4);
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads[0].cell_anchor, (4, 0));
        assert_eq!(uploads[0].cell_extent, (3, 4));
    }

    #[test]
    fn osc_1337_near_bottom_scrolls_grid_and_anchor_follows() {
        // Cursor at row 22 (1-based 23) in a 24-row grid, image is 5 rows.
        // 5 LFs from row 22: rows 22→23 is the only non-scrolling LF.
        // 4 LFs scroll. Anchor should be 22 - 4 = 18.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        t.feed("\x1b[23;1H"); // row 23 (1-based) = index 22
        t.feed(&iterm_osc(";width=4;height=5"));
        assert_eq!(t.cursor().row, 23); // pinned at bottom
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads[0].cell_anchor.0, 18);
        // The 4 scrolls also push any prior scrollback-eligible content
        // out — just sanity-check the anchor here.
    }

    //
    // P2.4: remove_placements_with_image — failure-cleanup helper.
    //

    #[test]
    fn remove_placements_with_image_drops_matching_entries_across_grids() {
        let mut t = Terminal::new(40, 10, 100);
        // Two placements referencing image 7 on primary; one referencing
        // image 8 on primary. Switch to alt and add another image-7
        // placement. Switch back, ensure remove(7) drops all three image-7
        // entries (across primary + alt) but leaves image 8 alone.
        place(&mut t, 7, 0, 0, 1, 2);
        place(&mut t, 7, 2, 0, 1, 2);
        place(&mut t, 8, 4, 0, 1, 2);
        t.feed("\x1b[?1049h");
        place(&mut t, 7, 0, 0, 1, 2);
        t.feed("\x1b[?1049l");

        let removed = t.remove_placements_with_image(ImageId(7));
        assert_eq!(removed, 3);
        // image 8 survives on primary.
        let primary_images: Vec<u32> =
            t.live_placements().iter().map(|p| p.image.0).collect();
        assert_eq!(primary_images, vec![8]);
        // alt grid is also cleared of image 7.
        t.feed("\x1b[?1049h");
        assert!(t.live_placements().is_empty());
    }

    #[test]
    fn remove_placements_with_image_clears_scrollback_entries() {
        let mut t = Terminal::new(20, 5, 100);
        place(&mut t, 99, 0, 0, 1, 2);
        // Scroll it into scrollback.
        t.feed("\x1b[1S");
        assert_eq!(t.scrollback_placements_for_test().len(), 1);
        let removed = t.remove_placements_with_image(ImageId(99));
        assert_eq!(removed, 1);
        assert!(t.scrollback_placements_for_test().is_empty());
    }

    #[test]
    fn remove_placements_with_image_unknown_id_is_no_op() {
        let mut t = Terminal::new(20, 5, 100);
        place(&mut t, 1, 0, 0, 1, 1);
        // Removing an id no placement references shouldn't touch the
        // surviving placements.
        let removed = t.remove_placements_with_image(ImageId(42));
        assert_eq!(removed, 0);
        assert_eq!(t.live_placements().len(), 1);
    }

    #[test]
    fn osc_1337_with_no_cell_size_set_still_parses_cleanly() {
        // Default cell size is 1×1 — pixel-spec produces huge cell counts
        // but the math shouldn't panic and the queue should still get an
        // entry so callers can detect the OSC arrived.
        let mut t = Terminal::new(80, 24, 100);
        // Skip set_cell_size_px deliberately.
        t.feed(&iterm_osc(";width=10;height=2"));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        assert_eq!(uploads[0].cell_extent, (2, 10));
    }

    #[test]
    fn keep_placements_in_scrollback_false_drops_on_full_screen_scroll() {
        let mut t = Terminal::new(20, 5, 100);
        t.set_keep_placements_in_scrollback(false);
        place(&mut t, 1, 0, 0, 1, 2);
        t.feed("\x1b[1S");
        // Without retention, the scrolled-off placement is dropped — not
        // promoted to scrollback_placements.
        assert!(t.live_placements().is_empty());
        assert!(t.scrollback_placements_for_test().is_empty());
    }

    #[test]
    fn keep_placements_in_scrollback_false_drops_on_resize_spill() {
        let mut t = Terminal::new(20, 10, 100);
        t.set_keep_placements_in_scrollback(false);
        place(&mut t, 1, 2, 0, 2, 4);
        // Cursor on the last row so the shrink spills the full height delta.
        t.feed("\x1b[10;1H");
        t.resize(20, 6); // spill = 4, placement at row 2 spills
        assert!(t.live_placements().is_empty());
        assert!(t.scrollback_placements_for_test().is_empty());
    }

    #[test]
    fn set_keep_placements_in_scrollback_false_clears_existing_queue() {
        let mut t = Terminal::new(20, 5, 100);
        place(&mut t, 1, 0, 0, 1, 2);
        t.feed("\x1b[1S"); // placement → scrollback_placements
        assert_eq!(t.scrollback_placements_for_test().len(), 1);
        t.set_keep_placements_in_scrollback(false);
        // Toggle drains the queue — otherwise stale entries would linger
        // until the next scrollback eviction.
        assert!(t.scrollback_placements_for_test().is_empty());
    }

    #[test]
    fn scrollback_eviction_drops_scrollback_placement_at_index_0() {
        let mut t = Terminal::new(10, 3, 2); // scrollback_limit = 2
        place(&mut t, 1, 0, 0, 1, 2);
        // Scroll twice — fills scrollback with 2 entries; our placement
        // (1 row tall, anchored at row 0) lands in scrollback at index 0
        // after the first scroll and stays put through the second.
        t.feed("\x1b[2S");
        assert_eq!(t.scrollback_placements_for_test().len(), 1);
        assert_eq!(t.scrollback_placements_for_test()[0].0, 0);
        // Third scroll: scrollback is at limit, pop_front evicts the row at
        // index 0 — placement anchored there must be dropped.
        t.feed("\x1b[1S");
        assert!(t.scrollback_placements_for_test().is_empty());
    }

    //
    // Placement helper tests (slice 2 follow-ups).
    //

    #[test]
    fn placement_fully_off_grid_each_edge() {
        // Pin every branch of the off-grid check independently. Drifting any
        // one of these to `<` vs `<=` (or `>=` vs `>`) would silently keep a
        // dead placement in the live list across scroll / resize.
        let mk = |top: isize, left: isize, rows: u16, cols: u16| Placement {
            id: 1,
            image: ImageId(1),
            top_row: top,
            left_col: left,
            rows,
            cols,
            z: 0,
            pixel_offset: (0, 0),
            src_rect: None,
            kitty_image_id: None,
            kitty_placement_id: None,
        };
        let grid_r = 10usize;
        let grid_c = 8usize;

        // Off top: bottom_row == 0 (top=-1 + rows=1).
        assert!(mk(-1, 0, 1, 1).fully_off_grid(grid_r, grid_c));
        // Boundary on top edge: bottom_row == 1 → barely visible, NOT off.
        assert!(!mk(-1, 0, 2, 1).fully_off_grid(grid_r, grid_c));

        // Off bottom: top_row == grid_rows.
        assert!(mk(grid_r as isize, 0, 1, 1).fully_off_grid(grid_r, grid_c));
        // Boundary: top_row == grid_rows - 1 → last row, still on.
        assert!(!mk(grid_r as isize - 1, 0, 1, 1).fully_off_grid(grid_r, grid_c));

        // Horizontal off-screen is intentionally NOT considered off-grid
        // (those placements come back into view if the user widens the
        // window). See the rustdoc on `fully_off_grid` and the
        // `resize_horizontal_shrink_preserves_off_screen_placements`
        // regression test.
        assert!(!mk(0, -2, 1, 2).fully_off_grid(grid_r, grid_c));
        assert!(!mk(0, grid_c as isize, 1, 1).fully_off_grid(grid_r, grid_c));

        // Fully inside is the obvious negative case.
        assert!(!mk(2, 2, 1, 1).fully_off_grid(grid_r, grid_c));
    }

    #[test]
    fn placement_rows_intersect_boundaries() {
        // Exclusive-on-top, inclusive-on-bottom semantics: a placement whose
        // bottom_row equals `top` (i.e. lives entirely in rows above the
        // region) must NOT intersect, while top_row==bottom must intersect.
        // Driving scroll-region math from here, a flip would shift the wrong
        // placements during a single-row scroll.
        let mk = |top: isize, rows: u16| Placement {
            id: 1, image: ImageId(1), top_row: top, left_col: 0, rows, cols: 1, z: 0,
            pixel_offset: (0, 0), src_rect: None,
            kitty_image_id: None, kitty_placement_id: None,
        };
        // Region [5..=9].
        // Placement at rows 3..=4 → bottom_row=5 == top → no overlap.
        assert!(!mk(3, 2).rows_intersect(5, 9));
        // Placement at rows 4..=5 → bottom_row=6 > 5 and top_row=4 <= 9 → yes.
        assert!(mk(4, 2).rows_intersect(5, 9));
        // Placement starting exactly at top of region.
        assert!(mk(5, 1).rows_intersect(5, 9));
        // Placement starting exactly at bottom of region (top_row == bottom).
        assert!(mk(9, 3).rows_intersect(5, 9));
        // Placement just past the bottom — top_row=10 > 9 → no.
        assert!(!mk(10, 1).rows_intersect(5, 9));
    }

    #[test]
    fn insert_placement_defaults_pixel_offset_and_src_rect_to_phase1_values() {
        // The original 7-arg `insert_placement` must keep producing the
        // exact same `Placement` data as before — pixel_offset zeroed and
        // src_rect None. Phase 1 callers (OSC 1337 parser, debug keybind)
        // rely on this; anything else would silently shift their draws.
        let mut t = Terminal::new(10, 10, 100);
        let id = t.insert_placement(ImageId(1), 2, 3, 4, 5, 0);
        let p = t.live_placements().iter().find(|p| p.id == id).unwrap();
        assert_eq!(p.pixel_offset, (0, 0));
        assert_eq!(p.src_rect, None);
    }

    #[test]
    fn insert_placement_with_crop_round_trips_offsets_and_rect() {
        // `insert_placement_with_crop` is the phase-2 entry point; the values
        // it accepts must survive into `live_placements()` unchanged so the
        // renderer sees what the parser produced. Single field-by-field
        // round trip pins the wiring.
        let mut t = Terminal::new(10, 10, 100);
        let id = t.insert_placement_with_crop(
            ImageId(7), 1, 2, 3, 4, 0, (5, -6), Some((10, 20, 30, 40)),
        );
        let p = t.live_placements().iter().find(|p| p.id == id).unwrap();
        assert_eq!(p.image, ImageId(7));
        assert_eq!(p.top_row, 1);
        assert_eq!(p.left_col, 2);
        assert_eq!(p.rows, 3);
        assert_eq!(p.cols, 4);
        assert_eq!(p.pixel_offset, (5, -6));
        assert_eq!(p.src_rect, Some((10, 20, 30, 40)));
    }

    #[test]
    fn pixel_offset_does_not_change_fully_off_grid() {
        // pixel_offset is a sub-cell visual nudge; it must NOT alter which
        // cells the placement covers for eviction math. If it did, a
        // placement with a +50px offset would falsely escape eviction.
        let p_zero = Placement {
            id: 1, image: ImageId(1), top_row: 5, left_col: 0,
            rows: 1, cols: 1, z: 0, pixel_offset: (0, 0), src_rect: None,
            kitty_image_id: None, kitty_placement_id: None,
        };
        let p_offset = Placement {
            pixel_offset: (999, -999), ..p_zero.clone()
        };
        assert_eq!(
            p_zero.fully_off_grid(10, 10),
            p_offset.fully_off_grid(10, 10),
        );
        // And neither should be off-grid at row 5 of a 10-row grid.
        assert!(!p_zero.fully_off_grid(10, 10));
        assert!(!p_offset.fully_off_grid(10, 10));
    }

    #[test]
    fn referenced_image_ids_is_empty_when_no_placements_anywhere() {
        // The renderer feeds this set straight into Store::retain every
        // frame; an empty union must yield an empty set (which evicts
        // everything) rather than e.g. a panic from the HashSet builder.
        let t = Terminal::new(10, 5, 100);
        assert!(t.referenced_image_ids().is_empty());
    }

    #[test]
    fn insert_lines_shifts_placements_down_within_region() {
        // IL routes through scroll_region_down with cursor.row as top — the
        // placement at row 2 should slide down to row 4. Mirrors the
        // existing direct-CSI-T test but via the higher-level IL command.
        let mut t = Terminal::new(20, 10, 100);
        place(&mut t, 1, 2, 0, 1, 4);
        // Cursor to row 1 (1-based) so IL's region top = 0 and the placement
        // at row 2 falls inside.
        t.feed("\x1b[1;1H\x1b[2L"); // CUP 1,1 then IL 2
        assert_eq!(t.live_placements().len(), 1);
        assert_eq!(t.live_placements()[0].top_row, 4);
    }

    #[test]
    fn delete_lines_shifts_placements_up_within_region() {
        // DL routes through scroll_region_up; full-width grid means the
        // shift-and-maybe-drop placement path runs. Placement at row 3
        // shifts up to row 1.
        let mut t = Terminal::new(20, 10, 100);
        place(&mut t, 1, 3, 0, 1, 4);
        t.feed("\x1b[1;1H\x1b[2M"); // CUP 1,1 then DL 2
        assert_eq!(t.live_placements().len(), 1);
        assert_eq!(t.live_placements()[0].top_row, 1);
    }

    #[test]
    fn delete_lines_drops_placement_that_fully_exits_top_of_region() {
        // DL on a full-screen region with cursor at home: a placement
        // entirely within the deleted span should fall off the top. Because
        // DL is NOT scroll_region_up_by (it doesn't push into scrollback),
        // the placement is dropped outright — not promoted.
        let mut t = Terminal::new(20, 10, 100);
        place(&mut t, 1, 0, 0, 2, 4);
        t.feed("\x1b[1;1H\x1b[3M"); // CUP 1,1 then DL 3
        assert!(t.live_placements().is_empty());
        // DL doesn't feed scrollback, so the placement is gone — not
        // promoted like a true SU would do.
        assert!(t.scrollback_placements_for_test().is_empty());
    }

    #[test]
    fn viewport_scroll_down_leaves_placements_alone() {
        // Scrolling the viewport is a pure read-side operation — placements
        // are anchored in grid coords, not visual rows, so view_offset
        // must not perturb them. Regression guard for a future "smart"
        // scroll that accidentally touches Grid::placements.
        let mut t = Terminal::new(20, 5, 100);
        place(&mut t, 1, 2, 0, 1, 2);
        // Push history into scrollback so view_offset has somewhere to go.
        t.feed("\r\nline\r\nline\r\nline\r\nline\r\nline");
        let before: Vec<_> = t
            .live_placements()
            .iter()
            .map(|p| (p.id, p.top_row, p.left_col))
            .collect();
        let _ = t.scroll_up(2);
        let _ = t.scroll_down(1);
        let after: Vec<_> = t
            .live_placements()
            .iter()
            .map(|p| (p.id, p.top_row, p.left_col))
            .collect();
        assert_eq!(before, after);
    }

    #[test]
    fn scrollback_eviction_keeps_remaining_indices_consistent() {
        // Build two scrollback placements at distinct indices, then evict the
        // oldest scrollback row. The surviving placement's index must
        // decrement (not get dropped, not stay stale) so future lookups by
        // sb row still resolve to it.
        let mut t = Terminal::new(10, 3, 4); // scrollback_limit = 4
        place(&mut t, 1, 0, 0, 1, 2); // A at row 0
        place(&mut t, 2, 1, 0, 1, 2); // B at row 1
        // First scroll: A exits to sb row 0, B slides up to live row 0.
        t.feed("\x1b[1S");
        assert_eq!(t.scrollback_placements_for_test().len(), 1);
        // Second scroll: B exits — scrollback now has 2 entries, A at sb_row 0
        // and B at sb_row 1.
        t.feed("\x1b[1S");
        let sb = t.scrollback_placements_for_test();
        assert_eq!(sb.len(), 2);
        let by_image: std::collections::HashMap<u32, isize> =
            sb.iter().map(|(row, p)| (p.image.0, *row)).collect();
        assert_eq!(by_image[&1], 0);
        assert_eq!(by_image[&2], 1);

        // Two more scrolls of empty rows fill scrollback to its limit (4).
        t.feed("\x1b[2S");
        // Now a 5th scroll forces pop_front → sb_row 0 evicted (A dropped),
        // surviving entries decrement: B should be at sb_row 0.
        t.feed("\x1b[1S");
        let sb = t.scrollback_placements_for_test();
        assert_eq!(sb.len(), 1);
        assert_eq!(sb[0].0, 0);
        assert_eq!(sb[0].1.image.0, 2);
    }

    #[test]
    fn resize_shrink_then_grow_round_trips_placement_anchor() {
        // Same placement, shrink-then-grow by the same amount — should land
        // back at (roughly) the original row. Catches a sign-flip in the
        // refill math that would otherwise only show up via the
        // visible-but-wrong-position bug.
        let mut t = Terminal::new(20, 10, 100);
        place(&mut t, 1, 2, 0, 2, 4);
        // Cursor on the last row so the shrink spills the full height delta.
        t.feed("\x1b[10;1H");
        t.resize(20, 4); // spill = 6 — placement goes to scrollback.
        assert!(t.live_placements().is_empty());
        assert_eq!(t.scrollback_placements_for_test().len(), 1);
        t.resize(20, 10);
        // Refill drains the most-recently-pushed scrollback rows back into
        // the grid; the placement at the oldest spilled row should
        // re-emerge in the live list.
        assert_eq!(t.live_placements().len(), 1);
        // Original top_row was 2; after a 6-row spill the placement landed
        // at sb_row 2, and the refill puts it back at top_row 2.
        assert_eq!(t.live_placements()[0].top_row, 2);
    }

    //
    // P2.5: compute_cell_extent edge cases — bounds, saturation, divide-by-
    // zero guards. Behavior that would silently regress to a panic or a
    // wrong-sized placement on a malformed input.
    //

    #[test]
    fn compute_cell_extent_both_pixels_with_preserve_aspect_uses_explicit() {
        // Explicit beats preserve, even when both axes are explicit and
        // would distort the aspect. iTerm contract — verified end-to-end
        // here since this is a common "looks stretched" footgun.
        let (rows, cols) = compute_cell_extent(
            ImageSizeSpec::Pixels(80),
            ImageSizeSpec::Pixels(160),
            Some((100, 100)), // square image
            8,
            16,
            80,
            24,
            true, // preserve_aspect on
        );
        // 80/8 = 10 cols, 160/16 = 10 rows. Aspect-distorted but explicit.
        assert_eq!((rows, cols), (10, 10));
    }

    #[test]
    fn compute_cell_extent_percent_zero_clamps_to_one_cell() {
        // 0% would naturally produce 0 pixels; the resolve closure has a
        // `.max(1)` so callers can't accidentally request a zero-sized
        // placement that the renderer would skip.
        let (rows, cols) = compute_cell_extent(
            ImageSizeSpec::Percent(0),
            ImageSizeSpec::Percent(0),
            Some((100, 100)),
            8,
            16,
            80,
            24,
            false,
        );
        assert_eq!((rows, cols), (1, 1));
    }

    #[test]
    fn compute_cell_extent_percent_two_hundred_oversizes_past_viewport() {
        // Percent isn't clamped to 100 — `width=200%` yields a placement
        // that extends past the right edge. The grid + renderer handle
        // clipping; the math just produces the raw extent.
        let (_rows, cols) = compute_cell_extent(
            ImageSizeSpec::Percent(200),
            ImageSizeSpec::Auto,
            Some((100, 16)),
            8,
            16,
            80, // viewport_w_px = 640
            24,
            false,
        );
        // 200% of 640 = 1280px → 1280/8 = 160 cells (2x the 80-col grid).
        assert_eq!(cols, 160);
    }

    #[test]
    fn compute_cell_extent_width_past_viewport_still_produces_extent() {
        // `Cells(1000)` on an 80-col grid: the parser doesn't clamp;
        // the grid's off-grid check handles overflow downstream. Pinning
        // that the math passes the raw count through (so future fixes
        // happen in the right place — the grid, not here).
        let (_rows, cols) = compute_cell_extent(
            ImageSizeSpec::Cells(1000),
            ImageSizeSpec::Cells(1),
            Some((100, 100)),
            8,
            16,
            80,
            24,
            false,
        );
        assert_eq!(cols, 1000);
    }

    #[test]
    fn compute_cell_extent_zero_image_dims_with_preserve_does_not_panic() {
        // Some malformed payloads have peek_dimensions returning (0, 0).
        // The preserve_aspect branch divides by ih / iw — the `if iw > 0
        // && ih > 0` guard must hold or we crash on a zero image.
        let (rows, cols) = compute_cell_extent(
            ImageSizeSpec::Auto,
            ImageSizeSpec::Cells(5),
            Some((0, 0)),
            8,
            16,
            80,
            24,
            true,
        );
        // height resolves to 5 cells; width stays Auto with no source
        // (image dim 0 → resolve returns Some(0) → ceil-div to 1).
        assert_eq!(rows, 5);
        assert_eq!(cols, 1);
    }

    #[test]
    fn compute_cell_extent_pixel_inputs_well_above_u16_clamp_to_u16_max() {
        // Pixel inputs that produce far more cells than u16 can hold must
        // clamp at u16::MAX rather than truncate. Stay below the
        // ceil-divide overflow threshold (see the should_panic test below)
        // — pick a value that still produces > 65_535 cells but doesn't
        // overflow `px + cell - 1` in the divisor path.
        let huge = 1_000_000_000u32; // 1B px / 8px-cell = 125M cells → clamps.
        let (rows, cols) = compute_cell_extent(
            ImageSizeSpec::Pixels(huge),
            ImageSizeSpec::Pixels(huge),
            None,
            8,
            16,
            80,
            24,
            false,
        );
        assert_eq!(rows, u16::MAX);
        assert_eq!(cols, u16::MAX);
    }

    /// A `Pixels(u32::MAX - 1)` spec used to overflow the `+ cell_w_px - 1`
    /// in the ceil-div (debug panic, release wrap). `saturating_add` makes
    /// the worst case clip cleanly to `u16::MAX` cells, which is the
    /// largest extent the renderer can represent.
    #[test]
    fn compute_cell_extent_pixels_near_u32_max_clamps_instead_of_overflowing() {
        let (rows, cols) = compute_cell_extent(
            ImageSizeSpec::Pixels(u32::MAX - 1),
            ImageSizeSpec::Pixels(u32::MAX - 1),
            None,
            8,
            16,
            80,
            24,
            false,
        );
        assert_eq!(rows, u16::MAX);
        assert_eq!(cols, u16::MAX);
    }

    #[test]
    fn set_cell_size_px_zero_clamps_to_one() {
        // Caller may pass 0 during a degenerate resize (e.g. window
        // minimized to a 0-px height); the OSC math would divide by zero.
        // The clamp turns that into a tiny-but-valid cell size.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(0, 0);
        // 100% percent against 80 cells of 1px each = 80 cells.
        t.feed(&iterm_osc(";width=100%;height=100%"));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        // With cell_w_px = 1 (clamped from 0), 100% of viewport = 80 cells.
        assert_eq!(uploads[0].cell_extent.1, 80);
    }

    #[test]
    fn set_cell_size_px_change_between_oscs_uses_active_value() {
        // Each OSC computes cell_extent from the cell size active at
        // parse time — a font-size change between OSCs must not retro-
        // active the prior upload.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        t.feed(&iterm_osc(";width=16px;height=16px"));
        // Bump cell size — second OSC should use the new value.
        t.set_cell_size_px(16, 32);
        t.feed(&iterm_osc(";width=16px;height=16px"));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 2);
        // First OSC: 16px / 8px-cell = 2 cols; 16px / 16px-line = 1 row.
        assert_eq!(uploads[0].cell_extent, (1, 2));
        // Second OSC: 16px / 16px-cell = 1 col; 16px / 32px-line = 1 row
        // (ceil-div of 16/32 floors to 0 then `.max(1)` brings it to 1).
        assert_eq!(uploads[1].cell_extent, (1, 1));
    }

    #[test]
    fn osc_1337_many_unknown_keys_with_known_mixed() {
        // Defensive parser: an arbitrary salad of unknown keys mixed with
        // known ones should still yield a clean upload with the known
        // values respected. Real iTerm payloads include `size=`, `type=`,
        // and others we don't model — accept-and-ignore.
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&iterm_osc(
            ";size=12345;type=image/png;futureA=1;width=7;futureB=2;height=3;futureC=hello;preserveAspectRatio=0",
        ));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        assert_eq!(uploads[0].width, ImageSizeSpec::Cells(7));
        assert_eq!(uploads[0].height, ImageSizeSpec::Cells(3));
        assert!(!uploads[0].preserve_aspect);
    }

    #[test]
    fn osc_1337_name_with_invalid_base64_leaves_label_none() {
        // `name=` is best-effort: an invalid base64 value must not abort
        // the whole OSC; the upload still arrives, just with label None.
        let mut t = Terminal::new(80, 24, 100);
        // `iterm_osc` already includes `inline=1`; append a bogus name.
        t.feed(&iterm_osc(";name=!!!notbase64!!!"));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1, "bad name must not drop the OSC");
        assert!(uploads[0].label.is_none());
    }

    #[test]
    fn osc_1337_split_across_feed_calls_still_parses() {
        // The ANSI parser is stateful per-char. An OSC arriving in two
        // (or more) feed() chunks should still produce exactly one
        // upload — the parser accumulates the OSC body until ST/BEL.
        let mut t = Terminal::new(80, 24, 100);
        let osc = iterm_osc(";width=4");
        let mid = osc.len() / 2;
        t.feed(&osc[..mid]);
        // No terminator yet — queue must be empty.
        assert!(t.take_pending_image_uploads().is_empty());
        t.feed(&osc[mid..]);
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1, "chunked OSC should reassemble into one upload");
        assert_eq!(uploads[0].width, ImageSizeSpec::Cells(4));
    }

    #[test]
    fn osc_1337_strips_internal_whitespace_in_base64() {
        // Real iTerm callers wrap base64 at 76 chars. The handler strips
        // ASCII whitespace before decode. Test it at the terminal level
        // (the e2e test in images.rs covers the GPU path too).
        let mut t = Terminal::new(80, 24, 100);
        use base64::Engine;
        let img = image::RgbaImage::from_pixel(2, 2, image::Rgba([0, 0, 0, 255]));
        let mut png: Vec<u8> = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageOutputFormat::Png)
            .unwrap();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        // Sprinkle tabs, spaces, and newlines through the payload.
        let mut wrapped = String::new();
        for (i, ch) in b64.chars().enumerate() {
            if i > 0 && i % 4 == 0 {
                wrapped.push_str("\n\t ");
            }
            wrapped.push(ch);
        }
        let osc = format!("\x1b]1337;File=inline=1:{}\x07", wrapped);
        t.feed(&osc);
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1, "whitespace in base64 must be stripped, not fail decode");
        assert_eq!(uploads[0].pixel_size, Some((2, 2)));
    }

    #[test]
    fn remove_placements_with_image_drops_multiple_placements_in_one_grid() {
        // Same image placed 3 times on a single grid (e.g. an app re-using
        // the same texture). remove_with_image must drop all 3, not just
        // the first match.
        let mut t = Terminal::new(20, 10, 100);
        place(&mut t, 5, 0, 0, 1, 2);
        place(&mut t, 5, 2, 0, 1, 2);
        place(&mut t, 5, 4, 0, 1, 2);
        place(&mut t, 6, 6, 0, 1, 2); // different image — must survive
        assert_eq!(t.live_placements().len(), 4);
        let removed = t.remove_placements_with_image(ImageId(5));
        assert_eq!(removed, 3);
        let surviving: Vec<u32> = t.live_placements().iter().map(|p| p.image.0).collect();
        assert_eq!(surviving, vec![6]);
    }

    #[test]
    fn osc_1337_in_decstbm_scroll_region_anchor_follows_scrolls() {
        // The anchor-compensation math uses cursor row-delta to count
        // scrolls, which should stay robust when a DECSTBM scroll region
        // is active. Cursor at the bottom of a [10..20] region: an image
        // taller than the remaining rows triggers in-region scrolling
        // rather than full-grid scrolling. The anchor must still resolve
        // to the original visual row.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        // DECSTBM: top=10, bottom=20 (1-based). After this, scroll_top=9,
        // scroll_bottom=19. CUP also moves cursor to home of region.
        t.feed("\x1b[10;20r");
        // Move cursor to bottom of region: row 20 (1-based) = index 19.
        t.feed("\x1b[20;1H");
        // Image with 3 rows — all 3 line-feeds at scroll_bottom will scroll
        // the in-region rows up. Cursor stays at row 19.
        t.feed(&iterm_osc(";width=4;height=3"));
        assert_eq!(t.cursor().row, 19, "cursor pinned at scroll_bottom");
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        // 3 line-feeds, 0 cursor advance → scrolls = 3 - 0 = 3. Anchor =
        // original_row 19 - 3 = 16.
        assert_eq!(uploads[0].cell_anchor, (16, 0));
        assert_eq!(uploads[0].cell_extent, (3, 4));
    }

    #[test]
    fn alt_screen_image_isolated_from_primary_referenced_ids_reflects_both() {
        // Per-grid placements + a global referenced_image_ids() union:
        // live_placements() must respect the active screen, while
        // retain()-input must keep both grids' images alive across an
        // alt-screen flip.
        let mut t = Terminal::new(40, 10, 100);
        place(&mut t, 100, 0, 0, 1, 2);
        // Switching to alt clears alt grid (matches text behaviour).
        t.feed("\x1b[?1049h");
        assert!(t.live_placements().is_empty(), "alt is fresh");
        place(&mut t, 200, 0, 0, 1, 2);
        let live_ids_on_alt: Vec<u32> =
            t.live_placements().iter().map(|p| p.image.0).collect();
        assert_eq!(live_ids_on_alt, vec![200]);

        // referenced_image_ids unions BOTH grids — primary's image survives
        // for retain() purposes even while alt is active, so the texture
        // isn't dropped + re-uploaded on every screen flip.
        let refs: std::collections::HashSet<u32> =
            t.referenced_image_ids().iter().map(|i| i.0).collect();
        assert!(refs.contains(&100));
        assert!(refs.contains(&200));

        // Back to primary: alt's image survives in the alt grid (switching
        // back doesn't clear alt — only switching TO alt does).
        t.feed("\x1b[?1049l");
        let live_ids_back: Vec<u32> =
            t.live_placements().iter().map(|p| p.image.0).collect();
        assert_eq!(live_ids_back, vec![100]);
        let refs_back: std::collections::HashSet<u32> =
            t.referenced_image_ids().iter().map(|i| i.0).collect();
        assert!(refs_back.contains(&100));
        assert!(refs_back.contains(&200));
    }

    #[test]
    fn keep_placements_in_scrollback_toggle_cycle_does_not_refill_queue() {
        // Disable drops the queue; re-enable should NOT magically reinstate
        // previously dropped entries (we have nothing to reinstate from).
        // Pins the documented one-way semantics so a future change that
        // tries to be clever about retention doesn't silently resurrect
        // entries.
        let mut t = Terminal::new(20, 5, 100);
        place(&mut t, 1, 0, 0, 1, 2);
        t.feed("\x1b[1S");
        assert_eq!(t.scrollback_placements_for_test().len(), 1);
        t.set_keep_placements_in_scrollback(false);
        assert!(t.scrollback_placements_for_test().is_empty());
        t.set_keep_placements_in_scrollback(true);
        // Re-enable doesn't restore — the dropped entry is gone for good.
        assert!(t.scrollback_placements_for_test().is_empty());
        // New scrolls now repopulate as expected.
        place(&mut t, 2, 0, 0, 1, 2);
        t.feed("\x1b[1S");
        assert_eq!(t.scrollback_placements_for_test().len(), 1);
    }

    //
    // Gap-fill: KittyAction::Other, handle_apc_delete selector edge cases,
    // register_kitty_image_id idempotence, placeholder bbox corners,
    // decode_kitty_placeholder_image_id boundary values, normalize/inflate
    // edge cases, kitty_placement_params interactions, and capability-vs-
    // dispatch parity. Append-only; reuses kitty_apc / kitty_apc_control_only
    // / kitty_png / placeholder_sgr_fg from above.
    //

    #[test]
    fn kitty_apc_unknown_action_value_drops_silently() {
        // `a=a` (animation, unimplemented) parses to KittyAction::Other.
        // handle_apc returns early before any transmission work — no
        // upload queued, no response emitted, no panic.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        let png = kitty_png(2, 2);
        t.feed(&kitty_apc("a=a,f=100,c=1,r=1", &png));
        assert!(t.take_pending_image_uploads().is_empty());
        assert!(t.take_response().is_empty());
        // Cursor untouched too.
        assert_eq!(t.cursor().row, 0);
        assert_eq!(t.cursor().col, 0);
    }

    #[test]
    fn kitty_apc_a_d_d_a_with_no_kitty_placements_is_noop() {
        // d=a sweeps Kitty placements but leaves iTerm placements alone.
        // With only an iTerm-style placement present (no kitty_image_id),
        // d=a must not touch it and must not panic.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        t.insert_placement(ImageId(1), 0, 0, 1, 1, 0);
        assert_eq!(t.live_placements().len(), 1);
        t.feed(&kitty_apc_control_only("a=d,d=a"));
        assert_eq!(t.live_placements().len(), 1, "iTerm placement survives");
        assert!(t.live_placements()[0].kitty_image_id.is_none());
    }

    #[test]
    fn kitty_apc_a_d_d_i_without_image_id_is_noop() {
        // d=i with no `i=` returns early — nothing to look up. Pin the
        // current behavior: no panic, no spurious placement removal.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        t.register_kitty_image_id(7, ImageId(99));
        t.feed(&kitty_apc_control_only("a=p,i=7,c=1,r=1"));
        assert_eq!(t.live_placements().len(), 1);
        t.feed(&kitty_apc_control_only("a=d,d=i")); // no i=
        assert_eq!(t.live_placements().len(), 1, "missing i= → no-op");
        assert_eq!(t.kitty_image_id_lookup(7), Some(ImageId(99)));
    }

    #[test]
    fn kitty_apc_a_d_d_p_without_placement_id_is_noop() {
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        t.register_kitty_image_id(7, ImageId(99));
        t.feed(&kitty_apc_control_only("a=p,i=7,p=1,c=1,r=1"));
        assert_eq!(t.live_placements().len(), 1);
        t.feed(&kitty_apc_control_only("a=d,d=p")); // no p=
        assert_eq!(t.live_placements().len(), 1, "missing p= → no-op");
    }

    #[test]
    fn kitty_apc_a_d_unknown_selector_drops_silently() {
        // `d=q` (or any unknown selector) parses to KittyDeleteSelector::Other,
        // which is a documented drop. Live placements survive.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        t.register_kitty_image_id(7, ImageId(99));
        t.feed(&kitty_apc_control_only("a=p,i=7,c=1,r=1"));
        assert_eq!(t.live_placements().len(), 1);
        t.feed(&kitty_apc_control_only("a=d,d=q,i=7"));
        assert_eq!(t.live_placements().len(), 1, "unknown selector → drop, no-op");
    }

    #[test]
    fn kitty_apc_a_d_d_i_repeated_second_call_is_noop() {
        // First d=i removes both the placements AND the id mapping. A
        // second d=i for the same id finds nothing to do — pin that it
        // doesn't crash on the missing lookup.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        t.register_kitty_image_id(7, ImageId(99));
        t.feed(&kitty_apc_control_only("a=p,i=7,c=1,r=1"));
        t.feed(&kitty_apc_control_only("a=d,d=i,i=7"));
        assert!(t.live_placements().is_empty());
        assert!(t.kitty_image_id_lookup(7).is_none());
        // Second call — image is already gone.
        t.feed(&kitty_apc_control_only("a=d,d=i,i=7"));
        assert!(t.live_placements().is_empty());
    }

    #[test]
    fn kitty_apc_register_same_store_id_is_idempotent() {
        // Re-registering with the SAME (client_id, store_id) pair must
        // leave the map unchanged — both the lookup and referenced_image_ids
        // still point at the same one entry. (The "new store id overwrites"
        // case is already covered; this pins the no-op branch.)
        let mut t = Terminal::new(80, 24, 100);
        t.register_kitty_image_id(7, ImageId(99));
        t.register_kitty_image_id(7, ImageId(99));
        t.register_kitty_image_id(7, ImageId(99));
        assert_eq!(t.kitty_image_id_lookup(7), Some(ImageId(99)));
        let refs = t.referenced_image_ids();
        assert!(refs.contains(&ImageId(99)));
        assert_eq!(refs.len(), 1, "single mapping → single referenced id");
    }

    #[test]
    fn placeholder_runs_single_cell_has_extent_one() {
        // One placeholder cell at (4, 4) → one run with one-cell
        // extent. Boundary check for the end = start + 1 math.
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&placeholder_sgr_fg(123));
        t.feed("\x1b[5;5H"); // row 4 col 4 (0-based)
        t.feed("\u{10EEEE}\u{0305}\u{0305}");
        let runs = t.kitty_placeholder_runs();
        assert_eq!(runs.len(), 1);
        let r = &runs[0];
        assert_eq!(r.client_id, 123);
        assert_eq!(r.screen_row, 4);
        assert_eq!((r.screen_col_start, r.screen_col_end), (4, 5));
        assert_eq!((r.image_col_start, r.image_col_end), (0, 1));
    }

    #[test]
    fn placeholder_runs_at_origin_handles_zero_indices() {
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&placeholder_sgr_fg(5));
        t.feed("\u{10EEEE}\u{0305}\u{0305}");
        let runs = t.kitty_placeholder_runs();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].screen_row, 0);
        assert_eq!(runs[0].screen_col_start, 0);
    }

    #[test]
    fn placeholder_runs_scan_respects_active_grid_alt_screen() {
        // Placeholders on the primary screen must not appear in
        // the scan after switching to the alt grid.
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&placeholder_sgr_fg(11));
        t.feed("\u{10EEEE}\u{0305}\u{0305}");
        assert_eq!(t.kitty_placeholder_runs().len(), 1);
        t.feed("\x1b[?1049h"); // enter alt screen — fresh empty grid
        assert!(
            t.kitty_placeholder_runs().is_empty(),
            "primary placeholders must not bleed into alt scan",
        );
        // And primary's still intact after returning.
        t.feed("\x1b[?1049l");
        assert_eq!(t.kitty_placeholder_runs().len(), 1);
    }

    #[test]
    fn placeholder_runs_same_id_with_gap_produces_two_runs() {
        // Same-id placeholders separated by non-placeholder cells
        // must produce TWO runs, NOT one merged bbox. The previous
        // bbox API merged them into one stretched rect — exactly
        // the distortion the per-cell rendering was designed to
        // fix.
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&placeholder_sgr_fg(42));
        t.feed("\x1b[1;1H");
        t.feed("\u{10EEEE}\u{0305}\u{0305}");
        // Gap cell with non-placeholder content somewhere later.
        t.feed("\x1b[3;5Hx");
        // Another placeholder of the same id.
        t.feed(&placeholder_sgr_fg(42));
        t.feed("\x1b[5;10H");
        t.feed("\u{10EEEE}\u{0305}\u{0305}");
        let runs = t.kitty_placeholder_runs();
        assert_eq!(runs.len(), 2, "disjoint same-id placeholders → two runs");
    }

    #[test]
    fn placeholder_runs_zero_id_cells_excluded_from_scan() {
        // (0,0,0) fg encodes id 0, which is the "no id" sentinel.
        // Those cells must NOT appear in any run.
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&placeholder_sgr_fg(7));
        t.feed("\u{10EEEE}\u{0305}\u{0305}");
        // Sentinel placeholder at (2,3) with rgb(0,0,0).
        t.feed("\x1b[3;4H");
        t.feed("\x1b[38;2;0;0;0m");
        t.feed("\u{10EEEE}\u{0305}\u{0305}");
        let runs = t.kitty_placeholder_runs();
        assert_eq!(runs.len(), 1, "only the id=7 placeholder appears");
        assert_eq!(runs[0].client_id, 7);
    }

    #[test]
    fn decode_kitty_placeholder_image_id_round_trips_max_24_bit() {
        // 0xFFFFFF (= 16_777_215) is the largest id encodable in 24 bits;
        // round-trips through SGR truecolor + sRGB linearization without loss.
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&placeholder_sgr_fg(0xFFFFFF));
        t.feed("\u{10EEEE}");
        let cell = t.extended_cell(0, 0).unwrap();
        assert_eq!(cell.placeholder_image_id, Some(0xFFFFFF));
    }

    #[test]
    fn decode_kitty_placeholder_image_id_round_trips_one() {
        // Smallest non-sentinel id — the bit just above 0.
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&placeholder_sgr_fg(1));
        t.feed("\u{10EEEE}");
        let cell = t.extended_cell(0, 0).unwrap();
        assert_eq!(cell.placeholder_image_id, Some(1));
    }

    #[test]
    fn decode_kitty_placeholder_image_id_ignores_alpha_channel() {
        // The encoder packs id into RGB only; the alpha component must
        // not perturb the result. Build a Style directly so we can poke
        // an arbitrary alpha without going through the SGR parser.
        let mut style = crate::style::Style::new();
        // The id is encoded in the fg truecolor RGB; `CellColor::Rgb` carries no
        // alpha at all, so the decode is alpha-agnostic by construction.
        style.fg = crate::style::CellColor::Rgb([0xA1, 0xB2, 0xC3]);
        assert_eq!(decode_kitty_placeholder_image_id(&style), Some(0xA1B2C3));
    }

    #[test]
    fn normalize_kitty_payload_raw_rgb_exact_size_passes_through() {
        // Raw RGB w*h*3 bytes exactly — must not be truncated to nothing.
        // The K3 macOS fix accepts oversize buffers (page-padded SHM); pin
        // that exact-size still works and decodes back to declared dims.
        let w = 4u32;
        let h = 4u32;
        let raw: Vec<u8> = (0..(w * h * 3) as u8).collect();
        let out = normalize_kitty_payload(KittyFormat::Rgb, &raw, Some(w), Some(h));
        assert!(out.is_some());
        let (png, dims) = out.unwrap();
        assert_eq!(dims, Some((w, h)));
        // PNG signature.
        assert_eq!(&png[..4], &[0x89, b'P', b'N', b'G']);
    }

    #[test]
    fn normalize_kitty_payload_raw_rgba_too_few_bytes_rejected() {
        // 4×4 RGBA needs 64 bytes; supply 32 → reject (we never truncate
        // upward, only downward from oversize).
        let raw: Vec<u8> = vec![0u8; 32];
        let out = normalize_kitty_payload(KittyFormat::Rgba, &raw, Some(4), Some(4));
        assert!(out.is_none());
    }

    #[test]
    fn normalize_kitty_payload_raw_dims_overflow_returns_none() {
        // s * v * bpp must use checked_mul to guard against malicious
        // s=u32::MAX, v=u32::MAX overflow. Pin: returns None instead of
        // panicking or allocating gigabytes.
        let raw: Vec<u8> = vec![0u8; 8];
        let out = normalize_kitty_payload(
            KittyFormat::Rgb,
            &raw,
            Some(u32::MAX),
            Some(u32::MAX),
        );
        assert!(out.is_none(), "overflow in s*v*bpp must short-circuit");
    }

    #[test]
    fn inflate_kitty_zlib_empty_input_returns_empty_vec() {
        // Empty input: flate2's ZlibDecoder treats "0 bytes available
        // before any header" as a clean EOF and `read_to_end` returns
        // Ok(0). Pin current behavior — None would be defensible too, but
        // any change should be deliberate (a downstream caller might be
        // relying on the empty-Vec path to short-circuit cleanly).
        let out = inflate_kitty_zlib(&[]);
        assert_eq!(out.as_deref(), Some(&[][..]));
    }

    #[test]
    fn inflate_kitty_zlib_cap_rejects_huge_inflation() {
        // Compress 257 MiB of zeros → very small payload that inflates
        // past the 256 MiB cap. Pin that we reject (None) rather than
        // returning the gigabyte allocation.
        use std::io::Write;
        const TOO_BIG: usize = 256 * 1024 * 1024 + 1;
        let zeros = vec![0u8; TOO_BIG];
        let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(&zeros).unwrap();
        let compressed = enc.finish().unwrap();
        // Sanity: the compressed payload is tiny (a few hundred KB tops);
        // we're feeding it through inflate, NOT keeping `zeros` around
        // during the inflate call itself (so the test doesn't double-RAM).
        drop(zeros);
        let out = inflate_kitty_zlib(&compressed);
        assert!(out.is_none(), "inflated size > 256 MiB cap must reject");
    }

    #[test]
    fn inflate_kitty_zlib_concatenated_streams_returns_only_first() {
        // Two zlib streams back-to-back — the decoder reads the first
        // and stops at its end-of-stream marker; the second is ignored.
        // Pin current behavior so a future swap to a multi-stream
        // decoder is a conscious decision.
        use std::io::Write;
        let mut enc1 = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        enc1.write_all(b"first").unwrap();
        let s1 = enc1.finish().unwrap();
        let mut enc2 = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        enc2.write_all(b"SECOND").unwrap();
        let s2 = enc2.finish().unwrap();
        let mut combined = s1.clone();
        combined.extend_from_slice(&s2);
        let out = inflate_kitty_zlib(&combined).expect("first stream decodes");
        assert_eq!(out, b"first", "only the first stream is read");
    }

    #[test]
    fn kitty_placement_params_explicit_zero_offset_is_some_zero() {
        // X=0 / Y=0 explicit MUST round-trip to (0, 0) — not None. Apps
        // use explicit zero to anchor at the top-left of the cell after
        // a previous non-zero offset.
        let mut ctrl = KittyControl::default();
        ctrl.pixel_offset_x = Some(0);
        ctrl.pixel_offset_y = Some(0);
        let (offset, _, _) = kitty_placement_params(&ctrl);
        assert_eq!(offset, (0, 0));
    }

    #[test]
    fn kitty_placement_params_z_min_value_parses_cleanly() {
        // i32::MIN is a valid z per the spec (signed). Pin that nothing
        // narrows or saturates it on the way through.
        let mut ctrl = KittyControl::default();
        ctrl.z_index = Some(i32::MIN);
        let (_, z, _) = kitty_placement_params(&ctrl);
        assert_eq!(z, i32::MIN);
    }

    #[test]
    fn kitty_placement_params_xy_without_wh_yields_no_crop() {
        // Source crop requires all four; x= + y= alone are insufficient.
        // The match `(Some, Some, Some, Some)` fails → None.
        let mut ctrl = KittyControl::default();
        ctrl.crop_x = Some(1);
        ctrl.crop_y = Some(2);
        let (_, _, src) = kitty_placement_params(&ctrl);
        assert!(src.is_none());
    }

    #[test]
    fn kitty_query_supported_matches_dispatch_for_every_format_transmission_tuple() {
        // The query-reply path and the dispatch path BOTH consult
        // `kitty_query_supported`. If the dispatch ever forks (e.g. drops
        // a tuple the query says OK to), apps see silent decode failures.
        //
        // Walk every (format, transmission) combo and assert: when the
        // query says OK, dispatch produces an upload OR a real side-effect
        // (the file/SHM-based paths can fail on the empty payload but
        // never on the format/transmission check itself). When the query
        // says ENOTSUPPORTED, dispatch produces no upload and no response.
        //
        // The "OK matches dispatch" half is easiest to verify positively
        // for direct base64 paths; file/temp/SHM need real artifacts to
        // succeed. So this test covers the ENOTSUPPORTED → silent-drop
        // half exhaustively, and the OK half for the direct paths.
        let formats = [
            (KittyFormat::Png, "100"),
            (KittyFormat::Rgb, "24"),
            (KittyFormat::Rgba, "32"),
        ];
        let transmissions = [
            (KittyTransmission::Direct, "d"),
            (KittyTransmission::File, "f"),
            (KittyTransmission::TempFile, "t"),
            (KittyTransmission::SharedMemory, "s"),
        ];
        let t = Terminal::new(80, 24, 100);
        for (fmt_enum, fmt_str) in &formats {
            for (tx_enum, tx_str) in &transmissions {
                let supported = t.kitty_query_supported(*fmt_enum, *tx_enum);
                // Issue the query and pin the reply prefix.
                let mut q = Terminal::new(80, 24, 100);
                q.feed(&kitty_apc_control_only(&format!(
                    "a=q,i=1,f={},t={},s=1,v=1",
                    fmt_str, tx_str,
                )));
                let reply = q.take_response();
                let s = std::str::from_utf8(&reply).unwrap();
                if supported {
                    assert!(
                        s.starts_with("\x1b_Gi=1;OK"),
                        "(f={}, t={}) query said OK but got: {:?}",
                        fmt_str, tx_str, s,
                    );
                } else {
                    assert!(
                        s.starts_with("\x1b_Gi=1;ENOTSUPPORTED"),
                        "(f={}, t={}) query said unsupported but got: {:?}",
                        fmt_str, tx_str, s,
                    );
                    // And dispatch on an unsupported tuple must drop —
                    // no upload from a one-shot APC. (Direct-base64 only;
                    // the file/SHM paths would fail upstream on missing
                    // artifact anyway, so testing dispatch parity for
                    // them adds no signal beyond the query reply.)
                    let mut d = Terminal::new(80, 24, 100);
                    d.set_cell_size_px(8, 16);
                    let png = kitty_png(2, 2);
                    d.feed(&kitty_apc(
                        &format!("a=T,f={},t={},s=2,v=2,c=1,r=1", fmt_str, tx_str),
                        &png,
                    ));
                    assert!(
                        d.take_pending_image_uploads().is_empty(),
                        "(f={}, t={}) unsupported but dispatch produced an upload",
                        fmt_str, tx_str,
                    );
                }
            }
        }
    }

    // --- OSC 2125: onboarding live-preview channel -----------------------

    /// Feed a single BEL-terminated OSC 2125 payload and drain the requests
    /// it produced. Drives the real `feed` -> parser -> `handle_osc_2125`
    /// path end-to-end so the tests pin the wire contract, not the helper.
    fn preview_after_2125(payload: &str) -> Vec<PreviewRequest> {
        let mut t = Terminal::new(10, 3, 100);
        t.feed(&format!("\x1b]2125;{}\x07", payload));
        t.take_preview_requests()
    }

    #[test]
    fn osc_2125_scheme_by_name() {
        assert_eq!(
            preview_after_2125("scheme;Solarized"),
            vec![PreviewRequest::Scheme(Some("Solarized".into()))],
        );
    }

    #[test]
    fn osc_2125_scheme_empty_is_default() {
        // Empty arg, the literal `-`, and the word `default` all mean
        // "the built-in palette", encoded as `Scheme(None)`.
        assert_eq!(
            preview_after_2125("scheme;"),
            vec![PreviewRequest::Scheme(None)],
        );
        assert_eq!(
            preview_after_2125("scheme;-"),
            vec![PreviewRequest::Scheme(None)],
        );
        assert_eq!(
            preview_after_2125("scheme;default"),
            vec![PreviewRequest::Scheme(None)],
        );
    }

    #[test]
    fn osc_2125_glow_presets() {
        assert_eq!(
            preview_after_2125("glow;off"),
            vec![PreviewRequest::Glow(crate::GlowLevel::Off)],
        );
        assert_eq!(
            preview_after_2125("glow;subtle"),
            vec![PreviewRequest::Glow(crate::GlowLevel::Subtle)],
        );
        assert_eq!(
            preview_after_2125("glow;full"),
            vec![PreviewRequest::Glow(crate::GlowLevel::Full)],
        );
    }

    #[test]
    fn osc_2125_glow_unknown_preset_dropped() {
        // An unparseable glow level is a no-op, matching the terminal's
        // "unknown OSC is silent" contract — nothing queued.
        assert!(preview_after_2125("glow;bogus").is_empty());
        assert!(preview_after_2125("glow;").is_empty());
    }

    #[test]
    fn osc_2125_scanlines_on_off() {
        assert_eq!(
            preview_after_2125("scanlines;on"),
            vec![PreviewRequest::Scanlines(true)],
        );
        assert_eq!(
            preview_after_2125("scanlines;off"),
            vec![PreviewRequest::Scanlines(false)],
        );
    }

    #[test]
    fn osc_2125_scanlines_unknown_arg_dropped() {
        assert!(preview_after_2125("scanlines;maybe").is_empty());
        assert!(preview_after_2125("scanlines;").is_empty());
    }

    #[test]
    fn osc_2125_reload() {
        assert_eq!(
            preview_after_2125("reload"),
            vec![PreviewRequest::Reload],
        );
    }

    #[test]
    fn osc_2125_crt_presets() {
        assert_eq!(
            preview_after_2125("crt;off"),
            vec![PreviewRequest::Crt(crate::CrtLevel::Off)],
        );
        assert_eq!(
            preview_after_2125("crt;low"),
            vec![PreviewRequest::Crt(crate::CrtLevel::Low)],
        );
        assert_eq!(
            preview_after_2125("crt;high"),
            vec![PreviewRequest::Crt(crate::CrtLevel::High)],
        );
    }

    #[test]
    fn osc_2125_crt_unknown_preset_dropped() {
        assert!(preview_after_2125("crt;medium").is_empty());
        assert!(preview_after_2125("crt;").is_empty());
    }

    #[test]
    fn osc_2125_font_size() {
        assert_eq!(
            preview_after_2125("font;12"),
            vec![PreviewRequest::FontSize(12.0)],
        );
        assert_eq!(
            preview_after_2125("font;9.5"),
            vec![PreviewRequest::FontSize(9.5)],
        );
    }

    #[test]
    fn osc_2125_font_size_garbage_dropped() {
        assert!(preview_after_2125("font;big").is_empty());
        assert!(preview_after_2125("font;").is_empty());
        // Non-finite values are rejected too.
        assert!(preview_after_2125("font;inf").is_empty());
    }

    #[test]
    fn osc_2125_unknown_verb_dropped() {
        assert!(preview_after_2125("wat").is_empty());
        assert!(preview_after_2125("wat;arg").is_empty());
    }

    #[test]
    fn osc_2125_multiple_requests_queue_in_order() {
        // Several 2125 sequences in one feed must all land, in arrival
        // order, so the front end can replay the user's choices faithfully.
        let mut t = Terminal::new(10, 3, 100);
        t.feed(
            "\x1b]2125;scheme;Solarized\x07\
             \x1b]2125;glow;full\x07\
             \x1b]2125;scanlines;on\x07\
             \x1b]2125;reload\x07",
        );
        assert_eq!(
            t.take_preview_requests(),
            vec![
                PreviewRequest::Scheme(Some("Solarized".into())),
                PreviewRequest::Glow(crate::GlowLevel::Full),
                PreviewRequest::Scanlines(true),
                PreviewRequest::Reload,
            ],
        );
    }

    #[test]
    fn osc_2125_dropped_requests_do_not_break_the_queue() {
        // A dropped (unknown) request mid-stream must not eat the valid
        // ones around it — the queue keeps exactly the parseable verbs.
        let mut t = Terminal::new(10, 3, 100);
        t.feed(
            "\x1b]2125;glow;bogus\x07\
             \x1b]2125;reload\x07\
             \x1b]2125;scanlines;nope\x07\
             \x1b]2125;scheme;Nord\x07",
        );
        assert_eq!(
            t.take_preview_requests(),
            vec![
                PreviewRequest::Reload,
                PreviewRequest::Scheme(Some("Nord".into())),
            ],
        );
    }

    #[test]
    fn osc_2125_take_drains_the_queue() {
        // The accessor moves the queue out; a second drain sees nothing.
        let mut t = Terminal::new(10, 3, 100);
        t.feed("\x1b]2125;reload\x07");
        assert_eq!(t.take_preview_requests(), vec![PreviewRequest::Reload]);
        assert!(t.take_preview_requests().is_empty());
    }

    // ---- Dirty-row damage tracking (Part A of the dirty-row render work) ----

    fn no_rows_dirty(t: &Terminal) -> bool {
        t.row_damage().iter().all(|&d| !d)
    }

    #[test]
    fn new_grid_starts_fully_dirty() {
        // The first frame must emit every row, so a fresh grid is all-dirty.
        let t = Terminal::new(10, 3, 100);
        assert_eq!(t.row_damage().len(), 3);
        assert!(t.row_damage().iter().all(|&d| d));
    }

    #[test]
    fn clear_row_damage_resets_all_flags() {
        let mut t = Terminal::new(10, 3, 100);
        t.feed("hi");
        t.clear_row_damage();
        assert!(no_rows_dirty(&t));
    }

    #[test]
    fn writing_identical_cell_does_not_dirty_row() {
        // The user's explicit requirement: re-printing the identical glyph+style
        // must NOT mark the row dirty.
        let mut t = Terminal::new(10, 3, 100);
        t.feed("A");
        t.clear_row_damage();
        t.feed("\x1b[HA"); // home, then re-print the same 'A'
        assert!(no_rows_dirty(&t), "identical re-print should not dirty");
    }

    #[test]
    fn writing_changed_cell_dirties_only_that_row() {
        let mut t = Terminal::new(10, 3, 100);
        t.feed("A");
        t.clear_row_damage();
        t.feed("\x1b[HB"); // home, overwrite 'A' with 'B'
        assert!(t.row_damage()[0], "changed cell must dirty its row");
        assert!(!t.row_damage()[1]);
        assert!(!t.row_damage()[2]);
    }

    #[test]
    fn changing_only_style_dirties_row() {
        // Same glyph, different color is still a rendered change.
        let mut t = Terminal::new(10, 3, 100);
        t.feed("A");
        t.clear_row_damage();
        t.feed("\x1b[H\x1b[31mA"); // home, red 'A' over default 'A'
        assert!(t.row_damage()[0]);
    }

    #[test]
    fn cursor_move_alone_does_not_dirty() {
        // The cursor is a renderer overlay; moving it touches no cell.
        let mut t = Terminal::new(10, 3, 100);
        t.feed("A");
        t.clear_row_damage();
        t.feed("\x1b[3;5H"); // CUP to row 3 col 5, no write
        assert!(no_rows_dirty(&t));
    }

    #[test]
    fn erase_in_line_dirties_only_the_cursor_row() {
        let mut t = Terminal::new(10, 3, 100);
        t.feed("aaa\r\nbbb\r\nccc");
        t.clear_row_damage();
        t.feed("\x1b[2;1H\x1b[2K"); // EL 2 on row 2 (clear whole line)
        assert!(!t.row_damage()[0]);
        assert!(t.row_damage()[1]);
        assert!(!t.row_damage()[2]);
    }

    #[test]
    fn erase_in_line_on_blank_span_stays_clean() {
        // EL over an already-blank tail changes nothing → no damage.
        let mut t = Terminal::new(10, 3, 100);
        t.feed("ab"); // cols 2..10 already blank
        t.clear_row_damage();
        t.feed("\x1b[1;3H\x1b[0K"); // EL 0 from col 3 to EOL (all blank)
        assert!(no_rows_dirty(&t));
    }

    #[test]
    fn erase_in_display_below_dirties_affected_rows() {
        let mut t = Terminal::new(10, 3, 100);
        t.feed("aaa\r\nbbb\r\nccc");
        t.clear_row_damage();
        t.feed("\x1b[2;1H\x1b[0J"); // ED 0: clear from row 2 col 1 down
        assert!(!t.row_damage()[0]);
        assert!(t.row_damage()[1]);
        assert!(t.row_damage()[2]);
    }

    #[test]
    fn full_region_scroll_shifts_damage_instead_of_marking_all() {
        // A scrollback-growing full-screen scroll moves each row's damage flag
        // up with its content rather than dirtying everything, so the renderer
        // can reuse the unchanged lines (now on new visual rows) by absolute
        // line. Only the freed+rewritten bottom row is damaged.
        let mut t = Terminal::new(10, 3, 100);
        t.feed("a\r\nb\r\nc"); // rows a,b,c; cursor at bottom
        t.clear_row_damage();
        t.feed("\r\nd"); // scroll up 1, print d → b,c,d
        assert!(!t.row_damage()[0], "shifted line stays clean");
        assert!(!t.row_damage()[1], "shifted line stays clean");
        assert!(t.row_damage()[2], "freed + rewritten bottom row is dirty");
    }

    #[test]
    fn damage_follows_content_through_full_scroll() {
        // A line dirtied in place keeps its damage when a later scroll moves it
        // — the flag rides up with the content so the renderer re-emits it at
        // its new visual row instead of reusing a stale cache entry.
        let mut t = Terminal::new(10, 3, 100);
        t.feed("a\r\nb\r\nc");
        t.clear_row_damage();
        t.feed("\x1b[2;1HX"); // overwrite row 1 'b' -> 'X' (dirties row 1)
        t.feed("\x1b[3;1H\n"); // cursor to bottom, LF -> scroll up 1
        // 'X…' moved from row 1 to row 0, carrying its damage; bottom row freed.
        assert!(t.row_damage()[0], "modified line's damage follows it up");
        assert!(!t.row_damage()[1], "unchanged shifted line stays clean");
        assert!(t.row_damage()[2], "freed bottom row is dirty");
    }

    #[test]
    fn partial_region_scroll_still_marks_all() {
        // A scroll that does NOT grow scrollback (here a DECSTBM region) shifts
        // content between fixed row positions, so abs-line identity isn't
        // preserved — the whole region must be marked dirty.
        let mut t = Terminal::new(10, 4, 100);
        t.feed("a\r\nb\r\nc\r\nd");
        t.feed("\x1b[1;3r"); // DECSTBM: scroll region rows 1..=3
        t.clear_row_damage();
        t.feed("\x1b[3;1H\n"); // LF at region bottom -> partial scroll up
        assert!(t.row_damage()[0], "region row dirtied");
        assert!(t.row_damage()[1], "region row dirtied");
        assert!(t.row_damage()[2], "region row dirtied");
    }

    #[test]
    fn full_region_scroll_by_two_shifts_damage_by_two() {
        // SU 2 shifts every row's damage up by 2: an in-place-dirtied row 3
        // lands on row 1 carrying its flag, the clean row 2 lands on row 0
        // staying clean, and the two freed bottom rows (3,4) are dirty.
        let mut t = Terminal::new(10, 5, 100);
        t.feed("a\r\nb\r\nc\r\nd\r\ne"); // rows 0..4 = a,b,c,d,e
        t.clear_row_damage(); // all rows clean
        t.feed("\x1b[4;1HX"); // overwrite row 3 'd' -> 'X' => only row 3 dirty
        assert!(!t.row_damage()[0] && !t.row_damage()[1] && !t.row_damage()[2]);
        assert!(t.row_damage()[3] && !t.row_damage()[4]); // precondition
        t.feed("\x1b[2S"); // SU 2 (full-screen, scrollback-growing)
        assert!(!t.row_damage()[0], "clean row 2 shifted to row 0 stays clean");
        assert!(t.row_damage()[1], "dirtied row 3 shifted to row 1 keeps damage");
        assert!(!t.row_damage()[2], "clean row 4 shifted to row 2 stays clean");
        assert!(t.row_damage()[3], "freed bottom row is dirty");
        assert!(t.row_damage()[4], "freed bottom row is dirty");
    }

    #[test]
    fn full_region_scroll_n_equals_region_clears_all_no_panic() {
        // Regression: SU with n >= region clamps to region; the damage-shift
        // path must NOT compute `bottom - n` (which would underflow when
        // n == region). It instead clears every row, leaving all dirty.
        let mut t = Terminal::new(10, 3, 100);
        t.feed("a\r\nb\r\nc");
        t.clear_row_damage();
        t.feed("\x1b[50S"); // SU 50 on a 3-row grid: clamps to 3 == region
        assert!(
            t.row_damage().iter().all(|&d| d),
            "n == region clears and dirties every row"
        );
    }

    #[test]
    fn full_region_scroll_then_identical_reprint_stays_clean() {
        // After a full scroll moves a clean line up (and we re-clear damage),
        // re-printing that line's identical content must not dirty it — the
        // change-gated `set` plus the shifted-clean flag keep the renderer's
        // cached row valid.
        let mut t = Terminal::new(10, 3, 100);
        t.feed("a\r\nb\r\nc"); // rows a,b,c
        t.feed("\r\nd"); // scroll up 1 -> rows b,c,d
        t.clear_row_damage();
        // 'b' is now on row 0; re-print the identical 'b' there.
        t.feed("\x1b[1;1Hb");
        assert!(no_rows_dirty(&t), "identical re-print of shifted line stays clean");
    }

    #[test]
    fn alt_screen_full_scroll_marks_region_not_shift() {
        // On the alt screen a full-screen LF scroll does NOT grow scrollback,
        // so use_alternate disables the damage shift: the whole region is
        // dirtied even though a clean line moved up.
        let mut t = Terminal::new(10, 3, 100);
        t.feed("\x1b[?1049h"); // enter alt screen
        t.feed("a\r\nb\r\nc"); // fill the alt grid, cursor at bottom
        t.clear_row_damage();
        t.feed("\r\nd"); // LF at bottom -> full-screen scroll up on alt
        assert!(
            t.row_damage().iter().all(|&d| d),
            "alt full scroll marks the whole region dirty"
        );
    }

    #[test]
    fn full_region_scroll_with_zero_scrollback_marks_region() {
        // shift_damage requires scrollback_limit > 0. With no scrollback the
        // line has no stable absolute-line identity to carry, so the scroll
        // falls back to marking the whole region dirty.
        let mut t = Terminal::new(10, 3, 0);
        t.feed("a\r\nb\r\nc");
        t.clear_row_damage();
        t.feed("\r\nd"); // scroll up 1 with no scrollback
        assert!(
            t.row_damage().iter().all(|&d| d),
            "zero-scrollback full scroll marks the whole region dirty"
        );
    }

    #[test]
    fn delete_lines_dirties_its_region_not_shift() {
        // DL (CSI M) routes through scroll_region_up with shift_damage=false:
        // it never grows scrollback, so its region [cursor.row..=bottom] is
        // marked dirty rather than shifting damage flags up.
        let mut t = Terminal::new(10, 4, 100);
        t.feed("a\r\nb\r\nc\r\nd"); // rows 0..3
        t.clear_row_damage();
        t.feed("\x1b[2;1H\x1b[1M"); // cursor to row 1, DL 1
        assert!(!t.row_damage()[0], "row above DL region untouched");
        assert!(t.row_damage()[1], "DL region row dirtied");
        assert!(t.row_damage()[2], "DL region row dirtied");
        assert!(t.row_damage()[3], "DL region row dirtied");
    }

    #[test]
    fn two_lf_scrolls_in_one_feed_compose_shift_correctly() {
        // Feeding two lines triggers two full-screen scrolls; the shift-damage
        // path composes so only genuinely-new bottom rows are dirty and the
        // surviving shifted lines stay clean.
        let mut t = Terminal::new(10, 3, 100);
        t.feed("a\r\nb\r\nc"); // rows a,b,c
        t.clear_row_damage();
        t.feed("\r\nd\r\ne"); // two scrolls -> rows c,d,e
        // 'c' rode up twice from row 2 to row 0, staying clean; 'd' and 'e'
        // are freshly written bottom rows.
        assert!(!t.row_damage()[0], "twice-shifted clean line stays clean");
        assert!(t.row_damage()[1], "newly written row is dirty");
        assert!(t.row_damage()[2], "newly written row is dirty");
    }

    #[test]
    fn delete_chars_dirties_its_row() {
        let mut t = Terminal::new(10, 3, 100);
        t.feed("abcdef");
        t.clear_row_damage();
        t.feed("\x1b[1;1H\x1b[2P"); // DCH 2 on row 1
        assert!(t.row_damage()[0]);
        assert!(!t.row_damage()[1]);
    }

    #[test]
    fn insert_chars_dirties_its_row() {
        let mut t = Terminal::new(10, 3, 100);
        t.feed("abcdef");
        t.clear_row_damage();
        t.feed("\x1b[1;1H\x1b[2@"); // ICH 2 on row 1
        assert!(t.row_damage()[0]);
        assert!(!t.row_damage()[1]);
    }

    #[test]
    fn alt_screen_toggle_marks_active_grid_dirty() {
        let mut t = Terminal::new(10, 3, 100);
        t.feed("hi");
        t.clear_row_damage();
        t.feed("\x1b[?1049h"); // enter alt screen
        assert!(t.row_damage().iter().all(|&d| d), "alt enter is all-dirty");
        t.clear_row_damage();
        t.feed("\x1b[?1049l"); // leave back to primary
        assert!(t.row_damage().iter().all(|&d| d), "primary restore is all-dirty");
    }

    #[test]
    fn reresolve_palette_marks_all_dirty() {
        let mut t = Terminal::new(10, 3, 100);
        t.feed("x");
        t.clear_row_damage();
        t.reresolve_palette();
        assert!(t.row_damage().iter().all(|&d| d));
    }

    #[test]
    fn resize_produces_an_all_dirty_primary_grid() {
        // A resize rebuilds the live grid wholesale; every row must repaint.
        let mut t = Terminal::new(10, 3, 100);
        t.feed("hi");
        t.clear_row_damage();
        t.resize(12, 5);
        assert_eq!(t.row_damage().len(), 5);
        assert!(t.row_damage().iter().all(|&d| d), "resize is all-dirty");
    }

    #[test]
    fn resize_produces_an_all_dirty_alternate_grid() {
        // The alternate buffer is recreated on resize too; when it later
        // becomes the active grid its damage must reflect a fresh, all-dirty
        // grid (the row_damage view follows the active grid).
        let mut t = Terminal::new(10, 3, 100);
        t.feed("\x1b[?1049h"); // enter alt screen
        t.feed("alt");
        t.clear_row_damage();
        t.resize(10, 4); // recreates the (active) alternate grid
        assert_eq!(t.row_damage().len(), 4);
        assert!(t.row_damage().iter().all(|&d| d), "resized alt is all-dirty");
    }

    #[test]
    fn erase_in_display_full_dirties_all_rows() {
        // ED 2 clears the whole screen → every row repaints.
        let mut t = Terminal::new(10, 3, 100);
        t.feed("aaa\r\nbbb\r\nccc");
        t.clear_row_damage();
        t.feed("\x1b[2J"); // ED 2: erase entire display
        assert!(t.row_damage().iter().all(|&d| d), "ED 2 dirties all rows");
    }

    #[test]
    fn erase_in_display_scrollback_leaves_screen_clean() {
        // ED 3 (xterm "Erase Saved Lines") drops scrollback only; on-screen
        // content is untouched, so no live row is dirtied.
        let mut t = Terminal::new(10, 3, 100);
        t.feed("a\r\nb\r\nc\r\nd\r\ne"); // push some lines into scrollback
        t.clear_row_damage();
        t.feed("\x1b[3J"); // ED 3: erase saved lines
        assert!(no_rows_dirty(&t), "ED 3 must not dirty on-screen rows");
    }

    #[test]
    fn erase_chars_dirties_only_the_cursor_row() {
        // ECH replaces n non-blank cells with blanks on the cursor row only.
        let mut t = Terminal::new(10, 3, 100);
        t.feed("aaa\r\nbbb\r\nccc");
        t.clear_row_damage();
        t.feed("\x1b[2;1H\x1b[3X"); // ECH 3 on row 2
        assert!(!t.row_damage()[0]);
        assert!(t.row_damage()[1]);
        assert!(!t.row_damage()[2]);
    }

    #[test]
    fn erase_chars_on_blank_span_stays_clean() {
        // ECH over already-blank cells changes nothing → no damage
        // (clear_row is change-gated).
        let mut t = Terminal::new(10, 3, 100);
        t.feed("ab"); // cols 2..10 already blank
        t.clear_row_damage();
        t.feed("\x1b[1;5H\x1b[3X"); // ECH 3 starting in the blank tail
        assert!(no_rows_dirty(&t), "ECH on blank span must not dirty");
    }

    #[test]
    fn clear_row_partially_nonblank_span_dirties_only_changed_row() {
        // A span that's part text, part blank still dirties because at least
        // one cell changes; the blank cells in the span don't matter.
        let mut t = Terminal::new(10, 3, 100);
        t.feed("\r\nab"); // row 1: "ab" then blanks, rows 0/2 blank
        t.clear_row_damage();
        t.feed("\x1b[2;1H\x1b[5X"); // ECH 5 over "ab___" (mixed) on row 1
        assert!(!t.row_damage()[0]);
        assert!(t.row_damage()[1]);
        assert!(!t.row_damage()[2]);
    }

    #[test]
    fn reverse_index_at_top_dirties_the_region() {
        // RI at the top margin scrolls the whole region down; with the default
        // full-screen region every surviving row renders differently.
        let mut t = Terminal::new(3, 3, 100);
        t.feed("AAA\r\nBBB\r\nCCC");
        t.feed("\x1b[H"); // home (cursor move only)
        t.clear_row_damage();
        t.feed("\x1bM"); // RI: scroll region down by 1
        assert!(t.row_damage().iter().all(|&d| d), "RI dirties its region");
    }

    #[test]
    fn scroll_down_dirties_the_region() {
        // SD (CSI T) shifts the region down in place; mark the whole region.
        let mut t = Terminal::new(3, 3, 100);
        t.feed("AAA\r\nBBB\r\nCCC");
        t.clear_row_damage();
        t.feed("\x1b[T"); // SD by 1
        assert!(t.row_damage().iter().all(|&d| d), "SD dirties its region");
    }

    #[test]
    fn writing_to_alt_marks_alt_rows_not_primary() {
        // row_damage() reflects only the ACTIVE grid. A write on the alt
        // screen dirties the alt row; switching back shows the primary's
        // (unchanged) damage state, not the alt's.
        let mut t = Terminal::new(10, 3, 100);
        t.feed("hi"); // on primary, row 0
        t.feed("\x1b[?1049h"); // enter alt (marks alt all-dirty)
        t.clear_row_damage(); // clear the active (alt) grid
        t.feed("\x1b[2;1HZ"); // write 'Z' on alt row 2
        assert!(!t.row_damage()[0]);
        assert!(t.row_damage()[1], "alt write dirties its row");
        assert!(!t.row_damage()[2]);
        // Leaving alt re-marks the now-active primary all-dirty (screen swap),
        // so the alt's per-row damage never leaks into the primary's view.
        t.feed("\x1b[?1049l");
        assert!(t.row_damage().iter().all(|&d| d), "primary restore all-dirty");
    }

    #[test]
    fn repeated_identical_writes_stay_clean() {
        // Re-feeding the same glyph at the same spot across several writes must
        // never dirty the row after the first paint.
        let mut t = Terminal::new(10, 3, 100);
        t.feed("A");
        t.clear_row_damage();
        t.feed("\x1b[HA"); // re-print 'A'
        t.feed("\x1b[HA"); // again
        t.feed("\x1b[HA"); // and again
        assert!(no_rows_dirty(&t), "repeated identical writes stay clean");
    }

    // ===================================================================
    // Character sets / DEC line drawing.
    //
    // CONFIRMED GAP: charset switching is entirely unimplemented. SCS
    // designators (`ESC ( 0`, `ESC ) 0`, ...) are consumed by the ANSI
    // parser but emit no event, and SI (0x0F) / SO (0x0E) are not handled.
    // These tests pin that the sequences are inert: the surrounding text
    // prints normally and the "graphics" letters print as their literal
    // ASCII, NOT as box-drawing glyphs.
    // ===================================================================

    #[test]
    fn dec_special_graphics_designation_is_inert() {
        // Invariant: `ESC ( 0` then 'q' prints a literal 'q', not '─'.
        let mut t = Terminal::new(10, 1, 100);
        t.feed("\x1b(0qxl");
        assert_eq!(t.row(0)[0].ch, 'q', "DEC graphics not mapped to ─");
        assert_eq!(t.row(0)[1].ch, 'x');
        assert_eq!(t.row(0)[2].ch, 'l');
        assert_eq!(t.cursor().col, 3);
    }

    #[test]
    fn shift_in_shift_out_are_inert() {
        // Invariant: SO (0x0E) / SI (0x0F) do not switch charsets nor consume
        // the surrounding glyphs incorrectly; 'q' prints literally throughout.
        let mut t = Terminal::new(10, 1, 100);
        t.feed("\x0eq\x0fq");
        assert_eq!(t.row(0)[0].ch, 'q');
        assert_eq!(t.row(0)[1].ch, 'q');
    }

    #[test]
    fn scs_terminator_does_not_leak_final_byte() {
        // Invariant: `ESC ( B` (reset G0 to ASCII, used by prompts) is fully
        // consumed — no stray 'B' leaks into the grid. Regression guard for
        // starship/tmux-style styled prompts.
        let mut t = Terminal::new(10, 1, 100);
        t.feed("a\x1b(Bb");
        assert_eq!(render(&t), "ab");
        assert_eq!(t.cursor().col, 2);
    }

    // ===================================================================
    // Tab stops.
    //
    // CONFIRMED GAP: tab stops are a hardcoded fixed 8-column grid. There
    // is no HTS (`ESC H`) / TBC (`CSI g`) custom-stop machinery, and CBT
    // (`CSI Z`, back-tab) is consumed by the parser with no event, so it
    // is a no-op. These tests pin the real fixed-8 behavior and the
    // inertness of the unsupported sequences.
    // ===================================================================

    #[test]
    fn tab_clamps_at_right_edge() {
        // Invariant: a tab past the last 8-multiple clamps to the final
        // column (cols-1) instead of running off the grid.
        let mut t = Terminal::new(6, 1, 100);
        t.feed("\t"); // from col 0, next multiple of 8 is 8 -> clamp to 5
        assert_eq!(t.cursor().col, 5);
        t.feed("\t"); // already at right edge, stays clamped
        assert_eq!(t.cursor().col, 5);
    }

    #[test]
    fn tab_from_a_stop_advances_a_full_eight() {
        // Invariant: tabbing while already on a stop jumps to the next stop,
        // not staying put — exercised on a wide grid so no clamping hides it.
        let mut t = Terminal::new(40, 1, 100);
        t.feed("\x1b[9G"); // col 8 (a stop)
        assert_eq!(t.cursor().col, 8);
        t.feed("\t");
        assert_eq!(t.cursor().col, 16);
    }

    #[test]
    fn tab_clears_wrap_pending() {
        // Invariant: a tab cancels a pending wrap (cursor was parked at the
        // last column) and moves within the same row.
        let mut t = Terminal::new(20, 2, 100);
        t.feed("\x1b[20G"); // last col (col 19)
        t.feed("X"); // sets wrap_pending
        assert_eq!(t.cursor().col, 19);
        t.feed("\t"); // clamps to right edge, clears wrap_pending
        assert_eq!(t.cursor().col, 19);
        t.feed("Y"); // would have wrapped if wrap_pending survived
        assert_eq!(t.cursor().row, 0, "tab cleared wrap, no line feed");
    }

    #[test]
    fn back_tab_cbt_is_a_noop() {
        // Invariant: CSI Z (CBT, back-tab) is unimplemented — the parser
        // consumes it without moving the cursor.
        let mut t = Terminal::new(40, 1, 100);
        t.feed("\x1b[17G"); // col 16
        assert_eq!(t.cursor().col, 16);
        t.feed("\x1b[Z"); // CBT — would move to col 8 if implemented
        assert_eq!(t.cursor().col, 16, "CBT is a no-op");
    }

    #[test]
    fn hts_and_tbc_are_inert() {
        // Invariant: HTS (ESC H) and TBC (CSI g / CSI 3 g) do not alter the
        // fixed 8-column tab grid (no custom-stop support). After issuing
        // them, tabs still land on multiples of 8.
        let mut t = Terminal::new(40, 1, 100);
        t.feed("\x1b[4G"); // col 3
        t.feed("\x1bH"); // HTS — try to set a stop at col 3 (ignored)
        t.feed("\x1b[g"); // TBC 0 (ignored)
        t.feed("\x1b[3g"); // TBC 3 — clear all (ignored)
        t.feed("\x1b[1G\t"); // back to col 0, then tab
        assert_eq!(t.cursor().col, 8, "tab still lands on multiple of 8");
    }

    // ===================================================================
    // Cursor-movement edge cases (CUU/CUD/CUF/CUB clamping, CHA/VPA,
    // CNL/CPL). These ARE implemented (except CNL/CPL — see report).
    // ===================================================================

    #[test]
    fn cursor_up_clamps_at_top_row() {
        // Invariant: CUU past the top row clamps to row 0.
        let mut t = Terminal::new(5, 4, 100);
        t.feed("\x1b[2;1H"); // row 1
        t.feed("\x1b[10A"); // up 10 -> clamp to row 0
        assert_eq!(t.cursor().row, 0);
    }

    #[test]
    fn cursor_down_and_back_clamp_at_edges() {
        // Invariant: CUD clamps at the last row, CUB clamps at col 0.
        let mut t = Terminal::new(5, 4, 100);
        t.feed("\x1b[1;3H"); // row 0, col 2
        t.feed("\x1b[10B"); // down 10 -> last row (3)
        assert_eq!(t.cursor().row, 3);
        t.feed("\x1b[10D"); // back 10 -> col 0
        assert_eq!(t.cursor().col, 0);
    }

    #[test]
    fn cursor_forward_clamps_at_right_edge() {
        // Invariant: CUF past the last column clamps to cols-1.
        let mut t = Terminal::new(6, 2, 100);
        t.feed("\x1b[99C");
        assert_eq!(t.cursor().col, 5);
    }

    #[test]
    fn cha_absolute_column_clamps_and_is_one_based() {
        // Invariant: CHA (CSI G) sets the column 1-based, clamped to cols-1,
        // and a zero/empty param means column 1 (col 0).
        let mut t = Terminal::new(6, 3, 100);
        t.feed("\x1b[3G"); // col 2
        assert_eq!(t.cursor().col, 2);
        t.feed("\x1b[99G"); // clamp to last col
        assert_eq!(t.cursor().col, 5);
        t.feed("\x1b[G"); // default -> col 0
        assert_eq!(t.cursor().col, 0);
    }

    #[test]
    fn hpa_backtick_alias_matches_cha() {
        // Invariant: HPA (CSI `) is parsed as CursorHorizontalAbs, same as CHA.
        let mut t = Terminal::new(10, 2, 100);
        t.feed("\x1b[5`");
        assert_eq!(t.cursor().col, 4);
    }

    #[test]
    fn vpa_absolute_row_clamps_and_is_one_based() {
        // Invariant: VPA (CSI d) sets the row 1-based, clamped to rows-1,
        // and leaves the column untouched.
        let mut t = Terminal::new(6, 4, 100);
        t.feed("\x1b[1;4H"); // row 0, col 3
        t.feed("\x1b[3d"); // row 2, col unchanged
        assert_eq!(t.cursor().row, 2);
        assert_eq!(t.cursor().col, 3);
        t.feed("\x1b[99d"); // clamp to last row
        assert_eq!(t.cursor().row, 3);
    }

    #[test]
    fn cnl_and_cpl_are_unimplemented_noops() {
        // CONFIRMED GAP: CSI E (CNL) and CSI F (CPL) are not emitted by the
        // ANSI parser (they fall through to a silent drop), so they neither
        // move to column 0 nor change rows. Pin that real no-op behavior.
        let mut t = Terminal::new(6, 4, 100);
        t.feed("\x1b[2;3H"); // row 1, col 2
        t.feed("\x1b[E"); // CNL — would go row 2 col 0 if implemented
        assert_eq!((t.cursor().row, t.cursor().col), (1, 2), "CNL is a no-op");
        t.feed("\x1b[F"); // CPL — would go row 0 col 0 if implemented
        assert_eq!((t.cursor().row, t.cursor().col), (1, 2), "CPL is a no-op");
    }

    // ---- Scrollback Vec recycling at capacity (perf optimization in
    // `scroll_region_up_by`: the evicted front row's allocation is reused for
    // the pushed line via pop_front + clear + extend_from_slice). These pin the
    // *content correctness* of that reuse — the risk is stale bytes from the
    // evicted row surviving into the recycled buffer.

    #[test]
    fn scroll_far_past_limit_keeps_exact_last_n_lines_uncorrupted() {
        // Feed many more distinct full-width lines than the scrollback limit, so
        // every push past the limit lands in a *recycled* buffer (the steady
        // state under a flood). The recycled rows must carry the freshly
        // scrolled bytes, never a tail of the evicted row they reused. With a
        // 1-row grid; each line is printed then a trailing CRLF scrolls it into
        // scrollback (including the last). So after `total` lines the grid is
        // blank and scrollback holds the final LIMIT lines in order: indices
        // total-LIMIT .. total-1.
        const LIMIT: usize = 4;
        let cols = 5;
        let mut t = Terminal::new(cols, 1, LIMIT);
        let total = 3 * LIMIT + 2; // well past 2*LIMIT
        // 26 distinct glyphs cycled; each line is `cols` copies of one glyph so
        // the whole row is a single recognizable byte pattern.
        let glyph = |i: usize| (b'A' + (i % 26) as u8) as char;
        for i in 0..total {
            let line: String = std::iter::repeat(glyph(i)).take(cols).collect();
            t.feed(&line);
            t.feed("\r\n");
        }
        // Scrollback pins at the limit and holds the final LIMIT lines.
        assert_eq!(t.scrollback_len(), LIMIT);
        assert!(t.scrollback_evicted() >= (total - LIMIT) as u64);
        for slot in 0..LIMIT {
            let line_idx = total - LIMIT + slot;
            let expected = glyph(line_idx);
            let row = &t.scrollback[slot];
            // No stale tail: the recycled buffer is exactly `cols` wide…
            assert_eq!(
                row.len(),
                cols,
                "recycled scrollback row {slot} must be exactly {cols} cells, no leftover tail"
            );
            // …and every cell is the freshly scrolled glyph, not stale bytes.
            for (col, cell) in row.iter().enumerate() {
                assert_eq!(
                    cell.ch, expected,
                    "scrollback row {slot} col {col}: recycled buffer carries wrong/stale content"
                );
            }
        }
    }

    #[test]
    fn recycled_row_holding_longer_line_then_shorter_has_no_leftover_tail() {
        // The recycle path is `pop_front -> clear() -> extend_from_slice`. If a
        // recycled buffer once held a *wider* row (more cells) and then receives
        // a *narrower* one, the clear() must drop the old length so no tail of
        // the evicted-wide row survives. A horizontal shrink makes the live grid
        // narrower while older scrollback Vecs keep their original (wider) width,
        // so a later scroll recycles a wide buffer to hold a narrow row.
        let wide = 8;
        let narrow = 3;
        let limit = 2;
        let mut t = Terminal::new(wide, 1, limit);
        // Fill scrollback to the limit with full-width WIDE rows.
        t.feed("WWWWWWWW\r\n"); // -> scrollback[0], 8 cells of 'W'
        t.feed("XXXXXXXX\r\n"); // -> scrollback[1], 8 cells of 'X'
        assert_eq!(t.scrollback_len(), limit);
        assert_eq!(t.scrollback[0].len(), wide, "fixture: wide rows are 8 cells");

        // Shrink columns; existing scrollback Vecs keep their 8-cell width.
        t.resize(narrow, 1);
        assert_eq!(t.scrollback[0].len(), wide, "old scrollback keeps its width");

        // Now flood narrow rows. Each scroll is at capacity, so it recycles a
        // previously-WIDE buffer to hold a NARROW (3-cell) row. After enough
        // pushes the wide originals are gone and every slot is a recycled buffer.
        for ch in ['a', 'b', 'c', 'd'] {
            let line: String = std::iter::repeat(ch).take(narrow).collect();
            t.feed(&line);
            t.feed("\r\n");
        }
        assert_eq!(t.scrollback_len(), limit);
        // Both rows must be exactly `narrow` wide — clear() dropped the old
        // wider length, leaving no leftover 'W'/'X' tail past column 2.
        for slot in 0..limit {
            assert_eq!(
                t.scrollback[slot].len(),
                narrow,
                "recycled wide buffer must shrink to the narrow row's width, no tail"
            );
        }
        // Content is the last two narrow lines ('c', then 'd'), uncorrupted.
        assert!(t.scrollback[0].iter().all(|c| c.ch == 'c'));
        assert!(t.scrollback[1].iter().all(|c| c.ch == 'd'));
    }

    #[test]
    fn freed_bottom_row_after_scroll_is_fully_blanked_and_dirty() {
        // The grid optimization blanks freed scroll-region rows with an
        // unconditional `fill(blank) + mark_dirty` (replacing the change-gated
        // `clear_row`). Existing damage tests assert the freed row is *dirty*;
        // this additionally pins that the freed row's *content* is fully blanked
        // across every column — the end-state guaranteed by the new `fill`.
        let mut t = Terminal::new(6, 3, 100);
        t.feed("aaa\r\nbbb\r\nccc"); // rows 0..2 filled, cursor at bottom
        t.clear_row_damage();
        t.feed("\x1b[3;1H\n"); // cursor to bottom, bare LF -> full-screen scroll up 1
        // Every cell of the freed bottom row is blank — no surviving 'ccc'.
        for col in 0..t.cols {
            assert_eq!(
                t.primary.get(2, col).ch,
                ' ',
                "freed bottom row col {col} must be fully blanked after scroll"
            );
        }
        // And the freed row is marked dirty so the renderer re-emits it.
        assert!(
            t.row_damage()[2],
            "freed bottom row must be marked dirty after scroll"
        );
    }
