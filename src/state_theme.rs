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
        self.shared.font.borrow_mut().set_char_size(self.pt_size, self.dpi);
        self.atlas = self.shared.font.borrow_mut().build_atlas();
        self.font_texture = renderer::texture::Texture::from_memory(
            &self.shared.gpu.device,
            &self.shared.gpu.queue,
            &self.atlas.buffer,
            self.atlas.width as u32,
            self.atlas.height as u32,
            wgpu::TextureFormat::R8Unorm,
            Some("font texture"),
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
            ],
            label: Some("font bind group"),
        });
        // Resize the grid to match the new cell dimensions, then refill the
        // vertex/index buffers (their capacity depends on grid size too).
        let metrics = self.shared.with_font(|f| f.face().size_metrics().unwrap());
        let viewport = WindowState::get_viewport_size(
            self.surface.config.width as f32,
            self.surface.config.height as f32,
            self.shared.with_font(|f| f.cell_width()),
            ((metrics.ascender - metrics.descender) >> 6) as usize,
        );
        self.active_tab_mut().terminal.resize(viewport.char_width, viewport.char_height);
        self.notify_pty_size(viewport.char_width, viewport.char_height);
        self.sync_terminal_cell_size();
        self.resize_buffers();
        self.active_tab_mut().cursor_anim = None;
        self.invalidate();
        true
    }

    /// Write the current in-memory config to disk and re-apply the scheme that
    /// now matches it (and the system appearance). Used by the palette's theme
    /// commands after they mutate a scheme slot or the follow-system flag, so
    /// the change both persists and takes effect live.
    pub(crate) fn persist_and_apply(&mut self) {
        self.config.save();
        self.apply_active_scheme();
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
        self.apply_active_scheme();
    }

    /// True when the OS is currently in dark mode, per winit's tracked window
    /// theme (updated from `WindowEvent::ThemeChanged`). Defaults to light if
    /// the platform doesn't report one.
    pub(crate) fn system_is_dark(&self) -> bool {
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

        // OSC 10/11/12 reports and the title-bar appearance both need to
        // reflect the new bg / fg.
        self.sync_theme_colors();
        self.window.set_theme(Some(theme_for_bg(p.background)));
        set_native_window_bg(&self.window, p.background);
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
        let to_u8 = |c: [f32; 4]| [
            palette::linear_to_srgb_u8(c[0]),
            palette::linear_to_srgb_u8(c[1]),
            palette::linear_to_srgb_u8(c[2]),
        ];
        self.active_tab_mut().terminal.set_default_colors(to_u8(p.foreground), to_u8(p.background), to_u8(p.cursor));
    }
}
