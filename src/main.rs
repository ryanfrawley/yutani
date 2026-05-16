mod app_window;
mod box_drawing;
mod font;
mod font_loader;
mod renderer;

mod ansi;
mod gpu;
mod input;
mod palette;
mod shaper;
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

// use rand_distr::{Distribution, Normal};
// use rand::thread_rng;

use wgpu::util::DeviceExt;

const WINDOW_PADDING: f32 = 16.0;
const DECORATOR_HEIGHT: f32 = 24.0;

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
    p.push("config");
    Some(p)
}

/// Resolve a scheme name to an on-disk path. Accepts either `.yml` or
/// `.yaml`; `.yml` wins when both exist so users can pick the shorter
/// extension without surprise.
fn scheme_path(name: &str) -> Option<std::path::PathBuf> {
    let mut dir = config_dir()?;
    dir.push("schemes");
    for ext in ["yml", "yaml"] {
        let p = dir.join(format!("{}.{}", name, ext));
        if p.exists() {
            return Some(p);
        }
    }
    // Fall through to the .yaml form so the error message points at a
    // canonical path (the loader will report "failed to read ...").
    Some(dir.join(format!("{}.yaml", name)))
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
    /// Name of a YAML scheme under ~/.config/yutani/schemes/. `None` keeps
    /// the built-in defaults; a missing file with `Some(_)` warns and falls
    /// back to defaults.
    color_scheme: Option<String>,
    /// When true, pixels whose HSV saturation exceeds `glow_threshold`
    /// contribute to the glow.
    glow_match_saturation: bool,
    /// When true, pixels whose HSV hue is within `glow_hue_tolerance_deg`
    /// of one of the colour scheme's 8 bright ANSI variants contribute
    /// to the glow. Matched on hue only so antialiased glyphs (which blend
    /// toward the background) still register.
    glow_match_bright_ansi: bool,
    /// HSV-saturation cutoff for `glow_match_saturation` mode.
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
            glow_match_saturation: false,
            glow_match_bright_ansi: false,
            glow_threshold: renderer::glow::DEFAULT_THRESHOLD,
            glow_intensity: renderer::glow::DEFAULT_INTENSITY,
            glow_softness: renderer::glow::DEFAULT_SOFTNESS,
            glow_hue_tolerance_deg: renderer::glow::DEFAULT_HUE_TOLERANCE_DEG,
            glow_iterations: 2,
            glow_match_foreground: false,
            glow_fg_tolerance: renderer::glow::DEFAULT_FG_TOLERANCE,
        }
    }

    fn load() -> Self {
        let Some(p) = config_path() else { return Self::defaults() };
        let Ok(s) = std::fs::read_to_string(p) else { return Self::defaults() };
        Self::parse_str(&s)
    }

    fn parse_str(s: &str) -> Self {
        let mut c = Self::defaults();
        for line in s.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((k, v)) = line.split_once('=') else { continue };
            let (k, v) = (k.trim(), v.trim());
            match k {
                "font_size" => if let Ok(x) = v.parse() { c.font_size = x; },
                "top_fade_height" => if let Ok(x) = v.parse() { c.top_fade_height = x; },
                "top_fade_solid_stop" => if let Ok(x) = v.parse() { c.top_fade_solid_stop = x; },
                "top_fade_anim_secs" => if let Ok(x) = v.parse() { c.top_fade_anim_secs = x; },
                "bottom_fade_height" => if let Ok(x) = v.parse() { c.bottom_fade_height = x; },
                "bottom_fade_anim_secs" => if let Ok(x) = v.parse() { c.bottom_fade_anim_secs = x; },
                "cursor_anim_secs" => if let Ok(x) = v.parse() { c.cursor_anim_secs = x; },
                "cursor_blink" => if let Ok(x) = v.parse() { c.cursor_blink = x; },
                "blur_iterations" => if let Ok(x) = v.parse::<usize>() {
                    c.blur_iterations = x.min(renderer::blur::MAX_BLUR_ITERATIONS);
                },
                "color_scheme" => c.color_scheme = if v.is_empty() { None } else { Some(v.to_string()) },
                "glow_match_saturation" => if let Ok(x) = v.parse() { c.glow_match_saturation = x; },
                "glow_match_bright_ansi" => if let Ok(x) = v.parse() { c.glow_match_bright_ansi = x; },
                "glow_threshold" => if let Ok(x) = v.parse::<f32>() {
                    c.glow_threshold = x.clamp(0.0, 1.0);
                },
                "glow_intensity" => if let Ok(x) = v.parse::<f32>() {
                    c.glow_intensity = x.max(0.0);
                },
                "glow_softness" => if let Ok(x) = v.parse::<f32>() {
                    c.glow_softness = x.clamp(0.0, 1.0);
                },
                "glow_hue_tolerance_deg" => if let Ok(x) = v.parse::<f32>() {
                    c.glow_hue_tolerance_deg = x.clamp(0.0, 180.0);
                },
                "glow_iterations" => if let Ok(x) = v.parse::<usize>() {
                    c.glow_iterations = x.clamp(1, renderer::glow::MAX_ITERATIONS);
                },
                "glow_match_foreground" => if let Ok(x) = v.parse() { c.glow_match_foreground = x; },
                "glow_fg_tolerance" => if let Ok(x) = v.parse::<f32>() {
                    c.glow_fg_tolerance = x.clamp(0.0, 3.0_f32.sqrt());
                },
                _ => (),
            }
        }
        c
    }

    fn save(&self) {
        let Some(p) = config_path() else { return };
        if let Some(parent) = p.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(p, self.serialize());
    }

    fn serialize(&self) -> String {
        let mut s = format!(
            "font_size = {}\n\
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
        );
        if let Some(name) = &self.color_scheme {
            s.push_str(&format!("color_scheme = {}\n", name));
        }
        s.push_str(&format!(
            "glow_match_saturation = {}\n\
             glow_match_bright_ansi = {}\n\
             glow_match_foreground = {}\n\
             glow_threshold = {}\n\
             glow_intensity = {}\n\
             glow_softness = {}\n\
             glow_hue_tolerance_deg = {}\n\
             glow_fg_tolerance = {}\n\
             glow_iterations = {}\n",
            self.glow_match_saturation,
            self.glow_match_bright_ansi,
            self.glow_match_foreground,
            self.glow_threshold,
            self.glow_intensity,
            self.glow_softness,
            self.glow_hue_tolerance_deg,
            self.glow_fg_tolerance,
            self.glow_iterations,
        ));
        s
    }
}

pub struct ViewportSize {
    char_width: usize,
    char_height: usize,
}

struct State {
    gpu: gpu::GpuContext,

    // Must be declared after `gpu` so it gets dropped after the surface —
    // the surface holds unsafe references to the window's resources.
    window: Window,

    render_pipeline: wgpu::RenderPipeline,
    /// Wireframe debug pipeline — same vertex shader but PolygonMode::Line
    /// and a flat-color fragment. `None` if the adapter doesn't expose
    /// POLYGON_MODE_LINE; the toggle becomes a no-op there.
    wireframe_pipeline: Option<wgpu::RenderPipeline>,
    /// Toggled by Cmd-Shift-W. When true, render() picks wireframe_pipeline.
    wireframe: bool,
    vertex_buffer: wgpu::Buffer,
    index_buffer: wgpu::Buffer,
    num_indices: u32,
    // Separate buffer for the edge-fade strip quads. Drawn in the composite
    // pass with the blur sampler bound, so the strips are filled with the
    // dual-Kawase blur of the scene rather than a flat white tint.
    strip_vertex_buffer: wgpu::Buffer,
    strip_index_buffer: wgpu::Buffer,
    num_strip_indices: u32,
    blur: renderer::blur::BlurChain,
    /// Saturation-threshold bloom. When `glow.enabled` is true, the scene is
    /// always rendered to the offscreen `blur.scene` texture so the glow
    /// pass can sample it, even when no edge-fade strips are active.
    glow: renderer::glow::Glow,
    font: font::Font,
    /// rustybuzz shaper, used during update_vertices to detect programming
    /// ligatures (`->`, `=>`, `!=`, …) so the renderer can draw them as a
    /// single wide glyph instead of two adjacent characters.
    shaper: shaper::Shaper,
    font_bind_group: wgpu::BindGroup,
    /// The actual font atlas texture. Kept around so on-demand-rasterized
    /// ligature glyphs can be uploaded incrementally via queue.write_texture
    /// without recreating the texture or bind group.
    font_texture: renderer::texture::Texture,
    /// Layout for the font texture + sampler. Kept around so we can rebind
    /// after a font-size change rebuilds the atlas texture.
    font_bind_group_layout: wgpu::BindGroupLayout,
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
    terminal: terminal::Terminal,
    modifiers: winit::keyboard::ModifiersState,
    scroll_y: f64,
    /// Drop in-flight trackpad momentum once a newer command (a keystroke
    /// that snaps to the bottom) has overridden the user's scroll intent.
    /// Cleared when momentum runs out OR a fresh gesture begins after a
    /// real idle gap — see `last_wheel_at` for how we tell the two apart
    /// (momentum's Started arrives ~one frame after the prior Ended).
    scroll_suppressed: bool,
    last_wheel_at: Option<std::time::Instant>,
    mouse_x: f64,
    mouse_y: f64,
    // Last cell we reported a motion event for. Mouse motion fires per pixel,
    // but the host only cares about per-cell transitions — coalesce.
    last_reported_cell: Option<(u16, u16)>,
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
    /// Smooth cursor motion: when the logical cursor moves, the rendered
    /// quad eases from the previous displayed position toward the new
    /// target over `config.cursor_anim_secs`. `None` while the cursor is
    /// off-screen (scrollback) or before the first frame; the next visible
    /// frame snaps to the target without animating.
    cursor_anim: Option<CursorAnim>,
    /// Snapshot of the previous frame's visible cells, plus a viewport key
    /// (rows / cols / view_offset / alt-screen). Compared on retarget to
    /// spot cells that just went non-blank → blank so the deleted glyph can
    /// fade out as a ghost while the cursor slides over it. A key mismatch
    /// (resize, scrollback, alt-screen toggle) skips ghost detection that
    /// frame so a wholesale grid shift doesn't spawn ghosts everywhere.
    prev_visible: Option<GridSnapshot>,
    /// Glyphs being faded out at their old cell position, captured from
    /// `prev_visible` when the cursor retargets across them. Each fades to
    /// alpha 0 over `cursor_anim_secs`; the entry is dropped early if the
    /// underlying cell becomes non-blank again (a follow-up keystroke).
    cursor_ghosts: Vec<CursorGhost>,
    // Active local text selection, in (absolute_line, col) coordinates so it
    // stays anchored to content as the grid scrolls. `None` when nothing is
    // selected. The two endpoints are anchor (mouse-down cell) and head
    // (latest cell under the cursor); they may be in either order.
    selection: Option<Selection>,
    // Granularity for the active drag (set on press from click_count).
    selection_mode: SelectionMode,
    // Cell where the current drag started; used to recompute word/line
    // selections as the head moves. `None` when no button is being dragged.
    press_cell: Option<(isize, usize)>,
    // Pixel position of the mouse-down. In Cell mode we suppress the
    // selection until the cursor has moved at least DRAG_THRESHOLD_PX from
    // here, so a plain click doesn't briefly highlight a single character.
    press_pixel: Option<(f64, f64)>,
    // Last left-button press, for multi-click detection (must match cell and
    // be within the threshold window).
    last_click: Option<(std::time::Instant, (isize, usize))>,
    click_count: u32,
    /// URL under the mouse while Cmd is held. `None` whenever Cmd is up or
    /// the pointer isn't over a URL. Drives the underline overlay and the
    /// Cmd-click open behavior.
    hover_url: Option<HoverUrl>,
    master: i32,
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
#[derive(Clone, Debug, PartialEq)]
struct HoverUrl {
    /// Absolute (scroll-stable) line indices. `start_abs_line == end_abs_line`
    /// for the common single-row case.
    start_abs_line: isize,
    end_abs_line: isize,
    /// Inclusive cell columns. For wrapped URLs, the underline strip on each
    /// intermediate row spans the full row width — only the first and last
    /// rows use these column positions.
    start_col: usize,
    end_col: usize,
    /// The URL text itself, ready to hand to `open(1)`.
    url: String,
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
fn find_url_at(
    terminal: &terminal::Terminal,
    abs_line: isize,
    col: usize,
) -> Option<HoverUrl> {
    let (start_abs, cols, flat) = build_wrapped_line(terminal, abs_line)?;
    if cols == 0 || col >= cols {
        return None;
    }
    let row_offset = (abs_line - start_abs) as usize;
    let virtual_col = row_offset * cols + col;
    let (s, e, url) = find_url_in_cells(&flat, virtual_col)?;
    Some(HoverUrl {
        start_abs_line: start_abs + (s / cols) as isize,
        start_col: s % cols,
        end_abs_line: start_abs + (e / cols) as isize,
        end_col: e % cols,
        url,
    })
}

#[cfg(target_os = "macos")]
fn open_url(url: &str) {
    let _ = std::process::Command::new("open").arg(url).spawn();
}
#[cfg(not(target_os = "macos"))]
fn open_url(_url: &str) {}

const BLINK_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);
const ANIM_FRAME: std::time::Duration = std::time::Duration::from_millis(16);

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
    fn current(&self, duration: f32) -> (f32, f32) {
        if duration <= 0.0 {
            return self.to;
        }
        let elapsed = self.started_at.elapsed().as_secs_f32();
        if elapsed >= duration {
            return self.to;
        }
        let t = elapsed / duration;
        let e = t * t * (3.0 - 2.0 * t);
        (
            self.from.0 + (self.to.0 - self.from.0) * e,
            self.from.1 + (self.to.1 - self.from.1) * e,
        )
    }

    fn animating(&self, duration: f32) -> bool {
        let moving = (self.from.0 - self.to.0).abs() > f32::EPSILON
            || (self.from.1 - self.to.1).abs() > f32::EPSILON;
        moving && self.started_at.elapsed().as_secs_f32() < duration
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

    fn contains(&self, line: isize, col: usize) -> bool {
        let (s, e) = self.range();
        (line, col) >= s && (line, col) <= e
    }
}

impl State {
    async fn new(
        master: i32,
        window: Window,
        mut font: font::Font,
        shaper: shaper::Shaper,
        config: Config,
        dpi: u32,
    ) -> Self {
        let pt_size = config.font_size;
        let gpu = gpu::GpuContext::new(&window).await;

        // Font texture setup
        let atlas = font.build_atlas();

        let font_texture = renderer::texture::Texture::from_memory(
            &gpu.device,
            &gpu.queue,
            &atlas.buffer,
            atlas.width as u32,
            atlas.height as u32,
            wgpu::TextureFormat::R8Unorm,
            Some("font texture"),
        );

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

        let font_bind_group = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            layout: &font_bind_group_layout,
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
        camera_uniform.update_view_proj(&camera, gpu.config.width as f32, gpu.config.height as f32);

        let camera_buffer = gpu.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("camera buffer"),
            contents: bytemuck::cast_slice(&[camera_uniform]),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
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

        let camera_bind_group = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            layout: &camera_bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: camera_buffer.as_entire_binding(),
            }],
            label: Some("camera bind group"),
        });

        // Edge-fade uniform: layout matches FadeUniform in shader.wgsl —
        // top.xy + bottom.xy + viewport.xy + bg_uv.xy = 4*vec4 = 64 bytes.
        let fade_buffer = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("fade uniform"),
            size: 64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
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
        let fade_bind_group = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            layout: &fade_bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: fade_buffer.as_entire_binding(),
            }],
            label: Some("fade bind group"),
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
                    format: gpu.config.format,
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
                        format: gpu.config.format,
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

        // Calculate console viewport & buffer sizes
        let metrics = font.face().size_metrics().unwrap();
        let viewport = State::get_viewport_size(
            gpu.config.width as f32,
            gpu.config.height as f32,
            font.cell_width(),
            ((metrics.ascender - metrics.descender) >> 6) as usize,
        );
        // Each cell contributes two quads (background + glyph) = 8 verts.
        // Slack covers four phantom rows (two top + two bottom) used during
        // smooth scrolling, the cursor quad, and the two edge-fade quads.
        let area = viewport.char_height * viewport.char_width;
        let extra_quads = 4 * viewport.char_width + 5;
        let mut vertex_buf: Vec<u8> = Vec::with_capacity(
            (2 * area + 2 * extra_quads) * std::mem::size_of::<renderer::vertex::Vertex>() * 4,
        );
        for _ in 0..vertex_buf.capacity() {
            vertex_buf.push(0);
        }
        let vertex_buffer = gpu.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("vertex buffer"),
            contents: &bytemuck::cast_slice(&vertex_buf),
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        });
        let mut index_buf: Vec<u8> = Vec::with_capacity(
            (2 * area + 2 * extra_quads) * std::mem::size_of::<u16>() * 6,
        );
        for _ in 0..index_buf.capacity() {
            index_buf.push(0);
        }
        let index_buffer = gpu.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("index buffer"),
            contents: &bytemuck::cast_slice(&index_buf),
            usage: wgpu::BufferUsages::INDEX | wgpu::BufferUsages::COPY_DST,
        });

        // Three strip quads max (top opaque, top gradient, bottom gradient) ⇒
        // 12 vertices, 18 indices. Sized generously so resize never reallocs.
        let strip_vertex_buffer = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("strip vertex buffer"),
            size: (32 * std::mem::size_of::<renderer::vertex::Vertex>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let strip_index_buffer = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("strip index buffer"),
            size: (64 * std::mem::size_of::<u16>()) as u64,
            usage: wgpu::BufferUsages::INDEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let mut blur = renderer::blur::BlurChain::new(
            &gpu.device,
            gpu.config.format,
            gpu.config.width,
            gpu.config.height,
            &camera_bind_group_layout,
            renderer::vertex::Vertex::desc(),
        );
        blur.write_uniforms(&gpu.queue, gpu.config.width, gpu.config.height);
        blur.iterations = config.blur_iterations.max(1);

        let mut glow = renderer::glow::Glow::new(
            &gpu.device,
            gpu.config.format,
            gpu.config.width,
            gpu.config.height,
            &blur.scene.view,
        );
        glow.match_saturation = config.glow_match_saturation;
        glow.match_bright_ansi = config.glow_match_bright_ansi;
        glow.match_foreground = config.glow_match_foreground;
        glow.threshold = config.glow_threshold;
        glow.intensity = config.glow_intensity;
        glow.softness = config.glow_softness;
        glow.hue_tolerance = config.glow_hue_tolerance_deg;
        glow.fg_tolerance = config.glow_fg_tolerance;
        glow.iterations = config.glow_iterations.clamp(1, renderer::glow::MAX_ITERATIONS);
        // Bright-ANSI matching needs the palette's hue table; foreground
        // matching needs the foreground RGB. Palette is installed before
        // State::new (see `run()`), so this reads the active scheme — or
        // the defaults if no scheme was configured.
        {
            let p = palette::get();
            let bright: [[f32; 4]; 8] = [
                p.ansi[8], p.ansi[9], p.ansi[10], p.ansi[11],
                p.ansi[12], p.ansi[13], p.ansi[14], p.ansi[15],
            ];
            glow.set_bright_palette(&gpu.queue, &bright);
            glow.set_foreground(p.foreground);
        }
        glow.write_uniforms(&gpu.queue, gpu.config.width, gpu.config.height);
        glow.write_glow_params(&gpu.queue);

        Self {
            window,
            gpu,
            atlas,
            render_pipeline,
            wireframe_pipeline,
            wireframe: false,
            vertex_buffer,
            index_buffer,
            num_indices: 0,
            strip_vertex_buffer,
            strip_index_buffer,
            num_strip_indices: 0,
            blur,
            glow,
            font,
            shaper,
            font_bind_group,
            font_texture,
            font_bind_group_layout,
            pt_size,
            dpi,
            config,
            camera,
            camera_uniform,
            camera_buffer,
            camera_bind_group,
            fade_buffer,
            fade_bind_group,
            terminal: terminal::Terminal::new(
                viewport.char_width,
                viewport.char_height,
                10000,
            ),
            modifiers: winit::keyboard::ModifiersState::empty(),
            scroll_y: 0.0,
            scroll_suppressed: false,
            last_wheel_at: None,
            mouse_x: 0.0,
            mouse_y: 0.0,
            last_reported_cell: None,
            held_button: None,
            blink_on: true,
            last_blink: std::time::Instant::now(),
            top_fade_phase: 0.0,
            bottom_fade_phase: 0.0,
            last_anim_tick: std::time::Instant::now(),
            cursor_anim: None,
            prev_visible: None,
            cursor_ghosts: Vec::new(),
            selection: None,
            selection_mode: SelectionMode::Cell,
            press_cell: None,
            press_pixel: None,
            last_click: None,
            click_count: 0,
            hover_url: None,
            master,
            perf: PerfLog::new(),
            vertices_dirty: true,
        }
    }

    /// Mark the vertex buffer stale and ask winit to redraw. Repeated calls
    /// inside one event-loop turn coalesce into a single RedrawRequested,
    /// and `surface.get_current_texture()` blocks at the swapchain to keep
    /// us aligned with the display's vsync cadence.
    fn invalidate(&mut self) {
        self.vertices_dirty = true;
        self.window.request_redraw();
    }

    /// Rebuild the vertex/index buffers if they're stale, recording the cost
    /// in `perf`. Called from the redraw handler before `render`.
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
            char_height: usize::max(
                1,
                (height - WINDOW_PADDING * 2.0 - DECORATOR_HEIGHT) as usize / line_height,
            ),
        }
    }

    fn resize_buffers(&mut self) {
        // Calculate console viewport & buffer sizes
        let metrics = self.font.face().size_metrics().unwrap();
        let viewport = State::get_viewport_size(
            self.gpu.config.width as f32,
            self.gpu.config.height as f32,
            self.font.cell_width(),
            ((metrics.ascender - metrics.descender) >> 6) as usize,
        );
        println!("w: {} h: {}", viewport.char_width, viewport.char_height);
        let extra_quads = 4 * viewport.char_width + 5;
        let mut vertex_buf: Vec<u8> = Vec::with_capacity(
            (2 * viewport.char_height * viewport.char_width + extra_quads)
                * std::mem::size_of::<renderer::vertex::Vertex>()
                * 4,
        );
        for _ in 0..vertex_buf.capacity() {
            vertex_buf.push(0);
        }
        self.vertex_buffer =
            self.gpu
                .device
                .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("vertex buffer"),
                    contents: &bytemuck::cast_slice(&vertex_buf),
                    usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                });
        let mut index_buf: Vec<u8> = Vec::with_capacity(
            (2 * viewport.char_height * viewport.char_width + extra_quads) * std::mem::size_of::<u16>() * 6,
        );
        for _ in 0..index_buf.capacity() {
            index_buf.push(0);
        }
        self.index_buffer =
            self.gpu
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
        let cols = self.terminal.cols;
        let rows = self.terminal.rows;
        let area = cols * rows;
        let mut vertices: Vec<renderer::vertex::Vertex> = Vec::with_capacity(8 * (area + 1));
        let mut indices: Vec<u16> = Vec::with_capacity(12 * (area + 1));

        let theme = self.window.theme().unwrap_or(winit::window::Theme::Light);
        let face = self.font.face();
        let metrics = face.size_metrics().unwrap();
        let line_height = ((metrics.ascender - metrics.descender) >> 6) as f32;
        let cell_w = self.font.cell_width() as f32;
        let bg_h = ((metrics.ascender - metrics.descender) >> 6) as f32;
        let descender = (metrics.descender >> 6) as f32;
        // Underline metrics from the font's `post` table. The face values are
        // in font design units; `y_scale` (16.16 fixed) converts to 26.6 px
        // for this size, matching how `ascender` / `descender` above land in
        // 26.6 — divide by 64 once for actual pixels.
        //   - `underline_position`: vertical center of the stem, in font
        //     units. Negative ⇒ below the baseline (the usual case).
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
        let scroll_y = self.scroll_y as f32;

        let mut push_quad =
            |verts: &mut Vec<renderer::vertex::Vertex>,
             idxs: &mut Vec<u16>,
             x: f32,
             y: f32,
             w: f32,
             h: f32,
             uv0: [f32; 2],
             uv1: [f32; 2],
             color: [f32; 4],
             radii: [f32; 4]| {
                let start = verts.len() as u16;
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
        let view_offset = self.terminal.view_offset() as f32;
        // Alt screen has no scrollback to fade toward — pin both distances
        // to zero so the top/bottom edge fades stay invisible.
        let scrollback_len = if self.terminal.on_alt_screen() {
            0.0
        } else {
            self.terminal.scrollback_len() as f32
        };
        let dist_from_bottom = view_offset * line_height + scroll_y;
        let dist_from_top = (scrollback_len - view_offset) * line_height - scroll_y;
        let near = (dist_from_bottom / line_height)
            .min(dist_from_top / line_height)
            .clamp(0.0, 1.0);
        let decorator_offset = DECORATOR_HEIGHT * (1.0 - near);
        let row_y = |r: isize| WINDOW_PADDING + decorator_offset + (r as f32 + 1.0) * line_height;
        let col_x = |c: usize| WINDOW_PADDING + c as f32 * cell_w;

        // Two extra rows above and below the visible grid are rendered so
        // smooth sub-line scrolling stays populated through the snap. Used
        // both for shaping (below) and the main emit loop further down.
        let r_lo: isize = -2;
        let r_hi: isize = rows as isize + 2;

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
        for r in r_lo..r_hi {
            row_chars.clear();
            for c in 0..cols {
                row_chars.push(
                    self.terminal
                        .extended_cell(r, c)
                        .map(|cell| cell.ch)
                        .unwrap_or(' '),
                );
            }
            let mut row_override: Option<Vec<Option<(u32, font::FaceVariant)>>> = None;
            let mut c = 0;
            while c < cols {
                let Some(start_cell) = self.terminal.extended_cell(r, c) else {
                    c += 1;
                    continue;
                };
                let variant =
                    font::FaceVariant::from_flags(start_cell.style.bold, start_cell.style.italic);
                let lig = match self.shaper.match_at(&row_chars[c..], variant) {
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
                    self.terminal
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
                    self.atlas.ensure_glyph_id(&mut self.font, variant, *gid)
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

        // Re-upload the atlas texture if the shaping pass rasterized any
        // new glyphs. write_texture reuses the existing GPU texture and
        // bind group — no need to recreate either.
        if self.atlas.dirty {
            self.gpu.queue.write_texture(
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
        enum GlyphSource {
            Char(char),
            Substituted(u32),
        }
        let mut emit_cell = |verts: &mut Vec<renderer::vertex::Vertex>,
                             idxs: &mut Vec<u16>,
                             fg_source: GlyphSource,
                             variant: font::FaceVariant,
                             r: isize,
                             c: usize,
                             fg: [f32; 4],
                             bg: [f32; 4]| {
            let x = col_x(c);
            let baseline_y = row_y(r);
            // Background quad spans one line-height strip, centered on the
            // typographic glyph extent. Centering matters when line_height
            // differs from (ascender − descender): top-anchoring would float
            // glyphs to the bottom of the strip on tall-line fonts, while
            // anchoring to the glyph extent risks overlap on tight-line ones.
            // Strip stride = line_height, so adjacent rows still tile cleanly.
            let strip_pad = (line_height - bg_h) * 0.5;
            let bg_y = baseline_y - bg_h - descender - strip_pad + scroll_y;
            push_quad(
                verts,
                idxs,
                x,
                bg_y,
                cell_w,
                line_height,
                [bg_u, bg_v],
                [bg_u, bg_v],
                bg,
                [0.0; 4],
            );
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
                        baseline_y - by + scroll_y,
                        g.height as f32,
                        0.0,
                        g.height as f32,
                    )
                };
                let u0 = (g.x as f32 + q_start) / atlas_w;
                let u1 = (g.x as f32 + q_end) / atlas_w;
                let v0 = (g.y as f32 + p_start) / atlas_h;
                let v1 = (g.y as f32 + p_end) / atlas_h;
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
        let selection = self.selection;

        // 1. Terminal grid + phantom rows on each side (`r_lo..r_hi` defined
        // above where the shaping pass lives — same range so ligature
        // covers and emits stay in sync).
        for r in r_lo..r_hi {
            let over = row_overrides.get(&r);
            for c in 0..cols {
                let Some(cell) = self.terminal.extended_cell(r, c) else { continue };
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
                let variant = font::FaceVariant::from_flags(cell.style.bold, cell.style.italic);
                // Ligature pass may have substituted this cell's glyph.
                let fg_source = match over.and_then(|cs| cs[c]) {
                    Some((glyph_id, _v)) => GlyphSource::Substituted(glyph_id),
                    None => GlyphSource::Char(cell.ch),
                };
                emit_cell(&mut vertices, &mut indices, fg_source, variant, r, c, fg, bg);
            }
        }

        // 1a. Cmd-hover URL underline. Drawn on top of the glyph row so the
        // line is visible regardless of cell bg, and below the selection
        // overlay (1b) so a selected URL still reads as selected. Walks the
        // phantom-row range like the cell loop so the underline follows the
        // text through smooth scroll.
        if let Some(hu) = &self.hover_url {
            for r in r_lo..r_hi {
                let abs_line = self.terminal.visual_to_abs_line(r);
                if abs_line < hu.start_abs_line || abs_line > hu.end_abs_line {
                    continue;
                }
                // Span on this row: first row honors start_col, last row
                // honors end_col, every middle row covers the full width
                // (the URL ran edge-to-edge to wrap).
                let from = if abs_line == hu.start_abs_line { hu.start_col } else { 0 };
                let to = if abs_line == hu.end_abs_line { hu.end_col } else { cols - 1 };
                if from >= cols {
                    continue;
                }
                let last = to.min(cols - 1);
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
                let uy = row_y(r) - underline_pos_px - uh * 0.5 + scroll_y;
                // Match the cell's foreground color so the underline tracks
                // theme overrides; fall back to the default fg.
                let fg = self
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
                let abs_line = self.terminal.visual_to_abs_line(r);
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
                let sy = row_y(r) - bg_h - descender - strip_pad + scroll_y;
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
                                   indices: &mut Vec<u16>,
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
            view_offset: self.terminal.view_offset(),
            on_alt_screen: self.terminal.on_alt_screen(),
        };
        // A viewport change (resize, scrollback, alt-screen toggle) makes
        // last frame's snapshot non-comparable cell-for-cell, so we drop
        // any in-flight ghosts and skip detection until we have a fresh
        // matching snapshot to compare against.
        let key_matches = self
            .prev_visible
            .as_ref()
            .map(|s| s.key == viewport_key)
            .unwrap_or(false);
        if !key_matches {
            self.cursor_ghosts.clear();
        }

        // The cursor and its ghosts animate in BUFFER coordinates so changes
        // to the user's scroll position (which only shift `view_offset`)
        // don't trigger a slide — they ride along with the rest of the
        // grid. `live_grid_offset` is the integer visual-row delta to apply
        // when converting buffer rows back to viewport pixel space; equal
        // to `scrollback_visible` on the primary screen, 0 on alt screen.
        let live_grid_offset_i = if self.terminal.on_alt_screen() {
            0usize
        } else {
            self.terminal.view_offset().min(rows)
        };
        let live_grid_offset = live_grid_offset_i as f32;

        if let Some(_cur_visual_row) = self.terminal.cursor_visual_row() {
            let cur = self.terminal.cursor();
            let cur_col = cur.col.min(cols.saturating_sub(1));
            let target = (cur_col as f32, cur.row as f32);
            let anim_secs = self.config.cursor_anim_secs;
            let visible = self.cursor_currently_visible();

            // Capture ghosts before retargeting — once `anim.to` advances we
            // lose the previous-target column/row. Bounding-box scan is in
            // buffer coords; prev_visible uses visual rows, so translate via
            // `live_grid_offset` (consistent because key_matches implies
            // view_offset hasn't changed since the snapshot).
            if anim_secs > 0.0 && key_matches {
                if let (Some(prev_anim), Some(snap)) = (
                    self.cursor_anim.as_ref(),
                    self.prev_visible.as_ref(),
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
                                let now_cell = self.terminal.visible_cell(vis_r, c);
                                if !is_blank_cell(&now_cell) {
                                    continue;
                                }
                                self.cursor_ghosts.push(CursorGhost {
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

            // Drop ghosts whose underlying cell got rewritten with new
            // content (e.g. user typed a replacement after the backspace),
            // or whose fade has run out.
            let now = std::time::Instant::now();
            let terminal = &self.terminal;
            self.cursor_ghosts.retain(|g| {
                let elapsed = now.duration_since(g.started_at).as_secs_f32();
                if anim_secs <= 0.0 || elapsed >= anim_secs {
                    return false;
                }
                let vis_r = g.buffer_row + live_grid_offset_i;
                is_blank_cell(&terminal.visible_cell(vis_r, g.col))
            });

            // Emit ghost glyphs as foreground-only quads with linearly
            // decaying alpha. Drawn before the cursor box so the cursor
            // visually consumes the ghost as it slides over.
            for ghost in &self.cursor_ghosts {
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
                emit_cell(
                    &mut vertices,
                    &mut indices,
                    GlyphSource::Char(ghost.ch),
                    variant,
                    vis_r as isize,
                    ghost.col,
                    fg,
                    [0.0; 4],
                );
            }

            let anim = self.cursor_anim.get_or_insert_with(|| CursorAnim::snapped(target));
            anim.retarget(target, anim_secs);

            if visible {
                let (eased_col, eased_buf_row) = anim.current(anim_secs);
                let eased_vis_row = eased_buf_row + live_grid_offset;
                let block_x = WINDOW_PADDING + eased_col * cell_w;
                // Cursor lives in the same per-row strip as the bg quad so
                // it aligns with selection / colored backgrounds.
                let cur_baseline =
                    WINDOW_PADDING + decorator_offset + (eased_vis_row + 1.0) * line_height;
                let block_y =
                    cur_baseline - bg_h - descender - (line_height - bg_h) * 0.5 + scroll_y;
                let cursor_color = palette::get().cursor;
                // Underline / bar use a 2-px stripe; block fills the full cell.
                let stripe = 2.0_f32;
                let (cx, cy, cw, ch) = match self.terminal.cursor_shape() {
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
            self.cursor_anim = None;
            self.cursor_ghosts.clear();
        }

        // Refresh the visible-grid snapshot with the current frame's cells
        // so the next retarget can spot what just got cleared. Keyed by
        // viewport so a resize / scrollback / alt-screen flip flushes the
        // comparison in `key_matches` above.
        let mut snap_cells: Vec<Vec<style::Cell>> = Vec::with_capacity(rows);
        for r in 0..rows {
            let mut row_cells: Vec<style::Cell> = Vec::with_capacity(cols);
            for c in 0..cols {
                row_cells.push(self.terminal.visible_cell(r, c));
            }
            snap_cells.push(row_cells);
        }
        self.prev_visible = Some(GridSnapshot {
            cells: snap_cells,
            key: viewport_key,
        });

        // 3. Edge fades: vertical gradient quads pinned to the top and bottom
        // of the window. The top one obscures content sliding up behind the
        // macOS traffic-light strip; the bottom one mirrors the effect so the
        // phantom row sliding into / out of the bottom edge dissolves rather
        // than clipping abruptly. Drawn last so they overlay every cell. RGB
        // is premultiplied with alpha to match PREMULTIPLIED_ALPHA_BLENDING.
        let win_w = self.gpu.config.width as f32;
        let win_h = self.gpu.config.height as f32;
        // Top fade is taller than the bottom: the title bar + toolbar takes
        // about DECORATOR_HEIGHT to fully occlude, and a longer gradient
        // below that gives content a soft runway as it scrolls into view
        // rather than popping out from a hard edge.
        let top_fade_height = self.config.top_fade_height;
        let bottom_fade_height_max = self.config.bottom_fade_height;
        let fade_rgb = [1.0, 1.0, 1.0];
        let clear = [0.0, 0.0, 0.0, 0.0];

        // Strip quads live in their own vertex/index buffer — they're drawn
        // by the blur strip pipeline in the composite pass.
        let mut strip_vertices: Vec<renderer::vertex::Vertex> = Vec::with_capacity(16);
        let mut strip_indices: Vec<u16> = Vec::with_capacity(32);
        let mut push_strip = |vertices: &mut Vec<renderer::vertex::Vertex>,
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

        // Premultiplied; passing an alpha multiplier scales RGB and alpha together.
        let scaled_opaque = |alpha: f32| {
            [
                fade_rgb[0] * alpha,
                fade_rgb[1] * alpha,
                fade_rgb[2] * alpha,
                alpha,
            ]
        };

        // Both fades emerge as the user scrolls away from a boundary. Each
        // edge tracks its own boundary independently — the top fade only
        // vanishes at the top of scrollback (where the topmost row sits
        // fully below the toolbar via decorator_offset), and the bottom
        // fade only vanishes at the live grid.
        let height_ramp_end = line_height * 4.0;

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

        self.gpu
            .queue
            .write_buffer(&self.vertex_buffer, 0, bytemuck::cast_slice(&vertices));
        self.gpu
            .queue
            .write_buffer(&self.index_buffer, 0, bytemuck::cast_slice(&indices));
        self.num_indices = indices.len() as u32;

        // Strip overlay: blur-only (tint = 0). The glyph fade already pulls
        // foreground text toward the bg color near each edge; the blur sits
        // on top to soften whatever's still visible in the gradient region.
        if !strip_indices.is_empty() {
            self.gpu.queue.write_buffer(
                &self.strip_vertex_buffer,
                0,
                bytemuck::cast_slice(&strip_vertices),
            );
            self.gpu.queue.write_buffer(
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
        self.gpu.queue.write_buffer(
            &self.fade_buffer,
            0,
            bytemuck::cast_slice(&fade_data),
        );
    }

    /// Bump (or shrink) the font by `delta_pt` points and rebuild everything
    /// that depends on cell metrics: atlas, font texture, bind group, terminal
    /// grid, vertex/index buffers. Clamped so the rasterizer never gets a
    /// nonsensical size.
    fn change_font_size(&mut self, delta_pt: f32) {
        let new_pt = (self.pt_size + delta_pt).clamp(6.0, 96.0);
        if (new_pt - self.pt_size).abs() < f32::EPSILON {
            return;
        }
        self.pt_size = new_pt;
        self.config.font_size = self.pt_size;
        self.config.save();
        self.font.set_char_size(self.pt_size, self.dpi);
        self.atlas = self.font.build_atlas();
        self.font_texture = renderer::texture::Texture::from_memory(
            &self.gpu.device,
            &self.gpu.queue,
            &self.atlas.buffer,
            self.atlas.width as u32,
            self.atlas.height as u32,
            wgpu::TextureFormat::R8Unorm,
            Some("font texture"),
        );
        self.font_bind_group = self.gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            layout: &self.font_bind_group_layout,
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
        let metrics = self.font.face().size_metrics().unwrap();
        let viewport = State::get_viewport_size(
            self.gpu.config.width as f32,
            self.gpu.config.height as f32,
            self.font.cell_width(),
            ((metrics.ascender - metrics.descender) >> 6) as usize,
        );
        self.terminal.resize(viewport.char_width, viewport.char_height);
        self.notify_pty_size(viewport.char_width, viewport.char_height);
        self.resize_buffers();
        self.cursor_anim = None;
        self.invalidate();
    }

    pub fn resize(&mut self, size: winit::dpi::PhysicalSize<u32>) {
        self.gpu.resize(size);
        if size.width > 0 && size.height > 0 {
            self.blur
                .resize(&self.gpu.device, &self.gpu.queue, size.width, size.height);
            // Glow samples the (just-recreated) blur scene texture, so its
            // bright-pass bind group has to be rebuilt against the new view.
            self.glow.resize(
                &self.gpu.device,
                &self.gpu.queue,
                size.width,
                size.height,
                &self.blur.scene.view,
            );
        }
        self.camera_uniform
            .update_view_proj(&self.camera, size.width as f32, size.height as f32);
        self.gpu.queue.write_buffer(
            &self.camera_buffer,
            0,
            bytemuck::cast_slice(&[self.camera_uniform]),
        );
        let metrics = self.font.face().size_metrics().unwrap();
        let size = State::get_viewport_size(
            self.gpu.config.width as f32,
            self.gpu.config.height as f32,
            self.font.cell_width(),
            ((metrics.ascender - metrics.descender) >> 6) as usize,
        );
        self.terminal.resize(size.char_width, size.char_height);
        self.notify_pty_size(size.char_width, size.char_height);
        self.resize_buffers();
        self.cursor_anim = None;
        self.invalidate();
    }

    fn notify_pty_size(&self, cols: usize, rows: usize) {
        let ws = libc::winsize {
            ws_row: rows as u16,
            ws_col: cols as u16,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        unsafe {
            libc::ioctl(self.master, libc::TIOCSWINSZ, &ws);
        }
    }

    fn write_pty(&self, bytes: &[u8]) {
        if let Err(e) = nix::unistd::write(self.master, bytes) {
            eprintln!("pty write failed: {e}");
        }
    }

    /// Effective blink state: DECSCUSR's request is gated by the user's
    /// `cursor_blink` config so opting out disables blinking globally.
    fn cursor_blink_enabled(&self) -> bool {
        self.config.cursor_blink && self.terminal.cursor_blink()
    }

    /// Combined visibility check: DECTCEM (cursor_visible) gates whether the
    /// cursor exists at all; blink only suppresses it on the "off" half-phase
    /// of the cycle when DECSCUSR has selected a blinking variant.
    fn cursor_currently_visible(&self) -> bool {
        self.terminal.cursor_visible() && (!self.cursor_blink_enabled() || self.blink_on)
    }

    /// If a blink half-cycle has elapsed, flip the phase and request a redraw.
    /// Returns true when the cursor visibility actually changed.
    fn maybe_blink_tick(&mut self) -> bool {
        if !self.cursor_blink_enabled() || !self.terminal.cursor_visible() {
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
        if self.cursor_blink_enabled() && self.terminal.cursor_visible() {
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
        let anim_active = match &self.cursor_anim {
            Some(a) => a.animating(self.config.cursor_anim_secs),
            None => false,
        };
        anim_active || !self.cursor_ghosts.is_empty()
    }

    /// True while either edge-fade phase is still chasing its target —
    /// used to keep the event loop ticking until the slide completes.
    fn is_top_fade_animating(&self) -> bool {
        let scrollback_len = if self.terminal.on_alt_screen() {
            0.0
        } else {
            self.terminal.scrollback_len() as f32
        };
        let view_offset = self.terminal.view_offset() as f32;
        let metrics = self.font.face().size_metrics().unwrap();
        let line_height = ((metrics.ascender - metrics.descender) >> 6) as f32;
        let scroll_y = self.scroll_y as f32;
        let dist_from_top = (scrollback_len - view_offset) * line_height - scroll_y;
        let dist_from_bottom = view_offset * line_height + scroll_y;
        let top_target = if dist_from_top > 0.0 { 1.0 } else { 0.0 };
        let bot_target = if dist_from_bottom > 0.0 { 1.0 } else { 0.0 };
        (self.top_fade_phase - top_target).abs() > f32::EPSILON
            || (self.bottom_fade_phase - bot_target).abs() > f32::EPSILON
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
        self.terminal.set_default_colors(to_u8(p.foreground), to_u8(p.background), to_u8(p.cursor));
    }

    /// 1-based (col, row) form of `pixel_to_visual_cell` for mouse reporting.
    fn pixel_to_cell(&self, px: f64, py: f64) -> (u16, u16) {
        let (c, r) = self.pixel_to_visual_cell(px, py);
        (c as u16 + 1, r as u16 + 1)
    }

    /// Forward a mouse event to the PTY in the host's preferred encoding,
    /// if any tracking mode is enabled. `motion` is set for drag/move events.
    fn report_mouse(&mut self, button: input::MouseButton, press: bool, motion: bool) {
        let mp = self.terminal.mouse_protocol();
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
            if self.last_reported_cell == Some((col, row)) {
                return;
            }
            self.last_reported_cell = Some((col, row));
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
        let metrics = self.font.face().size_metrics().unwrap();
        let line_height = ((metrics.ascender - metrics.descender) >> 6) as f64;
        let ascender = (metrics.ascender >> 6) as f64;
        let descender = (metrics.descender >> 6) as f64;
        let bg_h = ascender - descender;
        let cell_w = self.font.cell_width() as f64;
        // Mirror the renderer's dynamic decorator offset: full DECORATOR_HEIGHT
        // at both scroll-range boundaries (live grid and top of scrollback),
        // easing to 0 over one line in either direction. Out-of-sync formulas
        // here would drift the hit-test by a row vs. what's actually drawn.
        let view_offset = self.terminal.view_offset() as f64;
        let scrollback_len = if self.terminal.on_alt_screen() {
            0.0
        } else {
            self.terminal.scrollback_len() as f64
        };
        let dist_from_bottom = view_offset * line_height + self.scroll_y;
        let dist_from_top = (scrollback_len - view_offset) * line_height - self.scroll_y;
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
        let row = ((py - row_strip_top - self.scroll_y) / line_height).floor() as i64;
        let col = col.clamp(0, self.terminal.cols as i64 - 1) as usize;
        let row = row.clamp(0, self.terminal.rows as i64 - 1) as isize;
        (col, row)
    }

    /// Pixel coord → absolute (line, col) selection point.
    fn pixel_to_selection_point(&self, px: f64, py: f64) -> (isize, usize) {
        let (col, vrow) = self.pixel_to_visual_cell(px, py);
        (self.terminal.visual_to_abs_line(vrow), col)
    }

    /// Recompute the URL under the mouse pointer. Tracks Cmd state so the
    /// underline overlay and pointer cursor only appear while the user is
    /// actually holding the modifier; releasing Cmd clears the hover. Any
    /// state change here flips the system cursor icon and invalidates the
    /// frame so the underline can repaint.
    fn update_hover_url(&mut self) {
        let new = if self.modifiers.super_key() {
            let (col, vrow) = self.pixel_to_visual_cell(self.mouse_x, self.mouse_y);
            let abs_line = self.terminal.visual_to_abs_line(vrow);
            find_url_at(&self.terminal, abs_line, col)
        } else {
            None
        };
        if new == self.hover_url {
            return;
        }
        let icon = if new.is_some() {
            winit::window::CursorIcon::Pointer
        } else {
            winit::window::CursorIcon::Text
        };
        self.window.set_cursor_icon(icon);
        self.hover_url = new;
        self.invalidate();
    }

    /// Anchor a new selection at the mouse position. Click count cycles
    /// 1 → 2 → 3 → 1 for click sequences within the threshold on the same
    /// cell, picking Cell / Word / Line granularity respectively.
    fn handle_mouse_press(&mut self) {
        let p = self.pixel_to_selection_point(self.mouse_x, self.mouse_y);
        let now = std::time::Instant::now();
        let continued = self
            .last_click
            .map(|(t, c)| c == p && now.duration_since(t) < DOUBLE_CLICK_THRESHOLD)
            .unwrap_or(false);
        self.click_count = if continued { (self.click_count % 3) + 1 } else { 1 };
        self.last_click = Some((now, p));
        self.selection_mode = match self.click_count {
            1 => SelectionMode::Cell,
            2 => SelectionMode::Word,
            _ => SelectionMode::Line,
        };
        self.press_cell = Some(p);
        self.press_pixel = Some((self.mouse_x, self.mouse_y));
        // Word and Line modes show their selection on click. Cell mode waits
        // until the drag exceeds DRAG_THRESHOLD_PX so a plain click doesn't
        // briefly highlight a single character.
        self.selection = match self.selection_mode {
            SelectionMode::Cell => None,
            _ => self.compute_selection(p, p),
        };
    }

    /// Update the head of the active selection from the current mouse pos.
    fn handle_mouse_drag(&mut self) {
        let Some(p0) = self.press_cell else { return };
        if self.selection_mode == SelectionMode::Cell && self.selection.is_none() {
            let Some((px, py)) = self.press_pixel else { return };
            let dx = self.mouse_x - px;
            let dy = self.mouse_y - py;
            if dx * dx + dy * dy < DRAG_THRESHOLD_PX * DRAG_THRESHOLD_PX {
                return;
            }
        }
        let p = self.pixel_to_selection_point(self.mouse_x, self.mouse_y);
        self.selection = self.compute_selection(p0, p);
    }

    fn handle_mouse_release(&mut self) {
        self.press_cell = None;
        self.press_pixel = None;
    }

    /// Build a selection from two cells under the current `selection_mode`.
    /// In Word / Line mode, each end snaps outward to the word or line edge.
    fn compute_selection(&self, a: (isize, usize), b: (isize, usize)) -> Option<Selection> {
        let (start, end) = if a <= b { (a, b) } else { (b, a) };
        let (start, end) = match self.selection_mode {
            SelectionMode::Cell => (start, end),
            SelectionMode::Word => (self.word_start(start), self.word_end(end)),
            SelectionMode::Line => {
                let last = self.terminal.cols.saturating_sub(1);
                ((start.0, 0), (end.0, last))
            }
        };
        Some(Selection { anchor: start, head: end })
    }

    /// Walk left from `p` while the previous cell is a word char.
    fn word_start(&self, p: (isize, usize)) -> (isize, usize) {
        let Some(line) = self.terminal.line_at(p.0) else { return p };
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
        let Some(line) = self.terminal.line_at(p.0) else { return p };
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
        self.last_click = None;
        self.click_count = 0;
        if self.selection.is_some() {
            self.selection = None;
            true
        } else {
            false
        }
    }

    /// Materialize the current selection as plain text, trimming trailing
    /// whitespace per line and joining with '\n'.
    fn selection_text(&self) -> Option<String> {
        let sel = self.selection.as_ref()?;
        let (start, end) = sel.range();
        let mut out = String::new();
        for line in start.0..=end.0 {
            let Some(cells) = self.terminal.line_at(line) else { continue };
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

    /// Read the system clipboard and write it to the PTY, wrapped in
    /// bracketed-paste markers if the host has enabled them.
    fn paste_from_clipboard(&self) {
        let text = match arboard::Clipboard::new().and_then(|mut c| c.get_text()) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("clipboard read failed: {e}");
                return;
            }
        };
        if self.terminal.bracketed_paste() {
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
        elwt: &EventLoopWindowTarget<app_window::CustomEvent>,
    ) -> bool {
        match event {
            WindowEvent::CursorMoved { position, .. } => {
                self.mouse_x = position.x;
                self.mouse_y = position.y;
                // Mouse-mode reporting takes precedence unless the user is
                // shift-overriding it for local selection.
                let mouse_mode_active =
                    self.terminal.mouse_protocol().enabled() && !self.modifiers.shift_key();
                if mouse_mode_active {
                    if let Some(b) = self.held_button {
                        self.report_mouse(b, true, true);
                    } else if self.terminal.mouse_protocol().any_motion {
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
                        if let Some(hu) = self.hover_url.clone() {
                            open_url(&hu.url);
                            return true;
                        }
                    }
                    let mouse_mode_active = self.terminal.mouse_protocol().enabled()
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
                let m = self.font.face().size_metrics().unwrap();
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
                let gap = self.last_wheel_at.map(|t| now.duration_since(t));
                if matches!(phase, TouchPhase::Started)
                    && gap.map_or(true, |g| g >= FRESH_GESTURE_GAP)
                {
                    self.scroll_suppressed = false;
                }
                if self.scroll_suppressed {
                    // Don't advance `last_wheel_at` on suppressed events —
                    // otherwise the steady stream of momentum ticks keeps
                    // resetting the idle gap, and a real fresh gesture that
                    // arrives mid-momentum still looks like a 16ms follow-up.
                    return true;
                }
                self.last_wheel_at = Some(now);
                // Scroll-wheel forwarding to the PTY when an app has asked
                // for mouse tracking (vim, less, htop). Otherwise the wheel
                // drives our own scrollback viewport.
                if self.terminal.mouse_protocol().enabled() {
                    let lines = match delta {
                        MouseScrollDelta::LineDelta(_, d) => d.round() as i32,
                        MouseScrollDelta::PixelDelta(p) => (p.y / line_height) as i32,
                    };
                    let (button, count) = if lines > 0 {
                        (input::MOUSE_WHEEL_UP, lines.unsigned_abs())
                    } else if lines < 0 {
                        (input::MOUSE_WHEEL_DOWN, lines.unsigned_abs())
                    } else {
                        return true;
                    };
                    for _ in 0..count {
                        self.report_mouse(button, true, false);
                    }
                    return true;
                }
                // Alt screen has no scrollback to navigate; full-screen apps
                // (vim, less, htop) provide their own keyboard motion. Without
                // this guard, trackpad pixels would accumulate in scroll_y and
                // visually drift the grid past its bounds.
                if self.terminal.on_alt_screen() {
                    self.scroll_y = 0.0;
                    return true;
                }
                match delta {
                    MouseScrollDelta::LineDelta(_, d) => {
                        let n = d.round().abs() as usize;
                        if *d > 0.0 {
                            self.terminal.scroll_up(n);
                        } else if *d < 0.0 {
                            self.terminal.scroll_down(n);
                        }
                        // Discrete scrolls snap — don't leave a sub-line offset.
                        self.scroll_y = 0.0;
                    }
                    MouseScrollDelta::PixelDelta(p) => {
                        self.scroll_y += p.y;
                        // Drain accumulated pixels into discrete line scrolls.
                        // Zero the residue if scroll_up/down refused so scroll_y
                        // can't accumulate past a viewport boundary regardless
                        // of what at_top/at_bottom report.
                        while self.scroll_y >= line_height {
                            if !self.terminal.scroll_up(1) {
                                self.scroll_y = 0.0;
                                break;
                            }
                            self.scroll_y -= line_height;
                        }
                        while self.scroll_y <= -line_height {
                            if !self.terminal.scroll_down(1) {
                                self.scroll_y = 0.0;
                                break;
                            }
                            self.scroll_y += line_height;
                        }
                        // Hard-stop at viewport boundaries: no elastic overscroll.
                        if self.scroll_y > 0.0 && self.terminal.at_top() {
                            self.scroll_y = 0.0;
                        }
                        if self.scroll_y < 0.0 && self.terminal.at_bottom() {
                            self.scroll_y = 0.0;
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
                    // Cmd+C / Cmd+V: copy / paste through the system
                    // clipboard. Done before encode_key so the super_key
                    // check there doesn't drop them.
                    if self.modifiers.super_key() {
                        if let winit::keyboard::Key::Character(s) = &event.logical_key {
                            if s.eq_ignore_ascii_case("c") {
                                self.copy_selection();
                                return true;
                            }
                            if s.eq_ignore_ascii_case("v") {
                                self.paste_from_clipboard();
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
                                if self.wireframe_pipeline.is_some() {
                                    self.wireframe = !self.wireframe;
                                    self.window.request_redraw();
                                }
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
                        self.terminal.app_cursor_keys(),
                    );
                    if let Some(bytes) = bytes {
                        // A keystroke we're sending to the PTY snaps the view
                        // back to the live grid; passive modifiers (Cmd+C etc.)
                        // returned None and don't touch the scroll state.
                        self.terminal.scroll_to_bottom();
                        self.scroll_y = 0.0;
                        // Drop any in-flight trackpad momentum so the snap
                        // sticks — otherwise the tail of the flick keeps
                        // scrolling the view away from the bottom.
                        self.scroll_suppressed = true;
                        self.reset_blink();
                        self.clear_selection();
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
        let output = self.gpu.surface.get_current_texture().unwrap();
        let surface_wait = surface_t0.elapsed();
        let view = output
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder =
            self.gpu
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("terminal"),
                });

        // Fast path: when no edge-fade strips are visible AND glow is off,
        // the offscreen scene texture would never be sampled. Render the
        // scene directly to the swapchain and skip the composite pass
        // entirely (saves ~5 fullscreen passes per frame, the dominant cost
        // during PTY bursts).
        let needs_strips = self.num_strip_indices > 0;
        let glow_on = self.glow.enabled();
        let needs_offscreen = needs_strips || glow_on;
        let scene_target = if needs_offscreen { &self.blur.scene.view } else { &view };

        // 1. Scene → swapchain (fast path) or → offscreen (blur path).
        {
            let mut render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("scene pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: scene_target,
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

            let pipeline = if self.wireframe {
                self.wireframe_pipeline.as_ref().unwrap_or(&self.render_pipeline)
            } else {
                &self.render_pipeline
            };
            render_pass.set_pipeline(pipeline);
            render_pass.set_bind_group(0, &self.font_bind_group, &[]);
            render_pass.set_bind_group(1, &self.camera_bind_group, &[]);
            render_pass.set_bind_group(2, &self.fade_bind_group, &[]);
            render_pass.set_vertex_buffer(0, self.vertex_buffer.slice(..));
            render_pass.set_index_buffer(self.index_buffer.slice(..), wgpu::IndexFormat::Uint16);
            render_pass.draw_indexed(0..self.num_indices, 0, 0..1);
        }

        if needs_offscreen {
            // 2a. Glow runs against the (un-blurred) scene first so its
            // bright pass extracts crisp colour, not the post-blur smear.
            if glow_on {
                self.glow.run(&mut encoder);
            }
            // 2b. Dual-Kawase down/up over the scene for the strip pass.
            if needs_strips {
                self.blur.run(&mut encoder);
            }

            // 3. Composite to swapchain: blit the scene, additively layer
            // glow on top (if enabled), then alpha-blend the blur-sampled
            // strip quads (if any). Order matters: glow under strips so the
            // toolbar/edge fade reads cleanly over the bloom.
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

            pass.set_pipeline(&self.blur.blit_pipeline);
            pass.set_bind_group(0, self.blur.blit_bind_group(), &[]);
            pass.draw(0..3, 0..1);

            if glow_on {
                pass.set_pipeline(&self.glow.composite_pipeline);
                pass.set_bind_group(0, &self.glow.composite_bg, &[]);
                pass.draw(0..3, 0..1);
            }

            if needs_strips {
                pass.set_pipeline(&self.blur.strip_pipeline);
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
        }

        self.gpu.queue.submit(std::iter::once(encoder.finish()));
        output.present();

        Ok((surface_wait, !needs_offscreen))
    }
}

async fn run() {
    env_logger::init();
    let event_loop = EventLoopBuilder::<app_window::CustomEvent>::with_user_event()
        .build()
        .unwrap();
    let event_loop_proxy = event_loop.create_proxy();

    // create the pty before forking so we have the handle available
    let fdm: i32;
    unsafe {
        fdm = posix_openpt(O_RDWR);
        println!("fdm: {fdm}");
        if fdm < 0 {
            panic!("Error on posix_openpt()");
        }
    }

    // Fork before the window is created so we hold the master fd across setup.
    let pty = pty::fork_pty(fdm).expect("failed to fork pty");
    std::thread::spawn(move || {
        pty.run(|data| {
            let _ = event_loop_proxy.send_event(app_window::CustomEvent::PtyInput(data.to_owned()));
        });
    });

    let transparent = false; // needed because of a shadow bug
    let window = WindowBuilder::new()
        .with_title("Terminal")
        .with_titlebar_transparent(true)
        .with_transparent(transparent)
        .with_has_shadow(!transparent)
        .with_fullsize_content_view(true)
        .with_decorations(true)
        .with_blur(transparent)
        .build(&event_loop)
        .unwrap();

    // event_loop.set_control_flow(ControlFlow::Poll);

    let mut mono_prop = font_loader::system_fonts::FontPropertyBuilder::new()
        .monospace()
        .build();
    let mut mono_fonts = font_loader::system_fonts::query_specific(&mut mono_prop);
    mono_fonts.dedup();
    let installed = font_loader::system_fonts::query_all();

    // let family = &fonts[rand::prelude::random::<usize>() % fonts.len()];
    let primary_name = ["Iosevka Term", "Iosevka", "Fira Code", "Menlo"]
        .iter()
        .find_map(|want| mono_fonts.iter().find(|f| f.as_str() == *want))
        .or_else(|| mono_fonts.iter().find(|f| f.contains("Iosevka Term")))
        .or_else(|| mono_fonts.iter().find(|f| f.contains("Iosevka")))
        .expect("no monospace primary font found")
        .clone();
    println!("primary font: {}", primary_name);
    let primary_data = load_family(&primary_name).expect("failed to load primary font");

    let config = Config::load();
    // Install the color scheme before constructing State so style.rs and the
    // renderer see the right palette on their first read. Missing file is a
    // soft failure: warn and keep defaults so a typo in the config name
    // doesn't take the terminal down.
    if let Some(name) = &config.color_scheme {
        if let Some(path) = scheme_path(name) {
            match std::fs::read_to_string(&path) {
                Ok(src) => palette::install(palette::parse_yaml(&src)),
                Err(e) => eprintln!("palette: failed to read {}: {}", path.display(), e),
            }
        }
    }
    let pt_size = config.font_size;
    let dpi = (window.scale_factor() * 96.0) as u32;
    // Build the rustybuzz shaper alongside the FreeType font. We keep one
    // copy of the bytes for shaping (rustybuzz parses tables, doesn't
    // rasterize) and hand the other to FreeType. Only primary cuts are
    // shaped — fallbacks aren't asked to ligate.
    let mut shaper = shaper::Shaper::new();
    shaper.set_variant(font::FaceVariant::Regular, &primary_data);
    let mut font = font::Font::new(primary_data);
    font.set_char_size(pt_size, dpi);

    // Bold/italic/bold-italic primary cuts of the same family. Each is best-
    // effort: when a cut isn't installed the styled lookup falls back to the
    // regular face. Cores like Iosevka ship all four; users without them get
    // un-styled text rather than synthetic bolding/oblique.
    for (variant, bold, italic) in [
        (font::FaceVariant::Bold, true, false),
        (font::FaceVariant::Italic, false, true),
        (font::FaceVariant::BoldItalic, true, true),
    ] {
        let Some(data) = load_family_styled(&primary_name, bold, italic) else {
            continue;
        };
        shaper.set_variant(variant, &data);
        if font.set_variant(variant, data, pt_size, dpi) {
            println!("primary {:?}: {}", variant, primary_name);
        }
    }

    // Pre-shape every candidate ligature sequence for each installed
    // variant. After this, the render loop only needs prefix-matching
    // against a small per-variant table — no rustybuzz on the hot path.
    for variant in font::FaceVariant::ALL {
        shaper.precompute(variant);
    }

    // Fallback chain. Each entry is a list of candidate family substrings; the
    // first installed family wins. Order matters — earlier fallbacks shadow
    // later ones for any glyph they share.
    let fallback_categories: &[(&str, &[&str])] = &[
        // Nerd Font icons (Powerline, Devicons, Font Awesome, …) in the PUA.
        ("nerd", &[
            "Iosevka Nerd Font",
            "FiraCode Nerd Font",
            "JetBrainsMono Nerd Font",
            "Hack Nerd Font",
            "Symbols Nerd Font",
        ]),
        // CJK ideographs and kana.
        ("cjk", &[
            "PingFang SC",
            "Hiragino Sans",
            "Noto Sans CJK SC",
            "Noto Sans CJK JP",
            "Sarasa Mono SC",
        ]),
        // Long-tail symbols, math, dingbats, geometric shapes.
        ("symbols", &[
            "Apple Symbols",
            "Symbola",
            "Noto Sans Symbols 2",
            "Noto Sans Symbols",
        ]),
        // Monochrome emoji. (Apple Color Emoji is bitmap-only and currently
        // unsupported by our atlas pipeline, so we deliberately skip it.)
        ("emoji", &["Noto Emoji"]),
    ];
    // For each fallback category, attach the matching cut to every variant we
    // managed to install a primary for. A bold CJK glyph still wants the bold
    // CJK fallback; if no styled CJK is installed, the styled variant is left
    // without that fallback and Atlas::lookup tumbles down to Regular.
    let variants_to_fill = [
        (font::FaceVariant::Regular, false, false),
        (font::FaceVariant::Bold, true, false),
        (font::FaceVariant::Italic, false, true),
        (font::FaceVariant::BoldItalic, true, true),
    ];
    for (label, candidates) in fallback_categories {
        let Some(family) = pick_family(&installed, candidates) else {
            continue;
        };
        for (variant, bold, italic) in variants_to_fill {
            // Regular has no installed primary check — Font::new always
            // populates it. Styled variants only get fallbacks when their
            // primary face is installed; otherwise the chain is dead weight.
            if variant != font::FaceVariant::Regular
                && font.variants[variant as usize].face.is_none()
            {
                continue;
            }
            let Some(data) = load_family_styled(&family, bold, italic) else {
                continue;
            };
            if font.add_fallback(variant, data, pt_size, dpi) {
                println!("fallback {} {:?}: {}", label, variant, family);
            }
        }
    }

    let mut state = State::new(fdm, window, font, shaper, config, dpi).await;
    state.notify_pty_size(state.terminal.cols, state.terminal.rows);
    state.window.set_cursor_icon(winit::window::CursorIcon::Text);
    state.sync_theme_colors();
    state.invalidate();

    let mut theme = state.window.theme().unwrap_or(winit::window::Theme::Light);

    let _ = event_loop.run(move |event, elwt| {
        match event {
            Event::UserEvent(n) => match n {
                app_window::CustomEvent::PtyInput(z) => {
                    let bytes = z.len();
                    let t0 = std::time::Instant::now();
                    state.terminal.feed(&z);
                    let reply = state.terminal.take_response();
                    if !reply.is_empty() {
                        state.write_pty(&reply);
                    }
                    state.perf.note_pty(bytes, t0.elapsed());
                    state.invalidate();
                    // New / removed cells may have changed which URL (if any)
                    // sits under the pointer.
                    state.update_hover_url();
                }
            },
            Event::WindowEvent { window_id, event } if window_id == state.window.id() => {
                if !state.input(&event, elwt) {
                    match event {
                        WindowEvent::ThemeChanged(new_theme) => {
                            theme = new_theme;
                            state.sync_theme_colors();
                            state.invalidate();
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
                            state.window.request_redraw();
                        }
                        WindowEvent::RedrawRequested => {
                            state.update();
                            state.flush_vertices();
                            let t0 = std::time::Instant::now();
                            let result = state.render(clear_color(theme));
                            let render_dur = t0.elapsed();
                            match result {
                                Ok((surface_wait, fast)) => {
                                    state.perf.note_render(render_dur, surface_wait, fast);
                                }
                                Err(wgpu::SurfaceError::Lost) => state.resize(state.gpu.size),
                                Err(wgpu::SurfaceError::OutOfMemory) => elwt.exit(),
                                Err(e) => eprintln!("{:?}", e),
                            }
                        }
                        _ => (),
                    }
                }
            }
            Event::AboutToWait => {
                if state.maybe_blink_tick() {
                    state.invalidate();
                }
                // Edge-fade and cursor-position eases: keep ticking frames
                // as long as either is still chasing its target.
                let animating =
                    state.is_top_fade_animating() || state.is_cursor_animating();
                if animating {
                    state.invalidate();
                }
                state.perf.maybe_flush();
                let next_anim = if animating {
                    Some(std::time::Instant::now() + ANIM_FRAME)
                } else {
                    None
                };
                let next_wake = [
                    state.next_blink_wake(),
                    next_anim,
                    state.perf.next_wake(),
                ]
                .into_iter()
                .flatten()
                .min();
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
#[cfg(target_os = "macos")]
fn load_family_styled(family: &str, bold: bool, italic: bool) -> Option<Vec<u8>> {
    font_loader::system_fonts::get_strict(family, bold, italic).map(|(data, _)| data)
}

#[cfg(not(target_os = "macos"))]
fn load_family_styled(family: &str, bold: bool, italic: bool) -> Option<Vec<u8>> {
    let mut b = font_loader::system_fonts::FontPropertyBuilder::new().family(family);
    if bold {
        b = b.bold();
    }
    if italic {
        b = b.italic();
    }
    font_loader::system_fonts::get(&b.build()).map(|(data, _)| data)
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
        let a = CursorAnim::snapped((3.0, 4.0));
        assert_eq!(a.from, (3.0, 4.0));
        assert_eq!(a.to, (3.0, 4.0));
        // current() should immediately return the target regardless of duration.
        assert!(approx_pair(a.current(0.2), (3.0, 4.0)));
    }

    #[test]
    fn current_returns_to_when_duration_zero_or_negative() {
        let a = anim_at_t((0.0, 0.0), (10.0, 4.0), 0.2, 0.5);
        assert!(approx_pair(a.current(0.0), (10.0, 4.0)));
        assert!(approx_pair(a.current(-1.0), (10.0, 4.0)));
    }

    #[test]
    fn current_returns_to_after_duration_elapses() {
        // Back-date 10s to guarantee elapsed >= duration for any reasonable duration.
        let a = CursorAnim {
            from: (0.0, 0.0),
            to: (10.0, 4.0),
            started_at: Instant::now() - Duration::from_secs(10),
        };
        assert!(approx_pair(a.current(0.2), (10.0, 4.0)));
    }

    #[test]
    fn smoothstep_midpoint_is_half() {
        // smoothstep(0.5) = 0.25 * (3 - 1) = 0.5
        let a = anim_at_t((0.0, 0.0), (10.0, 4.0), 0.2, 0.5);
        let p = a.current(0.2);
        assert!(approx_pair(p, (5.0, 2.0)), "got {:?}", p);
    }

    #[test]
    fn smoothstep_quarter_point() {
        // smoothstep(0.25) = 0.0625 * (3 - 0.5) = 0.15625
        let a = anim_at_t((0.0, 0.0), (10.0, 4.0), 0.2, 0.25);
        let p = a.current(0.2);
        assert!(approx_pair(p, (1.5625, 0.625)), "got {:?}", p);
    }

    #[test]
    fn animating_false_for_snapped() {
        let a = CursorAnim::snapped((3.0, 4.0));
        assert!(!a.animating(0.2));
    }

    #[test]
    fn animating_true_in_flight_false_after_duration() {
        let mid = anim_at_t((0.0, 0.0), (10.0, 4.0), 0.2, 0.5);
        assert!(mid.animating(0.2));

        let done = CursorAnim {
            from: (0.0, 0.0),
            to: (10.0, 4.0),
            started_at: Instant::now() - Duration::from_secs(10),
        };
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
        // Midway through a 0.0 -> 10.0 (x) ease, eased x = 5.0.
        let mut a = anim_at_t((0.0, 0.0), (10.0, 4.0), 0.2, 0.5);
        let pre = a.current(0.2);
        assert!(approx_pair(pre, (5.0, 2.0)));

        a.retarget((20.0, 8.0), 0.2);

        // New `from` should equal the displayed-at-retarget position.
        assert!(approx_pair(a.from, (5.0, 2.0)), "from = {:?}", a.from);
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
        // Garbage value must not poison the rest of the config — the field
        // stays at its default (false) and other keys still parse.
        let parsed = Config::parse_str("cursor_blink = banana\nfont_size = 12.5\n");
        assert!(!parsed.cursor_blink);
        assert!(approx_eq(parsed.font_size, 12.5));
    }

    #[test]
    fn config_defaults_glow_disabled() {
        let c = Config::defaults();
        assert!(!c.glow_match_saturation);
        assert!(!c.glow_match_bright_ansi);
        assert!(!c.glow_match_foreground);
        assert!((0.0..=1.0).contains(&c.glow_threshold));
        assert!(c.glow_intensity > 0.0);
        assert!((0.0..=180.0).contains(&c.glow_hue_tolerance_deg));
        assert!(c.glow_fg_tolerance >= 0.0 && c.glow_fg_tolerance <= 3.0_f32.sqrt());
        assert!(c.glow_iterations >= 1);
    }

    #[test]
    fn config_round_trip_preserves_glow_fields() {
        let mut c = Config::defaults();
        c.glow_match_saturation = true;
        c.glow_match_bright_ansi = true;
        c.glow_match_foreground = true;
        c.glow_threshold = 0.42;
        c.glow_intensity = 1.75;
        c.glow_softness = 0.25;
        c.glow_hue_tolerance_deg = 22.5;
        c.glow_fg_tolerance = 0.20;
        c.glow_iterations = 5;
        let parsed = Config::parse_str(&c.serialize());
        assert!(parsed.glow_match_saturation);
        assert!(parsed.glow_match_bright_ansi);
        assert!(parsed.glow_match_foreground);
        assert!(approx_eq(parsed.glow_threshold, 0.42));
        assert!(approx_eq(parsed.glow_intensity, 1.75));
        assert!(approx_eq(parsed.glow_softness, 0.25));
        assert!(approx_eq(parsed.glow_hue_tolerance_deg, 22.5));
        assert!(approx_eq(parsed.glow_fg_tolerance, 0.20));
        assert_eq!(parsed.glow_iterations, 5);
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
        let parsed = Config::parse_str("glow_threshold = 2.5\nglow_threshold = -1\n");
        assert!((0.0..=1.0).contains(&parsed.glow_threshold));
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
            "glow_match_saturation = nope\nglow_match_bright_ansi = ?\nglow_intensity = abc\n",
        );
        let d = Config::defaults();
        assert_eq!(parsed.glow_match_saturation, d.glow_match_saturation);
        assert_eq!(parsed.glow_match_bright_ansi, d.glow_match_bright_ansi);
        assert!(approx_eq(parsed.glow_intensity, d.glow_intensity));
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
        assert_eq!(hover.start_abs_line, 0);
        assert_eq!(hover.end_abs_line, 1);
        assert_eq!(hover.start_col, 0);
        assert_eq!(hover.end_col, 18, "39 chars over 20 cols ends at col 18 of row 1");
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
        assert_eq!(hover.start_abs_line, 0);
        assert_eq!(hover.end_abs_line, 0);
        assert_eq!(hover.start_col, 0);
        assert_eq!(hover.end_col, 18);
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
        assert_eq!(hover.start_abs_line, start_abs);
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
        assert_eq!(hover.start_abs_line, 0);
        assert_eq!(hover.end_abs_line, 0);
        assert_eq!(hover.start_col, 0);
        assert_eq!(hover.end_col, 18);
        assert_eq!(hover.url, "https://example.com");
    }
}
