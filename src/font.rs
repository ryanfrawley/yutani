extern crate freetype as ft;
use std::collections::{HashMap, HashSet};

use crate::box_drawing;

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

    // Used by `find_face_index` (non-macOS face selection) and the tests;
    // macOS builds reach faces through `get_strict` instead, so the method
    // is dead there but must stay for other platforms.
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    pub fn flags(self) -> (bool, bool) {
        match self {
            FaceVariant::Regular => (false, false),
            FaceVariant::Bold => (true, false),
            FaceVariant::Italic => (false, true),
            FaceVariant::BoldItalic => (true, true),
        }
    }

    pub const ALL: [FaceVariant; 4] = [
        FaceVariant::Regular,
        FaceVariant::Bold,
        FaceVariant::Italic,
        FaceVariant::BoldItalic,
    ];
}

/// Find the face index inside a (potentially packed) font file whose
/// style flags match `variant`. Iosevka and similar fonts ship as TTC
/// collections that pack multiple weights / italics in a single file;
/// `new_memory_face(data, 0)` always lands on the *first* face inside
/// the collection (typically the regular face), so loading the italic
/// or bold-italic cut needs the right index.
///
/// Returns `0` as a conservative fallback when:
///   - the FreeType library fails to open the data,
///   - the file is a single-face TTF (one trip through the loop and
///     no other choice),
///   - none of the packed faces' style flags match the requested
///     combination (a discrepancy between Core Text's view of the
///     family and what's actually in the file — preferring face 0
///     means we at least render *something*, which beats a hard
///     failure to draw any glyph for that style).
///
/// `data` is passed by reference; we open throwaway faces just to
/// inspect metadata and never keep them — the real face is opened
/// later by the caller using the index returned here.
///
/// Only the non-macOS `load_family_styled` path calls this; on macOS
/// `get_strict` already returns the right face index, so the function is
/// dead there but stays for the other platforms.
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub fn find_face_index(data: &[u8], variant: FaceVariant) -> isize {
    let (want_bold, want_italic) = variant.flags();
    let Ok(library) = ft::Library::init() else {
        return 0;
    };
    // Probe face 0 to learn the collection's face count. We need a
    // fresh copy of the data per probe because freetype-rs takes
    // `Vec<u8>` by value.
    let probe = match library.new_memory_face(data.to_vec(), 0) {
        Ok(f) => f,
        Err(_) => return 0,
    };
    let n = probe.num_faces();
    if n <= 1 {
        return 0;
    }
    for i in 0..n as isize {
        let face = match library.new_memory_face(data.to_vec(), i) {
            Ok(f) => f,
            Err(_) => continue,
        };
        let flags = face.style_flags();
        let bold = flags.contains(ft::face::StyleFlag::BOLD);
        let italic = flags.contains(ft::face::StyleFlag::ITALIC);
        if bold == want_bold && italic == want_italic {
            return i;
        }
    }
    0
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
    // Glyph-id-keyed entries for ligatures (and any other glyphs the
    // shaper produces that aren't in the per-char maps). Same fallback
    // semantics as `variants` — Bold/Italic/BoldItalic maps may miss and
    // Regular is consulted next, otherwise notdef.
    pub ligatures: [HashMap<u32, AtlasEntry>; 4],
    // The font's .notdef glyph (usually a hollow box). Rendered in place of
    // any character the font doesn't provide, so missing glyphs are visibly
    // "tofu" rather than invisible.
    pub notdef: AtlasEntry,
    // Packing cursor — kept around so on-demand glyphs (ligatures
    // discovered at render time) can append to the atlas after the
    // initial pre-pack. Texture re-uploads are gated by `dirty`.
    pack_x: usize,
    pack_y: usize,
    pack_row_height: usize,
    pub dirty: bool,
    // Per-variant set of chars `ensure_char` has already attempted to
    // pack. Distinct from `variants` because a styled-variant attempt
    // may legitimately leave the styled slot empty (the glyph isn't in
    // the styled chain, so Atlas::lookup falls through to Regular).
    // Without this set we'd re-attempt face_for + the surrounding work
    // every frame for any such char.
    tried_chars: [HashSet<char>; 4],
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

    /// Look up a glyph by id (the shaper's output). Falls back through
    /// styled→regular→notdef like `lookup`. Caller must have populated the
    /// entry first via `ensure_glyph_id`.
    pub fn lookup_glyph_id(&self, glyph_id: u32, variant: FaceVariant) -> &AtlasEntry {
        let i = variant as usize;
        if i != 0 {
            if let Some(e) = self.ligatures[i].get(&glyph_id) {
                return e;
            }
        }
        self.ligatures[0].get(&glyph_id).unwrap_or(&self.notdef)
    }

    /// Rasterize and pack a glyph by its font-internal id (not codepoint),
    /// caching the result. No-op when already cached. Sets `dirty` so the
    /// renderer knows to re-upload the texture before the next draw.
    pub fn ensure_glyph_id(
        &mut self,
        font: &mut Font,
        variant: FaceVariant,
        glyph_id: u32,
    ) -> bool {
        let vi = variant as usize;
        if self.ligatures[vi].contains_key(&glyph_id) {
            return true;
        }
        // Compute cell metrics BEFORE loading the glyph. `cell_width`
        // calls `load_char('M', DEFAULT)` on the Regular face, and the
        // Regular variant's face shares its single glyph slot with us
        // when vi == 0 — doing this after `load_glyph(RENDER)` would
        // overwrite the just-rendered bitmap with an unrendered 'M' and
        // leave the FT_Bitmap pointer in an indeterminate state.
        let cell_w = font.cell_width();
        let metrics = font
            .face()
            .size_metrics()
            .expect("primary face has no size metrics");
        let cell_h = ((metrics.ascender - metrics.descender) >> 6) as usize;
        let face = match font.variants[vi].face.as_ref() {
            Some(f) => f,
            None => return false,
        };
        if face
            .load_glyph(glyph_id, ft::face::LoadFlag::RENDER)
            .is_err()
        {
            return false;
        }
        // If the atlas is full, fall back to notdef and remember that — so
        // we don't keep retrying the same glyph on every frame and so the
        // renderer picks up a sane (if blank) UV instead of garbage.
        let entry = pack_glyph(
            face.glyph(),
            &mut self.buffer,
            self.width,
            self.height,
            &mut self.pack_x,
            &mut self.pack_y,
            &mut self.pack_row_height,
            cell_w,
            cell_h,
        )
        .unwrap_or(self.notdef);
        self.ligatures[vi].insert(glyph_id, entry);
        self.dirty = true;
        true
    }

    /// Rasterize and pack a char into the atlas if not already present.
    /// Walks the variant's primary + fallback face chain via
    /// `Variant::face_for`; the first face to provide a glyph wins.
    ///
    /// Build-time pre-pack only covers a fixed set of codepoint ranges
    /// (printable ASCII, common symbols, Powerline PUA), so any char
    /// outside those — Nerd Font icons in SPUA, CJK, arbitrary symbols —
    /// reaches the render path with no atlas entry and would otherwise
    /// fall to `notdef`. This is the on-demand counterpart to
    /// `ensure_glyph_id`, but keyed by codepoint and walking fallbacks.
    ///
    /// Styled variants stay sparse: when the styled chain doesn't carry
    /// the glyph, the slot is left empty and `Atlas::lookup` falls back
    /// to Regular (same shape as `build_atlas`). `tried_chars` records
    /// the attempt so we don't redo the work each frame.
    pub fn ensure_char(&mut self, font: &mut Font, variant: FaceVariant, ch: char) {
        let vi = variant as usize;
        if !self.tried_chars[vi].insert(ch) {
            return;
        }
        // Box-drawing (U+2500..U+257F) and block-element (U+2580..U+259F)
        // codepoints are synthesized into the atlas at build time so they
        // tile edge-to-edge with no half-alpha hairlines (see `build_atlas`
        // and `box_drawing::synth`). The installed font usually ALSO provides
        // these glyphs, but its versions are trimmed to their ink bbox and
        // bear into the cell — packing one here would overwrite the
        // synthesized entry and bring the seams back (e.g. ▐ rendered as a
        // font glyph is a half-width bitmap that leaves a gap against ▛).
        // Leave the synthesized entry in place for every variant; bold/italic
        // lookups fall back to the Regular synth via `Atlas::lookup`.
        if (0x2500..=0x259F).contains(&(ch as u32)) {
            return;
        }
        // Styled variant with no primary face: any rendering for this
        // char will fall through to Regular via Atlas::lookup, so make
        // sure Regular is the one that gets ensured.
        if vi != 0 && font.variants[vi].face.is_none() {
            self.ensure_char(font, FaceVariant::Regular, ch);
            return;
        }
        // Compute cell metrics BEFORE loading the glyph — `cell_width`
        // calls `load_char('M', DEFAULT)` on the Regular face and the
        // shared FT_GlyphSlot would otherwise overwrite the bitmap we
        // just rendered. Same dance as `ensure_glyph_id`.
        let cell_w = font.cell_width();
        let metrics = font
            .face()
            .size_metrics()
            .expect("primary face has no size metrics");
        let cell_h = ((metrics.ascender - metrics.descender) >> 6) as usize;

        let face = match font.variants[vi].face_for(ch) {
            Some(f) => f,
            None => {
                // No face in this variant's chain has the glyph. For a
                // styled variant, ensure Regular is also tried so its
                // fallback chain (which can differ — Apple Symbols ships
                // no bold cut, for example) gets a chance. For Regular,
                // leave the slot empty; `Atlas::lookup` returns `notdef`.
                if vi != 0 {
                    self.ensure_char(font, FaceVariant::Regular, ch);
                }
                return;
            }
        };
        if face
            .load_char(ch as usize, ft::face::LoadFlag::RENDER)
            .is_err()
        {
            if vi != 0 {
                self.ensure_char(font, FaceVariant::Regular, ch);
            }
            return;
        }
        let Some(entry) = pack_glyph(
            face.glyph(),
            &mut self.buffer,
            self.width,
            self.height,
            &mut self.pack_x,
            &mut self.pack_y,
            &mut self.pack_row_height,
            cell_w,
            cell_h,
        ) else {
            // Atlas is full. We've already marked `tried`, so no retry
            // storm; `Atlas::lookup` will return `notdef` for this char.
            return;
        };
        self.variants[vi].insert(ch, entry);
        self.dirty = true;
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
    //
    // `face_index` selects which face inside a TTC collection to open.
    // Iosevka and other large families ship as TTCs that pack multiple
    // styled cuts into one file; loading face 0 always lands on the
    // regular face, so the caller must pass the index that matches the
    // requested variant. Use [`find_face_index`] to compute it.
    pub fn set_variant(
        &mut self,
        variant: FaceVariant,
        data: Vec<u8>,
        face_index: isize,
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
        let face = match library.new_memory_face(data, face_index) {
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
    //
    // See [`Font::set_variant`] for the `face_index` story; same TTC-
    // collection concern applies to fallback fonts too.
    pub fn add_fallback(
        &mut self,
        variant: FaceVariant,
        data: Vec<u8>,
        face_index: isize,
        height_points: f32,
        dpi: u32,
    ) -> bool {
        let library = match ft::Library::init() {
            Ok(l) => l,
            Err(_) => return false,
        };
        let face = match library.new_memory_face(data, face_index) {
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
        let cell_w = self.cell_width();
        let metrics = self
            .face()
            .size_metrics()
            .expect("primary face has no size metrics");
        let cell_h = ((metrics.ascender - metrics.descender) >> 6) as usize;
        let ascender_px = (metrics.ascender >> 6) as isize;

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

        // Pick atlas dimensions from cell metrics so very large fonts don't
        // overflow the texture and start sampling stale glyphs from the top.
        // Estimate area needed as installed_variants × glyphs_per_variant ×
        // (cell_w + gutter) × (cell_h + gutter), with packing slack, then
        // pick the smallest power-of-two ≤ 8192 (wgpu's default
        // `max_texture_dimension_2d`) that fits. 2048 is the lower bound so
        // small fonts still get a comfortable cache.
        let installed_variants = self
            .variants
            .iter()
            .filter(|v| v.face.is_some())
            .count()
            .max(1);
        let glyphs_per_variant: usize = ranges.iter().map(|r| r.clone().count()).sum();
        // +256 for ligatures we'll discover at render time, +1 for notdef.
        let total_glyphs = glyphs_per_variant * installed_variants + 257;
        let cell_footprint = (cell_w + 1) * (cell_h + 1);
        let needed_area = total_glyphs * cell_footprint * 3 / 2;
        let needed_side = (needed_area as f64).sqrt().ceil() as usize;
        // Floor: must hold at least the largest single glyph.
        let min_side = (cell_w + 4).max(cell_h + 4);
        let target = needed_side.max(min_side);
        let mut size: usize = 2048;
        while size < target && size < 8192 {
            size *= 2;
        }
        let size = size.min(8192);

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

        // The font's .notdef glyph (index 0). Rendered in place of any
        // character the font doesn't provide — usually a hollow box. Packed
        // first so it always fits even when the atlas is tight.
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
        )
        .expect("notdef must fit in a freshly-built atlas");

        let mut variants_entries: [HashMap<char, AtlasEntry>; 4] = [
            HashMap::with_capacity(4096),
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
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
                            if let Some(entry) = pack_synth(
                                &bm,
                                &mut texture,
                                width,
                                size,
                                &mut x,
                                &mut y,
                                &mut row_height,
                            ) {
                                variants_entries[vi].insert(ch, entry);
                            }
                            // Atlas full → leave the slot empty so lookup
                            // falls back to notdef instead of writing past
                            // the texture and corrupting earlier glyphs.
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
                    if let Some(entry) = pack_glyph(
                        face.glyph(),
                        &mut texture,
                        width,
                        size,
                        &mut x,
                        &mut y,
                        &mut row_height,
                        cell_w,
                        cell_h,
                    ) {
                        variants_entries[vi].insert(ch, entry);
                    }
                }
            }
        }

        Atlas {
            buffer: texture,
            width,
            height,
            variants: variants_entries,
            ligatures: [
                HashMap::new(),
                HashMap::new(),
                HashMap::new(),
                HashMap::new(),
            ],
            notdef,
            pack_x: x,
            pack_y: y,
            pack_row_height: row_height,
            dirty: false,
            tried_chars: [
                HashSet::new(),
                HashSet::new(),
                HashSet::new(),
                HashSet::new(),
            ],
        }
    }
}

// Pack a procedurally-generated bitmap into the atlas. Skips the edge-
// hardening logic in `pack_glyph` because synthesized strokes are already
// pixel-aligned at full coverage by construction. Returns `None` when the
// glyph won't fit in the remaining atlas space — caller falls back to
// notdef rather than corrupting earlier glyphs by writing past the texture.
fn pack_synth(
    bm: &box_drawing::Bitmap,
    texture: &mut [u8],
    width: usize,
    size: usize,
    x: &mut usize,
    y: &mut usize,
    row_height: &mut usize,
) -> Option<AtlasEntry> {
    let w = bm.width;
    let h = bm.height;
    let stride = bm.advance_x.max(w).max(1) + 1;

    // Wrap to the next row if the glyph won't fit on the current one.
    if *x + stride > width {
        *x = 0;
        *y += *row_height + 1;
        *row_height = 0;
    }
    // Atlas full vertically — caller must fall back. Don't write anything.
    if *y + h > size {
        return None;
    }
    if h > *row_height {
        *row_height = h;
    }
    for p in 0..h {
        for q in 0..w {
            texture[(p + *y) * width + (q + *x)] = bm.data[p * w + q];
        }
    }
    let entry = AtlasEntry {
        x: *x,
        y: *y,
        width: w,
        height: h,
        bearing_x: bm.bearing_x,
        bearing_y: bm.bearing_y,
    };
    *x += stride;
    Some(entry)
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
) -> Option<AtlasEntry> {
    let bitmap = glyph.bitmap();
    let metrics = glyph.metrics();
    let w = bitmap.width() as usize;
    let h = bitmap.rows() as usize;
    let advance = (metrics.horiAdvance >> 6) as usize;
    let stride = w.max(advance).max(1) + 1;

    // Wrap to the next row if this glyph won't fit on the current one. Done
    // before the row-height bump so we never grow the row past the wrap.
    if *x + stride > width {
        *x = 0;
        *y += *row_height + 1;
        *row_height = 0;
    }
    // Atlas is full vertically. Returning None lets the caller fall back to
    // notdef instead of writing past the texture (the previous modular
    // wrap silently sampled stale glyphs from the top of the atlas, which
    // showed up as garbled text at large font sizes).
    if *y + h > size {
        return None;
    }
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
            texture[(p + *y) * width + (q + *x)] = value;
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
        bearing_y: glyph.bitmap_top() as isize,
    };
    // Step the atlas cursor past the bitmap's full footprint, not just the
    // typographic advance. Programming-ligature substitutions (Fira Code calt
    // halves) are designed with bitmap.width > horiAdvance so adjacent halves
    // fuse across cells — advancing only by `advance` leaves the next packed
    // glyph stomping on the previous glyph's right overhang in the atlas. The
    // `+ 1` (already baked into `stride` above) keeps a one-pixel gutter so
    // the renderer's linear sampler can't bleed across glyphs at sub-pixel
    // UVs. Row wrapping is handled by the next call's leading bounds check.
    *x += stride;
    Some(entry)
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
            ligatures: [
                HashMap::new(),
                HashMap::new(),
                HashMap::new(),
                HashMap::new(),
            ],
            notdef: entry(99),
            pack_x: 0,
            pack_y: 0,
            pack_row_height: 0,
            dirty: false,
            tried_chars: [
                HashSet::new(),
                HashSet::new(),
                HashSet::new(),
                HashSet::new(),
            ],
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
    fn flags_roundtrips_through_from_flags() {
        for v in FaceVariant::ALL {
            let (b, i) = v.flags();
            assert_eq!(FaceVariant::from_flags(b, i), v);
        }
    }

    #[test]
    fn find_face_index_returns_zero_for_empty_data() {
        // Defensive: malformed input shouldn't panic; just hand back
        // face 0 so the caller can attempt to open it and surface the
        // real error from `new_memory_face`.
        assert_eq!(find_face_index(&[], FaceVariant::Italic), 0);
        assert_eq!(find_face_index(&[0u8; 16], FaceVariant::BoldItalic), 0);
    }

    #[test]
    fn find_face_index_picks_matching_style_when_available() {
        // Real-system test: probe an installed Iosevka Term TTC and
        // verify each requested variant resolves to a face whose
        // style flags actually match. Iosevka Term packs Regular,
        // Italic, Oblique, and Extended variants into one .ttc, so
        // face 0 is regular and other indices have the styled cuts.
        //
        // Skip when no Iosevka TTC is installed (CI / dev machines
        // without the fonts) — this is a smoke test against the real
        // user-visible behavior, not a load-bearing assertion.
        let candidates = [
            "/Users/ry/Library/Fonts/SGr-IosevkaTerm-Regular.ttc",
            "/Library/Fonts/SGr-IosevkaTerm-Regular.ttc",
            "/System/Library/Fonts/SGr-IosevkaTerm-Regular.ttc",
        ];
        let Some(path) = candidates.iter().find(|p| std::path::Path::new(p).exists()) else {
            eprintln!("skipping: no Iosevka Term TTC installed");
            return;
        };
        let Ok(data) = std::fs::read(path) else {
            eprintln!("skipping: failed to read {path}");
            return;
        };

        // For every variant the helper picks, verify by opening the
        // resulting face that its style flags match the request.
        let library = ft::Library::init().expect("ft init");
        for variant in FaceVariant::ALL {
            let (want_bold, want_italic) = variant.flags();
            let idx = find_face_index(&data, variant);
            let face = library
                .new_memory_face(data.clone(), idx)
                .expect("face opens at the picked index");
            let flags = face.style_flags();
            let got_bold = flags.contains(ft::face::StyleFlag::BOLD);
            let got_italic = flags.contains(ft::face::StyleFlag::ITALIC);
            // For variants the TTC actually contains, demand a real
            // match. If the TTC doesn't pack that variant at all, the
            // helper's documented fallback is index 0 (regular) —
            // accept that too.
            let strict_match = got_bold == want_bold && got_italic == want_italic;
            let fallback_to_regular = idx == 0
                && variant != FaceVariant::Regular
                && !got_bold
                && !got_italic;
            assert!(
                strict_match || fallback_to_regular,
                "variant {variant:?}: picked index {idx}, got bold={got_bold} italic={got_italic}, wanted bold={want_bold} italic={want_italic}",
            );
        }
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

    // Regression: programming-ligature glyphs from FiraCode have
    // `bitmap.width > horiAdvance` because their side bearings extend past
    // the cell to fuse with neighbors. Advancing the atlas cursor by the
    // typographic advance left the next packed glyph stomping on the prior
    // one's right overhang — visible as `===` getting clobbered when `->`
    // gets packed after it. The fix steps the cursor by `max(width, advance)
    // + 1`, so packed entries can never overlap in the atlas regardless of
    // the advance/width relationship.
    #[test]
    fn pack_synth_leaves_no_overlap_when_width_exceeds_advance() {
        let size = 4096;
        let width = size;
        let mut texture = vec![0u8; width * size];
        let mut x = 2;
        let mut y = 0;
        let mut row_height = 0;

        // Simulate a ligature half: bitmap wider than advance.
        let make = |w: usize, h: usize, advance: usize| crate::box_drawing::Bitmap {
            data: vec![255u8; w * h],
            width: w,
            height: h,
            bearing_x: 0,
            bearing_y: 0,
            advance_x: advance,
        };

        let a = make(18, 20, 12);
        let b = make(18, 20, 12);
        let e1 = pack_synth(&a, &mut texture, width, size, &mut x, &mut y, &mut row_height)
            .expect("first glyph fits");
        let e2 = pack_synth(&b, &mut texture, width, size, &mut x, &mut y, &mut row_height)
            .expect("second glyph fits");

        assert!(
            e2.x >= e1.x + e1.width,
            "second glyph at x={} must not overlap first glyph's [{}, {})",
            e2.x,
            e1.x,
            e1.x + e1.width,
        );
    }

    // Regression: the atlas used to write pixels with a modular `% size`
    // wrap, so a glyph that didn't fit silently overwrote glyphs at the
    // top and produced UVs > 1.0. The fix returns `None` on vertical
    // overflow and writes nothing — caller falls back to notdef.
    #[test]
    fn pack_synth_returns_none_and_writes_nothing_on_vertical_overflow() {
        let size = 32;
        let width = size;
        let mut texture = vec![0u8; width * size];
        let mut x = 0;
        let mut y = 0;
        let mut row_height = 0;

        let make = |w: usize, h: usize, advance: usize| crate::box_drawing::Bitmap {
            data: vec![255u8; w * h],
            width: w,
            height: h,
            bearing_x: 0,
            bearing_y: 0,
            advance_x: advance,
        };

        // First glyph fits — fills (roughly) the whole atlas height.
        let big = make(30, 30, 30);
        let e1 = pack_synth(&big, &mut texture, width, size, &mut x, &mut y, &mut row_height)
            .expect("first glyph fits");

        // Snapshot the buffer so we can prove the second call doesn't
        // clobber anything past the first glyph's footprint.
        let before = texture.clone();

        // Second glyph won't fit vertically (row wrap pushes y past `size`).
        let second = make(30, 30, 30);
        let result = pack_synth(
            &second,
            &mut texture,
            width,
            size,
            &mut x,
            &mut y,
            &mut row_height,
        );
        assert!(result.is_none(), "overflowing glyph must return None");
        assert_eq!(
            texture, before,
            "failed pack must not modify the texture buffer"
        );
        // Sanity: first glyph's pixels are intact at their original location.
        for p in 0..e1.height {
            for q in 0..e1.width {
                assert_eq!(
                    texture[(p + e1.y) * width + (q + e1.x)],
                    255,
                    "first glyph pixel at ({}, {}) clobbered",
                    e1.x + q,
                    e1.y + p,
                );
            }
        }
    }

    // When the next glyph won't fit on the current row, pack_synth must
    // bump to the next row and place the entry there — not overlap the
    // previous row and not modular-wrap back to x=0 on the same row.
    #[test]
    fn pack_synth_row_wraps_when_glyph_overflows_horizontally() {
        let size = 64;
        let width = size;
        let mut texture = vec![0u8; width * size];
        let mut x = 0;
        let mut y = 0;
        let mut row_height = 0;

        let make = |w: usize, h: usize, advance: usize| crate::box_drawing::Bitmap {
            data: vec![255u8; w * h],
            width: w,
            height: h,
            bearing_x: 0,
            bearing_y: 0,
            advance_x: advance,
        };

        // First glyph: 40 wide, height 10. Stride = 41 → leaves x=41.
        let a = make(40, 10, 40);
        let e1 = pack_synth(&a, &mut texture, width, size, &mut x, &mut y, &mut row_height)
            .expect("first glyph fits");
        assert_eq!(e1.y, 0, "first glyph sits on row 0");

        // Second glyph: 40 wide. 41 + 41 > 64 → must wrap to next row.
        let b = make(40, 10, 40);
        let e2 = pack_synth(&b, &mut texture, width, size, &mut x, &mut y, &mut row_height)
            .expect("second glyph fits on the next row");

        assert_eq!(e2.x, 0, "wrapped glyph starts at x=0");
        assert!(
            e2.y >= e1.y + e1.height,
            "wrapped glyph at y={} must sit below first row ending at y={}",
            e2.y,
            e1.y + e1.height,
        );
    }

    // Row-wrap then vertical overflow: the glyph would wrap to a row that
    // doesn't fit. Must return None without touching the buffer.
    #[test]
    fn pack_synth_row_wrap_then_vertical_overflow_returns_none() {
        let size = 32;
        let width = size;
        let mut texture = vec![0u8; width * size];
        let mut x = 0;
        let mut y = 0;
        let mut row_height = 0;

        let make = |w: usize, h: usize, advance: usize| crate::box_drawing::Bitmap {
            data: vec![255u8; w * h],
            width: w,
            height: h,
            bearing_x: 0,
            bearing_y: 0,
            advance_x: advance,
        };

        // Pack one glyph that fills most of the atlas height. Stride = 21
        // → x advances to 21, leaving room for nothing else 20 wide.
        let first = make(20, 25, 20);
        pack_synth(&first, &mut texture, width, size, &mut x, &mut y, &mut row_height)
            .expect("first glyph fits");

        let before = texture.clone();

        // Second glyph: 20 wide. 21 + 21 > 32 → row-wraps to y = 26, which
        // plus h=25 exceeds size=32 → vertical overflow → None.
        let second = make(20, 25, 20);
        let result = pack_synth(
            &second,
            &mut texture,
            width,
            size,
            &mut x,
            &mut y,
            &mut row_height,
        );
        assert!(
            result.is_none(),
            "row-wrap that lands past the atlas bottom must return None"
        );
        assert_eq!(
            texture, before,
            "failed pack must not modify the texture buffer"
        );
    }

    // ensure_glyph_id may fall back to notdef when the atlas is full and
    // store that under the glyph id. Independently, lookup_glyph_id with
    // an unknown id (ligature not yet rasterized) must also return notdef
    // — that's the fallback the renderer relies on whenever shaping
    // produces a glyph id we haven't packed.
    #[test]
    fn lookup_glyph_id_falls_back_to_notdef_when_unknown() {
        let a = atlas_with(&[], &[]);
        // No ligatures populated → any glyph id returns notdef.
        let g = a.lookup_glyph_id(42, FaceVariant::Regular);
        assert_eq!(g.x, 99, "unknown glyph id → notdef");

        // Same fallback path for styled variants.
        let g = a.lookup_glyph_id(42, FaceVariant::Bold);
        assert_eq!(g.x, 99, "unknown glyph id on bold → notdef (via regular miss)");

        let g = a.lookup_glyph_id(42, FaceVariant::Italic);
        assert_eq!(g.x, 99, "unknown glyph id on italic → notdef");

        let g = a.lookup_glyph_id(42, FaceVariant::BoldItalic);
        assert_eq!(g.x, 99, "unknown glyph id on bold-italic → notdef");
    }

    // ---------- ensure_char tests ----------

    /// Try to construct a real `Font` for `ensure_char` tests. We need a
    /// loaded FT face — there's no hermetic fake that satisfies
    /// `Font::face()` / `cell_width()`. Returns `None` (and the caller
    /// prints `skipping: …`) when no candidate font is installed, matching
    /// the pattern used by `find_face_index_picks_matching_style_when_available`
    /// and `assert_strict_match` in `font_loader::macos::tests`.
    fn load_test_font() -> Option<Font> {
        let candidates = [
            "/Users/ry/Library/Fonts/HackNerdFont-Regular.ttf",
            "/Users/ry/Library/Fonts/FiraCode-Regular.ttf",
            "/Library/Fonts/HackNerdFont-Regular.ttf",
            "/System/Library/Fonts/Menlo.ttc",
        ];
        let path = candidates.iter().find(|p| std::path::Path::new(p).exists())?;
        let data = std::fs::read(path).ok()?;
        let mut font = Font::new(data);
        font.set_char_size(14.0, 96);
        Some(font)
    }

    /// Same as `load_test_font` but also installs a Bold primary face so
    /// the styled-variant branches of `ensure_char` can be exercised.
    fn load_test_font_with_bold() -> Option<Font> {
        let mut font = load_test_font()?;
        let bold_candidates = [
            "/Users/ry/Library/Fonts/HackNerdFont-Bold.ttf",
            "/Users/ry/Library/Fonts/FiraCode-Bold.ttf",
            "/Library/Fonts/HackNerdFont-Bold.ttf",
        ];
        let bold_path = bold_candidates.iter().find(|p| std::path::Path::new(p).exists())?;
        let bold_data = std::fs::read(bold_path).ok()?;
        if !font.set_variant(FaceVariant::Bold, bold_data, 0, 14.0, 96) {
            return None;
        }
        Some(font)
    }

    /// A real atlas with a usable packing buffer. `atlas_with` produces a
    /// zero-sized buffer which is fine for lookup tests but blows up
    /// `pack_glyph`. Tests that actually exercise the packing path call
    /// this instead.
    fn real_atlas() -> Atlas {
        let width = 512;
        let height = 512;
        Atlas {
            width,
            height,
            buffer: vec![0u8; width * height],
            variants: [
                HashMap::new(),
                HashMap::new(),
                HashMap::new(),
                HashMap::new(),
            ],
            ligatures: [
                HashMap::new(),
                HashMap::new(),
                HashMap::new(),
                HashMap::new(),
            ],
            notdef: entry(99),
            pack_x: 2,
            pack_y: 0,
            pack_row_height: 0,
            dirty: false,
            tried_chars: [
                HashSet::new(),
                HashSet::new(),
                HashSet::new(),
                HashSet::new(),
            ],
        }
    }

    // Regression: block-element / box-drawing codepoints are synthesized
    // into the atlas (full-cell, edge-to-edge tiling). The lazy font loader
    // must not overwrite a synthesized entry with the font's trimmed glyph —
    // doing so brought back the hairline seam between e.g. ▐ and ▛, because
    // the font packs ▐ as a half-width bitmap bearing into the cell.
    #[test]
    fn ensure_char_keeps_synthesized_block_glyph() {
        let Some(mut font) = load_test_font() else {
            eprintln!("skipping: no test font installed");
            return;
        };
        let cell_w = font.cell_width();
        let mut atlas = font.build_atlas();
        // After build, ▐ is the synthesized entry: zero bearing, fills the
        // cell width (not the font's trimmed right-half bbox).
        let (x0, w0, bx0) = {
            let e = atlas.lookup('\u{2590}', FaceVariant::Regular);
            (e.x, e.width, e.bearing_x)
        };
        assert_eq!(bx0, 0, "synth ▐ should have zero bearing_x");
        assert_eq!(w0, cell_w, "synth ▐ should fill the cell width");
        // The lazy loader must leave it untouched.
        atlas.ensure_char(&mut font, FaceVariant::Regular, '\u{2590}');
        let (x1, w1, bx1) = {
            let e = atlas.lookup('\u{2590}', FaceVariant::Regular);
            (e.x, e.width, e.bearing_x)
        };
        assert_eq!((x1, w1, bx1), (x0, w0, bx0), "ensure_char must not repack ▐");
    }

    // Second call for the same (variant, char) is a no-op — the
    // `tried_chars` set short-circuits before any FT work. Without this
    // gate every frame would redo `face_for` (and potentially pack again)
    // for any char whose styled chain legitimately leaves the slot empty.
    #[test]
    fn ensure_char_short_circuits_on_repeat_call() {
        let Some(mut font) = load_test_font() else {
            eprintln!("skipping: no test font installed");
            return;
        };
        let mut atlas = real_atlas();

        atlas.ensure_char(&mut font, FaceVariant::Regular, 'A');
        assert!(
            atlas.tried_chars[FaceVariant::Regular as usize].contains(&'A'),
            "first call should record the attempt in tried_chars",
        );
        let dirty_before = atlas.dirty;
        let entries_before = atlas.variants[FaceVariant::Regular as usize].len();
        let pack_x_before = atlas.pack_x;
        let pack_y_before = atlas.pack_y;

        // Force-reset dirty so we can detect any mutation the second call
        // might do (real packing also sets dirty=true).
        atlas.dirty = false;
        atlas.ensure_char(&mut font, FaceVariant::Regular, 'A');

        assert!(!atlas.dirty, "repeat call must not set dirty");
        assert_eq!(
            atlas.variants[FaceVariant::Regular as usize].len(),
            entries_before,
            "repeat call must not insert a duplicate entry",
        );
        assert_eq!(atlas.pack_x, pack_x_before, "pack cursor x must not move");
        assert_eq!(atlas.pack_y, pack_y_before, "pack cursor y must not move");
        // Sanity that we actually exercised the success path on the first call.
        assert!(dirty_before, "first call should have set dirty for a real glyph");
    }

    // A successful ensure_char inserts an entry in the requested variant
    // map and flips `dirty` so the renderer re-uploads the texture.
    #[test]
    fn ensure_char_successful_pack_inserts_entry_and_sets_dirty() {
        let Some(mut font) = load_test_font() else {
            eprintln!("skipping: no test font installed");
            return;
        };
        let mut atlas = real_atlas();
        assert!(!atlas.dirty, "fresh atlas starts clean");
        assert!(atlas.variants[FaceVariant::Regular as usize].is_empty());

        // 'A' is in printable ASCII — every plausible test font has it.
        atlas.ensure_char(&mut font, FaceVariant::Regular, 'A');

        assert!(atlas.dirty, "successful pack must set dirty");
        assert!(
            atlas.variants[FaceVariant::Regular as usize].contains_key(&'A'),
            "Regular variant should now have an entry for 'A'",
        );
        assert!(
            atlas.tried_chars[FaceVariant::Regular as usize].contains(&'A'),
            "successful pack still records the attempt",
        );
    }

    // No face anywhere in the variant chain has the glyph. For the
    // Regular variant the slot is left empty (Atlas::lookup → notdef),
    // dirty stays false, and `tried_chars` marks the attempt so we don't
    // pay the face_for walk every frame.
    #[test]
    fn ensure_char_missing_glyph_leaves_slot_empty_and_marks_tried() {
        let Some(mut font) = load_test_font() else {
            eprintln!("skipping: no test font installed");
            return;
        };
        let mut atlas = real_atlas();

        // U+10FFFD is in Supplementary Private Use Area-B. Hack and the
        // other candidate test fonts ship nothing there, and we don't
        // install fallbacks in `load_test_font`, so the face_for walk is
        // guaranteed to come up empty.
        let ch = '\u{10FFFD}';
        atlas.ensure_char(&mut font, FaceVariant::Regular, ch);

        assert!(!atlas.dirty, "missing glyph must not dirty the atlas");
        assert!(
            !atlas.variants[FaceVariant::Regular as usize].contains_key(&ch),
            "missing glyph must not create an entry (lookup falls back to notdef)",
        );
        assert!(
            atlas.tried_chars[FaceVariant::Regular as usize].contains(&ch),
            "missing glyph must still be marked tried — that's the whole point of the set",
        );

        // And the short-circuit applies on retry: cursor/state unchanged.
        let pack_x = atlas.pack_x;
        let pack_y = atlas.pack_y;
        atlas.ensure_char(&mut font, FaceVariant::Regular, ch);
        assert!(!atlas.dirty);
        assert_eq!(atlas.pack_x, pack_x);
        assert_eq!(atlas.pack_y, pack_y);
    }

    // Styled variant has no installed primary face → ensure_char recurses
    // into Regular, leaves the styled slot empty, and marks BOTH the
    // styled and Regular variant `tried_chars` (the styled mark from the
    // top of the call, the Regular mark from the recursion). Atlas::lookup
    // then correctly returns the Regular entry for a Bold request.
    #[test]
    fn ensure_char_styled_without_primary_recurses_to_regular() {
        let Some(mut font) = load_test_font() else {
            eprintln!("skipping: no test font installed");
            return;
        };
        // Precondition for the branch under test.
        assert!(
            font.variants[FaceVariant::Bold as usize].face.is_none(),
            "load_test_font should leave Bold uninstalled — this test assumes it",
        );
        let mut atlas = real_atlas();

        atlas.ensure_char(&mut font, FaceVariant::Bold, 'B');

        assert!(
            atlas.tried_chars[FaceVariant::Bold as usize].contains(&'B'),
            "Bold attempt should be marked tried at the top of the call",
        );
        assert!(
            atlas.tried_chars[FaceVariant::Regular as usize].contains(&'B'),
            "recursion into Regular should also mark Regular tried",
        );
        assert!(
            !atlas.variants[FaceVariant::Bold as usize].contains_key(&'B'),
            "Bold slot stays empty — lookup falls through to Regular",
        );
        assert!(
            atlas.variants[FaceVariant::Regular as usize].contains_key(&'B'),
            "Regular slot should be populated by the recursive call",
        );
        assert!(atlas.dirty, "recursive pack into Regular must set dirty");

        // Atlas::lookup on Bold should return the Regular entry (not notdef).
        let g = atlas.lookup('B', FaceVariant::Bold);
        assert_ne!(g.x, atlas.notdef.x, "Bold lookup must resolve via Regular, not notdef");
    }

    // Styled variant whose chain doesn't carry the glyph still recurses
    // into Regular so Regular's (possibly broader) fallback chain gets a
    // chance. This is the `face_for` returns None branch with `vi != 0`.
    // We exercise it by installing Bold and asking for a codepoint
    // neither variant's primary actually has — both Bold and Regular's
    // chains miss, but the side-effect we're checking is the recursion
    // structure: the Regular tried_chars set picks up the mark.
    #[test]
    fn ensure_char_styled_chain_miss_recurses_into_regular() {
        let Some(mut font) = load_test_font_with_bold() else {
            eprintln!("skipping: no bold test font installed");
            return;
        };
        let mut atlas = real_atlas();
        // SPUA-B codepoint that no plain monospace coding font ships.
        let ch = '\u{10FFFD}';

        atlas.ensure_char(&mut font, FaceVariant::Bold, ch);

        assert!(
            atlas.tried_chars[FaceVariant::Bold as usize].contains(&ch),
            "Bold attempt marked tried",
        );
        assert!(
            atlas.tried_chars[FaceVariant::Regular as usize].contains(&ch),
            "missing styled glyph must recurse into Regular (which also marks tried)",
        );
        assert!(
            !atlas.variants[FaceVariant::Bold as usize].contains_key(&ch),
            "Bold slot stays empty",
        );
        assert!(
            !atlas.variants[FaceVariant::Regular as usize].contains_key(&ch),
            "Regular chain also misses → slot stays empty (Atlas::lookup → notdef)",
        );
        assert!(!atlas.dirty, "no pack happened on either variant");
    }
}
