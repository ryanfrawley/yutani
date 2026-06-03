# Profiling harness

Reproducible sampling profiles + a wall-clock macro number for the Yutani
terminal, so the performance improvements identified in the analysis can be
measured before/after instead of guessed at.

## What's here

| file            | role |
|-----------------|------|
| `profile.sh`    | the driver: build → generate corpus → record under `samply` → report |
| `gen-corpus.py` | deterministic corpus generator (byte-identical across runs) |
| `workload.sh`   | the fake `$SHELL` the app execs; floods a corpus, times it, then quits the app |

The profiler is [`samply`](https://github.com/mstange/samply) — chosen over
`cargo flamegraph` because on macOS it needs **no `sudo`** and doesn't fight
SIP. It produces a [Firefox Profiler](https://profiler.firefox.com) recording
whose **Flame Graph** tab is the flamegraph (plus a call tree and an inverted
"heaviest self-time" view). The first run offers to `cargo install samply`.

## Quick start

```sh
# baseline, plain-text/SGR streaming workload (parser + grid + scrollback + render)
scripts/profile/profile.sh stream

# open the interactive flamegraph UI instead of just saving
scripts/profile/profile.sh glyphs --open

# then view any saved profile:
samply load target/profile/stream.json
```

> Keep the Yutani window **frontmost and visible** during the run. Occluded /
> background windows deliberately skip the render path, so a hidden window would
> profile the parser but not the renderer.

## Workloads

Each targets a different hot path from the analysis:

- **`stream`** — colorized log lines. ANSI parser, change-gated grid writes,
  scrollback push/evict (`to_vec()` per scrolled row), full-grid vertex rebuild.
- **`glyphs`** — CJK + emoji + accents + box-drawing. Glyph-cache `HashMap`
  lookups (default SipHash), rasterization on miss, cluster intern, **full-atlas
  texture uploads**.
- **`scroll`** — short lines + periodic clear/home. Maximizes line-feed scroll
  churn (one `Vec<Cell>` clone+alloc per scrolled row).
- **`mixed`** — all three, a rough "real session" blend.

The four above are `cat`-floods (parse-bound): bytes arrive faster than vsync, so
they coalesce into a handful of rendered frames. Good for the parser / grid /
scrollback path, useless for the render path.

- **`redraw`** — the **render-bound** workload. Not a corpus + `cat`; a *paced*
  full-screen repainter (`redraw.py`) that emits one repaint per frame interval
  and sleeps between frames, so each repaint renders as its own frame. Every
  cell's glyph shifts each frame, so the per-row cache misses on every row and
  `update_vertices` does a full rebuild — exercising the glyph-lookup (FxHash) and
  vertex-Vec paths the render tier touches. `YUTANI_REDRAW_FRAMES` / `_FPS` /
  `_BATCH` tune it; `_BATCH=N` inserts a brief idle every N frames so `PERFLOG`
  flushes a per-batch `[perf]` line (the way to read per-frame `update_vertices`
  time).

  Caveat learned from building it: the renderer is **frame-paced at ~62fps and
  each `update_vertices` is sub-millisecond** (~80µs on a normal grid), so the
  render path is ~0.5% of a frame budget — render-tier changes are real per-frame
  headroom but **invisible to wall-clock / aggregate sampling** under any
  workload. To quantify a render change, read `PERFLOG`'s `update … x N` (per-frame
  µs), not the macro number.

## `bench_hasher.rs` — isolate a hasher change

`update_vertices` is too small a slice to measure a hasher swap through the GUI.
`bench_hasher.rs` is a dependency-free micro-benchmark of the exact hot op — a
`HashMap<char, AtlasEntry>::get` per cell — under SipHash vs FxHash:

```sh
rustc -O scripts/profile/bench_hasher.rs -o /tmp/bench_hasher && /tmp/bench_hasher
# SipHash ~5.5 ns/lookup, FxHash ~1.8 ns/lookup -> ~3x, ~37us/frame saved at 10k cells
```

## Measuring an improvement (before/after protocol)

The macro number is the workload's wall-clock consume time, appended to
`target/profile/results.txt`. The flamegraph tells you *where* the time went.

```sh
# 1. baseline on the unchanged tree
scripts/profile/profile.sh stream --out target/profile/stream-before.json
scripts/profile/profile.sh glyphs --out target/profile/glyphs-before.json

# 2. apply ONE improvement (e.g. swap the glyph-cache hasher), rebuild happens
#    automatically inside profile.sh

# 3. re-record to a distinct file
scripts/profile/profile.sh stream --out target/profile/stream-after.json
scripts/profile/profile.sh glyphs --out target/profile/glyphs-after.json

# 4. compare
tail target/profile/results.txt            # wall-clock before vs after
samply load target/profile/glyphs-before.json   # inspect, compare self-times
samply load target/profile/glyphs-after.json
```

For stable wall-clock numbers: close other heavy apps, plug in power, and run
each workload 3× (the corpus is deterministic, so variance is just the machine).
Increase signal with `--repeat 8` or `--mb 16` on a fast machine.

## Why `cat` wall-clock is a valid metric

The PTY kernel buffer is small, so once the app stops draining its master fd,
`cat` blocks on `write()`. The corpus replay time therefore tracks the app's
**parse + grid + scrollback** consume rate. Rendering is paced separately
(~62 fps) and overlaps, so it shows up in the flamegraph but only partially in
the wall-clock number — read both. For render-dominated changes (atlas uploads,
vertex rebuild, glow passes) lean on the flamegraph's self-time view.

## Notes / caveats

- Runs use a throwaway `XDG_STATE_HOME` (temp dir, stamped onboarded) so
  first-run onboarding never blocks automation and your real
  `~/.local/state/yutani` is untouched. Config is still read from
  `~/.config/yutani` — keep glow/blur settings constant across before/after.
- The `profiling` Cargo profile = `release` + `debug = true` + frame pointers,
  so stacks resolve to real (inlined) symbols. Build artifacts land in
  `target/profiling/`.
- A watchdog kills the run after `--timeout` (default 120s) if the app wedges.
- `target/` is git-ignored, so corpora and profiles don't get committed.
