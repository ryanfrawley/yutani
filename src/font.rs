extern crate freetype as ft;
use std::collections::HashMap;

pub struct Glyph {
    pub bitmap: ft::Bitmap,
    pub metrics: ft::GlyphMetrics,
}

pub struct Font {
    pub face: ft::Face,
}

pub struct Atlas {
    pub width: usize,
    pub height: usize,
    pub buffer: Vec<u8>,
    pub entries: HashMap<char, AtlasEntry>,
    // The font's .notdef glyph (usually a hollow box). Rendered in place of
    // any character the font doesn't provide, so missing glyphs are visibly
    // "tofu" rather than invisible.
    pub notdef: AtlasEntry,
}

#[derive(Copy, Clone)]
pub struct AtlasEntry {
    pub x: usize,
    pub y: usize,
    pub width: usize,
    pub height: usize,
    pub bearing_y: isize,
    pub bearing_x: usize,
    pub advance_x: usize,
}

impl Font {
    pub fn new(data: Vec<u8>) -> Self {
        let library = ft::Library::init().unwrap();

        Font { face: library.new_memory_face(data, 0).unwrap() }
    }

    pub fn load_glyph(&mut self, character: char) -> Glyph {
        self.face.load_char(character as usize, ft::face::LoadFlag::RENDER).unwrap();
        let glyph = self.face.glyph();
        Glyph { metrics: glyph.metrics(), bitmap: glyph.bitmap() }
    }

    pub fn set_char_size(&mut self, height_points: f32, dpi: u32) {
        let size = (height_points * 64.0) as isize;
        println!("size: {}", size);
        self.face.set_char_size(size, 0, dpi, dpi).unwrap();
    }

    pub fn build_atlas(&mut self) -> Atlas {
        let size = 4096;
        let width = size;
        let height = size;
        let mut row_height = 0;
        let mut texture: Vec<u8> = vec![0; width * height];
        let mut entries: HashMap<char, AtlasEntry> = HashMap::with_capacity(4096);
        let mut x = 0;
        let mut y = 0;
        texture[0] = 255;
        texture[1] = 255;
        texture[4096] = 255;
        texture[4097] = 255;

        // The font's .notdef glyph (index 0). Rendered in place of any
        // character the font doesn't provide — usually a hollow box.
        self.face
            .load_glyph(0, ft::face::LoadFlag::RENDER)
            .expect("font has no .notdef glyph");
        let notdef = pack_glyph(self.face.glyph(), &mut texture, width, size, &mut x, &mut y, &mut row_height);

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

        for range in ranges {
            for c in range.clone() {
                let ch = match char::from_u32(c) {
                    Some(ch) => ch,
                    None => continue,
                };
                if self.face.get_char_index(ch as usize).is_err() {
                    // Font doesn't have this glyph — don't waste atlas space.
                    // The render path will fall back to `atlas.notdef`.
                    continue;
                }
                self.face
                    .load_char(ch as usize, ft::face::LoadFlag::RENDER)
                    .unwrap();
                let entry = pack_glyph(
                    self.face.glyph(),
                    &mut texture,
                    width,
                    size,
                    &mut x,
                    &mut y,
                    &mut row_height,
                );
                entries.insert(ch, entry);
            }
        }
        Atlas {
            buffer: texture,
            width,
            height,
            entries,
            notdef,
        }
    }
}

fn pack_glyph(
    glyph: &ft::GlyphSlot,
    texture: &mut [u8],
    width: usize,
    size: usize,
    x: &mut usize,
    y: &mut usize,
    row_height: &mut usize,
) -> AtlasEntry {
    let bitmap = glyph.bitmap();
    let metrics = glyph.metrics();
    let w = bitmap.width() as usize;
    let h = bitmap.rows() as usize;
    let advance = (metrics.horiAdvance >> 6) as usize;
    if h > *row_height {
        *row_height = h;
    }
    for p in 0..h {
        for q in 0..w {
            texture[((p + *y) % size) * width + (q + *x) % size] = bitmap.buffer()[p * w + q];
        }
    }
    let entry = AtlasEntry {
        x: *x,
        y: *y,
        width: w,
        height: h,
        bearing_x: (metrics.horiBearingX >> 6) as usize,
        advance_x: advance,
        bearing_y: (metrics.horiBearingY >> 6) as isize,
    };
    *x += advance.max(1);
    if *x + w >= size {
        *x = 0;
        *y += *row_height;
        *row_height = 0;
    }
    entry
}
