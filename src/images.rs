//! Decode + storage layer for terminal image placements.
//!
//! Phase 1. Decode runs on a single worker thread so a 50ms PNG decode
//! doesn't stall the render loop; GPU upload happens on the main thread
//! during [`Store::poll`] because wgpu is single-threaded. Refcounting is
//! implicit: the renderer calls [`Store::retain`] each frame with the set
//! of `ImageId`s that any live or scrollback placement references;
//! everything else gets dropped one frame later. This keeps `terminal.rs`
//! free of GPU types — placements carry an opaque `ImageId` and don't know
//! what's behind it.
//!
//! Memory cap is enforced on upload, not on decode. LRU eviction would only
//! help by evicting in-use images (since `retain` already drops everything
//! unused each frame); refusing the new image is the safer call.
//!
//! Per-request timeout is enforced by the main thread on `poll`. The worker
//! can't be interrupted mid-decode, but its late result is discarded — so
//! a malformed image that sends the decoder into a slow loop only ties up
//! the worker, not the render thread.

use std::collections::{HashMap, HashSet};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crate::renderer::images::{GpuImage, ImagePipeline};

/// Opaque handle for a decoded image in the [`Store`]. Newtype so the
/// compiler catches the (easy) mistake of confusing it with `PlacementId`,
/// which lives at the same `u32` width.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct ImageId(pub u32);

/// Token returned by [`Store::request_insert`] for matching async decode
/// results in [`Store::poll`]. Separate type from [`ImageId`] so a stale
/// pending id can't be passed as an image id by mistake.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct PendingId(pub u32);

#[derive(Debug)]
pub enum DecodeError {
    /// `image` crate failed — malformed payload, unsupported format, etc.
    Decode(image::ImageError),
    /// Decoded image exceeds the configured pixel cap. Carries both the
    /// observed size and the cap so the caller can log a useful message.
    TooLarge { pixels: u64, max: u64 },
    /// Pending request aged past `images_decode_timeout_ms`. The worker's
    /// result, if it eventually arrives, will be silently dropped.
    TimedOut { elapsed_ms: u64 },
    /// Decode succeeded but the resulting image would push the store's
    /// total bytes past the configured cap. Refuse rather than evict
    /// in-use images (mark-and-sweep already prunes unused).
    BudgetExceeded { needed: usize, available: usize },
    /// `a=f` arrived before its parent image's base decode finished
    /// (so `base_dims` / `base_rgba` aren't populated yet) — we can't
    /// composite without dimensions. Distinct from `BudgetExceeded`
    /// so a caller doesn't confuse the two when reasoning about
    /// cleanup.
    ParentNotReady,
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::Decode(e) => write!(f, "decode failed: {e}"),
            DecodeError::TooLarge { pixels, max } => {
                write!(f, "image too large: {pixels} pixels (max {max})")
            }
            DecodeError::TimedOut { elapsed_ms } => {
                write!(f, "decode timed out after {elapsed_ms}ms")
            }
            DecodeError::BudgetExceeded { needed, available } => {
                write!(f, "image needs {needed} bytes, only {available} free in cache")
            }
            DecodeError::ParentNotReady => write!(
                f,
                "animation frame arrived before parent image finished decoding",
            ),
        }
    }
}

impl std::error::Error for DecodeError {}

/// Decode `bytes` into straight-alpha RGBA8 and report the size. Rejects
/// before allocating the pixel buffer when the header's declared dimensions
/// already exceed `max_pixels` — this is what protects against a tiny
/// compressed payload that decodes to gigabytes (PNG zlib bombs, etc.).
///
/// Returns `(rgba, width, height)`. The buffer is `width * height * 4` bytes.
pub fn decode_to_rgba(bytes: &[u8], max_pixels: u64) -> Result<(Vec<u8>, u32, u32), DecodeError> {
    // Peek at the header first so an attacker can't ask us to allocate
    // before the size check fires.
    let reader = image::io::Reader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| DecodeError::Decode(image::ImageError::IoError(e)))?;
    if let Some((w, h)) = reader.into_dimensions().ok() {
        let pixels = (w as u64) * (h as u64);
        if pixels > max_pixels {
            return Err(DecodeError::TooLarge { pixels, max: max_pixels });
        }
    }

    // Re-decode for real — `into_dimensions` consumed the reader.
    let img = image::load_from_memory(bytes).map_err(DecodeError::Decode)?;
    let (w, h) = (img.width(), img.height());
    // Final guard: some formats (animated GIF, ICO) report different sizes
    // through `into_dimensions` than through `load_from_memory`. Re-check.
    let pixels = (w as u64) * (h as u64);
    if pixels > max_pixels {
        return Err(DecodeError::TooLarge { pixels, max: max_pixels });
    }
    let rgba = img.into_rgba8();
    let (w, h) = rgba.dimensions();
    Ok((rgba.into_raw(), w, h))
}

/// Read only the format header to learn an image's pixel dimensions —
/// microseconds vs the ~50ms of full decode. Returns `None` on
/// unsupported / malformed input; callers should treat that as "use a
/// fallback size" rather than failing the whole insert.
///
/// Used by the OSC 1337 / Kitty / Sixel parsers to size the placement
/// (so the cursor can advance correctly) without blocking the PTY ingest
/// path on the eventual full decode running in the worker thread.
pub fn peek_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    image::io::Reader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()
}

struct StoredImage {
    /// `None` while the decode is in flight. Renderer's `peek` returns
    /// `None` in that window so any placement that already references this
    /// id renders blank (one or two frames, typically) until the worker
    /// finishes and `poll` fills the slot.
    image: Option<GpuImage>,
    /// CPU-side downsampled RGBA preview, generated at decode time. Used by
    /// the half-block fallback path when the GPU draw is suppressed (config
    /// `images_enabled = false`). Held alongside `image` because re-reading
    /// pixels back from the GPU each frame would be ruinous and the preview
    /// is tiny (≤ MAX_PREVIEW_PIXELS * 4 bytes). `None` while the decode is
    /// in flight, mirroring `image`.
    preview: Option<Preview>,
    /// Bytes-on-GPU estimate (`width * height * 4`). 0 while `image` is
    /// `None` so a reservation doesn't consume the byte budget before
    /// upload.
    bytes: usize,
    /// Wall-clock of the most recent `get`. Available for future LRU work;
    /// not currently used for eviction.
    last_used: Instant,
    /// Full-size CPU RGBA mirror of the base (frame 1 in Kitty's 1-based
    /// terms). Populated at decode time so the Kitty `a=f` path can
    /// composite incoming frame deltas against the base without
    /// reading back from the GPU. Same `width * height * 4` byte
    /// count as `image` — included in `bytes` accounting.
    base_rgba: Option<Vec<u8>>,
    /// Width/height of the base image in pixels. Mirrors
    /// `image.width_px` / `image.height_px` once `image` is `Some`;
    /// kept separately so the `a=f` validator can refuse a frame whose
    /// declared dimensions don't match the parent before going through
    /// the decode worker.
    base_dims: Option<(u32, u32)>,
    /// Additional frames after the base. `frames[i]` is Kitty frame
    /// `i + 2` (1-based, with the base being frame 1). Empty for
    /// non-animated images. Each frame holds its own GPU texture and
    /// CPU RGBA mirror so later frames can compose against any earlier
    /// one without GPU readback.
    frames: Vec<Frame>,
    /// Playback state. Defaults to `Stopped` on frame 1; mutated by
    /// `a=a c=` (make-current) and `a=a s=` (play / loop). Even
    /// stopped, `current_frame > 0` shows that specific frame instead
    /// of the base.
    animation: AnimationState,
    /// Per-frame gap (in ms) for the base image — Kitty frame 1.
    /// Set via `a=a r=1 z=N`. The base has no slot in `frames`, so
    /// without this field it would have to borrow some other frame's
    /// delay when wrapping back around. Default 0 means "advance
    /// immediately" (effectively a 1-tick minimum so the loop
    /// doesn't busy-spin).
    base_delay_ms: u32,
}

/// One frame past the base in an animated image. Stored alongside the
/// base in `StoredImage::frames`. Each frame is a fully-composed
/// snapshot at the parent image's full dimensions; the `a=f` ingest
/// path runs the composition CPU-side at decode-completion time so the
/// renderer's per-frame draw is a plain texture sample with no extra
/// blend math.
pub struct Frame {
    pub image: GpuImage,
    pub rgba: Vec<u8>,
    pub delay_ms: u32,
    pub bytes: usize,
}

/// Kitty animation play state. `peek_at` consults this together with
/// the wall clock to decide which frame's GPU image to hand back this
/// render tick.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AnimationState {
    pub play_mode: PlayMode,
    /// 0-based index into the (base + frames) sequence: 0 = the base
    /// (Kitty frame 1), `n` = `frames[n - 1]` (Kitty frame `n + 1`).
    /// Held even while stopped — `a=a c=N` sets this to display frame
    /// N statically.
    pub current_frame: u32,
    /// When `current_frame`'s display window opened. `peek_at` adds
    /// the frame's `delay_ms` and decides whether to advance.
    pub current_frame_started: Instant,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlayMode {
    /// Hold on `current_frame` forever. The default until `a=a s=2/3`
    /// is received.
    Stopped,
    /// `a=a s=2` — keep advancing frames as long as new ones may
    /// still arrive. We treat this as the same wall-clock advance as
    /// `LoopForever` since the store has no way to know whether the
    /// app is done sending frames.
    RunWhileLoading,
    /// `a=a s=3 v=0` — loop indefinitely.
    LoopForever,
    /// `a=a s=3 v=N` (N > 0) — play through the whole sequence N
    /// times, then stop on the last frame. `remaining` decrements
    /// each time we wrap back to frame 0.
    LoopFinite { remaining: u32 },
}

impl Default for AnimationState {
    fn default() -> Self {
        Self {
            play_mode: PlayMode::Stopped,
            current_frame: 0,
            current_frame_started: Instant::now(),
        }
    }
}

/// Small CPU-side RGBA8 thumbnail of a decoded image. Used to drive the
/// half-block fallback path (one glyph per two vertical pixels). Sized at
/// decode time to fit inside `MAX_PREVIEW_COLS × MAX_PREVIEW_ROWS` so the
/// memory cost is bounded and the per-cell sampling is a single index.
#[derive(Clone, Debug, PartialEq)]
pub struct Preview {
    pub width: u32,
    pub height: u32,
    /// Straight-alpha RGBA8, top-row-first. Same orientation as the
    /// source decode, so cell (0, 0) lands at pixel (0, 0).
    pub rgba: Vec<u8>,
}

/// Upper bound on the preview's horizontal pixel count. Picked so the
/// preview can supply two pixels per glyph cell for a full-width 160-column
/// half-block render without wasting memory. Each output cell is one
/// horizontal pixel (we don't try to render quarter-blocks in the X axis).
pub const MAX_PREVIEW_COLS: u32 = 160;
/// Upper bound on the preview's vertical pixel count. Each cell renders
/// U+2580 (▀) — fg = top half pixel, bg = bottom half pixel — so a 192px
/// preview covers a 96-row placement. 160 × 192 * 4 = 122 KiB worst case;
/// realistic placements stay well under that.
pub const MAX_PREVIEW_ROWS: u32 = 192;

/// Box-filter downsample `rgba` (straight-alpha RGBA8, `w × h`) to a
/// preview sized to fit inside the half-block budget while preserving
/// aspect ratio. The destination is always at least 1×1 so any non-empty
/// source produces a sampleable buffer.
///
/// Cheap on purpose: a simple area average per destination pixel. Decode
/// already cost ~tens of ms on a real image, and the preview is generated
/// once per image — a more expensive Lanczos resample would buy nothing
/// the half-block path can show.
pub fn build_preview(rgba: &[u8], w: u32, h: u32) -> Option<Preview> {
    if w == 0 || h == 0 || rgba.len() < (w as usize) * (h as usize) * 4 {
        return None;
    }
    // Pick the largest destination size that fits inside the budget and
    // matches the source aspect. Scale by the more-constrained axis so the
    // other axis stays bounded too.
    let sx = (MAX_PREVIEW_COLS as f32) / (w as f32);
    let sy = (MAX_PREVIEW_ROWS as f32) / (h as f32);
    let s = sx.min(sy).min(1.0); // Don't upscale — wastes memory.
    let dst_w = ((w as f32) * s).round().clamp(1.0, MAX_PREVIEW_COLS as f32) as u32;
    let dst_h = ((h as f32) * s).round().clamp(1.0, MAX_PREVIEW_ROWS as f32) as u32;

    let mut out = vec![0u8; (dst_w as usize) * (dst_h as usize) * 4];
    for dy in 0..dst_h {
        // Source row span for this destination row. Inclusive lower bound,
        // exclusive upper. `.max(y0 + 1)` guarantees at least one source
        // row per destination row even when scaling 1:1 lands on an
        // integer boundary that the float math rounds the same way.
        let y0 = ((dy as u64) * (h as u64) / (dst_h as u64)) as u32;
        let y1 = (((dy + 1) as u64) * (h as u64) / (dst_h as u64)).max((y0 + 1) as u64) as u32;
        let y1 = y1.min(h);
        for dx in 0..dst_w {
            let x0 = ((dx as u64) * (w as u64) / (dst_w as u64)) as u32;
            let x1 = (((dx + 1) as u64) * (w as u64) / (dst_w as u64)).max((x0 + 1) as u64) as u32;
            let x1 = x1.min(w);
            // Sum into u32s — a 160x192 source covering the whole 4x4-byte
            // budget stays well under u32::MAX even at 255 per sample.
            let (mut r, mut g, mut b, mut a, mut n) = (0u32, 0u32, 0u32, 0u32, 0u32);
            for sy_i in y0..y1 {
                let row_base = (sy_i as usize) * (w as usize) * 4;
                for sx_i in x0..x1 {
                    let i = row_base + (sx_i as usize) * 4;
                    r += rgba[i] as u32;
                    g += rgba[i + 1] as u32;
                    b += rgba[i + 2] as u32;
                    a += rgba[i + 3] as u32;
                    n += 1;
                }
            }
            // n is guaranteed ≥ 1 by the `.max(+1)` clamps above.
            let o = (dy as usize) * (dst_w as usize) * 4 + (dx as usize) * 4;
            out[o] = (r / n) as u8;
            out[o + 1] = (g / n) as u8;
            out[o + 2] = (b / n) as u8;
            out[o + 3] = (a / n) as u8;
        }
    }
    Some(Preview { width: dst_w, height: dst_h, rgba: out })
}

/// One half-block glyph cell. The renderer turns this into `Cell { ch: '▀',
/// style: Style { color_fg: Some(fg), color_bg: Some(bg), .. } }` — U+2580
/// fills the top half of the cell with fg and leaves the bottom half showing
/// bg, so two vertical pixels of the preview render per cell.
///
/// Coordinates are *cell offsets* relative to the placement's anchor —
/// caller adds `placement.top_row` / `placement.left_col` to get viewport
/// coords. Keeping them relative makes the unit test for the math trivial.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct HalfblockCell {
    pub row_offset: u16,
    pub col_offset: u16,
    /// Linear-space [r, g, b, a] in the same convention as `style::Cell`'s
    /// color slots. sRGB conversion happens here so the renderer just
    /// stuffs the values into `Style::color_fg` / `color_bg`.
    pub fg: [f32; 4],
    pub bg: [f32; 4],
}

/// Rasterize `preview` into a grid of `rows × cols` half-block cells.
/// Each output cell consumes two vertical and one horizontal preview pixel:
/// fg = the top pixel, bg = the bottom pixel. When `rows`/`cols` ask for
/// more cells than the preview has pixels, sampling is nearest-neighbour
/// against the preview — the placement was already sized to the image's
/// native cell extent, so the common case is exact-match.
///
/// Returns one entry per cell (no skipping). Callers can iterate and emit
/// glyph cells directly.
pub fn halfblock_cells(preview: &Preview, rows: u16, cols: u16) -> Vec<HalfblockCell> {
    use crate::palette::srgb_to_linear;
    let mut out = Vec::with_capacity((rows as usize) * (cols as usize));
    if preview.width == 0 || preview.height == 0 || rows == 0 || cols == 0 {
        return out;
    }
    for rr in 0..rows {
        for cc in 0..cols {
            // Nearest-neighbour sample in cell space → preview space.
            // The +0.5 centers each cell on its pixel cluster so the
            // first and last cells don't both sample the same edge pixel
            // when `rows`/`cols` exceed the preview.
            let px = ((cc as u32 * preview.width) / (cols as u32)).min(preview.width - 1);
            // Top and bottom half-pixel rows. `2 * rr` and `2 * rr + 1`
            // span the conceptual "two pixels per cell" — but the
            // preview's vertical extent isn't necessarily exactly 2*rows
            // (it was clamped to MAX_PREVIEW_ROWS), so scale through to
            // preview height.
            let top_p = (((2 * rr as u32) * preview.height) / (2 * rows as u32)).min(preview.height - 1);
            let bot_p =
                ((((2 * rr as u32) + 1) * preview.height) / (2 * rows as u32)).min(preview.height - 1);
            let top_i = ((top_p as usize) * (preview.width as usize) + (px as usize)) * 4;
            let bot_i = ((bot_p as usize) * (preview.width as usize) + (px as usize)) * 4;
            let p = &preview.rgba;
            // Preview is straight-alpha sRGB8 (image crate convention).
            // The renderer's color slots expect linear-space floats —
            // same path `style::rgb()` takes for SGR truecolor.
            let fg = [
                srgb_to_linear(p[top_i]),
                srgb_to_linear(p[top_i + 1]),
                srgb_to_linear(p[top_i + 2]),
                (p[top_i + 3] as f32) / 255.0,
            ];
            let bg = [
                srgb_to_linear(p[bot_i]),
                srgb_to_linear(p[bot_i + 1]),
                srgb_to_linear(p[bot_i + 2]),
                (p[bot_i + 3] as f32) / 255.0,
            ];
            out.push(HalfblockCell { row_offset: rr, col_offset: cc, fg, bg });
        }
    }
    out
}

/// Half-block glyph used by [`halfblock_cells`] callers. U+2580 (▀) —
/// upper half block. Public so the renderer's substitution path can match
/// against it without re-defining the codepoint.
pub const HALFBLOCK_CHAR: char = '▀';

/// Alpha-blend `src` (sized `src_w * src_h`, RGBA8) onto a copy of
/// `dst` (sized `dst_w * dst_h`, RGBA8) at top-left `(dx, dy)`. Returns
/// the composed buffer at `dst`'s dimensions.
///
/// Used by the Kitty `a=f` ingest path so each new frame turns into a
/// fully-realized full-frame RGBA snapshot the renderer can sample
/// directly. Out-of-bounds src pixels are clipped at the dst's edges
/// — the spec says new frames may overhang but the visible portion
/// is what gets composed.
///
/// Composition is "source-over" with straight alpha: fully-opaque src
/// pixels overwrite, fully-transparent src pixels leave dst unchanged,
/// partial-alpha src pixels are integer-blended onto dst. The integer
/// math (≤ 6 ops per channel, no float, no LUT) is cheap enough to
/// run on the main thread inside Store::poll without noticeable
/// frame-loop impact.
pub(crate) fn composite_rgba(
    dst: &[u8],
    dst_w: u32,
    dst_h: u32,
    src: &[u8],
    src_w: u32,
    src_h: u32,
    dx: u32,
    dy: u32,
) -> Vec<u8> {
    let dst_w_us = dst_w as usize;
    let dst_h_us = dst_h as usize;
    let src_w_us = src_w as usize;
    let src_h_us = src_h as usize;
    let dx_us = dx as usize;
    let dy_us = dy as usize;
    let mut out = dst.to_vec();
    if src.len() < src_w_us.saturating_mul(src_h_us).saturating_mul(4) {
        // Defensive: a malformed payload that's shorter than declared
        // would index out-of-bounds. Return the dst unchanged.
        return out;
    }
    for sy in 0..src_h_us {
        let dy_i = match dy_us.checked_add(sy) {
            Some(v) if v < dst_h_us => v,
            _ => continue,
        };
        for sx in 0..src_w_us {
            let dx_i = match dx_us.checked_add(sx) {
                Some(v) if v < dst_w_us => v,
                _ => continue,
            };
            let s = (sy * src_w_us + sx) * 4;
            let d = (dy_i * dst_w_us + dx_i) * 4;
            let sa = src[s + 3] as u32;
            if sa == 0 {
                continue;
            }
            if sa == 255 {
                out[d..d + 4].copy_from_slice(&src[s..s + 4]);
                continue;
            }
            let inv = 255 - sa;
            let sr = src[s] as u32;
            let sg = src[s + 1] as u32;
            let sb = src[s + 2] as u32;
            let dr = out[d] as u32;
            let dg = out[d + 1] as u32;
            let db = out[d + 2] as u32;
            let da = out[d + 3] as u32;
            // Source-over with straight-alpha: out = src*α + dst*(1-α).
            // Round-half-up via `+ 127` keeps a single pass of blends
            // from drifting toward zero.
            out[d] = (((sr * sa) + (dr * inv) + 127) / 255) as u8;
            out[d + 1] = (((sg * sa) + (dg * inv) + 127) / 255) as u8;
            out[d + 2] = (((sb * sa) + (db * inv) + 127) / 255) as u8;
            // Alpha output: standard "over" yields α_out = α_src +
            // α_dst*(1-α_src). Same precision as the color channels.
            out[d + 3] = ((sa * 255 + da * inv + 127) / 255).min(255) as u8;
        }
    }
    out
}

/// True for play modes that should advance frames over time.
fn is_advancing(mode: PlayMode) -> bool {
    matches!(
        mode,
        PlayMode::RunWhileLoading
            | PlayMode::LoopForever
            | PlayMode::LoopFinite { .. },
    )
}

/// Decide which frame index (0 = base, 1..=N = frames[0..N-1]) should
/// be displayed for `entry` at wall-clock `now`. Pure function so the
/// `peek_at` path can be tested without GPU mocking.
fn resolve_current_frame(entry: &StoredImage, now: Instant) -> u32 {
    let total_frames = (entry.frames.len() as u32) + 1; // base + frames
    if entry.frames.is_empty() || !is_advancing(entry.animation.play_mode) {
        return entry.animation.current_frame.min(total_frames - 1);
    }
    // Walk the timeline forward from `current_frame_started` consuming
    // each frame's delay_ms until we hit one whose end is past `now`.
    // The base's delay borrows from the first frame (spec has no
    // per-base gap field; the loop wraps base → frames[0] → frames[1] →
    // … → base).
    let mut idx = entry.animation.current_frame.min(total_frames - 1);
    let mut anchor = entry.animation.current_frame_started;
    let max_steps = total_frames.saturating_mul(8) as u64 + 1024; // safety cap
    let mut steps: u64 = 0;
    loop {
        let delay_ms = if idx == 0 {
            entry.base_delay_ms
        } else {
            entry
                .frames
                .get((idx as usize).saturating_sub(1))
                .map(|f| f.delay_ms)
                .unwrap_or(0)
        };
        // delay_ms == 0 means "advance immediately"; treat as 1ms to
        // bound the loop. Otherwise we'd spin forever for now > anchor.
        let delay = Duration::from_millis(delay_ms.max(1) as u64);
        let end = anchor + delay;
        if now < end {
            return idx;
        }
        anchor = end;
        // Advance to next frame, wrapping per play mode.
        let next = idx + 1;
        if next >= total_frames {
            match entry.animation.play_mode {
                PlayMode::LoopForever | PlayMode::RunWhileLoading => {
                    idx = 0;
                }
                PlayMode::LoopFinite { remaining } => {
                    if remaining <= 1 {
                        return total_frames - 1; // stop on last
                    }
                    idx = 0;
                }
                PlayMode::Stopped => return idx, // can't get here (is_advancing)
            }
        } else {
            idx = next;
        }
        steps += 1;
        if steps > max_steps {
            return idx;
        }
    }
}

struct DecodeJob {
    pending_id: u32,
    bytes: Vec<u8>,
    max_pixels: u64,
    label: Option<String>,
}

struct DecodeResult {
    pending_id: u32,
    label: Option<String>,
    outcome: Result<DecodedPixels, DecodeError>,
}

struct DecodedPixels {
    rgba: Vec<u8>,
    width: u32,
    height: u32,
}

struct PendingRequest {
    /// Pre-allocated `ImageId` returned to the caller at `request_insert`
    /// time. The slot in `images` is reserved with `image: None` and gets
    /// filled (or removed) when `poll` processes the worker's result.
    image_id: u32,
    /// Whether this pending decode is filling the base image or
    /// appending a frame. The `poll` path branches on this so a frame
    /// decode doesn't accidentally overwrite the base, and a base
    /// decode doesn't accidentally compose against a partially-loaded
    /// frame sequence.
    kind: PendingKind,
    issued_at: Instant,
    timeout: Duration,
}

#[derive(Clone, Copy, Debug)]
enum PendingKind {
    /// First-pass decode for an `a=t` / `a=T` payload. Result fills
    /// `StoredImage::image` (+ preview, plus `base_rgba` when
    /// `keep_rgba` is set so future `a=f` frames can composite).
    Base { keep_rgba: bool },
    /// `a=f` frame transmission. Result is composed against the
    /// indicated base frame and pushed onto `StoredImage::frames`.
    Frame {
        /// Pixel position in the parent image where the new frame's
        /// data lands.
        dst_x: u32,
        dst_y: u32,
        /// 0-based index into (base + frames) of the source to use as
        /// the composition base. `0` = the base RGBA, `n` = frame
        /// `n - 1`. `None` (or a stale index) falls back to the base.
        compose_base: Option<u32>,
        /// Gap before this frame advances, in milliseconds.
        delay_ms: u32,
        /// `Some(n)` (1-based, n >= 1) replaces the frame at that slot
        /// in `frames` (index `n - 2` since slot 1 is the base).
        /// `None` or `Some(0)` appends.
        target_slot: Option<u32>,
    },
}

/// Decode + GPU residency cache for images.
///
/// Decode runs on a worker thread; upload happens on the main thread in
/// `poll` (wgpu is single-threaded). Refcounting is mark-and-sweep via
/// `retain` so callers don't manage acquire/release.
pub struct Store {
    next_id: u32,
    images: HashMap<u32, StoredImage>,
    total_bytes: usize,
    cap_bytes: usize,

    next_pending: u32,
    pending: HashMap<u32, PendingRequest>,

    job_tx: mpsc::Sender<DecodeJob>,
    result_rx: mpsc::Receiver<DecodeResult>,

    /// Bypass queue for raw RGB/RGBA payloads. The Kitty `t=s` SHM
    /// path lets icat hand us pre-decoded pixel data; PNG-encoding
    /// it just so the worker can PNG-decode it back wastes ~50-150ms
    /// per frame at 450x450 — enough to bust the per-job timeout
    /// when an animation queues 20+ frames in a single PTY chunk.
    /// `request_insert_*_rgba` pushes a pre-resolved `DecodeResult`
    /// here; `poll` drains alongside the worker channel.
    immediate_results: Vec<DecodeResult>,

    // Kept so the worker thread is observable in diagnostics. We don't
    // join it on drop — dropping `job_tx` closes the channel and the
    // worker's `recv` returns Err, ending its loop after the current
    // decode finishes.
    _worker: thread::JoinHandle<()>,
}

/// 256 MiB default cap on total decoded bytes. Slice 7 makes this config-
/// driven. A single 4K screenshot is ~32 MiB so this fits ~8 of them.
pub const DEFAULT_CAP_BYTES: usize = 256 * 1024 * 1024;

impl Store {
    pub fn new(cap_bytes: usize) -> Self {
        let (job_tx, job_rx) = mpsc::channel::<DecodeJob>();
        let (result_tx, result_rx) = mpsc::channel::<DecodeResult>();
        let worker = thread::Builder::new()
            .name("image-decode".into())
            .spawn(move || decode_worker_loop(job_rx, result_tx))
            .expect("spawn image decode worker");
        Self {
            next_id: 1,
            images: HashMap::new(),
            total_bytes: 0,
            cap_bytes,
            next_pending: 1,
            pending: HashMap::new(),
            job_tx,
            result_rx,
            immediate_results: Vec::new(),
            _worker: worker,
        }
    }

    /// Queue a decode + upload. Non-blocking — the worker runs the decode,
    /// `poll` finishes it. The returned `ImageId` is reserved immediately
    /// so callers (parsers, the debug keybind) can create a `Placement`
    /// referencing it before pixel data exists; `peek(id)` returns `None`
    /// in the meantime and the renderer skips drawing.
    ///
    /// This shape exists so that PTY-stream-driven inserts can advance the
    /// cursor and place the image in the grid synchronously: subsequent
    /// scroll/erase/resize ops then maintain the placement's anchor
    /// correctly, instead of the placement landing at a stale cell once
    /// decode finishes 50ms later.
    pub fn request_insert(
        &mut self,
        bytes: Vec<u8>,
        max_pixels: u64,
        timeout: Duration,
        label: Option<String>,
    ) -> (PendingId, ImageId) {
        self.request_insert_inner(bytes, max_pixels, timeout, label, false)
    }

    /// Variant of [`Store::request_insert`] that keeps a CPU RGBA
    /// mirror of the decoded image alongside the GPU texture. The
    /// mirror is required for `a=f` frame compositing — without it
    /// the store would have to read back from the GPU each time a
    /// frame arrives, which is sync + slow.
    ///
    /// Only the Kitty `a=T` / `a=t` ingest path opts in. Other inserts
    /// (iTerm OSC 1337, the debug keybind) skip the mirror because
    /// they can never receive frames and the doubled memory budget
    /// would just be waste.
    pub fn request_insert_animatable(
        &mut self,
        bytes: Vec<u8>,
        max_pixels: u64,
        timeout: Duration,
        label: Option<String>,
    ) -> (PendingId, ImageId) {
        self.request_insert_inner(bytes, max_pixels, timeout, label, true)
    }

    fn request_insert_inner(
        &mut self,
        bytes: Vec<u8>,
        max_pixels: u64,
        timeout: Duration,
        label: Option<String>,
        keep_rgba: bool,
    ) -> (PendingId, ImageId) {
        let image_id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        self.images.insert(
            image_id,
            StoredImage {
                image: None,
                preview: None,
                bytes: 0,
                last_used: Instant::now(),
                base_rgba: None,
                base_dims: None,
                frames: Vec::new(),
                animation: AnimationState::default(),
                base_delay_ms: 0,
            },
        );

        let pending_id = self.next_pending;
        self.next_pending = self.next_pending.wrapping_add(1).max(1);
        self.pending.insert(
            pending_id,
            PendingRequest {
                image_id,
                kind: PendingKind::Base { keep_rgba },
                issued_at: Instant::now(),
                timeout,
            },
        );

        // Channel send only fails if the worker has died, which only
        // happens if `_worker` has been joined or the store dropped —
        // neither expected at runtime. A failed send leaves the pending
        // request in the map; it will eventually time out in `poll`,
        // which also evicts the reservation.
        let _ = self.job_tx.send(DecodeJob {
            pending_id,
            bytes,
            max_pixels,
            label,
        });
        (PendingId(pending_id), ImageId(image_id))
    }

    /// Queue a Kitty `a=f` frame decode + composite. The decode runs on
    /// the worker as a normal image decode; `poll` then composites the
    /// result onto the chosen source frame's CPU RGBA and uploads the
    /// composed full-frame buffer as a fresh GPU texture.
    ///
    /// Returns `None` if the parent image doesn't exist (e.g. an
    /// `a=f,i=N` arrived before `a=t,i=N`). The frame is silently
    /// dropped in that case; surfacing the error wouldn't help any
    /// caller currently.
    pub fn request_insert_frame(
        &mut self,
        parent: ImageId,
        bytes: Vec<u8>,
        max_pixels: u64,
        timeout: Duration,
        label: Option<String>,
        target_slot: Option<u32>,
        compose_base: Option<u32>,
        delay_ms: u32,
        dst_x: u32,
        dst_y: u32,
    ) -> Option<PendingId> {
        // Parent must exist (even if its decode is still in flight).
        // `a=f` against an unknown id is undefined per the spec — we
        // drop silently rather than allocating a stranded pending
        // entry that no `image_id` ever resolves to.
        self.images.get(&parent.0)?;

        // Map a Kitty 1-based compose-base hint to our 0-based
        // (base + frames) index. `None` / `Some(0)` / `Some(1)` all
        // resolve to the base (0); `Some(n)` (n >= 2) resolves to
        // `n - 1` so that Kitty frame 2 = index 1 = frames[0], etc.
        let compose_base_idx = compose_base.and_then(|n| {
            if n <= 1 {
                Some(0u32)
            } else {
                Some(n - 1)
            }
        });

        let pending_id = self.next_pending;
        self.next_pending = self.next_pending.wrapping_add(1).max(1);
        self.pending.insert(
            pending_id,
            PendingRequest {
                image_id: parent.0,
                kind: PendingKind::Frame {
                    dst_x,
                    dst_y,
                    compose_base: compose_base_idx,
                    delay_ms,
                    target_slot,
                },
                issued_at: Instant::now(),
                timeout,
            },
        );

        let _ = self.job_tx.send(DecodeJob {
            pending_id,
            bytes,
            max_pixels,
            label,
        });
        Some(PendingId(pending_id))
    }

    /// Animatable base-image insert for callers who already have RGBA
    /// bytes (e.g. Kitty `t=s` SHM with raw f=24/f=32 payloads).
    /// Bypasses the worker entirely — the decode step would be a
    /// no-op since the bytes are already in the format
    /// `pipeline.upload_rgba` expects, and PNG-round-tripping a
    /// large RGBA buffer to satisfy the worker's signature wastes
    /// ~50-150 ms per call on 450x450 noisy data. Caller is
    /// responsible for converting raw RGB → RGBA (pad alpha=255)
    /// before calling.
    pub fn request_insert_animatable_rgba(
        &mut self,
        rgba: Vec<u8>,
        width: u32,
        height: u32,
        label: Option<String>,
    ) -> (PendingId, ImageId) {
        let image_id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        self.images.insert(
            image_id,
            StoredImage {
                image: None,
                preview: None,
                bytes: 0,
                last_used: Instant::now(),
                base_rgba: None,
                base_dims: None,
                frames: Vec::new(),
                animation: AnimationState::default(),
                base_delay_ms: 0,
            },
        );

        let pending_id = self.next_pending;
        self.next_pending = self.next_pending.wrapping_add(1).max(1);
        // The bypass path can't time out (no worker queue to wait in),
        // but the pending entry still needs a timeout so `poll`'s
        // sweep doesn't panic on missing fields. Pick something
        // generous; the immediate_results push below means the entry
        // resolves on the very next poll anyway.
        self.pending.insert(
            pending_id,
            PendingRequest {
                image_id,
                kind: PendingKind::Base { keep_rgba: true },
                issued_at: Instant::now(),
                timeout: Duration::from_secs(60),
            },
        );
        self.immediate_results.push(DecodeResult {
            pending_id,
            label,
            outcome: Ok(DecodedPixels { rgba, width, height }),
        });
        (PendingId(pending_id), ImageId(image_id))
    }

    /// Frame insert for callers who already have RGBA bytes.
    /// Same bypass rationale as [`Store::request_insert_animatable_rgba`]
    /// — the worker would only be doing a memcpy for a raw payload,
    /// and serializing tens of those at animation submission time
    /// busts the per-job decode timeout. Returns `None` if the
    /// parent doesn't exist.
    #[allow(clippy::too_many_arguments)]
    pub fn request_insert_frame_rgba(
        &mut self,
        parent: ImageId,
        rgba: Vec<u8>,
        width: u32,
        height: u32,
        label: Option<String>,
        target_slot: Option<u32>,
        compose_base: Option<u32>,
        delay_ms: u32,
        dst_x: u32,
        dst_y: u32,
    ) -> Option<PendingId> {
        self.images.get(&parent.0)?;
        let compose_base_idx = compose_base.and_then(|n| {
            if n <= 1 {
                Some(0u32)
            } else {
                Some(n - 1)
            }
        });
        let pending_id = self.next_pending;
        self.next_pending = self.next_pending.wrapping_add(1).max(1);
        self.pending.insert(
            pending_id,
            PendingRequest {
                image_id: parent.0,
                kind: PendingKind::Frame {
                    dst_x,
                    dst_y,
                    compose_base: compose_base_idx,
                    delay_ms,
                    target_slot,
                },
                issued_at: Instant::now(),
                timeout: Duration::from_secs(60),
            },
        );
        self.immediate_results.push(DecodeResult {
            pending_id,
            label,
            outcome: Ok(DecodedPixels { rgba, width, height }),
        });
        Some(PendingId(pending_id))
    }

    /// `a=a` per-image mutation hook. Mutates the play state and / or
    /// individual frame fields based on which sub-op the parser
    /// detected. Bare `now` reset keeps `peek_at` aligned with the
    /// caller's wall clock without forcing every call site to thread
    /// an Instant explicitly.
    pub fn apply_animation_control(
        &mut self,
        id: ImageId,
        control: Option<u32>,
        loop_count: Option<u32>,
        make_current: Option<u32>,
        edit_frame: Option<u32>,
        edit_gap_ms: Option<u32>,
        now: Instant,
    ) {
        let Some(img) = self.images.get_mut(&id.0) else { return };
        // `r=N z=M` per-frame edit applies independently of the other
        // ops (the Kitty spec lets a single `a=a` both edit a gap and
        // change play state).
        if let (Some(n), Some(gap)) = (edit_frame, edit_gap_ms) {
            // 1-based; n == 1 is the base, which has its own
            // `base_delay_ms` slot. Frames vec is 0-indexed starting
            // at Kitty frame 2.
            if n == 1 {
                img.base_delay_ms = gap;
            } else if n >= 2 {
                let idx = (n - 2) as usize;
                if let Some(frame) = img.frames.get_mut(idx) {
                    frame.delay_ms = gap;
                }
            }
        }
        // `c=N` make-current and `s=N` play-mode control are mutually
        // exclusive — the Kitty spec doesn't define a single message
        // that both pins a frame and starts playback. When the parser
        // sees both keys (e.g. an app that sends an ambiguous payload)
        // we prefer `make_current` since it has fewer side effects
        // (no clock reset on top of the play-mode change).
        if let Some(n) = make_current {
            let frame_count = (img.frames.len() as u32) + 1; // base + frames
            // Clamp to [0, frame_count-1]; `n` is 1-based.
            let target = n.saturating_sub(1).min(frame_count - 1);
            img.animation.current_frame = target;
            img.animation.play_mode = PlayMode::Stopped;
            img.animation.current_frame_started = now;
        } else if let Some(c) = control {
            img.animation.play_mode = match c {
                1 => PlayMode::Stopped,
                2 => PlayMode::RunWhileLoading,
                3 => match loop_count {
                    Some(0) | None => PlayMode::LoopForever,
                    Some(n) => PlayMode::LoopFinite { remaining: n },
                },
                _ => img.animation.play_mode, // unknown control op = no-op
            };
            // Reset the per-frame clock so playback starts from now;
            // the previous current_frame stays as the starting frame.
            img.animation.current_frame_started = now;
        }
    }

    /// Build the GpuImage + bookkeeping for a base-image decode. Split
    /// out of `poll` so the per-result switch reads as a flat
    /// `kind → finish_*` dispatch.
    #[allow(clippy::too_many_arguments)]
    fn finish_base_decode(
        &mut self,
        image_id: u32,
        decoded: DecodedPixels,
        keep_rgba: bool,
        label: Option<&str>,
        pipeline: &ImagePipeline,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        nearest_filter: bool,
    ) -> Result<ImageId, DecodeError> {
        let gpu_bytes = (decoded.width as usize) * (decoded.height as usize) * 4;
        // We charge the GPU texture against the cap, plus the CPU
        // RGBA mirror when the Kitty animation path requested one
        // (see `request_insert_animatable`). Non-animatable inserts
        // (iTerm OSC, the debug keybind) pay GPU-only.
        let total_bytes = if keep_rgba { gpu_bytes.saturating_mul(2) } else { gpu_bytes };
        if self.total_bytes.saturating_add(total_bytes) > self.cap_bytes {
            self.images.remove(&image_id);
            let available = self.cap_bytes.saturating_sub(self.total_bytes);
            return Err(DecodeError::BudgetExceeded { needed: total_bytes, available });
        }
        let image = pipeline.upload_rgba(
            device,
            queue,
            &decoded.rgba,
            decoded.width,
            decoded.height,
            nearest_filter,
            label,
        );
        let preview = build_preview(&decoded.rgba, decoded.width, decoded.height);
        let entry = self
            .images
            .get_mut(&image_id)
            .expect("reservation present until decode resolves");
        entry.image = Some(image);
        entry.preview = preview;
        entry.bytes = total_bytes;
        entry.last_used = Instant::now();
        entry.base_dims = Some((decoded.width, decoded.height));
        if keep_rgba {
            entry.base_rgba = Some(decoded.rgba);
        }
        self.total_bytes += total_bytes;
        Ok(ImageId(image_id))
    }

    /// Build a composed frame from a decoded `a=f` payload. The new
    /// frame's pixel data is alpha-blended onto a copy of the source
    /// frame (per `compose_base`) at `(dst_x, dst_y)`, then uploaded
    /// as a full-parent-dimension GPU texture.
    #[allow(clippy::too_many_arguments)]
    fn finish_frame_decode(
        &mut self,
        parent_id: u32,
        decoded: DecodedPixels,
        dst_x: u32,
        dst_y: u32,
        compose_base: Option<u32>,
        delay_ms: u32,
        target_slot: Option<u32>,
        label: Option<&str>,
        pipeline: &ImagePipeline,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        nearest_filter: bool,
    ) -> Result<ImageId, DecodeError> {
        let parent = self
            .images
            .get(&parent_id)
            .ok_or(DecodeError::ParentNotReady)?;
        let Some((base_w, base_h)) = parent.base_dims else {
            // Parent's base decode hasn't landed yet — we can't
            // composite without dimensions. Surface as
            // `ParentNotReady` so callers can distinguish from a real
            // OOM / cap-exceeded event.
            return Err(DecodeError::ParentNotReady);
        };

        let parent_bytes = (base_w as usize) * (base_h as usize) * 4;
        // Frame adds CPU + GPU mirror just like the base.
        let total_bytes = parent_bytes.saturating_mul(2);
        if self.total_bytes.saturating_add(total_bytes) > self.cap_bytes {
            let available = self.cap_bytes.saturating_sub(self.total_bytes);
            return Err(DecodeError::BudgetExceeded { needed: total_bytes, available });
        }

        // Composition strategy.
        //
        // Full-frame replacement (no `c=` AND same dims as parent AND
        // dst at origin): the new payload IS the new frame. icat
        // sends transparent-GIF frames this way — each frame is the
        // complete RGBA snapshot for that point in time, with its
        // own transparency. Alpha-blending onto the prior frame would
        // make the prior's pixels bleed through wherever the new frame
        // is transparent, leaving the first frame visibly stuck behind
        // the animation. The Kitty spec defines `X=1` for this
        // semantic but icat doesn't send it; treat the shape itself
        // as the signal (per icat's de-facto convention).
        //
        // Otherwise: alpha-blend onto the chosen compose source.
        //   `Some(0)` → base
        //   `Some(n)` → frames[n - 1]
        //   `None`    → previous frame (Kitty spec default), falling
        //               back to base for the very first a=f.
        // Stale or out-of-range explicit indices fall back to the
        // base as a last resort — composing onto blank would hide
        // the rest of the image.
        let is_full_replacement = compose_base.is_none()
            && dst_x == 0
            && dst_y == 0
            && decoded.width == base_w
            && decoded.height == base_h;
        let composed: Vec<u8> = if is_full_replacement {
            decoded.rgba.clone()
        } else {
            let base_rgba_or_blank = || {
                parent
                    .base_rgba
                    .clone()
                    .unwrap_or_else(|| vec![0u8; parent_bytes])
            };
            let src_rgba: Vec<u8> = match compose_base {
                Some(0) => base_rgba_or_blank(),
                Some(n) => parent
                    .frames
                    .get((n as usize).saturating_sub(1))
                    .map(|f| f.rgba.clone())
                    .unwrap_or_else(base_rgba_or_blank),
                None => parent
                    .frames
                    .last()
                    .map(|f| f.rgba.clone())
                    .unwrap_or_else(base_rgba_or_blank),
            };
            composite_rgba(
                &src_rgba,
                base_w,
                base_h,
                &decoded.rgba,
                decoded.width,
                decoded.height,
                dst_x,
                dst_y,
            )
        };

        let image = pipeline.upload_rgba(
            device,
            queue,
            &composed,
            base_w,
            base_h,
            nearest_filter,
            label,
        );
        let frame = Frame {
            image,
            rgba: composed,
            delay_ms,
            bytes: total_bytes,
        };

        let parent = self
            .images
            .get_mut(&parent_id)
            .expect("checked above and not removed since");
        // Target slot: 1-based, where 1 = base (we don't allow
        // replacing the base via `a=f` — apps re-transmit with `a=t`
        // for that). `Some(0)` / `None` append; `Some(n)` (n >= 2)
        // replaces frames[n - 2] if it exists, else appends.
        let replaced_bytes = match target_slot {
            Some(n) if n >= 2 && ((n - 2) as usize) < parent.frames.len() => {
                let idx = (n - 2) as usize;
                let old = std::mem::replace(&mut parent.frames[idx], frame);
                old.bytes
            }
            _ => {
                parent.frames.push(frame);
                0
            }
        };
        // Per-image bytes mirror the global cap so `retain` can
        // recompute the global from the surviving images. Replacement
        // has to subtract the displaced frame's bytes here, not just
        // at the global level — otherwise repeated edits make
        // `parent.bytes` drift upward and the next `retain` carries
        // the drift into `total_bytes`.
        parent.bytes = parent
            .bytes
            .saturating_add(total_bytes)
            .saturating_sub(replaced_bytes);
        parent.last_used = Instant::now();
        self.total_bytes = self
            .total_bytes
            .saturating_add(total_bytes)
            .saturating_sub(replaced_bytes);
        Ok(ImageId(parent_id))
    }

    /// Drain finished decode jobs, upload them to the GPU, and surface
    /// timeouts. Call once per frame from the render loop.
    ///
    /// `nearest_filter` selects the sampler used for new uploads (config
    /// `images_filter` in slice 7). Same value applies to every upload in
    /// the batch — config flips between frames is fine.
    pub fn poll(
        &mut self,
        pipeline: &ImagePipeline,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        nearest_filter: bool,
    ) -> Vec<(PendingId, Result<ImageId, DecodeError>)> {
        let mut out = Vec::new();
        let now = Instant::now();

        // Time out pending requests whose deadline has passed. Doing this
        // before draining the result channel means a request whose result
        // arrived in the same poll-tick that crossed its deadline still
        // gets honored (we'd dequeue from `pending` either way — but the
        // result-channel branch wins, returning the actual outcome).
        let mut timed_out: Vec<u32> = Vec::new();
        for (&pid, req) in &self.pending {
            if now.duration_since(req.issued_at) > req.timeout {
                timed_out.push(pid);
            }
        }
        for pid in timed_out {
            let req = self
                .pending
                .remove(&pid)
                .expect("timed-out id came from pending");
            // Drop the pre-allocated reservation so any placement still
            // referencing this id renders blank from now on (and main.rs
            // can clean the placement up via the failure-cleanup path).
            self.images.remove(&req.image_id);
            out.push((
                PendingId(pid),
                Err(DecodeError::TimedOut {
                    elapsed_ms: now.duration_since(req.issued_at).as_millis() as u64,
                }),
            ));
        }

        // Drain the bypass queue (raw RGB/RGBA inserts) alongside the
        // worker channel. Both branches resolve through the same
        // finish_*_decode helpers; the only difference is where the
        // `DecodedPixels` came from. Worker results are collected
        // up-front so the loop body can mutate `self` (the from-fn
        // iterator borrow would conflict otherwise).
        let mut to_process: Vec<DecodeResult> =
            std::mem::take(&mut self.immediate_results);
        while let Ok(result) = self.result_rx.try_recv() {
            to_process.push(result);
        }
        for result in to_process {
            let pid = result.pending_id;
            // If the matching pending entry is gone, the request timed out
            // already — silently drop the late result. The reservation
            // was already removed in the timeout branch above.
            let Some(req) = self.pending.remove(&pid) else { continue };
            let outcome = match result.outcome {
                Err(e) => {
                    // Decode failure: only evict the base reservation.
                    // A failed frame decode leaves the parent intact —
                    // dropping the whole image just because one frame
                    // is malformed is too aggressive.
                    if matches!(req.kind, PendingKind::Base { .. }) {
                        self.images.remove(&req.image_id);
                    }
                    Err(e)
                }
                Ok(decoded) => match req.kind {
                    PendingKind::Base { keep_rgba } => self.finish_base_decode(
                        req.image_id,
                        decoded,
                        keep_rgba,
                        result.label.as_deref(),
                        pipeline,
                        device,
                        queue,
                        nearest_filter,
                    ),
                    PendingKind::Frame {
                        dst_x,
                        dst_y,
                        compose_base,
                        delay_ms,
                        target_slot,
                    } => self.finish_frame_decode(
                        req.image_id,
                        decoded,
                        dst_x,
                        dst_y,
                        compose_base,
                        delay_ms,
                        target_slot,
                        result.label.as_deref(),
                        pipeline,
                        device,
                        queue,
                        nearest_filter,
                    ),
                },
            };
            out.push((PendingId(pid), outcome));
        }

        out
    }

    /// Number of in-flight decode requests. Useful for diagnostics — a
    /// number that doesn't shrink across frames suggests the worker is
    /// stuck or the result channel is starving.
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// Look up a previously-inserted image. Bumps its `last_used`. Returns
    /// `None` for unknown ids AND for pre-allocated ids whose decode is
    /// still in flight — callers should treat both the same way (skip).
    pub fn get(&mut self, id: ImageId) -> Option<&GpuImage> {
        let entry = self.images.get_mut(&id.0)?;
        entry.last_used = Instant::now();
        entry.image.as_ref()
    }

    /// Read-only lookup; doesn't bump LRU. The renderer uses this so
    /// rendering doesn't unnecessarily reshuffle the `last_used` ordering.
    /// Returns `None` in the same cases as `get`.
    ///
    /// For animated images this returns the base frame (frame 1).
    /// Animation-aware callers should use [`Store::peek_at`] instead so
    /// the wall-clock advance lands on the right frame.
    pub fn peek(&self, id: ImageId) -> Option<&GpuImage> {
        self.images.get(&id.0).and_then(|e| e.image.as_ref())
    }

    /// Read-only lookup that selects the correct frame for an animated
    /// image given the current wall clock. For a static image (no
    /// frames added past the base) this is identical to [`Store::peek`].
    ///
    /// `now` is the caller's monotonic clock; passing a stale value
    /// just freezes the animation at whatever frame that timestamp
    /// resolves to, which is the right behavior for tests that need
    /// determinism.
    pub fn peek_at(&self, id: ImageId, now: Instant) -> Option<&GpuImage> {
        let entry = self.images.get(&id.0)?;
        let target = resolve_current_frame(entry, now);
        match target {
            0 => entry.image.as_ref(),
            n => entry
                .frames
                .get((n as usize).saturating_sub(1))
                .map(|f| &f.image)
                .or(entry.image.as_ref()),
        }
    }

    /// Number of frames past the base for `id` — `0` for a non-animated
    /// image, `n` for one with `n` `a=f` deliveries on top of the base.
    /// Test-only accessor; the renderer doesn't need this.
    #[cfg(test)]
    pub fn frame_count(&self, id: ImageId) -> usize {
        self.images.get(&id.0).map(|e| e.frames.len()).unwrap_or(0)
    }

    /// Test hook to inspect the playback state for an image. Returns
    /// `None` for unknown ids.
    #[cfg(test)]
    pub fn animation_state(&self, id: ImageId) -> Option<AnimationState> {
        self.images.get(&id.0).map(|e| e.animation)
    }

    /// Force playback state to the given values — used by the test
    /// suite so a test doesn't need to thread `Instant::now()` through
    /// the dispatcher just to set up a scenario. Production code goes
    /// through [`Store::apply_animation_control`].
    #[cfg(test)]
    pub fn force_animation_state_for_test(
        &mut self,
        id: ImageId,
        state: AnimationState,
    ) {
        if let Some(e) = self.images.get_mut(&id.0) {
            e.animation = state;
        }
    }

    /// Earliest wall-clock at which any animated image needs a render
    /// tick to advance to its next frame. The main loop threads this
    /// into its `WaitUntil` so playback doesn't stall between events.
    /// Returns `None` when no image is currently animating.
    pub fn next_frame_deadline(&self, now: Instant) -> Option<Instant> {
        let mut earliest: Option<Instant> = None;
        for entry in self.images.values() {
            if entry.frames.is_empty() {
                continue;
            }
            if !is_advancing(entry.animation.play_mode) {
                continue;
            }
            let cur = entry.animation.current_frame as usize;
            let total_frames = entry.frames.len() + 1; // base + frames
            // 0-based current_frame → delay sits on the *frame being
            // displayed*. Base uses its own `base_delay_ms` (set via
            // `a=a r=1 z=N`).
            let delay_ms = if cur == 0 {
                entry.base_delay_ms
            } else if cur < total_frames {
                entry.frames.get(cur - 1).map(|f| f.delay_ms).unwrap_or(0)
            } else {
                0
            };
            // delay_ms == 0 means "advance immediately" — schedule a
            // wakeup on the next tick so we don't busy-spin but the
            // frame still advances promptly.
            let delay = Duration::from_millis(delay_ms.max(1) as u64);
            let deadline = entry.animation.current_frame_started + delay;
            let deadline = deadline.max(now);
            earliest = Some(earliest.map(|e| e.min(deadline)).unwrap_or(deadline));
        }
        earliest
    }

    /// Read-only access to the half-block preview. Returns `None` for
    /// unknown ids AND for reservations whose decode is still in flight —
    /// callers should treat both as "no preview, skip the half-block draw".
    /// The preview lifetime tracks the GPU image: `retain` drops both
    /// together when the placement is gone.
    pub fn preview(&self, id: ImageId) -> Option<&Preview> {
        self.images.get(&id.0).and_then(|e| e.preview.as_ref())
    }

    /// True when the id is reserved but its decode hasn't completed yet.
    /// Distinct from `peek().is_none()` which also fires on unknown ids.
    /// Useful for diagnostics — a placement that hangs in this state past
    /// the configured timeout has hit a bug.
    pub fn is_pending(&self, id: ImageId) -> bool {
        self.images
            .get(&id.0)
            .map(|e| e.image.is_none())
            .unwrap_or(false)
    }

    /// Mark-and-sweep: drop every image whose id isn't in `keep`. Called by
    /// the renderer each frame after collecting live + scrollback placement
    /// ids. Calling with an empty set drops everything.
    pub fn retain(&mut self, keep: &HashSet<ImageId>) {
        let prev_bytes = self.total_bytes;
        self.images.retain(|id, _entry| keep.contains(&ImageId(*id)));
        // Recompute from the surviving set so an accounting bug can't
        // accumulate negative drift.
        self.total_bytes = self.images.values().map(|e| e.bytes).sum();
        debug_assert!(self.total_bytes <= prev_bytes);
    }

    /// Total bytes-on-GPU across all stored images. Approximate (4 bytes
    /// per pixel; ignores mipmaps and alignment padding).
    pub fn bytes(&self) -> usize {
        self.total_bytes
    }

    pub fn cap_bytes(&self) -> usize {
        self.cap_bytes
    }

    pub fn len(&self) -> usize {
        self.images.len()
    }

    pub fn is_empty(&self) -> bool {
        self.images.is_empty()
    }

    /// Test-only hatch: register a synthetic *already-filled* image with
    /// the given byte size so `Store`'s id allocation / retain / accounting
    /// can be tested without a real decode + GPU upload.
    #[cfg(test)]
    pub(crate) fn insert_synthetic_for_test(&mut self, gpu_image: GpuImage, bytes: usize) -> ImageId {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        let dims = (gpu_image.width_px, gpu_image.height_px);
        self.images.insert(
            id,
            StoredImage {
                image: Some(gpu_image),
                preview: None,
                bytes,
                last_used: Instant::now(),
                base_rgba: None,
                base_dims: Some(dims),
                frames: Vec::new(),
                animation: AnimationState::default(),
                base_delay_ms: 0,
            },
        );
        self.total_bytes += bytes;
        ImageId(id)
    }
}

fn decode_worker_loop(jobs: mpsc::Receiver<DecodeJob>, results: mpsc::Sender<DecodeResult>) {
    while let Ok(job) = jobs.recv() {
        let outcome = match decode_to_rgba(&job.bytes, job.max_pixels) {
            Ok((rgba, w, h)) => Ok(DecodedPixels { rgba, width: w, height: h }),
            Err(e) => Err(e),
        };
        // Discard send errors — the only cause is `result_rx` being
        // dropped, which means the Store is being torn down and there's
        // nobody to deliver to.
        let _ = results.send(DecodeResult {
            pending_id: job.pending_id,
            label: job.label,
            outcome,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encode a `w*h` RGBA PNG via the `image` crate. We could hand-roll
    /// bytes, but constructing through the same encoder/decoder pair real
    /// callers use keeps the test honest about format expectations.
    fn make_png(w: u32, h: u32) -> Vec<u8> {
        let buf = image::RgbaImage::from_pixel(w, h, image::Rgba([255, 0, 0, 255]));
        let mut bytes: Vec<u8> = Vec::new();
        image::DynamicImage::ImageRgba8(buf)
            .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageOutputFormat::Png)
            .expect("encode");
        bytes
    }

    #[test]
    fn decode_tiny_png_returns_one_pixel_rgba() {
        let png = make_png(1, 1);
        let (rgba, w, h) = decode_to_rgba(&png, 100).expect("decode");
        assert_eq!((w, h), (1, 1));
        assert_eq!(rgba.len(), 4);
        assert_eq!(&rgba, &[255, 0, 0, 255]);
    }

    #[test]
    fn decode_rejects_oversized_image_without_allocating() {
        // 4×4 = 16 pixels; cap of 4 forces the size check to fire on the
        // header read, before the full decode allocates anything.
        let png = make_png(4, 4);
        let err = decode_to_rgba(&png, 4).expect_err("expected size rejection");
        match err {
            DecodeError::TooLarge { pixels, max } => {
                assert_eq!(pixels, 16);
                assert_eq!(max, 4);
            }
            other => panic!("expected TooLarge, got {other:?}"),
        }
    }

    #[test]
    fn decode_rejects_malformed_bytes() {
        let err = decode_to_rgba(b"this is not a png", 1_000_000).expect_err("expected decode err");
        assert!(matches!(err, DecodeError::Decode(_)));
    }

    #[test]
    fn store_is_empty_on_construction() {
        let s = Store::new(DEFAULT_CAP_BYTES);
        assert_eq!(s.len(), 0);
        assert_eq!(s.bytes(), 0);
        assert!(s.is_empty());
        assert_eq!(s.pending_count(), 0);
        assert_eq!(s.cap_bytes(), DEFAULT_CAP_BYTES);
    }

    // GpuImage holds wgpu handles, so the synthetic-insert path needs a
    // device. We build a headless one here for the few tests that exercise
    // retain/get/accounting. Adapter request may fail in CI without GPU —
    // these tests skip themselves when that happens.
    fn try_make_pipeline_and_image() -> Option<(wgpu::Device, wgpu::Queue, ImagePipeline, GpuImage)> {
        let instance = wgpu::Instance::default();
        let adapter = pollster::block_on(instance.request_adapter(
            &wgpu::RequestAdapterOptions::default(),
        ))?;
        let (device, queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor::default(),
            None,
        ))
        .ok()?;
        let camera_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("test camera bgl"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let pipeline = ImagePipeline::new(&device, wgpu::TextureFormat::Rgba8UnormSrgb, &camera_bgl);
        let rgba = vec![0xFFu8; 4 * 4 * 4]; // 4x4 RGBA
        let image = pipeline.upload_rgba(&device, &queue, &rgba, 4, 4, false, Some("test"));
        Some((device, queue, pipeline, image))
    }

    #[test]
    fn store_retain_drops_unreferenced_images_and_recomputes_bytes() {
        let Some((_d, _q, _p, image)) = try_make_pipeline_and_image() else {
            eprintln!("skipping: no GPU adapter");
            return;
        };
        let mut s = Store::new(DEFAULT_CAP_BYTES);
        // Two synthetic entries — we only need one real GpuImage; the test
        // for retain/accounting doesn't care that both ids point at the
        // same underlying texture clone (we can't clone GpuImage, but we
        // can re-upload to get a second one).
        let (Some((_d2, _q2, _p2, image2)), Some((_d3, _q3, _p3, image3))) = (
            try_make_pipeline_and_image(),
            try_make_pipeline_and_image(),
        ) else {
            return;
        };
        let id_a = s.insert_synthetic_for_test(image, 64);
        let id_b = s.insert_synthetic_for_test(image2, 64);
        let _id_c = s.insert_synthetic_for_test(image3, 64);
        assert_eq!(s.len(), 3);
        assert_eq!(s.bytes(), 192);

        // Keep only a and b; c should drop.
        let keep: HashSet<ImageId> = [id_a, id_b].into_iter().collect();
        s.retain(&keep);
        assert_eq!(s.len(), 2);
        assert_eq!(s.bytes(), 128);
        assert!(s.peek(id_a).is_some());
        assert!(s.peek(id_b).is_some());

        // Empty set drops everything.
        s.retain(&HashSet::new());
        assert_eq!(s.len(), 0);
        assert_eq!(s.bytes(), 0);
    }

    #[test]
    fn store_get_returns_inserted_image_and_bumps_last_used() {
        let Some((_d, _q, _p, image)) = try_make_pipeline_and_image() else {
            eprintln!("skipping: no GPU adapter");
            return;
        };
        let mut s = Store::new(DEFAULT_CAP_BYTES);
        let id = s.insert_synthetic_for_test(image, 16);
        let before = s.images[&id.0].last_used;
        // Sleep just enough that Instant tick is observable on all platforms.
        std::thread::sleep(std::time::Duration::from_millis(2));
        assert!(s.get(id).is_some());
        let after = s.images[&id.0].last_used;
        assert!(after > before, "get() should refresh last_used");
    }

    #[test]
    fn store_ids_are_monotonic_and_dont_reuse_after_drop() {
        let Some((_d, _q, _p, image)) = try_make_pipeline_and_image() else {
            eprintln!("skipping: no GPU adapter");
            return;
        };
        let Some((_d2, _q2, _p2, image2)) = try_make_pipeline_and_image() else {
            return;
        };
        let mut s = Store::new(DEFAULT_CAP_BYTES);
        let id1 = s.insert_synthetic_for_test(image, 1);
        s.retain(&HashSet::new()); // drop id1
        let id2 = s.insert_synthetic_for_test(image2, 1);
        assert_ne!(id1, id2, "id reuse would break stale-placement detection");
    }

    //
    // Async path tests.
    //

    #[test]
    fn request_insert_returns_distinct_pending_ids() {
        let mut s = Store::new(DEFAULT_CAP_BYTES);
        let (pa, ia) = s.request_insert(b"x".to_vec(), 1, Duration::from_secs(1), None);
        let (pb, ib) = s.request_insert(b"y".to_vec(), 1, Duration::from_secs(1), None);
        assert_ne!(pa, pb);
        assert_ne!(ia, ib, "reserved ImageIds must also be distinct");
        assert_eq!(s.pending_count(), 2);
    }

    #[test]
    fn request_insert_reserves_image_id_immediately_with_no_pixels() {
        let mut s = Store::new(DEFAULT_CAP_BYTES);
        let (_pending, image_id) = s.request_insert(
            make_png(2, 2),
            100,
            Duration::from_secs(5),
            None,
        );
        // Reserved entry exists but has no pixels yet — peek returns None,
        // is_pending fires, byte budget unaffected.
        assert!(s.peek(image_id).is_none());
        assert!(s.is_pending(image_id));
        assert_eq!(s.bytes(), 0);
        // Slot still counts toward len so retain doesn't accidentally drop
        // the reservation between request_insert and the next poll.
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn retain_keeps_pending_reservation_alive() {
        let mut s = Store::new(DEFAULT_CAP_BYTES);
        let (_pending, image_id) = s.request_insert(
            make_png(2, 2),
            100,
            Duration::from_secs(5),
            None,
        );
        // Caller has just placed a Placement referencing image_id.
        // Mark-and-sweep on that single id must NOT drop the reservation —
        // otherwise the renderer would never see the eventual decoded image.
        let keep: HashSet<ImageId> = std::iter::once(image_id).collect();
        s.retain(&keep);
        assert!(s.is_pending(image_id));
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn poll_times_out_old_requests() {
        let Some((d, q, p, _)) = try_make_pipeline_and_image() else {
            eprintln!("skipping: no GPU adapter");
            return;
        };
        let mut s = Store::new(DEFAULT_CAP_BYTES);
        // Zero-timeout request — any non-zero wait crosses the deadline.
        let (pending, image_id) = s.request_insert(b"x".to_vec(), 1, Duration::ZERO, None);
        assert!(s.is_pending(image_id), "reservation present before poll");
        std::thread::sleep(Duration::from_millis(2));
        let results = s.poll(&p, &d, &q, false);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, pending);
        assert!(matches!(results[0].1, Err(DecodeError::TimedOut { .. })));
        assert_eq!(s.pending_count(), 0);
        // Timeout evicts the reservation so the orphaned id stops
        // counting toward the store's len.
        assert!(s.peek(image_id).is_none());
        assert!(!s.is_pending(image_id));
        assert_eq!(s.len(), 0);
    }

    #[test]
    fn poll_uploads_decoded_image_to_store() {
        let Some((d, q, p, _)) = try_make_pipeline_and_image() else {
            eprintln!("skipping: no GPU adapter");
            return;
        };
        let mut s = Store::new(DEFAULT_CAP_BYTES);
        let png = make_png(2, 2);
        let (pending, reserved) = s.request_insert(png, 100, Duration::from_secs(5), Some("test".into()));
        // Spin until worker delivers (or timeout — tests shouldn't hang).
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut results = Vec::new();
        while results.is_empty() && Instant::now() < deadline {
            results = s.poll(&p, &d, &q, false);
            if results.is_empty() {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        assert_eq!(results.len(), 1, "decode didn't complete in time");
        assert_eq!(results[0].0, pending);
        let id = results[0].1.as_ref().expect("decode succeeded").clone();
        // Critically: poll returns the SAME id that request_insert
        // reserved — otherwise placements would dangle.
        assert_eq!(id, reserved);
        assert!(s.peek(id).is_some());
        assert!(!s.is_pending(id));
        // 2*2*4 = 16 bytes.
        assert_eq!(s.bytes(), 16);
    }

    #[test]
    fn poll_refuses_upload_that_would_exceed_cap() {
        let Some((d, q, p, _)) = try_make_pipeline_and_image() else {
            eprintln!("skipping: no GPU adapter");
            return;
        };
        // 4x4 RGBA = 64 bytes. Cap at 32 → refuse on upload.
        let mut s = Store::new(32);
        let png = make_png(4, 4);
        let (pending, reserved) = s.request_insert(png, 100, Duration::from_secs(5), None);
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut results = Vec::new();
        while results.is_empty() && Instant::now() < deadline {
            results = s.poll(&p, &d, &q, false);
            if results.is_empty() {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, pending);
        assert!(matches!(
            results[0].1,
            Err(DecodeError::BudgetExceeded { needed: 64, available: 32 })
        ));
        // Store stays empty — refused upload evicts the reservation too,
        // so the orphaned id doesn't linger.
        assert!(s.is_empty());
        assert_eq!(s.bytes(), 0);
        assert!(s.peek(reserved).is_none());
    }

    #[test]
    fn poll_surfaces_decode_errors() {
        let Some((d, q, p, _)) = try_make_pipeline_and_image() else {
            eprintln!("skipping: no GPU adapter");
            return;
        };
        let mut s = Store::new(DEFAULT_CAP_BYTES);
        let (pending, reserved) = s.request_insert(b"not a png".to_vec(), 100, Duration::from_secs(5), None);
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut results = Vec::new();
        while results.is_empty() && Instant::now() < deadline {
            results = s.poll(&p, &d, &q, false);
            if results.is_empty() {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, pending);
        assert!(matches!(results[0].1, Err(DecodeError::Decode(_))));
    }

    #[test]
    fn poll_with_no_results_returns_empty() {
        let Some((d, q, p, _)) = try_make_pipeline_and_image() else {
            eprintln!("skipping: no GPU adapter");
            return;
        };
        let mut s = Store::new(DEFAULT_CAP_BYTES);
        let results = s.poll(&p, &d, &q, false);
        assert!(results.is_empty());
    }

    #[test]
    fn store_get_returns_none_for_unknown_id() {
        // Lookups for stale ids must return None rather than panic — the
        // renderer keeps placement ids around for a frame after retain has
        // dropped the underlying image, and that lookup needs to be benign.
        let mut s = Store::new(DEFAULT_CAP_BYTES);
        assert!(s.get(ImageId(1)).is_none());
        assert!(s.get(ImageId(99_999)).is_none());
    }

    #[test]
    fn store_peek_returns_none_for_unknown_id() {
        let s = Store::new(DEFAULT_CAP_BYTES);
        assert!(s.peek(ImageId(1)).is_none());
        assert!(s.peek(ImageId(99_999)).is_none());
    }

    #[test]
    fn decode_error_display_includes_useful_context() {
        // Display strings are surfaced in logs and (eventually) status-line
        // diagnostics; each variant must carry its identifying numbers so a
        // user can tell "too large" from "budget exceeded" without inspecting
        // the source.
        let too_large = DecodeError::TooLarge { pixels: 1024, max: 256 };
        let s = format!("{too_large}");
        assert!(s.contains("1024"), "got: {s}");
        assert!(s.contains("256"), "got: {s}");

        let timed_out = DecodeError::TimedOut { elapsed_ms: 1500 };
        let s = format!("{timed_out}");
        assert!(s.contains("1500"), "got: {s}");
        assert!(s.to_lowercase().contains("tim"), "got: {s}");

        let budget = DecodeError::BudgetExceeded { needed: 4096, available: 100 };
        let s = format!("{budget}");
        assert!(s.contains("4096"), "got: {s}");
        assert!(s.contains("100"), "got: {s}");

        // Decode wraps an image::ImageError — just confirm it doesn't panic
        // and includes the inner error's text in some form.
        let inner = image::ImageError::Limits(image::error::LimitError::from_kind(
            image::error::LimitErrorKind::DimensionError,
        ));
        let decode = DecodeError::Decode(inner);
        let s = format!("{decode}");
        assert!(s.contains("decode"), "got: {s}");
    }

    //
    // Half-block preview / fallback path tests.
    //
    // (b)-strategy lives or dies on these: the preview is small, must be
    // deterministic for the same input, and the cell math must land each
    // half-block on exactly the right two preview pixels.
    //

    #[test]
    fn build_preview_passes_through_when_image_fits_in_budget() {
        // A 4×4 source fits comfortably under MAX_PREVIEW_* — output stays
        // 4×4 (no upscale) and the pixel data round-trips byte-for-byte
        // through the single-pixel box-filter cell.
        let mut src = Vec::with_capacity(4 * 4 * 4);
        for i in 0..16 {
            // Distinct per-pixel colors so any swap or off-by-one in the
            // sampler is immediately visible.
            src.extend_from_slice(&[i as u8 * 17, 0, 0, 255]);
        }
        let p = build_preview(&src, 4, 4).expect("preview built");
        assert_eq!((p.width, p.height), (4, 4));
        assert_eq!(p.rgba, src);
    }

    #[test]
    fn build_preview_downsamples_oversized_image_within_budget() {
        // 1000-wide source forces a downscale; verify the result fits the
        // budget and preserves aspect to within rounding.
        let w = 1000u32;
        let h = 500u32;
        let src = vec![128u8; (w as usize) * (h as usize) * 4];
        let p = build_preview(&src, w, h).expect("preview built");
        assert!(p.width <= MAX_PREVIEW_COLS);
        assert!(p.height <= MAX_PREVIEW_ROWS);
        assert!(p.width >= 1 && p.height >= 1);
        // 2:1 aspect ratio in, ≈2:1 ratio out. Allow ±1 for rounding.
        let ratio = (p.width as f32) / (p.height as f32);
        assert!((ratio - 2.0).abs() < 0.25, "aspect drifted: {ratio}");
        // All source pixels were 128 → every output pixel is 128 too.
        assert!(p.rgba.iter().all(|&b| b == 128));
    }

    #[test]
    fn build_preview_is_deterministic_for_same_input() {
        // The half-block path samples the preview each frame; a
        // non-deterministic downsample would flicker. Pin determinism.
        let mut src = vec![0u8; 32 * 24 * 4];
        for (i, b) in src.iter_mut().enumerate() {
            *b = ((i * 7) % 256) as u8;
        }
        let a = build_preview(&src, 32, 24).unwrap();
        let b = build_preview(&src, 32, 24).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn build_preview_rejects_zero_dimensions() {
        let src = vec![0u8; 4];
        assert!(build_preview(&src, 0, 1).is_none());
        assert!(build_preview(&src, 1, 0).is_none());
    }

    #[test]
    fn build_preview_rejects_undersized_buffer() {
        // Caller mismatch — buffer length doesn't cover the claimed
        // dimensions. Reject cleanly rather than indexing past the end.
        let src = vec![0u8; 4 * 4 * 4 - 1];
        assert!(build_preview(&src, 4, 4).is_none());
    }

    #[test]
    fn halfblock_cells_emits_one_per_cell_with_correct_offsets() {
        // 4×2 preview = enough pixels for two cells in a column, or one
        // 2-wide × 1-tall cell row. We ask for 2 rows × 2 cols → 4 cells.
        let mut rgba = Vec::with_capacity(4 * 2 * 4);
        for i in 0..8 {
            rgba.extend_from_slice(&[i as u8 * 20, 0, 0, 255]);
        }
        let p = Preview { width: 2, height: 4, rgba };
        let cells = halfblock_cells(&p, 2, 2);
        assert_eq!(cells.len(), 4);
        // Offsets cover (0,0), (0,1), (1,0), (1,1) in row-major order.
        let offsets: Vec<(u16, u16)> =
            cells.iter().map(|c| (c.row_offset, c.col_offset)).collect();
        assert_eq!(offsets, vec![(0, 0), (0, 1), (1, 0), (1, 1)]);
    }

    #[test]
    fn halfblock_cells_top_pixel_drives_fg_bottom_drives_bg() {
        // 1×2 preview: pixel 0 (top) = pure red, pixel 1 (bottom) = pure
        // blue. One cell. fg must come from row 0, bg from row 1.
        let rgba = vec![
            255, 0, 0, 255,  // (0,0) red
            0, 0, 255, 255,  // (0,1) blue
        ];
        let p = Preview { width: 1, height: 2, rgba };
        let cells = halfblock_cells(&p, 1, 1);
        assert_eq!(cells.len(), 1);
        let c = &cells[0];
        // Top pixel is fg. srgb_to_linear(255) = 1.0 exactly.
        assert!((c.fg[0] - 1.0).abs() < 1e-4, "fg r: {}", c.fg[0]);
        assert!(c.fg[2] < 0.01, "fg b should be near zero, got {}", c.fg[2]);
        // Bottom pixel is bg.
        assert!(c.bg[0] < 0.01, "bg r should be near zero, got {}", c.bg[0]);
        assert!((c.bg[2] - 1.0).abs() < 1e-4, "bg b: {}", c.bg[2]);
        // Alpha passes through as straight-alpha float.
        assert!((c.fg[3] - 1.0).abs() < 1e-4);
        assert!((c.bg[3] - 1.0).abs() < 1e-4);
    }

    #[test]
    fn halfblock_cells_handles_empty_request() {
        let p = Preview { width: 4, height: 4, rgba: vec![255u8; 64] };
        assert!(halfblock_cells(&p, 0, 4).is_empty());
        assert!(halfblock_cells(&p, 4, 0).is_empty());
    }

    #[test]
    fn halfblock_cells_handles_oversized_request_without_panicking() {
        // Ask for more cells than the preview has pixels — nearest-
        // neighbour clamping must keep every sample inside bounds.
        let p = Preview { width: 2, height: 2, rgba: vec![255u8; 16] };
        let cells = halfblock_cells(&p, 10, 10);
        assert_eq!(cells.len(), 100, "every cell still emitted");
    }

    #[test]
    fn store_preview_is_some_after_decode() {
        let Some((d, q, p, _)) = try_make_pipeline_and_image() else {
            eprintln!("skipping: no GPU adapter");
            return;
        };
        let mut s = Store::new(DEFAULT_CAP_BYTES);
        let png = make_png(4, 4);
        let (_pending, image_id) =
            s.request_insert(png, 100, Duration::from_secs(5), None);
        // Pre-decode: preview unavailable (mirrors `peek`).
        assert!(s.preview(image_id).is_none());
        let results = poll_until_result(&mut s, &p, &d, &q, Duration::from_secs(2));
        assert_eq!(results.len(), 1);
        assert!(results[0].1.is_ok());
        // Post-decode: preview is populated and at least 1×1.
        let preview = s.preview(image_id).expect("preview should land");
        assert!(preview.width >= 1 && preview.height >= 1);
    }

    #[test]
    fn store_preview_is_none_for_unknown_id() {
        let s = Store::new(DEFAULT_CAP_BYTES);
        assert!(s.preview(ImageId(1)).is_none());
        assert!(s.preview(ImageId(99_999)).is_none());
    }

    #[test]
    fn store_preview_dropped_when_retain_evicts() {
        // Preview lifetime must track the GPU image: a placement that
        // has been fully cleaned up loses its preview at the next
        // retain, otherwise a stale preview could be rendered against a
        // freshly-allocated id that happens to recycle the same slot.
        let Some((d, q, p, _)) = try_make_pipeline_and_image() else {
            eprintln!("skipping: no GPU adapter");
            return;
        };
        let mut s = Store::new(DEFAULT_CAP_BYTES);
        let (_pid, image_id) =
            s.request_insert(make_png(2, 2), 100, Duration::from_secs(5), None);
        let results = poll_until_result(&mut s, &p, &d, &q, Duration::from_secs(2));
        assert_eq!(results.len(), 1);
        assert!(s.preview(image_id).is_some());
        s.retain(&HashSet::new());
        assert!(s.preview(image_id).is_none());
    }

    #[test]
    fn halfblock_cells_end_to_end_against_store_preview() {
        // Real decode → preview → halfblock_cells round-trip. Pins that
        // the consumer path (sampling preview pixels for half-block fg/bg)
        // works against an image that flowed through the worker, not just
        // a hand-built Preview.
        let Some((d, q, p, _)) = try_make_pipeline_and_image() else {
            eprintln!("skipping: no GPU adapter");
            return;
        };
        let mut s = Store::new(DEFAULT_CAP_BYTES);
        let png = make_png(8, 8);
        let (_pid, image_id) =
            s.request_insert(png, 100, Duration::from_secs(5), None);
        let results = poll_until_result(&mut s, &p, &d, &q, Duration::from_secs(2));
        assert_eq!(results.len(), 1);
        assert!(results[0].1.is_ok());
        let preview = s.preview(image_id).expect("preview built");
        let cells = halfblock_cells(preview, 3, 4);
        assert_eq!(cells.len(), 12);
        // make_png paints the whole image red — fg AND bg should both
        // be near-red (linear ≈ 1.0 on R, ≈ 0 on G/B).
        for c in cells {
            assert!(c.fg[0] > 0.9, "fg r: {}", c.fg[0]);
            assert!(c.bg[0] > 0.9, "bg r: {}", c.bg[0]);
            assert!(c.fg[1] < 0.1 && c.fg[2] < 0.1);
            assert!(c.bg[1] < 0.1 && c.bg[2] < 0.1);
        }
    }

    #[test]
    fn request_insert_with_empty_bytes_surfaces_decode_error() {
        // Empty payload is the trivial bad input — must come back as a clean
        // Decode error rather than hanging the worker or panicking on an
        // empty slice.
        let Some((d, q, p, _)) = try_make_pipeline_and_image() else {
            eprintln!("skipping: no GPU adapter");
            return;
        };
        let mut s = Store::new(DEFAULT_CAP_BYTES);
        let (pending, _id) = s.request_insert(Vec::new(), 100, Duration::from_secs(5), None);
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut results = Vec::new();
        while results.is_empty() && Instant::now() < deadline {
            results = s.poll(&p, &d, &q, false);
            if results.is_empty() {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, pending);
        assert!(matches!(results[0].1, Err(DecodeError::Decode(_))));
        assert!(s.is_empty());
    }

    //
    // P2.5: end-to-end iTerm2 flow — Terminal::feed parses an OSC, drains
    // it into a Store request, polls until decode completes, and verifies
    // the placement transitions from "reserved blank" to "pixels visible."
    // Mirrors what `State::drain_pending_image_uploads` / `poll_pending_images`
    // do in production. Needs a real GPU adapter; skips on CI without one.
    //

    /// Build a minimal iTerm2 OSC payload wrapping `png_bytes` (already
    /// raw PNG). `args` is the param string between `File=` and `:`.
    fn make_iterm_osc(args: &str, png_bytes: &[u8]) -> String {
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(png_bytes);
        format!("\x1b]1337;File={}:{}\x07", args, b64)
    }

    /// Run `Store::poll` repeatedly until the channel yields at least one
    /// result or `timeout` elapses. Returns whatever the poll produced —
    /// caller asserts on the contents.
    fn poll_until_result(
        s: &mut Store,
        pipeline: &ImagePipeline,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        timeout: Duration,
    ) -> Vec<(PendingId, Result<ImageId, DecodeError>)> {
        let deadline = Instant::now() + timeout;
        loop {
            let results = s.poll(pipeline, device, queue, false);
            if !results.is_empty() {
                return results;
            }
            if Instant::now() >= deadline {
                return results;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn e2e_iterm_osc_lands_visible_placement_after_decode() {
        let Some((d, q, p, _)) = try_make_pipeline_and_image() else {
            eprintln!("skipping: no GPU adapter");
            return;
        };
        let mut store = Store::new(DEFAULT_CAP_BYTES);
        let mut term = crate::terminal::Terminal::new(80, 24, 100);
        term.set_cell_size_px(8, 16);

        let png = make_png(4, 4);
        term.feed(&make_iterm_osc("inline=1;width=2;height=1", &png));

        // The OSC handler advances the cursor and queues the upload; main.rs
        // would do the request_insert + insert_placement step. Mirror that
        // here so the test exercises the same boundary main.rs sits on.
        let uploads = term.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        let up = uploads.into_iter().next().unwrap();

        let (pending, image_id) = store.request_insert(
            up.bytes,
            100,
            Duration::from_secs(5),
            None,
        );
        let (rows, cols) = up.cell_extent;
        let (row, col) = up.cell_anchor;
        term.insert_placement(image_id, row, col, rows, cols, 0);

        // Pre-decode: placement is in the grid but `peek` is None — the
        // renderer would skip drawing for this frame.
        assert_eq!(term.live_placements().len(), 1);
        assert_eq!(term.live_placements()[0].image, image_id);
        assert!(store.is_pending(image_id));
        assert!(store.peek(image_id).is_none());

        // Poll until the worker finishes.
        let results = poll_until_result(&mut store, &p, &d, &q, Duration::from_secs(2));
        assert_eq!(results.len(), 1, "decode didn't complete in time");
        assert_eq!(results[0].0, pending);
        let returned_id = results[0].1.as_ref().expect("decode succeeded");
        assert_eq!(*returned_id, image_id, "reservation id must match");

        // Post-decode: pixels visible, placement still anchored.
        assert!(store.peek(image_id).is_some());
        assert!(!store.is_pending(image_id));
        assert_eq!(term.live_placements().len(), 1);
    }

    #[test]
    fn e2e_iterm_osc_with_bad_bytes_removes_orphan_placement() {
        let Some((d, q, p, _)) = try_make_pipeline_and_image() else {
            eprintln!("skipping: no GPU adapter");
            return;
        };
        let mut store = Store::new(DEFAULT_CAP_BYTES);
        let mut term = crate::terminal::Terminal::new(80, 24, 100);
        term.set_cell_size_px(8, 16);

        // Valid base64, but the decoded bytes aren't a real image. The
        // OSC parser accepts it (base64 succeeds, header peek fails →
        // pixel_size: None, cell_extent falls back to 1×1). The worker
        // then fails the full decode with DecodeError::Decode.
        let osc = make_iterm_osc("inline=1", b"these bytes are not an image");
        term.feed(&osc);
        let uploads = term.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        let up = uploads.into_iter().next().unwrap();

        let (_pending, image_id) = store.request_insert(
            up.bytes,
            100,
            Duration::from_secs(5),
            None,
        );
        let (rows, cols) = up.cell_extent;
        let (row, col) = up.cell_anchor;
        term.insert_placement(image_id, row, col, rows, cols, 0);
        assert_eq!(term.live_placements().len(), 1);

        let results = poll_until_result(&mut store, &p, &d, &q, Duration::from_secs(2));
        assert_eq!(results.len(), 1);
        assert!(matches!(results[0].1, Err(DecodeError::Decode(_))));

        // main.rs runs this cleanup on failure — verify the contract.
        term.remove_placements_with_image(image_id);
        assert!(term.live_placements().is_empty());
        assert!(store.peek(image_id).is_none());
        assert!(!store.is_pending(image_id));
    }

    #[test]
    fn e2e_iterm_multiple_oscs_in_one_feed_all_decode() {
        let Some((d, q, p, _)) = try_make_pipeline_and_image() else {
            eprintln!("skipping: no GPU adapter");
            return;
        };
        let mut store = Store::new(DEFAULT_CAP_BYTES);
        let mut term = crate::terminal::Terminal::new(80, 24, 100);
        term.set_cell_size_px(8, 16);

        // Two distinct images back-to-back in one feed call.
        let png_a = make_png(2, 2);
        let png_b = make_png(3, 3);
        let combo = format!(
            "{}{}",
            make_iterm_osc("inline=1;width=1;height=1", &png_a),
            make_iterm_osc("inline=1;width=2;height=2", &png_b),
        );
        term.feed(&combo);

        let uploads = term.take_pending_image_uploads();
        assert_eq!(uploads.len(), 2);

        let mut pendings = Vec::new();
        let mut image_ids = Vec::new();
        for up in uploads {
            let (pid, iid) = store.request_insert(up.bytes, 100, Duration::from_secs(5), None);
            let (r, c) = up.cell_anchor;
            let (rows, cols) = up.cell_extent;
            term.insert_placement(iid, r, c, rows, cols, 0);
            pendings.push(pid);
            image_ids.push(iid);
        }
        assert_eq!(term.live_placements().len(), 2);

        // Poll until both results arrive. May come in either order.
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut completed = std::collections::HashSet::new();
        while completed.len() < 2 && Instant::now() < deadline {
            for (pid, outcome) in store.poll(&p, &d, &q, false) {
                assert!(outcome.is_ok(), "both decodes should succeed");
                completed.insert(pid);
            }
            if completed.len() < 2 {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        assert_eq!(completed.len(), 2, "both decodes completed");

        for iid in &image_ids {
            assert!(store.peek(*iid).is_some());
        }
    }

    #[test]
    fn e2e_iterm_osc_do_not_move_cursor_keeps_position() {
        // End-to-end variant of the unit test in terminal.rs — verifies
        // the cursor-stays-put behaviour survives the round-trip and that
        // a second OSC at the same cell stacks rather than offsetting.
        let Some((d, q, p, _)) = try_make_pipeline_and_image() else {
            eprintln!("skipping: no GPU adapter");
            return;
        };
        let mut store = Store::new(DEFAULT_CAP_BYTES);
        let mut term = crate::terminal::Terminal::new(80, 24, 100);
        term.set_cell_size_px(8, 16);
        term.feed("\x1b[5;1H"); // cursor at row 5 col 1 (1-based)

        let png = make_png(2, 2);
        let osc = make_iterm_osc("inline=1;width=2;height=2;doNotMoveCursor=1", &png);
        term.feed(&osc);
        assert_eq!(term.cursor().row, 4); // unchanged from CUP

        let uploads = term.take_pending_image_uploads();
        let up = uploads.into_iter().next().unwrap();
        assert!(up.do_not_move_cursor);
        assert_eq!(up.cell_anchor, (4, 0));

        let (_pid, iid) = store.request_insert(up.bytes, 100, Duration::from_secs(5), None);
        let (r, c) = up.cell_anchor;
        let (rows, cols) = up.cell_extent;
        term.insert_placement(iid, r, c, rows, cols, 0);

        // Drive decode to completion so the assertion isn't a no-op.
        let results = poll_until_result(&mut store, &p, &d, &q, Duration::from_secs(2));
        assert_eq!(results.len(), 1);
        assert!(results[0].1.is_ok());

        // Cursor still at row 4 — doNotMoveCursor doesn't get clobbered by
        // anything in the decode path.
        assert_eq!(term.cursor().row, 4);
    }

    //
    // is_pending contract: distinguishes "unknown id" from "reserved but
    // decode in flight". Each branch matters because the renderer treats
    // them differently (unknown → log a bug; in-flight → render blank
    // for one more frame).
    //

    #[test]
    fn is_pending_returns_false_for_unknown_id() {
        let s = Store::new(DEFAULT_CAP_BYTES);
        assert!(!s.is_pending(ImageId(1)));
        assert!(!s.is_pending(ImageId(99_999)));
    }

    #[test]
    fn is_pending_returns_false_after_decode_completes() {
        let Some((d, q, p, _)) = try_make_pipeline_and_image() else {
            eprintln!("skipping: no GPU adapter");
            return;
        };
        let mut s = Store::new(DEFAULT_CAP_BYTES);
        let (_pending, image_id) =
            s.request_insert(make_png(2, 2), 100, Duration::from_secs(5), None);
        assert!(s.is_pending(image_id));
        let results = poll_until_result(&mut s, &p, &d, &q, Duration::from_secs(2));
        assert_eq!(results.len(), 1);
        assert!(results[0].1.is_ok());
        // Slot is now filled — is_pending must drop to false so the
        // renderer stops treating the placement as "still loading".
        assert!(!s.is_pending(image_id));
    }

    #[test]
    fn is_pending_returns_false_after_retain_drops_reservation() {
        let mut s = Store::new(DEFAULT_CAP_BYTES);
        let (_pending, image_id) =
            s.request_insert(make_png(2, 2), 100, Duration::from_secs(5), None);
        assert!(s.is_pending(image_id));
        // retain with an empty keep-set drops the reservation — id becomes
        // unknown, which is_pending must report as false (not true).
        s.retain(&HashSet::new());
        assert!(!s.is_pending(image_id));
    }

    //
    // Budget-cap edge cases. The cap is a hard ceiling — verify both the
    // exact-fit path (must succeed) and the over-budget path doesn't leave
    // stale byte accounting behind.
    //

    #[test]
    fn poll_cap_exact_upload_succeeds() {
        let Some((d, q, p, _)) = try_make_pipeline_and_image() else {
            eprintln!("skipping: no GPU adapter");
            return;
        };
        // 2x2 RGBA = exactly 16 bytes; cap of 16 → the equality branch
        // must succeed (the check is `>` not `>=`).
        let mut s = Store::new(16);
        let (_pending, _reserved) =
            s.request_insert(make_png(2, 2), 100, Duration::from_secs(5), None);
        let results = poll_until_result(&mut s, &p, &d, &q, Duration::from_secs(2));
        assert_eq!(results.len(), 1);
        assert!(results[0].1.is_ok(), "exact-fit upload must succeed");
        assert_eq!(s.bytes(), 16);
    }

    #[test]
    fn poll_budget_eviction_leaves_total_bytes_zero() {
        let Some((d, q, p, _)) = try_make_pipeline_and_image() else {
            eprintln!("skipping: no GPU adapter");
            return;
        };
        // Cap=32, 4x4 image = 64 bytes → over budget. Pin that the
        // eviction path doesn't leak a bytes() ghost — store has no
        // surviving images and total_bytes must read as 0, not e.g. 64.
        let mut s = Store::new(32);
        let (_pending, _reserved) =
            s.request_insert(make_png(4, 4), 100, Duration::from_secs(5), None);
        let results = poll_until_result(&mut s, &p, &d, &q, Duration::from_secs(2));
        assert_eq!(results.len(), 1);
        assert!(matches!(results[0].1, Err(DecodeError::BudgetExceeded { .. })));
        assert_eq!(s.bytes(), 0, "evicted reservation must not inflate byte count");
        assert!(s.is_empty());
    }

    //
    // Wire-format edge cases via the full Terminal → Store flow.
    //

    #[test]
    fn e2e_iterm_osc_with_wrapped_base64_decodes_cleanly() {
        // Real iTerm callers (and `imgcat`) sometimes wrap base64 at 76
        // chars with embedded newlines. handle_osc_1337 strips whitespace
        // before decode; pin that contract end-to-end.
        let Some((d, q, p, _)) = try_make_pipeline_and_image() else {
            eprintln!("skipping: no GPU adapter");
            return;
        };
        let mut store = Store::new(DEFAULT_CAP_BYTES);
        let mut term = crate::terminal::Terminal::new(80, 24, 100);
        term.set_cell_size_px(8, 16);

        use base64::Engine;
        let png = make_png(2, 2);
        let raw = base64::engine::general_purpose::STANDARD.encode(&png);
        // Inject newlines + spaces at 8-char intervals — mimics imgcat's
        // line-wrapped output without depending on the exact base64 length.
        let mut wrapped = String::new();
        for (i, ch) in raw.chars().enumerate() {
            if i > 0 && i % 8 == 0 {
                wrapped.push('\n');
                wrapped.push(' ');
            }
            wrapped.push(ch);
        }
        let osc = format!("\x1b]1337;File=inline=1:{}\x07", wrapped);
        term.feed(&osc);

        let uploads = term.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1, "whitespace in base64 must not break parsing");
        let up = uploads.into_iter().next().unwrap();
        let (_pid, iid) = store.request_insert(up.bytes, 100, Duration::from_secs(5), None);
        let results = poll_until_result(&mut store, &p, &d, &q, Duration::from_secs(2));
        assert_eq!(results.len(), 1);
        assert!(results[0].1.is_ok(), "wrapped-base64 payload should decode");
        assert!(store.peek(iid).is_some());
    }

    #[test]
    fn e2e_iterm_osc_with_empty_base64_payload_drops_cleanly() {
        // `File=inline=1:` (nothing after the colon) — base64 decode of an
        // empty string succeeds and yields zero bytes; the worker then
        // fails on decode. Must not panic and must not leak a reservation.
        let Some((d, q, p, _)) = try_make_pipeline_and_image() else {
            eprintln!("skipping: no GPU adapter");
            return;
        };
        let mut store = Store::new(DEFAULT_CAP_BYTES);
        let mut term = crate::terminal::Terminal::new(80, 24, 100);
        term.set_cell_size_px(8, 16);
        term.feed("\x1b]1337;File=inline=1:\x07");

        let uploads = term.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1, "empty body still produces a pending upload");
        let up = uploads.into_iter().next().unwrap();
        assert!(up.bytes.is_empty());
        let (_pid, iid) = store.request_insert(up.bytes, 100, Duration::from_secs(5), None);
        let results = poll_until_result(&mut store, &p, &d, &q, Duration::from_secs(2));
        assert_eq!(results.len(), 1);
        assert!(matches!(results[0].1, Err(DecodeError::Decode(_))));
        // Reservation cleaned up on decode failure.
        assert!(store.is_empty());
        assert!(store.peek(iid).is_none());
    }

    #[test]
    fn e2e_iterm_osc_name_propagates_label_to_pending_upload() {
        // The label rides the DecodeJob into the worker and comes back on
        // the DecodeResult; main.rs threads it into the wgpu texture's
        // debug-label slot. Pin the contract: a base64-encoded ASCII
        // filename in `name=` must reach PendingImageUpload.label decoded.
        use base64::Engine;
        let mut term = crate::terminal::Terminal::new(80, 24, 100);
        let filename = "kitten.png";
        let name_b64 = base64::engine::general_purpose::STANDARD.encode(filename);
        let png = make_png(2, 2);
        let png_b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        let osc = format!("\x1b]1337;File=inline=1;name={}:{}\x07", name_b64, png_b64);
        term.feed(&osc);
        let uploads = term.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        assert_eq!(uploads[0].label.as_deref(), Some(filename));
    }

    //
    // K1.6 — Kitty graphics end-to-end. Mirrors the iTerm e2e suite:
    // feed Terminal an APC, hand the queued upload to Store, poll
    // until decode completes, verify the placement is visible.
    //

    fn make_kitty_apc(args: &str, png_bytes: &[u8]) -> String {
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(png_bytes);
        format!("\x1b_G{};{}\x1b\\", args, b64)
    }

    #[test]
    fn e2e_kitty_apc_lands_visible_placement_after_decode() {
        let Some((d, q, p, _)) = try_make_pipeline_and_image() else {
            eprintln!("skipping: no GPU adapter");
            return;
        };
        let mut store = Store::new(DEFAULT_CAP_BYTES);
        let mut term = crate::terminal::Terminal::new(80, 24, 100);
        term.set_cell_size_px(8, 16);

        let png = make_png(4, 4);
        term.feed(&make_kitty_apc("a=T,f=100,c=2,r=1", &png));

        let uploads = term.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        let up = uploads.into_iter().next().unwrap();

        let (pending, image_id) = store.request_insert(
            up.bytes,
            100,
            Duration::from_secs(5),
            up.label,
        );
        let (rows, cols) = up.cell_extent;
        let (row, col) = up.cell_anchor;
        term.insert_placement(image_id, row, col, rows, cols, 0);

        // Pre-decode: placement created, no pixels yet.
        assert_eq!(term.live_placements().len(), 1);
        assert!(store.is_pending(image_id));

        let results = poll_until_result(&mut store, &p, &d, &q, Duration::from_secs(2));
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, pending);
        let returned = results[0].1.as_ref().expect("decode succeeded");
        assert_eq!(*returned, image_id);
        assert!(store.peek(image_id).is_some());
    }

    #[test]
    fn e2e_kitty_apc_chunked_assembles_into_one_decode() {
        // Three-chunk transmission with one image_id. The terminal
        // accumulator concatenates them; only one upload reaches Store.
        let Some((d, q, p, _)) = try_make_pipeline_and_image() else {
            eprintln!("skipping: no GPU adapter");
            return;
        };
        let mut store = Store::new(DEFAULT_CAP_BYTES);
        let mut term = crate::terminal::Terminal::new(80, 24, 100);
        term.set_cell_size_px(8, 16);

        let png = make_png(4, 4);
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        let third = (b64.len() / 3 + 1).max(4);
        let (c1, rest) = b64.split_at(third.min(b64.len()));
        let split2 = third.min(rest.len());
        let (c2, c3) = rest.split_at(split2);

        term.feed(&format!("\x1b_Ga=T,f=100,c=2,r=1,i=99,m=1;{}\x1b\\", c1));
        term.feed(&format!("\x1b_Gi=99,m=1;{}\x1b\\", c2));
        term.feed(&format!("\x1b_Gi=99,m=0;{}\x1b\\", c3));

        let uploads = term.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1, "chunks must coalesce into one upload");

        let up = uploads.into_iter().next().unwrap();
        let (pending, image_id) = store.request_insert(
            up.bytes,
            100,
            Duration::from_secs(5),
            up.label,
        );
        term.insert_placement(image_id, up.cell_anchor.0, up.cell_anchor.1, up.cell_extent.0, up.cell_extent.1, 0);

        let results = poll_until_result(&mut store, &p, &d, &q, Duration::from_secs(2));
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, pending);
        assert!(results[0].1.is_ok(), "reassembled chunks decoded cleanly");
        assert!(store.peek(image_id).is_some());
    }

    #[test]
    fn e2e_kitty_query_then_image_does_full_handshake() {
        // Simulate icat's startup: send query, expect OK, then send the
        // real image. Verify both branches land their respective side
        // effects (response bytes for the query, upload for the image).
        let Some((d, q, p, _)) = try_make_pipeline_and_image() else {
            eprintln!("skipping: no GPU adapter");
            return;
        };
        let mut store = Store::new(DEFAULT_CAP_BYTES);
        let mut term = crate::terminal::Terminal::new(80, 24, 100);
        term.set_cell_size_px(8, 16);

        term.feed("\x1b_Ga=q,i=7,s=1,v=1\x1b\\");
        let reply = term.take_response();
        assert!(reply.starts_with(b"\x1b_Gi=7;OK"), "got: {:?}", reply);

        let png = make_png(3, 3);
        term.feed(&make_kitty_apc("a=T,f=100,i=8,c=2,r=2", &png));
        let uploads = term.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        let up = uploads.into_iter().next().unwrap();
        let (_pid, image_id) = store.request_insert(up.bytes, 100, Duration::from_secs(5), None);
        term.insert_placement(image_id, up.cell_anchor.0, up.cell_anchor.1, up.cell_extent.0, up.cell_extent.1, 0);

        let _ = poll_until_result(&mut store, &p, &d, &q, Duration::from_secs(2));
        assert!(store.peek(image_id).is_some());
    }

    #[test]
    fn e2e_kitty_concurrent_chunked_ids_each_land_independent_textures() {
        // Two interleaved chunked transmissions with distinct ids must
        // each surface as its own upload, decode independently, and
        // produce distinct ImageIds inside Store.
        let Some((d, q, p, _)) = try_make_pipeline_and_image() else {
            eprintln!("skipping: no GPU adapter");
            return;
        };
        let mut store = Store::new(DEFAULT_CAP_BYTES);
        let mut term = crate::terminal::Terminal::new(80, 24, 100);
        term.set_cell_size_px(8, 16);

        let png_a = make_png(4, 4);
        let png_b = make_png(2, 2);
        use base64::Engine;
        let b64_a = base64::engine::general_purpose::STANDARD.encode(&png_a);
        let b64_b = base64::engine::general_purpose::STANDARD.encode(&png_b);
        let mid_a = b64_a.len() / 2;
        let mid_b = b64_b.len() / 2;

        // Interleave starts: A first half, B first half, A finish, B finish.
        term.feed(&format!("\x1b_Ga=T,f=100,c=2,r=1,i=1001,m=1;{}\x1b\\", &b64_a[..mid_a]));
        term.feed(&format!("\x1b_Ga=T,f=100,c=1,r=1,i=1002,m=1;{}\x1b\\", &b64_b[..mid_b]));
        assert!(term.take_pending_image_uploads().is_empty(), "no flush mid-stream");
        term.feed(&format!("\x1b_Gi=1001;{}\x1b\\", &b64_a[mid_a..]));
        term.feed(&format!("\x1b_Gi=1002;{}\x1b\\", &b64_b[mid_b..]));

        let uploads = term.take_pending_image_uploads();
        assert_eq!(uploads.len(), 2);

        // Hand both off in order. ImageIds must be distinct (Store mints
        // a fresh one per request_insert).
        let mut image_ids = Vec::new();
        for up in uploads {
            let (_pending, id) =
                store.request_insert(up.bytes, 100, Duration::from_secs(5), up.label);
            image_ids.push(id);
        }
        assert_ne!(image_ids[0], image_ids[1]);

        let results = poll_until_result(&mut store, &p, &d, &q, Duration::from_secs(2));
        // Both should arrive — though possibly across multiple polls; the
        // helper returns on the first non-empty. Keep polling until both land.
        let mut decoded = results.len();
        let deadline = Instant::now() + Duration::from_secs(2);
        while decoded < 2 && Instant::now() < deadline {
            decoded += store.poll(&p, &d, &q, false).len();
        }
        assert_eq!(decoded, 2, "both chunked images must decode");
        assert!(store.peek(image_ids[0]).is_some());
        assert!(store.peek(image_ids[1]).is_some());
    }

    #[test]
    fn e2e_mixed_iterm_and_kitty_uploads_both_decode_through_store() {
        // Both wire formats funnel into the same upload queue. Feed one
        // iTerm OSC and one Kitty APC in a single feed; both should
        // decode independently inside Store and produce distinct
        // GpuImages.
        let Some((d, q, p, _)) = try_make_pipeline_and_image() else {
            eprintln!("skipping: no GPU adapter");
            return;
        };
        let mut store = Store::new(DEFAULT_CAP_BYTES);
        let mut term = crate::terminal::Terminal::new(80, 24, 100);
        term.set_cell_size_px(8, 16);

        let png = make_png(2, 2);
        let combo = format!(
            "{}{}",
            make_iterm_osc("inline=1", &png),
            make_kitty_apc("a=T,f=100,c=1,r=1", &png),
        );
        term.feed(&combo);

        let uploads = term.take_pending_image_uploads();
        assert_eq!(uploads.len(), 2);
        // Arrival order: iTerm OSC came first.
        assert_ne!(uploads[1].label.as_deref(), uploads[0].label.as_deref());

        let mut image_ids = Vec::new();
        for up in uploads {
            let (_pending, id) =
                store.request_insert(up.bytes, 100, Duration::from_secs(5), up.label);
            image_ids.push(id);
        }
        let mut decoded = 0;
        let deadline = Instant::now() + Duration::from_secs(2);
        while decoded < 2 && Instant::now() < deadline {
            decoded += store.poll(&p, &d, &q, false).len();
        }
        assert_eq!(decoded, 2, "both wire formats must decode through Store");
        assert!(store.peek(image_ids[0]).is_some());
        assert!(store.peek(image_ids[1]).is_some());
    }

    #[test]
    fn e2e_kitty_virtual_placement_through_store_with_placeholder_bbox() {
        // Full virtual-placement flow end-to-end:
        //   1. `a=T,U=1,i=N` transmits PNG bytes and registers a client-id
        //      → store-id mapping. NO Placement is created, cursor stays
        //      put. Image survives mark-and-sweep via referenced_image_ids
        //      because the kitty_image_ids map's values feed it.
        //   2. Printed U+10EEEE placeholder cells with the matching SGR fg
        //      produce a bbox via `kitty_placeholder_bboxes()`.
        //   3. The store image registered in step 1 is reachable via
        //      `Store::peek` for that bbox's id (after main.rs maps client
        //      → store via `kitty_image_id_lookup`).
        let Some((d, q, p, _)) = try_make_pipeline_and_image() else {
            eprintln!("skipping: no GPU adapter");
            return;
        };
        let mut store = Store::new(DEFAULT_CAP_BYTES);
        let mut term = crate::terminal::Terminal::new(80, 24, 100);
        term.set_cell_size_px(8, 16);
        let cursor_before = term.cursor();

        // 1. a=T,U=1 — transmit but don't display.
        let png = make_png(4, 4);
        let client_id = 0xABCDEFu32;
        term.feed(&make_kitty_apc(
            &format!("a=T,U=1,f=100,i={}", client_id),
            &png,
        ));
        let uploads = term.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        let up = uploads.into_iter().next().unwrap();
        assert!(!up.display_immediately, "U=1 must not display");
        assert_eq!(up.kitty_image_id, Some(client_id));
        // No placement created; cursor untouched.
        assert!(term.live_placements().is_empty());
        assert_eq!(term.cursor().row, cursor_before.row);
        assert_eq!(term.cursor().col, cursor_before.col);

        // Hand the bytes off to Store and register the mapping (main.rs's job).
        let (_pending, image_id) = store.request_insert(
            up.bytes,
            100,
            Duration::from_secs(5),
            up.label,
        );
        term.register_kitty_image_id(client_id, image_id);
        // Mark-and-sweep simulation: even with no placement, the image
        // must be in referenced_image_ids so Store::retain keeps it.
        assert!(term.referenced_image_ids().contains(&image_id));

        // 2. Print placeholder cells encoding `client_id` in SGR fg.
        let r = ((client_id >> 16) & 0xFF) as u8;
        let g = ((client_id >> 8) & 0xFF) as u8;
        let b = (client_id & 0xFF) as u8;
        term.feed(&format!("\x1b[38;2;{};{};{}m", r, g, b));
        term.feed("\x1b[3;5H"); // row 2 col 4 (0-based)
        term.feed("\u{10EEEE}\u{10EEEE}");
        term.feed("\x1b[4;5H");
        term.feed("\u{10EEEE}\u{10EEEE}");
        let bboxes = term.kitty_placeholder_bboxes();
        assert_eq!(bboxes.len(), 1, "one bbox per distinct id");
        let (bbox_id, top, left, rows, cols) = bboxes[0];
        assert_eq!(bbox_id, client_id);
        assert_eq!((top, left, rows, cols), (2, 4, 2, 2));

        // 3. Drive the decode; the image referenced by the bbox's id maps
        // through to a peek-able Store entry.
        let _ = poll_until_result(&mut store, &p, &d, &q, Duration::from_secs(2));
        let mapped = term.kitty_image_id_lookup(bbox_id).expect("client → store mapping");
        assert_eq!(mapped, image_id);
        assert!(store.peek(mapped).is_some(), "virtual-placement image must be peek-able");
    }

    #[test]
    fn e2e_kitty_a_d_then_retransmit_same_id_decodes_cleanly() {
        // Lifecycle stress: transmit id=N, delete with a=d,d=i,i=N, then
        // re-transmit the SAME client id. The fresh transmission must
        // produce its own decode (not be poisoned by the prior mapping or
        // accumulator state).
        let Some((d, q, p, _)) = try_make_pipeline_and_image() else {
            eprintln!("skipping: no GPU adapter");
            return;
        };
        let mut store = Store::new(DEFAULT_CAP_BYTES);
        let mut term = crate::terminal::Terminal::new(80, 24, 100);
        term.set_cell_size_px(8, 16);

        // First transmission (a=t — transmit only, no display so we can
        // exercise the bare mapping path).
        let png1 = make_png(4, 4);
        let client_id = 55u32;
        term.feed(&make_kitty_apc(
            &format!("a=t,f=100,i={}", client_id),
            &png1,
        ));
        let uploads = term.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        let up1 = uploads.into_iter().next().unwrap();
        let (_, image_id_1) = store.request_insert(
            up1.bytes,
            100,
            Duration::from_secs(5),
            up1.label,
        );
        term.register_kitty_image_id(client_id, image_id_1);
        let _ = poll_until_result(&mut store, &p, &d, &q, Duration::from_secs(2));
        assert!(store.peek(image_id_1).is_some(), "first image lands");

        // a=d,d=i,i=55 — drop the placement (none here, but the mapping
        // gets cleared either way).
        term.feed("\x1b_Ga=d,d=i,i=55\x1b\\");
        assert!(
            term.kitty_image_id_lookup(client_id).is_none(),
            "delete must clear the client→store mapping",
        );

        // Re-transmit with the SAME client id and different pixels.
        let png2 = make_png(2, 2);
        term.feed(&make_kitty_apc(
            &format!("a=t,f=100,i={}", client_id),
            &png2,
        ));
        let uploads = term.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1, "fresh transmission queues one upload");
        let up2 = uploads.into_iter().next().unwrap();
        // Pixel size echoes the new 2×2 PNG, not the stale 4×4.
        assert_eq!(up2.pixel_size, Some((2, 2)));
        let (_, image_id_2) = store.request_insert(
            up2.bytes,
            100,
            Duration::from_secs(5),
            up2.label,
        );
        // Store mints a fresh ImageId — pin distinctness so a future
        // store-id reuse doesn't silently alias.
        assert_ne!(image_id_2, image_id_1);
        term.register_kitty_image_id(client_id, image_id_2);
        let _ = poll_until_result(&mut store, &p, &d, &q, Duration::from_secs(2));
        assert!(store.peek(image_id_2).is_some(), "re-transmission decodes cleanly");
    }

    //
    // Animation (a=f / a=a) coverage. These exercise the pure helpers
    // (composite_rgba, resolve_current_frame) plus the Store-level
    // request_insert_frame + apply_animation_control plumbing.
    //

    #[test]
    fn composite_rgba_opaque_src_overwrites_dst() {
        // 2×2 red dst; 1×1 green src at (1, 0). After: top-right pixel
        // green; everything else still red.
        let dst: Vec<u8> = (0..4)
            .flat_map(|_| [255u8, 0, 0, 255])
            .collect();
        let src = vec![0u8, 255, 0, 255];
        let out = composite_rgba(&dst, 2, 2, &src, 1, 1, 1, 0);
        assert_eq!(&out[0..4], &[255, 0, 0, 255]);
        assert_eq!(&out[4..8], &[0, 255, 0, 255]); // top-right
        assert_eq!(&out[8..12], &[255, 0, 0, 255]);
        assert_eq!(&out[12..16], &[255, 0, 0, 255]);
    }

    #[test]
    fn composite_rgba_transparent_src_leaves_dst_unchanged() {
        let dst = vec![10u8, 20, 30, 255, 40, 50, 60, 255];
        let src = vec![200u8, 200, 200, 0]; // fully-transparent
        let out = composite_rgba(&dst, 2, 1, &src, 1, 1, 0, 0);
        assert_eq!(out, dst);
    }

    #[test]
    fn composite_rgba_half_alpha_blends_to_midpoint() {
        // dst black, src white at 50% alpha → blended to ~128.
        let dst = vec![0u8, 0, 0, 255];
        let src = vec![255u8, 255, 255, 128];
        let out = composite_rgba(&dst, 1, 1, &src, 1, 1, 0, 0);
        // 128*255 + 0*127 = 32640; /255 = 128. Round-half-up keeps it
        // at 128 rather than drifting to 127.
        for &c in &out[..3] {
            assert!((127..=129).contains(&c), "channel {c} not near midpoint");
        }
    }

    #[test]
    fn composite_rgba_src_overhangs_dst_clips_silently() {
        // 2×2 dst, 4×4 src at (1, 1): only the top-left pixel of src
        // lands on the bottom-right of dst. No panic, no overflow.
        // Use opaque src so the result is a straight overwrite.
        let dst = vec![0u8; 16];
        let mut src = vec![123u8; 64];
        // Force alpha = 255 in every pixel.
        for px in src.chunks_exact_mut(4) {
            px[3] = 255;
        }
        let out = composite_rgba(&dst, 2, 2, &src, 4, 4, 1, 1);
        assert_eq!(out.len(), 16);
        assert_eq!(&out[0..12], &[0u8; 12]);
        assert_eq!(&out[12..16], &[123, 123, 123, 255]);
    }

    #[test]
    fn composite_rgba_malformed_src_returns_dst_copy() {
        // src claims 4×4 but only carries 8 bytes (2 pixels). Should
        // return dst unchanged rather than panic.
        let dst = vec![5u8; 16];
        let src = vec![99u8; 8];
        let out = composite_rgba(&dst, 2, 2, &src, 4, 4, 0, 0);
        assert_eq!(out, dst);
    }

    fn make_frame_test_image(
        store: &mut Store,
        dims: (u32, u32),
    ) -> Option<(wgpu::Device, wgpu::Queue, ImagePipeline, ImageId)> {
        let (d, q, p, _img) = try_make_pipeline_and_image()?;
        let (w, h) = dims;
        let rgba = vec![0x55u8; (w as usize) * (h as usize) * 4];
        let gpu = p.upload_rgba(&d, &q, &rgba, w, h, false, Some("base"));
        let id = store.insert_synthetic_for_test(gpu, (w as usize) * (h as usize) * 4);
        // The synthetic-insert helper doesn't populate base_rgba, so
        // do it manually — request_insert_frame requires it to know
        // what to composite against.
        let entry = store.images.get_mut(&id.0).expect("just inserted");
        entry.base_rgba = Some(rgba);
        Some((d, q, p, id))
    }

    #[test]
    fn request_insert_frame_appends_decoded_frame_to_parent() {
        let Some((d, q, p, parent)) = make_frame_test_image(
            &mut Store::new(DEFAULT_CAP_BYTES),
            (4, 4),
        ) else {
            return;
        };
        let mut store = Store::new(DEFAULT_CAP_BYTES);
        // We need parent in *this* store, so redo with the store we
        // own. (try_make_pipeline_and_image hands back a fresh device
        // per call — that's fine; we just need any device for upload.)
        let _ = (d, q, p, parent); // ignore the throwaway store result
        let Some((d, q, p, parent)) = make_frame_test_image(&mut store, (4, 4)) else {
            return;
        };
        assert_eq!(store.frame_count(parent), 0);

        // 2×2 fully-opaque red square encoded as PNG, intended for
        // (1, 1) inside the 4×4 parent.
        let frame_png: Vec<u8> = {
            let buf = image::RgbaImage::from_pixel(2, 2, image::Rgba([200, 30, 30, 255]));
            let mut bytes = Vec::new();
            image::DynamicImage::ImageRgba8(buf)
                .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageOutputFormat::Png)
                .expect("encode");
            bytes
        };
        let pid = store
            .request_insert_frame(
                parent,
                frame_png,
                100,
                Duration::from_secs(5),
                Some("test frame".into()),
                None, // append
                None, // compose against base
                50,   // 50ms gap
                1, 1, // top-left at (1, 1)
            )
            .expect("parent exists");
        // Drive the decode through the poll loop until it lands.
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let r = store.poll(&p, &d, &q, false);
            if r.iter().any(|(id, _)| *id == pid) {
                break;
            }
            if Instant::now() > deadline {
                panic!("frame decode never completed");
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(store.frame_count(parent), 1);

        // Sanity-check the composited pixels: corners stay 0x55
        // (parent fill), the (1,1)..(3,3) box turned red.
        let entry = store.images.get(&parent.0).expect("parent still present");
        let f0 = &entry.frames[0];
        // (0,0) — outside the frame's dst rect.
        assert_eq!(&f0.rgba[0..4], &[0x55, 0x55, 0x55, 0x55]);
        // (1,1) — inside, should be solid red.
        let idx = ((1 * 4) + 1) * 4;
        assert_eq!(&f0.rgba[idx..idx + 4], &[200, 30, 30, 255]);
    }

    #[test]
    fn request_insert_frame_drops_silently_for_unknown_parent() {
        let mut store = Store::new(DEFAULT_CAP_BYTES);
        let r = store.request_insert_frame(
            ImageId(999),
            vec![0u8; 8],
            100,
            Duration::from_secs(5),
            None,
            None,
            None,
            0,
            0, 0,
        );
        assert!(r.is_none());
        assert_eq!(store.pending_count(), 0);
    }

    #[test]
    fn apply_animation_control_make_current_clamps_and_stops() {
        let mut store = Store::new(DEFAULT_CAP_BYTES);
        let Some((_d, _q, _p, parent)) = make_frame_test_image(&mut store, (2, 2)) else {
            return;
        };
        // Pretend we have 3 frames in addition to the base.
        for _ in 0..3 {
            let entry = store.images.get_mut(&parent.0).expect("parent");
            // Cheap fake: stick a clone of the base GpuImage via
            // upload-on-the-side. We only need *count* for the math.
            // Cheat: re-use insert_synthetic_for_test to mint a GpuImage.
            let (_d, _q, _p, image) = try_make_pipeline_and_image().expect("gpu");
            let fake = Frame {
                image,
                rgba: vec![0u8; 16],
                delay_ms: 100,
                bytes: 0,
            };
            entry.frames.push(fake);
        }
        // c=10 (out of range) clamps to last frame index (3 in our
        // 0-based scheme: base + 3 frames = 4 slots).
        store.apply_animation_control(
            parent,
            None,
            None,
            Some(10),
            None,
            None,
            Instant::now(),
        );
        let state = store.animation_state(parent).expect("present");
        assert_eq!(state.current_frame, 3);
        assert_eq!(state.play_mode, PlayMode::Stopped);
    }

    #[test]
    fn apply_animation_control_s3_v0_sets_loop_forever() {
        let mut store = Store::new(DEFAULT_CAP_BYTES);
        let Some((_d, _q, _p, parent)) = make_frame_test_image(&mut store, (2, 2)) else {
            return;
        };
        store.apply_animation_control(
            parent,
            Some(3),
            Some(0),
            None,
            None,
            None,
            Instant::now(),
        );
        let state = store.animation_state(parent).expect("present");
        assert_eq!(state.play_mode, PlayMode::LoopForever);
    }

    #[test]
    fn apply_animation_control_s3_vN_sets_finite_loops() {
        let mut store = Store::new(DEFAULT_CAP_BYTES);
        let Some((_d, _q, _p, parent)) = make_frame_test_image(&mut store, (2, 2)) else {
            return;
        };
        store.apply_animation_control(
            parent,
            Some(3),
            Some(5),
            None,
            None,
            None,
            Instant::now(),
        );
        let state = store.animation_state(parent).expect("present");
        assert_eq!(state.play_mode, PlayMode::LoopFinite { remaining: 5 });
    }

    #[test]
    fn resolve_current_frame_static_image_returns_zero() {
        let mut store = Store::new(DEFAULT_CAP_BYTES);
        let Some((_d, _q, _p, parent)) = make_frame_test_image(&mut store, (2, 2)) else {
            return;
        };
        // No frames added; resolve always returns 0 (the base).
        let now = Instant::now();
        let entry = store.images.get(&parent.0).expect("parent");
        assert_eq!(resolve_current_frame(entry, now), 0);
        assert_eq!(resolve_current_frame(entry, now + Duration::from_secs(60)), 0);
    }

    #[test]
    fn resolve_current_frame_advances_through_timeline_with_loop_forever() {
        let mut store = Store::new(DEFAULT_CAP_BYTES);
        let Some((_d, _q, _p, parent)) = make_frame_test_image(&mut store, (2, 2)) else {
            return;
        };
        // Push 2 frames with 100ms delays each.
        for _ in 0..2 {
            let (_d, _q, _p, image) = try_make_pipeline_and_image().expect("gpu");
            let entry = store.images.get_mut(&parent.0).expect("parent");
            entry.frames.push(Frame {
                image,
                rgba: vec![0u8; 16],
                delay_ms: 100,
                bytes: 0,
            });
        }
        // Base also gets a 100ms gap so the timeline math is uniform
        // across all three slots.
        store.images.get_mut(&parent.0).expect("parent").base_delay_ms = 100;
        let t0 = Instant::now();
        store.force_animation_state_for_test(
            parent,
            AnimationState {
                play_mode: PlayMode::LoopForever,
                current_frame: 0,
                current_frame_started: t0,
            },
        );
        let entry = store.images.get(&parent.0).expect("parent");
        // t = 50ms → still base (frame 0)
        assert_eq!(resolve_current_frame(entry, t0 + Duration::from_millis(50)), 0);
        // t = 150ms → frame 1 (index 1)
        assert_eq!(resolve_current_frame(entry, t0 + Duration::from_millis(150)), 1);
        // t = 250ms → frame 2 (index 2)
        assert_eq!(resolve_current_frame(entry, t0 + Duration::from_millis(250)), 2);
        // t = 350ms → wraps to base (frame 0)
        assert_eq!(resolve_current_frame(entry, t0 + Duration::from_millis(350)), 0);
        // t = 700ms → wrapped twice more, lands at frame 1.
        // Timeline: 0→100 base, 100→200 f1, 200→300 f2, 300→400 base,
        // 400→500 f1, 500→600 f2, 600→700 base, 700+ → f1.
        assert_eq!(resolve_current_frame(entry, t0 + Duration::from_millis(700)), 1);
    }

    #[test]
    fn resolve_current_frame_loop_finite_stops_on_last_after_exhausting() {
        let mut store = Store::new(DEFAULT_CAP_BYTES);
        let Some((_d, _q, _p, parent)) = make_frame_test_image(&mut store, (2, 2)) else {
            return;
        };
        for _ in 0..2 {
            let (_d, _q, _p, image) = try_make_pipeline_and_image().expect("gpu");
            let entry = store.images.get_mut(&parent.0).expect("parent");
            entry.frames.push(Frame {
                image,
                rgba: vec![0u8; 16],
                delay_ms: 100,
                bytes: 0,
            });
        }
        store.images.get_mut(&parent.0).expect("parent").base_delay_ms = 100;
        let t0 = Instant::now();
        // 1 loop only.
        store.force_animation_state_for_test(
            parent,
            AnimationState {
                play_mode: PlayMode::LoopFinite { remaining: 1 },
                current_frame: 0,
                current_frame_started: t0,
            },
        );
        let entry = store.images.get(&parent.0).expect("parent");
        // Play through base → f1 → f2 → would wrap but remaining=1 →
        // freeze on f2 (index 2).
        assert_eq!(resolve_current_frame(entry, t0 + Duration::from_millis(500)), 2);
        assert_eq!(resolve_current_frame(entry, t0 + Duration::from_secs(5)), 2);
    }

    #[test]
    fn next_frame_deadline_none_for_stopped_image() {
        let mut store = Store::new(DEFAULT_CAP_BYTES);
        let Some((_d, _q, _p, _parent)) = make_frame_test_image(&mut store, (2, 2)) else {
            return;
        };
        assert!(store.next_frame_deadline(Instant::now()).is_none());
    }

    #[test]
    fn next_frame_deadline_reflects_running_image() {
        let mut store = Store::new(DEFAULT_CAP_BYTES);
        let Some((_d, _q, _p, parent)) = make_frame_test_image(&mut store, (2, 2)) else {
            return;
        };
        let (_d, _q, _p, image) = try_make_pipeline_and_image().expect("gpu");
        let entry = store.images.get_mut(&parent.0).expect("parent");
        entry.frames.push(Frame {
            image,
            rgba: vec![0u8; 16],
            delay_ms: 80,
            bytes: 0,
        });
        // Match the test's expectation: base gap = 80ms so first
        // frame swap deadline is t0 + 80ms.
        entry.base_delay_ms = 80;
        let t0 = Instant::now();
        store.force_animation_state_for_test(
            parent,
            AnimationState {
                play_mode: PlayMode::LoopForever,
                current_frame: 0,
                current_frame_started: t0,
            },
        );
        let dl = store.next_frame_deadline(t0).expect("animating");
        assert!(dl >= t0 + Duration::from_millis(70));
        assert!(dl <= t0 + Duration::from_millis(90));
    }

    #[test]
    fn peek_at_returns_base_for_static_image() {
        let Some((_d, _q, _p, _img)) = try_make_pipeline_and_image() else { return };
        let mut store = Store::new(DEFAULT_CAP_BYTES);
        let Some((_d, _q, _p, parent)) = make_frame_test_image(&mut store, (2, 2)) else {
            return;
        };
        let a = store.peek_at(parent, Instant::now()).map(|g| g.width_px);
        let b = store.peek(parent).map(|g| g.width_px);
        assert_eq!(a, b);
        assert_eq!(a, Some(2));
    }

    //
    // Kitty graphics dispatch E2E for animation. Feeds the same APC
    // bytes through Terminal::feed that an `icat --transfer-mode=memory
    // --no-stop-on-error` (with frames) would send, then exercises the
    // ingest path all the way to Store::frame_count.
    //

    #[test]
    fn e2e_kitty_a_f_appends_frame_to_parent_image() {
        use crate::terminal::Terminal;
        let Some((d, q, pipeline, _)) = try_make_pipeline_and_image() else { return };
        let mut term = Terminal::new(80, 24, 100);
        term.set_cell_size_px(8, 16);
        let mut store = Store::new(DEFAULT_CAP_BYTES);

        // Base image: 4×4 PNG.
        let base_png = make_png(4, 4);
        let apc_base = make_kitty_apc("a=T,f=100,i=42,c=2,r=1", &base_png);
        term.feed(&apc_base);
        let uploads = term.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1, "base transmission queues one upload");
        let up = uploads.into_iter().next().unwrap();
        assert_eq!(up.kitty_image_id, Some(42));
        let (_, base_id) = store.request_insert_animatable(
            up.bytes,
            100,
            Duration::from_secs(5),
            up.label,
        );
        term.register_kitty_image_id(42, base_id);
        let _ = poll_until_result(&mut store, &pipeline, &d, &q, Duration::from_secs(2));
        assert!(store.peek(base_id).is_some(), "base decoded");
        assert!(
            store.images.get(&base_id.0).and_then(|e| e.base_rgba.as_ref()).is_some(),
            "base_rgba populated for animatable image"
        );

        // a=f frame: a 2×2 green PNG, gap 30ms, append.
        let frame_png = {
            let buf = image::RgbaImage::from_pixel(2, 2, image::Rgba([0, 255, 0, 255]));
            let mut bytes = Vec::new();
            image::DynamicImage::ImageRgba8(buf)
                .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageOutputFormat::Png)
                .expect("encode");
            bytes
        };
        let apc_frame = make_kitty_apc("a=f,f=100,i=42,z=30", &frame_png);
        term.feed(&apc_frame);
        let uploads = term.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1, "frame transmission queues one upload");
        let up = uploads.into_iter().next().unwrap();
        let spec = up.animation_frame.expect("a=f flags animation_frame");
        assert_eq!(spec.gap_ms, 30);
        assert_eq!(spec.target_slot, None);
        // Route through the store.
        let pid = store
            .request_insert_frame(
                base_id,
                up.bytes,
                100,
                Duration::from_secs(5),
                up.label,
                spec.target_slot,
                spec.compose_base,
                spec.gap_ms,
                spec.dst_x,
                spec.dst_y,
            )
            .expect("parent exists");
        // Drive the decode through poll.
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let r = store.poll(&pipeline, &d, &q, false);
            if r.iter().any(|(id, _)| *id == pid) {
                break;
            }
            if Instant::now() > deadline {
                panic!("frame decode never completed");
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(store.frame_count(base_id), 1);
        let entry = store.images.get(&base_id.0).expect("base present");
        assert_eq!(entry.frames[0].delay_ms, 30);
    }

    #[test]
    fn request_insert_frame_returns_parent_not_ready_for_in_flight_base() {
        // Reserve an ImageId (pending base) but never drive the
        // decode to completion. A frame upload arriving in that
        // window must return ParentNotReady, not BudgetExceeded.
        let mut store = Store::new(DEFAULT_CAP_BYTES);
        let png = make_png(2, 2);
        let (_pid, parent) = store.request_insert_animatable(
            png,
            100,
            Duration::from_secs(60), // long timeout
            None,
        );
        // Frame arrives before base decode completes.
        let pid = store
            .request_insert_frame(
                parent,
                make_png(1, 1),
                100,
                Duration::from_secs(5),
                None,
                None,
                None,
                10,
                0, 0,
            )
            .expect("parent reservation exists");
        let Some((d, q, pipeline, _)) = try_make_pipeline_and_image() else { return };
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut frame_outcome = None;
        loop {
            let results = store.poll(&pipeline, &d, &q, false);
            for (id, r) in results {
                if id == pid {
                    frame_outcome = Some(r);
                }
            }
            if frame_outcome.is_some() {
                break;
            }
            if Instant::now() > deadline {
                panic!("frame decode never resolved");
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        // Note: there's a race here — if the base decode happens to
        // beat the frame to the poll-loop, the frame would succeed.
        // Make the assertion tolerant: it's either ParentNotReady
        // (the typical case) OR Ok (the race-loser case). What we
        // explicitly forbid is BudgetExceeded for this scenario.
        match frame_outcome.unwrap() {
            Err(DecodeError::ParentNotReady) => {}
            Ok(_) => {}
            other => panic!("unexpected outcome: {other:?}"),
        }
    }

    #[test]
    fn finish_frame_decode_replacing_existing_frame_keeps_byte_accounting_consistent() {
        // Bug fix verification: per-image `parent.bytes` and
        // store-wide `total_bytes` must move in lockstep across
        // replacements. Before the fix, repeated edits drifted
        // `parent.bytes` upward and the next `retain` carried the
        // drift into the global.
        let Some((d, q, pipeline, _)) = try_make_pipeline_and_image() else { return };
        let mut store = Store::new(DEFAULT_CAP_BYTES);
        let Some((_d2, _q2, _p2, parent)) = make_frame_test_image(&mut store, (2, 2)) else {
            return;
        };
        // Add one frame.
        let frame_png = {
            let buf = image::RgbaImage::from_pixel(1, 1, image::Rgba([10, 20, 30, 255]));
            let mut bytes = Vec::new();
            image::DynamicImage::ImageRgba8(buf)
                .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageOutputFormat::Png)
                .expect("encode");
            bytes
        };
        let pid = store
            .request_insert_frame(
                parent,
                frame_png.clone(),
                100,
                Duration::from_secs(5),
                None,
                None,
                None,
                0,
                0, 0,
            )
            .expect("parent exists");
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let r = store.poll(&pipeline, &d, &q, false);
            if r.iter().any(|(id, _)| *id == pid) { break; }
            if Instant::now() > deadline { panic!("decode timeout"); }
            std::thread::sleep(Duration::from_millis(5));
        }
        let bytes_after_first = store.bytes();
        let parent_bytes_after_first =
            store.images.get(&parent.0).expect("parent").bytes;

        // Replace the same frame (target_slot = 2 → frames[0]) twice.
        for _ in 0..2 {
            let pid = store
                .request_insert_frame(
                    parent,
                    frame_png.clone(),
                    100,
                    Duration::from_secs(5),
                    None,
                    Some(2),
                    None,
                    0,
                    0, 0,
                )
                .expect("parent exists");
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                let r = store.poll(&pipeline, &d, &q, false);
                if r.iter().any(|(id, _)| *id == pid) { break; }
                if Instant::now() > deadline { panic!("decode timeout"); }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        assert_eq!(store.frame_count(parent), 1, "still exactly one frame");
        assert_eq!(
            store.bytes(),
            bytes_after_first,
            "store-wide bytes unchanged across replacements",
        );
        assert_eq!(
            store.images.get(&parent.0).expect("parent").bytes,
            parent_bytes_after_first,
            "per-image bytes unchanged across replacements",
        );
        // retain() of the live set must not drift the global.
        let mut keep = HashSet::new();
        keep.insert(parent);
        store.retain(&keep);
        assert_eq!(store.bytes(), bytes_after_first);
    }

    #[test]
    fn apply_animation_control_unknown_id_is_silent_noop() {
        let mut store = Store::new(DEFAULT_CAP_BYTES);
        // No image with id 9999 — call must not panic and must not
        // create a state entry.
        store.apply_animation_control(
            ImageId(9999),
            Some(3),
            Some(0),
            None,
            None,
            None,
            Instant::now(),
        );
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn apply_animation_control_s2_sets_run_while_loading() {
        let mut store = Store::new(DEFAULT_CAP_BYTES);
        let Some((_d, _q, _p, parent)) = make_frame_test_image(&mut store, (2, 2)) else {
            return;
        };
        store.apply_animation_control(
            parent,
            Some(2),
            None,
            None,
            None,
            None,
            Instant::now(),
        );
        assert_eq!(
            store.animation_state(parent).unwrap().play_mode,
            PlayMode::RunWhileLoading,
        );
    }

    #[test]
    fn apply_animation_control_unknown_s_value_leaves_play_mode_unchanged() {
        let mut store = Store::new(DEFAULT_CAP_BYTES);
        let Some((_d, _q, _p, parent)) = make_frame_test_image(&mut store, (2, 2)) else {
            return;
        };
        // Start running.
        store.apply_animation_control(
            parent,
            Some(2),
            None,
            None,
            None,
            None,
            Instant::now(),
        );
        // Now send an unknown s= value.
        store.apply_animation_control(
            parent,
            Some(99),
            None,
            None,
            None,
            None,
            Instant::now(),
        );
        assert_eq!(
            store.animation_state(parent).unwrap().play_mode,
            PlayMode::RunWhileLoading,
            "unknown control op leaves play_mode unchanged",
        );
    }

    #[test]
    fn apply_animation_control_edits_frame_gap() {
        let mut store = Store::new(DEFAULT_CAP_BYTES);
        let Some((_d, _q, _p, parent)) = make_frame_test_image(&mut store, (2, 2)) else {
            return;
        };
        // Push two frames each with gap 100.
        for _ in 0..2 {
            let (_d, _q, _p, image) = try_make_pipeline_and_image().expect("gpu");
            let entry = store.images.get_mut(&parent.0).expect("parent");
            entry.frames.push(Frame {
                image,
                rgba: vec![0u8; 16],
                delay_ms: 100,
                bytes: 0,
            });
        }
        // Edit frame 2's gap to 250ms.
        store.apply_animation_control(
            parent,
            None,
            None,
            None,
            Some(2),
            Some(250),
            Instant::now(),
        );
        assert_eq!(
            store.images.get(&parent.0).expect("parent").frames[0].delay_ms,
            250,
        );
        // frame 3's gap (frames[1]) untouched.
        assert_eq!(
            store.images.get(&parent.0).expect("parent").frames[1].delay_ms,
            100,
        );
    }

    #[test]
    fn apply_animation_control_r1_sets_base_delay_without_touching_frames() {
        // r=1 targets the base, which has its own `base_delay_ms`
        // slot — separate from any entry in `frames`. Verify the
        // write lands on the base and that frame[0] (Kitty frame 2)
        // is untouched.
        let mut store = Store::new(DEFAULT_CAP_BYTES);
        let Some((_d, _q, _p, parent)) = make_frame_test_image(&mut store, (2, 2)) else {
            return;
        };
        let (_d, _q, _p, image) = try_make_pipeline_and_image().expect("gpu");
        let entry = store.images.get_mut(&parent.0).expect("parent");
        entry.frames.push(Frame {
            image,
            rgba: vec![0u8; 16],
            delay_ms: 77,
            bytes: 0,
        });
        store.apply_animation_control(
            parent,
            None,
            None,
            None,
            Some(1),
            Some(250),
            Instant::now(),
        );
        let entry = store.images.get(&parent.0).expect("parent");
        assert_eq!(entry.base_delay_ms, 250, "r=1 writes the base's delay");
        assert_eq!(entry.frames[0].delay_ms, 77, "r=1 leaves frames[0] alone");
    }

    #[test]
    fn apply_animation_control_make_current_n1_pins_base() {
        let mut store = Store::new(DEFAULT_CAP_BYTES);
        let Some((_d, _q, _p, parent)) = make_frame_test_image(&mut store, (2, 2)) else {
            return;
        };
        for _ in 0..2 {
            let (_d, _q, _p, image) = try_make_pipeline_and_image().expect("gpu");
            let entry = store.images.get_mut(&parent.0).expect("parent");
            entry.frames.push(Frame {
                image,
                rgba: vec![0u8; 16],
                delay_ms: 100,
                bytes: 0,
            });
        }
        store.apply_animation_control(
            parent,
            None,
            None,
            Some(1),
            None,
            None,
            Instant::now(),
        );
        let s = store.animation_state(parent).unwrap();
        assert_eq!(s.current_frame, 0);
        assert_eq!(s.play_mode, PlayMode::Stopped);
    }

    #[test]
    fn apply_animation_control_make_current_takes_priority_over_control() {
        // Spec contract: `c=` and `s=` aren't combined in one message.
        // When the parser sees both, `make_current` wins.
        let mut store = Store::new(DEFAULT_CAP_BYTES);
        let Some((_d, _q, _p, parent)) = make_frame_test_image(&mut store, (2, 2)) else {
            return;
        };
        let (_d, _q, _p, image) = try_make_pipeline_and_image().expect("gpu");
        store.images.get_mut(&parent.0).expect("parent").frames.push(Frame {
            image,
            rgba: vec![0u8; 16],
            delay_ms: 100,
            bytes: 0,
        });
        store.apply_animation_control(
            parent,
            Some(3), // would normally set LoopForever
            Some(0),
            Some(2), // make-current frame 2 → index 1
            None,
            None,
            Instant::now(),
        );
        let s = store.animation_state(parent).unwrap();
        assert_eq!(s.play_mode, PlayMode::Stopped, "make_current wins over control");
        assert_eq!(s.current_frame, 1);
    }

    #[test]
    fn peek_at_returns_frame_image_when_advanced_past_base() {
        let mut store = Store::new(DEFAULT_CAP_BYTES);
        let Some((_d, _q, _p, parent)) = make_frame_test_image(&mut store, (2, 2)) else {
            return;
        };
        let (_d, _q, _p, image) = try_make_pipeline_and_image().expect("gpu");
        let frame_w = image.width_px;
        store.images.get_mut(&parent.0).expect("parent").frames.push(Frame {
            image,
            rgba: vec![0u8; 16],
            delay_ms: 100,
            bytes: 0,
        });
        // Make frame 2 (index 1) the static current.
        store.apply_animation_control(
            parent,
            None,
            None,
            Some(2),
            None,
            None,
            Instant::now(),
        );
        let got = store.peek_at(parent, Instant::now()).map(|g| g.width_px);
        assert_eq!(got, Some(frame_w));
    }

    #[test]
    fn next_frame_deadline_returns_earliest_across_multiple_animating_images() {
        let mut store = Store::new(DEFAULT_CAP_BYTES);
        let Some((_d, _q, _p, slow)) = make_frame_test_image(&mut store, (2, 2)) else {
            return;
        };
        let Some((_d, _q, _p, fast)) = make_frame_test_image(&mut store, (2, 2)) else {
            return;
        };
        for (parent, delay) in [(slow, 200u32), (fast, 50u32)] {
            let (_d, _q, _p, image) = try_make_pipeline_and_image().expect("gpu");
            let entry = store.images.get_mut(&parent.0).expect("parent");
            entry.frames.push(Frame {
                image,
                rgba: vec![0u8; 16],
                delay_ms: delay,
                bytes: 0,
            });
            // Each image's base gap matches its frame gap so the
            // earliest-deadline calculation lines up with the test's
            // intuition about which image fires first.
            entry.base_delay_ms = delay;
        }
        let t0 = Instant::now();
        for parent in [slow, fast] {
            store.force_animation_state_for_test(
                parent,
                AnimationState {
                    play_mode: PlayMode::LoopForever,
                    current_frame: 0,
                    current_frame_started: t0,
                },
            );
        }
        let dl = store.next_frame_deadline(t0).expect("animating");
        // Must be near the `fast` image's 50ms, not the `slow` 200ms.
        assert!(dl >= t0 + Duration::from_millis(40));
        assert!(dl <= t0 + Duration::from_millis(60));
    }

    #[test]
    fn request_insert_frame_compose_base_n_uses_prior_frame_rgba() {
        // Verify the 1-based → 0-based mapping for explicit compose_base:
        // Some(0) / Some(1) → base, Some(2) → frames[0], etc.
        // (The None / omitted case is exercised separately by
        // `request_insert_frame_compose_base_none_uses_previous_frame`.)
        let Some((d, q, pipeline, _)) = try_make_pipeline_and_image() else { return };
        let mut store = Store::new(DEFAULT_CAP_BYTES);
        let Some((_d, _q, _p, parent)) = make_frame_test_image(&mut store, (2, 2)) else {
            return;
        };
        // Set up frame 2 manually with a known RGBA (all 7s).
        let (_d, _q, _p, image) = try_make_pipeline_and_image().expect("gpu");
        let frame2_rgba = vec![7u8; 2 * 2 * 4];
        store.images.get_mut(&parent.0).expect("parent").frames.push(Frame {
            image,
            rgba: frame2_rgba.clone(),
            delay_ms: 100,
            bytes: 0,
        });

        // Send a 1×1 fully-transparent PNG (will not overwrite any
        // pixel, so the composite output equals the source). Tells us
        // which source the composite read from.
        let transparent_png = {
            let buf = image::RgbaImage::from_pixel(1, 1, image::Rgba([0, 0, 0, 0]));
            let mut bytes = Vec::new();
            image::DynamicImage::ImageRgba8(buf)
                .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageOutputFormat::Png)
                .expect("encode");
            bytes
        };
        let pid = store
            .request_insert_frame(
                parent,
                transparent_png,
                100,
                Duration::from_secs(5),
                None,
                None,
                Some(2), // compose against frame 2 (i.e., frames[0])
                0,
                0, 0,
            )
            .expect("parent exists");
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let r = store.poll(&pipeline, &d, &q, false);
            if r.iter().any(|(id, _)| *id == pid) { break; }
            if Instant::now() > deadline { panic!("decode timeout"); }
            std::thread::sleep(Duration::from_millis(5));
        }
        // frames[1] should equal frame2_rgba — the source we composed
        // against.
        let entry = store.images.get(&parent.0).expect("parent");
        assert_eq!(entry.frames[1].rgba, frame2_rgba);
    }

    #[test]
    fn request_insert_frame_compose_base_none_uses_previous_frame() {
        // Regression for the "base shows through behind animating
        // frames" bug. icat sends a=f with no c=, so compose_base
        // arrives as None. The Kitty spec says the default is to
        // compose against the previous frame (the most recent one),
        // not against the base. For a delta-encoded GIF, defaulting
        // to base would discard accumulated motion every frame.
        let Some((d, q, pipeline, _)) = try_make_pipeline_and_image() else { return };
        let mut store = Store::new(DEFAULT_CAP_BYTES);
        let Some((_d, _q, _p, parent)) = make_frame_test_image(&mut store, (2, 2)) else {
            return;
        };
        // Set up a prior frame with a sentinel RGBA pattern. If the
        // compose source is the BASE (the bug), this pattern will
        // not appear in the new frame. If the compose source is the
        // PREVIOUS frame (the fix), it will.
        let (_d, _q, _p, image) = try_make_pipeline_and_image().expect("gpu");
        let prev_rgba = vec![42u8; 2 * 2 * 4];
        store.images.get_mut(&parent.0).expect("parent").frames.push(Frame {
            image,
            rgba: prev_rgba.clone(),
            delay_ms: 100,
            bytes: 0,
        });

        // Fully-transparent 1×1 PNG so the composite output equals
        // the source. Compose_base = None — spec says use previous.
        let transparent_png = {
            let buf = image::RgbaImage::from_pixel(1, 1, image::Rgba([0, 0, 0, 0]));
            let mut bytes = Vec::new();
            image::DynamicImage::ImageRgba8(buf)
                .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageOutputFormat::Png)
                .expect("encode");
            bytes
        };
        let pid = store
            .request_insert_frame(
                parent,
                transparent_png,
                100,
                Duration::from_secs(5),
                None,
                None,
                None, // <-- the key bit: compose_base omitted
                0,
                0, 0,
            )
            .expect("parent exists");
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let r = store.poll(&pipeline, &d, &q, false);
            if r.iter().any(|(id, _)| *id == pid) { break; }
            if Instant::now() > deadline { panic!("decode timeout"); }
            std::thread::sleep(Duration::from_millis(5));
        }
        let entry = store.images.get(&parent.0).expect("parent");
        assert_eq!(
            entry.frames[1].rgba, prev_rgba,
            "omitted c= must compose against the previous frame, not the base",
        );
    }

    #[test]
    fn request_insert_frame_compose_base_none_falls_back_to_base_for_first_frame() {
        // For the *first* a=f after the base (no previous frame in
        // the sequence yet), None must fall back to the base —
        // there's nothing else to compose against.
        let Some((d, q, pipeline, _)) = try_make_pipeline_and_image() else { return };
        let mut store = Store::new(DEFAULT_CAP_BYTES);
        let Some((_d, _q, _p, parent)) = make_frame_test_image(&mut store, (2, 2)) else {
            return;
        };
        // make_frame_test_image set base_rgba to all 0x55s. No prior
        // frames in the sequence — None should resolve to that base.
        let transparent_png = {
            let buf = image::RgbaImage::from_pixel(1, 1, image::Rgba([0, 0, 0, 0]));
            let mut bytes = Vec::new();
            image::DynamicImage::ImageRgba8(buf)
                .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageOutputFormat::Png)
                .expect("encode");
            bytes
        };
        let pid = store
            .request_insert_frame(
                parent,
                transparent_png,
                100,
                Duration::from_secs(5),
                None,
                None,
                None,
                0,
                0, 0,
            )
            .expect("parent exists");
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let r = store.poll(&pipeline, &d, &q, false);
            if r.iter().any(|(id, _)| *id == pid) { break; }
            if Instant::now() > deadline { panic!("decode timeout"); }
            std::thread::sleep(Duration::from_millis(5));
        }
        let entry = store.images.get(&parent.0).expect("parent");
        let expected = vec![0x55u8; 2 * 2 * 4];
        assert_eq!(
            entry.frames[0].rgba, expected,
            "first a=f with no previous frame falls back to base",
        );
    }

    #[test]
    fn request_insert_animatable_rgba_skips_worker_and_resolves_on_next_poll() {
        // Worker-bypass path for raw RGBA base images. The reservation
        // is created and the pre-decoded result is queued onto
        // `immediate_results` synchronously; the next `poll` should
        // upload to the GPU without ever touching the worker thread.
        let Some((d, q, pipeline, _)) = try_make_pipeline_and_image() else { return };
        let mut store = Store::new(DEFAULT_CAP_BYTES);
        let rgba = vec![0x77u8; 4 * 4 * 4];
        let (pid, _) = store.request_insert_animatable_rgba(rgba, 4, 4, None);
        assert_eq!(
            store.pending_count(),
            1,
            "reservation tracked",
        );
        // One poll resolves the bypass entry — no worker round-trip
        // required even with a tight cap on iterations.
        let results = store.poll(&pipeline, &d, &q, false);
        assert!(
            results.iter().any(|(p, _)| *p == pid),
            "bypass entry resolves on the very next poll",
        );
        assert_eq!(store.pending_count(), 0);
    }

    #[test]
    fn request_insert_frame_rgba_skips_worker_and_resolves_on_next_poll() {
        let Some((d, q, pipeline, _)) = try_make_pipeline_and_image() else { return };
        let mut store = Store::new(DEFAULT_CAP_BYTES);
        let Some((_d, _q, _p, parent)) = make_frame_test_image(&mut store, (2, 2)) else {
            return;
        };
        let rgba = vec![0u8; 2 * 2 * 4];
        let pid = store
            .request_insert_frame_rgba(
                parent, rgba, 2, 2, None, None, None, 50, 0, 0,
            )
            .expect("parent exists");
        let results = store.poll(&pipeline, &d, &q, false);
        assert!(results.iter().any(|(p, _)| *p == pid));
        assert_eq!(store.frame_count(parent), 1);
    }

    #[test]
    fn request_insert_frame_full_size_no_c_overwrites_instead_of_blending() {
        // Regression for the "first frame visible behind animating
        // frames" bug on transparent GIFs. icat sends a=f frames at
        // the parent's full dimensions, dst=(0,0), no c= — its
        // de-facto signal that the frame IS the new image (each
        // payload has its own transparency, including transparent
        // regions that should show terminal bg, not the prior
        // frame). Alpha-blending here would let the prior frame's
        // pixels bleed through and stack across the timeline.
        let Some((d, q, pipeline, _)) = try_make_pipeline_and_image() else { return };
        let mut store = Store::new(DEFAULT_CAP_BYTES);
        let Some((_d, _q, _p, parent)) = make_frame_test_image(&mut store, (2, 2)) else {
            return;
        };
        // Seed an opaque sentinel frame so we can tell whether the
        // composite blended against it (bug) or just used the new
        // payload (fix).
        let (_d, _q, _p, image) = try_make_pipeline_and_image().expect("gpu");
        let prev_rgba = vec![0xCCu8; 2 * 2 * 4];
        store.images.get_mut(&parent.0).expect("parent").frames.push(Frame {
            image,
            rgba: prev_rgba,
            delay_ms: 100,
            bytes: 0,
        });

        // Fully-transparent 2×2 PNG. Same dims as parent, dst=(0,0),
        // no compose_base → heuristic should overwrite.
        let transparent_png = {
            let buf = image::RgbaImage::from_pixel(2, 2, image::Rgba([0, 0, 0, 0]));
            let mut bytes = Vec::new();
            image::DynamicImage::ImageRgba8(buf)
                .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageOutputFormat::Png)
                .expect("encode");
            bytes
        };
        let pid = store
            .request_insert_frame(
                parent,
                transparent_png,
                100,
                Duration::from_secs(5),
                None,
                None,
                None, // c= omitted → full-replacement heuristic fires
                0,
                0, 0, // dst=(0,0)
            )
            .expect("parent exists");
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let r = store.poll(&pipeline, &d, &q, false);
            if r.iter().any(|(id, _)| *id == pid) { break; }
            if Instant::now() > deadline { panic!("decode timeout"); }
            std::thread::sleep(Duration::from_millis(5));
        }
        // The new frame should be fully transparent (all zeros) —
        // the prior frame's 0xCC sentinel must NOT have bled through.
        let entry = store.images.get(&parent.0).expect("parent");
        assert_eq!(
            entry.frames[1].rgba,
            vec![0u8; 2 * 2 * 4],
            "full-size frame at (0,0) with no c= must overwrite, not blend with prior",
        );
    }

    #[test]
    fn request_insert_frame_partial_size_still_blends_against_previous() {
        // The full-replacement heuristic must NOT fire when the
        // frame is a true delta (smaller than parent OR offset).
        // The previous (working) icat GIF used this shape with
        // explicit c=, but verify that even with c= omitted a
        // partial frame still blends rather than blanking the
        // surrounding region.
        let Some((d, q, pipeline, _)) = try_make_pipeline_and_image() else { return };
        let mut store = Store::new(DEFAULT_CAP_BYTES);
        // 4x4 parent so a 2x2 frame at (1,1) is genuinely partial.
        let mut s = Store::new(DEFAULT_CAP_BYTES);
        let Some((_d, _q, _p, parent)) = make_frame_test_image(&mut s, (4, 4)) else {
            return;
        };
        let _ = (d, q, pipeline, parent, store); // hand back the throwaways
        let Some((d, q, pipeline, parent)) = make_frame_test_image(&mut s, (4, 4)) else {
            return;
        };
        // Seed a prior frame with sentinel pixels everywhere.
        let (_d, _q, _p, image) = try_make_pipeline_and_image().expect("gpu");
        let prev_rgba = vec![0xCCu8; 4 * 4 * 4];
        s.images.get_mut(&parent.0).expect("parent").frames.push(Frame {
            image,
            rgba: prev_rgba,
            delay_ms: 100,
            bytes: 0,
        });
        // 2x2 transparent frame at (1,1) — partial size, so the
        // heuristic shouldn't fire. With c= omitted the new frame
        // blends against the previous (whose pixels stay outside
        // the 2x2 region).
        let transparent_png = {
            let buf = image::RgbaImage::from_pixel(2, 2, image::Rgba([0, 0, 0, 0]));
            let mut bytes = Vec::new();
            image::DynamicImage::ImageRgba8(buf)
                .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageOutputFormat::Png)
                .expect("encode");
            bytes
        };
        let pid = s
            .request_insert_frame(
                parent,
                transparent_png,
                100,
                Duration::from_secs(5),
                None,
                None,
                None,
                0,
                1, 1,
            )
            .expect("parent exists");
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let r = s.poll(&pipeline, &d, &q, false);
            if r.iter().any(|(id, _)| *id == pid) { break; }
            if Instant::now() > deadline { panic!("decode timeout"); }
            std::thread::sleep(Duration::from_millis(5));
        }
        // Partial frame should have kept the prior frame's 0xCC
        // outside the 2x2 region (and inside the region too, since
        // the new payload is transparent).
        let entry = s.images.get(&parent.0).expect("parent");
        assert_eq!(
            entry.frames[1].rgba,
            vec![0xCCu8; 4 * 4 * 4],
            "partial frame with transparent payload preserves prior frame",
        );
    }

    #[test]
    fn e2e_kitty_a_a_c1_stops_playback_and_pins_frame() {
        use crate::terminal::Terminal;
        let mut store = Store::new(DEFAULT_CAP_BYTES);
        let Some((_d, _q, _p, parent)) = make_frame_test_image(&mut store, (2, 2)) else {
            return;
        };
        // Pretend we have two frames + base.
        for _ in 0..2 {
            let (_d, _q, _p, image) = try_make_pipeline_and_image().expect("gpu");
            let entry = store.images.get_mut(&parent.0).expect("parent");
            entry.frames.push(Frame {
                image,
                rgba: vec![0u8; 16],
                delay_ms: 50,
                bytes: 0,
            });
        }
        let mut term = Terminal::new(80, 24, 100);
        term.register_kitty_image_id(7, parent);
        term.feed("\x1b_Ga=a,i=7,s=1\x1b\\");
        let uploads = term.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1, "a=a queues a control message");
        let ctrl = uploads.into_iter().next().unwrap().animation_control.expect("flag");
        assert_eq!(ctrl.control, Some(1));
        store.apply_animation_control(
            parent,
            ctrl.control,
            ctrl.loop_count,
            ctrl.make_current,
            ctrl.edit_frame,
            ctrl.edit_gap_ms,
            Instant::now(),
        );
        assert_eq!(
            store.animation_state(parent).unwrap().play_mode,
            PlayMode::Stopped,
        );
    }
}
