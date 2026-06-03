#!/usr/bin/env python3
"""Render-bound benchmark driver — the counterpart to workload.sh.

A `cat`-flood is *parse*-bound: the bytes arrive faster than vsync, so they
coalesce into a handful of rendered frames and the renderer barely runs (the
per-frame `update_vertices` rebuild is <1% of samples). That makes it useless
for measuring render-path changes (the low-risk tier #208: FxHash glyph caches +
vertex-buffer pooling).

This driver instead PACES output: it repaints the whole screen once per frame
interval and sleeps between frames, so each repaint gets its own rendered frame.
Every cell's glyph shifts each frame, so the renderer's per-row cache misses on
every row and `update_vertices` does a full rebuild — exercising exactly the
glyph-lookup (FxHash) and vertex-Vec-allocation paths #208 touched. Parsing
stays light (≈one byte per cell, no per-cell SGR), so the render rebuild is a
large fraction of active CPU instead of a rounding error.

Run as the PTY child (profile.sh points $SHELL here for the `redraw` workload).

Env:
  YUTANI_REDRAW_FRAMES  number of frames to paint        (default 600)
  YUTANI_REDRAW_FPS     target frames/sec (paces sleep)  (default 55)
  YUTANI_BENCH_RESULT   file to append the elapsed time / effective fps
"""
import os
import signal
import sys
import time

# A small alphabet so glyphs stay resident in the atlas after frame 1 — we want
# to measure cache *lookups* (FxHash), not rasterization.
ALPHA = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789 .,:;/\\|-_=+*#@&%"


def main():
    frames = int(os.environ.get("YUTANI_REDRAW_FRAMES", "600"))
    fps = float(os.environ.get("YUTANI_REDRAW_FPS", "55"))
    result = os.environ.get("YUTANI_BENCH_RESULT", "/dev/null")
    dt = 1.0 / fps
    na = len(ALPHA)
    w = sys.stdout.write

    # Let the window/GPU come up and the app set the PTY winsize before we size
    # frames to the grid.
    time.sleep(0.5)
    try:
        cols, rows = os.get_terminal_size(1)
    except OSError:
        cols, rows = 80, 24
    rows = max(1, rows)
    cols = max(1, cols)

    w("\x1b[?25l\x1b[2J")  # hide cursor, clear
    sys.stdout.flush()

    # Precompute each row as a rotating window over a 2x-length ring of ALPHA,
    # so frame f / row r is a cheap slice — keeps the Python side from being the
    # bottleneck.
    # Every `batch` frames, idle long enough (> the app's 150ms PERFLOG flush
    # threshold) that the perf burst flushes a `[perf]` line covering that batch.
    # Without a pause the whole run is one uninterrupted burst that only flushes
    # after the app is killed — too late to capture. 0 disables the pause.
    batch = int(os.environ.get("YUTANI_REDRAW_BATCH", "0"))
    ring = (ALPHA * ((cols // na) + 2))
    t0 = time.time()
    for f in range(frames):
        parts = ["\x1b[H"]
        for r in range(rows):
            off = (f + r) % na
            parts.append(ring[off:off + cols])
            if r < rows - 1:
                parts.append("\r\n")
        w("".join(parts))
        sys.stdout.flush()
        time.sleep(dt)              # pace so each repaint renders as its own frame
        if batch and (f + 1) % batch == 0:
            time.sleep(0.30)        # idle past the 150ms threshold → flush a [perf] line
    elapsed = time.time() - t0

    w("\x1b[0m\x1b[?25h\x1b[2J\x1b[H")
    sys.stdout.flush()
    eff = frames / elapsed if elapsed else 0
    with open(result, "a") as fh:
        fh.write(f"redraw {elapsed:.4f} {frames}f {eff:.1f}fps\n")
    sys.stderr.write(f"redraw.py: {frames} frames in {elapsed:.2f}s ({eff:.1f} eff fps)\n")

    time.sleep(0.2)
    os.kill(os.getppid(), signal.SIGTERM)


if __name__ == "__main__":
    main()
