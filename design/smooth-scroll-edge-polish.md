# Smooth Alt-Screen Scroll — Toolbar Edge Polish (deferred)

## Status

Deferred. The smooth alt-screen scroll animation (DECSET 1007 + retained-rows
slide, see `src/terminal.rs` `take_alt_scroll`/`alt_anim_departing` and
`src/main.rs` `AltScrollAnim`/`update_alt_scroll`) ships with a minor cosmetic
artifact at the **top** edge during **upward** scrolls. This doc records two
ways to polish it, for later pickup. Downward scrolls exit at the bottom (no
toolbar) and don't show the artifact.

## The artifact

During an upward scroll the departing top row slides up and parks one line
above the viewport — i.e. **behind the translucent toolbar**, where it stays
faintly visible — and is then cleared the instant the slide ends. The clear is
abrupt, so the faint row "pops" out of existence rather than dissolving.

### Why it's not trivial

The slide applies a single shared vertical offset (`scroll_y`) to the whole
grid. The live content must *settle* exactly at its final position
(`scroll_y == 0`), but the departing row wants to keep travelling up and out
behind the toolbar. One shared offset can't satisfy both, so the departing row
is stranded half-behind the toolbar at rest.

A first attempt — re-engaging the existing top edge-fade (`top_fade_phase`,
the dual-Kawase blur strip in `update_vertices`) during the slide — made it
*worse*: the fade phase ramps over `top_fade_anim_secs`, which is far longer
than the ~70 ms (`ALT_SCROLL_ANIM_SECS`) slide, so the blur never visibly
engaged before the slide (and the row) were gone. That change was reverted.

## Option A — Frosted peek (retain + masked clear)

Keep the departing row drawn behind the toolbar with the blur band held **on**,
then clear it *while it is still fully blurred* so the removal is hidden.

Sketch:
- Don't clear `alt_anim_departing` when the positional slide finishes. Add a
  short hold phase (~120–150 ms) after the slide.
- During the hold, drive `top_fade_phase` directly to `1.0` (set it, don't
  ramp via `advance()` — the ramp is what was too slow) so the retained
  departing rows behind the toolbar are blurred.
- At the end of the hold, clear the departing rows *while the band is at full
  blur*, then let the band retract to `0` over its normal duration. Because the
  region behind the band is now empty, the retract reveals nothing — the clear
  was masked by the blur. No sharp pop.

Pros: reuses the existing blur strip and glyph-fade uniform; contained to
`main.rs` animation state + the alt-screen branch of `edge_fade_dists`.
Cons: introduces a two-phase (slide → hold) state machine; the row lingers
~150 ms as a frosted hint. Need to confirm the held band doesn't visibly dim
the live top row, and decide behavior when the band is held but the app
repaints underneath.

## Option B — Decoupled dissolve (separate departing track)

Render the departing rows on their **own** offset/alpha track, independent of
the settled grid, so they keep sliding up through the blur band and fade out
on their own timeline.

Sketch:
- Give the departing band its own offset that continues past `scroll_y == 0`
  (keeps moving up) and its own alpha that ramps to 0, while the live grid
  settles normally.
- Requires the emit loop to treat the departing phantom rows (negative visual
  rows during an up-scroll) as a distinct group: a per-row vertical offset and
  a per-row alpha applied to bg/glyph/underline quads. The loop currently bakes
  one global `scroll_y` into ~6 sites, so this means threading a per-row
  offset/alpha through them (or a small dedicated overlay pass for the
  departing rows).

Pros: most polished result — the departing row truly dissolves up and away,
matching scrollback's "content flows behind the bar" feel.
Cons: most work and most risk; touches the cell render hot path. A dedicated
overlay pass for just the departing rows may be cleaner than per-row offsets in
the main loop.

## Related: sub-region downward-scroll departing rows

A second, separate rough edge lives in the sub-region path (vim et al., which
reserve a status line). For a *downward* scroll within a region whose bottom is
above the last row, the departing bottom row exits into the grid index occupied
by the static status line, so `extended_cell` can't serve it (the real status
row shadows that index). The result is a brief ≤d-row gap at the bottom of the
text area at the *start* of the slide, shrinking to zero as it settles. In
practice (vim, d=1, ~70 ms) this tested as not noticeable. A dedicated
departing-row overlay (Option B above) would also resolve this, since it draws
the departing rows independently of the grid-index lookup.

## Recommendation

If picked up, try **Option A** first — it's far cheaper and likely "good
enough." Fall back to **Option B** only if the frosted-peek hold reads as
laggy or the held band dims the live content unacceptably.
