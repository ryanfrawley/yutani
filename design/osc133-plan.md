# OSC 133 Semantic Prompt Marks — Implementation Plan

## Status

All planned slices are implemented (PRs #76–#80):

- **K1 + K2 + K4** (#76) — parse A/B/C/D marks, store live, `command_regions`.
- **K3** (#77) — marks survive scroll into scrollback, eviction, and resize.
- **K5** (#78) — prompt navigation (Cmd-Shift-Up/Down).
- **K6** (#79) — exit-status gutter indicator.
- **K7** (#80) — select-last-command-output (Cmd-Shift-O).

Deferred items below (autocomplete overlay, 32-bit params, status-filtered
navigation) remain unbuilt — pick up if requested.

## Overview

Add support for the FinalTerm / shell-integration **OSC 133** protocol
(`A`/`B`/`C`/`D` marks) to yutani's terminal core. Marks annotate grid
positions with semantic roles (prompt start, command-input start,
command-output start, command end + exit code), letting the app reason about
*command regions*: where each prompt began, where the user's typed command
lives, where its output is, and whether it succeeded. Delivered as small,
independently-testable K-style slices, building from raw parsing up through a
queryable command-region API and the UI features it unlocks (prompt
navigation, exit-status gutter indicators, select-last-command-output,
autocomplete-overlay foundation).

The load-bearing slices (K1–K4) are terminal-core-only; renderer/UI wiring
(K5+) is sketched but deliberately deferred behind the data model, matching
how the Kitty graphics work landed (parse + state first, render later).

## Architecture findings

References are `file:line` against `main` at planning time.

- **OSC dispatch** lives in `Terminal::handle_osc` (`src/terminal.rs:2033-2059`).
  It splits on the first `;` into a numeric `Pn` head and `rest`, parses `Pn`,
  and matches. Adding a `133 => self.handle_osc_133(rest)` arm mirrors the
  existing `1337 => self.handle_osc_1337(rest)` arm and the `7 =>` arm added
  for OSC 7. Unknown codes fall through `_ => {}`, so doing nothing is the
  safe default.
- **The parser** (`src/ansi.rs`) emits `Event::Osc(s)` with the payload
  string; OSC 133 needs no parser changes — it arrives through the same path
  as 1337/7.
- **No local edit buffer** (`src/input.rs:1-5`): keystrokes go straight to the
  PTY. The *shell's* integration script emits the marks; yutani is a passive
  consumer. We never synthesize marks ourselves, and the `B`→`C` "input
  region" is whatever the shell delimits, not something we derive from keys.
- **Grid model**: `Grid` is a flat `Vec<Cell>` of `rows*cols`, with `primary`
  and `alternate` grids on `Terminal` selected by `use_alternate`. `Cell` is
  `Copy`. Marks must **not** live in `Cell` — a mark is a row-level (sometimes
  row+col) event; storing it per-cell would bloat the hot grid `Vec` and
  complicate scroll/clear.
- **The stable-line-index scheme is the key enabler.** `visual_to_abs_line`
  maps a visual row to an absolute line index where `0..scrollback_len` indexes
  scrollback (oldest first) and `scrollback_len + r` indexes live grid row `r`;
  `line_at(abs_line)` reads any absolute line. This absolute index is **not
  stable** — it shifts as scrollback grows/evicts. The codebase's answer for
  placements is the `ScrollbackPlacement` pattern.
- **`ScrollbackPlacement` is the template to copy.** `struct
  ScrollbackPlacement { scrollback_row: isize, placement }` anchors an image to
  a scrollback row index, and:
  - **On scroll into scrollback**, `scroll_region_up_by` pushes the live row
    into `scrollback` and promotes placements that scrolled off the top into
    `scrollback_placements` with `scrollback_row = sb_len + p.top_row`.
  - **On scrollback eviction** (ring full), `evict_scrollback_placement_front`
    drops anchors at row 0 and decrements the rest in lockstep with
    `scrollback.pop_front()`.
  - **On resize**, the spill loop pushes top rows into scrollback and migrates
    placements the same way.
  - **`view_offset` bumping** keeps the historical viewport stable as new rows
    stream in.
  Semantic marks follow this exact lifecycle: a mark on a live grid row is
  stored grid-relative; when that row spills into scrollback it converts to a
  scrollback-anchored index, decrements on eviction, and is dropped when its
  anchor row is evicted.
- **Screen switching**: `switch_screen` flips `use_alternate`. The alt screen
  has no scrollback. Marks are a primary-screen concept (shells run on the
  primary; vim/less grab the alt screen and don't emit prompt marks). Store
  marks only for the primary screen; ignore OSC 133 while `use_alternate`.
- **Clears / reset**: `Grid::clear` drops placements; `full_reset` clears
  scrollback and `scrollback_placements`. Marks must be cleared in the same
  places.
- **Existing test style**: inline `#[cfg(test)]` tests feed byte strings to a
  `Terminal` and assert on state — see `osc_1337_*` and the resize/scrollback
  suite. OSC 133 tests follow this exactly (build `\x1b]133;A\x07`, `feed`,
  assert on a new query API).

## Data model

Two coordinate domains, mirroring `ScrollbackPlacement`:

- **Live marks** — anchored to a primary-grid row (`usize`, `0..rows`). Move
  when the grid scrolls within itself.
- **Scrollback marks** — anchored to a `scrollback_row: isize`, decremented on
  eviction, dropped at row 0 (same rule as `evict_scrollback_placement_front`).

A mark records its kind and, for the prompt/input/output marks, optionally the
cursor column when emitted; for command-end it records the exit code.

```rust
enum SemanticMarkKind {
    PromptStart,
    InputStart,
    OutputStart,
    CommandEnd { exit: Option<i32> },
}
```

The internal store keeps a flat, ordered list of mark events `(anchor, kind,
col)` — O(1) append on the hot `feed` path. A separate query method folds the
flat list into structured `CommandRegion`s on demand (per-frame or per-gesture,
not per-byte), isolating the assembly logic where it's easy to test.

```rust
pub struct CommandRegion {
    pub prompt_start: isize,        // absolute line index at query time
    pub input_start:  Option<isize>,
    pub output_start: Option<isize>,
    pub command_end:  Option<isize>,
    pub exit_code:    Option<i32>,
}
```

Absolute line indices are computed *at query time* from current scrollback
length, so callers always get viewport-consistent coordinates (the `line_at`
convention).

## Staged slices

### K1 — Parse OSC 133 marks (no storage)

**What:** Add `133 => self.handle_osc_133(rest)` to `handle_osc`, plus a
`handle_osc_133` modeled on `handle_osc_1337`. Parse the leading token
(`A`/`B`/`C`/`D`) by splitting `rest` on `;`. For `D`, parse the optional exit
code from the next field. **Tolerate and ignore** trailing `key=value` params
(`A;aid=7`, `D;1;err=…`) by reading only positional fields we understand.
Parser only logs/discards; no state stored yet. Define `SemanticMarkKind`.

**Unlocks:** Nothing user-facing; isolates parse/param-tolerance for unit
tests without grid coupling.

**Tests:** Feed `;A`, `;B`, `;C`, `;D`, `;D;0`, `;D;130`, `;D;1;extra=x`,
`;A;aid=foo` and assert the parsed `SemanticMarkKind` (via a `#[cfg(test)]`
helper). Assert `;Z`, empty, `;D;notanumber` are no-ops.

**Risk:** Exit code may be negative or non-numeric; parse as `i32`, store
`None` on failure. ST vs BEL terminator already handled by the parser.

### K2 — Store live marks on the primary grid

**What:** Add `semantic_marks: Vec<(usize, SemanticMarkKind, usize)>` to
`Terminal`. In `handle_osc_133`, when **not** `use_alternate`, append a mark
anchored to `cursor.row`/`cursor.col`; when `use_alternate`, early-return.
Clear marks on ED-2 / full-screen clear and in `full_reset` alongside the
scrollback-placement clears. No scroll/scrollback handling yet (K3) — a screen
clear simply wipes them.

**Unlocks:** A queryable "marks on the live grid" list — enough for a first
integration test and a debug overlay.

**Tests:** Feed a fake prompt cycle (`A`, text, `B`, `C`, output, `D;0`)
without scrolling; assert stored `(row, kind)` tuples. Assert marks on the alt
screen (after `\x1b[?1049h`) are ignored. Assert ED-2 (`\x1b[2J`) clears marks.

### K3 — Survive scroll, scrollback eviction, and resize

**What:** The load-bearing slice. Convert to the two-domain model (live grid-row
marks + a `scrollback_marks` VecDeque anchored by `scrollback_row: isize`),
mirroring `Placement`/`ScrollbackPlacement`.
- In `scroll_region_up_by`: when full-region primary scroll pushes rows into
  scrollback, promote any live mark whose row scrolled off the top into
  `scrollback_marks` with `scrollback_row = sb_len + offset`; shift down marks
  that remain live — reuse the placement-promotion arithmetic.
- Add a mark analogue of `evict_scrollback_placement_front`, called from **both**
  scrollback `pop_front` sites (scroll and resize spill). Drop anchors at row 0,
  decrement the rest.
- In `resize`: migrate marks through the spill and the grow-refill-from-
  scrollback path the same way placements are migrated. Live-row marks shift
  with their row on shrink/grow.
- No column reflow on horizontal resize — clamp-only, like the rest of the
  codebase. Note this explicitly.

**Unlocks:** Correct mark lifecycle — prerequisite for any history-aware
feature. Still no UI.

**Tests:** Mirror the resize/scrollback suite:
- Emit a mark, scroll it into scrollback via LFs; assert it's now scrollback-
  anchored at the right index and `line_at` of that absolute index points at
  the marked content.
- Fill scrollback past a small `scrollback_limit` (`Terminal::new(80,24,3)`);
  assert oldest marks evicted, remaining indices decremented.
- Resize-shrink that spills marked rows; resize-grow that pulls them back —
  assert indices track their rows.
- Alt-screen resize doesn't touch marks.

**Risk:** Off-by-one in the `sb_len + offset` anchor math is the classic bug —
copy the placement formula verbatim and assert explicit indices. The
`view_offset` bump doesn't affect mark storage (marks are scrollback-indexed,
not viewport-indexed), but tests should scroll the viewport and confirm mark
queries are unaffected.

### K4 — Command-region query API

**What:** Add `pub fn command_regions(&self) -> Vec<CommandRegion>`. Walk the
ordered union of scrollback marks (oldest first) and live marks, converting
each anchor to an absolute line index via current scrollback length, and fold
the flat `A/B/C/D` stream into `CommandRegion`s. State machine: `A` opens a
region; `B`/`C` fill input/output starts; `D` closes it with its exit code.
Tolerate missing marks (shell may emit only `A`+`D`, or be interrupted mid-
command leaving `command_end: None`). Add accessors: `last_command_region()`,
`previous_prompt_above(line)`, `next_prompt_below(line)`.

**Unlocks:** The full data foundation — "where did the last command's output
start/end, and did it succeed."

**Tests:** Multi-command session (two full `A B C D` cycles) → two regions with
correct absolute indices and exit codes. Interrupted command (`A B C`, no `D`)
→ `command_end: None`. Back-to-back prompts (`A B`, `A B`) → two regions.
Combine with K3: scroll the first region into scrollback and assert its
absolute indices shift correctly.

### K5 — Prompt navigation (UI) — *first user-facing slice*

**What:** Wire a keybinding (via `src/input.rs` + the main loop) calling
`next_prompt_below` / `previous_prompt_above` against the viewport top and
adjusting `view_offset` (`scroll_up`/`scroll_down`) to bring that prompt to the
top. Pure consumer of K4.

**Unlocks:** Jump-to-previous/next-prompt — the single most-requested
shell-integration feature.

**Tests:** Assert `view_offset` after invoking navigation with a known set of
regions. Manual check via the `run` skill.

### K6 — Exit-status gutter indicator (UI)

**What:** Expose per-visible-row mark info to the renderer (for each visual
row: whether a `CommandEnd` with exit code falls there, and success/failure).
Renderer draws a small green/red gutter glyph at the prompt line. Read-only
consumer of K4; follows the Kitty placement-run renderer pattern.

**Unlocks:** At-a-glance success/failure of past commands.

**Tests:** Unit-test the per-row query mapping; visual check via `run`.

### K7 — Select-last-command-output (UI)

**What:** Use `last_command_region()`'s `output_start`..`command_end` span to
drive the existing selection mechanism. A keybinding selects (and optionally
copies) the entire output of the last command.

**Unlocks:** "Copy last command output."

**Tests:** Assert the computed selection span for a known session.

## Deferred (K-slice "deferred indefinitely" style)

- **Autocomplete overlay** — the `B`..`C` input region delimits exactly the
  text the user typed, the anchor an autocomplete/suggestion overlay needs.
  Data available from K4; the overlay UI (popup positioning, suggestion source,
  accepting completions back to the PTY) is its own feature. Build when
  requested.
- **OSC 133 `A` sub-params beyond `aid`** (`cl=`, `redraw=`, continuation-
  prompt `P k=…`) — parsed-and-ignored in K1. Add semantics only if a real
  integration script exercises them.
- **Horizontal-resize column reflow of marks** — marks keep their column
  verbatim across width changes (consistent with clamp-only horizontal
  resize). Revisit only if a feature needs column-accurate marks after reflow.
- **Alt-screen marks** — intentionally dropped; no shell emits prompt marks on
  the alt screen.
- **Navigation by status** ("jump to next *failed* command") — trivial
  extension of K4's accessors; add on request.

## Testing strategy summary

K1–K4 are covered by inline `#[cfg(test)]` tests in `terminal.rs` that build
OSC 133 byte strings, `feed` them to a `Terminal::new(...)`, and assert on the
new query APIs — the pattern of `osc_1337_*` and the resize/scrollback suite.
Add an `osc_133(payload) -> String` test helper alongside the existing
`iterm_osc` helper. K3 uses small-`scrollback_limit` Terminals to force
eviction deterministically. Per project + global CLAUDE.md, run the
`unit-test-writer` agent after each slice. UI slices (K5–K7) get thin unit
tests on computed values plus manual checks via `run`.

## Execution notes

- **Strict ordering K1 → K2 → K3 → K4.** K3 is highest-risk (anchor
  arithmetic); don't start UI work until K4's query API is proven against
  regions scrolled into scrollback.
- **Parallelizable:** once K4 lands, K5 / K6 / K7 are independent consumers.
- **Checkpoint:** after K3, validate the full mark lifecycle (scroll + evict +
  resize round-trip) before building the query API — an anchoring bug silently
  corrupts every downstream feature.
- **Blast radius:** all core changes are additive — a new `Terminal` field, a
  new OSC arm, new methods, and small additions inside `scroll_region_up_by`,
  the `resize` spill loop, and `full_reset`. No existing behavior changes if
  OSC 133 is never received, so any slice rolls back cleanly.
- **Per CLAUDE.md:** do all work in a dedicated worktree; one PR per slice via
  the `git push` + `gh pr create` flow.
