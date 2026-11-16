//! Rendering and hit testing share the same titlebar geometry.
use std::cell::RefCell;

use niri_config::titlebar::Titlebar as Config;
use niri_config::{Color, CornerRadius};
use pango::{EllipsizeMode, FontDescription};
use pangocairo::cairo::{self, ImageSurface};
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture};
use smithay::backend::renderer::{ContextId, Renderer as _};
use smithay::utils::{Logical, Point, Rectangle, Size, Transform};

use crate::render_helpers::primary_gpu_texture::PrimaryGpuTextureRenderElement;
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TitlebarPart {
    Drag,
    Maximize,
    Close,
}

#[derive(Debug, Clone, PartialEq)]
struct Key {
    renderer: ContextId<GlesTexture>,
    config: Config,
    title: String,
    size: Size<i32, Logical>,
    scale: f64,
    radius: CornerRadius,
    active: bool,
    maximized: bool,
}

#[derive(Debug, Default)]
pub struct Titlebar {
    // Keep only the current image, including a failed render, rather than accumulating widths
    // while interactively resizing. A title is plain text, never Pango markup.
    cache: RefCell<Option<(Key, Option<TextureBuffer<GlesTexture>>)>>,
    active: bool,
}

pub fn hit(size: Size<f64, Logical>, point: Point<f64, Logical>) -> Option<TitlebarPart> {
    if !Rectangle::from_size(size).contains(point) {
        return None;
    }
    let button = size.h.min(size.w / 2.);
    Some(if point.x >= size.w - button {
        TitlebarPart::Close
    } else if point.x >= size.w - button * 2. {
        TitlebarPart::Maximize
    } else {
        TitlebarPart::Drag
    })
}

impl Titlebar {
    pub fn set_active(&mut self, active: bool) {
        self.active = active;
    }

    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &self,
        renderer: &mut GlesRenderer,
        config: &Config,
        title: String,
        size: Size<f64, Logical>,
        location: Point<f64, Logical>,
        scale: f64,
        radius: CornerRadius,
        maximized: bool,
        alpha: f32,
    ) -> Option<PrimaryGpuTextureRenderElement> {
        let physical = size.to_physical_precise_round(scale).to_i32_round();
        // Bound allocation for malformed client sizes or extreme output scales.
        if physical.w <= 0 || physical.h <= 0 || physical.w > 32768 || physical.h > 512 {
            return None;
        }
        let key = Key {
            renderer: renderer.context_id(),
            config: config.clone(),
            title,
            size: physical.to_logical(1),
            scale,
            radius,
            active: self.active,
            maximized,
        };
        let mut cache = self.cache.borrow_mut();
        if cache.as_ref().is_none_or(|(old, _)| *old != key) {
            let buffer = rasterize(renderer, &key)
                .map_err(|err| warn!("error rendering titlebar: {err:#}"))
                .ok();
            *cache = Some((key, buffer));
        }
        let buffer = cache.as_ref()?.1.clone()?;
        Some(PrimaryGpuTextureRenderElement(
            TextureRenderElement::from_texture_buffer(
                buffer,
                location,
                alpha,
                None,
                Some(size),
                Kind::Unspecified,
            ),
        ))
    }
}

fn source(cr: &cairo::Context, color: Color) {
    cr.set_source_rgba(
        color.r.into(),
        color.g.into(),
        color.b.into(),
        color.a.into(),
    );
}

fn rasterize(renderer: &mut GlesRenderer, key: &Key) -> anyhow::Result<TextureBuffer<GlesTexture>> {
    let width = key.size.w;
    let height = key.size.h;
    let surface = ImageSurface::create(cairo::Format::ARgb32, width, height)?;
    let cr = cairo::Context::new(&surface)?;
    cr.scale(key.scale, key.scale);
    let w = f64::from(width) / key.scale;
    let h = f64::from(height) / key.scale;
    let left = f64::from(key.radius.top_left).min(h).min(w / 2.);
    let right = f64::from(key.radius.top_right).min(h).min(w / 2.);
    cr.move_to(0., h);
    cr.line_to(0., left);
    cr.arc(
        left,
        left,
        left,
        std::f64::consts::PI,
        std::f64::consts::PI * 1.5,
    );
    cr.line_to(w - right, 0.);
    cr.arc(w - right, right, right, -std::f64::consts::FRAC_PI_2, 0.);
    cr.line_to(w, h);
    cr.close_path();
    cr.clip();
    source(
        &cr,
        if key.active {
            key.config.active_color
        } else {
            key.config.inactive_color
        },
    );
    cr.paint()?;

    source(&cr, key.config.text_color);
    let button = h.min(w / 2.);
    let text_x = 12.;
    let text_width = (w - button * 2. - text_x - 8.).max(0.);
    if text_width > 0. {
        let layout = pangocairo::functions::create_layout(&cr);
        layout.set_font_description(Some(&FontDescription::from_string(&key.config.font)));
        layout.set_single_paragraph_mode(true);
        layout.set_ellipsize(EllipsizeMode::End);
        layout.set_width((text_width * f64::from(pango::SCALE)) as i32);
        layout.set_text(&key.title);
        let (_, text_height) = layout.pixel_size();
        cr.move_to(text_x, (h - f64::from(text_height)) / 2.);
        pangocairo::functions::show_layout(&cr, &layout);
    }
    let icon = (button / 3.).min(12.);
    let cy = h / 2.;
    let close_x = w - button / 2.;
    cr.set_line_width(1.5);
    cr.move_to(close_x - icon / 2., cy - icon / 2.);
    cr.line_to(close_x + icon / 2., cy + icon / 2.);
    cr.move_to(close_x - icon / 2., cy + icon / 2.);
    cr.line_to(close_x + icon / 2., cy - icon / 2.);
    cr.stroke()?;
    let maximize_x = w - button * 1.5;
    if key.maximized {
        cr.rectangle(maximize_x - icon / 2. + 2., cy - icon / 2. - 2., icon, icon);
        cr.stroke()?;
    }
    cr.rectangle(maximize_x - icon / 2., cy - icon / 2., icon, icon);
    cr.stroke()?;
    drop(cr);
    let data = surface.take_data()?;
    Ok(TextureBuffer::from_memory(
        renderer,
        &data,
        Fourcc::Argb8888,
        (width, height),
        false,
        key.scale,
        Transform::Normal,
        Vec::new(),
    )?)
}
