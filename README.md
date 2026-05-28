# Yutani

A GPU-accelerated terminal emulator written in Rust, targeting macOS first.
Yutani renders the grid with [wgpu](https://wgpu.rs/) and ships with the
features you'd expect from a modern terminal — truecolor, inline graphics,
clickable links, shell-aware autocomplete, find-in-scrollback, a command
palette — plus a few macOS-native touches like Liquid Glass UI and Touch ID
for `sudo`.

> **Status:** `v0.1.0`, under active development. macOS is the primary and
> best-supported platform; the rendering and font stack also build on
> Windows (GDI) and Linux (fontconfig), but those are less exercised. See
> [`FEATURES.md`](FEATURES.md) for a detailed breakdown of what's done and
> what's still missing.

## Features

- **Full VT/CSI/SGR emulation** — cursor movement, erase/edit, scroll regions,
  left/right margins, truecolor + 256-color + ANSI, alternate screen, and a
  scrollback ring buffer.
- **Inline graphics** — the full Kitty graphics protocol (inline / placement /
  animation / shared memory) and iTerm2 `OSC 1337 File=`.
- **Clickable links** — heuristic `http(s)://` detection over the grid plus
  **OSC 8** explicit hyperlinks; Cmd-hover to underline, Cmd-click to open.
- **Shell integration** (zsh, automatic) — prompt-mark navigation, an
  exit-status gutter, cwd → title, and a filesystem/history/$PATH autocomplete
  popup. See [`shell-integration/README.md`](shell-integration/README.md).
- **Find in scrollback** (Cmd-F) with smart-case matching and wrap-around
  next/prev.
- **Command palette** (Cmd-Shift-P) — fuzzy-searchable actions (themes, zoom,
  config reload, and more).
- **Touch ID for `sudo`** (macOS, opt-in) — see [below](#touch-id-for-sudo-macos).
- **Theming** — TOML config + color schemes, light/dark auto-switching, and
  hot-reload. Optional glow/bloom and CRT scanline effects.
- **First-run onboarding** rendered right in the terminal (font, theme,
  autocomplete, Touch ID).

## Building & running

Yutani is a standard Cargo project. With a recent stable Rust toolchain:

```sh
# Run a debug build
cargo run

# Optimized build
cargo build --release      # binary at target/release/yutani

# Tests
cargo test
```

> **Note on performance:** assess scroll/render performance on a `--release`
> build; debug builds are not representative. Set `PERFLOG=1` for the `[perf]`
> timing log.

### macOS `.app` bundle

To produce a signed, icon-complete `Yutani.app`:

```sh
scripts/bundle-mac.sh                 # release bundle at target/release/bundle/osx/Yutani.app
PROFILE=debug scripts/bundle-mac.sh   # debug bundle
```

This runs `cargo bundle`, installs the layered-icon `Assets.car`, patches
`Info.plist`, and codesigns with a stable identity (override with the
`SIGN_IDENTITY` env var). A full Xcode install is required for `actool` to
compile the icon. Running from a bundle (rather than `cargo run`) is what gives
the app a stable code-signing identity, so macOS permission grants and Touch ID
behave consistently across rebuilds.

## Keybindings

| Shortcut | Action |
|---|---|
| `Cmd-Shift-P` | Open the command palette |
| `Cmd-F` | Find in scrollback |
| `Cmd-C` / `Cmd-V` | Copy / paste |
| `Cmd-N` | New window |
| `Cmd-T` | New native macOS tab |
| `Cmd-Shift-]` / `Cmd-Shift-[` | Cycle tabs |
| `Cmd-1`…`Cmd-8` / `Cmd-9` | Jump to tab _n_ / last tab |
| `Cmd-W` | Close current tab |
| `Cmd-+` / `Cmd-=` / `Cmd--` | Zoom in / out |
| `Cmd-Shift-Up` / `Cmd-Shift-Down` | Jump between prompts (needs shell integration) |
| `Cmd-Shift-O` | Select + copy the last command's output |
| `Cmd-Shift-R` | Reload config |

## Configuration

Config lives at `~/.config/yutani/config.toml`, with color schemes under
`~/.config/yutani/schemes/*.toml`. The file is yours to hand-edit; changes can
be applied live with `Cmd-Shift-R`. Commonly used keys:

- `font_size`, `font_family`
- `color_scheme` — the active scheme when not following the system
- `auto_theme`, `light_scheme`, `dark_scheme` — follow the macOS light/dark
  appearance and pick a scheme per mode
- `autocomplete` — the as-you-type suggestion popup
- glow/bloom and CRT scanline knobs (also set via the onboarding "CRT effect"
  preset)

Most options are also reachable from the command palette (theme pickers, zoom,
follow-system toggle, etc.). Disposable state (the onboarding marker) lives
separately under `~/.local/state/yutani`.

## Touch ID for `sudo` (macOS)

`sudo` authenticates through PAM, so Touch ID becomes a sufficient factor once
an `auth sufficient pam_tid.so` line is added to `/etc/pam.d/sudo_local`. That's
a root-owned file, so there's no fully automatic path — macOS requires you to
authorize the change once. Yutani offers to do it for you:

- From the command palette: **"Touch ID for sudo…"** — an opt-in, reversible
  toggle. It reads the current state from the (world-readable) PAM files and,
  on confirmation, performs the edit with a single administrator authorization
  (whose dialog itself supports Touch ID). The same command turns it back off.
- During first-run onboarding, when it can actually be enabled.

Yutani only ever edits `/etc/pam.d/sudo_local` (the upgrade-safe location),
never `/etc/pam.d/sudo`.

**Caveats:** this does not work over SSH (no GUI session) or inside
`tmux`/`screen` (the multiplexer server detaches from the login session that
`pam_tid` needs — the standard workaround is the third-party `pam_reattach.so`).
It only affects `sudo`, not arbitrary password prompts that don't go through
PAM.

## License

Not yet specified.
