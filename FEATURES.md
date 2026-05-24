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
  resize; independent of `Style` so an SGR reset can't sever it. The `id=`
  param is honored — links are interned by `(id, uri)`, so non-contiguous
  spans of one logical link **co-highlight** on hover (`find_osc8_link_at`
  emits one underline segment per visible run), while anonymous spans stay
  distinct. Takes precedence over the heuristic.
- Graphics: full Kitty protocol (inline / placement / animation / shared mem)
  and iTerm2 OSC 1337 `File=`.
- Shell semantics: OSC 133 prompt marks (navigation, exit-status gutter,
  select-last-output), OSC 7 cwd -> window title, OSC 2122/2124 input + history
  reporting feeding multi-source autocomplete (filesystem / $PATH / $HISTFILE).
- **Find / search in scrollback** (Cmd-F) — `Search` state model (`search.rs`),
  smart-case matching, per-line match highlighting, and wrap-around next/prev
  navigation. Cmd-F opens the search bar and owns the keyboard while active.
- **Multi-window** (Cmd-N) — launches a fresh Yutani window, cascading
  down-and-right from its spawner. (This is separate windows, not in-window
  tabs/splits — see below.)
- First-run onboarding wizard rendered in the PTY (color / theme / font /
  autocomplete setup).
- Rendering: wgpu pipeline, glyph atlas, programming ligatures, procedural
  box-drawing, glow/bloom, CRT scanlines, blur.
- Config: TOML config + color schemes, hot-reload (Cmd-Shift-R) of palette/glow.
- Cross-platform font loading (Core Text / GDI / fontconfig); macOS primary.

---

## Remaining / missing features

### Tier 1 — defining gaps vs. mainstream terminals

#### Tabs, splits, and panes
Single-window, single-pane today. Cmd-N opens separate top-level windows, but
there's no in-window multiplexing model behind the macOS "new tab" toolbar
button. This is the largest gap vs. iTerm2 / kitty / WezTerm / Ghostty. Touches
window management, input routing, renderer viewport handling, and config.
Largest effort item on this list.

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

*(none outstanding for the shipped hyperlink support — the OSC 8 `id=`
co-highlighting follow-up has landed.)*

---

## Suggested priority order

1. **Tabs / splits** — the defining missing capability.
2. **Scrollbar + configurable keybindings** — visible polish.
3. **Bell / notifications** — small scope, frequently noticed.
