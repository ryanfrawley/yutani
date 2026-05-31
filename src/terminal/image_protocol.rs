//! Inline-image protocol handlers for [`Terminal`].
//!
//! Carved out of the main `terminal.rs` `impl Terminal` block: the iTerm2
//! `OSC 1337 File=` path (`handle_osc_1337`) and the full Kitty graphics
//! protocol (`handle_apc*` transmit/place/animate/delete plus the
//! `finalize_kitty_image_*` decode→placement helpers). These are the most
//! self-contained subsystem in the terminal model — they touch the grid only
//! through the placement API and a handful of cursor reads — so they live in
//! their own file. Shared Kitty types (`KittyControl`, `KittyChunks`, …) and
//! the pure decode/parse free functions stay in the parent module and reach
//! here via `use super::*`.

use super::*;

impl Terminal {
    /// `OSC 1337 ; <verb>=<args> [: <base64>] ST` — iTerm2's proprietary
    /// channel. The one verb we care about is `File=key=val,...:<base64>`
    /// for inline images.
    pub(super) fn handle_osc_1337(&mut self, payload: &str) {
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
    pub(super) fn handle_xtwinops_query(&mut self, ps: u16) {
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
    pub(super) fn handle_apc(&mut self, s: &str) {
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
            if let Some(id) = self.kitty.current_chunked_id {
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
                ctrl.image_id = self.kitty.last_image_id;
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
                    self.kitty.current_chunked_id = Some(id);
                } else if self.kitty.current_chunked_id == Some(id) {
                    self.kitty.current_chunked_id = None;
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
                    self.kitty.chunks.entry(client_id).or_insert_with(|| KittyChunks {
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
            if let Some(mut acc) = self.kitty.chunks.remove(&client_id) {
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
            self.kitty.image_formats.get(&client_id).copied(),
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
                self.kitty.image_ids.clear();
                self.kitty.image_formats.clear();
                self.kitty.image_cell_extents.clear();
            }
            KittyDeleteSelector::Image => {
                let Some(client_id) = ctrl.image_id else { return };
                let Some(image_id) = self.kitty_image_id_lookup(client_id) else { return };
                self.remove_placements_with_image(image_id);
                self.kitty.image_ids.remove(&client_id);
                self.kitty.image_formats.remove(&client_id);
                self.kitty.image_cell_extents.remove(&client_id);
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
    pub(super) fn kitty_query_supported(
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
                let entry = self.kitty.chunks.entry(id).or_insert_with(|| KittyChunks {
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
            (Some(id), false) if self.kitty.chunks.contains_key(&id) => {
                let mut acc = self.kitty.chunks.remove(&id).expect("contains_key");
                append_b64_filtered(&mut acc.b64, payload);
                self.finalize_kitty_image_from_chunks(&acc);
            }
            (None, true) => {
                // Anonymous chunk. The first one establishes the
                // sizing/format; later ones just append base64. A new
                // first-chunk while one's open overwrites — there's no
                // way to distinguish them otherwise.
                let entry = self.kitty.chunks_anon.get_or_insert_with(|| KittyChunks {
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
            (None, false) if self.kitty.chunks_anon.is_some() => {
                let mut acc = self.kitty.chunks_anon.take().expect("is_some");
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
            self.kitty.image_formats.insert(id, source_format);
            // Mark this image as "most recently completed" so a
            // subsequent `a=p` / `a=d` / `a=f` / `a=a` arriving
            // without `i=` can target it (per the Kitty spec
            // fallback). icat's animation-frame stream relies on
            // this — it emits `a=f` with no `i=` and no `m=` between
            // animation-control messages.
            self.kitty.last_image_id = Some(id);
            // Cache the image's total cell extent so the per-run
            // placeholder renderer can compute UVs against it. Only
            // record when BOTH dimensions are present — partial
            // values can't define a tiling.
            if let (Some(c), Some(r)) = (cells_cols, cells_rows) {
                self.kitty.image_cell_extents.insert(id, (c, r));
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
}
