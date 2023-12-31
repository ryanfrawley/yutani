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
}

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
        let mut texture: Vec<u8> = Vec::with_capacity(width * height);
        for _ in 0..(width * height) {
            texture.push(0);
        }
        let mut entries: HashMap<char, AtlasEntry> = HashMap::with_capacity(256);
        let mut x = 0;
        let mut y = 0;
        texture[0] = 255;
        texture[1] = 255;
        texture[4096] = 255;
        texture[4097] = 255;
        for c in 0..=255 {
            let ch = char::from_u32(c).unwrap();
            let glyph = self.load_glyph(ch);
            let w = glyph.bitmap.width() as usize;
            let h = glyph.bitmap.rows() as usize;
            if h > row_height {
                row_height = h;
            }
            for p in 0..h {
                for q in 0..w {
                    texture[(((p + y) % size) * width + (q + x) % size) as usize] = glyph.bitmap.buffer()[(p * w + q) as usize];
                }
            }
            entries.insert(ch, AtlasEntry { x, y, width: w, height: h, bearing_x: (glyph.metrics.horiBearingX >> 6) as usize, advance_x: (glyph.metrics.horiAdvance >> 6) as usize, bearing_y: (glyph.metrics.horiBearingY >> 6) as isize });
            x += (glyph.metrics.horiAdvance >> 6) as usize;
            if x >= size - w {
                x = 0;
                y += row_height;
                row_height = 0;
            }
        }
        Atlas { buffer: texture, width, height, entries }
    }
}
