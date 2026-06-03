#!/usr/bin/env python3
"""Generate deterministic corpora for profiling the Yutani terminal.

Each corpus is a stream of bytes meant to be `cat`-ed straight into the PTY by
workload.sh, exercising one of the hot paths surfaced in the perf analysis:

  stream  — colorized log lines (SGR-heavy). Stresses the ANSI parser, the
            change-gated grid writes, scrollback push/evict, and a full-grid
            vertex rebuild + render every frame.
  glyphs  — CJK + emoji + accented Latin + box-drawing. Stresses the glyph
            cache (per-cell HashMap lookups), freetype/Core Text rasterization
            on miss, the cluster-intern path, and full-atlas texture uploads.
  scroll  — many short lines plus periodic clear/home. Maximizes line-feed
            scroll churn: one Vec<Cell> clone+alloc per scrolled row.
  mixed   — all three interleaved, a rough "real session" blend.

Determinism matters: the same corpus must be byte-identical across runs so a
before/after flamegraph and the wall-clock macro number are comparable. No RNG,
no timestamps — every byte is a pure function of a counter.

Usage:
  gen-corpus.py OUTDIR [--mb N]      # generate all four, ~N MB each (default 8)
"""

import argparse
import os
import sys

ESC = "\x1b"


def sgr(*codes):
    return f"{ESC}[" + ";".join(str(c) for c in codes) + "m"


RESET = sgr(0)

# A small fixed palette of 256-color fg codes cycled deterministically.
FG = [sgr(38, 5, c) for c in (39, 208, 76, 213, 245, 220, 51, 197)]
LEVELS = ["TRACE", "DEBUG", " INFO", " WARN", "ERROR"]
LEVEL_COLOR = [sgr(38, 5, 244), sgr(38, 5, 39), sgr(38, 5, 76), sgr(38, 5, 220), sgr(38, 5, 197)]

WORDS = (
    "connection pool exhausted retrying request handler dispatch latency "
    "cache miss flush segment compaction throughput backlog queue drain "
    "allocator arena reclaim atlas glyph vertex buffer shader pipeline frame"
).split()


def stream_lines(target_bytes):
    """Colorized structured-log lines until target_bytes emitted."""
    out = []
    total = 0
    i = 0
    while total < target_bytes:
        lvl = i % len(LEVELS)
        # deterministic pseudo-message from the counter
        w = " ".join(WORDS[(i + k) % len(WORDS)] for k in range(6))
        seq = i * 7919 % 1000000
        line = (
            f"{sgr(38,5,240)}2026-06-02T{i//3600%24:02d}:{i//60%60:02d}:{i%60:02d}.{seq%1000:03d}Z{RESET} "
            f"{LEVEL_COLOR[lvl]}{LEVELS[lvl]}{RESET} "
            f"{FG[i % len(FG)]}worker#{i % 16:02d}{RESET} "
            f"{sgr(1)}{w}{RESET} "
            f"seq={seq} bytes={seq*13 % 65536}\n"
        )
        b = line.encode("utf-8")
        out.append(b)
        total += len(b)
        i += 1
    return b"".join(out)


def glyph_lines(target_bytes):
    """Unicode-dense lines: CJK, emoji, combining marks, box drawing."""
    cjk = [chr(0x4E00 + (i * 37) % 0x2000) for i in range(64)]          # Han
    hira = [chr(0x3041 + i) for i in range(0, 80)]                       # Hiragana
    emoji = [chr(cp) for cp in range(0x1F600, 0x1F640)]                  # emoticons
    accents = list("áéíóúñàèìòùâêîôûäëïöüçãõ")
    box = list("─│┌┐└┘├┤┬┴┼━┃┏┓┗┛╔╗╚╝║═╬")
    # ZWJ family + skin-tone sequences exercise the cluster-intern path.
    zwj = ["\U0001F468‍\U0001F469‍\U0001F467", "\U0001F44D\U0001F3FD", "\U0001F3F3️‍\U0001F308"]
    pools = [cjk, hira, emoji, accents, box]

    out = []
    total = 0
    i = 0
    while total < target_bytes:
        pool = pools[i % len(pools)]
        # build a line ~60 glyphs wide drawing from one pool, plus a cluster
        chunk = "".join(pool[(i + k) % len(pool)] for k in range(60))
        line = f"{FG[i % len(FG)]}{chunk} {zwj[i % len(zwj)]}{RESET}\n"
        b = line.encode("utf-8")
        out.append(b)
        total += len(b)
        i += 1
    return b"".join(out)


def scroll_lines(target_bytes):
    """Short lines + periodic clear/home to maximize scroll churn."""
    out = []
    total = 0
    i = 0
    while total < target_bytes:
        if i % 200 == 0:
            # clear screen + home: exercises scroll-region / erase paths
            out.append(f"{ESC}[2J{ESC}[H".encode())
        line = f"{i:08d} {FG[i % len(FG)]}{'#' * (i % 40)}{RESET}\n"
        b = line.encode("utf-8")
        out.append(b)
        total += len(b)
        i += 1
    return b"".join(out)


def mixed(target_bytes):
    third = target_bytes // 3
    return stream_lines(third) + glyph_lines(third) + scroll_lines(third)


GENERATORS = {
    "stream": stream_lines,
    "glyphs": glyph_lines,
    "scroll": scroll_lines,
    "mixed": mixed,
}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("outdir")
    ap.add_argument("--mb", type=int, default=8, help="approx size of each corpus in MiB")
    args = ap.parse_args()

    os.makedirs(args.outdir, exist_ok=True)
    target = args.mb * 1024 * 1024
    for name, gen in GENERATORS.items():
        path = os.path.join(args.outdir, f"{name}.txt")
        data = gen(target)
        with open(path, "wb") as f:
            f.write(data)
        print(f"  {name:8s} {len(data)/1024/1024:6.2f} MiB  ->  {path}", file=sys.stderr)


if __name__ == "__main__":
    main()
