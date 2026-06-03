//! Programming-ligature lookup, populated once at font load.
//!
//! At setup we run HarfBuzz over a battery of candidate sequences
//! (`->`, `=>`, `!=`, …) for each installed face variant, keep the ones
//! whose shaping output differs from the per-char cmap baseline, and
//! cache the substituted glyph ids. At render time the renderer does a
//! plain longest-prefix match against the cache and overrides each
//! covered cell's glyph id — no shaping call on the hot path.
//!
//! Why per-cell substitution instead of wide N→1 ligatures: Fira Code,
//! JetBrains Mono, Iosevka SS04 and most other modern programming
//! fonts implement ligatures via the OpenType `calt` feature using
//! chained-context **alternate** substitution rather than N→1 ligature
//! substitution. `->` shapes to two glyphs — different glyphs from the
//! standalone `-` and `>` — that are designed with side bearings such
//! that they visually fuse when drawn at the cell pitch. Treating that
//! output as "two cells, but with replaced glyph ids" gives the right
//! visual at every cell width and font size.
//!
//! Backend: `harfbuzz_rs` (bindings to the C HarfBuzz library, vendored
//! and built from source). The pure-Rust `rustybuzz` port didn't apply
//! Fira Code's chained-context calt rules at all.
//!
//! The font bytes backing each `harfbuzz_rs::Face` are leaked at
//! install time so the face borrows from a `'static` slice — one-time
//! cost on the order of a few MB per variant, the bytes live for the
//! lifetime of the program anyway.
use crate::font::FaceVariant;
// FxHashMap (aliased to HashMap): `by_first_char` is probed once per cell during
// the per-row ligature scan, keyed by `char`. Constructors use `::default()`.
use rustc_hash::FxHashMap as HashMap;

/// A programming-ligature instance the loaded font produces. `chars` is
/// the input sequence (e.g. `['-', '>']`); `output_glyphs[i]` is the
/// font glyph id that should be rendered in the cell that originally
/// held `chars[i]`. Always `output_glyphs.len() == chars.len()`.
pub struct Ligature {
    pub chars: Vec<char>,
    pub output_glyphs: Vec<u32>,
}

pub struct Shaper {
    fonts: [Option<harfbuzz_rs::Owned<harfbuzz_rs::Font<'static>>>; 4],
    /// Per-variant lookup, bucketed by the ligature's first char so the
    /// renderer can skip directly to viable candidates instead of scanning
    /// every cached entry. Inside each bucket entries are sorted longest
    /// first — `match_at` returns on the first prefix match, which is the
    /// longest available ligature for that starting position.
    by_first_char: [HashMap<char, Vec<Ligature>>; 4],
}

impl Shaper {
    pub fn new() -> Self {
        Self {
            fonts: [None, None, None, None],
            by_first_char: [
                HashMap::default(),
                HashMap::default(),
                HashMap::default(),
                HashMap::default(),
            ],
        }
    }

    pub fn set_variant(
        &mut self,
        variant: FaceVariant,
        data: &[u8],
        face_index: u32,
    ) -> bool {
        let leaked: &'static [u8] = Box::leak(data.to_vec().into_boxed_slice());
        let face = harfbuzz_rs::Face::from_bytes(leaked, face_index);
        let font = harfbuzz_rs::Font::new(face);
        self.fonts[variant as usize] = Some(font);
        true
    }

    /// Run every candidate sequence through HarfBuzz for `variant`,
    /// keeping the ones the font substitutes (any output glyph differs
    /// from its char's bare cmap glyph). Idempotent — call after
    /// `set_variant` for each variant the caller installed. Variants
    /// without an installed face are no-ops.
    pub fn precompute(&mut self, variant: FaceVariant) {
        let font = match &self.fonts[variant as usize] {
            Some(f) => f,
            None => return,
        };
        let mut buckets: HashMap<char, Vec<Ligature>> = HashMap::default();
        for s in CANDIDATES {
            let chars: Vec<char> = s.chars().collect();
            if chars.len() < 2 {
                continue;
            }
            // Shape the candidate. `guess_segment_properties` is required —
            // without it HarfBuzz doesn't infer script/direction and the
            // GSUB pipeline no-ops.
            let buffer = harfbuzz_rs::UnicodeBuffer::new()
                .add_str(s)
                .guess_segment_properties();
            let output = harfbuzz_rs::shape(font, buffer, &[]);
            let infos = output.get_glyph_infos();

            // We only handle 1:1 contextual substitution here (one
            // output glyph per input char). N→1 ligatures (Hasklig
            // style) and 1→N decompositions get skipped — rare in the
            // programming-ligature font landscape and would need their
            // own render path.
            if infos.len() != chars.len() {
                continue;
            }

            // Compare to the per-char cmap baseline. If every output
            // glyph already matches its char's bare glyph, no
            // substitution happened — the font doesn't implement this
            // sequence as a ligature.
            let mut substituted = false;
            for (i, ch) in chars.iter().enumerate() {
                let buf = harfbuzz_rs::UnicodeBuffer::new()
                    .add_str(&ch.to_string())
                    .guess_segment_properties();
                let baseline = harfbuzz_rs::shape(font, buf, &[]);
                let bid = baseline
                    .get_glyph_infos()
                    .first()
                    .map(|g| g.codepoint)
                    .unwrap_or(0);
                if infos[i].codepoint != bid {
                    substituted = true;
                    break;
                }
            }
            if !substituted {
                continue;
            }

            let output_glyphs: Vec<u32> = infos.iter().map(|g| g.codepoint).collect();
            // notdef in any slot would render as tofu — the font is
            // telling us it can't ligate this sequence. Skip.
            if output_glyphs.iter().any(|g| *g == 0) {
                continue;
            }
            let first = chars[0];
            buckets.entry(first).or_default().push(Ligature {
                chars,
                output_glyphs,
            });
        }
        for bucket in buckets.values_mut() {
            // Longest first so prefix matching finds the longest ligature
            // (e.g. `<==>` beats `<==` beats `<=`).
            bucket.sort_by(|a, b| b.chars.len().cmp(&a.chars.len()));
        }
        let total: usize = buckets.values().map(|v| v.len()).sum();
        println!(
            "shaper precompute {:?}: {} ligatures across {} buckets",
            variant,
            total,
            buckets.len()
        );
        self.by_first_char[variant as usize] = buckets;
    }

    /// Longest ligature whose char sequence is a prefix of `cells`, under
    /// `variant`. Falls back to the Regular variant's bucket so styled
    /// faces without their own primary still get ligatures via Regular —
    /// matches `Atlas::lookup`'s fallback so glyphs and shaping stay
    /// consistent.
    pub fn match_at(&self, cells: &[char], variant: FaceVariant) -> Option<&Ligature> {
        if cells.is_empty() {
            return None;
        }
        let first = cells[0];
        let v = variant as usize;
        let bucket = self.by_first_char[v]
            .get(&first)
            .or_else(|| self.by_first_char[FaceVariant::Regular as usize].get(&first))?;
        for lig in bucket {
            if cells.len() >= lig.chars.len() && cells[..lig.chars.len()] == lig.chars[..] {
                return Some(lig);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Build a `Shaper` with hand-populated `by_first_char` buckets, bypassing
    // HarfBuzz/`precompute` (which needs a real font face and so isn't usable
    // in a unit test). Exercises `match_at`'s pure lookup logic — and, because
    // `by_first_char` is one of the caches swapped to `FxHashMap` in this PR,
    // confirms insert+lookup round-trips identically under the new hasher.
    // Each `(first, sequences)` becomes a bucket sorted longest-first, mirroring
    // what `precompute` produces.
    fn shaper_with(variant: FaceVariant, entries: &[&str]) -> Shaper {
        let mut s = Shaper::new();
        let mut buckets: HashMap<char, Vec<Ligature>> = HashMap::default();
        for seq in entries {
            let chars: Vec<char> = seq.chars().collect();
            let first = chars[0];
            // Glyph ids are arbitrary here — `match_at` never inspects them, it
            // only matches on `chars`. Use the char count as a stand-in.
            let output_glyphs = vec![chars.len() as u32; chars.len()];
            buckets
                .entry(first)
                .or_default()
                .push(Ligature { chars, output_glyphs });
        }
        for bucket in buckets.values_mut() {
            bucket.sort_by(|a, b| b.chars.len().cmp(&a.chars.len()));
        }
        s.by_first_char[variant as usize] = buckets;
        s
    }

    #[test]
    fn match_at_returns_none_on_empty_cells() {
        let s = shaper_with(FaceVariant::Regular, &["->"]);
        assert!(s.match_at(&[], FaceVariant::Regular).is_none());
    }

    #[test]
    fn match_at_finds_a_prefix_match() {
        let s = shaper_with(FaceVariant::Regular, &["->"]);
        // Trailing cells beyond the ligature are ignored — it's a *prefix* match.
        let lig = s
            .match_at(&['-', '>', 'x'], FaceVariant::Regular)
            .expect("`->` should match");
        assert_eq!(lig.chars, vec!['-', '>']);
    }

    #[test]
    fn match_at_returns_none_when_first_char_has_no_bucket() {
        let s = shaper_with(FaceVariant::Regular, &["->"]);
        assert!(s.match_at(&['=', '>'], FaceVariant::Regular).is_none());
    }

    #[test]
    fn match_at_returns_none_when_bucket_exists_but_sequence_differs() {
        // Same first char, but the cached ligature's tail doesn't match.
        let s = shaper_with(FaceVariant::Regular, &["->"]);
        assert!(s.match_at(&['-', '<'], FaceVariant::Regular).is_none());
    }

    #[test]
    fn match_at_does_not_match_when_cells_shorter_than_ligature() {
        let s = shaper_with(FaceVariant::Regular, &["->"]);
        // Only one cell available; the ligature needs two.
        assert!(s.match_at(&['-'], FaceVariant::Regular).is_none());
    }

    #[test]
    fn match_at_prefers_longest_ligature_in_a_shared_bucket() {
        // `<==>`, `<==`, `<=` all start with `<`; longest-first ordering means
        // the longest viable prefix wins.
        let s = shaper_with(FaceVariant::Regular, &["<=", "<==", "<==>"]);
        let lig = s
            .match_at(&['<', '=', '=', '>'], FaceVariant::Regular)
            .expect("`<==>` should match");
        assert_eq!(lig.chars, vec!['<', '=', '=', '>']);

        // With only `<==` worth of cells, the next-longest viable prefix wins.
        let lig = s
            .match_at(&['<', '=', '='], FaceVariant::Regular)
            .expect("`<==` should match");
        assert_eq!(lig.chars, vec!['<', '=', '=']);

        // And the shortest when that's all that fits.
        let lig = s
            .match_at(&['<', '=', 'x'], FaceVariant::Regular)
            .expect("`<=` should match");
        assert_eq!(lig.chars, vec!['<', '=']);
    }

    #[test]
    fn match_at_falls_back_to_regular_bucket_for_styled_variant() {
        // A styled variant with no ligatures of its own resolves via Regular,
        // matching `Atlas::lookup`'s styled->regular fallback.
        let s = shaper_with(FaceVariant::Regular, &["->"]);
        let lig = s
            .match_at(&['-', '>'], FaceVariant::Bold)
            .expect("Bold should fall back to Regular's `->`");
        assert_eq!(lig.chars, vec!['-', '>']);
    }

    #[test]
    fn match_at_prefers_own_variant_bucket_over_regular_fallback() {
        // The styled variant has its own bucket for this first char, so the
        // Regular fallback is not consulted (the `.get(...).or_else(...)` short-
        // circuits on the first hit).
        let mut s = shaper_with(FaceVariant::Regular, &["->"]);
        // Give Bold its own, deliberately different ligature under the same
        // first char.
        let mut bold = shaper_with(FaceVariant::Bold, &["-<"]);
        s.by_first_char[FaceVariant::Bold as usize] =
            std::mem::take(&mut bold.by_first_char[FaceVariant::Bold as usize]);

        // `-<` matches Bold's own bucket.
        let lig = s
            .match_at(&['-', '<'], FaceVariant::Bold)
            .expect("Bold's own `-<` should match");
        assert_eq!(lig.chars, vec!['-', '<']);
        // `->` is only in Regular, but since Bold's bucket for `-` exists and
        // has no `->`, the Regular fallback is skipped -> no match.
        assert!(s.match_at(&['-', '>'], FaceVariant::Bold).is_none());
    }
}

/// Candidate ligature sequences. Pulled from the union of FiraCode,
/// JetBrains Mono, Hasklig, Iosevka SS04/SS05, and Cascadia Code. We
/// don't need to be precise here — sequences the loaded font doesn't
/// support get filtered out by `precompute`. Order doesn't matter; the
/// per-bucket sort by length picks the right match at render time.
const CANDIDATES: &[&str] = &[
    // Arrows
    "->", "<-", "<->", "-->", "<--", "<-->", "<-<", ">->", "<<-", "->>",
    "->>>", "<<<-", "-<", ">-", "-<<", ">>-",
    "=>", "<=", "==>", "<==", "<==>", "=>>", "<<=", ">>=", "<=>", "=<<",
    "<<<<", ">>>>", "<<<", ">>>", "<<", ">>",
    "~>", "<~", "<~>", "~~>", "<~~", "~~", "~~~",
    // Comparison / equality
    "==", "!=", "===", "!==", "<=", ">=", "=/=", "/=",
    // Logic / bitwise
    "&&", "||", "&&&", "|||",
    // Increment / decrement / multiplication
    "++", "--", "+++", "---", "**", "***",
    // Compound assignment
    "+=", "-=", "*=", "/=", "%=", "|=", "&=", "^=",
    // Pipes / monad operators
    "|>", "<|", "<|>", "<$", "$>", "<$>", "<*", "*>", "<*>", "<+", "+>",
    "<+>", "<>", "<*-*>",
    // Type / namespace
    "::", ":::", ":=", "::=", "<:", ":>",
    // Comment / closure markers
    "//", "///", "//=", "/*", "*/", "/**",
    "##", "###", "####", "#!", "#=", "#_",
    // Maybe / option
    "??", "?.", "?:",
    // Range / spread
    "..", "...", "..=", "..<",
    // Tildes
    "~=", "~@", "~-",
    // Misc
    "_|_", "<>", "<%", "%>", "@<", "@>", "</>", "</", "/>",
];
