// Compile the Icon Composer source (yutani.icon) into the artifacts a macOS
// app bundle needs:
//   target/macos-icon/yutani.icns  — flattened fallback for macOS < 26
//   target/macos-icon/Assets.car   — IconKit data (glass/specular/translucency)
//   target/macos-icon/partial.plist — icon keys actool wants merged into Info.plist
//
// `cargo bundle` runs `cargo build` first, so these exist by the time it reads
// `icon = ["target/macos-icon/yutani.icns"]`. scripts/bundle-mac.sh then copies
// Assets.car into the bundle and sets CFBundleIconName (things cargo-bundle
// can't do itself).
//
// This only runs on macOS and only when `actool` (part of a full Xcode install)
// is available; otherwise it warns and skips so plain `cargo build` still works
// on Linux, Windows, or a Command-Line-Tools-only Mac.

use std::path::Path;
use std::process::Command;

fn main() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let icon_src = Path::new(&manifest_dir).join("yutani.icon");

    // Rebuild the icon only when the source changes.
    println!("cargo:rerun-if-changed={}", icon_src.display());
    println!("cargo:rerun-if-changed=build.rs");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    if !icon_src.is_dir() {
        println!("cargo:warning=yutani.icon not found at {}; skipping icon compile", icon_src.display());
        return;
    }

    // `actool` lives inside Xcode.app; on a CLT-only machine the /usr/bin shim
    // exists but errors. Probe it and skip gracefully rather than failing dev builds.
    if !actool_available() {
        println!("cargo:warning=actool unavailable (full Xcode required); skipping .icon compile — `cargo bundle` will not have an app icon");
        return;
    }

    let out_dir = Path::new(&manifest_dir).join("target/macos-icon");
    let _ = std::fs::remove_dir_all(&out_dir);
    std::fs::create_dir_all(&out_dir).expect("create target/macos-icon");

    let status = Command::new("actool")
        .arg(&icon_src)
        .arg("--compile")
        .arg(&out_dir)
        .arg("--platform")
        .arg("macosx")
        .arg("--minimum-deployment-target")
        .arg("14.0")
        .arg("--app-icon")
        .arg("yutani")
        .arg("--output-partial-info-plist")
        .arg(out_dir.join("partial.plist"))
        .arg("--warnings")
        .arg("--errors")
        .arg("--notices")
        .status()
        .expect("failed to spawn actool");

    if !status.success() {
        panic!("actool failed to compile {}", icon_src.display());
    }

    for f in ["yutani.icns", "Assets.car"] {
        assert!(
            out_dir.join(f).exists(),
            "actool did not produce {f} in {}",
            out_dir.display()
        );
    }
}

fn actool_available() -> bool {
    Command::new("actool")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}
