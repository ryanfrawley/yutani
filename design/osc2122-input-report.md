# OSC 2122 — current-input report (yutani-private)

A yutani-private terminal extension that lets the terminal track the shell's
live interactive edit buffer. This terminal forwards every keystroke straight
to the PTY; the shell owns the edit line. To build autocomplete the terminal
needs to know the live input buffer and cursor without reconstructing them
from the grid — so the shell integration reports them explicitly.

This is a **private-use OSC number** (2122). It is unused by any known terminal
convention and is a silent no-op in terminals that don't implement it. It pairs
with the OSC 133 semantic prompt marks the shell already emits.

## Protocol

```
OSC 2122 ; <cursor> ; <base64(buffer)> ST
```

- `<cursor>` — decimal **character (code-point) offset** of the cursor within
  the buffer, as reported by zsh `$CURSOR`. The terminal clamps it to the
  buffer's character count.
- `<base64(buffer)>` — **STANDARD** base64 of the UTF-8 edit buffer (zsh
  `$BUFFER`). base64 is used so the buffer can safely contain `;`, control
  characters, and other bytes that would otherwise break the OSC framing. An
  empty buffer encodes to the empty string, so `OSC 2122 ; 0 ; ST` is a valid
  "empty active line" report (the terminal stores `Some` with an empty buffer,
  distinct from "no active edit line").
- `ST` — string terminator: either BEL (`\x07`) or `ESC \` (`\x1b\\`). The
  terminal's existing OSC layer handles both.

The shell's line editor emits this on **every edit and cursor move**, so the
terminal always has the current buffer + cursor.

## Lifecycle

The terminal exposes the most recent report via `Terminal::current_input() ->
Option<&CurrentInput>`:

- **Set** by each `OSC 2122` while the primary screen is active.
- **Ignored** while the alternate screen is active (full-screen apps like vim
  or less don't run a prompt line editor) — same rationale as OSC 133.
- **Cleared** to `None` when:
  - the shell submits a command (OSC 133 `C` — the command-output mark), or
  - the terminal switches to the alternate screen.

Malformed reports (missing `;`, non-numeric cursor, invalid base64, invalid
UTF-8) are dropped and leave the prior state untouched.

This is a data/parsing foundation only — there is no UI yet. A later
autocomplete slice consumes `current_input()` every frame.

## zsh hook

Copy-pasteable into `~/.zshrc` (alongside your existing OSC 133 / OSC 7
integration):

```zsh
_yutani_report_input() {
  printf '\e]2122;%d;%s\a' "$CURSOR" "$(print -rn -- "$BUFFER" | base64 | tr -d '\n')"
}
zle -N zle-line-pre-redraw _yutani_report_input
```

Notes:

- `zle-line-pre-redraw` fires on every edit and cursor movement while the line
  editor is active, so the report always reflects the live buffer.
- `$CURSOR` is a **code-point** offset (matches the protocol's character-indexed
  cursor); `$BUFFER` is the full edit buffer.
- `print -rn -- "$BUFFER"` emits the raw buffer with no escape processing and no
  trailing newline; piping through `base64` encodes it, and `tr -d '\n'` strips
  base64's line-wrapping so the whole payload is a single OSC string.
- `\a` is BEL (the ST). `\e]2122;...` is the OSC introducer.
- This is a yutani-private extension: in other terminals the sequence is an
  unrecognized OSC and is silently ignored, so the hook is safe to keep in a
  shared `~/.zshrc`.
