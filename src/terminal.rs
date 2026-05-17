use crate::ansi::{self, Event};
use crate::images::ImageId;
use crate::style::{Cell, Style};
use std::collections::VecDeque;

/// Per-instance handle for a placement on the grid. Distinct from the image
/// data behind it so multiple placements of the same image can be tracked
/// and removed independently (Kitty `p=` semantics in phase 3).
pub type PlacementId = u32;

/// One image placement anchored at a grid position. The anchor uses `isize`
/// so a placement whose top edge has scrolled above the viewport (during a
/// scroll burst or smooth-scroll animation) is still representable; the
/// renderer clips with the camera ortho rather than the grid clamping it.
#[derive(Clone, Debug, PartialEq)]
pub struct Placement {
    pub id: PlacementId,
    pub image: ImageId,
    /// Anchor row in viewport coords (negative = top edge above viewport).
    pub top_row: isize,
    /// Anchor column in viewport coords (negative = left edge off-screen).
    pub left_col: isize,
    /// Extent in cells. Drives hit-testing (which cells the image covers
    /// for the purposes of scrolling and the eventual delete-by-cell ops)
    /// and the off-screen check that promotes placements to scrollback.
    pub rows: u16,
    pub cols: u16,
    /// Tie-break when two placements overlap. Higher = drawn later (on
    /// top). Phase 1 doesn't expose this to apps — kept at 0 — but the
    /// field is here so the eventual Kitty `z=` parameter has a home.
    pub z: i32,
    /// Sub-cell pixel offset from the anchor cell's top-left, in
    /// framebuffer pixels. `(0, 0)` snaps to the cell grid (phase 1
    /// behavior); positive values shift the draw right/down. Plumbed so
    /// phase 2's Kitty `X=` / `Y=` params land here without further
    /// renderer changes. Doesn't affect eviction / scroll math — those
    /// still work in whole cells using `rows`/`cols`.
    pub pixel_offset: (i32, i32),
    /// Source crop in image pixel coords, `(x, y, w, h)`. `None` samples
    /// the whole image (phase 1 behavior); `Some(...)` selects a sub-rect.
    /// Renderer converts to UVs against the GpuImage's known width/height.
    /// Future home for Kitty's source-rectangle / animated-frame slicing.
    pub src_rect: Option<(u32, u32, u32, u32)>,
    /// Kitty graphics-protocol `i=` image id this placement was created
    /// with. `None` for iTerm OSC 1337 / Cmd-Shift-I / any other
    /// non-Kitty source. Lets `a=d,d=i,i=N` find every placement that
    /// references a given client image without scanning the entire grid.
    pub kitty_image_id: Option<u32>,
    /// Kitty graphics-protocol `p=` placement id. `None` for
    /// non-Kitty placements or Kitty placements where the client
    /// omitted `p=`. Lets `a=d,d=p,p=N` target one specific placement.
    pub kitty_placement_id: Option<u32>,
}

impl Placement {
    /// Row immediately past the last covered row (exclusive). May exceed
    /// grid height when the placement extends below the viewport.
    pub fn bottom_row(&self) -> isize {
        self.top_row + self.rows as isize
    }

    /// Column immediately past the last covered column (exclusive).
    #[allow(dead_code)]
    pub fn right_col(&self) -> isize {
        self.left_col + self.cols as isize
    }

    /// True when the placement is permanently off the live grid — i.e.
    /// it has scrolled fully above the top, or its row anchor sits below
    /// the bottom edge with no way to come back. Used by scroll/resize
    /// to drop placements that can never be displayed again.
    ///
    /// Horizontal position is intentionally NOT checked here: a
    /// placement anchored past the right edge of the grid is "off
    /// screen" but still recoverable — if the user widens the window
    /// the placement comes back into view. We dropped on horizontal
    /// off-screen for a while and it produced exactly that bug:
    /// shrinking the window past an image deleted it, and growing
    /// back never restored it. The renderer's natural viewport
    /// clipping handles off-screen draw culling, so keeping the
    /// placement around is cheap.
    ///
    /// `grid_cols` is unused but kept on the signature so call sites
    /// don't need to change shape if we add column-based eviction
    /// later for a different reason.
    pub fn fully_off_grid(&self, grid_rows: usize, _grid_cols: usize) -> bool {
        self.bottom_row() <= 0 || self.top_row >= grid_rows as isize
    }

    /// True when the placement's row span has any overlap with [top, bottom]
    /// (inclusive). Used by scroll-region shifting to pick which placements
    /// move with the region's rows.
    fn rows_intersect(&self, top: usize, bottom: usize) -> bool {
        self.bottom_row() > top as isize && self.top_row <= bottom as isize
    }
}

/// How a parser-supplied width / height parameter wants to translate into
/// cell extent. iTerm2 OSC 1337 accepts all four forms; Kitty has its own
/// (slice-N-much-later) but the spec maps cleanly onto this enum.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ImageSizeSpec {
    /// No explicit param — match the image's native size, converted to
    /// cells via the current font metrics.
    Auto,
    /// Exact cell count.
    Cells(u16),
    /// Pixel count; main.rs converts to cells via cell-pixel size.
    Pixels(u32),
    /// Percent of the viewport in the corresponding axis. iTerm uses this
    /// for responsive sizing (`width=50%`).
    Percent(u32),
}

/// A decoded-base64 image payload waiting for main-thread pickup. The
/// terminal parses the protocol-level wrapper (OSC 1337 today, others
/// later) and pushes one of these per inline image; `main.rs` drains
/// `Terminal::take_pending_image_uploads` each frame, fires
/// `Store::request_insert`, and inserts the corresponding `Placement`
/// using the pre-allocated `ImageId`.
#[derive(Clone, Debug, PartialEq)]
pub struct PendingImageUpload {
    /// Raw image bytes (PNG/JPEG/GIF/WebP) — passed straight to the
    /// decode worker. Not yet base64-encoded.
    pub bytes: Vec<u8>,
    /// Pixel dimensions from a header-only peek. `None` if the format
    /// wasn't recognised; main.rs should still attempt the full decode
    /// (which will fail cleanly via `DecodeError::Decode` if so).
    pub pixel_size: Option<(u32, u32)>,
    pub width: ImageSizeSpec,
    pub height: ImageSizeSpec,
    pub preserve_aspect: bool,
    /// iTerm2 `doNotMoveCursor=1`: caller skips the post-placement
    /// line-feed sequence. P2.3 reads this; P2.2 just records.
    pub do_not_move_cursor: bool,
    pub label: Option<String>,
    /// Cell anchor captured when the OSC was processed. main.rs uses this
    /// when it calls `Terminal::insert_placement` so the placement lands
    /// at the cursor position *before* the cursor advanced past the
    /// image — even if subsequent PTY input arrives before the drain.
    pub cell_anchor: (isize, isize),
    /// Cell extent (rows, cols) computed by the OSC handler from the
    /// size spec + image dims + current cell-pixel size.
    pub cell_extent: (u16, u16),
    /// Kitty graphics-protocol `i=` image id when the upload came from
    /// a Kitty APC. `None` for iTerm / Cmd-Shift-I. When `Some`, main.rs
    /// registers the resulting store ImageId via
    /// `Terminal::register_kitty_image_id` so future `a=p` / `a=d`
    /// ops can find the image.
    pub kitty_image_id: Option<u32>,
    /// Kitty graphics-protocol `p=` placement id. Flows into the
    /// `Placement.kitty_placement_id` field so `a=d,d=p,p=N` can target
    /// it. `None` for non-Kitty paths or when the client omitted `p=`.
    pub kitty_placement_id: Option<u32>,
    /// `true` for `a=T` (transmit AND display, the default) — main.rs
    /// creates a `Placement` at `cell_anchor` after the decode lands.
    /// `false` for `a=t` (transmit only) — the image becomes addressable
    /// via `kitty_image_id` for a later `a=p`, but no placement is made.
    pub display_immediately: bool,
    /// Kitty `X=`/`Y=` sub-cell pixel offsets, in framebuffer pixels.
    /// `(0, 0)` snaps to the cell grid (the common case).
    pub pixel_offset: (i32, i32),
    /// Kitty `z=` z-index for stacking. `0` is the default; higher
    /// draws later (on top). Can be negative.
    pub z_index: i32,
    /// Kitty `x=` / `y=` / `w=` / `h=` source crop in image pixels.
    /// `None` samples the whole image (the common case).
    pub src_rect: Option<(u32, u32, u32, u32)>,
    /// Per-frame metadata for `a=f` transmissions. When `Some`,
    /// `bytes` holds the new frame's pixel data and `kitty_image_id`
    /// names the parent image whose frames vec the result will be
    /// appended to. main.rs routes these through
    /// `Store::request_insert_frame` rather than `request_insert`.
    pub animation_frame: Option<KittyAnimationFrameSpec>,
    /// Animation-control message from `a=a`. When `Some`, `bytes` is
    /// empty and there's nothing to decode — main.rs applies the
    /// control op directly to the store for `kitty_image_id`.
    pub animation_control: Option<KittyAnimationControl>,
    /// Bypass marker for raw RGBA payloads (Kitty `f=24`/`f=32` SHM,
    /// post `convert_kitty_raw_to_rgba`). When `Some((w, h))`, `bytes`
    /// is already in the format `Store::request_insert_*_rgba`
    /// expects — main.rs's drain routes through the worker-bypass
    /// path instead of the PNG-encode-then-worker-decode path. PNG
    /// payloads (and any path that needs real decode) leave this
    /// `None`.
    pub raw_rgba_dims: Option<(u32, u32)>,
}

/// `a=f` per-frame metadata. Carried alongside the raw frame payload
/// from terminal.rs to main.rs to Store. Pulled into its own struct
/// so the existing fields of `PendingImageUpload` stay focused on the
/// One contiguous horizontal stretch of Kitty unicode-placeholder
/// cells the renderer can draw as a single textured quad. Produced
/// by [`Terminal::kitty_placeholder_runs`] from the active grid.
///
/// Each run corresponds to a strip of one row of the source image:
/// UV.x spans `[image_col_start / total_cols, image_col_end / total_cols]`,
/// UV.y spans `[image_row / total_rows, (image_row + 1) / total_rows]`.
/// The `total_*` denominators come from
/// [`Terminal::kitty_image_cell_extent`] (the `c=` / `r=` on the
/// original transmission).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct KittyPlaceholderRun {
    /// Kitty `i=` image id the placeholder cells encoded.
    pub client_id: u32,
    /// 0-based row index into the grid this run was scanned from.
    /// The renderer shifts by `view_offset` when converting to a
    /// viewport coordinate.
    pub screen_row: usize,
    /// First grid column the run covers.
    pub screen_col_start: usize,
    /// Exclusive end column.
    pub screen_col_end: usize,
    /// Image-row diacritic value shared by every cell in the run.
    pub image_row: u16,
    /// Image-col diacritic value of the leftmost cell.
    pub image_col_start: u16,
    /// Image-col diacritic value of the rightmost cell + 1. Always
    /// `image_col_start + (screen_col_end - screen_col_start)`
    /// because the grouping rule requires consecutive `image_col`s.
    pub image_col_end: u16,
}

/// base-image case.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KittyAnimationFrameSpec {
    /// `r=` target slot. `None` (or 0) = append a new frame. `Some(n)`
    /// (1-based) replaces frame slot `n` if it exists; otherwise
    /// silently appends.
    pub target_slot: Option<u32>,
    /// `c=` 1-based source frame to use as the composition base.
    /// `None` defaults to frame 1 (the original base image).
    pub compose_base: Option<u32>,
    /// `z=` gap in milliseconds before this frame advances. `0` is
    /// the spec's "as fast as possible" sentinel.
    pub gap_ms: u32,
    /// `X=` top-left x of the new frame's pixel data inside the
    /// parent image, in pixels.
    pub dst_x: u32,
    /// `Y=` top-left y of the new frame's pixel data inside the
    /// parent image, in pixels.
    pub dst_y: u32,
}

/// `a=a` control message. Some combination of these fields is set
/// based on which sub-operation the app requested.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KittyAnimationControl {
    /// `s=` playback control: `Some(1)` stop, `Some(2)` run while
    /// frames are still being loaded, `Some(3)` run with finite loops
    /// (count from `loop_count`).
    pub control: Option<u32>,
    /// `v=` loop count for `s=3`. `Some(0)` means infinite.
    pub loop_count: Option<u32>,
    /// `c=` 1-based frame to make the current static frame. Mutually
    /// exclusive with `control` (the spec doesn't combine the two in
    /// a single message).
    pub make_current: Option<u32>,
    /// `r=` 1-based frame to edit. Paired with `edit_gap_ms`.
    pub edit_frame: Option<u32>,
    /// `z=` new gap for `edit_frame`, in milliseconds.
    pub edit_gap_ms: Option<u32>,
}

#[derive(Clone)]
pub struct Grid {
    pub cells: Vec<Cell>,
    pub rows: usize,
    pub cols: usize,
    /// Image placements anchored to this grid. Per-Grid so primary and
    /// alternate screens hold their own image state — entering an alt-screen
    /// app like vim hides the primary's images and exposes the alt's
    /// (initially empty), matching how text content is segregated.
    pub placements: Vec<Placement>,
}

impl Grid {
    pub fn new(rows: usize, cols: usize, blank: Cell) -> Self {
        Self {
            cells: vec![blank; rows * cols],
            rows,
            cols,
            placements: Vec::new(),
        }
    }

    fn idx(&self, row: usize, col: usize) -> usize {
        row * self.cols + col
    }

    pub fn get(&self, row: usize, col: usize) -> Cell {
        self.cells[self.idx(row, col)]
    }

    pub fn set(&mut self, row: usize, col: usize, cell: Cell) {
        let i = self.idx(row, col);
        self.cells[i] = cell;
    }

    pub fn get_mut(&mut self, row: usize, col: usize) -> &mut Cell {
        let i = self.idx(row, col);
        &mut self.cells[i]
    }

    pub fn row(&self, row: usize) -> &[Cell] {
        let start = row * self.cols;
        &self.cells[start..start + self.cols]
    }

    pub fn clear(&mut self, blank: Cell) {
        for c in &mut self.cells {
            *c = blank;
        }
        // ED 2 / full-reset path: an image-aware terminal usually exposes
        // explicit delete commands, but most TUI image users (icat / imgcat /
        // chafa) lean on screen-clear as the implicit reset. Dropping all
        // placements here matches what users observe in Kitty.
        self.placements.clear();
    }

    /// Fill cells `from..to` of `row` with `blank`.
    pub fn clear_row(&mut self, row: usize, from: usize, to: usize, blank: Cell) {
        let base = row * self.cols;
        for i in from..to.min(self.cols) {
            self.cells[base + i] = blank;
        }
    }

    /// Shift rows `[top..=bottom]` up by `n` within columns `[left..=right]`,
    /// filling freed cells with `blank`. Cells outside the column range are
    /// untouched — that's what makes tmux-style per-pane scrolling work.
    ///
    /// Returns placements that scrolled fully off the top of the grid as a
    /// side effect of the shift. The caller decides whether to route them
    /// into scrollback (only `Terminal::scroll_region_up_by` does, and only
    /// when the live grid is the full-screen primary).
    pub fn scroll_region_up(
        &mut self,
        top: usize,
        bottom: usize,
        left: usize,
        right: usize,
        n: usize,
        blank: Cell,
    ) -> Vec<Placement> {
        let region = bottom - top + 1;
        let n = n.min(region);
        if n == 0 || left > right || right >= self.cols {
            return Vec::new();
        }
        let width = right - left + 1;
        let full_width = left == 0 && right == self.cols - 1;
        // Copy only if there's something to shift. When n == region, every
        // row of the region gets cleared and nothing moves.
        if n < region {
            for r in top..=bottom - n {
                let src = (r + n) * self.cols + left;
                let dst = r * self.cols + left;
                if full_width {
                    self.cells.copy_within(src..src + self.cols, dst);
                } else {
                    self.cells.copy_within(src..src + width, dst);
                }
            }
        }
        for r in bottom + 1 - n..=bottom {
            if full_width {
                self.clear_row(r, 0, self.cols, blank);
            } else {
                self.clear_row(r, left, right + 1, blank);
            }
        }
        // Shift placements that intersect the scroll region. Partial-width
        // scrolls (DECSLRM) leave images alone — the use case (per-pane tmux
        // scrolling with images) is rare and the math for "shift only the
        // pixels inside the column range" doesn't fit a whole-cell anchor.
        if full_width {
            self.shift_placements_up_in(top, bottom, n)
        } else {
            Vec::new()
        }
    }

    /// Shift rows `[top..=bottom]` down by `n` within columns `[left..=right]`,
    /// filling freed cells at the top of the region with `blank`.
    pub fn scroll_region_down(
        &mut self,
        top: usize,
        bottom: usize,
        left: usize,
        right: usize,
        n: usize,
        blank: Cell,
    ) {
        let region = bottom - top + 1;
        let n = n.min(region);
        if n == 0 || left > right || right >= self.cols {
            return;
        }
        let width = right - left + 1;
        let full_width = left == 0 && right == self.cols - 1;
        if n < region {
            for r in (top + n..=bottom).rev() {
                let src = (r - n) * self.cols + left;
                let dst = r * self.cols + left;
                if full_width {
                    self.cells.copy_within(src..src + self.cols, dst);
                } else {
                    self.cells.copy_within(src..src + width, dst);
                }
            }
        }
        for r in top..top + n {
            if full_width {
                self.clear_row(r, 0, self.cols, blank);
            } else {
                self.clear_row(r, left, right + 1, blank);
            }
        }
        if full_width {
            self.shift_placements_down_in(top, bottom, n);
        }
    }

    /// Shift placements intersecting `[top, bottom]` up by `n` rows. Drops
    /// any that have fully scrolled off the top of the grid (`bottom_row <= 0`).
    /// Returns the dropped placements so the caller can route them into
    /// scrollback when appropriate (`Terminal::scroll_region_up_by` does).
    pub(crate) fn shift_placements_up_in(
        &mut self,
        top: usize,
        bottom: usize,
        n: usize,
    ) -> Vec<Placement> {
        let mut dropped = Vec::new();
        self.placements.retain_mut(|p| {
            if p.rows_intersect(top, bottom) {
                p.top_row -= n as isize;
                if p.fully_off_grid(self.rows, self.cols) {
                    dropped.push(p.clone());
                    return false;
                }
            }
            true
        });
        dropped
    }

    /// Mirror of `shift_placements_up_in` for down-scroll. Placements that
    /// fall off the bottom are dropped (not surfaced to the caller — there's
    /// no "scrollback below" to route them to).
    pub(crate) fn shift_placements_down_in(
        &mut self,
        top: usize,
        bottom: usize,
        n: usize,
    ) {
        self.placements.retain_mut(|p| {
            if p.rows_intersect(top, bottom) {
                p.top_row += n as isize;
                if p.fully_off_grid(self.rows, self.cols) {
                    return false;
                }
            }
            true
        });
    }
}

/// Logical cursor shape. The DEC blink/steady distinction collapses here —
/// we don't blink yet.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum CursorShape {
    Block,
    Underline,
    Bar,
}

/// Snapshot of which mouse-tracking modes the host has enabled. The front-end
/// uses this to decide whether to swallow mouse events for its own UI vs.
/// forward them to the PTY.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct MouseProtocol {
    pub press_release: bool, // ?1000
    pub button_motion: bool, // ?1002
    pub any_motion: bool,    // ?1003
    pub sgr: bool,           // ?1006
}

impl MouseProtocol {
    pub fn enabled(&self) -> bool {
        self.press_release || self.button_motion || self.any_motion
    }
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Cursor {
    pub row: usize,
    pub col: usize,
    pub style: Style,
    // DECAWM deferred-wrap: a print that lands in the last column sets this
    // to true without advancing the cursor; the wrap happens on the next
    // print (if autowrap is still on). CR / LF / cursor addressing clear it.
    pub wrap_pending: bool,
}

impl Cursor {
    pub fn new() -> Self {
        Self {
            row: 0,
            col: 0,
            style: Style::new(),
            wrap_pending: false,
        }
    }
}

pub struct Terminal {
    pub cols: usize,
    pub rows: usize,
    primary: Grid,
    alternate: Grid,
    use_alternate: bool,
    cursor: Cursor,
    saved_primary: Option<Cursor>,
    saved_alternate: Option<Cursor>,
    scroll_top: usize,    // inclusive, 0-based
    scroll_bottom: usize, // inclusive, 0-based
    // DECLRMM (`?69`). When false (default) the next two fields are forced
    // to full width and CSI `s` with no params is SCOSC save-cursor.
    lrmm_enabled: bool,
    scroll_left: usize,  // inclusive, 0-based
    scroll_right: usize, // inclusive, 0-based
    autowrap: bool,
    cursor_visible: bool,
    // DECCKM (private mode 1). When true, unmodified cursor keys send SS3
    // forms (\eOA…) instead of CSI (\e[A…). The encoder reads this via
    // `app_cursor_keys()`.
    app_cursor_keys: bool,
    // DECSCUSR raw value: 0/1 = blink block, 2 = block, 3/4 = underline,
    // 5/6 = bar. Renderer reads via `cursor_shape()`.
    cursor_style_dec: u16,
    // Mouse-tracking flags (DEC private modes ?1000/?1002/?1003/?1006).
    mouse_press_release: bool,
    mouse_button_motion: bool,
    mouse_any_motion: bool,
    mouse_sgr: bool,
    // Bracketed paste (?2004): when set, the GUI wraps pasted text in
    // ESC [ 200 ~ … ESC [ 201 ~ before sending it to the PTY.
    bracketed_paste: bool,
    // Theme-derived defaults the terminal reports back for OSC 10/11/12
    // queries. Set by the front end via `set_default_colors`.
    default_fg_rgb: [u8; 3],
    default_bg_rgb: [u8; 3],
    default_cursor_rgb: [u8; 3],
    // Bytes the host has asked us to send back (DSR replies, etc.). Caller
    // drains via `take_response()` after each `feed`.
    pending_response: Vec<u8>,
    parser: ansi::Parser,
    scrollback: VecDeque<Vec<Cell>>,
    scrollback_limit: usize,
    // Scrollback viewport offset: number of history lines currently shifted
    // into view at the top of the viewport. 0 == showing the live grid.
    view_offset: usize,
    // Placements that have scrolled fully off the top of the primary grid.
    // `scrollback_row` indexes into `scrollback` (0 = oldest). When the
    // scrollback evicts its front rows, those indices shift down and
    // placements with a now-negative index are dropped. Alternate-screen
    // placements never reach here; the alt buffer has no scrollback.
    scrollback_placements: VecDeque<ScrollbackPlacement>,
    // Monotonic id source for new placements. Slice 2 doesn't yet allocate
    // these (test code does); slice 4 hooks it up via the debug keybind.
    next_placement_id: PlacementId,
    // Config-driven: when false, placements that fully scroll off the top
    // are dropped rather than promoted to `scrollback_placements`. Saves
    // memory in long-running shells with heavy image traffic. The trade-
    // off — losing the image when the user scrolls history back — is
    // small in phase 1 since scrollback rendering of placements isn't
    // wired up yet.
    keep_placements_in_scrollback: bool,
    // Per-frame outbox: parsed but not yet decoded image payloads. The
    // main thread drains this via `take_pending_image_uploads` and fires
    // the async decode + GPU upload. Kept on Terminal (rather than
    // returned from `feed`) because a single PTY chunk can contain
    // multiple OSC 1337 sequences and we want them all visible in one
    // drain.
    pending_image_uploads: Vec<PendingImageUpload>,
    // Kitty graphics chunked transmissions in flight. Keyed by `i=`
    // image id; chunks with `m=1` append, chunk with `m=0` (or omitted)
    // completes the upload. Only the FIRST chunk's sizing / cursor
    // params are kept — that's what the Kitty spec says wins.
    kitty_chunks: std::collections::HashMap<u32, KittyChunks>,
    // Chunked transmission without an image_id. The Kitty spec says
    // chunked transmissions MUST use `i=`, but `kitty +kitten icat`
    // doesn't in practice — when sending raw RGB/JPG it omits the id
    // and expects the terminal to thread the chunks together as a
    // singular anonymous in-flight image. Only one can be in flight at
    // a time; a new `m=1` without an id while one's open silently
    // overwrites (matching the implicit "only one anonymous stream"
    // contract).
    kitty_chunks_anon: Option<KittyChunks>,
    // Tracks the `i=` of the most recently opened id-keyed chunked
    // transmission. icat puts `i=` on the first chunk and omits it
    // on every continuation — without this thread, continuation
    // chunks fall through to the anonymous bucket and the id-keyed
    // entry leaks. Set when an `m=1` chunk with `i=` arrives, cleared
    // when that transmission's final chunk (`m=0` / no `m=`) finalizes
    // or when a fresh id-keyed transmission preempts it.
    current_chunked_id: Option<u32>,
    // Tracks the `i=` of the most recently *completed* transmission
    // (single-chunk or chunked) so `a=p` / `a=d` / `a=f` / `a=a`
    // without an explicit `i=` can fall back per the Kitty spec
    // ("if i is missing, the most recently created image is
    // targeted"). icat's animation-frame stream emits `a=f` with no
    // `i=` and no `m=` between control messages; without this
    // fallback those frames silently drop. Updated whenever
    // `finalize_kitty_image_bytes` runs with a `kitty_image_id`.
    last_kitty_image_id: Option<u32>,
    // Kitty client image-id → our store ImageId. Populated when a
    // transmission carries `i=` so subsequent `a=p,i=N` (place by id)
    // and `a=d,d=i,i=N` (delete by id) can find the image. Stays
    // populated across a=t (transmit only) → a=p (place later)
    // round-trips, which is the whole point of the protocol's
    // image-id mechanism — clients re-display without re-uploading.
    //
    // `referenced_image_ids` includes the values from this map so
    // mark-and-sweep doesn't drop a transmitted-but-not-yet-placed
    // image between a=t and a=p.
    kitty_image_ids: std::collections::HashMap<u32, ImageId>,
    // Format the base image was transmitted with, keyed by the same
    // client id used in `kitty_image_ids`. Animation frames (`a=f`)
    // commonly omit `f=` and expect the base's format to apply (icat
    // sends e.g. `f=24` on the base, then a=f frames with no `f=` at
    // all — the spec says raw frame data inherits the base format).
    // Populated whenever a Kitty transmission with a client id
    // finalizes; cleared on `a=d` selectors that drop the image.
    kitty_image_formats: std::collections::HashMap<u32, KittyFormat>,
    // Image's total cell extent `(cols, rows)` from the original
    // `a=T` / `a=t` transmission's `c=` / `r=` parameters. Used by
    // the renderer's per-run draw path as the UV denominator: a
    // placeholder cell at `(image_row, image_col)` shows the sub-rect
    // `(image_col/cols, image_row/rows)` to `((image_col+1)/cols,
    // (image_row+1)/rows)` of the source texture. Without this,
    // partial overwrites of a placeholder grid would distort the
    // image (the surviving cells would stretch the whole image into
    // a shrinking bbox). Cleared on the same `a=d` selectors as
    // `kitty_image_ids` / `kitty_image_formats`.
    kitty_image_cell_extents: std::collections::HashMap<u32, (u32, u32)>,
    // Font metrics in framebuffer pixels. The OSC 1337 handler needs
    // these to translate pixel-spec sizing to cell extent. State pushes
    // them in via `set_cell_size_px` at construction and on every font-
    // size change. Defaults to 1×1 — any pre-setter OSC produces a tiny
    // placement rather than panicking.
    cell_w_px: u32,
    line_h_px: u32,
    /// In-flight Kitty Unicode-placeholder absorbtion state. The
    /// placeholder protocol writes `U+10EEEE` followed by up to three
    /// combining diacritics encoding (image_row, image_col,
    /// image_id_high_byte). The diacritics must attach to the
    /// preceding cell rather than landing in their own cells as
    /// glyph-less "tofu". This state points at the cell that received
    /// the most recent `U+10EEEE` and tracks which diacritic slot is
    /// next. Reset on the first non-diacritic `print()`.
    placeholder_decode: Option<PlaceholderDecode>,
}

#[derive(Copy, Clone, Debug)]
struct PlaceholderDecode {
    /// Grid coordinates of the U+10EEEE cell the upcoming diacritics
    /// attach to.
    cell_row: usize,
    cell_col: usize,
    /// 0 = next diacritic is the image row, 1 = column, 2 = image id
    /// high byte. After 3, the state clears.
    next_slot: u8,
}

#[derive(Clone, Debug, PartialEq)]
struct ScrollbackPlacement {
    /// Index into `Terminal::scrollback` of the placement's anchor row.
    /// 0 = oldest scrollback row. Decremented in lockstep with scrollback
    /// eviction; placements that would go negative are dropped.
    scrollback_row: isize,
    placement: Placement,
}

impl Terminal {
    pub fn new(cols: usize, rows: usize, scrollback_limit: usize) -> Self {
        assert!(cols > 0 && rows > 0, "terminal must be non-empty");
        let blank = Cell::new(' ', Style::new());
        Self {
            cols,
            rows,
            primary: Grid::new(rows, cols, blank),
            alternate: Grid::new(rows, cols, blank),
            use_alternate: false,
            cursor: Cursor::new(),
            saved_primary: None,
            saved_alternate: None,
            scroll_top: 0,
            scroll_bottom: rows - 1,
            lrmm_enabled: false,
            scroll_left: 0,
            scroll_right: cols - 1,
            autowrap: true,
            cursor_visible: true,
            app_cursor_keys: false,
            cursor_style_dec: 0,
            mouse_press_release: false,
            mouse_button_motion: false,
            mouse_any_motion: false,
            mouse_sgr: false,
            bracketed_paste: false,
            default_fg_rgb: [0xcc, 0xcc, 0xcc],
            default_bg_rgb: [0x00, 0x00, 0x00],
            default_cursor_rgb: [0xcc, 0xcc, 0xcc],
            pending_response: Vec::new(),
            parser: ansi::Parser::new(),
            scrollback: VecDeque::new(),
            scrollback_limit,
            view_offset: 0,
            scrollback_placements: VecDeque::new(),
            next_placement_id: 1,
            keep_placements_in_scrollback: true,
            pending_image_uploads: Vec::new(),
            kitty_chunks: std::collections::HashMap::new(),
            kitty_chunks_anon: None,
            current_chunked_id: None,
            last_kitty_image_id: None,
            kitty_image_ids: std::collections::HashMap::new(),
            kitty_image_formats: std::collections::HashMap::new(),
            kitty_image_cell_extents: std::collections::HashMap::new(),
            cell_w_px: 1,
            line_h_px: 1,
            placeholder_decode: None,
        }
    }

    /// Tell the terminal how big a cell is in framebuffer pixels. Drives
    /// the OSC-1337 sizing math (Pixels/Percent specs and Auto fallback).
    /// State calls this once at construction and again on every font-size
    /// change; the values are otherwise stable across the session.
    pub fn set_cell_size_px(&mut self, cell_w_px: u32, line_h_px: u32) {
        self.cell_w_px = cell_w_px.max(1);
        self.line_h_px = line_h_px.max(1);
    }

    /// Drain pending image payloads parsed since the last call. main.rs
    /// invokes this each frame to hand decode jobs to the worker.
    pub fn take_pending_image_uploads(&mut self) -> Vec<PendingImageUpload> {
        std::mem::take(&mut self.pending_image_uploads)
    }

    /// Toggle whether fully-scrolled-off placements survive in
    /// `scrollback_placements`. Setting this to false also clears the
    /// current scrollback-placement queue — otherwise existing entries
    /// would linger until the next eviction.
    pub fn set_keep_placements_in_scrollback(&mut self, keep: bool) {
        self.keep_placements_in_scrollback = keep;
        if !keep {
            self.scrollback_placements.clear();
        }
    }

    /// Allocate a fresh placement id and insert the placement into the active
    /// grid. Returns the id so callers (parsers, the debug keybind) can refer
    /// back to it for delete/move ops in later phases.
    pub fn insert_placement(
        &mut self,
        image: ImageId,
        top_row: isize,
        left_col: isize,
        rows: u16,
        cols: u16,
        z: i32,
    ) -> PlacementId {
        self.insert_placement_with_crop(
            image, top_row, left_col, rows, cols, z, (0, 0), None,
        )
    }

    /// Like `insert_placement` but with sub-cell pixel offsets and an
    /// optional source-crop rectangle. Kept as a separate entry point so
    /// phase 1 callers (iTerm2 OSC 1337 parser, debug keybinds) stay on
    /// the cell-grid-snapped signature; phase 2's Kitty graphics protocol
    /// — which supports `X=` / `Y=` pixel offsets and an explicit source
    /// rect — will reach for this one.
    #[allow(clippy::too_many_arguments)]
    pub fn insert_placement_with_crop(
        &mut self,
        image: ImageId,
        top_row: isize,
        left_col: isize,
        rows: u16,
        cols: u16,
        z: i32,
        pixel_offset: (i32, i32),
        src_rect: Option<(u32, u32, u32, u32)>,
    ) -> PlacementId {
        self.insert_placement_full(
            image, top_row, left_col, rows, cols, z, pixel_offset, src_rect,
            None, None,
        )
    }

    /// Insertion path for Kitty graphics-protocol placements. Threads
    /// the client's `i=` / `p=` IDs through so `a=d,d=i,...` and
    /// `a=d,d=p,...` can find the placement later.
    #[allow(clippy::too_many_arguments)]
    pub fn insert_placement_kitty(
        &mut self,
        image: ImageId,
        top_row: isize,
        left_col: isize,
        rows: u16,
        cols: u16,
        z: i32,
        pixel_offset: (i32, i32),
        src_rect: Option<(u32, u32, u32, u32)>,
        kitty_image_id: Option<u32>,
        kitty_placement_id: Option<u32>,
    ) -> PlacementId {
        self.insert_placement_full(
            image, top_row, left_col, rows, cols, z, pixel_offset, src_rect,
            kitty_image_id, kitty_placement_id,
        )
    }

    /// One shared implementation behind the three insertion entry points
    /// so all callers route through identical id allocation + placement
    /// construction. Kept private; callers pick one of the three public
    /// shapes based on which fields they care about.
    #[allow(clippy::too_many_arguments)]
    fn insert_placement_full(
        &mut self,
        image: ImageId,
        top_row: isize,
        left_col: isize,
        rows: u16,
        cols: u16,
        z: i32,
        pixel_offset: (i32, i32),
        src_rect: Option<(u32, u32, u32, u32)>,
        kitty_image_id: Option<u32>,
        kitty_placement_id: Option<u32>,
    ) -> PlacementId {
        let id = self.next_placement_id;
        self.next_placement_id = self.next_placement_id.wrapping_add(1).max(1);
        let placement = Placement {
            id, image, top_row, left_col, rows, cols, z,
            pixel_offset, src_rect,
            kitty_image_id, kitty_placement_id,
        };
        self.active_grid_mut().placements.push(placement);
        id
    }

    /// Register a Kitty client image-id → store ImageId mapping. Called
    /// by main.rs immediately after `Store::request_insert` returns,
    /// for any upload carrying a Kitty `i=` id. Subsequent
    /// `a=p,i=N` / `a=d,d=i,i=N` look the image up here.
    ///
    /// Idempotent — re-registering the same client id with a new
    /// store id overwrites (a client that re-transmits with the same
    /// `i=` expects the new image to replace the old).
    pub fn register_kitty_image_id(&mut self, client_id: u32, store_id: ImageId) {
        self.kitty_image_ids.insert(client_id, store_id);
    }

    /// Look up the store ImageId for a Kitty client `i=` id, if known.
    /// Used by the `a=p` placement path.
    pub fn kitty_image_id_lookup(&self, client_id: u32) -> Option<ImageId> {
        self.kitty_image_ids.get(&client_id).copied()
    }

    /// Total cell extent `(cols, rows)` from the original `a=T` / `a=t`
    /// transmission's `c=` / `r=`. The per-run placeholder renderer
    /// uses these as UV denominators. Returns `None` for ids whose
    /// transmission omitted one or both of `c=` / `r=` (the renderer
    /// must then skip the runs — without the denominator there's no
    /// honest UV).
    pub fn kitty_image_cell_extent(&self, client_id: u32) -> Option<(u32, u32)> {
        self.kitty_image_cell_extents.get(&client_id).copied()
    }

    /// Scan the active grid for Kitty virtual-placement cells
    /// (`U+10EEEE` with an encoded image id) and emit one run per
    /// contiguous horizontal stretch that the renderer can draw as a
    /// single quad with a correct UV sub-rect.
    ///
    /// A run extends a previous cell when ALL of these hold:
    ///   - same screen row
    ///   - same `client_id`
    ///   - same `image_row` diacritic value
    ///   - `image_col == prev.image_col + 1`
    ///
    /// Any other transition (non-placeholder cell, id change, row
    /// change, image_col gap) starts a new run.
    ///
    /// This lets the renderer draw each surviving stretch with its
    /// correct slice of the source image, so partial overwrites
    /// (e.g. tmux scrolling new output over the top rows of an
    /// image) visibly clip instead of distorting. The previous
    /// merged-bbox approach stretched whatever image we had into
    /// whatever bbox survived — fine for an undisturbed grid, bad
    /// for anything else.
    ///
    /// Grouping happens per screen row only — vertical run-length
    /// compression would require the renderer to know that adjacent
    /// rows belong to the same image, which the bbox path got wrong
    /// (the merge swallowed valid sub-rect boundaries). Per-row is
    /// the smallest useful unit: a 29×15 image collapses to 15
    /// quads per frame, well under the renderer's per-call cost.
    pub fn kitty_placeholder_runs(&self) -> Vec<KittyPlaceholderRun> {
        let mut runs: Vec<KittyPlaceholderRun> = Vec::new();
        let grid = self.active_grid();
        for r in 0..grid.rows {
            let mut current: Option<KittyPlaceholderRun> = None;
            for c in 0..grid.cols {
                let cell = grid.get(r, c);
                let Some(id) = cell.placeholder_image_id else {
                    if let Some(run) = current.take() {
                        runs.push(run);
                    }
                    continue;
                };
                let image_row = cell.placeholder_image_row;
                let image_col = cell.placeholder_image_col;
                match current.as_mut() {
                    Some(open)
                        if open.client_id == id
                            && open.image_row == image_row
                            && open.image_col_end == image_col
                            && open.screen_col_end == c =>
                    {
                        // Extend.
                        open.screen_col_end = c + 1;
                        open.image_col_end = image_col.saturating_add(1);
                    }
                    _ => {
                        if let Some(run) = current.take() {
                            runs.push(run);
                        }
                        current = Some(KittyPlaceholderRun {
                            client_id: id,
                            screen_row: r,
                            screen_col_start: c,
                            screen_col_end: c + 1,
                            image_row,
                            image_col_start: image_col,
                            image_col_end: image_col.saturating_add(1),
                        });
                    }
                }
            }
            if let Some(run) = current.take() {
                runs.push(run);
            }
        }
        runs
    }

    /// Remove every placement that references `image_id`, across both
    /// grids and scrollback. Returns the number removed. Used by main.rs
    /// on decode failure: the placement was inserted optimistically at
    /// OSC time (so the cursor could advance synchronously), and we
    /// need to clean it up when the worker reports the bytes were
    /// undecodable / oversized / over-budget.
    pub fn remove_placements_with_image(&mut self, image_id: ImageId) -> usize {
        let mut removed = 0;
        self.primary.placements.retain(|p| {
            let drop = p.image == image_id;
            if drop { removed += 1; }
            !drop
        });
        self.alternate.placements.retain(|p| {
            let drop = p.image == image_id;
            if drop { removed += 1; }
            !drop
        });
        self.scrollback_placements.retain(|sp| {
            let drop = sp.placement.image == image_id;
            if drop { removed += 1; }
            !drop
        });
        removed
    }

    /// Snapshot of placements anchored to the *live* grid (active screen).
    /// The renderer uses this each frame to build draw rectangles. Does NOT
    /// include placements that have scrolled into scrollback — for those use
    /// `scrollback_placements_in_view`, which maps a scrollback-row anchor
    /// onto a viewport row based on the current view_offset.
    pub fn live_placements(&self) -> &[Placement] {
        &self.active_grid().placements
    }

    /// Scrollback placements that intersect the visible scrollback strip,
    /// rebased into viewport coordinates. Returned `Placement`s are clones
    /// of the originals with `top_row` adjusted so the renderer can apply
    /// the same `top_row * line_height + decorator_offset + scroll_y` math
    /// it already uses for `live_placements()`. `top_row` may be negative
    /// when the placement straddles the top of the viewport — that's fine,
    /// the camera ortho clips it.
    ///
    /// Returns an empty Vec on the alt screen (scrollback is primary-only)
    /// or when `view_offset == 0` (no scrollback visible). Intersection is
    /// against `[0, viewport_rows)`; a placement entirely above the visible
    /// scrollback strip or entirely below the live grid is filtered out.
    pub fn scrollback_placements_in_view(
        &self,
        viewport_rows: usize,
    ) -> Vec<Placement> {
        if self.use_alternate || self.view_offset == 0 {
            return Vec::new();
        }
        let sb_len = self.scrollback.len() as isize;
        let view_off = self.view_offset as isize;
        // Scrollback row sb_r appears at viewport row sb_r - (sb_len - view_off).
        // Equivalently: shift = view_off - sb_len; viewport_row = sb_r + shift.
        let shift = view_off - sb_len;
        let vp_rows = viewport_rows as isize;
        // 2-row slack on each side matches `update_vertices`'s `r_lo = -2`
        // / `r_hi = rows + 2` window. Smooth-scroll's `scroll_y` shifts
        // content by up to ±line_height between discrete view_offset ticks,
        // and the decorator-offset push at the boundaries adds another
        // partial-row. Without the slack, a placement whose bottom edge
        // is just peeking in from the top during smooth-scroll gets
        // filtered out — the image then "snaps" into view a frame later
        // when view_offset increments.
        const ROW_SLACK: isize = 2;
        let mut out = Vec::new();
        for sp in &self.scrollback_placements {
            let top = sp.scrollback_row + shift;
            let bottom = top + sp.placement.rows as isize;
            if bottom <= -ROW_SLACK || top >= vp_rows + ROW_SLACK {
                continue;
            }
            let mut p = sp.placement.clone();
            p.top_row = top;
            out.push(p);
        }
        out
    }

    /// Union of ImageIds referenced by every live + scrollback placement,
    /// across both primary and alternate grids. Fed to `images::Store::retain`
    /// each frame so the store drops images whose last placement just went
    /// away (mark-and-sweep refcounting). One-frame deferred drop is
    /// invisible at 60Hz and keeps `terminal.rs` free of GPU types.
    pub fn referenced_image_ids(&self) -> std::collections::HashSet<ImageId> {
        let mut out = std::collections::HashSet::new();
        for p in &self.primary.placements {
            out.insert(p.image);
        }
        for p in &self.alternate.placements {
            out.insert(p.image);
        }
        for sp in &self.scrollback_placements {
            out.insert(sp.placement.image);
        }
        // Kitty `a=t` (transmit without display) lands here. The image
        // has no placement yet — the client will send `a=p,i=N` later
        // to display it. Without this branch, mark-and-sweep would drop
        // the image between the two ops.
        for &iid in self.kitty_image_ids.values() {
            out.insert(iid);
        }
        out
    }

    pub fn feed(&mut self, s: &str) {
        let mut events = Vec::new();
        for ch in s.chars() {
            self.parser.feed(ch, |e| events.push(e));
        }
        for e in events {
            self.dispatch(e);
        }
    }

    pub fn cursor(&self) -> Cursor {
        self.cursor
    }

    pub fn cursor_visible(&self) -> bool {
        self.cursor_visible
    }

    pub fn app_cursor_keys(&self) -> bool {
        self.app_cursor_keys
    }

    pub fn cursor_shape(&self) -> CursorShape {
        match self.cursor_style_dec {
            3 | 4 => CursorShape::Underline,
            5 | 6 => CursorShape::Bar,
            _ => CursorShape::Block,
        }
    }

    /// DECSCUSR distinguishes blinking (0/1/3/5) from steady (2/4/6) variants.
    /// Default (value 0) is blink-block, matching xterm.
    pub fn cursor_blink(&self) -> bool {
        matches!(self.cursor_style_dec, 0 | 1 | 3 | 5)
    }

    pub fn bracketed_paste(&self) -> bool {
        self.bracketed_paste
    }

    /// Mouse-protocol state, in a form the front-end can consume directly.
    pub fn mouse_protocol(&self) -> MouseProtocol {
        MouseProtocol {
            press_release: self.mouse_press_release,
            button_motion: self.mouse_button_motion,
            any_motion: self.mouse_any_motion,
            sgr: self.mouse_sgr,
        }
    }

    /// Update the colors the terminal reports back for OSC 10/11/12 queries.
    /// Front-end should call this on theme change.
    pub fn set_default_colors(&mut self, fg: [u8; 3], bg: [u8; 3], cursor: [u8; 3]) {
        self.default_fg_rgb = fg;
        self.default_bg_rgb = bg;
        self.default_cursor_rgb = cursor;
    }

    /// Walk every cell on every grid (primary, alternate, scrollback)
    /// and re-resolve any `ColorSource::Indexed` foreground/background
    /// against the currently-installed palette. Truecolor and Default
    /// cells are untouched.
    ///
    /// Called after `palette::install` runs from the Cmd-Shift-R
    /// reload path. Without this sweep, already-painted cells keep
    /// the RGB values they were resolved to under the OLD palette;
    /// only cells that get re-printed afterward pick up the new
    /// scheme.
    pub fn reresolve_palette(&mut self) {
        for cell in self.primary.cells.iter_mut() {
            cell.style.reresolve_palette();
        }
        for cell in self.alternate.cells.iter_mut() {
            cell.style.reresolve_palette();
        }
        for row in self.scrollback.iter_mut() {
            for cell in row.iter_mut() {
                cell.style.reresolve_palette();
            }
        }
    }

    /// Drain any bytes the emulator wants written back to the host. Returns
    /// an empty vec when there's nothing pending.
    pub fn take_response(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending_response)
    }

    #[allow(dead_code)]
    pub fn row(&self, row: usize) -> &[Cell] {
        self.active_grid().row(row)
    }

    pub fn scrollback_len(&self) -> usize {
        self.scrollback.len()
    }

    pub fn view_offset(&self) -> usize {
        self.view_offset
    }

    pub fn at_top(&self) -> bool {
        self.view_offset >= self.scrollback.len()
    }

    pub fn at_bottom(&self) -> bool {
        self.view_offset == 0
    }

    /// True while a full-screen app (vim, less, htop) is on the alternate
    /// screen. Callers use this to suppress scrollback-targeted gestures
    /// like the mouse wheel, since the alt screen has no scroll history.
    pub fn on_alt_screen(&self) -> bool {
        self.use_alternate
    }

    /// Shift the viewport up by `n` lines, pulling older scrollback into view.
    /// No-op (returns false) on the alternate screen or if already at the top.
    pub fn scroll_up(&mut self, n: usize) -> bool {
        if self.use_alternate {
            return false;
        }
        let new = (self.view_offset + n).min(self.scrollback.len());
        if new == self.view_offset {
            return false;
        }
        self.view_offset = new;
        true
    }

    /// Shift the viewport down by `n` lines, toward the live grid.
    pub fn scroll_down(&mut self, n: usize) -> bool {
        if self.use_alternate || self.view_offset == 0 {
            return false;
        }
        self.view_offset = self.view_offset.saturating_sub(n);
        true
    }

    pub fn scroll_to_bottom(&mut self) {
        self.view_offset = 0;
    }

    /// Look up the cell visible at `(visual_row, col)`, accounting for any
    /// active scrollback offset. When the viewport is on the live grid (or
    /// on the alt screen), this is just the grid cell.
    pub fn visible_cell(&self, visual_row: usize, col: usize) -> Cell {
        if self.use_alternate || self.view_offset == 0 {
            return self.active_grid().get(visual_row, col);
        }
        let scrollback_visible = self.view_offset.min(self.rows);
        if visual_row < scrollback_visible {
            let sb_idx = self.scrollback.len() - self.view_offset + visual_row;
            self.scrollback[sb_idx]
                .get(col)
                .copied()
                .unwrap_or(Cell::new(' ', Style::new()))
        } else {
            self.primary.get(visual_row - scrollback_visible, col)
        }
    }

    /// Convert a visual row (relative to the current viewport) to an absolute
    /// line index that's stable across scrolling: 0..scrollback_len indexes
    /// scrollback (oldest first); scrollback_len + r indexes grid row r.
    /// On the alt screen there's no scrollback, so visual_row maps directly.
    pub fn visual_to_abs_line(&self, visual_row: isize) -> isize {
        if self.use_alternate {
            return visual_row;
        }
        self.scrollback.len() as isize - self.view_offset as isize + visual_row
    }

    /// Borrow a row's cells by absolute-line index. Returns `None` for indices
    /// outside the buffer (scrolled-off rows, or below the live grid).
    pub fn line_at(&self, abs_line: isize) -> Option<&[Cell]> {
        if abs_line < 0 {
            return None;
        }
        let abs = abs_line as usize;
        if !self.use_alternate && abs < self.scrollback.len() {
            return Some(&self.scrollback[abs]);
        }
        let base = if self.use_alternate {
            0
        } else {
            self.scrollback.len()
        };
        if abs < base {
            return None;
        }
        let row = abs - base;
        if row < self.rows {
            Some(self.active_grid().row(row))
        } else {
            None
        }
    }

    /// Cell at any signed `visual_row`, including phantom rows above (-1, -2, …)
    /// or below (`rows`, `rows + 1`, …) the visible viewport. Returns `None`
    /// when no content is available there: alt-screen out-of-bounds, scrollback
    /// exhausted above, or beyond the live grid below. Used by the renderer to
    /// draw extra phantom rows for smooth-scroll continuity — emitting two on
    /// each side keeps the area covered through the entire sub-line slide so
    /// rows don't pop into / out of existence at snap boundaries.
    pub fn extended_cell(&self, visual_row: isize, col: usize) -> Option<Cell> {
        if self.use_alternate {
            if visual_row < 0 || visual_row as usize >= self.rows {
                return None;
            }
            return Some(self.alternate.get(visual_row as usize, col));
        }
        let sb_target = self.scrollback.len() as isize - self.view_offset as isize + visual_row;
        if sb_target < 0 {
            return None;
        }
        let sb_target = sb_target as usize;
        if sb_target < self.scrollback.len() {
            return self.scrollback[sb_target].get(col).copied();
        }
        let primary_row = sb_target - self.scrollback.len();
        if primary_row >= self.rows {
            return None;
        }
        Some(self.primary.get(primary_row, col))
    }

    /// Visual row of the live cursor. Includes rows up to `rows + 1` — i.e.
    /// the renderer's phantom-row range below the viewport — so the cursor
    /// stays drawn whenever any pixel of its row could be visible (smooth
    /// sub-line scroll, decorator-offset push at the bottom edge, window
    /// padding slack). Returns `None` only when the cursor is beyond that,
    /// truly off-screen.
    pub fn cursor_visual_row(&self) -> Option<usize> {
        if self.use_alternate {
            return Some(self.cursor.row);
        }
        let scrollback_visible = self.view_offset.min(self.rows);
        let visual = self.cursor.row + scrollback_visible;
        if visual <= self.rows + 1 {
            Some(visual)
        } else {
            None
        }
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        if cols == 0 || rows == 0 || (cols == self.cols && rows == self.rows) {
            return;
        }
        let blank = Cell::new(' ', self.cursor.style);
        let old_rows = self.rows;

        // Vertical reflow on the primary screen so the bottom of the grid
        // (where the prompt almost always lives) survives shrink/grow
        // cycles. Shrink spills the top rows into scrollback; grow pulls
        // them back so previously-hidden content re-uncovers. Both anchor
        // the cursor to the rows it moved with. Alt screen has no
        // scrollback — fall through to the old clamp-only behavior.
        let (spill, refill) = if self.use_alternate {
            (0, 0)
        } else if rows < old_rows {
            (old_rows - rows, 0)
        } else if rows > old_rows {
            let extra = rows - old_rows;
            (0, extra.min(self.scrollback.len()))
        } else {
            (0, 0)
        };

        if spill > 0 && self.scrollback_limit > 0 {
            for r in 0..spill {
                let line = self.primary.row(r).to_vec();
                if self.scrollback.len() == self.scrollback_limit {
                    self.scrollback.pop_front();
                    self.evict_scrollback_placement_front();
                }
                self.scrollback.push_back(line);
            }
            // Match scroll_region_up_by: keep the user's view of historical
            // content stable while new lines stream into scrollback.
            if self.view_offset > 0 {
                self.view_offset = (self.view_offset + spill).min(self.scrollback.len());
            }
        }

        // Migrate primary placements through the spill: those anchored in
        // spilled rows promote to scrollback_placements; those below shift
        // up so they stay over the same logical row. Mirrors what
        // scroll_region_up_by does for a stream of LF events.
        let mut placements = std::mem::take(&mut self.primary.placements);
        if spill > 0 && self.scrollback_limit > 0 {
            let sb_len_after_spill = self.scrollback.len() as isize;
            let mut keep: Vec<Placement> = Vec::with_capacity(placements.len());
            for mut p in placements.drain(..) {
                // Already straddling above the viewport pre-resize — leave it
                // as-is; subsequent scroll_region_up_by calls will handle.
                if p.top_row < 0 {
                    keep.push(p);
                    continue;
                }
                let original_top = p.top_row;
                if (original_top as usize) < spill {
                    // Anchored in a spilled row. Its scrollback index is the
                    // pre-spill index of that row in scrollback (which is
                    // `(sb_len_after - spill) + original_top`). If negative
                    // (extreme case where spill exceeded scrollback limit and
                    // this row was itself evicted), drop the placement.
                    // Also dropped wholesale when retention is disabled.
                    if self.keep_placements_in_scrollback {
                        let scrollback_row = sb_len_after_spill - spill as isize + original_top;
                        if scrollback_row >= 0 {
                            self.scrollback_placements.push_back(ScrollbackPlacement {
                                scrollback_row,
                                placement: p,
                            });
                        }
                    }
                } else {
                    p.top_row -= spill as isize;
                    keep.push(p);
                }
            }
            placements = keep;
        }

        let mut new_primary = Grid::new(rows, cols, blank);
        let mut dst_row = 0usize;
        if refill > 0 {
            let take_from = self.scrollback.len() - refill;
            let pulled: Vec<Vec<Cell>> = self.scrollback.drain(take_from..).collect();
            for src in pulled {
                let width = cols.min(src.len());
                for c in 0..width {
                    new_primary.set(dst_row, c, src[c]);
                }
                dst_row += 1;
            }
            // We just removed `refill` rows from the tail of scrollback. Any
            // view_offset pointing into that tail is now stale; clamp it.
            self.view_offset = self.view_offset.min(self.scrollback.len());

            // Migrate placements through the refill: live placements shift
            // down to make room for the refilled rows; scrollback placements
            // in the drained tail promote to live.
            let sb_post = self.scrollback.len() as isize;
            for p in &mut placements {
                p.top_row += refill as isize;
            }
            let mut i = 0;
            while i < self.scrollback_placements.len() {
                if self.scrollback_placements[i].scrollback_row >= sb_post {
                    let sp = self.scrollback_placements.remove(i).unwrap();
                    let mut p = sp.placement;
                    p.top_row = sp.scrollback_row - sb_post;
                    placements.push(p);
                } else {
                    i += 1;
                }
            }
        }
        let src_start = spill;
        for old_r in src_start..old_rows {
            if dst_row >= rows {
                break;
            }
            let width = cols.min(self.cols);
            for c in 0..width {
                new_primary.set(dst_row, c, self.primary.get(old_r, c));
            }
            dst_row += 1;
        }
        // Final pass: clamp placements to the new grid. Anything fully
        // off-screen (e.g. anchored to columns past a horizontal shrink, or
        // anchored to rows the grow couldn't refill) is dropped now so the
        // live list stays tight. Partially off-screen placements keep their
        // extent and rely on the renderer's clipping.
        placements.retain(|p| !p.fully_off_grid(rows, cols));
        new_primary.placements = placements;

        self.primary = new_primary;
        // Alternate is wiped wholesale on resize (matches the existing cell
        // behavior — alt apps redraw on SIGWINCH), so its placements go too.
        self.alternate = Grid::new(rows, cols, blank);

        // Cursor follows the rows it sat with: spill moves the top down
        // into the grid (cursor steps up by `spill`); refill puts new rows
        // above (cursor steps down by `refill`). Clamp to grid bounds.
        if self.use_alternate {
            self.cursor.row = self.cursor.row.min(rows - 1);
        } else {
            let r = self.cursor.row as isize - spill as isize + refill as isize;
            self.cursor.row = r.clamp(0, rows as isize - 1) as usize;
        }
        self.cursor.col = self.cursor.col.min(cols - 1);
        self.cursor.wrap_pending = false;
        self.cols = cols;
        self.rows = rows;
        self.scroll_top = 0;
        self.scroll_bottom = rows - 1;
        self.scroll_left = 0;
        self.scroll_right = cols - 1;
        self.lrmm_enabled = false;
        self.view_offset = self.view_offset.min(self.scrollback.len());
    }

    fn active_grid(&self) -> &Grid {
        if self.use_alternate {
            &self.alternate
        } else {
            &self.primary
        }
    }

    fn active_grid_mut(&mut self) -> &mut Grid {
        if self.use_alternate {
            &mut self.alternate
        } else {
            &mut self.primary
        }
    }

    fn blank(&self) -> Cell {
        Cell::new(' ', self.cursor.style)
    }

    fn dispatch(&mut self, event: Event) {
        match event {
            Event::Print(ch) => self.print(ch),
            Event::Bell => {}
            Event::Backspace => self.backspace(),
            Event::Tab => self.tab(),
            Event::LineFeed => self.line_feed(),
            Event::CarriageReturn => {
                self.cursor.col = 0;
                self.cursor.wrap_pending = false;
            }
            Event::CursorUp(n) => self.move_cursor(-(n as isize), 0),
            Event::CursorDown(n) => self.move_cursor(n as isize, 0),
            Event::CursorForward(n) => self.move_cursor(0, n as isize),
            Event::CursorBack(n) => self.move_cursor(0, -(n as isize)),
            Event::CursorPosition(row, col) => self.set_cursor_1_based(row, col),
            Event::CursorHorizontalAbs(col) => {
                self.cursor.col = ((col as usize).max(1) - 1).min(self.cols - 1);
                self.cursor.wrap_pending = false;
            }
            Event::CursorVerticalAbs(row) => {
                self.cursor.row = ((row as usize).max(1) - 1).min(self.rows - 1);
                self.cursor.wrap_pending = false;
            }
            Event::EraseInDisplay(mode) => self.erase_in_display(mode),
            Event::EraseInLine(mode) => self.erase_in_line(mode),
            Event::ScrollUp(n) => self.scroll_region_up_by(n as usize),
            Event::ScrollDown(n) => {
                let blank = self.blank();
                let top = self.scroll_top;
                let bottom = self.scroll_bottom;
                let left = self.scroll_left;
                let right = self.scroll_right;
                self.active_grid_mut()
                    .scroll_region_down(top, bottom, left, right, n as usize, blank);
            }
            Event::SetScrollRegion(top, bottom) => self.set_scroll_region(top, bottom),
            Event::SetLeftRightMargin(l, r) => self.set_left_right_margin(l, r),
            Event::InsertLine(n) => self.insert_lines(n as usize),
            Event::DeleteLine(n) => self.delete_lines(n as usize),
            Event::InsertChar(n) => self.insert_chars(n as usize),
            Event::DeleteChar(n) => self.delete_chars(n as usize),
            Event::EraseChar(n) => self.erase_chars(n as usize),
            Event::DeviceStatusReport(code) => self.device_status_report(code),
            Event::DeviceAttributes => self.reply(b"\x1b[?1;2c"),
            // VT220 ID, firmware version 276, ROM cartridge 0 — what xterm
            // sends. Apps just check that the reply is well-formed.
            Event::SecondaryDeviceAttributes => self.reply(b"\x1b[>0;276;0c"),
            Event::SetCursorStyle(n) => self.cursor_style_dec = n,
            Event::Osc(s) => self.handle_osc(&s),
            Event::Dcs(s) => self.handle_dcs(&s),
            Event::Apc(s) => self.handle_apc(&s),
            Event::XtwinopsQuery(ps) => self.handle_xtwinops_query(ps),
            Event::Sgr(params) => self.cursor.style.apply_sgr(&params),
            Event::PrivateModeSet(n) => self.private_mode(n, true),
            Event::PrivateModeReset(n) => self.private_mode(n, false),
            Event::SaveCursor => self.save_cursor(),
            Event::RestoreCursor => self.restore_cursor(),
            Event::FullReset => self.full_reset(),
        }
    }

    fn print(&mut self, ch: char) {
        // Kitty Unicode-placeholder absorption: when the most recent
        // print was U+10EEEE, the next up-to-three diacritics encode
        // position (row, col, id-high-byte) and must attach to that
        // cell instead of spawning their own. Without this, every
        // diacritic lands in its own cell with no glyph — the
        // user-visible "bunch of characters without glyphs" symptom
        // under tmux, where kitten icat falls back to the placeholder
        // protocol.
        if let Some(state) = self.placeholder_decode {
            if let Some(index) = kitty_placeholder_diacritic_index(ch) {
                self.apply_placeholder_diacritic(state, index);
                return;
            }
        }
        self.placeholder_decode = None;

        // With DECLRMM enabled, autowrap pivots on the right margin instead
        // of the screen edge, and wraps back to the left margin. We detect
        // the "inside LRM" case so that cursor positions sitting *outside*
        // the margins (apps can CUP anywhere) still wrap at the screen edge
        // the way xterm does.
        let inside_lrm =
            self.cursor.col >= self.scroll_left && self.cursor.col <= self.scroll_right;
        let wrap_at = if inside_lrm {
            self.scroll_right
        } else {
            self.cols - 1
        };
        let wrap_to = if inside_lrm { self.scroll_left } else { 0 };

        if self.cursor.wrap_pending && self.autowrap {
            self.cursor.col = wrap_to;
            self.cursor.wrap_pending = false;
            self.line_feed_no_cr();
        }
        let mut cell = Cell::new(ch, self.cursor.style);
        // Kitty virtual placement: U+10EEEE is the placeholder
        // codepoint. The image id is encoded in the cell's foreground
        // color (24-bit truecolor); the renderer scans for these
        // cells and reconstructs the per-image bounding box.
        if ch == '\u{10EEEE}' {
            cell.placeholder_image_id = decode_kitty_placeholder_image_id(&cell.style);
        }
        let row = self.cursor.row;
        let col = self.cursor.col;
        if row < self.rows && col < self.cols {
            self.active_grid_mut().set(row, col, cell);
            if ch == '\u{10EEEE}' && cell.placeholder_image_id.is_some() {
                self.placeholder_decode = Some(PlaceholderDecode {
                    cell_row: row,
                    cell_col: col,
                    next_slot: 0,
                });
            }
        }
        if self.cursor.col >= wrap_at {
            if self.autowrap {
                self.cursor.wrap_pending = true;
            }
            // when autowrap is off, cursor sticks at the wrap column
        } else {
            self.cursor.col += 1;
        }
    }

    /// Apply one decoded Kitty placeholder diacritic to the cell that
    /// the most recent `U+10EEEE` print landed in. `slot` advances
    /// row → col → id-high-byte; after the third diacritic the
    /// absorption state clears (a fourth diacritic in a row will fall
    /// through to the normal print path and behave like any other
    /// combining mark).
    fn apply_placeholder_diacritic(&mut self, mut state: PlaceholderDecode, index: u32) {
        if state.cell_row < self.rows && state.cell_col < self.cols {
            let cell = self.active_grid_mut().get_mut(state.cell_row, state.cell_col);
            match state.next_slot {
                0 => cell.placeholder_image_row = index.min(u16::MAX as u32) as u16,
                1 => cell.placeholder_image_col = index.min(u16::MAX as u32) as u16,
                2 => {
                    if let Some(id) = cell.placeholder_image_id {
                        let high = (index & 0xFF) << 24;
                        cell.placeholder_image_id = Some((id & 0x00FF_FFFF) | high);
                    }
                }
                _ => {}
            }
        }
        state.next_slot = state.next_slot.saturating_add(1);
        self.placeholder_decode = if state.next_slot < 3 { Some(state) } else { None };
    }

    fn backspace(&mut self) {
        if self.cursor.col > 0 {
            self.cursor.col -= 1;
        }
        self.cursor.wrap_pending = false;
    }

    fn tab(&mut self) {
        let next = ((self.cursor.col / 8) + 1) * 8;
        self.cursor.col = next.min(self.cols - 1);
        self.cursor.wrap_pending = false;
    }

    fn line_feed(&mut self) {
        self.cursor.wrap_pending = false;
        self.line_feed_no_cr();
    }

    // Move cursor down one row. Scrolls the region iff cursor is at
    // scroll_bottom; otherwise just advances (clamped to rows-1).
    fn line_feed_no_cr(&mut self) {
        if self.cursor.row == self.scroll_bottom {
            self.scroll_region_up_by(1);
        } else if self.cursor.row + 1 < self.rows {
            self.cursor.row += 1;
        }
    }

    fn move_cursor(&mut self, drow: isize, dcol: isize) {
        self.cursor.wrap_pending = false;
        let new_row = (self.cursor.row as isize + drow).clamp(0, self.rows as isize - 1);
        let new_col = (self.cursor.col as isize + dcol).clamp(0, self.cols as isize - 1);
        self.cursor.row = new_row as usize;
        self.cursor.col = new_col as usize;
    }

    fn set_cursor_1_based(&mut self, row: u16, col: u16) {
        self.cursor.row = ((row as usize).max(1) - 1).min(self.rows - 1);
        self.cursor.col = ((col as usize).max(1) - 1).min(self.cols - 1);
        self.cursor.wrap_pending = false;
    }

    fn erase_in_display(&mut self, mode: u16) {
        let blank = self.blank();
        let row = self.cursor.row;
        let col = self.cursor.col;
        match mode {
            0 => {
                let grid = self.active_grid_mut();
                grid.clear_row(row, col, grid.cols, blank);
                for r in (row + 1)..grid.rows {
                    grid.clear_row(r, 0, grid.cols, blank);
                }
            }
            1 => {
                let grid = self.active_grid_mut();
                for r in 0..row {
                    grid.clear_row(r, 0, grid.cols, blank);
                }
                grid.clear_row(row, 0, col + 1, blank);
            }
            2 => self.active_grid_mut().clear(blank),
            // ED 3 — xterm "Erase Saved Lines": drop the scrollback buffer
            // (and snap the viewport back to the live grid) without touching
            // on-screen content. Used by `clear -x` / `tput E3`.
            3 => {
                self.scrollback.clear();
                self.scrollback_placements.clear();
                self.view_offset = 0;
            }
            _ => {}
        }
    }

    fn erase_in_line(&mut self, mode: u16) {
        let blank = self.blank();
        let row = self.cursor.row;
        let col = self.cursor.col;
        let grid = self.active_grid_mut();
        match mode {
            0 => grid.clear_row(row, col, grid.cols, blank),
            1 => grid.clear_row(row, 0, col + 1, blank),
            2 => grid.clear_row(row, 0, grid.cols, blank),
            _ => {}
        }
    }

    fn scroll_region_up_by(&mut self, n: usize) {
        // Lines rolling off the top only become scrollback when the *whole*
        // grid is the scroll region — partial regions (DECSTBM or DECSLRM
        // narrower than the screen) just shift in place.
        let full_region = self.scroll_top == 0
            && self.scroll_bottom == self.rows - 1
            && self.scroll_left == 0
            && self.scroll_right == self.cols - 1;
        if !self.use_alternate && full_region && self.scrollback_limit > 0 {
            for _ in 0..n.min(self.rows) {
                let line = self.primary.row(self.scroll_top).to_vec();
                if self.scrollback.len() == self.scrollback_limit {
                    self.scrollback.pop_front();
                    self.evict_scrollback_placement_front();
                }
                self.scrollback.push_back(line);
                // Keep the user's view of historical content stable while
                // new lines stream into scrollback. visible_cell indexes from
                // the end of scrollback, so without this bump every appended
                // line would shift the viewport down by one row.
                if self.view_offset > 0 {
                    self.view_offset = (self.view_offset + 1).min(self.scrollback.len());
                }
            }
        }
        let blank = self.blank();
        let top = self.scroll_top;
        let bottom = self.scroll_bottom;
        let left = self.scroll_left;
        let right = self.scroll_right;
        let dropped = self
            .active_grid_mut()
            .scroll_region_up(top, bottom, left, right, n, blank);
        // Promote scrolled-off placements into scrollback when the live grid
        // is the full-screen primary. Their anchor row in scrollback is
        // (current scrollback length - rows above the *original* viewport top),
        // which the grid already shifted to negative for us — recover the row
        // by anchoring at "current scrollback end + the placement's now-negative
        // top_row" (top_row==-1 means the row that was just pushed at the back).
        if !self.use_alternate && full_region && self.keep_placements_in_scrollback {
            let sb_len = self.scrollback.len() as isize;
            for p in dropped {
                let scrollback_row = sb_len + p.top_row;
                if scrollback_row >= 0 {
                    self.scrollback_placements.push_back(ScrollbackPlacement {
                        scrollback_row,
                        placement: p,
                    });
                }
            }
        }
        // If retention is off, `dropped` is simply discarded — same effect
        // as letting the placement fall off the bottom of `scroll_region_down`.
    }

    /// Decrement scrollback-placement indices because scrollback popped one
    /// row off the front; drop any whose anchor row was the evicted one.
    ///
    /// Phase 1 doesn't render scrollback placements so a strict
    /// drop-on-top-row-eviction rule is sufficient. A future slice that adds
    /// "image partially visible at scrollback top edge" rendering can relax
    /// this to drop only after the placement's bottom row scrolls off.
    fn evict_scrollback_placement_front(&mut self) {
        self.scrollback_placements.retain(|sp| sp.scrollback_row > 0);
        for sp in &mut self.scrollback_placements {
            sp.scrollback_row -= 1;
        }
    }

    #[cfg(test)]
    pub(crate) fn scrollback_placements_for_test(&self) -> Vec<(isize, &Placement)> {
        self.scrollback_placements
            .iter()
            .map(|sp| (sp.scrollback_row, &sp.placement))
            .collect()
    }

    // IL: blank lines pushed in at the cursor row; rows below shift down and
    // anything past scroll_bottom is lost. No-op outside the scroll region —
    // including outside the LRM column range when DECLRMM is on.
    fn insert_lines(&mut self, n: usize) {
        if self.cursor.row < self.scroll_top
            || self.cursor.row > self.scroll_bottom
            || self.cursor.col < self.scroll_left
            || self.cursor.col > self.scroll_right
        {
            return;
        }
        let blank = self.blank();
        let top = self.cursor.row;
        let bottom = self.scroll_bottom;
        let left = self.scroll_left;
        let right = self.scroll_right;
        self.active_grid_mut()
            .scroll_region_down(top, bottom, left, right, n, blank);
        self.cursor.wrap_pending = false;
    }

    // DL: cursor row and below shift up by n; bottom of region filled blank.
    fn delete_lines(&mut self, n: usize) {
        if self.cursor.row < self.scroll_top
            || self.cursor.row > self.scroll_bottom
            || self.cursor.col < self.scroll_left
            || self.cursor.col > self.scroll_right
        {
            return;
        }
        let blank = self.blank();
        let top = self.cursor.row;
        let bottom = self.scroll_bottom;
        let left = self.scroll_left;
        let right = self.scroll_right;
        self.active_grid_mut()
            .scroll_region_up(top, bottom, left, right, n, blank);
        self.cursor.wrap_pending = false;
    }

    // ICH: shift cells at and right of cursor n columns to the right; fill the
    // gap with blanks. With DECLRMM on, cells stop at the right margin instead
    // of the screen edge, so per-pane edits don't leak past the divider.
    fn insert_chars(&mut self, n: usize) {
        let row = self.cursor.row;
        let col = self.cursor.col;
        let cols = self.cols;
        let right = self.scroll_right;
        if col >= cols || col > right {
            return;
        }
        let n = n.min(right + 1 - col);
        let blank = self.blank();
        let grid = self.active_grid_mut();
        let base = row * cols;
        if col + n <= right {
            grid.cells.copy_within(base + col..base + right + 1 - n, base + col + n);
        }
        for i in col..col + n {
            grid.cells[base + i] = blank;
        }
        self.cursor.wrap_pending = false;
    }

    // DCH: cells right of cursor shift left by n; right margin filled blank.
    fn delete_chars(&mut self, n: usize) {
        let row = self.cursor.row;
        let col = self.cursor.col;
        let cols = self.cols;
        let right = self.scroll_right;
        if col >= cols || col > right {
            return;
        }
        let n = n.min(right + 1 - col);
        let blank = self.blank();
        let grid = self.active_grid_mut();
        let base = row * cols;
        if col + n <= right {
            grid.cells.copy_within(base + col + n..base + right + 1, base + col);
        }
        for i in right + 1 - n..=right {
            grid.cells[base + i] = blank;
        }
        self.cursor.wrap_pending = false;
    }

    // ECH: replace n cells starting at the cursor with blanks. Cursor unchanged.
    // Clipped to the right margin so per-pane erases don't reach into the
    // neighbor.
    fn erase_chars(&mut self, n: usize) {
        let row = self.cursor.row;
        let col = self.cursor.col;
        let right = self.scroll_right;
        if col > right {
            return;
        }
        let end = (col + n).min(right + 1);
        let blank = self.blank();
        self.active_grid_mut().clear_row(row, col, end, blank);
    }

    /// DECSLRM (`CSI Pl ; Pr s`). When DECLRMM is enabled, sets the column
    /// margins and homes the cursor like DECSTBM does for rows. When DECLRMM
    /// is disabled, the bare `CSI s` form is SCOSC (save cursor) — that's the
    /// classic xterm overload; with params it's silently ignored.
    fn set_left_right_margin(&mut self, left: Option<u16>, right: Option<u16>) {
        if !self.lrmm_enabled {
            if left.is_none() && right.is_none() {
                self.save_cursor();
            }
            return;
        }
        let l = left.map(|v| v as usize).unwrap_or(1).saturating_sub(1);
        let r = right
            .map(|v| v as usize)
            .unwrap_or(self.cols)
            .saturating_sub(1);
        if l < r && r < self.cols {
            self.scroll_left = l;
            self.scroll_right = r;
        } else {
            // Spec says invalid range resets to full width.
            self.scroll_left = 0;
            self.scroll_right = self.cols - 1;
        }
        // DECSLRM homes the cursor.
        self.cursor.row = 0;
        self.cursor.col = 0;
        self.cursor.wrap_pending = false;
    }

    fn set_scroll_region(&mut self, top: Option<u16>, bottom: Option<u16>) {
        let t = top.map(|v| v as usize).unwrap_or(1).saturating_sub(1);
        let b = bottom.map(|v| v as usize).unwrap_or(self.rows).saturating_sub(1);
        if t < b && b < self.rows {
            self.scroll_top = t;
            self.scroll_bottom = b;
        }
        // DECSTBM homes the cursor.
        self.cursor.row = 0;
        self.cursor.col = 0;
        self.cursor.wrap_pending = false;
    }

    fn private_mode(&mut self, code: u16, set: bool) {
        match code {
            1 => self.app_cursor_keys = set,
            7 => self.autowrap = set,
            25 => self.cursor_visible = set,
            1000 => self.mouse_press_release = set,
            1002 => self.mouse_button_motion = set,
            1003 => self.mouse_any_motion = set,
            1006 => self.mouse_sgr = set,
            2004 => self.bracketed_paste = set,
            1049 | 1047 | 47 => self.switch_screen(set, code == 1049),
            // DECLRMM. Enabling/disabling resets the margins to the full
            // screen — apps must re-issue DECSLRM after enabling.
            69 => {
                self.lrmm_enabled = set;
                self.scroll_left = 0;
                self.scroll_right = self.cols - 1;
            }
            _ => {}
        }
    }

    fn reply(&mut self, bytes: &[u8]) {
        self.pending_response.extend_from_slice(bytes);
    }

    /// OSC payload is `Pn[;data...]`. We dispatch on the numeric Pn.
    fn handle_osc(&mut self, s: &str) {
        let (head, rest) = s.split_once(';').unwrap_or((s, ""));
        let code: u16 = match head.parse() {
            Ok(n) => n,
            Err(_) => return,
        };
        match code {
            // Title setting (0/1/2): ignore.
            0 | 1 | 2 => {}
            10 if rest == "?" => self.reply_color(10, self.default_fg_rgb),
            11 if rest == "?" => self.reply_color(11, self.default_bg_rgb),
            12 if rest == "?" => self.reply_color(12, self.default_cursor_rgb),
            // iTerm2 proprietary namespace. Only `File=...` (inline
            // images) is implemented; everything else is silently
            // dropped to match iTerm's "unknown verb is a no-op" contract.
            1337 => self.handle_osc_1337(rest),
            _ => {}
        }
    }

    /// `OSC 1337 ; <verb>=<args> [: <base64>] ST` — iTerm2's proprietary
    /// channel. The one verb we care about is `File=key=val,...:<base64>`
    /// for inline images.
    fn handle_osc_1337(&mut self, payload: &str) {
        // The verb prefix is the run up to the first '='. For `File=…` the
        // remaining text is the param list (key=val pairs separated by ';')
        // followed by a ':' and base64 payload.
        let Some(rest) = payload.strip_prefix("File=") else {
            return;
        };
        // Split the param-list from the base64 payload on the FIRST ':'.
        // Base64 alphabet doesn't include ':', so any colon separates the
        // wrapper from the body cleanly.
        let Some((args, b64)) = rest.split_once(':') else {
            return;
        };

        let mut width = ImageSizeSpec::Auto;
        let mut height = ImageSizeSpec::Auto;
        let mut preserve_aspect = true;
        let mut inline = true;
        let mut do_not_move_cursor = false;
        let mut name: Option<String> = None;

        for kv in args.split(';') {
            if kv.is_empty() {
                continue;
            }
            let Some((k, v)) = kv.split_once('=') else { continue };
            match k {
                "width" => width = parse_iterm_size(v).unwrap_or(ImageSizeSpec::Auto),
                "height" => height = parse_iterm_size(v).unwrap_or(ImageSizeSpec::Auto),
                "preserveAspectRatio" => preserve_aspect = v != "0",
                // `inline=0` means "download mode" — iTerm offers to save
                // the file. We don't have a download UI; just skip.
                "inline" => inline = v != "0",
                "doNotMoveCursor" => do_not_move_cursor = v != "0",
                "name" => {
                    // Filename is base64'd in iTerm's spec. Best-effort
                    // decode — only used as a debug label.
                    use base64::Engine;
                    if let Ok(b) = base64::engine::general_purpose::STANDARD.decode(v) {
                        if let Ok(s) = String::from_utf8(b) {
                            name = Some(s);
                        }
                    }
                }
                // `size` and any unknown keys are accepted-but-ignored.
                _ => {}
            }
        }

        if !inline {
            return;
        }

        // Base64 payload. iTerm allows internal newlines / spaces for
        // wrapping — strip whitespace before decode.
        use base64::Engine;
        let cleaned: String = b64.chars().filter(|c| !c.is_ascii_whitespace()).collect();
        let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(cleaned.as_bytes()) else {
            return;
        };

        let pixel_size = crate::images::peek_dimensions(&bytes);
        let cell_extent = compute_cell_extent(
            width,
            height,
            pixel_size,
            self.cell_w_px,
            self.line_h_px,
            self.cols as u16,
            self.rows as u16,
            preserve_aspect,
        );

        let original_row = self.cursor.row as isize;
        let original_col = self.cursor.col as isize;

        // Cursor advance: line-feed once per cell row the image will
        // occupy. This pushes any subsequent text below the image AND
        // triggers scroll-up at the bottom of the grid. We don't insert
        // the placement here (main.rs owns the Store), so we need to
        // compensate the captured anchor for any scrolls those LFs
        // produced — measured via the cursor's row delta so the math
        // stays robust to whatever `line_feed` actually does (DECSTBM,
        // origin mode, etc.).
        let rows = cell_extent.0 as isize;
        if !do_not_move_cursor {
            for _ in 0..rows {
                self.line_feed();
            }
        }
        let cursor_advance = self.cursor.row as isize - original_row;
        let scrolls = if do_not_move_cursor { 0 } else { rows - cursor_advance };
        let cell_anchor = (original_row - scrolls, original_col);

        self.pending_image_uploads.push(PendingImageUpload {
            bytes,
            pixel_size,
            width,
            height,
            preserve_aspect,
            do_not_move_cursor,
            label: name,
            cell_anchor,
            cell_extent,
            // iTerm OSC 1337 doesn't use Kitty's id system.
            kitty_image_id: None,
            kitty_placement_id: None,
            // OSC 1337 always displays — there's no transmit-only variant.
            display_immediately: true,
            // iTerm OSC 1337 doesn't expose sub-cell offsets, z-index,
            // or source crops — all default.
            pixel_offset: (0, 0),
            z_index: 0,
            src_rect: None,
            animation_frame: None,
            animation_control: None,
            raw_rgba_dims: None,
        });
    }

    /// Reply to an XTWINOPS size query. Apps (Kitty's icat kitten in
    /// particular) probe these before sending image protocols so they
    /// know how many pixels a cell is — without a reply the kitten
    /// errors out with "terminal does not support reporting screen
    /// sizes in pixels."
    ///
    /// Only the report subset (14 / 16 / 18) is honored; the
    /// resize/move/raise/lower actions on the same final byte are
    /// silently ignored upstream in the parser.
    fn handle_xtwinops_query(&mut self, ps: u16) {
        let cell_w = self.cell_w_px.max(1) as usize;
        let line_h = self.line_h_px.max(1) as usize;
        match ps {
            // 14 → text area in pixels: `\e[4;<height>;<width>t`.
            14 => {
                let w = self.cols * cell_w;
                let h = self.rows * line_h;
                let s = format!("\x1b[4;{};{}t", h, w);
                self.pending_response.extend_from_slice(s.as_bytes());
            }
            // 16 → single cell in pixels: `\e[6;<height>;<width>t`.
            16 => {
                let s = format!("\x1b[6;{};{}t", line_h, cell_w);
                self.pending_response.extend_from_slice(s.as_bytes());
            }
            // 18 → text area in characters: `\e[8;<rows>;<cols>t`.
            18 => {
                let s = format!("\x1b[8;{};{}t", self.rows, self.cols);
                self.pending_response.extend_from_slice(s.as_bytes());
            }
            _ => {} // parser only emits the three above; defensive.
        }
    }

    /// Handle a captured APC payload. The Kitty graphics protocol
    /// (`_G<ctrl>;<base64>`) is the only consumer today; other APC
    /// strings drop silently.
    fn handle_apc(&mut self, s: &str) {
        // Set `YUTANI_LOG_APC=1` to see the control-data portion of
        // every Kitty APC the terminal receives — useful for figuring
        // out which protocol subset a tool (icat, ueberzug, etc.) is
        // actually using when an image doesn't render. The payload
        // body is elided since it's typically a multi-KB base64 blob.
        if std::env::var_os("YUTANI_LOG_APC").is_some() {
            let head: String = s.chars().take(120).collect();
            let elided = if s.len() > 120 { "…" } else { "" };
            eprintln!("[apc] {}{}", head, elided);
        }

        // Strip the `G` verb. The `;` separator between control data and
        // payload is optional — a control-only message (e.g. `a=q,...`
        // for capability query) may omit it.
        let Some(rest) = s.strip_prefix('G') else {
            return;
        };
        let (ctrl_str, payload) = match rest.split_once(';') {
            Some((c, p)) => (c, p),
            None => (rest, ""),
        };
        let Some(mut ctrl) = parse_kitty_control(ctrl_str) else {
            return;
        };

        // Thread continuation chunks back to the in-flight chunked
        // transmission. `kitten icat` (and many other apps) puts
        // `i=` on the FIRST chunk of a chunked image / frame and
        // omits it on every continuation — the spec lets the
        // terminal "remember" which transmission is currently being
        // assembled. Without this injection, continuation chunks
        // get routed to the anonymous-stream bucket and the
        // id-keyed entry leaks, so `kitty_image_id_lookup` later
        // returns None and the placeholder cells render as tofu.
        if ctrl.image_id.is_none() {
            if let Some(id) = self.current_chunked_id {
                if matches!(
                    ctrl.action,
                    KittyAction::Transmit
                        | KittyAction::TransmitAndDisplay
                        | KittyAction::AnimationFrame
                ) {
                    ctrl.image_id = Some(id);
                }
            }
        }
        // Second fallback: per Kitty spec, when `i=` is missing on an
        // op that targets an existing image (place / delete / animate
        // / frame), the most recently created image is the implicit
        // target. icat's animation stream relies on this — frame
        // transmissions for the GIF being animated arrive as bare
        // `a=f` with neither `i=` nor `m=` (no in-flight chunked
        // transmission to inherit from either), so the
        // `current_chunked_id` thread above doesn't help. Without
        // this second fallback those frames silently drop and the
        // animation never plays past frame 1.
        if ctrl.image_id.is_none() {
            if matches!(
                ctrl.action,
                KittyAction::Place
                    | KittyAction::Delete
                    | KittyAction::AnimationFrame
                    | KittyAction::AnimationControl,
            ) {
                ctrl.image_id = self.last_kitty_image_id;
            }
        }
        // Update the in-flight tracker. First chunk of an id-keyed
        // stream sets it; the matching final chunk clears it. Single
        // chunks (no `m=` on either side) leave it untouched.
        if matches!(
            ctrl.action,
            KittyAction::Transmit
                | KittyAction::TransmitAndDisplay
                | KittyAction::AnimationFrame
        ) {
            if let Some(id) = ctrl.image_id {
                if ctrl.more_chunks {
                    self.current_chunked_id = Some(id);
                } else if self.current_chunked_id == Some(id) {
                    self.current_chunked_id = None;
                }
            }
        }

        // Capability handshake. Apps query each (format, transmission)
        // combo on startup; we must answer truthfully or they'll pick a
        // path we can't serve and silently drop their image. Kitty's
        // icat in particular queries `f=24` (raw RGB) variants first and
        // will use raw + shared memory if we say OK to them — we don't
        // implement those, so reply ENOTSUPPORTED and force the fallback
        // to f=100/t=f which we do support.
        //
        // Quiet modes per spec: q=0 (default) reply always, q=1 suppress
        // success, q=2 suppress all.
        if matches!(ctrl.action, KittyAction::Query) {
            let supported = self.kitty_query_supported(ctrl.format, ctrl.transmission);
            let suppress = match ctrl.quiet {
                0 => false,
                1 => supported, // suppress OK, but still send errors
                _ => true,      // q>=2: silence everything
            };
            if !suppress {
                let body = if supported {
                    "OK".to_string()
                } else {
                    // Spec format is `ENOTSUPPORTED:<message>`; the
                    // message text is informational.
                    "ENOTSUPPORTED:format or transmission not supported".to_string()
                };
                let reply = match ctrl.image_id {
                    Some(id) => format!("\x1b_Gi={};{}\x1b\\", id, body),
                    None => format!("\x1b_G;{}\x1b\\", body),
                };
                self.pending_response.extend_from_slice(reply.as_bytes());
            }
            return;
        }

        // Place / Delete / animation-control don't carry an image
        // payload (or the control branch consumes it specially) —
        // handle and return.
        match ctrl.action {
            KittyAction::Place => return self.handle_apc_place(&ctrl),
            KittyAction::Delete => return self.handle_apc_delete(&ctrl),
            KittyAction::AnimationControl => return self.handle_apc_animation_control(&ctrl),
            KittyAction::AnimationFrame => return self.handle_apc_animation_frame(payload, &ctrl),
            KittyAction::Other => return,
            // Fall through for Transmit / TransmitAndDisplay; both
            // accept an image payload.
            KittyAction::Transmit | KittyAction::TransmitAndDisplay => {}
            KittyAction::Query => unreachable!("handled above"),
        }

        // Format/transmission pair must be in our supported set.
        // `kitty_query_supported` is the source of truth — the query
        // branch above tells the app exactly which combos work, and
        // here we enforce the same set on the actual transmission.
        // Anything outside (raw over file, shared memory, etc.) drops
        // silently.
        if !self.kitty_query_supported(ctrl.format, ctrl.transmission) {
            return;
        }

        match ctrl.transmission {
            KittyTransmission::Direct => self.handle_apc_direct(payload, &ctrl),
            KittyTransmission::File => self.handle_apc_file(payload, &ctrl, /*delete=*/ false),
            KittyTransmission::TempFile => self.handle_apc_file(payload, &ctrl, /*delete=*/ true),
            KittyTransmission::SharedMemory => self.handle_apc_shm(payload, &ctrl),
            KittyTransmission::Other => {} // unsupported medium → drop
        }
    }

    /// `t=s` — POSIX shared-memory transmission. Payload is the
    /// (base64'd) SHM object name. Unix-only; on other targets this
    /// silently drops (we shouldn't get here at all since
    /// `kitty_query_supported` returns false off-Unix).
    #[cfg(unix)]
    fn handle_apc_shm(&mut self, payload: &str, ctrl: &KittyControl) {
        let Some(name) = decode_kitty_shm_name(payload) else { return };
        let bytes_opt = read_kitty_shm(&name);
        // Per spec the terminal owns the unlink — even on read
        // failure, attempt cleanup so a malformed sender doesn't
        // leak SHM objects.
        unlink_kitty_shm(&name);
        let Some(mut raw) = bytes_opt else { return };
        if ctrl.compressed_zlib {
            let Some(inflated) = inflate_kitty_zlib(&raw) else { return };
            raw = inflated;
        }
        // icat (and other apps) sometimes omits `f=` on a=T payloads
        // that are actually raw RGB/RGBA. The parser turns omitted
        // `f=` into the PNG default, which would send the raw bytes
        // through the PNG decoder and fail with "image format could
        // not be determined". Run the same size-based inference the
        // a=f path uses, with no fallback since this IS the base.
        let effective_format =
            resolve_kitty_format(ctrl.format, &raw, ctrl.source_w, ctrl.source_h, None);
        let Some((bytes, pixel_size, raw_rgba_dims)) =
            prepare_kitty_payload(effective_format, &raw, ctrl.source_w, ctrl.source_h)
        else {
            return;
        };
        let display = matches!(ctrl.action, KittyAction::TransmitAndDisplay)
            && !ctrl.virtual_placement;
        let (pixel_offset, z_index, src_rect) = kitty_placement_params(ctrl);
        self.finalize_kitty_image_bytes(
            bytes,
            pixel_size,
            ctrl.cells_cols,
            ctrl.cells_rows,
            ctrl.do_not_move_cursor,
            ctrl.image_id,
            ctrl.placement_id,
            display,
            pixel_offset,
            z_index,
            src_rect,
            effective_format,
            raw_rgba_dims,
        );
    }

    #[cfg(not(unix))]
    fn handle_apc_shm(&mut self, _payload: &str, _ctrl: &KittyControl) {
        // Shared-memory transmission isn't implemented off-Unix.
    }

    /// `a=p` — place a previously-transmitted image at the cursor.
    /// Requires `i=` to identify which image, and `c=` / `r=` for cell
    /// extent (we don't track the image's native cell size in
    /// Terminal). Without one we drop silently per the Kitty contract.
    fn handle_apc_place(&mut self, ctrl: &KittyControl) {
        let Some(client_id) = ctrl.image_id else { return };
        let Some(image_id) = self.kitty_image_id_lookup(client_id) else { return };

        // Cell extent: prefer the explicit c/r values, fall back to
        // (1, 1) as a visible placeholder. (A future slice could track
        // the image's pixel size in Terminal so we can compute Auto.)
        let cols = ctrl.cells_cols.unwrap_or(1).clamp(1, u16::MAX as u32) as u16;
        let rows = ctrl.cells_rows.unwrap_or(1).clamp(1, u16::MAX as u32) as u16;

        let original_row = self.cursor.row as isize;
        let original_col = self.cursor.col as isize;
        if !ctrl.do_not_move_cursor {
            for _ in 0..rows {
                self.line_feed();
            }
        }
        let cursor_advance = self.cursor.row as isize - original_row;
        let scrolls = if ctrl.do_not_move_cursor {
            0
        } else {
            rows as isize - cursor_advance
        };
        let top_row = original_row - scrolls;

        let (pixel_offset, z_index, src_rect) = kitty_placement_params(ctrl);
        self.insert_placement_kitty(
            image_id,
            top_row,
            original_col,
            rows,
            cols,
            z_index,
            pixel_offset,
            src_rect,
            Some(client_id),
            ctrl.placement_id,
        );
    }

    /// `a=f` — transmit a new frame for an existing animated image.
    /// Queues the frame's raw payload (PNG / raw RGB / raw RGBA) plus
    /// the per-frame metadata (target slot, compose base, gap_ms, x/y
    /// position) into `pending_image_uploads` so main.rs can drive the
    /// async decode + composite + GPU upload through the existing
    /// store-poll loop. Drops silently when the parent image id is
    /// missing or when the transmission medium isn't one we support.
    fn handle_apc_animation_frame(&mut self, payload: &str, ctrl: &KittyControl) {
        let Some(client_id) = ctrl.image_id else { return };
        if !matches!(ctrl.format, KittyFormat::Other)
            && !self.kitty_query_supported(ctrl.format, ctrl.transmission)
        {
            return;
        }

        // Direct-transmission chunking: `m=1` on `a=f` accumulates
        // into `kitty_chunks` under the same image id, same as `a=t`.
        // Only finalize when the last chunk (`m=0` / missing) arrives.
        // File / shared-memory paths are inherently single-message so
        // their chunking branch is moot.
        if matches!(ctrl.transmission, KittyTransmission::Direct) {
            if ctrl.more_chunks {
                let entry =
                    self.kitty_chunks.entry(client_id).or_insert_with(|| KittyChunks {
                        b64: String::new(),
                        // Carry through the parser's view — even if
                        // Png (the "no f= seen" sentinel) — so the
                        // final-chunk path can run the same format
                        // inference the single-chunk path uses.
                        format: ctrl.format,
                        source_w: ctrl.source_w,
                        source_h: ctrl.source_h,
                        cells_cols: ctrl.cells_cols,
                        cells_rows: ctrl.cells_rows,
                        do_not_move_cursor: true,
                        kitty_image_id: Some(client_id),
                        kitty_placement_id: None,
                        display_immediately: false,
                        compressed_zlib: ctrl.compressed_zlib,
                        anim_first_chunk: Some(kitty_anim_frame_spec_from_ctrl(ctrl)),
                    });
                append_b64_filtered(&mut entry.b64, payload);
                return;
            }
            // Last chunk of a multi-chunk transmission: pull the
            // accumulator out, append the final piece, decode the
            // assembled base64, and continue through the normal
            // finalize path with the parent's params.
            if let Some(mut acc) = self.kitty_chunks.remove(&client_id) {
                use base64::Engine;
                append_b64_filtered(&mut acc.b64, payload);
                let Ok(mut raw) = base64::engine::general_purpose::STANDARD
                    .decode(acc.b64.as_bytes())
                else {
                    return;
                };
                if acc.compressed_zlib {
                    let Some(inflated) = inflate_kitty_zlib(&raw) else { return };
                    raw = inflated;
                }
                let effective_format = self.resolve_frame_format(
                    client_id,
                    acc.format,
                    &raw,
                    acc.source_w,
                    acc.source_h,
                );
                // Fast path: raw RGB/RGBA bypasses the worker. See
                // `convert_kitty_raw_to_rgba` for the rationale.
                let spec_override = acc.anim_first_chunk;
                if matches!(
                    effective_format,
                    KittyFormat::Rgb | KittyFormat::Rgba,
                ) {
                    let Some((rgba, w, h)) = convert_kitty_raw_to_rgba(
                        effective_format,
                        &raw,
                        acc.source_w,
                        acc.source_h,
                    ) else {
                        return;
                    };
                    self.queue_animation_frame_upload(
                        client_id, rgba, ctrl, Some((w, h)), spec_override,
                    );
                    return;
                }
                let Some((bytes, _pixel_size)) =
                    normalize_kitty_payload(effective_format, &raw, acc.source_w, acc.source_h)
                else {
                    return;
                };
                self.queue_animation_frame_upload(client_id, bytes, ctrl, None, spec_override);
                return;
            }
        }

        // Single-chunk decode path. Pull the bytes through the
        // medium-specific reader, decompress if needed, and normalize
        // raw RGB/RGBA into PNG so the decode worker sees one shape.
        let raw_bytes: Option<Vec<u8>> = match ctrl.transmission {
            KittyTransmission::Direct => {
                use base64::Engine;
                let mut b64 = String::with_capacity(payload.len());
                append_b64_filtered(&mut b64, payload);
                base64::engine::general_purpose::STANDARD.decode(b64.as_bytes()).ok()
            }
            KittyTransmission::File => {
                decode_kitty_file_path(payload).and_then(|p| read_kitty_file(&p))
            }
            KittyTransmission::TempFile => {
                decode_kitty_file_path(payload).and_then(|p| {
                    let bytes = read_kitty_file(&p);
                    if path_is_under_temp_dir(&p) {
                        let _ = std::fs::remove_file(&p);
                    }
                    bytes
                })
            }
            #[cfg(unix)]
            KittyTransmission::SharedMemory => {
                decode_kitty_shm_name(payload).and_then(|n| {
                    let bytes = read_kitty_shm(&n);
                    unlink_kitty_shm(&n);
                    bytes
                })
            }
            #[cfg(not(unix))]
            KittyTransmission::SharedMemory => None,
            KittyTransmission::Other => None,
        };
        let Some(mut raw) = raw_bytes else { return };
        if ctrl.compressed_zlib {
            let Some(inflated) = inflate_kitty_zlib(&raw) else { return };
            raw = inflated;
        }
        let effective_format = self.resolve_frame_format(
            client_id,
            ctrl.format,
            &raw,
            ctrl.source_w,
            ctrl.source_h,
        );
        if matches!(effective_format, KittyFormat::Rgb | KittyFormat::Rgba) {
            let Some((rgba, w, h)) = convert_kitty_raw_to_rgba(
                effective_format,
                &raw,
                ctrl.source_w,
                ctrl.source_h,
            ) else {
                return;
            };
            self.queue_animation_frame_upload(client_id, rgba, ctrl, Some((w, h)), None);
            return;
        }
        let Some((bytes, _pixel_size)) =
            normalize_kitty_payload(effective_format, &raw, ctrl.source_w, ctrl.source_h)
        else {
            return;
        };
        self.queue_animation_frame_upload(client_id, bytes, ctrl, None, None);
    }

    /// Pick the right pixel format for an `a=f` raw payload.
    ///
    /// Thin wrapper around [`resolve_kitty_format`] that supplies the
    /// recorded base format for this image id as the last-resort
    /// fallback. Frame payloads inherit the base's format when the
    /// app omits `f=` AND the byte count isn't a clean RGB / RGBA
    /// match (a degenerate case, but worth handling).
    fn resolve_frame_format(
        &self,
        client_id: u32,
        parsed_format: KittyFormat,
        raw: &[u8],
        source_w: Option<u32>,
        source_h: Option<u32>,
    ) -> KittyFormat {
        resolve_kitty_format(
            parsed_format,
            raw,
            source_w,
            source_h,
            self.kitty_image_formats.get(&client_id).copied(),
        )
    }

    /// Shared tail of the `a=f` path. Builds the `PendingImageUpload`
    /// that main.rs's drain routes into `Store::request_insert_frame`.
    /// `cell_extent: (0, 0)` plus `display_immediately: false` keep
    /// the upload off the placement-creation path entirely — frames
    /// are not displayable on their own; they're metadata for the
    /// parent image.
    fn queue_animation_frame_upload(
        &mut self,
        client_id: u32,
        bytes: Vec<u8>,
        ctrl: &KittyControl,
        raw_rgba_dims: Option<(u32, u32)>,
        spec_override: Option<KittyAnimationFrameSpec>,
    ) {
        // Chunked finalize passes `spec_override` lifted from the
        // FIRST chunk; only the first chunk carries `z=` (gap_ms),
        // `x=`/`y=` (dst), `r=` (target_slot), `c=` (compose_base)
        // — taking them from `ctrl` (the last chunk) zeros them all
        // out and the animation runs at the 1ms floor.
        let animation_frame =
            spec_override.unwrap_or_else(|| kitty_anim_frame_spec_from_ctrl(ctrl));
        self.pending_image_uploads.push(PendingImageUpload {
            bytes,
            pixel_size: None,
            width: ImageSizeSpec::Auto,
            height: ImageSizeSpec::Auto,
            preserve_aspect: true,
            do_not_move_cursor: true,
            label: Some("kitty animation frame".into()),
            cell_anchor: (0, 0),
            cell_extent: (0, 0),
            kitty_image_id: Some(client_id),
            kitty_placement_id: None,
            display_immediately: false,
            pixel_offset: (0, 0),
            z_index: 0,
            src_rect: None,
            animation_frame: Some(animation_frame),
            animation_control: None,
            raw_rgba_dims,
        });
    }

    /// `a=a` — animation playback / per-frame editing. Mutates the
    /// store's `AnimationState` for the target image. No payload is
    /// consumed.
    fn handle_apc_animation_control(&mut self, ctrl: &KittyControl) {
        let Some(client_id) = ctrl.image_id else { return };
        self.pending_image_uploads.push(PendingImageUpload {
            bytes: Vec::new(),
            pixel_size: None,
            width: ImageSizeSpec::Auto,
            height: ImageSizeSpec::Auto,
            preserve_aspect: true,
            do_not_move_cursor: true,
            label: None,
            cell_anchor: (0, 0),
            cell_extent: (0, 0),
            kitty_image_id: Some(client_id),
            kitty_placement_id: None,
            display_immediately: false,
            pixel_offset: (0, 0),
            z_index: 0,
            src_rect: None,
            animation_frame: None,
            animation_control: Some(KittyAnimationControl {
                control: ctrl.anim_control,
                loop_count: ctrl.anim_loop_count,
                make_current: ctrl.anim_make_current.filter(|&n| n > 0),
                edit_frame: ctrl.anim_frame_num.filter(|&n| n > 0),
                edit_gap_ms: ctrl.anim_gap_ms,
            }),
            raw_rgba_dims: None,
        });
    }

    /// `a=d` — delete placements (and optionally drop the underlying
    /// store entries via mark-and-sweep). Selector + relevant ids
    /// come from `d=` / `i=` / `p=`.
    fn handle_apc_delete(&mut self, ctrl: &KittyControl) {
        let selector = ctrl.delete_selector.unwrap_or(KittyDeleteSelector::All);
        match selector {
            KittyDeleteSelector::All => {
                // Every Kitty placement (those carrying a kitty_image_id)
                // drops. iTerm/Cmd-Shift-I placements survive — `a=d,d=a`
                // is a Kitty-specific cleanup, not a global one.
                self.primary
                    .placements
                    .retain(|p| p.kitty_image_id.is_none());
                self.alternate
                    .placements
                    .retain(|p| p.kitty_image_id.is_none());
                self.scrollback_placements
                    .retain(|sp| sp.placement.kitty_image_id.is_none());
                self.kitty_image_ids.clear();
                self.kitty_image_formats.clear();
                self.kitty_image_cell_extents.clear();
            }
            KittyDeleteSelector::Image => {
                let Some(client_id) = ctrl.image_id else { return };
                let Some(image_id) = self.kitty_image_id_lookup(client_id) else { return };
                self.remove_placements_with_image(image_id);
                self.kitty_image_ids.remove(&client_id);
                self.kitty_image_formats.remove(&client_id);
                self.kitty_image_cell_extents.remove(&client_id);
            }
            KittyDeleteSelector::Placement => {
                let Some(pid) = ctrl.placement_id else { return };
                self.primary
                    .placements
                    .retain(|p| p.kitty_placement_id != Some(pid));
                self.alternate
                    .placements
                    .retain(|p| p.kitty_placement_id != Some(pid));
                self.scrollback_placements
                    .retain(|sp| sp.placement.kitty_placement_id != Some(pid));
            }
            KittyDeleteSelector::Other => {} // unimplemented selector → drop
        }
    }

    /// Single source of truth for which (format, transmission) tuples we
    /// can actually serve. Used both by the capability-query reply and
    /// by the transmission branch — keeps the two paths in lockstep so
    /// we never say OK to something the dispatcher would then drop.
    ///
    /// Supported (Unix):
    /// - PNG over direct base64, file path, temp file, OR shared memory.
    /// - Raw RGB (`f=24`) and RGBA (`f=32`) over direct base64, temp
    ///   file, OR shared memory. Shared memory is the fastest path
    ///   for big raw payloads — zero copies through the PTY.
    ///
    /// On non-Unix targets `t=s` is unsupported (shm_open isn't
    /// available); the query reflects this so apps fall back.
    ///
    /// Unsupported everywhere:
    /// - Raw formats from a regular file path (`f=24/32, t=f`) — rare
    ///   and would need out-of-protocol dimension hints.
    fn kitty_query_supported(
        &self,
        format: KittyFormat,
        transmission: KittyTransmission,
    ) -> bool {
        // Shared memory is gated on `cfg(unix)` — Windows would need
        // a different API and the kitten won't pick `t=s` on Windows
        // anyway, but be explicit.
        let shm_ok = cfg!(unix);
        match (format, transmission) {
            (
                KittyFormat::Png,
                KittyTransmission::Direct | KittyTransmission::File | KittyTransmission::TempFile,
            ) => true,
            (KittyFormat::Png, KittyTransmission::SharedMemory) => shm_ok,
            (
                KittyFormat::Rgb | KittyFormat::Rgba,
                KittyTransmission::Direct | KittyTransmission::TempFile,
            ) => true,
            (KittyFormat::Rgb | KittyFormat::Rgba, KittyTransmission::SharedMemory) => shm_ok,
            _ => false,
        }
    }

    /// Direct base64 transmission: payload is the image bytes (possibly
    /// chunked across multiple APCs and reassembled by `kitty_chunks`).
    fn handle_apc_direct(&mut self, payload: &str, ctrl: &KittyControl) {
        // Branch on chunking. Four states: (id present, more chunks),
        // (id present, last chunk), (no id, more chunks), (no id, last
        // chunk). Continuation chunks without an explicit `i=` arrive
        // here with `ctrl.image_id` already injected by `handle_apc`'s
        // chunk-threading pre-step (see `current_chunked_id`), so the
        // id-keyed branches catch them just like real id-bearing
        // chunks. The two id-less branches handle genuinely anonymous
        // streams (`kitten icat` raw-JPG path, etc).
        //
        // Hot path: a typical 1MB image arrives in ~250 chunks, so the
        // per-chunk work has to stay minimal. Stream the whitespace
        // filter directly into the accumulator's existing String instead
        // of allocating a per-chunk staging buffer.
        // U=1 (virtual placement) suppresses the immediate placement
        // even when `a=T` was sent — the image is registered for later
        // unicode-placeholder positioning. Without this override, a=T,U=1
        // would create a duplicate placement at the cursor on top of
        // wherever the placeholders eventually land.
        let display = matches!(ctrl.action, KittyAction::TransmitAndDisplay)
            && !ctrl.virtual_placement;
        match (ctrl.image_id, ctrl.more_chunks) {
            (Some(id), true) => {
                let entry = self.kitty_chunks.entry(id).or_insert_with(|| KittyChunks {
                    b64: String::new(),
                    format: ctrl.format,
                    source_w: ctrl.source_w,
                    source_h: ctrl.source_h,
                    cells_cols: ctrl.cells_cols,
                    cells_rows: ctrl.cells_rows,
                    do_not_move_cursor: ctrl.do_not_move_cursor,
                    kitty_image_id: ctrl.image_id,
                    kitty_placement_id: ctrl.placement_id,
                    display_immediately: display,
                    compressed_zlib: ctrl.compressed_zlib,
                    anim_first_chunk: None, // a=T/a=t aren't animation frames
                });
                append_b64_filtered(&mut entry.b64, payload);
            }
            (Some(id), false) if self.kitty_chunks.contains_key(&id) => {
                let mut acc = self.kitty_chunks.remove(&id).expect("contains_key");
                append_b64_filtered(&mut acc.b64, payload);
                self.finalize_kitty_image_from_chunks(&acc);
            }
            (None, true) => {
                // Anonymous chunk. The first one establishes the
                // sizing/format; later ones just append base64. A new
                // first-chunk while one's open overwrites — there's no
                // way to distinguish them otherwise.
                let entry = self.kitty_chunks_anon.get_or_insert_with(|| KittyChunks {
                    b64: String::new(),
                    format: ctrl.format,
                    source_w: ctrl.source_w,
                    source_h: ctrl.source_h,
                    cells_cols: ctrl.cells_cols,
                    cells_rows: ctrl.cells_rows,
                    do_not_move_cursor: ctrl.do_not_move_cursor,
                    kitty_image_id: ctrl.image_id,
                    kitty_placement_id: ctrl.placement_id,
                    display_immediately: display,
                    compressed_zlib: ctrl.compressed_zlib,
                    anim_first_chunk: None, // a=T/a=t aren't animation frames
                });
                append_b64_filtered(&mut entry.b64, payload);
            }
            (None, false) if self.kitty_chunks_anon.is_some() => {
                let mut acc = self.kitty_chunks_anon.take().expect("is_some");
                append_b64_filtered(&mut acc.b64, payload);
                self.finalize_kitty_image_from_chunks(&acc);
            }
            _ => {
                // Single-chunk: id present (or not) with m=0 and no
                // in-flight buffer. Build a one-off filtered string
                // since the accumulator path isn't involved.
                let mut buf = String::with_capacity(payload.len());
                append_b64_filtered(&mut buf, payload);
                self.finalize_kitty_image_from_b64(&buf, ctrl, display);
            }
        }
    }

    /// Single-chunk dispatch: just bridges to the per-byte finalize
    /// with parameters lifted out of the current `KittyControl`.
    fn finalize_kitty_image_from_b64(&mut self, b64: &str, ctrl: &KittyControl, display: bool) {
        use base64::Engine;
        let Ok(mut raw) = base64::engine::general_purpose::STANDARD.decode(b64.as_bytes()) else {
            return;
        };
        if ctrl.compressed_zlib {
            let Some(inflated) = inflate_kitty_zlib(&raw) else { return };
            raw = inflated;
        }
        let effective_format =
            resolve_kitty_format(ctrl.format, &raw, ctrl.source_w, ctrl.source_h, None);
        let Some((bytes, pixel_size, raw_rgba_dims)) =
            prepare_kitty_payload(effective_format, &raw, ctrl.source_w, ctrl.source_h)
        else {
            return;
        };
        let (pixel_offset, z_index, src_rect) = kitty_placement_params(ctrl);
        self.finalize_kitty_image_bytes(
            bytes,
            pixel_size,
            ctrl.cells_cols,
            ctrl.cells_rows,
            ctrl.do_not_move_cursor,
            ctrl.image_id,
            ctrl.placement_id,
            display,
            pixel_offset,
            z_index,
            src_rect,
            effective_format,
            raw_rgba_dims,
        );
    }

    /// Chunked dispatch: same as `finalize_kitty_image_from_b64` but
    /// reads parameters from the first-chunk snapshot stored in
    /// `KittyChunks` (Kitty spec says only the first chunk's display
    /// attributes matter). Placement-side params (`X=`/`Y=`/`z=`/`x=`
    /// etc.) aren't stored on `KittyChunks` for now — apps that chunk
    /// rarely use them — so we pass defaults.
    fn finalize_kitty_image_from_chunks(&mut self, acc: &KittyChunks) {
        use base64::Engine;
        let Ok(mut raw) = base64::engine::general_purpose::STANDARD.decode(acc.b64.as_bytes()) else {
            return;
        };
        if acc.compressed_zlib {
            let Some(inflated) = inflate_kitty_zlib(&raw) else { return };
            raw = inflated;
        }
        let effective_format =
            resolve_kitty_format(acc.format, &raw, acc.source_w, acc.source_h, None);
        let Some((bytes, pixel_size, raw_rgba_dims)) =
            prepare_kitty_payload(effective_format, &raw, acc.source_w, acc.source_h)
        else {
            return;
        };
        self.finalize_kitty_image_bytes(
            bytes,
            pixel_size,
            acc.cells_cols,
            acc.cells_rows,
            acc.do_not_move_cursor,
            acc.kitty_image_id,
            acc.kitty_placement_id,
            acc.display_immediately,
            (0, 0),
            0,
            None,
            effective_format,
            raw_rgba_dims,
        );
    }

    /// File-based transmission (`t=f` or `t=t`). Payload is a
    /// base64-encoded UTF-8 filesystem path. We read the file ourselves
    /// rather than the app shipping its bytes over the PTY. Cheaper for
    /// large images (no base64 round-trip, no chunked reassembly).
    ///
    /// When `delete` is true (`t=t`, temp file), we unlink the file
    /// after reading, but only if it actually lives under
    /// `std::env::temp_dir()` — defense-in-depth against a malformed
    /// app pointing us at arbitrary paths.
    fn handle_apc_file(&mut self, payload: &str, ctrl: &KittyControl, delete: bool) {
        let Some(path) = decode_kitty_file_path(payload) else { return };
        let Some(mut raw) = read_kitty_file(&path) else { return };
        if delete && path_is_under_temp_dir(&path) {
            // Best-effort delete — if it fails (file already gone,
            // permission issue), there's nothing useful to do.
            let _ = std::fs::remove_file(&path);
        }
        if ctrl.compressed_zlib {
            let Some(inflated) = inflate_kitty_zlib(&raw) else { return };
            raw = inflated;
        }

        // Run through the same payload preparer the direct path uses
        // so raw formats over temp file (icat for big JPGs) skip the
        // PNG round-trip too.
        let effective_format =
            resolve_kitty_format(ctrl.format, &raw, ctrl.source_w, ctrl.source_h, None);
        let Some((bytes, pixel_size, raw_rgba_dims)) =
            prepare_kitty_payload(effective_format, &raw, ctrl.source_w, ctrl.source_h)
        else {
            return;
        };
        let display = matches!(ctrl.action, KittyAction::TransmitAndDisplay)
            && !ctrl.virtual_placement;
        let (pixel_offset, z_index, src_rect) = kitty_placement_params(ctrl);
        self.finalize_kitty_image_bytes(
            bytes,
            pixel_size,
            ctrl.cells_cols,
            ctrl.cells_rows,
            ctrl.do_not_move_cursor,
            ctrl.image_id,
            ctrl.placement_id,
            display,
            pixel_offset,
            z_index,
            src_rect,
            effective_format,
            raw_rgba_dims,
        );
    }

    /// Shared finalize for both direct-base64 and file transmissions —
    /// computes cell extent, advances the cursor with scroll
    /// compensation (only for `a=T`), queues the upload. Mirrors
    /// `handle_osc_1337`'s post-decode plumbing.
    #[allow(clippy::too_many_arguments)]
    fn finalize_kitty_image_bytes(
        &mut self,
        bytes: Vec<u8>,
        pixel_size: Option<(u32, u32)>,
        cells_cols: Option<u32>,
        cells_rows: Option<u32>,
        do_not_move_cursor: bool,
        kitty_image_id: Option<u32>,
        kitty_placement_id: Option<u32>,
        display_immediately: bool,
        pixel_offset: (i32, i32),
        z_index: i32,
        src_rect: Option<(u32, u32, u32, u32)>,
        source_format: KittyFormat,
        // `Some((w, h))` signals that `bytes` is already raw RGBA at
        // those dims — main.rs's drain routes through the
        // worker-bypass insert path. `None` means `bytes` is
        // PNG-or-similar and needs to go through the decode worker.
        raw_rgba_dims: Option<(u32, u32)>,
    ) {
        // Record the base's format so subsequent `a=f` frames that
        // omit `f=` can inherit it. Per Kitty spec the frame data
        // format defaults to the base image's format — typically
        // f=24 (RGB) or f=32 (RGBA) for animations sourced from
        // GIFs, since the app already decoded once.
        if let Some(id) = kitty_image_id {
            self.kitty_image_formats.insert(id, source_format);
            // Mark this image as "most recently completed" so a
            // subsequent `a=p` / `a=d` / `a=f` / `a=a` arriving
            // without `i=` can target it (per the Kitty spec
            // fallback). icat's animation-frame stream relies on
            // this — it emits `a=f` with no `i=` and no `m=` between
            // animation-control messages.
            self.last_kitty_image_id = Some(id);
            // Cache the image's total cell extent so the per-run
            // placeholder renderer can compute UVs against it. Only
            // record when BOTH dimensions are present — partial
            // values can't define a tiling.
            if let (Some(c), Some(r)) = (cells_cols, cells_rows) {
                self.kitty_image_cell_extents.insert(id, (c, r));
            }
        }
        // Kitty's `c=`/`r=` map onto `ImageSizeSpec::Cells` when present,
        // falling back to Auto (image's native cell extent) when not.
        // u16 clamping matches what the renderer can address.
        let width = cells_cols
            .map(|n| ImageSizeSpec::Cells(n.min(u16::MAX as u32) as u16))
            .unwrap_or(ImageSizeSpec::Auto);
        let height = cells_rows
            .map(|n| ImageSizeSpec::Cells(n.min(u16::MAX as u32) as u16))
            .unwrap_or(ImageSizeSpec::Auto);

        let cell_extent = compute_cell_extent(
            width,
            height,
            pixel_size,
            self.cell_w_px,
            self.line_h_px,
            self.cols as u16,
            self.rows as u16,
            true, // Kitty's default is to preserve aspect when one axis is omitted.
        );

        // For `a=t` (transmit only) we DON'T touch the cursor — the
        // image is being stored for a later `a=p` and the cursor should
        // stay where the app put it. cell_anchor is still populated
        // (with the current cursor) so main.rs has a sensible default
        // if it ever decides to display anyway.
        let original_row = self.cursor.row as isize;
        let original_col = self.cursor.col as isize;
        let rows = cell_extent.0 as isize;
        if display_immediately && !do_not_move_cursor {
            for _ in 0..rows {
                self.line_feed();
            }
        }
        let cursor_advance = self.cursor.row as isize - original_row;
        let scrolls = if display_immediately && !do_not_move_cursor {
            rows - cursor_advance
        } else {
            0
        };
        let cell_anchor = (original_row - scrolls, original_col);

        self.pending_image_uploads.push(PendingImageUpload {
            bytes,
            pixel_size,
            width,
            height,
            preserve_aspect: true,
            do_not_move_cursor,
            label: Some("kitty graphics".into()),
            cell_anchor,
            cell_extent,
            kitty_image_id,
            kitty_placement_id,
            display_immediately,
            pixel_offset,
            z_index,
            src_rect,
            animation_frame: None,
            animation_control: None,
            raw_rgba_dims,
        });
    }

    /// Handle a captured DCS payload. Currently we implement xterm's
    /// XTGETTCAP query (`+q<hex>;<hex>;...`) and tmux's passthrough
    /// wrapper (`tmux;<wrapped>`); other DCS strings are dropped.
    fn handle_dcs(&mut self, s: &str) {
        // tmux passthrough: apps running inside tmux that want to send
        // escape sequences to the OUTER terminal wrap them as
        // `ESC P tmux ; <wrapped> ESC \`. Inside `<wrapped>`, literal
        // ESC bytes are doubled (the ansi.rs parser already
        // un-doubles them in `dcs_esc`). Tmux strips this wrapper and
        // forwards the payload — but if someone cats a tmux-wrapped
        // recording directly into yutani (no tmux in the loop), we'd
        // never reach the inner sequences. Recognize the wrapper
        // here, strip it, and re-feed the body so any APC / CSI /
        // OSC inside dispatches through the normal pipeline.
        if let Some(wrapped) = s.strip_prefix("tmux;") {
            // The wrapped body is itself a sequence of escape codes —
            // re-parse it through a FRESH parser. Reusing
            // `self.parser` would inherit whatever state the outer
            // feed's Phase 1 ended in: when the PTY hands us a chunk
            // that splits a DCS mid-body, the outer parser is in
            // DcsString when handle_dcs gets called (Phase 2 dispatches
            // the prior complete DCS events first). Re-feeding the
            // inner `ESC _G ...` into that stuck state turns the
            // leading ESC into an unrecognized DCS escape, drops the
            // partial accumulator, and the rest of the inner APC
            // prints as plain text — visible as a screenful of base64
            // gibberish from each tmux-wrapped image transmission.
            //
            // A throwaway parser starts in Ground every time, so
            // re-entry is independent of whatever the outer parser is
            // in the middle of. Inner events still dispatch through
            // `self.dispatch`, so they hit the normal pipeline
            // (kitty_chunks accumulator, animation state, etc.).
            let mut inner = crate::ansi::Parser::new();
            let mut events = Vec::new();
            for ch in wrapped.chars() {
                inner.feed(ch, |e| events.push(e));
            }
            for e in events {
                self.dispatch(e);
            }
            return;
        }
        let Some(rest) = s.strip_prefix("+q") else {
            return;
        };
        // Group by status so we can emit one DCS per group rather than one
        // per cap — apps parse either form, but fewer round-trips is nicer.
        let mut known: Vec<(String, String)> = Vec::new();
        let mut unknown: Vec<String> = Vec::new();
        for hex_name in rest.split(';') {
            let Some(name) = hex_decode_ascii(hex_name) else { continue };
            match termcap_value(&name) {
                Some(v) => known.push((hex_name.to_ascii_lowercase(), hex_encode(v))),
                None => unknown.push(hex_name.to_ascii_lowercase()),
            }
        }
        if !known.is_empty() {
            let body = known
                .iter()
                .map(|(n, v)| format!("{}={}", n, v))
                .collect::<Vec<_>>()
                .join(";");
            let s = format!("\x1bP1+r{}\x1b\\", body);
            self.pending_response.extend_from_slice(s.as_bytes());
        }
        if !unknown.is_empty() {
            let body = unknown.join(";");
            let s = format!("\x1bP0+r{}\x1b\\", body);
            self.pending_response.extend_from_slice(s.as_bytes());
        }
    }

    fn reply_color(&mut self, code: u16, rgb: [u8; 3]) {
        // xterm RGB form repeats each 8-bit channel for 16-bit precision —
        // apps (vim, tmux) parse `rgb:RRRR/GGGG/BBBB` as either 8- or 16-bit.
        let s = format!(
            "\x1b]{};rgb:{:02x}{:02x}/{:02x}{:02x}/{:02x}{:02x}\x1b\\",
            code, rgb[0], rgb[0], rgb[1], rgb[1], rgb[2], rgb[2],
        );
        self.pending_response.extend_from_slice(s.as_bytes());
    }

    // CSI 5n → "\e[0n" (terminal OK). CSI 6n → "\e[<row>;<col>R" (cursor
    // position, 1-based). Other codes are ignored — we don't implement them
    // and replying with anything would only confuse the host.
    fn device_status_report(&mut self, code: u16) {
        match code {
            5 => self.reply(b"\x1b[0n"),
            6 => {
                let s = format!("\x1b[{};{}R", self.cursor.row + 1, self.cursor.col + 1);
                self.pending_response.extend_from_slice(s.as_bytes());
            }
            _ => {}
        }
    }

    fn switch_screen(&mut self, to_alt: bool, clear_and_save: bool) {
        if to_alt && !self.use_alternate {
            if clear_and_save {
                self.saved_primary = Some(self.cursor);
            }
            let blank = Cell::new(' ', Style::new());
            self.alternate.clear(blank);
            self.use_alternate = true;
            if clear_and_save {
                self.cursor = Cursor::new();
            }
        } else if !to_alt && self.use_alternate {
            self.use_alternate = false;
            if clear_and_save {
                if let Some(cur) = self.saved_primary.take() {
                    self.cursor = cur;
                }
            }
        }
    }

    fn save_cursor(&mut self) {
        let slot = if self.use_alternate {
            &mut self.saved_alternate
        } else {
            &mut self.saved_primary
        };
        *slot = Some(self.cursor);
    }

    fn restore_cursor(&mut self) {
        let saved = if self.use_alternate {
            self.saved_alternate
        } else {
            self.saved_primary
        };
        if let Some(cur) = saved {
            self.cursor = cur;
        }
    }

    fn full_reset(&mut self) {
        let blank = Cell::new(' ', Style::new());
        self.primary.clear(blank);
        self.alternate.clear(blank);
        self.cursor = Cursor::new();
        self.saved_primary = None;
        self.saved_alternate = None;
        self.use_alternate = false;
        self.scroll_top = 0;
        self.scroll_bottom = self.rows - 1;
        self.lrmm_enabled = false;
        self.scroll_left = 0;
        self.scroll_right = self.cols - 1;
        self.autowrap = true;
        self.cursor_visible = true;
        self.app_cursor_keys = false;
        self.cursor_style_dec = 0;
        self.mouse_press_release = false;
        self.mouse_button_motion = false;
        self.mouse_any_motion = false;
        self.mouse_sgr = false;
        self.bracketed_paste = false;
        self.pending_response.clear();
        self.scrollback.clear();
        // Grid::clear already dropped per-grid placements above; also drop
        // scrollback placements since the scrollback rows they anchor to are
        // about to be cleared.
        self.scrollback_placements.clear();
        self.next_placement_id = 1;
    }
}

/// Decode an even-length lowercase/uppercase hex string into its ASCII form.
/// XTGETTCAP queries name capabilities this way (`Co` → "436f").
/// Compute the cell extent (rows, cols) of an inline image given the
/// iTerm2 size specs, the image's native pixel dimensions (if known),
/// the current cell-pixel size, and the viewport extent.
///
/// preserveAspectRatio kicks in when one axis is `Auto` and the other is
/// explicit: the auto-side scales proportionally to the image's native
/// aspect ratio. When both are explicit, both are honoured verbatim
/// (matching iTerm's behaviour — explicit beats preserve).
///
/// Falls back to `(1, 1)` when no useful information is available
/// (Auto + Auto + no pixel size); the placement still appears, just
/// tiny, until decode completes and the user re-issues the OSC with
/// better params.
fn compute_cell_extent(
    width: ImageSizeSpec,
    height: ImageSizeSpec,
    image_px: Option<(u32, u32)>,
    cell_w_px: u32,
    line_h_px: u32,
    viewport_cols: u16,
    viewport_rows: u16,
    preserve_aspect: bool,
) -> (u16, u16) {
    // Resolve each axis to pixels first, then to cells. Lets us apply
    // preserveAspectRatio in pixel space where the math is cleaner.
    let cell_w_px = cell_w_px.max(1);
    let line_h_px = line_h_px.max(1);
    let viewport_w_px = (viewport_cols as u32).saturating_mul(cell_w_px);
    let viewport_h_px = (viewport_rows as u32).saturating_mul(line_h_px);

    // `cell_axis_px` is the cell size on the axis being resolved — width
    // axis uses cell_w_px, height axis uses line_h_px. Passing it
    // explicitly avoids a fragile equality check on viewport sizes (which
    // could match coincidentally on a square viewport).
    let resolve = |spec: ImageSizeSpec,
                   viewport_axis_px: u32,
                   cell_axis_px: u32,
                   img_axis_px: Option<u32>|
     -> Option<u32> {
        match spec {
            ImageSizeSpec::Cells(n) => Some((n as u32).saturating_mul(cell_axis_px)),
            ImageSizeSpec::Pixels(n) => Some(n),
            ImageSizeSpec::Percent(n) => {
                Some(viewport_axis_px.saturating_mul(n).saturating_div(100).max(1))
            }
            ImageSizeSpec::Auto => img_axis_px,
        }
    };

    let img_w = image_px.map(|(w, _)| w);
    let img_h = image_px.map(|(_, h)| h);
    let mut w_px = resolve(width, viewport_w_px, cell_w_px, img_w);
    let mut h_px = resolve(height, viewport_h_px, line_h_px, img_h);

    if preserve_aspect {
        if let Some((iw, ih)) = image_px {
            if iw > 0 && ih > 0 {
                // Only override the side that was left as Auto. Explicit
                // sizing always wins; preserve only fills in the missing
                // axis from the image's native aspect ratio.
                match (width, height) {
                    (ImageSizeSpec::Auto, _) => {
                        if let Some(h) = h_px {
                            w_px = Some(((h as u64) * (iw as u64) / (ih as u64)).max(1) as u32);
                        }
                    }
                    (_, ImageSizeSpec::Auto) => {
                        if let Some(w) = w_px {
                            h_px = Some(((w as u64) * (ih as u64) / (iw as u64)).max(1) as u32);
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    // Ceiling divide pixels → cells. `saturating_add` because a malformed
    // protocol payload (Kitty `s=`/`v=`, eventually) could pass a width
    // close to u32::MAX; a plain `+ cell_w_px - 1` panics in debug and
    // wraps in release. Saturating clips the result to u16::MAX cells via
    // the final `.min(...)`, which is the worst case the renderer can
    // handle. Fallback to 1 if the spec is unresolvable (e.g. Auto with
    // no header peek).
    let cols = w_px
        .map(|px| (px.saturating_add(cell_w_px - 1) / cell_w_px).max(1))
        .unwrap_or(1);
    let rows = h_px
        .map(|px| (px.saturating_add(line_h_px - 1) / line_h_px).max(1))
        .unwrap_or(1);
    (
        rows.min(u16::MAX as u32) as u16,
        cols.min(u16::MAX as u32) as u16,
    )
}

/// Parse an iTerm2 OSC 1337 size token. Accepts:
///
/// - `auto` (case-insensitive) → [`ImageSizeSpec::Auto`]
/// - `N` (digits only) → [`ImageSizeSpec::Cells`]
/// - `Npx` → [`ImageSizeSpec::Pixels`]
/// - `N%` → [`ImageSizeSpec::Percent`]
///
/// Returns `None` for anything else (negative numbers, unsupported units,
/// empty string). Callers default to `Auto` on `None`.
/// Kitty graphics-protocol action verb. We only handle the small subset
/// that `kitty +kitten icat` needs in slice 1; everything else routes to
/// `Other` and gets silently dropped (the Kitty contract — unknown action
/// is a no-op).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum KittyAction {
    /// `a=t` — transmit only, don't display. Image is held by id for a
    /// later `a=p`. Phase 1 of slice 1 doesn't yet support deferred
    /// placement; treat as a no-op.
    Transmit,
    /// `a=T` — transmit and display immediately. The default action when
    /// `a=` is absent. This is what icat sends.
    TransmitAndDisplay,
    /// `a=q` — query capability. Reply with `OK` so the app proceeds to
    /// send real images (K1.5).
    Query,
    /// `a=p` — place a previously-transmitted image (by `i=` id) at the
    /// current cursor. Pairs with `Transmit` to let an app re-display
    /// an image without re-uploading it. Requires `Terminal::kitty_image_ids`
    /// to have a mapping for the requested id.
    Place,
    /// `a=d` — delete placement(s) or image(s). The actual selector
    /// comes from `d=` (and the relevant `i=` / `p=` keys).
    Delete,
    /// `a=f` — transmit a new frame for an existing animated image.
    /// Uses `i=` to identify the parent, `r=` to pick a target frame
    /// slot (0 / missing = append), `c=` for the base frame to compose
    /// against (1-based, default 1), `z=` for the per-frame gap in ms,
    /// and `X=`/`Y=`/`s=`/`v=` for the per-frame placement + raw
    /// payload dimensions.
    AnimationFrame,
    /// `a=a` — animation control / per-frame editing. `s=` picks the
    /// playback state (1=stop, 2=run while frames loading, 3=run with
    /// loops), `v=` is the loop count, `c=` makes a frame the current
    /// one, and `r=`/`z=` together edit a frame's gap.
    AnimationControl,
    /// Unknown action — accepted into the parser but produces no
    /// observable behavior.
    Other,
}

/// `d=` delete selector. We implement the subset that real apps use:
/// by image id, by placement id, and the "all images" sweep. Other
/// selectors (`c` under cursor, `n` by image number, `f` / `F` by
/// frame, etc.) drop silently for now.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum KittyDeleteSelector {
    /// `d=a` — delete all visible placements (and their backing
    /// images). The capital `A` variant also removes images from the
    /// store; we treat both as "remove placements + drop store
    /// entries via mark-and-sweep on the next frame" since our
    /// architecture doesn't distinguish.
    All,
    /// `d=i` — delete placements + image for the given `i=` id.
    /// Lowercase `i` removes placements but keeps the image stored;
    /// uppercase `I` also removes the image. We implement only the
    /// remove-everything semantics (uppercase) — saves a code path
    /// and matches what `kitty +kitten icat --transfer-mode=memory`
    /// expects on cleanup.
    Image,
    /// `d=p` — delete placement with the given `p=` id.
    Placement,
    /// Unknown / unimplemented selector. The dispatcher drops these.
    Other,
}

/// Pixel-format identifier from `f=`.
///
/// PNG (`f=100`) is the universal format. Raw RGB (`f=24`) and RGBA
/// (`f=32`) are what `kitty +kitten icat` uses for non-PNG sources
/// (JPG, GIF, etc.) — it decodes to raw and ships those bytes rather
/// than re-encoding to PNG. Raw formats require source dimensions
/// (`s=` / `v=`); we PNG-encode them on the receiving side so they
/// flow through the same decoder path.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum KittyFormat {
    Png,
    /// `f=24` — bytes are `s * v * 3` straight RGB.
    Rgb,
    /// `f=32` — bytes are `s * v * 4` straight RGBA.
    Rgba,
    /// Unknown / unsupported `f=` value. Parser captures it but
    /// `handle_apc` drops the payload silently.
    Other,
}

/// Transmission medium from `t=`. `Direct` is the base64-in-APC default;
/// `File` reads the image from a path the app supplies; `TempFile`
/// reads-then-deletes. `Other` (shared memory) is not yet implemented.
///
/// File reads are safe from a privilege standpoint — the app is already
/// running as the user; it could read the file directly. We just shift
/// the read across the PTY so chunked base64 of a multi-MB image
/// doesn't have to traverse the byte stream.
///
/// Temp-file is the path `kitty +kitten icat` prefers for large images:
/// one APC + one file read instead of ~250 chunked APCs, which closes
/// most of the perf gap with iTerm OSC 1337 for big payloads.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum KittyTransmission {
    Direct,
    File,
    /// `t=t` — read the file at the given path, then delete it. Per
    /// spec the terminal owns the unlink (apps may use unique temp
    /// filenames per send and assume they're cleaned up). We only
    /// delete files actually under `std::env::temp_dir()` as
    /// defense-in-depth against a malformed app pointing us at
    /// arbitrary paths.
    TempFile,
    /// `t=s` — POSIX shared memory. Payload is the SHM object name
    /// (typically `/something`); we `shm_open` + `mmap` to read,
    /// then `shm_unlink` per spec. Zero copies through the PTY for
    /// arbitrarily large images; what icat picks for big payloads
    /// when we advertise it. Unix-only.
    SharedMemory,
    Other,
}

/// Parsed Kitty graphics-protocol control data — the `key=value,...` part
/// between `_G` and the first `;`. Fields are public so the dispatcher
/// (`handle_apc`) can pattern-match without going through accessors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KittyControl {
    pub action: KittyAction,
    pub format: KittyFormat,
    pub transmission: KittyTransmission,
    /// `i=` image id. Client-assigned u32. When present and `m=1`, the
    /// payload chunk appends to a per-id buffer; when present and `m=0`
    /// (or omitted), it completes the buffer.
    pub image_id: Option<u32>,
    /// `p=` placement id. Lets a client refer to a specific placement
    /// later (e.g. for delete). Not yet acted on in K1.
    pub placement_id: Option<u32>,
    /// `c=` target column count for the placement. Treated as
    /// `ImageSizeSpec::Cells`.
    pub cells_cols: Option<u32>,
    /// `r=` target row count for the placement.
    pub cells_rows: Option<u32>,
    /// `m=1` means more chunks coming; `m=0` (or omitted) means this is
    /// the last chunk. The accumulator keys on `image_id` to thread
    /// chunks of the same image together.
    pub more_chunks: bool,
    /// `C=1` suppresses the post-placement cursor advance — Kitty's
    /// equivalent of iTerm's `doNotMoveCursor=1`.
    pub do_not_move_cursor: bool,
    /// `q=` quiet mode. `q=0` (default) — always reply, `q=1` —
    /// suppress success responses, `q=2` — suppress all. K1.5 honors this.
    pub quiet: u8,
    /// `s=` source pixel width. Required for raw RGB/RGBA formats so we
    /// know how to reshape the byte stream; ignored for PNG (the header
    /// supplies the dimensions).
    pub source_w: Option<u32>,
    /// `v=` source pixel height. Same story as `source_w`.
    pub source_h: Option<u32>,
    /// `d=` selector for the delete action. Only meaningful when
    /// `action == Delete`; ignored otherwise.
    pub delete_selector: Option<KittyDeleteSelector>,
    /// `U=1` — virtual placement mode. The transmission registers an
    /// image but creates no `Placement`. The app then writes
    /// `U+10EEEE` placeholder cells (with the image id encoded in
    /// the fg color) to position the image. nvim's `image.nvim` and
    /// other modern viewers default to this path.
    pub virtual_placement: bool,
    /// `X=` — sub-cell pixel x-offset within the anchor cell. Lets
    /// apps position images at sub-cell precision. Plumbed straight
    /// to `Placement::pixel_offset.0`.
    pub pixel_offset_x: Option<u32>,
    /// `Y=` — sub-cell pixel y-offset within the anchor cell.
    pub pixel_offset_y: Option<u32>,
    /// `z=` — z-index for tie-breaking overlapping placements.
    /// Higher draws later (on top). Can be negative (per the Kitty
    /// spec — apps use negative z to put images "behind" text).
    pub z_index: Option<i32>,
    /// `x=` — source crop start, x in image pixels. Together with
    /// `y=` / `w=` / `h=` selects a sub-rect of the image to draw.
    pub crop_x: Option<u32>,
    /// `y=` — source crop start, y in image pixels.
    pub crop_y: Option<u32>,
    /// `w=` — source crop width in pixels. Independent from `c=`
    /// (target column count); `w=` is about WHICH pixels to draw,
    /// `c=` is about HOW MANY CELLS they cover on screen.
    pub crop_w: Option<u32>,
    /// `h=` — source crop height in pixels.
    pub crop_h: Option<u32>,
    /// `o=z` — zlib-compressed payload. After base64 decode (or file
    /// read), the bytes need to be inflated before being treated as
    /// image data.
    pub compressed_zlib: bool,
    /// Animation `r=` alias — frame number to operate on (1-based).
    /// `a=f`: which existing frame slot to replace (0 / missing =
    /// append). `a=a`: which frame to edit (for gap / compose changes).
    /// Parsed from the same raw value as `cells_rows`; the dispatcher
    /// picks one based on the action.
    pub anim_frame_num: Option<u32>,
    /// `a=f c=N` — 1-based index of the source frame to use as the
    /// composition base for a new frame. Default (when missing / 0) is
    /// frame 1, i.e., the original image. Parsed from the same raw
    /// value as `cells_cols`.
    pub anim_compose_base: Option<u32>,
    /// `a=a c=N` — 1-based frame number to make current (display
    /// statically without playing). Same raw value as `cells_cols`.
    pub anim_make_current: Option<u32>,
    /// `a=a s=N` — playback control: 1 = stop, 2 = run while frames
    /// are still being added, 3 = run with `v=` loops. Parsed from
    /// the same raw value as `source_w`.
    pub anim_control: Option<u32>,
    /// `a=a v=N` — total loop count when `anim_control == Some(3)`.
    /// 0 means infinite. Same raw value as `source_h`.
    pub anim_loop_count: Option<u32>,
    /// `a=f z=N` / `a=a z=N` — gap in milliseconds before this frame
    /// advances to the next. Parsed from the same raw value as
    /// `z_index`; the dispatcher uses one or the other based on
    /// action.
    pub anim_gap_ms: Option<u32>,
}

impl Default for KittyControl {
    fn default() -> Self {
        Self {
            // Spec default when `a=` is absent.
            action: KittyAction::TransmitAndDisplay,
            // PNG when `f=` is absent.
            format: KittyFormat::Png,
            // Direct when `t=` is absent.
            transmission: KittyTransmission::Direct,
            image_id: None,
            placement_id: None,
            cells_cols: None,
            cells_rows: None,
            more_chunks: false,
            do_not_move_cursor: false,
            quiet: 0,
            source_w: None,
            source_h: None,
            delete_selector: None,
            virtual_placement: false,
            pixel_offset_x: None,
            pixel_offset_y: None,
            z_index: None,
            crop_x: None,
            crop_y: None,
            crop_w: None,
            crop_h: None,
            compressed_zlib: false,
            anim_frame_num: None,
            anim_compose_base: None,
            anim_make_current: None,
            anim_control: None,
            anim_loop_count: None,
            anim_gap_ms: None,
        }
    }
}

/// In-flight Kitty chunked transmission. The accumulator on `Terminal`
/// holds one of these per `i=` image id between the first `m=1` chunk
/// and the eventual `m=0` (or omitted-`m`) chunk that completes it.
///
/// We accumulate the still-base64 strings rather than decoded bytes
/// because a chunk's base64 may not be a multiple of 4 — partial decodes
/// can't be combined trivially.
#[derive(Clone, Debug)]
struct KittyChunks {
    /// Concatenated base64 payload across all chunks so far. Decoded
    /// once at the terminal chunk.
    b64: String,
    /// Format and source dimensions from the *first* chunk — the Kitty
    /// spec says only the first chunk's attributes matter, so chunked
    /// raw RGB/RGBA payloads still know how to reshape their bytes.
    format: KittyFormat,
    source_w: Option<u32>,
    source_h: Option<u32>,
    /// Sizing / cursor params from the first chunk.
    cells_cols: Option<u32>,
    cells_rows: Option<u32>,
    do_not_move_cursor: bool,
    /// Client's `i=` / `p=` ids from the first chunk. Plumbed through
    /// to the eventual `Placement` so `a=d,d=i,…` / `a=d,d=p,…` can
    /// find it later. `None` when the client omitted the key.
    kitty_image_id: Option<u32>,
    kitty_placement_id: Option<u32>,
    /// `true` for `a=T`, `false` for `a=t`. Tells main.rs whether to
    /// create a `Placement` (display) or just register the image-id
    /// mapping (transmit-for-later).
    display_immediately: bool,
    /// `o=z` from the first chunk: payload should be zlib-inflated
    /// before normalizing. Set on the first chunk only (spec contract).
    compressed_zlib: bool,
    /// First-chunk animation-frame metadata. Only the first chunk
    /// carries `z=` (gap_ms), lowercase `x=` / `y=` (dst position),
    /// `r=` (target_slot), and `c=` (compose_base); continuation
    /// chunks omit them and the parser turns them into defaults.
    /// Stashing them here means the final-chunk finalize doesn't
    /// have to look at the LAST chunk's `ctrl` (which has all those
    /// fields zeroed) and accidentally turn every animation frame
    /// into a 0ms delay — symptom: animations run at thousands of
    /// fps because `delay_ms.max(1)` clamps to 1ms.
    anim_first_chunk: Option<KittyAnimationFrameSpec>,
}

/// Decode a Kitty virtual-placement image id from a cell's foreground
/// color. The Kitty spec packs the high 24 bits of the image id into
/// the truecolor RGB bytes:
///
/// - R (high 8 bits): bits 16..23 of the id
/// - G (mid  8 bits): bits  8..15 of the id
/// - B (low  8 bits): bits  0..7 of the id
///
/// The 4th byte (bits 24..31) comes from an optional 3rd diacritic on
/// the placeholder char, which we don't decode yet — so this is a
/// 24-bit id in practice. Most apps stay within that range.
///
/// Style colors are stored as linear-space `[f32; 4]`; we round-trip
/// through sRGB to recover the original u8 channels. `color_fg`
/// `None` (cell didn't explicitly set a fg color) returns `None`.
/// Sorted list of the 297 combining marks the Kitty Unicode-placeholder
/// protocol uses to encode `(row, column, image-id-high-byte)` after
/// each `U+10EEEE` cell. The lookup `kitty_placeholder_diacritic_index`
/// binary-searches this table and returns the codepoint's index — that
/// index IS the encoded value (0..=296). The set is copied verbatim
/// from kitty's `rowcolumn_diacritics.txt`; do not reorder.
const KITTY_PLACEHOLDER_DIACRITICS: &[char] = &[
    '\u{0305}', '\u{030D}', '\u{030E}', '\u{0310}', '\u{0312}', '\u{033D}', '\u{033E}',
    '\u{033F}', '\u{0346}', '\u{034A}', '\u{034B}', '\u{034C}', '\u{0350}', '\u{0351}',
    '\u{0352}', '\u{0357}', '\u{035B}', '\u{0363}', '\u{0364}', '\u{0365}', '\u{0366}',
    '\u{0367}', '\u{0368}', '\u{0369}', '\u{036A}', '\u{036B}', '\u{036C}', '\u{036D}',
    '\u{036E}', '\u{036F}', '\u{0483}', '\u{0484}', '\u{0485}', '\u{0486}', '\u{0487}',
    '\u{0592}', '\u{0593}', '\u{0594}', '\u{0595}', '\u{0597}', '\u{0598}', '\u{0599}',
    '\u{059C}', '\u{059D}', '\u{059E}', '\u{059F}', '\u{05A0}', '\u{05A1}', '\u{05A8}',
    '\u{05A9}', '\u{05AB}', '\u{05AC}', '\u{05AF}', '\u{05C4}', '\u{0610}', '\u{0611}',
    '\u{0612}', '\u{0613}', '\u{0614}', '\u{0615}', '\u{0616}', '\u{0617}', '\u{0657}',
    '\u{0658}', '\u{0659}', '\u{065A}', '\u{065B}', '\u{065D}', '\u{065E}', '\u{06D6}',
    '\u{06D7}', '\u{06D8}', '\u{06D9}', '\u{06DA}', '\u{06DB}', '\u{06DC}', '\u{06DF}',
    '\u{06E0}', '\u{06E1}', '\u{06E2}', '\u{06E4}', '\u{06E7}', '\u{06E8}', '\u{06EB}',
    '\u{06EC}', '\u{0730}', '\u{0732}', '\u{0733}', '\u{0735}', '\u{0736}', '\u{073A}',
    '\u{073D}', '\u{073F}', '\u{0740}', '\u{0741}', '\u{0743}', '\u{0745}', '\u{0747}',
    '\u{0749}', '\u{074A}', '\u{07EB}', '\u{07EC}', '\u{07ED}', '\u{07EE}', '\u{07EF}',
    '\u{07F0}', '\u{07F1}', '\u{07F3}', '\u{0816}', '\u{0817}', '\u{0818}', '\u{0819}',
    '\u{081B}', '\u{081C}', '\u{081D}', '\u{081E}', '\u{081F}', '\u{0820}', '\u{0821}',
    '\u{0822}', '\u{0823}', '\u{0825}', '\u{0826}', '\u{0827}', '\u{0829}', '\u{082A}',
    '\u{082B}', '\u{082C}', '\u{082D}', '\u{0951}', '\u{0953}', '\u{0954}', '\u{0F82}',
    '\u{0F83}', '\u{0F86}', '\u{0F87}', '\u{135D}', '\u{135E}', '\u{135F}', '\u{17DD}',
    '\u{193A}', '\u{1A17}', '\u{1A75}', '\u{1A76}', '\u{1A77}', '\u{1A78}', '\u{1A79}',
    '\u{1A7A}', '\u{1A7B}', '\u{1A7C}', '\u{1B6B}', '\u{1B6D}', '\u{1B6E}', '\u{1B6F}',
    '\u{1B70}', '\u{1B71}', '\u{1B72}', '\u{1B73}', '\u{1CD0}', '\u{1CD1}', '\u{1CD2}',
    '\u{1CDA}', '\u{1CDB}', '\u{1CE0}', '\u{1DC0}', '\u{1DC1}', '\u{1DC3}', '\u{1DC4}',
    '\u{1DC5}', '\u{1DC6}', '\u{1DC7}', '\u{1DC8}', '\u{1DC9}', '\u{1DCB}', '\u{1DCC}',
    '\u{1DD1}', '\u{1DD2}', '\u{1DD3}', '\u{1DD4}', '\u{1DD5}', '\u{1DD6}', '\u{1DD7}',
    '\u{1DD8}', '\u{1DD9}', '\u{1DDA}', '\u{1DDB}', '\u{1DDC}', '\u{1DDD}', '\u{1DDE}',
    '\u{1DDF}', '\u{1DE0}', '\u{1DE1}', '\u{1DE2}', '\u{1DE3}', '\u{1DE4}', '\u{1DE5}',
    '\u{1DE6}', '\u{1DFE}', '\u{20D0}', '\u{20D1}', '\u{20D4}', '\u{20D5}', '\u{20D6}',
    '\u{20D7}', '\u{20DB}', '\u{20DC}', '\u{20E1}', '\u{20E7}', '\u{20E9}', '\u{20F0}',
    '\u{2CEF}', '\u{2CF0}', '\u{2CF1}', '\u{2DE0}', '\u{2DE1}', '\u{2DE2}', '\u{2DE3}',
    '\u{2DE4}', '\u{2DE5}', '\u{2DE6}', '\u{2DE7}', '\u{2DE8}', '\u{2DE9}', '\u{2DEA}',
    '\u{2DEB}', '\u{2DEC}', '\u{2DED}', '\u{2DEE}', '\u{2DEF}', '\u{2DF0}', '\u{2DF1}',
    '\u{2DF2}', '\u{2DF3}', '\u{2DF4}', '\u{2DF5}', '\u{2DF6}', '\u{2DF7}', '\u{2DF8}',
    '\u{2DF9}', '\u{2DFA}', '\u{2DFB}', '\u{2DFC}', '\u{2DFD}', '\u{2DFE}', '\u{2DFF}',
    '\u{A66F}', '\u{A67C}', '\u{A67D}', '\u{A6F0}', '\u{A6F1}', '\u{A8E0}', '\u{A8E1}',
    '\u{A8E2}', '\u{A8E3}', '\u{A8E4}', '\u{A8E5}', '\u{A8E6}', '\u{A8E7}', '\u{A8E8}',
    '\u{A8E9}', '\u{A8EA}', '\u{A8EB}', '\u{A8EC}', '\u{A8ED}', '\u{A8EE}', '\u{A8EF}',
    '\u{A8F0}', '\u{A8F1}', '\u{AAB0}', '\u{AAB2}', '\u{AAB3}', '\u{AAB7}', '\u{AAB8}',
    '\u{AABE}', '\u{AABF}', '\u{AAC1}', '\u{FE20}', '\u{FE21}', '\u{FE22}', '\u{FE23}',
    '\u{FE24}', '\u{FE25}', '\u{FE26}', '\u{10A0F}', '\u{10A38}', '\u{1D185}', '\u{1D186}',
    '\u{1D187}', '\u{1D188}', '\u{1D189}', '\u{1D1AA}', '\u{1D1AB}', '\u{1D1AC}',
    '\u{1D1AD}', '\u{1D242}', '\u{1D243}', '\u{1D244}',
];

/// Look up a codepoint in the Kitty placeholder diacritic table; the
/// returned index is the encoded `(row | column | id-high-byte)` value.
/// Returns `None` for any non-diacritic — caller treats that as "end
/// of the placeholder's trailing diacritic run".
pub(crate) fn kitty_placeholder_diacritic_index(ch: char) -> Option<u32> {
    KITTY_PLACEHOLDER_DIACRITICS
        .binary_search(&ch)
        .ok()
        .map(|i| i as u32)
}

fn decode_kitty_placeholder_image_id(style: &crate::style::Style) -> Option<u32> {
    let fg = style.color_fg?;
    let r = crate::palette::linear_to_srgb_u8(fg[0]) as u32;
    let g = crate::palette::linear_to_srgb_u8(fg[1]) as u32;
    let b = crate::palette::linear_to_srgb_u8(fg[2]) as u32;
    let id = (r << 16) | (g << 8) | b;
    // id == 0 is the "no id" sentinel — the placeholder has no
    // meaningful image to reference. Treat as absent.
    if id == 0 {
        return None;
    }
    Some(id)
}

/// Decode the base64-encoded UTF-8 filesystem path that file-based Kitty
/// transmissions (`t=f` / `t=t`) carry in their payload. Returns `None`
/// on bad base64 or non-UTF-8 bytes.
fn decode_kitty_file_path(payload: &str) -> Option<std::path::PathBuf> {
    use base64::Engine;
    // Strip ASCII whitespace — apps may wrap base64 lines for readability.
    let cleaned: String = payload.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    let raw = base64::engine::general_purpose::STANDARD
        .decode(cleaned.as_bytes())
        .ok()?;
    let s = std::str::from_utf8(&raw).ok()?;
    Some(std::path::PathBuf::from(s))
}

/// Read a file at `path` with a 256 MiB cap. Guards against an app
/// pointing us at `/dev/zero` or a multi-GB log file. Oversized or
/// unreadable files return `None`.
fn read_kitty_file(path: &std::path::Path) -> Option<Vec<u8>> {
    const MAX_FILE_READ_BYTES: u64 = 256 * 1024 * 1024;
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() > MAX_FILE_READ_BYTES {
        return None;
    }
    std::fs::read(path).ok()
}

/// Decode the base64-encoded UTF-8 POSIX SHM object name from a `t=s`
/// payload. Names are short (typically `/icat-<random>`), so we apply
/// the same base64-with-whitespace tolerance the file-path decoder uses.
fn decode_kitty_shm_name(payload: &str) -> Option<String> {
    use base64::Engine;
    let cleaned: String = payload.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    let raw = base64::engine::general_purpose::STANDARD
        .decode(cleaned.as_bytes())
        .ok()?;
    String::from_utf8(raw).ok()
}

/// Open a POSIX shared-memory object by name, mmap it, copy out the
/// contents, and tear down. Same 256 MiB cap the file paths use guards
/// against malicious senders. Returns `None` on any syscall failure —
/// the caller still attempts `shm_unlink` so a partially-set-up object
/// doesn't leak.
#[cfg(unix)]
fn read_kitty_shm(name: &str) -> Option<Vec<u8>> {
    use std::ffi::CString;
    use std::ptr;
    const MAX_BYTES: usize = 256 * 1024 * 1024;
    let c_name = CString::new(name).ok()?;
    unsafe {
        let fd = libc::shm_open(c_name.as_ptr(), libc::O_RDONLY, 0);
        if fd < 0 {
            return None;
        }
        let mut st: libc::stat = std::mem::zeroed();
        if libc::fstat(fd, &mut st) < 0 {
            libc::close(fd);
            return None;
        }
        let size = st.st_size as usize;
        if size == 0 || size > MAX_BYTES {
            libc::close(fd);
            return None;
        }
        let p = libc::mmap(
            ptr::null_mut(),
            size,
            libc::PROT_READ,
            libc::MAP_SHARED,
            fd,
            0,
        );
        if p == libc::MAP_FAILED {
            libc::close(fd);
            return None;
        }
        let bytes = std::slice::from_raw_parts(p as *const u8, size).to_vec();
        libc::munmap(p, size);
        libc::close(fd);
        Some(bytes)
    }
}

/// Best-effort `shm_unlink`. Kitty spec says the terminal owns the
/// unlink, so we always try — silently swallow failures because a
/// double-unlink (or unlink of a name we never opened because read
/// failed) isn't worth surfacing.
#[cfg(unix)]
fn unlink_kitty_shm(name: &str) {
    use std::ffi::CString;
    if let Ok(c_name) = CString::new(name) {
        unsafe {
            libc::shm_unlink(c_name.as_ptr());
        }
    }
}

/// True when `path` resolves under `std::env::temp_dir()`. Used by `t=t`
/// to gate file deletion — we'll read any path the app gives us (it
/// could read the file itself anyway), but only delete inside the temp
/// hierarchy. Returns false if either side fails to canonicalize
/// (path doesn't exist, permission denied, symlink loop) — safer than
/// guessing.
fn path_is_under_temp_dir(path: &std::path::Path) -> bool {
    let Ok(c_path) = std::fs::canonicalize(path) else { return false };
    let Ok(c_temp) = std::fs::canonicalize(std::env::temp_dir()) else { return false };
    c_path.starts_with(&c_temp)
}

/// Append `payload`'s base64 chars to `dest`, skipping ASCII whitespace.
/// Some apps wrap base64 inside an APC at 76 chars per line for
/// readability; the base64 alphabet doesn't include whitespace, so the
/// skip is unambiguous.
///
/// Pulled out of `handle_apc_direct` because it runs on the hot path —
/// a typical 1MB image arrives in ~250 chunks and the old `chunk:
/// String = ... .collect()` path allocated a fresh String per chunk
/// then copied it into the accumulator. Streaming directly into the
/// destination is one pass, no allocation.
fn append_b64_filtered(dest: &mut String, payload: &str) {
    // Base64 is pure ASCII so we can byte-iterate without UTF-8 decoding.
    // Reserving up-front amortizes the growth cost across many chunks.
    dest.reserve(payload.len());
    for &b in payload.as_bytes() {
        if !b.is_ascii_whitespace() {
            // SAFETY-equivalent: pushing an ASCII byte into a String is
            // always valid UTF-8.
            dest.push(b as char);
        }
    }
}

/// Extract the placement-side fields (`X=`/`Y=` pixel offset, `z=`
/// z-index, `x=`/`y=`/`w=`/`h=` source crop) from a `KittyControl` in
/// the format `finalize_kitty_image_bytes` and `insert_placement_kitty`
/// expect. Centralizes the defaults so all dispatch sites agree.
/// Lift the per-frame metadata out of a `KittyControl` for `a=f`.
/// Single-chunk callers and the first-chunk capture in the chunked
/// path share this — the chunked finalize then plays back the
/// stashed spec rather than re-reading the last chunk's `ctrl`
/// (which has these fields zeroed because icat omits them on
/// continuations).
fn kitty_anim_frame_spec_from_ctrl(ctrl: &KittyControl) -> KittyAnimationFrameSpec {
    KittyAnimationFrameSpec {
        target_slot: ctrl.anim_frame_num,
        compose_base: ctrl.anim_compose_base.filter(|&n| n > 0),
        gap_ms: ctrl.anim_gap_ms.unwrap_or(0),
        // For `a=f`, the Kitty spec overloads lowercase `x=` and
        // `y=` (normally source-crop on `a=T`) as the destination
        // top-left within the parent frame. The parser populated
        // them into `crop_x` / `crop_y`; capital `X=`/`Y=` aren't
        // defined for `a=f`. Fall back to `pixel_offset_x/y` only if
        // the lowercase pair is absent, for forward-compat with apps
        // that pick the opposite convention.
        dst_x: ctrl.crop_x.or(ctrl.pixel_offset_x).unwrap_or(0),
        dst_y: ctrl.crop_y.or(ctrl.pixel_offset_y).unwrap_or(0),
    }
}

fn kitty_placement_params(
    ctrl: &KittyControl,
) -> ((i32, i32), i32, Option<(u32, u32, u32, u32)>) {
    let pixel_offset = (
        ctrl.pixel_offset_x.unwrap_or(0).min(i32::MAX as u32) as i32,
        ctrl.pixel_offset_y.unwrap_or(0).min(i32::MAX as u32) as i32,
    );
    let z_index = ctrl.z_index.unwrap_or(0);
    // A crop rect requires all four x/y/w/h. Partial sets are ignored
    // (per spec — defining only w but not x is undefined). w=0 / h=0
    // also fall through to None since a zero-size crop is degenerate.
    let src_rect = match (ctrl.crop_x, ctrl.crop_y, ctrl.crop_w, ctrl.crop_h) {
        (Some(x), Some(y), Some(w), Some(h)) if w > 0 && h > 0 => Some((x, y, w, h)),
        _ => None,
    };
    (pixel_offset, z_index, src_rect)
}

/// Resolve the effective pixel format for any Kitty raw payload —
/// base image (`a=T` / `a=t`) or animation frame (`a=f`).
///
/// `kitten icat` is loose about `f=` on transmissions: it sends
/// `f=24` (RGB) on some payloads, `f=32` (RGBA) on others, and omits
/// `f=` entirely on a meaningful fraction. The parser turns omitted
/// `f=` into the PNG default (the sentinel for "not specified"),
/// which is wrong for any raw GIF payload. Resolution order:
///
///   1. Explicit non-PNG from the parser → trust it.
///   2. PNG signature in the raw bytes → really is PNG.
///   3. Source dims + raw byte count match RGB or RGBA within one
///      page (16 KB on macOS SHM padding) → use that format.
///   4. Source dims set but raw byte count sits in the gap between
///      RGB and RGBA exact-match windows → assume the larger format
///      it's still big enough for, so a frame whose inflated size is
///      slightly off doesn't get misrouted to the PNG decoder.
///      Closes "image decode failed: The image format could not be
///      determined" on kitty animations where one frame's inflated
///      length falls between `w*h*3 + PAGE` and `w*h*4`.
///   5. `fallback` (recorded base format for `a=f` callers, `None`
///      for base-image callers) → use it.
///   6. Hand back the parser's view — `normalize_kitty_payload` will
///      reject if it can't decode.
pub(crate) fn resolve_kitty_format(
    parsed_format: KittyFormat,
    raw: &[u8],
    source_w: Option<u32>,
    source_h: Option<u32>,
    fallback: Option<KittyFormat>,
) -> KittyFormat {
    if !matches!(parsed_format, KittyFormat::Png) {
        return parsed_format;
    }
    const PNG_SIG: &[u8] = &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
    if raw.starts_with(PNG_SIG) {
        return KittyFormat::Png;
    }
    if let (Some(w), Some(h)) = (source_w, source_h) {
        const PAGE: usize = 16 * 1024;
        let pixels = (w as usize).saturating_mul(h as usize);
        let rgb_bytes = pixels.saturating_mul(3);
        let rgba_bytes = pixels.saturating_mul(4);
        if raw.len() >= rgba_bytes && raw.len() < rgba_bytes + PAGE {
            return KittyFormat::Rgba;
        }
        if raw.len() >= rgb_bytes && raw.len() < rgb_bytes + PAGE {
            return KittyFormat::Rgb;
        }
        if raw.len() >= rgba_bytes {
            return KittyFormat::Rgba;
        }
        // Mid-gap: between RGB and RGBA exact-match windows. Prefer
        // the base format if known (frames inherit per spec); else
        // fall through to RGB so the raw-bypass path can run rather
        // than handing the bytes to the PNG decoder, which has no
        // chance of recognizing the format.
        if raw.len() >= rgb_bytes {
            if let Some(fb) = fallback {
                if matches!(fb, KittyFormat::Rgb | KittyFormat::Rgba) {
                    return fb;
                }
            }
            return KittyFormat::Rgb;
        }
    }
    fallback.unwrap_or(parsed_format)
}

/// Inflate a zlib-compressed payload (`o=z` in the Kitty control
/// data). Returns `None` on malformed input or if the inflated size
/// would exceed the same 256 MiB cap the file readers use — guards
/// against zip-bomb-style attacks where a small APC payload inflates
/// to gigabytes.
fn inflate_kitty_zlib(compressed: &[u8]) -> Option<Vec<u8>> {
    use flate2::read::ZlibDecoder;
    use std::io::Read;
    const MAX_INFLATED_BYTES: usize = 256 * 1024 * 1024;
    let mut dec = ZlibDecoder::new(compressed);
    let mut out = Vec::new();
    // `read_to_end` will pull until the decoder reports EOF. We
    // can't bound it via a stock reader, so check after the read.
    dec.read_to_end(&mut out).ok()?;
    if out.len() > MAX_INFLATED_BYTES {
        return None;
    }
    Some(out)
}

/// Normalize a Kitty graphics payload into the PNG-bytes format the
/// `image::load_from_memory` decoder accepts.
///
/// - PNG payloads pass through unchanged, with `pixel_size` from the
///   PNG header peek.
/// - Raw RGB (`f=24`) and RGBA (`f=32`) payloads get PNG-encoded here.
///   The encode runs on the PTY thread (a few ms for typical images);
///   the alternative would be plumbing a "skip decode, here are raw
///   pixels" code path through the worker, which doubles the API
///   surface for one rare case.
///
/// Returns `None` when raw formats are missing required `s=` / `v=`
/// dimensions, when raw byte counts don't match the declared geometry,
/// or when PNG re-encoding fails.
fn normalize_kitty_payload(
    format: KittyFormat,
    raw: &[u8],
    source_w: Option<u32>,
    source_h: Option<u32>,
) -> Option<(Vec<u8>, Option<(u32, u32)>)> {
    match format {
        KittyFormat::Png => {
            let pixel_size = crate::images::peek_dimensions(raw);
            Some((raw.to_vec(), pixel_size))
        }
        KittyFormat::Rgb | KittyFormat::Rgba => {
            let w = source_w?;
            let h = source_h?;
            let bytes_per_px = if matches!(format, KittyFormat::Rgba) { 4 } else { 3 };
            let expected = (w as usize)
                .checked_mul(h as usize)?
                .checked_mul(bytes_per_px)?;
            // Accept oversized buffers and truncate — POSIX SHM
            // segments on macOS round up to page size, so a 109-byte
            // payload arrives as 4096 bytes with zero padding. The
            // declared `s=`/`v=` is the truth; trust them. Reject
            // only when we got LESS than declared.
            if raw.len() < expected {
                return None;
            }
            let trimmed: Vec<u8> = raw[..expected].to_vec();
            let dyn_img = match format {
                KittyFormat::Rgba => image::DynamicImage::ImageRgba8(
                    image::RgbaImage::from_raw(w, h, trimmed)?,
                ),
                KittyFormat::Rgb => image::DynamicImage::ImageRgb8(
                    image::RgbImage::from_raw(w, h, trimmed)?,
                ),
                _ => unreachable!(),
            };
            let mut out = Vec::new();
            dyn_img
                .write_to(
                    &mut std::io::Cursor::new(&mut out),
                    image::ImageOutputFormat::Png,
                )
                .ok()?;
            Some((out, Some((w, h))))
        }
        KittyFormat::Other => None,
    }
}

/// Pick the right payload shape to hand to the store: raw RGBA
/// (bypass the decode worker) for `f=24`/`f=32` payloads, or
/// PNG-or-equivalent bytes (route through the worker) for PNG
/// inputs. Centralizes the format-conditional dispatch so every
/// transmission entry point gets the same fast-path treatment.
///
/// Returns `(bytes, pixel_size, raw_rgba_dims)`:
///   - `bytes` — payload to put on the `PendingImageUpload`.
///   - `pixel_size` — natural pixel dims (`source_w`/`h` for raw,
///     PNG header peek otherwise).
///   - `raw_rgba_dims` — `Some((w, h))` when `bytes` is already raw
///     RGBA (signals the worker-bypass insert path); `None` for the
///     decode-worker path.
fn prepare_kitty_payload(
    format: KittyFormat,
    raw: &[u8],
    source_w: Option<u32>,
    source_h: Option<u32>,
) -> Option<(Vec<u8>, Option<(u32, u32)>, Option<(u32, u32)>)> {
    if matches!(format, KittyFormat::Rgb | KittyFormat::Rgba) {
        let (rgba, w, h) = convert_kitty_raw_to_rgba(format, raw, source_w, source_h)?;
        return Some((rgba, Some((w, h)), Some((w, h))));
    }
    let (bytes, pixel_size) = normalize_kitty_payload(format, raw, source_w, source_h)?;
    Some((bytes, pixel_size, None))
}

/// Convert a raw `f=24` (RGB) or `f=32` (RGBA) Kitty payload into a
/// straight-alpha RGBA byte buffer the GPU pipeline can upload
/// directly. Returns `(rgba, width, height)`. For PNG inputs the
/// caller should still route through `normalize_kitty_payload` plus
/// the decode worker — re-implementing PNG decode here would just
/// move the worker's job into the dispatcher.
///
/// Skipping the PNG round-trip is the whole point of this path: a
/// 450x450 RGBA payload PNG-encodes in ~50-150 ms (zlib is slow on
/// noisy GIF deltas), and serializing twenty of those in a single
/// PTY chunk busts the worker's per-job decode timeout. RGB → RGBA
/// padding here is a plain memcpy with alpha=255 — single-digit ms
/// even for full-screen frames.
pub(crate) fn convert_kitty_raw_to_rgba(
    format: KittyFormat,
    raw: &[u8],
    source_w: Option<u32>,
    source_h: Option<u32>,
) -> Option<(Vec<u8>, u32, u32)> {
    let w = source_w?;
    let h = source_h?;
    let pixels = (w as usize).checked_mul(h as usize)?;
    match format {
        KittyFormat::Rgba => {
            let expected = pixels.checked_mul(4)?;
            if raw.len() < expected {
                return None;
            }
            Some((raw[..expected].to_vec(), w, h))
        }
        KittyFormat::Rgb => {
            let expected = pixels.checked_mul(3)?;
            if raw.len() < expected {
                return None;
            }
            let mut out = Vec::with_capacity(pixels.checked_mul(4)?);
            for px in raw[..expected].chunks_exact(3) {
                out.extend_from_slice(&[px[0], px[1], px[2], 0xFF]);
            }
            Some((out, w, h))
        }
        _ => None,
    }
}

/// Parse the comma-separated `key=value` portion of a Kitty graphics
/// control sequence. Unknown keys are accepted-but-ignored per the Kitty
/// contract; malformed value parses fall back to the field's default.
/// Returns `None` only when the input doesn't look like a control list
/// at all (which can't happen via `handle_apc`'s split, but the guard
/// keeps the helper testable in isolation).
pub fn parse_kitty_control(s: &str) -> Option<KittyControl> {
    let mut ctrl = KittyControl::default();
    if s.is_empty() {
        return Some(ctrl);
    }
    for kv in s.split(',') {
        let Some((k, v)) = kv.split_once('=') else { continue };
        match k {
            "a" => ctrl.action = match v {
                "t" => KittyAction::Transmit,
                "T" => KittyAction::TransmitAndDisplay,
                "q" => KittyAction::Query,
                "p" => KittyAction::Place,
                "d" => KittyAction::Delete,
                "f" => KittyAction::AnimationFrame,
                "a" => KittyAction::AnimationControl,
                _ => KittyAction::Other,
            },
            "d" => ctrl.delete_selector = Some(match v {
                "a" | "A" => KittyDeleteSelector::All,
                "i" | "I" => KittyDeleteSelector::Image,
                "p" | "P" => KittyDeleteSelector::Placement,
                _ => KittyDeleteSelector::Other,
            }),
            "f" => ctrl.format = match v {
                "100" => KittyFormat::Png,
                "24" => KittyFormat::Rgb,
                "32" => KittyFormat::Rgba,
                _ => KittyFormat::Other,
            },
            "s" => {
                // `s=` is overloaded: source pixel width for image /
                // frame transmission, OR the playback-state code for
                // `a=a`. Parse into both — the dispatcher picks based
                // on the action.
                ctrl.source_w = v.parse().ok();
                ctrl.anim_control = v.parse().ok();
            }
            "v" => {
                // `v=` is overloaded: source pixel height for image /
                // frame transmission, OR the loop count for `a=a s=3`.
                ctrl.source_h = v.parse().ok();
                ctrl.anim_loop_count = v.parse().ok();
            }
            "t" => ctrl.transmission = match v {
                "d" => KittyTransmission::Direct,
                "f" => KittyTransmission::File,
                "t" => KittyTransmission::TempFile,
                "s" => KittyTransmission::SharedMemory,
                _ => KittyTransmission::Other,
            },
            "i" => ctrl.image_id = v.parse().ok(),
            "I" => {
                // `I=` is the Kitty "image number" — clients use it
                // when they want the terminal to assign the real
                // image id and reply. icat sends I= for the base and
                // for every a=f frame, never using lowercase i= for
                // transmission. We don't currently implement the
                // number→id reply protocol; instead we treat the
                // number as the identifier directly. That makes
                // `a=p,I=N` / `a=f,I=N` / `a=d,I=N` resolve through
                // the same `kitty_image_ids` map as `i=N` would.
                // If both keys are present, `i=` wins (it's the
                // explicit client-managed id).
                if ctrl.image_id.is_none() {
                    ctrl.image_id = v.parse().ok();
                }
            }
            "p" => ctrl.placement_id = v.parse().ok(),
            "c" => {
                // Overloaded: target column count for placement; OR
                // the compose-base frame number for `a=f`; OR the
                // make-current frame number for `a=a`.
                ctrl.cells_cols = v.parse().ok();
                ctrl.anim_compose_base = v.parse().ok();
                ctrl.anim_make_current = v.parse().ok();
            }
            "r" => {
                // Overloaded: target row count for placement; OR the
                // frame slot to operate on for `a=f` / `a=a`.
                ctrl.cells_rows = v.parse().ok();
                ctrl.anim_frame_num = v.parse().ok();
            }
            "m" => ctrl.more_chunks = v == "1",
            "C" => ctrl.do_not_move_cursor = v == "1",
            "q" => ctrl.quiet = v.parse().unwrap_or(0),
            "U" => ctrl.virtual_placement = v == "1",
            "X" => ctrl.pixel_offset_x = v.parse().ok(),
            "Y" => ctrl.pixel_offset_y = v.parse().ok(),
            "z" => {
                // Overloaded: signed z-index for placement; OR the
                // unsigned per-frame gap in milliseconds for `a=f` /
                // `a=a`. Negative `z=` makes no sense as a gap so the
                // animation path silently drops those.
                ctrl.z_index = v.parse().ok();
                ctrl.anim_gap_ms = v.parse().ok();
            }
            "x" => ctrl.crop_x = v.parse().ok(),
            "y" => ctrl.crop_y = v.parse().ok(),
            "w" => ctrl.crop_w = v.parse().ok(),
            "h" => ctrl.crop_h = v.parse().ok(),
            "o" => ctrl.compressed_zlib = v == "z",
            // s, v, x, y, w, h, X, Y, z, I, o, etc. — accepted but unused
            // in K1. Future slices wire them up.
            _ => {}
        }
    }
    Some(ctrl)
}

fn parse_iterm_size(s: &str) -> Option<ImageSizeSpec> {
    if s.eq_ignore_ascii_case("auto") || s.is_empty() {
        return Some(ImageSizeSpec::Auto);
    }
    if let Some(num) = s.strip_suffix("px") {
        return num.parse::<u32>().ok().map(ImageSizeSpec::Pixels);
    }
    if let Some(num) = s.strip_suffix('%') {
        return num.parse::<u32>().ok().map(ImageSizeSpec::Percent);
    }
    // Default unit is cells. u16 because Cells max ~screen columns; a
    // higher value would still be safely clamped to grid bounds later.
    s.parse::<u16>().ok().map(ImageSizeSpec::Cells)
}

fn hex_decode_ascii(s: &str) -> Option<String> {
    if s.is_empty() || s.len() % 2 != 0 {
        return None;
    }
    let mut out = String::with_capacity(s.len() / 2);
    for chunk in s.as_bytes().chunks(2) {
        let hi = (chunk[0] as char).to_digit(16)?;
        let lo = (chunk[1] as char).to_digit(16)?;
        out.push(((hi << 4) | lo) as u8 as char);
    }
    Some(out)
}

/// Hex-encode an ASCII string in the form XTGETTCAP expects (lowercase).
fn hex_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.as_bytes() {
        out.push_str(&format!("{:02x}", b));
    }
    out
}

/// Look up a termcap / terminfo capability value. We answer the subset that
/// vim and similar editors actually probe — colors, key sequences, and a
/// handful of terminal flags. Both two-letter (termcap) and long-form
/// (terminfo) names are accepted; values mirror what we *actually* emit, so
/// there's no risk of advertising support we don't implement.
fn termcap_value(name: &str) -> Option<&'static str> {
    Some(match name {
        // Terminal identification & capabilities.
        "TN" | "name" => "xterm-256color",
        "Co" | "colors" => "256",
        "RGB" | "Tc" => "8",
        "bce" => "",
        // Cursor keys (CSI form — apps that care about app-cursor mode read
        // smkx/rmkx separately and switch their own buffers).
        "ku" | "kcuu1" => "\x1b[A",
        "kd" | "kcud1" => "\x1b[B",
        "kr" | "kcuf1" => "\x1b[C",
        "kl" | "kcub1" => "\x1b[D",
        // Navigation cluster.
        "kh" | "khome" => "\x1b[H",
        "@7" | "kend" => "\x1b[F",
        "kP" | "kpp" => "\x1b[5~",
        "kN" | "knp" => "\x1b[6~",
        "kI" | "kich1" => "\x1b[2~",
        "kD" | "kdch1" => "\x1b[3~",
        "kb" | "kbs" => "\x7f",
        "kB" | "kcbt" => "\x1b[Z",
        // Function keys.
        "k1" | "kf1" => "\x1bOP",
        "k2" | "kf2" => "\x1bOQ",
        "k3" | "kf3" => "\x1bOR",
        "k4" | "kf4" => "\x1bOS",
        "k5" | "kf5" => "\x1b[15~",
        "k6" | "kf6" => "\x1b[17~",
        "k7" | "kf7" => "\x1b[18~",
        "k8" | "kf8" => "\x1b[19~",
        "k9" | "kf9" => "\x1b[20~",
        "k;" | "kf10" => "\x1b[21~",
        "F1" | "kf11" => "\x1b[23~",
        "F2" | "kf12" => "\x1b[24~",
        // Shifted nav (xterm modifier-encoded forms — shift = param 2).
        "#2" | "kHOM" => "\x1b[1;2H",
        "*7" | "kEND" => "\x1b[1;2F",
        "#4" | "kLFT" => "\x1b[1;2D",
        "%i" | "kRIT" => "\x1b[1;2C",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Place a 1×1 image with default fields and return its id. Most tests
    /// care only about anchor row/col + extent, not the id or z; this keeps
    /// the test bodies focused.
    fn place(t: &mut Terminal, image: u32, row: isize, col: isize, rows: u16, cols: u16) -> u32 {
        t.insert_placement(ImageId(image), row, col, rows, cols, 0)
    }

    fn live_anchors(t: &Terminal) -> Vec<(u32, isize, isize, u16, u16)> {
        t.live_placements()
            .iter()
            .map(|p| (p.image.0, p.top_row, p.left_col, p.rows, p.cols))
            .collect()
    }

    #[test]
    fn sgr_then_print_persists_color_source_on_cell() {
        // Pins the writer-path invariant `reresolve_palette_updates_*`
        // depends on: the printed cell must carry the cursor's style,
        // including its `ColorSource`. A regression here would let the
        // reresolve path appear to fail for unrelated reasons.
        let _guard = crate::palette::TEST_LOCK.lock().expect("test lock");
        crate::palette::install(crate::palette::Palette::defaults());
        let mut t = Terminal::new(5, 3, 10);
        t.feed("\x1b[31mA");
        let c = t.primary.get(0, 0);
        assert_eq!(c.ch, 'A');
        assert_eq!(c.style.color_fg_source, crate::style::ColorSource::Indexed(1));
        assert!(c.style.color_fg.is_some());
    }

    #[test]
    fn reresolve_palette_updates_all_grids_and_scrollback() {
        use crate::palette;
        use crate::style::ColorSource;
        // Serialize against other palette-touching tests; install is
        // global state.
        let _guard = palette::TEST_LOCK.lock().expect("test lock");
        // Set palette to a known baseline so the test isn't sensitive
        // to whatever palette the previous test left installed.
        palette::install(palette::Palette::defaults());
        // 10×5 grid with generous headroom so the paints below don't
        // trigger autowrap/scroll into scrollback unintentionally.
        // The scrollback case is exercised explicitly further down.
        let mut t = Terminal::new(10, 5, 10);

        // Paint cell (0, 0) with red fg, cell (1, 0) with truecolor
        // bg. `\r\n` keeps the cursor inside the grid bounds.
        t.feed("\x1b[31mA\r\n\x1b[48;2;200;100;50mB");

        // Sanity-check the fixture before the palette swap.
        let painted = t.primary.get(0, 0);
        assert_eq!(
            painted.style.color_fg_source,
            ColorSource::Indexed(1),
            "fixture: red A should carry Indexed(1) fg",
        );
        assert!(painted.style.color_fg.is_some());

        // Paint the alternate grid too — alt cells must also re-resolve.
        t.feed("\x1b[?1049h\x1b[33mC\x1b[?1049l");

        // Drive one cell into scrollback by feeding enough LFs to
        // scroll past the grid's row count. With rows=5, we need 5+
        // LFs from the current position to push the first painted
        // row off the top.
        t.feed("\r\n\n\n\n\n\n");

        // Snapshot the indexed-red fg under the OLD palette.
        let red_before = palette::get().ansi(1, false);

        // Swap palette: rewrite slot 1 to bright green.
        let mut new_palette = palette::Palette::defaults();
        new_palette.ansi[1] = [0.0, 1.0, 0.0, 1.0];
        palette::install(new_palette);

        // Scrollback row 0 is the displaced first-paint row. Before
        // re-resolve, it still carries the OLD red.
        assert!(!t.scrollback.is_empty(), "fixture: row 0 should have scrolled off");
        let sb_row = &t.scrollback[0];
        assert_eq!(sb_row[0].ch, 'A');
        assert_eq!(sb_row[0].style.color_fg_source, ColorSource::Indexed(1));
        assert_eq!(sb_row[0].style.color_fg, Some(red_before));

        t.reresolve_palette();

        // Scrollback row 0 updated.
        let sb_row = &t.scrollback[0];
        assert_eq!(sb_row[0].style.color_fg, Some([0.0, 1.0, 0.0, 1.0]));

        // Truecolor cell (originally cell (1, 0); after scrolling
        // it's at scrollback[1][0]) stays put.
        assert!(t.scrollback.len() >= 2);
        let tc_cell = &t.scrollback[1][0];
        assert_eq!(tc_cell.ch, 'B');
        assert_eq!(tc_cell.style.color_bg_source, ColorSource::Truecolor);
        let tc_bg_after = tc_cell.style.color_bg.expect("bg set");
        assert_eq!(
            crate::palette::linear_to_srgb_u8(tc_bg_after[0]),
            200,
            "truecolor R component preserved across re-resolve",
        );

        // Alt grid was also painted; SGR 33 = yellow = slot 3.
        let cell = t.alternate.get(0, 0);
        assert_eq!(cell.style.color_fg_source, ColorSource::Indexed(3));
        assert_eq!(cell.style.color_fg, Some(palette::get().ansi(3, false)));

        palette::install(palette::Palette::defaults());
    }

    /// Dump the visible grid as newline-separated row strings, with trailing
    /// spaces trimmed from each row for readability.
    fn render(t: &Terminal) -> String {
        let mut out = String::new();
        for r in 0..t.rows {
            let row: String = t.row(r).iter().map(|c| c.ch).collect();
            out.push_str(row.trim_end());
            if r + 1 < t.rows {
                out.push('\n');
            }
        }
        out
    }

    #[test]
    fn plain_text_goes_into_row_0() {
        let mut t = Terminal::new(10, 3, 100);
        t.feed("hello");
        assert_eq!(render(&t), "hello\n\n");
        assert_eq!(t.cursor().row, 0);
        assert_eq!(t.cursor().col, 5);
    }

    #[test]
    fn crlf_moves_to_next_row() {
        let mut t = Terminal::new(10, 3, 100);
        t.feed("a\r\nb");
        assert_eq!(render(&t), "a\nb\n");
        assert_eq!(t.cursor().row, 1);
        assert_eq!(t.cursor().col, 1);
    }

    #[test]
    fn cursor_position_is_1_based() {
        let mut t = Terminal::new(10, 5, 100);
        t.feed("\x1b[3;4H");
        assert_eq!(t.cursor().row, 2);
        assert_eq!(t.cursor().col, 3);
        // writing at that location
        t.feed("X");
        assert_eq!(t.row(2)[3].ch, 'X');
    }

    #[test]
    fn autowrap_end_of_line() {
        let mut t = Terminal::new(3, 3, 100);
        t.feed("abcd");
        // "abc" fills row 0, 'd' wraps to row 1 col 0
        assert_eq!(t.row(0)[0].ch, 'a');
        assert_eq!(t.row(0)[1].ch, 'b');
        assert_eq!(t.row(0)[2].ch, 'c');
        assert_eq!(t.row(1)[0].ch, 'd');
        assert_eq!(t.cursor().row, 1);
        assert_eq!(t.cursor().col, 1);
    }

    #[test]
    fn autowrap_disabled_sticks_at_last_column() {
        let mut t = Terminal::new(3, 3, 100);
        t.feed("\x1b[?7l"); // autowrap off
        t.feed("abcd");
        assert_eq!(t.row(0)[2].ch, 'd');
        assert_eq!(t.cursor().col, 2);
    }

    #[test]
    fn scroll_when_lf_at_bottom() {
        let mut t = Terminal::new(5, 2, 100);
        t.feed("AAA\r\nBBB\r\nCCC");
        // Row 2 doesn't exist; the "AAA" line scrolled off, now row 0=BBB, row 1=CCC
        assert_eq!(t.row(0).iter().map(|c| c.ch).collect::<String>(), "BBB  ");
        assert_eq!(t.row(1).iter().map(|c| c.ch).collect::<String>(), "CCC  ");
        assert_eq!(t.scrollback_len(), 1);
    }

    #[test]
    fn erase_display_mode_3_clears_scrollback_only() {
        let mut t = Terminal::new(5, 2, 100);
        t.feed("AAAAA\r\nBBBBB\r\nCCCCC\r\nDDDDD");
        assert_eq!(t.scrollback_len(), 2);
        t.feed("\x1b[3J");
        assert_eq!(t.scrollback_len(), 0);
        // Visible grid is untouched.
        assert_eq!(t.row(0).iter().map(|c| c.ch).collect::<String>(), "CCCCC");
        assert_eq!(t.row(1).iter().map(|c| c.ch).collect::<String>(), "DDDDD");
    }

    #[test]
    fn erase_display_mode_0() {
        let mut t = Terminal::new(5, 3, 100);
        t.feed("ABCDE\r\nFGHIJ\r\nKLMNO");
        t.feed("\x1b[2;3H"); // row 2, col 3 (0-based: 1, 2)
        t.feed("\x1b[0J"); // erase cursor to end of screen
        assert_eq!(t.row(0).iter().map(|c| c.ch).collect::<String>(), "ABCDE");
        assert_eq!(t.row(1).iter().map(|c| c.ch).collect::<String>(), "FG   ");
        assert_eq!(t.row(2).iter().map(|c| c.ch).collect::<String>(), "     ");
    }

    #[test]
    fn erase_line_mode_0_from_cursor() {
        let mut t = Terminal::new(5, 1, 100);
        t.feed("ABCDE");
        t.feed("\x1b[1;3H"); // col 3 (0-based: 2)
        t.feed("\x1b[K");
        assert_eq!(t.row(0).iter().map(|c| c.ch).collect::<String>(), "AB   ");
    }

    #[test]
    fn sgr_styles_cells() {
        let mut t = Terminal::new(5, 1, 100);
        t.feed("\x1b[31mA\x1b[0mB");
        assert!(t.row(0)[0].style.color_fg.is_some());
        assert_eq!(t.row(0)[1].style.color_fg, None);
    }

    #[test]
    fn alt_screen_preserves_primary() {
        let mut t = Terminal::new(5, 2, 100);
        t.feed("ABC");
        t.feed("\x1b[?1049h"); // enter alt screen, save cursor, clear alt
        t.feed("XYZ");
        assert_eq!(t.row(0).iter().map(|c| c.ch).collect::<String>(), "XYZ  ");
        t.feed("\x1b[?1049l"); // exit alt, restore cursor
        assert_eq!(t.row(0).iter().map(|c| c.ch).collect::<String>(), "ABC  ");
        assert_eq!(t.cursor().col, 3);
    }

    #[test]
    fn save_and_restore_cursor() {
        let mut t = Terminal::new(5, 3, 100);
        t.feed("\x1b[2;3H");
        t.feed("\x1b7"); // save
        t.feed("\x1b[1;1H");
        t.feed("\x1b8"); // restore
        assert_eq!(t.cursor().row, 1);
        assert_eq!(t.cursor().col, 2);
    }

    #[test]
    fn decslrm_ignored_when_lrmm_off() {
        // CSI 1;5s with DECLRMM disabled is silently dropped — margins stay
        // full-width.
        let mut t = Terminal::new(10, 5, 100);
        t.feed("\x1b[1;5s");
        // Prove margins are still full: fill a row, scroll the region down,
        // and confirm the rightmost cols moved (they wouldn't if clipped).
        t.feed("\x1b[1;1H");
        for c in 0..10 {
            t.feed(&format!("{}", (b'a' + c) as char));
        }
        // SU 1 row — without LRM, the whole row clears.
        t.feed("\x1b[S");
        assert_eq!(t.row(0).iter().map(|c| c.ch).collect::<String>(), "          ");
    }

    #[test]
    fn csi_s_no_params_is_save_cursor_when_lrmm_off() {
        // With DECLRMM off, bare `ESC[s` is SCOSC: save the cursor in the same
        // slot DECSC (ESC 7) uses, so ESC 8 restores it.
        let mut t = Terminal::new(10, 5, 100);
        t.feed("\x1b[3;5H\x1b[s\x1b[1;1H\x1b8");
        assert_eq!(t.cursor().row, 2);
        assert_eq!(t.cursor().col, 4);
    }

    #[test]
    fn decslrm_clips_scroll_up_to_margins() {
        // Reproduce tmux's per-pane scroll: enable DECLRMM, set LRM to the
        // "left pane" columns, scroll up — cells outside the LRM (the
        // divider + right pane) must NOT move.
        let mut t = Terminal::new(10, 5, 100);
        // Fill row 0 with "abcdefghij" (cols 0..9).
        t.feed("abcdefghij");
        // Mark a divider in col 6 on rows 1..4 by direct CUP+Print.
        for r in 2..=5 {
            t.feed(&format!("\x1b[{};7H|", r));
        }
        // Enable DECLRMM, set left=1, right=6 (1-based: cols 0..5).
        t.feed("\x1b[?69h\x1b[1;6s");
        // Set scroll region to rows 1..5 (1-based), put cursor inside.
        t.feed("\x1b[1;5r\x1b[1;1H");
        // SU 5 — should blank cols 0..5 of all 5 rows but leave col 6+
        // untouched (the divider remains).
        t.feed("\x1b[5S");
        for r in 1..=4 {
            assert_eq!(t.row(r)[6].ch, '|', "row {r}: divider should survive LRM scroll");
        }
        // And cols 0..5 of row 0 (with our 'abcdef') should now be blank
        // since SU consumed them.
        for c in 0..6 {
            assert_eq!(t.row(0)[c].ch, ' ', "col {c} should be blank after SU");
        }
        // Col 6 of row 0 stays 'g' — outside LRM, untouched.
        assert_eq!(t.row(0)[6].ch, 'g');
    }

    #[test]
    fn decslrm_clips_linefeed_scroll_to_margins() {
        // Same setup but the scroll is triggered by `\n` at scroll_bottom
        // (the path tmux actually uses while painting per-pane content).
        let mut t = Terminal::new(8, 4, 100);
        // Put 'X' in col 5 across every row as a "divider".
        for r in 1..=4 {
            t.feed(&format!("\x1b[{};6HX", r));
        }
        t.feed("\x1b[?69h\x1b[1;5s"); // LRM cols 1..5 (0..4)
        // Position cursor inside LRM at scroll_bottom, then LF — should
        // scroll within LRM only.
        t.feed("\x1b[4;1H\n");
        for r in 0..4 {
            assert_eq!(t.row(r)[5].ch, 'X', "row {r}: divider must survive LF scroll");
        }
    }

    #[test]
    fn decslrm_clips_insert_and_delete_char() {
        let mut t = Terminal::new(10, 1, 100);
        t.feed("abcdefghij");
        t.feed("\x1b[?69h\x1b[1;5s"); // LRM cols 1..5 (0..4)
        // ICH 2 at col 0 — cells shift within LRM only; cols 5..9 untouched.
        t.feed("\x1b[1;1H\x1b[2@");
        let s: String = t.row(0).iter().map(|c| c.ch).collect();
        assert_eq!(s, "  abcfghij");
        // DCH 2 at col 0 — pulls cells from within LRM only.
        t.feed("\x1b[2P");
        let s: String = t.row(0).iter().map(|c| c.ch).collect();
        assert_eq!(s, "abc  fghij");
    }

    #[test]
    fn decslrm_clips_erase_char_to_right_margin() {
        let mut t = Terminal::new(10, 1, 100);
        t.feed("abcdefghij");
        t.feed("\x1b[?69h\x1b[1;5s");
        t.feed("\x1b[1;3H\x1b[10X"); // ECH 10 at col 3 — clip to col 5
        let s: String = t.row(0).iter().map(|c| c.ch).collect();
        assert_eq!(s, "ab   fghij");
    }

    #[test]
    fn decslrm_69l_disables_and_resets() {
        let mut t = Terminal::new(10, 1, 100);
        t.feed("abcdefghij");
        t.feed("\x1b[?69h\x1b[1;5s"); // LRM on, cols 1..5
        t.feed("\x1b[?69l"); // disable LRMM → margins reset, future DECSLRM ignored
        // Now SU should affect the full width again.
        t.feed("\x1b[1S");
        let s: String = t.row(0).iter().map(|c| c.ch).collect();
        assert_eq!(s, "          ");
    }

    #[test]
    fn full_region_scrollback_requires_full_lrm() {
        // With a narrowed LRM the "full region" rule that pushes lines to
        // scrollback shouldn't fire — scrollback should stay empty.
        let mut t = Terminal::new(10, 3, 100);
        t.feed("row1\n\rrow2\n\rrow3");
        t.feed("\x1b[?69h\x1b[1;5s"); // LRM cols 1..5
        let prior = t.scrollback_len();
        // Force a scroll inside the LRM via SU on the full scroll region.
        t.feed("\x1b[3;1H\x1b[3S");
        assert_eq!(t.scrollback_len(), prior, "narrow LRM must not push to scrollback");
    }

    #[test]
    fn autowrap_inside_lrm_wraps_at_right_margin() {
        // 10 cols, 4 rows. LRM cols 1..5 (0..4). Print 7 chars starting at
        // col 0 row 0: chars 1-5 land in row 0 cols 0..4, char 6 wraps to
        // row 1 col 0 (the LEFT margin), char 7 lands at row 1 col 1.
        // Crucially: row 0 cols 5..9 stay blank — without LRM-aware wrap,
        // the run would have spilled into the neighbor pane.
        let mut t = Terminal::new(10, 4, 100);
        t.feed("\x1b[?69h\x1b[1;5s\x1b[1;1H"); // LRM, cursor to (0,0)
        t.feed("abcdefg");
        let r0: String = t.row(0).iter().map(|c| c.ch).collect();
        let r1: String = t.row(1).iter().map(|c| c.ch).collect();
        assert_eq!(r0, "abcde     ", "row 0 must not bleed past right margin");
        assert_eq!(r1, "fg        ", "wrap target column must be left margin");
    }

    #[test]
    fn autowrap_outside_lrm_uses_screen_edge() {
        // xterm-compat: when the cursor is sitting OUTSIDE the LRM (because
        // CUP placed it there — CUP isn't clipped), the wrap should fall
        // back to the screen edge rather than snapping to the left margin.
        let mut t = Terminal::new(10, 4, 100);
        t.feed("\x1b[?69h\x1b[1;5s"); // LRM cols 1..5
        t.feed("\x1b[1;7H"); // CUP to (0, 6) — outside LRM (col 6 > right=4)
        t.feed("xyzw");      // prints at cols 6,7,8,9 → wrap_pending
        t.feed("Q");         // wraps to (1, 0), prints 'Q'
        let r0: String = t.row(0).iter().map(|c| c.ch).collect();
        let r1: String = t.row(1).iter().map(|c| c.ch).collect();
        assert_eq!(r0, "      xyzw");
        assert_eq!(&r1[..2], "Q ", "outside-LRM wrap targets screen col 0, not left margin");
    }

    #[test]
    fn autowrap_disabled_sticks_at_right_margin() {
        // With DECAWM off, printing past the right margin should overwrite
        // the rightmost cell in place — same shape as the existing screen-
        // edge behavior, but pinned to the LRM right edge.
        let mut t = Terminal::new(10, 2, 100);
        t.feed("\x1b[?69h\x1b[1;5s\x1b[?7l\x1b[1;1H"); // LRM + DECAWM off
        t.feed("abcdefg");
        let r0: String = t.row(0).iter().map(|c| c.ch).collect();
        // Cols 0..3 keep their original chars; col 4 (right margin) ends up
        // holding the last char printed; rest of the row stays blank.
        assert_eq!(&r0[..4], "abcd");
        assert_eq!(r0.chars().nth(4), Some('g'));
        assert_eq!(&r0[5..], "     ");
    }

    #[test]
    fn decslrm_invalid_range_resets_to_full_width() {
        // VT spec: a DECSLRM request with left >= right (or out-of-bounds
        // right) must reset margins to the full screen rather than leaving
        // a previously-narrowed range in place. Apps rely on this to "clear"
        // margins by sending e.g. `CSI 1;1s`.
        let mut t = Terminal::new(10, 1, 100);
        t.feed("abcdefghij");
        // Establish a narrow LRM first so we can prove the next request
        // *resets* rather than no-ops.
        t.feed("\x1b[?69h\x1b[1;5s");
        // Now an invalid range: left == right (both 1-based '3'), which is
        // l >= r in the parsed form. Per spec, this should reset to full.
        t.feed("\x1b[3;3s");
        // SU 1 — if margins were reset, the entire row clears; if the old
        // narrow LRM survived, cols 5..9 would remain "fghij".
        t.feed("\x1b[1S");
        let s: String = t.row(0).iter().map(|c| c.ch).collect();
        assert_eq!(s, "          ", "invalid DECSLRM must reset margins to full width");
    }

    #[test]
    fn decslrm_right_beyond_cols_resets_to_full_width() {
        // Similar to above but tests the other invalid form: right > cols.
        // tmux occasionally emits `CSI 1;<huge>s` when computing margins
        // against a stale geometry; we must treat that as "reset to full".
        let mut t = Terminal::new(10, 1, 100);
        t.feed("abcdefghij");
        t.feed("\x1b[?69h\x1b[1;5s"); // narrow first
        t.feed("\x1b[1;99s"); // right > cols → reset
        t.feed("\x1b[1S");
        let s: String = t.row(0).iter().map(|c| c.ch).collect();
        assert_eq!(s, "          ");
    }

    #[test]
    fn decslrm_homes_the_cursor() {
        // Per VT spec, DECSLRM (like DECSTBM) homes the cursor to (0,0)
        // after setting margins. Apps depend on this when bootstrapping a
        // per-pane drawing context — they don't issue a separate CUP.
        let mut t = Terminal::new(10, 5, 100);
        t.feed("\x1b[3;7H"); // park cursor mid-screen
        assert_eq!(t.cursor().row, 2);
        assert_eq!(t.cursor().col, 6);
        t.feed("\x1b[?69h\x1b[2;6s"); // DECSLRM
        assert_eq!(t.cursor().row, 0, "DECSLRM should home cursor row");
        assert_eq!(t.cursor().col, 0, "DECSLRM should home cursor col");
    }

    #[test]
    fn cup_is_not_clipped_to_lrm() {
        // xterm behavior: DECSLRM only constrains scroll-style *operations*;
        // CUP/HVP can still address any cell on the screen. tmux relies on
        // this — it sets a left-pane LRM, then CUPs to the right pane to
        // draw the divider/right-pane content.
        let mut t = Terminal::new(10, 3, 100);
        t.feed("\x1b[?69h\x1b[1;5s"); // LRM cols 0..4 (narrow)
        // CUP to col 8 (1-based 9), well outside the LRM range.
        t.feed("\x1b[1;9HZ");
        assert_eq!(t.row(0)[8].ch, 'Z', "CUP must place cell outside LRM");
        // And the cursor itself should sit there, not get clamped to col 4.
        // After printing 'Z' at col 8 the cursor advances to col 9.
        assert_eq!(t.cursor().col, 9);
    }

    #[test]
    fn decslrm_clips_scroll_down_to_margins() {
        // Symmetric coverage to decslrm_clips_scroll_up_to_margins: SD (CSI T)
        // must also respect LRM so reverse-scroll inside a pane doesn't
        // disturb cells in neighboring panes.
        let mut t = Terminal::new(10, 5, 100);
        // Fill row 4 with content inside the LRM and a divider in col 6.
        t.feed("\x1b[5;1Habcdef|hij");
        // Mark the divider on the other rows too so we can detect leakage.
        for r in 1..=4 {
            t.feed(&format!("\x1b[{};7H|", r));
        }
        t.feed("\x1b[?69h\x1b[1;6s"); // LRM cols 0..5
        t.feed("\x1b[1;5r\x1b[1;1H"); // scroll region rows 0..4
        t.feed("\x1b[5T"); // SD 5 — should blank cols 0..5 within region
        for r in 0..=4 {
            assert_eq!(t.row(r)[6].ch, '|', "row {r}: divider must survive SD");
        }
        // Cols 0..5 of every row should now be blank (SD pushed content out).
        for c in 0..6 {
            assert_eq!(t.row(4)[c].ch, ' ', "row 4 col {c} should be blank after SD");
        }
    }

    #[test]
    fn insert_line_noop_when_cursor_outside_lrm_columns() {
        // IL/DL are documented as no-ops when the cursor is outside the
        // scroll region. With DECLRMM that "scroll region" is 2D — the
        // cursor must be inside the column range too. This matters because
        // an app drawing into the right pane should not accidentally
        // shift rows in the left pane just because it issued IL with the
        // cursor parked over there.
        let mut t = Terminal::new(10, 3, 100);
        t.feed("row1xxxxxx\n\rrow2xxxxxx\n\rrow3xxxxxx");
        t.feed("\x1b[?69h\x1b[1;5s"); // LRM cols 0..4
        // Park cursor at col 7 (outside LRM), then IL 1.
        t.feed("\x1b[1;8H\x1b[L");
        // Row 0 should still read "row1xxxxx" — IL was a no-op.
        let s: String = t.row(0).iter().map(|c| c.ch).collect();
        assert_eq!(s, "row1xxxxxx", "IL must be a no-op when cursor is outside LRM cols");
    }

    #[test]
    fn delete_line_noop_when_cursor_outside_lrm_columns() {
        // Mirror of the IL test above for DL.
        let mut t = Terminal::new(10, 3, 100);
        t.feed("row1xxxxxx\n\rrow2xxxxxx\n\rrow3xxxxxx");
        t.feed("\x1b[?69h\x1b[1;5s"); // LRM cols 0..4
        t.feed("\x1b[1;8H\x1b[M"); // DL 1 with cursor outside LRM
        let s: String = t.row(0).iter().map(|c| c.ch).collect();
        assert_eq!(s, "row1xxxxxx", "DL must be a no-op when cursor is outside LRM cols");
    }

    #[test]
    fn full_reset_clears_lrmm_state() {
        // RIS (`ESC c`) must restore DECLRMM to disabled and margins to
        // full width. Without this, an app that crashes mid-session and
        // issues RIS would still see a narrowed scroll region — a classic
        // "terminal stuck" symptom.
        let mut t = Terminal::new(10, 1, 100);
        t.feed("\x1b[?69h\x1b[1;5s"); // narrow LRM
        t.feed("\x1bc"); // RIS
        // Now reprint and SU — should clear the whole row, proving LRM
        // reset to full width AND DECLRMM is disabled (so a subsequent
        // bare `CSI s` is SCOSC again, not reset-margins).
        t.feed("abcdefghij\x1b[1S");
        let s: String = t.row(0).iter().map(|c| c.ch).collect();
        assert_eq!(s, "          ", "RIS must reset LRM to full width");
        // Verify DECLRMM was disabled too: bare CSI s must now be SCOSC.
        // Park cursor, save via `CSI s`, move, restore — should land back.
        t.feed("\x1b[1;5H\x1b[s\x1b[1;1H\x1b8");
        assert_eq!(t.cursor().col, 4, "after RIS, bare CSI s should be SCOSC");
    }

    #[test]
    fn resize_clears_lrmm_state() {
        // Resize must drop LRMM — the old margins are tied to the old
        // column count and would be nonsensical (or out of bounds) after
        // a resize. The renderer assumes scroll_right < cols.
        let mut t = Terminal::new(10, 3, 100);
        t.feed("\x1b[?69h\x1b[1;5s"); // narrow LRM on the 10-col grid
        t.resize(20, 3); // grow to 20 cols
        // Fill row 0 across the new width then SU — if LRMM survived, the
        // old narrow margin would leak through and cols 5..19 wouldn't
        // clear. We expect them to clear (LRMM disabled, full width).
        t.feed("\x1b[1;1H");
        for c in 0..20 {
            t.feed(&format!("{}", (b'a' + (c % 26) as u8) as char));
        }
        t.feed("\x1b[1S");
        let s: String = t.row(0).iter().map(|c| c.ch).collect();
        assert_eq!(s, "                    ", "resize must clear LRMM state");
    }

    #[test]
    fn cup_and_print_column_of_chars() {
        // Reproduce the way tmux paints a vertical pane border: for each row,
        // CUP to (row, divider_col) then Print('│'). Whole column must be
        // filled, including rows where nothing else was written.
        let mut t = Terminal::new(7, 5, 100);
        for r in 1..=5 {
            t.feed(&format!("\x1b[{};4H│", r));
        }
        for r in 0..5 {
            assert_eq!(t.row(r)[3].ch, '│', "row {r}: divider missing");
        }
    }

    #[test]
    fn alt_screen_cup_column_paint_survives() {
        // Same as above but inside alt-screen (where tmux actually runs).
        let mut t = Terminal::new(7, 5, 100);
        t.feed("\x1b[?1049h");
        for r in 1..=5 {
            t.feed(&format!("\x1b[{};4H│", r));
        }
        for r in 0..5 {
            assert_eq!(t.row(r)[3].ch, '│', "alt-screen row {r}: divider missing");
        }
    }

    #[test]
    fn backspace_moves_left_without_erasing() {
        let mut t = Terminal::new(5, 1, 100);
        t.feed("ABC\x08");
        assert_eq!(t.cursor().col, 2);
        assert_eq!(t.row(0)[2].ch, 'C'); // not erased
    }

    #[test]
    fn tab_advances_to_next_stop() {
        let mut t = Terminal::new(20, 1, 100);
        t.feed("A\t");
        assert_eq!(t.cursor().col, 8);
        t.feed("B\t");
        assert_eq!(t.cursor().col, 16);
    }

    #[test]
    fn cursor_visible_toggle() {
        let mut t = Terminal::new(5, 1, 100);
        assert!(t.cursor_visible());
        t.feed("\x1b[?25l");
        assert!(!t.cursor_visible());
        t.feed("\x1b[?25h");
        assert!(t.cursor_visible());
    }

    #[test]
    fn set_scroll_region_then_lf_scrolls_only_region() {
        let mut t = Terminal::new(5, 5, 100);
        t.feed("A\r\nB\r\nC\r\nD\r\nE");
        t.feed("\x1b[2;4r"); // region rows 2..=4 (0-based 1..=3); homes cursor
        assert_eq!(t.cursor().row, 0);
        t.feed("\x1b[4;1H"); // cursor at row 4 (0-based 3, last in region)
        t.feed("\n"); // LF at scroll_bottom → scroll region up by 1
        assert_eq!(t.row(0).iter().map(|c| c.ch).collect::<String>(), "A    ");
        // inside region: B (was at row 1) rolled off; row 1 now holds C.
        assert_eq!(t.row(1).iter().map(|c| c.ch).collect::<String>(), "C    ");
        assert_eq!(t.row(2).iter().map(|c| c.ch).collect::<String>(), "D    ");
        // bottom of region was cleared
        assert_eq!(t.row(3).iter().map(|c| c.ch).collect::<String>(), "     ");
        // outside region — untouched
        assert_eq!(t.row(4).iter().map(|c| c.ch).collect::<String>(), "E    ");
    }

    #[test]
    fn insert_line_pushes_rows_down_within_region() {
        let mut t = Terminal::new(3, 4, 100);
        t.feed("AAA\r\nBBB\r\nCCC\r\nDDD");
        t.feed("\x1b[2;1H"); // cursor row 1 (0-based)
        t.feed("\x1b[L"); // IL 1
        assert_eq!(t.row(0).iter().map(|c| c.ch).collect::<String>(), "AAA");
        assert_eq!(t.row(1).iter().map(|c| c.ch).collect::<String>(), "   ");
        assert_eq!(t.row(2).iter().map(|c| c.ch).collect::<String>(), "BBB");
        assert_eq!(t.row(3).iter().map(|c| c.ch).collect::<String>(), "CCC");
    }

    #[test]
    fn insert_line_respects_scroll_region() {
        let mut t = Terminal::new(3, 4, 100);
        t.feed("AAA\r\nBBB\r\nCCC\r\nDDD");
        t.feed("\x1b[2;3r"); // region rows 2..=3
        t.feed("\x1b[2;1H"); // cursor at row 1 (top of region)
        t.feed("\x1b[L");
        // outside region untouched
        assert_eq!(t.row(0).iter().map(|c| c.ch).collect::<String>(), "AAA");
        // inside region: blank inserted at top, BBB shifts down, CCC pushed off
        assert_eq!(t.row(1).iter().map(|c| c.ch).collect::<String>(), "   ");
        assert_eq!(t.row(2).iter().map(|c| c.ch).collect::<String>(), "BBB");
        assert_eq!(t.row(3).iter().map(|c| c.ch).collect::<String>(), "DDD");
    }

    #[test]
    fn insert_line_outside_region_is_noop() {
        let mut t = Terminal::new(3, 4, 100);
        t.feed("AAA\r\nBBB\r\nCCC\r\nDDD");
        t.feed("\x1b[2;3r"); // region rows 2..=3 (0-based 1..=2)
        t.feed("\x1b[1;1H"); // cursor at row 0 (outside region)
        t.feed("\x1b[L");
        assert_eq!(t.row(0).iter().map(|c| c.ch).collect::<String>(), "AAA");
        assert_eq!(t.row(1).iter().map(|c| c.ch).collect::<String>(), "BBB");
        assert_eq!(t.row(2).iter().map(|c| c.ch).collect::<String>(), "CCC");
    }

    #[test]
    fn delete_line_shifts_rows_up() {
        let mut t = Terminal::new(3, 4, 100);
        t.feed("AAA\r\nBBB\r\nCCC\r\nDDD");
        t.feed("\x1b[2;1H");
        t.feed("\x1b[M"); // DL 1
        assert_eq!(t.row(0).iter().map(|c| c.ch).collect::<String>(), "AAA");
        assert_eq!(t.row(1).iter().map(|c| c.ch).collect::<String>(), "CCC");
        assert_eq!(t.row(2).iter().map(|c| c.ch).collect::<String>(), "DDD");
        assert_eq!(t.row(3).iter().map(|c| c.ch).collect::<String>(), "   ");
    }

    #[test]
    fn delete_line_does_not_push_to_scrollback() {
        let mut t = Terminal::new(3, 4, 100);
        t.feed("AAA\r\nBBB\r\nCCC\r\nDDD");
        t.feed("\x1b[1;1H");
        t.feed("\x1b[M");
        assert_eq!(t.scrollback_len(), 0);
    }

    #[test]
    fn insert_char_shifts_within_row() {
        let mut t = Terminal::new(6, 1, 100);
        t.feed("ABCDEF");
        t.feed("\x1b[1;3H"); // col 3 (0-based 2)
        t.feed("\x1b[2@");
        // CD shifted right by 2; right edge dropped.
        assert_eq!(t.row(0).iter().map(|c| c.ch).collect::<String>(), "AB  CD");
    }

    #[test]
    fn insert_char_clamps_to_eol() {
        let mut t = Terminal::new(5, 1, 100);
        t.feed("ABCDE");
        t.feed("\x1b[1;3H");
        t.feed("\x1b[99@"); // clamps to 3 cols
        assert_eq!(t.row(0).iter().map(|c| c.ch).collect::<String>(), "AB   ");
    }

    #[test]
    fn delete_char_shifts_within_row() {
        let mut t = Terminal::new(6, 1, 100);
        t.feed("ABCDEF");
        t.feed("\x1b[1;3H"); // col 3
        t.feed("\x1b[2P");
        // CD removed; EF slides left; right edge filled.
        assert_eq!(t.row(0).iter().map(|c| c.ch).collect::<String>(), "ABEF  ");
    }

    #[test]
    fn erase_char_replaces_in_place() {
        let mut t = Terminal::new(6, 1, 100);
        t.feed("ABCDEF");
        t.feed("\x1b[1;3H");
        t.feed("\x1b[3X");
        assert_eq!(t.row(0).iter().map(|c| c.ch).collect::<String>(), "AB   F");
        // cursor unchanged
        assert_eq!(t.cursor().col, 2);
    }

    #[test]
    fn dsr_5_replies_ok() {
        let mut t = Terminal::new(5, 3, 100);
        t.feed("\x1b[5n");
        assert_eq!(t.take_response(), b"\x1b[0n".to_vec());
        // Drained — second call is empty.
        assert!(t.take_response().is_empty());
    }

    #[test]
    fn dsr_6_reports_cursor_position_1_based() {
        let mut t = Terminal::new(10, 5, 100);
        t.feed("\x1b[3;7H"); // row 3, col 7 (1-based)
        t.feed("\x1b[6n");
        assert_eq!(t.take_response(), b"\x1b[3;7R".to_vec());
    }

    #[test]
    fn dsr_unknown_code_no_reply() {
        let mut t = Terminal::new(5, 3, 100);
        t.feed("\x1b[99n");
        assert!(t.take_response().is_empty());
    }

    #[test]
    fn deccm_set_and_reset_toggles_app_cursor_keys() {
        let mut t = Terminal::new(5, 3, 100);
        assert!(!t.app_cursor_keys());
        t.feed("\x1b[?1h");
        assert!(t.app_cursor_keys());
        t.feed("\x1b[?1l");
        assert!(!t.app_cursor_keys());
    }

    #[test]
    fn primary_da_replies_with_vt100() {
        let mut t = Terminal::new(5, 3, 100);
        t.feed("\x1b[c");
        assert_eq!(t.take_response(), b"\x1b[?1;2c".to_vec());
    }

    #[test]
    fn secondary_da_replies_with_vt220() {
        let mut t = Terminal::new(5, 3, 100);
        t.feed("\x1b[>c");
        assert_eq!(t.take_response(), b"\x1b[>0;276;0c".to_vec());
    }

    #[test]
    fn cursor_blink_follows_decscusr() {
        let mut t = Terminal::new(5, 3, 100);
        assert!(t.cursor_blink()); // default 0 — blink block
        t.feed("\x1b[2 q");
        assert!(!t.cursor_blink()); // steady block
        t.feed("\x1b[3 q");
        assert!(t.cursor_blink()); // blink underline
        t.feed("\x1b[6 q");
        assert!(!t.cursor_blink()); // steady bar
    }

    #[test]
    fn decscusr_sets_cursor_shape() {
        let mut t = Terminal::new(5, 3, 100);
        assert_eq!(t.cursor_shape(), CursorShape::Block);
        t.feed("\x1b[3 q");
        assert_eq!(t.cursor_shape(), CursorShape::Underline);
        t.feed("\x1b[5 q");
        assert_eq!(t.cursor_shape(), CursorShape::Bar);
        t.feed("\x1b[2 q");
        assert_eq!(t.cursor_shape(), CursorShape::Block);
    }

    #[test]
    fn osc_color_query_replies_with_default() {
        let mut t = Terminal::new(5, 3, 100);
        t.set_default_colors([0xab, 0xcd, 0xef], [0x01, 0x02, 0x03], [0x77, 0x88, 0x99]);
        t.feed("\x1b]10;?\x07");
        assert_eq!(
            t.take_response(),
            b"\x1b]10;rgb:abab/cdcd/efef\x1b\\".to_vec(),
        );
        t.feed("\x1b]11;?\x1b\\");
        assert_eq!(
            t.take_response(),
            b"\x1b]11;rgb:0101/0202/0303\x1b\\".to_vec(),
        );
        t.feed("\x1b]12;?\x07");
        assert_eq!(
            t.take_response(),
            b"\x1b]12;rgb:7777/8888/9999\x1b\\".to_vec(),
        );
    }

    #[test]
    fn line_at_returns_scrollback_then_grid() {
        let mut t = Terminal::new(5, 2, 100);
        t.feed("AAAAA\r\nBBBBB\r\nCCCCC\r\nDDDDD");
        // scrollback = [AAAAA, BBBBB], grid = [CCCCC, DDDDD]
        assert_eq!(t.scrollback_len(), 2);
        assert_eq!(t.line_at(0).unwrap()[0].ch, 'A');
        assert_eq!(t.line_at(1).unwrap()[0].ch, 'B');
        assert_eq!(t.line_at(2).unwrap()[0].ch, 'C');
        assert_eq!(t.line_at(3).unwrap()[0].ch, 'D');
        assert!(t.line_at(4).is_none());
        assert!(t.line_at(-1).is_none());
    }

    #[test]
    fn visual_to_abs_line_matches_extended_cell() {
        let mut t = Terminal::new(5, 2, 100);
        t.feed("AAAAA\r\nBBBBB\r\nCCCCC\r\n");
        t.scroll_up(1);
        // Visual row 0 = newest scrollback (BBBBB) at abs index 1.
        let abs = t.visual_to_abs_line(0);
        assert_eq!(t.line_at(abs).unwrap()[0].ch, 'B');
        let abs = t.visual_to_abs_line(1);
        assert_eq!(t.line_at(abs).unwrap()[0].ch, 'C');
    }

    #[test]
    fn xtgettcap_replies_with_known_caps() {
        let mut t = Terminal::new(5, 3, 100);
        // "Co" = 0x43 0x6f -> "436f"; "ku" = "6b75". Both are known.
        t.feed("\x1bP+q436f;6b75\x1b\\");
        let reply = String::from_utf8(t.take_response()).unwrap();
        // Single 1+r DCS containing both name=value pairs.
        assert!(reply.starts_with("\x1bP1+r"), "got {reply:?}");
        assert!(reply.ends_with("\x1b\\"), "got {reply:?}");
        // "Co" = "256" → 323536, "ku" = ESC[A → 1b5b41
        assert!(reply.contains("436f=323536"), "got {reply:?}");
        assert!(reply.contains("6b75=1b5b41"), "got {reply:?}");
    }

    #[test]
    fn xtgettcap_replies_with_unknown_separately() {
        let mut t = Terminal::new(5, 3, 100);
        // "ZZ" = "5a5a" — not a real cap.
        t.feed("\x1bP+q5a5a\x1b\\");
        let reply = String::from_utf8(t.take_response()).unwrap();
        assert_eq!(reply, "\x1bP0+r5a5a\x1b\\");
    }

    #[test]
    fn xtgettcap_groups_known_and_unknown() {
        let mut t = Terminal::new(5, 3, 100);
        // "Co" known, "ZZ" unknown.
        t.feed("\x1bP+q436f;5a5a\x1b\\");
        let reply = String::from_utf8(t.take_response()).unwrap();
        assert!(reply.contains("\x1bP1+r436f=323536\x1b\\"));
        assert!(reply.contains("\x1bP0+r5a5a\x1b\\"));
    }

    #[test]
    fn xtgettcap_uppercase_hex_normalizes_in_reply() {
        let mut t = Terminal::new(5, 3, 100);
        t.feed("\x1bP+q436F\x1b\\"); // uppercase F
        let reply = String::from_utf8(t.take_response()).unwrap();
        // Reply hex names are emitted lowercase regardless of query case.
        assert!(reply.contains("436f="), "got {reply:?}");
    }

    #[test]
    fn dcs_without_xtgettcap_prefix_is_ignored() {
        let mut t = Terminal::new(5, 3, 100);
        t.feed("\x1bP$qmhello\x1b\\"); // DECRQSS or similar — not implemented
        assert!(t.take_response().is_empty());
    }

    #[test]
    fn dcs_tmux_passthrough_unwraps_and_reprocesses_body() {
        // tmux passthrough wraps an app's escape sequences for the
        // outer terminal: `ESC P tmux ; <body> ESC \\`, with literal
        // ESC bytes inside <body> doubled. Cat'ing a recording of
        // such output directly into yutani (no tmux in the loop)
        // should still dispatch the wrapped sequences — the ansi
        // parser un-doubles the ESCs and `handle_dcs` strips the
        // `tmux;` prefix and re-feeds the body through the parser.
        //
        // Pick an inner sequence whose effect we can observe: SGR 31
        // turns the cursor's fg red, and the printed 'A' should
        // carry that fg.
        let _guard = crate::palette::TEST_LOCK.lock().expect("test lock");
        crate::palette::install(crate::palette::Palette::defaults());
        let mut t = Terminal::new(5, 3, 100);
        // Doubled-ESC encoding of `ESC [ 3 1 m A`:
        t.feed("\x1bPtmux;\x1b\x1b[31mA\x1b\\");
        let cell = t.row(0)[0];
        assert_eq!(cell.ch, 'A');
        assert_eq!(
            cell.style.color_fg_source,
            crate::style::ColorSource::Indexed(1),
            "wrapped SGR must have reached apply_sgr",
        );
    }

    #[test]
    fn dcs_tmux_passthrough_survives_pty_chunk_split_mid_next_dcs() {
        // Regression: when a PTY chunk delivers DCS#1 in full plus
        // the *start* of DCS#2, the outer parser ends Phase 1 in
        // DcsString (DCS#2 is still accumulating). Phase 2 then
        // dispatches DCS#1's event. If `handle_dcs` re-parses the
        // inner body through `self.parser` it inherits that stuck
        // DcsString state — the leading ESC of the inner APC turns
        // into an unrecognized DCS escape, the partial buf is
        // cleared, and the rest of the inner sequence is printed as
        // text instead of dispatched as an APC. A fresh, independent
        // parser sidesteps this entirely.
        let mut t = Terminal::new(20, 5, 100);
        t.set_cell_size_px(8, 16);
        // Feed DCS#1 complete + DCS#2's opener (no terminator yet).
        // DCS#1 wraps `ESC [31m A` (set fg red, print A).
        let chunk1 = "\x1bPtmux;\x1b\x1b[31mA\x1b\\\x1bPtmux;";
        t.feed(chunk1);
        // Without the fix: 'A' is never printed (the SGR + print
        // sequence got mangled by the stuck-DcsString re-feed).
        // With the fix: 'A' lands on the grid with red fg via the
        // properly-dispatched inner SGR + Print.
        let cell = t.row(0)[0];
        assert_eq!(cell.ch, 'A', "inner Print event must dispatch even when outer parser is mid-DCS");
        assert_eq!(
            cell.style.color_fg_source,
            crate::style::ColorSource::Indexed(1),
            "inner SGR 31 must reach apply_sgr",
        );
        // Finish DCS#2 with a no-op body so the outer parser returns
        // to Ground cleanly for any follow-on chunk.
        t.feed("\x1b\\");
    }

    #[test]
    fn dcs_tmux_passthrough_dispatches_wrapped_kitty_transmit() {
        // Mirrors the real file from the bug report (~/bad-kitty.txt):
        // tmux-wrapped Kitty `a=T,i=N,...` payload. Walking it through
        // `Terminal::feed` should register the kitty image id so a
        // later `a=p,i=N` (or any lookup) sees it.
        let mut t = Terminal::new(20, 5, 100);
        t.set_cell_size_px(8, 16);
        // Build a tiny PNG just like the kitty E2E helpers.
        let png = {
            let buf = image::RgbaImage::from_pixel(2, 2, image::Rgba([0, 128, 255, 255]));
            let mut bytes = Vec::new();
            image::DynamicImage::ImageRgba8(buf)
                .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageOutputFormat::Png)
                .expect("encode");
            bytes
        };
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        // Wrap as tmux passthrough: doubled ESCs around the Kitty APC.
        let wrapped = format!(
            "\x1bPtmux;\x1b\x1b_Ga=T,f=100,i=4242,U=1;{}\x1b\x1b\\\x1b\\",
            b64,
        );
        t.feed(&wrapped);
        let uploads = t.take_pending_image_uploads();
        assert_eq!(
            uploads.len(), 1,
            "tmux-wrapped Kitty a=T must produce one pending upload",
        );
        assert_eq!(uploads[0].kitty_image_id, Some(4242));
    }

    #[test]
    fn a_f_without_i_targets_most_recently_completed_image() {
        // Per Kitty spec: when `i=` is missing on `a=f` (and `a=p` /
        // `a=d` / `a=a`), the most recently created image is the
        // implicit target. icat's animation stream relies on this —
        // frame transmissions for the GIF being animated arrive as
        // bare `a=f` with neither `i=` nor `m=`, no in-flight
        // chunked transmission to inherit from. Without the
        // last-completed fallback the frame silently drops and the
        // animation never plays past the base.
        let mut t = Terminal::new(20, 5, 100);
        t.set_cell_size_px(8, 16);
        // Establish a base image with explicit id 4242.
        let png = {
            let buf = image::RgbaImage::from_pixel(2, 2, image::Rgba([1, 2, 3, 255]));
            let mut bytes = Vec::new();
            image::DynamicImage::ImageRgba8(buf)
                .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageOutputFormat::Png)
                .expect("encode");
            bytes
        };
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        t.feed(&format!("\x1b_Ga=T,f=100,i=4242,U=1;{}\x1b\\", b64));
        let base_uploads = t.take_pending_image_uploads();
        assert_eq!(base_uploads.len(), 1);
        assert_eq!(base_uploads[0].kitty_image_id, Some(4242));

        // Now send a bare `a=f` — no `i=`, no `m=`. Should attach to
        // image 4242 via the spec fallback.
        let frame_png = {
            let buf = image::RgbaImage::from_pixel(1, 1, image::Rgba([9, 9, 9, 255]));
            let mut bytes = Vec::new();
            image::DynamicImage::ImageRgba8(buf)
                .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageOutputFormat::Png)
                .expect("encode");
            bytes
        };
        let b64 = base64::engine::general_purpose::STANDARD.encode(&frame_png);
        t.feed(&format!("\x1b_Ga=f,q=2;{}\x1b\\", b64));
        let frame_uploads = t.take_pending_image_uploads();
        assert_eq!(
            frame_uploads.len(),
            1,
            "bare a=f must produce one frame upload via implicit-i= fallback",
        );
        let up = &frame_uploads[0];
        assert!(up.animation_frame.is_some(), "a=f path flagged");
        assert_eq!(
            up.kitty_image_id,
            Some(4242),
            "implicit i= must resolve to the most recently completed image",
        );
    }

    #[test]
    fn a_f_without_i_drops_when_no_prior_image() {
        // Symmetric guard: no prior transmission, no implicit
        // fallback to leak into. Bare `a=f` must be ignored cleanly
        // rather than crash or create a stranded entry.
        let mut t = Terminal::new(20, 5, 100);
        t.set_cell_size_px(8, 16);
        let frame_png = {
            let buf = image::RgbaImage::from_pixel(1, 1, image::Rgba([9, 9, 9, 255]));
            let mut bytes = Vec::new();
            image::DynamicImage::ImageRgba8(buf)
                .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageOutputFormat::Png)
                .expect("encode");
            bytes
        };
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&frame_png);
        t.feed(&format!("\x1b_Ga=f,q=2;{}\x1b\\", b64));
        let uploads = t.take_pending_image_uploads();
        assert!(uploads.is_empty(), "no prior image → drop, don't crash");
    }

    #[test]
    fn dcs_tmux_passthrough_dispatches_wrapped_apc() {
        // The real-world case: tmux-wrapped Kitty graphics APC.
        // After unwrap, the body is `ESC _ G a=q,i=42 ESC \\` — a
        // Kitty capability query. Its dispatch path writes an `OK`
        // reply to `pending_response`; checking that proves the
        // unwrapped APC reached the right handler.
        let mut t = Terminal::new(5, 3, 100);
        t.feed("\x1bPtmux;\x1b\x1b_Ga=q,f=100,i=42\x1b\x1b\\\x1b\\");
        let reply = String::from_utf8(t.take_response()).unwrap_or_default();
        assert!(
            reply.contains("i=42") && reply.contains("OK"),
            "expected Kitty query OK reply with i=42; got {reply:?}",
        );
    }

    #[test]
    fn hex_codec_roundtrip() {
        for s in &["Co", "ku", "k1", "RGB", ""] {
            if s.is_empty() {
                continue;
            }
            let h = hex_encode(s);
            let back = hex_decode_ascii(&h).unwrap();
            assert_eq!(&back, s, "roundtrip {s:?}");
        }
        // Odd-length and non-hex are rejected.
        assert!(hex_decode_ascii("abc").is_none());
        assert!(hex_decode_ascii("zz").is_none());
    }

    #[test]
    fn osc_set_title_does_not_reply() {
        let mut t = Terminal::new(5, 3, 100);
        t.feed("\x1b]0;hello\x07");
        assert!(t.take_response().is_empty());
    }

    #[test]
    fn mouse_modes_track_private_set_reset() {
        let mut t = Terminal::new(5, 3, 100);
        t.feed("\x1b[?1000h\x1b[?1006h");
        let mp = t.mouse_protocol();
        assert!(mp.press_release);
        assert!(mp.sgr);
        assert!(mp.enabled());
        t.feed("\x1b[?1000l\x1b[?1006l");
        assert!(!t.mouse_protocol().enabled());
    }

    #[test]
    fn bracketed_paste_mode_tracks() {
        let mut t = Terminal::new(5, 3, 100);
        assert!(!t.bracketed_paste());
        t.feed("\x1b[?2004h");
        assert!(t.bracketed_paste());
        t.feed("\x1b[?2004l");
        assert!(!t.bracketed_paste());
    }

    #[test]
    fn full_reset_clears_app_cursor_keys_and_response() {
        let mut t = Terminal::new(5, 3, 100);
        t.feed("\x1b[?1h\x1b[5n");
        t.feed("\x1bc"); // RIS
        assert!(!t.app_cursor_keys());
        assert!(t.take_response().is_empty());
    }

    #[test]
    fn full_reset() {
        let mut t = Terminal::new(5, 3, 100);
        t.feed("ABC\r\nXYZ\x1b[?25l");
        t.feed("\x1bc");
        assert_eq!(render(&t), "\n\n");
        assert!(t.cursor_visible());
        assert_eq!(t.cursor().row, 0);
        assert_eq!(t.cursor().col, 0);
    }

    #[test]
    fn osc_is_swallowed() {
        let mut t = Terminal::new(10, 1, 100);
        t.feed("\x1b]0;title\x07after");
        assert_eq!(t.row(0).iter().map(|c| c.ch).take(5).collect::<String>(), "after");
    }

    #[test]
    fn prompt_sp_pattern() {
        // The pattern that caused the %-bug: zsh prints '%', pads with spaces,
        // then CR and CSI K to wipe it before printing the real prompt.
        let mut t = Terminal::new(10, 2, 100);
        t.feed("%         \r\x1b[K$ ");
        assert_eq!(t.row(0).iter().map(|c| c.ch).collect::<String>(), "$         ");
        assert_eq!(t.cursor().row, 0);
        assert_eq!(t.cursor().col, 2);
    }

    #[test]
    fn resize_preserves_top_left_content() {
        let mut t = Terminal::new(5, 3, 100);
        t.feed("HELLO");
        t.resize(3, 3);
        assert_eq!(t.row(0).iter().map(|c| c.ch).collect::<String>(), "HEL");
        assert_eq!(t.cols, 3);
    }

    /// Helper: read a row of cells back as a trimmed string, for legibility
    /// in the resize-reflow assertions below.
    fn row_string(cells: &[Cell]) -> String {
        cells.iter().map(|c| c.ch).collect::<String>().trim_end().to_string()
    }

    #[test]
    fn resize_vertical_shrink_spills_top_rows_into_scrollback() {
        let mut t = Terminal::new(10, 5, 100);
        t.feed("L1\r\nL2\r\nL3\r\nL4\r\nL5");
        // Pre-conditions: top row holds L1, cursor sits on the last grid row.
        assert_eq!(row_string(t.row(0)), "L1");
        assert_eq!(t.cursor().row, 4);
        assert_eq!(t.scrollback_len(), 0);

        t.resize(10, 3);

        // Two top rows spill into scrollback in chronological order.
        assert_eq!(t.scrollback_len(), 2);
        assert_eq!(row_string(t.line_at(0).unwrap()), "L1");
        assert_eq!(row_string(t.line_at(1).unwrap()), "L2");
        // Grid now holds the bottom three lines.
        assert_eq!(row_string(t.row(0)), "L3");
        assert_eq!(row_string(t.row(1)), "L4");
        assert_eq!(row_string(t.row(2)), "L5");
        // Cursor was at row 4; spill of 2 brings it down to row 2 (still on L5).
        assert_eq!(t.cursor().row, 2);
    }

    #[test]
    fn resize_vertical_grow_pulls_from_scrollback() {
        let mut t = Terminal::new(10, 3, 100);
        t.feed("L1\r\nL2\r\nL3\r\nL4\r\nL5");
        // Pre-conditions: 2 rows already in scrollback, cursor on L5 (row 2).
        assert_eq!(t.scrollback_len(), 2);
        assert_eq!(t.cursor().row, 2);

        t.resize(10, 5);

        // Scrollback fully drained back into the grid.
        assert_eq!(t.scrollback_len(), 0);
        assert_eq!(row_string(t.row(0)), "L1");
        assert_eq!(row_string(t.row(1)), "L2");
        assert_eq!(row_string(t.row(2)), "L3");
        assert_eq!(row_string(t.row(3)), "L4");
        assert_eq!(row_string(t.row(4)), "L5");
        // Cursor steps down by the pull count (2) to stay on L5.
        assert_eq!(t.cursor().row, 4);
    }

    #[test]
    fn resize_shrink_then_grow_round_trip_preserves_content() {
        // The user-reported regression: shrinking and growing back used to
        // leave blank rows where the prompt had been.
        let mut t = Terminal::new(10, 5, 100);
        t.feed("L1\r\nL2\r\nL3\r\nL4\r\nL5");
        let before = render(&t);
        assert_eq!(t.cursor().row, 4);

        t.resize(10, 3);
        t.resize(10, 5);

        assert_eq!(render(&t), before);
        assert_eq!(t.cursor().row, 4);
        assert_eq!(t.scrollback_len(), 0);
    }

    #[test]
    fn resize_vertical_shrink_evicts_oldest_when_scrollback_full() {
        // scrollback_limit = 1 means a 2-row spill must drop the older line.
        let mut t = Terminal::new(10, 5, 1);
        t.feed("L1\r\nL2\r\nL3\r\nL4\r\nL5");
        assert_eq!(t.scrollback_len(), 0);

        t.resize(10, 3);

        // Spill of 2 with capacity 1: L1 evicted via pop_front, L2 kept.
        assert_eq!(t.scrollback_len(), 1);
        assert_eq!(row_string(t.line_at(0).unwrap()), "L2");
        // Grid still holds the bottom three lines.
        assert_eq!(row_string(t.row(0)), "L3");
        assert_eq!(row_string(t.row(1)), "L4");
        assert_eq!(row_string(t.row(2)), "L5");
    }

    #[test]
    fn resize_vertical_grow_with_no_scrollback_adds_blank_rows_at_top() {
        let mut t = Terminal::new(10, 3, 100);
        // No input — empty grid, empty scrollback, cursor at origin.
        assert_eq!(t.scrollback_len(), 0);
        assert_eq!(t.cursor().row, 0);

        t.resize(10, 5);

        assert_eq!(t.scrollback_len(), 0);
        // Nothing was pulled — cursor untouched, all rows blank.
        assert_eq!(t.cursor().row, 0);
        for r in 0..5 {
            assert_eq!(row_string(t.row(r)), "");
        }
    }

    #[test]
    fn resize_on_alt_screen_does_not_touch_scrollback() {
        let mut t = Terminal::new(10, 5, 100);
        // Push two lines into primary scrollback before switching screens.
        t.feed("L1\r\nL2\r\nL3\r\nL4\r\nL5\r\nL6\r\nL7");
        let scrollback_before = t.scrollback_len();
        assert_eq!(scrollback_before, 2);

        // Enter alt screen and write something there.
        t.feed("\x1b[?1049h");
        t.feed("alt-text");
        assert_eq!(row_string(t.row(0)), "alt-text");

        // Shrink + grow on the alt screen must not touch primary scrollback.
        t.resize(10, 3);
        assert_eq!(t.scrollback_len(), scrollback_before);
        t.resize(10, 5);
        assert_eq!(t.scrollback_len(), scrollback_before);

        // Alt grid is rebuilt blank on resize (existing alt-screen behavior).
        for r in 0..5 {
            assert_eq!(row_string(t.row(r)), "");
        }
    }

    #[test]
    fn resize_no_op_when_dimensions_match() {
        let mut t = Terminal::new(10, 5, 100);
        t.feed("L1\r\nL2\r\nL3\r\nL4\r\nL5");
        let before_render = render(&t);
        let before_scrollback = t.scrollback_len();
        let before_cursor = t.cursor();
        let before_view = t.view_offset();

        t.resize(10, 5);

        assert_eq!(render(&t), before_render);
        assert_eq!(t.scrollback_len(), before_scrollback);
        assert_eq!(t.cursor().row, before_cursor.row);
        assert_eq!(t.cursor().col, before_cursor.col);
        assert_eq!(t.view_offset(), before_view);
    }

    #[test]
    fn scroll_up_pulls_scrollback_into_view() {
        let mut t = Terminal::new(5, 2, 100);
        // Fill beyond the viewport so a line is forced into scrollback.
        t.feed("AAAAA\r\nBBBBB\r\nCCCCC\r\n");
        assert_eq!(t.scrollback_len(), 2);
        assert!(t.at_bottom());
        assert!(t.scroll_up(1));
        assert_eq!(t.view_offset(), 1);
        // The top visible row is now the newest scrollback line (BBBBB); the
        // bottom visible row is what was previously row 0 of the grid (CCCCC).
        assert_eq!(t.visible_cell(0, 0).ch, 'B');
        assert_eq!(t.visible_cell(1, 0).ch, 'C');
    }

    #[test]
    fn viewport_stays_anchored_as_new_lines_enter_scrollback() {
        let mut t = Terminal::new(5, 2, 100);
        t.feed("AAAAA\r\nBBBBB\r\nCCCCC\r\n");
        // scrollback = [AAAAA, BBBBB], grid = [CCCCC, _]
        assert_eq!(t.scrollback_len(), 2);
        assert!(t.scroll_up(2));
        // Top of viewport pinned to AAAAA; bottom to BBBBB.
        assert_eq!(t.visible_cell(0, 0).ch, 'A');
        assert_eq!(t.visible_cell(1, 0).ch, 'B');

        // New lines stream in. The visible content must not shift.
        t.feed("DDDDD\r\nEEEEE\r\n");
        assert_eq!(t.visible_cell(0, 0).ch, 'A');
        assert_eq!(t.visible_cell(1, 0).ch, 'B');
    }

    #[test]
    fn viewport_pins_to_top_when_oldest_scrollback_evicts() {
        // scrollback_limit = 3 — once full, pop_front evicts oldest.
        let mut t = Terminal::new(5, 2, 3);
        t.feed("AAAAA\r\nBBBBB\r\nCCCCC\r\nDDDDD\r\nEEEEE\r\n");
        // scrollback = [BBBBB, CCCCC, DDDDD] (AAAAA already evicted),
        // grid = [EEEEE, _]
        assert_eq!(t.scrollback_len(), 3);
        assert!(t.scroll_up(3));
        assert_eq!(t.visible_cell(0, 0).ch, 'B');
        assert_eq!(t.visible_cell(1, 0).ch, 'C');

        // Push a new line: BBBBB is evicted. We can no longer show it, so
        // pin to the new oldest line (CCCCC) without indexing past the buffer.
        t.feed("FFFFF\r\n");
        assert_eq!(t.scrollback_len(), 3);
        assert_eq!(t.view_offset(), 3);
        assert_eq!(t.visible_cell(0, 0).ch, 'C');
        assert_eq!(t.visible_cell(1, 0).ch, 'D');
    }

    #[test]
    fn scroll_clamps_at_ends() {
        let mut t = Terminal::new(5, 2, 100);
        t.feed("AAAAA\r\nBBBBB\r\nCCCCC\r\n");
        // Scrolling past the top is a no-op.
        assert!(t.scroll_up(10));
        assert!(!t.scroll_up(1));
        assert!(t.at_top());
        // Scrolling back past bottom is a no-op.
        assert!(t.scroll_down(10));
        assert!(!t.scroll_down(1));
        assert!(t.at_bottom());
    }

    #[test]
    fn scroll_is_noop_on_alt_screen() {
        let mut t = Terminal::new(5, 2, 100);
        t.feed("AAAAA\r\nBBBBB\r\nCCCCC\r\n"); // populate scrollback
        t.feed("\x1b[?1049h"); // enter alt screen
        assert!(!t.scroll_up(1));
        assert_eq!(t.view_offset(), 0);
    }

    #[test]
    fn cursor_visual_row_hidden_when_scrolled_off() {
        let mut t = Terminal::new(5, 3, 100);
        // Push 4 lines into scrollback while parking the cursor on the
        // last row of the live grid (row=2).
        t.feed("AAAAA\r\nBBBBB\r\nCCCCC\r\nDDDDD\r\nEEEEE\r\nFFFFF\r\n");
        assert_eq!(t.cursor_visual_row(), Some(2));
        // 1 line back: visual=3 (== rows). Still drawn at the bottom edge.
        t.scroll_up(1);
        assert_eq!(t.cursor_visual_row(), Some(3));
        // 2 lines back: visual=4 (rows + 1). The renderer's phantom band
        // covers this so the cursor can still intersect the window at the
        // bottom edge — keep drawing.
        t.scroll_up(1);
        assert_eq!(t.cursor_visual_row(), Some(4));
        // 3 lines back: visual=5 (rows + 2). Beyond the phantom band; hide.
        t.scroll_up(1);
        assert_eq!(t.cursor_visual_row(), None);
        t.scroll_to_bottom();
        assert_eq!(t.cursor_visual_row(), Some(2));
    }

    #[test]
    fn extended_cell_matches_visible_inside_viewport() {
        let mut t = Terminal::new(5, 2, 100);
        t.feed("AAAAA\r\nBBBBB\r\nCCCCC\r\n");
        t.scroll_up(1);
        // Within [0, rows), extended_cell must agree with visible_cell.
        for r in 0..t.rows {
            for c in 0..t.cols {
                assert_eq!(
                    t.extended_cell(r as isize, c).map(|x| x.ch),
                    Some(t.visible_cell(r, c).ch),
                );
            }
        }
    }

    #[test]
    fn extended_cell_above_none_when_no_scrollback() {
        let mut t = Terminal::new(5, 3, 100);
        t.feed("AAAAA");
        assert_eq!(t.scrollback_len(), 0);
        assert!(t.extended_cell(-1, 0).is_none());
        assert!(t.extended_cell(-2, 0).is_none());
    }

    #[test]
    fn extended_cell_above_none_when_at_top() {
        let mut t = Terminal::new(5, 2, 100);
        t.feed("AAAAA\r\nBBBBB\r\nCCCCC\r\n");
        assert_eq!(t.scrollback_len(), 2);
        assert!(t.scroll_up(t.scrollback_len()));
        assert!(t.at_top());
        assert!(t.extended_cell(-1, 0).is_none());
        assert!(t.extended_cell(-2, 0).is_none());
    }

    #[test]
    fn extended_cell_above_returns_two_rows_of_history() {
        let mut t = Terminal::new(5, 2, 100);
        // scrollback = [AAAAA (oldest), BBBBB, CCCCC]
        t.feed("AAAAA\r\nBBBBB\r\nCCCCC\r\nDDDDD\r\n");
        assert_eq!(t.scrollback_len(), 3);
        // Scroll up by 1: visual row 0 is CCCCC; -1 = BBBBB; -2 = AAAAA.
        assert!(t.scroll_up(1));
        assert_eq!(t.visible_cell(0, 0).ch, 'C');
        assert_eq!(t.extended_cell(-1, 0).map(|c| c.ch), Some('B'));
        assert_eq!(t.extended_cell(-2, 0).map(|c| c.ch), Some('A'));
        // Only 3 lines exist; nothing further back.
        assert!(t.extended_cell(-3, 0).is_none());
    }

    #[test]
    fn extended_cell_below_none_when_at_bottom() {
        let mut t = Terminal::new(5, 2, 100);
        t.feed("AAAAA\r\nBBBBB\r\nCCCCC\r\n");
        assert!(t.at_bottom());
        assert!(t.extended_cell(t.rows as isize, 0).is_none());
        assert!(t.extended_cell(t.rows as isize + 1, 0).is_none());
    }

    #[test]
    fn extended_cell_below_returns_two_rows_of_live_grid() {
        let mut t = Terminal::new(5, 2, 100);
        // scrollback = [AAAAA, BBBBB]; primary rows = [CCCCC, DDDDD]
        t.feed("AAAAA\r\nBBBBB\r\nCCCCC\r\nDDDDD");
        assert_eq!(t.scrollback_len(), 2);
        // Scroll up 2: visible rows are [AAAAA, BBBBB]; below them are CCCCC then DDDDD.
        assert!(t.scroll_up(2));
        assert_eq!(t.visible_cell(0, 0).ch, 'A');
        assert_eq!(t.visible_cell(1, 0).ch, 'B');
        assert_eq!(t.extended_cell(2, 0).map(|c| c.ch), Some('C'));
        assert_eq!(t.extended_cell(3, 0).map(|c| c.ch), Some('D'));
    }

    #[test]
    fn extended_cell_alt_screen_only_returns_in_bounds() {
        let mut t = Terminal::new(5, 2, 100);
        t.feed("AAAAA\r\nBBBBB\r\nCCCCC\r\n");
        t.feed("\x1b[?1049h");
        // Phantom rows are suppressed on the alt screen.
        assert!(t.extended_cell(-1, 0).is_none());
        assert!(t.extended_cell(-2, 0).is_none());
        assert!(t.extended_cell(t.rows as isize, 0).is_none());
        assert!(t.extended_cell(t.rows as isize + 1, 0).is_none());
        // In-bounds rows still resolve (to the alt grid).
        assert!(t.extended_cell(0, 0).is_some());
        assert!(t.extended_cell((t.rows - 1) as isize, 0).is_some());
    }

    //
    // Image-placement tests (slice 2).
    //

    #[test]
    fn insert_placement_lands_on_active_grid_with_unique_ids() {
        let mut t = Terminal::new(20, 10, 100);
        let a = place(&mut t, 7, 2, 3, 4, 5);
        let b = place(&mut t, 8, 0, 0, 2, 2);
        assert_ne!(a, b);
        let anchors = live_anchors(&t);
        assert_eq!(anchors.len(), 2);
        assert!(anchors.contains(&(7, 2, 3, 4, 5)));
        assert!(anchors.contains(&(8, 0, 0, 2, 2)));
    }

    #[test]
    fn insert_placement_targets_alternate_when_active() {
        let mut t = Terminal::new(20, 10, 100);
        t.feed("\x1b[?1049h"); // switch to alt
        place(&mut t, 1, 0, 0, 2, 2);
        assert_eq!(t.live_placements().len(), 1);
        t.feed("\x1b[?1049l"); // back to primary
        assert!(t.live_placements().is_empty());
        t.feed("\x1b[?1049h");
        // Re-entering alt clears the alt buffer (matches existing alt-cell
        // behaviour); the placement we made here is gone.
        assert!(t.live_placements().is_empty());
    }

    #[test]
    fn grid_clear_drops_placements() {
        let mut t = Terminal::new(20, 10, 100);
        place(&mut t, 1, 0, 0, 2, 2);
        place(&mut t, 2, 5, 5, 3, 3);
        t.feed("\x1b[2J"); // ED 2 — erase whole screen
        assert!(t.live_placements().is_empty());
    }

    #[test]
    fn cell_erase_within_row_does_not_touch_placements() {
        // EL (erase-in-line) and partial-row clears must not delete images.
        // Kitty / iTerm both keep placements alive across cell-erase ops.
        let mut t = Terminal::new(20, 10, 100);
        place(&mut t, 1, 3, 0, 2, 4);
        t.feed("\x1b[3;1H"); // CUP row 3 col 1 (1-based)
        t.feed("\x1b[2K"); // erase entire line
        assert_eq!(t.live_placements().len(), 1);
    }

    #[test]
    fn ed3_clears_scrollback_placements() {
        let mut t = Terminal::new(20, 5, 100);
        place(&mut t, 1, 0, 0, 2, 2);
        // SU 3 — unconditional scroll-up, doesn't depend on cursor row.
        t.feed("\x1b[3S");
        assert!(t.live_placements().is_empty());
        assert!(!t.scrollback_placements_for_test().is_empty());
        t.feed("\x1b[3J");
        assert!(t.scrollback_placements_for_test().is_empty());
    }

    #[test]
    fn full_reset_clears_all_placements_and_id_state() {
        let mut t = Terminal::new(20, 5, 100);
        let _a = place(&mut t, 1, 0, 0, 1, 1);
        let _b = place(&mut t, 2, 2, 2, 1, 1);
        // Force a scrollback placement too.
        place(&mut t, 3, 0, 0, 1, 1);
        t.feed("\x1b[2S");
        t.feed("\x1bc"); // RIS — full reset
        assert!(t.live_placements().is_empty());
        assert!(t.scrollback_placements_for_test().is_empty());
        // First placement after reset gets id 1 again.
        let id = place(&mut t, 9, 0, 0, 1, 1);
        assert_eq!(id, 1);
    }

    #[test]
    fn scroll_region_up_shifts_intersecting_placements_only() {
        let mut t = Terminal::new(20, 10, 100);
        // Above scroll region — stays put.
        let above_id = place(&mut t, 1, 0, 0, 1, 2);
        // In region — shifts up.
        let in_id = place(&mut t, 2, 5, 0, 2, 2);
        // Below region — stays put.
        let below_id = place(&mut t, 3, 9, 0, 1, 2);
        // Set scroll region to rows 4..=8 (1-based 5..=9) and feed nothing —
        // call the internal scroll directly via a region scroll-up sequence.
        // CSI 5;9 r sets the region; then SU 2 scrolls within it.
        t.feed("\x1b[5;9r\x1b[2S");
        let by_id: std::collections::HashMap<u32, isize> = t
            .live_placements()
            .iter()
            .map(|p| (p.id, p.top_row))
            .collect();
        assert_eq!(by_id[&above_id], 0);
        assert_eq!(by_id[&in_id], 3); // 5 - 2
        assert_eq!(by_id[&below_id], 9);
    }

    #[test]
    fn scroll_region_up_full_screen_evicts_to_scrollback() {
        let mut t = Terminal::new(20, 5, 100);
        let id = place(&mut t, 1, 0, 0, 2, 2);
        // Scroll the whole grid up by 2 — the placement (rows 0,1) fully
        // exits the top. Should land in scrollback at index 0 (oldest).
        t.feed("\x1b[2S");
        assert!(t.live_placements().is_empty());
        let sb = t.scrollback_placements_for_test();
        assert_eq!(sb.len(), 1);
        assert_eq!(sb[0].0, 0); // anchor at oldest scrollback row
        assert_eq!(sb[0].1.id, id);
    }

    // ---- scrollback_placements_in_view: viewport-row mapping ----
    //
    // The renderer uses this to draw images that have scrolled into history
    // when the user scrolls back. The mapping puts scrollback row sb_r at
    // viewport row sb_r - (sb_len - view_off). Tests below pin that down
    // for the three interesting positions plus alt-screen / no-offset
    // short-circuits.

    #[test]
    fn scrollback_in_view_empty_when_not_scrolled() {
        // view_offset == 0: even with promoted placements, nothing renders
        // through this path — the live grid (now empty of them) is what
        // the user sees.
        let mut t = Terminal::new(20, 5, 100);
        place(&mut t, 1, 0, 0, 2, 2);
        t.feed("\x1b[2S");
        assert_eq!(t.view_offset(), 0);
        assert!(t.scrollback_placements_in_view(t.rows).is_empty());
    }

    #[test]
    fn scrollback_in_view_empty_on_alt_screen() {
        // Alt screen has no scrollback. Even if entries existed (they
        // don't — alt promotions are blocked upstream), this accessor
        // refuses to surface them.
        let mut t = Terminal::new(20, 5, 100);
        place(&mut t, 1, 0, 0, 2, 2);
        t.feed("\x1b[2S");
        t.feed("\x1b[?1049h"); // enter alt screen
        assert!(t.on_alt_screen());
        assert!(t.scrollback_placements_in_view(t.rows).is_empty());
    }

    #[test]
    fn scrollback_in_view_maps_anchor_to_viewport_row() {
        // Image lands at oldest scrollback row (0). Scroll back by the
        // full scrollback length and that row sits at the top of the
        // viewport (viewport_row == 0).
        let mut t = Terminal::new(20, 5, 100);
        let id = place(&mut t, 1, 0, 0, 2, 2);
        t.feed("\x1b[2S"); // promote to scrollback, sb_len now 2
        assert_eq!(t.scrollback_len(), 2);
        assert!(t.scroll_up(2));
        let in_view = t.scrollback_placements_in_view(t.rows);
        assert_eq!(in_view.len(), 1);
        assert_eq!(in_view[0].image.0, id);
        assert_eq!(in_view[0].top_row, 0);
        assert_eq!(in_view[0].rows, 2);
    }

    #[test]
    fn scrollback_in_view_filters_two_distinct_rows() {
        // Place image 1, scroll it into history, push enough more
        // scrollback that image 1 is well above the 2-row smooth-scroll
        // slack window; then place image 2 and scroll just one row back.
        // image 1 is filtered (far above the slack), image 2 is at
        // viewport row 0.
        let mut t = Terminal::new(20, 5, 100);
        place(&mut t, 1, 0, 0, 1, 1);
        t.feed("\x1b[1S"); // image 1 → scrollback row 0
        t.feed("\x1b[5S"); // push image 1 further above the slack window
        place(&mut t, 2, 0, 0, 1, 1);
        t.feed("\x1b[1S"); // image 2 → newest scrollback row
        let sb_len = t.scrollback_len();
        assert!(sb_len >= 7);
        assert!(t.scroll_up(1));
        let in_view = t.scrollback_placements_in_view(t.rows);
        assert_eq!(in_view.len(), 1);
        assert_eq!(in_view[0].image.0, 2);
        assert_eq!(in_view[0].top_row, 0);
    }

    #[test]
    fn scrollback_in_view_keeps_placement_within_top_slack() {
        // Placement whose discrete viewport position is 1 row above the
        // top of the viewport — fully off-screen by integer math, but
        // smooth-scroll can move it down by up to one line_height before
        // the next view_offset tick. Filter must keep it in the slack
        // window so the image fades in smoothly from the top edge instead
        // of snapping into view when view_offset increments.
        let mut t = Terminal::new(20, 5, 100);
        place(&mut t, 1, 0, 0, 1, 1);
        t.feed("\x1b[1S"); // sb_row=0
        t.feed("\x1b[1S"); // push image to sb shift territory: sb_len=2
        assert_eq!(t.scrollback_len(), 2);
        assert!(t.scroll_up(1));
        // sb_len=2, view_off=1 → shift=-1; image at sb_row=0 → top=-1,
        // bottom=0. Old filter (bottom <= 0) excluded; new slack keeps it.
        let in_view = t.scrollback_placements_in_view(t.rows);
        assert_eq!(in_view.len(), 1);
        assert_eq!(in_view[0].top_row, -1);
    }

    #[test]
    fn scrollback_in_view_keeps_placement_within_bottom_slack() {
        // Mirror image of the above: placement just past the bottom of
        // the viewport stays in the slack window so smooth-scroll up can
        // reveal its top edge.
        let mut t = Terminal::new(20, 5, 100);
        place(&mut t, 1, 0, 0, 1, 1);
        t.feed("\x1b[1S"); // sb_row=0
        // Build scrollback so the image lands one row below viewport
        // after scrolling all the way back. With viewport_rows=5 and slack=2,
        // a top_row of 5 or 6 must still be returned.
        for _ in 0..5 {
            t.feed("\n");
        }
        let sb_len = t.scrollback_len();
        assert!(t.scroll_up(sb_len));
        let in_view = t.scrollback_placements_in_view(t.rows);
        // sb_row=0, shift=0 → top=0; with slack we expect it kept.
        assert_eq!(in_view.len(), 1);
        assert_eq!(in_view[0].top_row, 0);
    }

    #[test]
    fn scrollback_in_view_straddles_live_boundary() {
        // 2-row image promoted, then scroll back by 1. Its anchor row is
        // one row above the viewport top but bottom_row=1 still spills
        // into the visible area. The renderer relies on this — it draws
        // the whole image, the camera ortho clips above row 0.
        let mut t = Terminal::new(20, 5, 100);
        place(&mut t, 1, 0, 0, 2, 2);
        t.feed("\x1b[2S");
        assert!(t.scroll_up(1));
        let in_view = t.scrollback_placements_in_view(t.rows);
        assert_eq!(in_view.len(), 1);
        // sb_len=2, view_off=1 → shift=-1; sb_row=0 → viewport_row=-1
        assert_eq!(in_view[0].top_row, -1);
        assert_eq!(in_view[0].rows, 2);
    }

    #[test]
    fn scrollback_in_view_filters_above_top() {
        // Scroll back by 1 only, but image was promoted many rows ago.
        // Anchor + height land entirely above viewport row 0 → filtered.
        let mut t = Terminal::new(20, 5, 100);
        place(&mut t, 1, 0, 0, 1, 1);
        t.feed("\x1b[1S"); // promotes (scrollback_row = 0)
        // Push more scrollback so view_offset=1 leaves the placement above.
        // SU evicts the top row of the live grid into scrollback.
        t.feed("\x1b[5S");
        assert!(t.scrollback_len() >= 6);
        assert!(t.scroll_up(1));
        // sb_len>=6, view_off=1 → shift<=-5; sb_row=0 → top_row<=-5;
        // bottom_row = top_row + 1 <= -4 → filtered.
        assert!(t.scrollback_placements_in_view(t.rows).is_empty());
    }

    #[test]
    fn scrollback_in_view_filters_below_grid() {
        // To make an image filtered when scrolled fully back, it has to
        // sit at a scrollback row past `viewport_rows + ROW_SLACK`. Once
        // promoted the image's scrollback_row is fixed, so we have to
        // build up lots of scrollback BEFORE placing it.
        let mut t = Terminal::new(20, 5, 100);
        // 15 newlines from row 0 scroll the bottom 11 times, putting
        // 11 rows in scrollback.
        for _ in 0..15 {
            t.feed("\n");
        }
        place(&mut t, 1, 0, 0, 1, 1); // image at grid row 0
        t.feed("\x1b[1S"); // promotes; image sits at scrollback_row ~11
        let sb_len = t.scrollback_len();
        assert!(sb_len > t.rows + 2);
        assert!(t.scroll_up(sb_len));
        // view_off == sb_len → shift = 0. Image's scrollback_row is past
        // viewport_rows + slack (5 + 2 = 7), so it's off the bottom and
        // filtered.
        assert!(t.scrollback_placements_in_view(t.rows).is_empty());
    }

    #[test]
    fn scrollback_in_view_walks_back_into_view() {
        // End-to-end: place + scroll into scrollback + walk back via
        // scroll_up, then assert the placement reappears in the in-view
        // list. Mirrors the user flow `kitty +kitten icat ; wheel up`.
        let mut t = Terminal::new(20, 5, 100);
        let id = place(&mut t, 1, 0, 0, 2, 2);
        t.feed("\x1b[3S"); // image is gone from live; now in scrollback
        assert!(t.live_placements().is_empty());
        assert!(t.scrollback_placements_in_view(t.rows).is_empty()); // no scroll yet
        assert!(t.scroll_up(3));
        let in_view = t.scrollback_placements_in_view(t.rows);
        assert_eq!(in_view.len(), 1);
        assert_eq!(in_view[0].image.0, id);
    }

    #[test]
    fn scroll_region_up_straddling_image_stays_with_negative_top() {
        let mut t = Terminal::new(20, 5, 100);
        place(&mut t, 1, 0, 0, 3, 2);
        // Scroll by 1 — image now straddles top of viewport.
        t.feed("\x1b[1S");
        let anchors = live_anchors(&t);
        assert_eq!(anchors.len(), 1);
        assert_eq!(anchors[0].1, -1);
        assert!(t.scrollback_placements_for_test().is_empty());
        // Scroll by 2 more — bottom_row was 2, becomes 0 — fully off.
        t.feed("\x1b[2S");
        assert!(t.live_placements().is_empty());
        let sb = t.scrollback_placements_for_test();
        assert_eq!(sb.len(), 1);
        // First scroll pushed 1 row to scrollback; second scroll pushed 2
        // more. Image was anchored at the very first pushed row.
        assert_eq!(sb[0].0, 0);
    }

    #[test]
    fn scroll_region_down_drops_off_bottom() {
        let mut t = Terminal::new(20, 5, 100);
        place(&mut t, 1, 3, 0, 2, 2); // covers rows 3,4 in a 5-row grid
        // CSI T scrolls the region down by 1; placement shifts to top_row=4
        // (still partially visible — bottom_row=6, top_row=4 < 5 rows).
        t.feed("\x1b[1T");
        assert_eq!(t.live_placements().len(), 1);
        assert_eq!(t.live_placements()[0].top_row, 4);
        // One more — top_row=5 hits fully_off_grid's top_row >= rows test.
        t.feed("\x1b[1T");
        assert!(t.live_placements().is_empty());
    }

    #[test]
    fn scroll_partial_width_region_leaves_placements_alone() {
        let mut t = Terminal::new(20, 10, 100);
        // Enable DECLRMM and set a partial-width margin, then scroll.
        t.feed("\x1b[?69h\x1b[5;15s\x1b[1;10r");
        place(&mut t, 1, 2, 7, 2, 4);
        t.feed("\x1b[2S");
        // Placement unchanged — partial-width scroll skips placements.
        assert_eq!(t.live_placements()[0].top_row, 2);
    }

    #[test]
    fn resize_vertical_shrink_spills_placement_to_scrollback() {
        let mut t = Terminal::new(20, 10, 100);
        place(&mut t, 1, 2, 0, 2, 4); // anchored in rows 2..4
        // Shrink rows 10 → 6. spill = 4 (rows 0..4). Placement at row 2 is
        // anchored in a spilled row, so it should land in scrollback.
        t.resize(20, 6);
        assert!(t.live_placements().is_empty());
        let sb = t.scrollback_placements_for_test();
        assert_eq!(sb.len(), 1);
        // Spilled 4 rows; placement's row 2 is the 3rd-from-oldest of the
        // spilled batch (sb_row = 0 + 2 = 2).
        assert_eq!(sb[0].0, 2);
    }

    #[test]
    fn resize_vertical_shrink_keeps_low_placements_live() {
        let mut t = Terminal::new(20, 10, 100);
        place(&mut t, 1, 7, 0, 2, 4); // rows 7..9
        // Shrink 10 → 6, spill = 4. Placement at row 7 stays live; shifts up
        // by 4 to row 3.
        t.resize(20, 6);
        assert_eq!(t.live_placements().len(), 1);
        assert_eq!(t.live_placements()[0].top_row, 3);
    }

    #[test]
    fn resize_vertical_grow_promotes_scrollback_placement_back() {
        let mut t = Terminal::new(20, 6, 100);
        place(&mut t, 1, 0, 0, 2, 4);
        // Force a single-row scroll so placement straddles, then another to
        // fully evict — but actually simplest: resize-shrink to push it into
        // scrollback, then resize-grow to pull it back.
        t.resize(20, 3); // spill=3, placement (row 0..1) → sb at row 0
        assert_eq!(t.scrollback_placements_for_test().len(), 1);
        // Now grow back. refill = up to scrollback length. The placement at
        // sb row 0 should not be the FIRST refilled, because refill drains
        // the TAIL of scrollback (most recent), not the head.
        t.resize(20, 6);
        // Scrollback had 3 entries (sb rows 0,1,2). refill drains the most
        // recent (rows 1,2 — there are 3 rows but new_rows-old_rows = 3, so
        // all 3 drain). sb_post = 0. Placement at sb_row=0 is in the
        // drained range (>= sb_post=0), so it promotes. Its new top_row =
        // 0 - 0 = 0.
        assert_eq!(t.scrollback_placements_for_test().len(), 0);
        assert_eq!(t.live_placements().len(), 1);
        assert_eq!(t.live_placements()[0].top_row, 0);
    }

    #[test]
    fn resize_horizontal_shrink_preserves_off_screen_placements() {
        // Regression: a placement whose left_col is now past the grid's
        // right edge MUST survive the shrink. Horizontal off-screen is
        // recoverable — the user can widen the window and the
        // placement comes back into view. Dropping on shrink (the old
        // behavior) made images vanish permanently on any resize that
        // briefly hid them.
        let mut t = Terminal::new(20, 5, 100);
        place(&mut t, 1, 0, 2, 1, 4); // cols 2..6
        place(&mut t, 2, 0, 15, 1, 2); // cols 15..17
        t.resize(10, 5); // new cols = 10
        let images: Vec<u32> = t.live_placements().iter().map(|p| p.image.0).collect();
        assert_eq!(
            images,
            vec![1, 2],
            "both placements survive the shrink; the renderer clips off-screen draws",
        );
        // Grow back — the off-screen placement is still visible at its
        // original left_col.
        t.resize(20, 5);
        let images: Vec<u32> = t.live_placements().iter().map(|p| p.image.0).collect();
        assert_eq!(images, vec![1, 2]);
        assert_eq!(
            t.live_placements()[1].left_col,
            15,
            "left_col preserved across shrink + grow round-trip",
        );
    }

    #[test]
    fn referenced_image_ids_unions_primary_alt_and_scrollback() {
        use std::collections::HashSet;
        let mut t = Terminal::new(10, 5, 100);
        // Primary placements with two distinct images.
        place(&mut t, 10, 0, 0, 1, 2);
        place(&mut t, 20, 0, 0, 1, 2);
        // Scroll one out so it lands in scrollback.
        t.feed("\x1b[1S");
        // Alt screen placement with a third image.
        t.feed("\x1b[?1049h");
        place(&mut t, 30, 0, 0, 1, 1);
        t.feed("\x1b[?1049l");
        // Back on primary: dedupe re-includes 10 and 20 (still live or sb),
        // plus 30 from alt grid.
        let ids: HashSet<u32> = t.referenced_image_ids().iter().map(|i| i.0).collect();
        assert!(ids.contains(&10));
        assert!(ids.contains(&20));
        assert!(ids.contains(&30));
    }

    //
    // iTerm2 OSC 1337 parsing tests (P2.2). The parser only fills the
    // `pending_image_uploads` queue here — cursor advancement and
    // placement creation land in P2.3.
    //

    /// Build a minimal PNG and wrap it in an iTerm2 OSC 1337 `File=…`
    /// payload. `extra` is appended to the args list (e.g. `;width=5`).
    fn iterm_osc(extra: &str) -> String {
        use base64::Engine;
        // 2×2 RGBA PNG — encoded via the `image` crate to stay aligned with
        // what real callers send and what our header peek expects.
        let img = image::RgbaImage::from_pixel(2, 2, image::Rgba([10, 20, 30, 255]));
        let mut png: Vec<u8> = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageOutputFormat::Png)
            .unwrap();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        format!("\x1b]1337;File=inline=1{}:{}\x07", extra, b64)
    }

    #[test]
    fn osc_1337_minimal_payload_queues_one_upload() {
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&iterm_osc(""));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        // Header peek runs synchronously inside `handle_osc_1337` — the
        // 2×2 dimensions of our fixture should round-trip.
        assert_eq!(uploads[0].pixel_size, Some((2, 2)));
        // Auto sizing when no width/height given.
        assert_eq!(uploads[0].width, ImageSizeSpec::Auto);
        assert_eq!(uploads[0].height, ImageSizeSpec::Auto);
        assert!(uploads[0].preserve_aspect);
        assert!(!uploads[0].do_not_move_cursor);
        // Bytes round-trip — re-decoding should give back the same image.
        assert!(crate::images::peek_dimensions(&uploads[0].bytes).is_some());
    }

    #[test]
    fn osc_1337_parses_explicit_cell_sizing() {
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&iterm_osc(";width=10;height=5"));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        assert_eq!(uploads[0].width, ImageSizeSpec::Cells(10));
        assert_eq!(uploads[0].height, ImageSizeSpec::Cells(5));
    }

    #[test]
    fn osc_1337_parses_pixel_and_percent_sizing() {
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&iterm_osc(";width=200px;height=50%"));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        assert_eq!(uploads[0].width, ImageSizeSpec::Pixels(200));
        assert_eq!(uploads[0].height, ImageSizeSpec::Percent(50));
    }

    #[test]
    fn osc_1337_preserve_aspect_off() {
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&iterm_osc(";preserveAspectRatio=0"));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        assert!(!uploads[0].preserve_aspect);
    }

    #[test]
    fn osc_1337_do_not_move_cursor_flag() {
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&iterm_osc(";doNotMoveCursor=1"));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        assert!(uploads[0].do_not_move_cursor);
    }

    #[test]
    fn osc_1337_inline_zero_is_dropped() {
        // inline=0 is download mode in iTerm; we have no download UI, so
        // the OSC must be a clean no-op rather than a partial decode.
        let mut t = Terminal::new(80, 24, 100);
        let payload = iterm_osc(";inline=0");
        // Strip the original `inline=1` so only `inline=0` is present.
        let payload = payload.replace("inline=1;", "");
        t.feed(&payload);
        assert!(t.take_pending_image_uploads().is_empty());
    }

    #[test]
    fn osc_1337_bad_base64_silently_dropped() {
        let mut t = Terminal::new(80, 24, 100);
        // `==` in the middle is not a valid base64 stream.
        t.feed("\x1b]1337;File=inline=1:not!base64!\x07");
        assert!(t.take_pending_image_uploads().is_empty());
    }

    #[test]
    fn osc_1337_missing_colon_silently_dropped() {
        // No `:` separator between args and body → no payload to decode.
        let mut t = Terminal::new(80, 24, 100);
        t.feed("\x1b]1337;File=inline=1;width=5\x07");
        assert!(t.take_pending_image_uploads().is_empty());
    }

    #[test]
    fn osc_1337_non_file_verb_silently_dropped() {
        // iTerm uses OSC 1337 for many things; we only handle File.
        let mut t = Terminal::new(80, 24, 100);
        t.feed("\x1b]1337;SetMark\x07");
        t.feed("\x1b]1337;CursorShape=1\x07");
        assert!(t.take_pending_image_uploads().is_empty());
    }

    #[test]
    fn osc_1337_unknown_keys_ignored_not_failed() {
        // iTerm contract: unknown keys are silently accepted so newer
        // params don't break older parsers.
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&iterm_osc(";futureKey=42;size=100;width=5"));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        assert_eq!(uploads[0].width, ImageSizeSpec::Cells(5));
    }

    #[test]
    fn osc_1337_st_terminator_accepted() {
        // The VT parser accepts BEL (0x07) or ESC \ (ST) — same payload
        // either way. Real iTerm callers use both.
        let mut t = Terminal::new(80, 24, 100);
        let with_bel = iterm_osc("");
        let with_st = with_bel.replace('\x07', "\x1b\\");
        t.feed(&with_st);
        assert_eq!(t.take_pending_image_uploads().len(), 1);
    }

    #[test]
    fn osc_1337_multiple_payloads_in_one_feed_all_queued() {
        let mut t = Terminal::new(80, 24, 100);
        let combo = format!("{}{}", iterm_osc(";width=5"), iterm_osc(";width=10"));
        t.feed(&combo);
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 2);
        assert_eq!(uploads[0].width, ImageSizeSpec::Cells(5));
        assert_eq!(uploads[1].width, ImageSizeSpec::Cells(10));
    }

    #[test]
    fn osc_1337_take_drains_queue() {
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&iterm_osc(""));
        let first = t.take_pending_image_uploads();
        assert_eq!(first.len(), 1);
        // Second take returns empty — pending list is owned-moved out.
        let second = t.take_pending_image_uploads();
        assert!(second.is_empty());
    }

    #[test]
    fn osc_1337_size_parser_handles_all_iterm_forms() {
        assert_eq!(parse_iterm_size("auto"), Some(ImageSizeSpec::Auto));
        assert_eq!(parse_iterm_size("AUTO"), Some(ImageSizeSpec::Auto));
        assert_eq!(parse_iterm_size(""), Some(ImageSizeSpec::Auto));
        assert_eq!(parse_iterm_size("5"), Some(ImageSizeSpec::Cells(5)));
        assert_eq!(parse_iterm_size("100px"), Some(ImageSizeSpec::Pixels(100)));
        assert_eq!(parse_iterm_size("50%"), Some(ImageSizeSpec::Percent(50)));
        // Unsupported units fall through to None — caller substitutes Auto.
        assert_eq!(parse_iterm_size("5em"), None);
        assert_eq!(parse_iterm_size("-1"), None);
    }

    //
    // XTWINOPS — what the kitty kitten queries on startup to learn cell
    // pixel size. Without these the kitten refuses to send images at all.
    //

    #[test]
    fn xtwinops_14_replies_with_text_area_pixel_size() {
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        t.feed("\x1b[14t");
        // height = rows * line_h = 24 * 16 = 384
        // width  = cols * cell_w = 80 * 8 = 640
        assert_eq!(t.take_response(), b"\x1b[4;384;640t");
    }

    #[test]
    fn xtwinops_16_replies_with_cell_pixel_size() {
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(9, 20);
        t.feed("\x1b[16t");
        // height = line_h = 20, width = cell_w = 9
        assert_eq!(t.take_response(), b"\x1b[6;20;9t");
    }

    #[test]
    fn xtwinops_18_replies_with_text_area_character_size() {
        let mut t = Terminal::new(80, 24, 100);
        t.feed("\x1b[18t");
        // rows=24, cols=80
        assert_eq!(t.take_response(), b"\x1b[8;24;80t");
    }

    #[test]
    fn xtwinops_unrelated_action_codes_are_ignored() {
        // 1 = de-iconify, 3 = move, 4 = resize, 5 = raise. Honoring
        // these would let any program move our window without consent.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        for ps in [1, 3, 4, 5, 22, 23] {
            t.feed(&format!("\x1b[{}t", ps));
            assert!(t.take_response().is_empty(), "ps={} should be silent", ps);
        }
    }

    #[test]
    fn xtwinops_uses_clamped_one_for_unset_cell_size() {
        // If State hasn't called set_cell_size_px yet, default is 1×1.
        // The kitten will still get a reply (no error), just a degenerate
        // one — better than nothing.
        let mut t = Terminal::new(80, 24, 100);
        t.feed("\x1b[14t");
        assert_eq!(t.take_response(), b"\x1b[4;24;80t");
    }

    //
    // K1.2: Kitty graphics-protocol control-data parser. Tests pin the
    // defaults (which apply when keys are omitted, common in real Kitty
    // payloads) and the action / format / transmission discriminants.
    //

    #[test]
    fn parse_kitty_control_empty_returns_defaults() {
        // Spec default when the control list is empty: a=T, f=100, t=d.
        let c = parse_kitty_control("").unwrap();
        assert_eq!(c.action, KittyAction::TransmitAndDisplay);
        assert_eq!(c.format, KittyFormat::Png);
        assert_eq!(c.transmission, KittyTransmission::Direct);
        assert!(!c.more_chunks);
        assert!(!c.do_not_move_cursor);
        assert_eq!(c.quiet, 0);
    }

    #[test]
    fn parse_kitty_control_action_variants() {
        assert_eq!(parse_kitty_control("a=t").unwrap().action, KittyAction::Transmit);
        assert_eq!(parse_kitty_control("a=T").unwrap().action, KittyAction::TransmitAndDisplay);
        assert_eq!(parse_kitty_control("a=q").unwrap().action, KittyAction::Query);
        assert_eq!(parse_kitty_control("a=p").unwrap().action, KittyAction::Place);
        assert_eq!(parse_kitty_control("a=d").unwrap().action, KittyAction::Delete);
        assert_eq!(parse_kitty_control("a=f").unwrap().action, KittyAction::AnimationFrame);
        assert_eq!(parse_kitty_control("a=a").unwrap().action, KittyAction::AnimationControl);
        // Unknown actions land on Other — the Kitty contract says
        // "unknown action = no-op", encoded as Other + dispatcher drop.
        assert_eq!(parse_kitty_control("a=Z").unwrap().action, KittyAction::Other);
    }

    #[test]
    fn parse_kitty_control_format_and_transmission() {
        assert_eq!(parse_kitty_control("f=100").unwrap().format, KittyFormat::Png);
        assert_eq!(parse_kitty_control("f=32").unwrap().format, KittyFormat::Rgba);
        assert_eq!(parse_kitty_control("f=24").unwrap().format, KittyFormat::Rgb);
        assert_eq!(parse_kitty_control("f=99").unwrap().format, KittyFormat::Other);
        assert_eq!(parse_kitty_control("t=d").unwrap().transmission, KittyTransmission::Direct);
        assert_eq!(parse_kitty_control("t=f").unwrap().transmission, KittyTransmission::File);
        assert_eq!(parse_kitty_control("t=s").unwrap().transmission, KittyTransmission::SharedMemory);
        assert_eq!(parse_kitty_control("t=x").unwrap().transmission, KittyTransmission::Other);
        assert_eq!(parse_kitty_control("t=t").unwrap().transmission, KittyTransmission::TempFile);
    }

    #[test]
    fn parse_kitty_control_ids_and_sizing() {
        let c = parse_kitty_control("i=42,p=7,c=10,r=5").unwrap();
        assert_eq!(c.image_id, Some(42));
        assert_eq!(c.placement_id, Some(7));
        assert_eq!(c.cells_cols, Some(10));
        assert_eq!(c.cells_rows, Some(5));
    }

    #[test]
    fn parse_kitty_control_chunking_and_cursor_and_quiet() {
        let c = parse_kitty_control("m=1,C=1,q=2").unwrap();
        assert!(c.more_chunks);
        assert!(c.do_not_move_cursor);
        assert_eq!(c.quiet, 2);
    }

    #[test]
    fn parse_kitty_control_unknown_keys_accepted() {
        // Forward-compat: unknown keys must not abort the parse.
        let c = parse_kitty_control("a=T,futureKey=99,o=z,i=1").unwrap();
        assert_eq!(c.action, KittyAction::TransmitAndDisplay);
        assert_eq!(c.image_id, Some(1));
    }

    #[test]
    fn parse_kitty_control_malformed_value_falls_back_to_default() {
        // i= with garbage → None (not Some(0)) so the dispatcher can
        // distinguish "no id given" from "id 0".
        let c = parse_kitty_control("i=notanumber,a=T").unwrap();
        assert_eq!(c.image_id, None);
        assert_eq!(c.action, KittyAction::TransmitAndDisplay);
    }

    #[test]
    fn parse_kitty_control_animation_keys_populate_overlapping_fields() {
        // `s=`, `v=`, `c=`, `r=`, `z=` are all overloaded between
        // transmission semantics and animation semantics. The parser
        // populates both alias fields; the dispatcher picks based on
        // action so a single raw value reaches the right place.
        let c = parse_kitty_control("a=f,i=7,r=3,c=2,z=50,X=4,Y=6,s=128,v=64").unwrap();
        assert_eq!(c.action, KittyAction::AnimationFrame);
        assert_eq!(c.image_id, Some(7));
        // r= is both cells_rows and anim_frame_num.
        assert_eq!(c.cells_rows, Some(3));
        assert_eq!(c.anim_frame_num, Some(3));
        // c= is both cells_cols and anim_compose_base / anim_make_current.
        assert_eq!(c.cells_cols, Some(2));
        assert_eq!(c.anim_compose_base, Some(2));
        assert_eq!(c.anim_make_current, Some(2));
        // z= is both z_index and anim_gap_ms.
        assert_eq!(c.z_index, Some(50));
        assert_eq!(c.anim_gap_ms, Some(50));
        // s= / v= remain source dimensions for `a=f` (raw pixel data).
        assert_eq!(c.source_w, Some(128));
        assert_eq!(c.source_h, Some(64));
        // ...and the animation aliases also get populated; the dispatcher
        // ignores them for `a=f`.
        assert_eq!(c.anim_control, Some(128));
        assert_eq!(c.anim_loop_count, Some(64));
    }

    #[test]
    fn parse_kitty_control_animation_control_keys() {
        // `a=a,i=1,s=3,v=0` → run with infinite loops.
        let c = parse_kitty_control("a=a,i=1,s=3,v=0").unwrap();
        assert_eq!(c.action, KittyAction::AnimationControl);
        assert_eq!(c.image_id, Some(1));
        assert_eq!(c.anim_control, Some(3));
        assert_eq!(c.anim_loop_count, Some(0));
        // `a=a,i=1,c=4` → make frame 4 current.
        let c = parse_kitty_control("a=a,i=1,c=4").unwrap();
        assert_eq!(c.anim_make_current, Some(4));
        // `a=a,i=1,r=2,z=200` → edit frame 2 gap to 200ms.
        let c = parse_kitty_control("a=a,i=1,r=2,z=200").unwrap();
        assert_eq!(c.anim_frame_num, Some(2));
        assert_eq!(c.anim_gap_ms, Some(200));
    }

    #[test]
    fn a_f_does_not_create_a_placement() {
        // Frame transmissions are metadata for the parent image, not
        // their own visible objects. The dispatcher must queue them
        // with `display_immediately: false` and `cell_extent: (0, 0)`
        // so main.rs's drain skips placement creation.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        let png = {
            let buf = image::RgbaImage::from_pixel(2, 2, image::Rgba([1, 2, 3, 255]));
            let mut bytes = Vec::new();
            image::DynamicImage::ImageRgba8(buf)
                .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageOutputFormat::Png)
                .expect("encode");
            bytes
        };
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        let apc = format!("\x1b_Ga=f,f=100,i=7,z=20;{}\x1b\\", b64);
        t.feed(&apc);
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        let up = &uploads[0];
        assert!(!up.display_immediately, "a=f must not display");
        assert_eq!(up.cell_extent, (0, 0), "a=f has no cell extent of its own");
        assert!(
            up.animation_frame.is_some(),
            "a=f must flag the animation_frame spec so the drain routes correctly",
        );
        assert_eq!(t.cursor().row, 0, "a=f must not advance the cursor");
        assert_eq!(t.cursor().col, 0);
    }

    #[test]
    fn a_f_chunked_assembles_into_one_upload() {
        // The chunking path on `a=f` mirrors `a=t`'s — split the
        // payload across multiple APCs with `m=1`, then a final `m=0`
        // chunk to flush. Only one PendingImageUpload should fall out
        // of the queue, carrying the assembled bytes.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        let png = {
            let buf = image::RgbaImage::from_pixel(3, 3, image::Rgba([9, 9, 9, 255]));
            let mut bytes = Vec::new();
            image::DynamicImage::ImageRgba8(buf)
                .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageOutputFormat::Png)
                .expect("encode");
            bytes
        };
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        let mid = b64.len() / 2;
        let (head, tail) = b64.split_at(mid);
        let apc1 = format!("\x1b_Ga=f,f=100,i=9,m=1;{}\x1b\\", head);
        let apc2 = format!("\x1b_Ga=f,f=100,i=9,m=0;{}\x1b\\", tail);
        t.feed(&apc1);
        // First chunk must not produce an upload — it's still in the
        // accumulator.
        assert!(
            t.take_pending_image_uploads().is_empty(),
            "intermediate m=1 chunk must not queue an upload",
        );
        t.feed(&apc2);
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1, "final chunk must finalize");
        let up = &uploads[0];
        assert!(up.animation_frame.is_some());
        assert!(up.bytes.starts_with(b"\x89PNG"), "assembled bytes look like PNG");
    }

    #[test]
    fn parse_kitty_control_capital_i_is_alias_for_image_id() {
        // icat uses I= (image number) for transmissions, not i=. The
        // protocol distinguishes them — number is meant to be a
        // client-side counter that the terminal maps to a real id —
        // but for our renderer's purposes the number works as the
        // identifier directly.
        let c = parse_kitty_control("I=60091135").unwrap();
        assert_eq!(c.image_id, Some(60091135));
        // If both keys are present, the lowercase `i=` wins.
        let c = parse_kitty_control("i=5,I=99").unwrap();
        assert_eq!(c.image_id, Some(5));
    }

    #[test]
    fn a_f_size_inference_picks_rgba_when_byte_count_matches_w_h_4() {
        // icat sends some a=f frames with no `f=` and raw RGBA
        // payload (w*h*4 bytes). The dispatcher must infer RGBA from
        // the byte count — *not* default to the base's f=24 or to
        // PNG. Mis-inference causes shifted rows that look like
        // tiled/repeating artifacts and the corruption compounds
        // across delta frames since each composes onto the previous.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        // Base: f=24 RGB, so kitty_image_formats[42] = Rgb.
        use base64::Engine;
        let raw_rgb_base: Vec<u8> = (0..4 * 4 * 3).map(|_| 0xAAu8).collect();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&raw_rgb_base);
        t.feed(&format!("\x1b_Ga=T,f=24,s=4,v=4,I=42;{}\x1b\\", b64));
        let _ = t.take_pending_image_uploads();

        // a=f frame, NO `f=`, payload is exactly w*h*4 bytes (RGBA).
        // Size inference picks RGBA; the raw-bypass path hands the
        // bytes through unchanged with `raw_rgba_dims` set.
        let raw_rgba_frame: Vec<u8> = (0..2 * 2 * 4).map(|_| 0x44u8).collect();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&raw_rgba_frame);
        t.feed(&format!("\x1b_Ga=f,s=2,v=2,I=42,z=100;{}\x1b\\", b64));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1, "frame queues despite omitted f=");
        let up = &uploads[0];
        assert!(up.animation_frame.is_some());
        assert_eq!(up.raw_rgba_dims, Some((2, 2)), "RGBA inferred and bypass taken");
        assert_eq!(up.bytes, raw_rgba_frame);
    }

    #[test]
    fn a_f_size_inference_picks_rgb_when_byte_count_matches_w_h_3() {
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        // Base is f=32 RGBA — so base-format inheritance would say
        // "RGBA" but the actual frame payload is RGB. Inference
        // must use the byte count to pick RGB instead.
        use base64::Engine;
        let raw_rgba_base: Vec<u8> = (0..4 * 4 * 4).map(|_| 0xAAu8).collect();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&raw_rgba_base);
        t.feed(&format!("\x1b_Ga=T,f=32,s=4,v=4,I=42;{}\x1b\\", b64));
        let _ = t.take_pending_image_uploads();

        // a=f with no f=, payload is exactly w*h*3 → must infer RGB.
        // Raw-bypass pads to RGBA (alpha=255).
        let raw_rgb_frame: Vec<u8> = (0..2 * 2 * 3).map(|_| 0x44u8).collect();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&raw_rgb_frame);
        t.feed(&format!("\x1b_Ga=f,s=2,v=2,I=42,z=100;{}\x1b\\", b64));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        let up = &uploads[0];
        assert!(up.animation_frame.is_some());
        assert_eq!(up.raw_rgba_dims, Some((2, 2)));
        assert_eq!(up.bytes.len(), 2 * 2 * 4);
        for px in up.bytes.chunks_exact(4) {
            assert_eq!(px[0..3], [0x44, 0x44, 0x44]);
            assert_eq!(px[3], 0xFF, "alpha padded to opaque for RGB input");
        }
    }

    #[test]
    fn a_f_mid_gap_byte_count_falls_back_to_base_format_not_png() {
        // Repro for "image decode failed: The image format could not
        // be determined" on kitty animations: a frame whose inflated
        // byte count sits between w*h*3+PAGE and w*h*4 used to fall
        // through every byte-count branch and end up at PNG (the
        // parser's "no f= seen" sentinel). The PNG decoder can't
        // recognize raw RGB(A) and prints the user-visible error.
        // With the fix, the resolver consults the base format and
        // accepts RGB/RGBA so the raw-bypass path runs.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        use base64::Engine;
        // Base: f=24 RGB at 4x4 → kitty_image_formats[42] = Rgb.
        let raw_rgb_base: Vec<u8> = (0..4 * 4 * 3).map(|_| 0x11u8).collect();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&raw_rgb_base);
        t.feed(&format!("\x1b_Ga=T,f=24,s=4,v=4,I=42;{}\x1b\\", b64));
        let _ = t.take_pending_image_uploads();

        // Frame omits f=. Build a payload sized to land in the gap
        // between w*h*3+PAGE and w*h*4 for w=h=64 (rgb=12288,
        // rgba=16384, PAGE=16384 so the +PAGE windows cover both —
        // pick larger dims to expose the gap).
        let (w, h) = (450u32, 450u32);
        let rgb = (w as usize) * (h as usize) * 3; // 607500
        let rgba = (w as usize) * (h as usize) * 4; // 810000
        let mid = rgb + 16 * 1024 + 10_000; // 633884 — past rgb+PAGE, under rgba
        assert!(mid > rgb + 16 * 1024);
        assert!(mid < rgba);
        let mid_payload = vec![0x77u8; mid];
        let b64 = base64::engine::general_purpose::STANDARD.encode(&mid_payload);
        t.feed(&format!("\x1b_Ga=f,s={},v={},I=42,z=10;{}\x1b\\", w, h, b64));
        let uploads = t.take_pending_image_uploads();
        // Frame queues via the raw-bypass path. Without the fix this
        // assertion fired because the frame got routed through the
        // PNG worker and was rejected before producing an upload.
        assert_eq!(uploads.len(), 1, "mid-gap frame must not be dropped");
        let up = &uploads[0];
        assert!(up.animation_frame.is_some());
        assert!(up.raw_rgba_dims.is_some(), "raw-bypass path taken (not PNG decoder)");
    }

    #[test]
    fn a_f_uses_lowercase_xy_as_destination_position() {
        // icat's a=f messages carry the frame's parent-coords via
        // lowercase `x=` / `y=` (not capital X/Y). On a=T those keys
        // mean source-crop; on a=f they mean "where in the parent
        // does this frame go". A regression that reads from
        // pixel_offset_x/y instead would stamp every frame at (0, 0)
        // and the animation would visibly distort.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        use base64::Engine;
        let raw_rgba = vec![0u8; 4 * 4 * 4];
        let b64 = base64::engine::general_purpose::STANDARD.encode(&raw_rgba);
        // Base.
        t.feed(&format!("\x1b_Ga=T,f=32,s=4,v=4,I=77;{}\x1b\\", b64));
        let _ = t.take_pending_image_uploads();
        // Frame with lowercase x=10, y=20 (and capital X/Y absent).
        let frame_rgba = vec![1u8; 2 * 2 * 4];
        let b64 = base64::engine::general_purpose::STANDARD.encode(&frame_rgba);
        t.feed(&format!(
            "\x1b_Ga=f,f=32,s=2,v=2,x=10,y=20,I=77,z=50;{}\x1b\\",
            b64,
        ));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        let spec = uploads[0]
            .animation_frame
            .as_ref()
            .expect("animation_frame populated");
        assert_eq!(spec.dst_x, 10, "lowercase x= must reach dst_x");
        assert_eq!(spec.dst_y, 20, "lowercase y= must reach dst_y");
    }

    #[test]
    fn a_t_base_image_with_omitted_f_infers_rgb_format() {
        // Regression: icat sometimes sends a=T with no f=. Payload
        // is raw RGB/RGBA but the parser defaults f= to PNG; the
        // dispatch path must run size-based inference. After the
        // raw-bypass perf change, raw inputs no longer round-trip
        // through PNG — they ride straight to the GPU upload path
        // via `raw_rgba_dims`. Assert the bypass marker is set with
        // the right dims and the bytes are raw RGBA (4 bytes/pixel,
        // alpha=255 padded from the RGB input).
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        use base64::Engine;
        let raw_rgb: Vec<u8> = (0..4 * 4 * 3).map(|i| (i % 256) as u8).collect();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&raw_rgb);
        t.feed(&format!("\x1b_Ga=T,s=4,v=4,I=99;{}\x1b\\", b64));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1, "base normalizes despite omitted f=");
        let up = &uploads[0];
        assert_eq!(up.kitty_image_id, Some(99));
        assert_eq!(
            up.raw_rgba_dims,
            Some((4, 4)),
            "raw RGB took the worker-bypass path",
        );
        assert_eq!(up.bytes.len(), 4 * 4 * 4, "RGB padded to RGBA");
        // Every 4th byte should be 0xFF (padded alpha).
        for px in up.bytes.chunks_exact(4) {
            assert_eq!(px[3], 0xFF);
        }
        // Subsequent a=f frames also bypass when the format inference
        // (or inherited base format) lands on a raw variant.
        let raw_rgb_frame: Vec<u8> = (0..2 * 2 * 3).map(|_| 0x33u8).collect();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&raw_rgb_frame);
        t.feed(&format!("\x1b_Ga=f,s=2,v=2,I=99,z=30;{}\x1b\\", b64));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        assert_eq!(uploads[0].raw_rgba_dims, Some((2, 2)));
        assert_eq!(uploads[0].bytes.len(), 2 * 2 * 4);
    }

    #[test]
    fn a_t_base_image_with_omitted_f_infers_rgba_format() {
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        use base64::Engine;
        // 4*4*4 byte payload → infer RGBA. After the raw-bypass
        // perf change, the dispatcher hands the bytes through
        // unchanged with `raw_rgba_dims` set.
        let raw_rgba: Vec<u8> = (0..4 * 4 * 4).map(|i| (i % 256) as u8).collect();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&raw_rgba);
        t.feed(&format!("\x1b_Ga=T,s=4,v=4,I=100;{}\x1b\\", b64));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        assert_eq!(uploads[0].raw_rgba_dims, Some((4, 4)));
        assert_eq!(uploads[0].bytes.len(), 4 * 4 * 4);
        // Bytes should be the raw payload truncated to expected size.
        assert_eq!(uploads[0].bytes, raw_rgba);
    }

    #[test]
    fn a_f_size_inference_keeps_png_when_payload_has_png_signature() {
        // App that genuinely sends PNG with no `f=100`. The 89 50 4E
        // 47 ... signature trumps everything else.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        // Build a real PNG.
        let png = {
            let buf = image::RgbaImage::from_pixel(4, 4, image::Rgba([1, 2, 3, 255]));
            let mut bytes = Vec::new();
            image::DynamicImage::ImageRgba8(buf)
                .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageOutputFormat::Png)
                .expect("encode");
            bytes
        };
        use base64::Engine;
        // Base also PNG so kitty_image_formats[42] = Png.
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        t.feed(&format!("\x1b_Ga=T,I=42;{}\x1b\\", b64));
        let _ = t.take_pending_image_uploads();

        // a=f with no f= and PNG signature → keep as PNG even though
        // a 4x4 image's byte count happens to land in some range.
        let frame_png = {
            let buf = image::RgbaImage::from_pixel(2, 2, image::Rgba([9, 9, 9, 255]));
            let mut bytes = Vec::new();
            image::DynamicImage::ImageRgba8(buf)
                .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageOutputFormat::Png)
                .expect("encode");
            bytes
        };
        let b64 = base64::engine::general_purpose::STANDARD.encode(&frame_png);
        t.feed(&format!("\x1b_Ga=f,I=42,z=100;{}\x1b\\", b64));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        let up = &uploads[0];
        assert!(up.animation_frame.is_some());
        // The bytes are still PNG (not re-encoded by the raw normalizer).
        assert!(up.bytes.starts_with(b"\x89PNG"));
    }

    #[test]
    fn parse_kitty_control_negative_z_doesnt_populate_gap_ms() {
        // `z=-1` parses as i32 z_index but fails u32 anim_gap_ms.
        // Important: dispatcher uses anim_gap_ms for animation paths,
        // so a negative z must not silently set a wrap-around gap.
        let c = parse_kitty_control("z=-1").unwrap();
        assert_eq!(c.z_index, Some(-1));
        assert_eq!(c.anim_gap_ms, None);
    }

    //
    // K1.3 / K1.4: Kitty graphics dispatch end-to-end. Builds a small
    // PNG, wraps it in an APC, feeds it through `Terminal::feed`, and
    // verifies the upload queue + cursor state. Same shape as the
    // `osc_1337_*` tests but for the Kitty wire format.
    //

    /// Build a Kitty graphics APC wrapping `png_bytes`. `args` is the
    /// pre-`;` control string (e.g. `"a=T,f=100,c=5,r=2"`).
    fn kitty_apc(args: &str, png_bytes: &[u8]) -> String {
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(png_bytes);
        format!("\x1b_G{};{}\x1b\\", args, b64)
    }

    /// Same as `kitty_apc` but with a control-only payload (no `;`).
    /// Used for `a=q` queries that carry no image data.
    fn kitty_apc_control_only(args: &str) -> String {
        format!("\x1b_G{}\x1b\\", args)
    }

    /// Re-encode the same fixture PNG that the iTerm tests use, so we
    /// know `peek_dimensions` will return Some((w, h)).
    fn kitty_png(w: u32, h: u32) -> Vec<u8> {
        let buf = image::RgbaImage::from_pixel(w, h, image::Rgba([0, 128, 255, 255]));
        let mut bytes: Vec<u8> = Vec::new();
        image::DynamicImage::ImageRgba8(buf)
            .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageOutputFormat::Png)
            .unwrap();
        bytes
    }

    #[test]
    fn kitty_apc_single_chunk_queues_one_upload() {
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        let png = kitty_png(4, 4);
        t.feed(&kitty_apc("a=T,f=100,c=2,r=1", &png));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        assert_eq!(uploads[0].cell_extent, (1, 2));
        assert_eq!(uploads[0].pixel_size, Some((4, 4)));
        // Cursor advanced by 1 row (cell_extent.0).
        assert_eq!(t.cursor().row, 1);
    }

    #[test]
    fn kitty_apc_default_action_is_transmit_and_display() {
        // `a=` omitted → default T per spec → image displays.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        let png = kitty_png(2, 2);
        t.feed(&kitty_apc("f=100,c=3,r=2", &png));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        assert_eq!(uploads[0].cell_extent, (2, 3));
    }

    #[test]
    fn kitty_apc_no_sizing_falls_back_to_image_native_extent() {
        // c/r omitted → Auto → cell extent = ceil(pixels / cell_size).
        // 16x16 image, 8x16 cell → 1 row × 2 cols.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        let png = kitty_png(16, 16);
        t.feed(&kitty_apc("a=T,f=100", &png));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        assert_eq!(uploads[0].cell_extent, (1, 2));
    }

    #[test]
    fn kitty_apc_do_not_move_cursor() {
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        t.feed("\x1b[10;1H"); // CUP row 10 col 1 → cursor.row = 9
        let png = kitty_png(4, 4);
        t.feed(&kitty_apc("a=T,f=100,c=2,r=1,C=1", &png));
        // C=1 must keep the cursor at row 9, NOT advance.
        assert_eq!(t.cursor().row, 9);
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads[0].cell_anchor, (9, 0));
    }

    #[test]
    fn kitty_apc_chunked_transmission_assembles_full_payload() {
        // Split the same payload across three APCs (m=1, m=1, m=0) with
        // the same i=. The accumulator must concatenate them in order
        // and only emit the upload on the terminator.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        let png = kitty_png(4, 4);
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        let chunk_size = (b64.len() / 3 + 1).max(4);
        let c1 = &b64[..chunk_size];
        let c2 = &b64[chunk_size..2 * chunk_size];
        let c3 = &b64[2 * chunk_size..];

        // First chunk carries the sizing.
        t.feed(&format!("\x1b_Ga=T,f=100,c=2,r=1,i=42,m=1;{}\x1b\\", c1));
        assert!(t.take_pending_image_uploads().is_empty());
        // Middle chunk continues — only `i=` and `m=1` matter, sizing
        // here would be ignored (and we leave it absent to verify that).
        t.feed(&format!("\x1b_Gi=42,m=1;{}\x1b\\", c2));
        assert!(t.take_pending_image_uploads().is_empty());
        // Terminal chunk — m=0 (or omitted; this test uses omitted to
        // pin the "absent m means last" path).
        t.feed(&format!("\x1b_Gi=42;{}\x1b\\", c3));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        assert_eq!(uploads[0].cell_extent, (1, 2));
        // Round-trip the payload: peek_dimensions on the assembled
        // bytes should still see 4×4.
        assert_eq!(uploads[0].pixel_size, Some((4, 4)));
    }

    #[test]
    fn kitty_apc_chunked_continuation_chunks_without_i_thread_to_first_chunks_id() {
        // Regression for the tmux-icat tofu bug: `kitten icat` puts
        // `i=` only on the FIRST chunk of a chunked transmission and
        // omits it on every continuation (and on the terminator).
        // Without the `current_chunked_id` threading in `handle_apc`,
        // continuation chunks fall into the anonymous bucket and the
        // id-keyed entry leaks — `register_kitty_image_id` never
        // fires, so placeholder cells later resolve to nothing.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        let png = kitty_png(4, 4);
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        let chunk_size = (b64.len() / 3 + 1).max(4);
        let c1 = &b64[..chunk_size];
        let c2 = &b64[chunk_size..2 * chunk_size];
        let c3 = &b64[2 * chunk_size..];
        // First chunk: i=43 (carries the sizing).
        t.feed(&format!("\x1b_Ga=T,f=100,c=2,r=1,i=43,m=1;{}\x1b\\", c1));
        // Continuation: NO i=, just m=1 — must still land in the id=43 entry.
        t.feed(&format!("\x1b_Ga=T,m=1;{}\x1b\\", c2));
        // Final chunk: NO i=, NO m= — must finalize the id=43 entry.
        t.feed(&format!("\x1b_Ga=T;{}\x1b\\", c3));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1, "id=43 stream must finalize");
        assert_eq!(uploads[0].kitty_image_id, Some(43));
        assert_eq!(uploads[0].pixel_size, Some((4, 4)));
    }

    #[test]
    fn kitty_apc_chunked_id_cleared_after_terminator() {
        // After an id-keyed chunked stream finalizes, a subsequent
        // unrelated chunked-without-id transmission must NOT inherit
        // the stale id. Otherwise a second image would pile onto the
        // first's bucket and corrupt both.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        let png = kitty_png(4, 4);
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        let mid = b64.len() / 2;
        // First image: id=43, two chunks, continuation drops `i=`.
        t.feed(&format!("\x1b_Ga=T,f=100,c=2,r=1,i=43,m=1;{}\x1b\\", &b64[..mid]));
        t.feed(&format!("\x1b_Ga=T;{}\x1b\\", &b64[mid..]));
        let first = t.take_pending_image_uploads();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].kitty_image_id, Some(43));
        // Second image: chunked, never says `i=`. Must go to the
        // anonymous bucket — NOT into a stale id=43 entry.
        t.feed(&format!("\x1b_Ga=T,f=100,c=2,r=1,m=1;{}\x1b\\", &b64[..mid]));
        t.feed(&format!("\x1b_Ga=T;{}\x1b\\", &b64[mid..]));
        let second = t.take_pending_image_uploads();
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].kitty_image_id, None);
    }

    #[test]
    fn kitty_apc_a_f_continuation_chunks_without_i_thread_to_first_chunks_id() {
        // Same threading guarantee for animation frames: icat sends
        // `i=43` on the first `a=f` chunk and omits it on
        // continuations. Without injection the continuation chunks
        // get dropped on the floor at the `client_id` guard at the
        // top of `handle_apc_animation_frame`.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        // Base image so kitty_image_formats[43] is recorded.
        use base64::Engine;
        let base_rgba = vec![0u8; 4 * 4 * 4];
        let b64 = base64::engine::general_purpose::STANDARD.encode(&base_rgba);
        t.feed(&format!("\x1b_Ga=T,f=32,s=4,v=4,i=43;{}\x1b\\", b64));
        let _ = t.take_pending_image_uploads();
        // Frame in 3 chunks; only the first carries `i=43`.
        let frame: Vec<u8> = vec![0xCDu8; 2 * 2 * 4];
        let b64 = base64::engine::general_purpose::STANDARD.encode(&frame);
        let chunk = (b64.len() / 3 + 1).max(4);
        let c1 = &b64[..chunk];
        let c2 = &b64[chunk..2 * chunk];
        let c3 = &b64[2 * chunk..];
        t.feed(&format!("\x1b_Ga=f,f=32,s=2,v=2,i=43,m=1;{}\x1b\\", c1));
        t.feed(&format!("\x1b_Ga=f,m=1;{}\x1b\\", c2));
        t.feed(&format!("\x1b_Ga=f;{}\x1b\\", c3));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1, "frame must finalize despite missing i=");
        assert!(uploads[0].animation_frame.is_some());
    }

    #[test]
    fn kitty_apc_a_f_chunked_frame_keeps_first_chunks_gap_ms() {
        // Regression for "animations run way too fast" under tmux.
        // `kitten icat` puts `z=N` (gap_ms), `x=`/`y=` (dst), `r=`
        // (target_slot), `c=` (compose_base) only on the FIRST `a=f`
        // chunk and omits them on continuations. The chunked
        // finalize was reading these off the LAST chunk's `ctrl`,
        // which zeroed them all — so every frame's gap_ms came out
        // as 0 and the playback ran at the 1ms floor (~1000 fps).
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        use base64::Engine;
        // Base so kitty_image_formats[43] is recorded.
        let base_rgba = vec![0u8; 4 * 4 * 4];
        let b64 = base64::engine::general_purpose::STANDARD.encode(&base_rgba);
        t.feed(&format!("\x1b_Ga=T,f=32,s=4,v=4,i=43;{}\x1b\\", b64));
        let _ = t.take_pending_image_uploads();
        // Frame in 2 chunks. First carries z=100 (gap_ms), x=5,
        // y=7, r=2 (target_slot), c=1 (compose_base). Second drops
        // all of them along with `i=`.
        let frame: Vec<u8> = vec![0xCDu8; 2 * 2 * 4];
        let b64 = base64::engine::general_purpose::STANDARD.encode(&frame);
        let mid = b64.len() / 2;
        t.feed(&format!(
            "\x1b_Ga=f,f=32,s=2,v=2,i=43,z=100,x=5,y=7,r=2,c=1,m=1;{}\x1b\\",
            &b64[..mid],
        ));
        t.feed(&format!("\x1b_Ga=f;{}\x1b\\", &b64[mid..]));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        let frame_spec = uploads[0]
            .animation_frame
            .as_ref()
            .expect("animation_frame spec present");
        assert_eq!(frame_spec.gap_ms, 100, "gap_ms preserved from first chunk");
        assert_eq!(frame_spec.dst_x, 5, "dst_x preserved from first chunk");
        assert_eq!(frame_spec.dst_y, 7, "dst_y preserved from first chunk");
        assert_eq!(frame_spec.target_slot, Some(2));
        assert_eq!(frame_spec.compose_base, Some(1));
    }

    #[test]
    fn kitty_apc_chunked_uses_first_chunks_sizing_not_last() {
        // First chunk: c=10. Last chunk: c=3 (would be ignored). Pin
        // the contract: first-chunk sizing wins.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        let png = kitty_png(4, 4);
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        let mid = b64.len() / 2;
        t.feed(&format!("\x1b_Ga=T,f=100,c=10,r=2,i=7,m=1;{}\x1b\\", &b64[..mid]));
        t.feed(&format!("\x1b_Ga=T,c=3,r=99,i=7;{}\x1b\\", &b64[mid..]));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        assert_eq!(uploads[0].cell_extent, (2, 10));
    }

    #[test]
    fn kitty_apc_unknown_format_silently_dropped() {
        // Unknown `f=` value (`f=99`) lands on KittyFormat::Other and
        // is silently dropped. f=24/32/100 are all wired up now —
        // see their focused tests.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        let png = kitty_png(4, 4);
        t.feed(&kitty_apc("a=T,f=99,c=2,r=1", &png));
        assert!(t.take_pending_image_uploads().is_empty());
    }

    #[test]
    fn kitty_apc_unsupported_transmission_silently_dropped() {
        // Unknown t= value falls into KittyTransmission::Other and
        // drops cleanly. (t=f / t=t / t=s are all implemented now —
        // see their own focused tests.)
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        let png = kitty_png(4, 4);
        for medium in ["x", "Q"] {
            t.feed(&kitty_apc(&format!("a=T,f=100,t={},c=2,r=1", medium), &png));
            assert!(
                t.take_pending_image_uploads().is_empty(),
                "t={} should drop",
                medium
            );
        }
    }

    #[test]
    fn kitty_apc_t_f_reads_file_from_disk() {
        // Real kitty +kitten icat picks `t=f` (file) by default for
        // local PNG inputs — the payload is a base64-encoded UTF-8
        // path. Write a PNG to a temp file, point an APC at it, and
        // assert the queue receives the file's bytes intact.
        use base64::Engine;
        let png = kitty_png(4, 4);
        let dir = std::env::temp_dir();
        let path = dir.join(format!("yutani-kitty-test-{}.png", std::process::id()));
        std::fs::write(&path, &png).expect("write temp png");
        let path_b64 = base64::engine::general_purpose::STANDARD.encode(path.to_str().unwrap());

        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        t.feed(&format!("\x1b_Ga=T,f=100,t=f,c=2,r=1;{}\x1b\\", path_b64));

        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1, "file-transmission upload should land");
        assert_eq!(uploads[0].bytes, png);
        assert_eq!(uploads[0].pixel_size, Some((4, 4)));
        assert_eq!(uploads[0].cell_extent, (1, 2));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn kitty_apc_t_f_missing_file_drops_silently() {
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD
            .encode("/definitely/does/not/exist.png");
        t.feed(&format!("\x1b_Ga=T,f=100,t=f;{}\x1b\\", b64));
        assert!(t.take_pending_image_uploads().is_empty());
    }

    #[test]
    fn kitty_apc_t_f_non_utf8_path_drops_silently() {
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        use base64::Engine;
        // Invalid UTF-8 in the path → reject before touching the
        // filesystem. (A real path with non-UTF-8 bytes on macOS would
        // also be rejected — kitty's spec implies UTF-8 paths.)
        let b64 = base64::engine::general_purpose::STANDARD.encode([0xFF, 0xFE, 0xFD]);
        t.feed(&format!("\x1b_Ga=T,f=100,t=f;{}\x1b\\", b64));
        assert!(t.take_pending_image_uploads().is_empty());
    }

    //
    // K6: spec corners (X/Y, z, x/y/w/h source crops, o=z zlib).
    //

    #[test]
    fn parse_kitty_control_pixel_offset_and_z() {
        let c = parse_kitty_control("X=3,Y=7,z=-5").unwrap();
        assert_eq!(c.pixel_offset_x, Some(3));
        assert_eq!(c.pixel_offset_y, Some(7));
        assert_eq!(c.z_index, Some(-5));
    }

    #[test]
    fn parse_kitty_control_source_crop_quad() {
        let c = parse_kitty_control("x=10,y=20,w=100,h=80").unwrap();
        assert_eq!(c.crop_x, Some(10));
        assert_eq!(c.crop_y, Some(20));
        assert_eq!(c.crop_w, Some(100));
        assert_eq!(c.crop_h, Some(80));
    }

    #[test]
    fn parse_kitty_control_zlib_compression() {
        let c = parse_kitty_control("o=z").unwrap();
        assert!(c.compressed_zlib);
        // Only `o=z` enables — other values (none defined yet) don't.
        let c = parse_kitty_control("o=q").unwrap();
        assert!(!c.compressed_zlib);
    }

    #[test]
    fn kitty_placement_params_partial_crop_returns_none() {
        // x/y/w/h is all-or-nothing per spec. Partial sets fall back to
        // None so the renderer samples the whole image.
        let mut ctrl = KittyControl::default();
        ctrl.crop_x = Some(10);
        ctrl.crop_w = Some(50);
        let (_, _, src) = kitty_placement_params(&ctrl);
        assert!(src.is_none());
    }

    #[test]
    fn kitty_placement_params_full_crop_passes_through() {
        let mut ctrl = KittyControl::default();
        ctrl.crop_x = Some(1);
        ctrl.crop_y = Some(2);
        ctrl.crop_w = Some(3);
        ctrl.crop_h = Some(4);
        let (_, _, src) = kitty_placement_params(&ctrl);
        assert_eq!(src, Some((1, 2, 3, 4)));
    }

    #[test]
    fn kitty_placement_params_zero_crop_size_falls_back_to_none() {
        // A degenerate w=0 / h=0 crop is treated as "no crop" — the
        // renderer can't sample a zero-size rect.
        let mut ctrl = KittyControl::default();
        ctrl.crop_x = Some(0);
        ctrl.crop_y = Some(0);
        ctrl.crop_w = Some(0);
        ctrl.crop_h = Some(10);
        let (_, _, src) = kitty_placement_params(&ctrl);
        assert!(src.is_none());
    }

    #[test]
    fn kitty_placement_params_defaults_zero() {
        let ctrl = KittyControl::default();
        let (offset, z, src) = kitty_placement_params(&ctrl);
        assert_eq!(offset, (0, 0));
        assert_eq!(z, 0);
        assert!(src.is_none());
    }

    #[test]
    fn kitty_apc_with_pixel_offset_and_z_lands_on_pending_upload() {
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        let png = kitty_png(4, 4);
        t.feed(&kitty_apc("a=T,f=100,c=2,r=1,X=4,Y=8,z=42", &png));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        assert_eq!(uploads[0].pixel_offset, (4, 8));
        assert_eq!(uploads[0].z_index, 42);
    }

    #[test]
    fn kitty_apc_with_source_crop_lands_on_pending_upload() {
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        let png = kitty_png(10, 10);
        t.feed(&kitty_apc("a=T,f=100,c=2,r=1,x=2,y=3,w=5,h=4", &png));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        assert_eq!(uploads[0].src_rect, Some((2, 3, 5, 4)));
    }

    #[test]
    fn kitty_apc_a_p_threads_pixel_offset_and_z_to_placement() {
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        t.register_kitty_image_id(7, ImageId(99));
        t.feed(&kitty_apc_control_only("a=p,i=7,c=2,r=1,X=3,Y=5,z=-2"));
        let placements = t.live_placements();
        assert_eq!(placements.len(), 1);
        assert_eq!(placements[0].pixel_offset, (3, 5));
        assert_eq!(placements[0].z, -2);
    }

    #[test]
    fn kitty_apc_o_z_zlib_inflates_before_decode() {
        // Compress a real PNG with zlib, send via t=d,o=z, assert the
        // inflated bytes round-trip into a working upload.
        use base64::Engine;
        use std::io::Write;
        let png = kitty_png(4, 4);
        let mut encoder = flate2::write::ZlibEncoder::new(
            Vec::new(),
            flate2::Compression::default(),
        );
        encoder.write_all(&png).unwrap();
        let compressed = encoder.finish().unwrap();
        // Compression of a tiny PNG often INFLATES because of headers
        // — that's fine for the test; the inflate path still has to work.
        let b64 = base64::engine::general_purpose::STANDARD.encode(&compressed);
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        t.feed(&format!("\x1b_Ga=T,f=100,o=z,c=2,r=1;{}\x1b\\", b64));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        assert_eq!(uploads[0].pixel_size, Some((4, 4)));
    }

    #[test]
    fn kitty_apc_o_z_malformed_zlib_silently_dropped() {
        // Garbage bytes claimed as zlib — inflate fails, no panic, no
        // partial upload.
        use base64::Engine;
        let bogus = base64::engine::general_purpose::STANDARD.encode(b"not zlib at all");
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        t.feed(&format!("\x1b_Ga=T,f=100,o=z;{}\x1b\\", bogus));
        assert!(t.take_pending_image_uploads().is_empty());
    }

    //
    // K3: POSIX shared-memory transmission (`t=s`). Unix-only — the
    // tests create a real SHM segment via libc, populate it, send the
    // APC, assert the upload landed AND the segment was unlinked.
    //

    #[cfg(unix)]
    fn write_shm(name: &str, bytes: &[u8]) {
        use std::ffi::CString;
        let c_name = CString::new(name).unwrap();
        unsafe {
            // O_CREAT | O_RDWR | 0o600 — caller is responsible for an
            // earlier unlink if reusing a name.
            let fd = libc::shm_open(
                c_name.as_ptr(),
                libc::O_CREAT | libc::O_RDWR,
                0o600,
            );
            assert!(fd >= 0, "shm_open failed");
            let r = libc::ftruncate(fd, bytes.len() as libc::off_t);
            assert_eq!(r, 0);
            let p = libc::mmap(
                std::ptr::null_mut(),
                bytes.len(),
                libc::PROT_WRITE | libc::PROT_READ,
                libc::MAP_SHARED,
                fd,
                0,
            );
            assert!(p != libc::MAP_FAILED);
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), p as *mut u8, bytes.len());
            libc::munmap(p, bytes.len());
            libc::close(fd);
        }
    }

    #[cfg(unix)]
    fn shm_object_exists(name: &str) -> bool {
        use std::ffi::CString;
        let c_name = CString::new(name).unwrap();
        unsafe {
            let fd = libc::shm_open(c_name.as_ptr(), libc::O_RDONLY, 0);
            if fd >= 0 {
                libc::close(fd);
                true
            } else {
                false
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn kitty_apc_t_s_reads_and_unlinks_shared_memory() {
        use base64::Engine;
        let png = kitty_png(4, 4);
        // POSIX SHM names start with `/`. Use the test PID for
        // uniqueness across parallel test runs.
        let name = format!("/yutani-shm-test-{}", std::process::id());
        // Pre-clean in case a previous failed run left it behind.
        unlink_kitty_shm(&name);
        write_shm(&name, &png);
        assert!(shm_object_exists(&name), "fixture should exist pre-test");
        let name_b64 = base64::engine::general_purpose::STANDARD.encode(&name);

        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        t.feed(&format!("\x1b_Ga=T,f=100,t=s,c=2,r=1;{}\x1b\\", name_b64));

        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1, "SHM upload should land");
        // On macOS the SHM segment rounds up to a page; the upload
        // bytes include zero padding past the PNG body. Compare the
        // prefix and let `peek_dimensions` confirm the PNG decoded.
        assert_eq!(&uploads[0].bytes[..png.len()], png.as_slice());
        assert_eq!(uploads[0].pixel_size, Some((4, 4)));
        assert!(!shm_object_exists(&name), "t=s must shm_unlink after read");
    }

    #[cfg(unix)]
    #[test]
    fn kitty_apc_t_s_unlinks_even_on_read_failure() {
        use base64::Engine;
        // Reference a nonexistent SHM name. The read fails (shm_open
        // returns ENOENT) but we still call shm_unlink — testing
        // that this doesn't panic or leave anything weird behind.
        let name = format!("/yutani-shm-missing-{}", std::process::id());
        unlink_kitty_shm(&name); // belt-and-braces
        let name_b64 = base64::engine::general_purpose::STANDARD.encode(&name);
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        t.feed(&format!("\x1b_Ga=T,f=100,t=s,c=1,r=1;{}\x1b\\", name_b64));
        assert!(t.take_pending_image_uploads().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn kitty_apc_t_s_with_raw_rgb_pngencodes_on_read() {
        // icat's preferred path for huge JPGs: raw RGB via SHM. The
        // SHM segment contains raw bytes; we read, PNG-encode, queue.
        use base64::Engine;
        let w = 4u32;
        let h = 4u32;
        let raw: Vec<u8> = (0..(w * h * 3) as u8).collect();
        let name = format!("/yutani-shm-raw-{}", std::process::id());
        unlink_kitty_shm(&name);
        write_shm(&name, &raw);
        let name_b64 = base64::engine::general_purpose::STANDARD.encode(&name);
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        t.feed(&format!(
            "\x1b_Ga=T,f=24,t=s,s={},v={},c=2,r=1;{}\x1b\\",
            w, h, name_b64
        ));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        assert_eq!(uploads[0].pixel_size, Some((w, h)));
        // Raw RGB now takes the worker-bypass path: bytes stay as
        // raw RGBA (alpha-padded), `raw_rgba_dims` carries the dims
        // for the store-side upload.
        assert_eq!(uploads[0].raw_rgba_dims, Some((w, h)));
        assert_eq!(uploads[0].bytes.len() as u32, w * h * 4);
        assert!(!shm_object_exists(&name));
    }

    //
    // K2: temp-file transmission (`t=t`). Same shape as `t=f` but
    // deletes the file after read, gated on the path being under
    // `std::env::temp_dir()`. This is what icat prefers for large
    // images — one APC + one file read instead of ~250 chunked APCs.
    //

    #[test]
    fn kitty_apc_t_t_reads_and_deletes_temp_file() {
        use base64::Engine;
        let png = kitty_png(4, 4);
        let dir = std::env::temp_dir();
        let path = dir.join(format!("yutani-kitty-t-test-{}.png", std::process::id()));
        std::fs::write(&path, &png).expect("write temp png");
        let path_b64 = base64::engine::general_purpose::STANDARD.encode(path.to_str().unwrap());
        // Sanity: file exists before the APC.
        assert!(path.exists(), "fixture should exist pre-test");

        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        t.feed(&format!("\x1b_Ga=T,f=100,t=t,c=2,r=1;{}\x1b\\", path_b64));

        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1, "temp-file upload should land");
        assert_eq!(uploads[0].bytes, png);
        // Per spec the terminal owns the unlink — file should be gone.
        assert!(!path.exists(), "t=t must delete the file after read");
    }

    #[test]
    fn kitty_apc_t_t_does_not_delete_files_outside_temp_dir() {
        // Defense-in-depth: even when the app asks for `t=t`, we only
        // delete files that actually live under temp_dir. A path
        // pointing outside is read (no privilege escalation — the app
        // could read it itself) but left in place.
        use base64::Engine;
        let png = kitty_png(2, 2);
        // Use the cargo target dir (definitely not under temp_dir) so
        // the test doesn't depend on writeable /tmp behaviour.
        let dir = std::env::current_dir().unwrap().join("target");
        std::fs::create_dir_all(&dir).ok();
        let path = dir.join(format!("yutani-outside-temp-{}.png", std::process::id()));
        std::fs::write(&path, &png).expect("write fixture");
        let path_b64 = base64::engine::general_purpose::STANDARD.encode(path.to_str().unwrap());

        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        t.feed(&format!("\x1b_Ga=T,f=100,t=t,c=1,r=1;{}\x1b\\", path_b64));

        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1, "non-temp file should still be read");
        assert!(path.exists(), "non-temp file must NOT be deleted");

        // Clean up after ourselves since the terminal won't.
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn kitty_apc_t_t_with_raw_rgb_reads_and_pngencodes() {
        // icat's preferred path for big JPGs: f=24 (raw RGB) over t=t
        // (temp file). The temp file contains raw `s*v*3` bytes; we
        // PNG-encode on read so the downstream decoder sees a PNG.
        use base64::Engine;
        let w = 4u32;
        let h = 4u32;
        let raw: Vec<u8> = (0..(w * h * 3) as u8).collect();
        let path = std::env::temp_dir().join(format!("yutani-raw-t-test-{}.rgb", std::process::id()));
        std::fs::write(&path, &raw).expect("write fixture");
        let path_b64 = base64::engine::general_purpose::STANDARD.encode(path.to_str().unwrap());

        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        t.feed(&format!(
            "\x1b_Ga=T,f=24,t=t,s={},v={},c=2,r=1;{}\x1b\\",
            w, h, path_b64
        ));

        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        assert_eq!(uploads[0].pixel_size, Some((w, h)));
        // Raw RGB takes the worker-bypass path; bytes are RGBA
        // (alpha-padded), not PNG re-encoded.
        assert_eq!(uploads[0].raw_rgba_dims, Some((w, h)));
        assert_eq!(uploads[0].bytes.len() as u32, w * h * 4);
        assert!(!path.exists(), "temp file should be deleted");
    }

    #[test]
    fn kitty_apc_query_replies_ok_for_t_t_temp_file() {
        // Capability handshake: must advertise `t=t` so icat will use
        // it instead of falling back to ~250 chunked direct APCs.
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&kitty_apc_control_only("a=q,i=8,f=100,t=t,s=1,v=1"));
        assert_eq!(t.take_response(), b"\x1b_Gi=8;OK\x1b\\");
        // And for raw RGB over temp file (the big-JPG fast path).
        t.feed(&kitty_apc_control_only("a=q,i=9,f=24,t=t,s=1,v=1"));
        assert_eq!(t.take_response(), b"\x1b_Gi=9;OK\x1b\\");
    }

    #[test]
    fn kitty_apc_anonymous_chunked_transmission_assembles() {
        // kitty +kitten icat omits `i=` on its chunked transmissions —
        // the spec says chunked MUST have an id, but reality differs.
        // Use the `kitty_chunks_anon` slot to thread chunks together.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        let png = kitty_png(4, 4);
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        let mid = b64.len() / 2;
        let (c1, c2) = b64.split_at(mid);

        // First chunk: m=1, no `i=`. Carries the sizing.
        t.feed(&format!("\x1b_Ga=T,q=2,f=100,m=1,c=2,r=1;{}\x1b\\", c1));
        assert!(t.take_pending_image_uploads().is_empty(),
            "first anon chunk must not flush");
        // Terminal chunk: m=0 (or omitted), no `i=`.
        t.feed(&format!("\x1b_Ga=T,m=0;{}\x1b\\", c2));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1, "anonymous chunks should coalesce");
        assert_eq!(uploads[0].cell_extent, (1, 2));
        assert_eq!(uploads[0].pixel_size, Some((4, 4)));
    }

    #[test]
    fn kitty_apc_anonymous_chunked_does_not_collide_with_id_keyed() {
        // Anonymous and id-keyed chunked transmissions use separate
        // slots so they can be in-flight at the same time. Pin that
        // by interleaving and verifying both land independently.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        let png_anon = kitty_png(2, 2);
        let png_keyed = kitty_png(3, 3);
        use base64::Engine;
        let b_anon = base64::engine::general_purpose::STANDARD.encode(&png_anon);
        let b_keyed = base64::engine::general_purpose::STANDARD.encode(&png_keyed);

        let (a1, a2) = b_anon.split_at(b_anon.len() / 2);
        let (k1, k2) = b_keyed.split_at(b_keyed.len() / 2);

        t.feed(&format!("\x1b_Ga=T,f=100,m=1,c=1,r=1;{}\x1b\\", a1));
        t.feed(&format!("\x1b_Ga=T,f=100,m=1,i=99,c=2,r=2;{}\x1b\\", k1));
        t.feed(&format!("\x1b_Ga=T,i=99,m=0;{}\x1b\\", k2));
        t.feed(&format!("\x1b_Ga=T,m=0;{}\x1b\\", a2));

        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 2);
        // Both should have valid pixel data — proves the buffers didn't
        // cross-contaminate.
        for up in &uploads {
            assert!(up.pixel_size.is_some(), "decoded cleanly: {:?}", up.cell_extent);
        }
    }

    #[test]
    fn kitty_apc_raw_rgb_direct_takes_worker_bypass() {
        // What `kitty +kitten icat` does for a JPG: decode locally to
        // raw RGB, ship over t=d, count on the terminal to handle
        // f=24. Used to PNG-encode on receive; now takes the
        // worker-bypass path — bytes stay as raw RGBA (alpha-padded
        // from RGB) and `raw_rgba_dims` carries the dims for the
        // store-side upload.
        use base64::Engine;
        let w = 4u32;
        let h = 4u32;
        let rgb: Vec<u8> = (0..(w * h * 3) as u8).collect();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&rgb);

        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        t.feed(&format!(
            "\x1b_Ga=T,f=24,s={},v={},c=2,r=1;{}\x1b\\",
            w, h, b64
        ));

        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        assert_eq!(uploads[0].pixel_size, Some((w, h)));
        assert_eq!(uploads[0].raw_rgba_dims, Some((w, h)));
        assert_eq!(uploads[0].bytes.len() as u32, w * h * 4);
        // Each pixel's alpha byte was padded to 0xFF.
        for px in uploads[0].bytes.chunks_exact(4) {
            assert_eq!(px[3], 0xFF);
        }
    }

    #[test]
    fn kitty_apc_raw_rgba_direct_takes_worker_bypass() {
        use base64::Engine;
        let w = 2u32;
        let h = 2u32;
        let rgba: Vec<u8> = (0..(w * h * 4) as u8).collect();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&rgba);

        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        t.feed(&format!(
            "\x1b_Ga=T,f=32,s={},v={};{}\x1b\\",
            w, h, b64
        ));

        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        assert_eq!(uploads[0].pixel_size, Some((w, h)));
        assert_eq!(uploads[0].raw_rgba_dims, Some((w, h)));
        // Bytes passed through unchanged.
        assert_eq!(uploads[0].bytes, rgba);
    }

    #[test]
    fn kitty_apc_raw_format_with_mismatched_byte_count_drops() {
        // s*v*3 mismatch — declared 4x4 RGB (48 bytes) but payload is
        // only 12. Reject so a buggy app can't crash the decoder.
        use base64::Engine;
        let too_few = base64::engine::general_purpose::STANDARD.encode(vec![0u8; 12]);
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        t.feed(&format!("\x1b_Ga=T,f=24,s=4,v=4;{}\x1b\\", too_few));
        assert!(t.take_pending_image_uploads().is_empty());
    }

    #[test]
    fn kitty_apc_raw_format_without_source_dims_drops() {
        // s= / v= are required for raw — there's no header to fall back on.
        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD.encode(vec![0u8; 48]);
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        t.feed(&format!("\x1b_Ga=T,f=24;{}\x1b\\", bytes));
        assert!(t.take_pending_image_uploads().is_empty());
    }

    #[test]
    fn parse_kitty_control_t_f_is_file() {
        assert_eq!(
            parse_kitty_control("t=f").unwrap().transmission,
            KittyTransmission::File
        );
    }

    #[test]
    fn kitty_apc_transmit_only_queues_upload_with_display_false() {
        // `a=t` (lowercase) — transmit-only, hold for a later `a=p`.
        // The upload IS queued (so the decode runs and the store gets
        // the pixels), but with display_immediately=false so main.rs
        // skips placement creation. Cursor stays put.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        t.feed("\x1b[5;1H"); // cursor at row 5 col 1
        let cursor_before = t.cursor().row;
        let png = kitty_png(4, 4);
        t.feed(&kitty_apc("a=t,f=100,c=2,r=1,i=1", &png));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        assert!(!uploads[0].display_immediately);
        assert_eq!(uploads[0].kitty_image_id, Some(1));
        // `a=t` MUST NOT advance the cursor — the placement happens
        // later via `a=p` and that's what moves the cursor.
        assert_eq!(t.cursor().row, cursor_before);
    }

    //
    // K5: virtual placements (U=1 + U+10EEEE placeholder cells)
    //

    /// Build the SGR truecolor escape for an image-id placeholder. The
    /// 24-bit id maps to RGB as (high, mid, low) bytes — matches what
    /// `decode_kitty_placeholder_image_id` reverses.
    fn placeholder_sgr_fg(image_id: u32) -> String {
        let r = ((image_id >> 16) & 0xFF) as u8;
        let g = ((image_id >> 8) & 0xFF) as u8;
        let b = (image_id & 0xFF) as u8;
        format!("\x1b[38;2;{};{};{}m", r, g, b)
    }

    #[test]
    fn parse_kitty_control_u_one_sets_virtual_placement() {
        let c = parse_kitty_control("U=1,i=1,a=T").unwrap();
        assert!(c.virtual_placement);
        let c = parse_kitty_control("a=T,i=1").unwrap();
        assert!(!c.virtual_placement, "default is false");
    }

    #[test]
    fn kitty_apc_virtual_placement_transmits_without_displaying() {
        // a=T,U=1,i=N: image goes into the store + kitty_image_ids
        // map, but no Placement is created. The cursor doesn't move
        // either — placeholders position it later.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        t.feed("\x1b[5;1H");
        let cursor_before = t.cursor().row;
        let png = kitty_png(4, 4);
        t.feed(&kitty_apc("a=T,U=1,f=100,i=42", &png));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        assert!(!uploads[0].display_immediately, "U=1 must not display");
        assert_eq!(uploads[0].kitty_image_id, Some(42));
        // No placement; cursor put.
        assert!(t.live_placements().is_empty());
        assert_eq!(t.cursor().row, cursor_before);
    }

    #[test]
    fn placeholder_cells_record_image_id_from_fg_color() {
        // SGR truecolor encodes a 24-bit id; print one placeholder
        // and read the cell back.
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&placeholder_sgr_fg(0xABCDEF));
        t.feed("\u{10EEEE}");
        let cell = t.extended_cell(0, 0).unwrap();
        assert_eq!(cell.placeholder_image_id, Some(0xABCDEF));
    }

    #[test]
    fn placeholder_cells_without_fg_color_have_no_image_id() {
        // Without an SGR fg, the cell's fg defaults to None — there's
        // no id to extract. Pin the contract so an accidental
        // placeholder doesn't act on whatever color was last printed.
        let mut t = Terminal::new(80, 24, 100);
        t.feed("\u{10EEEE}");
        let cell = t.extended_cell(0, 0).unwrap();
        assert_eq!(cell.placeholder_image_id, None);
    }

    #[test]
    fn placeholder_cells_with_zero_id_treated_as_no_id() {
        // (0, 0, 0) is the sentinel "no id" color. Print one such
        // placeholder and verify it's not picked up.
        let mut t = Terminal::new(80, 24, 100);
        t.feed("\x1b[38;2;0;0;0m");
        t.feed("\u{10EEEE}");
        let cell = t.extended_cell(0, 0).unwrap();
        assert_eq!(cell.placeholder_image_id, None);
    }

    #[test]
    fn placeholder_runs_single_full_row_collapses_to_one_run() {
        // Five placeholder cells in one screen row, each with the
        // same image_row (0) and consecutive image_col (0..5) — the
        // shape a normal `kitten icat` tiling produces. One run.
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&placeholder_sgr_fg(7));
        t.feed("\x1b[3;6H"); // row 2 (0-based), col 5 (0-based)
        for col in 0..5u32 {
            let col_dia = KITTY_PLACEHOLDER_DIACRITICS[col as usize];
            t.feed("\u{10EEEE}\u{0305}"); // row diacritic = index 0
            let mut s = String::new();
            s.push(col_dia);
            t.feed(&s);
        }
        let runs = t.kitty_placeholder_runs();
        assert_eq!(runs.len(), 1);
        let r = &runs[0];
        assert_eq!(r.client_id, 7);
        assert_eq!(r.screen_row, 2);
        assert_eq!((r.screen_col_start, r.screen_col_end), (5, 10));
        assert_eq!(r.image_row, 0);
        assert_eq!((r.image_col_start, r.image_col_end), (0, 5));
    }

    #[test]
    fn placeholder_runs_multi_row_block_emits_one_run_per_screen_row() {
        // 3×2 grid of placeholders. Each screen row carries a
        // different image_row diacritic. Output: 2 runs (one per
        // screen row), each 3 cells wide.
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&placeholder_sgr_fg(7));
        let row0 = KITTY_PLACEHOLDER_DIACRITICS[0]; // image_row = 0
        let row1 = KITTY_PLACEHOLDER_DIACRITICS[1]; // image_row = 1
        // Screen row 2.
        t.feed("\x1b[3;6H");
        for col in 0..3u32 {
            let col_dia = KITTY_PLACEHOLDER_DIACRITICS[col as usize];
            t.feed("\u{10EEEE}");
            let mut s = String::new();
            s.push(row0);
            s.push(col_dia);
            t.feed(&s);
        }
        // Screen row 3.
        t.feed("\x1b[4;6H");
        for col in 0..3u32 {
            let col_dia = KITTY_PLACEHOLDER_DIACRITICS[col as usize];
            t.feed("\u{10EEEE}");
            let mut s = String::new();
            s.push(row1);
            s.push(col_dia);
            t.feed(&s);
        }
        let runs = t.kitty_placeholder_runs();
        assert_eq!(runs.len(), 2, "one run per screen row");
        assert_eq!(runs[0].screen_row, 2);
        assert_eq!(runs[0].image_row, 0);
        assert_eq!(runs[1].screen_row, 3);
        assert_eq!(runs[1].image_row, 1);
    }

    #[test]
    fn placeholder_runs_two_distinct_image_ids_produce_two_runs() {
        // Adjacent cells encoding different ids must not merge.
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&placeholder_sgr_fg(7));
        t.feed("\x1b[1;1H");
        t.feed("\u{10EEEE}\u{0305}\u{0305}"); // id=7, row=0, col=0
        t.feed("\u{10EEEE}\u{0305}\u{030D}"); // id=7, row=0, col=1
        t.feed(&placeholder_sgr_fg(9));
        t.feed("\u{10EEEE}\u{0305}\u{0305}"); // id=9, row=0, col=0
        let runs = t.kitty_placeholder_runs();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].client_id, 7);
        assert_eq!((runs[0].screen_col_start, runs[0].screen_col_end), (0, 2));
        assert_eq!(runs[1].client_id, 9);
        assert_eq!((runs[1].screen_col_start, runs[1].screen_col_end), (2, 3));
    }

    #[test]
    fn placeholder_runs_break_on_image_col_gap() {
        // Cells at image_col 0, 1, then 3 (skipping 2) must split
        // into two runs. Otherwise the renderer would stretch
        // image_col 0..2 over a 3-cell span and skip image_col 2.
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&placeholder_sgr_fg(7));
        t.feed("\x1b[1;1H");
        let col0 = KITTY_PLACEHOLDER_DIACRITICS[0];
        let col1 = KITTY_PLACEHOLDER_DIACRITICS[1];
        let col3 = KITTY_PLACEHOLDER_DIACRITICS[3];
        for col in [col0, col1, col3] {
            t.feed("\u{10EEEE}");
            let mut s = String::new();
            s.push(KITTY_PLACEHOLDER_DIACRITICS[0]); // image_row = 0
            s.push(col);
            t.feed(&s);
        }
        let runs = t.kitty_placeholder_runs();
        assert_eq!(runs.len(), 2);
        assert_eq!((runs[0].image_col_start, runs[0].image_col_end), (0, 2));
        assert_eq!((runs[1].image_col_start, runs[1].image_col_end), (3, 4));
    }

    #[test]
    fn placeholder_runs_break_on_image_row_change_within_screen_row() {
        // Adjacent cells with different image_row diacritics start
        // separate runs. (Pathological — an encoder doesn't normally
        // do this — but it pins the contract.)
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&placeholder_sgr_fg(7));
        t.feed("\x1b[1;1H");
        t.feed("\u{10EEEE}\u{0305}\u{0305}"); // row 0
        t.feed("\u{10EEEE}\u{030D}\u{030D}"); // row 1
        let runs = t.kitty_placeholder_runs();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].image_row, 0);
        assert_eq!(runs[1].image_row, 1);
    }

    #[test]
    fn placeholder_runs_break_on_non_placeholder_cell() {
        // A plain character cell between placeholders splits the
        // run. The renderer should draw the left and right halves
        // as separate quads (each with the correct UV slice) so
        // the text shows through the gap.
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&placeholder_sgr_fg(7));
        t.feed("\x1b[1;1H");
        t.feed("\u{10EEEE}\u{0305}\u{0305}"); // col 0
        t.feed("\x1b[39mX"); // plain glyph
        t.feed(&placeholder_sgr_fg(7));
        t.feed("\u{10EEEE}\u{0305}\u{030E}"); // col 2
        let runs = t.kitty_placeholder_runs();
        assert_eq!(runs.len(), 2);
        assert_eq!((runs[0].screen_col_start, runs[0].screen_col_end), (0, 1));
        assert_eq!((runs[1].screen_col_start, runs[1].screen_col_end), (2, 3));
    }

    #[test]
    fn placeholder_runs_empty_when_no_placeholders() {
        let mut t = Terminal::new(80, 24, 100);
        t.feed("hello world");
        assert!(t.kitty_placeholder_runs().is_empty());
    }

    #[test]
    fn placeholder_runs_partial_overwrite_shows_remainder_at_original_scale() {
        // The regression that motivates this whole shape: a 3-cell
        // run gets its first cell overwritten by ordinary text.
        // The remaining 2 cells still encode `image_col` 1..3, so
        // the renderer draws the right two-thirds of the image
        // (against the original `c=3` denominator) — NOT the whole
        // image stretched into a 2-cell rect. This test only proves
        // the data is preserved; the UV math lives in main.rs.
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&placeholder_sgr_fg(7));
        t.feed("\x1b[1;1H");
        for col in 0..3u32 {
            let col_dia = KITTY_PLACEHOLDER_DIACRITICS[col as usize];
            t.feed("\u{10EEEE}\u{0305}"); // image_row 0
            let mut s = String::new();
            s.push(col_dia);
            t.feed(&s);
        }
        // Overwrite the first cell with a regular character.
        t.feed("\x1b[1;1H");
        t.feed("\x1b[39mX");
        let runs = t.kitty_placeholder_runs();
        assert_eq!(runs.len(), 1, "surviving cells form one contiguous run");
        let r = &runs[0];
        assert_eq!((r.screen_col_start, r.screen_col_end), (1, 3));
        assert_eq!(
            (r.image_col_start, r.image_col_end),
            (1, 3),
            "image-col data preserved so the UV samples the right portion",
        );
    }

    #[test]
    fn kitty_image_cell_extent_recorded_on_a_T_U1_finalize() {
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        let png = kitty_png(4, 4);
        t.feed(&kitty_apc("a=T,U=1,f=100,c=29,r=15,i=43", &png));
        // Drain the upload (irrelevant); the side-effect we care
        // about is the cached extent.
        let _ = t.take_pending_image_uploads();
        assert_eq!(t.kitty_image_cell_extent(43), Some((29, 15)));
    }

    #[test]
    fn kitty_image_cell_extent_missing_when_c_or_r_omitted() {
        // Without both `c=` and `r=` we can't define a tiling and
        // the renderer would have no UV denominator — record None.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        let png = kitty_png(4, 4);
        t.feed(&kitty_apc("a=T,U=1,f=100,c=29,i=44", &png));
        let _ = t.take_pending_image_uploads();
        assert_eq!(t.kitty_image_cell_extent(44), None);
    }

    #[test]
    fn kitty_image_cell_extent_cleared_on_a_d_i() {
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        let png = kitty_png(4, 4);
        t.feed(&kitty_apc("a=T,U=1,f=100,c=29,r=15,i=43", &png));
        let _ = t.take_pending_image_uploads();
        t.register_kitty_image_id(43, ImageId(1));
        assert_eq!(t.kitty_image_cell_extent(43), Some((29, 15)));
        t.feed(&kitty_apc_control_only("a=d,d=i,i=43"));
        assert_eq!(t.kitty_image_cell_extent(43), None);
    }

    #[test]
    fn kitty_image_cell_extent_cleared_on_a_d_a() {
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        let png = kitty_png(4, 4);
        t.feed(&kitty_apc("a=T,U=1,f=100,c=29,r=15,i=43", &png));
        let _ = t.take_pending_image_uploads();
        t.feed(&kitty_apc_control_only("a=d,d=a"));
        assert_eq!(t.kitty_image_cell_extent(43), None);
    }

    #[test]
    fn placeholder_diacritics_attach_to_previous_cell_not_their_own() {
        // Regression for the tmux unicode-placeholder bug: each cell
        // in the placeholder grid is U+10EEEE followed by combining
        // diacritics encoding (row, col). If the diacritics land in
        // their own cells they show up as glyph-less "tofu" because
        // most of those codepoints have no rasterized glyph.
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&placeholder_sgr_fg(0x111111));
        // U+10EEEE + 1st diacritic (row 0) + 2nd diacritic (col 0).
        t.feed("\u{10EEEE}\u{0305}\u{0305}");
        let cell0 = t.extended_cell(0, 0).unwrap();
        assert_eq!(cell0.placeholder_image_id, Some(0x111111));
        assert_eq!(cell0.placeholder_image_row, 0);
        assert_eq!(cell0.placeholder_image_col, 0);
        // Diacritics MUST NOT have landed in cells 1 and 2.
        let cell1 = t.extended_cell(0, 1).unwrap();
        let cell2 = t.extended_cell(0, 2).unwrap();
        assert_eq!(cell1.ch, ' ', "diacritic 1 must not occupy its own cell");
        assert_eq!(cell2.ch, ' ', "diacritic 2 must not occupy its own cell");
    }

    #[test]
    fn placeholder_diacritics_decode_row_and_column() {
        // Second diacritic in the kitty table → row 1; fourth → col 3.
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&placeholder_sgr_fg(0xCAFE00));
        t.feed("\u{10EEEE}\u{030D}\u{0310}"); // row=1, col=3
        let cell = t.extended_cell(0, 0).unwrap();
        assert_eq!(cell.placeholder_image_row, 1);
        assert_eq!(cell.placeholder_image_col, 3);
    }

    #[test]
    fn placeholder_third_diacritic_extends_image_id_high_byte() {
        // Third diacritic encodes the high byte (bits 24..31) of the
        // image id. The low 24 bits come from the FG truecolor; the
        // high byte adds the 25..32 bits without disturbing them.
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&placeholder_sgr_fg(0x00ABCDEF)); // low 24 bits
        // 1st='\u{0305}'→row 0, 2nd='\u{0305}'→col 0, 3rd='\u{030D}'→high=1
        t.feed("\u{10EEEE}\u{0305}\u{0305}\u{030D}");
        let cell = t.extended_cell(0, 0).unwrap();
        assert_eq!(cell.placeholder_image_id, Some(0x01ABCDEF));
    }

    #[test]
    fn placeholder_diacritic_absorption_clears_on_non_diacritic_char() {
        // After printing a real glyph, the absorption state must
        // reset — subsequent diacritics belong to that glyph (or
        // nothing), NOT the prior placeholder.
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&placeholder_sgr_fg(0x222222));
        t.feed("\u{10EEEE}");
        t.feed("a"); // breaks the absorption window
        t.feed("\u{0305}"); // should now land in its own cell
        // Placeholder at col 0, 'a' at col 1, diacritic at col 2.
        let cell0 = t.extended_cell(0, 0).unwrap();
        let cell1 = t.extended_cell(0, 1).unwrap();
        let cell2 = t.extended_cell(0, 2).unwrap();
        assert_eq!(cell0.placeholder_image_id, Some(0x222222));
        assert_eq!(cell1.ch, 'a');
        assert_eq!(cell2.ch, '\u{0305}');
    }

    #[test]
    fn placeholder_diacritics_after_three_stop_being_absorbed() {
        // The protocol allows at most 3 diacritics after each
        // U+10EEEE. A fourth diacritic must be treated like any
        // other character (lands in its own cell here).
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&placeholder_sgr_fg(0x333333));
        t.feed("\u{10EEEE}\u{0305}\u{0305}\u{0305}\u{0305}");
        let cell0 = t.extended_cell(0, 0).unwrap();
        let cell1 = t.extended_cell(0, 1).unwrap();
        assert_eq!(cell0.placeholder_image_id, Some(0x00333333));
        assert_eq!(cell1.ch, '\u{0305}', "4th diacritic falls through");
    }

    #[test]
    fn placeholder_full_row_with_diacritics_keeps_cursor_aligned() {
        // Pin the regression that motivated all of this: a row of
        // placeholders (each U+10EEEE + 2 diacritics) leaves the
        // cursor exactly where it would be without diacritics. If
        // absorption were broken the cursor would land further right
        // and subsequent text would wrap.
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&placeholder_sgr_fg(0x444444));
        // 5 placeholder cells, each "U+10EEEE + row + col" diacritics.
        for col in 0..5u32 {
            let col_dia = KITTY_PLACEHOLDER_DIACRITICS[col as usize];
            t.feed("\u{10EEEE}\u{0305}");
            let mut s = String::new();
            s.push(col_dia);
            t.feed(&s);
        }
        // Cursor must be at col 5 (one per placeholder), not 15
        // (one per placeholder + diacritic + diacritic).
        assert_eq!(t.cursor().col, 5);
    }

    #[test]
    fn kitty_placeholder_diacritic_index_lookup_round_trips() {
        // Sanity: first → 0, second → 1, last → 296. Anything not in
        // the table returns None.
        assert_eq!(kitty_placeholder_diacritic_index('\u{0305}'), Some(0));
        assert_eq!(kitty_placeholder_diacritic_index('\u{030D}'), Some(1));
        assert_eq!(kitty_placeholder_diacritic_index('\u{1D244}'), Some(296));
        assert_eq!(kitty_placeholder_diacritic_index('a'), None);
        assert_eq!(kitty_placeholder_diacritic_index('\u{10EEEE}'), None);
    }

    #[test]
    fn kitty_placeholder_diacritic_table_is_sorted_for_binary_search() {
        // The lookup is a binary search and silently returns wrong
        // indices if the table ever ends up unsorted. Cheap structural
        // invariant — pin it so a future edit can't corrupt the lookup
        // without tripping a test.
        for pair in KITTY_PLACEHOLDER_DIACRITICS.windows(2) {
            assert!(pair[0] < pair[1], "table must be strictly ascending");
        }
        assert_eq!(KITTY_PLACEHOLDER_DIACRITICS.len(), 297);
    }

    #[test]
    fn kitty_placeholder_diacritic_index_returns_none_for_char_between_table_entries() {
        // U+0306 sits between table[0]=U+0305 and table[1]=U+030D.
        // Binary search must report "not found" rather than the bracket
        // index — a regression in the comparator would leak a Some.
        assert_eq!(kitty_placeholder_diacritic_index('\u{0306}'), None);
    }

    #[test]
    fn diacritic_with_no_prior_placeholder_lands_in_own_cell() {
        // First-char-in-the-feed diacritic: placeholder_decode is None,
        // so absorption must not engage. The diacritic prints as a
        // normal (glyph-less) cell at col 0 and the cursor advances.
        // Regression risk: an unconditional "if diacritic, absorb"
        // would silently eat the first diacritic the user types.
        let mut t = Terminal::new(80, 24, 100);
        t.feed("\u{0305}");
        let cell = t.extended_cell(0, 0).unwrap();
        assert_eq!(cell.ch, '\u{0305}');
        assert_eq!(cell.placeholder_image_id, None);
        assert_eq!(t.cursor().col, 1);
    }

    #[test]
    fn placeholder_without_fg_color_does_not_absorb_following_diacritics() {
        // U+10EEEE with no SGR fg → placeholder_image_id stays None,
        // so the print() path leaves placeholder_decode = None.
        // Subsequent diacritics MUST fall through to normal print
        // (each in its own cell), since the protocol's row/col/id-high
        // encoding has nothing to attach to.
        let mut t = Terminal::new(80, 24, 100);
        t.feed("\u{10EEEE}\u{0305}\u{030D}");
        let cell0 = t.extended_cell(0, 0).unwrap();
        let cell1 = t.extended_cell(0, 1).unwrap();
        let cell2 = t.extended_cell(0, 2).unwrap();
        assert_eq!(cell0.placeholder_image_id, None);
        assert_eq!(cell0.placeholder_image_row, 0);
        assert_eq!(cell0.placeholder_image_col, 0);
        assert_eq!(cell1.ch, '\u{0305}', "diacritic 1 falls through");
        assert_eq!(cell2.ch, '\u{030D}', "diacritic 2 falls through");
        assert_eq!(t.cursor().col, 3);
    }

    #[test]
    fn placeholder_zero_id_fg_does_not_absorb_following_diacritics() {
        // (0,0,0) fg encodes id 0, which decode treats as "no id".
        // Same contract as the no-fg case: absorption must not engage.
        let mut t = Terminal::new(80, 24, 100);
        t.feed("\x1b[38;2;0;0;0m\u{10EEEE}\u{0305}");
        let cell0 = t.extended_cell(0, 0).unwrap();
        let cell1 = t.extended_cell(0, 1).unwrap();
        assert_eq!(cell0.placeholder_image_id, None);
        assert_eq!(cell1.ch, '\u{0305}');
    }

    #[test]
    fn placeholder_decode_state_survives_cup_between_placeholder_and_diacritic() {
        // CUP doesn't go through print(), so placeholder_decode is NOT
        // cleared by cursor movement. A diacritic after a CUP still
        // attaches to the cell the most recent U+10EEEE landed in —
        // NOT at the CUP destination. Pin this so a future "clear on
        // any cursor move" change is at least intentional and reviewed.
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&placeholder_sgr_fg(0x555555));
        t.feed("\u{10EEEE}");
        // Jump elsewhere on screen, then feed one diacritic.
        t.feed("\x1b[10;20H\u{030D}"); // table[1] → row=1
        let original = t.extended_cell(0, 0).unwrap();
        let cup_dest = t.extended_cell(9, 19).unwrap();
        assert_eq!(original.placeholder_image_row, 1, "diacritic landed on original cell");
        assert_eq!(cup_dest.ch, ' ', "diacritic did NOT land at CUP destination");
        assert_ne!(cup_dest.placeholder_image_row, 1);
    }

    #[test]
    fn resolve_kitty_format_mid_gap_with_none_fallback_defaults_to_rgb() {
        // Base-image (a=T) callers pass fallback=None. If the byte
        // count lands in the mid-gap (past rgb+PAGE, under rgba), the
        // resolver still must not hand the bytes to the PNG decoder —
        // it defaults to RGB so the raw-bypass path can run.
        let (w, h) = (450u32, 450u32);
        let rgb = (w as usize) * (h as usize) * 3;
        let mid = rgb + 16 * 1024 + 10_000;
        let payload = vec![0u8; mid];
        let resolved =
            resolve_kitty_format(KittyFormat::Png, &payload, Some(w), Some(h), None);
        assert!(matches!(resolved, KittyFormat::Rgb));
    }

    #[test]
    fn resolve_kitty_format_mid_gap_with_png_fallback_defaults_to_rgb() {
        // fallback=Some(Png) is degenerate — it means "the base was
        // also a PNG", which shouldn't happen for a mid-gap raw
        // payload. The resolver explicitly only honors RGB/RGBA
        // fallbacks; for Png it falls through to the RGB default
        // rather than ping-ponging the bytes through the PNG decoder.
        let (w, h) = (450u32, 450u32);
        let rgb = (w as usize) * (h as usize) * 3;
        let mid = rgb + 16 * 1024 + 10_000;
        let payload = vec![0u8; mid];
        let resolved = resolve_kitty_format(
            KittyFormat::Png,
            &payload,
            Some(w),
            Some(h),
            Some(KittyFormat::Png),
        );
        assert!(matches!(resolved, KittyFormat::Rgb));
    }

    #[test]
    fn resolve_kitty_format_exact_rgb_byte_count_beats_rgba_fallback() {
        // Pin the contract the existing
        // `a_f_size_inference_picks_rgb_when_byte_count_matches_w_h_3`
        // integration test depends on: when the payload is exactly
        // w*h*3 bytes, the resolver returns Rgb regardless of what
        // the base format said. Byte-count exact-match must beat
        // base-format inheritance — otherwise frames that change
        // bit-depth would be mis-decoded.
        let (w, h) = (2u32, 2u32);
        let rgb_payload = vec![0u8; (w as usize) * (h as usize) * 3];
        let resolved = resolve_kitty_format(
            KittyFormat::Png,
            &rgb_payload,
            Some(w),
            Some(h),
            Some(KittyFormat::Rgba),
        );
        assert!(matches!(resolved, KittyFormat::Rgb));
    }

    #[test]
    fn resolve_kitty_format_real_png_passthrough_ignores_fallback() {
        // The PNG signature short-circuit fires before any byte-count
        // or fallback logic. Without this, a payload that happens to
        // start with the PNG magic but whose length lands in the gap
        // would be mis-routed.
        let png = kitty_png(4, 4);
        let resolved = resolve_kitty_format(
            KittyFormat::Png,
            &png,
            Some(4),
            Some(4),
            Some(KittyFormat::Rgb),
        );
        assert!(matches!(resolved, KittyFormat::Png));
    }

    #[test]
    fn resolve_kitty_format_non_png_parsed_format_returns_as_is() {
        // When the parser already saw an explicit f=24 or f=32, the
        // resolver must not second-guess it — even if dimensions and
        // bytes would otherwise infer differently. Pin so the
        // resolver stays a narrow "fill in for PNG default" helper.
        let payload = vec![0u8; 999];
        let resolved = resolve_kitty_format(
            KittyFormat::Rgb,
            &payload,
            Some(2),
            Some(2),
            Some(KittyFormat::Rgba),
        );
        assert!(matches!(resolved, KittyFormat::Rgb));
    }

    #[test]
    fn placeholder_cells_scroll_with_grid() {
        // Placeholders ARE just cells — they move with scroll exactly
        // like any other content. After SU 1, our row=2 placeholders
        // appear at row 1.
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&placeholder_sgr_fg(42));
        t.feed("\x1b[3;1H"); // row 2 (0-based)
        t.feed("\u{10EEEE}\u{0305}\u{0305}");
        t.feed("\u{10EEEE}\u{0305}\u{030D}");
        t.feed("\x1b[1S"); // SU 1
        let runs = t.kitty_placeholder_runs();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].screen_row, 1, "row shifted from 2 → 1");
    }

    //
    // K4: image / placement ids + a=t / a=p / a=d
    //

    #[test]
    fn kitty_apc_a_p_places_previously_transmitted_image() {
        // Lifecycle test: a=t deposits the image with i=7 (no
        // placement); a=p,i=7,c=4,r=2 places it at the cursor.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        // a=t
        let png = kitty_png(4, 4);
        t.feed(&kitty_apc("a=t,f=100,c=4,r=2,i=7", &png));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        assert!(!uploads[0].display_immediately);
        // main.rs would call register_kitty_image_id here. Simulate.
        t.register_kitty_image_id(7, ImageId(99));
        // No placement yet.
        assert!(t.live_placements().is_empty());

        // a=p — place at cursor.
        t.feed("\x1b[5;1H");
        t.feed(&kitty_apc_control_only("a=p,i=7,c=4,r=2"));
        let placements = t.live_placements();
        assert_eq!(placements.len(), 1);
        assert_eq!(placements[0].image, ImageId(99));
        assert_eq!(placements[0].kitty_image_id, Some(7));
        // Cursor advanced by 2 rows (cell_extent).
        assert_eq!(t.cursor().row, 4 + 2);
    }

    #[test]
    fn kitty_apc_a_p_unknown_id_silently_drops() {
        // Per spec, placing an unknown image is a no-op.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        t.feed(&kitty_apc_control_only("a=p,i=999,c=2,r=1"));
        assert!(t.live_placements().is_empty());
    }

    #[test]
    fn kitty_apc_a_p_with_c_one_default_is_visible_placeholder() {
        // c=/r= omitted on a=p — fall back to (1,1) so the placement
        // is at least visible. The Kitty spec allows omitting c/r but
        // expects the terminal to know the image's natural cell size;
        // we don't track that yet, so (1,1) is the safer default.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        t.register_kitty_image_id(3, ImageId(50));
        t.feed(&kitty_apc_control_only("a=p,i=3"));
        let placements = t.live_placements();
        assert_eq!(placements.len(), 1);
        assert_eq!(placements[0].rows, 1);
        assert_eq!(placements[0].cols, 1);
    }

    #[test]
    fn kitty_apc_a_p_records_placement_id() {
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        t.register_kitty_image_id(5, ImageId(10));
        t.feed(&kitty_apc_control_only("a=p,i=5,p=42,c=1,r=1"));
        let placements = t.live_placements();
        assert_eq!(placements.len(), 1);
        assert_eq!(placements[0].kitty_placement_id, Some(42));
    }

    #[test]
    fn kitty_apc_a_p_with_capital_c_skips_cursor_advance() {
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        t.register_kitty_image_id(1, ImageId(1));
        t.feed("\x1b[5;1H");
        t.feed(&kitty_apc_control_only("a=p,i=1,c=4,r=3,C=1"));
        // C=1 → cursor doesn't move.
        assert_eq!(t.cursor().row, 4);
    }

    #[test]
    fn kitty_apc_a_d_by_image_removes_all_placements_for_image() {
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        t.register_kitty_image_id(7, ImageId(99));
        // Place the same image twice at different cells.
        t.feed(&kitty_apc_control_only("a=p,i=7,c=2,r=1"));
        t.feed(&kitty_apc_control_only("a=p,i=7,c=2,r=1"));
        assert_eq!(t.live_placements().len(), 2);

        t.feed(&kitty_apc_control_only("a=d,d=i,i=7"));
        assert!(t.live_placements().is_empty());
        // Mapping also gone — subsequent a=p,i=7 won't resurrect.
        assert!(t.kitty_image_id_lookup(7).is_none());
    }

    #[test]
    fn kitty_apc_a_d_by_placement_removes_only_matching() {
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        t.register_kitty_image_id(7, ImageId(99));
        t.feed(&kitty_apc_control_only("a=p,i=7,p=1,c=1,r=1"));
        t.feed(&kitty_apc_control_only("a=p,i=7,p=2,c=1,r=1"));
        t.feed(&kitty_apc_control_only("a=p,i=7,p=3,c=1,r=1"));
        assert_eq!(t.live_placements().len(), 3);

        t.feed(&kitty_apc_control_only("a=d,d=p,p=2"));
        let surviving: Vec<Option<u32>> = t
            .live_placements()
            .iter()
            .map(|p| p.kitty_placement_id)
            .collect();
        // p=2 is gone; p=1 and p=3 survive (order preserved).
        assert_eq!(surviving, vec![Some(1), Some(3)]);
        // Image mapping kept — only the placement was deleted.
        assert_eq!(t.kitty_image_id_lookup(7), Some(ImageId(99)));
    }

    #[test]
    fn kitty_apc_a_d_all_removes_kitty_only_not_iterm() {
        // a=d,d=a sweeps Kitty placements; iTerm / Cmd-Shift-I
        // placements (those without a kitty_image_id) survive.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        // iTerm-style placement (no kitty IDs).
        t.insert_placement(ImageId(1), 0, 0, 1, 1, 0);
        // Kitty placement.
        t.register_kitty_image_id(5, ImageId(50));
        t.feed(&kitty_apc_control_only("a=p,i=5,c=1,r=1"));
        assert_eq!(t.live_placements().len(), 2);

        t.feed(&kitty_apc_control_only("a=d,d=a"));
        let remaining = t.live_placements();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].kitty_image_id, None);
        // All Kitty mappings gone.
        assert!(t.kitty_image_id_lookup(5).is_none());
    }

    #[test]
    fn kitty_apc_referenced_image_ids_keeps_transmitted_only_images() {
        // `a=t` registers an image-id mapping but creates no placement.
        // The bare placement list would not reference the store id, so
        // mark-and-sweep would drop the GPU image. The map's values
        // need to make it into `referenced_image_ids` so the image
        // survives until either `a=p` references it or `a=d` clears it.
        let mut t = Terminal::new(80, 24, 100);
        t.register_kitty_image_id(7, ImageId(99));
        assert!(t.referenced_image_ids().contains(&ImageId(99)));
    }

    #[test]
    fn kitty_apc_register_idempotent_overwrites_with_new_store_id() {
        // Client retransmits the same `i=` with new pixels → mapping
        // updates to the new store id. Both the new and stale ids
        // appear in referenced until mark-and-sweep prunes the stale.
        let mut t = Terminal::new(80, 24, 100);
        t.register_kitty_image_id(7, ImageId(1));
        t.register_kitty_image_id(7, ImageId(2));
        assert_eq!(t.kitty_image_id_lookup(7), Some(ImageId(2)));
        let refs = t.referenced_image_ids();
        assert!(refs.contains(&ImageId(2)));
        // The old id is no longer reachable through the map.
        assert!(!refs.contains(&ImageId(1)));
    }

    #[test]
    fn kitty_apc_query_replies_ok_with_request_id() {
        // kitty +kitten icat probes support with `a=q,i=N` on startup.
        // For supported (format, transmission) tuples we reply
        // `\e_Gi=N;OK\e\\`. Defaults are f=100 / t=d (both supported).
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&kitty_apc_control_only("a=q,i=42,s=1,v=1"));
        assert!(t.take_pending_image_uploads().is_empty());
        let reply = t.take_response();
        assert_eq!(reply, b"\x1b_Gi=42;OK\x1b\\");
    }

    #[test]
    fn kitty_apc_query_without_id_replies_ok_idless() {
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&kitty_apc_control_only("a=q,s=1,v=1"));
        let reply = t.take_response();
        assert_eq!(reply, b"\x1b_G;OK\x1b\\");
    }

    #[test]
    fn kitty_apc_query_replies_ok_for_raw_rgb_direct() {
        // f=24 (raw RGB) over direct base64 IS supported — we PNG-encode
        // the raw bytes on the way in. icat uses this for JPG and other
        // non-PNG sources.
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&kitty_apc_control_only("a=q,i=5,f=24,s=1,v=1"));
        assert_eq!(t.take_response(), b"\x1b_Gi=5;OK\x1b\\");
    }

    #[test]
    fn kitty_apc_query_replies_enotsupported_for_raw_over_file() {
        // f=24 + t=f is a weird combo (file containing raw RGB bytes
        // with no header to know dimensions) and we don't handle it.
        // Pin the negative response.
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&kitty_apc_control_only("a=q,i=5,f=24,t=f,s=1,v=1"));
        let reply = t.take_response();
        let s = std::str::from_utf8(&reply).unwrap();
        assert!(s.starts_with("\x1b_Gi=5;ENOTSUPPORTED"), "got: {s}");
    }

    #[cfg(unix)]
    #[test]
    fn kitty_apc_query_replies_ok_for_shared_memory_on_unix() {
        // t=s is wired up on Unix (POSIX shm_open). On other targets
        // we'd reply ENOTSUPPORTED; gate the test on unix.
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&kitty_apc_control_only("a=q,i=6,f=100,t=s,s=1,v=1"));
        assert_eq!(t.take_response(), b"\x1b_Gi=6;OK\x1b\\");
    }

    #[test]
    fn kitty_apc_query_replies_ok_for_t_f_file() {
        // t=f is the path icat picks for local PNGs — it MUST be in
        // the "supported" set or the kitten won't use it.
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&kitty_apc_control_only("a=q,i=7,f=100,t=f,s=1,v=1"));
        assert_eq!(t.take_response(), b"\x1b_Gi=7;OK\x1b\\");
    }

    #[test]
    fn kitty_apc_query_quiet_modes() {
        // q=0 default → reply always. q=1 → suppress OK but still send
        // errors. q=2 → silence everything.
        let mut t = Terminal::new(80, 24, 100);

        t.feed(&kitty_apc_control_only("a=q,i=1,q=0")); // supported + q=0
        assert_eq!(t.take_response(), b"\x1b_Gi=1;OK\x1b\\");

        t.feed(&kitty_apc_control_only("a=q,i=1,q=1")); // supported + q=1
        assert!(t.take_response().is_empty(), "q=1 should suppress OK");

        // Trigger an actual error via an unknown transmission (t=x);
        // shared memory IS supported on Unix now so it's no longer
        // a reliable error trigger.
        t.feed(&kitty_apc_control_only("a=q,i=2,f=100,t=x,q=1")); // error + q=1
        let reply = t.take_response();
        assert!(
            std::str::from_utf8(&reply).unwrap().contains("ENOTSUPPORTED"),
            "q=1 must NOT suppress errors; got: {:?}",
            reply,
        );

        t.feed(&kitty_apc_control_only("a=q,i=3,f=100,t=x,q=2")); // error + q=2
        assert!(t.take_response().is_empty(), "q=2 must silence everything");
    }

    #[test]
    fn kitty_apc_bad_base64_silently_dropped() {
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        // Invalid base64 — decode fails, nothing queued.
        t.feed("\x1b_Ga=T,f=100,c=2,r=1;not!base64!\x1b\\");
        assert!(t.take_pending_image_uploads().is_empty());
    }

    #[test]
    fn kitty_apc_chunked_with_internal_whitespace_assembles() {
        // Real kitty payloads sometimes wrap base64 lines for
        // readability inside the APC. The handler strips whitespace.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        let png = kitty_png(2, 2);
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        let with_ws: String = b64
            .as_bytes()
            .chunks(8)
            .map(|s| std::str::from_utf8(s).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        t.feed(&format!("\x1b_Ga=T,f=100,c=1,r=1;{}\x1b\\", with_ws));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        assert_eq!(uploads[0].pixel_size, Some((2, 2)));
    }

    //
    // K1 gap-fill: edge cases around handle_apc, chunk lifecycle, query
    // formatting, and interactions with the iTerm path.
    //

    #[test]
    fn kitty_apc_only_g_verb_no_semicolon_does_not_panic() {
        // APC payload that's literally just "G" — no control string, no
        // payload, no `;`. Splits to ("", ""), parses to defaults (a=T),
        // f=100 PNG, t=d direct. Bare empty payload base64-decodes to
        // zero bytes; the upload is still queued (pin current behavior).
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        t.feed("\x1b_G\x1b\\");
        // Defaults are a=T,f=100,t=d → finalize fires. Empty base64 → zero
        // bytes → peek_dimensions returns None → cell_extent falls back to
        // (1, 1). Pin so a future tightening of the validator is intentional.
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        assert!(uploads[0].bytes.is_empty());
        assert_eq!(uploads[0].pixel_size, None);
    }

    #[test]
    fn kitty_apc_payload_without_g_prefix_is_silently_dropped() {
        // APC payloads not starting with `G` aren't Kitty — drop without
        // touching the upload queue or response buffer.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        t.feed("\x1b_other,a=T,f=100;ZGF0YQ==\x1b\\");
        t.feed("\x1b_X-custom\x1b\\");
        assert!(t.take_pending_image_uploads().is_empty());
        assert!(t.take_response().is_empty());
    }

    #[test]
    fn kitty_apc_empty_payload_after_semicolon_still_queues_upload() {
        // `G<ctrl>;` with empty body — base64 of "" succeeds and gives
        // zero bytes. handle_apc still pushes a pending upload; the
        // downstream Store decode is what ultimately fails. Pin the
        // current "queue first, validate later" behavior.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        t.feed("\x1b_Ga=T,f=100,c=2,r=1;\x1b\\");
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        assert!(uploads[0].bytes.is_empty(), "empty base64 → zero bytes");
        // Explicit c/r honored even when bytes are empty.
        assert_eq!(uploads[0].cell_extent, (1, 2));
    }

    #[test]
    fn kitty_apc_chunked_survives_interleaved_unrelated_apc() {
        // A non-Kitty APC arriving between chunks must not perturb the
        // accumulator keyed by image_id. Real terminals see all kinds of
        // APC payloads from misbehaving apps — this guards against a
        // future refactor that accidentally clears `kitty_chunks` on any
        // APC.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        let png = kitty_png(4, 4);
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        let mid = b64.len() / 2;

        t.feed(&format!("\x1b_Ga=T,f=100,c=2,r=1,i=11,m=1;{}\x1b\\", &b64[..mid]));
        // Unrelated APC payload — no G prefix.
        t.feed("\x1b_other-vendor-payload\x1b\\");
        // OSC sneaks in too.
        t.feed("\x1b]0;ignore me\x07");
        // Resume the same image — accumulator must still have the first half.
        t.feed(&format!("\x1b_Gi=11;{}\x1b\\", &b64[mid..]));

        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1, "interleaving must not lose the chunk buffer");
        assert_eq!(uploads[0].pixel_size, Some((4, 4)));
    }

    #[test]
    fn kitty_apc_chunked_terminator_with_no_buffer_falls_to_single_chunk() {
        // m=0 with an `i=` that has no in-flight buffer — falls through
        // to the single-chunk path. Pin: this should produce one upload
        // from the terminator's own payload (not zero, not two).
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        let png = kitty_png(2, 2);
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        t.feed(&format!("\x1b_Ga=T,f=100,c=1,r=1,i=77,m=0;{}\x1b\\", b64));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        assert_eq!(uploads[0].pixel_size, Some((2, 2)));
    }

    #[test]
    fn kitty_apc_chunks_cleared_after_flush_so_id_reuse_works() {
        // After a flush, the HashMap entry for that id is removed — so
        // a second transmission reusing the same id starts fresh and
        // gets its own first-chunk sizing (rather than inheriting the
        // prior one). Demonstrates the lifecycle without a private accessor.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        let png = kitty_png(4, 4);
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        let mid = b64.len() / 2;

        // First transmission with id=5, sized 2×1.
        t.feed(&format!("\x1b_Ga=T,f=100,c=2,r=1,i=5,m=1;{}\x1b\\", &b64[..mid]));
        t.feed(&format!("\x1b_Gi=5;{}\x1b\\", &b64[mid..]));
        let first = t.take_pending_image_uploads();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].cell_extent, (1, 2));

        // Reuse the same id with different sizing — must NOT inherit
        // the prior accumulator or its (2,1) sizing.
        t.feed(&format!("\x1b_Ga=T,f=100,c=4,r=2,i=5,m=1;{}\x1b\\", &b64[..mid]));
        t.feed(&format!("\x1b_Gi=5;{}\x1b\\", &b64[mid..]));
        let second = t.take_pending_image_uploads();
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].cell_extent, (2, 4), "second transmission's sizing wins");
    }

    #[test]
    fn kitty_apc_many_concurrent_image_ids_all_flush_independently() {
        // Interleave 5 chunked transmissions with distinct image_ids;
        // every one should flush cleanly when its terminator arrives.
        // Guards the HashMap-keyed-by-id design from a regression that
        // serializes uploads or cross-contaminates buffers.
        let mut t = Terminal::new(160, 60, 100);
        t.set_cell_size_px(8, 16);
        let png = kitty_png(4, 4);
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        let mid = b64.len() / 2;

        let ids = [101u32, 202, 303, 404, 505];
        // First-half chunks for all ids — interleaved.
        for &id in &ids {
            t.feed(&format!(
                "\x1b_Ga=T,f=100,c=2,r=1,i={},m=1;{}\x1b\\",
                id,
                &b64[..mid],
            ));
        }
        // No uploads yet — all in flight.
        assert!(t.take_pending_image_uploads().is_empty());
        // Send terminators in a different order — independence test.
        for &id in &[303, 101, 505, 202, 404] {
            t.feed(&format!("\x1b_Gi={};{}\x1b\\", id, &b64[mid..]));
        }
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 5);
        // All decoded to the original 4×4 — i.e. no buffer mixing.
        for up in &uploads {
            assert_eq!(up.pixel_size, Some((4, 4)));
        }
    }

    #[test]
    fn kitty_apc_query_explicit_q_zero_still_replies() {
        // Spec: q=0 == default == reply. Pin so a future shortcut
        // ("if q is set, suppress") doesn't silently break icat.
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&kitty_apc_control_only("a=q,i=9,q=0"));
        assert_eq!(t.take_response(), b"\x1b_Gi=9;OK\x1b\\");
    }

    #[test]
    fn kitty_apc_query_malformed_quiet_falls_back_to_zero_and_replies() {
        // `q=banana` doesn't parse — falls back to default 0, so the
        // reply fires. Pin the lenient parse contract.
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&kitty_apc_control_only("a=q,i=3,q=banana"));
        assert_eq!(t.take_response(), b"\x1b_Gi=3;OK\x1b\\");
    }

    #[test]
    fn kitty_apc_malformed_format_value_is_dropped() {
        // f=abc and f= (empty value) both fall to KittyFormat::Other,
        // which the dispatcher drops. Pin.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        let png = kitty_png(2, 2);
        t.feed(&kitty_apc("a=T,f=abc,c=1,r=1", &png));
        t.feed(&kitty_apc("a=T,f=,c=1,r=1", &png));
        assert!(t.take_pending_image_uploads().is_empty());
    }

    #[test]
    fn kitty_apc_explicit_c_and_r_fit_exactly_no_aspect_munging() {
        // K1's compute_cell_extent is called with preserve_aspect=true,
        // but when BOTH axes are explicit Cells the aspect branch is a
        // no-op — Kitty's c/r are exact cell extents. Pin so changing
        // the iTerm-shared default doesn't accidentally squish Kitty.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        // Use a wildly non-square source (32×4 px) with c=10,r=10. If
        // aspect were applied, one axis would be overridden; with both
        // explicit, we should get exactly (10, 10).
        let png = kitty_png(32, 4);
        t.feed(&kitty_apc("a=T,f=100,c=10,r=10", &png));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        assert_eq!(uploads[0].cell_extent, (10, 10));
    }

    #[test]
    fn kitty_apc_in_decstbm_scroll_region_anchor_follows_scrolls() {
        // Kitty mirror of osc_1337_in_decstbm_scroll_region_anchor_follows_scrolls.
        // Cursor pinned at scroll_bottom; an image taller than the
        // remaining region rows scrolls in-region. Anchor compensation
        // (original_row - scrolls) must still resolve to a visible row.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        t.feed("\x1b[10;20r"); // scroll region rows 10..20 (1-based)
        t.feed("\x1b[20;1H"); // cursor at row 20 (bottom of region)
        // Drain any uploads / responses from preamble (defensive).
        let _ = t.take_pending_image_uploads();
        let _ = t.take_response();
        let png = kitty_png(4, 4);
        // c=4, r=3 → 3 line-feeds at scroll_bottom → 3 in-region scrolls.
        t.feed(&kitty_apc("a=T,f=100,c=4,r=3", &png));
        assert_eq!(t.cursor().row, 19, "cursor pinned at scroll_bottom");
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        // 3 line-feeds, 0 cursor advance → scrolls = 3 - 0 = 3.
        // anchor row = 19 - 3 = 16.
        assert_eq!(uploads[0].cell_anchor, (16, 0));
        assert_eq!(uploads[0].cell_extent, (3, 4));
    }

    #[test]
    fn mixed_iterm_osc_and_kitty_apc_share_queue_in_arrival_order() {
        // Both paths push onto the same `pending_image_uploads` queue.
        // A feed containing one of each must surface both in arrival
        // order so the caller's hand-off to Store sees them as the
        // host sent them.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        // iTerm first (label = None per minimal OSC), then Kitty.
        let png = kitty_png(2, 2);
        let combo = format!("{}{}", iterm_osc(""), kitty_apc("a=T,f=100,c=1,r=1", &png));
        t.feed(&combo);
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 2);
        // Order: iTerm OSC was first → comes first.
        assert_eq!(uploads[0].label.as_deref(), None);
        assert_eq!(uploads[1].label.as_deref(), Some("kitty graphics"));
    }

    //
    // P2.3: cell-extent math + cursor advance + anchor capture.
    //

    #[test]
    fn compute_cell_extent_explicit_cells_passes_through() {
        let (rows, cols) = compute_cell_extent(
            ImageSizeSpec::Cells(10),
            ImageSizeSpec::Cells(5),
            Some((100, 50)),
            8,
            16,
            80,
            24,
            true,
        );
        assert_eq!((rows, cols), (5, 10));
    }

    #[test]
    fn compute_cell_extent_pixels_ceil_divides() {
        // 100px / 8px cell = 12.5 → ceil → 13 cells.
        let (_rows, cols) = compute_cell_extent(
            ImageSizeSpec::Pixels(100),
            ImageSizeSpec::Auto,
            Some((100, 16)),
            8,
            16,
            80,
            24,
            false,
        );
        assert_eq!(cols, 13);
    }

    #[test]
    fn compute_cell_extent_percent_of_viewport() {
        // 50% of 640px viewport = 320px → ceil-div by 8 = 40 cells.
        let (_rows, cols) = compute_cell_extent(
            ImageSizeSpec::Percent(50),
            ImageSizeSpec::Auto,
            Some((100, 16)),
            8,
            16,
            80, // viewport_cols → viewport_w = 640px
            24,
            false,
        );
        assert_eq!(cols, 40);
    }

    #[test]
    fn compute_cell_extent_auto_uses_image_dims() {
        // 32×48 image, 8×16 cells → 4 cols, 3 rows.
        let (rows, cols) = compute_cell_extent(
            ImageSizeSpec::Auto,
            ImageSizeSpec::Auto,
            Some((32, 48)),
            8,
            16,
            80,
            24,
            true,
        );
        assert_eq!((rows, cols), (3, 4));
    }

    #[test]
    fn compute_cell_extent_preserve_aspect_fills_auto_axis() {
        // width=Cells(10) (=80px), height=Auto, image is 100x50 (aspect 2:1),
        // preserve=true. height_px = 80 * 50/100 = 40 → 40/16 = 2.5 → 3 rows.
        let (rows, cols) = compute_cell_extent(
            ImageSizeSpec::Cells(10),
            ImageSizeSpec::Auto,
            Some((100, 50)),
            8,
            16,
            80,
            24,
            true,
        );
        assert_eq!((rows, cols), (3, 10));
    }

    #[test]
    fn compute_cell_extent_preserve_aspect_does_not_override_explicit() {
        // Both axes explicit → preserve is ignored. iTerm contract.
        let (rows, cols) = compute_cell_extent(
            ImageSizeSpec::Cells(10),
            ImageSizeSpec::Cells(2),
            Some((100, 100)),
            8,
            16,
            80,
            24,
            true,
        );
        assert_eq!((rows, cols), (2, 10));
    }

    #[test]
    fn compute_cell_extent_no_pixel_size_no_explicit_falls_back_to_one_cell() {
        // Worst case — format wasn't recognised by peek_dimensions and the
        // OSC didn't supply sizing. Visible-but-tiny beats a panic.
        let (rows, cols) = compute_cell_extent(
            ImageSizeSpec::Auto,
            ImageSizeSpec::Auto,
            None,
            8,
            16,
            80,
            24,
            true,
        );
        assert_eq!((rows, cols), (1, 1));
    }

    #[test]
    fn osc_1337_advances_cursor_by_image_rows() {
        let mut t = Terminal::new(80, 24, 100);
        // Set known cell size: 8×16. The fixture is a 2×2 PNG; auto sizing
        // gives 1×1 cells.
        t.set_cell_size_px(8, 16);
        // Move cursor to a known row first.
        t.feed("\x1b[5;1H"); // CUP row 5 col 1
        t.feed(&iterm_osc(";width=4;height=3")); // 3 rows × 4 cols
        // Cursor should have moved down 3 rows: row 4 (0-indexed) + 3 = 7.
        assert_eq!(t.cursor().row, 7);
        assert_eq!(t.cursor().col, 0);

        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        // Anchor was captured at the cursor's pre-LF position (row 4 since
        // CUP is 1-based: row 5 → index 4). Col 0 (CUP `;1` → index 0).
        assert_eq!(uploads[0].cell_anchor, (4, 0));
        assert_eq!(uploads[0].cell_extent, (3, 4));
    }

    #[test]
    fn osc_1337_do_not_move_cursor_skips_advance() {
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        t.feed("\x1b[5;1H"); // row 5 col 1 (1-based)
        t.feed(&iterm_osc(";width=4;height=3;doNotMoveCursor=1"));
        // Cursor stays at original position.
        assert_eq!(t.cursor().row, 4);
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads[0].cell_anchor, (4, 0));
        assert_eq!(uploads[0].cell_extent, (3, 4));
    }

    #[test]
    fn osc_1337_near_bottom_scrolls_grid_and_anchor_follows() {
        // Cursor at row 22 (1-based 23) in a 24-row grid, image is 5 rows.
        // 5 LFs from row 22: rows 22→23 is the only non-scrolling LF.
        // 4 LFs scroll. Anchor should be 22 - 4 = 18.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        t.feed("\x1b[23;1H"); // row 23 (1-based) = index 22
        t.feed(&iterm_osc(";width=4;height=5"));
        assert_eq!(t.cursor().row, 23); // pinned at bottom
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads[0].cell_anchor.0, 18);
        // The 4 scrolls also push any prior scrollback-eligible content
        // out — just sanity-check the anchor here.
    }

    //
    // P2.4: remove_placements_with_image — failure-cleanup helper.
    //

    #[test]
    fn remove_placements_with_image_drops_matching_entries_across_grids() {
        let mut t = Terminal::new(40, 10, 100);
        // Two placements referencing image 7 on primary; one referencing
        // image 8 on primary. Switch to alt and add another image-7
        // placement. Switch back, ensure remove(7) drops all three image-7
        // entries (across primary + alt) but leaves image 8 alone.
        place(&mut t, 7, 0, 0, 1, 2);
        place(&mut t, 7, 2, 0, 1, 2);
        place(&mut t, 8, 4, 0, 1, 2);
        t.feed("\x1b[?1049h");
        place(&mut t, 7, 0, 0, 1, 2);
        t.feed("\x1b[?1049l");

        let removed = t.remove_placements_with_image(ImageId(7));
        assert_eq!(removed, 3);
        // image 8 survives on primary.
        let primary_images: Vec<u32> =
            t.live_placements().iter().map(|p| p.image.0).collect();
        assert_eq!(primary_images, vec![8]);
        // alt grid is also cleared of image 7.
        t.feed("\x1b[?1049h");
        assert!(t.live_placements().is_empty());
    }

    #[test]
    fn remove_placements_with_image_clears_scrollback_entries() {
        let mut t = Terminal::new(20, 5, 100);
        place(&mut t, 99, 0, 0, 1, 2);
        // Scroll it into scrollback.
        t.feed("\x1b[1S");
        assert_eq!(t.scrollback_placements_for_test().len(), 1);
        let removed = t.remove_placements_with_image(ImageId(99));
        assert_eq!(removed, 1);
        assert!(t.scrollback_placements_for_test().is_empty());
    }

    #[test]
    fn remove_placements_with_image_unknown_id_is_no_op() {
        let mut t = Terminal::new(20, 5, 100);
        place(&mut t, 1, 0, 0, 1, 1);
        // Removing an id no placement references shouldn't touch the
        // surviving placements.
        let removed = t.remove_placements_with_image(ImageId(42));
        assert_eq!(removed, 0);
        assert_eq!(t.live_placements().len(), 1);
    }

    #[test]
    fn osc_1337_with_no_cell_size_set_still_parses_cleanly() {
        // Default cell size is 1×1 — pixel-spec produces huge cell counts
        // but the math shouldn't panic and the queue should still get an
        // entry so callers can detect the OSC arrived.
        let mut t = Terminal::new(80, 24, 100);
        // Skip set_cell_size_px deliberately.
        t.feed(&iterm_osc(";width=10;height=2"));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        assert_eq!(uploads[0].cell_extent, (2, 10));
    }

    #[test]
    fn keep_placements_in_scrollback_false_drops_on_full_screen_scroll() {
        let mut t = Terminal::new(20, 5, 100);
        t.set_keep_placements_in_scrollback(false);
        place(&mut t, 1, 0, 0, 1, 2);
        t.feed("\x1b[1S");
        // Without retention, the scrolled-off placement is dropped — not
        // promoted to scrollback_placements.
        assert!(t.live_placements().is_empty());
        assert!(t.scrollback_placements_for_test().is_empty());
    }

    #[test]
    fn keep_placements_in_scrollback_false_drops_on_resize_spill() {
        let mut t = Terminal::new(20, 10, 100);
        t.set_keep_placements_in_scrollback(false);
        place(&mut t, 1, 2, 0, 2, 4);
        t.resize(20, 6); // spill = 4, placement at row 2 spills
        assert!(t.live_placements().is_empty());
        assert!(t.scrollback_placements_for_test().is_empty());
    }

    #[test]
    fn set_keep_placements_in_scrollback_false_clears_existing_queue() {
        let mut t = Terminal::new(20, 5, 100);
        place(&mut t, 1, 0, 0, 1, 2);
        t.feed("\x1b[1S"); // placement → scrollback_placements
        assert_eq!(t.scrollback_placements_for_test().len(), 1);
        t.set_keep_placements_in_scrollback(false);
        // Toggle drains the queue — otherwise stale entries would linger
        // until the next scrollback eviction.
        assert!(t.scrollback_placements_for_test().is_empty());
    }

    #[test]
    fn scrollback_eviction_drops_scrollback_placement_at_index_0() {
        let mut t = Terminal::new(10, 3, 2); // scrollback_limit = 2
        place(&mut t, 1, 0, 0, 1, 2);
        // Scroll twice — fills scrollback with 2 entries; our placement
        // (1 row tall, anchored at row 0) lands in scrollback at index 0
        // after the first scroll and stays put through the second.
        t.feed("\x1b[2S");
        assert_eq!(t.scrollback_placements_for_test().len(), 1);
        assert_eq!(t.scrollback_placements_for_test()[0].0, 0);
        // Third scroll: scrollback is at limit, pop_front evicts the row at
        // index 0 — placement anchored there must be dropped.
        t.feed("\x1b[1S");
        assert!(t.scrollback_placements_for_test().is_empty());
    }

    //
    // Placement helper tests (slice 2 follow-ups).
    //

    #[test]
    fn placement_fully_off_grid_each_edge() {
        // Pin every branch of the off-grid check independently. Drifting any
        // one of these to `<` vs `<=` (or `>=` vs `>`) would silently keep a
        // dead placement in the live list across scroll / resize.
        let mk = |top: isize, left: isize, rows: u16, cols: u16| Placement {
            id: 1,
            image: ImageId(1),
            top_row: top,
            left_col: left,
            rows,
            cols,
            z: 0,
            pixel_offset: (0, 0),
            src_rect: None,
            kitty_image_id: None,
            kitty_placement_id: None,
        };
        let grid_r = 10usize;
        let grid_c = 8usize;

        // Off top: bottom_row == 0 (top=-1 + rows=1).
        assert!(mk(-1, 0, 1, 1).fully_off_grid(grid_r, grid_c));
        // Boundary on top edge: bottom_row == 1 → barely visible, NOT off.
        assert!(!mk(-1, 0, 2, 1).fully_off_grid(grid_r, grid_c));

        // Off bottom: top_row == grid_rows.
        assert!(mk(grid_r as isize, 0, 1, 1).fully_off_grid(grid_r, grid_c));
        // Boundary: top_row == grid_rows - 1 → last row, still on.
        assert!(!mk(grid_r as isize - 1, 0, 1, 1).fully_off_grid(grid_r, grid_c));

        // Horizontal off-screen is intentionally NOT considered off-grid
        // (those placements come back into view if the user widens the
        // window). See the rustdoc on `fully_off_grid` and the
        // `resize_horizontal_shrink_preserves_off_screen_placements`
        // regression test.
        assert!(!mk(0, -2, 1, 2).fully_off_grid(grid_r, grid_c));
        assert!(!mk(0, grid_c as isize, 1, 1).fully_off_grid(grid_r, grid_c));

        // Fully inside is the obvious negative case.
        assert!(!mk(2, 2, 1, 1).fully_off_grid(grid_r, grid_c));
    }

    #[test]
    fn placement_rows_intersect_boundaries() {
        // Exclusive-on-top, inclusive-on-bottom semantics: a placement whose
        // bottom_row equals `top` (i.e. lives entirely in rows above the
        // region) must NOT intersect, while top_row==bottom must intersect.
        // Driving scroll-region math from here, a flip would shift the wrong
        // placements during a single-row scroll.
        let mk = |top: isize, rows: u16| Placement {
            id: 1, image: ImageId(1), top_row: top, left_col: 0, rows, cols: 1, z: 0,
            pixel_offset: (0, 0), src_rect: None,
            kitty_image_id: None, kitty_placement_id: None,
        };
        // Region [5..=9].
        // Placement at rows 3..=4 → bottom_row=5 == top → no overlap.
        assert!(!mk(3, 2).rows_intersect(5, 9));
        // Placement at rows 4..=5 → bottom_row=6 > 5 and top_row=4 <= 9 → yes.
        assert!(mk(4, 2).rows_intersect(5, 9));
        // Placement starting exactly at top of region.
        assert!(mk(5, 1).rows_intersect(5, 9));
        // Placement starting exactly at bottom of region (top_row == bottom).
        assert!(mk(9, 3).rows_intersect(5, 9));
        // Placement just past the bottom — top_row=10 > 9 → no.
        assert!(!mk(10, 1).rows_intersect(5, 9));
    }

    #[test]
    fn insert_placement_defaults_pixel_offset_and_src_rect_to_phase1_values() {
        // The original 7-arg `insert_placement` must keep producing the
        // exact same `Placement` data as before — pixel_offset zeroed and
        // src_rect None. Phase 1 callers (OSC 1337 parser, debug keybind)
        // rely on this; anything else would silently shift their draws.
        let mut t = Terminal::new(10, 10, 100);
        let id = t.insert_placement(ImageId(1), 2, 3, 4, 5, 0);
        let p = t.live_placements().iter().find(|p| p.id == id).unwrap();
        assert_eq!(p.pixel_offset, (0, 0));
        assert_eq!(p.src_rect, None);
    }

    #[test]
    fn insert_placement_with_crop_round_trips_offsets_and_rect() {
        // `insert_placement_with_crop` is the phase-2 entry point; the values
        // it accepts must survive into `live_placements()` unchanged so the
        // renderer sees what the parser produced. Single field-by-field
        // round trip pins the wiring.
        let mut t = Terminal::new(10, 10, 100);
        let id = t.insert_placement_with_crop(
            ImageId(7), 1, 2, 3, 4, 0, (5, -6), Some((10, 20, 30, 40)),
        );
        let p = t.live_placements().iter().find(|p| p.id == id).unwrap();
        assert_eq!(p.image, ImageId(7));
        assert_eq!(p.top_row, 1);
        assert_eq!(p.left_col, 2);
        assert_eq!(p.rows, 3);
        assert_eq!(p.cols, 4);
        assert_eq!(p.pixel_offset, (5, -6));
        assert_eq!(p.src_rect, Some((10, 20, 30, 40)));
    }

    #[test]
    fn pixel_offset_does_not_change_fully_off_grid() {
        // pixel_offset is a sub-cell visual nudge; it must NOT alter which
        // cells the placement covers for eviction math. If it did, a
        // placement with a +50px offset would falsely escape eviction.
        let p_zero = Placement {
            id: 1, image: ImageId(1), top_row: 5, left_col: 0,
            rows: 1, cols: 1, z: 0, pixel_offset: (0, 0), src_rect: None,
            kitty_image_id: None, kitty_placement_id: None,
        };
        let p_offset = Placement {
            pixel_offset: (999, -999), ..p_zero.clone()
        };
        assert_eq!(
            p_zero.fully_off_grid(10, 10),
            p_offset.fully_off_grid(10, 10),
        );
        // And neither should be off-grid at row 5 of a 10-row grid.
        assert!(!p_zero.fully_off_grid(10, 10));
        assert!(!p_offset.fully_off_grid(10, 10));
    }

    #[test]
    fn referenced_image_ids_is_empty_when_no_placements_anywhere() {
        // The renderer feeds this set straight into Store::retain every
        // frame; an empty union must yield an empty set (which evicts
        // everything) rather than e.g. a panic from the HashSet builder.
        let t = Terminal::new(10, 5, 100);
        assert!(t.referenced_image_ids().is_empty());
    }

    #[test]
    fn insert_lines_shifts_placements_down_within_region() {
        // IL routes through scroll_region_down with cursor.row as top — the
        // placement at row 2 should slide down to row 4. Mirrors the
        // existing direct-CSI-T test but via the higher-level IL command.
        let mut t = Terminal::new(20, 10, 100);
        place(&mut t, 1, 2, 0, 1, 4);
        // Cursor to row 1 (1-based) so IL's region top = 0 and the placement
        // at row 2 falls inside.
        t.feed("\x1b[1;1H\x1b[2L"); // CUP 1,1 then IL 2
        assert_eq!(t.live_placements().len(), 1);
        assert_eq!(t.live_placements()[0].top_row, 4);
    }

    #[test]
    fn delete_lines_shifts_placements_up_within_region() {
        // DL routes through scroll_region_up; full-width grid means the
        // shift-and-maybe-drop placement path runs. Placement at row 3
        // shifts up to row 1.
        let mut t = Terminal::new(20, 10, 100);
        place(&mut t, 1, 3, 0, 1, 4);
        t.feed("\x1b[1;1H\x1b[2M"); // CUP 1,1 then DL 2
        assert_eq!(t.live_placements().len(), 1);
        assert_eq!(t.live_placements()[0].top_row, 1);
    }

    #[test]
    fn delete_lines_drops_placement_that_fully_exits_top_of_region() {
        // DL on a full-screen region with cursor at home: a placement
        // entirely within the deleted span should fall off the top. Because
        // DL is NOT scroll_region_up_by (it doesn't push into scrollback),
        // the placement is dropped outright — not promoted.
        let mut t = Terminal::new(20, 10, 100);
        place(&mut t, 1, 0, 0, 2, 4);
        t.feed("\x1b[1;1H\x1b[3M"); // CUP 1,1 then DL 3
        assert!(t.live_placements().is_empty());
        // DL doesn't feed scrollback, so the placement is gone — not
        // promoted like a true SU would do.
        assert!(t.scrollback_placements_for_test().is_empty());
    }

    #[test]
    fn viewport_scroll_down_leaves_placements_alone() {
        // Scrolling the viewport is a pure read-side operation — placements
        // are anchored in grid coords, not visual rows, so view_offset
        // must not perturb them. Regression guard for a future "smart"
        // scroll that accidentally touches Grid::placements.
        let mut t = Terminal::new(20, 5, 100);
        place(&mut t, 1, 2, 0, 1, 2);
        // Push history into scrollback so view_offset has somewhere to go.
        t.feed("\r\nline\r\nline\r\nline\r\nline\r\nline");
        let before: Vec<_> = t
            .live_placements()
            .iter()
            .map(|p| (p.id, p.top_row, p.left_col))
            .collect();
        let _ = t.scroll_up(2);
        let _ = t.scroll_down(1);
        let after: Vec<_> = t
            .live_placements()
            .iter()
            .map(|p| (p.id, p.top_row, p.left_col))
            .collect();
        assert_eq!(before, after);
    }

    #[test]
    fn scrollback_eviction_keeps_remaining_indices_consistent() {
        // Build two scrollback placements at distinct indices, then evict the
        // oldest scrollback row. The surviving placement's index must
        // decrement (not get dropped, not stay stale) so future lookups by
        // sb row still resolve to it.
        let mut t = Terminal::new(10, 3, 4); // scrollback_limit = 4
        place(&mut t, 1, 0, 0, 1, 2); // A at row 0
        place(&mut t, 2, 1, 0, 1, 2); // B at row 1
        // First scroll: A exits to sb row 0, B slides up to live row 0.
        t.feed("\x1b[1S");
        assert_eq!(t.scrollback_placements_for_test().len(), 1);
        // Second scroll: B exits — scrollback now has 2 entries, A at sb_row 0
        // and B at sb_row 1.
        t.feed("\x1b[1S");
        let sb = t.scrollback_placements_for_test();
        assert_eq!(sb.len(), 2);
        let by_image: std::collections::HashMap<u32, isize> =
            sb.iter().map(|(row, p)| (p.image.0, *row)).collect();
        assert_eq!(by_image[&1], 0);
        assert_eq!(by_image[&2], 1);

        // Two more scrolls of empty rows fill scrollback to its limit (4).
        t.feed("\x1b[2S");
        // Now a 5th scroll forces pop_front → sb_row 0 evicted (A dropped),
        // surviving entries decrement: B should be at sb_row 0.
        t.feed("\x1b[1S");
        let sb = t.scrollback_placements_for_test();
        assert_eq!(sb.len(), 1);
        assert_eq!(sb[0].0, 0);
        assert_eq!(sb[0].1.image.0, 2);
    }

    #[test]
    fn resize_shrink_then_grow_round_trips_placement_anchor() {
        // Same placement, shrink-then-grow by the same amount — should land
        // back at (roughly) the original row. Catches a sign-flip in the
        // refill math that would otherwise only show up via the
        // visible-but-wrong-position bug.
        let mut t = Terminal::new(20, 10, 100);
        place(&mut t, 1, 2, 0, 2, 4);
        t.resize(20, 4); // spill = 6 — placement goes to scrollback.
        assert!(t.live_placements().is_empty());
        assert_eq!(t.scrollback_placements_for_test().len(), 1);
        t.resize(20, 10);
        // Refill drains the most-recently-pushed scrollback rows back into
        // the grid; the placement at the oldest spilled row should
        // re-emerge in the live list.
        assert_eq!(t.live_placements().len(), 1);
        // Original top_row was 2; after a 6-row spill the placement landed
        // at sb_row 2, and the refill puts it back at top_row 2.
        assert_eq!(t.live_placements()[0].top_row, 2);
    }

    //
    // P2.5: compute_cell_extent edge cases — bounds, saturation, divide-by-
    // zero guards. Behavior that would silently regress to a panic or a
    // wrong-sized placement on a malformed input.
    //

    #[test]
    fn compute_cell_extent_both_pixels_with_preserve_aspect_uses_explicit() {
        // Explicit beats preserve, even when both axes are explicit and
        // would distort the aspect. iTerm contract — verified end-to-end
        // here since this is a common "looks stretched" footgun.
        let (rows, cols) = compute_cell_extent(
            ImageSizeSpec::Pixels(80),
            ImageSizeSpec::Pixels(160),
            Some((100, 100)), // square image
            8,
            16,
            80,
            24,
            true, // preserve_aspect on
        );
        // 80/8 = 10 cols, 160/16 = 10 rows. Aspect-distorted but explicit.
        assert_eq!((rows, cols), (10, 10));
    }

    #[test]
    fn compute_cell_extent_percent_zero_clamps_to_one_cell() {
        // 0% would naturally produce 0 pixels; the resolve closure has a
        // `.max(1)` so callers can't accidentally request a zero-sized
        // placement that the renderer would skip.
        let (rows, cols) = compute_cell_extent(
            ImageSizeSpec::Percent(0),
            ImageSizeSpec::Percent(0),
            Some((100, 100)),
            8,
            16,
            80,
            24,
            false,
        );
        assert_eq!((rows, cols), (1, 1));
    }

    #[test]
    fn compute_cell_extent_percent_two_hundred_oversizes_past_viewport() {
        // Percent isn't clamped to 100 — `width=200%` yields a placement
        // that extends past the right edge. The grid + renderer handle
        // clipping; the math just produces the raw extent.
        let (_rows, cols) = compute_cell_extent(
            ImageSizeSpec::Percent(200),
            ImageSizeSpec::Auto,
            Some((100, 16)),
            8,
            16,
            80, // viewport_w_px = 640
            24,
            false,
        );
        // 200% of 640 = 1280px → 1280/8 = 160 cells (2x the 80-col grid).
        assert_eq!(cols, 160);
    }

    #[test]
    fn compute_cell_extent_width_past_viewport_still_produces_extent() {
        // `Cells(1000)` on an 80-col grid: the parser doesn't clamp;
        // the grid's off-grid check handles overflow downstream. Pinning
        // that the math passes the raw count through (so future fixes
        // happen in the right place — the grid, not here).
        let (_rows, cols) = compute_cell_extent(
            ImageSizeSpec::Cells(1000),
            ImageSizeSpec::Cells(1),
            Some((100, 100)),
            8,
            16,
            80,
            24,
            false,
        );
        assert_eq!(cols, 1000);
    }

    #[test]
    fn compute_cell_extent_zero_image_dims_with_preserve_does_not_panic() {
        // Some malformed payloads have peek_dimensions returning (0, 0).
        // The preserve_aspect branch divides by ih / iw — the `if iw > 0
        // && ih > 0` guard must hold or we crash on a zero image.
        let (rows, cols) = compute_cell_extent(
            ImageSizeSpec::Auto,
            ImageSizeSpec::Cells(5),
            Some((0, 0)),
            8,
            16,
            80,
            24,
            true,
        );
        // height resolves to 5 cells; width stays Auto with no source
        // (image dim 0 → resolve returns Some(0) → ceil-div to 1).
        assert_eq!(rows, 5);
        assert_eq!(cols, 1);
    }

    #[test]
    fn compute_cell_extent_pixel_inputs_well_above_u16_clamp_to_u16_max() {
        // Pixel inputs that produce far more cells than u16 can hold must
        // clamp at u16::MAX rather than truncate. Stay below the
        // ceil-divide overflow threshold (see the should_panic test below)
        // — pick a value that still produces > 65_535 cells but doesn't
        // overflow `px + cell - 1` in the divisor path.
        let huge = 1_000_000_000u32; // 1B px / 8px-cell = 125M cells → clamps.
        let (rows, cols) = compute_cell_extent(
            ImageSizeSpec::Pixels(huge),
            ImageSizeSpec::Pixels(huge),
            None,
            8,
            16,
            80,
            24,
            false,
        );
        assert_eq!(rows, u16::MAX);
        assert_eq!(cols, u16::MAX);
    }

    /// A `Pixels(u32::MAX - 1)` spec used to overflow the `+ cell_w_px - 1`
    /// in the ceil-div (debug panic, release wrap). `saturating_add` makes
    /// the worst case clip cleanly to `u16::MAX` cells, which is the
    /// largest extent the renderer can represent.
    #[test]
    fn compute_cell_extent_pixels_near_u32_max_clamps_instead_of_overflowing() {
        let (rows, cols) = compute_cell_extent(
            ImageSizeSpec::Pixels(u32::MAX - 1),
            ImageSizeSpec::Pixels(u32::MAX - 1),
            None,
            8,
            16,
            80,
            24,
            false,
        );
        assert_eq!(rows, u16::MAX);
        assert_eq!(cols, u16::MAX);
    }

    #[test]
    fn set_cell_size_px_zero_clamps_to_one() {
        // Caller may pass 0 during a degenerate resize (e.g. window
        // minimized to a 0-px height); the OSC math would divide by zero.
        // The clamp turns that into a tiny-but-valid cell size.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(0, 0);
        // 100% percent against 80 cells of 1px each = 80 cells.
        t.feed(&iterm_osc(";width=100%;height=100%"));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        // With cell_w_px = 1 (clamped from 0), 100% of viewport = 80 cells.
        assert_eq!(uploads[0].cell_extent.1, 80);
    }

    #[test]
    fn set_cell_size_px_change_between_oscs_uses_active_value() {
        // Each OSC computes cell_extent from the cell size active at
        // parse time — a font-size change between OSCs must not retro-
        // active the prior upload.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        t.feed(&iterm_osc(";width=16px;height=16px"));
        // Bump cell size — second OSC should use the new value.
        t.set_cell_size_px(16, 32);
        t.feed(&iterm_osc(";width=16px;height=16px"));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 2);
        // First OSC: 16px / 8px-cell = 2 cols; 16px / 16px-line = 1 row.
        assert_eq!(uploads[0].cell_extent, (1, 2));
        // Second OSC: 16px / 16px-cell = 1 col; 16px / 32px-line = 1 row
        // (ceil-div of 16/32 floors to 0 then `.max(1)` brings it to 1).
        assert_eq!(uploads[1].cell_extent, (1, 1));
    }

    #[test]
    fn osc_1337_many_unknown_keys_with_known_mixed() {
        // Defensive parser: an arbitrary salad of unknown keys mixed with
        // known ones should still yield a clean upload with the known
        // values respected. Real iTerm payloads include `size=`, `type=`,
        // and others we don't model — accept-and-ignore.
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&iterm_osc(
            ";size=12345;type=image/png;futureA=1;width=7;futureB=2;height=3;futureC=hello;preserveAspectRatio=0",
        ));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        assert_eq!(uploads[0].width, ImageSizeSpec::Cells(7));
        assert_eq!(uploads[0].height, ImageSizeSpec::Cells(3));
        assert!(!uploads[0].preserve_aspect);
    }

    #[test]
    fn osc_1337_name_with_invalid_base64_leaves_label_none() {
        // `name=` is best-effort: an invalid base64 value must not abort
        // the whole OSC; the upload still arrives, just with label None.
        let mut t = Terminal::new(80, 24, 100);
        // `iterm_osc` already includes `inline=1`; append a bogus name.
        t.feed(&iterm_osc(";name=!!!notbase64!!!"));
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1, "bad name must not drop the OSC");
        assert!(uploads[0].label.is_none());
    }

    #[test]
    fn osc_1337_split_across_feed_calls_still_parses() {
        // The ANSI parser is stateful per-char. An OSC arriving in two
        // (or more) feed() chunks should still produce exactly one
        // upload — the parser accumulates the OSC body until ST/BEL.
        let mut t = Terminal::new(80, 24, 100);
        let osc = iterm_osc(";width=4");
        let mid = osc.len() / 2;
        t.feed(&osc[..mid]);
        // No terminator yet — queue must be empty.
        assert!(t.take_pending_image_uploads().is_empty());
        t.feed(&osc[mid..]);
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1, "chunked OSC should reassemble into one upload");
        assert_eq!(uploads[0].width, ImageSizeSpec::Cells(4));
    }

    #[test]
    fn osc_1337_strips_internal_whitespace_in_base64() {
        // Real iTerm callers wrap base64 at 76 chars. The handler strips
        // ASCII whitespace before decode. Test it at the terminal level
        // (the e2e test in images.rs covers the GPU path too).
        let mut t = Terminal::new(80, 24, 100);
        use base64::Engine;
        let img = image::RgbaImage::from_pixel(2, 2, image::Rgba([0, 0, 0, 255]));
        let mut png: Vec<u8> = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageOutputFormat::Png)
            .unwrap();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        // Sprinkle tabs, spaces, and newlines through the payload.
        let mut wrapped = String::new();
        for (i, ch) in b64.chars().enumerate() {
            if i > 0 && i % 4 == 0 {
                wrapped.push_str("\n\t ");
            }
            wrapped.push(ch);
        }
        let osc = format!("\x1b]1337;File=inline=1:{}\x07", wrapped);
        t.feed(&osc);
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1, "whitespace in base64 must be stripped, not fail decode");
        assert_eq!(uploads[0].pixel_size, Some((2, 2)));
    }

    #[test]
    fn remove_placements_with_image_drops_multiple_placements_in_one_grid() {
        // Same image placed 3 times on a single grid (e.g. an app re-using
        // the same texture). remove_with_image must drop all 3, not just
        // the first match.
        let mut t = Terminal::new(20, 10, 100);
        place(&mut t, 5, 0, 0, 1, 2);
        place(&mut t, 5, 2, 0, 1, 2);
        place(&mut t, 5, 4, 0, 1, 2);
        place(&mut t, 6, 6, 0, 1, 2); // different image — must survive
        assert_eq!(t.live_placements().len(), 4);
        let removed = t.remove_placements_with_image(ImageId(5));
        assert_eq!(removed, 3);
        let surviving: Vec<u32> = t.live_placements().iter().map(|p| p.image.0).collect();
        assert_eq!(surviving, vec![6]);
    }

    #[test]
    fn osc_1337_in_decstbm_scroll_region_anchor_follows_scrolls() {
        // The anchor-compensation math uses cursor row-delta to count
        // scrolls, which should stay robust when a DECSTBM scroll region
        // is active. Cursor at the bottom of a [10..20] region: an image
        // taller than the remaining rows triggers in-region scrolling
        // rather than full-grid scrolling. The anchor must still resolve
        // to the original visual row.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        // DECSTBM: top=10, bottom=20 (1-based). After this, scroll_top=9,
        // scroll_bottom=19. CUP also moves cursor to home of region.
        t.feed("\x1b[10;20r");
        // Move cursor to bottom of region: row 20 (1-based) = index 19.
        t.feed("\x1b[20;1H");
        // Image with 3 rows — all 3 line-feeds at scroll_bottom will scroll
        // the in-region rows up. Cursor stays at row 19.
        t.feed(&iterm_osc(";width=4;height=3"));
        assert_eq!(t.cursor().row, 19, "cursor pinned at scroll_bottom");
        let uploads = t.take_pending_image_uploads();
        assert_eq!(uploads.len(), 1);
        // 3 line-feeds, 0 cursor advance → scrolls = 3 - 0 = 3. Anchor =
        // original_row 19 - 3 = 16.
        assert_eq!(uploads[0].cell_anchor, (16, 0));
        assert_eq!(uploads[0].cell_extent, (3, 4));
    }

    #[test]
    fn alt_screen_image_isolated_from_primary_referenced_ids_reflects_both() {
        // Per-grid placements + a global referenced_image_ids() union:
        // live_placements() must respect the active screen, while
        // retain()-input must keep both grids' images alive across an
        // alt-screen flip.
        let mut t = Terminal::new(40, 10, 100);
        place(&mut t, 100, 0, 0, 1, 2);
        // Switching to alt clears alt grid (matches text behaviour).
        t.feed("\x1b[?1049h");
        assert!(t.live_placements().is_empty(), "alt is fresh");
        place(&mut t, 200, 0, 0, 1, 2);
        let live_ids_on_alt: Vec<u32> =
            t.live_placements().iter().map(|p| p.image.0).collect();
        assert_eq!(live_ids_on_alt, vec![200]);

        // referenced_image_ids unions BOTH grids — primary's image survives
        // for retain() purposes even while alt is active, so the texture
        // isn't dropped + re-uploaded on every screen flip.
        let refs: std::collections::HashSet<u32> =
            t.referenced_image_ids().iter().map(|i| i.0).collect();
        assert!(refs.contains(&100));
        assert!(refs.contains(&200));

        // Back to primary: alt's image survives in the alt grid (switching
        // back doesn't clear alt — only switching TO alt does).
        t.feed("\x1b[?1049l");
        let live_ids_back: Vec<u32> =
            t.live_placements().iter().map(|p| p.image.0).collect();
        assert_eq!(live_ids_back, vec![100]);
        let refs_back: std::collections::HashSet<u32> =
            t.referenced_image_ids().iter().map(|i| i.0).collect();
        assert!(refs_back.contains(&100));
        assert!(refs_back.contains(&200));
    }

    #[test]
    fn keep_placements_in_scrollback_toggle_cycle_does_not_refill_queue() {
        // Disable drops the queue; re-enable should NOT magically reinstate
        // previously dropped entries (we have nothing to reinstate from).
        // Pins the documented one-way semantics so a future change that
        // tries to be clever about retention doesn't silently resurrect
        // entries.
        let mut t = Terminal::new(20, 5, 100);
        place(&mut t, 1, 0, 0, 1, 2);
        t.feed("\x1b[1S");
        assert_eq!(t.scrollback_placements_for_test().len(), 1);
        t.set_keep_placements_in_scrollback(false);
        assert!(t.scrollback_placements_for_test().is_empty());
        t.set_keep_placements_in_scrollback(true);
        // Re-enable doesn't restore — the dropped entry is gone for good.
        assert!(t.scrollback_placements_for_test().is_empty());
        // New scrolls now repopulate as expected.
        place(&mut t, 2, 0, 0, 1, 2);
        t.feed("\x1b[1S");
        assert_eq!(t.scrollback_placements_for_test().len(), 1);
    }

    //
    // Gap-fill: KittyAction::Other, handle_apc_delete selector edge cases,
    // register_kitty_image_id idempotence, placeholder bbox corners,
    // decode_kitty_placeholder_image_id boundary values, normalize/inflate
    // edge cases, kitty_placement_params interactions, and capability-vs-
    // dispatch parity. Append-only; reuses kitty_apc / kitty_apc_control_only
    // / kitty_png / placeholder_sgr_fg from above.
    //

    #[test]
    fn kitty_apc_unknown_action_value_drops_silently() {
        // `a=a` (animation, unimplemented) parses to KittyAction::Other.
        // handle_apc returns early before any transmission work — no
        // upload queued, no response emitted, no panic.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        let png = kitty_png(2, 2);
        t.feed(&kitty_apc("a=a,f=100,c=1,r=1", &png));
        assert!(t.take_pending_image_uploads().is_empty());
        assert!(t.take_response().is_empty());
        // Cursor untouched too.
        assert_eq!(t.cursor().row, 0);
        assert_eq!(t.cursor().col, 0);
    }

    #[test]
    fn kitty_apc_a_d_d_a_with_no_kitty_placements_is_noop() {
        // d=a sweeps Kitty placements but leaves iTerm placements alone.
        // With only an iTerm-style placement present (no kitty_image_id),
        // d=a must not touch it and must not panic.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        t.insert_placement(ImageId(1), 0, 0, 1, 1, 0);
        assert_eq!(t.live_placements().len(), 1);
        t.feed(&kitty_apc_control_only("a=d,d=a"));
        assert_eq!(t.live_placements().len(), 1, "iTerm placement survives");
        assert!(t.live_placements()[0].kitty_image_id.is_none());
    }

    #[test]
    fn kitty_apc_a_d_d_i_without_image_id_is_noop() {
        // d=i with no `i=` returns early — nothing to look up. Pin the
        // current behavior: no panic, no spurious placement removal.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        t.register_kitty_image_id(7, ImageId(99));
        t.feed(&kitty_apc_control_only("a=p,i=7,c=1,r=1"));
        assert_eq!(t.live_placements().len(), 1);
        t.feed(&kitty_apc_control_only("a=d,d=i")); // no i=
        assert_eq!(t.live_placements().len(), 1, "missing i= → no-op");
        assert_eq!(t.kitty_image_id_lookup(7), Some(ImageId(99)));
    }

    #[test]
    fn kitty_apc_a_d_d_p_without_placement_id_is_noop() {
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        t.register_kitty_image_id(7, ImageId(99));
        t.feed(&kitty_apc_control_only("a=p,i=7,p=1,c=1,r=1"));
        assert_eq!(t.live_placements().len(), 1);
        t.feed(&kitty_apc_control_only("a=d,d=p")); // no p=
        assert_eq!(t.live_placements().len(), 1, "missing p= → no-op");
    }

    #[test]
    fn kitty_apc_a_d_unknown_selector_drops_silently() {
        // `d=q` (or any unknown selector) parses to KittyDeleteSelector::Other,
        // which is a documented drop. Live placements survive.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        t.register_kitty_image_id(7, ImageId(99));
        t.feed(&kitty_apc_control_only("a=p,i=7,c=1,r=1"));
        assert_eq!(t.live_placements().len(), 1);
        t.feed(&kitty_apc_control_only("a=d,d=q,i=7"));
        assert_eq!(t.live_placements().len(), 1, "unknown selector → drop, no-op");
    }

    #[test]
    fn kitty_apc_a_d_d_i_repeated_second_call_is_noop() {
        // First d=i removes both the placements AND the id mapping. A
        // second d=i for the same id finds nothing to do — pin that it
        // doesn't crash on the missing lookup.
        let mut t = Terminal::new(80, 24, 100);
        t.set_cell_size_px(8, 16);
        t.register_kitty_image_id(7, ImageId(99));
        t.feed(&kitty_apc_control_only("a=p,i=7,c=1,r=1"));
        t.feed(&kitty_apc_control_only("a=d,d=i,i=7"));
        assert!(t.live_placements().is_empty());
        assert!(t.kitty_image_id_lookup(7).is_none());
        // Second call — image is already gone.
        t.feed(&kitty_apc_control_only("a=d,d=i,i=7"));
        assert!(t.live_placements().is_empty());
    }

    #[test]
    fn kitty_apc_register_same_store_id_is_idempotent() {
        // Re-registering with the SAME (client_id, store_id) pair must
        // leave the map unchanged — both the lookup and referenced_image_ids
        // still point at the same one entry. (The "new store id overwrites"
        // case is already covered; this pins the no-op branch.)
        let mut t = Terminal::new(80, 24, 100);
        t.register_kitty_image_id(7, ImageId(99));
        t.register_kitty_image_id(7, ImageId(99));
        t.register_kitty_image_id(7, ImageId(99));
        assert_eq!(t.kitty_image_id_lookup(7), Some(ImageId(99)));
        let refs = t.referenced_image_ids();
        assert!(refs.contains(&ImageId(99)));
        assert_eq!(refs.len(), 1, "single mapping → single referenced id");
    }

    #[test]
    fn placeholder_runs_single_cell_has_extent_one() {
        // One placeholder cell at (4, 4) → one run with one-cell
        // extent. Boundary check for the end = start + 1 math.
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&placeholder_sgr_fg(123));
        t.feed("\x1b[5;5H"); // row 4 col 4 (0-based)
        t.feed("\u{10EEEE}\u{0305}\u{0305}");
        let runs = t.kitty_placeholder_runs();
        assert_eq!(runs.len(), 1);
        let r = &runs[0];
        assert_eq!(r.client_id, 123);
        assert_eq!(r.screen_row, 4);
        assert_eq!((r.screen_col_start, r.screen_col_end), (4, 5));
        assert_eq!((r.image_col_start, r.image_col_end), (0, 1));
    }

    #[test]
    fn placeholder_runs_at_origin_handles_zero_indices() {
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&placeholder_sgr_fg(5));
        t.feed("\u{10EEEE}\u{0305}\u{0305}");
        let runs = t.kitty_placeholder_runs();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].screen_row, 0);
        assert_eq!(runs[0].screen_col_start, 0);
    }

    #[test]
    fn placeholder_runs_scan_respects_active_grid_alt_screen() {
        // Placeholders on the primary screen must not appear in
        // the scan after switching to the alt grid.
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&placeholder_sgr_fg(11));
        t.feed("\u{10EEEE}\u{0305}\u{0305}");
        assert_eq!(t.kitty_placeholder_runs().len(), 1);
        t.feed("\x1b[?1049h"); // enter alt screen — fresh empty grid
        assert!(
            t.kitty_placeholder_runs().is_empty(),
            "primary placeholders must not bleed into alt scan",
        );
        // And primary's still intact after returning.
        t.feed("\x1b[?1049l");
        assert_eq!(t.kitty_placeholder_runs().len(), 1);
    }

    #[test]
    fn placeholder_runs_same_id_with_gap_produces_two_runs() {
        // Same-id placeholders separated by non-placeholder cells
        // must produce TWO runs, NOT one merged bbox. The previous
        // bbox API merged them into one stretched rect — exactly
        // the distortion the per-cell rendering was designed to
        // fix.
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&placeholder_sgr_fg(42));
        t.feed("\x1b[1;1H");
        t.feed("\u{10EEEE}\u{0305}\u{0305}");
        // Gap cell with non-placeholder content somewhere later.
        t.feed("\x1b[3;5Hx");
        // Another placeholder of the same id.
        t.feed(&placeholder_sgr_fg(42));
        t.feed("\x1b[5;10H");
        t.feed("\u{10EEEE}\u{0305}\u{0305}");
        let runs = t.kitty_placeholder_runs();
        assert_eq!(runs.len(), 2, "disjoint same-id placeholders → two runs");
    }

    #[test]
    fn placeholder_runs_zero_id_cells_excluded_from_scan() {
        // (0,0,0) fg encodes id 0, which is the "no id" sentinel.
        // Those cells must NOT appear in any run.
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&placeholder_sgr_fg(7));
        t.feed("\u{10EEEE}\u{0305}\u{0305}");
        // Sentinel placeholder at (2,3) with rgb(0,0,0).
        t.feed("\x1b[3;4H");
        t.feed("\x1b[38;2;0;0;0m");
        t.feed("\u{10EEEE}\u{0305}\u{0305}");
        let runs = t.kitty_placeholder_runs();
        assert_eq!(runs.len(), 1, "only the id=7 placeholder appears");
        assert_eq!(runs[0].client_id, 7);
    }

    #[test]
    fn decode_kitty_placeholder_image_id_round_trips_max_24_bit() {
        // 0xFFFFFF (= 16_777_215) is the largest id encodable in 24 bits;
        // round-trips through SGR truecolor + sRGB linearization without loss.
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&placeholder_sgr_fg(0xFFFFFF));
        t.feed("\u{10EEEE}");
        let cell = t.extended_cell(0, 0).unwrap();
        assert_eq!(cell.placeholder_image_id, Some(0xFFFFFF));
    }

    #[test]
    fn decode_kitty_placeholder_image_id_round_trips_one() {
        // Smallest non-sentinel id — the bit just above 0.
        let mut t = Terminal::new(80, 24, 100);
        t.feed(&placeholder_sgr_fg(1));
        t.feed("\u{10EEEE}");
        let cell = t.extended_cell(0, 0).unwrap();
        assert_eq!(cell.placeholder_image_id, Some(1));
    }

    #[test]
    fn decode_kitty_placeholder_image_id_ignores_alpha_channel() {
        // The encoder packs id into RGB only; the alpha component must
        // not perturb the result. Build a Style directly so we can poke
        // an arbitrary alpha without going through the SGR parser.
        let mut style = crate::style::Style::new();
        // 0xA1B2C3 in sRGB, then linearize (matches what the parser stores).
        let r = crate::palette::srgb_to_linear(0xA1);
        let g = crate::palette::srgb_to_linear(0xB2);
        let b = crate::palette::srgb_to_linear(0xC3);
        style.color_fg = Some([r, g, b, 0.25]); // weird alpha
        assert_eq!(decode_kitty_placeholder_image_id(&style), Some(0xA1B2C3));
        // And alpha=0 — still extracted from RGB.
        style.color_fg = Some([r, g, b, 0.0]);
        assert_eq!(decode_kitty_placeholder_image_id(&style), Some(0xA1B2C3));
    }

    #[test]
    fn normalize_kitty_payload_raw_rgb_exact_size_passes_through() {
        // Raw RGB w*h*3 bytes exactly — must not be truncated to nothing.
        // The K3 macOS fix accepts oversize buffers (page-padded SHM); pin
        // that exact-size still works and decodes back to declared dims.
        let w = 4u32;
        let h = 4u32;
        let raw: Vec<u8> = (0..(w * h * 3) as u8).collect();
        let out = normalize_kitty_payload(KittyFormat::Rgb, &raw, Some(w), Some(h));
        assert!(out.is_some());
        let (png, dims) = out.unwrap();
        assert_eq!(dims, Some((w, h)));
        // PNG signature.
        assert_eq!(&png[..4], &[0x89, b'P', b'N', b'G']);
    }

    #[test]
    fn normalize_kitty_payload_raw_rgba_too_few_bytes_rejected() {
        // 4×4 RGBA needs 64 bytes; supply 32 → reject (we never truncate
        // upward, only downward from oversize).
        let raw: Vec<u8> = vec![0u8; 32];
        let out = normalize_kitty_payload(KittyFormat::Rgba, &raw, Some(4), Some(4));
        assert!(out.is_none());
    }

    #[test]
    fn normalize_kitty_payload_raw_dims_overflow_returns_none() {
        // s * v * bpp must use checked_mul to guard against malicious
        // s=u32::MAX, v=u32::MAX overflow. Pin: returns None instead of
        // panicking or allocating gigabytes.
        let raw: Vec<u8> = vec![0u8; 8];
        let out = normalize_kitty_payload(
            KittyFormat::Rgb,
            &raw,
            Some(u32::MAX),
            Some(u32::MAX),
        );
        assert!(out.is_none(), "overflow in s*v*bpp must short-circuit");
    }

    #[test]
    fn inflate_kitty_zlib_empty_input_returns_empty_vec() {
        // Empty input: flate2's ZlibDecoder treats "0 bytes available
        // before any header" as a clean EOF and `read_to_end` returns
        // Ok(0). Pin current behavior — None would be defensible too, but
        // any change should be deliberate (a downstream caller might be
        // relying on the empty-Vec path to short-circuit cleanly).
        let out = inflate_kitty_zlib(&[]);
        assert_eq!(out.as_deref(), Some(&[][..]));
    }

    #[test]
    fn inflate_kitty_zlib_cap_rejects_huge_inflation() {
        // Compress 257 MiB of zeros → very small payload that inflates
        // past the 256 MiB cap. Pin that we reject (None) rather than
        // returning the gigabyte allocation.
        use std::io::Write;
        const TOO_BIG: usize = 256 * 1024 * 1024 + 1;
        let zeros = vec![0u8; TOO_BIG];
        let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(&zeros).unwrap();
        let compressed = enc.finish().unwrap();
        // Sanity: the compressed payload is tiny (a few hundred KB tops);
        // we're feeding it through inflate, NOT keeping `zeros` around
        // during the inflate call itself (so the test doesn't double-RAM).
        drop(zeros);
        let out = inflate_kitty_zlib(&compressed);
        assert!(out.is_none(), "inflated size > 256 MiB cap must reject");
    }

    #[test]
    fn inflate_kitty_zlib_concatenated_streams_returns_only_first() {
        // Two zlib streams back-to-back — the decoder reads the first
        // and stops at its end-of-stream marker; the second is ignored.
        // Pin current behavior so a future swap to a multi-stream
        // decoder is a conscious decision.
        use std::io::Write;
        let mut enc1 = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        enc1.write_all(b"first").unwrap();
        let s1 = enc1.finish().unwrap();
        let mut enc2 = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        enc2.write_all(b"SECOND").unwrap();
        let s2 = enc2.finish().unwrap();
        let mut combined = s1.clone();
        combined.extend_from_slice(&s2);
        let out = inflate_kitty_zlib(&combined).expect("first stream decodes");
        assert_eq!(out, b"first", "only the first stream is read");
    }

    #[test]
    fn kitty_placement_params_explicit_zero_offset_is_some_zero() {
        // X=0 / Y=0 explicit MUST round-trip to (0, 0) — not None. Apps
        // use explicit zero to anchor at the top-left of the cell after
        // a previous non-zero offset.
        let mut ctrl = KittyControl::default();
        ctrl.pixel_offset_x = Some(0);
        ctrl.pixel_offset_y = Some(0);
        let (offset, _, _) = kitty_placement_params(&ctrl);
        assert_eq!(offset, (0, 0));
    }

    #[test]
    fn kitty_placement_params_z_min_value_parses_cleanly() {
        // i32::MIN is a valid z per the spec (signed). Pin that nothing
        // narrows or saturates it on the way through.
        let mut ctrl = KittyControl::default();
        ctrl.z_index = Some(i32::MIN);
        let (_, z, _) = kitty_placement_params(&ctrl);
        assert_eq!(z, i32::MIN);
    }

    #[test]
    fn kitty_placement_params_xy_without_wh_yields_no_crop() {
        // Source crop requires all four; x= + y= alone are insufficient.
        // The match `(Some, Some, Some, Some)` fails → None.
        let mut ctrl = KittyControl::default();
        ctrl.crop_x = Some(1);
        ctrl.crop_y = Some(2);
        let (_, _, src) = kitty_placement_params(&ctrl);
        assert!(src.is_none());
    }

    #[test]
    fn kitty_query_supported_matches_dispatch_for_every_format_transmission_tuple() {
        // The query-reply path and the dispatch path BOTH consult
        // `kitty_query_supported`. If the dispatch ever forks (e.g. drops
        // a tuple the query says OK to), apps see silent decode failures.
        //
        // Walk every (format, transmission) combo and assert: when the
        // query says OK, dispatch produces an upload OR a real side-effect
        // (the file/SHM-based paths can fail on the empty payload but
        // never on the format/transmission check itself). When the query
        // says ENOTSUPPORTED, dispatch produces no upload and no response.
        //
        // The "OK matches dispatch" half is easiest to verify positively
        // for direct base64 paths; file/temp/SHM need real artifacts to
        // succeed. So this test covers the ENOTSUPPORTED → silent-drop
        // half exhaustively, and the OK half for the direct paths.
        let formats = [
            (KittyFormat::Png, "100"),
            (KittyFormat::Rgb, "24"),
            (KittyFormat::Rgba, "32"),
        ];
        let transmissions = [
            (KittyTransmission::Direct, "d"),
            (KittyTransmission::File, "f"),
            (KittyTransmission::TempFile, "t"),
            (KittyTransmission::SharedMemory, "s"),
        ];
        let t = Terminal::new(80, 24, 100);
        for (fmt_enum, fmt_str) in &formats {
            for (tx_enum, tx_str) in &transmissions {
                let supported = t.kitty_query_supported(*fmt_enum, *tx_enum);
                // Issue the query and pin the reply prefix.
                let mut q = Terminal::new(80, 24, 100);
                q.feed(&kitty_apc_control_only(&format!(
                    "a=q,i=1,f={},t={},s=1,v=1",
                    fmt_str, tx_str,
                )));
                let reply = q.take_response();
                let s = std::str::from_utf8(&reply).unwrap();
                if supported {
                    assert!(
                        s.starts_with("\x1b_Gi=1;OK"),
                        "(f={}, t={}) query said OK but got: {:?}",
                        fmt_str, tx_str, s,
                    );
                } else {
                    assert!(
                        s.starts_with("\x1b_Gi=1;ENOTSUPPORTED"),
                        "(f={}, t={}) query said unsupported but got: {:?}",
                        fmt_str, tx_str, s,
                    );
                    // And dispatch on an unsupported tuple must drop —
                    // no upload from a one-shot APC. (Direct-base64 only;
                    // the file/SHM paths would fail upstream on missing
                    // artifact anyway, so testing dispatch parity for
                    // them adds no signal beyond the query reply.)
                    let mut d = Terminal::new(80, 24, 100);
                    d.set_cell_size_px(8, 16);
                    let png = kitty_png(2, 2);
                    d.feed(&kitty_apc(
                        &format!("a=T,f={},t={},s=2,v=2,c=1,r=1", fmt_str, tx_str),
                        &png,
                    ));
                    assert!(
                        d.take_pending_image_uploads().is_empty(),
                        "(f={}, t={}) unsupported but dispatch produced an upload",
                        fmt_str, tx_str,
                    );
                }
            }
        }
    }
}
