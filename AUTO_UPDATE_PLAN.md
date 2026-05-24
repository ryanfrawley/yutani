# Auto-update for Yutani

## Context

Yutani ships as a hand-built macOS `.app` (via `scripts/bundle-mac.sh`) with **no
release pipeline, no version surfacing, no network code, and no code signing**.
There is no way for an installed copy to learn that a newer build exists, let
alone install one — every upgrade is a manual rebuild. This adds a background
update **checker** plus a user-confirmed **installer**, gated by a config
toggle so users can opt out entirely.

Decisions locked in with the user:
- **Install mode:** check in the background, *notify and require confirmation*
  before downloading/swapping (no silent auto-install).
- **Update source:** abstract behind one `update_feed_url` config knob so the
  concrete host (Forgejo Releases API vs. a plain static URL) can be chosen
  during implementation. Caveat to resolve then: `git.frawley.co` HTTPS needs a
  keychain client cert per `CLAUDE.md` — if that also gates release downloads,
  use a cert-free static URL for the feed.
- **Code signing:** add `codesign` + `notarytool` steps to the bundle/release
  flow as part of this work; the updater verifies the downloaded build's
  signature before swapping.

## Design overview

```
startup ──(if auto_update)──▶ spawn update-checker thread
                                  │  GET update_feed_url  → manifest JSON
                                  │  compare versions (semver)
                                  ▼
        EventLoopProxy.send_event(CustomEvent::UpdateAvailable(info))
                                  │
                                  ▼
        per-window overlay: "vX.Y.Z available — Install / Later / Skip"
                                  │ user picks Install
                                  ▼
        spawn installer thread → download .tar.gz → verify sha256
              → verify codesign/spctl → swap .app → relaunch + exit
```

Mirrors the app's existing patterns: PTY reader threads already do
`std::thread::spawn` + `EventLoopProxy::send_event` (`src/main.rs:7795`), and
the Command Palette / Search overlays (`src/command_palette.rs`, `src/search.rs`)
are the model for a non-modal confirmation UI. No tokio — blocking `ureq` in a
worker thread fits the codebase's `std::thread` philosophy.

## Manifest format

A small JSON document at `update_feed_url`:

```json
{
  "version": "0.2.0",
  "url": "https://.../Yutani-0.2.0.tar.gz",
  "sha256": "<hex>",
  "notes": "Optional one-line summary",
  "min_macos": "13.0"
}
```

## Implementation

### 1. Version & dependency plumbing
- Add a `const VERSION: &str = env!("CARGO_PKG_VERSION");` near the top of
  `src/main.rs` (today the version lives *only* in `Cargo.toml:5`). Wire a
  `--version` CLI flag in `main()` (`src/main.rs:8700`, alongside the existing
  `--onboard` branch) so the running version is inspectable.
- Add to `Cargo.toml` `[dependencies]`: `ureq` (blocking HTTP; pick TLS backend
  during impl — `rustls` if the feed is cert-free, `native-tls` if the keychain
  client cert is needed) and `serde` + `serde_json` for manifest parsing.
- New module `src/update.rs` holding: manifest fetch/parse, the `Version`
  comparator, the checker thread, and the installer thread. Keep all
  update logic out of the already-massive `main.rs`.

### 2. Config knobs (follow the existing field-add pattern)
Edit the four sites in `src/main.rs`: the `Config` struct (`~:462`),
`Config::defaults()` (`~:634`), `Config::apply()` (`~:709`), and
`Config::serialize()` (`~:845`).
- `auto_update: bool` — default `true`. The requested enable/disable toggle;
  when `false`, the checker thread is never spawned.
- `update_feed_url: String` — default to a `const DEFAULT_UPDATE_FEED_URL`.
- (optional) `update_check_interval_hours: u64` — default `24`, clamped to a
  sane minimum, controls re-check cadence.
Document each with a doc-comment like the surrounding fields. `Cmd+Shift+R`
reload already re-reads these via `Config::load()`.

### 3. CustomEvent variants
In `src/app_window.rs:8` extend `CustomEvent`:
```rust
UpdateAvailable(update::UpdateInfo),
UpdateProgress(u8),          // download %, for the overlay
UpdateReady,                 // staged + verified; ok to relaunch
UpdateFailed(String),
```
Handle these in the event-loop `Event::UserEvent` match (`src/main.rs:8134+`),
routing to the focused window's overlay state.

### 4. Checker thread
A `update::spawn_checker(proxy, config)` called once in `run()` just after the
event loop/proxy exist (`src/main.rs:~7951`), only if `config.auto_update`:
- initial short delay, then GET the manifest, parse, compare `manifest.version`
  vs `VERSION` using the semver comparator, honor a persisted "skipped version"
  marker, and on a genuine newer version `send_event(UpdateAvailable(info))`.
- loop with `sleep(interval)` for periodic re-checks. All failures are logged
  and swallowed (never crash the terminal over an update check).

### 5. Notification overlay
New `update_notice: Option<UpdateNotice>` field on `WindowState`
(`src/main.rs:~1542`), rendered in the frame draw after the grid like the
completion popup, and given a turn at keyboard input before the terminal when
present. Actions: **Install** (→ start installer), **Later** (dismiss for this
session), **Skip this version** (persist version to a marker in `state_dir()`
so it never re-notifies for that build). Reuse `state_dir()`/marker conventions
from `src/main.rs:101-152`.

### 6. Installer thread
`update::spawn_install(proxy, info)`:
1. Download `info.url` to `state_dir()/updates/Yutani-<ver>.tar.gz`, emitting
   `UpdateProgress`.
2. Verify SHA-256 against `info.sha256` (use existing `flate2` for the `.tar.gz`
   if we ship gzipped-tar; otherwise add a minimal tar step — decide format with
   the release script in §7).
3. Extract to a staging dir; verify integrity with `codesign --verify
   --deep --strict` and `spctl --assess --type execute` on the extracted
   `Yutani.app`. Abort on failure (`UpdateFailed`).
4. Locate the *current* bundle by walking up from `std::env::current_exe()` to
   the enclosing `.app`. Atomically swap: move current aside, move staged into
   place (fall back to copy across volumes).
5. `send_event(UpdateReady)`. The main thread then relaunches via
   `open -n <bundle>` and exits cleanly (close PTYs first).

### 7. Signing + release script (`scripts/`)
- Extend `scripts/bundle-mac.sh` (or a new `scripts/release-mac.sh` that calls
  it) to: `codesign --deep --options runtime --sign "$SIGN_IDENTITY"` the
  `.app`, submit with `xcrun notarytool submit --wait`, then
  `xcrun stapler staple`. Identity / Apple-ID creds via env vars with clear
  errors if unset (these are a release-time prerequisite, not a build-time one).
- The release script then: tar+gzip the stapled `.app`, compute its SHA-256,
  emit the manifest JSON (§Manifest), and upload artifact + manifest to the
  chosen host (Forgejo Releases API per `CLAUDE.md`'s `curl` recipe, or a static
  path). Bump `Cargo.toml` version + git tag as part of release.

## Files touched
- `src/main.rs` — `VERSION` const, `--version` flag, 4 config sites, `WindowState`
  overlay field + render/input, `CustomEvent` handling, `spawn_checker` call,
  relaunch-on-`UpdateReady`.
- `src/app_window.rs` — `CustomEvent` variants.
- `src/update.rs` — **new**: manifest, version compare, checker + installer.
- `Cargo.toml` — `ureq`, `serde`, `serde_json` deps.
- `scripts/bundle-mac.sh` / `scripts/release-mac.sh` — signing, notarization,
  tarball, sha256, manifest, upload.

## Verification
- **Unit tests** (`unit-test-writer` agent, per CLAUDE.md): `Version` comparison
  (older/newer/equal, malformed), manifest JSON parsing (valid/missing fields),
  and the skipped-version marker round-trip.
- **End-to-end, local:** run a throwaway static server (`python3 -m http.server`)
  serving a manifest whose `version` is higher than `Cargo.toml`'s and a real
  signed tarball; point `update_feed_url` at it; launch Yutani and confirm the
  overlay appears, **Install** downloads + verifies + swaps + relaunches into the
  new build, and `--version` reports the new number.
- **Toggle:** set `auto_update = false`, reload (`Cmd+Shift+R`) / relaunch, and
  confirm no network request is made and no overlay appears.
- **Negative paths:** corrupt the tarball (sha mismatch) and present an unsigned
  build (spctl failure) — both must abort with `UpdateFailed` and leave the
  installed app untouched.
