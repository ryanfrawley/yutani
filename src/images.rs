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
    issued_at: Instant,
    timeout: Duration,
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
        let image_id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        self.images.insert(
            image_id,
            StoredImage {
                image: None,
                preview: None,
                bytes: 0,
                last_used: Instant::now(),
            },
        );

        let pending_id = self.next_pending;
        self.next_pending = self.next_pending.wrapping_add(1).max(1);
        self.pending.insert(
            pending_id,
            PendingRequest { image_id, issued_at: Instant::now(), timeout },
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

        while let Ok(result) = self.result_rx.try_recv() {
            let pid = result.pending_id;
            // If the matching pending entry is gone, the request timed out
            // already — silently drop the late result. The reservation
            // was already removed in the timeout branch above.
            let Some(req) = self.pending.remove(&pid) else { continue };
            let outcome = match result.outcome {
                Err(e) => {
                    // Decode failure: evict the reservation.
                    self.images.remove(&req.image_id);
                    Err(e)
                }
                Ok(decoded) => {
                    let bytes_used = (decoded.width as usize) * (decoded.height as usize) * 4;
                    if self.total_bytes.saturating_add(bytes_used) > self.cap_bytes {
                        // Over budget: evict the reservation rather than
                        // hold an empty slot indefinitely.
                        self.images.remove(&req.image_id);
                        let available = self.cap_bytes.saturating_sub(self.total_bytes);
                        Err(DecodeError::BudgetExceeded { needed: bytes_used, available })
                    } else {
                        let image = pipeline.upload_rgba(
                            device,
                            queue,
                            &decoded.rgba,
                            decoded.width,
                            decoded.height,
                            nearest_filter,
                            result.label.as_deref(),
                        );
                        // Build the half-block preview from the same RGBA
                        // buffer before it gets dropped. Cheap (one box
                        // filter), tiny memory (≤ MAX_PREVIEW_* * 4 bytes).
                        // Held even when `images_enabled = true` so a
                        // runtime config flip can switch over instantly
                        // without waiting for the next decode.
                        let preview = build_preview(&decoded.rgba, decoded.width, decoded.height);
                        // Fill the reservation in place — DON'T allocate
                        // a new id, otherwise placements would reference
                        // the stale id and render blank forever.
                        let entry = self
                            .images
                            .get_mut(&req.image_id)
                            .expect("reservation present until decode resolves");
                        entry.image = Some(image);
                        entry.preview = preview;
                        entry.bytes = bytes_used;
                        entry.last_used = Instant::now();
                        self.total_bytes += bytes_used;
                        Ok(ImageId(req.image_id))
                    }
                }
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
    pub fn peek(&self, id: ImageId) -> Option<&GpuImage> {
        self.images.get(&id.0).and_then(|e| e.image.as_ref())
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
        self.images.insert(
            id,
            StoredImage {
                image: Some(gpu_image),
                preview: None,
                bytes,
                last_used: Instant::now(),
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
}
