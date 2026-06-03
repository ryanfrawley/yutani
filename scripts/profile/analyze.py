#!/usr/bin/env python3
"""Symbolicate and summarize a samply profile (or diff two), offline.

`profile.sh` records with `--unstable-presymbolicate`, which writes a
`<profile>.syms.json` sidecar mapping each library's RVA ranges to symbol names.
samply's own UI joins that at view time; this script does the same join headless
so we can get a function-level self-time table — and a before/after diff — without
opening a browser.

Self-time = samples whose *leaf* frame is in a given function. We attribute by
the demangled symbol the sidecar gives for the leaf frame's address.

Usage:
  analyze.py PROFILE.json [--top N] [--thread NAME] [--grep REGEX]
  analyze.py BEFORE.json AFTER.json [--top N]          # diff mode
"""
import argparse
import bisect
import collections
import json
import os
import re
import sys


def load_symbols(profile_path):
    """Return (libs, code_id -> sorted (starts, ends, names))."""
    side = profile_path + ".syms.json"
    alt = profile_path.replace(".json", ".syms.json")
    if not os.path.exists(side) and os.path.exists(alt):
        side = alt
    if not os.path.exists(side):
        return None
    s = json.load(open(side))
    strtab = s["string_table"]
    by_codeid = {}
    for e in s["data"]:
        code_id = (e.get("code_id") or "").upper()
        st = sorted(e["symbol_table"], key=lambda r: r["rva"])
        starts = [r["rva"] for r in st]
        ends = [r["rva"] + r.get("size", 0) for r in st]
        names = [strtab[r["symbol"]] for r in st]
        by_codeid[code_id] = (starts, ends, names)
    return by_codeid


def resolve_addr(symtab, addr):
    starts, ends, names = symtab
    i = bisect.bisect_right(starts, addr) - 1
    if i >= 0 and addr < ends[i]:
        return names[i]
    return None


# strip the Rust hash suffix (::h0a1b2c3) and the address-fallback hex names.
_HASH = re.compile(r"::h[0-9a-f]{16}$")


def clean(name):
    return _HASH.sub("", name)


def self_times(profile_path, thread_name=None):
    d = json.load(open(profile_path))
    by_codeid = load_symbols(profile_path)
    libs = d["libs"]
    # code_id per lib index (breakpadId = codeId + age digit; codeId field is exact)
    lib_codeid = [(l.get("codeId") or l.get("breakpadId", "")[:32]).upper() for l in libs]

    threads = d["threads"]
    if thread_name:
        threads = [t for t in threads if t.get("name") == thread_name]
    else:
        threads = [t for t in threads if t.get("isMainThread")] or threads[:1]

    counts = collections.Counter()
    total = 0
    for t in threads:
        S = t["stringArray"]
        ft, frt, stk, smp = t["funcTable"], t["frameTable"], t["stackTable"], t["samples"]
        fres = ft["resource"]
        fname = ft["name"]
        rlib = t["resourceTable"]["lib"]
        frame_func = frt["func"]
        frame_addr = frt["address"]
        stack_frame = stk["frame"]
        weights = smp.get("weight") or [1] * len(smp["stack"])
        for sidx, w in zip(smp["stack"], weights):
            if sidx is None:
                continue
            w = w or 1
            total += w
            fr = stack_frame[sidx]
            fn = frame_func[fr]
            name = None
            if by_codeid is not None:
                res = fres[fn] if fn < len(fres) else None
                addr = frame_addr[fr] if fr < len(frame_addr) else None
                if res is not None and res >= 0 and addr is not None and addr >= 0:
                    li = rlib[res] if res < len(rlib) else -1
                    if 0 <= li < len(lib_codeid):
                        st = by_codeid.get(lib_codeid[li])
                        if st:
                            name = resolve_addr(st, addr)
            if name is None:
                name = S[fname[fn]]  # fallback: whatever the profile stored
            counts[clean(name)] += w
    return counts, total


def fmt_table(counts, total, top, grep=None):
    rx = re.compile(grep) if grep else None
    rows = counts.most_common()
    if rx:
        rows = [(n, c) for n, c in rows if rx.search(n)]
    out = []
    for n, c in rows[:top]:
        out.append(f"  {c:6d} {100*c/total:5.1f}%  {n[:88]}")
    return "\n".join(out)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("profile")
    ap.add_argument("after", nargs="?", help="second profile -> diff mode")
    ap.add_argument("--top", type=int, default=30)
    ap.add_argument("--thread", default="yutani")
    ap.add_argument("--grep", default=None, help="only show functions matching this regex")
    args = ap.parse_args()

    a_counts, a_total = self_times(args.profile, args.thread)
    if not args.after:
        print(f"# {args.profile}  (thread={args.thread}, {a_total} samples)")
        if load_symbols(args.profile) is None:
            print("!! no .syms.json sidecar found — names are raw addresses.", file=sys.stderr)
        print(fmt_table(a_counts, a_total, args.top, args.grep))
        return

    b_counts, b_total = self_times(args.after, args.thread)
    # normalize to percentages so different total sample counts are comparable
    print(f"# DIFF  before={args.profile} ({a_total} smp)  after={args.after} ({b_total} smp)")
    print(f"# thread={args.thread}.  %self before -> after  (Δ percentage points)\n")
    keys = set(a_counts) | set(b_counts)
    rows = []
    for k in keys:
        ap_ = 100 * a_counts.get(k, 0) / a_total if a_total else 0
        bp_ = 100 * b_counts.get(k, 0) / b_total if b_total else 0
        rows.append((bp_ - ap_, ap_, bp_, k))
    rx = re.compile(args.grep) if args.grep else None
    if rx:
        rows = [r for r in rows if rx.search(r[3])]
    # biggest movers (by absolute delta), most-reduced first
    rows.sort(key=lambda r: r[0])
    print("## biggest reductions (after < before)")
    for dlt, ap_, bp_, k in rows[: args.top]:
        if dlt >= 0:
            break
        print(f"  {ap_:5.1f}% -> {bp_:5.1f}%  ({dlt:+5.1f}pp)  {k[:80]}")
    print("\n## biggest increases (after > before)")
    for dlt, ap_, bp_, k in sorted(rows, key=lambda r: -r[0])[: args.top]:
        if dlt <= 0:
            break
        print(f"  {ap_:5.1f}% -> {bp_:5.1f}%  ({dlt:+5.1f}pp)  {k[:80]}")


if __name__ == "__main__":
    main()
