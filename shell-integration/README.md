# yutani shell integration (zsh)

`yutani.zsh` teaches your shell to talk to the yutani terminal so that
shell-aware features light up. Without it, yutani is still a perfectly good
terminal — these features just stay dark.

## What it enables

- **Prompt navigation** — jump between previous prompts with
  Cmd-Shift-Up / Cmd-Shift-Down.
- **Exit-status gutter** — a per-command indicator showing whether each
  command succeeded or failed.
- **Select last command output** — select the output of the most recent
  command in one gesture.
- **Filesystem autocomplete popup** — as you type a path, yutani offers
  completions inline (see key map below).

## Install

**Nothing to do — it's automatic (zsh).** Yutani forks your shell itself, so on
zsh it auto-loads this integration by pointing the shell's `$ZDOTDIR` at a
Yutani-managed directory that sources your real startup files first, then this
script. The script is embedded in the binary, so there's no path to keep in
sync. Just open a new Yutani window.

To turn auto-loading off (e.g. you'd rather wire it up yourself), export
`YUTANI_SHELL_INTEGRATION=0` before Yutani launches.

### Manual install (other shells, or opting out of auto-load)

Auto-loading currently covers **zsh only** (bash/fish are a planned follow-up —
see `FEATURES.md`). For those, or if you set `YUTANI_SHELL_INTEGRATION=0`, source
the script yourself from your `~/.zshrc`:

```zsh
source /path/to/shell-integration/yutani.zsh
```

(Adjust the path to wherever the yutani repo lives.) Open a new shell, or
`source ~/.zshrc`, to pick it up.

### Already using starship / powerlevel10k / a custom prompt?

If your prompt framework already emits OSC 133 semantic marks, let yutani's
script know so it doesn't add a second, duplicate set of marks. Either:

- let yutani own the prompt (source `yutani.zsh` and don't enable your
  framework's own OSC 133 integration), **or**
- keep your framework's marks and set, before sourcing:

  ```zsh
  export YUTANI_NO_PROMPT_MARKS=1
  source /path/to/shell-integration/yutani.zsh
  ```

  This suppresses all of yutani's OSC 133 emission (the prompt A/B wrap and
  the C/D hooks). OSC 7 (cwd) and OSC 2122 (autocomplete input) are
  unaffected and keep working.

## Autocomplete key map

When the completion popup is showing (from slice K11):

| Key         | Action                  |
| ----------- | ----------------------- |
| ↑ / ↓       | Move selection          |
| Tab / Enter | Accept the selection    |
| Esc         | Dismiss the popup       |

## How it works

The script emits three OSC channels on the PTY; the terminal consumes them:

- **OSC 7** (`\e]7;file://<host>/<path>\a`) — reported on every prompt, tells
  the terminal the current working directory.
- **OSC 133** A/B/C/D — FinalTerm semantic prompt marks. `A`/`B` are baked
  into `$PROMPT` as zero-width escapes (prompt start / input start); `C` is
  emitted in `preexec` (command output start); `D;<exit>` in `precmd` for the
  previous command (command end + exit status). These drive prompt navigation,
  the exit-status gutter, and select-last-output.
- **OSC 2122** — yutani-private current-input report: the live edit buffer and
  cursor, base64-encoded, emitted from the zle `line-pre-redraw` hook on every
  edit/cursor move. This drives the autocomplete popup. The wire protocol is
  parsed by `Terminal::handle_osc_2122` in `src/terminal.rs`.

The script composes with existing hooks (it uses `add-zsh-hook` and
`add-zle-hook-widget` rather than clobbering with `zle -N`), is idempotent on
re-source, and only runs in interactive shells. In terminals other than yutani
the private OSC 2122 is silently ignored and OSC 133 / OSC 7 are widely
understood, so it's safe to keep in a shared `~/.zshrc`.
