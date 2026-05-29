//! Color schemes shipped inside the binary so a fresh install always has a few
//! themes to offer — during first-run onboarding and in the command palette's
//! theme picker — before the user has authored any of their own.
//!
//! The `.toml` sources live in `src/schemes/` and are embedded at compile time,
//! so the bundle is self-contained: no external files to install alongside the
//! binary. [`seed`] writes any that are missing into the user's
//! `~/.config/yutani/schemes/` directory on startup, which is exactly where
//! both onboarding's `discover_schemes` and the palette's `list_scheme_names`
//! read from — so the rest of the theme machinery needs no special-casing for
//! built-ins.

/// `(name, toml)` for every built-in scheme. The name is the file stem under
/// `~/.config/yutani/schemes/` (and the value stored in `color_scheme`), so it
/// must match what `scheme_path` expects. Kept alphabetical to match the sorted
/// order the pickers display.
pub(crate) const BUILTIN_SCHEMES: &[(&str, &str)] = &[
    ("dracula", include_str!("schemes/dracula.toml")),
    ("gruvbox", include_str!("schemes/gruvbox.toml")),
    ("nostromo", include_str!("schemes/nostromo.toml")),
    ("spacedust", include_str!("schemes/spacedust.toml")),
    ("yutani", include_str!("schemes/yutani.toml")),
];

/// Write any built-in scheme that isn't already present into
/// `~/.config/yutani/schemes/`. Existing files are left untouched, so a user's
/// hand-edits to a same-named scheme are never clobbered — and a scheme they
/// genuinely customized stays theirs. Creating the directory and each write are
/// best-effort: a failure just means that theme won't be offered this run, which
/// is the safe direction to fail (onboarding/palette simply show fewer choices).
///
/// Run once in the parent process at startup, before the PTY child is forked for
/// onboarding, so the files exist by the time `discover_schemes` reads them.
pub(crate) fn seed() {
    let Some(dir) = crate::config_dir().map(|d| d.join("schemes")) else {
        return;
    };
    seed_into(&dir);
}

/// Seed the built-ins into a specific schemes directory. Split out from [`seed`]
/// so it can be exercised against a temp dir without touching `$HOME`. Creates
/// the directory and skips any file that already exists; every step is
/// best-effort.
fn seed_into(dir: &std::path::Path) {
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    for (name, body) in BUILTIN_SCHEMES {
        let path = dir.join(format!("{name}.toml"));
        if !path.exists() {
            let _ = std::fs::write(&path, body);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every embedded scheme parses cleanly — guards against a malformed
    /// `src/schemes/*.toml` shipping in the binary, which the pickers would
    /// silently fall back to defaults for.
    #[test]
    fn builtins_parse() {
        for (name, body) in BUILTIN_SCHEMES {
            // parse_toml is infallible (unknown/garbage keys are skipped), so
            // assert it produces a distinct, non-default-only result: at minimum
            // every built-in sets a background different from one another isn't
            // guaranteed, but each must at least round-trip without panicking.
            let _ = crate::palette::parse_toml(body);
            assert!(!body.trim().is_empty(), "{name} embedded empty");
        }
    }

    /// Each built-in must override at least the background away from the
    /// all-white `Palette::defaults`. A scheme that parsed to the bare defaults
    /// would be indistinguishable from "no scheme," which means its `*.toml`
    /// shipped empty or with only keys `apply` silently dropped — exactly the
    /// failure `builtins_parse`'s infallible parse can't catch on its own.
    #[test]
    fn builtins_differ_from_defaults() {
        let defaults = crate::palette::Palette::defaults();
        for (name, body) in BUILTIN_SCHEMES {
            let p = crate::palette::parse_toml(body);
            assert_ne!(p, defaults, "{name} parsed to the bare defaults");
            // Every shipped scheme is dark, so the background must move off the
            // default white — a sharper signal than mere inequality.
            assert_ne!(
                p.background, defaults.background,
                "{name} left the default background"
            );
        }
    }

    /// Names double as both the `color_scheme` config value and the file stem
    /// under `~/.config/yutani/schemes/`, so duplicates would mean two entries
    /// race to own the same `<name>.toml` (and the picker would list a dupe).
    #[test]
    fn builtin_names_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for (name, _) in BUILTIN_SCHEMES {
            assert!(seen.insert(*name), "duplicate built-in name {name}");
        }
    }

    /// `seed_into` / `scheme_path` both build the path as `dir.join("{name}.toml")`,
    /// so a name containing a path separator (or `.`, `..`, empty) would escape
    /// the schemes dir or collide on the stem. Guard the stem invariant directly.
    #[test]
    fn builtin_names_are_safe_file_stems() {
        for (name, _) in BUILTIN_SCHEMES {
            assert!(!name.is_empty(), "empty built-in name");
            assert!(*name != "." && *name != "..", "{name} is a dir traversal stem");
            assert!(
                !name.contains('/') && !name.contains('\\'),
                "{name} contains a path separator"
            );
            assert!(!name.contains('.'), "{name} contains a dot");
            // A name must round-trip through the same `{name}.toml` join that
            // both seeding and scheme_path use, yielding a single file directly
            // inside the target directory (no nested components).
            let joined = std::path::Path::new("schemes").join(format!("{name}.toml"));
            assert_eq!(
                joined.parent(),
                Some(std::path::Path::new("schemes")),
                "{name} does not land directly in the schemes dir"
            );
            assert_eq!(
                joined.file_name().and_then(|s| s.to_str()),
                Some(format!("{name}.toml").as_str()),
                "{name} stem does not round-trip"
            );
        }
    }

    /// The doc comment promises the list stays alphabetical (the order the
    /// pickers display). Lock that in so an out-of-order insertion is caught.
    #[test]
    fn builtin_names_are_sorted() {
        let names: Vec<&str> = BUILTIN_SCHEMES.iter().map(|(n, _)| *n).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted, "BUILTIN_SCHEMES is not alphabetical");
    }

    #[test]
    fn seeds_missing_then_preserves_edits() {
        let tmp = unique_tmp("preserves-edits");
        let _ = std::fs::remove_dir_all(&tmp);
        let dir = tmp.join("schemes");

        // First seed populates an empty (here, nonexistent) directory.
        seed_into(&dir);
        for (name, _) in BUILTIN_SCHEMES {
            assert!(dir.join(format!("{name}.toml")).exists(), "{name} not seeded");
        }

        // A user edit to a seeded file must survive a re-seed (idempotent,
        // never clobbers). Pick the first built-in and rewrite it.
        let edited = dir.join(format!("{}.toml", BUILTIN_SCHEMES[0].0));
        std::fs::write(&edited, "background = 0x010203\n").unwrap();
        seed_into(&dir);
        assert_eq!(std::fs::read_to_string(&edited).unwrap(), "background = 0x010203\n");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Seeding into a directory that already exists and holds only *some* of the
    /// built-ins must fill in the gaps while leaving the present ones byte-for-byte
    /// untouched — the partial-population case that sits between "empty dir" and
    /// "user edited a file."
    #[test]
    fn seeds_only_missing_in_partial_dir() {
        let tmp = unique_tmp("partial");
        let _ = std::fs::remove_dir_all(&tmp);
        let dir = tmp.join("schemes");
        std::fs::create_dir_all(&dir).unwrap();

        // Pre-place the *last* built-in with sentinel content the seeder must
        // not overwrite, leaving the rest absent.
        let (kept_name, _) = BUILTIN_SCHEMES[BUILTIN_SCHEMES.len() - 1];
        let kept = dir.join(format!("{kept_name}.toml"));
        let sentinel = "# user owned\nbackground = 0x111213\n";
        std::fs::write(&kept, sentinel).unwrap();

        seed_into(&dir);

        // The pre-existing file is preserved verbatim...
        assert_eq!(std::fs::read_to_string(&kept).unwrap(), sentinel, "{kept_name} clobbered");
        // ...and every other built-in was written with its embedded body.
        for (name, body) in BUILTIN_SCHEMES {
            if *name == kept_name {
                continue;
            }
            let path = dir.join(format!("{name}.toml"));
            assert_eq!(std::fs::read_to_string(&path).unwrap(), *body, "{name} body mismatch");
        }

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// A first-time seed into a fresh, nonexistent directory must create the
    /// directory itself and write each file with exactly the embedded TOML.
    #[test]
    fn seeds_create_dir_and_write_embedded_bodies() {
        let tmp = unique_tmp("fresh");
        let _ = std::fs::remove_dir_all(&tmp);
        // Note the nested, nonexistent path — seed_into must create_dir_all it.
        let dir = tmp.join("nested").join("schemes");
        assert!(!dir.exists());

        seed_into(&dir);

        assert!(dir.is_dir(), "schemes dir not created");
        for (name, body) in BUILTIN_SCHEMES {
            let path = dir.join(format!("{name}.toml"));
            assert_eq!(std::fs::read_to_string(&path).unwrap(), *body, "{name} body mismatch");
        }

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Seeding is idempotent: a second pass over an already-fully-seeded dir
    /// changes nothing and adds no stray files.
    #[test]
    fn reseed_is_idempotent() {
        let tmp = unique_tmp("idempotent");
        let _ = std::fs::remove_dir_all(&tmp);
        let dir = tmp.join("schemes");

        seed_into(&dir);
        let after_first: std::collections::BTreeMap<_, _> = read_dir_contents(&dir);
        seed_into(&dir);
        let after_second = read_dir_contents(&dir);

        assert_eq!(after_first, after_second, "re-seed changed the directory");
        assert_eq!(
            after_first.len(),
            BUILTIN_SCHEMES.len(),
            "unexpected file count after seeding"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// A unique, per-test temp path so parallel `cargo test` runs (and the
    /// distinct tests within this module) never collide on the same directory.
    fn unique_tmp(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("yutani-seed-{tag}-{}", std::process::id()))
    }

    /// Map of `file name -> contents` for every file directly in `dir`.
    fn read_dir_contents(dir: &std::path::Path) -> std::collections::BTreeMap<String, String> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|e| {
                let e = e.unwrap();
                let name = e.file_name().to_string_lossy().into_owned();
                let body = std::fs::read_to_string(e.path()).unwrap();
                (name, body)
            })
            .collect()
    }
}
