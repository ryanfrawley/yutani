//! `WindowState` methods for font-size changes and color-scheme / theme
//! handling: config reload+persist, active-scheme resolution, palette-derived
//! colours, live preview, and theme-color sync.

use crate::*;

impl WindowState {
    /// Bump (or shrink) the font by `delta_pt` points and persist the new size.
    /// Thin wrapper over [`set_font_size`]; the zoom command saves, the
    /// onboarding live-preview path calls `set_font_size` directly so it
    /// doesn't touch disk.
    pub(crate) fn change_font_size(&mut self, delta_pt: f32) {
        if self.set_font_size(self.pt_size + delta_pt) {
            self.config.save();
        }
    }

    /// Set the font to an absolute point size and rebuild everything that
    /// depends on cell metrics: atlas, font texture, bind group, terminal grid,
    /// vertex/index buffers. Clamped to [6, 96] so the rasterizer never gets a
    /// nonsensical size. Returns whether the size actually changed. Does NOT
    /// persist config — callers that should (the zoom command) save themselves.
    pub(crate) fn set_font_size(&mut self, pt: f32) -> bool {
        let new_pt = pt.clamp(6.0, 96.0);
        if (new_pt - self.pt_size).abs() < f32::EPSILON {
            return false;
        }
        self.pt_size = new_pt;
        self.config.font_size = self.pt_size;
        self.rebuild_font_resources();
        true
    }

    /// React to a `WindowEvent::ScaleFactorChanged` — the window crossed onto a
    /// monitor with a different backing-scale (e.g. retina ↔ non-retina). The
    /// font is rasterized in physical pixels at `pt_size * dpi / 72`, so if we
    /// kept the old DPI the bitmaps would occupy the same pixel count on a
    /// screen with twice as many inches per pixel — doubling the apparent
    /// size. Recomputes `self.dpi = scale_factor * 96` and rebuilds the atlas
    /// + grid so the apparent point size stays constant across monitors.
    /// Returns whether the DPI actually changed.
    pub(crate) fn set_dpi_from_scale(&mut self, scale_factor: f64) -> bool {
        let new_dpi = (scale_factor * 96.0) as u32;
        if new_dpi == self.dpi {
            return false;
        }
        self.dpi = new_dpi;
        self.rebuild_font_resources();
        // The glow caches its uniforms (unlike the per-frame layout constants,
        // which re-read `self.dpi` each draw), so the scanline period and bloom
        // radius won't pick up the new backing scale until we re-push them.
        // Without this, the CRT stripes and halo would keep the old monitor's
        // size after a cross-display drag.
        let dpi_scale = dpi_px(1.0, self.dpi);
        let (w, h) = (self.surface.config.width, self.surface.config.height);
        for g in [&mut self.glow, &mut self.glow_fg] {
            g.set_dpi_scale(dpi_scale);
            g.write_glow_params(&self.shared.gpu.queue);
            g.write_uniforms(&self.shared.gpu.queue, w, h);
        }
        true
    }

    /// Re-apply the current `pt_size` / `dpi` to the shared FreeType faces and
    /// rebuild everything downstream of the cell metrics: glyph atlas, font
    /// texture + bind group, terminal grid dims, vertex/index buffers, cursor
    /// anim, row cache. Shared by [`set_font_size`] and [`set_dpi_from_scale`]
    /// — either field changing means the rasterizer produces different pixels
    /// and every cached glyph + per-row vertex segment is stale.
    fn rebuild_font_resources(&mut self) {
        // `with_font_mut` re-tunes the shared faces to this window's
        // (pt, dpi) before `build_atlas` rasterizes, so the atlas is packed
        // at the right pixel size even when a sibling window left the faces
        // tuned to a different monitor's scale.
        self.atlas = self.with_font_mut(|f| f.build_atlas());
        self.font_texture = renderer::texture::Texture::from_memory(
            &self.shared.gpu.device,
            &self.shared.gpu.queue,
            &self.atlas.buffer,
            self.atlas.width as u32,
            self.atlas.height as u32,
            wgpu::TextureFormat::R8Unorm,
            Some("font texture"),
        );
        // The new atlas carries a fresh (possibly re-dimensioned) color layer,
        // so rebuild the emoji texture alongside the mono one.
        self.emoji_texture = renderer::texture::Texture::from_memory(
            &self.shared.gpu.device,
            &self.shared.gpu.queue,
            &self.atlas.color_buffer,
            self.atlas.color_width as u32,
            self.atlas.color_height as u32,
            wgpu::TextureFormat::Rgba8UnormSrgb,
            Some("emoji texture"),
        );
        self.font_bind_group = self.shared.gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            layout: &self.shared.font_bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&self.font_texture.view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.font_texture.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&self.emoji_texture.view),
                },
            ],
            label: Some("font bind group"),
        });
        // Resize the grid to match the new cell dimensions, then refill the
        // vertex/index buffers (their capacity depends on grid size too).
        let metrics = self.with_font(|f| f.metrics());
        let viewport = WindowState::get_viewport_size(
            self.surface.config.width as f32,
            self.surface.config.height as f32,
            self.with_font(|f| f.cell_width()),
            ((metrics.ascender - metrics.descender) >> 6) as usize,
            self.chrome_extra_top(),
            self.dpi,
        );
        self.active_tab_mut().terminal.resize(viewport.char_width, viewport.char_height);
        self.notify_pty_size(viewport.char_width, viewport.char_height);
        self.sync_terminal_cell_size();
        self.resize_buffers();
        self.active_tab_mut().cursor_anim = None;
        // The atlas was rebuilt above, moving every glyph's UV; cached row
        // vertices reference the old atlas layout, so drop them all.
        self.invalidate_row_cache();
        self.invalidate();
    }

    /// Write the current in-memory config to disk and re-apply the scheme that
    /// now matches it (and the system appearance). Used by the palette's theme
    /// commands after they mutate a scheme slot or the follow-system flag, so
    /// the change both persists and takes effect live.
    ///
    /// Flags `pending_theme_broadcast` so the event loop fans the change out
    /// to every other window — sibling tabs in the same native group share
    /// the title bar and would otherwise hold stale palette state (terminal
    /// cells, glow uniforms, `NSWindow.backgroundColor`, appearance) until
    /// they were individually re-focused, flashing the old background for a
    /// frame the first time the user reveals them.
    pub(crate) fn persist_and_apply(&mut self) {
        self.config.save();
        self.apply_active_scheme();
        self.pending_theme_broadcast = true;
    }

    /// Re-read `~/.config/yutani/config` and re-install the color scheme,
    /// pushing palette-derived state into the GPU. Triggered by Cmd-Shift-R.
    ///
    /// Covers `color_scheme`, every `glow_*` knob, and any field the
    /// renderer reads off `self.config` each frame. Fields baked into
    /// one-shot resources at startup — pipeline creation, font face
    /// objects, etc. — still need a restart.
    pub(crate) fn reload_config(&mut self) {
        self.config = Config::load();
        // Hinting feeds the rasterizer, so a change only takes effect by
        // rebuilding the glyph atlas (and everything downstream of it), the
        // same way a font-size/DPI change does. `text_gamma` needs no rebuild —
        // it's re-read into the fade uniform on every frame. The shared Font is
        // process-wide; updating it here means the next window to rasterize
        // (this one, via the rebuild below) uses the new target.
        let hinting = self.config.font_hinting;
        if self.with_font_mut(|f| f.set_hinting(hinting)) {
            self.rebuild_font_resources();
        }
        self.apply_active_scheme();
    }

    /// True when the OS is currently in dark mode. On macOS this reads the
    /// *system* appearance (`NSApp.effectiveAppearance`) rather than
    /// `Window::theme()`: we pin each window's `NSAppearance` to its scheme
    /// background, which makes `Window::theme()` report that pinned value, not
    /// the OS setting (see [`crate::appearance`]). Elsewhere, and as a fallback,
    /// it uses winit's tracked window theme. Defaults to light if neither
    /// source reports one.
    pub(crate) fn system_is_dark(&self) -> bool {
        if let Some(dark) = crate::appearance::system_is_dark() {
            return dark;
        }
        self.window.theme() == Some(winit::window::Theme::Dark)
    }

    /// Install the color scheme that matches the current config and system
    /// appearance (see [`Config::active_scheme`]), then push every
    /// palette-derived value into the GPU and window chrome. Shared by config
    /// reloads, the palette's theme commands, and live system-appearance
    /// changes — anything that can change which scheme should be showing.
    pub(crate) fn apply_active_scheme(&mut self) {
        // Resolve to an owned name first so `self.config` isn't borrowed across
        // the `self.*` mutations below.
        let name = self.config.active_scheme(self.system_is_dark()).map(str::to_owned);
        install_color_scheme(name.as_deref());
        self.refresh_palette_derived();
    }

    /// Re-push every piece of renderer / window state that depends on the live
    /// palette and the current `self.config` glow knobs, then mark the frame
    /// dirty. Assumes `self.config` and the installed palette are already the
    /// ones we want shown — the caller is responsible for swapping those in
    /// (from disk in [`reload_config`], or transiently in [`apply_preview`]).
    pub(crate) fn refresh_palette_derived(&mut self) {
        // Glow uniforms cache palette-derived values (bright ANSI hues,
        // foreground / background RGB) — they don't re-read palette::get()
        // each frame the way the cell renderer does. Re-push them so the
        // halo, mask, and overlay all reflect the new scheme. Also re-mirror
        // every `glow_*` config slot so threshold / intensity / scanline
        // edits take effect — Glow holds its own copy of each field and
        // write_glow_params serializes whatever's on the instance.
        let p = palette::get();
        let bright: [[f32; 4]; 8] = [
            p.ansi[8], p.ansi[9], p.ansi[10], p.ansi[11],
            p.ansi[12], p.ansi[13], p.ansi[14], p.ansi[15],
        ];
        for g in [&mut self.glow, &mut self.glow_fg] {
            apply_glow_config(g, &self.config, &p.glow);
            g.set_bright_palette(&self.shared.gpu.queue, &bright);
            g.set_foreground(p.foreground);
            g.set_background(p.background);
            // Keep the scanline period (and bloom radius) scaled to this
            // window's DPI — a config reload or theme swap re-pushes the
            // params, so re-apply the scale. The bloom uniforms don't change
            // here (DPI is unchanged; they were set at create/resize), so only
            // the period needs re-uploading.
            g.set_dpi_scale(dpi_px(1.0, self.dpi));
            g.write_glow_params(&self.shared.gpu.queue);
        }

        // Already-painted cells carry pre-resolved RGBA from when their
        // SGR sequences ran under the old palette. Sweep them to pick
        // up the new scheme so the visible viewport actually changes
        // color, not just any new output printed after this point.
        // Truecolor cells (absolute RGB from the app) are left alone.
        self.active_tab_mut().terminal.reresolve_palette();
        // Cell vertex buffer caches bg colors and glyph fg colors per
        // cell; the sweep above just changed those values, so the
        // cached vertices are stale.
        self.vertices_dirty = true;
        // Every per-row cached vertex baked the old palette's colors — drop
        // them all so the next rebuild re-emits with the new scheme.
        self.invalidate_row_cache();

        // Flip the window's effective appearance and `NSWindow.backgroundColor`
        // first: `setAppearance:` triggers AppKit to re-render the title bar,
        // and we want our explicit `attributedTitle` (set in
        // `sync_theme_colors` → `tab_style::restyle` below) to be the last
        // write so the new foreground color always lands on the pill,
        // regardless of how AppKit orders its own redraw of the chrome.
        self.window.set_theme(Some(theme_for_bg(p.background)));
        set_native_window_bg(&self.window, p.background);
        // OSC 10/11/12 reports and the native tab title both need to reflect
        // the new bg / fg.
        self.sync_theme_colors();
        self.invalidate();
    }

    /// Apply one live-preview request from the first-run onboarding (OSC 2125).
    /// Scheme / glow / scanline previews mutate the live palette and the
    /// in-memory `self.config` *only* — nothing is written to disk, so quitting
    /// onboarding early leaves the user's config untouched. `Reload` is the
    /// commit step: the onboarding has saved its choices, so we re-read from
    /// disk to land on exactly the persisted look.
    pub(crate) fn apply_preview(&mut self, req: terminal::PreviewRequest) {
        use terminal::PreviewRequest;
        match req {
            PreviewRequest::Scheme(name) => {
                install_color_scheme(name.as_deref());
                self.config.color_scheme = name;
                self.refresh_palette_derived();
            }
            PreviewRequest::Glow(level) => {
                self.config.apply_glow_level(level);
                self.refresh_palette_derived();
            }
            PreviewRequest::Scanlines(on) => {
                self.config.apply_scanlines(on);
                self.refresh_palette_derived();
            }
            PreviewRequest::Crt(level) => {
                self.config.apply_crt_level(level);
                self.refresh_palette_derived();
            }
            // Font size rebuilds the atlas / grid itself; no palette refresh
            // needed. set_font_size doesn't persist, so the preview is transient
            // until the onboarding writes config + sends Reload.
            PreviewRequest::FontSize(pt) => {
                self.set_font_size(pt);
            }
            PreviewRequest::Reload => self.reload_config(),
        }
    }

    /// Push the current theme's foreground / background / cursor colors into
    /// the terminal so OSC 10/11/12 queries report something consistent with
    /// what the user actually sees.
    pub(crate) fn sync_theme_colors(&mut self) {
        let p = palette::get();
        // Palette values are stored linear (sRGB-decoded) so the GPU's
        // gamma-encoding lands on the user's intended hex. Re-encode here
        // for OSC 10/11/12 so reports match the scheme's hex literals.
        let to_u8 = palette::color_to_srgb_u8;
        self.active_tab_mut().terminal.set_default_colors(to_u8(p.foreground), to_u8(p.background), to_u8(p.cursor));
        // If an app subscribed to color-scheme updates (DEC mode 2031) and the
        // background just crossed the light/dark line, emit the notification and
        // flush it to the PTY now: this path is reached from theme changes
        // (system appearance, palette commands, config reload), none of which is
        // followed by a `PtyInput` feed, so the event loop's usual post-feed
        // response drain won't run for it.
        if self.active_tab_mut().terminal.notify_color_scheme_change() {
            let reply = self.active_tab_mut().terminal.take_response();
            self.write_pty(&reply);
        }
        // The native tab titles use dynamic system colors that track the window
        // appearance, but re-style so a theme flip repaints them immediately.
        tab_style::restyle(&self.window);
    }

    /// One-time native-tab chrome setup once the window exists (drops the
    /// title-bar separator), then style the initial tab title.
    pub(crate) fn configure_native_tabs(&mut self) {
        tab_style::configure_window(&self.window);
        tab_style::restyle(&self.window);
    }

    /// Re-style this window's native tab title (emphasized when selected, dimmed
    /// otherwise). Called whenever the tab set, selection, or title changes.
    pub(crate) fn refresh_tab_bar(&mut self) {
        tab_style::restyle(&self.window);
    }
}
