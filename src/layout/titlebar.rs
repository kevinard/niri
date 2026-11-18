//! Compositor titlebars for windows that negotiated server-side decorations.
//!
//! Rendering and hit testing share [`TitlebarLayout`]. The frame is composed of solid color
//! rectangles, two small corner textures and width-independent text and button textures, so
//! resizing a window does not rasterize the whole titlebar again on every frame.
use std::cell::RefCell;

use niri_config::titlebar::{ButtonsSide, TitleAlign, Titlebar as Config, TitlebarButton};
use niri_config::{Color, CornerRadius};
use pango::{EllipsizeMode, FontDescription};
use pangocairo::cairo::{self, ImageSurface};
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture};
use smithay::backend::renderer::{ContextId, Renderer as _};
use smithay::utils::{Logical, Point, Rectangle, Size, Transform};

use crate::render_helpers::primary_gpu_texture::PrimaryGpuTextureRenderElement;
use crate::render_helpers::solid_color::{SolidColorBuffer, SolidColorRenderElement};
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::utils::{round_logical_in_physical, round_logical_in_physical_max1};

/// Longest title, in characters, that is laid out.
const MAX_TITLE_CHARS: usize = 512;
/// Horizontal space between the frame edge and the title.
const TEXT_EDGE_PADDING: f64 = 12.;
/// Horizontal space between the buttons and the title.
const TEXT_BUTTON_PADDING: f64 = 8.;
/// Ellipsized titles are laid out at multiples of this many physical pixels, so that a resize
/// animation lays out a long title again only every few pixels.
const ELLIPSIZE_STEP: i32 = 16;
/// Upper bound for any texture side, in physical pixels.
const MAX_TEXTURE_SIDE: i32 = 16384;
const MAX_CACHED_ICONS: usize = 12;
const MAX_CACHED_CORNERS: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TitlebarPart {
    Drag,
    Maximize,
    Close,
}

/// Height a titlebar reserves above a window that negotiated server-side decorations.
///
/// `draw` is the window rule override; without it, the layout configuration decides.
pub fn reserved_height(config: &Config, draw: Option<bool>, scale: f64) -> f64 {
    if draw.unwrap_or(!config.off) {
        round_logical_in_physical_max1(scale, config.height)
    } else {
        0.
    }
}

/// Visual state of a titlebar button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonState {
    Hovered,
    Pressed,
}

/// Geometry of the titlebar contents, relative to the titlebar.
#[derive(Debug, Clone, PartialEq)]
pub struct TitlebarLayout {
    pub buttons: Vec<(TitlebarPart, Rectangle<f64, Logical>)>,
    /// Space available to the title.
    pub text: Rectangle<f64, Logical>,
    pub size: Size<f64, Logical>,
}

impl TitlebarLayout {
    pub fn new(config: &Config, size: Size<f64, Logical>, can_maximize: bool) -> Self {
        let parts: Vec<_> = config
            .buttons
            .iter()
            .filter_map(|button| match button {
                TitlebarButton::Close => Some(TitlebarPart::Close),
                TitlebarButton::Maximize => can_maximize.then_some(TitlebarPart::Maximize),
            })
            .collect();

        let count = parts.len() as f64;
        let width = if parts.is_empty() {
            0.
        } else {
            size.h.min(size.w / count).max(0.)
        };
        let total = width * count;
        let start = match config.buttons_side {
            ButtonsSide::Left => 0.,
            ButtonsSide::Right => size.w - total,
        };
        let buttons = parts
            .into_iter()
            .enumerate()
            .map(|(idx, part)| {
                let loc = Point::from((start + width * idx as f64, 0.));
                (part, Rectangle::new(loc, Size::from((width, size.h))))
            })
            .collect();

        let button_padding = |present: bool| {
            if present {
                total + TEXT_BUTTON_PADDING
            } else {
                TEXT_EDGE_PADDING
            }
        };
        let (left, right) = match config.buttons_side {
            ButtonsSide::Left => (button_padding(total > 0.), TEXT_EDGE_PADDING),
            ButtonsSide::Right => (TEXT_EDGE_PADDING, button_padding(total > 0.)),
        };
        let text = Rectangle::new(
            Point::from((left, 0.)),
            Size::from(((size.w - left - right).max(0.), size.h)),
        );

        Self {
            buttons,
            text,
            size,
        }
    }

    pub fn hit(&self, point: Point<f64, Logical>) -> Option<TitlebarPart> {
        if !Rectangle::from_size(self.size).contains(point) {
            return None;
        }
        Some(
            self.buttons
                .iter()
                .find(|(_, rect)| rect.contains(point))
                .map_or(TitlebarPart::Drag, |(part, _)| *part),
        )
    }
}

/// Everything that the titlebar of one tile needs for rendering, refreshed every frame.
#[derive(Debug, Clone, PartialEq)]
pub struct FrameParams {
    pub layout: TitlebarLayout,
    pub scale: f64,
    pub radius: CornerRadius,
    pub active: bool,
    pub maximized: bool,
    pub interaction: Option<(TitlebarPart, ButtonState)>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Icon {
    Close,
    Maximize,
    Restore,
}

#[derive(Debug, Clone, PartialEq)]
struct IconKey {
    renderer: ContextId<GlesTexture>,
    icon: Icon,
    state: Option<ButtonState>,
    size: Size<i32, Logical>,
    scale: f64,
    icon_color: Color,
    hover_color: Color,
}

#[derive(Debug, Clone, PartialEq)]
struct CornerKey {
    renderer: ContextId<GlesTexture>,
    left: bool,
    /// Radius in physical pixels.
    radius: i32,
    scale: f64,
    color: Color,
}

#[derive(Debug)]
struct TextMeasure {
    title: String,
    font: String,
    scale: f64,
    natural: Size<f64, Logical>,
    texture: Option<(TextKey, Option<TextureBuffer<GlesTexture>>)>,
}

#[derive(Debug, Clone, PartialEq)]
struct TextKey {
    renderer: ContextId<GlesTexture>,
    color: Color,
    /// Ellipsization width in physical pixels; `None` lays out the whole title.
    max_width: Option<i32>,
}

/// A texture with its location and logical size, relative to the titlebar.
type PlacedTexture = (
    TextureBuffer<GlesTexture>,
    Point<f64, Logical>,
    Size<f64, Logical>,
);

pub enum TitlebarRenderElement {
    Solid(SolidColorRenderElement),
    Texture(PrimaryGpuTextureRenderElement),
}

#[derive(Debug)]
pub struct Titlebar {
    params: Option<FrameParams>,
    /// Middle column, the area below the left corner and the area below the right corner.
    background: [SolidColorBuffer; 3],
    corners: RefCell<Vec<(CornerKey, Option<TextureBuffer<GlesTexture>>)>>,
    icons: RefCell<Vec<(IconKey, Option<TextureBuffer<GlesTexture>>)>>,
    text: RefCell<Option<TextMeasure>>,
}

impl Default for Titlebar {
    fn default() -> Self {
        Self {
            params: None,
            background: std::array::from_fn(|_| SolidColorBuffer::new((0., 0.), [0., 0., 0., 0.])),
            corners: RefCell::new(Vec::new()),
            icons: RefCell::new(Vec::new()),
            text: RefCell::new(None),
        }
    }
}

fn radii(params: &FrameParams) -> (f64, f64) {
    let size = params.layout.size;
    let clamp = |radius: f32| {
        let radius = f64::from(radius).min(size.h).min(size.w / 2.).max(0.);
        round_logical_in_physical(params.scale, radius)
    };
    (
        clamp(params.radius.top_left),
        clamp(params.radius.top_right),
    )
}

impl Titlebar {
    /// Updates the geometry and state for the next render. `None` hides the titlebar.
    pub fn update(&mut self, config: &Config, params: Option<FrameParams>) {
        if let Some(params) = &params {
            let size = params.layout.size;
            let (left, right) = radii(params);
            let color = config.background_color(params.active);
            let [middle, below_left, below_right] = &mut self.background;
            middle.update(((size.w - left - right).max(0.), size.h), color);
            below_left.update((left, (size.h - left).max(0.)), color);
            below_right.update((right, (size.h - right).max(0.)), color);
        }
        self.params = params;
    }

    pub fn params(&self) -> Option<&FrameParams> {
        self.params.as_ref()
    }

    /// Pushes the titlebar elements front to back. `title` is `None` when it must be hidden.
    pub fn render(
        &self,
        renderer: &mut GlesRenderer,
        config: &Config,
        location: Point<f64, Logical>,
        alpha: f32,
        title: Option<&str>,
        push: &mut dyn FnMut(TitlebarRenderElement),
    ) {
        let Some(params) = &self.params else {
            return;
        };
        let size = params.layout.size;
        if size.w <= 0. || size.h <= 0. {
            return;
        }
        let scale = params.scale;
        let round =
            |point: Point<f64, Logical>| point.to_physical_precise_round(scale).to_logical(scale);
        let texture = |buffer: TextureBuffer<GlesTexture>, loc, size| {
            TitlebarRenderElement::Texture(PrimaryGpuTextureRenderElement(
                TextureRenderElement::from_texture_buffer(
                    buffer,
                    loc,
                    alpha,
                    None,
                    Some(size),
                    Kind::Unspecified,
                ),
            ))
        };
        let text_color = config.text_color(params.active);

        for (part, rect) in &params.layout.buttons {
            let icon = match part {
                TitlebarPart::Close => Icon::Close,
                TitlebarPart::Maximize if params.maximized => Icon::Restore,
                TitlebarPart::Maximize => Icon::Maximize,
                TitlebarPart::Drag => continue,
            };
            let state = params
                .interaction
                .filter(|(hovered, _)| hovered == part)
                .map(|(_, state)| state);
            let hover_color = if *part == TitlebarPart::Close {
                config.close_hover_color
            } else {
                config.button_hover_color
            };
            let physical = rect.size.to_physical_precise_round(scale).to_i32_round();
            if physical.w <= 0 || physical.h <= 0 {
                continue;
            }
            let key = IconKey {
                renderer: renderer.context_id(),
                icon,
                state,
                size: Size::from((physical.w, physical.h)),
                scale,
                icon_color: text_color,
                hover_color,
            };
            if let Some(buffer) = cached(&self.icons, MAX_CACHED_ICONS, key, |key| {
                rasterize_icon(renderer, key)
            }) {
                let loc = round(location + rect.loc);
                push(texture(buffer, loc, rect.size));
            }
        }

        if let Some((buffer, loc, size)) =
            title.and_then(|title| self.render_text(renderer, config, params, title, text_color))
        {
            push(texture(buffer, round(location + loc), size));
        }

        let (left, right) = radii(params);
        let color = config.background_color(params.active);
        for (is_left, radius) in [(true, left), (false, right)] {
            let physical = (radius * scale).round() as i32;
            if physical <= 0 {
                continue;
            }
            let key = CornerKey {
                renderer: renderer.context_id(),
                left: is_left,
                radius: physical,
                scale,
                color,
            };
            if let Some(buffer) = cached(&self.corners, MAX_CACHED_CORNERS, key, |key| {
                rasterize_corner(renderer, key)
            }) {
                let x = if is_left { 0. } else { size.w - radius };
                let loc = location + Point::from((x, 0.));
                push(texture(buffer, loc, Size::from((radius, radius))));
            }
        }

        let [middle, below_left, below_right] = &self.background;
        for (buffer, offset) in [
            (middle, Point::from((left, 0.))),
            (below_left, Point::from((0., left))),
            (below_right, Point::from((size.w - right, right))),
        ] {
            let buffer_size = buffer.size();
            if buffer_size.w <= 0. || buffer_size.h <= 0. {
                continue;
            }
            push(TitlebarRenderElement::Solid(
                SolidColorRenderElement::from_buffer(
                    buffer,
                    location + offset,
                    alpha,
                    Kind::Unspecified,
                ),
            ));
        }
    }

    fn render_text(
        &self,
        renderer: &mut GlesRenderer,
        config: &Config,
        params: &FrameParams,
        title: &str,
        color: Color,
    ) -> Option<PlacedTexture> {
        let area = params.layout.text;
        if area.size.w <= 0. || title.is_empty() {
            return None;
        }
        let scale = params.scale;
        let mut cache = self.text.borrow_mut();
        let stale = cache
            .as_ref()
            .is_none_or(|old| old.title != title || old.font != config.font || old.scale != scale);
        if stale {
            let natural = match measure_text(title, &config.font, scale) {
                Ok(size) => size,
                Err(err) => {
                    warn!("error measuring titlebar text: {err:#}");
                    *cache = None;
                    return None;
                }
            };
            *cache = Some(TextMeasure {
                title: title.to_owned(),
                font: config.font.clone(),
                scale,
                natural,
                texture: None,
            });
        }
        let measure = cache.as_mut().unwrap();

        let available = (area.size.w * scale).floor() as i32;
        let natural_w = (measure.natural.w * scale).ceil() as i32;
        let max_width = if natural_w <= available {
            None
        } else {
            let stepped = available / ELLIPSIZE_STEP * ELLIPSIZE_STEP;
            if stepped <= 0 {
                return None;
            }
            Some(stepped)
        };
        let key = TextKey {
            renderer: renderer.context_id(),
            color,
            max_width,
        };
        if measure.texture.as_ref().is_none_or(|(old, _)| *old != key) {
            let buffer = rasterize_text(renderer, &measure.title, &config.font, scale, &key)
                .map_err(|err| warn!("error rendering titlebar text: {err:#}"))
                .ok();
            measure.texture = Some((key, buffer));
        }
        let buffer = measure
            .texture
            .as_ref()
            .and_then(|(_, buffer)| buffer.clone())?;

        let size = buffer.logical_size();
        let x = match config.title_align {
            TitleAlign::Left => area.loc.x,
            TitleAlign::Center => {
                let centered = (params.layout.size.w - size.w) / 2.;
                let max = area.loc.x + (area.size.w - size.w).max(0.);
                centered.clamp(area.loc.x, max)
            }
        };
        let y = (params.layout.size.h - size.h) / 2.;
        Some((buffer, Point::from((x, y)), size))
    }
}

fn cached<K: PartialEq, B: Clone>(
    cache: &RefCell<Vec<(K, Option<B>)>>,
    capacity: usize,
    key: K,
    create: impl FnOnce(&K) -> anyhow::Result<B>,
) -> Option<B> {
    let mut cache = cache.borrow_mut();
    if let Some(idx) = cache.iter().position(|(old, _)| *old == key) {
        // Keep the most recently used entry last.
        let entry = cache.remove(idx);
        let buffer = entry.1.clone();
        cache.push(entry);
        return buffer;
    }
    let buffer = create(&key)
        .map_err(|err| warn!("error rendering titlebar: {err:#}"))
        .ok();
    if cache.len() >= capacity {
        cache.remove(0);
    }
    cache.push((key, buffer.clone()));
    buffer
}

fn sanitize_title(title: &str) -> String {
    title
        .chars()
        .take(MAX_TITLE_CHARS)
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

fn source(cr: &cairo::Context, color: Color) {
    cr.set_source_rgba(
        color.r.into(),
        color.g.into(),
        color.b.into(),
        color.a.into(),
    );
}

fn create_surface(size: Size<i32, Logical>) -> anyhow::Result<ImageSurface> {
    anyhow::ensure!(
        size.w > 0 && size.h > 0 && size.w <= MAX_TEXTURE_SIDE && size.h <= MAX_TEXTURE_SIDE,
        "titlebar texture size {size:?} out of bounds"
    );
    Ok(ImageSurface::create(cairo::Format::ARgb32, size.w, size.h)?)
}

fn upload(
    renderer: &mut GlesRenderer,
    surface: ImageSurface,
    scale: f64,
) -> anyhow::Result<TextureBuffer<GlesTexture>> {
    let (width, height) = (surface.width(), surface.height());
    let data = surface.take_data()?;
    Ok(TextureBuffer::from_memory(
        renderer,
        &data,
        Fourcc::Argb8888,
        (width, height),
        false,
        scale,
        Transform::Normal,
        Vec::new(),
    )?)
}

fn layout_text(cr: &cairo::Context, title: &str, font: &str) -> pango::Layout {
    let layout = pangocairo::functions::create_layout(cr);
    layout.set_font_description(Some(&FontDescription::from_string(font)));
    layout.set_single_paragraph_mode(true);
    // A title is plain text, never Pango markup.
    layout.set_text(&sanitize_title(title));
    layout
}

fn measure_text(title: &str, font: &str, scale: f64) -> anyhow::Result<Size<f64, Logical>> {
    let surface = ImageSurface::create(cairo::Format::ARgb32, 1, 1)?;
    let cr = cairo::Context::new(&surface)?;
    cr.scale(scale, scale);
    let (w, h) = layout_text(&cr, title, font).pixel_size();
    Ok(Size::from((f64::from(w), f64::from(h))))
}

fn rasterize_text(
    renderer: &mut GlesRenderer,
    title: &str,
    font: &str,
    scale: f64,
    key: &TextKey,
) -> anyhow::Result<TextureBuffer<GlesTexture>> {
    // Lay out once to find the final size, then draw on a surface of exactly that size.
    let probe = ImageSurface::create(cairo::Format::ARgb32, 1, 1)?;
    let cr = cairo::Context::new(&probe)?;
    cr.scale(scale, scale);
    let layout = layout_text(&cr, title, font);
    if let Some(max_width) = key.max_width {
        layout.set_ellipsize(EllipsizeMode::End);
        layout.set_width((f64::from(max_width) / scale * f64::from(pango::SCALE)) as i32);
    }
    let (w, h) = layout.pixel_size();
    let physical: Size<i32, Logical> = Size::from((
        (f64::from(w) * scale).ceil() as i32,
        (f64::from(h) * scale).ceil() as i32,
    ));
    let physical: Size<i32, Logical> = Size::from((
        key.max_width.map_or(physical.w, |max| physical.w.min(max)),
        physical.h,
    ));
    drop(cr);

    let surface = create_surface(physical)?;
    let cr = cairo::Context::new(&surface)?;
    cr.scale(scale, scale);
    pangocairo::functions::update_layout(&cr, &layout);
    source(&cr, key.color);
    cr.move_to(0., 0.);
    pangocairo::functions::show_layout(&cr, &layout);
    drop(cr);
    upload(renderer, surface, scale)
}

fn rasterize_corner(
    renderer: &mut GlesRenderer,
    key: &CornerKey,
) -> anyhow::Result<TextureBuffer<GlesTexture>> {
    let surface = create_surface(Size::from((key.radius, key.radius)))?;
    let cr = cairo::Context::new(&surface)?;
    let r = f64::from(key.radius);
    let center_x = if key.left { r } else { 0. };
    cr.move_to(center_x, r);
    cr.arc(center_x, r, r, -std::f64::consts::PI, 0.);
    cr.close_path();
    source(&cr, key.color);
    cr.fill()?;
    drop(cr);
    upload(renderer, surface, key.scale)
}

fn pressed(color: Color) -> Color {
    let mix = |c: f32| c * 0.8;
    Color::new_unpremul(
        mix(color.r),
        mix(color.g),
        mix(color.b),
        1. - (1. - color.a) * (1. - color.a),
    )
}

/// A readable icon color on top of a strong hover background such as the close button's.
fn contrasting(background: Color, fallback: Color) -> Color {
    if background.a < 0.5 {
        return fallback;
    }
    let luminance = 0.2126 * background.r + 0.7152 * background.g + 0.0722 * background.b;
    if luminance < 0.5 {
        Color::new_unpremul(1., 1., 1., 1.)
    } else {
        Color::new_unpremul(0., 0., 0., 1.)
    }
}

fn rasterize_icon(
    renderer: &mut GlesRenderer,
    key: &IconKey,
) -> anyhow::Result<TextureBuffer<GlesTexture>> {
    let surface = create_surface(key.size)?;
    let cr = cairo::Context::new(&surface)?;
    cr.scale(key.scale, key.scale);
    let w = f64::from(key.size.w) / key.scale;
    let h = f64::from(key.size.h) / key.scale;
    let (cx, cy) = (w / 2., h / 2.);
    let extent = w.min(h);

    let mut icon_color = key.icon_color;
    if let Some(state) = key.state {
        let background = match state {
            ButtonState::Hovered => key.hover_color,
            ButtonState::Pressed => pressed(key.hover_color),
        };
        cr.arc(cx, cy, extent * 0.375, 0., std::f64::consts::TAU);
        source(&cr, background);
        cr.fill()?;
        if key.icon == Icon::Close {
            icon_color = contrasting(background, icon_color);
        }
    }

    source(&cr, icon_color);
    cr.set_line_width(1.5);
    let icon = (extent / 3.).min(12.);
    let half = icon / 2.;
    match key.icon {
        Icon::Close => {
            cr.move_to(cx - half, cy - half);
            cr.line_to(cx + half, cy + half);
            cr.move_to(cx - half, cy + half);
            cr.line_to(cx + half, cy - half);
        }
        Icon::Maximize => cr.rectangle(cx - half, cy - half, icon, icon),
        Icon::Restore => {
            let small = icon * 0.8;
            let offset = icon - small;
            cr.rectangle(cx - half + offset, cy - half, small, small);
            cr.rectangle(cx - half, cy - half + offset, small, small);
        }
    }
    cr.stroke()?;
    drop(cr);
    upload(renderer, surface, key.scale)
}
