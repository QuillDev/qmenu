//! qmenu — a minimal, themeable dmenu/rofi-style launcher for wlr-layer-shell
//! compositors.
//!
//! It renders a centred, rounded floating bar near the top of the screen, lets
//! you type to filter, and either prints your choice (dmenu mode) or launches an
//! application with icons (drun mode). Appearance and behaviour are driven by a
//! TOML config file — see `src/config.rs`.

mod config;
mod icons;

use std::collections::HashSet;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use cosmic_text::{
    Attrs, Buffer as TextBuffer, Color as TextColor, Family, FontSystem, Metrics, Shaping,
    SwashCache, Wrap,
};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_keyboard, delegate_layer, delegate_output, delegate_registry,
    delegate_seat, delegate_shm,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers},
        Capability, SeatHandler, SeatState,
    },
    shell::{
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
        WaylandSurface,
    },
    shm::{slot::SlotPool, Shm, ShmHandler},
};
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_keyboard, wl_output, wl_shm, wl_surface},
    Connection, QueueHandle,
};

use config::Config;
use icons::IconLoader;

const FALLBACK_SCREEN_WIDTH: u32 = 1920;

/// A selectable row: a visible `name`, the `action` emitted on stdout when
/// chosen, and an optional icon name (drun mode).
struct Entry {
    name: String,
    action: String,
    icon: Option<String>,
}

fn main() {
    let mut drun = false;
    let mut config_path: Option<PathBuf> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--drun" => drun = true,
            "--config" => config_path = args.next().map(PathBuf::from),
            "-h" | "--help" => {
                print_help();
                return;
            }
            s if s.starts_with("--config=") => {
                config_path = Some(PathBuf::from(&s["--config=".len()..]));
            }
            _ => {}
        }
    }

    let config = Config::load(config_path);

    // Build the candidate list. drun → XDG .desktop apps (with icons); default →
    // newline-separated stdin (dmenu-style, no icons, raw query allowed).
    let (entries, allow_custom): (Vec<Entry>, bool) = if drun {
        (load_desktop_entries(&config.terminal), false)
    } else {
        let mut input = String::new();
        std::io::stdin()
            .read_to_string(&mut input)
            .expect("failed to read stdin");
        let entries = input
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| Entry {
                name: l.to_string(),
                action: l.to_string(),
                icon: None,
            })
            .collect();
        (entries, true)
    };

    // Connect to Wayland.
    let conn = Connection::connect_to_env().expect("could not connect to a Wayland compositor");
    let (globals, mut event_queue) =
        registry_queue_init(&conn).expect("failed to initialize the Wayland registry");
    let qh = event_queue.handle();

    let compositor = CompositorState::bind(&globals, &qh).expect("wl_compositor is not available");
    let layer_shell = LayerShell::bind(&globals, &qh)
        .expect("wlr-layer-shell is not available on this compositor");
    let shm = Shm::bind(&globals, &qh).expect("wl_shm is not available");

    // Centred bar anchored to the top: anchoring TOP only lets the compositor
    // centre us horizontally; the concrete width comes from the output below.
    let surface = compositor.create_surface(&qh);
    let layer = layer_shell.create_layer_surface(&qh, surface, Layer::Overlay, Some("qmenu"), None);
    // Always anchor the TOP edge so results grow downward (the prompt stays put)
    // rather than the box re-centring and pushing up. The concrete top margin is
    // computed after the roundtrip below: `margin_top` for "top", or whatever
    // centres the collapsed bar for "center".
    layer.set_anchor(Anchor::TOP);
    layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);

    let icon_loader = IconLoader::new(config.icon_size, config.icon_theme.clone());

    // Pool sized for the largest buffer we might draw (full width × max height:
    // both panels fully populated, plus the gap between them).
    let panel_pad = 2.0 * (config.pad_y + config.border_width);
    let max_height = (config.line_height
        + panel_pad
        + config.result_gap
        + config.max_visible_items as f32 * config.line_height
        + panel_pad)
        .ceil() as u32;
    let pool_cap = (2200 * max_height.max(1) * 4) as usize;
    let pool = SlotPool::new(pool_cap.max(256 * 256 * 4), &shm).expect("failed to create a buffer pool");

    let mut state = Qmenu {
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        shm,
        pool,
        layer,
        keyboard: None,
        modifiers: Modifiers::default(),

        font_system: FontSystem::new(),
        swash_cache: SwashCache::new(),
        icon_loader,
        config,

        width: 0,
        height: 0,
        configured: false,

        entries,
        allow_custom,
        query: String::new(),
        cursor: 0,
        filtered: Vec::new(),
        selected: 0,
        scroll: 0,

        exit: false,
        result: None,
    };
    state.recompute_filter();

    // A roundtrip lets OutputState learn the monitor geometry; size the bar to a
    // centred fraction of the widest output (fallback if none reports a size).
    event_queue
        .roundtrip(&mut state)
        .expect("initial Wayland roundtrip failed");
    let (screen_width, screen_height) = state
        .output_state
        .outputs()
        .filter_map(|o| state.output_state.info(&o))
        .filter_map(|i| {
            i.logical_size
                .or_else(|| i.modes.iter().find(|m| m.current).map(|m| m.dimensions))
        })
        .max_by_key(|(w, _)| *w)
        .map(|(w, h)| (w as u32, h as u32))
        .unwrap_or((FALLBACK_SCREEN_WIDTH, 1080));
    let bar_width = ((screen_width as f32 * state.config.width_fraction) as u32)
        .max(state.config.min_width)
        .min(2200);

    // Pin the top edge. "top": a fixed gap. "center": position the *collapsed*
    // (prompt-only) bar at the vertical centre so it looks centred, then let
    // results extend downward without moving the prompt.
    let cfg = &state.config;
    let collapsed = (cfg.line_height + 2.0 * (cfg.pad_y + cfg.border_width)).ceil() as u32;
    let margin_top = if cfg.anchor == "top" {
        cfg.margin_top
    } else {
        (screen_height.saturating_sub(collapsed) / 2) as i32
    };
    state.layer.set_margin(margin_top, 0, 0, 0);
    state.width = bar_width;
    state.height = state.content_height();
    state.layer.set_size(state.width, state.height);
    state.layer.commit();

    loop {
        event_queue
            .blocking_dispatch(&mut state)
            .expect("Wayland dispatch failed");
        if state.exit {
            break;
        }
    }

    if let Some(choice) = state.result {
        let _ = writeln!(std::io::stdout(), "{}", choice);
    } else {
        std::process::exit(1);
    }
}

fn print_help() {
    println!(
        "qmenu — minimal themeable launcher for wlr-layer-shell\n\n\
         USAGE:\n    qmenu [--drun] [--config <path>]\n\n\
         MODES:\n    (default)   read newline items from stdin, print the choice (dmenu)\n    \
         --drun      list XDG .desktop apps with icons, print the chosen Exec\n\n\
         OPTIONS:\n    --config <path>   use this config file instead of the default\n    \
         -h, --help        show this help\n\n\
         CONFIG:\n    ~/.config/qmenu/config.toml (or $QMENU_CONFIG). Every colour, size,\n    \
         font and behaviour toggle lives there; see config.example.toml."
    );
}

struct Qmenu {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    shm: Shm,
    pool: SlotPool,
    layer: LayerSurface,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    modifiers: Modifiers,

    font_system: FontSystem,
    swash_cache: SwashCache,
    icon_loader: IconLoader,
    config: Config,

    width: u32,
    height: u32,
    configured: bool,

    entries: Vec<Entry>,
    allow_custom: bool,
    query: String,
    /// Byte offset of the text cursor within `query` (always on a char boundary).
    cursor: usize,
    filtered: Vec<usize>,
    selected: usize,
    scroll: usize,

    exit: bool,
    result: Option<String>,
}

impl Qmenu {
    /// Number of result rows shown in the results panel right now.
    fn visible_count(&self) -> usize {
        let end = (self.scroll + self.config.max_visible_items).min(self.filtered.len());
        end - self.scroll
    }

    /// Total surface height: the constant prompt panel, plus (when there are
    /// matches) a gap and a separate results panel below it.
    fn content_height(&self) -> u32 {
        let cfg = &self.config;
        let panel_pad = 2.0 * (cfg.pad_y + cfg.border_width);
        let prompt_h = cfg.line_height + panel_pad;
        let nvis = self.visible_count();
        let total = if nvis == 0 {
            prompt_h
        } else {
            prompt_h + cfg.result_gap + (nvis as f32 * cfg.line_height + panel_pad)
        };
        total.ceil() as u32
    }

    fn recompute_filter(&mut self) {
        let q = self.query.to_lowercase();
        self.filtered = if q.is_empty() && !self.config.show_all_when_empty {
            Vec::new()
        } else {
            self.entries
                .iter()
                .enumerate()
                .filter(|(_, e)| q.is_empty() || e.name.to_lowercase().contains(&q))
                .map(|(i, _)| i)
                .collect()
        };
        self.selected = 0;
        self.scroll = 0;
    }

    // ---- Text-field editing (operates on `query`/`cursor`) ---------------------

    fn insert_char(&mut self, ch: char) {
        self.query.insert(self.cursor, ch);
        self.cursor += ch.len_utf8();
    }

    fn prev_boundary(&self) -> usize {
        self.query[..self.cursor]
            .char_indices()
            .last()
            .map(|(i, _)| i)
            .unwrap_or(0)
    }

    fn next_boundary(&self) -> usize {
        self.query[self.cursor..]
            .chars()
            .next()
            .map(|c| self.cursor + c.len_utf8())
            .unwrap_or(self.cursor)
    }

    /// Start of the word before the cursor (skips trailing whitespace, then the
    /// word), for Ctrl/Alt+Left and word deletion.
    fn prev_word(&self) -> usize {
        let chars: Vec<(usize, char)> = self.query[..self.cursor].char_indices().collect();
        let mut k = chars.len();
        while k > 0 && chars[k - 1].1.is_whitespace() {
            k -= 1;
        }
        while k > 0 && !chars[k - 1].1.is_whitespace() {
            k -= 1;
        }
        chars.get(k).map(|(i, _)| *i).unwrap_or(0)
    }

    /// End of the word after the cursor (skips leading whitespace, then the word).
    fn next_word(&self) -> usize {
        let chars: Vec<(usize, char)> = self.query[self.cursor..].char_indices().collect();
        let mut j = 0;
        while j < chars.len() && chars[j].1.is_whitespace() {
            j += 1;
        }
        while j < chars.len() && !chars[j].1.is_whitespace() {
            j += 1;
        }
        chars.get(j).map(|(i, _)| self.cursor + *i).unwrap_or(self.query.len())
    }

    fn backspace(&mut self) {
        let p = self.prev_boundary();
        self.query.replace_range(p..self.cursor, "");
        self.cursor = p;
    }

    fn delete_forward(&mut self) {
        let n = self.next_boundary();
        self.query.replace_range(self.cursor..n, "");
    }

    fn delete_word_back(&mut self) {
        let p = self.prev_word();
        self.query.replace_range(p..self.cursor, "");
        self.cursor = p;
    }

    fn delete_word_forward(&mut self) {
        let n = self.next_word();
        self.query.replace_range(self.cursor..n, "");
    }

    fn kill_to_start(&mut self) {
        self.query.replace_range(0..self.cursor, "");
        self.cursor = 0;
    }

    fn kill_to_end(&mut self) {
        self.query.truncate(self.cursor);
    }

    /// Resize the surface to fit the current results, then redraw.
    fn relayout_and_draw(&mut self, qh: &QueueHandle<Self>) {
        let h = self.content_height();
        if h != self.height {
            self.height = h;
            self.layer.set_size(self.width, self.height);
        }
        self.draw(qh);
    }

    fn move_selection(&mut self, delta: isize) {
        if self.filtered.is_empty() {
            return;
        }
        let len = self.filtered.len() as isize;
        let sel = (self.selected as isize + delta).clamp(0, len - 1);
        self.selected = sel as usize;

        let max_vis = self.config.max_visible_items;
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + max_vis {
            self.scroll = self.selected + 1 - max_vis;
        }
    }

    fn confirm(&mut self) {
        if let Some(&idx) = self.filtered.get(self.selected) {
            self.result = Some(self.entries[idx].action.clone());
        } else if self.allow_custom && !self.query.is_empty() {
            self.result = Some(self.query.clone());
        }
        self.exit = true;
    }

    fn draw(&mut self, qh: &QueueHandle<Self>) {
        if self.width == 0 || self.height == 0 {
            return;
        }
        let width = self.width;
        let height = self.height;
        let stride = width as i32 * 4;

        // Disjoint borrows so the helpers can take canvas + font/icon state.
        let Qmenu {
            pool,
            layer,
            font_system,
            swash_cache,
            icon_loader,
            config,
            entries,
            filtered,
            query,
            cursor,
            selected,
            scroll,
            ..
        } = self;
        let cfg = &*config;

        let (buffer, canvas) = pool
            .create_buffer(width as i32, height as i32, stride, wl_shm::Format::Argb8888)
            .expect("failed to create a drawing buffer");

        // Transparent canvas: panels paint their own rounded shapes onto it, and
        // the gap between them stays see-through.
        clear(canvas);

        let bw = cfg.border_width;
        let panel_pad = 2.0 * (cfg.pad_y + bw);
        let prompt_h = cfg.line_height + panel_pad;
        let icon_col = if cfg.icons_enabled {
            cfg.icon_size as f32 + cfg.icon_gap
        } else {
            0.0
        };
        let text_x = cfg.pad_x + bw + icon_col;

        // --- Prompt panel: a constant rounded drawer that never changes shape. --
        draw_panel(
            canvas, width, height, 0.0, 0.0, width as f32, prompt_h, cfg.corner_radius, bw, cfg.bg,
            cfg.border,
        );

        let prompt_y = bw + cfg.pad_y;
        let (prompt_text, prompt_color) = if query.is_empty() {
            (cfg.placeholder.as_str(), cfg.muted)
        } else {
            (query.as_str(), cfg.fg)
        };
        draw_text_line(
            font_system, swash_cache, canvas, width, height, cfg, text_x, prompt_y, prompt_text,
            prompt_color,
        );

        // Solid caret at the cursor position in the prompt row.
        let prefix_end = (*cursor).min(query.len());
        let caret_x = text_x + measure_text(font_system, cfg, &query[..prefix_end]);
        fill_solid(
            canvas,
            width,
            height,
            caret_x,
            prompt_y + 3.0,
            2.0,
            cfg.line_height - 6.0,
            cfg.prompt,
        );

        // --- Results panel: a separate drawer that slides out below the prompt. -
        let end = (*scroll + cfg.max_visible_items).min(filtered.len());
        let nvis = end - *scroll;
        if nvis > 0 {
            let ry = prompt_h + cfg.result_gap;
            let results_h = nvis as f32 * cfg.line_height + panel_pad;
            draw_panel(
                canvas, width, height, 0.0, ry, width as f32, results_h, cfg.corner_radius, bw,
                cfg.bg, cfg.border,
            );

            let rows_top = ry + bw + cfg.pad_y;

            // Selection highlight (rounded), behind the selected row.
            let srow = *selected - *scroll;
            let y0 = rows_top + srow as f32 * cfg.line_height;
            let inset = bw + 6.0;
            fill_round_rect(
                canvas,
                width,
                height,
                inset,
                y0 + 1.0,
                width as f32 - 2.0 * inset,
                cfg.line_height - 2.0,
                cfg.row_radius,
                cfg.sel_bg,
            );

            for (vis, &idx) in filtered[*scroll..end].iter().enumerate() {
                let y = rows_top + vis as f32 * cfg.line_height;
                let entry = &entries[idx];

                if cfg.icons_enabled {
                    if let Some(name) = &entry.icon {
                        if let Some(icon) = icon_loader.get(name) {
                            let iy = y + (cfg.line_height - icon.size as f32) / 2.0;
                            blit_icon(canvas, width, height, cfg.pad_x + bw, iy, icon);
                        }
                    }
                }

                let color = if *scroll + vis == *selected { cfg.sel_fg } else { cfg.fg };
                draw_text_line(
                    font_system, swash_cache, canvas, width, height, cfg, text_x, y, &entry.name,
                    color,
                );
            }
        }

        // Convert the straight-alpha canvas to premultiplied (wl_shm expects it).
        premultiply(canvas);

        let surface = layer.wl_surface();
        surface.attach(Some(buffer.wl_buffer()), 0, 0);
        surface.damage_buffer(0, 0, width as i32, height as i32);
        surface.frame(qh, surface.clone());
        layer.commit();
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_text_line(
    font_system: &mut FontSystem,
    swash_cache: &mut SwashCache,
    canvas: &mut [u8],
    cw: u32,
    ch: u32,
    cfg: &Config,
    x: f32,
    y: f32,
    text: &str,
    color: u32,
) {
    let metrics = Metrics::new(cfg.font_size, cfg.line_height);
    let mut buffer = TextBuffer::new(font_system, metrics);
    buffer.set_wrap(font_system, Wrap::None);
    buffer.set_size(font_system, Some(cw as f32 - x - cfg.pad_x), Some(cfg.line_height));

    let mut attrs = Attrs::new();
    if let Some(fam) = &cfg.font_family {
        attrs = attrs.family(Family::Name(fam));
    }
    buffer.set_text(font_system, text, attrs, Shaping::Advanced);
    buffer.shape_until_scroll(font_system, false);

    let col = argb_to_text_color(color);
    buffer.draw(font_system, swash_cache, col, |gx, gy, gw, gh, c| {
        // cosmic-text packs anti-aliasing coverage into the alpha channel; keep
        // it (don't override `c.a()`) or glyphs render as solid boxes.
        blend_rect(canvas, cw, ch, x as i32 + gx, y as i32 + gy, gw, gh, c);
    });
}

/// Shaped pixel width of a single line of text (used to place the caret).
fn measure_text(font_system: &mut FontSystem, cfg: &Config, text: &str) -> f32 {
    if text.is_empty() {
        return 0.0;
    }
    let metrics = Metrics::new(cfg.font_size, cfg.line_height);
    let mut buffer = TextBuffer::new(font_system, metrics);
    buffer.set_wrap(font_system, Wrap::None);
    buffer.set_size(font_system, None, Some(cfg.line_height));
    let mut attrs = Attrs::new();
    if let Some(fam) = &cfg.font_family {
        attrs = attrs.family(Family::Name(fam));
    }
    buffer.set_text(font_system, text, attrs, Shaping::Advanced);
    buffer.shape_until_scroll(font_system, false);
    buffer
        .layout_runs()
        .map(|r| r.line_w)
        .fold(0.0_f32, f32::max)
}

// ---- Desktop entries (drun mode) ----------------------------------------------

/// Discover and parse XDG `.desktop` application entries sorted by name. Follows
/// the freedesktop precedence rule: the first occurrence of a desktop-file ID
/// wins, so `$XDG_DATA_HOME` shadows the system dirs.
fn load_desktop_entries(terminal: &str) -> Vec<Entry> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        let data_home = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(&home).join(".local/share"));
        dirs.push(data_home.join("applications"));
    }
    let data_dirs = std::env::var("XDG_DATA_DIRS")
        .unwrap_or_else(|_| "/usr/local/share:/usr/share".to_string());
    for d in data_dirs.split(':').filter(|d| !d.is_empty()) {
        dirs.push(PathBuf::from(d).join("applications"));
    }

    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<Entry> = Vec::new();
    for dir in dirs {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for entry in rd.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("desktop") {
                continue;
            }
            let Some(id) = path.file_name().and_then(|n| n.to_str()).map(String::from) else {
                continue;
            };
            if !seen.insert(id) {
                continue; // first ID wins, even if this copy is hidden/unparseable.
            }
            if let Some(e) = parse_desktop_entry(&path, terminal) {
                out.push(e);
            }
        }
    }
    out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    out
}

/// Parse a single `.desktop` file's `[Desktop Entry]` group. Returns None for
/// non-applications, hidden/NoDisplay entries, or entries without a usable Exec.
fn parse_desktop_entry(path: &Path, terminal: &str) -> Option<Entry> {
    let content = std::fs::read_to_string(path).ok()?;

    let mut in_entry = false;
    let mut name: Option<String> = None;
    let mut exec: Option<String> = None;
    let mut typ: Option<String> = None;
    let mut icon: Option<String> = None;
    let mut no_display = false;
    let mut hidden = false;
    let mut is_terminal = false;

    for line in content.lines() {
        let line = line.trim();
        if line.starts_with('[') && line.ends_with(']') {
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_entry || line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, val)) = line.split_once('=') else { continue };
        match key.trim() {
            "Name" => {
                name.get_or_insert_with(|| val.trim().to_string());
            }
            "Exec" => {
                exec.get_or_insert_with(|| val.trim().to_string());
            }
            "Type" => {
                typ.get_or_insert_with(|| val.trim().to_string());
            }
            "Icon" => {
                icon.get_or_insert_with(|| val.trim().to_string());
            }
            "NoDisplay" => no_display = val.trim().eq_ignore_ascii_case("true"),
            "Hidden" => hidden = val.trim().eq_ignore_ascii_case("true"),
            "Terminal" => is_terminal = val.trim().eq_ignore_ascii_case("true"),
            _ => {}
        }
    }

    if no_display || hidden {
        return None;
    }
    if let Some(t) = &typ {
        if t != "Application" {
            return None;
        }
    }

    let name = name?;
    let cmd = clean_exec(&exec?);
    if cmd.is_empty() {
        return None;
    }
    let action = if is_terminal {
        format!("{} -e {}", terminal, cmd)
    } else {
        cmd
    };
    Some(Entry { name, action, icon })
}

/// Strip Desktop Entry field codes (`%f`, `%U`, `%i`, …) from an Exec value and
/// unescape `%%`. Quoting is left intact so the result can run via `sh -c`.
fn clean_exec(exec: &str) -> String {
    let mut out = String::with_capacity(exec.len());
    let mut chars = exec.chars();
    while let Some(c) = chars.next() {
        if c == '%' {
            if let Some('%') = chars.next() {
                out.push('%');
            }
        } else {
            out.push(c);
        }
    }
    out.trim().to_string()
}

// ---- Pixel helpers ------------------------------------------------------------

fn argb_to_text_color(c: u32) -> TextColor {
    TextColor::rgba(
        ((c >> 16) & 0xff) as u8,
        ((c >> 8) & 0xff) as u8,
        (c & 0xff) as u8,
        ((c >> 24) & 0xff) as u8,
    )
}

/// Reset the canvas to fully transparent.
fn clear(canvas: &mut [u8]) {
    canvas.fill(0);
}

/// Fill a sharp rectangle by alpha-blending `argb` over the canvas (used for the
/// caret).
#[allow(clippy::too_many_arguments)]
fn fill_solid(canvas: &mut [u8], cw: u32, ch: u32, x: f32, y: f32, w: f32, h: f32, argb: u32) {
    let (r, g, b, a) = unpack(argb);
    let x0 = x.round().max(0.0) as u32;
    let y0 = y.round().max(0.0) as u32;
    let x1 = ((x + w).round() as u32).min(cw);
    let y1 = ((y + h).round() as u32).min(ch);
    for py in y0..y1 {
        for px in x0..x1 {
            let off = ((py * cw + px) * 4) as usize;
            blend_px(canvas, off, r, g, b, a);
        }
    }
}

/// Multiply every pixel's RGB by its alpha, converting the straight-alpha canvas
/// the rest of the drawing code builds into the premultiplied form wl_shm wants.
fn premultiply(canvas: &mut [u8]) {
    for px in canvas.chunks_exact_mut(4) {
        let a = px[3] as u32;
        if a == 255 {
            continue;
        }
        if a == 0 {
            px[0] = 0;
            px[1] = 0;
            px[2] = 0;
            continue;
        }
        px[0] = (px[0] as u32 * a / 255) as u8;
        px[1] = (px[1] as u32 * a / 255) as u8;
        px[2] = (px[2] as u32 * a / 255) as u8;
    }
}

/// Paint a rounded-rect panel (translucent fill + border) onto the transparent
/// canvas, writing straight-alpha pixels with anti-aliased outer edges. The
/// border is composited over the fill so it reads correctly after premultiply.
#[allow(clippy::too_many_arguments)]
fn draw_panel(
    canvas: &mut [u8],
    cw: u32,
    ch: u32,
    x: f32,
    y: f32,
    pw: f32,
    ph: f32,
    radius: f32,
    border_w: f32,
    fill: u32,
    border: u32,
) {
    let (fr, fg, fb, fa) = unpack(fill);
    let (br, bg, bb, ba) = unpack(border);
    let af = fa as f32 / 255.0;
    let radius = radius.min(pw / 2.0).min(ph / 2.0).max(0.0);

    let x0 = x.floor().max(0.0) as u32;
    let y0 = y.floor().max(0.0) as u32;
    let x1 = ((x + pw).ceil() as u32).min(cw);
    let y1 = ((y + ph).ceil() as u32).min(ch);
    for py in y0..y1 {
        for px in x0..x1 {
            let d = rr_sdf(px as f32 + 0.5 - x, py as f32 + 0.5 - y, pw, ph, radius);
            let cov = (0.5 - d).clamp(0.0, 1.0); // outer anti-aliased coverage
            if cov <= 0.0 {
                continue;
            }
            // Border alpha: 1 in the ring near the outer edge, fading to 0 at the
            // inner edge.
            let ab = if border_w > 0.0 {
                (ba as f32 / 255.0) * (0.5 + d + border_w).clamp(0.0, 1.0)
            } else {
                0.0
            };
            // Border over fill (straight-alpha source-over).
            let out_a = ab + af * (1.0 - ab);
            let (mut rr, mut gg, mut bbc) = (0.0, 0.0, 0.0);
            if out_a > 0.0 {
                rr = (br as f32 * ab + fr as f32 * af * (1.0 - ab)) / out_a;
                gg = (bg as f32 * ab + fg as f32 * af * (1.0 - ab)) / out_a;
                bbc = (bb as f32 * ab + fb as f32 * af * (1.0 - ab)) / out_a;
            }
            let off = ((py * cw + px) * 4) as usize;
            canvas[off] = bbc as u8;
            canvas[off + 1] = gg as u8;
            canvas[off + 2] = rr as u8;
            canvas[off + 3] = (out_a * cov * 255.0) as u8;
        }
    }
}

/// Alpha-blend a single straight-alpha colour over one canvas pixel.
#[inline]
fn blend_px(canvas: &mut [u8], off: usize, r: u8, g: u8, b: u8, a: u32) {
    if a == 0 {
        return;
    }
    let inv = 255 - a;
    canvas[off] = ((b as u32 * a + canvas[off] as u32 * inv) / 255) as u8;
    canvas[off + 1] = ((g as u32 * a + canvas[off + 1] as u32 * inv) / 255) as u8;
    canvas[off + 2] = ((r as u32 * a + canvas[off + 2] as u32 * inv) / 255) as u8;
    canvas[off + 3] = 0xff;
}

/// Alpha-blend a coverage rect emitted by cosmic-text onto the canvas.
fn blend_rect(canvas: &mut [u8], width: u32, height: u32, x: i32, y: i32, w: u32, h: u32, color: TextColor) {
    let (cr, cg, cb, ca) = (color.r(), color.g(), color.b(), color.a() as u32);
    if ca == 0 {
        return;
    }
    for dy in 0..h as i32 {
        let py = y + dy;
        if py < 0 || py >= height as i32 {
            continue;
        }
        for dx in 0..w as i32 {
            let px = x + dx;
            if px < 0 || px >= width as i32 {
                continue;
            }
            let off = ((py as u32 * width + px as u32) * 4) as usize;
            blend_px(canvas, off, cr, cg, cb, ca);
        }
    }
}

/// Blit a straight-alpha RGBA icon onto the canvas at (x, y).
fn blit_icon(canvas: &mut [u8], width: u32, height: u32, x: f32, y: f32, icon: &icons::Icon) {
    let ox = x.round() as i32;
    let oy = y.round() as i32;
    let s = icon.size;
    for iy in 0..s as i32 {
        let py = oy + iy;
        if py < 0 || py >= height as i32 {
            continue;
        }
        for ix in 0..s as i32 {
            let px = ox + ix;
            if px < 0 || px >= width as i32 {
                continue;
            }
            let si = ((iy as u32 * s + ix as u32) * 4) as usize;
            let (r, g, b, a) = (
                icon.rgba[si],
                icon.rgba[si + 1],
                icon.rgba[si + 2],
                icon.rgba[si + 3] as u32,
            );
            let off = ((py as u32 * width + px as u32) * 4) as usize;
            blend_px(canvas, off, r, g, b, a);
        }
    }
}

/// Signed distance from a point to a rounded rect of size (w, h) centred in its
/// own box, with corner radius r. Negative = inside.
fn rr_sdf(px: f32, py: f32, w: f32, h: f32, r: f32) -> f32 {
    let r = r.min(w / 2.0).min(h / 2.0).max(0.0);
    let qx = (px - w / 2.0).abs() - (w / 2.0 - r);
    let qy = (py - h / 2.0).abs() - (h / 2.0 - r);
    let ax = qx.max(0.0);
    let ay = qy.max(0.0);
    (ax * ax + ay * ay).sqrt() + qx.max(qy).min(0.0) - r
}

/// Fill a rounded rect with anti-aliased edges by alpha-blending `argb` over the
/// (assumed opaque) canvas.
#[allow(clippy::too_many_arguments)]
fn fill_round_rect(canvas: &mut [u8], cw: u32, ch: u32, x: f32, y: f32, w: f32, h: f32, r: f32, argb: u32) {
    let (cr, cg, cb, base_a) = unpack(argb);
    let x0 = x.floor().max(0.0) as u32;
    let y0 = y.floor().max(0.0) as u32;
    let x1 = ((x + w).ceil() as u32).min(cw);
    let y1 = ((y + h).ceil() as u32).min(ch);
    for py in y0..y1 {
        for px in x0..x1 {
            let d = rr_sdf(px as f32 + 0.5 - x, py as f32 + 0.5 - y, w, h, r);
            let cov = (0.5 - d).clamp(0.0, 1.0);
            if cov <= 0.0 {
                continue;
            }
            let a = (base_a as f32 * cov) as u32;
            let off = ((py * cw + px) * 4) as usize;
            blend_px(canvas, off, cr, cg, cb, a);
        }
    }
}

/// Unpack `0xAARRGGBB` into (r, g, b, a).
fn unpack(argb: u32) -> (u8, u8, u8, u32) {
    (
        ((argb >> 16) & 0xff) as u8,
        ((argb >> 8) & 0xff) as u8,
        (argb & 0xff) as u8,
        (argb >> 24) & 0xff,
    )
}

// ---- Wayland handlers ---------------------------------------------------------

impl CompositorHandler for Qmenu {
    fn scale_factor_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: i32) {}
    fn transform_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: wl_output::Transform) {}
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}
    fn surface_enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
    fn surface_leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
}

impl OutputHandler for Qmenu {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl LayerShellHandler for Qmenu {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        self.exit = true;
    }

    fn configure(&mut self, _: &Connection, qh: &QueueHandle<Self>, _: &LayerSurface, configure: LayerSurfaceConfigure, _: u32) {
        let (w, h) = configure.new_size;
        if w != 0 {
            self.width = w;
        }
        if h != 0 {
            self.height = h;
        }
        self.configured = true;
        self.draw(qh);
    }
}

impl SeatHandler for Qmenu {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }
    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wayland_client::protocol::wl_seat::WlSeat) {}

    fn new_capability(&mut self, _: &Connection, qh: &QueueHandle<Self>, seat: wayland_client::protocol::wl_seat::WlSeat, capability: Capability) {
        if capability == Capability::Keyboard && self.keyboard.is_none() {
            let kb = self.seat_state.get_keyboard(qh, &seat, None).expect("failed to obtain the keyboard");
            self.keyboard = Some(kb);
        }
    }

    fn remove_capability(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wayland_client::protocol::wl_seat::WlSeat, capability: Capability) {
        if capability == Capability::Keyboard {
            if let Some(kb) = self.keyboard.take() {
                kb.release();
            }
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wayland_client::protocol::wl_seat::WlSeat) {}
}

impl KeyboardHandler for Qmenu {
    fn enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: &wl_surface::WlSurface, _: u32, _: &[u32], _: &[Keysym]) {}
    fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: &wl_surface::WlSurface, _: u32) {}

    fn press_key(&mut self, _: &Connection, qh: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, event: KeyEvent) {
        let ctrl = self.modifiers.ctrl;
        let alt = self.modifiers.alt;
        let word = ctrl || alt; // modifier for word-wise motion / deletion
        match event.keysym {
            Keysym::Escape => {
                self.exit = true;
                return;
            }
            Keysym::Return | Keysym::KP_Enter => {
                self.confirm();
                return;
            }

            // List navigation.
            Keysym::Up => self.move_selection(-1),
            Keysym::Down => self.move_selection(1),
            Keysym::Page_Up => self.move_selection(-(self.config.max_visible_items as isize)),
            Keysym::Page_Down => self.move_selection(self.config.max_visible_items as isize),

            // Cursor motion within the query (word-wise with Ctrl/Alt).
            Keysym::Left => {
                self.cursor = if word { self.prev_word() } else { self.prev_boundary() }
            }
            Keysym::Right => {
                self.cursor = if word { self.next_word() } else { self.next_boundary() }
            }
            Keysym::Home => self.cursor = 0,
            Keysym::End => self.cursor = self.query.len(),

            // Deletion (word-wise with Ctrl/Alt).
            Keysym::BackSpace => {
                if word {
                    self.delete_word_back();
                } else {
                    self.backspace();
                }
                self.recompute_filter();
            }
            Keysym::Delete => {
                if word {
                    self.delete_word_forward();
                } else {
                    self.delete_forward();
                }
                self.recompute_filter();
            }

            // Emacs/readline-style Ctrl bindings.
            _ if ctrl => match event.keysym {
                Keysym::p => self.move_selection(-1),
                Keysym::n => self.move_selection(1),
                Keysym::c | Keysym::bracketleft => {
                    self.exit = true;
                    return;
                }
                Keysym::a => self.cursor = 0,
                Keysym::e => self.cursor = self.query.len(),
                Keysym::b => self.cursor = self.prev_boundary(),
                Keysym::f => self.cursor = self.next_boundary(),
                Keysym::u => {
                    self.kill_to_start();
                    self.recompute_filter();
                }
                Keysym::k => {
                    self.kill_to_end();
                    self.recompute_filter();
                }
                Keysym::w => {
                    self.delete_word_back();
                    self.recompute_filter();
                }
                _ => {}
            },

            // Alt word bindings (Alt+b/f move, Alt+d delete forward word).
            _ if alt => match event.keysym {
                Keysym::b => self.cursor = self.prev_word(),
                Keysym::f => self.cursor = self.next_word(),
                Keysym::d => {
                    self.delete_word_forward();
                    self.recompute_filter();
                }
                _ => {}
            },

            // Printable input: insert at the cursor.
            _ => {
                if let Some(text) = event.utf8 {
                    for ch in text.chars() {
                        if !ch.is_control() {
                            self.insert_char(ch);
                        }
                    }
                    self.recompute_filter();
                }
            }
        }
        self.relayout_and_draw(qh);
    }

    fn release_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, _: KeyEvent) {}

    fn update_modifiers(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, modifiers: Modifiers, _: u32) {
        self.modifiers = modifiers;
    }
}

impl ShmHandler for Qmenu {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

delegate_compositor!(Qmenu);
delegate_output!(Qmenu);
delegate_seat!(Qmenu);
delegate_keyboard!(Qmenu);
delegate_layer!(Qmenu);
delegate_shm!(Qmenu);
delegate_registry!(Qmenu);

impl ProvidesRegistryState for Qmenu {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}
