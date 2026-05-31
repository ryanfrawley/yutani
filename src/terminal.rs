//! The terminal data model: the grid/cell ring buffer ([`Grid`]), scrollback,
//! the [`Cursor`], scroll-region state, the scroll-region + semantic-mark
//! migration nucleus (`scroll_region_*`, `scroll_marks_*`, the `evict_*`
//! helpers), the placement store, the central OSC dispatcher, window-title
//! (OSC 0/2) and hyperlink (OSC 8) handling, and the `reply` response buffer
//! all live here. The feature-specific handlers split into submodules:
//!
//! - [`vt`] — VT/CSI/SGR execution: the `dispatch` match and its arm handlers
//!   (printing, cursor movement, erase, line/character edit ops, margins, DEC
//!   private modes, DCS, device-status replies, screen/cursor state, reset).
//! - [`image_protocol`] — the inline-image protocols (iTerm2 `OSC 1337 File=`
//!   and the full Kitty graphics protocol).
//! - [`shell_integration`] — cwd (OSC 7), FinalTerm semantic prompt marks
//!   (OSC 133), the yutani-private OSC 2122/2124/2125 (current input,
//!   history-file, live preview), and the prompt/command-region navigation
//!   built on the marks they record.
//!
//! All three submodules hold `impl Terminal` blocks. The shared types and pure
//! free functions stay in this file (the [`Terminal`] struct owns the state
//! fields) and the submodules reach them via `use super::*`; the handlers the
//! parent module, sibling submodules, or tests still call are `pub(super)`.

use crate::ansi::{self, Event};
use crate::images::ImageId;
use crate::style::{Cell, Style};
use std::collections::VecDeque;

mod image_protocol;
mod shell_integration;
mod vt;
mod grid;
pub(crate) use grid::Grid;

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
    /// Visual row coordinate (`extended_cell`'s frame of reference):
    /// 0..rows is the live grid, negative values are scrollback rows
    /// pulled into view, and `rows..rows+2` are the bottom phantom
    /// strip used during smooth scroll. The renderer plugs this
    /// directly into the cell row→pixel math — no `view_offset`
    /// shift needed.
    pub screen_row: isize,
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

/// An alt-screen scroll that just happened, reported to the front end so it
/// can animate the slide. `rows` is the net distance in cells; `up` is the
/// direction (content moved up, the common pager-forward case). `region_top`/
/// `region_bottom` are the scroll region it happened in (0-based, inclusive) —
/// often not the full height because apps reserve a status line. The captured
/// departing rows live on the `Terminal` (see `alt_anim_departing`) so the
/// renderer can draw them in the phantom band during the slide.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct AltScroll {
    pub up: bool,
    pub rows: usize,
    pub region_top: usize,
    pub region_bottom: usize,
}

/// Frozen departing rows for an in-flight scroll animation. `up` is the scroll
/// direction; `edge_row` is the visual row where the band begins (see
/// `alt_anim_departing`); `rows` are the cell rows, top-to-bottom.
#[derive(Clone, Debug)]
pub struct AltAnimRows {
    pub up: bool,
    pub edge_row: isize,
    pub rows: Vec<Vec<Cell>>,
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
    // OSC 8 active hyperlink. Cells printed while this is `Some` carry the
    // id (see `Cell::hyperlink`). Saved/restored with the cursor (DECSC/DECRC)
    // for free since `Cursor` is copied wholesale; reset by a full terminal
    // reset. Lives on the cursor, not `Style`, so SGR resets don't drop it.
    pub hyperlink: Option<std::num::NonZeroU32>,
}

impl Cursor {
    pub fn new() -> Self {
        Self {
            row: 0,
            col: 0,
            style: Style::new(),
            wrap_pending: false,
            hyperlink: None,
        }
    }
}

/// A semantic mark from the OSC 133 shell-integration protocol (FinalTerm).
/// The shell's prompt hook emits these to delimit prompt, user-input, and
/// command-output regions on the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SemanticMarkKind {
    /// `OSC 133 ; A` — a fresh prompt is about to be drawn.
    PromptStart,
    /// `OSC 133 ; B` — end of prompt / start of the user-typed command.
    InputStart,
    /// `OSC 133 ; C` — the user pressed enter; command output follows.
    OutputStart,
    /// `OSC 133 ; D [; <exit>]` — the command finished, with its optional
    /// exit code.
    CommandEnd { exit: Option<i32> },
}

/// Where a [`SemanticMark`] is anchored. A mark starts `Live` on a grid row;
/// when that row scrolls off the top of the primary grid it converts to
/// `Scrollback`, anchored to a scrollback row index the same way
/// [`ScrollbackPlacement`] is. Both domains stay in one emission-ordered list
/// (rather than two collections like placements) so `command_regions` can
/// fold them in the order the shell emitted them without re-sorting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MarkAnchor {
    /// Row on the live primary grid.
    Live { row: usize },
    /// Index into `scrollback` (0 = oldest), decremented on front eviction —
    /// the same bookkeeping as `ScrollbackPlacement::scrollback_row`.
    Scrollback { row: isize },
}

/// A semantic mark and its anchor. Marks live on the primary screen only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SemanticMark {
    anchor: MarkAnchor,
    /// Cursor column when the mark was emitted (anchor for column-aware
    /// features like an autocomplete overlay; not read by region queries).
    col: usize,
    kind: SemanticMarkKind,
}

/// One shell command's screen region, assembled from the A/B/C/D marks by
/// [`Terminal::command_regions`]. Line fields are absolute line indices in
/// the same space as [`Terminal::line_at`] (`0..scrollback_len` is
/// scrollback, `scrollback_len + r` is live grid row `r`), computed at
/// query time so they stay viewport-consistent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandRegion {
    pub prompt_start: isize,
    pub input_start: Option<isize>,
    pub output_start: Option<isize>,
    pub command_end: Option<isize>,
    pub exit_code: Option<i32>,
}

/// Status shown in the prompt gutter for a command region, derived from its
/// reported exit code. Drives the gutter-bar color in the renderer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptStatus {
    /// Command finished with exit code 0.
    Success,
    /// Command finished with a non-zero exit code.
    Failure,
    /// No completed-with-code command for this prompt yet — still running,
    /// interrupted, or the shell reported `D` without an exit code.
    Pending,
}

/// The shell's live interactive input line, reported via the yutani-private
/// `OSC 2122` extension (see [`Terminal::handle_osc_2122`]). Populated only
/// while the shell's line editor is active and cleared when a command is
/// submitted, so `Some` means "the user is editing a command line right now".
/// This is the foundation the autocomplete UI builds on: it gives the live
/// buffer + cursor without reconstructing them from the grid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurrentInput {
    /// The full edit buffer (zsh `$BUFFER`), decoded from the base64 payload.
    /// May be empty (the user cleared the line) — that is distinct from no
    /// active edit line at all, which is represented by `None` on the field.
    pub buffer: String,
    /// Cursor position as a *character* (code point) offset into `buffer`, as
    /// reported by the shell (zsh `$CURSOR`), clamped to `buffer.chars().count()`.
    /// Character-, not byte-, indexed because that is what the shell reports;
    /// consumers that need a byte offset convert via `buffer.char_indices()`.
    pub cursor: usize,
}

/// A live-preview action requested over the yutani-private `OSC 2125` extension
/// (see [`Terminal::handle_osc_2125`]). Emitted by the first-run onboarding to
/// drive the real renderer as the user chooses; the front end applies each
/// transiently, persisting nothing until the onboarding writes config and sends
/// [`PreviewRequest::Reload`]. (Not `Eq`: `FontSize` carries an `f32`.)
#[derive(Debug, Clone, PartialEq)]
pub enum PreviewRequest {
    /// Preview a color scheme by name; `None` = the built-in default palette.
    Scheme(Option<String>),
    /// Preview a CRT-glow preset.
    Glow(crate::GlowLevel),
    /// Toggle the scanline overlay.
    Scanlines(bool),
    /// Preview a combined CRT effect (bloom + scanlines together).
    Crt(crate::CrtLevel),
    /// Preview an absolute font size in points.
    FontSize(f32),
    /// Re-read config + scheme from disk — the commit step once the onboarding
    /// has saved the chosen settings.
    Reload,
}

/// Kitty graphics-protocol state, bundled off [`Terminal`] so the seven
/// related fields move as a unit. The protocol handlers that own this state
/// live in the [`image_protocol`] submodule and reach it via `self.kitty`.
#[derive(Default)]
struct KittyImageState {
    // Kitty graphics chunked transmissions in flight. Keyed by `i=`
    // image id; chunks with `m=1` append, chunk with `m=0` (or omitted)
    // completes the upload. Only the FIRST chunk's sizing / cursor
    // params are kept — that's what the Kitty spec says wins.
    chunks: std::collections::HashMap<u32, KittyChunks>,
    // Chunked transmission without an image_id. The Kitty spec says
    // chunked transmissions MUST use `i=`, but `kitty +kitten icat`
    // doesn't in practice — when sending raw RGB/JPG it omits the id
    // and expects the terminal to thread the chunks together as a
    // singular anonymous in-flight image. Only one can be in flight at
    // a time; a new `m=1` without an id while one's open silently
    // overwrites (matching the implicit "only one anonymous stream"
    // contract).
    chunks_anon: Option<KittyChunks>,
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
    last_image_id: Option<u32>,
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
    image_ids: std::collections::HashMap<u32, ImageId>,
    // Format the base image was transmitted with, keyed by the same
    // client id used in `kitty_image_ids`. Animation frames (`a=f`)
    // commonly omit `f=` and expect the base's format to apply (icat
    // sends e.g. `f=24` on the base, then a=f frames with no `f=` at
    // all — the spec says raw frame data inherits the base format).
    // Populated whenever a Kitty transmission with a client id
    // finalizes; cleared on `a=d` selectors that drop the image.
    image_formats: std::collections::HashMap<u32, KittyFormat>,
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
    image_cell_extents: std::collections::HashMap<u32, (u32, u32)>,
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
    // Alternate scroll (?1007). When set, and the alt screen is active with no
    // mouse tracking in effect, the front end turns wheel motion into cursor-key
    // presses so pagers (less, man) scroll. Defaults on, matching xterm's
    // `alternateScroll` resource and iTerm2/kitty; apps disable it via ?1007l.
    alternate_scroll: bool,
    // Alt-screen smooth-scroll animation capture. When a full-width scroll
    // happens on the alt screen (SU / line-feed at the bottom, SD, or RI at the
    // top), we snapshot the pre-scroll frame and accumulate the net row delta
    // over the current `feed`, then hand it to the front end via
    // `take_alt_scroll`. `snapshot` is taken once, at the first qualifying
    // scroll of the feed (so it captures the frame just before any scrolling);
    // `net` is +up / -down; `region` is the consistent [top, bottom] the scroll
    // happened in (apps reserve a status line, so it's often not full-height);
    // `poison` aborts the window when a partial-width, region-changing, or
    // mixed-direction scroll makes the clean uniform-shift assumption invalid.
    alt_scroll_snapshot: Option<Vec<Vec<Cell>>>,
    alt_scroll_net: isize,
    alt_scroll_region: Option<(usize, usize)>,
    alt_scroll_poison: bool,
    // The rows that scrolled off, frozen for the duration of the front-end
    // animation. `extended_cell` serves these in the phantom band so the slide
    // shows real departing content instead of a blank stripe. `edge_row` is the
    // visual row where the departing band begins: for an upward scroll it's the
    // region top (rows sit just above, at edge_row-1 .. edge_row-d); for a
    // downward scroll it's region_bottom+1 (rows sit at edge_row .. edge_row+d-1).
    alt_anim_departing: Option<AltAnimRows>,
    // Net rows the *primary* screen scrolled into scrollback during the
    // current `feed` (each new line of output pushes one). Unlike the alt
    // path this needs no snapshot or poison flag: the departing rows are real
    // scrollback the renderer already draws via the phantom band when
    // `scroll_y > 0`, so the front end only needs the count. Drained by
    // `take_primary_scroll` to drive a smooth scroll-on-output slide.
    primary_scroll_net: usize,
    // Theme-derived defaults the terminal reports back for OSC 10/11/12
    // queries. Set by the front end via `set_default_colors`.
    default_fg_rgb: [u8; 3],
    default_bg_rgb: [u8; 3],
    default_cursor_rgb: [u8; 3],
    // Bytes the host has asked us to send back (DSR replies, etc.). Caller
    // drains via `take_response()` after each `feed`.
    pending_response: Vec<u8>,
    // Shell's reported working directory (OSC 7). `None` until a shell
    // integration emits one. `cwd_dirty` is set when it changes so the
    // front end can pull the update via `take_cwd_update()` (e.g. to retitle
    // the window or seed a new tab's cwd) without polling on every feed.
    cwd: Option<String>,
    cwd_dirty: bool,
    // Manually-set window title from OSC 0/2. `None` means no program has set
    // one (or it was cleared with an empty string), in which case the front end
    // falls back to the cwd-derived title. `title_dirty` mirrors `cwd_dirty`:
    // the front end pulls changes via `take_title_update()` after each feed.
    title: Option<String>,
    title_dirty: bool,
    // When the user pins a title via the command palette, this locks out the
    // shell: subsequent OSC 0/2 title requests are ignored until the title is
    // cleared from the palette, which unlocks it. Programmatic OSC titles never
    // set this — only `set_manual_title` does.
    title_locked: bool,
    parser: ansi::Parser,
    scrollback: VecDeque<Vec<Cell>>,
    scrollback_limit: usize,
    // Total lines evicted from the *front* of scrollback over the terminal's
    // life. `scrollback.len()` pins at `scrollback_limit` once full, so the
    // abs-line space (`scrollback.len() - view_offset + r`) shifts down by one
    // per eviction. This monotonic count lets the renderer form a *stable*
    // per-line id (`scrollback_evicted + abs_line`) for its row-vertex cache,
    // so a cached row can't be reused for a different line after eviction.
    scrollback_evicted: u64,
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
    // OSC 133 semantic prompt marks on the *primary* grid, in emission
    // order, anchored to live grid rows. Surviving scroll into scrollback
    // and resize lands in a later slice. Read
    // by `command_regions`; the prompt-navigation UI that consumes that
    // query is also a later slice, hence the allow.
    #[allow(dead_code)]
    semantic_marks: Vec<SemanticMark>,
    // Shell's live interactive input line, reported via the yutani-private
    // OSC 2122 extension. `Some` while the shell's line editor is active;
    // set on every edit by `handle_osc_2122`, cleared when a command is
    // submitted (OSC 133 `C` / OutputStart) and when the terminal switches
    // to the alternate screen (no prompt line editing happens there). Read
    // via `current_input()` by a later autocomplete slice, hence the allow.
    #[allow(dead_code)]
    current_input: Option<CurrentInput>,
    // Shell's history file path, reported via the yutani-private OSC 2124
    // extension. `histfile_dirty` is set when it changes so the front end can
    // pull it via `take_histfile_update()` and read + parse the file for
    // history-based completion suggestions.
    histfile: Option<String>,
    histfile_dirty: bool,
    // The last command submitted at a prompt: the OSC 2122 edit buffer that was
    // live when OSC 133 `C` (OutputStart) fired. Drained once via
    // `take_submitted_command()` and folded into the in-session command history.
    last_submitted_command: Option<String>,
    // Live-preview requests from the yutani-private OSC 2125 extension, used by
    // the first-run onboarding (which runs as the PTY child) to drive the real
    // renderer as the user picks a scheme / glow level. A FIFO because one feed
    // chunk can carry several; the front end drains it via
    // `take_preview_requests()` after each feed and applies each transiently.
    preview_requests: Vec<PreviewRequest>,
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
    /// Kitty graphics-protocol state (chunk assembly + id/format/extent maps).
    kitty: KittyImageState,
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
    /// OSC 8 hyperlink target interner. See [`HyperlinkStore`].
    hyperlinks: HyperlinkStore,
    /// Grapheme-cluster string interner. See [`ClusterStore`].
    clusters: ClusterStore,
    /// Tracks the most recent printed grapheme so `print` can absorb cluster
    /// extenders (ZWJ continuations, variation selectors, skin tones, combining
    /// marks, regional-indicator pairs) into the cell they belong to instead of
    /// spawning a new cell. `(row, col, last_codepoint, ri_pending)` where
    /// `ri_pending` marks a lone regional indicator awaiting its flag partner.
    last_grapheme: Option<GraphemeAnchor>,
}

#[derive(Copy, Clone, Debug)]
struct GraphemeAnchor {
    /// Grid cell holding the grapheme's lead codepoint.
    row: usize,
    col: usize,
    /// Cursor position right after the grapheme was laid down. The next print
    /// only merges into this grapheme if the cursor is still here — any cursor
    /// move (CR/LF/CUP/wrap) leaves the anchor stale and a stray combining mark
    /// then prints on its own, as before.
    post_row: usize,
    post_col: usize,
    /// The most recent codepoint folded into this grapheme — drives ZWJ
    /// continuation (a base right after a ZWJ extends the sequence).
    last: char,
    /// This grapheme is a single regional indicator awaiting a second to form a
    /// flag. Cleared once paired or once any other grapheme is printed.
    ri_pending: bool,
    /// This grapheme already occupies two columns (a wide base, a paired flag,
    /// or a VS16-promoted symbol). Stops a second widening trigger from adding
    /// another spacer.
    wide: bool,
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

/// Max length of an OSC 8 target URI we'll intern. Anything longer is treated
/// as a malformed sequence and closes the active link rather than growing the
/// store with an unbounded string.
const MAX_HYPERLINK_URI_LEN: usize = 4096;

/// Interns OSC 8 hyperlink targets so each `Cell` references one by a compact
/// id instead of owning the string. The id also drives hover co-highlighting:
/// every cell sharing an id is one logical link.
///
/// Grouping follows the OSC 8 `id=` parameter. A link opened with an explicit
/// `id=` is keyed by `(id, uri)`, so the *same* `(id, uri)` reused anywhere —
/// even in a non-contiguous region elsewhere on screen — interns to the same
/// id and co-highlights. A link opened *without* an id is anonymous: each open
/// gets a fresh id, so two anonymous spans never merge (only the contiguous
/// cells of one open share an id). The table only grows; distinct links per
/// session are few, and dropping entries would orphan ids still held by
/// scrollback cells.
#[derive(Default)]
pub struct HyperlinkStore {
    /// id (1-based) -> target URI.
    uris: Vec<String>,
    /// `(id_param, uri)` -> interned id, for explicit-`id=` dedup/grouping.
    keyed: std::collections::HashMap<(String, String), std::num::NonZeroU32>,
}

impl HyperlinkStore {
    /// Append a fresh entry for `uri`, returning its (1-based) id.
    fn push(&mut self, uri: &str) -> std::num::NonZeroU32 {
        self.uris.push(uri.to_string());
        // 1-based so the id is never zero — lets `Cell` carry it as a niche
        // `Option<NonZeroU32>` with no extra storage.
        std::num::NonZeroU32::new(self.uris.len() as u32).expect("len >= 1")
    }

    /// Intern an anonymous link (no `id=`): always a fresh id, so separate
    /// opens of the same URI stay distinct logical links.
    fn intern_anon(&mut self, uri: &str) -> std::num::NonZeroU32 {
        self.push(uri)
    }

    /// Intern a link carrying an explicit `id=`: `(id, uri)` dedupes to one id
    /// so every span sharing them is the same logical link and co-highlights.
    /// The id is scoped to the URI — the same `id=` with a different URI is a
    /// different link.
    fn intern_keyed(&mut self, id_param: &str, uri: &str) -> std::num::NonZeroU32 {
        let key = (id_param.to_string(), uri.to_string());
        if let Some(&id) = self.keyed.get(&key) {
            return id;
        }
        let id = self.push(uri);
        self.keyed.insert(key, id);
        id
    }

    /// Resolve an id back to its target URI.
    pub fn get(&self, id: std::num::NonZeroU32) -> Option<&str> {
        self.uris.get(id.get() as usize - 1).map(String::as_str)
    }
}

/// Interner for grapheme-cluster strings — the multi-codepoint content of a
/// cell that holds more than its first codepoint (emoji ZWJ sequences, flags,
/// emoji + skin tone / variation selector, base + combining marks). Identical
/// clusters dedupe to one id so a screen full of the same emoji costs one
/// string. Cells reference an id via `Cell::cluster`; `ch` keeps the first
/// codepoint for the fast path and for width.
#[derive(Default)]
pub struct ClusterStore {
    /// id (1-based) -> cluster string.
    strings: Vec<String>,
    /// cluster string -> interned id, for dedup.
    interned: std::collections::HashMap<String, std::num::NonZeroU32>,
}

impl ClusterStore {
    /// Intern `s`, returning its (1-based) id; identical strings share one id.
    fn intern(&mut self, s: &str) -> std::num::NonZeroU32 {
        if let Some(&id) = self.interned.get(s) {
            return id;
        }
        self.strings.push(s.to_string());
        let id = std::num::NonZeroU32::new(self.strings.len() as u32).expect("len >= 1");
        self.interned.insert(s.to_string(), id);
        id
    }

    /// Resolve an id back to its cluster string.
    pub fn get(&self, id: std::num::NonZeroU32) -> Option<&str> {
        self.strings.get(id.get() as usize - 1).map(String::as_str)
    }
}

/// Clamp a 1-based VT cursor coordinate (column or row) to a 0-based index
/// within `len`. VT params are 1-based with 0 meaning "default to 1", which the
/// `.max(1)` handles; the saturating clamp keeps a zero-sized axis from
/// underflowing. Single source for the `((v as usize).max(1) - 1).min(dim - 1)`
/// math that CUP/HPA/VPA all repeat.
fn clamp_cursor_1based(v: u16, len: usize) -> usize {
    ((v as usize).max(1) - 1).min(len.saturating_sub(1))
}

/// Convert an optional 1-based VT margin parameter to a 0-based index,
/// substituting `default` (itself 1-based) when the parameter is omitted.
/// Shared by DECSTBM (top/bottom) and DECSLRM (left/right).
fn margin_param_0based(param: Option<u16>, default: usize) -> usize {
    param.map(|v| v as usize).unwrap_or(default).saturating_sub(1)
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
            alternate_scroll: true,
            alt_scroll_snapshot: None,
            alt_scroll_net: 0,
            alt_scroll_region: None,
            alt_scroll_poison: false,
            alt_anim_departing: None,
            primary_scroll_net: 0,
            default_fg_rgb: [0xcc, 0xcc, 0xcc],
            default_bg_rgb: [0x00, 0x00, 0x00],
            default_cursor_rgb: [0xcc, 0xcc, 0xcc],
            pending_response: Vec::new(),
            cwd: None,
            cwd_dirty: false,
            title: None,
            title_dirty: false,
            title_locked: false,
            parser: ansi::Parser::new(),
            scrollback: VecDeque::new(),
            scrollback_limit,
            scrollback_evicted: 0,
            view_offset: 0,
            scrollback_placements: VecDeque::new(),
            semantic_marks: Vec::new(),
            current_input: None,
            histfile: None,
            histfile_dirty: false,
            last_submitted_command: None,
            preview_requests: Vec::new(),
            next_placement_id: 1,
            keep_placements_in_scrollback: true,
            pending_image_uploads: Vec::new(),
            kitty: KittyImageState::default(),
            cell_w_px: 1,
            line_h_px: 1,
            placeholder_decode: None,
            hyperlinks: HyperlinkStore::default(),
            clusters: ClusterStore::default(),
            last_grapheme: None,
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
        self.kitty.image_ids.insert(client_id, store_id);
    }

    /// Look up the store ImageId for a Kitty client `i=` id, if known.
    /// Used by the `a=p` placement path.
    pub fn kitty_image_id_lookup(&self, client_id: u32) -> Option<ImageId> {
        self.kitty.image_ids.get(&client_id).copied()
    }

    /// Total cell extent `(cols, rows)` from the original `a=T` / `a=t`
    /// transmission's `c=` / `r=`. The per-run placeholder renderer
    /// uses these as UV denominators. Returns `None` for ids whose
    /// transmission omitted one or both of `c=` / `r=` (the renderer
    /// must then skip the runs — without the denominator there's no
    /// honest UV).
    pub fn kitty_image_cell_extent(&self, client_id: u32) -> Option<(u32, u32)> {
        self.kitty.image_cell_extents.get(&client_id).copied()
    }

    /// Scan visible rows — including scrollback pulled into view via
    /// `view_offset` and the phantom strips above / below the live
    /// grid — for Kitty unicode-placeholder cells (`U+10EEEE` with an
    /// encoded image id), and emit one run per contiguous horizontal
    /// stretch the renderer can draw as a single textured quad.
    ///
    /// A run extends a previous cell when ALL of these hold:
    ///   - same visual row
    ///   - same `client_id`
    ///   - same `image_row` diacritic value
    ///   - `image_col == prev.image_col + 1`
    ///
    /// Any other transition (non-placeholder cell, id change, row
    /// change, image_col gap) starts a new run.
    ///
    /// Walking via [`Self::extended_cell`] rather than `active_grid`
    /// directly is what makes the image survive scrolling off the
    /// top: once placeholder cells are in scrollback the active grid
    /// no longer holds them, but `extended_cell` looks them up in
    /// the scrollback ring at the matching visual row whenever
    /// `view_offset > 0`. The same call also covers the
    /// `r_lo..r_hi = -2..rows+2` phantom strips that the cell-vertex
    /// loop uses during smooth-scroll, so a placeholder coming
    /// in/out of view doesn't pop at scroll-tick boundaries.
    ///
    /// Grouping happens per visual row only — vertical run-length
    /// compression would require the renderer to know that adjacent
    /// rows belong to the same image, which the older merged-bbox
    /// path got wrong (the merge swallowed valid sub-rect
    /// boundaries). Per-row is the smallest useful unit: a 29×15
    /// image collapses to 15 quads per frame, well under the
    /// renderer's per-call cost.
    pub fn kitty_placeholder_runs(&self) -> Vec<KittyPlaceholderRun> {
        let mut runs: Vec<KittyPlaceholderRun> = Vec::new();
        // Same phantom-row window the cell-vertex loop uses
        // (`update_vertices`'s `r_lo..r_hi`): two strips top + two
        // bottom so smooth-scroll doesn't pop.
        let r_lo: isize = -2;
        let r_hi: isize = self.rows as isize + 2;
        for r in r_lo..r_hi {
            let mut current: Option<KittyPlaceholderRun> = None;
            for c in 0..self.cols {
                let Some(cell) = self.extended_cell(r, c) else {
                    if let Some(run) = current.take() {
                        runs.push(run);
                    }
                    continue;
                };
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
        for &iid in self.kitty.image_ids.values() {
            out.insert(iid);
        }
        out
    }

    pub fn feed(&mut self, s: &str) {
        // Dispatch each event inline as it's parsed, rather than collecting a
        // per-call `Vec<Event>` and re-iterating. `Event` is ~32 bytes (its
        // `Osc(String)`/`Sgr(Vec<u16>)` variants), so under heavy output that
        // Vec — one slot per character — dominated `feed` (~85% of its cost was
        // building/moving/dropping it, measured). The parser is moved out of
        // `self` for the call so the emit closure can borrow the rest of `self`
        // (the dummy left behind is a no-alloc `Parser::default()`); its state
        // is restored after, preserving mid-sequence parsing across calls.
        let mut parser = std::mem::take(&mut self.parser);
        for ch in s.chars() {
            parser.feed(ch, |e| self.dispatch(e));
        }
        self.parser = parser;
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

    /// DEC mode ?1007 (alternate scroll). When true and the alt screen is
    /// active, the front end converts wheel motion into cursor-key presses
    /// (gated on mouse tracking being off — mouse reporting takes priority).
    pub fn alternate_scroll(&self) -> bool {
        self.alternate_scroll
    }

    /// Consume any alt-screen scroll captured during the last `feed`, returning
    /// its net direction, distance, and scroll region for the front end to
    /// animate. Stashes the departing rows on the terminal for `extended_cell`
    /// to serve during the slide; the front end must call `clear_alt_anim`
    /// when the animation ends. Returns `None` when nothing scrolled, the
    /// window was poisoned (partial-width, region-changing, or mixed-direction
    /// scrolls), or the net distance was zero.
    pub fn take_alt_scroll(&mut self) -> Option<AltScroll> {
        let snapshot = self.alt_scroll_snapshot.take();
        let net = std::mem::replace(&mut self.alt_scroll_net, 0);
        let region = self.alt_scroll_region.take();
        let poison = std::mem::replace(&mut self.alt_scroll_poison, false);
        let snapshot = snapshot?;
        let (region_top, region_bottom) = region?;
        if poison || net == 0 {
            return None;
        }
        let up = net > 0;
        let region_h = region_bottom + 1 - region_top;
        let d = (net.unsigned_abs() as usize).min(region_h);
        if d == 0 {
            return None;
        }
        // Departing rows are the edge of the pre-scroll region that slid out of
        // view: the top `d` rows for an upward scroll (they exit at the region
        // top), the bottom `d` for downward (they exit past the region bottom).
        let (departing, edge_row): (Vec<Vec<Cell>>, isize) = if up {
            (
                snapshot[region_top..region_top + d].to_vec(),
                region_top as isize,
            )
        } else {
            (
                snapshot[region_bottom + 1 - d..region_bottom + 1].to_vec(),
                region_bottom as isize + 1,
            )
        };
        self.alt_anim_departing = Some(AltAnimRows {
            up,
            edge_row,
            rows: departing,
        });
        Some(AltScroll {
            up,
            rows: d,
            region_top,
            region_bottom,
        })
    }

    /// Drop the frozen departing rows once the front end's slide completes.
    pub fn clear_alt_anim(&mut self) {
        self.alt_anim_departing = None;
    }

    /// Consume the net number of rows the primary screen scrolled into
    /// scrollback during the last `feed`, for the front end to animate as a
    /// smooth scroll-on-output slide. Returns 0 when nothing scrolled (or only
    /// the alt screen / a partial region did). Unlike `take_alt_scroll` there
    /// are no frozen rows to clear afterwards — the departing content is real
    /// scrollback the renderer already serves while `scroll_y > 0`.
    pub fn take_primary_scroll(&mut self) -> usize {
        std::mem::replace(&mut self.primary_scroll_net, 0)
    }

    /// Record an alt-screen scroll for later animation. Called from the scroll
    /// primitives *before* they mutate the grid, so the first call of a `feed`
    /// snapshots the pre-scroll frame. Only full-width scrolls animate (the
    /// uniform vertical-shift reconstruction can't model column ranges).
    /// Partial-width scrolls, a changing region, and direction flips within one
    /// feed poison the window. No-op off the alt screen (the primary screen
    /// scrolls into real scrollback and isn't animated).
    fn note_alt_region_scroll(&mut self, up: bool, n: usize, top: usize, bottom: usize, full_width: bool) {
        if !self.use_alternate || n == 0 || self.alt_scroll_poison {
            return;
        }
        if !full_width {
            self.alt_scroll_poison = true;
            return;
        }
        // A direction flip within one feed breaks the single-shift model.
        if (up && self.alt_scroll_net < 0) || (!up && self.alt_scroll_net > 0) {
            self.alt_scroll_poison = true;
            return;
        }
        // The region must stay constant across the accumulation window.
        match self.alt_scroll_region {
            Some(r) if r != (top, bottom) => {
                self.alt_scroll_poison = true;
                return;
            }
            None => self.alt_scroll_region = Some((top, bottom)),
            _ => {}
        }
        if self.alt_scroll_snapshot.is_none() {
            let snap = (0..self.rows)
                .map(|r| self.alternate.row(r).to_vec())
                .collect();
            self.alt_scroll_snapshot = Some(snap);
        }
        self.alt_scroll_net += if up { n as isize } else { -(n as isize) };
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
        // Cell colors are stored as `CellColor` sources now and resolved
        // against the live palette at render time, so a scheme swap needs no
        // per-cell rewrite — just force a full re-emit (the renderer also bumps
        // its row-cache epoch) so the new palette is picked up.
        self.primary.mark_all_dirty();
        self.alternate.mark_all_dirty();
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

    /// Lines evicted from the front of scrollback so far. Added to an abs-line
    /// index, it yields an id that's stable for the life of a line — abs-line
    /// alone shifts down by one each eviction once scrollback is full, so it
    /// can't safely key a cross-frame cache. See the renderer's row cache.
    pub fn scrollback_evicted(&self) -> u64 {
        self.scrollback_evicted
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
            if visual_row >= 0 && (visual_row as usize) < self.rows {
                return Some(self.alternate.get(visual_row as usize, col));
            }
            // During a scroll animation, the phantom band on the departing
            // edge is drawn from the frozen pre-scroll rows. They begin at
            // `edge_row`: an upward scroll's sit just above it (edge_row-d ..
            // edge_row-1); a downward scroll's sit at it and below (edge_row ..
            // edge_row+d-1).
            if let Some(anim) = &self.alt_anim_departing {
                let d = anim.rows.len() as isize;
                let idx = if anim.up {
                    if visual_row < anim.edge_row && visual_row >= anim.edge_row - d {
                        visual_row - (anim.edge_row - d)
                    } else {
                        return None;
                    }
                } else if visual_row >= anim.edge_row && visual_row < anim.edge_row + d {
                    visual_row - anim.edge_row
                } else {
                    return None;
                };
                return anim.rows[idx as usize].get(col).copied();
            }
            return None;
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
        // Captured/frozen scroll-animation rows are sized to the old grid;
        // a dimension change retires them rather than risk an inconsistent slide.
        self.alt_scroll_snapshot = None;
        self.alt_scroll_net = 0;
        self.alt_scroll_region = None;
        self.alt_scroll_poison = false;
        self.alt_anim_departing = None;
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
            // Spill only as many top rows as it takes to keep the live
            // content on screen. "Content" is the bottom-most of: the cursor
            // (the prompt's input row in a shell), the last non-blank cell
            // row, and the lowest row any image placement covers. When there
            // is blank space below that — a prompt sitting near the top of an
            // otherwise empty grid — trim the blank rows instead of pushing
            // real content into scrollback. Otherwise the prompt churns into
            // scrollback on every shrink and, because the shell repaints on
            // WINCH, reappears as stacked copies when the window grows back.
            // A grid whose content reaches the bottom still spills the whole
            // height delta, the previous behavior.
            let mut content_bottom = self.cursor.row;
            for r in (0..old_rows).rev() {
                if self
                    .primary
                    .row(r)
                    .iter()
                    .any(|c| c.ch != ' ' || c.placeholder_image_id.is_some())
                {
                    content_bottom = content_bottom.max(r);
                    break;
                }
            }
            for p in &self.primary.placements {
                if p.top_row >= 0 {
                    let b = (p.bottom_row() - 1).clamp(0, old_rows as isize - 1) as usize;
                    content_bottom = content_bottom.max(b);
                }
            }
            let spill = (content_bottom + 1).saturating_sub(rows);
            (spill, 0)
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
                    self.scrollback_evicted += 1;
                    self.evict_scrollback_placement_front();
                    self.evict_scrollback_mark_front();
                }
                self.scrollback.push_back(line);
            }
            // Match scroll_region_up_by: keep the user's view of historical
            // content stable while new lines stream into scrollback.
            if self.view_offset > 0 {
                self.view_offset += spill;
                self.clamp_view_offset();
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
            // Marks follow the same spill migration as placements.
            let sb_len_after_spill = self.scrollback.len() as isize;
            self.spill_marks(spill, sb_len_after_spill);
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
            self.clamp_view_offset();

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
            // Marks follow the same refill migration as placements.
            self.refill_marks(refill, sb_post);
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
        // Drop live marks that fell off the new grid and clamp mark columns
        // into range (horizontal resize is clamp-only — no column reflow).
        self.clamp_marks_after_resize(rows, cols);

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
        self.clamp_view_offset();
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

    /// Per-live-grid-row damage for the renderer: `row_damage()[r]` is true
    /// when live row `r` of the active grid has changed since the last
    /// `clear_row_damage`. Indexed in live-grid row terms (the renderer maps
    /// its visual rows through `view_offset`); scrollback rows are static and
    /// never appear here.
    pub fn row_damage(&self) -> &[bool] {
        self.active_grid().row_damage()
    }

    /// Clear the active grid's damage flags — the renderer calls this once it
    /// has emitted (or reused a cached segment for) every row of the frame.
    pub fn clear_row_damage(&mut self) {
        self.active_grid_mut().clear_row_damage();
    }

    fn scroll_region_up_by(&mut self, n: usize) {
        // Lines rolling off the top only become scrollback when the *whole*
        // grid is the scroll region — partial regions (DECSTBM or DECSLRM
        // narrower than the screen) just shift in place.
        let full_region = self.scroll_top == 0
            && self.scroll_bottom == self.rows - 1
            && self.scroll_left == 0
            && self.scroll_right == self.cols - 1;
        // Capture for smooth-scroll animation before the grid shifts.
        let full_width = self.scroll_left == 0 && self.scroll_right == self.cols - 1;
        self.note_alt_region_scroll(true, n, self.scroll_top, self.scroll_bottom, full_width);
        if !self.use_alternate && full_region && self.scrollback_limit > 0 {
            // Record the slide distance for the front-end scroll-on-output
            // animation. Capped at the grid height — the rows beyond a full
            // screen have already scrolled past what any slide could show.
            self.primary_scroll_net = (self.primary_scroll_net + n.min(self.rows)).min(self.rows);
            for _ in 0..n.min(self.rows) {
                let line = self.primary.row(self.scroll_top).to_vec();
                if self.scrollback.len() == self.scrollback_limit {
                    self.scrollback.pop_front();
                    self.scrollback_evicted += 1;
                    self.evict_scrollback_placement_front();
                    self.evict_scrollback_mark_front();
                }
                self.scrollback.push_back(line);
                // Keep the user's view of historical content stable while
                // new lines stream into scrollback. visible_cell indexes from
                // the end of scrollback, so without this bump every appended
                // line would shift the viewport down by one row.
                if self.view_offset > 0 {
                    self.view_offset += 1;
                    self.clamp_view_offset();
                }
            }
        }
        let blank = self.blank();
        let top = self.scroll_top;
        let bottom = self.scroll_bottom;
        let left = self.scroll_left;
        let right = self.scroll_right;
        // The damage flags may shift up with their content only when this is
        // the scrollback-growing full-screen scroll — exactly the case where a
        // line keeps its absolute-line identity (matches the push condition
        // above). On the alt screen or a partial region, content changes per
        // row position, so let `scroll_region_up` mark the region dirty.
        let shift_damage = full_region && !self.use_alternate && self.scrollback_limit > 0;
        let dropped = self
            .active_grid_mut()
            .scroll_region_up(top, bottom, left, right, n, blank, shift_damage);
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

        // Semantic marks follow the same promotion (they're cheap, so always
        // retained regardless of the placement keep-flag). Existing scrollback
        // marks were already decremented by the eviction calls above, so
        // `scrollback.len()` is final here.
        if !self.use_alternate && full_region {
            self.scroll_marks_up(n);
        }
    }

    /// Scroll the active region down by `n` (content moves down, blanks fill in
    /// at the top). The counterpart to `scroll_region_up_by`; used by SD
    /// (`CSI T`) and RI (`ESC M`) at the top margin. Captures for the
    /// smooth-scroll animation before mutating the grid.
    fn scroll_region_down_by(&mut self, n: usize) {
        let blank = self.blank();
        let top = self.scroll_top;
        let bottom = self.scroll_bottom;
        let left = self.scroll_left;
        let right = self.scroll_right;
        let full_width = left == 0 && right == self.cols - 1;
        self.note_alt_region_scroll(false, n, top, bottom, full_width);
        self.active_grid_mut()
            .scroll_region_down(top, bottom, left, right, n, blank);
    }

    /// RI (`ESC M`): move the cursor up one row, scrolling the region down when
    /// it sits at the top margin. This is how pagers (`less`) reveal the
    /// previous line when scrolling back.
    fn reverse_index(&mut self) {
        self.cursor.wrap_pending = false;
        if self.cursor.row == self.scroll_top {
            self.scroll_region_down_by(1);
        } else if self.cursor.row > 0 {
            self.cursor.row -= 1;
        }
    }

    /// Shift live marks up by `n` after a full-region primary scroll,
    /// converting those that scrolled off the top into scrollback anchors.
    /// Mirrors the placement promotion in `scroll_region_up_by`: a live row
    /// `r` becomes `r - n`; once negative its scrollback index is
    /// `scrollback.len() + (r - n)`, dropped if it underflowed the limit.
    fn scroll_marks_up(&mut self, n: usize) {
        let sb_len = self.scrollback.len() as isize;
        let n = n as isize;
        self.semantic_marks.retain_mut(|m| {
            let MarkAnchor::Live { row } = m.anchor else {
                return true; // scrollback marks handled by eviction
            };
            let new_row = row as isize - n;
            if new_row >= 0 {
                m.anchor = MarkAnchor::Live {
                    row: new_row as usize,
                };
                true
            } else {
                let scrollback_row = sb_len + new_row;
                if scrollback_row >= 0 {
                    m.anchor = MarkAnchor::Scrollback { row: scrollback_row };
                    true
                } else {
                    false
                }
            }
        });
    }

    /// Scrollback popped its front row; drop marks anchored to it and
    /// decrement the rest. Mirrors `evict_scrollback_placement_front`.
    fn evict_scrollback_mark_front(&mut self) {
        self.semantic_marks
            .retain(|m| !matches!(m.anchor, MarkAnchor::Scrollback { row } if row == 0));
        for m in &mut self.semantic_marks {
            if let MarkAnchor::Scrollback { row } = &mut m.anchor {
                *row -= 1;
            }
        }
    }

    /// Resize-shrink: migrate live marks through `spill` top rows pushed into
    /// scrollback. Mirrors the placement spill migration in `resize` — a mark
    /// in a spilled row (`row < spill`) anchors at
    /// `sb_len_after_spill - spill + row` (dropped if that underflowed the
    /// limit); a mark below the spill shifts up by `spill`.
    fn spill_marks(&mut self, spill: usize, sb_len_after_spill: isize) {
        self.semantic_marks.retain_mut(|m| {
            let MarkAnchor::Live { row } = m.anchor else {
                return true;
            };
            if row < spill {
                let scrollback_row = sb_len_after_spill - spill as isize + row as isize;
                if scrollback_row >= 0 {
                    m.anchor = MarkAnchor::Scrollback { row: scrollback_row };
                    true
                } else {
                    false
                }
            } else {
                m.anchor = MarkAnchor::Live { row: row - spill };
                true
            }
        });
    }

    /// Resize-grow: migrate marks through `refill` rows pulled from
    /// scrollback's tail back onto the live grid. Mirrors the placement
    /// refill migration — live marks shift down by `refill`; scrollback marks
    /// in the drained tail (`row >= sb_post`) promote back to live rows.
    fn refill_marks(&mut self, refill: usize, sb_post: isize) {
        for m in &mut self.semantic_marks {
            match &mut m.anchor {
                MarkAnchor::Live { row } => *row += refill,
                MarkAnchor::Scrollback { row } if *row >= sb_post => {
                    m.anchor = MarkAnchor::Live {
                        row: (*row - sb_post) as usize,
                    };
                }
                MarkAnchor::Scrollback { .. } => {}
            }
        }
    }

    /// After a resize, drop live marks that fell off the new grid (rows the
    /// shrink couldn't fit and the grow couldn't refill) and clamp mark
    /// columns into the new width.
    fn clamp_marks_after_resize(&mut self, rows: usize, cols: usize) {
        self.semantic_marks.retain(|m| match m.anchor {
            MarkAnchor::Live { row } => row < rows,
            MarkAnchor::Scrollback { .. } => true,
        });
        for m in &mut self.semantic_marks {
            if m.col >= cols {
                m.col = cols.saturating_sub(1);
            }
        }
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
            // Window-title setting. OSC 0 sets both the icon name and the
            // window title; OSC 2 sets the window title. We map both to the
            // window title (the only one we display). An empty payload clears
            // the manual title, falling back to the cwd-derived one.
            0 | 2 => self.set_window_title(rest),
            // OSC 1 sets the icon name only, which we don't display: ignore.
            1 => {}
            10 if rest == "?" => self.reply_color(10, self.default_fg_rgb),
            11 if rest == "?" => self.reply_color(11, self.default_bg_rgb),
            12 if rest == "?" => self.reply_color(12, self.default_cursor_rgb),
            // Shell working-directory report (used to seed new-tab cwd /
            // window titles). Payload is a `file://host/path` URL.
            7 => self.handle_osc_7(rest),
            // OSC 8 explicit hyperlinks (iTerm2 / VTE protocol).
            8 => self.handle_osc_8(rest),
            // FinalTerm / shell-integration semantic prompt marks.
            133 => self.handle_osc_133(rest),
            // yutani-private current-input report (autocomplete foundation).
            2122 => self.handle_osc_2122(rest),
            // yutani-private history-file path report (autocomplete history).
            2124 => self.handle_osc_2124(rest),
            // yutani-private live-preview control, emitted by the first-run
            // onboarding to drive the renderer as the user chooses.
            2125 => self.handle_osc_2125(rest),
            // iTerm2 proprietary namespace. Only `File=...` (inline
            // images) is implemented; everything else is silently
            // dropped to match iTerm's "unknown verb is a no-op" contract.
            1337 => self.handle_osc_1337(rest),
            _ => {}
        }
    }

    /// Resolve an OSC 8 hyperlink id (from [`Cell::hyperlink`]) to its target
    /// URI. Used by the front end to turn a hovered cell into a clickable URL.
    pub fn hyperlink_uri(&self, id: std::num::NonZeroU32) -> Option<&str> {
        self.hyperlinks.get(id)
    }

    /// Resolve a grapheme-cluster id (from [`Cell::cluster`]) to its full
    /// multi-codepoint string. Used by the renderer to rasterize the shaped
    /// cluster and by text extraction to copy it whole.
    pub fn cluster_str(&self, id: std::num::NonZeroU32) -> Option<&str> {
        self.clusters.get(id)
    }

    /// `OSC 8 ; params ; URI ST` — open or close an explicit hyperlink (the
    /// iTerm2 / VTE protocol). `params` is a colon-separated `key=value` list;
    /// only `id=` is defined and we accept-and-ignore it (links group by URI
    /// instead). An empty — or absent, or over-long — URI closes the active
    /// link. While a link is open, every printed cell carries its interned id
    /// (`Cell::hyperlink`); the heuristic URL detector in the front end still
    /// covers bare URLs that arrive without this wrapper.
    fn handle_osc_8(&mut self, rest: &str) {
        // `rest` is `params;URI`. The first ';' ends params; the URI itself
        // may contain ';' (query strings), so split exactly once and keep the
        // remainder verbatim. A `rest` with no ';' is malformed — treat the
        // whole thing as the URI rather than dropping it.
        let (params, uri) = rest.split_once(';').unwrap_or(("", rest));
        let uri = uri.trim();
        if uri.is_empty() || uri.len() > MAX_HYPERLINK_URI_LEN {
            self.cursor.hyperlink = None;
            return;
        }
        // `params` is a colon-separated `key=value` list. Only `id=` is
        // defined: it groups (possibly non-contiguous) spans of one logical
        // link. With an id, intern by `(id, uri)` so siblings co-highlight;
        // without one the link is anonymous (a fresh id per open).
        let id_param = params
            .split(':')
            .find_map(|kv| kv.strip_prefix("id="))
            .filter(|v| !v.is_empty());
        self.cursor.hyperlink = Some(match id_param {
            Some(id) => self.hyperlinks.intern_keyed(id, uri),
            None => self.hyperlinks.intern_anon(uri),
        });
    }

    /// Record (or clear) the window title from OSC 0/2. An empty payload clears
    /// it so the front end falls back to the cwd-derived title. Re-setting the
    /// same title doesn't mark it dirty, so a program that re-emits its title
    /// every prompt is free for the front end.
    ///
    /// Ignored entirely while the title is locked by `set_manual_title`: once
    /// the user pins a title from the command palette, the shell can't move it.
    pub fn set_window_title(&mut self, title: &str) {
        if self.title_locked {
            return;
        }
        self.apply_window_title(title);
    }

    /// Set the window title from the command palette's "Set title" action. A
    /// non-empty title *locks* it, so subsequent OSC 0/2 requests from the
    /// shell are ignored. An empty title clears the override and unlocks, so
    /// the shell (and cwd fallback) take over again.
    pub fn set_manual_title(&mut self, title: &str) {
        self.title_locked = !title.is_empty();
        self.apply_window_title(title);
    }

    /// Shared core of the OSC and palette title paths: stores the title and
    /// marks it dirty when it actually changed.
    fn apply_window_title(&mut self, title: &str) {
        let new = if title.is_empty() {
            None
        } else {
            Some(title.to_string())
        };
        if self.title != new {
            self.title = new;
            self.title_dirty = true;
        }
    }

    /// Returns the manual window title once if it changed since the last call,
    /// clearing the dirty flag. The outer `Option` is "did it change?"; the
    /// inner `Option<String>` is the new title (`None` == cleared, so the
    /// front end should fall back to the cwd-derived title).
    pub fn take_title_update(&mut self) -> Option<Option<String>> {
        if self.title_dirty {
            self.title_dirty = false;
            Some(self.title.clone())
        } else {
            None
        }
    }

    /// The manually-set window title (OSC 0/2), if one is currently active.
    #[allow(dead_code)]
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
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

/// Percent-decode an OSC 7 path. `%XX` (two hex digits) becomes the byte it
/// names; a `%` not followed by two hex digits is passed through literally.
/// The resulting bytes are interpreted as UTF-8, lossily — a malformed
/// sequence yields replacement chars rather than dropping the directory,
/// since a partly-garbled path is more useful to show than none.
fn percent_decode_path(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
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
    let fg = match style.fg {
        crate::style::CellColor::Default => return None,
        c => c.resolve([0.0, 0.0, 0.0, 1.0]),
    };
    let [r, g, b] = crate::palette::color_to_srgb_u8(fg);
    let id = ((r as u32) << 16) | ((g as u32) << 8) | (b as u32);
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
mod tests;
