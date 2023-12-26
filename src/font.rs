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
        let width = 1024;
        let height = 1024;
        let mut texture: Vec<u8> = Vec::with_capacity(width * height);
        for _ in 0..(width * height) {
            texture.push(0);
        }
        let mut entries: HashMap<char, AtlasEntry> = HashMap::with_capacity(256);
        let mut x = 0;
        let mut y = 0;
        for c in 'a'..='z' {
            let glyph = self.load_glyph(c as char);
            let w = glyph.bitmap.width() as usize;
            let h = glyph.bitmap.rows() as usize;
            for p in 0..h {
                for q in 0..w {
                    texture[(((p + y) % 1024) * width + (q + x) % 1024) as usize] = glyph.bitmap.buffer()[(p * w + q) as usize];
                }
            }
            entries.insert(c, AtlasEntry { x, y, width: w, height: h });
            println!("{} {} {} {} {}", c, x as f32 / 1024.0, y as f32 / 1024.0, w as f32 / 1024.0, h as f32 / 1024.0);
            x += 100;
            if x >= (1024 - 100) {
                x = 0;
                y += 100;
            }
        }
        Atlas { buffer: texture, width, height, entries }
    }
}
