// Self-contained micro-benchmark isolating #208's dominant change: the glyph
// atlas lookup hasher (std SipHash -> FxHash). No GPU, no GUI — just the exact
// hot operation: `HashMap<char, AtlasEntry>::get(&ch)` once per visible cell per
// frame. FxHasher is reimplemented here (it's ~10 lines) so this needs no deps.
use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};
use std::time::Instant;

// rustc-hash's FxHasher (the algorithm #208 adopted), inlined.
#[derive(Default)]
struct FxHasher {
    hash: u64,
}
const K: u64 = 0x51_7c_c1_b7_27_22_0a_95;
impl FxHasher {
    #[inline]
    fn add(&mut self, i: u64) {
        self.hash = (self.hash.rotate_left(5) ^ i).wrapping_mul(K);
    }
}
impl Hasher for FxHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.add(b as u64);
        }
    }
    #[inline]
    fn write_u32(&mut self, i: u32) {
        self.add(i as u64);
    }
    #[inline]
    fn finish(&self) -> u64 {
        self.hash
    }
}
type FxBuild = BuildHasherDefault<FxHasher>;

// Stand-in for AtlasEntry (a handful of u16/f32 atlas coords — Copy).
#[derive(Clone, Copy, Default)]
struct AtlasEntry {
    _xy: [u16; 4],
    _uv: [f32; 4],
}

fn main() {
    // Populate both maps with a realistic resident glyph set: printable ASCII +
    // Latin-1 + a few hundred common BMP glyphs (what a real session caches).
    let mut keys: Vec<char> = Vec::new();
    for c in 0x20u32..0x7f {
        keys.push(char::from_u32(c).unwrap());
    }
    for c in 0xa0u32..0x180 {
        keys.push(char::from_u32(c).unwrap());
    }
    for c in 0x2500u32..0x2600 {
        // box drawing / blocks
        keys.push(char::from_u32(c).unwrap());
    }

    let mut sip: HashMap<char, AtlasEntry> = HashMap::new();
    let mut fx: HashMap<char, AtlasEntry, FxBuild> = HashMap::default();
    for &k in &keys {
        sip.insert(k, AtlasEntry::default());
        fx.insert(k, AtlasEntry::default());
    }

    // A frame's worth of lookups: ~a 200x50 grid = 10k cells, mostly ASCII with
    // a realistic tail, repeated for many frames. Build the access sequence once.
    let cells = 200 * 50;
    let mut access: Vec<char> = Vec::with_capacity(cells);
    let mut s: u64 = 0x1234_5678;
    for _ in 0..cells {
        s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        // 85% ASCII letters/punct, 15% from the wider resident set — mimics code/text.
        let k = if (s >> 60) % 100 < 85 {
            keys[(s as usize >> 7) % 95] // first 95 are ASCII
        } else {
            keys[(s as usize >> 7) % keys.len()]
        };
        access.push(k);
    }

    let frames = 2000; // ~33s of rendering at 60fps, compressed into a tight loop
    let mut sink: u64 = 0;

    // Warm caches.
    for &k in &access {
        if sip.get(&k).is_some() {
            sink += 1;
        }
        if fx.get(&k).is_some() {
            sink += 1;
        }
    }

    let t = Instant::now();
    for _ in 0..frames {
        for &k in &access {
            if let Some(e) = sip.get(&k) {
                sink = sink.wrapping_add(e._xy[0] as u64 + 1);
            }
        }
    }
    let sip_t = t.elapsed();

    let t = Instant::now();
    for _ in 0..frames {
        for &k in &access {
            if let Some(e) = fx.get(&k) {
                sink = sink.wrapping_add(e._xy[0] as u64 + 1);
            }
        }
    }
    let fx_t = t.elapsed();

    let total = (frames * cells) as f64;
    let sip_ns = sip_t.as_secs_f64() * 1e9 / total;
    let fx_ns = fx_t.as_secs_f64() * 1e9 / total;
    println!("lookups: {} ({} frames x {} cells)", frames * cells, frames, cells);
    println!("  SipHash (std default): {:5.2} ns/lookup   frame={:6.1} us", sip_ns, sip_ns * cells as f64 / 1e3);
    println!("  FxHash  (#208):        {:5.2} ns/lookup   frame={:6.1} us", fx_ns, fx_ns * cells as f64 / 1e3);
    println!("  speedup: {:.2}x   (per-frame lookup time saved: {:.1} us)", sip_ns / fx_ns, (sip_ns - fx_ns) * cells as f64 / 1e3);
    println!("(sink={})", sink);
}
