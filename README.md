# qmenu

A minimal, themeable dmenu/rofi-style launcher for Wayland compositors that
support [`wlr-layer-shell`](https://wayland.app/protocols/wlr-layer-shell-unstable-v1)
(Hyprland, Sway, river, …). It renders a centred, rounded floating bar near the
top of the screen, lets you type to filter, and either prints your choice (dmenu
mode) or launches an application — with icons — (drun mode). Colours, fonts,
sizes, icons and behaviour are all driven by a [TOML config file](#configuration).

It is a small, dependency-light Rust program built on
[`smithay-client-toolkit`](https://crates.io/crates/smithay-client-toolkit) for
the Wayland plumbing and [`cosmic-text`](https://crates.io/crates/cosmic-text)
for text shaping and rendering. Drawing is plain software blitting into a shared
memory buffer — no GPU/EGL required.

## Modes

- **dmenu mode (default).** Reads newline-separated items on stdin, shows them,
  and prints the selected line to stdout. If nothing matches, Enter echoes the
  raw query (like dmenu). Use it as a generic chooser:

  ```sh
  printf 'one\ntwo\nthree\n' | qmenu
  ```

- **drun mode (`--drun`).** Ignores stdin and instead discovers XDG `.desktop`
  application entries, shows their friendly `Name` (with the app icon), and
  prints the selected app's cleaned `Exec` line to stdout. Entries marked
  `NoDisplay`/`Hidden` and non-`Application` types are skipped, and desktop-file
  IDs are de-duplicated following the freedesktop precedence rules
  (`$XDG_DATA_HOME` shadows the system directories). Icons are resolved against
  the freedesktop icon theme dirs and rasterised from PNG or SVG. This is the
  equivalent of `rofi -show drun`.

  ```sh
  qmenu --drun
  ```

  `Terminal=true` entries are wrapped as `$TERMINAL -e <exec>`. The terminal is
  read from `QMENU_TERMINAL`, then `TERMINAL`, defaulting to `xterm`.

## Keybindings

| Key | Action |
| --- | --- |
| Type | Filter (substring, case-insensitive) |
| `Up` / `Down`, `Ctrl-p` / `Ctrl-n` | Move selection |
| `PageUp` / `PageDown` | Move a page |
| `Enter` | Confirm selection (or, in dmenu mode, the typed query) |
| `Esc`, `Ctrl-c` | Cancel (exit non-zero, like dmenu) |
| `Backspace`, `Ctrl-u`, `Ctrl-w` | Edit the query |

## Building

### Cargo

Native libraries are needed at build and run time: `wayland`, `libxkbcommon`,
`fontconfig`, `freetype` (and `pkg-config` to find them).

```sh
cargo build --release
./target/release/qmenu --drun
```

Some of those libraries are `dlopen`ed at runtime; if your distro doesn't place
them on the default loader path you may need `LD_LIBRARY_PATH` set (the Nix build
below wraps the binary to handle this automatically).

### Nix

```sh
# Flake:
nix build            # -> ./result/bin/qmenu
nix run . -- --drun

# Or without flakes:
nix-build            # -> ./result/bin/qmenu
nix develop          # or: nix-shell, for a dev environment
```

The Nix package wraps the binary with the required `LD_LIBRARY_PATH`, so the
result runs anywhere without further setup.

## Using it as an app launcher

`contrib/qmenu-run` is an example launcher script with **toggle** behaviour:
bind it to a key, and pressing that key again while qmenu is open closes it
(rather than opening a second instance). It tracks the running instance via a
pidfile in `$XDG_RUNTIME_DIR`.

Example Hyprland bind:

```
bind = SUPER, Space, exec, qmenu-run
```

Example Sway bind:

```
bindsym $mod+space exec qmenu-run
```

## Configuration

qmenu reads a TOML config file; every key is optional and falls back to a
built-in default. The lookup order (first that exists wins) is:

1. `--config <path>`
2. `$QMENU_CONFIG`
3. `$XDG_CONFIG_HOME/qmenu/config.toml` (i.e. `~/.config/qmenu/config.toml`)
4. `$QMENU_DEFAULT_CONFIG` — a packaged fallback (see [Nix](#nix) below)

Copy [`config.example.toml`](config.example.toml) to
`~/.config/qmenu/config.toml` and edit. It documents the full schema: the colour
palette, geometry (`width_fraction`, `corner_radius`, `border_width`, paddings,
`font_size`, `font_family`, …), `[icons]` (toggle/size/theme), and `[behavior]`
(`show_all_when_empty`, `placeholder`, `terminal`). Colours accept `#rgb`,
`#rrggbb`, or `#aarrggbb`.

By default nothing is shown until you start typing; set
`behavior.show_all_when_empty = true` for the classic always-listed behaviour.

### Theming via the flake

The Nix package takes an optional `settings` attrset (the config schema as Nix),
serialised to TOML and baked in as `QMENU_DEFAULT_CONFIG` — a shipped default
theme that a user's own `~/.config/qmenu/config.toml` still overrides:

```nix
inputs.qmenu.packages.${system}.default.override {
  settings = {
    colors.background = "#0b0b10";
    colors.border = "#ff4fa3";
    layout.font_family = "Inter";
  };
};
```

## Limitations

- Substring filtering only (no fuzzy ranking).
- No HiDPI / fractional-scale handling (text uses logical pixels).

## License

MIT — see [LICENSE](LICENSE).
