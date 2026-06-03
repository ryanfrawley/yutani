#!/bin/zsh -f
# Benchmark "shell" that Yutani execs as the PTY child during profiling.
#
# profile.sh launches the app with SHELL pointed here, so fork_pty/exec_login_shell
# run THIS instead of an interactive zsh. We flood the terminal with a fixed
# corpus, time how long the app takes to consume it, write the number where the
# driver can read it, then terminate the app so the profiling run is bounded.
#
# Env (all set by profile.sh):
#   YUTANI_BENCH         workload name: stream | glyphs | scroll | mixed
#   YUTANI_BENCH_CORPUS  directory holding <name>.txt corpora
#   YUTANI_BENCH_REPEAT  how many times to cat the corpus (default 4)
#   YUTANI_BENCH_RESULT  file to write the elapsed wall-clock seconds into
#
# Why wall-clock of `cat` is meaningful: the PTY kernel buffer is small, so once
# the app stops draining, cat blocks on write(). cat's total time therefore
# tracks the app's parse+grid+scrollback consume rate (render is paced
# separately at ~62fps and overlaps). The flamegraph shows *where* that time
# goes; this number is the macro before/after metric.

bench="${YUTANI_BENCH:-stream}"
corpus_dir="${YUTANI_BENCH_CORPUS:?YUTANI_BENCH_CORPUS not set}"
repeat="${YUTANI_BENCH_REPEAT:-4}"
result="${YUTANI_BENCH_RESULT:-/dev/null}"
corpus="$corpus_dir/$bench.txt"

# Give the GPU/window a moment to come up so we profile steady-state work, not
# first-frame surface creation.
sleep 0.5

if [[ ! -f "$corpus" ]]; then
  print -r -- "workload.sh: corpus not found: $corpus" >&2
  kill -TERM "$PPID" 2>/dev/null
  exit 1
fi

# zsh -f does not load zsh/datetime, so EPOCHREALTIME is unset. Use the
# built-in float SECONDS instead (typeset -F gives sub-second precision).
typeset -F SECONDS=0
for _ in $(seq 1 "$repeat"); do
  cat "$corpus"
done
elapsed=$SECONDS
printf '%s %s\n' "$bench" "$elapsed" >> "$result"
print -r -- "workload.sh: $bench x$repeat consumed in ${elapsed}s" >&2

# Let the final frames flush, then end the app regardless of shell_exit_mode so
# the sampler stops and writes its profile. SIGTERM is the clean path; the
# driver has a watchdog that escalates if anything wedges.
sleep 0.3
kill -TERM "$PPID" 2>/dev/null
exit 0
