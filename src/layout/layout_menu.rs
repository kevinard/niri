//! The window layout menu: a flyout under a titlebar's layout button that sizes, pins, floats or
//! tabs the window's column.
//!
//! The menu is a fixed grid of tiles. Each tile shows a small diagram of the arrangement it
//! applies, so the choices read at a glance, like the window layouts of other desktops.
use std::cell::RefCell;

use niri_config::Color;
use niri_ipc::PinSide;
use pango::EllipsizeMode;
use pangocairo::cairo::{self, ImageSurface};
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture};
use smithay::backend::renderer::{ContextId, Renderer as _};
use smithay::utils::{Logical, Point, Rectangle, Size};

use super::overview_caption::{
    caption_family, caption_font, is_light, rounded_rect, set_source, soft_shadow, upload_texture,
};
use crate::render_helpers::primary_gpu_texture::PrimaryGpuTextureRenderElement;
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};

/// Space around the menu for its shadow, included in its texture.
pub const SHADOW_MARGIN: f64 = 24.;
const PADDING: f64 = 10.;
const HEADER_HEIGHT: f64 = 22.;
const ROW_GAP: f64 = 6.;
const TILE_WIDTH: f64 = 62.;
const TILE_HEIGHT: f64 = 56.;
const TILE_GAP: f64 = 4.;
const COLUMNS: usize = 4;
const RADIUS: f64 = 14.;
/// Space between the layout button and the menu.
pub const ANCHOR_GAP: f64 = 6.;

/// A choice in the layout menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LayoutMenuItem {
    /// Set the column width to this percentage of the working area.
    Width(u8),
    PinLeft,
    PinRight,
    Floating,
    Tabs,
    /// Fold the pinned column past its edge, or unfold it.
    Fold,
    /// Add the window to the scratchpad, or take it out.
    Scratchpad,
}

const ITEMS: [LayoutMenuItem; 10] = [
    LayoutMenuItem::Width(33),
    LayoutMenuItem::Width(50),
    LayoutMenuItem::Width(67),
    LayoutMenuItem::Width(100),
    LayoutMenuItem::PinLeft,
    LayoutMenuItem::PinRight,
    LayoutMenuItem::Floating,
    LayoutMenuItem::Tabs,
    LayoutMenuItem::Fold,
    LayoutMenuItem::Scratchpad,
];
const HEADERS: [&str; 3] = ["Width", "Arrange", "Hide"];

/// How the window is arranged now, which the menu marks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LayoutMenuState {
    pub floating: bool,
    pub pinned: Option<PinSide>,
    /// The column width, when it is one of the menu's widths.
    pub width: Option<u8>,
    pub tabbed: bool,
    pub in_scratchpad: bool,
}

impl LayoutMenuState {
    fn enabled(&self, item: LayoutMenuItem) -> bool {
        match item {
            LayoutMenuItem::Floating | LayoutMenuItem::Scratchpad => true,
            LayoutMenuItem::Fold => !self.floating && self.pinned.is_some(),
            _ => !self.floating,
        }
    }

    fn selected(&self, item: LayoutMenuItem) -> bool {
        match item {
            LayoutMenuItem::Width(percent) => !self.floating && self.width == Some(percent),
            LayoutMenuItem::PinLeft => self.pinned == Some(PinSide::Left),
            LayoutMenuItem::PinRight => self.pinned == Some(PinSide::Right),
            LayoutMenuItem::Floating => self.floating,
            LayoutMenuItem::Tabs => !self.floating && self.tabbed,
            LayoutMenuItem::Fold => false,
            LayoutMenuItem::Scratchpad => self.in_scratchpad,
        }
    }
}

/// The menu width for a proportional column width, if it is one of the menu's choices.
pub fn width_choice(proportion: f64) -> Option<u8> {
    [(1. / 3., 33), (0.5, 50), (2. / 3., 67), (1., 100)]
        .into_iter()
        .find(|(value, _)| (proportion - value).abs() < 0.01)
        .map(|(_, percent)| percent)
}

/// Size of the menu, without its shadow.
pub fn size() -> Size<f64, Logical> {
    let width = 2. * PADDING + COLUMNS as f64 * TILE_WIDTH + (COLUMNS - 1) as f64 * TILE_GAP;
    let rows = HEADERS.len() as f64;
    let height = 2. * PADDING + rows * (HEADER_HEIGHT + TILE_HEIGHT) + (rows - 1.) * ROW_GAP;
    Size::from((width, height))
}

/// Places the menu under `anchor`, the layout button, aligned to its trailing edge and kept
/// within `bounds`. It opens above the button when there is no room below.
pub fn place(
    anchor: Rectangle<f64, Logical>,
    bounds: Rectangle<f64, Logical>,
) -> Rectangle<f64, Logical> {
    let size = size();
    let inset = 8.;
    let mut x = anchor.loc.x + anchor.size.w - size.w + 8.;
    x = x
        .min(bounds.loc.x + bounds.size.w - inset - size.w)
        .max(bounds.loc.x + inset);
    let below = anchor.loc.y + anchor.size.h + ANCHOR_GAP;
    let y = if below + size.h <= bounds.loc.y + bounds.size.h - inset {
        below
    } else {
        (anchor.loc.y - ANCHOR_GAP - size.h).max(bounds.loc.y + inset)
    };
    Rectangle::new(Point::from((x, y)), size)
}

fn item_rect(index: usize) -> Rectangle<f64, Logical> {
    let row = index / COLUMNS;
    let column = index % COLUMNS;
    let y = PADDING + HEADER_HEIGHT + row as f64 * (TILE_HEIGHT + ROW_GAP + HEADER_HEIGHT);
    // The last row has two choices, each as wide as two tiles.
    let (x, width) = if row == 2 {
        let width = 2. * TILE_WIDTH + TILE_GAP;
        (PADDING + column as f64 * (width + TILE_GAP), width)
    } else {
        (
            PADDING + column as f64 * (TILE_WIDTH + TILE_GAP),
            TILE_WIDTH,
        )
    };
    Rectangle::new(Point::from((x, y)), Size::from((width, TILE_HEIGHT)))
}

/// The enabled item under `point`, relative to the menu.
pub fn item_at(state: &LayoutMenuState, point: Point<f64, Logical>) -> Option<LayoutMenuItem> {
    ITEMS
        .iter()
        .enumerate()
        .find(|(index, item)| state.enabled(**item) && item_rect(*index).contains(point))
        .map(|(_, item)| *item)
}

#[derive(Debug, Clone, PartialEq)]
struct Key {
    renderer: ContextId<GlesTexture>,
    state: LayoutMenuState,
    hovered: Option<LayoutMenuItem>,
    background: Color,
    ink: Color,
    accent: Color,
    scale: f64,
    family: String,
}

/// The rendered menu, redrawn when its state, hover or colors change.
#[derive(Debug, Default)]
pub struct LayoutMenuTexture {
    cache: RefCell<Option<(Key, Option<TextureBuffer<GlesTexture>>)>>,
}

impl LayoutMenuTexture {
    /// Renders the menu with its top-left corner, without the shadow, at `location`.
    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &self,
        renderer: &mut GlesRenderer,
        state: LayoutMenuState,
        hovered: Option<LayoutMenuItem>,
        background: Color,
        ink: Color,
        accent: Color,
        scale: f64,
        location: Point<f64, Logical>,
        alpha: f32,
    ) -> Option<PrimaryGpuTextureRenderElement> {
        let key = Key {
            renderer: renderer.context_id(),
            state,
            hovered,
            background,
            ink,
            accent,
            scale,
            family: caption_family(),
        };
        let mut cache = self.cache.borrow_mut();
        if cache.as_ref().is_none_or(|(old, _)| old != &key) {
            let texture = rasterize(renderer, &key)
                .map_err(|error| warn!("error rendering the layout menu: {error:#}"))
                .ok();
            *cache = Some((key, texture));
        }
        let buffer = cache.as_ref()?.1.clone()?;
        let margin = Point::from((SHADOW_MARGIN, SHADOW_MARGIN));
        let location = (location - margin)
            .to_physical_precise_round(scale)
            .to_logical(scale);
        let element = TextureRenderElement::from_texture_buffer(
            buffer,
            location,
            alpha,
            None,
            None,
            Kind::Unspecified,
        );
        Some(PrimaryGpuTextureRenderElement(element))
    }
}

fn rasterize(renderer: &mut GlesRenderer, key: &Key) -> anyhow::Result<TextureBuffer<GlesTexture>> {
    let size = size();
    let width = size.w + 2. * SHADOW_MARGIN;
    let height = size.h + 2. * SHADOW_MARGIN;
    let pixel_width = (width * key.scale).ceil() as i32;
    let pixel_height = (height * key.scale).ceil() as i32;
    let surface = ImageSurface::create(cairo::Format::ARgb32, pixel_width, pixel_height)?;
    let cr = cairo::Context::new(&surface)?;
    cr.scale(key.scale, key.scale);
    cr.translate(SHADOW_MARGIN, SHADOW_MARGIN);

    let light = is_light(key.background);
    let ink = key.ink;
    let accent = key.accent;

    soft_shadow(
        &cr,
        0.,
        0.,
        size.w,
        size.h,
        RADIUS,
        18.,
        8.,
        if light { 0.18 } else { 0.55 },
    )?;
    rounded_rect(&cr, 0.5, 0.5, size.w - 1., size.h - 1., RADIUS);
    set_source(&cr, key.background, 1.);
    cr.fill_preserve()?;
    set_source(&cr, ink, if light { 0.12 } else { 0.16 });
    cr.set_line_width(1.);
    cr.stroke()?;

    let text = pangocairo::functions::create_layout(&cr);
    text.set_single_paragraph_mode(true);
    text.set_ellipsize(EllipsizeMode::End);

    for (row, header) in HEADERS.into_iter().enumerate() {
        text.set_font_description(Some(&caption_font("Semibold", 11.)));
        text.set_width((size.w - 2. * PADDING) as i32 * pango::SCALE);
        text.set_text(header);
        let y = PADDING + row as f64 * (HEADER_HEIGHT + TILE_HEIGHT + ROW_GAP);
        let (_, h) = text.pixel_size();
        set_source(&cr, ink, 0.62);
        cr.move_to(PADDING + 4., y + (HEADER_HEIGHT - f64::from(h)) / 2. - 1.);
        pangocairo::functions::show_layout(&cr, &text);
    }

    for (index, item) in ITEMS.into_iter().enumerate() {
        let rect = item_rect(index);
        let enabled = key.state.enabled(item);
        let selected = key.state.selected(item);
        let hovered = enabled && key.hovered == Some(item);
        let dim = if enabled { 1. } else { 0.32 };

        rounded_rect(
            &cr,
            rect.loc.x + 0.5,
            rect.loc.y + 0.5,
            rect.size.w - 1.,
            rect.size.h - 1.,
            9.,
        );
        if selected {
            set_source(&cr, accent, if hovered { 0.34 } else { 0.24 });
            cr.fill_preserve()?;
            set_source(&cr, accent, 0.7);
            cr.set_line_width(1.);
            cr.stroke()?;
        } else if hovered {
            set_source(&cr, ink, if light { 0.08 } else { 0.10 });
            cr.fill()?;
        } else {
            cr.new_path();
        }

        // The diagram: a small screen showing the arrangement.
        let screen = Rectangle::new(
            Point::from((rect.loc.x + (rect.size.w - 38.) / 2., rect.loc.y + 8.)),
            Size::<f64, Logical>::from((38., 24.)),
        );
        let strong = if selected { accent } else { ink };
        draw_diagram(&cr, item, screen, strong, ink, key.background, dim)?;

        text.set_font_description(Some(&caption_font(
            if selected { "Semibold" } else { "Medium" },
            11.,
        )));
        text.set_width((rect.size.w - 4.) as i32 * pango::SCALE);
        text.set_text(label(item, &key.state));
        let (w, h) = text.pixel_size();
        set_source(&cr, ink, if selected { 1. } else { 0.86 } * dim);
        cr.move_to(
            rect.loc.x + (rect.size.w - f64::from(w)) / 2.,
            rect.loc.y + rect.size.h - 7. - f64::from(h),
        );
        pangocairo::functions::show_layout(&cr, &text);
    }

    drop(cr);
    upload_texture(renderer, surface, pixel_width, pixel_height, key.scale)
}

fn label(item: LayoutMenuItem, state: &LayoutMenuState) -> &'static str {
    match item {
        LayoutMenuItem::Width(33) => "1/3",
        LayoutMenuItem::Width(50) => "1/2",
        LayoutMenuItem::Width(67) => "2/3",
        LayoutMenuItem::Width(_) => "Full",
        LayoutMenuItem::PinLeft if state.pinned == Some(PinSide::Left) => "Unpin",
        LayoutMenuItem::PinRight if state.pinned == Some(PinSide::Right) => "Unpin",
        LayoutMenuItem::PinLeft => "Pin left",
        LayoutMenuItem::PinRight => "Pin right",
        LayoutMenuItem::Floating => "Float",
        LayoutMenuItem::Tabs => "Tabs",
        LayoutMenuItem::Fold => "Fold",
        LayoutMenuItem::Scratchpad => "Scratchpad",
    }
}

fn draw_diagram(
    cr: &cairo::Context,
    item: LayoutMenuItem,
    screen: Rectangle<f64, Logical>,
    strong: Color,
    ink: Color,
    background: Color,
    dim: f64,
) -> Result<(), cairo::Error> {
    let (x, y, w, h) = (screen.loc.x, screen.loc.y, screen.size.w, screen.size.h);
    rounded_rect(cr, x + 0.5, y + 0.5, w - 1., h - 1., 4.);
    set_source(cr, ink, 0.42 * dim);
    cr.set_line_width(1.2);
    cr.stroke()?;

    let inset = 3.;
    let (ix, iy, iw, ih) = (x + inset, y + inset, w - 2. * inset, h - 2. * inset);
    let pane = |cr: &cairo::Context, px: f64, pw: f64, color: Color, alpha: f64| {
        rounded_rect(cr, px, iy, pw.max(1.), ih, 2.);
        set_source(cr, color, alpha * dim);
        cr.fill()
    };
    match item {
        LayoutMenuItem::Width(percent) => {
            let fraction = f64::from(percent) / 100.;
            pane(cr, ix, iw * fraction, strong, 0.9)?;
        }
        LayoutMenuItem::PinLeft | LayoutMenuItem::PinRight => {
            let pinned = iw * 0.32;
            let gap = 2.;
            let rest = (iw - pinned - 2. * gap) / 2.;
            let left = item == LayoutMenuItem::PinLeft;
            let pinned_x = if left { ix } else { ix + iw - pinned };
            let rest_x = if left { ix + pinned + gap } else { ix };
            pane(cr, pinned_x, pinned, strong, 0.9)?;
            pane(cr, rest_x, rest, ink, 0.28)?;
            pane(cr, rest_x + rest + gap, rest, ink, 0.28)?;
        }
        LayoutMenuItem::Floating => {
            let half = (iw - 2.) / 2.;
            pane(cr, ix, half, ink, 0.24)?;
            pane(cr, ix + half + 2., half, ink, 0.24)?;
            let (fw, fh) = (iw * 0.56, ih * 0.66);
            let (fx, fy) = (ix + (iw - fw) / 2., iy + (ih - fh) / 2. + 1.);
            rounded_rect(cr, fx - 1.5, fy - 1.5, fw + 3., fh + 3., 3.);
            set_source(cr, background, 1.);
            cr.fill()?;
            rounded_rect(cr, fx, fy, fw, fh, 2.);
            set_source(cr, strong, 0.9 * dim);
            cr.fill()?;
        }
        LayoutMenuItem::Fold => {
            // The pinned column slides out past the left edge, leaving its tab.
            let pinned = iw * 0.3;
            cr.save()?;
            rounded_rect(cr, x + 0.5, y + 0.5, w - 1., h - 1., 4.);
            cr.clip();
            rounded_rect(cr, ix - pinned * 0.6, iy, pinned, ih, 2.);
            set_source(cr, ink, 0.22 * dim);
            cr.fill()?;
            cr.restore()?;
            rounded_rect(cr, ix - 1., iy + ih * 0.3, 2.5, ih * 0.4, 1.2);
            set_source(cr, strong, 0.9 * dim);
            cr.fill()?;
            let rest = (iw - 6.) / 2.;
            pane(cr, ix + 4., rest, strong, 0.9)?;
            pane(cr, ix + 6. + rest, rest, ink, 0.28)?;
        }
        LayoutMenuItem::Scratchpad => {
            // A window dropping in over the others from the top.
            let half = (iw - 2.) / 2.;
            pane(cr, ix, half, ink, 0.24)?;
            pane(cr, ix + half + 2., half, ink, 0.24)?;
            let (fw, fh) = (iw * 0.6, ih * 0.55);
            let (fx, fy) = (ix + (iw - fw) / 2., iy);
            rounded_rect(cr, fx - 1.5, fy - 1.5, fw + 3., fh + 3., 3.);
            set_source(cr, background, 1.);
            cr.fill()?;
            rounded_rect(cr, fx, fy, fw, fh, 2.);
            set_source(cr, strong, 0.9 * dim);
            cr.fill()?;
            cr.move_to(x + w / 2. - 3., y + h - 6.);
            cr.line_to(x + w / 2., y + h - 3.5);
            cr.line_to(x + w / 2. + 3., y + h - 6.);
            set_source(cr, ink, 0.6 * dim);
            cr.set_line_width(1.2);
            cr.stroke()?;
        }
        LayoutMenuItem::Tabs => {
            let tab_h = 4.;
            let tab_w = (iw - 4.) / 3.;
            for tab in 0..3 {
                let alpha = if tab == 0 { 0.9 } else { 0.3 };
                rounded_rect(cr, ix + tab as f64 * (tab_w + 2.), iy, tab_w, tab_h, 1.5);
                set_source(cr, if tab == 0 { strong } else { ink }, alpha * dim);
                cr.fill()?;
            }
            rounded_rect(cr, ix, iy + tab_h + 2., iw, ih - tab_h - 2., 2.);
            set_source(cr, strong, 0.6 * dim);
            cr.fill()?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floating_windows_offer_only_tiling() {
        let state = LayoutMenuState {
            floating: true,
            ..Default::default()
        };
        let enabled: Vec<_> = ITEMS.into_iter().filter(|i| state.enabled(*i)).collect();
        assert_eq!(
            enabled,
            [LayoutMenuItem::Floating, LayoutMenuItem::Scratchpad]
        );
    }

    #[test]
    fn items_are_hit_inside_their_tiles_only() {
        // Pinned, so that every item is enabled.
        let state = LayoutMenuState {
            pinned: Some(PinSide::Left),
            ..Default::default()
        };
        for (index, item) in ITEMS.into_iter().enumerate() {
            let rect = item_rect(index);
            let center = rect.loc + rect.size.downscale(2.).to_point();
            assert_eq!(item_at(&state, center), Some(item));
        }
        // The headers and the padding do not choose anything.
        assert_eq!(
            item_at(&state, Point::from((PADDING + 2., PADDING + 2.))),
            None
        );
        assert_eq!(item_at(&state, Point::from((1., 1.))), None);
    }

    #[test]
    fn menu_stays_within_bounds() {
        let bounds = Rectangle::new(Point::from((0., 0.)), Size::from((1280., 800.)));
        // Under a button near the left edge, it shifts right to fit.
        let anchor = Rectangle::new(Point::from((10., 40.)), Size::from((36., 36.)));
        let menu = place(anchor, bounds);
        assert!(menu.loc.x >= bounds.loc.x);
        assert_eq!(menu.loc.y, 40. + 36. + ANCHOR_GAP);
        // Near the bottom, it opens above the button.
        let anchor = Rectangle::new(Point::from((900., 760.)), Size::from((36., 36.)));
        let menu = place(anchor, bounds);
        assert_eq!(menu.loc.y + menu.size.h, 760. - ANCHOR_GAP);
    }

    #[test]
    fn width_choices_match_presets() {
        assert_eq!(width_choice(1. / 3.), Some(33));
        assert_eq!(width_choice(0.5), Some(50));
        assert_eq!(width_choice(2. / 3.), Some(67));
        assert_eq!(width_choice(1.), Some(100));
        assert_eq!(width_choice(0.4), None);
    }
}
