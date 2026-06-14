//! qmenu — a minimal dmenu/rofi-style launcher for wlr-layer-shell compositors.
//!
//! Reads newline-separated items on stdin, shows a full-width bar at the top of
//! the screen, lets you type to filter, and prints the chosen line to stdout.

use std::collections::HashSet;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use cosmic_text::{
    Attrs, Buffer as TextBuffer, Color as TextColor, FontSystem, Metrics, Shaping, SwashCache, Wrap,
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

// ---- Appearance ---------------------------------------------------------------

const FONT_SIZE: f32 = 16.0;
const LINE_HEIGHT: f32 = 24.0;
const PAD_X: f32 = 12.0;
const PAD_Y: f32 = 6.0;
const MAX_VISIBLE_ITEMS: usize = 14;

// Centred floating bar: fraction of the output width, with a floor and a small
// gap below the top edge.
const WIDTH_FRACTION: f32 = 0.45;
const MIN_WIDTH: u32 = 480;
const FALLBACK_SCREEN_WIDTH: u32 = 1920;
const MARGIN_TOP: i32 = 8;

// 0xAARRGGBB
const BG: u32 = 0xff1e1e2e;
const FG: u32 = 0xffcdd6f4;
const SEL_BG: u32 = 0xff45475a;
const PROMPT_FG: u32 = 0xff89b4fa;

fn argb_to_text_color(c: u32) -> TextColor {
    let a = ((c >> 24) & 0xff) as u8;
    let r = ((c >> 16) & 0xff) as u8;
    let g = ((c >> 8) & 0xff) as u8;
    let b = (c & 0xff) as u8;
    TextColor::rgba(r, g, b, a)
}

fn main() {
    // 1. Build the candidate list. Two modes:
    //   --drun : parse XDG .desktop entries (rofi drun-style app launcher); the
    //            visible label is the app Name, the action is its cleaned Exec.
    //   default: read newline-separated items from stdin (dmenu-style); each line
    //            is both the label and the action.
    let drun = std::env::args().skip(1).any(|a| a == "--drun");

    let (items, actions, allow_custom): (Vec<String>, Vec<String>, bool) = if drun {
        let entries = load_desktop_entries();
        let items = entries.iter().map(|(n, _)| n.clone()).collect();
        let actions = entries.into_iter().map(|(_, e)| e).collect();
        // Apps-only: don't run an arbitrary typed command when nothing matches.
        (items, actions, false)
    } else {
        let mut input = String::new();
        std::io::stdin()
            .read_to_string(&mut input)
            .expect("failed to read stdin");
        let items: Vec<String> = input
            .lines()
            .map(|l| l.to_string())
            .filter(|l| !l.is_empty())
            .collect();
        let actions = items.clone();
        (items, actions, true)
    };

    // 2. Connect to the Wayland compositor.
    let conn = Connection::connect_to_env().expect("could not connect to a Wayland compositor");
    let (globals, mut event_queue) =
        registry_queue_init(&conn).expect("failed to initialize the Wayland registry");
    let qh = event_queue.handle();

    let compositor =
        CompositorState::bind(&globals, &qh).expect("wl_compositor is not available");
    let layer_shell =
        LayerShell::bind(&globals, &qh).expect("wlr-layer-shell is not available on this compositor");
    let shm = Shm::bind(&globals, &qh).expect("wl_shm is not available");

    // 3. Create a layer surface: a centred bar anchored to the top. Anchoring to
    // TOP only (not LEFT/RIGHT) lets the compositor centre us horizontally; the
    // concrete width is computed from the output below.
    let surface = compositor.create_surface(&qh);
    let layer =
        layer_shell.create_layer_surface(&qh, surface, Layer::Overlay, Some("qmenu"), None);
    layer.set_anchor(Anchor::TOP);
    layer.set_margin(MARGIN_TOP, 0, 0, 0);
    layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);

    let visible_rows = 1 + items.len().min(MAX_VISIBLE_ITEMS);
    let height = (visible_rows as f32 * LINE_HEIGHT + 2.0 * PAD_Y).ceil() as u32;

    let pool = SlotPool::new(256 * 256 * 4, &shm).expect("failed to create a buffer pool");

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

        width: 0,
        height,
        configured: false,

        items,
        actions,
        allow_custom,
        query: String::new(),
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
    let screen_width = state
        .output_state
        .outputs()
        .filter_map(|o| state.output_state.info(&o))
        .filter_map(|i| {
            i.logical_size
                .or_else(|| i.modes.iter().find(|m| m.current).map(|m| m.dimensions))
        })
        .map(|(w, _)| w as u32)
        .max()
        .unwrap_or(FALLBACK_SCREEN_WIDTH);
    let bar_width = ((screen_width as f32 * WIDTH_FRACTION) as u32).max(MIN_WIDTH);
    state.width = bar_width;
    state.layer.set_size(bar_width, height);
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
        let mut out = std::io::stdout();
        let _ = writeln!(out, "{}", choice);
    } else {
        // Nothing chosen (Escape): exit non-zero like dmenu.
        std::process::exit(1);
    }
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

    width: u32,
    height: u32,
    configured: bool,

    items: Vec<String>,
    /// Parallel to `items`: what to emit on stdout when an item is chosen.
    actions: Vec<String>,
    /// Whether Enter on a no-match query echoes the raw query (dmenu) or does
    /// nothing (drun apps-only).
    allow_custom: bool,
    query: String,
    filtered: Vec<usize>,
    selected: usize,
    scroll: usize,

    exit: bool,
    result: Option<String>,
}

impl Qmenu {
    fn recompute_filter(&mut self) {
        let q = self.query.to_lowercase();
        self.filtered = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| q.is_empty() || item.to_lowercase().contains(&q))
            .map(|(i, _)| i)
            .collect();
        self.selected = 0;
        self.scroll = 0;
    }

    fn move_selection(&mut self, delta: isize) {
        if self.filtered.is_empty() {
            return;
        }
        let len = self.filtered.len() as isize;
        let mut sel = self.selected as isize + delta;
        if sel < 0 {
            sel = 0;
        }
        if sel >= len {
            sel = len - 1;
        }
        self.selected = sel as usize;

        // Keep the selection within the visible window.
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + MAX_VISIBLE_ITEMS {
            self.scroll = self.selected + 1 - MAX_VISIBLE_ITEMS;
        }
    }

    fn confirm(&mut self) {
        if let Some(&idx) = self.filtered.get(self.selected) {
            self.result = Some(self.actions[idx].clone());
        } else if self.allow_custom && !self.query.is_empty() {
            // No match: echo the raw query (dmenu's behaviour).
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

        let (buffer, canvas) = self
            .pool
            .create_buffer(
                width as i32,
                height as i32,
                stride,
                wl_shm::Format::Argb8888,
            )
            .expect("failed to create a drawing buffer");

        // Background fill.
        paint_solid(canvas, BG);

        // Highlight bar behind the selected row.
        if !self.filtered.is_empty() {
            let row = (self.selected - self.scroll) + 1; // +1: row 0 is the prompt.
            let y0 = PAD_Y + row as f32 * LINE_HEIGHT;
            fill_rect(canvas, width, height, 0.0, y0, width as f32, LINE_HEIGHT, SEL_BG);
        }

        // Build the text block: prompt line + the visible slice of matches.
        let end = (self.scroll + MAX_VISIBLE_ITEMS).min(self.filtered.len());
        let mut text = format!("> {}", self.query);
        for &idx in &self.filtered[self.scroll..end] {
            text.push('\n');
            text.push_str(&self.items[idx]);
        }

        render_text(
            &mut self.font_system,
            &mut self.swash_cache,
            canvas,
            width,
            height,
            &text,
        );

        let surface = self.layer.wl_surface();
        surface.attach(Some(buffer.wl_buffer()), 0, 0);
        surface.damage_buffer(0, 0, width as i32, height as i32);
        surface.frame(qh, surface.clone());
        self.layer.commit();
    }

}

fn render_text(
    font_system: &mut FontSystem,
    swash_cache: &mut SwashCache,
    canvas: &mut [u8],
    width: u32,
    height: u32,
    text: &str,
) {
    let metrics = Metrics::new(FONT_SIZE, LINE_HEIGHT);
    let mut buffer = TextBuffer::new(font_system, metrics);
    buffer.set_wrap(font_system, Wrap::None);
    buffer.set_size(
        font_system,
        Some(width as f32 - 2.0 * PAD_X),
        Some(height as f32),
    );
    buffer.set_text(font_system, text, Attrs::new(), Shaping::Advanced);
    buffer.shape_until_scroll(font_system, false);

    let default = argb_to_text_color(FG);
    let prompt = argb_to_text_color(PROMPT_FG);

    buffer.draw(font_system, swash_cache, default, |x, y, w, h, color| {
        // The prompt line (y within the first row) gets an accent colour. Only
        // swap RGB — cosmic-text packs the anti-aliasing coverage into the alpha
        // channel, so keep `color.a()` or the glyphs render as solid boxes.
        let on_prompt_row = (y as f32) < PAD_Y + LINE_HEIGHT;
        let color = if on_prompt_row {
            TextColor::rgba(prompt.r(), prompt.g(), prompt.b(), color.a())
        } else {
            color
        };
        let px = x + PAD_X as i32;
        let py = y + PAD_Y as i32;
        blend_rect(canvas, width, height, px, py, w, h, color);
    });
}

// ---- Desktop entries (drun mode) ----------------------------------------------

/// Discover and parse XDG `.desktop` application entries, returning
/// (display name, launch command) pairs sorted by name. Follows the freedesktop
/// precedence rule: the first occurrence of a given desktop-file ID wins, so
/// `$XDG_DATA_HOME` shadows the system dirs.
fn load_desktop_entries() -> Vec<(String, String)> {
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

    // Terminal=true entries are wrapped so they get a window.
    let terminal = std::env::var("QMENU_TERMINAL")
        .or_else(|_| std::env::var("TERMINAL"))
        .unwrap_or_else(|_| "xterm".to_string());

    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<(String, String)> = Vec::new();
    for dir in dirs {
        let rd = match std::fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(_) => continue,
        };
        for entry in rd.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("desktop") {
                continue;
            }
            let id = match path.file_name().and_then(|n| n.to_str()) {
                Some(n) => n.to_string(),
                None => continue,
            };
            // First ID wins, even if this copy is hidden/unparseable.
            if !seen.insert(id) {
                continue;
            }
            if let Some(pair) = parse_desktop_entry(&path, &terminal) {
                out.push(pair);
            }
        }
    }
    out.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
    out
}

/// Parse a single `.desktop` file's `[Desktop Entry]` group. Returns None for
/// non-applications, hidden/NoDisplay entries, or entries without a usable Exec.
fn parse_desktop_entry(path: &Path, terminal: &str) -> Option<(String, String)> {
    let content = std::fs::read_to_string(path).ok()?;

    let mut in_entry = false;
    let mut name: Option<String> = None;
    let mut exec: Option<String> = None;
    let mut typ: Option<String> = None;
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
        let (key, val) = match line.split_once('=') {
            Some(kv) => kv,
            None => continue,
        };
        // Take the unlocalised key only (ignore `Name[de]` etc.).
        match key.trim() {
            "Name" => name.get_or_insert_with(|| val.trim().to_string()),
            "Exec" => exec.get_or_insert_with(|| val.trim().to_string()),
            "Type" => typ.get_or_insert_with(|| val.trim().to_string()),
            "NoDisplay" => {
                no_display = val.trim().eq_ignore_ascii_case("true");
                continue;
            }
            "Hidden" => {
                hidden = val.trim().eq_ignore_ascii_case("true");
                continue;
            }
            "Terminal" => {
                is_terminal = val.trim().eq_ignore_ascii_case("true");
                continue;
            }
            _ => continue,
        };
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
    let cmd = if is_terminal {
        format!("{} -e {}", terminal, cmd)
    } else {
        cmd
    };
    Some((name, cmd))
}

/// Strip Desktop Entry field codes (`%f`, `%U`, `%i`, …) from an Exec value and
/// unescape `%%`. Quoting is left intact so the result can run via `sh -c`.
fn clean_exec(exec: &str) -> String {
    let mut out = String::with_capacity(exec.len());
    let mut chars = exec.chars();
    while let Some(c) = chars.next() {
        if c == '%' {
            match chars.next() {
                Some('%') => out.push('%'),
                _ => {} // drop the field code
            }
        } else {
            out.push(c);
        }
    }
    out.trim().to_string()
}

// ---- Pixel helpers ------------------------------------------------------------

fn paint_solid(canvas: &mut [u8], argb: u32) {
    let bytes = argb.to_le_bytes(); // little-endian: [B, G, R, A] for Argb8888.
    for px in canvas.chunks_exact_mut(4) {
        px.copy_from_slice(&bytes);
    }
}

fn fill_rect(canvas: &mut [u8], width: u32, height: u32, x: f32, y: f32, w: f32, h: f32, argb: u32) {
    let bytes = argb.to_le_bytes();
    let x0 = x.max(0.0) as u32;
    let y0 = y.max(0.0) as u32;
    let x1 = ((x + w) as u32).min(width);
    let y1 = ((y + h) as u32).min(height);
    for py in y0..y1 {
        let row = (py * width) as usize * 4;
        for px in x0..x1 {
            let off = row + px as usize * 4;
            canvas[off..off + 4].copy_from_slice(&bytes);
        }
    }
}

/// Alpha-blend a coverage rect emitted by cosmic-text onto the canvas.
fn blend_rect(
    canvas: &mut [u8],
    width: u32,
    height: u32,
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    color: TextColor,
) {
    let (cr, cg, cb, ca) = (color.r(), color.g(), color.b(), color.a());
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
            let a = ca as u32;
            let inv = 255 - a;
            // canvas is [B, G, R, A]
            let db = canvas[off] as u32;
            let dg = canvas[off + 1] as u32;
            let dr = canvas[off + 2] as u32;
            canvas[off] = ((cb as u32 * a + db * inv) / 255) as u8;
            canvas[off + 1] = ((cg as u32 * a + dg * inv) / 255) as u8;
            canvas[off + 2] = ((cr as u32 * a + dr * inv) / 255) as u8;
            canvas[off + 3] = 0xff;
        }
    }
}

// ---- Wayland handlers ---------------------------------------------------------

impl CompositorHandler for Qmenu {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: i32,
    ) {
    }

    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wayland_client::protocol::wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: u32,
    ) {
    }

    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
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

    fn configure(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        _: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _: u32,
    ) {
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

    fn new_capability(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        seat: wayland_client::protocol::wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard && self.keyboard.is_none() {
            let kb = self
                .seat_state
                .get_keyboard(qh, &seat, None)
                .expect("failed to obtain the keyboard");
            self.keyboard = Some(kb);
        }
    }

    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wayland_client::protocol::wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard {
            if let Some(kb) = self.keyboard.take() {
                kb.release();
            }
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wayland_client::protocol::wl_seat::WlSeat) {}
}

impl KeyboardHandler for Qmenu {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: &wl_surface::WlSurface,
        _: u32,
        _: &[u32],
        _: &[Keysym],
    ) {
    }

    fn leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: &wl_surface::WlSurface,
        _: u32,
    ) {
    }

    fn press_key(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        let ctrl = self.modifiers.ctrl;
        match event.keysym {
            Keysym::Escape => {
                self.exit = true;
                return;
            }
            Keysym::Return | Keysym::KP_Enter => {
                self.confirm();
                return;
            }
            Keysym::BackSpace => {
                self.query.pop();
                self.recompute_filter();
            }
            Keysym::Up => self.move_selection(-1),
            Keysym::Down => self.move_selection(1),
            Keysym::Page_Up => self.move_selection(-(MAX_VISIBLE_ITEMS as isize)),
            Keysym::Page_Down => self.move_selection(MAX_VISIBLE_ITEMS as isize),
            _ if ctrl => match event.keysym {
                Keysym::p => self.move_selection(-1),
                Keysym::n => self.move_selection(1),
                Keysym::c | Keysym::bracketleft => {
                    self.exit = true;
                    return;
                }
                Keysym::u => {
                    self.query.clear();
                    self.recompute_filter();
                }
                Keysym::w => {
                    while self.query.ends_with(' ') {
                        self.query.pop();
                    }
                    while !self.query.is_empty() && !self.query.ends_with(' ') {
                        self.query.pop();
                    }
                    self.recompute_filter();
                }
                _ => {}
            },
            _ => {
                if let Some(text) = event.utf8 {
                    for ch in text.chars() {
                        if !ch.is_control() {
                            self.query.push(ch);
                        }
                    }
                    self.recompute_filter();
                }
            }
        }
        self.draw(qh);
    }

    fn release_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _: KeyEvent,
    ) {
    }

    fn update_modifiers(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        modifiers: Modifiers,
        _: u32,
    ) {
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
