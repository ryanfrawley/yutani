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

## What's shared vs per-window

The current `State` (~80 fields) splits cleanly:

### Shared once per process (`AppShared`)
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

### Per-window (`WindowState`)
- **Surface + surface config + size** (split out of `GpuContext`; the surface
  is tied to each `NSView`).
- **`Window`**, **PTY** (`master` fd + reader thread), **`Terminal`** grid +
  scrollback.
- **`pt_size` + `dpi`** — per-window so zoom (Cmd-±) and multi-monitor DPI work
  independently.
- **Atlas + `font_texture` + `font_bind_group`** — per-window. Rebuilt per
  window (~6ms) from the *shared* faces. Keeping the atlas per-window is what
  lets two windows have different DPI/zoom without fighting over one texture,
  and avoids shared-mutable-atlas coordination. `ensure_char(&mut atlas,
  &mut shared_font.borrow_mut(), …)`.
- **Camera + fade + vertex/index/strip buffers** — sized to the window.
- **`blur` / `glow` / `glow_fg` / `scene_fg` + their bind groups** — they bundle
  pipelines *and* size-dependent textures (see `renderer/{blur,glow}.rs`), so
  initially they stay per-window (rebuilt, ~30ms). Splitting their pipelines out
  to `AppShared` is a possible later optimization, not required.
- All **input/interaction state**: modifiers, mouse, selection, scroll anim,
  blink, completions, command palette, hover-url, command history, perf log.

## Known constraints / non-goals (first cut)
- **Palette is process-global** (`palette::install`). All windows share one
  color scheme initially. Per-window themes would require de-globalizing the
  palette — a separate effort, explicitly out of scope.
- **Uniform surface format** assumed across windows (same GPU/preference). True
  in practice; assert it when creating a window's surface.
- **Command history / completions** are per-window (each shell is independent).
  No cross-window history sharing.

## PTY event routing

Today `CustomEvent::PtyInput(String)` / `PtyExit(i32)` implicitly target the one
window. Each window gets its own PTY reader thread, so the events must carry the
target:

```rust
enum CustomEvent {
    PtyInput(WindowId, String),
    PtyExit(WindowId, i32),
}
```

Each reader thread clones the `EventLoopProxy` and tags sends with its window's
`WindowId`. The loop routes to `windows.get_mut(&id)`.

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

### Stage 1 — `WindowId`-keyed registry (no behavior change)
- Replace the single `state` binding in `run()` with
  `HashMap<WindowId, WindowState>` + the `AppShared`.
- Add `WindowId` to `CustomEvent`; route `UserEvent` and `WindowEvent` by id.
- Still exactly one window created at startup. Verify identical behavior.

### Stage 2 — Window factory
- Extract `fn create_window(shared, elwt, cwd) -> WindowState` that builds the
  `NSWindow`, surface, per-window buffers, atlas, and PTY (forking + spawning
  its reader thread, tagging events with the new `WindowId`), reusing
  `AppShared`. `run()` calls it once for the initial window.

### Stage 3 — Wire Cmd-N to in-process spawn (the feature)
- Cmd-N handler and palette `NewWindow` call `create_window` via the
  `EventLoopWindowTarget` available in the event handler, instead of
  `spawn_new_window` (subprocess). Cascade off the spawning window's live
  position directly (drop the `YUTANI_CASCADE_FROM` env-var hack).
- Lifecycle: `CloseRequested` removes the window from the map; quit when the map
  empties. `PtyExit` closes just that window per `shell_exit_mode`.

### Stage 4 — Cleanup
- Remove the subprocess path (`spawn_new_window`, `cascade_position`,
  `CASCADE_ENV`, `WINDOW_CASCADE_STEP`).
- Per-window title/cwd/theme polish; make sure `Cmd-W`/last-window semantics are
  right.

## Principal risks
1. **`RefCell` borrow overlaps at runtime.** `ensure_char` borrows the shared
   `Font` mutably during `update_vertices`. Audit that no other live borrow of
   the font is held across that call. (Single-threaded, so this is a logic
   check, not a data race.)
2. **The Stage 0 extraction is broad.** ~5,000 lines of `State` methods
   reference the soon-to-be-shared fields. Mitigate by doing it as pure
   mechanical extraction with no feature change and leaning on the test suite +
   a manual run.
3. **Per-window resize/scale changning the shared format** — assert format
   stability; if a window lands on a different-format surface, fall back to its
   own pipeline set (unlikely; document the assumption).
4. **wgpu 0.18 `Device` may not be `Clone`** — irrelevant, we share via `Rc`.

## Rough effort
- Stage 0: ~1 day (wide, careful).
- Stages 1–2: ~1 day.
- Stage 3: ~half day.
- Stage 4 + polish + tests: ~half day.

Total ≈ 2–3 focused days. Stage 0 is the gate; if it lands clean, the rest is
straightforward.
