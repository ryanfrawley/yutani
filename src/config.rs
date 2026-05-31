//! Configuration: the on-disk `Config` model (TOML load / serialize / live
//! apply), the small enums it embeds (glow / CRT levels, shell-exit and
//! prompt-gutter modes), and the parse helpers that back them.

use crate::{config_path, format_hex_rgb, images, palette, renderer, DECORATOR_HEIGHT, DEFAULT_FONT_SIZE};

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

/// What the window does when the child shell exits. Configured via the
/// `shell_exit_mode` key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShellExitMode {
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
    pub(crate) fn from_str(s: &str) -> Option<Self> {
        match s {
            "always" => Some(Self::Always),
            "never" => Some(Self::Never),
            "on_success" => Some(Self::OnSuccess),
            _ => None,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
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
pub(crate) enum PromptGutter {
    /// No indicator (default).
    None,
    /// A short vertical bar per prompt — green on success, red on failure,
    /// dim while a command is still running.
    Bar,
}

impl PromptGutter {
    pub(crate) fn from_str(s: &str) -> Option<Self> {
        match s {
            "none" => Some(Self::None),
            "bar" => Some(Self::Bar),
            _ => None,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Bar => "bar",
        }
    }
}

/// Style of the top scroll-edge effect — the band where scrollback content
/// meets the title-bar chrome. Mirrors macOS 26's `NSScrollEdgeEffectStyle`
/// (`.soft` / `.hard`), implemented here on the GPU since the terminal isn't an
/// `NSScrollView` the system effect could attach to. Configured via the
/// `scroll_edge_style` key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScrollEdgeStyle {
    /// A soft gradient: rows blur and dissolve toward the background as they
    /// slide up behind the toolbar (default).
    Soft,
    /// A hard, opaque background-colored backing the rows pass behind, with a
    /// crisp bottom edge — a solid separation rather than a fade.
    Hard,
}

impl ScrollEdgeStyle {
    pub(crate) fn from_str(s: &str) -> Option<Self> {
        match s {
            "soft" => Some(Self::Soft),
            "hard" => Some(Self::Hard),
            _ => None,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Soft => "soft",
            Self::Hard => "hard",
        }
    }
}

/// FreeType hinting target applied while rasterizing glyphs. Configured via
/// the `font_hinting` key. `Normal` is the default and — because FreeType's
/// `TARGET_NORMAL` is the zero/default load target — reproduces the exact
/// pixels Yutani produced before this knob existed (plain `RENDER`). The
/// other variants thread a different hinting target into `Font::load_flags`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Hinting {
    /// No hinting (`FT_LOAD_NO_HINTING`). Outlines are rendered as designed,
    /// at the cost of crispness on the pixel grid at small sizes.
    None,
    /// Light auto-hint target (`FT_LOAD_TARGET_LIGHT`): vertical positions are
    /// snapped, horizontal stems left alone, for a softer, less distorted look.
    Light,
    /// Normal hinting target (`FT_LOAD_TARGET_NORMAL`), the FreeType default
    /// and Yutani's historical behavior.
    Normal,
    /// Strongest hinting. FreeType has no distinct "full" load target, so this
    /// maps to `FT_LOAD_TARGET_NORMAL` (same as `Normal`) — kept as a separate
    /// config value for forward-compatibility and to mirror the common
    /// none/light/normal/full vocabulary users expect.
    Full,
}

impl Hinting {
    pub(crate) fn from_str(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "none" => Some(Self::None),
            "light" => Some(Self::Light),
            "normal" => Some(Self::Normal),
            "full" => Some(Self::Full),
            _ => None,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Light => "light",
            Self::Normal => "normal",
            Self::Full => "full",
        }
    }
}

impl Default for Hinting {
    fn default() -> Self {
        Self::Normal
    }
}

#[derive(Clone)]
pub(crate) struct Config {
    pub(crate) font_size: f32,
    pub(crate) top_fade_height: f32,
    pub(crate) top_fade_anim_secs: f32,
    pub(crate) bottom_fade_height: f32,
    pub(crate) bottom_fade_anim_secs: f32,
    /// Style of the top scroll-edge effect (`soft` blur fade vs `hard` opaque
    /// backing). See [`ScrollEdgeStyle`].
    pub(crate) scroll_edge_style: ScrollEdgeStyle,
    /// Ease-in-out duration for cursor position changes. 0 disables the
    /// animation and the cursor snaps as before.
    pub(crate) cursor_anim_secs: f32,
    /// Ease-out duration for the smooth scroll-on-output slide: when new output
    /// pushes lines into scrollback on the live (non-scrolled) primary view,
    /// the grid slides up over this many seconds instead of snapping. 0
    /// disables it and new lines appear instantly.
    pub(crate) scroll_on_output_secs: f32,
    /// When false, the cursor never blinks regardless of what DECSCUSR
    /// requests. Defaults to false because steady cursors are easier on
    /// the eyes; opt back in for xterm-faithful behavior.
    pub(crate) cursor_blink: bool,
    /// Name of a TOML scheme under ~/.config/yutani/schemes/. `None` keeps
    /// the built-in defaults; a missing file with `Some(_)` warns and falls
    /// back to defaults. Used as the active scheme when `auto_theme` is off,
    /// and as the fallback when an `auto_theme` slot below is unset.
    pub(crate) color_scheme: Option<String>,
    /// When true, follow the system light/dark appearance: install
    /// `light_scheme` in light mode and `dark_scheme` in dark mode, swapping
    /// live when the OS appearance changes. When false, `color_scheme` is used
    /// regardless of system appearance.
    pub(crate) auto_theme: bool,
    /// Scheme to use in system light mode while `auto_theme` is on. `None`
    /// falls back to `color_scheme`, then the built-in defaults.
    pub(crate) light_scheme: Option<String>,
    /// Scheme to use in system dark mode while `auto_theme` is on. `None`
    /// falls back to `color_scheme`, then the built-in defaults.
    pub(crate) dark_scheme: Option<String>,
    /// Preferred font family name. `None` (or empty) falls back to the
    /// built-in preference list (Iosevka Term → Iosevka → Fira Code → Menlo).
    /// Matched by exact family name first; failing that, by substring (so
    /// "Iosevka" picks up "Iosevka Term", etc.). A `Some(_)` value that
    /// doesn't match any installed family warns and falls back to the
    /// built-in list.
    pub(crate) font_family: Option<String>,
    /// When true, pixels whose HSV value (max channel) exceeds
    /// `glow_threshold` contribute to the glow. Brightness drives the
    /// glow intensity — brighter pixels bloom harder.
    pub(crate) glow_match_brightness: bool,
    /// When true, pixels whose HSV hue is within `glow_hue_tolerance_deg`
    /// of one of the colour scheme's 8 bright ANSI variants contribute
    /// to the glow. Matched on hue only so antialiased glyphs (which blend
    /// toward the background) still register.
    pub(crate) glow_match_bright_ansi: bool,
    /// HSV-value (brightness) cutoff for `glow_match_brightness` mode.
    pub(crate) glow_threshold: f32,
    /// Additive composite multiplier; 1.0 leaves the glow at original colour
    /// intensity, higher values bloom harder.
    pub(crate) glow_intensity: f32,
    /// Width of the smoothstep band above `glow_threshold` (and the hue
    /// tolerance for bright-ANSI mode). Larger values give a softer cutoff.
    pub(crate) glow_softness: f32,
    /// Degrees of hue slop allowed by `glow_match_bright_ansi`. Default 18°
    /// covers small palette drift; larger values catch tinted variants.
    pub(crate) glow_hue_tolerance_deg: f32,
    /// Dual-Kawase iterations applied to the bright extraction. Higher =
    /// wider, softer halo at the cost of fill rate.
    pub(crate) glow_iterations: usize,
    /// When true, pixels whose RGB distance to the palette's foreground
    /// colour is within `glow_fg_tolerance` contribute to the glow. The
    /// only mode that catches achromatic default text.
    pub(crate) glow_match_foreground: bool,
    /// RGB Euclidean radius around the foreground colour for
    /// `glow_match_foreground`. Max meaningful value is √3 ≈ 1.73.
    pub(crate) glow_fg_tolerance: f32,
    /// When true, the glow composite applies a CRT-style scanline
    /// knockout: alternating horizontal rows of the halo are alpha'd to
    /// zero. Affects both bg and fg glow layers.
    pub(crate) glow_scanlines: bool,
    /// 0 = no scanline effect, 1 = full knockout on dark rows. Clamped.
    pub(crate) glow_scanline_strength: f32,
    /// Scanline cycle in framebuffer pixels (half dark, half bright).
    /// 4 = 2px dark + 2px bright. On Retina that's ~1 logical-pixel
    /// alternation. Clamped to >= 1.
    pub(crate) glow_scanline_period: f32,
    /// When true, the scanline pattern is *also* applied (multiply
    /// blend) over all rendered content — bg colors and glyphs — not
    /// just the glow halo. Shares the period with `glow_scanlines` but
    /// has its own strength so each layer can be tuned separately.
    pub(crate) glow_scanlines_content: bool,
    /// Strength of the content-overlay scanlines. 0 = invisible,
    /// 1 = dark rows fully knocked to black. Clamped.
    pub(crate) glow_scanlines_content_strength: f32,
    /// RGBA multiplier for bright scan rows of the content overlay.
    /// Default white = no change. Tinted values colour the bright
    /// stripes (e.g. slight cyan/amber for a CRT phosphor look).
    /// Parsed from a `0xRRGGBB` literal in the config; alpha = 1.
    pub(crate) glow_scanline_color_bright: [f32; 4],
    /// RGBA multiplier for dark scan rows. Default black = full
    /// knockout. Non-black values let dark stripes show a dim colour
    /// instead of going pitch black.
    pub(crate) glow_scanline_color_dark: [f32; 4],
    /// When true, the content overlay is suppressed wherever the bg
    /// scene pixel matches the window's primary background colour —
    /// scanlines fade out over empty areas of the terminal and only
    /// appear over colored cells, glyphs, and glow. Only effective in
    /// the layered / strip-only render paths (which have a bg scene
    /// to sample); fast-path frames render the overlay uniformly.
    pub(crate) glow_scanlines_skip_primary_bg: bool,
    /// 0..1 amount to soften the masked content overlay over any
    /// drawn pixel — colored bg cells AND glyphs alike. 0 = full
    /// strength on all content; 1 = no overlay on drawn content
    /// (only empty terminal area gets scanlines). Has no effect when
    /// `glow_scanlines_skip_primary_bg` is off.
    pub(crate) glow_scanlines_content_attenuation: f32,
    /// When true, any `glow_*` override declared by the active color scheme
    /// (see `palette::GlowOverrides`) wins over the matching field in this
    /// `Config`. When false (the default), config values always win and
    /// scheme overrides are ignored — preserves prior behavior for users
    /// whose schemes happen to carry stray glow keys.
    pub(crate) theme_overrides_glow: bool,
    /// Master switch for the image-placement feature. When false, the
    /// Cmd-Shift-I keybind and the `YUTANI_TEST_IMAGE` startup hook
    /// silently no-op, and image-protocol payloads (once parsers land
    /// in phase 2) will be discarded. The render pipeline itself stays
    /// loaded — there's no measurable cost when no placements exist.
    pub(crate) images_enabled: bool,
    /// Hard upper bound on the total bytes the image `Store` holds. New
    /// uploads past this cap are refused (logged as BudgetExceeded);
    /// mark-and-sweep keeps actually-used images alive. 0 effectively
    /// disables image residency. Stored as megabytes so the config file
    /// stays readable.
    pub(crate) images_memory_cap_mb: usize,
    /// Per-decode rejection threshold. A header-check inside
    /// `decode_to_rgba` fires before pixel allocation, so a malicious
    /// 100MB-pixel PNG is rejected without touching memory. Default
    /// 16M pixels = 4096×4096 — fits any sane screenshot.
    pub(crate) images_max_pixels: u64,
    /// Per-decode deadline. The worker can't be interrupted mid-decode,
    /// but a pending request older than this is dropped on the main
    /// thread and the late result is discarded. Protects against the
    /// queue backing up when a parser-driven payload hits a slow path.
    pub(crate) images_decode_timeout_ms: u64,
    /// When false, placements that fully scroll off the top of the
    /// primary grid are dropped instead of promoted to scrollback. Saves
    /// memory in long-running shells with lots of image traffic; the
    /// trade-off is that scrolling back into history doesn't recover the
    /// image. (Scrollback rendering of placements isn't implemented in
    /// phase 1 anyway, so the toggle is mostly about retention cost
    /// today.)
    pub(crate) images_in_scrollback: bool,
    /// Sampler choice for new image uploads: "linear" smooths under
    /// scaling, "nearest" preserves crisp pixel edges (useful for
    /// pixel-art / sprite content). Existing GpuImages keep whichever
    /// sampler their bind group was built with — a flip applies to
    /// subsequent decodes only.
    pub(crate) images_filter: String,
    /// When the GPU image draw is unavailable for a placement — either
    /// `images_enabled = false` or the placement's decode failed in a
    /// case main.rs hasn't yet cleaned up — render the placement as
    /// Unicode half-block (▀) glyphs against the cell grid using the
    /// preview cached in `images::Store`. Off by default: the in-flight
    /// decode case (which would flicker) is *never* covered; only the
    /// fully-disabled / fully-failed cases are.
    pub(crate) images_halfblock_for_missing: bool,
    /// What happens to the window when the child shell exits. Defaults to
    /// `OnSuccess`: a clean exit closes the window, a non-zero/abnormal
    /// exit keeps it open with a status line. See `ShellExitMode`.
    pub(crate) shell_exit_mode: ShellExitMode,
    /// OSC 133 prompt-status gutter indicator. Off by default.
    pub(crate) prompt_gutter: PromptGutter,
    /// Whether the filesystem/history autocomplete popup is shown as you
    /// type. Default on; set `autocomplete = false` to disable.
    pub(crate) autocomplete: bool,
    /// Gamma curve applied to glyph coverage in the fragment shader to
    /// thicken (>1) or soften (<1) anti-aliased edges. `alpha' =
    /// pow(coverage, 1.0 / text_gamma)`. 1.0 is identity and reproduces the
    /// pre-existing behavior exactly. Clamped to `[0.25, 4.0]`.
    pub(crate) text_gamma: f32,
    /// FreeType hinting target used when rasterizing glyphs into the atlas.
    /// `Normal` (the default) reproduces the historical output. See [`Hinting`].
    pub(crate) font_hinting: Hinting,
}

impl Config {
    pub(crate) fn defaults() -> Self {
        Self {
            font_size: DEFAULT_FONT_SIZE,
            top_fade_height: DECORATOR_HEIGHT * 3.0,
            top_fade_anim_secs: 0.072,
            bottom_fade_height: DECORATOR_HEIGHT * 3.0,
            bottom_fade_anim_secs: 0.2,
            scroll_edge_style: ScrollEdgeStyle::Soft,
            cursor_anim_secs: 0.06,
            scroll_on_output_secs: 0.08,
            cursor_blink: false,
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
            text_gamma: 1.0,
            font_hinting: Hinting::default(),
        }
    }

    pub(crate) fn load() -> Self {
        let Some(p) = config_path() else { return Self::defaults() };
        let Ok(s) = std::fs::read_to_string(p) else { return Self::defaults() };
        Self::parse_str(&s)
    }

    pub(crate) fn parse_str(s: &str) -> Self {
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
    pub(crate) fn apply(&mut self, k: &str, v: &toml::Value) {
        match k {
            "font_size" => if let Some(x) = cfg_f32(v) { self.font_size = x; },
            "top_fade_height" => if let Some(x) = cfg_f32(v) { self.top_fade_height = x; },
            "top_fade_anim_secs" => if let Some(x) = cfg_f32(v) { self.top_fade_anim_secs = x; },
            "bottom_fade_height" => if let Some(x) = cfg_f32(v) { self.bottom_fade_height = x; },
            "bottom_fade_anim_secs" => if let Some(x) = cfg_f32(v) { self.bottom_fade_anim_secs = x; },
            "cursor_anim_secs" => if let Some(x) = cfg_f32(v) { self.cursor_anim_secs = x; },
            "scroll_on_output_secs" => if let Some(x) = cfg_f32(v) { self.scroll_on_output_secs = x; },
            "cursor_blink" => if let Some(x) = v.as_bool() { self.cursor_blink = x; },
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
            "shell_exit_mode" => set_enum_from_str(v, &mut self.shell_exit_mode, ShellExitMode::from_str),
            "scroll_edge_style" => set_enum_from_str(v, &mut self.scroll_edge_style, ScrollEdgeStyle::from_str),
            "prompt_gutter" => set_enum_from_str(v, &mut self.prompt_gutter, PromptGutter::from_str),
            "autocomplete" => if let Some(x) = v.as_bool() { self.autocomplete = x; },
            "text_gamma" => if let Some(x) = cfg_f32(v) {
                self.text_gamma = x.clamp(0.25, 4.0);
            },
            "font_hinting" => set_enum_from_str(v, &mut self.font_hinting, Hinting::from_str),
            _ => (),
        }
    }

    pub(crate) fn save(&self) {
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
    pub(crate) fn active_scheme(&self, dark: bool) -> Option<&str> {
        if !self.auto_theme {
            return self.color_scheme.as_deref();
        }
        let slot = if dark { &self.dark_scheme } else { &self.light_scheme };
        slot.as_deref().or(self.color_scheme.as_deref())
    }

    pub(crate) fn serialize(&self) -> String {
        let mut s = String::from("# Yutani configuration\n\n");
        s.push_str(&format!(
            "# Display\n\
             font_size = {}\n\
             top_fade_height = {}\n\
             top_fade_anim_secs = {}\n\
             bottom_fade_height = {}\n\
             bottom_fade_anim_secs = {}\n\
             # scroll_edge_style: \"soft\" | \"hard\"\n\
             scroll_edge_style = {}\n\
             cursor_anim_secs = {}\n\
             scroll_on_output_secs = {}\n\
             cursor_blink = {}\n\
             # text_gamma: glyph-edge gamma; 1.0 = unchanged, >1 thickens, <1 softens (clamped 0.25..4.0)\n\
             text_gamma = {}\n\
             # font_hinting: \"none\" | \"light\" | \"normal\" | \"full\"\n\
             font_hinting = {}\n",
            self.font_size,
            self.top_fade_height,
            self.top_fade_anim_secs,
            self.bottom_fade_height,
            self.bottom_fade_anim_secs,
            toml_str_lit(self.scroll_edge_style.as_str()),
            self.cursor_anim_secs,
            self.scroll_on_output_secs,
            self.cursor_blink,
            self.text_gamma,
            toml_str_lit(self.font_hinting.as_str()),
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
/// Assign `slot` from a string-typed TOML value parsed through `parse`,
/// leaving the existing value untouched if `v` isn't a string or `parse`
/// rejects it. This is the forgiving "keep the default on an unknown value"
/// contract shared by every enum-valued config slot (shell_exit_mode,
/// scroll_edge_style, prompt_gutter, font_hinting), factored out of the
/// otherwise-identical nested `if let` blocks in `apply`.
fn set_enum_from_str<T>(v: &toml::Value, slot: &mut T, parse: impl Fn(&str) -> Option<T>) {
    if let Some(s) = v.as_str() {
        if let Some(parsed) = parse(s) {
            *slot = parsed;
        }
    }
}

pub(crate) fn cfg_f32(v: &toml::Value) -> Option<f32> {
    v.as_float()
        .map(|f| f as f32)
        .or_else(|| v.as_integer().map(|i| i as f32))
}

pub(crate) fn cfg_usize(v: &toml::Value) -> Option<usize> {
    v.as_integer().and_then(|i| usize::try_from(i).ok())
}

pub(crate) fn cfg_u64(v: &toml::Value) -> Option<u64> {
    v.as_integer().and_then(|i| u64::try_from(i).ok())
}

/// Encode a string as a TOML basic-string literal (quoted, with escapes),
/// so values like a font family with spaces round-trip through `parse_str`.
pub(crate) fn toml_str_lit(s: &str) -> String {
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
pub(crate) fn apply_glow_config(
    g: &mut renderer::glow::Glow,
    config: &Config,
    overrides: &palette::GlowOverrides,
) {
    pub(crate) fn pick<T>(theme_wins: bool, scheme: Option<T>, cfg: T) -> T {
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
pub(crate) fn effective_skip_primary_bg(config: &Config, overrides: &palette::GlowOverrides) -> bool {
    if config.theme_overrides_glow {
        overrides
            .scanlines_skip_primary_bg
            .unwrap_or(config.glow_scanlines_skip_primary_bg)
    } else {
        config.glow_scanlines_skip_primary_bg
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Defaults must survive a serialize -> parse_str round-trip unchanged.
    /// This is the persistence contract every per-window config setting (e.g.
    /// the `autocomplete` palette toggle that a new window inherits) relies on:
    /// what gets written to disk must parse back to exactly the same value.
    #[test]
    fn defaults_round_trip_unchanged() {
        let original = Config::defaults();
        let parsed = Config::parse_str(&original.serialize());

        assert_eq!(parsed.autocomplete, original.autocomplete);
        assert_eq!(parsed.font_size, original.font_size);
        assert_eq!(parsed.cursor_blink, original.cursor_blink);
        assert_eq!(parsed.scroll_on_output_secs, original.scroll_on_output_secs);
        assert_eq!(parsed.auto_theme, original.auto_theme);
        assert_eq!(parsed.images_enabled, original.images_enabled);
        assert_eq!(parsed.images_filter, original.images_filter);
        assert_eq!(parsed.images_memory_cap_mb, original.images_memory_cap_mb);
        assert_eq!(parsed.shell_exit_mode, original.shell_exit_mode);
        assert_eq!(parsed.prompt_gutter, original.prompt_gutter);
    }

    /// `autocomplete = false` must survive the round-trip. This is the
    /// persistence half of the new-window-inherits-live-config fix: a window
    /// that disabled autocomplete writes `false`, and reloading (or a fresh
    /// process) must read `false` back, not the `true` default.
    #[test]
    fn autocomplete_false_round_trips() {
        let mut c = Config::defaults();
        c.autocomplete = false;

        let parsed = Config::parse_str(&c.serialize());
        assert!(!parsed.autocomplete);
    }

    /// `autocomplete = true` (the default) is emitted explicitly and parses
    /// back as true — guards against the field being dropped from serialize().
    #[test]
    fn autocomplete_true_round_trips() {
        let mut c = Config::defaults();
        c.autocomplete = true;

        let parsed = Config::parse_str(&c.serialize());
        assert!(parsed.autocomplete);
    }

    /// A spread of representative non-default values across the major config
    /// sections must all survive the round-trip together, including the
    /// `Option<String>` scheme/font slots (only emitted when `Some`) and the
    /// enum-backed `shell_exit_mode` / `prompt_gutter` keys.
    #[test]
    fn representative_fields_round_trip() {
        let mut c = Config::defaults();
        c.autocomplete = false;
        c.font_size = 14.5;
        c.cursor_blink = true;
        c.color_scheme = Some("dracula".to_string());
        c.auto_theme = true;
        c.light_scheme = Some("ayu light".to_string());
        c.dark_scheme = Some("spacedust".to_string());
        c.font_family = Some("Iosevka Term".to_string());
        c.glow_match_brightness = true;
        c.glow_scanlines = true;
        c.images_enabled = false;
        c.images_filter = "nearest".to_string();
        c.images_memory_cap_mb = 64;
        c.shell_exit_mode = ShellExitMode::Never;
        c.prompt_gutter = PromptGutter::Bar;
        c.scroll_edge_style = ScrollEdgeStyle::Hard;

        let parsed = Config::parse_str(&c.serialize());

        assert!(!parsed.autocomplete);
        assert_eq!(parsed.scroll_edge_style, ScrollEdgeStyle::Hard);
        assert_eq!(parsed.font_size, 14.5);
        assert!(parsed.cursor_blink);
        assert_eq!(parsed.color_scheme.as_deref(), Some("dracula"));
        assert!(parsed.auto_theme);
        assert_eq!(parsed.light_scheme.as_deref(), Some("ayu light"));
        assert_eq!(parsed.dark_scheme.as_deref(), Some("spacedust"));
        assert_eq!(parsed.font_family.as_deref(), Some("Iosevka Term"));
        assert!(parsed.glow_match_brightness);
        assert!(parsed.glow_scanlines);
        assert!(!parsed.images_enabled);
        assert_eq!(parsed.images_filter, "nearest");
        assert_eq!(parsed.images_memory_cap_mb, 64);
        assert_eq!(parsed.shell_exit_mode, ShellExitMode::Never);
        assert_eq!(parsed.prompt_gutter, PromptGutter::Bar);
    }

    /// Cloning a live Config (what the new-window spawn path does) yields a
    /// value indistinguishable from the source for the persisted fields — the
    /// in-memory half of the fix, complementing the disk round-trip above.
    #[test]
    fn clone_preserves_autocomplete_toggle() {
        let mut c = Config::defaults();
        c.autocomplete = false;
        let cloned = c.clone();
        assert_eq!(cloned.autocomplete, c.autocomplete);
        assert!(!cloned.autocomplete);
    }

    // ---------- text_gamma ----------

    /// A missing key leaves the identity default (1.0), which must reproduce
    /// the pre-existing no-gamma behavior.
    #[test]
    fn text_gamma_default_is_identity() {
        let c = Config::parse_str("");
        assert_eq!(c.text_gamma, 1.0);
    }

    /// A plain in-range value parses through `cfg_f32` (float or int).
    #[test]
    fn text_gamma_parses_in_range() {
        assert_eq!(Config::parse_str("text_gamma = 1.5").text_gamma, 1.5);
        assert_eq!(Config::parse_str("text_gamma = 2").text_gamma, 2.0);
    }

    /// Below the lower bound clamps up to 0.25.
    #[test]
    fn text_gamma_clamps_low() {
        assert_eq!(Config::parse_str("text_gamma = 0.0").text_gamma, 0.25);
        assert_eq!(Config::parse_str("text_gamma = -5.0").text_gamma, 0.25);
    }

    /// Above the upper bound clamps down to 4.0.
    #[test]
    fn text_gamma_clamps_high() {
        assert_eq!(Config::parse_str("text_gamma = 100.0").text_gamma, 4.0);
    }

    /// A non-default value survives the serialize -> parse round-trip.
    #[test]
    fn text_gamma_round_trips() {
        let mut c = Config::defaults();
        c.text_gamma = 1.75;
        let parsed = Config::parse_str(&c.serialize());
        assert_eq!(parsed.text_gamma, 1.75);
    }

    /// The default (1.0) is emitted explicitly and round-trips — guards
    /// against the field being dropped from serialize().
    #[test]
    fn text_gamma_default_round_trips() {
        let original = Config::defaults();
        let parsed = Config::parse_str(&original.serialize());
        assert_eq!(parsed.text_gamma, original.text_gamma);
    }

    // ---------- font_hinting ----------

    /// All four variants parse from their lowercase strings.
    #[test]
    fn font_hinting_parses_all_variants() {
        assert_eq!(Hinting::from_str("none"), Some(Hinting::None));
        assert_eq!(Hinting::from_str("light"), Some(Hinting::Light));
        assert_eq!(Hinting::from_str("normal"), Some(Hinting::Normal));
        assert_eq!(Hinting::from_str("full"), Some(Hinting::Full));
    }

    /// Parsing is case-insensitive.
    #[test]
    fn font_hinting_parse_is_case_insensitive() {
        assert_eq!(Hinting::from_str("NONE"), Some(Hinting::None));
        assert_eq!(Hinting::from_str("Light"), Some(Hinting::Light));
        assert_eq!(Hinting::from_str("FULL"), Some(Hinting::Full));
    }

    /// An unknown string yields None, and applying it leaves the default.
    #[test]
    fn font_hinting_unknown_keeps_default() {
        assert_eq!(Hinting::from_str("bogus"), None);
        let c = Config::parse_str("font_hinting = \"bogus\"");
        assert_eq!(c.font_hinting, Hinting::default());
    }

    /// A missing key leaves the default (Normal = historical behavior).
    #[test]
    fn font_hinting_default_is_normal() {
        let c = Config::parse_str("");
        assert_eq!(c.font_hinting, Hinting::Normal);
        assert_eq!(Hinting::default(), Hinting::Normal);
    }

    /// Each variant survives the serialize -> parse round-trip.
    #[test]
    fn font_hinting_round_trips() {
        for h in [Hinting::None, Hinting::Light, Hinting::Normal, Hinting::Full] {
            let mut c = Config::defaults();
            c.font_hinting = h;
            let parsed = Config::parse_str(&c.serialize());
            assert_eq!(parsed.font_hinting, h, "round-trip failed for {h:?}");
        }
    }
}
