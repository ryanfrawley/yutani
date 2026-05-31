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
