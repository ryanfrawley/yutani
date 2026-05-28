# Dirty-row rendering plan (Step 2 of the render-perf work)

Implementation plan for a fresh agent/session. Branch: `dirty-rows` (off `main`).
Worktree: `../yutani-dirty-rows`.

## Why

Profiling (`PERFLOG=1 ./target/release/yutani`, see the burst log's
`shape`/`body` split) established that the dominant per-frame cost is the
**per-cell `body`** of `update_vertices` (`src/state_render.rs`): emitting bg +
fg quads for **every visible cell, every frame** — ~13–29ms per rebuild,
~1–2µs/cell. The vsync wait (`swait` ~0.02ms), GPU encode (`render` ~0.15ms),
and the ligature `shape` pass (~0.1–0.3ms) are all negligible. This per-rebuild
cost is what makes typing-while-output-streams and multi-window typing laggy
(N windows each pay it, serialized on the single event-loop thread).

Prior steps already landed (all off `main`):
- **PR #128** `window-focus-gating` — unfocused/occluded windows stop
  blinking/animating, so idle background windows don't pay the rebuild.
- **PR #129** `font-metric-cache` — `cell_width`/`metrics`/underline cached on
  `Font`; per-frame path no longer touches the FreeType face.
- **PR #130** `scroll-via-uniform` — global `scroll_y` + decorator easing folded
  into the camera view-proj; scroll-on-output ease frames skip the rebuild via
  `refresh_scroll_uniforms`. **Crucially, this made a row's emitted geometry
  scroll-independent at rest** (no `scroll_y`/`decorator` baked into vertices in
  the global case), which is the prerequisite that makes per-row caching valid:
  a cached row segment stays correct as the view scrolls.

Step 2 attacks the rebuild cost itself: **only re-emit rows whose rendered
content changed.** Target: a keystroke / output line drops from ~15ms to <1ms.

## Decision (from the user): exact dirty-on-write damage

Track damage in the terminal **at write time**, precisely — and only mark a row
dirty when the write **actually changes the rendered cell**. (Not the
snapshot-compare alternative.) The renderer consumes the damage set and re-emits
only those rows, reusing cached vertex segments for the rest.

## Part A — damage tracking in the terminal (`src/terminal.rs`)

`Grid` is at `src/terminal.rs:269`; cells are `style::Cell` (Copy). Live grid
rows are written through the grid; scrollback rows (`Vec<Vec<Cell>>`) are static
once pushed.

1. Add per-row damage to the live grid, e.g. `dirty_rows: Vec<bool>` sized
   `rows` (or a `Vec<u32>` generation counter per row if you want cheap
   "changed since gen G" queries; bool is enough for one consumer).
2. **Gate on actual change.** Every cell write compares first:
   ```rust
   if self.cells[idx] != new_cell {
       self.cells[idx] = new_cell;
       self.dirty_rows[row] = true;
   }
   ```
   `Cell` already derives `PartialEq` (verify) — re-writing the same glyph+style
   must NOT dirty the row (this is the user's explicit requirement: "make sure
   the write actually needs a redraw"). This also kills churn from apps that
   repaint identical frames.
3. Mark rows dirty in the other mutators:
   - EL/ED line + screen clears → affected rows.
   - Region scroll (SU/SD, IL/DL, LF at bottom): content shifts between rows →
     mark every row in the scrolled region dirty. (A later optimization could
     track a scroll delta and shift cached segments instead, but start simple.)
   - Cursor move alone does **not** dirty (cursor is a renderer overlay, see
     Part B) — but a cell write under the old/new cursor that changes content
     does, naturally.
   - `resize`, `reset`, alt-screen enter/leave → mark all rows dirty (or signal
     a full invalidate; the viewport_key change in Part B already forces this).
4. Expose to the renderer:
   - `row_damage(&self) -> &[bool]` (or an iterator of dirty live-grid rows).
   - `clear_row_damage(&mut self)` — the renderer calls it after building. (Or a
     `take_*` that returns + clears; but the renderer needs the set during a
     borrow that also reads cells — return a small `Vec`/bitset copy, or clear
     in a separate pass.)
5. **Buffer-row vs visual-row mapping.** Damage is in live-grid row terms. The
   renderer iterates *visual* rows over the phantom band `r_lo..r_hi`
   (`Self::phantom_row_band`). Map: a visual row `r` → live-grid row via the
   existing `extended_cell`/`visual_to_abs_line` math. Scrollback rows are
   static (never written), so they're dirty only on first emit / viewport
   change. Live-grid rows map through `view_offset`. Get this mapping right or
   you'll re-emit the wrong rows.

## Part B — per-row vertex cache in the renderer (`src/state_render.rs`)

`update_vertices` currently builds one `vertices: Vec<Vertex>` / `indices` for
the whole frame: bg cell quads for all rows, then fg glyph quads for all rows
(boundary recorded in `num_bg_indices` for the two-pass glow), then overlays.

1. **Factor per-row emission.** Extract the bg-quad and fg-quad emission for a
   single row into a function that appends into per-row buffers, e.g.
   `emit_row(r) -> RowVerts { bg: Vec<Vertex>, fg: Vec<Vertex> }`. This runs the
   existing resolve (`extended_cell`, `project_cell`, reverse/selection color),
   the ligature override lookup, and `push_quad` — but for one row.
2. **Cache.** Store on `TabState` (per-shell, so tabs don't share):
   `row_cache: HashMap<isize, RowVerts>` (keyed by visual row) plus the
   `viewport_key` it was built against and a generation/epoch.
3. **Per-frame build:**
   - Compute `r_lo..r_hi` as today.
   - If `viewport_key` changed (resize / scrollback scroll / alt-screen toggle),
     **clear the whole cache** (visual→buffer mapping moved).
   - For each visual row `r` in the band: re-emit (call `emit_row`) iff the row
     is damaged (Part A) OR not in the cache. Otherwise reuse the cached
     `RowVerts`.
   - **Assemble:** concat every row's `bg` into the bg region, then every row's
     `fg` into the fg region; regenerate `indices` during assembly (6 per quad,
     sequential — don't cache indices, they're trivial and position-dependent).
     Set `num_bg_indices` at the boundary. Append the dynamic overlay section
     (below). Upload.
   - The shaping pass (`ensure_char`/`ensure_glyph_id`, which mutates the shared
     atlas) must still run for damaged rows before emit; it's cheap and only the
     dirty rows trigger new rasterization.
4. **Keep these OUT of the per-row cache — emit fresh each frame in a small
   "dynamic" section** (they're cheap and change on their own cadence, so
   caching them would cause needless row invalidations):
   - cursor quad + cursor ghosts (so blink/ease never dirties a text row),
   - command palette, find overlay, completion popup (already cursor-anchored).
   - Edge-fade strips + fade uniform already live in `refresh_scroll_uniforms`
     (PR #130) — leave them there.
5. **Things that DO belong to a row and must invalidate it explicitly** (they
   recolor / underline cells, which are in the cached cell quads):
   - **Selection**: a selection change recolors cells (`selection_fg`) and the
     bg. On selection range change, invalidate the rows overlapping the old ∪
     new range (renderer-side; selection lives in the renderer, not the
     terminal). Track `prev_selection_range` and diff.
   - **URL hover underline**: same — invalidate rows overlapping old ∪ new hover
     span on hover change.
   - **Theme/palette swap** (scheme change) → clear whole cache (colors change).
   - **Font size / DPI change** → clear whole cache (geometry changes); also
     changes `viewport_key`, so the existing clear covers it.

## Where state lives

- `Grid.dirty_rows` + mutator marking + `row_damage`/`clear_row_damage` →
  `src/terminal.rs`.
- `RowVerts` type, `row_cache`, `prev_selection_range`, `prev_hover_span`,
  cache epoch → `TabState` in `src/main.rs` (per-shell).
- `emit_row`, cache assembly, dynamic-overlay section, invalidation wiring →
  `src/state_render.rs` (`update_vertices`).

## Validation

- `cargo test` (1150 baseline) stays green. Add terminal unit tests:
  - writing the identical cell does NOT set `dirty_rows[r]`;
  - writing a changed cell does;
  - EL/ED/scroll/resize mark the expected rows.
- Behavioral (release build, manual): type (only the cursor's text row
  re-emits), stream output (only new bottom rows), drag-select (selected rows
  re-emit, others reused), scroll into scrollback (cache clears on viewport
  change, renders correctly), resize, theme swap, ligatures (Fira Code),
  images, full-screen apps (vim/htop/less), alt-screen ↔ primary toggles.
- `PERFLOG=1`: for localized edits, `body` should drop from ~15ms to <1ms; the
  `update` call count stays high but each call is cheap.

## Risk & guard

Highest risk: a **missed damage source** leaves a stale row on screen (visible
corruption). Mitigations:
- The change-gated write (`if cell != new`) is the single natural choke point —
  route all cell mutation through it.
- Add a debug env (e.g. `YUTANI_DIRTY_AUDIT=1`) that re-emits every row anyway
  and asserts the cached segment equals the fresh one, to catch divergence
  during development.
- When in doubt for a mutator, mark the whole live grid dirty (correct, just
  less optimal) and refine later.

## Sequencing

1. Part A damage map + change-gated writes + tests (no renderer change yet;
   verify damage is correct via a temporary log).
2. `emit_row` extraction (behavior-preserving: still emit all rows, just via the
   new per-row function — confirm identical output).
3. Add the cache + reuse for unchanged rows + assembly.
4. Wire selection/hover/theme/viewport invalidations.
5. Move cursor/overlays to the dynamic section if not already separable.
6. Measure with PERFLOG; add the audit env guard.

Relates to: PR #128/#129/#130, and the multi-window architecture memory.
