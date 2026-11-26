//! Application icons for compositor chrome: titlebars, the window bar and its previews.
//!
//! An icon is found through the application's desktop entry (`<app-id>.desktop`, `Icon=`) in
//! the XDG data directories, then looked up in the person's icon theme, the themes it
//! inherits, and `hicolor`. Icons are decoded once per name and size, at most
//! [`MAX_ICON_BYTES`] large, and kept for the life of the compositor thread.
use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use pangocairo::cairo::{self, ImageSurface};

/// Largest icon or desktop file read from disk.
const MAX_ICON_BYTES: u64 = 2 * 1024 * 1024;
const MAX_CACHED: usize = 256;
const SIZES: [&str; 9] = [
    "scalable", "256x256", "128x128", "96x96", "64x64", "48x48", "32x32", "24x24", "16x16",
];

thread_local! {
    static CACHE: RefCell<HashMap<(String, i32), Option<ImageSurface>>> =
        RefCell::new(HashMap::new());
    static PATHS: RefCell<HashMap<String, Option<PathBuf>>> = RefCell::new(HashMap::new());
}

/// The icon of the application with this Wayland app ID as a square premultiplied ARGB
/// surface `size` physical pixels wide, if one can be found.
pub fn icon(app_id: &str, size: i32) -> Option<ImageSurface> {
    if app_id.is_empty() || !(1..=512).contains(&size) {
        return None;
    }
    let key = (app_id.to_owned(), size);
    if let Some(cached) = CACHE.with(|cache| cache.borrow().get(&key).cloned()) {
        return cached;
    }
    let surface = icon_path(app_id).and_then(|path| {
        load(&path, size)
            .map_err(|err| debug!("error loading icon {path:?}: {err:#}"))
            .ok()
    });
    CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache.len() >= MAX_CACHED {
            cache.clear();
        }
        cache.insert(key, surface.clone());
    });
    surface
}

/// Paints the icon of `app_id` into a square at `(x, y)` of `size` logical pixels.
pub fn paint(cr: &cairo::Context, app_id: &str, x: f64, y: f64, size: f64, scale: f64) -> bool {
    let physical = (size * scale).round() as i32;
    let Some(surface) = icon(app_id, physical) else {
        return false;
    };
    let _ = cr.save();
    cr.translate(x, y);
    cr.scale(size / f64::from(physical), size / f64::from(physical));
    let painted = cr
        .set_source_surface(&surface, 0., 0.)
        .and_then(|()| cr.paint())
        .is_ok();
    let _ = cr.restore();
    painted
}

fn icon_path(app_id: &str) -> Option<PathBuf> {
    if let Some(path) = PATHS.with(|paths| paths.borrow().get(app_id).cloned()) {
        return path;
    }
    let name = desktop_icon(app_id).unwrap_or_else(|| app_id.to_owned());
    let path = if Path::new(&name).is_absolute() {
        Some(PathBuf::from(&name)).filter(|path| path.is_file())
    } else {
        find_in_themes(&name)
            .or_else(|| find_in_themes(&app_id.to_lowercase()))
            .or_else(|| {
                // Reverse-DNS IDs often name their icon after the last component.
                let short = app_id.rsplit('.').next()?.to_lowercase();
                (short != app_id).then(|| find_in_themes(&short)).flatten()
            })
    };
    PATHS.with(|paths| paths.borrow_mut().insert(app_id.to_owned(), path.clone()));
    path
}

fn data_dirs() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute());
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| home.as_ref().map(|home| home.join(".local/share")));
    let dirs = std::env::var_os("XDG_DATA_DIRS")
        .filter(|dirs| !dirs.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    data_home
        .into_iter()
        .chain(std::env::split_paths(&dirs).filter(|path| path.is_absolute()))
        .take(32)
        .collect()
}

fn read_text(path: &Path) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let mut text = String::new();
    file.take(MAX_ICON_BYTES).read_to_string(&mut text).ok()?;
    Some(text)
}

fn desktop_icon(app_id: &str) -> Option<String> {
    if app_id.contains('/') || app_id.starts_with('.') {
        return None;
    }
    let names = [
        format!("{app_id}.desktop"),
        format!("{}.desktop", app_id.to_lowercase()),
    ];
    data_dirs().iter().find_map(|dir| {
        names.iter().find_map(|name| {
            let text = read_text(&dir.join("applications").join(name))?;
            let mut in_entry = false;
            text.lines().map(str::trim).find_map(|line| {
                if line.starts_with('[') {
                    in_entry = line == "[Desktop Entry]";
                    return None;
                }
                let value = line.strip_prefix("Icon=").filter(|_| in_entry)?.trim();
                (!value.is_empty()).then(|| value.to_owned())
            })
        })
    })
}

fn user_theme() -> Option<String> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| home.join(".config"));
    ["gtk-4.0", "gtk-3.0"].iter().find_map(|toolkit| {
        let text = read_text(&config.join(toolkit).join("settings.ini"))?;
        text.lines().map(str::trim).find_map(|line| {
            let (key, value) = line.split_once('=')?;
            (key.trim() == "gtk-icon-theme-name").then(|| value.trim().trim_matches('"').to_owned())
        })
    })
}

fn themes() -> Vec<String> {
    let mut themes = Vec::new();
    let mut push = |name: &str| {
        let valid = !name.is_empty()
            && !name.contains(['/', '\\'])
            && !matches!(name, "." | "..")
            && !themes.iter().any(|theme| theme == name);
        if valid {
            themes.push(name.to_owned());
        }
    };
    if let Some(theme) = user_theme() {
        push(&theme);
        // One level of inheritance covers the common themes.
        for dir in data_dirs() {
            if let Some(text) = read_text(&dir.join("icons").join(&theme).join("index.theme")) {
                if let Some(parents) = text
                    .lines()
                    .find_map(|line| line.trim().strip_prefix("Inherits="))
                {
                    for parent in parents.split(',') {
                        push(parent.trim());
                    }
                }
                break;
            }
        }
    }
    // The same fallbacks as GTK: its default theme, then hicolor.
    for theme in ["Adwaita", "hicolor"] {
        push(theme);
    }
    themes
}

fn find_in_themes(name: &str) -> Option<PathBuf> {
    if name.is_empty() || name.contains('/') || name.starts_with('.') {
        return None;
    }
    let dirs = data_dirs();
    let home_icons = std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".icons"));
    let roots: Vec<PathBuf> = home_icons
        .into_iter()
        .chain(dirs.iter().map(|dir| dir.join("icons")))
        .collect();
    for theme in themes() {
        for root in &roots {
            let theme_dir = root.join(&theme);
            if !theme_dir.is_dir() {
                continue;
            }
            for size in SIZES {
                for category in ["apps", "places", "devices", "mimetypes"] {
                    for (sub, ext) in [
                        (format!("{size}/{category}"), "svg"),
                        (format!("{size}/{category}"), "png"),
                        (format!("{category}/{size}"), "svg"),
                        (format!("{category}/{size}"), "png"),
                    ] {
                        let path = theme_dir.join(&sub).join(format!("{name}.{ext}"));
                        if path.is_file() {
                            return Some(path);
                        }
                    }
                }
            }
        }
    }
    dirs.iter().find_map(|dir| {
        ["svg", "png"].iter().find_map(|ext| {
            let path = dir.join("pixmaps").join(format!("{name}.{ext}"));
            path.is_file().then_some(path)
        })
    })
}

fn read_bytes(path: &Path) -> anyhow::Result<Vec<u8>> {
    let file = std::fs::File::open(path)?;
    anyhow::ensure!(file.metadata()?.len() <= MAX_ICON_BYTES, "icon too large");
    let mut bytes = Vec::new();
    file.take(MAX_ICON_BYTES).read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn load(path: &Path, size: i32) -> anyhow::Result<ImageSurface> {
    let bytes = read_bytes(path)?;
    let is_svg = path.extension().is_some_and(|ext| ext == "svg");
    let (width, height, rgba) = if is_svg {
        let tree = resvg::usvg::Tree::from_data(&bytes, &resvg::usvg::Options::default())?;
        let mut pixmap = resvg::tiny_skia::Pixmap::new(size as u32, size as u32)
            .ok_or_else(|| anyhow::anyhow!("invalid icon size"))?;
        let view = tree.size();
        let fit = size as f32 / view.width().max(view.height());
        let dx = (size as f32 - view.width() * fit) / 2.;
        let dy = (size as f32 - view.height() * fit) / 2.;
        let transform = resvg::tiny_skia::Transform::from_scale(fit, fit).post_translate(dx, dy);
        resvg::render(&tree, transform, &mut pixmap.as_mut());
        // tiny-skia pixels are premultiplied RGBA.
        (size, size, pixmap.take())
    } else {
        let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
        decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
        let mut reader = decoder.read_info()?;
        let mut buf = vec![0; reader.output_buffer_size().unwrap_or(0)];
        let info = reader.next_frame(&mut buf)?;
        anyhow::ensure!(info.width <= 1024 && info.height <= 1024, "icon too large");
        let pixels = &buf[..info.buffer_size()];
        let rgba: Vec<u8> = match info.color_type {
            png::ColorType::Rgba => pixels.to_vec(),
            png::ColorType::Rgb => pixels
                .chunks(3)
                .flat_map(|p| [p[0], p[1], p[2], 255])
                .collect(),
            png::ColorType::GrayscaleAlpha => pixels
                .chunks(2)
                .flat_map(|p| [p[0], p[0], p[0], p[1]])
                .collect(),
            png::ColorType::Grayscale => pixels.iter().flat_map(|&g| [g, g, g, 255]).collect(),
            png::ColorType::Indexed => anyhow::bail!("unexpanded palette"),
        };
        let premultiplied = rgba
            .chunks(4)
            .flat_map(|p| {
                let a = u16::from(p[3]);
                let mul = |c: u8| ((u16::from(c) * a + 127) / 255) as u8;
                [mul(p[0]), mul(p[1]), mul(p[2]), p[3]]
            })
            .collect();
        (info.width as i32, info.height as i32, premultiplied)
    };

    let mut source = ImageSurface::create(cairo::Format::ARgb32, width, height)?;
    let stride = source.stride() as usize;
    {
        let mut data = source.data()?;
        for (row, pixels) in rgba.chunks(width as usize * 4).enumerate() {
            for (col, p) in pixels.chunks(4).enumerate() {
                let at = row * stride + col * 4;
                // Cairo stores premultiplied ARGB as native-endian 32-bit words.
                let word = u32::from_be_bytes([p[3], p[0], p[1], p[2]]);
                data[at..at + 4].copy_from_slice(&word.to_ne_bytes());
            }
        }
    }
    if width == size && height == size {
        return Ok(source);
    }
    let target = ImageSurface::create(cairo::Format::ARgb32, size, size)?;
    let cr = cairo::Context::new(&target)?;
    let fit = f64::from(size) / f64::from(width.max(height));
    cr.translate(
        (f64::from(size) - f64::from(width) * fit) / 2.,
        (f64::from(size) - f64::from(height) * fit) / 2.,
    );
    cr.scale(fit, fit);
    cr.set_source_surface(&source, 0., 0.)?;
    cr.source().set_filter(cairo::Filter::Good);
    cr.paint()?;
    drop(cr);
    Ok(target)
}

/// Whether the visible pixels of `icon` are mostly near black.
pub fn is_dark(icon: &ImageSurface) -> bool {
    // Read a private copy: the shared surface cannot lend its pixels.
    let (width, height) = (icon.width(), icon.height());
    let Ok(mut copy) = ImageSurface::create(cairo::Format::ARgb32, width, height) else {
        return false;
    };
    {
        let Ok(cr) = cairo::Context::new(&copy) else {
            return false;
        };
        if cr.set_source_surface(icon, 0., 0.).is_err() || cr.paint().is_err() {
            return false;
        }
    }
    copy.flush();
    let stride = copy.stride();
    let Ok(data) = copy.data() else {
        return false;
    };
    let (mut weight, mut light) = (0., 0.);
    for y in 0..height as usize {
        for x in 0..width as usize {
            let offset = y * stride as usize + x * 4;
            let Some(pixel) = data.get(offset..offset + 4) else {
                continue;
            };
            // Premultiplied BGRA.
            let a = f64::from(pixel[3]) / 255.;
            if a < 0.5 {
                continue;
            }
            let (b, g, r) = (
                f64::from(pixel[0]) / 255. / a,
                f64::from(pixel[1]) / 255. / a,
                f64::from(pixel[2]) / 255. / a,
            );
            weight += a;
            light += a * (0.2126 * r + 0.7152 * g + 0.0722 * b);
        }
    }
    weight > 0. && light / weight < 0.22
}
