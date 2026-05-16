# Kitty graphics — follow-up plan

Phase 1 + every K-slice from this plan landed (PRs #32–#37) and the
cursor-ease flake (PR forthcoming) is also fixed. What's below is what's
explicitly deferred — no actionable items remain unless one of these
deferred features starts being requested.

## Done

- **Slice 1** — APC parsing, parser + dispatcher, direct/file
  transmission, capability handshake, XTWINOPS `CSI t` size queries,
  raw RGB/RGBA over direct base64. (#32)
- **K2** — `t=t` temp-file transmission. (#33)
- **K4** — image / placement IDs + `a=t` / `a=p` / `a=d`. (#34)
- **K5** — virtual placements (`U=1`, `U+10EEEE` placeholders). (#35)
- **K3** — `t=s` POSIX shared memory. (#36)
- **K6** — `X`/`Y` pixel offsets, `z` z-index, `x`/`y`/`w`/`h` source
  crops, `o=z` zlib decompression. (#37)
- **Cursor-ease flake fix** — `tests::retarget_rebases_from_to_currently_eased_position`
  no longer depends on an exact `Instant::now()` drift.

## Deferred indefinitely

Pick up if a real user case appears.

### Animation (`a=a`)

Full sub-protocol of its own (frame composition, timing, transparency,
loop semantics). No tool in common use sends animated images yet; the
spec is also still evolving. Implementing requires:

- New `KittyAction::Animation` variant.
- Per-frame `Frame` struct on the store (delay, composition mode,
  per-frame bounds within the parent image).
- Render-loop frame advance keyed on wall-clock — the renderer would
  need to pick a frame index per draw, which complicates
  `Store::peek` (now returns a *frame*, not a fixed image).
- Texture array or atlas to avoid per-frame GPU upload churn.

### 32-bit image IDs via the 3rd diacritic on `U+10EEEE`

Today we encode 24 bits in the placeholder cell's RGB fg color and
ignore any combining diacritic. The Kitty spec puts the high byte
(bits 24..31) on an optional 3rd combining mark drawn after the
placeholder. Doing this requires:

- The ANSI parser's print path to detect a placeholder + accumulate
  the next 0..3 combining marks before emitting the cell.
- A lookup table mapping the 297 specific combining codepoints to
  row/col/high-byte indices (the table is well-defined in the spec
  but tedious to embed).
- `Cell::placeholder_image_id` widens to `Option<u32>` (already u32)
  — fine — plus new `placeholder_row_offset` / `placeholder_col_offset`
  fields if we also want spec-accurate per-cell tile addressing
  rather than the MVP merge-by-bbox approximation.

24-bit IDs cover ~16M concurrent images. No realistic app exceeds
this, so the bits-24..31 byte is dead in practice. Skip until
something needs it.
