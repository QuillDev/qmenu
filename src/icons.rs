//! Application icons for drun mode.
//!
//! Desktop entries carry an `Icon=` key that is usually a *name* (e.g. `firefox`)
//! rather than a path. We resolve it against the freedesktop icon theme
//! directories, then decode the PNG (via `image`) or SVG (via `resvg`) into a
//! straight-alpha RGBA square sized for the menu rows. Resolution and decoding
//! are done lazily for visible rows only and cached by icon name.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// A decoded icon: a `size`×`size` straight-alpha RGBA8 bitmap.
pub struct Icon {
    pub size: u32,
    pub rgba: Vec<u8>,
}

/// Caches `Icon=` name -> decoded icon (or None when nothing was found/decoded),
/// so repeated draws while filtering don't re-hit the filesystem or re-rasterise.
pub struct IconLoader {
    size: u32,
    base_dirs: Vec<PathBuf>,
    themes: Vec<String>,
    cache: HashMap<String, Option<Icon>>,
}

impl IconLoader {
    pub fn new(size: u32, theme: Option<String>) -> Self {
        let mut themes: Vec<String> = Vec::new();
        if let Some(t) = theme {
            themes.push(t);
        }
        // Fall back through the common defaults; `hicolor` is the spec-mandated
        // last resort that every theme inherits from.
        for t in ["Adwaita", "hicolor"] {
            if !themes.iter().any(|x| x == t) {
                themes.push(t.to_string());
            }
        }
        IconLoader {
            size,
            base_dirs: icon_base_dirs(),
            themes,
            cache: HashMap::new(),
        }
    }

    /// Return the decoded icon for an `Icon=` value, using the cache.
    pub fn get(&mut self, name: &str) -> Option<&Icon> {
        if !self.cache.contains_key(name) {
            let icon = self
                .resolve(name)
                .and_then(|path| decode_icon(&path, self.size));
            self.cache.insert(name.to_string(), icon);
        }
        self.cache.get(name).and_then(|o| o.as_ref())
    }

    /// Resolve an `Icon=` value to a concrete file path.
    fn resolve(&self, name: &str) -> Option<PathBuf> {
        if name.is_empty() {
            return None;
        }
        // Absolute path or explicit file.
        let p = Path::new(name);
        if p.is_absolute() {
            return p.exists().then(|| p.to_path_buf());
        }

        // Ordered list of theme-relative subpaths to probe, near-size first.
        let mut sizes: Vec<u32> = vec![self.size, self.size * 2, 48, 64, 32, 128, 256, 24, 22, 16, 96, 512];
        sizes.dedup();
        let mut rels: Vec<String> = Vec::new();
        for s in &sizes {
            rels.push(format!("{s}x{s}/apps"));
        }
        rels.push("scalable/apps".to_string());
        for s in &sizes {
            rels.push(format!("apps/{s}x{s}"));
        }
        rels.push("apps/scalable".to_string());

        for base in &self.base_dirs {
            for theme in &self.themes {
                let tdir = base.join(theme);
                if !tdir.is_dir() {
                    continue;
                }
                for rel in &rels {
                    let dir = tdir.join(rel);
                    for ext in ["svg", "png"] {
                        let cand = dir.join(format!("{name}.{ext}"));
                        if cand.is_file() {
                            return Some(cand);
                        }
                    }
                }
            }
        }

        // Last resort: flat pixmaps / data dirs.
        for base in &self.base_dirs {
            for ext in ["svg", "png", "xpm"] {
                let cand = base.join(format!("{name}.{ext}"));
                if cand.is_file() && ext != "xpm" {
                    return Some(cand);
                }
            }
        }
        for dir in pixmap_dirs() {
            for ext in ["svg", "png"] {
                let cand = dir.join(format!("{name}.{ext}"));
                if cand.is_file() {
                    return Some(cand);
                }
            }
        }
        None
    }
}

/// Icon theme base directories per the freedesktop icon theme spec.
fn icon_base_dirs() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        v.push(home.join(".icons"));
        let data_home = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local/share"));
        v.push(data_home.join("icons"));
    }
    let data_dirs = std::env::var("XDG_DATA_DIRS")
        .unwrap_or_else(|_| "/usr/local/share:/usr/share".to_string());
    for d in data_dirs.split(':').filter(|d| !d.is_empty()) {
        v.push(PathBuf::from(d).join("icons"));
    }
    v
}

fn pixmap_dirs() -> Vec<PathBuf> {
    let mut v = vec![PathBuf::from("/usr/share/pixmaps")];
    let data_dirs = std::env::var("XDG_DATA_DIRS").unwrap_or_default();
    for d in data_dirs.split(':').filter(|d| !d.is_empty()) {
        v.push(PathBuf::from(d).join("pixmaps"));
    }
    v
}

/// Decode an icon file into a `size`×`size` straight-alpha RGBA8 bitmap,
/// preserving aspect ratio and centring within the square.
fn decode_icon(path: &Path, size: u32) -> Option<Icon> {
    let data = std::fs::read(path).ok()?;
    let is_svg = path.extension().and_then(|e| e.to_str()).map(|e| e.eq_ignore_ascii_case("svg")) == Some(true);
    let (src, sw, sh) = if is_svg {
        decode_svg(&data, size)?
    } else {
        decode_raster(&data)?
    };
    Some(Icon {
        size,
        rgba: fit_center(&src, sw, sh, size),
    })
}

/// Rasterise an SVG straight into a `size`×`size` buffer (resvg/tiny-skia give
/// premultiplied output, which we un-premultiply to straight alpha).
fn decode_svg(data: &[u8], size: u32) -> Option<(Vec<u8>, u32, u32)> {
    use resvg::tiny_skia;
    use resvg::usvg;

    let opt = usvg::Options::default();
    let tree = usvg::Tree::from_data(data, &opt).ok()?;
    let ts = tree.size();
    let (w, h) = (ts.width(), ts.height());
    if w <= 0.0 || h <= 0.0 {
        return None;
    }
    let scale = (size as f32 / w).min(size as f32 / h);
    let mut pixmap = tiny_skia::Pixmap::new(size, size)?;
    let tx = (size as f32 - w * scale) / 2.0;
    let ty = (size as f32 - h * scale) / 2.0;
    let transform = tiny_skia::Transform::from_row(scale, 0.0, 0.0, scale, tx, ty);
    resvg::render(&tree, transform, &mut pixmap.as_mut());

    let mut out = pixmap.take();
    // Un-premultiply (tiny-skia stores premultiplied RGBA).
    for px in out.chunks_exact_mut(4) {
        let a = px[3] as u32;
        if a != 0 && a != 255 {
            px[0] = ((px[0] as u32 * 255) / a).min(255) as u8;
            px[1] = ((px[1] as u32 * 255) / a).min(255) as u8;
            px[2] = ((px[2] as u32 * 255) / a).min(255) as u8;
        }
    }
    Some((out, size, size))
}

/// Decode a raster icon (PNG) to straight-alpha RGBA8 at its native size.
fn decode_raster(data: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    let img = image::load_from_memory(data).ok()?.to_rgba8();
    let (w, h) = img.dimensions();
    Some((img.into_raw(), w, h))
}

/// Scale a source RGBA bitmap to fit a `size`×`size` square (preserving aspect)
/// and centre it, returning the square buffer. Source already square+sized (SVG)
/// is returned as-is.
fn fit_center(src: &[u8], sw: u32, sh: u32, size: u32) -> Vec<u8> {
    if sw == size && sh == size {
        return src.to_vec();
    }
    let scale = (size as f32 / sw as f32).min(size as f32 / sh as f32);
    let dw = ((sw as f32 * scale).round() as u32).max(1).min(size);
    let dh = ((sh as f32 * scale).round() as u32).max(1).min(size);

    let src_img = match image::RgbaImage::from_raw(sw, sh, src.to_vec()) {
        Some(i) => i,
        None => return vec![0u8; (size * size * 4) as usize],
    };
    let resized = image::imageops::resize(&src_img, dw, dh, image::imageops::FilterType::Lanczos3);

    let mut out = vec![0u8; (size * size * 4) as usize];
    let ox = (size - dw) / 2;
    let oy = (size - dh) / 2;
    for y in 0..dh {
        for x in 0..dw {
            let s = ((y * dw + x) * 4) as usize;
            let d = (((y + oy) * size + (x + ox)) * 4) as usize;
            out[d..d + 4].copy_from_slice(&resized.as_raw()[s..s + 4]);
        }
    }
    out
}
