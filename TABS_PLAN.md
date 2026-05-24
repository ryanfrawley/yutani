# Tabs — implementation plan (follow-up to in-process multi-window)

You're picking up a planned feature with the groundwork already done. **Read
this whole file first**, then skim `MULTIWINDOW_PLAN.md` (same directory) — its
"Three-tier model", "Tab tear-off", and "PTY event routing" sections are the
authoritative design this plan builds on. This document is self-contained
enough to implement from; the references are for deeper context.

## TL;DR

Yutani now opens multiple **windows** in one process (Cmd-N, ~10ms/window —
see the multi-window PR). The data model was deliberately built tab-ready: a
window already owns `tabs: Vec<TabState>` + `active: usize`, PTY events already
route by a process-unique `TabId`, and there are `create_tab` / `create_window`
factories. **What's missing is everything that lets more than one tab exist in
a window**: a correctness fix so PTY output reaches the *right* tab (not just
the active one), the new/close/switch plumbing, the tab-bar UI, and (last) tab
tear-off between windows.

This is **additive** — no rewrite of the existing structs. If you find yourself
wanting to change `WindowState`/`TabState`/`TabId`, stop and re-read; the model
was designed so you shouldn't have to.

## Where to work / how to build

- **Branch:** continue on `tabs` (this branch, which carries the multi-window
  work + this plan), or branch fresh off `multi-window` (or off `main` once the
  multi-window PR #117 has merged). Per the repo's `CLAUDE.md`, do all edits in
  a **git worktree**, never the primary checkout `/Users/ry/projects/terminal`.
- **Build/test:** `cargo build`, `cargo test` (no lib target; tests are inline
  `#[cfg(test)] mod tests`). Baseline is green at **1124 tests**.
- **Run:** `./target/debug/yutani` — it's a macOS GUI; launch it backgrounded
  and have the user drive it (you can't send keystrokes headlessly). Set
  `YUTANI_STARTUP_TIMING=1` for phase timings, and run with
  `RUST_LOG=wgpu_core=warn` to surface wgpu validation errors.
- **The borrow-overlap trap is still live** (see `AppShared::with_font` and the
  `shaper` borrow in `update_vertices`): font reads go through
  `with_font(|face| …)`, atlas fills are single-statement
  `ensure_*(&mut *shared.font.borrow_mut(), …)`. `cargo test` does **not**
  exercise the `RefCell`, so verify any hot-path change with a real run.
- **PRs:** self-hosted Forgejo at `git.frawley.co` — see `CLAUDE.md` for the
  exact `git push` (HTTPS + keychain cert) and `curl` API recipe. After
  significant changes, run the `unit-test-writer` agent (per `CLAUDE.md`),
  though note most of this feature is GPU/windowing/PTY side-effect code with
  little pure logic to unit-test.

## The model as it exists today (post multi-window, Stages 0–5)

All in `src/main.rs` unless noted.

- **`AppShared`** (held as `Rc<AppShared>`, one per process): GPU device/queue
  (`gpu: Rc<gpu::Gpu>`, `gpu.rs`), `font: Rc<RefCell<font::Font>>`,
  `shaper: Rc<RefCell<shaper::Shaper>>`, the render/wireframe pipelines, the
  bind-group layouts (font/camera/fade), and `blur_pipelines` / `glow_pipelines`
  (`renderer/blur.rs`, `renderer/glow.rs`). Built once by `AppShared::new`.
  `with_font(|&Font| …)` accessor.
- **`WindowState`** (one per OS window; `HashMap<WindowId, WindowState>` in
  `run()`): owns the `surface` (`gpu::WindowSurface`), `window`,
  `shared: Rc<AppShared>`, the per-window **atlas** + font texture/bind-group,
  camera + fade uniforms, vertex/index/strip **buffers**, the blur/glow
  **textures** + masks, `image_pipeline`, `pt_size`/`dpi`, `config`, the
  focus-scoped input (`modifiers`, `mouse_x/y`, `held_button`, click/blink
  state, `command_palette`, `search`, `chrome_band_px`, `over_toolbar`,
  `last_toolbar_click`), `manual_title`, `theme`, `pending_new_window`, `perf`,
  `vertices_dirty`, and **`tabs: Vec<TabState>` + `active: usize`**. Reaches the
  active tab via `active_tab()` / `active_tab_mut()` (and direct
  `self.tabs[self.active]` indexing at the ~16 sites that need a borrow disjoint
  from another field of `self` — the borrow checker can split field paths but
  not a `&mut self` accessor method).
- **`TabState`** (one per shell/PTY): `tab_id: app_window::TabId`, `master: i32`
  (PTY fd), `child: i32` (forked pid), `terminal: terminal::Terminal`,
  `image_store` + `pending_placements`, the scroll group
  (`scroll_y`/`alt_scroll_anim`/`wheel_pty_accum`/`scroll_suppressed`/
  `last_wheel_at`/`last_reported_cell`), the cursor-anim transients
  (`cursor_anim`/`prev_visible`/`cursor_ghosts`), the completion group
  (`completions`/`completions_input`/`selected_completion`/`completion_scroll`/
  `completion_dismissed`) + `command_history`, the selection group
  (`selection`/`selection_mode`/`press_cell`/`press_pixel`/`last_click`/
  `click_count`), and `hover_url`. **Holds zero window/GPU-coupled state** (no
  atlas/surface/buffers/`pt_size`/`dpi`) — this is the invariant that makes
  tear-off a pure data move; **do not violate it** (per-tab zoom is out of
  scope precisely because it would).
- **`TabId(u64)`** (`src/app_window.rs`): minted by `next_tab_id()` (monotonic
  atomic, never reused). `CustomEvent::PtyInput(TabId, String)` /
  `PtyExit(TabId, i32)`. The reader thread spawned in `create_tab` tags every
  send with its `TabId`.
- **Routing** (event loop in `run()`): `HashMap<WindowId, WindowState>` +
  `HashMap<TabId, WindowId>`. `UserEvent`s (PTY) resolve `TabId → WindowId →
  WindowState`; `WindowEvent`s resolve by `WindowId`. Unknown/freed `TabId` is a
  no-op (a just-closed tab's thread can deliver one last event).
- **Factories:**
  - `create_tab(proxy, program, zdotdir, cwd, cols, rows, image_cap) ->
    (TabId, TabState)` — opens a PTY, forks `program`, spawns the reader thread
    tagging the new `TabId`, builds the `TabState` sized to `cols`×`rows`. The
    thread takes the `Pty` by value; `TabState` keeps `master`+`child` copies.
  - `WindowState::create_window(shared, window, surface, config, dpi,
    initial_tab) -> WindowState` — builds per-window GPU resources and **adopts
    `initial_tab` by move** (so the same path serves tear-off-to-new-window).
  - `spawn_window_in_process(...)` — Cmd-N: build NSWindow via the event loop +
    sibling surface (`Gpu::create_surface`) + a fresh tab, register in the maps.
  - `close_tab_pty(tab)` — `kill(child, SIGHUP)` + `close(master)` so the reader
    thread unblocks and reaps. Use this for **every** tab teardown.

## ⚠️ The one correctness bug you MUST fix before any UI: PTY routing feeds the *active* tab

This is the single most important thing in this document.

Today the `PtyInput` arm in `run()` does
`tab_to_window.get(&ev_tab) → windows.get_mut(wid) → state.feed_terminal(&z)`,
and `feed_terminal` (and `write_pty`, the OSC title/cwd/histfile/submitted
polls, `recompute_completions`, `update_hover_url`) all operate on
**`active_tab()`**. With one tab per window that's correct because the active
tab *is* the event's tab. **With multiple tabs it is wrong**: shell output for a
background tab would be fed into the foreground tab's terminal.

So Tabs Stage A (below) must make the PTY path operate on the tab identified by
the event's `TabId`, not `active_tab()`. Concretely:

- Add `WindowState::tab_index_by_id(&self, id: TabId) -> Option<usize>` (linear
  scan of `self.tabs`; tab counts are tiny).
- Make the per-PTY handlers take a tab index (or a `&mut TabState`), e.g.
  `feed_terminal_for(&mut self, tab_idx, bytes)`, rather than implicitly using
  `active`. The same applies to the response/title/cwd/histfile/submitted/
  completions/hover follow-ups in the `PtyInput` arm — they must run against the
  resolved tab. A background tab still runs the full `feed_terminal` parse (just
  no redraw) — only call `invalidate()` / redraw when the fed tab **is** the
  active one (or it signals a bell/title change worth surfacing).
- The window title and the OSC-0 **`manual_title` should move from
  `WindowState` to `TabState`** (each shell sets its own title). The OS window
  title then derives from the **active** tab's `manual_title` + cwd; the tab bar
  shows each tab's own title. (Search/command-palette stay on `WindowState` —
  they're focus-scoped overlays, not per-shell.)

Get this right with two tabs (one running `tail -f` in the background) before
building the tab bar, or you'll chase rendering ghosts that are really routing
bugs.

## Architecture for tabs (what to add — additive only)

- **A window renders only its active tab.** `update_vertices` already reads
  `self.tabs[self.active].terminal`; switching tabs is "repoint `active`,
  invalidate, rebuild vertices". The per-window atlas is shared across the
  window's tabs, so glyphs one tab rasterized are free for the others.
- **Background tabs reflow lazily.** On a window resize, only the active tab's
  grid reflows (that's already what `resize()` does — it touches
  `active_tab().terminal`). When you switch to a tab that was backgrounded
  during a resize, the activation path **must** call `notify_pty_size(cols,
  rows)` + reflow that tab's terminal to the window's current grid size, else it
  renders at a stale size. Make "activate tab N" a single method that: sets
  `active = N`, reflows the now-active tab to the window's cols/rows, resets the
  cursor-anim snap (so it doesn't slide from a stale position), `invalidate()`s.
- **Tab bar is window chrome.** It's a per-window UI strip (in or just below the
  title-bar band — see `chrome_band_px` / `DECORATOR_HEIGHT` /
  `refresh_chrome_band` and the existing toolbar hit-testing: `over_toolbar`,
  `in_top_toolbar`, `last_toolbar_click`). Drawn by the renderer; hit-tested in
  `input()`'s mouse handling. It is **not** `TabState` — it's derived from the
  window's `tabs`/`active` each frame.
- **Tear-off is a `TabState` move** (final stage): `let tab =
  window_a.tabs.remove(i); window_b.tabs.push(tab); tab_to_window.insert(tab_id,
  b_id); window_b.activate_last(); reflow;`. Works only because `TabState` holds
  no window/GPU state and the GPU device lives in `AppShared` (a tab's
  `image_store` GPU textures stay valid across the move). A tab can be
  transiently window-less mid-drag — see the resolver note in Risks.

## Staged delivery

Each stage builds, passes `cargo test`, and should be verified with a real run.
Land them as separate commits so a regression bisects. Stages A–B are the
functional core; C is the bulk of the UI work; D–E are polish/advanced.

### Stage A — Route PTY to the correct tab (no UI; prerequisite correctness)
- `tab_index_by_id`; make the `PtyInput`/`PtyExit` arms and the per-shell
  handlers operate on the resolved tab, not `active_tab()` (see the bug section
  above). Move `manual_title` to `TabState`; derive the window title from the
  active tab.
- **Verify** with **two** tabs even though there's no UI yet: temporarily wire a
  debug keybind to `create_tab` + push, run a background producer in tab 0,
  switch to tab 1, confirm tab 0's output lands in tab 0 (not 1). Remove the
  debug keybind before committing, or fold it into Stage B.

### Stage B — New / close / switch tab (keyboard-driven, still no bar)
- **New tab (Cmd-T):** `create_tab(proxy, ChildProgram::Shell, zdotdir, cwd,
  cols, rows, image_cap)` using the window's current cols/rows and the active
  tab's cwd; `self.tabs.push(tab)`; `tab_to_window.insert(tab_id, window_id)`;
  activate it. The event loop owns the maps, so (like Cmd-N's
  `pending_new_window`) signal the request out of `input()` and fulfil it in the
  loop — add a `pending_new_tab: bool` (or richer pending-action enum) on
  `WindowState`, drained right where `pending_new_window` is drained.
- **Close tab (Cmd-W):** if the window has >1 tab, `close_tab_pty(removed)`,
  remove it from `self.tabs` + `tab_to_window`, fix up `active`, reflow+activate
  the new active tab. If it's the **last** tab, fall back to today's behavior
  (close the window; quit if it was the last window). NB: plain Cmd-W is
  currently *not* intercepted — it reaches the OS and comes back as
  `CloseRequested`, which closes the whole window. You'll now want to intercept
  Cmd-W in `input()` to close the active **tab** instead, and keep
  `CloseRequested` (red button) closing the whole window.
- **Switch tab:** next/prev (e.g. Cmd-Shift-`[` / `]` or Ctrl-Tab) and Cmd-1..9
  to jump. All go through the single `activate_tab(n)` method (reflow + cursor
  snap reset + invalidate).
- A `PtyExit` for a non-last tab should close just that tab (per
  `shell_exit_mode`), not the window.

### Stage C — Tab bar chrome (the UI)
- Render a tab strip per window: one cell per tab showing its title (truncated),
  the active tab highlighted, a close affordance, and a `+` new-tab button.
  Reuse the renderer's quad/text emit paths used for the toolbar/overlays.
- Hit-testing in `input()`: click a tab → `activate_tab`; click its close → close
  that tab; click `+` → new tab. Fold into the existing toolbar pointer handling
  (`over_toolbar` / chrome band) so grid I-beam vs arrow cursor stays correct.
- Decide the band layout: either grow `chrome_band_px` to include the tab strip,
  or draw the strip below the native title bar. Keep the grid's top offset in
  sync (the grid must not sit under the tab strip). **Show the bar only when
  there's >1 tab** (or always, per taste — confirm with the user).
- Update the title-bar drag/zoom regions so they don't swallow tab clicks.

### Stage D — Tab reorder (drag within the bar)
- Press-drag a tab horizontally to reorder within `self.tabs` (keep `active`
  pointing at the same `TabState`). Pure `Vec` reorder + redraw; no PTY changes.

### Stage E — Tab tear-off between windows (advanced; see MULTIWINDOW_PLAN "Tab tear-off")
- Drag a tab out of the bar; on drop, either move its `TabState` into another
  window's `tabs` or `create_window(shared, elwt, dragged_tab)` to spawn a new
  window around it. Update `tab_to_window`; reflow the tab to the destination
  size/DPI.
- The `TabId` resolver must tolerate a **window-less** tab mid-drag: hold it in
  an `app.dragging: Option<(TabId, TabState)>` that the loop also drains for PTY
  events (a detached tab still consumes shell output), and treat unknown ids as
  no-ops (already true).

## Risks / traps (most are already handled by the model — keep them handled)

1. **PTY-to-active-tab mis-routing** (Stage A) — the one real bug; fix before UI.
2. **Lazy background reflow** — the activate path *must* `notify_pty_size` +
   reflow the now-active tab, or it renders at the size it had when last active.
3. **Borrow-overlap panic** — unchanged from multi-window: `with_font` for
   reads, single-statement `borrow_mut` for atlas fills; not caught by tests.
4. **`image_store` eviction** — it's **per-tab** today (mark-and-sweep keys on
   one tab's live+scrollback placement set). Keep it per-tab. *If* you ever
   share a GPU texture cache across tabs for memory, the sweep must key on the
   **union** of all tabs' placement sets or one tab's render evicts another's
   images.
5. **Reader-thread lifecycle** — always tear a tab down with `close_tab_pty`
   (`kill(child, SIGHUP)` + `close(master)`); dropping `TabState` alone leaks the
   thread parked in `read(master)`. The final `PtyExit` for a just-closed tab
   hits an unmapped `TabId` → no-op (already handled).
6. **Tear-off invariants** (Stage E): `TabState` holds zero window/GPU state; the
   device lives in `AppShared`; routing is by `TabId`. Don't break these.
7. **`surface.get_current_texture()`** is already reconfigure-and-skip on
   `Outdated`/`Lost` — leave it.

## Out of scope (don't expand)
- **Per-tab zoom / font size** — would couple a tab to atlas metrics and break
  the shared-per-window atlas (and tear-off). Zoom stays per-window.
- **Per-tab / per-window color scheme** — the palette is process-global
  (`palette::install`); de-globalizing it is a separate effort.
- **Session restore / tab persistence across launches.**

## First move
Start with **Stage A** — it's invisible but it's the correctness foundation;
everything else renders garbage without it. Verify with two tabs and a chatty
background shell before touching the renderer.
