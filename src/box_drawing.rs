// Procedural rasterization of Unicode box-drawing (U+2500..U+257F) and
// block-element (U+2580..U+259F) glyphs.
//
// FreeType-rasterized box-drawing glyphs frequently end on sub-pixel
// boundaries: the stroke endings get ~50% coverage instead of 100%, leaving
// hairline gaps where ╭ on row N should join ╰ on row N+1. The fix used by
// professional terminals (Alacritty, Kitty, WezTerm) is to synthesize these
// glyphs algorithmically from cell metrics so every stroke lands on integer
// pixel boundaries and every connecting glyph in the family puts its strokes
// at the SAME canonical coordinates — guaranteeing pixel-perfect joins.
//
// This module exports `synth(ch, cell_w, cell_h, ascender)`. The caller
// (`font::build_atlas`) consults it before going to FreeType; if it returns
// Some, that bitmap is packed into the atlas verbatim (no edge-hardening).
//
// Coverage:
//   U+2500..U+254B   light/heavy/mixed straight + corner + tee + cross   yes
//   U+254C..U+254F   dashed                                              approx (drawn solid)
//   U+2550..U+256C   double-line corners/tees/crosses                    yes
//   U+256D..U+2570   light arcs (rounded corners)                        yes
//   U+2571..U+2573   diagonals                                           yes (Bresenham AA)
//   U+2574..U+257B   half-strokes (terminators)                          yes
//   U+257C..U+257F   mixed light/heavy joins                             yes
//   U+2580..U+2590   block halves / eighths                              yes
//   U+2591..U+2593   shaded blocks                                       yes (regular dither)
//   U+2594..U+259F   top/bottom eighths + quadrant blocks                yes
//
// Skipped/approximated:
//   - Dashed straights (U+2504..U+250B, U+254C..U+254F) are drawn as solid
//     light/heavy strokes. The dashing is purely cosmetic and most users
//     won't notice; doing it right would mean dashing patterns that align
//     with cell width which is fragile.
//   - Diagonals (U+2571..U+2573) use a simple Wu-line AA renderer. They
//     don't need to connect across cells (no diagonal connectors in the
//     codepoint range), so AA quality is acceptable.

pub struct Bitmap {
    pub data: Vec<u8>,
    pub width: usize,
    pub height: usize,
    pub bearing_x: isize,
    pub bearing_y: isize,
    pub advance_x: usize,
}

impl Bitmap {
    fn new(w: usize, h: usize, bearing_y: isize) -> Self {
        Bitmap {
            data: vec![0u8; w * h],
            width: w,
            height: h,
            bearing_x: 0,
            bearing_y,
            advance_x: w,
        }
    }

    fn put(&mut self, x: isize, y: isize, alpha: u8) {
        if x < 0 || y < 0 || x as usize >= self.width || y as usize >= self.height {
            return;
        }
        let i = y as usize * self.width + x as usize;
        // Take the max so overlapping primitives don't darken each other.
        if self.data[i] < alpha {
            self.data[i] = alpha;
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Canonical stroke geometry
// ─────────────────────────────────────────────────────────────────────────
//
// Every glyph in the family uses the same canonical row for horizontals and
// the same canonical column for verticals. That makes ╭'s vertical leg align
// with │'s, so when the terminal stacks them they join with no offset.

#[derive(Copy, Clone)]
struct Geom {
    cell_w: usize,
    cell_h: usize,
    // Center column of a vertical stroke. We use the same value for "light"
    // and "heavy" so heavy ┃ stays centered on light │'s axis.
    cx: usize,
    cy: usize,
    t_light: usize,
    t_heavy: usize,
}

impl Geom {
    fn new(cell_w: usize, cell_h: usize) -> Self {
        let t_light = (((cell_h as f32) / 16.0).round() as usize).max(1);
        let t_heavy = (((cell_h as f32) / 8.0).round() as usize).max(t_light + 1).max(2);
        // Center the light vertical stroke. We want stroke columns
        // [cx - t/2 .. cx - t/2 + t]. Bias so that for even t the stroke
        // straddles cx in the same way regardless of cell parity.
        let cx = cell_w / 2;
        let cy = cell_h / 2;
        Geom { cell_w, cell_h, cx, cy, t_light, t_heavy }
    }

    // Horizontal stroke band centered on `cy_band`, returns inclusive y
    // range [y0, y1) for a stroke of given thickness centered as exactly as
    // possible on the canonical center row.
    fn h_band(&self, cy_band: usize, thickness: usize) -> (usize, usize) {
        let half = thickness / 2;
        let y0 = cy_band.saturating_sub(half);
        let y1 = (y0 + thickness).min(self.cell_h);
        (y0, y1)
    }

    fn v_band(&self, cx_band: usize, thickness: usize) -> (usize, usize) {
        let half = thickness / 2;
        let x0 = cx_band.saturating_sub(half);
        let x1 = (x0 + thickness).min(self.cell_w);
        (x0, x1)
    }
}

fn fill_rect(buf: &mut Bitmap, x0: usize, y0: usize, x1: usize, y1: usize) {
    let x1 = x1.min(buf.width);
    let y1 = y1.min(buf.height);
    for y in y0..y1 {
        for x in x0..x1 {
            buf.data[y * buf.width + x] = 255;
        }
    }
}

// Horizontal stroke from cell-edge column `x_from` to `x_to` (exclusive),
// centered on canonical row `cy`, of given thickness.
fn h_line(buf: &mut Bitmap, g: &Geom, x_from: usize, x_to: usize, thickness: usize) {
    let (y0, y1) = g.h_band(g.cy, thickness);
    fill_rect(buf, x_from, y0, x_to, y1);
}

fn v_line(buf: &mut Bitmap, g: &Geom, y_from: usize, y_to: usize, thickness: usize) {
    let (x0, x1) = g.v_band(g.cx, thickness);
    fill_rect(buf, x0, y_from, x1, y_to);
}

// "Light" horizontal connector from a cell edge to the cell center.
fn h_left(buf: &mut Bitmap, g: &Geom, thickness: usize) {
    let (_, x1) = g.v_band(g.cx, thickness);
    h_line(buf, g, 0, x1, thickness);
}
fn h_right(buf: &mut Bitmap, g: &Geom, thickness: usize) {
    let (x0, _) = g.v_band(g.cx, thickness);
    h_line(buf, g, x0, g.cell_w, thickness);
}
fn v_top(buf: &mut Bitmap, g: &Geom, thickness: usize) {
    let (_, y1) = g.h_band(g.cy, thickness);
    v_line(buf, g, 0, y1, thickness);
}
fn v_bot(buf: &mut Bitmap, g: &Geom, thickness: usize) {
    let (y0, _) = g.h_band(g.cy, thickness);
    v_line(buf, g, y0, g.cell_h, thickness);
}

// ─────────────────────────────────────────────────────────────────────────
// Double-line offsets
// ─────────────────────────────────────────────────────────────────────────

// For double-line glyphs we draw two parallel light strokes separated by
// `t_light` pixels of gap, symmetric around the canonical center.
fn double_v_bands(g: &Geom) -> (usize, usize, usize, usize) {
    let t = g.t_light;
    // Outer-to-outer width = 3t. Center on cx.
    let total = 3 * t;
    let left_x0 = g.cx.saturating_sub(total / 2);
    let left_x1 = left_x0 + t;
    let right_x0 = left_x0 + 2 * t;
    let right_x1 = right_x0 + t;
    (left_x0, left_x1, right_x0, right_x1.min(g.cell_w))
}

fn double_h_bands(g: &Geom) -> (usize, usize, usize, usize) {
    let t = g.t_light;
    let total = 3 * t;
    let top_y0 = g.cy.saturating_sub(total / 2);
    let top_y1 = top_y0 + t;
    let bot_y0 = top_y0 + 2 * t;
    let bot_y1 = bot_y0 + t;
    (top_y0, top_y1, bot_y0, bot_y1.min(g.cell_h))
}

// ─────────────────────────────────────────────────────────────────────────
// Arcs (╭╮╯╰)
// ─────────────────────────────────────────────────────────────────────────
//
// A light-weight arc that meets the canonical horizontal-center row at the
// cell edge and the canonical vertical-center column at the opposite cell
// edge. We render it via signed-distance to a circular ring with a 1-pixel
// AA band, then OR in the straight stub from the arc's tangent point back
// to the cell edge so the glyph still connects to neighbours.

fn rasterize_arc(
    buf: &mut Bitmap,
    cx: f32,
    cy: f32,
    radius: f32,
    thickness: f32,
    angle_from: f32, // radians
    angle_to: f32,
) {
    let outer = radius + thickness * 0.5;
    let (x0, y0, x1, y1) = (
        (cx - outer - 1.0).floor().max(0.0) as usize,
        (cy - outer - 1.0).floor().max(0.0) as usize,
        (cx + outer + 1.0).ceil().min(buf.width as f32) as usize,
        (cy + outer + 1.0).ceil().min(buf.height as f32) as usize,
    );
    let half_t = thickness * 0.5;
    for y in y0..y1 {
        for x in x0..x1 {
            let dx = x as f32 + 0.5 - cx;
            let dy = y as f32 + 0.5 - cy;
            let d = (dx * dx + dy * dy).sqrt();
            // Distance from the ring centerline.
            let ring_d = (d - radius).abs();
            // Angle filter — only inside the requested sweep.
            let a = dy.atan2(dx);
            let mut a = a;
            // Normalize a, angle_from, angle_to to [0, 2π).
            let two_pi = std::f32::consts::TAU;
            while a < 0.0 { a += two_pi; }
            let mut af = angle_from; while af < 0.0 { af += two_pi; }
            let mut at = angle_to; while at < 0.0 { at += two_pi; }
            let in_arc = if af <= at {
                a >= af && a <= at
            } else {
                a >= af || a <= at
            };
            if !in_arc { continue; }
            // 1-px AA band on each side of the ring.
            let cov = if ring_d <= half_t - 0.5 {
                1.0
            } else if ring_d >= half_t + 0.5 {
                0.0
            } else {
                (half_t + 0.5 - ring_d).clamp(0.0, 1.0)
            };
            if cov > 0.0 {
                let alpha = (cov * 255.0).round() as u8;
                let i = y * buf.width + x;
                if buf.data[i] < alpha {
                    buf.data[i] = alpha;
                }
            }
        }
    }
}

// Render a rounded corner. `quadrant` describes which two cell edges the
// arc connects: e.g. ╭ connects right-edge horizontal to bottom-edge
// vertical (BR quadrant interior), ╰ is TR, ╯ is TL, ╮ is BL.
//
// We anchor the arc so it tangentially meets the canonical horizontal
// center row and canonical vertical center column. The arc center sits
// inside the "interior" cell quadrant at distance `radius` from each.
fn arc_corner(buf: &mut Bitmap, g: &Geom, corner: char) {
    let t = g.t_light as f32;
    // Use a radius that's the smaller of half-width or half-height minus a
    // little, so the arc doesn't bleed past the canonical center on tall
    // narrow cells. Floor to integer so the tangent points land on the
    // canonical center row/column exactly.
    let radius = ((g.cell_w.min(g.cell_h) as f32) * 0.45).floor().max(2.0);
    // Tangent points: arc must meet (cx, edge_y) horizontally and (edge_x,
    // cy) vertically. Arc center is offset by `radius` perpendicular to
    // each tangent direction.
    let (acx, acy, a_from, a_to, x_stub_range, y_stub_range);
    let cx = g.cx as f32 + 0.5;
    let cy = g.cy as f32 + 0.5;
    use std::f32::consts::PI;
    match corner {
        // ╭ TL of box → connects right-edge & bottom-edge of THIS cell.
        // Arc curves through the top-left of the (cx, cy) interior.
        '\u{256D}' => {
            acx = cx + radius;
            acy = cy + radius;
            a_from = PI;          // pointing to (-x): tangent meets vertical line at top
            a_to = 1.5 * PI;      // pointing to (-y)
            // Stub: horizontal from tangent col to right cell edge,
            // vertical from tangent row to bottom cell edge.
            x_stub_range = ((acx as usize).min(g.cell_w), g.cell_w); // horiz right of arc
            y_stub_range = ((acy as usize).min(g.cell_h), g.cell_h); // vert below arc
        }
        // ╮ TR of box → left & bottom
        '\u{256E}' => {
            acx = cx - radius;
            acy = cy + radius;
            a_from = 1.5 * PI;
            a_to = 2.0 * PI;
            x_stub_range = (0, (acx as usize).min(g.cell_w));
            y_stub_range = ((acy as usize).min(g.cell_h), g.cell_h);
        }
        // ╯ BR of box → left & top
        '\u{256F}' => {
            acx = cx - radius;
            acy = cy - radius;
            a_from = 0.0;
            a_to = 0.5 * PI;
            x_stub_range = (0, (acx as usize).min(g.cell_w));
            y_stub_range = (0, (acy as usize).min(g.cell_h));
        }
        // ╰ BL of box → right & top
        '\u{2570}' => {
            acx = cx + radius;
            acy = cy - radius;
            a_from = 0.5 * PI;
            a_to = PI;
            x_stub_range = ((acx as usize).min(g.cell_w), g.cell_w);
            y_stub_range = (0, (acy as usize).min(g.cell_h));
        }
        _ => unreachable!(),
    }
    // The stub straights guarantee pixel-perfect connection to neighbour
    // cells regardless of how the arc rasterizer rounds.
    let (vy0, vy1) = g.h_band(g.cy, g.t_light);
    let (vx0, vx1) = g.v_band(g.cx, g.t_light);
    // Horizontal stub at the canonical center row.
    fill_rect(buf, x_stub_range.0, vy0, x_stub_range.1, vy1);
    // Vertical stub at the canonical center column.
    fill_rect(buf, vx0, y_stub_range.0, vx1, y_stub_range.1);
    rasterize_arc(buf, acx, acy, radius, t, a_from, a_to);
}

// ─────────────────────────────────────────────────────────────────────────
// Diagonals (Xiaolin Wu)
// ─────────────────────────────────────────────────────────────────────────

fn wu_line(buf: &mut Bitmap, x0: f32, y0: f32, x1: f32, y1: f32) {
    let steep = (y1 - y0).abs() > (x1 - x0).abs();
    let (mut x0, mut y0, mut x1, mut y1) = if steep { (y0, x0, y1, x1) } else { (x0, y0, x1, y1) };
    if x0 > x1 {
        std::mem::swap(&mut x0, &mut x1);
        std::mem::swap(&mut y0, &mut y1);
    }
    let dx = x1 - x0;
    let dy = y1 - y0;
    let gradient = if dx.abs() < 1e-6 { 1.0 } else { dy / dx };

    let mut intery = y0 + gradient * (x0.round() - x0);
    let xpx0 = x0.round() as isize;
    let xpx1 = x1.round() as isize;
    for x in xpx0..=xpx1 {
        let yi = intery.floor() as isize;
        let f = intery - intery.floor();
        let a0 = ((1.0 - f) * 255.0) as u8;
        let a1 = (f * 255.0) as u8;
        if steep {
            buf.put(yi, x, a0);
            buf.put(yi + 1, x, a1);
        } else {
            buf.put(x, yi, a0);
            buf.put(x, yi + 1, a1);
        }
        intery += gradient;
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Public entry
// ─────────────────────────────────────────────────────────────────────────

pub fn synth(ch: char, cell_w: usize, cell_h: usize, ascender: isize) -> Option<Bitmap> {
    if cell_w == 0 || cell_h == 0 {
        return None;
    }
    let cp = ch as u32;
    let in_box = (0x2500..=0x257F).contains(&cp);
    let in_block = (0x2580..=0x259F).contains(&cp);
    if !in_box && !in_block {
        return None;
    }
    let g = Geom::new(cell_w, cell_h);
    let mut buf = Bitmap::new(cell_w, cell_h, ascender);

    if in_box {
        draw_box(&mut buf, &g, ch);
    } else {
        draw_block(&mut buf, &g, ch);
    }
    Some(buf)
}

fn draw_box(buf: &mut Bitmap, g: &Geom, ch: char) {
    let t_l = g.t_light;
    let t_h = g.t_heavy;
    match ch {
        // ─ light horizontal
        '\u{2500}' | '\u{2504}' | '\u{2508}' | '\u{254C}' => {
            h_line(buf, g, 0, g.cell_w, t_l);
        }
        // ━ heavy horizontal
        '\u{2501}' | '\u{2505}' | '\u{2509}' | '\u{254D}' => {
            h_line(buf, g, 0, g.cell_w, t_h);
        }
        // │ light vertical
        '\u{2502}' | '\u{2506}' | '\u{250A}' | '\u{254E}' => {
            v_line(buf, g, 0, g.cell_h, t_l);
        }
        // ┃ heavy vertical
        '\u{2503}' | '\u{2507}' | '\u{250B}' | '\u{254F}' => {
            v_line(buf, g, 0, g.cell_h, t_h);
        }

        // Light corners ┌ ┐ └ ┘
        '\u{250C}' => { h_right(buf, g, t_l); v_bot(buf, g, t_l); }
        '\u{250D}' => { h_right(buf, g, t_h); v_bot(buf, g, t_l); }
        '\u{250E}' => { h_right(buf, g, t_l); v_bot(buf, g, t_h); }
        '\u{250F}' => { h_right(buf, g, t_h); v_bot(buf, g, t_h); }
        '\u{2510}' => { h_left(buf, g, t_l); v_bot(buf, g, t_l); }
        '\u{2511}' => { h_left(buf, g, t_h); v_bot(buf, g, t_l); }
        '\u{2512}' => { h_left(buf, g, t_l); v_bot(buf, g, t_h); }
        '\u{2513}' => { h_left(buf, g, t_h); v_bot(buf, g, t_h); }
        '\u{2514}' => { h_right(buf, g, t_l); v_top(buf, g, t_l); }
        '\u{2515}' => { h_right(buf, g, t_h); v_top(buf, g, t_l); }
        '\u{2516}' => { h_right(buf, g, t_l); v_top(buf, g, t_h); }
        '\u{2517}' => { h_right(buf, g, t_h); v_top(buf, g, t_h); }
        '\u{2518}' => { h_left(buf, g, t_l); v_top(buf, g, t_l); }
        '\u{2519}' => { h_left(buf, g, t_h); v_top(buf, g, t_l); }
        '\u{251A}' => { h_left(buf, g, t_l); v_top(buf, g, t_h); }
        '\u{251B}' => { h_left(buf, g, t_h); v_top(buf, g, t_h); }

        // T-junctions ├-style (vertical + horizontal stub right)
        '\u{251C}' => { v_line(buf, g, 0, g.cell_h, t_l); h_right(buf, g, t_l); }
        '\u{251D}' => { v_line(buf, g, 0, g.cell_h, t_l); h_right(buf, g, t_h); }
        '\u{251E}' => { v_top(buf, g, t_h); v_bot(buf, g, t_l); h_right(buf, g, t_l); }
        '\u{251F}' => { v_top(buf, g, t_l); v_bot(buf, g, t_h); h_right(buf, g, t_l); }
        '\u{2520}' => { v_line(buf, g, 0, g.cell_h, t_h); h_right(buf, g, t_l); }
        '\u{2521}' => { v_top(buf, g, t_h); v_bot(buf, g, t_l); h_right(buf, g, t_h); }
        '\u{2522}' => { v_top(buf, g, t_l); v_bot(buf, g, t_h); h_right(buf, g, t_h); }
        '\u{2523}' => { v_line(buf, g, 0, g.cell_h, t_h); h_right(buf, g, t_h); }

        // ┤
        '\u{2524}' => { v_line(buf, g, 0, g.cell_h, t_l); h_left(buf, g, t_l); }
        '\u{2525}' => { v_line(buf, g, 0, g.cell_h, t_l); h_left(buf, g, t_h); }
        '\u{2526}' => { v_top(buf, g, t_h); v_bot(buf, g, t_l); h_left(buf, g, t_l); }
        '\u{2527}' => { v_top(buf, g, t_l); v_bot(buf, g, t_h); h_left(buf, g, t_l); }
        '\u{2528}' => { v_line(buf, g, 0, g.cell_h, t_h); h_left(buf, g, t_l); }
        '\u{2529}' => { v_top(buf, g, t_h); v_bot(buf, g, t_l); h_left(buf, g, t_h); }
        '\u{252A}' => { v_top(buf, g, t_l); v_bot(buf, g, t_h); h_left(buf, g, t_h); }
        '\u{252B}' => { v_line(buf, g, 0, g.cell_h, t_h); h_left(buf, g, t_h); }

        // ┬
        '\u{252C}' => { h_line(buf, g, 0, g.cell_w, t_l); v_bot(buf, g, t_l); }
        '\u{252D}' => { h_left(buf, g, t_h); h_right(buf, g, t_l); v_bot(buf, g, t_l); }
        '\u{252E}' => { h_left(buf, g, t_l); h_right(buf, g, t_h); v_bot(buf, g, t_l); }
        '\u{252F}' => { h_line(buf, g, 0, g.cell_w, t_h); v_bot(buf, g, t_l); }
        '\u{2530}' => { h_line(buf, g, 0, g.cell_w, t_l); v_bot(buf, g, t_h); }
        '\u{2531}' => { h_left(buf, g, t_h); h_right(buf, g, t_l); v_bot(buf, g, t_h); }
        '\u{2532}' => { h_left(buf, g, t_l); h_right(buf, g, t_h); v_bot(buf, g, t_h); }
        '\u{2533}' => { h_line(buf, g, 0, g.cell_w, t_h); v_bot(buf, g, t_h); }

        // ┴
        '\u{2534}' => { h_line(buf, g, 0, g.cell_w, t_l); v_top(buf, g, t_l); }
        '\u{2535}' => { h_left(buf, g, t_h); h_right(buf, g, t_l); v_top(buf, g, t_l); }
        '\u{2536}' => { h_left(buf, g, t_l); h_right(buf, g, t_h); v_top(buf, g, t_l); }
        '\u{2537}' => { h_line(buf, g, 0, g.cell_w, t_h); v_top(buf, g, t_l); }
        '\u{2538}' => { h_line(buf, g, 0, g.cell_w, t_l); v_top(buf, g, t_h); }
        '\u{2539}' => { h_left(buf, g, t_h); h_right(buf, g, t_l); v_top(buf, g, t_h); }
        '\u{253A}' => { h_left(buf, g, t_l); h_right(buf, g, t_h); v_top(buf, g, t_h); }
        '\u{253B}' => { h_line(buf, g, 0, g.cell_w, t_h); v_top(buf, g, t_h); }

        // ┼
        '\u{253C}' => { h_line(buf, g, 0, g.cell_w, t_l); v_line(buf, g, 0, g.cell_h, t_l); }
        '\u{253D}' => { h_left(buf, g, t_h); h_right(buf, g, t_l); v_line(buf, g, 0, g.cell_h, t_l); }
        '\u{253E}' => { h_left(buf, g, t_l); h_right(buf, g, t_h); v_line(buf, g, 0, g.cell_h, t_l); }
        '\u{253F}' => { h_line(buf, g, 0, g.cell_w, t_h); v_line(buf, g, 0, g.cell_h, t_l); }
        '\u{2540}' => { h_line(buf, g, 0, g.cell_w, t_l); v_top(buf, g, t_h); v_bot(buf, g, t_l); }
        '\u{2541}' => { h_line(buf, g, 0, g.cell_w, t_l); v_top(buf, g, t_l); v_bot(buf, g, t_h); }
        '\u{2542}' => { h_line(buf, g, 0, g.cell_w, t_l); v_line(buf, g, 0, g.cell_h, t_h); }
        '\u{2543}' => { h_left(buf, g, t_h); h_right(buf, g, t_l); v_top(buf, g, t_h); v_bot(buf, g, t_l); }
        '\u{2544}' => { h_left(buf, g, t_l); h_right(buf, g, t_h); v_top(buf, g, t_h); v_bot(buf, g, t_l); }
        '\u{2545}' => { h_left(buf, g, t_h); h_right(buf, g, t_l); v_top(buf, g, t_l); v_bot(buf, g, t_h); }
        '\u{2546}' => { h_left(buf, g, t_l); h_right(buf, g, t_h); v_top(buf, g, t_l); v_bot(buf, g, t_h); }
        '\u{2547}' => { h_line(buf, g, 0, g.cell_w, t_h); v_top(buf, g, t_h); v_bot(buf, g, t_l); }
        '\u{2548}' => { h_line(buf, g, 0, g.cell_w, t_h); v_top(buf, g, t_l); v_bot(buf, g, t_h); }
        '\u{2549}' => { h_left(buf, g, t_h); h_right(buf, g, t_l); v_line(buf, g, 0, g.cell_h, t_h); }
        '\u{254A}' => { h_left(buf, g, t_l); h_right(buf, g, t_h); v_line(buf, g, 0, g.cell_h, t_h); }
        '\u{254B}' => { h_line(buf, g, 0, g.cell_w, t_h); v_line(buf, g, 0, g.cell_h, t_h); }

        // U+254C..U+254F: dashed handled in the straight-stroke arms above.

        // Double lines U+2550..U+256C
        '\u{2550}' => double_horizontal(buf, g),
        '\u{2551}' => double_vertical(buf, g),
        '\u{2552}' => { // ╒ single-V double-H corner top-left
            let (h_top, h_bot, _, _) = double_h_bands(g);
            let (vx0, vx1) = g.v_band(g.cx, t_l);
            // Two horizontals starting at vertical's right edge to the right cell edge.
            fill_rect(buf, vx1, h_top, g.cell_w, h_top + t_l);
            fill_rect(buf, vx1, h_bot, g.cell_w, h_bot + t_l);
            // Vertical from top of cell down to bottom of upper horizontal? Actually
            // ╒ goes down. Start from top horizontal band downward.
            fill_rect(buf, vx0, h_top, vx1, g.cell_h);
        }
        '\u{2553}' => { // ╓ double-V single-H top-left
            let (vl0, vl1, vr0, vr1) = double_v_bands(g);
            let (hy0, hy1) = g.h_band(g.cy, t_l);
            // Single horizontal from right of right vertical to right edge.
            fill_rect(buf, vr1, hy0, g.cell_w, hy1);
            // Two verticals from top of horizontal down to bottom of cell.
            fill_rect(buf, vl0, hy0, vl1, g.cell_h);
            fill_rect(buf, vr0, hy0, vr1, g.cell_h);
        }
        '\u{2554}' => { // ╔ double both
            double_corner(buf, g, true, true);
        }
        '\u{2555}' => { // ╕ single-V double-H top-right
            let (h_top, h_bot, _, _) = double_h_bands(g);
            let (vx0, vx1) = g.v_band(g.cx, t_l);
            fill_rect(buf, 0, h_top, vx0, h_top + t_l);
            fill_rect(buf, 0, h_bot, vx0, h_bot + t_l);
            fill_rect(buf, vx0, h_top, vx1, g.cell_h);
        }
        '\u{2556}' => { // ╖ double-V single-H top-right
            let (vl0, vl1, vr0, vr1) = double_v_bands(g);
            let (hy0, hy1) = g.h_band(g.cy, t_l);
            fill_rect(buf, 0, hy0, vl0, hy1);
            fill_rect(buf, vl0, hy0, vl1, g.cell_h);
            fill_rect(buf, vr0, hy0, vr1, g.cell_h);
        }
        '\u{2557}' => { // ╗ double both
            double_corner(buf, g, false, true);
        }
        '\u{2558}' => { // ╘ single-V double-H bottom-left
            let (h_top, h_bot, _, _) = double_h_bands(g);
            let (vx0, vx1) = g.v_band(g.cx, t_l);
            fill_rect(buf, vx1, h_top, g.cell_w, h_top + t_l);
            fill_rect(buf, vx1, h_bot, g.cell_w, h_bot + t_l);
            fill_rect(buf, vx0, 0, vx1, h_bot + t_l);
        }
        '\u{2559}' => { // ╙ double-V single-H bottom-left
            let (vl0, vl1, vr0, vr1) = double_v_bands(g);
            let (hy0, hy1) = g.h_band(g.cy, t_l);
            fill_rect(buf, vr1, hy0, g.cell_w, hy1);
            fill_rect(buf, vl0, 0, vl1, hy1);
            fill_rect(buf, vr0, 0, vr1, hy1);
        }
        '\u{255A}' => { // ╚
            double_corner(buf, g, true, false);
        }
        '\u{255B}' => { // ╛
            let (h_top, h_bot, _, _) = double_h_bands(g);
            let (vx0, vx1) = g.v_band(g.cx, t_l);
            fill_rect(buf, 0, h_top, vx0, h_top + t_l);
            fill_rect(buf, 0, h_bot, vx0, h_bot + t_l);
            fill_rect(buf, vx0, 0, vx1, h_bot + t_l);
        }
        '\u{255C}' => { // ╜
            let (vl0, vl1, vr0, vr1) = double_v_bands(g);
            let (hy0, hy1) = g.h_band(g.cy, t_l);
            fill_rect(buf, 0, hy0, vl0, hy1);
            fill_rect(buf, vl0, 0, vl1, hy1);
            fill_rect(buf, vr0, 0, vr1, hy1);
        }
        '\u{255D}' => { // ╝
            double_corner(buf, g, false, false);
        }
        '\u{255E}' => { // ╞ single-V double-H tee
            let (h_top, h_bot, _, _) = double_h_bands(g);
            let (vx0, vx1) = g.v_band(g.cx, t_l);
            fill_rect(buf, vx0, 0, vx1, g.cell_h);
            fill_rect(buf, vx1, h_top, g.cell_w, h_top + t_l);
            fill_rect(buf, vx1, h_bot, g.cell_w, h_bot + t_l);
        }
        '\u{255F}' => { // ╟ double-V single-H tee
            let (vl0, vl1, vr0, vr1) = double_v_bands(g);
            let (hy0, hy1) = g.h_band(g.cy, t_l);
            fill_rect(buf, vl0, 0, vl1, g.cell_h);
            fill_rect(buf, vr0, 0, vr1, g.cell_h);
            fill_rect(buf, vr1, hy0, g.cell_w, hy1);
        }
        '\u{2560}' => { // ╠ both double tee
            let (vl0, vl1, vr0, vr1) = double_v_bands(g);
            let (h_top, h_bot, _, _) = double_h_bands(g);
            // Left vertical: full height
            fill_rect(buf, vl0, 0, vl1, g.cell_h);
            // Right vertical: split around the horizontals
            fill_rect(buf, vr0, 0, vr1, h_top);
            fill_rect(buf, vr0, h_bot + t_l, vr1, g.cell_h);
            // Two horizontals from right vertical's right edge to cell edge
            fill_rect(buf, vr1, h_top, g.cell_w, h_top + t_l);
            fill_rect(buf, vr1, h_bot, g.cell_w, h_bot + t_l);
        }
        '\u{2561}' => { // ╡
            let (h_top, h_bot, _, _) = double_h_bands(g);
            let (vx0, vx1) = g.v_band(g.cx, t_l);
            fill_rect(buf, vx0, 0, vx1, g.cell_h);
            fill_rect(buf, 0, h_top, vx0, h_top + t_l);
            fill_rect(buf, 0, h_bot, vx0, h_bot + t_l);
        }
        '\u{2562}' => { // ╢
            let (vl0, vl1, vr0, vr1) = double_v_bands(g);
            let (hy0, hy1) = g.h_band(g.cy, t_l);
            fill_rect(buf, vl0, 0, vl1, g.cell_h);
            fill_rect(buf, vr0, 0, vr1, g.cell_h);
            fill_rect(buf, 0, hy0, vl0, hy1);
        }
        '\u{2563}' => { // ╣
            let (vl0, vl1, vr0, vr1) = double_v_bands(g);
            let (h_top, h_bot, _, _) = double_h_bands(g);
            fill_rect(buf, vr0, 0, vr1, g.cell_h);
            fill_rect(buf, vl0, 0, vl1, h_top);
            fill_rect(buf, vl0, h_bot + t_l, vl1, g.cell_h);
            fill_rect(buf, 0, h_top, vl0, h_top + t_l);
            fill_rect(buf, 0, h_bot, vl0, h_bot + t_l);
        }
        '\u{2564}' => { // ╤ single-V double-H top-tee
            let (h_top, h_bot, _, _) = double_h_bands(g);
            let (vx0, vx1) = g.v_band(g.cx, t_l);
            fill_rect(buf, 0, h_top, g.cell_w, h_top + t_l);
            fill_rect(buf, 0, h_bot, g.cell_w, h_bot + t_l);
            fill_rect(buf, vx0, h_bot + t_l, vx1, g.cell_h);
        }
        '\u{2565}' => { // ╥
            let (vl0, vl1, vr0, vr1) = double_v_bands(g);
            let (hy0, hy1) = g.h_band(g.cy, t_l);
            fill_rect(buf, 0, hy0, g.cell_w, hy1);
            fill_rect(buf, vl0, hy1, vl1, g.cell_h);
            fill_rect(buf, vr0, hy1, vr1, g.cell_h);
        }
        '\u{2566}' => { // ╦
            let (vl0, vl1, vr0, vr1) = double_v_bands(g);
            let (h_top, h_bot, _, _) = double_h_bands(g);
            fill_rect(buf, 0, h_top, g.cell_w, h_top + t_l);
            fill_rect(buf, 0, h_bot, vl0, h_bot + t_l);
            fill_rect(buf, vr1, h_bot, g.cell_w, h_bot + t_l);
            fill_rect(buf, vl0, h_bot, vl1, g.cell_h);
            fill_rect(buf, vr0, h_bot, vr1, g.cell_h);
        }
        '\u{2567}' => { // ╧ single-V double-H bottom-tee
            let (h_top, h_bot, _, _) = double_h_bands(g);
            let (vx0, vx1) = g.v_band(g.cx, t_l);
            fill_rect(buf, 0, h_top, g.cell_w, h_top + t_l);
            fill_rect(buf, 0, h_bot, g.cell_w, h_bot + t_l);
            fill_rect(buf, vx0, 0, vx1, h_top);
        }
        '\u{2568}' => { // ╨
            let (vl0, vl1, vr0, vr1) = double_v_bands(g);
            let (hy0, hy1) = g.h_band(g.cy, t_l);
            fill_rect(buf, 0, hy0, g.cell_w, hy1);
            fill_rect(buf, vl0, 0, vl1, hy0);
            fill_rect(buf, vr0, 0, vr1, hy0);
        }
        '\u{2569}' => { // ╩
            let (vl0, vl1, vr0, vr1) = double_v_bands(g);
            let (h_top, h_bot, _, _) = double_h_bands(g);
            fill_rect(buf, 0, h_bot, g.cell_w, h_bot + t_l);
            fill_rect(buf, 0, h_top, vl0, h_top + t_l);
            fill_rect(buf, vr1, h_top, g.cell_w, h_top + t_l);
            fill_rect(buf, vl0, 0, vl1, h_top);
            fill_rect(buf, vr0, 0, vr1, h_top);
        }
        '\u{256A}' => { // ╪ single-V double-H cross
            let (h_top, h_bot, _, _) = double_h_bands(g);
            let (vx0, vx1) = g.v_band(g.cx, t_l);
            fill_rect(buf, 0, h_top, g.cell_w, h_top + t_l);
            fill_rect(buf, 0, h_bot, g.cell_w, h_bot + t_l);
            fill_rect(buf, vx0, 0, vx1, g.cell_h);
        }
        '\u{256B}' => { // ╫
            let (vl0, vl1, vr0, vr1) = double_v_bands(g);
            let (hy0, hy1) = g.h_band(g.cy, t_l);
            fill_rect(buf, 0, hy0, g.cell_w, hy1);
            fill_rect(buf, vl0, 0, vl1, g.cell_h);
            fill_rect(buf, vr0, 0, vr1, g.cell_h);
        }
        '\u{256C}' => { // ╬ double cross
            let (vl0, vl1, vr0, vr1) = double_v_bands(g);
            let (h_top, h_bot, _, _) = double_h_bands(g);
            // Horizontals: split at the center void
            fill_rect(buf, 0, h_top, vl0, h_top + t_l);
            fill_rect(buf, vr1, h_top, g.cell_w, h_top + t_l);
            fill_rect(buf, 0, h_bot, vl0, h_bot + t_l);
            fill_rect(buf, vr1, h_bot, g.cell_w, h_bot + t_l);
            // Verticals: split similarly
            fill_rect(buf, vl0, 0, vl1, h_top);
            fill_rect(buf, vl0, h_bot + t_l, vl1, g.cell_h);
            fill_rect(buf, vr0, 0, vr1, h_top);
            fill_rect(buf, vr0, h_bot + t_l, vr1, g.cell_h);
        }

        // Arcs ╭╮╯╰
        '\u{256D}' | '\u{256E}' | '\u{256F}' | '\u{2570}' => arc_corner(buf, g, ch),

        // Diagonals ╱╲╳
        '\u{2571}' => {
            wu_line(buf, 0.0, g.cell_h as f32 - 0.5, g.cell_w as f32 - 0.5, 0.0);
        }
        '\u{2572}' => {
            wu_line(buf, 0.0, 0.0, g.cell_w as f32 - 0.5, g.cell_h as f32 - 0.5);
        }
        '\u{2573}' => {
            wu_line(buf, 0.0, g.cell_h as f32 - 0.5, g.cell_w as f32 - 0.5, 0.0);
            wu_line(buf, 0.0, 0.0, g.cell_w as f32 - 0.5, g.cell_h as f32 - 0.5);
        }

        // Half-strokes (terminators)
        '\u{2574}' => h_left(buf, g, t_l),
        '\u{2575}' => v_top(buf, g, t_l),
        '\u{2576}' => h_right(buf, g, t_l),
        '\u{2577}' => v_bot(buf, g, t_l),
        '\u{2578}' => h_left(buf, g, t_h),
        '\u{2579}' => v_top(buf, g, t_h),
        '\u{257A}' => h_right(buf, g, t_h),
        '\u{257B}' => v_bot(buf, g, t_h),

        // Mixed light/heavy joins
        '\u{257C}' => { h_left(buf, g, t_l); h_right(buf, g, t_h); }
        '\u{257D}' => { v_top(buf, g, t_l); v_bot(buf, g, t_h); }
        '\u{257E}' => { h_left(buf, g, t_h); h_right(buf, g, t_l); }
        '\u{257F}' => { v_top(buf, g, t_h); v_bot(buf, g, t_l); }

        _ => {}
    }
}

fn double_horizontal(buf: &mut Bitmap, g: &Geom) {
    let (h_top, _, h_bot, _) = double_h_bands(g);
    let t = g.t_light;
    fill_rect(buf, 0, h_top, g.cell_w, h_top + t);
    fill_rect(buf, 0, h_bot, g.cell_w, h_bot + t);
}

fn double_vertical(buf: &mut Bitmap, g: &Geom) {
    let (vl0, vl1, vr0, vr1) = double_v_bands(g);
    fill_rect(buf, vl0, 0, vl1, g.cell_h);
    fill_rect(buf, vr0, 0, vr1, g.cell_h);
}

// ╔ / ╗ / ╚ / ╝ — full double corner.
// `right`: horizontal extends to right cell edge.  `down`: vertical extends down.
// We build it from a frame: outer L plus inner L offset by (2 * t_light).
fn double_corner(buf: &mut Bitmap, g: &Geom, right: bool, down: bool) {
    let t = g.t_light;
    let (h_top, _, h_bot, _) = double_h_bands(g);
    let (vl0, vl1, vr0, vr1) = double_v_bands(g);
    if down {
        if right {
            // ╔
            fill_rect(buf, vl0, h_top, g.cell_w, h_top + t);
            fill_rect(buf, vr1, h_bot, g.cell_w, h_bot + t);
            fill_rect(buf, vl0, h_top, vl1, g.cell_h);
            fill_rect(buf, vr0, h_bot + t, vr1, g.cell_h);
        } else {
            // ╗
            fill_rect(buf, 0, h_top, vr1, h_top + t);
            fill_rect(buf, 0, h_bot, vl0, h_bot + t);
            fill_rect(buf, vr0, h_top, vr1, g.cell_h);
            fill_rect(buf, vl0, h_bot + t, vl1, g.cell_h);
        }
    } else if right {
        // ╚
        fill_rect(buf, vl0, h_bot, g.cell_w, h_bot + t);
        fill_rect(buf, vr1, h_top, g.cell_w, h_top + t);
        fill_rect(buf, vl0, 0, vl1, h_bot + t);
        fill_rect(buf, vr0, 0, vr1, h_top);
    } else {
        // ╝
        fill_rect(buf, 0, h_bot, vr1, h_bot + t);
        fill_rect(buf, 0, h_top, vl0, h_top + t);
        fill_rect(buf, vr0, 0, vr1, h_bot + t);
        fill_rect(buf, vl0, 0, vl1, h_top);
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Block elements
// ─────────────────────────────────────────────────────────────────────────

fn draw_block(buf: &mut Bitmap, g: &Geom, ch: char) {
    let cw = g.cell_w;
    let ch_h = g.cell_h;
    // Helper: paint top-N-eighths or bottom-N-eighths.
    let frac_top = |buf: &mut Bitmap, eighths: usize| {
        let h = (ch_h * eighths + 4) / 8; // round
        fill_rect(buf, 0, 0, cw, h);
    };
    let frac_bot = |buf: &mut Bitmap, eighths: usize| {
        let h = (ch_h * eighths + 4) / 8;
        fill_rect(buf, 0, ch_h.saturating_sub(h), cw, ch_h);
    };
    let frac_left = |buf: &mut Bitmap, eighths: usize| {
        let w = (cw * eighths + 4) / 8;
        fill_rect(buf, 0, 0, w, ch_h);
    };
    let frac_right = |buf: &mut Bitmap, eighths: usize| {
        let w = (cw * eighths + 4) / 8;
        fill_rect(buf, cw.saturating_sub(w), 0, cw, ch_h);
    };
    match ch {
        '\u{2580}' => frac_top(buf, 4),    // ▀ upper half
        '\u{2581}' => frac_bot(buf, 1),    // ▁
        '\u{2582}' => frac_bot(buf, 2),
        '\u{2583}' => frac_bot(buf, 3),
        '\u{2584}' => frac_bot(buf, 4),    // ▄
        '\u{2585}' => frac_bot(buf, 5),
        '\u{2586}' => frac_bot(buf, 6),
        '\u{2587}' => frac_bot(buf, 7),
        '\u{2588}' => fill_rect(buf, 0, 0, cw, ch_h), // █
        '\u{2589}' => frac_left(buf, 7),
        '\u{258A}' => frac_left(buf, 6),
        '\u{258B}' => frac_left(buf, 5),
        '\u{258C}' => frac_left(buf, 4),   // ▌
        '\u{258D}' => frac_left(buf, 3),
        '\u{258E}' => frac_left(buf, 2),
        '\u{258F}' => frac_left(buf, 1),
        '\u{2590}' => frac_right(buf, 4),  // ▐
        // Shaded blocks: regular dither so the optical density matches the
        // intent (light=25%, medium=50%, dark=75%).
        '\u{2591}' => shade(buf, cw, ch_h, 1, 4),
        '\u{2592}' => shade(buf, cw, ch_h, 1, 2),
        '\u{2593}' => shade(buf, cw, ch_h, 3, 4),
        '\u{2594}' => frac_top(buf, 1),    // ▔
        '\u{2595}' => frac_right(buf, 1),  // ▕
        // Quadrants
        '\u{2596}' => quadrants(buf, cw, ch_h, 0b0010),
        '\u{2597}' => quadrants(buf, cw, ch_h, 0b0001),
        '\u{2598}' => quadrants(buf, cw, ch_h, 0b1000),
        '\u{2599}' => quadrants(buf, cw, ch_h, 0b1011),
        '\u{259A}' => quadrants(buf, cw, ch_h, 0b1001),
        '\u{259B}' => quadrants(buf, cw, ch_h, 0b1110),
        '\u{259C}' => quadrants(buf, cw, ch_h, 0b1101),
        '\u{259D}' => quadrants(buf, cw, ch_h, 0b0100),
        '\u{259E}' => quadrants(buf, cw, ch_h, 0b0110),
        '\u{259F}' => quadrants(buf, cw, ch_h, 0b0111),
        _ => {}
    }
}

// quad mask: bits TL=8, TR=4, BL=2, BR=1
fn quadrants(buf: &mut Bitmap, cw: usize, ch: usize, mask: u8) {
    let mx = cw / 2;
    let my = ch / 2;
    if mask & 0b1000 != 0 { fill_rect(buf, 0, 0, mx, my); }
    if mask & 0b0100 != 0 { fill_rect(buf, mx, 0, cw, my); }
    if mask & 0b0010 != 0 { fill_rect(buf, 0, my, mx, ch); }
    if mask & 0b0001 != 0 { fill_rect(buf, mx, my, cw, ch); }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col_alpha(b: &Bitmap, x: usize, y: usize) -> u8 {
        b.data[y * b.width + x]
    }

    const SIZES: &[(usize, usize)] = &[(8, 16), (10, 18), (14, 30), (12, 24), (9, 21)];

    fn synth_at(ch: char, w: usize, h: usize) -> Bitmap {
        synth(ch, w, h, h as isize).unwrap()
    }

    // Number of "on" pixels in row `y` across all columns.
    fn row_on_count(b: &Bitmap, y: usize) -> usize {
        (0..b.width).filter(|&x| col_alpha(b, x, y) > 0).count()
    }

    // Number of "on" pixels in column `x` across all rows.
    fn col_on_count(b: &Bitmap, x: usize) -> usize {
        (0..b.height).filter(|&y| col_alpha(b, x, y) > 0).count()
    }

    // Set of columns that are "on" in the center row — i.e. the horizontal
    // span of a vertical stroke at mid-height (the stroke's width in columns).
    fn stroke_cols_at_center(b: &Bitmap) -> Vec<usize> {
        let y = b.height / 2;
        (0..b.width).filter(|&x| col_alpha(b, x, y) > 0).collect()
    }

    // Set of rows that are "on" in the center column — the vertical span of a
    // horizontal stroke at mid-width (the stroke's height in rows).
    fn stroke_rows_at_center(b: &Bitmap) -> Vec<usize> {
        let x = b.width / 2;
        (0..b.height).filter(|&y| col_alpha(b, x, y) > 0).collect()
    }

    // The whole point: ╭ on row N must put its vertical leg at the same
    // column(s) as ╰ on row N+1 and as │, so the strokes join across cells.
    #[test]
    fn arc_legs_align_with_vertical() {
        for &(w, h) in &[(8, 16), (10, 18), (14, 30), (12, 24)] {
            let v = synth('│', w, h, h as isize).unwrap();
            let arc_top = synth('╭', w, h, h as isize).unwrap();
            let arc_bot = synth('╰', w, h, h as isize).unwrap();
            let bot_row = h - 1;
            for x in 0..w {
                // │'s column profile at any row equals ╭'s at bottom and ╰'s at top.
                assert_eq!(
                    col_alpha(&v, x, bot_row) > 0,
                    col_alpha(&arc_top, x, bot_row) > 0,
                    "╭/│ leg mismatch w={} h={} x={}",
                    w, h, x
                );
                assert_eq!(
                    col_alpha(&v, x, 0) > 0,
                    col_alpha(&arc_bot, x, 0) > 0,
                    "╰/│ leg mismatch w={} h={} x={}",
                    w, h, x
                );
            }
        }
    }

    // ─ on column C must match ─ on column C+1 at the same row(s) so a run
    // of horizontals reads as a single line.
    #[test]
    fn horizontal_strokes_align() {
        let h_line = synth('─', 12, 24, 24).unwrap();
        // Every column should have an identical row profile.
        for y in 0..24 {
            let first = col_alpha(&h_line, 0, y);
            for x in 1..12 {
                assert_eq!(col_alpha(&h_line, x, y), first, "row {} not uniform", y);
            }
        }
    }

    #[test]
    fn full_block_is_full() {
        let b = synth('█', 10, 20, 20).unwrap();
        assert!(b.data.iter().all(|&v| v == 255));
    }

    #[test]
    fn synth_returns_none_outside_range() {
        assert!(synth('A', 10, 20, 20).is_none());
        assert!(synth('字', 10, 20, 20).is_none());
    }

    // ── Heavy vs light stroke weight ───────────────────────────────────────

    // Invariant: ┃ (heavy vertical) occupies strictly more stroke columns than
    // │ (light vertical) at mid-height; both share the canonical center column.
    #[test]
    fn heavy_vertical_thicker_than_light() {
        for &(w, h) in SIZES {
            let light = synth_at('│', w, h);
            let heavy = synth_at('┃', w, h);
            let lc = stroke_cols_at_center(&light);
            let hc = stroke_cols_at_center(&heavy);
            assert!(!lc.is_empty() && !hc.is_empty(), "empty stroke w={} h={}", w, h);
            assert!(
                hc.len() > lc.len(),
                "┃ not thicker than │: heavy={} light={} w={} h={}",
                hc.len(), lc.len(), w, h
            );
            // Both centered on the same canonical axis: the heavy stroke's
            // column set must contain the light stroke's center column.
            let light_center = lc[lc.len() / 2];
            assert!(
                hc.contains(&light_center),
                "┃ not centered on │ axis w={} h={}", w, h
            );
        }
    }

    // Invariant: ━ (heavy horizontal) occupies strictly more stroke rows than
    // ─ (light horizontal) at mid-width; both share the canonical center row.
    #[test]
    fn heavy_horizontal_taller_than_light() {
        for &(w, h) in SIZES {
            let light = synth_at('─', w, h);
            let heavy = synth_at('━', w, h);
            let lr = stroke_rows_at_center(&light);
            let hr = stroke_rows_at_center(&heavy);
            assert!(!lr.is_empty() && !hr.is_empty(), "empty stroke w={} h={}", w, h);
            assert!(
                hr.len() > lr.len(),
                "━ not taller than ─: heavy={} light={} w={} h={}",
                hr.len(), lr.len(), w, h
            );
            let light_center = lr[lr.len() / 2];
            assert!(
                hr.contains(&light_center),
                "━ not centered on ─ axis w={} h={}", w, h
            );
        }
    }

    // ── Tee / cross junction alignment ─────────────────────────────────────

    // Invariant: the through-vertical of ├ and ┤ occupies exactly the same
    // columns as │, and its horizontal arm occupies the same rows as ─, so
    // junctions join seamlessly with straight neighbours across cells.
    #[test]
    fn vertical_tees_align_with_straights() {
        for &(w, h) in SIZES {
            let v = synth_at('│', w, h);
            let hbar = synth_at('─', w, h);
            for &tee in &['├', '┤'] {
                let t = synth_at(tee, w, h);
                // Vertical leg: identical column profile to │ at top and bottom rows.
                for &row in &[0usize, h - 1] {
                    for x in 0..w {
                        assert_eq!(
                            col_alpha(&v, x, row) > 0,
                            col_alpha(&t, x, row) > 0,
                            "{} vertical-leg mismatch vs │ w={} h={} x={} row={}",
                            tee, w, h, x, row
                        );
                    }
                }
                // Horizontal arm: matches ─'s row span at the cell edge it reaches.
                let edge = if tee == '├' { w - 1 } else { 0 };
                for y in 0..h {
                    assert_eq!(
                        col_alpha(&hbar, edge, y) > 0,
                        col_alpha(&t, edge, y) > 0,
                        "{} horizontal-arm mismatch vs ─ w={} h={} y={}",
                        tee, w, h, y
                    );
                }
            }
        }
    }

    // Invariant: the through-horizontal of ┬ and ┴ matches ─ across the full
    // width, and its vertical leg matches │'s columns at the edge it reaches.
    #[test]
    fn horizontal_tees_align_with_straights() {
        for &(w, h) in SIZES {
            let v = synth_at('│', w, h);
            let hbar = synth_at('─', w, h);
            for &tee in &['┬', '┴'] {
                let t = synth_at(tee, w, h);
                // Horizontal bar: identical row profile to ─ at left and right edges.
                for &col in &[0usize, w - 1] {
                    for y in 0..h {
                        assert_eq!(
                            col_alpha(&hbar, col, y) > 0,
                            col_alpha(&t, col, y) > 0,
                            "{} horizontal-bar mismatch vs ─ w={} h={} y={} col={}",
                            tee, w, h, y, col
                        );
                    }
                }
                // Vertical leg: matches │'s column span at the edge it reaches.
                let edge = if tee == '┬' { h - 1 } else { 0 };
                for x in 0..w {
                    assert_eq!(
                        col_alpha(&v, x, edge) > 0,
                        col_alpha(&t, x, edge) > 0,
                        "{} vertical-leg mismatch vs │ w={} h={} x={}",
                        tee, w, h, x
                    );
                }
            }
        }
    }

    // Invariant: the cross ┼ reaches all four edges, with its vertical legs
    // matching │ and its horizontal arms matching ─ at every edge.
    #[test]
    fn cross_aligns_with_straights_on_all_edges() {
        for &(w, h) in SIZES {
            let v = synth_at('│', w, h);
            let hbar = synth_at('─', w, h);
            let cross = synth_at('┼', w, h);
            // Vertical legs at top and bottom rows match │.
            for &row in &[0usize, h - 1] {
                for x in 0..w {
                    assert_eq!(
                        col_alpha(&v, x, row) > 0,
                        col_alpha(&cross, x, row) > 0,
                        "┼ vertical mismatch vs │ w={} h={} x={} row={}",
                        w, h, x, row
                    );
                }
            }
            // Horizontal arms at left and right columns match ─.
            for &col in &[0usize, w - 1] {
                for y in 0..h {
                    assert_eq!(
                        col_alpha(&hbar, col, y) > 0,
                        col_alpha(&cross, col, y) > 0,
                        "┼ horizontal mismatch vs ─ w={} h={} y={} col={}",
                        w, h, y, col
                    );
                }
            }
        }
    }

    // ── Light corners ──────────────────────────────────────────────────────

    // Invariant: each light corner reaches exactly its two expected edges
    // (matching the corresponding straight stroke) and leaves the other two
    // edges blank, mirroring the arc-corner connectivity already tested.
    #[test]
    fn light_corners_reach_correct_two_edges() {
        // (char, reaches_left, reaches_right, reaches_up, reaches_down)
        let cases = [
            ('┌', false, true, false, true),  // h_right + v_bot
            ('┐', true, false, false, true),  // h_left  + v_bot
            ('└', false, true, true, false),  // h_right + v_top
            ('┘', true, false, true, false),  // h_left  + v_top
        ];
        for &(w, h) in SIZES {
            let cy = h / 2;
            let cx = w / 2;
            for &(ch, left, right, up, down) in &cases {
                let b = synth_at(ch, w, h);
                // A leg "reaches" an edge iff that edge has an on-pixel on the
                // canonical center axis.
                let has_left = col_alpha(&b, 0, cy) > 0;
                let has_right = col_alpha(&b, w - 1, cy) > 0;
                let has_up = col_alpha(&b, cx, 0) > 0;
                let has_down = col_alpha(&b, cx, h - 1) > 0;
                assert_eq!(has_left, left, "{} left-edge w={} h={}", ch, w, h);
                assert_eq!(has_right, right, "{} right-edge w={} h={}", ch, w, h);
                assert_eq!(has_up, up, "{} top-edge w={} h={}", ch, w, h);
                assert_eq!(has_down, down, "{} bottom-edge w={} h={}", ch, w, h);
                // Exactly two legs present.
                let legs = [left, right, up, down].iter().filter(|&&b| b).count();
                assert_eq!(legs, 2, "{} should have exactly 2 legs", ch);
            }
        }
    }

    // Invariant: a corner's horizontal arm shares ─'s rows and its vertical
    // leg shares │'s columns, so corners join straights seamlessly.
    #[test]
    fn light_corner_legs_match_straight_profiles() {
        for &(w, h) in SIZES {
            let v = synth_at('│', w, h);
            let hbar = synth_at('─', w, h);
            // ┌: right arm matches ─ at right edge; down leg matches │ at bottom.
            let tl = synth_at('┌', w, h);
            for y in 0..h {
                assert_eq!(
                    col_alpha(&hbar, w - 1, y) > 0,
                    col_alpha(&tl, w - 1, y) > 0,
                    "┌ right-arm row mismatch vs ─ w={} h={} y={}", w, h, y
                );
            }
            for x in 0..w {
                assert_eq!(
                    col_alpha(&v, x, h - 1) > 0,
                    col_alpha(&tl, x, h - 1) > 0,
                    "┌ down-leg col mismatch vs │ w={} h={} x={}", w, h, x
                );
            }
            // ┘: left arm matches ─ at left edge; up leg matches │ at top.
            let br = synth_at('┘', w, h);
            for y in 0..h {
                assert_eq!(
                    col_alpha(&hbar, 0, y) > 0,
                    col_alpha(&br, 0, y) > 0,
                    "┘ left-arm row mismatch vs ─ w={} h={} y={}", w, h, y
                );
            }
            for x in 0..w {
                assert_eq!(
                    col_alpha(&v, x, 0) > 0,
                    col_alpha(&br, x, 0) > 0,
                    "┘ up-leg col mismatch vs │ w={} h={} x={}", w, h, x
                );
            }
        }
    }

    // ── Heavy junctions ────────────────────────────────────────────────────

    // Invariant: the heavy cross ╋ has both a thicker vertical leg than ┼ and
    // a thicker horizontal arm, confirming heavy weight propagates to junctions.
    #[test]
    fn heavy_cross_thicker_than_light_cross() {
        for &(w, h) in SIZES {
            let light = synth_at('┼', w, h);
            let heavy = synth_at('╋', w, h);
            // Vertical-leg thickness sampled at the top edge row (away from the
            // center, where the horizontal arm would mask the leg width).
            assert!(
                row_on_count(&heavy, 0) > row_on_count(&light, 0),
                "╋ top-leg not thicker than ┼ w={} h={}", w, h
            );
            // Horizontal-arm thickness sampled at the left edge column.
            assert!(
                col_on_count(&heavy, 0) > col_on_count(&light, 0),
                "╋ left-arm not thicker than ┼ w={} h={}", w, h
            );
        }
    }

    // ── Double lines ───────────────────────────────────────────────────────

    // Invariant: ═ (double horizontal) renders as exactly two separated
    // horizontal bands at the center column — two on-runs split by a gap —
    // unlike the single band of ─.
    #[test]
    fn double_horizontal_has_two_bands() {
        for &(w, h) in SIZES {
            let d = synth_at('═', w, h);
            let rows = stroke_rows_at_center(&d);
            assert!(rows.len() >= 2, "═ too few on-rows w={} h={}", w, h);
            // Count contiguous runs of on-rows; double line ⇒ exactly 2.
            let mut runs = 1;
            for win in rows.windows(2) {
                if win[1] != win[0] + 1 {
                    runs += 1;
                }
            }
            assert_eq!(runs, 2, "═ should have 2 bands, got {} w={} h={}", runs, w, h);
        }
    }

    // Invariant: ║ (double vertical) renders as exactly two separated vertical
    // bands at the center row.
    #[test]
    fn double_vertical_has_two_bands() {
        for &(w, h) in SIZES {
            let d = synth_at('║', w, h);
            let cols = stroke_cols_at_center(&d);
            assert!(cols.len() >= 2, "║ too few on-cols w={} h={}", w, h);
            let mut runs = 1;
            for win in cols.windows(2) {
                if win[1] != win[0] + 1 {
                    runs += 1;
                }
            }
            assert_eq!(runs, 2, "║ should have 2 bands, got {} w={} h={}", runs, w, h);
        }
    }

    // ── Dashed lines (documented approximation) ────────────────────────────

    // Per the module header, dashed straights are drawn SOLID, identical to
    // their non-dashed light counterpart. Assert that approximation holds so a
    // future change to real dashing is caught.
    #[test]
    fn dashed_horizontal_drawn_solid_like_light() {
        for &(w, h) in SIZES {
            let solid = synth_at('─', w, h);
            for &dashed in &['┄', '┈', '╌'] {
                let d = synth_at(dashed, w, h);
                assert_eq!(d.data, solid.data, "{} not solid like ─ w={} h={}", dashed, w, h);
            }
        }
    }
}

// 2x2 ordered dither giving `num/den` fill density on a regular grid.
fn shade(buf: &mut Bitmap, cw: usize, ch: usize, num: usize, den: usize) {
    // 4x4 Bayer-ish thresholds.
    const M: [[u8; 4]; 4] = [
        [ 0,  8,  2, 10],
        [12,  4, 14,  6],
        [ 3, 11,  1,  9],
        [15,  7, 13,  5],
    ];
    let threshold = (num * 16 / den) as u8;
    for y in 0..ch {
        for x in 0..cw {
            if M[y % 4][x % 4] < threshold {
                buf.data[y * cw + x] = 255;
            }
        }
    }
}
