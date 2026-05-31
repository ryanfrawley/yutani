//! Platform color-emoji rasterization.
//!
//! The bundled FreeType can't decode the PNG-encoded `sbix` strikes that Apple
//! Color Emoji ships (no libpng), so on macOS we rasterize emoji with Core Text
//! straight into a premultiplied BGRA bitmap and hand it to the atlas's color
//! layer. On other platforms there's no rasterizer here — color fonts that
//! FreeType *can* render (COLR/CPAL) go through its color path instead, and
//! anything else falls back to the monochrome chain.

/// Rasterize `ch` at roughly `px` pixels tall into a premultiplied **BGRA**
/// bitmap, returning `(bytes, width, height)`. BGRA (not RGBA) to match what
/// FreeType produces for `FT_PIXEL_MODE_BGRA`, so the atlas and shader treat
/// both sources identically. `None` if the platform has no rasterizer or the
/// glyph couldn't be drawn.
#[cfg(target_os = "macos")]
pub fn rasterize(ch: char, px: u32) -> Option<(Vec<u8>, usize, usize)> {
    use core_foundation::attributed_string::CFMutableAttributedString;
    use core_foundation::base::{CFRange, TCFType};
    use core_foundation::string::CFString;
    use core_graphics::color_space::CGColorSpace;
    use core_graphics::context::CGContext;
    use core_text::font::new_from_name;
    use core_text::line::CTLine;
    use core_text::string_attributes::kCTFontAttributeName;

    // Premultiplied, little-endian 32-bit: alpha-first in ARGB order reversed by
    // the little-endian flag → B,G,R,A in memory. Matches FreeType's BGRA.
    // kCGImageAlphaPremultipliedFirst (2) | kCGBitmapByteOrder32Little (2 << 12).
    const BITMAP_INFO: u32 = 2 | (2 << 12);

    let px = px.max(1) as f64;
    let font = new_from_name("Apple Color Emoji", px).ok()?;

    // Attributed string carrying just this character in the emoji font. Using a
    // CTLine (rather than raw glyph lookup) shapes surrogate pairs and ZWJ
    // sequences correctly — important since most emoji are astral codepoints.
    let text = CFString::new(&ch.to_string());
    let mut astr = CFMutableAttributedString::new();
    astr.replace_str(&text, CFRange::init(0, 0));
    let len = astr.char_len();
    if len == 0 {
        return None;
    }
    astr.set_attribute(
        CFRange::init(0, len),
        unsafe { kCTFontAttributeName },
        &font,
    );
    let line = CTLine::new_with_attributed_string(astr.as_concrete_TypeRef());

    // Measure ink bounds in a throwaway 1×1 context, then allocate exactly.
    let space = CGColorSpace::create_device_rgb();
    let probe = CGContext::create_bitmap_context(None, 1, 1, 8, 4, &space, BITMAP_INFO);
    let bounds = line.get_image_bounds(&probe);
    let w = bounds.size.width.ceil() as usize;
    let h = bounds.size.height.ceil() as usize;
    if w == 0 || h == 0 {
        return None;
    }

    let mut ctx = CGContext::create_bitmap_context(None, w, h, 8, w * 4, &space, BITMAP_INFO);
    // Shift the ink to the bitmap origin so it isn't clipped by bearings.
    ctx.translate(-bounds.origin.x, -bounds.origin.y);
    line.draw(&ctx);
    Some((ctx.data().to_vec(), w, h))
}

#[cfg(not(target_os = "macos"))]
pub fn rasterize(_ch: char, _px: u32) -> Option<(Vec<u8>, usize, usize)> {
    None
}

/// A monochrome glyph rasterized by the platform: 8-bit **coverage** (alpha)
/// plus its pixel size and bearings relative to the text baseline, exactly like
/// a FreeType bitmap so it packs into the coverage atlas and tints by the cell's
/// foreground color. `left`/`top` are `bitmap_left`/`bitmap_top` equivalents.
pub struct MonoGlyph {
    pub coverage: Vec<u8>,
    pub width: usize,
    pub height: usize,
    pub left: i32,
    pub top: i32,
}

/// Rasterize `ch` as a monochrome coverage mask via the platform text stack,
/// for glyphs FreeType can't render. The chief macOS case is CJK: the system
/// fonts (PingFang, Hiragino) fail to load in the bundled FreeType
/// (`LocationsMissing`), so without this they'd be tofu. Core Text's automatic
/// font substitution finds the right face; we draw it black-on-clear and keep
/// the alpha channel as coverage. `None` if nothing was drawn / off macOS.
#[cfg(target_os = "macos")]
pub fn rasterize_mono(ch: char, px: u32) -> Option<MonoGlyph> {
    use core_foundation::attributed_string::CFMutableAttributedString;
    use core_foundation::base::{CFRange, TCFType};
    use core_foundation::string::CFString;
    use core_graphics::color_space::CGColorSpace;
    use core_graphics::context::CGContext;
    use core_text::font::new_from_name;
    use core_text::line::CTLine;
    use core_text::string_attributes::kCTFontAttributeName;

    const BITMAP_INFO: u32 = 2 | (2 << 12); // premultiplied BGRA, little-endian

    let px = px.max(1) as f64;
    // A base font at the target size; Core Text substitutes the correct face
    // (e.g. PingFang for CJK) for any character this one lacks.
    let font = new_from_name("Helvetica", px).ok()?;

    let text = CFString::new(&ch.to_string());
    let mut astr = CFMutableAttributedString::new();
    astr.replace_str(&text, CFRange::init(0, 0));
    let len = astr.char_len();
    if len == 0 {
        return None;
    }
    astr.set_attribute(CFRange::init(0, len), unsafe { kCTFontAttributeName }, &font);
    let line = CTLine::new_with_attributed_string(astr.as_concrete_TypeRef());

    let space = CGColorSpace::create_device_rgb();
    let probe = CGContext::create_bitmap_context(None, 1, 1, 8, 4, &space, BITMAP_INFO);
    let bounds = line.get_image_bounds(&probe);
    let w = bounds.size.width.ceil() as usize;
    let h = bounds.size.height.ceil() as usize;
    if w == 0 || h == 0 {
        return None;
    }

    let mut ctx = CGContext::create_bitmap_context(None, w, h, 8, w * 4, &space, BITMAP_INFO);
    ctx.translate(-bounds.origin.x, -bounds.origin.y);
    line.draw(&ctx);
    // Keep the alpha channel (byte 3 of each BGRA texel) as coverage.
    let bgra = ctx.data();
    let mut coverage = vec![0u8; w * h];
    for (i, cov) in coverage.iter_mut().enumerate() {
        *cov = bgra[i * 4 + 3];
    }
    Some(MonoGlyph {
        coverage,
        width: w,
        height: h,
        // Bearings from the *untranslated* ink box: left edge, and top = how far
        // the ink rises above the baseline.
        left: bounds.origin.x.floor() as i32,
        top: (bounds.origin.y + bounds.size.height).ceil() as i32,
    })
}

#[cfg(not(target_os = "macos"))]
pub fn rasterize_mono(_ch: char, _px: u32) -> Option<MonoGlyph> {
    None
}
