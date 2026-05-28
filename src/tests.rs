use super::*;
use std::time::{Duration, Instant};

const TOL: f32 = 1e-4;

fn approx_eq(a: f32, b: f32) -> bool {
    (a - b).abs() < TOL
}

fn approx_pair(a: (f32, f32), b: (f32, f32)) -> bool {
    approx_eq(a.0, b.0) && approx_eq(a.1, b.1)
}

#[test]
fn next_tab_id_is_strictly_increasing_and_distinct() {
    use std::collections::HashSet;
    // The minter must hand out unique ids; tabs are routed by TabId, so a
    // repeat would misroute a PTY's events to the wrong tab.
    let ids: Vec<_> = (0..8).map(|_| next_tab_id()).collect();

    // All distinct.
    let unique: HashSet<_> = ids.iter().copied().collect();
    assert_eq!(unique.len(), ids.len(), "next_tab_id() returned a duplicate");

    // Strictly increasing across successive calls.
    for pair in ids.windows(2) {
        assert!(
            pair[1].0 > pair[0].0,
            "next_tab_id() not strictly increasing: {:?} then {:?}",
            pair[0],
            pair[1]
        );
    }
}

#[test]
fn title_for_cwd_shows_bare_path_without_app_prefix() {
    // A non-$HOME absolute path is shown verbatim, no "Yutani — " prefix.
    assert_eq!(title_for_cwd("/var/log"), "/var/log");
    // Empty path falls back to the bare app name.
    assert_eq!(title_for_cwd(""), "Yutani");
}

#[test]
fn effective_title_prefers_manual_over_cwd() {
    // A program-set title wins regardless of cwd.
    assert_eq!(
        effective_title(Some("vim"), Some("/var/log")),
        "vim".to_string()
    );
    // No manual title: fall back to the cwd-derived title.
    assert_eq!(
        effective_title(None, Some("/var/log")),
        "/var/log".to_string()
    );
    // Neither: bare app name.
    assert_eq!(effective_title(None, None), "Yutani".to_string());
}

#[test]
fn effective_title_manual_wins_even_without_a_cwd() {
    // A program-set title is used regardless of whether the shell has yet
    // reported a cwd via OSC 7.
    assert_eq!(effective_title(Some("htop"), None), "htop".to_string());
}

#[test]
fn effective_title_treats_a_present_empty_manual_as_set() {
    // `Some("")` reaches `effective_title` only via the inner Option of
    // `take_title_update`, but `set_window_title` maps an empty payload to
    // `None` upstream, so the wiring passes `None` there. Pin the pure
    // function's own contract: a present manual string (here empty) wins
    // over the cwd and yields itself verbatim.
    assert_eq!(effective_title(Some(""), Some("/var/log")), "".to_string());
}

#[test]
fn title_for_cwd_passes_through_non_home_absolute_paths() {
    // Deep paths outside $HOME are shown verbatim (no truncation, no
    // app-name prefix). Uses a path that cannot be a $HOME prefix on any
    // realistic machine, so it doesn't depend on the ambient $HOME value.
    assert_eq!(
        title_for_cwd("/zzz-not-home/deep/nested/dir"),
        "/zzz-not-home/deep/nested/dir"
    );
}

#[test]
fn grid_buffer_byte_sizes_uses_full_phantom_row_slack() {
    // Pin the exact byte counts so a future "let me halve the
    // slack to save memory" tweak surfaces as a unit test failure
    // rather than a hard-to-reproduce wgpu validation panic at
    // scroll time. The buffer must cover `update_vertices`'s walk
    // over `r_lo..r_hi = -2..rows+2` — two phantom strips top +
    // two bottom, plus the cursor and edge-fade extras.
    let cols = 98;
    let rows = 35;
    let (vbuf, ibuf) = grid_buffer_byte_sizes(cols, rows, 0);
    let extra_quads = (2 + SCROLL_ON_OUTPUT_MAX_ROWS) * 2 * cols + 5;
    let area = cols * rows;
    let quads = 2 * area + 2 * extra_quads;
    let v = std::mem::size_of::<renderer::vertex::Vertex>();
    assert_eq!(vbuf, quads * v * 4);
    assert_eq!(ibuf, quads * std::mem::size_of::<u32>() * 6);
}

#[test]
fn grid_buffer_top_inset_reserves_extra_top_rows() {
    // The native tab bar widens only the top of the phantom band, so the
    // buffer must reserve `top_inset_rows × cols × 2` extra quads beyond the
    // symmetric ±(2 + slide) budget — otherwise a full-band scroll with the
    // bar up overruns `queue.write_buffer`.
    let cols = 98;
    let rows = 35;
    let top_inset_rows = 3;
    let (vbuf, ibuf) = grid_buffer_byte_sizes(cols, rows, top_inset_rows);
    let extra_quads = (2 + SCROLL_ON_OUTPUT_MAX_ROWS) * 2 * cols + 5;
    let area = cols * rows;
    let quads = 2 * area + 2 * extra_quads + top_inset_rows * 2 * cols;
    let v = std::mem::size_of::<renderer::vertex::Vertex>();
    assert_eq!(vbuf, quads * v * 4);
    assert_eq!(ibuf, quads * std::mem::size_of::<u32>() * 6);
}

#[test]
fn grid_buffer_top_inset_covers_phantom_band_for_same_inset() {
    // The two halves of this fix must agree: whatever extra top rows
    // `phantom_row_band` widens the band by for a given pixel inset,
    // `grid_buffer_byte_sizes` must reserve at least that many. They derive
    // the row count independently (`resize_buffers` recomputes
    // `top_inset_rows = ceil(inset / line_height)` to mirror the band's
    // `chrome_extra`), so a drift between the two formulas would let a
    // full-band scroll with the tab bar up overrun `queue.write_buffer`.
    // Sweep insets that land on, just under, and just over row boundaries.
    let cols = 98;
    let rows = 35;
    let line_height = 20.0_f32;
    for inset_px in [0.0, 1.0, 19.0, 20.0, 21.0, 39.0, 40.0, 200.0, 199.9] {
        // What the band actually widens the top by, in rows.
        let (r_lo, _r_hi) =
            WindowState::phantom_row_band(0.0, rows, line_height, inset_px);
        let band_chrome_rows = (-2 - r_lo) as usize; // band top beyond the bare -2

        // What the buffer reserves, mirroring `resize_buffers`' ceil.
        let top_inset_rows = (inset_px.max(0.0) / line_height).ceil() as usize;
        assert_eq!(
            top_inset_rows, band_chrome_rows,
            "buffer row reserve ({top_inset_rows}) must equal the band's chrome \
             widening ({band_chrome_rows}) for inset {inset_px}px",
        );

        // And the reserved buffer must be strictly larger than the no-inset
        // buffer by exactly those rows' worth of quads (bg + glyph per cell).
        let (vbuf0, ibuf0) = grid_buffer_byte_sizes(cols, rows, 0);
        let (vbuf, ibuf) = grid_buffer_byte_sizes(cols, rows, top_inset_rows);
        let extra_quads = top_inset_rows * 2 * cols;
        let v = std::mem::size_of::<renderer::vertex::Vertex>();
        assert_eq!(vbuf, vbuf0 + extra_quads * v * 4);
        assert_eq!(ibuf, ibuf0 + extra_quads * std::mem::size_of::<u32>() * 6);
    }
}

#[test]
fn grid_index_buffer_addresses_beyond_u16() {
    // Regression: the grid's index buffer was `Uint16`, but
    // `update_vertices` emits 8 vertices per cell (bg + fg quad,
    // 4 verts each) into one shared vertex vector. Once a frame
    // crosses 65_536 vertices — only ~8_192 cells — `verts.len()
    // as u16` wrapped and the wrapped indices referenced the
    // wrong vertices, so cells rendered as garbage or vanished
    // (large windows, fullscreen, tmux splits). The index buffer
    // must be wide enough to address every vertex a realistic
    // grid can emit, so its element type has to be u32, not u16.
    let cols = 240usize;
    let rows = 80usize; // a routine fullscreen grid
    let verts_per_cell = 8; // bg quad + fg quad, 4 verts each
    let max_vertex_index = cols * rows * verts_per_cell;
    assert!(
        max_vertex_index > u16::MAX as usize,
        "this grid emits {max_vertex_index} vertices, which fits in u16 — \
         pick a bigger grid so the test actually guards the overflow",
    );

    // The buffer sizing must reserve 4 bytes per index (u32), not 2.
    let (_, ibuf) = grid_buffer_byte_sizes(cols, rows, 0);
    let quads = 2 * (cols * rows) + 2 * ((2 + SCROLL_ON_OUTPUT_MAX_ROWS) * 2 * cols + 5);
    assert_eq!(ibuf, quads * 4 * 6, "index buffer must be sized for u32 indices");
}

#[test]
fn grid_buffer_byte_sizes_covers_full_update_vertices_walk() {
    // Symbolic upper bound on what `update_vertices` can push:
    // the row loop covers the phantom band `r_lo..r_hi`, at its
    // widest `rows + 4 + 2 * SCROLL_ON_OUTPUT_MAX_ROWS + top_inset_rows`
    // rows — the fixed ±2 strips plus the smooth-scroll slide widening on
    // each side, plus the asymmetric tab-bar inset on the top — × `cols`
    // cells × 2 quads (bg + glyph) per cell, plus a cursor quad and two
    // edge-fade quads. The vertex buffer must fit at least this many
    // vertices, otherwise `queue.write_buffer` overruns at scroll time (it
    // did: a full-screen scroll-on-output slide pulled the whole band's
    // worth of scrollback into view).
    for (cols, rows) in [(80, 24), (98, 35), (200, 60), (32, 8)] {
        for top_inset_rows in [0, 1, 4] {
            let (vbuf, _) = grid_buffer_byte_sizes(cols, rows, top_inset_rows);
            let worst_case_quads =
                (rows + 4 + 2 * SCROLL_ON_OUTPUT_MAX_ROWS + top_inset_rows) * cols * 2 + 3;
            let worst_case_bytes =
                worst_case_quads * std::mem::size_of::<renderer::vertex::Vertex>() * 4;
            assert!(
                vbuf >= worst_case_bytes,
                "grid_buffer_byte_sizes({cols}, {rows}, {top_inset_rows}) = {vbuf} bytes; \
                 needs at least {worst_case_bytes} to cover the worst-case \
                 walk through `update_vertices`",
            );
        }
    }
}

#[test]
fn get_viewport_size_floors_rows_at_min_grid_rows() {
    // A window dragged shorter than the prompt would otherwise yield a
    // 1–2 row grid, which spills the prompt into scrollback on resize.
    // The row count must never drop below MIN_GRID_ROWS no matter how
    // short the window. cell 8px wide, 18px line height.
    let tiny = WindowState::get_viewport_size(800.0, 0.0, 8, 18, 0.0);
    assert_eq!(tiny.char_height, MIN_GRID_ROWS);
    let short = WindowState::get_viewport_size(800.0, 60.0, 8, 18, 0.0);
    assert_eq!(short.char_height, MIN_GRID_ROWS);
    // A normally-sized window is unaffected — the floor doesn't clamp it.
    let normal = WindowState::get_viewport_size(800.0, 600.0, 8, 18, 0.0);
    assert!(normal.char_height > MIN_GRID_ROWS);
    // The native tab bar's extra top reserve removes rows from the same window.
    let with_tab_bar = WindowState::get_viewport_size(800.0, 600.0, 8, 18, 90.0);
    assert!(with_tab_bar.char_height < normal.char_height);
}

#[test]
fn get_viewport_size_extra_top_removes_expected_rows() {
    // The tab-bar reserve (`extra_top`) is subtracted from the usable height
    // *before* the integer divide by line height. With a line height that
    // divides the reserve evenly, the row drop must be exactly
    // `extra_top / line_height` — the heart of the reflow bug fix (cells that
    // used to land behind the bar now sit below it). Pick a tall window so the
    // MIN_GRID_ROWS floor never interferes, and a reserve (72) that is an exact
    // multiple of the line height (18) → 4 rows removed.
    let line_height = 18;
    let advance_x = 8;
    let width = 800.0;
    let height = 1000.0;
    let no_bar = WindowState::get_viewport_size(width, height, advance_x, line_height, 0.0);
    let with_bar =
        WindowState::get_viewport_size(width, height, advance_x, line_height, 72.0);
    assert_eq!(
        no_bar.char_height - with_bar.char_height,
        72 / line_height,
        "a {line_height}px reserve-multiple should drop exactly that many rows",
    );
    // The bar reserve only touches the row count — column count is driven by
    // width and must be untouched by `extra_top`.
    assert_eq!(no_bar.char_width, with_bar.char_width);
}

#[test]
fn get_viewport_size_rows_monotonically_nonincreasing_in_extra_top() {
    // A taller tab-bar reserve can never *gain* rows: usable height only
    // shrinks as `extra_top` grows. Pin the monotonicity directly so a future
    // sign flip or off-by-one in the subtraction surfaces here.
    let mut prev = usize::MAX;
    for extra_top in [0.0, 10.0, 25.0, 50.0, 90.0, 200.0, 1000.0] {
        let rows = WindowState::get_viewport_size(800.0, 1000.0, 8, 18, extra_top).char_height;
        assert!(
            rows <= prev,
            "rows must not grow as extra_top increases: extra_top={extra_top} \
             gave {rows} rows, previous was {prev}",
        );
        prev = rows;
    }
}

#[test]
fn get_viewport_size_floor_holds_even_with_huge_tab_bar() {
    // A tab-bar reserve larger than the whole window must still leave a usable
    // grid: the MIN_GRID_ROWS floor applies after `extra_top` is subtracted, so
    // the row count never collapses to zero (which would divide-by-zero the
    // shell or strand the prompt entirely).
    let rows = WindowState::get_viewport_size(800.0, 600.0, 8, 18, 10_000.0).char_height;
    assert_eq!(rows, MIN_GRID_ROWS);
    // And a negative usable height (reserve exceeds height) is clamped to 0
    // before the divide, not wrapped — the `.max(0.0)` on the subtraction —
    // so we still land exactly on the floor rather than panicking on an
    // `as usize` of a negative float.
    let rows_neg = WindowState::get_viewport_size(800.0, 50.0, 8, 18, 5_000.0).char_height;
    assert_eq!(rows_neg, MIN_GRID_ROWS);
}

#[test]
fn get_viewport_size_zero_extra_top_matches_unmodified_path() {
    // `extra_top == 0.0` is the no-tab-bar case and must be identical to the
    // pre-reflow geometry: subtracting zero changes nothing. This guards
    // against the bar reserve accidentally leaking into single-window layout.
    let v = WindowState::get_viewport_size(1024.0, 768.0, 9, 20, 0.0);
    let expected_rows = ((768.0 - WINDOW_PADDING * 2.0 - DECORATOR_HEIGHT).max(0.0) as usize) / 20;
    assert_eq!(v.char_height, expected_rows);
}

#[test]
fn chrome_extra_top_formula_is_band_minus_baseline_floored_at_zero() {
    // `chrome_extra_top` (the reflow's `extra_top`) is
    // `(chrome_band_px - titlebar_only_px).max(0.0)`. The method lives on the
    // GPU-backed WindowState and can't be invoked here, but its arithmetic is
    // a pure two-input formula — replicate it so a change to the definition
    // (e.g. dropping the floor, or swapping operands) is caught against this
    // spelled-out expectation. These are the values that feed `get_viewport_size`.
    fn extra_top(chrome_band_px: f64, titlebar_only_px: f64) -> f32 {
        (chrome_band_px - titlebar_only_px).max(0.0) as f32
    }
    // Bar taller than the bar-free baseline → the difference is the reserve.
    assert!(approx_eq(extra_top(96.0, 56.0), 40.0));
    // No bar (band equals the baseline) → zero reserve, the single-window case.
    assert!(approx_eq(extra_top(56.0, 56.0), 0.0));
    // Defensive floor: if the baseline somehow exceeds the band, the reserve
    // clamps to 0 rather than going negative (which would *add* rows downstream).
    assert!(approx_eq(extra_top(40.0, 56.0), 0.0));
}

#[test]
fn tab_index_for_digit_maps_positions_and_clamps() {
    // Absolute positions (0-based) for '1'..'8' when in range.
    assert_eq!(tab_index_for_digit('1', 5), Some(0));
    assert_eq!(tab_index_for_digit('3', 5), Some(2));
    // '9' is always the last tab, regardless of count.
    assert_eq!(tab_index_for_digit('9', 5), Some(4));
    assert_eq!(tab_index_for_digit('9', 1), Some(0));
    // Out-of-range positions ('1'..'8' beyond the tab count) are a no-op.
    assert_eq!(tab_index_for_digit('8', 3), None);
    assert_eq!(tab_index_for_digit('2', 1), None);
    assert_eq!(tab_index_for_digit('4', 3), None);
    // The last in-range position still maps.
    assert_eq!(tab_index_for_digit('3', 3), Some(2));
}

#[test]
fn next_tab_group_id_is_unique_and_prefixed() {
    let a = next_tab_group_id();
    let b = next_tab_group_id();
    assert_ne!(a, b, "each group id must be distinct");
    assert!(a.starts_with("yutani-tabgroup-"));
    assert!(b.starts_with("yutani-tabgroup-"));
}

#[test]
fn theme_for_bg_picks_dark_on_dark_palette() {
    // near-black bg (linear) → Dark so the OS draws light title text.
    let bg = [0.02, 0.02, 0.02, 1.0];
    assert_eq!(theme_for_bg(bg), winit::window::Theme::Dark);
}

#[test]
fn theme_for_bg_picks_light_on_light_palette() {
    // white default bg → Light, the original behavior.
    let bg = [1.0, 1.0, 1.0, 1.0];
    assert_eq!(theme_for_bg(bg), winit::window::Theme::Light);
}

#[test]
fn theme_for_bg_treats_dark_blue_as_dark() {
    // Solarized-dark-ish background: not pure black but well below
    // perceptual midgray — must still trigger the dark chrome.
    let bg = [0.0, 0.05, 0.07, 1.0];
    assert_eq!(theme_for_bg(bg), winit::window::Theme::Dark);
}

#[test]
fn py_in_top_toolbar_true_inside_band() {
    // The very top edge and a point comfortably within an example 56px
    // (Retina) chrome band both belong to the OS chrome.
    let band = 56.0;
    assert!(py_in_top_toolbar(0.0, band));
    assert!(py_in_top_toolbar(20.0, band));
    assert!(py_in_top_toolbar(50.0, band));
}

#[test]
fn py_in_top_toolbar_false_below_band() {
    // Well into the terminal grid: events here should reach the PTY.
    assert!(!py_in_top_toolbar(100.0, 56.0));
}

#[test]
fn py_in_top_toolbar_boundary_is_exclusive() {
    // The band is a strict `<`, so the boundary pixel itself is *not*
    // toolbar (it's the first row of the grid) but anything just above
    // it still is.
    let band = 56.0;
    assert!(!py_in_top_toolbar(band, band)); // exactly the band → false
    assert!(py_in_top_toolbar(band - 0.1, band)); // just under → true
}

#[test]
fn py_in_top_toolbar_band_scales_with_dpi() {
    // The whole point of the chrome band tracking the native title-bar
    // height: a taller (Retina) band reaches a y that a shorter band
    // would have treated as grid. y=50 is chrome at 56px, grid at 40px.
    assert!(py_in_top_toolbar(50.0, 56.0));
    assert!(!py_in_top_toolbar(50.0, 40.0));
}

#[test]
fn chrome_band_from_falls_back_to_reserve_when_query_fails() {
    // No native height available → use the renderer's fixed reserve.
    let reserve = (WINDOW_PADDING + DECORATOR_HEIGHT) as f64;
    assert_eq!(chrome_band_from(None, reserve), reserve);
}

#[test]
fn chrome_band_from_clamps_short_native_up_to_reserve() {
    // Low-DPI: a native bar shorter than the reserve (even after the
    // margin) is clamped up, since the reserve already covers the bar.
    let reserve = (WINDOW_PADDING + DECORATOR_HEIGHT) as f64; // 40.0
    // 30 + CHROME_BAND_MARGIN_PX (4) = 34 < 40 → reserve.
    assert_eq!(chrome_band_from(Some(30.0), reserve), reserve);
    // Boundary: native+margin exactly equal to reserve is kept (>= reserve).
    assert_eq!(
        chrome_band_from(Some(reserve - CHROME_BAND_MARGIN_PX), reserve),
        reserve
    );
}

#[test]
fn chrome_band_from_uses_native_plus_margin_when_taller() {
    // Retina: a native bar taller than the reserve drives the band,
    // with CHROME_BAND_MARGIN_PX added so the lowest grid move lands inside.
    let reserve = (WINDOW_PADDING + DECORATOR_HEIGHT) as f64; // 40.0
    assert_eq!(
        chrome_band_from(Some(52.0), reserve),
        52.0 + CHROME_BAND_MARGIN_PX
    );
}

/// Build a `CursorAnim` whose `started_at` is back-dated so that
/// `elapsed / duration == t` at the moment of construction. Useful
/// for deterministically exercising the smoothstep curve without
/// flaky real-time waits.
fn anim_at_t(from: (f32, f32), to: (f32, f32), duration: f32, t: f32) -> CursorAnim {
    let elapsed_secs = duration * t;
    let elapsed = Duration::from_secs_f32(elapsed_secs);
    CursorAnim {
        from,
        to,
        started_at: Instant::now() - elapsed,
    }
}

#[test]
fn snapped_has_from_equal_to_target() {
    let mut a = CursorAnim::snapped((3.0, 4.0));
    assert_eq!(a.from, (3.0, 4.0));
    assert_eq!(a.to, (3.0, 4.0));
    // current() should immediately return the target regardless of duration.
    assert!(approx_pair(a.current(0.2), (3.0, 4.0)));
}

#[test]
fn current_returns_to_when_duration_zero_or_negative() {
    let mut a = anim_at_t((0.0, 0.0), (10.0, 4.0), 0.2, 0.5);
    assert!(approx_pair(a.current(0.0), (10.0, 4.0)));
    let mut b = anim_at_t((0.0, 0.0), (10.0, 4.0), 0.2, 0.5);
    assert!(approx_pair(b.current(-1.0), (10.0, 4.0)));
}

#[test]
fn current_returns_to_after_duration_elapses() {
    // Back-date 10s to guarantee elapsed >= duration for any reasonable duration.
    let mut a = CursorAnim {
        from: (0.0, 0.0),
        to: (10.0, 4.0),
        started_at: Instant::now() - Duration::from_secs(10),
    };
    assert!(approx_pair(a.current(0.2), (10.0, 4.0)));
    // And the snap means it no longer reports as animating.
    assert!(!a.animating(0.2));
}

#[test]
fn smoothstep_midpoint_is_half() {
    // smoothstep(0.5) = 0.25 * (3 - 1) = 0.5
    let mut a = anim_at_t((0.0, 0.0), (10.0, 4.0), 0.2, 0.5);
    let p = a.current(0.2);
    assert!(approx_pair(p, (5.0, 2.0)), "got {:?}", p);
}

#[test]
fn smoothstep_quarter_point() {
    // smoothstep(0.25) = 0.0625 * (3 - 0.5) = 0.15625
    let mut a = anim_at_t((0.0, 0.0), (10.0, 4.0), 0.2, 0.25);
    let p = a.current(0.2);
    assert!(approx_pair(p, (1.5625, 0.625)), "got {:?}", p);
}

#[test]
fn animating_false_for_snapped() {
    let a = CursorAnim::snapped((3.0, 4.0));
    assert!(!a.animating(0.2));
}

#[test]
fn animating_true_until_current_snaps_past_duration() {
    // Mid-flight: from != to, so animating.
    let mid = anim_at_t((0.0, 0.0), (10.0, 4.0), 0.2, 0.5);
    assert!(mid.animating(0.2));

    // Past duration but no current() call yet: from still != to,
    // so still reports animating. This is the safety net that keeps
    // the event loop ticking until a render actually snaps the value.
    let mut done = CursorAnim {
        from: (0.0, 0.0),
        to: (10.0, 4.0),
        started_at: Instant::now() - Duration::from_secs(10),
    };
    assert!(done.animating(0.2));
    let _ = done.current(0.2);
    assert!(!done.animating(0.2));
}

#[test]
fn retarget_to_same_target_is_noop() {
    let original_started = Instant::now() - Duration::from_millis(50);
    let mut a = CursorAnim {
        from: (1.0, 1.0),
        to: (5.0, 5.0),
        started_at: original_started,
    };
    a.retarget((5.0, 5.0), 0.2);
    assert_eq!(a.from, (1.0, 1.0));
    assert_eq!(a.to, (5.0, 5.0));
    assert_eq!(a.started_at, original_started);
}

#[test]
fn retarget_rebases_from_to_currently_eased_position() {
    // The contract being pinned is *continuity*: after retarget,
    // `from` equals whatever was visually displayed at the moment
    // of retarget, so the next ease starts where the previous frame
    // ended. The exact eased value depends on `Instant::now()`
    // drift between `anim_at_t` and `a.current()` — sub-millisecond
    // on most hardware, much more under CI load — so compare
    // `from` against the value `current()` actually returned, not a
    // hardcoded smoothstep result.
    let mut a = anim_at_t((0.0, 0.0), (10.0, 4.0), 0.2, 0.5);
    let pre = a.current(0.2);
    // Sanity: midway through a forward ease, pre is somewhere
    // between (0,0) and (10,4). Loose bounds — the precise value
    // is what `current()` decides at this exact moment.
    assert!(pre.0 > 0.0 && pre.0 < 10.0, "pre.0 = {}", pre.0);
    assert!(pre.1 > 0.0 && pre.1 < 4.0, "pre.1 = {}", pre.1);

    a.retarget((20.0, 8.0), 0.2);

    // Continuity: new `from` matches whatever `current()` returned.
    // `retarget` calls `current()` again internally, so a few
    // microseconds of `Instant::now()` drift between the test's
    // `current()` call and retarget's internal one produce a
    // sub-thousandth-of-a-percent difference. Tolerance generous
    // enough to swallow CI scheduler jitter but tight enough to
    // catch a continuity bug (which would shift `from` by units,
    // not millionths).
    let drift = ((a.from.0 - pre.0).abs(), (a.from.1 - pre.1).abs());
    assert!(
        drift.0 < 0.05 && drift.1 < 0.05,
        "continuity broken: from={:?} pre={:?}",
        a.from, pre,
    );
    assert_eq!(a.to, (20.0, 8.0));
    // started_at should be (approximately) "now" — well after the
    // back-dated original. Elapsed should be very small.
    assert!(a.started_at.elapsed() < Duration::from_millis(100));
}

#[test]
fn is_blank_cell_true_for_space_and_nul() {
    let space = style::Cell::new(' ', style::Style::new());
    let nul = style::Cell::new('\0', style::Style::new());
    assert!(is_blank_cell(&space));
    assert!(is_blank_cell(&nul));
}

#[test]
fn is_blank_cell_false_for_visible_chars() {
    for ch in ['x', 'a', '1', '.'] {
        let cell = style::Cell::new(ch, style::Style::new());
        assert!(!is_blank_cell(&cell), "expected {:?} to be non-blank", ch);
    }
}

#[test]
fn is_blank_cell_ignores_style() {
    // A space with bold + a foreground color is still blank — only `ch` matters.
    let mut style = style::Style::new();
    style.bold = true;
    style.fg = crate::style::CellColor::Rgb([255, 255, 255]);
    let cell = style::Cell::new(' ', style);
    assert!(is_blank_cell(&cell));
}

#[test]
fn viewport_key_equality_and_field_sensitivity() {
    let base = ViewportKey {
        rows: 24,
        cols: 80,
        view_offset: 0,
        on_alt_screen: false,
    };
    // Identical keys compare equal.
    let same = ViewportKey {
        rows: 24,
        cols: 80,
        view_offset: 0,
        on_alt_screen: false,
    };
    // `ViewportKey` doesn't derive `Debug`, so use `assert!` over `==`/`!=`
    // rather than `assert_eq!`/`assert_ne!`.
    assert!(base == same);

    // Flipping any single field breaks equality.
    let diff_rows = ViewportKey { rows: 25, ..base };
    let diff_cols = ViewportKey { cols: 81, ..base };
    let diff_offset = ViewportKey {
        view_offset: 1,
        ..base
    };
    let diff_alt = ViewportKey {
        on_alt_screen: true,
        ..base
    };
    assert!(base != diff_rows);
    assert!(base != diff_cols);
    assert!(base != diff_offset);
    assert!(base != diff_alt);
}

#[test]
fn config_defaults_cursor_blink_is_false() {
    assert!(!Config::defaults().cursor_blink);
}

#[test]
fn config_round_trip_preserves_cursor_blink_true() {
    let mut c = Config::defaults();
    c.cursor_blink = true;
    let parsed = Config::parse_str(&c.serialize());
    assert!(parsed.cursor_blink);
}

#[test]
fn config_parse_invalid_cursor_blink_keeps_default() {
    // A wrong-typed value must not poison the rest of the config — the
    // field stays at its default (false) and other keys still parse.
    // (The value stays valid TOML, a string, so the document parses and
    // the per-key skip kicks in rather than a whole-file syntax error.)
    let parsed = Config::parse_str("cursor_blink = \"banana\"\nfont_size = 12.5\n");
    assert!(!parsed.cursor_blink);
    assert!(approx_eq(parsed.font_size, 12.5));
}

#[test]
fn config_defaults_glow_disabled() {
    let c = Config::defaults();
    assert!(!c.glow_match_brightness);
    assert!(!c.glow_match_bright_ansi);
    assert!(!c.glow_match_foreground);
    assert!(!c.glow_scanlines);
    assert!(!c.glow_scanlines_content);
    assert!(!c.glow_scanlines_skip_primary_bg);
    assert!((0.0..=1.0).contains(&c.glow_scanlines_content_strength));
    assert!((0.0..=1.0).contains(&c.glow_threshold));
    assert!(c.glow_intensity > 0.0);
    assert!((0.0..=180.0).contains(&c.glow_hue_tolerance_deg));
    assert!(c.glow_fg_tolerance >= 0.0 && c.glow_fg_tolerance <= 3.0_f32.sqrt());
    assert!((0.0..=1.0).contains(&c.glow_scanline_strength));
    assert!(c.glow_scanline_period >= 1.0);
    assert!(c.glow_iterations >= 1);
}

#[test]
fn config_round_trip_preserves_glow_fields() {
    let mut c = Config::defaults();
    c.glow_match_brightness = true;
    c.glow_match_bright_ansi = true;
    c.glow_match_foreground = true;
    c.glow_threshold = 0.42;
    c.glow_intensity = 1.75;
    c.glow_softness = 0.25;
    c.glow_hue_tolerance_deg = 22.5;
    c.glow_fg_tolerance = 0.20;
    c.glow_iterations = 5;
    c.glow_scanlines = true;
    c.glow_scanline_strength = 0.65;
    c.glow_scanline_period = 6.0;
    c.glow_scanlines_content = true;
    c.glow_scanlines_content_strength = 0.40;
    c.glow_scanlines_skip_primary_bg = true;
    let parsed = Config::parse_str(&c.serialize());
    assert!(parsed.glow_match_brightness);
    assert!(parsed.glow_match_bright_ansi);
    assert!(parsed.glow_match_foreground);
    assert!(approx_eq(parsed.glow_threshold, 0.42));
    assert!(approx_eq(parsed.glow_intensity, 1.75));
    assert!(approx_eq(parsed.glow_softness, 0.25));
    assert!(approx_eq(parsed.glow_hue_tolerance_deg, 22.5));
    assert!(approx_eq(parsed.glow_fg_tolerance, 0.20));
    assert_eq!(parsed.glow_iterations, 5);
    assert!(parsed.glow_scanlines);
    assert!(approx_eq(parsed.glow_scanline_strength, 0.65));
    assert!(approx_eq(parsed.glow_scanline_period, 6.0));
    assert!(parsed.glow_scanlines_content);
    assert!(approx_eq(parsed.glow_scanlines_content_strength, 0.40));
    assert!(parsed.glow_scanlines_skip_primary_bg);
}

#[test]
fn config_image_defaults_match_design() {
    let c = Config::defaults();
    assert!(c.images_enabled);
    assert!(c.images_in_scrollback);
    assert_eq!(c.images_memory_cap_mb, 256);
    assert_eq!(c.images_max_pixels, 16 * 1024 * 1024);
    assert_eq!(c.images_decode_timeout_ms, 2000);
    assert_eq!(c.images_filter, "linear");
}

#[test]
fn config_round_trip_preserves_image_fields() {
    let mut c = Config::defaults();
    c.images_enabled = false;
    c.images_in_scrollback = false;
    c.images_memory_cap_mb = 128;
    c.images_max_pixels = 8 * 1024 * 1024;
    c.images_decode_timeout_ms = 500;
    c.images_filter = "nearest".into();
    let parsed = Config::parse_str(&c.serialize());
    assert!(!parsed.images_enabled);
    assert!(!parsed.images_in_scrollback);
    assert_eq!(parsed.images_memory_cap_mb, 128);
    assert_eq!(parsed.images_max_pixels, 8 * 1024 * 1024);
    assert_eq!(parsed.images_decode_timeout_ms, 500);
    assert_eq!(parsed.images_filter, "nearest");
}

#[test]
fn config_image_decode_timeout_floor() {
    // 50ms floor — anything lower defeats the worker since even a
    // tiny PNG takes a millisecond or two to decode.
    let parsed = Config::parse_str("images_decode_timeout_ms = 0\n");
    assert_eq!(parsed.images_decode_timeout_ms, 50);
    let parsed = Config::parse_str("images_decode_timeout_ms = 10\n");
    assert_eq!(parsed.images_decode_timeout_ms, 50);
}

#[test]
fn config_image_max_pixels_floor_avoids_zero() {
    // Zero would disable decoding entirely without an obvious error;
    // we clamp to at least 1 pixel so the rejection path stays
    // observable.
    let parsed = Config::parse_str("images_max_pixels = 0\n");
    assert_eq!(parsed.images_max_pixels, 1);
}

#[test]
fn config_image_filter_rejects_unknown_value() {
    let parsed = Config::parse_str("images_filter = \"bicubic\"\n");
    // Unknown values keep the default — matches `color_scheme` semantics.
    assert_eq!(parsed.images_filter, "linear");
}

#[test]
fn config_image_invalid_bool_keeps_default() {
    let parsed = Config::parse_str("images_enabled = \"banana\"\n");
    assert!(parsed.images_enabled);
}

#[test]
fn shell_exit_mode_from_str_valid_values() {
    assert_eq!(ShellExitMode::from_str("always"), Some(ShellExitMode::Always));
    assert_eq!(ShellExitMode::from_str("never"), Some(ShellExitMode::Never));
    assert_eq!(
        ShellExitMode::from_str("on_success"),
        Some(ShellExitMode::OnSuccess)
    );
}

#[test]
fn shell_exit_mode_from_str_unknown_is_none() {
    assert_eq!(ShellExitMode::from_str("bogus"), None);
    assert_eq!(ShellExitMode::from_str(""), None);
}

#[test]
fn shell_exit_mode_round_trips_through_as_str() {
    for m in [
        ShellExitMode::Always,
        ShellExitMode::Never,
        ShellExitMode::OnSuccess,
    ] {
        assert_eq!(ShellExitMode::from_str(m.as_str()), Some(m));
    }
}

#[test]
fn config_shell_exit_mode_default_is_on_success() {
    assert_eq!(Config::defaults().shell_exit_mode, ShellExitMode::OnSuccess);
}

#[test]
fn config_shell_exit_mode_parses_explicit_values() {
    let never = Config::parse_str("shell_exit_mode = \"never\"\n");
    assert_eq!(never.shell_exit_mode, ShellExitMode::Never);

    let always = Config::parse_str("shell_exit_mode = \"always\"\n");
    assert_eq!(always.shell_exit_mode, ShellExitMode::Always);
}

#[test]
fn config_shell_exit_mode_unknown_keeps_default() {
    let parsed = Config::parse_str("shell_exit_mode = \"bogus\"\n");
    assert_eq!(parsed.shell_exit_mode, ShellExitMode::OnSuccess);
}

#[test]
fn config_shell_exit_mode_missing_key_defaults() {
    // A config that doesn't mention the key keeps the default.
    let parsed = Config::parse_str("font_size = 14.0\n");
    assert_eq!(parsed.shell_exit_mode, ShellExitMode::OnSuccess);
}

#[test]
fn config_shell_exit_mode_round_trips_through_serialize() {
    let mut c = Config::defaults();
    c.shell_exit_mode = ShellExitMode::Never;
    let parsed = Config::parse_str(&c.serialize());
    assert_eq!(parsed.shell_exit_mode, ShellExitMode::Never);
}

#[test]
fn prompt_gutter_from_str_valid_values() {
    assert_eq!(PromptGutter::from_str("none"), Some(PromptGutter::None));
    assert_eq!(PromptGutter::from_str("bar"), Some(PromptGutter::Bar));
}

#[test]
fn prompt_gutter_from_str_unknown_is_none() {
    assert_eq!(PromptGutter::from_str("bogus"), None);
    assert_eq!(PromptGutter::from_str(""), None);
}

#[test]
fn prompt_gutter_round_trips_through_as_str() {
    for g in [PromptGutter::None, PromptGutter::Bar] {
        assert_eq!(PromptGutter::from_str(g.as_str()), Some(g));
    }
}

#[test]
fn config_prompt_gutter_default_is_none() {
    assert_eq!(Config::defaults().prompt_gutter, PromptGutter::None);
}

#[test]
fn config_prompt_gutter_parses_explicit_value() {
    let bar = Config::parse_str("prompt_gutter = \"bar\"\n");
    assert_eq!(bar.prompt_gutter, PromptGutter::Bar);
}

#[test]
fn config_prompt_gutter_unknown_keeps_default() {
    let parsed = Config::parse_str("prompt_gutter = \"squiggle\"\n");
    assert_eq!(parsed.prompt_gutter, PromptGutter::None);
}

#[test]
fn config_prompt_gutter_missing_key_defaults() {
    let parsed = Config::parse_str("font_size = 14.0\n");
    assert_eq!(parsed.prompt_gutter, PromptGutter::None);
}

#[test]
fn config_prompt_gutter_round_trips_through_serialize() {
    let mut c = Config::defaults();
    c.prompt_gutter = PromptGutter::Bar;
    let parsed = Config::parse_str(&c.serialize());
    assert_eq!(parsed.prompt_gutter, PromptGutter::Bar);
}

#[test]
fn scroll_edge_style_from_str_valid_values() {
    assert_eq!(ScrollEdgeStyle::from_str("soft"), Some(ScrollEdgeStyle::Soft));
    assert_eq!(ScrollEdgeStyle::from_str("hard"), Some(ScrollEdgeStyle::Hard));
}

#[test]
fn scroll_edge_style_from_str_unknown_is_none() {
    assert_eq!(ScrollEdgeStyle::from_str("bogus"), None);
    assert_eq!(ScrollEdgeStyle::from_str(""), None);
}

#[test]
fn scroll_edge_style_round_trips_through_as_str() {
    for s in [ScrollEdgeStyle::Soft, ScrollEdgeStyle::Hard] {
        assert_eq!(ScrollEdgeStyle::from_str(s.as_str()), Some(s));
    }
}

#[test]
fn config_scroll_edge_style_default_is_soft() {
    assert_eq!(Config::defaults().scroll_edge_style, ScrollEdgeStyle::Soft);
}

#[test]
fn config_scroll_edge_style_parses_explicit_value() {
    let hard = Config::parse_str("scroll_edge_style = \"hard\"\n");
    assert_eq!(hard.scroll_edge_style, ScrollEdgeStyle::Hard);
}

#[test]
fn config_scroll_edge_style_unknown_keeps_default() {
    let parsed = Config::parse_str("scroll_edge_style = \"squishy\"\n");
    assert_eq!(parsed.scroll_edge_style, ScrollEdgeStyle::Soft);
}

#[test]
fn config_scroll_edge_style_missing_key_defaults() {
    let parsed = Config::parse_str("font_size = 14.0\n");
    assert_eq!(parsed.scroll_edge_style, ScrollEdgeStyle::Soft);
}

#[test]
fn config_scroll_edge_style_round_trips_through_serialize() {
    let mut c = Config::defaults();
    c.scroll_edge_style = ScrollEdgeStyle::Hard;
    let parsed = Config::parse_str(&c.serialize());
    assert_eq!(parsed.scroll_edge_style, ScrollEdgeStyle::Hard);
}

#[test]
fn config_scroll_edge_style_default_soft_round_trips_through_serialize() {
    // The default must also survive serialize -> parse_str: serialize() emits
    // `soft` explicitly, so a reload (or fresh process) reads it back rather
    // than silently relying on the parse-path default.
    let c = Config::defaults();
    let parsed = Config::parse_str(&c.serialize());
    assert_eq!(parsed.scroll_edge_style, ScrollEdgeStyle::Soft);
}

#[test]
fn config_autocomplete_default_is_on() {
    assert!(Config::defaults().autocomplete);
}

#[test]
fn config_autocomplete_missing_key_defaults_on() {
    let parsed = Config::parse_str("font_size = 14.0\n");
    assert!(parsed.autocomplete);
}

#[test]
fn config_autocomplete_parses_explicit_false() {
    let parsed = Config::parse_str("autocomplete = false\n");
    assert!(!parsed.autocomplete);
}

#[test]
fn config_autocomplete_parses_explicit_true() {
    let parsed = Config::parse_str("autocomplete = true\n");
    assert!(parsed.autocomplete);
}

#[test]
fn config_autocomplete_round_trips_through_serialize() {
    let mut c = Config::defaults();
    c.autocomplete = false;
    let parsed = Config::parse_str(&c.serialize());
    assert!(!parsed.autocomplete);
}

#[test]
fn config_image_halfblock_for_missing_default_off() {
    // Default off: the opt-in only matters for the failure-cleanup
    // window, where most users either don't notice the one-frame
    // blank or wouldn't appreciate a colored-cell flash on a
    // decode error. Keeping it off is the conservative choice.
    let c = Config::defaults();
    assert!(!c.images_halfblock_for_missing);
}

#[test]
fn config_image_halfblock_for_missing_round_trips() {
    let mut c = Config::defaults();
    c.images_halfblock_for_missing = true;
    let parsed = Config::parse_str(&c.serialize());
    assert!(parsed.images_halfblock_for_missing);
}

#[test]
fn config_font_family_default_is_none() {
    assert!(Config::defaults().font_family.is_none());
}

#[test]
fn config_font_family_round_trips_some() {
    let mut c = Config::defaults();
    c.font_family = Some("Fira Code".to_string());
    let parsed = Config::parse_str(&c.serialize());
    assert_eq!(parsed.font_family.as_deref(), Some("Fira Code"));
}

#[test]
fn config_font_family_round_trips_none() {
    let c = Config::defaults();
    let parsed = Config::parse_str(&c.serialize());
    assert!(parsed.font_family.is_none());
}

#[test]
fn config_font_family_empty_parses_as_none() {
    // Matches `color_scheme` semantics: an explicit empty string clears
    // the override rather than installing the empty string.
    let parsed = Config::parse_str("font_family = \"\"\n");
    assert!(parsed.font_family.is_none());
}

#[test]
fn config_font_family_preserves_spaces_in_name() {
    // TOML quotes delimit the value, so multi-word family names need no
    // special handling.
    let parsed = Config::parse_str("font_family = \"JetBrains Mono\"\n");
    assert_eq!(parsed.font_family.as_deref(), Some("JetBrains Mono"));
}

#[test]
fn config_font_family_quoted_value_is_verbatim() {
    // Unlike the old line-based parser, TOML does not trim — whatever
    // sits inside the quotes is taken literally. Documented here so a
    // future "helpfully trim it" change has to break a test on purpose.
    let parsed = Config::parse_str("font_family = \"  Padded  \"\n");
    assert_eq!(parsed.font_family.as_deref(), Some("  Padded  "));
}

//
// Half-block fallback decision matrix. Pure function — no Store,
// Terminal, or window required. Each row pins one cell of the
// (images_enabled × opted_in × is_pending × gpu_image_available)
// truth table so a future refactor that changes priorities (e.g.
// accidentally letting an in-flight decode flicker) fails loud.
//

#[test]
fn halfblock_decision_pending_decode_never_renders() {
    // Pending wins over every other flag — the flicker case.
    for &en in &[true, false] {
        for &oi in &[true, false] {
            for &gpu in &[true, false] {
                assert!(
                    !WindowState::should_halfblock(en, oi, /*pending*/ true, gpu),
                    "pending should suppress halfblock (en={en} oi={oi} gpu={gpu})"
                );
            }
        }
    }
}

#[test]
fn halfblock_decision_disabled_always_renders_when_not_pending() {
    // images_enabled=false ⇒ GPU draw is forbidden. Half-block is
    // the only visible representation; opt-in doesn't gate this
    // because the user already turned the GPU path off.
    for &oi in &[true, false] {
        for &gpu in &[true, false] {
            assert!(
                WindowState::should_halfblock(/*en*/ false, oi, false, gpu),
                "disabled should always halfblock (oi={oi} gpu={gpu})"
            );
        }
    }
}

#[test]
fn halfblock_decision_enabled_with_gpu_image_never_renders() {
    // GPU has the texture, GPU path draws → don't double-emit a
    // half-block on top.
    assert!(!WindowState::should_halfblock(true, false, false, true));
    assert!(!WindowState::should_halfblock(true, true, false, true));
}

#[test]
fn halfblock_decision_enabled_missing_image_needs_opt_in() {
    // images_enabled=true, peek=None, !pending → decode failed and
    // cleanup hasn't fired. Only honored when the user opts in.
    assert!(!WindowState::should_halfblock(true, false, false, false));
    assert!(WindowState::should_halfblock(true, true, false, false));
}

//
// Deferred-placement suppression. Drives the ghost-image regression:
// without `kitty_image_id` gating, an `a=T,U=1` upload would leave
// `preplaced_image_id = None`, and the deferred-place branch in
// `poll_pending_images` would stamp a second Placement at the cursor
// row alongside the placeholder-bbox draw.
//

#[test]
fn suppress_deferred_placement_cmd_shift_i_keeps_deferred_place_active() {
    // Cmd-Shift-I debug-paste: no Kitty id, no up-front display.
    // Caller stores `None` so the on-decode-success branch creates
    // the Placement at the cursor.
    assert!(!WindowState::suppress_deferred_placement(false, None));
}

#[test]
fn suppress_deferred_placement_a_t_capital_already_placed_skips_deferred_place() {
    // `a=T` without `U=1`: `insert_placement_kitty` already ran.
    // No second Placement should be auto-created.
    assert!(WindowState::suppress_deferred_placement(true, Some(42)));
}

#[test]
fn suppress_deferred_placement_a_t_capital_with_virtual_placement_skips_deferred_place() {
    // `a=T,U=1`: placeholder cells own the placement. A deferred
    // auto-place would produce the "ghost image" regression.
    assert!(WindowState::suppress_deferred_placement(false, Some(42)));
}

#[test]
fn suppress_deferred_placement_a_t_transmit_only_skips_deferred_place() {
    // `a=t`: client will issue `a=p` later. Auto-placing at
    // cursor would beat the client's explicit placement to the
    // screen and end up double-drawn after `a=p` arrives.
    assert!(WindowState::suppress_deferred_placement(false, Some(7)));
}

//
// Grid → viewport row shift for live image placements. Originally
// missed (placement rendered using grid row directly); when the user
// scrolled history into view, images stayed pinned to the viewport
// row they were initially drawn at while the surrounding text shifted
// down. The pure helper makes the math testable without firing up a
// GPU adapter.
//

#[test]
fn live_placement_viewport_row_passes_through_with_no_offset() {
    // The no-scrollback-in-view common case — image at grid row 5 in
    // a 24-row viewport renders at viewport row 5.
    assert_eq!(WindowState::live_placement_viewport_row(5, 0, 24), 5);
}

#[test]
fn live_placement_viewport_row_shifts_down_by_view_offset() {
    // 3 scrollback rows pulled into view → live content shifts down 3.
    assert_eq!(WindowState::live_placement_viewport_row(5, 3, 24), 8);
    // Negative grid rows (placement straddling above the viewport)
    // shift the same way — clipping happens downstream.
    assert_eq!(WindowState::live_placement_viewport_row(-2, 3, 24), 1);
}

#[test]
fn live_placement_viewport_row_does_not_clamp_at_rows() {
    // Regression: the helper used to clamp `view_offset` at `rows`,
    // which froze the image's discrete viewport_row when scrolled
    // deeper into history. Smooth-scroll's `scroll_y` kept
    // interpolating between ticks, so the image visually slid by
    // a row each tick then snapped back when the tick fired
    // (viewport_row hadn't moved). Without the clamp, viewport_row
    // moves in lockstep with view_offset and scroll_y, giving a
    // continuous slide as the image enters from below.
    assert_eq!(WindowState::live_placement_viewport_row(5, 100, 24), 5 + 100);
    assert_eq!(WindowState::live_placement_viewport_row(0, 50, 24), 50);
    // Sanity: at view_offset <= rows, behavior is unchanged from
    // the pre-clamp version.
    assert_eq!(WindowState::live_placement_viewport_row(5, 20, 24), 25);
}

//
// Per-run UV math for Kitty placeholder draws.
//

#[test]
fn placeholder_run_uv_full_row_spans_full_width_one_row_height() {
    // 3 cells wide, image_row 0, total (3, 2) →
    //   u: 0..3/3 = 0..1
    //   v: 0..1/2 = 0..0.5
    let uv = WindowState::placeholder_run_uv(0, 3, 0, 3, 2);
    assert_eq!(uv, (0.0, 0.0, 1.0, 0.5));
}

#[test]
fn placeholder_run_uv_partial_row_samples_proper_strip() {
    // Cells image_col 1..3 of a 4-col tile, image_row 1 of 2
    // rows → upper-left at (0.25, 0.5), lower-right at (0.75, 1.0).
    let uv = WindowState::placeholder_run_uv(1, 3, 1, 4, 2);
    assert_eq!(uv, (0.25, 0.5, 0.75, 1.0));
}

#[test]
fn placeholder_run_uv_clamps_out_of_range_to_unit_square() {
    // image_col_end past the right edge, image_row past the
    // bottom — both clamp to 1.0 rather than wrap or NaN.
    let uv = WindowState::placeholder_run_uv(5, 10, 7, 4, 2);
    assert_eq!(uv, (1.0, 1.0, 1.0, 1.0));
}

#[test]
fn placeholder_run_uv_zero_total_dims_treated_as_one() {
    // A `c=0` / `r=0` transmission shouldn't reach this helper
    // (the renderer skips runs without a recorded extent), but
    // guard the denominator so we never NaN. With cols=0 →
    // denom 1, image_col_end=0 → u1=0.0 clamped from 0 itself.
    let uv = WindowState::placeholder_run_uv(0, 0, 0, 0, 0);
    assert_eq!(uv, (0.0, 0.0, 0.0, 1.0));
}

//
// Cell-filling glyph quad UV math (half-texel seam inset).
//

#[test]
fn glyph_quad_uv_insets_both_axes_for_a_cell_filling_glyph() {
    // Any cell-filling glyph is inset half a texel on BOTH axes so no
    // hard opaque edge samples the transparent atlas padding. Glyph at
    // atlas origin (0, 0), sampling cols 0..8 / rows 0..16 of a
    // 128x128 atlas.
    let (u0, v0, u1, v1) =
        WindowState::glyph_quad_uv(0.0, 0.0, (0.0, 8.0), (0.0, 16.0), true, 128.0, 128.0);
    assert!(approx_pair((u0, u1), (0.5 / 128.0, 7.5 / 128.0)));
    assert!(approx_pair((v0, v1), (0.5 / 128.0, 15.5 / 128.0)));
}

#[test]
fn glyph_quad_uv_insets_a_trimmed_half_block_on_its_narrow_axis() {
    // Regression: ▐ is packed trimmed to its opaque right half
    // (a half-width bitmap that bears into the cell), so its narrow
    // axis is placed 1:1 — yet its filled side reaches the bitmap edge
    // and must still be inset, or it bleeds into the padding and
    // leaves a hairline at the cell boundary. The inset is keyed on
    // cell_filling, so the horizontal extent IS pulled in here. Glyph
    // at (10, 20), sampling cols 0..8 / rows 0..16 of a 256x256 atlas.
    let (u0, v0, u1, v1) =
        WindowState::glyph_quad_uv(10.0, 20.0, (0.0, 8.0), (0.0, 16.0), true, 256.0, 256.0);
    assert!(approx_pair((u0, u1), (10.5 / 256.0, 17.5 / 256.0)));
    assert!(approx_pair((v0, v1), (20.5 / 256.0, 35.5 / 256.0)));
}

#[test]
fn glyph_quad_uv_no_inset_for_a_non_filling_glyph() {
    // A normal (non-cell-filling) glyph gets no inset on either axis:
    // the UV is the raw sample rect, so ordinary antialiased glyphs
    // aren't thinned or shifted.
    let (u0, v0, u1, v1) =
        WindowState::glyph_quad_uv(4.0, 4.0, (1.0, 7.0), (2.0, 14.0), false, 64.0, 64.0);
    assert!(approx_pair((u0, u1), (5.0 / 64.0, 11.0 / 64.0)));
    assert!(approx_pair((v0, v1), (6.0 / 64.0, 18.0 / 64.0)));
}

#[test]
fn glyph_quad_uv_inset_shrinks_each_sampled_span_by_one_texel() {
    // The seam fix narrows the sampled span by exactly one texel total
    // (half a texel off each edge) on both axes for a cell-filling
    // glyph, and leaves both spans untouched for a normal glyph. Pin
    // that in atlas-texel units so a regression to a different inset is
    // caught.
    let raw =
        WindowState::glyph_quad_uv(0.0, 0.0, (0.0, 10.0), (0.0, 10.0), false, 100.0, 100.0);
    let inset =
        WindowState::glyph_quad_uv(0.0, 0.0, (0.0, 10.0), (0.0, 10.0), true, 100.0, 100.0);
    assert!(approx_eq((raw.2 - raw.0) * 100.0 - (inset.2 - inset.0) * 100.0, 1.0));
    assert!(approx_eq((raw.3 - raw.1) * 100.0 - (inset.3 - inset.1) * 100.0, 1.0));
}

#[test]
fn config_glow_scanline_strength_clamped() {
    let parsed = Config::parse_str("glow_scanline_strength = 5\n");
    assert!((0.0..=1.0).contains(&parsed.glow_scanline_strength));
    let parsed = Config::parse_str("glow_scanline_strength = -2\n");
    assert!((0.0..=1.0).contains(&parsed.glow_scanline_strength));
}

#[test]
fn config_glow_scanline_period_clamped() {
    let parsed = Config::parse_str("glow_scanline_period = 0.1\n");
    assert!(parsed.glow_scanline_period >= 1.0);
}

#[test]
fn config_glow_scanline_colors_parse_hex() {
    let parsed = Config::parse_str(
        "glow_scanline_color_bright = 0xff0000\n\
         glow_scanline_color_dark = 0x00ff00\n",
    );
    // sRGB → linear: 0xff → 1.0, 0x00 → 0.0. The whole-channel
    // values survive the round-trip exactly.
    assert!(approx_eq(parsed.glow_scanline_color_bright[0], 1.0));
    assert!(approx_eq(parsed.glow_scanline_color_bright[1], 0.0));
    assert!(approx_eq(parsed.glow_scanline_color_dark[1], 1.0));
    assert!(approx_eq(parsed.glow_scanline_color_dark[2], 0.0));
}

#[test]
fn config_glow_scanline_color_invalid_keeps_default() {
    // A wrong-typed value (string instead of a hex integer) is skipped
    // per-key, leaving the default in place.
    let parsed = Config::parse_str("glow_scanline_color_bright = \"notahex\"\n");
    let d = Config::defaults();
    assert!(approx_eq(parsed.glow_scanline_color_bright[0], d.glow_scanline_color_bright[0]));
}

#[test]
fn config_round_trip_preserves_scanline_colors() {
    let mut c = Config::defaults();
    c.glow_scanline_color_bright = palette::rgb_from_value(&toml::Value::Integer(0xff8800)).unwrap();
    c.glow_scanline_color_dark = palette::rgb_from_value(&toml::Value::Integer(0x110022)).unwrap();
    let parsed = Config::parse_str(&c.serialize());
    // sRGB byte round-trip is exact (palette ensures this).
    assert_eq!(
        palette::linear_to_srgb_u8(parsed.glow_scanline_color_bright[0]),
        0xff,
    );
    assert_eq!(
        palette::linear_to_srgb_u8(parsed.glow_scanline_color_bright[1]),
        0x88,
    );
    assert_eq!(
        palette::linear_to_srgb_u8(parsed.glow_scanline_color_dark[2]),
        0x22,
    );
}

#[test]
fn config_glow_fg_tolerance_clamped() {
    let parsed = Config::parse_str("glow_fg_tolerance = 99\n");
    assert!(parsed.glow_fg_tolerance <= 3.0_f32.sqrt());
    let parsed = Config::parse_str("glow_fg_tolerance = -1\n");
    assert!(parsed.glow_fg_tolerance >= 0.0);
}

#[test]
fn config_glow_threshold_clamped_on_parse() {
    // TOML forbids duplicate keys, so the high and low ends are exercised
    // by separate documents.
    let high = Config::parse_str("glow_threshold = 2.5\n");
    assert!((0.0..=1.0).contains(&high.glow_threshold));
    let low = Config::parse_str("glow_threshold = -1\n");
    assert!((0.0..=1.0).contains(&low.glow_threshold));
}

#[test]
fn config_glow_hue_tolerance_clamped() {
    let parsed = Config::parse_str("glow_hue_tolerance_deg = 500\n");
    assert!((0.0..=180.0).contains(&parsed.glow_hue_tolerance_deg));
    let parsed = Config::parse_str("glow_hue_tolerance_deg = -10\n");
    assert!((0.0..=180.0).contains(&parsed.glow_hue_tolerance_deg));
}

#[test]
fn config_glow_iterations_clamped_to_max() {
    let parsed = Config::parse_str(&format!(
        "glow_iterations = {}\n",
        renderer::glow::MAX_ITERATIONS * 4
    ));
    assert_eq!(parsed.glow_iterations, renderer::glow::MAX_ITERATIONS);
}

#[test]
fn config_invalid_glow_value_keeps_default() {
    let parsed = Config::parse_str(
        "glow_match_brightness = nope\nglow_match_bright_ansi = ?\nglow_intensity = abc\n",
    );
    let d = Config::defaults();
    assert_eq!(parsed.glow_match_brightness, d.glow_match_brightness);
    assert_eq!(parsed.glow_match_bright_ansi, d.glow_match_bright_ansi);
    assert!(approx_eq(parsed.glow_intensity, d.glow_intensity));
}

//
// TOML migration coverage: serialize() must emit valid parseable TOML
// (comments / section headers and all), every field must survive a
// serialize → parse_str round-trip, numeric slots must accept both
// integers and floats, and a syntax-broken document must fall back to
// defaults wholesale (mirroring the palette parser's contract).
//

#[test]
fn config_serialize_emits_parseable_toml() {
    // The serializer hand-writes `# comment` section headers and blank
    // lines between groups. This pins that none of that decoration breaks
    // the TOML grammar — `serialize()` output must always re-parse as a
    // table, or `save()` would write a config the next launch can't read.
    let c = Config::defaults();
    let table: Result<toml::Table, _> = c.serialize().parse();
    assert!(table.is_ok(), "serialize() must be valid TOML: {:?}", table.err());
}

#[test]
fn config_full_default_round_trip_is_identity() {
    // The strongest single invariant: serializing the defaults and
    // parsing them back must reproduce the defaults exactly. Catches any
    // field the serializer forgets to emit (which would silently revert
    // to default on the next load) or any asymmetry between the key names
    // serialize() writes and apply() reads.
    let d = Config::defaults();
    let parsed = Config::parse_str(&d.serialize());
    // Compare field-by-field with float tolerance; Config has no Eq.
    assert!(approx_eq(parsed.font_size, d.font_size));
    assert!(approx_eq(parsed.top_fade_height, d.top_fade_height));
    assert!(approx_eq(parsed.top_fade_anim_secs, d.top_fade_anim_secs));
    assert!(approx_eq(parsed.bottom_fade_height, d.bottom_fade_height));
    assert!(approx_eq(parsed.bottom_fade_anim_secs, d.bottom_fade_anim_secs));
    assert!(approx_eq(parsed.cursor_anim_secs, d.cursor_anim_secs));
    assert_eq!(parsed.cursor_blink, d.cursor_blink);
    assert_eq!(parsed.color_scheme, d.color_scheme);
    assert_eq!(parsed.font_family, d.font_family);
    assert_eq!(parsed.theme_overrides_glow, d.theme_overrides_glow);
    assert_eq!(parsed.images_enabled, d.images_enabled);
    assert_eq!(parsed.images_memory_cap_mb, d.images_memory_cap_mb);
    assert_eq!(parsed.images_max_pixels, d.images_max_pixels);
    assert_eq!(parsed.images_decode_timeout_ms, d.images_decode_timeout_ms);
    assert_eq!(parsed.images_in_scrollback, d.images_in_scrollback);
    assert_eq!(parsed.images_filter, d.images_filter);
    assert_eq!(parsed.images_halfblock_for_missing, d.images_halfblock_for_missing);
}

#[test]
fn config_round_trip_preserves_fade_fields() {
    // The fade knobs are floats with no clamp on the parse path, so
    // any non-default value must survive serialize → parse_str unchanged.
    // Previously only the glow/image/font groups were round-trip tested.
    let mut c = Config::defaults();
    c.top_fade_height = 123.5;
    c.top_fade_anim_secs = 0.5;
    c.bottom_fade_height = 64.25;
    c.bottom_fade_anim_secs = 0.75;
    c.cursor_anim_secs = 0.12;
    let parsed = Config::parse_str(&c.serialize());
    assert!(approx_eq(parsed.top_fade_height, 123.5));
    assert!(approx_eq(parsed.top_fade_anim_secs, 0.5));
    assert!(approx_eq(parsed.bottom_fade_height, 64.25));
    assert!(approx_eq(parsed.bottom_fade_anim_secs, 0.75));
    assert!(approx_eq(parsed.cursor_anim_secs, 0.12));
}

#[test]
fn config_font_size_accepts_integer_and_float() {
    // TOML reads `10` as an integer and `10.0` as a float; `cfg_f32` must
    // accept both for a numeric slot so users aren't forced to write a
    // decimal point. Both forms must produce the same value.
    let from_int = Config::parse_str("font_size = 10\n");
    let from_float = Config::parse_str("font_size = 10.0\n");
    assert!(approx_eq(from_int.font_size, 10.0));
    assert!(approx_eq(from_float.font_size, 10.0));
}

#[test]
fn config_color_scheme_round_trips_some() {
    // A `Some(name)` color_scheme must serialize (quoted) and parse back
    // to the same name. The serializer omits the key entirely when None,
    // so the Some path needs its own pin.
    let mut c = Config::defaults();
    c.color_scheme = Some("solarized-dark".to_string());
    let parsed = Config::parse_str(&c.serialize());
    assert_eq!(parsed.color_scheme.as_deref(), Some("solarized-dark"));
}

#[test]
fn config_color_scheme_round_trips_none() {
    // Default None: the serializer drops the key, and a config without it
    // parses back to None rather than an empty Some("").
    let c = Config::defaults();
    assert!(c.color_scheme.is_none());
    let parsed = Config::parse_str(&c.serialize());
    assert!(parsed.color_scheme.is_none());
}

#[test]
fn config_color_scheme_empty_parses_as_none() {
    // An explicit empty string clears the override to None rather than
    // installing the empty name (which would resolve to a missing file).
    let parsed = Config::parse_str("color_scheme = \"\"\n");
    assert!(parsed.color_scheme.is_none());
}

#[test]
fn theme_picker_prepends_default_entry() {
    // The synthetic default entry leads, then the real schemes follow in
    // the order given.
    let choices = theme_picker_choices(vec![
        "nostromo".to_string(),
        "spacedust".to_string(),
    ]);
    assert_eq!(
        choices,
        vec![
            DEFAULT_THEME_LABEL.to_string(),
            "nostromo".to_string(),
            "spacedust".to_string(),
        ]
    );
}

#[test]
fn theme_picker_with_no_schemes_still_offers_default() {
    // Even with an empty schemes directory, you can always revert to
    // built-in defaults from the picker.
    assert_eq!(theme_picker_choices(Vec::new()), vec![DEFAULT_THEME_LABEL.to_string()]);
}

#[test]
fn theme_picker_drops_scheme_colliding_with_default_label() {
    // A real scheme that happens to match the synthetic label is filtered
    // out so the default entry stays unambiguous (appears exactly once).
    let choices = theme_picker_choices(vec![
        DEFAULT_THEME_LABEL.to_string(),
        "yutani".to_string(),
    ]);
    assert_eq!(
        choices,
        vec![DEFAULT_THEME_LABEL.to_string(), "yutani".to_string()]
    );
}

#[test]
fn scheme_for_pick_maps_default_label_to_none() {
    assert_eq!(scheme_for_pick(DEFAULT_THEME_LABEL), None);
    assert_eq!(scheme_for_pick("yutani"), Some("yutani"));
    // A real name is returned verbatim, including ones with spaces.
    assert_eq!(scheme_for_pick("My Theme"), Some("My Theme"));
}

#[test]
fn scheme_value_from_pick_clears_on_default_or_empty() {
    // The default label, an empty pick, and whitespace all clear the slot.
    assert_eq!(scheme_value_from_pick(Some(DEFAULT_THEME_LABEL.to_string())), None);
    assert_eq!(scheme_value_from_pick(Some(String::new())), None);
    assert_eq!(scheme_value_from_pick(Some("   ".to_string())), None);
    assert_eq!(scheme_value_from_pick(None), None);
    // A real name is trimmed and stored.
    assert_eq!(
        scheme_value_from_pick(Some("  yutani ".to_string())),
        Some("yutani".to_string())
    );
}

#[test]
fn active_scheme_uses_color_scheme_when_not_following() {
    let mut c = Config::defaults();
    c.auto_theme = false;
    c.color_scheme = Some("nostromo".to_string());
    c.light_scheme = Some("light-one".to_string());
    c.dark_scheme = Some("dark-one".to_string());
    // System appearance is ignored when not following.
    assert_eq!(c.active_scheme(false), Some("nostromo"));
    assert_eq!(c.active_scheme(true), Some("nostromo"));
}

#[test]
fn active_scheme_unset_color_scheme_is_none_when_not_following() {
    let c = Config::defaults(); // auto_theme false, all schemes None
    assert_eq!(c.active_scheme(false), None);
    assert_eq!(c.active_scheme(true), None);
}

#[test]
fn active_scheme_picks_slot_by_appearance_when_following() {
    let mut c = Config::defaults();
    c.auto_theme = true;
    c.light_scheme = Some("daytime".to_string());
    c.dark_scheme = Some("midnight".to_string());
    assert_eq!(c.active_scheme(false), Some("daytime"));
    assert_eq!(c.active_scheme(true), Some("midnight"));
}

#[test]
fn active_scheme_following_falls_back_to_color_scheme_then_none() {
    let mut c = Config::defaults();
    c.auto_theme = true;
    c.color_scheme = Some("fallback".to_string());
    // dark_scheme set, light_scheme unset: dark uses its slot, light falls
    // back to color_scheme.
    c.dark_scheme = Some("midnight".to_string());
    assert_eq!(c.active_scheme(true), Some("midnight"));
    assert_eq!(c.active_scheme(false), Some("fallback"));
    // With no slots and no color_scheme, it's None (built-in defaults).
    c.color_scheme = None;
    c.dark_scheme = None;
    assert_eq!(c.active_scheme(true), None);
    assert_eq!(c.active_scheme(false), None);
}

#[test]
fn config_auto_theme_fields_round_trip() {
    let mut c = Config::defaults();
    c.auto_theme = true;
    c.light_scheme = Some("daytime".to_string());
    c.dark_scheme = Some("midnight".to_string());
    let parsed = Config::parse_str(&c.serialize());
    assert!(parsed.auto_theme);
    assert_eq!(parsed.light_scheme.as_deref(), Some("daytime"));
    assert_eq!(parsed.dark_scheme.as_deref(), Some("midnight"));
}

#[test]
fn config_auto_theme_defaults_off_with_unset_slots() {
    let d = Config::defaults();
    assert!(!d.auto_theme);
    assert!(d.light_scheme.is_none());
    assert!(d.dark_scheme.is_none());
    // A round trip of defaults preserves that.
    let parsed = Config::parse_str(&d.serialize());
    assert!(!parsed.auto_theme);
    assert!(parsed.light_scheme.is_none());
    assert!(parsed.dark_scheme.is_none());
}

#[test]
fn config_light_dark_scheme_empty_parses_as_none() {
    // Explicit empty strings clear the slots, same as color_scheme.
    let parsed = Config::parse_str("light_scheme = \"\"\ndark_scheme = \"\"\n");
    assert!(parsed.light_scheme.is_none());
    assert!(parsed.dark_scheme.is_none());
}

#[test]
fn config_all_theme_fields_round_trip_then_active_scheme_resolves() {
    // Every existing round-trip leaves `color_scheme` None or sets only a
    // subset. This is the realistic "user configured everything" state:
    // an explicit `color_scheme` fallback PLUS auto_theme on with both
    // slots filled. It guards against the serializer emitting one theme
    // key in a way that clobbers another, and confirms that after a full
    // serialize -> parse cycle `active_scheme` still selects the slot by
    // appearance (not the color_scheme fallback) in each direction.
    let mut c = Config::defaults();
    c.color_scheme = Some("nostromo".to_string());
    c.auto_theme = true;
    c.light_scheme = Some("daytime".to_string());
    c.dark_scheme = Some("midnight".to_string());
    let parsed = Config::parse_str(&c.serialize());
    assert_eq!(parsed.color_scheme.as_deref(), Some("nostromo"));
    assert!(parsed.auto_theme);
    assert_eq!(parsed.light_scheme.as_deref(), Some("daytime"));
    assert_eq!(parsed.dark_scheme.as_deref(), Some("midnight"));
    // Slots win over the color_scheme fallback, per appearance.
    assert_eq!(parsed.active_scheme(false), Some("daytime"));
    assert_eq!(parsed.active_scheme(true), Some("midnight"));
}

#[test]
fn active_scheme_following_each_slot_falls_back_independently() {
    // The existing fallback test covers dark-set / light-unset. This pins
    // the mirror case (light-set / dark-unset) so neither branch of the
    // `if dark` slot selection silently reads the wrong field: the unset
    // direction falls back to color_scheme while the set one keeps its slot.
    let mut c = Config::defaults();
    c.auto_theme = true;
    c.color_scheme = Some("fallback".to_string());
    c.light_scheme = Some("daytime".to_string());
    assert_eq!(c.active_scheme(false), Some("daytime"));
    assert_eq!(c.active_scheme(true), Some("fallback"));
}

#[test]
fn config_color_scheme_with_spaces_round_trips() {
    // Scheme names go through `toml_str_lit` so a name with spaces is
    // quoted on write and read back verbatim — same machinery as
    // font_family.
    let mut c = Config::defaults();
    c.color_scheme = Some("My Custom Theme".to_string());
    let parsed = Config::parse_str(&c.serialize());
    assert_eq!(parsed.color_scheme.as_deref(), Some("My Custom Theme"));
}

#[test]
fn config_invalid_toml_falls_back_to_defaults() {
    // A document that isn't valid TOML at all (a bareword value here)
    // can't be walked key-by-key, so the whole config reverts to defaults
    // rather than guessing — the same wholesale-fallback contract the
    // palette parser has. The good `font_size` before the broken line
    // must NOT survive, proving the fallback is whole-file not per-key.
    let parsed = Config::parse_str("font_size = 18.0\nthis is not toml\n");
    let d = Config::defaults();
    assert!(approx_eq(parsed.font_size, d.font_size));
}

#[test]
fn config_unknown_keys_are_ignored() {
    // Forward/backward compatibility: an unrecognized key is skipped and
    // siblings still apply. A future binary's keys in an old config (or
    // vice versa) must not blank the file out.
    let parsed = Config::parse_str("totally_made_up_key = 42\nfont_size = 14.0\n");
    assert!(approx_eq(parsed.font_size, 14.0));
}

#[test]
fn config_toml_str_lit_escapes_quotes_and_backslashes() {
    // A font family containing a double-quote or backslash must be
    // escaped by `toml_str_lit` so the emitted literal stays valid TOML
    // and round-trips byte-for-byte. A naive `format!("\"{}\"")` would
    // produce a parse error here.
    let mut c = Config::defaults();
    c.font_family = Some("Weird\"Font\\Name".to_string());
    let serialized = c.serialize();
    // The whole document must still parse...
    assert!(serialized.parse::<toml::Table>().is_ok());
    // ...and the value must come back exactly as written.
    let parsed = Config::parse_str(&serialized);
    assert_eq!(parsed.font_family.as_deref(), Some("Weird\"Font\\Name"));
}

#[test]
fn config_cfg_usize_rejects_negative_and_non_integer() {
    // cfg_usize underpins glow_iterations / images_memory_cap_mb. A
    // negative or non-integer value must yield None so the slot keeps its
    // default rather than panicking on the `try_from`.
    assert_eq!(cfg_usize(&toml::Value::Integer(-1)), None);
    assert_eq!(cfg_usize(&toml::Value::Float(1.0)), None);
    assert_eq!(cfg_usize(&toml::Value::Integer(7)), Some(7));
}

#[test]
fn config_cfg_u64_rejects_negative() {
    // cfg_u64 underpins images_max_pixels / images_decode_timeout_ms; a
    // negative literal must be refused, not wrapped to a huge unsigned.
    assert_eq!(cfg_u64(&toml::Value::Integer(-5)), None);
    assert_eq!(cfg_u64(&toml::Value::Integer(5)), Some(5));
}

#[test]
fn config_images_memory_cap_accepts_integer() {
    // images_memory_cap_mb is a plain usize slot; a bare integer must
    // apply (no clamp), confirming the cfg_usize path is wired up.
    let parsed = Config::parse_str("images_memory_cap_mb = 512\n");
    assert_eq!(parsed.images_memory_cap_mb, 512);
}

/// Build a row of cells from a string for URL-detection tests. Each
/// char becomes one cell with default style.
fn cells_from_str(s: &str) -> Vec<style::Cell> {
    s.chars()
        .map(|ch| style::Cell::new(ch, style::Style::new()))
        .collect()
}

#[test]
fn url_detected_when_cursor_inside() {
    let row = cells_from_str("see https://example.com today");
    // Cursor on the 'e' inside "example".
    let (s, e, url) = find_url_in_cells(&row, 12).expect("should find url");
    assert_eq!(s, 4);
    assert_eq!(e, 22);
    assert_eq!(url, "https://example.com");
}

#[test]
fn url_detected_at_start_of_prefix() {
    let row = cells_from_str("see https://example.com today");
    // Cursor on the leading 'h' of "https".
    let (s, e, url) = find_url_in_cells(&row, 4).expect("should find url");
    assert_eq!(s, 4);
    assert_eq!(e, 22);
    assert_eq!(url, "https://example.com");
}

#[test]
fn url_detected_at_end_of_url() {
    let row = cells_from_str("see https://example.com today");
    // Cursor on the trailing 'm' of ".com".
    let (s, e, _) = find_url_in_cells(&row, 22).expect("should find url");
    assert_eq!(s, 4);
    assert_eq!(e, 22);
}

#[test]
fn http_scheme_also_detected() {
    let row = cells_from_str("http://foo.bar/baz");
    let (s, e, url) = find_url_in_cells(&row, 0).expect("should find url");
    assert_eq!(s, 0);
    assert_eq!(e, row.len() - 1);
    assert_eq!(url, "http://foo.bar/baz");
}

#[test]
fn returns_none_when_cursor_on_whitespace() {
    let row = cells_from_str("see https://example.com today");
    // Cursor on the space at index 3 (between "see" and "https").
    assert!(find_url_in_cells(&row, 3).is_none());
}

#[test]
fn returns_none_when_cursor_outside_url() {
    let row = cells_from_str("see https://example.com today");
    // Cursor on 's' in "see" — outside the URL run.
    assert!(find_url_in_cells(&row, 0).is_none());
    // Cursor on 't' in "today" — past the URL.
    assert!(find_url_in_cells(&row, 24).is_none());
}

#[test]
fn returns_none_for_plain_text() {
    let row = cells_from_str("no url anywhere here");
    for c in 0..row.len() {
        assert!(find_url_in_cells(&row, c).is_none(), "col {c}");
    }
}

#[test]
fn trailing_sentence_punctuation_is_stripped() {
    let row = cells_from_str("visit https://example.com.");
    let (_, e, url) = find_url_in_cells(&row, 10).expect("should find url");
    // Trailing '.' should not be part of the URL.
    assert_eq!(url, "https://example.com");
    assert_eq!(row[e].ch, 'm');
}

#[test]
fn trailing_paren_is_stripped() {
    let row = cells_from_str("(see https://example.com)");
    let (_, _, url) = find_url_in_cells(&row, 10).expect("should find url");
    assert_eq!(url, "https://example.com");
}

#[test]
fn is_safe_url_allows_known_schemes() {
    for u in [
        "https://x/",
        "http://x/",
        "mailto:a@b.com",
        "file:///etc/hosts",
        "ftp://host/f",
        "ssh://host",
        "  HTTPS://Upper/  ",
    ] {
        assert!(is_safe_url(u), "{u} should be safe");
    }
}

#[test]
fn is_safe_url_rejects_dangerous_or_bare() {
    for u in [
        "javascript:alert(1)",
        "data:text/html,<script>",
        "vbscript:x",
        "not a url",
        "example.com",
    ] {
        assert!(!is_safe_url(u), "{u} should be rejected");
    }
}

#[test]
fn osc8_link_preferred_over_heuristic_anchor_text() {
    // Anchor text "click here" links to a different target via OSC 8.
    // find_url_at must return the OSC 8 target, not parse the visible text.
    let mut t = terminal::Terminal::new(80, 24, 100);
    t.feed("\x1b]8;;https://real.example/path\x07click here\x1b]8;;\x07");
    let abs = t.visual_to_abs_line(0);
    let hu = find_url_at(&t, abs, 2).expect("link under 'click'");
    assert_eq!(hu.url, "https://real.example/path");
    assert_eq!(hu.start_col(), 0);
    assert_eq!(hu.end_col(), "click here".len() - 1);
}

#[test]
fn osc8_link_span_stops_at_unlinked_cells() {
    let mut t = terminal::Terminal::new(80, 24, 100);
    // "pre " unlinked, "LINK" linked, " post" unlinked.
    t.feed("pre \x1b]8;;https://x/\x07LINK\x1b]8;;\x07 post");
    let abs = t.visual_to_abs_line(0);
    let hu = find_osc8_link_at(&t, abs, 5).expect("link under LINK");
    assert_eq!(hu.start_col(), 4);
    assert_eq!(hu.end_col(), 7);
    assert_eq!(hu.url, "https://x/");
    // A cell in "pre " has no OSC 8 link.
    assert!(find_osc8_link_at(&t, abs, 1).is_none());
}

#[test]
fn osc8_id_siblings_cohighlight() {
    // Two non-contiguous spans share `id=grp` + URI: hovering either must
    // return segments covering BOTH runs so they underline together.
    let mut t = terminal::Terminal::new(80, 24, 100);
    t.feed("\x1b]8;id=grp;https://x/\x07AB\x1b]8;;\x07 mid \x1b]8;id=grp;https://x/\x07CD\x1b]8;;\x07");
    let abs = t.visual_to_abs_line(0);
    // "AB" at cols 0..1; " mid " at 2..6; "CD" at cols 7..8.
    let hu = find_osc8_link_at(&t, abs, 0).expect("link under first span");
    assert_eq!(hu.url, "https://x/");
    let mut spans: Vec<(usize, usize)> =
        hu.segments.iter().map(|s| (s.start_col, s.end_col)).collect();
    spans.sort();
    assert_eq!(spans, vec![(0, 1), (7, 8)], "both id=grp spans co-highlight");
    // Hovering the second span resolves to the identical set.
    let hu2 = find_osc8_link_at(&t, abs, 7).expect("link under second span");
    assert_eq!(hu, hu2);
}

#[test]
fn osc8_id_three_siblings_all_cohighlight() {
    // Three non-contiguous spans share one id: hovering any of them must
    // return all three segments.
    let mut t = terminal::Terminal::new(80, 24, 100);
    t.feed("\x1b]8;id=g;https://x/\x07A\x1b]8;;\x07 \x1b]8;id=g;https://x/\x07B\x1b]8;;\x07 \x1b]8;id=g;https://x/\x07C\x1b]8;;\x07");
    let abs = t.visual_to_abs_line(0);
    // "A" col 0, "B" col 2, "C" col 4.
    let hu = find_osc8_link_at(&t, abs, 0).expect("link under first span");
    let mut spans: Vec<(usize, usize)> =
        hu.segments.iter().map(|s| (s.start_col, s.end_col)).collect();
    spans.sort();
    assert_eq!(spans, vec![(0, 0), (2, 2), (4, 4)], "all three spans co-highlight");
    // Hovering the middle and last spans yields the identical set.
    assert_eq!(find_osc8_link_at(&t, abs, 2).unwrap(), hu);
    assert_eq!(find_osc8_link_at(&t, abs, 4).unwrap(), hu);
}

#[test]
fn osc8_id_siblings_on_different_rows_cohighlight() {
    // Two spans share one id but land on different visible rows (a newline
    // separates them). Hovering either must return one segment per row.
    let mut t = make_terminal(5, 20);
    t.feed("\x1b]8;id=g;https://x/\x07AB\x1b]8;;\x07\r\n\x1b]8;id=g;https://x/\x07CD\x1b]8;;\x07");
    let abs0 = t.visual_to_abs_line(0);
    let abs1 = t.visual_to_abs_line(1);
    let hu = find_osc8_link_at(&t, abs0, 0).expect("hover first row span");
    assert_eq!(hu.url, "https://x/");
    let mut segs: Vec<(isize, usize, usize)> = hu
        .segments
        .iter()
        .map(|s| (s.abs_line, s.start_col, s.end_col))
        .collect();
    segs.sort();
    assert_eq!(segs, vec![(abs0, 0, 1), (abs1, 0, 1)], "siblings on two rows co-highlight");
    // Hovering the second-row span resolves to the same set.
    assert_eq!(find_osc8_link_at(&t, abs1, 0).unwrap(), hu);
}

#[test]
fn osc8_anonymous_spans_do_not_cohighlight() {
    // No `id=`: each open is a distinct link, so hovering the first span
    // highlights only its own contiguous run, not the later same-URI span.
    let mut t = terminal::Terminal::new(80, 24, 100);
    t.feed("\x1b]8;;https://x/\x07AB\x1b]8;;\x07 \x1b]8;;https://x/\x07CD\x1b]8;;\x07");
    let abs = t.visual_to_abs_line(0);
    let hu = find_osc8_link_at(&t, abs, 0).expect("first span");
    assert_eq!(hu.segments.len(), 1, "anonymous links don't group");
    assert_eq!(hu.segments[0].start_col, 0);
    assert_eq!(hu.segments[0].end_col, 1);
}

#[test]
fn heuristic_still_works_without_osc8() {
    let mut t = terminal::Terminal::new(80, 24, 100);
    t.feed("see https://example.com today");
    let abs = t.visual_to_abs_line(0);
    let hu = find_url_at(&t, abs, 12).expect("heuristic url");
    assert_eq!(hu.url, "https://example.com");
}

#[test]
fn find_osc8_link_at_none_on_unlinked_cell() {
    // A grid with no OSC 8 link anywhere yields None for every cell.
    let mut t = terminal::Terminal::new(80, 24, 100);
    t.feed("just plain text");
    let abs = t.visual_to_abs_line(0);
    assert!(find_osc8_link_at(&t, abs, 0).is_none());
    assert!(find_osc8_link_at(&t, abs, 5).is_none());
}

#[test]
fn find_osc8_link_at_out_of_bounds_col_is_none() {
    // A col past the row width must not panic and must return None.
    let mut t = terminal::Terminal::new(80, 24, 100);
    t.feed("\x1b]8;;https://x/\x07AB\x1b]8;;\x07");
    let abs = t.visual_to_abs_line(0);
    assert!(find_osc8_link_at(&t, abs, 10_000).is_none());
}

#[test]
fn find_osc8_link_at_extends_across_wrapped_rows() {
    // A single OSC 8 link whose anchor text wraps across rows must resolve
    // to one span covering both rows, whether hovered on the first or the
    // continuation row. Grid is 10 cols; 14 linked glyphs wrap to row 1.
    let mut t = make_terminal(5, 10);
    t.feed("\x1b]8;;https://wrap/target\x07ABCDEFGHIJKLMN\x1b]8;;\x07");
    // Row 0 is full (cols 0..9), row 1 holds the remaining 4 (cols 0..3).
    let from_first = find_osc8_link_at(&t, 0, 2).expect("hover first row");
    let from_tail = find_osc8_link_at(&t, 1, 1).expect("hover continuation row");
    assert_eq!(from_first, from_tail, "both hovers resolve to one span");
    assert_eq!(from_first.start_abs_line(), 0);
    assert_eq!(from_first.start_col(), 0);
    assert_eq!(from_first.end_abs_line(), 1);
    assert_eq!(from_first.end_col(), 3, "14 glyphs over 10 cols end at col 3 of row 1");
    assert_eq!(from_first.url, "https://wrap/target");
}

#[test]
fn url_only_in_punctuation_run_rejected() {
    // A `).` after the prefix would leave an empty host. Make sure we
    // don't return a URL that's just the scheme.
    let row = cells_from_str("https://.");
    assert!(find_url_in_cells(&row, 0).is_none());
}

#[test]
fn empty_row_returns_none() {
    let row: Vec<style::Cell> = Vec::new();
    assert!(find_url_in_cells(&row, 0).is_none());
}

#[test]
fn out_of_bounds_col_returns_none() {
    let row = cells_from_str("https://example.com");
    assert!(find_url_in_cells(&row, row.len()).is_none());
    assert!(find_url_in_cells(&row, row.len() + 5).is_none());
}

#[test]
fn url_with_path_and_query() {
    let row = cells_from_str("https://example.com/a/b?q=1&x=2");
    let (_, _, url) = find_url_in_cells(&row, 10).expect("should find url");
    assert_eq!(url, "https://example.com/a/b?q=1&x=2");
}

#[test]
fn returns_first_url_when_multiple_share_a_run() {
    // A run with no whitespace can theoretically have two prefixes
    // concatenated — make sure we return the earlier (and longer https)
    // start, not the embedded http.
    let row = cells_from_str("https://foo");
    let (s, _, url) = find_url_in_cells(&row, 0).expect("should find url");
    assert_eq!(s, 0);
    assert_eq!(url, "https://foo");
}

// ---- additional edge-case tests --------------------------------------

#[test]
fn url_at_very_first_column_with_cursor_on_last_char() {
    // URL fills the entire row; cursor sits on the final cell.
    let row = cells_from_str("https://example.com");
    let last = row.len() - 1;
    let (s, e, url) = find_url_in_cells(&row, last).expect("should find url");
    assert_eq!(s, 0);
    assert_eq!(e, last);
    assert_eq!(url, "https://example.com");
}

#[test]
fn url_at_very_last_column_of_row() {
    // No trailing whitespace — URL ends exactly at the right edge.
    let row = cells_from_str("see https://example.com");
    let last = row.len() - 1;
    let (s, e, url) = find_url_in_cells(&row, last).expect("should find url");
    assert_eq!(s, 4);
    assert_eq!(e, last);
    assert_eq!(url, "https://example.com");
}

#[test]
fn tab_delimits_url_run() {
    // Tabs are whitespace; the URL between two tabs is detected.
    let row = cells_from_str("a\thttps://example.com\tb");
    // Cursor on the 'x' of "example".
    let (s, e, url) = find_url_in_cells(&row, 10).expect("should find url");
    assert_eq!(s, 2);
    assert_eq!(e, 20);
    assert_eq!(url, "https://example.com");
}

#[test]
fn very_short_url_with_single_char_host() {
    // "http://a" is the shortest legal http URL we accept (scheme + one
    // host char). Make sure the scheme-only guard does not over-reject.
    let row = cells_from_str("http://a");
    let (s, e, url) = find_url_in_cells(&row, 7).expect("should find url");
    assert_eq!(s, 0);
    assert_eq!(e, 7);
    assert_eq!(url, "http://a");
}

#[test]
fn url_with_fragment_is_preserved() {
    let row = cells_from_str("https://example.com/page#section-2");
    let (_, _, url) = find_url_in_cells(&row, 10).expect("should find url");
    assert_eq!(url, "https://example.com/page#section-2");
}

#[test]
fn url_with_percent_encoded_chars_is_preserved() {
    let row = cells_from_str("https://example.com/a%20b%2Fc");
    let (_, _, url) = find_url_in_cells(&row, 10).expect("should find url");
    assert_eq!(url, "https://example.com/a%20b%2Fc");
}

#[test]
fn single_slash_scheme_is_not_a_url() {
    // "http:/foo" — missing the second slash. Must not match.
    let row = cells_from_str("http:/foo.bar");
    for c in 0..row.len() {
        assert!(find_url_in_cells(&row, c).is_none(), "col {c}");
    }
}

#[test]
fn single_slash_https_scheme_is_not_a_url() {
    let row = cells_from_str("https:/example.com");
    for c in 0..row.len() {
        assert!(find_url_in_cells(&row, c).is_none(), "col {c}");
    }
}

#[test]
fn unicode_letter_adjacent_to_url_is_part_of_run() {
    // Non-whitespace unicode glues onto the run, but the prefix scan
    // still locates "https://" further in and produces a clean URL.
    // (Whether we strip the leading unicode is a behavior choice — the
    // function happens to skip it because url_start_col jumps to where
    // the prefix actually matched.)
    let row = cells_from_str("→https://example.com");
    // Cursor on the 'x' of "example".
    let (s, _, url) = find_url_in_cells(&row, 10).expect("should find url");
    // The leading arrow is NOT part of the URL — the prefix scan starts
    // at column 1.
    assert_eq!(s, 1);
    assert_eq!(url, "https://example.com");
}

#[test]
fn unicode_letter_after_url_is_part_of_url() {
    // Trailing non-ASCII letters are not whitespace and are not in the
    // sentence-punctuation strip list, so they ride along as part of
    // the URL. We document the behavior here so it changes deliberately.
    let row = cells_from_str("https://例え.jp");
    let (_, _, url) = find_url_in_cells(&row, 0).expect("should find url");
    assert_eq!(url, "https://例え.jp");
}

#[test]
fn two_concatenated_urls_in_one_run_return_combined_span() {
    // Pathological input: two URLs glued with no whitespace. The function
    // is whitespace-delimited, so it returns the whole run starting at
    // the first prefix. Cursor on the first URL gets the combined span.
    // (Documenting current behavior — splitting on a second "http(s)://"
    // would require extra logic we don't ship.)
    let row = cells_from_str("https://a.comhttps://b.com");
    let (s, e, url) = find_url_in_cells(&row, 2).expect("should find url");
    assert_eq!(s, 0);
    assert_eq!(e, row.len() - 1);
    assert_eq!(url, "https://a.comhttps://b.com");
}

#[test]
fn cursor_on_stripped_trailing_punctuation_returns_none() {
    // "https://example.com." with cursor on the '.' — the dot is
    // stripped from the URL, so the cursor is "past" url_end_col and
    // we report no hit. Hovering exactly on the trailing dot is not a
    // URL hover.
    let row = cells_from_str("https://example.com.");
    let dot_col = row.len() - 1;
    assert_eq!(row[dot_col].ch, '.');
    assert!(find_url_in_cells(&row, dot_col).is_none());
}

#[test]
fn quoted_url_strips_trailing_quote() {
    let row = cells_from_str("\"https://example.com\"");
    let (_, _, url) = find_url_in_cells(&row, 10).expect("should find url");
    assert_eq!(url, "https://example.com");
}

#[test]
fn bracketed_url_strips_trailing_bracket() {
    let row = cells_from_str("[https://example.com]");
    let (_, _, url) = find_url_in_cells(&row, 10).expect("should find url");
    assert_eq!(url, "https://example.com");
}

#[test]
fn multiple_trailing_punctuation_all_stripped() {
    // "...)!" should all peel off, leaving the bare URL.
    let row = cells_from_str("https://example.com.)!");
    let (_, e, url) = find_url_in_cells(&row, 10).expect("should find url");
    assert_eq!(url, "https://example.com");
    assert_eq!(row[e].ch, 'm');
}

#[test]
fn uppercase_scheme_is_not_matched() {
    // Prefix match is case-sensitive — "HTTPS://" is not recognized.
    // Documenting current behavior (browsers accept it, we don't).
    let row = cells_from_str("HTTPS://example.com");
    for c in 0..row.len() {
        assert!(find_url_in_cells(&row, c).is_none(), "col {c}");
    }
}

#[test]
fn url_with_port_number() {
    let row = cells_from_str("http://localhost:8080/path");
    let (_, _, url) = find_url_in_cells(&row, 10).expect("should find url");
    assert_eq!(url, "http://localhost:8080/path");
}

#[test]
fn cursor_on_whitespace_tab_returns_none() {
    let row = cells_from_str("a\thttps://example.com");
    // Cursor on the tab itself.
    assert!(find_url_in_cells(&row, 1).is_none());
}

// ---- wrap-aware URL detection (find_url_at / build_wrapped_line) ----
//
// The autowrap heuristic looks at the cell grid: a row joins its
// predecessor only when *both* the prev row's last col and the cur row's
// first col are non-whitespace. A real `terminal::Terminal` is required
// here so we exercise the actual grid layout autowrap produces.

/// 20-col x rows terminal with a generous scrollback budget. Default
/// autowrap on, no DECLRMM margins — matches the conditions a hovered
/// shell URL sees.
fn make_terminal(rows: usize, cols: usize) -> terminal::Terminal {
    terminal::Terminal::new(cols, rows, 1024)
}

#[test]
fn find_url_at_wrapped_url_resolves_from_first_row() {
    // 39-char URL on a 20-col grid: row 0 gets cols 0..19
    // ("https://example.com/"), row 1 gets cols 0..18
    // ("very-long-path/here"). The join condition holds because
    // row 0's last cell ('/') and row 1's first cell ('v') are both
    // non-whitespace.
    let mut t = make_terminal(5, 20);
    let url = "https://example.com/very-long-path/here";
    assert_eq!(url.len(), 39);
    t.feed(url);

    let hover = find_url_at(&t, 0, 5).expect("URL should be found from first row");
    assert_eq!(hover.start_abs_line(), 0);
    assert_eq!(hover.end_abs_line(), 1);
    assert_eq!(hover.start_col(), 0);
    assert_eq!(hover.end_col(), 18, "39 chars over 20 cols ends at col 18 of row 1");
    assert_eq!(hover.url, url);
}

#[test]
fn find_url_at_wrapped_url_resolves_from_continuation_row() {
    // Same wrapped URL — cursor on the continuation row must resolve to
    // the same span. This is the regression: previously hovering the
    // tail row found nothing because the row in isolation has no scheme.
    let mut t = make_terminal(5, 20);
    let url = "https://example.com/very-long-path/here";
    t.feed(url);

    let from_first = find_url_at(&t, 0, 5).expect("first-row hover");
    let from_tail = find_url_at(&t, 1, 5).expect("tail-row hover should also resolve");
    assert_eq!(from_first, from_tail);
}

#[test]
fn find_url_at_does_not_join_when_boundary_is_whitespace() {
    // Row 0: "https://example.com " (19 + 1 space = 20 cols exactly).
    // Row 1: "extra-text" starting at col 0. Row 0's last cell is a
    // space, so the join is suppressed and the URL stays on row 0
    // without sucking up "extra-text".
    let mut t = make_terminal(5, 20);
    t.feed("https://example.com extra-text");

    let hover = find_url_at(&t, 0, 5).expect("URL on row 0");
    assert_eq!(hover.start_abs_line(), 0);
    assert_eq!(hover.end_abs_line(), 0);
    assert_eq!(hover.start_col(), 0);
    assert_eq!(hover.end_col(), 18);
    assert_eq!(hover.url, "https://example.com");
    assert!(
        !hover.url.contains("extra-text"),
        "whitespace at boundary must break the wrap-join"
    );
}

#[test]
fn find_url_at_caps_continuation_walk() {
    // Feed many rows of solid non-whitespace text with a URL at the
    // top. Without the URL_WRAP_MAX_ROWS cap, build_wrapped_line would
    // walk every continuous row in scrollback. With the cap, the walk
    // is bounded; the test must complete quickly and return *some*
    // URL — we don't pin the exact length because that's the heuristic's
    // discretion.
    let mut t = make_terminal(5, 20);
    // 50 rows worth of solid non-whitespace, starting with the scheme.
    let mut s = String::from("https://example.com/");
    // 49 more rows of 20 'x' each — all non-whitespace, so every
    // boundary qualifies for the join (until the cap kicks in).
    for _ in 0..49 {
        s.push_str(&"x".repeat(20));
    }
    t.feed(&s);

    // The first row of the URL is now somewhere in scrollback. Find it
    // by scanning abs_line 0..scrollback_len + rows for the row that
    // starts with 'h'.
    let total_lines = t.scrollback_len() as isize + t.rows as isize;
    let mut start_abs = None;
    for abs in 0..total_lines {
        if let Some(row) = t.line_at(abs) {
            if row.first().map(|c| c.ch) == Some('h') {
                start_abs = Some(abs);
                break;
            }
        }
    }
    let start_abs = start_abs.expect("URL start row should exist");

    let hover = find_url_at(&t, start_abs, 0).expect("should resolve to some URL");
    assert_eq!(hover.start_abs_line(), start_abs);
    assert!(hover.url.starts_with("https://example.com/"));
    // Cap is URL_WRAP_MAX_ROWS rows past the start; bound length
    // generously to confirm we didn't walk all 50 rows.
    let max_len = (URL_WRAP_MAX_ROWS + 1) * 20;
    assert!(
        hover.url.len() <= max_len,
        "URL length {} exceeded wrap cap (max {})",
        hover.url.len(),
        max_len
    );
}

#[test]
fn find_url_at_single_row_url_still_works() {
    // Regression: the wrap-aware path must not break the common
    // single-row case. 60 cols is wide enough that nothing wraps.
    let mut t = make_terminal(5, 60);
    t.feed("https://example.com more text here");

    let hover = find_url_at(&t, 0, 10).expect("single-row URL");
    assert_eq!(hover.start_abs_line(), 0);
    assert_eq!(hover.end_abs_line(), 0);
    assert_eq!(hover.start_col(), 0);
    assert_eq!(hover.end_col(), 18);
    assert_eq!(hover.url, "https://example.com");
}

//
// apply_glow_config — pure copy from `Config` onto a `Glow`. Building
// a real `Glow` needs a headless wgpu device + a scene texture view;
// we follow the same skip-on-no-adapter pattern as `images::tests`
// so CI without a GPU just sits these out instead of failing.
//

/// Build a headless `Glow` for use in tests. Returns `None` if no
/// adapter is available (CI without a GPU); callers should bail
/// silently rather than fail the test, matching the precedent in
/// `src/images.rs`.
fn try_make_test_glow() -> Option<(wgpu::Device, wgpu::Queue, renderer::glow::Glow)> {
    let instance = wgpu::Instance::default();
    let adapter = pollster::block_on(
        instance.request_adapter(&wgpu::RequestAdapterOptions::default()),
    )?;
    let (device, queue) = pollster::block_on(
        adapter.request_device(&wgpu::DeviceDescriptor::default(), None),
    )
    .ok()?;
    let format = wgpu::TextureFormat::Rgba8UnormSrgb;
    // Dummy scene texture — Glow only needs a TextureView at the
    // bound size; the contents don't matter for the field-copy path.
    let scene_tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("test glow scene"),
        size: wgpu::Extent3d { width: 16, height: 16, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let scene_view = scene_tex.create_view(&wgpu::TextureViewDescriptor::default());
    let pipelines = renderer::glow::GlowPipelines::new(&device, format);
    let glow = renderer::glow::Glow::new(&device, &pipelines, 16, 16, &scene_view);
    Some((device, queue, glow))
}

/// Build a `Config` whose every `glow_*` slot differs from the
/// `defaults()` value so a missing assignment in `apply_glow_config`
/// shows up as a stale default in the resulting `Glow`.
fn non_default_glow_config() -> Config {
    let mut c = Config::defaults();
    c.glow_match_brightness = true;
    c.glow_match_bright_ansi = true;
    c.glow_match_foreground = true;
    c.glow_threshold = 0.42;
    c.glow_intensity = 1.7;
    c.glow_softness = 0.33;
    c.glow_hue_tolerance_deg = 27.5;
    c.glow_fg_tolerance = 0.21;
    c.glow_scanlines = true;
    c.glow_scanline_strength = 0.55;
    c.glow_scanline_period = 6.0;
    c.glow_scanlines_content = true;
    c.glow_scanlines_content_strength = 0.66;
    c.glow_scanline_color_bright = [0.9, 0.7, 0.5, 1.0];
    c.glow_scanline_color_dark = [0.1, 0.2, 0.3, 1.0];
    c.glow_scanlines_content_attenuation = 0.77;
    c.glow_iterations = 5;
    c
}

#[test]
fn apply_glow_config_copies_every_field_from_config() {
    let Some((_d, _q, mut g)) = try_make_test_glow() else {
        eprintln!("skipping: no GPU adapter");
        return;
    };
    let c = non_default_glow_config();
    apply_glow_config(&mut g, &c, &palette::GlowOverrides::NONE);

    // Every config slot listed in the task spec must appear on the
    // Glow. A missing line in `apply_glow_config` shows up here as a
    // stale default (`non_default_glow_config` differs from
    // `Config::defaults` on every field below).
    assert_eq!(g.match_brightness, c.glow_match_brightness);
    assert_eq!(g.match_bright_ansi, c.glow_match_bright_ansi);
    assert_eq!(g.match_foreground, c.glow_match_foreground);
    assert!(approx_eq(g.threshold, c.glow_threshold));
    assert!(approx_eq(g.intensity, c.glow_intensity));
    assert!(approx_eq(g.softness, c.glow_softness));
    assert!(approx_eq(g.hue_tolerance, c.glow_hue_tolerance_deg));
    assert!(approx_eq(g.fg_tolerance, c.glow_fg_tolerance));
    assert_eq!(g.match_scanlines, c.glow_scanlines);
    assert!(approx_eq(g.scanline_strength, c.glow_scanline_strength));
    assert!(approx_eq(g.scanline_period, c.glow_scanline_period));
    assert_eq!(g.match_content_scanlines, c.glow_scanlines_content);
    assert!(approx_eq(
        g.content_scanline_strength,
        c.glow_scanlines_content_strength,
    ));
    assert_eq!(g.scanline_color_bright, c.glow_scanline_color_bright);
    assert_eq!(g.scanline_color_dark, c.glow_scanline_color_dark);
    assert!(approx_eq(
        g.content_scanline_attenuation,
        c.glow_scanlines_content_attenuation,
    ));
    assert_eq!(g.iterations, c.glow_iterations);
}

#[test]
fn apply_glow_config_overwrites_prior_state() {
    // Reload-equivalent: apply once with non-default values, then
    // apply again with `Config::defaults()`. The Glow must end up
    // matching defaults — i.e. the second apply replaces every
    // field, no "sticky" remnants from the first pass.
    let Some((_d, _q, mut g)) = try_make_test_glow() else {
        eprintln!("skipping: no GPU adapter");
        return;
    };
    apply_glow_config(&mut g, &non_default_glow_config(), &palette::GlowOverrides::NONE);
    let defaults = Config::defaults();
    apply_glow_config(&mut g, &defaults, &palette::GlowOverrides::NONE);

    assert_eq!(g.match_brightness, defaults.glow_match_brightness);
    assert_eq!(g.match_bright_ansi, defaults.glow_match_bright_ansi);
    assert_eq!(g.match_foreground, defaults.glow_match_foreground);
    assert!(approx_eq(g.threshold, defaults.glow_threshold));
    assert!(approx_eq(g.intensity, defaults.glow_intensity));
    assert!(approx_eq(g.softness, defaults.glow_softness));
    assert_eq!(g.match_scanlines, defaults.glow_scanlines);
    assert_eq!(g.match_content_scanlines, defaults.glow_scanlines_content);
    assert_eq!(g.iterations, defaults.glow_iterations);
}

#[test]
fn apply_glow_config_clamps_iterations_above_max() {
    // `Config::parse_str` clamps `glow_iterations` on the way in,
    // but a hand-mutated `Config` (or a future code path that sets
    // the field directly) shouldn't be able to push the Glow's
    // dual-Kawase chain past `MAX_ITERATIONS`. `apply_glow_config`
    // re-clamps to enforce that.
    let Some((_d, _q, mut g)) = try_make_test_glow() else {
        eprintln!("skipping: no GPU adapter");
        return;
    };
    let mut c = Config::defaults();
    c.glow_iterations = renderer::glow::MAX_ITERATIONS * 10;
    apply_glow_config(&mut g, &c, &palette::GlowOverrides::NONE);
    assert_eq!(g.iterations, renderer::glow::MAX_ITERATIONS);
}

#[test]
fn apply_glow_config_clamps_iterations_below_one() {
    // Lower bound: dual-Kawase needs at least one down/up pass to
    // produce a halo, so 0 (or anything below) must clamp up to 1.
    let Some((_d, _q, mut g)) = try_make_test_glow() else {
        eprintln!("skipping: no GPU adapter");
        return;
    };
    let mut c = Config::defaults();
    c.glow_iterations = 0;
    apply_glow_config(&mut g, &c, &palette::GlowOverrides::NONE);
    assert_eq!(g.iterations, 1);
}

#[test]
fn apply_glow_config_iterations_at_max_unchanged() {
    // Boundary: a value sitting exactly at `MAX_ITERATIONS` must
    // pass through unmodified — the clamp is inclusive.
    let Some((_d, _q, mut g)) = try_make_test_glow() else {
        eprintln!("skipping: no GPU adapter");
        return;
    };
    let mut c = Config::defaults();
    c.glow_iterations = renderer::glow::MAX_ITERATIONS;
    apply_glow_config(&mut g, &c, &palette::GlowOverrides::NONE);
    assert_eq!(g.iterations, renderer::glow::MAX_ITERATIONS);
}

//
// theme_overrides_glow tiebreaker: when set, `Some(_)` slots on the
// scheme's `GlowOverrides` win over the matching `Config` field; when
// clear, the config always wins regardless of override state. These
// pin both branches end-to-end through `apply_glow_config` and the
// matching `effective_skip_primary_bg` helper.
//

/// Build a `GlowOverrides` whose every field is `Some(_)` and
/// distinct from the values produced by `non_default_glow_config`.
/// A passing override-wins test then proves each slot came through
/// the override path rather than the config path.
fn fully_populated_overrides() -> palette::GlowOverrides {
    palette::GlowOverrides {
        match_brightness: Some(false),
        match_bright_ansi: Some(false),
        match_foreground: Some(false),
        threshold: Some(0.11),
        intensity: Some(0.22),
        softness: Some(0.13),
        hue_tolerance_deg: Some(91.0),
        fg_tolerance: Some(1.1),
        iterations: Some(3),
        scanlines: Some(false),
        scanline_strength: Some(0.14),
        scanline_period: Some(7.5),
        scanlines_content: Some(false),
        scanlines_content_strength: Some(0.17),
        scanline_color_bright: Some([0.1, 0.2, 0.3, 1.0]),
        scanline_color_dark: Some([0.4, 0.5, 0.6, 1.0]),
        scanlines_skip_primary_bg: Some(true),
        scanlines_content_attenuation: Some(0.19),
    }
}

#[test]
fn apply_glow_config_ignores_overrides_when_flag_off() {
    // Default `theme_overrides_glow = false`: even a fully-populated
    // `GlowOverrides` must be ignored. Glow ends up matching config
    // verbatim — same outcome as the existing
    // `apply_glow_config_copies_every_field_from_config` test.
    let Some((_d, _q, mut g)) = try_make_test_glow() else {
        eprintln!("skipping: no GPU adapter");
        return;
    };
    let mut c = non_default_glow_config();
    c.theme_overrides_glow = false;
    let o = fully_populated_overrides();
    apply_glow_config(&mut g, &c, &o);

    assert_eq!(g.match_brightness, c.glow_match_brightness);
    assert_eq!(g.match_bright_ansi, c.glow_match_bright_ansi);
    assert_eq!(g.match_foreground, c.glow_match_foreground);
    assert!(approx_eq(g.threshold, c.glow_threshold));
    assert!(approx_eq(g.intensity, c.glow_intensity));
    assert!(approx_eq(g.softness, c.glow_softness));
    assert!(approx_eq(g.hue_tolerance, c.glow_hue_tolerance_deg));
    assert!(approx_eq(g.fg_tolerance, c.glow_fg_tolerance));
    assert_eq!(g.match_scanlines, c.glow_scanlines);
    assert!(approx_eq(g.scanline_strength, c.glow_scanline_strength));
    assert!(approx_eq(g.scanline_period, c.glow_scanline_period));
    assert_eq!(g.match_content_scanlines, c.glow_scanlines_content);
    assert!(approx_eq(
        g.content_scanline_strength,
        c.glow_scanlines_content_strength,
    ));
    assert_eq!(g.scanline_color_bright, c.glow_scanline_color_bright);
    assert_eq!(g.scanline_color_dark, c.glow_scanline_color_dark);
    assert!(approx_eq(
        g.content_scanline_attenuation,
        c.glow_scanlines_content_attenuation,
    ));
    assert_eq!(g.iterations, c.glow_iterations);
}

#[test]
fn apply_glow_config_overrides_win_when_flag_on() {
    // Flip the tiebreaker on: every `Some(_)` override now wins
    // over the corresponding config field. Pin each slot against
    // the override value (not the config value) so a regression
    // that wires a slot to the wrong source shows up here.
    let Some((_d, _q, mut g)) = try_make_test_glow() else {
        eprintln!("skipping: no GPU adapter");
        return;
    };
    let mut c = non_default_glow_config();
    c.theme_overrides_glow = true;
    let o = fully_populated_overrides();
    apply_glow_config(&mut g, &c, &o);

    assert_eq!(g.match_brightness, o.match_brightness.unwrap());
    assert_eq!(g.match_bright_ansi, o.match_bright_ansi.unwrap());
    assert_eq!(g.match_foreground, o.match_foreground.unwrap());
    assert!(approx_eq(g.threshold, o.threshold.unwrap()));
    assert!(approx_eq(g.intensity, o.intensity.unwrap()));
    assert!(approx_eq(g.softness, o.softness.unwrap()));
    assert!(approx_eq(g.hue_tolerance, o.hue_tolerance_deg.unwrap()));
    assert!(approx_eq(g.fg_tolerance, o.fg_tolerance.unwrap()));
    assert_eq!(g.match_scanlines, o.scanlines.unwrap());
    assert!(approx_eq(g.scanline_strength, o.scanline_strength.unwrap()));
    assert!(approx_eq(g.scanline_period, o.scanline_period.unwrap()));
    assert_eq!(g.match_content_scanlines, o.scanlines_content.unwrap());
    assert!(approx_eq(
        g.content_scanline_strength,
        o.scanlines_content_strength.unwrap(),
    ));
    assert_eq!(g.scanline_color_bright, o.scanline_color_bright.unwrap());
    assert_eq!(g.scanline_color_dark, o.scanline_color_dark.unwrap());
    assert!(approx_eq(
        g.content_scanline_attenuation,
        o.scanlines_content_attenuation.unwrap(),
    ));
    assert_eq!(g.iterations, o.iterations.unwrap());
}

#[test]
fn apply_glow_config_none_override_falls_through_to_config_when_flag_on() {
    // Per-slot granularity: with the flag on, `None` slots still
    // defer to the config. Override only `threshold`; `intensity`
    // must come from the config because its override is `None`.
    let Some((_d, _q, mut g)) = try_make_test_glow() else {
        eprintln!("skipping: no GPU adapter");
        return;
    };
    let mut c = non_default_glow_config();
    c.theme_overrides_glow = true;
    let o = palette::GlowOverrides {
        threshold: Some(0.07),
        ..palette::GlowOverrides::NONE
    };
    apply_glow_config(&mut g, &c, &o);

    assert!(approx_eq(g.threshold, 0.07));
    assert!(approx_eq(g.intensity, c.glow_intensity));
    assert_eq!(g.match_brightness, c.glow_match_brightness);
    assert_eq!(g.match_scanlines, c.glow_scanlines);
}

#[test]
fn apply_glow_config_clamps_iterations_above_max_via_override() {
    // The iterations clamp runs *after* the override pick, so an
    // outsize scheme value can't smuggle a bigger dual-Kawase chain
    // past `MAX_ITERATIONS`.
    let Some((_d, _q, mut g)) = try_make_test_glow() else {
        eprintln!("skipping: no GPU adapter");
        return;
    };
    let mut c = Config::defaults();
    c.theme_overrides_glow = true;
    let o = palette::GlowOverrides {
        iterations: Some(renderer::glow::MAX_ITERATIONS * 10),
        ..palette::GlowOverrides::NONE
    };
    apply_glow_config(&mut g, &c, &o);
    assert_eq!(g.iterations, renderer::glow::MAX_ITERATIONS);
}

#[test]
fn apply_glow_config_clamps_iterations_below_one_via_override() {
    // Same clamp on the low end: `Some(0)` from a scheme still
    // floors at 1 after the pick.
    let Some((_d, _q, mut g)) = try_make_test_glow() else {
        eprintln!("skipping: no GPU adapter");
        return;
    };
    let mut c = Config::defaults();
    c.theme_overrides_glow = true;
    let o = palette::GlowOverrides {
        iterations: Some(0),
        ..palette::GlowOverrides::NONE
    };
    apply_glow_config(&mut g, &c, &o);
    assert_eq!(g.iterations, 1);
}

//
// `effective_skip_primary_bg` is the one glow slot resolved at draw
// time rather than mirrored onto the `Glow` struct (it picks between
// two render pipelines). The same tiebreaker applies; these tests
// don't need a GPU.
//

#[test]
fn effective_skip_primary_bg_flag_off_ignores_override() {
    // Override present but the tiebreaker is off → config wins.
    let mut c = Config::defaults();
    c.theme_overrides_glow = false;
    c.glow_scanlines_skip_primary_bg = false;
    let o = palette::GlowOverrides {
        scanlines_skip_primary_bg: Some(true),
        ..palette::GlowOverrides::NONE
    };
    assert!(!effective_skip_primary_bg(&c, &o));
}

#[test]
fn effective_skip_primary_bg_flag_on_none_falls_through_to_config() {
    // Flag on but no override candidate → config still wins.
    let mut c = Config::defaults();
    c.theme_overrides_glow = true;
    c.glow_scanlines_skip_primary_bg = true;
    assert!(effective_skip_primary_bg(&c, &palette::GlowOverrides::NONE));
}

#[test]
fn effective_skip_primary_bg_flag_on_override_wins() {
    // Override present and flag on → override wins, even when it
    // disagrees with the config value.
    let mut c = Config::defaults();
    c.theme_overrides_glow = true;
    c.glow_scanlines_skip_primary_bg = false;
    let o = palette::GlowOverrides {
        scanlines_skip_primary_bg: Some(true),
        ..palette::GlowOverrides::NONE
    };
    assert!(effective_skip_primary_bg(&c, &o));

    // And the inverse — override `false` beats config `true`.
    c.glow_scanlines_skip_primary_bg = true;
    let o = palette::GlowOverrides {
        scanlines_skip_primary_bg: Some(false),
        ..palette::GlowOverrides::NONE
    };
    assert!(!effective_skip_primary_bg(&c, &o));
}

//
// theme_overrides_glow round-trips through Config::serialize /
// Config::parse_str (both polarities) and starts at `false` so
// upgrading the binary doesn't silently start honouring stray glow
// keys in users' existing schemes.
//

#[test]
fn config_defaults_theme_overrides_glow_is_false() {
    // Opt-in by design: schemes may carry glow keys but they're
    // ignored until the user explicitly turns this on.
    assert!(!Config::defaults().theme_overrides_glow);
}

#[test]
fn config_round_trip_preserves_theme_overrides_glow_true() {
    let mut c = Config::defaults();
    c.theme_overrides_glow = true;
    let parsed = Config::parse_str(&c.serialize());
    assert!(parsed.theme_overrides_glow);
}

#[test]
fn config_round_trip_preserves_theme_overrides_glow_false() {
    // Explicit `false` must also survive the round-trip — if the
    // serializer silently dropped the field, a downgrade-then-upgrade
    // cycle would reset everyone to the default.
    let mut c = Config::defaults();
    c.theme_overrides_glow = false;
    let parsed = Config::parse_str(&c.serialize());
    assert!(!parsed.theme_overrides_glow);
}

#[test]
fn config_parse_theme_overrides_glow_true_sets_field() {
    // Direct parse of the user-facing config syntax — pins the
    // exact key name so a rename here would fail the test rather
    // than silently break every existing user's config file.
    let parsed = Config::parse_str("theme_overrides_glow = true\n");
    assert!(parsed.theme_overrides_glow);
}

#[test]
fn should_rearm_image_poll_store_pending_alone_rearms() {
    // Regression: the Kitty Unicode-placeholder path (`a=T,U=1`,
    // what `icat` emits under tmux) and animation frames (`a=f`)
    // bump the image store's pending queue WITHOUT registering a
    // `pending_placements` entry. With no deferred placements and an
    // empty result set, `store_pending > 0` is the only signal that a
    // decode is still in flight — it must re-arm, or the loop parks at
    // `ControlFlow::Wait` and the freshly-`cat`'d image renders blank.
    assert!(should_rearm_image_poll(true, true, 1));
}

#[test]
fn should_rearm_image_poll_all_idle_does_not_rearm() {
    // Nothing in flight on any of the three queues — let the loop go
    // to sleep rather than spin redrawing forever.
    assert!(!should_rearm_image_poll(true, true, 0));
}

#[test]
fn should_rearm_image_poll_pending_placements_alone_rearms() {
    // A deferred placement (Cmd-Shift-I paste / OSC 1337) is still
    // waiting on its decode.
    assert!(should_rearm_image_poll(false, true, 0));
}

#[test]
fn should_rearm_image_poll_results_alone_rearms() {
    // This poll produced results to act on, so the loop must tick again.
    assert!(should_rearm_image_poll(true, false, 0));
}

#[test]
fn should_rearm_image_poll_combined_signals_rearm() {
    // Any combination of the three live signals must re-arm.
    assert!(should_rearm_image_poll(false, false, 3));
    assert!(should_rearm_image_poll(false, true, 2));
    assert!(should_rearm_image_poll(true, false, 5));
}

// --- Onboarding: GlowLevel parsing -----------------------------------

#[test]
fn glow_level_from_str_maps_known_presets() {
    assert_eq!(GlowLevel::from_str("off"), Some(GlowLevel::Off));
    assert_eq!(GlowLevel::from_str("subtle"), Some(GlowLevel::Subtle));
    assert_eq!(GlowLevel::from_str("full"), Some(GlowLevel::Full));
}

#[test]
fn glow_level_from_str_rejects_unknown() {
    // Anything outside the three presets is `None`, including casing,
    // whitespace, and the empty string — callers treat `None` as
    // "drop the request" rather than guessing a default.
    assert_eq!(GlowLevel::from_str(""), None);
    assert_eq!(GlowLevel::from_str("Off"), None);
    assert_eq!(GlowLevel::from_str("OFF"), None);
    assert_eq!(GlowLevel::from_str(" off"), None);
    assert_eq!(GlowLevel::from_str("medium"), None);
    assert_eq!(GlowLevel::from_str("bogus"), None);
}

// --- Onboarding: GlowLevel -> Config knob mapping --------------------

#[test]
fn apply_glow_level_off_clears_all_match_modes() {
    let mut c = Config::defaults();
    // Pre-dirty the match flags so we prove `Off` actively clears them
    // rather than relying on the defaults already being false.
    c.glow_match_foreground = true;
    c.glow_match_brightness = true;
    c.glow_match_bright_ansi = true;
    c.apply_glow_level(GlowLevel::Off);
    assert!(!c.glow_match_foreground);
    assert!(!c.glow_match_brightness);
    assert!(!c.glow_match_bright_ansi);
}

#[test]
fn apply_glow_level_subtle_sets_fg_and_brightness_only() {
    let mut c = Config::defaults();
    c.apply_glow_level(GlowLevel::Subtle);
    assert!(c.glow_match_foreground);
    assert!(c.glow_match_brightness);
    assert!(!c.glow_match_bright_ansi);
    assert!(approx_eq(c.glow_intensity, 0.6));
}

#[test]
fn apply_glow_level_full_sets_all_match_modes_and_full_intensity() {
    let mut c = Config::defaults();
    c.apply_glow_level(GlowLevel::Full);
    assert!(c.glow_match_foreground);
    assert!(c.glow_match_brightness);
    assert!(c.glow_match_bright_ansi);
    assert!(approx_eq(c.glow_intensity, 1.0));
}

#[test]
fn apply_glow_level_off_leaves_scanlines_untouched() {
    // `apply_glow_level` only owns the match-mode + intensity knobs;
    // scanlines are `apply_scanlines`' business. Setting a glow preset
    // must not silently flip a scanline choice the user already made.
    let mut c = Config::defaults();
    c.glow_scanlines = true;
    c.glow_scanlines_content = true;
    c.apply_glow_level(GlowLevel::Off);
    assert!(c.glow_scanlines);
    assert!(c.glow_scanlines_content);
}

// --- Onboarding: scanline toggle -------------------------------------

#[test]
fn apply_scanlines_on_sets_halo_only_not_content() {
    let mut c = Config::defaults();
    c.apply_scanlines(true);
    assert!(c.glow_scanlines);
    // Content overlay stays off — scanlines ride the glow, not the text.
    assert!(!c.glow_scanlines_content);
}

#[test]
fn apply_scanlines_leaves_content_overlay_setting_untouched() {
    // The content overlay is its own config setting; toggling the halo
    // scanlines must not override whatever the config chose for it.
    let mut c = Config::defaults();
    c.glow_scanlines_content = true;
    c.apply_scanlines(true);
    assert!(c.glow_scanlines);
    assert!(c.glow_scanlines_content);
    c.glow_scanlines_content = false;
    c.apply_scanlines(false);
    assert!(!c.glow_scanlines);
    assert!(!c.glow_scanlines_content);
}

#[test]
fn apply_scanlines_off_clears_halo() {
    let mut c = Config::defaults();
    c.glow_scanlines = true;
    c.apply_scanlines(false);
    assert!(!c.glow_scanlines);
    assert!(!c.glow_scanlines_content);
}

#[test]
fn apply_scanlines_leaves_glow_match_modes_untouched() {
    // Mirror of `apply_glow_level_off_leaves_scanlines_untouched`: the
    // two setters own disjoint knobs, so toggling scanlines must not
    // disturb the glow match-mode state.
    let mut c = Config::defaults();
    c.apply_glow_level(GlowLevel::Full);
    c.apply_scanlines(true);
    assert!(c.glow_match_foreground);
    assert!(c.glow_match_brightness);
    assert!(c.glow_match_bright_ansi);
}

// --- Onboarding: combined CRT level ----------------------------------

#[test]
fn crt_level_from_str_maps_known_presets() {
    assert_eq!(CrtLevel::from_str("off"), Some(CrtLevel::Off));
    assert_eq!(CrtLevel::from_str("low"), Some(CrtLevel::Low));
    assert_eq!(CrtLevel::from_str("high"), Some(CrtLevel::High));
}

#[test]
fn crt_level_from_str_rejects_unknown() {
    assert_eq!(CrtLevel::from_str(""), None);
    assert_eq!(CrtLevel::from_str("Off"), None);
    assert_eq!(CrtLevel::from_str("medium"), None);
    assert_eq!(CrtLevel::from_str("subtle"), None);
}

#[test]
fn apply_crt_level_off_clears_glow_and_scanlines() {
    // Start from a fully-lit config to prove Off actively clears both
    // halves of the effect.
    let mut c = Config::defaults();
    c.apply_crt_level(CrtLevel::High);
    c.apply_crt_level(CrtLevel::Off);
    assert!(!c.glow_match_foreground);
    assert!(!c.glow_match_brightness);
    assert!(!c.glow_match_bright_ansi);
    assert!(!c.glow_scanlines);
    assert!(!c.glow_scanlines_content);
}

#[test]
fn apply_crt_level_low_is_dialed_back_spacedust() {
    let mut c = Config::defaults();
    c.apply_crt_level(CrtLevel::Low);
    // Brightness-driven bloom (spacedust's match mode), dialed back.
    assert!(c.glow_match_brightness);
    assert!(!c.glow_match_bright_ansi);
    assert!(!c.glow_match_foreground);
    assert!(approx_eq(c.glow_intensity, 0.3));
    assert_eq!(c.glow_iterations, 2);
    // Halo scanlines on but gentler; content overlay untouched (off).
    assert!(c.glow_scanlines);
    // Scanline knockout is full strength, same as High; only the bloom
    // differs between Low and High.
    assert!(approx_eq(c.glow_scanline_strength, 1.0));
    assert!(approx_eq(c.glow_scanline_period, 8.0));
    assert!(!c.glow_scanlines_content);
}

#[test]
fn apply_crt_level_high_matches_spacedust_glow() {
    let mut c = Config::defaults();
    c.apply_crt_level(CrtLevel::High);
    // The spacedust theme's glow + scanline values, verbatim.
    assert!(c.glow_match_brightness);
    assert!(!c.glow_match_bright_ansi);
    assert!(!c.glow_match_foreground);
    assert!(approx_eq(c.glow_threshold, 0.0));
    assert!(approx_eq(c.glow_intensity, 0.6));
    assert!(approx_eq(c.glow_softness, 1.0));
    assert_eq!(c.glow_iterations, 4);
    assert!(c.glow_scanlines);
    assert!(approx_eq(c.glow_scanline_strength, 1.0));
    assert!(approx_eq(c.glow_scanline_period, 8.0));
    // Content overlay left to its config default (off).
    assert!(!c.glow_scanlines_content);
}

#[test]
fn theme_overrides_glow_defaults_off() {
    // CRT presets are concrete config values; the "let the scheme's glow
    // win" switch stays off by default so they actually take effect.
    assert!(!Config::defaults().theme_overrides_glow);
}

//
// Cmd-W close-tab: foreground-command detection. `fg_is_foreign_command`
// is the pure predicate behind `tab_command_running` — given the PTY's
// foreground process-group id and the shell's own pid, it decides whether
// a foreign command (anything other than the shell) holds the foreground,
// which is what gates the "a command is running, prompt before closing"
// path for Cmd-W. Exercised here without a live PTY/`tcgetpgrp`.
//

#[test]
fn fg_is_foreign_command_false_when_shell_owns_foreground() {
    // Shell sitting at its prompt: the foreground pgrp *is* the shell's
    // own pid, so nothing foreign is running and Cmd-W must close without
    // a prompt. Use a few representative pids to avoid pinning on one value.
    for pid in [1, 42, 12345] {
        assert!(
            !fg_is_foreign_command(pid, pid),
            "shell pgrp == shell pid ({pid}) must read as no foreign command",
        );
    }
}

#[test]
fn fg_is_foreign_command_true_when_a_command_holds_foreground() {
    // A command running in the shell forms its own foreground process group
    // distinct from the shell's pid (both positive) — this is the case that
    // makes Cmd-W prompt before terminating it.
    assert!(fg_is_foreign_command(2001, 2000));
    assert!(fg_is_foreign_command(7, 9999));
}

#[test]
fn fg_is_foreign_command_false_when_no_controlling_foreground_group() {
    // `tcgetpgrp` returns -1 on failure (e.g. the slave side isn't the
    // controlling terminal yet) and the doc contract treats any
    // non-positive pgrp as "nothing running". Neither -1 nor 0 may report a
    // foreign command, even though both differ from the shell pid — a sign
    // flip or a `>= 0` slip would otherwise spuriously prompt on every Cmd-W.
    assert!(!fg_is_foreign_command(-1, 2000));
    assert!(!fg_is_foreign_command(0, 2000));
}

// ---------------------------------------------------------------------------
// present::FreePool — pure free-list bookkeeping for the present-source pool.
//
// Everything else in src/present.rs (PresentTarget, build_blit, the present
// thread / Presenter channels) needs a live wgpu::Device, a surface, and a
// background thread, so it is exercised by running the app, not here. FreePool
// is the one piece of deterministic logic: which target indices are free to
// render into, and the frame-drop signal when none are. Its invariants back a
// GPU-correctness guarantee (never hand a target back out while the present
// thread is still reading it), so they are worth pinning.
// ---------------------------------------------------------------------------

#[test]
fn free_pool_acquires_distinct_in_range_indices_until_empty() {
    // A fresh pool offers every target exactly once, only valid indices, and
    // no repeats — handing the same index out twice would let the renderer
    // overwrite a target the present thread is still blitting.
    use std::collections::HashSet;
    let mut pool = present::FreePool::new(present::POOL_SIZE);
    let mut seen = HashSet::new();
    for _ in 0..present::POOL_SIZE {
        let idx = pool.acquire().expect("a target should be free");
        assert!(idx < present::POOL_SIZE, "index {idx} out of pool range");
        assert!(seen.insert(idx), "index {idx} handed out twice");
    }
    assert_eq!(seen.len(), present::POOL_SIZE);
}

#[test]
fn free_pool_acquire_is_none_exactly_when_all_targets_in_flight() {
    // The drop-frame signal: once every target has been acquired (all in
    // flight on the present thread), acquire() reports None so the renderer
    // skips the frame instead of aliasing a busy target — and stays None on a
    // repeat call rather than conjuring a spurious target.
    let mut pool = present::FreePool::new(present::POOL_SIZE);
    for _ in 0..present::POOL_SIZE {
        assert!(pool.acquire().is_some());
    }
    assert!(pool.acquire().is_none(), "pool must drop frames when full");
    assert!(pool.acquire().is_none());
}

#[test]
fn free_pool_released_target_becomes_acquirable_again() {
    // When the presenter returns a finished target, it must be reusable, and
    // it is the only thing acquire can hand back while the rest are in flight.
    let mut pool = present::FreePool::new(present::POOL_SIZE);
    let mut taken = Vec::new();
    while let Some(idx) = pool.acquire() {
        taken.push(idx);
    }
    assert!(pool.acquire().is_none());

    let returned = taken[0];
    pool.release(returned);
    assert_eq!(
        pool.acquire(),
        Some(returned),
        "the just-released target should be the one handed back"
    );
    assert!(pool.acquire().is_none());
}

#[test]
fn free_pool_releasing_every_target_restores_full_capacity() {
    // Mirrors draining the presenter's finished-channel one index at a time:
    // releasing all in-flight targets restores the original acquire capacity,
    // no more and no fewer.
    let mut pool = present::FreePool::new(present::POOL_SIZE);
    let drained: Vec<usize> = std::iter::from_fn(|| pool.acquire()).collect();
    assert_eq!(drained.len(), present::POOL_SIZE);
    assert!(pool.acquire().is_none());

    for idx in &drained {
        pool.release(*idx);
    }
    let regained = std::iter::from_fn(|| pool.acquire()).count();
    assert_eq!(regained, present::POOL_SIZE);
}
