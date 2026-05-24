//! Font loading: resolves the primary family plus its styled cuts and the
//! fallback chain, loading every file in parallel off the main thread.
//! `run()` drives [`load_font_data`] on a worker while the GPU spins up.

use crate::{font, font_loader, Config};

pub(crate) struct FontData {
    pub(crate) primary_name: String,
    pub(crate) primary_data: Vec<u8>,
    /// Bold / Italic / BoldItalic primary cuts that loaded, as
    /// `(variant, bytes, face_index)`. Missing cuts are simply absent.
    pub(crate) styled: Vec<(font::FaceVariant, Vec<u8>, isize)>,
    /// Fallback faces that loaded, as `(label, family, variant, bytes,
    /// face_index)`. Whether each is actually attached is decided on the main
    /// thread (a styled fallback only attaches when its primary cut built).
    pub(crate) fallbacks: Vec<(&'static str, String, font::FaceVariant, Vec<u8>, isize)>,
}

/// Resolve the primary family and load every font file the terminal needs —
/// primary cut, its bold/italic/bold-italic cuts, and the fallback chain — in
/// parallel. Pure data work (Core Text matching + file reads, all thread-safe
/// and `Send`), so `run()` drives it on a worker thread while the GPU spins up
/// on the main thread. Replaces what used to be ~250ms of sequential loading.
pub(crate) fn load_font_data(config: &Config) -> FontData {
    let mut mono_prop = font_loader::system_fonts::FontPropertyBuilder::new()
        .monospace()
        .build();
    let mut mono_fonts = font_loader::system_fonts::query_specific(&mut mono_prop);
    mono_fonts.dedup();
    let installed = font_loader::system_fonts::query_all();

    // User override wins over the built-in preference list (exact name, then
    // substring); a configured-but-missing family warns and falls through.
    let configured_primary = config.font_family.as_deref().and_then(|want| {
        let hit = installed
            .iter()
            .find(|f| f.as_str() == want)
            .or_else(|| installed.iter().find(|f| f.contains(want)))
            .cloned();
        if hit.is_none() {
            eprintln!(
                "font: configured font_family {:?} not installed, falling back to defaults",
                want,
            );
        }
        hit
    });
    let primary_name = configured_primary.unwrap_or_else(|| {
        ["Iosevka Term", "Iosevka", "Fira Code", "Menlo"]
            .iter()
            .find_map(|want| mono_fonts.iter().find(|f| f.as_str() == *want))
            .or_else(|| mono_fonts.iter().find(|f| f.contains("Iosevka Term")))
            .or_else(|| mono_fonts.iter().find(|f| f.contains("Iosevka")))
            .expect("no monospace primary font found")
            .clone()
    });
    println!("primary font: {}", primary_name);

    let styled_specs = [
        (font::FaceVariant::Bold, true, false),
        (font::FaceVariant::Italic, false, true),
        (font::FaceVariant::BoldItalic, true, true),
    ];

    // Fallback chain — first installed family in each category wins. Same list
    // and ordering as before; we only build the *job list* here, then load all
    // jobs in parallel below.
    let fallback_categories: &[(&str, &[&str])] = &[
        ("nerd", &[
            "Iosevka Nerd Font",
            "FiraCode Nerd Font",
            "JetBrainsMono Nerd Font",
            "Hack Nerd Font",
            "Symbols Nerd Font",
        ]),
        ("cjk", &[
            "PingFang SC",
            "Hiragino Sans",
            "Noto Sans CJK SC",
            "Noto Sans CJK JP",
            "Sarasa Mono SC",
        ]),
        ("symbols", &[
            "Apple Symbols",
            "Symbola",
            "Noto Sans Symbols 2",
            "Noto Sans Symbols",
        ]),
        ("emoji", &["Noto Emoji"]),
    ];
    let variants_to_fill = [
        (font::FaceVariant::Regular, false, false),
        (font::FaceVariant::Bold, true, false),
        (font::FaceVariant::Italic, false, true),
        (font::FaceVariant::BoldItalic, true, true),
    ];
    let mut fallback_jobs: Vec<(&'static str, String, font::FaceVariant, bool, bool)> = Vec::new();
    for (label, candidates) in fallback_categories {
        if let Some(family) = pick_family(&installed, candidates) {
            for (variant, bold, italic) in variants_to_fill {
                fallback_jobs.push((label, family.clone(), variant, bold, italic));
            }
        }
    }

    // Load primary, styled cuts, and every fallback file concurrently. Each is
    // an independent Core Text match + file read; fanning them across threads
    // turns the longest single load — not their sum — into the critical path.
    // The scope borrows `primary_name`, so `FontData` is assembled only after
    // the scope ends (all handles joined) and the borrow is released.
    let (primary_data, styled, fallbacks) = std::thread::scope(|s| {
        let primary_h =
            s.spawn(|| load_family(&primary_name).expect("failed to load primary font"));
        let styled_hs: Vec<_> = styled_specs
            .iter()
            .map(|&(v, b, i)| {
                let name = &primary_name;
                (v, s.spawn(move || load_family_styled(name, b, i)))
            })
            .collect();
        let fallback_hs: Vec<_> = fallback_jobs
            .iter()
            .map(|job| {
                let (label, v) = (job.0, job.2);
                let (family, b, i) = (&job.1, job.3, job.4);
                (label, family.clone(), v, s.spawn(move || load_family_styled(family, b, i)))
            })
            .collect();

        let primary_data = primary_h.join().expect("primary font loader panicked");
        let styled: Vec<_> = styled_hs
            .into_iter()
            .filter_map(|(v, h)| h.join().expect("styled loader panicked").map(|(d, i)| (v, d, i)))
            .collect();
        let fallbacks: Vec<_> = fallback_hs
            .into_iter()
            .filter_map(|(label, family, v, h)| {
                h.join()
                    .expect("fallback loader panicked")
                    .map(|(d, i)| (label, family, v, d, i))
            })
            .collect();

        (primary_data, styled, fallbacks)
    });

    FontData { primary_name, primary_data, styled, fallbacks }
}

// Pick the first installed family whose name contains one of the candidate
// substrings, in candidate order. Substring matching is forgiving across
// platform-specific naming variants (e.g. "FiraCode" vs "Fira Code").
pub(crate) fn pick_family(installed: &[String], candidates: &[&str]) -> Option<String> {
    for cand in candidates {
        if let Some(found) = installed.iter().find(|f| f.contains(cand)) {
            return Some(found.clone());
        }
    }
    None
}

pub(crate) fn load_family(family: &str) -> Option<Vec<u8>> {
    let prop = font_loader::system_fonts::FontPropertyBuilder::new()
        .family(family)
        .build();
    font_loader::system_fonts::get(&prop).map(|(data, _)| data)
}

// Variant-aware load. macOS's Core Text matcher silently substitutes the
// regular cut when no bold/italic is installed; `get_strict` rejects that
// substitution by re-checking the matched descriptor's actual traits. Other
// platforms fall back to the trait-tagged builder + plain `get`, which is
// best-effort.
/// Load the bytes of a styled face plus the face index inside its
/// (possibly TTC-packed) file. The index is what the caller must hand
/// to `freetype`'s `new_memory_face` / `harfbuzz_rs::Face::from_bytes`
/// to actually open the right face; passing 0 always lands on the
/// first packed face (usually regular), which is what made italic and
/// bold-italic silently render as regular in earlier revisions.
///
/// On macOS, `get_strict` already verifies against FreeType style
/// flags and returns the correct (file, face_index) tuple. On other
/// platforms we don't have an equivalent strict matcher, so we use
/// `find_face_index` as a best-effort second pass over whatever
/// Core/Fontconfig hand us.
#[cfg(target_os = "macos")]
pub(crate) fn load_family_styled(family: &str, bold: bool, italic: bool) -> Option<(Vec<u8>, isize)> {
    font_loader::system_fonts::get_strict(family, bold, italic)
        .map(|(data, idx)| (data, idx as isize))
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn load_family_styled(family: &str, bold: bool, italic: bool) -> Option<(Vec<u8>, isize)> {
    let mut b = font_loader::system_fonts::FontPropertyBuilder::new().family(family);
    if bold {
        b = b.bold();
    }
    if italic {
        b = b.italic();
    }
    font_loader::system_fonts::get(&b.build()).map(|(data, _)| {
        let idx = font::find_face_index(&data, font::FaceVariant::from_flags(bold, italic));
        (data, idx)
    })
}
