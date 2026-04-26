extern crate freetype as ft;
use std::collections::HashMap;

use crate::box_drawing;

pub struct Glyph {
    pub bitmap: ft::Bitmap,
    pub metrics: ft::GlyphMetrics,
}

// Style variant of a face. The numeric value is also a (bold,italic) bitmask
// (bit 0 = bold, bit 1 = italic) and indexes into Font::variants /
// Atlas::variants.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum FaceVariant {
    Regular = 0,
    Bold = 1,
    Italic = 2,
    BoldItalic = 3,
}

impl FaceVariant {
    pub fn from_flags(bold: bool, italic: bool) -> Self {
        match (bold, italic) {
            (false, false) => FaceVariant::Regular,
            (true, false) => FaceVariant::Bold,
            (false, true) => FaceVariant::Italic,
            (true, true) => FaceVariant::BoldItalic,
        }
    }

    pub const ALL: [FaceVariant; 4] = [
        FaceVariant::Regular,
        FaceVariant::Bold,
        FaceVariant::Italic,
        FaceVariant::BoldItalic,
    ];
}

// One installed style. Regular's `face` is required; the others are optional —
// when the styled face isn't installed, atlas lookups fall back to Regular.
pub struct Variant {
    pub face: Option<ft::Face>,
    pub fallbacks: Vec<ft::Face>,
}

impl Variant {
    fn empty() -> Self {
        Self { face: None, fallbacks: Vec::new() }
    }

    fn faces(&self) -> impl Iterator<Item = &ft::Face> {
        self.face.iter().chain(self.fallbacks.iter())
    }

    fn face_for(&self, ch: char) -> Option<&ft::Face> {
        self.faces().find(|f| f.get_char_index(ch as usize).is_ok())
    }
}

pub struct Font {
    pub variants: [Variant; 4],
}

pub struct Atlas {
    pub width: usize,
    pub height: usize,
    pub buffer: Vec<u8>,
    // One glyph map per variant. Bold/italic/bold-italic maps are sparse — a
    // miss falls back to the Regular variant (and finally to `notdef`).
    pub variants: [HashMap<char, AtlasEntry>; 4],
    // The font's .notdef glyph (usually a hollow box). Rendered in place of
    // any character the font doesn't provide, so missing glyphs are visibly
    // "tofu" rather than invisible.
    pub notdef: AtlasEntry,
}

impl Atlas {
    pub fn lookup(&self, ch: char, variant: FaceVariant) -> &AtlasEntry {
        let i = variant as usize;
        if i != 0 {
            if let Some(e) = self.variants[i].get(&ch) {
                return e;
            }
        }
        self.variants[0].get(&ch).unwrap_or(&self.notdef)
    }
}

#[derive(Copy, Clone)]
pub struct AtlasEntry {
    pub x: usize,
    pub y: usize,
    pub width: usize,
    pub height: usize,
    pub bearing_y: isize,
    pub bearing_x: isize,
    pub advance_x: usize,
}

impl Font {
    pub fn new(data: Vec<u8>) -> Self {
        let library = ft::Library::init().unwrap();
        let face = library.new_memory_face(data, 0).unwrap();
        let mut variants = [
            Variant::empty(),
            Variant::empty(),
            Variant::empty(),
            Variant::empty(),
        ];
        variants[FaceVariant::Regular as usize].face = Some(face);
        Font { variants }
    }

    // Install the primary face for a styled variant (Bold/Italic/BoldItalic).
    // Returns false if the data can't be opened or sized at the current
    // `set_char_size` — typically a bitmap-only face we can't currently render.
    pub fn set_variant(
        &mut self,
        variant: FaceVariant,
        data: Vec<u8>,
        height_points: f32,
        dpi: u32,
    ) -> bool {
        if variant == FaceVariant::Regular {
            return false;
        }
        let library = match ft::Library::init() {
            Ok(l) => l,
            Err(_) => return false,
        };
        let face = match library.new_memory_face(data, 0) {
            Ok(f) => f,
            Err(_) => return false,
        };
        let size = (height_points * 64.0) as isize;
        if face.set_char_size(size, 0, dpi, dpi).is_err() {
            return false;
        }
        self.variants[variant as usize].face = Some(face);
        true
    }

    // Append a fallback face onto the given variant. Returns false (and drops
    // the data) if the font can't be sized to the current `set_char_size` —
    // typically a bitmap-only face like Apple Color Emoji that we can't
    // currently render anyway.
    pub fn add_fallback(
        &mut self,
        variant: FaceVariant,
        data: Vec<u8>,
        height_points: f32,
        dpi: u32,
    ) -> bool {
        let library = match ft::Library::init() {
            Ok(l) => l,
            Err(_) => return false,
        };
        let face = match library.new_memory_face(data, 0) {
            Ok(f) => f,
            Err(_) => return false,
        };
        let size = (height_points * 64.0) as isize;
        if face.set_char_size(size, 0, dpi, dpi).is_err() {
            return false;
        }
        self.variants[variant as usize].fallbacks.push(face);
        true
    }

    // The regular face — exposed so call sites can read size metrics for grid
    // layout. All variants are sized identically, so metrics from Regular
    // describe every variant's cell.
    pub fn face(&self) -> &ft::Face {
        self.variants[FaceVariant::Regular as usize]
            .face
            .as_ref()
            .expect("regular face missing")
    }

    fn regular_face_mut(&mut self) -> &mut ft::Face {
        self.variants[FaceVariant::Regular as usize]
            .face
            .as_mut()
            .expect("regular face missing")
    }

    pub fn load_glyph(&mut self, character: char) -> Glyph {
        let face = self.regular_face_mut();
        face.load_char(character as usize, ft::face::LoadFlag::RENDER).unwrap();
        let glyph = face.glyph();
        Glyph { metrics: glyph.metrics(), bitmap: glyph.bitmap() }
    }

    // Width of a representative ASCII cell. For monospace fonts that ship
    // both half-width Latin and full-width CJK/symbol glyphs (e.g. Iosevka),
    // `size_metrics().max_advance` returns the *wide* cell, leaving Latin
    // text with a column of empty space after every glyph. Sampling 'M'
    // gives us the half-width advance the user actually expects.
    pub fn cell_width(&self) -> usize {
        let face = self.face();
        face.load_char('M' as usize, ft::face::LoadFlag::DEFAULT)
            .unwrap();
        (face.glyph().metrics().horiAdvance >> 6) as usize
    }

    pub fn set_char_size(&mut self, height_points: f32, dpi: u32) {
        let size = (height_points * 64.0) as isize;
        println!("size: {}", size);
        for variant in &mut self.variants {
            if let Some(face) = variant.face.as_mut() {
                face.set_char_size(size, 0, dpi, dpi).unwrap();
            }
            for face in &mut variant.fallbacks {
                let _ = face.set_char_size(size, 0, dpi, dpi);
            }
        }
    }

    pub fn build_atlas(&mut self) -> Atlas {
        let size = 4096;
        let width = size;
        let height = size;
        let mut row_height = 0;
        let mut texture: Vec<u8> = vec![0; width * height];
        // Reserve a 2x2 fully-opaque texel block at the top-left for solid
        // quads (cell backgrounds, cursor, fade). Pack glyphs starting at
        // x=2 so .notdef can't overwrite it — the fragment shader multiplies
        // vertex color by this sample, and a sub-1.0 sample causes solid
        // quads to render translucent and let underlying text bleed through.
        texture[0] = 255;
        texture[1] = 255;
        texture[width] = 255;
        texture[width + 1] = 255;
        let mut x = 2;
        let mut y = 0;

        let cell_w = self.cell_width();
        let metrics = self
            .face()
            .size_metrics()
            .expect("primary face has no size metrics");
        let cell_h = ((metrics.ascender - metrics.descender) >> 6) as usize;
        let ascender_px = (metrics.ascender >> 6) as isize;

        // The font's .notdef glyph (index 0). Rendered in place of any
        // character the font doesn't provide — usually a hollow box.
        self.face()
            .load_glyph(0, ft::face::LoadFlag::RENDER)
            .expect("font has no .notdef glyph");
        let notdef = pack_glyph(
            self.face().glyph(),
            &mut texture,
            width,
            size,
            &mut x,
            &mut y,
            &mut row_height,
            cell_w,
            cell_h,
        );

        let mut variants_entries: [HashMap<char, AtlasEntry>; 4] = [
            HashMap::with_capacity(4096),
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
        ];

        // Ranges we care about rendering. Control chars are excluded — the
        // terminal model strips them before they ever reach a cell. PUA is
        // included to cover Powerline + Nerd Font prompt glyphs.
        let ranges: &[std::ops::RangeInclusive<u32>] = &[
            0x0020..=0x007E, // printable ASCII
            0x00A0..=0x00FF, // Latin-1 supplement printable
            0x0100..=0x024F, // Latin Extended-A/B
            0x2000..=0x27BF, // punctuation, arrows, math, box-drawing, shapes, dingbats
            0x2900..=0x29FF, // supplemental arrows + math
            0xE000..=0xE0FF, // PUA: Powerline, common Nerd Font separators
        ];

        for variant in FaceVariant::ALL {
            let vi = variant as usize;
            if self.variants[vi].face.is_none() {
                continue;
            }
            for range in ranges {
                for c in range.clone() {
                    let ch = match char::from_u32(c) {
                        Some(ch) => ch,
                        None => continue,
                    };
                    // Box-drawing & block-element ranges are synthesized so
                    // strokes land on integer pixel boundaries and connecting
                    // glyphs align across cell boundaries with no half-alpha
                    // hairlines. Synthesize once into Regular only; bold/
                    // italic lookups for these codepoints fall back through
                    // Atlas::lookup to the regular synthesized entry.
                    if variant == FaceVariant::Regular {
                        if let Some(bm) = box_drawing::synth(ch, cell_w, cell_h, ascender_px) {
                            let entry = pack_synth(
                                &bm,
                                &mut texture,
                                width,
                                size,
                                &mut x,
                                &mut y,
                                &mut row_height,
                            );
                            variants_entries[vi].insert(ch, entry);
                            continue;
                        }
                    } else if box_drawing::synth(ch, cell_w, cell_h, ascender_px).is_some() {
                        continue;
                    }
                    // Walk primary → fallbacks for this variant only. If the
                    // styled variant has no glyph for a codepoint, leave the
                    // slot unfilled — Atlas::lookup falls back to Regular.
                    let face = match self.variants[vi].face_for(ch) {
                        Some(f) => f,
                        None => continue,
                    };
                    if face.load_char(ch as usize, ft::face::LoadFlag::RENDER).is_err() {
                        continue;
                    }
                    let entry = pack_glyph(
                        face.glyph(),
                        &mut texture,
                        width,
                        size,
                        &mut x,
                        &mut y,
                        &mut row_height,
                        cell_w,
                        cell_h,
                    );
                    variants_entries[vi].insert(ch, entry);
                }
            }
        }

        Atlas {
            buffer: texture,
            width,
            height,
            variants: variants_entries,
            notdef,
        }
    }
}

// Pack a procedurally-generated bitmap into the atlas. Skips the edge-
// hardening logic in `pack_glyph` because synthesized strokes are already
// pixel-aligned at full coverage by construction.
fn pack_synth(
    bm: &box_drawing::Bitmap,
    texture: &mut [u8],
    width: usize,
    size: usize,
    x: &mut usize,
    y: &mut usize,
    row_height: &mut usize,
) -> AtlasEntry {
    let w = bm.width;
    let h = bm.height;
    if h > *row_height {
        *row_height = h;
    }
    for p in 0..h {
        for q in 0..w {
            texture[((p + *y) % size) * width + (q + *x) % size] = bm.data[p * w + q];
        }
    }
    let entry = AtlasEntry {
        x: *x,
        y: *y,
        width: w,
        height: h,
        bearing_x: bm.bearing_x,
        bearing_y: bm.bearing_y,
        advance_x: bm.advance_x,
    };
    *x += bm.advance_x.max(1);
    if *x + w >= size {
        *x = 0;
        *y += *row_height;
        *row_height = 0;
    }
    entry
}

fn pack_glyph(
    glyph: &ft::GlyphSlot,
    texture: &mut [u8],
    width: usize,
    size: usize,
    x: &mut usize,
    y: &mut usize,
    row_height: &mut usize,
    cell_w: usize,
    cell_h: usize,
) -> AtlasEntry {
    let bitmap = glyph.bitmap();
    let metrics = glyph.metrics();
    let w = bitmap.width() as usize;
    let h = bitmap.rows() as usize;
    let advance = (metrics.horiAdvance >> 6) as usize;
    if h > *row_height {
        *row_height = h;
    }

    // Cell-filling glyphs (Powerline caps, box-drawing, half-blocks) often
    // land on the integer pixel grid with a sub-pixel-covered boundary
    // column or row — alpha ~128 instead of 255. When we stretch the bitmap
    // to the cell extent the linear sampler still emits a partially
    // transparent edge there, letting the cell BG bleed through as a
    // hairline. Detect filling per axis (▐/▌/▀/▄ fill on only one axis)
    // and, when an edge of a filling axis has a significant non-zero
    // average (a structural edge, not the empty flank of a curve), promote
    // that edge's pixels to fully opaque.
    let fills_h = w * 100 >= cell_w * 85;
    let fills_v = h * 100 >= cell_h * 85;
    let src = bitmap.buffer();
    let edge_avg = |stride: usize, start: usize, len: usize| -> u32 {
        if len == 0 { return 0; }
        let mut sum: u32 = 0;
        for k in 0..len {
            sum += src[start + k * stride] as u32;
        }
        sum / len as u32
    };
    let harden_top = fills_v && w > 0 && edge_avg(1, 0, w) > 100;
    let harden_bottom = fills_v && w > 0 && h > 0 && edge_avg(1, (h - 1) * w, w) > 100;
    let harden_left = fills_h && h > 0 && edge_avg(w, 0, h) > 100;
    let harden_right = fills_h && h > 0 && w > 0 && edge_avg(w, w - 1, h) > 100;
    for p in 0..h {
        for q in 0..w {
            let mut value = src[p * w + q];
            // Promote any non-zero pixel on a structurally-solid edge to fully
            // opaque so the stretched bitmap reaches the cell boundary at full
            // coverage instead of leaving a half-alpha hairline.
            let on_solid_edge = (harden_top && p == 0)
                || (harden_bottom && p == h - 1)
                || (harden_left && q == 0)
                || (harden_right && q == w - 1);
            if on_solid_edge && value > 0 {
                value = 255;
            }
            texture[((p + *y) % size) * width + (q + *x) % size] = value;
        }
    }
    // Use the post-rasterization pixel offsets (bitmap_left/top) rather than
    // the design metrics shifted right by 6. The metrics are in 26.6 fixed
    // point and `>> 6` floors the fractional part, so the stored bearing can
    // disagree by one pixel with where FreeType actually placed the bitmap on
    // the integer pixel grid. That offset shows up as a 1-px sliver of cell
    // background bleeding through the top/left edge of cell-filling glyphs
    // (Powerline/Nerd Font caps, box-drawing characters).
    let entry = AtlasEntry {
        x: *x,
        y: *y,
        width: w,
        height: h,
        bearing_x: glyph.bitmap_left() as isize,
        advance_x: advance,
        bearing_y: glyph.bitmap_top() as isize,
    };
    *x += advance.max(1);
    if *x + w >= size {
        *x = 0;
        *y += *row_height;
        *row_height = 0;
    }
    entry
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(tag: usize) -> AtlasEntry {
        AtlasEntry {
            x: tag,
            y: 0,
            width: 1,
            height: 1,
            bearing_x: 0,
            bearing_y: 0,
            advance_x: 1,
        }
    }

    fn atlas_with(reg: &[char], bold: &[char]) -> Atlas {
        let mut variants: [HashMap<char, AtlasEntry>; 4] = [
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
        ];
        for &c in reg {
            variants[FaceVariant::Regular as usize].insert(c, entry(0));
        }
        for &c in bold {
            variants[FaceVariant::Bold as usize].insert(c, entry(1));
        }
        Atlas {
            width: 0,
            height: 0,
            buffer: Vec::new(),
            variants,
            notdef: entry(99),
        }
    }

    #[test]
    fn from_flags_covers_all_combos() {
        assert_eq!(FaceVariant::from_flags(false, false), FaceVariant::Regular);
        assert_eq!(FaceVariant::from_flags(true, false), FaceVariant::Bold);
        assert_eq!(FaceVariant::from_flags(false, true), FaceVariant::Italic);
        assert_eq!(FaceVariant::from_flags(true, true), FaceVariant::BoldItalic);
    }

    #[test]
    fn lookup_uses_styled_variant_when_present() {
        let a = atlas_with(&['a'], &['a']);
        let g = a.lookup('a', FaceVariant::Bold);
        assert_eq!(g.x, 1, "should hit the bold map");
    }

    #[test]
    fn lookup_falls_back_to_regular_when_styled_misses() {
        // Bold map has no 'a'; regular does. Bold lookup should return regular.
        let a = atlas_with(&['a'], &[]);
        let g = a.lookup('a', FaceVariant::Bold);
        assert_eq!(g.x, 0, "should fall back to regular");
    }

    #[test]
    fn lookup_falls_back_to_notdef_when_neither_has_it() {
        let a = atlas_with(&[], &[]);
        let g = a.lookup('a', FaceVariant::Italic);
        assert_eq!(g.x, 99, "should fall back to notdef");
    }

    #[test]
    fn regular_lookup_skips_variant_check() {
        // Regular variant stays sparse; lookup on Regular shouldn't accidentally
        // peek at other maps.
        let a = atlas_with(&[], &['a']);
        let g = a.lookup('a', FaceVariant::Regular);
        assert_eq!(g.x, 99, "regular missing → notdef, not bold map");
    }
}
