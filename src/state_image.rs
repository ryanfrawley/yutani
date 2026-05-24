//! `WindowState` methods for inline image placements: anchoring a decode at
//! a cell, polling async decodes, draining GPU uploads, and cell-size sync.

use crate::*;

impl WindowState {
    /// Read the system clipboard and write it to the PTY, wrapped in
    /// bracketed-paste markers if the host has enabled them.
    /// Read `path` from disk and fire a decode job. The placement on the
    /// active grid lands later, when `poll_pending_images` (called each
    /// frame) sees the worker's result and computes the cell extent from
    /// the now-known image dimensions + current font metrics.
    pub(crate) fn load_image_at_cell(&mut self, path: &str, row: isize, col: isize, label: &str) {
        if !self.config.images_enabled {
            return;
        }
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("image load failed: {path}: {e}");
                return;
            }
        };
        // Cmd-Shift-I uses the deferred-placement path because no cell
        // extent was specified — `poll_pending_images` computes it from
        // the decoded image's pixel dimensions. The pre-allocated
        // ImageId is therefore discarded here; the OSC 1337 path uses it.
        let (pending, _image_id) = self.tabs[self.active].image_store.request_insert(
            bytes,
            self.config.images_max_pixels,
            std::time::Duration::from_millis(self.config.images_decode_timeout_ms),
            Some(label.to_string()),
        );
        self.active_tab_mut().pending_placements.push(PendingImagePlacement {
            request: pending,
            row,
            col,
            preplaced_image_id: None,
        });
        // Tick the loop until the decode completes (or times out).
        // Otherwise an idle window would never re-enter `render()` to call
        // `poll_pending_images`. The follow-up redraw in
        // `poll_pending_images` keeps polling until pending_placements
        // drains.
        self.window.request_redraw();
    }

    /// Drain finished decode results from `image_store.poll` and turn the
    /// successful ones into placements. Errors log and the pending entry
    /// is dropped; the render loop is otherwise unaffected.
    pub(crate) fn poll_pending_images(&mut self) {
        // Skip when both main.rs's placement queue AND the store's
        // own pending queue are empty. The store-side check matters
        // for Kitty animation frames (`a=f`): each frame insert
        // bumps `Store::pending` and queues an `immediate_results`
        // entry, but goes nowhere near `pending_placements`. Without
        // the store-side check, frames pile up unprocessed once the
        // base image's PendingImagePlacement finalizes and
        // `pending_placements` empties — and the animation stays
        // pinned on its first frame forever.
        if self.tabs[self.active].pending_placements.is_empty() && self.tabs[self.active].image_store.pending_count() == 0 {
            return;
        }
        let nearest = self.config.images_filter == "nearest";
        let results = self.tabs[self.active].image_store.poll(
            &self.image_pipeline,
            &self.shared.gpu.device,
            &self.shared.gpu.queue,
            nearest,
        );
        // CRITICAL: must request_redraw if there are still pending decodes,
        // even when this poll returned empty — otherwise the render loop
        // stalls and the request only completes when some unrelated event
        // (mouse move, keystroke) wakes the loop. Symptom: decode "timeouts"
        // at multi-second elapsed times that don't match the configured
        // timeout. Has to happen before the early-return when no results.
        //
        // Check the store's own `pending_count` too, not just
        // `pending_placements`: the Kitty Unicode-placeholder path
        // (`a=T,U=1`, as `icat` emits under tmux) and animation frames
        // (`a=f`) bump the store's queue WITHOUT registering a
        // `pending_placement`. With only the `pending_placements` check,
        // a single-burst transmit whose decode finishes after this frame's
        // poll re-arms nothing — the loop sleeps and the image renders
        // blank until the next unrelated event. (Release builds feed the
        // whole `cat`/`icat` in one burst and reliably lose this race;
        // slower debug builds spread ingest across events and win it.)
        if should_rearm_image_poll(
            self.active_tab().pending_placements.is_empty(),
            results.is_empty(),
            self.active_tab().image_store.pending_count(),
        ) {
            self.window.request_redraw();
        }
        if results.is_empty() {
            return;
        }
        let metrics = self.shared.with_font(|f| f.face().size_metrics().unwrap());
        let line_height = ((metrics.ascender - metrics.descender) >> 6) as u32;
        let cell_w = self.shared.with_font(|f| f.cell_width()) as u32;
        let mut any_placed = false;
        for (pending_id, outcome) in results {
            let Some(i) = self.active_tab()
                .pending_placements
                .iter()
                .position(|p| p.request == pending_id)
            else {
                // Result with no pending entry — caller did request_insert
                // but never registered a placement (shouldn't happen in
                // current code paths). Drop the GPU upload on the floor.
                continue;
            };
            let pp = self.active_tab_mut().pending_placements.remove(i);
            match (outcome, pp.preplaced_image_id) {
                // Deferred path success: compute extent from pixel dims
                // and create the placement now.
                (Ok(image_id), None) => {
                    let img = self.active_tab()
                        .image_store
                        .peek(image_id)
                        .expect("just-inserted image");
                    let rows = (img.height_px + line_height - 1) / line_height;
                    let cols = (img.width_px + cell_w - 1) / cell_w;
                    let rows = rows.clamp(1, u16::MAX as u32) as u16;
                    let cols = cols.clamp(1, u16::MAX as u32) as u16;
                    self.active_tab_mut().terminal
                        .insert_placement(image_id, pp.row, pp.col, rows, cols, 0);
                    any_placed = true;
                }
                // Pre-placed path success: the placement already exists
                // referencing this image_id; renderer's next `peek` will
                // start drawing pixels. Just force a redraw.
                (Ok(_image_id), Some(_)) => {
                    any_placed = true;
                }
                // Deferred path failure: nothing to clean up — placement
                // was never created.
                (Err(e), None) => eprintln!("image decode failed: {e}"),
                // Pre-placed path failure: drop the orphaned placement so
                // the user doesn't stare at a blank space forever.
                (Err(e), Some(image_id)) => {
                    eprintln!("image decode failed: {e}");
                    let removed = self.active_tab_mut().terminal.remove_placements_with_image(image_id);
                    if removed > 0 {
                        any_placed = true; // grid changed; redraw
                    }
                }
            }
        }
        if any_placed {
            // Mark vertices dirty: a new placement may need half-block
            // override cells, and a removed placement may need its
            // override cells cleared. The redraw request also fires so
            // the new vertex buffer reaches the screen this frame.
            self.vertices_dirty = true;
            self.window.request_redraw();
        }
        // (The "keep ticking while pending" redraw was issued up-front,
        // before the early-return for empty `results` — see the comment
        // there. Avoid double-requesting the same frame.)
    }

    /// Wrap `Terminal::feed` so any OSC-1337 payloads parsed in this PTY
    /// chunk are turned into real Store reservations + Placements before
    /// the next chunk is processed. Without this, a follow-up chunk
    /// containing scroll/text could mutate the grid between cursor
    /// advance and placement insertion — leaving the placement at a
    /// stale anchor. See sub-slice P2.4 design notes.
    pub(crate) fn feed_terminal(&mut self, bytes: &str) {
        self.active_tab_mut().terminal.feed(bytes);
        self.maybe_start_alt_scroll();
        self.drain_pending_image_uploads();
    }

    /// Pull every iTerm2 OSC-1337 (and future protocol) payload off the
    /// Terminal's outbox, fire a decode job per upload with the
    /// pre-allocated ImageId, and insert the placement at the cell
    /// anchor the parser captured. Placement creation happens *before*
    /// the worker finishes the decode; `Store::peek` returns `None`
    /// until then so the renderer skips drawing for one or two frames.
    pub(crate) fn drain_pending_image_uploads(&mut self) {
        if !self.config.images_enabled {
            // Drain anyway so the queue doesn't grow unboundedly if the
            // config is toggled at runtime.
            let _ = self.active_tab_mut().terminal.take_pending_image_uploads();
            return;
        }
        let uploads = self.active_tab_mut().terminal.take_pending_image_uploads();
        if uploads.is_empty() {
            return;
        }
        for up in uploads {
            // `a=a` control message — no pixel data, no decode. Route
            // straight into the store's playback-state mutation.
            if let Some(ctrl) = up.animation_control.clone() {
                let Some(client_id) = up.kitty_image_id else { continue };
                let Some(image_id) = self.active_tab().terminal.kitty_image_id_lookup(client_id) else {
                    continue;
                };
                self.active_tab_mut().image_store.apply_animation_control(
                    image_id,
                    ctrl.control,
                    ctrl.loop_count,
                    ctrl.make_current,
                    ctrl.edit_frame,
                    ctrl.edit_gap_ms,
                    std::time::Instant::now(),
                );
                // Animation state change may need a redraw to land on
                // the new current frame and / or kick off the
                // wall-clock advance.
                self.window.request_redraw();
                continue;
            }
            // `a=f` frame transmission — append to the parent image's
            // frames vec via the dedicated request path. Raw RGBA
            // payloads use the worker-bypass variant; PNG-style
            // payloads go through the worker.
            if let Some(frame_spec) = up.animation_frame.clone() {
                let Some(client_id) = up.kitty_image_id else { continue };
                let Some(parent) = self.active_tab().terminal.kitty_image_id_lookup(client_id) else {
                    continue;
                };
                if let Some((w, h)) = up.raw_rgba_dims {
                    let _ = self.active_tab_mut().image_store.request_insert_frame_rgba(
                        parent,
                        up.bytes,
                        w,
                        h,
                        up.label,
                        frame_spec.target_slot,
                        frame_spec.compose_base,
                        frame_spec.gap_ms,
                        frame_spec.dst_x,
                        frame_spec.dst_y,
                    );
                } else {
                    let _ = self.tabs[self.active].image_store.request_insert_frame(
                        parent,
                        up.bytes,
                        self.config.images_max_pixels,
                        std::time::Duration::from_millis(
                            self.config.images_decode_timeout_ms,
                        ),
                        up.label,
                        frame_spec.target_slot,
                        frame_spec.compose_base,
                        frame_spec.gap_ms,
                        frame_spec.dst_x,
                        frame_spec.dst_y,
                    );
                }
                self.window.request_redraw();
                continue;
            }
            // Kitty uploads (`a=t` / `a=T`) opt into the animatable
            // variant so the store keeps a CPU RGBA copy of the base
            // — needed if a later `a=f` arrives and has to composite
            // against it. iTerm OSC 1337 / debug-keybind paths skip
            // this since they can never receive frames. Raw RGBA
            // payloads (signaled by `raw_rgba_dims`) skip the decode
            // worker entirely; PNG-style payloads go through it.
            let (pending, image_id) = if let Some((w, h)) = up.raw_rgba_dims {
                self.active_tab_mut().image_store.request_insert_animatable_rgba(
                    up.bytes,
                    w,
                    h,
                    up.label,
                )
            } else if up.kitty_image_id.is_some() {
                self.tabs[self.active].image_store.request_insert_animatable(
                    up.bytes,
                    self.config.images_max_pixels,
                    std::time::Duration::from_millis(self.config.images_decode_timeout_ms),
                    up.label,
                )
            } else {
                self.tabs[self.active].image_store.request_insert(
                    up.bytes,
                    self.config.images_max_pixels,
                    std::time::Duration::from_millis(self.config.images_decode_timeout_ms),
                    up.label,
                )
            };
            // Kitty `a=t` / `a=T` may carry an `i=` id the client uses
            // to refer back to this image via `a=p` (place) or `a=d`
            // (delete). Register the mapping immediately so those ops
            // resolve even before the decode completes.
            if let Some(client_id) = up.kitty_image_id {
                self.active_tab_mut().terminal.register_kitty_image_id(client_id, image_id);
            }
            let (rows, cols) = up.cell_extent;
            let (row, col) = up.cell_anchor;
            // `a=t` (transmit-only): no placement yet; the client will
            // send `a=p` later to display. We still queue the upload
            // so the decode runs and the store gets the pixels.
            if up.display_immediately {
                // Route through the Kitty-aware variant so X=/Y= offsets,
                // z-index, source crops, and the client's image/placement
                // ids all thread onto the Placement. For iTerm OSCs all
                // the Kitty-only fields are at their defaults so this
                // produces the same result as `insert_placement`.
                self.active_tab_mut().terminal.insert_placement_kitty(
                    image_id,
                    row,
                    col,
                    rows,
                    cols,
                    up.z_index,
                    up.pixel_offset,
                    up.src_rect,
                    up.kitty_image_id,
                    up.kitty_placement_id,
                );
            }
            self.active_tab_mut().pending_placements.push(PendingImagePlacement {
                request: pending,
                row,
                col,
                preplaced_image_id: if Self::suppress_deferred_placement(
                    up.display_immediately,
                    up.kitty_image_id,
                ) {
                    Some(image_id)
                } else {
                    None
                },
            });
        }
        // A placement just appeared; trigger a redraw so the (still-empty)
        // reservation gets a chance to fill on the next poll.
        self.window.request_redraw();
    }

    /// Push current font cell metrics into the terminal so the OSC-1337
    /// sizing math can resolve `Npx` / `N%` / `Auto` specs. Called on
    /// init and on every font-size change.
    pub(crate) fn sync_terminal_cell_size(&mut self) {
        let metrics = self.shared.with_font(|f| f.face().size_metrics().unwrap());
        let line_h = ((metrics.ascender - metrics.descender) >> 6) as u32;
        let cell_w = self.shared.with_font(|f| f.cell_width()) as u32;
        self.active_tab_mut().terminal.set_cell_size_px(cell_w, line_h);
    }

    /// Path used by the Cmd-Shift-I keybind. Env var override beats the
    /// config-dir default so a quick `YUTANI_DEBUG_IMAGE=foo.png cargo run`
    /// works without touching the file system.
    pub(crate) fn debug_image_path() -> Option<std::path::PathBuf> {
        if let Ok(p) = std::env::var("YUTANI_DEBUG_IMAGE") {
            return Some(std::path::PathBuf::from(p));
        }
        let mut p = config_dir()?;
        p.push("debug_image.png");
        Some(p)
    }
}
