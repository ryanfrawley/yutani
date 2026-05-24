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
  not `Send`, but on a single thread `Rc<RefCell>` is fine.
  **Borrow discipline is the load-bearing detail (see Risk 1):** `self.font` is
  read at ~20 sites across `render`/`update_vertices`, and `ensure_char`/
  `ensure_glyph_id` need `&mut Font` *in the middle of* that read-heavy method.
  A naive `let font = self.font.borrow();` at method top will compile and then
  **panic at runtime** when the later `borrow_mut()` overlaps — and the test
  suite won't catch it (tests use `Font` directly, never the `RefCell`). So the
  `RefCell` must **never** be exposed raw to `State` methods. Instead expose two
  narrow accessors as a **Stage 0 deliverable**:
  - `fn with_font<R>(&self, f: impl FnOnce(&Font) -> R) -> R` — borrow, run,
    drop in one expression (callers can't hold the `Ref` open).
  - the fill path stays a single statement:
    `self.atlas.ensure_char(&mut shared.font.borrow_mut(), …)` so the `RefMut`
    can't escape.
  This turns a per-call-site audit into a type-enforced invariant.
- **Pipelines + bind-group layouts** that depend only on `device` + shader +
  surface format: `render_pipeline`, `wireframe_pipeline`, `image_pipeline`,
  `font_bind_group_layout`, `camera_bind_group_layout`, **and the blur/glow
  pipelines** (see below). Sharing these avoids repeat shader compilation (the
  cold-start spike).
- **Blur/glow *pipelines*** — `BlurChain`/`Glow` already separate pipeline
  construction (in `::new`) from the size-dependent textures (rebuilt in
  `resize`/`build_resources`). Split them: a shared `BlurPipelines`/
  `GlowPipelines` in `AppShared`, per-window textures + bind groups in
  `WindowState`. The seam already exists, so this is small — and it reclaims the
  ~30ms of per-window shader compilation that an opaque "blur/glow stays
  per-window" would otherwise burn (~20% of the <150ms new-window budget). Do
  it here, not "later" — leaving it out makes the plan internally inconsistent
  with the reason we share pipelines at all.

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
- **Blur/glow *textures*** — `scene` / `scene_fg` / bright / scratch targets and
  their bind groups, sized to the window. The *pipelines* live in `AppShared`
  (above); only these size-dependent resources are per-window, rebuilt on resize
  exactly as `BlurChain::resize` already does.
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
- **Images**: `image_store` (`images::Store`) and `pending_placements` — both
  per-tab (each shell has its own placements + scrollback). **If** the decode
  worker / GPU texture cache is ever shared across tabs for memory, its
  mark-and-sweep eviction (run per `render()` against the live+scrollback
  placement set) must key on the **union of all tabs'** placement sets, or one
  tab's render will evict another tab's images. Per-tab `Store` avoids this
  entirely for now; note the constraint so a later shared cache doesn't trip it.

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

**Background-tab cost:** a backgrounded tab still runs the full
`feed_terminal` parse on the main thread per `PtyInput` event (it just skips the
redraw), so many chatty background tabs serialize parse work on the event loop.
Acceptable for now; revisit if it bites once tabs ship.

### Reader-thread lifecycle (decide in Stage 3, not Stage 4)

This is the most easily-missed re-architecture trap. Today `Pty::run` is an
unbounded `loop { read(master, …) }` that blocks in the kernel until the child
EOFs, and `run()` **moves the whole `Pty` into the detached reader thread** — so
the main thread keeps no handle to the fd or child. That's fine when the thread
dies with the process, but for per-tab close it's broken:

- Dropping `TabState` does **not** stop the reader thread — it's parked in
  `read(master)` and only unblocks when the *child* exits. Closing a tab running
  `vim`/`tail -f`/`tmux` leaks the thread (and its `EventLoopProxy` clone) until
  that process dies, and it will still fire `PtyExit(TabId)` for a freed id.
- To actually stop it you must `close(master)` (forces the blocking `read` to
  return) **and** `kill(child, SIGHUP)` so the child doesn't orphan.

So the ownership model must change **when `create_tab` is built (Stage 3)**, not
patched at close time (Stage 4):

- `TabState` retains `master: i32` and `child: pid_t`; the reader thread gets the
  fd by value (single consumer) but the main thread keeps its own copy to signal.
- Tab close = `close(master)` + `kill(child, SIGHUP)`; thread then `waitpid`s and
  exits.
- The `TabId` resolver must treat an **unknown/freed `TabId` as a no-op**, not an
  `unwrap` — a just-closed tab's thread can still deliver one last event.

## Staged delivery

Each stage builds, passes `cargo test`, and (except the last two) is a
behavior-preserving refactor — so regressions are caught early and the risky
extraction is decoupled from the user-visible feature.

### Pre-flight — prove mid-loop window creation works (≈1 hour)
Before any extraction, throwaway-prototype the one assumption the whole plan
rests on: that a second `NSWindow` can be built **from inside the event
callback** via the `EventLoopWindowTarget` (winit 0.29). Wire Cmd-N to
`WindowBuilder::new()…build(elwt)` opening a bare empty window and discard it.
Our cocoa-extension builder calls (`with_titlebar_transparent`,
`with_fullsize_content_view`, `with_blur`) are exactly the kind Cocoa
occasionally no-ops or deadlocks on when invoked mid-loop. If it balks, the plan
changes shape — find out in an hour, not at Stage 4. Revert the prototype after.

### Stage 0 — Split `GpuContext`; extract `AppShared` (no behavior change)
- Split `gpu::GpuContext` into `gpu::Gpu { device, queue }` (shared) and a
  per-window `WindowSurface { surface, config, size }`.
- Introduce `AppShared` holding `Rc<Gpu>`, `Rc<RefCell<Font>>`,
  `Rc<RefCell<Shaper>>`, shared pipelines + layouts.
- `State` keeps its current single-window behavior but reaches GPU/font/pipeline
  through `AppShared`. Mechanical but wide: touches every `self.gpu.device`,
  `self.font`, `self.render_pipeline`, `self.shaper`, `self.atlas` site.
- Includes the `with_font(|face| …)` accessor (Risk 1) and the blur/glow
  pipeline split (shared pipelines, per-window textures) — both belong here, not
  later.
- **Highest-risk, highest-value stage.** Ships nothing new; fully testable
  against current behavior. Do this first and verify with the app + tests.
  Note: `cargo test` exercises `Font` directly, **not** the `RefCell` wrapper —
  so the borrow-overlap risk is verified by the manual run, not the suite.
  Budget for it.

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
- Lifecycle: `CloseRequested` removes the window (and frees its tabs' `TabId`s,
  `close(master)` + `kill(child, SIGHUP)` each) from the maps; quit when no
  windows remain. `PtyExit` closes just that tab (and the window if it was the
  last tab) per `shell_exit_mode`.
- **Fix `surface.get_current_texture().unwrap()`** (today's render path) to
  reconfigure-and-skip-frame on `Outdated`/`Lost`. With one window this only
  trips on resize; with N windows a backgrounded/occluded/closing surface
  returns `Outdated` routinely and the `unwrap` panics — a latent crash that is
  *invisible* to every behavior-preserving stage and surfaces exactly when the
  feature ships.

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
*(Ordered by how badly a late discovery hurts. 1–3 are the ones that could force
re-architecture; they're now addressed structurally in the stages above.)*

1. **`RefCell<Font>` borrow overlap → runtime panic.** `self.font` is read at
   ~20 sites across `render`/`update_vertices` and mutated (`ensure_char`,
   `&mut Font`) in the middle of the same hot method. A `Ref` held one line too
   long panics — and `cargo test` won't catch it (tests bypass the `RefCell`).
   **Mitigation:** the `with_font(|face| …)` accessor + single-statement
   `ensure_*` rule make the borrow scope un-extendable by construction
   (Stage 0 deliverable), not a per-site audit.
2. **PTY reader-thread can't be stopped by dropping the Rust side.** The
   blocking `read(master)` only returns on child EOF; today the thread owns the
   whole `Pty`. **Mitigation:** `TabState` keeps `master`+`child`; close does
   `close(master)` + `kill(child, SIGHUP)`; `TabId` resolver tolerates misses.
   Ownership decided in Stage 3, not patched in Stage 4. (See routing section.)
3. **`get_current_texture().unwrap()` panics with multiple windows.** Invisible
   to every behavior-preserving stage; fix to reconfigure-and-skip in Stage 4.
4. **Two wide back-to-back refactors** (Stage 0 AppShared, Stage 1 Window/Tab
   split) over the same ~5,000 lines. Keep them separate and individually
   verified so a regression bisects to one. Move methods, don't rewrite them.
5. **Per-window resize/scale changing the shared format** — assert format
   stability; if a window lands on a different-format surface, fall back to its
   own pipeline set (unlikely; document the assumption). `ScaleFactorChanged`
   must rebuild only *that* window's atlas (drag between monitors of differing
   DPI).
6. **Already-fine, don't "fix":** `arboard` clipboard is constructed per-op
   (process-safe as-is). **Watch for:** IME preedit, if added, is focus-scoped →
   `WindowState`. **wgpu 0.18 `Device` not `Clone`** — irrelevant, shared via
   `Rc`.

## Rough effort
- Pre-flight (mid-loop window prototype): ~1 hour.
- Stage 0 (AppShared + `with_font` accessor + blur/glow pipeline split): ~1–1.5 days.
- Stage 1 (WindowState/TabState split): ~1 day (second wide refactor).
- Stage 2 (registry + TabId routing): ~half day.
- Stage 3 (factories + PTY close/ownership): ~half–1 day.
- Stage 4 (wire Cmd-N + surface-acquire fix): ~half day.
- Stage 5 (cleanup + tests): ~half day.

Total ≈ 3.5–4.5 focused days for the multi-window feature with the tab-ready
model. The tab UX follow-up is additional. The pre-flight and Stages 0–1 are the
gates; if they land clean, the rest is straightforward.
