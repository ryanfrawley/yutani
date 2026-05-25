//! `WindowState` rendering: building the per-frame vertex/index buffers
//! (`update_vertices`) and the wgpu pass orchestration (`render`), plus the
//! glyph/half-block UV and edge-fade helpers they rely on.

use crate::*;

impl WindowState {
    // Rebuild the vertex/index buffers for the current terminal state. Emits
    // one bg quad + one glyph quad per cell for the grid, plus a cursor box
    // and the top/bottom edge fades.
    pub(crate) fn update_vertices(&mut self) {
        // Advance any in-flight scroll slide first so this frame reads the
        // freshly-eased `scroll_y` (alt-screen and primary are mutually
        // exclusive — one screen is active at a time).
        self.update_alt_scroll();
        self.update_primary_scroll();
        let cols = self.active_tab().terminal.cols;
        let rows = self.active_tab().terminal.rows;
        let area = cols * rows;
        let mut vertices: Vec<renderer::vertex::Vertex> = Vec::with_capacity(8 * (area + 1));
        let mut indices: Vec<u32> = Vec::with_capacity(12 * (area + 1));

        let theme = self.window.theme().unwrap_or(winit::window::Theme::Light);
        // All face-derived metrics are pulled in one borrow so the shared
        // font's `Ref` is dropped before the `ensure_*` fill calls below
        // (which take `&mut Font`) — see the `AppShared::font` borrow rule.
        let (line_height, cell_w, bg_h, descender, underline_thickness_px, underline_pos_px) =
            self.shared.with_font(|font| {
                let face = font.face();
                let metrics = face.size_metrics().unwrap();
                let line_height = ((metrics.ascender - metrics.descender) >> 6) as f32;
                let cell_w = font.cell_width() as f32;
                let bg_h = ((metrics.ascender - metrics.descender) >> 6) as f32;
                let descender = (metrics.descender >> 6) as f32;
                // Underline metrics from the font's `post` table. The face
                // values are in font design units; `y_scale` (16.16 fixed)
                // converts to 26.6 px for this size, matching how `ascender` /
                // `descender` above land in 26.6 — divide by 64 once for
                // actual pixels.
                //   - `underline_position`: vertical center of the stem, in
                //     font units. Negative ⇒ below the baseline (the usual
                //     case).
                //   - `underline_thickness`: stem height in font units.
                // Kept as floats; the rasterizer can render a sub-pixel quad
                // across two rows of fragments which reads as a softer-than-1px
                // line and lets the stripe grow smoothly with point size.
                // Fallbacks cover fonts whose `post` table is empty (some
                // bitmap-style monospace TTFs report 0).
                let y_scale = metrics.y_scale as f32 / 65536.0;
                let raw_thick_px = face.underline_thickness() as f32 * y_scale / 64.0;
                let underline_thickness_px = if raw_thick_px > 0.0 {
                    raw_thick_px
                } else {
                    line_height * 0.06
                };
                let raw_pos_px = face.underline_position() as f32 * y_scale / 64.0;
                let underline_pos_px = if face.underline_position() != 0 {
                    raw_pos_px
                } else {
                    descender * 0.5
                };
                (
                    line_height,
                    cell_w,
                    bg_h,
                    descender,
                    underline_thickness_px,
                    underline_pos_px,
                )
            });

        let pal = palette::get();
        let default_fg = pal.foreground;
        // `default_bg` stays fully transparent so the window can show
        // through cells with no SGR background; `default_bg_solid` is the
        // concrete window background, used when reverse-video needs to
        // swap a real color into the foreground slot.
        let default_bg = [0.0, 0.0, 0.0, 0.0];
        let default_bg_solid = pal.background;
        let atlas_w = self.atlas.width as f32;
        let atlas_h = self.atlas.height as f32;
        let bg_u = 1.0 / atlas_w;
        let bg_v = 1.0 / atlas_h;
        let scroll_y = self.active_tab().scroll_y as f32;

        let push_quad =
            |verts: &mut Vec<renderer::vertex::Vertex>,
             idxs: &mut Vec<u32>,
             x: f32,
             y: f32,
             w: f32,
             h: f32,
             uv0: [f32; 2],
             uv1: [f32; 2],
             color: [f32; 4],
             radii: [f32; 4]| {
                let start = verts.len() as u32;
                let hx = w * 0.5;
                let hy = h * 0.5;
                let half_size = [hx, hy];
                verts.push(renderer::vertex::Vertex {
                    position: [x, y, 0.0],
                    tex_coords: [uv0[0], uv0[1]],
                    color,
                    local_pos: [-hx, -hy],
                    half_size,
                    radii,
                });
                verts.push(renderer::vertex::Vertex {
                    position: [x, y + h, 0.0],
                    tex_coords: [uv0[0], uv1[1]],
                    color,
                    local_pos: [-hx, hy],
                    half_size,
                    radii,
                });
                verts.push(renderer::vertex::Vertex {
                    position: [x + w, y, 0.0],
                    tex_coords: [uv1[0], uv0[1]],
                    color,
                    local_pos: [hx, -hy],
                    half_size,
                    radii,
                });
                verts.push(renderer::vertex::Vertex {
                    position: [x + w, y + h, 0.0],
                    tex_coords: [uv1[0], uv1[1]],
                    color,
                    local_pos: [hx, hy],
                    half_size,
                    radii,
                });
                idxs.extend_from_slice(&[start, start + 1, start + 2, start + 1, start + 2, start + 3]);
            };

        // Grid row `r` sits with its baseline at (r+1) * line_height; the
        // glyph box extends up by bearing_y and down by (height - bearing_y).
        // Push content below the translucent title bar at the boundaries of
        // the scroll range — the bottom of the live grid AND the top of
        // scrollback — so the first/last row never sits half-behind the
        // toolbar. Mid-scroll the offset is 0 so older content can flow
        // behind the title bar smoothly. Eases linearly over one line at
        // each boundary. Hit-test in pixel_to_visual_cell mirrors this.
        let view_offset = self.active_tab().terminal.view_offset() as f32;
        // Alt screen has no scrollback to fade toward — pin both distances
        // to zero so the top/bottom edge fades stay invisible.
        let scrollback_len = if self.active_tab().terminal.on_alt_screen() {
            0.0
        } else {
            self.active_tab().terminal.scrollback_len() as f32
        };
        let (dist_from_bottom, dist_from_top) =
            self.edge_fade_dists(scroll_y, view_offset, scrollback_len, line_height);
        let near = (dist_from_bottom / line_height)
            .min(dist_from_top / line_height)
            .clamp(0.0, 1.0);
        let decorator_offset = DECORATOR_HEIGHT * (1.0 - near);
        let row_y = |r: isize| WINDOW_PADDING + decorator_offset + (r as f32 + 1.0) * line_height;
        let col_x = |c: usize| WINDOW_PADDING + c as f32 * cell_w;

        // Half-open band `[r_lo, r_hi)` of grid rows to render — the visible
        // grid plus phantom rows above and below so smooth sub-line scrolling
        // stays populated through the snap. Used both for shaping (below) and
        // the main emit loop further down. See `phantom_row_band` for the math.
        let (r_lo, r_hi) = Self::phantom_row_band(scroll_y, rows, line_height);

        // Programming-ligature pass. Walks each visible row, prefix-matches
        // each cell against the per-variant ligature table the Shaper
        // pre-built at font load. Mutates atlas (rasterizes ligature
        // glyphs on demand) so it has to run before the emit closure
        // captures &self.atlas immutably below.
        //
        // Fira Code and friends implement ligatures as 1:1 contextual
        // alternates (each char substituted to a half-glyph), not N→1
        // ligature substitutions, so each covered cell still draws at
        // its own column with normal cell width — only the glyph id
        // changes. See `shaper.rs` for the longer story.
        let mut row_overrides: std::collections::HashMap<
            isize,
            Vec<Option<(u32, font::FaceVariant)>>,
        > = std::collections::HashMap::new();
        // Reused across rows — refilled in place to avoid per-row allocation.
        let mut row_chars: Vec<char> = Vec::with_capacity(cols);
        // Borrow the shared shaper once for the whole pass. `match_at` returns
        // a `&Ligature` into it, so the borrow must outlive each match's use;
        // it's disjoint from the `font`/`atlas` fills below (different fields).
        let shaper = self.shared.shaper.borrow();
        for r in r_lo..r_hi {
            row_chars.clear();
            for c in 0..cols {
                let cell = self.active_tab().terminal.extended_cell(r, c);
                let ch = cell.map(|cell| cell.ch).unwrap_or(' ');
                // Pre-pack any char outside build_atlas's fixed ranges
                // (Nerd Font icons in SPUA, CJK, arbitrary symbols) so
                // the render-time lookup below hits the variant chain
                // — including fallback fonts — instead of notdef.
                if let Some(cell) = cell {
                    let variant = font::FaceVariant::from_flags(
                        cell.style.bold,
                        cell.style.italic,
                    );
                    self.atlas.ensure_char(&mut *self.shared.font.borrow_mut(), variant, ch);
                }
                row_chars.push(ch);
            }
            let mut row_override: Option<Vec<Option<(u32, font::FaceVariant)>>> = None;
            let mut c = 0;
            while c < cols {
                let Some(start_cell) = self.active_tab().terminal.extended_cell(r, c) else {
                    c += 1;
                    continue;
                };
                let variant =
                    font::FaceVariant::from_flags(start_cell.style.bold, start_cell.style.italic);
                let lig = match shaper.match_at(&row_chars[c..], variant) {
                    Some(l) => l,
                    None => {
                        c += 1;
                        continue;
                    }
                };
                let span = lig.chars.len();
                // All cells in the ligature must share the start cell's
                // style — a colored or weight-changing split breaks the
                // visual cohesion that contextual-alternate halves rely on.
                let style_uniform = (1..span).all(|i| {
                    self.active_tab().terminal
                        .extended_cell(r, c + i)
                        .map(|cell| cell.style == start_cell.style)
                        .unwrap_or(false)
                });
                if !style_uniform {
                    c += 1;
                    continue;
                }
                // Rasterize every output glyph into the atlas so the
                // override lookup at render time is a hit. If any one
                // glyph fails to load, abandon the substitution for this
                // span (better to render the chars than render half a
                // ligature).
                let all_ok = lig.output_glyphs.iter().all(|gid| {
                    self.atlas.ensure_glyph_id(&mut *self.shared.font.borrow_mut(), variant, *gid)
                });
                if !all_ok {
                    c += 1;
                    continue;
                }
                let over = row_override.get_or_insert_with(|| (0..cols).map(|_| None).collect());
                for (i, gid) in lig.output_glyphs.iter().enumerate() {
                    if c + i < cols {
                        over[c + i] = Some((*gid, variant));
                    }
                }
                c += span;
            }
            if let Some(over) = row_override {
                row_overrides.insert(r, over);
            }
        }

        // Rasterize any glyphs the completion popup will draw that the grid
        // didn't already pack this frame, so the immutable-`atlas` lookup in
        // `emit_text_run` below is a hit (and the dirty flag triggers the
        // re-upload right after). Done here, before the `&self.atlas` borrow,
        // because `ensure_char` needs `&mut self.atlas` and a `&mut Font`
        // (taken as a single-statement `borrow_mut` per the AppShared rule).
        if !self.active_tab().completions.is_empty() {
            let chars: Vec<char> = self.active_tab()
                .completions
                .iter()
                .flat_map(|s| s.text.chars())
                .chain(std::iter::once('…'))
                .collect();
            for ch in chars {
                self.atlas
                    .ensure_char(&mut *self.shared.font.borrow_mut(), font::FaceVariant::Regular, ch);
            }
        }

        // Re-upload the atlas texture if the shaping pass rasterized any
        // new glyphs. write_texture reuses the existing GPU texture and
        // bind group — no need to recreate either.
        if self.atlas.dirty {
            self.shared.gpu.queue.write_texture(
                wgpu::ImageCopyTexture {
                    texture: &self.font_texture.texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                &self.atlas.buffer,
                wgpu::ImageDataLayout {
                    offset: 0,
                    bytes_per_row: Some(self.atlas.width as u32),
                    rows_per_image: Some(self.atlas.height as u32),
                },
                wgpu::Extent3d {
                    width: self.atlas.width as u32,
                    height: self.atlas.height as u32,
                    depth_or_array_layers: 1,
                },
            );
            self.atlas.dirty = false;
        }

        let atlas = &self.atlas;
        // Foreground glyph source for a cell: a single char (existing
        // per-char path) or a font-internal glyph id (contextual
        // alternate from a programming ligature). Both render at the
        // cell's own column with normal cell width — Fira Code's
        // ligatures are per-cell substitutions, not wide N→1 glyphs.
        #[derive(Copy, Clone)]
        enum GlyphSource {
            Char(char),
            Substituted(u32),
        }
        // BG quad only — used by the bg-layer pass. Pulled out so we can
        // emit all cell backgrounds contiguously, record the boundary in
        // `num_bg_indices`, then emit all foreground content (glyphs,
        // cursor, overlays) after. The renderer issues two draw_indexed
        // calls against the resulting buffer so the glow pipeline can
        // bloom each layer independently.
        let strip_pad = (line_height - bg_h) * 0.5;

        // Alt-screen scroll slide: the offset applies only to rows inside the
        // moving span (the scroll region plus the departing band on the moving
        // edge); rows outside it — a reserved status line below the region —
        // stay put. Off the slide (scrollback smooth-scroll on the primary),
        // every row shares the global `scroll_y`. `clip_bottom_px` keeps a
        // moving row from drawing past the region's bottom edge, so incoming /
        // departing content slides *under* the static status line instead of
        // bleeding glyphs over it; for a full-height region it sits below the
        // window and clips nothing.
        let (anim_lo, anim_hi, clip_bottom_px) = match &self.active_tab().alt_scroll_anim {
            Some(a) => {
                let d = a.rows as isize;
                // Up-scroll departing rows sit above the region top (off-grid
                // when the region is anchored at row 0, which is the only case
                // we animate). Down-scroll departing rows sit below the region
                // bottom — include them in the moving span only when that's
                // off-grid (full-height region); otherwise they coincide with a
                // static status line that must not move.
                let lo = a.region_top as isize - if a.up { d } else { 0 };
                let hi = a.region_bottom as isize
                    + if !a.up && a.region_bottom + 1 == rows { d } else { 0 };
                let clip = row_y(a.region_bottom as isize + 1) - bg_h - descender - strip_pad;
                (lo, hi, clip)
            }
            None => (0, 0, f32::INFINITY),
        };
        let anim_active = self.active_tab().alt_scroll_anim.is_some();
        let row_moving = move |r: isize| anim_active && r >= anim_lo && r <= anim_hi;
        // Per-row vertical offset. When no slide is active this is the global
        // `scroll_y` for every row (unchanged scrollback behavior).
        let row_scroll = move |r: isize| {
            if !anim_active || row_moving(r) {
                scroll_y
            } else {
                0.0
            }
        };
        // Clamp `(y, h, v0, v1)` so a moving row's quad never extends past the
        // region's bottom edge. Returns `None` if fully clipped. `v0`/`v1` are
        // adjusted proportionally so glyph bitmaps clip cleanly (bg quads pass
        // `v0 == v1`, leaving the single sampled texel unchanged).
        let clip_row_quad = move |r: isize, y: f32, h: f32, v0: f32, v1: f32| {
            if !row_moving(r) || y + h <= clip_bottom_px {
                return Some((h, v1));
            }
            let visible = clip_bottom_px - y;
            if visible <= 0.0 {
                return None;
            }
            (Some((visible, v0 + (v1 - v0) * (visible / h)))).filter(|_| h > 0.0)
        };

        let emit_bg_for_cell = |verts: &mut Vec<renderer::vertex::Vertex>,
                                idxs: &mut Vec<u32>,
                                r: isize,
                                c: usize,
                                bg: [f32; 4]| {
            let x = col_x(c);
            let baseline_y = row_y(r);
            // Background quad spans one line-height strip, centered on the
            // typographic glyph extent. Centering matters when line_height
            // differs from (ascender − descender): top-anchoring would float
            // glyphs to the bottom of the strip on tall-line fonts, while
            // anchoring to the glyph extent risks overlap on tight-line ones.
            // Strip stride = line_height, so adjacent rows still tile cleanly.
            let bg_y = baseline_y - bg_h - descender - strip_pad + row_scroll(r);
            let Some((bg_height, _)) = clip_row_quad(r, bg_y, line_height, bg_v, bg_v) else {
                return;
            };
            push_quad(
                verts,
                idxs,
                x,
                bg_y,
                cell_w,
                bg_height,
                [bg_u, bg_v],
                [bg_u, bg_v],
                bg,
                [0.0; 4],
            );
        };

        // FG quad only — glyph for the cell. See `emit_bg_for_cell` above
        // for why bg/fg are split.
        let emit_fg_for_cell = |verts: &mut Vec<renderer::vertex::Vertex>,
                                idxs: &mut Vec<u32>,
                                fg_source: GlyphSource,
                                variant: font::FaceVariant,
                                r: isize,
                                c: usize,
                                fg: [f32; 4]| {
            let x = col_x(c);
            let baseline_y = row_y(r);
            let off = row_scroll(r);
            let bg_y = baseline_y - bg_h - descender - strip_pad + off;
            // Foreground glyph. The per-cell substitution case (Fira
            // Code-style contextual alternates) deliberately uses
            // glyphs whose side bearings extend past the cell edges so
            // adjacent halves visually fuse. The normal-char path's
            // fills_h UV-clipping (added for box-drawing) cuts off
            // exactly that overlap, so we disable it for substituted
            // glyphs.
            let (g, allow_overhang) = match fg_source {
                GlyphSource::Char(ch) => (atlas.lookup(ch, variant), false),
                GlyphSource::Substituted(glyph_id) => {
                    (atlas.lookup_glyph_id(glyph_id, variant), true)
                }
            };
            let span_w = cell_w;
            if g.width > 0 && g.height > 0 {
                // Cell-filling glyphs (Powerline caps, box-drawing,
                // half-blocks) get the affected axis stretched to the cell's
                // full extent. The rasterized bitmap can be a pixel shorter
                // than the typographic cell on a filling axis — drawing the
                // quad at cell extent there and letting the linear-filtered
                // sampler stretch the bitmap into it closes the gap. Each
                // axis is independent so e.g. ▐ (full-height, half-width)
                // gets vertical stretching without distorting horizontally.
                //
                // Gated on codepoint range so a generic glyph that happens to
                // fill both axes (e.g. ⏺ U+23FA, a near-square circle) isn't
                // stretched to the non-square cell aspect — that distortion
                // turns a round glyph into an oval. Only the ranges whose
                // glyphs are *designed* to tile across cell edges opt in:
                // box-drawing + block-elements (synthesized in this binary)
                // and the Powerline/separator slice of PUA.
                let bx = g.bearing_x as f32;
                let by = g.bearing_y as f32;
                let asc_eff = bg_h + descender; // pixels above baseline (descender is negative)
                let cell_filling = match fg_source {
                    GlyphSource::Char(ch) => {
                        let cp = ch as u32;
                        (0x2500..=0x259F).contains(&cp) || (0xE000..=0xE0FF).contains(&cp)
                    }
                    GlyphSource::Substituted(_) => false,
                };
                let fills_h = cell_filling && !allow_overhang
                    && g.width as f32 >= span_w * 0.85;
                let fills_v = cell_filling && g.height as f32 >= line_height * 0.85;
                let (gx, gw, q_start, q_end) = if fills_h {
                    // Restrict UV to the in-cell columns so a glyph designed
                    // to bleed into an adjacent cell (negative bearing or
                    // bitmap_width > cell_w) doesn't put its transparent
                    // overhang at the cell's left/right edge.
                    let q_start = (-bx).max(0.0).min(g.width as f32);
                    let q_end = (span_w - bx).max(0.0).min(g.width as f32);
                    (x, span_w, q_start, q_end)
                } else {
                    (x + bx, g.width as f32, 0.0, g.width as f32)
                };
                let (gy, gh, p_start, p_end) = if fills_v {
                    let p_start = (by - asc_eff).max(0.0).min(g.height as f32);
                    let p_end = (by - descender).max(0.0).min(g.height as f32);
                    (bg_y, line_height, p_start, p_end)
                } else {
                    (
                        baseline_y - by + off,
                        g.height as f32,
                        0.0,
                        g.height as f32,
                    )
                };
                // Half-texel inset on a stretched (cell-filling) axis. The
                // glyph is packed with one transparent column/row of padding
                // (`stride = w + 1` in font.rs), so sampling right up to the
                // texel boundary `g.x + g.width` makes the Linear filter
                // average the opaque edge with that transparent neighbor —
                // ~50% alpha along the seam, which reads as a hairline gap
                // between abutting blocks (and varies with the bitmap→cell
                // stretch ratio, hence "only at certain font sizes"). Pulling
                // the UV in by half a texel keeps every edge fragment on a
                // fully-opaque texel center. Only the filling axis is inset:
                // the non-filling axis is placed 1:1 and must keep its true
                // extent so normal glyphs aren't thinned.
                let (u0, v0, u1, v1) = Self::glyph_quad_uv(
                    g.x as f32,
                    g.y as f32,
                    (q_start, q_end),
                    (p_start, p_end),
                    cell_filling,
                    atlas_w,
                    atlas_h,
                );
                // Clip a moving glyph at the region's bottom edge so it slides
                // under the static status line rather than over it.
                let Some((gh, v1)) = clip_row_quad(r, gy, gh, v0, v1) else {
                    return;
                };
                push_quad(
                    verts,
                    idxs,
                    gx,
                    gy,
                    gw,
                    gh,
                    [u0, v0],
                    [u1, v1],
                    fg,
                    [0.0; 4],
                );
            }
        };

        // Selection highlight: translucent macOS text-selection blue, drawn
        // as an overlay on top of cells. Uses premultiplied alpha so RGB is
        // pre-scaled by alpha.
        let selection_alpha: f32 = match theme {
            winit::window::Theme::Light => 0.30,
            winit::window::Theme::Dark => 0.35,
        };
        let sel = palette::get().selection;
        let selection_bg = [
            sel[0] * selection_alpha,
            sel[1] * selection_alpha,
            sel[2] * selection_alpha,
            selection_alpha,
        ];
        let selection = self.active_tab().selection;

        // 0b. Half-block fallback overrides for image placements that
        // the GPU image pipeline can't draw this frame (config-disabled
        // or decode-failed). One entry per affected cell holds the
        // top/bottom half-pixel colors for a U+2580 ▀ glyph. Built only
        // when `images_halfblock_for_missing` is on; empty otherwise so
        // the lookup below is a single hash miss in the default case.
        let halfblock_overrides: std::collections::HashMap<(isize, usize), images::HalfblockCell> =
            self.halfblock_overrides();

        // 1. Terminal grid + phantom rows on each side (`r_lo..r_hi` defined
        // above where the shaping pass lives — same range so ligature
        // covers and emits stay in sync).
        //
        // Resolve every visible cell once, emit all bg quads, record the
        // boundary index, then emit all fg glyphs. The renderer issues two
        // draw_indexed calls against the resulting buffer (bg layer +
        // fg-and-overlays layer) so glow can bloom each layer independently.
        struct ResolvedCell {
            fg_source: GlyphSource,
            variant: font::FaceVariant,
            r: isize,
            c: usize,
            fg: [f32; 4],
        }
        let mut resolved: Vec<ResolvedCell> = Vec::with_capacity(rows * cols);
        // Optional per-scheme override for glyph color inside the selection.
        // Resolved once: `None` short-circuits the per-cell membership test
        // so the common (unselected / no-override) path stays branch-cheap.
        let selection_fg = pal.selection_fg;
        let selection_range = selection.as_ref().map(|s| s.range());
        for r in r_lo..r_hi {
            let over = row_overrides.get(&r);
            // Selection strip on this row (inclusive cols), or None if the
            // row falls outside the selection. Mirrors `strip_at` further
            // down where the overlay quads are emitted.
            let sel_strip = selection_range.and_then(|(start, end)| {
                let abs_line = self.active_tab().terminal.visual_to_abs_line(r);
                if abs_line < start.0 || abs_line > end.0 {
                    return None;
                }
                let from = if abs_line == start.0 { start.1 } else { 0 };
                let to = if abs_line == end.0 { end.1 } else { cols - 1 };
                if from > to || from >= cols { None } else { Some((from, to.min(cols - 1))) }
            });
            for c in 0..cols {
                // Half-block fallback: substitute the underlying cell
                // (typically a blank reserved by the placement) with a
                // ▀ glyph whose fg/bg pull from the preview's two
                // half-pixels. Wins over any other cell content because
                // the image placement *owns* these cells — there's no
                // real text the user expects to see here.
                if let Some(hb) = halfblock_overrides.get(&(r, c)) {
                    emit_bg_for_cell(&mut vertices, &mut indices, r, c, hb.bg);
                    // Image-replacement glyphs honour selection_fg too so a
                    // selection that runs through an image preview keeps a
                    // consistent text color.
                    let fg = match (selection_fg, sel_strip) {
                        (Some(sfg), Some((from, to))) if c >= from && c <= to => sfg,
                        _ => hb.fg,
                    };
                    resolved.push(ResolvedCell {
                        fg_source: GlyphSource::Char(images::HALFBLOCK_CHAR),
                        variant: font::FaceVariant::Regular,
                        r,
                        c,
                        fg,
                    });
                    continue;
                }
                let Some(cell) = self.active_tab().terminal.extended_cell(r, c) else { continue };
                // Kitty unicode-placeholder cells (`U+10EEEE` + image-id
                // encoded in fg). The image quad draws over this cell on
                // its own pipeline pass — emitting the U+10EEEE glyph
                // and the cell's fg-as-id color would just paint tofu
                // and ID-colored background on top of the image.
                if cell.placeholder_image_id.is_some() {
                    continue;
                }
                // SGR 7 (reverse) swaps fg/bg. Resolve unset colors to concrete
                // theme defaults before swapping — `default_bg` is transparent
                // so the window shows through, but reverse needs a solid bg
                // that the swap can move to fg (otherwise reverse-video text
                // and Claude Code's reverse-space cursor render invisible).
                let (fg, bg) = if cell.style.reverse {
                    let rfg = cell.style.color_fg.unwrap_or(default_fg);
                    let rbg = cell.style.color_bg.unwrap_or(default_bg_solid);
                    (rbg, rfg)
                } else {
                    (
                        cell.style.color_fg.unwrap_or(default_fg),
                        cell.style.color_bg.unwrap_or(default_bg),
                    )
                };
                // Quantise to whatever the scheme advertises (e.g. Mono /
                // Ansi16). Identity for the common Truecolor cap. Done
                // after reverse so reverse-video respects the cap too;
                // applied only to cell colors — cursor, selection, and
                // chrome remain at scheme-author fidelity. `project_cell` also
                // handles the Mono special case where a cell with an explicit
                // background is flipped to fg-ink-on-bg-text so it stays
                // distinct from an empty cell (see palette::Palette).
                let (fg, bg) = pal.project_cell(fg, bg);
                // Scheme-provided selection_fg wins over the cell's own fg
                // (including the post-reverse swap). Applied after projection
                // so it stays at scheme-author fidelity, matching how
                // cursor/selection chrome behave.
                let fg = match (selection_fg, sel_strip) {
                    (Some(sfg), Some((from, to))) if c >= from && c <= to => sfg,
                    _ => fg,
                };
                let variant = font::FaceVariant::from_flags(cell.style.bold, cell.style.italic);
                // Ligature pass may have substituted this cell's glyph.
                let fg_source = match over.and_then(|cs| cs[c]) {
                    Some((glyph_id, _v)) => GlyphSource::Substituted(glyph_id),
                    None => GlyphSource::Char(cell.ch),
                };
                emit_bg_for_cell(&mut vertices, &mut indices, r, c, bg);
                resolved.push(ResolvedCell { fg_source, variant, r, c, fg });
            }
        }
        // Everything emitted before this point is the BG layer. Glow runs
        // separately on bg vs fg, so the renderer needs this split index
        // to know where one layer's draw call ends and the next begins.
        let num_bg_indices = indices.len() as u32;
        for rc in &resolved {
            emit_fg_for_cell(
                &mut vertices,
                &mut indices,
                rc.fg_source,
                rc.variant,
                rc.r,
                rc.c,
                rc.fg,
            );
        }

        // 1a. Cmd-hover URL underline. Drawn on top of the glyph row so the
        // line is visible regardless of cell bg, and below the selection
        // overlay (1b) so a selected URL still reads as selected. Walks the
        // phantom-row range like the cell loop so the underline follows the
        // text through smooth scroll.
        if let Some(hu) = &self.active_tab().hover_url {
            for r in r_lo..r_hi {
                let abs_line = self.active_tab().terminal.visual_to_abs_line(r);
                // Underline every segment that lands on this line. Most links
                // have one per line; an OSC 8 link with an `id=` shared across
                // non-contiguous spans can have several, so don't stop early.
                for seg in hu.segments.iter().filter(|seg| seg.abs_line == abs_line) {
                    let from = seg.start_col;
                    if from >= cols {
                        continue;
                    }
                    let last = seg.end_col.min(cols - 1);
                    if last < from {
                        continue;
                    }
                    let ux = col_x(from);
                    let uw = (last - from + 1) as f32 * cell_w;
                    // Honor the font's own underline_position / underline_thickness
                    // so the line lands where the type designer intended and scales
                    // with point size. `underline_pos_px` is the (signed) offset of
                    // the stem center from the baseline — negative means below, so
                    // adding `-pos` walks downward in screen coords. Subtracting
                    // half the thickness then gives the top edge of the stripe.
                    let uh = underline_thickness_px;
                    let uy = row_y(r) - underline_pos_px - uh * 0.5 + row_scroll(r);
                    // Drop a moving row's underline once it crosses the region's
                    // bottom edge so it can't streak across the static status line.
                    if row_moving(r) && uy >= clip_bottom_px {
                        continue;
                    }
                    // Match the cell's foreground color so the underline tracks
                    // theme overrides; fall back to the default fg.
                    let fg = self.active_tab()
                        .terminal
                        .extended_cell(r, from)
                        .map(|cell| {
                            if cell.style.reverse {
                                cell.style.color_bg.unwrap_or(default_bg_solid)
                            } else {
                                cell.style.color_fg.unwrap_or(default_fg)
                            }
                        })
                        .unwrap_or(default_fg);
                    push_quad(
                        &mut vertices,
                        &mut indices,
                        ux,
                        uy,
                        uw,
                        uh,
                        [bg_u, bg_v],
                        [bg_u, bg_v],
                        fg,
                        [0.0; 4],
                    );
                }
            }
        }

        // 1b. Selection overlay. Each row's selected range is rendered as a
        // translucent strip; corner radii adapt to the neighbor rows so the
        // multi-row shape reads as one continuous form. Outer corners round
        // outward (convex), inner L-step corners round inward via a fillet
        // quad, and corners on a continuous vertical edge stay flat.
        if let Some(sel) = selection.as_ref() {
            let (start, end) = sel.range();
            // Outer convex corners get a generous radius for a soft pill
            // shape; inner concave fillets stay tighter so the L-step
            // joins read as a subtle curve rather than a deep bite.
            let convex_radius = (line_height * 0.35).min(cell_w * 0.7);
            let concave_radius = (line_height * 0.18).min(cell_w * 0.45);
            let strip_pad = (line_height - bg_h) * 0.5;

            // Range of selected columns on the row at `abs_line`, or `None`
            // if that line is outside the selection. Inclusive on both ends.
            let strip_at = |abs_line: isize| -> Option<(usize, usize)> {
                if abs_line < start.0 || abs_line > end.0 {
                    return None;
                }
                let from = if abs_line == start.0 { start.1 } else { 0 };
                let to = if abs_line == end.0 { end.1 } else { cols - 1 };
                if from > to || from >= cols {
                    None
                } else {
                    Some((from, to.min(cols - 1)))
                }
            };

            // Match the cell loop's phantom range so a partially-scrolled
            // row keeps its selection strip drawn through the slide.
            for r in r_lo..r_hi {
                let abs_line = self.active_tab().terminal.visual_to_abs_line(r);
                let Some((from, to)) = strip_at(abs_line) else { continue };
                let prev = strip_at(abs_line - 1);
                let next = strip_at(abs_line + 1);

                // The corner at column `to + 1` looks at column `to` in the
                // neighbor (the cell whose right edge meets the corner).
                let tl = classify_corner_with_neighbor(from, prev, HorizSide::Left);
                let tr = classify_corner_with_neighbor(to, prev, HorizSide::Right);
                let bl = classify_corner_with_neighbor(from, next, HorizSide::Left);
                let br = classify_corner_with_neighbor(to, next, HorizSide::Right);

                let r_tl = if tl == CornerType::Convex { convex_radius } else { 0.0 };
                let r_tr = if tr == CornerType::Convex { convex_radius } else { 0.0 };
                let r_bl = if bl == CornerType::Convex { convex_radius } else { 0.0 };
                let r_br = if br == CornerType::Convex { convex_radius } else { 0.0 };

                let sx = col_x(from);
                let sw = (to - from + 1) as f32 * cell_w;
                let sy = row_y(r) - bg_h - descender - strip_pad + row_scroll(r);
                push_quad(
                    &mut vertices,
                    &mut indices,
                    sx,
                    sy,
                    sw,
                    line_height,
                    [bg_u, bg_v],
                    [bg_u, bg_v],
                    selection_bg,
                    [r_tr, r_br, r_tl, r_bl],
                );

                // Concave fillets: each is an r×r quad in the unselected
                // quadrant adjacent to the strip's concave corner. The
                // negative radius slot tells the shader where to place the
                // quarter-circle bite (at the rect corner farthest from the
                // strip's concave corner).
                let cr = concave_radius;
                let push_fillet = |vertices: &mut Vec<renderer::vertex::Vertex>,
                                   indices: &mut Vec<u32>,
                                   fx: f32,
                                   fy: f32,
                                   bite: [f32; 4]| {
                    push_quad(
                        vertices,
                        indices,
                        fx,
                        fy,
                        cr,
                        cr,
                        [bg_u, bg_v],
                        [bg_u, bg_v],
                        selection_bg,
                        bite,
                    );
                };

                let strip_top = sy;
                let strip_bottom = sy + line_height;
                let left_edge = sx;
                let right_edge = sx + sw;
                if tl == CornerType::Concave {
                    // Bite cut at fillet's BL (radii.w).
                    push_fillet(
                        &mut vertices,
                        &mut indices,
                        left_edge - cr,
                        strip_top,
                        [0.0, 0.0, 0.0, -cr],
                    );
                }
                if tr == CornerType::Concave {
                    // Bite at fillet's BR (radii.y).
                    push_fillet(
                        &mut vertices,
                        &mut indices,
                        right_edge,
                        strip_top,
                        [0.0, -cr, 0.0, 0.0],
                    );
                }
                if bl == CornerType::Concave {
                    // Bite at fillet's TL (radii.z).
                    push_fillet(
                        &mut vertices,
                        &mut indices,
                        left_edge - cr,
                        strip_bottom - cr,
                        [0.0, 0.0, -cr, 0.0],
                    );
                }
                if br == CornerType::Concave {
                    // Bite at fillet's TR (radii.x).
                    push_fillet(
                        &mut vertices,
                        &mut indices,
                        right_edge,
                        strip_bottom - cr,
                        [-cr, 0.0, 0.0, 0.0],
                    );
                }
            }
        }

        // 1b. Find-in-scrollback match highlights. Translucent rounded quads
        //     over each matched run; the current (stepped-to) match gets a
        //     stronger fill drawn last so it reads as emphasised. Mapped from a
        //     match's absolute line to a visible row the same way the selection
        //     strip is, and only drawn for matches inside the phantom range.
        if self.search.open && !self.search.matches.is_empty() {
            let pal = palette::get();
            let strip_pad = (line_height - bg_h) * 0.5;
            let yellow = pal.ansi[3];
            let hl_radius = (cell_w * 0.18).min(line_height * 0.25);
            let all_a = 0.32_f32;
            let all_color = [
                yellow[0] * all_a,
                yellow[1] * all_a,
                yellow[2] * all_a,
                all_a,
            ];
            let cur_a = 0.62_f32;
            let cur_color = [
                yellow[0] * cur_a,
                yellow[1] * cur_a,
                yellow[2] * cur_a,
                cur_a,
            ];
            let top_abs = self.active_tab().terminal.visual_to_abs_line(0);
            let emit_match = |vertices: &mut Vec<renderer::vertex::Vertex>,
                              indices: &mut Vec<u32>,
                              m: &search::Match,
                              color: [f32; 4]| {
                let r = m.line - top_abs;
                if r < r_lo || r >= r_hi {
                    return;
                }
                let from = m.start_col.min(cols.saturating_sub(1));
                let to = m.end_col.min(cols.saturating_sub(1));
                if from > to {
                    return;
                }
                let sx = col_x(from);
                let sw = (to - from + 1) as f32 * cell_w;
                let sy = row_y(r) - bg_h - descender - strip_pad + row_scroll(r);
                push_quad(
                    vertices,
                    indices,
                    sx,
                    sy,
                    sw,
                    line_height,
                    [bg_u, bg_v],
                    [bg_u, bg_v],
                    color,
                    [hl_radius; 4],
                );
            };
            for (i, m) in self.search.matches.iter().enumerate() {
                if i == self.search.current {
                    continue; // drawn last, on top
                }
                emit_match(&mut vertices, &mut indices, m, all_color);
            }
            if let Some(cur) = self.search.current_match() {
                emit_match(&mut vertices, &mut indices, &cur, cur_color);
            }
        }

        // 1c. OSC 133 prompt-status gutter. A short rounded vertical bar in
        //     the left window padding at each prompt's row, colored by the
        //     command's exit status — green for success, red for failure, and
        //     a dim foreground tint while a command is still running (or the
        //     shell reported no code). Drawn in the padding so it never
        //     overlaps cell content. Off unless `prompt_gutter` opts in.
        let status_markers = if self.config.prompt_gutter == PromptGutter::None {
            Vec::new()
        } else {
            self.active_tab().terminal.prompt_status_markers()
        };
        if !status_markers.is_empty() {
            let pal = palette::get();
            let bar_w = (cell_w * 0.16).clamp(2.0, 4.0);
            let bar_x = (WINDOW_PADDING - bar_w) * 0.5; // centered in the padding
            let strip_pad = (line_height - bg_h) * 0.5;
            let bar_radius = bar_w * 0.5;
            for r in r_lo..r_hi {
                let abs_line = self.active_tab().terminal.visual_to_abs_line(r);
                let Some((_, status)) = status_markers.iter().find(|(l, _)| *l == abs_line)
                else {
                    continue;
                };
                let color = match status {
                    terminal::PromptStatus::Success => pal.ansi[2], // green
                    terminal::PromptStatus::Failure => pal.ansi[1], // red
                    terminal::PromptStatus::Pending => {
                        let fg = pal.foreground;
                        [fg[0], fg[1], fg[2], fg[3] * 0.35]
                    }
                };
                // Inset a little from the row's top/bottom so the bar reads as
                // a marker rather than filling the line.
                let inset = line_height * 0.18;
                let sy = row_y(r) - bg_h - descender - strip_pad + row_scroll(r) + inset;
                let sh = (line_height - 2.0 * inset).max(2.0);
                push_quad(
                    &mut vertices,
                    &mut indices,
                    bar_x,
                    sy,
                    bar_w,
                    sh,
                    [bg_u, bg_v],
                    [bg_u, bg_v],
                    color,
                    [bar_radius; 4],
                );
            }
        }

        // 2. Cursor box, only when the live cursor row is actually visible on
        //    screen (scrollback may have pushed it off the bottom). Shape
        //    follows DECSCUSR — block, underline, or bar. The displayed
        //    position eases in cell-space toward the logical position via
        //    `cursor_anim` so typing/navigation slides instead of snapping.
        //    Cells along the path that just went non-blank → blank (e.g.
        //    backspace overwriting with space) are captured as fading
        //    `cursor_ghosts` so the deleted glyph dissolves under the slide
        //    instead of vanishing the instant the cursor starts moving.
        let viewport_key = ViewportKey {
            rows,
            cols,
            view_offset: self.active_tab().terminal.view_offset(),
            on_alt_screen: self.active_tab().terminal.on_alt_screen(),
        };
        // A viewport change (resize, scrollback, alt-screen toggle) makes
        // last frame's snapshot non-comparable cell-for-cell, so we drop
        // any in-flight ghosts and skip detection until we have a fresh
        // matching snapshot to compare against.
        let key_matches = self.active_tab()
            .prev_visible
            .as_ref()
            .map(|s| s.key == viewport_key)
            .unwrap_or(false);
        if !key_matches {
            self.tabs[self.active].cursor_ghosts.clear();
        }

        // The cursor and its ghosts animate in BUFFER coordinates so changes
        // to the user's scroll position (which only shift `view_offset`)
        // don't trigger a slide — they ride along with the rest of the
        // grid. `live_grid_offset` is the integer visual-row delta to apply
        // when converting buffer rows back to viewport pixel space; equal
        // to `scrollback_visible` on the primary screen, 0 on alt screen.
        let live_grid_offset_i = if self.active_tab().terminal.on_alt_screen() {
            0usize
        } else {
            self.active_tab().terminal.view_offset().min(rows)
        };
        let live_grid_offset = live_grid_offset_i as f32;

        // Cursor anchor for the completion popup, captured while the cursor is
        // drawn (same row/col→pixel mapping). `(anchor_x, cursor_row_top)`.
        let mut popup_anchor: Option<(f32, f32)> = None;
        if let Some(_cur_visual_row) = self.active_tab().terminal.cursor_visual_row() {
            let cur = self.active_tab().terminal.cursor();
            let cur_col = cur.col.min(cols.saturating_sub(1));
            let target = (cur_col as f32, cur.row as f32);
            let anim_secs = self.config.cursor_anim_secs;
            let visible = self.cursor_currently_visible();

            // Capture ghosts before retargeting — once `anim.to` advances we
            // lose the previous-target column/row. Bounding-box scan is in
            // buffer coords; prev_visible uses visual rows, so translate via
            // `live_grid_offset` (consistent because key_matches implies
            // view_offset hasn't changed since the snapshot).
            // Collected into a local Vec, then appended after the `snap`
            // borrow below is dropped — pushing straight into the tab would
            // borrow it mutably while `snap` holds the same tab immutably.
            let mut new_ghosts: Vec<CursorGhost> = Vec::new();
            if anim_secs > 0.0 && key_matches {
                if let (Some(prev_anim), Some(snap)) = (
                    self.active_tab().cursor_anim.as_ref(),
                    self.active_tab().prev_visible.as_ref(),
                ) {
                    let (pcol, prow) = prev_anim.to;
                    let moved = (pcol - target.0).abs() > f32::EPSILON
                        || (prow - target.1).abs() > f32::EPSILON;
                    if moved {
                        let r0 = prow.min(target.1).floor().max(0.0) as usize;
                        let r1 = prow
                            .max(target.1)
                            .ceil()
                            .min((rows.saturating_sub(1)) as f32)
                            as usize;
                        let c0 = pcol.min(target.0).floor().max(0.0) as usize;
                        let c1 = pcol
                            .max(target.0)
                            .ceil()
                            .min((cols.saturating_sub(1)) as f32)
                            as usize;
                        let now = std::time::Instant::now();
                        for buf_r in r0..=r1 {
                            let vis_r = buf_r + live_grid_offset_i;
                            for c in c0..=c1 {
                                if vis_r >= snap.cells.len() || c >= snap.cells[vis_r].len() {
                                    continue;
                                }
                                let prev = snap.cells[vis_r][c];
                                if is_blank_cell(&prev) {
                                    continue;
                                }
                                let now_cell = self.active_tab().terminal.visible_cell(vis_r, c);
                                if !is_blank_cell(&now_cell) {
                                    continue;
                                }
                                new_ghosts.push(CursorGhost {
                                    ch: prev.ch,
                                    style: prev.style,
                                    buffer_row: buf_r,
                                    col: c,
                                    started_at: now,
                                });
                            }
                        }
                    }
                }
            }

            // Now that `snap`'s immutable borrow has ended, fold in the ghosts
            // captured above.
            self.tabs[self.active].cursor_ghosts.append(&mut new_ghosts);

            // Drop ghosts whose underlying cell got rewritten with new
            // content (e.g. user typed a replacement after the backspace),
            // or whose fade has run out.
            let now = std::time::Instant::now();
            {
                // Scoped so the `&mut tab` (which borrows `self.tabs`) is
                // released before the ghost-emit loop reads the active tab.
                // `cursor_ghosts` (mut) and `terminal` (read) split-borrow the
                // one tab.
                let tab = &mut self.tabs[self.active];
                let terminal = &tab.terminal;
                tab.cursor_ghosts.retain(|g| {
                    let elapsed = now.duration_since(g.started_at).as_secs_f32();
                    if anim_secs <= 0.0 || elapsed >= anim_secs {
                        return false;
                    }
                    let vis_r = g.buffer_row + live_grid_offset_i;
                    is_blank_cell(&terminal.visible_cell(vis_r, g.col))
                });
            }

            // Emit ghost glyphs as foreground-only quads with linearly
            // decaying alpha. Drawn before the cursor box so the cursor
            // visually consumes the ghost as it slides over.
            for ghost in &self.active_tab().cursor_ghosts {
                let elapsed = now.duration_since(ghost.started_at).as_secs_f32();
                let alpha = (1.0 - (elapsed / anim_secs).clamp(0.0, 1.0)).max(0.0);
                let mut fg = ghost.style.color_fg.unwrap_or(default_fg);
                // Premultiplied alpha to match the pipeline's blend mode.
                fg[0] *= alpha;
                fg[1] *= alpha;
                fg[2] *= alpha;
                fg[3] *= alpha;
                let variant =
                    font::FaceVariant::from_flags(ghost.style.bold, ghost.style.italic);
                let vis_r = ghost.buffer_row + live_grid_offset_i;
                emit_fg_for_cell(
                    &mut vertices,
                    &mut indices,
                    GlyphSource::Char(ghost.ch),
                    variant,
                    vis_r as isize,
                    ghost.col,
                    fg,
                );
            }

            let anim = self.tabs[self.active].cursor_anim.get_or_insert_with(|| CursorAnim::snapped(target));
            anim.retarget(target, anim_secs);

            if visible {
                let (eased_col, eased_buf_row) = anim.current(anim_secs);
                let eased_vis_row = eased_buf_row + live_grid_offset;
                let block_x = WINDOW_PADDING + eased_col * cell_w;
                // Cursor lives in the same per-row strip as the bg quad so
                // it aligns with selection / colored backgrounds.
                let cur_baseline =
                    WINDOW_PADDING + decorator_offset + (eased_vis_row + 1.0) * line_height;
                let block_y = cur_baseline - bg_h - descender - (line_height - bg_h) * 0.5
                    + row_scroll(eased_vis_row.round() as isize);
                // Anchor the completion popup to this cell's strip: left edge at
                // the cursor column, with `block_y` the top of the cursor row.
                popup_anchor = Some((block_x, block_y));
                let cursor_color = palette::get().cursor;
                // Underline / bar use a 2-px stripe; block fills the full cell.
                let stripe = 2.0_f32;
                let (cx, cy, cw, ch) = match self.active_tab().terminal.cursor_shape() {
                    terminal::CursorShape::Block => (block_x, block_y, cell_w, line_height),
                    terminal::CursorShape::Underline => {
                        (block_x, block_y + line_height - stripe, cell_w, stripe)
                    }
                    terminal::CursorShape::Bar => (block_x, block_y, stripe, line_height),
                };
                push_quad(
                    &mut vertices,
                    &mut indices,
                    cx,
                    cy,
                    cw,
                    ch,
                    [bg_u, bg_v],
                    [bg_u, bg_v],
                    cursor_color,
                    [0.0; 4],
                );
            }
        } else {
            // Cursor scrolled out of view. Drop the ease so the next time it
            // returns we snap to the new position instead of sliding in from
            // a stale one. Ghosts are tied to the cursor's motion so go with it.
            self.tabs[self.active].cursor_anim = None;
            self.tabs[self.active].cursor_ghosts.clear();
        }

        // Completion popup overlay (autocomplete slice K10). Drawn AFTER the
        // cursor/selection FG quads so it sits on top, and only when there are
        // cached suggestions, the cursor is on-screen (we have an anchor), and
        // the user hasn't scrolled into history (the cursor isn't where they're
        // looking then). Display-only: K11 adds keyboard nav + accept.
        //
        // TODO(K11+): consider excluding the popup from bloom. Appending into
        // the FG index range means it participates in the glow/bloom pass when
        // glow is on; acceptable for K10.
        let popup_visible = !self.active_tab().completions.is_empty() && self.active_tab().terminal.view_offset() == 0;
        if let Some((anchor_x, cursor_row_top)) = popup_anchor.filter(|_| popup_visible) {
            let len = self.active_tab().completions.len();
            // The visible window: `COMPLETION_MAX_VISIBLE` rows starting at the
            // scroll offset, clamped to the list. `n` is how many rows render.
            let start = self.active_tab().completion_scroll.min(len);
            let end = (start + COMPLETION_MAX_VISIBLE).min(len);
            let visible = &self.active_tab().completions[start..end];
            let n = visible.len();
            let item_h = line_height;
            // Box width: longest visible suggestion (chars) plus a little
            // horizontal padding, capped, so it's deterministic and testable.
            let longest = visible
                .iter()
                .map(|s| s.text.chars().count())
                .max()
                .unwrap_or(0);
            let text_pad = cell_w; // half a cell each side
            let box_w = (longest as f32 * cell_w + text_pad * 2.0).min(cell_w * 48.0);

            let anchor_below_y = cursor_row_top + line_height;
            let screen_w = self.surface.config.width as f32;
            let screen_h = self.surface.config.height as f32;
            let layout = completion::popup_layout(
                anchor_x,
                anchor_below_y,
                cursor_row_top,
                n,
                item_h,
                box_w,
                screen_w,
                screen_h,
                WINDOW_PADDING,
            );

            // Colors derived from the active palette so themes are respected.
            // The pipeline expects premultiplied alpha (RGB pre-scaled by A),
            // matching how selection/cursor colors are built above.
            let premul = |rgb: [f32; 4], a: f32| [rgb[0] * a, rgb[1] * a, rgb[2] * a, a];
            // Dark, semi-opaque box from a darkened background.
            let bg = pal.background;
            let box_rgb = [bg[0] * 0.6, bg[1] * 0.6, bg[2] * 0.6, 1.0];
            let box_color = premul(box_rgb, 0.92);
            // Highlight (selected row) — blend background toward foreground.
            let fgc = pal.foreground;
            let hl_rgb = [
                bg[0] * 0.5 + fgc[0] * 0.5,
                bg[1] * 0.5 + fgc[1] * 0.5,
                bg[2] * 0.5 + fgc[2] * 0.5,
                1.0,
            ];
            let hl_color = premul(hl_rgb, 0.85);
            let text_color = pal.foreground;
            let radius = 5.0_f32;

            // Box background (rounded corners, all four equal).
            push_quad(
                &mut vertices,
                &mut indices,
                layout.x,
                layout.y,
                layout.w,
                layout.h,
                [bg_u, bg_v],
                [bg_u, bg_v],
                box_color,
                [radius; 4],
            );

            // Highlight the selected row at its on-screen offset within the
            // visible window. Round the highlight's top corners only when it's
            // the box's first visible row, and its bottom corners only when it's
            // the last — so the highlight's rounding tracks the box's edges.
            let hl_row = self.active_tab().selected_completion.saturating_sub(start);
            if hl_row < n {
                let hl_y = layout.y + hl_row as f32 * item_h;
                let round_top = if hl_row == 0 { radius } else { 0.0 };
                let round_bot = if hl_row + 1 == n { radius } else { 0.0 };
                push_quad(
                    &mut vertices,
                    &mut indices,
                    layout.x,
                    hl_y,
                    layout.w,
                    item_h,
                    [bg_u, bg_v],
                    [bg_u, bg_v],
                    hl_color,
                    [round_top, round_bot, round_top, round_bot],
                );
            }

            // Suggestion text rows. Baseline within each row mirrors the grid:
            // strip_top + ascent (ascent above baseline = bg_h + descender,
            // descender being negative) + the centering pad.
            let text_x = layout.x + text_pad;
            let max_text_x = layout.x + layout.w - text_pad;
            for (i, sug) in visible.iter().enumerate() {
                let row_top = layout.y + i as f32 * item_h;
                let baseline = row_top + bg_h + descender + strip_pad;
                emit_text_run(
                    atlas,
                    &mut vertices,
                    &mut indices,
                    text_x,
                    baseline,
                    &sug.text,
                    text_color,
                    atlas_w,
                    atlas_h,
                    cell_w,
                    max_text_x,
                );
            }
        }

        // Command palette overlay (Cmd-Shift-P). Drawn last so it sits above
        // everything, anchored top-center rather than at the cursor. Reuses the
        // popup's quad/glyph helpers and palette-derived colors. Layout: a dim
        // backdrop, a rounded box, an input line with a caret, then (in command
        // mode) a separator and the filtered, scrollable command rows.
        if self.command_palette.open {
            use command_palette::{Mode, PALETTE_MAX_VISIBLE};
            let cp = &self.command_palette;
            let premul = |rgb: [f32; 3], a: f32| [rgb[0] * a, rgb[1] * a, rgb[2] * a, a];
            let screen_w = self.surface.config.width as f32;
            let screen_h = self.surface.config.height as f32;

            // Dim the terminal behind the palette to pull focus.
            push_quad(
                &mut vertices,
                &mut indices,
                0.0,
                0.0,
                screen_w,
                screen_h,
                [bg_u, bg_v],
                [bg_u, bg_v],
                premul([0.0, 0.0, 0.0], 0.45),
                [0.0; 4],
            );

            // Box geometry: a fixed-ish width centered horizontally, parked near
            // the top of the window.
            let box_w = (screen_w * 0.6).clamp(cell_w * 24.0, cell_w * 72.0).min(screen_w - WINDOW_PADDING * 2.0);
            let box_x = ((screen_w - box_w) * 0.5).round();
            let box_y = (screen_h * 0.12).round();
            let pad_v = (line_height * 0.45).round();
            let row_h = line_height;
            let sep_h = 1.0_f32;

            // Visible slice of the filtered list. Both command mode and the
            // choose-a-value mode (e.g. the theme picker) show a list; only
            // free-text argument mode hides it.
            let list_mode = cp.has_list();
            let start = cp.scroll.min(cp.filtered.len());
            let end = (start + PALETTE_MAX_VISIBLE).min(cp.filtered.len());
            let n = if list_mode { end - start } else { 0 };
            let has_list = n > 0;

            let total_h = pad_v * 2.0
                + row_h
                + if has_list { sep_h + n as f32 * row_h } else { 0.0 };

            // Colors, mirroring the completion popup so themes apply.
            let bg = pal.background;
            let box_color = premul([bg[0] * 0.55, bg[1] * 0.55, bg[2] * 0.55], 0.96);
            let fgc = pal.foreground;
            let hl_color = premul(
                [
                    bg[0] * 0.4 + fgc[0] * 0.6,
                    bg[1] * 0.4 + fgc[1] * 0.6,
                    bg[2] * 0.4 + fgc[2] * 0.6,
                ],
                0.9,
            );
            let text_color = pal.foreground;
            let caret_color = premul([fgc[0], fgc[1], fgc[2]], 0.9);
            let radius = 8.0_f32;

            // Box background.
            push_quad(
                &mut vertices,
                &mut indices,
                box_x,
                box_y,
                box_w,
                total_h,
                [bg_u, bg_v],
                [bg_u, bg_v],
                box_color,
                [radius; 4],
            );

            let text_x = box_x + cell_w;
            let max_text_x = box_x + box_w - cell_w;

            // Input line: a prompt prefix, then the typed text. In argument /
            // choose mode the prefix names what's being entered (e.g. "Title: ",
            // "Theme: ").
            let prefix = match cp.mode {
                Mode::Commands => "> ".to_string(),
                Mode::Argument { prompt, .. } | Mode::Choose { prompt, .. } => {
                    format!("{prompt}: ")
                }
            };
            let input_top = box_y + pad_v;
            let input_text = format!("{prefix}{}", cp.input.value);
            let baseline = input_top + bg_h + descender + strip_pad;
            emit_text_run(
                atlas,
                &mut vertices,
                &mut indices,
                text_x,
                baseline,
                &input_text,
                text_color,
                atlas_w,
                atlas_h,
                cell_w,
                max_text_x,
            );

            // Caret: a thin bar after the prefix + the chars left of the cursor.
            let caret_col = prefix.chars().count() + cp.input.cursor_col();
            let caret_x = text_x + caret_col as f32 * cell_w;
            if caret_x + 2.0 <= max_text_x {
                push_quad(
                    &mut vertices,
                    &mut indices,
                    caret_x,
                    input_top + strip_pad,
                    2.0,
                    bg_h,
                    [bg_u, bg_v],
                    [bg_u, bg_v],
                    caret_color,
                    [0.0; 4],
                );
            }

            if has_list {
                let list_top = input_top + row_h + sep_h;
                // Separator between the input and the results.
                push_quad(
                    &mut vertices,
                    &mut indices,
                    box_x,
                    input_top + row_h,
                    box_w,
                    sep_h,
                    [bg_u, bg_v],
                    [bg_u, bg_v],
                    premul([fgc[0], fgc[1], fgc[2]], 0.18),
                    [0.0; 4],
                );

                // Highlight the selected row within the visible window.
                let hl_row = cp.selected.saturating_sub(start);
                if hl_row < n {
                    push_quad(
                        &mut vertices,
                        &mut indices,
                        box_x,
                        list_top + hl_row as f32 * row_h,
                        box_w,
                        row_h,
                        [bg_u, bg_v],
                        [bg_u, bg_v],
                        hl_color,
                        [0.0; 4],
                    );
                }

                // Row labels: command titles in command mode, candidate values
                // (e.g. theme names) in choose mode — `row_label` hides which.
                for i in 0..n {
                    let Some(label) = cp.row_label(start + i) else {
                        continue;
                    };
                    let row_top = list_top + i as f32 * row_h;
                    let baseline = row_top + bg_h + descender + strip_pad;
                    emit_text_run(
                        atlas,
                        &mut vertices,
                        &mut indices,
                        text_x,
                        baseline,
                        label,
                        text_color,
                        atlas_w,
                        atlas_h,
                        cell_w,
                        max_text_x,
                    );
                }
            }
        }

        // Find-in-scrollback overlay (Cmd-F). Same centered, palette-styled box:
        // a dim backdrop, a rounded box, the "Find:" input line with a caret,
        // and — once there's a query — a separator and a result counter
        // ("3 / 17" or "No results"). Reuses the palette's quad/glyph helpers.
        if self.search.open {
            let premul = |rgb: [f32; 3], a: f32| [rgb[0] * a, rgb[1] * a, rgb[2] * a, a];
            let screen_w = self.surface.config.width as f32;
            let screen_h = self.surface.config.height as f32;

            // Dim the terminal behind the box.
            push_quad(
                &mut vertices,
                &mut indices,
                0.0,
                0.0,
                screen_w,
                screen_h,
                [bg_u, bg_v],
                [bg_u, bg_v],
                premul([0.0, 0.0, 0.0], 0.45),
                [0.0; 4],
            );

            let box_w = (screen_w * 0.6)
                .clamp(cell_w * 24.0, cell_w * 72.0)
                .min(screen_w - WINDOW_PADDING * 2.0);
            let box_x = ((screen_w - box_w) * 0.5).round();
            let box_y = (screen_h * 0.12).round();
            let pad_v = (line_height * 0.45).round();
            let row_h = line_height;
            let sep_h = 1.0_f32;

            let query = self.search.input.value.clone();
            let status = if query.is_empty() {
                String::new()
            } else if self.search.matches.is_empty() {
                "No results".to_string()
            } else {
                format!("{} / {}", self.search.current + 1, self.search.matches.len())
            };
            let has_status = !status.is_empty();

            let total_h = pad_v * 2.0 + row_h + if has_status { sep_h + row_h } else { 0.0 };

            let bg = pal.background;
            let box_color = premul([bg[0] * 0.55, bg[1] * 0.55, bg[2] * 0.55], 0.96);
            let fgc = pal.foreground;
            let text_color = pal.foreground;
            let caret_color = premul([fgc[0], fgc[1], fgc[2]], 0.9);
            let radius = 8.0_f32;

            push_quad(
                &mut vertices,
                &mut indices,
                box_x,
                box_y,
                box_w,
                total_h,
                [bg_u, bg_v],
                [bg_u, bg_v],
                box_color,
                [radius; 4],
            );

            let text_x = box_x + cell_w;
            let max_text_x = box_x + box_w - cell_w;

            // Input line: "Find: " prefix then the query.
            let prefix = "Find: ";
            let input_top = box_y + pad_v;
            let input_text = format!("{prefix}{query}");
            let baseline = input_top + bg_h + descender + strip_pad;
            emit_text_run(
                atlas,
                &mut vertices,
                &mut indices,
                text_x,
                baseline,
                &input_text,
                text_color,
                atlas_w,
                atlas_h,
                cell_w,
                max_text_x,
            );

            // Caret after the prefix + chars left of the cursor.
            let caret_col = prefix.chars().count() + self.search.input.cursor_col();
            let caret_x = text_x + caret_col as f32 * cell_w;
            if caret_x + 2.0 <= max_text_x {
                push_quad(
                    &mut vertices,
                    &mut indices,
                    caret_x,
                    input_top + strip_pad,
                    2.0,
                    bg_h,
                    [bg_u, bg_v],
                    [bg_u, bg_v],
                    caret_color,
                    [0.0; 4],
                );
            }

            if has_status {
                // Separator between the input and the counter.
                push_quad(
                    &mut vertices,
                    &mut indices,
                    box_x,
                    input_top + row_h,
                    box_w,
                    sep_h,
                    [bg_u, bg_v],
                    [bg_u, bg_v],
                    premul([fgc[0], fgc[1], fgc[2]], 0.18),
                    [0.0; 4],
                );
                let status_top = input_top + row_h + sep_h;
                let baseline = status_top + bg_h + descender + strip_pad;
                emit_text_run(
                    atlas,
                    &mut vertices,
                    &mut indices,
                    text_x,
                    baseline,
                    &status,
                    premul([fgc[0], fgc[1], fgc[2]], 0.7),
                    atlas_w,
                    atlas_h,
                    cell_w,
                    max_text_x,
                );
            }
        }

        // Refresh the visible-grid snapshot with the current frame's cells
        // so the next retarget can spot what just got cleared. Keyed by
        // viewport so a resize / scrollback / alt-screen flip flushes the
        // comparison in `key_matches` above.
        let mut snap_cells: Vec<Vec<style::Cell>> = Vec::with_capacity(rows);
        for r in 0..rows {
            let mut row_cells: Vec<style::Cell> = Vec::with_capacity(cols);
            for c in 0..cols {
                row_cells.push(self.active_tab().terminal.visible_cell(r, c));
            }
            snap_cells.push(row_cells);
        }
        self.tabs[self.active].prev_visible = Some(GridSnapshot {
            cells: snap_cells,
            key: viewport_key,
        });

        // 3. Edge fades: vertical gradient quads pinned to the top and bottom
        // of the window. The top one obscures content sliding up behind the
        // macOS traffic-light strip; the bottom one mirrors the effect so the
        // phantom row sliding into / out of the bottom edge dissolves rather
        // than clipping abruptly. Drawn last so they overlay every cell. RGB
        // is premultiplied with alpha to match PREMULTIPLIED_ALPHA_BLENDING.
        let win_w = self.surface.config.width as f32;
        let win_h = self.surface.config.height as f32;
        // Top fade is taller than the bottom: the title bar + toolbar takes
        // about DECORATOR_HEIGHT to fully occlude, and a longer gradient
        // below that gives content a soft runway as it scrolls into view
        // rather than popping out from a hard edge.
        let top_fade_height = self.config.top_fade_height;
        let bottom_fade_height_max = self.config.bottom_fade_height;
        let clear = [0.0, 0.0, 0.0, 0.0];

        // Strip quads live in their own vertex/index buffer — they're drawn
        // by the blur strip pipeline in the composite pass.
        let mut strip_vertices: Vec<renderer::vertex::Vertex> = Vec::with_capacity(16);
        let mut strip_indices: Vec<u16> = Vec::with_capacity(32);
        let push_strip = |vertices: &mut Vec<renderer::vertex::Vertex>,
                          indices: &mut Vec<u16>,
                          y0: f32,
                          y1: f32,
                          c0: [f32; 4],
                          c1: [f32; 4]| {
            let start = vertices.len() as u16;
            // radii = 0 so the shader skips the SDF mask; local_pos /
            // half_size go unused but we have to populate them.
            let stub = [0.0_f32, 0.0];
            vertices.push(renderer::vertex::Vertex {
                position: [0.0, y0, 0.0],
                tex_coords: [bg_u, bg_v],
                color: c0,
                local_pos: stub,
                half_size: stub,
                radii: [0.0; 4],
            });
            vertices.push(renderer::vertex::Vertex {
                position: [0.0, y1, 0.0],
                tex_coords: [bg_u, bg_v],
                color: c1,
                local_pos: stub,
                half_size: stub,
                radii: [0.0; 4],
            });
            vertices.push(renderer::vertex::Vertex {
                position: [win_w, y0, 0.0],
                tex_coords: [bg_u, bg_v],
                color: c0,
                local_pos: stub,
                half_size: stub,
                radii: [0.0; 4],
            });
            vertices.push(renderer::vertex::Vertex {
                position: [win_w, y1, 0.0],
                tex_coords: [bg_u, bg_v],
                color: c1,
                local_pos: stub,
                half_size: stub,
                radii: [0.0; 4],
            });
            indices.extend_from_slice(&[start, start + 1, start + 2, start + 1, start + 2, start + 3]);
        };

        // Edge fade animations: each phase ramps 0→1 the moment its
        // boundary distance leaves zero (and 1→0 when it returns) at a
        // constant rate, so the fade slides in fully in TOP_FADE_ANIM_SECS
        // regardless of scroll speed. The phase drives both the band height
        // (0 → full) and the alpha (0 → 1) together.
        let now = std::time::Instant::now();
        let dt = now.duration_since(self.last_anim_tick).as_secs_f32();
        self.last_anim_tick = now;
        let advance = |phase: &mut f32, target: f32, secs: f32| {
            let step = if secs > 0.0 { dt / secs } else { 1.0 };
            if *phase < target {
                *phase = (*phase + step).min(target);
            } else if *phase > target {
                *phase = (*phase - step).max(target);
            }
        };
        advance(
            &mut self.top_fade_phase,
            if dist_from_top > 0.0 { 1.0 } else { 0.0 },
            self.config.top_fade_anim_secs,
        );
        advance(
            &mut self.bottom_fade_phase,
            if dist_from_bottom > 0.0 { 1.0 } else { 0.0 },
            self.config.bottom_fade_anim_secs,
        );
        // top_band_height / top_alpha also feed the per-fragment glyph-fade
        // uniform below, so they're computed unconditionally. The strip quads
        // themselves are skipped at phase=0: emitting them would draw with
        // alpha 0 but still bump num_strip_indices, forcing render() through
        // the slow blur+composite path.
        let top_alpha = self.top_fade_phase;
        let top_band_height = top_fade_height * self.top_fade_phase;
        if self.top_fade_phase > 0.0 {
            let top_mid = top_band_height * self.config.top_fade_solid_stop.clamp(0.0, 1.0);
            let top_blur = [0.0_f32, 0.0, 0.0, top_alpha];
            push_strip(&mut strip_vertices, &mut strip_indices, 0.0, top_mid, top_blur, top_blur);
            push_strip(&mut strip_vertices, &mut strip_indices, top_mid, top_band_height, top_blur, clear);
        }

        // Computed unconditionally — like the top, these also feed the
        // per-fragment glyph-fade uniform below. The strip quad itself is
        // skipped at phase=0 (see the top-fade note).
        let bottom_alpha = self.bottom_fade_phase;
        let bottom_band_height = bottom_fade_height_max * self.bottom_fade_phase;
        if self.bottom_fade_phase > 0.0 {
            let bottom_blur = [0.0_f32, 0.0, 0.0, bottom_alpha];
            push_strip(
                &mut strip_vertices,
                &mut strip_indices,
                win_h - bottom_band_height,
                win_h,
                clear,
                bottom_blur,
            );
        }

        self.shared.gpu
            .queue
            .write_buffer(&self.vertex_buffer, 0, bytemuck::cast_slice(&vertices));
        self.shared.gpu
            .queue
            .write_buffer(&self.index_buffer, 0, bytemuck::cast_slice(&indices));
        self.num_indices = indices.len() as u32;
        self.num_bg_indices = num_bg_indices;

        // Strip overlay: blur-only (tint = 0). The glyph fade already pulls
        // foreground text toward the bg color near each edge; the blur sits
        // on top to soften whatever's still visible in the gradient region.
        if !strip_indices.is_empty() {
            self.shared.gpu.queue.write_buffer(
                &self.strip_vertex_buffer,
                0,
                bytemuck::cast_slice(&strip_vertices),
            );
            self.shared.gpu.queue.write_buffer(
                &self.strip_index_buffer,
                0,
                bytemuck::cast_slice(&strip_indices),
            );
        }
        self.num_strip_indices = strip_indices.len() as u32;

        // Both edges run the per-fragment glyph fade so scrollback text
        // dissolves into the blur strip instead of reaching the edge sharp.
        // band_height is 0 at phase=0 (the live prompt at rest), which the
        // shader treats as "no fade" — so cells stay solid up to the bottom
        // edge whenever the bottom strip isn't engaged.
        let fade_data: [f32; 16] = [
            top_band_height, top_alpha, 0.0, 0.0,
            bottom_band_height, bottom_alpha, 0.0, 0.0,
            win_w, win_h, 0.0, 0.0,
            bg_u, bg_v, 0.0, 0.0,
        ];
        self.shared.gpu.queue.write_buffer(
            &self.fade_buffer,
            0,
            bytemuck::cast_slice(&fade_data),
        );
    }

    /// Edge-fade distances `(bottom, top)` that drive the fade phases and the
    /// title-bar decorator offset. On the primary screen they track the
    /// scrollback viewport (sub-line `scroll_y` included). On the alt screen
    /// there's no scrollback, so they're zero — except during an upward scroll
    /// slide, where the top fade is engaged so the departing rows dissolve into
    /// the translucent toolbar instead of popping out when the slide ends.
    pub(crate) fn edge_fade_dists(
        &self,
        scroll_y: f32,
        view_offset: f32,
        scrollback_len: f32,
        line_height: f32,
    ) -> (f32, f32) {
        if self.active_tab().terminal.on_alt_screen() {
            (0.0, 0.0)
        } else {
            (
                view_offset * line_height + scroll_y,
                (scrollback_len - view_offset) * line_height - scroll_y,
            )
        }
    }

    /// Half-open band `[r_lo, r_hi)` of grid rows to render: the visible grid
    /// (`0..rows`) plus phantom rows above and below so smooth sub-line
    /// scrolling stays populated through the snap.
    ///
    /// The fixed ±2 covers the rows that peek past the nominal grid because
    /// content flows behind the translucent title bar: `get_viewport_size`
    /// reserves DECORATOR_HEIGHT when sizing the grid, and mid-scroll
    /// `decorator_offset` is 0, so the visible region extends ~2 rows below the
    /// last grid row. A non-zero `scroll_y` then slides one further row in from
    /// the moving edge — without widening the band that row stays blank until
    /// the scroll crosses a line boundary, popping (snapping) into place
    /// instead of sliding in from off screen.
    ///
    /// On an alt-screen slide `scroll_y` spans the whole scrolled distance and
    /// can exceed a line; on the primary it's the sub-line scrollback offset.
    /// `ceil` of the offset (in rows) covers both — so the band widens on the
    /// primary screen too, fixing the bottom row snapping instead of sliding in.
    pub(crate) fn phantom_row_band(
        scroll_y: f32,
        rows: usize,
        line_height: f32,
    ) -> (isize, isize) {
        let anim_extra = (scroll_y.abs() / line_height).ceil() as isize;
        let r_lo = -2 - anim_extra;
        let r_hi = rows as isize + 2 + anim_extra;
        (r_lo, r_hi)
    }

    /// True while either edge-fade phase is still chasing its target —
    /// used to keep the event loop ticking until the slide completes.
    pub(crate) fn is_top_fade_animating(&self) -> bool {
        let scrollback_len = if self.active_tab().terminal.on_alt_screen() {
            0.0
        } else {
            self.active_tab().terminal.scrollback_len() as f32
        };
        let view_offset = self.active_tab().terminal.view_offset() as f32;
        let metrics = self.shared.with_font(|f| f.face().size_metrics().unwrap());
        let line_height = ((metrics.ascender - metrics.descender) >> 6) as f32;
        let scroll_y = self.active_tab().scroll_y as f32;
        let (dist_from_bottom, dist_from_top) =
            self.edge_fade_dists(scroll_y, view_offset, scrollback_len, line_height);
        let top_target = if dist_from_top > 0.0 { 1.0 } else { 0.0 };
        let bot_target = if dist_from_bottom > 0.0 { 1.0 } else { 0.0 };
        (self.top_fade_phase - top_target).abs() > f32::EPSILON
            || (self.bottom_fade_phase - bot_target).abs() > f32::EPSILON
    }

    /// Decide whether `poll_pending_images` should auto-create a
    /// Placement when this upload's decode succeeds.
    ///
    /// Three buckets:
    ///   - Cmd-Shift-I debug-paste flow: no `kitty_image_id`, no
    ///     up-front display → main.rs computes extent from pixel
    ///     dims and places at the cursor on decode. Returns `false`
    ///     here (meaning: deferred-auto-place IS desired, caller
    ///     stores `None`).
    ///   - `a=T` (Transmit and Display) without `U=1` →
    ///     `insert_placement_kitty` already ran before this point;
    ///     no auto-place needed.
    ///   - Any Kitty transmit-only path (`a=t`, `a=T,U=1`) → client
    ///     owns placement timing. Either an `a=p` later, or
    ///     `U+10EEEE` placeholder cells. Auto-placing a second
    ///     Placement at the cursor produces a ghost image that
    ///     renders alongside the placeholder-bbox draw.
    ///
    /// Returns `true` when the upload should suppress the deferred
    /// auto-placement.
    pub(crate) fn suppress_deferred_placement(
        display_immediately: bool,
        kitty_image_id: Option<u32>,
    ) -> bool {
        display_immediately || kitty_image_id.is_some()
    }

    /// Decision oracle for "should we render this placement as half-block
    /// glyphs?". Split from `halfblock_overrides` so the matrix of
    /// (config × store-state) outcomes can be tested in isolation without
    /// the surrounding Store / Terminal scaffolding.
    pub(crate) fn should_halfblock(
        images_enabled: bool,
        opted_in: bool,
        is_pending: bool,
        gpu_image_available: bool,
    ) -> bool {
        // Decode-in-flight always wins: even if the user opted in,
        // flipping briefly to a thumbnail and then snapping to GPU
        // pixels would flicker badly.
        if is_pending {
            return false;
        }
        if !images_enabled {
            // GPU path disabled — half-block is the only thing the user
            // can see. (Default value of `opted_in` doesn't gate this
            // case; disabling images entirely already implies wanting
            // *some* representation.)
            return true;
        }
        // Images enabled and the GPU has the texture: GPU path draws.
        if gpu_image_available {
            return false;
        }
        // Images enabled, GPU image missing, decode not in flight ⇒
        // a failed decode where main.rs's cleanup hasn't fired yet (one
        // frame, typically). Honor the opt-in.
        opted_in
    }

    /// Convert a live placement's grid-coord `top_row` into the viewport
    /// row the renderer should draw at, given the current scrollback
    /// view offset. Mirrors what `extended_cell` does for cells: live
    /// row R lands at viewport row `R + view_offset`. The shift is
    /// uncapped — `view_offset > rows` is still meaningful because
    /// smooth-scroll's `scroll_y` interpolates between view_offset
    /// ticks. Clamping at `rows` here makes the discrete viewport_row
    /// stop moving past the bottom while scroll_y keeps advancing,
    /// which produces a visible snap each time scroll_y crosses a
    /// line boundary (the image slides a row visually via scroll_y,
    /// then jumps back when the tick fires because viewport_row
    /// didn't move).
    ///
    /// Scrollback placements come pre-shifted out of
    /// `Terminal::scrollback_placements_in_view`, so this helper
    /// applies only to live placements. Off-screen draws are
    /// naturally clipped by the rasterizer; passing a viewport_row
    /// well past `rows` costs only the vertex buffer write.
    ///
    /// `rows` is kept on the signature so call sites don't change
    /// shape if a future clamp becomes desirable.
    pub(crate) fn live_placement_viewport_row(top_row: isize, view_offset: usize, _rows: usize) -> isize {
        top_row + view_offset as isize
    }

    /// Compute the UV sub-rect for one Kitty placeholder run against
    /// the source image's total cell extent `(total_cols, total_rows)`.
    /// Clamps to `[0.0, 1.0]` so a malformed encoder (or a placeholder
    /// grid that survives a smaller-than-original re-transmission)
    /// samples the edge instead of wrapping or sampling outside the
    /// texture. `total_cols` / `total_rows` are clamped to `>= 1`
    /// since they're the denominator — a 0 here would NaN every UV.
    /// Returns `(u0, v0, u1, v1)`.
    pub(crate) fn placeholder_run_uv(
        image_col_start: u16,
        image_col_end: u16,
        image_row: u16,
        total_cols: u32,
        total_rows: u32,
    ) -> (f32, f32, f32, f32) {
        let denom_cols = total_cols.max(1) as f32;
        let denom_rows = total_rows.max(1) as f32;
        let u0 = (image_col_start as f32 / denom_cols).clamp(0.0, 1.0);
        let u1 = (image_col_end as f32 / denom_cols).clamp(0.0, 1.0);
        let v0 = (image_row as f32 / denom_rows).clamp(0.0, 1.0);
        let v1 = ((image_row as f32 + 1.0) / denom_rows).clamp(0.0, 1.0);
        (u0, v0, u1, v1)
    }

    /// UV sub-rect for one cell-filling glyph quad, including the
    /// half-texel inset that closes hairline seams between abutting
    /// block / box-drawing glyphs. `(gx, gy)` is the glyph's atlas
    /// origin in texels; `(q_start, q_end)` / `(p_start, p_end)` are the
    /// in-glyph sample extents (horizontal / vertical) already clamped to
    /// the glyph bitmap; `atlas_w` / `atlas_h` are the atlas dimensions in
    /// texels.
    ///
    /// Synthesized cell-filling glyphs have *hard* opaque edges, and the
    /// atlas packs every glyph with one transparent column/row of padding
    /// (`stride = w + 1` in font.rs). Sampling right up to the texel
    /// boundary therefore makes the Linear filter average the opaque edge
    /// with that transparent neighbour (~50% alpha — a visible seam
    /// between abutting cells). So for a cell-filling glyph we pull the UV
    /// in by half a texel on *both* axes, landing every edge fragment on a
    /// fully-opaque texel center.
    ///
    /// The inset is gated on `cell_filling`, not on whether the axis is
    /// stretched: a glyph trimmed to its opaque bounding box (e.g. ▐,
    /// packed as a half-width bitmap bearing into the cell) is placed 1:1
    /// on its narrow axis yet its filled side still reaches the bitmap
    /// edge and would bleed into the padding there — that was the residual
    /// seam a fills_h/fills_v-only inset left behind. Normal glyphs
    /// (`cell_filling == false`) keep their true extent so they aren't
    /// thinned or shifted. Returns `(u0, v0, u1, v1)`.
    pub(crate) fn glyph_quad_uv(
        gx: f32,
        gy: f32,
        (q_start, q_end): (f32, f32),
        (p_start, p_end): (f32, f32),
        cell_filling: bool,
        atlas_w: f32,
        atlas_h: f32,
    ) -> (f32, f32, f32, f32) {
        let inset = if cell_filling { 0.5 } else { 0.0 };
        let u0 = (gx + q_start + inset) / atlas_w;
        let u1 = (gx + q_end - inset) / atlas_w;
        let v0 = (gy + p_start + inset) / atlas_h;
        let v1 = (gy + p_end - inset) / atlas_h;
        (u0, v0, u1, v1)
    }

    /// Build the per-cell half-block override map for the upcoming vertex
    /// rebuild. Empty in the common case (images enabled and decoded), so
    /// the lookup in the cell loop is a single hash miss per cell.
    ///
    /// Three cases produce overrides:
    ///   1. `images_enabled = false` AND a preview is cached (decoded
    ///      before the toggle, or some other process populated it).
    ///   2. `images_enabled = true` AND `peek` returns None AND the
    ///      placement is *not* in-flight (decode failed, but cleanup
    ///      hasn't fired yet — typically one frame) AND the user opted
    ///      in via `images_halfblock_for_missing`.
    ///   3. *Never* when `is_pending` is true: the decode is in flight
    ///      and pixels are arriving within a frame or two. Showing a
    ///      half-block thumbnail and then snapping to GPU pixels would
    ///      flicker badly.
    ///
    /// Returns a row-major hash keyed on viewport-coord `(row, col)` so
    /// the cell loop can probe with the same coords it already iterates.
    pub(crate) fn halfblock_overrides(
        &self,
    ) -> std::collections::HashMap<(isize, usize), images::HalfblockCell> {
        let mut out = std::collections::HashMap::new();
        let images_enabled = self.config.images_enabled;
        let opted_in = self.config.images_halfblock_for_missing;
        // Early-out: no possible override when images are on and the user
        // hasn't opted in to the failure-cleanup window.
        if images_enabled && !opted_in {
            return out;
        }
        let cols = self.active_tab().terminal.cols;
        let view_offset = self.active_tab().terminal.view_offset();
        let rows = self.active_tab().terminal.rows;
        for p in self.active_tab().terminal.live_placements() {
            let is_pending = self.active_tab().image_store.is_pending(p.image);
            let gpu_ok = self.active_tab().image_store.peek(p.image).is_some();
            if !Self::should_halfblock(images_enabled, opted_in, is_pending, gpu_ok) {
                continue;
            }
            let Some(preview) = self.active_tab().image_store.preview(p.image) else {
                // Disabled-but-no-preview: nothing to draw with. The
                // empty space is the right behaviour here; users who
                // want a placeholder would have to wait for a future
                // sub-slice that synthesizes a generic frame.
                continue;
            };
            // Grid → viewport row shift matches what cells get; see
            // `live_placement_viewport_row`.
            let top_vp = Self::live_placement_viewport_row(p.top_row, view_offset, rows);
            let cells = images::halfblock_cells(preview, p.rows, p.cols);
            for hb in cells {
                let r = top_vp + hb.row_offset as isize;
                let c = p.left_col + hb.col_offset as isize;
                // Clip to viewport — phantom rows above/below are still
                // valid (`extended_cell` reads them), but a placement
                // extending past the grid horizontally would land off
                // any drawn cell.
                if c < 0 || c >= cols as isize {
                    continue;
                }
                // Vertical clipping happens implicitly in the cell
                // emission loop (it iterates r_lo..r_hi); an entry whose
                // row falls outside that range is silently ignored
                // there. Keeping the entry in the map costs one HashMap
                // slot per scrolled-off row and avoids duplicating the
                // r_lo / r_hi math here.
                out.insert((r, c as usize), hb);
            }
        }
        out
    }

    pub(crate) fn render(
        &mut self,
        clear: wgpu::Color,
    ) -> Result<(std::time::Duration, bool), wgpu::SurfaceError> {
        let surface_t0 = std::time::Instant::now();
        let output = match self.surface.surface.get_current_texture() {
            Ok(o) => o,
            // A backgrounded / occluded / just-resized surface returns
            // `Outdated` (and `Lost`) routinely once there's more than one
            // window — reconfigure and skip this frame rather than panicking;
            // the next redraw re-acquires. (With one window this only tripped
            // on resize, which is why the old `unwrap` survived.)
            Err(wgpu::SurfaceError::Outdated) | Err(wgpu::SurfaceError::Lost) => {
                self.surface
                    .surface
                    .configure(&self.shared.gpu.device, &self.surface.config);
                return Ok((surface_t0.elapsed(), true));
            }
            Err(e) => return Err(e),
        };
        let surface_wait = surface_t0.elapsed();
        let view = output
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder =
            self.shared.gpu
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("terminal"),
                });

        // Three paths, chosen on the fly:
        //   - Fast path (no glow, no strips): one render pass straight to
        //     the swapchain, no offscreen anything. Saves ~5 fullscreen
        //     passes per frame — the dominant cost during PTY bursts.
        //   - Layered path (glow_on): bg quads → `blur.scene`, fg quads →
        //     `scene_fg`, then glow each independently and composite
        //     stacked. Lets bg glow bloom under fg text without washing
        //     the glyphs out, and keeps fg glow above its source.
        //   - Strip-only path (strips on, glow off): single scene render
        //     into `blur.scene`, blur, blit + strip overlay. The original
        //     behaviour from before the glow layering.
        let needs_strips = self.num_strip_indices > 0;
        let glow_on = self.glow.enabled();
        let needs_offscreen = needs_strips || glow_on;
        // Content scanlines paint over bg + fg (but not strips/UI). Runs
        // via a multiply-blend overlay pass; in the layered / strip-only
        // paths it's inserted into the composite render pass between
        // content and strips, in the fast path it's a second pass on
        // the swapchain with LoadOp::Load.
        let content_overlay_on = self.glow.match_content_scanlines;

        let bg_index_range = 0..self.num_bg_indices;
        let fg_index_range = self.num_bg_indices..self.num_indices;

        // `prepare_frame` (called by the redraw handler before `render`)
        // already polled the image store and ran mark-and-sweep, so the
        // store's state here matches what `update_vertices` saw. This
        // matters for the half-block fallback: vertex emission and image
        // draw selection now agree on `peek`'s return value.

        // Build per-frame image draw list from `live_placements()`. Cell
        // anchor → pixel rect uses the same font metrics + decorator_offset
        // + scroll_y that `update_vertices` applies to cell quads, so
        // images scroll smoothly alongside text.
        let metrics = self.shared.with_font(|f| f.face().size_metrics().unwrap());
        let line_height = ((metrics.ascender - metrics.descender) >> 6) as f32;
        let cell_w = self.shared.with_font(|f| f.cell_width()) as f32;
        let view_offset = self.active_tab().terminal.view_offset() as f32;
        let scrollback_len = if self.active_tab().terminal.on_alt_screen() {
            0.0
        } else {
            self.active_tab().terminal.scrollback_len() as f32
        };
        let dist_from_bottom = view_offset * line_height + self.active_tab().scroll_y as f32;
        let dist_from_top = (scrollback_len - view_offset) * line_height - self.active_tab().scroll_y as f32;
        let near = (dist_from_bottom / line_height)
            .min(dist_from_top / line_height)
            .clamp(0.0, 1.0);
        let decorator_offset = DECORATOR_HEIGHT * (1.0 - near);
        let scroll_y = self.active_tab().scroll_y as f32;

        // Resolve store entries up front so the borrow can live alongside
        // the upcoming `&mut encoder` calls. Placements whose image was
        // already evicted (shouldn't happen with mark-and-sweep, but
        // defensible) are silently skipped. When `images_enabled = false`
        // we never call the GPU image pipeline — the half-block fallback
        // (computed in `update_vertices` via `halfblock_overrides`) is
        // the only visible representation in that case.
        //
        // When images are on, two walks share the same pixel math: live
        // placements (always) plus, when the user has scrolled history
        // into view on the primary screen, scrollback placements rebased
        // into viewport-row coords by `scrollback_placements_in_view`.
        // The two walks differ in one thing: live placements carry GRID
        // coords in `top_row`, while scrollback placements come pre-
        // shifted into viewport coords. Live placements need the same
        // grid→visual shift `update_vertices` gives cells (history rows
        // fill the top of the viewport when `view_offset > 0`, pushing
        // live content down); scrollback placements already have it baked
        // in.
        let mut image_draws: Vec<renderer::images::ImageDraw<'_>> = Vec::new();
        if self.config.images_enabled {
            let view_offset = self.active_tab().terminal.view_offset();
            let rows = self.active_tab().terminal.rows;
            let scrollback_draws = self.active_tab()
                .terminal
                .scrollback_placements_in_view(self.active_tab().terminal.rows);
            image_draws.reserve(
                self.active_tab().terminal.live_placements().len() + scrollback_draws.len(),
            );
            // `(viewport_row, placement)` tuples — by the time the pixel
            // math runs, the row index is in viewport coords. Live
            // placements get the same shift `extended_cell` applies to
            // cells (history rows push live content down); scrollback
            // placements arrive pre-shifted from `scrollback_placements_in_view`.
            let live_iter = self.active_tab().terminal.live_placements().iter().map(|p| {
                (
                    Self::live_placement_viewport_row(p.top_row, view_offset, rows),
                    p,
                )
            });
            let scrollback_iter = scrollback_draws.iter().map(|p| (p.top_row, p));
            let now = std::time::Instant::now();
            for (viewport_row, p) in live_iter.chain(scrollback_iter) {
                // peek_at picks the current animation frame for animated
                // images; for static images it returns the same texture
                // as peek().
                let Some(gpu_img) = self.active_tab().image_store.peek_at(p.image, now) else { continue };
                // pixel_offset shifts the draw inside the anchor cell — phase 2
                // Kitty `X=`/`Y=` plumb through here. Whole-cell math stays
                // identical so eviction / scroll-region shifting is unaffected.
                let x_px = WINDOW_PADDING
                    + (p.left_col as f32) * cell_w
                    + p.pixel_offset.0 as f32;
                let y_px = WINDOW_PADDING
                    + decorator_offset
                    + (viewport_row as f32) * line_height
                    + scroll_y
                    + p.pixel_offset.1 as f32;
                let w_px = (p.cols as f32) * cell_w;
                let h_px = (p.rows as f32) * line_height;
                // src_rect (pixels) → UVs (0..1) against this image's known size.
                // Guard against zero w/h on the GpuImage to avoid div-by-zero —
                // shouldn't happen for a successfully uploaded texture, but cheap.
                let uv_rect = p.src_rect.and_then(|(sx, sy, sw, sh)| {
                    let iw = gpu_img.width_px as f32;
                    let ih = gpu_img.height_px as f32;
                    if iw <= 0.0 || ih <= 0.0 { return None; }
                    Some((
                        sx as f32 / iw,
                        sy as f32 / ih,
                        (sx + sw) as f32 / iw,
                        (sy + sh) as f32 / ih,
                    ))
                });
                image_draws.push(renderer::images::ImageDraw {
                    image: gpu_img,
                    x_px,
                    y_px,
                    w_px,
                    h_px,
                    uv_rect,
                });
            }

            // Kitty virtual placements (U+10EEEE cells). Each run is
            // one horizontal stretch of one source-image row,
            // produced by the per-cell scan in
            // `Terminal::kitty_placeholder_runs`. Drawing per-run
            // (instead of one stretched quad over the merged bbox)
            // means a partial overwrite of the placeholder grid —
            // tmux scrolling new output across the top, a tear-down
            // halfway through the image — visibly clips at the
            // surviving cells instead of distorting the image into
            // whatever shrinking rect remained.
            for run in self.active_tab().terminal.kitty_placeholder_runs() {
                let Some(store_id) = self.active_tab().terminal.kitty_image_id_lookup(run.client_id)
                else {
                    continue;
                };
                let Some((total_cols, total_rows)) =
                    self.active_tab().terminal.kitty_image_cell_extent(run.client_id)
                else {
                    // No `c=`/`r=` on the transmission — no honest
                    // UV denominator. Skip rather than guess.
                    continue;
                };
                let Some(gpu_img) = self.active_tab().image_store.peek_at(store_id, now) else { continue };
                // `run.screen_row` is already in the same visual-row
                // frame `update_vertices` uses (the scanner walks
                // `extended_cell(-2..rows+2, ..)`), so plug it
                // straight into the cell row→pixel math. No
                // `view_offset` shift needed.
                let cells_wide = (run.screen_col_end - run.screen_col_start) as f32;
                let x_px = WINDOW_PADDING + (run.screen_col_start as f32) * cell_w;
                let y_px = WINDOW_PADDING
                    + decorator_offset
                    + (run.screen_row as f32) * line_height
                    + scroll_y;
                let w_px = cells_wide * cell_w;
                let h_px = line_height;
                let uv = Self::placeholder_run_uv(
                    run.image_col_start,
                    run.image_col_end,
                    run.image_row,
                    total_cols,
                    total_rows,
                );
                image_draws.push(renderer::images::ImageDraw {
                    image: gpu_img,
                    x_px,
                    y_px,
                    w_px,
                    h_px,
                    uv_rect: Some(uv),
                });
            }
        }
        let has_images = !image_draws.is_empty();

        let grid_pipeline = if self.wireframe {
            self.shared.wireframe_pipeline.as_ref().unwrap_or(&self.shared.render_pipeline)
        } else {
            &self.shared.render_pipeline
        };

        // Helper to issue a draw of part of the cell vertex buffer into
        // `target`, with the given load op. Sharing the body keeps the
        // three paths' grid draws byte-for-byte identical aside from
        // load/store and index range.
        let draw_grid = |encoder: &mut wgpu::CommandEncoder,
                         target: &wgpu::TextureView,
                         load: wgpu::LoadOp<wgpu::Color>,
                         range: std::ops::Range<u32>,
                         label: &str| {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some(label),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target,
                    resolve_target: None,
                    ops: wgpu::Operations { load, store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: None,
                occlusion_query_set: None,
                timestamp_writes: None,
            });
            pass.set_pipeline(grid_pipeline);
            pass.set_bind_group(0, &self.font_bind_group, &[]);
            pass.set_bind_group(1, &self.camera_bind_group, &[]);
            pass.set_bind_group(2, &self.fade_bind_group, &[]);
            pass.set_vertex_buffer(0, self.vertex_buffer.slice(..));
            pass.set_index_buffer(self.index_buffer.slice(..), wgpu::IndexFormat::Uint32);
            if range.start < range.end {
                pass.draw_indexed(range, 0, 0..1);
            }
        };

        if !needs_offscreen {
            // Fast path. When no image is on screen we issue the original
            // single grid pass; when one is, we split into bg + image + fg
            // so glyphs land on top of the image. The one-extra-pass cost
            // is paid only on frames that actually draw images.
            if has_images {
                draw_grid(
                    &mut encoder,
                    &view,
                    wgpu::LoadOp::Clear(clear),
                    bg_index_range.clone(),
                    "scene bg pass (fast+img)",
                );
                self.image_pipeline.render(
                    &mut encoder,
                    &self.shared.gpu.queue,
                    &self.camera_bind_group,
                    &view,
                    wgpu::LoadOp::Load,
                    &image_draws,
                );
                draw_grid(
                    &mut encoder,
                    &view,
                    wgpu::LoadOp::Load,
                    fg_index_range.clone(),
                    "scene fg pass (fast+img)",
                );
            } else {
                draw_grid(
                    &mut encoder,
                    &view,
                    wgpu::LoadOp::Clear(clear),
                    0..self.num_indices,
                    "scene pass",
                );
            }
            // Content overlay, fast path: separate render pass on the
            // swapchain with LoadOp::Load so it darkens what we just
            // drew. Only entered when scanlines are enabled but glow
            // and strips are off.
            if content_overlay_on {
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("scanline overlay pass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Load,
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    occlusion_query_set: None,
                    timestamp_writes: None,
                });
                pass.set_pipeline(&self.shared.glow_pipelines.scanline_overlay_pipeline);
                pass.set_bind_group(0, &self.glow.composite_bg, &[]);
                pass.draw(0..3, 0..1);
            }
        } else if glow_on {
            // Layered path. Pass 1: bg quads → bg scene.
            draw_grid(
                &mut encoder,
                &self.blur.scene.view,
                wgpu::LoadOp::Clear(clear),
                bg_index_range,
                "scene bg pass",
            );
            // Pass 1b: image placements → bg scene (composite over bg
            // cells). Putting images in `blur.scene` means glow and edge
            // blur treat them as scene content; fg glyphs that overlap
            // an image will still composite on top via the next pass.
            if has_images {
                self.image_pipeline.render(
                    &mut encoder,
                    &self.shared.gpu.queue,
                    &self.camera_bind_group,
                    &self.blur.scene.view,
                    wgpu::LoadOp::Load,
                    &image_draws,
                );
            }
            // Pass 2: fg quads → fg scene. Transparent clear so anything
            // the fg layer doesn't touch shows the bg layer through.
            draw_grid(
                &mut encoder,
                &self.scene_fg.view,
                wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                fg_index_range,
                "scene fg pass",
            );
            // Pass 3: glow each layer against its own un-blurred scene
            // so the bright pass extracts crisp colour, not post-blur smear.
            self.glow.run(&mut encoder, &self.shared.glow_pipelines);
            self.glow_fg.run(&mut encoder, &self.shared.glow_pipelines);
            // Strip blur still samples the bg scene — strips live near
            // the window edges where there's rarely text, so a bg-only
            // blur source reads close to the legacy combined-scene blur.
            if needs_strips {
                self.blur.run(&mut encoder, &self.shared.blur_pipelines);
            }

            // Pass 4: composite to swapchain. Order is bg → bg glow →
            // fg → fg glow → strips. Strips stay on top so the toolbar
            // / edge fade reads cleanly over everything.
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("composite pass (layered)"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(clear),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                occlusion_query_set: None,
                timestamp_writes: None,
            });

            // bg scene (opaque blit).
            pass.set_pipeline(&self.shared.blur_pipelines.blit_pipeline);
            pass.set_bind_group(0, self.blur.blit_bind_group(), &[]);
            pass.draw(0..3, 0..1);

            // bg glow (alpha-blended, masked by bg). Mask suppresses the
            // halo over colored cells so the bg's own pixels aren't
            // re-tinted by their bloom; the halo still appears in
            // transparent areas adjacent to colored cells.
            pass.set_pipeline(&self.shared.glow_pipelines.composite_masked_pipeline);
            pass.set_bind_group(0, &self.glow.composite_bg, &[]);
            pass.set_bind_group(1, &self.glow_bg_mask, &[]);
            pass.draw(0..3, 0..1);

            // fg scene (alpha-blended on top of bg + bg glow).
            pass.set_pipeline(&self.shared.blur_pipelines.blit_alpha_pipeline);
            pass.set_bind_group(0, &self.scene_fg_blit_bg, &[]);
            pass.draw(0..3, 0..1);

            // fg glow (alpha-blended, masked by bg). Without the mask
            // the bloom paints over adjacent cells' colored backgrounds
            // and visually shifts them; this keeps the halo only in
            // areas where bg is transparent.
            pass.set_pipeline(&self.shared.glow_pipelines.composite_masked_pipeline);
            pass.set_bind_group(0, &self.glow_fg.composite_bg, &[]);
            pass.set_bind_group(1, &self.glow_fg_mask, &[]);
            pass.draw(0..3, 0..1);

            // Content scanlines: multiply-blend overlay across bg + glow
            // + fg + fg glow. Drawn before strips so the edge fades
            // aren't darkened (they're UI, not content). The masked
            // variant additionally fades the overlay to identity where
            // the bg scene matches the window's primary background
            // colour — scanlines disappear over empty areas.
            if content_overlay_on {
                if effective_skip_primary_bg(&self.config, &palette::get().glow) {
                    // Layered path has both scene textures — mask
                    // samples bg + fg so glyphs on default-bg cells
                    // still get scanlines.
                    pass.set_pipeline(&self.shared.glow_pipelines.scanline_overlay_masked_pipeline);
                    pass.set_bind_group(0, &self.glow.composite_bg, &[]);
                    pass.set_bind_group(1, &self.scanline_overlay_mask, &[]);
                } else {
                    pass.set_pipeline(&self.shared.glow_pipelines.scanline_overlay_pipeline);
                    pass.set_bind_group(0, &self.glow.composite_bg, &[]);
                }
                pass.draw(0..3, 0..1);
            }

            if needs_strips {
                pass.set_pipeline(&self.shared.blur_pipelines.strip_pipeline);
                pass.set_bind_group(0, &self.blur.strip_blur_bg, &[]);
                pass.set_bind_group(1, &self.camera_bind_group, &[]);
                pass.set_bind_group(2, &self.blur.strip_uniform_bg, &[]);
                pass.set_vertex_buffer(0, self.strip_vertex_buffer.slice(..));
                pass.set_index_buffer(
                    self.strip_index_buffer.slice(..),
                    wgpu::IndexFormat::Uint16,
                );
                pass.draw_indexed(0..self.num_strip_indices, 0, 0..1);
            }
        } else {
            // Strip-only path: legacy single-scene render + blur + composite.
            // When images are on screen we split the grid pass into bg + fg
            // so the image quads land between them — same trick as the fast
            // path. Otherwise we keep the original single-pass behaviour.
            if has_images {
                draw_grid(
                    &mut encoder,
                    &self.blur.scene.view,
                    wgpu::LoadOp::Clear(clear),
                    bg_index_range,
                    "scene bg pass (strip+img)",
                );
                self.image_pipeline.render(
                    &mut encoder,
                    &self.shared.gpu.queue,
                    &self.camera_bind_group,
                    &self.blur.scene.view,
                    wgpu::LoadOp::Load,
                    &image_draws,
                );
                draw_grid(
                    &mut encoder,
                    &self.blur.scene.view,
                    wgpu::LoadOp::Load,
                    fg_index_range,
                    "scene fg pass (strip+img)",
                );
            } else {
                draw_grid(
                    &mut encoder,
                    &self.blur.scene.view,
                    wgpu::LoadOp::Clear(clear),
                    0..self.num_indices,
                    "scene pass",
                );
            }
            self.blur.run(&mut encoder, &self.shared.blur_pipelines);

            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("composite pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(clear),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                occlusion_query_set: None,
                timestamp_writes: None,
            });

            pass.set_pipeline(&self.shared.blur_pipelines.blit_pipeline);
            pass.set_bind_group(0, self.blur.blit_bind_group(), &[]);
            pass.draw(0..3, 0..1);

            // Content scanlines, strip-only path: overlay between scene
            // blit and the strip quads. Always unmasked here because
            // the layered fg scene is stale in this path — the masked
            // variant would suppress scanlines based on outdated fg
            // content, producing artifacts. `skip_primary_bg` therefore
            // only takes effect in the layered (glow-on) path.
            if content_overlay_on {
                pass.set_pipeline(&self.shared.glow_pipelines.scanline_overlay_pipeline);
                pass.set_bind_group(0, &self.glow.composite_bg, &[]);
                pass.draw(0..3, 0..1);
            }

            pass.set_pipeline(&self.shared.blur_pipelines.strip_pipeline);
            pass.set_bind_group(0, &self.blur.strip_blur_bg, &[]);
            pass.set_bind_group(1, &self.camera_bind_group, &[]);
            pass.set_bind_group(2, &self.blur.strip_uniform_bg, &[]);
            pass.set_vertex_buffer(0, self.strip_vertex_buffer.slice(..));
            pass.set_index_buffer(
                self.strip_index_buffer.slice(..),
                wgpu::IndexFormat::Uint16,
            );
            pass.draw_indexed(0..self.num_strip_indices, 0, 0..1);
        }

        self.shared.gpu.queue.submit(std::iter::once(encoder.finish()));
        output.present();

        Ok((surface_wait, !needs_offscreen))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A representative line height; the band is invariant in its absolute
    // value as long as `scroll_y` is expressed in the same pixel units.
    const LH: f32 = 20.0;
    const ROWS: usize = 24;

    #[test]
    fn zero_offset_is_exactly_visible_grid_plus_two() {
        // No sub-line scroll → no phantom rows beyond the fixed ±2.
        let (r_lo, r_hi) = WindowState::phantom_row_band(0.0, ROWS, LH);
        assert_eq!(r_lo, -2);
        assert_eq!(r_hi, ROWS as isize + 2);
    }

    #[test]
    fn sub_line_scroll_down_extends_band_at_bottom() {
        // Scrolling down (negative scroll_y) slides a row in from the bottom
        // edge. The band widens by `ceil(|offset|)` == 1 row; the bottom
        // gaining that row (r_hi == rows+3) is what fixes the snap. The band is
        // sign-agnostic (driven by |scroll_y|), so the top widens to -3 too.
        let (r_lo, r_hi) = WindowState::phantom_row_band(-0.5 * LH, ROWS, LH);
        assert_eq!(r_hi, ROWS as isize + 3);
        assert_eq!(r_lo, -3);
    }

    #[test]
    fn sub_line_scroll_up_extends_band_at_top() {
        // Scrolling up (positive scroll_y) slides a row in from the top edge.
        // Same one-row widening, top edge in focus (r_lo == -3); symmetric at
        // the bottom (rows+3).
        let (r_lo, r_hi) = WindowState::phantom_row_band(0.5 * LH, ROWS, LH);
        assert_eq!(r_lo, -3);
        assert_eq!(r_hi, ROWS as isize + 3);
    }

    #[test]
    fn multi_line_alt_screen_slide_ceils_offset_in_rows() {
        // An alt-screen slide where scroll_y spans 2.5 lines → ceil(2.5) == 3
        // extra rows on each side. The band is symmetric because the offset
        // magnitude, not its sign, sets the width.
        let (r_lo, r_hi) = WindowState::phantom_row_band(2.5 * LH, ROWS, LH);
        assert_eq!(r_lo, -2 - 3);
        assert_eq!(r_hi, ROWS as isize + 2 + 3);
    }
}
