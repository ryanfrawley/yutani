# In-process multi-window — implementation plan

## Goal

Make Cmd-N (and the "New window" palette command) open a new window **in the
same process**, reusing the expensive, already-built resources instead of
launching a fresh `yutani` subprocess that pays the full ~330–410ms startup
again. Target: a new window appears in well under ~150ms (essentially just the
OS `NSWindow` cost plus a surface and per-window buffers).

This supersedes the subprocess approach shipped in PR #108 (Cmd-N spawns a new
process) — that stays as the fallback until Stage 3 lands.

## Why this is tractable

1. **The winit event loop is single-threaded.** Every window lives on the main
   thread, so shared resources can be `Rc<RefCell<…>>` — no `Send`/`Sync`, no
   `Arc<Mutex>`, no cross-thread borrow gymnastics. This is the single biggest
   simplifier and the reason the refactor is days, not weeks.
2. **The event loop already dispatches by `WindowId`.** `Event::WindowEvent`
   carries `window_id`; today it's gated on `window_id == state.window.id()`.
   Multi-window is "look the window up in a map" rather than a redesign.
3. **Atlas build is now cheap (~6ms, lazy).** Thanks to the startup work
   (PR #110), the per-window glyph atlas no longer needs to be shared — which
   neatly sidesteps the hardest sharing problem (see "Per-window atlas",
   below). The expensive thing to share is the **font load** (~140ms), which is
   shareable.

## Three-tier model: process → window → tab

Today's `State` (~80 fields) conflates three levels that multi-window — and
**tabs** — pull apart. Even though the first shipped feature is multi-window
(one tab per window), the window/tab boundary must be designed in now, because
a tab shares almost everything a window owns (surface, atlas, camera, buffers)
*except* the PTY + terminal + the interaction state bound to that shell. Getting
this boundary wrong means refactoring `State` a third time when tabs land. So:

```
AppShared            (one per process)
└── WindowState      (one per OS window)   — owns the surface, atlas, renderer
    └── TabState     (one per shell/PTY)   — owns the terminal + its interaction
```

### `AppShared` — once per process (`Rc`, shared by all windows)
- **GPU device + queue** — `Rc<gpu::Gpu>` (split out of today's `GpuContext`).
  Avoids re-initializing the adapter/device.
- **Font stack** — `Rc<RefCell<font::Font>>` (FreeType faces) and
  `Rc<RefCell<shaper::Shaper>>`. **This is the ~140ms win.** FreeType `Face` is
  not `Send`, but on a single thread `Rc<RefCell>` is fine. `ensure_char`/
  `ensure_glyph_id` take `&mut Font`, satisfied by `borrow_mut()`.
- **Pipelines + bind-group layouts** that depend only on `device` + shader +
  surface format: `render_pipeline`, `wireframe_pipeline`, `image_pipeline`,
  `font_bind_group_layout`, `camera_bind_group_layout`, etc. Sharing these
  avoids repeat shader compilation (the cold-start spike).

### `WindowState` — once per OS window
Everything tied to the surface, the visual size, and the keyboard/pointer focus
(which are per-window, not per-tab). All tabs in a window share these:
- **Surface + surface config + size** (split out of `GpuContext`; the surface
  is tied to each `NSView`).
- **`Window`**, window chrome (title-bar band), and — later — the **tab bar**.
- **`pt_size` + `dpi`** — per-window so zoom (Cmd-±) and multi-monitor DPI work
  independently. All tabs in a window share one zoom/DPI, which is exactly why
  the atlas can be shared across the window's tabs.
- **Atlas + `font_texture` + `font_bind_group`** — per-window, **shared across
  the window's tabs** (same metrics). Rebuilt per window (~6ms) from the
  *shared* faces. Per-window (not per-process) lets two windows differ in
  DPI/zoom without fighting over one texture and avoids shared-mutable-atlas
  coordination. `ensure_char(&mut atlas, &mut shared_font.borrow_mut(), …)`.
- **Camera + fade + vertex/index/strip buffers** — sized to the window; reused
  to render whichever tab is active.
- **`blur` / `glow` / `glow_fg` / `scene_fg` + their bind groups** — they bundle
  pipelines *and* size-dependent textures (see `renderer/{blur,glow}.rs`), so
  initially they stay per-window (rebuilt, ~30ms). Splitting their pipelines out
  to `AppShared` is a possible later optimization, not required.
- **Focus-scoped input**: `modifiers`, `mouse_x/y`, `held_button`, click/drag
  tracking, `over_toolbar`, the command-palette overlay, blink phase. These
  belong to the focused window and operate on its **active** tab.
- **`tabs: Vec<TabState>` + `active: usize`** — the tab list and selection.

### `TabState` — once per shell/PTY
The shell session and the state that scrolls/selects/completes against it:
- **PTY** (`master` fd + reader thread) and **`Terminal`** grid + scrollback.
- **Scroll**: `scroll_y`, `alt_scroll_anim`, `wheel_pty_accum`,
  `scroll_suppressed`, `last_wheel_at`, `last_reported_cell`.
- **Selection**: `selection`, `selection_mode`, `press_cell`, `press_pixel`,
  `last_click`, `click_count`, `hover_url`.
- **Shell-derived**: `completions*`, `command_history`, per-tab title/cwd, the
  submitted-command/histfile plumbing.
- **Per-tab visual transients**: `cursor_anim`, `cursor_ghosts`, `prev_visible`
  (grid snapshot for diffing) — a tab freezes its animation state when
  backgrounded and resumes on activation.

Only the **active tab** is rendered: `update_vertices` reads
`window.tabs[window.active].terminal` and fills the window's buffers. Switching
tabs repoints to another `TabState`, invalidates, and rebuilds vertices; the
shared atlas means glyphs already rasterized by one tab are free for the others.

**First-cut scope:** build the `WindowState`/`TabState` split and run with
exactly one tab per window. The **tab-bar chrome, tab keybindings (new/close/
next/prev tab), and tab drag/reorder are deferred** to a follow-up — but the
data model above lands now so that follow-up is additive, not another rewrite.

## Known constraints / non-goals (first cut)
- **Palette is process-global** (`palette::install`). All windows *and tabs*
  share one color scheme initially. Per-window/per-tab themes would require
  de-globalizing the palette — a separate effort, explicitly out of scope.
- **Uniform surface format** assumed across windows (same GPU/preference). True
  in practice; assert it when creating a window's surface.
- **Command history / completions are per-tab** (each shell is independent). No
  cross-tab or cross-window history sharing.
- **Tab UI is deferred.** This effort builds the `WindowState`/`TabState` data
  model and runs one tab per window. The tab bar, tab keybindings, and
  drag/reorder are a follow-up that builds on the model — not in this scope.
- **Background-tab resize is lazy.** On a window resize only the active tab's
  grid reflows immediately; background tabs reflow on activation, so dragging a
  window doesn't reflow N shells at once. (Single-tab today, so this only bites
  once tabs ship — but the activation path must call `notify_pty_size`.)

## PTY event routing

Today `CustomEvent::PtyInput(String)` / `PtyExit(i32)` implicitly target the one
window. With tabs, the unit that owns a PTY is the **tab**, not the window, so
route by a process-unique `TabId` rather than `WindowId`:

```rust
struct TabId(u64);   // process-unique, minted per PTY

enum CustomEvent {
    PtyInput(TabId, String),
    PtyExit(TabId, i32),
}
```

Each reader thread clones the `EventLoopProxy` and tags sends with its tab's
`TabId`. The loop resolves `TabId → (WindowId, tab index)` via a small
`HashMap<TabId, WindowId>` (plus the window's own tab list), then feeds the
right `TabState` — whether or not that tab is currently foregrounded (a
background tab still consumes shell output; it just doesn't trigger a redraw
unless it's the active tab or signals a bell/title change). Routing by `TabId`
from the start means Stage 1 doesn't bake in a window-only assumption it has to
unwind later.

## Staged delivery

Each stage builds, passes `cargo test`, and (except the last two) is a
behavior-preserving refactor — so regressions are caught early and the risky
extraction is decoupled from the user-visible feature.

### Stage 0 — Split `GpuContext`; extract `AppShared` (no behavior change)
- Split `gpu::GpuContext` into `gpu::Gpu { device, queue }` (shared) and a
  per-window `WindowSurface { surface, config, size }`.
- Introduce `AppShared` holding `Rc<Gpu>`, `Rc<RefCell<Font>>`,
  `Rc<RefCell<Shaper>>`, shared pipelines + layouts.
- `State` keeps its current single-window behavior but reaches GPU/font/pipeline
  through `AppShared`. Mechanical but wide: touches every `self.gpu.device`,
  `self.font`, `self.render_pipeline`, `self.shaper`, `self.atlas` site.
- **Highest-risk, highest-value stage.** Ships nothing new; fully testable
  against current behavior. Do this first and verify with the app + tests.

### Stage 1 — Split `State` into `WindowState` + `TabState` (no behavior change)
- Carve the per-tab fields (PTY, terminal, scroll/selection/completions/
  shell-derived/cursor-anim per the model above) into a `TabState`; the
  remainder becomes `WindowState` with `tabs: Vec<TabState>` + `active: usize`.
- All methods that touch terminal/scroll/selection move to `TabState` or take
  `&mut self.active_tab()`; the render path reads the active tab.
- Still one window, **one tab**. Pure restructure — verify identical behavior.
  Doing this before the registry means the `TabId` routing in Stage 2 has a real
  `TabState` to land on.

### Stage 2 — `WindowId`/`TabId` registry (no behavior change)
- Replace the single binding in `run()` with `HashMap<WindowId, WindowState>` +
  `AppShared`, plus a `HashMap<TabId, WindowId>` resolver.
- Add `TabId` to `CustomEvent`; route `UserEvent` by `TabId` and `WindowEvent`
  by `WindowId`. Mint a `TabId` per PTY reader thread.
- Still one window / one tab at startup. Verify identical behavior.

### Stage 3 — Window + tab factories
- `fn create_tab(shared, ...) -> (TabId, TabState)` — fork the PTY, spawn its
  reader thread tagging events with the new `TabId`, build the `Terminal`.
- `fn create_window(shared, elwt, cwd) -> WindowState` — build the `NSWindow`,
  surface, per-window buffers + atlas, and an initial tab via `create_tab`,
  reusing `AppShared`. `run()` calls it once for the initial window.

### Stage 4 — Wire Cmd-N to in-process spawn (the feature)
- Cmd-N handler and palette `NewWindow` call `create_window` via the
  `EventLoopWindowTarget` available in the event handler, instead of
  `spawn_new_window` (subprocess). Cascade off the spawning window's live
  position directly (drop the `YUTANI_CASCADE_FROM` env-var hack).
- Lifecycle: `CloseRequested` removes the window (and frees its tabs' `TabId`s)
  from the maps; quit when no windows remain. `PtyExit` closes just that tab
  (and the window if it was the last tab) per `shell_exit_mode`.

### Stage 5 — Cleanup
- Remove the subprocess path (`spawn_new_window`, `cascade_position`,
  `CASCADE_ENV`, `WINDOW_CASCADE_STEP`).
- Per-window title/cwd/theme polish; make sure `Cmd-W`/last-window semantics are
  right.

### Follow-up (separate effort, not this scope) — tab UX
With the model in place: tab-bar chrome + hit-testing, keybindings (new/close/
next/prev tab), `create_tab` wired to "new tab in this window", tab reorder.
Each is additive against the Stage 1 data model.

## Principal risks
1. **`RefCell` borrow overlaps at runtime.** `ensure_char` borrows the shared
   `Font` mutably during `update_vertices`. Audit that no other live borrow of
   the font is held across that call. (Single-threaded, so this is a logic
   check, not a data race.)
2. **The Stage 0 extraction is broad.** ~5,000 lines of `State` methods
   reference the soon-to-be-shared fields. Mitigate by doing it as pure
   mechanical extraction with no feature change and leaning on the test suite +
   a manual run.
3. **Per-window resize/scale changing the shared format** — assert format
   stability; if a window lands on a different-format surface, fall back to its
   own pipeline set (unlikely; document the assumption).
4. **wgpu 0.18 `Device` may not be `Clone`** — irrelevant, we share via `Rc`.
5. **`WindowState`/`TabState` split touches the same ~5,000 lines as Stage 0.**
   Two wide refactors back to back. Keep them as separate, individually-verified
   stages (don't interleave) so a regression is bisectable to one of them.
6. **Method placement churn.** Many `State` methods will move to `TabState` or
   gain an `active_tab()` hop. Risk of accidentally changing behavior mid-move;
   mitigate with the test suite and by moving, not rewriting.

## Rough effort
- Stage 0 (AppShared): ~1 day (wide, careful).
- Stage 1 (WindowState/TabState split): ~1 day (second wide refactor).
- Stage 2 (registry + TabId routing): ~half day.
- Stage 3 (factories): ~half day.
- Stage 4 (wire Cmd-N): ~half day.
- Stage 5 (cleanup + tests): ~half day.

Total ≈ 3–4 focused days for the multi-window feature with the tab-ready model.
The tab UX follow-up is additional. Stages 0 and 1 are the gates; if they land
clean, the rest is straightforward.
