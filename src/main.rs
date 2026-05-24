mod app_window;
mod box_drawing;
mod command_palette;
mod completion;
mod search;
mod font;
mod font_loader;
mod renderer;

mod ansi;
mod gpu;
mod images;
mod input;
mod onboard;
mod palette;
mod shaper;
mod shell_integration;
mod style;
mod terminal;

mod pty;

use winit::{
    event::*,
    event_loop::EventLoopBuilder,
    event_loop::EventLoopWindowTarget,
    platform::macos::WindowBuilderExtMacOS,
    platform::modifier_supplement::KeyEventExtModifierSupplement,
    window::{Window, WindowBuilder},
};

extern crate libc;
use nix::libc::*;

use std::cell::RefCell;
use std::rc::Rc;

// use rand_distr::{Distribution, Normal};
// use rand::thread_rng;

use wgpu::util::DeviceExt;

const WINDOW_PADDING: f32 = 16.0;
const DECORATOR_HEIGHT: f32 = 24.0;
/// Maximum rows the completion popup shows at once; longer lists scroll. Shared
/// by the draw block and the keyboard-nav handler so the two can't drift.
const COMPLETION_MAX_VISIBLE: usize = 10;
/// Smallest grid height (in rows) we ever report to the PTY, regardless of how
/// short the window is dragged. Sized to keep a typical multi-line shell prompt
/// resident so resizing never spills it — see the floor in `get_viewport_size`.
const MIN_GRID_ROWS: usize = 4;

const DEFAULT_FONT_SIZE: f32 = 10.0;

fn config_dir() -> Option<std::path::PathBuf> {
    let home = std::env::var_os("HOME")?;
    let mut p = std::path::PathBuf::from(home);
    p.push(".config");
    p.push("yutani");
    Some(p)
}

fn config_path() -> Option<std::path::PathBuf> {
    let mut p = config_dir()?;
    p.push("config.toml");
    Some(p)
}

/// Read the named color scheme from disk and install it as the live
/// palette. `None` (or any read failure) reverts to the built-in defaults
/// so a config edit that *removes* `color_scheme` actually goes back to
/// neutral, not back to whatever was last loaded.
fn install_color_scheme(name: Option<&str>) {
    let palette_for = |name: &str| -> Option<palette::Palette> {
        let path = scheme_path(name)?;
        match std::fs::read_to_string(&path) {
            Ok(src) => Some(palette::parse_toml(&src)),
            Err(e) => {
                eprintln!("palette: failed to read {}: {}", path.display(), e);
                None
            }
        }
    };
    let p = name.and_then(palette_for).unwrap_or(palette::Palette::defaults());
    palette::install(p);
}

/// Resolve a scheme name to its on-disk `.toml` path under
/// `~/.config/yutani/schemes/`.
fn scheme_path(name: &str) -> Option<std::path::PathBuf> {
    let mut dir = config_dir()?;
    dir.push("schemes");
    Some(dir.join(format!("{}.toml", name)))
}

/// `~/.local/state/yutani` — XDG_STATE_HOME for persistent-but-disposable
/// state. Kept deliberately separate from `~/.config/yutani`: config is the
/// user's to hand-edit, delete, or version-control, and none of that should
/// silently re-arm or suppress first-run onboarding. Honors `$XDG_STATE_HOME`
/// when set, falling back to `$HOME/.local/state`.
fn state_dir() -> Option<std::path::PathBuf> {
    let mut p = match std::env::var_os("XDG_STATE_HOME") {
        Some(x) if !x.is_empty() => std::path::PathBuf::from(x),
        _ => {
            let home = std::env::var_os("HOME")?;
            let mut p = std::path::PathBuf::from(home);
            p.push(".local");
            p.push("state");
            p
        }
    };
    p.push("yutani");
    Some(p)
}

/// Marker file recording that first-run onboarding has completed. Its contents
/// are the onboarding *revision* that ran (a bare integer), so a future Yutani
/// can re-introduce setup for a new feature by bumping [`ONBOARD_REVISION`]
/// without re-onboarding users who are already current.
fn onboarding_marker() -> Option<std::path::PathBuf> {
    Some(state_dir()?.join("onboarded"))
}

/// Current onboarding revision. First-run fires when the marker is missing or
/// records a lower number; bump this when onboarding gains a step worth
/// re-showing to existing users.
const ONBOARD_REVISION: u32 = 1;

/// Read the onboarding revision recorded on disk, if any. `None` means setup
/// has never completed (or the marker is unreadable / malformed — both treated
/// as "not yet onboarded", erring toward showing setup rather than skipping it).
fn onboarded_revision() -> Option<u32> {
    let path = onboarding_marker()?;
    let s = std::fs::read_to_string(path).ok()?;
    s.trim().parse().ok()
}

/// Whether first-run onboarding should run: never completed, or completed at an
/// older revision than we ship now.
fn needs_onboarding() -> bool {
    onboarded_revision().map_or(true, |r| r < ONBOARD_REVISION)
}

/// Stamp the marker with the current revision, creating `state_dir()` as
/// needed. Best-effort: a write failure just means onboarding runs again next
/// launch, which is the safe direction to fail.
fn mark_onboarded() {
    let Some(path) = onboarding_marker() else { return };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, ONBOARD_REVISION.to_string());
}

/// Lower the onboarding marker so first-run fires on the next launch. Used by
/// the "Run first-time setup…" command. Best-effort.
fn rearm_onboarding() {
    if let Some(path) = onboarding_marker() {
        let _ = std::fs::remove_file(path);
    }
}

/// Coarse CRT-glow presets the onboarding offers, mapped to the underlying
/// `glow_*` config knobs by [`Config::apply_glow_level`]. Shared by the
/// onboarding writer (which persists the choice) and the live-preview path in
/// `WindowState::apply_preview` (which mirrors it onto the running renderer), so the
/// preview can never drift from what actually gets saved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlowLevel {
    Off,
    Subtle,
    Full,
}

impl GlowLevel {
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "off" => Some(Self::Off),
            "subtle" => Some(Self::Subtle),
            "full" => Some(Self::Full),
            _ => None,
        }
    }
}

/// The single CRT-effect control the onboarding exposes: a coarse dial that
/// turns the bloom *and* the scanline overlay on together, since they read as
/// one "looks like an old monitor" effect to a new user. Mapped onto the
/// underlying glow / scanline knobs by [`Config::apply_crt_level`]; like
/// [`GlowLevel`] it's shared by the onboarding writer and the live-preview path
/// so the two stay in lock-step. Defaults to `Off`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrtLevel {
    Off,
    Low,
    High,
}

impl CrtLevel {
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "off" => Some(Self::Off),
            "low" => Some(Self::Low),
            "high" => Some(Self::High),
            _ => None,
        }
    }
}

/// The names of every color scheme available under `~/.config/yutani/schemes/`,
/// i.e. the file stems of the `.toml` files there, sorted alphabetically. These
/// are exactly the names `scheme_path` / `install_color_scheme` accept, so the
/// command palette's theme picker can only offer schemes that actually load.
/// A missing or unreadable directory yields an empty list.
fn list_scheme_names() -> Vec<String> {
    let mut dir = match config_dir() {
        Some(d) => d,
        None => return Vec::new(),
    };
    dir.push("schemes");
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut names: Vec<String> = entries
        .filter_map(|e| {
            let path = e.ok()?.path();
            if path.extension().and_then(|x| x.to_str()) != Some("toml") {
                return None;
            }
            path.file_stem()?.to_str().map(|s| s.to_string())
        })
        .collect();
    names.sort();
    names
}

/// Label for the synthetic theme-picker entry that reverts to the built-in
/// defaults (i.e. clears `color_scheme`). It carries spaces and parens so it
/// can't collide with a real `.toml` file stem; any on-disk scheme that somehow
/// matched it is filtered out (see [`theme_picker_choices`]).
const DEFAULT_THEME_LABEL: &str = "Default (built-in)";

/// The theme picker's full candidate list: the synthetic "Default (built-in)"
/// entry first, then the real schemes (with any collision against the label
/// dropped so the entry is unambiguous).
fn theme_picker_choices(scheme_names: Vec<String>) -> Vec<String> {
    let mut v = Vec::with_capacity(scheme_names.len() + 1);
    v.push(DEFAULT_THEME_LABEL.to_string());
    v.extend(scheme_names.into_iter().filter(|n| n != DEFAULT_THEME_LABEL));
    v
}

/// Resolve a theme-picker selection to the scheme name to install: `None` for
/// the synthetic default entry (revert to built-in defaults), otherwise the
/// label *is* the scheme name.
fn scheme_for_pick(label: &str) -> Option<&str> {
    (label != DEFAULT_THEME_LABEL).then_some(label)
}

/// Convert a theme-picker selection into the value to store in a config scheme
/// slot (`color_scheme` / `light_scheme` / `dark_scheme`). The synthetic
/// "Default (built-in)" entry and an empty/whitespace pick both map to `None`
/// (revert to defaults); any other label becomes the scheme name.
fn scheme_value_from_pick(arg: Option<String>) -> Option<String> {
    let arg = arg.unwrap_or_default();
    match scheme_for_pick(arg.trim()) {
        Some(name) if !name.is_empty() => Some(name.to_string()),
        _ => None,
    }
}

/// Byte size of the grid's vertex and index buffers for a viewport of
/// `cols × rows`. `(vertex_bytes, index_bytes)`. Single source of
/// truth so the `WindowState::new` and `resize_buffers` paths can't drift.
///
/// Capacity model: each cell emits two quads (background + glyph), so
/// `area = cols * rows` cells contribute `2 * area` quads. The slack
/// term covers the per-row content the renderer emits *outside* the
/// visible grid — `update_vertices` walks `r_lo..r_hi` where
/// `r_lo = -2, r_hi = rows + 2`, i.e. two phantom rows top + two
/// bottom. Each phantom row contributes up to `cols` cells × 2 quads
/// each, so the total phantom-row contribution is `4 * cols * 2 = 8 *
/// cols` quads worth of vertices. The `+5` covers the cursor quad
/// plus a handful of edge-fade and decorator overlays.
///
/// Expressed as `2 * (area + extra_quads)` where
/// `extra_quads = 4 * cols + 5` — i.e. one phantom-row strip per side
/// plus the fixed extras, doubled to cover both top and bottom strips.
fn grid_buffer_byte_sizes(cols: usize, rows: usize) -> (usize, usize) {
    let area = cols * rows;
    let extra_quads = 4 * cols + 5;
    let quads = 2 * area + 2 * extra_quads;
    let vertex_bytes = quads * std::mem::size_of::<renderer::vertex::Vertex>() * 4;
    let index_bytes = quads * std::mem::size_of::<u32>() * 6;
    (vertex_bytes, index_bytes)
}

/// Emit quads for `text` as a left-to-right monospace run starting at pixel
/// (`x`, `baseline_y`), advancing by `cell_w` per character. Pushes into the
/// same `vertices`/`indices` the grid uses, so it must be called within the FG
/// portion of `update_vertices` (after `num_bg_indices` is recorded) to draw on
/// top. Returns the final x advance.
///
/// Reuses the glyph-atlas lookup + bearing math from `emit_fg_for_cell`'s
/// ordinary (non-cell-filling) glyph path. We deliberately *duplicate* that
/// minimal math here rather than refactor the grid closure: the closure
/// captures `&self.atlas`/`scroll_y` and folds in box-drawing UV-clipping and
/// ligature substitution that don't apply to plain popup text, so factoring it
/// into a shared free fn would be a larger, riskier change. Callers must have
/// rasterized the glyphs into `atlas` beforehand (via `ensure_char`) so the
/// lookups hit.
///
/// Characters whose advance would push the glyph past `max_x` are skipped
/// (truncation); the run stops there.
#[allow(clippy::too_many_arguments)]
fn emit_text_run(
    atlas: &font::Atlas,
    vertices: &mut Vec<renderer::vertex::Vertex>,
    indices: &mut Vec<u32>,
    mut x: f32,
    baseline_y: f32,
    text: &str,
    color: [f32; 4],
    atlas_w: f32,
    atlas_h: f32,
    cell_w: f32,
    max_x: f32,
) -> f32 {
    for ch in text.chars() {
        // Truncate once the next cell would overflow the box.
        if x + cell_w > max_x {
            break;
        }
        if ch != ' ' {
            let g = atlas.lookup(ch, font::FaceVariant::Regular);
            if g.width > 0 && g.height > 0 {
                let bx = g.bearing_x as f32;
                let by = g.bearing_y as f32;
                let gx = x + bx;
                let gy = baseline_y - by;
                let gw = g.width as f32;
                let gh = g.height as f32;
                let u0 = g.x as f32 / atlas_w;
                let u1 = (g.x as f32 + gw) / atlas_w;
                let v0 = g.y as f32 / atlas_h;
                let v1 = (g.y as f32 + gh) / atlas_h;
                let start = vertices.len() as u32;
                let hx = gw * 0.5;
                let hy = gh * 0.5;
                let half_size = [hx, hy];
                vertices.push(renderer::vertex::Vertex {
                    position: [gx, gy, 0.0],
                    tex_coords: [u0, v0],
                    color,
                    local_pos: [-hx, -hy],
                    half_size,
                    radii: [0.0; 4],
                });
                vertices.push(renderer::vertex::Vertex {
                    position: [gx, gy + gh, 0.0],
                    tex_coords: [u0, v1],
                    color,
                    local_pos: [-hx, hy],
                    half_size,
                    radii: [0.0; 4],
                });
                vertices.push(renderer::vertex::Vertex {
                    position: [gx + gw, gy, 0.0],
                    tex_coords: [u1, v0],
                    color,
                    local_pos: [hx, -hy],
                    half_size,
                    radii: [0.0; 4],
                });
                vertices.push(renderer::vertex::Vertex {
                    position: [gx + gw, gy + gh, 0.0],
                    tex_coords: [u1, v1],
                    color,
                    local_pos: [hx, hy],
                    half_size,
                    radii: [0.0; 4],
                });
                indices.extend_from_slice(&[
                    start,
                    start + 1,
                    start + 2,
                    start + 1,
                    start + 2,
                    start + 3,
                ]);
            }
        }
        x += cell_w;
    }
    x
}

/// What the window does when the child shell exits. Configured via the
/// `shell_exit_mode` key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShellExitMode {
    /// Quit the app whenever the shell exits, regardless of status.
    Always,
    /// Never quit automatically; print a status line and keep the window
    /// open so the user can read the final output and dismiss it.
    Never,
    /// Quit on a clean (exit code 0) shell exit; otherwise keep the window
    /// open with a status line so a crash isn't silently swallowed.
    OnSuccess,
}

impl ShellExitMode {
    fn from_str(s: &str) -> Option<Self> {
        match s {
            "always" => Some(Self::Always),
            "never" => Some(Self::Never),
            "on_success" => Some(Self::OnSuccess),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Always => "always",
            Self::Never => "never",
            Self::OnSuccess => "on_success",
        }
    }
}

/// How the OSC 133 prompt-status indicator is drawn in the left margin.
/// Configured via the `prompt_gutter` key; off by default since not every
/// shell emits OSC 133 marks and the indicator is otherwise just noise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PromptGutter {
    /// No indicator (default).
    None,
    /// A short vertical bar per prompt — green on success, red on failure,
    /// dim while a command is still running.
    Bar,
}

impl PromptGutter {
    fn from_str(s: &str) -> Option<Self> {
        match s {
            "none" => Some(Self::None),
            "bar" => Some(Self::Bar),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Bar => "bar",
        }
    }
}

#[derive(Clone)]
struct Config {
    font_size: f32,
    top_fade_height: f32,
    top_fade_solid_stop: f32,
    top_fade_anim_secs: f32,
    bottom_fade_height: f32,
    bottom_fade_anim_secs: f32,
    /// Ease-in-out duration for cursor position changes. 0 disables the
    /// animation and the cursor snaps as before.
    cursor_anim_secs: f32,
    /// When false, the cursor never blinks regardless of what DECSCUSR
    /// requests. Defaults to false because steady cursors are easier on
    /// the eyes; opt back in for xterm-faithful behavior.
    cursor_blink: bool,
    /// Shared by top and bottom strips — both sample the same blur output.
    /// Per-edge would need a second blur chain.
    blur_iterations: usize,
    /// Name of a TOML scheme under ~/.config/yutani/schemes/. `None` keeps
    /// the built-in defaults; a missing file with `Some(_)` warns and falls
    /// back to defaults. Used as the active scheme when `auto_theme` is off,
    /// and as the fallback when an `auto_theme` slot below is unset.
    color_scheme: Option<String>,
    /// When true, follow the system light/dark appearance: install
    /// `light_scheme` in light mode and `dark_scheme` in dark mode, swapping
    /// live when the OS appearance changes. When false, `color_scheme` is used
    /// regardless of system appearance.
    auto_theme: bool,
    /// Scheme to use in system light mode while `auto_theme` is on. `None`
    /// falls back to `color_scheme`, then the built-in defaults.
    light_scheme: Option<String>,
    /// Scheme to use in system dark mode while `auto_theme` is on. `None`
    /// falls back to `color_scheme`, then the built-in defaults.
    dark_scheme: Option<String>,
    /// Preferred font family name. `None` (or empty) falls back to the
    /// built-in preference list (Iosevka Term → Iosevka → Fira Code → Menlo).
    /// Matched by exact family name first; failing that, by substring (so
    /// "Iosevka" picks up "Iosevka Term", etc.). A `Some(_)` value that
    /// doesn't match any installed family warns and falls back to the
    /// built-in list.
    font_family: Option<String>,
    /// When true, pixels whose HSV value (max channel) exceeds
    /// `glow_threshold` contribute to the glow. Brightness drives the
    /// glow intensity — brighter pixels bloom harder.
    glow_match_brightness: bool,
    /// When true, pixels whose HSV hue is within `glow_hue_tolerance_deg`
    /// of one of the colour scheme's 8 bright ANSI variants contribute
    /// to the glow. Matched on hue only so antialiased glyphs (which blend
    /// toward the background) still register.
    glow_match_bright_ansi: bool,
    /// HSV-value (brightness) cutoff for `glow_match_brightness` mode.
    glow_threshold: f32,
    /// Additive composite multiplier; 1.0 leaves the glow at original colour
    /// intensity, higher values bloom harder.
    glow_intensity: f32,
    /// Width of the smoothstep band above `glow_threshold` (and the hue
    /// tolerance for bright-ANSI mode). Larger values give a softer cutoff.
    glow_softness: f32,
    /// Degrees of hue slop allowed by `glow_match_bright_ansi`. Default 18°
    /// covers small palette drift; larger values catch tinted variants.
    glow_hue_tolerance_deg: f32,
    /// Dual-Kawase iterations applied to the bright extraction. Higher =
    /// wider, softer halo at the cost of fill rate.
    glow_iterations: usize,
    /// When true, pixels whose RGB distance to the palette's foreground
    /// colour is within `glow_fg_tolerance` contribute to the glow. The
    /// only mode that catches achromatic default text.
    glow_match_foreground: bool,
    /// RGB Euclidean radius around the foreground colour for
    /// `glow_match_foreground`. Max meaningful value is √3 ≈ 1.73.
    glow_fg_tolerance: f32,
    /// When true, the glow composite applies a CRT-style scanline
    /// knockout: alternating horizontal rows of the halo are alpha'd to
    /// zero. Affects both bg and fg glow layers.
    glow_scanlines: bool,
    /// 0 = no scanline effect, 1 = full knockout on dark rows. Clamped.
    glow_scanline_strength: f32,
    /// Scanline cycle in framebuffer pixels (half dark, half bright).
    /// 4 = 2px dark + 2px bright. On Retina that's ~1 logical-pixel
    /// alternation. Clamped to >= 1.
    glow_scanline_period: f32,
    /// When true, the scanline pattern is *also* applied (multiply
    /// blend) over all rendered content — bg colors and glyphs — not
    /// just the glow halo. Shares the period with `glow_scanlines` but
    /// has its own strength so each layer can be tuned separately.
    glow_scanlines_content: bool,
    /// Strength of the content-overlay scanlines. 0 = invisible,
    /// 1 = dark rows fully knocked to black. Clamped.
    glow_scanlines_content_strength: f32,
    /// RGBA multiplier for bright scan rows of the content overlay.
    /// Default white = no change. Tinted values colour the bright
    /// stripes (e.g. slight cyan/amber for a CRT phosphor look).
    /// Parsed from a `0xRRGGBB` literal in the config; alpha = 1.
    glow_scanline_color_bright: [f32; 4],
    /// RGBA multiplier for dark scan rows. Default black = full
    /// knockout. Non-black values let dark stripes show a dim colour
    /// instead of going pitch black.
    glow_scanline_color_dark: [f32; 4],
    /// When true, the content overlay is suppressed wherever the bg
    /// scene pixel matches the window's primary background colour —
    /// scanlines fade out over empty areas of the terminal and only
    /// appear over colored cells, glyphs, and glow. Only effective in
    /// the layered / strip-only render paths (which have a bg scene
    /// to sample); fast-path frames render the overlay uniformly.
    glow_scanlines_skip_primary_bg: bool,
    /// 0..1 amount to soften the masked content overlay over any
    /// drawn pixel — colored bg cells AND glyphs alike. 0 = full
    /// strength on all content; 1 = no overlay on drawn content
    /// (only empty terminal area gets scanlines). Has no effect when
    /// `glow_scanlines_skip_primary_bg` is off.
    glow_scanlines_content_attenuation: f32,
    /// When true, any `glow_*` override declared by the active color scheme
    /// (see `palette::GlowOverrides`) wins over the matching field in this
    /// `Config`. When false (the default), config values always win and
    /// scheme overrides are ignored — preserves prior behavior for users
    /// whose schemes happen to carry stray glow keys.
    theme_overrides_glow: bool,
    /// Master switch for the image-placement feature. When false, the
    /// Cmd-Shift-I keybind and the `YUTANI_TEST_IMAGE` startup hook
    /// silently no-op, and image-protocol payloads (once parsers land
    /// in phase 2) will be discarded. The render pipeline itself stays
    /// loaded — there's no measurable cost when no placements exist.
    images_enabled: bool,
    /// Hard upper bound on the total bytes the image `Store` holds. New
    /// uploads past this cap are refused (logged as BudgetExceeded);
    /// mark-and-sweep keeps actually-used images alive. 0 effectively
    /// disables image residency. Stored as megabytes so the config file
    /// stays readable.
    images_memory_cap_mb: usize,
    /// Per-decode rejection threshold. A header-check inside
    /// `decode_to_rgba` fires before pixel allocation, so a malicious
    /// 100MB-pixel PNG is rejected without touching memory. Default
    /// 16M pixels = 4096×4096 — fits any sane screenshot.
    images_max_pixels: u64,
    /// Per-decode deadline. The worker can't be interrupted mid-decode,
    /// but a pending request older than this is dropped on the main
    /// thread and the late result is discarded. Protects against the
    /// queue backing up when a parser-driven payload hits a slow path.
    images_decode_timeout_ms: u64,
    /// When false, placements that fully scroll off the top of the
    /// primary grid are dropped instead of promoted to scrollback. Saves
    /// memory in long-running shells with lots of image traffic; the
    /// trade-off is that scrolling back into history doesn't recover the
    /// image. (Scrollback rendering of placements isn't implemented in
    /// phase 1 anyway, so the toggle is mostly about retention cost
    /// today.)
    images_in_scrollback: bool,
    /// Sampler choice for new image uploads: "linear" smooths under
    /// scaling, "nearest" preserves crisp pixel edges (useful for
    /// pixel-art / sprite content). Existing GpuImages keep whichever
    /// sampler their bind group was built with — a flip applies to
    /// subsequent decodes only.
    images_filter: String,
    /// When the GPU image draw is unavailable for a placement — either
    /// `images_enabled = false` or the placement's decode failed in a
    /// case main.rs hasn't yet cleaned up — render the placement as
    /// Unicode half-block (▀) glyphs against the cell grid using the
    /// preview cached in `images::Store`. Off by default: the in-flight
    /// decode case (which would flicker) is *never* covered; only the
    /// fully-disabled / fully-failed cases are.
    images_halfblock_for_missing: bool,
    /// What happens to the window when the child shell exits. Defaults to
    /// `OnSuccess`: a clean exit closes the window, a non-zero/abnormal
    /// exit keeps it open with a status line. See `ShellExitMode`.
    shell_exit_mode: ShellExitMode,
    /// OSC 133 prompt-status gutter indicator. Off by default.
    prompt_gutter: PromptGutter,
    /// Whether the filesystem/history autocomplete popup is shown as you
    /// type. Default on; set `autocomplete = false` to disable.
    autocomplete: bool,
}

impl Config {
    fn defaults() -> Self {
        Self {
            font_size: DEFAULT_FONT_SIZE,
            top_fade_height: DECORATOR_HEIGHT * 3.0,
            top_fade_solid_stop: 0.5,
            top_fade_anim_secs: 0.36,
            bottom_fade_height: DECORATOR_HEIGHT * 2.0,
            bottom_fade_anim_secs: 0.36,
            cursor_anim_secs: 0.06,
            cursor_blink: false,
            blur_iterations: 2,
            color_scheme: None,
            auto_theme: false,
            light_scheme: None,
            dark_scheme: None,
            font_family: None,
            glow_match_brightness: false,
            glow_match_bright_ansi: false,
            glow_threshold: renderer::glow::DEFAULT_THRESHOLD,
            glow_intensity: renderer::glow::DEFAULT_INTENSITY,
            glow_softness: renderer::glow::DEFAULT_SOFTNESS,
            glow_hue_tolerance_deg: renderer::glow::DEFAULT_HUE_TOLERANCE_DEG,
            glow_iterations: 2,
            glow_match_foreground: false,
            glow_fg_tolerance: renderer::glow::DEFAULT_FG_TOLERANCE,
            glow_scanlines: false,
            glow_scanline_strength: renderer::glow::DEFAULT_SCANLINE_STRENGTH,
            glow_scanline_period: renderer::glow::DEFAULT_SCANLINE_PERIOD,
            glow_scanlines_content: false,
            glow_scanlines_content_strength: renderer::glow::DEFAULT_SCANLINE_STRENGTH,
            glow_scanline_color_bright: renderer::glow::DEFAULT_SCANLINE_COLOR_BRIGHT,
            glow_scanline_color_dark: renderer::glow::DEFAULT_SCANLINE_COLOR_DARK,
            glow_scanlines_skip_primary_bg: false,
            glow_scanlines_content_attenuation: renderer::glow::DEFAULT_CONTENT_SCANLINE_ATTENUATION,
            theme_overrides_glow: false,
            images_enabled: true,
            images_memory_cap_mb: images::DEFAULT_CAP_BYTES / (1024 * 1024),
            images_max_pixels: 16 * 1024 * 1024,
            images_decode_timeout_ms: 2000,
            images_in_scrollback: true,
            images_filter: "linear".to_string(),
            images_halfblock_for_missing: false,
            shell_exit_mode: ShellExitMode::OnSuccess,
            prompt_gutter: PromptGutter::None,
            autocomplete: true,
        }
    }

    fn load() -> Self {
        let Some(p) = config_path() else { return Self::defaults() };
        let Ok(s) = std::fs::read_to_string(p) else { return Self::defaults() };
        Self::parse_str(&s)
    }

    fn parse_str(s: &str) -> Self {
        let mut c = Self::defaults();
        let table: toml::Table = match s.parse() {
            Ok(t) => t,
            Err(e) => {
                eprintln!("config: invalid TOML: {}", e);
                return c;
            }
        };
        for (k, v) in &table {
            c.apply(k, v);
        }
        c
    }

    /// Apply one parsed TOML key/value onto `self`. A value of the wrong
    /// type or out of range is silently skipped, so one bad key doesn't
    /// drop the rest of the file — the same forgiving contract the old
    /// line-based parser had, now at the value level. (A TOML *syntax*
    /// error still reverts the whole file to defaults, up in `parse_str`,
    /// since the document can't be walked key-by-key.)
    fn apply(&mut self, k: &str, v: &toml::Value) {
        match k {
            "font_size" => if let Some(x) = cfg_f32(v) { self.font_size = x; },
            "top_fade_height" => if let Some(x) = cfg_f32(v) { self.top_fade_height = x; },
            "top_fade_solid_stop" => if let Some(x) = cfg_f32(v) { self.top_fade_solid_stop = x; },
            "top_fade_anim_secs" => if let Some(x) = cfg_f32(v) { self.top_fade_anim_secs = x; },
            "bottom_fade_height" => if let Some(x) = cfg_f32(v) { self.bottom_fade_height = x; },
            "bottom_fade_anim_secs" => if let Some(x) = cfg_f32(v) { self.bottom_fade_anim_secs = x; },
            "cursor_anim_secs" => if let Some(x) = cfg_f32(v) { self.cursor_anim_secs = x; },
            "cursor_blink" => if let Some(x) = v.as_bool() { self.cursor_blink = x; },
            "blur_iterations" => if let Some(x) = cfg_usize(v) {
                self.blur_iterations = x.min(renderer::blur::MAX_BLUR_ITERATIONS);
            },
            "color_scheme" => if let Some(x) = v.as_str() {
                self.color_scheme = if x.is_empty() { None } else { Some(x.to_string()) };
            },
            "auto_theme" => if let Some(x) = v.as_bool() { self.auto_theme = x; },
            "light_scheme" => if let Some(x) = v.as_str() {
                self.light_scheme = if x.is_empty() { None } else { Some(x.to_string()) };
            },
            "dark_scheme" => if let Some(x) = v.as_str() {
                self.dark_scheme = if x.is_empty() { None } else { Some(x.to_string()) };
            },
            "font_family" => if let Some(x) = v.as_str() {
                self.font_family = if x.is_empty() { None } else { Some(x.to_string()) };
            },
            "glow_match_brightness" => if let Some(x) = v.as_bool() { self.glow_match_brightness = x; },
            "glow_match_bright_ansi" => if let Some(x) = v.as_bool() { self.glow_match_bright_ansi = x; },
            "glow_threshold" => if let Some(x) = cfg_f32(v) {
                self.glow_threshold = x.clamp(0.0, 1.0);
            },
            "glow_intensity" => if let Some(x) = cfg_f32(v) {
                self.glow_intensity = x.max(0.0);
            },
            "glow_softness" => if let Some(x) = cfg_f32(v) {
                self.glow_softness = x.clamp(0.0, 1.0);
            },
            "glow_hue_tolerance_deg" => if let Some(x) = cfg_f32(v) {
                self.glow_hue_tolerance_deg = x.clamp(0.0, 180.0);
            },
            "glow_iterations" => if let Some(x) = cfg_usize(v) {
                self.glow_iterations = x.clamp(1, renderer::glow::MAX_ITERATIONS);
            },
            "glow_match_foreground" => if let Some(x) = v.as_bool() { self.glow_match_foreground = x; },
            "glow_fg_tolerance" => if let Some(x) = cfg_f32(v) {
                self.glow_fg_tolerance = x.clamp(0.0, 3.0_f32.sqrt());
            },
            "glow_scanlines" => if let Some(x) = v.as_bool() { self.glow_scanlines = x; },
            "glow_scanline_strength" => if let Some(x) = cfg_f32(v) {
                self.glow_scanline_strength = x.clamp(0.0, 1.0);
            },
            "glow_scanline_period" => if let Some(x) = cfg_f32(v) {
                self.glow_scanline_period = x.max(1.0);
            },
            "glow_scanlines_content" => if let Some(x) = v.as_bool() { self.glow_scanlines_content = x; },
            "glow_scanlines_content_strength" => if let Some(x) = cfg_f32(v) {
                self.glow_scanlines_content_strength = x.clamp(0.0, 1.0);
            },
            "glow_scanline_color_bright" => if let Ok(x) = palette::rgb_from_value(v) {
                self.glow_scanline_color_bright = x;
            },
            "glow_scanline_color_dark" => if let Ok(x) = palette::rgb_from_value(v) {
                self.glow_scanline_color_dark = x;
            },
            "glow_scanlines_skip_primary_bg" => if let Some(x) = v.as_bool() {
                self.glow_scanlines_skip_primary_bg = x;
            },
            "glow_scanlines_content_attenuation" => if let Some(x) = cfg_f32(v) {
                self.glow_scanlines_content_attenuation = x.clamp(0.0, 1.0);
            },
            "theme_overrides_glow" => if let Some(x) = v.as_bool() { self.theme_overrides_glow = x; },
            "images_enabled" => if let Some(x) = v.as_bool() { self.images_enabled = x; },
            "images_memory_cap_mb" => if let Some(x) = cfg_usize(v) {
                self.images_memory_cap_mb = x;
            },
            "images_max_pixels" => if let Some(x) = cfg_u64(v) {
                // Cap at u32::MAX^2 wouldn't fit a meaningful image
                // anyway; just protect against zero by clamping low.
                self.images_max_pixels = x.max(1);
            },
            "images_decode_timeout_ms" => if let Some(x) = cfg_u64(v) {
                // 50ms floor — anything lower defeats the worker since
                // even a tiny PNG decode takes a millisecond or two.
                self.images_decode_timeout_ms = x.max(50);
            },
            "images_in_scrollback" => if let Some(x) = v.as_bool() {
                self.images_in_scrollback = x;
            },
            "images_filter" => if let Some(x) = v.as_str() {
                if x == "linear" || x == "nearest" {
                    self.images_filter = x.to_string();
                }
                // Silently keep the default on unknown values — same
                // contract as the `color_scheme` slot above.
            },
            "images_halfblock_for_missing" => if let Some(x) = v.as_bool() {
                self.images_halfblock_for_missing = x;
            },
            "shell_exit_mode" => if let Some(x) = v.as_str() {
                if let Some(m) = ShellExitMode::from_str(x) {
                    self.shell_exit_mode = m;
                }
                // Silently keep the default on an unknown value — same
                // forgiving contract as `images_filter`.
            },
            "prompt_gutter" => if let Some(x) = v.as_str() {
                if let Some(g) = PromptGutter::from_str(x) {
                    self.prompt_gutter = g;
                }
            },
            "autocomplete" => if let Some(x) = v.as_bool() { self.autocomplete = x; },
            _ => (),
        }
    }

    fn save(&self) {
        let Some(p) = config_path() else { return };
        if let Some(parent) = p.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(p, self.serialize());
    }

    /// The scheme name to install for the current system appearance, or `None`
    /// to use the built-in defaults. When `auto_theme` is off this is just
    /// `color_scheme`. When on, it's the `dark_scheme` / `light_scheme` slot for
    /// `dark`, falling back to `color_scheme` if that slot is unset — so a user
    /// can configure only one slot and keep their existing scheme for the other.
    fn active_scheme(&self, dark: bool) -> Option<&str> {
        if !self.auto_theme {
            return self.color_scheme.as_deref();
        }
        let slot = if dark { &self.dark_scheme } else { &self.light_scheme };
        slot.as_deref().or(self.color_scheme.as_deref())
    }

    fn serialize(&self) -> String {
        let mut s = String::from("# Yutani configuration\n\n");
        s.push_str(&format!(
            "# Display\n\
             font_size = {}\n\
             top_fade_height = {}\n\
             top_fade_solid_stop = {}\n\
             top_fade_anim_secs = {}\n\
             bottom_fade_height = {}\n\
             bottom_fade_anim_secs = {}\n\
             cursor_anim_secs = {}\n\
             cursor_blink = {}\n\
             blur_iterations = {}\n",
            self.font_size,
            self.top_fade_height,
            self.top_fade_solid_stop,
            self.top_fade_anim_secs,
            self.bottom_fade_height,
            self.bottom_fade_anim_secs,
            self.cursor_anim_secs,
            self.cursor_blink,
            self.blur_iterations,
        ));
        if let Some(name) = &self.color_scheme {
            s.push_str(&format!("color_scheme = {}\n", toml_str_lit(name)));
        }
        s.push_str(&format!("auto_theme = {}\n", self.auto_theme));
        if let Some(name) = &self.light_scheme {
            s.push_str(&format!("light_scheme = {}\n", toml_str_lit(name)));
        }
        if let Some(name) = &self.dark_scheme {
            s.push_str(&format!("dark_scheme = {}\n", toml_str_lit(name)));
        }
        if let Some(name) = &self.font_family {
            s.push_str(&format!("font_family = {}\n", toml_str_lit(name)));
        }
        s.push_str(&format!(
            "\n# Glow + scanlines\n\
             glow_match_brightness = {}\n\
             glow_match_bright_ansi = {}\n\
             glow_match_foreground = {}\n\
             glow_threshold = {}\n\
             glow_intensity = {}\n\
             glow_softness = {}\n\
             glow_hue_tolerance_deg = {}\n\
             glow_fg_tolerance = {}\n\
             glow_iterations = {}\n\
             glow_scanlines = {}\n\
             glow_scanline_strength = {}\n\
             glow_scanline_period = {}\n\
             glow_scanlines_content = {}\n\
             glow_scanlines_content_strength = {}\n\
             glow_scanline_color_bright = {}\n\
             glow_scanline_color_dark = {}\n\
             glow_scanlines_skip_primary_bg = {}\n\
             glow_scanlines_content_attenuation = {}\n\
             theme_overrides_glow = {}\n",
            self.glow_match_brightness,
            self.glow_match_bright_ansi,
            self.glow_match_foreground,
            self.glow_threshold,
            self.glow_intensity,
            self.glow_softness,
            self.glow_hue_tolerance_deg,
            self.glow_fg_tolerance,
            self.glow_iterations,
            self.glow_scanlines,
            self.glow_scanline_strength,
            self.glow_scanline_period,
            self.glow_scanlines_content,
            self.glow_scanlines_content_strength,
            format_hex_rgb(self.glow_scanline_color_bright),
            format_hex_rgb(self.glow_scanline_color_dark),
            self.glow_scanlines_skip_primary_bg,
            self.glow_scanlines_content_attenuation,
            self.theme_overrides_glow,
        ));
        s.push_str(&format!(
            "\n# Images\n\
             images_enabled = {}\n\
             images_memory_cap_mb = {}\n\
             images_max_pixels = {}\n\
             images_decode_timeout_ms = {}\n\
             images_in_scrollback = {}\n\
             images_filter = {}\n\
             images_halfblock_for_missing = {}\n",
            self.images_enabled,
            self.images_memory_cap_mb,
            self.images_max_pixels,
            self.images_decode_timeout_ms,
            self.images_in_scrollback,
            toml_str_lit(&self.images_filter),
            self.images_halfblock_for_missing,
        ));
        s.push_str(&format!(
            "\n# Shell\n\
             # shell_exit_mode: \"always\" | \"never\" | \"on_success\"\n\
             shell_exit_mode = {}\n",
            toml_str_lit(self.shell_exit_mode.as_str()),
        ));
        s.push_str(&format!(
            "\n# Shell integration (OSC 133)\n\
             # prompt_gutter: \"none\" | \"bar\"\n\
             prompt_gutter = {}\n",
            toml_str_lit(self.prompt_gutter.as_str()),
        ));
        s.push_str(&format!(
            "\n# Completion\n\
             # autocomplete: show the filesystem/history popup as you type\n\
             autocomplete = {}\n",
            self.autocomplete,
        ));
        s
    }

    /// Map a coarse [`GlowLevel`] onto the fine-grained `glow_*` knobs. The one
    /// place the preset→knob translation lives, so the onboarding's persisted
    /// config and its live preview stay in lock-step. Only the matching-mode
    /// and intensity fields are touched; scanlines and per-scheme overrides are
    /// left to their own setters / the config defaults.
    pub fn apply_glow_level(&mut self, level: GlowLevel) {
        match level {
            GlowLevel::Off => {
                self.glow_match_foreground = false;
                self.glow_match_brightness = false;
                self.glow_match_bright_ansi = false;
            }
            GlowLevel::Subtle => {
                // Foreground match catches achromatic default text; brightness
                // match catches anything light. Dialed-back intensity for a
                // gentle bloom.
                self.glow_match_foreground = true;
                self.glow_match_brightness = true;
                self.glow_match_bright_ansi = false;
                self.glow_intensity = 0.6;
            }
            GlowLevel::Full => {
                self.glow_match_foreground = true;
                self.glow_match_brightness = true;
                self.glow_match_bright_ansi = true;
                self.glow_intensity = 1.0;
            }
        }
    }

    /// Toggle the CRT scanline knockout on the glow halo (`glow_scanlines`).
    /// Whether scanlines *also* lay over the text and background is governed by
    /// the separate `glow_scanlines_content` config setting (off by default);
    /// the onboarding leaves that to the config so it isn't overridden here.
    pub fn apply_scanlines(&mut self, on: bool) {
        self.glow_scanlines = on;
    }

    /// Apply a [`CrtLevel`] — the onboarding's one-dial retro-monitor look,
    /// bloom + halo scanlines together. The presets are tuned off the
    /// `spacedust` theme's glow settings, which read as a sensible "this is
    /// what a CRT looks like" default: `High` mirrors them verbatim, `Low`
    /// dials the bloom and scanline strength back, and `Off` disables the
    /// effect. We set the underlying glow knobs directly (rather than via the
    /// coarse [`apply_glow_level`]) so the values are exactly the theme's.
    ///
    /// The content-overlay scanlines (`glow_scanlines_content`) and the
    /// "let the active scheme's glow win" switch (`theme_overrides_glow`) are
    /// deliberately left to their config defaults (both off) — these presets
    /// are concrete config values, not a hand-off to the theme.
    pub fn apply_crt_level(&mut self, level: CrtLevel) {
        // Match mode is the same across the on presets: bloom driven by
        // brightness, matching the spacedust theme.
        match level {
            CrtLevel::Off => {
                self.glow_match_brightness = false;
                self.glow_match_bright_ansi = false;
                self.glow_match_foreground = false;
                self.glow_scanlines = false;
            }
            CrtLevel::Low => {
                self.glow_match_brightness = true;
                self.glow_match_bright_ansi = false;
                self.glow_match_foreground = false;
                self.glow_threshold = 0.0;
                self.glow_intensity = 0.3;
                self.glow_softness = 1.0;
                self.glow_iterations = 2;
                self.glow_scanlines = true;
                // Full-strength knockout, same as High — "low" only dials back
                // the bloom, not the scanlines.
                self.glow_scanline_strength = 1.0;
                self.glow_scanline_period = 8.0;
            }
            CrtLevel::High => {
                // spacedust verbatim.
                self.glow_match_brightness = true;
                self.glow_match_bright_ansi = false;
                self.glow_match_foreground = false;
                self.glow_threshold = 0.0;
                self.glow_intensity = 0.6;
                self.glow_softness = 1.0;
                self.glow_iterations = 4;
                self.glow_scanlines = true;
                self.glow_scanline_strength = 1.0;
                self.glow_scanline_period = 8.0;
            }
        }
    }
}

/// Read a numeric config slot, accepting either a TOML float or a bare
/// integer (`font_size = 10` and `font_size = 10.0` both work). `None` if
/// the value isn't a number, so the caller keeps the default.
fn cfg_f32(v: &toml::Value) -> Option<f32> {
    v.as_float()
        .map(|f| f as f32)
        .or_else(|| v.as_integer().map(|i| i as f32))
}

fn cfg_usize(v: &toml::Value) -> Option<usize> {
    v.as_integer().and_then(|i| usize::try_from(i).ok())
}

fn cfg_u64(v: &toml::Value) -> Option<u64> {
    v.as_integer().and_then(|i| u64::try_from(i).ok())
}

/// Encode a string as a TOML basic-string literal (quoted, with escapes),
/// so values like a font family with spaces round-trip through `parse_str`.
fn toml_str_lit(s: &str) -> String {
    toml::Value::String(s.to_string()).to_string()
}

/// Copy every `glow_*` slot from `config` onto a `Glow` instance, letting
/// the active color scheme's `GlowOverrides` win when
/// `config.theme_overrides_glow` is set. Used by both initial construction
/// and Cmd-Shift-R reload, so changing a glow knob in the config — or
/// switching to a scheme that carries its own glow keys — takes effect
/// live. `glow_iterations` is clamped to `[1, MAX_ITERATIONS]` to keep the
/// dual-Kawase chain bounded — the same clamp that `Config::parse_str`
/// applies, repeated here so a hand-mutated `Config` or a scheme that
/// asked for too many iterations can't push past the limit.
fn apply_glow_config(
    g: &mut renderer::glow::Glow,
    config: &Config,
    overrides: &palette::GlowOverrides,
) {
    fn pick<T>(theme_wins: bool, scheme: Option<T>, cfg: T) -> T {
        if theme_wins { scheme.unwrap_or(cfg) } else { cfg }
    }
    let t = config.theme_overrides_glow;
    g.match_brightness = pick(t, overrides.match_brightness, config.glow_match_brightness);
    g.match_bright_ansi = pick(t, overrides.match_bright_ansi, config.glow_match_bright_ansi);
    g.match_foreground = pick(t, overrides.match_foreground, config.glow_match_foreground);
    g.threshold = pick(t, overrides.threshold, config.glow_threshold);
    g.intensity = pick(t, overrides.intensity, config.glow_intensity);
    g.softness = pick(t, overrides.softness, config.glow_softness);
    g.hue_tolerance = pick(t, overrides.hue_tolerance_deg, config.glow_hue_tolerance_deg);
    g.fg_tolerance = pick(t, overrides.fg_tolerance, config.glow_fg_tolerance);
    g.match_scanlines = pick(t, overrides.scanlines, config.glow_scanlines);
    g.scanline_strength = pick(t, overrides.scanline_strength, config.glow_scanline_strength);
    g.scanline_period = pick(t, overrides.scanline_period, config.glow_scanline_period);
    g.match_content_scanlines = pick(t, overrides.scanlines_content, config.glow_scanlines_content);
    g.content_scanline_strength = pick(
        t,
        overrides.scanlines_content_strength,
        config.glow_scanlines_content_strength,
    );
    g.scanline_color_bright = pick(
        t,
        overrides.scanline_color_bright,
        config.glow_scanline_color_bright,
    );
    g.scanline_color_dark = pick(
        t,
        overrides.scanline_color_dark,
        config.glow_scanline_color_dark,
    );
    g.content_scanline_attenuation = pick(
        t,
        overrides.scanlines_content_attenuation,
        config.glow_scanlines_content_attenuation,
    );
    let iterations = pick(t, overrides.iterations, config.glow_iterations);
    g.iterations = iterations.clamp(1, renderer::glow::MAX_ITERATIONS);
}

/// Resolve `glow_scanlines_skip_primary_bg` against the active scheme's
/// override, honouring the `theme_overrides_glow` tiebreaker. Used at draw
/// time because this slot is read straight off `Config` rather than mirrored
/// onto the `Glow` instance (it selects between two render pipelines).
fn effective_skip_primary_bg(config: &Config, overrides: &palette::GlowOverrides) -> bool {
    if config.theme_overrides_glow {
        overrides
            .scanlines_skip_primary_bg
            .unwrap_or(config.glow_scanlines_skip_primary_bg)
    } else {
        config.glow_scanlines_skip_primary_bg
    }
}

pub struct ViewportSize {
    char_width: usize,
    char_height: usize,
}

/// Minimal offscreen render target — texture + view + dims. Used by the
/// layered glow path for the FG scene; the BG scene lives inside
/// `BlurChain` already.
struct SceneTarget {
    _tex: wgpu::Texture,
    view: wgpu::TextureView,
}

impl SceneTarget {
    fn new(
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        width: u32,
        height: u32,
        label: &str,
    ) -> Self {
        let tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d {
                width: width.max(1),
                height: height.max(1),
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
        Self { _tex: tex, view }
    }
}

/// Where an in-flight image decode is supposed to land once the worker
/// thread finishes. Two modes:
///
/// - **Deferred (Cmd-Shift-I path):** `preplaced_image_id == None`. The
///   placement hasn't been created yet; `poll_pending_images` computes
///   cell extent from the image's pixel size and calls
///   `Terminal::insert_placement` on success. Failure just logs.
///
/// - **Pre-placed (OSC 1337 path):** `preplaced_image_id == Some(id)`. The
///   placement is already in `Grid::placements` because the OSC handler
///   needed to advance the cursor synchronously. Success is a no-op (the
///   renderer's next `peek` finds the freshly-uploaded pixels); failure
///   calls `Terminal::remove_placements_with_image` to drop the orphan.
struct PendingImagePlacement {
    request: images::PendingId,
    row: isize,
    col: isize,
    preplaced_image_id: Option<images::ImageId>,
}

/// Whether `poll_pending_images` should re-arm a redraw so the render
/// loop keeps ticking until every in-flight decode resolves.
///
/// Three independent reasons to keep going:
///   - `pending_placements` still has deferred placements (Cmd-Shift-I
///     paste, OSC 1337) waiting on their decode,
///   - this poll produced `results` to act on, or
///   - the store still holds undrained decodes (`store_pending > 0`).
///
/// The last one is load-bearing for the Kitty Unicode-placeholder path
/// (`a=T,U=1`, what `icat` emits under tmux) and animation frames
/// (`a=f`): those bump the store's queue without ever touching
/// `pending_placements`. Omitting it lets a single-burst transmit whose
/// decode lands after this frame's poll stall the loop, leaving the
/// image blank until an unrelated event wakes it.
fn should_rearm_image_poll(
    pending_placements_empty: bool,
    results_empty: bool,
    store_pending: usize,
) -> bool {
    !pending_placements_empty || !results_empty || store_pending > 0
}

/// Resources shared by every window in the process: the GPU device/queue, the
/// font stack, and the pipelines/layouts that depend only on the device (and
/// the shared surface format). Built once at startup; future windows borrow
/// this instead of re-initializing the adapter, re-loading fonts, or
/// recompiling shaders. Single-threaded (the winit event loop), so the
/// not-`Send` font lives behind `Rc<RefCell>` rather than `Arc<Mutex>`.
struct AppShared {
    gpu: Rc<gpu::Gpu>,
    /// FreeType faces. `Rc<RefCell>` because faces aren't `Send` but every
    /// window lives on the one event-loop thread. Borrow discipline is
    /// load-bearing: read through [`AppShared::with_font`], and fill via a
    /// single-statement `atlas.ensure_*(&mut shared.font.borrow_mut(), …)` so
    /// no `Ref`/`RefMut` is held across the overlapping borrow in
    /// `update_vertices` (which would panic at runtime — and the test suite,
    /// using `Font` directly, wouldn't catch it).
    font: Rc<RefCell<font::Font>>,
    /// rustybuzz shaper, used during update_vertices to detect programming
    /// ligatures (`->`, `=>`, `!=`, …) so the renderer can draw them as a
    /// single wide glyph instead of two adjacent characters. Read-only after
    /// startup; shared like the font.
    shaper: Rc<RefCell<shaper::Shaper>>,
    render_pipeline: wgpu::RenderPipeline,
    /// Wireframe debug pipeline — same vertex shader but PolygonMode::Line
    /// and a flat-color fragment. `None` if the adapter doesn't expose
    /// POLYGON_MODE_LINE; the toggle becomes a no-op there.
    wireframe_pipeline: Option<wgpu::RenderPipeline>,
    /// Layout for the font texture + sampler. Kept so each window can build
    /// its own `font_bind_group` (and rebind after a font-size change).
    font_bind_group_layout: wgpu::BindGroupLayout,
    /// Layouts shared by the render pipeline (above) and each window's
    /// per-window camera / fade bind groups, so a window can build those
    /// against the same layout the pipeline expects.
    camera_bind_group_layout: wgpu::BindGroupLayout,
    fade_bind_group_layout: wgpu::BindGroupLayout,
    /// Dual-Kawase blur pipelines (shader compiled once per process). The
    /// per-window textures/bind-groups live in `WindowState::blur`.
    blur_pipelines: renderer::blur::BlurPipelines,
    /// Glow/bloom pipelines (shared by both the bg and fg `Glow` instances of
    /// every window). The per-window/per-layer resources live in `WindowState::glow`
    /// / `WindowState::glow_fg`.
    glow_pipelines: renderer::glow::GlowPipelines,
}

impl AppShared {
    /// Build the once-per-process resources: device/queue (already created),
    /// the font stack, the bind-group layouts, and every shader pipeline that
    /// depends only on the device + surface format. Windows are then built by
    /// [`WindowState::create_window`] against the returned `Rc<AppShared>`.
    fn new(
        gpu: gpu::Gpu,
        surface_format: wgpu::TextureFormat,
        font: font::Font,
        shaper: shaper::Shaper,
    ) -> Self {
        let gpu = Rc::new(gpu);

        let font_bind_group_layout =
            gpu.device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            multisampled: false,
                            view_dimension: wgpu::TextureViewDimension::D2,
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                ],
                label: Some("font texture bind group layout"),
            });

        let camera_bind_group_layout =
            gpu.device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
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
                label: Some("camera bind group layout"),
            });

        let fade_bind_group_layout =
            gpu.device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                }],
                label: Some("fade bind group layout"),
            });

        let shader = gpu
            .device
            .create_shader_module(wgpu::include_wgsl!("renderer/shader.wgsl"));

        let render_pipeline_layout =
            gpu.device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("render pipeline layout"),
                bind_group_layouts: &[
                    &font_bind_group_layout,
                    &camera_bind_group_layout,
                    &fade_bind_group_layout,
                ],
                push_constant_ranges: &[],
            });

        let render_pipeline = gpu.device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("render pipeline"),
            layout: Some(&render_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: "vs_main",
                buffers: &[renderer::vertex::Vertex::desc()],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: "fs_main",
                targets: &[Some(wgpu::ColorTargetState {
                    format: surface_format,
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Cw,
                cull_mode: None,
                polygon_mode: wgpu::PolygonMode::Fill,
                unclipped_depth: false,
                conservative: false,
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState {
                count: 1,
                mask: !0,
                alpha_to_coverage_enabled: false,
            },
            multiview: None,
        });

        let wireframe_pipeline = if gpu
            .device
            .features()
            .contains(wgpu::Features::POLYGON_MODE_LINE)
        {
            Some(gpu.device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("wireframe pipeline"),
                layout: Some(&render_pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: "vs_main",
                    buffers: &[renderer::vertex::Vertex::desc()],
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: "fs_wire",
                    targets: &[Some(wgpu::ColorTargetState {
                        format: surface_format,
                        blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    strip_index_format: None,
                    front_face: wgpu::FrontFace::Cw,
                    cull_mode: None,
                    polygon_mode: wgpu::PolygonMode::Line,
                    unclipped_depth: false,
                    conservative: false,
                },
                depth_stencil: None,
                multisample: wgpu::MultisampleState {
                    count: 1,
                    mask: !0,
                    alpha_to_coverage_enabled: false,
                },
                multiview: None,
            }))
        } else {
            None
        };

        let blur_pipelines = renderer::blur::BlurPipelines::new(
            &gpu.device,
            surface_format,
            &camera_bind_group_layout,
            renderer::vertex::Vertex::desc(),
        );
        let glow_pipelines = renderer::glow::GlowPipelines::new(&gpu.device, surface_format);

        Self {
            gpu,
            font: Rc::new(RefCell::new(font)),
            shaper: Rc::new(RefCell::new(shaper)),
            render_pipeline,
            wireframe_pipeline,
            font_bind_group_layout,
            camera_bind_group_layout,
            fade_bind_group_layout,
            blur_pipelines,
            glow_pipelines,
        }
    }

    /// Borrow the shared font for the duration of `f` and no longer. Callers
    /// receive a `&Font`, never the `Ref`, so the borrow scope can't be
    /// widened past the call — the structural guard against the borrow-overlap
    /// panic described on the `font` field.
    fn with_font<R>(&self, f: impl FnOnce(&font::Font) -> R) -> R {
        f(&self.font.borrow())
    }
}

/// One shell session and the interaction state bound to it. Everything here
/// scrolls, selects, or completes against a single PTY + `Terminal`; none of
/// it is coupled to the window's GPU surface, atlas, or buffers (so a tab can
/// later move between windows — see MULTIWINDOW_PLAN.md). A window owns a
/// `Vec<TabState>` and renders only the active one. First cut: exactly one tab
/// per window; the tab UI is a later follow-up.
struct TabState {
    /// Process-unique id this tab's PTY reader thread tags its events with.
    /// The event loop resolves it to the owning window via `tab_to_window`.
    tab_id: app_window::TabId,
    /// PTY master fd. The reader thread owns its own copy (reads + reaps);
    /// this copy lets the main thread `close()` it to unblock that read on
    /// tab close.
    master: i32,
    /// Forked child pid. Kept so tab close can `kill(child, SIGHUP)` — the
    /// blocking `read(master)` only returns once the child exits, so closing a
    /// tab running e.g. `vim` needs both `close(master)` and the signal. Wired
    /// to the close path in Stage 4; stored here (per the plan) when the tab is
    /// minted.
    #[allow(dead_code)]
    child: i32,
    terminal: terminal::Terminal,
    /// Decode + GPU residency cache for this tab's images. Per-tab: each shell
    /// has its own placements + scrollback. Mark-and-sweep eviction keyed on
    /// the live + scrollback placement set runs at the start of each `render`.
    image_store: images::Store,
    /// Cell anchors waiting on async decode, matched back to the tab's
    /// `Terminal` by `PendingId` when `Store::poll` yields the result.
    pending_placements: Vec<PendingImagePlacement>,
    scroll_y: f64,
    /// In-flight smooth slide for an explicit alt-screen scroll captured from
    /// the running app (`Terminal::take_alt_scroll`).
    alt_scroll_anim: Option<AltScrollAnim>,
    /// Pixel accumulator for the PTY mouse-tracking wheel path (tmux, vim,
    /// less, htop). Drained per `line_height` like `scroll_y`.
    wheel_pty_accum: f64,
    /// Drop in-flight trackpad momentum once a newer command has overridden
    /// the user's scroll intent. See `last_wheel_at`.
    scroll_suppressed: bool,
    last_wheel_at: Option<std::time::Instant>,
    /// Last cell a motion event was reported for — coalesces per-pixel motion
    /// down to per-cell transitions for the host.
    last_reported_cell: Option<(u16, u16)>,
    /// Smooth cursor motion: eases the rendered cursor quad toward the logical
    /// cursor over `config.cursor_anim_secs`. `None` off-screen / pre-first-frame.
    cursor_anim: Option<CursorAnim>,
    /// Snapshot of the previous frame's visible cells (+ a viewport key) used
    /// to spawn fade-out ghosts when the cursor retargets across deleted glyphs.
    prev_visible: Option<GridSnapshot>,
    /// Glyphs fading out at their old cell position after the cursor moved off.
    cursor_ghosts: Vec<CursorGhost>,
    /// Completion popup suggestions, recomputed only when the shell's reported
    /// input changes (path completion does disk I/O).
    completions: Vec<completion::Suggestion>,
    /// The (buffer, cursor) the cached `completions` were computed from.
    completions_input: Option<(String, usize)>,
    /// Highlighted row in the completion popup (index into `completions`).
    selected_completion: usize,
    /// First visible popup row when `completions` exceeds MAX_VISIBLE.
    completion_scroll: usize,
    /// When true the popup stays closed as the shell re-reports input — set by
    /// Enter/Esc, cleared by the next real keystroke.
    completion_dismissed: bool,
    /// Past command lines for history-based completion, most-recent-first and
    /// deduped. Seeded from $HISTFILE (OSC 2124), grown at OSC 133 `C`.
    command_history: Vec<String>,
    /// Active local text selection in (absolute_line, col) coordinates so it
    /// stays anchored to content as the grid scrolls.
    selection: Option<Selection>,
    /// Granularity for the active drag (set on press from click_count).
    selection_mode: SelectionMode,
    /// Cell where the current drag started; recomputes word/line selections as
    /// the head moves. `None` when no button is being dragged.
    press_cell: Option<(isize, usize)>,
    /// Pixel position of the mouse-down — suppresses a Cell-mode selection
    /// until the cursor moves at least DRAG_THRESHOLD_PX.
    press_pixel: Option<(f64, f64)>,
    /// Last left-button press, for multi-click detection (cell + time window).
    last_click: Option<(std::time::Instant, (isize, usize))>,
    click_count: u32,
    /// URL under the mouse while Cmd is held. Drives the underline overlay and
    /// the Cmd-click open behavior.
    hover_url: Option<HoverUrl>,
}

struct WindowState {
    /// Per-window GPU surface. Declared first so it drops before `window` —
    /// the surface holds unsafe references to the window's resources.
    surface: gpu::WindowSurface,

    window: Window,

    /// Process-shared GPU device/queue, font stack, and pipelines. Held via
    /// `Rc` so every window shares one instance; declared after `surface` so
    /// the shared device outlives the surface configured against it.
    shared: Rc<AppShared>,

    /// Toggled by Cmd-Shift-W. When true, render() picks
    /// `shared.wireframe_pipeline`.
    wireframe: bool,
    vertex_buffer: wgpu::Buffer,
    index_buffer: wgpu::Buffer,
    num_indices: u32,
    /// Boundary inside `index_buffer`: indices `0..num_bg_indices` are the
    /// per-cell background quads (bg layer); `num_bg_indices..num_indices`
    /// are glyphs + cursor + selection + URL underline (fg layer). The
    /// composite pass draws them in two `draw_indexed` calls so glow can
    /// bloom each layer independently.
    num_bg_indices: u32,
    // Separate buffer for the edge-fade strip quads. Drawn in the composite
    // pass with the blur sampler bound, so the strips are filled with the
    // dual-Kawase blur of the scene rather than a flat white tint.
    strip_vertex_buffer: wgpu::Buffer,
    strip_index_buffer: wgpu::Buffer,
    num_strip_indices: u32,
    /// Textured-quad pipeline for image placements. Owns the per-frame
    /// vertex/index buffers and is invoked once per frame between the bg
    /// and fg cell passes (when there are placements to draw).
    image_pipeline: renderer::images::ImagePipeline,
    blur: renderer::blur::BlurChain,
    /// Saturation-threshold bloom. When `glow.enabled` is true, the scene is
    /// always rendered to the offscreen `blur.scene` texture so the glow
    /// pass can sample it, even when no edge-fade strips are active. This
    /// instance handles the BG layer; `glow_fg` mirrors it for the FG
    /// layer (glyphs + cursor + selection + URL underline). Both share
    /// the same parameters and palette/foreground installations; only
    /// their source textures differ.
    glow: renderer::glow::Glow,
    /// Second Glow instance, bound to `scene_fg.view`. Always kept in
    /// param-sync with `glow` — toggling either match mode toggles both.
    glow_fg: renderer::glow::Glow,
    /// Offscreen render target for the FG layer (glyphs + cursor +
    /// overlays). Same format/dimensions as `blur.scene`. Cleared
    /// transparent before fg quads draw so the BG layer (which already
    /// landed in the swapchain) shows through everywhere fg is absent.
    scene_fg: SceneTarget,
    /// Bind group for blit_pipeline / blit_alpha_pipeline that samples
    /// `scene_fg`. Rebuilt on resize when the texture is recreated.
    scene_fg_blit_bg: wgpu::BindGroup,
    /// Mask bind groups for the masked composite path. Both glows use
    /// the bg scene as their mask so the halo paints only where the bg
    /// is transparent — preventing the glow from tinting adjacent
    /// cells' colored backgrounds. Per-glow because the mask bgl is
    /// owned per `Glow` instance.
    glow_bg_mask: wgpu::BindGroup,
    glow_fg_mask: wgpu::BindGroup,
    /// Bind group for the scanline overlay's masked variant. Binds
    /// both the bg scene (binding 0) and the fg scene (binding 2) so
    /// the shader can detect "truly empty" pixels and only suppress
    /// the overlay there. Rebuilt on resize when either texture is
    /// recreated.
    scanline_overlay_mask: wgpu::BindGroup,
    font_bind_group: wgpu::BindGroup,
    /// The actual font atlas texture. Kept around so on-demand-rasterized
    /// ligature glyphs can be uploaded incrementally via queue.write_texture
    /// without recreating the texture or bind group.
    font_texture: renderer::texture::Texture,
    /// Current font size in points; mutated by Cmd-+ / Cmd--.
    pt_size: f32,
    dpi: u32,
    config: Config,
    camera: renderer::camera::Camera,
    camera_uniform: renderer::camera::CameraUniform,
    camera_buffer: wgpu::Buffer,
    camera_bind_group: wgpu::BindGroup,
    fade_buffer: wgpu::Buffer,
    fade_bind_group: wgpu::BindGroup,
    atlas: font::Atlas,
    /// The window's tabs and which one is active. Per-tab state (shell,
    /// scroll, selection, completions, images) lives in `TabState`; the
    /// window renders only `tabs[active]`. First cut: always exactly one tab.
    tabs: Vec<TabState>,
    active: usize,
    modifiers: winit::keyboard::ModifiersState,
    mouse_x: f64,
    mouse_y: f64,
    // Currently-held mouse button (in xterm code). `None` when no button down.
    held_button: Option<input::MouseButton>,
    // Cursor blink. `blink_on` is the visible phase; `last_blink` anchors the
    // timer so user input can reset it (cursor stays solid while typing).
    blink_on: bool,
    last_blink: std::time::Instant,
    /// Edge fade animations: phase ramps 0→1 in TOP_FADE_ANIM duration as
    /// soon as the view scrolls away from the corresponding boundary, and
    /// 1→0 when it returns. Decoupled from scroll distance so the fade
    /// slides in at a constant rate regardless of scroll speed.
    top_fade_phase: f32,
    bottom_fade_phase: f32,
    last_anim_tick: std::time::Instant,
    /// The command palette overlay (Cmd-Shift-P). While `open`, it owns the
    /// keyboard: keystrokes filter/drive it instead of reaching the PTY. Holds
    /// its own search/argument text field and selection state; see
    /// `command_palette.rs`.
    command_palette: command_palette::CommandPalette,
    /// The find-in-scrollback overlay (Cmd-F). While `open`, it owns the
    /// keyboard like the command palette; holds the query field and the
    /// list of matches across the buffer. See `search.rs`.
    search: search::Search,
    // Time of the last left-press in the title-bar band, for double-click
    // detection there. Separate from `last_click` (which keys on a grid cell)
    // since toolbar clicks have no cell. A double-click toggles window zoom.
    last_toolbar_click: Option<std::time::Instant>,
    // Whether the pointer is currently in the title-bar band. Tracked so a
    // crossing back into the grid can restore the I-beam exactly once.
    over_toolbar: bool,
    /// Height (physical px) of the title-bar / toolbar chrome band — the region
    /// where pointer input drives the window (drag / zoom / traffic lights) and
    /// the cursor is the arrow, not the grid's I-beam. Derived from the live
    /// native title-bar height (which scales with DPI) plus a small margin, not
    /// the renderer's fixed `WINDOW_PADDING + DECORATOR_HEIGHT` reserve — see
    /// `refresh_chrome_band`. Recomputed on resize / scale-factor change.
    chrome_band_px: f64,
    perf: PerfLog,
    /// Set whenever something invalidates the vertex/index buffers (PTY input,
    /// scroll, selection, blink, animation tick). Cleared by `flush_vertices`,
    /// which the redraw handler calls before drawing. Lets winit coalesce a
    /// burst of N events into one rebuild + one frame.
    vertices_dirty: bool,
}

const DOUBLE_CLICK_THRESHOLD: std::time::Duration = std::time::Duration::from_millis(500);

/// Burst-scoped timing aggregator. Accumulates work caused by a run of PTY
/// chunks + the frames that draw them, then prints a one-line summary once
/// the activity has settled (>= PERF_FLUSH_IDLE since the last sample).
/// Disabled unless `PERFLOG=1` is set in the environment so the steady-state
/// terminal stays quiet.
const PERF_FLUSH_IDLE: std::time::Duration = std::time::Duration::from_millis(150);

struct PerfLog {
    enabled: bool,
    burst_start: Option<std::time::Instant>,
    last_event: std::time::Instant,
    pty_chunks: u32,
    pty_bytes: usize,
    feed_ns: u128,
    update_ns: u128,
    update_calls: u32,
    render_ns: u128,
    render_calls: u32,
    fast_calls: u32,
    fast_ns: u128,
    slow_calls: u32,
    slow_ns: u128,
    surface_wait_ns: u128,
}

impl PerfLog {
    fn new() -> Self {
        Self {
            enabled: std::env::var("PERFLOG").map(|v| !v.is_empty() && v != "0").unwrap_or(false),
            burst_start: None,
            last_event: std::time::Instant::now(),
            pty_chunks: 0,
            pty_bytes: 0,
            feed_ns: 0,
            update_ns: 0,
            update_calls: 0,
            render_ns: 0,
            render_calls: 0,
            fast_calls: 0,
            fast_ns: 0,
            slow_calls: 0,
            slow_ns: 0,
            surface_wait_ns: 0,
        }
    }

    fn note_pty(&mut self, bytes: usize, feed: std::time::Duration) {
        if !self.enabled {
            return;
        }
        let now = std::time::Instant::now();
        if self.burst_start.is_none() {
            self.burst_start = Some(now);
        }
        self.last_event = now;
        self.pty_chunks += 1;
        self.pty_bytes += bytes;
        self.feed_ns += feed.as_nanos();
    }

    fn note_update(&mut self, dur: std::time::Duration) {
        if !self.enabled {
            return;
        }
        let now = std::time::Instant::now();
        if self.burst_start.is_none() {
            self.burst_start = Some(now);
        }
        self.last_event = now;
        self.update_ns += dur.as_nanos();
        self.update_calls += 1;
    }

    fn note_render(
        &mut self,
        dur: std::time::Duration,
        surface_wait: std::time::Duration,
        fast: bool,
    ) {
        if !self.enabled {
            return;
        }
        let now = std::time::Instant::now();
        if self.burst_start.is_none() {
            self.burst_start = Some(now);
        }
        self.last_event = now;
        self.render_ns += dur.as_nanos();
        self.render_calls += 1;
        self.surface_wait_ns += surface_wait.as_nanos();
        if fast {
            self.fast_calls += 1;
            self.fast_ns += dur.as_nanos();
        } else {
            self.slow_calls += 1;
            self.slow_ns += dur.as_nanos();
        }
    }

    /// Wake-up time the event loop should arm to so we can print the summary
    /// soon after the burst goes quiet. `None` when no burst is pending.
    fn next_wake(&self) -> Option<std::time::Instant> {
        if !self.enabled || self.burst_start.is_none() {
            return None;
        }
        Some(self.last_event + PERF_FLUSH_IDLE)
    }

    fn maybe_flush(&mut self) {
        if !self.enabled || self.burst_start.is_none() {
            return;
        }
        let now = std::time::Instant::now();
        if now.duration_since(self.last_event) < PERF_FLUSH_IDLE {
            return;
        }
        let total = now.duration_since(self.burst_start.unwrap());
        let accounted_ns = self.feed_ns + self.update_ns + self.render_ns;
        let avg_ns = |total_ns: u128, n: u32| {
            if n == 0 { 0.0 } else { total_ns as f64 / n as f64 / 1e6 }
        };
        eprintln!(
            "[perf] burst {:>6.1}ms wall | pty {:>2}c {:>6}B | feed {:>5.2}ms | update {:>6.2}ms x{:>2} | render {:>6.2}ms x{:>2} (fast x{:>2} avg{:>4.2} / slow x{:>2} avg{:>4.2}) | swait {:>6.2}ms | acc {:>4.1}%",
            total.as_secs_f64() * 1e3,
            self.pty_chunks,
            self.pty_bytes,
            self.feed_ns as f64 / 1e6,
            self.update_ns as f64 / 1e6,
            self.update_calls,
            self.render_ns as f64 / 1e6,
            self.render_calls,
            self.fast_calls,
            avg_ns(self.fast_ns, self.fast_calls),
            self.slow_calls,
            avg_ns(self.slow_ns, self.slow_calls),
            self.surface_wait_ns as f64 / 1e6,
            if total.as_nanos() > 0 {
                (accounted_ns as f64 / total.as_nanos() as f64) * 100.0
            } else {
                0.0
            },
        );
        self.burst_start = None;
        self.pty_chunks = 0;
        self.pty_bytes = 0;
        self.feed_ns = 0;
        self.update_ns = 0;
        self.update_calls = 0;
        self.render_ns = 0;
        self.render_calls = 0;
        self.fast_calls = 0;
        self.fast_ns = 0;
        self.slow_calls = 0;
        self.slow_ns = 0;
        self.surface_wait_ns = 0;
    }
}

/// Minimum pixel distance the mouse must travel after mouse-down before a
/// Cell-mode drag begins to paint a selection. Below this, a press-and-release
/// counts as a plain click and never flashes a single-cell highlight.
const DRAG_THRESHOLD_PX: f64 = 4.0;

#[derive(Copy, Clone, Debug, PartialEq)]
enum SelectionMode {
    Cell,
    Word,
    Line,
}

/// Classification of a single corner of a selection strip relative to the
/// strip in the row above (for top corners) or below (for bottom corners).
/// `Convex` rounds outward; `Concave` is an inner step that gets a fillet
/// quad in the unselected quadrant; `Straight` is on a continuous edge.
#[derive(Copy, Clone, Debug, PartialEq)]
enum CornerType {
    Convex,
    Straight,
    Concave,
}

/// Pick a corner type given this strip's column adjacent to the corner and
/// the neighbor strip's range, when looking at the LEFT side of either strip
/// (TL and BL corners). For RIGHT side (TR/BR), call with `mirror = true`
/// so the same logic applies symmetrically.
fn classify_corner_with_neighbor(
    col: usize,
    neighbor: Option<(usize, usize)>,
    side: HorizSide,
) -> CornerType {
    let Some((nf, nt)) = neighbor else {
        return CornerType::Convex;
    };
    match side {
        HorizSide::Left => {
            // The corner sits at `col`. Neighbor "covers further left" if its
            // strip starts before `col` (i.e., includes col - 1).
            let covers_outer = nf < col;
            // Neighbor "covers the same column" if `col` is inside its range.
            let covers_at = nf <= col && col <= nt;
            if covers_outer && covers_at {
                CornerType::Concave
            } else if covers_at {
                CornerType::Straight
            } else {
                CornerType::Convex
            }
        }
        HorizSide::Right => {
            // Mirror image: outer side is "to the right of `col`".
            let covers_outer = nt > col;
            let covers_at = nf <= col && col <= nt;
            if covers_outer && covers_at {
                CornerType::Concave
            } else if covers_at {
                CornerType::Straight
            } else {
                CornerType::Convex
            }
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq)]
enum HorizSide {
    Left,
    Right,
}

/// Word-character predicate for double-click word selection. Letters and
/// digits, plus the punctuation that's commonly part of identifiers, paths,
/// and URLs in shell output (so e.g. `~/foo/bar.txt` selects as one token).
fn is_word_char(ch: char) -> bool {
    ch.is_alphanumeric() || matches!(ch, '_' | '-' | '.' | '/' | '~' | '+' | ':' | '@' | '%')
}

/// A clickable URL the mouse is currently hovering over. Tracked while the
/// Cmd modifier is held so the renderer can underline the span and the
/// click handler can open it. URLs that wrap at the right edge span
/// multiple rows; the start/end pair is inclusive on both ends.
/// One underline strip on a single (scroll-stable) line; columns inclusive.
#[derive(Clone, Debug, PartialEq)]
struct HoverSegment {
    abs_line: isize,
    start_col: usize,
    end_col: usize,
}

#[derive(Clone, Debug, PartialEq)]
struct HoverUrl {
    /// Every strip to underline. A heuristic match or a contiguous OSC 8 link
    /// is one segment (or a few, when it wraps across rows); an OSC 8 link
    /// whose `id=` is shared by non-contiguous spans contributes a segment per
    /// visible run, so all siblings underline together. Ordered by line then
    /// column for stable equality (so hover repaint de-dup works).
    segments: Vec<HoverSegment>,
    /// The URL text itself, ready to hand to `open(1)`.
    url: String,
}

#[cfg(test)]
impl HoverUrl {
    /// First/last segment edges — convenient for the single-run (heuristic or
    /// contiguous OSC 8) cases the tests assert on. Segments are ordered by
    /// line then column.
    fn start_abs_line(&self) -> isize {
        self.segments.first().unwrap().abs_line
    }
    fn end_abs_line(&self) -> isize {
        self.segments.last().unwrap().abs_line
    }
    fn start_col(&self) -> usize {
        self.segments.first().unwrap().start_col
    }
    fn end_col(&self) -> usize {
        self.segments.last().unwrap().end_col
    }
}

/// Locate an http/https URL within a row of cells that covers `col`. The
/// run is bounded by surrounding whitespace; trailing sentence punctuation
/// (`.,;:!?)]}>'"`) is stripped so a URL at the end of a sentence still
/// opens cleanly.
fn find_url_in_cells(cells: &[style::Cell], col: usize) -> Option<(usize, usize, String)> {
    let n = cells.len();
    if col >= n || cells[col].ch.is_whitespace() {
        return None;
    }
    let mut start = col;
    while start > 0 && !cells[start - 1].ch.is_whitespace() {
        start -= 1;
    }
    let mut end = col;
    while end + 1 < n && !cells[end + 1].ch.is_whitespace() {
        end += 1;
    }
    let mut hit: Option<(usize, usize)> = None;
    'outer: for s in start..=end {
        for prefix in ["https://", "http://"] {
            let plen = prefix.len();
            if s + plen > end + 1 {
                continue;
            }
            if cells[s..s + plen]
                .iter()
                .zip(prefix.chars())
                .all(|(c, p)| c.ch == p)
            {
                hit = Some((s, plen));
                break 'outer;
            }
        }
    }
    let (url_start_col, prefix_len) = hit?;
    let mut url_end_col = end;
    while url_end_col > url_start_col
        && matches!(
            cells[url_end_col].ch,
            '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '>' | '\'' | '"'
        )
    {
        url_end_col -= 1;
    }
    // Reject scheme-only matches like "https://" or "https://." — a URL is
    // only useful if there's at least one host char past the separator.
    if url_end_col + 1 <= url_start_col + prefix_len {
        return None;
    }
    if col < url_start_col || col > url_end_col {
        return None;
    }
    let url: String = cells[url_start_col..=url_end_col]
        .iter()
        .map(|c| c.ch)
        .collect();
    Some((url_start_col, url_end_col, url))
}

/// Cap on how far we'll walk in either direction looking for a wrapped URL
/// continuation. URLs that span more than this many rows are exotic enough
/// that the heuristic isn't worth burning scrollback walks on.
const URL_WRAP_MAX_ROWS: usize = 32;

/// Build the wrapped logical line containing `abs_line`: walk back and
/// forward across rows whose adjacent edges are both non-whitespace (the
/// terminal's autowrap left no separator between them) and concatenate the
/// cells. Returns `(start_abs_line, cols, flat_cells)` so callers can map
/// flat indices back to (row, col). Bounded by `URL_WRAP_MAX_ROWS` either
/// side.
fn build_wrapped_line(
    terminal: &terminal::Terminal,
    abs_line: isize,
) -> Option<(isize, usize, Vec<style::Cell>)> {
    let row = terminal.line_at(abs_line)?;
    let cols = row.len();
    if cols == 0 {
        return Some((abs_line, 0, Vec::new()));
    }

    // Walk back as long as the previous row's last col is non-whitespace
    // *and* the current row's first col is non-whitespace — the only
    // signature autowrap leaves on the cell grid (no soft-wrap flag).
    let mut start = abs_line;
    let mut steps = 0;
    while steps < URL_WRAP_MAX_ROWS {
        let prev = match terminal.line_at(start - 1) {
            Some(p) if p.len() == cols => p,
            _ => break,
        };
        let cur = terminal.line_at(start).expect("walked from a valid row");
        if prev.last().map(|c| c.ch.is_whitespace()).unwrap_or(true)
            || cur.first().map(|c| c.ch.is_whitespace()).unwrap_or(true)
        {
            break;
        }
        start -= 1;
        steps += 1;
    }

    let mut end = abs_line;
    let mut steps = 0;
    while steps < URL_WRAP_MAX_ROWS {
        let next = match terminal.line_at(end + 1) {
            Some(n) if n.len() == cols => n,
            _ => break,
        };
        let cur = terminal.line_at(end).expect("walked from a valid row");
        if cur.last().map(|c| c.ch.is_whitespace()).unwrap_or(true)
            || next.first().map(|c| c.ch.is_whitespace()).unwrap_or(true)
        {
            break;
        }
        end += 1;
        steps += 1;
    }

    let mut buf = Vec::with_capacity(((end - start + 1) as usize) * cols);
    for line in start..=end {
        let r = terminal.line_at(line)?;
        buf.extend_from_slice(r);
    }
    Some((start, cols, buf))
}

/// Locate the URL under `(abs_line, col)`, joining wrap-continued rows so a
/// link that spilled past the right edge still resolves as a single span.
/// Falls back to a same-row search when no wrap continuation is in play.
/// Inclusive column runs of cells whose hyperlink id equals `id`, in one row.
fn hyperlink_runs(
    cells: &[style::Cell],
    id: std::num::NonZeroU32,
) -> Vec<(usize, usize)> {
    let mut runs = Vec::new();
    let mut i = 0;
    while i < cells.len() {
        if cells[i].hyperlink == Some(id) {
            let start = i;
            while i + 1 < cells.len() && cells[i + 1].hyperlink == Some(id) {
                i += 1;
            }
            runs.push((start, i));
        }
        i += 1;
    }
    runs
}

/// Locate an OSC 8 explicit hyperlink under `(abs_line, col)`. The link id is
/// taken from the cell; every visible cell sharing that id is part of the same
/// logical link (the OSC 8 `id=` contract), so we collect a [`HoverSegment`]
/// for every run of it across the visible rows — including non-contiguous
/// siblings, which then underline together. We scan only what's on screen
/// because that's all the overlay can draw; siblings scrolled off don't need a
/// strip. Takes precedence over the heuristic: the extent and target are
/// exactly what the app declared.
fn find_osc8_link_at(
    terminal: &terminal::Terminal,
    abs_line: isize,
    col: usize,
) -> Option<HoverUrl> {
    let row = terminal.line_at(abs_line)?;
    if col >= row.len() {
        return None;
    }
    let id = row[col].hyperlink?;
    let uri = terminal.hyperlink_uri(id)?.to_string();

    let mut segments = Vec::new();
    for v in 0..terminal.rows as isize {
        let line = terminal.visual_to_abs_line(v);
        let Some(cells) = terminal.line_at(line) else {
            continue;
        };
        for (start_col, end_col) in hyperlink_runs(cells, id) {
            segments.push(HoverSegment {
                abs_line: line,
                start_col,
                end_col,
            });
        }
    }
    if segments.is_empty() {
        return None;
    }
    Some(HoverUrl {
        segments,
        url: uri,
    })
}

/// Scheme allowlist for opening a clicked link. The heuristic only ever
/// produces http/https, but OSC 8 lets an app declare an arbitrary target, so
/// we refuse anything outside a small safe set (no `javascript:`, `data:`,
/// `vbscript:`, etc.) before handing it to the OS opener.
fn is_safe_url(url: &str) -> bool {
    let lower = url.trim().to_ascii_lowercase();
    const SAFE: [&str; 6] = ["http://", "https://", "mailto:", "ftp://", "file://", "ssh://"];
    SAFE.iter().any(|p| lower.starts_with(p))
}

fn find_url_at(
    terminal: &terminal::Terminal,
    abs_line: isize,
    col: usize,
) -> Option<HoverUrl> {
    // App-declared OSC 8 links win over the heuristic: exact bounds, and they
    // may carry non-http schemes the heuristic can't express.
    if let Some(hu) = find_osc8_link_at(terminal, abs_line, col) {
        return Some(hu);
    }
    let (start_abs, cols, flat) = build_wrapped_line(terminal, abs_line)?;
    if cols == 0 || col >= cols {
        return None;
    }
    let row_offset = (abs_line - start_abs) as usize;
    let virtual_col = row_offset * cols + col;
    let (s, e, url) = find_url_in_cells(&flat, virtual_col)?;
    // A heuristic match is one contiguous (possibly wrapped) run: first row
    // from `start_col` to the edge, full-width middle rows, last row to
    // `end_col`. Express it as the same per-line segments OSC 8 uses.
    let start_line = start_abs + (s / cols) as isize;
    let start_col = s % cols;
    let end_line = start_abs + (e / cols) as isize;
    let end_col = e % cols;
    let mut segments = Vec::new();
    let mut line = start_line;
    while line <= end_line {
        let from = if line == start_line { start_col } else { 0 };
        let to = if line == end_line { end_col } else { cols - 1 };
        segments.push(HoverSegment {
            abs_line: line,
            start_col: from,
            end_col: to,
        });
        line += 1;
    }
    Some(HoverUrl { segments, url })
}

#[cfg(target_os = "macos")]
fn open_url(url: &str) {
    let _ = std::process::Command::new("open").arg(url).spawn();
}
#[cfg(not(target_os = "macos"))]
fn open_url(_url: &str) {}

/// Logical-point offset applied to each cascaded window, matching the macOS
/// convention of stepping a new window down-and-right from its parent. Roughly
/// a title-bar height so successive windows stack like a fanned deck.
const WINDOW_CASCADE_STEP: f64 = 28.0;

/// Env var carrying the parent window's top-left, in logical points, to a
/// freshly spawned child (`"x,y"`). The child reads it in `run()` and places
/// its window one `WINDOW_CASCADE_STEP` down-and-right so new windows cascade
/// instead of landing exactly atop the one that spawned them. Absent for the
/// first window (launched from Finder/CLI), which keeps the OS default spot.
const CASCADE_ENV: &str = "YUTANI_CASCADE_FROM";

/// Launch a fresh Yutani window. Each window is its own process (the app is
/// single-window per process), so a new window is just another instance of our
/// own executable. `cwd` — the running shell's working directory from OSC 7 —
/// becomes the child's working directory so the new window opens where the
/// current one is, falling back to inheriting ours when it's unknown.
/// `origin` is the spawning window's top-left in logical points; when present
/// it's forwarded so the child can cascade off it. Failures are logged rather
/// than fatal: a missing exe path shouldn't kill the window the user is in.
fn spawn_new_window(cwd: Option<&str>, origin: Option<(f64, f64)>) {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("new window: cannot resolve current exe: {e}");
            return;
        }
    };
    let mut cmd = std::process::Command::new(exe);
    if let Some(dir) = cwd {
        if !dir.is_empty() {
            cmd.current_dir(dir);
        }
    }
    if let Some((x, y)) = origin {
        cmd.env(CASCADE_ENV, format!("{x},{y}"));
    }
    if let Err(e) = cmd.spawn() {
        eprintln!("new window: failed to spawn: {e}");
    }
}

/// Parse the cascade hint set by a parent window (see [`CASCADE_ENV`]) into the
/// child's target top-left, stepped one [`WINDOW_CASCADE_STEP`] down-and-right.
/// Returns `None` when the var is absent or malformed so the window falls back
/// to the OS-chosen position.
fn cascade_position() -> Option<winit::dpi::LogicalPosition<f64>> {
    let raw = std::env::var(CASCADE_ENV).ok()?;
    let (x, y) = raw.split_once(',')?;
    let x: f64 = x.trim().parse().ok()?;
    let y: f64 = y.trim().parse().ok()?;
    Some(winit::dpi::LogicalPosition::new(
        x + WINDOW_CASCADE_STEP,
        y + WINDOW_CASCADE_STEP,
    ))
}

const BLINK_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);
const ANIM_FRAME: std::time::Duration = std::time::Duration::from_millis(16);
/// Duration of the smooth-scroll slide for an explicit alt-screen scroll
/// (SU/SD/line-feed) captured from the running app. Kept short so the terminal
/// stays responsive — the final frame is reached this many seconds after the
/// scroll lands, regardless of distance.
const ALT_SCROLL_ANIM_SECS: f32 = 0.07;

/// An in-flight alt-screen scroll animation. `total_px` is the full slide
/// distance; the rendered offset eases from `total_px` down to 0 over
/// `ALT_SCROLL_ANIM_SECS`. `up` mirrors the captured scroll direction and
/// picks the sign of the offset applied to `scroll_y`.
#[derive(Copy, Clone)]
struct AltScrollAnim {
    up: bool,
    rows: usize,
    region_top: usize,
    region_bottom: usize,
    total_px: f32,
    started: std::time::Instant,
}

/// Eased cursor position in cell-space (col, visual_row) floats. Lerp-with-
/// retarget chase: when the logical cursor moves while an ease is still in
/// flight, `from` is rebased to the currently-displayed position so the new
/// segment starts where the eye last saw the quad.
#[derive(Copy, Clone)]
struct CursorAnim {
    from: (f32, f32),
    to: (f32, f32),
    started_at: std::time::Instant,
}

impl CursorAnim {
    fn snapped(target: (f32, f32)) -> Self {
        Self {
            from: target,
            to: target,
            started_at: std::time::Instant::now(),
        }
    }

    /// Smoothstep `t*t*(3 - 2t)` — symmetric ease-in-out, no overshoot.
    /// Snaps `from = to` once elapsed crosses `duration` so the next
    /// `animating()` call returns false — without that snap the event loop
    /// could stop ticking with the last drawn frame at `t < duration`
    /// (cursor a few pixels short of target) because the previous
    /// `WaitUntil` landed after the animation ended.
    fn current(&mut self, duration: f32) -> (f32, f32) {
        if duration <= 0.0 || self.started_at.elapsed().as_secs_f32() >= duration {
            self.from = self.to;
            return self.to;
        }
        let t = self.started_at.elapsed().as_secs_f32() / duration;
        let e = t * t * (3.0 - 2.0 * t);
        (
            self.from.0 + (self.to.0 - self.from.0) * e,
            self.from.1 + (self.to.1 - self.from.1) * e,
        )
    }

    /// "Still chasing": `from != to`. Stays true even past `duration`
    /// until a render calls `current()` and snaps `from = to`, so the
    /// event loop is guaranteed to render at least one frame past the
    /// end of the ease (where `current()` returns `to`) before parking.
    fn animating(&self, _duration: f32) -> bool {
        (self.from.0 - self.to.0).abs() > f32::EPSILON
            || (self.from.1 - self.to.1).abs() > f32::EPSILON
    }

    /// Point the ease at a new target, rebasing `from` to whatever is
    /// currently rendered so the motion is continuous.
    fn retarget(&mut self, new_target: (f32, f32), duration: f32) {
        if (new_target.0 - self.to.0).abs() < f32::EPSILON
            && (new_target.1 - self.to.1).abs() < f32::EPSILON
        {
            return;
        }
        self.from = self.current(duration);
        self.to = new_target;
        self.started_at = std::time::Instant::now();
    }
}

/// Identifies which viewport a `GridSnapshot` was taken from. Mismatch on
/// any field means cell coordinates aren't comparable across frames (the
/// whole grid was repainted), so ghost detection is skipped.
#[derive(Copy, Clone, PartialEq, Eq)]
struct ViewportKey {
    rows: usize,
    cols: usize,
    view_offset: usize,
    on_alt_screen: bool,
}

struct GridSnapshot {
    cells: Vec<Vec<style::Cell>>,
    key: ViewportKey,
}

/// A glyph being faded out at its old cell position to bridge the gap
/// between an instantaneous cell clear (e.g. backspace overwriting with a
/// space) and the cursor's animated slide across that cell. Stored in
/// buffer-row coordinates so the ghost stays anchored to the underlying
/// cell when the user scrolls; the visual row is recomputed each frame
/// from the current `live_grid_offset`.
struct CursorGhost {
    ch: char,
    style: style::Style,
    buffer_row: usize,
    col: usize,
    started_at: std::time::Instant,
}

fn is_blank_cell(cell: &style::Cell) -> bool {
    matches!(cell.ch, ' ' | '\0')
}

/// Cell-range selection in absolute-line coordinates. `anchor` is where the
/// drag started, `head` is where it currently is — they may be in either
/// order, so callers normalize via `range()` before iterating.
#[derive(Copy, Clone, Debug)]
struct Selection {
    anchor: (isize, usize),
    head: (isize, usize),
}

impl Selection {
    /// Endpoints in (start, end) reading order, inclusive on both ends.
    fn range(&self) -> ((isize, usize), (isize, usize)) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }

}

impl WindowState {
    /// Build one window against the shared, already-constructed `AppShared`,
    /// adopting `initial_tab` as its (sole, for now) tab. Builds only the
    /// per-window resources: the glyph atlas + font texture/bind-group, camera
    /// + fade uniforms/bind-groups, vertex/index buffers, and the blur/glow
    /// textures. Reused verbatim by Cmd-N (Stage 4) and tab tear-off (later).
    fn create_window(
        shared: Rc<AppShared>,
        window: Window,
        surface: gpu::WindowSurface,
        config: Config,
        dpi: u32,
        initial_tab: TabState,
    ) -> Self {
        let _sw = std::time::Instant::now();
        let _timing = std::env::var_os("YUTANI_STARTUP_TIMING").is_some();
        macro_rules! sub { ($l:expr) => { if _timing { eprintln!("[startup]   ... create_window {:>7.1}ms  {}", _sw.elapsed().as_secs_f64()*1000.0, $l); } } }
        let pt_size = config.font_size;

        // Per-window glyph atlas, rasterized on demand from the shared faces.
        let atlas = shared.font.borrow_mut().build_atlas();
        sub!("build_atlas");

        let font_texture = renderer::texture::Texture::from_memory(
            &shared.gpu.device,
            &shared.gpu.queue,
            &atlas.buffer,
            atlas.width as u32,
            atlas.height as u32,
            wgpu::TextureFormat::R8Unorm,
            Some("font texture"),
        );

        let font_bind_group = shared.gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            layout: &shared.font_bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&font_texture.view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&font_texture.sampler),
                },
            ],
            label: Some("font bind group"),
        });

        let camera = renderer::camera::Camera {};
        let mut camera_uniform = renderer::camera::CameraUniform::new();
        camera_uniform.update_view_proj(&camera, surface.config.width as f32, surface.config.height as f32);

        let camera_buffer = shared.gpu.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("camera buffer"),
            contents: bytemuck::cast_slice(&[camera_uniform]),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });

        let camera_bind_group = shared.gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            layout: &shared.camera_bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: camera_buffer.as_entire_binding(),
            }],
            label: Some("camera bind group"),
        });

        // Edge-fade uniform: layout matches FadeUniform in shader.wgsl —
        // top.xy + bottom.xy + viewport.xy + bg_uv.xy = 4*vec4 = 64 bytes.
        let fade_buffer = shared.gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("fade uniform"),
            size: 64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let fade_bind_group = shared.gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            layout: &shared.fade_bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: fade_buffer.as_entire_binding(),
            }],
            label: Some("fade bind group"),
        });

        // Buffers are sized to the adopted tab's grid so the vertex builder
        // (which iterates `terminal.cols/rows`) can't overrun them. The tab
        // was created at this window's computed viewport (see `run()` /
        // Cmd-N), so these dims match.
        let cols = initial_tab.terminal.cols;
        let rows = initial_tab.terminal.rows;
        // Each cell contributes two quads (background + glyph) = 8 verts.
        // Slack covers four phantom rows (two top + two bottom) used during
        // smooth scrolling, the cursor quad, and the two edge-fade quads.
        // The exact formula lives in `grid_buffer_byte_sizes` so the init
        // and resize paths can't drift apart (which they did — pre-fix the
        // resize path allocated half the slack, and the next time
        // `update_vertices` ran near a scroll edge, `queue.write_buffer`
        // panicked with a "Copy ... would end up overrunning" validation
        // error).
        let (vbuf_bytes, ibuf_bytes) =
            grid_buffer_byte_sizes(cols, rows);
        let vertex_buf: Vec<u8> = vec![0; vbuf_bytes];
        let vertex_buffer = shared.gpu.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("vertex buffer"),
            contents: &bytemuck::cast_slice(&vertex_buf),
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        });
        let index_buf: Vec<u8> = vec![0; ibuf_bytes];
        let index_buffer = shared.gpu.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("index buffer"),
            contents: &bytemuck::cast_slice(&index_buf),
            usage: wgpu::BufferUsages::INDEX | wgpu::BufferUsages::COPY_DST,
        });

        // Three strip quads max (top opaque, top gradient, bottom gradient) ⇒
        // 12 vertices, 18 indices. Sized generously so resize never reallocs.
        let strip_vertex_buffer = shared.gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("strip vertex buffer"),
            size: (32 * std::mem::size_of::<renderer::vertex::Vertex>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let strip_index_buffer = shared.gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("strip index buffer"),
            size: (64 * std::mem::size_of::<u16>()) as u64,
            usage: wgpu::BufferUsages::INDEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let mut blur = renderer::blur::BlurChain::new(
            &shared.gpu.device,
            &shared.blur_pipelines,
            surface.config.width,
            surface.config.height,
        );
        blur.write_uniforms(&shared.gpu.queue, surface.config.width, surface.config.height);
        blur.iterations = config.blur_iterations.max(1);
        sub!("BlurChain::new");

        let mut glow = renderer::glow::Glow::new(
            &shared.gpu.device,
            &shared.glow_pipelines,
            surface.config.width,
            surface.config.height,
            &blur.scene.view,
        );
        // Second offscreen scene for the FG layer (glyphs + cursor + overlays).
        // Same format/size as `blur.scene` so the same blit and glow shaders
        // can sample either one without pipeline divergence.
        let scene_fg = SceneTarget::new(
            &shared.gpu.device,
            surface.config.format,
            surface.config.width,
            surface.config.height,
            "scene fg",
        );
        let scene_fg_blit_bg =
            shared.blur_pipelines.make_blit_bind_group(&shared.gpu.device, &scene_fg.view, "scene fg blit bg");

        // Two Glow instances — one bound to the BG scene, one to the FG
        // scene. Identical params/palette/foreground, written below.
        let mut glow_fg = renderer::glow::Glow::new(
            &shared.gpu.device,
            &shared.glow_pipelines,
            surface.config.width,
            surface.config.height,
            &scene_fg.view,
        );
        sub!("Glow::new x2 + scene_fg");
        let initial_overrides = palette::get().glow;
        for g in [&mut glow, &mut glow_fg] {
            apply_glow_config(g, &config, &initial_overrides);
        }
        // Bright-ANSI matching needs the palette's hue table; foreground
        // matching needs the foreground RGB. Palette is installed before
        // WindowState::new (see `run()`), so this reads the active scheme — or
        // the defaults if no scheme was configured.
        {
            let p = palette::get();
            let bright: [[f32; 4]; 8] = [
                p.ansi[8], p.ansi[9], p.ansi[10], p.ansi[11],
                p.ansi[12], p.ansi[13], p.ansi[14], p.ansi[15],
            ];
            for g in [&mut glow, &mut glow_fg] {
                g.set_bright_palette(&shared.gpu.queue, &bright);
                g.set_foreground(p.foreground);
                // Masked composite needs the window bg colour to detect
                // colored cells in the mask texture.
                g.set_background(p.background);
            }
        }
        for g in [&glow, &glow_fg] {
            g.write_uniforms(&shared.gpu.queue, surface.config.width, surface.config.height);
            g.write_glow_params(&shared.gpu.queue);
        }
        // Both glows mask against the bg scene: the halo only appears
        // where bg is transparent (the window's default background), so
        // it can't paint over colored cell backgrounds and visually
        // shift their apparent colour.
        let glow_bg_mask = shared.glow_pipelines.make_mask_bind_group(
            &shared.gpu.device,
            &blur.scene.view,
            "glow bg mask (bg scene)",
        );
        let glow_fg_mask = shared.glow_pipelines.make_mask_bind_group(
            &shared.gpu.device,
            &blur.scene.view,
            "glow fg mask (bg scene)",
        );
        // Scanline overlay's masked mask samples both layers so it can
        // tell glyphs on default-bg cells from truly empty pixels.
        let scanline_overlay_mask = shared.glow_pipelines.make_overlay_mask_bind_group(
            &shared.gpu.device,
            &blur.scene.view,
            &scene_fg.view,
            "scanline overlay mask (bg + fg)",
        );

        // Image pipeline — drawn into `blur.scene` between bg cells and the
        // fg layer, so images participate in glow + edge blur the same way
        // colored bg cells do.
        let image_pipeline = renderer::images::ImagePipeline::new(
            &shared.gpu.device,
            surface.config.format,
            &shared.camera_bind_group_layout,
        );
        sub!("ImagePipeline::new");

        Self {
            surface,
            window,
            shared,
            atlas,
            wireframe: false,
            vertex_buffer,
            index_buffer,
            num_indices: 0,
            num_bg_indices: 0,
            strip_vertex_buffer,
            strip_index_buffer,
            num_strip_indices: 0,
            image_pipeline,
            blur,
            glow,
            glow_fg,
            scene_fg,
            scene_fg_blit_bg,
            glow_bg_mask,
            glow_fg_mask,
            scanline_overlay_mask,
            font_bind_group,
            font_texture,
            pt_size,
            dpi,
            config,
            camera,
            camera_uniform,
            camera_buffer,
            camera_bind_group,
            fade_buffer,
            fade_bind_group,
            tabs: vec![initial_tab],
            active: 0,
            modifiers: winit::keyboard::ModifiersState::empty(),
            mouse_x: 0.0,
            mouse_y: 0.0,
            held_button: None,
            blink_on: true,
            last_blink: std::time::Instant::now(),
            top_fade_phase: 0.0,
            bottom_fade_phase: 0.0,
            last_anim_tick: std::time::Instant::now(),
            command_palette: command_palette::CommandPalette::default(),
            search: search::Search::default(),
            last_toolbar_click: None,
            over_toolbar: false,
            // Seeded with the renderer's reserve; refresh_chrome_band() below
            // (and on every resize / scale change) replaces it with the real
            // DPI-scaled native title-bar height.
            chrome_band_px: (WINDOW_PADDING + DECORATOR_HEIGHT) as f64,
            perf: PerfLog::new(),
            vertices_dirty: true,
        }
    }

    /// The active tab (read-only). First cut: always `tabs[0]`.
    #[inline]
    fn active_tab(&self) -> &TabState {
        &self.tabs[self.active]
    }

    /// The active tab (mutable).
    #[inline]
    fn active_tab_mut(&mut self) -> &mut TabState {
        &mut self.tabs[self.active]
    }

    /// Mark the vertex buffer stale and ask winit to redraw. Repeated calls
    /// inside one event-loop turn coalesce into a single RedrawRequested,
    /// and `surface.get_current_texture()` blocks at the swapchain to keep
    /// us aligned with the display's vsync cadence.
    fn invalidate(&mut self) {
        self.vertices_dirty = true;
        self.window.request_redraw();
    }

    /// Per-frame setup. Runs in the redraw handler between `update` (input
    /// processing) and `render` (GPU encode). Resolves the image store's
    /// async state, then rebuilds vertices if stale.
    ///
    /// The ordering matters: poll → retain → vertex build → render. Both
    /// the vertex builder (half-block fallback path) and `render` (GPU
    /// image_draws) consult `Store::peek` for the same set of placements.
    /// If they disagreed on `peek`'s return value within a frame, a decode
    /// that resolved between the two would either double-draw (half-block
    /// behind GPU pixels) or vanish for a frame (vertex builder saw Some,
    /// then mark-and-sweep dropped the slot before render). Doing both
    /// store mutations before either consumer reads guarantees one snapshot
    /// per frame.
    ///
    /// **Invariant:** no caller may mutate `terminal.placements` between
    /// `prepare_frame` and `render` — `retain` has already been computed
    /// against the snapshot we hand to `render`, and any new placement
    /// would slip past it.
    fn prepare_frame(&mut self) {
        // Resolve worker decodes that completed since the last frame. May
        // call `insert_placement` (deferred path success) or
        // `remove_placements_with_image` (pre-placed path failure), both of
        // which mark vertices dirty internally.
        self.poll_pending_images();
        // Mark-and-sweep AFTER poll so a freshly-landed image referenced
        // by a placement created in this same `poll_pending_images` call
        // is kept alive.
        let referenced = self.active_tab().terminal.referenced_image_ids();
        self.active_tab_mut().image_store.retain(&referenced);
        self.flush_vertices();
    }

    /// Rebuild the vertex/index buffers if they're stale, recording the
    /// cost in `perf`. Called from `prepare_frame`; production code should
    /// not call this directly — the image-store snapshot has to be set up
    /// first.
    fn flush_vertices(&mut self) {
        if !self.vertices_dirty {
            return;
        }
        let t = std::time::Instant::now();
        self.update_vertices();
        self.perf.note_update(t.elapsed());
        self.vertices_dirty = false;
    }

    fn get_viewport_size(
        width: f32,
        height: f32,
        advance_x: usize,
        line_height: usize,
    ) -> ViewportSize {
        ViewportSize {
            char_width: usize::max(1, (width - WINDOW_PADDING * 2.0) as usize / advance_x),
            // Content extends full-height (behind the translucent title bar
            // on macOS's fullsize_content_view), gaining ~1–2 rows of
            // scrollable area at the top. Reserve DECORATOR_HEIGHT in the
            // row count so the boundary push-down (see `decorator_offset`
            // in `update_vertices`) never shoves the bottom row past the
            // window edge when the height isn't an integer multiple of
            // `line_height`.
            //
            // Floor at MIN_GRID_ROWS, well above 1. Multi-line shell prompts
            // (a powerline/segment bar, sometimes a blank separator, then the
            // input line — three rows is common) don't fit in a 1–2 row grid.
            // When the window is dragged shorter than the prompt, the prompt
            // spills into scrollback and the shell keeps repainting it in the
            // cramped viewport, scrolling a line into scrollback on every
            // WINCH; growing back then refills all that churn as stray blank /
            // duplicate rows above the prompt. Keeping a few rows resident
            // stops the prompt from ever spilling during a resize. A grid this
            // short is unusable as a terminal anyway, so clipping the bottom of
            // an even shorter window costs nothing real.
            char_height: usize::max(
                MIN_GRID_ROWS,
                (height - WINDOW_PADDING * 2.0 - DECORATOR_HEIGHT) as usize / line_height,
            ),
        }
    }

    fn resize_buffers(&mut self) {
        // Calculate console viewport & buffer sizes
        let metrics = self.shared.with_font(|f| f.face().size_metrics().unwrap());
        let viewport = WindowState::get_viewport_size(
            self.surface.config.width as f32,
            self.surface.config.height as f32,
            self.shared.with_font(|f| f.cell_width()),
            ((metrics.ascender - metrics.descender) >> 6) as usize,
        );
        let (vbuf_bytes, ibuf_bytes) =
            grid_buffer_byte_sizes(viewport.char_width, viewport.char_height);
        let vertex_buf: Vec<u8> = vec![0; vbuf_bytes];
        self.vertex_buffer =
            self.shared.gpu
                .device
                .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("vertex buffer"),
                    contents: &bytemuck::cast_slice(&vertex_buf),
                    usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                });
        let index_buf: Vec<u8> = vec![0; ibuf_bytes];
        self.index_buffer =
            self.shared.gpu
                .device
                .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("index buffer"),
                    contents: &bytemuck::cast_slice(&index_buf),
                    usage: wgpu::BufferUsages::INDEX | wgpu::BufferUsages::COPY_DST,
                });
    }

    // Rebuild the vertex/index buffers for the current terminal state. Emits
    // one bg quad + one glyph quad per cell for the grid, plus a cursor box
    // and the top/bottom edge fades.
    fn update_vertices(&mut self) {
        // Advance any alt-screen scroll slide first so this frame reads the
        // freshly-eased `scroll_y`.
        self.update_alt_scroll();
        let cols = self.active_tab().terminal.cols;
        let rows = self.active_tab().terminal.rows;
        let area = cols * rows;
        let mut vertices: Vec<renderer::vertex::Vertex> = Vec::with_capacity(8 * (area + 1));
        let mut indices: Vec<u32> = Vec::with_capacity(12 * (area + 1));

        let theme = self.window.theme().unwrap_or(winit::window::Theme::Light);
        // All face-derived metrics are pulled in one borrow so the shared
        // font's `Ref` is dropped before the `ensure_*` fill calls below
        // (which take `&mut Font`) — see the `AppShared::font` borrow rule.
        let (line_height, cell_w, bg_h, descender, underline_thickness_px, underline_pos_px) =
            self.shared.with_font(|font| {
                let face = font.face();
                let metrics = face.size_metrics().unwrap();
                let line_height = ((metrics.ascender - metrics.descender) >> 6) as f32;
                let cell_w = font.cell_width() as f32;
                let bg_h = ((metrics.ascender - metrics.descender) >> 6) as f32;
                let descender = (metrics.descender >> 6) as f32;
                // Underline metrics from the font's `post` table. The face
                // values are in font design units; `y_scale` (16.16 fixed)
                // converts to 26.6 px for this size, matching how `ascender` /
                // `descender` above land in 26.6 — divide by 64 once for
                // actual pixels.
                //   - `underline_position`: vertical center of the stem, in
                //     font units. Negative ⇒ below the baseline (the usual
                //     case).
                //   - `underline_thickness`: stem height in font units.
                // Kept as floats; the rasterizer can render a sub-pixel quad
                // across two rows of fragments which reads as a softer-than-1px
                // line and lets the stripe grow smoothly with point size.
                // Fallbacks cover fonts whose `post` table is empty (some
                // bitmap-style monospace TTFs report 0).
                let y_scale = metrics.y_scale as f32 / 65536.0;
                let raw_thick_px = face.underline_thickness() as f32 * y_scale / 64.0;
                let underline_thickness_px = if raw_thick_px > 0.0 {
                    raw_thick_px
                } else {
                    line_height * 0.06
                };
                let raw_pos_px = face.underline_position() as f32 * y_scale / 64.0;
                let underline_pos_px = if face.underline_position() != 0 {
                    raw_pos_px
                } else {
                    descender * 0.5
                };
                (
                    line_height,
                    cell_w,
                    bg_h,
                    descender,
                    underline_thickness_px,
                    underline_pos_px,
                )
            });

        let pal = palette::get();
        let default_fg = pal.foreground;
        // `default_bg` stays fully transparent so the window can show
        // through cells with no SGR background; `default_bg_solid` is the
        // concrete window background, used when reverse-video needs to
        // swap a real color into the foreground slot.
        let default_bg = [0.0, 0.0, 0.0, 0.0];
        let default_bg_solid = pal.background;
        let atlas_w = self.atlas.width as f32;
        let atlas_h = self.atlas.height as f32;
        let bg_u = 1.0 / atlas_w;
        let bg_v = 1.0 / atlas_h;
        let scroll_y = self.active_tab().scroll_y as f32;

        let push_quad =
            |verts: &mut Vec<renderer::vertex::Vertex>,
             idxs: &mut Vec<u32>,
             x: f32,
             y: f32,
             w: f32,
             h: f32,
             uv0: [f32; 2],
             uv1: [f32; 2],
             color: [f32; 4],
             radii: [f32; 4]| {
                let start = verts.len() as u32;
                let hx = w * 0.5;
                let hy = h * 0.5;
                let half_size = [hx, hy];
                verts.push(renderer::vertex::Vertex {
                    position: [x, y, 0.0],
                    tex_coords: [uv0[0], uv0[1]],
                    color,
                    local_pos: [-hx, -hy],
                    half_size,
                    radii,
                });
                verts.push(renderer::vertex::Vertex {
                    position: [x, y + h, 0.0],
                    tex_coords: [uv0[0], uv1[1]],
                    color,
                    local_pos: [-hx, hy],
                    half_size,
                    radii,
                });
                verts.push(renderer::vertex::Vertex {
                    position: [x + w, y, 0.0],
                    tex_coords: [uv1[0], uv0[1]],
                    color,
                    local_pos: [hx, -hy],
                    half_size,
                    radii,
                });
                verts.push(renderer::vertex::Vertex {
                    position: [x + w, y + h, 0.0],
                    tex_coords: [uv1[0], uv1[1]],
                    color,
                    local_pos: [hx, hy],
                    half_size,
                    radii,
                });
                idxs.extend_from_slice(&[start, start + 1, start + 2, start + 1, start + 2, start + 3]);
            };

        // Grid row `r` sits with its baseline at (r+1) * line_height; the
        // glyph box extends up by bearing_y and down by (height - bearing_y).
        // Push content below the translucent title bar at the boundaries of
        // the scroll range — the bottom of the live grid AND the top of
        // scrollback — so the first/last row never sits half-behind the
        // toolbar. Mid-scroll the offset is 0 so older content can flow
        // behind the title bar smoothly. Eases linearly over one line at
        // each boundary. Hit-test in pixel_to_visual_cell mirrors this.
        let view_offset = self.active_tab().terminal.view_offset() as f32;
        // Alt screen has no scrollback to fade toward — pin both distances
        // to zero so the top/bottom edge fades stay invisible.
        let scrollback_len = if self.active_tab().terminal.on_alt_screen() {
            0.0
        } else {
            self.active_tab().terminal.scrollback_len() as f32
        };
        let (dist_from_bottom, dist_from_top) =
            self.edge_fade_dists(scroll_y, view_offset, scrollback_len, line_height);
        let near = (dist_from_bottom / line_height)
            .min(dist_from_top / line_height)
            .clamp(0.0, 1.0);
        let decorator_offset = DECORATOR_HEIGHT * (1.0 - near);
        let row_y = |r: isize| WINDOW_PADDING + decorator_offset + (r as f32 + 1.0) * line_height;
        let col_x = |c: usize| WINDOW_PADDING + c as f32 * cell_w;

        // Two extra rows above and below the visible grid are rendered so
        // smooth sub-line scrolling stays populated through the snap. Used
        // both for shaping (below) and the main emit loop further down. During
        // an alt-screen scroll slide `scroll_y` can exceed a line, so widen the
        // band to cover the departing rows being slid in from the edge.
        let anim_extra = if self.active_tab().terminal.on_alt_screen() {
            (scroll_y.abs() / line_height).ceil() as isize
        } else {
            0
        };
        let r_lo: isize = -2 - anim_extra;
        let r_hi: isize = rows as isize + 2 + anim_extra;

        // Programming-ligature pass. Walks each visible row, prefix-matches
        // each cell against the per-variant ligature table the Shaper
        // pre-built at font load. Mutates atlas (rasterizes ligature
        // glyphs on demand) so it has to run before the emit closure
        // captures &self.atlas immutably below.
        //
        // Fira Code and friends implement ligatures as 1:1 contextual
        // alternates (each char substituted to a half-glyph), not N→1
        // ligature substitutions, so each covered cell still draws at
        // its own column with normal cell width — only the glyph id
        // changes. See `shaper.rs` for the longer story.
        let mut row_overrides: std::collections::HashMap<
            isize,
            Vec<Option<(u32, font::FaceVariant)>>,
        > = std::collections::HashMap::new();
        // Reused across rows — refilled in place to avoid per-row allocation.
        let mut row_chars: Vec<char> = Vec::with_capacity(cols);
        // Borrow the shared shaper once for the whole pass. `match_at` returns
        // a `&Ligature` into it, so the borrow must outlive each match's use;
        // it's disjoint from the `font`/`atlas` fills below (different fields).
        let shaper = self.shared.shaper.borrow();
        for r in r_lo..r_hi {
            row_chars.clear();
            for c in 0..cols {
                let cell = self.active_tab().terminal.extended_cell(r, c);
                let ch = cell.map(|cell| cell.ch).unwrap_or(' ');
                // Pre-pack any char outside build_atlas's fixed ranges
                // (Nerd Font icons in SPUA, CJK, arbitrary symbols) so
                // the render-time lookup below hits the variant chain
                // — including fallback fonts — instead of notdef.
                if let Some(cell) = cell {
                    let variant = font::FaceVariant::from_flags(
                        cell.style.bold,
                        cell.style.italic,
                    );
                    self.atlas.ensure_char(&mut *self.shared.font.borrow_mut(), variant, ch);
                }
                row_chars.push(ch);
            }
            let mut row_override: Option<Vec<Option<(u32, font::FaceVariant)>>> = None;
            let mut c = 0;
            while c < cols {
                let Some(start_cell) = self.active_tab().terminal.extended_cell(r, c) else {
                    c += 1;
                    continue;
                };
                let variant =
                    font::FaceVariant::from_flags(start_cell.style.bold, start_cell.style.italic);
                let lig = match shaper.match_at(&row_chars[c..], variant) {
                    Some(l) => l,
                    None => {
                        c += 1;
                        continue;
                    }
                };
                let span = lig.chars.len();
                // All cells in the ligature must share the start cell's
                // style — a colored or weight-changing split breaks the
                // visual cohesion that contextual-alternate halves rely on.
                let style_uniform = (1..span).all(|i| {
                    self.active_tab().terminal
                        .extended_cell(r, c + i)
                        .map(|cell| cell.style == start_cell.style)
                        .unwrap_or(false)
                });
                if !style_uniform {
                    c += 1;
                    continue;
                }
                // Rasterize every output glyph into the atlas so the
                // override lookup at render time is a hit. If any one
                // glyph fails to load, abandon the substitution for this
                // span (better to render the chars than render half a
                // ligature).
                let all_ok = lig.output_glyphs.iter().all(|gid| {
                    self.atlas.ensure_glyph_id(&mut *self.shared.font.borrow_mut(), variant, *gid)
                });
                if !all_ok {
                    c += 1;
                    continue;
                }
                let over = row_override.get_or_insert_with(|| (0..cols).map(|_| None).collect());
                for (i, gid) in lig.output_glyphs.iter().enumerate() {
                    if c + i < cols {
                        over[c + i] = Some((*gid, variant));
                    }
                }
                c += span;
            }
            if let Some(over) = row_override {
                row_overrides.insert(r, over);
            }
        }

        // Rasterize any glyphs the completion popup will draw that the grid
        // didn't already pack this frame, so the immutable-`atlas` lookup in
        // `emit_text_run` below is a hit (and the dirty flag triggers the
        // re-upload right after). Done here, before the `&self.atlas` borrow,
        // because `ensure_char` needs `&mut self.atlas` and a `&mut Font`
        // (taken as a single-statement `borrow_mut` per the AppShared rule).
        if !self.active_tab().completions.is_empty() {
            let chars: Vec<char> = self.active_tab()
                .completions
                .iter()
                .flat_map(|s| s.text.chars())
                .chain(std::iter::once('…'))
                .collect();
            for ch in chars {
                self.atlas
                    .ensure_char(&mut *self.shared.font.borrow_mut(), font::FaceVariant::Regular, ch);
            }
        }

        // Re-upload the atlas texture if the shaping pass rasterized any
        // new glyphs. write_texture reuses the existing GPU texture and
        // bind group — no need to recreate either.
        if self.atlas.dirty {
            self.shared.gpu.queue.write_texture(
                wgpu::ImageCopyTexture {
                    texture: &self.font_texture.texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                &self.atlas.buffer,
                wgpu::ImageDataLayout {
                    offset: 0,
                    bytes_per_row: Some(self.atlas.width as u32),
                    rows_per_image: Some(self.atlas.height as u32),
                },
                wgpu::Extent3d {
                    width: self.atlas.width as u32,
                    height: self.atlas.height as u32,
                    depth_or_array_layers: 1,
                },
            );
            self.atlas.dirty = false;
        }

        let atlas = &self.atlas;
        // Foreground glyph source for a cell: a single char (existing
        // per-char path) or a font-internal glyph id (contextual
        // alternate from a programming ligature). Both render at the
        // cell's own column with normal cell width — Fira Code's
        // ligatures are per-cell substitutions, not wide N→1 glyphs.
        #[derive(Copy, Clone)]
        enum GlyphSource {
            Char(char),
            Substituted(u32),
        }
        // BG quad only — used by the bg-layer pass. Pulled out so we can
        // emit all cell backgrounds contiguously, record the boundary in
        // `num_bg_indices`, then emit all foreground content (glyphs,
        // cursor, overlays) after. The renderer issues two draw_indexed
        // calls against the resulting buffer so the glow pipeline can
        // bloom each layer independently.
        let strip_pad = (line_height - bg_h) * 0.5;

        // Alt-screen scroll slide: the offset applies only to rows inside the
        // moving span (the scroll region plus the departing band on the moving
        // edge); rows outside it — a reserved status line below the region —
        // stay put. Off the slide (scrollback smooth-scroll on the primary),
        // every row shares the global `scroll_y`. `clip_bottom_px` keeps a
        // moving row from drawing past the region's bottom edge, so incoming /
        // departing content slides *under* the static status line instead of
        // bleeding glyphs over it; for a full-height region it sits below the
        // window and clips nothing.
        let (anim_lo, anim_hi, clip_bottom_px) = match &self.active_tab().alt_scroll_anim {
            Some(a) => {
                let d = a.rows as isize;
                // Up-scroll departing rows sit above the region top (off-grid
                // when the region is anchored at row 0, which is the only case
                // we animate). Down-scroll departing rows sit below the region
                // bottom — include them in the moving span only when that's
                // off-grid (full-height region); otherwise they coincide with a
                // static status line that must not move.
                let lo = a.region_top as isize - if a.up { d } else { 0 };
                let hi = a.region_bottom as isize
                    + if !a.up && a.region_bottom + 1 == rows { d } else { 0 };
                let clip = row_y(a.region_bottom as isize + 1) - bg_h - descender - strip_pad;
                (lo, hi, clip)
            }
            None => (0, 0, f32::INFINITY),
        };
        let anim_active = self.active_tab().alt_scroll_anim.is_some();
        let row_moving = move |r: isize| anim_active && r >= anim_lo && r <= anim_hi;
        // Per-row vertical offset. When no slide is active this is the global
        // `scroll_y` for every row (unchanged scrollback behavior).
        let row_scroll = move |r: isize| {
            if !anim_active || row_moving(r) {
                scroll_y
            } else {
                0.0
            }
        };
        // Clamp `(y, h, v0, v1)` so a moving row's quad never extends past the
        // region's bottom edge. Returns `None` if fully clipped. `v0`/`v1` are
        // adjusted proportionally so glyph bitmaps clip cleanly (bg quads pass
        // `v0 == v1`, leaving the single sampled texel unchanged).
        let clip_row_quad = move |r: isize, y: f32, h: f32, v0: f32, v1: f32| {
            if !row_moving(r) || y + h <= clip_bottom_px {
                return Some((h, v1));
            }
            let visible = clip_bottom_px - y;
            if visible <= 0.0 {
                return None;
            }
            (Some((visible, v0 + (v1 - v0) * (visible / h)))).filter(|_| h > 0.0)
        };

        let emit_bg_for_cell = |verts: &mut Vec<renderer::vertex::Vertex>,
                                idxs: &mut Vec<u32>,
                                r: isize,
                                c: usize,
                                bg: [f32; 4]| {
            let x = col_x(c);
            let baseline_y = row_y(r);
            // Background quad spans one line-height strip, centered on the
            // typographic glyph extent. Centering matters when line_height
            // differs from (ascender − descender): top-anchoring would float
            // glyphs to the bottom of the strip on tall-line fonts, while
            // anchoring to the glyph extent risks overlap on tight-line ones.
            // Strip stride = line_height, so adjacent rows still tile cleanly.
            let bg_y = baseline_y - bg_h - descender - strip_pad + row_scroll(r);
            let Some((bg_height, _)) = clip_row_quad(r, bg_y, line_height, bg_v, bg_v) else {
                return;
            };
            push_quad(
                verts,
                idxs,
                x,
                bg_y,
                cell_w,
                bg_height,
                [bg_u, bg_v],
                [bg_u, bg_v],
                bg,
                [0.0; 4],
            );
        };

        // FG quad only — glyph for the cell. See `emit_bg_for_cell` above
        // for why bg/fg are split.
        let emit_fg_for_cell = |verts: &mut Vec<renderer::vertex::Vertex>,
                                idxs: &mut Vec<u32>,
                                fg_source: GlyphSource,
                                variant: font::FaceVariant,
                                r: isize,
                                c: usize,
                                fg: [f32; 4]| {
            let x = col_x(c);
            let baseline_y = row_y(r);
            let off = row_scroll(r);
            let bg_y = baseline_y - bg_h - descender - strip_pad + off;
            // Foreground glyph. The per-cell substitution case (Fira
            // Code-style contextual alternates) deliberately uses
            // glyphs whose side bearings extend past the cell edges so
            // adjacent halves visually fuse. The normal-char path's
            // fills_h UV-clipping (added for box-drawing) cuts off
            // exactly that overlap, so we disable it for substituted
            // glyphs.
            let (g, allow_overhang) = match fg_source {
                GlyphSource::Char(ch) => (atlas.lookup(ch, variant), false),
                GlyphSource::Substituted(glyph_id) => {
                    (atlas.lookup_glyph_id(glyph_id, variant), true)
                }
            };
            let span_w = cell_w;
            if g.width > 0 && g.height > 0 {
                // Cell-filling glyphs (Powerline caps, box-drawing,
                // half-blocks) get the affected axis stretched to the cell's
                // full extent. The rasterized bitmap can be a pixel shorter
                // than the typographic cell on a filling axis — drawing the
                // quad at cell extent there and letting the linear-filtered
                // sampler stretch the bitmap into it closes the gap. Each
                // axis is independent so e.g. ▐ (full-height, half-width)
                // gets vertical stretching without distorting horizontally.
                //
                // Gated on codepoint range so a generic glyph that happens to
                // fill both axes (e.g. ⏺ U+23FA, a near-square circle) isn't
                // stretched to the non-square cell aspect — that distortion
                // turns a round glyph into an oval. Only the ranges whose
                // glyphs are *designed* to tile across cell edges opt in:
                // box-drawing + block-elements (synthesized in this binary)
                // and the Powerline/separator slice of PUA.
                let bx = g.bearing_x as f32;
                let by = g.bearing_y as f32;
                let asc_eff = bg_h + descender; // pixels above baseline (descender is negative)
                let cell_filling = match fg_source {
                    GlyphSource::Char(ch) => {
                        let cp = ch as u32;
                        (0x2500..=0x259F).contains(&cp) || (0xE000..=0xE0FF).contains(&cp)
                    }
                    GlyphSource::Substituted(_) => false,
                };
                let fills_h = cell_filling && !allow_overhang
                    && g.width as f32 >= span_w * 0.85;
                let fills_v = cell_filling && g.height as f32 >= line_height * 0.85;
                let (gx, gw, q_start, q_end) = if fills_h {
                    // Restrict UV to the in-cell columns so a glyph designed
                    // to bleed into an adjacent cell (negative bearing or
                    // bitmap_width > cell_w) doesn't put its transparent
                    // overhang at the cell's left/right edge.
                    let q_start = (-bx).max(0.0).min(g.width as f32);
                    let q_end = (span_w - bx).max(0.0).min(g.width as f32);
                    (x, span_w, q_start, q_end)
                } else {
                    (x + bx, g.width as f32, 0.0, g.width as f32)
                };
                let (gy, gh, p_start, p_end) = if fills_v {
                    let p_start = (by - asc_eff).max(0.0).min(g.height as f32);
                    let p_end = (by - descender).max(0.0).min(g.height as f32);
                    (bg_y, line_height, p_start, p_end)
                } else {
                    (
                        baseline_y - by + off,
                        g.height as f32,
                        0.0,
                        g.height as f32,
                    )
                };
                // Half-texel inset on a stretched (cell-filling) axis. The
                // glyph is packed with one transparent column/row of padding
                // (`stride = w + 1` in font.rs), so sampling right up to the
                // texel boundary `g.x + g.width` makes the Linear filter
                // average the opaque edge with that transparent neighbor —
                // ~50% alpha along the seam, which reads as a hairline gap
                // between abutting blocks (and varies with the bitmap→cell
                // stretch ratio, hence "only at certain font sizes"). Pulling
                // the UV in by half a texel keeps every edge fragment on a
                // fully-opaque texel center. Only the filling axis is inset:
                // the non-filling axis is placed 1:1 and must keep its true
                // extent so normal glyphs aren't thinned.
                let (u0, v0, u1, v1) = Self::glyph_quad_uv(
                    g.x as f32,
                    g.y as f32,
                    (q_start, q_end),
                    (p_start, p_end),
                    cell_filling,
                    atlas_w,
                    atlas_h,
                );
                // Clip a moving glyph at the region's bottom edge so it slides
                // under the static status line rather than over it.
                let Some((gh, v1)) = clip_row_quad(r, gy, gh, v0, v1) else {
                    return;
                };
                push_quad(
                    verts,
                    idxs,
                    gx,
                    gy,
                    gw,
                    gh,
                    [u0, v0],
                    [u1, v1],
                    fg,
                    [0.0; 4],
                );
            }
        };

        // Selection highlight: translucent macOS text-selection blue, drawn
        // as an overlay on top of cells. Uses premultiplied alpha so RGB is
        // pre-scaled by alpha.
        let selection_alpha: f32 = match theme {
            winit::window::Theme::Light => 0.30,
            winit::window::Theme::Dark => 0.35,
        };
        let sel = palette::get().selection;
        let selection_bg = [
            sel[0] * selection_alpha,
            sel[1] * selection_alpha,
            sel[2] * selection_alpha,
            selection_alpha,
        ];
        let selection = self.active_tab().selection;

        // 0b. Half-block fallback overrides for image placements that
        // the GPU image pipeline can't draw this frame (config-disabled
        // or decode-failed). One entry per affected cell holds the
        // top/bottom half-pixel colors for a U+2580 ▀ glyph. Built only
        // when `images_halfblock_for_missing` is on; empty otherwise so
        // the lookup below is a single hash miss in the default case.
        let halfblock_overrides: std::collections::HashMap<(isize, usize), images::HalfblockCell> =
            self.halfblock_overrides();

        // 1. Terminal grid + phantom rows on each side (`r_lo..r_hi` defined
        // above where the shaping pass lives — same range so ligature
        // covers and emits stay in sync).
        //
        // Resolve every visible cell once, emit all bg quads, record the
        // boundary index, then emit all fg glyphs. The renderer issues two
        // draw_indexed calls against the resulting buffer (bg layer +
        // fg-and-overlays layer) so glow can bloom each layer independently.
        struct ResolvedCell {
            fg_source: GlyphSource,
            variant: font::FaceVariant,
            r: isize,
            c: usize,
            fg: [f32; 4],
        }
        let mut resolved: Vec<ResolvedCell> = Vec::with_capacity(rows * cols);
        // Optional per-scheme override for glyph color inside the selection.
        // Resolved once: `None` short-circuits the per-cell membership test
        // so the common (unselected / no-override) path stays branch-cheap.
        let selection_fg = pal.selection_fg;
        let selection_range = selection.as_ref().map(|s| s.range());
        for r in r_lo..r_hi {
            let over = row_overrides.get(&r);
            // Selection strip on this row (inclusive cols), or None if the
            // row falls outside the selection. Mirrors `strip_at` further
            // down where the overlay quads are emitted.
            let sel_strip = selection_range.and_then(|(start, end)| {
                let abs_line = self.active_tab().terminal.visual_to_abs_line(r);
                if abs_line < start.0 || abs_line > end.0 {
                    return None;
                }
                let from = if abs_line == start.0 { start.1 } else { 0 };
                let to = if abs_line == end.0 { end.1 } else { cols - 1 };
                if from > to || from >= cols { None } else { Some((from, to.min(cols - 1))) }
            });
            for c in 0..cols {
                // Half-block fallback: substitute the underlying cell
                // (typically a blank reserved by the placement) with a
                // ▀ glyph whose fg/bg pull from the preview's two
                // half-pixels. Wins over any other cell content because
                // the image placement *owns* these cells — there's no
                // real text the user expects to see here.
                if let Some(hb) = halfblock_overrides.get(&(r, c)) {
                    emit_bg_for_cell(&mut vertices, &mut indices, r, c, hb.bg);
                    // Image-replacement glyphs honour selection_fg too so a
                    // selection that runs through an image preview keeps a
                    // consistent text color.
                    let fg = match (selection_fg, sel_strip) {
                        (Some(sfg), Some((from, to))) if c >= from && c <= to => sfg,
                        _ => hb.fg,
                    };
                    resolved.push(ResolvedCell {
                        fg_source: GlyphSource::Char(images::HALFBLOCK_CHAR),
                        variant: font::FaceVariant::Regular,
                        r,
                        c,
                        fg,
                    });
                    continue;
                }
                let Some(cell) = self.active_tab().terminal.extended_cell(r, c) else { continue };
                // Kitty unicode-placeholder cells (`U+10EEEE` + image-id
                // encoded in fg). The image quad draws over this cell on
                // its own pipeline pass — emitting the U+10EEEE glyph
                // and the cell's fg-as-id color would just paint tofu
                // and ID-colored background on top of the image.
                if cell.placeholder_image_id.is_some() {
                    continue;
                }
                // SGR 7 (reverse) swaps fg/bg. Resolve unset colors to concrete
                // theme defaults before swapping — `default_bg` is transparent
                // so the window shows through, but reverse needs a solid bg
                // that the swap can move to fg (otherwise reverse-video text
                // and Claude Code's reverse-space cursor render invisible).
                let (fg, bg) = if cell.style.reverse {
                    let rfg = cell.style.color_fg.unwrap_or(default_fg);
                    let rbg = cell.style.color_bg.unwrap_or(default_bg_solid);
                    (rbg, rfg)
                } else {
                    (
                        cell.style.color_fg.unwrap_or(default_fg),
                        cell.style.color_bg.unwrap_or(default_bg),
                    )
                };
                // Quantise to whatever the scheme advertises (e.g. Mono /
                // Ansi16). Identity for the common Truecolor cap. Done
                // after reverse so reverse-video respects the cap too;
                // applied only to cell colors — cursor, selection, and
                // chrome remain at scheme-author fidelity. `project_cell` also
                // handles the Mono special case where a cell with an explicit
                // background is flipped to fg-ink-on-bg-text so it stays
                // distinct from an empty cell (see palette::Palette).
                let (fg, bg) = pal.project_cell(fg, bg);
                // Scheme-provided selection_fg wins over the cell's own fg
                // (including the post-reverse swap). Applied after projection
                // so it stays at scheme-author fidelity, matching how
                // cursor/selection chrome behave.
                let fg = match (selection_fg, sel_strip) {
                    (Some(sfg), Some((from, to))) if c >= from && c <= to => sfg,
                    _ => fg,
                };
                let variant = font::FaceVariant::from_flags(cell.style.bold, cell.style.italic);
                // Ligature pass may have substituted this cell's glyph.
                let fg_source = match over.and_then(|cs| cs[c]) {
                    Some((glyph_id, _v)) => GlyphSource::Substituted(glyph_id),
                    None => GlyphSource::Char(cell.ch),
                };
                emit_bg_for_cell(&mut vertices, &mut indices, r, c, bg);
                resolved.push(ResolvedCell { fg_source, variant, r, c, fg });
            }
        }
        // Everything emitted before this point is the BG layer. Glow runs
        // separately on bg vs fg, so the renderer needs this split index
        // to know where one layer's draw call ends and the next begins.
        let num_bg_indices = indices.len() as u32;
        for rc in &resolved {
            emit_fg_for_cell(
                &mut vertices,
                &mut indices,
                rc.fg_source,
                rc.variant,
                rc.r,
                rc.c,
                rc.fg,
            );
        }

        // 1a. Cmd-hover URL underline. Drawn on top of the glyph row so the
        // line is visible regardless of cell bg, and below the selection
        // overlay (1b) so a selected URL still reads as selected. Walks the
        // phantom-row range like the cell loop so the underline follows the
        // text through smooth scroll.
        if let Some(hu) = &self.active_tab().hover_url {
            for r in r_lo..r_hi {
                let abs_line = self.active_tab().terminal.visual_to_abs_line(r);
                // Underline every segment that lands on this line. Most links
                // have one per line; an OSC 8 link with an `id=` shared across
                // non-contiguous spans can have several, so don't stop early.
                for seg in hu.segments.iter().filter(|seg| seg.abs_line == abs_line) {
                    let from = seg.start_col;
                    if from >= cols {
                        continue;
                    }
                    let last = seg.end_col.min(cols - 1);
                    if last < from {
                        continue;
                    }
                    let ux = col_x(from);
                    let uw = (last - from + 1) as f32 * cell_w;
                    // Honor the font's own underline_position / underline_thickness
                    // so the line lands where the type designer intended and scales
                    // with point size. `underline_pos_px` is the (signed) offset of
                    // the stem center from the baseline — negative means below, so
                    // adding `-pos` walks downward in screen coords. Subtracting
                    // half the thickness then gives the top edge of the stripe.
                    let uh = underline_thickness_px;
                    let uy = row_y(r) - underline_pos_px - uh * 0.5 + row_scroll(r);
                    // Drop a moving row's underline once it crosses the region's
                    // bottom edge so it can't streak across the static status line.
                    if row_moving(r) && uy >= clip_bottom_px {
                        continue;
                    }
                    // Match the cell's foreground color so the underline tracks
                    // theme overrides; fall back to the default fg.
                    let fg = self.active_tab()
                        .terminal
                        .extended_cell(r, from)
                        .map(|cell| {
                            if cell.style.reverse {
                                cell.style.color_bg.unwrap_or(default_bg_solid)
                            } else {
                                cell.style.color_fg.unwrap_or(default_fg)
                            }
                        })
                        .unwrap_or(default_fg);
                    push_quad(
                        &mut vertices,
                        &mut indices,
                        ux,
                        uy,
                        uw,
                        uh,
                        [bg_u, bg_v],
                        [bg_u, bg_v],
                        fg,
                        [0.0; 4],
                    );
                }
            }
        }

        // 1b. Selection overlay. Each row's selected range is rendered as a
        // translucent strip; corner radii adapt to the neighbor rows so the
        // multi-row shape reads as one continuous form. Outer corners round
        // outward (convex), inner L-step corners round inward via a fillet
        // quad, and corners on a continuous vertical edge stay flat.
        if let Some(sel) = selection.as_ref() {
            let (start, end) = sel.range();
            // Outer convex corners get a generous radius for a soft pill
            // shape; inner concave fillets stay tighter so the L-step
            // joins read as a subtle curve rather than a deep bite.
            let convex_radius = (line_height * 0.35).min(cell_w * 0.7);
            let concave_radius = (line_height * 0.18).min(cell_w * 0.45);
            let strip_pad = (line_height - bg_h) * 0.5;

            // Range of selected columns on the row at `abs_line`, or `None`
            // if that line is outside the selection. Inclusive on both ends.
            let strip_at = |abs_line: isize| -> Option<(usize, usize)> {
                if abs_line < start.0 || abs_line > end.0 {
                    return None;
                }
                let from = if abs_line == start.0 { start.1 } else { 0 };
                let to = if abs_line == end.0 { end.1 } else { cols - 1 };
                if from > to || from >= cols {
                    None
                } else {
                    Some((from, to.min(cols - 1)))
                }
            };

            // Match the cell loop's phantom range so a partially-scrolled
            // row keeps its selection strip drawn through the slide.
            for r in r_lo..r_hi {
                let abs_line = self.active_tab().terminal.visual_to_abs_line(r);
                let Some((from, to)) = strip_at(abs_line) else { continue };
                let prev = strip_at(abs_line - 1);
                let next = strip_at(abs_line + 1);

                // The corner at column `to + 1` looks at column `to` in the
                // neighbor (the cell whose right edge meets the corner).
                let tl = classify_corner_with_neighbor(from, prev, HorizSide::Left);
                let tr = classify_corner_with_neighbor(to, prev, HorizSide::Right);
                let bl = classify_corner_with_neighbor(from, next, HorizSide::Left);
                let br = classify_corner_with_neighbor(to, next, HorizSide::Right);

                let r_tl = if tl == CornerType::Convex { convex_radius } else { 0.0 };
                let r_tr = if tr == CornerType::Convex { convex_radius } else { 0.0 };
                let r_bl = if bl == CornerType::Convex { convex_radius } else { 0.0 };
                let r_br = if br == CornerType::Convex { convex_radius } else { 0.0 };

                let sx = col_x(from);
                let sw = (to - from + 1) as f32 * cell_w;
                let sy = row_y(r) - bg_h - descender - strip_pad + row_scroll(r);
                push_quad(
                    &mut vertices,
                    &mut indices,
                    sx,
                    sy,
                    sw,
                    line_height,
                    [bg_u, bg_v],
                    [bg_u, bg_v],
                    selection_bg,
                    [r_tr, r_br, r_tl, r_bl],
                );

                // Concave fillets: each is an r×r quad in the unselected
                // quadrant adjacent to the strip's concave corner. The
                // negative radius slot tells the shader where to place the
                // quarter-circle bite (at the rect corner farthest from the
                // strip's concave corner).
                let cr = concave_radius;
                let push_fillet = |vertices: &mut Vec<renderer::vertex::Vertex>,
                                   indices: &mut Vec<u32>,
                                   fx: f32,
                                   fy: f32,
                                   bite: [f32; 4]| {
                    push_quad(
                        vertices,
                        indices,
                        fx,
                        fy,
                        cr,
                        cr,
                        [bg_u, bg_v],
                        [bg_u, bg_v],
                        selection_bg,
                        bite,
                    );
                };

                let strip_top = sy;
                let strip_bottom = sy + line_height;
                let left_edge = sx;
                let right_edge = sx + sw;
                if tl == CornerType::Concave {
                    // Bite cut at fillet's BL (radii.w).
                    push_fillet(
                        &mut vertices,
                        &mut indices,
                        left_edge - cr,
                        strip_top,
                        [0.0, 0.0, 0.0, -cr],
                    );
                }
                if tr == CornerType::Concave {
                    // Bite at fillet's BR (radii.y).
                    push_fillet(
                        &mut vertices,
                        &mut indices,
                        right_edge,
                        strip_top,
                        [0.0, -cr, 0.0, 0.0],
                    );
                }
                if bl == CornerType::Concave {
                    // Bite at fillet's TL (radii.z).
                    push_fillet(
                        &mut vertices,
                        &mut indices,
                        left_edge - cr,
                        strip_bottom - cr,
                        [0.0, 0.0, -cr, 0.0],
                    );
                }
                if br == CornerType::Concave {
                    // Bite at fillet's TR (radii.x).
                    push_fillet(
                        &mut vertices,
                        &mut indices,
                        right_edge,
                        strip_bottom - cr,
                        [-cr, 0.0, 0.0, 0.0],
                    );
                }
            }
        }

        // 1b. Find-in-scrollback match highlights. Translucent rounded quads
        //     over each matched run; the current (stepped-to) match gets a
        //     stronger fill drawn last so it reads as emphasised. Mapped from a
        //     match's absolute line to a visible row the same way the selection
        //     strip is, and only drawn for matches inside the phantom range.
        if self.search.open && !self.search.matches.is_empty() {
            let pal = palette::get();
            let strip_pad = (line_height - bg_h) * 0.5;
            let yellow = pal.ansi[3];
            let hl_radius = (cell_w * 0.18).min(line_height * 0.25);
            let all_a = 0.32_f32;
            let all_color = [
                yellow[0] * all_a,
                yellow[1] * all_a,
                yellow[2] * all_a,
                all_a,
            ];
            let cur_a = 0.62_f32;
            let cur_color = [
                yellow[0] * cur_a,
                yellow[1] * cur_a,
                yellow[2] * cur_a,
                cur_a,
            ];
            let top_abs = self.active_tab().terminal.visual_to_abs_line(0);
            let emit_match = |vertices: &mut Vec<renderer::vertex::Vertex>,
                              indices: &mut Vec<u32>,
                              m: &search::Match,
                              color: [f32; 4]| {
                let r = m.line - top_abs;
                if r < r_lo || r >= r_hi {
                    return;
                }
                let from = m.start_col.min(cols.saturating_sub(1));
                let to = m.end_col.min(cols.saturating_sub(1));
                if from > to {
                    return;
                }
                let sx = col_x(from);
                let sw = (to - from + 1) as f32 * cell_w;
                let sy = row_y(r) - bg_h - descender - strip_pad + row_scroll(r);
                push_quad(
                    vertices,
                    indices,
                    sx,
                    sy,
                    sw,
                    line_height,
                    [bg_u, bg_v],
                    [bg_u, bg_v],
                    color,
                    [hl_radius; 4],
                );
            };
            for (i, m) in self.search.matches.iter().enumerate() {
                if i == self.search.current {
                    continue; // drawn last, on top
                }
                emit_match(&mut vertices, &mut indices, m, all_color);
            }
            if let Some(cur) = self.search.current_match() {
                emit_match(&mut vertices, &mut indices, &cur, cur_color);
            }
        }

        // 1c. OSC 133 prompt-status gutter. A short rounded vertical bar in
        //     the left window padding at each prompt's row, colored by the
        //     command's exit status — green for success, red for failure, and
        //     a dim foreground tint while a command is still running (or the
        //     shell reported no code). Drawn in the padding so it never
        //     overlaps cell content. Off unless `prompt_gutter` opts in.
        let status_markers = if self.config.prompt_gutter == PromptGutter::None {
            Vec::new()
        } else {
            self.active_tab().terminal.prompt_status_markers()
        };
        if !status_markers.is_empty() {
            let pal = palette::get();
            let bar_w = (cell_w * 0.16).clamp(2.0, 4.0);
            let bar_x = (WINDOW_PADDING - bar_w) * 0.5; // centered in the padding
            let strip_pad = (line_height - bg_h) * 0.5;
            let bar_radius = bar_w * 0.5;
            for r in r_lo..r_hi {
                let abs_line = self.active_tab().terminal.visual_to_abs_line(r);
                let Some((_, status)) = status_markers.iter().find(|(l, _)| *l == abs_line)
                else {
                    continue;
                };
                let color = match status {
                    terminal::PromptStatus::Success => pal.ansi[2], // green
                    terminal::PromptStatus::Failure => pal.ansi[1], // red
                    terminal::PromptStatus::Pending => {
                        let fg = pal.foreground;
                        [fg[0], fg[1], fg[2], fg[3] * 0.35]
                    }
                };
                // Inset a little from the row's top/bottom so the bar reads as
                // a marker rather than filling the line.
                let inset = line_height * 0.18;
                let sy = row_y(r) - bg_h - descender - strip_pad + row_scroll(r) + inset;
                let sh = (line_height - 2.0 * inset).max(2.0);
                push_quad(
                    &mut vertices,
                    &mut indices,
                    bar_x,
                    sy,
                    bar_w,
                    sh,
                    [bg_u, bg_v],
                    [bg_u, bg_v],
                    color,
                    [bar_radius; 4],
                );
            }
        }

        // 2. Cursor box, only when the live cursor row is actually visible on
        //    screen (scrollback may have pushed it off the bottom). Shape
        //    follows DECSCUSR — block, underline, or bar. The displayed
        //    position eases in cell-space toward the logical position via
        //    `cursor_anim` so typing/navigation slides instead of snapping.
        //    Cells along the path that just went non-blank → blank (e.g.
        //    backspace overwriting with space) are captured as fading
        //    `cursor_ghosts` so the deleted glyph dissolves under the slide
        //    instead of vanishing the instant the cursor starts moving.
        let viewport_key = ViewportKey {
            rows,
            cols,
            view_offset: self.active_tab().terminal.view_offset(),
            on_alt_screen: self.active_tab().terminal.on_alt_screen(),
        };
        // A viewport change (resize, scrollback, alt-screen toggle) makes
        // last frame's snapshot non-comparable cell-for-cell, so we drop
        // any in-flight ghosts and skip detection until we have a fresh
        // matching snapshot to compare against.
        let key_matches = self.active_tab()
            .prev_visible
            .as_ref()
            .map(|s| s.key == viewport_key)
            .unwrap_or(false);
        if !key_matches {
            self.tabs[self.active].cursor_ghosts.clear();
        }

        // The cursor and its ghosts animate in BUFFER coordinates so changes
        // to the user's scroll position (which only shift `view_offset`)
        // don't trigger a slide — they ride along with the rest of the
        // grid. `live_grid_offset` is the integer visual-row delta to apply
        // when converting buffer rows back to viewport pixel space; equal
        // to `scrollback_visible` on the primary screen, 0 on alt screen.
        let live_grid_offset_i = if self.active_tab().terminal.on_alt_screen() {
            0usize
        } else {
            self.active_tab().terminal.view_offset().min(rows)
        };
        let live_grid_offset = live_grid_offset_i as f32;

        // Cursor anchor for the completion popup, captured while the cursor is
        // drawn (same row/col→pixel mapping). `(anchor_x, cursor_row_top)`.
        let mut popup_anchor: Option<(f32, f32)> = None;
        if let Some(_cur_visual_row) = self.active_tab().terminal.cursor_visual_row() {
            let cur = self.active_tab().terminal.cursor();
            let cur_col = cur.col.min(cols.saturating_sub(1));
            let target = (cur_col as f32, cur.row as f32);
            let anim_secs = self.config.cursor_anim_secs;
            let visible = self.cursor_currently_visible();

            // Capture ghosts before retargeting — once `anim.to` advances we
            // lose the previous-target column/row. Bounding-box scan is in
            // buffer coords; prev_visible uses visual rows, so translate via
            // `live_grid_offset` (consistent because key_matches implies
            // view_offset hasn't changed since the snapshot).
            // Collected into a local Vec, then appended after the `snap`
            // borrow below is dropped — pushing straight into the tab would
            // borrow it mutably while `snap` holds the same tab immutably.
            let mut new_ghosts: Vec<CursorGhost> = Vec::new();
            if anim_secs > 0.0 && key_matches {
                if let (Some(prev_anim), Some(snap)) = (
                    self.active_tab().cursor_anim.as_ref(),
                    self.active_tab().prev_visible.as_ref(),
                ) {
                    let (pcol, prow) = prev_anim.to;
                    let moved = (pcol - target.0).abs() > f32::EPSILON
                        || (prow - target.1).abs() > f32::EPSILON;
                    if moved {
                        let r0 = prow.min(target.1).floor().max(0.0) as usize;
                        let r1 = prow
                            .max(target.1)
                            .ceil()
                            .min((rows.saturating_sub(1)) as f32)
                            as usize;
                        let c0 = pcol.min(target.0).floor().max(0.0) as usize;
                        let c1 = pcol
                            .max(target.0)
                            .ceil()
                            .min((cols.saturating_sub(1)) as f32)
                            as usize;
                        let now = std::time::Instant::now();
                        for buf_r in r0..=r1 {
                            let vis_r = buf_r + live_grid_offset_i;
                            for c in c0..=c1 {
                                if vis_r >= snap.cells.len() || c >= snap.cells[vis_r].len() {
                                    continue;
                                }
                                let prev = snap.cells[vis_r][c];
                                if is_blank_cell(&prev) {
                                    continue;
                                }
                                let now_cell = self.active_tab().terminal.visible_cell(vis_r, c);
                                if !is_blank_cell(&now_cell) {
                                    continue;
                                }
                                new_ghosts.push(CursorGhost {
                                    ch: prev.ch,
                                    style: prev.style,
                                    buffer_row: buf_r,
                                    col: c,
                                    started_at: now,
                                });
                            }
                        }
                    }
                }
            }

            // Now that `snap`'s immutable borrow has ended, fold in the ghosts
            // captured above.
            self.tabs[self.active].cursor_ghosts.append(&mut new_ghosts);

            // Drop ghosts whose underlying cell got rewritten with new
            // content (e.g. user typed a replacement after the backspace),
            // or whose fade has run out.
            let now = std::time::Instant::now();
            {
                // Scoped so the `&mut tab` (which borrows `self.tabs`) is
                // released before the ghost-emit loop reads the active tab.
                // `cursor_ghosts` (mut) and `terminal` (read) split-borrow the
                // one tab.
                let tab = &mut self.tabs[self.active];
                let terminal = &tab.terminal;
                tab.cursor_ghosts.retain(|g| {
                    let elapsed = now.duration_since(g.started_at).as_secs_f32();
                    if anim_secs <= 0.0 || elapsed >= anim_secs {
                        return false;
                    }
                    let vis_r = g.buffer_row + live_grid_offset_i;
                    is_blank_cell(&terminal.visible_cell(vis_r, g.col))
                });
            }

            // Emit ghost glyphs as foreground-only quads with linearly
            // decaying alpha. Drawn before the cursor box so the cursor
            // visually consumes the ghost as it slides over.
            for ghost in &self.active_tab().cursor_ghosts {
                let elapsed = now.duration_since(ghost.started_at).as_secs_f32();
                let alpha = (1.0 - (elapsed / anim_secs).clamp(0.0, 1.0)).max(0.0);
                let mut fg = ghost.style.color_fg.unwrap_or(default_fg);
                // Premultiplied alpha to match the pipeline's blend mode.
                fg[0] *= alpha;
                fg[1] *= alpha;
                fg[2] *= alpha;
                fg[3] *= alpha;
                let variant =
                    font::FaceVariant::from_flags(ghost.style.bold, ghost.style.italic);
                let vis_r = ghost.buffer_row + live_grid_offset_i;
                emit_fg_for_cell(
                    &mut vertices,
                    &mut indices,
                    GlyphSource::Char(ghost.ch),
                    variant,
                    vis_r as isize,
                    ghost.col,
                    fg,
                );
            }

            let anim = self.tabs[self.active].cursor_anim.get_or_insert_with(|| CursorAnim::snapped(target));
            anim.retarget(target, anim_secs);

            if visible {
                let (eased_col, eased_buf_row) = anim.current(anim_secs);
                let eased_vis_row = eased_buf_row + live_grid_offset;
                let block_x = WINDOW_PADDING + eased_col * cell_w;
                // Cursor lives in the same per-row strip as the bg quad so
                // it aligns with selection / colored backgrounds.
                let cur_baseline =
                    WINDOW_PADDING + decorator_offset + (eased_vis_row + 1.0) * line_height;
                let block_y = cur_baseline - bg_h - descender - (line_height - bg_h) * 0.5
                    + row_scroll(eased_vis_row.round() as isize);
                // Anchor the completion popup to this cell's strip: left edge at
                // the cursor column, with `block_y` the top of the cursor row.
                popup_anchor = Some((block_x, block_y));
                let cursor_color = palette::get().cursor;
                // Underline / bar use a 2-px stripe; block fills the full cell.
                let stripe = 2.0_f32;
                let (cx, cy, cw, ch) = match self.active_tab().terminal.cursor_shape() {
                    terminal::CursorShape::Block => (block_x, block_y, cell_w, line_height),
                    terminal::CursorShape::Underline => {
                        (block_x, block_y + line_height - stripe, cell_w, stripe)
                    }
                    terminal::CursorShape::Bar => (block_x, block_y, stripe, line_height),
                };
                push_quad(
                    &mut vertices,
                    &mut indices,
                    cx,
                    cy,
                    cw,
                    ch,
                    [bg_u, bg_v],
                    [bg_u, bg_v],
                    cursor_color,
                    [0.0; 4],
                );
            }
        } else {
            // Cursor scrolled out of view. Drop the ease so the next time it
            // returns we snap to the new position instead of sliding in from
            // a stale one. Ghosts are tied to the cursor's motion so go with it.
            self.tabs[self.active].cursor_anim = None;
            self.tabs[self.active].cursor_ghosts.clear();
        }

        // Completion popup overlay (autocomplete slice K10). Drawn AFTER the
        // cursor/selection FG quads so it sits on top, and only when there are
        // cached suggestions, the cursor is on-screen (we have an anchor), and
        // the user hasn't scrolled into history (the cursor isn't where they're
        // looking then). Display-only: K11 adds keyboard nav + accept.
        //
        // TODO(K11+): consider excluding the popup from bloom. Appending into
        // the FG index range means it participates in the glow/bloom pass when
        // glow is on; acceptable for K10.
        let popup_visible = !self.active_tab().completions.is_empty() && self.active_tab().terminal.view_offset() == 0;
        if let Some((anchor_x, cursor_row_top)) = popup_anchor.filter(|_| popup_visible) {
            let len = self.active_tab().completions.len();
            // The visible window: `COMPLETION_MAX_VISIBLE` rows starting at the
            // scroll offset, clamped to the list. `n` is how many rows render.
            let start = self.active_tab().completion_scroll.min(len);
            let end = (start + COMPLETION_MAX_VISIBLE).min(len);
            let visible = &self.active_tab().completions[start..end];
            let n = visible.len();
            let item_h = line_height;
            // Box width: longest visible suggestion (chars) plus a little
            // horizontal padding, capped, so it's deterministic and testable.
            let longest = visible
                .iter()
                .map(|s| s.text.chars().count())
                .max()
                .unwrap_or(0);
            let text_pad = cell_w; // half a cell each side
            let box_w = (longest as f32 * cell_w + text_pad * 2.0).min(cell_w * 48.0);

            let anchor_below_y = cursor_row_top + line_height;
            let screen_w = self.surface.config.width as f32;
            let screen_h = self.surface.config.height as f32;
            let layout = completion::popup_layout(
                anchor_x,
                anchor_below_y,
                cursor_row_top,
                n,
                item_h,
                box_w,
                screen_w,
                screen_h,
                WINDOW_PADDING,
            );

            // Colors derived from the active palette so themes are respected.
            // The pipeline expects premultiplied alpha (RGB pre-scaled by A),
            // matching how selection/cursor colors are built above.
            let premul = |rgb: [f32; 4], a: f32| [rgb[0] * a, rgb[1] * a, rgb[2] * a, a];
            // Dark, semi-opaque box from a darkened background.
            let bg = pal.background;
            let box_rgb = [bg[0] * 0.6, bg[1] * 0.6, bg[2] * 0.6, 1.0];
            let box_color = premul(box_rgb, 0.92);
            // Highlight (selected row) — blend background toward foreground.
            let fgc = pal.foreground;
            let hl_rgb = [
                bg[0] * 0.5 + fgc[0] * 0.5,
                bg[1] * 0.5 + fgc[1] * 0.5,
                bg[2] * 0.5 + fgc[2] * 0.5,
                1.0,
            ];
            let hl_color = premul(hl_rgb, 0.85);
            let text_color = pal.foreground;
            let radius = 5.0_f32;

            // Box background (rounded corners, all four equal).
            push_quad(
                &mut vertices,
                &mut indices,
                layout.x,
                layout.y,
                layout.w,
                layout.h,
                [bg_u, bg_v],
                [bg_u, bg_v],
                box_color,
                [radius; 4],
            );

            // Highlight the selected row at its on-screen offset within the
            // visible window. Round the highlight's top corners only when it's
            // the box's first visible row, and its bottom corners only when it's
            // the last — so the highlight's rounding tracks the box's edges.
            let hl_row = self.active_tab().selected_completion.saturating_sub(start);
            if hl_row < n {
                let hl_y = layout.y + hl_row as f32 * item_h;
                let round_top = if hl_row == 0 { radius } else { 0.0 };
                let round_bot = if hl_row + 1 == n { radius } else { 0.0 };
                push_quad(
                    &mut vertices,
                    &mut indices,
                    layout.x,
                    hl_y,
                    layout.w,
                    item_h,
                    [bg_u, bg_v],
                    [bg_u, bg_v],
                    hl_color,
                    [round_top, round_bot, round_top, round_bot],
                );
            }

            // Suggestion text rows. Baseline within each row mirrors the grid:
            // strip_top + ascent (ascent above baseline = bg_h + descender,
            // descender being negative) + the centering pad.
            let text_x = layout.x + text_pad;
            let max_text_x = layout.x + layout.w - text_pad;
            for (i, sug) in visible.iter().enumerate() {
                let row_top = layout.y + i as f32 * item_h;
                let baseline = row_top + bg_h + descender + strip_pad;
                emit_text_run(
                    atlas,
                    &mut vertices,
                    &mut indices,
                    text_x,
                    baseline,
                    &sug.text,
                    text_color,
                    atlas_w,
                    atlas_h,
                    cell_w,
                    max_text_x,
                );
            }
        }

        // Command palette overlay (Cmd-Shift-P). Drawn last so it sits above
        // everything, anchored top-center rather than at the cursor. Reuses the
        // popup's quad/glyph helpers and palette-derived colors. Layout: a dim
        // backdrop, a rounded box, an input line with a caret, then (in command
        // mode) a separator and the filtered, scrollable command rows.
        if self.command_palette.open {
            use command_palette::{Mode, PALETTE_MAX_VISIBLE};
            let cp = &self.command_palette;
            let premul = |rgb: [f32; 3], a: f32| [rgb[0] * a, rgb[1] * a, rgb[2] * a, a];
            let screen_w = self.surface.config.width as f32;
            let screen_h = self.surface.config.height as f32;

            // Dim the terminal behind the palette to pull focus.
            push_quad(
                &mut vertices,
                &mut indices,
                0.0,
                0.0,
                screen_w,
                screen_h,
                [bg_u, bg_v],
                [bg_u, bg_v],
                premul([0.0, 0.0, 0.0], 0.45),
                [0.0; 4],
            );

            // Box geometry: a fixed-ish width centered horizontally, parked near
            // the top of the window.
            let box_w = (screen_w * 0.6).clamp(cell_w * 24.0, cell_w * 72.0).min(screen_w - WINDOW_PADDING * 2.0);
            let box_x = ((screen_w - box_w) * 0.5).round();
            let box_y = (screen_h * 0.12).round();
            let pad_v = (line_height * 0.45).round();
            let row_h = line_height;
            let sep_h = 1.0_f32;

            // Visible slice of the filtered list. Both command mode and the
            // choose-a-value mode (e.g. the theme picker) show a list; only
            // free-text argument mode hides it.
            let list_mode = cp.has_list();
            let start = cp.scroll.min(cp.filtered.len());
            let end = (start + PALETTE_MAX_VISIBLE).min(cp.filtered.len());
            let n = if list_mode { end - start } else { 0 };
            let has_list = n > 0;

            let total_h = pad_v * 2.0
                + row_h
                + if has_list { sep_h + n as f32 * row_h } else { 0.0 };

            // Colors, mirroring the completion popup so themes apply.
            let bg = pal.background;
            let box_color = premul([bg[0] * 0.55, bg[1] * 0.55, bg[2] * 0.55], 0.96);
            let fgc = pal.foreground;
            let hl_color = premul(
                [
                    bg[0] * 0.4 + fgc[0] * 0.6,
                    bg[1] * 0.4 + fgc[1] * 0.6,
                    bg[2] * 0.4 + fgc[2] * 0.6,
                ],
                0.9,
            );
            let text_color = pal.foreground;
            let caret_color = premul([fgc[0], fgc[1], fgc[2]], 0.9);
            let radius = 8.0_f32;

            // Box background.
            push_quad(
                &mut vertices,
                &mut indices,
                box_x,
                box_y,
                box_w,
                total_h,
                [bg_u, bg_v],
                [bg_u, bg_v],
                box_color,
                [radius; 4],
            );

            let text_x = box_x + cell_w;
            let max_text_x = box_x + box_w - cell_w;

            // Input line: a prompt prefix, then the typed text. In argument /
            // choose mode the prefix names what's being entered (e.g. "Title: ",
            // "Theme: ").
            let prefix = match cp.mode {
                Mode::Commands => "> ".to_string(),
                Mode::Argument { prompt, .. } | Mode::Choose { prompt, .. } => {
                    format!("{prompt}: ")
                }
            };
            let input_top = box_y + pad_v;
            let input_text = format!("{prefix}{}", cp.input.value);
            let baseline = input_top + bg_h + descender + strip_pad;
            emit_text_run(
                atlas,
                &mut vertices,
                &mut indices,
                text_x,
                baseline,
                &input_text,
                text_color,
                atlas_w,
                atlas_h,
                cell_w,
                max_text_x,
            );

            // Caret: a thin bar after the prefix + the chars left of the cursor.
            let caret_col = prefix.chars().count() + cp.input.cursor_col();
            let caret_x = text_x + caret_col as f32 * cell_w;
            if caret_x + 2.0 <= max_text_x {
                push_quad(
                    &mut vertices,
                    &mut indices,
                    caret_x,
                    input_top + strip_pad,
                    2.0,
                    bg_h,
                    [bg_u, bg_v],
                    [bg_u, bg_v],
                    caret_color,
                    [0.0; 4],
                );
            }

            if has_list {
                let list_top = input_top + row_h + sep_h;
                // Separator between the input and the results.
                push_quad(
                    &mut vertices,
                    &mut indices,
                    box_x,
                    input_top + row_h,
                    box_w,
                    sep_h,
                    [bg_u, bg_v],
                    [bg_u, bg_v],
                    premul([fgc[0], fgc[1], fgc[2]], 0.18),
                    [0.0; 4],
                );

                // Highlight the selected row within the visible window.
                let hl_row = cp.selected.saturating_sub(start);
                if hl_row < n {
                    push_quad(
                        &mut vertices,
                        &mut indices,
                        box_x,
                        list_top + hl_row as f32 * row_h,
                        box_w,
                        row_h,
                        [bg_u, bg_v],
                        [bg_u, bg_v],
                        hl_color,
                        [0.0; 4],
                    );
                }

                // Row labels: command titles in command mode, candidate values
                // (e.g. theme names) in choose mode — `row_label` hides which.
                for i in 0..n {
                    let Some(label) = cp.row_label(start + i) else {
                        continue;
                    };
                    let row_top = list_top + i as f32 * row_h;
                    let baseline = row_top + bg_h + descender + strip_pad;
                    emit_text_run(
                        atlas,
                        &mut vertices,
                        &mut indices,
                        text_x,
                        baseline,
                        label,
                        text_color,
                        atlas_w,
                        atlas_h,
                        cell_w,
                        max_text_x,
                    );
                }
            }
        }

        // Find-in-scrollback overlay (Cmd-F). Same centered, palette-styled box:
        // a dim backdrop, a rounded box, the "Find:" input line with a caret,
        // and — once there's a query — a separator and a result counter
        // ("3 / 17" or "No results"). Reuses the palette's quad/glyph helpers.
        if self.search.open {
            let premul = |rgb: [f32; 3], a: f32| [rgb[0] * a, rgb[1] * a, rgb[2] * a, a];
            let screen_w = self.surface.config.width as f32;
            let screen_h = self.surface.config.height as f32;

            // Dim the terminal behind the box.
            push_quad(
                &mut vertices,
                &mut indices,
                0.0,
                0.0,
                screen_w,
                screen_h,
                [bg_u, bg_v],
                [bg_u, bg_v],
                premul([0.0, 0.0, 0.0], 0.45),
                [0.0; 4],
            );

            let box_w = (screen_w * 0.6)
                .clamp(cell_w * 24.0, cell_w * 72.0)
                .min(screen_w - WINDOW_PADDING * 2.0);
            let box_x = ((screen_w - box_w) * 0.5).round();
            let box_y = (screen_h * 0.12).round();
            let pad_v = (line_height * 0.45).round();
            let row_h = line_height;
            let sep_h = 1.0_f32;

            let query = self.search.input.value.clone();
            let status = if query.is_empty() {
                String::new()
            } else if self.search.matches.is_empty() {
                "No results".to_string()
            } else {
                format!("{} / {}", self.search.current + 1, self.search.matches.len())
            };
            let has_status = !status.is_empty();

            let total_h = pad_v * 2.0 + row_h + if has_status { sep_h + row_h } else { 0.0 };

            let bg = pal.background;
            let box_color = premul([bg[0] * 0.55, bg[1] * 0.55, bg[2] * 0.55], 0.96);
            let fgc = pal.foreground;
            let text_color = pal.foreground;
            let caret_color = premul([fgc[0], fgc[1], fgc[2]], 0.9);
            let radius = 8.0_f32;

            push_quad(
                &mut vertices,
                &mut indices,
                box_x,
                box_y,
                box_w,
                total_h,
                [bg_u, bg_v],
                [bg_u, bg_v],
                box_color,
                [radius; 4],
            );

            let text_x = box_x + cell_w;
            let max_text_x = box_x + box_w - cell_w;

            // Input line: "Find: " prefix then the query.
            let prefix = "Find: ";
            let input_top = box_y + pad_v;
            let input_text = format!("{prefix}{query}");
            let baseline = input_top + bg_h + descender + strip_pad;
            emit_text_run(
                atlas,
                &mut vertices,
                &mut indices,
                text_x,
                baseline,
                &input_text,
                text_color,
                atlas_w,
                atlas_h,
                cell_w,
                max_text_x,
            );

            // Caret after the prefix + chars left of the cursor.
            let caret_col = prefix.chars().count() + self.search.input.cursor_col();
            let caret_x = text_x + caret_col as f32 * cell_w;
            if caret_x + 2.0 <= max_text_x {
                push_quad(
                    &mut vertices,
                    &mut indices,
                    caret_x,
                    input_top + strip_pad,
                    2.0,
                    bg_h,
                    [bg_u, bg_v],
                    [bg_u, bg_v],
                    caret_color,
                    [0.0; 4],
                );
            }

            if has_status {
                // Separator between the input and the counter.
                push_quad(
                    &mut vertices,
                    &mut indices,
                    box_x,
                    input_top + row_h,
                    box_w,
                    sep_h,
                    [bg_u, bg_v],
                    [bg_u, bg_v],
                    premul([fgc[0], fgc[1], fgc[2]], 0.18),
                    [0.0; 4],
                );
                let status_top = input_top + row_h + sep_h;
                let baseline = status_top + bg_h + descender + strip_pad;
                emit_text_run(
                    atlas,
                    &mut vertices,
                    &mut indices,
                    text_x,
                    baseline,
                    &status,
                    premul([fgc[0], fgc[1], fgc[2]], 0.7),
                    atlas_w,
                    atlas_h,
                    cell_w,
                    max_text_x,
                );
            }
        }

        // Refresh the visible-grid snapshot with the current frame's cells
        // so the next retarget can spot what just got cleared. Keyed by
        // viewport so a resize / scrollback / alt-screen flip flushes the
        // comparison in `key_matches` above.
        let mut snap_cells: Vec<Vec<style::Cell>> = Vec::with_capacity(rows);
        for r in 0..rows {
            let mut row_cells: Vec<style::Cell> = Vec::with_capacity(cols);
            for c in 0..cols {
                row_cells.push(self.active_tab().terminal.visible_cell(r, c));
            }
            snap_cells.push(row_cells);
        }
        self.tabs[self.active].prev_visible = Some(GridSnapshot {
            cells: snap_cells,
            key: viewport_key,
        });

        // 3. Edge fades: vertical gradient quads pinned to the top and bottom
        // of the window. The top one obscures content sliding up behind the
        // macOS traffic-light strip; the bottom one mirrors the effect so the
        // phantom row sliding into / out of the bottom edge dissolves rather
        // than clipping abruptly. Drawn last so they overlay every cell. RGB
        // is premultiplied with alpha to match PREMULTIPLIED_ALPHA_BLENDING.
        let win_w = self.surface.config.width as f32;
        let win_h = self.surface.config.height as f32;
        // Top fade is taller than the bottom: the title bar + toolbar takes
        // about DECORATOR_HEIGHT to fully occlude, and a longer gradient
        // below that gives content a soft runway as it scrolls into view
        // rather than popping out from a hard edge.
        let top_fade_height = self.config.top_fade_height;
        let bottom_fade_height_max = self.config.bottom_fade_height;
        let clear = [0.0, 0.0, 0.0, 0.0];

        // Strip quads live in their own vertex/index buffer — they're drawn
        // by the blur strip pipeline in the composite pass.
        let mut strip_vertices: Vec<renderer::vertex::Vertex> = Vec::with_capacity(16);
        let mut strip_indices: Vec<u16> = Vec::with_capacity(32);
        let push_strip = |vertices: &mut Vec<renderer::vertex::Vertex>,
                          indices: &mut Vec<u16>,
                          y0: f32,
                          y1: f32,
                          c0: [f32; 4],
                          c1: [f32; 4]| {
            let start = vertices.len() as u16;
            // radii = 0 so the shader skips the SDF mask; local_pos /
            // half_size go unused but we have to populate them.
            let stub = [0.0_f32, 0.0];
            vertices.push(renderer::vertex::Vertex {
                position: [0.0, y0, 0.0],
                tex_coords: [bg_u, bg_v],
                color: c0,
                local_pos: stub,
                half_size: stub,
                radii: [0.0; 4],
            });
            vertices.push(renderer::vertex::Vertex {
                position: [0.0, y1, 0.0],
                tex_coords: [bg_u, bg_v],
                color: c1,
                local_pos: stub,
                half_size: stub,
                radii: [0.0; 4],
            });
            vertices.push(renderer::vertex::Vertex {
                position: [win_w, y0, 0.0],
                tex_coords: [bg_u, bg_v],
                color: c0,
                local_pos: stub,
                half_size: stub,
                radii: [0.0; 4],
            });
            vertices.push(renderer::vertex::Vertex {
                position: [win_w, y1, 0.0],
                tex_coords: [bg_u, bg_v],
                color: c1,
                local_pos: stub,
                half_size: stub,
                radii: [0.0; 4],
            });
            indices.extend_from_slice(&[start, start + 1, start + 2, start + 1, start + 2, start + 3]);
        };

        // Edge fade animations: each phase ramps 0→1 the moment its
        // boundary distance leaves zero (and 1→0 when it returns) at a
        // constant rate, so the fade slides in fully in TOP_FADE_ANIM_SECS
        // regardless of scroll speed. The phase drives both the band height
        // (0 → full) and the alpha (0 → 1) together.
        let now = std::time::Instant::now();
        let dt = now.duration_since(self.last_anim_tick).as_secs_f32();
        self.last_anim_tick = now;
        let advance = |phase: &mut f32, target: f32, secs: f32| {
            let step = if secs > 0.0 { dt / secs } else { 1.0 };
            if *phase < target {
                *phase = (*phase + step).min(target);
            } else if *phase > target {
                *phase = (*phase - step).max(target);
            }
        };
        advance(
            &mut self.top_fade_phase,
            if dist_from_top > 0.0 { 1.0 } else { 0.0 },
            self.config.top_fade_anim_secs,
        );
        advance(
            &mut self.bottom_fade_phase,
            if dist_from_bottom > 0.0 { 1.0 } else { 0.0 },
            self.config.bottom_fade_anim_secs,
        );
        // top_band_height / top_alpha also feed the per-fragment glyph-fade
        // uniform below, so they're computed unconditionally. The strip quads
        // themselves are skipped at phase=0: emitting them would draw with
        // alpha 0 but still bump num_strip_indices, forcing render() through
        // the slow blur+composite path.
        let top_alpha = self.top_fade_phase;
        let top_band_height = top_fade_height * self.top_fade_phase;
        if self.top_fade_phase > 0.0 {
            let top_mid = top_band_height * self.config.top_fade_solid_stop.clamp(0.0, 1.0);
            let top_blur = [0.0_f32, 0.0, 0.0, top_alpha];
            push_strip(&mut strip_vertices, &mut strip_indices, 0.0, top_mid, top_blur, top_blur);
            push_strip(&mut strip_vertices, &mut strip_indices, top_mid, top_band_height, top_blur, clear);
        }

        if self.bottom_fade_phase > 0.0 {
            let bottom_alpha = self.bottom_fade_phase;
            let bottom_blur = [0.0_f32, 0.0, 0.0, bottom_alpha];
            let bottom_fade_height = bottom_fade_height_max * self.bottom_fade_phase;
            push_strip(
                &mut strip_vertices,
                &mut strip_indices,
                win_h - bottom_fade_height,
                win_h,
                clear,
                bottom_blur,
            );
        }

        self.shared.gpu
            .queue
            .write_buffer(&self.vertex_buffer, 0, bytemuck::cast_slice(&vertices));
        self.shared.gpu
            .queue
            .write_buffer(&self.index_buffer, 0, bytemuck::cast_slice(&indices));
        self.num_indices = indices.len() as u32;
        self.num_bg_indices = num_bg_indices;

        // Strip overlay: blur-only (tint = 0). The glyph fade already pulls
        // foreground text toward the bg color near each edge; the blur sits
        // on top to soften whatever's still visible in the gradient region.
        if !strip_indices.is_empty() {
            self.shared.gpu.queue.write_buffer(
                &self.strip_vertex_buffer,
                0,
                bytemuck::cast_slice(&strip_vertices),
            );
            self.shared.gpu.queue.write_buffer(
                &self.strip_index_buffer,
                0,
                bytemuck::cast_slice(&strip_indices),
            );
        }
        self.num_strip_indices = strip_indices.len() as u32;

        // Bottom edge keeps just the blur strip — zero band_height here
        // disables the per-fragment glyph/bg fade so cells stay solid right
        // up to the window's bottom edge.
        let fade_data: [f32; 16] = [
            top_band_height, top_alpha, 0.0, 0.0,
            0.0, 0.0, 0.0, 0.0,
            win_w, win_h, 0.0, 0.0,
            bg_u, bg_v, 0.0, 0.0,
        ];
        self.shared.gpu.queue.write_buffer(
            &self.fade_buffer,
            0,
            bytemuck::cast_slice(&fade_data),
        );
    }

    /// Drive the command palette from a key press while it's open. Always
    /// consumes the event (returns `true`): the palette owns the keyboard, so
    /// nothing here reaches the PTY. Cmd-Shift-P (open/close) is handled by the
    /// caller before this; everything else — navigation, text editing, accept,
    /// dismiss — is handled here.
    fn command_palette_key(&mut self, event: &winit::event::KeyEvent) -> bool {
        use command_palette::Outcome;
        use winit::keyboard::{Key, NamedKey};
        match &event.logical_key {
            Key::Named(NamedKey::Escape) => {
                if self.command_palette.escape() == Outcome::Close {
                    self.command_palette.close();
                }
            }
            Key::Named(NamedKey::Enter) => match self.command_palette.accept() {
                Outcome::Run { action, arg } => {
                    self.command_palette.close();
                    self.run_palette_action(action, arg);
                }
                Outcome::RequestChoices { action, prompt } => {
                    // Only the host can enumerate the candidates; feed them back
                    // so the palette can present a filtered picker.
                    let choices = self.palette_choices(action);
                    self.command_palette.enter_choose(action, prompt, choices);
                }
                Outcome::Close => self.command_palette.close(),
                Outcome::Stay => {}
            },
            Key::Named(NamedKey::ArrowDown) => self.command_palette.move_down(),
            Key::Named(NamedKey::ArrowUp) => self.command_palette.move_up(),
            Key::Named(NamedKey::Backspace) => self.command_palette.backspace(),
            Key::Named(NamedKey::Delete) => self.command_palette.input.delete(),
            Key::Named(NamedKey::ArrowLeft) => self.command_palette.input.left(),
            Key::Named(NamedKey::ArrowRight) => self.command_palette.input.right(),
            Key::Named(NamedKey::Home) => self.command_palette.input.home(),
            Key::Named(NamedKey::End) => self.command_palette.input.end(),
            // Ctrl-N / Ctrl-P mirror ArrowDown / ArrowUp so the selection can
            // be moved without leaving the home row.
            Key::Character(s)
                if self.modifiers.control_key()
                    && !self.modifiers.super_key()
                    && !self.modifiers.alt_key()
                    && s.eq_ignore_ascii_case("n") =>
            {
                self.command_palette.move_down()
            }
            Key::Character(s)
                if self.modifiers.control_key()
                    && !self.modifiers.super_key()
                    && !self.modifiers.alt_key()
                    && s.eq_ignore_ascii_case("p") =>
            {
                self.command_palette.move_up()
            }
            _ => {
                // Printable text: insert it, unless a Cmd/Ctrl/Alt chord is
                // held (those aren't text input). winit hands us the composed
                // text in `event.text`.
                let plain = !self.modifiers.super_key()
                    && !self.modifiers.control_key()
                    && !self.modifiers.alt_key();
                if plain {
                    if let Some(text) = &event.text {
                        for c in text.chars() {
                            if !c.is_control() {
                                self.command_palette.type_char(c);
                            }
                        }
                    }
                }
            }
        }
        self.invalidate();
        true
    }

    /// Enumerate the candidate list for a pick-from-list palette command (one
    /// with `choose: true`), answering [`Outcome::RequestChoices`]. Kept on the
    /// host because the options come from outside the pure palette module — for
    /// `SetTheme`, the schemes on disk.
    fn palette_choices(&self, action: command_palette::PaletteAction) -> Vec<String> {
        use command_palette::PaletteAction as A;
        match action {
            // All three theme pickers offer the same list: the on-disk schemes
            // plus the synthetic "Default (built-in)" entry.
            A::SetTheme | A::SetLightTheme | A::SetDarkTheme => {
                theme_picker_choices(list_scheme_names())
            }
            _ => Vec::new(),
        }
    }

    /// Drive the find overlay from a key press while it's open. Always consumes
    /// the event (returns `true`): like the palette, the overlay owns the
    /// keyboard so nothing reaches the PTY. Cmd-F (open/close) is handled by the
    /// caller before this. Enter / Down steps to the next match, Shift-Enter /
    /// Up to the previous; editing the query re-runs the search live.
    fn search_key(&mut self, event: &winit::event::KeyEvent) -> bool {
        use winit::keyboard::{Key, NamedKey};
        match &event.logical_key {
            Key::Named(NamedKey::Escape) => {
                // Close but leave the viewport where it is, so the match the
                // user found stays in view.
                self.search.close();
            }
            Key::Named(NamedKey::Enter) => {
                if self.modifiers.shift_key() {
                    self.search.prev();
                } else {
                    self.search.next();
                }
                self.focus_current_match();
            }
            Key::Named(NamedKey::ArrowDown) => {
                self.search.next();
                self.focus_current_match();
            }
            Key::Named(NamedKey::ArrowUp) => {
                self.search.prev();
                self.focus_current_match();
            }
            Key::Named(NamedKey::Backspace) => {
                self.search.backspace();
                self.run_search();
            }
            Key::Named(NamedKey::Delete) => {
                self.search.input.delete();
                self.run_search();
            }
            Key::Named(NamedKey::ArrowLeft) => self.search.input.left(),
            Key::Named(NamedKey::ArrowRight) => self.search.input.right(),
            Key::Named(NamedKey::Home) => self.search.input.home(),
            Key::Named(NamedKey::End) => self.search.input.end(),
            _ => {
                // Printable text drives the query, unless a Cmd/Ctrl/Alt chord
                // is held (those aren't text input). winit hands us composed
                // text in `event.text`.
                let plain = !self.modifiers.super_key()
                    && !self.modifiers.control_key()
                    && !self.modifiers.alt_key();
                if plain {
                    if let Some(text) = &event.text {
                        for c in text.chars() {
                            if !c.is_control() {
                                self.search.type_char(c);
                            }
                        }
                    }
                    self.run_search();
                }
            }
        }
        self.invalidate();
        true
    }

    /// Re-scan the whole buffer (scrollback + live grid) for the current query
    /// and refresh the match list, then jump to the current match. An empty
    /// query clears the matches. Called on every query edit.
    fn run_search(&mut self) {
        let query = self.search.input.value.clone();
        if query.is_empty() {
            self.search.set_matches(Vec::new());
            self.invalidate();
            return;
        }
        let case_sensitive = search::smart_case(&query);
        let total = self.active_tab().terminal.scrollback_len() + self.active_tab().terminal.rows;
        let mut matches = Vec::new();
        for abs in 0..total as isize {
            let Some(cells) = self.active_tab().terminal.line_at(abs) else {
                continue;
            };
            // One char per column. Drop trailing blanks so padding spaces don't
            // bloat the scan; leading offsets stay intact so columns line up.
            let last = cells
                .iter()
                .rposition(|c| c.ch != ' ')
                .map(|i| i + 1)
                .unwrap_or(0);
            if last == 0 {
                continue;
            }
            let line: String = cells[..last].iter().map(|c| c.ch).collect();
            for (start, end) in search::match_line(&line, &query, case_sensitive) {
                matches.push(search::Match {
                    line: abs,
                    start_col: start,
                    end_col: end,
                });
            }
        }
        self.search.set_matches(matches);
        self.focus_current_match();
    }

    /// Scroll the viewport so the current match is visible, then redraw.
    fn focus_current_match(&mut self) {
        if let Some(m) = self.search.current_match() {
            self.active_tab_mut().terminal.scroll_line_into_view(m.line);
            self.active_tab_mut().scroll_y = 0.0;
        }
        self.invalidate();
    }

    /// This window's top-left in logical points, for handing to a child window
    /// to cascade off (see [`spawn_new_window`]). `None` if the platform can't
    /// report the position — the child then keeps the OS default spot.
    fn window_origin(&self) -> Option<(f64, f64)> {
        let phys = self.window.outer_position().ok()?;
        let logical: winit::dpi::LogicalPosition<f64> =
            phys.to_logical(self.window.scale_factor());
        Some((logical.x, logical.y))
    }

    /// Execute a command chosen in the palette. Every arm reuses behaviour that
    /// already exists elsewhere — the palette is a discoverable front end, not
    /// new functionality.
    fn run_palette_action(
        &mut self,
        action: command_palette::PaletteAction,
        arg: Option<String>,
    ) {
        use command_palette::PaletteAction as A;
        match action {
            // Route through the OSC-0/2 path so the existing title plumbing
            // (take_title_update -> effective_title) applies uniformly; the
            // event loop picks the change up after this returns.
            A::SetTitle => self.active_tab_mut()
                .terminal
                .set_window_title(arg.as_deref().unwrap_or("")),
            A::ClearTitle => self.active_tab_mut().terminal.set_window_title(""),
            A::ReloadConfig => self.reload_config(),
            // The picker hands back a label; map it to a scheme slot value
            // ("Default (built-in)" / empty -> None, revert to built-in).
            // "Set theme" is contextual: while following the system it assigns
            // the slot for the current appearance, otherwise the single
            // color_scheme.
            A::SetTheme => {
                let val = scheme_value_from_pick(arg);
                if self.config.auto_theme {
                    if self.system_is_dark() {
                        self.config.dark_scheme = val;
                    } else {
                        self.config.light_scheme = val;
                    }
                } else {
                    self.config.color_scheme = val;
                }
                self.persist_and_apply();
            }
            A::SetLightTheme => {
                self.config.light_scheme = scheme_value_from_pick(arg);
                self.persist_and_apply();
            }
            A::SetDarkTheme => {
                self.config.dark_scheme = scheme_value_from_pick(arg);
                self.persist_and_apply();
            }
            A::ToggleFollowSystem => {
                self.config.auto_theme = !self.config.auto_theme;
                self.persist_and_apply();
            }
            A::ZoomIn => self.change_font_size(1.0),
            A::ZoomOut => self.change_font_size(-1.0),
            A::ToggleWireframe => {
                if self.shared.wireframe_pipeline.is_some() {
                    self.wireframe = !self.wireframe;
                }
            }
            A::CopyLastOutput => {
                self.select_last_command_output();
            }
            A::NewWindow => spawn_new_window(self.active_tab().terminal.cwd(), self.window_origin()),
            // Re-arm first-run and relaunch into it. Each window owns a single
            // PTY (forked at startup, already attached to a shell), so there's
            // no in-place way to swap the running shell for onboarding —
            // instead we lower the marker and start a fresh Yutani, which boots
            // straight into onboarding-in-PTY with live preview, then exit this
            // one. This ends the current shell session, which is why the
            // command is spelled out plainly in the palette.
            A::RunOnboarding => {
                rearm_onboarding();
                let relaunched = std::env::current_exe()
                    .and_then(|exe| std::process::Command::new(exe).spawn());
                match relaunched {
                    Ok(_) => std::process::exit(0),
                    // Couldn't relaunch — leave this session alone. The marker
                    // is already re-armed, so the next manual launch onboards.
                    Err(e) => eprintln!("onboarding: failed to relaunch: {e}"),
                }
            }
        }
        self.invalidate();
    }

    /// Bump (or shrink) the font by `delta_pt` points and persist the new size.
    /// Thin wrapper over [`set_font_size`]; the zoom command saves, the
    /// onboarding live-preview path calls `set_font_size` directly so it
    /// doesn't touch disk.
    fn change_font_size(&mut self, delta_pt: f32) {
        if self.set_font_size(self.pt_size + delta_pt) {
            self.config.save();
        }
    }

    /// Set the font to an absolute point size and rebuild everything that
    /// depends on cell metrics: atlas, font texture, bind group, terminal grid,
    /// vertex/index buffers. Clamped to [6, 96] so the rasterizer never gets a
    /// nonsensical size. Returns whether the size actually changed. Does NOT
    /// persist config — callers that should (the zoom command) save themselves.
    fn set_font_size(&mut self, pt: f32) -> bool {
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

    pub fn resize(&mut self, size: winit::dpi::PhysicalSize<u32>) {
        // A move to a display with a different scale factor changes the native
        // title bar's physical height; keep the chrome band in step.
        self.refresh_chrome_band();
        self.surface.resize(&self.shared.gpu.device, size);
        if size.width > 0 && size.height > 0 {
            self.blur.resize(
                &self.shared.gpu.device,
                &self.shared.gpu.queue,
                &self.shared.blur_pipelines,
                size.width,
                size.height,
            );
            // FG scene mirrors the BG scene's size/format. Recreate the
            // texture and rebuild every bind group that samples it.
            self.scene_fg = SceneTarget::new(
                &self.shared.gpu.device,
                self.surface.config.format,
                size.width,
                size.height,
                "scene fg",
            );
            self.scene_fg_blit_bg = self.shared.blur_pipelines.make_blit_bind_group(
                &self.shared.gpu.device,
                &self.scene_fg.view,
                "scene fg blit bg",
            );
            // Each Glow's bright-pass bind group is bound to a specific
            // scene view — resize rebuilds it against the (potentially
            // recreated) texture handle.
            self.glow.resize(
                &self.shared.gpu.device,
                &self.shared.gpu.queue,
                &self.shared.glow_pipelines,
                size.width,
                size.height,
                &self.blur.scene.view,
            );
            self.glow_fg.resize(
                &self.shared.gpu.device,
                &self.shared.gpu.queue,
                &self.shared.glow_pipelines,
                size.width,
                size.height,
                &self.scene_fg.view,
            );
            // Mask bind groups sample the (just-recreated) bg scene
            // texture, so they have to be rebuilt against the new view.
            self.glow_bg_mask = self.shared.glow_pipelines.make_mask_bind_group(
                &self.shared.gpu.device,
                &self.blur.scene.view,
                "glow bg mask (bg scene)",
            );
            self.glow_fg_mask = self.shared.glow_pipelines.make_mask_bind_group(
                &self.shared.gpu.device,
                &self.blur.scene.view,
                "glow fg mask (bg scene)",
            );
            self.scanline_overlay_mask = self.shared.glow_pipelines.make_overlay_mask_bind_group(
                &self.shared.gpu.device,
                &self.blur.scene.view,
                &self.scene_fg.view,
                "scanline overlay mask (bg + fg)",
            );
        }
        self.camera_uniform
            .update_view_proj(&self.camera, size.width as f32, size.height as f32);
        self.shared.gpu.queue.write_buffer(
            &self.camera_buffer,
            0,
            bytemuck::cast_slice(&[self.camera_uniform]),
        );
        let metrics = self.shared.with_font(|f| f.face().size_metrics().unwrap());
        let size = WindowState::get_viewport_size(
            self.surface.config.width as f32,
            self.surface.config.height as f32,
            self.shared.with_font(|f| f.cell_width()),
            ((metrics.ascender - metrics.descender) >> 6) as usize,
        );
        // Only touch the PTY winsize when the character grid actually
        // changes. macOS raises SIGWINCH on any TIOCSWINSZ whose winsize
        // differs from the old one (a full bcmp), and ws_xpixel/ws_ypixel
        // shift on every pixel of a live drag. Notifying on pixel-only
        // changes floods the foreground process with SIGWINCH; shells that
        // repaint their prompt on WINCH (powerlevel10k &c.) then stack a
        // fresh prompt per frame, so growing the window appears to push the
        // prompt downward. Gating on rows/cols collapses a drag back to one
        // signal per row boundary crossed.
        let grid_changed = size.char_width != self.active_tab().terminal.cols
            || size.char_height != self.active_tab().terminal.rows;
        self.active_tab_mut().terminal.resize(size.char_width, size.char_height);
        if grid_changed {
            self.notify_pty_size(size.char_width, size.char_height);
        }
        self.resize_buffers();
        self.active_tab_mut().cursor_anim = None;
        self.invalidate();
    }

    fn notify_pty_size(&self, cols: usize, rows: usize) {
        // Pixel dimensions are what `kitty +kitten icat` (and any other
        // image-protocol-aware tool that reads `TIOCGWINSZ`) uses to
        // discover the cell-pixel size. Zero here would make those
        // tools refuse to send images with "Terminal does not support
        // reporting screen sizes in pixels."
        let metrics = self.shared.with_font(|f| f.face().size_metrics().unwrap());
        let cell_w = self.shared.with_font(|f| f.cell_width()) as u32;
        let line_h = ((metrics.ascender - metrics.descender) >> 6) as u32;
        let xpixel = (cols as u32).saturating_mul(cell_w).min(u16::MAX as u32) as u16;
        let ypixel = (rows as u32).saturating_mul(line_h).min(u16::MAX as u32) as u16;
        let ws = libc::winsize {
            ws_row: rows as u16,
            ws_col: cols as u16,
            ws_xpixel: xpixel,
            ws_ypixel: ypixel,
        };
        unsafe {
            libc::ioctl(self.active_tab().master, libc::TIOCSWINSZ, &ws);
        }
    }

    fn write_pty(&self, bytes: &[u8]) {
        if let Err(e) = nix::unistd::write(self.active_tab().master, bytes) {
            eprintln!("pty write failed: {e}");
        }
    }

    /// Effective blink state: DECSCUSR's request is gated by the user's
    /// `cursor_blink` config so opting out disables blinking globally.
    fn cursor_blink_enabled(&self) -> bool {
        self.config.cursor_blink && self.active_tab().terminal.cursor_blink()
    }

    /// Combined visibility check: DECTCEM (cursor_visible) gates whether the
    /// cursor exists at all; blink only suppresses it on the "off" half-phase
    /// of the cycle when DECSCUSR has selected a blinking variant.
    fn cursor_currently_visible(&self) -> bool {
        self.active_tab().terminal.cursor_visible() && (!self.cursor_blink_enabled() || self.blink_on)
    }

    /// If a blink half-cycle has elapsed, flip the phase and request a redraw.
    /// Returns true when the cursor visibility actually changed.
    fn maybe_blink_tick(&mut self) -> bool {
        if !self.cursor_blink_enabled() || !self.active_tab().terminal.cursor_visible() {
            return false;
        }
        if self.last_blink.elapsed() < BLINK_INTERVAL {
            return false;
        }
        self.blink_on = !self.blink_on;
        self.last_blink = std::time::Instant::now();
        true
    }

    /// Next instant the event loop should wake to flip the blink phase, or
    /// `None` if the cursor isn't blinking right now.
    fn next_blink_wake(&self) -> Option<std::time::Instant> {
        if self.cursor_blink_enabled() && self.active_tab().terminal.cursor_visible() {
            Some(self.last_blink + BLINK_INTERVAL)
        } else {
            None
        }
    }

    /// Snap the cursor to its visible phase and reset the blink timer.
    /// Called on user input so the cursor doesn't wink off mid-keystroke.
    fn reset_blink(&mut self) {
        self.blink_on = true;
        self.last_blink = std::time::Instant::now();
    }

    /// True while the cursor quad is mid-ease, or any ghost glyphs are
    /// still fading. Keeps the event loop ticking until both finish so
    /// the redraw isn't held up waiting for the next PTY/blink event.
    fn is_cursor_animating(&self) -> bool {
        let anim_active = match &self.active_tab().cursor_anim {
            Some(a) => a.animating(self.config.cursor_anim_secs),
            None => false,
        };
        anim_active || !self.active_tab().cursor_ghosts.is_empty()
    }

    /// Edge-fade distances `(bottom, top)` that drive the fade phases and the
    /// title-bar decorator offset. On the primary screen they track the
    /// scrollback viewport (sub-line `scroll_y` included). On the alt screen
    /// there's no scrollback, so they're zero — except during an upward scroll
    /// slide, where the top fade is engaged so the departing rows dissolve into
    /// the translucent toolbar instead of popping out when the slide ends.
    fn edge_fade_dists(
        &self,
        scroll_y: f32,
        view_offset: f32,
        scrollback_len: f32,
        line_height: f32,
    ) -> (f32, f32) {
        if self.active_tab().terminal.on_alt_screen() {
            (0.0, 0.0)
        } else {
            (
                view_offset * line_height + scroll_y,
                (scrollback_len - view_offset) * line_height - scroll_y,
            )
        }
    }

    /// True while either edge-fade phase is still chasing its target —
    /// used to keep the event loop ticking until the slide completes.
    fn is_top_fade_animating(&self) -> bool {
        let scrollback_len = if self.active_tab().terminal.on_alt_screen() {
            0.0
        } else {
            self.active_tab().terminal.scrollback_len() as f32
        };
        let view_offset = self.active_tab().terminal.view_offset() as f32;
        let metrics = self.shared.with_font(|f| f.face().size_metrics().unwrap());
        let line_height = ((metrics.ascender - metrics.descender) >> 6) as f32;
        let scroll_y = self.active_tab().scroll_y as f32;
        let (dist_from_bottom, dist_from_top) =
            self.edge_fade_dists(scroll_y, view_offset, scrollback_len, line_height);
        let top_target = if dist_from_top > 0.0 { 1.0 } else { 0.0 };
        let bot_target = if dist_from_bottom > 0.0 { 1.0 } else { 0.0 };
        (self.top_fade_phase - top_target).abs() > f32::EPSILON
            || (self.bottom_fade_phase - bot_target).abs() > f32::EPSILON
    }

    /// Write the current in-memory config to disk and re-apply the scheme that
    /// now matches it (and the system appearance). Used by the palette's theme
    /// commands after they mutate a scheme slot or the follow-system flag, so
    /// the change both persists and takes effect live.
    fn persist_and_apply(&mut self) {
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
    fn reload_config(&mut self) {
        self.config = Config::load();
        self.apply_active_scheme();
    }

    /// True when the OS is currently in dark mode, per winit's tracked window
    /// theme (updated from `WindowEvent::ThemeChanged`). Defaults to light if
    /// the platform doesn't report one.
    fn system_is_dark(&self) -> bool {
        self.window.theme() == Some(winit::window::Theme::Dark)
    }

    /// Install the color scheme that matches the current config and system
    /// appearance (see [`Config::active_scheme`]), then push every
    /// palette-derived value into the GPU and window chrome. Shared by config
    /// reloads, the palette's theme commands, and live system-appearance
    /// changes — anything that can change which scheme should be showing.
    fn apply_active_scheme(&mut self) {
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
    fn refresh_palette_derived(&mut self) {
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
    fn apply_preview(&mut self, req: terminal::PreviewRequest) {
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
    fn sync_theme_colors(&mut self) {
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

    /// 1-based (col, row) form of `pixel_to_visual_cell` for mouse reporting.
    fn pixel_to_cell(&self, px: f64, py: f64) -> (u16, u16) {
        let (c, r) = self.pixel_to_visual_cell(px, py);
        (c as u16 + 1, r as u16 + 1)
    }

    /// True when a window-relative `py` (physical px) falls inside the title
    /// bar / toolbar chrome band at the top of the window. The app draws with
    /// `fullsize_content_view` so terminal content renders behind the
    /// translucent macOS title bar. Pointer events landing in this band are the
    /// user driving the window chrome — dragging the bar, hitting the traffic
    /// lights — and must be swallowed rather than translated into mouse reports
    /// for the shell below, and the cursor must be the arrow rather than the
    /// grid's I-beam. The band tracks the live native title-bar height (see
    /// `chrome_band_px` / `refresh_chrome_band`), not the scroll-animated
    /// decorator offset, because the native title bar doesn't move with scroll.
    fn in_top_toolbar(&self, py: f64) -> bool {
        py_in_top_toolbar(py, self.chrome_band_px)
    }

    /// Recompute `chrome_band_px` from the live native title-bar height.
    ///
    /// The renderer's `WINDOW_PADDING + DECORATOR_HEIGHT` reserve is fixed in
    /// physical px, but the native title bar is a fixed number of *points*, so
    /// on a Retina display it's physically taller than the reserve. Sizing the
    /// band to the reserve left it shorter than the bar, and since macOS
    /// swallows pointer-moved events over the bar, the band's logic never ran
    /// up there — the grid's I-beam stayed frozen over the title bar. Track the
    /// real bar height instead (plus `CHROME_BAND_MARGIN_PX`, so the lowest grid
    /// move we still receive lands inside the band and flips the cursor to the
    /// arrow before the events cut out). Falls back to the reserve when the
    /// query fails or yields something shorter than the reserve (e.g. low-DPI,
    /// where the reserve already comfortably covers the bar).
    fn refresh_chrome_band(&mut self) {
        let reserve = (WINDOW_PADDING + DECORATOR_HEIGHT) as f64;
        self.chrome_band_px =
            chrome_band_from(native_titlebar_height_physical(&self.window), reserve);
    }

    /// Forward a mouse event to the PTY in the host's preferred encoding,
    /// if any tracking mode is enabled. `motion` is set for drag/move events.
    fn report_mouse(&mut self, button: input::MouseButton, press: bool, motion: bool) {
        let mp = self.active_tab().terminal.mouse_protocol();
        if !mp.enabled() {
            return;
        }
        if motion && !mp.button_motion && !mp.any_motion {
            return;
        }
        if motion && mp.button_motion && !mp.any_motion && self.held_button.is_none() {
            return;
        }
        let (col, row) = self.pixel_to_cell(self.mouse_x, self.mouse_y);
        if motion {
            // Coalesce: only report when the cell changes.
            if self.active_tab().last_reported_cell == Some((col, row)) {
                return;
            }
            self.active_tab_mut().last_reported_cell = Some((col, row));
        }
        let bytes = input::encode_mouse(button, col, row, press, motion, mp.sgr, self.modifiers);
        self.write_pty(&bytes);
    }

    /// Cell under a window-pixel coord, in 0-based (col, visual_row) form,
    /// clamped to the grid. Used to anchor and update text selection.
    ///
    /// The renderer puts each row's baseline at `top_offset + (r+1)*lh`, so
    /// row 0's drawn box starts at `top_offset + lh - ascender` rather than
    /// at `top_offset`. We align the hit-test strip with that drawn box;
    /// any in-progress smooth-scroll offset is folded in too so the mapping
    /// stays consistent during sub-line slides.
    fn pixel_to_visual_cell(&self, px: f64, py: f64) -> (usize, isize) {
        let metrics = self.shared.with_font(|f| f.face().size_metrics().unwrap());
        let line_height = ((metrics.ascender - metrics.descender) >> 6) as f64;
        let ascender = (metrics.ascender >> 6) as f64;
        let descender = (metrics.descender >> 6) as f64;
        let bg_h = ascender - descender;
        let cell_w = self.shared.with_font(|f| f.cell_width()) as f64;
        // Mirror the renderer's dynamic decorator offset: full DECORATOR_HEIGHT
        // at both scroll-range boundaries (live grid and top of scrollback),
        // easing to 0 over one line in either direction. Out-of-sync formulas
        // here would drift the hit-test by a row vs. what's actually drawn.
        let view_offset = self.active_tab().terminal.view_offset() as f64;
        let scrollback_len = if self.active_tab().terminal.on_alt_screen() {
            0.0
        } else {
            self.active_tab().terminal.scrollback_len() as f64
        };
        let dist_from_bottom = view_offset * line_height + self.active_tab().scroll_y;
        let dist_from_top = (scrollback_len - view_offset) * line_height - self.active_tab().scroll_y;
        let near = (dist_from_bottom / line_height)
            .min(dist_from_top / line_height)
            .clamp(0.0, 1.0);
        let chrome_offset = DECORATOR_HEIGHT as f64 * (1.0 - near);
        // Strip top = renderer's `baseline - ascender - (lh - bg_h)/2`
        // for row 0, where baseline_0 = WP + chrome + line_height.
        let strip_pad = (line_height - bg_h) * 0.5;
        let row_strip_top =
            WINDOW_PADDING as f64 + chrome_offset + line_height - ascender - strip_pad;
        let col = ((px - WINDOW_PADDING as f64) / cell_w).floor() as i64;
        let row = ((py - row_strip_top - self.active_tab().scroll_y) / line_height).floor() as i64;
        let col = col.clamp(0, self.active_tab().terminal.cols as i64 - 1) as usize;
        let row = row.clamp(0, self.active_tab().terminal.rows as i64 - 1) as isize;
        (col, row)
    }

    /// Pixel coord → absolute (line, col) selection point.
    fn pixel_to_selection_point(&self, px: f64, py: f64) -> (isize, usize) {
        let (col, vrow) = self.pixel_to_visual_cell(px, py);
        (self.active_tab().terminal.visual_to_abs_line(vrow), col)
    }

    /// Recompute the URL under the mouse pointer. Tracks Cmd state so the
    /// underline overlay and pointer cursor only appear while the user is
    /// actually holding the modifier; releasing Cmd clears the hover. Any
    /// state change here flips the system cursor icon and invalidates the
    /// frame so the underline can repaint.
    fn update_hover_url(&mut self) {
        // The pointer is over the title bar / toolbar band — window chrome,
        // not the grid. This method also runs on events that don't move the
        // mouse (PTY output, Cmd press/release, scroll, prompt jumps), and
        // must not flip the chrome's arrow back to the grid's I-beam (or to a
        // Pointer from a URL on the clamped row-0 hit-test) while it sits there.
        // CursorMoved owns the cursor in this band and clears any hovered URL.
        if self.in_top_toolbar(self.mouse_y) {
            return;
        }
        let new = if self.modifiers.super_key() {
            let (col, vrow) = self.pixel_to_visual_cell(self.mouse_x, self.mouse_y);
            let abs_line = self.active_tab().terminal.visual_to_abs_line(vrow);
            find_url_at(&self.active_tab().terminal, abs_line, col)
        } else {
            None
        };
        if new == self.active_tab().hover_url {
            return;
        }
        let icon = if new.is_some() {
            winit::window::CursorIcon::Pointer
        } else {
            winit::window::CursorIcon::Text
        };
        self.window.set_cursor_icon(icon);
        self.active_tab_mut().hover_url = new;
        self.invalidate();
    }

    /// Anchor a new selection at the mouse position. Click count cycles
    /// 1 → 2 → 3 → 1 for click sequences within the threshold on the same
    /// cell, picking Cell / Word / Line granularity respectively.
    fn handle_mouse_press(&mut self) {
        let p = self.pixel_to_selection_point(self.mouse_x, self.mouse_y);
        let now = std::time::Instant::now();
        let continued = self.active_tab()
            .last_click
            .map(|(t, c)| c == p && now.duration_since(t) < DOUBLE_CLICK_THRESHOLD)
            .unwrap_or(false);
        self.active_tab_mut().click_count = if continued { (self.active_tab().click_count % 3) + 1 } else { 1 };
        self.active_tab_mut().last_click = Some((now, p));
        self.active_tab_mut().selection_mode = match self.active_tab().click_count {
            1 => SelectionMode::Cell,
            2 => SelectionMode::Word,
            _ => SelectionMode::Line,
        };
        self.active_tab_mut().press_cell = Some(p);
        self.active_tab_mut().press_pixel = Some((self.mouse_x, self.mouse_y));
        // Word and Line modes show their selection on click. Cell mode waits
        // until the drag exceeds DRAG_THRESHOLD_PX so a plain click doesn't
        // briefly highlight a single character.
        self.active_tab_mut().selection = match self.active_tab().selection_mode {
            SelectionMode::Cell => None,
            _ => self.compute_selection(p, p),
        };
    }

    /// Update the head of the active selection from the current mouse pos.
    fn handle_mouse_drag(&mut self) {
        let Some(p0) = self.active_tab().press_cell else { return };
        if self.active_tab().selection_mode == SelectionMode::Cell && self.active_tab().selection.is_none() {
            let Some((px, py)) = self.active_tab().press_pixel else { return };
            let dx = self.mouse_x - px;
            let dy = self.mouse_y - py;
            if dx * dx + dy * dy < DRAG_THRESHOLD_PX * DRAG_THRESHOLD_PX {
                return;
            }
        }
        let p = self.pixel_to_selection_point(self.mouse_x, self.mouse_y);
        self.active_tab_mut().selection = self.compute_selection(p0, p);
    }

    fn handle_mouse_release(&mut self) {
        self.active_tab_mut().press_cell = None;
        self.active_tab_mut().press_pixel = None;
    }

    /// Build a selection from two cells under the current `selection_mode`.
    /// In Word / Line mode, each end snaps outward to the word or line edge.
    fn compute_selection(&self, a: (isize, usize), b: (isize, usize)) -> Option<Selection> {
        let (start, end) = if a <= b { (a, b) } else { (b, a) };
        let (start, end) = match self.active_tab().selection_mode {
            SelectionMode::Cell => (start, end),
            SelectionMode::Word => (self.word_start(start), self.word_end(end)),
            SelectionMode::Line => {
                let last = self.active_tab().terminal.cols.saturating_sub(1);
                ((start.0, 0), (end.0, last))
            }
        };
        Some(Selection { anchor: start, head: end })
    }

    /// Walk left from `p` while the previous cell is a word char.
    fn word_start(&self, p: (isize, usize)) -> (isize, usize) {
        let Some(line) = self.active_tab().terminal.line_at(p.0) else { return p };
        if p.1 >= line.len() || !is_word_char(line[p.1].ch) {
            return p;
        }
        let mut col = p.1;
        while col > 0 && is_word_char(line[col - 1].ch) {
            col -= 1;
        }
        (p.0, col)
    }

    /// Walk right from `p` while the next cell is a word char.
    fn word_end(&self, p: (isize, usize)) -> (isize, usize) {
        let Some(line) = self.active_tab().terminal.line_at(p.0) else { return p };
        if p.1 >= line.len() || !is_word_char(line[p.1].ch) {
            return p;
        }
        let mut col = p.1;
        while col + 1 < line.len() && is_word_char(line[col + 1].ch) {
            col += 1;
        }
        (p.0, col)
    }

    fn clear_selection(&mut self) -> bool {
        // Reset multi-click bookkeeping too — typing should make the next
        // click count as a fresh single-click.
        self.active_tab_mut().last_click = None;
        self.active_tab_mut().click_count = 0;
        if self.active_tab().selection.is_some() {
            self.active_tab_mut().selection = None;
            true
        } else {
            false
        }
    }

    /// Materialize the current selection as plain text, trimming trailing
    /// whitespace per line and joining with '\n'.
    fn selection_text(&self) -> Option<String> {
        let sel = self.active_tab().selection.as_ref()?;
        let (start, end) = sel.range();
        let mut out = String::new();
        for line in start.0..=end.0 {
            let Some(cells) = self.active_tab().terminal.line_at(line) else { continue };
            let from = if line == start.0 { start.1 } else { 0 };
            let to_inclusive = if line == end.0 { end.1 } else { cells.len().saturating_sub(1) };
            let to = (to_inclusive + 1).min(cells.len());
            let from = from.min(to);
            let row_text: String = cells[from..to].iter().map(|c| c.ch).collect();
            // Trim trailing spaces — selecting a full line shouldn't paste
            // padding into the clipboard.
            let trimmed = row_text.trim_end_matches(' ');
            out.push_str(trimmed);
            if line < end.0 {
                out.push('\n');
            }
        }
        Some(out)
    }

    fn copy_selection(&self) {
        let Some(text) = self.selection_text() else { return };
        if text.is_empty() {
            return;
        }
        match arboard::Clipboard::new().and_then(|mut c| c.set_text(text)) {
            Ok(()) => {}
            Err(e) => eprintln!("clipboard write failed: {e}"),
        }
    }

    /// Select and copy the most recent completed command's output (OSC 133
    /// `OutputStart`..`CommandEnd`). Returns false (no-op) when no completed
    /// command has any output. Drives the Cmd-Shift-O keybinding.
    fn select_last_command_output(&mut self) -> bool {
        let Some((start_line, end_line)) = self.active_tab().terminal.last_command_output_span() else {
            return false;
        };
        let last_col = self.active_tab().terminal.cols.saturating_sub(1);
        self.active_tab_mut().selection = Some(Selection {
            anchor: (start_line, 0),
            head: (end_line, last_col),
        });
        self.active_tab_mut().selection_mode = SelectionMode::Cell;
        self.copy_selection();
        self.invalidate();
        true
    }

    /// Read the system clipboard and write it to the PTY, wrapped in
    /// bracketed-paste markers if the host has enabled them.
    /// Read `path` from disk and fire a decode job. The placement on the
    /// active grid lands later, when `poll_pending_images` (called each
    /// frame) sees the worker's result and computes the cell extent from
    /// the now-known image dimensions + current font metrics.
    fn load_image_at_cell(&mut self, path: &str, row: isize, col: isize, label: &str) {
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
    fn poll_pending_images(&mut self) {
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

    /// Decide whether `poll_pending_images` should auto-create a
    /// Placement when this upload's decode succeeds.
    ///
    /// Three buckets:
    ///   - Cmd-Shift-I debug-paste flow: no `kitty_image_id`, no
    ///     up-front display → main.rs computes extent from pixel
    ///     dims and places at the cursor on decode. Returns `false`
    ///     here (meaning: deferred-auto-place IS desired, caller
    ///     stores `None`).
    ///   - `a=T` (Transmit and Display) without `U=1` →
    ///     `insert_placement_kitty` already ran before this point;
    ///     no auto-place needed.
    ///   - Any Kitty transmit-only path (`a=t`, `a=T,U=1`) → client
    ///     owns placement timing. Either an `a=p` later, or
    ///     `U+10EEEE` placeholder cells. Auto-placing a second
    ///     Placement at the cursor produces a ghost image that
    ///     renders alongside the placeholder-bbox draw.
    ///
    /// Returns `true` when the upload should suppress the deferred
    /// auto-placement.
    fn suppress_deferred_placement(
        display_immediately: bool,
        kitty_image_id: Option<u32>,
    ) -> bool {
        display_immediately || kitty_image_id.is_some()
    }

    /// Decision oracle for "should we render this placement as half-block
    /// glyphs?". Split from `halfblock_overrides` so the matrix of
    /// (config × store-state) outcomes can be tested in isolation without
    /// the surrounding Store / Terminal scaffolding.
    fn should_halfblock(
        images_enabled: bool,
        opted_in: bool,
        is_pending: bool,
        gpu_image_available: bool,
    ) -> bool {
        // Decode-in-flight always wins: even if the user opted in,
        // flipping briefly to a thumbnail and then snapping to GPU
        // pixels would flicker badly.
        if is_pending {
            return false;
        }
        if !images_enabled {
            // GPU path disabled — half-block is the only thing the user
            // can see. (Default value of `opted_in` doesn't gate this
            // case; disabling images entirely already implies wanting
            // *some* representation.)
            return true;
        }
        // Images enabled and the GPU has the texture: GPU path draws.
        if gpu_image_available {
            return false;
        }
        // Images enabled, GPU image missing, decode not in flight ⇒
        // a failed decode where main.rs's cleanup hasn't fired yet (one
        // frame, typically). Honor the opt-in.
        opted_in
    }

    /// Convert a live placement's grid-coord `top_row` into the viewport
    /// row the renderer should draw at, given the current scrollback
    /// view offset. Mirrors what `extended_cell` does for cells: live
    /// row R lands at viewport row `R + view_offset`. The shift is
    /// uncapped — `view_offset > rows` is still meaningful because
    /// smooth-scroll's `scroll_y` interpolates between view_offset
    /// ticks. Clamping at `rows` here makes the discrete viewport_row
    /// stop moving past the bottom while scroll_y keeps advancing,
    /// which produces a visible snap each time scroll_y crosses a
    /// line boundary (the image slides a row visually via scroll_y,
    /// then jumps back when the tick fires because viewport_row
    /// didn't move).
    ///
    /// Scrollback placements come pre-shifted out of
    /// `Terminal::scrollback_placements_in_view`, so this helper
    /// applies only to live placements. Off-screen draws are
    /// naturally clipped by the rasterizer; passing a viewport_row
    /// well past `rows` costs only the vertex buffer write.
    ///
    /// `rows` is kept on the signature so call sites don't change
    /// shape if a future clamp becomes desirable.
    fn live_placement_viewport_row(top_row: isize, view_offset: usize, _rows: usize) -> isize {
        top_row + view_offset as isize
    }

    /// Compute the UV sub-rect for one Kitty placeholder run against
    /// the source image's total cell extent `(total_cols, total_rows)`.
    /// Clamps to `[0.0, 1.0]` so a malformed encoder (or a placeholder
    /// grid that survives a smaller-than-original re-transmission)
    /// samples the edge instead of wrapping or sampling outside the
    /// texture. `total_cols` / `total_rows` are clamped to `>= 1`
    /// since they're the denominator — a 0 here would NaN every UV.
    /// Returns `(u0, v0, u1, v1)`.
    fn placeholder_run_uv(
        image_col_start: u16,
        image_col_end: u16,
        image_row: u16,
        total_cols: u32,
        total_rows: u32,
    ) -> (f32, f32, f32, f32) {
        let denom_cols = total_cols.max(1) as f32;
        let denom_rows = total_rows.max(1) as f32;
        let u0 = (image_col_start as f32 / denom_cols).clamp(0.0, 1.0);
        let u1 = (image_col_end as f32 / denom_cols).clamp(0.0, 1.0);
        let v0 = (image_row as f32 / denom_rows).clamp(0.0, 1.0);
        let v1 = ((image_row as f32 + 1.0) / denom_rows).clamp(0.0, 1.0);
        (u0, v0, u1, v1)
    }

    /// UV sub-rect for one cell-filling glyph quad, including the
    /// half-texel inset that closes hairline seams between abutting
    /// block / box-drawing glyphs. `(gx, gy)` is the glyph's atlas
    /// origin in texels; `(q_start, q_end)` / `(p_start, p_end)` are the
    /// in-glyph sample extents (horizontal / vertical) already clamped to
    /// the glyph bitmap; `atlas_w` / `atlas_h` are the atlas dimensions in
    /// texels.
    ///
    /// Synthesized cell-filling glyphs have *hard* opaque edges, and the
    /// atlas packs every glyph with one transparent column/row of padding
    /// (`stride = w + 1` in font.rs). Sampling right up to the texel
    /// boundary therefore makes the Linear filter average the opaque edge
    /// with that transparent neighbour (~50% alpha — a visible seam
    /// between abutting cells). So for a cell-filling glyph we pull the UV
    /// in by half a texel on *both* axes, landing every edge fragment on a
    /// fully-opaque texel center.
    ///
    /// The inset is gated on `cell_filling`, not on whether the axis is
    /// stretched: a glyph trimmed to its opaque bounding box (e.g. ▐,
    /// packed as a half-width bitmap bearing into the cell) is placed 1:1
    /// on its narrow axis yet its filled side still reaches the bitmap
    /// edge and would bleed into the padding there — that was the residual
    /// seam a fills_h/fills_v-only inset left behind. Normal glyphs
    /// (`cell_filling == false`) keep their true extent so they aren't
    /// thinned or shifted. Returns `(u0, v0, u1, v1)`.
    fn glyph_quad_uv(
        gx: f32,
        gy: f32,
        (q_start, q_end): (f32, f32),
        (p_start, p_end): (f32, f32),
        cell_filling: bool,
        atlas_w: f32,
        atlas_h: f32,
    ) -> (f32, f32, f32, f32) {
        let inset = if cell_filling { 0.5 } else { 0.0 };
        let u0 = (gx + q_start + inset) / atlas_w;
        let u1 = (gx + q_end - inset) / atlas_w;
        let v0 = (gy + p_start + inset) / atlas_h;
        let v1 = (gy + p_end - inset) / atlas_h;
        (u0, v0, u1, v1)
    }

    /// Build the per-cell half-block override map for the upcoming vertex
    /// rebuild. Empty in the common case (images enabled and decoded), so
    /// the lookup in the cell loop is a single hash miss per cell.
    ///
    /// Three cases produce overrides:
    ///   1. `images_enabled = false` AND a preview is cached (decoded
    ///      before the toggle, or some other process populated it).
    ///   2. `images_enabled = true` AND `peek` returns None AND the
    ///      placement is *not* in-flight (decode failed, but cleanup
    ///      hasn't fired yet — typically one frame) AND the user opted
    ///      in via `images_halfblock_for_missing`.
    ///   3. *Never* when `is_pending` is true: the decode is in flight
    ///      and pixels are arriving within a frame or two. Showing a
    ///      half-block thumbnail and then snapping to GPU pixels would
    ///      flicker badly.
    ///
    /// Returns a row-major hash keyed on viewport-coord `(row, col)` so
    /// the cell loop can probe with the same coords it already iterates.
    fn halfblock_overrides(
        &self,
    ) -> std::collections::HashMap<(isize, usize), images::HalfblockCell> {
        let mut out = std::collections::HashMap::new();
        let images_enabled = self.config.images_enabled;
        let opted_in = self.config.images_halfblock_for_missing;
        // Early-out: no possible override when images are on and the user
        // hasn't opted in to the failure-cleanup window.
        if images_enabled && !opted_in {
            return out;
        }
        let cols = self.active_tab().terminal.cols;
        let view_offset = self.active_tab().terminal.view_offset();
        let rows = self.active_tab().terminal.rows;
        for p in self.active_tab().terminal.live_placements() {
            let is_pending = self.active_tab().image_store.is_pending(p.image);
            let gpu_ok = self.active_tab().image_store.peek(p.image).is_some();
            if !Self::should_halfblock(images_enabled, opted_in, is_pending, gpu_ok) {
                continue;
            }
            let Some(preview) = self.active_tab().image_store.preview(p.image) else {
                // Disabled-but-no-preview: nothing to draw with. The
                // empty space is the right behaviour here; users who
                // want a placeholder would have to wait for a future
                // sub-slice that synthesizes a generic frame.
                continue;
            };
            // Grid → viewport row shift matches what cells get; see
            // `live_placement_viewport_row`.
            let top_vp = Self::live_placement_viewport_row(p.top_row, view_offset, rows);
            let cells = images::halfblock_cells(preview, p.rows, p.cols);
            for hb in cells {
                let r = top_vp + hb.row_offset as isize;
                let c = p.left_col + hb.col_offset as isize;
                // Clip to viewport — phantom rows above/below are still
                // valid (`extended_cell` reads them), but a placement
                // extending past the grid horizontally would land off
                // any drawn cell.
                if c < 0 || c >= cols as isize {
                    continue;
                }
                // Vertical clipping happens implicitly in the cell
                // emission loop (it iterates r_lo..r_hi); an entry whose
                // row falls outside that range is silently ignored
                // there. Keeping the entry in the map costs one HashMap
                // slot per scrolled-off row and avoids duplicating the
                // r_lo / r_hi math here.
                out.insert((r, c as usize), hb);
            }
        }
        out
    }

    /// Wrap `Terminal::feed` so any OSC-1337 payloads parsed in this PTY
    /// chunk are turned into real Store reservations + Placements before
    /// the next chunk is processed. Without this, a follow-up chunk
    /// containing scroll/text could mutate the grid between cursor
    /// advance and placement insertion — leaving the placement at a
    /// stale anchor. See sub-slice P2.4 design notes.
    fn feed_terminal(&mut self, bytes: &str) {
        self.active_tab_mut().terminal.feed(bytes);
        self.maybe_start_alt_scroll();
        self.drain_pending_image_uploads();
    }

    /// If the just-fed output scrolled the alt screen (an explicit SU/SD or
    /// line-feed), kick off a smooth slide for it. The terminal has stashed the
    /// departing rows; we set the initial `scroll_y` offset and let
    /// `update_alt_scroll` ease it to zero over `ALT_SCROLL_ANIM_SECS`.
    fn maybe_start_alt_scroll(&mut self) {
        if ALT_SCROLL_ANIM_SECS <= 0.0 {
            return;
        }
        let Some(scroll) = self.active_tab_mut().terminal.take_alt_scroll() else {
            return;
        };
        // The renderer slides the region and clips its bottom edge, so the
        // departing rows must exit at the top (behind the toolbar). That holds
        // only when the region is anchored at row 0 — the common case (apps
        // reserve a *bottom* status line). A region starting mid-screen would
        // bleed past its top edge, so skip the slide there; the scroll has
        // already been applied to the grid, it just snaps instead of animating.
        if scroll.region_top != 0 {
            self.active_tab_mut().terminal.clear_alt_anim();
            return;
        }
        let metrics = self.shared.with_font(|f| f.face().size_metrics().unwrap());
        let line_height = ((metrics.ascender - metrics.descender) >> 6) as f32;
        let total_px = scroll.rows as f32 * line_height;
        self.active_tab_mut().alt_scroll_anim = Some(AltScrollAnim {
            up: scroll.up,
            rows: scroll.rows,
            region_top: scroll.region_top,
            region_bottom: scroll.region_bottom,
            total_px,
            started: std::time::Instant::now(),
        });
        // Start displaying the pre-scroll frame: shift the (already-scrolled)
        // grid back by the full distance so the departing rows fill the gap.
        self.active_tab_mut().scroll_y = if scroll.up { total_px } else { -total_px } as f64;
        self.invalidate();
    }

    /// Advance the alt-screen scroll slide for this frame, easing `scroll_y`
    /// toward zero. Finishes (and releases the frozen rows) when the slide
    /// completes or the alt screen is no longer active. No-op when idle.
    fn update_alt_scroll(&mut self) {
        let Some(anim) = self.active_tab().alt_scroll_anim else {
            return;
        };
        if !self.active_tab().terminal.on_alt_screen() {
            self.finish_alt_scroll();
            self.active_tab_mut().scroll_y = 0.0;
            return;
        }
        let t = (anim.started.elapsed().as_secs_f32() / ALT_SCROLL_ANIM_SECS).clamp(0.0, 1.0);
        // Ease-out cubic: quick to start, gentle to settle.
        let eased = 1.0 - (1.0 - t).powi(3);
        let remaining = anim.total_px * (1.0 - eased);
        if t >= 1.0 || remaining <= 0.5 {
            self.finish_alt_scroll();
            self.active_tab_mut().scroll_y = 0.0;
            return;
        }
        self.active_tab_mut().scroll_y = if anim.up { remaining } else { -remaining } as f64;
    }

    /// End any alt-screen scroll slide and drop the terminal's frozen rows.
    /// Leaves `scroll_y` untouched — callers that cancel mid-slide (a keystroke
    /// snapping the view) zero it themselves.
    fn finish_alt_scroll(&mut self) {
        if self.active_tab_mut().alt_scroll_anim.take().is_some() {
            self.active_tab_mut().terminal.clear_alt_anim();
        }
    }

    /// True while an alt-screen scroll slide is mid-flight — keeps the event
    /// loop ticking frames until it settles.
    fn is_alt_scroll_animating(&self) -> bool {
        self.active_tab().alt_scroll_anim.is_some()
    }

    /// Refresh the cached completion-popup suggestions, but only when the
    /// shell's reported input (`OSC 2122`) actually changed since last time —
    /// `completion::complete_path` does a `read_dir`, so it must never run per
    /// frame. The popup is gated on a non-empty path token so an empty prompt
    /// doesn't dump the whole cwd. Returns true if the cache changed (so the
    /// caller can request a redraw).
    fn recompute_completions(&mut self) -> bool {
        let cur = self.active_tab().terminal.current_input().cloned();
        let key = cur.as_ref().map(|c| (c.buffer.clone(), c.cursor));
        if key == self.active_tab().completions_input {
            return false;
        }
        self.active_tab_mut().completions_input = key;
        // A dismissed popup (Enter/Esc) must not reopen when the shell re-emits
        // OSC 2122 after an accepted suffix — keep it empty until a real
        // keystroke clears the flag. The `autocomplete` config gate works the
        // same way: when the feature is off, never run `completion::complete`,
        // just keep the cache empty (read live so config reload toggles it).
        self.active_tab_mut().completions = if self.active_tab().completion_dismissed || !self.config.autocomplete {
            Vec::new()
        } else {
            match &cur {
                // `complete` returns empty for empty/whitespace input on its own
                // (no history match, empty token skipped), so no separate gate.
                Some(c) => {
                    let cwd = self.active_tab().terminal.cwd().map(std::path::Path::new);
                    let path = std::env::var_os("PATH");
                    let home = std::env::var_os("HOME");
                    completion::complete(
                        &c.buffer,
                        c.cursor,
                        cwd,
                        path.as_deref(),
                        home.as_deref().map(std::path::Path::new),
                        &self.active_tab().command_history,
                        false,
                    )
                }
                None => Vec::new(),
            }
        };
        // The cache was replaced: restart selection at the top and reset the
        // scroll window so navigation state never points past a shorter list.
        self.active_tab_mut().selected_completion = 0;
        self.active_tab_mut().completion_scroll = 0;
        true
    }

    /// Manually (re)open the completion popup for the current input — the
    /// Ctrl+Space command. Unlike the automatic path, this bypasses the
    /// `has_path_token` gate, so triggering on an empty token lists the whole
    /// working directory ("show me what's here"). Clears the dismissed flag so a
    /// popup closed with Esc/Enter comes back, and pins `completions_input` to the
    /// current input so the next (unchanged-input) recompute doesn't immediately
    /// wipe the freshly-summoned list.
    fn trigger_completion(&mut self) {
        // Honor the `autocomplete` config gate: no manual summon when the
        // feature is off. Read live so a config reload toggles it.
        if !self.config.autocomplete {
            return;
        }
        self.active_tab_mut().completion_dismissed = false;
        // Clone the buffer/cursor out of the immutable `current_input()` borrow so
        // it ends before we take `cwd()` and then mutably write `self.*` fields.
        if let Some((buffer, cursor)) = self.active_tab()
            .terminal
            .current_input()
            .map(|c| (c.buffer.clone(), c.cursor))
        {
            let cwd = self.active_tab().terminal.cwd().map(std::path::Path::new);
            let path = std::env::var_os("PATH");
            let home = std::env::var_os("HOME");
            self.active_tab_mut().completions = completion::complete(
                &buffer,
                cursor,
                cwd,
                path.as_deref(),
                home.as_deref().map(std::path::Path::new),
                &self.active_tab().command_history,
                true,
            );
            self.active_tab_mut().completions_input = Some((buffer, cursor));
            self.active_tab_mut().selected_completion = 0;
            self.active_tab_mut().completion_scroll = 0;
        }
        self.invalidate();
    }

    /// Accept the highlighted suggestion: write the bytes that extend the typed
    /// token into the chosen path, straight to the PTY (the shell's line editor
    /// inserts them at the cursor). Computes the suffix against the LIVE buffer
    /// (not the cached one) and bails if they no longer agree, so a stale cache
    /// can't inject wrong bytes.
    ///
    /// `keep_open` distinguishes drilling into a directory (Tab on a dir) from
    /// finishing (Enter, or Tab on a file): when set, the popup is left alone so
    /// the shell's round-trip (echoed suffix → new `$BUFFER` → OSC 2122) can
    /// refilter the list to the subdirectory's contents; otherwise the popup is
    /// closed and kept closed via `completion_dismissed`.
    ///
    /// `submit` runs the command in the same keystroke (Shift+Enter): the
    /// accept-suffix (if any) is written followed by a carriage return, so the
    /// line executes even when the suggestion didn't extend it (the buffer
    /// submits as-typed). `submit` implies not-keep-open and always closes the
    /// popup (the command is running, so the popup must go).
    fn accept_selected_completion(&mut self, keep_open: bool, submit: bool) {
        let Some(sug) = self.active_tab().completions.get(self.active_tab().selected_completion) else {
            return;
        };
        // Clone the suggestion so the byte payload below can hold the immutable
        // `self.active_tab().terminal` borrow without also borrowing `self.active_tab().completions`.
        let sug = sug.clone();
        // Build the byte payload (cloning the suffix) while holding the
        // immutable `self.active_tab().terminal` borrow, then drop it before the &self
        // `write_pty` call below.
        let bytes: Option<Vec<u8>> = self.active_tab().terminal.current_input().map(|c| {
            let mut bytes = match completion::accept_suffix(&c.buffer, c.cursor, &sug) {
                Some(suffix) => suffix.as_bytes().to_vec(),
                // A stale / non-extending suggestion: no suffix to insert, but
                // we may still submit the line as-typed below.
                None => Vec::new(),
            };
            if submit {
                bytes.push(b'\r');
            }
            bytes
        });
        if let Some(bytes) = bytes {
            if !bytes.is_empty() {
                self.write_pty(&bytes);
            }
        }
        if submit {
            // Submitting: the command is running, so the popup must close and
            // stay closed until the user types again.
            self.active_tab_mut().completions.clear();
            self.active_tab_mut().completions_input = None;
            self.active_tab_mut().completion_dismissed = true;
        } else if keep_open {
            // Drilling into a directory: force a fresh recompute on the next
            // OSC 2122 report, but leave the popup open and undismissed so the
            // round-trip refilters to the subdirectory's contents.
            self.active_tab_mut().completions_input = None;
        } else {
            // Finishing: close and keep closed until the user types.
            self.active_tab_mut().completions.clear();
            self.active_tab_mut().completions_input = None;
            self.active_tab_mut().completion_dismissed = true;
        }
        self.active_tab_mut().selected_completion = 0;
        self.active_tab_mut().completion_scroll = 0;
        self.invalidate();
    }

    /// Pull every iTerm2 OSC-1337 (and future protocol) payload off the
    /// Terminal's outbox, fire a decode job per upload with the
    /// pre-allocated ImageId, and insert the placement at the cell
    /// anchor the parser captured. Placement creation happens *before*
    /// the worker finishes the decode; `Store::peek` returns `None`
    /// until then so the renderer skips drawing for one or two frames.
    fn drain_pending_image_uploads(&mut self) {
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
    fn sync_terminal_cell_size(&mut self) {
        let metrics = self.shared.with_font(|f| f.face().size_metrics().unwrap());
        let line_h = ((metrics.ascender - metrics.descender) >> 6) as u32;
        let cell_w = self.shared.with_font(|f| f.cell_width()) as u32;
        self.active_tab_mut().terminal.set_cell_size_px(cell_w, line_h);
    }

    /// Path used by the Cmd-Shift-I keybind. Env var override beats the
    /// config-dir default so a quick `YUTANI_DEBUG_IMAGE=foo.png cargo run`
    /// works without touching the file system.
    fn debug_image_path() -> Option<std::path::PathBuf> {
        if let Ok(p) = std::env::var("YUTANI_DEBUG_IMAGE") {
            return Some(std::path::PathBuf::from(p));
        }
        let mut p = config_dir()?;
        p.push("debug_image.png");
        Some(p)
    }

    fn paste_from_clipboard(&self) {
        let text = match arboard::Clipboard::new().and_then(|mut c| c.get_text()) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("clipboard read failed: {e}");
                return;
            }
        };
        if self.active_tab().terminal.bracketed_paste() {
            self.write_pty(b"\x1b[200~");
            self.write_pty(text.as_bytes());
            self.write_pty(b"\x1b[201~");
        } else {
            self.write_pty(text.as_bytes());
        }
    }

    fn input(
        &mut self,
        event: &WindowEvent,
        _elwt: &EventLoopWindowTarget<app_window::CustomEvent>,
    ) -> bool {
        match event {
            WindowEvent::CursorMoved { position, .. } => {
                self.mouse_x = position.x;
                self.mouse_y = position.y;
                // Pointer over the title bar / toolbar: swallow it so motion
                // never becomes a mouse report or extends a selection into the
                // chrome. Show the arrow instead of the grid's I-beam, and drop
                // any hovered-URL highlight since nothing hoverable is up there.
                if self.in_top_toolbar(position.y) {
                    // Re-assert the arrow on *every* move within the band, not
                    // just on entry. set_cursor_icon keeps winit's stored cursor
                    // in sync (and covers non-macOS), but on macOS it's applied
                    // lazily via cursorUpdate:, which never fires over the native
                    // title-bar overlay — so we also push the arrow straight onto
                    // NSCursor here, or the grid's I-beam stays frozen on screen
                    // while the pointer is up here. See force_native_arrow_cursor.
                    self.over_toolbar = true;
                    self.window
                        .set_cursor_icon(winit::window::CursorIcon::Default);
                    force_native_arrow_cursor(&self.window);
                    if self.active_tab_mut().hover_url.take().is_some() {
                        self.invalidate();
                    }
                    return true;
                }
                if self.over_toolbar {
                    // Crossed back into the grid: restore the I-beam.
                    // update_hover_url below upgrades it to a pointer if the
                    // cursor is over a Cmd-hovered URL.
                    self.over_toolbar = false;
                    self.window
                        .set_cursor_icon(winit::window::CursorIcon::Text);
                }
                // Mouse-mode reporting takes precedence unless the user is
                // shift-overriding it for local selection.
                let mouse_mode_active =
                    self.active_tab().terminal.mouse_protocol().enabled() && !self.modifiers.shift_key();
                if mouse_mode_active {
                    if let Some(b) = self.held_button {
                        self.report_mouse(b, true, true);
                    } else if self.active_tab().terminal.mouse_protocol().any_motion {
                        // Per xterm, "no button" motion uses code 3 (release-ish).
                        self.report_mouse(3, true, true);
                    }
                } else if self.held_button == Some(input::MOUSE_LEFT) {
                    self.handle_mouse_drag();
                    self.invalidate();
                }
                self.update_hover_url();
            }
            WindowEvent::MouseInput { state, button, .. } => {
                // Press/release over the title bar / toolbar drives the window
                // chrome (drag, traffic lights), never the shell. Clear any
                // held button so a press that began here can't seed a phantom
                // selection or motion report once the pointer moves down.
                if self.in_top_toolbar(self.mouse_y) {
                    self.held_button = None;
                    if *button == MouseButton::Left && *state == ElementState::Pressed {
                        let now = std::time::Instant::now();
                        let double = self
                            .last_toolbar_click
                            .map_or(false, |t| now.duration_since(t) < DOUBLE_CLICK_THRESHOLD);
                        if double {
                            // Double-click the title bar zooms the window, the
                            // standard macOS gesture. We have to do this
                            // ourselves: drag_window below consumes the first
                            // click's mouseDown in a modal tracking loop, so the
                            // OS never sees the pair as a double-click.
                            self.last_toolbar_click = None;
                            self.window.set_maximized(!self.window.is_maximized());
                        } else {
                            self.last_toolbar_click = Some(now);
                            // Kick off a native window drag immediately. Our
                            // content view spans the title bar
                            // (fullsize_content_view) and consumes this
                            // mouseDown, so without this AppKit falls back to its
                            // slow drag path — the window only starts following
                            // the cursor after a ~1s hesitation. Routing the live
                            // mouseDown into performWindowDragWithEvent: makes the
                            // drag begin on the first movement. Clicks on the
                            // traffic lights don't reach us (system subviews on
                            // top), so this only fires on the empty draggable
                            // strip.
                            let _ = self.window.drag_window();
                        }
                    }
                    return true;
                }
                let code = match button {
                    MouseButton::Left => Some(input::MOUSE_LEFT),
                    MouseButton::Middle => Some(input::MOUSE_MIDDLE),
                    MouseButton::Right => Some(input::MOUSE_RIGHT),
                    _ => None,
                };
                if let Some(code) = code {
                    let press = *state == ElementState::Pressed;
                    if press {
                        self.held_button = Some(code);
                    } else {
                        self.held_button = None;
                    }
                    // Cmd-click on a hovered URL opens it. Done before the
                    // mouse-mode check so the gesture works even when an app
                    // (vim, less) has grabbed mouse tracking, matching the
                    // behavior every other macOS terminal ships.
                    if press
                        && code == input::MOUSE_LEFT
                        && self.modifiers.super_key()
                    {
                        if let Some(hu) = self.active_tab().hover_url.clone() {
                            if is_safe_url(&hu.url) {
                                open_url(&hu.url);
                            }
                            // Consume the click either way: a Cmd-click on a
                            // link shouldn't also fall through to selection.
                            return true;
                        }
                    }
                    let mouse_mode_active = self.active_tab().terminal.mouse_protocol().enabled()
                        && !self.modifiers.shift_key();
                    if mouse_mode_active {
                        self.report_mouse(code, press, false);
                        return true;
                    }
                    // Local selection: left-down anchors a fresh range,
                    // left-up either keeps the drag-built range or drops a
                    // bare click.
                    if code == input::MOUSE_LEFT {
                        if press {
                            self.handle_mouse_press();
                        } else {
                            self.handle_mouse_release();
                        }
                        self.invalidate();
                        return true;
                    }
                }
            }
            WindowEvent::MouseWheel { delta, phase, .. } => {
                // Wheel/scroll while the pointer sits over the title bar /
                // toolbar shouldn't reach the shell's wheel reporting nor the
                // local scrollback — swallow it like the other chrome events.
                if self.in_top_toolbar(self.mouse_y) {
                    return true;
                }
                let m = self.shared.with_font(|f| f.face().size_metrics().unwrap());
                let line_height = ((m.ascender - m.descender) >> 6) as f64;
                // A `Started` after a real idle gap is the user putting fingers
                // back on the trackpad — that supersedes any prior suppression.
                // Without the gap check, momentum's own Started (which fires
                // ~one frame after the previous gesture's Ended) would clear
                // the flag and let the tail of the flick re-scroll the view
                // after a key snap.
                const FRESH_GESTURE_GAP: std::time::Duration =
                    std::time::Duration::from_millis(100);
                let now = std::time::Instant::now();
                let gap = self.active_tab().last_wheel_at.map(|t| now.duration_since(t));
                if matches!(phase, TouchPhase::Started)
                    && gap.map_or(true, |g| g >= FRESH_GESTURE_GAP)
                {
                    self.active_tab_mut().scroll_suppressed = false;
                }
                if self.active_tab().scroll_suppressed {
                    // Don't advance `last_wheel_at` on suppressed events —
                    // otherwise the steady stream of momentum ticks keeps
                    // resetting the idle gap, and a real fresh gesture that
                    // arrives mid-momentum still looks like a 16ms follow-up.
                    return true;
                }
                self.active_tab_mut().last_wheel_at = Some(now);
                // Scroll-wheel forwarding to the PTY when an app has asked
                // for mouse tracking (vim, less, htop). Otherwise the wheel
                // drives our own scrollback viewport.
                if self.active_tab().terminal.mouse_protocol().enabled() {
                    // Accumulate pixels so a slow trackpad gesture (many
                    // sub-line events) still produces wheel reports instead
                    // of truncating every event to 0. LineDelta synthesizes
                    // pixels at line_height so both inputs share the drain.
                    let pixels = match delta {
                        MouseScrollDelta::LineDelta(_, d) => *d as f64 * line_height,
                        MouseScrollDelta::PixelDelta(p) => p.y,
                    };
                    let notches = input::drain_wheel_accum(
                        &mut self.active_tab_mut().wheel_pty_accum,
                        pixels,
                        line_height,
                    );
                    for _ in 0..notches.up {
                        self.report_mouse(input::MOUSE_WHEEL_UP, true, false);
                    }
                    for _ in 0..notches.down {
                        self.report_mouse(input::MOUSE_WHEEL_DOWN, true, false);
                    }
                    return true;
                }
                // Alt screen has no scrollback to navigate. With DEC mode
                // ?1007 (alternate scroll) — on by default, reachable here only
                // because mouse reporting is off (that path returned above) —
                // translate wheel motion into cursor-key presses so pagers
                // (less, man) and other full-screen apps scroll. When ?1007 is
                // disabled, swallow the wheel: full-screen apps provide their
                // own keyboard motion, and without this guard trackpad pixels
                // would accumulate in scroll_y and drift the grid past bounds.
                if self.active_tab().terminal.on_alt_screen() {
                    if self.active_tab().terminal.alternate_scroll() {
                        let pixels = match delta {
                            MouseScrollDelta::LineDelta(_, d) => *d as f64 * line_height,
                            MouseScrollDelta::PixelDelta(p) => p.y,
                        };
                        let notches = input::drain_wheel_accum(
                            &mut self.active_tab_mut().wheel_pty_accum,
                            pixels,
                            line_height,
                        );
                        let app_cursor = self.active_tab().terminal.app_cursor_keys();
                        for _ in 0..notches.up {
                            self.write_pty(&input::alt_scroll_key(true, app_cursor));
                        }
                        for _ in 0..notches.down {
                            self.write_pty(&input::alt_scroll_key(false, app_cursor));
                        }
                        // The app's response (a scroll op) will arrive on the
                        // next feed and (re)start the slide via
                        // `maybe_start_alt_scroll`; leave `scroll_y` to the
                        // animation rather than zeroing it here.
                    } else {
                        // No alternate scroll and no scrollback to navigate —
                        // swallow the wheel so trackpad pixels can't drift the
                        // grid via an accumulated offset.
                        self.active_tab_mut().scroll_y = 0.0;
                    }
                    return true;
                }
                match delta {
                    MouseScrollDelta::LineDelta(_, d) => {
                        let n = d.round().abs() as usize;
                        if *d > 0.0 {
                            self.active_tab_mut().terminal.scroll_up(n);
                        } else if *d < 0.0 {
                            self.active_tab_mut().terminal.scroll_down(n);
                        }
                        // Discrete scrolls snap — don't leave a sub-line offset.
                        self.active_tab_mut().scroll_y = 0.0;
                    }
                    MouseScrollDelta::PixelDelta(p) => {
                        self.active_tab_mut().scroll_y += p.y;
                        // Drain accumulated pixels into discrete line scrolls.
                        // Zero the residue if scroll_up/down refused so scroll_y
                        // can't accumulate past a viewport boundary regardless
                        // of what at_top/at_bottom report.
                        while self.active_tab().scroll_y >= line_height {
                            if !self.active_tab_mut().terminal.scroll_up(1) {
                                self.active_tab_mut().scroll_y = 0.0;
                                break;
                            }
                            self.active_tab_mut().scroll_y -= line_height;
                        }
                        while self.active_tab().scroll_y <= -line_height {
                            if !self.active_tab_mut().terminal.scroll_down(1) {
                                self.active_tab_mut().scroll_y = 0.0;
                                break;
                            }
                            self.active_tab_mut().scroll_y += line_height;
                        }
                        // Hard-stop at viewport boundaries: no elastic overscroll.
                        if self.active_tab().scroll_y > 0.0 && self.active_tab().terminal.at_top() {
                            self.active_tab_mut().scroll_y = 0.0;
                        }
                        if self.active_tab().scroll_y < 0.0 && self.active_tab().terminal.at_bottom() {
                            self.active_tab_mut().scroll_y = 0.0;
                        }
                    }
                }
                self.invalidate();
                // Content slid under the pointer — the URL (if any) might be
                // different now.
                self.update_hover_url();
                return true;
            }
            WindowEvent::ModifiersChanged(mods) => {
                self.modifiers = mods.state();
                // Pressing/releasing Cmd flips URL-hover affordances on or
                // off, even though the mouse hasn't moved.
                self.update_hover_url();
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state == winit::event::ElementState::Pressed {
                    // Cmd-F toggles the find-in-scrollback overlay. Checked
                    // first (and gated on !shift so it never fires for a
                    // Cmd-Shift chord) so it both opens the overlay and, while
                    // open, closes it before the overlay's key handler below
                    // swallows the keystroke. Closing leaves the viewport where
                    // it is so the found match stays in view.
                    if self.modifiers.super_key()
                        && !self.modifiers.shift_key()
                        && !self.command_palette.open
                    {
                        if let winit::keyboard::Key::Character(s) = &event.logical_key {
                            if s.eq_ignore_ascii_case("f") {
                                self.search.toggle();
                                self.invalidate();
                                return true;
                            }
                        }
                    }
                    // While the find overlay is open it owns the keyboard.
                    if self.search.open {
                        return self.search_key(&event);
                    }
                    // Cmd-Shift-P toggles the command palette. Checked before
                    // everything else so it both opens the palette and, while
                    // it's open, closes it (the palette's own key handler below
                    // otherwise swallows the keystroke).
                    if self.modifiers.super_key() && self.modifiers.shift_key() {
                        if let winit::keyboard::Key::Character(s) = &event.logical_key {
                            if s.eq_ignore_ascii_case("p") {
                                self.command_palette.toggle();
                                self.invalidate();
                                return true;
                            }
                        }
                    }
                    // While the palette is open it owns the keyboard: every
                    // keystroke filters/drives it and nothing reaches the PTY.
                    if self.command_palette.open {
                        return self.command_palette_key(&event);
                    }
                    // Cmd+C / Cmd+V: copy / paste through the system
                    // clipboard. Done before encode_key so the super_key
                    // check there doesn't drop them.
                    if self.modifiers.super_key() {
                        // Cmd-Shift-Up / Cmd-Shift-Down: jump the viewport to
                        // the previous / next shell prompt (OSC 133 marks).
                        // No-op when the shell isn't emitting marks or on the
                        // alt screen. Arrow keys arrive as Named, not
                        // Character, so handle them before the Character match.
                        if self.modifiers.shift_key() {
                            use winit::keyboard::{Key, NamedKey};
                            if event.logical_key == Key::Named(NamedKey::ArrowUp) {
                                if self.active_tab_mut().terminal.scroll_to_prev_prompt() {
                                    self.active_tab_mut().scroll_y = 0.0;
                                    self.invalidate();
                                    self.update_hover_url();
                                }
                                return true;
                            }
                            if event.logical_key == Key::Named(NamedKey::ArrowDown) {
                                if self.active_tab_mut().terminal.scroll_to_next_prompt() {
                                    self.active_tab_mut().scroll_y = 0.0;
                                    self.invalidate();
                                    self.update_hover_url();
                                }
                                return true;
                            }
                        }
                        if let winit::keyboard::Key::Character(s) = &event.logical_key {
                            if s.eq_ignore_ascii_case("c") {
                                self.copy_selection();
                                return true;
                            }
                            if s.eq_ignore_ascii_case("v") {
                                self.paste_from_clipboard();
                                return true;
                            }
                            // Cmd-N: launch a new Yutani window. It's a fresh
                            // process (one window per process), opened in the
                            // current shell's working directory. Guard on
                            // !shift so Cmd-Shift-N stays free for a future
                            // binding.
                            if !self.modifiers.shift_key() && s.eq_ignore_ascii_case("n") {
                                spawn_new_window(self.active_tab().terminal.cwd(), self.window_origin());
                                return true;
                            }
                            // Cmd-+ / Cmd-= zoom in, Cmd-- zooms out. macOS
                            // delivers `=` for the unshifted key and `+` when
                            // shift is held, so handle both as "increase".
                            if s.as_ref() == "+" || s.as_ref() == "=" {
                                self.change_font_size(1.0);
                                return true;
                            }
                            if s.as_ref() == "-" || s.as_ref() == "_" {
                                self.change_font_size(-1.0);
                                return true;
                            }
                            // Cmd-Shift-W: toggle wireframe debug view.
                            // Shift makes "w" arrive as "W"; check both for
                            // safety across keyboard layouts.
                            if self.modifiers.shift_key()
                                && (s.eq_ignore_ascii_case("w"))
                            {
                                if self.shared.wireframe_pipeline.is_some() {
                                    self.wireframe = !self.wireframe;
                                    self.window.request_redraw();
                                }
                                return true;
                            }
                            // Cmd-Shift-R: re-read the config file and
                            // swap the color scheme without restarting.
                            // Other config fields (font, pipeline knobs)
                            // still need a restart — see `reload_config`.
                            if self.modifiers.shift_key()
                                && (s.eq_ignore_ascii_case("r"))
                            {
                                self.reload_config();
                                return true;
                            }
                            // Cmd-Shift-I: load and place a debug image at
                            // the cursor. Path comes from `YUTANI_DEBUG_IMAGE`
                            // or `~/.config/yutani/debug_image.png`. Silent
                            // no-op (with stderr message) if neither exists.
                            if self.modifiers.shift_key()
                                && (s.eq_ignore_ascii_case("i"))
                            {
                                if let Some(path) = Self::debug_image_path() {
                                    let cur = self.active_tab().terminal.cursor();
                                    let row = cur.row as isize;
                                    let col = cur.col as isize;
                                    let path_str = path.to_string_lossy().to_string();
                                    self.load_image_at_cell(
                                        &path_str,
                                        row,
                                        col,
                                        "debug image (Cmd-Shift-I)",
                                    );
                                }
                                return true;
                            }
                            // Cmd-Shift-O: select and copy the last completed
                            // command's output (OSC 133 shell integration).
                            if self.modifiers.shift_key() && s.eq_ignore_ascii_case("o") {
                                self.select_last_command_output();
                                return true;
                            }
                            // Cmd-[ / Cmd-] tune the dual-Kawase iteration
                            // count live so the user can scrub through blur
                            // radii without recompiling.
                            if s.as_ref() == "[" || s.as_ref() == "{" {
                                self.blur.iterations = self.blur.iterations.saturating_sub(1).max(1);
                                self.config.blur_iterations = self.blur.iterations;
                                self.config.save();
                                println!("blur iterations: {}", self.blur.iterations);
                                self.window.request_redraw();
                                return true;
                            }
                            if s.as_ref() == "]" || s.as_ref() == "}" {
                                self.blur.iterations = (self.blur.iterations + 1)
                                    .min(renderer::blur::MAX_BLUR_ITERATIONS);
                                self.config.blur_iterations = self.blur.iterations;
                                self.config.save();
                                println!("blur iterations: {}", self.blur.iterations);
                                self.window.request_redraw();
                                return true;
                            }
                        }
                    }
                    // Ctrl+Space manually summons/refreshes the completion popup
                    // (autocomplete slice K14). Placed after the Cmd shortcuts and
                    // before the popup-active interception so it works whether or
                    // not a popup is currently open, and before the general
                    // encode_key path that would otherwise send NUL to the shell.
                    if self.modifiers.control_key()
                        && !self.modifiers.super_key()
                        && !self.modifiers.alt_key()
                    {
                        use winit::keyboard::{Key, NamedKey};
                        // winit 0.29 may deliver Space as Named(Space) (see
                        // input::named_key) or as a Character(" ") (folded to NUL
                        // by ctrl_byte); match both so the trigger is robust.
                        let is_space = matches!(&event.logical_key, Key::Named(NamedKey::Space))
                            || matches!(&event.logical_key, Key::Character(s) if s.as_str() == " ");
                        if is_space {
                            // Consume the key so the usual NUL byte doesn't reach
                            // the shell; summon/refresh the popup instead.
                            self.trigger_completion();
                            return true;
                        }
                    }
                    // Completion popup keyboard interaction (autocomplete slice
                    // K11). Only acts when the popup is genuinely active (cached
                    // suggestions + live view) and no Ctrl/Alt/Super is held;
                    // Shift is allowed so Shift-Tab can navigate up. Intercepting
                    // here — after the Cmd shortcuts, before the encode_key path —
                    // means these keys reach the shell normally when no popup is
                    // open, and only steer the popup while it is.
                    let popup_active =
                        !self.active_tab().completions.is_empty() && self.active_tab().terminal.view_offset() == 0;
                    let plain = !self.modifiers.control_key()
                        && !self.modifiers.alt_key()
                        && !self.modifiers.super_key();
                    if popup_active && plain {
                        use winit::keyboard::{Key, NamedKey};
                        let len = self.active_tab().completions.len();
                        match &event.logical_key {
                            Key::Named(NamedKey::ArrowDown) => {
                                self.active_tab_mut().selected_completion =
                                    (self.active_tab().selected_completion + 1).min(len - 1);
                                self.active_tab_mut().completion_scroll = completion::visible_window_start(
                                    self.active_tab().selected_completion,
                                    self.active_tab().completion_scroll,
                                    COMPLETION_MAX_VISIBLE,
                                );
                                self.invalidate();
                                return true;
                            }
                            // ArrowUp, and Shift-Tab, move the selection up.
                            Key::Named(NamedKey::ArrowUp) => {
                                self.active_tab_mut().selected_completion =
                                    self.active_tab().selected_completion.saturating_sub(1);
                                self.active_tab_mut().completion_scroll = completion::visible_window_start(
                                    self.active_tab().selected_completion,
                                    self.active_tab().completion_scroll,
                                    COMPLETION_MAX_VISIBLE,
                                );
                                self.invalidate();
                                return true;
                            }
                            Key::Named(NamedKey::Tab) if self.modifiers.shift_key() => {
                                self.active_tab_mut().selected_completion =
                                    self.active_tab().selected_completion.saturating_sub(1);
                                self.active_tab_mut().completion_scroll = completion::visible_window_start(
                                    self.active_tab().selected_completion,
                                    self.active_tab().completion_scroll,
                                    COMPLETION_MAX_VISIBLE,
                                );
                                self.invalidate();
                                return true;
                            }
                            // Tab accepts the suffix and, on a directory, drills
                            // in (popup stays open and refilters to the dir's
                            // contents); on a file it accepts and closes.
                            Key::Named(NamedKey::Tab) => {
                                let keep = self.active_tab()
                                    .completions
                                    .get(self.active_tab().selected_completion)
                                    .map(|s| s.is_dir)
                                    .unwrap_or(false);
                                self.accept_selected_completion(keep, false);
                                return true;
                            }
                            // Shift+Enter: accept the highlighted completion AND
                            // run the command in one keystroke (writes the
                            // accept-suffix + a carriage return, then closes the
                            // popup). Must precede the plain Enter arm.
                            Key::Named(NamedKey::Enter) if self.modifiers.shift_key() => {
                                self.accept_selected_completion(false, true);
                                return true;
                            }
                            // Enter accepts + closes + suppresses reopen. It
                            // intentionally does NOT submit the command — a
                            // second Enter (popup now empty, so not intercepted)
                            // submits normally.
                            Key::Named(NamedKey::Enter) => {
                                self.accept_selected_completion(false, false);
                                return true;
                            }
                            Key::Named(NamedKey::Escape) => {
                                // Dismiss without accepting: clear the list and
                                // set the dismissed flag so the popup stays
                                // closed even when the shell re-reports input,
                                // until the user types more.
                                self.active_tab_mut().completions.clear();
                                self.active_tab_mut().completions_input = None;
                                self.active_tab_mut().completion_dismissed = true;
                                self.invalidate();
                                return true;
                            }
                            _ => {}
                        }
                    }
                    // macOS Option-as-Meta: with Option held, winit reports the
                    // layout-composed char (e.g. Option+A → "å"). For terminal
                    // meta-bindings we want the base key, so `Option+A` sends
                    // `ESC a` rather than `ESC 0xC3 0xA5`. `key_without_modifiers`
                    // also resolves dead keys (Option+E → "e" instead of Dead('´')).
                    let alt_stripped = self
                        .modifiers
                        .alt_key()
                        .then(|| event.key_without_modifiers());
                    let logical_key = alt_stripped.as_ref().unwrap_or(&event.logical_key);
                    let text = if alt_stripped.is_some() {
                        None
                    } else {
                        event.text.as_deref()
                    };
                    let bytes = input::encode_key(
                        logical_key,
                        text,
                        self.modifiers,
                        self.active_tab().terminal.app_cursor_keys(),
                    );
                    if let Some(bytes) = bytes {
                        // A keystroke we're sending to the PTY snaps the view
                        // back to the live grid; passive modifiers (Cmd+C etc.)
                        // returned None and don't touch the scroll state.
                        self.active_tab_mut().terminal.scroll_to_bottom();
                        // A keystroke cancels any alt-screen scroll slide —
                        // snap straight to the settled frame.
                        self.finish_alt_scroll();
                        self.active_tab_mut().scroll_y = 0.0;
                        // Drop any in-flight trackpad momentum so the snap
                        // sticks — otherwise the tail of the flick keeps
                        // scrolling the view away from the bottom.
                        self.active_tab_mut().scroll_suppressed = true;
                        self.reset_blink();
                        self.clear_selection();
                        // A genuine keystroke re-enables the popup after a
                        // finish/dismiss. The auto-inserted accept suffix goes
                        // through `write_pty` directly (not this path), so
                        // accepting never clears the flag — only real input does.
                        self.active_tab_mut().completion_dismissed = false;
                        self.write_pty(&bytes);
                        self.invalidate();
                        return true;
                    }
                }
            }
            _ => (),
        }
        false
    }

    fn update(&mut self) {}

    fn render(
        &mut self,
        clear: wgpu::Color,
    ) -> Result<(std::time::Duration, bool), wgpu::SurfaceError> {
        let surface_t0 = std::time::Instant::now();
        let output = self.surface.surface.get_current_texture().unwrap();
        let surface_wait = surface_t0.elapsed();
        let view = output
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder =
            self.shared.gpu
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("terminal"),
                });

        // Three paths, chosen on the fly:
        //   - Fast path (no glow, no strips): one render pass straight to
        //     the swapchain, no offscreen anything. Saves ~5 fullscreen
        //     passes per frame — the dominant cost during PTY bursts.
        //   - Layered path (glow_on): bg quads → `blur.scene`, fg quads →
        //     `scene_fg`, then glow each independently and composite
        //     stacked. Lets bg glow bloom under fg text without washing
        //     the glyphs out, and keeps fg glow above its source.
        //   - Strip-only path (strips on, glow off): single scene render
        //     into `blur.scene`, blur, blit + strip overlay. The original
        //     behaviour from before the glow layering.
        let needs_strips = self.num_strip_indices > 0;
        let glow_on = self.glow.enabled();
        let needs_offscreen = needs_strips || glow_on;
        // Content scanlines paint over bg + fg (but not strips/UI). Runs
        // via a multiply-blend overlay pass; in the layered / strip-only
        // paths it's inserted into the composite render pass between
        // content and strips, in the fast path it's a second pass on
        // the swapchain with LoadOp::Load.
        let content_overlay_on = self.glow.match_content_scanlines;

        let bg_index_range = 0..self.num_bg_indices;
        let fg_index_range = self.num_bg_indices..self.num_indices;

        // `prepare_frame` (called by the redraw handler before `render`)
        // already polled the image store and ran mark-and-sweep, so the
        // store's state here matches what `update_vertices` saw. This
        // matters for the half-block fallback: vertex emission and image
        // draw selection now agree on `peek`'s return value.

        // Build per-frame image draw list from `live_placements()`. Cell
        // anchor → pixel rect uses the same font metrics + decorator_offset
        // + scroll_y that `update_vertices` applies to cell quads, so
        // images scroll smoothly alongside text.
        let metrics = self.shared.with_font(|f| f.face().size_metrics().unwrap());
        let line_height = ((metrics.ascender - metrics.descender) >> 6) as f32;
        let cell_w = self.shared.with_font(|f| f.cell_width()) as f32;
        let view_offset = self.active_tab().terminal.view_offset() as f32;
        let scrollback_len = if self.active_tab().terminal.on_alt_screen() {
            0.0
        } else {
            self.active_tab().terminal.scrollback_len() as f32
        };
        let dist_from_bottom = view_offset * line_height + self.active_tab().scroll_y as f32;
        let dist_from_top = (scrollback_len - view_offset) * line_height - self.active_tab().scroll_y as f32;
        let near = (dist_from_bottom / line_height)
            .min(dist_from_top / line_height)
            .clamp(0.0, 1.0);
        let decorator_offset = DECORATOR_HEIGHT * (1.0 - near);
        let scroll_y = self.active_tab().scroll_y as f32;

        // Resolve store entries up front so the borrow can live alongside
        // the upcoming `&mut encoder` calls. Placements whose image was
        // already evicted (shouldn't happen with mark-and-sweep, but
        // defensible) are silently skipped. When `images_enabled = false`
        // we never call the GPU image pipeline — the half-block fallback
        // (computed in `update_vertices` via `halfblock_overrides`) is
        // the only visible representation in that case.
        //
        // When images are on, two walks share the same pixel math: live
        // placements (always) plus, when the user has scrolled history
        // into view on the primary screen, scrollback placements rebased
        // into viewport-row coords by `scrollback_placements_in_view`.
        // The two walks differ in one thing: live placements carry GRID
        // coords in `top_row`, while scrollback placements come pre-
        // shifted into viewport coords. Live placements need the same
        // grid→visual shift `update_vertices` gives cells (history rows
        // fill the top of the viewport when `view_offset > 0`, pushing
        // live content down); scrollback placements already have it baked
        // in.
        let mut image_draws: Vec<renderer::images::ImageDraw<'_>> = Vec::new();
        if self.config.images_enabled {
            let view_offset = self.active_tab().terminal.view_offset();
            let rows = self.active_tab().terminal.rows;
            let scrollback_draws = self.active_tab()
                .terminal
                .scrollback_placements_in_view(self.active_tab().terminal.rows);
            image_draws.reserve(
                self.active_tab().terminal.live_placements().len() + scrollback_draws.len(),
            );
            // `(viewport_row, placement)` tuples — by the time the pixel
            // math runs, the row index is in viewport coords. Live
            // placements get the same shift `extended_cell` applies to
            // cells (history rows push live content down); scrollback
            // placements arrive pre-shifted from `scrollback_placements_in_view`.
            let live_iter = self.active_tab().terminal.live_placements().iter().map(|p| {
                (
                    Self::live_placement_viewport_row(p.top_row, view_offset, rows),
                    p,
                )
            });
            let scrollback_iter = scrollback_draws.iter().map(|p| (p.top_row, p));
            let now = std::time::Instant::now();
            for (viewport_row, p) in live_iter.chain(scrollback_iter) {
                // peek_at picks the current animation frame for animated
                // images; for static images it returns the same texture
                // as peek().
                let Some(gpu_img) = self.active_tab().image_store.peek_at(p.image, now) else { continue };
                // pixel_offset shifts the draw inside the anchor cell — phase 2
                // Kitty `X=`/`Y=` plumb through here. Whole-cell math stays
                // identical so eviction / scroll-region shifting is unaffected.
                let x_px = WINDOW_PADDING
                    + (p.left_col as f32) * cell_w
                    + p.pixel_offset.0 as f32;
                let y_px = WINDOW_PADDING
                    + decorator_offset
                    + (viewport_row as f32) * line_height
                    + scroll_y
                    + p.pixel_offset.1 as f32;
                let w_px = (p.cols as f32) * cell_w;
                let h_px = (p.rows as f32) * line_height;
                // src_rect (pixels) → UVs (0..1) against this image's known size.
                // Guard against zero w/h on the GpuImage to avoid div-by-zero —
                // shouldn't happen for a successfully uploaded texture, but cheap.
                let uv_rect = p.src_rect.and_then(|(sx, sy, sw, sh)| {
                    let iw = gpu_img.width_px as f32;
                    let ih = gpu_img.height_px as f32;
                    if iw <= 0.0 || ih <= 0.0 { return None; }
                    Some((
                        sx as f32 / iw,
                        sy as f32 / ih,
                        (sx + sw) as f32 / iw,
                        (sy + sh) as f32 / ih,
                    ))
                });
                image_draws.push(renderer::images::ImageDraw {
                    image: gpu_img,
                    x_px,
                    y_px,
                    w_px,
                    h_px,
                    uv_rect,
                });
            }

            // Kitty virtual placements (U+10EEEE cells). Each run is
            // one horizontal stretch of one source-image row,
            // produced by the per-cell scan in
            // `Terminal::kitty_placeholder_runs`. Drawing per-run
            // (instead of one stretched quad over the merged bbox)
            // means a partial overwrite of the placeholder grid —
            // tmux scrolling new output across the top, a tear-down
            // halfway through the image — visibly clips at the
            // surviving cells instead of distorting the image into
            // whatever shrinking rect remained.
            for run in self.active_tab().terminal.kitty_placeholder_runs() {
                let Some(store_id) = self.active_tab().terminal.kitty_image_id_lookup(run.client_id)
                else {
                    continue;
                };
                let Some((total_cols, total_rows)) =
                    self.active_tab().terminal.kitty_image_cell_extent(run.client_id)
                else {
                    // No `c=`/`r=` on the transmission — no honest
                    // UV denominator. Skip rather than guess.
                    continue;
                };
                let Some(gpu_img) = self.active_tab().image_store.peek_at(store_id, now) else { continue };
                // `run.screen_row` is already in the same visual-row
                // frame `update_vertices` uses (the scanner walks
                // `extended_cell(-2..rows+2, ..)`), so plug it
                // straight into the cell row→pixel math. No
                // `view_offset` shift needed.
                let cells_wide = (run.screen_col_end - run.screen_col_start) as f32;
                let x_px = WINDOW_PADDING + (run.screen_col_start as f32) * cell_w;
                let y_px = WINDOW_PADDING
                    + decorator_offset
                    + (run.screen_row as f32) * line_height
                    + scroll_y;
                let w_px = cells_wide * cell_w;
                let h_px = line_height;
                let uv = Self::placeholder_run_uv(
                    run.image_col_start,
                    run.image_col_end,
                    run.image_row,
                    total_cols,
                    total_rows,
                );
                image_draws.push(renderer::images::ImageDraw {
                    image: gpu_img,
                    x_px,
                    y_px,
                    w_px,
                    h_px,
                    uv_rect: Some(uv),
                });
            }
        }
        let has_images = !image_draws.is_empty();

        let grid_pipeline = if self.wireframe {
            self.shared.wireframe_pipeline.as_ref().unwrap_or(&self.shared.render_pipeline)
        } else {
            &self.shared.render_pipeline
        };

        // Helper to issue a draw of part of the cell vertex buffer into
        // `target`, with the given load op. Sharing the body keeps the
        // three paths' grid draws byte-for-byte identical aside from
        // load/store and index range.
        let draw_grid = |encoder: &mut wgpu::CommandEncoder,
                         target: &wgpu::TextureView,
                         load: wgpu::LoadOp<wgpu::Color>,
                         range: std::ops::Range<u32>,
                         label: &str| {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some(label),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target,
                    resolve_target: None,
                    ops: wgpu::Operations { load, store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: None,
                occlusion_query_set: None,
                timestamp_writes: None,
            });
            pass.set_pipeline(grid_pipeline);
            pass.set_bind_group(0, &self.font_bind_group, &[]);
            pass.set_bind_group(1, &self.camera_bind_group, &[]);
            pass.set_bind_group(2, &self.fade_bind_group, &[]);
            pass.set_vertex_buffer(0, self.vertex_buffer.slice(..));
            pass.set_index_buffer(self.index_buffer.slice(..), wgpu::IndexFormat::Uint32);
            if range.start < range.end {
                pass.draw_indexed(range, 0, 0..1);
            }
        };

        if !needs_offscreen {
            // Fast path. When no image is on screen we issue the original
            // single grid pass; when one is, we split into bg + image + fg
            // so glyphs land on top of the image. The one-extra-pass cost
            // is paid only on frames that actually draw images.
            if has_images {
                draw_grid(
                    &mut encoder,
                    &view,
                    wgpu::LoadOp::Clear(clear),
                    bg_index_range.clone(),
                    "scene bg pass (fast+img)",
                );
                self.image_pipeline.render(
                    &mut encoder,
                    &self.shared.gpu.queue,
                    &self.camera_bind_group,
                    &view,
                    wgpu::LoadOp::Load,
                    &image_draws,
                );
                draw_grid(
                    &mut encoder,
                    &view,
                    wgpu::LoadOp::Load,
                    fg_index_range.clone(),
                    "scene fg pass (fast+img)",
                );
            } else {
                draw_grid(
                    &mut encoder,
                    &view,
                    wgpu::LoadOp::Clear(clear),
                    0..self.num_indices,
                    "scene pass",
                );
            }
            // Content overlay, fast path: separate render pass on the
            // swapchain with LoadOp::Load so it darkens what we just
            // drew. Only entered when scanlines are enabled but glow
            // and strips are off.
            if content_overlay_on {
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("scanline overlay pass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Load,
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    occlusion_query_set: None,
                    timestamp_writes: None,
                });
                pass.set_pipeline(&self.shared.glow_pipelines.scanline_overlay_pipeline);
                pass.set_bind_group(0, &self.glow.composite_bg, &[]);
                pass.draw(0..3, 0..1);
            }
        } else if glow_on {
            // Layered path. Pass 1: bg quads → bg scene.
            draw_grid(
                &mut encoder,
                &self.blur.scene.view,
                wgpu::LoadOp::Clear(clear),
                bg_index_range,
                "scene bg pass",
            );
            // Pass 1b: image placements → bg scene (composite over bg
            // cells). Putting images in `blur.scene` means glow and edge
            // blur treat them as scene content; fg glyphs that overlap
            // an image will still composite on top via the next pass.
            if has_images {
                self.image_pipeline.render(
                    &mut encoder,
                    &self.shared.gpu.queue,
                    &self.camera_bind_group,
                    &self.blur.scene.view,
                    wgpu::LoadOp::Load,
                    &image_draws,
                );
            }
            // Pass 2: fg quads → fg scene. Transparent clear so anything
            // the fg layer doesn't touch shows the bg layer through.
            draw_grid(
                &mut encoder,
                &self.scene_fg.view,
                wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                fg_index_range,
                "scene fg pass",
            );
            // Pass 3: glow each layer against its own un-blurred scene
            // so the bright pass extracts crisp colour, not post-blur smear.
            self.glow.run(&mut encoder, &self.shared.glow_pipelines);
            self.glow_fg.run(&mut encoder, &self.shared.glow_pipelines);
            // Strip blur still samples the bg scene — strips live near
            // the window edges where there's rarely text, so a bg-only
            // blur source reads close to the legacy combined-scene blur.
            if needs_strips {
                self.blur.run(&mut encoder, &self.shared.blur_pipelines);
            }

            // Pass 4: composite to swapchain. Order is bg → bg glow →
            // fg → fg glow → strips. Strips stay on top so the toolbar
            // / edge fade reads cleanly over everything.
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("composite pass (layered)"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(clear),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                occlusion_query_set: None,
                timestamp_writes: None,
            });

            // bg scene (opaque blit).
            pass.set_pipeline(&self.shared.blur_pipelines.blit_pipeline);
            pass.set_bind_group(0, self.blur.blit_bind_group(), &[]);
            pass.draw(0..3, 0..1);

            // bg glow (alpha-blended, masked by bg). Mask suppresses the
            // halo over colored cells so the bg's own pixels aren't
            // re-tinted by their bloom; the halo still appears in
            // transparent areas adjacent to colored cells.
            pass.set_pipeline(&self.shared.glow_pipelines.composite_masked_pipeline);
            pass.set_bind_group(0, &self.glow.composite_bg, &[]);
            pass.set_bind_group(1, &self.glow_bg_mask, &[]);
            pass.draw(0..3, 0..1);

            // fg scene (alpha-blended on top of bg + bg glow).
            pass.set_pipeline(&self.shared.blur_pipelines.blit_alpha_pipeline);
            pass.set_bind_group(0, &self.scene_fg_blit_bg, &[]);
            pass.draw(0..3, 0..1);

            // fg glow (alpha-blended, masked by bg). Without the mask
            // the bloom paints over adjacent cells' colored backgrounds
            // and visually shifts them; this keeps the halo only in
            // areas where bg is transparent.
            pass.set_pipeline(&self.shared.glow_pipelines.composite_masked_pipeline);
            pass.set_bind_group(0, &self.glow_fg.composite_bg, &[]);
            pass.set_bind_group(1, &self.glow_fg_mask, &[]);
            pass.draw(0..3, 0..1);

            // Content scanlines: multiply-blend overlay across bg + glow
            // + fg + fg glow. Drawn before strips so the edge fades
            // aren't darkened (they're UI, not content). The masked
            // variant additionally fades the overlay to identity where
            // the bg scene matches the window's primary background
            // colour — scanlines disappear over empty areas.
            if content_overlay_on {
                if effective_skip_primary_bg(&self.config, &palette::get().glow) {
                    // Layered path has both scene textures — mask
                    // samples bg + fg so glyphs on default-bg cells
                    // still get scanlines.
                    pass.set_pipeline(&self.shared.glow_pipelines.scanline_overlay_masked_pipeline);
                    pass.set_bind_group(0, &self.glow.composite_bg, &[]);
                    pass.set_bind_group(1, &self.scanline_overlay_mask, &[]);
                } else {
                    pass.set_pipeline(&self.shared.glow_pipelines.scanline_overlay_pipeline);
                    pass.set_bind_group(0, &self.glow.composite_bg, &[]);
                }
                pass.draw(0..3, 0..1);
            }

            if needs_strips {
                pass.set_pipeline(&self.shared.blur_pipelines.strip_pipeline);
                pass.set_bind_group(0, &self.blur.strip_blur_bg, &[]);
                pass.set_bind_group(1, &self.camera_bind_group, &[]);
                pass.set_bind_group(2, &self.blur.strip_uniform_bg, &[]);
                pass.set_vertex_buffer(0, self.strip_vertex_buffer.slice(..));
                pass.set_index_buffer(
                    self.strip_index_buffer.slice(..),
                    wgpu::IndexFormat::Uint16,
                );
                pass.draw_indexed(0..self.num_strip_indices, 0, 0..1);
            }
        } else {
            // Strip-only path: legacy single-scene render + blur + composite.
            // When images are on screen we split the grid pass into bg + fg
            // so the image quads land between them — same trick as the fast
            // path. Otherwise we keep the original single-pass behaviour.
            if has_images {
                draw_grid(
                    &mut encoder,
                    &self.blur.scene.view,
                    wgpu::LoadOp::Clear(clear),
                    bg_index_range,
                    "scene bg pass (strip+img)",
                );
                self.image_pipeline.render(
                    &mut encoder,
                    &self.shared.gpu.queue,
                    &self.camera_bind_group,
                    &self.blur.scene.view,
                    wgpu::LoadOp::Load,
                    &image_draws,
                );
                draw_grid(
                    &mut encoder,
                    &self.blur.scene.view,
                    wgpu::LoadOp::Load,
                    fg_index_range,
                    "scene fg pass (strip+img)",
                );
            } else {
                draw_grid(
                    &mut encoder,
                    &self.blur.scene.view,
                    wgpu::LoadOp::Clear(clear),
                    0..self.num_indices,
                    "scene pass",
                );
            }
            self.blur.run(&mut encoder, &self.shared.blur_pipelines);

            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("composite pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(clear),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                occlusion_query_set: None,
                timestamp_writes: None,
            });

            pass.set_pipeline(&self.shared.blur_pipelines.blit_pipeline);
            pass.set_bind_group(0, self.blur.blit_bind_group(), &[]);
            pass.draw(0..3, 0..1);

            // Content scanlines, strip-only path: overlay between scene
            // blit and the strip quads. Always unmasked here because
            // the layered fg scene is stale in this path — the masked
            // variant would suppress scanlines based on outdated fg
            // content, producing artifacts. `skip_primary_bg` therefore
            // only takes effect in the layered (glow-on) path.
            if content_overlay_on {
                pass.set_pipeline(&self.shared.glow_pipelines.scanline_overlay_pipeline);
                pass.set_bind_group(0, &self.glow.composite_bg, &[]);
                pass.draw(0..3, 0..1);
            }

            pass.set_pipeline(&self.shared.blur_pipelines.strip_pipeline);
            pass.set_bind_group(0, &self.blur.strip_blur_bg, &[]);
            pass.set_bind_group(1, &self.camera_bind_group, &[]);
            pass.set_bind_group(2, &self.blur.strip_uniform_bg, &[]);
            pass.set_vertex_buffer(0, self.strip_vertex_buffer.slice(..));
            pass.set_index_buffer(
                self.strip_index_buffer.slice(..),
                wgpu::IndexFormat::Uint16,
            );
            pass.draw_indexed(0..self.num_strip_indices, 0, 0..1);
        }

        self.shared.gpu.queue.submit(std::iter::once(encoder.finish()));
        output.present();

        Ok((surface_wait, !needs_offscreen))
    }
}

/// Everything `run()` needs to build the font stack, loaded as owned bytes /
/// strings so it can be produced on a worker thread (a FreeType `Face` is not
/// `Send`, but the raw font data is). The main thread turns this into FreeType
/// faces + the rustybuzz shaper after the GPU has been brought up concurrently.
struct FontData {
    primary_data: Vec<u8>,
    /// Bold / Italic / BoldItalic primary cuts that loaded, as
    /// `(variant, bytes, face_index)`. Missing cuts are simply absent.
    styled: Vec<(font::FaceVariant, Vec<u8>, isize)>,
    /// Fallback faces that loaded, as `(label, family, variant, bytes,
    /// face_index)`. Whether each is actually attached is decided on the main
    /// thread (a styled fallback only attaches when its primary cut built).
    fallbacks: Vec<(&'static str, String, font::FaceVariant, Vec<u8>, isize)>,
}

/// Resolve the primary family and load every font file the terminal needs —
/// primary cut, its bold/italic/bold-italic cuts, and the fallback chain — in
/// parallel. Pure data work (Core Text matching + file reads, all thread-safe
/// and `Send`), so `run()` drives it on a worker thread while the GPU spins up
/// on the main thread. Replaces what used to be ~250ms of sequential loading.
fn load_font_data(config: &Config) -> FontData {
    let mut mono_prop = font_loader::system_fonts::FontPropertyBuilder::new()
        .monospace()
        .build();
    let mut mono_fonts = font_loader::system_fonts::query_specific(&mut mono_prop);
    mono_fonts.dedup();
    let installed = font_loader::system_fonts::query_all();

    // User override wins over the built-in preference list (exact name, then
    // substring); a configured-but-missing family warns and falls through.
    let configured_primary = config.font_family.as_deref().and_then(|want| {
        let hit = installed
            .iter()
            .find(|f| f.as_str() == want)
            .or_else(|| installed.iter().find(|f| f.contains(want)))
            .cloned();
        if hit.is_none() {
            eprintln!(
                "font: configured font_family {:?} not installed, falling back to defaults",
                want,
            );
        }
        hit
    });
    let primary_name = configured_primary.unwrap_or_else(|| {
        ["Iosevka Term", "Iosevka", "Fira Code", "Menlo"]
            .iter()
            .find_map(|want| mono_fonts.iter().find(|f| f.as_str() == *want))
            .or_else(|| mono_fonts.iter().find(|f| f.contains("Iosevka Term")))
            .or_else(|| mono_fonts.iter().find(|f| f.contains("Iosevka")))
            .expect("no monospace primary font found")
            .clone()
    });

    let styled_specs = [
        (font::FaceVariant::Bold, true, false),
        (font::FaceVariant::Italic, false, true),
        (font::FaceVariant::BoldItalic, true, true),
    ];

    // Fallback chain — first installed family in each category wins. Same list
    // and ordering as before; we only build the *job list* here, then load all
    // jobs in parallel below.
    let fallback_categories: &[(&str, &[&str])] = &[
        ("nerd", &[
            "Iosevka Nerd Font",
            "FiraCode Nerd Font",
            "JetBrainsMono Nerd Font",
            "Hack Nerd Font",
            "Symbols Nerd Font",
        ]),
        ("cjk", &[
            "PingFang SC",
            "Hiragino Sans",
            "Noto Sans CJK SC",
            "Noto Sans CJK JP",
            "Sarasa Mono SC",
        ]),
        ("symbols", &[
            "Apple Symbols",
            "Symbola",
            "Noto Sans Symbols 2",
            "Noto Sans Symbols",
        ]),
        ("emoji", &["Noto Emoji"]),
    ];
    let variants_to_fill = [
        (font::FaceVariant::Regular, false, false),
        (font::FaceVariant::Bold, true, false),
        (font::FaceVariant::Italic, false, true),
        (font::FaceVariant::BoldItalic, true, true),
    ];
    let mut fallback_jobs: Vec<(&'static str, String, font::FaceVariant, bool, bool)> = Vec::new();
    for (label, candidates) in fallback_categories {
        if let Some(family) = pick_family(&installed, candidates) {
            for (variant, bold, italic) in variants_to_fill {
                fallback_jobs.push((label, family.clone(), variant, bold, italic));
            }
        }
    }

    // Load primary, styled cuts, and every fallback file concurrently. Each is
    // an independent Core Text match + file read; fanning them across threads
    // turns the longest single load — not their sum — into the critical path.
    // The scope borrows `primary_name`, so `FontData` is assembled only after
    // the scope ends (all handles joined) and the borrow is released.
    let (primary_data, styled, fallbacks) = std::thread::scope(|s| {
        let primary_h =
            s.spawn(|| load_family(&primary_name).expect("failed to load primary font"));
        let styled_hs: Vec<_> = styled_specs
            .iter()
            .map(|&(v, b, i)| {
                let name = &primary_name;
                (v, s.spawn(move || load_family_styled(name, b, i)))
            })
            .collect();
        let fallback_hs: Vec<_> = fallback_jobs
            .iter()
            .map(|job| {
                let (label, v) = (job.0, job.2);
                let (family, b, i) = (&job.1, job.3, job.4);
                (label, family.clone(), v, s.spawn(move || load_family_styled(family, b, i)))
            })
            .collect();

        let primary_data = primary_h.join().expect("primary font loader panicked");
        let styled: Vec<_> = styled_hs
            .into_iter()
            .filter_map(|(v, h)| h.join().expect("styled loader panicked").map(|(d, i)| (v, d, i)))
            .collect();
        let fallbacks: Vec<_> = fallback_hs
            .into_iter()
            .filter_map(|(label, family, v, h)| {
                h.join()
                    .expect("fallback loader panicked")
                    .map(|(d, i)| (label, family, v, d, i))
            })
            .collect();

        (primary_data, styled, fallbacks)
    });

    FontData { primary_data, styled, fallbacks }
}

/// Mint the next process-unique `TabId`. Monotonic; never reused, so a freed
/// tab's id can't collide with a later one (a just-closed tab's reader thread
/// may still deliver one final event — the resolver treats unknown ids as a
/// no-op).
fn next_tab_id() -> app_window::TabId {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    app_window::TabId(NEXT.fetch_add(1, Ordering::Relaxed))
}

/// Create a new tab: open a PTY, fork `program` onto it, spawn the reader
/// thread (tagging every event with the new `TabId`), and build the
/// `TabState`. The grid is sized to `cols`×`rows` — the caller passes the
/// owning window's viewport so the window's vertex buffers match. The reader
/// thread takes the `Pty` by value (it reads + reaps the child); `TabState`
/// keeps copies of `master`+`child` so the tab can be closed cleanly later
/// (`close(master)` + `kill(child, SIGHUP)`).
fn create_tab(
    proxy: &winit::event_loop::EventLoopProxy<app_window::CustomEvent>,
    program: pty::ChildProgram,
    zdotdir: Option<std::path::PathBuf>,
    cols: usize,
    rows: usize,
    image_mem_cap_bytes: usize,
) -> (app_window::TabId, TabState) {
    let fdm = unsafe { posix_openpt(O_RDWR) };
    if fdm < 0 {
        panic!("Error on posix_openpt()");
    }
    let pty = pty::fork_pty(fdm, program, zdotdir).expect("failed to fork pty");
    let tab_id = next_tab_id();
    let master = pty.master;
    let child = pty.child;
    let proxy = proxy.clone();
    std::thread::spawn(move || {
        let code = pty.run(|data| {
            let _ = proxy
                .send_event(app_window::CustomEvent::PtyInput(tab_id, data.to_owned()));
        });
        // `run` returns once the shell has exited and been reaped; tell the
        // loop so it reacts instead of leaving a frozen tab.
        let _ = proxy.send_event(app_window::CustomEvent::PtyExit(tab_id, code));
    });
    let tab = TabState {
        tab_id,
        master,
        child,
        terminal: terminal::Terminal::new(cols, rows, 10000),
        image_store: images::Store::new(image_mem_cap_bytes),
        pending_placements: Vec::new(),
        scroll_y: 0.0,
        alt_scroll_anim: None,
        wheel_pty_accum: 0.0,
        scroll_suppressed: false,
        last_wheel_at: None,
        last_reported_cell: None,
        cursor_anim: None,
        prev_visible: None,
        cursor_ghosts: Vec::new(),
        completions: Vec::new(),
        completions_input: None,
        selected_completion: 0,
        completion_scroll: 0,
        completion_dismissed: false,
        command_history: Vec::new(),
        selection: None,
        selection_mode: SelectionMode::Cell,
        press_cell: None,
        press_pixel: None,
        last_click: None,
        click_count: 0,
        hover_url: None,
    };
    (tab_id, tab)
}

async fn run() {
    env_logger::init();
    // Startup phase timing, printed only when YUTANI_STARTUP_TIMING is set so
    // the perf work stays reproducible without spamming every launch.
    let t_start = std::time::Instant::now();
    let timing = std::env::var_os("YUTANI_STARTUP_TIMING").is_some();
    let lap = |label: &str| {
        if timing {
            eprintln!("[startup] {:>7.1}ms  {label}", t_start.elapsed().as_secs_f64() * 1000.0);
        }
    };
    let event_loop = EventLoopBuilder::<app_window::CustomEvent>::with_user_event()
        .build()
        .unwrap();
    let event_loop_proxy = event_loop.create_proxy();

    // On first run (or after a "Run first-time setup…" re-arm), the PTY child
    // becomes the onboarding console program instead of the shell; it execs the
    // shell in-place when done. Resolve our own path here, in the parent, so the
    // post-fork child doesn't have to.
    let child_program = if needs_onboarding() {
        match std::env::current_exe() {
            Ok(exe) => pty::ChildProgram::Onboard { exe },
            // Can't find ourselves to re-exec — skip onboarding rather than
            // fail to launch. It'll retry next time the marker is still unset.
            Err(_) => pty::ChildProgram::Shell,
        }
    } else {
        pty::ChildProgram::Shell
    };

    // Materialize the zsh shell-integration ZDOTDIR (no-op when opted out or
    // $SHELL isn't zsh) so the forked shell auto-loads it — no manual
    // `source …` in the user's rc required. The PTY itself is forked by
    // `create_tab` once the window/font exist and the grid size is known.
    let zdotdir = shell_integration::prepare_zdotdir();

    // Seed the window title from our own working directory — the shell the
    // PTY just forked inherits it (no chdir in the child), so this matches
    // what the first OSC 7 will report, avoiding a bare "Yutani" flash before
    // the first prompt.
    let initial_cwd = std::env::current_dir()
        .ok()
        .and_then(|p| p.to_str().map(str::to_owned));
    let initial_title = effective_title(None, initial_cwd.as_deref());

    let transparent = false; // needed because of a shadow bug
    let mut window_builder = WindowBuilder::new()
        .with_title(&initial_title)
        .with_titlebar_transparent(true)
        .with_transparent(transparent)
        .with_has_shadow(!transparent)
        .with_fullsize_content_view(true)
        .with_decorations(true)
        .with_blur(transparent);
    // When spawned via Cmd-N the parent forwards its position; cascade off it
    // so the new window steps down-and-right instead of stacking exactly atop.
    if let Some(pos) = cascade_position() {
        window_builder = window_builder.with_position(pos);
    }
    let window = window_builder.build(&event_loop).unwrap();
    lap("after window build");

    // event_loop.set_control_flow(ControlFlow::Poll);

    let config = Config::load();
    lap("after config load");

    // Load all font data on a worker thread while the GPU is brought up on
    // this (main) thread. The two are independent until WindowState::new needs both,
    // so overlapping them hides whichever finishes first. Font work must hand
    // back owned bytes (FreeType faces aren't Send); GPU/surface creation must
    // stay on the main thread (Cocoa isn't thread-safe), hence this split.
    let config_for_fonts = config.clone();
    let font_handle = std::thread::spawn(move || load_font_data(&config_for_fonts));

    let (gpu, surface) = gpu::Gpu::new(&window).await;
    lap("after Gpu::new (concurrent with font load)");

    let fd = font_handle.join().expect("font loader thread panicked");
    lap("after font data loaded (joined)");

    // Install the color scheme before constructing WindowState so style.rs and the
    // renderer see the right palette on their first read. Missing file is a
    // soft failure: warn and keep defaults so a typo in the config name
    // doesn't take the terminal down. With `auto_theme` on, pick the slot for
    // the OS's current appearance up front so we open in the right scheme.
    // Touches the window, so it stays on the main thread (after the join).
    let initial_dark = window.theme() == Some(winit::window::Theme::Dark);
    install_color_scheme(config.active_scheme(initial_dark));
    // Match the NSAppearance to the palette so the title-bar text the OS
    // draws over our transparent chrome reads against the actual bg —
    // otherwise dark schemes render black "Yutani" text on a dark fill.
    window.set_theme(Some(theme_for_bg(palette::get().background)));
    set_native_window_bg(&window, palette::get().background);

    let pt_size = config.font_size;
    let dpi = (window.scale_factor() * 96.0) as u32;

    // Turn the loaded bytes into FreeType faces + the rustybuzz shaper. This is
    // the not-Send tail that has to run here. Regular comes from the primary
    // lookup (face 0); the styled cuts carry their own TTC face index.
    let mut shaper = shaper::Shaper::new();
    shaper.set_variant(font::FaceVariant::Regular, &fd.primary_data, 0);
    let mut font = font::Font::new(fd.primary_data);
    font.set_char_size(pt_size, dpi);

    for (variant, data, face_index) in fd.styled {
        shaper.set_variant(variant, &data, face_index as u32);
        font.set_variant(variant, data, face_index, pt_size, dpi);
    }

    // Pre-shape every candidate ligature sequence for each installed variant.
    for variant in font::FaceVariant::ALL {
        shaper.precompute(variant);
    }

    // Attach the fallback faces. A styled fallback only attaches when its
    // primary cut actually built (same guard as before — otherwise the chain
    // is dead weight and Atlas::lookup tumbles to Regular anyway).
    for (_label, _family, variant, data, face_index) in fd.fallbacks {
        if variant != font::FaceVariant::Regular
            && font.variants[variant as usize].face.is_none()
        {
            continue;
        }
        font.add_fallback(variant, data, face_index, pt_size, dpi);
    }

    lap("after font faces + shaper built");

    // Build the once-per-process shared resources (device/queue/font/shaper +
    // pipelines), then the initial tab + window against them. `shared` is kept
    // (cloned per window) so Cmd-N can spawn further windows in-process.
    let shared = Rc::new(AppShared::new(gpu, surface.config.format, font, shaper));
    lap("after AppShared::new");

    // Size the initial tab's grid to this window's viewport so the vertex
    // buffers create_window allocates match the terminal dimensions.
    let (cols, rows) = {
        let (cell_w, line_h) = shared.with_font(|f| {
            let m = f.face().size_metrics().unwrap();
            (f.cell_width(), ((m.ascender - m.descender) >> 6) as usize)
        });
        let vp = WindowState::get_viewport_size(
            surface.config.width as f32,
            surface.config.height as f32,
            cell_w,
            line_h,
        );
        (vp.char_width, vp.char_height)
    };
    let (tab_id, initial_tab) = create_tab(
        &event_loop_proxy,
        child_program,
        zdotdir,
        cols,
        rows,
        config.images_memory_cap_mb * 1024 * 1024,
    );
    lap("after create_tab (fork)");
    let mut state =
        WindowState::create_window(shared.clone(), window, surface, config, dpi, initial_tab);
    lap("after create_window (GPU/atlas/pipelines)");
    state.notify_pty_size(state.active_tab().terminal.cols, state.active_tab().terminal.rows);
    // Size the chrome band to the real native title bar now that the window
    // exists; the field was seeded with the renderer's reserve in WindowState::new.
    state.refresh_chrome_band();
    state.window.set_cursor_icon(winit::window::CursorIcon::Text);
    state.sync_theme_colors();
    state.tabs[state.active]
        .terminal
        .set_keep_placements_in_scrollback(state.config.images_in_scrollback);
    state.sync_terminal_cell_size();
    // Developer smoke-test hook: drop a placement at the top-left on
    // startup if `YUTANI_TEST_IMAGE` is set. Goes through the same path
    // as the Cmd-Shift-I keybind so both are exercised together.
    if let Ok(path) = std::env::var("YUTANI_TEST_IMAGE") {
        state.load_image_at_cell(&path, 0, 0, "YUTANI_TEST_IMAGE");
    }
    state.invalidate();

    let mut theme = state.window.theme().unwrap_or(winit::window::Theme::Light);

    // Program-set window title (OSC 0/2). When `Some`, it wins over the
    // cwd-derived title; cleared back to `None` by an empty OSC 0/2 payload,
    // at which point we fall back to the cwd.
    let mut manual_title: Option<String> = None;
    let mut first_frame_done = false;

    // The window registry. One process owns every window; events are routed
    // here rather than against a single `state` binding. `tab_to_window`
    // resolves a `TabId` (carried on every PtyInput/PtyExit) to its window.
    // First cut: exactly one window with one tab.
    let initial_window_id = state.window.id();
    let mut windows: std::collections::HashMap<winit::window::WindowId, WindowState> =
        std::collections::HashMap::new();
    windows.insert(initial_window_id, state);
    let mut tab_to_window: std::collections::HashMap<app_window::TabId, winit::window::WindowId> =
        std::collections::HashMap::new();
    tab_to_window.insert(tab_id, initial_window_id);

    let _ = event_loop.run(move |event, elwt| {
        match event {
            Event::UserEvent(n) => match n {
                app_window::CustomEvent::PtyInput(ev_tab, z) => {
                    // Resolve the tab's window. A just-closed tab can still
                    // deliver one last event — treat an unknown id as a no-op.
                    let Some(state) = tab_to_window
                        .get(&ev_tab)
                        .and_then(|wid| windows.get_mut(wid))
                    else {
                        return;
                    };
                    let bytes = z.len();
                    let t0 = std::time::Instant::now();
                    state.feed_terminal(&z);
                    let reply = state.active_tab_mut().terminal.take_response();
                    if !reply.is_empty() {
                        state.write_pty(&reply);
                    }
                    // Recompute the window title when either the program-set
                    // title (OSC 0/2) or the shell's cwd (OSC 7) changed. A
                    // manual title wins; otherwise we show the cwd ($HOME
                    // collapsed to `~`). Both `take_*` calls must run to clear
                    // their dirty flags even when the title doesn't change.
                    let title_changed = state.active_tab_mut().terminal.take_title_update().map(|t| {
                        manual_title = t;
                    });
                    let cwd_changed = state.active_tab_mut().terminal.take_cwd_update();
                    if title_changed.is_some() || cwd_changed.is_some() {
                        state.window.set_title(&effective_title(
                            manual_title.as_deref(),
                            state.active_tab().terminal.cwd(),
                        ));
                    }
                    // The shell may have reported its history file (OSC 2124):
                    // read + parse it and merge past commands into the in-memory
                    // history (most-recent-first), behind any already-captured
                    // session commands so those stay at the front.
                    if let Some(path) = state.active_tab_mut().terminal.take_histfile_update() {
                        if let Ok(contents) = std::fs::read_to_string(&path) {
                            let parsed = completion::parse_zsh_history(&contents);
                            for cmd in parsed.iter().rev() {
                                if !state.active_tab().command_history.iter().any(|c| c == cmd) {
                                    state.active_tab_mut().command_history.push(cmd.clone());
                                }
                            }
                            state.active_tab_mut().command_history.truncate(COMMAND_HISTORY_CAP);
                        }
                    }
                    // A command just submitted at the prompt (OSC 133 C): fold
                    // it into the front of the history (deduped).
                    if let Some(cmd) = state.active_tab_mut().terminal.take_submitted_command() {
                        dedup_prepend(&mut state.active_tab_mut().command_history, cmd);
                    }
                    // First-run onboarding (running as the PTY child) may have
                    // emitted OSC 2125 live-preview requests in this chunk —
                    // apply each to the running renderer.
                    for req in state.active_tab_mut().terminal.take_preview_requests() {
                        state.apply_preview(req);
                    }
                    // The chunk may have carried an OSC 2122 input report;
                    // refresh the completion popup's cached suggestions (only
                    // recomputes — and only touches disk — when the input
                    // actually changed). `invalidate()` below redraws.
                    state.recompute_completions();
                    state.perf.note_pty(bytes, t0.elapsed());
                    state.invalidate();
                    // New / removed cells may have changed which URL (if any)
                    // sits under the pointer.
                    state.update_hover_url();
                }
                app_window::CustomEvent::PtyExit(ev_tab, code) => {
                    let Some(&wid) = tab_to_window.get(&ev_tab) else { return };
                    let close = match windows.get(&wid).map(|s| s.config.shell_exit_mode) {
                        Some(ShellExitMode::Always) => true,
                        Some(ShellExitMode::Never) => false,
                        Some(ShellExitMode::OnSuccess) => code == 0,
                        None => return,
                    };
                    if close {
                        // Drop this window and free every tab it owned from the
                        // resolver. Quit once the last window is gone. (With one
                        // window this is the old `exit()`; the registry
                        // generalizes it.)
                        if let Some(state) = windows.remove(&wid) {
                            for t in &state.tabs {
                                tab_to_window.remove(&t.tab_id);
                            }
                        }
                        if windows.is_empty() {
                            elwt.exit();
                        }
                    } else if let Some(state) = windows.get_mut(&wid) {
                        // Keep the window so the user can read the final
                        // output / a crash's exit code, then dismiss it
                        // themselves. The PTY master is closed, so typed
                        // input now goes nowhere — selection and scrollback
                        // still work. Render a dim status line via the normal
                        // ANSI path.
                        state.feed_terminal(&format!(
                            "\r\n\x1b[2m[Process completed — exit {code}]\x1b[0m\r\n"
                        ));
                        state.invalidate();
                        state.window.request_redraw();
                    }
                }
            },
            Event::WindowEvent { window_id, event } => {
                let Some(state) = windows.get_mut(&window_id) else { return };
                let consumed = state.input(&event, elwt);
                // The palette's Set/Clear title actions set the title through
                // the same terminal path OSC 0/2 uses, but a keystroke isn't
                // followed by PtyInput, so poll the title update here too.
                // Mirrors the OSC-driven poll in the PtyInput arm above.
                if let Some(t) = state.active_tab_mut().terminal.take_title_update() {
                    manual_title = t;
                    state.window.set_title(&effective_title(
                        manual_title.as_deref(),
                        state.active_tab().terminal.cwd(),
                    ));
                }
                if !consumed {
                    match event {
                        WindowEvent::ThemeChanged(new_theme) => {
                            theme = new_theme;
                            // Following the system appearance? Swap to the
                            // scheme slot for the new mode. Otherwise just keep
                            // the OSC color reports in sync as before — the
                            // active scheme doesn't track the OS.
                            if state.config.auto_theme {
                                state.apply_active_scheme();
                            } else {
                                state.sync_theme_colors();
                                state.invalidate();
                            }
                        }
                        WindowEvent::CloseRequested => {
                            elwt.exit();
                        }
                        WindowEvent::Resized(size) => {
                            state.resize(size);
                            state.window.request_redraw();
                        }
                        WindowEvent::ScaleFactorChanged {
                            scale_factor: _scale_factor,
                            ..
                        } => {
                            state.refresh_chrome_band();
                            state.window.request_redraw();
                        }
                        WindowEvent::RedrawRequested => {
                            state.update();
                            state.prepare_frame();
                            let t0 = std::time::Instant::now();
                            let result = state.render(clear_color(theme));
                            let render_dur = t0.elapsed();
                            if !first_frame_done {
                                first_frame_done = true;
                                if timing {
                                    eprintln!("[startup] {:>7.1}ms  FIRST FRAME presented", t_start.elapsed().as_secs_f64() * 1000.0);
                                }
                            }
                            match result {
                                Ok((surface_wait, fast)) => {
                                    state.perf.note_render(render_dur, surface_wait, fast);
                                }
                                Err(wgpu::SurfaceError::Lost) => state.resize(state.surface.size),
                                Err(wgpu::SurfaceError::OutOfMemory) => elwt.exit(),
                                Err(e) => eprintln!("{:?}", e),
                            }
                        }
                        _ => (),
                    }
                }
            }
            Event::AboutToWait => {
                // Each window animates independently; collect the earliest
                // wake-up across all of them and arm the loop for that.
                let mut next_wake: Option<std::time::Instant> = None;
                for state in windows.values_mut() {
                    if state.maybe_blink_tick() {
                        state.invalidate();
                    }
                    // Edge-fade and cursor-position eases: keep ticking frames
                    // as long as either is still chasing its target.
                    let animating = state.is_top_fade_animating()
                        || state.is_cursor_animating()
                        || state.is_alt_scroll_animating();
                    if animating {
                        state.invalidate();
                    }
                    state.perf.maybe_flush();
                    let next_anim = if animating {
                        Some(std::time::Instant::now() + ANIM_FRAME)
                    } else {
                        None
                    };
                    // Image-animation deadline (Kitty `a=a` playback). The
                    // store returns `None` if no animated image is
                    // currently advancing; otherwise it returns the
                    // earliest moment a frame swap is due.
                    let next_image_anim = state
                        .active_tab()
                        .image_store
                        .next_frame_deadline(std::time::Instant::now());
                    if next_image_anim.is_some() {
                        state.invalidate();
                    }
                    let this = [
                        state.next_blink_wake(),
                        next_anim,
                        next_image_anim,
                        state.perf.next_wake(),
                    ]
                    .into_iter()
                    .flatten()
                    .min();
                    next_wake = match (next_wake, this) {
                        (Some(a), Some(b)) => Some(a.min(b)),
                        (a, b) => a.or(b),
                    };
                }
                match next_wake {
                    Some(t) => elwt.set_control_flow(
                        winit::event_loop::ControlFlow::WaitUntil(t),
                    ),
                    None => elwt.set_control_flow(winit::event_loop::ControlFlow::Wait),
                }
            }
            _ => (),
        }
    });
}

// Pick the first installed family whose name contains one of the candidate
// substrings, in candidate order. Substring matching is forgiving across
// platform-specific naming variants (e.g. "FiraCode" vs "Fira Code").
fn pick_family(installed: &[String], candidates: &[&str]) -> Option<String> {
    for cand in candidates {
        if let Some(found) = installed.iter().find(|f| f.contains(cand)) {
            return Some(found.clone());
        }
    }
    None
}

fn load_family(family: &str) -> Option<Vec<u8>> {
    let prop = font_loader::system_fonts::FontPropertyBuilder::new()
        .family(family)
        .build();
    font_loader::system_fonts::get(&prop).map(|(data, _)| data)
}

// Variant-aware load. macOS's Core Text matcher silently substitutes the
// regular cut when no bold/italic is installed; `get_strict` rejects that
// substitution by re-checking the matched descriptor's actual traits. Other
// platforms fall back to the trait-tagged builder + plain `get`, which is
// best-effort.
/// Load the bytes of a styled face plus the face index inside its
/// (possibly TTC-packed) file. The index is what the caller must hand
/// to `freetype`'s `new_memory_face` / `harfbuzz_rs::Face::from_bytes`
/// to actually open the right face; passing 0 always lands on the
/// first packed face (usually regular), which is what made italic and
/// bold-italic silently render as regular in earlier revisions.
///
/// On macOS, `get_strict` already verifies against FreeType style
/// flags and returns the correct (file, face_index) tuple. On other
/// platforms we don't have an equivalent strict matcher, so we use
/// `find_face_index` as a best-effort second pass over whatever
/// Core/Fontconfig hand us.
#[cfg(target_os = "macos")]
fn load_family_styled(family: &str, bold: bool, italic: bool) -> Option<(Vec<u8>, isize)> {
    font_loader::system_fonts::get_strict(family, bold, italic)
        .map(|(data, idx)| (data, idx as isize))
}

#[cfg(not(target_os = "macos"))]
fn load_family_styled(family: &str, bold: bool, italic: bool) -> Option<(Vec<u8>, isize)> {
    let mut b = font_loader::system_fonts::FontPropertyBuilder::new().family(family);
    if bold {
        b = b.bold();
    }
    if italic {
        b = b.italic();
    }
    font_loader::system_fonts::get(&b.build()).map(|(data, _)| {
        let idx = font::find_face_index(&data, font::FaceVariant::from_flags(bold, italic));
        (data, idx)
    })
}

/// Serialize a linear-space RGBA back to a `0xRRGGBB` literal so the
/// scanline-colour config round-trips cleanly. Reuses palette's sRGB
/// conversion so the byte we emit matches the byte the user typed.
fn format_hex_rgb(c: [f32; 4]) -> String {
    let r = palette::linear_to_srgb_u8(c[0]);
    let g = palette::linear_to_srgb_u8(c[1]);
    let b = palette::linear_to_srgb_u8(c[2]);
    format!("0x{:02x}{:02x}{:02x}", r, g, b)
}

/// Pick a window NSAppearance to match a background color. Title-bar text is
/// drawn by the OS using that appearance, so a dark scheme must report Dark
/// or "Yutani" comes out black on near-black.
/// Paint the native `NSWindow` background to match the terminal's bg color.
/// The window is opaque, so during AppKit-driven frame changes — most visibly
/// the title-bar double-click zoom animation — any area exposed before our
/// Window title for an OSC 7 working directory: just the path, with `$HOME`
/// collapsed to `~`. An empty/`/` path falls back to the bare app name.
fn title_for_cwd(cwd: &str) -> String {
    let display = if let Some(home) = std::env::var_os("HOME") {
        let home = home.to_string_lossy();
        if cwd == home {
            "~".to_string()
        } else if let Some(rest) = cwd.strip_prefix(&format!("{home}/")) {
            format!("~/{rest}")
        } else {
            cwd.to_string()
        }
    } else {
        cwd.to_string()
    };
    if display.is_empty() {
        "Yutani".to_string()
    } else {
        display
    }
}

/// Resolve the effective window title: a program-set title (OSC 0/2) wins;
/// otherwise fall back to the cwd-derived title, then the bare app name.
fn effective_title(manual: Option<&str>, cwd: Option<&str>) -> String {
    match manual {
        Some(t) => t.to_string(),
        None => cwd.map(title_for_cwd).unwrap_or_else(|| "Yutani".to_string()),
    }
}

/// Upper bound on retained command-history entries, to keep memory bounded for
/// long-running shells with huge `$HISTFILE`s.
const COMMAND_HISTORY_CAP: usize = 10_000;

/// Insert `cmd` at the front of `history` (most-recent-first), removing any
/// existing equal entry first so it stays deduped, then cap the length.
fn dedup_prepend(history: &mut Vec<String>, cmd: String) {
    history.retain(|c| c != &cmd);
    history.insert(0, cmd);
    history.truncate(COMMAND_HISTORY_CAP);
}

/// Metal layer redraws is filled with the window's `backgroundColor`. Left
/// unset that's the default system window color, which flashes against the
/// real terminal background. `bg` is stored linear (the surface is sRGB), so
/// re-encode each channel to sRGB for `NSColor`, which expects sRGB components.
#[cfg(target_os = "macos")]
fn set_native_window_bg(window: &Window, bg: [f32; 4]) {
    use objc::{class, msg_send, runtime::Object, sel, sel_impl};
    use raw_window_handle::{HasRawWindowHandle, RawWindowHandle};

    let RawWindowHandle::AppKit(handle) = window.raw_window_handle() else {
        return;
    };
    let chan = |c: f32| palette::linear_to_srgb_u8(c) as f64 / 255.0;
    unsafe {
        let ns_view = handle.ns_view as *mut Object;
        let ns_window: *mut Object = msg_send![ns_view, window];
        if ns_window.is_null() {
            return;
        }
        let color: *mut Object = msg_send![class!(NSColor),
            colorWithSRGBRed: chan(bg[0])
            green: chan(bg[1])
            blue: chan(bg[2])
            alpha: bg[3] as f64];
        let _: () = msg_send![ns_window, setBackgroundColor: color];
    }
}

#[cfg(not(target_os = "macos"))]
fn set_native_window_bg(_window: &Window, _bg: [f32; 4]) {}

/// Force the system arrow cursor onto `NSCursor` immediately.
///
/// winit applies `set_cursor_icon` lazily: it stores the cursor and lets the
/// content view's `cursorUpdate:` push it the next time AppKit decides to.
/// AppKit does *not* fire `cursorUpdate:` while the pointer is over the native
/// title-bar overlay (which sits above our `fullsize_content_view` content
/// view), so the I-beam last applied down in the grid stays frozen on screen
/// up there — `set_cursor_icon(Default)` alone has no visible effect. Pushing
/// the arrow straight onto `[NSCursor set]` sidesteps `cursorUpdate:` and lands
/// the change now. Called on every move within the chrome band, so even if a
/// later `cursorUpdate:` re-applied something, the next move re-asserts it.
#[cfg(target_os = "macos")]
fn force_native_arrow_cursor(window: &Window) {
    use objc::{class, msg_send, runtime::Object, sel, sel_impl};
    use raw_window_handle::{HasRawWindowHandle, RawWindowHandle};

    let RawWindowHandle::AppKit(_handle) = window.raw_window_handle() else {
        return;
    };
    unsafe {
        let cursor: *mut Object = msg_send![class!(NSCursor), arrowCursor];
        if cursor.is_null() {
            return;
        }
        let _: () = msg_send![cursor, set];
    }
}

#[cfg(not(target_os = "macos"))]
fn force_native_arrow_cursor(_window: &Window) {}

/// Margin (physical px) added below the native title bar when sizing the chrome
/// band. macOS stops delivering pointer-moved events the instant the pointer
/// crosses into the title bar, so the lowest move we ever see sits just *below*
/// the bar, in the top grid row. That boundary event is our only chance to flip
/// the cursor to the arrow (which then sticks as the pointer continues up into
/// the event-dead bar). The margin pulls the band down far enough to include
/// it. Kept tiny so it barely reaches into real content.
const CHROME_BAND_MARGIN_PX: f64 = 4.0;

/// Height of the native `NSWindow` title bar in *physical* pixels, or `None` if
/// the handle isn't AppKit. This is the region macOS owns: it drives window
/// drag / zoom / traffic lights and swallows our pointer-moved events. Unlike
/// the renderer's fixed `WINDOW_PADDING + DECORATOR_HEIGHT` reserve (physical,
/// DPI-independent), the title bar is a fixed number of *points*, so on a
/// Retina display it's physically taller than that reserve — which is why a
/// band sized to the reserve never reached the bar and left the grid's I-beam
/// frozen over it. `contentLayoutRect` excludes the title bar even under
/// `fullsize_content_view`, so `frame.height - contentLayoutRect.height` is the
/// bar height in points; scale to physical.
#[cfg(target_os = "macos")]
fn native_titlebar_height_physical(window: &Window) -> Option<f64> {
    use objc::{msg_send, runtime::Object, sel, sel_impl};
    use raw_window_handle::{HasRawWindowHandle, RawWindowHandle};

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct NSPoint {
        x: f64,
        y: f64,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct NSSize {
        width: f64,
        height: f64,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct NSRect {
        origin: NSPoint,
        size: NSSize,
    }

    let RawWindowHandle::AppKit(handle) = window.raw_window_handle() else {
        return None;
    };
    unsafe {
        let ns_view = handle.ns_view as *mut Object;
        let ns_window: *mut Object = msg_send![ns_view, window];
        if ns_window.is_null() {
            return None;
        }
        let frame: NSRect = msg_send![ns_window, frame];
        let content: NSRect = msg_send![ns_window, contentLayoutRect];
        let scale: f64 = msg_send![ns_window, backingScaleFactor];
        let titlebar_pts = frame.size.height - content.size.height;
        if titlebar_pts <= 0.0 || scale <= 0.0 {
            return None;
        }
        Some(titlebar_pts * scale)
    }
}

#[cfg(not(target_os = "macos"))]
fn native_titlebar_height_physical(_window: &Window) -> Option<f64> {
    None
}

fn theme_for_bg(bg: [f32; 4]) -> winit::window::Theme {
    // Rec. 709 luma in linear-light. <0.18 is roughly perceptual midgray
    // (sRGB 0.5). Below that, dark chrome reads better.
    let luma = 0.2126 * bg[0] + 0.7152 * bg[1] + 0.0722 * bg[2];
    if luma < 0.18 {
        winit::window::Theme::Dark
    } else {
        winit::window::Theme::Light
    }
}

/// Window-relative `py` (physical pixels) falls inside the title bar / toolbar
/// chrome band of height `band_px`. Free function so the boundary is
/// unit-testable without standing up a full `WindowState`; `WindowState::in_top_toolbar`
/// delegates here, passing the live `chrome_band_px`. See `in_top_toolbar` for
/// why the band tracks the native title-bar height rather than the
/// scroll-animated decorator offset.
fn py_in_top_toolbar(py: f64, band_px: f64) -> bool {
    py < band_px
}

/// Choose the chrome band height from the live native title-bar height (if
/// queryable) and the renderer's fixed `reserve`. Extracted from
/// `WindowState::refresh_chrome_band` so the selection arithmetic is unit-testable
/// without a real `Window`: add `CHROME_BAND_MARGIN_PX` to the native height,
/// but never go below the reserve (and fall back to the reserve when the query
/// failed). See `refresh_chrome_band` for the rationale.
fn chrome_band_from(native: Option<f64>, reserve: f64) -> f64 {
    native
        .map(|h| h + CHROME_BAND_MARGIN_PX)
        .filter(|band| *band >= reserve)
        .unwrap_or(reserve)
}

fn clear_color(_theme: winit::window::Theme) -> wgpu::Color {
    let bg = palette::get().background;
    wgpu::Color {
        r: bg[0] as f64,
        g: bg[1] as f64,
        b: bg[2] as f64,
        a: bg[3] as f64,
    }
}

fn main() {
    // First-run onboarding runs as the PTY child (see `run`), re-invoking this
    // same binary with `--onboard`. In that mode we are a thin console program
    // talking to our host terminal over stdin/stdout, not a GUI — so branch
    // before any window / GPU setup. `onboard::run` never returns: it execs the
    // user's shell in-place when done, so the same PTY flows straight into the
    // shell with no second window.
    if std::env::args().skip(1).any(|a| a == "--onboard") {
        onboard::run();
    }
    pollster::block_on(run());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    const TOL: f32 = 1e-4;

    fn approx_eq(a: f32, b: f32) -> bool {
        (a - b).abs() < TOL
    }

    fn approx_pair(a: (f32, f32), b: (f32, f32)) -> bool {
        approx_eq(a.0, b.0) && approx_eq(a.1, b.1)
    }

    #[test]
    fn next_tab_id_is_strictly_increasing_and_distinct() {
        use std::collections::HashSet;
        // The minter must hand out unique ids; tabs are routed by TabId, so a
        // repeat would misroute a PTY's events to the wrong tab.
        let ids: Vec<_> = (0..8).map(|_| next_tab_id()).collect();

        // All distinct.
        let unique: HashSet<_> = ids.iter().copied().collect();
        assert_eq!(unique.len(), ids.len(), "next_tab_id() returned a duplicate");

        // Strictly increasing across successive calls.
        for pair in ids.windows(2) {
            assert!(
                pair[1].0 > pair[0].0,
                "next_tab_id() not strictly increasing: {:?} then {:?}",
                pair[0],
                pair[1]
            );
        }
    }

    #[test]
    fn title_for_cwd_shows_bare_path_without_app_prefix() {
        // A non-$HOME absolute path is shown verbatim, no "Yutani — " prefix.
        assert_eq!(title_for_cwd("/var/log"), "/var/log");
        // Empty path falls back to the bare app name.
        assert_eq!(title_for_cwd(""), "Yutani");
    }

    #[test]
    fn effective_title_prefers_manual_over_cwd() {
        // A program-set title wins regardless of cwd.
        assert_eq!(
            effective_title(Some("vim"), Some("/var/log")),
            "vim".to_string()
        );
        // No manual title: fall back to the cwd-derived title.
        assert_eq!(
            effective_title(None, Some("/var/log")),
            "/var/log".to_string()
        );
        // Neither: bare app name.
        assert_eq!(effective_title(None, None), "Yutani".to_string());
    }

    #[test]
    fn effective_title_manual_wins_even_without_a_cwd() {
        // A program-set title is used regardless of whether the shell has yet
        // reported a cwd via OSC 7.
        assert_eq!(effective_title(Some("htop"), None), "htop".to_string());
    }

    #[test]
    fn effective_title_treats_a_present_empty_manual_as_set() {
        // `Some("")` reaches `effective_title` only via the inner Option of
        // `take_title_update`, but `set_window_title` maps an empty payload to
        // `None` upstream, so the wiring passes `None` there. Pin the pure
        // function's own contract: a present manual string (here empty) wins
        // over the cwd and yields itself verbatim.
        assert_eq!(effective_title(Some(""), Some("/var/log")), "".to_string());
    }

    #[test]
    fn title_for_cwd_passes_through_non_home_absolute_paths() {
        // Deep paths outside $HOME are shown verbatim (no truncation, no
        // app-name prefix). Uses a path that cannot be a $HOME prefix on any
        // realistic machine, so it doesn't depend on the ambient $HOME value.
        assert_eq!(
            title_for_cwd("/zzz-not-home/deep/nested/dir"),
            "/zzz-not-home/deep/nested/dir"
        );
    }

    #[test]
    fn grid_buffer_byte_sizes_uses_full_phantom_row_slack() {
        // Pin the exact byte counts so a future "let me halve the
        // slack to save memory" tweak surfaces as a unit test failure
        // rather than a hard-to-reproduce wgpu validation panic at
        // scroll time. The buffer must cover `update_vertices`'s walk
        // over `r_lo..r_hi = -2..rows+2` — two phantom strips top +
        // two bottom, plus the cursor and edge-fade extras.
        let cols = 98;
        let rows = 35;
        let (vbuf, ibuf) = grid_buffer_byte_sizes(cols, rows);
        let extra_quads = 4 * cols + 5;
        let area = cols * rows;
        let quads = 2 * area + 2 * extra_quads;
        let v = std::mem::size_of::<renderer::vertex::Vertex>();
        assert_eq!(vbuf, quads * v * 4);
        assert_eq!(ibuf, quads * std::mem::size_of::<u32>() * 6);
    }

    #[test]
    fn grid_index_buffer_addresses_beyond_u16() {
        // Regression: the grid's index buffer was `Uint16`, but
        // `update_vertices` emits 8 vertices per cell (bg + fg quad,
        // 4 verts each) into one shared vertex vector. Once a frame
        // crosses 65_536 vertices — only ~8_192 cells — `verts.len()
        // as u16` wrapped and the wrapped indices referenced the
        // wrong vertices, so cells rendered as garbage or vanished
        // (large windows, fullscreen, tmux splits). The index buffer
        // must be wide enough to address every vertex a realistic
        // grid can emit, so its element type has to be u32, not u16.
        let cols = 240usize;
        let rows = 80usize; // a routine fullscreen grid
        let verts_per_cell = 8; // bg quad + fg quad, 4 verts each
        let max_vertex_index = cols * rows * verts_per_cell;
        assert!(
            max_vertex_index > u16::MAX as usize,
            "this grid emits {max_vertex_index} vertices, which fits in u16 — \
             pick a bigger grid so the test actually guards the overflow",
        );

        // The buffer sizing must reserve 4 bytes per index (u32), not 2.
        let (_, ibuf) = grid_buffer_byte_sizes(cols, rows);
        let quads = 2 * (cols * rows) + 2 * (4 * cols + 5);
        assert_eq!(ibuf, quads * 4 * 6, "index buffer must be sized for u32 indices");
    }

    #[test]
    fn grid_buffer_byte_sizes_covers_full_update_vertices_walk() {
        // Symbolic upper bound on what `update_vertices` can push:
        // the row loop covers `rows + 4` rows (two phantom strips on
        // each side) × `cols` cells × 2 quads (bg + glyph) per cell,
        // plus a cursor quad and two edge-fade quads. The vertex
        // buffer must fit at least this many vertices, otherwise
        // `queue.write_buffer` overruns at scroll time.
        for (cols, rows) in [(80, 24), (98, 35), (200, 60), (32, 8)] {
            let (vbuf, _) = grid_buffer_byte_sizes(cols, rows);
            let worst_case_quads = (rows + 4) * cols * 2 + 3;
            let worst_case_bytes =
                worst_case_quads * std::mem::size_of::<renderer::vertex::Vertex>() * 4;
            assert!(
                vbuf >= worst_case_bytes,
                "grid_buffer_byte_sizes({cols}, {rows}) = {vbuf} bytes; \
                 needs at least {worst_case_bytes} to cover the worst-case \
                 walk through `update_vertices`",
            );
        }
    }

    #[test]
    fn get_viewport_size_floors_rows_at_min_grid_rows() {
        // A window dragged shorter than the prompt would otherwise yield a
        // 1–2 row grid, which spills the prompt into scrollback on resize.
        // The row count must never drop below MIN_GRID_ROWS no matter how
        // short the window. cell 8px wide, 18px line height.
        let tiny = WindowState::get_viewport_size(800.0, 0.0, 8, 18);
        assert_eq!(tiny.char_height, MIN_GRID_ROWS);
        let short = WindowState::get_viewport_size(800.0, 60.0, 8, 18);
        assert_eq!(short.char_height, MIN_GRID_ROWS);
        // A normally-sized window is unaffected — the floor doesn't clamp it.
        let normal = WindowState::get_viewport_size(800.0, 600.0, 8, 18);
        assert!(normal.char_height > MIN_GRID_ROWS);
    }

    #[test]
    fn theme_for_bg_picks_dark_on_dark_palette() {
        // near-black bg (linear) → Dark so the OS draws light title text.
        let bg = [0.02, 0.02, 0.02, 1.0];
        assert_eq!(theme_for_bg(bg), winit::window::Theme::Dark);
    }

    #[test]
    fn theme_for_bg_picks_light_on_light_palette() {
        // white default bg → Light, the original behavior.
        let bg = [1.0, 1.0, 1.0, 1.0];
        assert_eq!(theme_for_bg(bg), winit::window::Theme::Light);
    }

    #[test]
    fn theme_for_bg_treats_dark_blue_as_dark() {
        // Solarized-dark-ish background: not pure black but well below
        // perceptual midgray — must still trigger the dark chrome.
        let bg = [0.0, 0.05, 0.07, 1.0];
        assert_eq!(theme_for_bg(bg), winit::window::Theme::Dark);
    }

    #[test]
    fn py_in_top_toolbar_true_inside_band() {
        // The very top edge and a point comfortably within an example 56px
        // (Retina) chrome band both belong to the OS chrome.
        let band = 56.0;
        assert!(py_in_top_toolbar(0.0, band));
        assert!(py_in_top_toolbar(20.0, band));
        assert!(py_in_top_toolbar(50.0, band));
    }

    #[test]
    fn py_in_top_toolbar_false_below_band() {
        // Well into the terminal grid: events here should reach the PTY.
        assert!(!py_in_top_toolbar(100.0, 56.0));
    }

    #[test]
    fn py_in_top_toolbar_boundary_is_exclusive() {
        // The band is a strict `<`, so the boundary pixel itself is *not*
        // toolbar (it's the first row of the grid) but anything just above
        // it still is.
        let band = 56.0;
        assert!(!py_in_top_toolbar(band, band)); // exactly the band → false
        assert!(py_in_top_toolbar(band - 0.1, band)); // just under → true
    }

    #[test]
    fn py_in_top_toolbar_band_scales_with_dpi() {
        // The whole point of the chrome band tracking the native title-bar
        // height: a taller (Retina) band reaches a y that a shorter band
        // would have treated as grid. y=50 is chrome at 56px, grid at 40px.
        assert!(py_in_top_toolbar(50.0, 56.0));
        assert!(!py_in_top_toolbar(50.0, 40.0));
    }

    #[test]
    fn chrome_band_from_falls_back_to_reserve_when_query_fails() {
        // No native height available → use the renderer's fixed reserve.
        let reserve = (WINDOW_PADDING + DECORATOR_HEIGHT) as f64;
        assert_eq!(chrome_band_from(None, reserve), reserve);
    }

    #[test]
    fn chrome_band_from_clamps_short_native_up_to_reserve() {
        // Low-DPI: a native bar shorter than the reserve (even after the
        // margin) is clamped up, since the reserve already covers the bar.
        let reserve = (WINDOW_PADDING + DECORATOR_HEIGHT) as f64; // 40.0
        // 30 + CHROME_BAND_MARGIN_PX (4) = 34 < 40 → reserve.
        assert_eq!(chrome_band_from(Some(30.0), reserve), reserve);
        // Boundary: native+margin exactly equal to reserve is kept (>= reserve).
        assert_eq!(
            chrome_band_from(Some(reserve - CHROME_BAND_MARGIN_PX), reserve),
            reserve
        );
    }

    #[test]
    fn chrome_band_from_uses_native_plus_margin_when_taller() {
        // Retina: a native bar taller than the reserve drives the band,
        // with CHROME_BAND_MARGIN_PX added so the lowest grid move lands inside.
        let reserve = (WINDOW_PADDING + DECORATOR_HEIGHT) as f64; // 40.0
        assert_eq!(
            chrome_band_from(Some(52.0), reserve),
            52.0 + CHROME_BAND_MARGIN_PX
        );
    }

    /// Build a `CursorAnim` whose `started_at` is back-dated so that
    /// `elapsed / duration == t` at the moment of construction. Useful
    /// for deterministically exercising the smoothstep curve without
    /// flaky real-time waits.
    fn anim_at_t(from: (f32, f32), to: (f32, f32), duration: f32, t: f32) -> CursorAnim {
        let elapsed_secs = duration * t;
        let elapsed = Duration::from_secs_f32(elapsed_secs);
        CursorAnim {
            from,
            to,
            started_at: Instant::now() - elapsed,
        }
    }

    #[test]
    fn snapped_has_from_equal_to_target() {
        let mut a = CursorAnim::snapped((3.0, 4.0));
        assert_eq!(a.from, (3.0, 4.0));
        assert_eq!(a.to, (3.0, 4.0));
        // current() should immediately return the target regardless of duration.
        assert!(approx_pair(a.current(0.2), (3.0, 4.0)));
    }

    #[test]
    fn current_returns_to_when_duration_zero_or_negative() {
        let mut a = anim_at_t((0.0, 0.0), (10.0, 4.0), 0.2, 0.5);
        assert!(approx_pair(a.current(0.0), (10.0, 4.0)));
        let mut b = anim_at_t((0.0, 0.0), (10.0, 4.0), 0.2, 0.5);
        assert!(approx_pair(b.current(-1.0), (10.0, 4.0)));
    }

    #[test]
    fn current_returns_to_after_duration_elapses() {
        // Back-date 10s to guarantee elapsed >= duration for any reasonable duration.
        let mut a = CursorAnim {
            from: (0.0, 0.0),
            to: (10.0, 4.0),
            started_at: Instant::now() - Duration::from_secs(10),
        };
        assert!(approx_pair(a.current(0.2), (10.0, 4.0)));
        // And the snap means it no longer reports as animating.
        assert!(!a.animating(0.2));
    }

    #[test]
    fn smoothstep_midpoint_is_half() {
        // smoothstep(0.5) = 0.25 * (3 - 1) = 0.5
        let mut a = anim_at_t((0.0, 0.0), (10.0, 4.0), 0.2, 0.5);
        let p = a.current(0.2);
        assert!(approx_pair(p, (5.0, 2.0)), "got {:?}", p);
    }

    #[test]
    fn smoothstep_quarter_point() {
        // smoothstep(0.25) = 0.0625 * (3 - 0.5) = 0.15625
        let mut a = anim_at_t((0.0, 0.0), (10.0, 4.0), 0.2, 0.25);
        let p = a.current(0.2);
        assert!(approx_pair(p, (1.5625, 0.625)), "got {:?}", p);
    }

    #[test]
    fn animating_false_for_snapped() {
        let a = CursorAnim::snapped((3.0, 4.0));
        assert!(!a.animating(0.2));
    }

    #[test]
    fn animating_true_until_current_snaps_past_duration() {
        // Mid-flight: from != to, so animating.
        let mid = anim_at_t((0.0, 0.0), (10.0, 4.0), 0.2, 0.5);
        assert!(mid.animating(0.2));

        // Past duration but no current() call yet: from still != to,
        // so still reports animating. This is the safety net that keeps
        // the event loop ticking until a render actually snaps the value.
        let mut done = CursorAnim {
            from: (0.0, 0.0),
            to: (10.0, 4.0),
            started_at: Instant::now() - Duration::from_secs(10),
        };
        assert!(done.animating(0.2));
        let _ = done.current(0.2);
        assert!(!done.animating(0.2));
    }

    #[test]
    fn retarget_to_same_target_is_noop() {
        let original_started = Instant::now() - Duration::from_millis(50);
        let mut a = CursorAnim {
            from: (1.0, 1.0),
            to: (5.0, 5.0),
            started_at: original_started,
        };
        a.retarget((5.0, 5.0), 0.2);
        assert_eq!(a.from, (1.0, 1.0));
        assert_eq!(a.to, (5.0, 5.0));
        assert_eq!(a.started_at, original_started);
    }

    #[test]
    fn retarget_rebases_from_to_currently_eased_position() {
        // The contract being pinned is *continuity*: after retarget,
        // `from` equals whatever was visually displayed at the moment
        // of retarget, so the next ease starts where the previous frame
        // ended. The exact eased value depends on `Instant::now()`
        // drift between `anim_at_t` and `a.current()` — sub-millisecond
        // on most hardware, much more under CI load — so compare
        // `from` against the value `current()` actually returned, not a
        // hardcoded smoothstep result.
        let mut a = anim_at_t((0.0, 0.0), (10.0, 4.0), 0.2, 0.5);
        let pre = a.current(0.2);
        // Sanity: midway through a forward ease, pre is somewhere
        // between (0,0) and (10,4). Loose bounds — the precise value
        // is what `current()` decides at this exact moment.
        assert!(pre.0 > 0.0 && pre.0 < 10.0, "pre.0 = {}", pre.0);
        assert!(pre.1 > 0.0 && pre.1 < 4.0, "pre.1 = {}", pre.1);

        a.retarget((20.0, 8.0), 0.2);

        // Continuity: new `from` matches whatever `current()` returned.
        // `retarget` calls `current()` again internally, so a few
        // microseconds of `Instant::now()` drift between the test's
        // `current()` call and retarget's internal one produce a
        // sub-thousandth-of-a-percent difference. Tolerance generous
        // enough to swallow CI scheduler jitter but tight enough to
        // catch a continuity bug (which would shift `from` by units,
        // not millionths).
        let drift = ((a.from.0 - pre.0).abs(), (a.from.1 - pre.1).abs());
        assert!(
            drift.0 < 0.05 && drift.1 < 0.05,
            "continuity broken: from={:?} pre={:?}",
            a.from, pre,
        );
        assert_eq!(a.to, (20.0, 8.0));
        // started_at should be (approximately) "now" — well after the
        // back-dated original. Elapsed should be very small.
        assert!(a.started_at.elapsed() < Duration::from_millis(100));
    }

    #[test]
    fn is_blank_cell_true_for_space_and_nul() {
        let space = style::Cell::new(' ', style::Style::new());
        let nul = style::Cell::new('\0', style::Style::new());
        assert!(is_blank_cell(&space));
        assert!(is_blank_cell(&nul));
    }

    #[test]
    fn is_blank_cell_false_for_visible_chars() {
        for ch in ['x', 'a', '1', '.'] {
            let cell = style::Cell::new(ch, style::Style::new());
            assert!(!is_blank_cell(&cell), "expected {:?} to be non-blank", ch);
        }
    }

    #[test]
    fn is_blank_cell_ignores_style() {
        // A space with bold + a foreground color is still blank — only `ch` matters.
        let mut style = style::Style::new();
        style.bold = true;
        style.color_fg = Some([1.0; 4]);
        let cell = style::Cell::new(' ', style);
        assert!(is_blank_cell(&cell));
    }

    #[test]
    fn viewport_key_equality_and_field_sensitivity() {
        let base = ViewportKey {
            rows: 24,
            cols: 80,
            view_offset: 0,
            on_alt_screen: false,
        };
        // Identical keys compare equal.
        let same = ViewportKey {
            rows: 24,
            cols: 80,
            view_offset: 0,
            on_alt_screen: false,
        };
        // `ViewportKey` doesn't derive `Debug`, so use `assert!` over `==`/`!=`
        // rather than `assert_eq!`/`assert_ne!`.
        assert!(base == same);

        // Flipping any single field breaks equality.
        let diff_rows = ViewportKey { rows: 25, ..base };
        let diff_cols = ViewportKey { cols: 81, ..base };
        let diff_offset = ViewportKey {
            view_offset: 1,
            ..base
        };
        let diff_alt = ViewportKey {
            on_alt_screen: true,
            ..base
        };
        assert!(base != diff_rows);
        assert!(base != diff_cols);
        assert!(base != diff_offset);
        assert!(base != diff_alt);
    }

    #[test]
    fn config_defaults_cursor_blink_is_false() {
        assert!(!Config::defaults().cursor_blink);
    }

    #[test]
    fn config_round_trip_preserves_cursor_blink_true() {
        let mut c = Config::defaults();
        c.cursor_blink = true;
        let parsed = Config::parse_str(&c.serialize());
        assert!(parsed.cursor_blink);
    }

    #[test]
    fn config_parse_invalid_cursor_blink_keeps_default() {
        // A wrong-typed value must not poison the rest of the config — the
        // field stays at its default (false) and other keys still parse.
        // (The value stays valid TOML, a string, so the document parses and
        // the per-key skip kicks in rather than a whole-file syntax error.)
        let parsed = Config::parse_str("cursor_blink = \"banana\"\nfont_size = 12.5\n");
        assert!(!parsed.cursor_blink);
        assert!(approx_eq(parsed.font_size, 12.5));
    }

    #[test]
    fn config_defaults_glow_disabled() {
        let c = Config::defaults();
        assert!(!c.glow_match_brightness);
        assert!(!c.glow_match_bright_ansi);
        assert!(!c.glow_match_foreground);
        assert!(!c.glow_scanlines);
        assert!(!c.glow_scanlines_content);
        assert!(!c.glow_scanlines_skip_primary_bg);
        assert!((0.0..=1.0).contains(&c.glow_scanlines_content_strength));
        assert!((0.0..=1.0).contains(&c.glow_threshold));
        assert!(c.glow_intensity > 0.0);
        assert!((0.0..=180.0).contains(&c.glow_hue_tolerance_deg));
        assert!(c.glow_fg_tolerance >= 0.0 && c.glow_fg_tolerance <= 3.0_f32.sqrt());
        assert!((0.0..=1.0).contains(&c.glow_scanline_strength));
        assert!(c.glow_scanline_period >= 1.0);
        assert!(c.glow_iterations >= 1);
    }

    #[test]
    fn config_round_trip_preserves_glow_fields() {
        let mut c = Config::defaults();
        c.glow_match_brightness = true;
        c.glow_match_bright_ansi = true;
        c.glow_match_foreground = true;
        c.glow_threshold = 0.42;
        c.glow_intensity = 1.75;
        c.glow_softness = 0.25;
        c.glow_hue_tolerance_deg = 22.5;
        c.glow_fg_tolerance = 0.20;
        c.glow_iterations = 5;
        c.glow_scanlines = true;
        c.glow_scanline_strength = 0.65;
        c.glow_scanline_period = 6.0;
        c.glow_scanlines_content = true;
        c.glow_scanlines_content_strength = 0.40;
        c.glow_scanlines_skip_primary_bg = true;
        let parsed = Config::parse_str(&c.serialize());
        assert!(parsed.glow_match_brightness);
        assert!(parsed.glow_match_bright_ansi);
        assert!(parsed.glow_match_foreground);
        assert!(approx_eq(parsed.glow_threshold, 0.42));
        assert!(approx_eq(parsed.glow_intensity, 1.75));
        assert!(approx_eq(parsed.glow_softness, 0.25));
        assert!(approx_eq(parsed.glow_hue_tolerance_deg, 22.5));
        assert!(approx_eq(parsed.glow_fg_tolerance, 0.20));
        assert_eq!(parsed.glow_iterations, 5);
        assert!(parsed.glow_scanlines);
        assert!(approx_eq(parsed.glow_scanline_strength, 0.65));
        assert!(approx_eq(parsed.glow_scanline_period, 6.0));
        assert!(parsed.glow_scanlines_content);
        assert!(approx_eq(parsed.glow_scanlines_content_strength, 0.40));
        assert!(parsed.glow_scanlines_skip_primary_bg);
    }

    #[test]
    fn config_image_defaults_match_design() {
        let c = Config::defaults();
        assert!(c.images_enabled);
        assert!(c.images_in_scrollback);
        assert_eq!(c.images_memory_cap_mb, 256);
        assert_eq!(c.images_max_pixels, 16 * 1024 * 1024);
        assert_eq!(c.images_decode_timeout_ms, 2000);
        assert_eq!(c.images_filter, "linear");
    }

    #[test]
    fn config_round_trip_preserves_image_fields() {
        let mut c = Config::defaults();
        c.images_enabled = false;
        c.images_in_scrollback = false;
        c.images_memory_cap_mb = 128;
        c.images_max_pixels = 8 * 1024 * 1024;
        c.images_decode_timeout_ms = 500;
        c.images_filter = "nearest".into();
        let parsed = Config::parse_str(&c.serialize());
        assert!(!parsed.images_enabled);
        assert!(!parsed.images_in_scrollback);
        assert_eq!(parsed.images_memory_cap_mb, 128);
        assert_eq!(parsed.images_max_pixels, 8 * 1024 * 1024);
        assert_eq!(parsed.images_decode_timeout_ms, 500);
        assert_eq!(parsed.images_filter, "nearest");
    }

    #[test]
    fn config_image_decode_timeout_floor() {
        // 50ms floor — anything lower defeats the worker since even a
        // tiny PNG takes a millisecond or two to decode.
        let parsed = Config::parse_str("images_decode_timeout_ms = 0\n");
        assert_eq!(parsed.images_decode_timeout_ms, 50);
        let parsed = Config::parse_str("images_decode_timeout_ms = 10\n");
        assert_eq!(parsed.images_decode_timeout_ms, 50);
    }

    #[test]
    fn config_image_max_pixels_floor_avoids_zero() {
        // Zero would disable decoding entirely without an obvious error;
        // we clamp to at least 1 pixel so the rejection path stays
        // observable.
        let parsed = Config::parse_str("images_max_pixels = 0\n");
        assert_eq!(parsed.images_max_pixels, 1);
    }

    #[test]
    fn config_image_filter_rejects_unknown_value() {
        let parsed = Config::parse_str("images_filter = \"bicubic\"\n");
        // Unknown values keep the default — matches `color_scheme` semantics.
        assert_eq!(parsed.images_filter, "linear");
    }

    #[test]
    fn config_image_invalid_bool_keeps_default() {
        let parsed = Config::parse_str("images_enabled = \"banana\"\n");
        assert!(parsed.images_enabled);
    }

    #[test]
    fn shell_exit_mode_from_str_valid_values() {
        assert_eq!(ShellExitMode::from_str("always"), Some(ShellExitMode::Always));
        assert_eq!(ShellExitMode::from_str("never"), Some(ShellExitMode::Never));
        assert_eq!(
            ShellExitMode::from_str("on_success"),
            Some(ShellExitMode::OnSuccess)
        );
    }

    #[test]
    fn shell_exit_mode_from_str_unknown_is_none() {
        assert_eq!(ShellExitMode::from_str("bogus"), None);
        assert_eq!(ShellExitMode::from_str(""), None);
    }

    #[test]
    fn shell_exit_mode_round_trips_through_as_str() {
        for m in [
            ShellExitMode::Always,
            ShellExitMode::Never,
            ShellExitMode::OnSuccess,
        ] {
            assert_eq!(ShellExitMode::from_str(m.as_str()), Some(m));
        }
    }

    #[test]
    fn config_shell_exit_mode_default_is_on_success() {
        assert_eq!(Config::defaults().shell_exit_mode, ShellExitMode::OnSuccess);
    }

    #[test]
    fn config_shell_exit_mode_parses_explicit_values() {
        let never = Config::parse_str("shell_exit_mode = \"never\"\n");
        assert_eq!(never.shell_exit_mode, ShellExitMode::Never);

        let always = Config::parse_str("shell_exit_mode = \"always\"\n");
        assert_eq!(always.shell_exit_mode, ShellExitMode::Always);
    }

    #[test]
    fn config_shell_exit_mode_unknown_keeps_default() {
        let parsed = Config::parse_str("shell_exit_mode = \"bogus\"\n");
        assert_eq!(parsed.shell_exit_mode, ShellExitMode::OnSuccess);
    }

    #[test]
    fn config_shell_exit_mode_missing_key_defaults() {
        // A config that doesn't mention the key keeps the default.
        let parsed = Config::parse_str("font_size = 14.0\n");
        assert_eq!(parsed.shell_exit_mode, ShellExitMode::OnSuccess);
    }

    #[test]
    fn config_shell_exit_mode_round_trips_through_serialize() {
        let mut c = Config::defaults();
        c.shell_exit_mode = ShellExitMode::Never;
        let parsed = Config::parse_str(&c.serialize());
        assert_eq!(parsed.shell_exit_mode, ShellExitMode::Never);
    }

    #[test]
    fn prompt_gutter_from_str_valid_values() {
        assert_eq!(PromptGutter::from_str("none"), Some(PromptGutter::None));
        assert_eq!(PromptGutter::from_str("bar"), Some(PromptGutter::Bar));
    }

    #[test]
    fn prompt_gutter_from_str_unknown_is_none() {
        assert_eq!(PromptGutter::from_str("bogus"), None);
        assert_eq!(PromptGutter::from_str(""), None);
    }

    #[test]
    fn prompt_gutter_round_trips_through_as_str() {
        for g in [PromptGutter::None, PromptGutter::Bar] {
            assert_eq!(PromptGutter::from_str(g.as_str()), Some(g));
        }
    }

    #[test]
    fn config_prompt_gutter_default_is_none() {
        assert_eq!(Config::defaults().prompt_gutter, PromptGutter::None);
    }

    #[test]
    fn config_prompt_gutter_parses_explicit_value() {
        let bar = Config::parse_str("prompt_gutter = \"bar\"\n");
        assert_eq!(bar.prompt_gutter, PromptGutter::Bar);
    }

    #[test]
    fn config_prompt_gutter_unknown_keeps_default() {
        let parsed = Config::parse_str("prompt_gutter = \"squiggle\"\n");
        assert_eq!(parsed.prompt_gutter, PromptGutter::None);
    }

    #[test]
    fn config_prompt_gutter_missing_key_defaults() {
        let parsed = Config::parse_str("font_size = 14.0\n");
        assert_eq!(parsed.prompt_gutter, PromptGutter::None);
    }

    #[test]
    fn config_prompt_gutter_round_trips_through_serialize() {
        let mut c = Config::defaults();
        c.prompt_gutter = PromptGutter::Bar;
        let parsed = Config::parse_str(&c.serialize());
        assert_eq!(parsed.prompt_gutter, PromptGutter::Bar);
    }

    #[test]
    fn config_autocomplete_default_is_on() {
        assert!(Config::defaults().autocomplete);
    }

    #[test]
    fn config_autocomplete_missing_key_defaults_on() {
        let parsed = Config::parse_str("font_size = 14.0\n");
        assert!(parsed.autocomplete);
    }

    #[test]
    fn config_autocomplete_parses_explicit_false() {
        let parsed = Config::parse_str("autocomplete = false\n");
        assert!(!parsed.autocomplete);
    }

    #[test]
    fn config_autocomplete_parses_explicit_true() {
        let parsed = Config::parse_str("autocomplete = true\n");
        assert!(parsed.autocomplete);
    }

    #[test]
    fn config_autocomplete_round_trips_through_serialize() {
        let mut c = Config::defaults();
        c.autocomplete = false;
        let parsed = Config::parse_str(&c.serialize());
        assert!(!parsed.autocomplete);
    }

    #[test]
    fn config_image_halfblock_for_missing_default_off() {
        // Default off: the opt-in only matters for the failure-cleanup
        // window, where most users either don't notice the one-frame
        // blank or wouldn't appreciate a colored-cell flash on a
        // decode error. Keeping it off is the conservative choice.
        let c = Config::defaults();
        assert!(!c.images_halfblock_for_missing);
    }

    #[test]
    fn config_image_halfblock_for_missing_round_trips() {
        let mut c = Config::defaults();
        c.images_halfblock_for_missing = true;
        let parsed = Config::parse_str(&c.serialize());
        assert!(parsed.images_halfblock_for_missing);
    }

    #[test]
    fn config_font_family_default_is_none() {
        assert!(Config::defaults().font_family.is_none());
    }

    #[test]
    fn config_font_family_round_trips_some() {
        let mut c = Config::defaults();
        c.font_family = Some("Fira Code".to_string());
        let parsed = Config::parse_str(&c.serialize());
        assert_eq!(parsed.font_family.as_deref(), Some("Fira Code"));
    }

    #[test]
    fn config_font_family_round_trips_none() {
        let c = Config::defaults();
        let parsed = Config::parse_str(&c.serialize());
        assert!(parsed.font_family.is_none());
    }

    #[test]
    fn config_font_family_empty_parses_as_none() {
        // Matches `color_scheme` semantics: an explicit empty string clears
        // the override rather than installing the empty string.
        let parsed = Config::parse_str("font_family = \"\"\n");
        assert!(parsed.font_family.is_none());
    }

    #[test]
    fn config_font_family_preserves_spaces_in_name() {
        // TOML quotes delimit the value, so multi-word family names need no
        // special handling.
        let parsed = Config::parse_str("font_family = \"JetBrains Mono\"\n");
        assert_eq!(parsed.font_family.as_deref(), Some("JetBrains Mono"));
    }

    #[test]
    fn config_font_family_quoted_value_is_verbatim() {
        // Unlike the old line-based parser, TOML does not trim — whatever
        // sits inside the quotes is taken literally. Documented here so a
        // future "helpfully trim it" change has to break a test on purpose.
        let parsed = Config::parse_str("font_family = \"  Padded  \"\n");
        assert_eq!(parsed.font_family.as_deref(), Some("  Padded  "));
    }

    //
    // Half-block fallback decision matrix. Pure function — no Store,
    // Terminal, or window required. Each row pins one cell of the
    // (images_enabled × opted_in × is_pending × gpu_image_available)
    // truth table so a future refactor that changes priorities (e.g.
    // accidentally letting an in-flight decode flicker) fails loud.
    //

    #[test]
    fn halfblock_decision_pending_decode_never_renders() {
        // Pending wins over every other flag — the flicker case.
        for &en in &[true, false] {
            for &oi in &[true, false] {
                for &gpu in &[true, false] {
                    assert!(
                        !WindowState::should_halfblock(en, oi, /*pending*/ true, gpu),
                        "pending should suppress halfblock (en={en} oi={oi} gpu={gpu})"
                    );
                }
            }
        }
    }

    #[test]
    fn halfblock_decision_disabled_always_renders_when_not_pending() {
        // images_enabled=false ⇒ GPU draw is forbidden. Half-block is
        // the only visible representation; opt-in doesn't gate this
        // because the user already turned the GPU path off.
        for &oi in &[true, false] {
            for &gpu in &[true, false] {
                assert!(
                    WindowState::should_halfblock(/*en*/ false, oi, false, gpu),
                    "disabled should always halfblock (oi={oi} gpu={gpu})"
                );
            }
        }
    }

    #[test]
    fn halfblock_decision_enabled_with_gpu_image_never_renders() {
        // GPU has the texture, GPU path draws → don't double-emit a
        // half-block on top.
        assert!(!WindowState::should_halfblock(true, false, false, true));
        assert!(!WindowState::should_halfblock(true, true, false, true));
    }

    #[test]
    fn halfblock_decision_enabled_missing_image_needs_opt_in() {
        // images_enabled=true, peek=None, !pending → decode failed and
        // cleanup hasn't fired. Only honored when the user opts in.
        assert!(!WindowState::should_halfblock(true, false, false, false));
        assert!(WindowState::should_halfblock(true, true, false, false));
    }

    //
    // Deferred-placement suppression. Drives the ghost-image regression:
    // without `kitty_image_id` gating, an `a=T,U=1` upload would leave
    // `preplaced_image_id = None`, and the deferred-place branch in
    // `poll_pending_images` would stamp a second Placement at the cursor
    // row alongside the placeholder-bbox draw.
    //

    #[test]
    fn suppress_deferred_placement_cmd_shift_i_keeps_deferred_place_active() {
        // Cmd-Shift-I debug-paste: no Kitty id, no up-front display.
        // Caller stores `None` so the on-decode-success branch creates
        // the Placement at the cursor.
        assert!(!WindowState::suppress_deferred_placement(false, None));
    }

    #[test]
    fn suppress_deferred_placement_a_t_capital_already_placed_skips_deferred_place() {
        // `a=T` without `U=1`: `insert_placement_kitty` already ran.
        // No second Placement should be auto-created.
        assert!(WindowState::suppress_deferred_placement(true, Some(42)));
    }

    #[test]
    fn suppress_deferred_placement_a_t_capital_with_virtual_placement_skips_deferred_place() {
        // `a=T,U=1`: placeholder cells own the placement. A deferred
        // auto-place would produce the "ghost image" regression.
        assert!(WindowState::suppress_deferred_placement(false, Some(42)));
    }

    #[test]
    fn suppress_deferred_placement_a_t_transmit_only_skips_deferred_place() {
        // `a=t`: client will issue `a=p` later. Auto-placing at
        // cursor would beat the client's explicit placement to the
        // screen and end up double-drawn after `a=p` arrives.
        assert!(WindowState::suppress_deferred_placement(false, Some(7)));
    }

    //
    // Grid → viewport row shift for live image placements. Originally
    // missed (placement rendered using grid row directly); when the user
    // scrolled history into view, images stayed pinned to the viewport
    // row they were initially drawn at while the surrounding text shifted
    // down. The pure helper makes the math testable without firing up a
    // GPU adapter.
    //

    #[test]
    fn live_placement_viewport_row_passes_through_with_no_offset() {
        // The no-scrollback-in-view common case — image at grid row 5 in
        // a 24-row viewport renders at viewport row 5.
        assert_eq!(WindowState::live_placement_viewport_row(5, 0, 24), 5);
    }

    #[test]
    fn live_placement_viewport_row_shifts_down_by_view_offset() {
        // 3 scrollback rows pulled into view → live content shifts down 3.
        assert_eq!(WindowState::live_placement_viewport_row(5, 3, 24), 8);
        // Negative grid rows (placement straddling above the viewport)
        // shift the same way — clipping happens downstream.
        assert_eq!(WindowState::live_placement_viewport_row(-2, 3, 24), 1);
    }

    #[test]
    fn live_placement_viewport_row_does_not_clamp_at_rows() {
        // Regression: the helper used to clamp `view_offset` at `rows`,
        // which froze the image's discrete viewport_row when scrolled
        // deeper into history. Smooth-scroll's `scroll_y` kept
        // interpolating between ticks, so the image visually slid by
        // a row each tick then snapped back when the tick fired
        // (viewport_row hadn't moved). Without the clamp, viewport_row
        // moves in lockstep with view_offset and scroll_y, giving a
        // continuous slide as the image enters from below.
        assert_eq!(WindowState::live_placement_viewport_row(5, 100, 24), 5 + 100);
        assert_eq!(WindowState::live_placement_viewport_row(0, 50, 24), 50);
        // Sanity: at view_offset <= rows, behavior is unchanged from
        // the pre-clamp version.
        assert_eq!(WindowState::live_placement_viewport_row(5, 20, 24), 25);
    }

    //
    // Per-run UV math for Kitty placeholder draws.
    //

    #[test]
    fn placeholder_run_uv_full_row_spans_full_width_one_row_height() {
        // 3 cells wide, image_row 0, total (3, 2) →
        //   u: 0..3/3 = 0..1
        //   v: 0..1/2 = 0..0.5
        let uv = WindowState::placeholder_run_uv(0, 3, 0, 3, 2);
        assert_eq!(uv, (0.0, 0.0, 1.0, 0.5));
    }

    #[test]
    fn placeholder_run_uv_partial_row_samples_proper_strip() {
        // Cells image_col 1..3 of a 4-col tile, image_row 1 of 2
        // rows → upper-left at (0.25, 0.5), lower-right at (0.75, 1.0).
        let uv = WindowState::placeholder_run_uv(1, 3, 1, 4, 2);
        assert_eq!(uv, (0.25, 0.5, 0.75, 1.0));
    }

    #[test]
    fn placeholder_run_uv_clamps_out_of_range_to_unit_square() {
        // image_col_end past the right edge, image_row past the
        // bottom — both clamp to 1.0 rather than wrap or NaN.
        let uv = WindowState::placeholder_run_uv(5, 10, 7, 4, 2);
        assert_eq!(uv, (1.0, 1.0, 1.0, 1.0));
    }

    #[test]
    fn placeholder_run_uv_zero_total_dims_treated_as_one() {
        // A `c=0` / `r=0` transmission shouldn't reach this helper
        // (the renderer skips runs without a recorded extent), but
        // guard the denominator so we never NaN. With cols=0 →
        // denom 1, image_col_end=0 → u1=0.0 clamped from 0 itself.
        let uv = WindowState::placeholder_run_uv(0, 0, 0, 0, 0);
        assert_eq!(uv, (0.0, 0.0, 0.0, 1.0));
    }

    //
    // Cell-filling glyph quad UV math (half-texel seam inset).
    //

    #[test]
    fn glyph_quad_uv_insets_both_axes_for_a_cell_filling_glyph() {
        // Any cell-filling glyph is inset half a texel on BOTH axes so no
        // hard opaque edge samples the transparent atlas padding. Glyph at
        // atlas origin (0, 0), sampling cols 0..8 / rows 0..16 of a
        // 128x128 atlas.
        let (u0, v0, u1, v1) =
            WindowState::glyph_quad_uv(0.0, 0.0, (0.0, 8.0), (0.0, 16.0), true, 128.0, 128.0);
        assert!(approx_pair((u0, u1), (0.5 / 128.0, 7.5 / 128.0)));
        assert!(approx_pair((v0, v1), (0.5 / 128.0, 15.5 / 128.0)));
    }

    #[test]
    fn glyph_quad_uv_insets_a_trimmed_half_block_on_its_narrow_axis() {
        // Regression: ▐ is packed trimmed to its opaque right half
        // (a half-width bitmap that bears into the cell), so its narrow
        // axis is placed 1:1 — yet its filled side reaches the bitmap edge
        // and must still be inset, or it bleeds into the padding and
        // leaves a hairline at the cell boundary. The inset is keyed on
        // cell_filling, so the horizontal extent IS pulled in here. Glyph
        // at (10, 20), sampling cols 0..8 / rows 0..16 of a 256x256 atlas.
        let (u0, v0, u1, v1) =
            WindowState::glyph_quad_uv(10.0, 20.0, (0.0, 8.0), (0.0, 16.0), true, 256.0, 256.0);
        assert!(approx_pair((u0, u1), (10.5 / 256.0, 17.5 / 256.0)));
        assert!(approx_pair((v0, v1), (20.5 / 256.0, 35.5 / 256.0)));
    }

    #[test]
    fn glyph_quad_uv_no_inset_for_a_non_filling_glyph() {
        // A normal (non-cell-filling) glyph gets no inset on either axis:
        // the UV is the raw sample rect, so ordinary antialiased glyphs
        // aren't thinned or shifted.
        let (u0, v0, u1, v1) =
            WindowState::glyph_quad_uv(4.0, 4.0, (1.0, 7.0), (2.0, 14.0), false, 64.0, 64.0);
        assert!(approx_pair((u0, u1), (5.0 / 64.0, 11.0 / 64.0)));
        assert!(approx_pair((v0, v1), (6.0 / 64.0, 18.0 / 64.0)));
    }

    #[test]
    fn glyph_quad_uv_inset_shrinks_each_sampled_span_by_one_texel() {
        // The seam fix narrows the sampled span by exactly one texel total
        // (half a texel off each edge) on both axes for a cell-filling
        // glyph, and leaves both spans untouched for a normal glyph. Pin
        // that in atlas-texel units so a regression to a different inset is
        // caught.
        let raw =
            WindowState::glyph_quad_uv(0.0, 0.0, (0.0, 10.0), (0.0, 10.0), false, 100.0, 100.0);
        let inset =
            WindowState::glyph_quad_uv(0.0, 0.0, (0.0, 10.0), (0.0, 10.0), true, 100.0, 100.0);
        assert!(approx_eq((raw.2 - raw.0) * 100.0 - (inset.2 - inset.0) * 100.0, 1.0));
        assert!(approx_eq((raw.3 - raw.1) * 100.0 - (inset.3 - inset.1) * 100.0, 1.0));
    }

    #[test]
    fn config_glow_scanline_strength_clamped() {
        let parsed = Config::parse_str("glow_scanline_strength = 5\n");
        assert!((0.0..=1.0).contains(&parsed.glow_scanline_strength));
        let parsed = Config::parse_str("glow_scanline_strength = -2\n");
        assert!((0.0..=1.0).contains(&parsed.glow_scanline_strength));
    }

    #[test]
    fn config_glow_scanline_period_clamped() {
        let parsed = Config::parse_str("glow_scanline_period = 0.1\n");
        assert!(parsed.glow_scanline_period >= 1.0);
    }

    #[test]
    fn config_glow_scanline_colors_parse_hex() {
        let parsed = Config::parse_str(
            "glow_scanline_color_bright = 0xff0000\n\
             glow_scanline_color_dark = 0x00ff00\n",
        );
        // sRGB → linear: 0xff → 1.0, 0x00 → 0.0. The whole-channel
        // values survive the round-trip exactly.
        assert!(approx_eq(parsed.glow_scanline_color_bright[0], 1.0));
        assert!(approx_eq(parsed.glow_scanline_color_bright[1], 0.0));
        assert!(approx_eq(parsed.glow_scanline_color_dark[1], 1.0));
        assert!(approx_eq(parsed.glow_scanline_color_dark[2], 0.0));
    }

    #[test]
    fn config_glow_scanline_color_invalid_keeps_default() {
        // A wrong-typed value (string instead of a hex integer) is skipped
        // per-key, leaving the default in place.
        let parsed = Config::parse_str("glow_scanline_color_bright = \"notahex\"\n");
        let d = Config::defaults();
        assert!(approx_eq(parsed.glow_scanline_color_bright[0], d.glow_scanline_color_bright[0]));
    }

    #[test]
    fn config_round_trip_preserves_scanline_colors() {
        let mut c = Config::defaults();
        c.glow_scanline_color_bright = palette::rgb_from_value(&toml::Value::Integer(0xff8800)).unwrap();
        c.glow_scanline_color_dark = palette::rgb_from_value(&toml::Value::Integer(0x110022)).unwrap();
        let parsed = Config::parse_str(&c.serialize());
        // sRGB byte round-trip is exact (palette ensures this).
        assert_eq!(
            palette::linear_to_srgb_u8(parsed.glow_scanline_color_bright[0]),
            0xff,
        );
        assert_eq!(
            palette::linear_to_srgb_u8(parsed.glow_scanline_color_bright[1]),
            0x88,
        );
        assert_eq!(
            palette::linear_to_srgb_u8(parsed.glow_scanline_color_dark[2]),
            0x22,
        );
    }

    #[test]
    fn config_glow_fg_tolerance_clamped() {
        let parsed = Config::parse_str("glow_fg_tolerance = 99\n");
        assert!(parsed.glow_fg_tolerance <= 3.0_f32.sqrt());
        let parsed = Config::parse_str("glow_fg_tolerance = -1\n");
        assert!(parsed.glow_fg_tolerance >= 0.0);
    }

    #[test]
    fn config_glow_threshold_clamped_on_parse() {
        // TOML forbids duplicate keys, so the high and low ends are exercised
        // by separate documents.
        let high = Config::parse_str("glow_threshold = 2.5\n");
        assert!((0.0..=1.0).contains(&high.glow_threshold));
        let low = Config::parse_str("glow_threshold = -1\n");
        assert!((0.0..=1.0).contains(&low.glow_threshold));
    }

    #[test]
    fn config_glow_hue_tolerance_clamped() {
        let parsed = Config::parse_str("glow_hue_tolerance_deg = 500\n");
        assert!((0.0..=180.0).contains(&parsed.glow_hue_tolerance_deg));
        let parsed = Config::parse_str("glow_hue_tolerance_deg = -10\n");
        assert!((0.0..=180.0).contains(&parsed.glow_hue_tolerance_deg));
    }

    #[test]
    fn config_glow_iterations_clamped_to_max() {
        let parsed = Config::parse_str(&format!(
            "glow_iterations = {}\n",
            renderer::glow::MAX_ITERATIONS * 4
        ));
        assert_eq!(parsed.glow_iterations, renderer::glow::MAX_ITERATIONS);
    }

    #[test]
    fn config_invalid_glow_value_keeps_default() {
        let parsed = Config::parse_str(
            "glow_match_brightness = nope\nglow_match_bright_ansi = ?\nglow_intensity = abc\n",
        );
        let d = Config::defaults();
        assert_eq!(parsed.glow_match_brightness, d.glow_match_brightness);
        assert_eq!(parsed.glow_match_bright_ansi, d.glow_match_bright_ansi);
        assert!(approx_eq(parsed.glow_intensity, d.glow_intensity));
    }

    //
    // TOML migration coverage: serialize() must emit valid parseable TOML
    // (comments / section headers and all), every field must survive a
    // serialize → parse_str round-trip, numeric slots must accept both
    // integers and floats, and a syntax-broken document must fall back to
    // defaults wholesale (mirroring the palette parser's contract).
    //

    #[test]
    fn config_serialize_emits_parseable_toml() {
        // The serializer hand-writes `# comment` section headers and blank
        // lines between groups. This pins that none of that decoration breaks
        // the TOML grammar — `serialize()` output must always re-parse as a
        // table, or `save()` would write a config the next launch can't read.
        let c = Config::defaults();
        let table: Result<toml::Table, _> = c.serialize().parse();
        assert!(table.is_ok(), "serialize() must be valid TOML: {:?}", table.err());
    }

    #[test]
    fn config_full_default_round_trip_is_identity() {
        // The strongest single invariant: serializing the defaults and
        // parsing them back must reproduce the defaults exactly. Catches any
        // field the serializer forgets to emit (which would silently revert
        // to default on the next load) or any asymmetry between the key names
        // serialize() writes and apply() reads.
        let d = Config::defaults();
        let parsed = Config::parse_str(&d.serialize());
        // Compare field-by-field with float tolerance; Config has no Eq.
        assert!(approx_eq(parsed.font_size, d.font_size));
        assert!(approx_eq(parsed.top_fade_height, d.top_fade_height));
        assert!(approx_eq(parsed.top_fade_solid_stop, d.top_fade_solid_stop));
        assert!(approx_eq(parsed.top_fade_anim_secs, d.top_fade_anim_secs));
        assert!(approx_eq(parsed.bottom_fade_height, d.bottom_fade_height));
        assert!(approx_eq(parsed.bottom_fade_anim_secs, d.bottom_fade_anim_secs));
        assert!(approx_eq(parsed.cursor_anim_secs, d.cursor_anim_secs));
        assert_eq!(parsed.cursor_blink, d.cursor_blink);
        assert_eq!(parsed.blur_iterations, d.blur_iterations);
        assert_eq!(parsed.color_scheme, d.color_scheme);
        assert_eq!(parsed.font_family, d.font_family);
        assert_eq!(parsed.theme_overrides_glow, d.theme_overrides_glow);
        assert_eq!(parsed.images_enabled, d.images_enabled);
        assert_eq!(parsed.images_memory_cap_mb, d.images_memory_cap_mb);
        assert_eq!(parsed.images_max_pixels, d.images_max_pixels);
        assert_eq!(parsed.images_decode_timeout_ms, d.images_decode_timeout_ms);
        assert_eq!(parsed.images_in_scrollback, d.images_in_scrollback);
        assert_eq!(parsed.images_filter, d.images_filter);
        assert_eq!(parsed.images_halfblock_for_missing, d.images_halfblock_for_missing);
    }

    #[test]
    fn config_round_trip_preserves_fade_fields() {
        // The five fade knobs are floats with no clamp on the parse path, so
        // any non-default value must survive serialize → parse_str unchanged.
        // Previously only the glow/image/font groups were round-trip tested.
        let mut c = Config::defaults();
        c.top_fade_height = 123.5;
        c.top_fade_solid_stop = 0.625;
        c.top_fade_anim_secs = 0.5;
        c.bottom_fade_height = 64.25;
        c.bottom_fade_anim_secs = 0.75;
        c.cursor_anim_secs = 0.12;
        let parsed = Config::parse_str(&c.serialize());
        assert!(approx_eq(parsed.top_fade_height, 123.5));
        assert!(approx_eq(parsed.top_fade_solid_stop, 0.625));
        assert!(approx_eq(parsed.top_fade_anim_secs, 0.5));
        assert!(approx_eq(parsed.bottom_fade_height, 64.25));
        assert!(approx_eq(parsed.bottom_fade_anim_secs, 0.75));
        assert!(approx_eq(parsed.cursor_anim_secs, 0.12));
    }

    #[test]
    fn config_round_trip_preserves_blur_iterations() {
        // blur_iterations is a usize clamped to MAX_BLUR_ITERATIONS on parse.
        // A non-default in-range value must round-trip exactly.
        let mut c = Config::defaults();
        c.blur_iterations = 5;
        let parsed = Config::parse_str(&c.serialize());
        assert_eq!(parsed.blur_iterations, 5);
    }

    #[test]
    fn config_blur_iterations_clamped_to_max() {
        // Same clamp the renderer relies on: an over-large request is capped
        // at MAX_BLUR_ITERATIONS so the blur chain stays bounded.
        let parsed = Config::parse_str(&format!(
            "blur_iterations = {}\n",
            renderer::blur::MAX_BLUR_ITERATIONS + 10
        ));
        assert_eq!(parsed.blur_iterations, renderer::blur::MAX_BLUR_ITERATIONS);
    }

    #[test]
    fn config_font_size_accepts_integer_and_float() {
        // TOML reads `10` as an integer and `10.0` as a float; `cfg_f32` must
        // accept both for a numeric slot so users aren't forced to write a
        // decimal point. Both forms must produce the same value.
        let from_int = Config::parse_str("font_size = 10\n");
        let from_float = Config::parse_str("font_size = 10.0\n");
        assert!(approx_eq(from_int.font_size, 10.0));
        assert!(approx_eq(from_float.font_size, 10.0));
    }

    #[test]
    fn config_color_scheme_round_trips_some() {
        // A `Some(name)` color_scheme must serialize (quoted) and parse back
        // to the same name. The serializer omits the key entirely when None,
        // so the Some path needs its own pin.
        let mut c = Config::defaults();
        c.color_scheme = Some("solarized-dark".to_string());
        let parsed = Config::parse_str(&c.serialize());
        assert_eq!(parsed.color_scheme.as_deref(), Some("solarized-dark"));
    }

    #[test]
    fn config_color_scheme_round_trips_none() {
        // Default None: the serializer drops the key, and a config without it
        // parses back to None rather than an empty Some("").
        let c = Config::defaults();
        assert!(c.color_scheme.is_none());
        let parsed = Config::parse_str(&c.serialize());
        assert!(parsed.color_scheme.is_none());
    }

    #[test]
    fn config_color_scheme_empty_parses_as_none() {
        // An explicit empty string clears the override to None rather than
        // installing the empty name (which would resolve to a missing file).
        let parsed = Config::parse_str("color_scheme = \"\"\n");
        assert!(parsed.color_scheme.is_none());
    }

    #[test]
    fn theme_picker_prepends_default_entry() {
        // The synthetic default entry leads, then the real schemes follow in
        // the order given.
        let choices = theme_picker_choices(vec![
            "nostromo".to_string(),
            "spacedust".to_string(),
        ]);
        assert_eq!(
            choices,
            vec![
                DEFAULT_THEME_LABEL.to_string(),
                "nostromo".to_string(),
                "spacedust".to_string(),
            ]
        );
    }

    #[test]
    fn theme_picker_with_no_schemes_still_offers_default() {
        // Even with an empty schemes directory, you can always revert to
        // built-in defaults from the picker.
        assert_eq!(theme_picker_choices(Vec::new()), vec![DEFAULT_THEME_LABEL.to_string()]);
    }

    #[test]
    fn theme_picker_drops_scheme_colliding_with_default_label() {
        // A real scheme that happens to match the synthetic label is filtered
        // out so the default entry stays unambiguous (appears exactly once).
        let choices = theme_picker_choices(vec![
            DEFAULT_THEME_LABEL.to_string(),
            "yutani".to_string(),
        ]);
        assert_eq!(
            choices,
            vec![DEFAULT_THEME_LABEL.to_string(), "yutani".to_string()]
        );
    }

    #[test]
    fn scheme_for_pick_maps_default_label_to_none() {
        assert_eq!(scheme_for_pick(DEFAULT_THEME_LABEL), None);
        assert_eq!(scheme_for_pick("yutani"), Some("yutani"));
        // A real name is returned verbatim, including ones with spaces.
        assert_eq!(scheme_for_pick("My Theme"), Some("My Theme"));
    }

    #[test]
    fn scheme_value_from_pick_clears_on_default_or_empty() {
        // The default label, an empty pick, and whitespace all clear the slot.
        assert_eq!(scheme_value_from_pick(Some(DEFAULT_THEME_LABEL.to_string())), None);
        assert_eq!(scheme_value_from_pick(Some(String::new())), None);
        assert_eq!(scheme_value_from_pick(Some("   ".to_string())), None);
        assert_eq!(scheme_value_from_pick(None), None);
        // A real name is trimmed and stored.
        assert_eq!(
            scheme_value_from_pick(Some("  yutani ".to_string())),
            Some("yutani".to_string())
        );
    }

    #[test]
    fn active_scheme_uses_color_scheme_when_not_following() {
        let mut c = Config::defaults();
        c.auto_theme = false;
        c.color_scheme = Some("nostromo".to_string());
        c.light_scheme = Some("light-one".to_string());
        c.dark_scheme = Some("dark-one".to_string());
        // System appearance is ignored when not following.
        assert_eq!(c.active_scheme(false), Some("nostromo"));
        assert_eq!(c.active_scheme(true), Some("nostromo"));
    }

    #[test]
    fn active_scheme_unset_color_scheme_is_none_when_not_following() {
        let c = Config::defaults(); // auto_theme false, all schemes None
        assert_eq!(c.active_scheme(false), None);
        assert_eq!(c.active_scheme(true), None);
    }

    #[test]
    fn active_scheme_picks_slot_by_appearance_when_following() {
        let mut c = Config::defaults();
        c.auto_theme = true;
        c.light_scheme = Some("daytime".to_string());
        c.dark_scheme = Some("midnight".to_string());
        assert_eq!(c.active_scheme(false), Some("daytime"));
        assert_eq!(c.active_scheme(true), Some("midnight"));
    }

    #[test]
    fn active_scheme_following_falls_back_to_color_scheme_then_none() {
        let mut c = Config::defaults();
        c.auto_theme = true;
        c.color_scheme = Some("fallback".to_string());
        // dark_scheme set, light_scheme unset: dark uses its slot, light falls
        // back to color_scheme.
        c.dark_scheme = Some("midnight".to_string());
        assert_eq!(c.active_scheme(true), Some("midnight"));
        assert_eq!(c.active_scheme(false), Some("fallback"));
        // With no slots and no color_scheme, it's None (built-in defaults).
        c.color_scheme = None;
        c.dark_scheme = None;
        assert_eq!(c.active_scheme(true), None);
        assert_eq!(c.active_scheme(false), None);
    }

    #[test]
    fn config_auto_theme_fields_round_trip() {
        let mut c = Config::defaults();
        c.auto_theme = true;
        c.light_scheme = Some("daytime".to_string());
        c.dark_scheme = Some("midnight".to_string());
        let parsed = Config::parse_str(&c.serialize());
        assert!(parsed.auto_theme);
        assert_eq!(parsed.light_scheme.as_deref(), Some("daytime"));
        assert_eq!(parsed.dark_scheme.as_deref(), Some("midnight"));
    }

    #[test]
    fn config_auto_theme_defaults_off_with_unset_slots() {
        let d = Config::defaults();
        assert!(!d.auto_theme);
        assert!(d.light_scheme.is_none());
        assert!(d.dark_scheme.is_none());
        // A round trip of defaults preserves that.
        let parsed = Config::parse_str(&d.serialize());
        assert!(!parsed.auto_theme);
        assert!(parsed.light_scheme.is_none());
        assert!(parsed.dark_scheme.is_none());
    }

    #[test]
    fn config_light_dark_scheme_empty_parses_as_none() {
        // Explicit empty strings clear the slots, same as color_scheme.
        let parsed = Config::parse_str("light_scheme = \"\"\ndark_scheme = \"\"\n");
        assert!(parsed.light_scheme.is_none());
        assert!(parsed.dark_scheme.is_none());
    }

    #[test]
    fn config_all_theme_fields_round_trip_then_active_scheme_resolves() {
        // Every existing round-trip leaves `color_scheme` None or sets only a
        // subset. This is the realistic "user configured everything" state:
        // an explicit `color_scheme` fallback PLUS auto_theme on with both
        // slots filled. It guards against the serializer emitting one theme
        // key in a way that clobbers another, and confirms that after a full
        // serialize -> parse cycle `active_scheme` still selects the slot by
        // appearance (not the color_scheme fallback) in each direction.
        let mut c = Config::defaults();
        c.color_scheme = Some("nostromo".to_string());
        c.auto_theme = true;
        c.light_scheme = Some("daytime".to_string());
        c.dark_scheme = Some("midnight".to_string());
        let parsed = Config::parse_str(&c.serialize());
        assert_eq!(parsed.color_scheme.as_deref(), Some("nostromo"));
        assert!(parsed.auto_theme);
        assert_eq!(parsed.light_scheme.as_deref(), Some("daytime"));
        assert_eq!(parsed.dark_scheme.as_deref(), Some("midnight"));
        // Slots win over the color_scheme fallback, per appearance.
        assert_eq!(parsed.active_scheme(false), Some("daytime"));
        assert_eq!(parsed.active_scheme(true), Some("midnight"));
    }

    #[test]
    fn active_scheme_following_each_slot_falls_back_independently() {
        // The existing fallback test covers dark-set / light-unset. This pins
        // the mirror case (light-set / dark-unset) so neither branch of the
        // `if dark` slot selection silently reads the wrong field: the unset
        // direction falls back to color_scheme while the set one keeps its slot.
        let mut c = Config::defaults();
        c.auto_theme = true;
        c.color_scheme = Some("fallback".to_string());
        c.light_scheme = Some("daytime".to_string());
        assert_eq!(c.active_scheme(false), Some("daytime"));
        assert_eq!(c.active_scheme(true), Some("fallback"));
    }

    #[test]
    fn config_color_scheme_with_spaces_round_trips() {
        // Scheme names go through `toml_str_lit` so a name with spaces is
        // quoted on write and read back verbatim — same machinery as
        // font_family.
        let mut c = Config::defaults();
        c.color_scheme = Some("My Custom Theme".to_string());
        let parsed = Config::parse_str(&c.serialize());
        assert_eq!(parsed.color_scheme.as_deref(), Some("My Custom Theme"));
    }

    #[test]
    fn config_invalid_toml_falls_back_to_defaults() {
        // A document that isn't valid TOML at all (a bareword value here)
        // can't be walked key-by-key, so the whole config reverts to defaults
        // rather than guessing — the same wholesale-fallback contract the
        // palette parser has. The good `font_size` before the broken line
        // must NOT survive, proving the fallback is whole-file not per-key.
        let parsed = Config::parse_str("font_size = 18.0\nthis is not toml\n");
        let d = Config::defaults();
        assert!(approx_eq(parsed.font_size, d.font_size));
    }

    #[test]
    fn config_unknown_keys_are_ignored() {
        // Forward/backward compatibility: an unrecognized key is skipped and
        // siblings still apply. A future binary's keys in an old config (or
        // vice versa) must not blank the file out.
        let parsed = Config::parse_str("totally_made_up_key = 42\nfont_size = 14.0\n");
        assert!(approx_eq(parsed.font_size, 14.0));
    }

    #[test]
    fn config_toml_str_lit_escapes_quotes_and_backslashes() {
        // A font family containing a double-quote or backslash must be
        // escaped by `toml_str_lit` so the emitted literal stays valid TOML
        // and round-trips byte-for-byte. A naive `format!("\"{}\"")` would
        // produce a parse error here.
        let mut c = Config::defaults();
        c.font_family = Some("Weird\"Font\\Name".to_string());
        let serialized = c.serialize();
        // The whole document must still parse...
        assert!(serialized.parse::<toml::Table>().is_ok());
        // ...and the value must come back exactly as written.
        let parsed = Config::parse_str(&serialized);
        assert_eq!(parsed.font_family.as_deref(), Some("Weird\"Font\\Name"));
    }

    #[test]
    fn config_cfg_usize_rejects_negative_and_non_integer() {
        // cfg_usize underpins blur_iterations / images_memory_cap_mb. A
        // negative or non-integer value must yield None so the slot keeps its
        // default rather than panicking on the `try_from`.
        assert_eq!(cfg_usize(&toml::Value::Integer(-1)), None);
        assert_eq!(cfg_usize(&toml::Value::Float(1.0)), None);
        assert_eq!(cfg_usize(&toml::Value::Integer(7)), Some(7));
    }

    #[test]
    fn config_cfg_u64_rejects_negative() {
        // cfg_u64 underpins images_max_pixels / images_decode_timeout_ms; a
        // negative literal must be refused, not wrapped to a huge unsigned.
        assert_eq!(cfg_u64(&toml::Value::Integer(-5)), None);
        assert_eq!(cfg_u64(&toml::Value::Integer(5)), Some(5));
    }

    #[test]
    fn config_images_memory_cap_accepts_integer() {
        // images_memory_cap_mb is a plain usize slot; a bare integer must
        // apply (no clamp), confirming the cfg_usize path is wired up.
        let parsed = Config::parse_str("images_memory_cap_mb = 512\n");
        assert_eq!(parsed.images_memory_cap_mb, 512);
    }

    /// Build a row of cells from a string for URL-detection tests. Each
    /// char becomes one cell with default style.
    fn cells_from_str(s: &str) -> Vec<style::Cell> {
        s.chars()
            .map(|ch| style::Cell::new(ch, style::Style::new()))
            .collect()
    }

    #[test]
    fn url_detected_when_cursor_inside() {
        let row = cells_from_str("see https://example.com today");
        // Cursor on the 'e' inside "example".
        let (s, e, url) = find_url_in_cells(&row, 12).expect("should find url");
        assert_eq!(s, 4);
        assert_eq!(e, 22);
        assert_eq!(url, "https://example.com");
    }

    #[test]
    fn url_detected_at_start_of_prefix() {
        let row = cells_from_str("see https://example.com today");
        // Cursor on the leading 'h' of "https".
        let (s, e, url) = find_url_in_cells(&row, 4).expect("should find url");
        assert_eq!(s, 4);
        assert_eq!(e, 22);
        assert_eq!(url, "https://example.com");
    }

    #[test]
    fn url_detected_at_end_of_url() {
        let row = cells_from_str("see https://example.com today");
        // Cursor on the trailing 'm' of ".com".
        let (s, e, _) = find_url_in_cells(&row, 22).expect("should find url");
        assert_eq!(s, 4);
        assert_eq!(e, 22);
    }

    #[test]
    fn http_scheme_also_detected() {
        let row = cells_from_str("http://foo.bar/baz");
        let (s, e, url) = find_url_in_cells(&row, 0).expect("should find url");
        assert_eq!(s, 0);
        assert_eq!(e, row.len() - 1);
        assert_eq!(url, "http://foo.bar/baz");
    }

    #[test]
    fn returns_none_when_cursor_on_whitespace() {
        let row = cells_from_str("see https://example.com today");
        // Cursor on the space at index 3 (between "see" and "https").
        assert!(find_url_in_cells(&row, 3).is_none());
    }

    #[test]
    fn returns_none_when_cursor_outside_url() {
        let row = cells_from_str("see https://example.com today");
        // Cursor on 's' in "see" — outside the URL run.
        assert!(find_url_in_cells(&row, 0).is_none());
        // Cursor on 't' in "today" — past the URL.
        assert!(find_url_in_cells(&row, 24).is_none());
    }

    #[test]
    fn returns_none_for_plain_text() {
        let row = cells_from_str("no url anywhere here");
        for c in 0..row.len() {
            assert!(find_url_in_cells(&row, c).is_none(), "col {c}");
        }
    }

    #[test]
    fn trailing_sentence_punctuation_is_stripped() {
        let row = cells_from_str("visit https://example.com.");
        let (_, e, url) = find_url_in_cells(&row, 10).expect("should find url");
        // Trailing '.' should not be part of the URL.
        assert_eq!(url, "https://example.com");
        assert_eq!(row[e].ch, 'm');
    }

    #[test]
    fn trailing_paren_is_stripped() {
        let row = cells_from_str("(see https://example.com)");
        let (_, _, url) = find_url_in_cells(&row, 10).expect("should find url");
        assert_eq!(url, "https://example.com");
    }

    #[test]
    fn is_safe_url_allows_known_schemes() {
        for u in [
            "https://x/",
            "http://x/",
            "mailto:a@b.com",
            "file:///etc/hosts",
            "ftp://host/f",
            "ssh://host",
            "  HTTPS://Upper/  ",
        ] {
            assert!(is_safe_url(u), "{u} should be safe");
        }
    }

    #[test]
    fn is_safe_url_rejects_dangerous_or_bare() {
        for u in [
            "javascript:alert(1)",
            "data:text/html,<script>",
            "vbscript:x",
            "not a url",
            "example.com",
        ] {
            assert!(!is_safe_url(u), "{u} should be rejected");
        }
    }

    #[test]
    fn osc8_link_preferred_over_heuristic_anchor_text() {
        // Anchor text "click here" links to a different target via OSC 8.
        // find_url_at must return the OSC 8 target, not parse the visible text.
        let mut t = terminal::Terminal::new(80, 24, 100);
        t.feed("\x1b]8;;https://real.example/path\x07click here\x1b]8;;\x07");
        let abs = t.visual_to_abs_line(0);
        let hu = find_url_at(&t, abs, 2).expect("link under 'click'");
        assert_eq!(hu.url, "https://real.example/path");
        assert_eq!(hu.start_col(), 0);
        assert_eq!(hu.end_col(), "click here".len() - 1);
    }

    #[test]
    fn osc8_link_span_stops_at_unlinked_cells() {
        let mut t = terminal::Terminal::new(80, 24, 100);
        // "pre " unlinked, "LINK" linked, " post" unlinked.
        t.feed("pre \x1b]8;;https://x/\x07LINK\x1b]8;;\x07 post");
        let abs = t.visual_to_abs_line(0);
        let hu = find_osc8_link_at(&t, abs, 5).expect("link under LINK");
        assert_eq!(hu.start_col(), 4);
        assert_eq!(hu.end_col(), 7);
        assert_eq!(hu.url, "https://x/");
        // A cell in "pre " has no OSC 8 link.
        assert!(find_osc8_link_at(&t, abs, 1).is_none());
    }

    #[test]
    fn osc8_id_siblings_cohighlight() {
        // Two non-contiguous spans share `id=grp` + URI: hovering either must
        // return segments covering BOTH runs so they underline together.
        let mut t = terminal::Terminal::new(80, 24, 100);
        t.feed("\x1b]8;id=grp;https://x/\x07AB\x1b]8;;\x07 mid \x1b]8;id=grp;https://x/\x07CD\x1b]8;;\x07");
        let abs = t.visual_to_abs_line(0);
        // "AB" at cols 0..1; " mid " at 2..6; "CD" at cols 7..8.
        let hu = find_osc8_link_at(&t, abs, 0).expect("link under first span");
        assert_eq!(hu.url, "https://x/");
        let mut spans: Vec<(usize, usize)> =
            hu.segments.iter().map(|s| (s.start_col, s.end_col)).collect();
        spans.sort();
        assert_eq!(spans, vec![(0, 1), (7, 8)], "both id=grp spans co-highlight");
        // Hovering the second span resolves to the identical set.
        let hu2 = find_osc8_link_at(&t, abs, 7).expect("link under second span");
        assert_eq!(hu, hu2);
    }

    #[test]
    fn osc8_id_three_siblings_all_cohighlight() {
        // Three non-contiguous spans share one id: hovering any of them must
        // return all three segments.
        let mut t = terminal::Terminal::new(80, 24, 100);
        t.feed("\x1b]8;id=g;https://x/\x07A\x1b]8;;\x07 \x1b]8;id=g;https://x/\x07B\x1b]8;;\x07 \x1b]8;id=g;https://x/\x07C\x1b]8;;\x07");
        let abs = t.visual_to_abs_line(0);
        // "A" col 0, "B" col 2, "C" col 4.
        let hu = find_osc8_link_at(&t, abs, 0).expect("link under first span");
        let mut spans: Vec<(usize, usize)> =
            hu.segments.iter().map(|s| (s.start_col, s.end_col)).collect();
        spans.sort();
        assert_eq!(spans, vec![(0, 0), (2, 2), (4, 4)], "all three spans co-highlight");
        // Hovering the middle and last spans yields the identical set.
        assert_eq!(find_osc8_link_at(&t, abs, 2).unwrap(), hu);
        assert_eq!(find_osc8_link_at(&t, abs, 4).unwrap(), hu);
    }

    #[test]
    fn osc8_id_siblings_on_different_rows_cohighlight() {
        // Two spans share one id but land on different visible rows (a newline
        // separates them). Hovering either must return one segment per row.
        let mut t = make_terminal(5, 20);
        t.feed("\x1b]8;id=g;https://x/\x07AB\x1b]8;;\x07\r\n\x1b]8;id=g;https://x/\x07CD\x1b]8;;\x07");
        let abs0 = t.visual_to_abs_line(0);
        let abs1 = t.visual_to_abs_line(1);
        let hu = find_osc8_link_at(&t, abs0, 0).expect("hover first row span");
        assert_eq!(hu.url, "https://x/");
        let mut segs: Vec<(isize, usize, usize)> = hu
            .segments
            .iter()
            .map(|s| (s.abs_line, s.start_col, s.end_col))
            .collect();
        segs.sort();
        assert_eq!(segs, vec![(abs0, 0, 1), (abs1, 0, 1)], "siblings on two rows co-highlight");
        // Hovering the second-row span resolves to the same set.
        assert_eq!(find_osc8_link_at(&t, abs1, 0).unwrap(), hu);
    }

    #[test]
    fn osc8_anonymous_spans_do_not_cohighlight() {
        // No `id=`: each open is a distinct link, so hovering the first span
        // highlights only its own contiguous run, not the later same-URI span.
        let mut t = terminal::Terminal::new(80, 24, 100);
        t.feed("\x1b]8;;https://x/\x07AB\x1b]8;;\x07 \x1b]8;;https://x/\x07CD\x1b]8;;\x07");
        let abs = t.visual_to_abs_line(0);
        let hu = find_osc8_link_at(&t, abs, 0).expect("first span");
        assert_eq!(hu.segments.len(), 1, "anonymous links don't group");
        assert_eq!(hu.segments[0].start_col, 0);
        assert_eq!(hu.segments[0].end_col, 1);
    }

    #[test]
    fn heuristic_still_works_without_osc8() {
        let mut t = terminal::Terminal::new(80, 24, 100);
        t.feed("see https://example.com today");
        let abs = t.visual_to_abs_line(0);
        let hu = find_url_at(&t, abs, 12).expect("heuristic url");
        assert_eq!(hu.url, "https://example.com");
    }

    #[test]
    fn find_osc8_link_at_none_on_unlinked_cell() {
        // A grid with no OSC 8 link anywhere yields None for every cell.
        let mut t = terminal::Terminal::new(80, 24, 100);
        t.feed("just plain text");
        let abs = t.visual_to_abs_line(0);
        assert!(find_osc8_link_at(&t, abs, 0).is_none());
        assert!(find_osc8_link_at(&t, abs, 5).is_none());
    }

    #[test]
    fn find_osc8_link_at_out_of_bounds_col_is_none() {
        // A col past the row width must not panic and must return None.
        let mut t = terminal::Terminal::new(80, 24, 100);
        t.feed("\x1b]8;;https://x/\x07AB\x1b]8;;\x07");
        let abs = t.visual_to_abs_line(0);
        assert!(find_osc8_link_at(&t, abs, 10_000).is_none());
    }

    #[test]
    fn find_osc8_link_at_extends_across_wrapped_rows() {
        // A single OSC 8 link whose anchor text wraps across rows must resolve
        // to one span covering both rows, whether hovered on the first or the
        // continuation row. Grid is 10 cols; 14 linked glyphs wrap to row 1.
        let mut t = make_terminal(5, 10);
        t.feed("\x1b]8;;https://wrap/target\x07ABCDEFGHIJKLMN\x1b]8;;\x07");
        // Row 0 is full (cols 0..9), row 1 holds the remaining 4 (cols 0..3).
        let from_first = find_osc8_link_at(&t, 0, 2).expect("hover first row");
        let from_tail = find_osc8_link_at(&t, 1, 1).expect("hover continuation row");
        assert_eq!(from_first, from_tail, "both hovers resolve to one span");
        assert_eq!(from_first.start_abs_line(), 0);
        assert_eq!(from_first.start_col(), 0);
        assert_eq!(from_first.end_abs_line(), 1);
        assert_eq!(from_first.end_col(), 3, "14 glyphs over 10 cols end at col 3 of row 1");
        assert_eq!(from_first.url, "https://wrap/target");
    }

    #[test]
    fn url_only_in_punctuation_run_rejected() {
        // A `).` after the prefix would leave an empty host. Make sure we
        // don't return a URL that's just the scheme.
        let row = cells_from_str("https://.");
        assert!(find_url_in_cells(&row, 0).is_none());
    }

    #[test]
    fn empty_row_returns_none() {
        let row: Vec<style::Cell> = Vec::new();
        assert!(find_url_in_cells(&row, 0).is_none());
    }

    #[test]
    fn out_of_bounds_col_returns_none() {
        let row = cells_from_str("https://example.com");
        assert!(find_url_in_cells(&row, row.len()).is_none());
        assert!(find_url_in_cells(&row, row.len() + 5).is_none());
    }

    #[test]
    fn url_with_path_and_query() {
        let row = cells_from_str("https://example.com/a/b?q=1&x=2");
        let (_, _, url) = find_url_in_cells(&row, 10).expect("should find url");
        assert_eq!(url, "https://example.com/a/b?q=1&x=2");
    }

    #[test]
    fn returns_first_url_when_multiple_share_a_run() {
        // A run with no whitespace can theoretically have two prefixes
        // concatenated — make sure we return the earlier (and longer https)
        // start, not the embedded http.
        let row = cells_from_str("https://foo");
        let (s, _, url) = find_url_in_cells(&row, 0).expect("should find url");
        assert_eq!(s, 0);
        assert_eq!(url, "https://foo");
    }

    // ---- additional edge-case tests --------------------------------------

    #[test]
    fn url_at_very_first_column_with_cursor_on_last_char() {
        // URL fills the entire row; cursor sits on the final cell.
        let row = cells_from_str("https://example.com");
        let last = row.len() - 1;
        let (s, e, url) = find_url_in_cells(&row, last).expect("should find url");
        assert_eq!(s, 0);
        assert_eq!(e, last);
        assert_eq!(url, "https://example.com");
    }

    #[test]
    fn url_at_very_last_column_of_row() {
        // No trailing whitespace — URL ends exactly at the right edge.
        let row = cells_from_str("see https://example.com");
        let last = row.len() - 1;
        let (s, e, url) = find_url_in_cells(&row, last).expect("should find url");
        assert_eq!(s, 4);
        assert_eq!(e, last);
        assert_eq!(url, "https://example.com");
    }

    #[test]
    fn tab_delimits_url_run() {
        // Tabs are whitespace; the URL between two tabs is detected.
        let row = cells_from_str("a\thttps://example.com\tb");
        // Cursor on the 'x' of "example".
        let (s, e, url) = find_url_in_cells(&row, 10).expect("should find url");
        assert_eq!(s, 2);
        assert_eq!(e, 20);
        assert_eq!(url, "https://example.com");
    }

    #[test]
    fn very_short_url_with_single_char_host() {
        // "http://a" is the shortest legal http URL we accept (scheme + one
        // host char). Make sure the scheme-only guard does not over-reject.
        let row = cells_from_str("http://a");
        let (s, e, url) = find_url_in_cells(&row, 7).expect("should find url");
        assert_eq!(s, 0);
        assert_eq!(e, 7);
        assert_eq!(url, "http://a");
    }

    #[test]
    fn url_with_fragment_is_preserved() {
        let row = cells_from_str("https://example.com/page#section-2");
        let (_, _, url) = find_url_in_cells(&row, 10).expect("should find url");
        assert_eq!(url, "https://example.com/page#section-2");
    }

    #[test]
    fn url_with_percent_encoded_chars_is_preserved() {
        let row = cells_from_str("https://example.com/a%20b%2Fc");
        let (_, _, url) = find_url_in_cells(&row, 10).expect("should find url");
        assert_eq!(url, "https://example.com/a%20b%2Fc");
    }

    #[test]
    fn single_slash_scheme_is_not_a_url() {
        // "http:/foo" — missing the second slash. Must not match.
        let row = cells_from_str("http:/foo.bar");
        for c in 0..row.len() {
            assert!(find_url_in_cells(&row, c).is_none(), "col {c}");
        }
    }

    #[test]
    fn single_slash_https_scheme_is_not_a_url() {
        let row = cells_from_str("https:/example.com");
        for c in 0..row.len() {
            assert!(find_url_in_cells(&row, c).is_none(), "col {c}");
        }
    }

    #[test]
    fn unicode_letter_adjacent_to_url_is_part_of_run() {
        // Non-whitespace unicode glues onto the run, but the prefix scan
        // still locates "https://" further in and produces a clean URL.
        // (Whether we strip the leading unicode is a behavior choice — the
        // function happens to skip it because url_start_col jumps to where
        // the prefix actually matched.)
        let row = cells_from_str("→https://example.com");
        // Cursor on the 'x' of "example".
        let (s, _, url) = find_url_in_cells(&row, 10).expect("should find url");
        // The leading arrow is NOT part of the URL — the prefix scan starts
        // at column 1.
        assert_eq!(s, 1);
        assert_eq!(url, "https://example.com");
    }

    #[test]
    fn unicode_letter_after_url_is_part_of_url() {
        // Trailing non-ASCII letters are not whitespace and are not in the
        // sentence-punctuation strip list, so they ride along as part of
        // the URL. We document the behavior here so it changes deliberately.
        let row = cells_from_str("https://例え.jp");
        let (_, _, url) = find_url_in_cells(&row, 0).expect("should find url");
        assert_eq!(url, "https://例え.jp");
    }

    #[test]
    fn two_concatenated_urls_in_one_run_return_combined_span() {
        // Pathological input: two URLs glued with no whitespace. The function
        // is whitespace-delimited, so it returns the whole run starting at
        // the first prefix. Cursor on the first URL gets the combined span.
        // (Documenting current behavior — splitting on a second "http(s)://"
        // would require extra logic we don't ship.)
        let row = cells_from_str("https://a.comhttps://b.com");
        let (s, e, url) = find_url_in_cells(&row, 2).expect("should find url");
        assert_eq!(s, 0);
        assert_eq!(e, row.len() - 1);
        assert_eq!(url, "https://a.comhttps://b.com");
    }

    #[test]
    fn cursor_on_stripped_trailing_punctuation_returns_none() {
        // "https://example.com." with cursor on the '.' — the dot is
        // stripped from the URL, so the cursor is "past" url_end_col and
        // we report no hit. Hovering exactly on the trailing dot is not a
        // URL hover.
        let row = cells_from_str("https://example.com.");
        let dot_col = row.len() - 1;
        assert_eq!(row[dot_col].ch, '.');
        assert!(find_url_in_cells(&row, dot_col).is_none());
    }

    #[test]
    fn quoted_url_strips_trailing_quote() {
        let row = cells_from_str("\"https://example.com\"");
        let (_, _, url) = find_url_in_cells(&row, 10).expect("should find url");
        assert_eq!(url, "https://example.com");
    }

    #[test]
    fn bracketed_url_strips_trailing_bracket() {
        let row = cells_from_str("[https://example.com]");
        let (_, _, url) = find_url_in_cells(&row, 10).expect("should find url");
        assert_eq!(url, "https://example.com");
    }

    #[test]
    fn multiple_trailing_punctuation_all_stripped() {
        // "...)!" should all peel off, leaving the bare URL.
        let row = cells_from_str("https://example.com.)!");
        let (_, e, url) = find_url_in_cells(&row, 10).expect("should find url");
        assert_eq!(url, "https://example.com");
        assert_eq!(row[e].ch, 'm');
    }

    #[test]
    fn uppercase_scheme_is_not_matched() {
        // Prefix match is case-sensitive — "HTTPS://" is not recognized.
        // Documenting current behavior (browsers accept it, we don't).
        let row = cells_from_str("HTTPS://example.com");
        for c in 0..row.len() {
            assert!(find_url_in_cells(&row, c).is_none(), "col {c}");
        }
    }

    #[test]
    fn url_with_port_number() {
        let row = cells_from_str("http://localhost:8080/path");
        let (_, _, url) = find_url_in_cells(&row, 10).expect("should find url");
        assert_eq!(url, "http://localhost:8080/path");
    }

    #[test]
    fn cursor_on_whitespace_tab_returns_none() {
        let row = cells_from_str("a\thttps://example.com");
        // Cursor on the tab itself.
        assert!(find_url_in_cells(&row, 1).is_none());
    }

    // ---- wrap-aware URL detection (find_url_at / build_wrapped_line) ----
    //
    // The autowrap heuristic looks at the cell grid: a row joins its
    // predecessor only when *both* the prev row's last col and the cur row's
    // first col are non-whitespace. A real `terminal::Terminal` is required
    // here so we exercise the actual grid layout autowrap produces.

    /// 20-col x rows terminal with a generous scrollback budget. Default
    /// autowrap on, no DECLRMM margins — matches the conditions a hovered
    /// shell URL sees.
    fn make_terminal(rows: usize, cols: usize) -> terminal::Terminal {
        terminal::Terminal::new(cols, rows, 1024)
    }

    #[test]
    fn find_url_at_wrapped_url_resolves_from_first_row() {
        // 39-char URL on a 20-col grid: row 0 gets cols 0..19
        // ("https://example.com/"), row 1 gets cols 0..18
        // ("very-long-path/here"). The join condition holds because
        // row 0's last cell ('/') and row 1's first cell ('v') are both
        // non-whitespace.
        let mut t = make_terminal(5, 20);
        let url = "https://example.com/very-long-path/here";
        assert_eq!(url.len(), 39);
        t.feed(url);

        let hover = find_url_at(&t, 0, 5).expect("URL should be found from first row");
        assert_eq!(hover.start_abs_line(), 0);
        assert_eq!(hover.end_abs_line(), 1);
        assert_eq!(hover.start_col(), 0);
        assert_eq!(hover.end_col(), 18, "39 chars over 20 cols ends at col 18 of row 1");
        assert_eq!(hover.url, url);
    }

    #[test]
    fn find_url_at_wrapped_url_resolves_from_continuation_row() {
        // Same wrapped URL — cursor on the continuation row must resolve to
        // the same span. This is the regression: previously hovering the
        // tail row found nothing because the row in isolation has no scheme.
        let mut t = make_terminal(5, 20);
        let url = "https://example.com/very-long-path/here";
        t.feed(url);

        let from_first = find_url_at(&t, 0, 5).expect("first-row hover");
        let from_tail = find_url_at(&t, 1, 5).expect("tail-row hover should also resolve");
        assert_eq!(from_first, from_tail);
    }

    #[test]
    fn find_url_at_does_not_join_when_boundary_is_whitespace() {
        // Row 0: "https://example.com " (19 + 1 space = 20 cols exactly).
        // Row 1: "extra-text" starting at col 0. Row 0's last cell is a
        // space, so the join is suppressed and the URL stays on row 0
        // without sucking up "extra-text".
        let mut t = make_terminal(5, 20);
        t.feed("https://example.com extra-text");

        let hover = find_url_at(&t, 0, 5).expect("URL on row 0");
        assert_eq!(hover.start_abs_line(), 0);
        assert_eq!(hover.end_abs_line(), 0);
        assert_eq!(hover.start_col(), 0);
        assert_eq!(hover.end_col(), 18);
        assert_eq!(hover.url, "https://example.com");
        assert!(
            !hover.url.contains("extra-text"),
            "whitespace at boundary must break the wrap-join"
        );
    }

    #[test]
    fn find_url_at_caps_continuation_walk() {
        // Feed many rows of solid non-whitespace text with a URL at the
        // top. Without the URL_WRAP_MAX_ROWS cap, build_wrapped_line would
        // walk every continuous row in scrollback. With the cap, the walk
        // is bounded; the test must complete quickly and return *some*
        // URL — we don't pin the exact length because that's the heuristic's
        // discretion.
        let mut t = make_terminal(5, 20);
        // 50 rows worth of solid non-whitespace, starting with the scheme.
        let mut s = String::from("https://example.com/");
        // 49 more rows of 20 'x' each — all non-whitespace, so every
        // boundary qualifies for the join (until the cap kicks in).
        for _ in 0..49 {
            s.push_str(&"x".repeat(20));
        }
        t.feed(&s);

        // The first row of the URL is now somewhere in scrollback. Find it
        // by scanning abs_line 0..scrollback_len + rows for the row that
        // starts with 'h'.
        let total_lines = t.scrollback_len() as isize + t.rows as isize;
        let mut start_abs = None;
        for abs in 0..total_lines {
            if let Some(row) = t.line_at(abs) {
                if row.first().map(|c| c.ch) == Some('h') {
                    start_abs = Some(abs);
                    break;
                }
            }
        }
        let start_abs = start_abs.expect("URL start row should exist");

        let hover = find_url_at(&t, start_abs, 0).expect("should resolve to some URL");
        assert_eq!(hover.start_abs_line(), start_abs);
        assert!(hover.url.starts_with("https://example.com/"));
        // Cap is URL_WRAP_MAX_ROWS rows past the start; bound length
        // generously to confirm we didn't walk all 50 rows.
        let max_len = (URL_WRAP_MAX_ROWS + 1) * 20;
        assert!(
            hover.url.len() <= max_len,
            "URL length {} exceeded wrap cap (max {})",
            hover.url.len(),
            max_len
        );
    }

    #[test]
    fn find_url_at_single_row_url_still_works() {
        // Regression: the wrap-aware path must not break the common
        // single-row case. 60 cols is wide enough that nothing wraps.
        let mut t = make_terminal(5, 60);
        t.feed("https://example.com more text here");

        let hover = find_url_at(&t, 0, 10).expect("single-row URL");
        assert_eq!(hover.start_abs_line(), 0);
        assert_eq!(hover.end_abs_line(), 0);
        assert_eq!(hover.start_col(), 0);
        assert_eq!(hover.end_col(), 18);
        assert_eq!(hover.url, "https://example.com");
    }

    //
    // apply_glow_config — pure copy from `Config` onto a `Glow`. Building
    // a real `Glow` needs a headless wgpu device + a scene texture view;
    // we follow the same skip-on-no-adapter pattern as `images::tests`
    // so CI without a GPU just sits these out instead of failing.
    //

    /// Build a headless `Glow` for use in tests. Returns `None` if no
    /// adapter is available (CI without a GPU); callers should bail
    /// silently rather than fail the test, matching the precedent in
    /// `src/images.rs`.
    fn try_make_test_glow() -> Option<(wgpu::Device, wgpu::Queue, renderer::glow::Glow)> {
        let instance = wgpu::Instance::default();
        let adapter = pollster::block_on(
            instance.request_adapter(&wgpu::RequestAdapterOptions::default()),
        )?;
        let (device, queue) = pollster::block_on(
            adapter.request_device(&wgpu::DeviceDescriptor::default(), None),
        )
        .ok()?;
        let format = wgpu::TextureFormat::Rgba8UnormSrgb;
        // Dummy scene texture — Glow only needs a TextureView at the
        // bound size; the contents don't matter for the field-copy path.
        let scene_tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("test glow scene"),
            size: wgpu::Extent3d { width: 16, height: 16, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let scene_view = scene_tex.create_view(&wgpu::TextureViewDescriptor::default());
        let pipelines = renderer::glow::GlowPipelines::new(&device, format);
        let glow = renderer::glow::Glow::new(&device, &pipelines, 16, 16, &scene_view);
        Some((device, queue, glow))
    }

    /// Build a `Config` whose every `glow_*` slot differs from the
    /// `defaults()` value so a missing assignment in `apply_glow_config`
    /// shows up as a stale default in the resulting `Glow`.
    fn non_default_glow_config() -> Config {
        let mut c = Config::defaults();
        c.glow_match_brightness = true;
        c.glow_match_bright_ansi = true;
        c.glow_match_foreground = true;
        c.glow_threshold = 0.42;
        c.glow_intensity = 1.7;
        c.glow_softness = 0.33;
        c.glow_hue_tolerance_deg = 27.5;
        c.glow_fg_tolerance = 0.21;
        c.glow_scanlines = true;
        c.glow_scanline_strength = 0.55;
        c.glow_scanline_period = 6.0;
        c.glow_scanlines_content = true;
        c.glow_scanlines_content_strength = 0.66;
        c.glow_scanline_color_bright = [0.9, 0.7, 0.5, 1.0];
        c.glow_scanline_color_dark = [0.1, 0.2, 0.3, 1.0];
        c.glow_scanlines_content_attenuation = 0.77;
        c.glow_iterations = 5;
        c
    }

    #[test]
    fn apply_glow_config_copies_every_field_from_config() {
        let Some((_d, _q, mut g)) = try_make_test_glow() else {
            eprintln!("skipping: no GPU adapter");
            return;
        };
        let c = non_default_glow_config();
        apply_glow_config(&mut g, &c, &palette::GlowOverrides::NONE);

        // Every config slot listed in the task spec must appear on the
        // Glow. A missing line in `apply_glow_config` shows up here as a
        // stale default (`non_default_glow_config` differs from
        // `Config::defaults` on every field below).
        assert_eq!(g.match_brightness, c.glow_match_brightness);
        assert_eq!(g.match_bright_ansi, c.glow_match_bright_ansi);
        assert_eq!(g.match_foreground, c.glow_match_foreground);
        assert!(approx_eq(g.threshold, c.glow_threshold));
        assert!(approx_eq(g.intensity, c.glow_intensity));
        assert!(approx_eq(g.softness, c.glow_softness));
        assert!(approx_eq(g.hue_tolerance, c.glow_hue_tolerance_deg));
        assert!(approx_eq(g.fg_tolerance, c.glow_fg_tolerance));
        assert_eq!(g.match_scanlines, c.glow_scanlines);
        assert!(approx_eq(g.scanline_strength, c.glow_scanline_strength));
        assert!(approx_eq(g.scanline_period, c.glow_scanline_period));
        assert_eq!(g.match_content_scanlines, c.glow_scanlines_content);
        assert!(approx_eq(
            g.content_scanline_strength,
            c.glow_scanlines_content_strength,
        ));
        assert_eq!(g.scanline_color_bright, c.glow_scanline_color_bright);
        assert_eq!(g.scanline_color_dark, c.glow_scanline_color_dark);
        assert!(approx_eq(
            g.content_scanline_attenuation,
            c.glow_scanlines_content_attenuation,
        ));
        assert_eq!(g.iterations, c.glow_iterations);
    }

    #[test]
    fn apply_glow_config_overwrites_prior_state() {
        // Reload-equivalent: apply once with non-default values, then
        // apply again with `Config::defaults()`. The Glow must end up
        // matching defaults — i.e. the second apply replaces every
        // field, no "sticky" remnants from the first pass.
        let Some((_d, _q, mut g)) = try_make_test_glow() else {
            eprintln!("skipping: no GPU adapter");
            return;
        };
        apply_glow_config(&mut g, &non_default_glow_config(), &palette::GlowOverrides::NONE);
        let defaults = Config::defaults();
        apply_glow_config(&mut g, &defaults, &palette::GlowOverrides::NONE);

        assert_eq!(g.match_brightness, defaults.glow_match_brightness);
        assert_eq!(g.match_bright_ansi, defaults.glow_match_bright_ansi);
        assert_eq!(g.match_foreground, defaults.glow_match_foreground);
        assert!(approx_eq(g.threshold, defaults.glow_threshold));
        assert!(approx_eq(g.intensity, defaults.glow_intensity));
        assert!(approx_eq(g.softness, defaults.glow_softness));
        assert_eq!(g.match_scanlines, defaults.glow_scanlines);
        assert_eq!(g.match_content_scanlines, defaults.glow_scanlines_content);
        assert_eq!(g.iterations, defaults.glow_iterations);
    }

    #[test]
    fn apply_glow_config_clamps_iterations_above_max() {
        // `Config::parse_str` clamps `glow_iterations` on the way in,
        // but a hand-mutated `Config` (or a future code path that sets
        // the field directly) shouldn't be able to push the Glow's
        // dual-Kawase chain past `MAX_ITERATIONS`. `apply_glow_config`
        // re-clamps to enforce that.
        let Some((_d, _q, mut g)) = try_make_test_glow() else {
            eprintln!("skipping: no GPU adapter");
            return;
        };
        let mut c = Config::defaults();
        c.glow_iterations = renderer::glow::MAX_ITERATIONS * 10;
        apply_glow_config(&mut g, &c, &palette::GlowOverrides::NONE);
        assert_eq!(g.iterations, renderer::glow::MAX_ITERATIONS);
    }

    #[test]
    fn apply_glow_config_clamps_iterations_below_one() {
        // Lower bound: dual-Kawase needs at least one down/up pass to
        // produce a halo, so 0 (or anything below) must clamp up to 1.
        let Some((_d, _q, mut g)) = try_make_test_glow() else {
            eprintln!("skipping: no GPU adapter");
            return;
        };
        let mut c = Config::defaults();
        c.glow_iterations = 0;
        apply_glow_config(&mut g, &c, &palette::GlowOverrides::NONE);
        assert_eq!(g.iterations, 1);
    }

    #[test]
    fn apply_glow_config_iterations_at_max_unchanged() {
        // Boundary: a value sitting exactly at `MAX_ITERATIONS` must
        // pass through unmodified — the clamp is inclusive.
        let Some((_d, _q, mut g)) = try_make_test_glow() else {
            eprintln!("skipping: no GPU adapter");
            return;
        };
        let mut c = Config::defaults();
        c.glow_iterations = renderer::glow::MAX_ITERATIONS;
        apply_glow_config(&mut g, &c, &palette::GlowOverrides::NONE);
        assert_eq!(g.iterations, renderer::glow::MAX_ITERATIONS);
    }

    //
    // theme_overrides_glow tiebreaker: when set, `Some(_)` slots on the
    // scheme's `GlowOverrides` win over the matching `Config` field; when
    // clear, the config always wins regardless of override state. These
    // pin both branches end-to-end through `apply_glow_config` and the
    // matching `effective_skip_primary_bg` helper.
    //

    /// Build a `GlowOverrides` whose every field is `Some(_)` and
    /// distinct from the values produced by `non_default_glow_config`.
    /// A passing override-wins test then proves each slot came through
    /// the override path rather than the config path.
    fn fully_populated_overrides() -> palette::GlowOverrides {
        palette::GlowOverrides {
            match_brightness: Some(false),
            match_bright_ansi: Some(false),
            match_foreground: Some(false),
            threshold: Some(0.11),
            intensity: Some(0.22),
            softness: Some(0.13),
            hue_tolerance_deg: Some(91.0),
            fg_tolerance: Some(1.1),
            iterations: Some(3),
            scanlines: Some(false),
            scanline_strength: Some(0.14),
            scanline_period: Some(7.5),
            scanlines_content: Some(false),
            scanlines_content_strength: Some(0.17),
            scanline_color_bright: Some([0.1, 0.2, 0.3, 1.0]),
            scanline_color_dark: Some([0.4, 0.5, 0.6, 1.0]),
            scanlines_skip_primary_bg: Some(true),
            scanlines_content_attenuation: Some(0.19),
        }
    }

    #[test]
    fn apply_glow_config_ignores_overrides_when_flag_off() {
        // Default `theme_overrides_glow = false`: even a fully-populated
        // `GlowOverrides` must be ignored. Glow ends up matching config
        // verbatim — same outcome as the existing
        // `apply_glow_config_copies_every_field_from_config` test.
        let Some((_d, _q, mut g)) = try_make_test_glow() else {
            eprintln!("skipping: no GPU adapter");
            return;
        };
        let mut c = non_default_glow_config();
        c.theme_overrides_glow = false;
        let o = fully_populated_overrides();
        apply_glow_config(&mut g, &c, &o);

        assert_eq!(g.match_brightness, c.glow_match_brightness);
        assert_eq!(g.match_bright_ansi, c.glow_match_bright_ansi);
        assert_eq!(g.match_foreground, c.glow_match_foreground);
        assert!(approx_eq(g.threshold, c.glow_threshold));
        assert!(approx_eq(g.intensity, c.glow_intensity));
        assert!(approx_eq(g.softness, c.glow_softness));
        assert!(approx_eq(g.hue_tolerance, c.glow_hue_tolerance_deg));
        assert!(approx_eq(g.fg_tolerance, c.glow_fg_tolerance));
        assert_eq!(g.match_scanlines, c.glow_scanlines);
        assert!(approx_eq(g.scanline_strength, c.glow_scanline_strength));
        assert!(approx_eq(g.scanline_period, c.glow_scanline_period));
        assert_eq!(g.match_content_scanlines, c.glow_scanlines_content);
        assert!(approx_eq(
            g.content_scanline_strength,
            c.glow_scanlines_content_strength,
        ));
        assert_eq!(g.scanline_color_bright, c.glow_scanline_color_bright);
        assert_eq!(g.scanline_color_dark, c.glow_scanline_color_dark);
        assert!(approx_eq(
            g.content_scanline_attenuation,
            c.glow_scanlines_content_attenuation,
        ));
        assert_eq!(g.iterations, c.glow_iterations);
    }

    #[test]
    fn apply_glow_config_overrides_win_when_flag_on() {
        // Flip the tiebreaker on: every `Some(_)` override now wins
        // over the corresponding config field. Pin each slot against
        // the override value (not the config value) so a regression
        // that wires a slot to the wrong source shows up here.
        let Some((_d, _q, mut g)) = try_make_test_glow() else {
            eprintln!("skipping: no GPU adapter");
            return;
        };
        let mut c = non_default_glow_config();
        c.theme_overrides_glow = true;
        let o = fully_populated_overrides();
        apply_glow_config(&mut g, &c, &o);

        assert_eq!(g.match_brightness, o.match_brightness.unwrap());
        assert_eq!(g.match_bright_ansi, o.match_bright_ansi.unwrap());
        assert_eq!(g.match_foreground, o.match_foreground.unwrap());
        assert!(approx_eq(g.threshold, o.threshold.unwrap()));
        assert!(approx_eq(g.intensity, o.intensity.unwrap()));
        assert!(approx_eq(g.softness, o.softness.unwrap()));
        assert!(approx_eq(g.hue_tolerance, o.hue_tolerance_deg.unwrap()));
        assert!(approx_eq(g.fg_tolerance, o.fg_tolerance.unwrap()));
        assert_eq!(g.match_scanlines, o.scanlines.unwrap());
        assert!(approx_eq(g.scanline_strength, o.scanline_strength.unwrap()));
        assert!(approx_eq(g.scanline_period, o.scanline_period.unwrap()));
        assert_eq!(g.match_content_scanlines, o.scanlines_content.unwrap());
        assert!(approx_eq(
            g.content_scanline_strength,
            o.scanlines_content_strength.unwrap(),
        ));
        assert_eq!(g.scanline_color_bright, o.scanline_color_bright.unwrap());
        assert_eq!(g.scanline_color_dark, o.scanline_color_dark.unwrap());
        assert!(approx_eq(
            g.content_scanline_attenuation,
            o.scanlines_content_attenuation.unwrap(),
        ));
        assert_eq!(g.iterations, o.iterations.unwrap());
    }

    #[test]
    fn apply_glow_config_none_override_falls_through_to_config_when_flag_on() {
        // Per-slot granularity: with the flag on, `None` slots still
        // defer to the config. Override only `threshold`; `intensity`
        // must come from the config because its override is `None`.
        let Some((_d, _q, mut g)) = try_make_test_glow() else {
            eprintln!("skipping: no GPU adapter");
            return;
        };
        let mut c = non_default_glow_config();
        c.theme_overrides_glow = true;
        let o = palette::GlowOverrides {
            threshold: Some(0.07),
            ..palette::GlowOverrides::NONE
        };
        apply_glow_config(&mut g, &c, &o);

        assert!(approx_eq(g.threshold, 0.07));
        assert!(approx_eq(g.intensity, c.glow_intensity));
        assert_eq!(g.match_brightness, c.glow_match_brightness);
        assert_eq!(g.match_scanlines, c.glow_scanlines);
    }

    #[test]
    fn apply_glow_config_clamps_iterations_above_max_via_override() {
        // The iterations clamp runs *after* the override pick, so an
        // outsize scheme value can't smuggle a bigger dual-Kawase chain
        // past `MAX_ITERATIONS`.
        let Some((_d, _q, mut g)) = try_make_test_glow() else {
            eprintln!("skipping: no GPU adapter");
            return;
        };
        let mut c = Config::defaults();
        c.theme_overrides_glow = true;
        let o = palette::GlowOverrides {
            iterations: Some(renderer::glow::MAX_ITERATIONS * 10),
            ..palette::GlowOverrides::NONE
        };
        apply_glow_config(&mut g, &c, &o);
        assert_eq!(g.iterations, renderer::glow::MAX_ITERATIONS);
    }

    #[test]
    fn apply_glow_config_clamps_iterations_below_one_via_override() {
        // Same clamp on the low end: `Some(0)` from a scheme still
        // floors at 1 after the pick.
        let Some((_d, _q, mut g)) = try_make_test_glow() else {
            eprintln!("skipping: no GPU adapter");
            return;
        };
        let mut c = Config::defaults();
        c.theme_overrides_glow = true;
        let o = palette::GlowOverrides {
            iterations: Some(0),
            ..palette::GlowOverrides::NONE
        };
        apply_glow_config(&mut g, &c, &o);
        assert_eq!(g.iterations, 1);
    }

    //
    // `effective_skip_primary_bg` is the one glow slot resolved at draw
    // time rather than mirrored onto the `Glow` struct (it picks between
    // two render pipelines). The same tiebreaker applies; these tests
    // don't need a GPU.
    //

    #[test]
    fn effective_skip_primary_bg_flag_off_ignores_override() {
        // Override present but the tiebreaker is off → config wins.
        let mut c = Config::defaults();
        c.theme_overrides_glow = false;
        c.glow_scanlines_skip_primary_bg = false;
        let o = palette::GlowOverrides {
            scanlines_skip_primary_bg: Some(true),
            ..palette::GlowOverrides::NONE
        };
        assert!(!effective_skip_primary_bg(&c, &o));
    }

    #[test]
    fn effective_skip_primary_bg_flag_on_none_falls_through_to_config() {
        // Flag on but no override candidate → config still wins.
        let mut c = Config::defaults();
        c.theme_overrides_glow = true;
        c.glow_scanlines_skip_primary_bg = true;
        assert!(effective_skip_primary_bg(&c, &palette::GlowOverrides::NONE));
    }

    #[test]
    fn effective_skip_primary_bg_flag_on_override_wins() {
        // Override present and flag on → override wins, even when it
        // disagrees with the config value.
        let mut c = Config::defaults();
        c.theme_overrides_glow = true;
        c.glow_scanlines_skip_primary_bg = false;
        let o = palette::GlowOverrides {
            scanlines_skip_primary_bg: Some(true),
            ..palette::GlowOverrides::NONE
        };
        assert!(effective_skip_primary_bg(&c, &o));

        // And the inverse — override `false` beats config `true`.
        c.glow_scanlines_skip_primary_bg = true;
        let o = palette::GlowOverrides {
            scanlines_skip_primary_bg: Some(false),
            ..palette::GlowOverrides::NONE
        };
        assert!(!effective_skip_primary_bg(&c, &o));
    }

    //
    // theme_overrides_glow round-trips through Config::serialize /
    // Config::parse_str (both polarities) and starts at `false` so
    // upgrading the binary doesn't silently start honouring stray glow
    // keys in users' existing schemes.
    //

    #[test]
    fn config_defaults_theme_overrides_glow_is_false() {
        // Opt-in by design: schemes may carry glow keys but they're
        // ignored until the user explicitly turns this on.
        assert!(!Config::defaults().theme_overrides_glow);
    }

    #[test]
    fn config_round_trip_preserves_theme_overrides_glow_true() {
        let mut c = Config::defaults();
        c.theme_overrides_glow = true;
        let parsed = Config::parse_str(&c.serialize());
        assert!(parsed.theme_overrides_glow);
    }

    #[test]
    fn config_round_trip_preserves_theme_overrides_glow_false() {
        // Explicit `false` must also survive the round-trip — if the
        // serializer silently dropped the field, a downgrade-then-upgrade
        // cycle would reset everyone to the default.
        let mut c = Config::defaults();
        c.theme_overrides_glow = false;
        let parsed = Config::parse_str(&c.serialize());
        assert!(!parsed.theme_overrides_glow);
    }

    #[test]
    fn config_parse_theme_overrides_glow_true_sets_field() {
        // Direct parse of the user-facing config syntax — pins the
        // exact key name so a rename here would fail the test rather
        // than silently break every existing user's config file.
        let parsed = Config::parse_str("theme_overrides_glow = true\n");
        assert!(parsed.theme_overrides_glow);
    }

    #[test]
    fn should_rearm_image_poll_store_pending_alone_rearms() {
        // Regression: the Kitty Unicode-placeholder path (`a=T,U=1`,
        // what `icat` emits under tmux) and animation frames (`a=f`)
        // bump the image store's pending queue WITHOUT registering a
        // `pending_placements` entry. With no deferred placements and an
        // empty result set, `store_pending > 0` is the only signal that a
        // decode is still in flight — it must re-arm, or the loop parks at
        // `ControlFlow::Wait` and the freshly-`cat`'d image renders blank.
        assert!(should_rearm_image_poll(true, true, 1));
    }

    #[test]
    fn should_rearm_image_poll_all_idle_does_not_rearm() {
        // Nothing in flight on any of the three queues — let the loop go
        // to sleep rather than spin redrawing forever.
        assert!(!should_rearm_image_poll(true, true, 0));
    }

    #[test]
    fn should_rearm_image_poll_pending_placements_alone_rearms() {
        // A deferred placement (Cmd-Shift-I paste / OSC 1337) is still
        // waiting on its decode.
        assert!(should_rearm_image_poll(false, true, 0));
    }

    #[test]
    fn should_rearm_image_poll_results_alone_rearms() {
        // This poll produced results to act on, so the loop must tick again.
        assert!(should_rearm_image_poll(true, false, 0));
    }

    #[test]
    fn should_rearm_image_poll_combined_signals_rearm() {
        // Any combination of the three live signals must re-arm.
        assert!(should_rearm_image_poll(false, false, 3));
        assert!(should_rearm_image_poll(false, true, 2));
        assert!(should_rearm_image_poll(true, false, 5));
    }

    // --- Onboarding: GlowLevel parsing -----------------------------------

    #[test]
    fn glow_level_from_str_maps_known_presets() {
        assert_eq!(GlowLevel::from_str("off"), Some(GlowLevel::Off));
        assert_eq!(GlowLevel::from_str("subtle"), Some(GlowLevel::Subtle));
        assert_eq!(GlowLevel::from_str("full"), Some(GlowLevel::Full));
    }

    #[test]
    fn glow_level_from_str_rejects_unknown() {
        // Anything outside the three presets is `None`, including casing,
        // whitespace, and the empty string — callers treat `None` as
        // "drop the request" rather than guessing a default.
        assert_eq!(GlowLevel::from_str(""), None);
        assert_eq!(GlowLevel::from_str("Off"), None);
        assert_eq!(GlowLevel::from_str("OFF"), None);
        assert_eq!(GlowLevel::from_str(" off"), None);
        assert_eq!(GlowLevel::from_str("medium"), None);
        assert_eq!(GlowLevel::from_str("bogus"), None);
    }

    // --- Onboarding: GlowLevel -> Config knob mapping --------------------

    #[test]
    fn apply_glow_level_off_clears_all_match_modes() {
        let mut c = Config::defaults();
        // Pre-dirty the match flags so we prove `Off` actively clears them
        // rather than relying on the defaults already being false.
        c.glow_match_foreground = true;
        c.glow_match_brightness = true;
        c.glow_match_bright_ansi = true;
        c.apply_glow_level(GlowLevel::Off);
        assert!(!c.glow_match_foreground);
        assert!(!c.glow_match_brightness);
        assert!(!c.glow_match_bright_ansi);
    }

    #[test]
    fn apply_glow_level_subtle_sets_fg_and_brightness_only() {
        let mut c = Config::defaults();
        c.apply_glow_level(GlowLevel::Subtle);
        assert!(c.glow_match_foreground);
        assert!(c.glow_match_brightness);
        assert!(!c.glow_match_bright_ansi);
        assert!(approx_eq(c.glow_intensity, 0.6));
    }

    #[test]
    fn apply_glow_level_full_sets_all_match_modes_and_full_intensity() {
        let mut c = Config::defaults();
        c.apply_glow_level(GlowLevel::Full);
        assert!(c.glow_match_foreground);
        assert!(c.glow_match_brightness);
        assert!(c.glow_match_bright_ansi);
        assert!(approx_eq(c.glow_intensity, 1.0));
    }

    #[test]
    fn apply_glow_level_off_leaves_scanlines_untouched() {
        // `apply_glow_level` only owns the match-mode + intensity knobs;
        // scanlines are `apply_scanlines`' business. Setting a glow preset
        // must not silently flip a scanline choice the user already made.
        let mut c = Config::defaults();
        c.glow_scanlines = true;
        c.glow_scanlines_content = true;
        c.apply_glow_level(GlowLevel::Off);
        assert!(c.glow_scanlines);
        assert!(c.glow_scanlines_content);
    }

    // --- Onboarding: scanline toggle -------------------------------------

    #[test]
    fn apply_scanlines_on_sets_halo_only_not_content() {
        let mut c = Config::defaults();
        c.apply_scanlines(true);
        assert!(c.glow_scanlines);
        // Content overlay stays off — scanlines ride the glow, not the text.
        assert!(!c.glow_scanlines_content);
    }

    #[test]
    fn apply_scanlines_leaves_content_overlay_setting_untouched() {
        // The content overlay is its own config setting; toggling the halo
        // scanlines must not override whatever the config chose for it.
        let mut c = Config::defaults();
        c.glow_scanlines_content = true;
        c.apply_scanlines(true);
        assert!(c.glow_scanlines);
        assert!(c.glow_scanlines_content);
        c.glow_scanlines_content = false;
        c.apply_scanlines(false);
        assert!(!c.glow_scanlines);
        assert!(!c.glow_scanlines_content);
    }

    #[test]
    fn apply_scanlines_off_clears_halo() {
        let mut c = Config::defaults();
        c.glow_scanlines = true;
        c.apply_scanlines(false);
        assert!(!c.glow_scanlines);
        assert!(!c.glow_scanlines_content);
    }

    #[test]
    fn apply_scanlines_leaves_glow_match_modes_untouched() {
        // Mirror of `apply_glow_level_off_leaves_scanlines_untouched`: the
        // two setters own disjoint knobs, so toggling scanlines must not
        // disturb the glow match-mode state.
        let mut c = Config::defaults();
        c.apply_glow_level(GlowLevel::Full);
        c.apply_scanlines(true);
        assert!(c.glow_match_foreground);
        assert!(c.glow_match_brightness);
        assert!(c.glow_match_bright_ansi);
    }

    // --- Onboarding: combined CRT level ----------------------------------

    #[test]
    fn crt_level_from_str_maps_known_presets() {
        assert_eq!(CrtLevel::from_str("off"), Some(CrtLevel::Off));
        assert_eq!(CrtLevel::from_str("low"), Some(CrtLevel::Low));
        assert_eq!(CrtLevel::from_str("high"), Some(CrtLevel::High));
    }

    #[test]
    fn crt_level_from_str_rejects_unknown() {
        assert_eq!(CrtLevel::from_str(""), None);
        assert_eq!(CrtLevel::from_str("Off"), None);
        assert_eq!(CrtLevel::from_str("medium"), None);
        assert_eq!(CrtLevel::from_str("subtle"), None);
    }

    #[test]
    fn apply_crt_level_off_clears_glow_and_scanlines() {
        // Start from a fully-lit config to prove Off actively clears both
        // halves of the effect.
        let mut c = Config::defaults();
        c.apply_crt_level(CrtLevel::High);
        c.apply_crt_level(CrtLevel::Off);
        assert!(!c.glow_match_foreground);
        assert!(!c.glow_match_brightness);
        assert!(!c.glow_match_bright_ansi);
        assert!(!c.glow_scanlines);
        assert!(!c.glow_scanlines_content);
    }

    #[test]
    fn apply_crt_level_low_is_dialed_back_spacedust() {
        let mut c = Config::defaults();
        c.apply_crt_level(CrtLevel::Low);
        // Brightness-driven bloom (spacedust's match mode), dialed back.
        assert!(c.glow_match_brightness);
        assert!(!c.glow_match_bright_ansi);
        assert!(!c.glow_match_foreground);
        assert!(approx_eq(c.glow_intensity, 0.3));
        assert_eq!(c.glow_iterations, 2);
        // Halo scanlines on but gentler; content overlay untouched (off).
        assert!(c.glow_scanlines);
        // Scanline knockout is full strength, same as High; only the bloom
        // differs between Low and High.
        assert!(approx_eq(c.glow_scanline_strength, 1.0));
        assert!(approx_eq(c.glow_scanline_period, 8.0));
        assert!(!c.glow_scanlines_content);
    }

    #[test]
    fn apply_crt_level_high_matches_spacedust_glow() {
        let mut c = Config::defaults();
        c.apply_crt_level(CrtLevel::High);
        // The spacedust theme's glow + scanline values, verbatim.
        assert!(c.glow_match_brightness);
        assert!(!c.glow_match_bright_ansi);
        assert!(!c.glow_match_foreground);
        assert!(approx_eq(c.glow_threshold, 0.0));
        assert!(approx_eq(c.glow_intensity, 0.6));
        assert!(approx_eq(c.glow_softness, 1.0));
        assert_eq!(c.glow_iterations, 4);
        assert!(c.glow_scanlines);
        assert!(approx_eq(c.glow_scanline_strength, 1.0));
        assert!(approx_eq(c.glow_scanline_period, 8.0));
        // Content overlay left to its config default (off).
        assert!(!c.glow_scanlines_content);
    }

    #[test]
    fn theme_overrides_glow_defaults_off() {
        // CRT presets are concrete config values; the "let the scheme's glow
        // win" switch stays off by default so they actually take effect.
        assert!(!Config::defaults().theme_overrides_glow);
    }
}
