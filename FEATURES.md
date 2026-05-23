# Yutani — Feature Status & Remaining Work

A snapshot of what the terminal does today and what's still missing, to guide
prioritization. "Done" reflects the current `main`; "Missing" is work not yet
started.

## Already implemented (for context)

- Full VT/CSI/SGR emulation: cursor movement, erase/edit, scroll regions,
  left/right margins, truecolor + 256-color + ANSI, alternate screen,
  scrollback ring buffer.
- Mouse: press-release / button-motion / any-motion tracking, SGR encoding,
  wheel-notch accumulation. Drag-to-select with word/line granularity and
  system-clipboard copy.
- **Clickable links** — two paths: heuristic `http(s)://` detection over the
  cell grid (`find_url_in_cells`, wrap-aware via `build_wrapped_line`) for bare
  URLs in any output, plus **OSC 8** explicit hyperlinks (below) for
  app-declared links. Cmd-hover underline + Cmd-click to `open(1)`, gated by a
  scheme allowlist (`is_safe_url`).
- **OSC 8 explicit hyperlinks** — `handle_osc_8` parses `OSC 8 ; params ; URI`;
  the active link rides on the cursor and is stamped onto each printed cell as
  an interned id (`Cell.hyperlink` + `HyperlinkStore`). Survives scrollback and
  resize; independent of `Style` so an SGR reset can't sever it. On hover,
  `find_osc8_link_at` resolves the cell's link and takes precedence over the
  heuristic. *Known limitation: the `id=` param is not yet honored — see below.*
- Graphics: full Kitty protocol (inline / placement / animation / shared mem)
  and iTerm2 OSC 1337 `File=`.
- Shell semantics: OSC 133 prompt marks (navigation, exit-status gutter,
  select-last-output), OSC 7 cwd -> window title, OSC 2122/2124 input + history
  reporting feeding multi-source autocomplete (filesystem / $PATH / $HISTFILE).
- Rendering: wgpu pipeline, glyph atlas, programming ligatures, procedural
  box-drawing, glow/bloom, CRT scanlines, blur.
- Config: TOML config + color schemes, hot-reload (Cmd-Shift-R) of palette/glow.
- Cross-platform font loading (Core Text / GDI / fontconfig); macOS primary.

---

## Remaining / missing features

### Tier 1 — defining gaps vs. mainstream terminals

#### Tabs, splits, and panes
Single-window, single-pane today. No multiplexing model behind the macOS
"new tab" toolbar button. This is the largest gap vs. iTerm2 / kitty /
WezTerm / Ghostty. Touches window management, input routing, renderer
viewport handling, and config. Largest effort item on this list.

#### Find / search in scrollback
No way to search the visible buffer or history (Cmd-F). High-frequency feature,
relatively contained: needs a search-state model, match highlighting in the
renderer, and next/prev navigation keybindings.

### Tier 2 — polish users notice quickly

#### Scrollbar / scroll position indicator
Scrolling works but there's no visual feedback of position or buffer extent.

#### Configurable keybindings
Shortcuts (Cmd-Shift-R, Cmd-Shift-O, Ctrl+Space, prompt nav) appear hardcoded.
No keybinding section in config; users can't rebind or add chords.

#### Bell / notifications
BEL is parsed but there's no visible or audible bell, and no OSC 9 / OSC 777
desktop-notification support.

### Tier 3 — nice to have

#### Session persistence / restore
No saving of window state or restoring sessions on relaunch.

#### Profiles
Single global config; no per-profile shell / theme / working directory.

#### Window resize on config hot-reload
Cmd-Shift-R reloads palette and glow but does not re-apply font size to window
dimensions.

#### Quick / dropdown ("Quake") terminal
Global-hotkey drop-down window mode.

#### Charset switching (DEC Special Graphics, G0/G1)
`ESC ( 0` etc. are consumed but not implemented. Legacy TUIs that draw lines
via the special graphics charset instead of UTF-8 box characters render wrong.

#### Sixel graphics
Not supported. Kitty + iTerm2 protocols already cover the image use case, so
this is low priority — listed for completeness.

---

## Known limitations / follow-ups

### OSC 8 `id=` parameter not yet honored
OSC 8's params field carries an optional `id=` token. Its purpose is to let a
program mark several — possibly **non-contiguous** — cell spans as parts of the
*same* logical link, so hovering any one highlights all of them (e.g. a link
deliberately split across regions, or the same link rendered in several
places). Conversely, two spans with the *same URI* but *different* (or absent)
ids are meant to be treated as **distinct** links.

The current implementation ignores `id=` and groups purely by **interned URI**.
Two consequences:

- **Sibling spans aren't co-highlighted.** Hover highlights only the contiguous
  run under the cursor (extended across autowrapped rows). Non-contiguous spans
  that an app tied together with a shared `id=` won't light up together.
- **Same-URI spans merge.** Two adjacent spans with identical URIs but separate
  `id=` values (intended as distinct links) collapse into one hover span. Rare
  in practice, and harmless for opening — the target is identical.

Neither breaks click-to-open; this is purely about hover-grouping fidelity.
To fully honor `id=`: intern by `(id, uri)` when an id is present, and on hover
collect every cell sharing that interned id (a grid/scrollback scan, or an
`id -> cells` index) rather than just the contiguous run. Deferred as a
low-priority refinement.

---

## Suggested priority order

1. **Tabs / splits** — the defining missing capability.
2. **Find in scrollback** — high frequency, contained scope.
3. **Scrollbar + configurable keybindings** — visible polish.
4. **OSC 8 `id=` grouping** — small refinement to the shipped hyperlink support.
