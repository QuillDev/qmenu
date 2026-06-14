//! Runtime configuration for qmenu.
//!
//! Everything that affects appearance or behaviour is read from a TOML file so
//! the launcher can be themed without recompiling. Resolution order (first that
//! exists wins):
//!
//!   1. `--config <path>` command-line flag
//!   2. `$QMENU_CONFIG`
//!   3. `$XDG_CONFIG_HOME/qmenu/config.toml` (or `~/.config/qmenu/config.toml`)
//!   4. `$QMENU_DEFAULT_CONFIG` — a packaged fallback (the Nix `settings`
//!      option points this at a store path), so a flake can ship a theme that
//!      the user's own `~/.config` still overrides.
//!
//! Any key the file omits keeps its built-in default, so a partial config is
//! fine. See `config.example.toml` in the repo for the full schema.

use std::path::PathBuf;

use serde::Deserialize;

/// Fully-resolved configuration used by the rest of the program. Colours are
/// stored as `0xAARRGGBB`.
#[derive(Clone)]
pub struct Config {
    // Colours.
    pub bg: u32,
    pub fg: u32,
    pub prompt: u32,
    pub sel_bg: u32,
    pub sel_fg: u32,
    pub muted: u32,
    pub border: u32,

    // Layout / geometry.
    pub width_fraction: f32,
    pub min_width: u32,
    pub margin_top: i32,
    pub max_visible_items: usize,
    pub font_size: f32,
    pub line_height: f32,
    pub pad_x: f32,
    pub pad_y: f32,
    pub corner_radius: f32,
    pub border_width: f32,
    pub row_radius: f32,
    pub font_family: Option<String>,

    // Icons.
    pub icons_enabled: bool,
    pub icon_size: u32,
    pub icon_gap: f32,
    pub icon_theme: Option<String>,

    // Behaviour.
    pub show_all_when_empty: bool,
    pub placeholder: String,
    pub terminal: String,
}

impl Default for Config {
    fn default() -> Self {
        // Neutral dark default theme (Catppuccin-ish) with a blue accent. The
        // bundled flake overrides these to match the user's desktop.
        Config {
            bg: 0xf21e1e2e,
            fg: 0xffcdd6f4,
            prompt: 0xff89b4fa,
            sel_bg: 0xff89b4fa,
            sel_fg: 0xff11111b,
            muted: 0xff9399b2,
            border: 0xff89b4fa,

            width_fraction: 0.45,
            min_width: 480,
            margin_top: 8,
            max_visible_items: 12,
            font_size: 15.0,
            line_height: 30.0,
            pad_x: 14.0,
            pad_y: 10.0,
            corner_radius: 14.0,
            border_width: 2.0,
            row_radius: 8.0,
            font_family: None,

            icons_enabled: true,
            icon_size: 20,
            icon_gap: 10.0,
            icon_theme: None,

            show_all_when_empty: false,
            placeholder: "Search…".to_string(),
            terminal: "xterm".to_string(),
        }
    }
}

impl Config {
    /// Load configuration, applying the file found via `explicit` / env / XDG on
    /// top of the built-in defaults. Returns defaults (and warns on stderr) if a
    /// file is present but unparseable.
    pub fn load(explicit: Option<PathBuf>) -> Config {
        let mut cfg = Config::default();
        let env_path = |var: &str| std::env::var_os(var).map(PathBuf::from).filter(|p| p.exists());
        let path = explicit
            .filter(|p| p.exists())
            .or_else(|| env_path("QMENU_CONFIG"))
            .or_else(default_config_path)
            .or_else(|| env_path("QMENU_DEFAULT_CONFIG"));

        let Some(path) = path else { return cfg };
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(_) => return cfg,
        };
        let raw: RawConfig = match toml::from_str(&text) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("qmenu: ignoring config {}: {e}", path.display());
                return cfg;
            }
        };
        raw.apply(&mut cfg);
        cfg
    }
}

fn default_config_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    let p = base.join("qmenu/config.toml");
    p.exists().then_some(p)
}

// ---- TOML schema (all optional) -----------------------------------------------

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct RawConfig {
    colors: RawColors,
    layout: RawLayout,
    icons: RawIcons,
    behavior: RawBehavior,
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct RawColors {
    background: Option<String>,
    foreground: Option<String>,
    prompt: Option<String>,
    selection_background: Option<String>,
    selection_foreground: Option<String>,
    muted: Option<String>,
    border: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct RawLayout {
    width_fraction: Option<f32>,
    min_width: Option<u32>,
    margin_top: Option<i32>,
    max_visible_items: Option<usize>,
    font_size: Option<f32>,
    line_height: Option<f32>,
    pad_x: Option<f32>,
    pad_y: Option<f32>,
    corner_radius: Option<f32>,
    border_width: Option<f32>,
    row_radius: Option<f32>,
    font_family: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct RawIcons {
    enabled: Option<bool>,
    size: Option<u32>,
    gap: Option<f32>,
    theme: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct RawBehavior {
    show_all_when_empty: Option<bool>,
    placeholder: Option<String>,
    terminal: Option<String>,
}

impl RawConfig {
    fn apply(self, c: &mut Config) {
        let col = |opt: Option<String>, fallback: u32| opt.and_then(|s| parse_color(&s)).unwrap_or(fallback);

        c.bg = col(self.colors.background, c.bg);
        c.fg = col(self.colors.foreground, c.fg);
        c.prompt = col(self.colors.prompt, c.prompt);
        c.sel_bg = col(self.colors.selection_background, c.sel_bg);
        c.sel_fg = col(self.colors.selection_foreground, c.sel_fg);
        c.muted = col(self.colors.muted, c.muted);
        c.border = col(self.colors.border, c.border);

        let l = self.layout;
        if let Some(v) = l.width_fraction { c.width_fraction = v; }
        if let Some(v) = l.min_width { c.min_width = v; }
        if let Some(v) = l.margin_top { c.margin_top = v; }
        if let Some(v) = l.max_visible_items { c.max_visible_items = v.max(1); }
        if let Some(v) = l.font_size { c.font_size = v; }
        if let Some(v) = l.line_height { c.line_height = v; }
        if let Some(v) = l.pad_x { c.pad_x = v; }
        if let Some(v) = l.pad_y { c.pad_y = v; }
        if let Some(v) = l.corner_radius { c.corner_radius = v; }
        if let Some(v) = l.border_width { c.border_width = v; }
        if let Some(v) = l.row_radius { c.row_radius = v; }
        if l.font_family.is_some() { c.font_family = l.font_family; }

        let i = self.icons;
        if let Some(v) = i.enabled { c.icons_enabled = v; }
        if let Some(v) = i.size { c.icon_size = v; }
        if let Some(v) = i.gap { c.icon_gap = v; }
        if i.theme.is_some() { c.icon_theme = i.theme; }

        let b = self.behavior;
        if let Some(v) = b.show_all_when_empty { c.show_all_when_empty = v; }
        if let Some(v) = b.placeholder { c.placeholder = v; }
        if let Some(v) = b.terminal { c.terminal = v; }
    }
}

/// Parse `#rgb`, `#rrggbb`, or `#aarrggbb` into `0xAARRGGBB`. Returns None on
/// anything malformed so the caller can keep its default.
pub fn parse_color(s: &str) -> Option<u32> {
    let h = s.trim().strip_prefix('#')?;
    let (a, rgb): (u32, &str) = match h.len() {
        3 => {
            // #rgb -> expand each nibble.
            let mut v: u32 = 0xff000000;
            for (i, ch) in h.chars().enumerate() {
                let d = ch.to_digit(16)?;
                v |= (d * 0x11) << (16 - i * 8);
            }
            return Some(v);
        }
        6 => (0xff, h),
        8 => (u32::from_str_radix(&h[0..2], 16).ok()?, &h[2..]),
        _ => return None,
    };
    let rgb = u32::from_str_radix(rgb, 16).ok()?;
    Some((a << 24) | rgb)
}
