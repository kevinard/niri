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
/// Size of the application icon before the title.
const ICON_SIZE: f64 = 16.;
/// Horizontal space between the application icon and the title.
const ICON_PADDING: f64 = 8.;
/// Ellipsized titles are laid out at multiples of this many physical pixels, so that a resize
/// animation lays out a long title again only every few pixels.
const ELLIPSIZE_STEP: i32 = 16;
/// Upper bound for any texture side, in physical pixels.
const MAX_TEXTURE_SIDE: i32 = 16384;
const MAX_CACHED_ICONS: usize = 12;
const MAX_CACHED_CORNERS: usize = 4;
const MAX_CACHED_TABS: usize = 16;
/// Space between neighboring tabs.
const TAB_GAP: f64 = 4.;
/// Space above and below the tabs.
const TAB_INSET: f64 = 5.;
const TAB_MIN_WIDTH: f64 = 120.;
const TAB_MAX_WIDTH: f64 = 240.;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TitlebarPart {
    Drag,
    /// Opens the window layout menu.
    Layout,
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
    /// Whether the application icon precedes the title.
    pub icon: bool,
}

impl TitlebarLayout {
    pub fn new(config: &Config, size: Size<f64, Logical>, can_maximize: bool) -> Self {
        let parts: Vec<_> = config
            .buttons
            .iter()
            .filter_map(|button| match button {
                TitlebarButton::Close => Some(TitlebarPart::Close),
                TitlebarButton::Layout => Some(TitlebarPart::Layout),
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
            icon: false,
        }
    }

    /// The tabs of a tabbed column, laid out from the leading edge of the title space. Each
    /// tab is sized to its label (`natural`) and they shrink together when space runs out.
    pub fn tab_rects(&self, natural: &[f64]) -> Vec<Rectangle<f64, Logical>> {
        let count = natural.len();
        if count < 2 {
            return Vec::new();
        }
        let area = self.text;
        let height = (area.size.h - 2. * TAB_INSET).max(1.);
        let available = area.size.w - TAB_GAP * (count - 1) as f64;
        let widths: Vec<f64> = natural
            .iter()
            .map(|w| w.clamp(TAB_MIN_WIDTH, TAB_MAX_WIDTH))
            .collect();
        let total: f64 = widths.iter().sum();
        let widths: Vec<f64> = if total <= available {
            widths
        } else {
            // Shrink the part above the minimum first, then share the space evenly.
            let slack: f64 = widths.iter().map(|w| w - TAB_MIN_WIDTH).sum();
            let excess = total - available;
            if slack >= excess && slack > 0. {
                let factor = 1. - excess / slack;
                widths
                    .iter()
                    .map(|w| TAB_MIN_WIDTH + (w - TAB_MIN_WIDTH) * factor)
                    .collect()
            } else {
                vec![available / count as f64; count]
            }
        };
        if widths.iter().any(|w| *w < 24.) {
            return Vec::new();
        }
        let mut x = area.loc.x;
        widths
            .into_iter()
            .map(|width| {
                let rect = Rectangle::new(
                    Point::from((x, area.loc.y + TAB_INSET)),
                    Size::from((width, height)),
                );
                x += width + TAB_GAP;
                rect
            })
            .collect()
    }

    /// Moves the icon and title right so that they start at least `x` into the titlebar.
    pub fn keep_text_after(&mut self, x: f64) {
        let icon = if self.icon {
            ICON_SIZE + ICON_PADDING
        } else {
            0.
        };
        let start = x + TEXT_EDGE_PADDING + icon;
        if x <= 0. || start <= self.text.loc.x {
            return;
        }
        let end = self.text.loc.x + self.text.size.w;
        if end - start < 48. {
            // Too little room left; let the title scroll away with the window.
            return;
        }
        self.text.loc.x = start;
        self.text.size.w = end - start;
    }

    /// Makes room for the application icon at the start of the title.
    pub fn with_icon(mut self) -> Self {
        let shift = ICON_SIZE + ICON_PADDING;
        if self.text.size.w > shift + 24. {
            self.text.loc.x += shift;
            self.text.size.w -= shift;
            self.icon = true;
        }
        self
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
/// How the window is arranged, which the layout button's icon shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TitlebarBadge {
    #[default]
    None,
    /// Its column is pinned to an edge.
    Pinned,
    Floating,
    /// It belongs to the scratchpad.
    Scratchpad,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FrameParams {
    pub layout: TitlebarLayout,
    /// The window's arrangement, shown by the layout button.
    pub badge: TitlebarBadge,
    pub scale: f64,
    pub radius: CornerRadius,
    pub active: bool,
    pub maximized: bool,
    pub interaction: Option<(TitlebarPart, ButtonState)>,
    /// The windows of a tabbed column, shown as tabs in place of the title.
    pub tabs: Option<TitlebarTabs>,
    /// The application ID, for the icon.
    pub app_id: String,
    /// The focus accent, which marks the shown tab.
    pub accent: Color,
}

/// The titles of the windows in a tabbed column and the index of the shown one.
#[derive(Debug, Clone, PartialEq)]
pub struct TitlebarTabs {
    pub titles: Vec<String>,
    pub app_ids: Vec<String>,
    pub active: usize,
}

/// Horizontal padding inside a titlebar tab.
const TAB_PADDING: f64 = 12.;
/// Size of a titlebar tab's icon and the space after it.
const TAB_ICON_SIZE: f64 = 14.;
const TAB_ICON_GAP: f64 = 6.;

impl TitlebarTabs {
    /// The width each tab would like: its icon and full label with padding.
    pub fn natural_widths(&self, font: &str) -> Vec<f64> {
        thread_local! {
            static CACHE: RefCell<std::collections::HashMap<(String, String), f64>> =
                RefCell::new(std::collections::HashMap::new());
        }
        self.titles
            .iter()
            .enumerate()
            .map(|(idx, title)| {
                let text = CACHE.with(|cache| {
                    let mut cache = cache.borrow_mut();
                    if cache.len() > 512 {
                        cache.clear();
                    }
                    *cache
                        .entry((title.clone(), font.to_owned()))
                        .or_insert_with(|| measure_text(title, font, 1.).map_or(0., |s| s.w))
                });
                let icon = self
                    .app_ids
                    .get(idx)
                    .is_some_and(|id| !id.is_empty())
                    .then_some(TAB_ICON_SIZE + TAB_ICON_GAP)
                    .unwrap_or(0.);
                (text + icon + 2. * TAB_PADDING).ceil()
            })
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq)]
struct TabKey {
    renderer: ContextId<GlesTexture>,
    title: String,
    app_id: String,
    font: String,
    active: bool,
    /// Size in physical pixels.
    size: Size<i32, Logical>,
    scale: f64,
    text_color: Color,
    fill: Color,
    accent: Color,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Icon {
    Close,
    Layout,
    Pin,
    Floating,
    Scratchpad,
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
    /// A hairline between the titlebar and the window body.
    divider: SolidColorBuffer,
    corners: RefCell<Vec<(CornerKey, Option<TextureBuffer<GlesTexture>>)>>,
    icons: RefCell<Vec<(IconKey, Option<TextureBuffer<GlesTexture>>)>>,
    text: RefCell<Option<TextMeasure>>,
    tabs: RefCell<Vec<(TabKey, Option<TextureBuffer<GlesTexture>>)>>,
    app_icon: RefCell<Option<(AppIconKey, Option<TextureBuffer<GlesTexture>>)>>,
}

#[derive(Debug, Clone, PartialEq)]
struct AppIconKey {
    renderer: ContextId<GlesTexture>,
    app_id: String,
    size: i32,
    scale: f64,
    alpha: u8,
}

impl Default for Titlebar {
    fn default() -> Self {
        Self {
            params: None,
            background: std::array::from_fn(|_| SolidColorBuffer::new((0., 0.), [0., 0., 0., 0.])),
            divider: SolidColorBuffer::new((0., 0.), [0., 0., 0., 0.]),
            corners: RefCell::new(Vec::new()),
            icons: RefCell::new(Vec::new()),
            text: RefCell::new(None),
            tabs: RefCell::new(Vec::new()),
            app_icon: RefCell::new(None),
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
            let ink = config.text_color(true);
            let line = 1. / params.scale;
            self.divider.update(
                (size.w, line),
                Color::new_unpremul(ink.r, ink.g, ink.b, 0.10),
            );
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
                TitlebarPart::Layout => match params.badge {
                    TitlebarBadge::None => Icon::Layout,
                    TitlebarBadge::Pinned => Icon::Pin,
                    TitlebarBadge::Floating => Icon::Floating,
                    TitlebarBadge::Scratchpad => Icon::Scratchpad,
                },
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

        let tab_rects = params
            .tabs
            .as_ref()
            .map(|tabs| params.layout.tab_rects(&tabs.natural_widths(&config.font)))
            .unwrap_or_default();
        if let (Some(tabs), false, Some(_)) = (&params.tabs, tab_rects.is_empty(), title) {
            for (idx, (rect, tab_title)) in tab_rects.iter().zip(&tabs.titles).enumerate() {
                let active = idx == tabs.active;
                let physical = rect.size.to_physical_precise_round(scale).to_i32_round();
                if physical.w <= 0 || physical.h <= 0 {
                    continue;
                }
                // Other tabs stay readable while their column has focus.
                let inactive_text = if params.active {
                    Color {
                        a: text_color.a * 0.72,
                        ..text_color
                    }
                } else {
                    config.text_color(false)
                };
                let key = TabKey {
                    renderer: renderer.context_id(),
                    title: sanitize_title(tab_title),
                    app_id: tabs.app_ids.get(idx).cloned().unwrap_or_default(),
                    font: config.font.clone(),
                    active,
                    size: Size::from((physical.w, physical.h)),
                    scale,
                    text_color: if active { text_color } else { inactive_text },
                    fill: mix(config.background_color(params.active), text_color, 0.16),
                    accent: params.accent,
                };
                if let Some(buffer) = cached(&self.tabs, MAX_CACHED_TABS, key, |key| {
                    rasterize_tab(renderer, key)
                }) {
                    push(texture(buffer, round(location + rect.loc), rect.size));
                }
            }
        } else {
            if let (true, Some(_)) = (params.layout.icon, title) {
                let physical = (ICON_SIZE * scale).round() as i32;
                // Unfocused windows show their icon with the title's transparency.
                let alpha = (text_color.a * 255.).round() as u8;
                let key = AppIconKey {
                    renderer: renderer.context_id(),
                    app_id: params.app_id.clone(),
                    size: physical,
                    scale,
                    alpha,
                };
                let mut cache = self.app_icon.borrow_mut();
                if cache.as_ref().is_none_or(|(old, _)| *old != key) {
                    let buffer = rasterize_app_icon(renderer, &key)
                        .map_err(|err| debug!("error rendering titlebar icon: {err:#}"))
                        .ok();
                    *cache = Some((key, buffer));
                }
                if let Some(buffer) = cache.as_ref().and_then(|(_, buffer)| buffer.clone()) {
                    let x = params.layout.text.loc.x - ICON_SIZE - ICON_PADDING;
                    let y = (size.h - ICON_SIZE) / 2.;
                    push(texture(
                        buffer,
                        round(location + Point::from((x, y))),
                        Size::from((ICON_SIZE, ICON_SIZE)),
                    ));
                }
            }
            if let Some((buffer, loc, size)) = title
                .and_then(|title| self.render_text(renderer, config, params, title, text_color))
            {
                push(texture(buffer, round(location + loc), size));
            }
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

        if self.divider.size().w > 0. {
            push(TitlebarRenderElement::Solid(
                SolidColorRenderElement::from_buffer(
                    &self.divider,
                    location + Point::from((0., size.h - self.divider.size().h)),
                    alpha,
                    Kind::Unspecified,
                ),
            ));
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

fn mix(base: Color, toward: Color, amount: f32) -> Color {
    let mix = |a: f32, b: f32| a + (b - a) * amount;
    Color::new_unpremul(
        mix(base.r, toward.r),
        mix(base.g, toward.g),
        mix(base.b, toward.b),
        1.,
    )
}

fn rasterize_app_icon(
    renderer: &mut GlesRenderer,
    key: &AppIconKey,
) -> anyhow::Result<TextureBuffer<GlesTexture>> {
    let icon =
        crate::app_icons::icon(&key.app_id, key.size).ok_or_else(|| anyhow::anyhow!("no icon"))?;
    let surface = create_surface(Size::from((key.size, key.size)))?;
    let cr = cairo::Context::new(&surface)?;
    // A nearly black icon gets a faint light rim, so it does not vanish on a dark titlebar;
    // on a light one the rim is invisible.
    if crate::app_icons::is_dark(&icon) {
        let alpha = 0.28 * f64::from(key.alpha) / 255.;
        cr.set_source_rgba(1., 1., 1., alpha);
        for (dx, dy) in [(-1., 0.), (1., 0.), (0., -1.), (0., 1.)] {
            cr.mask_surface(&icon, dx, dy)?;
        }
        cr.set_operator(cairo::Operator::Source);
        cr.set_source_rgba(0., 0., 0., 0.);
        cr.mask_surface(&icon, 0., 0.)?;
        cr.set_operator(cairo::Operator::Over);
    }
    cr.set_source_surface(&icon, 0., 0.)?;
    cr.paint_with_alpha(f64::from(key.alpha) / 255.)?;
    drop(cr);
    upload(renderer, surface, key.scale)
}

fn rasterize_tab(
    renderer: &mut GlesRenderer,
    key: &TabKey,
) -> anyhow::Result<TextureBuffer<GlesTexture>> {
    let surface = create_surface(key.size)?;
    let cr = cairo::Context::new(&surface)?;
    cr.scale(key.scale, key.scale);
    let w = f64::from(key.size.w) / key.scale;
    let h = f64::from(key.size.h) / key.scale;
    // The shown tab sits on an accent-tinted fill; the others stay plain, on the same
    // baseline.
    if key.active {
        let r = (h / 2.).min(8.);
        cr.new_sub_path();
        cr.arc(w - r, r, r, -std::f64::consts::FRAC_PI_2, 0.);
        cr.arc(w - r, h - r, r, 0., std::f64::consts::FRAC_PI_2);
        cr.arc(
            r,
            h - r,
            r,
            std::f64::consts::FRAC_PI_2,
            std::f64::consts::PI,
        );
        cr.arc(r, r, r, std::f64::consts::PI, 1.5 * std::f64::consts::PI);
        cr.close_path();
        cr.set_source_rgba(
            key.accent.r.into(),
            key.accent.g.into(),
            key.accent.b.into(),
            // A pale accent (dark themes) needs less to show, and keeps the label readable.
            if super::overview_caption::is_light(key.accent) {
                0.24
            } else {
                0.28
            },
        );
        cr.fill()?;
    }
    let layout = pangocairo::functions::create_layout(&cr);
    let mut font = FontDescription::from_string(&key.font);
    // The shown tab is set in semibold, the others in regular weight.
    font.set_weight(if key.active {
        pango::Weight::Semibold
    } else {
        pango::Weight::Normal
    });
    layout.set_font_description(Some(&font));
    layout.set_single_paragraph_mode(true);
    layout.set_ellipsize(EllipsizeMode::End);
    layout.set_text(&key.title);
    let padding = TAB_PADDING;
    let icon_size = TAB_ICON_SIZE;
    let has_icon = !key.app_id.is_empty()
        && crate::app_icons::icon(&key.app_id, (icon_size * key.scale).round() as i32).is_some();
    let icon_space = if has_icon {
        icon_size + TAB_ICON_GAP
    } else {
        0.
    };
    layout.set_width((((w - 2. * padding - icon_space).max(1.)) * f64::from(pango::SCALE)) as i32);
    let (text_w, text_h) = layout.pixel_size();
    let x = ((w - f64::from(text_w) - icon_space) / 2.).max(padding);
    if has_icon {
        cr.push_group();
        crate::app_icons::paint(
            &cr,
            &key.app_id,
            x,
            (h - icon_size) / 2.,
            icon_size,
            key.scale,
        );
        cr.pop_group_to_source()?;
        cr.paint_with_alpha(key.text_color.a.into())?;
    }
    source(&cr, key.text_color);
    cr.move_to(x + icon_space, (h - f64::from(text_h)) / 2.);
    pangocairo::functions::show_layout(&cr, &layout);
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

fn rounded_square(cr: &cairo::Context, x: f64, y: f64, side: f64, r: f64) {
    use std::f64::consts::{FRAC_PI_2, PI};
    cr.new_sub_path();
    cr.arc(x + side - r, y + r, r, -FRAC_PI_2, 0.);
    cr.arc(x + side - r, y + side - r, r, 0., FRAC_PI_2);
    cr.arc(x + r, y + side - r, r, FRAC_PI_2, PI);
    cr.arc(x + r, y + r, r, PI, 1.5 * PI);
    cr.close_path();
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
        cr.arc(cx, cy, (extent * 0.36).min(13.), 0., std::f64::consts::TAU);
        source(&cr, background);
        cr.fill()?;
        if key.icon == Icon::Close {
            icon_color = contrasting(background, icon_color);
        }
    }

    source(&cr, icon_color);
    cr.set_line_width(1.5);
    cr.set_line_cap(cairo::LineCap::Round);
    cr.set_line_join(cairo::LineJoin::Round);
    // Large enough to read as a control at a glance, inside its hover disc.
    let icon = (extent * 0.36).min(13.5);
    let half = icon / 2.;
    match key.icon {
        Icon::Close => {
            // The cross spans the same box as the square beside it.
            let half = half * 0.9;
            cr.move_to(cx - half, cy - half);
            cr.line_to(cx + half, cy + half);
            cr.move_to(cx - half, cy + half);
            cr.line_to(cx + half, cy - half);
        }
        // An outlined square reads larger than a cross of the same size; draw it smaller,
        // with softened corners.
        Icon::Maximize => {
            let side = icon * 0.94;
            rounded_square(&cr, cx - side / 2., cy - side / 2., side, 1.5);
        }
        // A window split into a wide pane and two stacked ones, as in the menu it opens.
        Icon::Layout => {
            let side = icon * 0.94;
            let (x, y) = (cx - side / 2., cy - side / 2.);
            rounded_square(&cr, x, y, side, 1.5);
            let split = (x + side * 0.45).round() + 0.5 / key.scale;
            cr.move_to(split, y);
            cr.line_to(split, y + side);
            cr.move_to(split, cy);
            cr.line_to(x + side, cy);
        }
        // A push pin: its head, the collar and the point, for a pinned column.
        Icon::Pin => {
            let top = cy - half;
            cr.move_to(cx - half * 0.45, top + 0.5);
            cr.line_to(cx + half * 0.45, top + 0.5);
            cr.move_to(cx - half * 0.3, top + 0.5);
            cr.line_to(cx - half * 0.3, cy + half * 0.05);
            cr.move_to(cx + half * 0.3, top + 0.5);
            cr.line_to(cx + half * 0.3, cy + half * 0.05);
            cr.move_to(cx - half * 0.75, cy + half * 0.1);
            cr.line_to(cx + half * 0.75, cy + half * 0.1);
            cr.move_to(cx, cy + half * 0.1);
            cr.line_to(cx, cy + half);
        }
        // A window floating over another, for a floating window.
        Icon::Floating => {
            let side = icon * 0.94;
            let (x, y) = (cx - side / 2., cy - side / 2.);
            let small = side * 0.62;
            cr.move_to(x + small * 0.4, y + side);
            cr.line_to(x, y + side);
            cr.line_to(x, y + side - small * 0.6);
            rounded_square(&cr, x + side - small, y, small, 1.5);
        }
        // A window dropping down from the top edge, for a scratchpad window.
        Icon::Scratchpad => {
            let side = icon * 0.94;
            let (x, y) = (cx - side / 2., cy - side / 2.);
            cr.move_to(x, y + 0.5);
            cr.line_to(x + side, y + 0.5);
            let inner = side * 0.72;
            rounded_square(&cr, cx - inner / 2., y + side - inner, inner, 1.5);
        }
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
