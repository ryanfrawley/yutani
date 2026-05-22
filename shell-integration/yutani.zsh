# yutani.zsh — zsh shell integration for the yutani terminal
# ---------------------------------------------------------------------------
# Source this from ~/.zshrc to light up yutani's shell-aware features:
#
#     source /path/to/yutani.zsh
#
# It emits three OSC channels on the PTY:
#
#   * OSC 7    — current working directory. Reported on every prompt so the
#                terminal knows $PWD (window titles, new-tab cwd, etc.).
#
#   * OSC 133  — FinalTerm semantic prompt marks (A/B/C/D). These power
#                prompt navigation (Cmd-Shift-Up/Down), the exit-status
#                gutter, and select-last-command-output.
#
#   * OSC 2122 — yutani-private current-input report: the live edit buffer
#                and cursor position, base64-encoded. Drives the filesystem
#                autocomplete popup. See design/osc2122-input-report.md.
#
# Opt-out: set YUTANI_NO_PROMPT_MARKS=1 (any non-empty value) BEFORE sourcing
# to suppress ALL OSC 133 emission — the prompt wrap and the C/D hooks. Use
# this if your prompt (e.g. starship, powerlevel10k) already emits OSC 133;
# otherwise you'd get duplicate marks. OSC 7 and OSC 2122 are unaffected.
#
# Safe everywhere: OSC 2122 is a private sequence that other terminals ignore,
# and OSC 133 / OSC 7 are widely understood, so this is a no-op (or a graceful
# enhancement) outside yutani and safe to keep in a shared ~/.zshrc.
# ---------------------------------------------------------------------------

# Only run in interactive shells — there's no line editor or prompt otherwise.
[[ -o interactive ]] || return

# Idempotency guard: re-sourcing must not double-register hooks or re-wrap the
# prompt (which would emit nested 133 marks).
(( ${+_YUTANI_INTEGRATION_LOADED} )) && return
typeset -g _YUTANI_INTEGRATION_LOADED=1

# Whether OSC 133 prompt marks are enabled (negated opt-out). Resolved once at
# source time.
typeset -g _yutani_prompt_marks=1
if [[ -n ${YUTANI_NO_PROMPT_MARKS:-} ]]; then
  _yutani_prompt_marks=0
fi

# The hook helpers we depend on. If they're unavailable (ancient zsh, exotic
# build) bail out rather than half-installing.
autoload -Uz add-zsh-hook 2>/dev/null || return
if (( $+functions[add-zsh-hook] == 0 )); then
  return
fi

# ---------------------------------------------------------------------------
# OSC 7 — working directory
# ---------------------------------------------------------------------------
# Emit file://<host>/<path>. We use ${HOST} for the host component. Exotic
# paths (spaces, non-ASCII, '?') are NOT percent-encoded — out of scope; the
# yutani parser strips the host and takes the path verbatim, so a plain path
# is fine for the common case.
_yutani_osc7() {
  printf '\e]7;file://%s%s\a' "${HOST}" "${PWD}"
}

# ---------------------------------------------------------------------------
# OSC 133 — semantic prompt marks
# ---------------------------------------------------------------------------
# Strategy:
#   A (prompt start) and B (input start) are baked into $PROMPT as zero-width
#     %{...%} escapes so they're re-emitted on every prompt redraw without a
#     hook firing.
#   C (command output start) is emitted in preexec, just before the command
#     runs.
#   D;<exit> (command end) is emitted in precmd for the PREVIOUS command,
#     carrying its exit status. The very first precmd (before any command has
#     run) must NOT emit a spurious D;0.

# Tracks whether a command has actually run, so the first precmd skips D.
typeset -g _yutani_cmd_ran=0

_yutani_precmd() {
  # Capture exit status of the just-finished command FIRST, before anything
  # else clobbers $?.
  local -i exit_status=$?

  if (( _yutani_prompt_marks )); then
    # D for the previous command (skip on the very first prompt).
    if (( _yutani_cmd_ran )); then
      printf '\e]133;D;%d\a' "$exit_status"
      _yutani_cmd_ran=0
    fi
  fi

  # Always report cwd on each prompt.
  _yutani_osc7
}

_yutani_preexec() {
  if (( _yutani_prompt_marks )); then
    # A command is about to run: mark output start and remember that a command
    # ran (so the next precmd emits its D).
    _yutani_cmd_ran=1
    printf '\e]133;C\a'
  fi
}

# Wrap the prompt with A (before) and B (after) as zero-width escapes. Only
# done when prompt marks are enabled. %{...%} tells zsh the enclosed bytes
# occupy no display columns.
if (( _yutani_prompt_marks )); then
  PROMPT="%{"$'\e]133;A\a'"%}${PROMPT}%{"$'\e]133;B\a'"%}"
fi

add-zsh-hook precmd _yutani_precmd
add-zsh-hook preexec _yutani_preexec

# ---------------------------------------------------------------------------
# OSC 2122 — current-input report (yutani-private)
# ---------------------------------------------------------------------------
# Emitted from the line editor on every edit/cursor move so yutani can drive
# the autocomplete popup. Format:
#
#     \e]2122;<cursor>;<base64(buffer)>\a
#
# <cursor> is zsh's $CURSOR, a code-point (character) offset — exactly what the
# terminal expects. <base64(buffer)> is STANDARD base64 of $BUFFER; an empty
# buffer yields empty base64, a valid "empty line" report.

# Dedup cache: line-pre-redraw fires very frequently, and base64 forks a
# process. Skip the fork+emit when neither buffer nor cursor changed since the
# last call.
typeset -g _yutani_last_buffer=
typeset -g -i _yutani_last_cursor=-1
# Distinguishes "cache holds an empty buffer" from "cache never populated", so
# a genuinely-empty first report still emits.
typeset -g -i _yutani_input_primed=0

_yutani_report_input() {
  # Suppress redundant emits on no-op redraws.
  if (( _yutani_input_primed )) \
     && [[ "$BUFFER" == "$_yutani_last_buffer" ]] \
     && (( CURSOR == _yutani_last_cursor )); then
    return
  fi
  _yutani_last_buffer="$BUFFER"
  _yutani_last_cursor=$CURSOR
  _yutani_input_primed=1

  local b64
  # print -rn -- emits the raw buffer with no escape processing / no trailing
  # newline; tr strips base64's line wrapping so the payload is one OSC string.
  b64="$(print -rn -- "$BUFFER" | base64 | tr -d '\n')"
  printf '\e]2122;%d;%s\a' "$CURSOR" "$b64"
}

# Reset the dedup cache when a fresh command line starts, so the first redraw
# of a new (possibly identical) line re-emits.
_yutani_reset_input_cache() {
  _yutani_last_buffer=
  _yutani_last_cursor=-1
  _yutani_input_primed=0
}

# At line-init, also emit the current (typically empty) buffer so a stale popup
# from an aborted or previous line is cleared even when no command ran (Ctrl-C
# abort emits no OSC 133 C). Safe to call _yutani_report_input here: it only
# does printf + variable reads, no `zle` builtins.
_yutani_init_input() {
  _yutani_reset_input_cache
  _yutani_report_input
}

# Compose via add-zle-hook-widget so we coexist with other line-pre-redraw /
# line-init consumers instead of clobbering them with `zle -N`.
autoload -Uz add-zle-hook-widget 2>/dev/null
if (( $+functions[add-zle-hook-widget] )); then
  add-zle-hook-widget line-pre-redraw _yutani_report_input
  # line-init fires when a new edit line begins; reset the cache and emit the
  # (empty) buffer so any stale popup is cleared — including after a Ctrl-C
  # abort, which runs no command and so emits no OSC 133 C.
  add-zle-hook-widget line-init _yutani_init_input
  # line-finish fires when the line is accepted; reset so the next line emits.
  # Do NOT emit here — $BUFFER holds the accepted command we don't want to report.
  add-zle-hook-widget line-finish _yutani_reset_input_cache
fi
