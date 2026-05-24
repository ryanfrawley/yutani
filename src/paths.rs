//! Filesystem locations and scheme/onboarding bookkeeping: config + state
//! directories, color-scheme install/listing, the first-run onboarding
//! marker, and the theme-picker label <-> scheme-name mapping.

use crate::palette;

pub(crate) fn config_dir() -> Option<std::path::PathBuf> {
    let home = std::env::var_os("HOME")?;
    let mut p = std::path::PathBuf::from(home);
    p.push(".config");
    p.push("yutani");
    Some(p)
}

pub(crate) fn config_path() -> Option<std::path::PathBuf> {
    let mut p = config_dir()?;
    p.push("config.toml");
    Some(p)
}

/// Read the named color scheme from disk and install it as the live
/// palette. `None` (or any read failure) reverts to the built-in defaults
/// so a config edit that *removes* `color_scheme` actually goes back to
/// neutral, not back to whatever was last loaded.
pub(crate) fn install_color_scheme(name: Option<&str>) {
    let palette_for = |name: &str| -> Option<palette::Palette> {
        let path = scheme_path(name)?;
        match std::fs::read_to_string(&path) {
            Ok(src) => Some(palette::parse_toml(&src)),
            Err(e) => {
                eprintln!("palette: failed to read {}: {}", path.display(), e);
                None
            }
        }
    };
    let p = name.and_then(palette_for).unwrap_or(palette::Palette::defaults());
    palette::install(p);
}

/// Resolve a scheme name to its on-disk `.toml` path under
/// `~/.config/yutani/schemes/`.
pub(crate) fn scheme_path(name: &str) -> Option<std::path::PathBuf> {
    let mut dir = config_dir()?;
    dir.push("schemes");
    Some(dir.join(format!("{}.toml", name)))
}

/// `~/.local/state/yutani` — XDG_STATE_HOME for persistent-but-disposable
/// state. Kept deliberately separate from `~/.config/yutani`: config is the
/// user's to hand-edit, delete, or version-control, and none of that should
/// silently re-arm or suppress first-run onboarding. Honors `$XDG_STATE_HOME`
/// when set, falling back to `$HOME/.local/state`.
pub(crate) fn state_dir() -> Option<std::path::PathBuf> {
    let mut p = match std::env::var_os("XDG_STATE_HOME") {
        Some(x) if !x.is_empty() => std::path::PathBuf::from(x),
        _ => {
            let home = std::env::var_os("HOME")?;
            let mut p = std::path::PathBuf::from(home);
            p.push(".local");
            p.push("state");
            p
        }
    };
    p.push("yutani");
    Some(p)
}

/// Marker file recording that first-run onboarding has completed. Its contents
/// are the onboarding *revision* that ran (a bare integer), so a future Yutani
/// can re-introduce setup for a new feature by bumping [`ONBOARD_REVISION`]
/// without re-onboarding users who are already current.
pub(crate) fn onboarding_marker() -> Option<std::path::PathBuf> {
    Some(state_dir()?.join("onboarded"))
}

/// Current onboarding revision. First-run fires when the marker is missing or
/// records a lower number; bump this when onboarding gains a step worth
/// re-showing to existing users.
pub(crate) const ONBOARD_REVISION: u32 = 1;

/// Read the onboarding revision recorded on disk, if any. `None` means setup
/// has never completed (or the marker is unreadable / malformed — both treated
/// as "not yet onboarded", erring toward showing setup rather than skipping it).
pub(crate) fn onboarded_revision() -> Option<u32> {
    let path = onboarding_marker()?;
    let s = std::fs::read_to_string(path).ok()?;
    s.trim().parse().ok()
}

/// Whether first-run onboarding should run: never completed, or completed at an
/// older revision than we ship now.
pub(crate) fn needs_onboarding() -> bool {
    onboarded_revision().map_or(true, |r| r < ONBOARD_REVISION)
}

/// Stamp the marker with the current revision, creating `state_dir()` as
/// needed. Best-effort: a write failure just means onboarding runs again next
/// launch, which is the safe direction to fail.
pub(crate) fn mark_onboarded() {
    let Some(path) = onboarding_marker() else { return };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, ONBOARD_REVISION.to_string());
}

/// Lower the onboarding marker so first-run fires on the next launch. Used by
/// the "Run first-time setup…" command. Best-effort.
pub(crate) fn rearm_onboarding() {
    if let Some(path) = onboarding_marker() {
        let _ = std::fs::remove_file(path);
    }
}

/// The names of every color scheme available under `~/.config/yutani/schemes/`,
/// i.e. the file stems of the `.toml` files there, sorted alphabetically. These
/// are exactly the names `scheme_path` / `install_color_scheme` accept, so the
/// command palette's theme picker can only offer schemes that actually load.
/// A missing or unreadable directory yields an empty list.
pub(crate) fn list_scheme_names() -> Vec<String> {
    let mut dir = match config_dir() {
        Some(d) => d,
        None => return Vec::new(),
    };
    dir.push("schemes");
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut names: Vec<String> = entries
        .filter_map(|e| {
            let path = e.ok()?.path();
            if path.extension().and_then(|x| x.to_str()) != Some("toml") {
                return None;
            }
            path.file_stem()?.to_str().map(|s| s.to_string())
        })
        .collect();
    names.sort();
    names
}

/// Label for the synthetic theme-picker entry that reverts to the built-in
/// defaults (i.e. clears `color_scheme`). It carries spaces and parens so it
/// can't collide with a real `.toml` file stem; any on-disk scheme that somehow
/// matched it is filtered out (see [`theme_picker_choices`]).
pub(crate) const DEFAULT_THEME_LABEL: &str = "Default (built-in)";

/// The theme picker's full candidate list: the synthetic "Default (built-in)"
/// entry first, then the real schemes (with any collision against the label
/// dropped so the entry is unambiguous).
pub(crate) fn theme_picker_choices(scheme_names: Vec<String>) -> Vec<String> {
    let mut v = Vec::with_capacity(scheme_names.len() + 1);
    v.push(DEFAULT_THEME_LABEL.to_string());
    v.extend(scheme_names.into_iter().filter(|n| n != DEFAULT_THEME_LABEL));
    v
}

/// Resolve a theme-picker selection to the scheme name to install: `None` for
/// the synthetic default entry (revert to built-in defaults), otherwise the
/// label *is* the scheme name.
pub(crate) fn scheme_for_pick(label: &str) -> Option<&str> {
    (label != DEFAULT_THEME_LABEL).then_some(label)
}

/// Convert a theme-picker selection into the value to store in a config scheme
/// slot (`color_scheme` / `light_scheme` / `dark_scheme`). The synthetic
/// "Default (built-in)" entry and an empty/whitespace pick both map to `None`
/// (revert to defaults); any other label becomes the scheme name.
pub(crate) fn scheme_value_from_pick(arg: Option<String>) -> Option<String> {
    let arg = arg.unwrap_or_default();
    match scheme_for_pick(arg.trim()) {
        Some(name) if !name.is_empty() => Some(name.to_string()),
        _ => None,
    }
}
