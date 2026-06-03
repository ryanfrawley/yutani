#!/usr/bin/env bash
# Record a sampling profile of Yutani under a fixed, reproducible workload.
#
# Pipeline:
#   1. build the `profiling` profile (release + debug symbols)
#   2. generate the deterministic corpus if missing
#   3. launch the binary under `samply`, with SHELL pointed at workload.sh, which
#      floods the chosen corpus into the terminal then terminates the app
#   4. write a Firefox-Profiler JSON; print how to view it
#
# The flamegraph lives in the recorded profile (open it; the call-tree / flame
# views are there). A wall-clock macro number per workload is appended to the
# results file for quick before/after comparison without opening the UI.
#
# Usage:
#   scripts/profile/profile.sh [workload] [options]
#
#   workload : stream | glyphs | scroll | mixed   (default: stream)
#   --repeat N   times to replay the corpus           (default: 4)
#   --mb N       approx corpus size per workload, MiB  (default: 8)
#   --out FILE   profile json path        (default: target/profile/<workload>.json)
#   --rate HZ    sampling rate                          (default: 1000)
#   --open       launch `samply load` on the result instead of save-only
#   --timeout S  watchdog kill after S seconds          (default: 120)
#
# Examples:
#   scripts/profile/profile.sh stream                 # baseline, save-only
#   scripts/profile/profile.sh glyphs --open          # record + open the UI
#   scripts/profile/profile.sh stream --out target/profile/stream-after.json
set -euo pipefail

# --- locate repo root (this script lives in scripts/profile/) ---------------
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$ROOT"

# --- defaults ---------------------------------------------------------------
WORKLOAD="stream"
REPEAT=4
MB=8
RATE=1000
OUT=""
OPEN=0
TIMEOUT=120

# --- parse args -------------------------------------------------------------
while [[ $# -gt 0 ]]; do
  case "$1" in
    stream|glyphs|scroll|mixed) WORKLOAD="$1"; shift ;;
    --repeat) REPEAT="$2"; shift 2 ;;
    --mb) MB="$2"; shift 2 ;;
    --out) OUT="$2"; shift 2 ;;
    --rate) RATE="$2"; shift 2 ;;
    --open) OPEN=1; shift ;;
    --timeout) TIMEOUT="$2"; shift 2 ;;
    -h|--help) sed -n '2,33p' "$0"; exit 0 ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done

OUT="${OUT:-target/profile/${WORKLOAD}.json}"
CORPUS_DIR="target/profile/corpus-${MB}mb"
RESULT="target/profile/results.txt"

# --- ensure samply ----------------------------------------------------------
if ! command -v samply >/dev/null 2>&1; then
  echo "samply (the sampling profiler) is not installed." >&2
  echo "Install it with:  cargo install samply --locked" >&2
  read -r -p "Install it now? [y/N] " ans
  if [[ "$ans" == "y" || "$ans" == "Y" ]]; then
    cargo install samply --locked
  else
    echo "Aborting: samply is required." >&2
    exit 1
  fi
fi

# --- build ------------------------------------------------------------------
echo ">> building (profile=profiling)..." >&2
cargo build --profile profiling
BIN="$ROOT/target/profiling/yutani"
[[ -x "$BIN" ]] || { echo "binary not found at $BIN" >&2; exit 1; }

# Produce a .dSYM next to the binary. `split-debuginfo = "unpacked"` leaves the
# DWARF in scattered deps/*.o files that samply won't follow; dsymutil walks the
# binary's debug map and consolidates them into yutani.dSYM, which samply
# auto-discovers to symbolicate our Rust frames (otherwise the profile is just
# raw addresses and before/after function-level diffs are impossible).
if command -v dsymutil >/dev/null 2>&1; then
  echo ">> generating dSYM for symbolication..." >&2
  dsymutil "$BIN" 2>/dev/null || echo "   (dsymutil failed; frames may be unsymbolicated)" >&2
fi

# --- corpus -----------------------------------------------------------------
if [[ ! -f "$CORPUS_DIR/${WORKLOAD}.txt" ]]; then
  echo ">> generating corpus (~${MB} MiB each) in $CORPUS_DIR..." >&2
  python3 "$SCRIPT_DIR/gen-corpus.py" "$CORPUS_DIR" --mb "$MB"
fi

# --- isolate state so first-run onboarding never blocks the run, and the
#     user's real ~/.local/state/yutani is left untouched --------------------
TMPSTATE="$(mktemp -d)"
mkdir -p "$TMPSTATE/yutani"
echo 1 > "$TMPSTATE/yutani/onboarded"
cleanup() { rm -rf "$TMPSTATE"; }
trap cleanup EXIT

mkdir -p "$(dirname "$OUT")"
chmod +x "$SCRIPT_DIR/workload.sh"

echo ">> recording: workload=$WORKLOAD repeat=$REPEAT rate=${RATE}Hz" >&2
echo "   keep the Yutani window frontmost/visible (occluded windows skip rendering)." >&2

# `--unstable-presymbolicate` writes a <OUT>.syms.json sidecar with the resolved
# symbol for every sampled frame, gathered at record time from the dSYM. This is
# what lets the saved profile (and scripted analysis) show function names without
# a live `samply load` symbolication pass.
SAMPLY_ARGS=(record --rate "$RATE" --unstable-presymbolicate -o "$OUT")
[[ "$OPEN" -eq 1 ]] || SAMPLY_ARGS+=(--save-only)

# --- run under the profiler, with a watchdog safety net ---------------------
set +e
env \
  XDG_STATE_HOME="$TMPSTATE" \
  SHELL="$SCRIPT_DIR/workload.sh" \
  YUTANI_BENCH="$WORKLOAD" \
  YUTANI_BENCH_CORPUS="$ROOT/$CORPUS_DIR" \
  YUTANI_BENCH_REPEAT="$REPEAT" \
  YUTANI_BENCH_RESULT="$ROOT/$RESULT" \
  samply "${SAMPLY_ARGS[@]}" -- "$BIN" &
SAMPLY_PID=$!

(
  sleep "$TIMEOUT"
  if kill -0 "$SAMPLY_PID" 2>/dev/null; then
    echo "!! watchdog: ${TIMEOUT}s elapsed, killing run" >&2
    pkill -TERM -f "target/profiling/yutani" 2>/dev/null
    kill -TERM "$SAMPLY_PID" 2>/dev/null
  fi
) &
WATCHDOG=$!

wait "$SAMPLY_PID"
STATUS=$?
kill "$WATCHDOG" 2>/dev/null
wait "$WATCHDOG" 2>/dev/null
set -e

# --- report -----------------------------------------------------------------
echo >&2
if [[ -f "$RESULT" ]]; then
  LAST="$(tail -n 1 "$RESULT")"
  echo ">> wall-clock (workload elapsed): $LAST" >&2
  echo "   (full history in $RESULT)" >&2
fi
if [[ -f "$OUT" ]]; then
  echo ">> profile written: $OUT" >&2
  echo "   view it with:  samply load $OUT" >&2
else
  echo "!! no profile written (status $STATUS) -- did the window stay focused and the shell run?" >&2
  exit 1
fi
