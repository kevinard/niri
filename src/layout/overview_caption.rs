use std::cell::RefCell;

use niri_config::{Color, CornerRadius};
use pango::{EllipsizeMode, FontDescription};
use pangocairo::cairo::{self, ImageSurface};
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture};
use smithay::backend::renderer::{ContextId, Renderer};
use smithay::utils::{Logical, Point, Rectangle, Transform};

use super::focus_ring::FocusRingRenderElement;
use crate::render_helpers::border::BorderRenderElement;
use crate::render_helpers::primary_gpu_texture::PrimaryGpuTextureRenderElement;
use crate::render_helpers::renderer::NiriRenderer;
use crate::render_helpers::solid_color::{SolidColorBuffer, SolidColorRenderElement};
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};

const HEIGHT: f64 = 28.;
const PADDING: f64 = 10.;
const TRACK_WIDTH: f64 = 72.;

#[derive(Debug)]
pub(super) struct OverviewCaption {
    texture: RefCell<Option<(Key, Option<TextureBuffer<GlesTexture>>)>>,
    frame: RefCell<[BorderRenderElement; 2]>,
    fallback_fill: RefCell<SolidColorBuffer>,
}

impl Default for OverviewCaption {
    fn default() -> Self {
        Self {
            texture: RefCell::default(),
            frame: RefCell::new(std::array::from_fn(|_| BorderRenderElement::empty())),
            fallback_fill: RefCell::default(),
        }
    }
}

#[derive(Debug, PartialEq)]
struct Key {
    renderer: ContextId<GlesTexture>,
    text: String,
    background: Color,
    ink: Color,
    viewport: Option<(u8, u8)>,
    width: u32,
    scale: f64,
}

impl OverviewCaption {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn render(
        &self,
        renderer: &mut GlesRenderer,
        index: usize,
        name: Option<&str>,
        empty: bool,
        active: bool,
        accent: Color,
        backdrop: Color,
        viewport: Option<(u8, u8)>,
        width: f64,
        scale: f64,
        location: Point<f64, Logical>,
        alpha: f32,
    ) -> Option<PrimaryGpuTextureRenderElement> {
        // Round the width down in steps so a zoom redraw does not repeatedly
        // rasterize the same short caption.
        let viewport = viewport.filter(|_| width >= 255.);
        let background = if active {
            Color::new_unpremul(accent.r, accent.g, accent.b, 1.)
        } else if is_light(backdrop) {
            Color::new_unpremul(0.92, 0.95, 0.93, 0.97)
        } else {
            Color::new_unpremul(0.08, 0.14, 0.15, 0.96)
        };
        let ink = if is_light(background) {
            Color::new_unpremul(0.07, 0.16, 0.13, 1.)
        } else {
            Color::new_unpremul(0.94, 0.98, 0.95, 1.)
        };
        let key = Key {
            renderer: renderer.context_id(),
            text: caption(index, name, empty, viewport.is_some()),
            background,
            ink,
            viewport,
            width: (width as u32 / 8) * 8,
            scale,
        };
        let mut cache = self.texture.borrow_mut();
        if cache.as_ref().is_none_or(|(old, _)| old != &key) {
            let texture = rasterize(renderer, &key)
                .map_err(|error| warn!("error rendering overview caption: {error:#}"))
                .ok();
            *cache = Some((key, texture));
        }
        let buffer = cache.as_ref()?.1.clone()?;
        let location = location.to_physical_precise_round(scale).to_logical(scale);
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

    #[allow(clippy::too_many_arguments)]
    pub(super) fn render_frame(
        &self,
        renderer: &mut impl NiriRenderer,
        rect: Rectangle<f64, Logical>,
        background: Color,
        active: bool,
        progress: f32,
        scale: f64,
        push: &mut dyn FnMut(FocusRingRenderElement),
    ) {
        if rect.size.w <= 0. || rect.size.h <= 0. {
            return;
        }
        let strength = if is_light(background) { 0.52 } else { 0.32 };
        let opacity = background.a + (1. - background.a) * progress * strength;
        let fill = Color::new_unpremul(background.r, background.g, background.b, opacity);
        if !BorderRenderElement::has_shader(renderer) {
            let mut fallback = self.fallback_fill.borrow_mut();
            fallback.update(rect.size, fill);
            push(
                SolidColorRenderElement::from_buffer(&fallback, rect.loc, 1., Kind::Unspecified)
                    .into(),
            );
            return;
        }

        let ink = if is_light(background) { 0.08 } else { 0.94 };
        let outline_alpha = if active { 0.51 } else { 0.32 };
        let outline = Color::new_unpremul(ink, ink, ink, outline_alpha * progress);
        let geo = Rectangle::from_size(rect.size);
        let radius =
            CornerRadius::from(18. * progress).fit_to(rect.size.w as f32, rect.size.h as f32);
        let mut frame = self.frame.borrow_mut();
        for (element, (color, width)) in frame
            .iter_mut()
            .zip([(outline, (1. / scale) as f32), (fill, 0.)])
        {
            element.update(
                rect.size,
                geo,
                Default::default(),
                color,
                color,
                0.,
                geo,
                width,
                radius,
                scale as f32,
                1.,
            );
            push(element.clone().with_location(rect.loc).into());
        }
    }
}

fn is_light(color: Color) -> bool {
    0.2126 * color.r + 0.7152 * color.g + 0.0722 * color.b >= 0.55
}

fn caption(index: usize, name: Option<&str>, empty: bool, has_viewport: bool) -> String {
    let name: String = name
        .unwrap_or_default()
        .chars()
        .take(128)
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let name = name.trim();
    let mut title = match (name.is_empty(), empty) {
        (true, true) => format!("{index}   Empty workspace"),
        (true, false) => format!("Workspace {index}"),
        (false, _) => format!("{index}   {name}"),
    };
    if has_viewport {
        title.push_str("   ·   Visible area");
    }
    title
}

fn layout(cr: &cairo::Context, key: &Key) -> pango::Layout {
    let layout = pangocairo::functions::create_layout(cr);
    layout.set_font_description(Some(&FontDescription::from_string(
        "sans-serif Medium 14px",
    )));
    layout.set_single_paragraph_mode(true);
    layout.set_ellipsize(EllipsizeMode::End);
    layout.set_text(&key.text);
    layout
}

fn rasterize(renderer: &mut GlesRenderer, key: &Key) -> anyhow::Result<TextureBuffer<GlesTexture>> {
    let probe = ImageSurface::create(cairo::Format::ARgb32, 1, 1)?;
    let cr = cairo::Context::new(&probe)?;
    cr.scale(key.scale, key.scale);
    let label = layout(&cr, key);
    let track_space = key.viewport.map_or(0., |_| TRACK_WIDTH + PADDING);
    let width = (f64::from(label.pixel_size().0) + 2. * PADDING + track_space)
        .max(30.)
        .min(f64::from(key.width));
    let pixel_width = (width * key.scale).ceil() as i32;
    let pixel_height = (HEIGHT * key.scale).ceil() as i32;

    let surface = ImageSurface::create(cairo::Format::ARgb32, pixel_width, pixel_height)?;
    let cr = cairo::Context::new(&surface)?;
    cr.scale(key.scale, key.scale);
    let radius = HEIGHT / 2.;
    cr.arc(
        radius,
        radius,
        radius,
        std::f64::consts::FRAC_PI_2,
        3. * std::f64::consts::FRAC_PI_2,
    );
    cr.arc(
        width - radius,
        radius,
        radius,
        -std::f64::consts::FRAC_PI_2,
        std::f64::consts::FRAC_PI_2,
    );
    cr.close_path();
    let color = key.background;
    cr.set_source_rgba(
        color.r.into(),
        color.g.into(),
        color.b.into(),
        color.a.into(),
    );
    cr.fill()?;

    let label = layout(&cr, key);
    label.set_width(((width - 2. * PADDING - track_space) * f64::from(pango::SCALE)) as i32);
    let (_, h) = label.pixel_size();
    let ink = key.ink;
    cr.move_to(PADDING, (HEIGHT - f64::from(h)) / 2.);
    cr.set_source_rgb(ink.r.into(), ink.g.into(), ink.b.into());
    pangocairo::functions::show_layout(&cr, &label);
    if let Some((start, end)) = key.viewport {
        let x = width - PADDING - TRACK_WIDTH;
        let y = HEIGHT / 2.;
        cr.set_line_cap(cairo::LineCap::Round);
        cr.set_line_width(4.);
        cr.set_source_rgba(ink.r.into(), ink.g.into(), ink.b.into(), 0.28);
        cr.move_to(x, y);
        cr.line_to(x + TRACK_WIDTH, y);
        cr.stroke()?;
        cr.set_source_rgb(ink.r.into(), ink.g.into(), ink.b.into());
        let at = |percent: u8| x + TRACK_WIDTH * f64::from(percent) / 100.;
        cr.move_to(at(start), y);
        cr.line_to(at(end), y);
        cr.stroke()?;
    }
    drop(cr);

    Ok(TextureBuffer::from_memory(
        renderer,
        &surface.take_data()?,
        Fourcc::Argb8888,
        (pixel_width, pixel_height),
        false,
        key.scale,
        Transform::Normal,
        Vec::new(),
    )?)
}

#[cfg(test)]
mod tests {
    use super::caption;

    #[test]
    fn captions_use_the_workspace_index_and_plain_names() {
        assert_eq!(caption(2, None, false, false), "Workspace 2");
        assert_eq!(caption(3, Some("  "), false, false), "Workspace 3");
        assert_eq!(
            caption(4, Some("Field\nnotes"), false, false),
            "4   Field notes"
        );
        assert_eq!(caption(1, Some(&"x".repeat(160)), false, false).len(), 132);
    }
}
